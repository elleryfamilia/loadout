//! Workflows — a named, ordered stage spine bound to a profile.
//!
//! A **workflow** is loadout's house process spine: an ordered list of
//! **stages** (e.g. Research → Specify → Plan → Implement → Verify → Review)
//! that travels across every agent through the same per-agent render pipeline a
//! profile already uses. A profile binds exactly one workflow by id
//! ([`crate::profile::LoadoutConfig::workflow`]); selection stays deterministic.
//!
//! Each stage carries a short contract — a free-string `name`, a `purpose`,
//! optional `reads`/`writes` of a **handoff artifact**, an optional `gate`, and
//! an optional `exit` checklist. The handoff artifact is the load-bearing part:
//! a file under `.loadout/workflow/artifacts/` (e.g. Plan writes `plan.md`,
//! Implement reads it) that carries state from one stage to the next. It is what
//! makes a workflow more than "a profile with subheadings".
//!
//! loadout owns the path convention and renders the spine, but it never
//! enforces, judges completion, or tracks a live "current stage" — this is
//! guidance, not policy, with no runtime and no LLM.
//!
//! Workflows are **global-only**, exactly like fragments and profiles: a repo
//! layer may *declare* `[[workflows]]` but the loader strips them (see
//! [`crate::fragment::Layer::contributes_workflows`] and
//! `strip_global_only`), so a cloned repo can never inject a workflow.
//!
//! This module is the data model: the types, validation, the artifact-path
//! convention, a content hash, the shipped [`builtin_workflows`] catalog, and
//! [`resolve_workflow`]. Rendering — the context section and the per-stage slash
//! commands — lives in a later slice and is intentionally absent here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fragment::Layer;

/// Subdirectory of a repo's `.loadout/` that holds workflow handoff artifacts.
/// A stage's `reads`/`writes` name a file directly inside it.
pub const ARTIFACT_SUBDIR: &str = "workflow/artifacts";

/// A named, ordered stage spine a profile can bind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// Stable id referenced by `loadouts[].workflow` (e.g. `spec-driven`).
    pub id: String,
    /// Display name shown on the gallery card and as the rendered section
    /// heading (e.g. `Superpowers`, `Boris's workflow`). Falls back to
    /// `description`, then `id`. Set on the curated built-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable summary — the brief blurb on the gallery card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Studio glyph name for the gallery card (from the built-in icon set, e.g.
    /// `bolt`, `refresh`). `None` falls back to a default glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The ordered stages. A workflow needs ≥1 (enforced by [`Workflow::validate`],
    /// surfaced by `doctor`/studio rather than rejected by the parser).
    #[serde(default)]
    pub stages: Vec<WorkflowStage>,
    /// Provenance: the suite this workflow is modeled on (e.g. `Spec Kit`). Set
    /// on built-ins; optional on your own. Display-only — never affects render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modeled_on: Option<String>,
    /// Provenance: a short note on the research behind it. Display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub researched: Option<String>,
    /// Upstream source URL (the repo or writeup this is drawn from). Display-only
    /// for now; the future "keep curated workflows in sync with their source
    /// repos" milestone hangs off this. Set on the curated built-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Off-switch: kept in config, never selected. Only serialized when set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Which config layer defined it (set at load, not deserialized) — drives
    /// global-only enforcement, like [`crate::fragment::Fragment::origin`].
    #[serde(skip)]
    pub origin: Layer,
}

/// One stage in a [`Workflow`]: a free-string name plus a short contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStage {
    /// Free-string stage name (e.g. `plan`, `implement`). **Not** a closed enum
    /// — you can add your own stages. Becomes the generated slash-command name
    /// in the render slice.
    pub name: String,
    /// What this stage is for — the one-line contract rendered into the spine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Elaborate, on-demand guidance for this stage — the full prescriptive
    /// body baked into the per-step command file (channel 2) when
    /// `/loadout:<command>` runs. The always-on `## Workflow` context section
    /// (channel 1) keeps using only `purpose`, so depth here costs nothing
    /// until the step is actually invoked. Markdown, injected verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Handoff artifact this stage reads: a bare filename under
    /// `.loadout/workflow/artifacts/` (e.g. `plan.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<String>,
    /// Handoff artifact this stage writes: a bare filename under
    /// `.loadout/workflow/artifacts/` (e.g. `plan.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<String>,
    /// Whether this stage is a checkpoint the user is expected to review before
    /// moving on. Guidance only — loadout never blocks. Serialized only when set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate: bool,
    /// An optional "done when" checklist rendered alongside the stage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit: Vec<String>,
}

impl Workflow {
    /// The display title for this workflow: its `name`, else its `description`,
    /// else its `id`.
    pub fn title(&self) -> &str {
        self.name
            .as_deref()
            .or(self.description.as_deref())
            .unwrap_or(&self.id)
    }

    /// A stable fingerprint of this workflow's content (id + stages + …). The
    /// render layer folds it into the overlay fingerprint so editing a bound
    /// workflow invalidates a repo's cached overlay even when the detected
    /// context is unchanged. Independent of any live state — it hashes the
    /// source, so it's deterministic across renders.
    pub fn content_hash(&self) -> String {
        crate::hash::context_hash(self)
    }

    /// Validate a workflow, returning a list of human-readable problems (empty =
    /// well-formed). Surfaced by `doctor`/studio; never panics, never rejects at
    /// parse time (a malformed workflow degrades, it doesn't break the load).
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.id.trim().is_empty() {
            problems.push("workflow has an empty id".to_string());
        }
        if self.stages.is_empty() {
            problems.push(format!("workflow '{}' has no stages", self.id));
        }
        let mut seen = std::collections::HashSet::new();
        for stage in &self.stages {
            let name = stage.name.trim();
            if name.is_empty() {
                problems.push(format!(
                    "workflow '{}' has a stage with an empty name",
                    self.id
                ));
                continue;
            }
            if !seen.insert(name.to_string()) {
                problems.push(format!(
                    "workflow '{}' has a duplicate stage name '{name}'",
                    self.id
                ));
            }
            for (verb, artifact) in [("reads", &stage.reads), ("writes", &stage.writes)] {
                if let Some(a) = artifact {
                    if !is_safe_artifact_name(a) {
                        problems.push(format!(
                            "workflow '{}' stage '{name}' {verb} an unsafe artifact name '{a}' \
                             (use a plain filename like `plan.md`)",
                            self.id
                        ));
                    }
                }
            }
        }
        problems
    }

    /// Every distinct handoff artifact this workflow touches (read or written),
    /// in first-seen order. The render/run layer uses this to set one
    /// `LOADOUT_<NAME>_PATH` env var per artifact and to ensure the artifacts
    /// dir exists. Unsafe names are skipped (they never become paths).
    pub fn artifacts(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for stage in &self.stages {
            for artifact in [&stage.reads, &stage.writes].into_iter().flatten() {
                if is_safe_artifact_name(artifact) && seen.insert(artifact.clone()) {
                    out.push(artifact.clone());
                }
            }
        }
        out
    }

    /// Lay this workflow's stages onto the fixed canonical spine: the five
    /// canonical slots in order (each filled by the first stage that claims it,
    /// or left empty), then any custom stages that match no canonical phase. The
    /// first stage to claim a slot wins, so a workflow that names two stages onto
    /// the same phase (e.g. `verify` and `ship` both → `verify`) keeps only the
    /// first — exactly one `/loadout:<command>` per slot. This is the single
    /// source of truth shared by the command channel, the context section, and
    /// the studio slot reader.
    pub fn canonical_layout(&self) -> CanonicalLayout<'_> {
        let mut by_slot: std::collections::HashMap<&str, &WorkflowStage> =
            std::collections::HashMap::new();
        let mut extras: Vec<&WorkflowStage> = Vec::new();
        for s in &self.stages {
            match canonical_slot(&s.name) {
                Some(slot) => {
                    by_slot.entry(slot).or_insert(s);
                }
                None => extras.push(s),
            }
        }
        let slots = CANONICAL_SLOTS
            .iter()
            .map(|&(command, desc)| LaidSlot {
                command,
                desc,
                stage: by_slot.get(command).copied(),
            })
            .collect();
        CanonicalLayout { slots, extras }
    }

    /// The stage this workflow assigns to a generated command: the first stage
    /// that fills the canonical slot (`plan`, `verify`, …), or an exact-named
    /// custom stage (`compound`). Lets the editor inherit a step's handoff/gate/
    /// exit from the workflow it was customized from while editing only the prose.
    pub fn stage_for_command(&self, command: &str) -> Option<&WorkflowStage> {
        self.stages
            .iter()
            .find(|s| canonical_slot(&s.name) == Some(command))
            .or_else(|| self.stages.iter().find(|s| s.name == command))
    }
}

impl WorkflowStage {
    /// The full path to this stage's read artifact under `repo_base`, if it
    /// names one safely.
    pub fn read_path(&self, repo_base: &Path) -> Option<PathBuf> {
        self.reads
            .as_deref()
            .and_then(|a| artifact_path(repo_base, a))
    }

    /// The full path to this stage's write artifact under `repo_base`, if it
    /// names one safely.
    pub fn write_path(&self, repo_base: &Path) -> Option<PathBuf> {
        self.writes
            .as_deref()
            .and_then(|a| artifact_path(repo_base, a))
    }
}

/// The fixed spine every workflow maps onto: the canonical stages loadout
/// offers as a stable set of slash commands (`/loadout:<slot>`). Picking a
/// workflow doesn't change *which* commands exist — it changes what each one
/// means. Each entry is `(slot, what-this-step-is)`; the description is
/// workflow-independent (the process), distinct from a workflow's own purpose
/// text (the style).
pub const CANONICAL_SLOTS: &[(&str, &str)] = &[
    (
        "explore",
        "Understand the problem and the code before changing anything.",
    ),
    ("brainstorm", "Shape the idea — the design or the spec."),
    ("plan", "Break it into an ordered task list."),
    ("implement", "Build it."),
    ("verify", "Check the result — tests, review, commit."),
];

/// Map a free-string stage name to the canonical slot it fills, or `None` for a
/// custom stage that matches no known phase (shown after the fixed spine).
/// Matching is case-insensitive on common synonyms — a workflow can name its
/// stages naturally (`research`, `specify`, `review`, `iterate`, `commit`) and
/// still land in the right slot.
pub fn canonical_slot(stage_name: &str) -> Option<&'static str> {
    let slot = match stage_name.trim().to_ascii_lowercase().as_str() {
        "explore" | "research" | "investigate" | "understand" | "scope" => "explore",
        "brainstorm" | "specify" | "spec" | "design" | "ideate" | "discovery" => "brainstorm",
        "plan" | "planning" | "decompose" => "plan",
        "implement" | "iterate" | "code" | "build" | "execute" | "develop" => "implement",
        "verify" | "review" | "commit" | "test" | "validate" | "ship" | "qa" => "verify",
        _ => return None,
    };
    Some(slot)
}

/// One canonical slot after a workflow's stages are laid onto the fixed spine:
/// either filled by the first stage that claimed it, or empty (the workflow
/// skips that phase). `command` is the stable `/loadout:<command>` name; `desc`
/// is the workflow-independent description of the phase (the process).
#[derive(Debug, Clone, Copy)]
pub struct LaidSlot<'a> {
    /// Canonical command name (e.g. `verify`) — what the slash command is called.
    pub command: &'static str,
    /// The process-level description of this phase (workflow-independent).
    pub desc: &'static str,
    /// The stage filling this slot, or `None` when the workflow skips it.
    pub stage: Option<&'a WorkflowStage>,
}

/// A workflow laid onto the canonical spine: the five fixed slots in order
/// (each filled or skipped), plus any custom stages that match no canonical
/// phase (kept in declaration order, rendered after the spine).
#[derive(Debug, Clone)]
pub struct CanonicalLayout<'a> {
    /// The five canonical slots, in spine order.
    pub slots: Vec<LaidSlot<'a>>,
    /// Stages that matched no canonical slot (a hand-authored extra step).
    pub extras: Vec<&'a WorkflowStage>,
}

impl<'a> CanonicalLayout<'a> {
    /// The stages that actually produce a command/section, in spine order: every
    /// filled canonical slot (named by its canonical command) followed by every
    /// extra (named by its own stage name). This is the exact, de-duplicated set
    /// the command channel writes and the context section lists — one entry per
    /// `/loadout:<command>`. Skipped canonical slots are omitted.
    pub fn steps(&self) -> Vec<(&'a str, &'a WorkflowStage)> {
        let mut out: Vec<(&'a str, &'a WorkflowStage)> = Vec::new();
        for slot in &self.slots {
            if let Some(stage) = slot.stage {
                out.push((slot.command, stage));
            }
        }
        for stage in &self.extras {
            out.push((stage.name.as_str(), stage));
        }
        out
    }
}

/// The directory holding a repo's workflow handoff artifacts:
/// `<repo>/.loadout/workflow/artifacts`.
pub fn artifacts_dir(repo_base: &Path) -> PathBuf {
    crate::config::repo_dir(repo_base).join(ARTIFACT_SUBDIR)
}

/// The full path to a named handoff artifact under `repo_base`, or `None` when
/// the name isn't a safe bare filename. The guard keeps a workflow — even a
/// malformed or hostile one — from writing or pointing an env var outside the
/// artifacts dir (mirrors the repo-confinement [`crate::target`] applies to
/// detection paths).
pub fn artifact_path(repo_base: &Path, name: &str) -> Option<PathBuf> {
    if !is_safe_artifact_name(name) {
        return None;
    }
    Some(artifacts_dir(repo_base).join(name))
}

/// Whether `name` is a safe handoff-artifact filename: a single non-empty path
/// component, not hidden, with no separators and no `.`/`..`. So `plan.md` is
/// fine; `../x`, `a/b`, `/etc/passwd`, `.`, and `.hidden` are not.
pub fn is_safe_artifact_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') {
        return false;
    }
    // A bare filename only: reject any path separator outright (so `a/b`, the
    // trailing-slash `x/` that `Path` would otherwise normalize away, and
    // Windows-style `a\b` are all out) before the component check.
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    let mut comps = Path::new(name).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// The environment variable loadout sets to a handoff artifact's path, so a
/// stage command can reference the artifact without hardcoding its location: the
/// filename stem, uppercased and non-alphanumerics folded to `_`, wrapped as
/// `LOADOUT_<STEM>_PATH` (e.g. `plan.md` → `LOADOUT_PLAN_PATH`). `None` for an
/// unsafe name or one with no alphanumeric stem.
pub fn artifact_env_var(name: &str) -> Option<String> {
    if !is_safe_artifact_name(name) {
        return None;
    }
    let stem = Path::new(name).file_stem()?.to_str()?;
    let key: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let key = key.trim_matches('_');
    if key.is_empty() {
        return None;
    }
    Some(format!("LOADOUT_{key}_PATH"))
}

/// Resolve a workflow id against your library plus the built-in catalog: your
/// own `[[workflows]]` **shadow** a built-in of the same id (the "copy a
/// built-in and hand-edit it" story — your copy wins even if you then disable
/// it). A disabled match, or an unknown id, yields `None` — a dangling
/// `workflow = "typo"` binding that degrades gracefully.
pub fn resolve_workflow<'a>(
    id: &str,
    user: &'a [Workflow],
    builtins: &'a [Workflow],
) -> Option<&'a Workflow> {
    let chosen = user
        .iter()
        .find(|w| w.id == id)
        .or_else(|| builtins.iter().find(|w| w.id == id))?;
    (!chosen.disabled).then_some(chosen)
}

/// Strip a leading YAML frontmatter block (`---\n…\n---`) from a vendored skill
/// file, returning the body. The vendored files under `vendored/` are kept
/// byte-for-byte so the sync action can diff them against upstream; their
/// frontmatter is loader metadata (name/description), just noise inside a
/// rendered command, so it's dropped when the body becomes a step's
/// `instructions`. No frontmatter → the input, trimmed.
fn strip_frontmatter(s: &str) -> &str {
    match s
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
    {
        Some((_frontmatter, body)) => body.trim_start(),
        None => s.trim_start(),
    }
}

/// The shipped workflow catalog: read-only starting points you bind directly or
/// **copy and hand-edit** (a user `[[workflows]]` of the same id shadows the
/// built-in). Each is modeled on a real suite and stamped with provenance.
///
/// Three opinionated spines covering the common shapes:
/// - `lean` — Anthropic's explore → plan → code → commit.
/// - `spec-driven` — write-the-spec-first (GitHub Spec Kit, AWS Kiro).
/// - `loop` — a single-prompt backlog loop (the "Ralph" technique).
pub fn builtin_workflows() -> Vec<Workflow> {
    fn stage(name: &str, purpose: &str) -> WorkflowStage {
        WorkflowStage {
            name: name.to_string(),
            purpose: Some(purpose.to_string()),
            instructions: None,
            reads: None,
            writes: None,
            gate: false,
            exit: Vec::new(),
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn wf(
        id: &str,
        name: &str,
        description: &str,
        icon: &str,
        modeled_on: &str,
        source: &str,
        researched: &str,
        stages: Vec<WorkflowStage>,
    ) -> Workflow {
        Workflow {
            id: id.to_string(),
            name: Some(name.to_string()),
            description: Some(description.to_string()),
            icon: Some(icon.to_string()),
            stages,
            modeled_on: Some(modeled_on.to_string()),
            researched: Some(researched.to_string()),
            source: Some(source.to_string()),
            disabled: false,
            origin: Layer::BuiltIn,
        }
    }

    vec![
        // --- superpowers — obra/superpowers (the biggest community framework) ---
        wf(
            "superpowers",
            "Superpowers",
            "The community's most-starred agent framework.",
            "bolt",
            "obra/superpowers (Jesse Vincent)",
            "https://github.com/obra/superpowers",
            "The biggest community Claude Code skills framework (~41k★): refine the idea, \
             write a tight plan of tiny tasks, then execute with fresh subagents + review.",
            vec![
                WorkflowStage {
                    writes: Some("design.md".to_string()),
                    // The real upstream skill, vendored verbatim (see vendored/
                    // superpowers + vendored/sources.toml). Loaded only into the
                    // on-demand command body, never the always-on context map.
                    instructions: Some(
                        strip_frontmatter(include_str!(
                            "../vendored/superpowers/brainstorming.SKILL.md"
                        ))
                        .to_string(),
                    ),
                    ..stage(
                        "brainstorm",
                        "Refine the rough idea through questions, explore alternatives, and \
                         agree a design before any code.",
                    )
                },
                WorkflowStage {
                    reads: Some("design.md".to_string()),
                    writes: Some("plan.md".to_string()),
                    instructions: Some(
                        strip_frontmatter(include_str!(
                            "../vendored/superpowers/writing-plans.SKILL.md"
                        ))
                        .to_string(),
                    ),
                    ..stage(
                        "plan",
                        "Break the approved design into bite-sized tasks — each a few \
                         minutes, with exact file paths and a verification step.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    instructions: Some(
                        strip_frontmatter(include_str!(
                            "../vendored/superpowers/subagent-driven-development.SKILL.md"
                        ))
                        .to_string(),
                    ),
                    ..stage(
                        "implement",
                        "Dispatch a fresh subagent per task; work test-first and keep each \
                         task isolated.",
                    )
                },
                WorkflowStage {
                    gate: true,
                    exit: vec![
                        "each task reviewed for spec compliance".to_string(),
                        "then reviewed for code quality".to_string(),
                    ],
                    instructions: Some(
                        strip_frontmatter(include_str!(
                            "../vendored/superpowers/requesting-code-review.SKILL.md"
                        ))
                        .to_string(),
                    ),
                    ..stage(
                        "review",
                        "Two-stage review — does it match the spec, then is the code good \
                         — before merging.",
                    )
                },
            ],
        ),
        // --- boris — how Claude Code's creator works -----------------------
        wf(
            "boris",
            "Boris's workflow",
            "The workflow Claude Code's creator uses daily.",
            "rocket",
            "Boris Cherny (Claude Code's creator)",
            "https://howborisusesclaudecode.com/",
            "Nail the plan in plan-mode, let it implement in one pass, lean hard on a \
             verify loop, then /commit-push-pr.",
            vec![
                WorkflowStage {
                    writes: Some("plan.md".to_string()),
                    ..stage(
                        "plan",
                        "Work in plan mode; iterate on the plan until it's right before \
                         touching code.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    ..stage(
                        "implement",
                        "Switch to auto-accept and let it execute the whole plan in one pass.",
                    )
                },
                WorkflowStage {
                    gate: true,
                    exit: vec![
                        "the agent verified its own work (tests / browser)".to_string(),
                        "the behavior and UX actually check out".to_string(),
                        "then commit, push, and open the PR (Boris's /commit-push-pr, \
                         run dozens of times a day)"
                            .to_string(),
                    ],
                    ..stage(
                        "verify",
                        "Give the agent a way to check its own work — run tests, open the \
                         browser, iterate until it's right (the biggest quality lever) — then \
                         commit, push, and open the PR in one step.",
                    )
                },
            ],
        ),
        // --- lean — Anthropic explore-plan-code-commit ---------------------
        wf(
            "lean",
            "Lean",
            "Anthropic's recommended starting point.",
            "git-branch",
            "Anthropic explore-plan-code-commit",
            "https://www.anthropic.com/engineering/claude-code-best-practices",
            "Anthropic's agent best-practices loop: read first, plan on paper, then build.",
            vec![
                stage(
                    "explore",
                    "Read the code paths and tests involved before changing anything. \
                     Don't write code yet — build a map of what's there.",
                ),
                WorkflowStage {
                    writes: Some("plan.md".to_string()),
                    exit: vec![
                        "objective stated in one sentence".to_string(),
                        "approach and key risks listed".to_string(),
                        "validation steps named".to_string(),
                    ],
                    ..stage(
                        "plan",
                        "Write a short plan: objective, approach, risks, and how you'll validate.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    ..stage(
                        "implement",
                        "Build the change following the plan; keep edits focused and matched to \
                         the surrounding code.",
                    )
                },
                WorkflowStage {
                    gate: true,
                    exit: vec![
                        "build, tests, and linter pass".to_string(),
                        "commit message follows Conventional Commits".to_string(),
                    ],
                    ..stage(
                        "commit",
                        "Run the build, tests, and linter, then commit at a logical checkpoint.",
                    )
                },
            ],
        ),
        wf(
            "spec-driven",
            "Spec-driven",
            "The spec-first method behind Spec Kit & Kiro.",
            "book",
            "GitHub Spec Kit / AWS Kiro",
            "https://github.com/github/spec-kit",
            "Spec-driven development: a written spec is the source of truth that the plan, \
             implementation, and verification all answer to.",
            vec![
                stage(
                    "research",
                    "Gather the context and constraints; note open questions and unknowns.",
                ),
                WorkflowStage {
                    writes: Some("spec.md".to_string()),
                    ..stage(
                        "specify",
                        "Write what to build and why — requirements and acceptance criteria, \
                         not implementation detail.",
                    )
                },
                WorkflowStage {
                    reads: Some("spec.md".to_string()),
                    writes: Some("plan.md".to_string()),
                    ..stage(
                        "plan",
                        "Turn the spec into a technical plan and an ordered task list.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    ..stage("implement", "Work the plan task by task, in order.")
                },
                WorkflowStage {
                    reads: Some("spec.md".to_string()),
                    gate: true,
                    exit: vec![
                        "every acceptance criterion in the spec is met".to_string(),
                        "no known regressions".to_string(),
                    ],
                    ..stage(
                        "verify",
                        "Check the result against the spec's acceptance criteria.",
                    )
                },
            ],
        ),
        wf(
            "loop",
            "Ralph loop",
            "The viral 'Ralph' loop for long autonomous runs.",
            "refresh",
            "the Ralph single-prompt loop",
            "https://ghuntley.com/ralph/",
            "The Ralph technique: keep a durable backlog and repeatedly take the next item, \
             so a long task survives across many short agent runs.",
            vec![
                WorkflowStage {
                    writes: Some("backlog.md".to_string()),
                    ..stage(
                        "plan",
                        "Break the work into a checklist of small, independently shippable items.",
                    )
                },
                WorkflowStage {
                    reads: Some("backlog.md".to_string()),
                    writes: Some("backlog.md".to_string()),
                    ..stage(
                        "iterate",
                        "Take the next unchecked item, implement it, verify it, then check it \
                         off. Repeat until the backlog is clear.",
                    )
                },
                WorkflowStage {
                    gate: true,
                    ..stage(
                        "verify",
                        "Run the full build and test suite; confirm nothing regressed.",
                    )
                },
            ],
        ),
        // --- compound engineering — Every's compounding loop ---------------
        wf(
            "compound",
            "Compound engineering",
            "Every's loop where each cycle makes the next one easier.",
            "package",
            "Every (Kieran Klaassen & T.M. Chow)",
            "https://github.com/EveryInc/compound-engineering-plugin",
            "Compound engineering: plan-heavy cycles (brainstorm the requirements, plan in \
             detail, build, review against the plan) that END by capturing what you learned, \
             so each cycle compounds and the next one starts ahead.",
            vec![
                WorkflowStage {
                    writes: Some("requirements.md".to_string()),
                    ..stage(
                        "brainstorm",
                        "Interactive Q&A to pin down requirements — produce a right-sized \
                         requirements doc before any code.",
                    )
                },
                WorkflowStage {
                    reads: Some("requirements.md".to_string()),
                    writes: Some("plan.md".to_string()),
                    ..stage(
                        "plan",
                        "Turn the requirements into a detailed implementation plan with \
                         safeguards. Planning is ~80% of the work.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    ..stage(
                        "implement",
                        "Execute the plan in a worktree, tracking each task, then simplify the \
                         new code for clarity and reuse.",
                    )
                },
                WorkflowStage {
                    reads: Some("plan.md".to_string()),
                    gate: true,
                    exit: vec![
                        "reviewed against the plan by independent agents".to_string(),
                        "issues fixed before merging".to_string(),
                    ],
                    ..stage(
                        "review",
                        "Multi-agent review against the plan before merging.",
                    )
                },
                stage(
                    "compound",
                    "Capture what you learned into docs/solutions/ so the next cycle starts \
                     ahead — the step that compounds.",
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_well_formed() {
        let workflows = builtin_workflows();
        let mut ids = std::collections::HashSet::new();
        for w in &workflows {
            assert!(ids.insert(w.id.clone()), "duplicate workflow id {}", w.id);
            assert!(w.description.is_some(), "{} lacks a description", w.id);
            assert!(w.name.is_some(), "{} lacks a display name", w.id);
            assert!(w.icon.is_some(), "{} lacks a card icon", w.id);
            assert!(w.modeled_on.is_some(), "{} lacks provenance", w.id);
            assert!(w.researched.is_some(), "{} lacks a research note", w.id);
            // Curated built-ins carry an upstream source (for display + the
            // future source-sync milestone).
            assert!(w.source.is_some(), "{} lacks a source link", w.id);
            assert_eq!(w.origin, Layer::BuiltIn, "{} should be built-in", w.id);
            // Every shipped workflow must itself validate.
            assert!(
                w.validate().is_empty(),
                "{} fails validation: {:?}",
                w.id,
                w.validate()
            );
        }
        // The curated gallery: the recognizable named ones + the generic spines.
        for needed in ["superpowers", "boris", "lean", "spec-driven", "loop"] {
            assert!(ids.contains(needed), "missing built-in workflow {needed}");
        }
    }

    #[test]
    fn lean_has_the_plan_implement_handoff() {
        // The load-bearing part: the plan stage writes plan.md and the implement
        // stage reads it. Without this handoff the feature is just headings.
        let lean = builtin_workflows()
            .into_iter()
            .find(|w| w.id == "lean")
            .unwrap();
        let plan = lean.stages.iter().find(|s| s.name == "plan").unwrap();
        let implement = lean.stages.iter().find(|s| s.name == "implement").unwrap();
        assert_eq!(plan.writes.as_deref(), Some("plan.md"));
        assert_eq!(implement.reads.as_deref(), Some("plan.md"));
        // plan.md is surfaced once in the workflow's artifact set.
        assert_eq!(lean.artifacts(), vec!["plan.md".to_string()]);
    }

    #[test]
    fn validate_flags_empty_id_no_stages_dupes_and_unsafe_artifacts() {
        let ok = Workflow {
            id: "ok".into(),
            name: None,
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "plan".into(),
                purpose: None,
                instructions: None,
                reads: None,
                writes: Some("plan.md".into()),
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: Layer::Global,
        };
        assert!(ok.validate().is_empty());

        // Empty id + no stages.
        let empty = Workflow {
            id: "  ".into(),
            stages: vec![],
            ..ok.clone()
        };
        assert_eq!(empty.validate().len(), 2);

        // Duplicate stage names.
        let dupe = Workflow {
            stages: vec![
                WorkflowStage {
                    name: "plan".into(),
                    purpose: None,
                    instructions: None,
                    reads: None,
                    writes: None,
                    gate: false,
                    exit: vec![],
                },
                WorkflowStage {
                    name: "plan".into(),
                    purpose: None,
                    instructions: None,
                    reads: None,
                    writes: None,
                    gate: false,
                    exit: vec![],
                },
            ],
            ..ok.clone()
        };
        assert!(dupe
            .validate()
            .iter()
            .any(|p| p.contains("duplicate stage")));

        // Path-traversal / nested artifact names are rejected.
        let unsafe_artifact = Workflow {
            stages: vec![WorkflowStage {
                name: "x".into(),
                purpose: None,
                instructions: None,
                reads: None,
                writes: Some("../escape.md".into()),
                gate: false,
                exit: vec![],
            }],
            ..ok.clone()
        };
        assert!(unsafe_artifact
            .validate()
            .iter()
            .any(|p| p.contains("unsafe artifact")));
    }

    #[test]
    fn is_safe_artifact_name_confines_to_a_bare_filename() {
        assert!(is_safe_artifact_name("plan.md"));
        assert!(is_safe_artifact_name("spec_v2.md"));
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "/etc/passwd", "x/"] {
            assert!(!is_safe_artifact_name(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn artifact_path_joins_under_artifacts_dir_and_rejects_escapes() {
        let repo = Path::new("/repo");
        let p = artifact_path(repo, "plan.md").unwrap();
        assert!(p.ends_with(".loadout/workflow/artifacts/plan.md"));
        assert_eq!(artifact_path(repo, "../escape.md"), None);
        // The stage helpers thread through the same guard.
        let stage = WorkflowStage {
            name: "plan".into(),
            purpose: None,
            instructions: None,
            reads: None,
            writes: Some("plan.md".into()),
            gate: false,
            exit: vec![],
        };
        assert_eq!(stage.write_path(repo), Some(p));
        assert_eq!(stage.read_path(repo), None);
    }

    #[test]
    fn artifact_env_var_derives_from_the_stem() {
        assert_eq!(
            artifact_env_var("plan.md").as_deref(),
            Some("LOADOUT_PLAN_PATH")
        );
        assert_eq!(
            artifact_env_var("spec.md").as_deref(),
            Some("LOADOUT_SPEC_PATH")
        );
        // Non-alphanumerics in the stem fold to underscores.
        assert_eq!(
            artifact_env_var("design-notes.md").as_deref(),
            Some("LOADOUT_DESIGN_NOTES_PATH")
        );
        assert_eq!(artifact_env_var("../escape.md"), None);
    }

    #[test]
    fn resolve_prefers_user_then_builtin_and_honors_disabled() {
        let builtins = builtin_workflows();
        // A built-in resolves directly (bind without copying).
        assert_eq!(
            resolve_workflow("lean", &[], &builtins).map(|w| w.id.as_str()),
            Some("lean")
        );
        // Unknown id → None (dangling binding degrades).
        assert!(resolve_workflow("nope", &[], &builtins).is_none());

        // A user workflow shadows a built-in of the same id.
        let user = vec![Workflow {
            id: "lean".into(),
            name: None,
            description: Some("my lean".into()),
            icon: None,
            stages: vec![WorkflowStage {
                name: "go".into(),
                purpose: None,
                instructions: None,
                reads: None,
                writes: None,
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: Layer::Global,
        }];
        assert_eq!(
            resolve_workflow("lean", &user, &builtins).map(|w| w.description.clone()),
            Some(Some("my lean".into()))
        );

        // A disabled user copy shadows AND suppresses the built-in (off means off).
        let disabled = vec![Workflow {
            disabled: true,
            ..user[0].clone()
        }];
        assert!(resolve_workflow("lean", &disabled, &builtins).is_none());
    }

    #[test]
    fn content_hash_is_stable_and_tracks_edits() {
        let mut w = builtin_workflows()
            .into_iter()
            .find(|w| w.id == "lean")
            .unwrap();
        let base = w.content_hash();
        assert_eq!(base, w.content_hash(), "deterministic for the same content");
        // The skipped `origin` field doesn't affect the hash.
        w.origin = Layer::Repo;
        assert_eq!(base, w.content_hash(), "origin is skipped from the hash");
        // Editing a stage's purpose changes it.
        w.stages[0].purpose = Some("edited".into());
        assert_ne!(base, w.content_hash(), "editing a stage changes the hash");
    }

    #[test]
    fn deserializes_minimal_and_full() {
        let minimal: Workflow = toml::from_str(
            r#"
            id = "x"
            [[stages]]
            name = "plan"
            "#,
        )
        .unwrap();
        assert_eq!(minimal.id, "x");
        assert_eq!(minimal.stages.len(), 1);
        assert!(!minimal.stages[0].gate);

        let full: Workflow = toml::from_str(
            r#"
            id = "spec"
            description = "Spec first"
            modeled_on = "Spec Kit"
            [[stages]]
            name = "specify"
            purpose = "write the spec"
            writes = "spec.md"
            [[stages]]
            name = "implement"
            reads = "spec.md"
            gate = true
            exit = ["criteria met"]
            "#,
        )
        .unwrap();
        assert_eq!(full.description.as_deref(), Some("Spec first"));
        assert_eq!(full.stages[0].writes.as_deref(), Some("spec.md"));
        assert!(full.stages[1].gate);
        assert_eq!(full.stages[1].exit, vec!["criteria met".to_string()]);
        // origin defaults to BuiltIn (it is `#[serde(skip)]`); the loader re-tags it.
        assert_eq!(full.origin, Layer::BuiltIn);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields guards against typos in a hand-written workflow.
        let err = toml::from_str::<Workflow>("id = \"x\"\nstagez = []\n");
        assert!(err.is_err(), "unknown top-level field must be rejected");
        let err = toml::from_str::<Workflow>(
            "id = \"x\"\n[[stages]]\nname = \"p\"\nwritez = \"plan.md\"\n",
        );
        assert!(err.is_err(), "unknown stage field must be rejected");
    }
}
