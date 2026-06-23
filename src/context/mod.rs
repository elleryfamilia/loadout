//! Context detection: what/where/how of the current project & runtime.
//!
//! Detection is split into small [`ContextDetector`] implementations, each of
//! which enriches a shared [`Context`]. Detectors are **best-effort**: a single
//! detector failing (no git, unreadable file, missing `ps`) is logged at verbose
//! level and never aborts the run — a context tool must degrade gracefully.

pub mod commands;
pub mod git;
pub mod languages;
pub mod system;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::vlog;

/// Everything we detected about the current project and runtime.
#[derive(Debug, Clone, Serialize)]
pub struct Context {
    /// Current working directory (the dir loadout was asked to operate on).
    pub cwd: PathBuf,
    /// Repo base: git root when present, otherwise `cwd`.
    pub repo_base: PathBuf,
    /// Repository name (from remote URL or directory name).
    pub repo_name: Option<String>,
    /// Git details, if inside a work tree.
    pub git: Option<GitContext>,
    /// Detected languages (human names), most-prevalent first.
    pub languages: Vec<String>,
    /// Detected stack keys (`rust`, `nextjs`, `node`, `go`, `python`, …).
    pub stacks: Vec<String>,
    /// Detected package managers (`cargo`, `pnpm`, `uv`, …).
    pub package_managers: Vec<String>,
    /// Custom targets (user-defined labels) that matched this project. Fed into
    /// [`Context::selection_targets`] alongside `stacks`, but kept separate so
    /// the built-in stack→commands mapping never sees an arbitrary label.
    /// Skipped from the serialized form when empty, so a repo with no custom
    /// targets fingerprints (and renders) exactly as before the field existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub custom_targets: Vec<String>,
    /// Discovered project commands.
    pub commands: ProjectCommands,
    /// OS / arch / host / user / caller.
    pub system: SystemContext,
    /// Allowlisted, redacted environment variables.
    pub env: BTreeMap<String, String>,
}

/// Git context for the current work tree.
#[derive(Debug, Clone, Serialize)]
pub struct GitContext {
    /// Top-level work-tree directory.
    pub root: PathBuf,
    /// Current branch, or `None` when detached.
    pub branch: Option<String>,
    /// Remotes with credential-sanitized URLs.
    pub remotes: Vec<GitRemote>,
    /// Whether this is a linked worktree (not the primary checkout).
    pub is_worktree: bool,
}

/// A git remote (URL already sanitized via [`crate::redact`]).
#[derive(Debug, Clone, Serialize)]
pub struct GitRemote {
    /// Remote name, e.g. `origin`.
    pub name: String,
    /// Sanitized URL.
    pub url: String,
}

/// OS / architecture / host / user / caller information.
#[derive(Debug, Clone, Serialize)]
pub struct SystemContext {
    /// `std::env::consts::OS` (e.g. `macos`, `linux`).
    pub os: String,
    /// `std::env::consts::ARCH` (e.g. `aarch64`, `x86_64`).
    pub arch: String,
    /// Machine hostname.
    pub hostname: String,
    /// Current user (from environment).
    pub user: String,
    /// Best-effort parent-process name (the caller), if discoverable.
    pub parent_process: Option<String>,
    /// Host class derived from config `host_classes` (e.g. `work`).
    pub host_class: Option<String>,
}

/// Where loadout is operating: inside a git repo, or on the bare machine.
///
/// **Derived, never stored** — and intentionally **not** part of the context
/// hash: the `git: Option<…>` field already encodes repo-vs-machine, so adding a
/// field would needlessly invalidate every existing overlay. Drives profile
/// selection (the `machine` target) and where the per-project binding lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Operating inside a git work tree.
    Repo,
    /// Operating outside any repo (general machine/devops context).
    Machine,
}

/// Discovered build/test/lint/run commands.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectCommands {
    /// Build commands.
    pub build: Vec<String>,
    /// Test commands.
    pub test: Vec<String>,
    /// Lint commands.
    pub lint: Vec<String>,
    /// Run/dev commands.
    pub run: Vec<String>,
}

impl ProjectCommands {
    /// True when nothing was discovered.
    pub fn is_empty(&self) -> bool {
        self.build.is_empty() && self.test.is_empty() && self.lint.is_empty() && self.run.is_empty()
    }
}

impl Context {
    fn empty(cwd: PathBuf, repo_base: PathBuf) -> Self {
        Context {
            cwd,
            repo_base,
            repo_name: None,
            git: None,
            languages: Vec::new(),
            stacks: Vec::new(),
            package_managers: Vec::new(),
            custom_targets: Vec::new(),
            commands: ProjectCommands::default(),
            system: SystemContext {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                hostname: String::new(),
                user: String::new(),
                parent_process: None,
                host_class: None,
            },
            env: BTreeMap::new(),
        }
    }

    /// Repo vs machine, derived from whether a git work tree was detected.
    pub fn scope(&self) -> Scope {
        if self.git.is_some() {
            Scope::Repo
        } else {
            Scope::Machine
        }
    }

    /// The coarse language/platform tags a profile's `targets` are matched
    /// against: the detected stacks, then any matched custom targets, plus
    /// `machine` when not in a repo.
    pub fn selection_targets(&self) -> Vec<String> {
        let mut tags = self.stacks.clone();
        tags.extend(self.custom_targets.iter().cloned());
        if self.scope() == Scope::Machine {
            tags.push("machine".to_string());
        }
        tags
    }

    /// cwd relative to the repo base, using forward slashes. Empty at the root.
    pub fn rel_path(&self) -> String {
        let rel = self.cwd.strip_prefix(&self.repo_base).unwrap_or(&self.cwd);
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Deterministic content fingerprint of this context.
    ///
    /// Excludes [`SystemContext::parent_process`], which is provenance (it differs
    /// between a direct call and `load run`) and must not churn the hash.
    pub fn compute_hash(&self) -> String {
        let mut clone = self.clone();
        clone.system.parent_process = None;
        crate::hash::context_hash(&clone)
    }

    fn normalize(&mut self) {
        dedup_preserving(&mut self.stacks);
        dedup_preserving(&mut self.package_managers);
        dedup_preserving(&mut self.languages);
        dedup_preserving(&mut self.custom_targets);
    }
}

fn dedup_preserving(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

/// Inputs handed to every detector.
pub struct DetectInput<'a> {
    /// Directory being operated on.
    pub cwd: PathBuf,
    /// Repo base (git root or cwd).
    pub repo_base: PathBuf,
    /// Merged configuration (for env allowlist, host classes, …).
    pub config: &'a Config,
    /// Whether detection may execute side-effecting probes — specifically
    /// script-predicate custom targets. `false` (the default, used by
    /// inspection commands like `explain`/`detect`) serves cached verdicts
    /// only; `true` (real renders: `run`/`render`/`refresh`) runs them.
    pub live: bool,
}

/// A unit of context detection that enriches the shared [`Context`].
pub trait ContextDetector {
    /// Stable name for diagnostics.
    fn name(&self) -> &'static str;
    /// Enrich `ctx`. Errors are treated as best-effort and logged, not fatal.
    fn detect(&self, input: &DetectInput, ctx: &mut Context) -> crate::Result<()>;
}

/// The default detector pipeline, in dependency order.
pub fn default_detectors() -> Vec<Box<dyn ContextDetector>> {
    vec![
        Box::new(git::GitDetector),
        Box::new(languages::LanguageDetector),
        Box::new(commands::CommandDetector),
        Box::new(system::SystemDetector),
        Box::new(system::EnvDetector),
    ]
}

/// Resolve the repo base for `cwd`: the git root, or `cwd` when not in a repo.
pub fn repo_base_for(cwd: &Path) -> PathBuf {
    git::find_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// Run the full detector pipeline for `cwd` against `config`. Detection is
/// **cache-only** for script-predicate targets (no code runs) — the default for
/// inspection commands; pass `live = true` via [`detect_context_with`] on the
/// real render paths (`run`/`refresh`).
pub fn detect_context(cwd: &Path, config: &Config) -> crate::Result<Context> {
    detect_context_with(cwd, config, false)
}

/// Run the full detector pipeline for `cwd` against `config`, with `live`
/// controlling whether script-predicate targets execute.
pub fn detect_context_with(cwd: &Path, config: &Config, live: bool) -> crate::Result<Context> {
    let repo_base = repo_base_for(cwd);
    let input = DetectInput {
        cwd: cwd.to_path_buf(),
        repo_base: repo_base.clone(),
        config,
        live,
    };
    let mut ctx = Context::empty(cwd.to_path_buf(), repo_base);
    for d in default_detectors() {
        if let Err(e) = d.detect(&input, &mut ctx) {
            vlog!("detector '{}' degraded: {e:#}", d.name());
        }
    }
    ctx.normalize();

    // Outside a git repo there is no remote/root to name the project from, so
    // fall back to the base directory's name (non-repo is a first-class case).
    if ctx.repo_name.is_none() {
        ctx.repo_name = ctx
            .repo_base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
    }

    Ok(ctx)
}

/// Test fixtures shared across module unit tests (compiled only under `cfg(test)`).
#[cfg(test)]
pub mod test_support {
    use super::*;

    /// A minimal but valid context rooted at a fake repo for pure-logic tests.
    pub fn sample_context() -> Context {
        let root = PathBuf::from("/tmp/sample-repo");
        let mut ctx = Context::empty(root.clone(), root.clone());
        ctx.repo_name = Some("sample-repo".into());
        ctx.git = Some(GitContext {
            root,
            branch: Some("main".into()),
            remotes: vec![],
            is_worktree: false,
        });
        ctx.system.hostname = "test-host".into();
        ctx.system.user = "tester".into();
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_is_empty_at_root() {
        let ctx = test_support::sample_context();
        assert_eq!(ctx.rel_path(), "");
    }

    #[test]
    fn rel_path_under_subdir() {
        let mut ctx = test_support::sample_context();
        ctx.cwd = ctx.repo_base.join("infra/db");
        assert_eq!(ctx.rel_path(), "infra/db");
    }

    #[test]
    fn hash_ignores_parent_process() {
        let mut a = test_support::sample_context();
        let mut b = a.clone();
        a.system.parent_process = Some("zsh".into());
        b.system.parent_process = Some("claude".into());
        assert_eq!(a.compute_hash(), b.compute_hash());
    }

    #[test]
    fn hash_tracks_stacks() {
        let mut a = test_support::sample_context();
        let mut b = a.clone();
        a.stacks = vec!["rust".into()];
        b.stacks = vec!["go".into()];
        assert_ne!(a.compute_hash(), b.compute_hash());
    }

    #[test]
    fn selection_targets_includes_custom_targets() {
        let mut ctx = test_support::sample_context();
        ctx.git = None; // off-repo → `machine` is appended
        ctx.stacks = vec!["rust".into()];
        ctx.custom_targets = vec!["monorepo".into()];
        let tags = ctx.selection_targets();
        assert!(tags.contains(&"rust".to_string()), "stacks still included");
        assert!(
            tags.contains(&"monorepo".to_string()),
            "custom target included"
        );
        assert!(
            tags.contains(&"machine".to_string()),
            "machine still appended"
        );
    }

    #[test]
    fn hash_tracks_custom_targets() {
        // A custom target appearing must invalidate the overlay; an empty
        // custom_targets must hash identically to before the field existed.
        let base = test_support::sample_context();
        let mut with = base.clone();
        with.custom_targets = vec!["monorepo".into()];
        assert_ne!(base.compute_hash(), with.compute_hash());
    }
}
