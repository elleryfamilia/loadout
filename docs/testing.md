# Testing rosita

Two levels: the **automated suite** (fast, zero side effects) and a **hands-on
walkthrough** that drives the real CLI in a sandbox. The output below is real
(trimmed; machine-specific values shown as placeholders).

## Level 1 — Automated tests (~30s, no side effects)

```bash
git clone https://github.com/elleryfamilia/rosita
cd rosita
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

**Why a sandbox:** fragments and profiles are **global-only**, so rosita
writes them into your global config dir (`~/.config/rosita`); in a repo it writes
only the gitignored overlay, the binding, and `.gitignore`. To kick the tires
without touching your real config or any real project, use a throwaway git repo
plus an isolated config dir via `ROSITA_CONFIG_DIR`.

### Setup

```bash
# Install the binary onto PATH (tests the repo end-to-end):
cargo install --git https://github.com/elleryfamilia/rosita
#   …or, from a local clone:  cargo install --path .

# Isolated global config (so nothing real is touched) + a throwaway rust repo:
export ROSITA_CONFIG_DIR="$(mktemp -d)"          # isolated global library
SB="$(mktemp -d)"; mkdir -p "$SB/src" "$SB/infra/db"
printf '[package]\nname="demo"\nversion="0.1.0"\n' > "$SB/Cargo.toml"
printf 'fn main(){}\n' > "$SB/src/main.rs"
git -C "$SB" init -q
cd "$SB"
```

### 1. See what it detects

```bash
rosita detect
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
`rosita detect --json` gives the machine-readable form. The coarse **stack**
(`rust`) is what a profile's `targets` match against.

### 2. Author your library (global-only)

Fragments and profiles live in the **global** config, not the repo. (Normally
you'd do this visually with `rosita studio`; here we write the file directly.)

```bash
cat > "$ROSITA_CONFIG_DIR/config.toml" <<'TOML'
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

[[profiles]]
name = "rust"
targets = ["rust"]
fragments = ["rust-conventions", "terse-comms", "infra-caution", "host-info"]
TOML
```

### 3. Explain the selection (dry — writes nothing)

```bash
rosita explain
```
```
Detected targets: [rust]
Profile selection → rust

Active fragments
  • rust-conventions   fragment 'rust-conventions' via profile 'rust'
  • terse-comms        fragment 'terse-comms' via profile 'rust'
  • host-info          fragment 'host-info' via profile 'rust'

Profiles considered
  → rust           targets [rust] match
```
**One profile per context.** `rust` is the only profile whose `targets` match, so
it's auto-selected (no prompt). `infra-caution` is absent here — its own `when`
self-gate (`path starts_with "infra/"`) doesn't match the repo root.

### 4. Within-profile gating in a subdirectory

```bash
rosita --cwd "$SB/infra/db" explain
```
```
Active fragments
  • rust-conventions
  • terse-comms
  • infra-caution
  • host-info
```
Same profile, but now `infra-caution` contributes — its `when` matches the
`infra/` path. Gating happens **inside** the chosen profile (per-fragment
`when`), not by composing extra profiles. (`--cwd` runs as if invoked there.)

### 5. Render the overlay and inspect it

```bash
rosita refresh --agent claude
cat .rosita/generated/claude.md
```
```
claude  ·  profile rust  ·  sha256:…
  created       .rosita/generated/claude.md
  created       CLAUDE.local.md        (a gitignored @import of the overlay)
  created       .gitignore
```
The overlay carries a self-healing banner, the detected context, then the
profile's composed guidance — including the **live** `host-info` output:
```
## Profile guidance — rust
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
rosita fragments          # ● = active here, · = available but inactive
rosita profiles              # marks which match, and the selected one
```
```
Fragments (4 in library, 3 active for this context)
  ● rust-conventions — rust-conventions
  ● terse-comms — terse-comms
  · infra-caution — infra-caution
  ● host-info — host-info  (provider: host)

Profiles (1 configured; selected: rust)
  → rust             targets [rust]
        fragments: rust-conventions, terse-comms, infra-caution, host-info
```

### 7. Global-only enforcement

Fragments and profiles declared in a **repo** are ignored — `rosita doctor`
flags the mistake instead of silently honoring it:

```bash
mkdir -p .rosita
printf '[[fragments]]\nid="repo-cap"\nguidance="x"\n' > .rosita/config.toml
rosita doctor | grep "global-only"
# → ⚠ .rosita/config.toml declares fragments — these are global-only and are
#   ignored here; move them to ~/.config/rosita/config.toml
rm .rosita/config.toml
```

### 8. Dynamic fragments & providers

`host-info` above is a built-in **provider** — always safe, no trust needed.
Providers (`host`/`toolchain`/`ai-tools`/`tailnet`/`docker`) probe the live
environment; their (redacted) output lands only in the gitignored overlay and is
kept out of the context hash. A bare `detect` never probes; `rosita detect
--probes` opts in:

```bash
rosita detect --probes        # host/toolchain/ai-tools/(tailnet/docker if present)
```

The generic escape hatch is a fragment `command` (any shell command, redacted
stdout embedded). It runs at render unless you set `allow_exec = false` (the
per-fragment off-switch). There's no repo-trust prompt: fragments are
global-only (§7), so a `command` is always one you authored globally.

### 9. Freshness lifecycle

```bash
rosita refresh    # re-render initialized overlays
rosita doctor     # → ✓ claude: up to date  + config/agent/template health
rosita run claude --dry-run -- chat --model sonnet
# → "dry run — no files will be written" + "would update …"  (no launch)
rosita clean      # removes the overlay + CLAUDE.local.md; never touches AGENTS.md
```
A **static** overlay is idempotent — re-rendering an unchanged context is a
no-op. An overlay with a **dynamic** fragment (like `host-info`) re-probes, so
`refresh` rewrites it. (`rosita run claude` without `--dry-run` launches the
`claude` CLI if installed, passing your args through.)

### Teardown

```bash
cd ~ && rm -rf "$SB" "$ROSITA_CONFIG_DIR" && unset ROSITA_CONFIG_DIR
```
Because the global library was isolated under `ROSITA_CONFIG_DIR`, nothing
touched your real `~/.config/rosita`, and the only repo affected was the
throwaway one.

## What "passing" looks like

- **Level 1:** green tests / clippy / fmt (263 tests).
- **Level 2:** `rust` auto-selected as the **one** profile; `infra-caution`
  gated in only under `infra/`; the dynamic `host-info` output rendered into the
  overlay; a repo-declared fragment flagged as **ignored** (global-only); and
  `clean` removing only the generated artifacts.
