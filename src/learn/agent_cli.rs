//! One-shot agent-CLI runner for the harvest worker's single paid extraction
//! call: pick an installed CLI ([`select`]) and invoke it once, bounded, with
//! hygiene flags that keep the call from touching session state, tools, or
//! hooks ([`invoke`]).
//!
//! ## Why a separate one-shot runner
//!
//! The worker must call a coding-agent CLI (`claude`/`codex`/`gemini`) purely
//! as a text-in/JSON-out function: no interactive session, no tool use, no
//! session persistence, no hooks firing (a hook that itself triggered another
//! harvest would recurse). Every invocation therefore:
//!
//! - carries `LOADOUT_LEARN_WORKER=1` — the recursion guard other layers check
//!   before starting a worker (belt-and-suspenders alongside the per-CLI
//!   hook-disabling flags);
//! - passes the prompt on **stdin**, never argv (the extraction prompt embeds
//!   whole transcripts and can be hundreds of KB — well past `ARG_MAX`);
//! - runs under [`crate::providers::output_with_timeout_stdin`], inheriting its
//!   process-group kill and deadline-bounded pipe drain (a wedged CLI is killed
//!   at the deadline, its background grandchildren reaped);
//! - runs with `cwd = work_dir`, a caller-chosen scratch dir (codex writes its
//!   `--output-schema` file and reads its final-message file there; gemini's
//!   session/trust state stays scoped to it, not the user's real projects).
//!
//! ## Per-CLI hygiene flags and output unwrapping (pinned against the real
//! installed binaries — claude 2.1.206, codex-cli 0.142.0, gemini 0.45.2)
//!
//! - **claude** `-p --safe-mode --no-session-persistence --output-format json
//!   --tools "" [--model <model>]`. `--safe-mode` disables hooks, plugins, MCP
//!   servers, CLAUDE.md discovery, skills, and custom commands/agents while
//!   keeping auth working normally — unlike `--bare`, which additionally blocks
//!   OAuth/keychain reads and so returns "Not logged in" for a
//!   subscription-authenticated user (verified empirically). `--tools ""` is
//!   documented and verified to disable ALL built-in tools. Output: stdout is a
//!   single JSON object; final text is `.result`, usage blob is `.usage`, and
//!   `.is_error == true` is treated as a failed call.
//! - **codex** `exec -s read-only --ephemeral --skip-git-repo-check
//!   --output-schema <work_dir>/schema.json -o <work_dir>/last-message.txt
//!   --json [-m <model>]`. `-s read-only` forbids writes; `--ephemeral` skips
//!   session files; `--skip-git-repo-check` lets it run in a non-repo scratch
//!   dir; codex hooks are trust-gated and do not run without an explicit
//!   `--dangerously-bypass-hook-trust`. `--output-schema` is fed
//!   [`crate::learn::extract::output_json_schema`]. Final text is read from the
//!   `-o` file (codex writes exactly the final agent message there); usage blob
//!   is the `usage` object on the last `{"type":"turn.completed",...}` line of
//!   the `--json` stdout stream. An empty/absent `-o` file means the run
//!   failed.
//! - **gemini** `-p "" --approval-mode plan -o json --skip-trust [-m <model>]`.
//!   `-p ""` runs headless with the prompt taken from stdin; `--approval-mode
//!   plan` is read-only (no tool execution); `--skip-trust` is required because
//!   an untrusted scratch dir otherwise silently downgrades plan mode to
//!   `default` and refuses to run headless. Output: stdout JSON `.response` is
//!   the final text, `.stats` is the usage blob.
//!
//! Default cheap models when `learn.model` is unset: claude → the `haiku` alias
//! (resolves to a current haiku-class model), gemini → `gemini-2.5-flash` (the
//! CLI's own `DEFAULT_GEMINI_FLASH_MODEL`), codex → its configured default
//! (`-m` omitted). A set `learn.model` overrides all three.
//!
//! Everything the CLI returns is untrusted data — a transcript the model
//! summarized can contain injection attempts. This layer only extracts the text
//! and usage; validation of the output shape is [`crate::learn::extract`]'s job
//! and claim vetting is [`crate::learn::gate`]'s.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};

use crate::config::LearnConfig;
use crate::providers;

/// The extraction CLIs loadout knows how to drive, in probe order. `select`
/// walks this list; `learn.cli` (when set) restricts the walk to the one
/// matching id (no fallback — a pin means "this CLI or nothing").
const PROBE_ORDER: &[&str] = &["claude", "codex", "gemini"];

/// A resolved choice of extraction CLI: which one, the program to spawn, and
/// the model id to pass. `model` is empty when the CLI's own configured default
/// should be used (codex) — [`invoke`] then omits the `-m`/`--model` flag.
#[derive(Debug, Clone)]
pub struct CliChoice {
    /// One of `"claude"`, `"codex"`, `"gemini"`.
    pub cli_id: &'static str,
    /// The program name (resolved via `PATH`) or an absolute path.
    pub program: String,
    /// The model id, or empty to use the CLI's own default.
    pub model: String,
}

/// The usable result of one extraction call.
#[derive(Debug, Clone)]
pub struct InvokeOut {
    /// The model's final message, unwrapped from the CLI's output envelope. Raw
    /// untrusted text — validate before use.
    pub text: String,
    /// The CLI's raw usage/stats blob (serialized JSON) when it reports one,
    /// for the harvest log's token accounting.
    pub usage: Option<String>,
}

/// Pick the extraction CLI: `learn.cli` pins one (used only if that id is known
/// and installed — otherwise `None`, no fallback); else the first of
/// [`PROBE_ORDER`] that is installed, probed via
/// [`crate::providers::probe_cli`] (3s-bounded). `None` when nothing eligible
/// is installed.
pub fn select(learn: &LearnConfig) -> Option<CliChoice> {
    select_with(learn, |program| {
        matches!(providers::probe_cli(program), providers::CliProbe::Found(_))
    })
}

/// [`select`] with an injectable availability check, so the probe-order /
/// pin / skip-missing logic is unit-testable without spawning real CLIs.
fn select_with(learn: &LearnConfig, available: impl Fn(&str) -> bool) -> Option<CliChoice> {
    // A `learn.cli` pin restricts the candidates to that one known id; an
    // unknown pin yields an empty candidate list (→ None). No pin ⇒ full order.
    let candidates: Vec<&'static str> = match learn.cli.as_deref() {
        Some(pin) => PROBE_ORDER.iter().copied().filter(|c| *c == pin).collect(),
        None => PROBE_ORDER.to_vec(),
    };
    for cli_id in candidates {
        if available(cli_id) {
            let model = learn
                .model
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| default_model(cli_id).to_string());
            return Some(CliChoice {
                cli_id,
                program: cli_id.to_string(),
                model,
            });
        }
    }
    None
}

/// Default cheap model per CLI when `learn.model` is unset. Empty string means
/// "omit the model flag and let the CLI use its own configured default"
/// (codex). Pinned against each CLI's live model list at implementation time.
fn default_model(cli_id: &str) -> &'static str {
    match cli_id {
        "claude" => "haiku",
        "gemini" => "gemini-2.5-flash",
        // codex: use the account's configured default (omit -m).
        _ => "",
    }
}

/// Invoke `choice` once with `prompt` on stdin, `cwd = work_dir`, bounded by
/// `deadline`. Returns the unwrapped final text and usage blob, or `Err` if the
/// CLI could not be spawned, exceeded the deadline, produced unparseable
/// output, or reported an error result.
pub fn invoke(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut> {
    match choice.cli_id {
        "claude" => invoke_claude(choice, prompt, work_dir, deadline),
        "codex" => invoke_codex(choice, prompt, work_dir, deadline),
        "gemini" => invoke_gemini(choice, prompt, work_dir, deadline),
        other => bail!("unknown extraction CLI id {other:?}"),
    }
}

/// A `Command` for `program` carrying the invariants every extraction call
/// needs: `cwd = work_dir` and the `LOADOUT_LEARN_WORKER=1` recursion guard.
fn base_command(program: &str, work_dir: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(work_dir);
    cmd.env("LOADOUT_LEARN_WORKER", "1");
    cmd
}

/// Run `cmd` feeding `prompt` on stdin, bounded by `deadline`, via
/// [`crate::providers::output_with_timeout_stdin`]. `Err` on spawn failure or
/// deadline expiry; otherwise the captured [`std::process::Output`] (whatever
/// its exit status — the per-CLI unwrapper decides success from the payload,
/// since e.g. claude exits non-zero yet still emits a usable JSON envelope).
fn run_capture(
    cmd: &mut Command,
    prompt: &str,
    deadline: Duration,
) -> Result<std::process::Output> {
    match providers::output_with_timeout_stdin(cmd, deadline, prompt.as_bytes()) {
        Ok(Some(out)) => Ok(out),
        Ok(None) => bail!("extraction CLI exceeded its {deadline:?} deadline and was killed"),
        Err(e) => Err(anyhow!("spawning extraction CLI failed: {e}")),
    }
}

/// Append `-m <model>` (flag_name = "-m") only when a model id is set. codex
/// leaves it off to keep its configured default.
fn push_model(cmd: &mut Command, flag: &str, model: &str) {
    if !model.is_empty() {
        cmd.arg(flag).arg(model);
    }
}

fn invoke_claude(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut> {
    let mut cmd = base_command(&choice.program, work_dir);
    cmd.arg("-p")
        .arg("--safe-mode")
        .arg("--no-session-persistence")
        .arg("--output-format")
        .arg("json")
        .arg("--tools")
        .arg(""); // disable ALL built-in tools
    push_model(&mut cmd, "--model", &choice.model);

    let out = run_capture(&mut cmd, prompt, deadline)?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "claude --output-format json produced non-JSON stdout: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )
    })?;
    if value.get("is_error").and_then(|b| b.as_bool()) == Some(true) {
        let msg = value
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("(no result field)");
        bail!("claude reported an error result: {msg}");
    }
    let text = value
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow!("claude JSON is missing a string `result` field"))?
        .to_string();
    let usage = value.get("usage").map(|u| u.to_string());
    Ok(InvokeOut { text, usage })
}

fn invoke_codex(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut> {
    // Write the strict output schema codex should steer its structured output
    // toward, then clear any stale final-message file so an old one can't be
    // mistaken for this run's success.
    let schema_path = work_dir.join("schema.json");
    let schema = crate::learn::extract::output_json_schema();
    std::fs::write(&schema_path, serde_json::to_vec(&schema)?)
        .with_context(|| format!("writing codex output schema to {}", schema_path.display()))?;
    let last_path = work_dir.join("last-message.txt");
    let _ = std::fs::remove_file(&last_path);

    let mut cmd = base_command(&choice.program, work_dir);
    cmd.arg("exec")
        .arg("-s")
        .arg("read-only")
        .arg("--ephemeral")
        .arg("--skip-git-repo-check")
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("-o")
        .arg(&last_path)
        .arg("--json");
    push_model(&mut cmd, "-m", &choice.model);

    let out = run_capture(&mut cmd, prompt, deadline)?;
    let text = std::fs::read_to_string(&last_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "codex produced no final message (exec failed); stderr: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })?;
    let usage = codex_usage(&out.stdout);
    Ok(InvokeOut { text, usage })
}

/// The `usage` blob from the last `{"type":"turn.completed",...}` line of
/// codex's `--json` stdout stream, serialized. `None` if no such line carries a
/// usage object (e.g. a failed turn).
fn codex_usage(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            if value.get("type").and_then(|t| t.as_str()) == Some("turn.completed") {
                value.get("usage").map(|u| u.to_string())
            } else {
                None
            }
        })
}

fn invoke_gemini(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut> {
    let mut cmd = base_command(&choice.program, work_dir);
    cmd.arg("-p")
        .arg("") // headless; prompt comes from stdin
        .arg("--approval-mode")
        .arg("plan")
        .arg("-o")
        .arg("json")
        .arg("--skip-trust");
    push_model(&mut cmd, "-m", &choice.model);

    let out = run_capture(&mut cmd, prompt, deadline)?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "gemini -o json produced non-JSON stdout: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )
    })?;
    let text = value
        .get("response")
        .and_then(|r| r.as_str())
        .ok_or_else(|| {
            let err = value
                .get("error")
                .map(|e| e.to_string())
                .unwrap_or_default();
            if err.is_empty() {
                anyhow!("gemini JSON is missing a string `response` field")
            } else {
                anyhow!("gemini JSON is missing a string `response` field; error: {err}")
            }
        })?
        .to_string();
    let usage = value.get("stats").map(|s| s.to_string());
    Ok(InvokeOut { text, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learn(cli: Option<&str>, model: Option<&str>) -> LearnConfig {
        LearnConfig {
            enabled: true,
            interval: Duration::from_secs(6 * 3600),
            scope: crate::config::LearnScope::Adopted,
            cli: cli.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    // --- select: probe order, pin, skip-missing, model defaults -------------

    #[test]
    fn select_prefers_claude_when_all_available() {
        let choice = select_with(&learn(None, None), |_| true).unwrap();
        assert_eq!(choice.cli_id, "claude");
        assert_eq!(choice.program, "claude");
        assert_eq!(choice.model, "haiku");
    }

    #[test]
    fn select_skips_missing_and_falls_to_codex() {
        let choice = select_with(&learn(None, None), |p| p != "claude").unwrap();
        assert_eq!(choice.cli_id, "codex");
        // codex uses its configured default: empty model, `-m` omitted.
        assert_eq!(choice.model, "");
    }

    #[test]
    fn select_falls_through_to_gemini() {
        let choice = select_with(&learn(None, None), |p| p == "gemini").unwrap();
        assert_eq!(choice.cli_id, "gemini");
        assert_eq!(choice.model, "gemini-2.5-flash");
    }

    #[test]
    fn select_returns_none_when_nothing_installed() {
        assert!(select_with(&learn(None, None), |_| false).is_none());
    }

    #[test]
    fn select_pin_wins_over_probe_order() {
        // codex is pinned even though claude is also available.
        let choice = select_with(&learn(Some("codex"), None), |_| true).unwrap();
        assert_eq!(choice.cli_id, "codex");
    }

    #[test]
    fn select_pin_does_not_fall_back_when_pinned_cli_missing() {
        // Pinned gemini is not installed; claude/codex are — but a pin means
        // "this CLI or nothing", so there is no fallback.
        let choice = select_with(&learn(Some("gemini"), None), |p| p != "gemini");
        assert!(choice.is_none());
    }

    #[test]
    fn select_unknown_pin_yields_none() {
        assert!(select_with(&learn(Some("nope"), None), |_| true).is_none());
    }

    #[test]
    fn select_model_pin_overrides_per_cli_default() {
        let choice = select_with(&learn(None, Some("opus")), |_| true).unwrap();
        assert_eq!(choice.cli_id, "claude");
        assert_eq!(choice.model, "opus");
    }

    // --- invoke: per-CLI flags, env guard, stdin, unwrap (stub-driven) -------

    #[cfg(unix)]
    fn write_stub(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn dumped_lines(work: &Path, file: &str) -> Vec<String> {
        std::fs::read_to_string(work.join(file))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A stub script prologue that dumps argv, the recursion-guard env var, and
    /// stdin into files in its cwd (the invoke `work_dir`), for assertions.
    #[cfg(unix)]
    const DUMP_PROLOGUE: &str = "#!/bin/sh\n\
        : > argv.txt\n\
        for a in \"$@\"; do printf '%s\\n' \"$a\" >> argv.txt; done\n\
        printf '%s' \"${LOADOUT_LEARN_WORKER}\" > env.txt\n\
        cat > stdin.txt\n";

    #[cfg(unix)]
    fn choice(cli_id: &'static str, program: &Path, model: &str) -> CliChoice {
        CliChoice {
            cli_id,
            program: program.to_string_lossy().into_owned(),
            model: model.to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn invoke_claude_flags_env_stdin_and_result_unwrap() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"type\":\"result\",\"is_error\":false,\"result\":\"CLAUDE-OUTPUT\",\"usage\":{{\"input_tokens\":5}}}}\n\
                 JSON\n"
            ),
        );
        let out = invoke(
            &choice("claude", &stub, "haiku"),
            "PROMPT-BODY-CLAUDE",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(out.text, "CLAUDE-OUTPUT");
        assert_eq!(out.usage.as_deref(), Some(r#"{"input_tokens":5}"#));

        let argv = dumped_lines(work.path(), "argv.txt");
        for expected in [
            "-p",
            "--safe-mode",
            "--no-session-persistence",
            "--output-format",
            "json",
            "--tools",
            "--model",
            "haiku",
        ] {
            assert!(
                argv.contains(&expected.to_string()),
                "argv missing {expected:?}: {argv:?}"
            );
        }
        // `--tools` is followed by an empty argument (disable all tools).
        let tools_idx = argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(
            argv[tools_idx + 1],
            "",
            "--tools must be followed by an empty arg"
        );
        // Recursion guard set, prompt delivered on stdin.
        assert_eq!(
            std::fs::read_to_string(work.path().join("env.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(work.path().join("stdin.txt")).unwrap(),
            "PROMPT-BODY-CLAUDE"
        );
    }

    #[cfg(unix)]
    #[test]
    fn invoke_claude_is_error_true_is_a_failure() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"is_error\":true,\"result\":\"Not logged in\"}}\n\
                 JSON\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_codex_flags_schema_file_and_message_file_unwrap() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // Stub: dump, then write the canned final message to the `-o` path, and
        // emit a JSONL stream carrying the final message + a turn.completed
        // usage blob — mirroring real `codex exec --json`.
        let stub = write_stub(
            bin.path(),
            "codex",
            &format!(
                "{DUMP_PROLOGUE}\
                 out=\"\"; prev=\"\"\n\
                 for a in \"$@\"; do if [ \"$prev\" = \"-o\" ]; then out=\"$a\"; fi; prev=\"$a\"; done\n\
                 printf '%s' '{{\"candidates\":[]}}' > \"$out\"\n\
                 printf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"t1\"}}'\n\
                 printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"x\"}}}}'\n\
                 printf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":11}}}}'\n"
            ),
        );
        let out = invoke(
            &choice("codex", &stub, ""),
            "PROMPT-BODY-CODEX",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(out.text, r#"{"candidates":[]}"#);
        assert_eq!(out.usage.as_deref(), Some(r#"{"input_tokens":11}"#));

        // The schema file invoke wrote is the strict extraction schema.
        let schema = std::fs::read_to_string(work.path().join("schema.json")).unwrap();
        assert!(
            schema.contains("candidates"),
            "schema not written: {schema}"
        );

        let argv = dumped_lines(work.path(), "argv.txt");
        for expected in [
            "exec",
            "-s",
            "read-only",
            "--ephemeral",
            "--skip-git-repo-check",
            "--output-schema",
            "-o",
            "--json",
        ] {
            assert!(
                argv.contains(&expected.to_string()),
                "argv missing {expected:?}: {argv:?}"
            );
        }
        // Empty model ⇒ no `-m` flag.
        assert!(
            !argv.contains(&"-m".to_string()),
            "codex default must omit -m: {argv:?}"
        );
        assert_eq!(
            std::fs::read_to_string(work.path().join("env.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(work.path().join("stdin.txt")).unwrap(),
            "PROMPT-BODY-CODEX"
        );
    }

    #[cfg(unix)]
    #[test]
    fn invoke_codex_passes_model_when_set() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "codex",
            &format!(
                "{DUMP_PROLOGUE}\
                 out=\"\"; prev=\"\"\n\
                 for a in \"$@\"; do if [ \"$prev\" = \"-o\" ]; then out=\"$a\"; fi; prev=\"$a\"; done\n\
                 printf '%s' 'ok' > \"$out\"\n"
            ),
        );
        invoke(
            &choice("codex", &stub, "gpt-5.5"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap();
        let argv = dumped_lines(work.path(), "argv.txt");
        let idx = argv
            .iter()
            .position(|a| a == "-m")
            .expect("-m present when model set");
        assert_eq!(argv[idx + 1], "gpt-5.5");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_codex_empty_message_file_is_a_failure() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // Stub writes NOTHING to the -o file (simulates a failed turn).
        let stub = write_stub(bin.path(), "codex", &format!("{DUMP_PROLOGUE}printf ''\n"));
        let err = invoke(
            &choice("codex", &stub, ""),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no final message"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_gemini_flags_env_stdin_and_response_unwrap() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "gemini",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"session_id\":\"s1\",\"response\":\"GEMINI-OUTPUT\",\"stats\":{{\"tokens\":9}}}}\n\
                 JSON\n"
            ),
        );
        let out = invoke(
            &choice("gemini", &stub, "gemini-2.5-flash"),
            "PROMPT-BODY-GEMINI",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(out.text, "GEMINI-OUTPUT");
        assert_eq!(out.usage.as_deref(), Some(r#"{"tokens":9}"#));

        let argv = dumped_lines(work.path(), "argv.txt");
        for expected in [
            "-p",
            "--approval-mode",
            "plan",
            "-o",
            "json",
            "--skip-trust",
            "-m",
            "gemini-2.5-flash",
        ] {
            assert!(
                argv.contains(&expected.to_string()),
                "argv missing {expected:?}: {argv:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(work.path().join("env.txt")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(work.path().join("stdin.txt")).unwrap(),
            "PROMPT-BODY-GEMINI"
        );
    }

    #[cfg(unix)]
    #[test]
    fn invoke_gemini_missing_response_is_a_failure() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "gemini",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"error\":{{\"message\":\"boom\"}}}}\n\
                 JSON\n"
            ),
        );
        let err = invoke(
            &choice("gemini", &stub, "gemini-2.5-flash"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("response"), "{err}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn invoke_kills_a_wedged_cli_at_the_deadline() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // Sleeps far past the deadline; must be killed and reported as a failure.
        let stub = write_stub(bin.path(), "claude", "#!/bin/sh\nsleep 30\n");
        let start = std::time::Instant::now();
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "deadline was not enforced"
        );
        assert!(err.to_string().contains("deadline"), "{err}");
    }
}
