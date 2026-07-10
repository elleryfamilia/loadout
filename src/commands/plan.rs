//! `load plan` — validate, render, and review agent-emitted development plans.
//!
//! Every verb first ensures the plan gitignore entries (the agent writes
//! plan.json before any render runs, and this must never be committable by
//! accident), then does its work. See design-plan-visualizer.md.

use std::fmt::Write as _;
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

/// Resolve a user-supplied path against the directory loadout was invoked
/// from (`Runtime.cwd` — the explicit `--cwd` value, else the process's real
/// OS working directory), matching the universal CLI convention that a
/// relative path resolves against the invocation directory. This is
/// deliberately NOT the repo base: from `repo/docs/`, a relative `--out
/// preview.html` must land in `docs/`, not silently jump to the repo root.
/// Every other plan artifact (plan.json, plan.html, plan-feedback.json) is
/// its own separate, always-repo-base-anchored path — those are internal
/// canonical locations, not user-supplied ones. Absolute paths pass through
/// untouched.
pub(crate) fn resolve_relative(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
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
        None => status(&prep, rt),
        Some(PlanAction::Check {
            file,
            json,
            lenient,
        }) => check(&prep, rt, file.as_deref(), *json, *lenient),
        Some(PlanAction::Render { file, out, no_open }) => {
            render(&prep, rt, file.as_deref(), out.as_deref(), *no_open)
        }
        Some(PlanAction::Schema) => {
            let skill = crate::skills::by_id("loadout-plan-preview").expect("shipped skill");
            let reference = skill
                .files
                .iter()
                .find(|f| f.relpath == "reference.md")
                .expect("skill ships reference.md");
            println!("{}", reference.content);
            Ok(())
        }
        Some(PlanAction::Clean) => clean(&prep, rt),
    }
}

fn clean(prep: &Prepared, rt: &Runtime) -> crate::Result<()> {
    let removed = clean_artifacts(&prep.repo_base, rt.dry_run)?;
    if removed.is_empty() {
        println!("  (no plan artifacts)");
    } else {
        for p in &removed {
            println!(
                "  {:<10} {}",
                if rt.dry_run { "would rm" } else { "removed" },
                p.display()
            );
        }
    }
    // Drop the recents entry unconditionally on a non-dry-run clean: whether
    // the file was removed, skipped as not-ours, or already absent, the
    // entry about it is dead. Best-effort.
    if !rt.dry_run {
        let mut store = crate::recents::RecentsStore::load_default();
        if let Err(e) = store.remove_path(&plan_html_path(&prep.repo_base)) {
            crate::vlog!("could not update recents: {e}");
        }
    }
    Ok(())
}

/// Remove the rendered plan.html (marker-gated) and plan-feedback.json.
/// Never touches plan.json (the agent's input) or anything unmarked.
pub(crate) fn clean_artifacts(repo_base: &Path, dry_run: bool) -> crate::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let html = plan_html_path(repo_base);
    match std::fs::read_to_string(&html) {
        Ok(content) => {
            if content.starts_with(crate::render::header::GENERATED_MARKER) {
                if !dry_run {
                    std::fs::remove_file(&html)
                        .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", html.display()))?;
                }
                removed.push(html);
            } else {
                println!("  skipping {} (not loadout-generated)", html.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail!("cannot read {}: {e}", html.display()),
    }
    let fb = feedback_path(repo_base);
    if fb.exists() {
        if !dry_run {
            std::fs::remove_file(&fb)
                .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", fb.display()))?;
        }
        removed.push(fb);
    }
    Ok(removed)
}

/// Load + parse + validate; returns the plan or prints diagnostics and errs.
fn load_checked(
    prep: &Prepared,
    cwd: &Path,
    file: Option<&Path>,
    json: bool,
    lenient: bool,
) -> crate::Result<(model::Plan, Vec<model::Issue>)> {
    let path = file
        .map(|f| resolve_relative(cwd, f))
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

fn check(
    prep: &Prepared,
    rt: &Runtime,
    file: Option<&Path>,
    json: bool,
    lenient: bool,
) -> crate::Result<()> {
    let (plan, mut warnings) = load_checked(prep, &rt.cwd, file, json, lenient)?;
    warnings.extend(model::advisories(&plan));
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
        // Structure at a glance, so the author can sanity-check the shape
        // without opening the render.
        if !plan.phases.is_empty() {
            let breakdown = plan
                .phases
                .iter()
                .map(|p| format!("{} {}", p.title, p.tasks.len()))
                .collect::<Vec<_>>()
                .join(" · ");
            println!("  {} phases: {breakdown}", plan.phases.len());
        }
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

fn status(prep: &Prepared, rt: &Runtime) -> crate::Result<()> {
    let json = plan_json_path(&prep.repo_base);
    if !json.exists() {
        println!(
            "no plan.json at {} — an agent with the loadout-plan-preview skill writes one",
            json.display()
        );
        return Ok(());
    }
    match load_checked(prep, &rt.cwd, None, false, true) {
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

fn render(
    prep: &Prepared,
    rt: &Runtime,
    file: Option<&Path>,
    out: Option<&Path>,
    no_open: bool,
) -> crate::Result<()> {
    let (plan, warnings) = load_checked(prep, &rt.cwd, file, false, false)?;
    for w in &warnings {
        println!("warning[{}] {}: {}", w.code, w.path, w.message);
    }
    warn_stale_feedback(prep, &plan);
    let html = crate::plan::render::render(&plan);
    let path = out
        .map(|o| resolve_relative(&rt.cwd, o))
        .unwrap_or_else(|| plan_html_path(&prep.repo_base));
    let written = AtomicWriter::new(rt.dry_run).write(&path, &html)?;
    println!(
        "rendered {} ({}) → {}",
        plan.meta.id,
        written.action.label(),
        path.display()
    );
    if !no_open && !rt.dry_run {
        crate::studio::server::open_browser(&file_url(&path));
        println!("opened in your browser (pass --no-open to skip)");
    }
    // Record in the per-machine recents registry — canonical renders only
    // (default input AND default output): a --out/FILE render pairs a
    // non-canonical plan or scratch path with no clean verb or staleness
    // story, and would sit as a permanent dead row (no-prune rule).
    if !rt.dry_run && file.is_none() && out.is_none() {
        record_render(&prep.repo_base, &plan, &path);
    }
    Ok(())
}

/// Best-effort recents recording. A registry failure must never fail the
/// render; messaging keys off the outcome so we never advertise an entry
/// that wasn't written.
fn record_render(repo_base: &Path, plan: &model::Plan, html_path: &Path) {
    use crate::recents::{clamp_title, Entry, RecentsStore, RecordOutcome};
    let mut detail = std::collections::BTreeMap::new();
    detail.insert(
        "plan_id".to_string(),
        serde_json::Value::from(plan.meta.id.clone()),
    );
    detail.insert(
        "phases".to_string(),
        serde_json::Value::from(plan.phases.len()),
    );
    detail.insert(
        "tasks".to_string(),
        serde_json::Value::from(plan.phases.iter().map(|p| p.tasks.len()).sum::<usize>()),
    );
    let entry = Entry {
        kind: "plan".to_string(),
        path: html_path.to_path_buf(), // record() absolutizes
        repo: std::path::absolute(repo_base).unwrap_or_else(|_| repo_base.to_path_buf()),
        title: clamp_title(&plan.meta.title),
        hash: model::plan_hash(plan),
        rendered_at: super::now_rfc3339(),
        detail,
        extra: std::collections::BTreeMap::new(),
    };
    let mut store = RecentsStore::load_default();
    match store.record(entry) {
        RecordOutcome::Recorded => {
            println!("(also available under Recents in `load studio`)");
        }
        RecordOutcome::ReadOnlyNewer => crate::warn_user!(
            "recents state was written by a newer loadout — this render won't appear in studio Recents until you upgrade"
        ),
        RecordOutcome::NoStateDir => {
            crate::vlog!("no state dir; skipping recents record");
        }
        RecordOutcome::Failed(e) => {
            crate::vlog!("could not record recents entry: {e}");
        }
    }
}

/// Build a `file://` URL for `path`: absolutize it first (so a relative
/// `--out` still yields a URL a browser can open — a bare `file://custom/out`
/// treats `custom` as a host, not a path) and percent-encode every byte of
/// its UTF-8 (lossy) form except the unreserved characters and `/`, so paths
/// with spaces or other reserved characters don't produce a broken URL.
/// Dependency-free: no `url`/`percent-encoding` crate.
pub(crate) fn file_url(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = String::from("file://");
    for byte in absolute.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => {
                write!(out, "%{byte:02X}").expect("writing to a String never fails");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_percent_encodes_spaces() {
        let url = file_url(Path::new("/tmp/my plan/plan.html"));
        assert!(
            url.contains("/my%20plan/"),
            "expected %20-encoded space, got {url}"
        );
        assert!(
            !url.contains(' '),
            "url must not contain a raw space: {url}"
        );
    }

    #[test]
    fn file_url_absolutizes_relative_paths() {
        let url = file_url(Path::new("custom-out/plan.html"));
        assert!(
            url.starts_with("file:///"),
            "relative path must be absolutized before the file:// URL is built, got {url}"
        );
        assert!(url.ends_with("custom-out/plan.html"));
    }

    #[test]
    fn file_url_passes_through_plain_absolute_path() {
        let url = file_url(Path::new("/tmp/plan.html"));
        assert_eq!(url, "file:///tmp/plan.html");
    }
}
