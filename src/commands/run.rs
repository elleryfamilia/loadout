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
//!
//! For an agent with no persistent local hook but a `launch_context_dir_env`
//! (e.g. Copilot's `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`), rosita also sets that env
//! var to the directory holding the generated overlay, so the agent discovers it
//! at launch without any committed file being touched.

use std::io::{IsTerminal, Write as _};
use std::process::Command;

use anyhow::anyhow;

use std::time::Duration;

use super::{
    now_rfc3339, prepare_with_live, Aborted, Choice, MissingPolicy, ProfileChooser, Runtime,
};
use crate::adapters::{self, AgentDescriptor, ApplyOptions, ApplyResult};
use crate::cli::{RunArgs, StudioArgs};
use crate::context::Context;
use crate::hash;
use crate::profile::ProfileConfig;
use crate::style::Painter;
use crate::sync::{self, SyncStatus};
use crate::vlog;

/// Interactive "which profile?" prompt for `rosita run` when 2+ profiles match
/// and no choice is remembered yet. Falls back to no-profile (no prompt) when
/// stdin/stdout isn't a terminal, so CI/piped runs never block.
struct StdinChooser;

impl ProfileChooser for StdinChooser {
    fn choose(&self, ctx: &Context, candidates: &[ProfileConfig]) -> crate::Result<Choice> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            crate::warn_user!(
                "{} profiles match but this isn't an interactive terminal — applying none. \
                 Re-run `rosita run` interactively (or use `rosita studio`) to pick.",
                candidates.len()
            );
            return Ok(Choice::Skip);
        }

        let langs = if ctx.stacks.is_empty() {
            "this".to_string()
        } else {
            ctx.stacks.join("/")
        };
        println!(
            "rosita › this {langs} project matches {} profiles — pick one:",
            candidates.len()
        );
        println!("  ↑/↓ to move · Enter to select · or press a number · Esc/Ctrl-C to cancel");

        let items: Vec<String> = candidates.iter().map(|p| p.name.clone()).collect();
        match crate::tui::select(&items)? {
            Some(i) => {
                let name = candidates[i].name.clone();
                println!("rosita › bound \"{name}\" → remembered for this project; launching…");
                Ok(Choice::Profile(name))
            }
            // Cancelled (Esc / Ctrl-C / q / EOF): the user invoked rosita but
            // didn't pick — abort the run rather than launch with no profile.
            None => Ok(Choice::Abort),
        }
    }
}

/// The user's answer to the missing-fragment prompt.
enum MissingChoice {
    /// Launch anyway; the referenced fragment(s) stay out of this overlay.
    Continue,
    /// Open `rosita studio` to fix the library — a handoff (the launch does not
    /// resume; the user re-runs after fixing).
    OpenStudio,
    /// Don't launch.
    Quit,
}

/// Prompt about fragment ids the active profile references but that aren't in
/// the library. Non-interactive runs (CI/piped) can't prompt, so they fall back
/// to the prior behavior — warn per missing id, then continue — and never block.
/// EOF (Ctrl-D) also continues, matching that pre-prompt default.
fn resolve_missing(prep: &super::Prepared, p: &Painter) -> crate::Result<MissingChoice> {
    let missing = &prep.composition.missing;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        for m in missing {
            crate::warn_user!("unknown fragment '{}' ({})", m.id, m.provenance);
        }
        return Ok(MissingChoice::Continue);
    }

    println!();
    for m in missing {
        println!(
            "  {} missing fragment {} {}",
            p.yellow("⚠"),
            p.bold(&format!("'{}'", m.id)),
            p.dim(&format!("({})", m.provenance)),
        );
    }
    let it = if missing.len() == 1 { "it" } else { "they" };
    println!(
        "  {}",
        p.dim(&format!("{it} won't be included in this launch's context."))
    );
    println!();
    println!("  how would you like to proceed?");
    println!("    1) ignore once and launch anyway");
    println!("    2) open rosita studio to fix it");
    println!("    3) quit");

    loop {
        print!("  ❯ ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(MissingChoice::Continue); // EOF — preserve the prior default.
        }
        match line.trim() {
            "1" => return Ok(MissingChoice::Continue),
            "2" => return Ok(MissingChoice::OpenStudio),
            "3" => return Ok(MissingChoice::Quit),
            _ => println!("  please enter 1, 2, or 3."),
        }
    }
}

/// Launch `rosita studio` so the user can fix the library, then stop. studio's
/// server blocks until Ctrl-C, so this is a clean handoff: rosita does not
/// resume the launch — the user re-runs `rosita run <agent>` after fixing.
fn open_studio_handoff(rt: &Runtime, agent: &str) -> crate::Result<()> {
    println!();
    println!("Opening rosita studio — fix the fragment, then re-run `rosita run {agent}`.");
    let args = StudioArgs {
        port: 7777,
        no_open: false,
    };
    crate::studio::serve(rt, &args)
}

/// Entry point for `rosita run`.
pub fn run(rt: &Runtime, args: &RunArgs) -> crate::Result<()> {
    let agent = args.agent.as_str();
    let p = Painter::auto();

    // Pull the latest config first — best-effort, throttled, timeout-bounded;
    // it never blocks the launch. Done before `prepare_with` so the render below
    // composes freshly-pulled fragments/profiles. Print the line right away.
    let sync_status = sync_before_render(rt);
    print_sync_step(&p, &sync_status);

    let prep = match prepare_with_live(rt, &StdinChooser, MissingPolicy::Defer, true) {
        Ok(prep) => prep,
        // The user cancelled the profile chooser — exit cleanly, launch nothing.
        Err(e) if e.downcast_ref::<Aborted>().is_some() => {
            println!(
                "  {} {}",
                p.yellow("✗"),
                p.dim("cancelled — no profile picked, nothing launched")
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // A profile that references a fragment id not in the library would silently
    // drop it from the overlay. Interrupt here — before any render/launch work —
    // and let the user decide: ignore once, open studio to fix it, or quit.
    if !prep.composition.missing.is_empty() {
        match resolve_missing(&prep, &p)? {
            MissingChoice::Continue => {}
            MissingChoice::OpenStudio => return open_studio_handoff(rt, agent),
            MissingChoice::Quit => {
                println!(
                    "  {} {}",
                    p.yellow("✗"),
                    p.dim(&format!(
                        "aborted — fix the fragment, then re-run `rosita run {agent}`"
                    ))
                );
                return Ok(());
            }
        }
    }

    let descriptor = adapters::descriptor(&prep.config, agent)
        .ok_or_else(|| anyhow!("unknown agent '{agent}'"))?
        .clone();
    let program = descriptor
        .launch
        .clone()
        .ok_or_else(|| anyhow!("agent '{agent}' is not launchable (no `launch` program)"))?;

    // Fail gracefully before doing any work if the agent CLI isn't installed —
    // no half-rendered overlay or stray global registration for a missing tool.
    // (Dry-run skips this: it only simulates and shouldn't require the binary.)
    if !rt.dry_run && !super::program_on_path(&program) {
        return Err(anyhow!(
            "the '{agent}' CLI ('{program}') isn't on your PATH — install it (or fix PATH), \
             then retry. `rosita render --agent {agent}` still writes the overlay."
        ));
    }

    // Preflight render (quiet — `run` prints its own concise summary).
    let rendered = !args.skip_render;
    let result = if rendered {
        let opts = ApplyOptions {
            codex_override: args.codex_override,
            codex_no_override: args.codex_no_override,
            force: false,
        };
        super::render::apply_for_agents(rt, &prep, &[agent.to_string()], &opts)?
            .into_iter()
            .next()
            .map(|(_, r)| r)
    } else {
        vlog!("skipping pre-launch render (--skip-render)");
        None
    };
    print_render_step(&p, &prep, agent, result.as_ref());

    let rendered_at = now_rfc3339();
    let launch_args = build_launch_args(
        &descriptor,
        &prep,
        result.as_ref(),
        &rendered_at,
        &args.args,
    );
    let extra_env = launch_context_env(&descriptor, &prep);

    if rt.dry_run {
        let env_prefix: String = extra_env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        println!(
            "  {} {}  {} {}",
            p.cyan("▸"),
            p.bold("dry run"),
            p.dim("would exec:"),
            p.dim(&format!("{env_prefix}{program} {}", launch_args.join(" ")))
        );
        return Ok(());
    }
    print_launch_step(&p, &program, &args.args);

    launch(&program, &launch_args, &rendered_at, &rt.cwd, &extra_env)
}

// --- sync + the stepped run summary ------------------------------------------

/// Best-effort auto-pull of the global config before rendering. Loads config to
/// read `[sync]`, then pulls if enabled + stale (the subsequent `prepare_with`
/// re-reads the now-current config). Never fails — errors map to `Offline`.
fn sync_before_render(rt: &Runtime) -> SyncStatus {
    let Ok(dir) = sync::config_dir() else {
        return SyncStatus::Disabled;
    };
    let repo_base = crate::context::repo_base_for(&rt.cwd);
    match crate::config::Config::load(&repo_base) {
        Ok(cfg) => sync::auto_pull(&cfg.sync, &dir),
        Err(_) => SyncStatus::Disabled,
    }
}

/// `  <glyph> <label>  <detail>` — one aligned step line.
fn step(p: &Painter, glyph: String, label: &str, detail: String) -> String {
    format!("  {glyph} {}  {detail}", p.bold(&format!("{label:<6}")))
}

fn print_sync_step(p: &Painter, s: &SyncStatus) {
    let line = match s {
        SyncStatus::Disabled => return,
        SyncStatus::Skipped { age } => step(
            p,
            p.green("✓"),
            "sync",
            format!(
                "up to date {}",
                p.dim(&format!("· synced {}", age_ago(*age)))
            ),
        ),
        SyncStatus::UpToDate => step(
            p,
            p.green("✓"),
            "sync",
            format!("up to date {}", p.dim("· synced just now")),
        ),
        SyncStatus::Pulled {
            commits,
            remote,
            took,
        } => step(
            p,
            p.green("⟳"),
            "sync",
            format!(
                "pulled {} {}",
                changes(*commits),
                p.dim(&format!("· {remote}  {}", dur(*took)))
            ),
        ),
        SyncStatus::Offline { last } => step(
            p,
            p.yellow("⚠"),
            "sync",
            format!(
                "offline — using local config{}",
                last.map(|a| p.dim(&format!(" · synced {}", age_ago(a))))
                    .unwrap_or_default()
            ),
        ),
        SyncStatus::Diverged => step(
            p,
            p.yellow("⚠"),
            "sync",
            "diverged — run `rosita sync` to reconcile".to_string(),
        ),
    };
    println!("{line}");
}

fn print_render_step(
    p: &Painter,
    prep: &super::Prepared,
    agent: &str,
    result: Option<&ApplyResult>,
) {
    let label = prep.profile_label();
    let profile = if label.is_empty() {
        "no profile"
    } else {
        label
    };
    let detail = match result {
        Some(r) => format!(
            "{profile} → {agent} {}",
            p.dim(&format!("· {}", hash::short(&r.context_hash)))
        ),
        None => format!("{profile} → {agent} {}", p.dim("· render skipped")),
    };
    println!("{}", step(p, p.green("✓"), "render", detail));
}

fn print_launch_step(p: &Painter, program: &str, args: &[String]) {
    let cmd = if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    };
    println!("{}", step(p, p.cyan("▸"), "launch", cmd));
}

/// "1 change" / "N changes".
fn changes(n: usize) -> String {
    if n == 1 {
        "1 change".to_string()
    } else {
        format!("{n} changes")
    }
}

/// "just now" / "Nm ago" / "Nh ago" / "Nd ago".
fn age_ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        "just now".to_string()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

/// "320ms" / "1.3s".
fn dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Env vars `rosita run` injects so an agent with no persistent local hook finds
/// the overlay at launch: maps `launch_context_dir_env` → the absolute
/// `launch_context_dir` under `.rosita/generated/` (e.g. Copilot's
/// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → `<repo>/.rosita/generated/copilot`).
fn launch_context_env(
    descriptor: &AgentDescriptor,
    prep: &super::Prepared,
) -> Vec<(String, String)> {
    let (Some(var), Some(rel)) = (
        &descriptor.launch_context_dir_env,
        &descriptor.launch_context_dir,
    ) else {
        return Vec::new();
    };
    let dir = crate::config::generated_dir(&prep.repo_base).join(rel);
    vec![(var.clone(), dir.to_string_lossy().into_owned())]
}

/// Prepend a freshness note via the agent's prompt flag when we just rendered.
fn build_launch_args(
    descriptor: &AgentDescriptor,
    prep: &super::Prepared,
    result: Option<&ApplyResult>,
    rendered_at: &str,
    user_args: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(flag), Some(result)) = (&descriptor.append_prompt_flag, result) {
        out.push(flag.clone());
        let guidance = result.profile_guidance.trim();
        if result.wiring_suppressed && !guidance.is_empty() {
            // Off-repo: the persistent importer was withheld (it would bleed into
            // every repo under here), so carry the machine context into *this*
            // session only — bounded so a huge overlay can't blow the arg up.
            const CAP: usize = 16 * 1024;
            let body = if guidance.len() > CAP {
                let mut end = CAP;
                while !guidance.is_char_boundary(end) {
                    end -= 1;
                }
                &guidance[..end]
            } else {
                guidance
            };
            out.push(format!(
                "rosita machine context for profile '{}' (session-only — not written to any file):\n\n{body}",
                prep.profile_label()
            ));
        } else {
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
    extra_env: &[(String, String)],
) -> crate::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("ROSITA_RUN", "1")
        .env("ROSITA_RENDERED_AT", rendered_at);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
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
