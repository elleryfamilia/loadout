//! Per-machine watermark store: how far the harvest worker has read into
//! each transcript source, so a run only mines what is new.
//!
//! Lives in the per-machine state dir (`state_dir()/learn/watermarks.json`,
//! see [`crate::learn::state::learn_dir`]) — never synced, same placement
//! rule as the rest of [`crate::learn::state`] (contrast
//! [`crate::learn::journal`], which syncs by design).
//!
//! Copies the shape of the script trust store ([`crate::trust`]): a small
//! versioned JSON file, missing = fresh start, a parse failure or a
//! newer-than-known schema version is refused loudly as "corrupt" rather
//! than silently reset or misread, and `save` is a no-op while corrupt so a
//! damaged file is never overwritten out from under a human trying to
//! inspect or recover it. `load learn reset` deletes the file outright
//! (`reset`), which is the supported recovery path — the next run starts
//! from a fresh baseline rather than the store trying to self-heal.
//!
//! ## The monotonic-advance rule
//!
//! [`Watermarks::advance`] never lets a file's recorded offset or mtime go
//! backwards — it always stores `max(current, new)`. This is a deliberate
//! backstop, not an optimization: the harvest worker's lock uses a fencing
//! token (see the design doc) so that a worker instance which resumes after
//! its lock was reclaimed aborts before writing anything. But fencing is
//! only as good as every call site remembering to check the token, and a
//! bug in one of them — a missed check, a race, a future refactor — must
//! not be able to turn into double-harvested content (the same session
//! re-sent to a paid extraction call, or the same preference journaled
//! twice). By making the watermark itself monotonic, a stray call that
//! passes a stale/smaller offset is harmless: the recorded position can
//! only ever move forward. This is the last line of defense the design doc
//! refers to, independent of and in addition to the lock fencing.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version of the on-disk store. A store written by a newer loadout
/// (a higher version) is refused rather than misread, same rationale as
/// [`crate::trust`]'s `STORE_VERSION`: an older binary can't know a newer
/// schema's meaning, and misjudging "already harvested" could either skip
/// real content or double-harvest it.
const VERSION: u32 = 1;

/// How far the worker has read into one append-only transcript file
/// (claude/codex JSONL sources resume by byte offset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMark {
    /// Bytes of the file already scanned; the next run resumes from here.
    pub bytes_processed: u64,
    /// The file's mtime (unix seconds) as of the run that set
    /// `bytes_processed` — used to detect a file that shrank/rotated out
    /// from under the recorded offset.
    pub mtime_seen: i64,
}

/// The on-disk shape. Private: callers only ever see it through
/// [`Watermarks`]'s methods, same pattern as `TrustStore`/`StoreFile`.
#[derive(Debug, Serialize, Deserialize)]
struct WatermarkFile {
    version: u32,
    /// Append-only sources (claude, codex): path/key -> read position.
    files: BTreeMap<String, FileMark>,
    /// Gemini's `logs.json` is a rewritten array, not append-only, so it is
    /// tracked as a processed-session-id set instead of an offset.
    gemini_sessions: BTreeSet<String>,
    /// RFC 3339 UTC timestamp of the first `load learn on` on this machine
    /// (or the most recent `reset`) — the 14-day age cutoff anchors here,
    /// set once and never moved by ordinary runs.
    baseline: Option<String>,
}

impl Default for WatermarkFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            files: BTreeMap::new(),
            gemini_sessions: BTreeSet::new(),
            baseline: None,
        }
    }
}

/// The watermark store: load once per run, advance as sources are scanned,
/// save at the end. Missing file = normal first run (empty, not corrupt). A
/// file that exists but fails to parse, or that declares a schema version
/// newer than this binary knows, is corrupt: NEVER treated as empty, NEVER
/// silently reset.
#[derive(Debug)]
pub struct Watermarks {
    path: PathBuf,
    file: WatermarkFile,
    corrupt: bool,
}

impl Watermarks {
    /// Load from an explicit path (the real per-machine store, or a
    /// `tempdir()` path in tests).
    pub fn load_from(path: &Path) -> Self {
        let (file, corrupt) = match std::fs::read_to_string(path) {
            Err(_) => (WatermarkFile::default(), false),
            Ok(text) => match serde_json::from_str::<WatermarkFile>(&text) {
                // A store written by a newer loadout is refused, not
                // misread — treated like corruption (loud, save refused,
                // `reset` recovers) rather than silently reinterpreted.
                Ok(f) if f.version > VERSION => (WatermarkFile::default(), true),
                Ok(f) => (f, false),
                Err(_) => (WatermarkFile::default(), true),
            },
        };
        Self {
            path: path.to_path_buf(),
            file,
            corrupt,
        }
    }

    /// Whether the store on disk was unreadable or from a newer schema.
    /// Callers should warn loudly and steer the user at `load learn reset`
    /// rather than acting as if this were a fresh (empty) store.
    pub fn corrupt(&self) -> bool {
        self.corrupt
    }

    /// The recorded position for one append-only source, if any.
    pub fn mark(&self, file: &str) -> Option<&FileMark> {
        self.file.files.get(file)
    }

    /// Record a new read position for `file`, monotonically: the stored
    /// `bytes_processed`/`mtime_seen` become `max(current, new)`, never
    /// regressing. See the module docs for why this must never be a plain
    /// overwrite. Refused (no-op) while the store is corrupt — like
    /// `trust.rs::record`, corruption blocks the mutation itself, not just
    /// the save: a corrupt load leaves an in-memory default, and marks
    /// fabricated on top of it would read back through
    /// [`Watermarks::mark`] as if they were legitimate resume points.
    pub fn advance(&mut self, file: &str, offset: u64, mtime: i64) {
        if self.corrupt {
            return;
        }
        let entry = self.file.files.entry(file.to_string()).or_default();
        entry.bytes_processed = entry.bytes_processed.max(offset);
        entry.mtime_seen = entry.mtime_seen.max(mtime);
    }

    /// Shrink-recovery exception to the monotonic-advance rule
    /// ([`Watermarks::advance`]): set `file`'s mark to exactly
    /// `(offset, mtime)`, allowing it to move **backwards**. This is the ONE
    /// sanctioned way a mark regresses, and it exists for a single scenario
    /// (cross-task contract C8): a reader re-read a file from byte 0 because
    /// the recorded offset was past the current end (the file shrank, was
    /// truncated, or was rotated out and replaced by a shorter one — the
    /// reader reports this via [`crate::learn::readers::SessionSlice::rewound`]).
    /// Without this, monotonic `advance` would keep the old, too-large offset;
    /// every future run would then see `recorded > len`, rewind to 0, and
    /// re-harvest — and re-pay for — the whole shrunk file forever. Resetting
    /// the mark down to the freshly-observed end offset stops that unbounded
    /// paid re-harvest.
    ///
    /// Used ONLY on the reader's shrink signal; ordinary advances still go
    /// through [`Watermarks::advance`] and can never regress. Refused (no-op)
    /// while the store is corrupt, for the same reason as `advance`.
    pub fn reset_file(&mut self, file: &str, offset: u64, mtime: i64) {
        if self.corrupt {
            return;
        }
        self.file.files.insert(
            file.to_string(),
            FileMark {
                bytes_processed: offset,
                mtime_seen: mtime,
            },
        );
    }

    /// Every append-only file key currently recorded (claude/codex). The
    /// harvest worker uses this to build the `existing_files` set passed to
    /// [`Watermarks::save`]: it checks each known key for on-disk existence
    /// and keeps only the survivors, which is how marks for deleted files get
    /// pruned (critic MINOR-4) without the worker having to re-walk every
    /// transcript store itself.
    pub fn known_files(&self) -> Vec<String> {
        self.file.files.keys().cloned().collect()
    }

    /// Whether a gemini session id has already been harvested.
    pub fn gemini_seen(&self, session_id: &str) -> bool {
        self.file.gemini_sessions.contains(session_id)
    }

    /// Record a gemini session id as harvested. Idempotent: recording the
    /// same id twice is a no-op (the set can only grow, never shrink), the
    /// same monotonic spirit as [`Watermarks::advance`]. Refused (no-op)
    /// while the store is corrupt, for the same reason as `advance`.
    pub fn gemini_record(&mut self, session_id: &str) {
        if self.corrupt {
            return;
        }
        self.file.gemini_sessions.insert(session_id.to_string());
    }

    /// The 14-day-cutoff baseline timestamp (RFC 3339 UTC), if set.
    pub fn baseline(&self) -> Option<&str> {
        self.file.baseline.as_deref()
    }

    /// Set the baseline only if it is not already set — the first
    /// `load learn on` (or the first run after a `reset`) fixes it; later
    /// calls must never move it. Refused (no-op) while the store is
    /// corrupt, for the same reason as `advance` — the on-disk store may
    /// hold a real baseline this in-memory default can't see.
    pub fn set_baseline_if_absent(&mut self, ts: &str) {
        if self.corrupt {
            return;
        }
        if self.file.baseline.is_none() {
            self.file.baseline = Some(ts.to_string());
        }
    }

    /// Prune entries for files that no longer exist, then persist. Refused
    /// (no-op, `Ok(())`) while the store is corrupt — recovery goes through
    /// [`Watermarks::reset`], never through overwriting a damaged file with
    /// an in-memory default.
    pub fn save(&mut self, existing_files: &BTreeSet<String>) -> io::Result<()> {
        if self.corrupt {
            return Ok(());
        }
        self.file.files.retain(|f, _| existing_files.contains(f));
        let body = serde_json::to_string_pretty(&self.file).map_err(io::Error::other)?;
        crate::writer::atomic_write(&self.path, &body).map_err(io::Error::other)
    }

    /// Delete the store outright (`load learn reset`). Missing file is not
    /// an error. The next [`Watermarks::load_from`] starts fresh — a new
    /// baseline gets set on the next eligible run.
    pub fn reset(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_store_is_fresh_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let wm = Watermarks::load_from(&dir.path().join("watermarks.json"));
        assert!(!wm.corrupt());
        assert!(wm.mark("a.jsonl").is_none());
        assert!(wm.baseline().is_none());
    }

    #[test]
    fn advance_with_smaller_offset_keeps_the_max() {
        let dir = tempfile::tempdir().unwrap();
        let mut wm = Watermarks::load_from(&dir.path().join("watermarks.json"));

        wm.advance("a.jsonl", 500, 1_000);
        assert_eq!(
            wm.mark("a.jsonl"),
            Some(&FileMark {
                bytes_processed: 500,
                mtime_seen: 1_000
            })
        );

        // A fencing bug (or any stale/racing caller) tries to regress the
        // offset and mtime — both must stay at their prior maximum.
        wm.advance("a.jsonl", 100, 200);
        assert_eq!(
            wm.mark("a.jsonl"),
            Some(&FileMark {
                bytes_processed: 500,
                mtime_seen: 1_000
            }),
            "a smaller offset/mtime must never regress the recorded mark"
        );

        // A genuine advance still moves it forward.
        wm.advance("a.jsonl", 900, 2_000);
        assert_eq!(
            wm.mark("a.jsonl"),
            Some(&FileMark {
                bytes_processed: 900,
                mtime_seen: 2_000
            })
        );
    }

    #[test]
    fn reset_file_moves_a_mark_backwards_for_shrink_recovery() {
        // Contract C8: a file that shrank (recorded offset past the new end)
        // must be able to move DOWN to the freshly-observed end, or every
        // future run rewinds-to-0 and re-harvests the shrunk file forever.
        let dir = tempfile::tempdir().unwrap();
        let mut wm = Watermarks::load_from(&dir.path().join("watermarks.json"));

        wm.advance("a.jsonl", 500, 1_000);
        // A plain advance refuses to regress (monotonic backstop)…
        wm.advance("a.jsonl", 200, 900);
        assert_eq!(wm.mark("a.jsonl").unwrap().bytes_processed, 500);
        // …but reset_file is the sanctioned shrink exception: it moves down.
        wm.reset_file("a.jsonl", 200, 900);
        assert_eq!(
            wm.mark("a.jsonl"),
            Some(&FileMark {
                bytes_processed: 200,
                mtime_seen: 900
            }),
            "reset_file must set the mark to exactly the new (smaller) offset"
        );
    }

    #[test]
    fn reset_file_is_a_no_op_on_a_corrupt_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mut wm = Watermarks::load_from(&path);
        assert!(wm.corrupt());
        wm.reset_file("a.jsonl", 10, 10);
        assert!(
            wm.mark("a.jsonl").is_none(),
            "reset_file on a corrupt store must not fabricate a mark"
        );
    }

    #[test]
    fn known_files_lists_recorded_append_only_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut wm = Watermarks::load_from(&dir.path().join("watermarks.json"));
        assert!(wm.known_files().is_empty());
        wm.advance("a.jsonl", 10, 10);
        wm.advance("b.jsonl", 20, 20);
        let mut keys = wm.known_files();
        keys.sort();
        assert_eq!(keys, vec!["a.jsonl".to_string(), "b.jsonl".to_string()]);
    }

    #[test]
    fn newer_version_file_loads_corrupt_and_save_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");
        let newer = r#"{"version":999,"files":{},"gemini_sessions":[],"baseline":null}"#;
        std::fs::write(&path, newer).unwrap();

        let mut wm = Watermarks::load_from(&path);
        assert!(wm.corrupt());

        // Refused, not overwritten: the newer store's bytes are preserved
        // exactly, same as the trust store's refusal semantics.
        wm.advance("a.jsonl", 10, 10);
        wm.save(&set(&["a.jsonl"])).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn mutators_are_no_ops_on_a_corrupt_store() {
        // A corrupt load leaves an in-memory default; mutation must be
        // refused too (like trust.rs::record), not just the save — otherwise
        // a caller that mutates before checking corrupt() fabricates marks
        // this run, and mark()/gemini_seen()/baseline() report them back as
        // if they were legitimate resume points (the "silent misread" the
        // design forbids).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");
        let newer = r#"{"version":999,"files":{},"gemini_sessions":[],"baseline":null}"#;
        std::fs::write(&path, newer).unwrap();

        let mut wm = Watermarks::load_from(&path);
        assert!(wm.corrupt());

        wm.advance("a.jsonl", 500, 1_000);
        wm.gemini_record("sess-1");
        wm.set_baseline_if_absent("2026-01-01T00:00:00Z");

        assert!(
            wm.mark("a.jsonl").is_none(),
            "advance on a corrupt store must not fabricate a mark"
        );
        assert!(
            !wm.gemini_seen("sess-1"),
            "gemini_record on a corrupt store must not record"
        );
        assert!(
            wm.baseline().is_none(),
            "set_baseline_if_absent on a corrupt store must not set"
        );
    }

    #[test]
    fn malformed_json_loads_corrupt_and_save_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");
        std::fs::write(&path, "{ not json").unwrap();

        let mut wm = Watermarks::load_from(&path);
        assert!(wm.corrupt());
        wm.save(&BTreeSet::new()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn save_prunes_marks_for_files_that_no_longer_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");
        let mut wm = Watermarks::load_from(&path);

        wm.advance("kept.jsonl", 10, 10);
        wm.advance("deleted.jsonl", 20, 20);
        wm.save(&set(&["kept.jsonl"])).unwrap();

        let reloaded = Watermarks::load_from(&path);
        assert!(reloaded.mark("kept.jsonl").is_some());
        assert!(
            reloaded.mark("deleted.jsonl").is_none(),
            "a mark for a file absent from existing_files must be pruned on save"
        );
    }

    #[test]
    fn baseline_is_set_once_and_never_moved() {
        let dir = tempfile::tempdir().unwrap();
        let mut wm = Watermarks::load_from(&dir.path().join("watermarks.json"));

        wm.set_baseline_if_absent("2026-01-01T00:00:00Z");
        assert_eq!(wm.baseline(), Some("2026-01-01T00:00:00Z"));

        // A later call must not move an already-set baseline.
        wm.set_baseline_if_absent("2026-06-01T00:00:00Z");
        assert_eq!(wm.baseline(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn gemini_sessions_are_recorded_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut wm = Watermarks::load_from(&dir.path().join("watermarks.json"));

        assert!(!wm.gemini_seen("sess-1"));
        wm.gemini_record("sess-1");
        assert!(wm.gemini_seen("sess-1"));

        // Recording the same session again is a harmless no-op.
        wm.gemini_record("sess-1");
        assert!(wm.gemini_seen("sess-1"));
        assert!(!wm.gemini_seen("sess-2"));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");

        let mut wm = Watermarks::load_from(&path);
        wm.advance("a.jsonl", 500, 1_000);
        wm.gemini_record("sess-1");
        wm.set_baseline_if_absent("2026-01-01T00:00:00Z");
        wm.save(&set(&["a.jsonl"])).unwrap();

        let reloaded = Watermarks::load_from(&path);
        assert!(!reloaded.corrupt());
        assert_eq!(
            reloaded.mark("a.jsonl"),
            Some(&FileMark {
                bytes_processed: 500,
                mtime_seen: 1_000
            })
        );
        assert!(reloaded.gemini_seen("sess-1"));
        assert_eq!(reloaded.baseline(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn reset_deletes_the_file_and_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermarks.json");

        let mut wm = Watermarks::load_from(&path);
        wm.advance("a.jsonl", 500, 1_000);
        wm.save(&set(&["a.jsonl"])).unwrap();
        assert!(path.exists());

        Watermarks::reset(&path).unwrap();
        assert!(!path.exists());

        // Deleting an already-absent store is a no-op, not an error.
        Watermarks::reset(&path).unwrap();

        let fresh = Watermarks::load_from(&path);
        assert!(!fresh.corrupt());
        assert!(fresh.mark("a.jsonl").is_none());
    }
}
