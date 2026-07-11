//! Slice assembly: turn raw reader output ([`SessionSlice`]s) into the
//! bounded, redacted input the extraction call is built from (the prompt
//! builder itself is [`super::extract`], Task 10).
//!
//! ## Ordering, and why it is fixed
//!
//! [`assemble`] applies six filters/transforms in a specific order, each
//! individually under test below:
//!
//! 1. **Recursion guard (self-exclusion layer 3).** Drop any session whose
//!    `cwd` is the harvest worker's own working directory
//!    (`state_dir/learn/work/`, passed in as `work_dir`) — the worker's own
//!    agent-CLI invocation could otherwise mine its own transcript.
//! 2. **Sentinel guard (self-exclusion layer 4).** Drop any session with a
//!    message containing [`super::extract::SENTINEL`] — the harvest prompt's
//!    own marker. This is the backstop for sessions layer 3 cannot catch
//!    (no recoverable `cwd` — gemini, always; or a one-shot invocation from
//!    another store).
//! 3. **Scope filter.** `LearnScope::Adopted` (the default) keeps only
//!    sessions whose cwd resolves to a repo loadout has adopted
//!    ([`repo_is_adopted`]); `All` keeps everything regardless of cwd. A
//!    session with no cwd (gemini, always — see
//!    [`crate::learn::readers::SessionSlice::cwd`]) has nothing to resolve,
//!    so it is dropped under `Adopted` and kept only under `All`.
//! 4. **Age cutoff.** Sessions older than `age_cutoff_days` days *before*
//!    the watermark baseline are dropped — the "don't harvest years of
//!    backlog on first enable" guarantee. No baseline yet (not set, or
//!    unparseable) means no cutoff is applied.
//! 5. **Redaction.** [`redact_secrets`] runs over every remaining message
//!    *before* anything is size-accounted (step 6). A secret is never let
//!    through on the theory that its *original* byte count "already fit" —
//!    only the redacted text is ever counted, returned, or sent anywhere.
//! 6. **Caps.** Sort newest-first by `ts`, then keep a strict prefix: as
//!    soon as either the session-count cap or the cumulative-byte cap would
//!    be exceeded, stop entirely — every remaining (older) session is
//!    dropped, never deferred to a later run (the caller still advances its
//!    watermark past everything scanned, including what was cut here).
//!    `dropped_over_cap` reports how many so the run log is never silent
//!    about a cut. This is a *prefix* truncation, not best-fit packing: an
//!    older-but-smaller session is never kept ahead of a newer-but-larger
//!    one, so every kept session is newer than every dropped one.
//!
//! Redaction (step 5) running before caps (step 6) is the one ordering rule
//! that is a safety property rather than a convenience: caps must never see,
//! and therefore never budget around, an unredacted secret.
//!
//! Steps 1 and 4 have no counters on [`Assembled`] (only scope/sentinel/caps
//! do) — see that struct's doc for why.
//!
//! ## `ts` and lexicographic ordering
//!
//! Every reader ([`super::readers::claude`], [`super::readers::codex`],
//! [`super::readers::gemini`]) documents `ts` as "RFC 3339 UTC", and every
//! fixture across all three (verified while writing this module) uses the
//! same fixed-width form, `YYYY-MM-DDTHH:MM:SS.mmmZ` — millisecond
//! precision, always `Z`, never a numeric offset. That makes plain `str`
//! comparison equivalent to chronological comparison: every field is at a
//! fixed width, so no field is ever short a digit the way e.g.
//! non-zero-padded hours would be. This module relies on that and does not
//! re-parse `ts` for sorting or the caps truncation (step 6) — only the
//! baseline cutoff (step 4, [`cutoff_ts`]) parses a timestamp at all, and
//! only to *produce* a cutoff string in that same fixed-width form, so the
//! rest of the comparisons can stay plain string comparisons. If a reader
//! ever emitted a differently-shaped timestamp (a raw offset instead of `Z`,
//! or second instead of millisecond precision), sorting and the cutoff
//! compare would silently misorder rather than error — worth a regression
//! fixture per reader if that assumption is ever loosened.

use std::path::Path;

use crate::binding;
use crate::config::{self, LearnScope};
use crate::context;
use crate::learn::readers::SessionSlice;
use crate::redact::redact_secrets;

use super::extract;

/// Whether `cwd` sits inside a repo loadout has adopted: rendered generated
/// output present, or a repo-scope profile binding recorded — both live
/// under the repo's `.loadout/` directory (see [`crate::config`]), keyed off
/// the git repo root (or `cwd` itself outside a repo — [`context::repo_base_for`]
/// falls back the same way the rest of loadout's detection does).
pub fn repo_is_adopted(cwd: &Path) -> bool {
    let repo_base = context::repo_base_for(cwd);
    binding::read_repo(&repo_base).is_some() || generated_dir_nonempty(&repo_base)
}

/// `.loadout/generated/` has at least one entry. `false` for a missing
/// directory or a read error (unreadable, not a directory) — fail closed:
/// an adoption check that can't confirm adoption must not claim it.
fn generated_dir_nonempty(repo_base: &Path) -> bool {
    std::fs::read_dir(config::generated_dir(repo_base))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Per-run limits enforced by [`assemble`]'s caps step (step 6). Defaults
/// match the shipped release caps (design doc Decision #3): at most 20
/// session slices, at most 400KB of already-redacted message text, and a
/// 14-day age cutoff measured back from the watermark baseline.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Maximum number of sessions kept per run.
    pub max_sessions: usize,
    /// Maximum cumulative bytes of (redacted) message text kept per run.
    pub max_bytes: usize,
    /// Sessions whose `ts` is older than this many days before the
    /// watermark baseline are dropped (step 4).
    pub age_cutoff_days: i64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_sessions: 20,
            max_bytes: 400_000,
            age_cutoff_days: 14,
        }
    }
}

/// The result of one [`assemble`] pass: the bounded, redacted slices ready
/// for the extraction prompt, plus how many sessions the scope filter, the
/// sentinel guard, and the caps step each dropped.
///
/// Steps 1 (recursion guard) and 4 (age cutoff) have no counters here: they
/// are not expected to fire in ordinary operation the way scope/sentinel/caps
/// routinely do (layer 1's job is defense-in-depth against a bug elsewhere,
/// and a first-enable age-cutoff drop is a one-time event, not a per-run
/// signal worth surfacing the way an unexpected mid-cap cut is). If a later
/// task wants those counted too (e.g. for the run log), add fields here —
/// this struct's shape is not otherwise load-bearing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembled {
    pub slices: Vec<SessionSlice>,
    pub dropped_over_cap: usize,
    pub dropped_scope: usize,
    pub dropped_sentinel: usize,
}

/// Turn raw reader output into the bounded, redacted extraction input. See
/// the module doc for the exact order of operations and why it is fixed.
///
/// `work_dir` is the harvest worker's own working directory
/// (`state_dir/learn/work/` in production); `baseline` is
/// `watermarks.baseline()` — `None` before the first enable has set one, in
/// which case step 4 (the age cutoff) is skipped entirely.
pub fn assemble(
    mut slices: Vec<SessionSlice>,
    caps: &Caps,
    scope: LearnScope,
    baseline: Option<&str>,
    work_dir: &Path,
) -> Assembled {
    // 1. Recursion guard (self-exclusion layer 3): drop the worker's own cwd.
    slices.retain(|s| s.cwd.as_deref() != Some(work_dir));

    // 2. Sentinel guard (self-exclusion layer 4): drop any session whose
    // messages contain the harvest prompt's own marker.
    let before = slices.len();
    slices.retain(|s| !s.messages.iter().any(|m| m.contains(extract::SENTINEL)));
    let dropped_sentinel = before - slices.len();

    // 3. Scope filter.
    let before = slices.len();
    slices.retain(|s| match scope {
        LearnScope::All => true,
        LearnScope::Adopted => s.cwd.as_deref().is_some_and(repo_is_adopted),
    });
    let dropped_scope = before - slices.len();

    // 4. Age cutoff: drop sessions older than `age_cutoff_days` before the
    // baseline. No baseline (unset, or unparseable) means no cutoff.
    if let Some(cutoff) = baseline.and_then(|b| cutoff_ts(b, caps.age_cutoff_days)) {
        slices.retain(|s| s.ts >= cutoff);
    }

    // 5. Redact every remaining message before anything is size-accounted.
    for s in &mut slices {
        for m in &mut s.messages {
            *m = redact_secrets(m);
        }
    }

    // 6. Newest-first, then a strict prefix under caps. Over-cap content is
    // dropped, not deferred; the caller still advances watermarks past it.
    slices.sort_by(|a, b| b.ts.cmp(&a.ts));
    let before = slices.len();
    let mut kept = Vec::with_capacity(slices.len().min(caps.max_sessions));
    let mut bytes = 0usize;
    for s in slices {
        if kept.len() >= caps.max_sessions {
            break;
        }
        let size: usize = s.messages.iter().map(|m| m.len()).sum();
        if bytes + size > caps.max_bytes {
            break;
        }
        bytes += size;
        kept.push(s);
    }
    let dropped_over_cap = before - kept.len();

    Assembled {
        slices: kept,
        dropped_over_cap,
        dropped_scope,
        dropped_sentinel,
    }
}

/// `baseline` minus `age_cutoff_days`, reformatted to the same fixed-width
/// `YYYY-MM-DDTHH:MM:SS.mmmZ` form the readers emit, so [`assemble`] can drop
/// old sessions with a plain string compare against `ts` (see the module
/// doc). `None` if `baseline` doesn't parse as RFC 3339 — treated as "no
/// cutoff" rather than a crash or a fail-closed drop-everything, since a
/// malformed baseline is a bug in the code that wrote it, not a signal this
/// function should act on.
fn cutoff_ts(baseline: &str, age_cutoff_days: i64) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(baseline).ok()?;
    let cutoff = dt.with_timezone(&chrono::Utc) - chrono::Duration::days(age_cutoff_days);
    Some(cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::REDACTED;
    use std::path::PathBuf;

    /// Build a fixture [`SessionSlice`]. `cwd` and `messages` are the only
    /// fields most tests vary; the rest get harmless defaults.
    fn slice(session_id: &str, cwd: Option<&Path>, ts: &str, messages: &[&str]) -> SessionSlice {
        SessionSlice {
            agent: "claude",
            session_id: session_id.to_string(),
            cwd: cwd.map(|p| p.to_path_buf()),
            ts: ts.to_string(),
            messages: messages.iter().map(|m| m.to_string()).collect(),
            source_file: PathBuf::from(format!("/tmp/{session_id}.jsonl")),
            end_offset: 0,
        }
    }

    const WORK_DIR: &str = "/state/learn/work";

    fn work_dir() -> &'static Path {
        Path::new(WORK_DIR)
    }

    // --- step 1: recursion guard (cwd == work_dir) --------------------------

    #[test]
    fn recursion_guard_drops_session_whose_cwd_is_the_worker_dir() {
        let worker_session = slice(
            "worker",
            Some(work_dir()),
            "2026-06-01T10:00:00.000Z",
            &["hello"],
        );
        let real_session = slice(
            "real",
            Some(Path::new("/home/dev/project")),
            "2026-06-01T09:00:00.000Z",
            &["hello"],
        );
        let out = assemble(
            vec![worker_session, real_session],
            &Caps::default(),
            LearnScope::All,
            None,
            work_dir(),
        );
        assert_eq!(out.slices.len(), 1);
        assert_eq!(out.slices[0].session_id, "real");
    }

    #[test]
    fn recursion_guard_does_not_drop_a_session_with_no_cwd() {
        // Gemini sessions always have cwd: None; the recursion guard (a plain
        // cwd equality check) cannot and must not touch them — that's the
        // sentinel guard's (step 2) job instead.
        let s = slice("gemini-sess", None, "2026-06-01T10:00:00.000Z", &["hi"]);
        let out = assemble(vec![s], &Caps::default(), LearnScope::All, None, work_dir());
        assert_eq!(out.slices.len(), 1);
    }

    // --- step 2: sentinel guard ---------------------------------------------

    #[test]
    fn sentinel_guard_drops_any_session_containing_the_marker() {
        let tainted = slice(
            "tainted",
            Some(Path::new("/home/dev/project")),
            "2026-06-01T10:00:00.000Z",
            &[
                "some preamble",
                &format!("prompt has {}", extract::SENTINEL),
            ],
        );
        let clean = slice(
            "clean",
            Some(Path::new("/home/dev/project")),
            "2026-06-01T09:00:00.000Z",
            &["just a normal message"],
        );
        let out = assemble(
            vec![tainted, clean],
            &Caps::default(),
            LearnScope::All,
            None,
            work_dir(),
        );
        assert_eq!(out.slices.len(), 1);
        assert_eq!(out.slices[0].session_id, "clean");
        assert_eq!(out.dropped_sentinel, 1);
    }

    // --- step 3: scope filter -----------------------------------------------

    #[test]
    fn adopted_scope_keeps_only_sessions_in_adopted_repos() {
        let adopted = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::generated_dir(adopted.path())).unwrap();
        std::fs::write(config::generated_dir(adopted.path()).join("claude.md"), "x").unwrap();

        let not_adopted = tempfile::tempdir().unwrap();

        let s_adopted = slice(
            "adopted-sess",
            Some(adopted.path()),
            "2026-06-01T10:00:00.000Z",
            &["hi"],
        );
        let s_not_adopted = slice(
            "not-adopted-sess",
            Some(not_adopted.path()),
            "2026-06-01T09:00:00.000Z",
            &["hi"],
        );
        let out = assemble(
            vec![s_adopted, s_not_adopted],
            &Caps::default(),
            LearnScope::Adopted,
            None,
            work_dir(),
        );
        assert_eq!(out.slices.len(), 1);
        assert_eq!(out.slices[0].session_id, "adopted-sess");
        assert_eq!(out.dropped_scope, 1);
    }

    #[test]
    fn adopted_scope_via_local_toml_binding_also_counts() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::repo_dir(repo.path())).unwrap();
        std::fs::write(
            config::repo_local_path(repo.path()),
            "[binding]\nprofile = \"rust\"\n",
        )
        .unwrap();

        assert!(repo_is_adopted(repo.path()));
    }

    #[test]
    fn not_adopted_repo_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!repo_is_adopted(dir.path()));
    }

    #[test]
    fn gemini_sessions_with_no_cwd_are_dropped_under_adopted_and_kept_under_all() {
        let s = slice("gemini-sess", None, "2026-06-01T10:00:00.000Z", &["hi"]);

        let adopted_out = assemble(
            vec![s.clone()],
            &Caps::default(),
            LearnScope::Adopted,
            None,
            work_dir(),
        );
        assert_eq!(adopted_out.slices.len(), 0, "no cwd => nothing to adopt");
        assert_eq!(adopted_out.dropped_scope, 1);

        let all_out = assemble(vec![s], &Caps::default(), LearnScope::All, None, work_dir());
        assert_eq!(all_out.slices.len(), 1, "All scope keeps cwd-less sessions");
    }

    // --- step 4: age cutoff ---------------------------------------------------

    #[test]
    fn sessions_older_than_the_cutoff_are_dropped_others_kept() {
        // Baseline 2026-06-15T00:00:00Z, 14-day cutoff => 2026-06-01T00:00:00Z.
        let baseline = "2026-06-15T00:00:00.000Z";
        let too_old = slice(
            "too-old",
            Some(Path::new("/p")),
            "2026-05-31T23:59:59.000Z",
            &["hi"],
        );
        let exactly_at_cutoff = slice(
            "at-cutoff",
            Some(Path::new("/p")),
            "2026-06-01T00:00:00.000Z",
            &["hi"],
        );
        let newer = slice(
            "newer",
            Some(Path::new("/p")),
            "2026-06-10T00:00:00.000Z",
            &["hi"],
        );
        let out = assemble(
            vec![too_old, exactly_at_cutoff, newer],
            &Caps::default(),
            LearnScope::All,
            Some(baseline),
            work_dir(),
        );
        let ids: Vec<&str> = out.slices.iter().map(|s| s.session_id.as_str()).collect();
        assert!(!ids.contains(&"too-old"));
        assert!(ids.contains(&"at-cutoff"), "cutoff boundary is inclusive");
        assert!(ids.contains(&"newer"));
    }

    #[test]
    fn no_baseline_means_no_age_cutoff() {
        let ancient = slice(
            "ancient",
            Some(Path::new("/p")),
            "2000-01-01T00:00:00.000Z",
            &["hi"],
        );
        let out = assemble(
            vec![ancient],
            &Caps::default(),
            LearnScope::All,
            None,
            work_dir(),
        );
        assert_eq!(out.slices.len(), 1);
    }

    // --- step 5: redaction before caps ----------------------------------------

    #[test]
    fn secrets_are_redacted_in_the_output() {
        let s = slice(
            "has-secret",
            Some(Path::new("/p")),
            "2026-06-01T10:00:00.000Z",
            &["my key is AKIAIOSFODNN7EXAMPLE, don't share it"],
        );
        let out = assemble(vec![s], &Caps::default(), LearnScope::All, None, work_dir());
        assert_eq!(out.slices.len(), 1);
        assert!(out.slices[0].messages[0].contains(REDACTED));
        assert!(!out.slices[0].messages[0].contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redaction_happens_before_the_byte_cap_is_checked() {
        // The raw secret is 20 bytes; the cap is 15. If caps were checked
        // before redaction, this session would be dropped as over-cap. It
        // must survive, because redaction (14-byte "***REDACTED***") runs
        // first and only the redacted text is ever counted.
        let s = slice(
            "secret-only",
            Some(Path::new("/p")),
            "2026-06-01T10:00:00.000Z",
            &["AKIAIOSFODNN7EXAMPLE"],
        );
        let caps = Caps {
            max_sessions: 20,
            max_bytes: 15,
            age_cutoff_days: 14,
        };
        let out = assemble(vec![s], &caps, LearnScope::All, None, work_dir());
        assert_eq!(
            out.slices.len(),
            1,
            "redacted text (14 bytes) must fit under a 15-byte cap even though \
             the raw secret (20 bytes) would not"
        );
        assert_eq!(out.slices[0].messages[0], REDACTED);
    }

    // --- step 6: caps (newest-first prefix truncation) ------------------------

    #[test]
    fn max_sessions_keeps_only_the_newest_n_and_reports_the_rest() {
        let s1 = slice(
            "s1",
            Some(Path::new("/p")),
            "2026-06-01T10:00:00.000Z",
            &["a"],
        );
        let s2 = slice(
            "s2",
            Some(Path::new("/p")),
            "2026-06-02T10:00:00.000Z",
            &["b"],
        );
        let s3 = slice(
            "s3",
            Some(Path::new("/p")),
            "2026-06-03T10:00:00.000Z",
            &["c"],
        );
        let caps = Caps {
            max_sessions: 2,
            max_bytes: 400_000,
            age_cutoff_days: 14,
        };
        let out = assemble(vec![s1, s2, s3], &caps, LearnScope::All, None, work_dir());
        assert_eq!(out.slices.len(), 2);
        assert_eq!(out.dropped_over_cap, 1);
        // Newest-first: s3 (06-03) then s2 (06-02); s1 (06-01, oldest) cut.
        assert_eq!(out.slices[0].session_id, "s3");
        assert_eq!(out.slices[1].session_id, "s2");
    }

    #[test]
    fn max_bytes_stops_at_a_strict_prefix_not_best_fit() {
        // Three sessions, 100 bytes of message text each, newest-first order
        // s3, s2, s1. A 250-byte cap admits s3 (100) + s2 (100) = 200, then
        // stops before s1 (100 more would be 300 > 250) even though s1 alone
        // would fit — the cap must not cherry-pick an older, smaller session
        // ahead of a newer one that was cut.
        let msg = "x".repeat(100);
        let s1 = slice(
            "s1",
            Some(Path::new("/p")),
            "2026-06-01T10:00:00.000Z",
            &[&msg],
        );
        let s2 = slice(
            "s2",
            Some(Path::new("/p")),
            "2026-06-02T10:00:00.000Z",
            &[&msg],
        );
        let s3 = slice(
            "s3",
            Some(Path::new("/p")),
            "2026-06-03T10:00:00.000Z",
            &[&msg],
        );
        let caps = Caps {
            max_sessions: 20,
            max_bytes: 250,
            age_cutoff_days: 14,
        };
        let out = assemble(vec![s1, s2, s3], &caps, LearnScope::All, None, work_dir());
        let ids: Vec<&str> = out.slices.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s3", "s2"]);
        assert_eq!(out.dropped_over_cap, 1);
    }

    #[test]
    fn full_pipeline_orders_all_six_steps_together() {
        // One fixture per drop reason, plus one clean survivor, run through
        // the whole pipeline at once as an end-to-end sanity check (each
        // reason already has its own focused test above).
        let adopted = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config::generated_dir(adopted.path())).unwrap();
        std::fs::write(config::generated_dir(adopted.path()).join("claude.md"), "x").unwrap();

        let recursive = slice(
            "recursive",
            Some(work_dir()),
            "2026-06-10T00:00:00.000Z",
            &["hi"],
        );
        let tainted = slice(
            "tainted",
            Some(adopted.path()),
            "2026-06-10T00:00:00.000Z",
            &[extract::SENTINEL],
        );
        let out_of_scope = slice(
            "out-of-scope",
            Some(Path::new("/not/adopted")),
            "2026-06-10T00:00:00.000Z",
            &["hi"],
        );
        let too_old = slice(
            "too-old",
            Some(adopted.path()),
            "2026-01-01T00:00:00.000Z",
            &["hi"],
        );
        let survivor = slice(
            "survivor",
            Some(adopted.path()),
            "2026-06-10T00:00:00.000Z",
            &["my key is AKIAIOSFODNN7EXAMPLE"],
        );

        let out = assemble(
            vec![recursive, tainted, out_of_scope, too_old, survivor],
            &Caps::default(),
            LearnScope::Adopted,
            Some("2026-06-15T00:00:00.000Z"),
            work_dir(),
        );
        assert_eq!(out.slices.len(), 1);
        assert_eq!(out.slices[0].session_id, "survivor");
        assert!(out.slices[0].messages[0].contains(REDACTED));
    }
}
