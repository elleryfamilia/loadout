//! `load update` — self-update to the latest release via cargo-dist's updater.
//!
//! Works for installs done with the loadout installer (which leaves an install
//! receipt). Other installs (`cargo install`, package managers) report how to
//! switch to an installer-based install instead of erroring.

use super::Runtime;
use crate::cli::UpdateArgs;
use crate::style::Painter;
use crate::update::{self, Outcome};

/// Entry point for `load update`.
pub fn run(_rt: &Runtime, args: &UpdateArgs) -> crate::Result<()> {
    // The detached refresher spawned by the ambient nudge: refresh the verdict
    // cache, print nothing, exit 0. Handled before anything interactive.
    if args.refresh_cache {
        update::refresh_cache();
        return Ok(());
    }
    let p = Painter::auto();
    match update::perform(args.check)? {
        Outcome::Updated { from, to } => {
            let was = from
                .map(|f| format!("{} → ", p.dim(&f)))
                .unwrap_or_default();
            println!("  {} updated loadout {was}{}", p.green("✓"), p.bold(&to));
        }
        Outcome::AlreadyCurrent => println!(
            "  {} loadout {} is the latest release",
            p.green("✓"),
            p.bold(env!("CARGO_PKG_VERSION"))
        ),
        Outcome::UpdateAvailable => println!(
            "  {} a newer loadout is available — run {} to install",
            p.cyan("↑"),
            p.bold("load update")
        ),
        Outcome::NotManaged => {
            println!(
                "  {} this loadout wasn't installed via the loadout installer, so it can't \
                 self-update.",
                p.yellow("⚠")
            );
            println!(
                "    {}",
                p.dim("reinstall with the installer to enable `load update`:")
            );
            println!(
                "    {}",
                p.dim(
                    "curl -LsSf https://github.com/elleryfamilia/loadout/releases/latest/download/loadout-installer.sh | sh"
                )
            );
        }
    }
    update_artefacto(&p, args.check)?;
    Ok(())
}

/// artefacto, when it is here and its own installer put it here: run that
/// installer again, which fetches the latest release, and say what changed.
/// Not axoupdater: that is a self-updater, and asked about a second binary
/// it compares the receipt against the running `load` and answers "current"
/// offline whenever the two live in different places. Absent means nothing
/// to update; no receipt means a copy loadout will not replace.
fn update_artefacto(p: &Painter, check_only: bool) -> crate::Result<()> {
    use crate::artefacto::{self, Presence};
    let version = match artefacto::probe() {
        Presence::Missing => return Ok(()),
        Presence::TimedOut => {
            println!(
                "  {} artefacto is installed but its version probe timed out; not updated",
                p.yellow("⚠")
            );
            return Ok(());
        }
        Presence::Found { version } => version,
    };
    if !artefacto::has_receipt() {
        println!(
            "  {} artefacto {version} wasn't installed via its installer, so `load update` \
             can't update it.",
            p.yellow("⚠")
        );
        println!(
            "    {}",
            p.dim("reinstall with the installer to enable it:")
        );
        println!("    {}", p.dim(&artefacto::install_hint()));
        return Ok(());
    }
    if check_only {
        println!(
            "  {} artefacto {} is installed; {} reinstalls the latest release",
            p.cyan("·"),
            p.bold(&version),
            p.bold("load update")
        );
        return Ok(());
    }
    artefacto::install()?;
    match artefacto::probe() {
        Presence::Found { version: now } if now == version => println!(
            "  {} artefacto {} is the latest release",
            p.green("✓"),
            p.bold(&version)
        ),
        Presence::Found { version: now } => println!(
            "  {} updated artefacto {} → {}",
            p.green("✓"),
            p.dim(&version),
            p.bold(&now)
        ),
        _ => crate::warn_user!(
            "the artefacto installer finished but `artefacto --version` does not answer"
        ),
    }
    Ok(())
}
