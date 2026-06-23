//! The built-in default overlay template and template-source resolution.
//!
//! There is **one** agent-neutral body template (`overlay.md.j2`); the
//! agent-specific freshness/provenance header is prepended in Rust. Resolution
//! order for a given template name (first hit wins):
//! 1. repo templates dir — `<repo>/.loadout/templates/<name>.md.j2`
//! 2. global templates dir — `…/loadout/templates/<name>.md.j2`
//! 3. the embedded default overlay.
//!
//! So an agent's `template` field can name a custom per-agent body (drop a file
//! at `templates/<id>.md.j2`); otherwise every agent shares the embedded overlay.

use std::path::Path;

use crate::config;

/// The single embedded default body template, shared by all agents.
pub const DEFAULT_OVERLAY: &str = include_str!("../templates/overlay.md.j2");

/// A template's resolved content plus a label describing where it came from.
pub struct ResolvedTemplate {
    /// Source label for explain/audit (e.g. `embedded`, a path).
    pub source: String,
    /// Template content.
    pub content: String,
}

/// Resolve a body template by name across repo → global → embedded overlay.
///
/// Any name falls back to the shared embedded overlay, so adding a new agent
/// never requires shipping a new template.
pub fn resolve(repo_base: &Path, name: &str) -> crate::Result<ResolvedTemplate> {
    let file = format!("{name}.md.j2");

    let repo_candidate = config::repo_templates_dir(repo_base).join(&file);
    if let Some(t) = read_if_exists(&repo_candidate)? {
        return Ok(t);
    }
    if let Some(global_dir) = config::global_templates_dir() {
        if let Some(t) = read_if_exists(&global_dir.join(&file))? {
            return Ok(t);
        }
    }
    Ok(ResolvedTemplate {
        source: "embedded".to_string(),
        content: DEFAULT_OVERLAY.to_string(),
    })
}

fn read_if_exists(path: &Path) -> crate::Result<Option<ResolvedTemplate>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading template {}: {e}", path.display()))?;
    Ok(Some(ResolvedTemplate {
        source: display_path(path),
        content,
    }))
}

fn display_path(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_embedded_when_no_files() {
        let d = tempfile::tempdir().unwrap();
        let t = resolve(d.path(), "claude").unwrap();
        assert_eq!(t.source, "embedded");
        assert!(t.content.contains("agent context"));
        // Any agent id falls back to the same shared overlay.
        let t2 = resolve(d.path(), "gemini").unwrap();
        assert_eq!(t2.source, "embedded");
        assert_eq!(t.content, t2.content);
    }

    #[test]
    fn repo_override_wins() {
        let d = tempfile::tempdir().unwrap();
        let tdir = config::repo_templates_dir(d.path());
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("copilot.md.j2"), "CUSTOM {{ profile }}").unwrap();
        let t = resolve(d.path(), "copilot").unwrap();
        assert!(t.source.ends_with("copilot.md.j2"));
        assert_eq!(t.content, "CUSTOM {{ profile }}");
    }
}
