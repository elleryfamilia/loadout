//! Pure config health checks shared by `load doctor` (verbose, every check)
//! and `load refresh` (fast subset, warnings only).
//!
//! Eligibility for this module: a check reads only the loaded `Config` and
//! repo-local files — no script execution, no network, no `$HOME` state.
//! Checks return findings instead of printing so each caller controls its own
//! output (doctor prints `Ok` lines too; refresh stays silent when healthy).

use crate::config;

/// Severity of one finding (doctor also prints `Ok` lines; refresh never does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
        }
    }
}

/// One check outcome line.
#[derive(Debug)]
pub struct Finding {
    pub status: Status,
    pub message: String,
}

impl Finding {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            message: message.into(),
        }
    }
    pub fn warn(message: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            message: message.into(),
        }
    }
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            message: message.into(),
        }
    }
}

/// Scan the RAW text of every contributing config source file for
/// secret-looking strings, reporting file path + line so the user removes the
/// secret at its origin. Covers `local.toml` too — a secret doesn't belong in
/// any config layer (render-time redaction is the backstop, not a licence).
pub fn secret_leaks(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut scanned = 0usize;
    for src in &cfg.sources {
        let Ok(text) = std::fs::read_to_string(src) else {
            continue;
        };
        scanned += 1;
        let mut line_hits = 0usize;
        for (i, line) in text.lines().enumerate() {
            if crate::redact::looks_secret(line) {
                line_hits += 1;
                findings.push(Finding::warn(format!(
                    "{}:{}: looks like a secret — remove it from config and rotate the credential",
                    src.display(),
                    i + 1,
                )));
            }
        }
        // Multi-line secrets (PEM private-key blocks) never match a single
        // line — fall back to a whole-file scan for those.
        if line_hits == 0 && crate::redact::looks_secret(&text) {
            findings.push(Finding::warn(format!(
                "{}: looks like it contains a multi-line secret (e.g. a private key) — remove it from config and rotate the credential",
                src.display(),
            )));
        }
    }
    if findings.is_empty() {
        findings.push(Finding::ok(format!(
            "no secret-looking strings in {scanned} config file(s)"
        )));
    }
    findings
}
