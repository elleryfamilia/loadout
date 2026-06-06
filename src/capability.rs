//! Capabilities — reusable, self-contained units of guidance.
//!
//! A **capability** is one atom of agent guidance ("Rust conventions", "be
//! conservative with infrastructure", "be terse"). Capabilities are authored
//! once, kept in a library (built-ins plus `[[capabilities]]` config entries),
//! and **composed by profiles** (see [`crate::profile::compose`]). This is the
//! reuse seam: many profiles can pull the same capability instead of repeating
//! inline guidance.
//!
//! A capability can self-gate with `when` rules, declare `requires`
//! dependencies, carry `risk`/`tags` metadata, be restricted to specific
//! `agents`, and expose free-form `params` to its guidance template.
//!
//! Phase 1 ships only **static** capabilities (fixed, templated `guidance`).
//! Dynamic capabilities (provider/command-backed live output) arrive in a later
//! phase; the struct is laid out so those fields can be added without churn.

use serde::{Deserialize, Serialize};

use crate::profile::Rule;

/// Which config layer defined a capability. Used for trust: commands authored
/// in built-in/global layers are trusted; commands from a repo layer require
/// `rosita allow`.
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
    /// Whether commands authored in this layer run without `rosita allow`
    /// (you authored built-in/global config; repo config is untrusted by default).
    pub fn is_trusted_authorship(self) -> bool {
        matches!(self, Layer::BuiltIn | Layer::Global | Layer::GlobalLocal)
    }

    /// Whether `[[capabilities]]` defined in this layer are honored. Capabilities
    /// are a **global** concept (the library any profile can compose); a repo
    /// layer that declares them is ignored (and `doctor` flags it). Aligns with
    /// trusted authorship — a repo can never contribute a capability.
    pub fn contributes_capabilities(self) -> bool {
        self.is_trusted_authorship()
    }

    /// Whether `[[profiles]]` defined in this layer are honored. Profiles are
    /// **public-global only**: authored once in the global `config.toml`, shared
    /// across repos. Not the private global `local.toml`, never a repo layer.
    pub fn contributes_profiles(self) -> bool {
        matches!(self, Layer::BuiltIn | Layer::Global)
    }
}

/// How attention-worthy a capability's guidance is. Rendered as an annotation
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
pub struct Capability {
    /// Stable id referenced by `profiles[].capabilities` and `requires`.
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
    /// Attention level; annotated in the overlay when not [`Risk::Info`].
    #[serde(default)]
    pub risk: Risk,
    /// Self-gate: all clauses must match the context. Empty = always applies
    /// (the composing profile's own rules still gate when it is pulled in).
    #[serde(default)]
    pub when: Vec<Rule>,
    /// Other capability ids this one pulls in (resolved before it, deduped).
    #[serde(default)]
    pub requires: Vec<String>,
    /// Free-form parameters exposed to the guidance template as `params`.
    #[serde(default = "empty_params")]
    pub params: toml::Value,
    /// The guidance markdown, itself rendered as a minijinja template. For a
    /// dynamic capability, `provider.output`/`provider.data` are in scope; an
    /// empty `guidance` falls back to the raw provider/command output.
    #[serde(default)]
    pub guidance: String,
    /// Optional agent restriction (ids); empty = all agents. Applied at render
    /// time because the active agent varies per render.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Dynamic: a built-in provider id (`host`/`docker`/…) whose live output is
    /// embedded. Always trusted (built-in probes are safe).
    #[serde(default)]
    pub provider: Option<String>,
    /// Dynamic: a shell command (or script body) whose (redacted) stdout is
    /// embedded. Trust-gated when authored in a repo layer (see [`crate::trust`]).
    #[serde(default)]
    pub command: Option<String>,
    /// Interpreter for `command` when it is a script body: `bash`, `sh`, or
    /// `python`. `None` runs `command` as a plain `sh -c` line (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_lang: Option<String>,
    /// Whether a `command`-backed capability is allowed to execute. Defaults to
    /// `true` (existing configs keep running); set `false` to disable a script
    /// without deleting it. Layered *on top of* repo trust — a command runs only
    /// when the repo is trusted AND this is `true`. Only serialized when `false`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_exec: bool,
    /// Cache TTL for dynamic output (e.g. `60s`, `5m`); default 60s.
    #[serde(default)]
    pub cache: Option<String>,
    /// Which config layer defined this capability (set during config load, not
    /// deserialized). Drives command trust.
    #[serde(skip)]
    pub origin: Layer,
}

/// Default `params`: an empty TOML table (so `{{ params.x }}` is just empty).
fn empty_params() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

/// Serde default for [`Capability::allow_exec`] (execution on unless disabled).
fn default_true() -> bool {
    true
}

/// `skip_serializing_if` for [`Capability::allow_exec`] — only persist the
/// off-switch (`allow_exec = false`), never the default.
fn is_true(b: &bool) -> bool {
    *b
}

impl Capability {
    /// The heading title for this capability: its description, else its id.
    pub fn title(&self) -> &str {
        self.description.as_deref().unwrap_or(&self.id)
    }

    /// Whether this capability resolves live output (provider- or command-backed).
    pub fn is_dynamic(&self) -> bool {
        self.provider.is_some() || self.command.is_some()
    }

    /// The synthetic capability that carries a profile's inline `guidance`
    /// (back-compat). Its id is `<profile>:inline`; it always applies and is
    /// rendered last among a profile's contributions.
    pub fn inline(profile: &str, guidance: String) -> Capability {
        Capability {
            id: format!("{profile}:inline"),
            description: None,
            tags: Vec::new(),
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

    /// Whether this capability applies to `agent` given its `agents` restriction.
    pub fn applies_to_agent(&self, agent: &str) -> bool {
        self.agents.is_empty() || self.agents.iter().any(|a| a == agent)
    }
}

/// The shipped capability **palette**: a read-only catalog you *pick from* when
/// composing a profile. Palette items are **never auto-composed and never
/// written into your config** — to use or customize one you duplicate it into a
/// config layer and own the copy (studio's `DuplicatePaletteItem`). Composition
/// resolves a profile's capability refs against your *own* library only, so a
/// profile that names a palette id you haven't duplicated renders nothing for it.
pub fn palette() -> Vec<Capability> {
    fn cap(id: &str, description: &str, guidance: &str) -> Capability {
        Capability {
            id: id.to_string(),
            description: Some(description.to_string()),
            tags: Vec::new(),
            risk: Risk::Info,
            when: Vec::new(),
            requires: Vec::new(),
            params: empty_params(),
            guidance: guidance.to_string(),
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
    fn tagged(mut c: Capability, tags: &[&str]) -> Capability {
        c.tags = tags.iter().map(|t| t.to_string()).collect();
        c
    }

    vec![
        // --- a sensible baseline starter -----------------------------------
        tagged(
            cap(
                "baseline",
                "Baseline",
                "Follow the repository's existing conventions and keep changes \
                 minimal, focused, and well-tested.",
            ),
            &["awareness"],
        ),
        // --- stack conventions (one per detected language/platform) --------
        tagged(
            cap(
                "rust-conventions",
                "Rust conventions",
                "Rust project. Build with cargo, format with rustfmt, lint with \
                 clippy (`cargo clippy --all-targets`). Prefer `?`/`Result` over \
                 `unwrap()` in non-test code.",
            ),
            &["stack"],
        ),
        tagged(
            cap(
                "nextjs-conventions",
                "Next.js conventions",
                "Next.js app. Respect the existing app/pages router convention. \
                 Use the detected package manager. Keep server/client component \
                 boundaries explicit.",
            ),
            &["stack"],
        ),
        tagged(
            cap(
                "node-conventions",
                "Node.js conventions",
                "Node.js project. Use the detected package manager for scripts; \
                 prefer TypeScript where the project already uses it.",
            ),
            &["stack"],
        ),
        tagged(
            cap(
                "go-conventions",
                "Go conventions",
                "Go project. Use the standard toolchain: `go build`, `go test`, \
                 `go vet`, `gofmt`.",
            ),
            &["stack"],
        ),
        tagged(
            cap(
                "python-conventions",
                "Python conventions",
                "Python project. Prefer the detected tool (uv/poetry) for envs \
                 and deps; run tests with pytest.",
            ),
            &["stack"],
        ),
        // --- safety / workflow starters ------------------------------------
        Capability {
            risk: Risk::Caution,
            ..tagged(
                cap(
                    "infra-caution",
                    "Infrastructure caution",
                    "This is infrastructure code. Be conservative: prefer plans \
                     over direct mutation, never apply changes to shared/remote \
                     state without explicit confirmation, and call out anything \
                     that touches production.",
                ),
                &["infra", "safety"],
            )
        },
        tagged(
            cap(
                "experimental-iteration",
                "Experimental iteration",
                "Experimental branch — optimize for iteration speed. Throwaway \
                 spikes are fine; keep changes scoped to this branch and don't \
                 wire them into shared modules yet.",
            ),
            &["dev-workflow"],
        ),
        // --- reusable cross-cutting starters -------------------------------
        tagged(
            cap(
                "terse-comms",
                "Terse communication",
                "Be terse: lead with the result and what changed; skip preamble. \
                 For non-trivial decisions, briefly note the reasoning and the \
                 alternatives considered.",
            ),
            &["comms"],
        ),
        tagged(
            cap(
                "conventional-commits",
                "Conventional commits",
                "Use Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, \
                 …). Imperative subject ≤72 chars; the body explains *why* when \
                 it is non-obvious.",
            ),
            &["dev-workflow"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_unique_and_well_formed() {
        let caps = palette();
        let mut ids = std::collections::HashSet::new();
        for c in &caps {
            assert!(ids.insert(c.id.clone()), "duplicate capability id {}", c.id);
            assert!(!c.guidance.trim().is_empty(), "{} has empty guidance", c.id);
        }
        // A representative spread of palette atoms is present to pick from.
        for needed in ["rust-conventions", "terse-comms", "conventional-commits"] {
            assert!(ids.contains(needed), "missing palette capability {needed}");
        }
    }

    #[test]
    fn palette_items_are_built_in_origin() {
        // Palette items default to the BuiltIn origin and are never trusted as
        // your own authorship until duplicated into a config layer.
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
        let minimal: Capability = toml::from_str("id = \"x\"\nguidance = \"hi\"\n").unwrap();
        assert_eq!(minimal.id, "x");
        assert_eq!(minimal.risk, Risk::Info);
        assert!(minimal.params.as_table().unwrap().is_empty());

        let full: Capability = toml::from_str(
            r#"
            id = "ssh"
            description = "SSH within my tailnet"
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
        assert_eq!(full.requires, vec!["baseline"]);
        assert_eq!(full.params.get("host").unwrap().as_str(), Some("box"));
    }
}
