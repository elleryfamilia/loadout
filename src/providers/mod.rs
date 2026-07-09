//! Native environment discovery — the "agent-env idea", built in.
//!
//! A [`EnvProvider`] probes the live environment (host, tailnet, docker,
//! installed toolchains and agent CLIs) and returns a [`ProviderOutput`] a
//! dynamic fragment can embed. Probing is **best-effort**: a missing tool
//! degrades to `None`, never an error.
//!
//! Provider output is machine-specific and volatile, so it is:
//! - **redacted** ([`crate::redact::redact_secrets`]) before it leaves here,
//! - kept **out of [`crate::context::Context`]** so it never affects the context
//!   hash (it lives in a separate [`Probes`] value used only for display/render),
//! - **cached** under `.loadout/cache/<id>.json` with a TTL.
//!
//! New providers are rows in [`builtin_providers`]. Dynamic fragments embed
//! provider output (or a shell `command`) via [`crate::dynamic`].

mod ai_tools;
mod docker;
mod host;
mod tailnet;
mod toolchain;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::context::Context;
use crate::{redact, vlog};

/// What a provider discovered: a human-readable summary plus structured data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOutput {
    /// Redacted, human-readable summary (markdown-ish).
    pub text: String,
    /// Structured form for templates / `--json`.
    pub data: serde_json::Value,
    /// How many token-like strings were redacted from this outcome (both
    /// channels). Not persisted — cache entries are already stored redacted;
    /// a fresh count is produced on every load/exec so the render layer can
    /// warn while the fragment's source keeps emitting secrets.
    #[serde(skip)]
    pub redacted: usize,
}

/// A unit of live environment discovery.
pub trait EnvProvider {
    /// Stable id (`host`, `tailnet`, `docker`, `toolchain`, `ai-tools`).
    fn id(&self) -> &'static str;
    /// Probe the environment. `Ok(None)` means "not available here" (tool
    /// absent, daemon down, logged out) — never fatal.
    fn probe(&self, ctx: &Context) -> crate::Result<Option<ProviderOutput>>;
}

/// The built-in provider registry, in display order.
pub fn builtin_providers() -> Vec<Box<dyn EnvProvider>> {
    vec![
        Box::new(host::HostProvider),
        Box::new(toolchain::ToolchainProvider),
        Box::new(ai_tools::AiToolsProvider),
        Box::new(tailnet::TailnetProvider),
        Box::new(docker::DockerProvider),
    ]
}

/// Probe results for the available providers, in registry order.
#[derive(Debug, Clone, Default)]
pub struct Probes {
    /// `(provider id, output)` for each available provider.
    pub entries: Vec<(String, ProviderOutput)>,
}

impl Probes {
    /// Whether nothing was discovered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Probe every built-in provider (cache-backed, redacted), returning what is
/// available. Provider errors are logged at verbose level and skipped.
///
/// `now` is injected for deterministic cache-freshness tests.
pub fn gather(ctx: &Context, repo_base: &Path, ttl: Duration, now: DateTime<Utc>) -> Probes {
    let mut entries = Vec::new();
    for p in builtin_providers() {
        match probe_provider(p.as_ref(), ctx, repo_base, ttl, now, true) {
            Ok(Some(out)) => entries.push((p.id().to_string(), out)),
            Ok(None) => {}
            Err(e) => vlog!("provider '{}' degraded: {e:#}", p.id()),
        }
    }
    Probes { entries }
}

/// Probe a single built-in provider by id (cache-backed). `live = false` serves
/// only an existing cache entry (any age) and never executes — for read-only
/// callers (`explain`, dry-run). Returns `None` if the id is unknown or the
/// provider is unavailable.
pub fn probe_one(
    id: &str,
    ctx: &Context,
    repo_base: &Path,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
) -> Option<ProviderOutput> {
    let providers = builtin_providers();
    let p = providers.iter().find(|p| p.id() == id)?;
    probe_provider(p.as_ref(), ctx, repo_base, ttl, now, live).unwrap_or(None)
}

/// Run a shell command or script (cached under `key`), embedding its redacted
/// stdout. `lang` picks the interpreter (`bash`/`sh`/`python`); `None` runs the
/// command as a plain `sh -c` line. `live = false` serves only an existing cache
/// entry and never executes.
/// The outcome of resolving a `command` fragment — distinguishes "ran and
/// produced output" from "ran clean but said nothing" and "failed to run", so
/// the studio can show an honest error (with a retry) instead of a blank body.
pub enum CommandOutcome {
    /// Usable (non-empty) output — a fresh run or a fresh cache hit. Cached.
    Output(ProviderOutput),
    /// Ran successfully but produced no output. **Not cached** — a transient
    /// empty (e.g. a daemon mid-restart) must not pin "nothing" for the TTL.
    Empty,
    /// Failed to run: a non-zero exit, a signal kill, or a spawn failure. The
    /// string is a short redacted message. **Not cached** (fail-open to retry).
    Failed(String),
    /// Read-only mode with no usable cache — nothing to show yet.
    NotCached,
}

/// Run a `command` fragment, honoring the cache. Only non-empty successful
/// output is cached; empty-success and failures are surfaced but never cached,
/// so a transient hiccup can't pin a blank result for the whole TTL. A cached
/// entry with empty text (a legacy pre-fix cache) is treated as a miss.
pub fn run_command(
    command: &str,
    lang: Option<&str>,
    repo_base: &Path,
    key: &str,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
) -> CommandOutcome {
    let path = config::cache_dir(repo_base).join(format!("{}.json", sanitize_key(key)));
    let cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<CacheEntry>(&t).ok())
        // Empty cached text is not usable output — ignore it so the card shows a
        // run prompt rather than a misleading blank body.
        .filter(|e| !e.text.trim().is_empty());

    if !live {
        // Read-only: surface any cached value (even stale), never execute.
        return match cached {
            Some(e) => CommandOutcome::Output(redact_output(ProviderOutput {
                text: e.text,
                data: e.data,
                redacted: 0,
            })),
            None => CommandOutcome::NotCached,
        };
    }
    if let Some(e) = &cached {
        if is_fresh(&e.generated_at, ttl, now) {
            return CommandOutcome::Output(redact_output(ProviderOutput {
                text: e.text.clone(),
                data: e.data.clone(),
                redacted: 0,
            }));
        }
    }

    let out = exec_command(command, lang, None);
    // Spawn failure: `exec_command` returns a Null `data` (the program couldn't
    // run at all).
    if out.data.is_null() {
        return CommandOutcome::Failed(redact::redact_secrets(&out.text));
    }
    // Non-zero exit (or a signal kill → `status` is null) is a failure: surface
    // it, don't cache it.
    let status = out.data.get("status").and_then(serde_json::Value::as_i64);
    if status != Some(0) {
        return CommandOutcome::Failed(command_error_message(&out.data));
    }
    if out.text.trim().is_empty() {
        return CommandOutcome::Empty; // ran clean, nothing to add — don't cache
    }
    let out = redact_output(out);

    let entry = CacheEntry {
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        text: out.text.clone(),
        data: out.data.clone(),
    };
    if let Ok(serialized) = serde_json::to_string(&entry) {
        if std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).is_ok() {
            let _ = std::fs::write(&path, serialized); // best-effort
        }
    }
    CommandOutcome::Output(out)
}

/// Build a short, redacted failure message from a command's captured `data`
/// (`status`/`stdout`/`stderr`). Prefers stderr, falls back to stdout.
fn command_error_message(data: &serde_json::Value) -> String {
    let pick = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
    let detail = match (pick("stderr"), pick("stdout")) {
        ("", out) => out,
        (err, _) => err,
    };
    let prefix = match data.get("status").and_then(serde_json::Value::as_i64) {
        Some(c) => format!("command exited {c}"),
        None => "command was killed by a signal".to_string(),
    };
    let msg = if detail.is_empty() {
        prefix
    } else {
        format!("{prefix}: {detail}")
    };
    redact::redact_secrets(&msg)
}

/// Run a command once, bypassing the cache — for studio's "test run" of a draft
/// script in the editor. Same interpreter + capture path as render-time
/// execution ([`run_command`]), so what the user sees is what would be embedded.
pub fn run_once(command: &str, lang: Option<&str>) -> ProviderOutput {
    exec_command(command, lang, None)
}

/// Like [`run_once`] but with the working directory set to `dir` — for the
/// studio "Try" of a draft target predicate, so a `test -f …` resolves against
/// the repo (matching how detection runs it).
pub fn run_once_in(command: &str, lang: Option<&str>, dir: &Path) -> ProviderOutput {
    exec_command(command, lang, Some(dir))
}

/// Hard cap on a detection script predicate, so a hanging script can't stall
/// `load run` (which fronts the agent launch). A timeout is fail-closed.
const PREDICATE_TIMEOUT: Duration = Duration::from_secs(3);

/// Run a **script predicate** for target detection: exit 0 ⇒ match. Unlike
/// [`run_command`], it runs with the working directory set to `repo_base`
/// (so `test -f WORKSPACE` works), bounds the run with [`PREDICATE_TIMEOUT`],
/// and caches only the boolean verdict (keyed by `key`, which the caller derives
/// from the target id + a hash of the script so an edited script busts the
/// cache). `live = false` serves an existing cached verdict and never executes.
/// Returns `None` when there is no verdict — a cold cache in read-only mode, a
/// spawn failure, or a timeout — and callers treat `None` as **no match**.
pub fn run_predicate(
    command: &str,
    lang: Option<&str>,
    repo_base: &Path,
    key: &str,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
) -> Option<bool> {
    cached(repo_base, key, ttl, now, live, || {
        Ok(Some(exec_predicate(command, lang, repo_base)))
    })
    .ok()
    .flatten()
    .and_then(|out| out.data.get("matched").and_then(serde_json::Value::as_bool))
}

/// Run a script predicate under the chosen interpreter with `cwd = repo_base`
/// and a bounded timeout, capturing only the exit-0 verdict into a
/// [`ProviderOutput`] (`data.matched`). stdout/stderr are discarded — only the
/// exit code decides the match. A non-zero exit, a signal/timeout kill, or a
/// spawn failure all resolve to `matched: false` (fail-closed).
fn exec_predicate(command: &str, lang: Option<&str>, repo_base: &Path) -> ProviderOutput {
    use std::process::Stdio;
    use std::time::Instant;
    let (program, args) = interpreter(lang);
    let spawned = Command::new(program)
        .args(args)
        .arg(command)
        .current_dir(repo_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let matched = match spawned {
        Ok(mut child) => {
            let deadline = Instant::now() + PREDICATE_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status.success(),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break false; // timed out → fail-closed
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break false,
                }
            }
        }
        Err(_) => false,
    };
    ProviderOutput {
        text: String::new(),
        data: serde_json::json!({ "matched": matched }),
        redacted: 0,
    }
}

/// The interpreter program + leading args for a script `lang` (or a plain
/// `sh -c` shell line when `lang` is `None`/unrecognized).
fn interpreter(lang: Option<&str>) -> (&'static str, &'static [&'static str]) {
    match lang {
        Some("bash") => ("bash", &["-c"]),
        Some("python") => ("python3", &["-c"]),
        _ => ("sh", &["-c"]),
    }
}

/// A cache entry on disk.
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    generated_at: String,
    text: String,
    data: serde_json::Value,
}

/// Redact both channels of a provider outcome, counting replacements so the
/// render layer can warn while the fragment's source keeps emitting secrets.
/// Cache entries read back from disk are re-redacted too, purging `data`
/// cached raw by older versions.
fn redact_output(mut out: ProviderOutput) -> ProviderOutput {
    let (text, n_text) = redact::redact_secrets_report(&out.text);
    out.text = text;
    let n_data = redact::redact_json(&mut out.data);
    out.redacted = n_text + n_data;
    out
}

fn probe_provider(
    p: &dyn EnvProvider,
    ctx: &Context,
    repo_base: &Path,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
) -> crate::Result<Option<ProviderOutput>> {
    cached(repo_base, p.id(), ttl, now, live, || p.probe(ctx))
}

/// Cache wrapper. Serves a fresh cache hit; otherwise (when `live`) runs
/// `produce`, redacts its text, and writes the cache. When `!live`, returns any
/// existing cache entry (regardless of age) and never runs `produce` or writes.
/// `None` results are not cached.
fn cached<F>(
    repo_base: &Path,
    key: &str,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
    produce: F,
) -> crate::Result<Option<ProviderOutput>>
where
    F: FnOnce() -> crate::Result<Option<ProviderOutput>>,
{
    let path = config::cache_dir(repo_base).join(format!("{}.json", sanitize_key(key)));
    let cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<CacheEntry>(&t).ok());

    if !live {
        // Read-only: surface any cached value (even stale), never execute.
        return Ok(cached.map(|e| {
            redact_output(ProviderOutput {
                text: e.text,
                data: e.data,
                redacted: 0,
            })
        }));
    }
    if let Some(e) = &cached {
        if is_fresh(&e.generated_at, ttl, now) {
            return Ok(Some(redact_output(ProviderOutput {
                text: e.text.clone(),
                data: e.data.clone(),
                redacted: 0,
            })));
        }
    }

    let Some(out) = produce()? else {
        return Ok(None);
    };
    let out = redact_output(out);

    let entry = CacheEntry {
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        text: out.text.clone(),
        data: out.data.clone(),
    };
    if let Ok(serialized) = serde_json::to_string(&entry) {
        if std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).is_ok() {
            let _ = std::fs::write(&path, serialized); // best-effort
        }
    }
    Ok(Some(out))
}

/// Keep cache filenames safe regardless of provider id / command key.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Run a command/script body under the chosen interpreter and capture its
/// output into a [`ProviderOutput`]. stdout is preferred for `text`, falling
/// back to stderr; the structured form keeps both plus the exit code. Bounded
/// by [`EXEC_TIMEOUT`] — a hung script degrades to an error note, never a
/// hung render.
fn exec_command(command: &str, lang: Option<&str>, cwd: Option<&Path>) -> ProviderOutput {
    let (program, args) = interpreter(lang);
    let mut cmd = Command::new(program);
    cmd.args(args).arg(command);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    match output_with_timeout(&mut cmd, EXEC_TIMEOUT) {
        Ok(None) => ProviderOutput {
            text: format!(
                "(command timed out after {}s and was killed)",
                EXEC_TIMEOUT.as_secs()
            ),
            data: serde_json::Value::Null,
            redacted: 0,
        },
        Ok(Some(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let text = if !stdout.is_empty() {
                stdout.clone()
            } else {
                stderr.clone()
            };
            ProviderOutput {
                text,
                data: serde_json::json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "status": o.status.code(),
                }),
                redacted: 0,
            }
        }
        Err(e) => ProviderOutput {
            text: format!("(command failed to run: {e})"),
            data: serde_json::Value::Null,
            redacted: 0,
        },
    }
}

/// Whether a cache `generated_at` (RFC3339) is within `ttl` of `now`.
pub fn is_fresh(generated_at: &str, ttl: Duration, now: DateTime<Utc>) -> bool {
    let Ok(t) = DateTime::parse_from_rfc3339(generated_at) else {
        return false;
    };
    let age = now.signed_duration_since(t.with_timezone(&Utc));
    match age.to_std() {
        Ok(age) => age <= ttl, // 0 <= age <= ttl
        Err(_) => false,       // negative age (future stamp) → re-probe
    }
}

/// Parse a duration like `60s`, `5m`, `2h`, `500ms`, or a bare seconds count.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s.find(|c: char| c.is_ascii_alphabetic()) {
        None => s.parse::<u64>().ok().map(Duration::from_secs),
        Some(i) => {
            let n: u64 = s[..i].trim().parse().ok()?;
            match s[i..].trim() {
                "ms" => Some(Duration::from_millis(n)),
                "s" => Some(Duration::from_secs(n)),
                "m" => Some(Duration::from_secs(n * 60)),
                "h" => Some(Duration::from_secs(n * 3600)),
                _ => None,
            }
        }
    }
}

// --- shared exec helpers (used by the shell-out providers) -------------------

/// Hard cap on any probe/command subprocess. A wedged daemon (an unresponsive
/// `docker ps`, a CLI waiting for a TTY) must degrade to "no output" — never
/// hang the render, or the `load hook` process running behind every IDE
/// session start. Generous because the slow path this must tolerate is a cold
/// first probe, not the wedge itself.
const EXEC_TIMEOUT: Duration = Duration::from_secs(10);

/// `Command::output()` with a deadline. stdout/stderr are drained by reader
/// threads (a chatty child can't deadlock on a full pipe), stdin is null (a
/// CLI that prompts gets EOF, not a wait), and the child — plus, on unix, its
/// whole process group — is killed at the deadline. `Ok(None)` = the child was
/// still running at the deadline.
///
/// The deadline covers the **reads** too, not just the child's exit: a
/// grandchild that inherited a pipe write-end (a CLI's background update
/// check, a `sh -c` leaving its command behind) keeps the pipe open long
/// after the direct child exited, so an unconditional reader join would
/// resurrect the hang. Readers therefore drain into shared buffers, the
/// post-exit wait is deadline-bounded, and on expiry the process group is
/// killed (reaping the pipe-holder) and whatever was read so far is returned.
/// Never-joined reader threads expire with the killed group.
fn output_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    // A fresh process group (unix) so the deadline kill reaches background
    // grandchildren, not just the direct child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    fn drain<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
        buf: Arc<Mutex<Vec<u8>>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let Some(mut p) = pipe else { return };
            let mut chunk = [0u8; 4096];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        })
    }
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let t_out = drain(child.stdout.take(), Arc::clone(&out_buf));
    let t_err = drain(child.stderr.take(), Arc::clone(&err_buf));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                kill_group(&mut child);
                let _ = child.wait();
                return Ok(None); // timed out
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // The child exited; the reads are only complete once both pipes hit EOF,
    // which a lingering grandchild can hold off indefinitely — bound this too.
    let drained = loop {
        if t_out.is_finished() && t_err.is_finished() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if drained {
        let _ = t_out.join();
        let _ = t_err.join();
    } else {
        kill_group(&mut child); // reap the pipe-holding grandchildren
    }
    let stdout = std::mem::take(&mut *out_buf.lock().unwrap());
    let stderr = std::mem::take(&mut *err_buf.lock().unwrap());
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

/// Kill the child and, on unix, its whole process group — the child was
/// spawned as a group leader (`process_group(0)`), so this also takes out
/// background grandchildren that would otherwise linger holding a pipe end
/// (observed: agent-CLI update checks surviving `--version` for hours).
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: killpg only sends a signal; the pgid can't have been reused
    // while a member of the group is still alive.
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Run `program args…` and return its stdout (trimmed) on success, falling back
/// to stderr when stdout is empty. `None` when the program can't be spawned
/// (not installed), exits non-zero (daemon down, logged out, …), or exceeds
/// [`EXEC_TIMEOUT`] (daemon wedged).
pub(crate) fn run_ok(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let out = output_with_timeout(&mut cmd, EXEC_TIMEOUT).ok()??;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Some(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    (!stderr.is_empty()).then_some(stderr)
}

/// Hard cap on an agent-CLI `--version` probe (doctor's PATH checks) — the
/// same discipline as script predicates ([`PREDICATE_TIMEOUT`]): a wedged CLI
/// must degrade to a warning, never hang. `LOADOUT_PROBE_TIMEOUT_MS` overrides
/// it (tests use this; it's also an escape hatch for a legitimately slow CLI).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

fn probe_timeout() -> Duration {
    std::env::var("LOADOUT_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(PROBE_TIMEOUT)
}

/// Outcome of a bounded `<program> --version` probe.
pub(crate) enum CliProbe {
    /// Spawned and answered — carries the trimmed version output.
    Found(String),
    /// Could not spawn (not installed) or produced no usable answer.
    Missing,
    /// Spawned but still running at the deadline — killed. The binary exists,
    /// but the CLI is wedged (or slower than [`PROBE_TIMEOUT`]).
    TimedOut,
}

/// Probe `<program> --version` under [`probe_timeout`]. Replaces doctor's raw
/// `Command::output()` PATH check, which had no deadline and hung indefinitely
/// on CLIs whose version check leaves a pipe-holding background updater
/// (GitHub Copilot's `--version` was observed doing exactly that).
pub(crate) fn probe_cli(program: &str) -> CliProbe {
    let mut cmd = Command::new(program);
    cmd.arg("--version");
    match output_with_timeout(&mut cmd, probe_timeout()) {
        Ok(Some(o)) if o.status.success() || !o.stdout.is_empty() => {
            CliProbe::Found(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        Ok(Some(_)) => CliProbe::Missing,
        Ok(None) => CliProbe::TimedOut,
        Err(_) => CliProbe::Missing,
    }
}

/// Probe `<tool> --version` for each tool, collecting `(tool, version)` for the
/// ones present. Shared by the `toolchain` and `ai-tools` providers.
pub(crate) fn probe_versions(tools: &[&str]) -> Vec<(String, String)> {
    tools
        .iter()
        .filter_map(|t| run_ok(t, &["--version"]).map(|raw| (t.to_string(), parse_version(&raw))))
        .collect()
}

/// Extract a version token (`1.85.0`, `2.43.0`, …) from a `--version` line,
/// falling back to the trimmed first line when no version-looking token is found.
pub(crate) fn parse_version(raw: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\d+\.\d+(?:\.\d+)?(?:[\w.\-+]*)").unwrap());
    let first = raw.lines().next().unwrap_or("").trim();
    re.find(first)
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90))); // bare = seconds
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("10x"), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn cache_freshness_within_ttl() {
        let now = DateTime::parse_from_rfc3339("2026-05-29T00:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 30s old, ttl 60s → fresh.
        assert!(is_fresh(
            "2026-05-29T00:00:30Z",
            Duration::from_secs(60),
            now
        ));
        // 60s old, ttl 60s → still fresh (boundary inclusive).
        assert!(is_fresh(
            "2026-05-29T00:00:00Z",
            Duration::from_secs(60),
            now
        ));
        // 61s old, ttl 60s → stale.
        assert!(!is_fresh(
            "2026-05-28T23:59:59Z",
            Duration::from_secs(60),
            now
        ));
        // garbage / future / unparseable → not fresh.
        assert!(!is_fresh("not-a-date", Duration::from_secs(60), now));
        assert!(!is_fresh(
            "2026-05-29T00:02:00Z",
            Duration::from_secs(60),
            now
        ));
    }

    #[test]
    fn parse_version_extracts_token() {
        assert_eq!(parse_version("cargo 1.85.0 (abc 2025-01-01)"), "1.85.0");
        assert_eq!(parse_version("git version 2.43.0"), "2.43.0");
        assert_eq!(parse_version("Python 3.12.1"), "3.12.1");
        assert_eq!(parse_version("v22.1.0"), "22.1.0");
        // No version token → trimmed first line.
        assert_eq!(parse_version("weird tool\nsecond line"), "weird tool");
    }

    #[test]
    fn run_command_caches_only_nonempty_success() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let now = DateTime::parse_from_rfc3339("2026-06-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ttl = Duration::from_secs(300);
        let cache = config::cache_dir(base).join("cmd-k.json");

        // Success with output → returned and cached.
        match run_command("printf hello", Some("sh"), base, "cmd-k", ttl, now, true) {
            CommandOutcome::Output(o) => assert_eq!(o.text, "hello"),
            _ => panic!("expected Output"),
        }
        assert!(cache.exists(), "non-empty success should cache");

        // Empty-but-clean run → Empty, and nothing cached.
        let dir2 = tempfile::tempdir().unwrap();
        let base2 = dir2.path();
        assert!(matches!(
            run_command("true", Some("sh"), base2, "cmd-k", ttl, now, true),
            CommandOutcome::Empty
        ));
        assert!(
            !config::cache_dir(base2).join("cmd-k.json").exists(),
            "empty output must not be cached"
        );

        // Non-zero exit → Failed with the stderr message, and nothing cached.
        let dir3 = tempfile::tempdir().unwrap();
        let base3 = dir3.path();
        match run_command(
            "echo boom >&2; exit 3",
            Some("sh"),
            base3,
            "cmd-k",
            ttl,
            now,
            true,
        ) {
            CommandOutcome::Failed(msg) => {
                assert!(msg.contains("exited 3"), "message: {msg}");
                assert!(msg.contains("boom"), "message: {msg}");
            }
            _ => panic!("expected Failed"),
        }
        assert!(
            !config::cache_dir(base3).join("cmd-k.json").exists(),
            "failure must not be cached"
        );
    }

    #[test]
    fn run_command_ignores_empty_cache_entry() {
        // A legacy/pre-fix cache with empty text is treated as a miss in
        // read-only mode (so the card shows a run prompt, not a blank body).
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let path = config::cache_dir(base).join("cmd-k.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"generated_at":"2026-06-09T12:00:00Z","text":"","data":{"status":0}}"#,
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-06-09T12:00:30Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(matches!(
            run_command(
                "true",
                Some("sh"),
                base,
                "cmd-k",
                Duration::from_secs(300),
                now,
                false
            ),
            CommandOutcome::NotCached
        ));
    }

    #[test]
    fn registry_has_the_five_builtins() {
        let ids: Vec<&str> = builtin_providers().iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["host", "toolchain", "ai-tools", "tailnet", "docker"]);
    }

    #[test]
    fn output_with_timeout_kills_a_wedged_child_quickly() {
        use std::time::{Duration, Instant};
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let out = super::output_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap();
        assert!(out.is_none(), "timed out run must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return promptly after the deadline, not wait for the child"
        );
    }

    #[test]
    fn output_with_timeout_captures_both_streams_on_success() {
        use std::time::Duration;
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2"]);
        let out = super::output_with_timeout(&mut cmd, Duration::from_secs(10))
            .unwrap()
            .expect("fast command must not time out");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
    }

    #[test]
    fn output_with_timeout_survives_a_pipe_holding_grandchild() {
        use std::time::{Duration, Instant};
        // The copilot repro: the probed CLI exits quickly but leaves behind a
        // background child (an update check) that inherited the stdout pipe.
        // The pipe's write end stays open long after the direct child exited,
        // so an unbounded read_to_end/join would block until the grandchild
        // dies — minutes to hours. The deadline must cover the reads too.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo held; sleep 30 & exit 0"]);
        let started = Instant::now();
        let out = super::output_with_timeout(&mut cmd, Duration::from_millis(500)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return by the deadline even though a grandchild holds the pipe"
        );
        let out = out.expect("child exited before the deadline — not a timeout");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "held",
            "output written before the deadline must be captured"
        );
    }

    #[test]
    fn output_with_timeout_gives_children_no_stdin() {
        use std::time::{Duration, Instant};
        // A CLI that waits for input (the "press any key" class) must get EOF
        // and exit immediately, not sit on a TTY read until the deadline.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "read -r x; echo done"]);
        let started = Instant::now();
        let out = super::output_with_timeout(&mut cmd, Duration::from_secs(10)).unwrap();
        assert!(
            out.is_some(),
            "EOF on stdin must end the read, not time out"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
