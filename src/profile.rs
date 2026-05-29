//! Rule-based profiles and **additive capability composition**.
//!
//! A profile maps context → guidance via `when` rules (all clauses AND-ed; an
//! empty rule set always matches, acting as a fallback). Instead of selecting a
//! single winner, [`compose`] takes **every** matching profile and unions the
//! [`Capability`](crate::capability::Capability)s they pull in — deduped by id,
//! priority-ordered, `requires`-resolved, per-capability `when`-filtered, and
//! `exclude`-applied. An `exclusive` profile can still replace rather than add.
//!
//! This is what lets context layer instead of fight: "in `infra/` I get infra
//! caution, on a Rust repo I get Rust conventions, everywhere I get the
//! baseline" all compose into one overlay.
//!
//! Back-compat: a profile's inline `guidance` is treated as an implicit
//! `<profile>:inline` capability, appended after that profile's explicit
//! capabilities.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::context::Context;

/// A configured profile: the conditions under which it applies and the
/// capabilities (and/or inline guidance) it contributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Profile name (also used to find `profiles/<name>.md.j2` templates).
    pub name: String,
    /// Conditions; all must match. Empty = always matches (fallback).
    #[serde(default)]
    pub when: Vec<Rule>,
    /// Higher wins for ordering and exclusivity among multiple matches.
    #[serde(default)]
    pub priority: i32,
    /// Capability ids this profile composes (in declaration order).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Capability ids to suppress across the whole composition.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// When set, this profile *replaces* (rather than adds to) the composition:
    /// if any matching profile is exclusive, only the highest-priority exclusive
    /// one contributes.
    #[serde(default)]
    pub exclusive: bool,
    /// Optional base-template override (per agent the renderer appends the
    /// agent suffix). Rarely needed.
    #[serde(default)]
    pub template: Option<String>,
    /// Inline guidance markdown (back-compat; becomes a `<profile>:inline`
    /// capability appended after the explicit ones).
    #[serde(default)]
    pub guidance: Option<String>,
}

/// A single match condition.
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

/// Context fields that rules can match against.
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
    /// Human-readable provenance, e.g. "via profile 'rust' (Stack equals …)".
    pub reason: String,
    /// True for a synthetic `<profile>:inline` capability — enables the
    /// `profiles/<name>.md.j2` template-file override at render time.
    pub inline: bool,
}

/// The outcome of composition: the matching profiles, the ordered/deduped
/// capabilities they contribute, and human-readable reasons.
#[derive(Debug, Clone, Default)]
pub struct Composition {
    /// Matching profile names, priority order (highest first).
    pub profiles: Vec<String>,
    /// Resolved capabilities, in render order.
    pub capabilities: Vec<ResolvedCapability>,
    /// Provenance lines (one per contributing capability).
    pub reasons: Vec<String>,
}

impl Composition {
    /// The primary (highest-priority) matching profile, used as the display /
    /// audit label and for the base-template override. `None` only if nothing
    /// matched (callers ship an always-matching `default`, so in practice
    /// `Some`).
    pub fn primary_profile(&self) -> Option<&str> {
        self.profiles.first().map(String::as_str)
    }

    /// The display label for the composition (primary profile, or `none`).
    pub fn label(&self) -> &str {
        self.primary_profile().unwrap_or("none")
    }
}

/// Compose every matching profile's capabilities into one ordered set.
///
/// Algorithm:
/// 1. Collect matching profiles; order by priority desc, then declaration.
/// 2. If any match is `exclusive`, keep only the highest-priority exclusive.
/// 3. For each profile in order, add its `capabilities` (skipping any already
///    added or in any selected profile's `exclude`), expanding `requires`
///    depth-first (dependencies first) with cycle protection, and filtering
///    each capability by its own `when`. Then append the profile's inline
///    guidance as a synthetic capability.
///
/// Agent restriction (`Capability::agents`) is intentionally **not** applied
/// here — the active agent varies per render, so it is applied at render time.
pub fn compose(
    ctx: &Context,
    profiles: &[ProfileConfig],
    capabilities: &[Capability],
) -> Composition {
    // 1. Matching profiles + their per-rule reasons.
    let mut matching: Vec<(&ProfileConfig, Vec<String>)> = profiles
        .iter()
        .filter_map(|p| matches(ctx, p).map(|r| (p, r)))
        .collect();
    // Priority desc; stable sort keeps declaration order for ties.
    matching.sort_by(|a, b| b.0.priority.cmp(&a.0.priority));

    // 2. Exclusivity: the first exclusive (now highest-priority) replaces all.
    if let Some(pos) = matching.iter().position(|(p, _)| p.exclusive) {
        let chosen = matching.remove(pos);
        matching = vec![chosen];
    }

    let profile_names: Vec<String> = matching.iter().map(|(p, _)| p.name.clone()).collect();

    // Union of every selected profile's exclude list.
    let exclude: HashSet<&str> = matching
        .iter()
        .flat_map(|(p, _)| p.exclude.iter().map(String::as_str))
        .collect();

    let lib: BTreeMap<&str, &Capability> =
        capabilities.iter().map(|c| (c.id.as_str(), c)).collect();

    let mut acc = Accumulator::default();
    for (p, prule) in &matching {
        let provenance = format!("via profile '{}' ({})", p.name, prule.join(", "));
        for cap_id in &p.capabilities {
            acc.add(
                cap_id,
                &p.name,
                &provenance,
                &lib,
                &exclude,
                ctx,
                &mut HashSet::new(),
            );
        }
        // Back-compat: inline guidance as a synthetic, always-on capability,
        // appended after this profile's explicit capabilities.
        if let Some(text) = &p.guidance {
            let inline = Capability::inline(&p.name, text.clone());
            if exclude.contains(inline.id.as_str()) || !acc.added.insert(inline.id.clone()) {
                continue;
            }
            let reason = format!("inline guidance via profile '{}'", p.name);
            acc.reasons.push(reason.clone());
            acc.resolved.push(ResolvedCapability {
                capability: inline,
                via_profile: p.name.clone(),
                reason,
                inline: true,
            });
        }
    }

    Composition {
        profiles: profile_names,
        capabilities: acc.resolved,
        reasons: acc.reasons,
    }
}

/// Mutable state threaded through capability resolution.
#[derive(Default)]
struct Accumulator {
    resolved: Vec<ResolvedCapability>,
    added: HashSet<String>,
    reasons: Vec<String>,
}

impl Accumulator {
    /// Resolve `id` (and its `requires`, dependencies first) into the set.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        id: &str,
        via_profile: &str,
        provenance: &str,
        lib: &BTreeMap<&str, &Capability>,
        exclude: &HashSet<&str>,
        ctx: &Context,
        in_progress: &mut HashSet<String>,
    ) {
        if exclude.contains(id) || self.added.contains(id) {
            return;
        }
        if !in_progress.insert(id.to_string()) {
            crate::warn_user!("capability dependency cycle at '{id}' — skipping");
            return;
        }
        let outcome = match lib.get(id) {
            None => {
                crate::warn_user!("unknown capability '{id}' ({provenance})");
                None
            }
            Some(cap) if !capability_applies(ctx, cap) => None, // `when` not satisfied
            Some(cap) => Some(*cap),
        };
        if let Some(cap) = outcome {
            // Dependencies first, so they render before the dependent.
            for dep in &cap.requires {
                let dep_provenance = format!("required by '{id}'");
                self.add(
                    dep,
                    via_profile,
                    &dep_provenance,
                    lib,
                    exclude,
                    ctx,
                    in_progress,
                );
            }
            self.added.insert(id.to_string());
            let reason = format!("capability '{id}' {provenance}");
            self.reasons.push(reason.clone());
            self.resolved.push(ResolvedCapability {
                capability: cap.clone(),
                via_profile: via_profile.to_string(),
                reason,
                inline: false,
            });
        }
        in_progress.remove(id);
    }
}

/// If `profile` matches `ctx`, return the per-rule reasons; else `None`.
pub fn matches(ctx: &Context, profile: &ProfileConfig) -> Option<Vec<String>> {
    if profile.when.is_empty() {
        return Some(vec!["fallback profile (no rules)".to_string()]);
    }
    let mut reasons = Vec::with_capacity(profile.when.len());
    for rule in &profile.when {
        if rule_matches(ctx, rule) {
            reasons.push(describe(rule));
        } else {
            return None; // AND semantics: one failure disqualifies.
        }
    }
    Some(reasons)
}

/// Whether a capability's own `when` gate is satisfied (empty = always).
fn capability_applies(ctx: &Context, cap: &Capability) -> bool {
    cap.when.is_empty() || cap.when.iter().all(|r| rule_matches(ctx, r))
}

fn describe(rule: &Rule) -> String {
    let op = match rule.op {
        Op::Equals => "equals",
        Op::StartsWith => "starts with",
        Op::Contains => "contains",
        Op::Matches => "matches",
    };
    format!("{:?} {} {:?}", rule.field, op, rule.value)
}

fn rule_matches(ctx: &Context, rule: &Rule) -> bool {
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
                crate::warn_user!("invalid regex {value:?} in profile rule: {e}");
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

/// The built-in profiles, always present as a base layer (overridable by name
/// in user config). They reference built-in capabilities rather than carrying
/// inline guidance; `default` always matches and contributes the baseline.
pub fn builtin_profiles() -> Vec<ProfileConfig> {
    fn rule(field: Field, op: Op, value: &str) -> Rule {
        Rule {
            field,
            op,
            value: value.to_string(),
        }
    }
    fn p(name: &str, when: Vec<Rule>, priority: i32, capabilities: &[&str]) -> ProfileConfig {
        ProfileConfig {
            name: name.to_string(),
            when,
            priority,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            exclude: Vec::new(),
            exclusive: false,
            template: None,
            guidance: None,
        }
    }

    vec![
        // Path/branch profiles are most specific → highest priority.
        p(
            "infra",
            vec![rule(Field::Path, Op::StartsWith, "infra/")],
            50,
            &["infra-caution"],
        ),
        p(
            "experimental",
            vec![rule(Field::Branch, Op::StartsWith, "experiment/")],
            40,
            &["experimental-iteration"],
        ),
        // Stack profiles.
        p(
            "rust",
            vec![rule(Field::Stack, Op::Equals, "rust")],
            20,
            &["rust-conventions"],
        ),
        p(
            "nextjs",
            vec![rule(Field::Stack, Op::Equals, "nextjs")],
            25,
            &["nextjs-conventions"],
        ),
        p(
            "node",
            vec![rule(Field::Stack, Op::Equals, "node")],
            20,
            &["node-conventions"],
        ),
        p(
            "go",
            vec![rule(Field::Stack, Op::Equals, "go")],
            20,
            &["go-conventions"],
        ),
        p(
            "python",
            vec![rule(Field::Stack, Op::Equals, "python")],
            20,
            &["python-conventions"],
        ),
        // Always-on baseline.
        p("default", vec![], 0, &["baseline"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{builtin_capabilities, Risk};
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
        }
    }

    fn rule(field: Field, op: Op, value: &str) -> Rule {
        Rule {
            field,
            op,
            value: value.into(),
        }
    }

    fn prof(name: &str, priority: i32, when: Vec<Rule>, caps: &[&str]) -> ProfileConfig {
        ProfileConfig {
            name: name.into(),
            when,
            priority,
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            exclude: vec![],
            exclusive: false,
            template: None,
            guidance: None,
        }
    }

    fn ids(c: &Composition) -> Vec<String> {
        c.capabilities
            .iter()
            .map(|r| r.capability.id.clone())
            .collect()
    }

    #[test]
    fn empty_rules_is_fallback() {
        let ctx = sample_context();
        let profile = prof("default", 0, vec![], &[]);
        assert!(matches(&ctx, &profile).is_some());
    }

    #[test]
    fn stack_equals_matches() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        assert!(rule_matches(&ctx, &rule(Field::Stack, Op::Equals, "rust")));
        assert!(!rule_matches(&ctx, &rule(Field::Stack, Op::Equals, "go")));
    }

    #[test]
    fn path_prefix_and_branch_prefix() {
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
    fn regex_match_op() {
        let mut ctx = sample_context();
        if let Some(g) = ctx.git.as_mut() {
            g.branch = Some("release/2026.05".into());
        }
        assert!(rule_matches(
            &ctx,
            &rule(Field::Branch, Op::Matches, r"^release/\d{4}\.\d{2}$")
        ));
    }

    // --- composition ------------------------------------------------------

    #[test]
    fn compose_is_additive_and_priority_ordered() {
        // A Rust repo in infra/ matches infra(50) + rust(20) + default(0):
        // all three contribute, ordered by priority.
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        ctx.cwd = ctx.git.as_ref().unwrap().root.join("infra/db");
        let c = compose(&ctx, &builtin_profiles(), &builtin_capabilities());
        assert_eq!(c.profiles, vec!["infra", "rust", "default"]);
        assert_eq!(
            ids(&c),
            vec!["infra-caution", "rust-conventions", "baseline"]
        );
        // The infra capability is flagged Caution.
        let infra = &c.capabilities[0];
        assert_eq!(infra.capability.risk, Risk::Caution);
    }

    #[test]
    fn compose_dedups_by_id_keeping_highest_priority() {
        let caps = vec![cap("shared", "S"), cap("a", "A"), cap("b", "B")];
        let profiles = vec![
            prof("hi", 10, vec![], &["shared", "a"]),
            prof("lo", 1, vec![], &["shared", "b"]),
        ];
        let c = compose(&sample_context(), &profiles, &caps);
        // `shared` appears once, attributed to the higher-priority profile.
        assert_eq!(ids(&c), vec!["shared", "a", "b"]);
        let shared = c
            .capabilities
            .iter()
            .find(|r| r.capability.id == "shared")
            .unwrap();
        assert_eq!(shared.via_profile, "hi");
    }

    #[test]
    fn compose_applies_exclude_across_profiles() {
        let caps = vec![cap("a", "A"), cap("b", "B")];
        let mut excluder = prof("x", 5, vec![], &["a"]);
        excluder.exclude = vec!["b".into()];
        let profiles = vec![excluder, prof("y", 1, vec![], &["a", "b"])];
        let c = compose(&sample_context(), &profiles, &caps);
        // `b` is excluded everywhere; `a` survives once.
        assert_eq!(ids(&c), vec!["a"]);
    }

    #[test]
    fn compose_resolves_requires_dependencies_first() {
        let mut top = cap("top", "T");
        top.requires = vec!["dep".into()];
        let caps = vec![top, cap("dep", "D")];
        let profiles = vec![prof("p", 1, vec![], &["top"])];
        let c = compose(&sample_context(), &profiles, &caps);
        // Dependency renders before the dependent.
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
        let c = compose(&sample_context(), &[prof("p", 1, vec![], &["a"])], &caps);
        // No panic/infinite loop; both still land exactly once.
        let got = ids(&c);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"a".to_string()) && got.contains(&"b".to_string()));
    }

    #[test]
    fn compose_filters_capability_when() {
        let mut gated = cap("gated", "G");
        gated.when = vec![rule(Field::Stack, Op::Equals, "go")];
        let caps = vec![gated, cap("always", "A")];
        let profiles = vec![prof("p", 1, vec![], &["gated", "always"])];

        // Stack is rust → gated capability is filtered out.
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        assert_eq!(ids(&compose(&ctx, &profiles, &caps)), vec!["always"]);

        // Stack is go → it applies.
        ctx.stacks = vec!["go".into()];
        assert_eq!(
            ids(&compose(&ctx, &profiles, &caps)),
            vec!["gated", "always"]
        );
    }

    #[test]
    fn compose_exclusive_replaces_rather_than_adds() {
        let caps = vec![cap("a", "A"), cap("b", "B"), cap("base", "Base")];
        let mut lockdown = prof("lockdown", 30, vec![], &["b"]);
        lockdown.exclusive = true;
        let profiles = vec![
            prof("infra", 50, vec![], &["a"]), // higher priority, non-exclusive
            lockdown,
            prof("default", 0, vec![], &["base"]),
        ];
        let c = compose(&sample_context(), &profiles, &caps);
        // Only the (highest-priority) exclusive profile contributes.
        assert_eq!(c.profiles, vec!["lockdown"]);
        assert_eq!(ids(&c), vec!["b"]);
    }

    #[test]
    fn compose_back_compat_inline_guidance() {
        let mut p = prof("legacy", 5, vec![], &[]);
        p.guidance = Some("legacy inline guidance".into());
        let c = compose(&sample_context(), &[p], &[]);
        assert_eq!(ids(&c), vec!["legacy:inline"]);
        let inline = &c.capabilities[0];
        assert!(inline.inline);
        assert_eq!(inline.capability.guidance, "legacy inline guidance");
    }

    #[test]
    fn compose_inline_follows_explicit_capabilities() {
        let caps = vec![cap("a", "A")];
        let mut p = prof("p", 5, vec![], &["a"]);
        p.guidance = Some("note".into());
        let c = compose(&sample_context(), &[p], &caps);
        assert_eq!(ids(&c), vec!["a", "p:inline"]);
    }

    #[test]
    fn compose_unknown_capability_is_skipped() {
        let profiles = vec![prof("p", 1, vec![], &["does-not-exist", "real"])];
        let caps = vec![cap("real", "R")];
        let c = compose(&sample_context(), &profiles, &caps);
        assert_eq!(ids(&c), vec!["real"]);
    }

    #[test]
    fn default_only_when_nothing_else_matches() {
        let ctx = sample_context(); // no stack, not in infra/
        let c = compose(&ctx, &builtin_profiles(), &builtin_capabilities());
        assert_eq!(c.profiles, vec!["default"]);
        assert_eq!(ids(&c), vec!["baseline"]);
        assert_eq!(c.label(), "default");
    }
}
