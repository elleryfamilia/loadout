//! Remover for loadout's entry in Claude Code's `.claude/settings.json` hooks —
//! the **nested matcher schema**, a different on-disk shape from Cursor's flat
//! `hooks.json`.
//!
//! Nothing registers here any more. Claude's only hook was ambient learning's
//! `SessionEnd` entry, removed in 0.21.0; all that survives is the remover the
//! one-time cleanup in [`crate::legacy`] needs to take that entry back out.
//! This module goes when that cleanup retires.
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
//! matched by its ` <subcommand>` command suffix, and containers **we created
//! and then emptied** (our matcher group, an otherwise-empty `SessionEnd`
//! array, an otherwise-empty `hooks` object) are removed — while any foreign
//! sibling is kept.

use anyhow::Context as _;
use serde_json::Value;

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

/// Strip our SessionEnd command entry (matched by the ` <subcommand>` suffix) from
/// the nested settings JSON, then remove any container **we emptied** — our matcher
/// group, an event array that became empty through our removal, and the `hooks`
/// object when it became empty solely through those removals — while leaving
/// foreign siblings (including a foreign *pre-existing empty* event array) and
/// every other key byte-value identical. Returns the new JSON, or `Ok(None)` when
/// no entry of ours was present. Deliberately ignores `disableAllHooks`:
/// deregistration always cleans up our entry, even while hooks are globally
/// disabled — leaving a dead entry behind would be worse.
///
/// `only_event` restricts the scan to a single lifecycle event (`Some("SessionEnd")`);
/// `None` scans them all. Pass `Some` whenever the caller knows which event it
/// registered under, so ownership never rests on the command suffix alone.
pub fn remove_claude_hook(
    existing: &str,
    subcommand: &str,
    only_event: Option<&str>,
) -> anyhow::Result<Option<String>> {
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
        // `only_event` narrows removal to the one lifecycle event the entry was
        // registered under (e.g. `SessionEnd`). Without it, ownership rests on
        // the command suffix alone, so a foreign hook on an unrelated event
        // whose command happens to end in the same words would be removed.
        if only_event.is_some_and(|want| want != event) {
            continue;
        }
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
    use serde_json::json;

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

    /// Removal against a realistically dense, user-owned settings.json: our
    /// entry goes, and every foreign key survives at the VALUE level (serde may
    /// reorder keys, so this asserts values, not bytes). This is the shape that
    /// matters — a real `.claude/settings.json` carries permissions, plugins,
    /// mcpServers and other tools' hooks alongside ours.
    #[test]
    fn remove_from_a_dense_file_preserves_every_foreign_value() {
        let mut before = dense();
        // Splice our entry in beside the foreign SessionEnd sibling.
        before["hooks"]["SessionEnd"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "hooks": [ { "type": "command", "command": CMD, "timeout": 10 } ] }));

        let out = remove_claude_hook(&before.to_string(), SUB, Some("SessionEnd"))
            .unwrap()
            .expect("ours present → a write");
        let after: Value = serde_json::from_str(&out).unwrap();

        // Every foreign top-level key survives value-identical.
        let original = dense();
        for key in [
            "env",
            "permissions",
            "model",
            "enabledPlugins",
            "statusLine",
            "mcpServers",
        ] {
            assert_eq!(after[key], original[key], "foreign key {key} must survive");
        }
        // Both foreign hooks survive; ours is gone.
        assert_eq!(
            after["hooks"]["PreToolUse"],
            original["hooks"]["PreToolUse"]
        );
        assert_eq!(
            after["hooks"]["SessionEnd"],
            original["hooks"]["SessionEnd"]
        );
        assert!(!out.contains(CMD), "our entry must be gone: {out}");
    }

    // Remove drops the group we emptied but keeps a foreign SessionEnd sibling
    // and every other key.
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
        let out = remove_claude_hook(&existing, SUB, None)
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
        assert!(remove_claude_hook(&out, SUB, None).unwrap().is_none());
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
        let out = remove_claude_hook(&existing, SUB, None)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.get("hooks").is_none(),
            "empty loadout-created `hooks` container removed: {v}"
        );
        assert_eq!(v["model"], "keep-me", "foreign key preserved");
    }

    // Parity with the flat writer: malformed JSON errors rather than clobbering.
    #[test]
    fn garbage_json_errors_rather_than_clobbering() {
        assert!(remove_claude_hook("not json", SUB, None).is_err());
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
        let out = remove_claude_hook(&existing, SUB, None)
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
        let out = remove_claude_hook(&existing, SUB, None)
            .unwrap()
            .expect("ours present → a change");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["hooks"].get("SessionEnd").is_none(), "ours removed: {v}");
        assert_eq!(v["hooks"]["PostToolUse"], json!([]), "foreign kept: {v}");
        assert_eq!(v["model"], "keep-me");
        // Idempotent: nothing of ours left, foreign shape untouched.
        assert!(remove_claude_hook(&out, SUB, None).unwrap().is_none());
    }

    // Structural type confusion: never destroy what we don't understand.
    #[test]
    fn non_string_command_entry_is_preserved_untouched() {
        let weird = json!({ "type": "command", "command": 42 });
        let existing = json!({
            "hooks": { "SessionEnd": [ { "hooks": [ weird.clone() ] } ] }
        })
        .to_string();
        // A non-string `command` is never mistaken for ours, so there is
        // nothing to remove and the file is left exactly as it was.
        assert!(
            remove_claude_hook(&existing, SUB, None).unwrap().is_none(),
            "nothing of ours present → no write"
        );
        let v: Value = serde_json::from_str(&existing).unwrap();
        let se = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(se.len(), 1);
        assert_eq!(se[0]["hooks"][0], weird, "weird entry preserved untouched");
    }

    #[test]
    fn non_object_root_noops_on_remove() {
        assert!(remove_claude_hook("[1, 2]", SUB, None).unwrap().is_none());
    }
}
