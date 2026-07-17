//! Per-machine learning state: identity, activation ack, throttle stamps, and
//! the consecutive-failure counter.
//!
//! Everything here lives under [`learn_dir`] (`state_dir()/learn/`) — never
//! synced (`load sync` moves the config dir under git; this must not follow).
//! Every stateful function takes an explicit path (the `*_at` seam): the
//! identity/activation/failure-counter functions take the learn dir itself
//! (`dir: &Path`, joining their own fixed filename inside it) and the stamp
//! functions take the exact stamp file path (there are two: `scan-stamp` and
//! `spend-stamp`, on different debounce intervals). Callers pass
//! [`learn_dir`]'s result (or a file inside it); unit tests pass a
//! `tempdir()` so they never touch the real per-machine state dir.
//!
//! `read_stamp`/`is_due` delegate to the private `update.rs` helpers of the
//! same shape (same on-disk unix-seconds format, same due-when-elapsed-or-
//! backwards semantics) rather than duplicating them. `write_stamp` does
//! NOT delegate: `update.rs`'s version takes an explicit `at: SystemTime` and
//! writes via a raw `std::fs::write`, while this module's stamp files must go
//! through [`crate::writer::atomic_write`] (same durability guarantee as
//! every other state-dir store) and always stamp "now" — a genuine signature
//! and behavior mismatch, so it gets its own small implementation instead of
//! a bad-fit re-export.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Where per-machine learning state lives: `state_dir()/learn/`. `None` only
/// when no home directory can be resolved (same condition [`crate::config::state_dir`]
/// returns `None` for).
pub fn learn_dir() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("learn"))
}

/// `n_bytes` of randomness as a lowercase hex string (`2 * n_bytes` chars).
///
/// Reads `/dev/urandom` directly on unix (no new dependency: plain
/// `std::fs`/`std::io`, no `libc`/`rand` needed for this). Anywhere that
/// fails (permission, non-unix, sandboxed environments) falls back to a
/// `sha256(pid | nanos | hostname)` digest truncated to `n_bytes` — not
/// cryptographically strong, but this only seeds a per-machine identity
/// label, not a security credential.
pub fn random_hex(n_bytes: usize) -> String {
    #[cfg(unix)]
    {
        if let Some(bytes) = read_urandom(n_bytes) {
            return to_hex(&bytes);
        }
    }
    to_hex(&fallback_bytes(n_bytes))
}

#[cfg(unix)]
fn read_urandom(n_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    let mut buf = vec![0u8; n_bytes];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// `sha256(pid | nanos | hostname)`, truncated to `n_bytes` (or padded by
/// re-hashing if a caller ever asks for more than a sha256 digest's 32
/// bytes — not exercised today, but keeps this total rather than panicking).
fn fallback_bytes(n_bytes: usize) -> Vec<u8> {
    use sha2::{Digest as _, Sha256};
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    let mut out = Vec::with_capacity(n_bytes);
    let mut counter: u32 = 0;
    while out.len() < n_bytes {
        let seed = format!("{pid}|{nanos}|{hostname}|{counter}");
        out.extend_from_slice(&Sha256::digest(seed.as_bytes()));
        counter += 1;
    }
    out.truncate(n_bytes);
    out
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// File name (inside the learn dir) holding the persistent machine id.
const MACHINE_ID_FILE: &str = "machine-id";

/// Read the per-machine id from `dir/machine-id`, minting and persisting a
/// fresh 16-byte-hex (32 char) id the first time it's asked for. Stable
/// across every subsequent call (same `dir`) once minted.
///
/// Only "file absent" (and an empty file, as self-heal) mints. Any OTHER
/// read failure on an existing file (permissions, transient I/O) propagates
/// as `Err` — journals are named `journal-<machine-id>.jsonl` and
/// `activation.json` captures the id once, so silently reminting over a
/// momentarily-unreadable file would fork the persistent identity and
/// orphan both. (`atomic_write` needs only PARENT-dir write access, so
/// read-fails/write-succeeds is a real scenario, not a hypothetical.)
pub fn machine_id_at(dir: &Path) -> io::Result<String> {
    let path = dir.join(MACHINE_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(existing) => {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
            // Empty file: self-heal by minting below.
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let id = random_hex(16);
    crate::writer::atomic_write(&path, &id).map_err(io::Error::other)?;
    Ok(id)
}

/// File name (inside the learn dir) holding the activation ack.
const ACTIVATION_FILE: &str = "activation.json";

/// The per-machine activation ack: `load learn on` run on this machine.
/// `hostname` is display metadata only (hostnames rename and collide —
/// `machine_id` is the durable identity); `activated_at` is an RFC 3339 UTC
/// timestamp string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    pub machine_id: String,
    pub hostname: String,
    pub activated_at: String,
}

/// Read the activation ack, or `None` if this machine was never activated
/// (or the file is unreadable/corrupt — absence and corruption both just
/// mean "not activated"; the ack is re-written wholesale by the next
/// `load learn on`, nothing here needs recovery machinery).
pub fn read_activation_at(dir: &Path) -> Option<Activation> {
    let text = std::fs::read_to_string(dir.join(ACTIVATION_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether ambient learning is active on THIS machine: the synced `[learn]`
/// intent flag is on AND this machine carries an activation ack (`load learn on`
/// was run here). This is the single gate the passive hook bootstrap
/// ([`crate::adapters::bootstrap_hook_registrations`]) consults to decide
/// whether to (re)register learn-purpose hooks. It must be `false` after
/// `load learn off` (which clears the ack locally and, once synced, the flag)
/// so a routine refresh can never re-add the learning hooks. Cheap: one
/// config-field read plus, only when the flag is on, a single `stat` of the
/// activation file.
pub fn learn_active(cfg: &crate::config::Config) -> bool {
    cfg.learn.enabled
        && learn_dir()
            .map(|dir| read_activation_at(&dir).is_some())
            .unwrap_or(false)
}

/// Write the activation ack, replacing any prior one.
pub fn write_activation_at(dir: &Path, a: &Activation) -> io::Result<()> {
    let path = dir.join(ACTIVATION_FILE);
    let body = serde_json::to_string_pretty(a).map_err(io::Error::other)?;
    crate::writer::atomic_write(&path, &body).map_err(io::Error::other)
}

/// Remove the activation ack (`load learn off`). Missing file is not an
/// error — deactivating an already-inactive machine is a no-op.
pub fn remove_activation_at(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(dir.join(ACTIVATION_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read a throttle stamp (unix-seconds text file), or `None` if unset/
/// unreadable. Delegates to `update.rs`'s identical parsing — same on-disk
/// format, one implementation.
pub fn read_stamp(path: &Path) -> Option<SystemTime> {
    crate::update::read_stamp(path)
}

/// Record "now" at `path`, via [`crate::writer::atomic_write`] (durability
/// parity with every other state-dir store — same-directory temp file, fsync,
/// rename). Does NOT delegate to `update.rs`'s `write_stamp`: that helper
/// takes an explicit `at` and writes with a plain `std::fs::write`, a real
/// shape mismatch from this signature, not just a naming difference.
pub fn write_stamp(path: &Path) -> io::Result<()> {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::writer::atomic_write(path, &secs.to_string()).map_err(io::Error::other)
}

/// Whether a throttled action is due: never run, `interval` has elapsed
/// since `last`, or the clock went backwards (`now` < `last` also counts as
/// due, so a clock step never wedges triggering forever). Delegates to
/// `update.rs`'s identical semantics.
pub fn is_due(last: Option<SystemTime>, now: SystemTime, interval: Duration) -> bool {
    crate::update::is_due(last, now, interval)
}

/// File name (inside the learn dir) holding the consecutive-failure count.
const FAILURES_FILE: &str = "failures.json";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct FailuresFile {
    consecutive: u32,
}

fn read_failures(path: &Path) -> FailuresFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort: a failure to persist the counter must not itself crash the
/// worker's failure-handling path, so this and [`reset_failures_at`] swallow
/// write errors rather than propagating them.
fn write_failures(path: &Path, f: FailuresFile) {
    if let Ok(body) = serde_json::to_string_pretty(&f) {
        let _ = crate::writer::atomic_write(path, &body);
    }
}

/// Record one more consecutive failure (a failed harvest run, including a
/// reclaimed stale lock — see the design doc) and return the new count.
pub fn record_failure_at(dir: &Path) -> u32 {
    let path = dir.join(FAILURES_FILE);
    let mut f = read_failures(&path);
    f.consecutive = f.consecutive.saturating_add(1);
    write_failures(&path, f);
    f.consecutive
}

/// Clear the consecutive-failure count (a successful manual `load harvest`
/// or a fresh `load learn on` resets it).
pub fn reset_failures_at(dir: &Path) {
    write_failures(&dir.join(FAILURES_FILE), FailuresFile { consecutive: 0 });
}

/// Read the current consecutive-failure count.
///
/// Missing, unreadable, and malformed state all fail closed to zero, matching
/// [`paused_at`]. This is the read-only key used to decide whether an older run
/// log failure is still actionable.
pub fn consecutive_failures_at(dir: &Path) -> u32 {
    read_failures(&dir.join(FAILURES_FILE)).consecutive
}

/// Whether ambient triggering is paused: 2 or more consecutive failures.
pub fn paused_at(dir: &Path) -> bool {
    consecutive_failures_at(dir) >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- machine id ---------------------------------------------------------

    #[test]
    fn machine_id_mints_once_and_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = machine_id_at(dir.path()).unwrap();
        assert_eq!(first.len(), 32, "16 bytes hex-encoded is 32 chars");
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "must be hex: {first}"
        );

        let second = machine_id_at(dir.path()).unwrap();
        assert_eq!(first, second, "the id must not change across calls");

        // The file itself survives and holds exactly the returned id.
        let on_disk = std::fs::read_to_string(dir.path().join(MACHINE_ID_FILE)).unwrap();
        assert_eq!(on_disk.trim(), first);
    }

    /// A read failure that is NOT "file absent" (here: permissions) must
    /// propagate as `Err`, never silently mint a replacement id — journals
    /// are named by this id and `activation.json` captures it once, so a
    /// remint would orphan both. `atomic_write` only needs write access to
    /// the PARENT dir, so read-fails/write-succeeds is a real scenario the
    /// mint path would happily clobber through.
    #[test]
    #[cfg(unix)]
    fn machine_id_unreadable_file_errors_instead_of_reminting() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_ID_FILE);
        std::fs::write(&path, "0123456789abcdef0123456789abcdef").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let err = machine_id_at(dir.path()).expect_err("unreadable id file must error, not remint");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // The existing identity must survive untouched.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0123456789abcdef0123456789abcdef",
            "a failed read must not clobber the persisted id"
        );
    }

    #[test]
    fn machine_id_differs_across_machines() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            machine_id_at(a.path()).unwrap(),
            machine_id_at(b.path()).unwrap(),
            "two freshly-minted ids must not collide"
        );
    }

    #[test]
    fn random_hex_respects_length() {
        assert_eq!(random_hex(16).len(), 32);
        assert_eq!(random_hex(4).len(), 8);
    }

    // --- activation ----------------------------------------------------------

    #[test]
    fn activation_round_trips_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read_activation_at(dir.path()).is_none(),
            "no activation yet"
        );

        let a = Activation {
            machine_id: "deadbeef".to_string(),
            hostname: "example.local".to_string(),
            activated_at: "2026-07-10T21:00:00Z".to_string(),
        };
        write_activation_at(dir.path(), &a).unwrap();
        assert_eq!(read_activation_at(dir.path()), Some(a));

        remove_activation_at(dir.path()).unwrap();
        assert!(read_activation_at(dir.path()).is_none());

        // Removing an already-absent activation is a no-op, not an error.
        remove_activation_at(dir.path()).unwrap();
    }

    // --- stamps / is_due -------------------------------------------------------

    #[test]
    fn is_due_never_elapsed_backwards() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let interval = Duration::from_secs(100);
        assert!(is_due(None, now, interval), "never run → due");
        assert!(
            is_due(Some(now - Duration::from_secs(150)), now, interval),
            "elapsed past the interval → due"
        );
        assert!(
            !is_due(Some(now - Duration::from_secs(50)), now, interval),
            "within the interval → not due"
        );
        assert!(
            is_due(Some(now + Duration::from_secs(50)), now, interval),
            "clock went backwards → due (never stuck)"
        );
    }

    #[test]
    fn stamp_round_trips_and_feeds_is_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan-stamp");
        assert_eq!(read_stamp(&path), None, "no stamp yet");

        let before = SystemTime::now();
        write_stamp(&path).unwrap();
        let after = SystemTime::now();

        let read_back = read_stamp(&path).expect("stamp must be readable after write");
        // Unix-seconds granularity: allow the read-back to land within the
        // [before, after] window once truncated to whole seconds.
        assert!(read_back + Duration::from_secs(1) >= before);
        assert!(read_back <= after + Duration::from_secs(1));

        assert!(
            !is_due(Some(read_back), after, Duration::from_secs(3600)),
            "a stamp just written must not be immediately due for a long interval"
        );
    }

    // --- failure counter / pause -------------------------------------------

    #[test]
    fn two_consecutive_failures_pause_reset_clears() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!paused_at(dir.path()), "no failures yet → not paused");

        let first = record_failure_at(dir.path());
        assert_eq!(first, 1);
        assert_eq!(consecutive_failures_at(dir.path()), 1);
        assert!(!paused_at(dir.path()), "one failure → not yet paused");

        let second = record_failure_at(dir.path());
        assert_eq!(second, 2);
        assert!(paused_at(dir.path()), "two consecutive failures → paused");

        // A third failure keeps it paused (and keeps counting).
        let third = record_failure_at(dir.path());
        assert_eq!(third, 3);
        assert!(paused_at(dir.path()));

        reset_failures_at(dir.path());
        assert_eq!(consecutive_failures_at(dir.path()), 0);
        assert!(!paused_at(dir.path()), "reset clears the pause");
    }

    #[test]
    fn consecutive_failures_is_zero_for_missing_or_malformed_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(consecutive_failures_at(dir.path()), 0);

        std::fs::write(dir.path().join(FAILURES_FILE), "not json").unwrap();
        assert_eq!(consecutive_failures_at(dir.path()), 0);
    }
}
