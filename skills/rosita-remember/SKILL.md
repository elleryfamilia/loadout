---
name: rosita-remember
description: Save a durable, cross-project user preference into rosita's global fragments/profiles — or update the fragment it contradicts. Use when the user states lasting guidance that should follow them across projects and agents ("always X", "never Y", "stop doing Z"), especially when it conflicts with guidance the rosita context block already injects. Not a replacement for your normal memory — project-specific, session-specific, or factual notes stay there.
when_to_use: The user gives feedback or states a preference that is (a) about how agents should work, (b) durable, and (c) not specific to the current project — or explicitly asks to "remember this in rosita", "make this global", or "update my rosita guidance".
---

# Remember durable guidance in rosita

rosita injects the user's **global** agent guidance from
`~/.config/rosita/config.toml`: reusable **fragments** composed into
stack-targeted **profiles**. When the user states a preference that should
outlive this session and this project, the right home for it is a fragment —
not a repo CLAUDE.md, and not your agent-local memory.

Read [reference.md](reference.md) for the exact TOML schema before editing.

## Decision boundary — when this skill applies

Apply the test in order; stop at the first match:

1. **Project- or session-specific?** ("this repo uses pnpm", "call it Foo in
   this PR") → your normal memory / the repo's own files. **Not this skill.**
2. **A fact, not guidance?** (who the user is, an URL, a deadline) → normal
   memory. **Not this skill.**
3. **Durable cross-project guidance that contradicts or refines something the
   rosita context block already says?** → **edit that fragment** (the strongest
   signal: the user is correcting guidance rosita itself injected).
4. **Durable cross-project guidance with no matching fragment?** → offer a
   **new fragment** (and ask which profiles should compose it).

When unsure whether it's durable or global, ask the user one short question
rather than guessing. Saving to rosita *and* your own memory is redundant —
prefer rosita for anything that passes the test, since it reaches every agent.

## Orientation (run these probes first)

```bash
# rosita on PATH?
command -v rosita >/dev/null 2>&1 && rosita --version || echo "NOT INSTALLED — stop; suggest installing rosita"
# Current fragments (ids + descriptions)
rosita fragments 2>/dev/null
# The active profile for this repo (what's actually injected here)
rosita explain 2>/dev/null | head -40
```

Also check the rosita context block already in your conversation (it starts
with "What is rosita?" / "rosita snapshot") — the section headings there map to
fragment descriptions, which tells you *which* fragment the user is correcting.

## Process

1. **Pin down the guidance.** Restate it in one sentence and confirm scope
   with the user if ambiguous: all projects, or one stack (rust/node/…)?

2. **Find the target fragment.** Match the guidance against `rosita fragments`
   and the in-context overlay. Correction of existing guidance → that fragment.
   New topic → a new fragment (kebab `id`, short `description`).

3. **Propose the edit before writing.** Show: the fragment id, the exact new
   or changed `guidance` text (condensed, faithful — don't editorialize), and —
   for a new fragment — which profiles will compose it. If the agreed scope has
   no profile yet (e.g. the first node-specific rule and no profile targets
   `node`), propose creating one: a `[[profiles]]` block with the right
   `targets` composing the new fragment (see reference.md). Get explicit
   approval.

4. **Write to `~/.config/rosita/config.toml`** (the global config — never a
   repo's `.rosita/`):
   - Back it up first: `cp ~/.config/rosita/config.toml ~/.config/rosita/config.toml.bak`.
   - **Edit minimally** — change only the target fragment's `guidance` (or
     append one `[[fragments]]` block and add its id to the agreed profiles).
     Preserve all other content, comments, and formatting.
   - Machine-specific literals (hostnames, IPs, usernames) belong in
     `~/.config/rosita/local.toml` under `[fragment_params.<id>]`, not the
     shareable public config.

5. **Validate** (and fix anything flagged):
   - `rosita doctor` — healthy, no leak-lint findings.
   - `rosita fragments show <id>` — the new text reads back correctly.
   - Suggest `rosita refresh` so the running repo's overlay picks it up now
     (other repos refresh on their next `rosita run`).

6. **Tell the user where it landed** — fragment id, profiles affected, and
   that it now applies across all their agents and machines (if they sync).

## Rules

- **Confirm before writing**, and back up `config.toml` first.
- **Minimal diffs.** Never reorder, rewrite, or delete config the user didn't
  ask to change.
- **Fragments and profiles are global-only.** Never write them into a repo's
  `.rosita/`.
- **Stay faithful.** Capture the user's preference as stated; condense, don't
  invent policy or generalize beyond what they said.
- **Don't hoard.** If it fails the decision boundary, say so and use your
  normal memory instead — this skill is for guidance rosita should inject.
