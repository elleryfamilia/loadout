//! One-time cleanup of features loadout used to have.
//!
//! Right now that means exactly one: **ambient learning**, removed in 0.21.0.
//! It mined your own agent session transcripts for durable preferences and
//! staged them in a review inbox. See `docs/shelved-ambient-learning.md`.
//!
//! Deleting the code is not enough, because the feature left control state on
//! disk that outlives the binary:
//!
//! 1. `[learn] enabled` in the global config — which **syncs to your other
//!    machines**, so a machine still on 0.18 keeps harvesting until the flag
//!    itself flips.
//! 2. A per-machine activation ack at `<state>/learn/activation.json`. Learning
//!    was active only when the flag *and* the ack were both present, so an ack
//!    left behind means a downgrade to 0.18 silently switches it back on.
//! 3. Session-end hook entries in `~/.claude/settings.json` and
//!    `~/.cursor/hooks.json` that call `load hook <agent> --event session-end`.
//!
//! [`retire_learning`] clears all three, once, and then never runs again. It
//! does **not** touch your data — the harvested candidates in
//! `<config>/inbox/` and the evidence in `<state>/learn/evidence/` are yours;
//! it points at them once and says they are safe to delete.
//!
//! Self-contained on purpose. It duplicates a little knowledge that also lives
//! in `adapters` (the two hook descriptors) rather than importing it, so that
//! removing the learning code cannot break the cleanup that removal depends on.

use std::path::PathBuf;

use crate::adapters::hooks_claude;
use crate::adapters::remove_hook_command;
use crate::config;
use crate::style::Painter;

/// Marker written in the state dir once the cleanup has run. Its presence is
/// the whole idempotency mechanism: the cleanup is skipped when it exists.
const MARKER: &str = "learning-retired";

/// Ceiling on the git push that propagates the disable. Short on purpose: this
/// runs inside whatever command the user actually asked for, so a slow or
/// unreachable remote must not hold it up. Missing the push only delays
/// propagation to the next `load sync`.
const PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The learning hook entries as they were registered, by agent. Frozen copies
/// of the `learn_hooks` descriptors that used to live in `adapters` — kept here
/// verbatim so this cleanup keeps working after those descriptors are deleted.
///
/// `(agent, hooks_file_relative_to_home, event, command_suffix, nested_schema)`.
/// `nested_schema` picks the dialect: Claude Code's nested matcher groups vs
/// Cursor's flat array.
const LEARN_HOOKS: &[(&str, &str, &str, &str, bool)] = &[
    (
        "claude",
        ".claude/settings.json",
        "SessionEnd",
        "hook claude --event session-end",
        true,
    ),
    (
        "cursor",
        ".cursor/hooks.json",
        "stop",
        "hook cursor --event session-end",
        false,
    ),
];

/// Turn ambient learning off and clean up after it. Runs at most once per
/// machine; a no-op on every later invocation.
///
/// Best-effort throughout. This is called from ordinary command paths, and a
/// failure here must never take down the command the user actually asked for —
/// every step warns rather than propagating, and a step that fails is simply
/// retried next time (the marker is written only after a clean pass).
///
/// Returns the lines to print, or empty when there was nothing to do.
pub fn retire_learning(dry_run: bool) -> Vec<String> {
    let Some(state) = config::state_dir() else {
        return Vec::new(); // no home → nothing addressable to clean
    };
    if state.join(MARKER).exists() {
        return Vec::new(); // already done on this machine
    }

    let learn_dir = state.join("learn");
    let inbox_dir = config::global_config_dir().map(|d| d.join("inbox"));

    // Is there anything to do at all? A machine that never turned learning on
    // must stay silent — no marker, no output, no config write.
    let had_flag = learn_flag_is_on();
    let had_ack = learn_dir.join("activation.json").exists();
    // Real harvested output, not merely a directory that exists. Most of
    // `learn/` is the feature's own bookkeeping; pointing the user at a
    // directory that only ever held that is telling them to go look at
    // nothing.
    let learn_data = has_harvested_evidence(&learn_dir);
    let inbox_data = inbox_dir.as_deref().map(has_any_file).unwrap_or(false);
    let had_data = learn_data || inbox_data;
    let hook_plan = plan_hook_removals();
    if !had_flag && !had_ack && !had_data && hook_plan.is_empty() {
        return Vec::new();
    }

    let p = Painter::auto();
    let mut out = Vec::new();
    if dry_run {
        out.push(format!(
            "{} dry run: would turn off the retired ambient-learning feature and clean up after it.",
            p.dim("~")
        ));
        return out; // no marker under --dry-run: the real run must still happen
    }

    out.push(format!(
        "{} ambient learning was removed in 0.21.0 — cleaning up after it, once.",
        p.dim("·")
    ));
    let mut clean_pass = true;

    // 1. Clear the synced intent flag. This is the step that matters for your
    //    other machines: one still running 0.18 reads this flag and keeps
    //    harvesting until it flips. Left as an explicit `false` rather than
    //    deleted, so an older binary reads a deliberate "off" instead of an
    //    absent table.
    if had_flag {
        match set_learn_enabled_false() {
            Ok(path) => {
                out.push(format!(
                    "  {} turned it off in {}.",
                    p.dim("·"),
                    path.display()
                ));
                // Push it, so the flip actually reaches your other machines.
                // Writing the flag locally is only half the job: a machine still
                // on 0.18 reads this flag out of the synced repo and keeps
                // harvesting until it changes there. Bounded and
                // warn-don't-fail — never leave this machine half-retired
                // because a remote was slow or had moved on.
                if let Some(dir) = config::global_config_dir() {
                    if crate::sync::is_synced(&dir) {
                        match crate::sync::commit_push(
                            &dir,
                            "loadout: retire ambient learning",
                            PUSH_TIMEOUT,
                        ) {
                            Ok(crate::sync::PushOutcome::Pushed) => out.push(format!(
                                "  {} pushed the change — your other machines pick it up at their next launch.",
                                p.dim("·")
                            )),
                            Ok(crate::sync::PushOutcome::NothingToPush) => {}
                            Ok(crate::sync::PushOutcome::Diverged) | Err(_) => {
                                crate::warn_user!(
                                    "turned learning off here, but couldn't push it. Run `load sync` \
                                     so your other machines stop harvesting too."
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                clean_pass = false;
                crate::warn_user!("could not turn off [learn] in the global config: {e}");
            }
        }
    }

    // 2. Delete the per-machine activation ack. Control state, not user data:
    //    with the flag off and the ack gone, no binary that still knows the
    //    concept can consider learning active here.
    if had_ack {
        match std::fs::remove_file(learn_dir.join("activation.json")) {
            Ok(()) => out.push(format!("  {} deactivated this machine.", p.dim("·"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                clean_pass = false;
                crate::warn_user!("could not remove the learning activation ack: {e}");
            }
        }
    }

    // 3. Deregister the session-end hooks.
    for note in apply_hook_removals(&hook_plan) {
        out.push(format!("  {} {}", p.dim("·"), note));
    }

    // 4. Point at the data. Never delete it: the inbox lives inside your synced
    //    config git repo, and removing files there is your call, not ours.
    if had_data {
        out.push(format!(
            "  {} your harvested suggestions are untouched and no longer used:",
            p.dim("·")
        ));
        if learn_data {
            out.push(format!("      {}", learn_dir.join("evidence").display()));
        }
        if inbox_data {
            if let Some(dir) = inbox_dir.as_deref() {
                out.push(format!("      {}", dir.display()));
            }
        }
        out.push(format!(
            "    {} safe to delete whenever you like.",
            p.dim("·")
        ));
    }

    // Write the marker only after a clean pass, so a failed step is retried on
    // the next command instead of being silently skipped forever.
    if clean_pass {
        if let Err(e) = std::fs::create_dir_all(&state).and_then(|()| {
            std::fs::write(state.join(MARKER), "ambient learning, removed in 0.21.0\n")
        }) {
            crate::warn_user!("could not record that the learning cleanup ran: {e}");
        }
    }
    out
}

/// Whether `dir` holds anything at all (one entry is enough).
fn has_any_file(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Whether the learning state dir holds anything the user would call *theirs*.
///
/// Only the extracted evidence counts. Everything else in `learn/` is
/// bookkeeping the feature kept for itself — the activation ack, the machine
/// id, throttle stamps, the run log, the failure counter, session-eligibility
/// hints, a scratch dir — and none of it means anything once the feature is
/// gone. An allowlist rather than a denylist of those names, so a file this
/// release never knew about can't be mistaken for user data.
fn has_harvested_evidence(dir: &std::path::Path) -> bool {
    has_any_file(&dir.join("evidence"))
}

/// Whether the global config still carries `[learn] enabled = true`. Read
/// straight from the file rather than through `Config`, because the `[learn]`
/// schema is deleted in the same release that adds this module.
fn learn_flag_is_on() -> bool {
    let Some(path) = config::global_config_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    toml::from_str::<toml::Table>(&text)
        .ok()
        .and_then(|t| t.get("learn")?.as_table()?.get("enabled")?.as_bool())
        .unwrap_or(false)
}

/// Set `[learn] enabled = false` in the global config, preserving comments and
/// formatting. Ported from the `load learn off` path this replaces.
fn set_learn_enabled_false() -> crate::Result<PathBuf> {
    use anyhow::{anyhow, Context as _};

    let path = config::global_config_path()
        .ok_or_else(|| anyhow!("cannot resolve the global config path (no home)"))?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {} before turning [learn] off", path.display()))?;
    if !doc.contains_key("learn") {
        return Ok(path); // nothing to turn off
    }
    doc["learn"]["enabled"] = toml_edit::value(false);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::writer::atomic_write(&path, &doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// One planned hook removal: which agent's entry, which file, and the rewritten
/// JSON that takes it out.
struct HookRemoval {
    agent: &'static str,
    path: PathBuf,
    updated: String,
}

/// Work out which hook entries would actually be removed, without writing
/// anything.
///
/// Deliberately runs the **same matchers** that do the writing, rather than a
/// cheaper approximation like a substring search. A `contains` pre-check would
/// fire on a foreign command that merely mentions those words without ending in
/// the suffix, and the cleanup would then announce itself and remove nothing.
/// Computing the real answer once means the "is there anything to do?" question
/// and the removal can never disagree.
fn plan_hook_removals() -> Vec<HookRemoval> {
    let Some(home) = config::home_dir() else {
        return Vec::new();
    };
    let mut plan = Vec::new();
    for &(agent, file, event, subcommand, nested) in LEARN_HOOKS {
        let path = home.join(file);
        let Ok(existing) = std::fs::read_to_string(&path) else {
            continue; // absent or unreadable → nothing to remove, never clobber
        };
        let removed = if nested {
            hooks_claude::remove_claude_hook(&existing, subcommand, Some(event))
        } else {
            remove_hook_command(&existing, subcommand, Some(event))
        };
        match removed {
            Ok(Some(updated)) => plan.push(HookRemoval {
                agent,
                path,
                updated,
            }),
            Ok(None) => {} // no entry of ours present
            Err(_) => crate::warn_user!(
                "{} is not valid JSON — leaving it alone. The retired {agent} session-end \
                 hook entry may still be in it; it is inert and safe to delete by hand.",
                path.display()
            ),
        }
    }
    plan
}

/// Write out a plan from [`plan_hook_removals`], returning a note per file
/// actually changed.
///
/// The matching behind the plan is scoped two ways so nothing else in these
/// shared, user-owned files is at risk: to the exact command suffix, and to the
/// one event the entry was registered under. Cursor's *freshness* hook lives in
/// the same `hooks.json` and survives — it sits on a different event and
/// carries the shorter ` hook cursor` suffix, which cannot match the longer
/// ` hook cursor --event session-end` we remove.
///
/// A one-time `.loadout-bak` backup precedes the first edit of each file.
/// Corrupt JSON never reaches here: [`plan_hook_removals`] leaves it strictly
/// alone and warns. The no-op shim in `commands::hook` means a hook we fail to
/// remove still cannot break a session, so refusing to guess is always the
/// safer half of the trade.
fn apply_hook_removals(plan: &[HookRemoval]) -> Vec<String> {
    let mut notes = Vec::new();
    for r in plan {
        let bak = r.path.with_extension("json.loadout-bak");
        if !bak.exists() {
            let _ = std::fs::copy(&r.path, &bak);
        }
        if crate::writer::atomic_write(&r.path, &r.updated).is_ok() {
            notes.push(format!(
                "removed the {} session-end hook from {}",
                r.agent,
                r.path.display()
            ));
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A Cursor `hooks.json` carrying BOTH loadout hooks — the freshness one on
    /// `sessionStart` and the retired learning one on `stop` — plus a foreign
    /// entry whose command deliberately ends in the same words as ours.
    fn cursor_hooks() -> String {
        json!({
            "version": 1,
            "hooks": {
                "sessionStart": [ { "command": "/usr/local/bin/load hook cursor" } ],
                "stop": [
                    { "command": "/usr/local/bin/load hook cursor --event session-end" },
                    { "command": "/opt/other/tool run" }
                ],
                "afterEdit": [ { "command": "/opt/vendor/wrap hook cursor --event session-end" } ]
            }
        })
        .to_string()
    }

    #[test]
    fn removal_takes_the_learn_hook_and_leaves_the_freshness_hook() {
        let out = remove_hook_command(
            &cursor_hooks(),
            "hook cursor --event session-end",
            Some("stop"),
        )
        .unwrap()
        .expect("the learn entry must be found");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();

        // Ours is gone from `stop`, the co-located foreign entry stays.
        let stop = v["hooks"]["stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "only the foreign entry should remain: {out}");
        assert_eq!(stop[0]["command"], "/opt/other/tool run");

        // The freshness hook is untouched.
        assert_eq!(
            v["hooks"]["sessionStart"][0]["command"], "/usr/local/bin/load hook cursor",
            "the freshness hook must survive the learning cleanup"
        );
    }

    #[test]
    fn removal_is_scoped_to_its_own_event() {
        // The `afterEdit` entry ends with the exact suffix we match on, but sits
        // on an event we never registered under. Suffix matching alone would
        // delete someone else's hook.
        let out = remove_hook_command(
            &cursor_hooks(),
            "hook cursor --event session-end",
            Some("stop"),
        )
        .unwrap()
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hooks"]["afterEdit"][0]["command"],
            "/opt/vendor/wrap hook cursor --event session-end",
            "a same-suffix entry on a different event must survive"
        );
    }

    #[test]
    fn claude_removal_is_scoped_to_session_end() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "SessionEnd": [ { "hooks": [
                    { "type": "command", "command": "/usr/local/bin/load hook claude --event session-end" }
                ] } ],
                "PreToolUse": [ { "hooks": [
                    { "type": "command", "command": "/opt/vendor/x hook claude --event session-end" }
                ] } ]
            }
        })
        .to_string();

        let out = hooks_claude::remove_claude_hook(
            &existing,
            "hook claude --event session-end",
            Some("SessionEnd"),
        )
        .unwrap()
        .expect("the SessionEnd entry must be found");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert!(
            v["hooks"].get("SessionEnd").is_none(),
            "the container we emptied should go: {out}"
        );
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/opt/vendor/x hook claude --event session-end",
            "a same-suffix entry on a different event must survive"
        );
        assert_eq!(v["model"], "opus", "foreign keys must be preserved");
    }

    #[test]
    fn corrupt_hooks_json_is_never_rewritten() {
        assert!(
            remove_hook_command(
                "{ not json",
                "hook cursor --event session-end",
                Some("stop")
            )
            .is_err(),
            "corrupt JSON must surface an error, not a guessed rewrite"
        );
    }
}
