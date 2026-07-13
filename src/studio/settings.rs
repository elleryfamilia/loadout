//! The studio **Settings** page — the minimalist home for config the studio
//! can edit: ambient-learning consent (+ this machine's activation status),
//! the default launch agent, and the `[env]` exposure policy. Every config
//! write stages through the [`crate::studio::edit::Session`] pipeline (stage
//! → diff → apply); only machine-local learning state (the activation ack) is
//! written directly, because it is not config and is inert without the
//! synced flag.
//!
//! Not here (TOML-only, by design): `[sync]`, `[codex]`, `[learn]`'s
//! interval/scope/cli/model knobs, and the trust store (a future tenant).

use std::sync::{Arc, Mutex};

use maud::{html, Markup};

use crate::learn::state as learn_state;
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

fn render_page(state: &Arc<Mutex<StudioState>>, notice: Option<(bool, String)>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let learn_dir = state
        .lock()
        .unwrap()
        .inbox
        .as_ref()
        .map(|p| p.learn_dir.clone());
    let activation = learn_dir
        .as_deref()
        .and_then(learn_state::read_activation_at);
    let agents: Vec<String> = cfg.agents.iter().map(|a| a.id.clone()).collect();

    let mut html = page_fragment(
        &SettingsView {
            learn_enabled: cfg.learn.enabled,
            activated_here: activation.is_some(),
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
    learn_enabled: bool,
    activated_here: bool,
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
            (learning_section(v.learn_enabled, v.activated_here))
            (agent_section(&v.default_agent, &v.agents))
            div id="settings-env" { } // Task 8 fills this section
        }
    }
    .into_string()
}

/// The Learning section: honest consent copy (mirrors `load learn on`'s
/// consent block), current state (synced flag + this machine), and the
/// toggle. The toggle STAGES `[learn] enabled` — Apply writes it; the
/// activation ack is machine-local and written at confirm time (inert until
/// the flag lands).
fn learning_section(enabled: bool, activated_here: bool) -> Markup {
    html! {
        section class="settings-section" id="settings-learning" {
            h3 { "Learning" }
            p class="muted" {
                "loadout mines your recent agent sessions for durable, cross-project preferences "
                "and stages them as candidates you review in the Inbox. It runs "
                code { "load harvest --ambient" }
                " — a normal process, never a daemon — after sessions end, at most once per 6h per machine."
            }
            p class="settings-status" {
                @if enabled { "Learning is " strong { "on" } } @else { "Learning is " strong { "off" } }
                " in your synced config · this machine is "
                @if activated_here { strong { "activated" } } @else { strong { "not activated" } }
                "."
            }
            @if enabled && activated_here {
                button class="btn btn-ghost btn-sm" hx-post="/settings/learn/disable" hx-target="#main"
                    hx-confirm="Turn ambient learning off on this machine (and, once applied, everywhere your config syncs)?" {
                    (views::icon("power")) "Turn learning off"
                }
            } @else {
                button class="btn btn-primary btn-sm" hx-post="/settings/learn/enable" hx-target="#main"
                    hx-confirm="Enable ambient learning on this machine? The staged config change still needs Apply." {
                    (views::icon("power")) "Turn learning on"
                }
            }
        }
    }
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

/// `POST /settings/agent` — stage `[defaults] agent`.
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
    let staged = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::SetDefaultAgent {
            layer: crate::fragment::Layer::Global,
            agent,
        });
    respond_after_stage(state, staged, "staged the default agent — Apply to save")
}

/// `POST /settings/learn/enable` — write this machine's activation ack (inert
/// until the flag applies) and stage `[learn] enabled = true`.
pub fn learn_enable(state: &Arc<Mutex<StudioState>>) -> Resp {
    let Some(learn_dir) = state
        .lock()
        .unwrap()
        .inbox
        .as_ref()
        .map(|p| p.learn_dir.clone())
    else {
        return render_page(
            state,
            Some((true, "learning state is unavailable".to_string())),
        );
    };
    let ack = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&learn_dir).ok();
        let machine_id = learn_state::machine_id_at(&learn_dir)?;
        learn_state::write_activation_at(
            &learn_dir,
            &learn_state::Activation {
                machine_id,
                hostname: gethostname::gethostname().to_string_lossy().into_owned(),
                activated_at: crate::commands::now_rfc3339(),
            },
        )?;
        learn_state::reset_failures_at(&learn_dir);
        Ok(())
    })();
    if let Err(e) = ack {
        return render_page(
            state,
            Some((true, format!("could not activate this machine: {e}"))),
        );
    }
    let staged = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::SetLearnEnabled {
            layer: crate::fragment::Layer::Global,
            enabled: true,
        });
    respond_after_stage(
        state,
        staged,
        "learning staged ON — Apply to save; hooks register on Apply",
    )
}

/// `POST /settings/learn/disable` — remove the ack immediately (stopping is
/// the safe direction: this machine goes dormant even if Apply never comes)
/// and stage `[learn] enabled = false`.
pub fn learn_disable(state: &Arc<Mutex<StudioState>>) -> Resp {
    let learn_dir_opt = state
        .lock()
        .unwrap()
        .inbox
        .as_ref()
        .map(|p| p.learn_dir.clone());
    if let Some(learn_dir) = learn_dir_opt {
        let _ = learn_state::remove_activation_at(&learn_dir);
    }
    let staged = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::SetLearnEnabled {
            layer: crate::fragment::Layer::Global,
            enabled: false,
        });
    respond_after_stage(
        state,
        staged,
        "learning staged OFF — Apply to save; hooks deregister on Apply",
    )
}

/// Shared tail: re-render the page with a banner + the staged-indicator loader.
fn respond_after_stage(
    state: &Arc<Mutex<StudioState>>,
    staged: crate::Result<()>,
    ok_msg: &str,
) -> Resp {
    let notice = match staged {
        Ok(()) => (false, ok_msg.to_string()),
        Err(e) => (true, e.to_string()),
    };
    let mut resp = render_page(state, Some(notice));
    resp.body
        .extend_from_slice(views::staged_indicator_loader().as_bytes());
    resp
}
