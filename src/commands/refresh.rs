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
use crate::config::{self, Config};
use crate::learn::trigger::{maybe_spawn, Trigger};
use crate::learn::{state as learn_state, worker as learn_worker};
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
    // Folded ONCE for this whole invocation (zero cost when `[learn]` is
    // disabled) — every agent's header gets the same count.
    let learn_pending = apply::learn_pending_count(&prep.config);
    let results = apply::apply_for_agents(rt, &prep, &agents, &opts, learn_pending)?;
    for (agent, result) in &results {
        apply::print_result(agent, prep.profile_label(), result);
    }

    // Trigger fast path: never blocks, never errors outward. Skipped on
    // dry-run — a dry run must have no side effects beyond its own output.
    if !rt.dry_run {
        maybe_spawn(&prep.config, Trigger::Refresh);

        // Ambient-run summary: "learning: harvested N sessions via CLI
        // (model) — M new candidates", printed at most once per new ambient
        // run (design's "Refresh" surface). Behind the same dry-run gate as
        // the trigger: it writes the `last-notified` stamp when it prints, and
        // a dry run must neither leave that file behind nor consume the real
        // refresh's one-time notification.
        print_learn_summary(&prep.config);
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

/// Print, at most once per new ambient run, a summary of the latest completed
/// **ambient** harvest: `learning: harvested N sessions via CLI (model) — M
/// new candidates`. Compares the latest such log entry's timestamp against a
/// `last-notified` stamp (the same on-disk stamp mechanics as the trigger's
/// scan/spend stamps — see `crate::learn::state`), bumping the stamp after
/// printing so the same run is never announced twice. Guarded behind
/// `[learn] enabled` (consistent with every other reader of learn state —
/// disabled users pay no file read here either). The CALLER must gate this
/// behind `!rt.dry_run`: printing bumps the stamp, so a dry run reaching here
/// would both leave a file behind and steal the real refresh's one-time
/// notification.
fn print_learn_summary(cfg: &Config) {
    if !cfg.learn.enabled {
        return;
    }
    let Some(learn_dir) = learn_state::learn_dir() else {
        return;
    };
    let Some(entry) = learn_worker::latest_ambient_extraction(&learn_dir.join("log.jsonl")) else {
        return;
    };
    let Some(entry_secs) = entry.ts_unix() else {
        return;
    };
    let stamp_path = learn_dir.join("last-notified");
    let last_secs = learn_state::read_stamp(&stamp_path)
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    if let Some(last) = last_secs {
        if entry_secs <= last {
            return; // already notified for this (or a newer) run
        }
    }
    println!(
        "learning: harvested {} sessions via {} ({}) — {} new candidates",
        entry.sessions,
        entry.cli.as_deref().unwrap_or("cli"),
        entry.model.as_deref().unwrap_or("model"),
        entry.candidates,
    );
    let _ = learn_state::write_stamp(&stamp_path);
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
