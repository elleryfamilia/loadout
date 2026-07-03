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
use crate::config;

/// Debounce window per repo. Cursor fires more than one session event on a
/// single window open (verified live: two ~5s apart); only the first should
/// pay for a render.
const DEBOUNCE: Duration = Duration::from_secs(30);

/// Entry point for `load hook`.
pub fn run(rt: &Runtime, args: &HookArgs) -> crate::Result<()> {
    // Resolve the agent's hook registry from config. Interactive misuse
    // (unknown agent, no hook integration) may error normally — the registered
    // hook command always names a valid agent.
    let repo_base = crate::context::repo_base_for(&rt.cwd);
    let cfg = config::Config::load(&repo_base)?;
    let d = adapters::resolve_agent_token(&cfg, &args.agent)
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{}'", args.agent))?;
    let Some(hr) = d.hook_registry.clone() else {
        anyhow::bail!("agent '{}' has no hook integration", args.agent);
    };
    // The canonical id, not the invocation token — `load hook cursor-agent`
    // must adopt/refresh `cursor`, and everything downstream keys on ids.
    let id = d.id.clone();
    if args.remove {
        return remove(&hr, rt.dry_run);
    }
    serve(&id, hr.auto_adopt, rt.dry_run);
    Ok(())
}

/// Serve mode: refresh (and possibly adopt) the workspace roots from the
/// stdin payload. Infallible by design — every failure is swallowed.
fn serve(agent: &str, auto_adopt: bool, dry_run: bool) {
    // Nothing may reach stdout (the agent parses it as the hook response) and
    // warnings would only confuse a machine caller.
    crate::report::set_quiet_warnings(true);
    let mut payload = String::new();
    // Bound the read defensively; real payloads are a few hundred bytes.
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut payload);
    for root in workspace_roots(&payload) {
        refresh_root(&root, agent, auto_adopt, dry_run);
    }
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
fn refresh_root(root: &Path, agent: &str, auto_adopt: bool, dry_run: bool) {
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
    let _ = apply::apply_for_agents(&rt, &prep, &agents, &ApplyOptions::default());
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
