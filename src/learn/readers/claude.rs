//! Reader for Claude Code transcripts under `~/.claude/projects/*/*.jsonl`.
//!
//! One JSONL file per session (the file stem is the session id). Lines are
//! append-only records of assorted `type`s; only a narrow allow-list of them
//! is user-authored interactive text. See [`scan_claude`] for the exact gate.
//!
//! Format facts this reader depends on (verified against real files on a
//! developer machine, Claude Code v2.1.x; re-verify each release):
//! - A user prompt is `type == "user"` with `userType == "external"`. Tool
//!   results arrive as `type == "user"` too, so `userType` alone is not
//!   enough — they are dropped because their `message.content` carries no
//!   `text` parts (only `tool_result`), leaving no text to harvest.
//! - `isMeta == true` marks injected/system-summary user lines; `isSidechain
//!   == true` marks subagent (Task) lines. Both are excluded.
//! - `entrypoint` rides on every user/assistant line, not just a header, and
//!   is uniform within a file. Interactive sessions use `cli` (or
//!   `claude-desktop`); programmatic ones use `sdk-cli`/`sdk-ts`. Any line
//!   whose `entrypoint` is outside {`cli`, `claude-desktop`} disqualifies the
//!   whole file. Because it rides on the user lines themselves, this stays
//!   correct even when a scan resumes past the file header.
//! - Every real file ends with a trailing newline, so a final line *without*
//!   one is a session caught mid-write: it is never consumed (see the
//!   `end_offset` discipline in [`scan_claude`]).

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::{resume_start, too_fresh, SessionSlice};
use crate::learn::watermarks::Watermarks;

/// Scan every Claude Code transcript under `home/.claude/projects/*/*.jsonl`
/// and return one [`SessionSlice`] per session that has new, eligible user
/// text.
///
/// Per file, in order:
/// 1. **Quiescence.** Skip a file modified more recently than [`QUIESCENCE`]
///    ago (relative to `now`) unless its session id is in `hooked` — a
///    session-end hook fired for it, so we know it is over. `hooked` bypasses
///    *only* the quiescence wait, never the exclusions below.
/// 2. **Resume.** Start reading at `marks.mark(path).bytes_processed`. If the
///    file is now shorter than that offset (it was truncated or replaced),
///    the recorded position is meaningless, so re-read from the start rather
///    than seek past the end and silently lose the file.
/// 3. **Parse leniently.** Split into newline-terminated lines; a line that
///    fails to parse as JSON is skipped, the rest of the file is not. A
///    trailing chunk with no terminating newline is a partial final line and
///    is not consumed at all.
/// 4. **Session-level exclusion.** If any parsed line carries an `entrypoint`
///    outside {`cli`, `claude-desktop`}, the whole file is dropped (no slice).
/// 5. **Message inclusion.** Keep a line's text iff `type == "user"` and
///    `userType == "external"` and not `isMeta` and not `isSidechain`. The
///    text is `message.content` when it is a string, else the `text` parts of
///    a content array joined by newlines. Empty results (e.g. a tool-result
///    line) contribute nothing.
///
/// `cwd` is taken from the first scanned line that has one; `ts` is the newest
/// included message's timestamp; `end_offset` is one byte past the last fully
/// parsed line. A file that yields no eligible message produces no slice (and
/// so leaves its watermark unadvanced, to be re-examined cheaply next run).
///
/// Fail-closed: any IO error (missing dir, unreadable file) yields nothing for
/// the affected path and never aborts the wider scan.
pub fn scan_claude(
    home: &Path,
    marks: &Watermarks,
    now: SystemTime,
    hooked: &BTreeSet<String>,
) -> Vec<SessionSlice> {
    let projects = home.join(".claude").join("projects");
    let mut files = Vec::new();
    collect_jsonl(&projects, &mut files);
    files.sort();

    let mut slices = Vec::new();
    for path in files {
        if let Some(slice) = scan_one(&path, marks, now, hooked) {
            slices.push(slice);
        }
    }
    slices
}

/// Collect `projects/*/*.jsonl` into `out`. Unreadable directories are
/// skipped silently (fail-closed): a store we cannot list yields nothing.
fn collect_jsonl(projects: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(projects) else {
        return;
    };
    for project in entries.flatten() {
        let dir = project.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
}

/// Scan a single transcript file. Returns `None` when the file is too fresh,
/// excluded, unreadable, or simply has no new eligible user text.
fn scan_one(
    path: &Path,
    marks: &Watermarks,
    now: SystemTime,
    hooked: &BTreeSet<String>,
) -> Option<SessionSlice> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let session_id = path.file_stem()?.to_string_lossy().into_owned();

    // Quiescence: a file still being written to (modified within QUIESCENCE)
    // is skipped unless a hook told us this session just ended.
    if !hooked.contains(&session_id) {
        let mtime = meta.modified().ok()?;
        if too_fresh(mtime, now) {
            return None;
        }
    }

    let key = path.to_string_lossy();
    let mark = marks.mark(&key).copied().unwrap_or_default();
    // Staleness guard: a recorded offset past the current end means the file
    // was truncated or replaced. Seeking there would read nothing and strand
    // us forever, so re-read from the top instead (see [`super::resume_start`]).
    // (`mark.mtime_seen` is corroborating context recorded by the worker; the
    // length comparison is the authoritative, unambiguous trigger.)
    let start = resume_start(mark.bytes_processed, meta.len());
    let rewound = super::was_rewound(mark.bytes_processed, meta.len());

    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;

    let mut messages: Vec<String> = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut newest_ts: Option<String> = None;
    let mut consumed: usize = 0; // bytes of complete (newline-terminated) lines

    let mut idx = 0usize;
    while let Some(rel) = buf[idx..].iter().position(|&b| b == b'\n') {
        let line = &buf[idx..idx + rel];
        idx += rel + 1;
        consumed = idx; // this line is complete; safe to resume past it

        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue; // bad line: skip the line, keep the file
        };

        // Session-level exclusion: one programmatic entrypoint disqualifies
        // the entire file. Because entrypoint rides on the user lines, this is
        // caught even when we resumed past the header.
        if let Some(ep) = value.get("entrypoint").and_then(Value::as_str) {
            if ep != "cli" && ep != "claude-desktop" {
                return None;
            }
        }

        if cwd.is_none() {
            if let Some(c) = value.get("cwd").and_then(Value::as_str) {
                if !c.is_empty() {
                    cwd = Some(PathBuf::from(c));
                }
            }
        }

        if is_included_user_line(&value) {
            if let Some(text) = user_text(&value) {
                if !text.trim().is_empty() {
                    if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
                        newest_ts = Some(ts.to_string());
                    }
                    messages.push(text);
                }
            }
        }
    }

    if messages.is_empty() {
        return None;
    }

    Some(SessionSlice {
        agent: "claude",
        session_id,
        cwd,
        ts: newest_ts.unwrap_or_default(),
        messages,
        source_file: path.to_path_buf(),
        end_offset: start + consumed as u64,
        rewound,
    })
}

/// A line is user-authored interactive text iff it is a `user` line from an
/// `external` author that is neither a meta/summary injection nor a subagent
/// (sidechain) line. Absent `isMeta`/`isSidechain` count as false.
fn is_included_user_line(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("user")
        && value.get("userType").and_then(Value::as_str) == Some("external")
        && value.get("isMeta").and_then(Value::as_bool) != Some(true)
        && value.get("isSidechain").and_then(Value::as_bool) != Some(true)
}

/// Extract the user's text from a `user` line: `message.content` verbatim when
/// it is a string, otherwise the `text` parts of a content array joined by
/// newlines. Non-text parts (`tool_result`, `image`, …) contribute nothing,
/// which is how tool-result lines fall out to empty.
fn user_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
            }
            Some(text)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use super::super::QUIESCENCE;

    /// Absolute path to a committed fixture.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/learn/claude")
            .join(name)
    }

    /// Stage a fixture into a `home`-shaped tempdir as
    /// `<home>/.claude/projects/<slug>/<session_id>.jsonl`, returning
    /// `(home_tempdir, staged_path)`. The reader derives the session id from
    /// the file stem, so `session_id` names the staged file.
    fn stage(fixture_name: &str, session_id: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let proj = home
            .path()
            .join(".claude/projects/-home-dev-synthetic-webapp");
        fs::create_dir_all(&proj).unwrap();
        let dst = proj.join(format!("{session_id}.jsonl"));
        fs::copy(fixture(fixture_name), &dst).unwrap();
        (home, dst)
    }

    /// A `now` far enough after the staged file's mtime that quiescence has
    /// elapsed (so the file is harvestable).
    fn now_past_quiescence(staged: &Path) -> SystemTime {
        let mtime = fs::metadata(staged).unwrap().modified().unwrap();
        mtime + QUIESCENCE + Duration::from_secs(60)
    }

    /// A `now` only seconds after the staged file's mtime (still within
    /// quiescence, so the file looks live).
    fn now_within_quiescence(staged: &Path) -> SystemTime {
        let mtime = fs::metadata(staged).unwrap().modified().unwrap();
        mtime + Duration::from_secs(60)
    }

    fn empty_marks() -> Watermarks {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir path via a stable temp file path; load_from on a
        // missing file yields a fresh, non-corrupt store.
        Watermarks::load_from(&dir.path().join("watermarks.json"))
    }

    #[test]
    fn interactive_file_yields_its_three_user_messages() {
        let (home, staged) = stage("interactive.jsonl", "sess-interactive");
        let marks = empty_marks();
        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );

        assert_eq!(slices.len(), 1, "one interactive session, one slice");
        let s = &slices[0];
        assert_eq!(s.agent, "claude");
        assert_eq!(s.session_id, "sess-interactive");
        assert_eq!(
            s.messages,
            vec![
                "Always use tabs, not spaces, for indentation in this project.".to_string(),
                "Prefer pnpm over npm for every install.".to_string(),
                "Run the linter before every commit, no exceptions.".to_string(),
            ],
            "tool-result, isMeta, and assistant lines are all excluded"
        );
        assert_eq!(s.cwd, Some(PathBuf::from("/home/dev/synthetic-webapp")));
        assert_eq!(
            s.ts, "2026-06-01T10:10:00.000Z",
            "newest included message ts"
        );
        assert_eq!(s.source_file, staged);
        assert_eq!(
            s.end_offset,
            fs::metadata(&staged).unwrap().len(),
            "a fully newline-terminated file is consumed to its end"
        );
    }

    #[test]
    fn sdk_session_is_excluded_whole() {
        let (home, staged) = stage("sdk.jsonl", "sess-sdk");
        let marks = empty_marks();
        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert!(
            slices.is_empty(),
            "an entrypoint of sdk-cli disqualifies the whole file"
        );
    }

    #[test]
    fn sidechain_file_yields_nothing() {
        let (home, staged) = stage("sidechain.jsonl", "sess-sidechain");
        let marks = empty_marks();
        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert!(
            slices.is_empty(),
            "subagent (isSidechain) user lines are never harvested"
        );
    }

    #[test]
    fn malformed_line_skips_only_that_line() {
        let (home, staged) = stage("malformed.jsonl", "sess-malformed");
        let marks = empty_marks();
        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec![
                "Keep functions under fifty lines.".to_string(),
                "Write a test for every bug fix.".to_string(),
            ],
            "the malformed middle line is skipped; the valid lines survive"
        );
    }

    #[test]
    fn fresh_file_is_skipped_unless_hooked() {
        let (home, staged) = stage("interactive.jsonl", "sess-fresh");
        let marks = empty_marks();

        // Within quiescence, not hooked: skipped.
        let slices = scan_claude(
            home.path(),
            &marks,
            now_within_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert!(slices.is_empty(), "a just-modified file is treated as live");

        // Same file, same too-fresh `now`, but the session is hook-named:
        // quiescence is bypassed and it is harvested.
        let hooked: BTreeSet<String> = ["sess-fresh".to_string()].into_iter().collect();
        let slices = scan_claude(home.path(), &marks, now_within_quiescence(&staged), &hooked);
        assert_eq!(
            slices.len(),
            1,
            "a hook-named session bypasses the quiescence wait"
        );
    }

    #[test]
    fn resume_from_offset_reads_only_new_messages() {
        let (home, staged) = stage("resume.jsonl", "sess-resume");

        // Offset one byte past the first user line's terminating newline.
        let bytes = fs::read(&staged).unwrap();
        let first_nl = bytes.iter().position(|&b| b == b'\n').unwrap();
        let resume_at = (first_nl + 1) as u64;

        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), resume_at, 0);

        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec![
                "Message two about branch naming.".to_string(),
                "Message three about PR descriptions.".to_string(),
            ],
            "the already-recorded first message is not re-read"
        );
        assert_eq!(
            slices[0].end_offset,
            bytes.len() as u64,
            "end_offset advances to the end of the fully consumed file"
        );
    }

    #[test]
    fn resume_at_eof_yields_nothing() {
        let (home, staged) = stage("resume.jsonl", "sess-eof");
        let len = fs::metadata(&staged).unwrap().len();
        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), len, 0);

        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert!(
            slices.is_empty(),
            "a fully consumed file has nothing new to offer"
        );
    }

    #[test]
    fn partial_final_line_is_never_consumed() {
        let (home, staged) = stage("partial-tail.jsonl", "sess-partial");
        let marks = empty_marks();
        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );

        let full_len = fs::metadata(&staged).unwrap().len();
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec!["Complete message before the crash.".to_string()],
            "the truncated final line contributes no message"
        );
        // end_offset must stop at the newline boundary, strictly before EOF,
        // so the next run reparses the (by then completed) final line.
        let bytes = fs::read(&staged).unwrap();
        let boundary = (bytes.iter().position(|&b| b == b'\n').unwrap() + 1) as u64;
        assert_eq!(
            slices[0].end_offset, boundary,
            "end_offset stops after the last complete line, not mid-line"
        );
        assert!(slices[0].end_offset < full_len);
    }

    #[test]
    fn offset_past_eof_rereads_from_start() {
        let (home, staged) = stage("interactive.jsonl", "sess-shrunk");
        let len = fs::metadata(&staged).unwrap().len();
        // A recorded offset beyond the current end (file truncated/replaced).
        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), len + 10_000, 0);

        let slices = scan_claude(
            home.path(),
            &marks,
            now_past_quiescence(&staged),
            &BTreeSet::new(),
        );
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages.len(),
            3,
            "a stale over-long offset is discarded and the file re-read from 0"
        );
        assert_eq!(slices[0].end_offset, len);
    }

    #[test]
    fn missing_projects_dir_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        let marks = empty_marks();
        let slices = scan_claude(home.path(), &marks, SystemTime::now(), &BTreeSet::new());
        assert!(slices.is_empty(), "no projects dir is not an error");
    }
}
