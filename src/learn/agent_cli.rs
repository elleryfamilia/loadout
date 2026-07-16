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
//! installed binaries — claude 2.1.211, codex-cli 0.142.0, gemini 0.45.2)
//!
//! - **claude** `-p --safe-mode --no-session-persistence --output-format json
//!   --json-schema <schema> --tools "" [--model <model>]`. `--safe-mode`
//!   disables hooks, plugins, MCP
//!   servers, CLAUDE.md discovery, skills, and custom commands/agents while
//!   keeping auth working normally — unlike `--bare`, which additionally blocks
//!   OAuth/keychain reads and so returns "Not logged in" for a
//!   subscription-authenticated user (verified empirically). `--tools ""` is
//!   documented and verified to disable ALL built-in tools. Output: stdout is a
//!   single JSON object; strict extraction data is the object-valued
//!   `.structured_output`, usage blob is `.usage`, and `.is_error == true`
//!   is treated as a failed call. Free-form `.result` is never accepted as
//!   extraction output.
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

/// Old Claude releases did not provide the structured-output contract harvest
/// relies on. This is the exact version used for the approved live check.
pub const CLAUDE_MIN_VERSION: &str = "2.1.211";

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

/// Why an installed Claude executable cannot be used for harvest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
    TooOld,
    UnrecognizedVersion,
    ProbeTimedOut,
}

/// A known extraction CLI that cannot satisfy harvest's output contract.
/// Provider-controlled version text is never retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCli {
    pub cli_id: &'static str,
    /// Canonical numeric form only. Unknown raw output is discarded.
    pub installed_version: Option<String>,
    pub minimum_version: &'static str,
    pub reason: UnsupportedReason,
}

/// Result of extraction-CLI selection.
#[derive(Debug, Clone)]
pub enum Selection {
    Chosen(CliChoice),
    Unsupported(UnsupportedCli),
    None,
}

impl Selection {
    /// Compatibility for the current worker. A later diagnostics checkpoint
    /// replaces this with explicit unsupported-version handling.
    pub(crate) fn map<T>(self, f: impl FnOnce(CliChoice) -> T) -> Option<T> {
        match self {
            Self::Chosen(choice) => Some(f(choice)),
            Self::Unsupported(_) | Self::None => None,
        }
    }
}

/// Numeric CLI version with missing components normalized to zero.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NumericVersion {
    components: [u64; 3],
    prerelease: bool,
}

impl NumericVersion {
    fn canonical(&self) -> String {
        let [major, minor, patch] = self.components;
        if self.prerelease {
            format!("{major}.{minor}.{patch}-prerelease")
        } else {
            format!("{major}.{minor}.{patch}")
        }
    }
}

impl PartialOrd for NumericVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NumericVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.components
            .cmp(&other.components)
            .then_with(|| (!self.prerelease).cmp(&(!other.prerelease)))
    }
}

fn numeric_version(raw: &str) -> Option<NumericVersion> {
    let token = providers::parse_version(raw);
    let (core, prerelease) = match token.split_once('-') {
        Some((core, suffix)) if recognized_prerelease(suffix) => (core, true),
        Some(_) => return None,
        None => (token.as_str(), false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some(NumericVersion {
        components: [major, minor, patch],
        prerelease,
    })
}

fn recognized_prerelease(suffix: &str) -> bool {
    let label = suffix.split(['.', '-']).next().unwrap_or_default();
    matches!(label, "alpha" | "beta" | "rc" | "pre" | "dev")
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
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
pub fn select(learn: &LearnConfig) -> Selection {
    select_with(learn, providers::probe_cli)
}

/// Selection with an injectable bounded probe for deterministic tests.
fn select_with(learn: &LearnConfig, probe: impl Fn(&str) -> providers::CliProbe) -> Selection {
    // A `learn.cli` pin restricts the candidates to that one known id; an
    // unknown pin yields an empty candidate list (→ None). No pin ⇒ full order.
    let candidates: Vec<&'static str> = match learn.cli.as_deref() {
        Some(pin) => PROBE_ORDER.iter().copied().filter(|c| *c == pin).collect(),
        None => PROBE_ORDER.to_vec(),
    };
    let mut unsupported = None;
    for cli_id in candidates {
        match probe(cli_id) {
            providers::CliProbe::Found(raw) => {
                if cli_id == "claude" {
                    let Some(found) = numeric_version(&raw) else {
                        unsupported = Some(UnsupportedCli {
                            cli_id,
                            installed_version: None,
                            minimum_version: CLAUDE_MIN_VERSION,
                            reason: UnsupportedReason::UnrecognizedVersion,
                        });
                        continue;
                    };
                    let minimum =
                        numeric_version(CLAUDE_MIN_VERSION).expect("valid Claude minimum version");
                    if found < minimum {
                        unsupported = Some(UnsupportedCli {
                            cli_id,
                            installed_version: Some(found.canonical()),
                            minimum_version: CLAUDE_MIN_VERSION,
                            reason: UnsupportedReason::TooOld,
                        });
                        continue;
                    }
                }
                let model = learn
                    .model
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| default_model(cli_id).to_string());
                return Selection::Chosen(CliChoice {
                    cli_id,
                    program: cli_id.to_string(),
                    model,
                });
            }
            providers::CliProbe::TimedOut if cli_id == "claude" => {
                unsupported = Some(UnsupportedCli {
                    cli_id,
                    installed_version: None,
                    minimum_version: CLAUDE_MIN_VERSION,
                    reason: UnsupportedReason::ProbeTimedOut,
                });
            }
            providers::CliProbe::Missing | providers::CliProbe::TimedOut => {}
        }
    }
    unsupported.map_or(Selection::None, Selection::Unsupported)
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
    let schema = serde_json::to_string(&crate::learn::extract::output_json_schema())
        .context("serializing Claude output schema")?;
    let mut cmd = base_command(&choice.program, work_dir);
    cmd.arg("-p")
        .arg("--safe-mode")
        .arg("--no-session-persistence")
        .arg("--output-format")
        .arg("json")
        .arg("--json-schema")
        .arg(schema)
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
    let structured = value
        .get("structured_output")
        .filter(|output| output.is_object())
        .ok_or_else(|| {
            anyhow!("claude JSON is missing an object-valued `structured_output` field")
        })?;
    let text = serde_json::to_string(structured).context("serializing Claude structured output")?;
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
        let selection = select_with(&learn(None, None), |_| {
            providers::CliProbe::Found("claude 2.1.211".to_string())
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected supported Claude to be selected");
        };
        assert_eq!(choice.cli_id, "claude");
        assert_eq!(choice.program, "claude");
        assert_eq!(choice.model, "haiku");
    }

    #[test]
    fn select_skips_missing_and_falls_to_codex() {
        let selection = select_with(&learn(None, None), |program| match program {
            "claude" => providers::CliProbe::Missing,
            "codex" => providers::CliProbe::Found("codex-cli 0.144.4".to_string()),
            _ => providers::CliProbe::Missing,
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected Codex fallback");
        };
        assert_eq!(choice.cli_id, "codex");
        // codex uses its configured default: empty model, `-m` omitted.
        assert_eq!(choice.model, "");
    }

    #[test]
    fn select_falls_through_to_gemini() {
        let selection = select_with(&learn(None, None), |program| match program {
            "gemini" => providers::CliProbe::Found("0.45.2".to_string()),
            _ => providers::CliProbe::Missing,
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected Gemini fallback");
        };
        assert_eq!(choice.cli_id, "gemini");
        assert_eq!(choice.model, "gemini-2.5-flash");
    }

    #[test]
    fn select_returns_none_when_nothing_installed() {
        assert!(matches!(
            select_with(&learn(None, None), |_| providers::CliProbe::Missing),
            Selection::None
        ));
    }

    #[test]
    fn select_pin_wins_over_probe_order() {
        let selection = select_with(&learn(Some("codex"), None), |_| {
            providers::CliProbe::Found("codex-cli 0.144.4".to_string())
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected pinned Codex");
        };
        assert_eq!(choice.cli_id, "codex");
    }

    #[test]
    fn select_pin_does_not_fall_back_when_pinned_cli_missing() {
        // Pinned gemini is not installed; claude/codex are — but a pin means
        // "this CLI or nothing", so there is no fallback.
        let selection = select_with(&learn(Some("gemini"), None), |_| {
            providers::CliProbe::Missing
        });
        assert!(matches!(selection, Selection::None));
    }

    #[test]
    fn select_unknown_pin_yields_none() {
        assert!(matches!(
            select_with(&learn(Some("nope"), None), |_| {
                providers::CliProbe::Found("9.9.9".to_string())
            }),
            Selection::None
        ));
    }

    #[test]
    fn select_model_pin_overrides_per_cli_default() {
        let selection = select_with(&learn(None, Some("opus")), |_| {
            providers::CliProbe::Found("claude 2.1.211".to_string())
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected supported Claude");
        };
        assert_eq!(choice.cli_id, "claude");
        assert_eq!(choice.model, "opus");
    }

    #[test]
    fn numeric_versions_compare_components_not_lexically() {
        assert!(numeric_version("claude 2.1.99").unwrap() < numeric_version("2.1.211").unwrap());
        assert_eq!(
            numeric_version("2.1").unwrap(),
            numeric_version("2.1.0").unwrap()
        );
    }

    #[test]
    fn prerelease_is_below_the_matching_release() {
        assert!(
            numeric_version("claude 2.1.211-beta.1").unwrap() < numeric_version("2.1.211").unwrap()
        );
    }

    #[test]
    fn unknown_version_suffix_fails_closed() {
        assert!(numeric_version("claude 2.1.211-custom").is_none());
        assert!(numeric_version("claude 2.1.211+local").is_none());
    }

    #[test]
    fn pinned_old_claude_is_reported_unsupported() {
        let selection = select_with(&learn(Some("claude"), None), |_| {
            providers::CliProbe::Found("claude 2.1.210".to_string())
        });
        let Selection::Unsupported(unsupported) = selection else {
            panic!("expected unsupported Claude");
        };
        assert_eq!(unsupported.cli_id, "claude");
        assert_eq!(unsupported.installed_version.as_deref(), Some("2.1.210"));
        assert_eq!(unsupported.minimum_version, CLAUDE_MIN_VERSION);
        assert_eq!(unsupported.reason, UnsupportedReason::TooOld);
    }

    #[test]
    fn unpinned_old_claude_falls_back_to_codex() {
        let selection = select_with(&learn(None, None), |program| match program {
            "claude" => providers::CliProbe::Found("claude 2.1.210".to_string()),
            "codex" => providers::CliProbe::Found("codex-cli 0.144.4".to_string()),
            _ => providers::CliProbe::Missing,
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected Codex fallback");
        };
        assert_eq!(choice.cli_id, "codex");
    }

    #[test]
    fn unknown_claude_version_falls_back_but_is_reported_when_alone() {
        let selection = select_with(&learn(None, None), |program| match program {
            "claude" => providers::CliProbe::Found("claude 2.1.211-custom".to_string()),
            _ => providers::CliProbe::Missing,
        });
        let Selection::Unsupported(unsupported) = selection else {
            panic!("expected unsupported Claude");
        };
        assert_eq!(unsupported.installed_version, None);
        assert_eq!(unsupported.reason, UnsupportedReason::UnrecognizedVersion);
    }

    #[test]
    fn timed_out_claude_probe_falls_back_but_is_reported_when_alone() {
        let selection = select_with(&learn(None, None), |program| match program {
            "claude" => providers::CliProbe::TimedOut,
            _ => providers::CliProbe::Missing,
        });
        let Selection::Unsupported(unsupported) = selection else {
            panic!("expected timed-out Claude to be reported");
        };
        assert_eq!(unsupported.reason, UnsupportedReason::ProbeTimedOut);
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
    fn invoke_claude_flags_env_stdin_and_structured_output_unwrap() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"type\":\"result\",\"is_error\":false,\"result\":\"MISLEADING PROSE\",\"structured_output\":{{\"candidates\":[]}},\"usage\":{{\"input_tokens\":5}}}}\n\
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

        assert_eq!(out.text, r#"{"candidates":[]}"#);
        assert_eq!(out.usage.as_deref(), Some(r#"{"input_tokens":5}"#));

        let argv = dumped_lines(work.path(), "argv.txt");
        for expected in [
            "-p",
            "--safe-mode",
            "--no-session-persistence",
            "--output-format",
            "json",
            "--json-schema",
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
        let schema_idx = argv.iter().position(|a| a == "--json-schema").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&argv[schema_idx + 1]).unwrap(),
            crate::learn::extract::output_json_schema(),
            "Claude must receive the exact extraction schema"
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
    fn invoke_claude_never_falls_back_to_result() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"is_error\":false,\"result\":\"{{\\\"candidates\\\":[]}}\"}}\n\
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
        assert!(err.to_string().contains("structured_output"), "{err}");
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
