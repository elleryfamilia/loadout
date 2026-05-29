//! rosita — the library behind the `rosita` CLI.
//!
//! `rosita` is "direnv for AI coding agents": it detects the current
//! project/runtime context, selects a profile via rules, renders an
//! agent-specific instruction overlay, and writes it safely (atomic writes,
//! managed marker blocks). The binary is a thin shell over this library so the
//! behaviour is fully unit/integration testable.
//!
//! ## Module map
//! - [`context`]    — detect cwd/git/languages/stack/commands/system/env.
//! - [`config`]     — layered TOML config (global + repo) and the merged model.
//! - [`capability`] — reusable guidance atoms composed by profiles.
//! - [`profile`]    — rule-based profiles + additive capability composition.
//! - [`render`]   — template rendering (minijinja) + generated header.
//! - [`adapters`] — per-agent wiring (Claude / Codex / generic).
//! - [`writer`]   — atomic file writes and managed marker-block upserts.
//! - [`redact`]   — secret/credential redaction.
//! - [`audit`]    — JSONL audit log of every render.
//! - [`hash`]     — deterministic context hash.
//! - [`commands`] — the implementation of each CLI subcommand.
//!
//! Generated instruction files are **agent guidance, not enforced policy**.
//! Nothing here should be treated as a hard security control.

pub mod adapters;
pub mod audit;
pub mod capability;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod hash;
pub mod profile;
pub mod redact;
pub mod render;
pub mod report;
pub mod templates;
pub mod writer;

/// Crate-wide result alias built on [`anyhow`].
pub type Result<T> = anyhow::Result<T>;
