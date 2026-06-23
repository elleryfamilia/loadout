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
    Ok(())
}
