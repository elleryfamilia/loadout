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
//! - **`target_file`** set → auto-wire: write the overlay raw (optionally
//!   after a `preamble`, e.g. MDC frontmatter) as a fully loadout-owned,
//!   gitignored file at that path (e.g. Cursor's `.cursor/rules/loadout.mdc`).
//!   No marker-block wrap; a foreign file at the path is never overwritten.
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
use crate::workflow::Workflow;
use crate::writer::{self, WriteAction, Writer, WrittenFile};

pub mod commands;
mod hooks_claude;

// The one sentence both surfaces (hook registration notes and doctor's
// Learning section) print for Claude's `disableAllHooks: true` state — a
// single constant so the wording can never drift between them.
pub use hooks_claude::DISABLE_ALL_HOOKS_NOTE;

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
    /// Extra tokens that resolve to this agent wherever an agent is named —
    /// for CLIs known by more than one binary (Cursor installs `agent` as an
    /// alias of `cursor-agent`). The `launch` program is always accepted; these
    /// add to it.
    #[serde(default)]
    pub aliases: Vec<String>,
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
    /// Repo-relative path of a fully loadout-owned wired file, written **raw**
    /// (`preamble` + overlay, no marker-block wrap) — for agents whose
    /// instruction files must be loadout's content as the *entire* file in a
    /// specific shape (e.g. Cursor's `.cursor/rules/loadout.mdc`, whose MDC
    /// frontmatter must be the file's first bytes). Gitignored; a pre-existing
    /// file without loadout's generated marker is never overwritten.
    #[serde(default)]
    pub target_file: Option<String>,
    /// Raw first bytes of [`target_file`](Self::target_file) (e.g. MDC
    /// frontmatter). The generated-marker header follows it, so hash-based
    /// freshness detection still works.
    #[serde(default)]
    pub preamble: Option<String>,
    /// User-level lifecycle-hook registration that keeps the overlay fresh for
    /// sessions loadout doesn't launch (e.g. the Cursor IDE).
    #[serde(default)]
    pub hook_registry: Option<HookRegistry>,
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
    /// Project-relative directory this agent reads slash commands from (e.g.
    /// `.claude/commands`). When set **and** a workflow is bound, loadout writes
    /// one command file per stage under `<commands_dir>/loadout/` (a dir it owns).
    /// `None` → the agent gets the workflow context section only.
    #[serde(default)]
    pub commands_dir: Option<String>,
    /// On-disk format for this agent's command files (markdown vs Gemini TOML).
    /// Ignored unless `commands_dir` is set; defaults to markdown.
    #[serde(default)]
    pub command_format: Option<commands::CommandFormat>,
    /// Native review commands to run during the verify stage (e.g. Claude
    /// Code's `/code-review`, `/security-review`). Built-in descriptor data
    /// only — deliberately NOT part of the `[[agents]]` config schema
    /// (`deny_unknown_fields` still rejects it there), so a newer-written,
    /// synced config can never brick an older binary.
    #[serde(skip)]
    pub review_commands: Vec<String>,
    /// Session-end hooks that drive ambient learning, registered by the passive
    /// bootstrap ONLY while learning is active on this machine and removed by
    /// [`remove_learn_hooks`] on `load learn off`. Kept separate from
    /// [`hook_registry`](Self::hook_registry) (the always-on freshness hook) so
    /// a routine refresh can never re-add them once learning is off. Descriptor
    /// data only (`#[serde(skip)]`), like [`review_commands`](Self::review_commands).
    #[serde(skip)]
    pub learn_hooks: Vec<HookRegistry>,
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

/// What a registered hook is *for*, so the passive bootstrap and the removal
/// path can tell loadout's two hook families apart. Freshness hooks keep the
/// overlay current in IDE sessions and register on every refresh; learn hooks
/// drive ambient harvesting and register only while learning is active on this
/// machine (`load learn on`). Code-side descriptor data only (`#[serde(skip)]`,
/// the `review_commands` precedent) — never a config-schema key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookPurpose {
    /// Keeps the overlay fresh for sessions loadout doesn't launch (the IDE).
    #[default]
    Freshness,
    /// Fires ambient learning at session end.
    Learn,
}

/// The on-disk shape of an agent's hooks file, so registration writes the right
/// dialect. `Flat` is the single-array-of-`{command}` layout Cursor uses;
/// `ClaudeNested` is Claude Code's nested matcher schema in `.claude/settings.json`,
/// written by [`hooks_claude`]. Both [`apply_hook_registry_at`] and
/// [`remove_learn_hooks_at`] route on this so a flat `{command}` line is never
/// written into the nested file (which would corrupt it). Code-side descriptor
/// data only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookFormat {
    /// Flat `{ hooks: { <event>: [ { command } ] } }` (Cursor's `hooks.json`).
    #[default]
    Flat,
    /// Claude Code's nested matcher schema in `.claude/settings.json`
    /// (`hooks.<event>: [{ matcher?, hooks: [{ type, command, timeout? }] }]`).
    ClaudeNested,
}

/// Registration of a loadout freshness hook in an agent's **user-level** hooks
/// file (e.g. Cursor's `~/.cursor/hooks.json`): the agent then re-renders the
/// overlay itself on its own lifecycle events — the freshness path for IDE
/// sessions, which never go through `load run`. Registration is global and
/// idempotent; loadout's entry is matched by the `subcommand` suffix (so a
/// moved binary is re-pointed, not duplicated) and every other tool's entry is
/// preserved value-identically. (Preservation is semantic, not byte-exact: the
/// file is parsed to a `serde_json::Value` and re-emitted, which sorts each
/// object's keys — a rich foreign entry keeps all its fields and values but may
/// see its keys reordered.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRegistry {
    /// Hooks file relative to `$HOME` (e.g. `.cursor/hooks.json`).
    pub hooks_file: String,
    /// Event to register under (e.g. `sessionStart`).
    pub event: String,
    /// The `load` subcommand the hook runs (e.g. `hook cursor`). The registered
    /// command is `"<current load binary>" <subcommand>`; the suffix also
    /// identifies our entry for updates and `--remove`.
    ///
    /// CAUTION: entries are matched by `command.ends_with(" <subcommand>")`, so
    /// this discrimination is safe only while no subcommand's ` <subcommand>`
    /// string is a suffix of another subcommand's full command line. Today it
    /// holds — `hook cursor` is NOT a suffix of `hook cursor --event
    /// session-end` (which ends in `session-end`), and vice versa. Adding a
    /// subcommand like `session-end` (a trailing substring of an existing one)
    /// would break it: prefer distinct, mutually non-suffixing tails.
    pub subcommand: String,
    /// Whether the hook may **adopt** a repo on first open — wiring this agent
    /// into a git repo some loadout applies to, with no prior `load refresh`
    /// there (default: true). Off → the hook only refreshes repos already
    /// adopted by hand.
    #[serde(default = "default_true")]
    pub auto_adopt: bool,
    /// Whether this hook keeps the overlay fresh (`Freshness`) or drives ambient
    /// learning (`Learn`). Descriptor data only — never serialized, so a synced
    /// `[[agents]]` override can't set it (the `review_commands` precedent).
    /// Defaults to `Freshness`.
    #[serde(skip)]
    pub purpose: HookPurpose,
    /// The hooks-file dialect this entry writes (`Flat` vs `ClaudeNested`).
    /// Descriptor data only; defaults to `Flat`.
    #[serde(skip)]
    pub format: HookFormat,
}

fn default_true() -> bool {
    true
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
            aliases: Vec::new(),
            importer: None,
            importer_registry: None,
            override_target: None,
            override_base: None,
            target_file: None,
            preamble: None,
            hook_registry: None,
            wire_hint: None,
            append_prompt_flag: None,
            launch_context_dir_env: None,
            launch_context_dir: None,
            commands_dir: None,
            command_format: None,
            review_commands: Vec::new(),
            learn_hooks: Vec::new(),
        }
    }
    vec![
        AgentDescriptor {
            display_name: Some("Claude Code".into()),
            launch: Some("claude".into()),
            importer: Some("CLAUDE.local.md".into()),
            append_prompt_flag: Some("--append-system-prompt".into()),
            // Claude reads project commands from `.claude/commands/`; a `loadout/`
            // subdir namespaces them as `/loadout:<stage>`.
            commands_dir: Some(".claude/commands".into()),
            review_commands: vec!["/code-review".into(), "/security-review".into()],
            // Ambient learning: a SessionEnd hook fires the harvest fast path when
            // a Claude session ends. Written in the nested matcher schema of
            // `.claude/settings.json` (`format: ClaudeNested`) by `hooks_claude`.
            learn_hooks: vec![HookRegistry {
                hooks_file: ".claude/settings.json".into(),
                event: "SessionEnd".into(),
                subcommand: "hook claude --event session-end".into(),
                auto_adopt: false,
                purpose: HookPurpose::Learn,
                format: HookFormat::ClaudeNested,
            }],
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
            // Gemini reads project commands from `.gemini/commands/` as TOML; a
            // `loadout/` subdir namespaces them as `/loadout:<stage>`.
            commands_dir: Some(".gemini/commands".into()),
            command_format: Some(commands::CommandFormat::Toml),
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
            display_name: Some("Cursor (IDE + CLI)".into()),
            launch: Some("cursor-agent".into()),
            // Cursor ships `agent` as an alias binary of `cursor-agent`.
            aliases: vec!["agent".into()],
            // Cursor — IDE agent and `cursor-agent` CLI alike — reads project
            // rules from `.cursor/rules/*.mdc`; an `alwaysApply: true` rule is
            // always-on, and rules discovery is NOT gitignore-filtered
            // (verified live, unlike Copilot above). So one gitignored,
            // loadout-owned rule file wires both surfaces. MDC frontmatter
            // must be the file's first bytes, hence `preamble` + the raw
            // `target_file` write (no marker-block wrap).
            target_file: Some(".cursor/rules/loadout.mdc".into()),
            preamble: Some(
                "---\ndescription: loadout — the user's personal cross-project context\n\
                 alwaysApply: true\n---\n\n"
                    .into(),
            ),
            // IDE freshness: a user-level sessionStart hook re-renders adopted
            // repos (`load hook cursor` self-gates + debounces — Cursor fires
            // more than one session event per window open, verified live).
            hook_registry: Some(HookRegistry {
                hooks_file: ".cursor/hooks.json".into(),
                event: "sessionStart".into(),
                subcommand: "hook cursor".into(),
                auto_adopt: true,
                purpose: HookPurpose::Freshness,
                format: HookFormat::Flat,
            }),
            // Ambient learning: Cursor's `stop` event fires the harvest fast path
            // when a session ends. Flat dialect (same `hooks.json` as the freshness
            // hook), but a distinct `--event session-end` subcommand suffix so the
            // two are never confused during registration or removal.
            learn_hooks: vec![HookRegistry {
                hooks_file: ".cursor/hooks.json".into(),
                event: "stop".into(),
                subcommand: "hook cursor --event session-end".into(),
                auto_adopt: false,
                purpose: HookPurpose::Learn,
                format: HookFormat::Flat,
            }],
            wire_hint: Some(
                "Cursor reads .cursor/rules/*.mdc; loadout writes the overlay to a \
                 gitignored .cursor/rules/loadout.mdc (alwaysApply: true)."
                    .into(),
            ),
            // Cursor Skills: `.cursor/skills/<category>/<skill>/SKILL.md` — the
            // leaf folder names the skill (the category above it doesn't), so
            // loadout owns `.cursor/skills/loadout/` whole and the stages
            // invoke as `/loadout-<stage>`.
            commands_dir: Some(".cursor/skills".into()),
            command_format: Some(commands::CommandFormat::Skill),
            ..d("cursor", "cursor.md")
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

/// Resolve a user-supplied agent token to a descriptor: an exact id match
/// first, else a **unique** match on the agent's `launch` program or declared
/// `aliases` — people type the binary they know (`load cursor-agent`, or
/// Cursor's `agent` alias) for an agent whose id is shorter (`cursor`).
/// Ambiguous matches resolve to nothing.
pub fn resolve_agent_token<'a>(config: &'a Config, token: &str) -> Option<&'a AgentDescriptor> {
    if let Some(d) = descriptor(config, token) {
        return Some(d);
    }
    let mut matches = config
        .agents
        .iter()
        .filter(|a| a.launch.as_deref() == Some(token) || a.aliases.iter().any(|al| al == token));
    match (matches.next(), matches.next()) {
        (Some(d), None) => Some(d),
        _ => None,
    }
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
    /// Count of `Pending` learn candidates awaiting review — computed ONCE
    /// per command invocation by the caller (see
    /// `commands::apply::learn_pending_count`) and threaded through to the
    /// rendered header's discovery line. `0` for callers outside the four
    /// learn entry points (`explain`, `clean`) that don't need it.
    pub learn_pending: usize,
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
    /// `load run --workflow <id>` override: render this workflow instead of the
    /// profile's bound one for this apply. `None` → use the profile's binding.
    pub workflow_override: Option<String>,
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
    // The workflow (if any) for this apply: a `--workflow` override wins,
    // otherwise the selected profile's binding. A dangling id resolves to `None`
    // and simply isn't rendered (doctor/run surface it). Used by both render
    // channels: the context section and the per-stage commands.
    let workflow = app.config.resolve_active_workflow(
        opts.workflow_override.as_deref(),
        app.composition.primary_profile(),
    );
    let mut rendered = render_overlay(d, app, workflow.as_ref())?;
    rendered.content = redact_artifact(std::mem::take(&mut rendered.content), "overlay");
    // `profile_guidance` is injected into the launch prompt off-repo
    // (`--append-system-prompt`), not just written to the overlay. Its fragment
    // sections were already redacted at render, but the appended workflow-map
    // section was not — scrub it here so a secret in a workflow step can't reach
    // the agent's prompt. Silent (no warning): the same secret already surfaced
    // via the overlay pass above; this is idempotent over already-redacted text.
    rendered.profile_guidance = crate::redact::redact_secrets(&rendered.profile_guidance);
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
    let suppress_wiring = bleeds
        && (d.importer.is_some()
            || d.target_file.is_some()
            || (d.override_target.is_some() && want_override));

    if suppress_wiring {
        let what = d
            .importer
            .as_deref()
            .or(d.override_target.as_deref())
            .or(d.target_file.as_deref())
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
    } else if let Some(target) = &d.target_file {
        // Owned-target wiring: the overlay IS the file — written raw (preamble
        // first, e.g. Cursor's MDC frontmatter, then header + body), with no
        // marker-block wrap and independent of the [codex] override knobs.
        // Never overwrites a file loadout didn't generate.
        let path = app.repo_base().join(target);
        let foreign = std::fs::read_to_string(&path)
            .map(|c| !c.contains(header::GENERATED_MARKER))
            .unwrap_or(false);
        if foreign {
            warnings.push(format!(
                "{target} exists but wasn't generated by loadout — not overwriting \
                 (move it aside and re-run to wire {})",
                d.id
            ));
        } else {
            let content = match &d.preamble {
                Some(p) => format!("{p}{}", rendered.content),
                None => rendered.content.clone(),
            };
            files.push(write_hash_skipping(
                app,
                force,
                &path,
                &content,
                &rendered.context_hash,
            )?);
            gitignore_extra.push(target.clone());
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

    // 2b. Per-stage slash commands (the command channel), for agents that read
    // project commands. Only inside a repo and never at $HOME — a global command
    // dir (`~/.claude/commands/`) would bleed into every repo, exactly like an
    // importer. One file per stage under `<commands_dir>/loadout/`, a dir we own.
    if let (Some(commands_dir), Some(wf)) = (&d.commands_dir, workflow.as_ref()) {
        if app.in_repo() && !bleeds && is_repo_relative(commands_dir) {
            write_stage_commands(app, d, commands_dir, wf, &mut files)?;
            gitignore_extra.push(format!("{commands_dir}/{}/", commands::COMMAND_NAMESPACE));
        }
    }

    // 2c. User-level freshness hook (e.g. Cursor's sessionStart): registered
    // whenever we're wiring at all. Global + idempotent, like the importer
    // registries — the hook subcommand self-gates per repo, so registering
    // from any one repo is safe for all.
    if let (Some(hr), false) = (&d.hook_registry, suppress_wiring) {
        apply_hook_registry(app.writer, hr, &mut files, &mut notes, &mut warnings)?;
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
    if let Some(target) = &d.target_file {
        // Only ours if it carries the generated marker (collision guard).
        let p = repo_base.join(target);
        let ours = std::fs::read_to_string(&p)
            .map(|c| c.contains(header::GENERATED_MARKER))
            .unwrap_or(false);
        if ours {
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
    // Generated slash-command dir (loadout-owned).
    if let Some(commands_dir) = &d.commands_dir {
        let ns = command_namespace_dir(repo_base, commands_dir);
        if ns.exists() {
            out.push(ns);
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

    // Owned target file → remove only when it carries our generated marker
    // (respects the collision guard: a foreign file was never ours to delete).
    if let Some(target) = &d.target_file {
        let p = app.repo_base().join(target);
        let ours = std::fs::read_to_string(&p)
            .map(|c| c.contains(header::GENERATED_MARKER))
            .unwrap_or(false);
        if ours {
            if !dry {
                std::fs::remove_file(&p).ok();
            }
            removed.push(p);
        }
    }

    // Generated slash-command dir (loadout-owned entirely) → remove it whole.
    // The agent's own `<commands_dir>` (e.g. `.claude/commands/`) is left alone.
    if let Some(commands_dir) = &d.commands_dir {
        let ns = command_namespace_dir(app.repo_base(), commands_dir);
        if ns.exists() {
            if !dry {
                std::fs::remove_dir_all(&ns).ok();
            }
            removed.push(ns);
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
    // The user-level hook is global — other repos rely on it, so a repo-local
    // clean never deregisters it.
    if let Some(hr) = &d.hook_registry {
        let registered = config::home_dir()
            .and_then(|h| std::fs::read_to_string(h.join(&hr.hooks_file)).ok())
            .map(|c| c.contains(&format!(" {}", hr.subcommand)))
            .unwrap_or(false);
        if registered {
            notes.push(format!(
                "left the user-level {} hook registered (other repos may rely on it); \
                 `load {} --remove` deregisters it everywhere",
                hr.hooks_file, hr.subcommand
            ));
        }
    }

    Ok(CleanResult {
        removed,
        modified,
        notes,
    })
}

// --- shared mechanics --------------------------------------------------------

fn render_overlay(
    d: &AgentDescriptor,
    app: &AppContext,
    workflow: Option<&Workflow>,
) -> crate::Result<render::RenderOutput> {
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
        workflow,
        config: app.config,
        generated_at: app.generated_at.clone(),
        dynamic,
        learn_pending: app.learn_pending,
    })
}

/// Belt-and-braces: final redaction over loadout-generated artifact bytes just
/// before they are written. Fragment-sourced secrets were already caught (and
/// warned about, naming the fragment) at render time; anything caught here
/// came from a non-fragment channel (header, workflow text), so the warning
/// names the artifact instead. Repo-authored base content merged around a
/// marker block is deliberately NOT covered — the repo's own text is the
/// repo's business.
fn redact_artifact(content: String, what: &str) -> String {
    let (clean, n) = crate::redact::redact_secrets_report(&content);
    if n > 0 {
        crate::warn_user!("redacted a token-like string in generated {what} — check its sources");
    }
    clean
}

/// The directory loadout owns under an agent's command dir, for `wf`'s stages.
fn command_namespace_dir(repo_base: &Path, commands_dir: &str) -> PathBuf {
    repo_base
        .join(commands_dir)
        .join(commands::COMMAND_NAMESPACE)
}

/// Whether a configured `commands_dir` is safely inside the repo (relative, no
/// `..`). A guard against a hand-rolled global `[[agents]]` override escaping the
/// project tree; built-ins are already safe.
fn is_repo_relative(dir: &str) -> bool {
    let p = Path::new(dir);
    !p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Write one slash-command file per workflow stage under the owned namespace
/// dir, pruning any stale files left by removed/renamed stages first (we own the
/// whole dir, so anything not in the current set is ours to clean up).
fn write_stage_commands(
    app: &AppContext,
    d: &AgentDescriptor,
    commands_dir: &str,
    wf: &Workflow,
    files: &mut Vec<WrittenFile>,
) -> crate::Result<()> {
    let format = d
        .command_format
        .unwrap_or(commands::CommandFormat::Markdown);
    let ns_dir = command_namespace_dir(app.repo_base(), commands_dir);
    let generated = commands::stage_commands(wf, format, &d.review_commands);
    // A command's top-level entry in the namespace dir: the file itself, or —
    // for folder-shaped formats (Cursor skills' `loadout-plan/SKILL.md`) — the
    // folder. Pruning compares at that level.
    let keep: std::collections::HashSet<&str> = generated
        .iter()
        .filter_map(|c| c.filename.split('/').next())
        .collect();

    // Prune stale command entries (a removed/renamed stage's leftover).
    if let Ok(entries) = std::fs::read_dir(&ns_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !keep.contains(name.to_string_lossy().as_ref()) && !app.writer.is_dry_run() {
                let p = entry.path();
                if p.is_dir() {
                    std::fs::remove_dir_all(&p).ok();
                } else {
                    std::fs::remove_file(&p).ok();
                }
            }
        }
    }

    for cmd in &generated {
        let content = redact_artifact(cmd.content.clone(), &cmd.filename);
        files.push(app.writer.write(&ns_dir.join(&cmd.filename), &content)?);
    }
    Ok(())
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

/// Passive hook bootstrap, called from the commands every user runs anyway
/// (`refresh`, `run`, `studio`, `sync`): register each configured agent's
/// freshness hook — but only when the host agent shows evidence of being
/// installed (the hooks file's parent dir exists under `$HOME`, e.g.
/// `~/.cursor/`), so loadout never writes config for a product the user
/// doesn't have. Explicitly rendering the agent (`refresh --agent cursor`)
/// still registers the freshness hook unconditionally via [`apply`].
///
/// Learn hooks (ambient harvesting) register here ONLY when `learn_active` — the
/// caller passes `learn::state::learn_active(cfg)` (`[learn] enabled` in config
/// AND a per-machine activation ack). Gating the learn hooks in bootstrap rather
/// than in [`apply`] is the safety property: a routine refresh can never re-add
/// them after `load learn off` cleared the ack, and [`apply`] never touches
/// them. Idempotent and cheap after the first time; returns human notes for
/// anything actually written.
pub fn bootstrap_hook_registrations(
    config: &Config,
    learn_active: bool,
    dry_run: bool,
) -> Vec<String> {
    let Some(home) = config::home_dir() else {
        return Vec::new();
    };
    bootstrap_hook_registrations_at(config, learn_active, &home, dry_run)
}

/// Home-explicit core of [`bootstrap_hook_registrations`] (the `_at` test seam,
/// following the rest of the learn module). Unit tests point `home` at a
/// tempdir so they never touch the real `$HOME`.
fn bootstrap_hook_registrations_at(
    config: &Config,
    learn_active: bool,
    home: &Path,
    dry_run: bool,
) -> Vec<String> {
    let writer = crate::writer::AtomicWriter::new(dry_run);
    let mut notes = Vec::new();
    let mut warnings = Vec::new(); // silent path: bootstrap never nags
    for d in &config.agents {
        // Freshness hook: registered on every bootstrap, as before.
        if let Some(hr) = &d.hook_registry {
            register_if_installed(&writer, home, hr, &mut notes, &mut warnings);
        }
        // Learn hooks: registered ONLY while learning is active on this machine.
        if learn_active {
            for hr in &d.learn_hooks {
                register_if_installed(&writer, home, hr, &mut notes, &mut warnings);
            }
        }
    }
    notes
}

/// Register `hr` when the host agent shows evidence of being installed (its
/// hooks file's parent dir exists under `home`), so loadout never writes config
/// for a product the user doesn't have. Shared by the freshness and learn
/// bootstrap loops.
fn register_if_installed(
    writer: &dyn Writer,
    home: &Path,
    hr: &HookRegistry,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let installed = Path::new(&hr.hooks_file)
        .parent()
        .map(|p| home.join(p).is_dir())
        .unwrap_or(false);
    if !installed {
        return;
    }
    let mut files = Vec::new();
    let _ = apply_hook_registry_at(writer, home, hr, &mut files, notes, warnings);
}

/// Deregister every agent's ambient-learning hooks — called by `load learn off`.
/// Strips only entries whose command carries a learn subcommand's ` <subcommand>`
/// suffix, so Cursor's freshness hook (`… hook cursor`, a *different* suffix) and
/// every other tool's entries survive. Routes on [`HookRegistry::format`]: `Flat`
/// entries via [`remove_hook_command`], `ClaudeNested` via
/// [`hooks_claude::remove_claude_hook`] (which also drops the loadout-created
/// containers it emptied). A one-time `.loadout-bak` backup precedes the first
/// edit of a pre-existing file. Returns human notes for each file actually cleaned.
pub fn remove_learn_hooks(config: &Config, dry_run: bool) -> Vec<String> {
    let Some(home) = config::home_dir() else {
        return Vec::new();
    };
    remove_learn_hooks_at(config, &home, dry_run)
}

/// Home-explicit core of [`remove_learn_hooks`] (the `_at` test seam).
fn remove_learn_hooks_at(config: &Config, home: &Path, dry_run: bool) -> Vec<String> {
    let mut notes = Vec::new();
    for d in &config.agents {
        for hr in &d.learn_hooks {
            let path = home.join(&hr.hooks_file);
            let Ok(existing) = std::fs::read_to_string(&path) else {
                continue; // absent/unreadable → nothing to remove (never clobber)
            };
            // Route on dialect: Cursor's flat array vs Claude Code's nested schema.
            let removed = match hr.format {
                HookFormat::Flat => remove_hook_command(&existing, &hr.subcommand),
                HookFormat::ClaudeNested => {
                    hooks_claude::remove_claude_hook(&existing, &hr.subcommand)
                }
            };
            match removed {
                Ok(Some(updated)) => {
                    if dry_run {
                        notes.push(format!(
                            "would deregister the {} learning hook from {}",
                            hr.event,
                            path.display()
                        ));
                    } else {
                        // One-time backup before we first edit a pre-existing file.
                        let bak = path.with_extension("json.loadout-bak");
                        if !bak.exists() {
                            let _ = std::fs::copy(&path, &bak);
                        }
                        if crate::writer::atomic_write(&path, &updated).is_ok() {
                            notes.push(format!(
                                "deregistered the {} learning hook from {}",
                                hr.event,
                                path.display()
                            ));
                        }
                    }
                }
                Ok(None) => {} // no learn entry present — nothing to do
                Err(_) => {}   // corrupt JSON → leave it untouched (never clobber)
            }
        }
    }
    notes
}

/// Ensure loadout's hook is registered in the agent's user-level hooks file.
/// Resolves `$HOME`, then delegates to [`apply_hook_registry_at`]. A pre-existing
/// file gets a one-time `.loadout-bak` backup before its first modification.
/// Degrades to a warning (never corrupts) on any read/parse failure, exactly
/// like the importer registries.
fn apply_hook_registry(
    writer: &dyn Writer,
    hr: &HookRegistry,
    files: &mut Vec<WrittenFile>,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> crate::Result<()> {
    let Some(home) = config::home_dir() else {
        warnings.push(format!(
            "$HOME unset — can't register the {} hook in {}; add it by hand",
            hr.event, hr.hooks_file
        ));
        return Ok(());
    };
    apply_hook_registry_at(writer, &home, hr, files, notes, warnings)
}

/// Home-explicit core of [`apply_hook_registry`] (the `_at` test seam). Routes on
/// [`HookRegistry::format`]: `Flat` writes Cursor's single-array dialect;
/// `ClaudeNested` writes Claude Code's nested matcher schema in
/// `.claude/settings.json` via [`hooks_claude`]. Both share the read → backup →
/// atomic-write → note path; only the JSON transform differs. Claude Code's
/// top-level `disableAllHooks: true` short-circuits (register nothing) and
/// surfaces [`hooks_claude::DISABLE_ALL_HOOKS_NOTE`].
fn apply_hook_registry_at(
    writer: &dyn Writer,
    home: &Path,
    hr: &HookRegistry,
    files: &mut Vec<WrittenFile>,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> crate::Result<()> {
    let path = home.join(&hr.hooks_file);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warnings.push(format!(
                "could not read {} ({e}); register the `load {}` hook by hand",
                path.display(),
                hr.subcommand
            ));
            return Ok(());
        }
    };

    // Claude Code disables every hook when this top-level flag is set — our entry
    // would be inert, so we register nothing and tell the user learning falls back
    // to entry-point triggers.
    if hr.format == HookFormat::ClaudeNested
        && hooks_claude::hooks_disabled(existing.as_deref().unwrap_or(""))
    {
        notes.push(hooks_claude::DISABLE_ALL_HOOKS_NOTE.to_string());
        return Ok(());
    }

    let Ok(exe) = std::env::current_exe() else {
        warnings.push("could not resolve the load binary path for hook registration".into());
        return Ok(());
    };
    let command = format!("\"{}\" {}", exe.display(), hr.subcommand);

    let updated = match hr.format {
        HookFormat::Flat => {
            upsert_hook_command(existing.as_deref(), &hr.event, &hr.subcommand, &command)
        }
        HookFormat::ClaudeNested => hooks_claude::upsert_claude_hook(
            existing.as_deref().unwrap_or(""),
            &hr.event,
            &hr.subcommand,
            &command,
        ),
    };
    match updated {
        Ok(Some(updated)) => {
            // One-time backup of a pre-existing file we're about to modify.
            if existing.is_some() && !writer.is_dry_run() {
                let bak = path.with_extension("json.loadout-bak");
                if !bak.exists() {
                    let _ = std::fs::copy(&path, &bak);
                }
            }
            files.push(writer.write(&path, &updated)?);
            let note = match hr.purpose {
                HookPurpose::Freshness => format!(
                    "registered the {} hook in {} (keeps the overlay fresh in the IDE)",
                    hr.event,
                    path.display()
                ),
                HookPurpose::Learn => format!(
                    "registered the {} learning hook in {}",
                    hr.event,
                    path.display()
                ),
            };
            notes.push(note);
        }
        Ok(None) => {} // already registered with the current binary — no churn
        Err(e) => warnings.push(format!(
            "could not update {} ({e:#}); register the `load {}` hook by hand",
            path.display(),
            hr.subcommand
        )),
    }
    Ok(())
}

/// Ensure an entry running `command` exists under `hooks.<event>` in the hooks
/// file JSON, preserving every other field and entry value-identically (the file
/// is shared with other tools). Preservation is semantic, not byte-exact: the
/// JSON is parsed and re-emitted, so a foreign object's keys may be reordered
/// (serde_json sorts them) — every field and value survives, the byte layout
/// need not. An existing loadout entry — identified by the
/// ` <subcommand>` suffix — is updated in place when the binary moved. Returns
/// the new pretty-printed JSON, or `None` when already current (no churn).
fn upsert_hook_command(
    existing: Option<&str>,
    event: &str,
    subcommand: &str,
    command: &str,
) -> crate::Result<Option<String>> {
    use anyhow::{anyhow, bail, Context as _};
    use serde_json::{json, Map, Value};

    let mut root: Value = match existing {
        Some(s) if !s.trim().is_empty() => {
            serde_json::from_str(s).context("parsing existing hooks JSON")?
        }
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks file root is not a JSON object"))?;
    // Seed the version only when absent — never rewrite one the agent set.
    obj.entry("version").or_insert(json!(1));
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` is not a JSON object"))?;
    let arr = hooks
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(entries) = arr else {
        bail!("`hooks.{event}` is not an array");
    };

    let suffix = format!(" {subcommand}");
    let is_ours = |v: &Value| {
        v.get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.ends_with(&suffix))
            .unwrap_or(false)
    };
    if let Some(entry) = entries.iter_mut().find(|v| is_ours(v)) {
        if entry.get("command").and_then(|c| c.as_str()) == Some(command) {
            return Ok(None); // already registered with this binary
        }
        entry
            .as_object_mut()
            .ok_or_else(|| anyhow!("`hooks.{event}` entry is not an object"))?
            .insert("command".into(), json!(command));
    } else {
        entries.push(json!({ "command": command }));
    }
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

/// Strip loadout's entries (matched by the ` <subcommand>` suffix) from every
/// event array in the hooks file, leaving everything else untouched. Returns
/// the new JSON, or `None` when no loadout entry was present.
pub fn remove_hook_command(existing: &str, subcommand: &str) -> crate::Result<Option<String>> {
    use anyhow::Context as _;
    use serde_json::Value;

    let mut root: Value = serde_json::from_str(existing).context("parsing existing hooks JSON")?;
    let suffix = format!(" {subcommand}");
    let mut removed = false;
    if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_event, arr) in hooks.iter_mut() {
            if let Value::Array(entries) = arr {
                let before = entries.len();
                entries.retain(|v| {
                    !v.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.ends_with(&suffix))
                        .unwrap_or(false)
                });
                removed |= entries.len() != before;
            }
        }
    }
    if !removed {
        return Ok(None);
    }
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
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
mod hook_registry_tests {
    use super::{
        bootstrap_hook_registrations_at, remove_hook_command, remove_learn_hooks_at,
        upsert_hook_command,
    };
    use crate::config::Config;

    const CMD: &str = "\"/usr/local/bin/load\" hook cursor";

    /// Modeled on a real-world ~/.cursor/hooks.json shared with another tool.
    fn third_party() -> String {
        serde_json::json!({
            "version": 1,
            "hooks": {
                "beforeSubmitPrompt": [
                    { "command": "\"/opt/bun\" worker.cjs hook cursor session-init" }
                ],
                "stop": [ { "command": "\"/opt/bun\" worker.cjs hook cursor summarize" } ]
            }
        })
        .to_string()
    }

    #[test]
    fn creates_fresh_file_with_version_and_entry() {
        let out = upsert_hook_command(None, "sessionStart", "hook cursor", CMD)
            .unwrap()
            .expect("should write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["sessionStart"][0]["command"], CMD);
    }

    #[test]
    fn preserves_third_party_entries_and_is_idempotent() {
        let out = upsert_hook_command(Some(&third_party()), "sessionStart", "hook cursor", CMD)
            .unwrap()
            .expect("should write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // Other tools' entries survive value-identically…
        assert_eq!(
            v["hooks"]["beforeSubmitPrompt"].as_array().unwrap().len(),
            1
        );
        assert_eq!(v["hooks"]["stop"].as_array().unwrap().len(), 1);
        // …their `… hook cursor <extra>` commands are NOT mistaken for ours
        // (suffix match is exact), so ours is appended fresh.
        assert_eq!(v["hooks"]["sessionStart"][0]["command"], CMD);
        // Second apply with the same binary: no churn.
        assert!(
            upsert_hook_command(Some(&out), "sessionStart", "hook cursor", CMD)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn repoints_a_moved_binary_in_place() {
        let first = upsert_hook_command(None, "sessionStart", "hook cursor", CMD)
            .unwrap()
            .unwrap();
        let moved = "\"/new/home/load\" hook cursor";
        let out = upsert_hook_command(Some(&first), "sessionStart", "hook cursor", moved)
            .unwrap()
            .expect("should update");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "updated in place, not duplicated");
        assert_eq!(arr[0]["command"], moved);
    }

    #[test]
    fn preserves_an_existing_version_field() {
        let existing = r#"{ "version": 3, "hooks": {} }"#;
        let out = upsert_hook_command(Some(existing), "sessionStart", "hook cursor", CMD)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["version"], 3);
    }

    #[test]
    fn remove_strips_only_ours_across_all_events() {
        let mut with_ours: serde_json::Value = serde_json::from_str(&third_party()).unwrap();
        with_ours["hooks"]["sessionStart"] =
            serde_json::json!([{ "command": CMD }, { "command": "\"/opt/other\" thing" }]);
        let out = remove_hook_command(&with_ours.to_string(), "hook cursor")
            .unwrap()
            .expect("should change");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hooks"]["sessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hooks"]["sessionStart"][0]["command"],
            "\"/opt/other\" thing"
        );
        assert_eq!(
            v["hooks"]["beforeSubmitPrompt"].as_array().unwrap().len(),
            1
        );
        // Removing again: nothing left to do.
        assert!(remove_hook_command(&out, "hook cursor").unwrap().is_none());
    }

    #[test]
    fn garbage_json_errors_rather_than_clobbering() {
        assert!(upsert_hook_command(Some("not json"), "sessionStart", "hook cursor", CMD).is_err());
        assert!(remove_hook_command("not json", "hook cursor").is_err());
    }

    // --- purpose ids: freshness vs learn suffixes never cross-match ---------

    /// The suffix distinction, direction 1: registering the FRESHNESS hook
    /// (`hook cursor`) must never repoint loadout's LEARN entry
    /// (`hook cursor --event session-end`). Both entries share one event array so
    /// the freshness upsert actually scans the learn entry while hunting for
    /// "its own" (the ` hook cursor` suffix) — and, because the learn command
    /// ends in `session-end`, it is left byte-identical and the freshness entry
    /// is appended fresh.
    #[test]
    fn freshness_registration_never_repoints_a_learn_entry() {
        let learn_cmd = "\"/usr/local/bin/load\" hook cursor --event session-end";
        let existing = serde_json::json!({
            "version": 1,
            "hooks": { "sessionStart": [ { "command": learn_cmd } ] }
        })
        .to_string();
        let out = upsert_hook_command(Some(&existing), "sessionStart", "hook cursor", CMD)
            .unwrap()
            .expect("freshness suffix ≠ learn suffix → a fresh append, not a repoint");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            2,
            "learn entry kept, freshness appended (not repointed)"
        );
        assert_eq!(arr[0]["command"], learn_cmd, "learn entry byte-identical");
        assert_eq!(arr[1]["command"], CMD);
    }

    /// The suffix distinction, direction 2: removing loadout's LEARN entry
    /// (`hook cursor --event session-end`) must never touch the freshness hook
    /// (`hook cursor`) nor any foreign entry — all in the same event array.
    #[test]
    fn learn_removal_never_touches_freshness_or_foreign() {
        let freshness = "\"/usr/local/bin/load\" hook cursor";
        let learn_cmd = "\"/usr/local/bin/load\" hook cursor --event session-end";
        let foreign = "\"/opt/bun\" worker.cjs hook cursor summarize";
        let existing = serde_json::json!({
            "version": 1,
            "hooks": { "stop": [
                { "command": freshness },
                { "command": learn_cmd },
                { "command": foreign },
            ] }
        })
        .to_string();
        let out = remove_hook_command(&existing, "hook cursor --event session-end")
            .unwrap()
            .expect("the learn entry is present → a change");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["stop"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "only the learn entry removed");
        assert_eq!(
            arr[0]["command"], freshness,
            "freshness survives value-identical"
        );
        assert_eq!(
            arr[1]["command"], foreign,
            "foreign survives value-identical"
        );
        // Reverse: with the learn entry gone, removing it again is a no-op — the
        // freshness `hook cursor` suffix is NOT mistaken for the learn suffix.
        assert!(remove_hook_command(&out, "hook cursor --event session-end")
            .unwrap()
            .is_none());
    }

    // --- bootstrap gating: learn hooks register only while active -----------

    /// A tempdir `$HOME` where both host agents look installed (their hook-file
    /// parent dirs exist). `Config::defaults()` carries the built-in cursor +
    /// claude learn hooks.
    fn installed_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        home
    }

    #[test]
    fn bootstrap_registers_no_learn_hooks_when_inactive() {
        let home = installed_home();
        bootstrap_hook_registrations_at(&Config::defaults(), false, home.path(), false);

        let cursor: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".cursor/hooks.json")).unwrap(),
        )
        .unwrap();
        // Cursor's FRESHNESS hook still registers (bootstrap always does that)…
        let ss = cursor["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1);
        assert!(ss[0]["command"].as_str().unwrap().ends_with(" hook cursor"));
        // …but the learn event ("stop") was never created while inactive.
        assert!(
            cursor["hooks"].get("stop").is_none(),
            "no learn hook while learning is inactive: {cursor}"
        );
        // Claude has only a learn hook → its settings file is never written.
        assert!(
            !home.path().join(".claude/settings.json").exists(),
            "no claude write while inactive"
        );
    }

    #[test]
    fn bootstrap_registers_both_learn_dialects_when_active() {
        let home = installed_home();
        bootstrap_hook_registrations_at(&Config::defaults(), true, home.path(), false);

        let cursor: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".cursor/hooks.json")).unwrap(),
        )
        .unwrap();
        // Freshness (sessionStart) AND the Flat learn hook (stop) are both present.
        assert!(cursor["hooks"]["sessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["command"].as_str().unwrap().ends_with(" hook cursor")));
        let stop = cursor["hooks"]["stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(
            stop[0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" hook cursor --event session-end"),
            "cursor's Flat learn hook is registered: {stop:?}"
        );
        // Claude's learn hook is ClaudeNested → written in the nested matcher schema
        // of .claude/settings.json by the dedicated writer (Task 17).
        let claude: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let groups = claude["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let inner = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(inner[0]["type"], "command");
        assert!(inner[0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" hook claude --event session-end"));
        assert_eq!(inner[0]["timeout"], 10);
    }

    // --- remove_learn_hooks at the descriptor level -------------------------

    #[test]
    fn remove_learn_hooks_leaves_freshness_and_foreign_value_identical() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
        let freshness = "\"/usr/local/bin/load\" hook cursor";
        let learn_cmd = "\"/usr/local/bin/load\" hook cursor --event session-end";
        let foreign = "\"/opt/bun\" worker.cjs hook cursor summarize";
        // A cursor hooks file with the freshness hook (sessionStart), loadout's
        // learn hook (stop), and a foreign tool's entry (stop).
        let contents = serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "hooks": {
                "sessionStart": [ { "command": freshness } ],
                "stop": [ { "command": learn_cmd }, { "command": foreign } ]
            }
        }))
        .unwrap();
        let path = home.path().join(".cursor/hooks.json");
        std::fs::write(&path, &contents).unwrap();

        let notes = remove_learn_hooks_at(&Config::defaults(), home.path(), false);
        assert!(!notes.is_empty(), "removal reported an action");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Freshness hook untouched.
        assert_eq!(v["hooks"]["sessionStart"][0]["command"], freshness);
        // Learn hook gone; foreign hook survives value-identical.
        let stop = v["hooks"]["stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["command"], foreign);
        // A one-time backup of the pre-existing file was written before the edit.
        assert!(
            path.with_extension("json.loadout-bak").exists(),
            "backup written before edit"
        );
        // Idempotent: a second removal finds nothing left and writes nothing new.
        let again = remove_learn_hooks_at(&Config::defaults(), home.path(), false);
        assert!(again.is_empty(), "second removal is a no-op");
    }

    /// `remove_learn_hooks_at` dispatches the ClaudeNested dialect to the nested
    /// writer: our SessionEnd group is stripped, a foreign sibling group and every
    /// foreign key survive, a `.loadout-bak` backup is written, and it is idempotent.
    #[test]
    fn remove_learn_hooks_strips_claude_nested_keeps_foreign() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let ours = "\"/usr/local/bin/load\" hook claude --event session-end";
        let foreign = "\"/opt/other\" wrapup";
        let contents = serde_json::to_string_pretty(&serde_json::json!({
            "model": "keep-me",
            "hooks": {
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": foreign } ] },
                    { "hooks": [ { "type": "command", "command": ours, "timeout": 10 } ] }
                ]
            }
        }))
        .unwrap();
        let path = home.path().join(".claude/settings.json");
        std::fs::write(&path, &contents).unwrap();

        let notes = remove_learn_hooks_at(&Config::defaults(), home.path(), false);
        assert!(!notes.is_empty(), "removal reported an action");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let se = v["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(
            se.len(),
            1,
            "our emptied group dropped, foreign sibling kept"
        );
        assert_eq!(se[0]["hooks"][0]["command"], foreign);
        assert_eq!(v["model"], "keep-me", "foreign key preserved");
        assert!(
            path.with_extension("json.loadout-bak").exists(),
            "backup written before edit"
        );
        let again = remove_learn_hooks_at(&Config::defaults(), home.path(), false);
        assert!(again.is_empty(), "second removal is a no-op");
    }
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
