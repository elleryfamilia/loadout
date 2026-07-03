# Architecture

Reflects the current code.

## Shape

A library crate `loadout` with trait seams; the binary `loadout` is a thin shell
over it (`src/main.rs`). Everything is testable without spawning the binary;
`tests/cli.rs` also drives the real binary end-to-end, and `tests/studio.rs`
drives the studio handlers.

## The render pipeline

```
cwd → repo_base → Config::load → detect_context → select (one loadout by targets)
    → compose (its fragments) → render overlay → AgentDescriptor.apply → write + audit
```

1. **`repo_base`** = git root (`git rev-parse --show-toplevel`) or, outside a
   repo, the cwd. Resolved once and reused (`context::repo_base_for`).
2. **Config** is loaded and merged across layers (`config::Config::load`).
3. **Context** is detected by a pipeline of best-effort `ContextDetector`s.
4. **Selection** picks the **one** loadout whose `targets` match the context
   (`loadout::select`; 0/1/many → none / auto / prompt-and-remember-the-binding).
   **Composition** then resolves *that* loadout's fragments
   (`loadout::compose_loadout` → `Composition`): deduped, `requires`-resolved,
   each fragment's own `when` self-gate applied, `params` merged. No
   cross-loadout union.
5. **Render** produces one agent-neutral overlay (header + a `###` section per
   fragment), filtering fragments restricted to other agents. If a workflow is
   active (`Config::resolve_active_workflow`), it also contributes a `## Workflow`
   section and per-agent `/loadout:<slot>` slash-command files for the fixed spine.
6. **Delivery** is per-agent, driven by an `AgentDescriptor`.
7. **Audit** appends a JSONL event (skipped on `--dry-run`).

## Module map

| Module | Responsibility |
| --- | --- |
| `cli` | clap definitions; agents selected by id string, validated at runtime. |
| `commands/` | one file per subcommand (`detect`/`run`/`explain`/`refresh`/`clean`/`doctor`/`introspect` (`fragments`/`loadouts`/`agents`)) + shared `prepare()`/`resolve_agents()` and the render/sync plumbing in `apply`. (`studio` lives in `studio/`.) |
| `config` | layered TOML model; per-layer `RawConfig` (all-optional) merged then finalized. Built-in **agents** are defaults (merged by id); **fragments, loadouts, and workflows are global-only** and never injected from built-ins. `Config::from_layer_strs` assembles staged docs in-memory (origin-tagged) for studio. |
| `context/` | `Context` (+ `Scope` repo/machine, `selection_targets()`) + the `ContextDetector` trait and detectors: `git`, `languages`, `commands`, `system`, `env`. |
| `fragment` | `Fragment` (reusable guidance atom) + `Risk` + the read-only shipped `palette()` (starters to duplicate from, never auto-composed). |
| `loadout` | `LoadoutConfig` (with `targets`), `Rule`/`Field`/`Op` (fragment `when`), `FragmentRef`, pick-one `select()`, and `compose_loadout()` → `Composition` of `ResolvedFragment`s. |
| `binding` | the per-project remembered loadout choice: repo `local.toml` `[binding]` (via `toml_edit`) + a global path-keyed store; records only *which* loadout (no opt-out — a legacy `none = true` is parsed but ignored). |
| `workflow` | the house-process model: `Workflow`/`WorkflowStage`, the fixed six-slot `canonical_layout()` (the single source of truth shared by the command channel, the context section, and studio), handoff-artifact paths, the built-in catalog, and `resolve_workflow` (the active-workflow resolution `Config::resolve_active_workflow` lives in `config`). Global-only like fragments/loadouts. |
| `providers/` | `EnvProvider` trait + built-ins (`host`/`toolchain`/`ai-tools`/`tailnet`/`docker`), `gather()`/`probe_one()`/`run_command()`, TTL cache; output redacted and excluded from the context hash. |
| `dynamic` | resolves a dynamic fragment's `provider`/`command` output at render time (`DynamicMode` Live/ReadOnly); a `command` runs unless `allow_exec = false`. |
| `render/` | `TemplateRenderer` trait (minijinja impl) + `header` (the self-healing banner) + the high-level `render()`. |
| `templates` | the single embedded `overlay.md.j2` + repo→global→embedded resolution. |
| `adapters/` | the descriptor-driven agent engine: `AgentDescriptor`, `builtin_agents()`, `apply()`, `clean()`, `artifacts()`. |
| `studio/` | the ephemeral `tiny_http` + `maud` + htmx web UI: a `toml_edit` edit engine (`Session`/`StagedOp`/diff/apply), socket-free model computations (selection, ReadOnly preview, library view), the router + Origin/cookie guards, and `maud` views. |
| `lint` | the leak-pattern detector (machine-specific literals) shared by `doctor` and studio's leak warning. |
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

- **One overlay, N deliveries.** loadout renders a single agent-neutral overlay;
  per-agent differences are *delivery*, expressed by the descriptor (target
  file, import vs embed, owned vs managed-block).
- **Auto-wire only through local/gitignored paths.** Claude's `CLAUDE.local.md`
  (`@import`), Codex's `AGENTS.override.md` (read before the committed `AGENTS.md`),
  Gemini's `GEMINI.local.md` (`@import`, registered once in
  `~/.gemini/settings.json` `context.fileName`), Cursor's `.cursor/rules/loadout.mdc`
  (a fully loadout-owned always-on rule, gitignored — Cursor doesn't filter rules
  by gitignore), and Copilot's gitignored overlay
  (pointed at via `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` by `load run`) are wired
  automatically; loadout never edits a committed shared file. Agents with no wiring
  path (only `generic`, plus any custom agent) are emit-only. (opencode registers
  the overlay path in `~/.config/opencode/opencode.json` `instructions`.)
- **Derived artifacts are gitignored, never committed** — `.loadout/generated/`,
  `.loadout/logs/`, `AGENTS.override.md`, and `CLAUDE.local.md` (when loadout
  created it). gitignore management is skipped entirely outside a repo.
- **Idempotent.** Every overlay embeds a `sha256:` context hash; re-rendering an
  unchanged context is a no-op despite the per-render timestamp
  (`adapters::write_hash_skipping`). The parent process is excluded from the
  hash so `run` vs direct invocation don't churn it.
- **Dry-run touches nothing** — not even the audit log.
- **Best-effort detection.** No detector failure is fatal; the tool degrades.

## Data flow types

- `context::Context` — `Serialize`d into the template model and hashed.
- `loadout::Composition` — matching loadouts + ordered `ResolvedFragment`s + reasons.
- `render::RenderOutput` — `content` (header+body) + `context_hash`.
- `adapters::ApplyResult` — files written, warnings, notes, hash.
- `audit::AuditEvent` — one JSONL line per render.
