//! The studio **Settings** page — the minimalist home for config the studio
//! can edit: today, the default launch agent. Every write stages through the
//! [`crate::studio::edit::Session`] pipeline, and — unlike content edits
//! (fragments/loadouts), which always wait for an explicit Apply — applies
//! immediately when nothing else is staged: a settings toggle should just
//! take effect, not sit "staged" (see [`apply_or_stage`]).
//!
//! Not here (TOML-only, by design): `[env]`, `[codex]`, and the trust store
//! (a future tenant). `[sync]` is surfaced by [`crate::studio::sync_card`].

use std::sync::{Arc, Mutex};

use maud::{html, Markup};

use crate::studio::edit::StagedOp;
use crate::studio::server::{Req, Resp};
use crate::studio::state::{self, StudioState};
use crate::studio::views;

/// `GET /settings` — the full page into `#main`. Marks the gear active and
/// appends the drawer-close loader (a drawer action may have navigated here).
pub fn page(state: &Arc<Mutex<StudioState>>) -> Resp {
    state.lock().unwrap().active_tab = "settings".to_string();
    render_page(state, None)
}

pub(crate) fn render_page(state: &Arc<Mutex<StudioState>>, notice: Option<(bool, String)>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    // Only agents `load run` can actually launch (a `launch` program). The
    // current default is kept even if it fell out of that set (e.g. a hand-
    // edited config naming "generic") so the select never lies about the
    // live value — it just won't be reachable by picking it again.
    let mut agents: Vec<String> = cfg
        .agents
        .iter()
        .filter(|a| a.launch.is_some())
        .map(|a| a.id.clone())
        .collect();
    if !agents.contains(&cfg.default_agent) {
        agents.push(cfg.default_agent.clone());
    }

    let mut html = page_fragment(
        &SettingsView {
            default_agent: cfg.default_agent.clone(),
            agents,
        },
        notice,
    );
    html.push_str(&views::drawer_close_loader());
    Resp::html(html)
}

/// Everything the page renders, prepared by the handler (no fs in the view).
struct SettingsView {
    default_agent: String,
    agents: Vec<String>,
}

fn page_fragment(v: &SettingsView, notice: Option<(bool, String)>) -> String {
    html! {
        div class="settings" {
            h2 { "Settings" }
            @if let Some((is_error, msg)) = &notice {
                div class=(if *is_error { "banner error" } else { "banner" }) {
                    span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                    div class="banner-body" { (msg) }
                }
            }
            (agent_section(&v.default_agent, &v.agents))
            // Loads lazily so the settings render never blocks on git.
            div id="sync-card" hx-get="/sync/card" hx-trigger="load" hx-target="#sync-card" {}
        }
    }
    .into_string()
}

fn agent_section(current: &str, agents: &[String]) -> Markup {
    html! {
        section class="settings-section" id="settings-agent" {
            h3 { "Default agent" }
            p class="muted" { "Which agent " code { "load run" } " launches when you don't name one." }
            form hx-post="/settings/agent" hx-target="#main" {
                select name="agent" {
                    @for a in agents {
                        option value=(a) selected[a == current] { (a) }
                    }
                }
                button type="submit" class="btn btn-primary btn-sm" { "Save" }
            }
        }
    }
}

/// `POST /settings/agent` — set `[defaults] agent`.
pub fn set_agent(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let Some(agent) = pairs
        .iter()
        .find(|(k, _)| k == "agent")
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return render_page(state, Some((true, "pick an agent".to_string())));
    };
    apply_or_stage(
        state,
        StagedOp::SetDefaultAgent {
            layer: crate::fragment::Layer::Global,
            agent,
        },
        "saved",
    )
}

/// Shared tail for every settings write: stage `op`, then either apply it
/// immediately or leave it queued, depending on whether anything else was
/// already staged when this request arrived.
///
/// Staging is the right model for *content* (fragments/loadouts) — you build
/// up a batch, review it on the diff page, then Apply. A settings toggle is
/// not content: Ellery's expectation is that flipping it just takes effect,
/// the same way editing `[sync]`/`[codex]` by hand does. So when the session
/// was clean, this stages `op` and immediately calls `session.apply()` — the
/// same write `handle_apply`'s Apply button performs, including its
/// learn-hook bootstrap and auto-push side effects. When something else was
/// already staged, `op` joins it and waits for that Apply instead, so a
/// half-reviewed batch of content edits is never silently written early.
///
/// `applied_msg` is the banner shown when `op` applies immediately (e.g.
/// "saved", or the learn-specific "learning is on"/"learning is off" so D's
/// green-pill state and the banner text agree).
fn apply_or_stage(state: &Arc<Mutex<StudioState>>, op: StagedOp, applied_msg: &str) -> Resp {
    // The was_clean read, the stage, and the apply below each take the lock
    // separately. That check-then-act sequence is race-free ONLY because
    // `serve_loop` handles requests strictly one at a time (single-threaded);
    // if the server ever goes concurrent, this must become one critical
    // section or an auto-apply could flush another request's staged edits.
    let was_clean = state.lock().unwrap().session.ops().is_empty();

    // Each lock is taken in its own `let` statement rather than directly in a
    // `match` scrutinee: a `match`'s scrutinee temporaries (here, the
    // `MutexGuard`) live for the *whole* match expression, not just the
    // scrutinee evaluation — matching on `state.lock().unwrap().session.stage(..)`
    // directly would hold the guard across the arms below, and the nested
    // `.lock()` for apply() would then deadlock against itself.
    let stage_result = state.lock().unwrap().session.stage(op);
    let notice = match stage_result {
        Err(e) => (true, e.to_string()),
        Ok(()) if !was_clean => (
            false,
            "staged alongside your pending edits — Apply to save".to_string(),
        ),
        Ok(()) => {
            let apply_result = state.lock().unwrap().session.apply();
            match apply_result {
                Ok(_written) => {
                    // Same post-apply side effect `handle_apply` runs, so a
                    // settings save behaves identically whichever button
                    // triggered the write.
                    let mut msg = applied_msg.to_string();
                    if let Some(note) = crate::studio::server::auto_push_after_apply(state) {
                        msg.push_str(" · ");
                        msg.push_str(&note);
                    }
                    (false, msg)
                }
                // The op stays staged — `apply()` only clears ops on success —
                // so this is surfaced rather than discarded; the top-bar
                // Apply can retry once the conflict is resolved.
                //
                // `apply()`'s external-edit gate runs before any write, so if
                // that's why this failed, the on-disk bytes it compared
                // against are still out of sync with what this session
                // loaded — checking `external_edits()` here (same lock,
                // right after the failure) tells us that structurally
                // instead of guessing from the error text. Any other failure
                // (e.g. a backup write that couldn't create its directory)
                // leaves the loaded files untouched, so this reads empty and
                // the message doesn't assert a cause it doesn't know.
                Err(e) => {
                    let is_external_edit =
                        !state.lock().unwrap().session.external_edits().is_empty();
                    let msg = if is_external_edit {
                        format!("config changed on disk — review and apply from the top bar: {e:#}")
                    } else {
                        format!("apply failed: {e:#}")
                    };
                    (true, msg)
                }
            }
        }
    };

    let mut resp = render_page(state, Some(notice));
    resp.body
        .extend_from_slice(views::staged_indicator_loader().as_bytes());
    resp
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fragment::Layer;

    /// A repo tempdir plus a global config dir (a subdir of it) whose
    /// `config.toml` starts with `body`. Mirrors the fixture in
    /// `studio::edit::tests` / `studio::sync_card::tests`.
    fn repo_with_global(body: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let gdir = d.path().join("global");
        std::fs::create_dir_all(&gdir).unwrap();
        std::fs::write(gdir.join("config.toml"), body).unwrap();
        (d, gdir)
    }

    /// A studio state whose session is opened via `open_with`, so the state
    /// dir is injected rather than resolved from the real `~/.local/state`
    /// (see `studio::edit::Session::open_with`).
    fn state_with_session(
        repo: &Path,
        gdir: &Path,
        state_dir: Option<PathBuf>,
    ) -> Arc<Mutex<StudioState>> {
        let gcfg = gdir.join("config.toml");
        let config = crate::config::Config::load_from(Some(&gcfg), repo).unwrap();
        let base_context = crate::context::detect_context(repo, &config).unwrap();
        let session = crate::studio::edit::Session::open_with(repo, Some(gdir), state_dir).unwrap();
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

    #[test]
    fn apply_or_stage_names_the_external_edit_when_thats_the_cause() {
        let (d, gdir) = repo_with_global("[defaults]\nagent = \"claude\"\n");
        let state = state_with_session(d.path(), &gdir, Some(d.path().join("state")));

        // Someone edits the global config out from under the open session.
        std::fs::write(gdir.join("config.toml"), "[defaults]\nagent = \"codex\"\n").unwrap();

        let resp = apply_or_stage(
            &state,
            StagedOp::SetDefaultAgent {
                layer: Layer::Global,
                agent: "opencode".into(),
            },
            "saved",
        );
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            body.contains("config changed on disk — review and apply from the top bar"),
            "external-edit failure should keep its specific wording: {body}"
        );
        assert!(
            body.contains("changed on disk"),
            "should still surface the real error text: {body}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn apply_or_stage_does_not_blame_an_external_edit_for_an_unrelated_failure() {
        // Regression for the settings-page half of the read-only-backup bug:
        // a failure that has nothing to do with an external edit (here, the
        // same unwritable-backup-dir failure `Session::apply` was just fixed
        // for at the repo-scoped layer) must not be reported with the
        // external-edit wording, and must show the real chain.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!(
                "skipping apply_or_stage_does_not_blame_an_external_edit_for_an_unrelated_failure: \
                 running as root, permission bits are not enforced"
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt as _;

        let (d, gdir) = repo_with_global("[defaults]\nagent = \"claude\"\n");
        // No state dir injected: a Global-layer backup falls back to the
        // repo-scoped cache dir (`Session::open` with no `$HOME` behaves the
        // same way), which we then make unwritable.
        let state = state_with_session(d.path(), &gdir, None);

        let original_mode = std::fs::metadata(d.path()).unwrap().permissions().mode();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let resp = apply_or_stage(
            &state,
            StagedOp::SetDefaultAgent {
                layer: Layer::Global,
                agent: "codex".into(),
            },
            "saved",
        );

        // Restore unconditionally so `tempfile` can clean up.
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(original_mode)).unwrap();

        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            !body.contains("changed on disk"),
            "must not claim an external edit when the real cause was a write failure: {body}"
        );
        assert!(
            body.contains("apply failed"),
            "should surface a cause-neutral message: {body}"
        );
        assert!(
            body.contains("Permission denied"),
            "{{e:#}} should surface the full chain, not just the outermost \
             'backing up ...' frame: {body}"
        );
    }
}
