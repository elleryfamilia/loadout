//! Extraction prompt builder — Task 10's file. It holds only the sentinel
//! marker for now, added ahead of the builder itself because slice assembly
//! (Task 9, [`crate::learn::slices`]) needs it for a recursion guard. Task 10
//! extends this file with the actual prompt builder (instructions, current
//! fragment ids+descriptions, pending candidates as inert JSON data, and the
//! redacted, capped session slices) built around this marker.

/// The harvest prompt's sentinel marker. Every prompt sent to the extraction
/// CLI embeds this exact string. A transcript that recorded the prompt
/// itself — e.g. a one-shot invocation with no recoverable `cwd`, which the
/// recursion guard's cwd check (self-exclusion layer 3, see
/// `.loadout/workflow/artifacts/design-learning.md`) cannot catch on its own —
/// still gets dropped: [`crate::learn::slices::assemble`] drops any session
/// with a message containing this marker (self-exclusion layer 4).
///
/// Exact value is pinned by Task 10's card; do not change it without
/// checking both call sites (the prompt builder this file will gain, and
/// the sentinel guard in [`crate::learn::slices`]).
pub const SENTINEL: &str = "loadout-harvest-marker-v1";
