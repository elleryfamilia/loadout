//! `load hook <agent>` — the machine-invoked freshness endpoint an agent's own
//! lifecycle hooks call (e.g. Cursor's `sessionStart` in `~/.cursor/hooks.json`).
//!
//! Serve mode reads the agent's hook payload JSON on stdin, extracts the
//! workspace roots, and quietly re-renders each: already-adopted repos refresh
//! every overlay they have, and (by default) a git repo some loadout applies
//! to is **adopted on first open** — wired for this agent with no prior
//! `load refresh`. It is an observational hook: it prints nothing on stdout,
//! suppresses warnings, and **always exits 0** — a loadout failure must never
//! block the agent.
//!
//! The `--event session-end` variant is a second serve path for ambient
//! learning: an agent's session-end hook (Claude's `SessionEnd`, Cursor's
//! `stop`) calls it when a session ends. It does no refresh work — it records
//! the just-ended session as eligible for the next harvest and wakes the
//! throttled worker — while keeping the same serve invariants (stdout-silent,
//! warnings quiet, bounded stdin, always exit 0).
//!
//! `--remove` deregisters loadout's entries from the agent's hooks file (the
//! counterpart to the automatic registration during `refresh`/`run`; repo-local
//! `clean` deliberately leaves the global hook alone).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::apply::{self, sync_before_render};
use super::{prepare_live, Runtime};
use crate::adapters::{self, ApplyOptions, HookRegistry};
use crate::cli::HookArgs;
use crate::config::{self, Config};
use crate::learn::trigger::{maybe_spawn, write_eligibility_hint, Trigger};

/// Debounce window per repo. Cursor fires more than one session event on a
/// single window open (verified live: two ~5s apart); only the first should
/// pay for a render.
const DEBOUNCE: Duration = Duration::from_secs(30);

/// Entry point for `load hook`.
pub fn run(rt: &Runtime, args: &HookArgs) -> crate::Result<()> {
    // Resolve the agent (interactive misuse of an unknown agent may error
    // normally — the registered hook command always names a valid agent). The
    // freshness serve path additionally needs a hook registry, resolved below;
    // the session-end learn path does not.
    let repo_base = crate::context::repo_base_for(&rt.cwd);
    let cfg = config::Config::load(&repo_base)?;
    let d = adapters::resolve_agent_token(&cfg, &args.agent)
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{}'", args.agent))?;
    // The canonical id, not the invocation token — `load hook cursor-agent`
    // must adopt/refresh `cursor`, and everything downstream keys on ids.
    let id = d.id.clone();

    // Ambient-learning session-end serve path. Taken BEFORE the freshness
    // hook-registry resolution below on purpose: claude registers a SessionEnd
    // learn hook but has NO freshness `hook_registry`, so requiring one here
    // would reject `load hook claude --event session-end`. This path does no
    // refresh/adopt work — it only marks the just-ended session eligible and
    // wakes the throttled harvest worker.
    if args.event.as_deref() == Some("session-end") {
        serve_session_end(&id, &cfg);
        return Ok(());
    }

    let Some(hr) = d.hook_registry.clone() else {
        anyhow::bail!("agent '{}' has no hook integration", args.agent);
    };
    if args.remove {
        return remove(&hr, rt.dry_run);
    }
    serve(&id, hr.auto_adopt, rt.dry_run, &cfg);
    Ok(())
}

/// Serve mode: refresh (and possibly adopt) the workspace roots from the
/// stdin payload. Infallible by design — every failure is swallowed.
fn serve(agent: &str, auto_adopt: bool, dry_run: bool, cfg: &Config) {
    // Nothing may reach stdout (the agent parses it as the hook response) and
    // warnings would only confuse a machine caller.
    crate::report::set_quiet_warnings(true);
    let mut payload = String::new();
    // Bound the read defensively; real payloads are a few hundred bytes.
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut payload);
    // Folded ONCE for this whole invocation, however many roots the payload
    // names (`[learn]` is global-only, so every root sees the same count
    // anyway) — same "one fold call per command" rule as `run`/`refresh`. Uses
    // `cfg` as loaded at hook-command startup, before any per-root sync pull
    // (each `refresh_root` runs its own `sync_before_render`) — the same
    // staleness the trigger check below already accepts (Decision #4:
    // propagation is "the next launch there"); it self-heals on this hook's
    // very next invocation, which reloads `cfg` fresh.
    let learn_pending = apply::learn_pending_count(cfg);
    for root in workspace_roots(&payload) {
        refresh_root(&root, agent, auto_adopt, dry_run, learn_pending);
    }
    // Trigger fast path — still stdout-silent (never blocks, never errors
    // outward, and prints nothing: the guard chain's diagnostics are `vlog!`
    // only). `cfg` is the config loaded at this hook invocation's own cwd;
    // `[learn]` is global-only (a repo layer can't override it), so it's the
    // same value regardless of which workspace root(s) were just refreshed.
    maybe_spawn(cfg, Trigger::HookServe);
}

/// Serve mode for a just-ended agent session (the `--event session-end` path):
/// record the session as immediately eligible for the next harvest and wake the
/// throttled worker. Does NO refresh/adopt work — the freshness [`serve`] owns
/// that. Keeps every serve invariant: nothing to stdout, warnings quiet,
/// bounded stdin, and ALWAYS returns (the caller exits 0). Infallible by design.
fn serve_session_end(agent: &str, cfg: &Config) {
    // Same stdout/stderr discipline as `serve`: the agent parses stdout as the
    // hook response, and warnings only confuse a machine caller.
    crate::report::set_quiet_warnings(true);
    let mut payload = String::new();
    // Bound the read defensively; real session-end payloads are a few hundred
    // bytes. `session_id`/`conversation_id` come from an EXTERNAL payload, so
    // everything below stays lenient — a missing or odd field degrades to "no
    // hint", never an error.
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut payload);

    // Recursion hygiene: if this serve is somehow running inside the harvest
    // worker's OWN agent-CLI call (`LOADOUT_LEARN_WORKER` set), never mark that
    // session eligible. `maybe_spawn`'s guard 4 already blocks the nested spawn;
    // skipping the hint write too keeps the worker's own sessions out of the
    // eligible set. Belt and braces — the readers' sentinel/entry-point
    // self-exclusion is the primary defense; this is a second, cheaper layer.
    if std::env::var_os("LOADOUT_LEARN_WORKER").is_none() {
        if let Some(hint_id) = hint_id(agent, &payload, cfg) {
            write_eligibility_hint(agent, &hint_id);
        }
    }

    // Wake the throttled worker. The guard chain (including the recursion guard)
    // is the single authority on whether a spawn actually happens; this never
    // blocks past the millisecond-lived intermediate and never errors outward.
    maybe_spawn(
        cfg,
        Trigger::SessionEnd {
            agent: agent.to_string(),
        },
    );
}

/// The eligibility-hint id for a session-end payload, or `None` to write no
/// hint. Lenient throughout: an unparseable payload or a missing id degrades to
/// `None` (claude) or a generic wake (cursor); it never errors outward.
///
/// - **claude**: the `session_id` field. Under `scope = Adopted` (the default),
///   a hint is skipped when the payload's `cwd` names a repo loadout has NOT
///   adopted — the worker's scope filter ([`crate::learn::slices`]) would drop
///   that session anyway, so the hint would only wake the worker to do nothing
///   (carry-forward C13). This is a best-effort write-time optimization, NOT a
///   correctness guarantee: the config read here can differ from the worker's at
///   run time (e.g. `scope` flipped, or the repo adopted, between now and the
///   worker), so the deterministic backstop is the TTL sweep in
///   [`crate::learn::trigger::read_hints`]. When `cwd` is absent the hint is
///   kept (can't judge) and the worker's own scope filter — the authority —
///   still applies.
/// - **cursor**: the `conversation_id` field (doc-verified 2026-07-11 against
///   <https://cursor.com/docs/hooks> — the `stop` payload carries
///   `conversation_id`, no `cwd`, so no scope skip). A missing/empty id degrades
///   to a generic `tick-<unix>` hint (file `cursor-tick-<unix>`): we ship no
///   cursor transcript reader this release, so a cursor hint's only job is to
///   wake the claude/codex/gemini scan.
fn hint_id(agent: &str, payload: &str, cfg: &Config) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(payload).ok();
    match agent {
        "cursor" => Some(cursor_hint_id(v.as_ref())),
        // claude (the only other agent that registers a session-end learn hook).
        _ => {
            let v = v.as_ref()?;
            let session_id = v.get("session_id").and_then(|s| s.as_str())?;
            if session_id.is_empty() || scope_skips_hint(cfg, v) {
                return None;
            }
            Some(session_id.to_string())
        }
    }
}

/// Cursor's stop-payload id: the doc-verified `conversation_id` (stable across
/// turns). Absent/empty → a generic `tick-<unix>` so the worker still wakes.
fn cursor_hint_id(v: Option<&serde_json::Value>) -> String {
    let id = v
        .and_then(|v| v.get("conversation_id"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty());
    match id {
        Some(s) => s.to_string(),
        None => format!("tick-{}", now_unix()),
    }
}

/// C13 write-time scope skip (claude): `true` when `scope = Adopted` and the
/// payload's `cwd` is present and names a repo loadout has not adopted. A
/// missing `cwd` returns `false` (keep the hint — can't judge).
fn scope_skips_hint(cfg: &Config, payload: &serde_json::Value) -> bool {
    if cfg.learn.scope != config::LearnScope::Adopted {
        return false;
    }
    match payload.get("cwd").and_then(|c| c.as_str()) {
        Some(cwd) => !crate::learn::slices::repo_is_adopted(Path::new(cwd)),
        None => false,
    }
}

/// Wall-clock unix seconds (best-effort; `0` if the clock is before the epoch).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `workspace_roots` array of the hook payload; empty on any mismatch.
fn workspace_roots(payload: &str) -> Vec<PathBuf> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .as_ref()
        .and_then(|v| v.get("workspace_roots"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Quietly re-render one workspace root, mirroring a bare `load refresh` there
/// (auto-pull sync, then every agent with an existing overlay) — and, with
/// `auto_adopt`, wiring `agent` into a repo on its first open so no prior
/// `load refresh` is ever needed. All guards fail closed; errors are swallowed.
/// `learn_pending` is folded once by the caller ([`serve`]), not re-derived here.
fn refresh_root(root: &Path, agent: &str, auto_adopt: bool, dry_run: bool, learn_pending: usize) {
    let Ok(root) = root.canonicalize() else {
        return;
    };
    // Never at $HOME — wiring there would bleed into every repo beneath it
    // (also enforced by `apply`, but don't even render).
    if let Some(home) = config::home_dir() {
        if home.canonicalize().map(|h| h == root).unwrap_or(false) {
            return;
        }
    }
    if debounced(&root) {
        return;
    }
    let rt = Runtime::new(root.clone(), dry_run);
    let _ = sync_before_render(&rt);
    let Ok(prep) = prepare_live(&rt) else {
        return;
    };
    let mut agents = super::refresh::existing_overlay_agents(&prep);
    // Zero-friction adoption: opening a repo in the IDE wires this agent on
    // first session, gated only on it being worth anything — a git repo (an
    // arbitrary folder like ~/Downloads gets nothing) that some loadout
    // actually applies to (else the overlay would be an empty husk). NOT
    // gated on bindings or prior refreshes.
    if auto_adopt
        && !agents.iter().any(|a| a == agent)
        && prep.context.git.is_some()
        && prep.composition.primary_profile().is_some()
    {
        agents.push(agent.to_string());
    }
    if agents.is_empty() {
        return;
    }
    let _ = apply::apply_for_agents(&rt, &prep, &agents, &ApplyOptions::default(), learn_pending);
    if !dry_run {
        stamp(&root);
    }
}

/// Whether this root was hook-refreshed within the debounce window.
fn debounced(root: &Path) -> bool {
    stamp_path(root)
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| age < DEBOUNCE)
        .unwrap_or(false)
}

fn stamp_path(root: &Path) -> PathBuf {
    root.join(".loadout/cache/hook-stamp")
}

fn stamp(root: &Path) {
    let p = stamp_path(root);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, b"");
}

/// `--remove`: strip loadout's entries from the agent's user-level hooks file.
fn remove(hr: &HookRegistry, dry_run: bool) -> crate::Result<()> {
    let home =
        config::home_dir().ok_or_else(|| anyhow::anyhow!("$HOME unset — nothing to remove"))?;
    let path = home.join(&hr.hooks_file);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing to remove — {} does not exist", path.display());
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("could not read {}: {e}", path.display())),
    };
    match adapters::remove_hook_command(&existing, &hr.subcommand)? {
        Some(updated) => {
            if dry_run {
                println!(
                    "dry run — would remove loadout's hook entries from {}",
                    path.display()
                );
            } else {
                crate::writer::atomic_write(&path, &updated)?;
                println!("removed loadout's hook entries from {}", path.display());
            }
        }
        None => println!("no loadout hook entries in {}", path.display()),
    }
    Ok(())
}
