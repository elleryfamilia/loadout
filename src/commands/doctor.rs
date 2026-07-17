//! `load doctor` — diagnose environment, config, and generated state.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::checks::{self, Status};
use super::{prepare, Runtime};
use crate::providers::CliProbe;
use crate::render::header;
use crate::{config, templates};

struct Checks {
    warns: usize,
    fails: usize,
}

impl Checks {
    fn new() -> Self {
        Checks { warns: 0, fails: 0 }
    }
    fn line(&mut self, status: Status, msg: impl AsRef<str>) {
        match status {
            Status::Warn => self.warns += 1,
            Status::Fail => self.fails += 1,
            Status::Ok => {}
        }
        println!("  {} {}", status.symbol(), msg.as_ref());
    }
}

/// Print a batch of extracted-check findings through the doctor tally.
fn report(c: &mut Checks, findings: Vec<checks::Finding>) {
    for f in findings {
        c.line(f.status, f.message);
    }
}

/// Entry point for `load doctor`.
pub fn run(rt: &Runtime) -> crate::Result<()> {
    let mut c = Checks::new();

    println!("Environment");
    match crate::providers::probe_cli("git") {
        CliProbe::Found(version) => c.line(Status::Ok, format!("git: {version}")),
        CliProbe::TimedOut => c.line(
            Status::Warn,
            "git: probe timed out — git is installed but not responding",
        ),
        CliProbe::Missing => c.line(
            Status::Fail,
            "git not found on PATH (git detection disabled)",
        ),
    }
    // Config + context. Suppress compose's `warn_user!` lines here — doctor
    // reports the same conditions (dangling refs, etc.) through its own checks,
    // so the raw stderr warnings would just duplicate them.
    println!("\nConfiguration");
    crate::report::set_quiet_warnings(true);
    let prep = prepare(rt);
    crate::report::set_quiet_warnings(false);
    let prep = match prep {
        Ok(p) => p,
        Err(e) => {
            c.line(
                Status::Fail,
                format!("failed to load config / detect context: {e:#}"),
            );
            print_summary(&c);
            return Ok(());
        }
    };
    if prep.config.sources.is_empty() {
        c.line(
            Status::Warn,
            "no config files found; author fragments and loadouts in ~/.config/loadout/config.toml (or run `load studio`)",
        );
    } else {
        for s in &prep.config.sources {
            c.line(Status::Ok, format!("loaded config: {}", s.display()));
        }
    }
    // Fragments/profiles authored in a repo layer (global-only mistake).
    check_repo_global_only(&mut c, &prep.repo_base);
    // Profiles referencing fragments that don't exist (e.g. a hand-deleted cap).
    report(&mut c, checks::dangling_fragment_refs(&prep.config));
    // Profiles binding an unknown workflow, and malformed user workflows.
    report(&mut c, checks::workflows(&prep.config));
    // The single-default invariant (one no-targets catch-all loadout).
    report(&mut c, checks::default_loadout(&prep.config));
    // Allowlist/denylist consistency.
    report(&mut c, checks::env_policy(&prep.config));
    // Private-data leak lint over public config layers.
    report(&mut c, checks::public_leaks(&prep.config));
    // Secret-looking strings in any config source layer (incl. local.toml).
    report(&mut c, checks::secret_leaks(&prep.config));
    // Prompt-injection-shaped phrasing in imported workflow step text.
    report(&mut c, checks::injection(&prep.config));
    // Script fragments whose output loadout would silently drop (non-zero exit).
    check_script_dropouts(&mut c, &prep);

    // Agents + their launch CLIs.
    println!("\nAgents ({} configured)", prep.config.agents.len());
    for a in &prep.config.agents {
        match &a.launch {
            Some(prog) => match crate::providers::probe_cli(prog) {
                CliProbe::Found(_) => {
                    c.line(Status::Ok, format!("{}: CLI '{prog}' found", a.id))
                }
                CliProbe::TimedOut => c.line(
                    Status::Warn,
                    format!(
                        "{}: CLI '{prog}' is installed but its version probe timed out — the CLI may be wedged",
                        a.id
                    ),
                ),
                CliProbe::Missing => c.line(
                    Status::Warn,
                    format!(
                        "{}: CLI '{prog}' not on PATH (needed for `run {}`)",
                        a.id, a.id
                    ),
                ),
            },
            None => c.line(Status::Ok, format!("{}: render-only", a.id)),
        }
        check_hook_registry(&mut c, a);
    }

    // Templates.
    println!("\nTemplates");
    match templates::resolve(&prep.repo_base, "overlay") {
        Ok(t) => c.line(Status::Ok, format!("overlay template ← {}", t.source)),
        Err(e) => c.line(Status::Fail, format!("overlay template: {e:#}")),
    }

    // Writability.
    println!("\nFilesystem");
    match writable(&prep.repo_base) {
        true => c.line(
            Status::Ok,
            format!("base dir is writable: {}", prep.repo_base.display()),
        ),
        false => c.line(
            Status::Fail,
            format!("base dir not writable: {}", prep.repo_base.display()),
        ),
    }
    if prep.context.git.is_some() {
        report(&mut c, checks::gitignore(&prep.repo_base));
    } else {
        c.line(
            Status::Ok,
            "not a git repo — non-repo mode (.gitignore not managed)",
        );
    }
    report(&mut c, checks::claude_marker(&prep.repo_base));

    // Generated overlays freshness.
    println!(
        "\nGenerated overlays (context {})",
        crate::hash::short(&prep.context.compute_hash())
    );
    check_overlays(&mut c, &prep);

    // Embedded agent skills (global; managed by `load skill`).
    println!("\nAgent skills (~/.agents/skills)");
    check_skills(&mut c);

    // Ambient learning (opt-in): activation/hook health, last run, pause and
    // corruption conditions, and a 14-day silence nudge. Reads the per-machine
    // state dir and $HOME hook files directly — outside checks.rs's
    // config-and-repo-only purity contract — so it lives here rather than in
    // checks.rs. Read-only: doctor never spawns the harvest worker.
    println!("\nLearning");
    report(&mut c, check_learn(&prep.config));

    print_summary(&c);
    Ok(())
}

/// Health of loadout's embedded skills: install state, content freshness, the
/// per-agent links, and the remembered ask-once decision.
fn check_skills(c: &mut Checks) {
    let Some(home) = config::home_dir() else {
        c.line(Status::Warn, "cannot resolve $HOME — skill checks skipped");
        return;
    };
    for skill in crate::skills::all() {
        let st = crate::skills::status(&home, skill);
        let decision = crate::binding::read_skill_decision(skill.id);
        use crate::binding::SkillDecision as D;
        use crate::skills::{LinkState, SkillState};

        match (&st.state, decision) {
            (SkillState::NotInstalled, Some(D::Declined)) => c.line(
                Status::Ok,
                format!("{}: not installed (declined — `load skill install` re-enables)", skill.id),
            ),
            (SkillState::NotInstalled, Some(D::Accepted)) => c.line(
                Status::Warn,
                format!(
                    "{}: accepted but missing from disk — `load skill install` restores it",
                    skill.id
                ),
            ),
            (SkillState::NotInstalled, None) => c.line(
                Status::Ok,
                format!(
                    "{}: not installed — `load skill install` imports your CLAUDE.md/AGENTS.md into loadout",
                    skill.id
                ),
            ),
            (SkillState::Unmanaged, _) => c.line(
                Status::Ok,
                format!(
                    "{}: present but not loadout-managed (your own copy; loadout leaves it alone)",
                    skill.id
                ),
            ),
            (SkillState::Managed { user_modified: true, .. }, _) => c.line(
                Status::Warn,
                format!(
                    "{}: installed with local edits — auto-upgrade is off ('load skill install' would not overwrite)",
                    skill.id
                ),
            ),
            (SkillState::Managed { upgrade_available: true, .. }, _) => c.line(
                Status::Warn,
                format!(
                    "{}: installed but stale — `load skill install` upgrades it to this loadout's version",
                    skill.id
                ),
            ),
            (SkillState::Managed { .. }, _) => c.line(
                Status::Ok,
                format!("{}: installed and current", skill.id),
            ),
        }

        if matches!(st.state, SkillState::Managed { .. }) {
            for link in &st.links {
                match link.state {
                    LinkState::Missing | LinkState::Dangling => c.line(
                        Status::Warn,
                        format!(
                            "{}: link {} is {} — `load skill install` repairs it",
                            skill.id,
                            link.path.display(),
                            if link.state == LinkState::Missing {
                                "missing"
                            } else {
                                "dangling"
                            },
                        ),
                    ),
                    LinkState::Foreign => c.line(
                        Status::Warn,
                        format!(
                            "{}: {} exists but isn't loadout's — left alone",
                            skill.id,
                            link.path.display()
                        ),
                    ),
                    LinkState::CopyManaged => c.line(
                        Status::Ok,
                        format!(
                            "{}: {} is a copy (symlink fallback) — upgrades re-copy it",
                            skill.id,
                            link.path.display()
                        ),
                    ),
                    LinkState::Linked | LinkState::AgentAbsent => {}
                }
            }
        }
    }
}

/// 14-day silence threshold for the "no trigger fired" nudge (design doc's
/// Error-handling section: a machine that's been on this long with nothing
/// logged is worth a nudge — not a failure, just possibly-forgotten).
const LEARN_STALE: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 60 * 60);

/// Ambient-learning diagnostics: intent vs. per-machine activation, hook
/// registration/health per agent (both dialects — Cursor's flat `hooks.json`
/// and Claude's nested `.claude/settings.json` schema), last run + next
/// eligibility, paused-after-failures, a corrupt watermark store, and a
/// 14-day no-trigger nudge. Reads the per-machine state dir and `$HOME` hook
/// files (outside checks.rs's config-and-repo-only purity contract) — the
/// reason this lives in doctor.rs rather than checks.rs. Never spawns the
/// harvest worker: every read here is passive.
fn check_learn(cfg: &config::Config) -> Vec<checks::Finding> {
    // Same convention as `check_skills`: an unresolvable $HOME/state dir gets
    // a visible skip line, never a silently empty section.
    let Some(learn_dir) = crate::learn::state::learn_dir() else {
        return vec![checks::Finding::warn(
            "cannot resolve the learning state dir (no home) — learning checks skipped",
        )];
    };
    let Some(home) = config::home_dir() else {
        return vec![checks::Finding::warn(
            "cannot resolve $HOME — learning checks skipped",
        )];
    };
    check_learn_at(cfg, &learn_dir, &home, SystemTime::now())
}

/// Path-explicit core of [`check_learn`] (the `_at` seam used throughout
/// `crate::learn`), so unit tests can inject fixture dirs instead of the real
/// per-machine state dir / `$HOME`.
fn check_learn_at(
    cfg: &config::Config,
    learn_dir: &Path,
    home: &Path,
    now: SystemTime,
) -> Vec<checks::Finding> {
    let selection =
        if cfg.learn.enabled && crate::learn::state::read_activation_at(learn_dir).is_some() {
            crate::learn::agent_cli::select(&cfg.learn)
        } else {
            crate::learn::agent_cli::Selection::None
        };
    check_learn_at_with_selection(cfg, learn_dir, home, now, &selection)
}

/// Selection-explicit core for deterministic tests of current CLI
/// compatibility. The production wrapper probes the real configured CLI once.
fn check_learn_at_with_selection(
    cfg: &config::Config,
    learn_dir: &Path,
    home: &Path,
    now: SystemTime,
    selection: &crate::learn::agent_cli::Selection,
) -> Vec<checks::Finding> {
    use checks::Finding;
    let mut out = Vec::new();

    let activation = crate::learn::state::read_activation_at(learn_dir);
    let activated = activation.is_some();
    let enabled = cfg.learn.enabled;
    let active = enabled && activated;

    // Intent (synced) vs. this machine's activation ack.
    match (enabled, activated) {
        (false, _) => out.push(Finding::ok("learning is off — `load learn on` to enable")),
        (true, false) => out.push(Finding::warn(
            "learning is enabled (synced) but not activated on this machine — \
             run `load learn on` on this machine",
        )),
        (true, true) => out.push(Finding::ok(format!(
            "learning is on and activated ({})",
            activation
                .as_ref()
                .map(|a| a.hostname.as_str())
                .unwrap_or("this machine")
        ))),
    }
    // A local activation ack surviving a synced disable (Decision #4: `off`
    // is only *eventually* effective on other machines) — distinct from the
    // per-hook orphan check below, since the ack itself is what needs
    // cleaning even if no hook was ever registered here.
    if activated && !enabled {
        out.push(Finding::warn(
            "this machine is still activated for learning even though it's disabled \
             (synced) — run `load learn off` here to clean up",
        ));
    }

    // Hook registration/health, per agent, per dialect.
    for agent in &cfg.agents {
        for hr in &agent.learn_hooks {
            check_learn_hook_at(&mut out, &agent.id, hr, home, active);
        }
    }

    // Last run + outcome (log/stamps), shown once there's history or while
    // learning is on and waiting for a first run.
    let log = crate::learn::worker::read_log(&learn_dir.join("log.jsonl"));
    let unresolved = active
        .then(|| crate::learn::worker::latest_unresolved_failure(learn_dir))
        .flatten();
    let latest_is_unresolved = match (log.last(), unresolved.as_ref()) {
        (Some(latest), Some(failure)) => {
            latest.ts == failure.ts
                && latest.trigger == failure.trigger
                && latest.outcome == failure.outcome
                && latest.error_stage == failure.error_stage
                && latest.error_code == failure.error_code
        }
        _ => false,
    };
    if enabled || !log.is_empty() {
        match log.last() {
            Some(r) => {
                let summary = format!(
                    "last run: {} — {} ({}), {} session{}, {} candidate{}",
                    r.ts,
                    r.outcome,
                    r.cli.as_deref().unwrap_or("?"),
                    r.sessions,
                    plural(r.sessions),
                    r.candidates,
                    plural(r.candidates),
                );
                if latest_is_unresolved {
                    out.push(Finding::warn(format!(
                        "{summary} — {}",
                        super::harvest::format_logged_diagnostic(r)
                    )));
                } else {
                    out.push(Finding::ok(summary));
                }
            }
            None => out.push(Finding::ok("last run: none yet")),
        }
    }

    // Breaker history is actionable only for an active learner. The worker's
    // counter-aware lookup deliberately survives later empty/no-op log rows.
    if active {
        if let Some(failure) = unresolved.as_ref().filter(|_| !latest_is_unresolved) {
            out.push(Finding::warn(format!(
                "unresolved harvest failure: {}",
                super::harvest::format_logged_diagnostic(failure)
            )));
        }

        // Unsupported selection is pre-spend and does not increment the
        // breaker, so surface it independently even with no sessions or log.
        if let crate::learn::agent_cli::Selection::Unsupported(unsupported) = selection {
            let diagnostic =
                crate::learn::worker::HarvestDiagnostic::UnsupportedCli(unsupported.clone());
            out.push(Finding::warn(format!(
                "current extraction CLI is unsupported: {}",
                super::harvest::format_diagnostic(&diagnostic)
            )));
        }
    }

    // Next eligibility — the trigger module's own guard-6/7 math
    // ([`crate::learn::trigger::eligibility_at`]), so this can never drift
    // from what a real trigger would decide (or from `load learn status`).
    if enabled {
        let e = crate::learn::trigger::eligibility_at(learn_dir, cfg.learn.interval, now);
        out.push(Finding::ok(format!(
            "next eligibility: {}",
            eligibility_note(&e)
        )));
    }

    // Paused after repeated failures.
    if active && crate::learn::state::paused_at(learn_dir) {
        out.push(Finding::warn(
            "learning paused after repeated failures — a successful `load harvest` \
             or `load learn on` clears it",
        ));
    }

    // Corrupt watermark store — loud, never silently reset.
    let wm = crate::learn::watermarks::Watermarks::load_from(&learn_dir.join("watermarks.json"));
    if wm.corrupt() {
        out.push(Finding::warn(
            "learning watermark store is corrupt — run `load learn reset` to re-baseline",
        ));
    }

    // 14-day no-trigger nudge: newest log entry (any trigger) vs. now; an
    // empty/unparseable log counts as stale too (it never ran).
    if enabled {
        let stale = match log.last().and_then(|r| r.ts_unix()) {
            Some(ts) => {
                let now_secs = now
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                now_secs - ts > LEARN_STALE.as_secs() as i64
            }
            None => true,
        };
        if stale {
            out.push(Finding::warn(
                "learning is on but no trigger has fired in 14+ days — \
                 run `load harvest` or open `load studio` to check in",
            ));
        }
    }

    out
}

/// One agent's learn-hook health: malformed JSON, Claude's `disableAllHooks`
/// carve-out (informational, not a warning, per the T17/T18 carry-note), and
/// otherwise registered-vs-expected across both hook-file dialects.
fn check_learn_hook_at(
    out: &mut Vec<checks::Finding>,
    agent_id: &str,
    hr: &crate::adapters::HookRegistry,
    home: &Path,
    active: bool,
) {
    use checks::Finding;
    let path = home.join(&hr.hooks_file);
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            // Absent entirely. Only worth a word when learning is active here
            // AND the agent shows signs of being installed (the same signal
            // `register_if_installed` uses) — otherwise this is just "the
            // tool isn't on this machine," not a health problem.
            let installed = path.parent().map(|p| p.is_dir()).unwrap_or(false);
            if active && installed {
                out.push(Finding::warn(format!(
                    "{agent_id}: {} learning hook not registered in {} — \
                     run `load refresh` to register it",
                    hr.event, hr.hooks_file
                )));
            }
            return;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            out.push(Finding::warn(format!(
                "{agent_id}: {} is not valid JSON — can't verify the learning hook; fix it by hand",
                hr.hooks_file
            )));
            return;
        }
    };

    // Claude's global kill switch: every hook (ours included) is inert, so
    // its absence here is expected, not a fault — say why instead of warning,
    // in the exact sentence hook registration already prints (the shared
    // constant, so the two surfaces can never drift).
    if hr.format == crate::adapters::HookFormat::ClaudeNested
        && json
            .get("disableAllHooks")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        if active {
            out.push(Finding::ok(crate::adapters::DISABLE_ALL_HOOKS_NOTE));
        }
        return;
    }

    let suffix = format!(" {}", hr.subcommand);
    let present = json_has_command_suffix(&json, &suffix);
    match (active, present) {
        (true, true) => out.push(Finding::ok(format!(
            "{agent_id}: {} learning hook registered",
            hr.event
        ))),
        (true, false) => out.push(Finding::warn(format!(
            "{agent_id}: {} learning hook not registered in {} — run `load refresh` to register it",
            hr.event, hr.hooks_file
        ))),
        (false, true) => out.push(Finding::warn(format!(
            "{agent_id}: {} learning hook still registered in {} but learning is inactive here — \
             run `load learn off` here to clean up",
            hr.event, hr.hooks_file
        ))),
        (false, false) => {} // nothing registered, learning inactive — expected, silent
    }
}

/// Whether any `"command"` string value anywhere in `v` ends with `suffix`.
/// Deliberately dialect-agnostic rather than hand-rolled per dialect: Cursor's
/// flat `{ hooks: { <event>: [ {command} ] } }` and Claude's nested
/// `{ hooks: { <event>: [ { hooks: [ {command} ] } ] } }` both nest a
/// `"command"` key somewhere, so one recursive walk finds either.
fn json_has_command_suffix(v: &serde_json::Value, suffix: &str) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(cmd) = map.get("command").and_then(serde_json::Value::as_str) {
                if cmd.ends_with(suffix) {
                    return true;
                }
            }
            map.values().any(|v| json_has_command_suffix(v, suffix))
        }
        serde_json::Value::Array(arr) => arr.iter().any(|v| json_has_command_suffix(v, suffix)),
        _ => false,
    }
}

/// A short "eligible now" / "in ~Xh" note from the trigger module's
/// guard-6/7 view ([`crate::learn::trigger::eligibility_at`] — the one source
/// of truth for next-eligibility, shared with `load learn status`).
fn eligibility_note(e: &crate::learn::trigger::Eligibility) -> String {
    if e.now() {
        "eligible now".to_string()
    } else {
        let secs = e.wait.as_secs();
        if secs >= 3600 {
            format!("in ~{}h{}m", secs / 3600, (secs % 3600) / 60)
        } else if secs >= 60 {
            format!("in ~{}m", secs / 60)
        } else {
            format!("in ~{secs}s")
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Execute each configured script-backed fragment and flag any that exit
/// non-zero while still printing to stdout. loadout drops a probe's output when
/// its script exits non-zero, so such a fragment renders as nothing — usually a
/// final `[ cond ] && cmd` that short-circuits. (Exit non-zero with *no* stdout
/// is the normal "tool absent / nothing found" case and is left alone.) These
/// are the user's own probes, which already run at render time, so executing
/// them here adds no new capability — only a diagnosis.
fn check_script_dropouts(c: &mut Checks, prep: &super::Prepared) {
    // Honor `allow_exec = false` (the off-switch): render skips such a fragment
    // without running it, so doctor must not execute it either — doing so would
    // run an opted-out command and misdiagnose it (it renders a skip note, not
    // "nothing").
    let scripts: Vec<&crate::fragment::Fragment> = prep
        .config
        .fragments
        .iter()
        .filter(|f| f.command.is_some() && f.allow_exec)
        .collect();
    if scripts.is_empty() {
        return;
    }
    println!("\nScript fragments ({} probed)", scripts.len());
    let mut clean = 0usize;
    for f in scripts {
        // Doctor is a script execution site too: warn on an out-of-band change
        // before running the script, same as render/run/refresh and target
        // detection do — otherwise `load doctor` would silently run a changed
        // script while every other path warns.
        if let Some(hashes) = crate::trust::fragment_hashes(f) {
            crate::trust::check_and_warn(crate::trust::Kind::Fragment, &f.id, &hashes);
        }
        let cmd = f.command.as_deref().unwrap_or_default();
        let out = crate::providers::run_once_in(cmd, f.script_lang.as_deref(), &prep.repo_base);
        let status = out.data.get("status").and_then(serde_json::Value::as_i64);
        let stdout = out
            .data
            .get("stdout")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if status != Some(0) && !stdout.trim().is_empty() {
            let exit = status
                .map(|s| format!("exits {s}"))
                .unwrap_or_else(|| "was killed by a signal".to_string());
            c.line(
                Status::Warn,
                format!(
                    "{}: prints output but {exit} — loadout drops a probe's output on a non-zero exit, so this renders nothing. End the script with `exit 0`.",
                    f.id
                ),
            );
        } else {
            clean += 1;
        }
    }
    if clean > 0 {
        c.line(
            Status::Ok,
            format!("{clean} script fragment(s) exit cleanly"),
        );
    }
}

fn print_summary(c: &Checks) {
    println!();
    if c.fails > 0 {
        println!("doctor: {} failure(s), {} warning(s)", c.fails, c.warns);
    } else if c.warns > 0 {
        println!("doctor: healthy, {} warning(s)", c.warns);
    } else {
        println!("doctor: all good ✓");
    }
}

fn writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".loadout-doctor-")
        .tempfile_in(dir)
        .is_ok()
}

/// For an agent with a user-level freshness hook (e.g. Cursor): is loadout's
/// entry registered, and does the binary it points at still exist? A stale
/// path happens when the load binary moves (or its volume is unmounted) —
/// the next `load refresh` re-points it.
fn check_hook_registry(c: &mut Checks, a: &crate::adapters::AgentDescriptor) {
    let Some(hr) = &a.hook_registry else { return };
    let Some(home) = config::home_dir() else {
        return;
    };
    let path = home.join(&hr.hooks_file);
    let Ok(content) = std::fs::read_to_string(&path) else {
        c.line(
            Status::Warn,
            format!(
                "{}: no {} — IDE sessions won't self-refresh (any `load refresh --agent {}` registers the hook)",
                a.id, hr.hooks_file, a.id
            ),
        );
        return;
    };
    let suffix = format!(" {}", hr.subcommand);
    let command = serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|v| {
            v.get("hooks")?.as_object()?.values().find_map(|arr| {
                arr.as_array()?.iter().find_map(|e| {
                    let cmd = e.get("command")?.as_str()?;
                    cmd.ends_with(&suffix).then(|| cmd.to_string())
                })
            })
        });
    match command {
        None => c.line(
            Status::Warn,
            format!(
                "{}: `load {}` hook not registered in {} — IDE sessions won't self-refresh \
                 (any `load refresh --agent {}` registers it)",
                a.id, hr.subcommand, hr.hooks_file, a.id
            ),
        ),
        Some(cmd) => {
            // The registered command is `"<binary>" <subcommand>` — check the
            // binary still exists.
            let bin = cmd.trim_end_matches(&suffix).trim().trim_matches('"');
            if Path::new(bin).is_file() {
                c.line(
                    Status::Ok,
                    format!("{}: {} hook registered ({})", a.id, hr.event, hr.hooks_file),
                );
            } else {
                c.line(
                    Status::Warn,
                    format!(
                        "{}: registered hook points at a missing binary ({bin}) — \
                         run `load refresh --agent {}` to re-point it",
                        a.id, a.id
                    ),
                );
            }
        }
    }
}

/// Fragments and profiles are global-only. A repo `config.toml`/`local.toml`
/// that declares them is silently ignored by the loader (so the mistake is
/// invisible at render time) — surface it here. Scans the raw file because the
/// stripped tables never reach the merged config.
fn check_repo_global_only(c: &mut Checks, repo_base: &Path) {
    for (label, path) in [
        ("config.toml", config::repo_config_path(repo_base)),
        ("local.toml", config::repo_local_path(repo_base)),
    ] {
        if let Some(what) = repo_declares_caps_or_profiles(&path) {
            c.line(
                Status::Warn,
                format!(
                    ".loadout/{label} declares {what} — these are global-only and are ignored here; move them to ~/.config/loadout/config.toml"
                ),
            );
        }
    }
}

/// What global-only tables (if any) a repo TOML file declares. `None` when the
/// file is absent, unparseable, or declares none. Workflows are global-only too
/// (the loader strips them from repo layers, see [`strip_global_only`]), so a
/// repo `[[workflows]]` is flagged alongside fragments/loadouts/targets.
fn repo_declares_caps_or_profiles(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let val: toml::Value = toml::from_str(&text).ok()?;
    let has = |k: &str| {
        val.get(k)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    };
    // The load list accepts both the canonical `[[loadouts]]` key and the
    // legacy `[[profiles]]` alias, so a repo declaring either is global-only.
    let has_loadouts = has("loadouts") || has("profiles");
    let present: Vec<&str> = [
        has("fragments").then_some("fragments"),
        has_loadouts.then_some("loadouts"),
        has("targets").then_some("targets"),
        has("workflows").then_some("workflows"),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!present.is_empty()).then(|| oxford_join(&present))
}

/// Join names for a human-readable list: `"a"`, `"a and b"`, or `"a, b, and c"`.
fn oxford_join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn check_overlays(c: &mut Checks, prep: &super::Prepared) {
    let dir = config::generated_dir(&prep.repo_base);
    // Resolve the bound workflow the same way the renderer does, so the
    // staleness comparison matches the stamped fingerprint.
    let workflow = prep
        .config
        .workflow_for_profile(prep.composition.primary_profile());
    let current =
        crate::render::overlay_fingerprint(&prep.context, &prep.composition, workflow.as_ref());
    let mut found = false;
    for a in &prep.config.agents {
        let path = dir.join(&a.generated_filename);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        found = true;
        match header::extract_context_hash(&content) {
            Some(h) if h == current => c.line(Status::Ok, format!("{}: up to date", a.id)),
            Some(_) => c.line(
                Status::Warn,
                format!("{}: stale (run `load refresh`)", a.id),
            ),
            None => c.line(
                Status::Warn,
                format!("{}: present but missing loadout header", a.id),
            ),
        }
    }
    if !found {
        c.line(
            Status::Warn,
            "no overlays generated yet (run `load refresh`)",
        );
    }
}

#[cfg(test)]
mod learn_tests {
    use super::*;
    use crate::config::Config;
    use crate::learn::state::Activation;
    use checks::Status;
    use std::time::Duration;

    /// A fresh learn dir + home dir under one tempdir, plus the guard kept
    /// alive so the paths stay valid for the test's lifetime.
    struct Env {
        _tmp: tempfile::TempDir,
        learn_dir: std::path::PathBuf,
        home: std::path::PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let learn_dir = tmp.path().join("state/learn");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&learn_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Env {
            _tmp: tmp,
            learn_dir,
            home,
        }
    }

    fn activate(learn_dir: &Path) {
        crate::learn::state::write_activation_at(
            learn_dir,
            &Activation {
                machine_id: "test-machine".into(),
                hostname: "test.local".into(),
                activated_at: "2026-07-10T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    /// A far-future instant so stamp/log-age math never accidentally lands in
    /// the past relative to "now" unless a test backdates something itself.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    fn find<'a>(findings: &'a [checks::Finding], needle: &str) -> Option<&'a checks::Finding> {
        findings.iter().find(|f| f.message.contains(needle))
    }

    /// Fetch a built-in agent's learn-hook descriptor by agent id (from the
    /// real `builtin_agents()` table via `Config::defaults()`), so fixtures
    /// write hook files matched by the descriptor's actual subcommand suffix
    /// rather than a hand-duplicated string that could drift.
    fn learn_hook<'a>(cfg: &'a Config, agent_id: &str) -> &'a crate::adapters::HookRegistry {
        cfg.agents
            .iter()
            .find(|a| a.id == agent_id)
            .and_then(|a| a.learn_hooks.first())
            .unwrap_or_else(|| panic!("no learn hook registered for agent '{agent_id}'"))
    }

    // --- healthy / quiet ----------------------------------------------------

    /// A totally vanilla install (learning never touched) must produce exactly
    /// one quiet `Ok` line naming the enabling command — no warnings, no
    /// per-hook noise, no log/eligibility clutter.
    #[test]
    fn healthy_quiet_when_learning_untouched() {
        let e = env();
        let cfg = Config::defaults(); // learn.enabled = false by default
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        assert_eq!(findings.len(), 1, "expected exactly one line: {findings:?}");
        assert_eq!(findings[0].status, Status::Ok);
        assert!(findings[0].message.contains("load learn on"));
    }

    // --- enabled but not activated here --------------------------------------

    #[test]
    fn enabled_but_not_activated_warns_with_clearing_action() {
        let e = env();
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "not activated on this machine").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.message.contains("load learn on"),
            "must name the clearing action: {}",
            f.message
        );
    }

    // --- activated but the synced intent flipped off -------------------------

    #[test]
    fn ack_surviving_a_synced_disable_warns_here_with_learn_off() {
        let e = env();
        activate(&e.learn_dir); // this machine was activated…
        let cfg = Config::defaults(); // …but `enabled` is false (default)
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "still activated for learning").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load learn off"));
    }

    // --- orphaned hooks, both dialects ---------------------------------------

    /// Cursor's flat `hooks.json` dialect: a learn hook left registered after
    /// learning went inactive here must be flagged with the Decision #4
    /// clearing action.
    #[test]
    fn orphaned_cursor_flat_hook_warns_to_clean_up() {
        let e = env();
        let cfg = Config::defaults(); // not enabled, not activated → inactive
        let hr = learn_hook(&cfg, "cursor");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"hooks":{{"{}":[{{"command":"\"/usr/local/bin/load\" {}"}}]}}}}"#,
                hr.event, hr.subcommand
            ),
        )
        .unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "cursor: stop learning hook still registered")
            .expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load learn off"));
    }

    /// Claude's nested `.claude/settings.json` matcher schema: same orphan
    /// condition, different on-disk dialect — the check must handle both.
    #[test]
    fn orphaned_claude_nested_hook_warns_to_clean_up() {
        let e = env();
        let cfg = Config::defaults();
        let hr = learn_hook(&cfg, "claude");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"hooks":{{"{}":[{{"hooks":[{{"type":"command","command":"\"/usr/local/bin/load\" {}","timeout":10}}]}}]}}}}"#,
                hr.event, hr.subcommand
            ),
        )
        .unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(
            &findings,
            "claude: SessionEnd learning hook still registered",
        )
        .expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load learn off"));
    }

    /// While learning IS active here, a correctly registered hook is quiet
    /// (`Ok`), not a warning — the orphan message only fires the other way.
    #[test]
    fn registered_hook_while_active_is_ok_not_warn() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        let hr = learn_hook(&cfg, "cursor");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"hooks":{{"{}":[{{"command":"\"/usr/local/bin/load\" {}"}}]}}}}"#,
                hr.event, hr.subcommand
            ),
        )
        .unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "cursor: stop learning hook registered").expect("must be present");
        assert_eq!(f.status, Status::Ok);
        assert!(
            find(&findings, "still registered").is_none(),
            "must not also fire the orphan warning: {findings:?}"
        );
    }

    // --- hook expected but missing, while active ------------------------------

    /// Learning is active and the agent looks installed (its dotfile dir
    /// exists), but the hooks file is absent entirely — the file-absent early
    /// return must still warn with the `load refresh` clearing action.
    #[test]
    fn active_with_hooks_file_absent_warns_not_registered() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        // The agent's dotfile dir exists (installed signal) but no hooks file.
        std::fs::create_dir_all(e.home.join(".cursor")).unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "cursor: stop learning hook not registered")
            .expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.message.contains("load refresh"),
            "must name the clearing action: {}",
            f.message
        );
    }

    /// Learning is active and the hooks file exists, but carries no entry with
    /// our subcommand suffix (only a foreign hook) — the `(active, !present)`
    /// arm must warn with the `load refresh` clearing action.
    #[test]
    fn active_with_file_present_but_our_entry_absent_warns_not_registered() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        let hr = learn_hook(&cfg, "claude");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Valid JSON, foreign hook only — nothing carrying our suffix.
        std::fs::write(
            &path,
            r#"{"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"\"/opt/other\" wrapup"}]}]}}"#,
        )
        .unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "claude: SessionEnd learning hook not registered")
            .expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.message.contains("load refresh"),
            "must name the clearing action: {}",
            f.message
        );
    }

    // --- disableAllHooks: informational, not a warning -----------------------

    #[test]
    fn claude_disable_all_hooks_is_informational_while_active() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        let hr = learn_hook(&cfg, "claude");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"disableAllHooks":true}"#).unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "disableAllHooks").expect("must note it: {findings:?}");
        assert_eq!(
            f.status,
            Status::Ok,
            "absence under disableAllHooks is informational, not a warning"
        );
        // The exact sentence is the shared constant hook registration prints
        // too — one source of truth, no drift between the two surfaces.
        assert_eq!(f.message, crate::adapters::DISABLE_ALL_HOOKS_NOTE);
        assert!(
            find(&findings, "not registered").is_none(),
            "must not ALSO warn 'not registered': {findings:?}"
        );
    }

    // --- malformed hook JSON --------------------------------------------------

    #[test]
    fn malformed_hook_json_is_flagged() {
        let e = env();
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        activate(&e.learn_dir);
        let hr = learn_hook(&cfg, "claude");
        let path = e.home.join(&hr.hooks_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "not valid JSON").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
    }

    // --- paused after repeated failures ---------------------------------------

    #[test]
    fn paused_after_failures_names_the_clearing_action() {
        let e = env();
        crate::learn::state::record_failure_at(&e.learn_dir);
        crate::learn::state::record_failure_at(&e.learn_dir); // 2 → paused
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        activate(&e.learn_dir);
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "paused after repeated failures").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load harvest") && f.message.contains("load learn on"));
    }

    #[test]
    fn disabled_learning_treats_breaker_failure_and_pause_as_history() {
        let e = env();
        std::fs::write(
            e.learn_dir.join("log.jsonl"),
            concat!(
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"ambient","cli":"claude","sessions":1,"candidates":0,"outcome":"failed","error_stage":"validate_output","error_code":"output_json_invalid","error":"The extraction CLI returned invalid JSON output."}"#,
                "\n"
            ),
        )
        .unwrap();
        crate::learn::state::record_failure_at(&e.learn_dir);
        crate::learn::state::record_failure_at(&e.learn_dir);

        let cfg = Config::defaults();
        let findings = check_learn_at_with_selection(
            &cfg,
            &e.learn_dir,
            &e.home,
            now(),
            &crate::learn::agent_cli::Selection::None,
        );
        assert!(find(&findings, "unresolved harvest failure").is_none());
        assert!(find(&findings, "paused after repeated failures").is_none());
        assert!(find(&findings, "output_json_invalid").is_none());
    }

    #[test]
    fn latest_breaker_failure_is_one_warn_last_run_finding() {
        let e = env();
        activate(&e.learn_dir);
        std::fs::write(
            e.learn_dir.join("log.jsonl"),
            concat!(
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"ambient","cli":"claude","sessions":1,"candidates":0,"outcome":"failed","error_stage":"validate_output","error_code":"output_json_invalid","error":"SECRET_FORGED_ERROR"}"#,
                "\n"
            ),
        )
        .unwrap();
        crate::learn::state::record_failure_at(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;

        let findings = check_learn_at_with_selection(
            &cfg,
            &e.learn_dir,
            &e.home,
            now(),
            &crate::learn::agent_cli::Selection::None,
        );
        let last = find(&findings, "last run:").expect("must show last run: {findings:?}");
        assert_eq!(last.status, Status::Warn);
        assert!(last.message.contains("validate_output/output_json_invalid"));
        assert!(last.message.contains("returned invalid JSON output"));
        assert!(!last.message.contains("SECRET_FORGED_ERROR"));
        assert!(
            find(&findings, "unresolved harvest failure").is_none(),
            "latest failure must not get a duplicate warning: {findings:?}"
        );
    }

    #[test]
    fn later_empty_is_ok_last_run_plus_one_unresolved_warning() {
        let e = env();
        activate(&e.learn_dir);
        std::fs::write(
            e.learn_dir.join("log.jsonl"),
            concat!(
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"ambient","cli":"claude","sessions":1,"candidates":0,"outcome":"failed","error_stage":"validate_output","error_code":"output_json_invalid","error":"SECRET_FORGED_ERROR"}"#,
                "\n",
                r#"{"ts":"2026-07-10T10:05:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"empty"}"#,
                "\n"
            ),
        )
        .unwrap();
        crate::learn::state::record_failure_at(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;

        let findings = check_learn_at_with_selection(
            &cfg,
            &e.learn_dir,
            &e.home,
            now(),
            &crate::learn::agent_cli::Selection::None,
        );
        let last = find(&findings, "last run:").expect("must show last run: {findings:?}");
        assert_eq!(last.status, Status::Ok);
        assert!(last.message.contains("empty"));
        let failure = find(&findings, "unresolved harvest failure")
            .expect("must preserve breaker reason: {findings:?}");
        assert_eq!(failure.status, Status::Warn);
        assert!(failure
            .message
            .contains("validate_output/output_json_invalid"));
        assert!(!failure.message.contains("SECRET_FORGED_ERROR"));
    }

    #[test]
    fn active_unsupported_selection_warns_without_log_history() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        cfg.learn.cli = Some("claude".into());
        let selection = crate::learn::agent_cli::Selection::Unsupported(
            crate::learn::agent_cli::UnsupportedCli {
                cli_id: "claude",
                installed_version: Some("2.1.210".into()),
                minimum_version: crate::learn::agent_cli::CLAUDE_MIN_VERSION,
                reason: crate::learn::agent_cli::UnsupportedReason::TooOld,
            },
        );

        let findings =
            check_learn_at_with_selection(&cfg, &e.learn_dir, &e.home, now(), &selection);
        let f = find(&findings, "current extraction CLI is unsupported")
            .expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f
            .message
            .contains("preflight/claude_structured_output_unsupported"));
        assert!(f.message.contains("Upgrade to 2.1.211 or newer"));
    }

    // --- corrupt watermarks -----------------------------------------------------

    #[test]
    fn corrupt_watermarks_are_flagged_loudly() {
        let e = env();
        std::fs::write(e.learn_dir.join("watermarks.json"), "{ not json").unwrap();
        let cfg = Config::defaults();
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let f = find(&findings, "watermark store is corrupt").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load learn reset"));
    }

    // --- 14-day no-trigger nudge -----------------------------------------------

    fn log_line(ts: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","trigger":"ambient","cli":"claude","model":"haiku","sessions":1,"candidates":1,"outcome":"extracted"}}"#
        )
    }

    #[test]
    fn stale_14_day_nudge_fires_when_enabled_and_last_run_is_old() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;

        let old_ts = "2020-01-01T00:00:00Z";
        let old_unix = chrono::DateTime::parse_from_rfc3339(old_ts)
            .unwrap()
            .timestamp();
        std::fs::write(e.learn_dir.join("log.jsonl"), log_line(old_ts) + "\n").unwrap();
        // 20 days after the last run.
        let now =
            SystemTime::UNIX_EPOCH + Duration::from_secs((old_unix + 20 * 24 * 60 * 60) as u64);

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now);
        let f =
            find(&findings, "no trigger has fired in 14+ days").expect("must warn: {findings:?}");
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains("load harvest") || f.message.contains("load studio"));
    }

    /// Enabled with no run ever logged also nudges (never ran is stale too).
    #[test]
    fn stale_14_day_nudge_fires_when_never_run() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;
        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        assert!(
            find(&findings, "no trigger has fired in 14+ days").is_some(),
            "never having run must also nudge: {findings:?}"
        );
    }

    /// A recent run inside the 14-day window must NOT nudge.
    #[test]
    fn no_stale_nudge_when_last_run_is_recent() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;

        let recent_ts = "2020-01-01T00:00:00Z";
        let recent_unix = chrono::DateTime::parse_from_rfc3339(recent_ts)
            .unwrap()
            .timestamp();
        std::fs::write(e.learn_dir.join("log.jsonl"), log_line(recent_ts) + "\n").unwrap();
        // Only 1 day later — well inside the 14-day window.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs((recent_unix + 24 * 60 * 60) as u64);

        let findings = check_learn_at(&cfg, &e.learn_dir, &e.home, now);
        assert!(
            find(&findings, "no trigger has fired in 14+ days").is_none(),
            "a recent run must not nudge: {findings:?}"
        );
        // And the last-run line reflects it.
        assert!(find(&findings, "last run:").is_some());
    }

    // --- doctor never triggers a harvest ---------------------------------------

    /// Running the check must be side-effect-free on the state dir: no new
    /// eligible-hint files, no log growth, no stamp writes. `check_learn_at`
    /// only reads; the real spawn seam (`trigger::maybe_spawn`) is never
    /// called from doctor at all — this snapshots the state dir before/after
    /// to prove it, rather than merely asserting by code inspection.
    #[test]
    fn check_learn_never_mutates_the_state_dir() {
        let e = env();
        activate(&e.learn_dir);
        let mut cfg = Config::defaults();
        cfg.learn.enabled = true;

        let snapshot = |dir: &Path| -> Vec<(std::path::PathBuf, u64)> {
            fn walk(dir: &Path, out: &mut Vec<(std::path::PathBuf, u64)>) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if let Ok(meta) = entry.metadata() {
                        out.push((path, meta.len()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(dir, &mut out);
            out.sort();
            out
        };

        let before = snapshot(&e.learn_dir);
        let _ = check_learn_at(&cfg, &e.learn_dir, &e.home, now());
        let after = snapshot(&e.learn_dir);
        assert_eq!(
            before, after,
            "check_learn_at must not write anything to the state dir"
        );
    }

    // --- eligibility_note format ---------------------------------------------

    /// Lock the note's concrete text for one branch each: eligible-now, and a
    /// waiting duration in the hours (and minutes) form.
    #[test]
    fn eligibility_note_formats_now_and_wait_branches() {
        use crate::learn::trigger::Eligibility;
        let eligible = Eligibility {
            scan_due: true,
            spend_due: true,
            hint: false,
            wait: Duration::ZERO,
        };
        assert_eq!(eligibility_note(&eligible), "eligible now");

        let waiting = Eligibility {
            scan_due: true,
            spend_due: false,
            hint: false,
            wait: Duration::from_secs(2 * 3600 + 5 * 60), // 2h05m
        };
        assert_eq!(eligibility_note(&waiting), "in ~2h5m");

        let waiting_minutes = Eligibility {
            scan_due: false,
            spend_due: false,
            hint: false,
            wait: Duration::from_secs(90),
        };
        assert_eq!(eligibility_note(&waiting_minutes), "in ~1m");
    }
}
