//! Minimal verbosity-aware reporting to stderr.
//!
//! Kept dependency-free on purpose: a process-global verbosity flag plus a few
//! macros. Normal command output goes to stdout via `println!`; diagnostics and
//! progress go through here to stderr so machine-readable stdout stays clean.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static VERBOSE: AtomicBool = AtomicBool::new(false);
static QUIET_WARNINGS: AtomicBool = AtomicBool::new(false);

/// When `Some`, `warn_user!` lines are collected here instead of printed, so an
/// animated region (the equipping HUD) that owns the terminal isn't corrupted
/// by a stray stderr write mid-frame. Drained and printed once the region ends.
static WARN_BUFFER: Mutex<Option<Vec<String>>> = Mutex::new(None);

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

/// Start capturing `warn_user!` lines instead of printing them (idempotent).
/// The equipping HUD calls this while it owns the terminal.
pub fn begin_warning_capture() {
    let mut b = WARN_BUFFER.lock().unwrap();
    if b.is_none() {
        *b = Some(Vec::new());
    }
}

/// Stop capturing and return the buffered warnings (empty if none / not
/// capturing). The caller prints them once its animated region has ended.
pub fn end_warning_capture() -> Vec<String> {
    WARN_BUFFER.lock().unwrap().take().unwrap_or_default()
}

/// Route one warning: to the capture buffer if active, else straight to stderr.
/// Suppressed warnings ([`set_quiet_warnings`]) are dropped either way. Called
/// only by [`warn_user!`]; not part of the public surface.
#[doc(hidden)]
pub fn emit_warning(args: std::fmt::Arguments<'_>) {
    if warnings_suppressed() {
        return;
    }
    let line = format!("warning: {args}");
    let mut b = WARN_BUFFER.lock().unwrap();
    match b.as_mut() {
        Some(buf) => buf.push(line),
        None => eprintln!("{line}"),
    }
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
/// process — see [`set_quiet_warnings`]). Routed through
/// [`emit_warning`](crate::report::emit_warning) so it can be captured while an
/// animated region owns the terminal.
#[macro_export]
macro_rules! warn_user {
    ($($arg:tt)*) => {{
        $crate::report::emit_warning(format_args!($($arg)*));
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
