//! `load clean` — remove loadout-generated overlays and managed blocks.
//!
//! Removes only what loadout created (gitignored overlays, override files, and
//! our managed marker block in importer files). Hand-authored, committed
//! instruction files (`AGENTS.md`, `GEMINI.md`, `copilot-instructions.md`) are
//! never touched.

use anyhow::anyhow;

use super::{now_rfc3339, prepare, resolve_agents, Prepared, Runtime};
use crate::adapters::{self, AppContext};
use crate::cli::CleanArgs;
use crate::writer::AtomicWriter;

/// Entry point for `load clean`.
pub fn run(rt: &Runtime, args: &CleanArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;

    let agents: Vec<String> = match &args.agent {
        Some(_) => resolve_agents(args.agent.as_deref(), &prep.config)?,
        None => agents_with_artifacts(&prep),
    };

    let writer = AtomicWriter::new(rt.dry_run);
    if rt.dry_run {
        println!("dry run — nothing will be removed\n");
    }

    for agent in &agents {
        let descriptor = adapters::descriptor(&prep.config, agent)
            .ok_or_else(|| anyhow!("unknown agent '{agent}'"))?;
        let app = AppContext {
            context: &prep.context,
            composition: &prep.composition,
            config: &prep.config,
            generated_at: now_rfc3339(),
            writer: &writer,
        };
        let result = adapters::clean(descriptor, &app)?;

        println!("{agent}");
        for p in &result.removed {
            println!(
                "  {:<10} {}",
                if rt.dry_run { "would rm" } else { "removed" },
                p.display()
            );
        }
        for p in &result.modified {
            println!(
                "  {:<10} {} (managed block stripped)",
                if rt.dry_run { "would edit" } else { "edited" },
                p.display()
            );
        }
        if result.removed.is_empty() && result.modified.is_empty() {
            println!("  (no artifacts)");
        }
        for note in &result.notes {
            println!("  note: {note}");
        }
        println!();
    }

    let plan_removed = super::plan::clean_artifacts(&prep.repo_base, rt.dry_run)?;
    if !plan_removed.is_empty() {
        println!("plan");
        for p in &plan_removed {
            println!(
                "  {:<10} {}",
                if rt.dry_run { "would rm" } else { "removed" },
                p.display()
            );
        }
    }

    if agents.is_empty() && plan_removed.is_empty() {
        println!("nothing to clean (no loadout artifacts found).");
    }
    Ok(())
}

/// Agents that currently have loadout artifacts on disk.
fn agents_with_artifacts(prep: &Prepared) -> Vec<String> {
    prep.config
        .agents
        .iter()
        .filter(|a| !adapters::artifacts(a, &prep.repo_base).is_empty())
        .map(|a| a.id.clone())
        .collect()
}
