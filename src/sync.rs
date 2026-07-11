//! Cross-machine sync of the global config dir via the `git` CLI (no libgit2).
//!
//! The global config dir (where `config.toml` lives) is a git repo. `config.toml`
//! — your shareable fragments & profiles, secret-free by design — is tracked
//! and syncs; `local.toml` (per-machine hostnames / secret-adjacent params) is
//! gitignored and never leaves the machine. Every network op is **timeout-bounded
//! and non-fatal**: a slow/offline remote degrades to "use the local config",
//! never a hang and never a failed `load run`.
//!
//! Auto-pull (before run/refresh) is throttled by `[sync] pull_max_age`; auto-push
//! (after an apply) is best-effort. `load sync` is the manual force.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context as _, Result};

use crate::config::SyncConfig;

/// The status of an auto-pull, used to drive the `load run` sync line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// Not configured here (auto-pull off, or the dir isn't a synced repo).
    /// No line is printed.
    Disabled,
    /// Skipped the network: the last sync is within `pull_max_age`.
    Skipped { age: Duration },
    /// Pulled new commits from the remote.
    Pulled {
        commits: usize,
        remote: String,
        took: Duration,
    },
    /// Reached the remote; already current.
    UpToDate,
    /// Couldn't reach the remote (offline / timeout / auth) — using local config.
    Offline { last: Option<Duration> },
    /// Local and remote diverged — a manual `load sync` is needed.
    Diverged,
}

/// Outcome of a fast-forward pull.
pub enum PullOutcome {
    /// Fast-forwarded `n` commits (0 = already up to date).
    Pulled(usize),
    /// Remote has commits that don't fast-forward onto local.
    Diverged,
}

/// Outcome of a commit + push.
pub enum PushOutcome {
    /// Pushed (committed first if there were changes).
    Pushed,
    /// Working tree clean and nothing unpushed.
    NothingToPush,
    /// Push rejected — the remote moved ahead; pull first.
    Diverged,
}

/// Outcome of reconciling a diverged branch by rebasing local onto the remote.
pub enum ReconcileOutcome {
    /// Rebased `n` local commits cleanly onto the remote tip (ready to push).
    Rebased(usize),
    /// The rebase hit a content conflict; it was aborted and the working tree
    /// restored to its pre-rebase state — the user must reconcile by hand.
    Conflicted,
}

/// The global config dir we sync (the directory holding `config.toml`).
pub fn config_dir() -> Result<PathBuf> {
    crate::config::global_config_dir()
        .ok_or_else(|| anyhow!("no config directory (could not determine home)"))
}

/// Whether `dir` is a git repo with at least one remote — i.e. `load sync init`
/// (or `clone`) has been run. Auto-pull/push are inert otherwise.
pub fn is_synced(dir: &Path) -> bool {
    dir.join(".git").exists()
        && git(dir, &["remote"], None)
            .map(|o| o.ok && !o.stdout.trim().is_empty())
            .unwrap_or(false)
}

/// Time of the last successful sync (a stamp file we touch on pull/clone/init —
/// more reliable than `FETCH_HEAD`, which `git clone` doesn't always write).
pub fn last_synced(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(stamp_path(dir)).ok()?.modified().ok()
}

fn stamp_path(dir: &Path) -> PathBuf {
    dir.join(".git").join("loadout-sync")
}

/// Record "synced just now" (mtime = now). Best-effort; ignored on failure.
fn touch_stamp(dir: &Path) {
    let _ = std::fs::write(stamp_path(dir), b"");
}

/// A short, human display name for the remote (the repo name, e.g. `loadout-config`).
pub fn remote_name(dir: &Path) -> String {
    git(dir, &["remote", "get-url", "origin"], None)
        .ok()
        .filter(|o| o.ok)
        .map(|o| {
            let url = o.stdout.trim();
            url.rsplit(['/', ':'])
                .next()
                .unwrap_or(url)
                .trim_end_matches(".git")
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "remote".to_string())
}

// --- the throttled auto-pull (the run/render hook) ---------------------------

/// Pull the latest config before a render, honoring `[sync]`: inert if not a
/// synced repo or `auto_pull = false`; skipped when synced within `pull_max_age`;
/// otherwise a timeout-bounded fast-forward pull. **Never fails** — any error maps
/// to `Offline` so the caller proceeds with the on-disk config.
pub fn auto_pull(cfg: &SyncConfig, dir: &Path) -> SyncStatus {
    if !cfg.auto_pull || !is_synced(dir) {
        return SyncStatus::Disabled;
    }
    let age = last_synced(dir).and_then(|t| t.elapsed().ok());
    if let Some(age) = age {
        if age < cfg.pull_max_age {
            return SyncStatus::Skipped { age };
        }
    }
    let start = Instant::now();
    match pull(dir, cfg.timeout) {
        Ok(PullOutcome::Pulled(0)) => SyncStatus::UpToDate,
        Ok(PullOutcome::Pulled(n)) => SyncStatus::Pulled {
            commits: n,
            remote: remote_name(dir),
            took: start.elapsed(),
        },
        Ok(PullOutcome::Diverged) => SyncStatus::Diverged,
        Err(_) => SyncStatus::Offline { last: age },
    }
}

// --- primitive git operations ------------------------------------------------

/// Fast-forward pull, timeout-bounded. Returns how many commits were pulled.
///
/// Robust to a branch with **no upstream tracking** (e.g. a first push that never
/// completed): it fetches, adopts the remote branch if it exists, or — if the
/// remote is empty — reports nothing-to-pull so the subsequent push can publish.
pub fn pull(dir: &Path, timeout: Duration) -> Result<PullOutcome> {
    let branch = current_branch(dir);
    if !has_upstream(dir) {
        let _ = git(dir, &["fetch", "--quiet", "origin"], Some(timeout));
        if remote_branch_exists(dir, &branch) {
            let _ = git(
                dir,
                &[
                    "branch",
                    &format!("--set-upstream-to=origin/{branch}"),
                    &branch,
                ],
                None,
            );
        } else {
            // Origin has no such branch yet — nothing to pull; the push publishes.
            touch_stamp(dir);
            return Ok(PullOutcome::Pulled(0));
        }
    }
    let before = head(dir);
    let out = git(dir, &["pull", "--ff-only", "--no-rebase"], Some(timeout))?;
    if !out.ok {
        if out.stderr.contains("fast-forward") || out.stderr.contains("diverg") {
            return Ok(PullOutcome::Diverged);
        }
        bail!("git pull failed: {}", first_line(&out.stderr));
    }
    touch_stamp(dir);
    let after = head(dir);
    let n = match (before, after) {
        (Some(a), Some(b)) if a != b => count_commits(dir, &a, &b),
        _ => 0,
    };
    Ok(PullOutcome::Pulled(n))
}

/// Reconcile a diverged branch by rebasing the local commits onto the upstream.
///
/// Used **only** by the manual `load sync` (the user is waiting): unlike the
/// fast-forward-only auto-pull on the `run`/`refresh` hot path, this is allowed to
/// rewrite local history, because the common divergence — two machines editing
/// different fragments — rebases cleanly. On a real content conflict the rebase is
/// aborted (restoring the working tree) and `Conflicted` is returned so the caller
/// can punt to hand-reconciliation. Timeout-bounded; assumes an upstream is set
/// (the caller only reaches here after a `pull` reported `Diverged`).
pub fn reconcile_rebase(dir: &Path, timeout: Duration) -> Result<ReconcileOutcome> {
    // `--autostash` lets this reconcile even with uncommitted edits in the working
    // tree (a hand-edited config.toml not yet committed): git stashes them, rebases,
    // then reapplies — without it, git refuses to rebase a dirty tree.
    let pulled = git(
        dir,
        &["pull", "--rebase", "--autostash", "--quiet"],
        Some(timeout),
    );
    if !matches!(&pulled, Ok(o) if o.ok) {
        // The pull didn't finish cleanly — a content conflict, or a timeout / other
        // failure. Abort any in-progress rebase so the repo is never stranded
        // mid-rebase: `git rebase --abort` succeeds *iff* a rebase was actually
        // underway, which is exactly the content-conflict case. (Reading its exit
        // code also avoids coupling to git's private `.git/rebase-*` layout, and
        // covers the timeout path where the rebase started before git was killed.)
        let was_rebasing = git(dir, &["rebase", "--abort"], None)
            .map(|o| o.ok)
            .unwrap_or(false);
        if was_rebasing {
            return Ok(ReconcileOutcome::Conflicted);
        }
        return match pulled {
            Ok(o) => bail!("git pull --rebase failed: {}", first_line(&o.stderr)),
            Err(e) => Err(e.context("git pull --rebase")),
        };
    }
    touch_stamp(dir);
    // After a clean rebase, HEAD = remote tip + our replayed commits, so
    // `@{u}..HEAD` counts exactly the local work now waiting to be pushed.
    let n = count_commits(dir, "@{u}", "HEAD");
    Ok(ReconcileOutcome::Rebased(n))
}

/// Stage tracked changes (the `.gitignore` keeps `local.toml` out), commit if
/// anything changed, and push. Timeout-bounded; returns what happened. Pushes
/// with `-u` so it also establishes tracking on a first publish, and recovers
/// once from GitHub's private-email rejection (GH007) by re-stamping the commit
/// with your GitHub noreply address.
pub fn commit_push(dir: &Path, message: &str, timeout: Duration) -> Result<PushOutcome> {
    // Repos created by older versions managed a shorter ignore list (no
    // `loadout-receipt.json`, no `run/`) and may track those files — refresh
    // the .gitignore and untrack them so the repo heals with this push.
    ensure_gitignore(dir)?;
    untrack_machine_local(dir);
    git(dir, &["add", "-A"], None)?; // respects .gitignore → never stages local.toml
    let staged_clean = git(dir, &["diff", "--cached", "--quiet"], None)?.ok;
    if !staged_clean {
        let c = git(dir, &["commit", "-q", "-m", message], None)?;
        if !c.ok {
            bail!("git commit failed: {}", first_line(&c.stderr));
        }
    }
    // Anything to push? (clean tree AND no unpushed commits → nothing to do.)
    if staged_clean && !has_unpushed(dir) {
        return Ok(PushOutcome::NothingToPush);
    }
    let branch = current_branch(dir);
    let mut out = git(dir, &["push", "-u", "origin", &branch], Some(timeout))?;
    if !out.ok && is_private_email_rejection(&out.stderr) {
        if let Some(noreply) = gh_noreply_email() {
            let _ = set_commit_email(dir, &noreply);
            let _ = amend_reset_author(dir);
            out = git(dir, &["push", "-u", "origin", &branch], Some(timeout))?;
        }
    }
    if !out.ok {
        if out.stderr.contains("rejected") || out.stderr.contains("non-fast-forward") {
            return Ok(PushOutcome::Diverged);
        }
        bail!("git push failed: {}", first_line(&out.stderr));
    }
    touch_stamp(dir);
    Ok(PushOutcome::Pushed)
}

/// The current branch name (or `main` if detached/unknown).
fn current_branch(dir: &Path) -> String {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"], None)
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
        .unwrap_or_else(|| "main".to_string())
}

/// Whether the current branch tracks an upstream.
fn has_upstream(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--abbrev-ref", "@{u}"], None)
        .map(|o| o.ok)
        .unwrap_or(false)
}

/// Whether `origin/<branch>` exists locally (after a fetch).
fn remote_branch_exists(dir: &Path, branch: &str) -> bool {
    git(
        dir,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
        None,
    )
    .map(|o| o.ok)
    .unwrap_or(false)
}

/// GitHub's GH007 ("your push would publish a private email address").
fn is_private_email_rejection(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("gh007") || s.contains("private email")
}

/// Initialize `dir` as the synced config repo: scaffold `.gitignore`, ensure
/// `local.toml`/generated artifacts are untracked, and commit `config.toml`.
/// `remote` (if set) is wired as `origin` and the first commit pushed.
pub fn init(dir: &Path, remote: Option<&str>, timeout: Duration) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    ensure_gitignore(dir)?;
    if !dir.join(".git").exists() {
        run_ok(dir, &["init", "-q"])?;
    }
    // Normalize the branch to `main` regardless of git's `init.defaultBranch`
    // (some setups still default to `master`) and whether the repo pre-existed —
    // we push `main` below, so the local branch must actually be named `main`.
    // `git branch -M main` renames an unborn branch and is a no-op when already
    // on `main`, so it's safe to run unconditionally.
    run_ok(dir, &["branch", "-M", "main"])?;
    // Belt-and-suspenders: never track the private/derived files even if a prior
    // setup added them.
    untrack_machine_local(dir);
    let _ = git(dir, &["add", "config.toml"], None); // may not exist yet
    run_ok(dir, &["add", ".gitignore"])?;
    if !git(dir, &["diff", "--cached", "--quiet"], None)?.ok {
        run_ok(dir, &["commit", "-q", "-m", "loadout: sync config"])?;
    }
    if let Some(url) = remote {
        // Set or replace origin, then push.
        let _ = git(dir, &["remote", "remove", "origin"], None);
        run_ok(dir, &["remote", "add", "origin", url])?;
        let out = git(dir, &["push", "-u", "origin", "main"], Some(timeout))?;
        if !out.ok {
            bail!("git push to {url} failed: {}", first_line(&out.stderr));
        }
    }
    touch_stamp(dir);
    Ok(())
}

/// Clone an existing config repo into `dir`, then ensure a fresh per-machine
/// `local.toml` exists. `dir` need not be empty: loadout's own machine-local
/// files are tolerated — notably `loadout-receipt.json`, which the shell
/// installer writes there *before* `load` ever runs, so on a fresh machine the
/// config dir is never empty. Anything else blocks the clone rather than being
/// silently mixed into the checkout.
pub fn clone(url: &str, dir: &Path, timeout: Duration) -> Result<()> {
    if dir.join(".git").exists() {
        bail!(
            "{} is already a git repo — use `load sync` to pull",
            dir.display()
        );
    }
    let blocking: Vec<String> = match dir.read_dir() {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| !is_machine_local(n))
            .map(|n| n.to_string_lossy().into_owned())
            .collect(),
        // Absent (or unreadable — the clone below will surface that).
        Err(_) => Vec::new(),
    };
    if !blocking.is_empty() {
        bail!(
            "{} already has content ({}) — move it aside, or set LOADOUT_CONFIG_DIR to a fresh path",
            dir.display(),
            blocking.join(", ")
        );
    }
    // `git clone` refuses *any* non-empty target, machine-local files included —
    // so clone into a temp sibling and move the checkout in around them.
    let parent = dir.parent().unwrap_or(dir).to_path_buf();
    std::fs::create_dir_all(&parent).ok();
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "loadout".to_string());
    let tmp = parent.join(format!(".{base}.clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp); // stale leftover from a crashed run
    let out = git_in(
        &parent,
        &["clone", "-q", url, &tmp.to_string_lossy()],
        Some(timeout),
    )?;
    if !out.ok {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git clone {url} failed: {}", first_line(&out.stderr));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for entry in tmp
        .read_dir()
        .with_context(|| format!("reading {}", tmp.display()))?
    {
        let entry = entry?;
        let to = dir.join(entry.file_name());
        // A tracked path colliding with a machine-local file (e.g. a repo that
        // synced `loadout-receipt.json` before it was gitignored) must not
        // clobber THIS machine's copy — the local file wins, and the stale
        // tracked copy is untracked on the next push.
        if to.exists() {
            continue;
        }
        std::fs::rename(entry.path(), &to).with_context(|| {
            format!(
                "moving {} into {}",
                entry.file_name().to_string_lossy(),
                dir.display()
            )
        })?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // local.toml is gitignored and per-machine — make sure one exists to edit.
    let local = dir.join("local.toml");
    if !local.exists() {
        std::fs::write(
            &local,
            "# Private, per-machine config (gitignored). Real hostnames,\n\
             # [host_classes], and [fragment_params] live here.\n",
        )
        .ok();
    }
    ensure_gitignore(dir)?;
    touch_stamp(dir);
    Ok(())
}

/// Machine-local entries that live in the config dir but must never sync:
/// per-machine config (`local.toml`, `bindings.toml`), derived state
/// (`generated/`, `cache/`, `logs/`, studio's `run/`), the self-update
/// timestamp (`update-check`), the shell installer's `loadout-receipt.json`
/// (it records THIS machine's installed version/platform for the updater —
/// synced across machines it breaks self-update), and Finder's `.DS_Store`.
/// One list drives the managed `.gitignore`, the defensive untrack on push,
/// and what `clone` tolerates in a not-quite-empty dir. `true` = directory
/// (rendered with a trailing `/` in `.gitignore`).
const MACHINE_LOCAL: &[(&str, bool)] = &[
    ("local.toml", false),
    ("bindings.toml", false),
    ("generated", true),
    ("cache", true),
    ("logs", true),
    ("run", true),
    ("update-check", false),
    ("loadout-receipt.json", false),
    (".DS_Store", false),
];

/// Whether `name` is one of loadout's own machine-local entries.
fn is_machine_local(name: &std::ffi::OsStr) -> bool {
    MACHINE_LOCAL.iter().any(|(n, _)| name == *n)
}

/// Drop any machine-local entries from the git index (working copies stay put).
/// Belt-and-suspenders on every `init` and push: versions before 0.16 didn't
/// gitignore `loadout-receipt.json` or `run/`, and `commit_push` stages with
/// `add -A`, so repos created back then track them — this heals such a repo the
/// next time any machine pushes.
fn untrack_machine_local(dir: &Path) {
    let mut args = vec!["rm", "--cached", "-r", "--ignore-unmatch", "-q"];
    args.extend(MACHINE_LOCAL.iter().map(|(n, _)| *n));
    let _ = git(dir, &args, None);
}

/// The `.gitignore` for a synced config dir: track `config.toml` + `templates/`,
/// never the private/derived files.
pub fn ensure_gitignore(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    let lines: Vec<String> = MACHINE_LOCAL
        .iter()
        .map(|(n, is_dir)| {
            if *is_dir {
                format!("{n}/")
            } else {
                (*n).to_string()
            }
        })
        .collect();
    let want = format!(
        "# load sync — keep machine-private and generated files out of the repo.\n{}\n",
        lines.join("\n")
    );
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    // Only (re)write if our managed lines aren't all present — e.g. a repo
    // created by an older version that managed a shorter list.
    let missing = lines
        .iter()
        .any(|l| !current.lines().any(|c| c.trim() == l));
    if missing {
        let body = if current.trim().is_empty() {
            want.to_string()
        } else {
            format!("{}\n{want}", current.trim_end())
        };
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

// --- low-level git runner ----------------------------------------------------

struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn head(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "HEAD"], None)
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout.trim().to_string())
}

fn count_commits(dir: &Path, a: &str, b: &str) -> usize {
    git(dir, &["rev-list", "--count", &format!("{a}..{b}")], None)
        .ok()
        .filter(|o| o.ok)
        .and_then(|o| o.stdout.trim().parse().ok())
        .unwrap_or(0)
}

fn has_unpushed(dir: &Path) -> bool {
    // `@{u}..HEAD` is the set of local commits not on the upstream; non-zero ⇒
    // there's something to push. No upstream tracked ⇒ assume yes (first push).
    match git(dir, &["rev-list", "--count", "@{u}..HEAD"], None) {
        Ok(o) if o.ok => o.stdout.trim().parse::<usize>().unwrap_or(0) > 0,
        _ => true,
    }
}

fn run_ok(dir: &Path, args: &[&str]) -> Result<()> {
    let out = git(dir, args, None)?;
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), first_line(&out.stderr));
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// Run `git <args>` with the working directory set to `dir` (the repo root).
fn git(dir: &Path, args: &[&str], timeout: Option<Duration>) -> Result<GitOut> {
    git_in(dir, args, timeout)
}

/// Like [`git`] but spelled out for callers that run from a *parent* dir (e.g.
/// `clone`, whose target doesn't exist yet). `args` are passed verbatim.
fn git_in(cwd: &Path, args: &[&str], timeout: Option<Duration>) -> Result<GitOut> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0") // never block on a credential prompt
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes"); // SSH fails fast, no prompt
    run_capture(cmd, timeout, "git")
}

/// Run `gh <args>` in `cwd`. Inherits gh's own auth.
fn gh_in(cwd: &Path, args: &[&str], timeout: Option<Duration>) -> Result<GitOut> {
    let mut cmd = Command::new("gh");
    cmd.args(args).current_dir(cwd);
    run_capture(cmd, timeout, "gh")
}

/// Spawn `cmd`, capture stdout/stderr, and — with `timeout` — kill the child past
/// the deadline so a hung network op never blocks. `stdin` is null (no prompts).
fn run_capture(mut cmd: Command, timeout: Option<Duration>, name: &str) -> Result<GitOut> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {name} (is it installed?)"))?;

    if let Some(to) = timeout {
        let deadline = Instant::now() + to;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{name} timed out after {}s", to.as_secs());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let out = child.wait_with_output()?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

// --- GitHub (`gh`) integration for `load sync init` ------------------------

/// Whether the GitHub CLI is available to create a repo for the user.
pub fn gh_available() -> bool {
    crate::commands::program_on_path("gh")
}

/// The authenticated user's GitHub **noreply** email (`<id>+<login>@users.
/// noreply.github.com`). Setting the config repo's commit email to this avoids
/// GitHub's "push would publish a private email" (GH007) rejection. `None` if gh
/// isn't authenticated.
pub fn gh_noreply_email() -> Option<String> {
    let out = gh_in(
        Path::new("."),
        &["api", "user", "--jq", ".id, .login"],
        Some(Duration::from_secs(10)),
    )
    .ok()?;
    if !out.ok {
        return None;
    }
    let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let id = lines.next()?;
    let login = lines.next()?;
    Some(format!("{id}+{login}@users.noreply.github.com"))
}

/// Set the config repo's commit email (and a name, if unset) so its commits push
/// cleanly — used with the GitHub noreply address.
pub fn set_commit_email(dir: &Path, email: &str) -> Result<()> {
    run_ok(dir, &["config", "user.email", email])?;
    let has_name = git(dir, &["config", "user.name"], None)
        .map(|o| o.ok && !o.stdout.trim().is_empty())
        .unwrap_or(false);
    if !has_name {
        let _ = git(dir, &["config", "user.name", "loadout"], None);
    }
    Ok(())
}

/// Re-stamp the latest commit with the current identity (after changing the
/// commit email), so the about-to-be-pushed commit uses it.
pub fn amend_reset_author(dir: &Path) -> Result<()> {
    run_ok(dir, &["commit", "--amend", "--reset-author", "--no-edit"])
}

/// Outcome of `gh repo create`.
pub enum GhCreate {
    /// Created and pushed; `url` is the repo's web URL (best-effort).
    Created { url: String },
    /// A repo of that name already exists on the account.
    NameExists,
    /// gh failed for another reason (message for the user).
    Failed(String),
}

/// Create a GitHub repo from the config dir and push, via `gh`.
pub fn gh_create_repo(name: &str, public: bool, dir: &Path, timeout: Duration) -> Result<GhCreate> {
    let vis = if public { "--public" } else { "--private" };
    let out = gh_in(
        dir,
        &[
            "repo", "create", name, vis, "--source", ".", "--remote", "origin", "--push",
        ],
        Some(timeout),
    )?;
    if out.ok {
        // Ensure the branch tracks origin/main so a later bare `load sync` pulls.
        let _ = git(
            dir,
            &["branch", "--set-upstream-to=origin/main", "main"],
            None,
        );
        touch_stamp(dir);
        let url = extract_github_url(&out.stdout)
            .or_else(|| extract_github_url(&out.stderr))
            .unwrap_or_default();
        return Ok(GhCreate::Created { url });
    }
    let msg = format!("{}\n{}", out.stdout, out.stderr);
    if msg.to_lowercase().contains("already exists") {
        return Ok(GhCreate::NameExists);
    }
    Ok(GhCreate::Failed(first_line(&msg).to_string()))
}

/// The web URL of an existing repo on the account (for the "use it" path).
pub fn gh_repo_url(name: &str, dir: &Path) -> Option<String> {
    let out = gh_in(
        dir,
        &["repo", "view", name, "--json", "url", "--jq", ".url"],
        Some(Duration::from_secs(10)),
    )
    .ok()?;
    out.ok
        .then(|| out.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Wire `url` as `origin` (replacing any) and push — for adopting an existing
/// empty repo. Fails clearly if the repo already has diverging history.
pub fn wire_remote_and_push(dir: &Path, url: &str, timeout: Duration) -> Result<()> {
    let _ = git(dir, &["remote", "remove", "origin"], None);
    run_ok(dir, &["remote", "add", "origin", url])?;
    let out = git(dir, &["push", "-u", "origin", "main"], Some(timeout))?;
    if !out.ok {
        if out.stderr.contains("rejected") || out.stderr.contains("non-fast-forward") {
            bail!("that repo isn't empty (its history differs) — pick a new name, or clone it instead");
        }
        bail!("git push to {url} failed: {}", first_line(&out.stderr));
    }
    touch_stamp(dir);
    Ok(())
}

/// Pull the first `https://github.com/...` token out of gh output.
fn extract_github_url(s: &str) -> Option<String> {
    s.split_whitespace()
        .find(|w| w.starts_with("https://github.com/"))
        .map(|w| w.trim_end_matches(['.', ',']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A bare repo to act as the "remote". Its default branch is pinned to
    /// `main` (like GitHub's default, and what `init`/`commit_push` push) so the
    /// remote's HEAD resolves regardless of the host's `init.defaultBranch`.
    fn bare(parent: &Path) -> PathBuf {
        let r = parent.join("remote.git");
        Command::new("git")
            .args(["init", "--bare", "-b", "main", "-q"])
            .arg(&r)
            .status()
            .unwrap();
        r
    }

    /// Give a repo dir a committer identity so `init`/`commit_push` work in any
    /// environment (CI may have none configured).
    fn identify(dir: &Path) {
        for (k, v) in [
            ("user.email", "t@example.test"),
            ("user.name", "loadout test"),
        ] {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["config", k, v])
                .status()
                .unwrap();
        }
    }

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn init_push_clone_edit_pull_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        // Machine A: author config (+ a private local.toml) and `sync init`.
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        fs::write(a.join("local.toml"), "secret = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();
        assert!(is_synced(&a));

        // The remote got config.toml + .gitignore but NOT the private local.toml.
        let tree = Command::new("git")
            .arg("--git-dir")
            .arg(&remote)
            .args(["ls-tree", "--name-only", "HEAD"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&tree.stdout);
        assert!(names.contains("config.toml"));
        assert!(names.contains(".gitignore"));
        assert!(
            !names.contains("local.toml"),
            "local.toml must never sync: {names}"
        );

        // Machine B: clone onto a fresh box.
        let b = tmp.path().join("b");
        clone(url, &b, timeout()).unwrap();
        assert!(is_synced(&b));
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 1\n"
        );
        assert!(
            b.join("local.toml").exists(),
            "a fresh local.toml is created"
        );
        identify(&b);

        // A edits and pushes; B pulls exactly one commit.
        fs::write(a.join("config.toml"), "x = 2\n").unwrap();
        assert!(matches!(
            commit_push(&a, "edit", timeout()).unwrap(),
            PushOutcome::Pushed
        ));
        match pull(&b, timeout()).unwrap() {
            PullOutcome::Pulled(n) => assert_eq!(n, 1),
            PullOutcome::Diverged => panic!("unexpected divergence"),
        }
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 2\n"
        );

        // A second push with nothing changed is a no-op.
        assert!(matches!(
            commit_push(&a, "noop", timeout()).unwrap(),
            PushOutcome::NothingToPush
        ));
    }

    #[test]
    fn auto_pull_honors_disabled_throttle_and_pulls() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();
        let b = tmp.path().join("b");
        clone(url, &b, timeout()).unwrap();
        identify(&b);

        // auto_pull = false → no network, Disabled.
        let off = SyncConfig {
            auto_pull: false,
            ..Default::default()
        };
        assert!(matches!(auto_pull(&off, &b), SyncStatus::Disabled));
        // A non-synced dir → Disabled.
        assert!(matches!(
            auto_pull(&SyncConfig::default(), tmp.path()),
            SyncStatus::Disabled
        ));
        // Throttled (just cloned, huge window) → Skipped, no network.
        let throttled = SyncConfig {
            pull_max_age: Duration::from_secs(9999),
            ..Default::default()
        };
        assert!(matches!(
            auto_pull(&throttled, &b),
            SyncStatus::Skipped { .. }
        ));

        // Force a pull (zero window): A pushes a change, B auto-pulls it.
        fs::write(a.join("config.toml"), "x = 9\n").unwrap();
        commit_push(&a, "edit", timeout()).unwrap();
        let force = SyncConfig {
            pull_max_age: Duration::from_secs(0),
            ..Default::default()
        };
        assert!(matches!(
            auto_pull(&force, &b),
            SyncStatus::Pulled { commits: 1, .. }
        ));
    }

    #[test]
    fn gitignore_excludes_private_and_derived() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_gitignore(tmp.path()).unwrap();
        let gi = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        for line in [
            "local.toml",
            "bindings.toml",
            "generated/",
            "cache/",
            "logs/",
            "run/",
            "update-check",
            "loadout-receipt.json",
            ".DS_Store",
        ] {
            assert!(gi.contains(line), "missing ignore: {line}");
        }
        // Idempotent: a second call preserves existing content and doesn't dupe.
        ensure_gitignore(tmp.path()).unwrap();
        let gi2 = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(gi, gi2);
    }

    #[test]
    fn clone_tolerates_machine_local_files() {
        // The exact fresh-machine sequence: the shell installer writes
        // loadout-receipt.json into the config dir before `load` ever runs,
        // then the user's first command is `load sync clone`.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "a = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();

        let b = tmp.path().join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("loadout-receipt.json"), r#"{"version":"0.15.1"}"#).unwrap();
        fs::write(b.join("update-check"), "123").unwrap();
        clone(url, &b, timeout()).unwrap();

        // The synced content arrived and the machine-local files survived.
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "a = 1\n"
        );
        assert_eq!(
            fs::read_to_string(b.join("loadout-receipt.json")).unwrap(),
            r#"{"version":"0.15.1"}"#
        );
        assert!(b.join(".git").exists());
        assert!(b.join("local.toml").exists());
        // No temp clone dir left behind.
        assert!(!tmp.path().join(".b.clone-tmp").exists());
        assert_eq!(
            tmp.path()
                .read_dir()
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".b.clone-"))
                .count(),
            0
        );
    }

    #[test]
    fn clone_refuses_real_content() {
        // Anything that isn't loadout's own machine-local state still blocks —
        // an existing config.toml must not be silently mixed into a checkout.
        let tmp = tempfile::tempdir().unwrap();
        let b = tmp.path().join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("config.toml"), "x = 1\n").unwrap();
        fs::write(b.join("loadout-receipt.json"), "{}").unwrap();

        let err = clone("unused-url", &b, timeout()).unwrap_err();
        let msg = err.to_string();
        // Names the offender, and only the offender.
        assert!(msg.contains("config.toml"), "unexpected: {msg}");
        assert!(!msg.contains("loadout-receipt.json"), "unexpected: {msg}");
        // And nothing was touched.
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 1\n"
        );
        assert!(!b.join(".git").exists());
    }

    #[test]
    fn clone_keeps_local_receipt_over_a_synced_one() {
        // Repos created before loadout-receipt.json was gitignored may track a
        // receipt (add -A picked it up). The cloning machine's own receipt must
        // win — the tracked copy describes some *other* machine's install.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "a = 1\n").unwrap();
        fs::write(a.join("loadout-receipt.json"), r#"{"machine":"a"}"#).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        // Simulate the old behavior: force-track the receipt despite init's
        // gitignore, then publish it.
        init(&a, Some(url), timeout()).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["add", "-f", "loadout-receipt.json"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["commit", "-q", "-m", "old version tracked the receipt"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["push", "-q", "origin", "main"])
            .status()
            .unwrap();

        let b = tmp.path().join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("loadout-receipt.json"), r#"{"machine":"b"}"#).unwrap();
        clone(url, &b, timeout()).unwrap();

        assert_eq!(
            fs::read_to_string(b.join("loadout-receipt.json")).unwrap(),
            r#"{"machine":"b"}"#
        );
    }

    #[test]
    fn push_untracks_machine_local_files() {
        // A repo that already synced a receipt (or studio run-state) heals on
        // its next push: the files leave the index and the remote, but the
        // working copies stay.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "a = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();
        fs::create_dir_all(a.join("run")).unwrap();
        fs::write(a.join("run/studio-1234.json"), "{}").unwrap();
        fs::write(a.join("loadout-receipt.json"), "{}").unwrap();
        for path in ["run/studio-1234.json", "loadout-receipt.json"] {
            Command::new("git")
                .arg("-C")
                .arg(&a)
                .args(["add", "-f", path])
                .status()
                .unwrap();
        }
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["commit", "-q", "-m", "old version tracked machine state"])
            .status()
            .unwrap();

        assert!(matches!(
            commit_push(&a, "heal", timeout()).unwrap(),
            PushOutcome::Pushed
        ));

        let tracked = Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["ls-files"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&tracked.stdout);
        assert!(!tracked.contains("loadout-receipt.json"), "{tracked}");
        assert!(!tracked.contains("run/"), "{tracked}");
        // Working copies survive the untrack.
        assert!(a.join("loadout-receipt.json").exists());
        assert!(a.join("run/studio-1234.json").exists());
    }

    #[test]
    fn reconcile_rebase_replays_nonoverlapping_divergence() {
        // The everyday case: two machines edit *different* lines. A manual sync
        // must rebase the local commit onto the remote and end up with both edits.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "a = 1\n\nb = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();

        let b = tmp.path().join("b");
        clone(url, &b, timeout()).unwrap();
        identify(&b);

        // A edits one line and publishes.
        fs::write(a.join("config.toml"), "a = 2\n\nb = 1\n").unwrap();
        assert!(matches!(
            commit_push(&a, "edit a", timeout()).unwrap(),
            PushOutcome::Pushed
        ));

        // B edits a *different* line; its push is rejected (it committed locally).
        fs::write(b.join("config.toml"), "a = 1\n\nb = 2\n").unwrap();
        assert!(matches!(
            commit_push(&b, "edit b", timeout()).unwrap(),
            PushOutcome::Diverged
        ));
        // A plain ff pull confirms the divergence.
        assert!(matches!(
            pull(&b, timeout()).unwrap(),
            PullOutcome::Diverged
        ));

        // Reconcile: B's one commit replays cleanly onto A's, yielding both edits.
        assert!(matches!(
            reconcile_rebase(&b, timeout()).unwrap(),
            ReconcileOutcome::Rebased(1)
        ));
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "a = 2\n\nb = 2\n"
        );
        // Now B can publish, and A pulls the union.
        assert!(matches!(
            commit_push(&b, "noop", timeout()).unwrap(),
            PushOutcome::Pushed
        ));
        assert!(matches!(
            pull(&a, timeout()).unwrap(),
            PullOutcome::Pulled(1)
        ));
        assert_eq!(
            fs::read_to_string(a.join("config.toml")).unwrap(),
            "a = 2\n\nb = 2\n"
        );
    }

    #[test]
    fn reconcile_rebase_autostashes_uncommitted_edits() {
        // `load sync` runs reconcile *before* committing, so a hand-edited (still
        // uncommitted) config.toml must not block the rebase — `--autostash` stashes
        // it, rebases onto the remote, and reapplies it.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "a = 1\n\nb = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();

        let b = tmp.path().join("b");
        clone(url, &b, timeout()).unwrap();
        identify(&b);

        // A publishes a committed edit; B has an *uncommitted* edit to a different
        // line and a committed one too, so its branch both diverges and is dirty.
        fs::write(a.join("config.toml"), "a = 2\n\nb = 1\n").unwrap();
        commit_push(&a, "edit a", timeout()).unwrap();
        fs::write(b.join("config.toml"), "a = 1\n\nb = 2\n").unwrap();
        commit_push(&b, "commit b", timeout()).unwrap(); // commits b=2, push rejected
        fs::write(b.join("config.toml"), "a = 1\n\nb = 3\n").unwrap(); // now dirty
        assert!(matches!(
            pull(&b, timeout()).unwrap(),
            PullOutcome::Diverged
        ));

        // Reconcile succeeds despite the dirty tree, and the uncommitted edit survives.
        assert!(matches!(
            reconcile_rebase(&b, timeout()).unwrap(),
            ReconcileOutcome::Rebased(1)
        ));
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "a = 2\n\nb = 3\n"
        );
    }

    #[test]
    fn reconcile_rebase_aborts_cleanly_on_conflict() {
        // Both machines change the *same* line: the rebase must abort and restore
        // the local state, reporting Conflicted (no half-finished rebase left).
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, Some(url), timeout()).unwrap();

        let b = tmp.path().join("b");
        clone(url, &b, timeout()).unwrap();
        identify(&b);

        fs::write(a.join("config.toml"), "x = 2\n").unwrap();
        commit_push(&a, "a", timeout()).unwrap();

        fs::write(b.join("config.toml"), "x = 3\n").unwrap();
        assert!(matches!(
            commit_push(&b, "b", timeout()).unwrap(),
            PushOutcome::Diverged
        ));
        // B's local commit — the state an aborted rebase must restore to.
        let b_head_before = head(&b);

        assert!(matches!(
            reconcile_rebase(&b, timeout()).unwrap(),
            ReconcileOutcome::Conflicted
        ));
        // No rebase left in progress; HEAD and the file are exactly as before.
        let git_dir = b.join(".git");
        assert!(
            !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists(),
            "rebase left in progress after abort"
        );
        assert_eq!(head(&b), b_head_before);
        assert_eq!(
            fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 3\n"
        );
        let porcelain = git(&b, &["status", "--porcelain"], None).unwrap();
        assert!(
            porcelain.stdout.trim().is_empty(),
            "tree not clean after abort"
        );
    }

    #[test]
    fn not_synced_without_a_remote() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(tmp.path())
            .status()
            .unwrap();
        // A git repo with no remote isn't "synced".
        assert!(!is_synced(tmp.path()));
    }

    #[test]
    fn sync_recovers_from_remote_set_but_no_upstream() {
        // The orphan state from a first push that failed (e.g. GH007): origin is
        // wired but the branch has no upstream. `pull` must be a graceful no-op,
        // and `commit_push` must establish tracking and publish.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        init(&a, None, timeout()).unwrap(); // local repo, no remote yet
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["remote", "add", "origin", remote.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(is_synced(&a)); // a remote is wired, but no upstream tracking

        // pull: no upstream + empty remote → graceful no-op (not an error).
        assert!(matches!(
            pull(&a, timeout()).unwrap(),
            PullOutcome::Pulled(0)
        ));
        // push establishes tracking (`-u`) and publishes.
        assert!(matches!(
            commit_push(&a, "first", timeout()).unwrap(),
            PushOutcome::Pushed
        ));
        // now a normal pull works (upstream is set) and is a clean no-op.
        assert!(matches!(
            pull(&a, timeout()).unwrap(),
            PullOutcome::Pulled(0)
        ));
    }

    #[test]
    fn extract_github_url_from_gh_output() {
        let s = "✓ Created repository elleryfamilia/loadout-config on github.com\n  \
                 https://github.com/elleryfamilia/loadout-config\n";
        assert_eq!(
            extract_github_url(s).as_deref(),
            Some("https://github.com/elleryfamilia/loadout-config")
        );
        // Trailing punctuation is trimmed; no URL → None.
        assert_eq!(
            extract_github_url("see https://github.com/a/b.").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(extract_github_url("nothing here"), None);
    }
}
