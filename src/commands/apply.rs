//! Shared render/apply plumbing for `refresh` and the pre-launch step of
//! `run`: compose-and-write overlays for a set of agents, plus the auto-pull
//! sync step and its one-line status display.

use std::time::Duration;

use anyhow::anyhow;

use super::{now_rfc3339, Prepared, Runtime};
use crate::adapters::{self, AppContext, ApplyOptions, ApplyResult};
use crate::audit::{self, AuditEvent};
use crate::hash;
use crate::style::Painter;
use crate::sync::{self, SyncStatus};
use crate::warn_user;
use crate::writer::AtomicWriter;

/// Render + apply for each agent id and audit each, returning the per-agent
/// results (the caller decides how to present them — detailed for `refresh`,
/// a concise summary for `run`).
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

// --- the auto-pull sync step --------------------------------------------------

/// Best-effort auto-pull of the global config before rendering. Loads config to
/// read `[sync]`, then pulls if enabled + stale (the caller's subsequent
/// `prepare_*` re-reads the now-current config). Never fails — errors map to
/// `Offline`. Inert on `--dry-run`: a pull mutates the config repo, and dry
/// runs must not touch disk or network.
pub(crate) fn sync_before_render(rt: &Runtime) -> SyncStatus {
    if rt.dry_run {
        return SyncStatus::Disabled;
    }
    let Ok(dir) = sync::config_dir() else {
        return SyncStatus::Disabled;
    };
    let repo_base = crate::context::repo_base_for(&rt.cwd);
    match crate::config::Config::load(&repo_base) {
        Ok(cfg) => sync::auto_pull(&cfg.sync, &dir),
        Err(_) => SyncStatus::Disabled,
    }
}

/// `  <glyph> <label>  <detail>` — one aligned step line.
pub(crate) fn step(p: &Painter, glyph: String, label: &str, detail: String) -> String {
    format!("  {glyph} {}  {detail}", p.bold(&format!("{label:<6}")))
}

pub(crate) fn print_sync_step(p: &Painter, s: &SyncStatus) {
    let line = match s {
        SyncStatus::Disabled => return,
        SyncStatus::Skipped { age } => step(
            p,
            p.green("✓"),
            "sync",
            format!(
                "up to date {}",
                p.dim(&format!("· synced {}", age_ago(*age)))
            ),
        ),
        SyncStatus::UpToDate => step(
            p,
            p.green("✓"),
            "sync",
            format!("up to date {}", p.dim("· synced just now")),
        ),
        SyncStatus::Pulled {
            commits,
            remote,
            took,
        } => step(
            p,
            p.green("⟳"),
            "sync",
            format!(
                "pulled {} {}",
                changes(*commits),
                p.dim(&format!("· {remote}  {}", dur(*took)))
            ),
        ),
        SyncStatus::Offline { last } => step(
            p,
            p.yellow("⚠"),
            "sync",
            format!(
                "offline — using local config{}",
                last.map(|a| p.dim(&format!(" · synced {}", age_ago(a))))
                    .unwrap_or_default()
            ),
        ),
        SyncStatus::Diverged => step(
            p,
            p.yellow("⚠"),
            "sync",
            "diverged — run `rosita sync` to reconcile".to_string(),
        ),
    };
    println!("{line}");
}

/// "1 change" / "N changes".
fn changes(n: usize) -> String {
    if n == 1 {
        "1 change".to_string()
    } else {
        format!("{n} changes")
    }
}

/// "just now" / "Nm ago" / "Nh ago" / "Nd ago".
pub(crate) fn age_ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        "just now".to_string()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

/// "320ms" / "1.3s".
fn dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}
