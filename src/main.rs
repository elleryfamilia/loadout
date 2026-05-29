//! `rosita` — the CLI binary. Thin shell over the `rosita` library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use rosita::cli::{Cli, Command};
use rosita::commands::{self, Runtime};
use rosita::report;

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

    let result = match &cli.command {
        Command::Init(args) => commands::init::run(&rt, args),
        Command::Detect(args) => commands::detect::run(&rt, args),
        Command::Render(args) => commands::render::run(&rt, args),
        Command::Run(args) => commands::run::run(&rt, args),
        Command::Explain(args) => commands::explain::run(&rt, args),
        Command::Refresh(args) => commands::refresh::run(&rt, args),
        Command::Clean(args) => commands::clean::run(&rt, args),
        Command::Doctor => commands::doctor::run(&rt),
        Command::Allow => commands::trust::allow(&rt),
        Command::Deny => commands::trust::deny(&rt),
        Command::Trust(_) => commands::trust::status(&rt),
        Command::Capabilities(args) => commands::introspect::capabilities(&rt, args),
        Command::Profiles(args) => commands::introspect::profiles(&rt, args),
        Command::Agents(args) => commands::introspect::agents(&rt, args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
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
