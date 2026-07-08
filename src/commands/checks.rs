//! Pure config health checks shared by `load doctor` (verbose, every check)
//! and `load refresh` (fast subset, warnings only).
//!
//! Eligibility for this module: a check reads only the loaded `Config` and
//! repo-local files — no script execution, no network, no `$HOME` state.
//! Checks return findings instead of printing so each caller controls its own
//! output (doctor prints `Ok` lines too; refresh stays silent when healthy).

use std::path::Path;

use crate::config;

/// Severity of one finding (doctor also prints `Ok` lines; refresh never does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
        }
    }
}

/// One check outcome line.
#[derive(Debug)]
pub struct Finding {
    pub status: Status,
    pub message: String,
}

impl Finding {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            message: message.into(),
        }
    }
    pub fn warn(message: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            message: message.into(),
        }
    }
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            message: message.into(),
        }
    }
}

/// Scan the RAW text of every contributing config source file for
/// secret-looking strings, reporting file path + line so the user removes the
/// secret at its origin. Covers `local.toml` too — a secret doesn't belong in
/// any config layer (render-time redaction is the backstop, not a licence).
pub fn secret_leaks(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut scanned = 0usize;
    for src in &cfg.sources {
        let Ok(text) = std::fs::read_to_string(src) else {
            continue;
        };
        scanned += 1;
        let mut line_hits = 0usize;
        for (i, line) in text.lines().enumerate() {
            if crate::redact::looks_secret(line) {
                line_hits += 1;
                findings.push(Finding::warn(format!(
                    "{}:{}: looks like a secret — remove it from config and rotate the credential",
                    src.display(),
                    i + 1,
                )));
            }
        }
        // Multi-line secrets (PEM private-key blocks) never match a single
        // line — fall back to a whole-file scan for those.
        if line_hits == 0 && crate::redact::looks_secret(&text) {
            findings.push(Finding::warn(format!(
                "{}: looks like it contains a multi-line secret (e.g. a private key) — remove it from config and rotate the credential",
                src.display(),
            )));
        }
    }
    if findings.is_empty() {
        findings.push(Finding::ok(format!(
            "no secret-looking strings in {scanned} config file(s)"
        )));
    }
    findings
}

/// Allowlist/denylist consistency: an env name that is both allowlisted and
/// matched by a deny pattern will be dropped at render time regardless — flag
/// the contradiction so the user fixes the allowlist or the pattern.
pub fn env_policy(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
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
        findings.push(Finding::ok(format!(
            "env allowlist: {} name(s), denylist consistent",
            cfg.env.allowlist.len()
        )));
    } else {
        findings.push(Finding::warn(format!(
            "env names allowlisted but denied (will be dropped): {conflicting:?}"
        )));
    }
    findings
}

/// Warn when a **public** config layer (`config.toml`) contains literals that
/// look machine-specific — IPv4 addresses, `*.domain.tld` globs, or
/// multi-label hostnames — which belong in the gitignored `local.toml`. Only
/// public layers are scanned; `local.toml` is the place for these.
pub fn public_leaks(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut scanned = 0usize;
    let mut flagged = 0usize;
    for src in &cfg.sources {
        if src.file_name().and_then(|s| s.to_str()) != Some("config.toml") {
            continue; // local.toml is the private layer — never linted
        }
        let Ok(text) = std::fs::read_to_string(src) else {
            continue;
        };
        scanned += 1;
        for h in crate::lint::find_in_text(&text) {
            flagged += 1;
            findings.push(Finding::warn(format!(
                "{}: {h:?} looks private — move to local.toml",
                src.display()
            )));
        }
    }
    if scanned > 0 && flagged == 0 {
        findings.push(Finding::ok("public config has no private-looking literals"));
    }
    findings
}

/// A profile that references a fragment id not in the library renders nothing
/// for that entry (compose silently skips it). Surface the dangling reference —
/// it usually means a fragment was hand-deleted without cleaning up the
/// profile (studio's delete does this cleanup automatically).
pub fn dangling_fragment_refs(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    let known: std::collections::HashSet<&str> =
        cfg.fragments.iter().map(|x| x.id.as_str()).collect();
    for p in &cfg.profiles {
        for r in &p.fragments {
            if !known.contains(r.id()) {
                findings.push(Finding::warn(format!(
                    "loadout '{}' references unknown fragment '{}' (it renders nothing — remove it or define the fragment)",
                    p.name,
                    r.id()
                )));
            }
        }
    }
    findings
}

/// The single-default invariant: exactly one enabled loadout with no targets is
/// the catch-all that applies when nothing else matches (in any project or none).
/// Zero ⇒ unmatched contexts get no loadout; more than one ⇒ ambiguous.
pub fn default_loadout(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    let defaults: Vec<&str> = cfg
        .profiles
        .iter()
        .filter(|p| !p.disabled && p.targets.is_empty())
        .map(|p| p.name.as_str())
        .collect();
    match defaults.len() {
        1 => findings.push(Finding::ok(format!(
            "default loadout: '{}' (applies everywhere nothing else matches)",
            defaults[0]
        ))),
        0 => findings.push(Finding::warn(
            "no default loadout — nothing applies in a project that matches no loadout (or outside a project). In `load studio`, clear a loadout's targets to make it the default.",
        )),
        _ => findings.push(Finding::warn(format!(
            "{} default loadouts ({}) — only one loadout should have no targets; give the others a target so selection isn't ambiguous",
            defaults.len(),
            defaults.join(", ")
        ))),
    }
    findings
}

/// Profiles that bind a workflow id resolving to nothing (a typo or a deleted
/// `[[workflows]]` entry), plus any malformed user-authored workflow. A dangling
/// binding degrades silently at render time, so surface it here. Resolves
/// against the same built-in + user catalog the renderer uses.
pub fn workflows(cfg: &config::Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    // A workflow is bound per-loadout (the Workflow slot) — there's no global
    // default workflow anymore. Flag any loadout whose binding doesn't resolve.
    for p in &cfg.profiles {
        let Some(id) = &p.workflow else { continue };
        if cfg.resolve_workflow(id).is_some() {
            findings.push(Finding::ok(format!(
                "loadout '{}' → workflow '{id}'",
                p.name
            )));
        } else {
            findings.push(Finding::warn(format!(
                "loadout '{}' binds unknown workflow '{id}' (it won't apply — define it under [[workflows]] or fix the id)",
                p.name
            )));
        }
    }
    for w in &cfg.workflows {
        for problem in w.validate() {
            findings.push(Finding::warn(format!("workflow '{}': {problem}", w.id)));
        }
    }
    findings
}

pub fn gitignore(repo_base: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let gi = std::fs::read_to_string(repo_base.join(".gitignore")).unwrap_or_default();
    if gi
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == ".loadout/generated")
    {
        findings.push(Finding::ok(".gitignore covers .loadout/generated/"));
    } else {
        findings.push(Finding::warn(
            ".gitignore missing .loadout/generated/ (render an agent to manage it)",
        ));
    }
    findings
}

pub fn claude_marker(repo_base: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let path = repo_base.join("CLAUDE.local.md");
    if !path.exists() {
        return findings; // nothing rendered for Claude yet; not a problem
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.contains(crate::writer::BLOCK_BEGIN) {
        findings.push(Finding::ok("CLAUDE.local.md has the managed import block"));
    } else {
        findings.push(Finding::warn(
            "CLAUDE.local.md exists but lacks the managed block (re-run render)",
        ));
    }
    findings
}

/// The fixed subset run on every `load refresh`: pure, fast, config- and
/// repo-local reads only — no script execution, no network. Doctor remains
/// the verbose superset.
pub fn refresh_subset(cfg: &config::Config, repo_base: &std::path::Path) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(env_policy(cfg));
    out.extend(public_leaks(cfg));
    out.extend(secret_leaks(cfg));
    out.extend(dangling_fragment_refs(cfg));
    out.extend(default_loadout(cfg));
    out.extend(workflows(cfg));
    // Repo-file checks only where loadout is actually wired into this repo —
    // an off-repo refresh must stay silent (see commit b23e225).
    if config::repo_dir(repo_base).exists() {
        out.extend(gitignore(repo_base));
        out.extend(claude_marker(repo_base));
    }
    out
}
