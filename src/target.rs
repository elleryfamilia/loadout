//! Targets — the labels that decide which profile applies to a detected project.
//!
//! A **target** is an id (e.g. `rust`, `deno`) plus the rule that detects it.
//! Profiles match a detected context through their `targets` list (see
//! [`crate::profile`]).
//!
//! The built-in targets (`rust`/`node`/`nextjs`/`go`/`python`) are detected in
//! Rust by [`crate::context::languages`]; here they are exposed only as
//! read-only **descriptors** ([`builtin_targets`]) so the studio can show how
//! each one works. **Custom** targets, authored in the global config as
//! `[[targets]]`, carry an *evaluable* rule — declarative (filesystem probes,
//! evaluated before stacks exist, so they never test the context they help
//! produce) or a script predicate (the escape hatch, run in the repo).
//!
//! Custom-target matches feed a dedicated `custom_targets` list on the detected
//! context, kept separate from `stacks` so the built-in stack→commands mapping
//! stays clean.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fragment::Layer;
use crate::profile::Op;

/// Serde default for [`TargetRule::Script::allow_exec`] (execution on unless disabled).
fn default_true() -> bool {
    true
}

/// `skip_serializing_if` for `allow_exec` — only persist the off-switch.
fn is_true(b: &bool) -> bool {
    *b
}

/// A detection target: an id plus the rule that decides it applies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetDef {
    /// The label matched against `profiles[].targets` (e.g. `rust`, `deno`).
    pub id: String,
    /// Human summary; doubles as the studio row title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How this target is detected.
    pub rule: TargetRule,
    /// Off-switch: kept in config, never evaluated. Only serialized when set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Which config layer defined it (set at load, not deserialized) — drives
    /// global-only enforcement, like [`crate::fragment::Fragment::origin`].
    #[serde(skip)]
    pub origin: Layer,
}

/// How a target is detected. The declarative variants probe the repo
/// **filesystem** (cheap, pure, evaluated before stacks exist — so they can
/// never test `stack`, the thing detection produces); `Script` is the escape
/// hatch for logic the declarative rules can't express.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetRule {
    /// A repo-relative file or directory exists (matched literally, no glob).
    FileExists { path: String },
    /// A repo-relative file exists and its contents satisfy `op`/`value`.
    FileContains { path: String, op: Op, value: String },
    /// Every sub-rule matches.
    AllOf { rules: Vec<TargetRule> },
    /// At least one sub-rule matches.
    AnyOf { rules: Vec<TargetRule> },
    /// Escape hatch: run a script in the repo; exit 0 ⇒ match. Evaluated by the
    /// dedicated script path (cwd = repo base, bounded timeout, cached).
    Script {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_lang: Option<String>,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        allow_exec: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<String>,
    },
}

impl TargetRule {
    /// Whether this rule (or any nested rule) is a script predicate — i.e. its
    /// evaluation may execute code.
    pub fn has_script(&self) -> bool {
        match self {
            TargetRule::Script { .. } => true,
            TargetRule::AllOf { rules } | TargetRule::AnyOf { rules } => {
                rules.iter().any(|r| r.has_script())
            }
            _ => false,
        }
    }

    /// Evaluate the **declarative** part of this rule against `repo_base`. A
    /// `Script` node evaluates to `false` here — script predicates are resolved
    /// separately (they need exec + caching, which detection threads in). A
    /// composite containing a script still evaluates its declarative siblings.
    pub fn declarative_match(&self, repo_base: &Path) -> bool {
        match self {
            TargetRule::FileExists { path } => {
                safe_join(repo_base, path).is_some_and(|p| p.exists())
            }
            TargetRule::FileContains { path, op, value } => safe_join(repo_base, path)
                .and_then(|p| read_capped(&p))
                .is_some_and(|text| op_matches(*op, &text, value)),
            TargetRule::AllOf { rules } => rules.iter().all(|r| r.declarative_match(repo_base)),
            TargetRule::AnyOf { rules } => rules.iter().any(|r| r.declarative_match(repo_base)),
            TargetRule::Script { .. } => false,
        }
    }
}

/// Resolve `rel` under `repo_base`, rejecting absolute paths and any `..`
/// component so a target rule can only ever probe inside the repo. Returns the
/// joined path, or `None` if `rel` tries to escape.
fn safe_join(repo_base: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return None;
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(repo_base.join(p))
}

/// Read a file for content matching, capped so a huge file can't stall the hot
/// detection path. Returns `None` if absent or unreadable.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    const CAP: u64 = 1 << 20; // 1 MiB is ample for a manifest/lockfile probe.
    let file = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    file.take(CAP).read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Apply a comparison operator (mirrors fragment `when` semantics).
fn op_matches(op: Op, haystack: &str, needle: &str) -> bool {
    match op {
        Op::Equals => haystack == needle,
        Op::StartsWith => haystack.starts_with(needle),
        Op::Contains => haystack.contains(needle),
        Op::Matches => regex::Regex::new(needle).is_ok_and(|re| re.is_match(haystack)),
    }
}

/// The built-in target catalog: a read-only descriptor per code-detected target.
/// These are **not** evaluated through [`TargetRule::declarative_match`] at
/// detection time (the authoritative detection lives in
/// [`crate::context::languages`]); they exist so the studio can render how each
/// built-in is detected, uniformly with custom targets.
pub fn builtin_targets() -> Vec<TargetDef> {
    fn t(id: &str, description: &str, rule: TargetRule) -> TargetDef {
        TargetDef {
            id: id.to_string(),
            description: Some(description.to_string()),
            rule,
            disabled: false,
            origin: Layer::BuiltIn,
        }
    }
    fn file(path: &str) -> TargetRule {
        TargetRule::FileExists {
            path: path.to_string(),
        }
    }
    vec![
        t(
            "rust",
            "a Cargo manifest at the repo root",
            file("Cargo.toml"),
        ),
        t(
            "node",
            "a package.json at the repo root",
            file("package.json"),
        ),
        t(
            "nextjs",
            "package.json names `next` as a dependency",
            TargetRule::AllOf {
                rules: vec![
                    file("package.json"),
                    TargetRule::FileContains {
                        path: "package.json".to_string(),
                        op: Op::Contains,
                        value: "\"next\"".to_string(),
                    },
                ],
            },
        ),
        t(
            "go",
            "a Go module (go.mod) at the repo root",
            file("go.mod"),
        ),
        t(
            "python",
            "a Python project marker at the root",
            TargetRule::AnyOf {
                rules: vec![
                    file("pyproject.toml"),
                    file("requirements.txt"),
                    file("setup.py"),
                    file("Pipfile"),
                ],
            },
        ),
    ]
}

/// A plain-language, one-line summary of a detection rule — the studio's "how
/// this target works" text.
pub fn rule_summary(rule: &TargetRule) -> String {
    match rule {
        TargetRule::FileExists { path } => format!("{path} exists"),
        TargetRule::FileContains { path, op, value } => {
            let verb = match op {
                Op::Equals => "equals",
                Op::StartsWith => "starts with",
                Op::Contains => "contains",
                Op::Matches => "matches",
            };
            format!("{path} {verb} {value}")
        }
        TargetRule::AllOf { rules } => rules
            .iter()
            .map(rule_summary)
            .collect::<Vec<_>>()
            .join(" and "),
        TargetRule::AnyOf { rules } => rules
            .iter()
            .map(rule_summary)
            .collect::<Vec<_>>()
            .join(" or "),
        TargetRule::Script { script_lang, .. } => {
            let lang = script_lang.as_deref().unwrap_or("shell");
            format!("runs a {lang} script (exit 0 = match)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn builtin_catalog_is_well_formed() {
        let ids: Vec<String> = builtin_targets().into_iter().map(|t| t.id).collect();
        for needed in ["rust", "node", "nextjs", "go", "python"] {
            assert!(
                ids.contains(&needed.to_string()),
                "missing built-in {needed}"
            );
        }
        // Every built-in carries a description (the studio row title).
        for t in builtin_targets() {
            assert!(t.description.is_some(), "{} lacks a description", t.id);
        }
    }

    #[test]
    fn file_exists_rule_matches() {
        let d = tmp();
        fs::write(d.path().join("Cargo.toml"), "x").unwrap();
        let r = TargetRule::FileExists {
            path: "Cargo.toml".into(),
        };
        assert!(r.declarative_match(d.path()));
        let miss = TargetRule::FileExists {
            path: "go.mod".into(),
        };
        assert!(!miss.declarative_match(d.path()));
    }

    #[test]
    fn nextjs_descriptor_matches_real_package_json() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"dependencies":{"next":"14"}}"#,
        )
        .unwrap();
        let nextjs = builtin_targets()
            .into_iter()
            .find(|t| t.id == "nextjs")
            .unwrap();
        assert!(nextjs.rule.declarative_match(d.path()));
        // Plain node (no next) doesn't match the nextjs descriptor.
        fs::write(d.path().join("package.json"), r#"{"dependencies":{}}"#).unwrap();
        assert!(!nextjs.rule.declarative_match(d.path()));
    }

    #[test]
    fn python_any_of_matches_each_marker() {
        let python = builtin_targets()
            .into_iter()
            .find(|t| t.id == "python")
            .unwrap();
        for marker in ["pyproject.toml", "requirements.txt", "setup.py", "Pipfile"] {
            let d = tmp();
            fs::write(d.path().join(marker), "x").unwrap();
            assert!(python.rule.declarative_match(d.path()), "marker {marker}");
        }
        let d = tmp();
        assert!(
            !python.rule.declarative_match(d.path()),
            "empty dir matches nothing"
        );
    }

    #[test]
    fn path_traversal_is_rejected() {
        let d = tmp();
        // A secret outside the repo must never be reachable via `..`.
        fs::write(d.path().join("outside"), "secret").unwrap();
        let sub = d.path().join("repo");
        fs::create_dir_all(&sub).unwrap();
        let escape = TargetRule::FileExists {
            path: "../outside".into(),
        };
        assert!(
            !escape.declarative_match(&sub),
            "must not escape the repo base"
        );
        let abs = TargetRule::FileExists {
            path: "/etc/hosts".into(),
        };
        assert!(!abs.declarative_match(&sub), "absolute paths rejected");
    }

    #[test]
    fn has_script_detects_nested_predicate() {
        let declarative = TargetRule::AnyOf {
            rules: vec![TargetRule::FileExists { path: "x".into() }],
        };
        assert!(!declarative.has_script());
        let scripted = TargetRule::AllOf {
            rules: vec![TargetRule::Script {
                command: "true".into(),
                script_lang: Some("bash".into()),
                allow_exec: true,
                cache: None,
            }],
        };
        assert!(scripted.has_script());
    }

    #[test]
    fn rule_summary_is_readable() {
        assert_eq!(
            rule_summary(&TargetRule::FileExists {
                path: "go.mod".into()
            }),
            "go.mod exists"
        );
        let nextjs = builtin_targets()
            .into_iter()
            .find(|t| t.id == "nextjs")
            .unwrap();
        assert_eq!(
            rule_summary(&nextjs.rule),
            "package.json exists and package.json contains \"next\""
        );
    }
}
