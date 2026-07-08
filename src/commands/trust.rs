//! `load trust`, `load fragments trust`, `load targets trust` — the explicit
//! re-approval paths for the per-machine script trust store (see
//! [`crate::trust`]). Warn-and-execute policy means these commands are how a
//! legitimate out-of-band change stops warning.

use anyhow::bail;

use super::{Prepared, Runtime};
use crate::cli::{TargetsAction, TargetsArgs, TrustArgs};
use crate::trust::{self, Kind, TrustStore};

/// `load fragments trust <id>`.
pub fn trust_fragment(prep: &Prepared, id: &str) -> crate::Result<()> {
    let Some(cap) = prep.config.fragments.iter().find(|c| c.id == id) else {
        bail!("unknown fragment '{id}'");
    };
    let Some(hashes) = trust::fragment_hashes(cap) else {
        bail!("fragment '{id}' has no script to trust");
    };
    let mut store = load_writable()?;
    store.record(Kind::Fragment, id, &hashes)?;
    println!(
        "trusted fragment '{id}' ({} script hash(es) recorded)",
        hashes.len()
    );
    Ok(())
}

/// `load targets trust <id>` (the only `targets` action for now).
pub fn targets(rt: &Runtime, args: &TargetsArgs) -> crate::Result<()> {
    let prep = super::prepare(rt)?;
    match &args.action {
        TargetsAction::Trust { id } => {
            let Some(t) = prep.config.targets.iter().find(|t| &t.id == id) else {
                bail!("unknown target '{id}'");
            };
            let hashes = trust::target_hashes(t);
            if hashes.is_empty() {
                bail!("target '{id}' has no script predicate to trust");
            }
            let mut store = load_writable()?;
            store.record(Kind::Target, id, &hashes)?;
            println!(
                "trusted target '{id}' ({} script hash(es) recorded)",
                hashes.len()
            );
            Ok(())
        }
    }
}

/// `load trust` (status) / `load trust --rebuild`.
pub fn run(rt: &Runtime, args: &TrustArgs) -> crate::Result<()> {
    if args.rebuild {
        let prep = super::prepare(rt)?;
        let mut store = TrustStore::load_default();
        store.rebuild();
        let mut n = 0usize;
        for cap in &prep.config.fragments {
            if let Some(h) = trust::fragment_hashes(cap) {
                store.record(Kind::Fragment, &cap.id, &h)?;
                n += 1;
            }
        }
        for t in &prep.config.targets {
            let h = trust::target_hashes(t);
            if !h.is_empty() {
                store.record(Kind::Target, &t.id, &h)?;
                n += 1;
            }
        }
        store.save()?; // persist even when n == 0 (clears a corrupt file)
        println!("trust store rebuilt: {n} script-bearing object(s) re-approved");
        return Ok(());
    }
    let store = TrustStore::load_default();
    if store.is_corrupt() {
        println!("trust store: UNREADABLE — run 'load trust --rebuild' to recover");
    } else {
        let n = store.len();
        println!(
            "trust store: {n} entr{} recorded",
            if n == 1 { "y" } else { "ies" }
        );
    }
    Ok(())
}

/// A store you can record into: corrupt state must be surfaced, not papered
/// over (`record()` silently no-ops on a corrupt store by design).
fn load_writable() -> crate::Result<TrustStore> {
    let store = TrustStore::load_default();
    if store.is_corrupt() {
        bail!("trust store is unreadable — run 'load trust --rebuild' to recover");
    }
    Ok(store)
}
