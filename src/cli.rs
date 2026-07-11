//! Command-line interface (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// loadout — inject global context into your AI coding agents.
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
    /// Remove loadout-generated overlays and managed blocks for an agent.
    Clean(CleanArgs),
    /// Diagnose the environment and configuration.
    Doctor,
    /// List fragments, or show one (`load fragments [list|show <id>]`).
    Fragments(FragmentsArgs),
    /// List configured loadouts and which match the current context.
    Profiles(ProfilesArgs),
    /// List configured agents and how each delivers the overlay.
    Agents(AgentsArgs),
    /// Launch the local studio web UI (ephemeral; serves until Ctrl-C).
    Studio(StudioArgs),
    /// Sync your global config (fragments & loadouts) across machines via git.
    Sync(SyncArgs),
    /// Manage the agent skills loadout ships (installed under `~/.agents/skills`).
    Skill(SkillArgs),
    /// Preview a development plan: validate plan.json, render plan.html, review.
    Plan(PlanArgs),
    /// Update loadout to the latest release (installer-based installs only).
    Update(UpdateArgs),
    /// Mine your recent agent sessions for durable preferences and stage them
    /// as candidates in the review inbox (one bounded, cheap-model extraction
    /// call). A bare `load harvest` is a manual run; ambient triggers pass a
    /// hidden `--ambient`.
    Harvest(HarvestArgs),
    /// (machine-invoked) Agent lifecycle-hook endpoint: reads the hook payload
    /// on stdin and quietly refreshes the adopted repos among its workspace
    /// roots. Registered automatically (e.g. Cursor's ~/.cursor/hooks.json);
    /// `--remove` deregisters. Always exits 0 in serve mode.
    #[command(hide = true)]
    Hook(HookArgs),
    /// Pin this project to a loadout (remembers the choice; `load use <name>`).
    Use(UseArgs),
    /// List your loadouts (default), fragments, agents, or targets.
    List(ListArgs),
    /// Open your global config to edit a loadout or fragment in `$EDITOR`.
    Edit(EditArgs),
    /// Manage custom targets (`load targets trust <id>`).
    Targets(TargetsArgs),
    /// Show or rebuild the per-machine script trust store.
    Trust(TrustArgs),
    /// Launch an agent by id — the implicit form of `run` (e.g. `load claude`).
    /// Any first token that isn't a known command is treated as an agent id;
    /// the rest pass through to the agent.
    #[command(external_subcommand)]
    Launch(Vec<String>),
}

/// `harvest` options.
#[derive(Debug, Args)]
pub struct HarvestArgs {
    /// Marks a throttled, trigger-driven ambient run (the triggers pass this;
    /// it only changes how the run is labelled in the log). Hidden: a user
    /// types a bare `load harvest`, which is a manual run.
    #[arg(long, hide = true)]
    pub ambient: bool,
}

/// `hook` options.
#[derive(Debug, Args)]
pub struct HookArgs {
    /// Agent id whose hook integration to serve (e.g. `cursor`).
    pub agent: String,
    /// Deregister loadout's entries from the agent's user-level hooks file.
    #[arg(long)]
    pub remove: bool,
    /// Which lifecycle event fired. `session-end` takes the ambient-learning
    /// serve path (mark the just-ended session eligible + wake the harvest
    /// worker); absent is the freshness serve path. Set by the registered hook
    /// command, never typed by hand.
    #[arg(long)]
    pub event: Option<String>,
}

/// `skill` options. Bare `load skill` shows status.
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
    /// Remove loadout-installed skills (canonical files + agent symlinks).
    Remove {
        /// Skill id (defaults to every shipped skill).
        id: Option<String>,
    },
    /// Show each shipped skill's install state, links, and remembered decision.
    Status,
}

/// `plan` options. Bare `load plan` shows status.
#[derive(Debug, Args)]
pub struct PlanArgs {
    /// `check`, `render`, `schema`, `clean`, or status (the default).
    #[command(subcommand)]
    pub action: Option<PlanAction>,
}

/// `plan` subcommands.
#[derive(Debug, Subcommand)]
pub enum PlanAction {
    /// Validate plan.json; machine-readable diagnostics with --json.
    Check {
        /// Input file (default .loadout/workflow/artifacts/plan.json).
        file: Option<PathBuf>,
        /// Emit machine-readable JSON diagnostics instead of plain text.
        #[arg(long)]
        json: bool,
        /// Downgrade unknown fields to warnings (read newer plans).
        #[arg(long)]
        lenient: bool,
    },
    /// Render plan.json to a self-contained plan.html and open it.
    Render {
        /// Input file (default .loadout/workflow/artifacts/plan.json).
        file: Option<PathBuf>,
        /// Output path (default .loadout/generated/plan.html).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Don't open the browser after rendering.
        #[arg(long)]
        no_open: bool,
    },
    /// Print the plan.json schema reference.
    Schema,
    /// Remove the rendered plan.html (and plan-feedback.json if present).
    Clean,
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

/// `sync` options. Bare `load sync` pulls the latest and pushes local edits.
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
    /// Remote URL to push to (e.g. `git@github.com:you/loadout-config.git`).
    /// Omit to set up the repo locally only (add a remote later).
    pub remote: Option<String>,
}

/// `sync clone` options.
#[derive(Debug, Args)]
pub struct SyncCloneArgs {
    /// The config repo URL to clone (e.g. `https://github.com/you/loadout-config.git`).
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
    /// Re-approve a script fragment's current script after an out-of-band change.
    Trust {
        /// Fragment id.
        id: String,
    },
}

/// `targets` options.
#[derive(Debug, Args)]
pub struct TargetsArgs {
    #[command(subcommand)]
    pub action: TargetsAction,
}

/// `targets` subcommands.
#[derive(Debug, Subcommand)]
pub enum TargetsAction {
    /// Re-approve a custom target's current script(s) after an out-of-band change.
    Trust {
        /// Target id.
        id: String,
    },
}

/// `trust` options.
#[derive(Debug, Args)]
pub struct TrustArgs {
    /// Re-record every script currently in config as explicitly approved
    /// (also recovers a corrupt trust store).
    #[arg(long)]
    pub rebuild: bool,
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
    /// Override the profile's bound workflow for this run (a built-in or your own
    /// `[[workflows]]` id). An unknown id applies no workflow.
    #[arg(long)]
    pub workflow: Option<String>,
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
            workflow: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_launch_captures_everything_after_agent_verbatim() {
        // `load claude agents --resume …`: nothing after the agent token is
        // parsed by load — even flags that collide with RunArgs' own.
        let cli = Cli::try_parse_from([
            "load",
            "claude",
            "agents",
            "--resume",
            "--workflow",
            "w",
            "--skip-render",
        ])
        .unwrap();
        let Command::Launch(argv) = cli.command else {
            panic!("expected Command::Launch, got {:?}", cli.command);
        };
        assert_eq!(
            argv,
            [
                "claude",
                "agents",
                "--resume",
                "--workflow",
                "w",
                "--skip-render"
            ]
        );
    }

    #[test]
    fn from_launch_splits_agent_from_passthrough_args() {
        let args = RunArgs::from_launch(vec![
            "claude".to_string(),
            "agents".to_string(),
            "--resume".to_string(),
        ]);
        assert_eq!(args.agent, "claude");
        assert_eq!(args.args, ["agents", "--resume"]);
        assert!(!args.skip_render);
        assert!(args.workflow.is_none());
    }

    #[test]
    fn from_launch_tolerates_empty_argv() {
        let args = RunArgs::from_launch(Vec::new());
        assert_eq!(args.agent, "");
        assert!(args.args.is_empty());
    }

    #[test]
    fn run_form_parses_own_flags_after_agent() {
        // Unlike the bare form, `load run <agent>` still parses RunArgs flags
        // placed after the agent id — they do NOT pass through …
        let cli = Cli::try_parse_from(["load", "run", "claude", "--workflow", "w"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected Command::Run, got {:?}", cli.command);
        };
        assert_eq!(args.agent, "claude");
        assert_eq!(args.workflow.as_deref(), Some("w"));
        assert!(args.args.is_empty());
    }

    #[test]
    fn run_form_double_dash_forwards_colliding_flags() {
        // … and `--` is the escape hatch that forwards them to the agent.
        let cli = Cli::try_parse_from(["load", "run", "claude", "--", "--workflow", "w"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected Command::Run, got {:?}", cli.command);
        };
        assert_eq!(args.agent, "claude");
        assert!(args.workflow.is_none());
        assert_eq!(args.args, ["--workflow", "w"]);
    }

    #[test]
    fn run_form_passes_unknown_flags_through() {
        // Flags load doesn't define reach the agent even without `--`.
        let cli = Cli::try_parse_from(["load", "run", "claude", "--resume"]).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected Command::Run, got {:?}", cli.command);
        };
        assert_eq!(args.args, ["--resume"]);
    }
}
