# loadout config reference (for the remember skill)

Everything lives in `~/.config/loadout/config.toml` (public, shareable) and,
for machine-specific values, `~/.config/loadout/local.toml` (private,
gitignored). Fragments and profiles are **global-only** — never put them in
a repo's `.loadout/`.

## `[[fragments]]`

A fragment is one reusable unit of context. The parser is strict
(`deny_unknown_fields`) — only these keys are valid:

| key | required | notes |
|-----|----------|-------|
| `id` | yes | kebab-case, unique. How profiles reference it. |
| `description` | no | short title shown in listings. |
| `guidance` | no¹ | the instructions. A minijinja template — may reference `{{ params.x }}`. |
| `category` | no | human-friendly group label shown in studio, e.g. `"Safety"`. |
| `agents` | no | restrict to certain agents, e.g. `["claude", "codex"]`. Empty/absent = all. |
| `when` | no | conditions that gate the fragment (advanced; usually omit). |
| `requires` | no | ids of other fragments to pull in first. |
| `params` | no | default values for `{{ params.* }}` in guidance. |
| `provider` | no² | a built-in live probe: `host`, `toolchain`, `ai-tools`, `tailnet`, `docker`. |
| `command` | no² | a shell script whose stdout becomes the rendered body. Set `script_lang = "bash"`. |
| `script_lang` | no | language for `command` (use `"bash"`). |
| `cache` | no | for dynamic fragments: how long to cache output, e.g. `"5m"`. |

¹ A fragment needs *either* `guidance` (static) *or* `command`/`provider` (dynamic).
² `provider` and `command` are mutually exclusive.

## `[[loadouts]]`

A profile composes fragments and is selected by detected context.

| key | required | notes |
|-----|----------|-------|
| `name` | yes | unique. |
| `targets` | no | stacks this profile applies to: `["rust"]`, `["node"]`, … or `["machine"]` for the no-repo context. Empty ⇒ the catch-all default. |
| `fragments` | no | ordered list of fragment ids (or `{ id = "x", params = { … } }`). |
| `guidance` | no | inline guidance appended as a synthetic fragment. |
| `disabled` | no | `true` keeps the definition but never selects it. |

## Editing example

The user says *"stop suggesting `git push --force`; always use
`--force-with-lease`"* and the overlay's "Git commit conventions" section maps
to the `conventional-commits` fragment. The minimal edit — only that
fragment's `guidance` changes, everything else byte-identical:

```toml
[[fragments]]
id = "conventional-commits"
description = "Git commit conventions"
guidance = """
Use Conventional Commits (feat:/fix:/refactor:/docs:). Imperative subject
≤72 chars; body explains why.
Never `git push --force`; use `--force-with-lease` when a force is required.
"""
```

A brand-new preference instead becomes a new fragment plus a one-line addition
to each profile that should carry it:

```toml
[[fragments]]
id = "dependency-policy"
description = "Dependency policy"
guidance = "Ask before adding a new dependency; prefer the standard library when reasonable."

[[loadouts]]
name = "machine"
targets = ["machine"]
fragments = ["conventional-commits", "dependency-policy"]
```

(In a real edit you *add* `"dependency-policy"` to the existing profile's
`fragments` list rather than restating the profile.)

### Private values → `local.toml`

If guidance needs a real hostname or other machine-specific literal, keep it
out of the public config:

```toml
# ~/.config/loadout/config.toml  (public)
[[fragments]]
id = "deploy"
description = "Deploy target"
guidance = "Deploy as {{ params.user }}@{{ params.host }}."

# ~/.config/loadout/local.toml  (private, gitignored)
[fragment_params.deploy]
host = "box.internal.example"
user = "deployer"
```

`load doctor` leak-lints the public config and tells you when a literal looks
private and belongs in `local.toml`.
