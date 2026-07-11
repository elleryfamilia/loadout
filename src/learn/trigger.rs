//! The trigger fast path: the cheap guard chain every loadout command runs, and
//! the detached double-spawn that launches the harvest worker without blocking
//! the user's command.
//!
//! [`maybe_spawn`] is called at loadout's own entry points (`run`, `refresh`,
//! `studio`, `hook serve`) and by the session-end hook handler. It NEVER blocks
//! (it waits only on a millisecond-lived intermediate) and NEVER errors outward
//! (every failure degrades to a verbose log line) — a leaked child or a blocking
//! wait would tax EVERY loadout invocation, so both are design requirements.
//!
//! ## Guard chain (cheapest first — the order is part of the spec)
//!
//! [`should_spawn`] short-circuits on the first guard that fails, in this exact
//! order:
//!
//! 1. `cfg.learn.enabled` — a config-field read, no I/O (config is already
//!    loaded at every trigger site, read AFTER that flow's throttled auto-pull,
//!    so a synced `load learn off` lands before this check).
//! 2. per-machine activation ack present ([`state::read_activation_at`]).
//! 3. `LOADOUT_LEARN != "off"` — the user kill switch.
//! 4. `LOADOUT_LEARN_WORKER` unset — the recursion guard (the worker's own
//!    agent-CLI call carries this, so a nested `load` can never re-trigger).
//! 5. not paused ([`state::paused_at`] — 2+ consecutive failures).
//! 6. the scan stamp is past its 15-min debounce (bounds scan thrash).
//! 7. the spend stamp is past `learn.interval`, OR an eligibility hint exists
//!    (a session-end hook named a just-ended session).
//!
//! Residual idle cost when enabled: the config field plus a few `stat()`s. In
//! the common steady state (harvested recently) guard 6 short-circuits before
//! guard 7 ever reads the hint dir.
//!
//! ## Detached double-spawn (unix)
//!
//! ```text
//! trigger process  (load run / refresh / studio / hook)
//!   └─ Command::spawn  → intermediate   (argv: <exe> harvest --ambient,
//!        │                                process_group(0), stdio → worker.log)
//!        │  pre_exec double-fork:
//!        ├─ intermediate branch: libc::_exit(0)      ← parent's wait() reaps
//!        │                                              this in milliseconds
//!        └─ grandchild branch:  setsid() + nice(10)  → exec → WORKER
//!                                                        (no living ancestor
//!                                                         but init)
//! ```
//!
//! The parent waits ONLY on the intermediate, which forks the real worker and
//! `_exit(0)`s at once — so trigger latency is milliseconds. The second fork
//! (inside `pre_exec`, running only async-signal-safe libc calls) reparents the
//! worker to init, so it never becomes a zombie under a long-lived ancestor
//! (`studio`, or the agent that `run` will `exec()` into). `#[cfg(not(unix))]`
//! is a graceful no-op — no Windows dist target ships.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::learn::state;
use crate::vlog;

/// The scan-stamp debounce: a worker start is throttled to at most once per this
/// window regardless of how many triggers fire (bounds free re-scan thrash).
const SCAN_DEBOUNCE: Duration = Duration::from_secs(15 * 60);

/// Eligibility-hint TTL. A hint older than this is swept (deleted) on read,
/// whatever the run's outcome — the deterministic backstop for carry-forward
/// C13, where a *never-harvestable* hint would otherwise be re-read on every
/// worker start forever. A hint becomes never-harvestable when its session is
/// permanently out of scope (`scope = Adopted`, repo never adopted) or its
/// transcript was rotated away before any harvest reached it: the normal
/// success-path deletion (step 8 of the worker) is never taken, so nothing
/// removes it. 7 days is far longer than any legitimately-pending session waits
/// for its first harvest (the spend interval defaults to 6h), so a swept hint
/// was, with certainty, going to be harvested at run time — never.
const HINT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which entry point fired the trigger. All variants funnel into the same
/// throttled worker (more triggers never mean more spend); the label is carried
/// for verbose diagnostics and to document intent at each call site.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// `load run` (pre-exec, before it `exec()`s the agent).
    Run,
    /// `load refresh`.
    Refresh,
    /// `load studio` start.
    Studio,
    /// A `load hook … serve` invocation.
    HookServe,
    /// A session-end hook handler named a just-ended session of `agent`.
    SessionEnd { agent: String },
}

/// Why the guard chain declined to spawn (verbose-log detail only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// `[learn] enabled = false` in config.
    Disabled,
    /// This machine was never activated (`load learn on` not run here).
    NoActivation,
    /// `LOADOUT_LEARN=off` — the user kill switch.
    KillSwitch,
    /// `LOADOUT_LEARN_WORKER` is set — we are already inside a worker's spawn.
    Recursion,
    /// Ambient triggering is paused after repeated failures.
    Paused,
    /// The scan stamp is still within its debounce window.
    ScanDebounced,
    /// Neither the spend interval elapsed nor an eligibility hint exists.
    NotDue,
}

/// The two guard inputs that come from the process environment, captured once so
/// [`should_spawn`] stays a pure function of `(config, filesystem, clock)` and is
/// unit-testable without mutating process-global environment variables.
#[derive(Debug, Clone, Copy)]
struct EnvGuards {
    /// `LOADOUT_LEARN == "off"`.
    kill_switch: bool,
    /// `LOADOUT_LEARN_WORKER` is set (to anything).
    worker: bool,
}

impl EnvGuards {
    fn from_env() -> Self {
        EnvGuards {
            kill_switch: std::env::var_os("LOADOUT_LEARN").is_some_and(|v| v == "off"),
            worker: std::env::var_os("LOADOUT_LEARN_WORKER").is_some(),
        }
    }
}

/// Run the guard chain in the spec's exact order and decide whether a worker
/// should be spawned. `Ok(())` means every guard held; `Err(Skip)` names the
/// first that failed. Pure over `(cfg, learn_dir, env, now)` — no process-env
/// reads, no spawning — so the whole chain is exercised by deterministic tests.
///
/// Guard 1 (the config field) is checked before any filesystem access, so a
/// disabled machine pays nothing beyond the field read.
fn should_spawn(
    cfg: &Config,
    learn_dir: &Path,
    env: &EnvGuards,
    now: SystemTime,
) -> Result<(), Skip> {
    // 1. config field (no I/O)
    if !cfg.learn.enabled {
        return Err(Skip::Disabled);
    }
    // 2. per-machine activation ack
    if state::read_activation_at(learn_dir).is_none() {
        return Err(Skip::NoActivation);
    }
    // 3. user kill switch
    if env.kill_switch {
        return Err(Skip::KillSwitch);
    }
    // 4. recursion guard
    if env.worker {
        return Err(Skip::Recursion);
    }
    // 5. failure-pause
    if state::paused_at(learn_dir) {
        return Err(Skip::Paused);
    }
    // 6 + 7. scan-stamp debounce, then spend interval OR a session-end
    //    eligibility hint. The hint bypasses the spend interval (a just-ended
    //    session should be harvested promptly) but NOT the scan-stamp debounce.
    //    Shared with `load learn status` via [`eligibility_at`], so the status
    //    line and the guard chain can never drift apart.
    let e = eligibility_at(learn_dir, cfg.learn.interval, now);
    if !e.scan_due {
        return Err(Skip::ScanDebounced);
    }
    if !e.spend_due && !e.hint {
        return Err(Skip::NotDue);
    }
    Ok(())
}

/// The guard-6/7 eligibility view, shared by [`should_spawn`] and `load learn
/// status` (one source of truth for "when would a trigger actually run?").
/// Pure over `(learn_dir, interval, now)` like the guard chain itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Eligibility {
    /// Guard 6: the scan-stamp debounce window has elapsed.
    pub scan_due: bool,
    /// Guard 7a: the spend interval has elapsed.
    pub spend_due: bool,
    /// Guard 7b: a session-end eligibility hint is waiting. It bypasses the
    /// spend interval, but not the scan debounce.
    pub hint: bool,
    /// How long until a trigger would pass guards 6+7: zero when eligible
    /// now, else the later of the scan-debounce remainder and — absent a
    /// hint — the spend-interval remainder.
    pub wait: Duration,
}

impl Eligibility {
    /// Whether a trigger firing right now would pass guards 6+7.
    pub fn now(&self) -> bool {
        self.scan_due && (self.spend_due || self.hint)
    }
}

/// Compute the guard-6/7 [`Eligibility`] from both throttle stamps and the
/// hint dir under `learn_dir`.
pub(crate) fn eligibility_at(learn_dir: &Path, interval: Duration, now: SystemTime) -> Eligibility {
    let scan_last = state::read_stamp(&learn_dir.join("scan-stamp"));
    let spend_last = state::read_stamp(&learn_dir.join("spend-stamp"));
    let hint = has_hint(learn_dir);
    let scan_due = state::is_due(scan_last, now, SCAN_DEBOUNCE);
    let spend_due = state::is_due(spend_last, now, interval);
    let scan_wait = remaining(scan_last, now, SCAN_DEBOUNCE);
    let content_wait = if hint || spend_due {
        Duration::ZERO // a hint (or an elapsed interval) satisfies guard 7 already
    } else {
        remaining(spend_last, now, interval)
    };
    Eligibility {
        scan_due,
        spend_due,
        hint,
        wait: scan_wait.max(content_wait),
    }
}

/// Time left on a stamp's interval; zero when due (including never-run and
/// clock-went-backwards, matching [`state::is_due`]'s semantics exactly).
fn remaining(last: Option<SystemTime>, now: SystemTime, interval: Duration) -> Duration {
    match last {
        Some(last) if !state::is_due(Some(last), now, interval) => {
            interval.saturating_sub(now.duration_since(last).unwrap_or_default())
        }
        _ => Duration::ZERO,
    }
}

/// The cheap guard-7 check: at least one eligibility hint file exists.
fn has_hint(learn_dir: &Path) -> bool {
    std::fs::read_dir(eligible_dir(learn_dir))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// The trigger fast path. Runs the guard chain and, only if every guard holds,
/// launches the detached worker. Never blocks past the millisecond-lived
/// intermediate and never returns an error — every failure is swallowed to a
/// verbose log line.
pub fn maybe_spawn(cfg: &Config, trigger: Trigger) {
    let Some(learn_dir) = state::learn_dir() else {
        vlog!("learning: no state dir; trigger ({trigger:?}) skipped");
        return;
    };
    if let Err(skip) = should_spawn(cfg, &learn_dir, &EnvGuards::from_env(), SystemTime::now()) {
        vlog!("learning: trigger ({trigger:?}) skipped: {skip:?}");
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            vlog!("learning: cannot resolve current_exe ({e}); trigger skipped");
            return;
        }
    };
    if let Err(e) = spawn_worker(&exe, &learn_dir.join("worker.log")) {
        vlog!("learning: detached worker spawn failed: {e}");
    }
}

/// Launch `<exe> harvest --ambient` as a fully detached worker and return once
/// the intermediate has been reaped (milliseconds). See the module docs for the
/// process tree. Unix-only; a non-unix build is a graceful no-op.
///
/// This is the injected spawn seam: tests point `exe` at a short-lived stub so
/// the real `load harvest` never runs.
fn spawn_worker(exe: &Path, log_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Append: the worker writes its own structured `log.jsonl`; this file
        // only catches stray stdout/stderr, so accumulating across runs is fine.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let log_err = log.try_clone()?;

        let mut cmd = Command::new(exe);
        cmd.arg("harvest")
            .arg("--ambient")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            // Isolate the intermediate's process group (the src/providers/mod.rs
            // precedent); the grandchild gets its own session via setsid below.
            .process_group(0);

        // SAFETY: the closure runs in the forked child after fork(), before
        // exec(). It calls only async-signal-safe libc functions
        // (fork/_exit/setsid/nice) and touches no heap or locks — the sole
        // discipline required to fork safely from a possibly-threaded parent.
        // The second fork + immediate `_exit(0)` of the intermediate reparents
        // the grandchild (the real worker) to init, so it never zombies under a
        // long-lived ancestor.
        unsafe {
            cmd.pre_exec(|| {
                match libc::fork() {
                    // fork failed → surface as a spawn error (nothing detached).
                    -1 => Err(io::Error::last_os_error()),
                    // Grandchild (the worker): detach from the controlling
                    // terminal and lower priority. Both are best-effort — a
                    // failure must not abort the worker, so the return values are
                    // deliberately ignored (nice(10) failing is explicitly
                    // ignorable per the design).
                    0 => {
                        libc::setsid();
                        libc::nice(10);
                        Ok(())
                    }
                    // Intermediate: its job (forking the worker) is done. Exit
                    // now so the parent's wait() returns in milliseconds.
                    _ => libc::_exit(0),
                }
            });
        }

        let mut child = cmd.spawn()?;
        // Bounded-fast: the intermediate `_exit(0)`s right after forking the
        // worker, so this reaps it in milliseconds and never waits on the worker
        // itself (a different pid, reparented to init).
        let _ = child.wait();
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (exe, log_path);
        vlog!("learning: detached worker spawn is unix-only; skipping");
        Ok(())
    }
}

// --- eligibility hints -------------------------------------------------------

/// The dir holding session-end eligibility hints under a learn dir.
fn eligible_dir(learn_dir: &Path) -> PathBuf {
    learn_dir.join("eligible")
}

/// Record that `session_id` (from `agent`) just ended, so the next worker treats
/// it as immediately eligible (bypassing the readers' quiescence wait). Creates
/// `state_dir/learn/eligible/<agent>-<session_id>`. Best-effort: a failure is
/// logged verbosely and swallowed (the session is still harvested on the normal
/// quiescence schedule).
pub fn write_eligibility_hint(agent: &str, session_id: &str) {
    let Some(learn_dir) = state::learn_dir() else {
        vlog!("learning: no state dir; cannot record eligibility hint");
        return;
    };
    if let Err(e) = write_hint_at(&learn_dir, agent, session_id) {
        vlog!("learning: could not write eligibility hint: {e}");
    }
}

/// Path-explicit seam behind [`write_eligibility_hint`] (unit-testable without
/// touching the real per-machine state dir).
fn write_hint_at(learn_dir: &Path, agent: &str, session_id: &str) -> io::Result<()> {
    let dir = eligible_dir(learn_dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(hint_name(agent, session_id)), b"")
}

/// The hint filename `<agent>-<session_id>`. The agent portion is sanitized to
/// contain no `-` so the FIRST `-` is always the agent/session boundary — real
/// session ids (uuids) keep their own hyphens, and [`read_hints`] recovers them
/// with a single `split_once('-')`. Path separators and other odd characters in
/// either portion are replaced so a hint can never escape the eligible dir.
fn hint_name(agent: &str, session_id: &str) -> String {
    let agent = sanitize(agent).replace('-', "_");
    format!("{agent}-{}", sanitize(session_id))
}

/// Keep `[A-Za-z0-9._-]`; replace everything else (path separators, control
/// chars) with `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Read every eligibility hint under `learn_dir`, returning the session ids to
/// merge into the readers' `hooked` set (seeded from `seed`) and the hint file
/// paths to delete once a run has advanced its watermarks past them. Called by
/// the worker at step 3; the deletion is the worker's, gated on the fence and
/// done only on the success path (see the worker).
pub(crate) fn read_hints(
    learn_dir: &Path,
    seed: &BTreeSet<String>,
) -> (BTreeSet<String>, Vec<PathBuf>) {
    let mut hooked = seed.clone();
    let mut paths = Vec::new();
    let Ok(entries) = std::fs::read_dir(eligible_dir(learn_dir)) else {
        return (hooked, paths);
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // TTL sweep (C13 backstop): a hint older than `HINT_TTL` is deleted here
        // and now, whatever this run's outcome — otherwise a never-harvestable
        // hint (permanently out of scope, or its transcript rotated away) would
        // wake the worker on every scan forever. Safe to delete mid-read: this
        // runs under the single-writer harvest lock, so no concurrent worker is
        // reading the same dir, and a hint written by a session-end hook after
        // this point is far younger than the TTL.
        if hint_expired(&entry, now) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // `<agent>-<session_id>`; the session id is everything after the first
        // `-` (agent names carry none). A name without a `-` is malformed —
        // skip it but still schedule it for deletion so it can't wedge.
        if let Some((_agent, session_id)) = name.split_once('-') {
            if !session_id.is_empty() {
                hooked.insert(session_id.to_string());
            }
        }
        paths.push(path);
    }
    (hooked, paths)
}

/// Whether a hint file's mtime is older than [`HINT_TTL`]. A file whose mtime
/// can't be read is treated as NOT expired (fail safe — never delete a hint we
/// can't age; the success-path deletion will still reclaim it once harvested).
fn hint_expired(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .map(|age| age > HINT_TTL)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::state::Activation;

    /// A learn dir under a fresh tempdir, plus the tempdir guard kept alive.
    struct Env {
        _tmp: tempfile::TempDir,
        learn_dir: PathBuf,
    }

    fn learn_env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let learn_dir = tmp.path().join("learn");
        std::fs::create_dir_all(&learn_dir).unwrap();
        Env {
            _tmp: tmp,
            learn_dir,
        }
    }

    /// A far-past instant so `now` is always well after any stamp we write.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    /// An enabled config whose spend interval is 6h (the default).
    fn enabled_config() -> Config {
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        cfg
    }

    fn activate(learn_dir: &Path) {
        state::write_activation_at(
            learn_dir,
            &Activation {
                machine_id: "test".into(),
                hostname: "host.local".into(),
                activated_at: "2026-07-10T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    /// All-pass env guards (no kill switch, not inside a worker).
    fn pass_env() -> EnvGuards {
        EnvGuards {
            kill_switch: false,
            worker: false,
        }
    }

    /// Write a stamp file holding an ancient unix-seconds value → always "due".
    fn stale_stamp(learn_dir: &Path, name: &str) {
        std::fs::write(learn_dir.join(name), "1000").unwrap();
    }

    // --- guard chain, in order --------------------------------------------

    #[test]
    fn disabled_config_short_circuits_first() {
        let e = learn_env();
        let cfg = Config::defaults(); // enabled = false
        assert_eq!(
            should_spawn(&cfg, &e.learn_dir, &pass_env(), now()),
            Err(Skip::Disabled)
        );
    }

    #[test]
    fn missing_activation_short_circuits() {
        let e = learn_env();
        // enabled, but never activated on this machine.
        assert_eq!(
            should_spawn(&enabled_config(), &e.learn_dir, &pass_env(), now()),
            Err(Skip::NoActivation)
        );
    }

    #[test]
    fn kill_switch_short_circuits() {
        let e = learn_env();
        activate(&e.learn_dir);
        let env = EnvGuards {
            kill_switch: true,
            worker: false,
        };
        assert_eq!(
            should_spawn(&enabled_config(), &e.learn_dir, &env, now()),
            Err(Skip::KillSwitch)
        );
    }

    #[test]
    fn worker_env_short_circuits_recursion() {
        let e = learn_env();
        activate(&e.learn_dir);
        let env = EnvGuards {
            kill_switch: false,
            worker: true,
        };
        assert_eq!(
            should_spawn(&enabled_config(), &e.learn_dir, &env, now()),
            Err(Skip::Recursion)
        );
    }

    #[test]
    fn paused_after_failures_short_circuits() {
        let e = learn_env();
        activate(&e.learn_dir);
        state::record_failure_at(&e.learn_dir);
        state::record_failure_at(&e.learn_dir); // 2 → paused
        assert!(state::paused_at(&e.learn_dir));
        assert_eq!(
            should_spawn(&enabled_config(), &e.learn_dir, &pass_env(), now()),
            Err(Skip::Paused)
        );
    }

    #[test]
    fn fresh_scan_stamp_short_circuits() {
        let e = learn_env();
        activate(&e.learn_dir);
        // A scan stamp written "now" is inside the 15-min debounce.
        state::write_stamp(&e.learn_dir.join("scan-stamp")).unwrap();
        assert_eq!(
            should_spawn(
                &enabled_config(),
                &e.learn_dir,
                &pass_env(),
                SystemTime::now()
            ),
            Err(Skip::ScanDebounced)
        );
    }

    #[test]
    fn scan_due_but_fresh_spend_stamp_without_hint_short_circuits() {
        let e = learn_env();
        activate(&e.learn_dir);
        stale_stamp(&e.learn_dir, "scan-stamp"); // scan due
                                                 // Spend stamp written "now" → not past the 6h interval, and no hint.
        state::write_stamp(&e.learn_dir.join("spend-stamp")).unwrap();
        assert_eq!(
            should_spawn(
                &enabled_config(),
                &e.learn_dir,
                &pass_env(),
                SystemTime::now()
            ),
            Err(Skip::NotDue)
        );
    }

    #[test]
    fn hint_bypasses_spend_stamp_but_not_scan_debounce() {
        let e = learn_env();
        activate(&e.learn_dir);
        write_hint_at(&e.learn_dir, "claude", "sess-1").unwrap();

        // With a fresh scan stamp, the hint must NOT rescue us — scan debounce
        // is checked before the spend/hint guard.
        state::write_stamp(&e.learn_dir.join("scan-stamp")).unwrap();
        state::write_stamp(&e.learn_dir.join("spend-stamp")).unwrap();
        assert_eq!(
            should_spawn(
                &enabled_config(),
                &e.learn_dir,
                &pass_env(),
                SystemTime::now()
            ),
            Err(Skip::ScanDebounced),
        );

        // Scan due + fresh spend stamp, but the hint exists → spawn.
        stale_stamp(&e.learn_dir, "scan-stamp");
        state::write_stamp(&e.learn_dir.join("spend-stamp")).unwrap();
        assert_eq!(
            should_spawn(
                &enabled_config(),
                &e.learn_dir,
                &pass_env(),
                SystemTime::now()
            ),
            Ok(()),
        );
    }

    #[test]
    fn all_guards_pass_with_no_stamps() {
        let e = learn_env();
        activate(&e.learn_dir);
        // No stamps at all → both stamps are "due" (never run).
        assert_eq!(
            should_spawn(&enabled_config(), &e.learn_dir, &pass_env(), now()),
            Ok(())
        );
    }

    // --- eligibility hints -------------------------------------------------

    #[test]
    fn write_and_read_hints_round_trip() {
        let e = learn_env();
        assert!(!has_hint(&e.learn_dir), "no hints yet");

        write_hint_at(&e.learn_dir, "claude", "abc-123-def").unwrap();
        write_hint_at(&e.learn_dir, "codex", "xyz").unwrap();
        assert!(has_hint(&e.learn_dir));

        let (hooked, paths) = read_hints(&e.learn_dir, &BTreeSet::new());
        // uuid-style session id keeps its own hyphens (split on the FIRST '-').
        assert!(hooked.contains("abc-123-def"), "got {hooked:?}");
        assert!(hooked.contains("xyz"), "got {hooked:?}");
        assert_eq!(paths.len(), 2);

        // Deleting the returned paths clears the hint dir.
        for p in &paths {
            std::fs::remove_file(p).unwrap();
        }
        assert!(!has_hint(&e.learn_dir));
    }

    #[test]
    fn read_hints_seeds_from_the_existing_hooked_set() {
        let e = learn_env();
        let seed: BTreeSet<String> = ["already-hooked".to_string()].into_iter().collect();
        let (hooked, _paths) = read_hints(&e.learn_dir, &seed);
        assert!(hooked.contains("already-hooked"));
    }

    /// C13 backstop: a hint older than `HINT_TTL` is swept (deleted) on read and
    /// never surfaced, whatever the run outcome; a fresh hint survives and is
    /// scheduled for the normal success-path deletion. This bounds the lifetime
    /// of a never-harvestable hint (out of scope, or transcript rotated away).
    #[test]
    fn expired_hints_are_swept_on_read_and_fresh_ones_survive() {
        let e = learn_env();
        write_hint_at(&e.learn_dir, "claude", "old-session").unwrap();
        write_hint_at(&e.learn_dir, "claude", "fresh-session").unwrap();

        // Backdate the "old" hint's mtime well past the TTL.
        let old = eligible_dir(&e.learn_dir).join("claude-old-session");
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - HINT_TTL - Duration::from_secs(60))
            .unwrap();

        let (hooked, paths) = read_hints(&e.learn_dir, &BTreeSet::new());
        assert!(!old.exists(), "an expired hint is swept on read");
        assert!(
            !hooked.contains("old-session"),
            "an expired hint is not surfaced to the readers"
        );
        assert!(hooked.contains("fresh-session"), "a fresh hint survives");
        assert_eq!(
            paths.len(),
            1,
            "only the fresh hint is scheduled for success-path deletion"
        );
    }

    #[test]
    fn hint_name_is_confined_to_the_eligible_dir() {
        // A malicious session id with path separators cannot escape.
        let name = hint_name("claude", "../../etc/passwd");
        assert!(!name.contains('/'), "path separators stripped: {name}");
        assert!(name.starts_with("claude-"));
    }

    // --- spawn seam: real detached double-spawn against a stub -------------

    #[test]
    #[cfg(unix)]
    fn spawn_worker_detaches_and_runs_the_binary() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Instant;

        let tmp = tempfile::tempdir().unwrap();
        let sentinel = tmp.path().join("ran");
        let stub = tmp.path().join("stub.sh");
        // The stub ignores its args (`harvest --ambient`), records that it ran,
        // and exits immediately — a short-lived process that init reaps, so this
        // test spawns nothing that can leak or hang.
        std::fs::write(
            &stub,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = tmp.path().join("worker.log");
        spawn_worker(&stub, &log).expect("spawn must succeed");

        // The intermediate was already reaped by spawn_worker; the detached
        // grandchild runs the stub and reparents to init. Poll (bounded) for its
        // side effect.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sentinel.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            sentinel.exists(),
            "the detached worker must have executed the binary"
        );
    }
}
