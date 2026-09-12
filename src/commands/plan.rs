//! `load plan` — a dispatcher over `artefacto plan`.
//!
//! artefacto owns the plan artifact: the `artefacto.plan/1` format, its
//! validation, the rendered page, and the live review (`push`, the served
//! page, the loop an agent runs). loadout keeps what is loadout's: the
//! well-known paths under `.loadout/`, the gitignore entries (the agent
//! writes plan.json before anything runs, and it must never be committable
//! by accident), the Recents registry, and `clean`. Every verb ensures the
//! gitignore entries, then runs `artefacto plan <verb>` with those paths
//! filled in. artefacto's exit codes reach the caller unchanged: an agent
//! branches on them.
//!
//! `LOADOUT_ARTEFACTO_BIN` names the binary to run; tests point it at a
//! stand-in.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use anyhow::{bail, Context as _};

use super::{prepare, Prepared, Runtime};
use crate::artefacto::{self, Presence};
use crate::cli::{PlanAction, PlanArgs};
use crate::style::Painter;
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
        Some(PlanAction::Render {
            file,
            out,
            no_open,
            json,
        }) => render(&prep, rt, file.as_deref(), out.as_deref(), *no_open, *json),
        Some(PlanAction::Push { args }) => push(&prep, rt, args),
        Some(PlanAction::Schema) => {
            require_artefacto()?;
            forward(&["schema".to_string()])
        }
        Some(PlanAction::Clean) => clean(&prep, rt),
    }
}

/// artefacto must answer before a verb runs. Missing, it is offered on a
/// terminal and named off one; either way the verb does not run without it.
fn require_artefacto() -> crate::Result<()> {
    match artefacto::probe() {
        Presence::Found { .. } => Ok(()),
        Presence::TimedOut => bail!(
            "artefacto is installed but `{} --version` did not answer within the probe deadline",
            artefacto::program()
        ),
        Presence::Missing => {
            if artefacto::offer_install(&Painter::auto())? {
                Ok(())
            } else {
                bail!("artefacto is not installed")
            }
        }
    }
}

/// `artefacto plan <args>` with the terminal's stdio. A non-zero exit ends
/// this process with the same code: 2, 4, 6, and 7 are artefacto's contract
/// with an agent, and a dispatcher that flattened them would break the loop.
fn forward(args: &[String]) -> crate::Result<()> {
    let status = artefacto::command()
        .arg("plan")
        .args(args)
        .status()
        .with_context(|| format!("running `{} plan`", artefacto::program()))?;
    exit_unless_ok(status)
}

fn exit_unless_ok(status: ExitStatus) -> crate::Result<()> {
    if status.success() {
        return Ok(());
    }
    std::process::exit(status.code().unwrap_or(1))
}

/// `artefacto plan <args>` with stdout captured, for a verb loadout reads
/// the result of. stderr stays the terminal's. On failure the captured
/// stdout is printed (an agent's JSON lives there) and the code is kept.
fn capture(args: &[String]) -> crate::Result<String> {
    let out = artefacto::command()
        .arg("plan")
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running `{} plan`", artefacto::program()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        print!("{stdout}");
        std::process::exit(out.status.code().unwrap_or(1));
    }
    Ok(stdout)
}

/// Like [`capture`], for a side question: `None` on any failure, nothing
/// printed anywhere.
fn capture_quiet(args: &[String]) -> Option<String> {
    let out = artefacto::command()
        .arg("plan")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

fn plan_file(prep: &Prepared, rt: &Runtime, file: Option<&Path>) -> PathBuf {
    file.map(|f| resolve_relative(&rt.cwd, f))
        .unwrap_or_else(|| plan_json_path(&prep.repo_base))
}

fn arg(p: &Path) -> String {
    p.display().to_string()
}

fn status(prep: &Prepared, rt: &Runtime) -> crate::Result<()> {
    let _ = rt;
    let json = plan_json_path(&prep.repo_base);
    if !json.exists() {
        println!(
            "no plan.json at {} — an agent with the artefacto-plan skill writes one",
            json.display()
        );
        return Ok(());
    }
    require_artefacto()?;
    let html = plan_html_path(&prep.repo_base);
    let raw = capture_quiet(&[
        "status".to_string(),
        arg(&json),
        "--out".to_string(),
        arg(&html),
        "--json".to_string(),
    ]);
    let Some(v) = raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()) else {
        println!("plan.json present but invalid — run `load plan check`");
        return Ok(());
    };
    let hash = v["plan_hash"].as_str().unwrap_or_default().to_string();
    println!(
        "plan '{}' — {} tasks, hash {}",
        v["title"].as_str().unwrap_or_default(),
        v["tasks"],
        crate::hash::short(&hash)
    );
    match v["state"].as_str().unwrap_or("none") {
        "fresh" => println!("render: fresh ({})", html.display()),
        "stale" => println!("render: STALE — run `load plan render`"),
        _ => println!("render: none — run `load plan render`"),
    }
    warn_stale_feedback(prep, &hash);
    Ok(())
}

fn check(
    prep: &Prepared,
    rt: &Runtime,
    file: Option<&Path>,
    json: bool,
    lenient: bool,
) -> crate::Result<()> {
    require_artefacto()?;
    let path = plan_file(prep, rt, file);
    let mut args = vec!["check".to_string(), arg(&path)];
    if json {
        args.push("--json".to_string());
    }
    if lenient {
        args.push("--lenient".to_string());
    }
    // The stale-feedback warning wants the plan's hash, which only the JSON
    // form carries: one quiet extra call, and only when there is a feedback
    // file to compare it with.
    if feedback_path(&prep.repo_base).exists() {
        let mut quiet = vec!["check".to_string(), arg(&path), "--json".to_string()];
        if lenient {
            quiet.push("--lenient".to_string());
        }
        let hash = capture_quiet(&quiet)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v["files"][0]["plan_hash"].as_str().map(str::to_string));
        if let Some(hash) = hash {
            warn_stale_feedback(prep, &hash);
        }
    }
    forward(&args)
}

/// Loud stderr warning when plan-feedback.json was written against another
/// plan than the one with `plan_hash`.
fn warn_stale_feedback(prep: &Prepared, plan_hash: &str) {
    let path = feedback_path(&prep.repo_base);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let fhash = v.get("plan_hash").and_then(|x| x.as_str()).unwrap_or("");
    if fhash != plan_hash {
        crate::warn_user!(
            "plan-feedback.json targets plan hash {fhash}; the current plan is {plan_hash} — feedback may be stale"
        );
    }
}

fn render(
    prep: &Prepared,
    rt: &Runtime,
    file: Option<&Path>,
    out: Option<&Path>,
    no_open: bool,
    json: bool,
) -> crate::Result<()> {
    require_artefacto()?;
    let path = plan_file(prep, rt, file);
    let target = out
        .map(|o| resolve_relative(&rt.cwd, o))
        .unwrap_or_else(|| plan_html_path(&prep.repo_base));
    if rt.dry_run {
        println!(
            "would run: {} plan render {} --out {}",
            artefacto::program(),
            path.display(),
            target.display()
        );
        return Ok(());
    }
    // `--json` makes artefacto print its result instead of opening a
    // browser; loadout opens the page itself, or prints the result.
    let raw = capture(&[
        "render".to_string(),
        arg(&path),
        "--out".to_string(),
        arg(&target),
        "--json".to_string(),
    ])?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).context("reading artefacto's render result")?;
    let hash = v["plan_hash"].as_str().unwrap_or_default().to_string();
    if json {
        print!("{raw}");
    } else {
        for w in v["warnings"].as_array().into_iter().flatten() {
            println!(
                "warning[{}] {}: {}",
                w["code"].as_str().unwrap_or_default(),
                w["path"].as_str().unwrap_or_default(),
                w["message"].as_str().unwrap_or_default()
            );
        }
        println!(
            "rendered {} → {}",
            v["title"].as_str().unwrap_or_default(),
            target.display()
        );
        warn_stale_feedback(prep, &hash);
        if !no_open {
            crate::studio::server::open_browser(&file_url(&target));
            println!("opened in your browser (pass --no-open to skip)");
        }
    }
    // Record in the per-machine recents registry — canonical renders only
    // (default input AND default output): a --out/FILE render pairs a
    // non-canonical plan or scratch path with no clean verb or staleness
    // story, and would sit as a permanent dead row (no-prune rule).
    if file.is_none() && out.is_none() {
        record_render(&prep.repo_base, &v, &target, json);
    }
    Ok(())
}

/// `push`: the plan file is the first argument when there is one that does
/// not start with `-`; loadout's default otherwise. Everything else goes to
/// artefacto as it is, and its output and exit code come back as they are.
fn push(prep: &Prepared, rt: &Runtime, args: &[String]) -> crate::Result<()> {
    require_artefacto()?;
    let (file, rest) = match args.first() {
        Some(first) if !first.starts_with('-') => {
            (resolve_relative(&rt.cwd, Path::new(first)), &args[1..])
        }
        _ => (plan_json_path(&prep.repo_base), args),
    };
    if rt.dry_run {
        println!(
            "would run: {} plan push {} {}",
            artefacto::program(),
            file.display(),
            rest.join(" ")
        );
        return Ok(());
    }
    let mut argv = vec!["push".to_string(), arg(&file)];
    argv.extend(rest.iter().cloned());
    forward(&argv)
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

/// Remove the rendered plan.html (marker-gated: loadout's own marker or
/// artefacto's) and plan-feedback.json. Never touches plan.json (the agent's
/// input) or anything unmarked.
pub(crate) fn clean_artifacts(repo_base: &Path, dry_run: bool) -> crate::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let html = plan_html_path(repo_base);
    match std::fs::read_to_string(&html) {
        Ok(content) => {
            if crate::render::header::is_plan_page_marker(&content) {
                if !dry_run {
                    std::fs::remove_file(&html)
                        .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", html.display()))?;
                }
                removed.push(html);
            } else {
                println!("  skipping {} (not a generated plan page)", html.display());
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

/// Best-effort recents recording from artefacto's render result. A registry
/// failure must never fail the render; messaging keys off the outcome so we
/// never advertise an entry that wasn't written, and says nothing at all
/// when the caller asked for JSON.
fn record_render(repo_base: &Path, result: &serde_json::Value, html_path: &Path, quiet: bool) {
    use crate::recents::{clamp_title, Entry, RecentsStore, RecordOutcome};
    let mut detail = std::collections::BTreeMap::new();
    detail.insert("phases".to_string(), result["phases"].clone());
    detail.insert("tasks".to_string(), result["tasks"].clone());
    let entry = Entry {
        kind: "plan".to_string(),
        path: html_path.to_path_buf(), // record() absolutizes
        repo: std::path::absolute(repo_base).unwrap_or_else(|_| repo_base.to_path_buf()),
        title: clamp_title(result["title"].as_str().unwrap_or_default()),
        hash: result["plan_hash"].as_str().unwrap_or_default().to_string(),
        rendered_at: super::now_rfc3339(),
        detail,
        extra: std::collections::BTreeMap::new(),
    };
    let mut store = RecentsStore::load_default();
    match store.record(entry) {
        RecordOutcome::Recorded => {
            if !quiet {
                println!("(also available under Recents in `load studio`)");
            }
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
