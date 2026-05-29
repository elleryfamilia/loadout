# rosita — `rosita`

**direnv for AI coding agents.** `rosita` detects your project & runtime
context, composes reusable **capabilities** from every matching profile (with
optional live, trust-gated environment output), renders an agent-specific
instruction overlay, writes it safely, and (optionally) launches the agent.

> The repository/project is **rosita**; the CLI command it builds is **`rosita`**.

```
$ rosita render --agent claude
claude  ·  profile rust  ·  sha256:a1fb087e1a81…
  created       .rosita/generated/claude.md
  created       CLAUDE.local.md
  created       .gitignore
```

> ⚠️ Generated overlays are **agent guidance, not enforced policy.** They are
> regular files an agent reads; nothing here is a security control. The only
> real safety boundary is the environment-variable allowlist (see *Safety*).

---

## Why

When you `cd` into a repo, your tooling adapts (direnv, asdf, nvm…). Your AI
coding agent doesn't: it reads whatever `CLAUDE.md` / `AGENTS.md` happens to be
there. `rosita` closes that gap — it derives the *right* instructions for
*this* repo, *this* branch, *this* host, and wires them into the agent's
instruction file without clobbering your hand-written content.

## Install

Requires a stable Rust toolchain (1.85+). Git is used for repo detection (via
the `git` CLI — no libgit2 build dependency).

```bash
cargo install --path .      # installs the `rosita` binary
# or, during development:
cargo build --release       # ./target/release/rosita
```

## Quickstart

```bash
rosita init                 # scaffold .rosita/ (config + templates)
rosita detect               # show what was detected
rosita explain              # show which profile is selected, and why
rosita render --agent claude   # render + wire up the overlay
rosita run claude            # render, then launch `claude` (passes args through)
```

## Commands

| Command | What it does |
| --- | --- |
| `rosita init [--global] [--force]` | Scaffold `.rosita/` (config + templates); `--global` also seeds `~/.config/rosita`. |
| `rosita detect [--json] [--probes]` | Detect and print the current context; `--probes` also runs environment providers (host/toolchain/ai-tools/tailnet/docker). |
| `rosita render [--agent <id>\|all] [--override] [--force]` | Render the overlay(s) and wire them up. |
| `rosita run <id> [args…] [--skip-render] [--override]` | Render for a launchable agent, then exec it (args passed through). |
| `rosita explain [--agent <id>\|all] [--json]` | Explain selection, matched rules, and the write plan. |
| `rosita refresh [--agent <id>\|all] [--force]` | Re-render already-initialized overlays (no-op if context unchanged). |
| `rosita clean [--agent <id>\|all]` | Remove rosita-generated overlays + managed blocks (never touches committed files). |
| `rosita doctor` | Diagnose environment, config, agents, templates, overlay freshness, and public-config leaks. |
| `rosita capabilities [list\|show <id>] [--json]` | List the capability library (active ones marked), or show one in detail. |
| `rosita profiles [--json]` | List profiles and which match the current context. |
| `rosita agents [--json]` | List configured agents and how each delivers the overlay. |
| `rosita allow` / `deny` / `trust` | Trust / untrust / show trust for this repo's `command`-backed capabilities. |

`<id>` is an agent id — built-ins are `claude`, `codex`, `gemini`, `opencode`,
`copilot`, `generic` (plus any you add via `[[agents]]`). `--agent` defaults to
the config default agent.

**Global flags:** `--cwd <DIR>` (operate as if run from there), `--verbose`,
`--dry-run` (write nothing; show what would change).

## What gets detected

`rosita detect` exposes: cwd, git root/branch/remotes (credential-sanitized)/
worktree flag, repo name, languages (by extension), stack (Rust, Next.js, Node,
Go, Python…), package manager (cargo, pnpm, yarn, npm, bun, uv, poetry, pip…),
discovered build/test/lint/run commands, OS/arch/hostname/user, the parent
process (caller), and an **allowlisted, redacted** subset of environment
variables.

## Configuration (layered)

Built-in defaults ← global `config.toml` ← global `local.toml` ← repo
`config.toml` ← repo `local.toml` (later wins).

- Global: `~/.config/rosita/config.toml` (honors `$XDG_CONFIG_HOME`, and
  `$ROSITA_CONFIG_DIR` for tests/isolation).
- Repo: `.rosita/config.toml`.
- **`local.toml`** (global and/or repo) is the **private**, gitignored layer —
  real hostnames, `[host_classes]` globs, and `[capability_params.<id>]` values.
  `config.toml` is public/shareable; `rosita doctor` flags machine-specific
  literals that belong in `local.toml`.
- Templates: `.rosita/templates/` then `~/.config/rosita/templates/` then
  the embedded defaults.
- Generated overlays: `.rosita/generated/`, probe cache: `.rosita/cache/`,
  audit log: `.rosita/logs/events.jsonl` (all gitignored).

See [`examples/config.toml`](examples/config.toml) and
[`examples/local.toml`](examples/local.toml) for annotated configs.

### Profiles & capabilities

A **capability** is a reusable unit of guidance (`rust-conventions`,
`infra-caution`, "be terse", …). **Profiles compose capabilities**: a profile
matches when **all** its `when` clauses match (AND), and selection is
**additive** — *every* matching profile contributes, its capabilities unioned
(deduped by id, priority-ordered, `requires`-resolved, per-capability `when`
filtered, `exclude` applied). An `exclusive` profile replaces rather than adds.
Inline `guidance` still works (back-compat). Inspect with `rosita capabilities`
/ `rosita profiles`.

```toml
[[capabilities]]
id = "house-style"
guidance = "Run the formatter and the linter before every commit."

[[profiles]]
name = "infra"
priority = 50
when = [{ field = "path", op = "starts_with", value = "infra/" }]
capabilities = ["infra-caution", "house-style"]
```

- **Rule fields:** `stack`, `language`, `package_manager`, `path` (cwd relative
  to repo root), `branch`, `repo`, `host_class`, `os`, `arch`.
- **Ops:** `equals`, `starts_with`, `contains`, `matches` (regex).
- **`host_class`** is derived from `[host_classes]` hostname globs (define them
  in `local.toml`), then matched via `host_class equals "work"`.

Built-in profiles (`rust`, `nextjs`, `node`, `go`, `python`, `infra`,
`experimental`, `default`) and a built-in capability library are always present
and overridable by name/id.

### Dynamic capabilities & providers

A capability may embed **live** environment output via a built-in `provider`
(`host`/`toolchain`/`ai-tools`/`tailnet`/`docker`) or a shell `command`
(`{{ provider.output }}` / `{{ provider.data }}` in scope, cache-backed). Output
is redacted, kept out of the context hash, and lands only in the gitignored
overlay. A repo-authored `command` is **refused until you `rosita allow`** the
repo (direnv-style trust, re-locked when the config changes); built-in providers
and global-authored commands are always trusted.

### Templates

Markdown templates rendered with [minijinja](https://github.com/mitsuhiko/minijinja).
The model exposes `context`, `profile`, `profile_guidance`, and `agent`. Profile
guidance resolves with precedence: a `profiles/<name>.md.j2` template file (repo
then global) wins; otherwise the inline `guidance = "…"` string (your config's,
or the built-in default) is used — both are themselves templated. See
[`examples/profiles/rust.md.j2`](examples/profiles/rust.md.j2) for the file form.

Every generated file starts with a header carrying the generation timestamp,
selected profile, **context hash**, source config files, and a "do not edit"
warning.

## Agents — one overlay, N deliveries

rosita produces **one** overlay (the rendered context for the active profile).
Everything agent-specific is *delivery*, described declaratively — not coded.
Each agent is a descriptor along four axes: **where** it reads, **how** content
gets there (reference vs embed), **whose** file it is (rosita-owned vs a managed
block in a user file), and **freshness**. Built-ins:

| Agent | rosita writes | Default wiring |
| --- | --- | --- |
| `claude` | `.rosita/generated/claude.md` | **auto-wires** a managed `@import` block into `CLAUDE.local.md` (a *local* file) |
| `codex` | `.rosita/generated/agents.md` | emit-only; `--override` merges into `AGENTS.override.md` (gitignored); never touches `AGENTS.md` |
| `gemini` | `.rosita/generated/gemini.md` | emit-only (Gemini reads `AGENTS.md`/`GEMINI.md`) |
| `opencode` | `.rosita/generated/opencode.md` | emit-only (add to `opencode.json` `instructions`) |
| `copilot` | `.rosita/generated/copilot.md` | emit-only (`.github/copilot-instructions.md` / `AGENTS.md`) |
| `generic` | `.rosita/generated/generic.md` | emit-only; you wire it in |

**The key rule:** rosita auto-wires only agents whose instruction file is itself
*local* (Claude's `CLAUDE.local.md`). Agents whose only file is **committed and
shared** (`AGENTS.md`, `GEMINI.md`, `copilot-instructions.md`) are **emit-only by
default** — rosita writes a gitignored overlay and prints how to wire it, rather
than injecting machine-specific content into a file your teammates share.

Add or override agents in config without code changes:

```toml
[[agents]]
id = "gemini"
generated_filename = "gemini.md"
launch = "gemini"
template = "overlay"        # body template name (repo/global override → embedded)
# importer = "GEMINI.local.md"          # auto-wire via @import (only for LOCAL files)
# override_target = "AGENTS.override.md" # opt-in merge target
wire_hint = "…how to include it…"
```

## Staleness & freshness

Overlays are point-in-time snapshots, so every one carries a **self-healing
banner**: host, timestamp, profile, context hash, and the exact commands to
verify/regenerate/remove it (`rosita doctor` / `refresh` / `clean`). `rosita run`
re-renders first **and** launches the agent with `ROSITA_RUN=1` +
`ROSITA_RENDERED_AT` in the environment (and, for Claude, an `--append-system-prompt`
note), so an agent launched via rosita knows the context is current — and one
launched directly knows to check. `doctor` flags drift by comparing hashes.

## Safety

- **Env vars are allowlist-only.** Only names you list are surfaced; names
  matching the denylist (`secret|token|key|password|…`) are dropped even if
  allowlisted; values are run through redaction as a backstop.
- **Redaction** strips embedded URL credentials and common token formats
  (GitHub/AWS/Slack/Google/OpenAI/JWT/PEM keys).
- **Atomic writes** — temp file in the same dir → `fsync` → rename.
- **Marker blocks** are surgically updated; surrounding content is preserved.
- **Derived artifacts are gitignored, never committed** — `.rosita/generated/`,
  `.rosita/logs/`, `AGENTS.override.md`, and (when rosita creates it)
  `CLAUDE.local.md`. Hand-authored `AGENTS.md`/`GEMINI.md`/`copilot-instructions.md`
  stay committed and untouched. (gitignore management is skipped outside a repo.)
- **Idempotent** — overlays embed a context hash; re-rendering an unchanged
  context is a no-op (`--force` overrides).
- **`--dry-run`** previews every change without touching disk (not even the log).

This is hygiene, not a security boundary. Treat generated files as guidance.

## Audit

Every render appends a JSON line to `.rosita/logs/events.jsonl`: selected
agent & profile, detected stacks, files written, the rule-match reasons, the
context hash, and whether it was a dry-run.

## Documentation

Full docs live in [`docs/`](docs/):

- [Concepts](docs/concepts.md) · [Configuration](docs/configuration.md) ·
  [Security & trust](docs/security.md) — for consumers.
- [Architecture](docs/architecture.md) · [Extending](docs/extending.md) — for devs.
- [Implementation plan](docs/implementation-plan.md) — the roadmap for
  capabilities, native environment providers, dynamic capabilities, and the
  public/private layer.

## Architecture

A small library (`rosita`) with trait seams; the binary (`rosita`) is a thin
shell over it (see [docs/architecture.md](docs/architecture.md)).

- `ContextDetector` — `git`, `languages`, `commands`, `system`, `env` detectors.
- profile selection — rule matching engine.
- `TemplateRenderer` — minijinja-backed; pluggable.
- agent delivery — one descriptor-driven engine (`claude`/`codex`/`gemini`/
  `opencode`/`copilot`/`generic`), extensible via `[[agents]]`.
- `Writer` — atomic FS writer with dry-run; pure marker-block helpers.
- redaction, audit, hashing as focused modules.

## Testing

```bash
cargo test       # unit + end-to-end CLI tests
cargo clippy --all-targets
cargo fmt --check
```

Unit tests cover detection, rule matching, rendering, atomic writes, the Claude
marker block, Codex override generation, redaction, and explain output.
`tests/cli.rs` drives the real binary against temp repos.

## Non-goals / future work

- **No FUSE in the MVP.** This uses the simple preflight/wrapper approach
  (render then launch). A FUSE-backed virtual file (live, per-process overlays)
  is a possible future extension *if* live virtual files become necessary — it
  is intentionally out of scope here.
- Generated files are guidance; `rosita` does not enforce policy.

## License

MIT OR Apache-2.0.
