//! Per-machine "recents" registry: loadout-generated HTML artifacts (today:
//! plan previews from `load plan render`) that `load studio` lists and serves.
//!
//! Lives in the per-machine state dir ([`crate::config::state_dir`]) — never
//! synced (entries hold absolute machine-local paths; the config dir goes
//! under git via `load sync`, this must not).
//!
//! The registry is a convenience cache, NOT a security boundary: the serving
//! route re-checks the file itself (marker gate) and serves under an
//! origin-isolating sandbox CSP. Consequently, unlike trust.rs, a CORRUPT
//! store loads empty-and-writable and self-heals on the next record.
//! A store written by a NEWER loadout is refused read-only (same as trust):
//! rewriting it would destroy entry kinds this binary doesn't understand.
//!
//! Future-kind contract (recaps, reports, …) — a kind that wants to appear in
//! Recents must: (1) render a fully self-contained HTML file whose first line
//! is the GENERATED_MARKER comment with a context hash — anything else does
//! not belong in this registry; (2) `record()` after successful non-dry-run
//! writes, best-effort (a registry failure must never fail the command);
//! (3) `remove_path()` in its clean verb; (4) optionally add one match arm in
//! the studio row builder for a nicer detail line / staleness badge — without
//! it the generic row (kind, title, repo, age) renders for free.
//! Adding a kind is NOT a version bump; `version` is reserved for structural
//! changes to this envelope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the state dir.
pub const STORE_FILE: &str = "recents.json";

/// Schema version of the on-disk store (envelope structure, not kinds).
const STORE_VERSION: u32 = 1;

/// Bounded recency: evict the oldest beyond this on record.
pub const MAX_ENTRIES: usize = 30;

/// The exact CSP attached to served artifacts. `sandbox` works only as a
/// RESPONSE header (ignored in <meta>): it gives the document an opaque
/// origin — scripts run, but studio's cookie/API/storage are unreachable.
/// The rest mirrors the plan page's own meta CSP (the file at the recorded
/// path is attacker-writable, so the meta CSP can't be relied on; policies
/// intersect, so this header can only tighten).
/// NEVER add `allow-same-origin`. NEVER serve artifact bytes any other way
/// (inline swap, srcdoc, blob:) — that would run them in studio's origin.
pub const SERVE_CSP: &str = "sandbox allow-scripts; default-src 'none'; \
style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; \
font-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The full header set for serving an artifact — single source shared by the
/// studio handler, its route() tests, and the browser-smoke server.
pub fn serve_header_pairs() -> [(&'static str, &'static str); 5] {
    [
        ("content-type", "text/html; charset=utf-8"),
        ("content-security-policy", SERVE_CSP),
        ("x-content-type-options", "nosniff"),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
    ]
}

/// Stable route key for an artifact path: first 16 hex chars of SHA-256 over
/// the raw path bytes. Deliberately NOT `hash::context_hash` (serde-serializes
/// and panics on non-UTF-8 paths — a panic inside best-effort recording would
/// kill `load plan render` after a successful render) and NOT `hash::short`
/// (its output contains `:` and `…`, hostile to URL round-tripping).
pub fn id_for_path(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    let digest = Sha256::digest(path.as_os_str().as_bytes());
    digest.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// One recorded artifact. `path` is the dedup key (absolutized at record
/// time). `detail` is kind-specific display data the registry never
/// interprets; `extra` round-trips fields a newer binary may have written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: String,
    pub path: PathBuf,
    pub repo: PathBuf,
    pub title: String,
    pub hash: String,
    pub rendered_at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, serde_json::Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Entry {
    pub fn id(&self) -> String {
        id_for_path(&self.path)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    entries: Vec<Entry>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            entries: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// What `record()` actually did — the render command's messaging depends on
/// it (never advertise an entry that wasn't written).
#[must_use]
#[derive(Debug)]
pub enum RecordOutcome {
    Recorded,
    /// Store written by a newer loadout: read-only, bytes preserved.
    ReadOnlyNewer,
    /// No resolvable state dir (no home).
    NoStateDir,
    Failed(String),
}

/// The on-disk recents store.
#[derive(Debug)]
pub struct RecentsStore {
    path: Option<PathBuf>,
    file: StoreFile,
    readonly: bool,
}

impl RecentsStore {
    /// Load from the per-machine state dir.
    pub fn load_default() -> Self {
        Self::load_opt(Self::default_path())
    }

    #[cfg(not(test))]
    fn default_path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join(STORE_FILE))
    }

    /// In-process unit tests must never touch the real per-machine store —
    /// same guard as `TrustStore::default_path`.
    #[cfg(test)]
    fn default_path() -> Option<PathBuf> {
        Some(
            std::env::temp_dir()
                .join(format!("loadout-recents-test-{}", std::process::id()))
                .join(STORE_FILE),
        )
    }

    /// Load from an optional path; `None` (no state dir) is an empty store
    /// whose writes report [`RecordOutcome::NoStateDir`] / no-op.
    pub fn load_opt(path: Option<PathBuf>) -> Self {
        match path {
            Some(p) => Self::load_from(&p),
            None => Self {
                path: None,
                file: StoreFile::default(),
                readonly: false,
            },
        }
    }

    /// Load from an explicit path (studio handlers, unit tests).
    pub fn load_from(path: &Path) -> Self {
        let (file, readonly) = match std::fs::read_to_string(path) {
            Err(_) => (StoreFile::default(), false),
            Ok(text) => match serde_json::from_str::<StoreFile>(&text) {
                // Newer schema: refuse read-only (a rewrite would destroy
                // structure this binary can't represent). Bytes preserved.
                Ok(f) if f.version > STORE_VERSION => (StoreFile::default(), true),
                Ok(f) => (f, false),
                // Corrupt: empty AND writable — self-heals on next record
                // (deliberate divergence from trust.rs; see module docs).
                Err(_) => (StoreFile::default(), false),
            },
        };
        Self {
            path: Some(path.to_path_buf()),
            file,
            readonly,
        }
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    pub fn is_empty(&self) -> bool {
        self.file.entries.is_empty()
    }

    /// Entries newest-first. RFC 3339 UTC "Z" strings sort chronologically
    /// as plain strings, so no parsing is needed here.
    pub fn entries(&self) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.file.entries.iter().collect();
        v.sort_by(|a, b| b.rendered_at.cmp(&a.rendered_at));
        v
    }

    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.file.entries.iter().find(|e| e.id() == id)
    }

    /// Upsert by path (absolutized), evict beyond [`MAX_ENTRIES`], save.
    pub fn record(&mut self, mut entry: Entry) -> RecordOutcome {
        if self.readonly {
            return RecordOutcome::ReadOnlyNewer;
        }
        if self.path.is_none() {
            return RecordOutcome::NoStateDir;
        }
        entry.path = std::path::absolute(&entry.path).unwrap_or(entry.path);
        self.file.entries.retain(|e| e.path != entry.path);
        self.file.entries.insert(0, entry);
        // Evict the oldest by rendered_at, not storage position.
        while self.file.entries.len() > MAX_ENTRIES {
            let oldest = self
                .file
                .entries
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| a.rendered_at.cmp(&b.rendered_at))
                .map(|(i, _)| i)
                .expect("non-empty");
            self.file.entries.remove(oldest);
        }
        match self.save() {
            Ok(()) => RecordOutcome::Recorded,
            Err(e) => RecordOutcome::Failed(e.to_string()),
        }
    }

    /// Remove the entry for `path` (absolutized first, matching `record`).
    /// No-op on a read-only store.
    pub fn remove_path(&mut self, path: &Path) -> crate::Result<()> {
        if self.readonly {
            return Ok(());
        }
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let before = self.file.entries.len();
        self.file.entries.retain(|e| e.path != abs);
        if self.file.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// Remove one entry by its route id. No-op on a read-only store.
    pub fn remove_id(&mut self, id: &str) -> crate::Result<()> {
        if self.readonly {
            return Ok(());
        }
        let before = self.file.entries.len();
        self.file.entries.retain(|e| e.id() != id);
        if self.file.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// Forget every entry (never touches artifact files). No-op read-only.
    pub fn clear(&mut self) -> crate::Result<()> {
        if self.readonly {
            return Ok(());
        }
        self.file.entries.clear();
        self.save()
    }

    fn save(&self) -> crate::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        crate::writer::atomic_write(path, &serde_json::to_string_pretty(&self.file)?)
    }
}

/// Humanized age for a stored RFC 3339 timestamp; empty string when it
/// doesn't parse (never fails a render over display sugar).
pub fn age_label(rendered_at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rendered_at) else {
        return String::new();
    };
    let secs = (now - t.with_timezone(&chrono::Utc)).num_seconds().max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Clamp a (hostile-influenced) title for storage: ≤120 chars + ellipsis,
/// char-boundary safe.
pub fn clamp_title(s: &str) -> String {
    if s.chars().count() <= 120 {
        return s.to_string();
    }
    let mut out: String = s.chars().take(120).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn entry(path: &str, title: &str, at: &str) -> Entry {
        Entry {
            kind: "plan".into(),
            path: PathBuf::from(path),
            repo: PathBuf::from("/repo"),
            title: title.into(),
            hash: "sha256:h".into(),
            rendered_at: at.into(),
            detail: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn missing_file_is_an_empty_writable_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecentsStore::load_from(&dir.path().join("recents.json"));
        assert!(!store.is_readonly());
        assert!(store.is_empty());
    }

    #[test]
    fn record_upserts_by_path_reorders_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recents.json");
        let mut store = RecentsStore::load_from(&path);
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-01T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        assert!(matches!(
            store.record(entry("/b/plan.html", "B", "2026-07-02T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        // Re-record /a with a newer timestamp: still 2 entries, /a first.
        assert!(matches!(
            store.record(entry("/a/plan.html", "A2", "2026-07-03T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        let store = RecentsStore::load_from(&path);
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "A2");
        assert_eq!(entries[1].title, "B");
        // Cap: fill past MAX_ENTRIES; the oldest falls off.
        let mut store = RecentsStore::load_from(&path);
        for i in 0..MAX_ENTRIES {
            let _ = store.record(entry(
                &format!("/cap/{i}/plan.html"),
                "cap",
                &format!("2026-07-04T00:00:{i:02}Z"),
            ));
        }
        let store = RecentsStore::load_from(&path);
        assert_eq!(store.entries().len(), MAX_ENTRIES);
        assert!(!store
            .entries()
            .iter()
            .any(|e| e.path == std::path::Path::new("/a/plan.html")));
    }

    #[test]
    fn corrupt_store_loads_empty_and_self_heals_on_next_record() {
        // DELIBERATE divergence from trust.rs: a corrupt trust store must stay
        // loud and unwritable (change warnings are a security property); a
        // corrupt recents list is cosmetic, so the next render overwrites it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recents.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mut store = RecentsStore::load_from(&path);
        assert!(!store.is_readonly());
        assert!(store.is_empty());
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-01T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        let healed = RecentsStore::load_from(&path);
        assert_eq!(healed.entries().len(), 1);
    }

    #[test]
    fn newer_version_store_is_readonly_and_byte_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recents.json");
        let newer = r#"{"version":999,"entries":[],"future_field":true}"#;
        std::fs::write(&path, newer).unwrap();
        let mut store = RecentsStore::load_from(&path);
        assert!(store.is_readonly());
        assert!(store.is_empty());
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-01T00:00:00Z")),
            RecordOutcome::ReadOnlyNewer
        ));
        store.clear().unwrap();
        store.remove_id("whatever").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn no_state_dir_store_is_empty_and_reports_no_state_dir() {
        let mut store = RecentsStore::load_opt(None);
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-01T00:00:00Z")),
            RecordOutcome::NoStateDir
        ));
    }

    #[test]
    fn unknown_kinds_and_fields_survive_a_rewrite() {
        // Forward compat: an entry written by a newer binary (unknown kind,
        // extra fields) must round-trip losslessly through this binary's
        // record() rewrite. serde(flatten) maps carry the unknowns.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recents.json");
        let doc = r#"{
          "version": 1,
          "entries": [{
            "kind": "recap", "path": "/x/recap.html", "repo": "/x",
            "title": "Recap", "hash": "sha256:r",
            "rendered_at": "2026-07-01T00:00:00Z",
            "novel_field": {"nested": true}
          }],
          "store_novelty": 7
        }"#;
        std::fs::write(&path, doc).unwrap();
        let mut store = RecentsStore::load_from(&path);
        assert_eq!(store.entries().len(), 1);
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-02T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("novel_field"),
            "entry-level unknown kept: {raw}"
        );
        assert!(
            raw.contains("store_novelty"),
            "store-level unknown kept: {raw}"
        );
        assert!(raw.contains("\"recap\""));
    }

    #[test]
    fn remove_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recents.json");
        let mut store = RecentsStore::load_from(&path);
        assert!(matches!(
            store.record(entry("/a/plan.html", "A", "2026-07-01T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        assert!(matches!(
            store.record(entry("/b/plan.html", "B", "2026-07-02T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        let id_a = id_for_path(std::path::Path::new("/a/plan.html"));
        store.remove_id(&id_a).unwrap();
        assert_eq!(store.entries().len(), 1);
        store
            .remove_path(std::path::Path::new("/b/plan.html"))
            .unwrap();
        assert!(store.is_empty());
        assert!(matches!(
            store.record(entry("/c/plan.html", "C", "2026-07-03T00:00:00Z")),
            RecordOutcome::Recorded
        ));
        store.clear().unwrap();
        assert!(RecentsStore::load_from(&path).is_empty());
    }

    #[test]
    fn id_is_16_hex_and_stable_per_path() {
        let a = id_for_path(std::path::Path::new("/a/plan.html"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, id_for_path(std::path::Path::new("/a/plan.html")));
        assert_ne!(a, id_for_path(std::path::Path::new("/b/plan.html")));
        let e = entry("/a/plan.html", "A", "2026-07-01T00:00:00Z");
        assert_eq!(e.id(), a);
    }

    #[test]
    fn age_label_buckets() {
        use chrono::{TimeZone, Utc};
        let now = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
        assert_eq!(age_label("2026-07-09T11:59:30Z", now), "just now");
        assert_eq!(age_label("2026-07-09T11:30:00Z", now), "30m ago");
        assert_eq!(age_label("2026-07-09T03:00:00Z", now), "9h ago");
        assert_eq!(age_label("2026-07-01T12:00:00Z", now), "8d ago");
        assert_eq!(age_label("not a timestamp", now), "");
    }

    #[test]
    fn clamp_title_is_char_boundary_safe() {
        assert_eq!(clamp_title("short"), "short");
        let long = "é".repeat(200);
        let clamped = clamp_title(&long);
        assert_eq!(clamped.chars().count(), 121); // 120 + '…'
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn serve_headers_pin_the_csp() {
        let pairs = serve_header_pairs();
        assert_eq!(pairs.len(), 5);
        let csp = pairs
            .iter()
            .find(|(k, _)| *k == "content-security-policy")
            .unwrap()
            .1;
        assert_eq!(csp, SERVE_CSP);
        assert!(SERVE_CSP.starts_with("sandbox allow-scripts;"));
        assert!(pairs
            .iter()
            .any(|(k, v)| *k == "x-content-type-options" && *v == "nosniff"));
        assert!(pairs
            .iter()
            .any(|(k, v)| *k == "cache-control" && *v == "no-store"));
        assert!(pairs
            .iter()
            .any(|(k, v)| *k == "referrer-policy" && *v == "no-referrer"));
        assert!(pairs
            .iter()
            .any(|(k, v)| *k == "content-type" && *v == "text/html; charset=utf-8"));
    }
}
