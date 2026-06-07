//! Fragments — reusable, self-contained units of guidance.
//!
//! A **fragment** is one atom of agent guidance ("Rust conventions", "be
//! conservative with infrastructure", "be terse"). Fragments are authored
//! once, kept in a library (built-ins plus `[[fragments]]` config entries — the
//! legacy `[[capabilities]]` key is still accepted), and **composed by
//! profiles** (see [`crate::profile::compose`]). This is the reuse seam: many
//! profiles can pull the same fragment instead of repeating inline guidance.
//!
//! A fragment can self-gate with `when` rules, declare `requires`
//! dependencies, carry `category`/`risk`/`tags` metadata, be restricted to
//! specific `agents`, and expose free-form `params` to its guidance template.
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

/// How attention-worthy a fragment's guidance is. Rendered as an annotation
/// when it is not [`Risk::Info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Ordinary guidance (the default); rendered without annotation.
    #[default]
    Info,
    /// Worth flagging — touches shared state, has side effects, etc.
    Caution,
    /// High-stakes — destructive or hard to reverse.
    Dangerous,
}

impl Risk {
    /// A short annotation for headings, or `None` for [`Risk::Info`].
    pub fn annotation(self) -> Option<&'static str> {
        match self {
            Risk::Info => None,
            Risk::Caution => Some("⚠️ caution"),
            Risk::Dangerous => Some("🚨 dangerous"),
        }
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
    /// Optional icon name from studio's curated set (e.g. `box`, `bolt`). Purely
    /// cosmetic — surfaced in studio, never in the rendered overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Free-form tags for discovery (`comms`, `safety`, `dev-workflow`, …).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional human-friendly category that groups this fragment in studio's
    /// tree (e.g. `Operating Style`, `Local Environment`). Distinct from the
    /// free-form `tags`. `skip_serializing_if` keeps the freshness fingerprint
    /// of an uncategorized fragment unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Attention level; annotated in the overlay when not [`Risk::Info`].
    #[serde(default)]
    pub risk: Risk,
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

    /// The synthetic fragment that carries a profile's inline `guidance`
    /// (back-compat). Its id is `<profile>:inline`; it always applies and is
    /// rendered last among a profile's contributions.
    pub fn inline(profile: &str, guidance: String) -> Fragment {
        Fragment {
            id: format!("{profile}:inline"),
            description: None,
            tags: Vec::new(),
            category: None,
            risk: Risk::Info,
            when: Vec::new(),
            requires: Vec::new(),
            params: empty_params(),
            guidance,
            agents: Vec::new(),
            provider: None,
            command: None,
            script_lang: None,
            icon: None,
            allow_exec: true,
            cache: None,
            origin: Layer::default(),
        }
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
    // Build a static (markdown) palette fragment: a curated icon + a friendly
    // category + discovery tags + templated guidance. Risk defaults to Info; the
    // caution starters wrap the result with `caution(..)`.
    fn frag(
        id: &str,
        description: &str,
        icon: &str,
        category: &str,
        tags: &[&str],
        guidance: &str,
    ) -> Fragment {
        Fragment {
            id: id.to_string(),
            description: Some(description.to_string()),
            icon: Some(icon.to_string()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            category: Some(category.to_string()),
            risk: Risk::Info,
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
    // Raise a starter's attention level to Caution (a warning annotation in the
    // rendered overlay and a colored risk spine in studio).
    fn caution(c: Fragment) -> Fragment {
        Fragment {
            risk: Risk::Caution,
            ..c
        }
    }

    vec![
        // --- baseline awareness --------------------------------------------
        frag(
            "baseline",
            "Follow repo conventions",
            "box",
            "Operating Style",
            &["awareness"],
            "Follow the repository's existing conventions and keep changes minimal, \
             focused, and well-tested. Match the surrounding code's style and naming \
             rather than imposing your own.",
        ),
        // --- communication -------------------------------------------------
        frag(
            "terse-comms",
            "Terse communication",
            "bolt",
            "Operating Style",
            &["comms"],
            "Be terse: lead with the result and what changed; skip preamble. For \
             non-trivial decisions, briefly note the reasoning, the tradeoffs, and the \
             alternatives considered.",
        ),
        // --- stack conventions (one per detected language/platform) --------
        frag(
            "rust-conventions",
            "Rust conventions",
            "code",
            "Stack Conventions",
            &["stack"],
            "Rust project. Build with cargo, format with rustfmt, lint with clippy \
             (`cargo clippy --all-targets`). Prefer `?`/`Result` over `unwrap()` or \
             `panic!` in non-test code.",
        ),
        frag(
            "node-conventions",
            "Node.js conventions",
            "code",
            "Stack Conventions",
            &["stack"],
            "Node.js project. Use pnpm for scripts and dependencies, and prefer \
             TypeScript over plain JavaScript. Keep the type-checker and linter clean \
             before committing.",
        ),
        frag(
            "nextjs-conventions",
            "Next.js conventions",
            "code",
            "Stack Conventions",
            &["stack"],
            "Next.js app. Respect the existing app/pages router convention and keep \
             server/client component boundaries explicit. Use pnpm for scripts and \
             dependencies.",
        ),
        frag(
            "go-conventions",
            "Go conventions",
            "code",
            "Stack Conventions",
            &["stack"],
            "Go project. Use the standard toolchain (`go build`, `go test`, `go vet`, \
             `gofmt`); add golangci-lint for stricter checks. Handle errors explicitly \
             — don't silently discard them.",
        ),
        frag(
            "python-conventions",
            "Python conventions",
            "code",
            "Stack Conventions",
            &["stack"],
            "Python project. Use uv for environments and dependencies, ruff for \
             linting and formatting, and pytest for tests.",
        ),
        // --- workflow ------------------------------------------------------
        frag(
            "conventional-commits",
            "Conventional commits",
            "git-branch",
            "Dev Workflow",
            &["dev-workflow"],
            "Use Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, …). \
             Imperative subject ≤72 chars; the body explains *why* when it is \
             non-obvious.",
        ),
        frag(
            "commit-checkpoints",
            "Commit at checkpoints",
            "git-branch",
            "Dev Workflow",
            &["dev-workflow"],
            "Commit at logical checkpoints with clear, descriptive messages rather \
             than one giant commit at the end — don't wait to be told.",
        ),
        frag(
            "plan-nontrivial",
            "Plan non-trivial work",
            "book",
            "Dev Workflow",
            &["dev-workflow"],
            "For non-trivial work, sketch a short plan before implementing: the \
             objective, the approach, and the risks. Skip the ceremony for typos and \
             obvious one-line fixes.",
        ),
        frag(
            "experimental-iteration",
            "Spike fast on a throwaway branch",
            "rocket",
            "Dev Workflow",
            &["dev-workflow"],
            "Experimental branch — optimize for iteration speed. Throwaway spikes are \
             fine; keep changes scoped to this branch and don't wire them into shared \
             modules yet.",
        ),
        // --- quality -------------------------------------------------------
        frag(
            "validate-before-done",
            "Build, test, and lint before done",
            "terminal",
            "Quality",
            &["quality"],
            "Before declaring work done, run the build, the tests, and the linter, and \
             report the results honestly. If something failed or was skipped, say so \
             plainly — don't claim success you didn't verify.",
        ),
        frag(
            "testing-discipline",
            "Cover changes with tests",
            "flask",
            "Quality",
            &["quality"],
            "Add or update tests to match the change: unit or integration tests for \
             logic, end-to-end tests for user-facing behavior. If a real harness is \
             impractical, say so instead of skipping silently.",
        ),
        // --- safety --------------------------------------------------------
        caution(frag(
            "branch-discipline",
            "Never commit to main",
            "git-branch",
            "Safety",
            &["safety"],
            "Never commit or push directly to the main/master branch — always work on \
             a branch and open a pull request instead of pushing to shared branches.",
        )),
        caution(frag(
            "ask-before-risky",
            "Ask before risky actions",
            "shield",
            "Safety",
            &["safety"],
            "Confirm before destructive or hard-to-reverse actions (`rm -rf`, database \
             drops, bulk deletes, file overwrites, history rewrites). Prefer a dry run \
             or a plan first.",
        )),
        caution(frag(
            "infra-caution",
            "Be conservative with infrastructure",
            "server",
            "Safety",
            &["infra", "safety"],
            "This is infrastructure code. Be conservative: prefer plans over direct \
             mutation, never apply changes to shared/remote state without explicit \
             confirmation, and call out anything that touches production.",
        )),
        // --- security ------------------------------------------------------
        caution(frag(
            "secrets-hygiene",
            "Never commit or log secrets",
            "lock",
            "Security",
            &["security"],
            "Never print, log, or commit secrets, credentials, tokens, or `.env` \
             files. Keep sensitive values out of code and out of command output.",
        )),
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
            // Every shipped fragment carries a curated icon, a category, and at
            // least one tag so the studio tree renders a glyph and groups it.
            assert!(
                c.icon.as_deref().is_some_and(|i| !i.is_empty()),
                "{} has no icon",
                c.id
            );
            assert!(
                c.category.as_deref().is_some_and(|i| !i.is_empty()),
                "{} has no category",
                c.id
            );
            assert!(!c.tags.is_empty(), "{} has no tags", c.id);
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
    fn risk_annotation_only_for_non_info() {
        assert_eq!(Risk::Info.annotation(), None);
        assert!(Risk::Caution.annotation().is_some());
        assert!(Risk::Dangerous.annotation().is_some());
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
        assert_eq!(minimal.risk, Risk::Info);
        assert!(minimal.params.as_table().unwrap().is_empty());

        let full: Fragment = toml::from_str(
            r#"
            id = "ssh"
            description = "SSH within my tailnet"
            category = "Local Environment"
            tags = ["machine", "infra"]
            risk = "caution"
            requires = ["baseline"]
            agents = ["claude"]
            guidance = "You may ssh to {{ params.host }}."
            [params]
            host = "box"
            "#,
        )
        .unwrap();
        assert_eq!(full.risk, Risk::Caution);
        assert_eq!(full.category.as_deref(), Some("Local Environment"));
        assert_eq!(full.requires, vec!["baseline"]);
        assert_eq!(full.params.get("host").unwrap().as_str(), Some("box"));
    }
}
