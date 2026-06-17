# Changelog

All notable changes to rosita are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions aim for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`dist` pulls the section matching a tagged version into that release's notes, so
keep entries user-facing. When cutting a release, rename **Unreleased** to the
version and date (see [RELEASING.md](RELEASING.md)).

## 0.7.0 — 2026-06-16

### Added

- **Brand-logo icons for targets.** Every built-in target now shows its real
  brand logo — Rust, Node, Bun, Next.js, Go, Python, Java (OpenJDK), Ruby, PHP,
  Swift, .NET — on profile cards, the Targets tab, and the profile editor.
  Custom targets pick an icon in their editor: a glyph from a curated set, or a
  short lettermark badge derived from the name.
- **Editable profile names.** The Studio profile editor now lets you rename a
  profile; the rename replaces it in place and refuses a name already in use.

### Changed

- **Profile cards show target icons (icon-only) at the top-right**, replacing the
  labeled chips.
- **Scripts read distinctly in the Fragments tab**: script and live-provider
  fragments get a warm-tinted glyph tile, set apart from static markdown.
- **The "Show me around" tour** now opens as a dimmed full-screen overlay so it
  reads as its own screen rather than the content of the highlighted tab.

### Fixed

- **The profile editor's target list is now derived from the catalog**, so it
  includes every built-in (Bun was missing from the old hardcoded list) and your
  custom targets — and can't drift out of sync again.
- **The `machine` target icon no longer collides** with the theme toggle's auto
  glyph (it's now a CPU chip).

## 0.6.2 — 2026-06-16

### Internal

- **Dead-code cleanup (~250 lines, no user-facing change).** Removed unused
  public helpers, write-only struct fields, an unconstructed staged-edit
  variant, and two unreachable Studio HTTP routes (`/fs-status`,
  `/profiles/<name>/preview`). Also dropped the Studio "context simulator": its
  only mutator was never wired to a route, so it always rendered the real
  detected context unchanged — an inert passthrough for a UI control that was
  never built. Behavior is identical; the build, clippy, and the full test
  suite are unaffected.

## 0.6.1 — 2026-06-16

### Fixed

- **`rosita sync` now reconciles a diverged config instead of giving up.** When
  two machines edit the global config (for example, a Studio apply on one box
  and a push from another), a manual `rosita sync` rebases your local edits onto
  the remote — the common case, where the two machines touched different
  fragments, merges cleanly — and only asks you to reconcile by hand on a true
  same-line conflict. Uncommitted edits are auto-stashed across the rebase, and
  the rebase is aborted on conflict so the repo is never left half-merged. The
  `run`/`refresh` auto-pull stays strictly fast-forward.
- **Stop syncing the machine-specific `update-check` timestamp.** rosita's
  once-a-day update check writes a timestamp into the config directory; it was
  tracked by the sync repo, so every machine committed a different value and the
  config repo diverged on it daily. It is now gitignored (existing synced repos
  drop it on the next `rosita sync`).

## 0.6.0 — 2026-06-16

### Added

- **Bun support.** rosita detects `bun` as a stack (alongside `node`, the way
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
  agent live machine/repo context (composition is one-profile-per-repo, so the
  machine `everyday` pack never co-applies).
- `rosita doctor` now flags script-backed fragments that exit non-zero while
  still printing output — rosita drops a probe's output on a non-zero exit, so
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

- `rosita refresh` now auto-pulls the synced global config before rendering
  (same best-effort, throttled, timeout-bounded pull `rosita run` does), so a
  refresh from inside a running agent session also picks up edits pushed from
  other machines.
- `--dry-run` no longer performs the auto-pull on `run` (or `refresh`): dry
  runs touch neither disk nor network.

### Removed

- **Breaking:** the `rosita render` subcommand. `rosita refresh` is the single
  no-launch render verb — bare `refresh` re-renders already-initialized
  overlays, and `refresh --agent <id>` renders (and first-adopts) that agent
  exactly as `render --agent <id>` did. Replace `rosita render` with
  `rosita refresh` in scripts.

## 0.4.0 — 2026-06-10

### Added

- The `rosita-migrate` agent skill is now embedded in the binary and managed by
  the new `rosita skill [install|remove|status]` command — no repo checkout or
  manual symlink needed. It installs to the cross-agent `~/.agents/skills/`
  location (read natively by Gemini CLI and opencode) with symlinks into
  `~/.claude/skills/` and `~/.codex/skills/` when those agents are present, and
  the skill itself was rewritten to the portable Agent Skills format so it works
  beyond Claude Code.
- A second embedded skill, `rosita-remember`: when you state a durable,
  cross-project preference mid-session, your agent saves it as a rosita
  fragment (or updates the fragment it contradicts) instead of leaving it in
  one agent's local memory. Deliberately scoped: project- and session-specific
  notes stay in the agent's own memory.
- `rosita run` offers the skills once, as a single bundled question (interactive terminals only, and only while
  your config has no profiles yet — i.e. before you've migrated); the answer is
  remembered per machine. Accepted installs are kept healthy on later runs:
  deleted symlinks are repaired and new rosita versions refresh the skill files —
  unless you've edited them, in which case rosita leaves them alone.
- `rosita doctor` gained an "Agent skills" section reporting install state,
  staleness, local edits, and broken links; `rosita studio`'s welcome screen
  gained a card that installs the skill (with confirmation) and shows the
  one-liner to invoke it.

## 0.3.0 — 2026-06-09

### Added

- `rosita studio` now shuts itself down after a period of inactivity, so a
  forgotten browser tab no longer leaves a localhost server bound indefinitely.
  The window is configurable with `--idle-timeout` (default `30m`; `0` disables
  it and serves until Ctrl-C). Any request resets the clock.

### Fixed

- Dynamic `command` fragments (e.g. the `tailnet` peer dump) no longer go blank
  after a transient hiccup: a script that briefly produced no output — say, while
  its tool's daemon was restarting — was cached as an empty result for the whole
  cache window, hiding the fragment even once the tool recovered. Empty and
  failed runs are no longer cached, and in `rosita studio` a failed script now
  shows its error with a **Retry** button instead of a blank panel.

## 0.2.1 — 2026-06-08

### Added

- `rosita update` — self-update to the latest release in place, for installs done
  with the rosita installer (it uses cargo-dist's updater). Installs from
  `cargo install` report how to switch instead of failing. `rosita update --check`
  reports whether a newer release exists without installing it.
- `rosita run` now prints a quiet, once-a-day "a newer rosita is available" hint
  when an update exists. It's best-effort and never slows a launch — gated to an
  interactive terminal, time-bounded, and silenced by `ROSITA_NO_UPDATE_CHECK`.

### Fixed

- `rosita studio`'s profile editor now offers the correct target checkboxes: the
  phantom `android` target is gone, and the `ruby`/`php`/`swift`/`dotnet` stacks
  added in 0.2.0 are now selectable. A starter-pack card also labels its atom
  count "fragments" rather than the old "caps".

## 0.2.0 — 2026-06-08

### Added

- **Targets** in `rosita studio`: a Targets tab listing every detection target
  and the rule that powers it, plus a way to author your own. Custom targets can
  be declarative (file exists — with `*` globs — file contains, and any/all
  combinations) or a **script predicate** that rosita runs safely (in the repo,
  with a timeout, results cached). Custom targets feed profile selection exactly
  like the built-ins.
- Built-in detection for **Java** (Maven/Gradle), **Ruby**, **PHP**, **Swift**,
  and **.NET**, alongside the existing Rust/Node/Next.js/Go/Python.
- An **arrow-key profile chooser** for `rosita run`: when several profiles match,
  pick with ↑/↓ and Enter (number keys still work; Ctrl-C aborts the run). Falls
  back to a numbered prompt when the terminal isn't interactive.
- A profile that declares **no `targets` is the catch-all default** — it applies
  wherever nothing more specific matches. When nothing matches at all, `run` and
  `render` now report what was detected and how to fix it.
- Live machine grounding in the **everyday** starter pack — real host and runtime
  facts refreshed at launch, not a hand-typed snapshot.

### Changed

- `rosita run` no longer offers an opt-out. When 2+ profiles match it lists
  **only the matching profiles** — invoking rosita means you want one of them.
- Relicensed to **MIT-only** (previously MIT OR Apache-2.0).
- Removed the per-project "bound" badge from studio (it was noise).

### Fixed

- Running rosita **outside a repo** (e.g. from `$HOME`) no longer writes a managed
  importer that bleeds a stale machine-context block into every repo beneath that
  directory. Off-repo context now reaches Claude via `--append-system-prompt`.
- A legacy remembered opt-out (`[binding] none = true` from an older rosita) is
  now **ignored** rather than honored, so a project stuck on "none" re-prompts
  for a profile instead of silently rendering an empty overlay.

## 0.1.0 — 2026-06-08

First tagged release.

### Added

- `rosita studio` guided first-run: lands on the **Profiles** tab and walks a
  three-step onboarding — welcome (detect stack + pick a starter pack) → review
  what will change → "you're set" (names `rosita run <agent>`). A top-bar **?**
  button re-opens the tour anytime.
- Release pipeline via [`dist`](https://opensource.axo.dev/cargo-dist/): tagging
  `vX.Y.Z` builds prebuilt binaries for macOS (Apple Silicon + Intel) and Linux
  (x86_64 + ARM64), with a shell installer attached to the GitHub Release.
  (Windows is omitted for now — rosita is unix-only today.)
- CI workflow: rustfmt, clippy (`-D warnings`), the test suite on Linux + macOS,
  and an MSRV (1.85) check.
