//! Implementations of the CLI subcommands.
//!
//! Each subcommand is a thin function over the library. Shared plumbing —
//! loading config, detecting context, selecting a profile, and resolving the
//! target agent set — lives here.

pub mod clean;
pub mod detect;
pub mod doctor;
pub mod explain;
pub mod init;
pub mod refresh;
pub mod render;
pub mod run;

use std::path::PathBuf;

use anyhow::{bail, Context as _};

use crate::adapters;
use crate::config::Config;
use crate::context::{self, Context};
use crate::profile::{self, Composition};

/// Per-invocation runtime settings derived from global args.
pub struct Runtime {
    /// Directory to operate on.
    pub cwd: PathBuf,
    /// Whether writes are simulated.
    pub dry_run: bool,
}

impl Runtime {
    /// Build from the resolved cwd and dry-run flag.
    pub fn new(cwd: PathBuf, dry_run: bool) -> Self {
        Runtime { cwd, dry_run }
    }
}

/// The result of config load + detection + composition.
pub struct Prepared {
    /// Repo base (git root or cwd).
    pub repo_base: PathBuf,
    /// Merged configuration.
    pub config: Config,
    /// Detected context.
    pub context: Context,
    /// Composed capabilities + matching profiles.
    pub composition: Composition,
}

impl Prepared {
    /// The display/audit label for this render (primary matching profile).
    pub fn profile_label(&self) -> &str {
        self.composition.label()
    }
}

/// Load config, detect context, and compose capabilities for `rt`.
pub fn prepare(rt: &Runtime) -> crate::Result<Prepared> {
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let context = context::detect_context(&rt.cwd, &config).context("detecting context")?;
    let composition = profile::compose(&context, &config.profiles, &config.capabilities);
    Ok(Prepared {
        repo_base,
        config,
        context,
        composition,
    })
}

/// Current time as an RFC3339 (`…Z`) string, injected into renders.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Resolve an `--agent` value (id, `all`, or absent → default) to concrete ids,
/// validating against the configured agents.
pub fn resolve_agents(arg: Option<&str>, config: &Config) -> crate::Result<Vec<String>> {
    let ids = adapters::agent_ids(config);
    match arg {
        Some("all") => Ok(ids),
        Some(id) => {
            if ids.iter().any(|a| a == id) {
                Ok(vec![id.to_string()])
            } else {
                bail!("unknown agent '{id}' (known: {})", ids.join(", "))
            }
        }
        None => {
            let def = config.default_agent.clone();
            if ids.iter().any(|a| a == &def) {
                Ok(vec![def])
            } else {
                bail!("default agent '{def}' is not configured")
            }
        }
    }
}

/// The sample repo config written by `rosita init`.
///
/// Embedded from `examples/config.toml` so the scaffolded config and the
/// documented example are the same single source of truth.
pub const SAMPLE_REPO_CONFIG: &str = include_str!("../../examples/config.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_config_parses() {
        // Ensure the scaffolded sample is always valid against the parser.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".rosita")).unwrap();
        std::fs::write(
            crate::config::repo_config_path(d.path()),
            SAMPLE_REPO_CONFIG,
        )
        .unwrap();
        // No global layer → fully hermetic.
        let cfg = Config::load_from(None, d.path()).expect("sample config must parse");
        assert_eq!(cfg.default_agent, "claude");
        assert!(cfg.profiles.iter().any(|p| p.name == "infra"));
    }

    #[test]
    fn resolve_agents_defaults_all_and_unknown() {
        let cfg = Config::defaults();
        assert_eq!(
            resolve_agents(None, &cfg).unwrap(),
            vec!["claude".to_string()]
        );
        // `all` expands to every built-in agent (now six).
        let all = resolve_agents(Some("all"), &cfg).unwrap();
        assert!(all.contains(&"gemini".to_string()));
        assert!(all.contains(&"copilot".to_string()));
        assert!(all.contains(&"opencode".to_string()));
        // A specific id resolves to itself; unknown ids error.
        assert_eq!(resolve_agents(Some("codex"), &cfg).unwrap(), vec!["codex"]);
        assert!(resolve_agents(Some("nope"), &cfg).is_err());
    }
}
