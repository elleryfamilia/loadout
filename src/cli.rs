//! Command-line interface (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// rosita — inject global context into your AI coding agents.
///
/// Detects the current project/runtime context, selects the loadout that fits,
/// renders an agent-specific instruction overlay, writes it safely, and can
/// launch the agent. Generated files are agent *guidance*, not enforced policy.
#[derive(Debug, Parser)]
#[command(
    name = "load",
    version,
    about,
    long_about = None,
    after_help = "Examples:\n  \
        load claude              Equip the matching loadout and launch Claude\n  \
        load run claude -- -p    Same, but pass `-p` through to the agent\n  \
        load use nextjs          Pin this project to the \"nextjs\" loadout\n  \
        load list                List your loadouts\n  \
        load edit nextjs         Open your config to edit a loadout or fragment\n\n\
        Tip: `load <agent>` is shorthand for `load run <agent>`."
)]
pub struct Cli {
    /// Global options shared by all subcommands.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Options available on every subcommand.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Operate as if invoked from this directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Verbose diagnostics on stderr.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Do not write any files; report what would change.
    #[arg(long, global = true)]
    pub dry_run: bool,
}

// Agents are selected by id string (claude/codex/gemini/opencode/copilot/generic,
// or "all"), validated at runtime against the loaded config so new agents added
// via `[[agents]]` work without code changes.

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Detect and print the current context.
    Detect(DetectArgs),
    /// Pull the latest config, render, then launch an agent (claude/codex),
    /// passing through args.
    Run(RunArgs),
    /// Explain what would be selected and written, and why.
    Explain(ExplainArgs),
    /// Pull the latest config, then (re-)render overlays (`--agent` to target
    /// or first-adopt one).
    Refresh(RefreshArgs),
    /// Remove rosita-generated overlays and managed blocks for an agent.
    Clean(CleanArgs),
    /// Diagnose the environment and configuration.
    Doctor,
    /// List fragments, or show one (`rosita fragments [list|show <id>]`).
    Fragments(FragmentsArgs),
    /// List configured loadouts and which match the current context.
    Profiles(ProfilesArgs),
    /// List configured agents and how each delivers the overlay.
    Agents(AgentsArgs),
    /// Launch the local studio web UI (ephemeral; serves until Ctrl-C).
    Studio(StudioArgs),
    /// Sync your global config (fragments & loadouts) across machines via git.
    Sync(SyncArgs),
    /// Manage the agent skills rosita ships (installed under `~/.agents/skills`).
    Skill(SkillArgs),
    /// Update rosita to the latest release (installer-based installs only).
    Update(UpdateArgs),
    /// Pin this project to a loadout (remembers the choice; `load use <name>`).
    Use(UseArgs),
    /// List your loadouts (default), fragments, agents, or targets.
    List(ListArgs),
    /// Open your global config to edit a loadout or fragment in `$EDITOR`.
    Edit(EditArgs),
    /// Launch an agent by id — the implicit form of `run` (e.g. `load claude`).
    /// Any first token that isn't a known command is treated as an agent id;
    /// the rest pass through to the agent.
    #[command(external_subcommand)]
    Launch(Vec<String>),
}

/// `skill` options. Bare `rosita skill` shows status.
#[derive(Debug, Args)]
pub struct SkillArgs {
    /// `install`, `remove`, or `status` (the default).
    #[command(subcommand)]
    pub action: Option<SkillAction>,
}

/// `skill` subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillAction {
    /// Install (or repair/upgrade) shipped skills into `~/.agents/skills`,
    /// with symlinks for agents that need their own skills dir.
    Install {
        /// Skill id (defaults to every shipped skill).
        id: Option<String>,
    },
    /// Remove rosita-installed skills (canonical files + agent symlinks).
    Remove {
        /// Skill id (defaults to every shipped skill).
        id: Option<String>,
    },
    /// Show each shipped skill's install state, links, and remembered decision.
    Status,
}

/// `update` options.
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only report whether a newer release exists; don't install it.
    #[arg(long)]
    pub check: bool,
}

/// `use` options — pin this project to a named loadout.
#[derive(Debug, Args)]
pub struct UseArgs {
    /// The loadout name to pin for this project.
    pub loadout: String,
}

/// `list` options.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// What to list (default: loadouts).
    #[arg(value_enum, default_value_t = ListKind::Loadouts)]
    pub kind: ListKind,
    /// Emit JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

/// What `load list` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ListKind {
    /// Your loadouts and which one matches the current project.
    Loadouts,
    /// Your fragment library.
    Fragments,
    /// Configured agents and how each receives the overlay.
    Agents,
    /// Detectable project/environment targets.
    Targets,
}

/// `edit` options.
#[derive(Debug, Args)]
pub struct EditArgs {
    /// Optional loadout or fragment name to confirm exists before opening.
    pub name: Option<String>,
}

/// `sync` options. Bare `rosita sync` pulls the latest and pushes local edits.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// `init` (set this machine up) or `clone` (pull config onto a new machine).
    #[command(subcommand)]
    pub action: Option<SyncAction>,
}

/// `sync` subcommands.
#[derive(Debug, Subcommand)]
pub enum SyncAction {
    /// Make this machine's config dir a synced git repo (and optionally wire +
    /// push a remote).
    Init(SyncInitArgs),
    /// Clone an existing config repo onto this machine (for a new headless box).
    Clone(SyncCloneArgs),
}

/// `sync init` options.
#[derive(Debug, Args)]
pub struct SyncInitArgs {
    /// Remote URL to push to (e.g. `git@github.com:you/rosita-config.git`).
    /// Omit to set up the repo locally only (add a remote later).
    pub remote: Option<String>,
}

/// `sync clone` options.
#[derive(Debug, Args)]
pub struct SyncCloneArgs {
    /// The config repo URL to clone (e.g. `https://github.com/you/rosita-config.git`).
    pub url: String,
}

/// `studio` options.
#[derive(Debug, Args)]
pub struct StudioArgs {
    /// Port to bind on 127.0.0.1 (0 = let the OS choose a free port).
    #[arg(long, default_value_t = 7777)]
    pub port: u16,
    /// Don't open a browser window automatically.
    #[arg(long)]
    pub no_open: bool,
    /// Shut down after this much inactivity (e.g. `30m`, `90s`, `2h`).
    /// `0` disables the timeout (serve until Ctrl-C).
    #[arg(long, default_value = "30m")]
    pub idle_timeout: String,
}

/// `fragments` options.
#[derive(Debug, Args)]
pub struct FragmentsArgs {
    /// `list` (default) or `show <id>`.
    #[command(subcommand)]
    pub action: Option<FragmentsAction>,
    /// Emit JSON instead of a human summary.
    #[arg(long, global = true)]
    pub json: bool,
}

/// `fragments` subcommands.
#[derive(Debug, Subcommand)]
pub enum FragmentsAction {
    /// List every fragment in the library (the default).
    List,
    /// Show one fragment's full details.
    Show {
        /// Fragment id.
        id: String,
    },
}

/// `profiles` options.
#[derive(Debug, Args)]
pub struct ProfilesArgs {
    /// Emit JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

/// `agents` options.
#[derive(Debug, Args)]
pub struct AgentsArgs {
    /// Emit JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

/// `detect` options.
#[derive(Debug, Args)]
pub struct DetectArgs {
    /// Emit JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
    /// Also run environment probes (host/tailnet/docker/toolchain/ai-tools).
    /// Opt-in because probes shell out to external tools.
    #[arg(long)]
    pub probes: bool,
}

/// `run` options.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Agent id to launch (must have a launch program).
    pub agent: String,
    /// Skip the pre-launch render.
    #[arg(long)]
    pub skip_render: bool,
    /// Force-write Codex's `AGENTS.override.md` even if disabled in config.
    #[arg(long = "override")]
    pub codex_override: bool,
    /// Skip Codex's `AGENTS.override.md` (emit-only; leaves `AGENTS.md` untouched).
    #[arg(long = "no-override", conflicts_with = "codex_override")]
    pub codex_no_override: bool,
    /// Arguments passed through to the agent.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl RunArgs {
    /// Build run-args from the bare-launch form (`load <agent> [args…]`), where
    /// the first token is the agent id and the rest pass through to the agent.
    /// The Codex override flags are not parsed in this form — use `load run` for
    /// those. `argv` always has at least one element (clap's external subcommand).
    pub fn from_launch(mut argv: Vec<String>) -> Self {
        let agent = if argv.is_empty() {
            String::new()
        } else {
            argv.remove(0)
        };
        RunArgs {
            agent,
            skip_render: false,
            codex_override: false,
            codex_no_override: false,
            args: argv,
        }
    }
}

/// `explain` options.
#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// Agent id to explain the write-plan for, or `all` (defaults to config default).
    #[arg(long)]
    pub agent: Option<String>,
    /// Emit JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

/// `refresh` options.
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Agent id to render, or `all` (defaults to already-initialized overlays;
    /// naming an agent renders it even if it was never initialized here).
    #[arg(long)]
    pub agent: Option<String>,
    /// Force-write Codex's `AGENTS.override.md` even if disabled in config.
    #[arg(long = "override")]
    pub codex_override: bool,
    /// Skip Codex's `AGENTS.override.md` (emit-only; leaves `AGENTS.md` untouched).
    #[arg(long = "no-override", conflicts_with = "codex_override")]
    pub codex_no_override: bool,
    /// Re-render even if the context hash is unchanged.
    #[arg(long)]
    pub force: bool,
}

/// `clean` options.
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Restrict to an agent id, or `all` (defaults to all agents with artifacts).
    #[arg(long)]
    pub agent: Option<String>,
}
