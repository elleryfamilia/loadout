//! Profiles and **per-profile** capability composition (pick-one model).
//!
//! A **profile** is tied to a detected language/platform via its `targets`
//! (e.g. `targets = ["rust"]`, or `["machine"]` for the no-repo context). A
//! project uses **one** profile and renders *its* capabilities — there is no
//! additive union across profiles and no always-on baseline. Composition now
//! happens only *within* a profile: its capability list, deduped by id,
//! `requires`-resolved (dependencies first), per-capability `when`-filtered, with
//! effective `params` merged (defaults ← profile-ref ← private `capability_params`).
//!
//! Which profile (if any) applies is decided by [`select`]: match the context's
//! coarse [`targets`](crate::context::Context::selection_targets) → 0 matches =
//! none, exactly 1 = auto-use, 2+ = ambiguous (the caller prompts and remembers
//! the choice as a [`Binding`](crate::binding::Binding)). Selection is fully
//! deterministic; no LLM is ever involved.
//!
//! Back-compat: a profile's inline `guidance` is treated as an implicit
//! `<profile>:inline` capability, appended after its explicit capabilities.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::binding::Binding;
use crate::capability::Capability;
use crate::context::Context;

/// A configured profile: the language/platform it targets and the capabilities
/// (and/or inline guidance) it contributes when selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Profile name (also used to find `profiles/<name>.md.j2` templates).
    pub name: String,
    /// Detected language/platform tags this profile is for (`rust`, `node`,
    /// `nextjs`, `go`, `python`, `android`, `java`, `machine`, …). The profile is
    /// a selection candidate when **any** of its targets matches the detected
    /// context. Empty `targets` ⇒ never auto-selected (still bindable by name).
    #[serde(default)]
    pub targets: Vec<String>,
    /// Capabilities this profile composes (in declaration order). Each entry is
    /// either a bare id (`"rust-conventions"`) or an id with inline `params`
    /// overrides (`{ id = "ssh", params = { user = "deploy" } }`). A saved
    /// profile needs ≥1 (enforced by studio validation, not the parser).
    #[serde(default)]
    pub capabilities: Vec<CapabilityRef>,
    /// Optional base-template override (per agent the renderer appends the
    /// agent suffix). Rarely needed.
    #[serde(default)]
    pub template: Option<String>,
    /// Inline guidance markdown (back-compat; becomes a `<profile>:inline`
    /// capability appended after the explicit ones).
    #[serde(default)]
    pub guidance: Option<String>,
    /// When `true`, the profile is never selected or composed (an off-switch that
    /// keeps the definition around). Only serialized when set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// How a profile references a capability: a bare id, or an id with inline
/// `params` overrides. Inline params are public (they live in the profile);
/// sensitive values belong in `local.toml`'s `[capability_params]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CapabilityRef {
    /// `capabilities = ["rust-conventions"]`
    Id(String),
    /// `capabilities = [{ id = "ssh", params = { user = "deploy" } }]`
    Detailed {
        /// The capability id.
        id: String,
        /// Inline params overrides (merged over the capability's defaults).
        #[serde(default)]
        params: Option<toml::Value>,
    },
}

impl CapabilityRef {
    /// The referenced capability id.
    pub fn id(&self) -> &str {
        match self {
            CapabilityRef::Id(s) => s,
            CapabilityRef::Detailed { id, .. } => id,
        }
    }
    /// The inline params overrides, if any.
    pub fn params(&self) -> Option<&toml::Value> {
        match self {
            CapabilityRef::Detailed { params, .. } => params.as_ref(),
            CapabilityRef::Id(_) => None,
        }
    }
}

/// A single match condition, used by a **capability**'s `when` self-gate.
/// (Profiles no longer carry `when` rules — they select on `targets`.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Which context field to test.
    pub field: Field,
    /// How to compare.
    pub op: Op,
    /// The value to compare against.
    pub value: String,
}

/// Context fields that capability `when` rules can match against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// Detected stack keys, e.g. `rust`, `nextjs`, `node`, `go`, `python`.
    Stack,
    /// Detected language names, e.g. `Rust`, `TypeScript`.
    Language,
    /// Detected package managers, e.g. `cargo`, `pnpm`, `uv`.
    PackageManager,
    /// cwd path relative to the repo root (forward slashes), e.g. `infra/db`.
    Path,
    /// git branch.
    Branch,
    /// repository name.
    Repo,
    /// host class derived from config (`work`, …).
    HostClass,
    /// operating system (`macos`, `linux`, …).
    Os,
    /// CPU architecture (`aarch64`, `x86_64`, …).
    Arch,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Exact equality (case-sensitive).
    Equals,
    /// Prefix match.
    StartsWith,
    /// Substring match.
    Contains,
    /// Regex match (anchored as written).
    Matches,
}

/// A capability resolved into the composition, with provenance.
#[derive(Debug, Clone)]
pub struct ResolvedCapability {
    /// The capability itself (cloned from the library).
    pub capability: Capability,
    /// The profile that pulled it in (directly, or transitively via `requires`).
    pub via_profile: String,
    /// Human-readable provenance, e.g. "capability 'rust-conventions' via profile 'rust'".
    pub reason: String,
    /// True for a synthetic `<profile>:inline` capability — enables the
    /// `profiles/<name>.md.j2` template-file override at render time.
    pub inline: bool,
}

/// The outcome of composing the selected profile: its name, the ordered/deduped
/// capabilities it contributes, and human-readable reasons. Default (`profile:
/// None`, empty caps) is the "no profile applies" overlay.
#[derive(Debug, Clone, Default)]
pub struct Composition {
    /// The selected profile name, or `None` when no profile applies.
    pub profile: Option<String>,
    /// Resolved capabilities, in render order.
    pub capabilities: Vec<ResolvedCapability>,
    /// Provenance lines (one per contributing capability).
    pub reasons: Vec<String>,
}

impl Composition {
    /// The selected profile name, used as the display/audit label and for the
    /// base-template override. `None` only when no profile applied.
    pub fn primary_profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// The display label for the composition (selected profile, or `none`).
    pub fn label(&self) -> &str {
        self.profile.as_deref().unwrap_or("none")
    }
}

/// The result of profile selection for a context (see [`select`]).
#[derive(Debug, Clone)]
pub enum Selection {
    /// No profile applies: 0 targets matched, or an explicit `None` binding.
    None,
    /// Exactly one profile applies, or a remembered binding resolved to one.
    /// Auto-used with no prompt.
    Use(ProfileConfig),
    /// 2+ profiles match and there is no remembered choice — the caller must
    /// prompt the user and then persist the answer as a [`Binding`].
    Ambiguous(Vec<ProfileConfig>),
}

/// Whether `profile` is a selection candidate for the given context `tags`
/// (any target matches). Empty `targets` never matches.
pub fn profile_matches_targets(profile: &ProfileConfig, tags: &[String]) -> bool {
    profile
        .targets
        .iter()
        .any(|t| tags.iter().any(|tag| tag == t))
}

/// Select the profile for `ctx`, honoring a remembered [`Binding`] first.
///
/// 1. A binding wins: `None` ⇒ no profile (remembered opt-out); a named profile
///    that still exists ⇒ use it (a deleted/renamed binding falls through).
/// 2. Otherwise match `ctx`'s coarse targets: 0 ⇒ [`Selection::None`], exactly 1
///    ⇒ [`Selection::Use`] (no prompt), 2+ ⇒ [`Selection::Ambiguous`].
pub fn select(ctx: &Context, profiles: &[ProfileConfig], binding: Option<&Binding>) -> Selection {
    if let Some(b) = binding {
        match b {
            Binding::None => return Selection::None,
            Binding::Profile(name) => {
                // A disabled bound profile is inert — fall through to re-selection.
                if let Some(p) = profiles.iter().find(|p| &p.name == name && !p.disabled) {
                    return Selection::Use(p.clone());
                }
            }
        }
    }

    let tags = ctx.selection_targets();
    let candidates: Vec<ProfileConfig> = profiles
        .iter()
        .filter(|p| !p.disabled && profile_matches_targets(p, &tags))
        .cloned()
        .collect();

    match candidates.len() {
        0 => Selection::None,
        1 => Selection::Use(candidates.into_iter().next().unwrap()),
        _ => Selection::Ambiguous(candidates),
    }
}

/// Compose the chosen `selection` into a [`Composition`]. Only [`Selection::Use`]
/// produces capabilities; `None`/`Ambiguous` yield the empty overlay (the caller
/// resolves an `Ambiguous` to a concrete profile or `None` before this point).
pub fn compose_selection(
    ctx: &Context,
    selection: &Selection,
    capabilities: &[Capability],
    capability_params: &BTreeMap<String, toml::Value>,
) -> Composition {
    match selection {
        Selection::Use(p) => compose_profile(ctx, p, capabilities, capability_params),
        Selection::None | Selection::Ambiguous(_) => Composition::default(),
    }
}

/// Compose a single profile's capability list into an ordered, deduped set.
///
/// In declaration order, add each referenced capability — expanding `requires`
/// depth-first (dependencies first) with cycle protection, filtering each by its
/// own `when`, and skipping ids not in *your* library (palette items must be
/// duplicated in first). Then append the profile's inline guidance as a
/// synthetic capability.
///
/// Agent restriction (`Capability::agents`) is intentionally **not** applied
/// here — the active agent varies per render, so it is applied at render time.
///
/// Effective `params` per resolved capability are merged (later wins): the
/// capability's own defaults ← the profile reference's inline `params` ← the
/// `capability_params[id]` (private/local) overrides.
pub fn compose_profile(
    ctx: &Context,
    profile: &ProfileConfig,
    capabilities: &[Capability],
    capability_params: &BTreeMap<String, toml::Value>,
) -> Composition {
    let cx = ComposeCtx {
        ctx,
        lib: capabilities.iter().map(|c| (c.id.as_str(), c)).collect(),
        capability_params,
    };

    let mut acc = Accumulator::default();
    let provenance = format!("via profile '{}'", profile.name);
    for cap_ref in &profile.capabilities {
        acc.add(
            &cx,
            cap_ref.id(),
            &profile.name,
            &provenance,
            cap_ref.params(),
            &mut HashSet::new(),
        );
    }

    // Back-compat: inline guidance as a synthetic capability, appended after the
    // profile's explicit capabilities.
    if let Some(text) = &profile.guidance {
        let inline = Capability::inline(&profile.name, text.clone());
        if acc.added.insert(inline.id.clone()) {
            let reason = format!("inline guidance via profile '{}'", profile.name);
            acc.reasons.push(reason.clone());
            acc.resolved.push(ResolvedCapability {
                capability: inline,
                via_profile: profile.name.clone(),
                reason,
                inline: true,
            });
        }
    }

    Composition {
        profile: Some(profile.name.clone()),
        capabilities: acc.resolved,
        reasons: acc.reasons,
    }
}

/// Immutable inputs shared across [`Accumulator::add`] recursion.
struct ComposeCtx<'a> {
    ctx: &'a Context,
    lib: BTreeMap<&'a str, &'a Capability>,
    capability_params: &'a BTreeMap<String, toml::Value>,
}

/// Mutable state threaded through capability resolution.
#[derive(Default)]
struct Accumulator {
    resolved: Vec<ResolvedCapability>,
    added: HashSet<String>,
    reasons: Vec<String>,
}

impl Accumulator {
    /// Resolve `id` (and its `requires`, dependencies first) into the set,
    /// applying `ref_params` (the profile reference's inline overrides) and the
    /// private `capability_params` to the capability's effective params.
    fn add(
        &mut self,
        cx: &ComposeCtx,
        id: &str,
        via_profile: &str,
        provenance: &str,
        ref_params: Option<&toml::Value>,
        in_progress: &mut HashSet<String>,
    ) {
        if self.added.contains(id) {
            return;
        }
        if !in_progress.insert(id.to_string()) {
            crate::warn_user!("capability dependency cycle at '{id}' — skipping");
            return;
        }
        let outcome = match cx.lib.get(id) {
            None => {
                crate::warn_user!("unknown capability '{id}' ({provenance})");
                None
            }
            Some(cap) if !capability_applies(cx.ctx, cap) => None, // `when` not satisfied
            Some(cap) => Some(*cap),
        };
        if let Some(cap) = outcome {
            // Dependencies first, so they render before the dependent. Deps
            // carry no per-reference params (they aren't listed by a profile).
            for dep in &cap.requires {
                let dep_provenance = format!("required by '{id}'");
                self.add(cx, dep, via_profile, &dep_provenance, None, in_progress);
            }
            self.added.insert(id.to_string());
            let reason = format!("capability '{id}' {provenance}");
            self.reasons.push(reason.clone());

            // Effective params: defaults ← ref params ← private params.
            let mut params = cap.params.clone();
            if let Some(rp) = ref_params {
                params = crate::config::merge_toml(params, rp.clone());
            }
            if let Some(lp) = cx.capability_params.get(id) {
                params = crate::config::merge_toml(params, lp.clone());
            }
            let mut resolved_cap = cap.clone();
            resolved_cap.params = params;

            self.resolved.push(ResolvedCapability {
                capability: resolved_cap,
                via_profile: via_profile.to_string(),
                reason,
                inline: false,
            });
        }
        in_progress.remove(id);
    }
}

/// Whether a capability's own `when` gate is satisfied (empty = always).
fn capability_applies(ctx: &Context, cap: &Capability) -> bool {
    cap.when.is_empty() || cap.when.iter().all(|r| rule_matches(ctx, r))
}

/// Whether a single `when` rule matches the context.
pub fn rule_matches(ctx: &Context, rule: &Rule) -> bool {
    let candidates = field_values(ctx, rule.field);
    candidates.iter().any(|c| apply(rule.op, c, &rule.value))
}

fn apply(op: Op, candidate: &str, value: &str) -> bool {
    match op {
        Op::Equals => candidate == value,
        Op::StartsWith => candidate.starts_with(value),
        Op::Contains => candidate.contains(value),
        Op::Matches => regex::Regex::new(value)
            .map(|re| re.is_match(candidate))
            .unwrap_or_else(|e| {
                crate::warn_user!("invalid regex {value:?} in capability `when` rule: {e}");
                false
            }),
    }
}

fn field_values(ctx: &Context, field: Field) -> Vec<String> {
    match field {
        Field::Stack => ctx.stacks.clone(),
        Field::Language => ctx.languages.clone(),
        Field::PackageManager => ctx.package_managers.clone(),
        Field::Path => vec![ctx.rel_path()],
        Field::Branch => ctx
            .git
            .as_ref()
            .and_then(|g| g.branch.clone())
            .into_iter()
            .collect(),
        Field::Repo => ctx.repo_name.clone().into_iter().collect(),
        Field::HostClass => ctx.system.host_class.clone().into_iter().collect(),
        Field::Os => vec![ctx.system.os.clone()],
        Field::Arch => vec![ctx.system.arch.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Risk;
    use crate::context::test_support::sample_context;

    fn cap(id: &str, guidance: &str) -> Capability {
        Capability {
            id: id.into(),
            description: Some(id.into()),
            tags: vec![],
            risk: Risk::Info,
            when: vec![],
            requires: vec![],
            params: toml::Value::Table(Default::default()),
            guidance: guidance.into(),
            agents: vec![],
            provider: None,
            command: None,
            script_lang: None,
            icon: None,
            allow_exec: true,
            cache: None,
            origin: crate::capability::Layer::default(),
        }
    }

    fn rule(field: Field, op: Op, value: &str) -> Rule {
        Rule {
            field,
            op,
            value: value.into(),
        }
    }

    fn prof(name: &str, targets: &[&str], caps: &[&str]) -> ProfileConfig {
        ProfileConfig {
            name: name.into(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            capabilities: caps
                .iter()
                .map(|s| CapabilityRef::Id(s.to_string()))
                .collect(),
            template: None,
            guidance: None,
            disabled: false,
        }
    }

    fn compose_t(ctx: &Context, profile: &ProfileConfig, caps: &[Capability]) -> Composition {
        compose_profile(ctx, profile, caps, &BTreeMap::new())
    }

    fn ids(c: &Composition) -> Vec<String> {
        c.capabilities
            .iter()
            .map(|r| r.capability.id.clone())
            .collect()
    }

    // --- capability `when` building blocks (still used for self-gating) -----

    #[test]
    fn rule_stack_equals_matches() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        assert!(rule_matches(&ctx, &rule(Field::Stack, Op::Equals, "rust")));
        assert!(!rule_matches(&ctx, &rule(Field::Stack, Op::Equals, "go")));
    }

    #[test]
    fn rule_path_prefix_and_branch_prefix() {
        let mut ctx = sample_context();
        ctx.cwd = ctx.git.as_ref().unwrap().root.join("infra/db");
        assert!(rule_matches(
            &ctx,
            &rule(Field::Path, Op::StartsWith, "infra/")
        ));
        if let Some(g) = ctx.git.as_mut() {
            g.branch = Some("experiment/foo".into());
        }
        assert!(rule_matches(
            &ctx,
            &rule(Field::Branch, Op::StartsWith, "experiment/")
        ));
    }

    #[test]
    fn rule_regex_match_op() {
        let mut ctx = sample_context();
        if let Some(g) = ctx.git.as_mut() {
            g.branch = Some("release/2026.05".into());
        }
        assert!(rule_matches(
            &ctx,
            &rule(Field::Branch, Op::Matches, r"^release/\d{4}\.\d{2}$")
        ));
    }

    // --- selection (pick-one) ----------------------------------------------

    #[test]
    fn select_zero_one_many() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        let none = [prof("go", &["go"], &["x"])];
        let one = [prof("rust", &["rust"], &["x"])];
        let many = [
            prof("rust-kernel", &["rust"], &["x"]),
            prof("rust-browser", &["rust"], &["y"]),
        ];

        assert!(matches!(select(&ctx, &none, None), Selection::None));
        match select(&ctx, &one, None) {
            Selection::Use(p) => assert_eq!(p.name, "rust"),
            other => panic!("expected Use, got {other:?}"),
        }
        match select(&ctx, &many, None) {
            Selection::Ambiguous(cands) => assert_eq!(cands.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn select_machine_target_only_off_repo() {
        let mut ctx = sample_context(); // has git → Repo scope
        ctx.stacks = vec![];
        let machine = [prof("machine", &["machine"], &["x"])];
        // In a repo, `machine` is not a target → no match.
        assert!(matches!(select(&ctx, &machine, None), Selection::None));
        // Off-repo, `machine` is a target → auto-use.
        ctx.git = None;
        assert!(matches!(select(&ctx, &machine, None), Selection::Use(_)));
    }

    #[test]
    fn select_honors_binding_profile_and_none() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        let profs = [
            prof("rust-kernel", &["rust"], &["x"]),
            prof("rust-browser", &["rust"], &["y"]),
        ];
        // A named binding resolves straight to that profile (no prompt) even
        // though 2 match.
        match select(&ctx, &profs, Some(&Binding::Profile("rust-browser".into()))) {
            Selection::Use(p) => assert_eq!(p.name, "rust-browser"),
            other => panic!("expected Use, got {other:?}"),
        }
        // An explicit `None` binding means "no profile here", even with matches.
        assert!(matches!(
            select(&ctx, &profs, Some(&Binding::None)),
            Selection::None
        ));
    }

    #[test]
    fn select_falls_through_when_bound_profile_missing() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        let profs = [prof("rust", &["rust"], &["x"])];
        // Bound to a now-deleted profile → ignore the binding, re-select (1 match).
        match select(&ctx, &profs, Some(&Binding::Profile("gone".into()))) {
            Selection::Use(p) => assert_eq!(p.name, "rust"),
            other => panic!("expected Use, got {other:?}"),
        }
    }

    // --- within-profile composition ----------------------------------------

    #[test]
    fn compose_resolves_in_declaration_order_and_dedups() {
        let caps = vec![cap("a", "A"), cap("b", "B")];
        let p = prof("p", &["rust"], &["a", "b", "a"]);
        let c = compose_t(&sample_context(), &p, &caps);
        assert_eq!(c.profile.as_deref(), Some("p"));
        assert_eq!(ids(&c), vec!["a", "b"]); // duplicate `a` collapsed
    }

    #[test]
    fn compose_resolves_requires_dependencies_first() {
        let mut top = cap("top", "T");
        top.requires = vec!["dep".into()];
        let caps = vec![top, cap("dep", "D")];
        let c = compose_t(&sample_context(), &prof("p", &["rust"], &["top"]), &caps);
        assert_eq!(ids(&c), vec!["dep", "top"]);
        let dep = c
            .capabilities
            .iter()
            .find(|r| r.capability.id == "dep")
            .unwrap();
        assert!(dep.reason.contains("required by 'top'"));
    }

    #[test]
    fn compose_guards_requires_cycles() {
        let mut a = cap("a", "A");
        a.requires = vec!["b".into()];
        let mut b = cap("b", "B");
        b.requires = vec!["a".into()];
        let caps = vec![a, b];
        let c = compose_t(&sample_context(), &prof("p", &["rust"], &["a"]), &caps);
        let got = ids(&c);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"a".to_string()) && got.contains(&"b".to_string()));
    }

    #[test]
    fn compose_filters_capability_when() {
        let mut gated = cap("gated", "G");
        gated.when = vec![rule(Field::Stack, Op::Equals, "go")];
        let caps = vec![gated, cap("always", "A")];
        let p = prof("p", &["rust"], &["gated", "always"]);

        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        assert_eq!(ids(&compose_t(&ctx, &p, &caps)), vec!["always"]);

        ctx.stacks = vec!["go".into()];
        assert_eq!(ids(&compose_t(&ctx, &p, &caps)), vec!["gated", "always"]);
    }

    #[test]
    fn compose_unknown_capability_is_skipped() {
        let caps = vec![cap("real", "R")];
        let p = prof("p", &["rust"], &["does-not-exist", "real"]);
        let c = compose_t(&sample_context(), &p, &caps);
        assert_eq!(ids(&c), vec!["real"]);
    }

    #[test]
    fn compose_back_compat_inline_guidance_follows_explicit() {
        let caps = vec![cap("a", "A")];
        let mut p = prof("p", &["rust"], &["a"]);
        p.guidance = Some("note".into());
        let c = compose_t(&sample_context(), &p, &caps);
        assert_eq!(ids(&c), vec!["a", "p:inline"]);
        assert!(c.capabilities.last().unwrap().inline);
    }

    #[test]
    fn compose_merges_params_defaults_then_ref_then_private() {
        let mut ssh = cap("ssh", "ssh {{ params.user }}@{{ params.host }}");
        ssh.params = toml::from_str("user = \"root\"\nport = 22\n").unwrap();
        let mut p = prof("p", &["rust"], &[]);
        p.capabilities = vec![CapabilityRef::Detailed {
            id: "ssh".into(),
            params: Some(toml::from_str("user = \"deploy\"").unwrap()),
        }];
        let mut private = BTreeMap::new();
        private.insert(
            "ssh".to_string(),
            toml::from_str("host = \"box.local\"\nport = 2222\n").unwrap(),
        );

        let c = compose_profile(&sample_context(), &p, &[ssh], &private);
        let params = &c.capabilities[0].capability.params;
        assert_eq!(params.get("user").unwrap().as_str(), Some("deploy")); // ref > default
        assert_eq!(params.get("host").unwrap().as_str(), Some("box.local")); // private adds
        assert_eq!(params.get("port").unwrap().as_integer(), Some(2222)); // private > default
    }

    #[test]
    fn compose_selection_none_is_empty_overlay() {
        let c = compose_selection(&sample_context(), &Selection::None, &[], &BTreeMap::new());
        assert!(c.profile.is_none());
        assert!(c.capabilities.is_empty());
        assert_eq!(c.label(), "none");
    }
}
