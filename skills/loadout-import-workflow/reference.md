# loadout workflow reference (for the import skill)

Workflows live in `~/.config/loadout/config.toml` as `[[workflows]]` blocks and
are **global-only** — a repo's `.loadout/` may declare them but the loader
strips them, so never write a workflow into a repo.

## The fixed spine

loadout always exposes the same six slash commands. A workflow fills these
slots; it never adds or renames commands.

| # | slot | what the step is (process, workflow-independent) |
|---|------|--------------------------------------------------|
| 1 | `explore`   | Understand the problem and the code before changing anything. |
| 2 | `brainstorm`| Shape the idea — the design or the spec. |
| 3 | `plan`      | Break it into an ordered task list. |
| 4 | `implement` | Build it. |
| 5 | `verify`    | Check the result — tests, review, quality. |
| 6 | `ship`      | Commit, push, and open the PR. |

A source step whose name matches no slot becomes an **extra**, rendered after
the six in declaration order.

## Mapping table (source step name → slot)

Match case-insensitively. Anything not listed here is an **extra**.

| slot | names that map to it |
|------|----------------------|
| `explore`   | explore, research, investigate, understand, scope |
| `brainstorm`| brainstorm, specify, spec, design, ideate, discovery |
| `plan`      | plan, planning, decompose |
| `implement` | implement, iterate, code, build, execute, develop |
| `verify`    | verify, review, test, validate, qa |
| `ship`      | ship, commit, pr, push, deliver, release, deploy, merge, finish, finishing |

Examples: a "Research" command → `explore`; "Specify" → `brainstorm`;
"Review" → `verify` and "Commit & PR" → `ship` (they're separate phases now —
each keeps its own command); a "Capture learnings" step → an **extra**.

## `[[workflows]]`

The parser is strict (`deny_unknown_fields`). Top-level keys:

| key | required | notes |
|-----|----------|-------|
| `id` | yes | kebab-case, unique. How a profile/`[defaults]` binds it. A user id equal to a built-in's **shadows** that built-in. |
| `name` | no | display title on the studio gallery card and the rendered heading. |
| `description` | no | one-line blurb on the card. |
| `icon` | no | studio glyph: `bolt`, `rocket`, `git-branch`, `book`, `refresh`, `package`, … |
| `stages` | yes¹ | the ordered steps, as `[[workflows.stages]]` sub-tables (below). |
| `modeled_on` | no | provenance: the suite it's drawn from (display-only). |
| `researched` | no | provenance: a short research note (display-only). |
| `source` | no | upstream URL (display-only; future source-sync hangs off it). |
| `disabled` | no | `true` keeps the definition but never selects it. |

¹ A workflow needs ≥1 stage (surfaced by `load doctor`, not rejected at parse).

### `[[workflows.stages]]`

| key | required | notes |
|-----|----------|-------|
| `name` | yes | the source step's name. Determines the slot via the table above; an unmatched name is an extra. |
| `purpose` | no | the one-line contract rendered into the step (the author's framing, condensed). The label shown everywhere — the always-on `## Workflow` context map, the command's frontmatter, and the studio card. |
| `instructions` | no | the step's elaborate, multi-line body — the full prescriptive guidance. Injected **only** into the per-step `/loadout:<command>` file when that command runs, never into the always-on context map. This is where an import carries real substance; markdown, written as a TOML multi-line string (`"""…"""`). |
| `reads` | no | a bare handoff filename it consumes, e.g. `plan.md` (lives under `.loadout/workflow/artifacts/`). |
| `writes` | no | a bare handoff filename it produces, e.g. `plan.md`. |
| `gate` | no | `true` marks a checkpoint the user reviews before moving on (guidance only). |
| `exit` | no | a "done when…" checklist, as an array of short strings. |

`reads`/`writes` must be a plain filename — no slashes, no `..`, not hidden.
Pair them: the producer's `writes` and the consumer's `reads` use the **same**
filename to form the handoff.

## Worked example — Every's compound-engineering-plugin

Source: `https://github.com/EveryInc/compound-engineering-plugin`. Its commands
describe a plan-heavy loop that ends by capturing what was learned. Reading its
steps and mapping them:

| source step | slot | handoff / notes |
|-------------|------|-----------------|
| brainstorm requirements (interactive Q&A) | `brainstorm` | writes `requirements.md` |
| plan in detail (~80% of the work) | `plan` | reads `requirements.md`, writes `plan.md` |
| implement in a worktree, then simplify | `implement` | reads `plan.md` |
| multi-agent review against the plan | `verify` | reads `plan.md`, gate |
| capture learnings into docs/ | **extra** | the step that compounds — matches no slot |

`explore` is left empty (this process jumps straight to brainstorming
requirements). The resulting block:

```toml
[[workflows]]
id = "compound"
name = "Compound engineering"
description = "Every's loop where each cycle makes the next one easier."
icon = "package"
modeled_on = "Every (Kieran Klaassen & T.M. Chow)"
researched = "Plan-heavy cycles that END by capturing what you learned, so each cycle compounds and the next starts ahead."
source = "https://github.com/EveryInc/compound-engineering-plugin"

[[workflows.stages]]
name = "brainstorm"
purpose = "Interactive Q&A to pin down requirements — produce a right-sized requirements doc before any code."
writes = "requirements.md"

[[workflows.stages]]
name = "plan"
purpose = "Turn the requirements into a detailed implementation plan with safeguards. Planning is ~80% of the work."
instructions = """
Planning is where most of the work happens — treat a thin plan as the bug, not the implementation.

- Turn each requirement into concrete, ordered tasks with the exact files and changes spelled out.
- Build in safeguards: for every risky step, say how you'll know it worked and how you'd back it out.
- Resolve unknowns here, in the plan, rather than leaving them for the implementer to improvise.
- The plan you hand off should let a fresh agent build the whole thing without re-deriving the design.
"""
reads = "requirements.md"
writes = "plan.md"

[[workflows.stages]]
name = "implement"
purpose = "Execute the plan in a worktree, tracking each task, then simplify the new code for clarity and reuse."
reads = "plan.md"

[[workflows.stages]]
name = "review"
purpose = "Multi-agent review against the plan before merging."
reads = "plan.md"
gate = true
exit = ["reviewed against the plan by independent agents", "issues fixed before merging"]

[[workflows.stages]]
name = "compound"
purpose = "Capture what you learned into docs/solutions/ so the next cycle starts ahead — the step that compounds."
```

Note the `review` stage is **named** `review` but fills the `verify` slot (per
the mapping table), and `compound` matches no slot so it renders as an extra
after the six. This workflow already ships as the built-in `compound` — so it
doubles as an answer key. When you import a framework that *isn't* shipped, give
it a fresh `id` and the same treatment.

## Activating the imported workflow

A `[[workflows]]` block does nothing until it's selected. Add one of:

```toml
# Global active — applies in every repo (the common choice):
[defaults]
workflow = "compound"
```

```toml
# Per-profile — applies only where this profile binds (advanced):
[[loadouts]]
name = "rust"
targets = ["rust"]
fragments = ["..."]
workflow = "compound"
```

Then `load doctor` should report `active workflow: 'compound'` and
`load refresh` regenerates the six `/loadout:*` commands with the imported
steps.
