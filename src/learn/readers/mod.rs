//! Transcript readers: turn an agent's on-disk session logs into
//! [`SessionSlice`]s the harvest worker can hand to a paid extraction call.
//!
//! # Fail-closed philosophy
//!
//! Every store format read here is an **undocumented internal** of another
//! tool (Claude Code's `~/.claude/projects/*.jsonl`, codex rollouts, gemini
//! logs). The plan names this the highest ongoing maintenance risk, so the
//! readers are written to fail *closed*, never open:
//!
//! - A parse failure is contained to the smallest unit that failed. A bad
//!   line skips that line, not the file. An unreadable file or directory
//!   yields nothing for that store, never a panic and never an aborted run.
//! - Inclusion is **allow-listed, not deny-listed**. A line is harvested only
//!   when it positively proves it is user-authored, interactive, top-level
//!   content (see [`claude::scan_claude`]). Anything unrecognized is dropped.
//!   When the format drifts, the failure mode is "we harvested less", never
//!   "we harvested a tool result / a subagent's prompt / a programmatic
//!   session as if the human typed it".
//! - Resume is by byte offset and is **exactly-once by construction**: a
//!   truncated final line (a session caught mid-write) is never consumed past,
//!   so the watermark can never strand the reader mid-line, and the
//!   already-monotonic watermark (see [`crate::learn::watermarks`]) makes a
//!   stale/racing advance harmless.
//!
//! The reader functions return their slices directly; the orchestrating
//! worker (a later task) wraps a whole scan pass in [`ScanOutcome`], recording
//! any store it had to skip wholesale as a `"<agent>: <why>"` line so a
//! human can see *why* a store went quiet rather than guessing.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub mod claude;
pub mod codex;
pub mod gemini;

/// One harvestable session's user-authored text, plus everything the worker
/// needs to attribute it and to advance the watermark exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSlice {
    /// Which reader produced this (`"claude"`, `"codex"`, or `"gemini"`).
    /// A `&'static str` because the set is closed and known at compile time.
    pub agent: &'static str,
    /// The session identifier, sourced per reader: claude uses the file stem
    /// (which is the session uuid); codex uses the `session_meta` payload's
    /// `id` (the file name is `rollout-*`, not the id); gemini uses the
    /// `sessionId` field on each `logs.json` entry.
    pub session_id: String,
    /// The session's working directory, taken from the first line that
    /// carries one. `None` when no scanned line recorded a `cwd` — always the
    /// case for gemini, whose `logs.json` carries no cwd (the directory hash
    /// is the only locator; see [`gemini::gemini_project_hash`]).
    pub cwd: Option<PathBuf>,
    /// Timestamp (verbatim from the transcript, RFC 3339 UTC) of the newest
    /// user message in `messages` — the freshness signal the worker sorts by.
    pub ts: String,
    /// The user-authored messages, oldest first, in transcript order. Never
    /// empty: a scan that finds no eligible message produces no slice at all.
    pub messages: Vec<String>,
    /// The transcript file this slice was read from. Its string form is the
    /// key the worker passes to [`crate::learn::watermarks::Watermarks`].
    pub source_file: PathBuf,
    /// Byte offset one past the last **fully parsed** line (a complete,
    /// newline-terminated line). The worker records this as the new watermark
    /// so the next run resumes exactly here — never inside a partial line.
    /// Always `0` for gemini, which has no byte offsets: it resumes by the
    /// processed-`sessionId` set ([`crate::learn::watermarks::Watermarks::gemini_record`]).
    pub end_offset: u64,
}

/// The result of one whole scan pass across every enabled store. The
/// per-store readers return `Vec<SessionSlice>`; the worker assembles this,
/// appending a `"<agent>: <reason>"` entry to `skipped_stores` for any store
/// it could not read at all (missing home, unreadable dir, corrupt watermark),
/// so a quiet store is explained rather than silent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Every harvestable slice found this pass, across all stores.
    pub slices: Vec<SessionSlice>,
    /// Human-readable `"<agent>: <why>"` notes for stores skipped wholesale.
    pub skipped_stores: Vec<String>,
}

/// How long a transcript file must sit unmodified before a non-hook-triggered
/// scan will read it. Twenty minutes with no writes is treated as "the
/// session is over"; a shorter gap risks reading a session that is still
/// live (and whose final line may be half-written). A scan triggered by a
/// session-end hook names the just-ended session explicitly and bypasses this
/// wait for that one session only.
pub const QUIESCENCE: Duration = Duration::from_secs(20 * 60);

/// Resume position for an append-only transcript, guarding against a file that
/// shrank or was replaced. A recorded offset at or before the current end is
/// trusted; an offset *past* the end is stale (the file was truncated or
/// rotated out from under the watermark), so reading resumes from byte 0 rather
/// than seeking past EOF and stranding the reader on an empty read forever.
///
/// Genuinely identical across the append-only readers (claude, codex), so it
/// lives here rather than being copied per reader.
pub(crate) fn resume_start(recorded_offset: u64, file_len: u64) -> u64 {
    if recorded_offset <= file_len {
        recorded_offset
    } else {
        0
    }
}

/// Whether a transcript file is too fresh to read: modified within
/// [`QUIESCENCE`] of `now`, or carrying an mtime in the future (clock skew,
/// treated as "just now"). A file still being written must not be read because
/// its final line may be half-written.
///
/// Shared by the non-hooked quiescence gate in every reader. The claude reader
/// applies it only when a session-end hook did *not* name the session; codex
/// (no hooks in v0.15) always applies it.
pub(crate) fn too_fresh(mtime: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(mtime) {
        Ok(age) => age < QUIESCENCE,
        // mtime in the future (clock skew): treat as "just now" and wait.
        Err(_) => true,
    }
}
