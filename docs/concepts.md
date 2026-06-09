# Concepts

The mental model behind rosita. Sections marked **(implemented)** ship today;
everything described here is in the current binary.

## Context **(implemented)**

What rosita detects about where and how you're working: cwd, git
(root/branch/remotes [credential-sanitized]/worktree), repo name, languages,
stack, package manager, build/test/lint/run commands, OS/arch/host/user, the
calling process, and an allowlisted+redacted slice of the environment. Detection
is best-effort and degrades gracefully (e.g. outside a git repo).

## Fragments **(implemented)**

A **fragment** is one reusable, self-contained unit of guidance — e.g.
"Rust conventions", "you may SSH within my tailnet", "be terse, lead with the
result". You author them once into your own library (`[[fragments]]` in your
global config); a shipped, read-only **palette** of starters is there to
duplicate from (it is never auto-composed). Profiles compose them.

Two flavors:
- **Static** — fixed, templated guidance text.
- **Dynamic** — guidance computed at render time from a `provider`
  (a built-in probe) or a `command` (a shell command), whose live output is
  embedded as `{{ provider.output }}` / `{{ provider.data }}`. Cache-backed
  (per-fragment `cache` TTL), redacted, and gated only by `allow_exec` (see
  *Providers*). This is how rosita natively answers "what
  machine/network am I on?"

Fragments are parameterized (`params`), can self-gate (`when`), declare
dependencies (`requires`), can be restricted to specific `agents`, and carry a
`category`. Each renders as its own `###` section. See
[configuration](configuration.md#fragments-implemented).

## Profiles & selection **(implemented)**

A **profile** is a named bundle of fragments tied to one or more **targets** —
the coarse thing rosita detects: `rust`, `node`, `nextjs`, `go`, `python`,
`java`, `ruby`, `php`, `swift`, `dotnet`, or `machine` (the no-repo context).
Inline `guidance` is still supported for back-compat (it becomes a
`<profile>:inline` fragment, rendered after the explicit ones).

**One profile per context — not a union.** rosita gathers the profiles whose
`targets` match the detected context and selects **exactly one**:

- **0 match** → fall back to a **no-targets "default" profile** if you have one;
  otherwise no profile applies (the overlay is empty).
- **1 matches** → use it, no prompt.
- **2+ match** → you pick once, and the choice is remembered for that project
  (the **binding**, below).

A profile that declares **no `targets`** is the implicit catch-all default — it
applies wherever nothing more specific matches. Two such profiles is just another
tie you resolve once.

Composition then happens *within* the chosen profile, over its fragment list:
deduped by id, `requires`-resolved (dependencies first, cycle-protected), each
fragment's own `when` self-gate applied (fields `stack`, `language`,
`package_manager`, `path`, `branch`, `repo`, `host_class`, `os`, `arch`; ops
`equals`/`starts_with`/`contains`/`matches`), and `params` merged (fragment
default ← profile-supplied ← private `[fragment_params]`). There is **no**
priority ordering, no `exclude`/`exclusive`, and no always-on baseline profile —
all retired along with additive composition. Selection is deterministic and
inspectable (`rosita explain` shows what was detected, which profiles matched,
and which one is bound); no LLM is involved.

## The binding **(implemented)**

When more than one profile matches a project, rosita asks once which to use and
remembers the answer so it never asks again. In a repo the choice lives in the
gitignored `.rosita/local.toml` `[binding]` (per-checkout); outside a repo it
lives in a global, path-keyed store. A binding records only *which* profile to
use — there is no "opt out" choice (invoking rosita means you want a profile, so
when 2+ match the chooser always offers the matching ones). A binding also
fingerprints the profile's `targets`, so if you later retarget that profile the
stale binding is dropped and selection re-runs.

## Providers (native environment discovery) **(implemented)**

rosita owns environment discovery natively (the "agent-env idea", built in — not
an external tool). A **provider** (`providers::EnvProvider`) probes the live
environment and returns output (`text` + structured `data`):

- `host` — machine identity (OS/arch/hostname/user) — reuses detection, no exec.
- `toolchain` — installed dev CLIs + versions (`<tool> --version`).
- `ai-tools` — installed agent CLIs + versions.
- `tailnet` — tailscale peers (parsed from `tailscale status`).
- `docker` — running containers (parsed from `docker ps`).

The generic escape hatch is a fragment's `command` (run any shell command,
embed redacted stdout) rather than a provider — it runs unless `allow_exec` is
`false`.

Probing is **opt-in** via `rosita detect --probes` (a bare `detect` never spawns
subprocesses), and dynamic fragments embed provider/command output into the
(gitignored) overlay at render time. Output is **machine-specific and volatile**,
so it is redacted, kept **out of `Context`** (never affects the context hash;
dynamic overlays always rewrite, governed by the cache TTL not the hash), and
cached under `.rosita/cache/<id>.json` with a TTL (default 60s). Missing tools
degrade to "unavailable", never an error.

## Agents & delivery **(implemented)**

rosita produces **one** overlay; everything agent-specific is *delivery*,
described by an `AgentDescriptor` along four axes:

1. **Where** — the file the agent reads, and its scope.
2. **How** — *reference* (`@import` a generated file) vs *embed* (inline the
   content).
3. **Whose** — rosita-owned file vs a managed marker block in a user file.
4. **Freshness** — banner ▸ wrapper (`rosita run`) ▸ (no enforced hook).

The decisive rule: **auto-wire through local/gitignored paths only** — Claude →
`CLAUDE.local.md` (`@import`), Codex → `AGENTS.override.md` (which Codex reads
before the committed `AGENTS.md`), Gemini → a gitignored `GEMINI.local.md`
(`@import`) registered once in `~/.gemini/settings.json` `context.fileName`,
Copilot → the gitignored overlay via `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` set by
`rosita run` (no persistent local hook exists). rosita **never edits a committed,
shared instruction file** (`AGENTS.md`, `GEMINI.md`, `.github/copilot-instructions.md`);
agents with no wiring path are **emit-only** — a gitignored overlay plus a hint on
how to wire it, not content in a shared file.

Built-ins: `claude` (import), `codex` (auto `AGENTS.override.md` merge,
`--no-override` to skip), `gemini` (auto `GEMINI.local.md` @import + registers it
in `~/.gemini/settings.json`), `copilot` (`rosita run` sets
`COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → the gitignored overlay), `opencode` (registers
the overlay path in `~/.config/opencode/opencode.json` `instructions`), `generic`
(emit-only). All overridable / extendable via `[[agents]]`.

## Freshness **(implemented)**

Overlays are point-in-time snapshots, so each carries a **self-healing banner**:
host, timestamp, profile, context hash, and the commands to verify / regenerate
/ remove it (`rosita doctor` / `refresh` / `clean`). `rosita run` re-renders and
launches the agent with `ROSITA_RUN=1` + `ROSITA_RENDERED_AT` in the environment
(and, for Claude, an `--append-system-prompt` note), so an agent launched via
rosita knows the context is current; one launched directly knows to check.
`doctor` flags drift by comparing hashes. Staleness is made *evident*, not
prevented.

## Public vs private **(implemented)**

The guiding principle: **references are public; definitions of sensitive
specifics are private.**

- **Public / shareable** — fragment guidance and profile rule *references*
  (`host_class == "work"`, `{{ params.host }}`). Lives in `config.toml`. Safe to
  commit, even open-source.
- **Private** — the sensitive *definitions*: real hostnames, `host_classes`
  globs, fragment `params` values, and all dynamic provider/command output
  (which only ever lands in the gitignored overlay/cache). These live in
  `local.toml` (global and/or repo), gitignored, layered **after** `config.toml`
  so they win. `[fragment_params.<id>]` supplies a fragment's
  private params without redefining it; a profile may also pass public `params`
  overrides via `{ id = "x", params = … }`. Merge order: fragment default ←
  profile-supplied ← local.
- **`rosita doctor` lints** the public layers for machine-specific literals
  (IPv4, `*.domain.tld` globs, multi-label hostnames) and nudges you to move
  them to `local.toml`. The private `local.toml` is created on demand and is
  auto-gitignored the first time rosita renders into a repo.
- **Prefer detection over storage** — don't store network topology; let a
  provider probe it at runtime. It can't leak (it's local) and can't go stale.

This is what lets you share a fragment library across machines (and publicly)
without exposing what your machines are or what they can reach.

## Safety posture **(implemented)**

Generated files are **agent guidance, not enforced policy** — they're regular
files an agent reads. The only hard control is the env allowlist; everything
else (redaction, gitignore, trust) is hygiene. See [security](security.md).
