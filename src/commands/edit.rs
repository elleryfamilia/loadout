//! `load edit [name]` — open your global config in `$EDITOR`.
//!
//! Loadouts and fragments live in hand-editable TOML (`config.toml`), so editing
//! is just opening that file. When a name is given it's validated first — a typo
//! shouldn't silently open the file with nothing to fix — and the kind (loadout
//! or fragment) is echoed so you know what to look for. Honors `$VISUAL` then
//! `$EDITOR`, falling back to `vi`.

use anyhow::{anyhow, bail, Context as _};

use super::Runtime;
use crate::cli::EditArgs;
use crate::config::{self, Config};
use crate::context;

/// Entry point for `load edit`.
pub fn run(rt: &Runtime, args: &EditArgs) -> crate::Result<()> {
    let dir = config::global_config_dir()
        .ok_or_else(|| anyhow!("no home directory to resolve the global config"))?;
    let path = dir.join("config.toml");

    if let Some(name) = &args.name {
        let repo_base = context::repo_base_for(&rt.cwd);
        let config = Config::load(&repo_base).context("loading configuration")?;
        let is_loadout = config.profiles.iter().any(|p| &p.name == name);
        let is_fragment = config.fragments.iter().any(|f| &f.id == name);
        if !is_loadout && !is_fragment {
            bail!(
                "no loadout or fragment named '{name}' — see `load list` / `load list fragments`"
            );
        }
        let kind = if is_loadout { "loadout" } else { "fragment" };
        println!("opening config — look for the {kind} '{name}'.");
    }

    if !path.exists() {
        bail!(
            "no global config at {} yet — run `load studio` to create one",
            path.display()
        );
    }

    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());

    if rt.dry_run {
        println!("would open {} in {editor}", path.display());
        return Ok(());
    }

    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching editor '{editor}'"))?;
    if !status.success() {
        bail!("editor '{editor}' exited without success ({status})");
    }

    // The explicit edit is the approval: re-record trust for every
    // script-bearing object defined in the file that was just edited (the
    // global config layer). Best-effort — trust must never fail the edit
    // itself.
    if let Ok(config) = Config::load(&context::repo_base_for(&rt.cwd)) {
        let mut store = crate::trust::TrustStore::load_default();
        for cap in &config.fragments {
            if cap.origin == crate::fragment::Layer::Global {
                if let Some(h) = crate::trust::fragment_hashes(cap) {
                    let _ = store.record(crate::trust::Kind::Fragment, &cap.id, &h);
                }
            }
        }
        for t in &config.targets {
            if t.origin == crate::fragment::Layer::Global && t.rule.has_script() {
                let _ = store.record(
                    crate::trust::Kind::Target,
                    &t.id,
                    &crate::trust::target_hashes(t),
                );
            }
        }
    }

    Ok(())
}
