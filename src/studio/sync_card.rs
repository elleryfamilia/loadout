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
//!
//! **These handlers block the whole studio while they run.** `serve_loop` is
//! single-threaded and synchronous — one request at a time, no thread pool — so
//! a sync handler holds up every other request, assets included, for its full
//! duration. One "Sync now" chains several network operations at
//! [`MANUAL_TIMEOUT`] each, so that can be a minute or more. Two consequences
//! for anything added here: every button must show its in-flight state (the
//! `.sync-action` rule in `studio.css`), and no action may be added that can
//! block for longer than a user will wait.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use maud::html;

use crate::studio::server::{Req, Resp};
use crate::studio::state::StudioState;
use crate::studio::views;

/// What an action reports back to the card.
pub(crate) struct Outcome {
    /// `(is_error, message)` for the card notice.
    pub notice: (bool, String),
    /// The action rewrote the config files on disk. Studio read them into
    /// memory once, at `serve()` time, so the session must be reloaded — see
    /// [`act_at`].
    pub config_changed: bool,
}

impl Outcome {
    fn ok(msg: impl Into<String>) -> Self {
        Outcome {
            notice: (false, msg.into()),
            config_changed: false,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Outcome {
            notice: (true, msg.into()),
            config_changed: false,
        }
    }

    /// Mark this outcome as having rewritten config on disk.
    fn changed(mut self) -> Self {
        self.config_changed = true;
        self
    }
}

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
    // `section.settings-section` + an `h3`, the chrome the rest of Settings
    // uses — the card sits inside that page, not on the welcome screen the
    // `.cmd-block` styling belongs to.
    //
    // `.sync-action` marks what the htmx shim puts `htmx-request` on while a
    // request is in flight: the button for a bare `hx-post`, the *form* for a
    // submit. `studio.css` styles both shapes; without it these long, studio-
    // blocking actions would look like they did nothing.
    //
    // The URL inputs are `type="text"` with `inputmode="url"`, not
    // `type="url"`: HTML constraint validation demands a scheme, which would
    // reject `git@github.com:you/loadout-config.git` — the standard SSH remote,
    // and the form `auth_hint` below tells users to use — before the submit
    // event fires, so no request would ever be sent.
    let body = html! {
        section class="settings-section" {
            h3 { "Sync" }
            // Banner markup copied from `settings::page_fragment` so it reuses
            // the existing CSS — `banner error`, and a `banner-body` div.
            @if let Some((is_error, msg)) = &v.notice {
                div class=(if *is_error { "banner error" } else { "banner" }) {
                    span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                    div class="banner-body" { (msg) }
                }
            }
            @if v.synced {
                p class="muted" {
                    "Your global config syncs with " strong { (v.remote) } "."
                    @if let Some(t) = v.last_synced { " Last synced " (ago(t)) "." }
                }
                button class="btn btn-ghost sync-action"
                    hx-post="/sync/now" hx-target="#sync-card" {
                    (views::icon("refresh")) "Sync now"
                }
            } @else {
                p class="muted" {
                    "Sync keeps your global config in a git repo so every machine you work on \
                     equips the same context. Nothing leaves this machine until you set it up."
                }
                form class="sync-action" hx-post="/sync/init" hx-target="#sync-card" {
                    input type="text" inputmode="url" name="remote"
                        placeholder="git remote URL (optional)" {}
                    button class="btn btn-ghost" type="submit" {
                        (views::icon("plus"))
                        @if v.gh { "Set up sync (creates a private GitHub repo)" } @else { "Set up sync" }
                    }
                }
                @if v.dir_empty {
                    form class="sync-action" hx-post="/sync/clone" hx-target="#sync-card" {
                        input type="text" inputmode="url" name="url" required
                            placeholder="https://github.com/you/loadout-config" {}
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
    with_dir(|dir| Resp::html(card_fragment(&view_for(dir, None))))
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

/// Append the fix when a git failure looks like an auth failure.
///
/// This is the spec's whole credential story: an auth failure is an error
/// message, not a mechanism — studio never collects a token. Every sync
/// operation shells out to plain `git`, so anything git already trusts works:
/// a loaded SSH key, or any configured credential helper (macOS Keychain,
/// Windows Credential Manager, GCM). `gh auth login` is named only as the
/// GitHub-specific shortcut for installing one, not as the general answer —
/// the remote may be GitLab, Gitea, or self-hosted.
pub(crate) fn auth_hint(msg: &str, gh: bool) -> String {
    let lower = msg.to_lowercase();
    if !AUTH_MARKERS.iter().any(|m| lower.contains(m)) {
        return msg.to_string();
    }
    if gh {
        format!(
            "{msg} — on a GitHub remote, run `gh auth login` once to give git credentials; on \
             another host, use an SSH URL with a loaded key or configure a git credential helper"
        )
    } else {
        format!(
            "{msg} — git has no credentials for this remote. Use an SSH URL with a loaded key, \
             or set a git credential helper (`credential.helper` — macOS: `osxkeychain`, \
             Windows: `manager`). On GitHub, `gh auth login` sets one up for you"
        )
    }
}

/// Pull, then commit + push.
///
/// A `Diverged` pull is reconciled with `sync::reconcile_rebase` — the same
/// call the CLI makes — so studio resolves divergence in place instead of
/// telling the user to open a terminal.
///
/// `gh` is a parameter rather than a `gh_available()` probe so tests can pin
/// both [`auth_hint`] branches (the convention `init_at`/`clone_at` follow).
pub(crate) fn sync_now_at(dir: &Path, gh: bool, timeout: Duration) -> Outcome {
    if !crate::sync::is_synced(dir) {
        return Outcome::err("sync isn't set up on this machine yet");
    }

    // What the pull did, as (notice phrase, did the working tree move).
    let (pulled, moved) = match crate::sync::pull(dir, timeout) {
        Ok(crate::sync::PullOutcome::Pulled(n)) => (format!("pulled {n}"), n > 0),
        Ok(crate::sync::PullOutcome::Diverged) => match crate::sync::reconcile_rebase(dir, timeout)
        {
            // `Rebased(n)` counts the *local* commits replayed, not commits
            // received, so it can't be phrased as "pulled n". A rebase only
            // happens after a `Diverged` pull — the remote had commits this
            // machine lacked — so the working tree always moved.
            Ok(crate::sync::ReconcileOutcome::Rebased(_)) => {
                ("rebased onto the remote".to_string(), true)
            }
            Ok(crate::sync::ReconcileOutcome::Conflicted) => {
                return Outcome::err(
                    "your config and the remote both changed the same lines — the rebase was \
                     aborted and nothing was lost. Reconcile by hand in the config dir.",
                )
            }
            Err(e) => return Outcome::err(auth_hint(&format!("reconciling failed: {e:#}"), gh)),
        },
        Err(e) => return Outcome::err(auth_hint(&format!("pulling failed: {e:#}"), gh)),
    };

    let mut out = match crate::sync::commit_push(dir, "load studio: sync now", timeout) {
        Ok(crate::sync::PushOutcome::Pushed) => {
            Outcome::ok(format!("synced ✓ — {pulled}, pushed your changes"))
        }
        Ok(crate::sync::PushOutcome::NothingToPush) => {
            Outcome::ok(format!("synced ✓ — {pulled}, nothing to push"))
        }
        Ok(crate::sync::PushOutcome::Diverged) => {
            Outcome::err("the remote moved ahead again mid-sync — press Sync now once more")
        }
        Err(e) => Outcome::err(auth_hint(&format!("pushing failed: {e:#}"), gh)),
    };
    // Set after the push: a pull that moved the tree still needs the reload
    // even when the push that followed it failed.
    out.config_changed = moved;
    out
}

/// `POST /sync/now`
pub fn sync_now(state: &Arc<Mutex<StudioState>>) -> Resp {
    act(state, |dir| {
        sync_now_at(dir, crate::sync::gh_available(), MANUAL_TIMEOUT)
    })
}

/// Set sync up. With an explicit `remote`, wires it and pushes. Without one,
/// falls back to `gh` creating a private repo named `loadout-config`.
///
/// The `gh` path runs the same preemptive GH007 mitigation the CLI's `offer_gh`
/// (`src/commands/sync.rs`) runs before it creates a repo — see the comment on
/// those three calls below. It does *not* prompt for a repo name or visibility
/// the way the CLI does: studio always creates a private `loadout-config`, and
/// surfaces a name collision as a notice instead of a prompt.
pub(crate) fn init_at(dir: &Path, remote: Option<&str>, gh: bool, timeout: Duration) -> Outcome {
    if crate::sync::is_synced(dir) {
        return Outcome::err("sync is already set up on this machine");
    }
    let remote = remote.map(str::trim).filter(|r| !r.is_empty());

    if let Some(url) = remote {
        return match crate::sync::init(dir, Some(url), timeout) {
            // The notice names the remote, never the URL the user pasted: an
            // HTTPS URL can carry a token (`https://user:TOKEN@host/…`), and
            // this string is rendered into the page. Matches what the CLI
            // prints on the same path.
            Ok(()) => Outcome::ok(format!(
                "sync set up against remote {} ✓",
                crate::sync::remote_name(dir)
            )),
            Err(e) => Outcome::err(auth_hint(&format!("setting up sync failed: {e:#}"), gh)),
        };
    }

    if !gh {
        return Outcome::err(
            "paste a git remote URL to sync against — create an empty private repo on your \
             host first. (With the GitHub CLI installed and `gh auth login` done, loadout can \
             create one for you.)",
        );
    }

    // gh path: local repo first, then create + push.
    //
    // No `auth_hint` on this arm (the only error branch here without one):
    // `sync::init(dir, None, ..)` wires no remote and performs no network I/O,
    // so an auth failure is not reachable from it.
    if let Err(e) = crate::sync::init(dir, None, timeout) {
        return Outcome::err(format!("preparing the local repo failed: {e:#}"));
    }
    // GitHub rejects a push that would publish a private commit email (GH007).
    // `gh repo create --push` pushes *outside* `sync::commit_push`, so it never
    // reaches that function's reactive GH007 recovery — without these three
    // calls this is the one push route in the codebase with neither preemptive
    // nor reactive protection. Same three calls, same order, as the CLI's
    // `offer_gh` makes immediately before `gh repo create`. Best-effort: if gh
    // isn't authenticated `gh_noreply_email` returns None and `gh_create_repo`
    // below surfaces that as the real error.
    if let Some(noreply) = crate::sync::gh_noreply_email() {
        let _ = crate::sync::set_commit_email(dir, &noreply);
        let _ = crate::sync::amend_reset_author(dir);
    }
    // These URLs come from `gh` (the created/looked-up repo's web URL), not
    // from the user, so they carry no credentials and are safe to show.
    match crate::sync::gh_create_repo("loadout-config", false, dir, timeout) {
        Ok(crate::sync::GhCreate::Created { url }) => {
            Outcome::ok(format!("created and pushed to {url} ✓"))
        }
        Ok(crate::sync::GhCreate::NameExists) => {
            match crate::sync::gh_repo_url("loadout-config", dir) {
                Some(url) => match crate::sync::wire_remote_and_push(dir, &url, timeout) {
                    Ok(()) => Outcome::ok(format!("adopted your existing {url} ✓")),
                    Err(e) => Outcome::err(auth_hint(
                        &format!(
                            "a repo named loadout-config exists but adopting it failed: {e:#}"
                        ),
                        gh,
                    )),
                },
                None => Outcome::err(
                    "a repo named loadout-config already exists on your account — paste its URL \
                     above to use it",
                ),
            }
        }
        Ok(crate::sync::GhCreate::Failed(m)) => {
            Outcome::err(auth_hint(&format!("gh failed: {m}"), gh))
        }
        Err(e) => Outcome::err(auth_hint(&format!("gh failed: {e:#}"), gh)),
    }
}

/// `POST /sync/init` — body is the form-encoded `remote` field (may be empty;
/// `init_at` trims and treats empty as "no remote given").
pub fn init(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let remote = crate::studio::server::field(&req.body, "remote");
    act(state, |dir| {
        init_at(
            dir,
            Some(remote.as_str()),
            crate::sync::gh_available(),
            MANUAL_TIMEOUT,
        )
    })
}

/// Clone an existing config repo onto this machine. `sync::clone` does the
/// real work, including tolerating the installer's machine-local files and
/// refusing a dir that already holds real config.
pub(crate) fn clone_at(dir: &Path, url: &str, gh: bool, timeout: Duration) -> Outcome {
    let url = url.trim();
    if url.is_empty() {
        return Outcome::err("paste the URL of your config repo");
    }
    match crate::sync::clone(url, dir, timeout) {
        // Named by remote, not by the pasted URL (which can carry a token),
        // and with no "reopen studio" advice: `act_at` reloads the session, so
        // the cloned config is live in this studio already.
        Ok(()) => Outcome::ok(format!(
            "cloned your config from remote {} ✓",
            crate::sync::remote_name(dir)
        ))
        .changed(),
        Err(e) => Outcome::err(auth_hint(&format!("cloning failed: {e:#}"), gh)),
    }
}

/// `POST /sync/clone` — body is the form-encoded `url` field.
pub fn clone_repo(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let url = crate::studio::server::field(&req.body, "url");
    act(state, |dir| {
        clone_at(dir, &url, crate::sync::gh_available(), MANUAL_TIMEOUT)
    })
}

/// Resolve the global config dir once per request, or render the one error
/// fragment every route shares. Every handler goes through here, so the dir is
/// resolved once rather than once to act on and again to re-render.
fn with_dir(f: impl FnOnce(&Path) -> Resp) -> Resp {
    match crate::sync::config_dir() {
        Ok(dir) => f(&dir),
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}

/// Shared body of every state-changing handler: run `f` against the config dir,
/// reload studio's config session when `f` rewrote config on disk, then
/// re-render the card from that same dir.
fn act(state: &Arc<Mutex<StudioState>>, f: impl FnOnce(&Path) -> Outcome) -> Resp {
    with_dir(|dir| act_at(state, dir, f))
}

/// [`act`] against an explicit dir, so tests drive the reload against a temp
/// repo instead of the developer's real config dir.
///
/// The reload is what keeps a pull or clone from stranding studio: `Session`
/// reads every config layer into memory once, at `serve()` time, and gates
/// Apply on the on-disk bytes still matching. Without this, a successful sync
/// leaves the page rendering a config that is no longer on disk and every later
/// Apply failing — with no in-studio way back, since `load studio` re-attaches
/// to the running instance rather than restarting it.
pub(crate) fn act_at(
    state: &Arc<Mutex<StudioState>>,
    dir: &Path,
    f: impl FnOnce(&Path) -> Outcome,
) -> Resp {
    let mut out = f(dir);
    if out.config_changed {
        // Taken in its own `let` so the guard is dropped before rendering
        // (the studio rule: never hold the session mutex across rendering).
        let reloaded = state.lock().unwrap().session.reload();
        if let Err(e) = reloaded {
            out.notice = (
                true,
                format!(
                    "{} · but studio could not load the new config ({e:#}) — press Discard in \
                     the top bar to drop your staged edits and pick it up",
                    out.notice.1
                ),
            );
        }
    }
    Resp::html(card_fragment(&view_for(dir, Some(out.notice))))
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
    fn remote_inputs_accept_an_ssh_url() {
        // `type="url"` fails HTML constraint validation on
        // `git@github.com:you/loadout-config.git` — the standard SSH remote,
        // and what `auth_hint` tells users to use. The browser then blocks the
        // submit event the htmx shim listens for, so no request is ever sent.
        let html = card_fragment(&view(false, true));
        assert!(
            !html.contains("type=\"url\""),
            "an SSH remote must be typeable: {html}"
        );
        assert_eq!(html.matches("inputmode=\"url\"").count(), 2, "{html}");
        // The clone URL is still required (the blank case is also guarded
        // server-side by `clone_at`).
        assert!(html.contains("required"), "{html}");
    }

    #[test]
    fn every_action_is_marked_for_the_in_flight_style() {
        // These actions block the entire (single-threaded) studio for as long
        // as the network takes, so each one must carry the class the stylesheet
        // gives an in-flight progress bar. The shim sets `htmx-request` on the
        // element carrying `hx-post`: the button for "Sync now", the form for a
        // submit — so the class goes on those same elements.
        let unsynced = card_fragment(&view(false, true));
        assert_eq!(
            unsynced.matches("class=\"sync-action\"").count(),
            2,
            "both forms: {unsynced}"
        );
        let synced = card_fragment(&view(true, false));
        assert!(synced.contains("btn-ghost sync-action"), "{synced}");

        // …and the stylesheet actually styles that class. (What the rule looks
        // like is not asserted — only that markup and CSS still agree.)
        let (css, _) = crate::studio::assets::get("/assets/studio.css").unwrap();
        let css = String::from_utf8(css).unwrap();
        assert!(css.contains("button.sync-action.htmx-request"), "no style");
        assert!(css.contains("form.sync-action.htmx-request"), "no style");
    }

    #[test]
    fn the_card_wears_settings_chrome() {
        // It renders inside the Settings page, next to `agent_section` — not on
        // the welcome screen, where the `cmd-block` styling it used to borrow
        // belongs.
        let html = card_fragment(&view(true, false));
        assert!(
            html.starts_with("<section class=\"settings-section\">"),
            "{html}"
        );
        assert!(html.contains("<h3>Sync</h3>"), "{html}");
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
        let (is_error, msg) = sync_now_at(d.path(), false, Duration::from_secs(5)).notice;
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
        let (is_error, msg) = sync_now_at(&a, false, Duration::from_secs(30)).notice;
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

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30)).notice;
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
        let (is_error, msg) = init_at(d.path(), None, false, Duration::from_secs(30)).notice;
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

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30)).notice;
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
        let (is_error, msg) = sync_now_at(&a, false, Duration::from_secs(30)).notice;
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

        let (is_error, msg) = clone_at(&b, url, false, Duration::from_secs(30)).notice;
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

    /// A studio state whose config session is open over `gdir` — the shape
    /// `serve()` builds, minus the socket. Used to prove that an action which
    /// rewrites `config.toml` on disk also refreshes studio's in-memory copy.
    fn studio_over(repo: &Path, gdir: &Path) -> Arc<Mutex<StudioState>> {
        let config = crate::config::Config::load_from(Some(&gdir.join("config.toml")), repo)
            .expect("fixture config parses");
        let base_context = crate::context::detect_context(repo, &config).unwrap();
        let session = crate::studio::edit::Session::open(repo, Some(gdir)).unwrap();
        Arc::new(Mutex::new(StudioState {
            session,
            base_context,
            repo_base: repo.to_path_buf(),
            token: "testtoken".into(),
            port: 7777,
            onboarding_active: false,
            active_tab: "settings".into(),
            recents_path: None,
        }))
    }

    /// What studio currently believes the global `config.toml` says.
    fn session_sees(state: &Arc<Mutex<StudioState>>, gdir: &Path) -> String {
        let want = gdir.join("config.toml");
        state
            .lock()
            .unwrap()
            .session
            .staged_layer_texts()
            .into_iter()
            .find(|(_, p, _)| *p == want)
            .map(|(_, _, t)| t)
            .expect("the global layer is open in this session")
    }

    #[test]
    fn a_pull_refreshes_the_studio_session_not_just_the_disk() {
        // The regression: `Session::open` reads every layer once at serve()
        // time, so a pull that rewrites config.toml behind studio leaves the
        // page showing a config that is no longer on disk — and every later
        // Apply fails the external-edit gate naming a "reload" studio didn't
        // offer. The handler must reload the session after a pull that moved.
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();
        let old = "[[fragments]]\nid = \"rc\"\nguidance = \"old\"\n";
        let new = "[[fragments]]\nid = \"rc\"\nguidance = \"new\"\n";

        // Machine A: a synced config dir, with studio open over it.
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), old).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = studio_over(&repo, &a);
        assert!(session_sees(&state, &a).contains("old"));

        // Machine B publishes a change.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        identify(&b);
        std::fs::write(b.join("config.toml"), new).unwrap();
        crate::sync::commit_push(&b, "b edit", Duration::from_secs(30)).unwrap();

        // A presses Sync now, through the same helper the route uses.
        let resp = act_at(&state, &a, |dir| {
            sync_now_at(dir, false, Duration::from_secs(30))
        });
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains("banner error"), "expected success: {body}");

        assert_eq!(std::fs::read_to_string(a.join("config.toml")).unwrap(), new);
        assert!(
            session_sees(&state, &a).contains("new"),
            "studio must show the pulled config, not the one it loaded at startup"
        );
        // The Apply gate agrees: nothing looks externally edited any more.
        assert!(state.lock().unwrap().session.external_edits().is_empty());
    }

    #[test]
    fn a_clone_refreshes_the_studio_session() {
        // The headline journey: a fresh machine clones from studio and must
        // then see the cloned config without restarting studio (which a user
        // with no terminal cannot do).
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();
        let published = "[[fragments]]\nid = \"rc\"\nguidance = \"published\"\n";

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), published).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Fresh machine: empty config dir, studio already open over it.
        let fresh = tmp.path().join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = studio_over(&repo, &fresh);
        assert_eq!(session_sees(&state, &fresh), "");

        let resp = act_at(&state, &fresh, |dir| {
            clone_at(dir, url, false, Duration::from_secs(30))
        });
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains("banner error"), "expected success: {body}");
        assert!(
            session_sees(&state, &fresh).contains("published"),
            "studio must show the cloned config without a restart"
        );
    }

    #[test]
    fn a_clone_notice_names_the_remote_not_the_url_the_user_pasted() {
        // A credential-bearing HTTPS URL must not be echoed into the page.
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

        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        let (is_error, msg) = clone_at(&b, url, false, Duration::from_secs(30)).notice;
        assert!(!is_error, "got: {msg}");
        assert!(!msg.contains(url), "the pasted URL is echoed back: {msg}");
        assert!(msg.contains("remote"), "names the remote instead: {msg}");
    }

    #[test]
    fn init_notice_names_the_remote_not_the_url_the_user_pasted() {
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

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30)).notice;
        assert!(!is_error, "got: {msg}");
        assert!(!msg.contains(url), "the pasted URL is echoed back: {msg}");
        assert!(msg.contains("remote"), "names the remote instead: {msg}");
    }

    /// A synced repo whose remote always fails the way an unauthenticated
    /// HTTPS remote does — offline and deterministically.
    ///
    /// Git's `ext::` transport runs a command instead of talking to a server,
    /// so the helper below prints a real auth rejection to stderr and exits
    /// non-zero. That is the only way to exercise the auth branches without a
    /// live server that answers 401. Unix-only (the helper is a shell script);
    /// CI runs ubuntu + macos.
    #[cfg(unix)]
    fn repo_with_a_rejecting_remote(tmp: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let helper = tmp.join("reject.sh");
        std::fs::write(
            &helper,
            "#!/bin/sh\necho 'fatal: Authentication failed for this remote' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let a = tmp.join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&a)
            .status()
            .unwrap();
        identify(&a);
        // `ext::` is refused by default; allowing it is repo-local config, so
        // it never leaks past this fixture.
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["config", "protocol.ext.allow", "always"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["add", "-A"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["commit", "-qm", "seed"])
            .status()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&a)
            .args(["remote", "add", "origin"])
            .arg(format!("ext::{}", helper.display()))
            .status()
            .unwrap();
        a
    }

    #[cfg(unix)]
    #[test]
    fn sync_now_appends_the_auth_hint_and_honours_the_gh_flag() {
        // `sync_now_at` used to probe `gh_available()` itself, so its advice
        // varied by machine and neither branch could be pinned. With `gh` as a
        // parameter both are testable — which is the point of taking it.
        let tmp = tempfile::tempdir().unwrap();
        let a = repo_with_a_rejecting_remote(tmp.path());

        let (is_error, with_gh) = sync_now_at(&a, true, Duration::from_secs(10)).notice;
        assert!(is_error, "a rejected push is an error: {with_gh}");
        assert!(
            with_gh.contains("gh auth login"),
            "with gh installed, name the one-command fix: {with_gh}"
        );

        let (is_error, without) = sync_now_at(&a, false, Duration::from_secs(10)).notice;
        assert!(is_error);
        assert!(
            without.contains("credential helper") && without.contains("SSH URL"),
            "without gh, the advice must be host-agnostic: {without}"
        );
    }

    #[test]
    fn clone_rejects_a_blank_url() {
        let d = tempfile::tempdir().unwrap();
        let (is_error, msg) = clone_at(d.path(), "   ", false, Duration::from_secs(5)).notice;
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
        )
        .notice;
        assert!(is_error);
        assert!(
            msg.to_lowercase().contains("already has content"),
            "got: {msg}"
        );
    }
}
