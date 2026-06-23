//! Minimal verbosity-aware reporting to stderr.
//!
//! Kept dependency-free on purpose: a process-global verbosity flag plus a few
//! macros. Normal command output goes to stdout via `println!`; diagnostics and
//! progress go through here to stderr so machine-readable stdout stays clean.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);
static QUIET_WARNINGS: AtomicBool = AtomicBool::new(false);

/// Enable or disable verbose diagnostics for the whole process.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether verbose diagnostics are enabled.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Suppress (or re-enable) `warn_user!` output for the whole process. Used by
/// commands like `doctor` that surface the same conditions through their own
/// structured checks and don't want the duplicate stderr lines.
pub fn set_quiet_warnings(on: bool) {
    QUIET_WARNINGS.store(on, Ordering::Relaxed);
}

/// Whether `warn_user!` output is currently suppressed.
pub fn warnings_suppressed() -> bool {
    QUIET_WARNINGS.load(Ordering::Relaxed)
}

/// Emit a verbose diagnostic line to stderr (only when `--verbose`).
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {{
        if $crate::report::is_verbose() {
            eprintln!("[loadout] {}", format!($($arg)*));
        }
    }};
}

/// Emit a warning to stderr (shown unless warnings are suppressed for the
/// process — see [`set_quiet_warnings`]).
#[macro_export]
macro_rules! warn_user {
    ($($arg:tt)*) => {{
        if !$crate::report::warnings_suppressed() {
            eprintln!("warning: {}", format!($($arg)*));
        }
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn quiet_warnings_toggles() {
        assert!(!super::warnings_suppressed());
        super::set_quiet_warnings(true);
        assert!(super::warnings_suppressed());
        super::set_quiet_warnings(false);
        assert!(!super::warnings_suppressed());
    }
}
