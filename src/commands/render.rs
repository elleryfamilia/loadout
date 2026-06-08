//! `rosita render` — render overlays for one or more agents and wire them up.

use anyhow::anyhow;

use super::{now_rfc3339, prepare_live, resolve_agents, Prepared, Runtime};
use crate::adapters::{self, AppContext, ApplyOptions, ApplyResult};
use crate::audit::{self, AuditEvent};
use crate::cli::RenderArgs;
use crate::hash;
use crate::warn_user;
use crate::writer::AtomicWriter;

/// Entry point for `rosita render`.
pub fn run(rt: &Runtime, args: &RenderArgs) -> crate::Result<()> {
    let prep = prepare_live(rt)?;
    let agents = resolve_agents(args.agent.as_deref(), &prep.config)?;
    let opts = ApplyOptions {
        codex_override: args.codex_override,
        codex_no_override: args.codex_no_override,
        force: args.force,
    };
    if rt.dry_run {
        println!("dry run — no files will be written\n");
    }
    let results = apply_for_agents(rt, &prep, &agents, &opts)?;
    for (agent, result) in &results {
        print_result(agent, prep.profile_label(), result);
    }
    Ok(())
}

/// Render + apply for each agent id and audit each, returning the per-agent
/// results (the caller decides how to present them — detailed for
/// `render`/`refresh`, a concise summary for `run`).
///
/// Shared by `render`, `refresh`, and the pre-launch step of `run`.
pub fn apply_for_agents(
    rt: &Runtime,
    prep: &Prepared,
    agents: &[String],
    opts: &ApplyOptions,
) -> crate::Result<Vec<(String, ApplyResult)>> {
    let writer = AtomicWriter::new(rt.dry_run);
    let generated_at = now_rfc3339();

    let mut results = Vec::with_capacity(agents.len());
    for agent in agents {
        let descriptor = adapters::descriptor(&prep.config, agent)
            .ok_or_else(|| anyhow!("unknown agent '{agent}'"))?;
        let app = AppContext {
            context: &prep.context,
            composition: &prep.composition,
            config: &prep.config,
            generated_at: generated_at.clone(),
            writer: &writer,
        };
        let result = adapters::apply(descriptor, &app, opts)?;

        // Dry-run must not touch disk at all — including the audit log.
        if !rt.dry_run {
            let event = AuditEvent {
                timestamp: generated_at.clone(),
                agent: agent.clone(),
                profile: prep.profile_label().to_string(),
                fragments: prep
                    .composition
                    .fragments
                    .iter()
                    .map(|c| c.fragment.id.clone())
                    .collect(),
                stacks: prep.context.stacks.clone(),
                files: result.files.clone(),
                reasons: prep.composition.reasons.clone(),
                context_hash: result.context_hash.clone(),
                dry_run: false,
            };
            if let Err(e) = audit::record(&prep.repo_base, &event) {
                warn_user!("could not write audit log: {e:#}");
            }
        }

        results.push((agent.clone(), result));
    }
    Ok(results)
}

pub(crate) fn print_result(agent: &str, profile_label: &str, result: &ApplyResult) {
    println!(
        "{agent}  ·  profile {profile_label}  ·  {}",
        hash::short(&result.context_hash)
    );
    for f in &result.files {
        println!("  {:<13} {}", f.action.label(), f.path.display());
    }
    for note in &result.notes {
        println!("  note: {note}");
    }
    for w in &result.warnings {
        println!("  ⚠️  {w}");
    }
    println!();
}
