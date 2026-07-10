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
`AGENTS.md` / `GEMINI.md` / `.github/copilot-instructions.md` are committed and
never auto-edited. Committing a derived file would either churn, leak host-
specific content, or (for `AGENTS.override.md`, which Codex *prefers* over
`AGENTS.md`) force your machine's snapshot onto teammates.

gitignore management is skipped entirely outside a git repo (no stray
`.gitignore` in `$HOME`).

## Untrusted plan.json **(implemented)**

`load plan render` turns an agent-authored `.loadout/workflow/artifacts/plan.json`
into `plan.html`. The agent's model output is treated as **untrusted input**,
same as any other generated text an agent might produce, so the renderer
defends on several layers:

- **Markdown is sanitized, not trusted.** Every markdown field (`goal_md`,
  `summary_md`, `mitigation_md`, `question_md`) goes through a shared
  sanitizer (`src/markdown.rs`): raw HTML is neutralized to plain text; link
  destinations are limited to `http://`, `https://`, `mailto:`, an
  in-page `#fragment`, or a scheme-less relative path — anything else
  (`javascript:`, `data:`, `vbscript:`, …) is checked case-insensitively
  after stripping control/whitespace characters (so `java\tscript:` can't
  sneak through) and de-linked to plain text. Images never fetch: a safe
  destination renders as a link instead of `<img>`; an unsafe one renders as
  plain emphasis. **This is a security fix, not just a plan-viewer feature:**
  this same sanitizer now backs `load studio`'s markdown previews too
  (fragment guidance, loadout overlays). Before this change, studio escaped
  raw HTML but did not check link URL schemes — a `[text](javascript:…)` link
  in guidance markdown rendered as a real, clickable `<a href="javascript:…">`.
  Guidance text can come from a cloned repo's fragments, so that was a
  latent XSS. It's closed now, in both surfaces.
- **`icon` is a closed vocabulary, not free text.** A phase's or task's
  `icon` field must name one of 16 vendored Lucide icons (`unknown_icon`
  rejects anything else); the renderer inlines the matching SVG directly,
  which is only safe *because* the value can never be attacker-controlled
  markup — it's a lookup key into loadout's own vendored assets, not
  `plan.json`-supplied SVG content.
- **The embedded JSON data island is escaped, not just serialized.** The
  rendered page includes a `<script type="application/json">` copy of the
  plan for the client-side comment tooling. Before embedding, `<`, `>`, `&`,
  U+2028, and U+2029 are replaced with `\uXXXX` JSON escapes, so the island
  can never contain a literal `</script>` or `<!--` — breaking out of the
  script tag is not possible, full stop. `\uXXXX` is an ordinary JSON string
  escape, so `JSON.parse` decodes it back to the original character; the
  island still parses to the same data.
- **The island is a display-sanitized copy, not the canonical model.**
  Its markdown fields are pre-rendered through the same sanitizer as the
  visible page, so the artifact never contains a raw `javascript:` payload
  anywhere — not even inertly, sitting unused in the JSON. Because of that,
  the island is *not* byte-identical to the original `plan.json`; the
  `data-plan-fingerprint` attribute on `<body>` is a hash of the original
  canonical model, not the island. Anything that needs the canonical plan
  (e.g. an agent re-reading its own output) reads `plan.json` from disk, not
  the island.
- **Strict CSP.** The document ships its own
  `Content-Security-Policy`: `default-src 'none'; style-src 'unsafe-inline';
  script-src 'unsafe-inline'; img-src data:; font-src data:`. Nothing on the
  page can fetch an external resource — no images, no scripts, no
  stylesheets, no fonts — which is also why the renderer never uses a CDN:
  the page is self-contained by construction, and the CSP would block a CDN
  reference even if one were added by mistake. `font-src data:` allows the
  embedded Inter font — base64-encoded directly into `plan.css`'s
  `@font-face` rules, not fetched — and nothing else; a `url(https://…)`
  reference would still be blocked.
- **Input limits bound the blast radius of a runaway or hostile plan.json**
  before any rendering happens: 2 MiB input size; 500 tasks; 50 phases; 100
  risks; 100 open questions; 2000 dependency edges; 10,000 characters per
  string field. Exceeding a collection limit or a string limit is a hard
  validation error (`load plan check` reports it), not a truncation.
- **Gitignore-before-write.** Every `load plan` verb ensures three exact
  gitignore entries — `.loadout/workflow/artifacts/plan.json`,
  `.loadout/workflow/artifacts/plan-feedback.json`, `.loadout/generated/` —
  before doing anything else, and only inside a git repo. A plan (which may
  contain draft, half-finished, or sensitive task detail) or its rendered
  HTML can't end up committed by accident.

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
