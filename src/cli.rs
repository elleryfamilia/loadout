//! Command-line interface (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// rosita — inject global context into your AI coding agents.
///
/// Detects the current project/runtime context, selects the profile that fits,
/// renders an agent-specific instruction overlay, writes it safely, and can
/// launch the agent. Generated files are agent *guidance*, not enforced policy.
#[derive(Debug, Parser)]
#[command(name = "rosita", version, about, long_about = None)]
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
    /// Render the overlay for an agent and wire it up.
    Render(RenderArgs),
    /// Render then launch an agent (claude/codex), passing through args.
    Run(RunArgs),
    /// Explain what would be selected and written, and why.
    Explain(ExplainArgs),
    /// Re-render overlays for already-initialized agents.
    Refresh(RefreshArgs),
    /// Remove rosita-generated overlays and managed blocks for an agent.
    Clean(CleanArgs),
    /// Diagnose the environment and configuration.
    Doctor,
    /// List fragments, or show one (`rosita fragments [list|show <id>]`).
    #[command(alias = "capabilities")]
    Fragments(FragmentsArgs),
    /// List configured profiles and which match the current context.
    Profiles(ProfilesArgs),
    /// List configured agents and how each delivers the overlay.
    Agents(AgentsArgs),
    /// Launch the local studio web UI (ephemeral; serves until Ctrl-C).
    Studio(StudioArgs),
    /// Sync your global config (fragments & profiles) across machines via git.
    Sync(SyncArgs),
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

/// `render` options.
#[derive(Debug, Args)]
pub struct RenderArgs {
    /// Agent id to render for, or `all` (defaults to the config default agent).
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
    /// Restrict to an agent id, or `all` (defaults to already-initialized overlays).
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
