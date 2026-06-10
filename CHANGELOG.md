# Changelog

All notable changes to rosita are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions aim for
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`dist` pulls the section matching a tagged version into that release's notes, so
keep entries user-facing. When cutting a release, rename **Unreleased** to the
version and date (see [RELEASING.md](RELEASING.md)).

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
