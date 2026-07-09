# loadout docs

`loadout` injects global context into your AI coding agents: detect the
project/runtime context, select the **one** loadout that fits, compose its
fragments into a single agent-neutral overlay, and deliver it to each agent —
keeping it fresh and never leaking secrets.

## For consumers
- [Concepts](concepts.md) — the mental model (context, fragments, loadouts, the binding, workflows, plan previews, agents, freshness, public/private).
- [Configuration](configuration.md) — the layered config and full schema reference.
- [Security](security.md) — secrets, redaction, the public/private split, command execution (`allow_exec`), and untrusted `plan.json` handling.

## For developers / extenders
- [Architecture](architecture.md) — modules, trait seams, and the render pipeline (reflects the current code).
- [Extending](extending.md) — add an agent, a fragment, a provider, a detector, a rule, or change the plan schema.
- [Testing](testing.md) — the automated suite, a hands-on CLI walkthrough, and the plan viewer's browser smoke test.

## Status legend
Sections are marked **(implemented)** — shipped in the current binary. Everything
documented here ships today.
