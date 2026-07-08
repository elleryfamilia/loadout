# Concepts

The mental model behind loadout. Sections marked **(implemented)** ship today;
everything described here is in the current binary.

## Context **(implemented)**

What loadout detects about where and how you're working: cwd, git
(root/branch/remotes [credential-sanitized]/worktree), repo name, languages,
stack, package manager, build/test/lint/run commands, OS/arch/host/user, the
calling process, and an allowlisted+redacted slice of the environment. Detection
is best-effort and degrades gracefully (e.g. outside a git repo).

## Fragments **(implemented)**

A **fragment** is one reusable, self-contained unit of guidance — e.g.
"Rust conventions", "you may SSH within my tailnet", "be terse, lead with the
result". You author them once into your own library (`[[fragments]]` in your
global config); a shipped, read-only **palette** of starters is there to
duplicate from (it is never auto-composed). Loadouts compose them.

Two flavors:
- **Static** — fixed, templated guidance text.
- **Dynamic** — guidance computed at render time from a `provider`
  (a built-in probe) or a `command` (a shell command), whose live output is
  embedded as `{{ provider.output }}` / `{{ provider.data }}`. Cache-backed
  (per-fragment `cache` TTL), redacted, and gated only by `allow_exec` (see
  *Providers*). This is how loadout natively answers "what
  machine/network am I on?"

Fragments are parameterized (`params`), can self-gate (`when`), declare
dependencies (`requires`), can be restricted to specific `agents`, and carry a
`category`. Each renders as its own `###` section. See
[configuration](configuration.md#fragments-implemented).

## Loadouts & selection **(implemented)**

A **loadout** is a named bundle of fragments tied to one or more **targets** —
the coarse thing loadout detects: `rust`, `node`, `nextjs`, `go`, `python`,
`java`, `ruby`, `php`, `swift`, `dotnet`, or `machine` (the no-repo context).
Inline `guidance` is still supported for back-compat (it becomes a
`<loadout>:inline` fragment, rendered after the explicit ones).

**One loadout per context — not a union.** loadout gathers the loadouts whose
`targets` match the detected context and selects **exactly one**:

- **0 match** → fall back to a **no-targets "default" loadout** if you have one;
  otherwise no loadout applies (the overlay is empty).
- **1 matches** → use it, no prompt.
- **2+ match** → you pick once, and the choice is remembered for that project
  (the **binding**, below).

A loadout that declares **no `targets`** is the implicit catch-all default — it
applies wherever nothing more specific matches. Two such loadouts is just another
tie you resolve once.

Composition then happens *within* the chosen loadout, over its fragment list:
deduped by id, `requires`-resolved (dependencies first, cycle-protected), each
fragment's own `when` self-gate applied (fields `stack`, `language`,
`package_manager`, `path`, `branch`, `repo`, `host_class`, `os`, `arch`; ops
`equals`/`starts_with`/`contains`/`matches`), and `params` merged (fragment
default ← loadout-supplied ← private `[fragment_params]`). There is **no**
priority ordering, no `exclude`/`exclusive`, and no always-on baseline loadout —
all retired along with additive composition. Selection is deterministic and
inspectable (`load explain` shows what was detected, which loadouts matched,
and which one is bound); no LLM is involved.

## The binding **(implemented)**

When more than one loadout matches a project, loadout asks once which to use and
remembers the answer so it never asks again. In a repo the choice lives in the
gitignored `.loadout/local.toml` `[binding]` (per-checkout); outside a repo it
lives in a global, path-keyed store. A binding records only *which* loadout to
use — there is no "opt out" choice (invoking loadout means you want a loadout, so
when 2+ match the chooser always offers the matching ones). A binding also
fingerprints the loadout's `targets`, so if you later retarget that loadout the
stale binding is dropped and selection re-runs.

## Workflows **(implemented)**

A **workflow** is loadout's house *process* — the named way you like to work
(explore-plan-code-commit, spec-first, a compounding loop), carried across
every agent the same way a loadout carries your context. Where a loadout answers
*"what context applies here?"*, a workflow answers *"what's the process for doing
the work?"*

**One fixed command spine.** loadout always exposes the same six slash commands
to your agent — `/loadout:explore`, `/loadout:brainstorm`, `/loadout:plan`,
`/loadout:implement`, `/loadout:verify`, `/loadout:ship`. Picking a workflow does
**not** add or rename commands; it changes what each step *means*. "Plan
spec-first" and "plan compound-style" are the same `/loadout:plan` command
carrying different instructions.

- A workflow is an ordered list of **stages**, each a free-form `name` plus a
  short `purpose`. Each stage maps onto one of the six canonical slots by its
  name (`research`→explore, `specify`→brainstorm, `review`→verify,
  `commit`/`pr`→ship, …); the **first** stage to claim a slot wins. A workflow
  may fill all six or skip some. (Review and shipping are separate slots:
  `verify` is "check the result", `ship` is "commit, push, open the PR".)
- A stage may also carry deeper, on-demand `instructions` — the full prescriptive
  guidance baked into that step's `/loadout:<slot>` command body, loaded only when
  the step actually runs. The always-on `## Workflow` context section stays terse
  (it uses just `purpose`), so depth in `instructions` costs nothing until you
  invoke the step.
- A stage whose name matches no slot becomes an **extra** — rendered after the
  six (e.g. compound engineering's "capture what you learned" step).

**Handoff artifacts** are the load-bearing part. A stage can `write` a file (e.g.
`plan.md`) and a later stage can `read` it; the file lives under
`.loadout/workflow/artifacts/` and `load run` exposes its path as
`LOADOUT_<NAME>_PATH`. That handoff — plan writes the plan, implement reads it —
is what makes a workflow more than headings.

**Selection** mirrors loadout selection but is simpler — there's one active
workflow, resolved in this order:

1. `--workflow <id>` on a single run (an override; a bad id applies nothing).
2. The workflow the selected loadout binds — `workflow = "<id>"` on its
   `[[loadouts]]` block (equipped in studio's **Workflow slot**). To get a
   workflow *everywhere*, bind it on the default (no-targets) loadout. There is
   no separate global active workflow. (The shipped **starter packs** bind a
   house workflow for you, so a fresh loadout already has one.)

**The catalog.** Three built-ins ship, each modeled on a real,
permissively-licensed framework whose actual skill/command files are vendored
verbatim (so binding one gives you that framework's real guidance, not a
paraphrase), and stamped with provenance: `superpowers` (the obra/superpowers
framework), `spec-driven` (Spec Kit / Kiro), and `compound` (Every's compounding
loop). Bind one directly, or copy it into your own `[[workflows]]` and hand-edit —
a user workflow of the same id shadows the built-in.

**Global-only**, exactly like fragments and loadouts: a repo's `.loadout/` may
*declare* `[[workflows]]` but the loader strips them (and `load doctor` flags
it), so a cloned repo can never inject a workflow. loadout renders the spine and
owns the artifact-path convention, but it never enforces a step, judges
completion, or tracks a live "current stage" — this is guidance, not policy, with
no runtime and no LLM.

**Building your own.** The `load studio` **Library → Workflows** tab is a gallery
of the built-ins plus your own; you can build a workflow from scratch or customize
a built-in (which duplicates it to a new id), editing each step as plain markdown
(its summary `purpose` plus the optional deeper `instructions`).
The [`loadout-import-workflow`](../skills/loadout-import-workflow/SKILL.md) skill
imports another repo's command/skill suite into a workflow. Schema in
[configuration](configuration.md#workflows-implemented).

## Plan previews **(implemented)**

`load plan` turns an agent-authored development plan into a reviewable,
self-contained HTML page — a separate loop from the fragment/loadout/workflow
model above, but built the same way: the agent writes a structured document,
loadout deterministically renders it, nothing is invented by the renderer.

**The model.** An agent (equipped with the embedded
[`loadout-plan-preview`](../skills/loadout-plan-preview/SKILL.md) skill)
writes a JSON document under the `loadout.plan/1` format: a `meta` block
(id, title, goal), a list of `phases`, each holding `tasks` (with `status`,
`risk`, `estimate`, file touches, acceptance criteria, and `depends_on`
edges to other tasks), plus top-level `risks` and `open_questions`. The
schema lives in `src/plan/model.rs`; the full field reference is
[`skills/loadout-plan-preview/reference.md`](../skills/loadout-plan-preview/reference.md).
`load plan schema` prints it on demand.

**Two well-known paths.** The agent writes its plan to
`.loadout/workflow/artifacts/plan.json` — the input. `load plan render`
validates and renders it to `.loadout/generated/plan.html` — the output, a
single file with inlined CSS/JS, no CDN, no external fetches, and a
deterministic dependency graph. Same `plan.json` and the same loadout version
always produce byte-identical HTML. (`plan.json` is deliberately not a
declared *workflow* artifact — its filename stem would collide with a
workflow's own `plan.md` handoff file.)

**The loop.** `load plan check --json` validates before rendering
(machine-readable paths on every error, so an agent can fix precisely —
JSON-pointer for structural/validation issues, dot-notation for type
mismatches from the typed deserializer); `load plan render` opens the page in
your browser. You comment inline on any task, phase, risk, or open
question; a "Copy feedback" button assembles every comment into one
`loadout.plan-feedback/1` document (fenced JSON, with a readable markdown
mirror underneath) that you paste back to the agent, or that the agent reads
directly from `.loadout/workflow/artifacts/plan-feedback.json` if you saved it
there. Ids are stable across revisions — the agent reuses an element's id when
revising it and mints a new one only for something genuinely new — which is
what lets a comment's `ref` and a dependency edge keep pointing at the right
thing after a re-render.

**The feedback contract.** Each comment carries a `ref` (`"task:t-foo"`,
`"phase:p-core"`, `"risk:r-locking"`, `"question:q-ttl"`, or `"meta:<plan
id>"`), the free-form `text`, a `quote` of the commented-on element for
context, and a `blocking` boolean — there is no comment-type taxonomy, just a
single "Blocks approval" checkbox on the comment box. The feedback document
also carries the `plan_hash` of the plan it was written against and an
overall `verdict` (`request_changes` if any comment is `blocking`, `comment`
otherwise); `load plan check` warns loudly if `plan-feedback.json` targets a
plan that no longer matches (stale feedback).

`load plan` (no subcommand) prints status — whether a plan exists, whether the
render is fresh, whether feedback is pending — and `load plan clean` (also
swept by a plain `load clean`) removes the rendered `plan.html` and any
`plan-feedback.json`, never the plan itself. `plan` is a named subcommand, so
it takes precedence over the `load <agent>` shorthand — an agent binary
literally named `plan` can't be launched as `load plan` and needs `load run
plan` instead.

## Providers (native environment discovery) **(implemented)**

loadout owns environment discovery natively (the "agent-env idea", built in — not
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

Probing is **opt-in** via `load detect --probes` (a bare `detect` never spawns
subprocesses), and dynamic fragments embed provider/command output into the
(gitignored) overlay at render time. Output is **machine-specific and volatile**,
so it is redacted, kept **out of `Context`** (never affects the context hash;
dynamic overlays always rewrite, governed by the cache TTL not the hash), and
cached under `.loadout/cache/<id>.json` with a TTL (default 60s). Missing tools
degrade to "unavailable", never an error.

## Agents & delivery **(implemented)**

loadout produces **one** overlay; everything agent-specific is *delivery*,
described by an `AgentDescriptor` along four axes:

1. **Where** — the file the agent reads, and its scope.
2. **How** — *reference* (`@import` a generated file) vs *embed* (inline the
   content).
3. **Whose** — loadout-owned file vs a managed marker block in a user file.
4. **Freshness** — banner ▸ wrapper (`load run`) ▸ (no enforced hook).

The decisive rule: **auto-wire through local/gitignored paths only** — Claude →
`CLAUDE.local.md` (`@import`), Codex → `AGENTS.override.md` (which Codex reads
before the committed `AGENTS.md`), Gemini → a gitignored `GEMINI.local.md`
(`@import`) registered once in `~/.gemini/settings.json` `context.fileName`,
Copilot → the gitignored overlay via `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` set by
`load run` (no persistent local hook exists). loadout **never edits a committed,
shared instruction file** (`AGENTS.md`, `GEMINI.md`, `.github/copilot-instructions.md`);
agents with no wiring path are **emit-only** — a gitignored overlay plus a hint on
how to wire it, not content in a shared file.

Built-ins: `claude` (import), `codex` (auto `AGENTS.override.md` merge,
`--no-override` to skip), `gemini` (auto `GEMINI.local.md` @import + registers it
in `~/.gemini/settings.json`), `copilot` (`load run` sets
`COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → the gitignored overlay), `cursor` (owned
`.cursor/rules/loadout.mdc` rule — one file read by the IDE agent *and* the
`cursor-agent` CLI — plus a user-level `sessionStart` hook in `~/.cursor/hooks.json`
that keeps repos fresh — and auto-adopts a git repo some loadout applies to on
first open, no prior `load refresh` needed — since IDE sessions never pass
through `load run`; workflow commands land as Cursor Skills,
`.cursor/skills/loadout/loadout-<slot>/SKILL.md`, invoked `/loadout-<slot>`
rather than `/loadout:<slot>`),
`opencode` (registers
the overlay path in `~/.config/opencode/opencode.json` `instructions`), `generic`
(emit-only). All overridable / extendable via `[[agents]]`.

## Freshness **(implemented)**

Overlays are point-in-time snapshots, so each carries a **self-healing banner**:
host, timestamp, loadout, context hash, and the commands to verify / regenerate
/ remove it (`load doctor` / `refresh` / `clean`). `load run` re-renders and
launches the agent with `LOADOUT_RUN=1` + `LOADOUT_RENDERED_AT` in the environment
(and, for Claude, an `--append-system-prompt` note), so an agent launched via
loadout knows the context is current; one launched directly knows to check.
`doctor` flags drift by comparing hashes. Staleness is made *evident*, not
prevented.

## Public vs private **(implemented)**

The guiding principle: **references are public; definitions of sensitive
specifics are private.**

- **Public / shareable** — fragment guidance and loadout rule *references*
  (`host_class == "work"`, `{{ params.host }}`). Lives in `config.toml`. Safe to
  commit, even open-source.
- **Private** — the sensitive *definitions*: real hostnames, `host_classes`
  globs, fragment `params` values, and all dynamic provider/command output
  (which only ever lands in the gitignored overlay/cache). These live in
  `local.toml` (global and/or repo), gitignored, layered **after** `config.toml`
  so they win. `[fragment_params.<id>]` supplies a fragment's
  private params without redefining it; a loadout may also pass public `params`
  overrides via `{ id = "x", params = … }`. Merge order: fragment default ←
  loadout-supplied ← local.
- **`load doctor` lints** the public layers for machine-specific literals
  (IPv4, `*.domain.tld` globs, multi-label hostnames) and nudges you to move
  them to `local.toml`. The private `local.toml` is created on demand and is
  auto-gitignored the first time loadout renders into a repo.
- **Prefer detection over storage** — don't store network topology; let a
  provider probe it at runtime. It can't leak (it's local) and can't go stale.

This is what lets you share a fragment library across machines (and publicly)
without exposing what your machines are or what they can reach.

## Safety posture **(implemented)**

Generated files are **agent guidance, not enforced policy** — they're regular
files an agent reads. The only hard control is the env allowlist; everything
else (redaction, gitignore, trust) is hygiene. See [security](security.md).
