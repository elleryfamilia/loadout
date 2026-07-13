//! The studio **Inbox** drawer: the human review surface for learned candidate
//! preferences. Nothing enters a profile without a promote here — this module
//! is the own-your-markdown gate the release's trust story rests on.
//!
//! It reads the synced inbox journals ([`crate::learn::journal`]) and the
//! machine-local evidence store (quotes) written by the harvest worker, folds
//! them into a candidate view, and offers three actions:
//!
//! - **promote** — stage a fragment (new, or merged into an existing one) plus
//!   optional profile bindings through the existing [`edit::Session`] engine,
//!   and queue a `Promote` disposition that flushes to the journal **iff** the
//!   config write lands ([`edit::Session::queue_disposition`], mirroring the
//!   `pending_trust` precedent). The user reviews the diff and Applies like any
//!   other studio edit;
//! - **dismiss** — append a `Dismiss` disposition immediately (no config
//!   change); the candidate folds to `Suppressed` and drops out of the pending
//!   list;
//! - **un-dismiss** — append an `Unsuppress` disposition, but **only** for a
//!   candidate that is currently `Suppressed` (carry-forward C4: a stale
//!   `Unsuppress` on a `Promoted`/`Pending` id would, under latest-disposition-
//!   wins folding, demote it — so it is refused).
//!
//! Two safety rules bind the promote path:
//!
//! - **Re-gate on promote (C12):** user-edited claim text is run back through
//!   [`gate::gate_claim`] before it becomes a fragment. A `Quarantined` verdict
//!   blocks the promote and names the matched lint pattern — which is what makes
//!   "quarantined claims are promotable only after user edit" safe: the edit
//!   must actually clear the lint, not merely exist.
//! - **Escaping:** claim text, evidence quotes, and lint labels are all
//!   candidate-derived (ultimately from third-party transcript content). They
//!   render through `maud`, which HTML-escapes text and attribute values by
//!   construction. **Never** wrap any candidate-derived string in `PreEscaped`.
//!
//! Concurrency: the journal/evidence/log stores live on disk, not behind the
//! session mutex. Handlers clone the injected [`InboxPaths`] under the lock,
//! then fold and render **outside** it (the studio snapshot-then-render rule).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use maud::{html, Markup};
use serde::Deserialize;

use crate::learn::gate::{self, Gated};
use crate::learn::journal::{self, Action, Candidate, CandidateStatus, Disposition, Event};
use crate::learn::state as learn_state;
use crate::profile::FragmentRef;
use crate::studio::edit::StagedOp;
use crate::studio::server::{Req, Resp};
use crate::studio::settings;
use crate::studio::state::{self, StudioState};
use crate::studio::views;

/// Where the Inbox drawer's three stores live, injected so router tests can point
/// them at a fixture tempdir (the `recents_path` precedent).
#[derive(Clone)]
pub struct InboxPaths {
    /// The synced journals directory (`global_config_dir()/inbox`), holding one
    /// `journal-<machine-id>.jsonl` per machine.
    pub inbox_dir: PathBuf,
    /// The machine-local learn dir (`state_dir()/learn`) — holds the evidence
    /// store (`evidence/<id>.json`), the run log (`log.jsonl`), and this
    /// machine's persistent id (`machine-id`). Never syncs.
    pub learn_dir: PathBuf,
}

impl InboxPaths {
    fn evidence_dir(&self) -> PathBuf {
        self.learn_dir.join("evidence")
    }
    pub(crate) fn log_path(&self) -> PathBuf {
        self.learn_dir.join("log.jsonl")
    }
}

/// Clone the injected paths out from under the session mutex so folding and
/// rendering happen lock-free (snapshot-then-render).
fn paths(state: &Arc<Mutex<StudioState>>) -> Option<InboxPaths> {
    state.lock().unwrap().inbox.clone()
}

/// The number of `Pending` candidates — the shell's Inbox-icon badge count.
/// `Quarantined` candidates are held, not pending, so they don't count (same
/// number every discovery-line surface shows).
pub fn pending_count(state: &Arc<Mutex<StudioState>>) -> usize {
    match paths(state) {
        Some(p) => journal::fold_at(&p.inbox_dir).pending_count(),
        None => 0,
    }
}

/// The current UTC time in the fixed RFC 3339 whole-second "Z" form every
/// journal ts uses (so the multi-journal fold sorts lexicographically).
fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn values(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, v)| k == key && !v.is_empty())
        .map(|(_, v)| v.clone())
        .collect()
}

// --- GET /drawer/inbox --------------------------------------------------------

/// `GET /drawer/inbox` — the review-queue drawer. Never touches `active_tab`;
/// the drawer overlays the current destination.
pub fn drawer(state: &Arc<Mutex<StudioState>>) -> Resp {
    render_drawer(state, None)
}

/// Fold the journals + read evidence (outside the mutex) and render the drawer.
/// `notice` is an optional `(is_error, message)` banner shown above the list.
fn render_drawer(state: &Arc<Mutex<StudioState>>, notice: Option<(bool, String)>) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(inbox_drawer_fragment(&[], false, notice));
    };
    let fold = journal::fold_at(&paths.inbox_dir);
    let evidence_dir = paths.evidence_dir();
    let mut cards: Vec<CandidateCard> = Vec::new();
    for c in fold.candidates.values() {
        if matches!(
            c.status,
            CandidateStatus::Pending | CandidateStatus::Quarantined
        ) {
            cards.push(build_card(c, &evidence_dir));
        }
    }
    // Newest first (last_seen desc) so the freshest suggestions lead.
    cards.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    // Learning is "on here" when the synced flag is set AND this machine holds
    // an activation ack — the same two-part gate `learn_active` uses.
    let snap = state.lock().unwrap().snapshot();
    let learn_on = state::staged_config(&snap)
        .map(|cfg| cfg.learn.enabled)
        .unwrap_or(false)
        && learn_state::read_activation_at(&paths.learn_dir).is_some();
    Resp::html(inbox_drawer_fragment(&cards, learn_on, notice))
}

/// One evidence file: `state_dir/learn/evidence/<id>.json`, written by the
/// worker as `{ id, quotes: [{ session_ref, quote }] }`. Only `quote` is read
/// here — session refs are display plumbing the drawer doesn't surface.
#[derive(Deserialize)]
struct EvidenceFile {
    #[serde(default)]
    quotes: Vec<EvidenceQuote>,
}

#[derive(Deserialize)]
struct EvidenceQuote {
    #[serde(default)]
    quote: String,
}

/// Read this machine's local evidence quotes for a candidate, if any. The
/// evidence store never syncs (design Decision #5): a candidate harvested on
/// another machine has a synced journal entry here but no local evidence file.
fn read_quotes(evidence_dir: &Path, id: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(evidence_dir.join(format!("{id}.json"))) else {
        return Vec::new();
    };
    match serde_json::from_str::<EvidenceFile>(&text) {
        Ok(f) => f
            .quotes
            .into_iter()
            .map(|q| q.quote)
            .filter(|q| !q.trim().is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn build_card(c: &Candidate, evidence_dir: &Path) -> CandidateCard {
    let quotes = read_quotes(evidence_dir, &c.id);
    // Evidence locality is honest per machine: quotes show only where they were
    // harvested. A candidate observed somewhere but with no local evidence file
    // shows a note rather than another machine's verbatim transcript prose.
    let evidence_elsewhere = quotes.is_empty() && !c.machines.is_empty();
    CandidateCard {
        id: c.id.clone(),
        claim: c.claim.clone(),
        source: c.source.clone(),
        observation_count: c.observation_count,
        machine_count: c.machines.len(),
        quarantine_labels: c.quarantine_labels.clone(),
        quotes,
        evidence_elsewhere,
        last_seen: c.last_seen.clone(),
    }
}

/// One candidate card, fully prepared by the handler (no fs access in the view).
/// Every string field is candidate-derived and MUST stay `maud`-escaped.
struct CandidateCard {
    id: String,
    claim: String,
    source: String,
    observation_count: usize,
    machine_count: usize,
    quarantine_labels: Vec<String>,
    quotes: Vec<String>,
    evidence_elsewhere: bool,
    last_seen: String,
}

// --- GET /inbox/<id>/promote (drawer) ----------------------------------------

/// Render the promote form for one candidate, replacing the queue in the
/// drawer: editable claim, new-fragment vs merge-into-existing, and a profile
/// multi-select.
pub fn promote_form(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(views::drawer_error("learning state is unavailable"));
    };
    let fold = journal::fold_at(&paths.inbox_dir);
    let Some(cand) = fold.candidates.get(id) else {
        return Resp::html(views::drawer_error(
            "that suggestion is no longer in the inbox",
        ));
    };
    let quarantined = cand.status == CandidateStatus::Quarantined;

    let snap = state.lock().unwrap().snapshot();
    let (fragments, profiles) = match state::staged_config(&snap) {
        Ok(cfg) => (
            cfg.fragments
                .iter()
                .map(|f| f.id.clone())
                .collect::<Vec<_>>(),
            cfg.profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    Resp::html(promote_drawer_fragment(
        &cand.id,
        &cand.claim,
        quarantined,
        &fragments,
        &profiles,
    ))
}

// --- POST /inbox/<id>/promote ------------------------------------------------

/// Stage the promote: re-gate the (possibly edited) claim, stage a fragment op
/// plus any profile bindings, and queue the `Promote` disposition for the
/// ORIGINAL candidate id. Nothing is written until the user Applies — the
/// disposition then lands iff the config write lands.
pub fn promote(state: &Arc<Mutex<StudioState>>, id: &str, req: &Req) -> Resp {
    // Errors from this form render inside the drawer's own message slot (its
    // `#inbox-drawer-msg` slot), not by replacing the drawer's whole body.
    let err = |msg: String| Resp::html_retarget(views::error_fragment(&msg), "#inbox-drawer-msg");

    let Some(paths) = paths(state) else {
        return err("learning state is unavailable".to_string());
    };
    let pairs = state::parse_pairs(&req.body);
    let fold = journal::fold_at(&paths.inbox_dir);
    let Some(cand) = fold.candidates.get(id).cloned() else {
        return err("that suggestion is no longer in the inbox".to_string());
    };

    // The claim text to promote: the user's edit, or the original if untouched.
    let claim_text = value(&pairs, "claim")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| cand.claim.clone());

    // C12 re-gate: the edited text must pass the same injection lint rendered
    // guidance gets before it can become a fragment. A quarantined verdict
    // blocks the promote and names the pattern — so the edit must actually clear
    // the lint, not merely exist. A Clean verdict yields the redacted, capped
    // text that becomes the fragment guidance (idempotent for already-clean
    // claims).
    let gated = match gate::gate_claim(&claim_text) {
        Gated::Clean(text) => text,
        Gated::Quarantined { labels, .. } => {
            return err(format!(
                "This claim is still held by the injection lint ({}). Edit it to remove the flagged text before promoting.",
                labels.join(", ")
            ));
        }
    };
    // NEW-id awareness: an edited claim folds under a different candidate id than
    // the one in the inbox. We compute it (matching the worker/journal identity
    // rule) but the disposition below is keyed on the ORIGINAL id — that is what
    // suppression/promotion must key on, so this promote settles the candidate
    // the user is actually looking at.
    let _new_id = journal::candidate_id(&gated);

    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return err(e.to_string()),
    };

    let merge = value(&pairs, "mode") == Some("merge");
    // Build the fragment op: merge appends the claim to an existing fragment's
    // guidance (preserving its other fields); new creates a fresh markdown
    // fragment whose guidance is the gated claim.
    let (frag_id, op) = if merge {
        let merge_id = match value(&pairs, "merge_id")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(m) => m.to_string(),
            None => return err("pick a fragment to merge into".to_string()),
        };
        let Some(base) = cfg.fragments.iter().find(|f| f.id == merge_id) else {
            return err(format!("no fragment “{merge_id}” to merge into"));
        };
        let mut cap = base.clone();
        cap.guidance = if cap.guidance.trim().is_empty() {
            gated.clone()
        } else {
            format!("{}\n\n{}", cap.guidance.trim_end(), gated)
        };
        let layer = state
            .lock()
            .unwrap()
            .session
            .fragment_layer(&merge_id)
            .unwrap_or(crate::fragment::Layer::Global);
        (
            merge_id.clone(),
            StagedOp::EditFragment {
                layer,
                id: merge_id,
                cap: Box::new(cap),
            },
        )
    } else {
        // New fragment: name from the form, else a readable slug of the claim.
        let name = value(&pairs, "name")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_name(&gated));
        let synth = vec![
            ("name".to_string(), name),
            ("kind".to_string(), "markdown".to_string()),
            ("guidance".to_string(), gated.clone()),
        ];
        let cap = match state::fragment_from_form(None, &synth) {
            Ok(c) => c,
            Err(e) => return err(e.to_string()),
        };
        if cfg.fragments.iter().any(|f| f.id == cap.id) {
            return err(format!(
                "a fragment “{}” already exists — merge into it or choose another name",
                cap.id
            ));
        }
        (
            cap.id.clone(),
            StagedOp::CreateFragment {
                layer: crate::fragment::Layer::Global,
                cap: Box::new(cap),
            },
        )
    };

    let selected_profiles = values(&pairs, "profiles");
    let machine_id = match learn_state::machine_id_at(&paths.learn_dir) {
        Ok(m) => m,
        Err(e) => return err(format!("could not resolve this machine's id: {e}")),
    };

    // Stage the fragment op + profile bindings, then queue the disposition — all
    // under one lock. Any staging failure aborts before the disposition queues,
    // so a promote never records itself against a config edit that didn't stage.
    let staged = (|| -> crate::Result<()> {
        let mut s = state.lock().unwrap();
        s.session.stage(op)?;
        for prof in &selected_profiles {
            let fid = frag_id.clone();
            state::edit_profile(&mut s.session, prof, |p| {
                if !p.fragments.iter().any(|fr| fr.id() == fid) {
                    p.fragments.push(FragmentRef::Id(fid));
                }
            })?;
        }
        s.session.queue_disposition(
            &paths.inbox_dir,
            &machine_id,
            Disposition {
                id: id.to_string(), // ORIGINAL id — what promotion keys on
                action: Action::Promote,
                ts: now_ts(),
            },
        );
        Ok(())
    })();
    if let Err(e) = staged {
        return err(e.to_string());
    }

    // Success: re-render the drawer's queue (the candidate still shows as
    // Pending — the disposition is queued, not yet flushed), refresh the
    // badge, and refresh the staged indicator so Review/Apply appear. The form
    // is gone because `render_drawer` re-renders the queue body wholesale.
    let mut resp = render_drawer(
        state,
        Some((
            false,
            format!("staged promotion of “{frag_id}” — Apply to save"),
        )),
    );
    resp.body
        .extend_from_slice(views::inbox_badge_loader().as_bytes());
    resp.body
        .extend_from_slice(views::staged_indicator_loader().as_bytes());
    resp
}

/// A readable default fragment id/name from a claim: the first few words,
/// slugged, or `suggestion` when nothing usable survives.
fn default_name(claim: &str) -> String {
    let words: Vec<&str> = claim.split_whitespace().take(6).collect();
    let slug = state::slug(&words.join(" "));
    if slug.is_empty() {
        "suggestion".to_string()
    } else {
        slug
    }
}

// --- POST /inbox/<id>/dismiss ------------------------------------------------

/// Append a `Dismiss` disposition immediately (no config change), re-render the
/// drawer, and append a badge-refresh loader (the pending count just dropped).
pub fn dismiss(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(views::drawer_error("learning state is unavailable"));
    };
    match append_disposition(&paths, id, Action::Dismiss) {
        Ok(()) => {
            let mut resp = render_drawer(
                state,
                Some((
                    false,
                    "dismissed — restore it under Settings → Learning".to_string(),
                )),
            );
            resp.body
                .extend_from_slice(views::inbox_badge_loader().as_bytes());
            resp
        }
        Err(e) => Resp::html(views::drawer_error(&format!("could not dismiss: {e}"))),
    }
}

// --- POST /inbox/<id>/unsuppress ---------------------------------------------

/// Un-dismiss a candidate — but only if it is **currently** `Suppressed`.
///
/// Carry-forward C4: status folds from each id's latest disposition, so an
/// `Unsuppress` stamped against a `Promoted` (or already-`Pending`) id would
/// win the fold and demote it back to observation-derived status. We fold
/// first, check the suppressed set, and refuse otherwise — an `Unsuppress` only
/// ever lands on a candidate that a `Dismiss` currently governs.
///
/// The dismissed list lives under Settings → Learning (not the drawer), so
/// both the refusal and the success path re-render the Settings page.
pub fn unsuppress(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(views::error_fragment("learning state is unavailable"));
    };
    let fold = journal::fold_at(&paths.inbox_dir);
    if !fold.suppressed.contains(id) {
        // Not currently dismissed → refuse (a stale un-dismiss must never demote
        // a promoted or pending candidate).
        let what = fold
            .candidates
            .get(id)
            .map(|c| status_word(c.status))
            .unwrap_or("not in the inbox");
        return settings::render_page(
            state,
            Some((
                true,
                format!("can't un-dismiss: that candidate is {what}, not dismissed"),
            )),
        );
    }
    match append_disposition(&paths, id, Action::Unsuppress) {
        Ok(()) => {
            let mut resp =
                settings::render_page(state, Some((false, "restored to the inbox".to_string())));
            resp.body
                .extend_from_slice(views::inbox_badge_loader().as_bytes());
            resp
        }
        Err(e) => Resp::html(views::error_fragment(&format!("could not un-dismiss: {e}"))),
    }
}

fn status_word(s: CandidateStatus) -> &'static str {
    match s {
        CandidateStatus::Pending => "pending",
        CandidateStatus::Promoted => "already promoted",
        CandidateStatus::Suppressed => "dismissed",
        CandidateStatus::Quarantined => "held by the injection lint",
    }
}

/// Append one disposition to THIS machine's journal (`journal-<machine-id>`).
fn append_disposition(paths: &InboxPaths, id: &str, action: Action) -> crate::Result<()> {
    let machine_id = learn_state::machine_id_at(&paths.learn_dir)?;
    let ev = Event::Disposition(Disposition {
        id: id.to_string(),
        action,
        ts: now_ts(),
    });
    journal::append_events_at(&paths.inbox_dir, &machine_id, &[ev])?;
    Ok(())
}

// --- views -------------------------------------------------------------------

/// The Inbox drawer body. All strings shown here are candidate-derived; `maud`
/// escapes them by construction and none are `PreEscaped`. Dismissed rows and
/// harvest history live under Settings → Learning — this drawer shows only
/// the pending/quarantined queue, with a footer link to Settings.
fn inbox_drawer_fragment(
    cards: &[CandidateCard],
    learn_on: bool,
    notice: Option<(bool, String)>,
) -> String {
    let body = html! {
        @if let Some((is_error, msg)) = &notice {
            div class=(if *is_error { "banner error" } else { "banner" }) {
                span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                div class="banner-body" { (msg) }
            }
        }
        @if cards.is_empty() {
            @if learn_on {
                p class="muted" { "You're all caught up — nothing to review." }
            } @else {
                p class="muted" { "Learning is off." }
                button class="btn btn-primary btn-sm" hx-get="/settings" hx-target="#main" {
                    (views::icon("gear")) "Turn it on in Settings"
                }
            }
        }
        @for c in cards { (candidate_card(c)) }
    };
    let foot = Some(html! {
        button class="btn btn-ghost btn-sm" hx-get="/settings" hx-target="#main" {
            (views::icon("gear")) "Learning settings & history"
        }
    });
    views::drawer("Inbox", body, foot)
}

fn candidate_card(c: &CandidateCard) -> Markup {
    html! {
        div class="inbox-card" {
            @if !c.quarantine_labels.is_empty() {
                // Labels come from the injection lint over candidate-derived
                // claim text — escaped, never PreEscaped.
                div class="banner error quarantine-banner" {
                    span class="banner-icon" { (views::icon("shield")) }
                    div class="banner-body" {
                        p { "Held by the injection lint: " (c.quarantine_labels.join(", ")) "." }
                        p class="muted" { "Edit the claim when you promote — the edit is re-checked and must clear the lint." }
                    }
                }
            }
            // Untrusted claim text — escaped by maud.
            p class="candidate-claim" { (c.claim) }
            div class="candidate-meta muted" {
                span class="source-badge" { (c.source) }
                span {
                    (c.observation_count) " observation"
                    @if c.observation_count != 1 { "s" }
                }
                @if c.machine_count > 1 { span { (c.machine_count) " machines" } }
            }
            @if !c.quotes.is_empty() {
                ul class="evidence" {
                    // Untrusted evidence quotes — escaped by maud.
                    @for q in &c.quotes { li class="quote" { (q) } }
                }
            } @else if c.evidence_elsewhere {
                p class="muted evidence-note" {
                    "Evidence was recorded on another machine — quotes stay local and don't sync."
                }
            }
            div class="candidate-actions" {
                button class="btn btn-primary btn-sm"
                    hx-get=(format!("/inbox/{}/promote", views::enc(&c.id))) hx-target="#drawer" {
                    (views::icon("check")) "Promote"
                }
                button class="btn btn-ghost btn-sm"
                    hx-post=(format!("/inbox/{}/dismiss", views::enc(&c.id))) hx-target="#drawer"
                    hx-confirm="Dismiss this suggestion? It won't return unless you un-dismiss it." {
                    (views::icon("x")) "Dismiss"
                }
            }
        }
    }
}

/// The promote form, rendered as the drawer's content (replacing the queue).
/// `claim` is the candidate's current text (escaped into the textarea);
/// `quarantined` adds the edit-required banner. `fragments` feed the merge
/// picker and `profiles` the multi-select. Same form fields as ever —
/// `promote()`'s parsing is unchanged.
fn promote_drawer_fragment(
    id: &str,
    claim: &str,
    quarantined: bool,
    fragments: &[String],
    profiles: &[String],
) -> String {
    let body = html! {
        form class="fragment-form" hx-post=(format!("/inbox/{}/promote", views::enc(id))) hx-target="#drawer" {
            // Save errors land here (via HX-Retarget), inside the drawer.
            div id="inbox-drawer-msg" class="wf-editor-msg" {}
            @if quarantined {
                div class="banner error" {
                    span class="banner-icon" { (views::icon("shield")) }
                    div class="banner-body" {
                        p { "This claim was held by the injection lint. Edit it to remove the flagged text — the edit is re-checked when you promote." }
                    }
                }
            }
            label class="field" {
                span class="field-label" { "claim" span class="field-hint" { "becomes the fragment's guidance" } }
                // Untrusted claim text as the textarea's initial value.
                textarea name="claim" class="wf-step-textarea" rows="3" required { (claim) }
            }
            fieldset class="field inbox-mode" {
                label class="radio" {
                    input type="radio" name="mode" value="new" checked;
                    " New fragment"
                }
                label class="field" {
                    span class="field-label" { "name" span class="field-hint" { "becomes the fragment id — optional" } }
                    input type="text" name="name" placeholder="e.g. prefer-pnpm";
                }
                @if !fragments.is_empty() {
                    label class="radio" {
                        input type="radio" name="mode" value="merge";
                        " Merge into an existing fragment"
                    }
                    label class="field" {
                        span class="field-label" { "fragment" }
                        select name="merge_id" {
                            @for f in fragments { option value=(f) { (f) } }
                        }
                    }
                }
            }
            @if !profiles.is_empty() {
                fieldset class="field inbox-profiles" {
                    span class="field-label" { "add to loadouts" span class="field-hint" { "optional" } }
                    @for p in profiles {
                        label class="check" {
                            input type="checkbox" name="profiles" value=(p);
                            " " (p)
                        }
                    }
                }
            }
            div class="drawer-actions" {
                button type="button" class="btn btn-ghost" hx-get="/drawer/inbox" hx-target="#drawer" {
                    (views::icon("arrow-left")) "Back"
                }
                button type="submit" class="btn btn-primary" { (views::icon("check")) "Promote" }
            }
        }
    };
    views::drawer("Promote suggestion", body, None)
}
