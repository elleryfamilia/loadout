# Security & trust

rosita is **agent guidance, not enforced policy.** Generated files are regular
files an agent reads — treat them as advice, not a control plane. The notes
below are about *hygiene* (don't leak secrets, don't surprise teammates, don't
execute untrusted code), not about constraining the agent.

## Secrets are never stored **(implemented)**

- **Env is allowlist-only.** Only names in `env.allowlist` are surfaced; any name
  matching `env.deny_name_patterns` is dropped even if allowlisted; values are
  then run through redaction as a backstop.
- **Redaction** (`src/redact.rs`) strips embedded URL credentials
  (`user:pass@host`) and common token formats: GitHub (`ghp_`/`github_pat_`…),
  AWS (`AKIA…`), Slack (`xox…`), Google (`AIza…`), OpenAI/Anthropic (`sk-`/
  `sk-ant-`), JWTs, PEM private-key blocks, and generic `secret/token/key =
  value` assignments. Conservative by design — over-redacts rather than leaks.
- Git remote URLs are credential-sanitized before they're ever surfaced.

## The public/private split **(implemented)**

The rule: **references are public; definitions of sensitive specifics are
private.**

| Kind | Example | Where it lives |
| --- | --- | --- |
| Generic structure | fragment guidance, profile rules | public layer (commit / open-source) |
| Sensitive specifics | real hostnames, `host_classes` globs, fragment `params` values | **private** layer (gitignored `local.toml` / private repo) |
| Live topology | tailnet hosts, containers | **don't store** — probe at runtime via a provider |
| Secrets | tokens, keys | **never** anywhere |

Why it matters: a *public* dotfiles-style config that hard-codes which machines
you can SSH to, your employer's internal domains, or your tailnet leaks that to
the world. Keep the *behavior* public ("you may SSH within my tailnet, confirm
first") and the *specifics* private or detected.

**`rosita doctor` lints** the public layer and warns if a
fragment/profile/`host_class` there contains hostname/IP/domain-looking
literals ("looks private — move it to local.toml").

## Derived artifacts are gitignored, never committed **(implemented)**

Anything rosita generates is machine-specific and local: `.rosita/generated/`,
`.rosita/logs/`, `AGENTS.override.md`, and `CLAUDE.local.md` (only when rosita
created it — if you already track it, your gitignore is left alone). Hand-authored
`AGENTS.md` / `GEMINI.md` / `.github/copilot-instructions.md` are committed and
never auto-edited. Committing a derived file would either churn, leak host-
specific content, or (for `AGENTS.override.md`, which Codex *prefers* over
`AGENTS.md`) force your machine's snapshot onto teammates.

gitignore management is skipped entirely outside a git repo (no stray
`.gitignore` in `$HOME`).

## Command execution **(implemented)**

Dynamic fragments can run code at render time, so the surface is kept small:

- **Built-in providers** (`host`, `toolchain`, `ai-tools`, `tailnet`, `docker`)
  are rosita-controlled probes — they never run arbitrary commands.
- **`command`-backed fragments** run a shell command. The per-fragment
  `allow_exec` flag is the off-switch: `allow_exec = false` makes rosita embed a
  skip note instead of running it.
- **Fragments are global-only** (see [configuration](configuration.md)).
  They're honored only from your built-in / global / global-local config — *you*
  author them. A cloned repo cannot contribute a fragment at all: repo-declared
  caps are dropped by the loader and `doctor` flags them. So there's no
  "untrusted command from a cloned repo" to gate — the global-only model removes
  that surface rather than prompting for it (there is no `rosita allow`).
- Provider/command output is treated as sensitive (see the split above):
  local/gitignored only, redacted, never committed.

So `rosita render` in a cloned repo composes only *your* global library — it
never reads or runs what the repo itself declares.

## Threat model summary

rosita defends against: leaking secrets into overlays; leaking sensitive
topology into shareable/committed config; and running code a cloned repo tries
to introduce (it can't — fragments are global-only). It does **not** attempt
to constrain what the agent does once it reads the overlay — that is out of scope
by design (guidance, not policy).
