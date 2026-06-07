# rosita config reference (for the migration skill)

Everything lives in `~/.config/rosita/config.toml` (public, shareable) and,
for machine-specific values, `~/.config/rosita/local.toml` (private,
gitignored). Fragments and profiles are **global-only** — never put them in
a repo's `.rosita/`.

## `[[fragments]]`

A fragment is one reusable unit of context. The parser is strict
(`deny_unknown_fields`) — only these keys are valid:

| key | required | notes |
|-----|----------|-------|
| `id` | yes | kebab-case, unique. How profiles reference it. |
| `description` | no | short title shown in listings. |
| `guidance` | no¹ | the instructions. A minijinja template — may reference `{{ params.x }}` and (for dynamic caps) `{{ provider.data.x }}`. |
| `category` | no | human-friendly group label shown in studio's tree, e.g. `"Safety"`. |
| `agents` | no | restrict to certain agents, e.g. `["claude", "codex"]`. Empty/absent = all. |
| `when` | no | conditions that gate the fragment (advanced; usually omit). |
| `requires` | no | ids of other fragments to pull in first. |
| `params` | no | default values for `{{ params.* }}` in guidance. |
| `provider` | no² | a built-in live probe: `host`, `toolchain`, `ai-tools`, `tailnet`, `docker`. Output is exposed as `{{ provider.data.* }}` / `{{ provider.output }}`. Always trusted. |
| `command` | no² | a shell script whose stdout becomes the rendered body. Set `script_lang = "bash"`. |
| `script_lang` | no | language for `command` (use `"bash"`). |
| `cache` | no | for dynamic caps: how long to cache output, e.g. `"5m"`. |

¹ A fragment needs *either* `guidance` (static) *or* `command`/`provider` (dynamic).
² `provider` and `command` are mutually exclusive; either makes the cap "dynamic".

## `[[profiles]]`

A profile composes fragments and is selected by detected context.

| key | required | notes |
|-----|----------|-------|
| `name` | yes | unique. Also the binding key and template name. |
| `targets` | no | stacks this profile applies to: `["rust"]`, `["node"]`, `["python"]`, `["go"]`, … or `["machine"]` for the no-repo context. The profile binds where **any** target matches the repo's detected stacks. Empty `targets` ⇒ never auto-selected (still bindable by name). |
| `fragments` | no | ordered list of fragment ids (or `{ id = "x", params = { … } }` for per-profile param overrides). A saved profile needs ≥1. |
| `guidance` | no | inline guidance appended as a synthetic fragment. |
| `disabled` | no | `true` keeps the definition but never selects it. |

Only **one** profile binds per repo: 0 matches → no overlay; exactly 1 → it's
used; 2+ → rosita asks once and remembers the choice.

## Worked example

A typical decomposition of a personal `CLAUDE.md` into global caps + a `machine`
profile (the universal rules) plus a `rust` profile (stack-specific):

```toml
# --- universal working rules (compose into every context) ---

[[fragments]]
id = "terse-comms"
description = "Communication style"
guidance = """
Default to terse: lead with the result and what changed; skip preamble.
For non-trivial decisions, briefly explain the reasoning and tradeoffs.
"""

[[fragments]]
id = "guardrails"
description = "Safety guardrails"
category = "Safety"
guidance = """
Never commit or push directly to main/master — always work on a branch.
Never print, log, or commit secrets, credentials, or .env files.
Ask before deleting files or running hard-to-reverse actions.
"""

[[fragments]]
id = "conventional-commits"
description = "Git commit conventions"
guidance = "Use Conventional Commits (feat:/fix:/refactor:/docs:). Imperative subject ≤72 chars; body explains why."

# --- live environment context (a dynamic script fragment) ---

[[fragments]]
id = "host"
description = "Host"
script_lang = "bash"
cache = "5m"
command = '''
printf 'os:       %s\n' "$(uname -s)"
printf 'arch:     %s\n' "$(uname -m)"
printf 'hostname: %s\n' "$(hostname 2>/dev/null || echo unknown)"
'''

# --- a built-in provider fragment (no script to maintain) ---

[[fragments]]
id = "toolchain"
description = "Toolchain"
provider = "toolchain"
guidance = "Detected toolchains:\n{{ provider.output }}"

# --- profiles: the matching one binds per repo ---

[[profiles]]
name = "machine"
targets = ["machine"]            # the no-repo context
fragments = ["terse-comms", "guardrails", "conventional-commits", "host", "toolchain"]

[[profiles]]
name = "rust"
targets = ["rust"]               # any repo detected as rust
fragments = ["terse-comms", "guardrails", "conventional-commits"]
```

### Private values → `local.toml`

If a fragment's guidance needs a real hostname or other machine-specific
literal, keep it out of the public config:

```toml
# ~/.config/rosita/config.toml  (public)
[[fragments]]
id = "deploy"
description = "Deploy target"
guidance = "Deploy as {{ params.user }}@{{ params.host }}."

# ~/.config/rosita/local.toml  (private, gitignored)
[fragment_params.deploy]
host = "box.internal.example"
user = "deployer"
```

`rosita doctor` leak-lints the public config and tells you when a literal looks
private and belongs in `local.toml`.
