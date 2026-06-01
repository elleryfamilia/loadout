# Testing rosita

Two levels: the **automated suite** (fast, zero side effects) and a **hands-on
walkthrough** that drives the real CLI in a sandbox. The expected output below is
real (trimmed; machine-specific values shown as placeholders).

## Level 1 — Automated tests (~30s, no side effects)

```bash
git clone https://github.com/elleryfamilia/rosita
cd rosita
cargo test                      # → 126 tests passing (94 lib + 32 e2e)
cargo clippy --all-targets      # → no warnings
cargo fmt --check               # → clean
```

`tests/cli.rs` drives the real binary against temp repos; the lib tests cover
composition, capability-params merge, the providers' pure parsers, the cache TTL,
and trust. All three green ⇒ the build is sound.

## Level 2 — Hands-on walkthrough (sandboxed)

**Why a sandbox:** rosita writes into the repo it operates on (`.rosita/`,
`CLAUDE.local.md`, `.gitignore`) and into a global config dir (the trust store
lives there). To kick the tires without touching your real `~/.config/rosita` or
any real project, use a throwaway git repo plus an isolated config dir via
`ROSITA_CONFIG_DIR`.

### Setup

```bash
# Install the published binary onto PATH (tests the repo end-to-end):
cargo install --git https://github.com/elleryfamilia/rosita
#   …or, from a local clone:  cargo install --path .
#   …or just build:           cargo build --release   (→ ./target/release/rosita)

# Throwaway rust repo + isolated global config (so nothing real is touched):
export ROSITA_CONFIG_DIR="$(mktemp -d)"      # global layer + trust store sandbox
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
  stacks     : rust
  languages  : Rust
  pkg mgrs   : cargo
  commands   :  build cargo build   test cargo test   lint cargo clippy --all-targets
  git        : branch main · 0 remote(s)
```
`rosita detect --json` gives the machine-readable form.

### 2. Scaffold config

```bash
rosita init
```
Creates `.rosita/config.toml` (public), `.rosita/local.toml` (private,
gitignored), a template, and a `.gitignore`.

### 3. Explain the composition (dry — writes nothing)

```bash
rosita explain
```
```
Profile selection → rust
  composing: rust + default
Active capabilities
  • rust-conventions   … via profile 'rust' (Stack equals "rust")
  • baseline           … via profile 'default' (fallback profile (no rules))
```
This is the heart of the design: **both** `rust` and the always-on `default`
match, so their capabilities **layer** instead of one winning.

### 4. Render the overlay and inspect it

```bash
rosita render --agent claude
cat .rosita/generated/claude.md
```
You'll see a self-healing banner, the detected context, then composed guidance:
```
## Profile guidance — rust
### Rust conventions
Rust project. Build with cargo … clippy … Prefer `?`/`Result` over `unwrap()` …
### Baseline
Follow the repository's existing conventions and keep changes minimal …
```
It also created `CLAUDE.local.md` (a gitignored `@import` of the overlay) and
added `.gitignore` entries. Committed files like `AGENTS.md` are never touched.

### 5. Introspect the resolved sets

```bash
rosita capabilities          # ● = active here, · = available but inactive
rosita capabilities show rust-conventions
rosita profiles              # → marks profiles matching this context
rosita agents                # delivery mode per agent (import / override / emit-only)
```

### 6. See additive layering in a subdirectory

```bash
rosita --cwd "$SB/infra/db" explain
```
Now **three** profiles compose — `infra` (path `infra/`) + `rust` + `default` —
and the overlay gains the ⚠️ caution capability. (`--cwd` runs as if invoked
from there.)

### 7. Native environment probes (opt-in)

```bash
rosita detect --probes
```
```
Probes
  host       : <hostname> — <os>/<arch>, user <you>
  toolchain  : installed: git …, cargo …, node …, docker …
  ai-tools   : claude …, codex …
  docker     : N running container(s) …   (if docker is up)
```
A bare `detect` never spawns subprocesses; `--probes` is opt-in.

### 8. Dynamic capabilities + the trust gate

The security-relevant part. Add a provider-backed cap and a command-backed cap:

```bash
cat >> .rosita/config.toml <<'TOML'

[[capabilities]]
id = "machine"
provider = "host"
guidance = "Running on {{ provider.output }}"

[[capabilities]]
id = "greet"
command = "echo hello-from-rosita"

[[profiles]]
name = "dyn"
priority = 90
capabilities = ["machine", "greet"]
TOML

rosita render --agent claude
grep -E "Running on|hello-from|skipped" .rosita/generated/claude.md
```
**Before trusting**, the provider runs but the repo-authored command is refused:
```
Running on <hostname> — <os>/<arch> …
> [rosita] skipped untrusted command — run `rosita allow` to enable
```
Now trust the repo and re-render:
```bash
rosita trust          # → status: untrusted
rosita allow          # → trusted …
rosita render --agent claude
grep hello-from .rosita/generated/claude.md     # → hello-from-rosita
```
Then prove trust **re-locks** when the config changes (so a command can't be
slipped in after approval):
```bash
echo '# any edit' >> .rosita/config.toml
rosita render --agent claude
grep skipped .rosita/generated/claude.md        # → skipped again
rosita deny           # revoke
```

### 9. Public/private split + leak lint

```bash
printf '\n[host_classes]\nwork = ["*.corp.example.com"]\n' >> .rosita/config.toml
rosita doctor | grep "looks private"
# → ⚠ .rosita/config.toml: "*.corp.example.com" looks private — move to local.toml
```
The fix it nudges you toward: put machine-specific values in `.rosita/local.toml`
(gitignored) instead of the shareable `config.toml`.

### 10. Freshness lifecycle

```bash
rosita refresh        # static overlay unchanged → reports "unchanged" (idempotent)
rosita doctor         # → "claude: up to date", config/agent/template health
rosita run claude --dry-run -- chat --model sonnet
# → "would exec: claude --append-system-prompt … chat --model sonnet"  (no launch)
rosita clean          # removes the generated overlay + CLAUDE.local.md; never touches AGENTS.md
```
(`rosita run claude` with no `--dry-run` actually launches the `claude` CLI if
it's installed, passing your args through.)

### Teardown

```bash
cd ~ && rm -rf "$SB" "$ROSITA_CONFIG_DIR" && unset ROSITA_CONFIG_DIR
```
Because the global layer and trust store were isolated under `ROSITA_CONFIG_DIR`,
nothing touched your real `~/.config/rosita`, and the only repo affected was the
throwaway one.

## What "passing" looks like

- **Level 1:** green tests / clippy / fmt (126 tests).
- **Level 2:** additive composition in `explain`; both capabilities rendered in
  the overlay; the command **refused before `allow`** and **running after**;
  trust **re-locking** on a config edit; and the leak lint firing on the domain.
