//! `load` — the CLI binary. Thin shell over the `loadout` library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use loadout::cli::{Cli, Command, RunArgs};
use loadout::commands::{self, Runtime};
use loadout::report;

fn main() -> ExitCode {
    let cli = Cli::parse();
    report::set_verbose(cli.global.verbose);

    let cwd = match resolve_cwd(cli.global.cwd.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let rt = Runtime::new(cwd, cli.global.dry_run);

    // One-time cleanup after ambient learning was removed. Sits here rather
    // than in `run`/`refresh` so it reaches people who mostly use `studio` or
    // `sync` too — it is a cheap `stat` of a marker file on every later
    // invocation. Never for `hook`: that is machine-invoked, and the agent
    // parses its stdout as the hook response.
    if !matches!(cli.command, Command::Hook(_)) {
        for line in loadout::legacy::retire_learning(rt.dry_run) {
            println!("{line}");
        }
    }

    // The ambient update nudge runs after every interactive command, except:
    // `run`/`launch` print their own before the launch `exec()`s away, `update`
    // would be telling you what you just did, and `hook` is a machine caller
    // whose stdout must stay clean. `nudge_detail`'s own gates (config mode,
    // TTY, opt-out env, cap window) keep everything else quiet and cheap.
    let ambient_nudge = !matches!(
        &cli.command,
        Command::Run(_) | Command::Launch(_) | Command::Update(_) | Command::Hook(_)
    );

    let result = match &cli.command {
        Command::Detect(args) => commands::detect::run(&rt, args),
        Command::Run(args) => commands::run::run(&rt, args),
        Command::Explain(args) => commands::explain::run(&rt, args),
        Command::Refresh(args) => commands::refresh::run(&rt, args),
        Command::Clean(args) => commands::clean::run(&rt, args),
        Command::Doctor => commands::doctor::run(&rt),
        Command::Fragments(args) => commands::introspect::fragments(&rt, args),
        Command::Profiles(args) => commands::introspect::profiles(&rt, args),
        Command::Agents(args) => commands::introspect::agents(&rt, args),
        Command::Studio(args) => loadout::studio::serve(&rt, args),
        Command::Sync(args) => commands::sync::run(&rt, args),
        Command::Skill(args) => commands::skill::run(&rt, args),
        Command::Plan(args) => commands::plan::run(&rt, args),
        Command::Update(args) => commands::update::run(&rt, args),
        Command::Hook(args) => commands::hook::run(&rt, args),
        Command::Use(args) => commands::bind::run(&rt, args),
        Command::List(args) => commands::introspect::list(&rt, args),
        Command::Edit(args) => commands::edit::run(&rt, args),
        Command::Targets(args) => commands::trust::targets(&rt, args),
        Command::Trust(args) => commands::trust::run(&rt, args),
        // Bare `load <agent> [args…]` — the implicit form of `run`.
        Command::Launch(argv) => commands::run::run(&rt, &RunArgs::from_launch(argv.clone())),
    };

    match result {
        Ok(()) => {
            if ambient_nudge {
                loadout::update::ambient_nudge(&rt.cwd);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the working directory: explicit `--cwd`, else the process cwd.
/// Canonicalizes so git/path logic sees a stable absolute path.
fn resolve_cwd(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = match explicit {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    Ok(raw.canonicalize().unwrap_or(raw))
}
