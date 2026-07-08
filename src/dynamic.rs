//! Resolving dynamic fragments — live provider/command output.
//!
//! A fragment is *dynamic* when it has a `provider` (a built-in probe) or a
//! `command` (a shell command). At render time [`resolve`] produces its output,
//! honoring the cache:
//!
//! - built-in `provider` → always run (safe probes),
//! - `command` → run unless `allow_exec` is `false` (the per-fragment
//!   off-switch), in which case a skip note is rendered instead.
//!
//! [`DynamicMode::ReadOnly`] (used by `explain` and dry-run) never executes or
//! writes — it surfaces only what is already cached. This keeps `explain`
//! side-effect-free and honors "dry-run touches nothing".

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::context::Context;
use crate::fragment::Fragment;
use crate::providers::{self, ProviderOutput};

/// Default cache TTL when a dynamic fragment sets no `cache`.
const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// How dynamic fragments are resolved during a render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicMode {
    /// May execute providers/commands and write the cache (real renders).
    Live,
    /// Cache-only: never executes or writes (explain, dry-run).
    ReadOnly,
}

/// The outcome of resolving one dynamic fragment.
pub struct Resolution {
    /// Embedded output, if available (absent when unavailable or not cached).
    pub output: Option<ProviderOutput>,
    /// Set when a command was skipped (e.g. `allow_exec = false`); the value is
    /// the human note, rendered in place of output.
    pub skipped: Option<String>,
    /// Set when a command **failed** to run (non-zero exit, signal, or spawn
    /// failure) during a live resolve; the value is a short redacted message.
    /// The render omits the fragment, but the studio surfaces it with a retry.
    pub error: Option<String>,
}

/// Resolve a fragment's dynamic output, or `None` if it isn't dynamic.
pub fn resolve(
    cap: &Fragment,
    ctx: &Context,
    repo_base: &Path,
    mode: DynamicMode,
    now: DateTime<Utc>,
) -> Option<Resolution> {
    if !cap.is_dynamic() {
        return None;
    }
    let live = mode == DynamicMode::Live;
    let ttl = cap
        .cache
        .as_deref()
        .and_then(providers::parse_duration)
        .unwrap_or(DEFAULT_TTL);

    // An explicit command wins over a provider when both are set.
    if let Some(command) = &cap.command {
        // Per-fragment off-switch (`allow_exec = false` disables execution).
        if !cap.allow_exec {
            return Some(Resolution {
                output: None,
                skipped: Some(
                    "execution disabled for this fragment (allow_exec = false)".to_string(),
                ),
                error: None,
            });
        }
        if live {
            if let Some(hashes) = crate::trust::fragment_hashes(cap) {
                crate::trust::check_and_warn(crate::trust::Kind::Fragment, &cap.id, &hashes);
            }
        }
        let key = format!("cmd-{}", cap.id);
        let (output, error) = match providers::run_command(
            command,
            cap.script_lang.as_deref(),
            repo_base,
            &key,
            ttl,
            now,
            live,
        ) {
            providers::CommandOutcome::Output(o) => (Some(o), None),
            providers::CommandOutcome::Failed(msg) => (None, Some(msg)),
            // Empty-but-clean and a cold read-only cache both contribute nothing
            // and aren't errors.
            providers::CommandOutcome::Empty | providers::CommandOutcome::NotCached => (None, None),
        };
        return Some(Resolution {
            output,
            skipped: None,
            error,
        });
    }

    let pid = cap.provider.as_deref()?;
    Some(Resolution {
        output: providers::probe_one(pid, ctx, repo_base, ttl, now, live),
        skipped: None,
        error: None,
    })
}
