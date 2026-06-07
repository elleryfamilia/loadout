//! The headless `toml_edit` edit engine — studio's write risk core.
//!
//! A [`Session`] opens the writable config layer files, keeps a *staged*
//! `toml_edit` document per file plus an ordered, replayable list of
//! [`StagedOp`]s, and can:
//!
//! - **stage** typed create/edit/delete of fragments & profiles (and
//!   duplicate a palette item into a layer) — built by mutating the parsed tree
//!   in place, so comments and key order on untouched regions survive by
//!   construction (never string concatenation);
//! - **diff** the staged document against the raw on-disk bytes (via `similar`),
//!   surfacing when `toml_edit` would also reformat untouched lines;
//! - **apply** atomically — an external-edit hash gate, a one-shot `.bak` per
//!   touched file, writes ordered public-before-private, then reload + baseline
//!   reset.
//!
//! The staged config can also be assembled in memory via
//! [`Config::from_layer_strs`](crate::config::Config::from_layer_strs), which
//! **re-tags fragment origins by layer** so the global-only enforcement
//! (`Layer::contributes_fragments`) sees the right authorship.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::config::{self, Config};
use crate::fragment::{palette, Fragment, Layer};
use crate::profile::ProfileConfig;
use crate::writer::{atomic_write, AtomicWriter, Writer, WrittenFile};

/// A typed, replayable staged edit. Each carries the [`Layer`] it targets.
#[derive(Debug, Clone)]
pub enum StagedOp {
    /// Add a new fragment to a layer.
    CreateFragment { layer: Layer, cap: Box<Fragment> },
    /// Replace the fragment with this id in a layer (created if absent).
    EditFragment {
        layer: Layer,
        id: String,
        cap: Box<Fragment>,
    },
    /// Remove the fragment with this id from a layer.
    DeleteFragment { layer: Layer, id: String },
    /// Add a new profile to a layer.
    CreateProfile {
        layer: Layer,
        profile: Box<ProfileConfig>,
    },
    /// Replace the profile with this name in a layer (created if absent).
    EditProfile {
        layer: Layer,
        name: String,
        profile: Box<ProfileConfig>,
    },
    /// Remove the profile with this name from a layer.
    DeleteProfile { layer: Layer, name: String },
    /// Copy a shipped palette fragment into a layer to own it (the only way to
    /// "edit" a palette item). Re-resolved from the palette on replay.
    DuplicatePaletteItem { id: String, to_layer: Layer },
}

impl StagedOp {
    /// The layer this op writes to.
    pub fn layer(&self) -> Layer {
        match self {
            StagedOp::CreateFragment { layer, .. }
            | StagedOp::EditFragment { layer, .. }
            | StagedOp::DeleteFragment { layer, .. }
            | StagedOp::CreateProfile { layer, .. }
            | StagedOp::EditProfile { layer, .. }
            | StagedOp::DeleteProfile { layer, .. } => *layer,
            StagedOp::DuplicatePaletteItem { to_layer, .. } => *to_layer,
        }
    }
}

/// One writable config layer file held by a [`Session`].
struct LayerFile {
    layer: Layer,
    path: PathBuf,
    public: bool,
    /// Raw bytes at load (empty string if the file was absent).
    original: String,
    /// Parsed-from-original document (untouched baseline).
    doc: DocumentMut,
    /// Working copy that staged ops mutate.
    staged: DocumentMut,
    /// `sha256:…` of `original`, for the external-edit gate.
    sha: String,
}

/// A studio editing session over a repo's (and optionally the global) config
/// layers: staged `toml_edit` docs + an ordered, replayable op list.
pub struct Session {
    repo_base: PathBuf,
    layers: Vec<LayerFile>,
    ops: Vec<StagedOp>,
}

/// A per-file diff of the staged document against the raw on-disk bytes.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Which layer the file is.
    pub layer: Layer,
    /// File path.
    pub path: PathBuf,
    /// Public (`config.toml`) vs private (`local.toml`).
    pub public: bool,
    /// Raw bytes currently on disk at load.
    pub raw_before: String,
    /// Bytes the staged document would write.
    pub staged_after: String,
    /// Unified diff (raw → staged), from `similar`.
    pub unified: String,
    /// Staged bytes differ from the raw on-disk bytes.
    pub changed: bool,
    /// `toml_edit` would also reformat untouched lines (parsed-then-reserialized
    /// original ≠ raw) — surfaced so hand-authored TOML rewrites aren't hidden.
    pub reformats_untouched: bool,
}

impl Session {
    /// Open a session over the repo's writable layers (`config.toml` +
    /// `local.toml`), plus the global layers when `global_dir` is given. Missing
    /// files are tracked as empty so they can be created by staged ops.
    pub fn open(repo_base: &Path, global_dir: Option<&Path>) -> Result<Self> {
        let mut candidates: Vec<(Layer, PathBuf, bool)> = Vec::new();
        if let Some(g) = global_dir {
            candidates.push((Layer::Global, g.join("config.toml"), true));
            candidates.push((Layer::GlobalLocal, g.join("local.toml"), false));
        }
        candidates.push((Layer::Repo, config::repo_config_path(repo_base), true));
        candidates.push((Layer::RepoLocal, config::repo_local_path(repo_base), false));

        let mut layers = Vec::with_capacity(candidates.len());
        for (layer, path, public) in candidates {
            layers.push(LayerFile::open(layer, path, public)?);
        }
        Ok(Session {
            repo_base: repo_base.to_path_buf(),
            layers,
            ops: Vec::new(),
        })
    }

    /// Stage one typed op: mutate the target layer's working document and record
    /// the op for replay. Errors if the op targets a layer not in this session.
    pub fn stage(&mut self, op: StagedOp) -> Result<()> {
        let layer = op.layer();
        let lf = self
            .layers
            .iter_mut()
            .find(|l| l.layer == layer)
            .ok_or_else(|| anyhow!("layer {layer:?} is not open in this session"))?;
        apply_op(&mut lf.staged, &op)?;
        self.ops.push(op);
        Ok(())
    }

    /// The staged ops recorded so far, in order.
    pub fn ops(&self) -> &[StagedOp] {
        &self.ops
    }

    /// Throw away every staged op, reloading each layer from disk so the working
    /// documents match the current on-disk state. The inverse of [`apply`], minus
    /// the writing — nothing on disk is touched.
    pub fn discard(&mut self) -> Result<()> {
        for lf in &mut self.layers {
            lf.reread()?;
        }
        self.ops.clear();
        Ok(())
    }

    /// The staged text of every open layer, `(layer, path, text)`, for in-memory
    /// assembly via [`Config::from_layer_strs`].
    pub fn staged_layer_texts(&self) -> Vec<(Layer, PathBuf, String)> {
        self.layers
            .iter()
            .map(|lf| (lf.layer, lf.path.clone(), lf.staged.to_string()))
            .collect()
    }

    /// Paths of layers whose on-disk bytes differ from what was loaded (the
    /// external-edit poll). Empty ⇒ nothing changed underneath the session.
    pub fn external_edits(&self) -> Vec<PathBuf> {
        self.layers
            .iter()
            .filter(|lf| sha(&std::fs::read_to_string(&lf.path).unwrap_or_default()) != lf.sha)
            .map(|lf| lf.path.clone())
            .collect()
    }

    /// Which open layer currently holds the fragment with this id (in the
    /// staged docs), if any. Used to target a delete (`ProfileConfig` has no
    /// origin field, so studio looks the layer up rather than guessing).
    pub fn fragment_layer(&self, id: &str) -> Option<Layer> {
        self.layers
            .iter()
            .find(|lf| has_entry(&lf.staged, "fragments", "id", id))
            .map(|lf| lf.layer)
    }

    /// Which open layer currently holds the profile with this name (staged).
    pub fn profile_layer(&self, name: &str) -> Option<Layer> {
        self.layers
            .iter()
            .find(|lf| has_entry(&lf.staged, "profiles", "name", name))
            .map(|lf| lf.layer)
    }

    /// Assemble the staged config in memory (origin-tagged per layer). `Ok`
    /// means every staged layer re-parses through the strict config parser.
    pub fn staged_config(&self) -> Result<Config> {
        let layers = self
            .layers
            .iter()
            .map(|lf| (lf.layer, lf.path.clone(), lf.staged.to_string()))
            .collect();
        Config::from_layer_strs(layers)
    }

    /// Gate apply: every staged layer must re-parse through the strict config
    /// parser. (Richer diagnostics — cycles, regex, minijinja, leak-lint — land
    /// with the HTTP slices.)
    pub fn validate(&self) -> Result<()> {
        self.staged_config().map(|_| ())
    }

    /// Per-file diffs of staged vs. raw on-disk bytes, only for files that differ
    /// (or that `toml_edit` would reformat).
    pub fn diff(&self) -> Vec<FileDiff> {
        self.layers
            .iter()
            .filter_map(|lf| {
                let staged_after = lf.staged.to_string();
                let changed = staged_after != lf.original;
                let reformats_untouched = lf.doc.to_string() != lf.original;
                if !changed && !reformats_untouched {
                    return None;
                }
                Some(FileDiff {
                    layer: lf.layer,
                    path: lf.path.clone(),
                    public: lf.public,
                    unified: unified_diff(&lf.original, &staged_after, &lf.path),
                    raw_before: lf.original.clone(),
                    staged_after,
                    changed,
                    reformats_untouched,
                })
            })
            .collect()
    }

    /// Re-read every layer from disk and replay the staged ops onto fresh
    /// baselines (the external-edit "Reload" action).
    pub fn reload(&mut self) -> Result<()> {
        for lf in &mut self.layers {
            lf.reread()?;
        }
        let ops = std::mem::take(&mut self.ops);
        for op in &ops {
            if let Some(lf) = self.layers.iter_mut().find(|l| l.layer == op.layer()) {
                apply_op(&mut lf.staged, op)?;
            }
        }
        self.ops = ops;
        Ok(())
    }

    /// Validate, gate on external edits, back up touched files, then write each
    /// changed layer atomically (public `config.toml` before private
    /// `local.toml`), and reset the baseline. Returns the files written.
    ///
    /// Cross-file atomicity is best-effort (per-file atomic + backups + ordering;
    /// no journal) — a documented limitation.
    pub fn apply(&mut self) -> Result<Vec<WrittenFile>> {
        self.validate().context("staged config is invalid")?;
        self.check_external_edits()?;

        // Snapshot a one-shot .bak for each existing file that will change.
        let backup_dir = config::cache_dir(&self.repo_base).join("studio-backups");
        for lf in &self.layers {
            if lf.staged.to_string() != lf.original && !lf.original.is_empty() {
                let name = lf
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "layer".to_string());
                let bak = backup_dir.join(format!("{}.{:?}.bak", name, lf.layer));
                atomic_write(&bak, &lf.original)
                    .with_context(|| format!("backing up {}", lf.path.display()))?;
            }
        }

        // Write public before private (layers are already stored in that order).
        let writer = AtomicWriter::new(false);
        let mut written = Vec::new();
        for lf in &self.layers {
            let staged = lf.staged.to_string();
            if staged != lf.original {
                written.push(writer.write(&lf.path, &staged)?);
            }
        }

        // Reset baseline from what is now on disk; the ops are committed.
        for lf in &mut self.layers {
            lf.reread()?;
        }
        self.ops.clear();
        Ok(written)
    }

    fn check_external_edits(&self) -> Result<()> {
        let mut changed = Vec::new();
        for lf in &self.layers {
            let now = std::fs::read_to_string(&lf.path).unwrap_or_default();
            if sha(&now) != lf.sha {
                changed.push(lf.path.display().to_string());
            }
        }
        if !changed.is_empty() {
            bail!(
                "config changed on disk since the session loaded — reload before applying: {}",
                changed.join(", ")
            );
        }
        Ok(())
    }
}

impl LayerFile {
    fn open(layer: Layer, path: PathBuf, public: bool) -> Result<Self> {
        let original = std::fs::read_to_string(&path).unwrap_or_default();
        let doc: DocumentMut = original
            .parse()
            .with_context(|| format!("parsing {} as TOML", path.display()))?;
        Ok(LayerFile {
            sha: sha(&original),
            staged: doc.clone(),
            doc,
            original,
            public,
            layer,
            path,
        })
    }

    /// Re-read from disk and reset the baseline + staged copy.
    fn reread(&mut self) -> Result<()> {
        let original = std::fs::read_to_string(&self.path).unwrap_or_default();
        self.doc = original
            .parse()
            .with_context(|| format!("re-parsing {} as TOML", self.path.display()))?;
        self.staged = self.doc.clone();
        self.sha = sha(&original);
        self.original = original;
        Ok(())
    }
}

// --- op application (toml_edit tree mutation, never string concat) -----------

fn apply_op(doc: &mut DocumentMut, op: &StagedOp) -> Result<()> {
    match op {
        StagedOp::CreateFragment { cap, .. } => {
            aot_mut(doc, "fragments").push(fragment_table(cap)?);
        }
        StagedOp::EditFragment { id, cap, .. } => {
            upsert(aot_mut(doc, "fragments"), "id", id, fragment_table(cap)?);
        }
        StagedOp::DeleteFragment { id, .. } => {
            remove(aot_mut(doc, "fragments"), "id", id);
        }
        StagedOp::CreateProfile { profile, .. } => {
            aot_mut(doc, "profiles").push(profile_table(profile)?);
        }
        StagedOp::EditProfile { name, profile, .. } => {
            upsert(
                aot_mut(doc, "profiles"),
                "name",
                name,
                profile_table(profile)?,
            );
        }
        StagedOp::DeleteProfile { name, .. } => {
            remove(aot_mut(doc, "profiles"), "name", name);
        }
        StagedOp::DuplicatePaletteItem { id, .. } => {
            let cap = palette()
                .into_iter()
                .find(|c| &c.id == id)
                .ok_or_else(|| anyhow!("unknown palette fragment '{id}'"))?;
            // Duplicating an existing id replaces it (you own the copy now).
            upsert(aot_mut(doc, "fragments"), "id", id, fragment_table(&cap)?);
        }
    }
    Ok(())
}

/// Get (or create) an array-of-tables at `key`.
fn aot_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut ArrayOfTables {
    if doc.get(key).and_then(Item::as_array_of_tables).is_none() {
        doc.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc[key]
        .as_array_of_tables_mut()
        .expect("just inserted an array-of-tables")
}

/// Replace the entry whose `field` equals `val`, or push when absent.
fn upsert(aot: &mut ArrayOfTables, field: &str, val: &str, table: Table) {
    match find_index(aot, field, val) {
        Some(i) => {
            if let Some(slot) = aot.get_mut(i) {
                *slot = table;
            }
        }
        None => aot.push(table),
    }
}

/// Remove the entry whose `field` equals `val`, if present.
fn remove(aot: &mut ArrayOfTables, field: &str, val: &str) {
    if let Some(i) = find_index(aot, field, val) {
        aot.remove(i);
    }
}

fn find_index(aot: &ArrayOfTables, field: &str, val: &str) -> Option<usize> {
    aot.iter()
        .position(|t| t.get(field).and_then(Item::as_str) == Some(val))
}

/// Whether `doc[key]` is an array-of-tables containing an entry with `field == val`.
fn has_entry(doc: &DocumentMut, key: &str, field: &str, val: &str) -> bool {
    doc.get(key)
        .and_then(Item::as_array_of_tables)
        .map(|aot| find_index(aot, field, val).is_some())
        .unwrap_or(false)
}

// --- typed → toml_edit table builders ----------------------------------------

/// Build a clean array-of-tables entry for a fragment — only meaningful
/// (non-default) fields, so studio writes TOML you could have authored.
fn fragment_table(c: &Fragment) -> Result<Table> {
    let mut t = Table::new();
    t["id"] = value(c.id.as_str());
    if let Some(d) = &c.description {
        t["description"] = value(d.as_str());
    }
    if let Some(ic) = &c.icon {
        t["icon"] = value(ic.as_str());
    }
    if let Some(cat) = &c.category {
        t["category"] = value(cat.as_str());
    }
    if !c.when.is_empty() {
        let when = toml::Value::try_from(&c.when).context("serializing fragment `when`")?;
        t["when"] = to_edit_item(&when);
    }
    if !c.requires.is_empty() {
        t["requires"] = str_array(&c.requires);
    }
    if !c.agents.is_empty() {
        t["agents"] = str_array(&c.agents);
    }
    if let Some(p) = &c.provider {
        t["provider"] = value(p.as_str());
    }
    if let Some(cmd) = &c.command {
        t["command"] = value(cmd.as_str());
    }
    if let Some(lang) = &c.script_lang {
        t["script_lang"] = value(lang.as_str());
    }
    // Only persist the off-switch; `allow_exec = true` is the default.
    if !c.allow_exec {
        t["allow_exec"] = value(false);
    }
    if let Some(cache) = &c.cache {
        t["cache"] = value(cache.as_str());
    }
    if !c.guidance.is_empty() {
        t["guidance"] = value(c.guidance.as_str());
    }
    if c.params.as_table().map(|p| !p.is_empty()).unwrap_or(false) {
        // Inline so it reads `params = { … }` rather than a nested header.
        t["params"] = Item::Value(to_edit_value(&c.params));
    }
    Ok(t)
}

/// Build a clean array-of-tables entry for a profile.
fn profile_table(p: &ProfileConfig) -> Result<Table> {
    let mut t = Table::new();
    t["name"] = value(p.name.as_str());
    if !p.targets.is_empty() {
        t["targets"] = str_array(&p.targets);
    }
    if !p.fragments.is_empty() {
        let caps = toml::Value::try_from(&p.fragments).context("serializing profile fragments")?;
        t["fragments"] = to_edit_item(&caps);
    }
    if let Some(tmpl) = &p.template {
        t["template"] = value(tmpl.as_str());
    }
    if let Some(g) = &p.guidance {
        t["guidance"] = value(g.as_str());
    }
    if p.disabled {
        t["disabled"] = value(true);
    }
    Ok(t)
}

fn str_array(items: &[String]) -> Item {
    let mut a = Array::new();
    for s in items {
        a.push(s.as_str());
    }
    Item::Value(Value::Array(a))
}

/// Convert a `toml::Value` to a `toml_edit::Value` (tables become inline tables).
fn to_edit_value(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::from(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Datetime(d) => Value::from(d.to_string()),
        toml::Value::Array(arr) => {
            let mut a = Array::new();
            for e in arr {
                a.push(to_edit_value(e));
            }
            Value::Array(a)
        }
        toml::Value::Table(tbl) => {
            let mut it = InlineTable::new();
            for (k, vv) in tbl {
                it.insert(k, to_edit_value(vv));
            }
            Value::InlineTable(it)
        }
    }
}

/// Convert a `toml::Value` to a `toml_edit::Item` (top-level tables stay tables).
fn to_edit_item(v: &toml::Value) -> Item {
    Item::Value(to_edit_value(v))
}

fn sha(text: &str) -> String {
    crate::hash::context_hash(&text)
}

fn unified_diff(before: &str, after: &str, path: &Path) -> String {
    let label = path.display().to_string();
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .header(&format!("{label} (on disk)"), &format!("{label} (staged)"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(id: &str, guidance: &str) -> Fragment {
        Fragment {
            id: id.into(),
            description: Some(format!("{id} desc")),
            category: None,
            when: vec![],
            requires: vec![],
            params: toml::Value::Table(Default::default()),
            guidance: guidance.into(),
            agents: vec![],
            provider: None,
            command: None,
            script_lang: None,
            icon: None,
            allow_exec: true,
            cache: None,
            origin: Layer::default(),
        }
    }

    /// A repo with a hand-authored, commented `config.toml`.
    fn repo_with_config(body: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::repo_dir(d.path())).unwrap();
        std::fs::write(config::repo_config_path(d.path()), body).unwrap();
        d
    }

    fn session(repo: &Path) -> Session {
        Session::open(repo, None).unwrap()
    }

    /// A repo plus a global config dir (a subdir of the same tempdir) whose
    /// `config.toml` starts with `body`. Fragments and profiles are
    /// global-only, so tests that assert on the *merged* staged config author
    /// them here and stage to `Layer::Global`.
    fn repo_with_global(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let gdir = d.path().join("global");
        std::fs::create_dir_all(&gdir).unwrap();
        std::fs::write(gdir.join("config.toml"), body).unwrap();
        (d, gdir)
    }

    fn session_global(repo: &Path, gdir: &Path) -> Session {
        Session::open(repo, Some(gdir)).unwrap()
    }

    #[test]
    fn untouched_session_round_trips_comments_exactly() {
        let body = "# top comment\n\n[[fragments]]\nid = \"a\"  # inline\nguidance = \"hi\"\n";
        let d = repo_with_config(body);
        let s = session(d.path());
        // No ops staged → no diffs at all (byte-exact round-trip).
        assert!(
            s.diff().is_empty(),
            "untouched session should produce no diff"
        );
    }

    #[test]
    fn create_fragment_preserves_existing_comments() {
        let body = "# keep me\n\n[[fragments]]\nid = \"a\"\nguidance = \"A\"\n";
        let d = repo_with_config(body);
        let mut s = session(d.path());
        s.stage(StagedOp::CreateFragment {
            layer: Layer::Repo,
            cap: Box::new(cap("b", "B body")),
        })
        .unwrap();

        let diffs = s.diff();
        assert_eq!(diffs.len(), 1);
        let after = &diffs[0].staged_after;
        // Original comment + entry survive; the new entry is appended cleanly.
        assert!(after.contains("# keep me"));
        assert!(after.contains("id = \"a\""));
        assert!(after.contains("id = \"b\""));
        assert!(after.contains("guidance = \"B body\""));
        // The staged text must re-parse through the strict config parser.
        s.validate().unwrap();
    }

    #[test]
    fn edit_then_delete_fragment() {
        let body = "[[fragments]]\nid = \"a\"\nguidance = \"old\"\n\n[[fragments]]\nid = \"b\"\nguidance = \"B\"\n";
        let (d, gdir) = repo_with_global(body);
        let mut s = session_global(d.path(), &gdir);

        let edited = cap("a", "new guidance");
        s.stage(StagedOp::EditFragment {
            layer: Layer::Global,
            id: "a".into(),
            cap: Box::new(edited),
        })
        .unwrap();
        s.stage(StagedOp::DeleteFragment {
            layer: Layer::Global,
            id: "b".into(),
        })
        .unwrap();

        let after = s.diff()[0].staged_after.clone();
        assert!(after.contains("new guidance"));
        assert!(!after.contains("id = \"b\""));
        // Still valid TOML/config.
        s.validate().unwrap();
        let cfg = s.staged_config().unwrap();
        assert_eq!(cfg.fragments.len(), 1);
        assert_eq!(cfg.fragments[0].guidance, "new guidance");
    }

    #[test]
    fn create_profile_with_targets_and_caps() {
        let d = repo_with_config("");
        let mut s = session(d.path());
        let profile = ProfileConfig {
            name: "rust".into(),
            targets: vec!["rust".into()],
            fragments: vec![crate::profile::FragmentRef::Id("a".into())],
            template: None,
            guidance: None,
            disabled: false,
        };
        s.stage(StagedOp::CreateProfile {
            layer: Layer::Repo,
            profile: Box::new(profile),
        })
        .unwrap();
        let after = s.diff()[0].staged_after.clone();
        assert!(after.contains("name = \"rust\""));
        assert!(after.contains("targets = [\"rust\"]"));
        assert!(after.contains("fragments = [\"a\"]"));
        s.validate().unwrap();
    }

    #[test]
    fn apply_writes_and_is_idempotent_on_reload() {
        let body = "# header\n[[fragments]]\nid = \"a\"\nguidance = \"A\"\n";
        let d = repo_with_config(body);
        let mut s = session(d.path());
        s.stage(StagedOp::CreateFragment {
            layer: Layer::Repo,
            cap: Box::new(cap("b", "B")),
        })
        .unwrap();

        let written = s.apply().unwrap();
        assert_eq!(written.len(), 1);

        // On disk: both fragments, comment preserved.
        let on_disk = std::fs::read_to_string(config::repo_config_path(d.path())).unwrap();
        assert!(on_disk.contains("# header"));
        assert!(on_disk.contains("id = \"a\""));
        assert!(on_disk.contains("id = \"b\""));

        // Baseline reset: nothing staged → no diff, and a no-op re-open agrees.
        assert!(s.diff().is_empty());
        let reopened = session(d.path());
        assert!(reopened.diff().is_empty());

        // A backup of the pre-apply file was snapshotted.
        let bak_dir = config::cache_dir(d.path()).join("studio-backups");
        assert!(bak_dir.exists());
    }

    #[test]
    fn external_edit_gate_blocks_apply() {
        let d = repo_with_config("[[fragments]]\nid = \"a\"\nguidance = \"A\"\n");
        let mut s = session(d.path());
        s.stage(StagedOp::CreateFragment {
            layer: Layer::Repo,
            cap: Box::new(cap("b", "B")),
        })
        .unwrap();

        // Someone edits the file out from under the session.
        std::fs::write(
            config::repo_config_path(d.path()),
            "[[fragments]]\nid = \"z\"\nguidance = \"Z\"\n",
        )
        .unwrap();

        let err = s.apply().unwrap_err();
        assert!(err.to_string().contains("changed on disk"));

        // Reload re-reads + replays the staged op onto the new baseline.
        s.reload().unwrap();
        let after = s.diff()[0].staged_after.clone();
        assert!(after.contains("id = \"z\"")); // the external edit is now the base
        assert!(after.contains("id = \"b\"")); // our staged op replayed
        s.apply().unwrap();
    }

    #[test]
    fn created_command_fragment_is_owned_globally() {
        // Fragments are global-only, so an authored `command` fragment lands
        // in the global layer. Origin tagging is verified via the in-memory
        // assembly seam (it gates global-only enforcement, not trust).
        let (d, gdir) = repo_with_global("");
        let mut s = session_global(d.path(), &gdir);
        let mut command_cap = cap("danger", "runs a command");
        command_cap.command = Some("echo hi".into());
        s.stage(StagedOp::CreateFragment {
            layer: Layer::Global,
            cap: Box::new(command_cap),
        })
        .unwrap();

        let cfg = s.staged_config().unwrap();
        let landed = cfg.fragments.iter().find(|c| c.id == "danger").unwrap();
        assert_eq!(landed.origin, Layer::Global);
        assert!(landed.origin.contributes_fragments());
    }

    #[test]
    fn real_palette_duplicate_is_owned_globally() {
        let (d, gdir) = repo_with_global("");
        let mut s = session_global(d.path(), &gdir);
        let palette_id = palette()[0].id.clone();
        s.stage(StagedOp::DuplicatePaletteItem {
            id: palette_id.clone(),
            to_layer: Layer::Global,
        })
        .unwrap();
        let cfg = s.staged_config().unwrap();
        let dup = cfg
            .fragments
            .iter()
            .find(|c| c.id == palette_id)
            .expect("duplicated palette item should now be in the global library");
        assert_eq!(dup.origin, Layer::Global);
    }
}
