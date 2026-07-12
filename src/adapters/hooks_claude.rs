//! Writer for Claude Code's `.claude/settings.json` hooks — the **nested matcher
//! schema**, a different on-disk shape from Cursor's flat `hooks.json`.
//!
//! Claude Code stores hooks as:
//!
//! ```json
//! { "hooks": { "SessionEnd": [ { "hooks": [ { "type": "command",
//!   "command": "…", "timeout": 10 } ] } ] } }
//! ```
//!
//! Each event (`SessionEnd`, `PreToolUse`, …) maps to an array of **matcher
//! groups**; each group has an optional `matcher` and a `hooks` array of
//! `{ type, command, timeout? }` entries. Our SessionEnd learning hook uses **no
//! matcher** (fires for every end reason) and a short `timeout: 10` (SessionEnd's
//! default is 600s; our handler only runs the fast path plus a millisecond spawn).
//!
//! `.claude/settings.json` is **user-owned and shared** with Claude Code itself
//! and other tools (it holds `env`, `permissions`, `model`, `mcpServers`, …). The
//! safety property is **semantic preservation**: every foreign key and value
//! survives a round-trip. It is *not* byte preservation — serde_json re-serializes
//! the document and may reorder object keys and normalize whitespace. What is
//! guaranteed is that no foreign key/value is altered or dropped, our entry is
//! matched by its ` <subcommand>` command suffix (so a moved binary is repointed,
//! not duplicated), and containers **we created and then emptied** (our matcher
//! group, an otherwise-empty `SessionEnd` array, an otherwise-empty `hooks`
//! object) are removed on deregistration — while any foreign sibling is kept.
//!
//! When Claude Code's top-level `disableAllHooks: true` is set, every hook is
//! inert, so [`upsert_claude_hook`] writes nothing and the caller surfaces
//! [`DISABLE_ALL_HOOKS_NOTE`].

use anyhow::{anyhow, bail, Context as _};
use serde_json::{json, Map, Value};

/// Note the caller surfaces when Claude Code has `disableAllHooks: true` — our
/// SessionEnd hook would never fire, so learning relies on entry-point triggers.
pub const DISABLE_ALL_HOOKS_NOTE: &str =
    "claude hooks disabled by disableAllHooks — learning will rely on entry-point triggers";

/// True when the settings JSON has a top-level `disableAllHooks: true`. A missing
/// key, a non-boolean value, `false`, or unparseable input all read as "not
/// disabled" (we still register, and an unparseable file surfaces its parse error
/// through [`upsert_claude_hook`] instead).
pub fn hooks_disabled(existing: &str) -> bool {
    serde_json::from_str::<Value>(existing)
        .ok()
        .and_then(|v| v.get("disableAllHooks").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// True when `command` is a loadout entry, i.e. its `command` string ends with
/// the ` <subcommand>` suffix. An exact suffix match, so a foreign command that
/// merely *contains* the subcommand text is never mistaken for ours.
fn is_ours(entry: &Value, suffix: &str) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .map(|c| c.ends_with(suffix))
        .unwrap_or(false)
}

/// Ensure a `{ type: "command", command, timeout: 10 }` entry running `command`
/// exists under `hooks.<event>` in Claude Code's nested settings JSON, matched by
/// the ` <subcommand>` command suffix. A moved binary is repointed **in place**
/// (its group and timeout preserved); an already-current entry returns `Ok(None)`
/// (no churn). `disableAllHooks: true` returns `Ok(None)` (register nothing).
/// Every foreign key/value is preserved. Returns the new pretty-printed JSON.
pub fn upsert_claude_hook(
    existing: &str,
    event: &str,
    subcommand: &str,
    command: &str,
) -> anyhow::Result<Option<String>> {
    let mut root: Value = if existing.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(existing).context("parsing existing .claude/settings.json")?
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!(".claude/settings.json root is not a JSON object"))?;

    // `disableAllHooks: true` → every hook is inert; register nothing (the caller
    // surfaces DISABLE_ALL_HOOKS_NOTE).
    if obj.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }

    let groups = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` is not a JSON object"))?
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(groups) = groups else {
        bail!("`hooks.{event}` is not an array");
    };

    let suffix = format!(" {subcommand}");
    let mut handled = false;
    let mut no_change = false;
    // Find our command inside any matcher group's `hooks` array and repoint it in
    // place (preserving the group, its matcher, and the entry's timeout).
    for group in groups.iter_mut() {
        let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(cmd) = inner.iter_mut().find(|c| is_ours(c, &suffix)) {
            handled = true;
            if cmd.get("command").and_then(Value::as_str) == Some(command) {
                no_change = true; // already current with this binary
            } else {
                cmd.as_object_mut()
                    .ok_or_else(|| anyhow!("hook command entry is not an object"))?
                    .insert("command".into(), json!(command));
            }
            break;
        }
    }
    if !handled {
        // Not present: append a fresh matcher group with no matcher (fires for all
        // end reasons) and our short timeout.
        groups.push(json!({
            "hooks": [ { "type": "command", "command": command, "timeout": 10 } ]
        }));
    }

    if no_change {
        return Ok(None);
    }
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

/// Strip our SessionEnd command entry (matched by the ` <subcommand>` suffix) from
/// the nested settings JSON, then remove any container **we emptied** — our matcher
/// group, an event array that became empty through our removal, and the `hooks`
/// object when it became empty solely through those removals — while leaving
/// foreign siblings (including a foreign *pre-existing empty* event array) and
/// every other key byte-value identical. Returns the new JSON, or `Ok(None)` when
/// no entry of ours was present. Deliberately ignores `disableAllHooks`:
/// deregistration always cleans up our entry, even while hooks are globally
/// disabled — leaving a dead entry behind would be worse.
pub fn remove_claude_hook(existing: &str, subcommand: &str) -> anyhow::Result<Option<String>> {
    let mut root: Value =
        serde_json::from_str(existing).context("parsing existing .claude/settings.json")?;
    let suffix = format!(" {subcommand}");

    let Some(root_obj) = root.as_object_mut() else {
        return Ok(None); // not an object → nothing of ours to touch
    };
    let Some(hooks) = root_obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(None); // no hooks section → nothing to remove
    };

    let mut removed = false;
    let mut empty_events: Vec<String> = Vec::new();
    for (event, groups_val) in hooks.iter_mut() {
        let Some(groups) = groups_val.as_array_mut() else {
            continue;
        };
        // Drop our command from each group's `hooks` array; drop a whole group only
        // when OUR removal is what emptied it (never a foreign group, never a
        // pre-existing empty one).
        let mut removed_here = false; // did OUR removal touch this event?
        let mut kept: Vec<Value> = Vec::with_capacity(groups.len());
        for mut group in groups.drain(..) {
            let mut we_emptied = false;
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|c| !is_ours(c, &suffix));
                if inner.len() != before {
                    removed = true;
                    removed_here = true;
                    we_emptied = inner.is_empty();
                }
            }
            if !we_emptied {
                kept.push(group);
            }
        }
        *groups = kept;
        // Only an event array OUR removal emptied is a loadout-created container;
        // a foreign pre-existing empty array (e.g. `PreToolUse: []`) is not ours
        // to delete.
        if removed_here && groups.is_empty() {
            empty_events.push(event.clone());
        }
    }

    if !removed {
        return Ok(None); // nothing of ours was present
    }
    // Remove event arrays our removal emptied, then the `hooks` object — but only
    // when dropping those events is what left it empty (any surviving foreign
    // event, even an empty one, keeps the object alive).
    for event in &empty_events {
        hooks.remove(event);
    }
    if hooks.is_empty() {
        root_obj.remove("hooks");
    }

    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB: &str = "hook claude --event session-end";
    const CMD: &str = "\"/usr/local/bin/load\" hook claude --event session-end";

    /// A realistic-shaped `.claude/settings.json` densely populated with FOREIGN
    /// keys (synthetic — never the real file's content), including a foreign hook
    /// under another event and a foreign `SessionEnd` sibling group.
    fn dense() -> Value {
        json!({
            "env": { "SOME_FLAG": "1" },
            "permissions": {
                "allow": ["Bash(ls:*)", "WebSearch"],
                "defaultMode": "auto"
            },
            "model": "claude-fable-5[1m]",
            "enabledPlugins": { "figma@x": true, "posthog@x": false },
            "statusLine": { "type": "command", "command": "\"/opt/status\" render" },
            "mcpServers": {
                "savio": { "command": "npx", "args": ["-y", "@scope/savio"] }
            },
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash",
                      "hooks": [ { "type": "command", "command": "\"/opt/tool\" guard" } ] }
                ],
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": "\"/opt/other\" wrapup" } ] }
                ]
            }
        })
    }

    // (1) Empty file → creates the nested SessionEnd shape with no matcher.
    #[test]
    fn upsert_into_empty_file_creates_nested_shape() {
        let out = upsert_claude_hook("", "SessionEnd", SUB, CMD)
            .unwrap()
            .expect("empty file → a write");
        let v: Value = serde_json::from_str(&out).unwrap();
        let groups = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0].get("matcher").is_none(),
            "no matcher → fires for all end reasons"
        );
        let inner = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["type"], "command");
        assert_eq!(inner[0]["command"], CMD);
        assert_eq!(inner[0]["timeout"], 10);
    }

    // (2) Dense foreign settings.json → every foreign VALUE survives (serde may
    //     reorder keys; we assert value-level, not byte-level, preservation).
    #[test]
    fn upsert_preserves_every_foreign_value() {
        let before = dense();
        let out = upsert_claude_hook(&before.to_string(), "SessionEnd", SUB, CMD)
            .unwrap()
            .expect("ours is not present yet → a write");
        let after: Value = serde_json::from_str(&out).unwrap();

        for key in [
            "env",
            "permissions",
            "model",
            "enabledPlugins",
            "statusLine",
            "mcpServers",
        ] {
            assert_eq!(after[key], before[key], "foreign key `{key}` preserved");
        }
        // Foreign hook under another event untouched.
        assert_eq!(after["hooks"]["PreToolUse"], before["hooks"]["PreToolUse"]);
        // Foreign SessionEnd sibling kept; ours appended alongside it.
        let se = after["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(se.len(), 2, "foreign group kept, ours appended");
        assert_eq!(
            se[0], before["hooks"]["SessionEnd"][0],
            "foreign SessionEnd group value-identical"
        );
        let ours = &se[1]["hooks"][0];
        assert_eq!(ours["command"], CMD);
        assert_eq!(ours["timeout"], 10);
        assert!(
            se[1].get("matcher").is_none(),
            "our appended group carries no matcher"
        );
    }

    // (3) Repoint a moved binary in place (group + timeout preserved).
    #[test]
    fn repoints_moved_binary_in_place() {
        let first = upsert_claude_hook("", "SessionEnd", SUB, CMD)
            .unwrap()
            .unwrap();
        let moved = "\"/new/home/load\" hook claude --event session-end";
        let out = upsert_claude_hook(&first, "SessionEnd", SUB, moved)
            .unwrap()
            .expect("binary moved → an in-place update");
        let v: Value = serde_json::from_str(&out).unwrap();
        let se = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(se.len(), 1, "updated in place, not duplicated");
        let inner = se[0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["command"], moved);
        assert_eq!(inner[0]["timeout"], 10, "timeout preserved on repoint");
    }

    // (4) Idempotent second upsert with the same binary → None (no churn).
    #[test]
    fn second_upsert_same_binary_is_no_churn() {
        let first = upsert_claude_hook("", "SessionEnd", SUB, CMD)
            .unwrap()
            .unwrap();
        assert!(upsert_claude_hook(&first, "SessionEnd", SUB, CMD)
            .unwrap()
            .is_none());
    }

    // (5a) Remove drops the group we emptied but keeps a foreign SessionEnd
    //      sibling and every other key.
    #[test]
    fn remove_drops_our_group_keeps_foreign_sibling() {
        let existing = json!({
            "model": "keep-me",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash",
                      "hooks": [ { "type": "command", "command": "\"/opt\" guard" } ] }
                ],
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": "\"/opt/other\" wrapup" } ] },
                    { "hooks": [ { "type": "command", "command": CMD, "timeout": 10 } ] }
                ]
            }
        })
        .to_string();
        let out = remove_claude_hook(&existing, SUB)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        let se = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(se.len(), 1, "our emptied group dropped");
        assert_eq!(se[0]["hooks"][0]["command"], "\"/opt/other\" wrapup");
        assert_eq!(
            v["hooks"]["PreToolUse"].as_array().unwrap().len(),
            1,
            "foreign event untouched"
        );
        assert_eq!(v["model"], "keep-me", "foreign key preserved");
        // Idempotent: nothing of ours left.
        assert!(remove_claude_hook(&out, SUB).unwrap().is_none());
    }

    // (5b) When ours is the only content, removal cascades group → event → the
    //      `hooks` object away, leaving only foreign keys.
    #[test]
    fn remove_cleans_loadout_created_empty_containers() {
        let existing = json!({
            "model": "keep-me",
            "hooks": {
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": CMD, "timeout": 10 } ] }
                ]
            }
        })
        .to_string();
        let out = remove_claude_hook(&existing, SUB)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.get("hooks").is_none(),
            "empty loadout-created `hooks` container removed: {v}"
        );
        assert_eq!(v["model"], "keep-me", "foreign key preserved");
    }

    // (6) disableAllHooks short-circuits registration; predicate is exact.
    #[test]
    fn disable_all_hooks_short_circuits() {
        let disabled = json!({ "disableAllHooks": true, "model": "x" }).to_string();
        assert!(hooks_disabled(&disabled));
        assert!(
            upsert_claude_hook(&disabled, "SessionEnd", SUB, CMD)
                .unwrap()
                .is_none(),
            "disableAllHooks → register nothing"
        );
        // A false / absent / empty flag does NOT short-circuit.
        assert!(!hooks_disabled(
            &json!({ "disableAllHooks": false }).to_string()
        ));
        assert!(!hooks_disabled(&json!({ "model": "x" }).to_string()));
        assert!(!hooks_disabled(""));
    }

    // Parity with the flat writer: malformed JSON errors rather than clobbering.
    #[test]
    fn garbage_json_errors_rather_than_clobbering() {
        assert!(upsert_claude_hook("not json", "SessionEnd", SUB, CMD).is_err());
        assert!(remove_claude_hook("not json", SUB).is_err());
    }

    // (fix 1a) Reviewer's reproduction: a FOREIGN pre-existing empty event array
    // (`PreToolUse: []`) is not a loadout-created container — removal of our
    // SessionEnd entry must leave it, and therefore the `hooks` object, in place.
    #[test]
    fn remove_keeps_foreign_preexisting_empty_event_array() {
        let existing = json!({
            "hooks": {
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": CMD, "timeout": 10 } ] }
                ],
                "PreToolUse": []
            }
        })
        .to_string();
        let out = remove_claude_hook(&existing, SUB)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        let hooks = v.get("hooks").expect("`hooks` object survives: {v}");
        assert!(
            hooks.get("SessionEnd").is_none(),
            "the event WE emptied is removed: {v}"
        );
        assert_eq!(
            hooks["PreToolUse"],
            json!([]),
            "foreign pre-existing empty event array survives: {v}"
        );
    }

    // (fix 1b) Same invariant with our removal emptying SessionEnd beside a
    // foreign empty PostToolUse: SessionEnd goes, PostToolUse and `hooks` stay.
    #[test]
    fn remove_drops_only_the_event_we_emptied() {
        let existing = json!({
            "model": "keep-me",
            "hooks": {
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": CMD, "timeout": 10 } ] }
                ],
                "PostToolUse": []
            }
        })
        .to_string();
        let out = remove_claude_hook(&existing, SUB)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["hooks"].get("SessionEnd").is_none(), "ours removed: {v}");
        assert_eq!(v["hooks"]["PostToolUse"], json!([]), "foreign kept: {v}");
        assert_eq!(v["model"], "keep-me");
        // Idempotent: nothing of ours left, foreign shape untouched.
        assert!(remove_claude_hook(&out, SUB).unwrap().is_none());
    }

    // (fix 2) Structural type confusion: never destroy what we don't understand.
    #[test]
    fn hooks_not_an_object_errors_no_clobber() {
        let existing = json!({ "hooks": "nope" }).to_string();
        assert!(upsert_claude_hook(&existing, "SessionEnd", SUB, CMD).is_err());
    }

    #[test]
    fn event_not_an_array_errors_no_clobber() {
        let existing = json!({ "hooks": { "SessionEnd": {} } }).to_string();
        assert!(upsert_claude_hook(&existing, "SessionEnd", SUB, CMD).is_err());
    }

    #[test]
    fn non_string_command_entry_is_preserved_untouched() {
        let weird = json!({ "type": "command", "command": 42 });
        let existing = json!({
            "hooks": { "SessionEnd": [ { "hooks": [ weird.clone() ] } ] }
        })
        .to_string();
        // Upsert: the numeric-command entry is never mistaken for ours — it
        // survives value-identical and ours is appended as a fresh group.
        let out = upsert_claude_hook(&existing, "SessionEnd", SUB, CMD)
            .unwrap()
            .expect("ours absent → a write");
        let v: Value = serde_json::from_str(&out).unwrap();
        let se = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(se.len(), 2);
        assert_eq!(se[0]["hooks"][0], weird, "weird entry preserved untouched");
        // Remove from the ORIGINAL (ours not present): nothing of ours → None.
        assert!(remove_claude_hook(&existing, SUB).unwrap().is_none());
    }

    #[test]
    fn non_object_root_errors_on_upsert_and_noops_on_remove() {
        assert!(upsert_claude_hook("[1, 2]", "SessionEnd", SUB, CMD).is_err());
        assert!(remove_claude_hook("[1, 2]", SUB).unwrap().is_none());
    }
}
