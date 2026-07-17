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
//! (`-m` omitted). A set `learn.model` overrides the selected CLI's default.
//! Without a `learn.cli` pin, an explicit model restricts selection to the
//! preferred CLI so the model id is never forwarded to another provider.
//!
//! Everything the CLI returns is untrusted data — a transcript the model
//! summarized can contain injection attempts. This layer only extracts the text
//! and usage; validation of the output shape is [`crate::learn::extract`]'s job
//! and claim vetting is [`crate::learn::gate`]'s.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

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

/// A provider identifier safe to retain in harvest diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderId {
    Claude,
    Codex,
    Gemini,
    Unknown,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Unknown => "unknown",
        }
    }
}

/// Closed failure classifications returned by provider adapters. None of the
/// variants accepts provider-controlled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeFailureKind {
    SpawnFailed,
    TimedOut,
    ProcessFailed,
    EnvelopeInvalid,
    Authentication,
    RateLimited,
    StructuredRetriesExhausted,
    ProviderReported,
    OutputMissing,
}

/// Privacy-safe failure from one extraction-CLI invocation.
///
/// Provider stdout, stderr, error bodies, paths, prompts, and model output are
/// deliberately absent. The optional usage value is copied only from the
/// provider envelope's dedicated usage/stats field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeFailure {
    pub provider: ProviderId,
    pub kind: InvokeFailureKind,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub io_kind: Option<std::io::ErrorKind>,
    pub os_error: Option<i32>,
    pub usage: Option<String>,
}

impl InvokeFailure {
    fn new(provider: ProviderId, kind: InvokeFailureKind) -> Self {
        Self {
            provider,
            kind,
            exit_code: None,
            signal: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            io_kind: None,
            os_error: None,
            usage: None,
        }
    }

    fn from_output(
        provider: ProviderId,
        kind: InvokeFailureKind,
        output: &std::process::Output,
        usage: Option<String>,
    ) -> Self {
        let (exit_code, signal) = exit_parts(&output.status);
        Self {
            provider,
            kind,
            exit_code,
            signal,
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
            io_kind: None,
            os_error: None,
            usage,
        }
    }

    pub fn stage(&self) -> &'static str {
        match self.kind {
            InvokeFailureKind::SpawnFailed
            | InvokeFailureKind::TimedOut
            | InvokeFailureKind::ProcessFailed => "invoke",
            InvokeFailureKind::EnvelopeInvalid
            | InvokeFailureKind::Authentication
            | InvokeFailureKind::RateLimited
            | InvokeFailureKind::StructuredRetriesExhausted
            | InvokeFailureKind::ProviderReported
            | InvokeFailureKind::OutputMissing => "cli_output",
        }
    }

    pub fn code(&self) -> &'static str {
        match self.kind {
            InvokeFailureKind::SpawnFailed => "cli_spawn_failed",
            InvokeFailureKind::TimedOut => "cli_timed_out",
            InvokeFailureKind::ProcessFailed => "cli_process_failed",
            InvokeFailureKind::EnvelopeInvalid => "cli_envelope_invalid",
            InvokeFailureKind::Authentication => "cli_auth_failed",
            InvokeFailureKind::RateLimited => "cli_rate_limited",
            InvokeFailureKind::StructuredRetriesExhausted => "cli_structured_retries_exhausted",
            InvokeFailureKind::ProviderReported => "cli_reported_error",
            InvokeFailureKind::OutputMissing => "provider_output_missing",
        }
    }

    pub fn message(&self) -> &'static str {
        match self.kind {
            InvokeFailureKind::SpawnFailed => "Loadout could not start the extraction CLI.",
            InvokeFailureKind::TimedOut => "The extraction CLI exceeded its deadline.",
            InvokeFailureKind::ProcessFailed => "The extraction CLI exited unsuccessfully.",
            InvokeFailureKind::EnvelopeInvalid => {
                "The extraction CLI returned an invalid response envelope."
            }
            InvokeFailureKind::Authentication => match self.provider {
                ProviderId::Claude => {
                    "Claude could not authenticate. Open Claude and sign in, then run `load harvest` again."
                }
                ProviderId::Codex => {
                    "Codex could not authenticate. Open Codex and sign in, then run `load harvest` again."
                }
                ProviderId::Gemini => {
                    "Gemini could not authenticate. Open Gemini and sign in, then run `load harvest` again."
                }
                ProviderId::Unknown => {
                    "The extraction CLI could not authenticate. Sign in, then run `load harvest` again."
                }
            },
            InvokeFailureKind::RateLimited => {
                "The extraction provider rate-limited this request. Wait, then run `load harvest` again."
            }
            InvokeFailureKind::StructuredRetriesExhausted => {
                "The extraction CLI could not produce valid structured output. Upgrade the CLI if this repeats."
            }
            InvokeFailureKind::ProviderReported => {
                "The extraction provider reported an error. Run `load harvest` again."
            }
            InvokeFailureKind::OutputMissing => {
                "The extraction CLI did not return the required structured output."
            }
        }
    }
}

impl std::fmt::Display for InvokeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for InvokeFailure {}

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
    let has_explicit_model = learn
        .model
        .as_deref()
        .is_some_and(|model| !model.is_empty());
    // A `learn.cli` pin restricts the candidates to that one known id; an
    // unknown pin yields an empty candidate list (→ None). An unpinned model
    // restricts selection to the preferred CLI so it cannot cross providers.
    let candidates: Vec<&'static str> = match learn.cli.as_deref() {
        Some(pin) => PROBE_ORDER.iter().copied().filter(|c| *c == pin).collect(),
        None if has_explicit_model => PROBE_ORDER.first().copied().into_iter().collect(),
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
) -> Result<InvokeOut, InvokeFailure> {
    match choice.cli_id {
        "claude" => invoke_claude(choice, prompt, work_dir, deadline),
        "codex" => invoke_codex(choice, prompt, work_dir, deadline),
        "gemini" => invoke_gemini(choice, prompt, work_dir, deadline),
        _ => Err(InvokeFailure::new(
            ProviderId::Unknown,
            InvokeFailureKind::ProviderReported,
        )),
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
    provider: ProviderId,
    cmd: &mut Command,
    prompt: &str,
    deadline: Duration,
) -> Result<std::process::Output, InvokeFailure> {
    match providers::output_with_timeout_stdin(cmd, deadline, prompt.as_bytes()) {
        Ok(Some(out)) => Ok(out),
        Ok(None) => Err(InvokeFailure::new(provider, InvokeFailureKind::TimedOut)),
        Err(error) => {
            let mut failure = InvokeFailure::new(provider, InvokeFailureKind::SpawnFailed);
            failure.io_kind = Some(error.kind());
            failure.os_error = error.raw_os_error();
            Err(failure)
        }
    }
}

fn exit_parts(status: &std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

fn classify_provider_error(text: &str) -> InvokeFailureKind {
    let normalized = text.to_ascii_lowercase();
    if [
        "not logged in",
        "authentication failed",
        "unauthorized",
        "invalid api key",
        "login required",
        "please log in",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        InvokeFailureKind::Authentication
    } else if [
        "rate limit",
        "rate_limit",
        "too many requests",
        "resource exhausted",
        "resource_exhausted",
        "quota exceeded",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        InvokeFailureKind::RateLimited
    } else if (normalized.contains("structured output") || normalized.contains("structured_output"))
        && ["retry", "retries", "attempt", "attempts"]
            .iter()
            .any(|marker| normalized.contains(marker))
    {
        InvokeFailureKind::StructuredRetriesExhausted
    } else {
        InvokeFailureKind::ProviderReported
    }
}

fn process_failure(
    provider: ProviderId,
    output: &std::process::Output,
    usage: Option<String>,
) -> Option<InvokeFailure> {
    (!output.status.success()).then(|| {
        InvokeFailure::from_output(provider, InvokeFailureKind::ProcessFailed, output, usage)
    })
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
) -> Result<InvokeOut, InvokeFailure> {
    let provider = ProviderId::Claude;
    let schema = serde_json::to_string(&crate::learn::extract::output_json_schema())
        .expect("the extraction schema is JSON-serializable");
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

    let out = run_capture(provider, &mut cmd, prompt, deadline)?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| {
        InvokeFailure::from_output(provider, InvokeFailureKind::EnvelopeInvalid, &out, None)
    })?;
    let usage = value.get("usage").map(|u| u.to_string());
    if value.get("is_error").and_then(|b| b.as_bool()) == Some(true) {
        let body = value.get("result").and_then(|r| r.as_str()).unwrap_or("");
        return Err(InvokeFailure::from_output(
            provider,
            classify_provider_error(body),
            &out,
            usage,
        ));
    }
    let structured = value
        .get("structured_output")
        .filter(|output| output.is_object())
        .ok_or_else(|| {
            InvokeFailure::from_output(
                provider,
                InvokeFailureKind::OutputMissing,
                &out,
                usage.clone(),
            )
        })?;
    let text = serde_json::to_string(structured)
        .expect("a parsed JSON value can always be serialized back to JSON");
    if let Some(failure) = process_failure(provider, &out, usage.clone()) {
        return Err(failure);
    }
    Ok(InvokeOut { text, usage })
}

fn invoke_codex(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut, InvokeFailure> {
    let provider = ProviderId::Codex;
    // Write the strict output schema codex should steer its structured output
    // toward, then clear any stale final-message file so an old one can't be
    // mistaken for this run's success.
    let schema_path = work_dir.join("schema.json");
    let schema = crate::learn::extract::output_json_schema();
    std::fs::write(
        &schema_path,
        serde_json::to_vec(&schema).expect("the extraction schema is JSON-serializable"),
    )
    .map_err(|error| {
        let mut failure = InvokeFailure::new(provider, InvokeFailureKind::SpawnFailed);
        failure.io_kind = Some(error.kind());
        failure.os_error = error.raw_os_error();
        failure
    })?;
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

    let out = run_capture(provider, &mut cmd, prompt, deadline)?;
    let envelope = codex_envelope(&out)
        .map_err(|kind| InvokeFailure::from_output(provider, kind, &out, None))?;
    if let Some(kind) = envelope.error_kind {
        return Err(InvokeFailure::from_output(
            provider,
            kind,
            &out,
            envelope.usage,
        ));
    }
    let text = std::fs::read_to_string(&last_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            InvokeFailure::from_output(
                provider,
                InvokeFailureKind::OutputMissing,
                &out,
                envelope.usage.clone(),
            )
        })?;
    let usage = envelope.usage;
    if let Some(failure) = process_failure(provider, &out, usage.clone()) {
        return Err(failure);
    }
    Ok(InvokeOut { text, usage })
}

/// The last `usage` blob in codex's parsed `--json` event stream, plus any
/// provider-reported failure classification.
struct CodexEnvelope {
    usage: Option<String>,
    error_kind: Option<InvokeFailureKind>,
}

fn codex_envelope(output: &std::process::Output) -> Result<CodexEnvelope, InvokeFailureKind> {
    let mut usage = None;
    let mut error_kind = None;
    for line in String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|_| InvokeFailureKind::EnvelopeInvalid)?;
        let event_type = value.get("type").and_then(|kind| kind.as_str());
        if value.get("usage").is_some() {
            usage = value.get("usage").map(|value| value.to_string());
        }
        if matches!(event_type, Some("turn.failed" | "error")) && error_kind.is_none() {
            let body = value
                .get("error")
                .or_else(|| value.get("message"))
                .map(serde_json::Value::to_string)
                .unwrap_or_default();
            error_kind = Some(classify_provider_error(&body));
        }
    }
    Ok(CodexEnvelope { usage, error_kind })
}

fn invoke_gemini(
    choice: &CliChoice,
    prompt: &str,
    work_dir: &Path,
    deadline: Duration,
) -> Result<InvokeOut, InvokeFailure> {
    let provider = ProviderId::Gemini;
    let mut cmd = base_command(&choice.program, work_dir);
    cmd.arg("-p")
        .arg("") // headless; prompt comes from stdin
        .arg("--approval-mode")
        .arg("plan")
        .arg("-o")
        .arg("json")
        .arg("--skip-trust");
    push_model(&mut cmd, "-m", &choice.model);

    let out = run_capture(provider, &mut cmd, prompt, deadline)?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| {
        InvokeFailure::from_output(provider, InvokeFailureKind::EnvelopeInvalid, &out, None)
    })?;
    let usage = value.get("stats").map(|s| s.to_string());
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let body = error.to_string();
        return Err(InvokeFailure::from_output(
            provider,
            classify_provider_error(&body),
            &out,
            usage,
        ));
    }
    let text = value
        .get("response")
        .and_then(|r| r.as_str())
        .ok_or_else(|| {
            InvokeFailure::from_output(
                provider,
                InvokeFailureKind::OutputMissing,
                &out,
                usage.clone(),
            )
        })?
        .to_string();
    if let Some(failure) = process_failure(provider, &out, usage.clone()) {
        return Err(failure);
    }
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
    fn select_pin_keeps_its_explicit_model() {
        let selection = select_with(&learn(Some("codex"), Some("o3")), |program| {
            assert_eq!(program, "codex");
            providers::CliProbe::Found("codex-cli 0.144.4".to_string())
        });
        let Selection::Chosen(choice) = selection else {
            panic!("expected pinned Codex");
        };
        assert_eq!(choice.cli_id, "codex");
        assert_eq!(choice.model, "o3");
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
    fn unpinned_old_claude_with_explicit_model_does_not_probe_fallback_clis() {
        let probed = std::cell::RefCell::new(Vec::new());
        let selection = select_with(&learn(None, Some("haiku")), |program| {
            probed.borrow_mut().push(program.to_string());
            match program {
                "claude" => providers::CliProbe::Found("claude 2.1.210".to_string()),
                "codex" => providers::CliProbe::Found("codex-cli 0.144.4".to_string()),
                "gemini" => providers::CliProbe::Found("0.45.2".to_string()),
                _ => providers::CliProbe::Missing,
            }
        });

        let Selection::Unsupported(unsupported) = selection else {
            panic!("expected unsupported Claude");
        };
        assert_eq!(unsupported.cli_id, "claude");
        assert_eq!(unsupported.reason, UnsupportedReason::TooOld);
        assert_eq!(&*probed.borrow(), &["claude"]);
    }

    #[test]
    fn unpinned_missing_claude_with_explicit_model_does_not_probe_fallback_clis() {
        let probed = std::cell::RefCell::new(Vec::new());
        let selection = select_with(&learn(None, Some("haiku")), |program| {
            probed.borrow_mut().push(program.to_string());
            match program {
                "claude" => providers::CliProbe::Missing,
                "codex" => providers::CliProbe::Found("codex-cli 0.144.4".to_string()),
                "gemini" => providers::CliProbe::Found("0.45.2".to_string()),
                _ => providers::CliProbe::Missing,
            }
        });

        assert!(matches!(selection, Selection::None));
        assert_eq!(&*probed.borrow(), &["claude"]);
    }

    #[test]
    fn unpinned_old_claude_with_empty_model_uses_fallback_default() {
        let selection = select_with(&learn(None, Some("")), |program| match program {
            "claude" => providers::CliProbe::Found("claude 2.1.210".to_string()),
            "codex" => providers::CliProbe::Found("codex-cli 0.144.4".to_string()),
            _ => providers::CliProbe::Missing,
        });

        let Selection::Chosen(choice) = selection else {
            panic!("expected Codex fallback");
        };
        assert_eq!(choice.cli_id, "codex");
        assert_eq!(choice.model, "");
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
    fn assert_safe_failure(failure: &InvokeFailure, kind: InvokeFailureKind, forbidden: &[&str]) {
        assert_eq!(failure.kind, kind);
        let retained = format!("{failure:?}\n{failure}");
        for sentinel in forbidden {
            assert!(
                !retained.contains(sentinel),
                "provider-controlled sentinel leaked into failure: {retained}"
            );
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
                 printf '%s' 'SECRET_CLAUDE_STDERR' >&2\n\
                 cat <<'JSON'\n\
                 {{\"is_error\":true,\"result\":\"Not logged in SECRET_CLAUDE_BODY\",\"usage\":{{\"input_tokens\":7}}}}\n\
                 JSON\n\
                 exit 19\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::Authentication,
            &["SECRET_CLAUDE_BODY", "SECRET_CLAUDE_STDERR"],
        );
        assert_eq!(err.stage(), "cli_output");
        assert_eq!(err.code(), "cli_auth_failed");
        assert_eq!(err.exit_code, Some(19));
        assert_eq!(err.usage.as_deref(), Some(r#"{"input_tokens":7}"#));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_claude_invalid_envelope_precedes_process_status() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s' 'SECRET_INVALID_STDOUT'\n\
                 printf '%s' 'SECRET_INVALID_STDERR' >&2\n\
                 exit 23\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::EnvelopeInvalid,
            &["SECRET_INVALID_STDOUT", "SECRET_INVALID_STDERR"],
        );
        assert_eq!(err.exit_code, Some(23));
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
                 {{\"is_error\":false,\"result\":\"SECRET_RESULT_BODY\"}}\n\
                 JSON\n\
                 printf '%s' 'SECRET_MISSING_STDERR' >&2\n\
                 exit 29\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::OutputMissing,
            &["SECRET_RESULT_BODY", "SECRET_MISSING_STDERR"],
        );
        assert_eq!(err.exit_code, Some(29));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_claude_process_status_precedes_payload_validation() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s' '{{\"is_error\":false,\"structured_output\":{{\"not_candidates\":\"SECRET_PAYLOAD\"}}}}'\n\
                 printf '%s' 'SECRET_PROCESS_STDERR' >&2\n\
                 exit 31\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::ProcessFailed,
            &["SECRET_PAYLOAD", "SECRET_PROCESS_STDERR"],
        );
        assert_eq!(err.exit_code, Some(31));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_claude_classifies_structured_retry_exhaustion_without_body() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s' '{{\"is_error\":true,\"result\":\"Failed to provide valid structured output after retries SECRET_RETRY_BODY\"}}'\n"
            ),
        );
        let err = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::StructuredRetriesExhausted,
            &["SECRET_RETRY_BODY"],
        );
    }

    #[test]
    fn known_rate_limit_marker_maps_to_safe_classification() {
        assert_eq!(
            classify_provider_error("Too many requests: SECRET_RATE_BODY"),
            InvokeFailureKind::RateLimited
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_invoke_defers_strict_payload_validation_to_extract() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "claude",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s' '{{\"is_error\":false,\"structured_output\":{{\"not_candidates\":\"SECRET_SCHEMA_VALUE\"}}}}'\n"
            ),
        );
        let out = invoke(
            &choice("claude", &stub, "haiku"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap();
        let failure = crate::learn::extract::parse_output(&out.text).unwrap_err();
        assert_eq!(
            failure.kind(),
            crate::learn::extract::ParseFailureKind::SchemaMismatch
        );
        let retained = format!("{failure:?}\n{failure}");
        assert!(!retained.contains("SECRET_SCHEMA_VALUE"), "{retained}");
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
        let stub = write_stub(
            bin.path(),
            "codex",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":13}},\"detail\":\"SECRET_CODEX_STDOUT\"}}'\n\
                 printf '%s' 'SECRET_CODEX_STDERR' >&2\n"
            ),
        );
        let err = invoke(
            &choice("codex", &stub, ""),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::OutputMissing,
            &["SECRET_CODEX_STDOUT", "SECRET_CODEX_STDERR"],
        );
        assert_eq!(err.usage.as_deref(), Some(r#"{"input_tokens":13}"#));
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
    fn invoke_gemini_provider_error_precedes_missing_response() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "gemini",
            &format!(
                "{DUMP_PROLOGUE}\
                 cat <<'JSON'\n\
                 {{\"error\":{{\"message\":\"SECRET_GEMINI_BODY\"}},\"stats\":{{\"tokens\":17}}}}\n\
                 JSON\n\
                 printf '%s' 'SECRET_GEMINI_STDERR' >&2\n\
                 exit 37\n"
            ),
        );
        let err = invoke(
            &choice("gemini", &stub, "gemini-2.5-flash"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::ProviderReported,
            &["SECRET_GEMINI_BODY", "SECRET_GEMINI_STDERR"],
        );
        assert_eq!(err.usage.as_deref(), Some(r#"{"tokens":17}"#));
        assert_eq!(err.exit_code, Some(37));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_gemini_missing_response_does_not_retain_output() {
        let bin = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let stub = write_stub(
            bin.path(),
            "gemini",
            &format!(
                "{DUMP_PROLOGUE}\
                 printf '%s' '{{\"other\":\"SECRET_GEMINI_STDOUT\"}}'\n\
                 printf '%s' 'SECRET_GEMINI_MISSING_STDERR' >&2\n"
            ),
        );
        let err = invoke(
            &choice("gemini", &stub, "gemini-2.5-flash"),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(
            &err,
            InvokeFailureKind::OutputMissing,
            &["SECRET_GEMINI_STDOUT", "SECRET_GEMINI_MISSING_STDERR"],
        );
    }

    #[cfg(unix)]
    #[test]
    fn invoke_spawn_failure_does_not_retain_program_path() {
        let work = tempfile::tempdir().unwrap();
        let err = invoke(
            &choice(
                "claude",
                Path::new("/definitely/missing/SECRET_SPAWN_PATH"),
                "haiku",
            ),
            "p",
            work.path(),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert_safe_failure(&err, InvokeFailureKind::SpawnFailed, &["SECRET_SPAWN_PATH"]);
        assert!(err.io_kind.is_some());
        assert!(err.os_error.is_some());
        assert_eq!(err.exit_code, None);
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
        assert_safe_failure(&err, InvokeFailureKind::TimedOut, &[]);
        assert_eq!(err.code(), "cli_timed_out");
    }
}
