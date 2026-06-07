# Configuration

rosita is configured by layered TOML. **(implemented)** unless marked
**(planned)**.

## Layers (precedence: later wins)

Built-in defaults ← global `config.toml` ← global `local.toml` ← repo
`config.toml` ← repo `local.toml`.

| Layer | Path | Shareable? |
| --- | --- | --- |
| global | `$ROSITA_CONFIG_DIR` or `$XDG_CONFIG_HOME/rosita` or `~/.config/rosita`, file `config.toml` | yes (commit / open-source) |
| global-local | `<global>/local.toml` | **no** (gitignored / private) |
| repo | `<repo_base>/.rosita/config.toml` | yes (committable) |
| repo-local | `<repo_base>/.rosita/local.toml` | **no** (gitignored) |

**Fragments and profiles are global-only.** You author them once, in the
global layers, and share them across machines by committing `config.toml` to a
synced repo. A repo's `.rosita/` carries only the per-project **`[binding]`** (in
the gitignored `local.toml`), the generated overlays, the audit log, the probe
cache, and optional template overrides — *not* fragments or profiles.
Fragments or profiles declared in a repo layer are ignored, and `rosita
doctor` flags them.

- `$ROSITA_CONFIG_DIR` overrides the global dir (used in tests / isolation).
- Templates resolve repo → global → embedded; agents merge **by id** (later
  layers override).
- Lists like `env.allowlist` are **additive** across layers (union, deduped).
- The merge keeps "unset" distinct from "default": each layer parses into an
  all-optional `RawConfig`, layers fold, then defaults are applied.

Other directories under `<repo_base>/.rosita/`: `generated/` (overlays,
gitignored), `logs/events.jsonl` (audit, gitignored), `templates/` (overrides),
`cache/` (gitignored — provider caches).

## `[defaults]` (implemented)

```toml
[defaults]
agent = "claude"   # agent used when --agent is omitted
```

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

## `[[profiles]]` (implemented)

A profile is tied to one or more detected **targets** and composes a list of
fragments. It is the unit of selection — one profile per context.

```toml
[[profiles]]
name = "rust — web"
targets = ["rust"]                                  # selected when the repo detects as rust
fragments = [
  "rust-conventions",
  { id = "ssh", params = { user = "deploy" } },     # optional inline params override
]
# guidance = "…"        # optional inline guidance (becomes a <profile>:inline fragment)
# template = "infra"    # optional body-template override
# disabled = true       # keep the definition but never select or compose it
```

- **`targets`:** the coarse detected tags — `stack` values `rust`, `node`,
  `nextjs`, `go`, `python`, `android`, `java`, plus `machine` (the no-repo
  context). A profile is a selection candidate when **any** of its targets
  matches. Empty `targets` ⇒ never auto-selected (still bindable by name).
- **Selection is pick-one:** of the profiles whose targets match, exactly one is
  used — 0 → none (empty overlay), 1 → auto, 2+ → you pick once and it's
  remembered (the [`[binding]`](#binding-implemented)). Profiles do **not** merge;
  there is no `priority`, `exclude`, or `exclusive`, and no built-in profiles.
- A saved profile needs **≥1 fragment** (studio enforces it; the parser accepts
  zero for hand-edits).

Profiles select on `targets`, not rules — a *fragment* may still self-gate with
`when` rules (see [`[[fragments]]`](#fragments-implemented)).

## `[binding]` (implemented)

The per-project remembered profile choice, written when 2+ profiles match and you
pick one. It lives in the gitignored repo `local.toml` (a global path-keyed store
is used outside a repo); rosita manages it, so you rarely hand-edit it.

```toml
[binding]
profile = "rust — web"      # the chosen profile … or:  none = true  to opt this project out
# targets_hash = "…"        # fingerprint of the profile's targets at bind time (freshness)
```

## `[sync]` (implemented)

Cross-machine sync of the global config dir, git-backed (see
[Sync across machines](../README.md#sync-across-machines)). Auto-pull/push default
on but are **inert** until `rosita sync init` makes the dir a git repo with a
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
change. Required: `id`, `generated_filename`.

```toml
[[agents]]
id = "gemini"
generated_filename = "gemini.md"
launch = "gemini"                  # program for `rosita run gemini` (omit → render-only)
template = "overlay"              # body template name (repo/global override → embedded)
# importer = "GEMINI.local.md"             # auto-wire @import into a LOCAL file
# override_target = "AGENTS.override.md"   # auto-merge target, gitignored (default-on)
# override_base   = "AGENTS.md"            # file whose content seeds the override
# append_prompt_flag = "--append-system-prompt"   # run injects a freshness note via this flag
wire_hint = "include .rosita/generated/gemini.md from your agent config"
```

Built-in defaults:

| id | generated file | wiring | launch |
| --- | --- | --- | --- |
| `claude` | `claude.md` | import → `CLAUDE.local.md` | `claude` |
| `codex` | `agents.md` | auto → gitignored `AGENTS.override.md` (Codex prefers it); `--no-override` = emit-only | `codex` |
| `gemini` | `gemini.md` | auto → gitignored `GEMINI.local.md` (`@import`) + registers it in `~/.gemini/settings.json` `context.fileName` | `gemini` |
| `opencode` | `opencode.md` | registers overlay path in `~/.config/opencode/opencode.json` `instructions` | `opencode` |
| `copilot` | `copilot/.github/instructions/rosita.instructions.md` | `rosita run` sets `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → `.rosita/generated/copilot` | `copilot` |
| `generic` | `generic.md` | emit-only | — |

## `[host_classes]` (implemented; keep mappings private)

Maps hostname globs (`*`/`?`) to a class you reference in rules
(`host_class == "work"`). **The mappings contain real hostnames/domains — keep
them in the private layer**, even though the *reference* in a profile is public.

```toml
# put this in local.toml (private), not the shareable config:
[host_classes]
work     = ["*.corp.example.com", "work-*"]
personal = ["my-laptop", "*.tailnet.ts.net"]
```

## `[[fragments]]` (implemented)

> Renamed from `[[capabilities]]`. The old key (and the inner profile key
> `capabilities = [...]`, and `[fragment_params]`'s former `[capability_params]`)
> is still accepted, so existing configs load unchanged; studio writes the new
> `[[fragments]]` form. Guidance templates may use `{{ fragment }}` or the
> back-compat alias `{{ capability }}`.

```toml
[[fragments]]
id          = "ssh-tailnet"
description = "SSH into machines on my tailnet to do work"
category    = "Local Environment"  # groups it in studio's tree (free-form)
tags        = ["infra", "network"] # free-form discovery keywords
risk        = "caution"            # info | caution | dangerous (informational)
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
