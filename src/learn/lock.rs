//! Single-worker lock for the ambient harvest worker: an `O_EXCL` lock file
//! carrying a **fencing token**, plus reclaim of a stale (crashed or wedged)
//! holder.
//!
//! Lives at `<learn dir>/lock.json` (`state_dir()/learn/lock.json` in
//! production, a `tempdir()` in tests — the same `*_at(dir)` seam as
//! [`crate::learn::state`]). On disk it is `{pid, started_at, token}`.
//!
//! ## Why a fencing token (not just a lock file)
//!
//! The token is minted at acquisition, **before any other side effect**, and
//! the worker re-checks [`LockGuard::still_held`] before every side effect it
//! performs — the scan stamp, the spend stamp, the agent-CLI spawn, each
//! journal/evidence/watermark/log write. A worker that was suspended, had its
//! lock reclaimed by a later run, and then resumed finds a *different* token
//! on disk and aborts before spending or writing. That is why `still_held()`
//! is deliberately cheap: it is on the hot path of every write. The watermark
//! store is additionally monotonic (per-file `max`), so even a missed
//! `still_held()` check cannot regress into double-harvesting.
//!
//! ## Reclaim, and why the file-level race is safe
//!
//! [`acquire_at`] creates the lock with
//! `OpenOptions::new().write(true).create_new(true)` (`O_EXCL`). If the file
//! already exists it reads the holder and decides **stale** iff:
//! - the holder pid is dead (`kill(pid, 0)` fails with `ESRCH`, unix only), or
//! - the lock is older than `2 * deadline` (`now_unix - started_at`), or
//! - the file cannot be parsed (a corrupt or half-written lock — see below).
//!
//! A stale lock is removed and `create_new` is retried **exactly once**:
//! success is [`Acquire::Reclaimed`], and losing the retry (someone else won
//! the `O_EXCL` create) is [`Acquire::Busy`] — never a panic or a loop. A live
//! holder is [`Acquire::Busy`] with no removal.
//!
//! Two racing reclaimers can still momentarily clobber each other's fresh
//! lock (a blind `remove_file` may delete a lock a competitor just created).
//! That is intentional and harmless: mutual exclusion for *side effects* is
//! enforced by the fencing token, not by the file race. A reclaimer whose
//! fresh lock gets clobbered sees a foreign token on its very next
//! `still_held()` and aborts before doing any work. The lock file only has to
//! be good enough to keep the common case single-writer; the token makes the
//! uncommon case safe.
//!
//! ## Corrupt lock → reclaimable (deliberate)
//!
//! A lock file that exists but does not parse is treated as **stale**, not
//! `Busy`. `create_new` reserves the path before the body is written, so a
//! crash between the two leaves a valid-but-empty file holding the slot; and a
//! `Busy`-forever reading of a corrupt lock would silently brick ambient
//! learning until someone deleted the file by hand. Reclaiming it is safe for
//! the same reason the file race is: any still-live original holder fences
//! itself out on its next `still_held()`.
//!
//! ## pid reuse
//!
//! A dead holder's pid can be recycled to an unrelated live process, so
//! `kill(pid, 0)` would report it alive and the pid check alone would keep the
//! stale lock forever. The `2 * deadline` age check is the escape hatch: past
//! that age the lock is reclaimed regardless of what the pid probe says.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// File name (inside the learn dir) that holds the lock.
const LOCK_FILE: &str = "lock.json";

/// The on-disk lock body. Private: callers only ever see a [`LockGuard`].
#[derive(Debug, Serialize, Deserialize)]
struct Holder {
    /// The acquiring process id (`std::process::id()`), used for the unix
    /// liveness probe.
    pid: i32,
    /// Unix seconds when the lock was acquired, used for the age check.
    started_at: i64,
    /// The fencing token — the field every side effect re-checks.
    token: String,
}

/// A held lock plus the fencing token minted when it was acquired. Dropping a
/// guard **does not** release the lock (there is no `Drop` impl): a crashed
/// worker leaves its lock behind on purpose, to be reclaimed by staleness at
/// the next run. Release is the explicit, consuming [`LockGuard::release`].
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    /// The fencing token this guard holds. Compared against the on-disk
    /// token by [`LockGuard::still_held`].
    pub token: String,
}

/// Outcome of an acquisition attempt.
#[derive(Debug)]
pub enum Acquire {
    /// The lock was free and is now held.
    Held(LockGuard),
    /// A stale holder was reclaimed. The caller counts this as one failure
    /// (two consecutive failures pause ambient triggering — see the design
    /// doc), so it is a distinct variant from [`Acquire::Held`].
    Reclaimed(LockGuard),
    /// A live holder owns the lock; nothing was changed.
    Busy,
}

/// Try to acquire the lock in `dir`, reclaiming a stale holder if present.
///
/// `deadline` is the worker's hard wall-clock deadline; a lock older than
/// `2 * deadline` is considered stale. `now_unix` is the current time in unix
/// seconds (an explicit parameter so tests are deterministic).
pub fn acquire_at(dir: &Path, deadline: Duration, now_unix: i64) -> io::Result<Acquire> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOCK_FILE);

    match try_create(&path, now_unix) {
        Ok(guard) => Ok(Acquire::Held(guard)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if !is_stale(&path, deadline, now_unix) {
                return Ok(Acquire::Busy);
            }
            // Stale: remove and retry create_new exactly once. A concurrent
            // reclaimer may have already removed it (NotFound) or already
            // recreated it (the retry fails with AlreadyExists → Busy). Never
            // loop.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            match try_create(&path, now_unix) {
                Ok(guard) => Ok(Acquire::Reclaimed(guard)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Acquire::Busy),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Mint a fresh token and create the lock with `O_EXCL`. On `AlreadyExists`
/// the error propagates to the caller, which decides stale-vs-busy.
fn try_create(path: &Path, now_unix: i64) -> io::Result<LockGuard> {
    let token = super::state::random_hex(16);
    let holder = Holder {
        pid: std::process::id() as i32,
        started_at: now_unix,
        token: token.clone(),
    };
    let body = serde_json::to_string(&holder).map_err(io::Error::other)?;

    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all().ok();

    Ok(LockGuard {
        path: path.to_path_buf(),
        token,
    })
}

/// Whether the lock currently at `path` is reclaimable. A file that cannot be
/// read or parsed counts as stale (see the module docs).
fn is_stale(path: &Path, deadline: Duration, now_unix: i64) -> bool {
    match read_holder(path) {
        None => true,
        Some(h) => pid_is_dead(h.pid) || age_exceeded(h.started_at, deadline, now_unix),
    }
}

/// Read and parse the lock body, or `None` if it is absent, unreadable, or
/// malformed.
fn read_holder(path: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the lock is older than `2 * deadline`.
fn age_exceeded(started_at: i64, deadline: Duration, now_unix: i64) -> bool {
    let threshold = 2i64.saturating_mul(deadline.as_secs() as i64);
    now_unix.saturating_sub(started_at) > threshold
}

/// Whether `pid` names no live process. Only a definitive "no such process"
/// (`ESRCH`) counts as dead; `kill` succeeding, or failing with `EPERM` (the
/// process exists but we may not signal it), or any other errno, is treated
/// as alive so we never reclaim a holder we merely failed to probe.
#[cfg(unix)]
fn pid_is_dead(pid: i32) -> bool {
    // SAFETY: kill with signal 0 performs permission/existence checks only —
    // it delivers no signal and mutates no process state.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return false;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// No portable liveness probe off unix: staleness relies solely on the age
/// check, which degrades gracefully rather than reclaiming a possibly-live
/// holder.
#[cfg(not(unix))]
fn pid_is_dead(_pid: i32) -> bool {
    false
}

impl LockGuard {
    /// Whether this guard still owns the lock: re-read `lock.json`, parse it,
    /// and compare the on-disk token against this guard's token. A missing,
    /// unreadable, or foreign-token lock is `false`. Cheap by design — the
    /// worker calls this before every side effect.
    pub fn still_held(&self) -> bool {
        match read_holder(&self.path) {
            Some(h) => h.token == self.token,
            None => false,
        }
    }

    /// Remove the lock file, but only if this guard still owns it. A reclaimer
    /// may have replaced the lock with its own; deleting that foreign lock
    /// would let a third worker in. There is an unavoidable micro-race between
    /// the check and the delete, but a guard is only reclaimed once it is
    /// stale, and the fencing token — not this delete — is what actually
    /// guards side effects.
    pub fn release(self) {
        if self.still_held() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const NOW: i64 = 1_000_000;
    const DEADLINE: Duration = Duration::from_secs(300); // 5 min

    /// Plant a raw lock file with an exact `{pid, started_at, token}` so a
    /// test can control staleness inputs directly (the real acquire path
    /// always mints its own).
    fn plant(dir: &Path, pid: i32, started_at: i64, token: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let body = format!("{{\"pid\":{pid},\"started_at\":{started_at},\"token\":\"{token}\"}}");
        std::fs::write(dir.join(LOCK_FILE), body).unwrap();
    }

    #[test]
    fn fresh_acquire_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let g = match acquire_at(dir.path(), DEADLINE, NOW).unwrap() {
            Acquire::Held(g) => g,
            other => panic!("expected Held, got {other:?}"),
        };
        assert_eq!(g.token.len(), 32, "16 bytes hex-encoded is 32 chars");
        assert!(
            g.token.chars().all(|c| c.is_ascii_hexdigit()),
            "token must be hex: {}",
            g.token
        );
        assert!(dir.path().join(LOCK_FILE).exists(), "lock file must exist");
        assert!(g.still_held(), "a fresh guard holds its own token");
    }

    #[test]
    fn second_acquire_against_live_holder_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        // First acquire writes a lock with OUR (live) pid at started_at=NOW.
        let g = match acquire_at(dir.path(), DEADLINE, NOW).unwrap() {
            Acquire::Held(g) => g,
            other => panic!("expected Held, got {other:?}"),
        };
        // Same instant → not stale by age; our pid is live → Busy.
        let acq = acquire_at(dir.path(), DEADLINE, NOW).unwrap();
        assert!(
            matches!(acq, Acquire::Busy),
            "a live holder must yield Busy, got {acq:?}"
        );
        assert!(g.still_held(), "the original guard is untouched");
    }

    #[test]
    fn stale_by_age_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        // Plant via the real path so the holder carries OUR live pid; then
        // acquire far past 2*deadline later. Only age makes it stale, which
        // proves the age escape hatch fires even when the pid is alive (the
        // pid-reuse case).
        let _leaked = match acquire_at(dir.path(), DEADLINE, NOW).unwrap() {
            Acquire::Held(g) => g, // dropped without release → file stays
            other => panic!("expected Held, got {other:?}"),
        };
        let future = NOW + 2 * DEADLINE.as_secs() as i64 + 1;
        let acq = acquire_at(dir.path(), DEADLINE, future).unwrap();
        let g = match acq {
            Acquire::Reclaimed(g) => g,
            other => panic!("expected Reclaimed, got {other:?}"),
        };
        assert!(g.still_held(), "the reclaimer holds its own fresh token");
    }

    #[test]
    #[cfg(unix)]
    fn dead_pid_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        // A reaped child's pid is dead: spawn, wait (reaps the zombie), reuse
        // its id. Reuse in the tiny window before the probe is vanishingly
        // unlikely inside a single-threaded test.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let dead_pid = child.id() as i32;
        child.wait().unwrap();

        // Fresh age so ONLY the dead pid makes it stale.
        plant(
            dir.path(),
            dead_pid,
            NOW,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let acq = acquire_at(dir.path(), DEADLINE, NOW).unwrap();
        assert!(
            matches!(acq, Acquire::Reclaimed(_)),
            "a dead-pid holder must be Reclaimed, got {acq:?}"
        );
    }

    #[test]
    fn corrupt_lock_is_reclaimable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(LOCK_FILE), "{ not json").unwrap();
        // A lock we cannot parse must be reclaim-eligible, never permanently
        // Busy — a corrupt lock that read as Busy forever would silently
        // brick ambient learning.
        let acq = acquire_at(dir.path(), DEADLINE, NOW).unwrap();
        assert!(
            matches!(acq, Acquire::Reclaimed(_)),
            "a corrupt lock must be reclaimable, got {acq:?}"
        );
    }

    #[test]
    fn external_overwrite_defeats_still_held_and_release_spares_foreign_lock() {
        let dir = tempfile::tempdir().unwrap();
        let g = match acquire_at(dir.path(), DEADLINE, NOW).unwrap() {
            Acquire::Held(g) => g,
            other => panic!("expected Held, got {other:?}"),
        };
        assert!(g.still_held());

        // An external process reclaims and rewrites the lock with ITS token.
        let foreign = "ffffffffffffffffffffffffffffffff";
        plant(dir.path(), 999_999, NOW, foreign);

        assert!(
            !g.still_held(),
            "a different token on disk means we no longer hold the lock"
        );

        // release() must NOT delete a lock it no longer owns.
        g.release();
        let on_disk = std::fs::read_to_string(dir.path().join(LOCK_FILE)).unwrap();
        assert!(
            on_disk.contains(foreign),
            "release must leave the foreign lock intact: {on_disk}"
        );
    }

    #[test]
    fn release_removes_our_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let g = match acquire_at(dir.path(), DEADLINE, NOW).unwrap() {
            Acquire::Held(g) => g,
            other => panic!("expected Held, got {other:?}"),
        };
        let path = dir.path().join(LOCK_FILE);
        assert!(path.exists());

        g.release();
        assert!(!path.exists(), "release deletes a lock we still own");

        // The slot is free again.
        assert!(matches!(
            acquire_at(dir.path(), DEADLINE, NOW).unwrap(),
            Acquire::Held(_)
        ));
    }
}
