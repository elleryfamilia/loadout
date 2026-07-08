//! `load doctor` — diagnose environment, config, and generated state.

use std::path::Path;
use std::process::Command;

use super::checks::{self, Status};
use super::{prepare, Runtime};
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
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => {
            c.line(
                Status::Ok,
                format!("git: {}", String::from_utf8_lossy(&o.stdout).trim()),
            );
        }
        _ => c.line(
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
    // Script fragments whose output loadout would silently drop (non-zero exit).
    check_script_dropouts(&mut c, &prep);

    // Agents + their launch CLIs.
    println!("\nAgents ({} configured)", prep.config.agents.len());
    for a in &prep.config.agents {
        match &a.launch {
            Some(prog) if on_path(prog) => {
                c.line(Status::Ok, format!("{}: CLI '{prog}' found", a.id))
            }
            Some(prog) => c.line(
                Status::Warn,
                format!(
                    "{}: CLI '{prog}' not on PATH (needed for `run {}`)",
                    a.id, a.id
                ),
            ),
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

fn on_path(program: &str) -> bool {
    // `command -v` is portable across the shells we target.
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty())
        .unwrap_or(false)
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
