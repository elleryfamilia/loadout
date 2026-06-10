//! `rosita skill` — install, remove, and inspect the skills rosita ships.
//!
//! The sole lifecycle path for embedded skills (see [`crate::skills`]):
//! `clean` stays repo-scoped and never touches them, and the ask-once prompt
//! in `rosita run` is just a frontend over `install`. Installing records an
//! `accepted` decision; removing records `declined` so `run` won't re-offer —
//! `rosita skill install` is the re-enable path.

use anyhow::anyhow;

use super::Runtime;
use crate::binding::{self, SkillDecision};
use crate::cli::{SkillAction, SkillArgs};
use crate::config;
use crate::skills::{self, InstallAction, LinkState, Skill, SkillState};
use crate::style::Painter;

/// Entry point for `rosita skill`.
pub fn run(rt: &Runtime, args: &SkillArgs) -> crate::Result<()> {
    match args.action.as_ref().unwrap_or(&SkillAction::Status) {
        SkillAction::Status => status(),
        SkillAction::Install { id } => install(rt, id.as_deref()),
        SkillAction::Remove { id } => remove(rt, id.as_deref()),
    }
}

/// Resolve `id` to one shipped skill, or all of them when omitted.
fn targets(id: Option<&str>) -> crate::Result<Vec<&'static Skill>> {
    match id {
        None => Ok(skills::all().iter().collect()),
        Some(id) => skills::by_id(id).map(|s| vec![s]).ok_or_else(|| {
            anyhow!(
                "unknown skill '{id}' — shipped skills: {}",
                skills::all()
                    .iter()
                    .map(|s| s.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
    }
}

fn home() -> crate::Result<std::path::PathBuf> {
    config::home_dir().ok_or_else(|| anyhow!("cannot resolve $HOME"))
}

/// `rosita skill install [id]`.
fn install(rt: &Runtime, id: Option<&str>) -> crate::Result<()> {
    let p = Painter::auto();
    let home = home()?;
    for skill in targets(id)? {
        if rt.dry_run {
            describe_plan(&p, &home, skill);
            continue;
        }
        let actions = skills::install(&home, skill)?;
        binding::write_skill_decision(skill.id, SkillDecision::Accepted)?;
        report_actions(&p, skill, &actions);
    }
    Ok(())
}

/// `rosita skill remove [id]`.
fn remove(rt: &Runtime, id: Option<&str>) -> crate::Result<()> {
    let p = Painter::auto();
    let home = home()?;
    for skill in targets(id)? {
        if rt.dry_run {
            println!(
                "  {} (dry-run) would remove {} and its agent links",
                p.cyan("→"),
                skills::canonical_dir(&home, skill.id).display()
            );
            continue;
        }
        let removed = skills::remove(&home, skill)?;
        // Remember the opt-out so `rosita run` doesn't immediately re-offer it.
        binding::write_skill_decision(skill.id, SkillDecision::Declined)?;
        if removed.is_empty() {
            println!("  {} {} was not installed", p.dim("·"), p.bold(skill.id));
        } else {
            for path in removed {
                println!("  {} removed {}", p.green("✓"), path.display());
            }
            println!(
                "    {}",
                p.dim("re-enable any time with `rosita skill install`")
            );
        }
    }
    Ok(())
}

/// `rosita skill [status]`.
fn status() -> crate::Result<()> {
    let p = Painter::auto();
    let home = home()?;
    for skill in skills::all() {
        let st = skills::status(&home, skill);
        let decision = match binding::read_skill_decision(skill.id) {
            Some(SkillDecision::Accepted) => "accepted",
            Some(SkillDecision::Declined) => "declined",
            None => "undecided",
        };
        let state = match &st.state {
            SkillState::NotInstalled => p.dim("not installed"),
            SkillState::Unmanaged => p.yellow("present but not rosita-managed"),
            SkillState::Managed {
                user_modified: true,
                ..
            } => p.yellow("installed, edited by you (auto-upgrade off)"),
            SkillState::Managed {
                upgrade_available: true,
                ..
            } => p.cyan("installed, upgrade available (`rosita skill install`)"),
            SkillState::Managed { .. } => p.green("installed, current"),
        };
        println!(
            "  {} — {state} {}",
            p.bold(skill.id),
            p.dim(&format!("(decision: {decision})"))
        );
        println!(
            "    {}",
            p.dim(&format!(
                "canonical: {}",
                skills::canonical_dir(&home, skill.id).display()
            ))
        );
        if st.state == SkillState::NotInstalled {
            continue; // no install → per-agent link detail is just noise
        }
        for link in &st.links {
            let what = match link.state {
                LinkState::AgentAbsent => continue, // agent not installed; nothing expected
                LinkState::Linked => p.green("linked"),
                LinkState::Missing => p.yellow("missing (`rosita skill install` repairs)"),
                LinkState::Dangling => p.yellow("dangling (`rosita skill install` repairs)"),
                LinkState::CopyManaged => p.cyan("copy (symlink fallback)"),
                LinkState::Foreign => p.yellow("occupied by a file rosita didn't create"),
            };
            println!("    {}", p.dim(&format!("{}: {what}", link.path.display())));
        }
    }
    Ok(())
}

fn describe_plan(p: &Painter, home: &std::path::Path, skill: &Skill) {
    let st = skills::status(home, skill);
    let verb = match st.state {
        SkillState::NotInstalled => "install",
        SkillState::Managed {
            user_modified: false,
            upgrade_available: true,
            ..
        } => "upgrade",
        _ => "leave as-is",
    };
    println!(
        "  {} (dry-run) would {verb} {} at {}",
        p.cyan("→"),
        p.bold(skill.id),
        skills::canonical_dir(home, skill.id).display()
    );
}

fn report_actions(p: &Painter, skill: &Skill, actions: &[InstallAction]) {
    for a in actions {
        match a {
            InstallAction::WroteCanonical(path) => {
                println!("  {} installed {} → {}", p.green("✓"), p.bold(skill.id), path.display());
            }
            InstallAction::CanonicalCurrent(path) => {
                println!(
                    "  {} {} already current at {}",
                    p.green("✓"),
                    p.bold(skill.id),
                    path.display()
                );
            }
            InstallAction::SkippedModified(path) => {
                println!(
                    "  {} {} at {} has local edits — left untouched",
                    p.yellow("⚠"),
                    p.bold(skill.id),
                    path.display()
                );
            }
            InstallAction::Linked(path) => {
                println!("    {}", p.dim(&format!("linked {}", path.display())));
            }
            InstallAction::Copied(path) => {
                println!(
                    "    {}",
                    p.dim(&format!("copied to {} (symlink unavailable)", path.display()))
                );
            }
            InstallAction::SkippedForeign(path) => {
                println!(
                    "  {} {} exists and isn't rosita's — left untouched",
                    p.yellow("⚠"),
                    path.display()
                );
            }
        }
    }
}
