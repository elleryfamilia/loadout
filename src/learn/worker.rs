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
//! live primarily in the trigger fast path; an **ambient** run additionally
//! re-checks the spend interval right after the scan stamp (defense-in-depth
//! via [`trigger::eligibility_at`] — the consent ceiling holds even for a
//! direct `load harvest --ambient` invocation, which the fast path never
//! vetted) and exits as a logged, non-failure [`Outcome::Throttled`] no-op
//! when the interval hasn't elapsed. A session-end hint does NOT lift this
//! throttle: it never buys an extra extraction call (design Decision #3). Its
//! only role is at harvest time, where step 3 merges hook-named sessions into
//! the readers' `hooked` set so a *due* tick harvests a just-ended session
//! despite the quiescence window. A manual `load harvest` bypasses the interval
//! check entirely but still writes the spend stamp here (a manual run resets
//! the ambient tick — the cheapest honest semantics).

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

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
    /// Privacy-safe reason for an actionable terminal outcome, when one is
    /// available. The run log is derived from this same closed value.
    pub diagnostic: Option<HarvestDiagnostic>,
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

/// Closed, privacy-safe explanation for an actionable harvest outcome.
///
/// No variant accepts provider output, transcript content, paths, or an
/// arbitrary error string. Messages and wire codes are derived exclusively
/// from this enum and the already-sanitized provider/parser failure types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarvestDiagnostic {
    RunDeadlineExceeded,
    UnsupportedCli(agent_cli::UnsupportedCli),
    WatermarkStoreCorrupt,
    StaleLockReclaimed,
    SpendStampWriteFailed {
        io_kind: std::io::ErrorKind,
        os_error: Option<i32>,
    },
    Invoke(agent_cli::InvokeFailure),
    Parse(extract::ParseFailure),
    JournalAppendFailed {
        io_kind: std::io::ErrorKind,
        os_error: Option<i32>,
    },
}

impl HarvestDiagnostic {
    pub fn stage(&self) -> &'static str {
        match self {
            Self::RunDeadlineExceeded
            | Self::UnsupportedCli(_)
            | Self::WatermarkStoreCorrupt
            | Self::StaleLockReclaimed => "preflight",
            Self::SpendStampWriteFailed { .. } => "spend_guard",
            Self::Invoke(failure) => failure.stage(),
            Self::Parse(_) => "validate_output",
            Self::JournalAppendFailed { .. } => "persist_journal",
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::RunDeadlineExceeded => "run_deadline_exceeded",
            Self::UnsupportedCli(_) => "claude_structured_output_unsupported",
            Self::WatermarkStoreCorrupt => "watermark_store_corrupt",
            Self::StaleLockReclaimed => "stale_lock_reclaimed",
            Self::SpendStampWriteFailed { .. } => "spend_stamp_write_failed",
            Self::Invoke(failure) => failure.code(),
            Self::Parse(failure) => failure.code(),
            Self::JournalAppendFailed { .. } => "journal_append_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::RunDeadlineExceeded => {
                "The harvest run exceeded its deadline before extraction started.".to_string()
            }
            Self::UnsupportedCli(unsupported) => match (
                unsupported.reason,
                unsupported.installed_version.as_deref(),
            ) {
                (agent_cli::UnsupportedReason::TooOld, Some(installed)) => format!(
                    "Claude Code {installed} cannot provide the required structured output. Upgrade to {} or newer.",
                    unsupported.minimum_version
                ),
                (agent_cli::UnsupportedReason::ProbeTimedOut, _) => format!(
                    "Loadout could not verify Claude Code's version. Upgrade to {} or newer, then run `load harvest` again.",
                    unsupported.minimum_version
                ),
                _ => format!(
                    "Loadout could not verify that Claude Code supports structured output. Upgrade to {} or newer.",
                    unsupported.minimum_version
                ),
            },
            Self::WatermarkStoreCorrupt => {
                "The learning watermark store is corrupt. Run `load learn reset` to re-baseline it."
                    .to_string()
            }
            Self::StaleLockReclaimed => {
                "Loadout reclaimed a stale harvest lock left by an interrupted run.".to_string()
            }
            Self::SpendStampWriteFailed { .. } => {
                "Loadout could not write the harvest spend stamp. Check permissions and available disk space, then run `load harvest` again."
                    .to_string()
            }
            Self::Invoke(failure) => failure.message().to_string(),
            Self::Parse(failure) => failure.message().to_string(),
            Self::JournalAppendFailed { .. } => {
                "Loadout could not append extracted candidates to the learning journal. Check permissions and available disk space."
                    .to_string()
            }
        }
    }
}

/// The terminal state of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A live worker already holds the lock; this invocation did nothing.
    Busy,
    /// The fencing token went foreign mid-run; aborted before any write.
    Fenced,
    /// An **ambient** run found the spend interval unelapsed; it exited before
    /// any reader work (defense-in-depth: the trigger fast path is the primary
    /// throttle, this bounds direct `load harvest --ambient` invocations too).
    /// A waiting session-end hint does not lift this — a hint never buys an
    /// extra call. Not a failure; spend stamp and watermarks untouched.
    Throttled,
    /// No eligible new content; a no-op run (spend stamp untouched).
    Empty,
    /// Eligible content exists but no extraction CLI is installed; nothing
    /// spent, nothing advanced (retry once a CLI appears).
    NoCli,
    /// Claude is installed but cannot satisfy harvest's required structured
    /// output contract. Nothing was spent and the failure breaker is unchanged.
    UnsupportedCli,
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
            Outcome::Throttled => "throttled",
            Outcome::Empty => "empty",
            Outcome::NoCli => "no_cli",
            Outcome::UnsupportedCli => "unsupported_cli",
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
    ) -> Result<agent_cli::InvokeOut, agent_cli::InvokeFailure>;
}

/// The production extractor: a resolved [`agent_cli::CliChoice`] invoked via
/// [`agent_cli::invoke`] (one bounded, hygiene-flagged agent-CLI spawn).
struct RealExtractor {
    choice: agent_cli::CliChoice,
}

/// Worker-facing projection of [`agent_cli::Selection`]. Keeping the
/// unsupported case distinct prevents production from silently treating an
/// installed-but-incompatible Claude as though no CLI were installed.
enum WorkerSelection<'a> {
    Chosen(&'a dyn Extractor),
    Unsupported(agent_cli::UnsupportedCli),
    None,
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
    ) -> Result<agent_cli::InvokeOut, agent_cli::InvokeFailure> {
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
    /// `learn.interval` — the spend-stamp interval the ambient self-throttle
    /// re-checks in the worker (defense-in-depth; the trigger fast path is the
    /// primary throttle and uses the same value).
    interval: Duration,
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
        interval: cfg.learn.interval,
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

    match agent_cli::select(&cfg.learn) {
        agent_cli::Selection::Chosen(choice) => {
            let extractor = RealExtractor::new(choice);
            run_harvest_ctx(&ctx, WorkerSelection::Chosen(&extractor))
        }
        agent_cli::Selection::Unsupported(unsupported) => {
            run_harvest_ctx(&ctx, WorkerSelection::Unsupported(unsupported))
        }
        agent_cli::Selection::None => run_harvest_ctx(&ctx, WorkerSelection::None),
    }
}

/// Step 1 (lock) + dispatch to the fenced body. A `Busy` lock exits quietly; a
/// `Reclaimed` stale lock counts as one failure, recorded immediately (C7),
/// then proceeds. The guard is released after the body regardless of outcome
/// ([`lock::LockGuard::release`] no-ops if the token went foreign).
fn run_harvest_ctx(ctx: &Ctx, selection: WorkerSelection<'_>) -> Result<RunOutcome> {
    let (guard, reclaimed) = match lock::acquire_at(&ctx.learn_dir, DEADLINE, ctx.now_unix())? {
        lock::Acquire::Busy => return Ok(empty_outcome(ctx, Outcome::Busy)),
        lock::Acquire::Held(g) => (g, false),
        lock::Acquire::Reclaimed(g) => (g, true),
    };

    // Reclamation is an observed failure of the previous holder, not this
    // continuation. Record it as its own fenced audit line before doing any
    // further work, so a later empty result cannot hide the breaker reason.
    if reclaimed {
        if !guard.still_held() {
            return Ok(empty_outcome(ctx, Outcome::Fenced));
        }
        let diagnostic = HarvestDiagnostic::StaleLockReclaimed;
        state::record_failure_at(&ctx.learn_dir);
        let mut fields = LogFields::new(ctx, Outcome::Failed, Duration::ZERO);
        fields.diagnostic = Some(diagnostic);
        log_run(ctx, &fields);
    }

    let outcome = run_body(ctx, &guard, selection);
    guard.release();
    outcome
}

/// Steps 2–8, each fenced. Returns [`Outcome::Fenced`] (writing nothing) the
/// moment the token goes foreign.
fn run_body(
    ctx: &Ctx,
    guard: &lock::LockGuard,
    selection: WorkerSelection<'_>,
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

    // --- Ambient self-throttle (defense-in-depth) -------------------------
    // The trigger fast path (guards 6+7 in `trigger::should_spawn`) is the
    // throttle's primary home — production only spawns an ambient worker after
    // those passed. But the consent ceiling ("at most 1 extraction call per
    // interval per machine") must hold even if a wiring bug or an external
    // script invokes `load harvest --ambient` directly: the lock serializes
    // CONCURRENT workers but does not bound SEQUENTIAL direct calls. So an
    // ambient run re-checks the spend interval here, via the trigger module's
    // own eligibility math (one source of truth, never duplicated). A
    // session-end eligibility hint does NOT lift this throttle — it never buys
    // an extra extraction call (design Decision #3 rejects spend that scales
    // with usage). The hint's role is purely at harvest time (step 3 merges
    // hook-named sessions into the readers' `hooked` set so a due tick harvests
    // a just-ended session despite the quiescence window). Manual runs are
    // deliberately exempt: a typed `load harvest` bypasses the interval but
    // still writes the spend stamp at step 6 (the consent wording's semantics).
    // A throttled exit is NOT a failure: one log entry (throttled), spend stamp
    // and watermarks untouched.
    if ctx.trigger == "ambient" {
        let e = trigger::eligibility_at(&ctx.learn_dir, ctx.interval, ctx.now_sys());
        if !e.spend_due {
            if !guard.still_held() {
                return Ok(empty_outcome(ctx, Outcome::Fenced));
            }
            log_run(
                ctx,
                &LogFields::new(ctx, Outcome::Throttled, start.elapsed()),
            );
            return Ok(empty_outcome(ctx, Outcome::Throttled));
        }
    }

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
        let diagnostic = HarvestDiagnostic::WatermarkStoreCorrupt;
        return Ok(terminal_failure(
            ctx,
            guard,
            start,
            Outcome::Corrupt,
            diagnostic,
            None,
            None,
            None,
            Vec::new(),
        ));
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
        return Ok(terminal_failure(
            ctx,
            guard,
            start,
            Outcome::Deadline,
            HarvestDiagnostic::RunDeadlineExceeded,
            None,
            Some(&assembled),
            None,
            skipped,
        ));
    }

    let extractor = match selection {
        WorkerSelection::Chosen(extractor) => extractor,
        WorkerSelection::Unsupported(unsupported) => {
            // This is actionable but not a breaker failure: compatibility was
            // rejected before the spend stamp and no extraction was attempted.
            if !guard.still_held() {
                return Ok(empty_outcome(ctx, Outcome::Fenced));
            }
            let cli = unsupported.cli_id.to_string();
            let diagnostic = HarvestDiagnostic::UnsupportedCli(unsupported);
            let mut fields = LogFields::new(ctx, Outcome::UnsupportedCli, start.elapsed());
            fields.cli = Some(cli.clone());
            fields.sessions = assembled.slices.len();
            fields.dropped_over_cap = assembled.dropped_over_cap;
            fields.skipped = skipped.clone();
            fields.diagnostic = Some(diagnostic.clone());
            log_run(ctx, &fields);
            return Ok(RunOutcome {
                outcome: Outcome::UnsupportedCli,
                diagnostic: Some(diagnostic),
                trigger: ctx.trigger,
                cli: Some(cli),
                model: None,
                sessions: assembled.slices.len(),
                candidates: 0,
                quarantined: 0,
                dropped_over_cap: assembled.dropped_over_cap,
                duration_ms: start.elapsed().as_millis(),
                skipped,
            });
        }
        WorkerSelection::None => {
            // Content exists but no CLI is installed: nothing to spend,
            // nothing to advance — a future run can still harvest it.
            if !guard.still_held() {
                return Ok(empty_outcome(ctx, Outcome::Fenced));
            }
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
        }
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
    if let Err(error) = state::write_stamp(&ctx.spend_stamp) {
        let diagnostic = HarvestDiagnostic::SpendStampWriteFailed {
            io_kind: error.kind(),
            os_error: error.raw_os_error(),
        };
        crate::warn_user!("learning: {}", diagnostic.message());
        return Ok(terminal_failure(
            ctx,
            guard,
            start,
            Outcome::Failed,
            diagnostic,
            Some(extractor),
            Some(&assembled),
            None,
            skipped,
        ));
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
        Err(failure) => {
            let usage = parse_usage(failure.usage.as_deref());
            let diagnostic = HarvestDiagnostic::Invoke(failure);
            crate::warn_user!("learning: {}", diagnostic.message());
            return Ok(terminal_failure(
                ctx,
                guard,
                start,
                Outcome::Failed,
                diagnostic,
                Some(extractor),
                Some(&assembled),
                usage,
                skipped,
            ));
        }
    };

    let parsed = match extract::parse_output(&out.text) {
        Ok(p) => p,
        Err(failure) => {
            // Malformed output: failed run. The spend stamp already burned the
            // tick — do NOT advance watermarks, do NOT write events.
            let diagnostic = HarvestDiagnostic::Parse(failure);
            crate::warn_user!("learning: {}", diagnostic.message());
            return Ok(terminal_failure(
                ctx,
                guard,
                start,
                Outcome::Failed,
                diagnostic,
                Some(extractor),
                Some(&assembled),
                parse_usage(out.usage.as_deref()),
                skipped,
            ));
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
            // Only evidence whose session_ref resolves to a scanned slice is
            // kept — both the deduped SessionRef AND the display quote. A quote
            // citing an unknown/unresolvable session_ref has no attributable
            // session backing (the model may cite a session not in this scan),
            // so it is dropped here, mirroring the candidate-level
            // `refs.is_empty()` drop below — we never store a quote we cannot
            // attribute to a session.
            let Some(slice) = by_ref.get(&ev.session_ref) else {
                continue;
            };
            let key = (slice.agent.to_string(), slice.session_id.clone());
            if seen.insert(key) {
                refs.push(SessionRef {
                    agent: slice.agent.to_string(),
                    session_id: slice.session_id.clone(),
                    ts: slice.ts.clone(), // C1: the session's own stable ts
                });
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
        if let Err(error) = journal::append_events_at(&ctx.inbox_dir, &ctx.machine_id, &events) {
            // The call already spent; a lost journal append must not also
            // advance the watermark (that would drop the content). Treat as a
            // failed run: log, record failure, do not advance.
            let diagnostic = HarvestDiagnostic::JournalAppendFailed {
                io_kind: error.kind(),
                os_error: error.raw_os_error(),
            };
            crate::warn_user!("learning: {}", diagnostic.message());
            return Ok(terminal_failure(
                ctx,
                guard,
                start,
                Outcome::Failed,
                diagnostic,
                Some(extractor),
                Some(&assembled),
                parse_usage(out.usage.as_deref()),
                skipped,
            ));
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
        diagnostic: None,
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
        diagnostic: None,
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

/// Record one terminal breaker failure. The fence check, counter update, log
/// entry, duration, usage, and returned outcome are centralized here so they
/// cannot disagree. If ownership was lost after spending, the spend stamp is
/// deliberately the only durable evidence and this worker writes nothing.
#[allow(clippy::too_many_arguments)]
fn terminal_failure(
    ctx: &Ctx,
    guard: &lock::LockGuard,
    start: Instant,
    outcome: Outcome,
    diagnostic: HarvestDiagnostic,
    extractor: Option<&dyn Extractor>,
    assembled: Option<&slices::Assembled>,
    usage: Option<serde_json::Value>,
    skipped: Vec<String>,
) -> RunOutcome {
    if !guard.still_held() {
        return empty_outcome(ctx, Outcome::Fenced);
    }

    state::record_failure_at(&ctx.learn_dir);
    let elapsed = start.elapsed();
    let mut fields = LogFields::new(ctx, outcome, elapsed);
    fields.cli = extractor.map(|extractor| extractor.cli_id().to_string());
    fields.model = extractor.map(|extractor| extractor.model().to_string());
    fields.sessions = assembled.map_or(0, |assembled| assembled.slices.len());
    fields.dropped_over_cap = assembled.map_or(0, |assembled| assembled.dropped_over_cap);
    fields.usage = usage;
    fields.skipped = skipped.clone();
    fields.diagnostic = Some(diagnostic.clone());
    log_run(ctx, &fields);

    RunOutcome {
        outcome,
        diagnostic: Some(diagnostic),
        trigger: ctx.trigger,
        cli: extractor.map(|extractor| extractor.cli_id().to_string()),
        model: extractor.map(|extractor| extractor.model().to_string()),
        sessions: assembled.map_or(0, |assembled| assembled.slices.len()),
        candidates: 0,
        quarantined: 0,
        dropped_over_cap: assembled.map_or(0, |assembled| assembled.dropped_over_cap),
        duration_ms: elapsed.as_millis(),
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
    diagnostic: Option<HarvestDiagnostic>,
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
            diagnostic: None,
            ts: ctx.now_ts(),
        }
    }
}

/// One line of `state_dir/learn/log.jsonl`. Diagnostics are flattened to three
/// optional strings so older readers ignore them and newer readers can consume
/// the stage, stable code, and safe human message independently.
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
    error_stage: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// Append one run-log line (best-effort append, like `src/audit.rs`). The
/// caller has already confirmed the fence still holds.
fn log_run(ctx: &Ctx, f: &LogFields) {
    let error = f.diagnostic.as_ref().map(HarvestDiagnostic::message);
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
        error_stage: f.diagnostic.as_ref().map(HarvestDiagnostic::stage),
        error_code: f.diagnostic.as_ref().map(HarvestDiagnostic::code),
        error: error.as_deref(),
    };
    let append = || -> std::io::Result<()> {
        let line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
        if let Some(parent) = ctx.log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ctx.log_path)?;
        writeln!(file, "{line}")
    };
    if append().is_err() && (f.diagnostic.is_some() || matches!(f.outcome, Outcome::Extracted)) {
        crate::warn_user!("learning: could not append the harvest run log");
    }
}

// --- reading the run log back (the refresh discovery line) -----------------
//
// [`LogEntry`] above is write-only (borrowed fields, no `Deserialize` derive —
// a deliberate zero-copy shape for appends). [`LogRecord`] is its owned,
// read-back counterpart: the refresh summary line (T15) needs to find the
// latest successful ambient run without re-implementing the wire shape. Only
// the fields that line needs are declared; extra keys on the wire (`error`,
// `usage`, `skipped`, …) are ignored rather than rejected, so this reader
// never breaks on a log written by a newer or older loadout.
#[derive(Debug, Clone, Deserialize)]
pub struct LogRecord {
    pub ts: String,
    pub trigger: String,
    #[serde(default)]
    pub cli: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub sessions: usize,
    pub candidates: usize,
    /// Wall-clock run duration in milliseconds. Optional so a log line from an
    /// older loadout (which never wrote it) still folds; `#[serde(default)]`
    /// yields `None` when absent. Surfaced in the studio history panel.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// The CLI's token-usage blob as a display string — the spend-audit signal
    /// the history panel shows. On the wire `usage` is polymorphic (a JSON
    /// object like `{"input_tokens":10}`, a bare string, `null`, or absent), so
    /// it is read leniently into a single display string rather than a fixed
    /// shape: an object is compacted to its JSON text, a string kept verbatim,
    /// and `null`/absent fold to `None`. Reading it as a plain `Option<String>`
    /// would instead REJECT the common object form and drop the whole log line.
    #[serde(default, deserialize_with = "de_usage_string")]
    pub usage: Option<String>,
    /// Optional diagnostic fields. Each is parsed independently and leniently:
    /// a foreign non-string value becomes `None` without dropping the line.
    #[serde(default, deserialize_with = "de_optional_string")]
    pub error_stage: Option<String>,
    #[serde(default, deserialize_with = "de_optional_string")]
    pub error_code: Option<String>,
    #[serde(default, deserialize_with = "de_optional_string")]
    pub error: Option<String>,
    pub outcome: String,
}

/// Lenient deserializer for [`LogRecord::usage`]: accept any JSON value and
/// render it to a display string. An object (the usual `{"input_tokens":…}`
/// blob) becomes its compact JSON text, a string stays verbatim, and `null`
/// becomes `None`. This is what lets `usage` be an `Option<String>` without a
/// present-but-object value failing the whole line's parse.
fn de_usage_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }))
}

fn de_optional_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value.and_then(|value| value.as_str().map(str::to_string)))
}

impl LogRecord {
    /// This entry's `ts` as unix seconds, or `None` if it doesn't parse as
    /// RFC 3339 (a corrupt/foreign line — treated as unusable, not fatal).
    pub fn ts_unix(&self) -> Option<i64> {
        DateTime::parse_from_rfc3339(&self.ts)
            .ok()
            .map(|dt| dt.timestamp())
    }
}

/// Parse every line of `log_path` into a [`LogRecord`], in on-disk (oldest
/// first) order. Malformed lines are skipped, not fatal — mirrors
/// [`crate::learn::journal::fold_at`]'s resilience over the sibling store. A
/// missing file (nothing harvested yet) yields an empty vec, not an error.
pub fn read_log(log_path: &Path) -> Vec<LogRecord> {
    let Ok(content) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<LogRecord>(l.trim()).ok())
        .collect()
}

/// The most recent **ambient**, successfully **extracted** run in the log —
/// the refresh discovery line's data source (design doc's "Refresh" surface).
/// Manual (`load harvest`) runs and non-extracted outcomes (`empty`,
/// `failed`, `no_cli`, …) are not summarized: the line's wording
/// ("harvested N sessions via CLI (model) — M new candidates") only has
/// content to show for a completed extraction, and the card scopes the line
/// to ambient runs specifically (a manual run's outcome is already visible in
/// that same terminal).
pub fn latest_ambient_extraction(log_path: &Path) -> Option<LogRecord> {
    read_log(log_path)
        .into_iter()
        .rev()
        .find(|r| r.trigger == "ambient" && r.outcome == Outcome::Extracted.label())
}

/// Return the newest still-actionable breaker failure for this machine.
///
/// The consecutive-failure counter is authoritative: a reset makes all older
/// log entries historical. While the counter is nonzero, scan backward only
/// for outcomes that increment it and stop at a successful extraction. Empty,
/// throttled, unsupported, and future unknown outcomes cannot hide a failure
/// or accidentally become one.
pub fn latest_unresolved_failure(learn_dir: &Path) -> Option<LogRecord> {
    if state::consecutive_failures_at(learn_dir) == 0 {
        return None;
    }

    for record in read_log(&learn_dir.join("log.jsonl")).into_iter().rev() {
        if record.outcome == Outcome::Extracted.label() {
            return None;
        }
        if matches!(
            record.outcome.as_str(),
            "failed" | "deadline_exceeded" | "corrupt_watermarks"
        ) {
            return Some(record);
        }
    }
    None
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
        ) -> Result<agent_cli::InvokeOut, agent_cli::InvokeFailure> {
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

    struct FailingExtractor {
        failure: agent_cli::InvokeFailure,
        calls: Cell<usize>,
    }

    struct FencingExtractor {
        lock_path: PathBuf,
        calls: Cell<usize>,
    }

    impl Extractor for FencingExtractor {
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
        ) -> Result<agent_cli::InvokeOut, agent_cli::InvokeFailure> {
            self.calls.set(self.calls.get() + 1);
            let foreign =
                r#"{"pid":999999,"started_at":1,"token":"ffffffffffffffffffffffffffffffff"}"#;
            std::fs::write(&self.lock_path, foreign).unwrap();
            Err(agent_cli::InvokeFailure {
                provider: agent_cli::ProviderId::Claude,
                kind: agent_cli::InvokeFailureKind::TimedOut,
                exit_code: None,
                signal: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                io_kind: None,
                os_error: None,
                usage: None,
            })
        }
    }

    impl Extractor for FailingExtractor {
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
        ) -> Result<agent_cli::InvokeOut, agent_cli::InvokeFailure> {
            self.calls.set(self.calls.get() + 1);
            Err(self.failure.clone())
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
            interval: Duration::from_secs(6 * 3600),
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

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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

    #[test]
    fn corrupt_watermarks_get_a_stable_preflight_diagnostic() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        std::fs::write(&ctx.watermarks_path, "not json").unwrap();
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::None).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Corrupt);
        assert_eq!(
            out.diagnostic.as_ref().map(HarvestDiagnostic::code),
            Some("watermark_store_corrupt")
        );
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 1);
        let logs = log_lines(ctx);
        assert_eq!(logs[0]["error_stage"], "preflight");
        assert_eq!(logs[0]["error_code"], "watermark_store_corrupt");
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
        state::record_failure_at(&ctx.learn_dir);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 0);
        assert!(
            latest_unresolved_failure(&ctx.learn_dir).is_none(),
            "a later extraction clears the breaker"
        );
    }

    // --- Fix N2: evidence quotes kept only for resolvable session_refs -----

    #[test]
    fn evidence_quotes_are_kept_only_for_resolvable_session_refs() {
        // A candidate can cite evidence for a session NOT in this scan (the
        // model naming an unknown or rotated-away session). Such a quote has no
        // attributable session backing, so it must be dropped — only the quote
        // for the resolvable ref is stored (the same `by_ref` gate the
        // SessionRef already uses).
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm, never npm.");
        // Two evidence entries, distinct quotes: one resolvable (claude:sess-1),
        // one not (claude:ghost, never scanned).
        let response = r#"{"candidates":[{"claim":"Always use pnpm, never npm.","kind":"preference","evidence":[{"session_ref":"claude:sess-1","quote":"resolvable quote"},{"session_ref":"claude:ghost","quote":"unresolvable quote"}]}]}"#;
        let stub = StubExtractor::new(&[response]);
        let guard = acquire(ctx);
        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Extracted);
        assert_eq!(out.candidates, 1);

        let id = journal::candidate_id("Always use pnpm, never npm.");
        let body = std::fs::read_to_string(ctx.evidence_dir.join(format!("{id}.json"))).unwrap();
        assert!(
            body.contains("resolvable quote"),
            "the resolvable ref's quote is stored: {body}"
        );
        assert!(
            !body.contains("unresolvable quote"),
            "a quote for an unresolvable session_ref must be dropped: {body}"
        );
    }

    // --- ACCEPTANCE 2: malformed output burns the tick and nothing else ---

    #[test]
    fn malformed_output_writes_spend_stamp_but_no_marks_or_events() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(ctx, "sess-1", "TRANSCRIPT_SENTINEL Always use pnpm.");
        let sentinel = "RAW_MODEL_SENTINEL this is not valid json {";
        let stub = StubExtractor::new(&[sentinel]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Failed);
        assert_eq!(
            out.diagnostic.as_ref().map(HarvestDiagnostic::code),
            Some("output_json_invalid")
        );
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
        assert_eq!(logs[0]["error_stage"], "validate_output");
        assert_eq!(logs[0]["error_code"], "output_json_invalid");
        let raw_log = std::fs::read_to_string(&ctx.log_path).unwrap();
        assert!(
            !raw_log.contains("RAW_MODEL_SENTINEL"),
            "raw model output must never enter log.jsonl: {raw_log}"
        );
        assert!(
            !raw_log.contains("TRANSCRIPT_SENTINEL"),
            "transcript text must never enter log.jsonl: {raw_log}"
        );
        // One failure recorded → not yet paused.
        assert!(!state::paused_at(&ctx.learn_dir));
    }

    #[test]
    fn invoke_failure_uses_one_diagnostic_for_outcome_log_and_counter() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let stub = FailingExtractor {
            failure: agent_cli::InvokeFailure {
                provider: agent_cli::ProviderId::Claude,
                kind: agent_cli::InvokeFailureKind::RateLimited,
                exit_code: Some(1),
                signal: None,
                stdout_bytes: 123,
                stderr_bytes: 45,
                io_kind: None,
                os_error: None,
                usage: Some(r#"{"input_tokens":10}"#.to_string()),
            },
            calls: Cell::new(0),
        };
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Failed);
        assert_eq!(stub.calls.get(), 1);
        assert_eq!(
            out.diagnostic.as_ref().map(HarvestDiagnostic::code),
            Some("cli_rate_limited")
        );
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 1);
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["error_stage"], "cli_output");
        assert_eq!(logs[0]["error_code"], "cli_rate_limited");
        assert_eq!(logs[0]["usage"]["input_tokens"], 10);
    }

    #[test]
    fn journal_append_failure_is_classified_without_advancing_watermarks() {
        let mut e = env(LearnScope::All, vec![]);
        let ctx = &mut e.ctx;
        let src = write_claude_session(ctx, "sess-1", "Always use pnpm.");
        std::fs::write(&ctx.inbox_dir, "not a directory").unwrap();
        let stub = StubExtractor::new(&[valid_extraction("claude:sess-1", "Always use pnpm.")]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Failed);
        assert_eq!(
            out.diagnostic.as_ref().map(HarvestDiagnostic::code),
            Some("journal_append_failed")
        );
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 1);
        assert!(
            Watermarks::load_from(&ctx.watermarks_path)
                .mark(&src.to_string_lossy())
                .is_none(),
            "a failed journal append must not advance the watermark"
        );
        let logs = log_lines(ctx);
        assert_eq!(logs[0]["error_stage"], "persist_journal");
        assert_eq!(logs[0]["error_code"], "journal_append_failed");
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

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
        assert_eq!(logs[0]["error_stage"], "spend_guard");
        assert_eq!(logs[0]["error_code"], "spend_stamp_write_failed");
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

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();

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

    #[test]
    fn worker_fenced_after_spend_writes_no_failure_counter_or_log() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let stub = FencingExtractor {
            lock_path: ctx.learn_dir.join("lock.json"),
            calls: Cell::new(0),
        };
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(stub.calls.get(), 1);
        assert_eq!(out.outcome, Outcome::Fenced);
        assert!(
            ctx.spend_stamp.exists(),
            "the pre-call spend stamp remains the only durable spend evidence"
        );
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 0);
        assert!(
            log_lines(ctx).is_empty(),
            "a post-spend worker that lost its fence must not append a competing log entry"
        );
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
        let out1 = run_harvest_ctx(ctx, WorkerSelection::None).unwrap();
        assert_eq!(out1.outcome, Outcome::Empty);
        let unresolved = latest_unresolved_failure(&ctx.learn_dir).unwrap();
        assert_eq!(
            unresolved.error_code.as_deref(),
            Some("stale_lock_reclaimed")
        );
        assert!(
            !state::paused_at(&ctx.learn_dir),
            "one reclaim → not yet paused"
        );

        plant_stale(ctx);
        let out2 = run_harvest_ctx(ctx, WorkerSelection::None).unwrap();
        assert_eq!(out2.outcome, Outcome::Empty);
        assert!(
            state::paused_at(&ctx.learn_dir),
            "a second reclaim → two failures → paused"
        );
        let logs = log_lines(ctx);
        assert_eq!(
            logs.len(),
            4,
            "each reclaim is audited before its empty run"
        );
        assert_eq!(logs[0]["error_code"], "stale_lock_reclaimed");
        assert_eq!(logs[1]["outcome"], "empty");
        assert_eq!(logs[2]["error_code"], "stale_lock_reclaimed");
        assert_eq!(logs[3]["outcome"], "empty");
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
            let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
            let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
            let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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

        let out = run_body(ctx, &guard, WorkerSelection::None).unwrap();
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

    #[test]
    fn unsupported_cli_is_non_spending_and_does_not_increment_breaker() {
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx;
        let src = write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let unsupported = agent_cli::UnsupportedCli {
            cli_id: "claude",
            installed_version: Some("2.1.205".to_string()),
            minimum_version: agent_cli::CLAUDE_MIN_VERSION,
            reason: agent_cli::UnsupportedReason::TooOld,
        };
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Unsupported(unsupported)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::UnsupportedCli);
        assert_eq!(
            out.diagnostic.as_ref().map(HarvestDiagnostic::code),
            Some("claude_structured_output_unsupported")
        );
        assert!(!ctx.spend_stamp.exists(), "unsupported CLI → no spend");
        assert_eq!(state::consecutive_failures_at(&ctx.learn_dir), 0);
        assert!(
            Watermarks::load_from(&ctx.watermarks_path)
                .mark(&src.to_string_lossy())
                .is_none(),
            "unsupported CLI → content is preserved"
        );
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["outcome"], "unsupported_cli");
        assert_eq!(logs[0]["error_stage"], "preflight");
        assert_eq!(
            logs[0]["error_code"],
            "claude_structured_output_unsupported"
        );
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

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
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
        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Empty);
        assert_eq!(stub.calls(), 0, "an empty scan spends nothing");
        assert!(
            hint.exists(),
            "a run that advanced no watermarks must leave the hint for a later retry"
        );
    }

    // --- ambient self-throttle (defense-in-depth) --------------------------

    /// Write a spend stamp that is FRESH relative to the test clock
    /// (`fixed_now()`), i.e. within the 6h interval.
    fn fresh_spend_stamp(ctx: &Ctx) -> String {
        let val = (ctx.now_unix() - 60).to_string();
        std::fs::write(&ctx.spend_stamp, &val).unwrap();
        val
    }

    #[test]
    fn ambient_run_with_fresh_spend_stamp_is_throttled_before_any_scan() {
        let mut e = env(LearnScope::All, vec![]);
        e.ctx.trigger = "ambient";
        let ctx = &e.ctx;
        // Content EXISTS — only the interval throttle may stop this run.
        write_claude_session(ctx, "sess-1", "Always use pnpm.");
        let seeded = fresh_spend_stamp(ctx);
        let stub = StubExtractor::new(&["unused"]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(out.outcome, Outcome::Throttled);
        assert_eq!(stub.calls(), 0, "a throttled ambient run makes ZERO calls");
        assert_eq!(
            std::fs::read_to_string(&ctx.spend_stamp).unwrap(),
            seeded,
            "the spend stamp is untouched (not re-written, not consumed)"
        );
        assert!(!ctx.watermarks_path.exists(), "watermarks untouched");
        assert!(journal_events(ctx).is_empty(), "no events");
        let logs = log_lines(ctx);
        assert_eq!(logs.len(), 1, "one attributable throttled entry");
        assert_eq!(logs[0]["outcome"], "throttled");
        assert_eq!(logs[0]["trigger"], "ambient");
        assert!(
            !ctx.learn_dir.join("failures.json").exists(),
            "a throttled exit is NOT a failure"
        );
    }

    #[test]
    fn ambient_hint_does_not_bypass_the_worker_spend_throttle() {
        // A session-end eligibility hint never buys an extra extraction call
        // (design Decision #3 rejects spend that scales with usage; the consent
        // ceiling is ≤4 calls/day at defaults). So with a FRESH spend stamp an
        // ambient run is throttled even with a hint waiting — the hint is left
        // for the next DUE tick, which will harvest the just-ended session
        // (bypassing the readers' quiescence window, not the spend interval).
        let mut e = env(LearnScope::All, vec![]);
        e.ctx.trigger = "ambient";
        let ctx = &e.ctx;
        write_claude_session(ctx, "sess-1", "Always use pnpm, never npm.");
        let seeded = fresh_spend_stamp(ctx);
        let hint = plant_hint(ctx, "sess-1");
        let stub = StubExtractor::new(&["unused"]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(
            out.outcome,
            Outcome::Throttled,
            "a fresh spend stamp throttles the ambient run even with a hint waiting"
        );
        assert_eq!(stub.calls(), 0, "a throttled ambient run makes ZERO calls");
        assert_eq!(
            std::fs::read_to_string(&ctx.spend_stamp).unwrap(),
            seeded,
            "the spend stamp is untouched"
        );
        assert!(
            hint.exists(),
            "the hint survives a throttled run for the next due tick"
        );
    }

    #[test]
    fn manual_run_bypasses_the_worker_spend_throttle() {
        // Manual runs deliberately ignore the interval (q-manual-throttle): the
        // user typed it, so it runs — and still resets the ambient tick at step 6.
        let e = env(LearnScope::All, vec![]);
        let ctx = &e.ctx; // trigger stays "manual"
        write_claude_session(ctx, "sess-1", "Always use pnpm, never npm.");
        let seeded = fresh_spend_stamp(ctx);
        let stub = StubExtractor::new(&[valid_extraction(
            "claude:sess-1",
            "Always use pnpm, never npm.",
        )]);
        let guard = acquire(ctx);

        let out = run_body(ctx, &guard, WorkerSelection::Chosen(&stub)).unwrap();
        guard.release();

        assert_eq!(
            out.outcome,
            Outcome::Extracted,
            "manual runs are never throttled"
        );
        assert_eq!(stub.calls(), 1);
        assert_ne!(
            std::fs::read_to_string(&ctx.spend_stamp).unwrap(),
            seeded,
            "the manual run re-wrote the spend stamp (resets the ambient tick)"
        );
    }

    // --- LogRecord / read_log / latest_ambient_extraction (T15) -----------

    fn write_log_lines(log_path: &Path, lines: &[&str]) {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(log_path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn read_log_skips_malformed_lines_and_parses_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log.jsonl");
        write_log_lines(
            &log_path,
            &[
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"manual","cli":"claude","model":"haiku","sessions":1,"dropped_over_cap":0,"candidates":1,"quarantined":0,"duration_ms":10,"outcome":"extracted","usage":null,"skipped":[]}"#,
                "not json at all",
                r#"{"ts":"2026-07-10T11:00:00Z","trigger":"ambient","cli":"claude","model":"haiku","sessions":2,"dropped_over_cap":0,"candidates":2,"quarantined":0,"duration_ms":10,"outcome":"extracted","usage":null,"skipped":[]}"#,
            ],
        );

        let records = read_log(&log_path);
        assert_eq!(records.len(), 2, "the malformed line must be skipped");
        assert_eq!(records[0].trigger, "manual");
        assert_eq!(records[1].trigger, "ambient");
        assert_eq!(records[1].sessions, 2);
        assert_eq!(records[1].candidates, 2);
    }

    #[test]
    fn read_log_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_log(&dir.path().join("nope.jsonl")).is_empty());
    }

    #[test]
    fn latest_ambient_extraction_ignores_manual_runs_and_non_extracted_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log.jsonl");
        write_log_lines(
            &log_path,
            &[
                // A manual run must never surface as an ambient summary.
                r#"{"ts":"2026-07-10T09:00:00Z","trigger":"manual","cli":"claude","model":"haiku","sessions":9,"dropped_over_cap":0,"candidates":9,"quarantined":0,"duration_ms":10,"outcome":"extracted","usage":null,"skipped":[]}"#,
                // An ambient run that found nothing — not a completed extraction.
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"ambient","cli":null,"model":null,"sessions":0,"dropped_over_cap":0,"candidates":0,"quarantined":0,"duration_ms":10,"outcome":"empty","usage":null,"skipped":[]}"#,
                // The real one.
                r#"{"ts":"2026-07-10T11:00:00Z","trigger":"ambient","cli":"claude","model":"haiku","sessions":3,"dropped_over_cap":0,"candidates":2,"quarantined":0,"duration_ms":10,"outcome":"extracted","usage":null,"skipped":[]}"#,
            ],
        );

        let latest = latest_ambient_extraction(&log_path).expect("one qualifying entry");
        assert_eq!(latest.ts, "2026-07-10T11:00:00Z");
        assert_eq!(latest.sessions, 3);
        assert_eq!(latest.candidates, 2);
        assert_eq!(latest.cli.as_deref(), Some("claude"));
        assert_eq!(latest.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn latest_ambient_extraction_none_when_no_qualifying_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log.jsonl");
        write_log_lines(
            &log_path,
            &[
                r#"{"ts":"2026-07-10T09:00:00Z","trigger":"manual","cli":"claude","model":"haiku","sessions":1,"dropped_over_cap":0,"candidates":1,"quarantined":0,"duration_ms":10,"outcome":"extracted","usage":null,"skipped":[]}"#,
            ],
        );
        assert!(latest_ambient_extraction(&log_path).is_none());
        assert!(latest_ambient_extraction(&dir.path().join("missing.jsonl")).is_none());
    }

    #[test]
    fn ts_unix_parses_rfc3339_and_rejects_garbage() {
        let good = LogRecord {
            ts: "2026-07-10T11:00:00Z".to_string(),
            trigger: "ambient".to_string(),
            cli: None,
            model: None,
            sessions: 0,
            candidates: 0,
            duration_ms: None,
            usage: None,
            error_stage: None,
            error_code: None,
            error: None,
            outcome: "extracted".to_string(),
        };
        assert!(good.ts_unix().is_some());

        let mut bad = good.clone();
        bad.ts = "not a timestamp".to_string();
        assert!(bad.ts_unix().is_none());
    }

    #[test]
    fn log_record_reads_object_string_and_absent_usage_shapes() {
        // The production wire shape writes `usage` as a JSON OBJECT
        // (`{"input_tokens":…}`). A naive `Option<String>` would reject that and
        // `read_log` would silently drop the whole (successful, spend-bearing)
        // run — the exact spend-audit line the studio history must show. The
        // lenient deserializer keeps all three real shapes.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log.jsonl");
        write_log_lines(
            &log_path,
            &[
                // object usage (the real extracted-run shape)
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"ambient","cli":"claude","model":"haiku","sessions":1,"dropped_over_cap":0,"candidates":1,"quarantined":0,"duration_ms":1200,"outcome":"extracted","usage":{"input_tokens":10,"output_tokens":5},"skipped":[]}"#,
                // string usage
                r#"{"ts":"2026-07-10T10:05:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"empty","usage":"raw-blob"}"#,
                // absent usage + absent duration (older loadout)
                r#"{"ts":"2026-07-10T10:10:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"empty"}"#,
                // malformed foreign diagnostic fields must not drop the line
                r#"{"ts":"2026-07-10T10:15:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"failed","error_stage":{"bad":true},"error_code":42,"error":["not","a","string"]}"#,
            ],
        );

        let records = read_log(&log_path);
        assert_eq!(
            records.len(),
            4,
            "no line dropped by a usage-shape mismatch"
        );
        assert_eq!(
            records[0].usage.as_deref(),
            Some(r#"{"input_tokens":10,"output_tokens":5}"#)
        );
        assert_eq!(records[0].duration_ms, Some(1200));
        assert_eq!(records[1].usage.as_deref(), Some("raw-blob"));
        assert_eq!(records[2].usage, None);
        assert_eq!(records[2].duration_ms, None);
        assert_eq!(records[3].error_stage, None);
        assert_eq!(records[3].error_code, None);
        assert_eq!(records[3].error, None);
    }

    #[test]
    fn latest_unresolved_failure_survives_empty_and_reset_hides_history() {
        let dir = tempfile::tempdir().unwrap();
        write_log_lines(
            &dir.path().join("log.jsonl"),
            &[
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"manual","sessions":1,"candidates":0,"outcome":"failed","error_stage":"validate_output","error_code":"output_json_invalid","error":"safe"}"#,
                r#"{"ts":"2026-07-10T10:05:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"empty"}"#,
            ],
        );
        state::record_failure_at(dir.path());

        let unresolved = latest_unresolved_failure(dir.path()).unwrap();
        assert_eq!(
            unresolved.error_code.as_deref(),
            Some("output_json_invalid")
        );

        state::reset_failures_at(dir.path());
        assert!(
            latest_unresolved_failure(dir.path()).is_none(),
            "reset makes older log failures historical"
        );
    }

    #[test]
    fn latest_unresolved_failure_stops_at_later_extraction() {
        let dir = tempfile::tempdir().unwrap();
        write_log_lines(
            &dir.path().join("log.jsonl"),
            &[
                r#"{"ts":"2026-07-10T10:00:00Z","trigger":"manual","sessions":1,"candidates":0,"outcome":"failed","error_code":"old_failure"}"#,
                r#"{"ts":"2026-07-10T10:05:00Z","trigger":"manual","sessions":1,"candidates":1,"outcome":"extracted"}"#,
                r#"{"ts":"2026-07-10T10:10:00Z","trigger":"manual","sessions":0,"candidates":0,"outcome":"future_unknown"}"#,
            ],
        );
        // Keep the counter nonzero deliberately to exercise the log boundary;
        // production extraction resets it and returns even earlier.
        state::record_failure_at(dir.path());
        assert!(latest_unresolved_failure(dir.path()).is_none());
    }
}
