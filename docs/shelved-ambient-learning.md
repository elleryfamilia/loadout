# Shelved: ambient learning

Ambient learning was removed in **0.19.0**. It shipped in 0.16.0 and was
refined through 0.18.0. This note records what it did, why it came out, and how
to bring it back.

## What it did

It mined your own recent agent session transcripts — Claude Code, Codex, and
Gemini CLI — for durable, cross-project preferences you had already stated once,
and staged what it found as candidates you reviewed in the studio inbox. Nothing
reached a profile without an explicit promote.

Mechanically: a session-end hook in `~/.claude/settings.json` and
`~/.cursor/hooks.json` marked the just-ended session eligible and woke a
throttled worker. The worker made at most one bounded extraction call per
six-hour tick, redacted secrets before measuring or sending anything, and
appended candidates to a per-machine journal inside the synced config directory.
Claim text synced; the verbatim quote behind each claim stayed on the machine
that observed it.

User-facing surface: `load learn on|off|status|reset`, `load harvest`, the
studio inbox drawer and its badge, a Learning section in `load doctor`, and a
"N staged suggestions await review" line in the generated context header.

## Why it came out

The feature was large relative to what it returned — roughly 15,000 lines of
dedicated code, plus integration points in about twenty other files. It threaded
a pending-candidate count through the render pipeline, the apply path, the
studio, and doctor, and that plumbing had to be reasoned about on every
unrelated change. The judgment was that the cost was not being repaid.

This is a shelving, not a repudiation. The design still looks sound; it was the
carrying cost that did not justify itself.

## Where it lives now

| Ref | Contents |
| --- | --- |
| `archive/ambient-learning` (annotated tag) | The complete implementation as of v0.18.0, plus the design documents |
| `shelf/ambient-learning` (branch) | Same commit; a named branch so it survives a tag sweep |

Both point at the same commit. Neither name contains a version number, so
neither matches the release workflow's tag filter
(`**[0-9]+.[0-9]+.[0-9]+*` in `.github/workflows/release.yml`) and neither can
trigger a release build.

The design record is on that commit under `.loadout/workflow/artifacts/`, which
main deliberately ignores:

- `design-learning.md` — the original design
- `design-learn-fenced-output.md`, `plan-learn-fenced-output.md` — the
  structured-output fix (PR #33)
- `plan-blocks-learn-fix.md` — the blocks follow-up
- `plan-remove-learning.md` — the removal plan

## Where it shipped

| Commit | PR | Release |
| --- | --- | --- |
| `00741a3` | #30 | v0.16.0 — the feature |
| `f7dab51` | #33 | structured harvest output and diagnostics |
| `c00fa81` | #34 | v0.18.0 — the inbox drawer, bundled with the two-tab nav |

Note that #34 mixes the inbox drawer with the two-tab nav redesign that was
kept, which is why the removal was done by hand rather than as a revert.

## How to restore it

**Preferred: revert the removal commits on main, in reverse order.** They were
deliberately sequenced consumer-first, so reverting them back-to-front restores
each provider before the code that depends on it, and every step compiles.

**Alternative: file-level restore from the tag.**

```sh
git restore --source=refs/tags/archive/ambient-learning -- \
  src/learn/ \
  src/commands/learn.rs \
  src/commands/harvest.rs \
  src/studio/inbox.rs \
  src/adapters/hooks_claude.rs \
  tests/learn.rs \
  tests/fixtures/learn/
```

That recovers the files that existed only for learning. It does **not** restore
the integration points in `src/cli.rs`, `src/config.rs`, `src/adapters/mod.rs`,
`src/render/`, `src/commands/`, and `src/studio/` — take those from the revert
path or re-apply them by hand against the tag's versions.

**Do not merge `shelf/ambient-learning` to restore the feature.** Relative to
its merge base that branch only *adds* documents; the learning code on it is
unchanged. Merging it after main deleted that code preserves the deletions and
brings back nothing but the design documents.

## What was left behind on purpose

- `tests/fixtures/plan/learning-v0-15.json` — a plan-viewer fixture that happens
  to hold the learning plan. It exercises the renderer against a real 23-task
  plan and has nothing to do with the learning code.
- `src/lint.rs::find_injection` — a general prompt-injection lint that learning
  used but does not own.
- `"learn"` in `config.rs`'s `KNOWN_TOP_LEVEL` — a tombstone, so a synced config
  still carrying a `[learn]` table loads without an unrecognized-key warning.
- `load hook <agent> --event session-end` — accepted as a no-op that exits 0, so
  a stale hook on a machine that upgraded past 0.19 without running it cannot
  fail a session.
- Your data. `~/.local/state/loadout/learn/` and `~/.config/loadout/inbox/` are
  left in place; the upgrade points at them once and says they are safe to
  delete.
