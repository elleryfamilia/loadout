//! Embedded agent skills — install, inspect, upgrade, remove.
//!
//! loadout ships agent skills (SKILL.md directories per the cross-agent Agent
//! Skills format) embedded in the binary, so installer-based users get them
//! without a repo checkout. The canonical install location is
//! `~/.agents/skills/<id>/` — read natively by Gemini CLI and opencode — with
//! symlinks from `~/.claude/skills/<id>` and `~/.codex/skills/<id>` for agents
//! that only scan their own dotdir. Symlinks are only created when the agent's
//! dotdir already exists (no littering for agents the user doesn't have), and
//! fall back to a marked copy where symlinking fails.
//!
//! Installed SKILL.md files carry a managed marker line (after the YAML
//! frontmatter) holding the content hash of the installed version. Three hashes
//! drive the lifecycle:
//!
//! - **marker hash** — what loadout last installed;
//! - **embedded hash** — what this binary ships;
//! - **on-disk hash** — recomputed from the files (marker stripped).
//!
//! managed = marker present; user-modified = on-disk ≠ marker (never touched
//! again; `doctor` warns); upgrade available = marker ≠ embedded. The ask-once
//! install decision is remembered per-machine in the bindings store (see
//! [`crate::binding`]); all lifecycle changes go through `load skill …` —
//! `clean` stays repo-scoped and never touches skills.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};

use crate::hash;
use crate::writer::atomic_write;

/// One file of an embedded skill.
#[derive(Debug, Clone, Copy)]
pub struct SkillFile {
    /// Path relative to the skill directory (e.g. `SKILL.md`).
    pub relpath: &'static str,
    /// Embedded content (the repo source, marker-free).
    pub content: &'static str,
}

/// An embedded skill: a directory of files identified by a stable id.
#[derive(Debug, Clone, Copy)]
pub struct Skill {
    /// Stable id — also the install directory name.
    pub id: &'static str,
    /// The files; the first must be `SKILL.md`.
    pub files: &'static [SkillFile],
}

/// The `loadout-migrate` skill: imports an existing CLAUDE.md/AGENTS.md into
/// load fragments + profiles.
pub const MIGRATE: Skill = Skill {
    id: "loadout-migrate",
    files: &[
        SkillFile {
            relpath: "SKILL.md",
            content: include_str!("../skills/loadout-migrate/SKILL.md"),
        },
        SkillFile {
            relpath: "reference.md",
            content: include_str!("../skills/loadout-migrate/reference.md"),
        },
    ],
};

/// The `loadout-remember` skill: saves durable, cross-project user guidance
/// into load fragments — the ongoing-maintenance complement to migrate's
/// one-time import.
pub const REMEMBER: Skill = Skill {
    id: "loadout-remember",
    files: &[
        SkillFile {
            relpath: "SKILL.md",
            content: include_str!("../skills/loadout-remember/SKILL.md"),
        },
        SkillFile {
            relpath: "reference.md",
            content: include_str!("../skills/loadout-remember/reference.md"),
        },
    ],
};

/// Every skill shipped in this binary.
pub fn all() -> &'static [Skill] {
    &[MIGRATE, REMEMBER]
}

/// Look up an embedded skill by id.
pub fn by_id(id: &str) -> Option<&'static Skill> {
    all().iter().find(|s| s.id == id)
}

// --- marker -------------------------------------------------------------------

/// Prefix of the managed marker line written into installed SKILL.md files.
pub const SKILL_MARKER: &str = "<!-- loadout:skill";

fn marker_line(content_hash: &str) -> String {
    format!(
        "{SKILL_MARKER} content={content_hash} — installed by loadout; edits disable auto-upgrade; manage with `load skill` -->"
    )
}

/// Extract the `content=sha256:…` hash from an installed SKILL.md, if marked.
pub fn extract_marker_hash(skill_md: &str) -> Option<String> {
    for line in skill_md.lines() {
        let Some(rest) = line.trim_start().strip_prefix(SKILL_MARKER) else {
            continue;
        };
        let Some(token) = rest.trim_start().strip_prefix("content=") else {
            continue;
        };
        let hash: String = token.chars().take_while(|c| !c.is_whitespace()).collect();
        if !hash.is_empty() {
            return Some(hash);
        }
    }
    None
}

/// Remove any managed marker lines, restoring the embedded content byte-for-byte.
fn strip_marker(content: &str) -> String {
    // Filter marker lines while preserving the original line endings exactly:
    // split inclusively so unmarked content round-trips unchanged.
    content
        .split_inclusive('\n')
        .filter(|line| !line.trim_start().starts_with(SKILL_MARKER))
        .collect()
}

/// Insert the marker line after the YAML frontmatter (agents require the
/// frontmatter to open the file); prepends if no frontmatter is found.
fn insert_marker(content: &str, content_hash: &str) -> String {
    let marker = marker_line(content_hash);
    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let split = 4 + end + 5; // opening "---\n" + body + closing "\n---\n"
            return format!("{}{}\n{}", &content[..split], marker, &content[split..]);
        }
    }
    format!("{}\n{}", marker, content)
}

// --- hashes -------------------------------------------------------------------

/// `sha256:…` over the skill's embedded files (relpath + content).
pub fn embedded_hash(skill: &Skill) -> String {
    let pairs: Vec<(&str, &str)> = skill.files.iter().map(|f| (f.relpath, f.content)).collect();
    hash::context_hash(&pairs)
}

/// Recompute the content hash from an installed directory, stripping markers.
/// `None` if any manifest file is missing or unreadable.
fn on_disk_hash(dir: &Path, skill: &Skill) -> Option<String> {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    for f in skill.files {
        let text = std::fs::read_to_string(dir.join(f.relpath)).ok()?;
        pairs.push((f.relpath, strip_marker(&text)));
    }
    Some(hash::context_hash(&pairs))
}

// --- layout -------------------------------------------------------------------

/// Agent dotdirs that need their own `skills/` entry (symlinked to canonical).
/// Gemini CLI and opencode read `~/.agents/skills/` natively and need nothing.
const LINKED_AGENT_DIRS: &[&str] = &[".claude", ".codex"];

/// Canonical install directory: `<home>/.agents/skills/<id>`.
pub fn canonical_dir(home: &Path, id: &str) -> PathBuf {
    home.join(".agents").join("skills").join(id)
}

fn link_path(home: &Path, dotdir: &str, id: &str) -> PathBuf {
    home.join(dotdir).join("skills").join(id)
}

// --- status -------------------------------------------------------------------

/// State of the canonical install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillState {
    /// Canonical directory absent.
    NotInstalled,
    /// Marker present: loadout manages this install.
    Managed {
        /// Hash recorded by the marker (the installed version).
        marker_hash: String,
        /// On-disk content (marker stripped) no longer matches the marker —
        /// the user edited the files; loadout won't touch them again.
        user_modified: bool,
        /// The binary ships a different version than the marker records.
        upgrade_available: bool,
    },
    /// Files exist but carry no marker — the user's own copy; never touched.
    Unmanaged,
}

/// State of one agent-dotdir link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    /// The agent dotdir (e.g. `.claude`).
    pub dotdir: &'static str,
    /// The link/copy path under that dotdir.
    pub path: PathBuf,
    /// What's there.
    pub state: LinkState,
}

/// What occupies an agent-dotdir skill path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Agent dotdir absent — agent not installed; nothing expected.
    AgentAbsent,
    /// Symlink resolving to the canonical directory.
    Linked,
    /// Nothing there (agent present, link missing).
    Missing,
    /// Symlink exists but its target is gone or isn't the canonical dir.
    Dangling,
    /// A real directory carrying our marker (Windows/copy-fallback install).
    CopyManaged,
    /// A real file/dir without our marker — the user's own; never touched.
    Foreign,
}

/// Full install status for a skill under `home`.
#[derive(Debug, Clone)]
pub struct SkillStatus {
    /// Canonical directory state.
    pub state: SkillState,
    /// Per-agent link states.
    pub links: Vec<LinkStatus>,
}

/// Inspect the install under `home` (pure filesystem; no decision state).
pub fn status(home: &Path, skill: &Skill) -> SkillStatus {
    let dir = canonical_dir(home, skill.id);
    let state = if !dir.exists() {
        SkillState::NotInstalled
    } else {
        let skill_md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
        match extract_marker_hash(&skill_md) {
            None => SkillState::Unmanaged,
            Some(marker_hash) => {
                let on_disk = on_disk_hash(&dir, skill);
                SkillState::Managed {
                    user_modified: on_disk.as_deref() != Some(marker_hash.as_str()),
                    upgrade_available: marker_hash != embedded_hash(skill),
                    marker_hash,
                }
            }
        }
    };

    let links = LINKED_AGENT_DIRS
        .iter()
        .map(|dotdir| {
            let path = link_path(home, dotdir, skill.id);
            LinkStatus {
                dotdir,
                state: link_state(home, dotdir, &path, &dir),
                path,
            }
        })
        .collect();

    SkillStatus { state, links }
}

fn link_state(home: &Path, dotdir: &str, path: &Path, canonical: &Path) -> LinkState {
    if !home.join(dotdir).exists() {
        return LinkState::AgentAbsent;
    }
    match std::fs::symlink_metadata(path) {
        Err(_) => LinkState::Missing,
        Ok(meta) if meta.is_symlink() => match (std::fs::read_link(path), path.exists()) {
            (Ok(target), true) if resolves_to(path, &target, canonical) => LinkState::Linked,
            _ => LinkState::Dangling,
        },
        Ok(_) => {
            let skill_md = std::fs::read_to_string(path.join("SKILL.md")).unwrap_or_default();
            if extract_marker_hash(&skill_md).is_some() {
                LinkState::CopyManaged
            } else {
                LinkState::Foreign
            }
        }
    }
}

/// Does `target` (as read from the symlink at `link`) point at `canonical`?
fn resolves_to(link: &Path, target: &Path, canonical: &Path) -> bool {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent().unwrap_or(Path::new("/")).join(target)
    };
    // Compare canonicalized forms when possible; fall back to literal paths.
    match (resolved.canonicalize(), canonical.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => resolved == canonical,
    }
}

// --- install / repair / remove -------------------------------------------------

/// One line of an install/repair report, for the CLI to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAction {
    /// Wrote the canonical files (fresh install).
    WroteCanonical(PathBuf),
    /// Rewrote a pristine managed install with this binary's newer version.
    UpgradedCanonical(PathBuf),
    /// Canonical files already current; nothing written.
    CanonicalCurrent(PathBuf),
    /// Canonical files left alone: user-modified (or unmanaged).
    SkippedModified(PathBuf),
    /// Created a symlink in an agent dotdir.
    Linked(PathBuf),
    /// Symlinking failed; installed a marked copy instead.
    Copied(PathBuf),
    /// Existing foreign file/dir in the way; left alone.
    SkippedForeign(PathBuf),
}

/// Install (or repair) the skill under `home`: write canonical files when new
/// or upgrading a pristine managed install, then ensure links for every agent
/// dotdir that exists. Idempotent; never touches user-modified or foreign
/// files. Returns the actions taken.
pub fn install(home: &Path, skill: &Skill) -> Result<Vec<InstallAction>> {
    let mut actions = Vec::new();
    let dir = canonical_dir(home, skill.id);
    let current = status(home, skill);

    match current.state {
        SkillState::NotInstalled => {
            write_skill_files(&dir, skill)?;
            actions.push(InstallAction::WroteCanonical(dir.clone()));
        }
        SkillState::Managed {
            user_modified: false,
            upgrade_available: true,
            ..
        } => {
            write_skill_files(&dir, skill)?;
            actions.push(InstallAction::UpgradedCanonical(dir.clone()));
        }
        SkillState::Managed {
            user_modified: true,
            ..
        } => actions.push(InstallAction::SkippedModified(dir.clone())),
        SkillState::Unmanaged => actions.push(InstallAction::SkippedModified(dir.clone())),
        SkillState::Managed { .. } => actions.push(InstallAction::CanonicalCurrent(dir.clone())),
    }

    for link in status(home, skill).links {
        match link.state {
            LinkState::AgentAbsent | LinkState::Linked | LinkState::CopyManaged => {}
            LinkState::Foreign => actions.push(InstallAction::SkippedForeign(link.path)),
            LinkState::Missing | LinkState::Dangling => {
                if link.state == LinkState::Dangling {
                    remove_path(&link.path)?;
                }
                if let Some(parent) = link.path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                match make_symlink(&dir, &link.path) {
                    Ok(()) => actions.push(InstallAction::Linked(link.path)),
                    Err(_) => {
                        // Copy fallback (e.g. Windows without symlink rights):
                        // a marked, independent copy; doctor tracks its staleness.
                        write_skill_files(&link.path, skill)?;
                        actions.push(InstallAction::Copied(link.path));
                    }
                }
            }
        }
    }

    Ok(actions)
}

/// Remove the managed install: canonical dir (only when it carries our marker)
/// plus every symlink/copy loadout created. Refuses to delete an unmanaged
/// (marker-less) canonical dir. Returns the paths removed.
pub fn remove(home: &Path, skill: &Skill) -> Result<Vec<PathBuf>> {
    let current = status(home, skill);
    let mut removed = Vec::new();

    // Links first, so a failure midway never leaves links pointing at nothing.
    for link in &current.links {
        match link.state {
            LinkState::Linked | LinkState::Dangling | LinkState::CopyManaged => {
                remove_path(&link.path)?;
                removed.push(link.path.clone());
            }
            _ => {}
        }
    }

    let dir = canonical_dir(home, skill.id);
    match current.state {
        SkillState::NotInstalled => {}
        SkillState::Unmanaged => bail!(
            "{} exists but has no loadout marker — not removing a file loadout didn't install",
            dir.display()
        ),
        SkillState::Managed { .. } => {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            removed.push(dir);
        }
    }

    Ok(removed)
}

fn write_skill_files(dir: &Path, skill: &Skill) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let content_hash = embedded_hash(skill);
    for f in skill.files {
        let body = if f.relpath == "SKILL.md" {
            insert_marker(f.content, &content_hash)
        } else {
            f.content.to_string()
        };
        atomic_write(&dir.join(f.relpath), &body)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} → {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> Result<()> {
    Err(anyhow::anyhow!("symlinks unavailable on this platform"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn embedded_skill_mds_have_frontmatter_and_no_marker() {
        for skill in all() {
            assert_eq!(skill.files[0].relpath, "SKILL.md");
            let md = skill.files[0].content;
            assert!(
                md.starts_with("---\n"),
                "{}: SKILL.md must open with frontmatter",
                skill.id
            );
            assert!(!md.contains(SKILL_MARKER), "{}: embedded marker", skill.id);
            // The skills must be portable: no Claude-only dynamic preamble.
            assert!(
                !md.contains("!`"),
                "{}: SKILL.md must not use Claude-only dynamic preamble commands",
                skill.id
            );
        }
    }

    #[test]
    fn marker_insert_after_frontmatter_round_trips() {
        let src = "---\nname: x\n---\n\n# Title\nbody\n";
        let marked = insert_marker(src, "sha256:abc");
        let lines: Vec<&str> = marked.lines().collect();
        assert_eq!(lines[2], "---");
        assert!(lines[3].starts_with(SKILL_MARKER));
        assert_eq!(extract_marker_hash(&marked).as_deref(), Some("sha256:abc"));
        assert_eq!(strip_marker(&marked), src);
    }

    #[test]
    fn fresh_install_links_only_existing_agent_dirs() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude")).unwrap(); // claude present, codex absent
        let actions = install(h.path(), &MIGRATE).unwrap();

        let dir = canonical_dir(h.path(), MIGRATE.id);
        assert!(dir.join("SKILL.md").exists());
        assert!(dir.join("reference.md").exists());
        assert!(actions.contains(&InstallAction::WroteCanonical(dir.clone())));
        assert!(actions.contains(&InstallAction::Linked(
            h.path().join(".claude/skills").join(MIGRATE.id)
        )));
        assert!(
            !h.path().join(".codex").exists(),
            "codex dir must not be created"
        );

        let st = status(h.path(), &MIGRATE);
        assert!(matches!(
            st.state,
            SkillState::Managed {
                user_modified: false,
                upgrade_available: false,
                ..
            }
        ));
        assert!(st
            .links
            .iter()
            .any(|l| l.dotdir == ".claude" && l.state == LinkState::Linked));
        assert!(st
            .links
            .iter()
            .any(|l| l.dotdir == ".codex" && l.state == LinkState::AgentAbsent));
    }

    #[test]
    fn reinstall_is_idempotent_and_repairs_deleted_symlink() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude")).unwrap();
        install(h.path(), &MIGRATE).unwrap();

        // Second run: canonical current, nothing rewritten.
        let actions = install(h.path(), &MIGRATE).unwrap();
        let dir = canonical_dir(h.path(), MIGRATE.id);
        assert!(actions.contains(&InstallAction::CanonicalCurrent(dir)));

        // Delete only the symlink → repaired regardless of version.
        let link = h.path().join(".claude/skills").join(MIGRATE.id);
        std::fs::remove_file(&link).unwrap();
        let actions = install(h.path(), &MIGRATE).unwrap();
        assert!(actions.contains(&InstallAction::Linked(link.clone())));
        assert!(link.exists());
    }

    #[test]
    fn user_modified_install_is_never_overwritten() {
        let h = home();
        install(h.path(), &MIGRATE).unwrap();
        let dir = canonical_dir(h.path(), MIGRATE.id);

        // Edit a non-marker file.
        let refpath = dir.join("reference.md");
        let mut text = std::fs::read_to_string(&refpath).unwrap();
        text.push_str("\nuser note\n");
        std::fs::write(&refpath, &text).unwrap();

        let st = status(h.path(), &MIGRATE);
        assert!(matches!(
            st.state,
            SkillState::Managed {
                user_modified: true,
                ..
            }
        ));

        let actions = install(h.path(), &MIGRATE).unwrap();
        assert!(actions.contains(&InstallAction::SkippedModified(dir)));
        assert!(std::fs::read_to_string(&refpath)
            .unwrap()
            .contains("user note"));
    }

    #[test]
    fn unmanaged_dir_is_never_touched_or_removed() {
        let h = home();
        let dir = canonical_dir(h.path(), MIGRATE.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: mine\n---\nmy own skill\n").unwrap();

        assert_eq!(status(h.path(), &MIGRATE).state, SkillState::Unmanaged);
        let actions = install(h.path(), &MIGRATE).unwrap();
        assert!(actions.contains(&InstallAction::SkippedModified(dir.clone())));
        assert!(std::fs::read_to_string(dir.join("SKILL.md"))
            .unwrap()
            .contains("my own skill"));
        assert!(remove(h.path(), &MIGRATE).is_err());
    }

    #[test]
    fn remove_deletes_canonical_and_links() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude")).unwrap();
        std::fs::create_dir_all(h.path().join(".codex")).unwrap();
        install(h.path(), &MIGRATE).unwrap();

        let removed = remove(h.path(), &MIGRATE).unwrap();
        assert_eq!(removed.len(), 3); // two links + canonical dir
        assert_eq!(status(h.path(), &MIGRATE).state, SkillState::NotInstalled);
        assert!(!h.path().join(".claude/skills").join(MIGRATE.id).exists());

        // Removing again is a no-op.
        assert!(remove(h.path(), &MIGRATE).unwrap().is_empty());
    }

    #[test]
    fn stale_marker_reports_upgrade_and_reinstall_refreshes() {
        let h = home();
        let dir = canonical_dir(h.path(), MIGRATE.id);
        std::fs::create_dir_all(&dir).unwrap();

        // Hand-build a *pristine* older install: the marker hash matches the
        // on-disk content but differs from what this binary embeds.
        let old_skill_md = "---\nname: loadout-migrate\n---\nold body\n";
        let old_reference = "old reference\n";
        let old_hash = hash::context_hash(&[
            ("SKILL.md", old_skill_md.to_string()),
            ("reference.md", old_reference.to_string()),
        ]);
        std::fs::write(dir.join("SKILL.md"), insert_marker(old_skill_md, &old_hash)).unwrap();
        std::fs::write(dir.join("reference.md"), old_reference).unwrap();

        let st = status(h.path(), &MIGRATE);
        assert!(matches!(
            st.state,
            SkillState::Managed {
                user_modified: false,
                upgrade_available: true,
                ..
            }
        ));

        let actions = install(h.path(), &MIGRATE).unwrap();
        assert!(actions.contains(&InstallAction::UpgradedCanonical(dir)));
        let st = status(h.path(), &MIGRATE);
        assert!(matches!(
            st.state,
            SkillState::Managed {
                user_modified: false,
                upgrade_available: false,
                ..
            }
        ));
    }
}
