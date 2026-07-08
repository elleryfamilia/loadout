//! Secret / credential redaction.
//!
//! Two layers of defense:
//! 1. [`sanitize_url`] strips embedded credentials (`user:pass@host`) from URLs
//!    so git remotes can be surfaced without leaking tokens.
//! 2. [`redact_secrets`] scrubs token-like patterns from arbitrary text, used on
//!    every environment-variable value we surface (in addition to the allowlist).
//!
//! This is best-effort hygiene, **not** a security boundary. The real control is
//! the env allowlist in [`crate::context`]; redaction is the belt to its braces.

use std::sync::OnceLock;

use regex::Regex;

/// Placeholder substituted in place of a detected secret.
pub const REDACTED: &str = "***REDACTED***";

/// Strip embedded credentials from a URL while preserving host/path.
///
/// - `https://user:token@github.com/org/repo.git` → `https://github.com/org/repo.git`
/// - `https://x-access-token:ghp_abc@host/r.git`  → `https://host/r.git`
/// - SSH form `git@github.com:org/repo.git` is returned unchanged (no secret).
/// - Anything that doesn't parse as a credentialed URL is returned unchanged,
///   then passed through [`redact_secrets`] as a backstop.
pub fn sanitize_url(url: &str) -> String {
    let trimmed = url.trim();

    // scheme://[user[:pass]@]rest  -> scheme://rest
    if let Some(scheme_end) = trimmed.find("://") {
        let (scheme, rest) = trimmed.split_at(scheme_end + 3);
        // Only the authority section (up to the first '/') may contain creds.
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        if let Some(at) = authority.rfind('@') {
            let host = &authority[at + 1..];
            return redact_secrets(&format!("{scheme}{host}{path}"));
        }
        return redact_secrets(trimmed);
    }

    redact_secrets(trimmed)
}

/// Replace token/credential-like substrings in `text` with [`REDACTED`].
///
/// Covers common provider token formats plus generic `key = value` secret
/// assignments and PEM private-key blocks. Conservative by design: it would
/// rather over-redact than leak.
pub fn redact_secrets(text: &str) -> String {
    redact_secrets_report(text).0
}

/// Like [`redact_secrets`], but also reports how many replacements were made
/// so call sites can warn the user (`0` means the text was already clean).
pub fn redact_secrets_report(text: &str) -> (String, usize) {
    let mut out = text.to_string();
    let mut count = 0usize;
    for re in patterns() {
        // Skip matches that only re-match an earlier pattern's placeholder
        // (e.g. `token=***REDACTED***` re-matching the generic assignment
        // pattern) — they'd double-count one secret and eat the key name.
        let replaced = re.replace_all(&out, |caps: &regex::Captures| {
            let m = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            if m.contains(REDACTED) {
                m.to_string()
            } else {
                count += 1;
                REDACTED.to_string()
            }
        });
        out = replaced.into_owned();
    }
    (out, count)
}

/// Recursively redact every string leaf of a JSON value in place, returning
/// the number of replacements. Applied to provider `data` before it is cached
/// or exposed to templates.
pub fn redact_json(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => {
            let (clean, n) = redact_secrets_report(s);
            if n > 0 {
                *s = clean;
            }
            n
        }
        serde_json::Value::Array(items) => items.iter_mut().map(redact_json).sum(),
        serde_json::Value::Object(map) => map.values_mut().map(redact_json).sum(),
        _ => 0,
    }
}

/// True if `text` appears to contain a secret (used by `doctor`/tests).
pub fn looks_secret(text: &str) -> bool {
    patterns().iter().any(|re| re.is_match(text))
}

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // PEM private key blocks (collapse the whole block).
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            // GitHub tokens (classic + fine-grained).
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            // AWS access key id + generic AWS secret key assignment.
            r"AKIA[0-9A-Z]{16}",
            // Slack tokens.
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
            // Google API keys.
            r"AIza[0-9A-Za-z\-_]{30,}",
            // OpenAI / Anthropic style keys.
            r"sk-[A-Za-z0-9_\-]{16,}",
            r"sk-ant-[A-Za-z0-9_\-]{16,}",
            // JWTs.
            r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
            // Generic `secret/token/key/password = <value>` assignments.
            r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|pwd|access[_-]?key|client[_-]?secret|authorization|bearer)\b\s*[:=]\s*['"]?[^\s'"]{6,}"#,
        ]
        .into_iter()
        .map(|p| Regex::new(p).expect("static redaction pattern must compile"))
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_basic_auth_from_https_url() {
        assert_eq!(
            sanitize_url("https://user:token@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn strips_access_token_user() {
        assert_eq!(
            sanitize_url("https://x-access-token:ghp_abcdefghijklmnopqrstuvwxyz012345@host/r.git"),
            "https://host/r.git"
        );
    }

    #[test]
    fn leaves_clean_https_url() {
        assert_eq!(
            sanitize_url("https://github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn leaves_ssh_remote_unchanged() {
        assert_eq!(
            sanitize_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }

    #[test]
    fn redacts_github_token() {
        let out = redact_secrets("token=ghp_abcdefghijklmnopqrstuvwxyz012345");
        assert!(out.contains(REDACTED), "got: {out}");
        assert!(!out.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redacts_generic_assignment() {
        let out = redact_secrets("API_KEY=supersecretvalue123");
        assert!(out.contains(REDACTED), "got: {out}");
        assert!(!out.contains("supersecretvalue123"));
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nABCDEF\n-----END RSA PRIVATE KEY-----";
        let out = redact_secrets(pem);
        assert_eq!(out, REDACTED);
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert!(looks_secret(jwt));
        assert_eq!(redact_secrets(jwt), REDACTED);
    }

    #[test]
    fn clean_text_untouched() {
        let s = "This is normal guidance about running cargo test.";
        assert_eq!(redact_secrets(s), s);
        assert!(!looks_secret(s));
    }

    #[test]
    fn report_counts_replacements() {
        let (out, n) = redact_secrets_report(
            "a=ghp_abcdefghijklmnopqrstuvwxyz012345 b=ghp_zyxwvutsrqponmlkjihgfedcba54321",
        );
        assert_eq!(n, 2);
        assert!(!out.contains("ghp_"), "got: {out}");
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn report_is_zero_on_clean_text() {
        let (out, n) = redact_secrets_report("run cargo test before shipping");
        assert_eq!(n, 0);
        assert_eq!(out, "run cargo test before shipping");
    }

    #[test]
    fn redact_json_walks_nested_values() {
        let mut v = serde_json::json!({
            "stdout": "token=ghp_abcdefghijklmnopqrstuvwxyz012345",
            "nested": { "list": ["sk-abcdefghijklmnop", 42, true] },
        });
        let n = redact_json(&mut v);
        assert_eq!(n, 2);
        let s = v.to_string();
        assert!(!s.contains("ghp_"), "{s}");
        assert!(!s.contains("sk-abcdef"), "{s}");
        assert!(s.contains(REDACTED));
    }
}
