//! Ambient learning: mine the user's own agent sessions for durable
//! cross-project preferences, stage them as candidates in a review inbox.
//!
//! This is the module family root. State lives under
//! `state_dir()/learn/` ([`state::learn_dir`]) — machine-local, never synced
//! (the config dir syncs via `load sync`; this must not). See
//! `.loadout/workflow/artifacts/design-learning.md` for the full design:
//! trigger fast path, detached worker, transcript readers, journal/inbox
//! model. This task lays only the pure-logic foundation every later piece
//! builds on: per-machine identity, the per-machine activation ack, the
//! two-stamp throttle, and the consecutive-failure pause counter.

pub mod state;
