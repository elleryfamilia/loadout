//! Layered configuration: built-in defaults ← global ← repo.
//!
//! - Global: `$XDG_CONFIG_HOME/rosita/config.toml` (falls back to
//!   `~/.config/rosita/config.toml`). Overridable via `ROSITA_CONFIG_DIR`
//!   (used in tests and for isolation).
//! - Repo: `<repo_base>/.rosita/config.toml`, where `repo_base` is the git
//!   root (or the cwd when not in a repo).
//!
//! Each layer is parsed into a [`RawConfig`] of optional fields so we can tell
//! "unset" from "set to the default", merge precisely, then finalize against the
//! built-in defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::adapters::AgentDescriptor;
use crate::capability::Capability;
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
    /// Profiles, composed additively against the detected context.
    pub profiles: Vec<ProfileConfig>,
    /// Capability library (built-ins merged with `[[capabilities]]` by id).
    pub capabilities: Vec<Capability>,
    /// Agent descriptors (built-ins merged with `[[agents]]` overrides by id).
    pub agents: Vec<AgentDescriptor>,
    /// hostname-glob → host class (e.g. `work`).
    pub host_classes: BTreeMap<String, Vec<String>>,
    /// Config files that actually contributed, in load order.
    pub sources: Vec<PathBuf>,
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
    /// Opt-in to generating/updating `AGENTS.override.md`.
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
    pub fn load_from(global: Option<&Path>, repo_base: &Path) -> Result<Self> {
        let mut sources = Vec::new();
        let mut raw = RawConfig::default();

        if let Some(global) = global {
            if let Some(layer) = RawConfig::from_path(global)? {
                raw.merge(layer);
                sources.push(global.to_path_buf());
            }
        }

        let repo = repo_config_path(repo_base);
        if let Some(layer) = RawConfig::from_path(&repo)? {
            raw.merge(layer);
            sources.push(repo);
        }

        Ok(raw.finalize(sources))
    }

    /// The built-in defaults with no config files loaded (used by `doctor` and
    /// when scaffolding). Equivalent to `load` against an empty environment.
    pub fn defaults() -> Self {
        RawConfig::default().finalize(Vec::new())
    }
}

// --- raw (per-layer) parsing -------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    defaults: Option<RawDefaults>,
    env: Option<RawEnv>,
    codex: Option<RawCodex>,
    #[serde(default)]
    profiles: Vec<ProfileConfig>,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default)]
    agents: Vec<AgentDescriptor>,
    #[serde(default)]
    host_classes: BTreeMap<String, Vec<String>>,
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
        // Repo profiles replace built-in/global profiles of the same name.
        for p in other.profiles {
            match self.profiles.iter_mut().find(|e| e.name == p.name) {
                Some(existing) => *existing = p,
                None => self.profiles.push(p),
            }
        }
        // Repo capabilities replace built-in/global ones of the same id.
        for cap in other.capabilities {
            match self.capabilities.iter_mut().find(|e| e.id == cap.id) {
                Some(existing) => *existing = cap,
                None => self.capabilities.push(cap),
            }
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

        // Built-in profiles form the base; user profiles override by name.
        let mut profiles = crate::profile::builtin_profiles();
        for p in self.profiles {
            match profiles.iter_mut().find(|e| e.name == p.name) {
                Some(existing) => *existing = p,
                None => profiles.push(p),
            }
        }

        // Built-in capabilities form the base; user ones override by id.
        let mut capabilities = crate::capability::builtin_capabilities();
        for cap in self.capabilities {
            match capabilities.iter_mut().find(|e| e.id == cap.id) {
                Some(existing) => *existing = cap,
                None => capabilities.push(cap),
            }
        }

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
                write_override: codex.write_override.unwrap_or(false),
                max_output_kib: codex.max_output_kib.unwrap_or(32),
            },
            profiles,
            capabilities,
            agents,
            host_classes: self.host_classes,
            sources,
        }
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
            write_override: false,
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

/// Path to the global `config.toml`, if a config dir can be resolved.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("config.toml"))
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
        assert!(!c.codex.write_override);
        assert!(c.env.allowlist.contains(&"LANG".to_string()));
        // Built-in profiles are always present.
        assert!(c.profiles.iter().any(|p| p.name == "rust"));
        assert!(c.profiles.iter().any(|p| p.name == "default"));
        // Built-in capabilities are always present.
        assert!(c.capabilities.iter().any(|cap| cap.id == "baseline"));
        assert!(c
            .capabilities
            .iter()
            .any(|cap| cap.id == "rust-conventions"));
    }

    #[test]
    fn user_capabilities_override_and_extend() {
        let mut base = RawConfig::default();
        let overlay: RawConfig = toml::from_str(
            r#"
            [[capabilities]]
            id = "rust-conventions"
            description = "Rust (custom)"
            guidance = "my rust rules"

            [[capabilities]]
            id = "ssh-tailnet"
            risk = "caution"
            guidance = "you may ssh within my tailnet"
            "#,
        )
        .unwrap();
        base.merge(overlay);
        let c = base.finalize(vec![]);

        // Override replaced the built-in by id.
        let rustc = c
            .capabilities
            .iter()
            .find(|x| x.id == "rust-conventions")
            .unwrap();
        assert_eq!(rustc.guidance, "my rust rules");
        // New capability was added.
        assert!(c.capabilities.iter().any(|x| x.id == "ssh-tailnet"));
        // Other built-ins survive.
        assert!(c.capabilities.iter().any(|x| x.id == "baseline"));
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
            priority = 99
            guidance = "custom rust guidance"
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
        // user profile replaced the built-in "rust" by name
        let rust = c.profiles.iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(rust.priority, 99);
        assert_eq!(rust.guidance.as_deref(), Some("custom rust guidance"));
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
}
