//! loadout — the library behind the `loadout` CLI.
//!
//! `loadout` injects global context into your AI coding agents: it detects
//! the current project/runtime context, selects the profile that fits, renders
//! an agent-specific instruction overlay, and writes it safely (atomic writes,
//! managed marker blocks). The binary is a thin shell over this library so the
//! behaviour is fully unit/integration testable.
//!
//! ## Module map
//! - [`context`]    — detect cwd/git/languages/stack/commands/system/env.
//! - [`config`]     — layered TOML config (global + repo) and the merged model.
//! - [`fragment`]   — reusable guidance atoms (your library + the shipped palette).
//! - [`profile`]    — targeted profiles + pick-one selection & composition.
//! - [`workflow`]   — named stage spine + handoff artifacts a profile can bind.
//! - [`binding`]    — the per-project remembered profile choice.
//! - [`render`]   — template rendering (minijinja) + generated header.
//! - [`providers`]— native environment discovery (host/tailnet/docker/…).
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
pub mod binding;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod dynamic;
pub mod fragment;
pub mod hash;
pub mod lint;
pub mod markdown;
pub mod pack;
pub mod plan;
pub mod profile;
pub mod providers;
pub mod recents;
pub mod redact;
pub mod render;
pub mod report;
pub mod skills;
pub mod studio;
pub mod style;
pub mod sync;
pub mod target;
pub mod templates;
pub mod trust;
pub mod tui;
pub mod update;
pub mod workflow;
pub mod writer;

/// Crate-wide result alias built on [`anyhow`].
pub type Result<T> = anyhow::Result<T>;
