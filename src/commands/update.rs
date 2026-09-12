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

/// artefacto, when it is here: the same updater, off artefacto's own install
/// receipt. Absent means nothing to update; `load doctor` says how to get it.
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
    match update::perform_app("artefacto", check_only)? {
        Outcome::Updated { from, to } => {
            let was = from
                .map(|f| format!("{} → ", p.dim(&f)))
                .unwrap_or_default();
            println!("  {} updated artefacto {was}{}", p.green("✓"), p.bold(&to));
        }
        Outcome::AlreadyCurrent => println!(
            "  {} artefacto {} is the latest release",
            p.green("✓"),
            p.bold(&version)
        ),
        Outcome::UpdateAvailable => println!(
            "  {} a newer artefacto is available — run {} to install",
            p.cyan("↑"),
            p.bold("load update")
        ),
        Outcome::NotManaged => {
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
        }
    }
    Ok(())
}
