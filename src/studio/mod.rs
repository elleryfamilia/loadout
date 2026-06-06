//! `rosita studio` — a local, ephemeral web UI for viewing and editing
//! capabilities and profiles as a lens over your plain TOML config.
//!
//! - [`edit`] — the headless comment/format-preserving `toml_edit` write engine
//!   (Slice 0): `Session`/`StagedOp`/diff/apply.
//! - [`state`] — session state + the read-only model computations (selection,
//!   ReadOnly overlay preview, library snapshot), socket-free for testing.
//! - [`server`] — the `tiny_http` spine: bind 127.0.0.1, bootstrap-token +
//!   Host/Origin/cookie guards, and the `(method, path)` router ([`serve`]).
//! - [`views`] — `maud` server-rendered HTML (shell + htmx fragments).
//! - [`assets`] — CSS + the htmx-shim JS, embedded via `rust-embed`.
//!
//! The full read+write UI is shipped: the library and ReadOnly live preview,
//! the capability editor (static + script caps, run-on-demand), the profile
//! composer (targets + capability picker), stage → diff → apply, the leak
//! banner, the starter-pack gallery (`GET /packs`, `POST /packs/<id>/apply`),
//! and the fresh-config onboarding (`GET /onboarding/quickstart`).
//! See `docs/studio-design.md`.

pub mod assets;
pub mod edit;
pub mod server;
pub mod state;
pub mod views;

pub use edit::{FileDiff, Session, StagedOp};
pub use server::serve;
