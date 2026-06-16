//! Fragments — reusable, self-contained units of guidance.
//!
//! A **fragment** is one atom of agent guidance ("Rust conventions", "be
//! conservative with infrastructure", "be terse"). Fragments are authored
//! once, kept in a library (built-ins plus `[[fragments]]` config entries), and
//! **composed by profiles** (see [`crate::profile::compose`]). This is the reuse seam: many
//! profiles can pull the same fragment instead of repeating inline guidance.
//!
//! A fragment can self-gate with `when` rules, declare `requires`
//! dependencies, carry a `category`, be restricted to specific `agents`, and
//! expose free-form `params` to its guidance template.
//!
//! Phase 1 ships only **static** fragments (fixed, templated `guidance`).
//! Dynamic fragments (provider/command-backed live output) arrive in a later
//! phase; the struct is laid out so those fields can be added without churn.

use serde::{Deserialize, Serialize};

use crate::profile::Rule;

/// Which config layer defined a fragment. Drives global-only enforcement:
/// fragments are honored only from built-in/global/global-local layers (a
/// repo layer that declares them is ignored, and `doctor` flags it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    /// Shipped with rosita.
    #[default]
    BuiltIn,
    /// Global `config.toml`.
    Global,
    /// Global `local.toml`.
    GlobalLocal,
    /// Repo `.rosita/config.toml`.
    Repo,
    /// Repo `.rosita/local.toml`.
    RepoLocal,
}

impl Layer {
    /// Whether `[[fragments]]` defined in this layer are honored. Fragments
    /// are a **global** concept (the library any profile can compose): built-in,
    /// the global `config.toml`, or the global `local.toml`. A repo layer that
    /// declares them is ignored (and `doctor` flags it).
    pub fn contributes_fragments(self) -> bool {
        matches!(self, Layer::BuiltIn | Layer::Global | Layer::GlobalLocal)
    }

    /// Whether `[[profiles]]` defined in this layer are honored. Profiles are
    /// **public-global only**: authored once in the global `config.toml`, shared
    /// across repos. Not the private global `local.toml`, never a repo layer.
    pub fn contributes_profiles(self) -> bool {
        matches!(self, Layer::BuiltIn | Layer::Global)
    }
}

/// A reusable unit of guidance composed by profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    /// Stable id referenced by `profiles[].fragments` and `requires`.
    pub id: String,
    /// Human-readable summary; doubles as the rendered section heading.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional human-friendly category that groups this fragment in studio's
    /// tree (e.g. `Operating Style`, `Local Environment`). `skip_serializing_if`
    /// keeps the freshness fingerprint of an uncategorized fragment unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Self-gate: all clauses must match the context. Empty = always applies
    /// (the composing profile's own rules still gate when it is pulled in).
    #[serde(default)]
    pub when: Vec<Rule>,
    /// Other fragment ids this one pulls in (resolved before it, deduped).
    #[serde(default)]
    pub requires: Vec<String>,
    /// Free-form parameters exposed to the guidance template as `params`.
    #[serde(default = "empty_params")]
    pub params: toml::Value,
    /// The guidance markdown, itself rendered as a minijinja template. For a
    /// dynamic fragment, `provider.output`/`provider.data` are in scope; an
    /// empty `guidance` falls back to the raw provider/command output.
    #[serde(default)]
    pub guidance: String,
    /// Optional agent restriction (ids); empty = all agents. Applied at render
    /// time because the active agent varies per render.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Dynamic: a built-in provider id (`host`/`docker`/…) whose live output is
    /// embedded. Built-in probes are safe (no arbitrary command execution).
    #[serde(default)]
    pub provider: Option<String>,
    /// Dynamic: a shell command (or script body) whose (redacted) stdout is
    /// embedded. Runs at render unless `allow_exec` is `false`.
    #[serde(default)]
    pub command: Option<String>,
    /// Interpreter for `command` when it is a script body: `bash`, `sh`, or
    /// `python`. `None` runs `command` as a plain `sh -c` line (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_lang: Option<String>,
    /// Whether a `command`-backed fragment is allowed to execute. Defaults to
    /// `true` (existing configs keep running); set `false` to disable a script
    /// without deleting it — the off-switch for command execution. Only
    /// serialized when `false`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_exec: bool,
    /// Cache TTL for dynamic output (e.g. `60s`, `5m`); default 60s.
    #[serde(default)]
    pub cache: Option<String>,
    /// Which config layer defined this fragment (set during config load, not
    /// deserialized). Drives global-only enforcement.
    #[serde(skip)]
    pub origin: Layer,
}

/// Default `params`: an empty TOML table (so `{{ params.x }}` is just empty).
fn empty_params() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

/// Serde default for [`Fragment::allow_exec`] (execution on unless disabled).
fn default_true() -> bool {
    true
}

/// `skip_serializing_if` for [`Fragment::allow_exec`] — only persist the
/// off-switch (`allow_exec = false`), never the default.
fn is_true(b: &bool) -> bool {
    *b
}

impl Fragment {
    /// The heading title for this fragment: its description, else its id.
    pub fn title(&self) -> &str {
        self.description.as_deref().unwrap_or(&self.id)
    }

    /// Whether this fragment resolves live output (provider- or command-backed).
    pub fn is_dynamic(&self) -> bool {
        self.provider.is_some() || self.command.is_some()
    }

    /// Whether this fragment applies to `agent` given its `agents` restriction.
    pub fn applies_to_agent(&self, agent: &str) -> bool {
        self.agents.is_empty() || self.agents.iter().any(|a| a == agent)
    }
}

/// The shipped fragment **palette**: a read-only catalog you *pick from* when
/// composing a profile. Palette items are **never auto-composed and never
/// written into your config** — to use or customize one you duplicate it into a
/// config layer and own the copy (studio's `DuplicatePaletteItem`). Composition
/// resolves a profile's fragment refs against your *own* library only, so a
/// profile that names a palette id you haven't duplicated renders nothing for it.
pub fn palette() -> Vec<Fragment> {
    // Build a static (markdown) palette fragment: a friendly category +
    // templated guidance. The studio glyph is derived from content type.
    fn frag(id: &str, description: &str, category: &str, guidance: &str) -> Fragment {
        Fragment {
            id: id.to_string(),
            description: Some(description.to_string()),
            category: Some(category.to_string()),
            when: Vec::new(),
            requires: Vec::new(),
            params: empty_params(),
            guidance: guidance.to_string(),
            agents: Vec::new(),
            provider: None,
            command: None,
            script_lang: None,
            allow_exec: true,
            cache: None,
            origin: Layer::default(),
        }
    }

    // Build a dynamic (script-backed) palette fragment: an owned bash script
    // whose redacted stdout is embedded at render time. Guidance is empty so it
    // falls back to the raw output and studio treats it as a "pure script"
    // (opened for view/edit, with a "runs at render" placeholder in preview).
    // Every script below is strictly read-only — it probes, never mutates, and
    // degrades to nothing (exit 0, empty output) when a tool or daemon is absent.
    fn script_frag(id: &str, description: &str, cache: &str, command: &str) -> Fragment {
        Fragment {
            command: Some(command.trim_start_matches('\n').to_string()),
            script_lang: Some("bash".to_string()),
            cache: Some(cache.to_string()),
            ..frag(id, description, "Local Environment", "")
        }
    }

    vec![
        // --- baseline awareness --------------------------------------------
        frag(
            "baseline",
            "Follow repo conventions",
            "Operating Style",
            "Follow the repository's existing conventions and keep changes minimal, \
             focused, and well-tested. Match the surrounding code's style and naming \
             rather than imposing your own.",
        ),
        // --- communication -------------------------------------------------
        frag(
            "terse-comms",
            "Terse communication",
            "Operating Style",
            "Be terse: lead with the result and what changed; skip preamble. For \
             non-trivial decisions, briefly note the reasoning, the tradeoffs, and the \
             alternatives considered.",
        ),
        // --- stack conventions (one per detected language/platform) --------
        frag(
            "rust-conventions",
            "Rust conventions",
            "Stack Conventions",
            "Rust project. Build with cargo, format with rustfmt, lint with clippy \
             (`cargo clippy --all-targets`). Prefer `?`/`Result` over `unwrap()` or \
             `panic!` in non-test code.",
        ),
        frag(
            "node-conventions",
            "Node.js conventions",
            "Stack Conventions",
            "Node.js project. Use pnpm for scripts and dependencies, and prefer \
             TypeScript over plain JavaScript. Keep the type-checker and linter clean \
             before committing.",
        ),
        frag(
            "nextjs-conventions",
            "Next.js conventions",
            "Stack Conventions",
            "Next.js app. Respect the existing app/pages router convention and keep \
             server/client component boundaries explicit. Use pnpm for scripts and \
             dependencies.",
        ),
        frag(
            "go-conventions",
            "Go conventions",
            "Stack Conventions",
            "Go project. Use the standard toolchain (`go build`, `go test`, `go vet`, \
             `gofmt`); add golangci-lint for stricter checks. Handle errors explicitly \
             — don't silently discard them.",
        ),
        frag(
            "python-conventions",
            "Python conventions",
            "Stack Conventions",
            "Python project. Use uv for environments and dependencies, ruff for \
             linting and formatting, and pytest for tests.",
        ),
        // --- workflow ------------------------------------------------------
        frag(
            "conventional-commits",
            "Conventional commits",
            "Dev Workflow",
            "Use Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, …). \
             Imperative subject ≤72 chars; the body explains *why* when it is \
             non-obvious.",
        ),
        frag(
            "commit-checkpoints",
            "Commit at checkpoints",
            "Dev Workflow",
            "Commit at logical checkpoints with clear, descriptive messages rather \
             than one giant commit at the end — don't wait to be told.",
        ),
        frag(
            "plan-nontrivial",
            "Plan non-trivial work",
            "Dev Workflow",
            "For non-trivial work, sketch a short plan before implementing: the \
             objective, the approach, and the risks. Skip the ceremony for typos and \
             obvious one-line fixes.",
        ),
        frag(
            "experimental-iteration",
            "Spike fast on a throwaway branch",
            "Dev Workflow",
            "Experimental branch — optimize for iteration speed. Throwaway spikes are \
             fine; keep changes scoped to this branch and don't wire them into shared \
             modules yet.",
        ),
        frag(
            "work-summary",
            "Summarize work and next steps",
            "Dev Workflow",
            "When you finish a unit of work, close with two short, scannable bullet \
             lists: **Done** — one bullet per change, in plain language (what changed \
             and where, not how it was implemented); and **Next steps** — one concrete \
             action per bullet for what remains, or an explicit note that nothing does. \
             Keep both tight: no preamble, no restating the request.",
        ),
        // --- quality -------------------------------------------------------
        frag(
            "validate-before-done",
            "Build, test, and lint before done",
            "Quality",
            "Before declaring work done, run the build, the tests, and the linter, and \
             report the results honestly. If something failed or was skipped, say so \
             plainly — don't claim success you didn't verify.",
        ),
        frag(
            "testing-discipline",
            "Cover changes with tests",
            "Quality",
            "Add or update tests to match the change: unit or integration tests for \
             logic, end-to-end tests for user-facing behavior. If a real harness is \
             impractical, say so instead of skipping silently.",
        ),
        // --- safety --------------------------------------------------------
        frag(
            "branch-discipline",
            "Never commit to main",
            "Safety",
            "Never commit or push directly to the main/master branch — always work on \
             a branch and open a pull request instead of pushing to shared branches.",
        ),
        frag(
            "ask-before-risky",
            "Ask before risky actions",
            "Safety",
            "Confirm before destructive or hard-to-reverse actions (`rm -rf`, database \
             drops, bulk deletes, file overwrites, history rewrites). Prefer a dry run \
             or a plan first.",
        ),
        frag(
            "infra-caution",
            "Be conservative with infrastructure",
            "Safety",
            "This is infrastructure code. Be conservative: prefer plans over direct \
             mutation, never apply changes to shared/remote state without explicit \
             confirmation, and call out anything that touches production.",
        ),
        // --- security ------------------------------------------------------
        frag(
            "secrets-hygiene",
            "Never commit or log secrets",
            "Security",
            "Never print, log, or commit secrets, credentials, tokens, or `.env` \
             files. Keep sensitive values out of code and out of command output.",
        ),
        // --- local environment (live probes) -------------------------------
        // A static framing fragment plus owned bash scripts that probe this
        // machine at render time. The framing tells the agent how to treat the
        // sections that follow; the scripts embed their redacted stdout.
        frag(
            "environment",
            "Environment ground truth",
            "Local Environment",
            "Environment is ground truth, not assumption. The sections that follow \
             are probed live at render time — treat them as authoritative for what \
             is actually installed, running, and reachable here, plus the commands \
             this repo defines. Consult them before assuming a tool, version, \
             service, or command exists, and prefer them over guessing. A missing \
             or empty section means the probe found nothing (tool absent, daemon \
             down, or logged out), so ask rather than guess.",
        ),
        script_frag(
            "host",
            "Machine detection",
            "5m",
            r#"
detect_os() {
  case "$(uname -s 2>/dev/null || echo unknown)" in
    Darwin) echo "macOS" ;;
    Linux)  echo "Linux" ;;
    *)      uname -s 2>/dev/null || echo unknown ;;
  esac
}
distro_info() {
  if [ -r /etc/os-release ]; then
    . /etc/os-release
    printf '%s %s' "${NAME:-Linux}" "${VERSION_ID:-}"
  elif command -v sw_vers >/dev/null 2>&1; then
    printf 'macOS %s (build %s)' "$(sw_vers -productVersion 2>/dev/null)" "$(sw_vers -buildVersion 2>/dev/null)"
  else
    echo unknown
  fi
}
printf 'hostname:  %s\n' "$(hostname 2>/dev/null || echo unknown)"
printf 'user:      %s\n' "${USER:-$(id -un 2>/dev/null || echo unknown)}"
printf 'os:        %s\n' "$(detect_os)"
printf 'distro:    %s\n' "$(distro_info)"
printf 'arch:      %s\n' "$(uname -m 2>/dev/null || echo unknown)"
printf 'kernel:    %s\n' "$(uname -r 2>/dev/null || echo unknown)"
printf 'shell:     %s\n' "${SHELL:-unknown}"
if command -v uptime >/dev/null 2>&1; then
  up=$(uptime | sed -E 's/^[^,]*up //; s/,[[:space:]]+[0-9]+ users?.*$//; s/,[[:space:]]+load[[:space:]]+average.*$//')
  [ -n "$up" ] && printf 'uptime:    %s\n' "$up"
fi
"#,
        ),
        script_frag(
            "toolchain",
            "Toolchain detection",
            "5m",
            r#"
for tool in git node pnpm npm bun deno python3 uv ruby go cargo rustc rg fd gh docker; do
  if command -v "$tool" >/dev/null 2>&1; then
    # `go` reports its version via `go version`, not `--version` (the latter errors).
    if [ "$tool" = go ]; then
      v=$(go version 2>&1 | head -n1)
    else
      v=$("$tool" --version 2>&1 | head -n1)
    fi
    printf '%-10s %s\n' "$tool" "$v"
  fi
done
"#,
        ),
        script_frag(
            "project-scripts",
            "Runnable project commands",
            "5m",
            r#"
# Repo-scoped probe: the commands THIS project defines (package.json scripts,
# Makefile/justfile targets, Cargo, go.mod) so the agent uses real entry points
# instead of inventing them. Read-only; ends with `exit 0` so a short-circuiting
# final test can't drop the output.
out=""
add() { out="${out}$1
"; }

# package.json scripts (node / bun)
if [ -f package.json ]; then
  pm="<pm> run"
  [ -f package-lock.json ] && pm="npm run"
  [ -f yarn.lock ] && pm="yarn"
  [ -f pnpm-lock.yaml ] && pm="pnpm run"
  [ -f bun.lock ] && pm="bun run"
  [ -f bun.lockb ] && pm="bun run"
  scripts=""
  if command -v jq >/dev/null 2>&1; then
    scripts=$(jq -r '.scripts // {} | to_entries[] | "  \(.key) — \(.value | gsub("\\s+";" "))"' package.json 2>/dev/null)
  elif command -v node >/dev/null 2>&1; then
    scripts=$(node -e 'try{const s=(require(process.cwd()+"/package.json").scripts)||{};for(const[k,v]of Object.entries(s))console.log("  "+k+" — "+String(v).replace(/\s+/g," ").trim())}catch(e){}' 2>/dev/null)
  elif command -v bun >/dev/null 2>&1; then
    scripts=$(bun -e 'try{const s=(require(process.cwd()+"/package.json").scripts)||{};for(const[k,v]of Object.entries(s))console.log("  "+k+" — "+String(v).replace(/\s+/g," ").trim())}catch(e){}' 2>/dev/null)
  fi
  if [ -n "$scripts" ]; then
    add "package.json scripts (run with \`$pm <name>\`):"
    add "$scripts"
    add ""
  fi
fi

# Makefile targets
for mk in Makefile makefile GNUmakefile; do
  if [ -f "$mk" ]; then
    targets=$(grep -E '^[a-zA-Z0-9][a-zA-Z0-9_.-]*:' "$mk" 2>/dev/null | grep -v ':=' | sed -E 's/:.*//' | sort -u | sed 's/^/  /')
    if [ -n "$targets" ]; then
      add "Makefile targets (run with \`make <target>\`):"
      add "$targets"
      add ""
    fi
    break
  fi
done

# justfile recipes
for jf in justfile Justfile .justfile; do
  if [ -f "$jf" ]; then
    if command -v just >/dev/null 2>&1; then
      recipes=$(just --list --unsorted 2>/dev/null | sed '1d')
    else
      recipes=$(grep -E '^[a-zA-Z0-9][a-zA-Z0-9_-]*( [^:]*)?:' "$jf" 2>/dev/null | sed -E 's/[ :].*//' | sort -u | sed 's/^/  /')
    fi
    if [ -n "$recipes" ]; then
      add "justfile recipes (run with \`just <recipe>\`):"
      add "$recipes"
      add ""
    fi
    break
  fi
done

# Cargo (rust)
if [ -f Cargo.toml ]; then
  add "Cargo: \`cargo build | test | run | clippy | fmt\` (standard)."
  for cfg in .cargo/config.toml .cargo/config; do
    if [ -f "$cfg" ]; then
      aliases=$(sed -n '/^\[alias\]/,/^\[/p' "$cfg" 2>/dev/null | grep -E '^[a-zA-Z]')
      if [ -n "$aliases" ]; then
        add "  cargo aliases:"
        add "$(printf '%s\n' "$aliases" | sed 's/^/    /')"
      fi
      break
    fi
  done
  add ""
fi

# Go module
if [ -f go.mod ]; then
  add "Go module: \`go build ./... | go test ./... | go vet ./... | gofmt\` (standard)."
  add ""
fi

if [ -n "$out" ]; then
  echo "Commands this repo defines — prefer these over inventing build/test/lint/run invocations:"
  echo
  printf '%s' "$out"
fi
exit 0
"#,
        ),
        script_frag(
            "containers",
            "Container runtime",
            "5m",
            r#"
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  docker ps --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}' 2>/dev/null
fi
if command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
  podman ps --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}' 2>/dev/null
fi
"#,
        ),
        script_frag(
            "ai-tools",
            "AI tooling",
            "5m",
            r#"
for tool in claude codex gemini opencode cursor-agent aider droid amp q goose; do
  if command -v "$tool" >/dev/null 2>&1; then
    v=$("$tool" --version 2>/dev/null | head -n1)
    printf '%-15s %s\n' "$tool" "$v"
  fi
done
if command -v gh >/dev/null 2>&1 && gh extension list 2>/dev/null | grep -qi copilot; then
  printf '%-15s %s\n' "gh copilot" "(gh extension)"
fi
"#,
        ),
        script_frag(
            "tailnet",
            "Tailnet discovery",
            "5m",
            r#"
ts=""
if command -v tailscale >/dev/null 2>&1; then
  ts=$(command -v tailscale)
else
  for p in \
    /Applications/Tailscale.app/Contents/MacOS/Tailscale \
    /opt/homebrew/bin/tailscale \
    /usr/local/bin/tailscale; do
    [ -x "$p" ] && { ts="$p"; break; }
  done
fi
[ -n "$ts" ] || exit 0
# No `2>/dev/null || true`: a stopped/logged-out daemon must surface as a
# non-zero exit + stderr so the run is reported as failed (and retryable),
# not silently cached as "no peers".
"$ts" status
"#,
        ),
        script_frag(
            "vpn-posture",
            "VPN & egress posture",
            "5m",
            r#"
found=0
# Tailscale: connection state + exit node in use (not just offered).
ts=""
if command -v tailscale >/dev/null 2>&1; then
  ts=$(command -v tailscale)
else
  for p in /Applications/Tailscale.app/Contents/MacOS/Tailscale /opt/homebrew/bin/tailscale /usr/local/bin/tailscale; do
    [ -x "$p" ] && { ts="$p"; break; }
  done
fi
if [ -n "$ts" ]; then
  if "$ts" status >/dev/null 2>&1; then
    exitnode=$("$ts" status 2>/dev/null | awk '/^[0-9]/ && /exit node/ && !/offers exit node/ {print $2; exit}')
    if [ -n "$exitnode" ]; then
      printf 'tailscale: up (exit node: %s)\n' "$exitnode"
    else
      printf 'tailscale: up\n'
    fi
  else
    printf 'tailscale: installed, not connected\n'
  fi
  found=1
fi
# Mullvad
if command -v mullvad >/dev/null 2>&1; then
  st=$(mullvad status 2>/dev/null | head -n1)
  [ -n "$st" ] && { printf 'mullvad:   %s\n' "$st"; found=1; }
fi
# WireGuard interfaces (Linux, no root needed)
if command -v ip >/dev/null 2>&1; then
  wg=$(ip -o link show type wireguard 2>/dev/null | awk -F': ' '{print $2}' | paste -sd, -)
  [ -n "$wg" ] && { printf 'wireguard: %s\n' "$wg"; found=1; }
fi
# OpenVPN process
if command -v pgrep >/dev/null 2>&1 && pgrep -x openvpn >/dev/null 2>&1; then
  printf 'openvpn:   running\n'; found=1
fi
# Generic tunnel interfaces (tun/tap/wg). macOS utun* is deliberately excluded:
# the OS spawns many (iCloud Private Relay, Tailscale, …) so it is noise, not a
# VPN signal — Tailscale/WireGuard are detected specifically above.
tuns=""
if command -v ip >/dev/null 2>&1; then
  tuns=$(ip -o link 2>/dev/null | awk -F': ' '{print $2}' | grep -E '^(tun|tap|wg)' | paste -sd, -)
elif command -v ifconfig >/dev/null 2>&1; then
  tuns=$(ifconfig -l 2>/dev/null | tr ' ' '\n' | grep -E '^(tun|tap|wg)' | paste -sd, -)
fi
[ -n "$tuns" ] && printf 'tunnels:   %s\n' "$tuns"
# No VPN/tunnel found leaves output empty; always succeed.
exit 0
"#,
        ),
        script_frag(
            "secrets-posture",
            "Secret stores present (no values)",
            "5m",
            r#"
# Posture only: reports which secret stores and secret-management tools EXIST.
# It never reads, prints, or embeds any secret value — presence checks and a
# private-key count are the only things it does.
out=""
add() { out="${out}  ${1}\n"; }
[ -e "$HOME/.aws/credentials" ]     && add "aws credentials (~/.aws/credentials)"
[ -e "$HOME/.config/gh/hosts.yml" ] && add "github cli (~/.config/gh/hosts.yml)"
[ -e "$HOME/.netrc" ]               && add "netrc (~/.netrc)"
[ -e "$HOME/.docker/config.json" ]  && add "docker auth (~/.docker/config.json)"
[ -e "$HOME/.npmrc" ]               && add "npm (~/.npmrc)"
[ -e "$HOME/.pypirc" ]              && add "pypi (~/.pypirc)"
[ -e "$HOME/.kube/config" ]         && add "kubeconfig (~/.kube/config)"
{ [ -e "$HOME/.cargo/credentials.toml" ] || [ -e "$HOME/.cargo/credentials" ]; } && add "cargo (~/.cargo/credentials*)"
[ -d "$HOME/.gnupg" ]               && add "gnupg (~/.gnupg/)"
[ -d "$HOME/.config/gcloud" ]       && add "gcloud (~/.config/gcloud/)"
if [ -d "$HOME/.ssh" ]; then
  n=$(find "$HOME/.ssh" -maxdepth 1 -type f ! -name '*.pub' ! -name 'known_hosts*' ! -name 'config' ! -name 'authorized_keys' 2>/dev/null | wc -l | tr -d ' ')
  [ "${n:-0}" -gt 0 ] && add "ssh (~/.ssh/: ${n} private key file(s))"
fi
# Repo-local env files: names only, and only when the cwd is a git repo.
if [ -d .git ]; then
  envs=$(find . -maxdepth 2 -type f -name '.env*' ! -name '.env.example' ! -name '.env.sample' 2>/dev/null | sed 's|^\./||' | paste -sd, -)
  [ -n "$envs" ] && add "repo env files: ${envs}"
fi
tools=""
for t in op pass gpg vault sops age doppler bw aws gcloud; do
  command -v "$t" >/dev/null 2>&1 && tools="${tools}${t} "
done
if [ -n "$out" ]; then
  printf 'Secret stores present (presence only — no values are read):\n'
  printf '%b' "$out"
fi
[ -n "$tools" ] && printf 'Secret-management CLIs on PATH: %s\n' "$tools"
# Nothing found leaves output empty; always succeed.
exit 0
"#,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_unique_and_well_formed() {
        let frags = palette();
        let mut ids = std::collections::HashSet::new();
        for c in &frags {
            assert!(ids.insert(c.id.clone()), "duplicate fragment id {}", c.id);
            // Static fragments need guidance; dynamic ones legitimately leave it
            // empty (the redacted script/provider output is embedded instead).
            assert!(
                c.is_dynamic() || !c.guidance.trim().is_empty(),
                "{} has empty guidance and is not dynamic",
                c.id
            );
            // Every shipped fragment carries a category so the studio tree groups it.
            assert!(
                c.category.as_deref().is_some_and(|i| !i.is_empty()),
                "{} has no category",
                c.id
            );
        }
        // A representative spread of palette atoms is present to pick from.
        for needed in [
            "rust-conventions",
            "terse-comms",
            "conventional-commits",
            "branch-discipline",
            "secrets-hygiene",
            "validate-before-done",
            // dynamic environment probes
            "host",
            "toolchain",
            "vpn-posture",
            "secrets-posture",
        ] {
            assert!(ids.contains(needed), "missing palette fragment {needed}");
        }
    }

    #[test]
    fn dynamic_palette_fragments_are_bash_scripts() {
        // The shipped environment probes are command-backed bash scripts (not
        // provider-backed), so studio opens them for view/edit.
        for id in ["host", "toolchain", "containers", "ai-tools", "tailnet"] {
            let c = palette().into_iter().find(|c| c.id == id).unwrap();
            assert!(c.is_dynamic(), "{id} should be dynamic");
            assert!(c.command.is_some(), "{id} should be command-backed");
            assert_eq!(c.script_lang.as_deref(), Some("bash"), "{id} is bash");
            assert!(c.guidance.is_empty(), "{id} is a pure script (no guidance)");
        }
    }

    #[test]
    fn dynamic_palette_scripts_are_valid_bash() {
        // Validate the Rust-raw-string → bash round-trip for every shipped
        // script: `bash -n` parses without executing (no probes run, no side
        // effects), so this catches escaping breakage (e.g. a mangled `\t` in a
        // docker --format, or an unterminated quote) without touching the system.
        for c in palette().into_iter().filter(|c| c.command.is_some()) {
            let cmd = c.command.unwrap();
            let out = std::process::Command::new("bash")
                .arg("-n")
                .arg("-c")
                .arg(&cmd)
                .output()
                .expect("bash should be available to syntax-check scripts");
            assert!(
                out.status.success(),
                "{} is not valid bash:\n{}",
                c.id,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn secrets_posture_never_reads_secret_values() {
        // Posture, not values: the secrets probe must only test for presence and
        // count keys — never read a store's contents. Guard against a future edit
        // that pipes a credential file into the output.
        let c = palette()
            .into_iter()
            .find(|c| c.id == "secrets-posture")
            .unwrap();
        let cmd = c.command.unwrap();
        for forbidden in ["cat ", "head ", "tail ", "less ", "< \"$HOME", "xxd", "od "] {
            assert!(
                !cmd.contains(forbidden),
                "secrets-posture must not read file contents (found {forbidden:?})"
            );
        }
    }

    #[test]
    fn palette_items_are_built_in_origin() {
        // Palette items default to the BuiltIn origin; you don't own them until
        // you duplicate one into a config layer.
        for c in palette() {
            assert_eq!(c.origin, Layer::BuiltIn);
        }
    }

    #[test]
    fn agent_restriction() {
        let mut c = palette()
            .into_iter()
            .find(|c| c.id == "rust-conventions")
            .unwrap();
        assert!(c.applies_to_agent("claude")); // empty = all
        c.agents = vec!["codex".into()];
        assert!(c.applies_to_agent("codex"));
        assert!(!c.applies_to_agent("claude"));
    }

    #[test]
    fn deserializes_minimal_and_full() {
        let minimal: Fragment = toml::from_str("id = \"x\"\nguidance = \"hi\"\n").unwrap();
        assert_eq!(minimal.id, "x");
        assert!(minimal.params.as_table().unwrap().is_empty());

        let full: Fragment = toml::from_str(
            r#"
            id = "ssh"
            description = "SSH within my tailnet"
            category = "Local Environment"
            requires = ["baseline"]
            agents = ["claude"]
            guidance = "You may ssh to {{ params.host }}."
            [params]
            host = "box"
            "#,
        )
        .unwrap();
        assert_eq!(full.category.as_deref(), Some("Local Environment"));
        assert_eq!(full.requires, vec!["baseline"]);
        assert_eq!(full.params.get("host").unwrap().as_str(), Some("box"));
    }

    #[test]
    fn category_none_is_omitted_from_serialization() {
        // The freshness fingerprint serializes the struct by field; a `None`
        // category must not appear, so an uncategorized fragment fingerprints
        // exactly as it did before the field existed. A set category does appear.
        let mut frag: Fragment = toml::from_str("id = \"x\"\nguidance = \"g\"\n").unwrap();
        let none = serde_json::to_string(&frag).unwrap();
        assert!(!none.contains("category"), "None category must be skipped");
        frag.category = Some("Operating Style".into());
        let some = serde_json::to_string(&frag).unwrap();
        assert!(
            some.contains("Operating Style"),
            "set category is serialized"
        );
    }
}
