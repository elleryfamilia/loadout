# Architecture

Reflects the current code (commit `feat: rosita MVP`). Forward-looking pieces
are in the [implementation plan](implementation-plan.md).

## Shape

A library crate `rosita` with trait seams; the binary `agentctx`→`rosita` (bin
name is `rosita`) is a thin shell over it (`src/main.rs`). Everything is
testable without spawning the binary; `tests/cli.rs` also drives the real
binary end-to-end.

## The render pipeline

```
cwd ──► repo_base ──► Config::load ──► detect_context ──► compose ─────────► render overlay ──► AgentDescriptor.apply ──► write + audit
        (git root      (built-in ←      (ContextDetector   (every matching     (minijinja per       (per-agent delivery:      (atomic write,
         or cwd)         global ←         pipeline)          profile → union     capability +         import / override /        marker blocks,
                         repo)                               of capabilities)    self-healing         emit-only)                 gitignore, JSONL)
                                                                                 banner)
```

1. **`repo_base`** = git root (`git rev-parse --show-toplevel`) or, outside a
   repo, the cwd. Resolved once and reused (`context::repo_base_for`).
2. **Config** is loaded and merged across layers (`config::Config::load`).
3. **Context** is detected by a pipeline of best-effort `ContextDetector`s.
4. **Composition** unions the capabilities of every matching profile
   (`profile::compose` → `Composition`): deduped, priority-ordered, `requires`
   resolved, per-capability `when` filtered, `exclude` applied.
5. **Render** produces one agent-neutral overlay (header + a `###` section per
   capability), filtering capabilities restricted to other agents.
6. **Delivery** is per-agent, driven by an `AgentDescriptor`.
7. **Audit** appends a JSONL event (skipped on `--dry-run`).

## Module map

| Module | Responsibility |
| --- | --- |
| `cli` | clap definitions; agents selected by id string, validated at runtime. |
| `commands/` | one file per subcommand (`init`/`detect`/`render`/`run`/`explain`/`refresh`/`clean`/`doctor`) + shared `prepare()`/`resolve_agents()`. |
| `config` | layered TOML model; per-layer `RawConfig` (all-optional) merged then finalized against built-in defaults (profiles, capabilities, agents merged by id/name). |
| `context/` | `Context` + the `ContextDetector` trait and detectors: `git`, `languages`, `commands`, `system`, `env`. |
| `capability` | `Capability` (reusable guidance atom) + `Risk` + the built-in library `builtin_capabilities()`. |
| `profile` | `ProfileConfig`, `Rule`/`Field`/`Op`, and additive `compose()` → `Composition` of `ResolvedCapability`s. |
| `render/` | `TemplateRenderer` trait (minijinja impl) + `header` (the self-healing banner) + the high-level `render()`. |
| `templates` | the single embedded `overlay.md.j2` + repo→global→embedded resolution. |
| `adapters/` | the descriptor-driven agent engine: `AgentDescriptor`, `builtin_agents()`, `apply()`, `clean()`, `artifacts()`. |
| `writer` | atomic writes, managed marker blocks (`upsert`/`remove`), `ensure_line` (gitignore), dry-run. |
| `redact` | URL credential stripping + token/secret scrubbing. |
| `audit` | append-only JSONL event log. |
| `hash` | deterministic `sha256:` context hash. |
| `report` | verbosity-gated stderr logging. |

## Trait seams (where to extend)

- **`context::ContextDetector`** — `detect(&DetectInput, &mut Context)`. Detectors
  are best-effort: a failure is logged at `--verbose` and never aborts a run.
  The default pipeline is `context::default_detectors()`.
- **`adapters::AgentDescriptor`** — data, not code. The single `adapters::apply()`
  engine consumes a descriptor. New agents are rows in `builtin_agents()` or
  `[[agents]]` config entries, never new modules.
- **`render::TemplateRenderer`** — abstracts the template engine (minijinja today).
- **`writer::Writer`** — `AtomicWriter` implements apply vs dry-run.

## Key invariants

- **One overlay, N deliveries.** rosita renders a single agent-neutral overlay;
  per-agent differences are *delivery*, expressed by the descriptor (target
  file, import vs embed, owned vs managed-block).
- **Auto-wire only local files.** Claude's `CLAUDE.local.md` is the only
  instruction file rosita wires automatically (it's local/gitignorable). Agents
  whose only file is committed (`AGENTS.md`, `GEMINI.md`, …) are emit-only by
  default.
- **Derived artifacts are gitignored, never committed** — `.rosita/generated/`,
  `.rosita/logs/`, `AGENTS.override.md`, and `CLAUDE.local.md` (when rosita
  created it). gitignore management is skipped entirely outside a repo.
- **Idempotent.** Every overlay embeds a `sha256:` context hash; re-rendering an
  unchanged context is a no-op despite the per-render timestamp
  (`adapters::write_hash_skipping`). The parent process is excluded from the
  hash so `run` vs direct invocation don't churn it.
- **Dry-run touches nothing** — not even the audit log.
- **Best-effort detection.** No detector failure is fatal; the tool degrades.

## Data flow types

- `context::Context` — `Serialize`d into the template model and hashed.
- `profile::Composition` — matching profiles + ordered `ResolvedCapability`s + reasons.
- `render::RenderOutput` — `content` (header+body) + `context_hash`.
- `adapters::ApplyResult` — files written, warnings, notes, hash.
- `audit::AuditEvent` — one JSONL line per render.
