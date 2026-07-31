# Reliable harvest output and diagnostics — implementation plan

**Objective:** Stop Claude harvests from failing on unconstrained inner output,
and make harvest failures specific, persistent, and safe to inspect without
logging transcript-derived content.

**Design source:**
`.loadout/workflow/artifacts/design-learn-fenced-output.md`

**Branch/worktree:** Use the existing `fix/learn-fenced-output` branch in
`.claude/worktrees/learn-fence-fix`. Do not work on `main`.

**Done means:** Claude extraction uses a live-verified JSON Schema contract;
unsupported Claude versions are rejected before spend; every existing terminal
extraction failure gets a closed, privacy-safe diagnostic; unresolved failures
remain visible until the breaker is reset; manual CLI, status, doctor, and
Studio show the same reason; the full Rust validation suite is green.

## Constraints

- Use test-first changes. Every task begins with a failing focused test.
- Add no dependencies and do not change config schemas.
- Keep the strict extraction parser. Do not add JSON repair or fallback to
  Claude's free-form `.result`.
- Never log or print prompts, transcripts, raw stdout/stderr, model result
  bodies, candidate claims, evidence quotes, session ids, or environment data.
- Do not add another paid invocation, retry, or post-spend provider fallback.
- Keep evidence and watermark persistence semantics unchanged in this patch;
  their transactional recovery needs a separate design.
- Make logical Conventional Commits at the checkpoints below.

## Task 1 — Verify the live Claude structured-output contract

**Files:**

- `.loadout/workflow/artifacts/design-learn-fenced-output.md` — record only the
  verified contract facts; never record the response body.
- `/private/tmp` — temporary schema-print helper, removed after the check.

**Steps:**

1. Ask for explicit approval for one small metered Claude call.
2. Generate the exact compact value of
   `loadout::learn::extract::output_json_schema()` with a temporary helper; do
   not hand-copy the schema.
3. Invoke installed Claude 2.1.211 with the harvest flags, exact schema, tools
   disabled, no session persistence, and an inert prompt requesting
   `{"candidates":[]}`. Provide no transcript data.
4. Inspect in memory only. Confirm `--json-schema` works with
   `--output-format json` and `--tools ""`, the full schema is accepted,
   `structured_output` exists, and it is a JSON object rather than an encoded
   string.
5. Record those four booleans and the verified version in the design artifact.
   Delete the temporary helper. If any check fails, stop and revise the plan.

**Acceptance:** The production contract is based on observed CLI behavior, not
stub assumptions, and no response body or prompt is retained.

**Validation:** `git status --short` in the implementation worktree shows no
temporary helper or source change.

## Task 2 — Gate Claude selection and enforce structured output

**Files:**

- `src/learn/agent_cli.rs`
- `src/commands/learn.rs`

**Steps:**

1. Replace the boolean `select_with` seam with test inputs that carry probe
   versions. Add numeric version tests covering `2.1.99 < 2.1.206`,
   `2.1 == 2.1.0`, prerelease ordering, unknown suffixes, probe timeout, pinned
   old Claude, and unpinned fallback to Codex/Gemini.
2. Add a named minimum-Claude constant set to the live-verified version. Parse
   with `providers::parse_version`; store only numeric components and render
   only a canonical version string.
3. Replace `select() -> Option<CliChoice>` with `Selection::Chosen`,
   `Selection::Unsupported`, and `Selection::None`. Update the status call site
   so installed-but-old Claude is never reported as “no CLI.”
4. Update `invoke_claude_flags_env_stdin_and_result_unwrap` first: the stub must
   return `structured_output`, include misleading prose in `result`, and assert
   the exact compact schema follows `--json-schema`.
5. Implement the Claude adapter against the live contract. Keep the shared
   strict parser as defense in depth and never fall back to `.result`.
6. Update the `agent_cli.rs` module documentation from the 2.1.206/`.result`
   contract to the verified version/`.structured_output` contract.

**Acceptance:** Supported Claude produces strict extraction text; old/unknown
Claude is skipped or reported before spend; `.result` cannot influence output.

**Validation:** Run the focused `agent_cli` and selection unit tests.

## Task 3 — Update both end-to-end Claude stub suites

**Files:**

- `tests/learn.rs`
- `tests/cli.rs`

**Steps:**

1. Change shared Claude envelopes to carry an object-valued
   `structured_output` and deliberately invalid prose in `result`.
2. Assert `--json-schema` and its exact schema value pass through production
   selection, worker orchestration, and subprocess invocation.
3. Preserve the existing journal, evidence, watermark, usage, and candidate
   assertions.
4. Add unsupported-version cases proving there is no spend stamp or extraction
   call, pinned mode reports the minimum, and unpinned mode selects the next
   supported provider before spend.

**Acceptance:** The real `load harvest` path succeeds only from
`structured_output`; unsupported Claude never spends.

**Validation:** `cargo test --test learn` and the focused harvest test in
`tests/cli.rs`.

**Checkpoint commit:** `fix(learn): enforce Claude structured output`

## Task 4 — Add closed, privacy-safe provider and parse diagnostics

**Files:**

- `src/learn/agent_cli.rs`
- `src/learn/extract.rs`

**Steps:**

1. Add failing tests for the exact precedence: spawn/timeout, envelope parse,
   provider-reported error, required output field, process exit status, then
   strict payload validation.
2. Add closed provider failure variants containing only allowlisted metadata:
   provider id, numeric exit/signal, byte counts, known subtype, or normalized
   `io::ErrorKind`. Do not accept arbitrary error strings.
3. Map known auth, rate-limit, and structured-retry signals to loadout-authored
   messages and next actions. Map unknown bodies to a generic code without
   persisting the body.
4. Preserve usage from parsed error envelopes when available.
5. Change `extract::parse_output` so callers can classify Serde syntax/EOF
   versus data/schema failures using category, line, and column. Never persist
   `serde_json::Error::to_string()` or model-controlled field names.
6. Update `invoke_claude_is_error_true_is_a_failure`,
   `invoke_codex_empty_message_file_is_a_failure`, and
   `invoke_gemini_missing_response_is_a_failure` to assert stable safe codes
   and that sentinel stdout/stderr/error bodies are absent.

**Acceptance:** No provider failure can carry raw provider/model text into the
worker; the diagnostic still distinguishes authentication, timeout, protocol,
missing output, syntax, and schema failures.

**Validation:** Run the focused `agent_cli` and `extract` unit tests.

## Task 5 — Centralize fenced worker diagnostics and log compatibility

**Files:**

- `src/learn/worker.rs`
- `src/learn/state.rs`

**Steps:**

1. Define a closed `HarvestDiagnostic` enum with methods for stage, code, and a
   loadout-authored message. Add `diagnostic: Option<HarvestDiagnostic>` to
   `RunOutcome`.
2. Thread `Selection` into `run_harvest_ctx`. Add non-spending
   `Outcome::UnsupportedCli`/`unsupported_cli`; it logs an actionable result but
   does not increment the breaker.
3. Centralize terminal failure handling so outcome, duration, usage, counter,
   and log entry cannot disagree. Move all failure-counter/log writes behind
   `guard.still_held()`.
4. Preserve the current post-spend fence rule: a fenced-out worker writes no
   counter or log entry; the spend stamp is the only durable spend evidence.
5. When a stale lock is reclaimed, append a fenced `failed` entry with
   `stale_lock_reclaimed` before continuing.
6. Flatten diagnostics into optional `error_stage`, `error_code`, and existing
   `error` fields. Add lenient independent deserializers so non-string or absent
   fields never drop an old log line. Cap only legacy display text later.
7. Add `state::consecutive_failures_at` and
   `worker::latest_unresolved_failure`. Scan only the breaker-failure allowlist
   and stop at `extracted`; ignore all other and unknown outcomes.
8. Make a failed log append emit a fixed stderr warning only for spend-bearing
   or diagnostic entries, not `empty`/`throttled` no-ops.
9. Keep best-effort evidence writes and ignored watermark-save behavior
   unchanged and documented as deferred.

**Acceptance:** Every existing terminal extraction failure gets one stable safe
diagnostic, all shared failure writes obey the fence, and old logs remain
readable.

**Validation:** Focused worker/state tests, including: fail→empty still shows the
failure; `load learn on` reset hides history; reclaimed→empty shows reclamation;
later extraction clears the breaker; raw sentinels never enter `log.jsonl`.

**Checkpoint commit:** `fix(learn): record safe harvest diagnostics`

## Task 6 — Surface the same diagnostic in CLI, status, and doctor

**Files:**

- `src/commands/harvest.rs`
- `src/commands/learn.rs`
- `src/commands/doctor.rs`
- `tests/cli.rs`

**Steps:**

1. Make manual harvest output show `stage/code` and the derived message for
   failures; keep generic fallback text for legacy records.
2. Make status show the latest unresolved breaker failure even after `empty`,
   and show current unsupported selection independently of the breaker.
3. Make doctor warn only while learning is enabled and activated with an
   unresolved failure or unsupported selected CLI. Disabled learning treats
   old failures as history.
4. Add CLI tests for active, disabled, reset, fail→empty, unsupported pinned,
   and an older log record with no diagnostic fields.

**Acceptance:** The terminal tells the user whether to upgrade, authenticate,
inspect schema output, or fix local state without exposing raw data.

**Validation:** Focused command tests plus `cargo test --test cli`.

## Task 7 — Render escaped diagnostics in Studio history

**Files:**

- `src/studio/inbox.rs`
- `src/studio/assets/studio.css`
- `src/studio/server.rs`

**Steps:**

1. Add a failing route test for a failed log row with stage, code, and error.
2. Render one full-width diagnostic line below failed history metadata. Let
   Maud escape all log-derived text.
3. Cap only legacy error strings to 512 Unicode scalar values at display time.
4. Add tests for missing fields, non-string diagnostic fields, malicious HTML,
   and a long legacy error. Confirm none drop the history row.
5. Add compact CSS that preserves the existing history density.

**Acceptance:** Harvest history names the failure safely and remains readable
for old or malformed local log lines.

**Validation:** Focused Studio route tests. No new browser-test dependency is
needed because the change is static server-rendered HTML.

**Checkpoint commit:** `fix(studio): show harvest failure diagnostics`

## Task 8 — Full verification and handoff

**Steps:**

1. Run:

   ```text
   cargo fmt --check
   cargo test
   cargo clippy --all-targets
   cargo build
   ```

2. Re-run `load doctor` and the isolated stub harvest suite. Do not run another
   paid Claude call.
3. Review `git diff --check`, `git status`, and each logical commit.
4. Ask the user to open Studio → Settings → Learning → Harvest history and
   manually confirm the failed-row layout. The existing browser smoke targets
   the plan viewer, not Studio, and Playwright is disproportionate for this
   static row.

**Acceptance:** All automated gates pass; any skipped or manual validation is
reported explicitly; no secrets or raw model output appear in diffs or test
fixtures.

## Risks

- The live Claude contract may differ from current official documentation. The
  Task 1 gate stops implementation before assumptions become code.
- The 2.1.211 floor may reject versions that happen to support the flag. This is
  deliberate fail-closed behavior until an earlier version is verified.
- Gemini remains prompt-constrained because its CLI lacks a schema flag. The
  new diagnostics improve visibility, not Gemini reliability.
- Journal append is not transactional. This patch diagnoses its existing fatal
  path but does not claim retry idempotency.
- Evidence and watermark persistence can still fail silently after journal
  output. Fixing that safely requires a replay checkpoint and separate design.

## Rollback

Revert the three checkpoint commits. New log keys and the `unsupported_cli`
outcome are additive; old readers accept outcome strings and ignore unknown
fields. Do not rewrite logs, reset watermarks, or delete the learning state.

## First implementation step

Get explicit approval for Task 1's one small metered compatibility smoke. If it
confirms the contract, begin Task 2 with the failing selection/version and
Claude-envelope tests in `src/learn/agent_cli.rs`.
