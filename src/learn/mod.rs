//! Ambient learning: mine the user's own agent sessions for durable
//! cross-project preferences, stage them as candidates in a review inbox.
//!
//! This is the module family root, split across two locations on purpose:
//! - [`state`] lives under `state_dir()/learn/` ([`state::learn_dir`]) —
//!   machine-local, never synced (the config dir syncs via `load sync`; this
//!   must not). Per-machine identity, the activation ack, the two-stamp
//!   throttle, and the consecutive-failure pause counter.
//! - [`journal`] lives under `global_config_dir()/inbox/` — synced by
//!   design, so every machine's observations and dispositions travel with
//!   `load sync` and fold together cleanly.
//!
//! See `.loadout/workflow/artifacts/design-learning.md` for the full design:
//! trigger fast path, detached worker, transcript readers, journal/inbox
//! model.

pub mod journal;
pub mod state;
