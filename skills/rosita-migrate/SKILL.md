---
name: rosita-migrate
description: Migrate existing global AI agent instructions into rosita. Use when adopting rosita, importing a CLAUDE.md / AGENTS.md / global agent rules into rosita, setting up ~/.config/rosita/config.toml, or turning prose agent instructions into reusable fragments and stack-targeted profiles.
when_to_use: The user wants to start using rosita, asks to "import my CLAUDE.md", "set up my rosita config", "turn my agent rules into fragments/profiles", or otherwise convert hand-written global agent instructions into rosita's fragment/profile model.
---

# Migrate agent instructions into rosita

rosita manages a user's **global** AI-agent context as reusable **fragments**
composed into stack-targeted **profiles**, all in `~/.config/rosita/config.toml`.
This skill converts existing prose instruction files (a `CLAUDE.md`, `AGENTS.md`,
etc.) into that structure — **additively**. The original files are left
untouched; rosita renders a separate, generated overlay.

Read [reference.md](reference.md) for the full TOML schema and a worked example
before writing any config.

## Orientation (gathered for you)

- rosita on PATH: !`command -v rosita >/dev/null 2>&1 && rosita --version || echo "NOT INSTALLED — install rosita first"`
- Existing global config: !`sed -n '1,60p' ~/.config/rosita/config.toml 2>/dev/null || echo "(none yet — this will be a fresh setup)"`
- Candidate source files: !`for f in ~/.claude/CLAUDE.md ~/.codex/AGENTS.md ~/.config/AGENTS.md ./CLAUDE.md ./AGENTS.md ./.github/copilot-instructions.md; do [ -f "$f" ] && echo "$f"; done; echo "(also ask the user if their global rules live elsewhere)"`

## The model (so you decompose well)

- A **fragment** is ONE coherent unit of context — a single topic: communication
  style, git conventions, guardrails, planning workflow, validation policy,
  tooling preferences, or a piece of live environment context. Fields: `id`
  (kebab-case), `description` (short), `guidance` (the actual instructions).
  Optional: `risk` (`info`/`caution`/`danger`), `tags`, `agents` (restrict to
  certain agents), and for live data a `command` (a bash script) or built-in
  `provider` (`host`, `toolchain`, `ai-tools`, `tailnet`, `docker`).
- A **profile** is a named set of fragments, selected when its `targets`
  match a repo's detected stack (`rust`, `node`, `python`, `go`, …) or the
  no-repo `machine` context. Fragments and profiles are **global** — shared
  across every repo; the profile whose targets match a given repo binds there.

## Process

1. **Read** the source file(s) the user points at (default to `~/.claude/CLAUDE.md`
   if present). Ask which file if there are several or none of the candidates fit.

2. **Decompose into fragments.** Split the prose along its natural topic
   boundaries — one fragment per coherent rule-group. Don't make one giant
   fragment, and don't over-split into trivia. For each: a kebab `id`, a short
   `description`, and the rules themselves (condensed, faithful — don't
   editorialize) as `guidance`. Mark guardrails / "never do X" rules
   `risk = "caution"`. Turn environment/host/toolchain-detection prose into
   `command` script fragments (bash) — or a built-in `provider` fragment
   where one fits (host, toolchain, ai-tools, tailnet, docker).

3. **Propose profiles — and ASK about granularity.** Do NOT assume one profile
   per language. Most people want a single general profile (`targets = ["machine"]`
   or a broad target set) composing the universal rules, plus per-stack profiles
   (`["rust"]`, `["node"]`, …) ONLY where they actually have stack-specific
   guidance. Surface the choice; let the user decide how fine-grained to go.

4. **Show the plan and confirm.** Present the proposed fragment ids +
   one-line descriptions and each profile's composition. Get explicit approval
   **before writing anything.**

5. **Write to `~/.config/rosita/config.toml`** (the global config — never a
   repo's `.rosita/`):
   - If it exists, back it up first: `cp ~/.config/rosita/config.toml ~/.config/rosita/config.toml.bak`.
   - **Merge, don't clobber** — append new `[[fragments]]`/`[[profiles]]`
     and preserve everything already there; match the existing TOML style.
   - Machine-specific literals (real hostnames, IPs) belong in
     `~/.config/rosita/local.toml` under `[fragment_params.<id>]`, not the
     shareable public config. Keep the public fragment clean.

6. **Validate** (and fix anything flagged):
   - `rosita doctor` — should report healthy; it also leak-lints for private
     literals in the public config.
   - `rosita fragments` and `rosita profiles` — confirm everything is listed.
   - `rosita explain --cwd <a representative repo>` — confirm the intended
     profile binds for that repo's stack.
   - Offer `rosita studio` for a visual review/edit.

7. **Wrap up.** Tell the user their original `CLAUDE.md`/`AGENTS.md` are
   untouched (rosita is additive). To wire an agent inside a repo:
   `rosita run claude` (or `rosita render --agent claude`) — repo setup is
   automatic, no `init` needed.

## Rules

- **Additive only.** Never edit or delete the user's `CLAUDE.md`/`AGENTS.md` or
  any source file. rosita layers on top; it doesn't absorb them.
- **Confirm before writing** the config, and back it up first.
- **Fragments and profiles are global-only.** Never write them into a repo's
  `.rosita/` — `rosita doctor` will flag that, and they'd be ignored.
- **Stay faithful** to the source. Condense; don't invent new policy.
