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
use crate::learn::worker::{self, HarvestDiagnostic, Outcome};
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

/// A single, plain-language line describing the run's result. Shared with
/// `load learn on`'s offered first run.
pub fn print_summary(out: &worker::RunOutcome) {
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
        Outcome::Throttled => println!(
            "{}",
            p.dim(
                "learning: ambient run throttled — the spend interval hasn't elapsed \
                 (a manual `load harvest` runs regardless)"
            )
        ),
        Outcome::NoCli => println!(
            "{}",
            p.dim(
                "learning: found new sessions but no extraction CLI is installed \
                 (set `learn.cli` or install claude/codex/gemini)"
            )
        ),
        Outcome::UnsupportedCli => print_failure(&p, out),
        Outcome::Busy => println!("{}", p.dim("learning: a harvest is already running")),
        Outcome::Fenced => println!("{}", p.dim("learning: another run took over; nothing done")),
        Outcome::Corrupt => print_failure(&p, out),
        Outcome::Failed | Outcome::Deadline => print_failure(&p, out),
    }
}

fn print_failure(p: &Painter, out: &worker::RunOutcome) {
    let diagnostic = out
        .diagnostic
        .as_ref()
        .map(format_diagnostic)
        .unwrap_or_else(legacy_diagnostic);
    println!(
        "{} learning: harvest run failed ({}) — {}",
        p.yellow("!"),
        out.outcome.label(),
        diagnostic
    );
}

/// Format one worker-owned diagnostic consistently across manual harvest,
/// status, and doctor.
pub(crate) fn format_diagnostic(diagnostic: &HarvestDiagnostic) -> String {
    format!(
        "{}/{}: {}",
        diagnostic.stage(),
        diagnostic.code(),
        diagnostic.message()
    )
}

/// Format a typed diagnostic read back from the append-only run log. Persisted
/// human text remains untrusted even when the stage/code look familiar, so the
/// message is reconstructed from this loadout-owned allowlist. Legacy or
/// future records get a generic fallback.
pub(crate) fn format_logged_diagnostic(record: &worker::LogRecord) -> String {
    match (record.error_stage.as_deref(), record.error_code.as_deref()) {
        (Some(stage), Some(code)) => logged_message(stage, code, record.cli.as_deref())
            .map(|message| format!("{stage}/{code}: {message}"))
            .unwrap_or_else(legacy_diagnostic),
        _ => legacy_diagnostic(),
    }
}

fn legacy_diagnostic() -> String {
    "diagnostic unavailable: this loadout version did not record a safe diagnostic".to_string()
}

fn logged_message(stage: &str, code: &str, cli: Option<&str>) -> Option<String> {
    let message = match (stage, code) {
        ("preflight", "run_deadline_exceeded") => {
            "The harvest run exceeded its deadline before extraction started.".to_string()
        }
        ("preflight", "claude_structured_output_unsupported") => format!(
            "Claude Code cannot provide the required structured output. Upgrade to {} or newer.",
            crate::learn::agent_cli::CLAUDE_MIN_VERSION
        ),
        ("preflight", "watermark_store_corrupt") => {
            "The learning watermark store is corrupt. Run `load learn reset` to re-baseline it."
                .to_string()
        }
        ("preflight", "stale_lock_reclaimed") => {
            "Loadout reclaimed a stale harvest lock left by an interrupted run.".to_string()
        }
        ("spend_guard", "spend_stamp_write_failed") => {
            "Loadout could not write the harvest spend stamp. Check permissions and available disk space, then run `load harvest` again."
                .to_string()
        }
        ("invoke", "cli_spawn_failed") => {
            "Loadout could not start the extraction CLI.".to_string()
        }
        ("invoke", "cli_timed_out") => {
            "The extraction CLI exceeded its deadline.".to_string()
        }
        ("invoke", "cli_process_failed") => {
            "The extraction CLI exited unsuccessfully.".to_string()
        }
        ("cli_output", "cli_envelope_invalid") => {
            "The extraction CLI returned an invalid response envelope.".to_string()
        }
        ("cli_output", "cli_auth_failed") => match cli {
            Some("claude") => "Claude could not authenticate. Open Claude and sign in, then run `load harvest` again.".to_string(),
            Some("codex") => "Codex could not authenticate. Open Codex and sign in, then run `load harvest` again.".to_string(),
            Some("gemini") => "Gemini could not authenticate. Open Gemini and sign in, then run `load harvest` again.".to_string(),
            _ => "The extraction CLI could not authenticate. Sign in, then run `load harvest` again.".to_string(),
        },
        ("cli_output", "cli_rate_limited") => {
            "The extraction provider rate-limited this request. Wait, then run `load harvest` again."
                .to_string()
        }
        ("cli_output", "cli_structured_retries_exhausted") => {
            "The extraction CLI could not produce valid structured output. Upgrade the CLI if this repeats."
                .to_string()
        }
        ("cli_output", "cli_reported_error") => {
            "The extraction provider reported an error. Run `load harvest` again.".to_string()
        }
        ("cli_output", "provider_output_missing") => {
            "The extraction CLI did not return the required structured output.".to_string()
        }
        ("validate_output", "output_json_invalid") => {
            "The extraction CLI returned invalid JSON output.".to_string()
        }
        ("validate_output", "output_schema_mismatch") => {
            "The extraction CLI returned JSON that does not match the required schema.".to_string()
        }
        ("persist_journal", "journal_append_failed") => {
            "Loadout could not append extracted candidates to the learning journal. Check permissions and available disk space."
                .to_string()
        }
        _ => return None,
    };
    Some(message)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn log_record(
        stage: Option<&str>,
        code: Option<&str>,
        message: Option<&str>,
    ) -> worker::LogRecord {
        worker::LogRecord {
            ts: "2026-07-10T10:00:00Z".into(),
            trigger: "manual".into(),
            cli: Some("claude".into()),
            model: Some("haiku".into()),
            sessions: 1,
            candidates: 0,
            duration_ms: Some(10),
            usage: None,
            error_stage: stage.map(str::to_string),
            error_code: code.map(str::to_string),
            error: message.map(str::to_string),
            outcome: "failed".into(),
        }
    }

    #[test]
    fn logged_diagnostic_requires_a_known_typed_pair() {
        let typed = log_record(
            Some("validate_output"),
            Some("output_json_invalid"),
            Some("SECRET_FORGED_KNOWN_CODE_TEXT"),
        );
        let rendered = format_logged_diagnostic(&typed);
        assert_eq!(
            rendered,
            "validate_output/output_json_invalid: The extraction CLI returned invalid JSON output."
        );
        assert!(!rendered.contains("SECRET_FORGED_KNOWN_CODE_TEXT"));

        let legacy = log_record(None, None, Some("SECRET_LEGACY_PROVIDER_TEXT"));
        let rendered = format_logged_diagnostic(&legacy);
        assert!(rendered.contains("did not record a safe diagnostic"));
        assert!(!rendered.contains("SECRET_LEGACY_PROVIDER_TEXT"));

        let foreign = log_record(
            Some("provider_text"),
            Some("future_error"),
            Some("SECRET_PROVIDER_TEXT"),
        );
        assert!(!format_logged_diagnostic(&foreign).contains("SECRET_PROVIDER_TEXT"));
    }
}
