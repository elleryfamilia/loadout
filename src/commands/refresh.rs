//! `load refresh` — pull the latest config, then (re-)render overlays.
//!
//! Without `--agent`, refreshes every agent whose generated overlay already
//! exists; if none do, falls back to the default agent. With `--agent` it
//! renders and wires that agent even if it was never initialized here — this
//! is also how an agent is first adopted in a repo. Hash-skipping means a
//! refresh with no context change is a cheap no-op (unless `--force`).

use super::apply::{self, print_sync_step, sync_before_render};
use super::{prepare_live, resolve_agents, Prepared, Runtime};
use crate::adapters::ApplyOptions;
use crate::cli::RefreshArgs;
use crate::config;
use crate::style::Painter;

/// Entry point for `load refresh`.
pub fn run(rt: &Runtime, args: &RefreshArgs) -> crate::Result<()> {
    let p = Painter::auto();

    // Pull the latest config first — best-effort, throttled, timeout-bounded;
    // it never blocks the refresh. Done before `prepare_live` so the render
    // below composes freshly-pulled fragments/profiles.
    let sync_status = sync_before_render(rt);
    print_sync_step(&p, &sync_status);

    let prep = prepare_live(rt)?;

    // Passive hook bootstrap: any refresh registers the IDE freshness hooks of
    // agents that are installed (e.g. ~/.cursor exists), so no one ever has to
    // run `refresh --agent cursor` just for the hook.
    for note in crate::adapters::bootstrap_hook_registrations(&prep.config, rt.dry_run) {
        println!("  note: {note}");
    }

    let agents: Vec<String> = match &args.agent {
        Some(_) => resolve_agents(args.agent.as_deref(), &prep.config)?,
        None => {
            let existing = existing_overlay_agents(&prep);
            if existing.is_empty() {
                println!(
                    "no generated overlays found; rendering the default agent ({})",
                    prep.config.default_agent
                );
                vec![prep.config.default_agent.clone()]
            } else {
                existing
            }
        }
    };

    let opts = ApplyOptions {
        codex_override: args.codex_override,
        codex_no_override: args.codex_no_override,
        force: args.force,
        // refresh uses the profile's bound workflow; the override is run-only.
        workflow_override: None,
    };
    if rt.dry_run {
        println!("dry run — no files will be written\n");
    }
    let results = apply::apply_for_agents(rt, &prep, &agents, &opts)?;
    for (agent, result) in &results {
        apply::print_result(agent, prep.profile_label(), result);
    }

    // Fast config health pass — doctor's pure subset. Warnings only, zero
    // output when healthy, never injected into rendered context files.
    for f in super::checks::refresh_subset(&prep.config, &prep.repo_base) {
        if f.status != super::checks::Status::Ok {
            println!("  ⚠  {}", f.message);
        }
    }
    Ok(())
}

/// Which agents already have a generated overlay on disk. Also the adoption
/// test `load hook` uses to decide whether a workspace root is loadout-managed.
pub(crate) fn existing_overlay_agents(prep: &Prepared) -> Vec<String> {
    let dir = config::generated_dir(&prep.repo_base);
    prep.config
        .agents
        .iter()
        .filter(|a| dir.join(&a.generated_filename).exists())
        .map(|a| a.id.clone())
        .collect()
}
