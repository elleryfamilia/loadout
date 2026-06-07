//! `rosita doctor` — diagnose environment, config, and generated state.

use std::path::Path;
use std::process::Command;

use super::{prepare, Runtime};
use crate::render::header;
use crate::writer::BLOCK_BEGIN;
use crate::{config, templates};

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn symbol(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
        }
    }
}

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

/// Entry point for `rosita doctor`.
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
            "no config files found; author fragments and profiles in ~/.config/rosita/config.toml (or run `rosita studio`)",
        );
    } else {
        for s in &prep.config.sources {
            c.line(Status::Ok, format!("loaded config: {}", s.display()));
        }
    }
    // Fragments/profiles authored in a repo layer (global-only mistake).
    check_repo_global_only(&mut c, &prep.repo_base);
    // Profiles referencing fragments that don't exist (e.g. a hand-deleted cap).
    check_dangling_fragment_refs(&mut c, &prep.config);
    // Allowlist/denylist consistency.
    check_env_policy(&mut c, &prep.config);
    // Private-data leak lint over public config layers.
    check_public_leaks(&mut c, &prep);

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
        check_gitignore(&mut c, &prep.repo_base);
    } else {
        c.line(
            Status::Ok,
            "not a git repo — non-repo mode (.gitignore not managed)",
        );
    }
    check_claude_marker(&mut c, &prep.repo_base);

    // Generated overlays freshness.
    println!(
        "\nGenerated overlays (context {})",
        crate::hash::short(&prep.context.compute_hash())
    );
    check_overlays(&mut c, &prep);

    print_summary(&c);
    Ok(())
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
        .prefix(".rosita-doctor-")
        .tempfile_in(dir)
        .is_ok()
}

fn check_env_policy(c: &mut Checks, cfg: &config::Config) {
    let deny: Vec<regex::Regex> = cfg
        .env
        .deny_name_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();
    let conflicting: Vec<&String> = cfg
        .env
        .allowlist
        .iter()
        .filter(|name| deny.iter().any(|re| re.is_match(name)))
        .collect();
    if conflicting.is_empty() {
        c.line(
            Status::Ok,
            format!(
                "env allowlist: {} name(s), denylist consistent",
                cfg.env.allowlist.len()
            ),
        );
    } else {
        c.line(
            Status::Warn,
            format!("env names allowlisted but denied (will be dropped): {conflicting:?}"),
        );
    }
}

/// Warn when a **public** config layer (`config.toml`) contains literals that
/// look machine-specific — IPv4 addresses, `*.domain.tld` globs, or
/// multi-label hostnames — which belong in the gitignored `local.toml`. Only
/// public layers are scanned; `local.toml` is the place for these.
fn check_public_leaks(c: &mut Checks, prep: &super::Prepared) {
    let mut scanned = 0usize;
    let mut flagged = 0usize;
    for src in &prep.config.sources {
        if src.file_name().and_then(|s| s.to_str()) != Some("config.toml") {
            continue; // local.toml is the private layer — never linted
        }
        let Ok(text) = std::fs::read_to_string(src) else {
            continue;
        };
        scanned += 1;
        for h in crate::lint::find_in_text(&text) {
            flagged += 1;
            c.line(
                Status::Warn,
                format!(
                    "{}: {h:?} looks private — move to local.toml",
                    src.display()
                ),
            );
        }
    }
    if scanned > 0 && flagged == 0 {
        c.line(Status::Ok, "public config has no private-looking literals");
    }
}

/// A profile that references a fragment id not in the library renders nothing
/// for that entry (compose silently skips it). Surface the dangling reference —
/// it usually means a fragment was hand-deleted without cleaning up the
/// profile (studio's delete does this cleanup automatically).
fn check_dangling_fragment_refs(c: &mut Checks, cfg: &config::Config) {
    let known: std::collections::HashSet<&str> =
        cfg.fragments.iter().map(|x| x.id.as_str()).collect();
    for p in &cfg.profiles {
        for r in &p.fragments {
            if !known.contains(r.id()) {
                c.line(
                    Status::Warn,
                    format!(
                        "profile '{}' references unknown fragment '{}' (it renders nothing — remove it or define the fragment)",
                        p.name,
                        r.id()
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
                    ".rosita/{label} declares {what} — these are global-only and are ignored here; move them to ~/.config/rosita/config.toml"
                ),
            );
        }
    }
}

/// What global-only tables (if any) a repo TOML file declares. `None` when the
/// file is absent, unparseable, or declares neither.
fn repo_declares_caps_or_profiles(path: &Path) -> Option<&'static str> {
    let text = std::fs::read_to_string(path).ok()?;
    let val: toml::Value = toml::from_str(&text).ok()?;
    let has = |k: &str| {
        val.get(k)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    };
    // Accept the pre-rename `[[capabilities]]` key alongside `[[fragments]]`.
    let has_fragments = has("fragments") || has("capabilities");
    match (has_fragments, has("profiles")) {
        (true, true) => Some("fragments and profiles"),
        (true, false) => Some("fragments"),
        (false, true) => Some("profiles"),
        (false, false) => None,
    }
}

fn check_gitignore(c: &mut Checks, repo_base: &Path) {
    let gi = std::fs::read_to_string(repo_base.join(".gitignore")).unwrap_or_default();
    if gi
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == ".rosita/generated")
    {
        c.line(Status::Ok, ".gitignore covers .rosita/generated/");
    } else {
        c.line(
            Status::Warn,
            ".gitignore missing .rosita/generated/ (render an agent to manage it)",
        );
    }
}

fn check_claude_marker(c: &mut Checks, repo_base: &Path) {
    let path = repo_base.join("CLAUDE.local.md");
    if !path.exists() {
        return; // nothing rendered for Claude yet; not a problem
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.contains(BLOCK_BEGIN) {
        c.line(Status::Ok, "CLAUDE.local.md has the managed import block");
    } else {
        c.line(
            Status::Warn,
            "CLAUDE.local.md exists but lacks the managed block (re-run render)",
        );
    }
}

fn check_overlays(c: &mut Checks, prep: &super::Prepared) {
    let dir = config::generated_dir(&prep.repo_base);
    let current = crate::render::overlay_fingerprint(&prep.context, &prep.composition);
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
                format!("{}: stale (run `rosita refresh`)", a.id),
            ),
            None => c.line(
                Status::Warn,
                format!("{}: present but missing rosita header", a.id),
            ),
        }
    }
    if !found {
        c.line(
            Status::Warn,
            "no overlays generated yet (run `rosita render`)",
        );
    }
}
