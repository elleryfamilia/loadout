# Testing loadout

Two levels: the **automated suite** (fast, zero side effects) and a **hands-on
walkthrough** that drives the real CLI in a sandbox. The output below is real
(trimmed; machine-specific values shown as placeholders).

## Level 1 — Automated tests (~30s, no side effects)

```bash
git clone https://github.com/elleryfamilia/loadout
cd loadout
cargo test                      # → 263 tests passing
cargo clippy --all-targets      # → no warnings
cargo fmt --check               # → clean
```

`tests/cli.rs` drives the real binary against temp repos and `tests/studio.rs`
drives the studio HTTP handlers; the lib tests cover detection, **pick-one
selection + the per-project binding**, the comment-preserving **studio
`toml_edit` write engine** (stage/diff/apply), fragment-params merge, the
providers' pure parsers, the cache TTL, rendering, atomic writes, and
redaction. All three green ⇒ the build is sound.

## Level 2 — Hands-on walkthrough (sandboxed)

**Why a sandbox:** fragments and loadouts are **global-only**, so loadout
writes them into your global config dir (`~/.config/loadout`); in a repo it writes
only the gitignored overlay, the binding, and `.gitignore`. To kick the tires
without touching your real config or any real project, use a throwaway git repo
plus an isolated config dir via `LOADOUT_CONFIG_DIR`.

### Setup

```bash
# Install the binary onto PATH (tests the repo end-to-end):
cargo install --git https://github.com/elleryfamilia/loadout
#   …or, from a local clone:  cargo install --path .

# Isolated global config (so nothing real is touched) + a throwaway rust repo:
export LOADOUT_CONFIG_DIR="$(mktemp -d)"          # isolated global library
SB="$(mktemp -d)"; mkdir -p "$SB/src" "$SB/infra/db"
printf '[package]\nname="demo"\nversion="0.1.0"\n' > "$SB/Cargo.toml"
printf 'fn main(){}\n' > "$SB/src/main.rs"
git -C "$SB" init -q
cd "$SB"
```

### 1. See what it detects

```bash
load detect
```
```
Context
  cwd        : /tmp/demo
  name       : demo
  git        : branch main · 0 remote(s)
  stacks     : rust
  languages  : Rust
  pkg mgrs   : cargo
  commands   :  build cargo build   test cargo test   lint cargo clippy --all-targets
  system     : <os> / <arch> · host <hostname> · user <you>
```
`load detect --json` gives the machine-readable form. The coarse **stack**
(`rust`) is what a loadout's `targets` match against.

### 2. Author your library (global-only)

Fragments and loadouts live in the **global** config, not the repo. (Normally
you'd do this visually with `load studio`; here we write the file directly.)

```bash
cat > "$LOADOUT_CONFIG_DIR/config.toml" <<'TOML'
[[fragments]]
id = "rust-conventions"
guidance = "Build with cargo, lint with clippy; prefer ?/Result over unwrap()."

[[fragments]]
id = "terse-comms"
guidance = "Be terse: lead with the result and what changed."

[[fragments]]                          # self-gates: only contributes under infra/
id = "infra-caution"
when = [{ field = "path", op = "starts_with", value = "infra/" }]
guidance = "Infrastructure path — prefer plans; confirm before touching shared state."

[[fragments]]                          # dynamic: live output embedded at render
id = "host-info"
provider = "host"
guidance = "Running on {{ provider.output }}"

[[loadouts]]
name = "rust"
targets = ["rust"]
fragments = ["rust-conventions", "terse-comms", "infra-caution", "host-info"]
TOML
```

### 3. Explain the selection (dry — writes nothing)

```bash
load explain
```
```
Detected targets: [rust]
Loadout selection → rust

Active fragments
  • rust-conventions   fragment 'rust-conventions' via loadout 'rust'
  • terse-comms        fragment 'terse-comms' via loadout 'rust'
  • host-info          fragment 'host-info' via loadout 'rust'

Loadouts considered
  → rust           targets [rust] match
```
**One loadout per context.** `rust` is the only loadout whose `targets` match, so
it's auto-selected (no prompt). `infra-caution` is absent here — its own `when`
self-gate (`path starts_with "infra/"`) doesn't match the repo root.

### 4. Within-loadout gating in a subdirectory

```bash
loadout --cwd "$SB/infra/db" explain
```
```
Active fragments
  • rust-conventions
  • terse-comms
  • infra-caution
  • host-info
```
Same loadout, but now `infra-caution` contributes — its `when` matches the
`infra/` path. Gating happens **inside** the chosen loadout (per-fragment
`when`), not by composing extra loadouts. (`--cwd` runs as if invoked there.)

### 5. Render the overlay and inspect it

```bash
load refresh --agent claude
cat .loadout/generated/claude.md
```
```
claude  ·  loadout rust  ·  sha256:…
  created       .loadout/generated/claude.md
  created       CLAUDE.local.md        (a gitignored @import of the overlay)
  created       .gitignore
```
The overlay carries a self-healing banner, the detected context, then the
loadout's composed guidance — including the **live** `host-info` output:
```
## Loadout guidance — rust
### rust-conventions
Build with cargo, lint with clippy; prefer ?/Result over unwrap().
### terse-comms
Be terse: lead with the result and what changed.
### host-info
Running on <hostname> — <os>/<arch>, user <you>
```
Committed files like `AGENTS.md` are never touched.

### 6. Introspect the resolved sets

```bash
load list fragments     # ● = active here, · = available but inactive
load list               # loadouts: marks which match, and the selected one
```
```
Fragments (4 in library, 3 active for this context)
  ● rust-conventions — rust-conventions
  ● terse-comms — terse-comms
  · infra-caution — infra-caution
  ● host-info — host-info  (provider: host)

Loadouts (1 configured; selected: rust)
  → rust             targets [rust]
        fragments: rust-conventions, terse-comms, infra-caution, host-info
```

### 7. Global-only enforcement

Fragments and loadouts declared in a **repo** are ignored — `load doctor`
flags the mistake instead of silently honoring it:

```bash
mkdir -p .loadout
printf '[[fragments]]\nid="repo-cap"\nguidance="x"\n' > .loadout/config.toml
load doctor | grep "global-only"
# → ⚠ .loadout/config.toml declares fragments — these are global-only and are
#   ignored here; move them to ~/.config/loadout/config.toml
rm .loadout/config.toml
```

### 8. Dynamic fragments & providers

`host-info` above is a built-in **provider** — always safe, no trust needed.
Providers (`host`/`toolchain`/`ai-tools`/`tailnet`/`docker`) probe the live
environment; their (redacted) output lands only in the gitignored overlay and is
kept out of the context hash. A bare `detect` never probes; `load detect
--probes` opts in:

```bash
load detect --probes        # host/toolchain/ai-tools/(tailnet/docker if present)
```

The generic escape hatch is a fragment `command` (any shell command, redacted
stdout embedded). It runs at render unless you set `allow_exec = false` (the
per-fragment off-switch). There's no repo-trust prompt: fragments are
global-only (§7), so a `command` is always one you authored globally.

### 9. Freshness lifecycle

```bash
load refresh    # re-render initialized overlays
load doctor     # → ✓ claude: up to date  + config/agent/template health
load run claude --dry-run -- chat --model sonnet
# → "dry run — no files will be written" + "would update …"  (no launch)
load clean      # removes the overlay + CLAUDE.local.md; never touches AGENTS.md
```
A **static** overlay is idempotent — re-rendering an unchanged context is a
no-op. An overlay with a **dynamic** fragment (like `host-info`) re-probes, so
`refresh` rewrites it. (`load run claude` without `--dry-run` launches the
`claude` CLI if installed, passing your args through.)

### Teardown

```bash
cd ~ && rm -rf "$SB" "$LOADOUT_CONFIG_DIR" && unset LOADOUT_CONFIG_DIR
```
Because the global library was isolated under `LOADOUT_CONFIG_DIR`, nothing
touched your real `~/.config/loadout`, and the only repo affected was the
throwaway one.

## Level 3 — Browser smoke test for the plan viewer

`load plan render`'s output page (`plan.html`) carries its own client-side JS
(`src/plan/assets/plan.js`) for the dependency graph, comment anchoring, and
the "Copy feedback" button. `tests/browser_smoke.rs` checks that JS actually
runs correctly in a real browser, not just that the HTML is well-formed.

The page's JS has a `#selftest` mode: opening `plan.html#selftest` (instead of
the plain path) runs a small self-check of the pure-core logic (comment
anchoring, feedback-document assembly) inside the page itself, then appends a
result marker to the DOM: `<pre id="selftest-result">LOADOUT_SELFTEST_PASS…</pre>`
on success. The test drives headless Chrome with `--dump-dom` against a
rendered fixture plan and asserts that exact marker element is present — it
anchors on the DOM element, not the bare string `LOADOUT_SELFTEST_PASS` (that
literal also appears in `plan.js`'s own source, which `--dump-dom` serializes
into the page regardless of whether the selftest ran).

The test is `#[ignore]`d by default (it needs a real Chrome/Chromium binary,
which isn't guaranteed to be present). **CI** runs it explicitly on
`ubuntu-latest` in a dedicated `browser-smoke` job:

```bash
cargo test --test browser_smoke --locked -- --ignored
```

with `CHROME_BIN=google-chrome` in the environment (Ubuntu CI images ship
Chrome under that name).

**Locally**, run the same command. The test auto-detects a browser in this
order: the `CHROME_BIN` env var, then `google-chrome` / `chromium` /
`chromium-browser` on `PATH`, then the common macOS app bundle paths
(`/Applications/Google Chrome.app/…`, `/Applications/Chromium.app/…`). If none
of those resolve, the test panics with "no Chrome found; set CHROME_BIN" —
set `CHROME_BIN` to your browser's binary path and re-run.

## What "passing" looks like

- **Level 1:** green tests / clippy / fmt.
- **Level 2:** `rust` auto-selected as the **one** loadout; `infra-caution`
  gated in only under `infra/`; the dynamic `host-info` output rendered into the
  overlay; a repo-declared fragment flagged as **ignored** (global-only); and
  `clean` removing only the generated artifacts.
- **Level 3:** the plan viewer's `#selftest` marker
  (`LOADOUT_SELFTEST_PASS…`) present in the DOM after headless Chrome loads a
  rendered `plan.html#selftest`.
