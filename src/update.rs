//! Self-update via [`axoupdater`] (cargo-dist's own updater), plus a throttled,
//! best-effort "a newer loadout is available" nudge for `load run`.
//!
//! axoupdater works off the *install receipt* the cargo-dist shell installer
//! writes to the config dir (`~/.config/loadout/`). A binary installed any other
//! way (`cargo install`, a package manager, hand-copied) has no receipt, so
//! self-update degrades gracefully to [`Outcome::NotManaged`] — loadout never
//! pretends to update something it can't.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The app name axoupdater uses to locate the install receipt and releases.
const APP: &str = "loadout";

/// Opt out of the `load run` update nudge (any value disables it).
pub const NUDGE_OPT_OUT_ENV: &str = "LOADOUT_NO_UPDATE_CHECK";

/// How often the `run` nudge re-checks for a newer release.
const NUDGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bound the nudge's network check so a slow release host can't stall a launch.
const NUDGE_TIMEOUT: Duration = Duration::from_millis(1500);

/// What [`perform`] did, or why it couldn't.
pub enum Outcome {
    /// The binary was replaced. `from` is the prior version if the receipt knew it.
    Updated { from: Option<String>, to: String },
    /// Already on the newest release — nothing to do.
    AlreadyCurrent,
    /// `--check` only: a newer release exists (and was not installed).
    UpdateAvailable,
    /// No install receipt — this binary wasn't installed via the loadout installer,
    /// so it can't self-update.
    NotManaged,
}

/// Run the update (or, with `check_only`, just report whether one exists).
/// Network- and filesystem-heavy; backs the `load update` subcommand.
pub fn perform(check_only: bool) -> crate::Result<Outcome> {
    use axoupdater::AxoUpdater;
    let mut updater = AxoUpdater::new_for(APP);
    // No receipt ⇒ not an installer-based install ⇒ can't self-update.
    if updater.load_receipt().is_err() {
        return Ok(Outcome::NotManaged);
    }
    if check_only {
        return Ok(if updater.is_update_needed_sync()? {
            Outcome::UpdateAvailable
        } else {
            Outcome::AlreadyCurrent
        });
    }
    match updater.run_sync()? {
        Some(result) => Ok(Outcome::Updated {
            from: result.old_version.map(|v| v.to_string()),
            to: result.new_version.to_string(),
        }),
        None => Ok(Outcome::AlreadyCurrent),
    }
}

/// Best-effort "update available" hint for `load run`. Returns the detail line
/// to show (the caller renders it in its own step style), or `None` to stay
/// quiet. It never errors and is cheap on the common path: gated on a TTY and the
/// opt-out env, throttled to once per [`NUDGE_INTERVAL`] via an on-disk stamp, and
/// the actual network check is time-bounded so it can't slow a launch.
pub fn nudge_detail() -> Option<String> {
    if std::env::var_os(NUDGE_OPT_OUT_ENV).is_some() {
        return None;
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let stamp = stamp_path()?;
    if !is_due(read_stamp(&stamp), SystemTime::now(), NUDGE_INTERVAL) {
        return None;
    }
    // Stamp once per interval regardless of the outcome, so a flaky or offline
    // check never nags and never repeatedly delays launches.
    let available = check_available_bounded();
    let _ = write_stamp(&stamp, SystemTime::now());
    if available == Some(true) {
        Some("a newer loadout is available — run `load update`".to_string())
    } else {
        None
    }
}

/// Where the once-a-day check timestamp lives (alongside the global config).
fn stamp_path() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("update-check"))
}

/// Read the last-check time (unix seconds), or `None` if unset/unreadable.
///
/// `pub(crate)`: `learn::state::read_stamp` delegates to this exact parsing —
/// same on-disk format (unix-seconds text), kept in one place rather than
/// duplicated.
pub(crate) fn read_stamp(path: &Path) -> Option<SystemTime> {
    let secs: u64 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Record the last-check time (unix seconds), creating the dir if needed.
fn write_stamp(path: &Path, at: SystemTime) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(path, secs.to_string())
}

/// Whether a check is due: never checked, or `interval` has elapsed. A clock that
/// went backwards (`now` < `last`) also counts as due.
///
/// `pub(crate)`: `learn::state::is_due` delegates to this exact semantics
/// rather than duplicating it.
pub(crate) fn is_due(last: Option<SystemTime>, now: SystemTime, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t).map(|d| d >= interval).unwrap_or(true),
    }
}

/// The network check, bounded by [`NUDGE_TIMEOUT`]. Timeout, offline, or a missing
/// receipt all collapse to `None` ("don't nudge").
fn check_available_bounded() -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check_available());
    });
    rx.recv_timeout(NUDGE_TIMEOUT).ok().flatten()
}

/// `Some(true/false)` if we could ask the release host; `None` if there's no
/// receipt or the query failed.
fn check_available() -> Option<bool> {
    use axoupdater::AxoUpdater;
    let mut updater = AxoUpdater::new_for(APP);
    updater.load_receipt().ok()?;
    updater.is_update_needed_sync().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_when_never_checked_or_stale_not_when_recent() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let interval = Duration::from_secs(100);
        assert!(is_due(None, now, interval), "never checked → due");
        assert!(
            is_due(Some(now - Duration::from_secs(150)), now, interval),
            "older than the interval → due"
        );
        assert!(
            !is_due(Some(now - Duration::from_secs(50)), now, interval),
            "within the interval → not due"
        );
        assert!(
            is_due(Some(now + Duration::from_secs(50)), now, interval),
            "clock went backwards → due (don't get stuck)"
        );
    }

    #[test]
    fn stamp_round_trips_at_second_granularity() {
        let dir = tempfile::tempdir().unwrap();
        // Parent dir doesn't exist yet — write_stamp must create it.
        let path = dir.path().join("nested").join("update-check");
        assert_eq!(read_stamp(&path), None, "missing stamp reads as None");

        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_stamp(&path, at).unwrap();
        let back = read_stamp(&path).unwrap();
        assert_eq!(
            back.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_700_000_000
        );
    }
}
