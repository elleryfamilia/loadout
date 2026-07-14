//! The studio **Settings** page — the minimalist home for config the studio
//! can edit: ambient-learning consent (+ this machine's activation status)
//! and the default launch agent. Every write stages through the
//! [`crate::studio::edit::Session`] pipeline, and — unlike content edits
//! (fragments/loadouts), which always wait for an explicit Apply — applies
//! immediately when nothing else is staged: a settings toggle should just
//! take effect, not sit "staged" (see [`apply_or_stage`]). Only machine-local
//! learning state (the activation ack) is written directly, because it is not
//! config and is inert without the synced flag.
//!
//! Not here (TOML-only, by design): `[env]`, `[sync]`, `[codex]`, `[learn]`'s
//! interval/scope/cli/model knobs, and the trust store (a future tenant).

use std::sync::{Arc, Mutex};

use maud::{html, Markup};

use crate::learn::journal::{self, CandidateStatus};
use crate::learn::state as learn_state;
use crate::learn::worker::{self, LogRecord};
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
    let inbox_paths = state.lock().unwrap().inbox.clone();
    let learn_dir = inbox_paths.as_ref().map(|p| p.learn_dir.clone());
    let activation = learn_dir
        .as_deref()
        .and_then(learn_state::read_activation_at);
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

    // Harvest history + dismissed suggestions, folded/read outside the mutex
    // (snapshot-then-render): the run log is newest-first for display, and the
    // suppressed set comes straight out of the journal fold's `Suppressed`
    // status.
    let (log_records, suppressed) = match &inbox_paths {
        Some(p) => {
            let mut records = worker::read_log(&p.log_path());
            records.reverse(); // read_log is oldest-first; show newest first
            let fold = journal::fold_at(&p.inbox_dir);
            let suppressed: Vec<(String, String)> = fold
                .candidates
                .values()
                .filter(|c| c.status == CandidateStatus::Suppressed)
                .map(|c| (c.id.clone(), c.claim.clone()))
                .collect();
            (records, suppressed)
        }
        None => (Vec::new(), Vec::new()),
    };

    let mut html = page_fragment(
        &SettingsView {
            learn_enabled: cfg.learn.enabled,
            activated_here: activation.is_some(),
            default_agent: cfg.default_agent.clone(),
            agents,
            log_records,
            suppressed,
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
    /// This machine's harvest run log, newest first.
    log_records: Vec<LogRecord>,
    /// `(candidate_id, claim)` for every currently-`Suppressed` candidate.
    suppressed: Vec<(String, String)>,
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
            (learning_section(v.learn_enabled, v.activated_here, &v.log_records, &v.suppressed))
            (agent_section(&v.default_agent, &v.agents))
        }
    }
    .into_string()
}

/// The Learning section: honest consent copy (mirrors `load learn on`'s
/// consent block), current state (synced flag + this machine), the toggle,
/// this machine's harvest history, and any dismissed suggestions with an
/// un-dismiss control. The toggle sets `[learn] enabled` via
/// [`apply_or_stage`] — applied immediately on a clean session, or staged
/// alongside other pending edits; the activation ack is machine-local and
/// written directly at confirm time (inert until the flag lands).
fn learning_section(
    enabled: bool,
    activated_here: bool,
    log_records: &[LogRecord],
    suppressed: &[(String, String)],
) -> Markup {
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
                @if enabled && activated_here {
                    "Learning is " span class="learn-on-pill" { "on" }
                } @else if enabled {
                    "Learning is " strong { "on" }
                } @else {
                    "Learning is " strong { "off" }
                }
                " in your synced config · this machine is "
                @if activated_here { strong { "activated" } } @else { strong { "not activated" } }
                "."
            }
            @if enabled && activated_here {
                button class="btn btn-ghost btn-sm" hx-post="/settings/learn/disable" hx-target="#main"
                    hx-confirm="Turn ambient learning off on this machine (and, once synced, everywhere your config syncs)?" {
                    (views::icon("power")) "Turn learning off"
                }
            } @else {
                button class="btn btn-primary btn-sm" hx-post="/settings/learn/enable" hx-target="#main"
                    hx-confirm="Enable ambient learning on this machine?" {
                    (views::icon("power")) "Turn learning on"
                }
            }
            h4 class="inbox-subhead" { "Harvest history" }
            @if log_records.is_empty() {
                p class="muted" { "No harvest runs recorded on this machine yet." }
            } @else {
                ul class="learn-log" {
                    @for r in log_records {
                        li class="learn-log-row" {
                            span class=(format!("log-outcome log-{}", r.outcome)) { (r.outcome) }
                            span class="log-ts muted" { (r.ts) }
                            span class="log-trigger muted" { (r.trigger) }
                            @if let Some(cli) = &r.cli {
                                span class="log-cli muted" {
                                    (cli)
                                    @if let Some(model) = &r.model { " (" (model) ")" }
                                }
                            }
                            span class="log-counts muted" {
                                (r.sessions) " sessions · " (r.candidates) " candidates"
                            }
                            @if let Some(ms) = r.duration_ms {
                                span class="log-duration muted" { (ms) "ms" }
                            }
                            // Token usage — the spend-audit signal. Untrusted
                            // CLI-derived text, escaped by maud.
                            @if let Some(usage) = &r.usage {
                                span class="log-usage muted" { "usage " (usage) }
                            }
                        }
                    }
                }
            }
            @if !suppressed.is_empty() {
                h4 class="inbox-subhead" { "Dismissed suggestions" }
                ul class="suppressions" {
                    @for (id, claim) in suppressed {
                        li class="suppression" {
                            // Untrusted claim text — escaped by maud.
                            span class="suppression-claim" { (claim) }
                            button class="btn btn-ghost btn-sm"
                                hx-post=(format!("/inbox/{}/unsuppress", views::enc(id))) hx-target="#main" {
                                (views::icon("refresh")) "Un-dismiss"
                            }
                        }
                    }
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

/// `POST /settings/learn/enable` — write this machine's activation ack (inert
/// until the flag applies) and set `[learn] enabled = true`.
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
    apply_or_stage(
        state,
        StagedOp::SetLearnEnabled {
            layer: crate::fragment::Layer::Global,
            enabled: true,
        },
        "learning is on",
    )
}

/// `POST /settings/learn/disable` — remove the ack immediately (stopping is
/// the safe direction: this machine goes dormant even if Apply never comes)
/// and set `[learn] enabled = false`.
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
    apply_or_stage(
        state,
        StagedOp::SetLearnEnabled {
            layer: crate::fragment::Layer::Global,
            enabled: false,
        },
        "learning is off",
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
    let is_learn_toggle = matches!(op, StagedOp::SetLearnEnabled { .. });
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
                    // Same post-apply side effects `handle_apply` runs, so a
                    // settings save behaves identically whichever button
                    // triggered the write.
                    if is_learn_toggle {
                        crate::studio::server::learn_bootstrap_after_apply(state);
                    }
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
    // A landed (or reverted) learn toggle can change a queued promote's
    // disposition, so refresh the badge unconditionally — cheap, idempotent.
    if is_learn_toggle {
        resp.body
            .extend_from_slice(views::inbox_badge_loader().as_bytes());
    }
    resp
}
