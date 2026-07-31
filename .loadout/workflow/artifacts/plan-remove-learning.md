# Plan — remove ambient learning, shelve it for later

Revision 2. Revision 1 was reviewed by codex (gpt-5.6-sol, xhigh) and came back
RETHINK with two blockers; both are fixed here. Changes from revision 1 are
marked **[r2]**.

## Objective

Take the ambient-learning feature out of loadout: the `learn` and `harvest`
commands, the session mining worker, the review inbox, and every place they
touch the rest of the tool. Preserve the whole implementation somewhere it can
be found and restored. Do it without breaking anyone already running it.

## Why

The feature is large relative to what it returns. It is roughly 15,000 lines of
dedicated code plus integration points in about 20 other files, and it threads
a `learn_pending` count through the render pipeline, the apply path, the
studio, and doctor. That plumbing costs something on every unrelated change.

## What "learning" actually is

Files that exist only for learning (deleted outright):

| Path | Lines |
| --- | --- |
| `src/learn/**` (15 files) | 11,072 |
| `tests/cli.rs` learn blocks **[r2]** | ~1,250 |
| `tests/learn.rs` | 1,108 |
| `src/studio/inbox.rs` | 692 |
| `src/commands/learn.rs` | 541 |
| `src/adapters/hooks_claude.rs` **[r2]** | 508 |
| `src/commands/harvest.rs` | 261 |
| `tests/fixtures/learn/**` (16 files) **[r2]** | 67 |
| **Total** | **~15,500** |

**[r2]** Three of these were missing from revision 1:

- `tests/fixtures/learn/**` — 16 transcript fixtures the readers load directly
  (`src/learn/readers/claude.rs:255`).
- `src/adapters/hooks_claude.rs` — Claude Code's nested hooks-file dialect. Its
  only caller is `HookFormat::ClaudeNested`, and the only descriptor using that
  format is Claude's *learn* hook (`src/adapters/mod.rs:313`). Claude has no
  freshness hook registry at all. So once the migration shim retires, the whole
  file goes, and `HookFormat`, `HookPurpose`, and the `format` field go with it.
- `tests/cli.rs` — revision 1 said "~212 lines". That was a count of grep hits,
  not of code. The real coverage is about 1,250 lines in five blocks starting at
  lines 2565, 3569, 3755, 4072, and 4435, plus shared helpers.

Also becoming dead, in files that stay **[r2]**:

- `providers::output_with_timeout_stdin` and the optional-stdin branch in
  `output_bounded` — used only by `learn::agent_cli` (`src/providers/mod.rs:543`).
- `update.rs`'s `pub(crate)` stamp helpers can drop back to private
  (`src/update.rs:96`).

Files that carry learning code but stay (edited):

- `src/cli.rs` — `Learn`/`Harvest` commands, `LearnArgs`, `LearnAction`, `HookArgs::event`
- `src/config.rs` — `LearnConfig`, `LearnScope`, `RawLearn`, repo-layer strip, `KNOWN_TOP_LEVEL`
- `src/adapters/mod.rs` — `learn_hooks`, `HookPurpose`, `HookFormat`, `remove_learn_hooks`, the `learn_active` parameter on `bootstrap_hook_registrations`, `AppliedMeta::learn_pending`
- `src/render/mod.rs`, `src/render/header.rs` — `RenderRequest::learn_pending`, `HeaderMeta::pending_learn`, the header discovery line
- `src/commands/` — `apply.rs`, `run.rs`, `refresh.rs`, `hook.rs`, `sync.rs`, `clean.rs`, `explain.rs`, `doctor.rs`
- `src/studio/` — `server.rs`, `settings.rs`, `edit.rs`, `state.rs`, `views.rs`, `assets/studio.css`
- `src/lint.rs` — doc references only
- `README.md`, `CHANGELOG.md`

Deliberately kept: `tests/fixtures/plan/learning-v0-15.json`. It is a plan-viewer
test fixture that happens to hold the learning plan; it exercises the renderer
against a real 23-task plan and has nothing to do with the learning code.
Renaming it would be churn. **[r2]** Noted here so it doesn't read as an oversight.

Where it shipped: `00741a3` (PR #30, v0.16.0), `f7dab51` (PR #33), and the inbox
drawer inside `c00fa81` (PR #34, v0.18.0).

**A revert will not work.** PR #34 combines the inbox drawer with the two-tab
nav redesign we are keeping, so the removal is done by hand, commit by commit.

## The part that is easy to get wrong

Learning leaves four things on a user's machine that outlive the binary:

1. **The `[learn] enabled` flag** in `~/.config/loadout/config.toml`, which
   syncs to every other machine through `load sync`.
2. **The per-machine activation ack**, `~/.local/state/loadout/learn/activation.json`.
3. **Session-end hook entries** in `~/.claude/settings.json` and
   `~/.cursor/hooks.json`, each running `load hook <agent> --event session-end`.
4. **Data** — the rest of `~/.local/state/loadout/learn/`, and
   `~/.config/loadout/inbox/` inside the synced config git repo.

**[r2] Items 1 and 2 are the control gates, and revision 1 got this wrong.**
`learn::state::learn_active` is `cfg.learn.enabled && activation ack exists`
(`src/learn/state.rs:159`), and `run`, `refresh`, and `sync` all re-register the
hooks whenever it is true. Revision 1 proposed stripping the hooks but leaving
both gates set. That fails in two ways:

- A **second machine still on 0.18** pulls the synced config, sees
  `enabled = true`, and keeps harvesting — and keeps re-adding its own hooks.
- **Downgrading** an upgraded machine back to 0.18 silently switches learning
  back on and re-registers the hooks.

So the migration has to turn the feature off, not just sweep up after it.

Item 3 is the visible one: a stale hook fires a command the new binary rejects,
so the agent reports a failing hook at the end of every session.

## Approach

### Phase 0 — Shelve

Do this before editing anything.

1. Commit the four untracked design documents onto a shelf branch. They are in
   `.loadout/workflow/artifacts/` (`design-learning.md`,
   `design-learn-fenced-output.md`, `plan-learn-fenced-output.md`,
   `plan-blocks-learn-fix.md`) and `.loadout/` is excluded in
   `.git/info/exclude`, so they exist only on this machine and are not in any
   commit. Force-add them (`git add -f`) onto the shelf branch only — main
   keeps ignoring `.loadout/`.
2. Push branch `shelf/ambient-learning` and annotated tag
   `archive/ambient-learning`. **[r2]** Distinct names: revision 1 gave both the
   same name, which makes `git show shelf/ambient-learning` and unqualified
   pushes ambiguous.
3. Write `docs/shelved-ambient-learning.md` on main: what the feature did, why
   it came out, the shelf refs, the three shipping commits, a complete path
   inventory, and the restore command.

**[r2] Restoring is not a merge.** The shelf branch only *adds* documents; the
learning code on it is unchanged from the merge base. Once main deletes that
code, merging the shelf brings back the documents and keeps the deletions.
Revision 1's "merge the shelf branch back" was wrong. The two real restore
paths, both of which go in the doc:

```
# Preferred: revert the removal commits in reverse order.
# Or, file-level restore from the archive tag:
git restore --source=refs/tags/archive/ambient-learning -- <path inventory>
```

**Tag naming matters.** `.github/workflows/release.yml:44` triggers on tags
matching `**[0-9]+.[0-9]+.[0-9]+*`, where `**` crosses slashes and `+` repeats
the preceding pattern. A tag named `shelf/learning-v0.18.0` would match and
start a real cargo-dist release build. Neither `shelf/ambient-learning` nor
`archive/ambient-learning` contains digits, so neither matches.

### Phase 1 — Migration (lands first, while the learning code still exists)

Commit: `feat: retire ambient learning — turn it off and clean up`

**[r2] This phase now turns the feature off, not just tidies after it.** Port
the body of `commands::learn::off` (`src/commands/learn.rs:201`) into a
`legacy` module before deleting the original. It already does the right things
in the right order.

1. **Clear the synced intent flag.** Set `[learn] enabled = false` in the global
   config (after a sync pull, with a backup), so a 0.18 machine that pulls it
   stops harvesting. Do not delete the whole `[learn]` table yet — an explicit
   `false` is a clearer tombstone for older binaries than an absent table, which
   they would read as the default `false` anyway but which loses the signal that
   it was deliberately turned off.
2. **Delete the activation ack** (`learn/activation.json`). It is control state,
   not user data. With the flag false and the ack gone, `learn_active` is false
   on every binary that still knows the concept.
3. **Strip the hook entries** from `~/.claude/settings.json` and
   `~/.cursor/hooks.json`. Same `.loadout-bak` backup, same never-clobber-on-
   corrupt-JSON rule. **[r2]** Match on the expected event as well as the
   subcommand suffix — Cursor `stop`, Claude `SessionEnd` — so a foreign entry
   whose command happens to end in the same words is not removed. Cursor's
   *freshness* hook is already safe (it matches the shorter ` hook cursor`
   suffix, and removal matches the longer ` hook cursor --event session-end`;
   `src/adapters/mod.rs:1780` already tests exactly this cross-match). Warn when
   JSON is corrupt instead of skipping silently.
4. **Make `--event session-end` a no-op that cannot fail.** **[r2]** Today the
   branch sits at `src/commands/hook.rs:59`, *after* `Config::load` and
   `resolve_agent_token` — so a stale hook still exits non-zero if config
   loading fails. Move the check to the top of `hook::run`, before config load,
   repo discovery, and agent resolution. Return `Ok(())` and write nothing.
5. **Keep the shim indefinitely** — **[r2]** not "one more minor release". A
   machine can upgrade 0.18 → 0.21 and never run 0.19. It is about ten lines.
6. **Run cleanup from a common startup path**, not just `run`/`refresh` — **[r2]**
   users who mostly invoke `studio` or `sync` would otherwise never get swept.
7. **Never delete data.** `~/.local/state/loadout/learn/` (minus the ack) and
   `~/.config/loadout/inbox/` stay. Point at them once and say they are safe to
   delete. **[r2]** "Once" needs a mechanism: write a `retired` marker in the
   state dir, print only when the marker is absent, write it after printing,
   skip the write under `--dry-run`, and never print from a hook invocation.

### Phase 2 — Remove the feature

**[r2] Order reversed — consumers first, providers last.** Revision 1 deleted
`commands/harvest.rs` in the first commit while `doctor.rs:408`, `:424`, and
`:435` still call `harvest::format_logged_diagnostic` and
`harvest::format_diagnostic`; and deleted `src/learn/` while `run`, `refresh`,
`studio/server`, `studio/settings`, `studio/edit`, and `tests/cli.rs` still
imported it. Neither commit would have compiled. Consumer-first also makes the
sequence reverse-revertible: restoring a provider always precedes restoring its
consumers.

Each commit must pass build, tests, and clippy on its own. Each commit removes
its own tests alongside its production surface.

1. `refactor: drop the pending-suggestions line from the header` — the header
   line, `HeaderMeta::pending_learn`, `RenderRequest::learn_pending`,
   `apply::learn_pending_count`, and the `learn_pending` argument threaded
   through `apply_for_agents`, `run`, `refresh`, `hook`, `clean`, `explain`.
   Bump `HEADER_VERSION` 3 → 4 in `src/render/mod.rs:184`.
2. `refactor: drop the review inbox from the studio` — the drawer route, the
   badge and its loader, `StagedOp::SetLearnEnabled`, the Settings Learning
   section, `InboxPaths`, `bootstrap_learn_hooks`, the `inbox` icon, the inbox
   CSS, and `src/studio/inbox.rs`. **Keep** the two-tab nav, the recents drawer,
   and the drawer infrastructure.
3. `refactor: drop the learning section from doctor` — `check_learn`,
   `check_learn_at`, `check_learn_at_with_selection`, `check_learn_hook_at`, the
   section header, and the three `harvest::format_*` call sites.
4. `refactor: drop learning triggers from the command paths` — the `maybe_spawn`
   / `Trigger` calls in `run`, `refresh`, `hook`, and `studio/server`, and the
   ambient summary in `refresh`. **[r2]** Revision 1 never assigned these to a
   commit.
5. `refactor: drop the learn and harvest commands` — `src/cli.rs`,
   `src/main.rs`, `src/commands/mod.rs`, and `commands/learn.rs` +
   `commands/harvest.rs`. Keep `HookArgs::event` and the Phase 1 shim.
6. `refactor: drop learn hooks from the adapter layer` — `learn_hooks`,
   `HookPurpose`, `remove_learn_hooks`, and the `learn_active` parameter on
   `bootstrap_hook_registrations`.
7. `refactor: drop [learn] from the config schema` — `LearnConfig`,
   `LearnScope`, `RawLearn`, the repo-layer strip, the sub-table entry in
   `warn_unknown_config_keys`, and the related tests. Keep `"learn"` in
   `KNOWN_TOP_LEVEL` as a tombstone so a synced config that still carries the
   table does not warn.
8. `refactor: drop the learning subsystem` — `src/learn/**`, `tests/learn.rs`,
   `tests/fixtures/learn/**`, the `src/lib.rs` module line, and
   `providers::output_with_timeout_stdin`.

`src/adapters/hooks_claude.rs` survives Phase 2 because the Phase 1 migration
still needs its remover. It is deleted when the migration retires, along with
`HookFormat` and the `format` field. **[r2]** Track that as a follow-up, not as
part of this release.

`src/lint.rs::find_injection` stays. It is a general prompt-injection lint with
its own tests, and keeping a `pub` function costs nothing in a library crate.
Only its doc references to `crate::learn::gate` change.

### Phase 3 — Docs and release

1. `README.md` — delete the Learning section (line ~164) and the two command
   table rows (`load learn`, `load harvest`).
2. `CHANGELOG.md` — a **Removed** entry in Unreleased: what went, why, that the
   feature is turned off and its hooks cleaned up automatically, and that inbox
   data is left in place for the user to delete.
3. Version 0.19.0. The Unreleased section already holds the plan-viewer work,
   so this rides the same release.
4. Recapture the studio Settings screenshot if the Learning section was visible.

## Assumptions

- Ellery is the primary user; there is no large installed base to migrate.
- Other machines may hold a synced config with `[learn] enabled = true`, an
  `inbox/` directory, and possibly an older binary.
- Learning is not coming back soon. If it might return within weeks, a cargo
  feature flag would be the better trade — but that keeps the `#[cfg]` noise
  the removal is meant to eliminate.

## Risks

| Risk | Handling |
| --- | --- |
| A second machine on 0.18 keeps harvesting **[r2]** | Phase 1 clears the synced `enabled` flag |
| A downgrade to 0.18 reactivates learning **[r2]** | Phase 1 also deletes the activation ack |
| A stale session-end hook fires on a machine that skipped 0.19 **[r2]** | Shim returns before config load, kept indefinitely |
| Cleanup damages the Cursor freshness hook, which shares a file | Match on event + exact subcommand suffix; port the existing cross-match test |
| A commit in the sequence does not compile **[r2]** | Consumer-first ordering; gate every commit on build + test + clippy |
| `HEADER_VERSION` 3 → 4 re-renders every overlay once | Expected, and matches what PR #25 already did |
| Inbox removal breaks the recents drawer or the two-tab nav | Studio browser smoke test plus targeted studio tests |
| The shelf tag starts a release build | `shelf/ambient-learning`, `archive/ambient-learning` — no digits |
| The learning design docs are lost | Untracked today; Phase 0 force-adds them |
| Restoring later fails because a merge brings back nothing **[r2]** | Restore is a revert or a `git restore --source=<tag> -- <paths>`, documented with a full path inventory |

## Validation

Automated, before calling it done:

- `cargo build`, `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`
  — **after each Phase 2 commit**, not just at the end
- `load hook claude --event session-end` exits 0 and writes nothing, including
  **[r2]** with an invalid config, no `$HOME`, and malformed stdin
- A config containing `[learn] enabled = true` loads clean, with no
  unrecognized-key warning
- Fixture `~/.claude/settings.json` and `~/.cursor/hooks.json` carrying learn
  entries get them stripped, with the Cursor freshness entry intact and a
  foreign entry ending in similar words left alone **[r2]**
- The retirement notice prints once and not twice, prints on a machine with data
  but no hook, and does not print under `--dry-run` **[r2]**
- Existing studio browser smoke test stays green

Manual, on the real machine:

- Deliberately leave learning **on** at 0.18.0, upgrade to 0.19.0, run
  `load run`, and confirm: the flag flips to `false`, the ack is gone, the hooks
  are stripped, no warning appears, and a Claude session ends without a hook
  error.
- **[r2]** Then check the synced config on a second machine and confirm it reads
  `enabled = false`.

## Rollback

Revert the removal commits in reverse order. `archive/ambient-learning` holds
the complete implementation plus the design documents for file-level restore.
Do not attempt to merge the shelf branch — see Phase 0.

## First implementation step

Phase 0, step 1: create branch `shelf/ambient-learning` from `main` at
`46b7d7c`, `git add -f` the four `.loadout/workflow/artifacts/` learning
documents, commit, then push the branch and the `archive/ambient-learning` tag.
