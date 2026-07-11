//! Reader for Gemini CLI transcripts under
//! `~/.gemini/tmp/<project-hash>/logs.json`.
//!
//! Gemini is the odd store out. The other two readers (claude, codex) are
//! append-only JSONL files resumed by byte offset; gemini keeps **one
//! `logs.json` per project, a JSON ARRAY it rewrites in place**. There are no
//! byte offsets to resume from, so eligibility is tracked as a processed
//! `sessionId` *set* ([`Watermarks::gemini_seen`]/`gemini_record`) instead of
//! per-file offsets. The whole file is read and grouped by `sessionId` on
//! every scan; a session already in the set is skipped.
//!
//! Format facts this reader depends on (verified against real files on this
//! machine, gemini-cli 0.45.2; re-verify each release):
//!
//! - **`logs.json` is a flat JSON array of user-message records.** Each element
//!   is `{sessionId, messageId, type: "user", message, timestamp}`. `messageId`
//!   is an integer that restarts at 0 per session and increases with each of
//!   that session's messages; `timestamp` is an RFC 3339 UTC "Z" string. Every
//!   record seen carries `type == "user"` (gemini writes only user prompts
//!   here), but the allow-list keeps only `type == "user"` so a future kind is
//!   dropped rather than mined.
//! - **The array order is not assumed chronological.** Sessions can interleave
//!   and the same session can appear across two project dirs. The reader groups
//!   by `sessionId`, orders each session's messages by `messageId`, and takes
//!   the newest `timestamp` per session by explicit max — never "last element
//!   wins".
//! - **There is no interactive-vs-programmatic marker** (unlike claude's
//!   `entrypoint` or codex's `session_meta`). Per Decision #6 the fallback
//!   heuristic is: **skip any session with exactly one user message** — a `-p`
//!   one-shot from another tool looks exactly like a single-message session and
//!   there is no verified field to tell them apart. Fail closed: a real
//!   two-plus-message interactive session that happens to be one message long
//!   is dropped rather than a machine-written one-shot mined.
//! - **`logs.json` carries no `cwd`.** The working directory is encoded only in
//!   the parent directory name, which is the **lowercase hex SHA-256 of the
//!   absolute cwd string** (see [`gemini_project_hash`], verified at
//!   implementation time by hashing known repo paths and matching real
//!   `~/.gemini/tmp/` dir names). Slices therefore carry `cwd: None`; the worker
//!   attributes a slice to a repo by comparing `gemini_project_hash(repo)`
//!   against the hash in the slice's [`SessionSlice::source_file`] path.
//! - **cwd guard.** Gemini has no "write no transcript" flag, so the harvest
//!   worker's own runs land in `~/.gemini/tmp/<hash-of-work-dir>/`. The dir
//!   whose name equals `work_dir_hash` is dropped whole, so the worker never
//!   harvests itself.
//!
//! Fail-closed like the other readers: an unreadable dir, a missing or
//! non-array or unparseable `logs.json`, or a session whose newest timestamp
//! will not parse all yield nothing for the affected unit and never abort the
//! wider scan. Only directories whose name is a 64-char hex project hash are
//! scanned — a legacy basename-layout dir cannot be cwd-guarded or attributed,
//! so it is skipped (harvest less, never harvest something unguardable).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{too_fresh, SessionSlice};
use crate::learn::watermarks::Watermarks;

/// The `<project-hash>` gemini derives for a working directory: the lowercase
/// hex SHA-256 of the absolute path string. Gemini (a Node CLI) hashes the cwd
/// as a UTF-8 string, so this hashes `path.to_string_lossy()` — identical for
/// every real (valid-UTF-8) path. Verified at implementation time against real
/// `~/.gemini/tmp/` directory names.
pub fn gemini_project_hash(path: &Path) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Scan every gemini `logs.json` under `home/.gemini/tmp/<project-hash>/` and
/// return one [`SessionSlice`] per session that is new, quiescent, and has more
/// than one user message.
///
/// Eligibility per session (grouped by `sessionId`):
/// 1. **cwd guard.** The project-hash dir equal to `work_dir_hash` is dropped
///    before any file is read (the worker's own runs).
/// 2. **Processed set.** A `sessionId` already in `marks` (via
///    [`Watermarks::gemini_record`], the worker's job after a successful
///    harvest) is skipped.
/// 3. **Single-message heuristic.** A session with fewer than two user messages
///    is skipped (the `-p` one-shot fallback, Decision #6).
/// 4. **Quiescence.** A session whose newest message timestamp is within
///    [`super::QUIESCENCE`] of `now` is treated as still live and skipped. This
///    is a *per-session* gate on the message timestamp, not the file mtime,
///    because the rewritten array's mtime reflects the newest write across all
///    sessions — an old, finished session sharing a file with a live one must
///    still be harvestable.
///
/// `ts` is the session's newest message timestamp (max, not array order);
/// `messages` are that session's user texts ordered by `messageId`; `cwd` is
/// always `None` (gemini logs carry none); `end_offset` is always `0` (gemini
/// resumes by the session set, not a byte offset).
///
/// Fail-closed: any IO/parse error yields nothing for the affected path and
/// never aborts the wider scan.
pub fn scan_gemini(
    home: &Path,
    marks: &Watermarks,
    now: SystemTime,
    work_dir_hash: &str,
) -> Vec<SessionSlice> {
    let tmp = home.join(".gemini").join("tmp");
    let Ok(entries) = fs::read_dir(&tmp) else {
        return Vec::new();
    };

    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Only real <project-hash> dirs: a 64-char hex name we can both
        // cwd-guard and attribute. Anything else (a legacy basename dir) is
        // skipped fail-closed.
        if !is_project_hash(name) {
            continue;
        }
        // cwd guard: never harvest the worker's own project-hash dir.
        if name == work_dir_hash {
            continue;
        }
        dirs.push(path);
    }
    dirs.sort();

    let mut slices = Vec::new();
    for dir in dirs {
        slices.extend(scan_file(&dir.join("logs.json"), marks, now));
    }
    slices
}

/// A 64-character lowercase-or-upper hex string (a SHA-256 project hash).
fn is_project_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One user message pulled from a `logs.json` entry.
struct Msg {
    message_id: i64,
    ts: String,
    text: String,
}

/// Read and fold one `logs.json` into its eligible session slices. Returns an
/// empty vec (never an error) for a missing/unreadable/non-array/unparseable
/// file.
fn scan_file(logs: &Path, marks: &Watermarks, now: SystemTime) -> Vec<SessionSlice> {
    let Ok(text) = fs::read_to_string(logs) else {
        return Vec::new();
    };
    // A whole-file parse: the store is one JSON document. A non-array top level
    // or any JSON error skips the store (fail closed), the same "harvest less"
    // failure mode as a bad line elsewhere.
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };

    // Group user messages by session id. `Value::get` returns `None` for a
    // non-object element (a bare number, say), so junk entries fall out here.
    let mut sessions: std::collections::BTreeMap<String, Vec<Msg>> =
        std::collections::BTreeMap::new();
    for entry in &entries {
        if entry.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(session_id) = entry.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let Some(message) = entry.get("message").and_then(Value::as_str) else {
            continue;
        };
        if message.trim().is_empty() {
            continue;
        }
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // A missing/absent messageId sorts last (defensive; every real entry
        // has one).
        let message_id = entry
            .get("messageId")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX);
        sessions
            .entry(session_id.to_string())
            .or_default()
            .push(Msg {
                message_id,
                ts,
                text: message.to_string(),
            });
    }

    let mut slices = Vec::new();
    for (session_id, mut msgs) in sessions {
        // Processed-set skip: an already-harvested session is never re-mined.
        if marks.gemini_seen(&session_id) {
            continue;
        }
        // Single-message heuristic (Decision #6): drop <2 user messages.
        if msgs.len() < 2 {
            continue;
        }
        // Oldest-first, by messageId — independent of array order.
        msgs.sort_by_key(|m| m.message_id);

        // Newest timestamp by explicit max over parseable timestamps.
        let mut newest: Option<(SystemTime, String)> = None;
        for m in &msgs {
            let Some(t) = parse_rfc3339(&m.ts) else {
                continue;
            };
            let is_newer = match &newest {
                None => true,
                Some((cur, _)) => t > *cur,
            };
            if is_newer {
                newest = Some((t, m.ts.clone()));
            }
        }
        // No parseable timestamp means no way to gate quiescence — fail closed.
        let Some((newest_time, newest_ts)) = newest else {
            continue;
        };
        // Per-session quiescence: a session whose newest message is still fresh
        // is treated as live and left for a later run.
        if too_fresh(newest_time, now) {
            continue;
        }

        slices.push(SessionSlice {
            agent: "gemini",
            session_id,
            cwd: None,
            ts: newest_ts,
            messages: msgs.into_iter().map(|m| m.text).collect(),
            source_file: logs.to_path_buf(),
            end_offset: 0,
        });
    }
    slices
}

/// Parse an RFC 3339 timestamp to a [`SystemTime`], or `None` if it will not
/// parse (or predates the Unix epoch — impossible for real session data).
fn parse_rfc3339(ts: &str) -> Option<SystemTime> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::new(secs as u64, dt.timestamp_subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::super::QUIESCENCE;
    use super::*;
    use std::fs;
    use std::time::Duration;

    const SESSION_1: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
    const SESSION_2_SINGLE: &str = "22222222-aaaa-4bbb-8ccc-000000000002";
    const SESSION_3: &str = "33333333-aaaa-4bbb-8ccc-000000000003";

    /// Absolute path to a committed gemini fixture.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/learn/gemini")
            .join(name)
    }

    /// Stage a fixture as `<home>/.gemini/tmp/<dir_hash>/logs.json`, returning
    /// `(home_tempdir, staged_logs_path)`. The fixture is always written under
    /// the real `logs.json` name; `dir_hash` names the project-hash directory.
    fn stage(fixture_name: &str, dir_hash: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".gemini/tmp").join(dir_hash);
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("logs.json");
        fs::copy(fixture(fixture_name), &dst).unwrap();
        (home, dst)
    }

    /// A distinct, valid project-hash dir name for staging.
    fn proj_dir() -> String {
        gemini_project_hash(Path::new("/home/dev/synthetic-webapp"))
    }

    fn at(ts: &str) -> SystemTime {
        parse_rfc3339(ts).expect("fixture timestamp parses")
    }

    /// `now` past the quiescence window for every session in `logs.json`
    /// (session 1's newest message is the latest, at 10:10).
    fn now_past_quiescence() -> SystemTime {
        at("2026-06-01T10:10:00.000Z") + QUIESCENCE + Duration::from_secs(60)
    }

    fn empty_marks() -> Watermarks {
        let dir = tempfile::tempdir().unwrap();
        Watermarks::load_from(&dir.path().join("watermarks.json"))
    }

    #[test]
    fn multi_session_file_folds_by_session_id() {
        let (home, staged) = stage("logs.json", &proj_dir());
        let marks = empty_marks();
        let slices = scan_gemini(home.path(), &marks, now_past_quiescence(), "");

        assert_eq!(
            slices.len(),
            2,
            "sessions 1 and 3 eligible; 2 is single-message"
        );

        let s1 = slices
            .iter()
            .find(|s| s.session_id == SESSION_1)
            .expect("session 1 present");
        assert_eq!(s1.agent, "gemini");
        assert_eq!(
            s1.messages,
            vec![
                "Always use tabs, not spaces, for indentation in this project.".to_string(),
                "Prefer pnpm over npm for every install.".to_string(),
                "Run the linter before every commit, no exceptions.".to_string(),
            ],
            "grouped by sessionId and ordered by messageId, independent of the array order"
        );
        assert_eq!(
            s1.ts, "2026-06-01T10:10:00.000Z",
            "newest per-session timestamp, taken as an explicit max, not last-in-array"
        );
        assert_eq!(s1.cwd, None, "gemini logs.json carries no cwd");
        assert_eq!(s1.source_file, staged);
        assert_eq!(
            s1.end_offset, 0,
            "gemini has no byte offset; it resumes via the processed-session set"
        );

        let s3 = slices
            .iter()
            .find(|s| s.session_id == SESSION_3)
            .expect("session 3 present");
        assert_eq!(
            s3.messages,
            vec![
                "Write a test for every bug fix.".to_string(),
                "Keep functions under fifty lines.".to_string(),
            ]
        );
        assert_eq!(s3.ts, "2026-06-01T08:30:00.000Z");
    }

    #[test]
    fn single_message_session_is_excluded() {
        let (home, _staged) = stage("logs.json", &proj_dir());
        let marks = empty_marks();
        let slices = scan_gemini(home.path(), &marks, now_past_quiescence(), "");
        assert!(
            slices.iter().all(|s| s.session_id != SESSION_2_SINGLE),
            "a session with exactly one user message is dropped (the -p one-shot heuristic)"
        );
    }

    #[test]
    fn processed_session_is_not_reharvested() {
        let (home, _staged) = stage("logs.json", &proj_dir());
        let mut marks = empty_marks();
        marks.gemini_record(SESSION_1);
        let slices = scan_gemini(home.path(), &marks, now_past_quiescence(), "");

        assert_eq!(
            slices.len(),
            1,
            "session 1 already processed; session 3 remains"
        );
        assert_eq!(slices[0].session_id, SESSION_3);
    }

    #[test]
    fn work_dir_hash_directory_is_dropped_whole() {
        let work = gemini_project_hash(Path::new("/work/learn"));
        let (home, _staged) = stage("logs.json", &work);
        let marks = empty_marks();
        let slices = scan_gemini(home.path(), &marks, now_past_quiescence(), &work);
        assert!(
            slices.is_empty(),
            "the worker's own project-hash dir is dropped (gemini has no no-persist flag)"
        );
    }

    #[test]
    fn quiescence_is_per_session_not_per_file() {
        // `now` is one minute after session 1's newest message but ~1h40m
        // after session 3's — the two share one file. A per-file mtime gate
        // would block both; the per-session gate harvests only the old one.
        let (home, _staged) = stage("logs.json", &proj_dir());
        let marks = empty_marks();
        let now = at("2026-06-01T10:10:00.000Z") + Duration::from_secs(60);
        let slices = scan_gemini(home.path(), &marks, now, "");

        assert_eq!(
            slices.len(),
            1,
            "session 1 is still live; session 3 is quiescent"
        );
        assert_eq!(slices[0].session_id, SESSION_3);
    }

    #[test]
    fn malformed_logs_json_yields_nothing() {
        let (home, _staged) = stage("logs-malformed.json", &proj_dir());
        let marks = empty_marks();
        let slices = scan_gemini(home.path(), &marks, SystemTime::now(), "");
        assert!(
            slices.is_empty(),
            "a logs.json that is not valid JSON is skipped wholesale (fail closed)"
        );
    }

    #[test]
    fn junk_entries_are_skipped_but_valid_session_survives() {
        let (home, _staged) = stage("logs-mixed.json", &proj_dir());
        let marks = empty_marks();
        let now = at("2026-06-01T08:04:00.000Z") + QUIESCENCE + Duration::from_secs(60);
        let slices = scan_gemini(home.path(), &marks, now, "");

        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].session_id, "bbbbbbbb-aaaa-4bbb-8ccc-000000000002");
        assert_eq!(
            slices[0].messages,
            vec![
                "Document every public function.".to_string(),
                "Never leave a failing test in main.".to_string(),
            ],
            "a bare number, a message-less entry, a non-user entry, and a whitespace-only \
             message are all skipped; the two valid user messages survive"
        );
    }

    #[test]
    fn legacy_basename_dir_is_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".gemini/tmp/zerminal");
        fs::create_dir_all(&dir).unwrap();
        fs::copy(fixture("logs.json"), dir.join("logs.json")).unwrap();
        let slices = scan_gemini(home.path(), &empty_marks(), now_past_quiescence(), "");
        assert!(
            slices.is_empty(),
            "a non-<project-hash> dir (legacy basename layout) can't be cwd-guarded or attributed, so it is skipped"
        );
    }

    #[test]
    fn missing_tmp_dir_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        let slices = scan_gemini(home.path(), &empty_marks(), SystemTime::now(), "");
        assert!(slices.is_empty(), "no ~/.gemini/tmp is not an error");
    }

    #[test]
    fn project_hash_matches_verified_formula() {
        // Locked to the verified formula: lowercase hex SHA-256 of the absolute
        // path string (matched against real ~/.gemini/tmp dir names at
        // implementation time).
        assert_eq!(
            gemini_project_hash(Path::new("/home/dev/synthetic-webapp")),
            "f4626ae21c1df70c9a45039f6cc9ab3d6371ace85868ff998a4326b40d9523ae"
        );
        let h = gemini_project_hash(Path::new("/work/learn"));
        assert_eq!(
            h,
            "35c83a93615faa49f665661e7c4817b1a7fb047aef8f12f5c5d3ea5c7329b3d7"
        );
        assert_eq!(h.len(), 64, "sha256 hex is 64 chars");
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only"
        );
    }
}
