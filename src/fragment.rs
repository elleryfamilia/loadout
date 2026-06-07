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
            assert!(!c.guidance.trim().is_empty(), "{} has empty guidance", c.id);
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
        ] {
            assert!(ids.contains(needed), "missing palette fragment {needed}");
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
