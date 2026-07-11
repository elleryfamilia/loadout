//! Ambient learning: mine the user's own agent sessions for durable
//! cross-project preferences, stage them as candidates in a review inbox.
//!
//! This is the module family root, split across two locations on purpose:
//! - [`state`] lives under `state_dir()/learn/` ([`state::learn_dir`]) —
//!   machine-local, never synced (the config dir syncs via `load sync`; this
//!   must not). Per-machine identity, the activation ack, the two-stamp
//!   throttle, and the consecutive-failure pause counter.
//! - [`watermarks`] also lives under `state_dir()/learn/`, same
//!   machine-local/never-synced rule as `state` — how far the harvest
//!   worker has read into each transcript source, so a run only mines
//!   what's new.
//! - [`journal`] lives under `global_config_dir()/inbox/` — synced by
//!   design, so every machine's observations and dispositions travel with
//!   `load sync` and fold together cleanly.
//!
//! See `.loadout/workflow/artifacts/design-learning.md` for the full design:
//! trigger fast path, detached worker, transcript readers, journal/inbox
//! model.

pub mod agent_cli;
pub mod extract;
pub mod gate;
pub mod journal;
pub mod lock;
pub mod readers;
pub mod slices;
pub mod state;
pub mod watermarks;
pub mod worker;
