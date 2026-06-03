//! `rosita studio` — a local, ephemeral web UI for viewing and editing
//! capabilities and profiles as a lens over your plain TOML config.
//!
//! Slice 0 is **headless**: only the comment/format-preserving [`edit`] engine
//! (the risk core) lands here, proven by tests, before any HTTP. The server,
//! routes, and views arrive in later slices. See `docs/studio-design.md`.

pub mod edit;

pub use edit::{FileDiff, Session, StagedOp};
