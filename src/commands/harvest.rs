//! `load harvest` — mine recent agent sessions for durable preferences into
//! the review inbox. A thin wrapper over [`crate::learn::worker::run_harvest`]:
//! it loads config and runs one bounded, fenced, logged harvest pass, then
//! prints a one-line summary of what happened.
//!
//! A bare `load harvest` is a **manual** run (foreground, outcome printed — the
//! user typed it and wants to see something happen). Ambient triggers pass the
//! hidden `--ambient` flag, which only changes the run's log label; the worker
//! path is identical either way.

use anyhow::{Context as _, Result};

use crate::cli::HarvestArgs;
use crate::config::Config;
use crate::context;
use crate::learn::worker::{self, Outcome};
use crate::style::Painter;

pub fn run(rt: &super::Runtime, args: &HarvestArgs) -> Result<()> {
    // A harvest both writes files (journal, evidence, watermarks, log) and
    // makes a metered extraction call — exactly the kind of side effect
    // `--dry-run` promises to suppress. So a dry run reports intent and does
    // nothing, rather than silently spending.
    if rt.dry_run {
        println!(
            "{} dry run: would harvest recent sessions (this makes a metered \
             extraction call and stages candidates); re-run without --dry-run",
            Painter::auto().dim("~")
        );
        return Ok(());
    }

    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;

    let outcome =
        worker::run_harvest(&config, !args.ambient).context("running the harvest worker")?;
    print_summary(&outcome);
    Ok(())
}

/// A single, plain-language line describing the run's result.
fn print_summary(out: &worker::RunOutcome) {
    let p = Painter::auto();
    match out.outcome {
        Outcome::Extracted => {
            let cli = out.cli.as_deref().unwrap_or("?");
            let model = out.model.as_deref().unwrap_or("?");
            println!(
                "{} harvested {} session{} via {} ({}) — {} new candidate{}{}",
                p.green("✓"),
                out.sessions,
                plural(out.sessions),
                cli,
                model,
                out.candidates,
                plural(out.candidates),
                quarantine_note(out.quarantined),
            );
        }
        Outcome::Empty => println!("{}", p.dim("learning: no new sessions to harvest")),
        Outcome::NoCli => println!(
            "{}",
            p.dim(
                "learning: found new sessions but no extraction CLI is installed \
                 (set `learn.cli` or install claude/codex/gemini)"
            )
        ),
        Outcome::Busy => println!("{}", p.dim("learning: a harvest is already running")),
        Outcome::Fenced => println!("{}", p.dim("learning: another run took over; nothing done")),
        Outcome::Corrupt => println!(
            "{} learning watermark store is corrupt — run `load learn reset`",
            p.yellow("!")
        ),
        Outcome::Failed | Outcome::Deadline => println!(
            "{} learning: harvest run failed ({}) — see the run log",
            p.yellow("!"),
            out.outcome.label()
        ),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn quarantine_note(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(" ({n} held by the injection lint)")
    }
}
