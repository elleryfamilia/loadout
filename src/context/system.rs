//! System + environment detection.
//!
//! [`SystemDetector`] fills host/user/caller info and derives a host class from
//! config globs. [`EnvDetector`] surfaces **only** allowlisted environment
//! variables, drops any whose name matches a denylist pattern, and redacts
//! values as a backstop.

use std::process::Command;

use crate::context::{Context, ContextDetector, DetectInput};
use crate::redact::redact_secrets;

/// Detector for OS/arch/host/user/caller/host-class.
pub struct SystemDetector;

impl ContextDetector for SystemDetector {
    fn name(&self) -> &'static str {
        "system"
    }

    fn detect(&self, input: &DetectInput, ctx: &mut Context) -> crate::Result<()> {
        ctx.system.os = std::env::consts::OS.to_string();
        ctx.system.arch = std::env::consts::ARCH.to_string();
        ctx.system.hostname = hostname();
        ctx.system.user = current_user();
        ctx.system.parent_process = parent_process_name();
        ctx.system.host_class = classify_host(&ctx.system.hostname, input);
        Ok(())
    }
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

fn current_user() -> String {
    for key in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

/// Best-effort parent process (the caller) name via `getppid` + `ps`.
///
/// Returns `None` on any platform/permission failure — purely informational.
fn parent_process_name() -> Option<String> {
    // SAFETY: getppid is always safe; it just reads the parent PID.
    let ppid = unsafe { libc::getppid() };
    if ppid <= 1 {
        return None;
    }
    let output = Command::new("ps")
        .args(["-o", "comm=", "-p", &ppid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // `comm` may be a path; keep just the executable name.
    let name = line.rsplit('/').next().unwrap_or(line);
    Some(name.to_string())
}

/// Match the hostname against config `host_classes` globs; first class wins.
fn classify_host(hostname: &str, input: &DetectInput) -> Option<String> {
    for (class, patterns) in &input.config.host_classes {
        if patterns.iter().any(|p| glob_match(p, hostname)) {
            return Some(class.clone());
        }
    }
    None
}

/// Tiny `*`/`?` glob matcher (anchored, case-insensitive).
///
/// `*` matches any run (including empty), `?` matches one character. Sufficient
/// for hostname classification without pulling in a glob crate.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    // Classic DP / two-pointer with star backtracking.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Detector that surfaces allowlisted, redacted environment variables.
pub struct EnvDetector;

impl ContextDetector for EnvDetector {
    fn name(&self) -> &'static str {
        "env"
    }

    fn detect(&self, input: &DetectInput, ctx: &mut Context) -> crate::Result<()> {
        let cfg = &input.config.env;
        let deny: Vec<regex::Regex> = cfg
            .deny_name_patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();

        for name in &cfg.allowlist {
            // Denylist on the *name* wins even if it was allowlisted.
            if deny.iter().any(|re| re.is_match(name)) {
                crate::vlog!("env '{name}' allowlisted but matches denylist; skipping");
                continue;
            }
            if let Ok(value) = std::env::var(name) {
                ctx.env.insert(name.clone(), redact_secrets(&value));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basic() {
        assert!(glob_match("*.corp.example.com", "build.corp.example.com"));
        assert!(glob_match("work-*", "work-laptop"));
        assert!(glob_match("exact", "exact"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("work-*", "home-laptop"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn glob_case_insensitive() {
        assert!(glob_match("WORK-*", "work-laptop"));
    }

    #[test]
    fn classify_picks_matching_class() {
        let mut cfg = crate::config::Config::defaults();
        cfg.host_classes
            .insert("work".into(), vec!["*.corp.example.com".into()]);
        let input = DetectInput {
            cwd: ".".into(),
            repo_base: ".".into(),
            config: &cfg,
            live: false,
        };
        assert_eq!(
            classify_host("ci.corp.example.com", &input).as_deref(),
            Some("work")
        );
        assert_eq!(classify_host("home.lan", &input), None);
    }

    #[test]
    fn env_detector_respects_denylist() {
        // SECRET_TOKEN matches the default name denylist even if allowlisted.
        let mut cfg = crate::config::Config::defaults();
        cfg.env.allowlist = vec!["ROSITA_TEST_PLAIN".into(), "ROSITA_TEST_SECRET".into()];
        // We can't reliably set process env in parallel tests; assert the name
        // filter directly instead.
        let deny: Vec<regex::Regex> = cfg
            .env
            .deny_name_patterns
            .iter()
            .map(|p| regex::Regex::new(p).unwrap())
            .collect();
        assert!(deny.iter().any(|re| re.is_match("ROSITA_TEST_SECRET")));
        assert!(!deny.iter().any(|re| re.is_match("ROSITA_TEST_PLAIN")));
    }
}
