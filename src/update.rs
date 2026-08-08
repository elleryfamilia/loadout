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

/// Opt out of the update nudge entirely (any value disables it). The hard
/// kill switch — it wins over any `[update] check` config.
pub const NUDGE_OPT_OUT_ENV: &str = "LOADOUT_NO_UPDATE_CHECK";

/// How often `check = "daily"` re-asks the release host.
const DAILY_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How often `check = "always"` refreshes its verdict. "Always" means the
/// nudge is *evaluated* on every command; the network refresh itself is capped
/// at once per this window (GitHub's unauthenticated API allows 60 calls/hour
/// per IP, shared with everything else on the machine) — within the window the
/// cached verdict still nudges, so a stale install is reminded on every run.
const ALWAYS_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// When the ambient "newer release available" check runs. Configured via
/// `[update] check = "always" | "daily" | "off"` in the global config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckMode {
    /// Evaluate on every loadout command (network-capped; see [`ALWAYS_INTERVAL`]).
    #[default]
    Always,
    /// At most one network check per day (the pre-0.20 behavior).
    Daily,
    /// Never check, never nudge.
    Off,
}

impl CheckMode {
    /// Parse the config string leniently. `None` (key absent) → the default;
    /// an unrecognized value (likely written by a newer loadout) degrades to
    /// `Daily` — still useful, minimal network — rather than erroring.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            None => Self::default(),
            Some(s) => match s.to_ascii_lowercase().as_str() {
                "always" => Self::Always,
                "daily" => Self::Daily,
                "off" | "never" => Self::Off,
                other => {
                    crate::warn_user!(
                        "unknown `[update] check` value '{other}' (expected always/daily/off); \
                         using daily"
                    );
                    Self::Daily
                }
            },
        }
    }
}

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
        Some(result) => {
            // A cached "available" verdict was computed against the binary we
            // just replaced — drop it so the next command doesn't nudge about
            // the update that already happened. (The version stamp in the
            // cache guards this too; deleting is belt and braces.)
            if let Some(cache) = cache_path() {
                let _ = std::fs::remove_file(cache);
            }
            Ok(Outcome::Updated {
                from: result.old_version.map(|v| v.to_string()),
                to: result.new_version.to_string(),
            })
        }
        None => Ok(Outcome::AlreadyCurrent),
    }
}

/// Best-effort "update available" hint. Returns the detail line to show (the
/// caller renders it in its own step style), or `None` to stay quiet.
///
/// **Never touches the network inline.** The nudge only reads the on-disk
/// verdict cache; when the verdict is stale per [`CheckMode`], it spawns a
/// tiny detached `load update --refresh-cache` that performs the (unbounded)
/// check and writes the verdict for the *next* command. So a launch is never
/// delayed and a slow release host can never suppress the nudge — the only
/// cost is that a brand-new release shows up at most one command late.
/// Within the refresh window a cached "available" verdict nudges on every
/// command, so a stale install is reminded until it updates.
pub fn nudge_detail(mode: CheckMode) -> Option<String> {
    if mode == CheckMode::Off {
        return None;
    }
    if std::env::var_os(NUDGE_OPT_OUT_ENV).is_some() {
        return None;
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let path = cache_path()?;
    let (nudge, refresh) = decide(mode, read_cache(&path).as_ref(), SystemTime::now());
    if refresh {
        // Stamp before spawning so a burst of commands starts ONE refresher,
        // not a swarm: the stamp closes the window immediately, and the child
        // overwrites it with the real verdict. `Failed` is the honest
        // placeholder ("no verdict"), and doubles as the retry backoff if the
        // child dies before writing.
        let _ = write_cache(
            &path,
            &CheckCache {
                at: SystemTime::now(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                verdict: Verdict::Failed,
            },
        );
        spawn_refresher();
    }
    nudge.then(|| "a newer loadout is available — run `load update`".to_string())
}

/// The detached cache refresher (the `load update --refresh-cache` child):
/// perform the release check without a deadline and persist the verdict.
/// Silent and infallible by design — its only observable effect is the cache
/// file the next command's nudge reads.
pub fn refresh_cache() {
    let Some(path) = cache_path() else {
        return;
    };
    let verdict = match check_available() {
        Some(true) => Verdict::Available,
        Some(false) => Verdict::Current,
        None => Verdict::Failed,
    };
    let _ = write_cache(
        &path,
        &CheckCache {
            at: SystemTime::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            verdict,
        },
    );
}

/// Launch `load update --refresh-cache` detached: no wait, no inherited stdio,
/// its own process group. Short-lived parents exit right after (init reaps);
/// `run`'s exec reparents it the same way. Best-effort — a failed spawn just
/// means the stamped window retries later.
fn spawn_refresher() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["update", "--refresh-cache"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    if let Ok(child) = cmd.spawn() {
        drop(child);
    }
}

/// The ambient post-command nudge: [`nudge_detail`] with the mode read from
/// the global config, printed in the standard step style. Called from `main`
/// after interactive commands; `run` calls [`nudge_detail`] itself (it must
/// print *before* the launch `exec()`s away). Infallible: an unreadable
/// config falls back to the default mode.
pub fn ambient_nudge(cwd: &Path) {
    let repo_base = crate::context::repo_base_for(cwd);
    let mode = crate::config::Config::load(&repo_base)
        .map(|c| c.update.check)
        .unwrap_or_default();
    if let Some(detail) = nudge_detail(mode) {
        let p = crate::style::Painter::auto();
        println!(
            "{}",
            crate::commands::apply::step(&p, p.cyan("↑"), "update", detail)
        );
    }
}

/// What the last completed network check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// A newer release exists.
    Available,
    /// This binary is the newest release.
    Current,
    /// The check could not complete (offline, timeout, no receipt).
    Failed,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Current => "current",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "available" => Some(Self::Available),
            "current" => Some(Self::Current),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The persisted result of the last check: when, by which binary version, and
/// what it concluded. The version stamp invalidates the verdict across a
/// binary swap (`load update`, reinstall) — an "available" computed by 0.19.0
/// must not nudge the 0.20.0 that replaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckCache {
    at: SystemTime,
    version: String,
    verdict: Verdict,
}

/// The one decision, pure over `(mode, cache, now)` — no I/O, fully
/// unit-tested. Returns `(nudge, spawn-a-refresh)`.
///
/// The nudge always comes from the last known verdict — a due-for-refresh
/// "available" still nudges (stale-but-known beats silent). The refresh flag
/// is simply "the verdict is older than the mode's window, or missing, or
/// from another binary version".
fn decide(mode: CheckMode, cache: Option<&CheckCache>, now: SystemTime) -> (bool, bool) {
    let interval = match mode {
        CheckMode::Always => ALWAYS_INTERVAL,
        CheckMode::Daily => DAILY_INTERVAL,
        // Handled by the caller; treated as Daily defensively if it gets here.
        CheckMode::Off => DAILY_INTERVAL,
    };
    // A cache from a different binary version is meaningless — ignore it (and
    // refresh now). This also self-heals right after an update.
    let valid = cache.filter(|c| c.version == env!("CARGO_PKG_VERSION"));
    let nudge = valid.is_some_and(|c| c.verdict == Verdict::Available);
    let refresh = valid.is_none_or(|c| is_due(Some(c.at), now, interval));
    (nudge, refresh)
}

/// Where the check cache lives (alongside the global config).
fn cache_path() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("update-check"))
}

/// Read the cache: one line, `<unix-secs> <version> <verdict>`. A legacy file
/// holding only `<unix-secs>` (pre-0.20 stamp) reads as a `Failed` verdict at
/// that time by this version — it throttles like before and never nudges from
/// stale data.
fn read_cache(path: &Path) -> Option<CheckCache> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let secs: u64 = parts.next()?.parse().ok()?;
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    let (version, verdict) = match (parts.next(), parts.next().and_then(Verdict::parse)) {
        (Some(v), Some(verdict)) => (v.to_string(), verdict),
        _ => (env!("CARGO_PKG_VERSION").to_string(), Verdict::Failed),
    };
    Some(CheckCache {
        at,
        version,
        verdict,
    })
}

/// Persist the cache (see [`read_cache`] for the format).
fn write_cache(path: &Path, cache: &CheckCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let secs = cache
        .at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(
        path,
        format!("{secs} {} {}", cache.version, cache.verdict.as_str()),
    )
}

/// Whether a check is due: never checked, or `interval` has elapsed. A clock that
/// went backwards (`now` < `last`) also counts as due.
fn is_due(last: Option<SystemTime>, now: SystemTime, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t).map(|d| d >= interval).unwrap_or(true),
    }
}

/// `Some(true/false)` if we could ask the release host; `None` if there's no
/// receipt or the query failed. Only the detached refresher calls this — the
/// nudge itself never touches the network.
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
    fn cache_round_trips_and_reads_the_legacy_stamp() {
        let dir = tempfile::tempdir().unwrap();
        // Parent dir doesn't exist yet — write_cache must create it.
        let path = dir.path().join("nested").join("update-check");
        assert!(read_cache(&path).is_none(), "missing cache reads as None");

        let cache = CheckCache {
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            version: "0.20.0".to_string(),
            verdict: Verdict::Available,
        };
        write_cache(&path, &cache).unwrap();
        assert_eq!(read_cache(&path).unwrap(), cache);

        // A pre-0.20 stamp (bare unix seconds) reads as a Failed verdict at
        // that time — it throttles, but can never nudge from stale data.
        std::fs::write(&path, "1700000000").unwrap();
        let legacy = read_cache(&path).unwrap();
        assert_eq!(legacy.verdict, Verdict::Failed);
        assert_eq!(legacy.at, cache.at);
    }

    /// A cache entry `age` seconds old, written by this binary version.
    fn cache_aged(now: SystemTime, age: u64, verdict: Verdict) -> CheckCache {
        CheckCache {
            at: now - Duration::from_secs(age),
            version: env!("CARGO_PKG_VERSION").to_string(),
            verdict,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    #[test]
    fn no_verdict_spawns_a_refresh_and_stays_quiet() {
        // No cache at all: nothing to nudge from; a refresh is requested.
        let (nudge, refresh) = decide(CheckMode::Always, None, now());
        assert!(!nudge, "no verdict yet - never nudge on a guess");
        assert!(refresh, "missing verdict must request a refresh");
    }

    #[test]
    fn within_the_window_the_cached_verdict_rules_and_nothing_respawns() {
        // 5 min old (inside the 10-min always-window): available nudges,
        // current stays quiet, and neither asks for a refresh.
        let cache = cache_aged(now(), 5 * 60, Verdict::Available);
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(nudge, "a known-stale install is reminded on every command");
        assert!(!refresh, "fresh verdict - no refresh");

        let cache = cache_aged(now(), 5 * 60, Verdict::Current);
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(!nudge);
        assert!(!refresh);
    }

    #[test]
    fn a_due_available_verdict_still_nudges_while_refreshing() {
        // Stale-but-known beats silent: an 11-minute-old "available" nudges
        // AND requests a refresh in the same command.
        let cache = cache_aged(now(), 11 * 60, Verdict::Available);
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(nudge, "the last known verdict keeps nudging");
        assert!(
            refresh,
            "and the stale verdict is refreshed in the background"
        );
    }

    #[test]
    fn always_refreshes_after_its_window_daily_after_a_day() {
        // 11 min old: due under Always, not under Daily.
        let cache = cache_aged(now(), 11 * 60, Verdict::Current);
        let (_, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(refresh, "always refreshes after 10 minutes");
        let (_, refresh) = decide(CheckMode::Daily, Some(&cache), now());
        assert!(!refresh, "daily must not refresh an 11-minute-old verdict");

        // 25h old: due under Daily too.
        let cache = cache_aged(now(), 25 * 60 * 60, Verdict::Current);
        let (_, refresh) = decide(CheckMode::Daily, Some(&cache), now());
        assert!(refresh);
    }

    #[test]
    fn failed_verdict_backs_off_inside_the_window_and_retries_after() {
        // A fresh Failed stamp (the pre-spawn placeholder, or a dead-network
        // child's verdict): quiet, and no respawn inside the window - so a
        // burst of commands starts exactly one refresher.
        let cache = cache_aged(now(), 60, Verdict::Failed);
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(!nudge);
        assert!(!refresh, "the window doubles as the retry backoff");

        // Past the window the refresh is requested again.
        let cache = cache_aged(now(), 11 * 60, Verdict::Failed);
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(!nudge);
        assert!(refresh);
    }

    #[test]
    fn cache_from_another_binary_version_is_ignored() {
        // An "available" verdict computed by the binary this one replaced must
        // not nudge - it is treated as absent, and a refresh is requested.
        let cache = CheckCache {
            at: now() - Duration::from_secs(60),
            version: "0.0.1-not-this-binary".to_string(),
            verdict: Verdict::Available,
        };
        let (nudge, refresh) = decide(CheckMode::Always, Some(&cache), now());
        assert!(!nudge, "stale-version verdict discarded");
        assert!(refresh);
    }

    #[test]
    fn check_mode_parses_leniently() {
        assert_eq!(CheckMode::parse(None), CheckMode::Always, "default");
        assert_eq!(CheckMode::parse(Some("always")), CheckMode::Always);
        assert_eq!(CheckMode::parse(Some("Daily")), CheckMode::Daily);
        assert_eq!(CheckMode::parse(Some("off")), CheckMode::Off);
        assert_eq!(CheckMode::parse(Some("never")), CheckMode::Off);
        // Unknown (a newer loadout's value) degrades to Daily, never errors.
        assert_eq!(CheckMode::parse(Some("hourly")), CheckMode::Daily);
    }
}
