//! The `artefacto` binary, which owns the plan artifact: `load plan` is a
//! dispatcher over it. This module knows where the binary is, which version
//! it is, whether that version is one this loadout was tested with, and how
//! to install it with the user's consent.
//!
//! Loadout never bundles artefacto. It looks for it on PATH (or at
//! [`BIN_ENV`]), offers to install it through artefacto's own cargo-dist
//! installer when a command needs it and the terminal can ask, reports it in
//! `load doctor`, and updates it in `load update` through the same updater
//! loadout uses for itself, off artefacto's own install receipt.

use std::io::{IsTerminal, Write};
use std::process::Command;

use anyhow::{bail, Context};

use crate::providers::{parse_version, probe_cli, CliProbe};
use crate::style::Painter;

/// The artefacto version this loadout was tested against. A different major
/// or minor on the machine is reported by `load doctor`, never refused.
pub const TESTED_VERSION: &str = "0.1.0";

/// artefacto's cargo-dist installer: the one-liner `load plan` offers to run.
pub const INSTALLER_URL: &str =
    "https://github.com/elleryfamilia/artefacto/releases/latest/download/artefacto-installer.sh";

/// Names the binary to run instead of `artefacto` on PATH. Tests point it at
/// a script; a user can point it at a build.
pub const BIN_ENV: &str = "LOADOUT_ARTEFACTO_BIN";

/// A local installer script to run instead of downloading [`INSTALLER_URL`].
/// A test hook, so the consent flow can be driven without the network.
pub const INSTALLER_ENV: &str = "LOADOUT_ARTEFACTO_INSTALLER";

/// The program `load plan` runs.
pub fn program() -> String {
    std::env::var(BIN_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "artefacto".to_string())
}

/// Whether the binary answers, and with which version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Found { version: String },
    Missing,
    TimedOut,
}

/// Run `artefacto --version` under the probe deadline.
pub fn probe() -> Presence {
    match probe_cli(&program()) {
        CliProbe::Found(raw) => Presence::Found {
            version: parse_version(&raw),
        },
        CliProbe::Missing => Presence::Missing,
        CliProbe::TimedOut => Presence::TimedOut,
    }
}

/// A note when `version` is not one this loadout was tested with: a
/// different major or minor. A patch difference is silent.
pub fn version_note(version: &str) -> Option<String> {
    if major_minor(version) == major_minor(TESTED_VERSION) {
        return None;
    }
    Some(format!(
        "artefacto {version} found; this loadout was tested with {TESTED_VERSION} — \
         `load plan` should still work, but a mismatch is the first thing to suspect"
    ))
}

fn major_minor(version: &str) -> (u64, u64) {
    let mut parts = version.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// A command for the binary, arguments to be added by the caller.
pub fn command() -> Command {
    Command::new(program())
}

/// How to install artefacto by hand.
pub fn install_hint() -> String {
    format!("curl -LsSf {INSTALLER_URL} | sh")
}

/// Run artefacto's installer through `sh`. Consent is the caller's business.
pub fn install() -> crate::Result<()> {
    let line = match std::env::var(INSTALLER_ENV) {
        Ok(path) if !path.is_empty() => format!("sh '{}'", path.replace('\'', "'\\''")),
        _ => install_hint(),
    };
    let status = Command::new("sh")
        .arg("-c")
        .arg(&line)
        .status()
        .context("running the artefacto installer")?;
    if !status.success() {
        bail!("the artefacto installer exited with {status}");
    }
    Ok(())
}

/// Offer the install on a terminal, and run it on a yes. Off a terminal,
/// say how to install and decline. Returns whether the binary answers now.
pub fn offer_install(p: &Painter) -> crate::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "artefacto is not installed; `load plan` needs it. Install it with:\n  {}",
            install_hint()
        );
        return Ok(false);
    }
    println!();
    println!(
        "  {} {} needs {}, the tool that renders and reviews plans.",
        p.cyan("✦"),
        p.bold("load plan"),
        p.bold("artefacto")
    );
    println!(
        "    it installs into ~/.cargo/bin from {}",
        p.dim(INSTALLER_URL)
    );
    print!("  install it now? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !line.trim().to_ascii_lowercase().starts_with('y') {
        println!("  not installed. Later: {}", install_hint());
        return Ok(false);
    }
    install()?;
    match probe() {
        Presence::Found { version } => {
            println!("  {} artefacto {version} installed", p.green("✓"));
            Ok(true)
        }
        _ => {
            crate::warn_user!(
                "the installer finished but `artefacto --version` does not answer; is ~/.cargo/bin on your PATH?"
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_difference_is_silent_and_a_minor_one_is_noted() {
        assert_eq!(version_note(TESTED_VERSION), None);
        assert_eq!(version_note("0.1.9"), None);
        let note = version_note("0.2.0").expect("a minor bump is noted");
        assert!(
            note.contains("0.2.0") && note.contains(TESTED_VERSION),
            "{note}"
        );
        assert!(version_note("1.0.0").is_some());
        assert!(
            version_note("garbage").is_some(),
            "unparseable reads as 0.0"
        );
    }

    #[test]
    fn the_hint_names_the_installer() {
        assert_eq!(install_hint(), format!("curl -LsSf {INSTALLER_URL} | sh"));
        assert!(INSTALLER_URL.starts_with("https://github.com/elleryfamilia/artefacto/"));
    }

    #[test]
    fn the_program_is_artefacto_unless_named() {
        // Read through the env var directly rather than setting it: tests
        // share one process, and a set var is a data race with every other
        // test that reads the environment.
        match std::env::var(BIN_ENV) {
            Ok(v) if !v.is_empty() => assert_eq!(program(), v),
            _ => assert_eq!(program(), "artefacto"),
        }
    }
}
