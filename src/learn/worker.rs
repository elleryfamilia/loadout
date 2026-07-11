//! The harvest worker: the one-per-machine, fenced, bounded orchestration
//! that turns new session transcripts into staged inbox candidates.
//!
//! [`run_harvest`] implements the design's eight steps (see
//! `.loadout/workflow/artifacts/design-learning.md`), assembling every piece
//! the earlier tasks built — the single-writer [`lock`], the two throttle
//! [`state`] stamps, the transcript [`readers`], [`slices`] assembly, the
//! [`extract`] prompt/parse, the [`gate`] claim/quote defenses, the
//! [`journal`] inbox store, and the [`watermarks`] resume store — into one
//! detached, logged, spend-bounded pass.
//!
//! ## Fencing discipline (the release's cost ceiling)
//!
//! A fencing token is minted when the lock is acquired ([`lock::acquire_at`]),
//! **before any other side effect**. Every numbered side effect below
//! re-checks [`lock::LockGuard::still_held`] first and aborts (writing
//! nothing) if the on-disk token is no longer ours — a worker that was
//! suspended, had its lock reclaimed by a later run, and then resumed finds a
//! different token and stops before spending or writing. The watermark store
//! is additionally monotonic ([`watermarks::Watermarks::advance`]) so even a
//! missed check cannot double-harvest.
//!
//! ## Two-stamp semantics
//!
//! A short **scan stamp** is written at step 2 on every worker start (it
//! bounds scan thrash). The 6h **spend stamp** is written only at step 6,
//! immediately before the one paid extraction call — so an empty scan cannot
//! burn the tick, while a crash-looping worker still costs at most one call
//! per interval because the stamp precedes the spend. The throttle *checks*
//! (is the scan stamp debounced? is the spend stamp past `learn.interval`?)
//! live in the trigger fast path (a later task); this worker only *writes* the
//! stamps as it proceeds. A manual `load harvest` bypasses the interval check
//! upstream but still writes the spend stamp here (a manual run resets the
//! ambient tick — the cheapest honest semantics).

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::config::{self, Config, LearnScope};
use crate::learn::gate::{self, Gated};
use crate::learn::journal::{self, Event, Observed, ProducedBy, SessionRef};
use crate::learn::readers::{claude, codex, gemini, SessionSlice};
use crate::learn::watermarks::Watermarks;
use crate::learn::{agent_cli, extract, lock, slices, state, trigger};

/// Hard internal wall-clock deadline for one run. The lock treats a holder
/// older than `2 * DEADLINE` as stale, and the extraction CLI call is bounded
/// by (the remainder of) this same budget via `output_with_timeout`.
const DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Outer sanity cap (in `chars()`) applied to each extracted claim **before**
/// the claim gate (cross-task contract C11). The gate keeps a *quarantined*
/// claim whole (uncapped) for display, so without an outer bound a
/// pathological extraction could journal an unbounded blob; this hard-truncates
/// at a generous, deterministic length so journal growth stays bounded while
/// leaving ordinary claims (well under this) untouched.
const CLAIM_SANITY_CAP: usize = 10_000;

/// At most this many evidence quotes are kept per candidate (design doc's
/// evidence-store note: 5 × ~200 chars, the length cap enforced by
/// [`gate::gate_quote`]).
const MAX_QUOTES: usize = 5;

/// What one harvest run did, enough for the caller to print a one-line summary
/// and for a test to assert on. Mirrors the run-log entry.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub outcome: Outcome,
    /// `"manual"` or `"ambient"`.
    pub trigger: &'static str,
    pub cli: Option<String>,
    pub model: Option<String>,
    /// Sessions actually sent to the extraction call (post scope/caps).
    pub sessions: usize,
    /// New candidates journaled this run.
    pub candidates: usize,
    /// Of `candidates`, how many the claim gate quarantined.
    pub quarantined: usize,
    /// Sessions dropped by the per-run caps (drop-don't-defer).
    pub dropped_over_cap: usize,
    pub duration_ms: u128,
    /// Per-store skip reasons surfaced from the scan (contract C9).
    pub skipped: Vec<String>,
}

/// The terminal state of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A live worker already holds the lock; this invocation did nothing.
    Busy,
    /// The fencing token went foreign mid-run; aborted before any write.
    Fenced,
    /// No eligible new content; a no-op run (spend stamp untouched).
    Empty,
    /// Eligible content exists but no extraction CLI is installed; nothing
    /// spent, nothing advanced (retry once a CLI appears).
    NoCli,
    /// The watermark store is corrupt; run refused, `load learn reset` needed.
    Corrupt,
    /// The extraction call failed to spawn or produced unusable/malformed
    /// output; the spend stamp already burned the tick, nothing else changed.
    Failed,
    /// The internal wall-clock deadline was hit before the call; failed run.
    Deadline,
    /// A successful extraction: candidates folded, watermarks advanced.
    Extracted,
}

impl Outcome {
    /// The stable string written to the run log / returned for display.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Busy => "busy",
            Outcome::Fenced => "fenced",
            Outcome::Empty => "empty",
            Outcome::NoCli => "no_cli",
            Outcome::Corrupt => "corrupt_watermarks",
            Outcome::Failed => "failed",
            Outcome::Deadline => "deadline_exceeded",
            Outcome::Extracted => "extracted",
        }
    }
}

/// The one paid side effect, behind a trait so the deterministic tests inject a
/// stub in place of a real spawned agent CLI. Production is [`RealExtractor`].
trait Extractor {
    fn cli_id(&self) -> &str;
    fn model(&self) -> &str;
    fn invoke(
        &self,
        prompt: &str,
        work_dir: &Path,
        deadline: Duration,
    ) -> Result<agent_cli::InvokeOut>;
}

/// The production extractor: a resolved [`agent_cli::CliChoice`] invoked via
/// [`agent_cli::invoke`] (one bounded, hygiene-flagged agent-CLI spawn).
struct RealExtractor {
    choice: agent_cli::CliChoice,
}

impl RealExtractor {
    fn new(choice: agent_cli::CliChoice) -> Self {
        Self { choice }
    }
}

impl Extractor for RealExtractor {
    fn cli_id(&self) -> &str {
        self.choice.cli_id
    }
    fn model(&self) -> &str {
        &self.choice.model
    }
    fn invoke(
        &self,
        prompt: &str,
        work_dir: &Path,
        deadline: Duration,
    ) -> Result<agent_cli::InvokeOut> {
        agent_cli::invoke(&self.choice, prompt, work_dir, deadline)
    }
}

/// Everything one run needs, resolved from [`Config`] in production and hand-
/// built from tempdirs in tests. Bundling the paths keeps the fenced body a
/// straight-line sequence rather than re-deriving locations at every step.
struct Ctx {
    learn_dir: PathBuf,
    inbox_dir: PathBuf,
    work_dir: PathBuf,
    evidence_dir: PathBuf,
    home: PathBuf,
    watermarks_path: PathBuf,
    scan_stamp: PathBuf,
    spend_stamp: PathBuf,
    log_path: PathBuf,
    scope: LearnScope,
    /// `(id, description)` for every configured fragment — the CURRENT
    /// FRAGMENTS list the prompt anchors against, and the exact-duplicate
    /// dedupe source.
    fragments: Vec<(String, String)>,
    machine_id: String,
    now_utc: DateTime<Utc>,
    trigger: &'static str,
    /// Seed session ids a session-end hook named as just-ended (they bypass the
    /// claude reader's quiescence wait). Empty in production — step 3 merges this
    /// with the on-disk eligibility hints ([`trigger::read_hints`]) into the set
    /// actually passed to the readers; it stays a field so tests can inject
    /// hook-named sessions directly. Scanned-session exclusions still apply to
    /// hook-named sessions exactly as to scanned ones.
    hooked: BTreeSet<String>,
}

impl Ctx {
    fn now_unix(&self) -> i64 {
        self.now_utc.timestamp()
    }

    /// The run's wall-clock as a [`SystemTime`] for the readers' quiescence
    /// gate. Built by hand (chrono is compiled without its default features)
    /// rather than via `From`, and clamped at the epoch (real/test clocks are
    /// always well past it).
    fn now_sys(&self) -> SystemTime {
        let secs = self.now_utc.timestamp().max(0) as u64;
        SystemTime::UNIX_EPOCH + Duration::new(secs, self.now_utc.timestamp_subsec_nanos())
    }

    /// The run's timestamp in the fixed RFC3339 whole-second UTC "Z" form
    /// (contract C2) — the shape every ts this worker *mints* (Observed.ts,
    /// baseline, log ts) must take so the journal fold sorts lexicographically.
    fn now_ts(&self) -> String {
        self.now_utc.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

/// Run one harvest against `cfg`. `manual` marks a user-typed `load harvest`
/// (trigger `"manual"`); `false` is a throttled ambient run (`"ambient"`).
/// Never panics on a normal failure — every abnormal path degrades to a
/// [`RunOutcome`] (and a log line where the lock is held). See the module doc
/// for the fencing and two-stamp guarantees.
pub fn run_harvest(cfg: &Config, manual: bool) -> Result<RunOutcome> {
    let learn_dir = state::learn_dir()
        .ok_or_else(|| anyhow!("cannot resolve the learning state dir (no home)"))?;
    let inbox_dir = config::global_config_dir()
        .ok_or_else(|| anyhow!("cannot resolve the inbox dir (no home)"))?
        .join("inbox");
    let home =
        config::home_dir().ok_or_else(|| anyhow!("cannot read transcript stores (no HOME)"))?;
    let machine_id =
        state::machine_id_at(&learn_dir).context("resolving this machine's learning id")?;

    let ctx = Ctx {
        work_dir: learn_dir.join("work"),
        evidence_dir: learn_dir.join("evidence"),
        watermarks_path: learn_dir.join("watermarks.json"),
        scan_stamp: learn_dir.join("scan-stamp"),
        spend_stamp: learn_dir.join("spend-stamp"),
        log_path: learn_dir.join("log.jsonl"),
        scope: cfg.learn.scope,
        fragments: cfg
            .fragments
            .iter()
            .map(|f| (f.id.clone(), f.description.clone().unwrap_or_default()))
            .collect(),
        machine_id,
        now_utc: Utc::now(),
        trigger: if manual { "manual" } else { "ambient" },
        hooked: BTreeSet::new(),
        learn_dir,
        inbox_dir,
        home,
    };

    let extractor = agent_cli::select(&cfg.learn).map(RealExtractor::new);
    run_harvest_ctx(&ctx, extractor.as_ref().map(|e| e as &dyn Extractor))
}

/// Step 1 (lock) + dispatch to the fenced body. A `Busy` lock exits quietly; a
/// `Reclaimed` stale lock counts as one failure, recorded immediately (C7),
/// then proceeds. The guard is released after the body regardless of outcome
/// ([`lock::LockGuard::release`] no-ops if the token went foreign).
fn run_harvest_ctx(ctx: &Ctx, extractor: Option<&dyn Extractor>) -> Result<RunOutcome> {
    let guard = match lock::acquire_at(&ctx.learn_dir, DEADLINE, ctx.now_unix())? {
        lock::Acquire::Busy => return Ok(empty_outcome(ctx, Outcome::Busy)),
        lock::Acquire::Held(g) => g,
        lock::Acquire::Reclaimed(g) => {
            state::record_failure_at(&ctx.learn_dir);
            g
        }
    };
    let outcome = run_body(ctx, &guard, extractor);
    guard.release();
    outcome
}

/// Steps 2–8, each fenced. Returns [`Outcome::Fenced`] (writing nothing) the
/// moment the token goes foreign.
fn run_body(
    ctx: &Ctx,
    guard: &lock::LockGuard,
    extractor: Option<&dyn Extractor>,
) -> Result<RunOutcome> {
    let start = Instant::now();

    // --- Step 2: scan stamp (bounds scan thrash) -------------------------
    if !guard.still_held() {
        return Ok(empty_outcome(ctx, Outcome::Fenced));
    }
    // Swallowing this write error is tolerable: the scan stamp only debounces
    // free re-scans. A missing stamp means the fast path re-scans sooner —
    // wasted stat()s, never money. Contrast the SPEND stamp at step 6, whose
    // write failure must abort the run before the paid call.
    let _ = state::write_stamp(&ctx.scan_stamp);

    // --- Step 3: watermarks, scan, assemble ------------------------------
    let mut wm = Watermarks::load_from(&ctx.watermarks_path);
    if wm.corrupt() {
        // The warning is deliberately NOT fenced: the store is corrupt
        // regardless of who holds the lock, and stderr advice is not shared
        // state. The failure counter and the log ARE shared state the healthy
        // holder owns, so both writes sit behind the fence — a fenced-out
        // worker writes nothing, consistent with every other branch.
        crate::warn_user!(
            "learning watermark store is corrupt — run `load learn reset` to re-baseline \
             (it harvests forward only, never re-mines old sessions)"
        );
        if !guard.still_held() {
            return Ok(empty_outcome(ctx, Outcome::Fenced));
        }
        state::record_failure_at(&ctx.learn_dir);
        log_run(ctx, &LogFields::new(ctx, Outcome::Corrupt, start.elapsed()));
        return Ok(empty_outcome(ctx, Outcome::Corrupt));
    }
    // Fix the 14-day age-cutoff baseline on the first ever run (idempotent;
    // `load learn on` may also set it — same value, no conflict). Only
    // persisted by the step-8 save.
    wm.set_baseline_if_absent(&ctx.now_ts());

    // Session-end eligibility hints: hook-named sessions bypass the readers'
    // quiescence wait. Read them here (before the scan) into the readers'
    // `hooked` set; the hint files are deleted only on the success path at step
    // 8, and only while the fence still holds, so a fenced-out, failed, or
    // no-CLI run leaves them for the next worker to retry.
    let (hooked, hint_paths) = trigger::read_hints(&ctx.learn_dir, &ctx.hooked);

    let now_sys = ctx.now_sys();
    let mut scanned: Vec<SessionSlice> = Vec::new();
    scanned.extend(claude::scan_claude(&ctx.home, &wm, now_sys, &hooked));
    scanned.extend(codex::scan_codex(&ctx.home, &wm, now_sys));
    let work_hash = gemini::gemini_project_hash(&ctx.work_dir);
    scanned.extend(gemini::scan_gemini(&ctx.home, &wm, now_sys, &work_hash));
    // Per-store skip reasons would populate here (contract C9's field); the
    // readers fail closed silently today, so this stays empty until a later
    // task threads richer reasons out of them.
    let skipped: Vec<String> = Vec::new();

    let assembled = slices::assemble(
        scanned.clone(),
        &slices::Caps::default(),
        ctx.scope,
        wm.baseline(),
        &ctx.work_dir,
    );

    // Empty assembly → no-op log, exit; spend stamp untouched (two-stamp
    // semantics) and watermarks NOT advanced. That preservation is exactly as
    // narrow as this branch: it holds only when the ENTIRE scan assembled to
    // nothing. In a MIXED successful run, step 8 advances past everything in
    // `scanned` — in-scope and out-of-scope alike — so out-of-scope sessions
    // consumed alongside a paid extraction do NOT resurface if their repo is
    // adopted later (drop-don't-defer applies to scope drops too).
    if assembled.slices.is_empty() {
        if !guard.still_held() {
            return Ok(empty_outcome(ctx, Outcome::Fenced));
        }
        let mut fields = LogFields::new(ctx, Outcome::Empty, start.elapsed());
        fields.skipped = skipped.clone();
        log_run(ctx, &fields);
        let mut out = empty_outcome(ctx, Outcome::Empty);
        out.skipped = skipped;
        return Ok(out);
    }

    // --- Step 6: spend stamp, then ONE extraction call -------------------
    if !guard.still_held() {
        return Ok(empty_outcome(ctx, Outcome::Fenced));
    }
    if start.elapsed() >= DEADLINE {
        state::record_failure_at(&ctx.learn_dir);
        log_run(
            ctx,
            &LogFields::new(ctx, Outcome::Deadline, start.elapsed()),
        );
        return Ok(empty_outcome(ctx, Outcome::Deadline));
    }

    let Some(extractor) = extractor else {
        // Content exists but no CLI is installed: nothing to spend, nothing to
        // advance — a future run (once a CLI appears) can still harvest it.
        let mut fields = LogFields::new(ctx, Outcome::NoCli, start.elapsed());
        fields.sessions = assembled.slices.len();
        fields.dropped_over_cap = assembled.dropped_over_cap;
        fields.skipped = skipped.clone();
        log_run(ctx, &fields);
        let mut out = empty_outcome(ctx, Outcome::NoCli);
        out.sessions = assembled.slices.len();
        out.dropped_over_cap = assembled.dropped_over_cap;
        out.skipped = skipped;
        return Ok(out);
    };

    // Build the prompt (free) and ensure the work dir exists BEFORE writing the
    // spend stamp — so nothing but the fresh fence sits between the stamp and
    // the spawn (contract C7).
    let pending = pending_claims(&ctx.inbox_dir);
    let prompt = extract::build_prompt(&ctx.fragments, &pending, &assembled.slices);
    let _ = std::fs::create_dir_all(&ctx.work_dir);

    // C7: spend stamp, then a fresh still_held() check, then the spawn — with
    // NO other side effect in between. A reclaim landing in this window is
    // caught by the fresh check and aborts WITHOUT spawning (the tick is
    // already burned by design, so the interval — not a retry — governs).
    //
    // The stamp write must SUCCEED before anything is spent: a paid call with
    // no stamp on disk (ENOSPC, EIO, permissions) would read as "never spent"
    // to the interval throttle, which would then re-spend on every tick — the
    // exact crash-loop cost the stamp exists to cap. A failed write therefore
    // aborts the run as a failure, making zero calls.
    if let Err(e) = state::write_stamp(&ctx.spend_stamp) {
        crate::warn_user!(
            "learning: could not write the spend stamp ({e}); aborting before the extraction call"
        );
        if guard.still_held() {
            state::record_failure_at(&ctx.learn_dir);
            let mut fields = LogFields::new(ctx, Outcome::Failed, start.elapsed());
            fields.cli = Some(extractor.cli_id().to_string());
            fields.model = Some(extractor.model().to_string());
            fields.sessions = assembled.slices.len();
            fields.dropped_over_cap = assembled.dropped_over_cap;
            fields.error = Some(format!("spend stamp write failed: {e}"));
            fields.skipped = skipped.clone();
            log_run(ctx, &fields);
        }
        return Ok(failed_outcome(ctx, extractor, &assembled, skipped));
    }
    if !guard.still_held() {
        return Ok(empty_outcome(ctx, Outcome::Fenced));
    }
    let cli_deadline = DEADLINE
        .saturating_sub(start.elapsed())
        .max(Duration::from_secs(1));
    let invoke = extractor.invoke(&prompt, &ctx.work_dir, cli_deadline);

    // --- Step 7: parse, gate, dedupe, fold, evidence ---------------------
    let out = match invoke {
        Ok(o) => o,
        Err(e) => {
            crate::warn_user!("learning extraction call failed: {e:#}");
            state::record_failure_at(&ctx.learn_dir);
            if guard.still_held() {
                let mut fields = LogFields::new(ctx, Outcome::Failed, start.elapsed());
                fields.cli = Some(extractor.cli_id().to_string());
                fields.model = Some(extractor.model().to_string());
                fields.sessions = assembled.slices.len();
                fields.dropped_over_cap = assembled.dropped_over_cap;
                fields.skipped = skipped.clone();
                log_run(ctx, &fields);
            }
            return Ok(failed_outcome(ctx, extractor, &assembled, skipped));
        }
    };

    let parsed = match extract::parse_output(&out.text) {
        Ok(p) => p,
        Err(_) => {
            // Malformed output: failed run. The spend stamp already burned the
            // tick — do NOT advance watermarks, do NOT write events.
            state::record_failure_at(&ctx.learn_dir);
            if guard.still_held() {
                let mut fields = LogFields::new(ctx, Outcome::Failed, start.elapsed());
                fields.cli = Some(extractor.cli_id().to_string());
                fields.model = Some(extractor.model().to_string());
                fields.sessions = assembled.slices.len();
                fields.dropped_over_cap = assembled.dropped_over_cap;
                fields.usage = parse_usage(out.usage.as_deref());
                fields.skipped = skipped.clone();
                log_run(ctx, &fields);
            }
            return Ok(failed_outcome(ctx, extractor, &assembled, skipped));
        }
    };

    // Resolve every candidate's evidence session_ref back to the slice it came
    // from (`<agent>:<session_id>`) so SessionRef.ts is the SESSION's own
    // stable ts (contract C1), never observation/run time.
    let by_ref: HashMap<String, &SessionSlice> = assembled
        .slices
        .iter()
        .map(|s| (format!("{}:{}", s.agent, s.session_id), s))
        .collect();

    // Exact-duplicate-of-existing-fragment guard: normalized fragment
    // descriptions to drop candidates against.
    let existing_descriptions: BTreeSet<String> = ctx
        .fragments
        .iter()
        .map(|(_, d)| journal::normalize(d))
        .filter(|n| !n.is_empty())
        .collect();

    let produced_by = ProducedBy {
        cli: extractor.cli_id().to_string(),
        model: extractor.model().to_string(),
    };
    let now_ts = ctx.now_ts();

    let mut events: Vec<Event> = Vec::new();
    let mut evidence: Vec<(String, Vec<EvidenceQuote>)> = Vec::new();
    let mut journaled: BTreeSet<String> = BTreeSet::new();
    let mut quarantined_count = 0usize;

    for cand in &parsed.candidates {
        // C11: outer sanity cap before the gate (which keeps quarantined
        // claims whole).
        let capped = truncate_chars(&cand.claim, CLAIM_SANITY_CAP);
        let (claim_text, quarantined) = match gate::gate_claim(&capped) {
            Gated::Clean(text) => (text, None),
            Gated::Quarantined { claim, labels } => (
                claim,
                Some(labels.iter().map(|l| l.to_string()).collect::<Vec<_>>()),
            ),
        };

        let normalized = journal::normalize(&claim_text);
        if normalized.is_empty() || existing_descriptions.contains(&normalized) {
            continue; // empty after gating, or an exact duplicate of a fragment
        }
        let id = journal::candidate_id(&claim_text);

        // Session refs (deduped by agent+session) from resolvable evidence,
        // plus up to MAX_QUOTES gated quotes. A candidate whose evidence cites
        // no known session is unattributable — drop it rather than journal an
        // observation with no session backing.
        let mut refs: Vec<SessionRef> = Vec::new();
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut quotes: Vec<EvidenceQuote> = Vec::new();
        for ev in &cand.evidence {
            if let Some(slice) = by_ref.get(&ev.session_ref) {
                let key = (slice.agent.to_string(), slice.session_id.clone());
                if seen.insert(key) {
                    refs.push(SessionRef {
                        agent: slice.agent.to_string(),
                        session_id: slice.session_id.clone(),
                        ts: slice.ts.clone(), // C1: the session's own stable ts
                    });
                }
            }
            if quotes.len() < MAX_QUOTES {
                let q = gate::gate_quote(&ev.quote);
                if !q.trim().is_empty() {
                    quotes.push(EvidenceQuote {
                        session_ref: ev.session_ref.clone(),
                        quote: q,
                    });
                }
            }
        }
        if refs.is_empty() {
            continue;
        }

        if quarantined.is_some() {
            quarantined_count += 1;
        }
        journaled.insert(id.clone());
        events.push(Event::Observed(Observed {
            id: id.clone(),
            kind: truncate_chars(&cand.kind, 100),
            source: "session".to_string(),
            claim: claim_text,
            session_refs: refs,
            produced_by: produced_by.clone(),
            quarantined,
            ts: now_ts.clone(), // C2: minted, whole-second Z
        }));
        if !quotes.is_empty() {
            evidence.push((id, quotes));
        }
    }

    // Write events + evidence (fenced together).
    if !guard.still_held() {
        return Ok(empty_outcome(ctx, Outcome::Fenced));
    }
    if !events.is_empty() {
        if let Err(e) = journal::append_events_at(&ctx.inbox_dir, &ctx.machine_id, &events) {
            // The call already spent; a lost journal append must not also
            // advance the watermark (that would drop the content). Treat as a
            // failed run: log, record failure, do not advance.
            crate::warn_user!("learning journal append failed: {e:#}");
            state::record_failure_at(&ctx.learn_dir);
            if guard.still_held() {
                let mut fields = LogFields::new(ctx, Outcome::Failed, start.elapsed());
                fields.cli = Some(extractor.cli_id().to_string());
                fields.model = Some(extractor.model().to_string());
                fields.sessions = assembled.slices.len();
                fields.dropped_over_cap = assembled.dropped_over_cap;
                fields.usage = parse_usage(out.usage.as_deref());
                fields.skipped = skipped.clone();
                log_run(ctx, &fields);
            }
            return Ok(failed_outcome(ctx, extractor, &assembled, skipped));
        }
    }
    for (id, quotes) in &evidence {
        write_evidence(&ctx.evidence_dir, id, quotes);
    }

    // --- Step 8: advance watermarks, prune, log, reset failures ----------
    if !guard.still_held() {
        return Ok(empty_outcome(ctx, Outcome::Fenced));
    }
    // Advance past EVERYTHING scanned (drop-don't-defer), including over-cap
    // content that was dropped rather than extracted.
    for slice in &scanned {
        if slice.agent == "gemini" {
            wm.gemini_record(&slice.session_id);
        } else {
            let key = slice.source_file.to_string_lossy().to_string();
            let mtime = observed_mtime(&slice.source_file); // C6: independent observation
            if slice.rewound {
                // C8: the reader re-read from 0 (file shrank); move the mark
                // DOWN to the new end so we don't rewind-and-re-harvest forever.
                wm.reset_file(&key, slice.end_offset, mtime);
            } else {
                wm.advance(&key, slice.end_offset, mtime);
            }
        }
    }
    // Prune marks for files that no longer exist (critic MINOR-4).
    let existing_files: BTreeSet<String> = wm
        .known_files()
        .into_iter()
        .filter(|k| Path::new(k).exists())
        .collect();
    let _ = wm.save(&existing_files);

    // Consume the eligibility hints read at step 3: this run advanced watermarks
    // past everything scanned (drop-don't-defer, hook-named sessions included),
    // so the hints are spent. Only reached on the success path and inside the
    // fence (still_held checked at step 8's entry) — a fenced/failed/empty/no-CLI
    // run leaves the hints so the just-ended session can still be retried.
    for path in &hint_paths {
        let _ = std::fs::remove_file(path);
    }

    let candidates = journaled.len();
    let mut fields = LogFields::new(ctx, Outcome::Extracted, start.elapsed());
    fields.cli = Some(extractor.cli_id().to_string());
    fields.model = Some(extractor.model().to_string());
    fields.sessions = assembled.slices.len();
    fields.candidates = candidates;
    fields.quarantined = quarantined_count;
    fields.dropped_over_cap = assembled.dropped_over_cap;
    fields.usage = parse_usage(out.usage.as_deref());
    fields.skipped = skipped.clone();
    log_run(ctx, &fields);
    state::reset_failures_at(&ctx.learn_dir);

    Ok(RunOutcome {
        outcome: Outcome::Extracted,
        trigger: ctx.trigger,
        cli: Some(extractor.cli_id().to_string()),
        model: Some(extractor.model().to_string()),
        sessions: assembled.slices.len(),
        candidates,
        quarantined: quarantined_count,
        dropped_over_cap: assembled.dropped_over_cap,
        duration_ms: start.elapsed().as_millis(),
        skipped,
    })
}

/// The Pending candidates (Clean, not suppressed/promoted/quarantined) folded
/// from the inbox, as prompt-anchoring input (T10 note). Only these reuse their
/// exact claim text so a re-observation keeps the same candidate id.
fn pending_claims(inbox_dir: &Path) -> Vec<extract::PendingClaim> {
    journal::fold_at(inbox_dir)
        .candidates
        .into_values()
        .filter(|c| c.status == journal::CandidateStatus::Pending)
        .map(|c| extract::PendingClaim {
            id: c.id,
            claim: c.claim,
            observation_count: c.observation_count,
        })
        .collect()
}

/// One evidence quote as stored in the machine-local evidence file.
#[derive(Debug, Clone, Serialize)]
struct EvidenceQuote {
    session_ref: String,
    quote: String,
}

/// The machine-local evidence file for one candidate:
/// `state_dir/learn/evidence/<id>.json`. Quotes are already redacted and
/// length-capped by [`gate::gate_quote`]; this store never syncs (the journal
/// carries claim text and counts, quotes stay local — design Decision #5).
#[derive(Debug, Serialize)]
struct EvidenceFile<'a> {
    id: &'a str,
    quotes: &'a [EvidenceQuote],
}

/// Best-effort: write one candidate's evidence file. A failure here is
/// non-fatal (the journal event is the source of truth; quotes are display).
fn write_evidence(evidence_dir: &Path, id: &str, quotes: &[EvidenceQuote]) {
    if std::fs::create_dir_all(evidence_dir).is_err() {
        return;
    }
    let file = EvidenceFile { id, quotes };
    if let Ok(body) = serde_json::to_string_pretty(&file) {
        let _ = crate::writer::atomic_write(&evidence_dir.join(format!("{id}.json")), &body);
    }
}

/// A file's current mtime in unix seconds, best-effort (`0` on any error) —
/// contract C6's independent observation, recorded alongside the offset.
fn observed_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate `s` to at most `cap` chars (never bytes — char-boundary safe).
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        s.chars().take(cap).collect()
    }
}

/// Parse the CLI's raw usage blob into JSON for the log; a non-JSON blob is
/// embedded verbatim as a string rather than dropped.
fn parse_usage(usage: Option<&str>) -> Option<serde_json::Value> {
    usage.map(|s| {
        serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.to_string()))
    })
}

/// A [`RunOutcome`] carrying only the run-shape fields (no per-session counts).
fn empty_outcome(ctx: &Ctx, outcome: Outcome) -> RunOutcome {
    RunOutcome {
        outcome,
        trigger: ctx.trigger,
        cli: None,
        model: None,
        sessions: 0,
        candidates: 0,
        quarantined: 0,
        dropped_over_cap: 0,
        duration_ms: 0,
        skipped: Vec::new(),
    }
}

/// A failed-run [`RunOutcome`] carrying the CLI/session counts (the tick was
/// burned; nothing was journaled or advanced).
fn failed_outcome(
    ctx: &Ctx,
    extractor: &dyn Extractor,
    assembled: &slices::Assembled,
    skipped: Vec<String>,
) -> RunOutcome {
    RunOutcome {
        outcome: Outcome::Failed,
        trigger: ctx.trigger,
        cli: Some(extractor.cli_id().to_string()),
        model: Some(extractor.model().to_string()),
        sessions: assembled.slices.len(),
        candidates: 0,
        quarantined: 0,
        dropped_over_cap: assembled.dropped_over_cap,
        duration_ms: 0,
        skipped,
    }
}

/// The mutable field-set backing one run-log entry, filled per outcome.
struct LogFields {
    outcome: Outcome,
    trigger: &'static str,
    cli: Option<String>,
    model: Option<String>,
    sessions: usize,
    candidates: usize,
    quarantined: usize,
    dropped_over_cap: usize,
    duration_ms: u128,
    usage: Option<serde_json::Value>,
    skipped: Vec<String>,
    error: Option<String>,
    ts: String,
}

impl LogFields {
    fn new(ctx: &Ctx, outcome: Outcome, elapsed: Duration) -> Self {
        LogFields {
            outcome,
            trigger: ctx.trigger,
            cli: None,
            model: None,
            sessions: 0,
            candidates: 0,
            quarantined: 0,
            dropped_over_cap: 0,
            duration_ms: elapsed.as_millis(),
            usage: None,
            skipped: Vec::new(),
            error: None,
            ts: ctx.now_ts(),
        }
    }
}

/// One line of `state_dir/learn/log.jsonl`. Two fields are additive over the
/// card's shape: `skipped` (contract C9 — per-store skip reasons the studio
/// history panel can surface) and `error` (a short reason string on failed
/// runs, e.g. a spend-stamp write failure; omitted from the wire when absent
/// so ordinary entries keep the card's exact shape).
#[derive(Serialize)]
struct LogEntry<'a> {
    ts: &'a str,
    trigger: &'a str,
    cli: Option<&'a str>,
    model: Option<&'a str>,
    sessions: usize,
    dropped_over_cap: usize,
    candidates: usize,
    quarantined: usize,
    duration_ms: u128,
    outcome: &'a str,
    usage: Option<&'a serde_json::Value>,
    skipped: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// Append one run-log line (best-effort append, like `src/audit.rs`). The
/// caller has already confirmed the fence still holds.
fn log_run(ctx: &Ctx, f: &LogFields) {
    let entry = LogEntry {
        ts: &f.ts,
        trigger: f.trigger,
        cli: f.cli.as_deref(),
        model: f.model.as_deref(),
        sessions: f.sessions,
        dropped_over_cap: f.dropped_over_cap,
        candidates: f.candidates,
        quarantined: f.quarantined,
        duration_ms: f.duration_ms,
        outcome: f.outcome.label(),
        usage: f.usage.as_ref(),
        skipped: &f.skipped,
        error: f.error.as_deref(),
    };
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    if let Some(parent) = ctx.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ctx.log_path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // --- test double: a canned extractor that counts its invocations ------

    struct StubExtractor {
        responses: Vec<String>,
        calls: Cell<usize>,
    }

    impl StubExtractor {
        fn new<S: AsRef<str>>(responses: &[S]) -> Self {
            StubExtractor {
                responses: responses.iter().map(|s| s.as_ref().to_string()).collect(),
                calls: Cell::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.get()
        }
    }

    impl Extractor for StubExtractor {
        fn cli_id(&self) -> &str {
            "claude"
        }
        fn model(&self) -> &str {
            "haiku"
        }
        fn invoke(
            &self,
            _prompt: &str,
            _work: &Path,
            _d: Duration,
        ) -> Result<agent_cli::InvokeOut> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            let text = self
                .responses
                .get(n)
                .cloned()
                .unwrap_or_else(|| self.responses.last().cloned().unwrap_or_default());
            Ok(agent_cli::InvokeOut {
                text,
                usage: Some(r#"{"input_tokens":10,"output_tokens":5}"#.to_string()),
            })
        }
    }

    /// A fixed far-future run clock so the readers' quiescence gate always
    /// treats fixture files (real mtime ~now) as long finished, deterministically.
    fn fixed_now() -> DateTime<Utc> {
        "2126-01-01T00:00:00Z".parse().unwrap()
    }

    /// A canned valid extraction citing one claude session.
    fn valid_extraction(session_ref: &str, claim: &str) -> String {
        format!(
            r#"{{"candidates":[{{"claim":{claim:?},"kind":"preference","evidence":[{{"session_ref":{session_ref:?},"quote":"the user said so"}}]}}]}}"#
        )
    }

    /// Build a Ctx over fresh tempdirs. Returns (Ctx, TempDirs kept alive).
    struct Env {
        _state: tempfile::TempDir,
        _cfg: tempfile::TempDir,
        _home: tempfile::TempDir,
        ctx: Ctx,
    }

    fn env(scope: LearnScope, fragments: Vec<(String, String)>) -> Env {
        let state = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let learn_dir = state.path().join("learn");
        std::fs::create_dir_all(&learn_dir).unwrap();
        let ctx = Ctx {
            work_dir: learn_dir.join("work"),
            evidence_dir: learn_dir.join("evidence"),
            watermarks_path: learn_dir.join("watermarks.json"),
            scan_stamp: learn_dir.join("scan-stamp"),
            spend_stamp: learn_dir.join("spend-stamp"),
            log_path: learn_dir.join("log.jsonl"),
            scope,
            fragments,
            machine_id: "test-machine".to_string(),
            now_utc: fixed_now(),
            trigger: "manual",
            hooked: BTreeSet::new(),
            learn_dir,
            inbox_dir: cfg.path().join("inbox"),
            home: home.path().to_path_buf(),
        };
        Env {
            _state: state,
            _cfg: cfg,
            _home: home,
            ctx,
        }
    }

    /// Write a one-line claude transcript for `session_id` under the env's home
    /// and return its on-disk path. The message text is `msg`; timestamp is a
    /// fixed value just before `fixed_now()` so the age cutoff keeps it.
    fn write_claude_session(ctx: &Ctx, session_id: &str, msg: &str) -> PathBuf {
        let proj = ctx.home.join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{session_id}.jsonl"));
        let line = format!(
            r#"{{"type":"user","userType":"external","entrypoint":"cli","cwd":"/work/repo","timestamp":"2126-01-01T00:00:00.000Z","message":{{"content":{msg:?}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();
        path
    }

    fn acquire(ctx: &Ctx) -> lock::LockGuard {
        match lock::acquire_at(&ctx.learn_dir, DEADLINE, ctx.now_unix()).unwrap() {
            lock::Acquire::Held(g) => g,
            other => panic!("expected Held, got {other:?}"),
        }
    }

    fn plant_foreign_lock(ctx: &Ctx) {
        let body = r#"{"pid":999999,"started_at":1,"token":"ffffffffffffffffffffffffffffffff"}"#;
        std::fs::write(ctx.learn_dir.join("lock.json"), body).unwrap();
    }

    fn journal_events(ctx: &Ctx) -> Vec<Event> {
        let path = ctx
            .inbox_dir
            .join(format!("journal-{}.jsonl", ctx.machine_id));
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Event>(l).unwrap())
            .collect()
    }

    fn log_lines(ctx: &Ctx) -> Vec<serde_json::Value> {
        let Ok(content) = std::fs::read_to_string(&ctx.log_path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .collect()
    }

    // --- ACCEPTANCE 1: empty scan -----------------------------------------

    #[test]
    fn empty_scan_makes_zero_calls_and_leaves_spend_stamp_untouched() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let stub = StubExtractor::new(&["unused"]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Empty);
        assert_eq!(stub.calls(), 0, "empty scan must make ZERO CLI calls");
        assert!(
            !ctx.spend_stamp.exists(),
            "empty scan must leave the spend stamp untouched"
        );
        assert!(ctx.scan_stamp.exists(), "the scan stamp is still written");
        assert!(journal_events(ctx).is_empty(), "no events on an empty scan");
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1, "one no-op log entry");
        assert_eq!(logs[0]["outcome"], "empty");
    }

    // --- ACCEPTANCE 2 + full cycle: valid extraction ----------------------

    #[test]
    fn full_cycle_writes_journal_evidence_log_and_marks() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(ctx, "sess-1", "Always use pnpm, never npm.");
        let stub = StubExtractor::new(&[valid_extraction(
            "claude:sess-1",
            "Always use pnpm, never npm.",
        )]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Extracted);
        assert_eq!(stub.calls(), 1);
        assert_eq!(out.sessions, 1);
        assert_eq!(out.candidates, 1);

        // Journal event with the SESSION's own ts on the ref (C1) and a minted
        // whole-second Z ts on the observation (C2).
        let events = journal_events(ctx);
        assert_eq!(events.len(), 1);
        let Event::Observed(o) = &events[0] else {
            panic!("expected an Observed event");
        };
        assert_eq!(o.claim, "Always use pnpm, never npm.");
        assert_eq!(o.produced_by.cli, "claude");
        assert_eq!(o.session_refs.len(), 1);
        assert_eq!(o.session_refs[0].ts, "2126-01-01T00:00:00.000Z");
        assert_eq!(o.ts, "2126-01-01T00:00:00Z");

        // Evidence file present under the candidate id.
        let id = journal::candidate_id("Always use pnpm, never npm.");
        assert!(
            ctx.evidence_dir.join(format!("{id}.json")).exists(),
            "evidence file for the candidate must exist"
        );

        // Watermark advanced to the file length; spend + scan stamps written.
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        let key = src.to_string_lossy().to_string();
        let file_len = std::fs::metadata(&src).unwrap().len();
        assert_eq!(wm.mark(&key).unwrap().bytes_processed, file_len);
        assert!(ctx.spend_stamp.exists());
        assert!(ctx.scan_stamp.exists());

        // Exactly one attributable log entry.
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["outcome"], "extracted");
        assert_eq!(logs[0]["sessions"], 1);
        assert_eq!(logs[0]["candidates"], 1);
        assert_eq!(logs[0]["trigger"], "manual");
        assert!(logs[0]["usage"].is_object(), "usage blob recorded");
    }

    // --- ACCEPTANCE 2: malformed output burns the tick and nothing else ---

    #[test]
    fn malformed_output_writes_spend_stamp_but_no_marks_or_events() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let stub = StubExtractor::new(&["this is not valid json {"]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Failed);
        assert_eq!(
            stub.calls(),
            1,
            "the call was made (spend stamp precedes it)"
        );
        assert!(
            ctx.spend_stamp.exists(),
            "the spend stamp burned the tick even though output was malformed"
        );
        assert!(
            journal_events(ctx).is_empty(),
            "no events on malformed output"
        );
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        assert!(
            wm.mark(&src.to_string_lossy()).is_none(),
            "watermarks must NOT advance on a malformed run"
        );
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["outcome"], "failed");
        // One failure recorded → not yet paused.
        assert!(!state::paused_at(&ctx.learn_dir));
    }

    // --- money path: a spend-stamp write failure must not spend ------------

    #[test]
    fn spend_stamp_write_failure_aborts_before_the_call() {
        // If the paid call fired with no stamp on disk, the interval throttle
        // would read "never spent" and re-spend every tick. A failed stamp
        // write must therefore make ZERO calls and log a failed run.
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm.");
        // Make write_stamp fail deterministically: a DIRECTORY at the stamp
        // path (atomic_write's rename onto an existing directory errors),
        // while the learn dir itself stays writable so the log/failure-counter
        // assertions below still exercise their real paths.
        std::fs::create_dir_all(&ctx.spend_stamp).unwrap();
        let stub = StubExtractor::new(&[valid_extraction("claude:sess-1", "Always use pnpm.")]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Failed);
        assert_eq!(
            stub.calls(),
            0,
            "an unwritten spend stamp must abort BEFORE the extraction call"
        );
        assert!(journal_events(ctx).is_empty(), "no events");
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        assert!(
            wm.mark(
                &ctx.home
                    .join(".claude/projects/proj/sess-1.jsonl")
                    .to_string_lossy()
            )
            .is_none(),
            "no watermark advance"
        );
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1, "one attributable failure entry");
        assert_eq!(logs[0]["outcome"], "failed");
        assert!(
            logs[0]["error"]
                .as_str()
                .is_some_and(|s| s.contains("spend stamp")),
            "the log entry's error must name the stamp write: {}",
            logs[0]
        );
        // The failure counter advanced (one failure → not yet paused).
        assert!(!state::paused_at(&ctx.learn_dir));
        state::record_failure_at(&ctx.learn_dir);
        assert!(
            state::paused_at(&ctx.learn_dir),
            "the stamp-write failure must have been the FIRST recorded failure"
        );
    }

    // --- ACCEPTANCE 3 + C7: fenced-out worker makes no spend and no writes -

    #[test]
    fn fenced_out_worker_makes_no_spend_and_no_writes() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let stub = StubExtractor::new(&[valid_extraction("claude:sess-1", "Always use pnpm.")]);

        // Acquire, then an external process overwrites the lock with ITS token.
        let guard = acquire(ctx);
        plant_foreign_lock(ctx);
        assert!(!guard.still_held(), "the token is now foreign");

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();

        assert_eq!(out.outcome, Outcome::Fenced);
        assert_eq!(stub.calls(), 0, "a fenced worker must not spawn the CLI");
        assert!(!ctx.spend_stamp.exists(), "no spend");
        assert!(!ctx.scan_stamp.exists(), "no scan-stamp write either");
        assert!(journal_events(ctx).is_empty(), "no events");
        assert!(
            log_lines(ctx).is_empty(),
            "a fenced-out worker writes no log"
        );
        assert!(!ctx.watermarks_path.exists(), "no watermark write");
    }

    // --- C7: a reclaimed stale lock counts as one failure -----------------

    #[test]
    fn reclaimed_stale_lock_is_recorded_as_one_failure_each() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        // No transcripts → empty scans (which never reset the failure counter),
        // so two reclaims accumulate to the pause threshold.
        let plant_stale = |ctx: &Ctx| {
            // A lock far older than 2*DEADLINE → stale by age → reclaimed.
            let started = ctx.now_unix() - 10 * DEADLINE.as_secs() as i64;
            let body = format!(
                r#"{{"pid":{},"started_at":{started},"token":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
                std::process::id()
            );
            std::fs::write(ctx.learn_dir.join("lock.json"), body).unwrap();
        };

        plant_stale(ctx);
        let out1 = run_harvest_ctx(ctx, None).unwrap();
        assert_eq!(out1.outcome, Outcome::Empty);
        assert!(
            !state::paused_at(&ctx.learn_dir),
            "one reclaim → not yet paused"
        );

        plant_stale(ctx);
        let out2 = run_harvest_ctx(ctx, None).unwrap();
        assert_eq!(out2.outcome, Outcome::Empty);
        assert!(
            state::paused_at(&ctx.learn_dir),
            "a second reclaim → two failures → paused"
        );
    }

    // --- C8: a shrunk file advances the mark DOWN and is not re-harvested --

    #[test]
    fn shrunk_file_moves_mark_down_and_does_not_re_harvest() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(
            ctx,
            "sess-1",
            "Always use pnpm and prefer ripgrep over grep in every project.",
        );
        let long_len = std::fs::metadata(&src).unwrap().len();

        // Run 1: full length harvested; mark advances to long_len.
        let stub = StubExtractor::new(&[valid_extraction("claude:sess-1", "Always use pnpm.")]);
        {
            let guard = acquire(ctx);
            let out = run_body(ctx, &guard, Some(&stub)).unwrap();
            guard.release();
            assert_eq!(out.outcome, Outcome::Extracted);
        }
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        assert_eq!(
            wm.mark(&src.to_string_lossy()).unwrap().bytes_processed,
            long_len
        );

        // The file shrinks (rewritten shorter). Same session id, less content.
        write_claude_session(ctx, "sess-1", "Use pnpm.");
        let short_len = std::fs::metadata(&src).unwrap().len();
        assert!(
            short_len < long_len,
            "the file must actually be shorter now"
        );

        // Run 2: reader rewinds to 0 (mark > len), worker resets the mark DOWN
        // to the new end via reset_file. The call is made (call #2).
        {
            let guard = acquire(ctx);
            let out = run_body(ctx, &guard, Some(&stub)).unwrap();
            guard.release();
            assert_eq!(out.outcome, Outcome::Extracted);
        }
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        assert_eq!(
            wm.mark(&src.to_string_lossy()).unwrap().bytes_processed,
            short_len,
            "the mark must move DOWN to the shrunk file's new end (C8)"
        );
        assert_eq!(stub.calls(), 2);

        // Run 3: mark == len now → nothing new → no slice → NO extraction. Had
        // we kept the old (too-large) mark, run 3 would rewind and re-harvest.
        {
            let guard = acquire(ctx);
            let out = run_body(ctx, &guard, Some(&stub)).unwrap();
            guard.release();
            assert_eq!(out.outcome, Outcome::Empty);
        }
        assert_eq!(
            stub.calls(),
            2,
            "the shrunk file must NOT be re-harvested on the next run"
        );
    }

    // --- no-CLI path: content but nothing to run --------------------------

    #[test]
    fn no_cli_installed_logs_but_does_not_spend_or_advance() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, None).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::NoCli);
        assert!(!ctx.spend_stamp.exists(), "no CLI → no spend");
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        assert!(
            wm.mark(&src.to_string_lossy()).is_none(),
            "no CLI → content preserved (not advanced)"
        );
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["outcome"], "no_cli");
    }

    // --- dedupe against existing fragment descriptions --------------------

    #[test]
    fn exact_duplicate_of_a_fragment_description_is_dropped() {
        let e = env(
            LearnScope::All,
            vec![("f1".to_string(), "Always use pnpm.".to_string())],
        );
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "pnpm please");
        // The model proposes a claim that exactly matches an existing fragment.
        let stub = StubExtractor::new(&[valid_extraction("claude:sess-1", "always use  pnpm.")]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Extracted);
        assert_eq!(out.candidates, 0, "an exact-duplicate candidate is dropped");
        assert!(journal_events(ctx).is_empty());
        // Still a successful run (the call answered validly): watermark advanced.
        let wm = Watermarks::load_from(&ctx.watermarks_path);
        let key = ctx.home.join(".claude/projects/proj/sess-1.jsonl");
        assert!(wm.mark(&key.to_string_lossy()).is_some());
    }

    // --- eligibility hint consumption (T14 wiring) ------------------------

    /// Plant an eligibility hint file for `session_id` under the env's learn dir
    /// and return its path.
    fn plant_hint(ctx: &Ctx, session_id: &str) -> PathBuf {
        let dir = ctx.learn_dir.join("eligible");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("claude-{session_id}"));
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn eligibility_hint_is_deleted_after_a_successful_extraction() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm, never npm.");
        let hint = plant_hint(ctx, "sess-1");

        let stub = StubExtractor::new(&[valid_extraction(
            "claude:sess-1",
            "Always use pnpm, never npm.",
        )]);
        let guard = acquire(ctx);
        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Extracted);
        assert!(
            !hint.exists(),
            "a successful run advances past everything scanned and must consume the hint"
        );
    }

    #[test]
    fn eligibility_hint_survives_a_non_advancing_run() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        // No transcripts → empty scan → watermarks NOT advanced.
        let hint = plant_hint(ctx, "ghost");

        let stub = StubExtractor::new(&["unused"]);
        let guard = acquire(ctx);
        let out = run_body(ctx, &guard, Some(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Empty);
        assert_eq!(stub.calls(), 0, "an empty scan spends nothing");
        assert!(
            hint.exists(),
            "a run that advanced no watermarks must leave the hint for a later retry"
        );
    }
}
