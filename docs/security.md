# Security & trust

loadout is **agent guidance, not enforced policy.** Generated files are regular
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
| Generic structure | fragment guidance, loadout rules | public layer (commit / open-source) |
| Sensitive specifics | real hostnames, `host_classes` globs, fragment `params` values | **private** layer (gitignored `local.toml` / private repo) |
| Live topology | tailnet hosts, containers | **don't store** — probe at runtime via a provider |
| Secrets | tokens, keys | **never** anywhere |

Why it matters: a *public* dotfiles-style config that hard-codes which machines
you can SSH to, your employer's internal domains, or your tailnet leaks that to
the world. Keep the *behavior* public ("you may SSH within my tailnet, confirm
first") and the *specifics* private or detected.

**`load doctor` lints** the public layer and warns if a
fragment/loadout/`host_class` there contains hostname/IP/domain-looking
literals ("looks private — move it to local.toml").

## Derived artifacts are gitignored, never committed **(implemented)**

Anything loadout generates is machine-specific and local: `.loadout/generated/`,
`.loadout/logs/`, `AGENTS.override.md`, and `CLAUDE.local.md` (only when loadout
created it — if you already track it, your gitignore is left alone). Hand-authored
`AGENTS.md` / `.github/copilot-instructions.md` are committed and
never auto-edited. Committing a derived file would either churn, leak host-
specific content, or (for `AGENTS.override.md`, which Codex *prefers* over
`AGENTS.md`) force your machine's snapshot onto teammates.

gitignore management is skipped entirely outside a git repo (no stray
`.gitignore` in `$HOME`).

## Untrusted plan.json **(moved to artefacto)**

`load plan` hands an agent-authored `.loadout/workflow/artifacts/plan.json` to
[artefacto](https://github.com/elleryfamilia/artefacto), which treats the
agent's model output as **untrusted input**: markdown fields go through a
sanitizer, ids are constrained, the page carries a content security policy,
and input limits bound a runaway or hostile document. That work, and its
tests, live with the renderer. What loadout still does:

- **Runs artefacto as a subprocess, never bundles it.** `load plan` finds the
  binary on `PATH` (or at `LOADOUT_ARTEFACTO_BIN`), offers to install it from
  artefacto's own release installer on a terminal, and prints the one-liner off
  one. It never downloads anything without asking.
- **Gitignore-before-write.** Every `load plan` verb ensures three exact
  gitignore entries — `.loadout/workflow/artifacts/plan.json`,
  `.loadout/workflow/artifacts/plan-feedback.json`, `.loadout/generated/` —
  before doing anything else, and only inside a git repo. A plan (which may
  quote code and secrets) must never be committable by accident.
- **Marker-gated removal and serving.** `load plan clean` and `load clean`
  remove a `plan.html`, and studio's Recents serves one, only when its first
  line is the generated marker — loadout's own or artefacto's, whose exact bytes are
  pinned by `tests/fixtures/artefacto-marker-first-line.txt`. A file that is
  neither is left alone and never served.

## Command execution **(implemented)**

Dynamic fragments can run code at render time, so the surface is kept small:

- **Built-in providers** (`host`, `toolchain`, `ai-tools`, `tailnet`, `docker`)
  are loadout-controlled probes — they never run arbitrary commands.
- **`command`-backed fragments** run a shell command. The per-fragment
  `allow_exec` flag is the off-switch: `allow_exec = false` makes loadout embed a
  skip note instead of running it.
- **Fragments are global-only** (see [configuration](configuration.md)).
  They're honored only from your built-in / global / global-local config — *you*
  author them. A cloned repo cannot contribute a fragment at all: repo-declared
  fragments are dropped by the loader and `doctor` flags them. So there's no
  "untrusted command from a cloned repo" to gate — the global-only model removes
  that surface rather than prompting for it (there is no `loadout allow`).
- Provider/command output is treated as sensitive (see the split above):
  local/gitignored only, redacted, never committed.

So `load refresh` in a cloned repo composes only *your* global library — it
never reads or runs what the repo itself declares.

## Threat model summary

loadout defends against: leaking secrets into overlays; leaking sensitive
topology into shareable/committed config; and running code a cloned repo tries
to introduce (it can't — fragments are global-only). It does **not** attempt
to constrain what the agent does once it reads the overlay — that is out of scope
by design (guidance, not policy).

## Studio artifact serving

The studio Recents tab serves rendered plan previews through
`GET /artifacts/<id>`. Artifact HTML is model output — potentially hostile —
so it is never trusted with studio's origin:

- The id indexes a per-machine registry written only by `load` itself; no
  filesystem path is ever derived from the request.
- Only files that begin with the loadout-generated marker are served, and
  the response carries `Content-Security-Policy: sandbox allow-scripts …` —
  a header-level sandbox that gives the document an opaque origin. Its
  scripts run, but studio's session cookie, API, and storage are
  unreachable (`default-src 'none'` blocks fetch outright, and the exact
  Origin check on state-changing routes rejects the opaque origin's
  `Origin: null`).
- Artifact bytes are never inlined into a studio-origin response, never
  served via `blob:` URLs, and the sandbox never gains `allow-same-origin`.
