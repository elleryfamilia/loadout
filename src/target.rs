//! Targets — the labels that decide which profile applies to a detected project.
//!
//! A **target** is an id (e.g. `rust`, `deno`) plus the rule that detects it.
//! Profiles match a detected context through their `targets` list (see
//! [`crate::profile`]).
//!
//! The built-in targets (rust/node/nextjs/go/python/java/ruby/php/swift/dotnet)
//! are detected in Rust by [`crate::context::languages`]; here they are exposed
//! only as read-only **descriptors** ([`builtin_targets`]) so the studio can
//! show how each one works. **Custom** targets, authored in the global config as
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
    /// The studio glyph shown for this target on profiles and the Targets tab.
    /// A name from the studio's closed icon set (e.g. `rust`, `database`); an
    /// unrecognized or absent value renders as a lettermark badge derived from
    /// the id (see [`lettermark`]). Built-ins set a brand glyph; custom targets
    /// pick one (or fall back to the lettermark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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
    /// A repo-relative file or directory exists. A `*` in the final path
    /// component is a glob over that directory's children (e.g. `*.csproj`).
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
            TargetRule::FileExists { path } => file_exists_match(repo_base, path),
            TargetRule::FileContains { path, op, value } => safe_join(repo_base, path)
                .and_then(|p| read_capped(&p))
                .is_some_and(|text| op_matches(*op, &text, value)),
            TargetRule::AllOf { rules } => rules.iter().all(|r| r.declarative_match(repo_base)),
            TargetRule::AnyOf { rules } => rules.iter().any(|r| r.declarative_match(repo_base)),
            TargetRule::Script { .. } => false,
        }
    }
}

/// Whether `path` exists under `repo_base`. A `*` in the final component makes
/// it a glob over that directory's direct children (e.g. `*.csproj`,
/// `src/*.tf`); otherwise it's a literal existence check.
fn file_exists_match(repo_base: &Path, path: &str) -> bool {
    if path.contains('*') {
        glob_exists(repo_base, path)
    } else {
        safe_join(repo_base, path).is_some_and(|p| p.exists())
    }
}

/// Match a glob whose wildcard is confined to the final path component: the
/// directory prefix is literal (and `..`/absolute-guarded), the final component
/// is matched against the directory's direct children.
fn glob_exists(repo_base: &Path, pattern: &str) -> bool {
    let (dir_rel, name_pat) = pattern.rsplit_once('/').unwrap_or(("", pattern));
    let Some(dir) = safe_join(repo_base, dir_rel) else {
        return false;
    };
    std::fs::read_dir(&dir)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .any(|e| wildcard_match(&e.file_name().to_string_lossy(), name_pat))
        })
        .unwrap_or(false)
}

/// A minimal `*`-glob match: literal segments (split on `*`) must appear in
/// order, the first anchored to the start and the last to the end.
fn wildcard_match(name: &str, pattern: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return name == pattern; // no wildcard
    }
    if !name.starts_with(parts[0]) {
        return false;
    }
    let mut pos = parts[0].len();
    for seg in &parts[1..parts.len() - 1] {
        match name[pos..].find(*seg) {
            Some(i) => pos += i + seg.len(),
            None => return false,
        }
    }
    let last = parts[parts.len() - 1];
    name.len() - pos >= last.len() && name[pos..].ends_with(last)
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
    // Each built-in's `icon` comes from `builtin_icon` (the single source of
    // truth) — every built-in resolves to a real brand logo there.
    fn t(id: &str, description: &str, rule: TargetRule) -> TargetDef {
        TargetDef {
            id: id.to_string(),
            description: Some(description.to_string()),
            icon: builtin_icon(id).map(str::to_string),
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
            "bun",
            "a Bun lockfile (bun.lock or bun.lockb) at the repo root",
            TargetRule::AnyOf {
                rules: vec![file("bun.lock"), file("bun.lockb")],
            },
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
        t(
            "java",
            "a Maven or Gradle build file at the repo root",
            TargetRule::AnyOf {
                rules: vec![
                    file("pom.xml"),
                    file("build.gradle"),
                    file("build.gradle.kts"),
                ],
            },
        ),
        t("ruby", "a Gemfile at the repo root", file("Gemfile")),
        t(
            "php",
            "a composer.json at the repo root",
            file("composer.json"),
        ),
        t(
            "swift",
            "a Swift package, Xcode project, or Podfile",
            TargetRule::AnyOf {
                rules: vec![
                    file("Package.swift"),
                    file("*.xcodeproj"),
                    file("*.xcworkspace"),
                    file("Podfile"),
                ],
            },
        ),
        t(
            "dotnet",
            "a .NET solution or project at the repo root",
            TargetRule::AnyOf {
                rules: vec![file("*.sln"), file("*.csproj"), file("global.json")],
            },
        ),
    ]
}

/// The studio icon name for a *built-in* target id (or the synthetic `machine`
/// scope). Lets view code resolve a built-in's icon without threading the whole
/// config through. Every language/framework built-in has a real brand logo (the
/// studio renders it filled); `machine` uses a line glyph. Custom ids return
/// `None`, so the caller falls back to a [`lettermark`].
pub fn builtin_icon(id: &str) -> Option<&'static str> {
    match id {
        "rust" => Some("rust"),
        "node" => Some("node"),
        "bun" => Some("bun"),
        "nextjs" => Some("nextjs"),
        "go" => Some("go"),
        "python" => Some("python"),
        "java" => Some("java"),
        "ruby" => Some("ruby"),
        "php" => Some("php"),
        "swift" => Some("swift"),
        "dotnet" => Some("dotnet"),
        // A CPU chip reads as "the machine/host"; deliberately *not* `monitor`,
        // which the theme toggle already uses for its auto glyph.
        "machine" => Some("cpu"),
        _ => None,
    }
}

/// A short uppercase lettermark for a target id, used as the badge when a target
/// has no glyph icon (the default for custom targets). A hyphen/underscore id
/// takes the first letter of its first two words (`my-stack` → `MS`); otherwise
/// the first two alphanumeric characters (`deno` → `DE`). Falls back to `?` for
/// an id with no alphanumerics.
pub fn lettermark(id: &str) -> String {
    let words: Vec<&str> = id
        .split(['-', '_', ' '])
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .collect();
    let mark: String = if words.len() >= 2 {
        words
            .iter()
            .take(2)
            .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
            .collect()
    } else {
        id.chars().filter(|c| c.is_alphanumeric()).take(2).collect()
    };
    if mark.is_empty() {
        "?".to_string()
    } else {
        mark.to_uppercase()
    }
}

/// Ids a custom target may not claim: the built-in stacks (read-only, detected
/// in Rust) and the `machine` scope (derived, not file-detected).
pub fn reserved_target_ids() -> std::collections::HashSet<String> {
    builtin_targets()
        .into_iter()
        .map(|t| t.id)
        .chain(std::iter::once("machine".to_string()))
        .collect()
}

/// Default cache TTL for a script-predicate target verdict. Targets change far
/// less often than environment probes, so this is generous — a live render
/// re-checks at most this often.
const DEFAULT_SCRIPT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Evaluate the user's `custom` targets against `repo_base`, returning the ids
/// that matched (deduped, in declaration order). Disabled targets and any whose
/// id is reserved (a built-in stack or `machine`) are skipped — built-ins are
/// detected in Rust and are not overridable.
///
/// `live` controls script predicates: `true` executes them (cwd = repo, bounded
/// timeout, verdict cached); `false` serves only a cached verdict and never
/// executes — so read-only commands (`explain`, `detect`, …) stay side-effect
/// free. Declarative rules are always evaluated (pure filesystem reads).
pub fn detect_custom(custom: &[TargetDef], repo_base: &Path, live: bool) -> Vec<String> {
    let reserved = reserved_target_ids();
    let mut matched: Vec<String> = Vec::new();
    for t in custom {
        if t.disabled || reserved.contains(&t.id) || matched.contains(&t.id) {
            continue;
        }
        if eval_rule(&t.rule, &t.id, repo_base, live) {
            matched.push(t.id.clone());
        }
    }
    matched
}

/// Evaluate one rule, executing any script predicate via the cached, bounded
/// predicate runner (under the `live` policy above).
fn eval_rule(rule: &TargetRule, id: &str, repo_base: &Path, live: bool) -> bool {
    match rule {
        TargetRule::FileExists { .. } | TargetRule::FileContains { .. } => {
            rule.declarative_match(repo_base)
        }
        TargetRule::AllOf { rules } => rules.iter().all(|r| eval_rule(r, id, repo_base, live)),
        TargetRule::AnyOf { rules } => rules.iter().any(|r| eval_rule(r, id, repo_base, live)),
        TargetRule::Script {
            command,
            script_lang,
            allow_exec,
            cache,
        } => {
            if !*allow_exec {
                return false; // off-switch → fail-closed
            }
            let ttl = cache
                .as_deref()
                .and_then(crate::providers::parse_duration)
                .unwrap_or(DEFAULT_SCRIPT_TTL);
            // Key by target id + a hash of the script body, so editing the
            // script busts the cached verdict.
            let key = format!(
                "target-{}-{}",
                id,
                crate::hash::context_hash(&(command, script_lang))
            );
            crate::providers::run_predicate(
                command,
                script_lang.as_deref(),
                repo_base,
                &key,
                ttl,
                chrono::Utc::now(),
                live,
            )
            .unwrap_or(false)
        }
    }
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
        for needed in [
            "rust", "node", "bun", "nextjs", "go", "python", "java", "ruby", "php", "swift",
            "dotnet",
        ] {
            assert!(
                ids.contains(&needed.to_string()),
                "missing built-in {needed}"
            );
        }
        // Every built-in carries a description (the studio row title) and a brand
        // icon, and each one's `icon` agrees with `builtin_icon` (the single
        // source of truth).
        for t in builtin_targets() {
            assert!(t.description.is_some(), "{} lacks a description", t.id);
            assert!(t.icon.is_some(), "{} lacks a brand icon", t.id);
            assert_eq!(
                builtin_icon(&t.id),
                t.icon.as_deref(),
                "builtin_icon disagrees with the catalog for {}",
                t.id
            );
        }
        // Representative built-ins + the synthetic machine scope; a custom id
        // has no built-in icon (it falls back to a lettermark).
        assert_eq!(builtin_icon("rust"), Some("rust"));
        assert_eq!(builtin_icon("python"), Some("python"));
        assert_eq!(builtin_icon("machine"), Some("cpu"));
        assert_eq!(builtin_icon("deno"), None);
    }

    #[test]
    fn lettermark_derives_a_short_badge() {
        assert_eq!(lettermark("deno"), "DE");
        assert_eq!(lettermark("my-stack"), "MS");
        assert_eq!(lettermark("infra_tools"), "IT");
        assert_eq!(lettermark("x"), "X");
        // Non-alphanumeric ids degrade to a placeholder rather than empty.
        assert_eq!(lettermark("***"), "?");
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
    fn bun_descriptor_matches_either_lockfile() {
        let bun = builtin_targets()
            .into_iter()
            .find(|t| t.id == "bun")
            .unwrap();
        for lock in ["bun.lock", "bun.lockb"] {
            let d = tmp();
            fs::write(d.path().join(lock), "").unwrap();
            assert!(bun.rule.declarative_match(d.path()), "should match {lock}");
        }
        // A plain node repo (no bun lockfile) doesn't match the bun descriptor.
        let d = tmp();
        fs::write(d.path().join("package.json"), "{}").unwrap();
        assert!(!bun.rule.declarative_match(d.path()));
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
    fn file_exists_supports_globs() {
        let d = tmp();
        fs::write(d.path().join("App.csproj"), "<Project/>").unwrap();
        let glob = TargetRule::FileExists {
            path: "*.csproj".into(),
        };
        assert!(glob.declarative_match(d.path()));
        let miss = TargetRule::FileExists {
            path: "*.sln".into(),
        };
        assert!(!miss.declarative_match(d.path()));
        // The dotnet built-in matches via the .csproj glob.
        let dotnet = builtin_targets()
            .into_iter()
            .find(|t| t.id == "dotnet")
            .unwrap();
        assert!(dotnet.rule.declarative_match(d.path()));
        // A glob can't escape the repo base.
        let escape = TargetRule::FileExists {
            path: "../*.csproj".into(),
        };
        assert!(!escape.declarative_match(d.path()));
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
    fn detect_custom_matches_and_filters() {
        let d = tmp();
        fs::write(d.path().join("deno.json"), "{}").unwrap();
        let mk = |id: &str, disabled: bool| TargetDef {
            id: id.to_string(),
            description: None,
            icon: None,
            rule: TargetRule::FileExists {
                path: "deno.json".into(),
            },
            disabled,
            origin: Layer::Global,
        };
        let targets = vec![
            mk("deno", false),    // matches
            mk("rust", false),    // reserved built-in id → ignored
            mk("machine", false), // reserved scope → ignored
            mk("off", true),      // disabled → ignored
        ];
        assert_eq!(
            detect_custom(&targets, d.path(), false),
            vec!["deno".to_string()]
        );
        // No matching file → nothing detected.
        let empty = tmp();
        assert!(detect_custom(&targets, empty.path(), false).is_empty());
    }

    #[test]
    fn detect_custom_runs_script_predicate_live() {
        let d = tmp();
        let scripted = |allow_exec: bool| TargetDef {
            id: "scripted".to_string(),
            description: None,
            icon: None,
            rule: TargetRule::Script {
                command: "test -f marker".to_string(),
                script_lang: Some("bash".to_string()),
                allow_exec,
                cache: Some("0s".to_string()), // re-run each call (no fresh cache)
            },
            disabled: false,
            origin: Layer::Global,
        };
        let on = scripted(true);
        // No marker → exit 1 → no match.
        assert!(detect_custom(std::slice::from_ref(&on), d.path(), true).is_empty());
        // Marker present → exit 0 → match.
        fs::write(d.path().join("marker"), "").unwrap();
        assert_eq!(
            detect_custom(std::slice::from_ref(&on), d.path(), true),
            vec!["scripted".to_string()]
        );
        // allow_exec = false → never matches, even with the marker.
        let off = scripted(false);
        assert!(detect_custom(std::slice::from_ref(&off), d.path(), true).is_empty());
        // Cache-only (live = false) with a cold cache → fail-closed (no match),
        // even though the marker is present.
        let cold = tmp();
        fs::write(cold.path().join("marker"), "").unwrap();
        assert!(detect_custom(std::slice::from_ref(&on), cold.path(), false).is_empty());
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
