//! Per-machine script trust: a TOFU (trust-on-first-use) hash store for
//! fragment and target scripts.
//!
//! Lives in the per-machine state dir ([`crate::config::state_dir`]) — never
//! synced. For each script-bearing object it records the sorted set of hashes
//! of every script body (+ interpreter) reachable in that object; any set
//! difference is an out-of-band change (hand edit, `load sync` pull) and is
//! warned about until the user re-approves via `load fragments trust <id>` /
//! `load targets trust <id>` — or edits through loadout itself, where the
//! explicit edit is the approval. Policy in this release: warn-and-execute.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use serde::{Deserialize, Serialize};

/// File name inside the state dir.
pub const STORE_FILE: &str = "trust.json";

/// Which kind of object owns the scripts (keys are namespaced by it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fragment,
    Target,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Fragment => "fragment",
            Kind::Target => "target",
        }
    }
}

/// Outcome of comparing an object's current script hashes to the record.
#[derive(Debug, PartialEq, Eq)]
pub enum TrustStatus {
    /// Hashes match the record exactly.
    Trusted,
    /// Never seen before — record silently (TOFU).
    FirstSeen,
    /// Any set difference from the record: warn until re-approved.
    Changed,
    /// The store exists but is unreadable — loud warning, record nothing
    /// (corrupting the store must not silence change warnings).
    Unavailable,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    /// `"fragment:<id>"` / `"target:<id>"` → sorted script hashes.
    entries: BTreeMap<String, BTreeSet<String>>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

/// The on-disk trust store. Missing file = normal first run (empty). A file
/// that exists but fails to parse = corrupt: NEVER treated as empty.
#[derive(Debug)]
pub struct TrustStore {
    path: Option<PathBuf>,
    file: StoreFile,
    corrupt: bool,
}

impl TrustStore {
    /// Load from the per-machine state dir.
    pub fn load_default() -> Self {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => Self {
                path: None,
                file: StoreFile::default(),
                corrupt: false,
            },
        }
    }

    #[cfg(not(test))]
    fn default_path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join(STORE_FILE))
    }

    /// In-process unit tests must never touch the real per-machine store —
    /// ANY code path (exec sites, studio apply, CLI, `load edit`) that
    /// reaches `load_default()` from a test build resolves here instead.
    #[cfg(test)]
    fn default_path() -> Option<PathBuf> {
        Some(
            std::env::temp_dir()
                .join(format!("loadout-trust-test-{}", std::process::id()))
                .join(STORE_FILE),
        )
    }

    /// Load from an explicit path (unit tests and recovery paths).
    pub fn load_from(path: &Path) -> Self {
        let (file, corrupt) = match std::fs::read_to_string(path) {
            Err(_) => (StoreFile::default(), false),
            Ok(text) => match serde_json::from_str::<StoreFile>(&text) {
                Ok(f) => (f, false),
                Err(_) => (StoreFile::default(), true),
            },
        };
        Self {
            path: Some(path.to_path_buf()),
            file,
            corrupt,
        }
    }

    pub fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    pub fn len(&self) -> usize {
        self.file.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file.entries.is_empty()
    }

    fn key(kind: Kind, id: &str) -> String {
        format!("{}:{}", kind.as_str(), id)
    }

    /// Compare without recording.
    pub fn status(&self, kind: Kind, id: &str, hashes: &BTreeSet<String>) -> TrustStatus {
        if self.corrupt {
            return TrustStatus::Unavailable;
        }
        match self.file.entries.get(&Self::key(kind, id)) {
            None => TrustStatus::FirstSeen,
            Some(known) if known == hashes => TrustStatus::Trusted,
            Some(_) => TrustStatus::Changed,
        }
    }

    /// Record (overwrite) one object's hashes and save. Refused while the
    /// store is corrupt — recovery goes through [`TrustStore::rebuild`].
    pub fn record(&mut self, kind: Kind, id: &str, hashes: &BTreeSet<String>) -> crate::Result<()> {
        if self.corrupt {
            return Ok(());
        }
        self.file
            .entries
            .insert(Self::key(kind, id), hashes.clone());
        self.save()
    }

    /// Drop everything for an explicit from-scratch re-approval; clears the
    /// corrupt flag (this is the `load trust --rebuild` recovery path).
    pub fn rebuild(&mut self) {
        self.file = StoreFile::default();
        self.corrupt = false;
    }

    pub fn save(&self) -> crate::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        crate::writer::atomic_write(path, &serde_json::to_string_pretty(&self.file)?)
    }
}

/// `sha256:` hash of one script body + interpreter — the same tuple the
/// per-repo verdict cache keys by (`src/target.rs`), so the two stay
/// comparable by construction.
pub fn script_hash(command: &str, script_lang: Option<&str>) -> String {
    crate::hash::context_hash(&(command, script_lang))
}

/// Hash set for a script-backed fragment; `None` for provider/static fragments.
pub fn fragment_hashes(cap: &crate::fragment::Fragment) -> Option<BTreeSet<String>> {
    let command = cap.command.as_deref()?;
    Some(std::iter::once(script_hash(command, cap.script_lang.as_deref())).collect())
}

/// Every script hash reachable in a target's rule tree (one target id can own
/// several nested `Script` predicates — the SET is what is trusted).
pub fn target_hashes(t: &crate::target::TargetDef) -> BTreeSet<String> {
    fn walk(rule: &crate::target::TargetRule, out: &mut BTreeSet<String>) {
        use crate::target::TargetRule;
        match rule {
            TargetRule::Script {
                command,
                script_lang,
                ..
            } => {
                out.insert(script_hash(command, script_lang.as_deref()));
            }
            TargetRule::AllOf { rules } | TargetRule::AnyOf { rules } => {
                for r in rules {
                    walk(r, out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(&t.rule, &mut out);
    out
}

/// Process-wide store handle: the CLI is short-lived, and the exec sites are
/// deep in call chains that shouldn't all grow a store parameter.
/// (Test isolation lives in `TrustStore::default_path`, so every
/// `load_default()` caller is safe by construction — not just this one.)
fn store() -> &'static Mutex<TrustStore> {
    static STORE: OnceLock<Mutex<TrustStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(TrustStore::load_default()))
}

/// Consult-and-record at a script execution site. TOFU: a first sighting is
/// recorded silently; a set difference warns (and keeps warning until
/// re-approved) but does not block execution in this release.
pub fn check_and_warn(kind: Kind, id: &str, hashes: &BTreeSet<String>) {
    if hashes.is_empty() {
        return;
    }
    let mut store = store().lock().unwrap();
    match store.status(kind, id, hashes) {
        TrustStatus::Trusted => {}
        TrustStatus::FirstSeen => {
            if let Err(e) = store.record(kind, id, hashes) {
                crate::vlog!("could not record script trust for {} '{id}': {e}", kind.as_str());
            }
        }
        TrustStatus::Changed => match kind {
            Kind::Fragment => crate::warn_user!(
                "script fragment '{id}' changed outside loadout — review it, then run 'load fragments trust {id}'"
            ),
            Kind::Target => crate::warn_user!(
                "target '{id}' script changed outside loadout — review it, then run 'load targets trust {id}'"
            ),
        },
        TrustStatus::Unavailable => {
            static WARNED: Once = Once::new();
            WARNED.call_once(|| {
                crate::warn_user!(
                    "script trust store is unreadable — change warnings are off; run 'load trust --rebuild' to re-approve everything and recover"
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hs(items: &[&str]) -> std::collections::BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_store_is_first_run_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::load_from(&dir.path().join("trust.json"));
        assert!(!store.is_corrupt());
        assert_eq!(
            store.status(Kind::Fragment, "probe", &hs(&["sha256:aa"])),
            TrustStatus::FirstSeen
        );
    }

    #[test]
    fn record_then_reload_is_trusted_and_any_set_difference_is_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let mut store = TrustStore::load_from(&path);
        store
            .record(Kind::Fragment, "probe", &hs(&["sha256:aa", "sha256:bb"]))
            .unwrap();
        let store = TrustStore::load_from(&path);
        assert_eq!(
            store.status(Kind::Fragment, "probe", &hs(&["sha256:aa", "sha256:bb"])),
            TrustStatus::Trusted
        );
        // Replaced member and dropped member both count as change.
        assert_eq!(
            store.status(Kind::Fragment, "probe", &hs(&["sha256:aa", "sha256:cc"])),
            TrustStatus::Changed
        );
        assert_eq!(
            store.status(Kind::Fragment, "probe", &hs(&["sha256:aa"])),
            TrustStatus::Changed
        );
        // Kinds are namespaced: a target with the same id is unrelated.
        assert_eq!(
            store.status(Kind::Target, "probe", &hs(&["sha256:aa"])),
            TrustStatus::FirstSeen
        );
    }

    #[test]
    fn corrupt_store_is_unavailable_and_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        std::fs::write(&path, "{ not json").unwrap();
        let mut store = TrustStore::load_from(&path);
        assert!(store.is_corrupt());
        assert_eq!(
            store.status(Kind::Target, "t", &hs(&["sha256:aa"])),
            TrustStatus::Unavailable
        );
        // Recording is refused: corrupting/emptying the store must not let
        // fresh hashes be silently re-recorded (design: NOT treated as empty).
        store
            .record(Kind::Target, "t", &hs(&["sha256:aa"]))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn script_hash_matches_the_target_cache_tuple() {
        // Must stay in lockstep with the verdict-cache key recipe in
        // src/target.rs (context_hash over (command, script_lang)).
        assert_eq!(
            script_hash("test -f x", Some("bash")),
            crate::hash::context_hash(&("test -f x", Some("bash"))),
        );
    }

    #[test]
    fn fragment_hashes_only_for_script_fragments() {
        let mut cap = crate::fragment::Fragment {
            id: "probe".to_string(),
            description: None,
            category: None,
            when: Vec::new(),
            requires: Vec::new(),
            params: toml::Value::Table(toml::map::Map::new()),
            guidance: String::new(),
            agents: Vec::new(),
            provider: None,
            command: Some("echo hi".to_string()),
            script_lang: None,
            allow_exec: true,
            cache: None,
            origin: crate::fragment::Layer::default(),
        };
        assert_eq!(fragment_hashes(&cap).unwrap().len(), 1);
        cap.command = None;
        assert!(fragment_hashes(&cap).is_none());
    }

    #[test]
    fn target_hashes_collects_every_nested_script() {
        use crate::target::TargetRule;
        let rule = TargetRule::AnyOf {
            rules: vec![
                TargetRule::Script {
                    command: "test -f a".into(),
                    script_lang: None,
                    allow_exec: true,
                    cache: None,
                },
                TargetRule::AllOf {
                    rules: vec![
                        TargetRule::FileExists {
                            path: "Cargo.toml".into(),
                        },
                        TargetRule::Script {
                            command: "test -f b".into(),
                            script_lang: Some("bash".into()),
                            allow_exec: true,
                            cache: None,
                        },
                    ],
                },
            ],
        };
        let t = crate::target::TargetDef {
            id: "multi".to_string(),
            description: None,
            icon: None,
            rule,
            disabled: false,
            origin: crate::fragment::Layer::default(),
        };
        assert_eq!(target_hashes(&t).len(), 2);
    }
}
