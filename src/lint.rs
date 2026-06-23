//! Shared private-data leak lint.
//!
//! Flags machine-specific literals — IPv4 addresses, `*.domain.tld` globs, and
//! multi-label hostnames — that belong in the gitignored `local.toml`, not the
//! shareable `config.toml`. Used by both `load doctor` and `load studio`
//! (where it doubles as the cross-machine **sync-safety** guard, §6/§7).
//!
//! It is a **heuristic warning, never a gate**: the multi-label-hostname rule
//! false-positives on legitimate values (`next.config.js`, `example.com` in
//! prose), so callers inform and let the user decide rather than blocking.

use regex::Regex;

/// Regexes for machine-specific literals. Patterns are static and valid, so the
/// `unwrap` is sound. Compiled per call (the call sites are not hot).
pub fn patterns() -> Vec<Regex> {
    [
        r"\b(?:\d{1,3}\.){3}\d{1,3}\b",                    // IPv4
        r"\*\.[A-Za-z0-9-]+\.[A-Za-z0-9.-]+",              // *.domain.tld glob
        r"\b[A-Za-z0-9-]+\.[A-Za-z0-9-]+\.[A-Za-z]{2,}\b", // multi-label hostname
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
}

/// Whether a single string looks machine-specific (private).
pub fn looks_private(s: &str) -> bool {
    patterns().iter().any(|re| re.is_match(s))
}

/// Every string leaf in a parsed TOML value that looks private (sorted, deduped).
pub fn find_in_toml(value: &toml::Value) -> Vec<String> {
    let pats = patterns();
    let mut hits = Vec::new();
    collect(value, &pats, &mut hits);
    hits.sort();
    hits.dedup();
    hits
}

/// Parse `toml_text` and return its private-looking string leaves. A parse error
/// yields no hits (it surfaces elsewhere as a real error).
pub fn find_in_text(toml_text: &str) -> Vec<String> {
    match toml::from_str::<toml::Value>(toml_text) {
        Ok(v) => find_in_toml(&v),
        Err(_) => Vec::new(),
    }
}

fn collect(value: &toml::Value, patterns: &[Regex], out: &mut Vec<String>) {
    match value {
        toml::Value::String(s) => {
            if patterns.iter().any(|re| re.is_match(s)) {
                out.push(s.clone());
            }
        }
        toml::Value::Array(items) => {
            for v in items {
                collect(v, patterns, out);
            }
        }
        toml::Value::Table(t) => {
            for v in t.values() {
                collect(v, patterns, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_machine_specific_literals() {
        assert!(looks_private("192.168.1.10"));
        assert!(looks_private("*.corp.example.com"));
        assert!(looks_private("build-box.corp.example.com"));
        // Ordinary values aren't flagged.
        assert!(!looks_private("deploy"));
        assert!(!looks_private("rust-conventions"));
    }

    #[test]
    fn finds_hits_in_toml_deduped() {
        let v: toml::Value = toml::from_str(
            "[host_classes]\nwork = [\"*.corp.example.com\", \"*.corp.example.com\"]\nip = \"10.0.0.1\"\n",
        )
        .unwrap();
        let hits = find_in_toml(&v);
        assert!(hits.contains(&"*.corp.example.com".to_string()));
        assert!(hits.contains(&"10.0.0.1".to_string()));
        // deduped
        assert_eq!(
            hits.iter().filter(|h| *h == "*.corp.example.com").count(),
            1
        );
    }

    #[test]
    fn parse_error_yields_no_hits() {
        assert!(find_in_text("not = valid = toml").is_empty());
    }
}
