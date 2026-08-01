//! The studio **Settings** page — the minimalist home for config the studio
//! can edit: today, the default launch agent. Every write stages through the
//! [`crate::studio::edit::Session`] pipeline, and — unlike content edits
//! (fragments/loadouts), which always wait for an explicit Apply — applies
//! immediately when nothing else is staged: a settings toggle should just
//! take effect, not sit "staged" (see [`apply_or_stage`]).
//!
//! Not here (TOML-only, by design): `[env]`, `[sync]`, `[codex]`, and the
//! trust store (a future tenant).

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
                Err(e) => (
                    true,
                    format!("config changed on disk — review and apply from the top bar: {e}"),
                ),
            }
        }
    };

    let mut resp = render_page(state, Some(notice));
    resp.body
        .extend_from_slice(views::staged_indicator_loader().as_bytes());
    resp
}
