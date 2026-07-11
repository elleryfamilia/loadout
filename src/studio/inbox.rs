//! The studio **Inbox** tab: the human review surface for learned candidate
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
use crate::learn::worker::{self, LogRecord};
use crate::profile::FragmentRef;
use crate::studio::edit::StagedOp;
use crate::studio::server::{Req, Resp};
use crate::studio::state::{self, StudioState};
use crate::studio::views;

/// Where the Inbox tab's three stores live, injected so router tests can point
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
    fn log_path(&self) -> PathBuf {
        self.learn_dir.join("log.jsonl")
    }
}

/// Clone the injected paths out from under the session mutex so folding and
/// rendering happen lock-free (snapshot-then-render).
fn paths(state: &Arc<Mutex<StudioState>>) -> Option<InboxPaths> {
    state.lock().unwrap().inbox.clone()
}

/// The number of `Pending` candidates — the shell's Inbox-tab badge count.
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

// --- GET /tab/inbox ----------------------------------------------------------

/// The Inbox tab body: pending/quarantined candidate cards + a dismissed list.
pub fn tab(state: &Arc<Mutex<StudioState>>) -> Resp {
    state.lock().unwrap().active_tab = "inbox".to_string();
    render_tab(state, None)
}

/// Fold the journals + read evidence (outside the mutex) and render the tab.
/// `notice` is an optional `(is_error, message)` banner shown above the list.
fn render_tab(state: &Arc<Mutex<StudioState>>, notice: Option<(bool, String)>) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(inbox_fragment(&[], &[], notice));
    };
    let fold = journal::fold_at(&paths.inbox_dir);
    let evidence_dir = paths.evidence_dir();

    let mut cards: Vec<CandidateCard> = Vec::new();
    let mut dismissed: Vec<SuppressedRow> = Vec::new();
    for c in fold.candidates.values() {
        match c.status {
            CandidateStatus::Pending | CandidateStatus::Quarantined => {
                cards.push(build_card(c, &evidence_dir))
            }
            CandidateStatus::Suppressed => dismissed.push(SuppressedRow {
                id: c.id.clone(),
                claim: c.claim.clone(),
            }),
            CandidateStatus::Promoted => {}
        }
    }
    // Newest first (last_seen desc) so the freshest suggestions lead.
    cards.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    Resp::html(inbox_fragment(&cards, &dismissed, notice))
}

/// One evidence file: `state_dir/learn/evidence/<id>.json`, written by the
/// worker as `{ id, quotes: [{ session_ref, quote }] }`. Only `quote` is read
/// here — session refs are display plumbing the tab doesn't surface.
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

/// One dismissed candidate row (the un-dismiss list).
struct SuppressedRow {
    id: String,
    claim: String,
}

// --- GET /inbox/<id>/promote (modal) -----------------------------------------

/// Render the promote modal for one candidate: editable claim, new-fragment vs
/// merge-into-existing, and a profile multi-select.
pub fn promote_form(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(views::error_fragment("learning state is unavailable"));
    };
    let fold = journal::fold_at(&paths.inbox_dir);
    let Some(cand) = fold.candidates.get(id) else {
        return Resp::html(views::error_fragment(
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
    Resp::html(promote_modal(
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
    // Errors from this modal render inside it (its `#inbox-modal-msg` slot),
    // not by replacing the tab behind the still-open modal.
    let err = |msg: String| Resp::html_retarget(views::error_fragment(&msg), "#inbox-modal-msg");

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

    // Success: close the modal, re-render the tab (the candidate still shows as
    // Pending — the disposition is queued, not yet flushed), and refresh the
    // staged indicator so Review/Apply appear.
    let mut resp = render_tab(
        state,
        Some((
            false,
            format!("staged promotion of “{frag_id}” — Apply to save"),
        )),
    );
    resp.body
        .extend_from_slice(views::modal_close_loader().as_bytes());
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

/// Append a `Dismiss` disposition immediately (no config change) and re-render.
pub fn dismiss(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(paths) = paths(state) else {
        return Resp::html(views::error_fragment("learning state is unavailable"));
    };
    match append_disposition(&paths, id, Action::Dismiss) {
        Ok(()) => render_tab(
            state,
            Some((false, "dismissed — Un-dismiss to restore".to_string())),
        ),
        Err(e) => Resp::html(views::error_fragment(&format!("could not dismiss: {e}"))),
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
        return render_tab(
            state,
            Some((
                true,
                format!("can't un-dismiss: that candidate is {what}, not dismissed"),
            )),
        );
    }
    match append_disposition(&paths, id, Action::Unsuppress) {
        Ok(()) => render_tab(state, Some((false, "restored to the inbox".to_string()))),
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

// --- GET /inbox/history ------------------------------------------------------

/// The run-log panel: every harvest run this machine logged, newest first.
pub fn history(state: &Arc<Mutex<StudioState>>) -> Resp {
    state.lock().unwrap().active_tab = "inbox".to_string();
    let Some(paths) = paths(state) else {
        return Resp::html(history_fragment(&[]));
    };
    let mut records = worker::read_log(&paths.log_path());
    records.reverse(); // read_log is oldest-first; show newest first
    Resp::html(history_fragment(&records))
}

// --- views -------------------------------------------------------------------

/// Percent-encode a path segment for a route. Candidate ids are sha-256 hex
/// (already URL-safe), but encoding defensively keeps the route honest.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The Inbox tab body. All strings shown here are candidate-derived; `maud`
/// escapes them by construction and none are `PreEscaped`.
fn inbox_fragment(
    cards: &[CandidateCard],
    dismissed: &[SuppressedRow],
    notice: Option<(bool, String)>,
) -> String {
    html! {
        div class="inbox" {
            div class="inbox-head" {
                h2 { "Inbox" }
                a class="btn btn-ghost btn-sm" hx-get="/inbox/history" hx-target="#main" {
                    (views::icon("clock")) "History"
                }
            }
            @if let Some((is_error, msg)) = &notice {
                div class=(if *is_error { "banner error" } else { "banner" }) {
                    span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                    div class="banner-body" { (msg) }
                }
            }
            @if cards.is_empty() && dismissed.is_empty() {
                p class="muted" {
                    "Nothing staged yet. Once learning is on ("
                    code { "load learn on" }
                    "), preferences harvested from your own sessions show up here to review."
                }
            }
            @for c in cards { (candidate_card(c)) }
            @if !dismissed.is_empty() {
                h3 class="inbox-subhead" { "Dismissed" }
                ul class="suppressions" {
                    @for s in dismissed {
                        li class="suppression" {
                            span class="suppression-claim" { (s.claim) }
                            button class="btn btn-ghost btn-sm"
                                hx-post=(format!("/inbox/{}/unsuppress", enc(&s.id))) hx-target="#main" {
                                (views::icon("refresh")) "Un-dismiss"
                            }
                        }
                    }
                }
            }
        }
    }
    .into_string()
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
                    hx-get=(format!("/inbox/{}/promote", enc(&c.id))) hx-target="#modal" {
                    (views::icon("check")) "Promote"
                }
                button class="btn btn-ghost btn-sm"
                    hx-post=(format!("/inbox/{}/dismiss", enc(&c.id))) hx-target="#main"
                    hx-confirm="Dismiss this suggestion? It won't return unless you un-dismiss it." {
                    (views::icon("x")) "Dismiss"
                }
            }
        }
    }
}

/// The promote modal. `claim` is the candidate's current text (escaped into the
/// textarea); `quarantined` adds the edit-required banner. `fragments` feed the
/// merge picker and `profiles` the multi-select.
fn promote_modal(
    id: &str,
    claim: &str,
    quarantined: bool,
    fragments: &[String],
    profiles: &[String],
) -> String {
    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal modal-lg" {
            form class="fragment-form" hx-post=(format!("/inbox/{}/promote", enc(id))) hx-target="#main" {
                div class="modal-head" {
                    h2 { "Promote suggestion" }
                    button class="icon-btn" type="button" title="Close" hx-get="/close" hx-target="#modal" { (views::icon("x")) }
                }
                div class="modal-body" {
                    // Save errors land here (via HX-Retarget), inside the modal.
                    div id="inbox-modal-msg" class="wf-editor-msg" {}
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
                }
                div class="modal-foot" {
                    button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                    button type="submit" class="btn btn-primary" { (views::icon("check")) "Promote" }
                }
            }
        }
    }
    .into_string()
}

/// The run-log history panel. Fields come from [`worker::LogRecord`] (the
/// stable read-back reader); it exposes ts, trigger, cli/model, sessions,
/// candidates, run duration, token usage, and outcome. Duration and usage are
/// the spend-audit signals — usage is shown verbatim so a metered run's cost is
/// legible on the machine that harvested it.
fn history_fragment(records: &[LogRecord]) -> String {
    html! {
        div class="inbox" {
            div class="inbox-head" {
                h2 { "Harvest history" }
                a class="btn btn-ghost btn-sm" hx-get="/tab/inbox" hx-target="#main" {
                    (views::icon("arrow-left")) "Back to inbox"
                }
            }
            @if records.is_empty() {
                p class="muted" { "No harvest runs recorded on this machine yet." }
            } @else {
                ul class="learn-log" {
                    @for r in records {
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
        }
    }
    .into_string()
}
