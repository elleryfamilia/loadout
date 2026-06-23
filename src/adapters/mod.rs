//! Agent integration: one descriptor-driven engine, not per-agent code.
//!
//! loadout produces **one** overlay (the rendered context for the active
//! profile). Everything agent-specific is *delivery*, captured declaratively by
//! an [`AgentDescriptor`]. The engine ([`apply`]) renders the overlay, writes it
//! as a gitignored artifact, and wires it up according to the descriptor:
//!
//! - **`importer`** set → auto-wire: a managed marker block that `@`-imports the
//!   overlay into a *local* file (e.g. Claude's `CLAUDE.local.md`). Safe to
//!   auto-wire because the importer is itself local/gitignored. With
//!   **`importer_registry`** also set, the importer's name is registered in the
//!   agent's own settings so it's actually loaded (e.g. Gemini's
//!   `~/.gemini/settings.json` `context.fileName`).
//! - **`override_target`** set → auto-wire (default-on): merge the overlay
//!   (inlined) into a gitignored override file the agent *prefers* over its
//!   committed instruction file (e.g. Codex reads `AGENTS.override.md` before
//!   `AGENTS.md`). Opt out with `--no-override` / `[codex] write_override`.
//! - otherwise (or override opted out) → **emit-only**: write the gitignored
//!   overlay and print a hint on how to wire it (committed instruction files
//!   like `AGENTS.md` are never touched).
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
    /// Filename under `.loadout/generated/`.
    pub generated_filename: String,
    /// Program to exec for `load run`, if launchable.
    #[serde(default)]
    pub launch: Option<String>,
    /// Local file to auto-wire via `@import` (e.g. `CLAUDE.local.md`).
    #[serde(default)]
    pub importer: Option<String>,
    /// Some agents only load the `importer` file once its name is registered in
    /// an external settings file. This declares that registration so the import
    /// is actually read (e.g. Gemini's `~/.gemini/settings.json` `context.fileName`).
    #[serde(default)]
    pub importer_registry: Option<ImporterRegistry>,
    /// Opt-in override file to merge the overlay into (e.g. `AGENTS.override.md`).
    #[serde(default)]
    pub override_target: Option<String>,
    /// Source file whose content seeds the override (e.g. `AGENTS.md`).
    #[serde(default)]
    pub override_base: Option<String>,
    /// Note shown in emit-only mode explaining how to wire the overlay.
    #[serde(default)]
    pub wire_hint: Option<String>,
    /// `load run` injects a freshness note via this flag, if set (e.g.
    /// Claude's `--append-system-prompt`).
    #[serde(default)]
    pub append_prompt_flag: Option<String>,
    /// `load run` sets this env var to [`launch_context_dir`] (an absolute path)
    /// so an agent with no persistent local hook discovers the overlay at launch
    /// (e.g. Copilot's `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`).
    #[serde(default)]
    pub launch_context_dir_env: Option<String>,
    /// Directory (relative to `.loadout/generated/`) that [`launch_context_dir_env`]
    /// points at. The agent scans it for its own instruction layout, so the
    /// `generated_filename` is written *inside* this dir in the shape the agent
    /// expects — e.g. Copilot scans `<dir>/.github/instructions/**/*.instructions.md`,
    /// so copilot uses dir `copilot` + file `copilot/.github/instructions/loadout.instructions.md`.
    #[serde(default)]
    pub launch_context_dir: Option<String>,
}

fn default_template() -> String {
    "overlay".to_string()
}

/// How to register an [`AgentDescriptor::importer`]'s filename in an agent's own
/// settings file so the agent actually loads it. The settings file is resolved
/// relative to the user's home dir; the importer's basename is ensured present in
/// the JSON string-array at `key_path`, seeding it with `default_name` (the
/// agent's built-in default) when the array doesn't exist yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImporterRegistry {
    /// Settings file relative to `$HOME` (e.g. `.gemini/settings.json`).
    pub settings_file: String,
    /// JSON object-key path to the registered string array (e.g.
    /// `["context", "fileName"]` for Gemini, `["instructions"]` for opencode).
    pub key_path: Vec<String>,
    /// The agent's built-in default, preserved when we first create the array
    /// (e.g. `GEMINI.md`). `None` for keys with no implicit default (opencode's
    /// `instructions`).
    #[serde(default)]
    pub default_name: Option<String>,
    /// The literal value to register. When `None`, the [`AgentDescriptor::importer`]
    /// basename is registered instead (Gemini registers `GEMINI.local.md`; opencode
    /// has no importer and registers the overlay path `.loadout/generated/opencode.md`).
    #[serde(default)]
    pub value: Option<String>,
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
            importer_registry: None,
            override_target: None,
            override_base: None,
            wire_hint: None,
            append_prompt_flag: None,
            launch_context_dir_env: None,
            launch_context_dir: None,
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
                "override writing is OFF — Codex won't see this overlay (it only reads \
                 AGENTS.md). Drop --no-override (or set [codex] write_override = true) to \
                 merge it into a gitignored AGENTS.override.md, which Codex prefers."
                    .into(),
            ),
            ..d("codex", "agents.md")
        },
        AgentDescriptor {
            display_name: Some("Gemini CLI".into()),
            launch: Some("gemini".into()),
            // Gemini has no built-in local-context filename, so auto-wire a
            // gitignored `GEMINI.local.md` (@import) and register that name in
            // `~/.gemini/settings.json` `context.fileName` so Gemini loads it
            // alongside the committed `GEMINI.md` (additive, never shadowing).
            importer: Some("GEMINI.local.md".into()),
            importer_registry: Some(ImporterRegistry {
                settings_file: ".gemini/settings.json".into(),
                key_path: vec!["context".into(), "fileName".into()],
                default_name: Some("GEMINI.md".into()),
                value: None, // registers the importer basename (GEMINI.local.md)
            }),
            wire_hint: Some(
                "Gemini reads GEMINI.md (and resolves @imports). To wire this overlay \
                 manually instead, add `@.loadout/generated/gemini.md` to a GEMINI.md."
                    .into(),
            ),
            ..d("gemini", "gemini.md")
        },
        AgentDescriptor {
            display_name: Some("opencode".into()),
            launch: Some("opencode".into()),
            // opencode's `instructions` config takes file paths/globs (resolved
            // per-project, missing ones skipped). Register the gitignored overlay's
            // path once in the global `~/.config/opencode/opencode.json` so opencode
            // loads it in every loadout-managed repo — additive, never touches a
            // committed `opencode.json` or `AGENTS.md`.
            importer_registry: Some(ImporterRegistry {
                settings_file: ".config/opencode/opencode.json".into(),
                key_path: vec!["instructions".into()],
                default_name: None,
                value: Some(".loadout/generated/opencode.md".into()),
            }),
            wire_hint: Some(
                "opencode reads AGENTS.md; add \".loadout/generated/opencode.md\" to the \
                 `instructions` array in opencode.json (loadout registers it globally)."
                    .into(),
            ),
            ..d("opencode", "opencode.md")
        },
        AgentDescriptor {
            display_name: Some("GitHub Copilot CLI".into()),
            launch: Some("copilot".into()),
            // The Copilot CLI has no gitignored persistent hook (its repo
            // .github/instructions discovery is gitignore-filtered, and
            // copilot-instructions.md / AGENTS.md are committed). So `load run`
            // points it at the gitignored overlay dir via an env var. The overlay
            // is written as a `.instructions.md` (with no `applyTo`, so Copilot
            // *inlines* it — a nested AGENTS.md would only become a "view this
            // file" pointer). Additive; never touches committed files.
            launch_context_dir_env: Some("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into()),
            launch_context_dir: Some("copilot".into()),
            wire_hint: Some(
                "`load run copilot` wires this via COPILOT_CUSTOM_INSTRUCTIONS_DIRS. \
                 For other entry points, point that env at .loadout/generated/copilot."
                    .into(),
            ),
            ..d(
                "copilot",
                "copilot/.github/instructions/loadout.instructions.md",
            )
        },
        AgentDescriptor {
            display_name: Some("Generic (AGENTS.md-style)".into()),
            wire_hint: Some(
                "Include .loadout/generated/generic.md from your agent's instruction file.".into(),
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
    /// Composed fragments + matching profiles.
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
    /// Force-write the override file even when config has it disabled.
    pub codex_override: bool,
    /// Suppress the override file (emit-only), overriding config + `--override`.
    pub codex_no_override: bool,
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
    /// The composed guidance body (the overlay minus header). Used by `run` to
    /// inject context at launch when the persistent importer was withheld.
    pub profile_guidance: String,
    /// True when a persistent importer/override was *not* written — because no
    /// profile applies, or because writing it would bleed into child repos
    /// (off-repo / `$HOME`). `run` then delivers context via the launch prompt.
    pub wiring_suppressed: bool,
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

    // 2. Wiring. A persistent importer/override is written everywhere it's safe.
    // The one place it isn't is `$HOME`: agents walk the directory tree upward,
    // so a managed importer at `$HOME` loads in *every* repo underneath it — the
    // "bleed". (A standalone off-repo project dir is fine; nothing inherits it.)
    // The gitignored generated overlay (step 1) is still written; it's reached
    // *only* through the wiring we're withholding, so withholding it at `$HOME`
    // prevents the bleed, and `run` delivers context at launch instead (Claude's
    // `--append-system-prompt`).
    let bleeds = is_home(app.repo_base());
    let want_override =
        !opts.codex_no_override && (opts.codex_override || app.config.codex.write_override);
    let suppress_wiring =
        bleeds && (d.importer.is_some() || (d.override_target.is_some() && want_override));

    if suppress_wiring {
        let what = d
            .importer
            .as_deref()
            .or(d.override_target.as_deref())
            .unwrap_or("the overlay");
        if d.append_prompt_flag.is_some() {
            notes.push(format!(
                "at $HOME — not writing {what} (it would load in every repo under here); \
                 context is injected at launch instead"
            ));
        } else {
            notes.push(format!(
                "at $HOME — not writing {what} (it would load in every repo under here); \
                 run inside a repo for persistent context"
            ));
        }
    } else if let Some(importer) = &d.importer {
        // Auto-wire: managed @import block in a local file.
        let path = app.repo_base().join(importer);
        let existed = path.exists();
        let import_line = format!("@.loadout/generated/{}", d.generated_filename);
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
        // Register the importer's name in the agent's own settings so it actually
        // loads (e.g. Gemini's global `~/.gemini/settings.json` `context.fileName`).
        if let Some(reg) = &d.importer_registry {
            apply_importer_registry(
                app,
                reg,
                Some(importer),
                &mut files,
                &mut notes,
                &mut warnings,
            )?;
        }
    } else if let (Some(ovr), true) = (&d.override_target, want_override) {
        // Auto-wire: merge the overlay (inlined) into a gitignored override file
        // that Codex prefers over the committed AGENTS.md.
        let override_path = app.repo_base().join(ovr);
        let base = d
            .override_base
            .as_ref()
            .and_then(|b| std::fs::read_to_string(app.repo_base().join(b)).ok());
        // Re-seed the file body from the live base whenever we (re)write it, so a
        // changed AGENTS.md is picked up (the freshness hash below forces that
        // rewrite). Fall back to any existing override, then to empty, when there
        // is no base. (A hand-edit to the override's base region with no other
        // change isn't auto-restored — it's a generated file; `refresh --force`
        // resets it.)
        let seed = base
            .clone()
            .or_else(|| std::fs::read_to_string(&override_path).ok())
            .unwrap_or_default();

        // Freshness for the override must track BOTH the loadout context and the
        // base file: a changed AGENTS.md with an unchanged context must still
        // rewrite. Fold the base content (only — never the existing override,
        // whose own embedded hash would make this unstable across runs) into the
        // skip-hash, and re-stamp the inlined overlay so its embedded marker
        // matches what we compare against next time.
        let base_for_hash = base.unwrap_or_default();
        let override_hash =
            crate::hash::context_hash(&(rendered.context_hash.as_str(), base_for_hash.as_str()));
        let body = rendered
            .content
            .replace(&rendered.context_hash, &override_hash);
        let new_content = writer::upsert_marker_block(Some(&seed), &body);

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
            &override_hash,
        )?);
        gitignore_extra.push(ovr.clone());
        if let Some(base) = &d.override_base {
            notes.push(format!(
                "{base} left untouched; overlay merged into {ovr} (Codex prefers it)"
            ));
        }
    } else if let Some(reg) = &d.importer_registry {
        // Registry-only wiring (no importer/override): the agent loads the overlay
        // directly once its path is registered in the agent's own settings (e.g.
        // opencode's `~/.config/opencode/opencode.json` `instructions`).
        apply_importer_registry(app, reg, None, &mut files, &mut notes, &mut warnings)?;
    } else if let Some(hint) = &d.wire_hint {
        // Emit-only: never touch committed instruction files.
        notes.push(hint.clone());
    }

    // 3. gitignore (only inside a repo): the loadout-managed dirs + the private
    // local.toml (binding + param overrides) + any root files we created. This
    // keeps a repo clean automatically on every render — there is no `init`.
    if app.in_repo() {
        let mut entries = vec![
            ".loadout/generated/".to_string(),
            ".loadout/cache/".to_string(),
            ".loadout/logs/".to_string(),
            ".loadout/local.toml".to_string(),
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
        profile_guidance: rendered.profile_guidance,
        wiring_suppressed: suppress_wiring,
    })
}

/// Whether `repo_base` is the user's `$HOME` — where a managed importer would be
/// inherited by every repo underneath it. Canonicalized so a symlinked home
/// still compares equal. Honors a `$HOME` override (used by tests).
fn is_home(repo_base: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(repo_base) == canon(&home)
}

/// Existing loadout-owned files for this agent (used by `clean` to discover what
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

/// Remove the artifacts loadout created for an agent: the generated overlay, any
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

    // Override file (loadout-owned, gitignored) → remove entirely.
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
    // Dry-run (and explain's dry apply) resolves dynamic fragments cache-only
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
/// the context hash, so the hash alone can't detect that live output changed —
/// always rewriting lets it land (the cache TTL still prevents re-executing the
/// probe).
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

/// Register a value in the agent's own settings file (resolved under `$HOME`) so
/// the agent actually loads the overlay, and warn if a workspace settings file
/// would mask that registration. The registered value is `reg.value` when set
/// (e.g. opencode's overlay path), else the `importer` basename (e.g. Gemini's
/// `GEMINI.local.md`). Appends to `files`/`notes`/`warnings`; degrades to a
/// warning (never corrupts) on any read/parse failure.
fn apply_importer_registry(
    app: &AppContext,
    reg: &ImporterRegistry,
    importer: Option<&str>,
    files: &mut Vec<WrittenFile>,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> crate::Result<()> {
    let key = reg.key_path.join(".");
    let Some(value) = reg.value.as_deref().or(importer) else {
        warnings.push(format!(
            "registry for {} has nothing to register (no `value` or importer)",
            reg.settings_file
        ));
        return Ok(());
    };
    let Some(home) = config::home_dir() else {
        warnings.push(format!(
            "$HOME unset — can't register {value} in {} `{key}`; add it by hand",
            reg.settings_file
        ));
        return Ok(());
    };
    let settings_path = home.join(&reg.settings_file);

    // Read the current settings. Only "not found" means "create fresh"; any other
    // read error (perms, non-UTF8) must NOT be mistaken for an empty file and
    // overwrite it.
    let existing = match std::fs::read_to_string(&settings_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warnings.push(format!(
                "could not read {} ({e}); add {value} to `{key}` by hand",
                settings_path.display()
            ));
            return Ok(());
        }
    };

    match register_context_name(
        existing.as_deref(),
        &reg.key_path,
        reg.default_name.as_deref(),
        value,
    ) {
        Ok(Some(updated)) => {
            files.push(app.writer.write(&settings_path, &updated)?);
            notes.push(format!(
                "registered {value} in {} ({key})",
                settings_path.display()
            ));
        }
        Ok(None) => {} // already registered — idempotent no-op
        Err(e) => warnings.push(format!(
            "could not update {} to register {value} ({e:#}); add it to `{key}` by hand",
            settings_path.display()
        )),
    }

    // A workspace settings file that sets the same key *replaces* (does not merge
    // with) the home one, masking the global registration. Warn rather than edit a
    // possibly-committed shared file.
    let workspace = app.repo_base().join(&reg.settings_file);
    if workspace != settings_path {
        if let Ok(text) = std::fs::read_to_string(&workspace) {
            if let Some(names) = read_string_list_at(&text, &reg.key_path) {
                if !names.iter().any(|n| n == value) {
                    warnings.push(format!(
                        "{} sets `{key}` and overrides the home registration — \
                         add {value} there too, or it won't load",
                        workspace.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Read the JSON string-array (or single string) at `key_path` in `text`.
/// Returns `None` if absent, unparseable, or not a string/array-of-strings.
fn read_string_list_at(text: &str, key_path: &[String]) -> Option<Vec<String>> {
    let mut cur: &serde_json::Value = &serde_json::from_str(text).ok()?;
    for k in key_path {
        cur = cur.get(k)?;
    }
    match cur {
        serde_json::Value::String(s) => Some(vec![s.clone()]),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}

/// Ensure `name` is present in the JSON string-array at `key_path` within
/// `existing` settings JSON (creating intermediate objects as needed). A freshly
/// created array is seeded with `default_name` (when `Some`) so the agent's
/// built-in default is preserved; pass `None` for keys with no implicit default
/// (e.g. opencode's `instructions`). A user-customized value (string or array) is
/// kept and only extended. Returns the new pretty-printed JSON when a change is
/// needed, or `None` when `name` is already registered (idempotent — no churn).
fn register_context_name(
    existing: Option<&str>,
    key_path: &[String],
    default_name: Option<&str>,
    name: &str,
) -> crate::Result<Option<String>> {
    use anyhow::{anyhow, bail, Context as _};
    use serde_json::{Map, Value};

    let (last, parents) = key_path
        .split_last()
        .ok_or_else(|| anyhow!("empty settings key_path"))?;

    let mut root: Value = match existing {
        Some(s) if !s.trim().is_empty() => {
            serde_json::from_str(s).context("parsing existing settings JSON")?
        }
        _ => Value::Object(Map::new()),
    };
    if !root.is_object() {
        bail!("settings root is not a JSON object");
    }

    // Descend (creating objects) to the parent of the target key.
    let mut cur = &mut root;
    for k in parents {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| anyhow!("settings path at '{k}' is not an object"))?;
        cur = obj
            .entry(k.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let obj = cur
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings path at '{last}' is not an object"))?;

    let mut names: Vec<String> = match obj.get(last) {
        None => default_name
            .map(|d| vec![d.to_string()])
            .unwrap_or_default(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| anyhow!("'{last}' array has a non-string entry"))
            })
            .collect::<crate::Result<_>>()?,
        Some(_) => bail!("'{last}' is not a string or array of strings"),
    };
    if names.iter().any(|n| n == name) {
        return Ok(None);
    }
    names.push(name.to_string());
    obj.insert(
        last.clone(),
        Value::Array(names.into_iter().map(Value::String).collect()),
    );

    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

#[cfg(test)]
mod register_tests {
    use super::register_context_name;

    fn keys() -> Vec<String> {
        vec!["context".into(), "fileName".into()]
    }

    #[test]
    fn creates_nested_array_seeded_with_default() {
        let out = register_context_name(None, &keys(), Some("GEMINI.md"), "GEMINI.local.md")
            .unwrap()
            .expect("should write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["context"]["fileName"],
            serde_json::json!(["GEMINI.md", "GEMINI.local.md"])
        );
    }

    #[test]
    fn creates_array_without_default_when_none() {
        // opencode's `instructions` has no implicit default → array is just [value].
        let out = register_context_name(
            None,
            &["instructions".to_string()],
            None,
            ".loadout/generated/opencode.md",
        )
        .unwrap()
        .expect("should write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["instructions"],
            serde_json::json!([".loadout/generated/opencode.md"])
        );
    }

    #[test]
    fn idempotent_when_already_present() {
        let existing = r#"{"context":{"fileName":["GEMINI.md","GEMINI.local.md"]}}"#;
        assert!(register_context_name(
            Some(existing),
            &keys(),
            Some("GEMINI.md"),
            "GEMINI.local.md"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn preserves_user_values_and_other_keys() {
        let existing = r#"{"context":{"fileName":"AGENTS.md","x":1},"ui":{"theme":"dark"}}"#;
        let out = register_context_name(
            Some(existing),
            &keys(),
            Some("GEMINI.md"),
            "GEMINI.local.md",
        )
        .unwrap()
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // User's custom value kept (NOT forced back to GEMINI.md), ours appended.
        assert_eq!(
            v["context"]["fileName"],
            serde_json::json!(["AGENTS.md", "GEMINI.local.md"])
        );
        assert_eq!(v["context"]["x"], serde_json::json!(1));
        assert_eq!(v["ui"]["theme"], serde_json::json!("dark"));
    }

    #[test]
    fn rejects_non_object_root_without_clobbering() {
        // A present-but-unexpected settings shape must error (caller warns + skips
        // the write) rather than silently overwrite the user's file.
        assert!(register_context_name(Some("[1,2,3]"), &keys(), Some("GEMINI.md"), "x").is_err());
        assert!(register_context_name(Some("not json"), &keys(), Some("GEMINI.md"), "x").is_err());
    }

    #[test]
    fn read_string_list_at_reads_string_array_or_none() {
        use super::read_string_list_at;
        let k = keys();
        assert_eq!(
            read_string_list_at(r#"{"context":{"fileName":["A.md","B.md"]}}"#, &k),
            Some(vec!["A.md".into(), "B.md".into()])
        );
        assert_eq!(
            read_string_list_at(r#"{"context":{"fileName":"A.md"}}"#, &k),
            Some(vec!["A.md".into()])
        );
        assert_eq!(read_string_list_at(r#"{"context":{}}"#, &k), None);
        assert_eq!(read_string_list_at("{bad json", &k), None);
    }
}
