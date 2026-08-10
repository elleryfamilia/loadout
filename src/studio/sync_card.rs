//! The studio **Sync** card — set up, inspect, and run config-repo sync
//! without leaving the UI.
//!
//! Unlike the rest of Settings (which stages every write through the
//! [`crate::studio::edit::Session`] pipeline), every action here is an
//! **immediate side effect** on the git repo in the global config dir — the
//! same shape as the agent-skill card. Nothing is staged, and Discard undoes
//! none of it.
//!
//! Every handler is a thin wrapper over [`crate::sync`]; no sync logic lives
//! here. Each has a `*_at` core taking an explicit dir so tests drive it
//! against a temp repo instead of the developer's real config.

use std::path::Path;
use std::time::{Duration, SystemTime};

use maud::html;

use crate::studio::server::{Req, Resp};
use crate::studio::views;

/// Everything the card renders, prepared by the handler — no filesystem access
/// inside the view (the rule `settings.rs` follows).
pub(crate) struct SyncView {
    /// The config dir is a git repo with a remote.
    pub synced: bool,
    /// Short display name for the remote; empty when not synced.
    pub remote: String,
    pub last_synced: Option<SystemTime>,
    /// `gh` is installed — enables the one-click GitHub set-up path.
    pub gh: bool,
    /// The config dir holds nothing but machine-local files, so `sync::clone`
    /// would be accepted. Clone is hidden otherwise: it is guaranteed to fail.
    pub dir_empty: bool,
    /// `(is_error, message)` from the action that just ran.
    pub notice: Option<(bool, String)>,
}

pub(crate) fn view_for(dir: &Path, notice: Option<(bool, String)>) -> SyncView {
    let synced = crate::sync::is_synced(dir);
    SyncView {
        synced,
        remote: if synced {
            crate::sync::remote_name(dir)
        } else {
            String::new()
        },
        last_synced: crate::sync::last_synced(dir),
        gh: crate::sync::gh_available(),
        dir_empty: crate::sync::clone_target_is_clear(dir),
        notice,
    }
}

/// "3 minutes ago" / "2 days ago" — coarse on purpose; the exact second is noise.
fn ago(t: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "just now".into();
    };
    let s = d.as_secs();
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} minutes ago", s / 60),
        3600..=86399 => format!("{} hours ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

pub(crate) fn card_fragment(v: &SyncView) -> String {
    let body = html! {
        div class="cmd-block" {
            // Banner markup copied from `settings::page_fragment` so it reuses
            // the existing CSS — `banner error`, and a `banner-body` div.
            @if let Some((is_error, msg)) = &v.notice {
                div class=(if *is_error { "banner error" } else { "banner" }) {
                    span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                    div class="banner-body" { (msg) }
                }
            }
            @if v.synced {
                span class="muted small" {
                    "Your global config syncs with " strong { (v.remote) } "."
                    @if let Some(t) = v.last_synced { " Last synced " (ago(t)) "." }
                }
                button class="btn btn-ghost"
                    hx-post="/sync/now" hx-target="#sync-card" {
                    (views::icon("refresh")) "Sync now"
                }
            } @else {
                span class="muted small" {
                    "Sync keeps your global config in a git repo so every machine you work on \
                     equips the same context. Nothing leaves this machine until you set it up."
                }
                form hx-post="/sync/init" hx-target="#sync-card" {
                    input type="url" name="remote" placeholder="git remote URL (optional)" {}
                    button class="btn btn-ghost" type="submit" {
                        (views::icon("plus"))
                        @if v.gh { "Set up sync (creates a private GitHub repo)" } @else { "Set up sync" }
                    }
                }
                @if v.dir_empty {
                    form hx-post="/sync/clone" hx-target="#sync-card" {
                        input type="url" name="url" placeholder="https://github.com/you/loadout-config" required {}
                        button class="btn btn-ghost" type="submit" {
                            (views::icon("git-branch")) "Clone an existing config"
                        }
                    }
                }
            }
        }
    };
    body.into_string()
}

/// `GET /sync/card` — render the card against the real config dir.
pub fn card() -> Resp {
    render(None)
}

/// Manual actions (Sync now) wait on the network while the user watches, so
/// they get a longer budget than the throttled auto-pull/auto-push hooks —
/// matching the CLI's own `load sync` (`src/commands/sync.rs`).
pub(crate) const MANUAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Substrings that mean "git wanted credentials it did not have". Matched
/// case-insensitively against the whole error chain.
const AUTH_MARKERS: &[&str] = &[
    "authentication failed",
    "could not read username",
    "could not read password",
    "permission denied",
    "terminal prompts disabled",
    "403",
];

/// Append the one-command fix when a git failure looks like an auth failure.
///
/// This is the spec's whole credential story: an auth failure is an error
/// message, not a mechanism. `gh auth login` installs a git credential helper,
/// which is the single command that unblocks the common (GitHub, HTTPS) case.
pub(crate) fn auth_hint(msg: &str, gh: bool) -> String {
    let lower = msg.to_lowercase();
    if !AUTH_MARKERS.iter().any(|m| lower.contains(m)) {
        return msg.to_string();
    }
    if gh {
        format!("{msg} — run `gh auth login` once to give git credentials, then try again")
    } else {
        format!(
            "{msg} — git has no credentials for this remote. Use an SSH URL with a loaded key, \
             or install the GitHub CLI and run `gh auth login`"
        )
    }
}

/// Pull, then commit + push. Returns `(is_error, message)` for the card notice.
///
/// A `Diverged` pull is reconciled with `sync::reconcile_rebase` — the same
/// call the CLI makes — so studio resolves divergence in place instead of
/// telling the user to open a terminal.
pub(crate) fn sync_now_at(dir: &Path, timeout: Duration) -> (bool, String) {
    if !crate::sync::is_synced(dir) {
        return (true, "sync isn't set up on this machine yet".to_string());
    }
    let gh = crate::sync::gh_available();

    let pulled = match crate::sync::pull(dir, timeout) {
        Ok(crate::sync::PullOutcome::Pulled(n)) => n,
        Ok(crate::sync::PullOutcome::Diverged) => match crate::sync::reconcile_rebase(dir, timeout)
        {
            Ok(crate::sync::ReconcileOutcome::Rebased(n)) => n,
            Ok(crate::sync::ReconcileOutcome::Conflicted) => {
                return (
                    true,
                    "your config and the remote both changed the same lines — the rebase was \
                     aborted and nothing was lost. Reconcile by hand in the config dir."
                        .to_string(),
                )
            }
            Err(e) => return (true, auth_hint(&format!("reconciling failed: {e:#}"), gh)),
        },
        Err(e) => return (true, auth_hint(&format!("pulling failed: {e:#}"), gh)),
    };

    match crate::sync::commit_push(dir, "load studio: sync now", timeout) {
        Ok(crate::sync::PushOutcome::Pushed) => (
            false,
            format!("synced ✓ — pulled {pulled}, pushed your changes"),
        ),
        Ok(crate::sync::PushOutcome::NothingToPush) => (
            false,
            format!("synced ✓ — pulled {pulled}, nothing to push"),
        ),
        Ok(crate::sync::PushOutcome::Diverged) => (
            true,
            "the remote moved ahead again mid-sync — press Sync now once more".to_string(),
        ),
        Err(e) => (true, auth_hint(&format!("pushing failed: {e:#}"), gh)),
    }
}

/// `POST /sync/now`
pub fn sync_now() -> Resp {
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = sync_now_at(&dir, MANUAL_TIMEOUT);
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}

/// Set sync up. With an explicit `remote`, wires it and pushes. Without one,
/// falls back to `gh` creating a private repo named `loadout-config` — the same
/// flow `src/commands/sync.rs:123` runs for the CLI.
pub(crate) fn init_at(
    dir: &Path,
    remote: Option<&str>,
    gh: bool,
    timeout: Duration,
) -> (bool, String) {
    if crate::sync::is_synced(dir) {
        return (true, "sync is already set up on this machine".to_string());
    }
    let remote = remote.map(str::trim).filter(|r| !r.is_empty());

    if let Some(url) = remote {
        return match crate::sync::init(dir, Some(url), timeout) {
            Ok(()) => (false, format!("sync set up against {url} ✓")),
            Err(e) => (
                true,
                auth_hint(&format!("setting up sync failed: {e:#}"), gh),
            ),
        };
    }

    if !gh {
        return (
            true,
            "paste a git remote URL to sync against — create an empty private repo on your \
             host first. (With the GitHub CLI installed and `gh auth login` done, loadout can \
             create one for you.)"
                .to_string(),
        );
    }

    // gh path: local repo first, then create + push.
    if let Err(e) = crate::sync::init(dir, None, timeout) {
        return (true, format!("preparing the local repo failed: {e:#}"));
    }
    match crate::sync::gh_create_repo("loadout-config", false, dir, timeout) {
        Ok(crate::sync::GhCreate::Created { url }) => {
            (false, format!("created and pushed to {url} ✓"))
        }
        Ok(crate::sync::GhCreate::NameExists) => match crate::sync::gh_repo_url(
            "loadout-config",
            dir,
        ) {
            Some(url) => match crate::sync::wire_remote_and_push(dir, &url, timeout) {
                Ok(()) => (false, format!("adopted your existing {url} ✓")),
                Err(e) => (
                    true,
                    auth_hint(
                        &format!(
                            "a repo named loadout-config exists but adopting it failed: {e:#}"
                        ),
                        gh,
                    ),
                ),
            },
            None => (
                true,
                "a repo named loadout-config already exists on your account — paste its URL above \
                 to use it"
                    .to_string(),
            ),
        },
        Ok(crate::sync::GhCreate::Failed(m)) => (true, auth_hint(&format!("gh failed: {m}"), gh)),
        Err(e) => (true, auth_hint(&format!("gh failed: {e:#}"), gh)),
    }
}

/// `POST /sync/init` — body is the form-encoded `remote` field (may be empty;
/// `init_at` trims and treats empty as "no remote given").
pub fn init(req: &Req) -> Resp {
    let remote = crate::studio::server::field(&req.body, "remote");
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = init_at(
                &dir,
                Some(remote.as_str()),
                crate::sync::gh_available(),
                MANUAL_TIMEOUT,
            );
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}

/// Clone an existing config repo onto this machine. `sync::clone` does the
/// real work, including tolerating the installer's machine-local files and
/// refusing a dir that already holds real config.
pub(crate) fn clone_at(dir: &Path, url: &str, gh: bool, timeout: Duration) -> (bool, String) {
    let url = url.trim();
    if url.is_empty() {
        return (true, "paste the URL of your config repo".to_string());
    }
    match crate::sync::clone(url, dir, timeout) {
        Ok(()) => (
            false,
            format!("cloned your config from {url} ✓ — reopen studio to see it"),
        ),
        Err(e) => (true, auth_hint(&format!("cloning failed: {e:#}"), gh)),
    }
}

/// `POST /sync/clone` — body is the form-encoded `url` field.
pub fn clone_repo(req: &Req) -> Resp {
    let url = crate::studio::server::field(&req.body, "url");
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = clone_at(&dir, &url, crate::sync::gh_available(), MANUAL_TIMEOUT);
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}

/// Shared tail of every handler: rebuild the view from disk and re-render.
pub(crate) fn render(notice: Option<(bool, String)>) -> Resp {
    match crate::sync::config_dir() {
        Ok(dir) => Resp::html(card_fragment(&view_for(&dir, notice))),
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(synced: bool, dir_empty: bool) -> SyncView {
        SyncView {
            synced,
            remote: if synced {
                "loadout-config".into()
            } else {
                String::new()
            },
            last_synced: None,
            gh: false,
            dir_empty,
            notice: None,
        }
    }

    #[test]
    fn unsynced_card_offers_setup_and_clone() {
        let html = card_fragment(&view(false, true));
        assert!(html.contains("/sync/init"), "offers set-up");
        assert!(html.contains("/sync/clone"), "offers clone");
        assert!(!html.contains("/sync/now"), "nothing to sync yet");
    }

    #[test]
    fn unsynced_card_hides_clone_when_config_dir_has_content() {
        // `sync::clone` refuses a non-empty dir, so the button must not be
        // offered where it is guaranteed to fail.
        let html = card_fragment(&view(false, false));
        assert!(html.contains("/sync/init"));
        assert!(!html.contains("/sync/clone"));
    }

    #[test]
    fn synced_card_shows_remote_and_offers_sync_now() {
        let html = card_fragment(&view(true, false));
        assert!(html.contains("loadout-config"), "names the remote");
        assert!(html.contains("/sync/now"));
        assert!(!html.contains("/sync/init"), "already set up");
    }

    #[test]
    fn notice_renders_and_marks_errors() {
        let mut v = view(true, false);
        v.notice = Some((true, "pulling from the remote failed".into()));
        let html = card_fragment(&v);
        assert!(html.contains("pulling from the remote failed"));
        // Same banner markup Settings already uses (`banner error`), so it
        // picks up the existing CSS rather than inventing a class.
        assert!(html.contains("banner error"));

        v.notice = Some((false, "synced ✓".into()));
        let ok = card_fragment(&v);
        assert!(ok.contains("synced ✓"));
        assert!(!ok.contains("banner error"));
    }

    #[test]
    fn view_for_reports_unsynced_on_a_plain_directory() {
        let d = tempfile::tempdir().unwrap();
        let v = view_for(d.path(), None);
        assert!(!v.synced);
        assert!(v.dir_empty);
    }

    #[test]
    fn view_for_reports_dir_not_empty_ignoring_machine_local_files() {
        // `loadout-receipt.json` is written by the installer before `load` ever
        // runs; `sync::clone` tolerates it, so it must not count as content.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("loadout-receipt.json"), "{}").unwrap();
        assert!(view_for(d.path(), None).dir_empty);

        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        assert!(!view_for(d.path(), None).dir_empty);
    }

    use std::process::Command;

    /// A bare repo on `main`, matching `sync.rs`'s own test helper.
    fn bare(parent: &Path) -> std::path::PathBuf {
        let r = parent.join("remote.git");
        Command::new("git")
            .args(["init", "--bare", "-b", "main", "-q"])
            .arg(&r)
            .status()
            .unwrap();
        r
    }

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

    #[test]
    fn auth_hint_names_gh_login_when_gh_is_installed() {
        let msg = "pushing failed: fatal: Authentication failed for 'https://github.com/x'";
        let with = auth_hint(msg, true);
        assert!(with.contains("gh auth login"));
        let without = auth_hint(msg, false);
        assert!(without.contains("SSH URL"), "still actionable without gh");
    }

    #[test]
    fn auth_hint_leaves_unrelated_errors_alone() {
        let msg = "pushing failed: could not resolve host github.com";
        assert_eq!(auth_hint(msg, true), msg);
    }

    #[test]
    fn sync_now_on_an_unsynced_dir_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().unwrap();
        let (is_error, msg) = sync_now_at(d.path(), Duration::from_secs(5));
        assert!(is_error);
        assert!(msg.contains("isn't set up"));
    }

    #[test]
    fn sync_now_pushes_local_edits_to_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Edit, then Sync now.
        std::fs::write(a.join("config.toml"), "x = 2\n").unwrap();
        let (is_error, msg) = sync_now_at(&a, Duration::from_secs(30));
        assert!(!is_error, "expected success, got: {msg}");
        assert!(msg.contains("synced"), "got: {msg}");

        // A fresh clone sees the edit.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        assert_eq!(
            std::fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 2\n"
        );
    }

    #[test]
    fn init_with_an_explicit_remote_publishes_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert!(crate::sync::is_synced(&a));

        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        assert_eq!(
            std::fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 1\n"
        );
    }

    #[test]
    fn init_without_a_remote_and_without_gh_explains_what_is_needed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        let (is_error, msg) = init_at(d.path(), None, false, Duration::from_secs(30));
        assert!(is_error);
        assert!(msg.contains("remote"), "names what's missing: {msg}");
    }

    #[test]
    fn init_is_idempotent_on_an_already_synced_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30));
        assert!(is_error);
        assert!(msg.contains("already"), "got: {msg}");
    }

    #[test]
    fn sync_now_pulls_a_remote_edit_made_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Machine B clones, edits, pushes.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        identify(&b);
        std::fs::write(b.join("config.toml"), "x = 99\n").unwrap();
        crate::sync::commit_push(&b, "b edit", Duration::from_secs(30)).unwrap();

        // Machine A presses Sync now and receives it.
        let (is_error, msg) = sync_now_at(&a, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert_eq!(
            std::fs::read_to_string(a.join("config.toml")).unwrap(),
            "x = 99\n"
        );
    }

    #[test]
    fn clone_brings_an_existing_config_onto_a_fresh_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 7\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Fresh machine: config dir holds only the installer's receipt.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("loadout-receipt.json"), r#"{"version":"0.25.0"}"#).unwrap();

        let (is_error, msg) = clone_at(&b, url, false, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert_eq!(
            std::fs::read_to_string(b.join("config.toml")).unwrap(),
            "x = 7\n"
        );
        assert!(
            b.join("loadout-receipt.json").exists(),
            "machine-local file survives"
        );
    }

    #[test]
    fn clone_rejects_a_blank_url() {
        let d = tempfile::tempdir().unwrap();
        let (is_error, msg) = clone_at(d.path(), "   ", false, Duration::from_secs(5));
        assert!(is_error);
        assert!(msg.contains("URL"));
    }

    #[test]
    fn clone_refuses_when_the_config_dir_already_has_content() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        let (is_error, msg) = clone_at(
            d.path(),
            "https://example.test/x.git",
            false,
            Duration::from_secs(5),
        );
        assert!(is_error);
        assert!(
            msg.to_lowercase().contains("already has content"),
            "got: {msg}"
        );
    }
}
