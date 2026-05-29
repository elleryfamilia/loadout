//! Append-only audit log of every render.
//!
//! Each `render`/`refresh`/`run` writes one JSON object per line to
//! `.rosita/logs/events.jsonl`, capturing what was selected, why, and what was
//! written (including dry-runs, flagged as such).

use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::writer::WrittenFile;

/// One audit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// RFC3339 timestamp.
    pub timestamp: String,
    /// Agent id.
    pub agent: String,
    /// Primary (highest-priority) matching profile.
    pub profile: String,
    /// Active capability ids, in render order.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Detected stacks.
    #[serde(default)]
    pub stacks: Vec<String>,
    /// Files written (or that would be written in dry-run).
    #[serde(default)]
    pub files: Vec<WrittenFile>,
    /// Reasons the profile matched.
    #[serde(default)]
    pub reasons: Vec<String>,
    /// Context hash.
    pub context_hash: String,
    /// Whether this was a dry-run.
    #[serde(default)]
    pub dry_run: bool,
}

/// Append `event` to the repo audit log. Best-effort: never fails a render.
pub fn record(repo_base: &Path, event: &AuditEvent) -> crate::Result<()> {
    let path = config::audit_log_path(repo_base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(event)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Read the most recent audit event for a repo, if any.
pub fn last_event(repo_base: &Path) -> Option<AuditEvent> {
    let path = config::audit_log_path(repo_base);
    let content = std::fs::read_to_string(path).ok()?;
    let last = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .next_back()?;
    serde_json::from_str(last).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::WriteAction;

    fn event() -> AuditEvent {
        AuditEvent {
            timestamp: "2026-05-29T00:00:00Z".into(),
            agent: "claude".into(),
            profile: "rust".into(),
            capabilities: vec!["rust-conventions".into()],
            stacks: vec!["rust".into()],
            files: vec![WrittenFile {
                path: ".rosita/generated/claude.md".into(),
                action: WriteAction::Created,
                bytes: 42,
            }],
            reasons: vec!["Stack equals \"rust\"".into()],
            context_hash: "sha256:abc".into(),
            dry_run: false,
        }
    }

    #[test]
    fn appends_and_reads_back_last() {
        let d = tempfile::tempdir().unwrap();
        record(d.path(), &event()).unwrap();
        let mut e2 = event();
        e2.profile = "infra".into();
        record(d.path(), &e2).unwrap();

        let last = last_event(d.path()).unwrap();
        assert_eq!(last.profile, "infra");
        assert_eq!(last.agent, "claude");
        assert_eq!(last.files.len(), 1);
        assert_eq!(last.files[0].action, WriteAction::Created);

        // Two lines in the file.
        let content = std::fs::read_to_string(config::audit_log_path(d.path())).unwrap();
        assert_eq!(content.lines().filter(|l| !l.is_empty()).count(), 2);
    }
}
