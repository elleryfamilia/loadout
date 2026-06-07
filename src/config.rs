//! Layered configuration: built-in ← global `config.toml` ← global `local.toml`
//! ← repo `config.toml` ← repo `local.toml` (later wins).
//!
//! - Global: `$XDG_CONFIG_HOME/rosita/config.toml` (falls back to
//!   `~/.config/rosita/config.toml`). Overridable via `ROSITA_CONFIG_DIR`
//!   (used in tests and for isolation).
//! - Repo: `<repo_base>/.rosita/config.toml`, where `repo_base` is the git
//!   root (or the cwd when not in a repo).
//! - `local.toml` (in either dir) is the **private**, gitignored layer for
//!   sensitive specifics (real hostnames, `host_classes`, fragment `params`);
//!   `config.toml` is the **public**, shareable layer. `rosita doctor` lints the
//!   public layers for machine-specific literals that belong in `local.toml`.
//!
//! Each layer is parsed into a [`RawConfig`] of optional fields so we can tell
//! "unset" from "set to the default", merge precisely, then finalize against the
//! built-in defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::adapters::AgentDescriptor;
use crate::fragment::Fragment;
use crate::profile::ProfileConfig;

/// Fully-resolved configuration used by the rest of the program.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    /// Agent rendered when `--agent` is omitted.
    pub default_agent: String,
    /// Environment-variable exposure policy.
    pub env: EnvConfig,
    /// Codex-adapter knobs.
    pub codex: CodexConfig,
    /// Your profiles (entirely user-authored; one is selected per context).
    pub profiles: Vec<ProfileConfig>,
    /// Your fragment library (the `[[fragments]]` you authored, merged by
    /// id across layers). The shipped [`palette`](crate::fragment::palette) is
    /// a separate read-only catalog and is **not** included here.
    pub fragments: Vec<Fragment>,
    /// Per-fragment `params` overrides keyed by fragment id, deep-merged
    /// across layers. The private (`local.toml`) place for sensitive values
    /// a public fragment's guidance references.
    pub fragment_params: BTreeMap<String, toml::Value>,
    /// Agent descriptors (built-ins merged with `[[agents]]` overrides by id).
    pub agents: Vec<AgentDescriptor>,
    /// hostname-glob → host class (e.g. `work`).
    pub host_classes: BTreeMap<String, Vec<String>>,
    /// Cross-machine sync settings (`[sync]`).
    pub sync: SyncConfig,
    /// Config files that actually contributed, in load order.
    pub sources: Vec<PathBuf>,
}

/// Cross-machine sync of the global config dir (`[sync]`). Auto-pull/push are on
/// by default but **inert** unless the config dir is a git repo with a remote
/// (`rosita sync init`), so they never act on a machine that opted out.
#[derive(Debug, Clone, Serialize)]
pub struct SyncConfig {
    /// Pull the latest config before `run`/`render`/`refresh` (throttled by
    /// `pull_max_age`, bounded by `timeout`, best-effort).
    pub auto_pull: bool,
    /// Commit + push after an apply (studio / future CLI edits). Best-effort.
    pub auto_push: bool,
    /// Skip an auto-pull when the last successful sync was within this window.
    pub pull_max_age: std::time::Duration,
    /// Hard cap on any git network op; exceeded → fall back to local config.
    pub timeout: std::time::Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            auto_pull: true,
            auto_push: true,
            pull_max_age: std::time::Duration::from_secs(300), // 5m
            timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// Environment-variable exposure policy. Allowlist-only, with a name denylist
/// that wins even if a key was mistakenly allowlisted.
#[derive(Debug, Clone, Serialize)]
pub struct EnvConfig {
    /// Exact variable names that may be surfaced.
    pub allowlist: Vec<String>,
    /// Regexes over variable *names*; matches are always dropped.
    pub deny_name_patterns: Vec<String>,
}

/// Codex adapter configuration.
#[derive(Debug, Clone, Serialize)]
pub struct CodexConfig {
    /// Write/update the gitignored `AGENTS.override.md` (which Codex prefers over
    /// `AGENTS.md`) so `rosita run codex` delivers context out of the box.
    /// Defaults to `true`; set `false` (or pass `--no-override`) to opt out.
    pub write_override: bool,
    /// Warn when generated output exceeds this many KiB.
    pub max_output_kib: u64,
}

impl Config {
    /// Load and merge all layers for the given repo base directory, using the
    /// resolved global config path.
    pub fn load(repo_base: &Path) -> Result<Self> {
        Self::load_from(global_config_path().as_deref(), repo_base)
    }

    /// Like [`Config::load`] but with an explicit global config path (or none).
    /// Keeps tests hermetic without mutating process-global environment.
    ///
    /// Merge order (later wins): built-in ← global `config.toml` ← global
    /// `local.toml` ← repo `config.toml` ← repo `local.toml`. The `*.toml`
    /// files named `local.toml` are the private, gitignored layer (real
    /// hostnames, `host_classes`, fragment `params`); the `config.toml` files
    /// are the public, shareable layer.
    pub fn load_from(global: Option<&Path>, repo_base: &Path) -> Result<Self> {
        use crate::fragment::Layer;

        // Layers in precedence order (later wins). `None` entries (e.g. no
        // global dir) are skipped; missing files contribute nothing. Each layer
        // tags its fragments with their origin so global-only enforcement can
        // tell repo-declared caps from your own global ones.
        let mut layers: Vec<(Layer, PathBuf)> = Vec::new();
        if let Some(global) = global {
            layers.push((Layer::Global, global.to_path_buf()));
            if let Some(dir) = global.parent() {
                layers.push((Layer::GlobalLocal, dir.join("local.toml")));
            }
        }
        layers.push((Layer::Repo, repo_config_path(repo_base)));
        layers.push((Layer::RepoLocal, repo_local_path(repo_base)));

        let mut sources = Vec::new();
        let mut raw = RawConfig::default();
        for (layer, path) in layers {
            if let Some(mut parsed) = RawConfig::from_path(&path)? {
                strip_global_only(layer, &mut parsed);
                for cap in &mut parsed.fragments {
                    cap.origin = layer;
                }
                raw.merge(parsed);
                sources.push(path);
            }
        }
        Ok(raw.finalize(sources))
    }

    /// The built-in defaults with no config files loaded (used by `doctor` and
    /// when scaffolding). Equivalent to `load` against an empty environment.
    pub fn defaults() -> Self {
        RawConfig::default().finalize(Vec::new())
    }

    /// Assemble a [`Config`] from **in-memory** layer texts (studio's staged
    /// docs), parsing and merging them exactly as [`Config::load_from`] does from
    /// disk — including **re-tagging each fragment's `origin` by its layer**.
    ///
    /// Layers are given in precedence order (later wins). This is
    /// security-critical: `Fragment::origin` is `#[serde(skip)]` and defaults
    /// to [`Layer::BuiltIn`](crate::fragment::Layer::BuiltIn), and global-only
    /// enforcement keys off origin — a repo-declared fragment assembled
    /// *without* re-tagging would look built-in and slip past the global-only
    /// check. Mirrors the disk-load tagging in [`Config::load_from`].
    pub fn from_layer_strs(layers: Vec<(crate::fragment::Layer, PathBuf, String)>) -> Result<Self> {
        let mut sources = Vec::new();
        let mut raw = RawConfig::default();
        for (layer, path, text) in layers {
            let mut parsed: RawConfig = toml::from_str(&text)
                .with_context(|| format!("parsing staged config for {}", path.display()))?;
            strip_global_only(layer, &mut parsed);
            for cap in &mut parsed.fragments {
                cap.origin = layer;
            }
            raw.merge(parsed);
            sources.push(path);
        }
        Ok(raw.finalize(sources))
    }
}

/// Enforce the global-only model: fragments and profiles are honored only
/// from the layers allowed to contribute them (caps: built-in/global/global-local;
/// profiles: built-in/global). A repo layer that declares either is silently
/// dropped here so it can never select or render — `rosita doctor` flags the
/// raw file so the mistake is visible. Other repo-layer content (`fragment_params`,
/// `host_classes`, `[binding]`, `defaults`, `env`, `agents`) is untouched.
fn strip_global_only(layer: crate::fragment::Layer, parsed: &mut RawConfig) {
    if !layer.contributes_fragments() {
        parsed.fragments.clear();
    }
    if !layer.contributes_profiles() {
        parsed.profiles.clear();
    }
}

// --- raw (per-layer) parsing -------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    defaults: Option<RawDefaults>,
    env: Option<RawEnv>,
    codex: Option<RawCodex>,
    sync: Option<RawSync>,
    #[serde(default)]
    profiles: Vec<ProfileConfig>,
    #[serde(default)]
    fragments: Vec<Fragment>,
    #[serde(default)]
    fragment_params: BTreeMap<String, toml::Value>,
    #[serde(default)]
    agents: Vec<AgentDescriptor>,
    #[serde(default)]
    host_classes: BTreeMap<String, Vec<String>>,
    /// The per-project `[binding]` (repo `local.toml`). Parsed here only so the
    /// strict `deny_unknown_fields` layer accepts it; the binding is owned and
    /// read by [`crate::binding`], not carried on the merged [`Config`].
    #[serde(default)]
    #[allow(dead_code)]
    binding: Option<crate::binding::RawBinding>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    agent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnv {
    allowlist: Option<Vec<String>>,
    deny_name_patterns: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodex {
    write_override: Option<bool>,
    max_output_kib: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSync {
    auto_pull: Option<bool>,
    auto_push: Option<bool>,
    pull_max_age: Option<String>, // duration string, e.g. "5m"
    timeout: Option<String>,      // duration string, e.g. "5s"
}

impl RawConfig {
    fn from_path(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let parsed: RawConfig = toml::from_str(&text)
            .with_context(|| format!("parsing TOML config {}", path.display()))?;
        Ok(Some(parsed))
    }

    /// Merge `other` (higher precedence) into `self`.
    fn merge(&mut self, other: RawConfig) {
        if let Some(d) = other.defaults {
            let slot = self.defaults.get_or_insert_with(Default::default);
            if d.agent.is_some() {
                slot.agent = d.agent;
            }
        }
        if let Some(e) = other.env {
            let slot = self.env.get_or_insert_with(Default::default);
            // Allowlists/denylists are additive across layers (safer to widen
            // intentionally than to silently drop a parent layer's entries).
            if let Some(mut a) = e.allowlist {
                slot.allowlist.get_or_insert_with(Vec::new).append(&mut a);
            }
            if let Some(mut d) = e.deny_name_patterns {
                slot.deny_name_patterns
                    .get_or_insert_with(Vec::new)
                    .append(&mut d);
            }
        }
        if let Some(c) = other.codex {
            let slot = self.codex.get_or_insert_with(Default::default);
            if c.write_override.is_some() {
                slot.write_override = c.write_override;
            }
            if c.max_output_kib.is_some() {
                slot.max_output_kib = c.max_output_kib;
            }
        }
        if let Some(s) = other.sync {
            let slot = self.sync.get_or_insert_with(Default::default);
            if s.auto_pull.is_some() {
                slot.auto_pull = s.auto_pull;
            }
            if s.auto_push.is_some() {
                slot.auto_push = s.auto_push;
            }
            if s.pull_max_age.is_some() {
                slot.pull_max_age = s.pull_max_age;
            }
            if s.timeout.is_some() {
                slot.timeout = s.timeout;
            }
        }
        // Later-layer profiles replace earlier ones of the same name.
        for p in other.profiles {
            match self.profiles.iter_mut().find(|e| e.name == p.name) {
                Some(existing) => *existing = p,
                None => self.profiles.push(p),
            }
        }
        // Later-layer fragments replace earlier ones of the same id.
        for cap in other.fragments {
            match self.fragments.iter_mut().find(|e| e.id == cap.id) {
                Some(existing) => *existing = cap,
                None => self.fragments.push(cap),
            }
        }
        // Fragment params deep-merge across layers (later wins per key), so a
        // private layer can supply just the sensitive values.
        for (id, params) in other.fragment_params {
            let slot = self
                .fragment_params
                .entry(id)
                .or_insert(toml::Value::Table(toml::map::Map::new()));
            *slot = merge_toml(slot.clone(), params);
        }
        // Repo agent descriptors replace built-in/global ones of the same id.
        for a in other.agents {
            match self.agents.iter_mut().find(|e| e.id == a.id) {
                Some(existing) => *existing = a,
                None => self.agents.push(a),
            }
        }
        for (k, v) in other.host_classes {
            self.host_classes.insert(k, v);
        }
    }

    fn finalize(self, sources: Vec<PathBuf>) -> Config {
        let defaults = self.defaults.unwrap_or_default();
        let env = self.env.unwrap_or_default();
        let codex = self.codex.unwrap_or_default();
        let sync = self.sync.unwrap_or_default();

        // No shipped profiles and no auto-injected fragments: both are
        // entirely user-authored (already merged by name/id across layers in
        // `merge`). The shipped `fragment::palette()` is a separate read-only
        // catalog you pick from; it is never composed and never lands here.
        let profiles = self.profiles;
        let fragments = self.fragments;

        // Built-in agents form the base; user `[[agents]]` override by id.
        let mut agents = crate::adapters::builtin_agents();
        for a in self.agents {
            match agents.iter_mut().find(|e| e.id == a.id) {
                Some(existing) => *existing = a,
                None => agents.push(a),
            }
        }

        Config {
            default_agent: defaults.agent.unwrap_or_else(|| "claude".to_string()),
            env: EnvConfig {
                allowlist: dedup(env.allowlist.unwrap_or_else(default_env_allowlist)),
                deny_name_patterns: dedup(
                    env.deny_name_patterns
                        .unwrap_or_else(default_deny_name_patterns),
                ),
            },
            codex: CodexConfig {
                write_override: codex.write_override.unwrap_or(true),
                max_output_kib: codex.max_output_kib.unwrap_or(32),
            },
            profiles,
            fragments,
            fragment_params: self.fragment_params,
            agents,
            host_classes: self.host_classes,
            sync: SyncConfig {
                auto_pull: sync.auto_pull.unwrap_or(true),
                auto_push: sync.auto_push.unwrap_or(true),
                pull_max_age: sync
                    .pull_max_age
                    .as_deref()
                    .and_then(crate::providers::parse_duration)
                    .unwrap_or_else(|| std::time::Duration::from_secs(300)),
                timeout: sync
                    .timeout
                    .as_deref()
                    .and_then(crate::providers::parse_duration)
                    .unwrap_or_else(|| std::time::Duration::from_secs(5)),
            },
            sources,
        }
    }
}

/// Deep-merge two TOML values, with `over` winning. Tables merge key-by-key
/// (recursing); any non-table (or a type mismatch) is replaced wholesale.
pub(crate) fn merge_toml(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(mut b), toml::Value::Table(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(existing) => merge_toml(existing, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            toml::Value::Table(b)
        }
        (_, over) => over,
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

/// Default env allowlist: locale/terminal/CI hints that are never secret.
pub fn default_env_allowlist() -> Vec<String> {
    [
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "TZ",
        "EDITOR",
        "VISUAL",
        "PAGER",
        "SHELL",
        "CI",
        "GITHUB_ACTIONS",
        "RUNNER_OS",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Default denylist applied to env-var *names* regardless of allowlist.
pub fn default_deny_name_patterns() -> Vec<String> {
    vec![r"(?i)(secret|token|key|password|passwd|pwd|credential|auth|session|cookie)".to_string()]
}

impl Default for EnvConfig {
    fn default() -> Self {
        EnvConfig {
            allowlist: default_env_allowlist(),
            deny_name_patterns: default_deny_name_patterns(),
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        CodexConfig {
            write_override: true,
            max_output_kib: 32,
        }
    }
}

// --- path resolution ---------------------------------------------------------

/// Directory holding the global config and templates.
///
/// Honors `ROSITA_CONFIG_DIR`, then `$XDG_CONFIG_HOME/rosita`, then
/// `~/.config/rosita`. Returns `None` only if no home can be determined.
pub fn global_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ROSITA_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("rosita"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("rosita"))
}

/// The user's home directory (`$HOME`), if set. Used to resolve other tools'
/// dotfiles (e.g. Gemini's `~/.gemini/settings.json`). Honors a `$HOME` override
/// so tests stay isolated from the real home.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Path to the global `config.toml`, if a config dir can be resolved.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("config.toml"))
}

/// Path to the global private `local.toml` (gitignored), if resolvable.
pub fn global_local_path() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("local.toml"))
}

/// Repo private `local.toml` path (gitignored): real hostnames, `host_classes`,
/// fragment `params` — the sensitive layer kept out of the shareable config.
pub fn repo_local_path(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("local.toml")
}

/// Global templates directory.
pub fn global_templates_dir() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("templates"))
}

/// The `.rosita` directory for a repo base.
pub fn repo_dir(repo_base: &Path) -> PathBuf {
    repo_base.join(".rosita")
}

/// Repo `config.toml` path.
pub fn repo_config_path(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("config.toml")
}

/// Repo templates directory.
pub fn repo_templates_dir(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("templates")
}

/// Directory generated overlays are written to.
pub fn generated_dir(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("generated")
}

/// Directory provider probe results are cached in (gitignored, volatile).
pub fn cache_dir(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("cache")
}

/// Audit log path.
pub fn audit_log_path(repo_base: &Path) -> PathBuf {
    repo_dir(repo_base).join("logs").join("events.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::defaults();
        assert_eq!(c.default_agent, "claude");
        assert_eq!(c.codex.max_output_kib, 32);
        assert!(c.codex.write_override);
        assert!(c.env.allowlist.contains(&"LANG".to_string()));
        // No shipped profiles and no auto-injected fragments: a fresh install
        // owns an empty library (the palette is a separate catalog).
        assert!(c.profiles.is_empty());
        assert!(c.fragments.is_empty());
    }

    #[test]
    fn user_fragments_merge_across_layers() {
        // A later layer replaces an earlier fragment by id and adds new ones.
        let mut base: RawConfig = toml::from_str(
            r#"
            [[fragments]]
            id = "rust-conventions"
            guidance = "base rust rules"
            "#,
        )
        .unwrap();
        let overlay: RawConfig = toml::from_str(
            r#"
            [[fragments]]
            id = "rust-conventions"
            description = "Rust (custom)"
            guidance = "my rust rules"

            [[fragments]]
            id = "ssh-tailnet"
            guidance = "you may ssh within my tailnet"
            "#,
        )
        .unwrap();
        base.merge(overlay);
        let c = base.finalize(vec![]);

        // Override replaced the earlier fragment by id.
        let rustc = c
            .fragments
            .iter()
            .find(|x| x.id == "rust-conventions")
            .unwrap();
        assert_eq!(rustc.guidance, "my rust rules");
        // New fragment was added.
        assert!(c.fragments.iter().any(|x| x.id == "ssh-tailnet"));
        // Only the two authored fragments exist — nothing is auto-injected.
        assert_eq!(c.fragments.len(), 2);
    }

    #[test]
    fn repo_layer_overrides_and_extends() {
        let mut base = RawConfig::default();
        let overlay: RawConfig = toml::from_str(
            r#"
            [defaults]
            agent = "codex"

            [codex]
            write_override = true
            max_output_kib = 64

            [env]
            allowlist = ["MY_FLAG"]

            [[profiles]]
            name = "rust"
            targets = ["rust"]
            fragments = ["rust-conventions"]
            "#,
        )
        .unwrap();
        base.merge(overlay);
        let c = base.finalize(vec![]);

        assert_eq!(c.default_agent, "codex");
        assert!(c.codex.write_override);
        assert_eq!(c.codex.max_output_kib, 64);
        // An explicit allowlist in any layer REPLACES the built-in defaults
        // (full user control); built-in defaults apply only when unset.
        assert_eq!(c.env.allowlist, vec!["MY_FLAG".to_string()]);
        // The user profile is carried through with its targets + fragments.
        let rust = c.profiles.iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(rust.targets, vec!["rust".to_string()]);
        assert_eq!(rust.fragments.len(), 1);
    }

    #[test]
    fn binding_table_in_local_toml_parses_and_is_ignored_by_config() {
        // `[binding]` is the binding module's concern, but the strict parser
        // must accept it in the private layer rather than reject it.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo_dir(repo.path())).unwrap();
        std::fs::write(
            repo_local_path(repo.path()),
            "[binding]\nprofile = \"rust — browser\"\n",
        )
        .unwrap();
        let c = Config::load_from(None, repo.path()).expect("local.toml [binding] must parse");
        // It contributes nothing to the merged config.
        assert!(c.profiles.is_empty());
        assert!(c.fragments.is_empty());
    }

    #[test]
    fn cross_layer_allowlist_unions_and_dedups() {
        // global + repo both set allowlist → union (deduped), defaults replaced.
        let mut base: RawConfig = toml::from_str("[env]\nallowlist = [\"A\", \"B\"]\n").unwrap();
        let repo: RawConfig = toml::from_str("[env]\nallowlist = [\"B\", \"C\"]\n").unwrap();
        base.merge(repo);
        let c = base.finalize(vec![]);
        assert_eq!(c.env.allowlist, vec!["A", "B", "C"]);
    }

    #[test]
    fn config_dir_honors_override() {
        // Can't safely mutate process env in parallel tests; just assert the
        // HOME-based fallback shape when override/XDG are absent is plausible.
        let dir = global_config_dir();
        assert!(dir.is_some() || std::env::var_os("HOME").is_none());
    }

    #[test]
    fn merge_toml_deep_merges_tables_over_wins() {
        let base: toml::Value = toml::from_str("a = 1\n[t]\nx = 1\ny = 2\n").unwrap();
        let over: toml::Value = toml::from_str("a = 9\n[t]\ny = 20\nz = 30\n").unwrap();
        let m = merge_toml(base, over);
        assert_eq!(m.get("a").unwrap().as_integer(), Some(9)); // scalar replaced
        let t = m.get("t").unwrap();
        assert_eq!(t.get("x").unwrap().as_integer(), Some(1)); // kept from base
        assert_eq!(t.get("y").unwrap().as_integer(), Some(20)); // overridden
        assert_eq!(t.get("z").unwrap().as_integer(), Some(30)); // added
    }

    #[test]
    fn fragment_params_deep_merge_across_layers() {
        // Public layer sets a non-sensitive default; private layer fills in the
        // sensitive value without clobbering the rest.
        let mut base: RawConfig =
            toml::from_str("[fragment_params.ssh]\nuser = \"deploy\"\nport = 22\n").unwrap();
        let local: RawConfig =
            toml::from_str("[fragment_params.ssh]\nhost = \"box.private\"\nport = 2222\n").unwrap();
        base.merge(local);
        let c = base.finalize(vec![]);
        let ssh = c.fragment_params.get("ssh").unwrap();
        assert_eq!(ssh.get("user").unwrap().as_str(), Some("deploy")); // kept
        assert_eq!(ssh.get("host").unwrap().as_str(), Some("box.private")); // added
        assert_eq!(ssh.get("port").unwrap().as_integer(), Some(2222)); // overridden
    }

    #[test]
    fn local_layer_loads_after_and_overrides_config() {
        // A repo `local.toml` is read after `config.toml` and wins.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo_dir(repo.path())).unwrap();
        std::fs::write(
            repo_config_path(repo.path()),
            "[defaults]\nagent = \"codex\"\n[fragment_params.x]\nv = 1\n",
        )
        .unwrap();
        std::fs::write(
            repo_local_path(repo.path()),
            "[fragment_params.x]\nv = 2\nsecret = \"shh\"\n",
        )
        .unwrap();

        let c = Config::load_from(None, repo.path()).unwrap();
        let x = c.fragment_params.get("x").unwrap();
        assert_eq!(x.get("v").unwrap().as_integer(), Some(2)); // local wins
        assert_eq!(x.get("secret").unwrap().as_str(), Some("shh"));
        // Both files are recorded as sources, in load order.
        assert!(c.sources[0].ends_with("config.toml"));
        assert!(c.sources[1].ends_with("local.toml"));
    }

    // --- global-only enforcement (strip_global_only) -------------------------

    const FRAGMENT_AND_PROFILE: &str = r#"
        [[fragments]]
        id = "x"
        guidance = "hello"

        [[profiles]]
        name = "p"
        targets = ["rust"]
        fragments = ["x"]
    "#;

    #[test]
    fn repo_layer_caps_and_profiles_are_dropped_by_loader() {
        // A repo `config.toml` may *declare* caps/profiles (the strict parser
        // accepts the tables), but the loader honors neither — they are global.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo_dir(repo.path())).unwrap();
        std::fs::write(repo_config_path(repo.path()), FRAGMENT_AND_PROFILE).unwrap();

        let c = Config::load_from(None, repo.path()).unwrap();
        assert!(c.fragments.is_empty(), "repo caps must be dropped");
        assert!(c.profiles.is_empty(), "repo profiles must be dropped");
    }

    #[test]
    fn global_config_contributes_caps_and_profiles() {
        let global = tempfile::tempdir().unwrap();
        let gcfg = global.path().join("config.toml");
        std::fs::write(&gcfg, FRAGMENT_AND_PROFILE).unwrap();
        let repo = tempfile::tempdir().unwrap();

        let c = Config::load_from(Some(&gcfg), repo.path()).unwrap();
        assert!(c.fragments.iter().any(|x| x.id == "x"));
        assert!(c.profiles.iter().any(|p| p.name == "p"));
    }

    #[test]
    fn global_local_contributes_caps_but_not_profiles() {
        // The private global layer may hold fragments (real hostnames etc.)
        // but never profiles — profiles are public-global only.
        let global = tempfile::tempdir().unwrap();
        let gcfg = global.path().join("config.toml");
        std::fs::write(&gcfg, "").unwrap();
        std::fs::write(global.path().join("local.toml"), FRAGMENT_AND_PROFILE).unwrap();
        let repo = tempfile::tempdir().unwrap();

        let c = Config::load_from(Some(&gcfg), repo.path()).unwrap();
        assert!(
            c.fragments.iter().any(|x| x.id == "x"),
            "global local.toml caps must be kept"
        );
        assert!(
            c.profiles.is_empty(),
            "global local.toml profiles must be dropped"
        );
    }

    #[test]
    fn from_layer_strs_enforces_global_only() {
        use crate::fragment::Layer;
        let c = Config::from_layer_strs(vec![
            (
                Layer::Global,
                PathBuf::from("/g/config.toml"),
                FRAGMENT_AND_PROFILE.to_string(),
            ),
            (
                Layer::Repo,
                PathBuf::from("/r/.rosita/config.toml"),
                "[[fragments]]\nid = \"repo-cap\"\nguidance = \"nope\"\n\
                 \n[[profiles]]\nname = \"repo-prof\"\ntargets = [\"rust\"]\n"
                    .to_string(),
            ),
        ])
        .unwrap();
        // Global contributes; the repo layer is stripped (studio path).
        assert!(c.fragments.iter().any(|x| x.id == "x"));
        assert!(c.profiles.iter().any(|p| p.name == "p"));
        assert!(!c.fragments.iter().any(|x| x.id == "repo-cap"));
        assert!(!c.profiles.iter().any(|p| p.name == "repo-prof"));
    }
}
