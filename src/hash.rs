//! Deterministic context hashing.
//!
//! The context hash lets `render`/`refresh` skip rewriting a file when nothing
//! that affects its content changed, and gives `explain`/the audit log a stable
//! fingerprint of "the inputs that produced this output".
//!
//! It is computed over a canonical JSON form of the [`Context`](crate::context::Context)
//! with volatile, output-irrelevant fields removed (e.g. the parent process,
//! which differs between a direct invocation and `load run`).

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Compute `sha256:<hex>` over the canonical JSON of `value`.
///
/// `serde_json` serializes derived structs in field-declaration order and we use
/// `BTreeMap`/sorted `Vec`s for collections, so the encoding is stable across
/// runs without an explicit canonicalization pass.
pub fn context_hash<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_vec(value).expect("context must serialize to JSON");
    let mut hasher = Sha256::new();
    hasher.update(&json);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Short form of a hash for compact display (`sha256:abcd1234…`).
pub fn short(hash: &str) -> String {
    match hash.strip_prefix("sha256:") {
        Some(hex) if hex.len() > 12 => format!("sha256:{}…", &hex[..12]),
        _ => hash.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Sample {
        a: u32,
        b: Vec<String>,
    }

    #[test]
    fn stable_and_prefixed() {
        let s = Sample {
            a: 1,
            b: vec!["x".into(), "y".into()],
        };
        let h1 = context_hash(&s);
        let h2 = context_hash(&s);
        assert_eq!(h1, h2, "hash must be deterministic");
        assert!(h1.starts_with("sha256:"));
        assert_eq!(h1.len(), 7 + 64);
    }

    #[test]
    fn changes_with_content() {
        let a = Sample { a: 1, b: vec![] };
        let b = Sample { a: 2, b: vec![] };
        assert_ne!(context_hash(&a), context_hash(&b));
    }

    #[test]
    fn short_truncates() {
        let h = context_hash(&Sample { a: 1, b: vec![] });
        let s = short(&h);
        assert!(s.starts_with("sha256:"));
        assert!(s.ends_with('…'));
        assert!(s.len() < h.len());
    }
}
