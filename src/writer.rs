//! Safe file writing: atomic replace, managed marker blocks, dry-run.
//!
//! Atomic writes go through a temp file in the *same directory* (so the final
//! rename is atomic on the same filesystem), with an `fsync` before rename. The
//! marker-block helpers are pure functions so the tricky "preserve surrounding
//! content" logic is fully unit tested.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use std::io::Write as _;

/// Begin marker for managed blocks inside user-owned files.
pub const BLOCK_BEGIN: &str = "<!-- BEGIN loadout (managed) -->";
/// End marker for managed blocks.
pub const BLOCK_END: &str = "<!-- END loadout (managed) -->";

/// What a write did (or would do, in dry-run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteAction {
    /// File did not exist and was created.
    Created,
    /// File existed and content changed.
    Updated,
    /// Content already matched; nothing written.
    Unchanged,
    /// Dry-run: would create.
    WouldCreate,
    /// Dry-run: would update.
    WouldUpdate,
}

impl WriteAction {
    /// Short human label.
    pub fn label(self) -> &'static str {
        match self {
            WriteAction::Created => "created",
            WriteAction::Updated => "updated",
            WriteAction::Unchanged => "unchanged",
            WriteAction::WouldCreate => "would create",
            WriteAction::WouldUpdate => "would update",
        }
    }
}

/// Record of a single write.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WrittenFile {
    /// Target path.
    pub path: PathBuf,
    /// Outcome.
    pub action: WriteAction,
    /// Byte length of the intended content.
    pub bytes: usize,
}

/// A file writer (apply or dry-run).
pub trait Writer {
    /// Write `contents` to `path`, returning what happened.
    fn write(&self, path: &Path, contents: &str) -> crate::Result<WrittenFile>;
    /// Whether this writer only simulates.
    fn is_dry_run(&self) -> bool;
}

/// Atomic filesystem writer.
pub struct AtomicWriter {
    dry_run: bool,
}

impl AtomicWriter {
    /// Construct, choosing apply vs dry-run.
    pub fn new(dry_run: bool) -> Self {
        AtomicWriter { dry_run }
    }
}

impl Writer for AtomicWriter {
    fn write(&self, path: &Path, contents: &str) -> crate::Result<WrittenFile> {
        let existing = std::fs::read_to_string(path).ok();
        let unchanged = existing.as_deref() == Some(contents);
        let existed = existing.is_some();

        let action = if unchanged {
            WriteAction::Unchanged
        } else if self.dry_run {
            if existed {
                WriteAction::WouldUpdate
            } else {
                WriteAction::WouldCreate
            }
        } else if existed {
            WriteAction::Updated
        } else {
            WriteAction::Created
        };

        if !self.dry_run && !unchanged {
            atomic_write(path, contents)?;
        }

        Ok(WrittenFile {
            path: path.to_path_buf(),
            action,
            bytes: contents.len(),
        })
    }

    fn is_dry_run(&self) -> bool {
        self.dry_run
    }
}

/// Write `contents` to `path` atomically: temp file in the same dir → fsync →
/// rename. Creates parent directories as needed.
pub fn atomic_write(path: &Path, contents: &str) -> crate::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".loadout-tmp-")
        .tempfile_in(&parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    tmp.write_all(contents.as_bytes())
        .context("writing temp file")?;
    tmp.flush().context("flushing temp file")?;
    // fsync the data before the rename for durability where supported.
    tmp.as_file().sync_all().ok();
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("atomically replacing {}: {}", path.display(), e.error))?;
    Ok(())
}

/// Insert or update a managed block inside `existing`, preserving everything
/// outside the markers. Returns the full new file content.
///
/// - No existing content → just the block.
/// - Markers present → the region between (and including) them is replaced.
/// - No markers → the block is appended after the existing content.
pub fn upsert_marker_block(existing: Option<&str>, body: &str) -> String {
    let block = format!(
        "{BLOCK_BEGIN}\n{}\n{BLOCK_END}",
        body.trim_end_matches('\n')
    );

    let Some(existing) = existing.filter(|s| !s.trim().is_empty()) else {
        return format!("{block}\n");
    };

    if let (Some(begin), Some(end_start)) = (existing.find(BLOCK_BEGIN), existing.find(BLOCK_END)) {
        if end_start >= begin {
            let end = end_start + BLOCK_END.len();
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..begin]);
            out.push_str(&block);
            out.push_str(&existing[end..]);
            return out;
        }
    }

    // No (valid) markers: append, separated by a blank line.
    let mut out = existing.trim_end_matches('\n').to_string();
    out.push_str("\n\n");
    out.push_str(&block);
    out.push('\n');
    out
}

/// Remove the managed block (markers included) from `content`, preserving
/// everything outside it. Returns `content` unchanged if no block is present.
/// The inverse of [`upsert_marker_block`].
pub fn remove_marker_block(content: &str) -> String {
    let (Some(begin), Some(end_start)) = (content.find(BLOCK_BEGIN), content.find(BLOCK_END))
    else {
        return content.to_string();
    };
    if end_start < begin {
        return content.to_string();
    }
    let end = end_start + BLOCK_END.len();
    let head = content[..begin].trim_end_matches('\n');
    let tail = content[end..].trim_start_matches('\n');
    let mut out = String::with_capacity(content.len());
    out.push_str(head);
    if !head.is_empty() && !tail.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(tail);
    out
}

/// Compute the new `.gitignore` content needed to ensure `entry` is present,
/// or `None` if it already is. Matches whole lines (ignoring trailing slashes).
pub fn ensure_line(existing: Option<&str>, entry: &str) -> Option<String> {
    let needle = entry.trim_end_matches('/');
    let present = existing
        .into_iter()
        .flat_map(|c| c.lines())
        .any(|l| l.trim().trim_end_matches('/') == needle);
    if present {
        return None;
    }
    match existing.filter(|s| !s.is_empty()) {
        Some(c) => {
            let mut out = c.trim_end_matches('\n').to_string();
            out.push('\n');
            out.push_str(entry);
            out.push('\n');
            Some(out)
        }
        None => Some(format!("{entry}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nested/dir/file.md");
        let w = AtomicWriter::new(false);

        let r = w.write(&p, "hello").unwrap();
        assert_eq!(r.action, WriteAction::Created);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");

        let r = w.write(&p, "hello").unwrap();
        assert_eq!(r.action, WriteAction::Unchanged);

        let r = w.write(&p, "world").unwrap();
        assert_eq!(r.action, WriteAction::Updated);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "world");
    }

    #[test]
    fn dry_run_does_not_write() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("file.md");
        let w = AtomicWriter::new(true);
        let r = w.write(&p, "hello").unwrap();
        assert_eq!(r.action, WriteAction::WouldCreate);
        assert!(!p.exists());
    }

    #[test]
    fn marker_block_into_empty() {
        let out = upsert_marker_block(None, "@.loadout/generated/claude.md");
        assert!(out.starts_with(BLOCK_BEGIN));
        assert!(out.contains("@.loadout/generated/claude.md"));
        assert!(out.trim_end().ends_with(BLOCK_END));
    }

    #[test]
    fn marker_block_appends_preserving_content() {
        let existing = "# My notes\n\nSome important user content.\n";
        let out = upsert_marker_block(Some(existing), "IMPORT");
        assert!(out.contains("Some important user content."));
        assert!(out.contains(BLOCK_BEGIN));
        // user content stays before the block
        assert!(out.find("important").unwrap() < out.find(BLOCK_BEGIN).unwrap());
    }

    #[test]
    fn marker_block_updates_in_place() {
        let existing = format!("# Top\n\n{BLOCK_BEGIN}\nOLD\n{BLOCK_END}\n\n# Bottom kept\n");
        let out = upsert_marker_block(Some(&existing), "NEW");
        assert!(out.contains("NEW"));
        assert!(!out.contains("OLD"));
        assert!(out.contains("# Top"));
        assert!(out.contains("# Bottom kept"));
        // exactly one managed block
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn remove_marker_block_strips_and_preserves() {
        let existing = format!("# Top\n\n{BLOCK_BEGIN}\nIMPORT\n{BLOCK_END}\n\n# Bottom\n");
        let out = remove_marker_block(&existing);
        assert!(!out.contains(BLOCK_BEGIN));
        assert!(!out.contains("IMPORT"));
        assert!(out.contains("# Top"));
        assert!(out.contains("# Bottom"));
    }

    #[test]
    fn remove_marker_block_noop_without_block() {
        assert_eq!(remove_marker_block("# just notes\n"), "# just notes\n");
    }

    #[test]
    fn upsert_then_remove_roundtrips_to_empty() {
        let out = upsert_marker_block(None, "IMPORT");
        assert!(remove_marker_block(&out).trim().is_empty());
    }

    #[test]
    fn ensure_line_adds_when_missing() {
        assert_eq!(
            ensure_line(None, ".loadout/generated/"),
            Some(".loadout/generated/\n".to_string())
        );
        let existing = "/target\n";
        let out = ensure_line(Some(existing), ".loadout/generated/").unwrap();
        assert!(out.contains("/target"));
        assert!(out.contains(".loadout/generated/"));
    }

    #[test]
    fn ensure_line_noop_when_present() {
        let existing = "/target\n.loadout/generated/\n";
        assert_eq!(ensure_line(Some(existing), ".loadout/generated/"), None);
        // trailing-slash-insensitive
        assert_eq!(ensure_line(Some(existing), ".loadout/generated"), None);
    }
}
