//! Implementations of the CLI subcommands.
//!
//! Each subcommand is a thin function over the library. Shared plumbing —
//! loading config, detecting context, selecting a profile, and resolving the
//! target agent set — lives here.

pub mod clean;
pub mod detect;
pub mod doctor;
pub mod explain;
pub mod introspect;
pub mod refresh;
pub mod render;
pub mod run;
pub mod sync;

use std::path::PathBuf;

use anyhow::{bail, Context as _};

use crate::adapters;
use crate::binding::{self, Binding};
use crate::config::Config;
use crate::context::{self, Context};
use crate::profile::{self, Composition, ProfileConfig, Selection};

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
    /// Composed fragments + matching profiles.
    pub composition: Composition,
}

impl Prepared {
    /// The display/audit label for this render (primary matching profile).
    pub fn profile_label(&self) -> &str {
        self.composition.label()
    }
}

/// Best-effort check that `program` is an executable on `$PATH` (no subprocess).
/// A path with a separator is checked directly; a bare name is searched on PATH.
pub fn program_on_path(program: &str) -> bool {
    let p = std::path::Path::new(program);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// How an ambiguous (2+ matching) profile selection is resolved. The default
/// [`SkipChooser`] never prompts; `rosita run` injects an interactive one.
pub trait ProfileChooser {
    /// Pick among `candidates` for `ctx`. Implementations may prompt the user.
    fn choose(&self, ctx: &Context, candidates: &[ProfileConfig]) -> crate::Result<Choice>;
}

/// A chooser's answer to an ambiguous selection.
pub enum Choice {
    /// Use this profile (by name) and remember the choice.
    Profile(String),
    /// Apply no profile here and remember the opt-out.
    None,
    /// Don't decide now (e.g. non-interactive): no profile, nothing persisted.
    Skip,
}

/// The default non-interactive chooser: warns and applies no profile when a
/// project matches 2+ profiles. Used by every command except `run`.
pub struct SkipChooser;

impl ProfileChooser for SkipChooser {
    fn choose(&self, _ctx: &Context, candidates: &[ProfileConfig]) -> crate::Result<Choice> {
        let names: Vec<&str> = candidates.iter().map(|p| p.name.as_str()).collect();
        crate::warn_user!(
            "{} profiles match this project ({}); none chosen — overlay is empty. \
             Run `rosita run <agent>` to pick one (remembered afterwards).",
            candidates.len(),
            names.join(", ")
        );
        Ok(Choice::Skip)
    }
}

/// Load config, detect context, select a profile, and compose its fragments
/// for `rt` (non-interactively — see [`prepare_with`] to supply a chooser).
pub fn prepare(rt: &Runtime) -> crate::Result<Prepared> {
    prepare_with(rt, &SkipChooser)
}

/// Like [`prepare`] but resolves an ambiguous selection via `chooser` (which may
/// prompt and persist the choice as a [`Binding`]).
pub fn prepare_with(rt: &Runtime, chooser: &dyn ProfileChooser) -> crate::Result<Prepared> {
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let context = context::detect_context(&rt.cwd, &config).context("detecting context")?;

    let remembered = binding::read(&context);
    let selection = profile::select(&context, &config.profiles, remembered.as_ref());
    let resolved = resolve_selection(rt, &context, selection, chooser)?;

    let composition = profile::compose_selection(
        &context,
        &resolved,
        &config.fragments,
        &config.fragment_params,
    );
    Ok(Prepared {
        repo_base,
        config,
        context,
        composition,
    })
}

/// Resolve a [`Selection`] to a concrete one. `Ambiguous` is handed to the
/// chooser; a real choice is persisted as a binding (unless dry-run).
fn resolve_selection(
    rt: &Runtime,
    context: &Context,
    selection: Selection,
    chooser: &dyn ProfileChooser,
) -> crate::Result<Selection> {
    let Selection::Ambiguous(candidates) = selection else {
        return Ok(selection);
    };
    match chooser.choose(context, &candidates)? {
        Choice::Profile(name) => {
            let chosen = candidates
                .iter()
                .find(|p| p.name == name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("chooser returned unknown profile '{name}'"))?;
            if !rt.dry_run {
                // Fingerprint the chosen profile's targets so the binding goes
                // stale (and re-detects) if the profile is later retargeted.
                let targets_hash = Some(crate::hash::context_hash(&chosen.targets));
                binding::write(context, &Binding::Profile { name, targets_hash })
                    .context("remembering profile choice")?;
            }
            Ok(Selection::Use(chosen))
        }
        Choice::None => {
            if !rt.dry_run {
                binding::write(context, &Binding::None).context("remembering profile opt-out")?;
            }
            Ok(Selection::None)
        }
        Choice::Skip => Ok(Selection::None),
    }
}

/// Current time as an RFC3339 (`…Z`) string, injected into renders.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Current UTC time, injected into probe cache-freshness checks.
pub fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
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

#[cfg(test)]
mod tests {
    use super::*;

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
