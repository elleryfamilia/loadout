//! `load run <agent> [args...]` — render the overlay, then launch the agent.
//!
//! This is the simple "preflight/wrapper" approach: refresh the generated files
//! for the chosen agent, then hand control to the real agent CLI (replacing this
//! process on Unix so signals and exit codes pass through cleanly). FUSE-style
//! live virtual files are explicitly out of scope for the MVP.
//!
//! Because loadout is the launching parent, it passes a freshness signal to the
//! agent: `LOADOUT_RUN=1` + `LOADOUT_RENDERED_AT` in the environment (so an agent
//! that can read env — or its hook — knows the context is current), and, for
//! agents with an `append_prompt_flag` (e.g. Claude's `--append-system-prompt`),
//! a short "context is fresh" note injected directly into the launch.
//!
//! For an agent with no persistent local hook but a `launch_context_dir_env`
//! (e.g. Copilot's `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`), loadout also sets that env
//! var to the directory holding the generated overlay, so the agent discovers it
//! at launch without any committed file being touched.

use std::io::{IsTerminal, Write as _};
use std::process::Command;

use anyhow::anyhow;

use super::apply::{
    apply_for_agents, learn_pending_count, print_sync_step, step, sync_before_render,
};
use super::{
    now_rfc3339, prepare_with_live, Aborted, Choice, MissingPolicy, ProfileChooser, Runtime,
};
use crate::adapters::{self, AgentDescriptor, ApplyOptions, ApplyResult};
use crate::binding::SkillDecision;
use crate::cli::{RunArgs, StudioArgs};
use crate::context::Context;
use crate::hash;
use crate::learn::trigger::{maybe_spawn, Trigger};
use crate::profile::LoadoutConfig;
use crate::skills::{self, LinkState, SkillState};
use crate::style::Painter;
use crate::vlog;

/// Interactive "which profile?" prompt for `load run` when 2+ profiles match
/// and no choice is remembered yet. Falls back to no-profile (no prompt) when
/// stdin/stdout isn't a terminal, so CI/piped runs never block.
struct StdinChooser;

impl ProfileChooser for StdinChooser {
    fn choose(&self, ctx: &Context, candidates: &[LoadoutConfig]) -> crate::Result<Choice> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            crate::warn_user!(
                "{} loadouts match but this isn't an interactive terminal — applying none. \
                 Re-run `load run` interactively (or use `load studio`) to pick.",
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
            "loadout › this {langs} project matches {} loadouts — pick one:",
            candidates.len()
        );
        println!("  ↑/↓ to move · Enter to select · or press a number · Esc/Ctrl-C to cancel");

        let items: Vec<String> = candidates.iter().map(|p| p.name.clone()).collect();
        match crate::tui::select(&items)? {
            Some(i) => {
                let name = candidates[i].name.clone();
                println!("loadout › bound \"{name}\" → remembered for this project; launching…");
                Ok(Choice::Profile(name))
            }
            // Cancelled (Esc / Ctrl-C / q / EOF): the user invoked loadout but
            // didn't pick — abort the run rather than launch with no profile.
            None => Ok(Choice::Abort),
        }
    }
}

/// The user's answer to the missing-fragment prompt.
enum MissingChoice {
    /// Launch anyway; the referenced fragment(s) stay out of this overlay.
    Continue,
    /// Open `load studio` to fix the library — a handoff (the launch does not
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
    println!("    2) open load studio to fix it");
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

/// Launch `load studio` so the user can fix the library, then stop. studio's
/// server blocks until Ctrl-C, so this is a clean handoff: loadout does not
/// resume the launch — the user re-runs `load run <agent>` after fixing.
fn open_studio_handoff(rt: &Runtime, agent: &str) -> crate::Result<()> {
    println!();
    println!("Opening load studio — fix the fragment, then re-run `load run {agent}`.");
    let args = StudioArgs {
        port: 7777,
        no_open: false,
        idle_timeout: "30m".to_string(),
    };
    crate::studio::serve(rt, &args)
}

/// Entry point for `load run`.
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
                p.dim("cancelled — no loadout picked, nothing launched")
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // Passive hook bootstrap (see `bootstrap_hook_registrations`): launching
    // any agent also wires the IDE freshness hooks of installed ones. Learn
    // hooks register only while learning is active on this machine.
    let learn_active = crate::learn::state::learn_active(&prep.config);
    for note in
        crate::adapters::bootstrap_hook_registrations(&prep.config, learn_active, rt.dry_run)
    {
        println!("  {} {}", p.green("✓"), p.dim(&note));
    }

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
                        "aborted — fix the fragment, then re-run `load run {agent}`"
                    ))
                );
                return Ok(());
            }
        }
    }

    // Embedded-skill preflight: keep accepted installs healthy, and — ask-once,
    // TTY-gated, only while the user looks pre-migration — offer the migrate
    // skill. Best-effort: a skill hiccup must never block the launch.
    if !rt.dry_run {
        skill_preflight(&prep, &p);
    }

    // Accept the agent's launch program as an alias for its id — people type
    // the binary they know (`load cursor-agent`) for the `cursor` agent.
    let descriptor = adapters::resolve_agent_token(&prep.config, agent)
        .ok_or_else(|| {
            anyhow!(
                "unknown agent '{agent}' (known: {})",
                adapters::agent_ids(&prep.config).join(", ")
            )
        })?
        .clone();
    let agent: &str = &descriptor.id;
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
             then retry. `load refresh --agent {agent}` still writes the overlay."
        ));
    }

    // Learn discovery count: folded ONCE for this whole invocation (zero cost
    // when `[learn]` is disabled) and reused for both the header (via the
    // render below) and the `learn` step line just after it.
    let learn_pending = learn_pending_count(&prep.config);

    // Preflight render (quiet — `run` prints its own concise summary).
    let rendered = !args.skip_render;
    let result = if rendered {
        let opts = ApplyOptions {
            codex_override: args.codex_override,
            codex_no_override: args.codex_no_override,
            force: false,
            workflow_override: args.workflow.clone(),
        };
        apply_for_agents(rt, &prep, &[agent.to_string()], &opts, learn_pending)?
            .into_iter()
            .next()
            .map(|(_, r)| r)
    } else {
        vlog!("skipping pre-launch render (--skip-render)");
        None
    };
    print_render_step(&p, &prep, agent, result.as_ref());

    // The workflow active for this run (override wins, else the profile binding),
    // resolved the same way the render did. Drives the launch-env wiring + notice.
    let workflow = prep
        .config
        .resolve_active_workflow(args.workflow.as_deref(), prep.composition.primary_profile());
    print_workflow_step(&p, workflow.as_ref());
    print_learn_step(&p, learn_pending);

    let rendered_at = now_rfc3339();
    let launch_args = build_launch_args(
        &descriptor,
        &prep,
        result.as_ref(),
        &rendered_at,
        &args.args,
    );
    let mut extra_env = launch_context_env(&descriptor, &prep);
    extra_env.extend(workflow_launch_env(workflow.as_ref(), &prep.repo_base));

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

    // Ensure the handoff-artifact dir exists so a stage command can write its
    // output (e.g. `$LOADOUT_PLAN_PATH`) without first creating the directory.
    // Best-effort: a failure here must never block the launch.
    if workflow.is_some() {
        std::fs::create_dir_all(crate::workflow::artifacts_dir(&prep.repo_base)).ok();
    }

    // Best-effort, throttled (once/day), time-bounded "update available" hint —
    // skipped on dry-run, non-TTY, and via `LOADOUT_NO_UPDATE_CHECK`. Printed
    // before the launch line since the launch `exec`s away on Unix.
    if let Some(detail) = crate::update::nudge_detail() {
        println!("{}", step(&p, p.cyan("↑"), "update", detail));
    }
    print_launch_step(&p, &program, &args.args);

    // Fire the trigger fast path right before the launch `exec()`s away
    // (unix) and replaces this process: `maybe_spawn` only ever waits on a
    // millisecond-lived intermediate, and the double-spawn it performs
    // reparents the real worker to init — so the worker survives the exec
    // that's about to happen. Never blocks, never errors outward; a disabled/
    // unactivated/off-interval machine pays only the cheap guard-chain checks.
    maybe_spawn(&prep.config, Trigger::Run);

    launch(&program, &launch_args, &rendered_at, &rt.cwd, &extra_env)
}

// --- embedded-skill preflight --------------------------------------------------

/// Maintain or offer loadout's embedded skills before launch. All branches are
/// best-effort: errors are logged verbosely and never abort the run. Undecided,
/// not-yet-installed skills are collected into ONE bundled offer — a fresh user
/// gets a single question no matter how many skills loadout ships.
fn skill_preflight(prep: &super::Prepared, p: &Painter) {
    let Some(home) = crate::config::home_dir() else {
        return;
    };
    let mut offerable: Vec<&skills::Skill> = Vec::new();
    for skill in skills::all() {
        let outcome = match crate::binding::read_skill_decision(skill.id) {
            Some(SkillDecision::Declined) => Ok(()),
            Some(SkillDecision::Accepted) => maintain_skill(&home, skill, p),
            None => match skills::status(&home, skill).state {
                // Already present with our marker (installed elsewhere or on
                // another loadout version): adopt silently instead of asking.
                SkillState::Managed { .. } => {
                    crate::binding::write_skill_decision(skill.id, SkillDecision::Accepted)
                }
                SkillState::Unmanaged => Ok(()), // the user's own copy; never ours
                SkillState::NotInstalled => {
                    offerable.push(skill);
                    Ok(())
                }
            },
        };
        if let Err(e) = outcome {
            vlog!("skill preflight for '{}' failed: {e:#}", skill.id);
        }
    }
    if !offerable.is_empty() {
        if let Err(e) = offer_skills(&home, &offerable, prep, p) {
            vlog!("skill offer failed: {e:#}");
        }
    }
}

/// The user said yes once — keep the install healthy: repair deleted/dangling
/// links, refresh a pristine install when this binary ships a newer version.
/// A user-deleted canonical dir is respected (recorded as declined, one notice);
/// user-edited files are never touched (`doctor` reports them).
fn maintain_skill(home: &std::path::Path, skill: &skills::Skill, p: &Painter) -> crate::Result<()> {
    let st = skills::status(home, skill);
    match st.state {
        SkillState::NotInstalled => {
            // The user deleted it; don't resurrect. Remember the opt-out.
            crate::binding::write_skill_decision(skill.id, SkillDecision::Declined)?;
            println!(
                "{}",
                step(
                    p,
                    p.dim("·"),
                    "skill",
                    p.dim(&format!(
                        "'{}' was removed — leaving it; `load skill install` restores it",
                        skill.id
                    )),
                )
            );
        }
        SkillState::Unmanaged
        | SkillState::Managed {
            user_modified: true,
            ..
        } => {} // hands off; `load doctor` reports the divergence
        SkillState::Managed {
            upgrade_available, ..
        } => {
            let links_broken = st
                .links
                .iter()
                .any(|l| matches!(l.state, LinkState::Missing | LinkState::Dangling));
            if upgrade_available || links_broken {
                skills::install(home, skill)?;
                let what = if upgrade_available {
                    "refreshed to this loadout's version"
                } else {
                    "repaired agent links"
                };
                println!(
                    "{}",
                    step(p, p.green("✓"), "skill", format!("'{}' {what}", skill.id))
                );
            }
        }
    }
    Ok(())
}

/// No decision recorded yet: offer the not-yet-installed skills once, as one
/// bundle — only on a real terminal, and only while the user looks
/// pre-migration (no profiles configured yet), which is exactly when the
/// migrate skill is useful. Configured users are never interrupted; they get
/// the skills via `load skill install` or studio.
fn offer_skills(
    home: &std::path::Path,
    offerable: &[&skills::Skill],
    prep: &super::Prepared,
    p: &Painter,
) -> crate::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    if !prep.config.profiles.is_empty() {
        return Ok(());
    }

    let ids = offerable
        .iter()
        .map(|s| format!("'{}'", s.id))
        .collect::<Vec<_>>()
        .join(", ");
    println!();
    println!(
        "  {} loadout ships agent skills {}",
        p.cyan("✦"),
        p.dim("(work in Claude Code, Codex, Gemini CLI, opencode)")
    );
    for skill in offerable {
        println!("    {} — {}", p.bold(skill.id), skill_blurb(skill.id));
    }
    println!("  install to {}?", p.bold("~/.agents/skills"));
    println!(
        "    1) yes — install {}",
        p.dim("(`load skill remove` undoes this)")
    );
    println!(
        "    2) no — don't ask again {}",
        p.dim("(`load skill install` re-enables)")
    );

    loop {
        print!("  ❯ ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(()); // EOF: no answer — stay undecided, never nag mid-launch
        }
        match line.trim() {
            "1" => {
                for skill in offerable {
                    skills::install(home, skill)?;
                    crate::binding::write_skill_decision(skill.id, SkillDecision::Accepted)?;
                }
                println!(
                    "{}",
                    step(
                        p,
                        p.green("✓"),
                        "skill",
                        format!(
                            "{ids} installed → {}",
                            home.join(".agents").join("skills").display()
                        ),
                    )
                );
                return Ok(());
            }
            "2" => {
                for skill in offerable {
                    crate::binding::write_skill_decision(skill.id, SkillDecision::Declined)?;
                }
                return Ok(());
            }
            _ => println!("  please enter 1 or 2."),
        }
    }
}

/// One-line pitch per shipped skill for the bundled offer.
fn skill_blurb(id: &str) -> &'static str {
    match id {
        "loadout-migrate" => "imports an existing CLAUDE.md/AGENTS.md into loadout",
        "loadout-remember" => "saves durable preferences you state mid-session as loadout guidance",
        "loadout-import-workflow" => "imports another repo's command suite as a loadout workflow",
        "loadout-plan-preview" => {
            "turns an agent-written plan.json into a reviewable plan.html preview"
        }
        _ => "an agent skill shipped with loadout",
    }
}

// --- the stepped run summary --------------------------------------------------

fn print_render_step(
    p: &Painter,
    prep: &super::Prepared,
    agent: &str,
    result: Option<&ApplyResult>,
) {
    let label = prep.profile_label();
    let profile = if label.is_empty() {
        "no loadout"
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

/// One concise run-summary line naming the active workflow (or nothing when
/// none is bound). Tells the user the spine is live and how to invoke a stage.
fn print_workflow_step(p: &Painter, workflow: Option<&crate::workflow::Workflow>) {
    let Some(wf) = workflow else { return };
    println!(
        "{}",
        step(
            p,
            p.cyan("◆"),
            "flow",
            format!(
                "{} {}",
                wf.title(),
                p.dim(&format!("· {} stages · /loadout:<stage>", wf.stages.len()))
            ),
        )
    );
}

/// The learn discovery step line: at most one, present only when candidates
/// are actually staged. `pending` is folded once (see `learn_pending_count`)
/// and passed in — this function is display-only, no I/O of its own.
fn print_learn_step(p: &Painter, pending: usize) {
    if pending == 0 {
        return;
    }
    println!(
        "{}",
        step(
            p,
            p.cyan("✦"),
            "learn",
            format!("{pending} staged suggestions await review — load studio"),
        )
    );
}

/// The `LOADOUT_<NAME>_PATH` env vars for a bound workflow's handoff artifacts,
/// each an absolute path under `.loadout/workflow/artifacts/`. A stage command
/// references these so it reads/writes the right file without hardcoding a path.
/// Empty when no workflow is active; unsafe artifact names are skipped.
fn workflow_launch_env(
    workflow: Option<&crate::workflow::Workflow>,
    repo_base: &std::path::Path,
) -> Vec<(String, String)> {
    let Some(wf) = workflow else {
        return Vec::new();
    };
    wf.artifacts()
        .iter()
        .filter_map(|name| {
            let var = crate::workflow::artifact_env_var(name)?;
            let path = crate::workflow::artifact_path(repo_base, name)?;
            Some((var, path.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Env vars `load run` injects so an agent with no persistent local hook finds
/// the overlay at launch: maps `launch_context_dir_env` → the absolute
/// `launch_context_dir` under `.loadout/generated/` (e.g. Copilot's
/// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` → `<repo>/.loadout/generated/copilot`).
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
                "loadout machine context for loadout '{}' (session-only — not written to any file):\n\n{body}",
                prep.profile_label()
            ));
        } else {
            out.push(format!(
                "loadout: project context refreshed for loadout '{}' at {rendered_at} — current. \
                 Run `load refresh` if the project changes mid-session.",
                prep.profile_label()
            ));
        }
    }
    out.extend(user_args.iter().cloned());
    out
}

/// Launch `program` with `args` in `cwd`, passing the loadout freshness signal in
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
        .env("LOADOUT_RUN", "1")
        .env("LOADOUT_RENDERED_AT", rendered_at);
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::commands::Prepared;
    use crate::config::Config;
    use crate::context::{ProjectCommands, SystemContext};

    /// A minimal `Prepared` — no profile applies (label "none"), which is all
    /// `build_launch_args` reads from it.
    fn prepared() -> Prepared {
        Prepared {
            repo_base: PathBuf::from("/repo"),
            config: Config::defaults(),
            context: Context {
                cwd: PathBuf::from("/repo"),
                repo_base: PathBuf::from("/repo"),
                repo_name: None,
                git: None,
                languages: Vec::new(),
                stacks: Vec::new(),
                package_managers: Vec::new(),
                custom_targets: Vec::new(),
                commands: ProjectCommands::default(),
                system: SystemContext {
                    os: "test-os".to_string(),
                    arch: "test-arch".to_string(),
                    hostname: "test-host".to_string(),
                    user: "test-user".to_string(),
                    parent_process: None,
                    host_class: None,
                },
                env: Default::default(),
            },
            composition: Default::default(),
        }
    }

    fn descriptor(id: &str) -> AgentDescriptor {
        adapters::builtin_agents()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("no built-in agent '{id}'"))
    }

    fn rendered() -> ApplyResult {
        ApplyResult {
            files: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
            context_hash: "sha256:test".to_string(),
            profile_guidance: String::new(),
            wiring_suppressed: false,
        }
    }

    #[test]
    fn user_args_follow_the_injected_freshness_flag() {
        let d = descriptor("claude");
        let flag = d
            .append_prompt_flag
            .clone()
            .expect("claude injects a prompt flag");
        let user = vec!["agents".to_string(), "--resume".to_string()];
        let out = build_launch_args(
            &d,
            &prepared(),
            Some(&rendered()),
            "2026-07-11T00:00:00Z",
            &user,
        );
        assert_eq!(out[0], flag);
        assert!(
            out[1].contains("context refreshed"),
            "expected the freshness note, got: {}",
            out[1]
        );
        assert_eq!(out[2..], user[..]);
    }

    #[test]
    fn agents_without_prompt_flag_get_pure_passthrough() {
        let d = descriptor("codex");
        assert!(d.append_prompt_flag.is_none());
        let user = vec!["exec".to_string(), "--full-auto".to_string()];
        let out = build_launch_args(&d, &prepared(), Some(&rendered()), "t", &user);
        assert_eq!(out, user);
    }

    #[test]
    fn skipped_render_still_passes_user_args() {
        // `--skip-render` → no ApplyResult → no injected note, args intact.
        let d = descriptor("claude");
        let user = vec!["--resume".to_string()];
        let out = build_launch_args(&d, &prepared(), None, "t", &user);
        assert_eq!(out, user);
    }
}
