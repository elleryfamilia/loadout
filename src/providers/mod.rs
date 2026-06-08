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
//! - **cached** under `.rosita/cache/<id>.json` with a TTL.
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
pub fn run_command(
    command: &str,
    lang: Option<&str>,
    repo_base: &Path,
    key: &str,
    ttl: Duration,
    now: DateTime<Utc>,
    live: bool,
) -> Option<ProviderOutput> {
    cached(repo_base, key, ttl, now, live, || {
        Ok(Some(exec_command(command, lang, None)))
    })
    .unwrap_or(None)
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
/// `rosita run` (which fronts the agent launch). A timeout is fail-closed.
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
        return Ok(cached.map(|e| ProviderOutput {
            text: e.text,
            data: e.data,
        }));
    }
    if let Some(e) = &cached {
        if is_fresh(&e.generated_at, ttl, now) {
            return Ok(Some(ProviderOutput {
                text: e.text.clone(),
                data: e.data.clone(),
            }));
        }
    }

    let Some(mut out) = produce()? else {
        return Ok(None);
    };
    out.text = redact::redact_secrets(&out.text);

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
/// back to stderr; the structured form keeps both plus the exit code.
fn exec_command(command: &str, lang: Option<&str>, cwd: Option<&Path>) -> ProviderOutput {
    let (program, args) = interpreter(lang);
    let mut cmd = Command::new(program);
    cmd.args(args).arg(command);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    match cmd.output() {
        Ok(o) => {
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
            }
        }
        Err(e) => ProviderOutput {
            text: format!("(command failed to run: {e})"),
            data: serde_json::Value::Null,
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

/// Run `program args…` and return its stdout (trimmed) on success, falling back
/// to stderr when stdout is empty. `None` when the program can't be spawned
/// (not installed) or exits non-zero (daemon down, logged out, …).
pub(crate) fn run_ok(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
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
    fn registry_has_the_five_builtins() {
        let ids: Vec<&str> = builtin_providers().iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["host", "toolchain", "ai-tools", "tailnet", "docker"]);
    }
}
