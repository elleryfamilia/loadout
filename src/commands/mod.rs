//! Implementations of the CLI subcommands.
//!
//! Each subcommand is a thin function over the library. Shared plumbing —
//! loading config, detecting context, selecting a profile, and resolving the
//! target agent set — lives here.

pub mod apply;
pub mod bind;
pub mod clean;
pub mod detect;
pub mod doctor;
pub mod edit;
pub mod explain;
pub mod hook;
pub mod introspect;
pub mod plan;
pub mod refresh;
pub mod run;
pub mod skill;
pub mod sync;
pub mod update;

use std::path::PathBuf;

use anyhow::{bail, Context as _};

use crate::adapters;
use crate::binding::{self, Binding};
use crate::config::Config;
use crate::context::{self, Context};
use crate::profile::{self, Composition, LoadoutConfig, Selection};

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
/// [`SkipChooser`] never prompts; `load run` injects an interactive one.
pub trait ProfileChooser {
    /// Pick among `candidates` for `ctx`. Implementations may prompt the user.
    fn choose(&self, ctx: &Context, candidates: &[LoadoutConfig]) -> crate::Result<Choice>;
}

/// A chooser's answer to an ambiguous selection.
pub enum Choice {
    /// Use this profile (by name) and remember the choice.
    Profile(String),
    /// Don't decide now (e.g. non-interactive can't prompt): no profile applies,
    /// nothing persisted. The caller renders the empty overlay + warns.
    Skip,
    /// The user cancelled the interactive prompt — abort the command (don't
    /// apply a profile, don't launch). Surfaced as [`Aborted`].
    Abort,
}

/// Marker error for a user-cancelled interactive prompt (the profile chooser).
/// `load run` catches it and exits cleanly (`Ok(())`) without launching.
#[derive(Debug)]
pub struct Aborted;

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled by user")
    }
}

impl std::error::Error for Aborted {}

/// What [`prepare_with`] does with fragment ids a profile references but that
/// aren't in the library (they would otherwise be silently dropped). Most
/// commands `Warn`; `load run` uses `Defer` so it can prompt interactively.
pub enum MissingPolicy {
    /// Emit a `warning:` line per missing fragment, then continue.
    Warn,
    /// Emit nothing — the caller inspects [`Prepared::composition`]`.missing`.
    Defer,
}

/// The default non-interactive chooser: warns and applies no profile when a
/// project matches 2+ profiles. Used by every command except `run`.
pub struct SkipChooser;

impl ProfileChooser for SkipChooser {
    fn choose(&self, _ctx: &Context, candidates: &[LoadoutConfig]) -> crate::Result<Choice> {
        let names: Vec<&str> = candidates.iter().map(|p| p.name.as_str()).collect();
        crate::warn_user!(
            "{} loadouts match this project ({}); none chosen — overlay is empty. \
             Run `load run <agent>` to pick one (remembered afterwards).",
            candidates.len(),
            names.join(", ")
        );
        Ok(Choice::Skip)
    }
}

/// Load config, detect context, select a profile, and compose its fragments
/// for `rt` (non-interactively — see [`prepare_with`] to supply a chooser).
/// Detection is **cache-only** for script-predicate targets — the safe default
/// for inspection commands (`explain`, `detect`, `doctor`, …). Real render
/// commands use [`prepare_live`].
pub fn prepare(rt: &Runtime) -> crate::Result<Prepared> {
    prepare_with(rt, &SkipChooser, MissingPolicy::Warn)
}

/// Like [`prepare`] but **live**: script-predicate custom targets may execute
/// during detection. For `render`/`refresh` (and `run`, via [`prepare_with`]).
pub fn prepare_live(rt: &Runtime) -> crate::Result<Prepared> {
    prepare_with_live(rt, &SkipChooser, MissingPolicy::Warn, true)
}

/// Like [`prepare`] but resolves an ambiguous selection via `chooser` (which may
/// prompt and persist the choice as a [`Binding`]) and lets the caller choose
/// how missing-fragment references are surfaced via `missing`. Detection is
/// cache-only; use [`prepare_with_live`] to detect live.
pub fn prepare_with(
    rt: &Runtime,
    chooser: &dyn ProfileChooser,
    missing: MissingPolicy,
) -> crate::Result<Prepared> {
    prepare_with_live(rt, chooser, missing, false)
}

/// The full [`prepare`] with both a `chooser` and an explicit `live` detection
/// policy (`true` lets script-predicate targets execute).
pub fn prepare_with_live(
    rt: &Runtime,
    chooser: &dyn ProfileChooser,
    missing: MissingPolicy,
    live: bool,
) -> crate::Result<Prepared> {
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let context =
        context::detect_context_with(&rt.cwd, &config, live).context("detecting context")?;

    let remembered = binding::read(&context);
    let selection = profile::select(&context, &config.profiles, remembered.as_ref());
    // On a real render (not a read-only inspection), tell the user when nothing
    // matched — either falling back to a no-targets default profile, or noting
    // the empty overlay and how to fix it. Off-repo the default fallback IS the
    // expected path (running in $HOME etc.), so only in-repo warrants a warning;
    // the render step line already names the loadout used either way.
    if live {
        match &selection {
            Selection::Default(_) if context.scope() == context::Scope::Machine => {}
            Selection::Default(p) => crate::warn_user!(
                "no loadout targets this project ({}); using the default loadout '{}' \
                 (it declares no targets).",
                detected_summary(&context),
                p.name
            ),
            Selection::None => crate::warn_user!(
                "no loadout targets this project ({}) — overlay is empty. Create a loadout \
                 (one with no targets becomes the default).",
                detected_summary(&context)
            ),
            _ => {}
        }
    }
    let resolved = resolve_selection(rt, &context, selection, chooser)?;

    let composition = profile::compose_selection(
        &context,
        &resolved,
        &config.fragments,
        &config.fragment_params,
    );
    if let MissingPolicy::Warn = missing {
        for m in &composition.missing {
            crate::warn_user!("unknown fragment '{}' ({})", m.id, m.provenance);
        }
    }
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
                .ok_or_else(|| anyhow::anyhow!("chooser returned unknown loadout '{name}'"))?;
            if !rt.dry_run {
                // Fingerprint the chosen profile's targets so the binding goes
                // stale (and re-detects) if the profile is later retargeted.
                let targets_hash = Some(crate::hash::context_hash(&chosen.targets));
                binding::write(context, &Binding::Profile { name, targets_hash })
                    .context("remembering loadout choice")?;
            }
            Ok(Selection::Use(chosen))
        }
        // Non-interactive can't pick → no profile applies (nothing persisted).
        Choice::Skip => Ok(Selection::None),
        // Interactive cancel → abort the command cleanly (no profile, no launch).
        Choice::Abort => Err(Aborted.into()),
    }
}

/// A short human summary of what detection found, for the no-profile notice
/// (e.g. `stack java, language Java` or `off-repo`).
fn detected_summary(ctx: &Context) -> String {
    let mut parts = Vec::new();
    if !ctx.stacks.is_empty() {
        parts.push(format!("stack {}", ctx.stacks.join("/")));
    }
    if !ctx.custom_targets.is_empty() {
        parts.push(format!("target {}", ctx.custom_targets.join("/")));
    }
    if !ctx.languages.is_empty() {
        parts.push(format!("language {}", ctx.languages.join("/")));
    }
    if ctx.scope() == crate::context::Scope::Machine {
        parts.push("off-repo".to_string());
    }
    if parts.is_empty() {
        "nothing detected".to_string()
    } else {
        parts.join(", ")
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
        // Accept a launch-program name as an alias for the id (`cursor-agent`
        // → `cursor`): people type the binary they know.
        Some(id) => match adapters::resolve_agent_token(config, id) {
            Some(d) => Ok(vec![d.id.clone()]),
            None => bail!("unknown agent '{id}' (known: {})", ids.join(", ")),
        },
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
