# Implementation plan — capabilities, providers, dynamic, public/private

This is the executable plan for the next phases, written so a fresh session can
pick it up. It builds on the committed MVP. Read [architecture](architecture.md)
and [concepts](concepts.md) first.

## Current state (committed MVP)

Implemented and tested (83 tests, clippy+fmt clean) on branch `feat/rosita-mvp`:

- Layered config **built-in ← global ← repo** (`src/config.rs`); `[defaults]`,
  `[env]`, `[codex]`, `[[profiles]]`, `[[agents]]`, `[host_classes]`.
- Context detection (`src/context/`): git, languages, stack, package manager,
  build/test/lint/run commands, system (os/arch/host/user/parent), env
  (allowlist+redacted). Trait `ContextDetector`.
- Profiles (`src/profile.rs`): rules (`stack|language|package_manager|path|
  branch|repo|host_class|os|arch` × `equals|starts_with|contains|matches`),
  **single-winner** selection by priority, inline `guidance` or
  `profiles/<name>.md.j2`.
- Render (`src/render/`): minijinja, single `overlay.md.j2`, self-healing banner.
- Agents (`src/adapters/`): descriptor-driven `apply`/`clean`/`artifacts`;
  claude/codex/gemini/opencode/copilot/generic; `[[agents]]` extensible.
- Commands: init, detect, render, run (sets `ROSITA_RUN`/`ROSITA_RENDERED_AT`,
  claude `--append-system-prompt`), explain, refresh, clean, doctor.
- Safety: allowlist+denylist, redaction, atomic writes, marker blocks, gitignore
  of derived artifacts (skipped outside a repo), dry-run (no disk writes at all),
  audit JSONL, hash idempotency, first-class non-repo support.

## Locked decisions

1. **Capabilities** = reusable guidance atoms; **profiles compose them**.
2. **Composition is additive** — all matching profiles contribute; union by id,
   priority-ordered, `requires`-resolved, per-capability `when`-filtered,
   `exclude`-applied; `exclusive` profile can replace.
3. **Native environment discovery** — the "agent-env idea" is built into rosita
   as native **providers** (`tailnet`/`docker`/`toolchain`/`ai-tools`/`host`),
   *not* a wrapper around the external `agent-env` tool. A generic `command`
   provider exists as a trust-gated escape hatch.
4. **Trust model** (direnv-style): built-in providers allowed anywhere; `command`
   providers only from trusted layers (global/global-local); repo-layer commands
   refused until `rosita allow`.
5. **Public/private** — references public, sensitive definitions private. Add
   `local.toml` layers (global-local, repo-local), gitignored. `host_classes`,
   capability `params`, and all provider output are private/local. Prefer
   detection over stored topology.
6. **Dynamic output** — sensitive → local/gitignored overlay only, redacted,
   excluded from the context hash, cached with TTL, re-probed on `rosita run`,
   graceful on missing tools.

## Phase 1 — Capabilities (static) + additive composition ✅ done

**Goal:** profiles compose reusable static capabilities; selection becomes additive.

**Status:** landed. `src/capability.rs` ships `Capability`/`Risk` +
`builtin_capabilities()`; `profile::compose` → `Composition`/`ResolvedCapability`
replaces single-winner `select`; built-in profiles reference capabilities;
config merges `[[capabilities]]` by id; render emits one `###` section per
capability (risk-annotated, agent-filtered, inline template-file override
preserved); explain lists active capabilities with provenance; audit records the
capability set. 102 tests, clippy+fmt clean.

- New `src/capability.rs`:
  ```rust
  pub struct Capability {
      pub id: String,
      pub description: Option<String>,
      pub tags: Vec<String>,
      pub risk: Risk,                 // Info | Caution | Dangerous (serde snake_case), default Info
      pub when: Vec<crate::profile::Rule>,   // self-gate; empty = always
      pub requires: Vec<String>,
      pub params: toml::Value,        // free-form table; default empty
      pub guidance: String,           // templated
      pub agents: Vec<String>,        // optional restriction; empty = all
      // dynamic (Phase 4): provider/command/cache — add later, default None
  }
  pub fn builtin_capabilities() -> Vec<Capability> { /* starter library */ }
  ```
- `src/profile.rs`: add to `ProfileConfig`: `capabilities: Vec<String>` (default
  []), `exclude: Vec<String>` (default []), `exclusive: bool` (default false).
  Keep `guidance` (back-compat: treat a profile's inline guidance as an implicit
  capability named `<profile>:inline`, appended last).
- `src/config.rs`: add `capabilities: Vec<Capability>` to `Config`; `RawConfig`
  gets `#[serde(default)] capabilities`; merge by id (mirror the profiles/agents
  merge); finalize seeds `builtin_capabilities()` then overrides by id.
- New selection in `src/profile.rs` (replace single-winner usage in
  `commands::prepare`):
  ```rust
  pub struct Composition {
      pub profiles: Vec<String>,         // matching profile names, priority order
      pub capabilities: Vec<ResolvedCapability>,  // ordered, deduped
      pub reasons: Vec<String>,          // "capability X via profile Y (rule …)"
  }
  pub fn compose(ctx, profiles, capabilities) -> Composition
  ```
  Algorithm: collect matching profiles (existing `matches`); if any `exclusive`
  matches, keep only the highest-priority exclusive; else union. Order profiles
  by priority desc then declaration. For each, append its `capabilities` (skip if
  already added or in any profile's `exclude`). Expand `requires` (topological,
  cycle-guarded). Filter each capability by its own `when` against ctx. Drop
  capabilities whose `agents` excludes the current agent (at render time, since
  agent varies). Record reasons.
- `commands/mod.rs`: replace `Prepared.selection: Selection` with
  `Prepared.composition: Composition` (keep a `profile_label` for display/audit —
  e.g. join of matching profile names, or the top one). Update render/explain/
  refresh/run/clean/audit accordingly.
- `src/render/`: render the overlay body from the ordered capabilities. Each
  capability's `guidance` is rendered with the model `{ context, profile,
  capability, params, agent }`. Concatenate under `## <description>` headings;
  annotate `risk` when not `Info`. Keep the header banner. The
  `profile_guidance` template var becomes the concatenated capability output (so
  the existing `overlay.md.j2` keeps working with minimal change).
- `commands/explain.rs`: list active capabilities + the rule/profile that pulled
  each in.
- `audit::AuditEvent`: add `capabilities: Vec<String>`.
- Ship a starter `builtin_capabilities()` library (see [concepts](concepts.md)):
  awareness, dev-workflow, infra, safety, comms, machine. Port the built-in
  profiles' inline guidance into named capabilities where it makes sense.
- **Tests:** compose() additive union/order/exclude/requires/when; back-compat
  inline guidance; render concatenation; explain provenance.

## Phase 2 — Public/private layering + `local.toml` + doctor lint ✅ done

**Goal:** keep sensitive specifics out of shareable config.

**Status:** landed. `Config::load_from` now layers built-in ← global
`config.toml` ← global `local.toml` ← repo `config.toml` ← repo `local.toml`
(`global_local_path`/`repo_local_path` helpers, each recorded in `sources`).
`capability_params` (keyed by id) deep-merge across layers via `merge_toml`;
`CapabilityRef` lets a profile pass public `params` overrides; compose resolves
effective params as default ← profile-supplied ← local. `init` scaffolds a
gitignored `local.toml` stub and gitignores it (repo + `--global`); the sample
`[host_classes]` moved out of the public `config.toml` into `local.toml`.
`doctor` adds a leak lint over public layers (IPv4 / `*.tld` globs / multi-label
hostnames). 108 tests, clippy+fmt clean.

- `src/config.rs`: extend `load_from` to also read `<global>/local.toml` (after
  global) and `<repo_base>/.rosita/local.toml` (after repo). New helpers
  `global_local_path()`, `repo_local_path()`. Merge order: built-in ← global ←
  global-local ← repo ← repo-local. Record each in `sources`.
- `src/commands/init.rs`: scaffold a gitignored `local.toml` stub (with a
  commented `[host_classes]`/`params` example); ensure `.rosita/local.toml` is in
  `.gitignore` (and `~/.config/rosita/.gitignore` ignores `local.toml` when
  `--global`). Move the sample `[host_classes]` out of the public sample.
- Capability `params` resolution: a profile that lists a capability may pass
  `params` overrides; private values come from the local layer. Define merge:
  capability default params ← profile-supplied params ← local-layer params.
- `src/commands/doctor.rs`: add a **leak lint** — scan public-layer
  capabilities/profiles/`host_classes` for hostname/IP/domain-looking literals
  (regex: IPv4, `*.tld` globs, `\w+\.\w+\.\w+` hostnames) and warn "looks private
  — move to local.toml". Only lint files in public layers.
- **Tests:** layer precedence (local overrides global/repo); params merge; lint
  flags a domain in public config and not in local.

## Phase 3 — Native environment providers ✅ done

**Goal:** rosita natively discovers host/tailnet/docker/toolchain (the agent-env
idea, built in).

**Status:** landed. `src/providers/` ships the `EnvProvider` trait +
`ProviderOutput` + `builtin_providers()` registry with five built-ins (`host`
reuses `context::system`; `toolchain`/`ai-tools` probe `--version`; `tailnet`
parses `tailscale status`; `docker` parses `docker ps`), each with a pure parser
and graceful `None` when the tool is absent. `gather()` is cache-backed
(`.rosita/cache/<id>.json`, TTL via `parse_duration`, pure `is_fresh`) and
redacts output; results live in a separate `Probes` value kept out of `Context`
(hash-safe). `detect --probes` shows them (human + `--json`), opt-in so a bare
`detect` spawns no subprocesses. `.rosita/cache/` gitignored in init + apply.
117 tests, clippy+fmt clean. (The `command` provider + embedding probe output
into rendered overlays come in Phase 4.)

- New `src/providers/` with:
  ```rust
  pub struct ProviderOutput { pub text: String, pub data: serde_json::Value }
  pub trait EnvProvider {
      fn id(&self) -> &'static str;
      fn probe(&self, ctx: &Context) -> crate::Result<Option<ProviderOutput>>; // None = unavailable
  }
  pub fn builtin_providers() -> Vec<Box<dyn EnvProvider>>;
  ```
- Built-ins (each shells out, best-effort, pure parser split out for tests):
  - `host` — reuse `context::system` (no new exec).
  - `tailnet` — `tailscale status`; parse peers (name/ip/os/online); redact? keep
    local-only. Pure `parse_tailscale(&str)`.
  - `docker` — `docker ps --format '{{.Names}}\t{{.Image}}\t{{.Status}}'`.
  - `toolchain` — probe a known list (node/pnpm/python/uv/cargo/go/rg/fd/gh/
    docker…) via `--version`; collect present ones.
  - `ai-tools` — claude/codex/gemini/cursor-agent/opencode `--version`.
- Caching: `.rosita/cache/<provider>.json` (gitignored) with a timestamp;
  `cache = "60s"` honored. Add `cache/` to gitignore in init/apply.
- Redaction: run `redact::redact_secrets` over `text`. Output is local-only.
- `commands/detect.rs`: add an optional `--probes` section (or always-on,
  best-effort) showing provider output, and include in `--json`.
- Exclude provider output from the context hash (it's volatile): the hash is
  already computed from `Context`; keep provider output *out* of `Context` (carry
  it in a separate `Probes` struct used only by render), or add it to `Context`
  but skip it in `compute_hash` like `parent_process`.
- **Tests:** pure parsers (`parse_tailscale`, docker, version lines) with fixture
  strings; cache TTL logic; graceful `None` when tool absent.

## Phase 4 — Dynamic capabilities + `command` provider + trust ✅ done

**Goal:** capabilities embed live provider/command output, safely.

**Status:** landed. `Capability` gained `provider`/`command`/`cache` (+ a
`#[serde(skip)] origin: Layer` set during config load). `src/trust.rs` is a
direnv-style store at `<global>/trust.toml` (repo path → sha256 of the
`config.toml`+`local.toml` bundle) with `allow`/`deny`/`status` and testable
`*_at` cores. `src/dynamic.rs` resolves dynamic capabilities (`DynamicMode`
Live vs ReadOnly): built-in providers and built-in/global commands always run;
repo-authored commands run only when the repo is trusted, else render a
`> [rosita] skipped untrusted command` note. `providers` gained
`probe_one`/`run_command` over a cache that honors the mode. Render exposes
`provider.output`/`provider.data`; dynamic overlays bypass hash-skip so output
and trust changes land (volatile output stays out of the context hash; the cache
TTL governs churn). `explain`/dry-run are ReadOnly (no exec, no writes). New
commands `rosita allow`/`deny`/`trust`. 123 tests (trust unit tests landed
first), clippy+fmt clean.

- `src/capability.rs`: add `provider: Option<String>`, `command: Option<String>`,
  `cache: Option<String>` (duration). In render, if a capability is dynamic:
  resolve the provider (built-in registry) or the command, run it (honoring
  cache), and expose `provider.output` (text) + `provider.data` (json) in the
  capability's template model.
- **Trust** (`src/trust.rs`): a store at `<global>/trust.toml` mapping a repo
  path → the sha256 of its `.rosita` config bundle. Rules:
  - built-in `provider` → always allowed.
  - `command` from global/global-local layer → allowed (you authored it).
  - `command` from repo/repo-local layer → allowed **only** if the repo's current
    config hash is present in the trust store; else **refuse to run it** (render
    the capability as a `> [rosita] skipped untrusted command — run \`rosita
    allow\`` note) and warn.
  - New commands: `rosita allow` (record current repo config hash), `rosita deny`
    (remove), `rosita trust status`.
- Dynamic output → local/gitignored overlay only; never the committed path;
  redacted; excluded from hash; cache-backed.
- **Tests:** trust gating (repo command refused pre-allow, runs post-allow,
  re-refused after config change); cache hit/miss; provider-backed dynamic
  capability renders embedded output; command provider from global layer runs
  without `allow`.

## Phase 5 — Introspection & polish ✅ done

- `rosita capabilities [list|show <id>]`, `rosita profiles list`, `rosita agents
  list` — print the resolved/active sets (great for debugging composition).
- Update `examples/` with a capabilities-based config and a `local.toml` example.
- Update these docs' status markers as phases land.

**Status:** landed. `src/commands/introspect.rs` adds `rosita capabilities`
(`list` default, `show <id>`), `rosita profiles`, and `rosita agents`, each with
`--json`; all run the real config-load + detection + composition so they mark
which capabilities are **active** and which profiles **match** the current
context. `examples/config.toml` is capabilities-based (with commented dynamic +
`host_classes`-in-local notes) and `examples/local.toml` ships the private-layer
stub. Docs' status markers updated across all phases. 126 tests, clippy+fmt
clean. **All five phases complete.**

## Cross-cutting conventions (keep)

- Best-effort everywhere; never panic on missing tools/files.
- Pure parsing in free functions with unit tests; integration via `tests/cli.rs`.
- `cargo test && cargo clippy --all-targets && cargo fmt --check` before "done".
- Conventional Commits; one logical commit per phase. (Pre-release, this project
  commits directly to `main`, no PR; there is no remote yet.)
- Anything rosita derives is gitignored/local; committed instruction files are
  never auto-edited; dry-run touches nothing.

## Suggested order

Phase 1 → 2 → 3 → 4 → 5. Phases 1–2 are pure-Rust/config and low-risk; Phase 3
adds exec (probes) but only built-ins; Phase 4 adds the trust surface — do it
carefully and land the trust tests first.
