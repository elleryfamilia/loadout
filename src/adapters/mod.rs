//! Agent integration: one descriptor-driven engine, not per-agent code.
//!
//! rosita produces **one** overlay (the rendered context for the active
//! profile). Everything agent-specific is *delivery*, captured declaratively by
//! an [`AgentDescriptor`]. The engine ([`apply`]) renders the overlay, writes it
//! as a gitignored artifact, and wires it up according to the descriptor:
//!
//! - **`importer`** set → auto-wire: a managed marker block that `@`-imports the
//!   overlay into a *local* file (e.g. Claude's `CLAUDE.local.md`). Safe to
//!   auto-wire because the importer is itself local/gitignored.
//! - **`override_target`** set + opted in → merge the overlay (inlined) into a
//!   gitignored override file (e.g. Codex's `AGENTS.override.md`).
//! - otherwise → **emit-only**: write the gitignored overlay and print a hint on
//!   how to wire it (committed instruction files like `AGENTS.md` are never
//!   touched by default).
//!
//! New agents are descriptor rows ([`builtin_agents`]) or `[[agents]]` config
//! entries — not new code.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::context::Context;
use crate::profile::Composition;
use crate::render::{self, header, RenderRequest};
use crate::writer::{self, WriteAction, Writer, WrittenFile};

/// A declarative description of how to deliver the overlay to one agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    /// Stable agent id (`claude`, `codex`, `gemini`, …).
    pub id: String,
    /// Human-friendly name (defaults to the id).
    #[serde(default)]
    pub display_name: Option<String>,
    /// Body template name (resolved repo → global → embedded overlay).
    #[serde(default = "default_template")]
    pub template: String,
    /// Filename under `.rosita/generated/`.
    pub generated_filename: String,
    /// Program to exec for `rosita run`, if launchable.
    #[serde(default)]
    pub launch: Option<String>,
    /// Local file to auto-wire via `@import` (e.g. `CLAUDE.local.md`).
    #[serde(default)]
    pub importer: Option<String>,
    /// Opt-in override file to merge the overlay into (e.g. `AGENTS.override.md`).
    #[serde(default)]
    pub override_target: Option<String>,
    /// Source file whose content seeds the override (e.g. `AGENTS.md`).
    #[serde(default)]
    pub override_base: Option<String>,
    /// Note shown in emit-only mode explaining how to wire the overlay.
    #[serde(default)]
    pub wire_hint: Option<String>,
    /// `rosita run` injects a freshness note via this flag, if set (e.g.
    /// Claude's `--append-system-prompt`).
    #[serde(default)]
    pub append_prompt_flag: Option<String>,
}

fn default_template() -> String {
    "overlay".to_string()
}

impl AgentDescriptor {
    /// Display name, falling back to the id.
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

/// The built-in agent descriptors. Overridable by id via `[[agents]]` in config.
pub fn builtin_agents() -> Vec<AgentDescriptor> {
    fn d(id: &str, file: &str) -> AgentDescriptor {
        AgentDescriptor {
            id: id.into(),
            display_name: None,
            template: default_template(),
            generated_filename: file.into(),
            launch: None,
            importer: None,
            override_target: None,
            override_base: None,
            wire_hint: None,
            append_prompt_flag: None,
        }
    }
    vec![
        AgentDescriptor {
            display_name: Some("Claude Code".into()),
            launch: Some("claude".into()),
            importer: Some("CLAUDE.local.md".into()),
            append_prompt_flag: Some("--append-system-prompt".into()),
            ..d("claude", "claude.md")
        },
        AgentDescriptor {
            display_name: Some("OpenAI Codex CLI".into()),
            launch: Some("codex".into()),
            override_target: Some("AGENTS.override.md".into()),
            override_base: Some("AGENTS.md".into()),
            wire_hint: Some(
                "Codex reads AGENTS.md; pass --override to also write a (gitignored) \
                 AGENTS.override.md that merges your AGENTS.md with this overlay."
                    .into(),
            ),
            ..d("codex", "agents.md")
        },
        AgentDescriptor {
            display_name: Some("Gemini CLI".into()),
            launch: Some("gemini".into()),
            wire_hint: Some(
                "Gemini reads AGENTS.md/GEMINI.md (and resolves @imports). Add \
                 `@.rosita/generated/gemini.md` to a local GEMINI.md, or copy it in."
                    .into(),
            ),
            ..d("gemini", "gemini.md")
        },
        AgentDescriptor {
            display_name: Some("opencode".into()),
            launch: Some("opencode".into()),
            wire_hint: Some(
                "opencode reads AGENTS.md; add \".rosita/generated/opencode.md\" to the \
                 `instructions` array in opencode.json."
                    .into(),
            ),
            ..d("opencode", "opencode.md")
        },
        AgentDescriptor {
            display_name: Some("GitHub Copilot".into()),
            // Copilot is IDE/cloud-driven: render-only (no launch) by default.
            wire_hint: Some(
                "Copilot reads .github/copilot-instructions.md and AGENTS.md; include \
                 .rosita/generated/copilot.md there."
                    .into(),
            ),
            ..d("copilot", "copilot.md")
        },
        AgentDescriptor {
            display_name: Some("Generic (AGENTS.md-style)".into()),
            wire_hint: Some(
                "Include .rosita/generated/generic.md from your agent's instruction file.".into(),
            ),
            ..d("generic", "generic.md")
        },
    ]
}

/// Look up a descriptor by id within the loaded config.
pub fn descriptor<'a>(config: &'a Config, id: &str) -> Option<&'a AgentDescriptor> {
    config.agents.iter().find(|a| a.id == id)
}

/// All configured agent ids, in declaration order.
pub fn agent_ids(config: &Config) -> Vec<String> {
    config.agents.iter().map(|a| a.id.clone()).collect()
}

/// Everything the engine needs to apply a descriptor.
pub struct AppContext<'a> {
    /// Detected context.
    pub context: &'a Context,
    /// Composed capabilities + matching profiles.
    pub composition: &'a Composition,
    /// Merged config.
    pub config: &'a Config,
    /// Injected RFC3339 timestamp.
    pub generated_at: String,
    /// The writer (apply or dry-run).
    pub writer: &'a dyn Writer,
}

impl AppContext<'_> {
    fn repo_base(&self) -> &Path {
        &self.context.repo_base
    }
    fn in_repo(&self) -> bool {
        self.context.git.is_some()
    }
}

/// Knobs controlling how the engine applies.
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Opt-in to writing an override file (in addition to config).
    pub codex_override: bool,
    /// Re-render even when the context hash is unchanged.
    pub force: bool,
}

/// What an apply did.
pub struct ApplyResult {
    /// Files written / would-write / unchanged.
    pub files: Vec<WrittenFile>,
    /// Non-fatal warnings (size limits, etc.).
    pub warnings: Vec<String>,
    /// Informational notes (e.g. how to wire an emit-only overlay).
    pub notes: Vec<String>,
    /// Context hash of this render.
    pub context_hash: String,
}

/// Render the overlay and wire it up per the descriptor.
pub fn apply(
    d: &AgentDescriptor,
    app: &AppContext,
    opts: &ApplyOptions,
) -> crate::Result<ApplyResult> {
    let rendered = render_overlay(d, app)?;
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    // Root-level files we created and therefore should gitignore.
    let mut gitignore_extra: Vec<String> = Vec::new();
    // Dynamic overlays always rewrite (volatile output is excluded from the hash).
    let force = opts.force || rendered.has_dynamic;

    // 1. Always: the gitignored generated overlay.
    let gen = generated_path(app, &d.generated_filename);
    files.push(write_hash_skipping(
        app,
        force,
        &gen,
        &rendered.content,
        &rendered.context_hash,
    )?);

    // 2. Wiring.
    let want_override = opts.codex_override || app.config.codex.write_override;
    if let Some(importer) = &d.importer {
        // Auto-wire: managed @import block in a local file.
        let path = app.repo_base().join(importer);
        let existed = path.exists();
        let import_line = format!("@.rosita/generated/{}", d.generated_filename);
        let existing = std::fs::read_to_string(&path).ok();
        let new_content = writer::upsert_marker_block(existing.as_deref(), &import_line);
        let wf = app.writer.write(&path, &new_content)?;
        if wf.action == WriteAction::Created {
            notes.push(format!("created {importer} importing {import_line}"));
        }
        files.push(wf);
        // Only gitignore the importer if WE created it (don't touch a tracked file).
        if !existed {
            gitignore_extra.push(importer.clone());
        }
    } else if let (Some(ovr), true) = (&d.override_target, want_override) {
        // Opt-in: merge the overlay (inlined) into a gitignored override file.
        let override_path = app.repo_base().join(ovr);
        let existing_override = std::fs::read_to_string(&override_path).ok();
        let seed = match &existing_override {
            Some(s) => s.clone(),
            None => d
                .override_base
                .as_ref()
                .and_then(|b| std::fs::read_to_string(app.repo_base().join(b)).ok())
                .unwrap_or_default(),
        };
        let new_content = writer::upsert_marker_block(Some(&seed), &rendered.content);

        let limit = app.config.codex.max_output_kib.saturating_mul(1024) as usize;
        if limit > 0 && new_content.len() > limit {
            warnings.push(format!(
                "{ovr} is {} KiB, exceeding the {} KiB limit (raise [codex] max_output_kib to silence)",
                new_content.len() / 1024,
                app.config.codex.max_output_kib
            ));
        }
        files.push(write_hash_skipping(
            app,
            force,
            &override_path,
            &new_content,
            &rendered.context_hash,
        )?);
        gitignore_extra.push(ovr.clone());
        if let Some(base) = &d.override_base {
            notes.push(format!("{base} left untouched; overlay merged into {ovr}"));
        }
    } else if let Some(hint) = &d.wire_hint {
        // Emit-only: never touch committed instruction files.
        notes.push(hint.clone());
    }

    // 3. gitignore (only inside a repo): the rosita-managed dirs + the private
    // local.toml (binding + param overrides) + any root files we created. This
    // keeps a repo clean automatically on every render — there is no `init`.
    if app.in_repo() {
        let mut entries = vec![
            ".rosita/generated/".to_string(),
            ".rosita/cache/".to_string(),
            ".rosita/logs/".to_string(),
            ".rosita/local.toml".to_string(),
        ];
        entries.extend(gitignore_extra);
        if let Some(wf) = ensure_gitignored(app, &entries)? {
            files.push(wf);
        }
    }

    Ok(ApplyResult {
        files,
        warnings,
        notes,
        context_hash: rendered.context_hash,
    })
}

/// Existing rosita-owned files for this agent (used by `clean` to discover what
/// to remove). Does not include committed instruction files we never touch.
pub fn artifacts(d: &AgentDescriptor, repo_base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let gen = config::generated_dir(repo_base).join(&d.generated_filename);
    if gen.exists() {
        out.push(gen);
    }
    if let Some(ovr) = &d.override_target {
        let p = repo_base.join(ovr);
        if p.exists() {
            out.push(p);
        }
    }
    if let Some(importer) = &d.importer {
        let p = repo_base.join(importer);
        let has_block = std::fs::read_to_string(&p)
            .map(|c| c.contains(writer::BLOCK_BEGIN))
            .unwrap_or(false);
        if has_block {
            out.push(p);
        }
    }
    out
}

/// Result of [`clean`].
pub struct CleanResult {
    /// Files removed (or that would be removed in dry-run).
    pub removed: Vec<PathBuf>,
    /// Files modified (managed block stripped) without full removal.
    pub modified: Vec<PathBuf>,
    /// Informational notes.
    pub notes: Vec<String>,
}

/// Remove the artifacts rosita created for an agent: the generated overlay, any
/// override file, and our managed block in the importer (deleting the importer
/// if nothing else remains). Never touches committed instruction files.
pub fn clean(d: &AgentDescriptor, app: &AppContext) -> crate::Result<CleanResult> {
    let dry = app.writer.is_dry_run();
    let mut removed = Vec::new();
    let mut modified = Vec::new();
    let mut notes = Vec::new();

    // Generated overlay.
    let gen = generated_path(app, &d.generated_filename);
    if gen.exists() {
        if !dry {
            std::fs::remove_file(&gen).ok();
        }
        removed.push(gen);
    }

    // Override file (rosita-owned, gitignored) → remove entirely.
    if let Some(ovr) = &d.override_target {
        let p = app.repo_base().join(ovr);
        if p.exists() {
            if !dry {
                std::fs::remove_file(&p).ok();
            }
            removed.push(p);
        }
    }

    // Importer: strip our managed block; delete the file if nothing else is left.
    if let Some(importer) = &d.importer {
        let p = app.repo_base().join(importer);
        if let Ok(content) = std::fs::read_to_string(&p) {
            if content.contains(writer::BLOCK_BEGIN) {
                let stripped = writer::remove_marker_block(&content);
                if stripped.trim().is_empty() {
                    if !dry {
                        std::fs::remove_file(&p).ok();
                    }
                    removed.push(p);
                } else {
                    if !dry {
                        writer::atomic_write(&p, &stripped)?;
                    }
                    modified.push(p);
                }
            }
        }
    }

    notes.push("committed instruction files (AGENTS.md, GEMINI.md, …) were not touched".into());
    if app.in_repo() {
        notes.push("left .gitignore entries in place (remove them by hand if desired)".into());
    }

    Ok(CleanResult {
        removed,
        modified,
        notes,
    })
}

// --- shared mechanics --------------------------------------------------------

fn render_overlay(d: &AgentDescriptor, app: &AppContext) -> crate::Result<render::RenderOutput> {
    // Dry-run (and explain's dry apply) resolves dynamic capabilities cache-only
    // — never executing providers/commands or writing — so it touches nothing.
    let dynamic = if app.writer.is_dry_run() {
        crate::dynamic::DynamicMode::ReadOnly
    } else {
        crate::dynamic::DynamicMode::Live
    };
    render::render(&RenderRequest {
        agent: &d.id,
        template_name: &d.template,
        context: app.context,
        composition: app.composition,
        config: app.config,
        generated_at: app.generated_at.clone(),
        dynamic,
    })
}

/// Write `content` to `path`, skipping when the embedded context hash already
/// matches (unless `force`). Keeps renders idempotent despite the timestamp.
///
/// Dynamic overlays pass `force = true`: their volatile output is excluded from
/// the context hash, so the hash alone can't detect that live output or a trust
/// decision changed — always rewriting lets those land (the cache TTL still
/// prevents re-executing the probe).
fn write_hash_skipping(
    app: &AppContext,
    force: bool,
    path: &Path,
    content: &str,
    new_hash: &str,
) -> crate::Result<WrittenFile> {
    if !force {
        if let Ok(existing) = std::fs::read_to_string(path) {
            if header::extract_context_hash(&existing).as_deref() == Some(new_hash) {
                return Ok(WrittenFile {
                    path: path.to_path_buf(),
                    action: WriteAction::Unchanged,
                    bytes: content.len(),
                });
            }
        }
    }
    app.writer.write(path, content)
}

/// Ensure each entry is present in `.gitignore`, writing once if anything was
/// added. Caller guarantees we're inside a repo.
fn ensure_gitignored(app: &AppContext, entries: &[String]) -> crate::Result<Option<WrittenFile>> {
    let gitignore = app.repo_base().join(".gitignore");
    let mut content = std::fs::read_to_string(&gitignore).ok();
    let mut changed = false;
    for entry in entries {
        if let Some(updated) = writer::ensure_line(content.as_deref(), entry) {
            content = Some(updated);
            changed = true;
        }
    }
    if changed {
        if let Some(c) = content {
            return Ok(Some(app.writer.write(&gitignore, &c)?));
        }
    }
    Ok(None)
}

/// Path to a generated overlay file.
fn generated_path(app: &AppContext, filename: &str) -> PathBuf {
    config::generated_dir(app.repo_base()).join(filename)
}
