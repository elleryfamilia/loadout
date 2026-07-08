//! `load plan` — validate, render, and review agent-emitted development plans.
//!
//! Every verb first ensures the plan gitignore entries (the agent writes
//! plan.json before any render runs, and this must never be committable by
//! accident), then does its work. See design-plan-visualizer.md.

use std::path::{Path, PathBuf};

use anyhow::bail;

use super::{prepare, Prepared, Runtime};
use crate::cli::{PlanAction, PlanArgs};
use crate::plan::model;
use crate::workflow::artifacts_dir;
use crate::writer::{ensure_line, AtomicWriter, Writer as _};

pub(crate) const GITIGNORE_ENTRIES: &[&str] = &[
    ".loadout/workflow/artifacts/plan.json",
    ".loadout/workflow/artifacts/plan-feedback.json",
    ".loadout/generated/",
];

pub(crate) fn plan_json_path(repo_base: &Path) -> PathBuf {
    artifacts_dir(repo_base).join("plan.json")
}
pub(crate) fn plan_html_path(repo_base: &Path) -> PathBuf {
    crate::config::generated_dir(repo_base).join("plan.html")
}
pub(crate) fn feedback_path(repo_base: &Path) -> PathBuf {
    artifacts_dir(repo_base).join("plan-feedback.json")
}

/// Ensure the exact-file gitignore entries. Only inside a git repo; a
/// non-repo directory has nothing to protect against `git add`.
pub(crate) fn ensure_plan_gitignore(prep: &Prepared, writer: &AtomicWriter) -> crate::Result<()> {
    if prep.context.git.is_none() {
        return Ok(());
    }
    let gitignore = prep.repo_base.join(".gitignore");
    let mut content = std::fs::read_to_string(&gitignore).ok();
    let mut changed = false;
    for entry in GITIGNORE_ENTRIES {
        if let Some(updated) = ensure_line(content.as_deref(), entry) {
            content = Some(updated);
            changed = true;
        }
    }
    if changed {
        if let Some(c) = content {
            writer.write(&gitignore, &c)?;
        }
    }
    Ok(())
}

/// Entry point for `load plan`.
pub fn run(rt: &Runtime, args: &PlanArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let writer = AtomicWriter::new(rt.dry_run);
    ensure_plan_gitignore(&prep, &writer)?;
    match args.action.as_ref() {
        None => status(&prep),
        Some(PlanAction::Check {
            file,
            json,
            lenient,
        }) => check(&prep, file.as_deref(), *json, *lenient),
        Some(PlanAction::Render { .. }) => bail!("not implemented yet"),
        Some(PlanAction::Schema) => bail!("not implemented yet"),
        Some(PlanAction::Clean) => bail!("not implemented yet"),
    }
}

/// Load + parse + validate; returns the plan or prints diagnostics and errs.
fn load_checked(
    prep: &Prepared,
    file: Option<&Path>,
    json: bool,
    lenient: bool,
) -> crate::Result<(model::Plan, Vec<model::Issue>)> {
    let path = file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| plan_json_path(&prep.repo_base));
    let input = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let (plan, warnings, errors) = match model::parse(&input, lenient) {
        Ok(p) => {
            let errs = model::validate(&p.plan);
            if errs.is_empty() {
                (Some(p.plan), p.warnings, vec![])
            } else {
                (None, p.warnings, errs)
            }
        }
        Err(errs) => (None, vec![], errs),
    };
    if !errors.is_empty() {
        report_issues(json, &errors, &warnings);
        bail!("plan.json has {} error(s)", errors.len());
    }
    Ok((plan.expect("no errors means a plan"), warnings))
}

fn report_issues(json: bool, errors: &[model::Issue], warnings: &[model::Issue]) {
    if json {
        let doc = serde_json::json!({
            "ok": errors.is_empty(), "errors": errors, "warnings": warnings });
        println!("{}", serde_json::to_string(&doc).expect("issues serialize"));
    } else {
        for e in errors {
            println!("error[{}] {}: {}", e.code, e.path, e.message);
        }
        for w in warnings {
            println!("warning[{}] {}: {}", w.code, w.path, w.message);
        }
    }
}

fn check(prep: &Prepared, file: Option<&Path>, json: bool, lenient: bool) -> crate::Result<()> {
    let (plan, warnings) = load_checked(prep, file, json, lenient)?;
    warn_stale_feedback(prep, &plan);
    if json {
        report_issues(true, &[], &warnings);
    } else {
        for w in &warnings {
            println!("warning[{}] {}: {}", w.code, w.path, w.message);
        }
        println!(
            "plan.json is valid ({} tasks, hash {})",
            plan.phases.iter().map(|p| p.tasks.len()).sum::<usize>(),
            crate::hash::short(&model::plan_hash(&plan))
        );
    }
    Ok(())
}

/// Loud stderr warning when plan-feedback.json targets a different plan.
fn warn_stale_feedback(prep: &Prepared, plan: &model::Plan) {
    let path = feedback_path(&prep.repo_base);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let fid = v.get("plan_id").and_then(|x| x.as_str()).unwrap_or("");
    let fhash = v.get("plan_hash").and_then(|x| x.as_str()).unwrap_or("");
    let hash = model::plan_hash(plan);
    if fid != plan.meta.id || fhash != hash {
        crate::warn_user!(
            "plan-feedback.json targets plan '{fid}' ({fhash}); current plan is '{}' ({hash}) — feedback may be stale",
            plan.meta.id
        );
    }
}

fn status(prep: &Prepared) -> crate::Result<()> {
    let json = plan_json_path(&prep.repo_base);
    if !json.exists() {
        println!(
            "no plan.json at {} — an agent with the loadout-plan-preview skill writes one",
            json.display()
        );
        return Ok(());
    }
    match load_checked(prep, None, false, true) {
        Ok((plan, _)) => {
            let hash = model::plan_hash(&plan);
            println!(
                "plan '{}' — {} tasks, hash {}",
                plan.meta.id,
                plan.phases.iter().map(|p| p.tasks.len()).sum::<usize>(),
                crate::hash::short(&hash)
            );
            let html = plan_html_path(&prep.repo_base);
            match std::fs::read_to_string(&html)
                .ok()
                .and_then(|c| crate::render::header::extract_context_hash(&c))
            {
                Some(h) if h == hash => println!("render: fresh ({})", html.display()),
                Some(_) => println!("render: STALE — run `load plan render`"),
                None => println!("render: none — run `load plan render`"),
            }
            warn_stale_feedback(prep, &plan);
        }
        Err(e) => println!("plan.json present but invalid: {e:#} — run `load plan check`"),
    }
    Ok(())
}
