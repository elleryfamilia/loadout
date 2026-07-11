//! Reader for codex CLI rollouts under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! One JSONL file per session. The first line is a `session_meta` record; the
//! rest are appended `event_msg` / `response_item` / `turn_context` records.
//! Only a narrow allow-list is user-authored interactive text (see
//! [`scan_codex`]).
//!
//! Format facts this reader depends on (verified against 21 real interactive
//! rollouts on a developer machine spanning codex CLI 0.63.0 – 0.142.0;
//! re-verify each release):
//!
//! - **The session gate is the first line only.** Line 1 is
//!   `{type:"session_meta", payload:{id, cwd, originator, source, cli_version, …}}`.
//!   A session is interactive human CLI iff `source == "cli"` (the string) AND
//!   `originator` is one of the interactive CLI originators. `source` is
//!   otherwise `"exec"` (codex exec / programmatic), `"vscode"` (Codex
//!   Desktop / IDE extension), or an **object** like
//!   `{"subagent":{…}}` (a spawned subagent thread) — all excluded. Because
//!   the gate rides only on line 1, the reader always re-reads line 1 even
//!   when resuming past it, so a resumed scan is gated exactly like a fresh one.
//!
//! - **The interactive originator was renamed across codex versions.** Older
//!   builds (0.63–0.101) tag interactive CLI sessions `codex_cli_rs`; newer
//!   builds (0.121+) tag them `codex-tui`. Both are accepted; the closed
//!   allow-list [`INTERACTIVE_ORIGINATORS`] enumerates them. (The plan card
//!   named only `codex_cli_rs`; that value alone would drop every recent
//!   session — see the task report.) Anything outside the list is excluded, so
//!   a future rename fails closed to "harvested less", never to harvesting a
//!   non-interactive session.
//!
//! - **User text is the `event_msg` / `user_message` event, not the
//!   `response_item` conversation.** Each human turn is recorded twice: once as
//!   an `event_msg` with `payload.type == "user_message"` and a plain-string
//!   `payload.message`, and again as a `response_item` `message`/`role:"user"`
//!   whose content array *also* includes the injected AGENTS.md instructions
//!   and `<environment_context>` blocks. Harvesting the `user_message` events
//!   yields exactly the human-typed text and excludes those injections by
//!   construction (verified: `response_item` user lines == `user_message`
//!   events + 2 in every file, the 2 being AGENTS.md + environment_context).
//!   `payload.message` is a plain string in every version seen.
//!
//! - **Records are chronological.** Top-level `timestamp` is non-decreasing
//!   within a file (verified across all 21 files), so the newest included
//!   message's timestamp is simply the last one scanned — the same append-order
//!   assumption the claude reader relies on.
//!
//! - Every complete record ends with a trailing newline, so a final line
//!   *without* one is a session caught mid-write and is never consumed (see the
//!   `end_offset` discipline in [`scan_codex`]).

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::{resume_start, too_fresh, SessionSlice};
use crate::learn::watermarks::Watermarks;

/// Interactive CLI `originator` values. Closed allow-list: `codex_cli_rs` is
/// the older tag (codex 0.63–0.101), `codex-tui` the newer (0.121+). Any other
/// originator (`codex_exec`, `Codex Desktop`) is a non-interactive session and
/// is excluded.
const INTERACTIVE_ORIGINATORS: [&str; 2] = ["codex_cli_rs", "codex-tui"];

/// Scan every codex rollout under `home/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
/// and return one [`SessionSlice`] per interactive CLI session that has new,
/// eligible user text.
///
/// Per file, in order:
/// 1. **Quiescence.** Skip a file modified more recently than
///    [`super::QUIESCENCE`] ago (relative to `now`). There are no codex
///    session-end hooks in this release, so — unlike claude — there is no
///    per-session bypass.
/// 2. **Session gate.** Read line 1 (`session_meta`), always, even when
///    resuming past it. Include the file iff `payload.source == "cli"` and
///    `payload.originator` is in [`INTERACTIVE_ORIGINATORS`]. `session_id` and
///    `cwd` come from the meta payload. A missing/unparseable meta, a first
///    line that is not `session_meta`, an absent `id`, or a non-matching
///    originator/source excludes the whole file (fail closed).
/// 3. **Resume.** Start reading at `marks.mark(path).bytes_processed`; a
///    recorded offset past the current end (file truncated/replaced) is
///    discarded and the file re-read from 0 (see [`super::resume_start`]).
/// 4. **Parse leniently.** A line that fails to parse as JSON is skipped, the
///    rest of the file is not. A trailing chunk with no terminating newline is
///    a partial final line and is not consumed at all.
/// 5. **Message inclusion.** Keep a line's text iff it is an `event_msg` with
///    `payload.type == "user_message"` and a non-empty string `payload.message`.
///
/// `ts` is the newest included message's timestamp; `end_offset` is one byte
/// past the last fully parsed line. A file that yields no eligible message
/// produces no slice (and so leaves its watermark unadvanced, to be re-examined
/// cheaply next run).
///
/// Fail-closed: any IO error (missing dir, unreadable file) yields nothing for
/// the affected path and never aborts the wider scan.
pub fn scan_codex(home: &Path, marks: &Watermarks, now: SystemTime) -> Vec<SessionSlice> {
    let sessions = home.join(".codex").join("sessions");
    let mut files = Vec::new();
    collect_rollouts(&sessions, &mut files);
    files.sort();

    let mut slices = Vec::new();
    for path in files {
        if let Some(slice) = scan_one(&path, marks, now) {
            slices.push(slice);
        }
    }
    slices
}

/// Collect `sessions/YYYY/MM/DD/rollout-*.jsonl` into `out`, matching codex's
/// date-nested layout exactly. Unreadable directories at any level are skipped
/// silently (fail-closed): a store we cannot list yields nothing.
fn collect_rollouts(sessions: &Path, out: &mut Vec<PathBuf>) {
    let Ok(years) = fs::read_dir(sessions) else {
        return;
    };
    for year in years.flatten() {
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(days) = fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                let Ok(entries) = fs::read_dir(day.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_rollout(&path) {
                        out.push(path);
                    }
                }
            }
        }
    }
}

/// A `rollout-*.jsonl` file (codex names every session file this way).
fn is_rollout(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("rollout-")
        && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        && path.is_file()
}

/// Scan a single rollout. Returns `None` when the file is too fresh, excluded
/// by the session gate, unreadable, or simply has no new eligible user text.
fn scan_one(path: &Path, marks: &Watermarks, now: SystemTime) -> Option<SessionSlice> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }

    // Quiescence: always enforced (no codex session-end hooks in this release).
    let mtime = meta.modified().ok()?;
    if too_fresh(mtime, now) {
        return None;
    }

    let mut reader = BufReader::new(fs::File::open(path).ok()?);

    // Session gate: read line 1 (session_meta) unconditionally, even when
    // resuming past it, so inclusion is decided the same way on every run.
    let mut first_line = Vec::new();
    reader.read_until(b'\n', &mut first_line).ok()?;
    // A first line with no terminating newline is a session still being
    // created (session_meta half-written): cannot trust the gate — fail closed.
    if first_line.last() != Some(&b'\n') {
        return None;
    }
    let meta_value = serde_json::from_slice::<Value>(&first_line).ok()?;
    let (session_id, cwd) = session_gate(&meta_value)?;

    // Resume offset (guarded against a shrunk/replaced file).
    let key = path.to_string_lossy();
    let mark = marks.mark(&key).copied().unwrap_or_default();
    let start = resume_start(mark.bytes_processed, meta.len());

    // Seek discards the BufReader buffer and repositions the underlying file at
    // the absolute offset; read the tail (or, when start == 0, the whole file —
    // line 1 is re-scanned harmlessly, session_meta is not a user_message).
    reader.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).ok()?;

    let mut messages: Vec<String> = Vec::new();
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

        if let Some(text) = user_message_text(&value) {
            if !text.trim().is_empty() {
                if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
                    newest_ts = Some(ts.to_string());
                }
                messages.push(text);
            }
        }
    }

    if messages.is_empty() {
        return None;
    }

    Some(SessionSlice {
        agent: "codex",
        session_id,
        cwd,
        ts: newest_ts.unwrap_or_default(),
        messages,
        source_file: path.to_path_buf(),
        end_offset: start + consumed as u64,
    })
}

/// Evaluate the `session_meta` gate. Returns `(session_id, cwd)` when the file
/// is an included interactive CLI session, else `None` (fail closed). A source
/// that is a JSON object (spawned subagent) yields `None` from `as_str`, so it
/// is excluded without special-casing.
fn session_gate(meta: &Value) -> Option<(String, Option<PathBuf>)> {
    if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = meta.get("payload")?;
    let originator = payload.get("originator").and_then(Value::as_str)?;
    let source = payload.get("source").and_then(Value::as_str)?;
    if source != "cli" || !INTERACTIVE_ORIGINATORS.contains(&originator) {
        return None;
    }
    let session_id = payload.get("id").and_then(Value::as_str)?.to_string();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from);
    Some((session_id, cwd))
}

/// The human's typed text from a codex `event_msg` / `user_message` event.
/// `payload.message` is a plain string across every codex version seen. Returns
/// `None` for any other record, which is how injected `response_item` user
/// lines, assistant messages, tool calls, and token-count events all fall out.
fn user_message_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("user_message") {
        return None;
    }
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    use super::super::QUIESCENCE;

    /// Absolute path to a committed codex fixture.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/learn/codex")
            .join(name)
    }

    /// Stage a fixture into a `home`-shaped tempdir under the real codex
    /// layout `<home>/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. The reader
    /// derives the session id from the `session_meta` payload, not the file
    /// name, so the file only has to match the `rollout-*.jsonl` glob.
    fn stage(fixture_name: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex/sessions/2026/06/01");
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join(format!("rollout-{fixture_name}"));
        fs::copy(fixture(fixture_name), &dst).unwrap();
        (home, dst)
    }

    fn now_past_quiescence(staged: &Path) -> SystemTime {
        let mtime = fs::metadata(staged).unwrap().modified().unwrap();
        mtime + QUIESCENCE + Duration::from_secs(60)
    }

    fn now_within_quiescence(staged: &Path) -> SystemTime {
        let mtime = fs::metadata(staged).unwrap().modified().unwrap();
        mtime + Duration::from_secs(60)
    }

    fn empty_marks() -> Watermarks {
        let dir = tempfile::tempdir().unwrap();
        Watermarks::load_from(&dir.path().join("watermarks.json"))
    }

    #[test]
    fn interactive_codex_tui_yields_its_three_user_messages() {
        let (home, staged) = stage("interactive.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));

        assert_eq!(slices.len(), 1, "one interactive session, one slice");
        let s = &slices[0];
        assert_eq!(s.agent, "codex");
        assert_eq!(s.session_id, "synthetic-interactive-0001");
        assert_eq!(
            s.messages,
            vec![
                "Always use tabs, not spaces, for indentation in this project.".to_string(),
                "Prefer pnpm over npm for every install.".to_string(),
                "Run the linter before every commit, no exceptions.".to_string(),
            ],
            "only event_msg/user_message events are harvested; injected \
             AGENTS.md/environment_context response_item user lines, assistant \
             lines, and tool/token lines are all ignored"
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
    fn legacy_codex_cli_rs_originator_is_included() {
        let (home, staged) = stage("interactive-legacy.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));

        assert_eq!(
            slices.len(),
            1,
            "codex_cli_rs is an interactive CLI originator"
        );
        assert_eq!(
            slices[0].messages,
            vec![
                "Keep commit messages in imperative mood.".to_string(),
                "Never commit directly to the main branch.".to_string(),
            ],
            "the older user_message event shape (no local_images/text_elements) parses too"
        );
    }

    #[test]
    fn exec_source_is_excluded_whole() {
        let (home, staged) = stage("exec.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert!(
            slices.is_empty(),
            "source==exec (codex_exec) disqualifies the whole file even with user text present"
        );
    }

    #[test]
    fn subagent_object_source_is_excluded_whole() {
        let (home, staged) = stage("subagent.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert!(
            slices.is_empty(),
            "a source that is an object (spawned subagent), not the string \"cli\", is excluded"
        );
    }

    #[test]
    fn malformed_line_skips_only_that_line() {
        let (home, staged) = stage("malformed.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec![
                "Keep functions under fifty lines.".to_string(),
                "Write a test for every bug fix.".to_string(),
            ],
            "the malformed middle line is skipped; the valid user messages survive"
        );
    }

    #[test]
    fn fresh_file_is_skipped() {
        let (home, staged) = stage("interactive.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_within_quiescence(&staged));
        assert!(
            slices.is_empty(),
            "a just-modified file is treated as live (no codex hook bypass in v0.15)"
        );
    }

    #[test]
    fn resume_from_offset_reads_only_new_messages() {
        let (home, staged) = stage("resume.jsonl");

        // Resume past the FIRST user_message event (line 2). The session_meta
        // gate line (line 1) is *before* this offset, proving the gate is
        // re-read on resume rather than skipped.
        let bytes = fs::read(&staged).unwrap();
        let nl1 = bytes.iter().position(|&b| b == b'\n').unwrap();
        let nl2 = nl1 + 1 + bytes[nl1 + 1..].iter().position(|&b| b == b'\n').unwrap();
        let resume_at = (nl2 + 1) as u64;

        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), resume_at, 0);

        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec![
                "Message two about branch naming.".to_string(),
                "Message three about PR descriptions.".to_string(),
            ],
            "the already-recorded first message is not re-read; the session is \
             still included because line 1 (session_meta) is re-read for the gate"
        );
        assert_eq!(
            slices[0].end_offset,
            bytes.len() as u64,
            "end_offset advances to the end of the fully consumed file"
        );
        assert_eq!(
            slices[0].cwd,
            Some(PathBuf::from("/home/dev/resume-proj")),
            "cwd comes from session_meta, re-read on resume"
        );
    }

    #[test]
    fn resume_at_eof_yields_nothing() {
        let (home, staged) = stage("resume.jsonl");
        let len = fs::metadata(&staged).unwrap().len();
        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), len, 0);

        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert!(
            slices.is_empty(),
            "a fully consumed file has no new user text to offer"
        );
    }

    #[test]
    fn partial_final_line_is_never_consumed() {
        let (home, staged) = stage("partial-tail.jsonl");
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));

        let full_len = fs::metadata(&staged).unwrap().len();
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages,
            vec!["Complete message before the crash.".to_string()],
            "the truncated final user_message line contributes no message"
        );
        // end_offset stops at the last complete line's newline, strictly before
        // EOF, so the next run reparses the (by then completed) final line.
        let bytes = fs::read(&staged).unwrap();
        let nl1 = bytes.iter().position(|&b| b == b'\n').unwrap();
        let nl2 = nl1 + 1 + bytes[nl1 + 1..].iter().position(|&b| b == b'\n').unwrap();
        let boundary = (nl2 + 1) as u64;
        assert_eq!(
            slices[0].end_offset, boundary,
            "end_offset stops after the last complete line, not mid-line"
        );
        assert!(slices[0].end_offset < full_len);
    }

    #[test]
    fn offset_past_eof_rereads_from_start() {
        let (home, staged) = stage("interactive.jsonl");
        let len = fs::metadata(&staged).unwrap().len();
        let mut marks = empty_marks();
        marks.advance(&staged.to_string_lossy(), len + 10_000, 0);

        let slices = scan_codex(home.path(), &marks, now_past_quiescence(&staged));
        assert_eq!(slices.len(), 1);
        assert_eq!(
            slices[0].messages.len(),
            3,
            "a stale over-long offset is discarded and the file re-read from 0"
        );
        assert_eq!(slices[0].end_offset, len);
    }

    #[test]
    fn missing_sessions_dir_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        let marks = empty_marks();
        let slices = scan_codex(home.path(), &marks, SystemTime::now());
        assert!(slices.is_empty(), "no sessions dir is not an error");
    }
}
