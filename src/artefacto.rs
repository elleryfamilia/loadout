//! The `artefacto` binary, which owns the plan artifact: `load plan` is a
//! dispatcher over it. This module knows where the binary is, which version
//! it is, whether that version is one this loadout was tested with, and how
//! to install it with the user's consent.
//!
//! Loadout never bundles artefacto. It looks for it on PATH (or at
//! [`BIN_ENV`]), offers to install it through artefacto's own cargo-dist
//! installer when a command needs it and the terminal can ask, reports it in
//! `load doctor`, and in `load update` reruns that installer — into the
//! directory artefacto's install receipt names — when the receipt shows the
//! installer put it there.

use std::io::{IsTerminal, Write};
use std::process::Command;

use anyhow::{bail, Context};

use crate::providers::parse_version;
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

/// Run `artefacto --version` under doctor's probe deadline (three seconds,
/// or `LOADOUT_PROBE_TIMEOUT_MS`).
pub fn probe() -> Presence {
    probe_within(crate::providers::probe_timeout())
}

/// How long a `load plan` verb waits for `artefacto --version` before
/// deciding the binary is wedged. Longer than doctor's deadline on purpose:
/// doctor reports, a verb is about to do work, and a slow first start on a
/// loaded machine must not fail a push. `LOADOUT_PROBE_TIMEOUT_MS` still
/// overrides it.
pub const VERB_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// [`probe`] under a deadline of the caller's choosing.
pub fn probe_within(timeout: std::time::Duration) -> Presence {
    let timeout = std::env::var("LOADOUT_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(timeout);
    let mut cmd = command();
    cmd.arg("--version");
    match crate::providers::output_with_timeout(&mut cmd, timeout) {
        Ok(Some(o)) if o.status.success() || !o.stdout.is_empty() => Presence::Found {
            version: parse_version(&String::from_utf8_lossy(&o.stdout)),
        },
        Ok(Some(_)) => Presence::Missing,
        Ok(None) => Presence::TimedOut,
        Err(_) => Presence::Missing,
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

/// The plan skill artefacto ships, as `(relative path, contents)` pairs
/// from `artefacto skill --print`, with the skill's own directory stripped
/// from each path. `None` when the binary is missing, does not answer within
/// the probe deadline, or prints something that is not the manifest.
pub fn skill_manifest() -> Option<Vec<(String, String)>> {
    let mut cmd = command();
    cmd.args(["skill", "--print"]);
    let out = crate::providers::output_with_timeout(&mut cmd, crate::providers::probe_timeout())
        .ok()
        .flatten()?;
    if !out.status.success() {
        return None;
    }
    let manifest: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if manifest["format"].as_str() != Some("artefacto.skill/1") {
        return None;
    }
    let skill = manifest["skills"]
        .as_array()?
        .iter()
        .find(|s| s["name"].as_str() == Some(crate::skills::ARTEFACTO_PLAN_ID))?;
    let prefix = format!("{}/", crate::skills::ARTEFACTO_PLAN_ID);
    let files: Vec<(String, String)> = skill["files"]
        .as_array()?
        .iter()
        .filter_map(|f| {
            let path = f["path"].as_str()?;
            let contents = f["contents"].as_str()?;
            let relpath = path.strip_prefix(&prefix).unwrap_or(path);
            Some((relpath.to_string(), contents.to_string()))
        })
        .collect();
    // SKILL.md first: the skill lifecycle reads the marker from files[0].
    let mut ordered: Vec<(String, String)> = files
        .iter()
        .filter(|(p, _)| p == "SKILL.md")
        .cloned()
        .collect();
    ordered.extend(files.into_iter().filter(|(p, _)| p != "SKILL.md"));
    (!ordered.is_empty() && ordered[0].0 == "SKILL.md").then_some(ordered)
}

/// A command for the binary, arguments to be added by the caller.
pub fn command() -> Command {
    Command::new(program())
}

/// Where cargo-dist's installer leaves its receipt: `artefacto/artefacto-receipt.json`
/// under `$XDG_CONFIG_HOME`, else under `~/.config`.
fn receipt_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| crate::config::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("artefacto").join("artefacto-receipt.json"))
}

/// Whether artefacto's own installer put it here. `load update` reinstalls
/// through the same installer only when it did; a build or a package
/// manager's copy is not loadout's to replace.
pub fn has_receipt() -> bool {
    receipt_path().is_some_and(|p| p.is_file())
}

/// What the receipt says about where the binary went and whether PATH was
/// touched, so a reinstall lands in the same place and does the same thing.
/// `None` for no receipt or one this binary cannot read.
fn receipt_install(path: &std::path::Path) -> Option<(Option<String>, Option<bool>)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some((
        v["install_prefix"].as_str().map(str::to_string),
        v["modify_path"].as_bool(),
    ))
}

/// How to install artefacto by hand.
pub fn install_hint() -> String {
    format!("curl -LsSf {INSTALLER_URL} | sh")
}

/// Run artefacto's installer through `sh`. Consent is the caller's business.
/// When a receipt names an install directory, the installer is told to use
/// it again (`ARTEFACTO_INSTALL_DIR`, which cargo-dist installers honour),
/// and told to leave PATH alone if the first install did; otherwise a second
/// copy could land in `~/.cargo/bin` beside the one the user chose.
pub fn install() -> crate::Result<()> {
    let line = match std::env::var(INSTALLER_ENV) {
        Ok(path) if !path.is_empty() => format!("sh '{}'", path.replace('\'', "'\\''")),
        _ => install_hint(),
    };
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&line);
    if let Some((prefix, modify_path)) = receipt_path().as_deref().and_then(receipt_install) {
        if let Some(prefix) = prefix {
            cmd.env("ARTEFACTO_INSTALL_DIR", prefix);
        }
        if modify_path == Some(false) {
            cmd.env("ARTEFACTO_NO_MODIFY_PATH", "1");
        }
    }
    let status = cmd.status().context("running the artefacto installer")?;
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
