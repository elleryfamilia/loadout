# Changelog

All notable changes to loadout are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions aim for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`dist` pulls the section matching a tagged version into that release's notes, so
keep entries user-facing. When cutting a release, rename **Unreleased** to the
version and date (see [RELEASING.md](RELEASING.md)).

## Unreleased

### Added

- **`load plan`** — validate, render, and review an agent-written development
  plan. An agent (with the embedded `loadout-plan-preview` skill) writes a
  structured `plan.json`; `load plan check [--json] [--lenient]` validates it
  with JSON-pointer diagnostics; `load plan render [FILE] [--out] [--no-open]`
  renders a self-contained `plan.html` — inline dependency graph, task cards,
  element-anchored commenting, a "Copy feedback" button — and opens it in your
  browser; `load plan schema` prints the schema reference; `load plan clean`
  removes the rendered page and any feedback file (a plain `load clean` sweeps
  them too). Rendering is deterministic (same `plan.json` + same loadout
  version → byte-identical HTML) and fully self-contained — no CDN, no
  external fetches. See [docs/concepts.md](docs/concepts.md#plan-previews-implemented).
- **Per-skill install/remove toggles** — Studio: per-skill install/remove toggles
  on the skills card (previously the card only offered install-all; the CLI's
  `load skill install/remove <id>` already supported single skills).

### Security

- **Studio's markdown rendering now de-links unsafe URL schemes.** Fragment
  and loadout guidance previews in `load studio` previously escaped raw HTML
  but did not check link destinations — a `[text](javascript:…)` link in
  guidance markdown rendered as a real, clickable `<a href="javascript:…">`.
  Studio now shares the same sanitizing markdown renderer introduced for
  `load plan` (`src/markdown.rs`): link destinations are limited to
  `http(s):`, `mailto:`, in-page `#fragment`s, and scheme-less relative
  paths; anything else is de-linked to plain text, checked case-insensitively
  after stripping control/whitespace characters. Images never fetch — a safe
  destination renders as a link, not `<img>`.

## 0.12.0 — 2026-07-03

### Added

- **Cursor support** — a new built-in `cursor` agent covers the Cursor IDE and
  the `cursor-agent` CLI with one wiring: a gitignored, always-on rule at
  `.cursor/rules/loadout.mdc` (Cursor doesn't filter rules by gitignore —
  verified live in both surfaces). Workflow stages ship as Cursor Skills
  (`/loadout-plan`, `/loadout-verify`, …), and `load run cursor` launches the
  CLI with a fresh overlay like any other agent.
- **Hands-free freshness and adoption in the IDE** — loadout registers a
  `sessionStart` hook in `~/.cursor/hooks.json` (automatically, from any
  `refresh`/`run`/`studio`/`sync`, and only when Cursor is installed; other
  tools' hook entries are preserved). Each IDE session quietly re-renders the
  workspace before the first prompt, and opening a git repo one of your
  loadouts applies to wires it on the spot — no `load refresh` ever needed.
  Opt-outs: `auto_adopt = false` on the hook registry, or
  `load hook cursor --remove`. `load doctor` checks the registration.
- **Agent aliases** — agents resolve by the binary name you know, not just
  their id: `load cursor-agent` and `load agent` (Cursor's alias binary) both
  reach `cursor`. Custom `[[agents]]` entries can declare their own `aliases`.
  Unknown agent errors now list the known ids.
- **New `[[agents]]` descriptor fields** for custom agents: `target_file`
  (a fully loadout-owned wired file, written raw), `preamble` (mandatory first
  bytes, e.g. MDC frontmatter), `hook_registry` (user-level lifecycle-hook
  freshness), and `aliases`.

### Changed

- **The generated header now tells agents to self-refresh** — an agent launched
  outside `load run` (an IDE session, a direct CLI launch) is instructed to run
  `load refresh` and re-read the overlay instead of merely being warned it may
  be stale. Existing overlays pick the new wording up on their next refresh
  (expect a one-time rewrite of every overlay).

### Fixed

- **Probes and script fragments can no longer hang a render** — every
  provider/command subprocess is bounded (10s), gets no stdin (a CLI that
  prompts sees EOF), and is killed at the deadline. A wedged daemon (e.g. an
  unresponsive `docker ps`) now degrades to "not available" like a missing
  tool; a timed-out script fragment surfaces as a visible failure and is never
  cached.

## 0.11.0 — 2026-06-26

### Added

- **Create-a-Loadout board** — selecting a loadout in `load studio` now opens an
  editable board of slots instead of a read-only document: **Applies to**
  (targets), **Fragments**, and a single **Workflow** slot, plus a one-line
  readout of what it renders and where. Each slot edits in place (add/remove a
  target, equip/remove a fragment from a category-grouped picker, bind/swap/clear
  the workflow). The composed-guidance view moves behind a **Preview** action
  (with a "← Board" button back).
- **Per-loadout workflow, in the UI** — a loadout's workflow binding
  (`Profile.workflow`) is now first-class: equip one in the board's Workflow
  slot. (It was always in the model but had no UI — and a write bug meant it
  never persisted; see Fixed.)
- **Paged fragments** — the board's Fragments section pages a long list 9 at a
  time in a fixed 3×3 grid, so a big loadout no longer sprawls and flipping pages
  never shifts the layout.
- **Single canonical default loadout** — the no-targets catch-all is now *the*
  default: pinned to the top of the rail with a Default badge, a locked "Applies
  to", and no rename/delete. Every other loadout needs ≥1 target. `load doctor`
  warns when there are zero or more than one. Starter packs reflect this — the
  **everyday** pack is the default (no targets), and every pack now binds the
  house workflow (`superpowers`).

### Changed

- **Studio top nav is now two destinations: Loadouts | Library.** Fragments,
  Targets, and Workflows moved into the Library (a pill sub-nav) — they're the
  shared gear a loadout binds, not peers of a loadout.

### Removed

- **The global active workflow (`[defaults].workflow`) is gone (breaking).** A
  workflow is bound per-loadout only (equip it in the Workflow slot; use the
  default loadout for "everywhere"). A leftover `[defaults].workflow` is
  tolerated and ignored. The studio's "active workflow" / "Use this workflow"
  activation is replaced by per-loadout binding.

### Fixed

- The studio wrote profiles without their `workflow` field, so a per-loadout
  workflow binding never persisted. It does now.

## 0.10.0 — 2026-06-26

### Added

- **Per-step instructions** — a workflow step can now carry the full
  prescriptive guidance, not just a one-line summary. It rides in that step's
  `/loadout:<step>` command and loads only when you run the step, so the
  always-on workflow map stays terse. The studio step editor splits into a
  one-line summary plus an instructions body, and a step that carries one shows
  a "details" marker on its card.
- **`/loadout:ship`** — a sixth canonical command for the finish-and-ship phase
  (commit, push, open the PR). The spine is now explore → brainstorm → plan →
  implement → verify → ship. `commit`/`ship`/`pr` map to `ship` instead of
  folding into `verify`, so a workflow with both a review step and a commit step
  keeps both as distinct commands.

### Changed

- **Built-in workflows now ship their frameworks' real content, verbatim.**
  Superpowers, spec-driven, and compound embed the actual upstream skill/command
  files — vendored from each project's MIT-licensed repo and pinned by release
  in `vendored/sources.toml` — instead of loadout's own summaries, so binding
  one gives you that framework's real guidance. The `loadout-import-workflow`
  skill now captures a source step's full body the same way.

### Removed

- **Dropped the `boris`, `lean`, and `loop` built-ins.** Each was only a prose
  summary of a methodology with no upstream repo to copy faithfully (Boris's
  site, an Anthropic article, the Ralph blog), so it misrepresented what it
  named. A profile still bound to one now renders no workflow (and `load doctor`
  flags the dangling binding) — rebind to `superpowers`, `spec-driven`, or
  `compound`, or import the source with `loadout-import-workflow`.

## 0.9.0 — 2026-06-25

### Added

- **Workflows** — a named development process (Anthropic's lean loop, Boris's
  daily flow, spec-driven, the Ralph loop, Every's compound engineering) that
  travels across every agent. loadout exposes one fixed five-command spine —
  `/loadout:explore`, `brainstorm`, `plan`, `implement`, `verify` — and a
  workflow changes what each step *means*, optionally handing a file (e.g.
  `plan.md`) from one step to the next. Bind one globally with
  `[defaults].workflow`, per-loadout, or per-run with `--workflow`. Ships six
  built-ins plus a studio **Workflows** tab to browse, build, customize, and
  edit your own.
- **`loadout-import-workflow` skill** — turns another repo's command/skill suite
  into a loadout workflow (`load skill install loadout-import-workflow`), mapping
  its steps onto the canonical spine.

### Changed

- **The tool-managed config tolerates unknown keys instead of failing to
  parse.** A config written by a newer loadout — a new `[defaults]` key, a new
  top-level table — no longer bricks an older binary: unknown keys in
  `[defaults]`/`[env]`/`[codex]`/`[sync]` (and at the top level) now warn and are
  ignored rather than aborting the whole load. Hand-authored
  `[[fragments]]`/`[[loadouts]]`/`[[targets]]`/`[[workflows]]` stay strict, so
  typos there are still caught.

### Fixed

- **`load doctor` now flags a repo `.loadout/config.toml` that declares
  `[[workflows]]`** — they're global-only and ignored there, matching the
  existing warning for fragments, loadouts, and targets.

### Docs

- Trimmed the README by ~60% (691 → ~260 lines): the deep reference material
  (templates, dynamic fragments, the full safety bullet list, audit, staleness)
  now lives in `docs/` and the README links out to it. Reworked the copy around
  the loadout framing — *equip the right context for the job* — and refreshed the
  studio screenshot to the new branding.

## 0.8.0 — 2026-06-22

**rosita is now Loadout, and the command is `load`.** This is a clean-break
rename — there is no backwards compatibility. Existing setups must migrate (see
below).

### Changed

- **The binary is `load`** (was `rosita`). `load <agent>` equips the loadout
  that matches the current project and launches the agent — `load claude` is the
  everyday command; `load run claude` is the explicit form.
- **"profiles" are now "loadouts".** A loadout is the named bundle of fragments
  selected per project. The config key is `[[loadouts]]` (was `[[profiles]]`).
- **New commands:** `load use <loadout>` pins a loadout for a project,
  `load list [loadouts|fragments|agents|targets]` is one inspector, and
  `load edit [name]` opens your config in `$EDITOR`.
- **Paths and env moved:** global config `~/.config/loadout` (was
  `~/.config/rosita`), per-repo `.loadout/` (was `.rosita/`), and `LOADOUT_*`
  environment variables (were `ROSITA_*`).
- **Studio rebrand:** the web UI is "Loadout studio" with a backpack mark and an
  Alfa Slab One wordmark; it writes the `[[loadouts]]` key.

### Migrating from rosita

There is no auto-migration. Move your config and rewrite the old keys:

```bash
mv ~/.config/rosita ~/.config/loadout
sed -i '' 's/\[\[profiles\]\]/[[loadouts]]/g' ~/.config/loadout/config.toml
```

Per repo, the gitignored `.rosita/` is regenerated as `.loadout/` on the next
`load refresh` / `load run`; remove the old directory. Reinstall the agent
skills (now `loadout-migrate` / `loadout-remember`) with `load skill install`.

## 0.7.2 — 2026-06-17

### Changed

- **Starter packs ship a plain "Communication style" fragment.** The everyday
  and per-stack packs now compose plain, direct communication guidance —
  explain ideas before using shorthand, report the result first, and separate
  facts, decisions, risks, and next steps — in place of the old "terse
  communication" wording. The example config reflects the same change.
- **`work-summary` is no longer baked into the starter packs.** Its reporting
  guidance is now covered by the communication fragment, so packs compose one
  coherent communication section instead of two overlapping ones. The
  `work-summary` fragment remains in the palette for anyone who still wants to
  pick it.

## 0.7.1 — 2026-06-16

### Changed

- **The machine-scope loadout is pinned to the top of the Studio loadout list**,
  so the off-repo loadout is always first regardless of config order.
- **Scripts read consistently in the loadout view.** A loadout's fragment list
  now tints script and live-provider fragments with the same amber tile — and
  amber run buttons — as the Fragments tab, so executable fragments stand out.

### Fixed

- **Documented the `bun` built-in target**, which was detected and selectable but
  missing from the README's target list.

## 0.7.0 — 2026-06-16

### Added

- **Brand-logo icons for targets.** Every built-in target now shows its real
  brand logo — Rust, Node, Bun, Next.js, Go, Python, Java (OpenJDK), Ruby, PHP,
  Swift, .NET — on loadout cards, the Targets tab, and the loadout editor.
  Custom targets pick an icon in their editor: a glyph from a curated set, or a
  short lettermark badge derived from the name.
- **Editable loadout names.** The Studio loadout editor now lets you rename a
  loadout; the rename replaces it in place and refuses a name already in use.

### Changed

- **Loadout cards show target icons (icon-only) at the top-right**, replacing the
  labeled chips.
- **Scripts read distinctly in the Fragments tab**: script and live-provider
  fragments get a warm-tinted glyph tile, set apart from static markdown.
- **The "Show me around" tour** now opens as a dimmed full-screen overlay so it
  reads as its own screen rather than the content of the highlighted tab.

### Fixed

- **The loadout editor's target list is now derived from the catalog**, so it
  includes every built-in (Bun was missing from the old hardcoded list) and your
  custom targets — and can't drift out of sync again.
- **The `machine` target icon no longer collides** with the theme toggle's auto
  glyph (it's now a CPU chip).

## 0.6.2 — 2026-06-16

### Internal

- **Dead-code cleanup (~250 lines, no user-facing change).** Removed unused
  public helpers, write-only struct fields, an unconstructed staged-edit
  variant, and two unreachable Studio HTTP routes (`/fs-status`,
  `/loadouts/<name>/preview`). Also dropped the Studio "context simulator": its
  only mutator was never wired to a route, so it always rendered the real
  detected context unchanged — an inert passthrough for a UI control that was
  never built. Behavior is identical; the build, clippy, and the full test
  suite are unaffected.

## 0.6.1 — 2026-06-16

### Fixed

- **`load sync` now reconciles a diverged config instead of giving up.** When
  two machines edit the global config (for example, a Studio apply on one box
  and a push from another), a manual `load sync` rebases your local edits onto
  the remote — the common case, where the two machines touched different
  fragments, merges cleanly — and only asks you to reconcile by hand on a true
  same-line conflict. Uncommitted edits are auto-stashed across the rebase, and
  the rebase is aborted on conflict so the repo is never left half-merged. The
  `run`/`refresh` auto-pull stays strictly fast-forward.
- **Stop syncing the machine-specific `update-check` timestamp.** loadout's
  once-a-day update check writes a timestamp into the config directory; it was
  tracked by the sync repo, so every machine committed a different value and the
  config repo diverged on it daily. It is now gitignored (existing synced repos
  drop it on the next `load sync`).

## 0.6.0 — 2026-06-16

### Added

- **Bun support.** loadout detects `bun` as a stack (alongside `node`, the way
  `nextjs` rides along), ships a built-in `bun` target (matched by
  `bun.lock`/`bun.lockb`), a `bun-conventions` fragment, and a **Bun** starter
  pack.
- **`project-scripts` fragment** — a live probe that lists the commands a repo
  actually defines (package.json scripts, Makefile/justfile targets, Cargo,
  `go.mod`) so agents use real entry points instead of inventing them.
- **`work-summary` fragment** — asks agents to close a unit of work with concise
  Done / Next-steps bullet lists.
- **Live grounding in the stack packs.** The Rust, Node.js, Next.js, Go, and
  Python starter packs now bake in the `environment` framing plus `toolchain`,
  `project-scripts`, and `containers`, so selecting a stack pack alone gives the
  agent live machine/repo context (composition is one-loadout-per-repo, so the
  machine `everyday` pack never co-applies).
- `load doctor` now flags script-backed fragments that exit non-zero while
  still printing output — loadout drops a probe's output on a non-zero exit, so
  such a fragment renders as nothing. The check points at the `exit 0` fix and
  leaves the normal "tool absent → no output" case alone.

### Changed

- The live environment probes (`toolchain`, `containers`, `ai-tools`,
  `tailnet`) now lead with a one-line explanation of what each section is and
  how to use it, instead of emitting a bare data dump.

### Fixed

- The `toolchain` probe now reports `go` via `go version` rather than the
  invalid `go --version`, which errored and embedded the error string in the
  rendered output.

## 0.5.0 — 2026-06-10

### Changed

- `load refresh` now auto-pulls the synced global config before rendering
  (same best-effort, throttled, timeout-bounded pull `load run` does), so a
  refresh from inside a running agent session also picks up edits pushed from
  other machines.
- `--dry-run` no longer performs the auto-pull on `run` (or `refresh`): dry
  runs touch neither disk nor network.

### Removed

- **Breaking:** the `loadout render` subcommand. `load refresh` is the single
  no-launch render verb — bare `refresh` re-renders already-initialized
  overlays, and `refresh --agent <id>` renders (and first-adopts) that agent
  exactly as `render --agent <id>` did. Replace `loadout render` with
  `load refresh` in scripts.

## 0.4.0 — 2026-06-10

### Added

- The `loadout-migrate` agent skill is now embedded in the binary and managed by
  the new `load skill [install|remove|status]` command — no repo checkout or
  manual symlink needed. It installs to the cross-agent `~/.agents/skills/`
  location (read natively by Gemini CLI and opencode) with symlinks into
  `~/.claude/skills/` and `~/.codex/skills/` when those agents are present, and
  the skill itself was rewritten to the portable Agent Skills format so it works
  beyond Claude Code.
- A second embedded skill, `loadout-remember`: when you state a durable,
  cross-project preference mid-session, your agent saves it as a loadout
  fragment (or updates the fragment it contradicts) instead of leaving it in
  one agent's local memory. Deliberately scoped: project- and session-specific
  notes stay in the agent's own memory.
- `load run` offers the skills once, as a single bundled question (interactive terminals only, and only while
  your config has no loadouts yet — i.e. before you've migrated); the answer is
  remembered per machine. Accepted installs are kept healthy on later runs:
  deleted symlinks are repaired and new loadout versions refresh the skill files —
  unless you've edited them, in which case loadout leaves them alone.
- `load doctor` gained an "Agent skills" section reporting install state,
  staleness, local edits, and broken links; `load studio`'s welcome screen
  gained a card that installs the skill (with confirmation) and shows the
  one-liner to invoke it.

## 0.3.0 — 2026-06-09

### Added

- `load studio` now shuts itself down after a period of inactivity, so a
  forgotten browser tab no longer leaves a localhost server bound indefinitely.
  The window is configurable with `--idle-timeout` (default `30m`; `0` disables
  it and serves until Ctrl-C). Any request resets the clock.

### Fixed

- Dynamic `command` fragments (e.g. the `tailnet` peer dump) no longer go blank
  after a transient hiccup: a script that briefly produced no output — say, while
  its tool's daemon was restarting — was cached as an empty result for the whole
  cache window, hiding the fragment even once the tool recovered. Empty and
  failed runs are no longer cached, and in `load studio` a failed script now
  shows its error with a **Retry** button instead of a blank panel.

## 0.2.1 — 2026-06-08

### Added

- `load update` — self-update to the latest release in place, for installs done
  with the loadout installer (it uses cargo-dist's updater). Installs from
  `cargo install` report how to switch instead of failing. `load update --check`
  reports whether a newer release exists without installing it.
- `load run` now prints a quiet, once-a-day "a newer loadout is available" hint
  when an update exists. It's best-effort and never slows a launch — gated to an
  interactive terminal, time-bounded, and silenced by `LOADOUT_NO_UPDATE_CHECK`.

### Fixed

- `load studio`'s loadout editor now offers the correct target checkboxes: the
  phantom `android` target is gone, and the `ruby`/`php`/`swift`/`dotnet` stacks
  added in 0.2.0 are now selectable. A starter-pack card also labels its atom
  count "fragments" rather than the old "caps".

## 0.2.0 — 2026-06-08

### Added

- **Targets** in `load studio`: a Targets tab listing every detection target
  and the rule that powers it, plus a way to author your own. Custom targets can
  be declarative (file exists — with `*` globs — file contains, and any/all
  combinations) or a **script predicate** that loadout runs safely (in the repo,
  with a timeout, results cached). Custom targets feed loadout selection exactly
  like the built-ins.
- Built-in detection for **Java** (Maven/Gradle), **Ruby**, **PHP**, **Swift**,
  and **.NET**, alongside the existing Rust/Node/Next.js/Go/Python.
- An **arrow-key loadout chooser** for `load run`: when several loadouts match,
  pick with ↑/↓ and Enter (number keys still work; Ctrl-C aborts the run). Falls
  back to a numbered prompt when the terminal isn't interactive.
- A loadout that declares **no `targets` is the catch-all default** — it applies
  wherever nothing more specific matches. When nothing matches at all, `run` and
  `render` now report what was detected and how to fix it.
- Live machine grounding in the **everyday** starter pack — real host and runtime
  facts refreshed at launch, not a hand-typed snapshot.

### Changed

- `load run` no longer offers an opt-out. When 2+ loadouts match it lists
  **only the matching loadouts** — invoking loadout means you want one of them.
- Relicensed to **MIT-only** (previously MIT OR Apache-2.0).
- Removed the per-project "bound" badge from studio (it was noise).

### Fixed

- Running loadout **outside a repo** (e.g. from `$HOME`) no longer writes a managed
  importer that bleeds a stale machine-context block into every repo beneath that
  directory. Off-repo context now reaches Claude via `--append-system-prompt`.
- A legacy remembered opt-out (`[binding] none = true` from an older loadout) is
  now **ignored** rather than honored, so a project stuck on "none" re-prompts
  for a loadout instead of silently rendering an empty overlay.

## 0.1.0 — 2026-06-08

First tagged release.

### Added

- `load studio` guided first-run: lands on the **Loadouts** tab and walks a
  three-step onboarding — welcome (detect stack + pick a starter pack) → review
  what will change → "you're set" (names `load run <agent>`). A top-bar **?**
  button re-opens the tour anytime.
- Release pipeline via [`dist`](https://opensource.axo.dev/cargo-dist/): tagging
  `vX.Y.Z` builds prebuilt binaries for macOS (Apple Silicon + Intel) and Linux
  (x86_64 + ARM64), with a shell installer attached to the GitHub Release.
  (Windows is omitted for now — loadout is unix-only today.)
- CI workflow: rustfmt, clippy (`-D warnings`), the test suite on Linux + macOS,
  and an MSRV (1.85) check.
