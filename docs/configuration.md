# Configuration

loadout is configured by layered TOML. Everything below ships in the current
binary (sections are marked **(implemented)**).

## Layers (precedence: later wins)

Built-in defaults ← global `config.toml` ← global `local.toml` ← repo
`config.toml` ← repo `local.toml`.

| Layer | Path | Shareable? |
| --- | --- | --- |
| global | `$LOADOUT_CONFIG_DIR` or `$XDG_CONFIG_HOME/loadout` or `~/.config/loadout`, file `config.toml` | yes (commit / open-source) |
| global-local | `<global>/local.toml` | **no** (gitignored / private) |
| repo | `<repo_base>/.loadout/config.toml` | yes (committable) |
| repo-local | `<repo_base>/.loadout/local.toml` | **no** (gitignored) |

**Fragments and loadouts are global-only.** You author them once, in the
global layers, and share them across machines by committing `config.toml` to a
synced repo. A repo's `.loadout/` carries only the per-project **`[binding]`** (in
the gitignored `local.toml`), the generated overlays, the audit log, the probe
cache, and optional template overrides — *not* fragments or loadouts.
Fragments or loadouts declared in a repo layer are ignored, and `loadout
doctor` flags them.

- `$LOADOUT_CONFIG_DIR` overrides the global dir (used in tests / isolation).
- Templates resolve repo → global → embedded; agents merge **by id** (later
  layers override).
- Lists like `env.allowlist` are **additive** across layers (union, deduped).
- The merge keeps "unset" distinct from "default": each layer parses into an
  all-optional `RawConfig`, layers fold, then defaults are applied.

Other directories under `<repo_base>/.loadout/`: `generated/` (overlays,
gitignored), `logs/events.jsonl` (audit, gitignored), `templates/` (overrides),
`cache/` (gitignored — provider caches).

## `[defaults]` (implemented)

```toml
[defaults]
agent = "claude"     # agent used when --agent is omitted
```

(There is no global workflow setting — a workflow is bound per-loadout in its
Workflow slot; see `[[loadouts]]` and `[[workflows]]` below.)

## `[env]` (implemented)

Allowlist-only environment exposure; a name denylist wins even if allowlisted.

```toml
[env]
allowlist = ["LANG", "TERM", "CI", "TZ", "EDITOR"]      # ONLY these names surface
deny_name_patterns = ["(?i)(secret|token|key|password|credential|auth)"]
```

## `[codex]` (implemented)

```toml
[codex]
write_override = true    # auto-write AGENTS.override.md (default; `--no-override` to skip)
max_output_kib = 32      # warn when generated output exceeds this
```

## `[[loadouts]]` (implemented)

A loadout is tied to one or more detected **targets** and composes a list of
fragments. It is the unit of selection — one loadout per context.

```toml
[[loadouts]]
name = "rust — web"
targets = ["rust"]                                  # selected when the repo detects as rust
fragments = [
  "rust-conventions",
  { id = "ssh", params = { user = "deploy" } },     # optional inline params override
]
# template = "infra"    # optional body-template override
# disabled = true       # keep the definition but never select or compose it
```

- **`targets`:** the coarse detected tags — `stack` values `rust`, `node`,
  `nextjs`, `go`, `python`, `java`, `ruby`, `php`, `swift`, `dotnet`, plus
  `machine` (the no-repo context). A loadout is a selection candidate when
  **any** of its targets matches. Empty `targets` ⇒ the loadout is the **catch-all
  default**: selected whenever no targeted loadout matches (and still bindable by
  name).
- **Selection is pick-one:** of the loadouts whose targets match, exactly one is
  used — 0 → fall back to a no-targets default if you have one, else none (empty
  overlay), 1 → auto, 2+ → you pick once and it's remembered (the
  [`[binding]`](#binding-implemented)). Loadouts do **not** merge; there is no
  `priority`, `exclude`, or `exclusive`, and no built-in loadouts.
- A saved loadout needs **≥1 fragment** (studio enforces it; the parser accepts
  zero for hand-edits).

Loadouts select on `targets`, not rules — a *fragment* may still self-gate with
`when` rules (see [`[[fragments]]`](#fragments-implemented)).

A loadout may also pin a workflow for the contexts it covers:

```toml
[[loadouts]]
name = "rust"
targets = ["rust"]
fragments = ["rust-conventions"]
workflow = "boris"     # the workflow this loadout equips (its Workflow slot); omit for none
```

## `[[workflows]]` (implemented)

A workflow is your house *process*, mapped onto loadout's fixed five-command
spine (`explore`, `brainstorm`, `plan`, `implement`, `verify`). **Global-only**
like fragments and loadouts — a repo declaring `[[workflows]]` is stripped at
load (`load doctor` flags it). Six built-ins ship; your own of the same `id`
shadows a built-in. See [concepts](concepts.md#workflows-implemented).

```toml
[[workflows]]
id = "lean"                    # kebab-case, unique; how a [[loadouts]] block binds it
name = "Lean"                  # gallery title + rendered heading (optional)
description = "Read first, plan on paper, then build."   # one-line blurb (optional)
icon = "git-branch"            # studio card glyph (optional)
# modeled_on / researched / source — display-only provenance (optional)
# disabled = true              # keep the definition but never select it

[[workflows.stages]]
name = "explore"               # maps onto a canonical slot by name; unmatched ⇒ an "extra"
purpose = "Read the code paths and tests before changing anything."

[[workflows.stages]]
name = "plan"
purpose = "Write a short plan: objective, approach, risks, validation."
writes = "plan.md"             # handoff artifact this stage produces

[[workflows.stages]]
name = "implement"
purpose = "Build the change following the plan."
reads = "plan.md"              # …consumed by a later stage

[[workflows.stages]]
name = "commit"                # `commit` maps onto the `verify` slot
purpose = "Run build, tests, and linter, then commit at a logical checkpoint."
gate = true                    # a checkpoint to review before moving on (guidance only)
exit = ["build, tests, and linter pass", "commit follows Conventional Commits"]
```

- **`name` → slot:** matched case-insensitively against synonyms —
  `research`/`investigate`→explore, `specify`/`spec`/`design`→brainstorm,
  `iterate`/`code`/`build`→implement, `review`/`commit`/`ship`/`test`→verify. The
  first stage to claim a slot wins; the rest of that slot's claimants are skipped.
- **`reads`/`writes`:** a bare filename (no path separators) under
  `.loadout/workflow/artifacts/`; pair a producer's `writes` with a consumer's
  `reads` to form a handoff.
- **`gate` / `exit`:** rendered as a review checkpoint and a "done when" checklist
  — guidance only; loadout never blocks.

## `[binding]` (implemented)

The per-project remembered loadout choice, written when 2+ loadouts match and you
pick one. It lives in the gitignored repo `local.toml` (a global path-keyed store
is used outside a repo); loadout manages it, so you rarely hand-edit it.

```toml
[binding]
loadout = "rust — web"      # the chosen loadout (the only remembered choice)
# targets_hash = "…"        # fingerprint of the loadout's targets at bind time (freshness)
```

There is no opt-out binding — invoking loadout means you want a loadout. A legacy
`none = true` from an older loadout still parses but is ignored, so a project
stuck on it re-prompts the next time 2+ loadouts match.

## `[sync]` (implemented)

Cross-machine sync of the global config dir, git-backed (see
[Sync across machines](../README.md#sync-across-machines)). Auto-pull/push default
on but are **inert** until `load sync init` makes the dir a git repo with a
remote, so they never act on a machine that opted out.

```toml
[sync]
auto_pull    = true     # pull the latest before run/render/refresh (throttled)
auto_push    = true     # commit + push after a studio apply (best-effort)
pull_max_age = "5m"     # skip the auto-pull when synced within this window
timeout      = "5s"     # hard cap on a git network op → fall back to local config
```

`config.toml` (shared, secret-free) is tracked and syncs; `local.toml` (per-machine
hostnames/secrets) is gitignored and never leaves the box. Put `[sync]` in
`local.toml` to vary it per machine — e.g. a CI box that should pull but never
push: `auto_push = false`.

## `[[agents]]` (implemented)

Built-in agents are a base layer; override by `id` or add new ones — no code
change. Required: `id`, `generated_filename`. One sync caveat: descriptor fields
are parsed strictly, so a hand-written `[[agents]]` entry using a field from a
newer loadout (e.g. `target_file`) fails loudly on a machine running an older
binary — update that machine's `load` first. Built-ins are unaffected.

```toml
[[agents]]
id = "gemini"
generated_filename = "gemini.md"
launch = "gemini"                  # program for `load run gemini` (omit → render-only)
template = "overlay"              # body template name (repo/global override → embedded)
# importer = "GEMINI.local.md"             # auto-wire @import into a LOCAL file
# override_target = "AGENTS.override.md"   # auto-merge target, gitignored (default-on)
# override_base   = "AGENTS.md"            # file whose content seeds the override
# target_file = ".cursor/rules/loadout.mdc" # loadout-owned wired file, written raw (no marker block)
# preamble = "---\nalwaysApply: true\n---\n" # raw first bytes of target_file (e.g. MDC frontmatter)
# append_prompt_flag = "--append-system-prompt"   # run injects a freshness note via this flag
wire_hint = "include .loadout/generated/gemini.md from your agent config"
```

Built-in defaults:

| id | generated file | wiring | launch |
| --- | --- | --- | --- |
| `claude` | `claude.md` | import → `CLAUDE.local.md` | `claude` |
| `codex` | `agents.md` | auto → gitignored `AGENTS.override.md` (Codex prefers it); `--no-override` = emit-only | `codex` |
| `gemini` | `gemini.md` | auto → gitignored `GEMINI.local.md` (`@import`) + registers it in `~/.gemini/settings.json` `context.fileName` | `gemini` |
| `opencode` | `opencode.md` | registers overlay path in `~/.config/opencode/opencode.json` `instructions` | `opencode` |
| `copilot` | `copilot/.github/instructions/loadout.instructions.md` | `load run` sets `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → `.loadout/generated/copilot` | `copilot` |
| `cursor` | `cursor.md` | owned file → gitignored `.cursor/rules/loadout.mdc` (`alwaysApply` rule, IDE + CLI) + `sessionStart` hook in `~/.cursor/hooks.json` for IDE freshness | `cursor-agent` |
| `generic` | `generic.md` | emit-only | — |

## `[host_classes]` (implemented; keep mappings private)

Maps hostname globs (`*`/`?`) to a class you reference in rules
(`host_class == "work"`). **The mappings contain real hostnames/domains — keep
them in the private layer**, even though the *reference* in a loadout is public.

```toml
# put this in local.toml (private), not the shareable config:
[host_classes]
work     = ["*.corp.example.com", "work-*"]
personal = ["my-laptop", "*.tailnet.ts.net"]
```

## `[[fragments]]` (implemented)

```toml
[[fragments]]
id          = "ssh-tailnet"
description = "SSH into machines on my tailnet to do work"
category    = "Local Environment"  # groups it in studio's tree (free-form)
when        = [{ field = "host_class", op = "equals", value = "personal" }]   # self-gate
requires    = ["network-awareness"]
params      = { allowed_hosts = ["host-a", "host-b"] }   # value belongs in private layer
guidance    = """
You may SSH to {{ params.allowed_hosts | join(', ') }}.
Confirm before any destructive remote command.
"""

# dynamic fragment (native provider):
[[fragments]]
id       = "network-awareness"
provider = "tailnet"     # built-in provider (or: command = "..." for the generic shell-command form)
cache    = "60s"
guidance = "Live tailnet (as of {{ generated_at }}):\n{{ provider.output }}"
```

See [security](security.md) for how `command` execution is handled
(`allow_exec`).

## Global flags (implemented)

`--cwd <DIR>` (operate as if there), `--verbose`/`-v`, `--dry-run` (write
nothing, not even the audit log).
