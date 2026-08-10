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
use std::time::SystemTime;

use maud::html;

use crate::studio::server::Resp;
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
}
