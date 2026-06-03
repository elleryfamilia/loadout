# rosita studio — design

> Status: design + wireframes complete (v2). Supersedes the exploratory v1.
> Reflects the full design pass (3 explorations → synthesis → critic → codex
> gpt-5.5 review) **and** the subsequent model simplifications agreed with the
> maintainer. UI wireframes are in §16. Next step: implementation at Slice 0 (§15).

## 1. Objective & philosophy

`rosita studio` is an **ephemeral, localhost-only web UI** — run a command, it
starts a server, opens the browser, and exits on Ctrl-C — for **viewing,
creating, and managing capabilities and profiles** visually. It is **not a
daemon** and not a service.

The guiding principle: studio is a **lens and editor over your plain TOML
config**, never a hidden store. That yields three non-negotiables:

1. **Files stay the source of truth** — git-diffable, hand-editable,
   comment-preserving. Hand-edit a file and studio reflects it; studio writes
   clean TOML you could have written yourself.
2. **Nothing is hidden** — every screen shows the *resulting overlay* (the actual
   `claude.md`) for a chosen context, live.
3. **The layering and sharing are visible** — where a thing lives (global vs
   repo), whether it's shareable (public `config.toml`) or private
   (gitignored `local.toml`), is a first-class, legible control.

## 2. Locked decisions

- **CRUD scope (v1):** capabilities and profiles only. Explicitly deferred to
  hand-editing / future tabs: `[env]` allow/deny, `[defaults]`, `[codex]`,
  `[[agents]]`, per-profile `templates/*.j2`.
- **Write model:** stage edits → show the exact per-file TOML diff → Apply.
  Format/comment-preserving via `toml_edit`.
- **Frontend:** server-rendered HTML + **maud** + **htmx**, assets embedded with
  **rust-embed**; diffs via **similar**. Single static binary, no JS build step,
  no SPA.
- **Server:** **`tiny_http`** (blocking, synchronous) — no async runtime, no
  cargo feature gate. State sits behind `Arc<Mutex<Session>>`, which already
  serializes mutations, so async buys nothing for ~20 local fragment routes.
  *Operational rule: never hold the session mutex across rendering, disk I/O, or
  probe execution.*
- **Command:** `rosita studio` (`--port`, `--no-open`; cwd via the existing
  global `--cwd`).

## 3. Capability model — a flat, owned library

This is the core simplification relative to v1's layered/override framing.

- **Capabilities are one flat library you own.** There are **no defaults, no
  seeding, no always-on baseline.**
- **Shipped capabilities are a read-only *palette*** compiled into the binary —
  things you can *pick from* when composing a profile. They are **never
  auto-composed and never written into your config**. To customize one, you
  **duplicate it into your config** and own the copy.
- **A profile renders exactly the capabilities added to it.** A profile with
  zero capabilities **cannot be saved** (≥1 required). An empty overlay happens
  only when *no profile applies* to the current context.
- **Delete just deletes.** No override, no tombstone, no "reveal the layer
  beneath." (You only ever edit/create/delete entries that physically live in
  *your* config files; palette items are immutable templates.)

Consequence in code: `compose()` stops injecting built-in capabilities; the
always-on `default`/`baseline` profile is removed; `builtin_capabilities()`
becomes the palette (a read-only catalog), not an active layer.

## 4. Profile & selection model — pick-one

**Additive composition is retired.** A project uses **one** profile and renders
*its* capabilities — not a union of every matching profile. This removes
priority-ordering, `exclude`, `exclusive`, and all cross-profile merging.
Composition now happens only *within* a profile (a profile is its capability
list).

**Detection stays coarse and easy.** rosita detects language/platform at the
level that's cheap and reliable to test — `rust`, `node`/`nextjs`, `go`,
`python`, plus **`android` and `java`** to add. It does **not** attempt
fine-grained detection (no AAOS detector, no framework catalog, no
content-grepping rules — all explored and dropped as over-engineering).

**A profile is tied to a detected language/platform** (its `targets`).
Specificity for projects that look alike on disk comes from *you having multiple
profiles plus a remembered choice*, not from clever detection. (AAOS vs. plain
Android, or linux-kernel vs. browser Rust: both detect as the coarse language;
you pick the right profile once.)

**Selection algorithm**, on `rosita run` / `render` / studio preview:

1. Detect the context → coarse language/platform tag(s) (and `machine` when not
   in a repo — see §5).
2. Gather profiles whose `targets` match.
3. **0 matches** → no profile (empty overlay; studio offers to create one).
4. **Exactly 1** → **auto-use it, no prompt.**
5. **2+** → **prompt the user to pick one**, then **remember the choice for that
   project.**

**Remembered choice (the binding):**
- In a repo → the repo's `.rosita/local.toml` (gitignored, per-checkout; a
  teammate's checkout makes its own choice).
- Non-repo CWD → a **global path-keyed store** (the way `trust.toml` already keys
  by repo path).
- **"None" is always an explicit choice** — you can opt a project out of rosita
  entirely, and that opt-out is remembered too.

Selection is fully deterministic and inspectable (`rosita explain` shows which
language was detected, which profiles matched, and which is bound). No LLM is
ever involved in selection — the agent only consumes the resulting overlay.

## 5. Scope — repo vs machine

- **Derived, not stored:** `Repo` when `ctx.git.is_some()`, else `Machine`.
  Exposed as `Context::scope()`. **Not added to the context hash** — the
  `git: Option<…>` field already encodes repo-vs-machine in the hash, and adding
  a field would needlessly invalidate every existing overlay.
- **Machine scope = general devops/sysadmin assistance** (anything you'd use a
  terminal for, not in a repo). Rather than a fixed task taxonomy, machine mode
  leans on the **existing providers** (`host`/`toolchain`/`tailnet`/`docker`) for
  situational awareness plus a **careful-operator safety posture** capability;
  you add your own caps for your workflows. `machine` is just the "no repo"
  context for selection purposes.
- **Machine overlay delivery = emit-only in v1.** With no repo to write into,
  render the overlay to a namespaced global location and surface (a) a wire hint
  and (b) the `run --append-system-prompt` ephemeral path. **Do not auto-edit
  `~/.claude/CLAUDE.md`** in v1: on the maintainer's machine (and commonly) it is
  a *symlink into a git repo*, so rosita's atomic temp-file+rename would either
  de-link the symlink or edit a version-controlled file — both violate rosita's
  invariant of never modifying committed instruction files. Safe auto-wire
  (symlink-aware write + namespaced path + `rosita clean` user-scope support) is
  a deliberate **later phase**.

## 6. Sharing config across machines

The **public/private split *is* the sync boundary** — no new mechanism needed:

- **Global `config.toml`** = your reusable capabilities and profiles → commit it
  to a synced git repo (e.g. the maintainer's `_git/agent-config` dotfiles repo).
  It syncs because you commit it.
- **Global `local.toml`** = this-box specifics (real hostnames, `host_class`
  definitions, secret-adjacent params) → gitignored, never leaves the machine.
- The **leak-lint doubles as the sync-safety guard** — it already stops a real
  hostname from landing in the shareable `config.toml`, which is exactly what you
  want to keep from syncing one machine's identity to all of them.

Mechanism: prefer an **`include` directive** in `config.toml` (to pull in a
synced file) over symlinking individual config files — the atomic
temp-file+rename write would de-link a per-file symlink (same hazard as
`~/.claude/CLAUDE.md`). Relocating the whole config dir via `ROSITA_CONFIG_DIR`
to a tracked directory also works, but keep `local.toml` truly local.

## 7. Leak-lint

A **visible, non-blocking warning** — never a gate. Shown inline on the field
*and* at the diff/apply step. A single, honest warning everywhere (no
team-vs-personal cleverness): the multi-label-hostname heuristic false-positives
on legitimate values (`next.config.js`, `example.com` in prose), so it informs
and lets you decide rather than blocking. `doctor` keeps flagging it on the next
run, as today.

## 8. Write engine & data model (the risk core)

`toml_edit` is a **new dependency** (today only `toml 0.8` is used, via typed
`from_str` into `RawConfig`). It is mandatory because it preserves comments and
formatting on round-trip; the `toml` crate does not.

- **`Session`** holds, per writable layer file:
  `{ path, original_bytes, doc: DocumentMut, staged: DocumentMut, mtime, sha256 }`,
  plus an ordered, replayable `Vec<StagedOp>`, the simulator overrides, and the
  target agent.
- **`StagedOp`** (typed): `CreateCapability{layer, cap}`, `EditCapability{…}`,
  `DeleteCapability{layer, id}`, `Create/Edit/DeleteProfile{…}`,
  `SetCapabilityParam{…}`, and a `DuplicatePaletteItem{id → layer}` (the only way
  to "edit" a shipped palette cap). New array-of-tables entries are built via
  `toml_edit::Table::new()` + `array.push` so toml_edit owns formatting — never
  string concatenation. Because we mutate the parsed tree in place, comments and
  key order on untouched regions survive by construction.
- **Diff** against **raw on-disk bytes** (not the re-serialized parse): show the
  unified diff via `similar`, and when `toml_edit` normalizes untouched
  formatting (`raw != doc.to_string()`), surface that explicitly in the diff
  ("rosita will also reformat these lines") rather than hiding it — hiding it can
  mask real rewrites in hand-authored TOML. Per-file panels headed
  `scope · layer · file · public|private`.
- **Apply:** validate → external-edit re-hash gate → snapshot a one-shot `.bak`
  per touched file under `.rosita/cache/studio-backups/` → `atomic_write` each in
  a fixed order (public `config.toml` before private `local.toml`) → reload,
  reset baseline. Per-file atomic; cross-file is best-effort (backups + ordering
  + a "restore last apply" action) — no journal, documented limitation. **After
  apply, force-render the affected overlays** — config edits don't change the
  context hash, so the normal write-skip would otherwise treat stale overlays as
  up-to-date.
- **In-memory config assembly seam** — required, and not "free reuse":
  `RawConfig`/`merge`/`finalize` are private and `load_from` only reads from
  disk. Add a public `Config::from_layer_strs(Vec<(Layer, path, text)>)` that
  parses staged docs and **re-tags each capability's `origin` by layer exactly as
  disk load does** (`config.rs:106`). This is security-critical: `origin` is
  `#[serde(skip)]` and defaults to `BuiltIn`, and the trust gate keys off origin
  — a repo-authored `command` capability assembled without re-tagging would look
  built-in and **bypass `rosita allow`**.
- **Validation** (gates Apply; surfaced inline; never blocks *staging*): a new
  structured `validate()` that *returns diagnostics* (compose currently only
  logs warnings). Covers: re-parse through `RawConfig` (`deny_unknown_fields`);
  unknown/`requires`-cycle capabilities; `Op::Matches` regex compiled up-front;
  minijinja compile+render of guidance against the simulated context; the
  leak-lint warning; id/name uniqueness within a layer; and a **dangling-ref
  warning** when deleting a capability a profile still references.
- **External-edit detection:** record (mtime, sha256) at load; re-check before
  Apply and on a light `every 3s` poll; on mismatch a banner offers **Reload**
  (re-read, replay staged ops, re-diff) or **Overwrite** (explicit). Hash, not
  mtime alone (covers `git checkout`). Concurrent `rosita studio` instances on
  one repo = last-Apply-wins (documented).

## 9. HTTP API + htmx

`tiny_http` with a small `match` on `(method, path)`. Every route — **including
GET** (reads expose cached provider/env output) — requires the session token and
passes the Host-header check.

- `GET /` — page shell (library / profile composer / simulator+preview panes).
- `GET /assets/*` — embedded static (htmx, CSS).
- `GET /library`, `GET /capabilities/{id}/edit`, `POST|DELETE /capabilities/{id}`,
  `POST /capabilities/{id}/duplicate`.
- `GET /profiles/{name}/edit`, `POST|DELETE /profiles/{name}` — the composer:
  name, the **language/platform tie** (`targets`), and the **capability picker**
  (add from palette + your own).
- `POST /preview` — recompute live: assemble the staged config, select the
  profile for the simulated context, render the overlay via `compose`+`render`
  in **`DynamicMode::ReadOnly`** (never shells out). minijinja errors render
  inline, never a 500.
- `POST /simulator` / `POST /scope` — change the simulated context (language,
  scope, target agent).
- `GET /diff`, `POST /apply`.
- `GET /fs-status` — external-edit poll.
- `POST /trust/allow|deny`.

**Live preview sync:** debounced htmx fragment swaps —
`hx-trigger="keyup changed delay:400ms, change"` + `hx-post="/preview"` →
`#overlay-pane`. No SSE/websocket (no push source; stays daemon-free). The 400ms
debounce + ReadOnly mode means no per-keystroke shelling out. Swaps target
`#overlay-pane`/`#errors`, never the active form → no lost form state. Dynamic
capabilities with no cache entry render nothing under ReadOnly, so the UI
**badges them "runs live — not previewed"** with an explicit per-section "Render
live (executes probe)" opt-in.

## 10. Trust & security

- **Trust:** command-backed capabilities show the existing skip note plus a
  teaching banner; `POST /trust/allow` is explicit + `hx-confirm`, calls
  `trust::allow`, never implicit. Because an Apply changes the `.rosita` bundle
  hash, trust re-locks afterward — studio surfaces that proactively ("config
  changed → command caps re-locked; re-allow?"). Studio itself **never executes**
  a capability `command` (preview is ReadOnly), keeping it off the command-trust
  attack surface.
- **Security:** bind **127.0.0.1 only**; a one-time **bootstrap-token route** is
  the sole tokenless route, which sets an **`HttpOnly; SameSite=Strict` cookie**
  and redirects to a tokenless URL (keeps the token out of history/`Referer`);
  **exact `Origin`/`Referer` checks on every POST/DELETE**, no CORS; a
  **Host-header allowlist** (`127.0.0.1`/`localhost` + port) defeats
  DNS-rebinding; the token/guard wrapper applies to assets and GETs too.

## 11. Concrete changes to existing code

- `context`: `Scope` enum + `Context::scope()` (derived, not hashed).
- `profile`: **retire** the additive machinery (priority ordering, `exclude`,
  `exclusive`, cross-profile union in `compose`). Profiles gain a `targets`
  field (detected language/platform[s], incl. `machine`). Add the pick-one
  **selection** function (match → 0/1/many → none/auto/prompt) and the
  per-project **binding** (read/write the remembered choice). Remove
  `builtin_profiles()` (no shipped profiles).
- `capability`: `builtin_capabilities()` becomes the read-only **palette**
  (compiled-in, never injected into composition).
- `config`: `Config::from_layer_strs(...)` (origin-tagging, in-memory assembly);
  an `include` directive; a namespaced `global_generated_dir()` for machine
  emit-only output; `finalize` no longer auto-injects built-ins.
- `cli`: `Command::Studio(StudioArgs { port, no_open })`; the run-time
  "which profile?" prompt + binding write in `rosita run`/`render`.
- New `src/studio/*` (mod/serve, state, server/router, edit engine, routes,
  maud views, assets). `Cargo.toml` += `tiny_http`, `maud`, `rust-embed`,
  `toml_edit`, `similar`.
- Refactor (no behavior change): extract doctor's leak patterns into a shared
  `lint` module reused by the studio warning.

## 12. Risks / tradeoffs

- **toml_edit layered round-trip** (esp. create + delete preserving comments) →
  proven headless first (Slice 0) with golden round-trip tests.
- **Cross-file non-atomic apply** → backups + write ordering + restore; no
  journal.
- **Trust bypass via the in-memory seam** → origin re-tagging is mandatory and
  tested with a repo `command` cap.
- **Stale overlays after apply** → force-render affected overlays.
- **External-edit clobber** → hash gate; concurrent studio = last-write-wins
  (documented).
- **Pick-one means more profiles** (no composing a base + specifics) → accepted;
  it's the explicit, no-magic tradeoff the maintainer chose.
- **Machine emit-only** is a smaller feature than auto-wire, but removes the
  symlink/committed-file/clean debt; auto-wire deferred.

## 13. Validation / tests

- **Slice-0 headless (edit engine + selection):** comment-preserving round-trip
  on hand-authored TOML; create / edit / delete; proposed text re-parses through
  `RawConfig`; idempotent apply→reload; external-edit hash gate; origin re-tag
  verified with a repo `command` cap (trust); selection logic (0/1/many →
  none/auto/prompt) and binding read/write in `.rosita/local.toml`.
- **HTTP:** drive handler functions directly over a `Session` on a tempdir repo +
  `ROSITA_CONFIG_DIR` (no socket); assert fragment HTML and that `/apply` wrote
  the expected TOML (re-load via `Config` for semantics); assert token /
  Host-header / Origin guards reject forged requests.
- **Smoke:** `rosita studio --no-open --port 0` binds and serves `/`
  (`assert_cmd`, in the style of `tests/cli.rs`).
- **Detection:** each coarse detector ships with a fixture asserting it fires and
  doesn't false-positive.

## 14. Rollback

New command + additive modules + new deps. Revert = drop `src/studio/*`, the
`Command::Studio` arm, and the deps. The selection-model change (pick-one,
retiring additive composition) is the one non-additive change and is the bigger
revert surface — it should land as its own reviewed commit, separable from the
studio UI.

## 15. Sequencing

- **Slice 0 — headless core, no server:** the `toml_edit` edit engine + the
  origin-tagging `Config::from_layer_strs` seam + the pick-one selection &
  binding logic, all proven by tests. De-risks the dangerous parts before any
  HTTP.
- **Slice 1 — read-only HTTP spine + live preview:** `serve()` binds 127.0.0.1,
  bootstrap-token + guards, `GET /` library list, `POST /preview` rendering the
  selected profile's overlay in ReadOnly.
- **Slice 2 — wire write engine to HTTP:** capability editor, profile composer
  (language tie + capability picker), diff/apply, trust banner, leak warning, and
  the "which profile?" prompt surfaced in `rosita run`.

## 16. UI wireframes

**Layout principle:** the core loop is *edit a thing → watch the overlay change*,
so the **live overlay preview is always on screen** (right), you edit in the
center, and navigate/list on the left. A top bar holds the **context simulator**
and the **staged-changes → Apply** control. All updates are htmx fragment swaps.

**Responsive:** pure-CSS breakpoint. Below it the preview pane drops out and a
`⟱ Preview` toggle appears in the top bar; tapping it slides the preview in as a
drawer over the editor. The preview fragment keeps updating via htmx even while
hidden, so it's current the moment it's revealed.

### Shell — Capabilities mode (library + editor + preview)

```
┌ rosita studio ──────────────────────────────────────────── ◷ 127.0.0.1:7777 ┐
│ Simulate ▸ lang[rust ▾]  scope[repo ▾]  agent[claude ▾]   ◍ 2 staged › Review │
├───────────────┬────────────────────────────────────┬─────────────────────────┤
│ Capabilities ▸│  EDIT  rust-conventions             │ Live overlay · claude   │
│ Profiles      │  ──────────────────────────────────  │ # demo — agent context  │
│  YOURS        │  id       rust-conventions           │ _profile: rust—browser_ │
│  ● rust-conv ✎│  desc     Rust conventions           │ ## Rust conventions     │
│  ● terse     ✎│  tags     [stack] [＋]               │ Build with cargo, lint  │
│  PALETTE      │  risk     ◉info ○caution ○dangerous  │ with clippy; prefer ?/  │
│  ○ go-conv  ⎘ │  guidance ┌──────────────────────┐   │ Result over unwrap()…   │
│  ○ python   ⎘ │           │Rust project. Build…  │   │                         │
│  ○ infra-…  ⎘ │           └──────────────────────┘   │ ⟳ updates as you type   │
│ [＋ New cap]  │  lives in ◉repo ○global · ◉public ○private                    │
│               │  ⚠ leak check: clean    [Discard] [Stage change]              │
└───────────────┴────────────────────────────────────┴─────────────────────────┘
```
`●` yours (editable). `○` read-only palette; `⎘` duplicates into your config to
own. "lives in" is the 2×2 layer/visibility picker; the leak check sits under it,
visible and non-blocking.

### Profiles composer (left list + center; preview stays)

```
│ Profiles    ▸ │  EDIT PROFILE   rust — browser       │ Live overlay · claude   │
│  ● rust—kernel│  name     rust — browser             │ ## Rust conventions     │
│  ● rust—brows…│  targets  [rust ▾] [＋]  ← language   │ …                       │
│  ● machine    │           tie (detected lang)        │ ## WASM                 │
│ [＋ New prof] │  capabilities  (need ≥1 to save)     │ Target wasm32; keep     │
│               │   ┌────────────────────────────────┐ │ bindings thin…          │
│               │   │ ⠿ rust-conventions          ✕ │ │                         │
│               │   │ ⠿ wasm-conventions          ✕ │ │ ⟳ live                  │
│               │   └────────────────────────────────┘ │                         │
│               │   [＋ add capability ▾] palette+yours │                         │
│               │  lives in ◉repo ○global · ◉public  [Discard] [Stage change]    │
```
`targets` is the language tie (§4); `⠿` drag handles set render order. Save
disabled until ≥1 capability.

### Review & apply (diff against raw bytes; normalization surfaced)

```
┌ Review staged changes (2) ──────────────────────────────────────────────────┐
│  .rosita/config.toml   repo · public                                +6 −1    │
│  + id = "wasm-conventions"                                                   │
│  + guidance = "Target wasm32; keep bindings thin…"                          │
│    ⚠ rosita will also re-wrap 2 lines it parsed (shown in context)           │
│  .rosita/local.toml    repo · private                               +1       │
│  + [binding] profile = "rust — browser"  ← remembered pick for this project  │
│  ⚠ leak: none.   ⚠ on-disk unchanged since load.                            │
│                                   [Cancel]        [Apply 2 changes]          │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Run-time pick (CLI — `rosita run`, on 2+ matches)

```
$ rosita run claude
rosita › this rust project matches 2 profiles:
   1) rust — kernel    2) rust — browser    3) none (don't apply rosita here)
 ❯ 2
rosita › bound "rust — browser" → remembered in .rosita/local.toml; launching…
```
Single match = no prompt. The binding makes it skip straight to launch next time.

### First launch / onboarding (no profiles or own caps yet)

Welcome state — show that detection worked, explain what a profile is for, and
explain *why* the overlay is empty (so it reads "not set up," not "broken"):

```
│ Profiles  ▸   │  👋 Welcome to rosita studio          │ Live overlay · claude  │
│  (none yet)   │  rosita detected this project:        │   (empty)              │
│ [＋ New prof] │    ● rust · branch main · repo         │  No profile applies    │
│  PALETTE      │  A *profile* decides what guidance    │  here yet, so the      │
│  ○ rust-conv  │  your agent gets here. You have none  │  overlay is empty.     │
│  ○ terse  …   │  yet — let's make your first.          │  Create one to fill ↓  │
│               │  ┌──────────────────────────────────┐ │                        │
│               │  │ ▶ Quick start — a "rust" profile, │ │                        │
│               │  │   pre-filled with rust palette    │ │                        │
│               │  │   caps. You edit & own it.         │ │                        │
│               │  │ ▷ Start from scratch               │ │                        │
│               │  └──────────────────────────────────┘ │                        │
```

Quick start → composer pre-filled (name `rust`, `targets [rust]` from detection,
capabilities suggested from the palette, **labeled "suggested, edit freely"** —
a head start, not a magic default), and the preview lights up. A language profile
**defaults to `global`** with a first-use explainer (`ⓘ global = reuse in every
rust repo; repo = just this one`). On apply, since it's the only rust profile, it
auto-binds → success state: `✓ Created "rust" (global) — applied here. It'll
auto-apply in any rust repo.  [Open it] [Create another]`.

Variants (same skeleton): **machine first-launch** (detected `● machine · no
repo`; Quick start offers a provider-backed "machine" profile + the safety cap;
reminds that machine scope is emit-only with the `@import` line to copy);
**has profiles, none target here** (`You have 4 profiles, none target rust.
[Create one] · [Pick an existing one] · [None]`).

### Dynamic capability + trust

```
EDIT  current-ctx  ·  kind ○ static  ◉ dynamic
source ◉ provider[docker ▾]   ○ command[ kubectl config current-context ]
cache  [30s]    guidance: "On {{ provider.output }}"
lives in ◉ repo ○ global · ◉ public ○ private
─ trust ───────────────────────────────────────────────
provider source → always trusted (no allow).
command in a repo → ⚠ won't run until you trust this repo:
   this repo: untrusted   [Allow this repo…]  (explicit, confirmed)
   preview shows the skip note until then.
```

### Machine scope (no repo; providers surfaced; emit-only delivery)

```
Simulate ▸ scope[machine]  agent[claude ▾]            delivery: emit-only ⓘ
─ Live overlay · claude (machine) ───────────────────────────
# machine — agent context · host ellerys-mba · macos/arm64
## Careful operator   live box, not a sandbox; confirm destructive ops…
## Environment (providers)  toolchain git/cargo/node/docker · 7 containers
─────────────────────────────────────────────────────────────
ⓘ emit-only — add to your global agent config once:
   @import ~/.config/rosita/generated/claude.md        [Copy]
```

### Context simulator (preview any context; drives selection)

```
Simulate ▸ lang[rust ▾] scope[repo ▾] branch[main] path[/] agent[claude ▾]
  → resolved profile: rust — browser (bound)     [change binding] [unbind]
  the preview + which-profile reflect this simulated context, not just real cwd
```

## 17. Next

Design and wireframes are complete. The next step is **implementation, starting
at Slice 0** (§15): the headless `toml_edit` edit engine + the origin-tagging
`Config::from_layer_strs` seam + the pick-one selection & binding logic, all
proven by tests, before any HTTP. Carry the §8/§10 must-dos (origin re-tagging
for trust; diff-against-raw-bytes; comment-preserving round-trip tests).
