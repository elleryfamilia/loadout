# Concepts

The mental model behind rosita. Status markers: **(implemented)** ships today;
**(planned)** is specified in the [implementation plan](implementation-plan.md).

## Context **(implemented)**

What rosita detects about where and how you're working: cwd, git
(root/branch/remotes [credential-sanitized]/worktree), repo name, languages,
stack, package manager, build/test/lint/run commands, OS/arch/host/user, the
calling process, and an allowlisted+redacted slice of the environment. Detection
is best-effort and degrades gracefully (e.g. outside a git repo).

## Capabilities **(static implemented; dynamic planned)**

A **capability** is one reusable, self-contained unit of guidance — e.g.
"Rust conventions", "you may SSH within my tailnet", "be terse, lead with the
result". Authored once, kept in a library (built-ins plus `[[capabilities]]`
config entries, merged by id), composed by profiles.

Two flavors:
- **Static (implemented)** — fixed, templated guidance text.
- **Dynamic (planned)** — guidance computed at render time by a **provider** (a
  native probe or an allowed command) whose live output is embedded. This is how
  rosita natively answers "what machine/network am I on?" (see *Providers*).

Capabilities are parameterized (`params`), can self-gate (`when`), declare
dependencies (`requires`), can be restricted to specific `agents`, and carry
`risk`/`tags` metadata. Each renders as its own `###` section, annotated when its
risk is not `Info`. See [configuration](configuration.md#capabilities-planned).

## Profiles **(implemented)**

A **profile** maps context → guidance. It has `when` rules and lists the
`capabilities` it composes (inline `guidance` is still supported for back-compat
— it becomes a `<profile>:inline` capability, rendered after the explicit ones).

- **Rules** match context fields — `stack`, `language`, `package_manager`,
  `path` (cwd relative to repo root), `branch`, `repo`, `host_class`, `os`,
  `arch` — with ops `equals` / `starts_with` / `contains` / `matches` (regex).
  All clauses in a profile are AND-ed.
- **Selection is additive**: every matching profile contributes; their
  capabilities are unioned (deduped by id, priority-ordered, `requires` resolved
  dependencies-first with cycle protection), each capability's own `when` is
  filtered, and any selected profile's `exclude` is applied across the whole set.
  An `exclusive` profile replaces rather than adds (the highest-priority
  exclusive match wins alone). The built-in `default` has empty rules, always
  matches, and contributes the `baseline` capability. The primary
  (highest-priority) matching profile is the display/audit label. This is what
  lets "in `~` I get these, on repo X these, on macOS these" *layer* instead of
  fight.

## Providers (native environment discovery) **(planned)**

rosita owns environment discovery natively (the "agent-env idea", built in — not
an external tool). A **provider** probes the live environment and returns
output a dynamic capability embeds:

- `host` — machine identity (OS/arch/hostname/user) — extends current detection.
- `tailnet` — tailscale peers / exit nodes.
- `docker` — running containers.
- `toolchain` — installed CLIs + versions.
- `ai-tools` — installed agent CLIs + versions.
- `command` — generic escape hatch: run any command, embed stdout (trust-gated).

Provider output is **machine-specific and sensitive**, so it only ever lands in
the local/gitignored overlay, is redacted, is excluded from the context hash,
and is cached with a TTL (re-probed on `rosita run`). Missing tools degrade to
empty, never an error.

## Agents & delivery **(implemented)**

rosita produces **one** overlay; everything agent-specific is *delivery*,
described by an `AgentDescriptor` along four axes:

1. **Where** — the file the agent reads, and its scope.
2. **How** — *reference* (`@import` a generated file) vs *embed* (inline the
   content).
3. **Whose** — rosita-owned file vs a managed marker block in a user file.
4. **Freshness** — banner ▸ wrapper (`rosita run`) ▸ (no enforced hook).

The decisive rule: **auto-wire only agents whose instruction file is itself
local** (Claude → `CLAUDE.local.md`). Agents whose only file is committed and
shared (`AGENTS.md`, `GEMINI.md`, `.github/copilot-instructions.md`) are
**emit-only by default** — rosita writes a gitignored overlay and prints how to
wire it, rather than injecting machine-specific content into a shared file.

Built-ins: `claude` (import), `codex` (opt-in `AGENTS.override.md` merge),
`gemini`/`opencode`/`copilot`/`generic` (emit-only). All overridable / extendable
via `[[agents]]`.

## Freshness **(implemented)**

Overlays are point-in-time snapshots, so each carries a **self-healing banner**:
host, timestamp, profile, context hash, and the commands to verify / regenerate
/ remove it (`rosita doctor` / `refresh` / `clean`). `rosita run` re-renders and
launches the agent with `ROSITA_RUN=1` + `ROSITA_RENDERED_AT` in the environment
(and, for Claude, an `--append-system-prompt` note), so an agent launched via
rosita knows the context is current; one launched directly knows to check.
`doctor` flags drift by comparing hashes. Staleness is made *evident*, not
prevented.

## Public vs private **(partially implemented; private layer planned)**

The guiding principle: **references are public; definitions of sensitive
specifics are private.**

- **Public / shareable** — capability guidance and profile rule *references*
  (`host_class == "work"`, `params.allowed_hosts`). Safe to commit, even
  open-source.
- **Private** — the sensitive *definitions*: real hostnames, `host_classes`
  globs, capability `params` values, and all dynamic provider output. These live
  in a gitignored local layer (and/or a separate private repo), never in the
  shareable config.
- **Prefer detection over storage** — don't store network topology; let a
  provider probe it at runtime. It can't leak (it's local) and can't go stale.

This is what lets you share a capability library across machines (and publicly)
without exposing what your machines are or what they can reach.

## Safety posture **(implemented)**

Generated files are **agent guidance, not enforced policy** — they're regular
files an agent reads. The only hard control is the env allowlist; everything
else (redaction, gitignore, trust) is hygiene. See [security](security.md).
