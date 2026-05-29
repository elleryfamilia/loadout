//! `rosita run <agent> [args...]` — render the overlay, then launch the agent.
//!
//! This is the simple "preflight/wrapper" approach: refresh the generated files
//! for the chosen agent, then hand control to the real agent CLI (replacing this
//! process on Unix so signals and exit codes pass through cleanly). FUSE-style
//! live virtual files are explicitly out of scope for the MVP.
//!
//! Because rosita is the launching parent, it passes a freshness signal to the
//! agent: `ROSITA_RUN=1` + `ROSITA_RENDERED_AT` in the environment (so an agent
//! that can read env — or its hook — knows the context is current), and, for
//! agents with an `append_prompt_flag` (e.g. Claude's `--append-system-prompt`),
//! a short "context is fresh" note injected directly into the launch.

use std::process::Command;

use anyhow::anyhow;

use super::{now_rfc3339, prepare, Runtime};
use crate::adapters::{self, AgentDescriptor, ApplyOptions};
use crate::cli::RunArgs;
use crate::vlog;

/// Entry point for `rosita run`.
pub fn run(rt: &Runtime, args: &RunArgs) -> crate::Result<()> {
    let agent = args.agent.as_str();
    let prep = prepare(rt)?;
    let descriptor = adapters::descriptor(&prep.config, agent)
        .ok_or_else(|| anyhow!("unknown agent '{agent}'"))?
        .clone();
    let program = descriptor
        .launch
        .clone()
        .ok_or_else(|| anyhow!("agent '{agent}' is not launchable (no `launch` program)"))?;

    // Preflight render (unless skipped).
    let rendered = !args.skip_render;
    if rendered {
        let opts = ApplyOptions {
            codex_override: args.codex_override,
            force: false,
        };
        super::render::apply_for_agents(rt, &prep, &[agent.to_string()], &opts)?;
    } else {
        vlog!("skipping pre-launch render (--skip-render)");
    }

    let rendered_at = now_rfc3339();
    let launch_args = build_launch_args(&descriptor, &prep, rendered, &rendered_at, &args.args);

    if rt.dry_run {
        println!("dry run — would exec: {program} {}", launch_args.join(" "));
        return Ok(());
    }

    launch(&program, &launch_args, &rendered_at, &rt.cwd)
}

/// Prepend a freshness note via the agent's prompt flag when we just rendered.
fn build_launch_args(
    descriptor: &AgentDescriptor,
    prep: &super::Prepared,
    rendered: bool,
    rendered_at: &str,
    user_args: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    if rendered {
        if let Some(flag) = &descriptor.append_prompt_flag {
            out.push(flag.clone());
            out.push(format!(
                "rosita: project context refreshed for profile '{}' at {rendered_at} — current. \
                 Run `rosita refresh` if the project changes mid-session.",
                prep.profile_label()
            ));
        }
    }
    out.extend(user_args.iter().cloned());
    out
}

/// Launch `program` with `args` in `cwd`, passing the rosita freshness signal in
/// the environment. On Unix this replaces the current process; elsewhere it
/// spawns, waits, and mirrors the exit code.
fn launch(
    program: &str,
    args: &[String],
    rendered_at: &str,
    cwd: &std::path::Path,
) -> crate::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("ROSITA_RUN", "1")
        .env("ROSITA_RENDERED_AT", rendered_at);
    vlog!("launching: {program} {}", args.join(" "));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // exec only returns on failure.
        let err = cmd.exec();
        Err(anyhow!("failed to exec '{program}': {err}")
            .context("is the agent CLI installed and on PATH?"))
    }

    #[cfg(not(unix))]
    {
        use anyhow::Context as _;
        let status = cmd
            .status()
            .with_context(|| format!("failed to launch '{program}'"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
