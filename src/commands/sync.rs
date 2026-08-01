//! `load sync` — git-backed sync of the global config (fragments & profiles)
//! across machines. `init` sets a machine up, `clone` onboards a new one, and a
//! bare `load sync` pulls the latest and pushes local edits.

use std::io::{IsTerminal, Write as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

use crate::cli::{SyncAction, SyncArgs};
use crate::providers::{probe_cli, CliProbe};
use crate::style::Painter;
use crate::sync::{self, GhCreate, PullOutcome, PushOutcome, ReconcileOutcome};

/// Manual sync ops are interactive — the user is waiting — so give them a roomy
/// timeout. (Auto-pull on the `run` hot path uses the short `[sync] timeout`.)
const MANUAL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run(rt: &super::Runtime, args: &SyncArgs) -> Result<()> {
    ensure_git("git")?;
    let dir = sync::config_dir()?;
    let p = Painter::auto();
    let result = match &args.action {
        Some(SyncAction::Init(a)) => init_flow(&dir, a.remote.as_deref(), &p),
        Some(SyncAction::Clone(a)) => {
            sync::clone(&a.url, &dir, MANUAL_TIMEOUT).context("cloning the config repo")?;
            println!(
                "{} cloned your config into {}",
                p.green("✓"),
                p.dim(&dir.display().to_string())
            );
            println!(
                "{}",
                p.dim("  a fresh local.toml was created for this machine (gitignored).")
            );
            Ok(())
        }
        None => sync_now(&dir, &p),
    };
    // Passive hook bootstrap, after the sync so a fresh machine's first
    // `load sync clone` loads the just-synced config and leaves the IDE
    // freshness hooks of installed agents (e.g. Cursor) wired too.
    if result.is_ok() {
        let repo_base = crate::context::repo_base_for(&rt.cwd);
        if let Ok(config) = crate::config::Config::load(&repo_base) {
            for note in crate::adapters::bootstrap_hook_registrations(&config, rt.dry_run) {
                println!("{} {}", p.green("✓"), p.dim(&note));
            }
        }
    }
    result
}

/// `load sync init [url]`: set the config dir up as a synced repo. With a URL,
/// wire it non-interactively. Without one, offer to create + push a GitHub repo
/// via `gh` (interactive), or print manual guidance.
fn init_flow(dir: &Path, remote_arg: Option<&str>, p: &Painter) -> Result<()> {
    sync::init(dir, remote_arg, MANUAL_TIMEOUT).context("setting up the config repo")?;

    if remote_arg.is_some() {
        println!(
            "{} config repo ready · pushed to {}",
            p.green("✓"),
            p.dim(&sync::remote_name(dir))
        );
        print_tracked(p);
        return Ok(());
    }

    // No remote given. If one is already wired (e.g. a previous init), there's
    // nothing to create — just point at `load sync`.
    if sync::is_synced(dir) {
        println!(
            "{} already set up · origin {} · run `load sync` to publish/pull",
            p.green("✓"),
            p.dim(&sync::remote_name(dir))
        );
        return Ok(());
    }

    println!("{} config dir is now a git repo.", p.green("✓"));
    print_tracked(p);

    if sync::gh_available() && interactive() {
        offer_gh(dir, p)
    } else {
        println!(
            "{}",
            p.dim("  publish it: `load sync init <url>`  (or: gh repo create <name> --source . --push)")
        );
        Ok(())
    }
}

/// Interactive `gh repo create` flow: name + visibility, with recovery for a
/// name collision and GitHub's private-email push rejection.
fn offer_gh(dir: &Path, p: &Painter) -> Result<()> {
    if !prompt_yes("  Create a GitHub repo with gh and push now?", true)? {
        println!(
            "{}",
            p.dim("  ok — publish later with `load sync init <url>`.")
        );
        return Ok(());
    }

    // GitHub rejects a push that would publish a private commit email (GH007).
    // Stamp the config repo's commits with your GitHub noreply address so the
    // push just works — config-repo commits don't need your real email.
    if let Some(noreply) = sync::gh_noreply_email() {
        let _ = sync::set_commit_email(dir, &noreply);
        let _ = sync::amend_reset_author(dir);
    }

    let mut name = prompt_line("  Repo name", "loadout-config")?;
    // config.toml is secret-free by design, so public is a safe, zero-auth option.
    let public = prompt_yes(
        "  Make it public? (config.toml carries no secrets; public = no git auth on other boxes)",
        false,
    )?;

    loop {
        match sync::gh_create_repo(&name, public, dir, MANUAL_TIMEOUT)? {
            GhCreate::Created { url } => {
                let shown = if url.is_empty() { name.clone() } else { url };
                println!(
                    "{} created {} repo · pushed · {}",
                    p.green("✓"),
                    if public { "public" } else { "private" },
                    p.dim(&shown)
                );
                return Ok(());
            }
            GhCreate::NameExists => {
                println!(
                    "{} a repo named “{name}” already exists on your account.",
                    p.yellow("!")
                );
                if prompt_yes("  Use it (push into it)?  (No = pick a new name)", true)? {
                    let url = sync::gh_repo_url(&name, dir)
                        .ok_or_else(|| anyhow::anyhow!("couldn't resolve the URL for “{name}”"))?;
                    sync::wire_remote_and_push(dir, &url, MANUAL_TIMEOUT)?;
                    println!("{} pushed to {}", p.green("✓"), p.dim(&url));
                    return Ok(());
                }
                name = prompt_line("  New repo name", "")?;
                if name.is_empty() {
                    bail!("no repo name given");
                }
            }
            GhCreate::Failed(e) => bail!(
                "gh repo create failed: {e}\n  check `gh auth login`, then retry — \
                 or set up the remote by hand: `load sync init <url>`"
            ),
        }
    }
}

/// Every sync op shells out to git, but a fresh machine (a minimal Debian or
/// container image) may not have it installed. Catch that up front with install
/// guidance instead of failing mid-operation with a raw spawn error. `program`
/// is a parameter only so tests can probe a name that never exists; real
/// callers pass "git". A probe timeout means git exists but is slow — proceed,
/// the ops have their own timeouts.
fn ensure_git(program: &str) -> Result<()> {
    if matches!(probe_cli(program), CliProbe::Missing) {
        bail!(
            "`load sync` needs git, which wasn't found on this machine — install it and re-run \
             (Debian/Ubuntu: `apt install git`, macOS: `xcode-select --install`)"
        );
    }
    Ok(())
}

/// Whether we can prompt (both stdin and stdout are a terminal).
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn print_tracked(p: &Painter) {
    println!(
        "{}",
        p.dim(
            "  tracked: config.toml + templates/   ignored: local.toml, generated/, cache/, logs/"
        )
    );
}

/// A yes/no prompt with a default.
fn prompt_yes(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim().to_ascii_lowercase();
    Ok(if t.is_empty() {
        default_yes
    } else {
        t.starts_with('y')
    })
}

/// A line prompt with a default shown in brackets.
fn prompt_line(question: &str, default: &str) -> Result<String> {
    print!("{question} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim();
    Ok(if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    })
}

fn sync_now(dir: &Path, p: &Painter) -> Result<()> {
    if !sync::is_synced(dir) {
        bail!(
            "config isn't set up for sync yet — run `load sync init [remote-url]` \
             (or `load sync clone <url>` on a new machine) first"
        );
    }
    let remote = sync::remote_name(dir);

    match sync::pull(dir, MANUAL_TIMEOUT).context("pulling from the remote")? {
        PullOutcome::Pulled(0) => {
            println!("{} already up to date · {}", p.green("✓"), p.dim(&remote))
        }
        PullOutcome::Pulled(n) => println!(
            "{} pulled {} · {}",
            p.green("✓"),
            changes(n),
            p.dim(&remote)
        ),
        PullOutcome::Diverged => {
            // The manual sync can safely reconcile (the hot-path auto-pull can't):
            // rebase local edits onto the remote, and only punt to a hand-merge if
            // the two sides actually touched the same lines.
            match sync::reconcile_rebase(dir, MANUAL_TIMEOUT)
                .context("reconciling with the remote")?
            {
                ReconcileOutcome::Rebased(0) => {
                    println!("{} reconciled with {}", p.green("✓"), p.dim(&remote))
                }
                ReconcileOutcome::Rebased(n) => println!(
                    "{} reconciled · replayed {} onto {}",
                    p.green("✓"),
                    changes(n),
                    p.dim(&remote)
                ),
                ReconcileOutcome::Conflicted => bail!(
                    "local and remote changed the same lines — reconcile by hand in {} \
                     (e.g. `git -C {} pull --rebase`, fix the conflicts, then `load sync`)",
                    dir.display(),
                    dir.display()
                ),
            }
        }
    }

    match sync::commit_push(dir, "loadout: sync config", MANUAL_TIMEOUT)
        .context("pushing to the remote")?
    {
        PushOutcome::Pushed => println!("{} pushed your changes", p.green("✓")),
        PushOutcome::NothingToPush => println!("{} nothing to push", p.dim("·")),
        PushOutcome::Diverged => {
            bail!("push rejected — the remote moved ahead; run `load sync` again to pull first")
        }
    }
    Ok(())
}

/// "1 change" / "N changes".
fn changes(n: usize) -> String {
    if n == 1 {
        "1 change".to_string()
    } else {
        format!("{n} changes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_git_yields_install_guidance() {
        let err = ensure_git("loadout-test-no-such-binary").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("needs git"), "unexpected message: {msg}");
        assert!(msg.contains("apt install git"), "unexpected message: {msg}");
    }

    #[test]
    fn present_git_passes_preflight() {
        // git is a hard dev/CI dependency of this repo, so it's always present here.
        assert!(ensure_git("git").is_ok());
    }
}
