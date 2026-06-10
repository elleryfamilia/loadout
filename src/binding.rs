//! Per-project remembered profile choice — "the binding".
//!
//! When 2+ profiles match a project, rosita asks once which to use and remembers
//! the answer so it never asks again. Where the answer lives depends on scope:
//!
//! - **Repo** → the repo's gitignored `.rosita/local.toml` `[binding]` table
//!   (per-checkout; a teammate's checkout makes its own choice). Written with
//!   `toml_edit` so the rest of the private layer is preserved.
//! - **Machine** (no repo) → a global, path-keyed store `bindings.toml`, keyed
//!   by the project path.
//!
//! The store is rosita-owned, so the global file is written with the plain
//! `toml` serializer; only the hand-editable `local.toml` needs `toml_edit`.
//!
//! The same store also remembers per-machine **skill decisions** (the ask-once
//! "install the rosita-migrate skill?" answer) in a `[skills]` table — rosita-
//! owned machine state of the same class as a binding, and deliberately *not*
//! in `local.toml`, whose strict config parse rejects unknown tables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::context::{Context, Scope};
use crate::writer::atomic_write;

/// A remembered profile choice for a project. There is no "opt out" binding:
/// invoking rosita means you want a profile, so the only remembered choice is
/// *which* one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Use this profile by name. `targets_hash` fingerprints the profile's
    /// `targets` at the moment it was bound. When it is present and the live
    /// profile's targets hash differs, the profile was *retargeted* since you
    /// chose it — the binding is stale and selection re-detects. A `None`
    /// fingerprint (a hand-written or pre-hash binding) is trusted by name, so
    /// a deliberate manual bind keeps working.
    Profile {
        name: String,
        targets_hash: Option<String>,
    },
}

impl Binding {
    /// A name-only profile binding (no freshness fingerprint).
    pub fn profile(name: impl Into<String>) -> Self {
        Binding::Profile {
            name: name.into(),
            targets_hash: None,
        }
    }
}

/// The on-disk shape of a binding: the `[binding]` table in repo `local.toml`,
/// and each per-path entry in the global store. `profile` names the chosen
/// profile; unset ⇒ no binding. A legacy `none = true` opt-out is still parsed
/// (so old files don't hard-error) but no longer honored — see [`RawBinding::none`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawBinding {
    /// The chosen profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Fingerprint of the bound profile's `targets` at bind time. Absent for
    /// hand-written bindings (trusted by name). Used to detect a retargeted
    /// profile and re-run selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets_hash: Option<String>,
    /// Legacy opt-out flag ("no profile here"). No longer honored — invoking
    /// rosita means you want a profile — but still accepted on read so old
    /// `none = true` files parse, and never re-emitted on write.
    #[serde(default, skip_serializing_if = "is_false")]
    pub none: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl RawBinding {
    /// Interpret the raw fields as a [`Binding`]. A legacy `none = true` opt-out
    /// is ignored (treated as no binding): invoking rosita means you want a
    /// profile, so selection re-runs and the chooser fires when 2+ match.
    pub fn to_binding(&self) -> Option<Binding> {
        self.profile.clone().map(|name| Binding::Profile {
            name,
            targets_hash: self.targets_hash.clone(),
        })
    }

    fn from_binding(b: &Binding) -> Self {
        let Binding::Profile { name, targets_hash } = b;
        RawBinding {
            profile: Some(name.clone()),
            targets_hash: targets_hash.clone(),
            none: false,
        }
    }
}

// --- scope-aware front door --------------------------------------------------

/// Read the remembered binding for `ctx`, from the repo `local.toml` (repo
/// scope) or the global path-keyed store (machine scope).
pub fn read(ctx: &Context) -> Option<Binding> {
    match ctx.scope() {
        Scope::Repo => read_repo(&ctx.repo_base),
        Scope::Machine => read_global(&ctx.cwd),
    }
}

/// Persist the binding for `ctx` to the scope-appropriate location.
pub fn write(ctx: &Context, b: &Binding) -> Result<()> {
    match ctx.scope() {
        Scope::Repo => write_repo(&ctx.repo_base, b),
        Scope::Machine => write_global(&ctx.cwd, b),
    }
}

// --- repo scope: the `[binding]` table in `.rosita/local.toml` ---------------

/// Lenient view over `local.toml` that extracts only `[binding]` (every other
/// table — `host_classes`, `fragment_params`, … — is ignored).
#[derive(Debug, Default, Deserialize)]
struct LocalBindingFile {
    #[serde(default)]
    binding: Option<RawBinding>,
}

/// Read the `[binding]` from a repo's private `local.toml`, if any.
pub fn read_repo(repo_base: &Path) -> Option<Binding> {
    let text = std::fs::read_to_string(config::repo_local_path(repo_base)).ok()?;
    let parsed: LocalBindingFile = toml::from_str(&text).ok()?;
    parsed.binding?.to_binding()
}

/// Write the `[binding]` into a repo's private `local.toml`, preserving the rest
/// of the file's content, comments, and formatting.
pub fn write_repo(repo_base: &Path, b: &Binding) -> Result<()> {
    let path = config::repo_local_path(repo_base);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {} before writing binding", path.display()))?;

    // Replace the whole `[binding]` table so rebinding to a different profile
    // (or clearing a legacy `none`) never leaves a stale key behind.
    let mut table = toml_edit::Table::new();
    let Binding::Profile { name, targets_hash } = b;
    table["profile"] = toml_edit::value(name.as_str());
    if let Some(h) = targets_hash {
        table["targets_hash"] = toml_edit::value(h.as_str());
    }
    doc["binding"] = toml_edit::Item::Table(table);

    atomic_write(&path, &doc.to_string())
}

// --- machine scope: the global path-keyed store ------------------------------

/// The global bindings store: cwd path → remembered choice, plus per-machine
/// skill decisions (skill id → `"accepted"`/`"declined"`). Lenient parse: files
/// written by newer rosita versions with extra tables still load.
#[derive(Debug, Default, Serialize, Deserialize)]
struct BindingStore {
    #[serde(default)]
    bound: BTreeMap<String, RawBinding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    skills: BTreeMap<String, String>,
}

/// Path to the global bindings store, if a global config dir resolves.
pub fn store_path() -> Option<PathBuf> {
    config::global_config_dir().map(|d| d.join("bindings.toml"))
}

fn key(cwd: &Path) -> String {
    cwd.display().to_string()
}

fn load(store_path: &Path) -> BindingStore {
    std::fs::read_to_string(store_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(store_path: &Path, store: &BindingStore) -> Result<()> {
    let text = toml::to_string(store).context("serializing bindings store")?;
    atomic_write(store_path, &text)
}

/// Read the global binding for `cwd`, if one is recorded.
pub fn read_global(cwd: &Path) -> Option<Binding> {
    read_global_at(&store_path()?, cwd)
}

/// Persist the global binding for `cwd`.
pub fn write_global(cwd: &Path, b: &Binding) -> Result<()> {
    let path = store_path().ok_or_else(|| anyhow!("no global config dir to store bindings in"))?;
    write_global_at(&path, cwd, b)
}

/// [`read_global`] against an explicit store path (testable core).
pub fn read_global_at(store_path: &Path, cwd: &Path) -> Option<Binding> {
    load(store_path)
        .bound
        .get(&key(cwd))
        .and_then(|r| r.to_binding())
}

/// [`write_global`] against an explicit store path (testable core).
pub fn write_global_at(store_path: &Path, cwd: &Path, b: &Binding) -> Result<()> {
    let mut store = load(store_path);
    store.bound.insert(key(cwd), RawBinding::from_binding(b));
    save(store_path, &store)
}

// --- skill decisions: the ask-once install answer, per machine ----------------

/// The remembered answer to "install this skill?". There is no "ask again
/// later" value: an unrecorded decision *is* "ask when appropriate".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDecision {
    /// Install and keep current (repair links, refresh on upgrade).
    Accepted,
    /// Don't install, don't ask again. Re-enable via `rosita skill install`.
    Declined,
}

impl SkillDecision {
    fn as_str(self) -> &'static str {
        match self {
            SkillDecision::Accepted => "accepted",
            SkillDecision::Declined => "declined",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "accepted" => Some(SkillDecision::Accepted),
            "declined" => Some(SkillDecision::Declined),
            _ => None,
        }
    }
}

/// Read the remembered decision for a skill id, if any.
pub fn read_skill_decision(skill_id: &str) -> Option<SkillDecision> {
    read_skill_decision_at(&store_path()?, skill_id)
}

/// Persist the decision for a skill id.
pub fn write_skill_decision(skill_id: &str, d: SkillDecision) -> Result<()> {
    let path = store_path().ok_or_else(|| anyhow!("no global config dir to store decisions in"))?;
    write_skill_decision_at(&path, skill_id, d)
}

/// [`read_skill_decision`] against an explicit store path (testable core).
pub fn read_skill_decision_at(store_path: &Path, skill_id: &str) -> Option<SkillDecision> {
    load(store_path)
        .skills
        .get(skill_id)
        .and_then(|s| SkillDecision::parse(s))
}

/// [`write_skill_decision`] against an explicit store path (testable core).
pub fn write_skill_decision_at(store_path: &Path, skill_id: &str, d: SkillDecision) -> Result<()> {
    let mut store = load(store_path);
    store.skills.insert(skill_id.to_string(), d.as_str().to_string());
    save(store_path, &store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_binding_round_trips_and_preserves_content() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::repo_dir(repo.path())).unwrap();
        // Hand-authored private layer with a comment and an unrelated table.
        std::fs::write(
            config::repo_local_path(repo.path()),
            "# private notes\n[fragment_params.ssh]\nuser = \"deploy\"\n",
        )
        .unwrap();

        // No binding yet.
        assert_eq!(read_repo(repo.path()), None);

        // Write a profile binding; the unrelated content + comment survive.
        write_repo(repo.path(), &Binding::profile("rust — browser")).unwrap();
        assert_eq!(
            read_repo(repo.path()),
            Some(Binding::profile("rust — browser"))
        );
        let text = std::fs::read_to_string(config::repo_local_path(repo.path())).unwrap();
        assert!(text.contains("# private notes"));
        assert!(text.contains("user = \"deploy\""));

        // Rebinding to another profile replaces the table (no stale name left).
        write_repo(repo.path(), &Binding::profile("go")).unwrap();
        assert_eq!(read_repo(repo.path()), Some(Binding::profile("go")));
        let text = std::fs::read_to_string(config::repo_local_path(repo.path())).unwrap();
        assert!(!text.contains("rust — browser"));
    }

    #[test]
    fn legacy_none_opt_out_is_ignored() {
        // Files written by old rosita versions may carry `none = true`. It is no
        // longer honored: read back as "no binding" so selection runs normally
        // (the chooser fires when 2+ profiles match).
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::repo_dir(repo.path())).unwrap();
        std::fs::write(
            config::repo_local_path(repo.path()),
            "[binding]\nnone = true\n",
        )
        .unwrap();
        assert_eq!(read_repo(repo.path()), None);

        // Same for the global path-keyed store.
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bindings.toml");
        std::fs::write(&store, "[bound.\"/work/p\"]\nnone = true\n").unwrap();
        assert_eq!(read_global_at(&store, Path::new("/work/p")), None);
    }

    #[test]
    fn global_store_round_trips_and_is_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bindings.toml");
        let a = Path::new("/work/proj-a");
        let b = Path::new("/work/proj-b");

        assert_eq!(read_global_at(&store, a), None);
        write_global_at(&store, a, &Binding::profile("machine")).unwrap();
        write_global_at(&store, b, &Binding::profile("kernel")).unwrap();

        assert_eq!(read_global_at(&store, a), Some(Binding::profile("machine")));
        assert_eq!(read_global_at(&store, b), Some(Binding::profile("kernel")));
        // Independent paths don't bleed into each other.
        assert_eq!(read_global_at(&store, Path::new("/elsewhere")), None);
    }

    #[test]
    fn skill_decisions_round_trip_and_coexist_with_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bindings.toml");

        assert_eq!(read_skill_decision_at(&store, "rosita-migrate"), None);
        write_skill_decision_at(&store, "rosita-migrate", SkillDecision::Declined).unwrap();
        assert_eq!(
            read_skill_decision_at(&store, "rosita-migrate"),
            Some(SkillDecision::Declined)
        );

        // Flipping the decision and adding a binding don't clobber each other.
        write_skill_decision_at(&store, "rosita-migrate", SkillDecision::Accepted).unwrap();
        write_global_at(&store, Path::new("/work/p"), &Binding::profile("machine")).unwrap();
        assert_eq!(
            read_skill_decision_at(&store, "rosita-migrate"),
            Some(SkillDecision::Accepted)
        );
        assert_eq!(
            read_global_at(&store, Path::new("/work/p")),
            Some(Binding::profile("machine"))
        );

        // An unknown value (written by a future rosita) reads as undecided.
        std::fs::write(&store, "[skills]\nrosita-migrate = \"snoozed\"\n").unwrap();
        assert_eq!(read_skill_decision_at(&store, "rosita-migrate"), None);
    }

    #[test]
    fn targets_hash_round_trips_through_repo_and_global() {
        let bound = Binding::Profile {
            name: "rust".into(),
            targets_hash: Some("sha256:abc123".into()),
        };

        // Repo `local.toml` (toml_edit path).
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::repo_dir(repo.path())).unwrap();
        write_repo(repo.path(), &bound).unwrap();
        assert_eq!(read_repo(repo.path()), Some(bound.clone()));
        let text = std::fs::read_to_string(config::repo_local_path(repo.path())).unwrap();
        assert!(text.contains("targets_hash"));

        // Global path-keyed store (plain toml serializer).
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bindings.toml");
        let p = Path::new("/work/proj");
        write_global_at(&store, p, &bound).unwrap();
        assert_eq!(read_global_at(&store, p), Some(bound));
    }
}
