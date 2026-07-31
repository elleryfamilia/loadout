# Reliable harvest structured output and diagnostics

- **Date:** 2026-07-16
- **Status:** revised after adversarial and cross-model review; ready for task planning
- **Target:** focused reliability fix after loadout 0.17.0
- **Implementation branch/worktree:** `fix/learn-fenced-output` in `.claude/worktrees/learn-fence-fix`

## Objective

Make `load harvest` reliably consume Claude's extraction output and make every
terminal failure on the existing extraction path diagnosable without retaining
transcript-derived model output. A successful fix should stop the current
repeated malformed-output failures, preserve the spend and watermark safety
rules, and show a safe, specific reason in the manual CLI, `load learn status`,
doctor, and Studio Harvest history.

## Relevant context

- Claude currently receives `--output-format json`, which only structures the
  outer CLI envelope. Loadout then reads free-form `.result` and subjects it to
  a strict Serde parser.
- Claude Code 2.1.211 is installed. Its current CLI supports `--json-schema` and
  returns validated data in `.structured_output`.
- Codex already receives the same extraction schema through `--output-schema`.
- `LogEntry` already has an optional `error` string, but almost every failure
  branch leaves it empty. `LogRecord` and Studio do not read or render it.
- The present three failed runs contain usage, proving the CLI call completed,
  but the raw output and parser error were discarded.
- Harvest failures must not advance watermarks or trigger a second loadout-level
  paid invocation. Two consecutive failures continue to pause ambient learning.

## Live Claude contract verification

The approved compatibility smoke ran on 2026-07-16 with Claude Code 2.1.211,
the harvest hygiene flags, tools disabled, the exact compact value from
`extract::output_json_schema()`, and no transcript data. The response body was
not retained. The observed contract facts were:

- `--json-schema` was accepted with `--output-format json` and `--tools ""`;
- the full schema, including `additionalProperties: false`, was accepted;
- the outer envelope contained `structured_output`; and
- `structured_output` was an object, not a JSON-encoded string.

## Approaches considered

### A. Prompt-only repair or tolerant parsing

Strengthen the prompt, strip markdown fences, or attempt to repair invalid JSON.
This is rejected because it leaves the model output contract probabilistic and
weakens the strict trust boundary.

### B. Claude structured output plus one free-form error string

Pass `--json-schema`, read `.structured_output`, and populate the existing
`error` field. This fixes the immediate problem with little code, but free-form
strings are unstable and make it easy to accidentally persist raw stdout,
stderr, or model text.

### C. Claude structured output plus typed, privacy-safe diagnostics

Pass the schema, keep the existing strict parser as defense in depth, and add a
small typed failure contract that is serialized as stable stage/code/message
fields. This is the preferred approach. It provides useful diagnostics without
storing the prompt or model response and can carry usage on provider-reported
errors.

## Preferred design

### 1. Enforce Claude's output contract

Before encoding the contract in stubs, run one separately approved, minimal
live compatibility smoke with the installed Claude CLI. Use the exact harvest
hygiene flags, exact extraction schema, an inert prompt that requests an empty
candidate list, and no transcript data. Confirm all four facts: the flag is
accepted with `--output-format json` and `--tools ""`; the full schema with
`additionalProperties: false` is accepted; the outer field is named
`structured_output`; and its value is an object rather than a JSON-encoded
string. Do not save the response body. If any fact differs, stop and revise the
adapter design before writing tests or production code.

After that gate, update `src/learn/agent_cli.rs::invoke_claude`:

1. Serialize `extract::output_json_schema()` to compact JSON.
2. Pass it as `--json-schema <schema>` alongside `--output-format json`.
3. Read the validated JSON value from `.structured_output`, not `.result`.
4. Serialize that value into `InvokeOut.text` so
   `extract::parse_output` remains the final strict validation boundary shared
   by providers.
5. Do not fall back to `.result`. An unsupported/old CLI or exhausted structured
   output retry must fail clearly instead of returning to the unreliable path.

Before the spend stamp, reuse the existing bounded `claude --version` probe and
require a minimum version actually verified against the smoke above. Start with
the installed 2.1.211. Lower that floor only when release notes or an additional
live fixture verifies the same contract on an earlier version; the old 2.1.206
hygiene-flags baseline does not prove structured-output support.

Replace `select() -> Option<CliChoice>` with a result that preserves why no
extractor was chosen:

```text
Selection::Chosen(CliChoice)
Selection::Unsupported { cli, found_version, minimum_version }
Selection::None
```

Reuse `providers::parse_version`, then carry a parsed numeric `CliVersion`
rather than the raw provider-controlled stdout token. Compare dotted components
numerically without a new dependency; missing components are zero, a prerelease
sorts below the matching release, and an unrecognized suffix fails closed. Test
that `2.1.99 < 2.1.206`, `2.1 == 2.1.0`, and `2.1.211-beta < 2.1.211`. Only a
canonical string rendered from the numeric value may appear in status, doctor,
or the log. An unparseable version or timed-out version probe fails closed for
Claude structured extraction. When Claude is not pinned, continue
the existing pre-spend probe order to Codex or Gemini, but retain the skipped
reason so `load learn status` and doctor can explain it if no supported choice
is found. When Claude is pinned, return `Unsupported` immediately. Thread the
selection through `run_harvest_ctx`; do not collapse it back to `None`.

`Unsupported` produces a non-spending `Outcome::UnsupportedCli` with wire label
`unsupported_cli` and diagnostic code
`claude_structured_output_unsupported`. It does not increment the consecutive
failure breaker because no extraction was attempted, but status and doctor show
an actionable upgrade warning. This adds a new outcome value, not merely new
fields; older readers still accept and display the string because `LogRecord`
does not deserialize outcomes into an enum. Do not use `--help` as the
capability test because Claude's own CLI documentation says the help output is
not exhaustive.

The prompt's existing JSON instructions stay in place. No JSON repair, markdown
fence stripping, or parser loosening is introduced.

### 2. Add a typed failure contract

Add a closed internal `HarvestDiagnostic` enum. Each variant owns only
allowlisted parameters such as provider id, numeric exit status, output byte
count, Serde category/line/column, normalized `io::ErrorKind`, and a known local
file class (`spend-stamp`, `journal`). Methods derive the stable stage, code,
and loadout-authored user message. No constructor accepts arbitrary message
text.

`RunOutcome` carries `diagnostic: Option<HarvestDiagnostic>` so the manual CLI
and persistent log use the same typed value. `LogEntry` flattens it into the
existing safe human `error` plus optional `error_stage` and `error_code` fields.
Those strings are derived only from the closed enum. For old or foreign log
lines, `LogRecord` reads the three fields independently with lenient custom
deserializers: non-string values become `None` rather than dropping the whole
line. Legacy error text is escaped and capped to 512 Unicode scalar values only
at display time.

The first stable stages/codes are:

| Stage | Codes |
|---|---|
| `preflight` | `run_deadline_exceeded`, `claude_structured_output_unsupported`, `watermark_store_corrupt`, `stale_lock_reclaimed` |
| `spend_guard` | `spend_stamp_write_failed` |
| `invoke` | `cli_spawn_failed`, `cli_timed_out`, `cli_process_failed` |
| `cli_output` | `cli_envelope_invalid`, `cli_auth_failed`, `cli_rate_limited`, `cli_structured_retries_exhausted`, `cli_reported_error`, `provider_output_missing` |
| `validate_output` | `output_json_invalid`, `output_schema_mismatch` |
| `persist_journal` | `journal_append_failed` |

Provider adapters return classified safe errors rather than `anyhow` messages
that interpolate stdout, stderr, or model error bodies. Classification follows
one exact order: spawn/timeout; parse the provider envelope; provider-reported
error; required structured field; process exit status; strict payload
validation. A valid provider error envelope therefore wins over a non-zero exit
status. Diagnostics may retain a numeric exit code/signal and an allowlisted
provider subtype, but never provider-controlled error prose.

Recognized Claude envelope subtypes and exact allowlisted authentication/rate
limit markers map to loadout-authored codes and next actions. For example,
`cli_auth_failed` tells the user to open Claude and authenticate. Unknown error
bodies map to `cli_reported_error`; their text is never persisted or printed.
`provider_output_missing` is provider-neutral and also covers an absent Codex
final-message file or Gemini `.response`.

`extract::parse_output` preserves the underlying `serde_json::Error` category,
line, and column long enough to distinguish syntax/EOF failures from data/schema
failures. It must not persist `serde_json::Error::to_string()` because an
unknown field name is model-controlled. I/O failures record a normalized I/O
kind and, when safe, an OS error number.

Never persist or print the extraction prompt, transcript text, raw result,
`structured_output`, stdout, stderr, candidate claims, evidence quotes, session
ids, cwd, environment, or provider-controlled paths as diagnostics. Known local
state file classes plus `io::ErrorKind` are allowed because they make
`journal_append_failed` and `spend_stamp_write_failed` actionable without
revealing transcript content. Do not add raw debug capture in this fix.

### 3. Make failure recording consistent and fenced

In `src/learn/worker.rs`:

- centralize the repeated failure handling so `RunOutcome`, the consecutive
  failure counter, and `LogEntry` receive the same diagnostic and usage;
- check `guard.still_held()` before every shared failure-counter/log write;
- preserve the spend stamp and no-watermark-advance behavior after any failed
  paid extraction;
- do not retry at the loadout level and do not switch providers after spending;
- make `log_run` emit a fixed safe stderr warning if a spend-bearing or
  diagnostic entry cannot be appended. Suppress that secondary warning for
  ordinary `empty` and `throttled` no-ops so an unwritable log does not spam
  every ambient tick. The log cannot record its own failure.

When a stale lock is reclaimed, write a fenced `failed` audit entry with
`stale_lock_reclaimed` before continuing the run. This keeps the existing rule
that reclamation increments the breaker while ensuring an `empty` continuation
cannot leave a nonzero failure count with no matching reason. A later extracted
success resets the counter and makes the reclamation historical.

The fence remains authoritative for the failure counter and run log. A worker
that loses the fence after spending does not append a competing log entry; the
spend stamp remains the durable evidence that a call occurred, but detailed
usage for that fenced-out call is unavailable. This preserves the module's
single-writer contract rather than weakening it for diagnostics.

This patch does not change the existing best-effort evidence-write or ignored
watermark-save semantics. Making either post-journal failure fatal would require
a replay checkpoint or transactional/idempotent journal design; otherwise the
next eligible run buys another extraction after durable output already exists.
That larger correctness issue is documented under Deferred work.

The run log keeps all existing fields and adds optional `error_stage` and
`error_code`. The existing optional `error` string becomes the safe human
message. Old records remain readable, and old binaries ignore the new fields.

### 4. Surface the reason where users look

- `src/commands/harvest.rs`: a manual failed run prints stage/code and the safe
  message instead of only saying “see the run log.”
- `src/commands/learn.rs`: status shows the latest failure reason when present.
- `src/commands/doctor.rs`: while learning is enabled and this machine is
  activated, a failed latest run is a warning rather than an OK finding and
  includes the safe diagnostic. When learning is disabled, the old failure is
  historical and does not keep doctor yellow. Old records fall back to a
  generic “this loadout version did not record a reason” explanation.
- `src/studio/inbox.rs` and `src/studio/assets/studio.css`: failed history rows
  get one compact full-width diagnostic line. Maud escaping remains the HTML
  boundary.

Do not use `log.last()` to decide whether a failure is still actionable. Add a
read-only consecutive-failure accessor in `src/learn/state.rs` and a
`latest_unresolved_failure` log helper. When the counter is nonzero, scan
backward for the explicit breaker-failure allowlist (`failed`,
`deadline_exceeded`, `corrupt_watermarks`) and stop at `extracted`; ignore every
other outcome, including future unknown values. (`busy` and `fenced` are not
logged today.) A manual empty scan does not reset the breaker and therefore
must not hide the failure. `load learn on` resets the counter, so historical log
entries do not keep status or doctor in warning state.

Unsupported selection is not a breaker failure. Status and doctor derive that
warning from the current `Selection` result even when no session was eligible
and no run-log entry exists.

## Assumptions

- Claude Code 2.1.211 is the initial supported baseline because that is the
  installed version whose exact live contract will be verified before code.
- Schema validation may cause Claude to repair output internally. That remains
  one CLI invocation and its additional tokens are visible in the usage blob.
- Machine-local run logs are diagnostic metadata, not a place for transcript or
  raw model content.
- The existing append-only log needs no migration; all new fields are optional.

## Scope boundaries

Included:

- Claude schema enforcement and correct envelope unwrapping;
- a pre-spend supported-version selection gate with fallback only before spend;
- typed safe provider/worker failure classification;
- unresolved-failure lookup that follows the breaker rather than `log.last()`;
- consistent fenced failure writes;
- CLI/status/doctor/Studio surfacing;
- automated regression, compatibility, privacy, and rendering tests.

The `src/learn/agent_cli.rs` module documentation is part of the implementation:
replace its 2.1.206 baseline and `.result` description with the verified minimum
and `.structured_output` contract.

Deferred:

- Gemini-native response schemas until its CLI exposes one;
- raw failed-output capture, even behind a debug flag;
- log rotation and bounded shared stdout/stderr buffering;
- transactional/idempotent recovery for partial journal appends and
  post-journal evidence or watermark persistence failures;
- evidence-write and watermark-save diagnostic hardening, which must be
  designed with that recovery mechanism rather than triggering paid retries;
- prompt-only output fallback for old Claude versions;
- Codex scratch-file lifecycle hardening, unless implementation shows it is
  necessary for the touched diagnostic path;
- release/version/changelog work.

## Risks and mitigations

1. **Claude envelope mismatch.** `--json-schema` changes the payload field from
   `result` to `structured_output`. Pin that exact shape in unit and end-to-end
   stub tests and retain strict parsing afterward.
2. **Sensitive data in error text.** Use typed errors and allowlisted metadata;
   add sentinel tests proving raw stdout/stderr/model bodies never reach the
   log, CLI output, or Studio HTML.
3. **Partial journal append on an existing fatal path.** The current append can
   leave a partial final line before reporting failure. This patch improves the
   diagnostic but does not claim journal retries are fully idempotent; a replay
   checkpoint/transactional append design is separate work.
4. **Older Claude installations.** Reject or skip them from the bounded version
   probe before writing the spend stamp. Never switch providers after a
   possibly-paid invocation and never silently fall back to prompt-only output.
5. **Error classification still too generic.** Pin the precedence order and
   test valid error envelope + non-zero status, invalid envelope + non-zero
   status, and successful status + missing structured output.
6. **Gemini remains prompt-constrained.** Gemini stays last in the existing
   probe order because its CLI exposes no response-schema flag. Its reliability
   is unchanged; strict validation and the new diagnostics remain the safety
   boundary. Do not claim this patch makes Gemini schema-enforced.
7. **UI test scope.** Studio history is server-rendered with no new interaction.
   Existing route tests cover content and escaping. The implementation handoff
   asks the user for one manual browser check; no Playwright dependency is
   introduced for a static row.

## Validation

Automated tests should cover:

- Claude argv contains `--json-schema` followed by the exact compact schema;
- `.structured_output` is used even if `.result` contains prose;
- unsupported Claude selection, missing provider output, provider errors,
  syntax errors, schema/data mismatches, journal errors, corrupt watermarks, and
  deadline errors receive stable diagnostics;
- usage is retained on failed envelopes when available;
- raw sentinel strings in stdout, stderr, result bodies, and malformed output
  never enter `log.jsonl`, terminal output, or rendered HTML;
- old log lines without diagnostic fields still parse and render;
- malformed/non-string legacy diagnostic fields do not drop their log line;
- manual harvest, status, doctor, and Studio show the safe reason;
- doctor warns for an active failed learner, stays quiet for disabled learning,
  and clears the warning only after an extracted success or `load learn on`
  resets the consecutive-failure counter;
- a failure leaves watermarks/hints intact and increments the breaker only while
  the fencing token is still held;
- a later success advances state and clears the breaker;
- fail, then log `empty`, and prove status/doctor still show the unresolved
  failure while the consecutive-failure count is nonzero;
- reclaim a stale lock, continue to `empty`, and prove the new reclamation
  diagnostic—not an older historical failure—is surfaced;
- Studio escapes adversarial legacy `error` text.

The adapter privacy tests also update
`invoke_codex_empty_message_file_is_a_failure` and
`invoke_gemini_missing_response_is_a_failure`: they assert stable loadout-owned
codes/messages and prove raw stderr/provider error bodies are absent.

The end-to-end coverage must include both Claude stub suites:
`tests/learn.rs` and
`tests/cli.rs::harvest_full_cycle_with_stub_claude_writes_inbox_and_log`. Each
stub returns `structured_output`, includes misleading prose in `result`, and
asserts the exact `--json-schema` argument made it through production selection
and worker orchestration.

Run the repository gates:

```text
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build
```

Use the existing stub-based integration suites. The separately approved live
compatibility smoke is the pre-implementation contract gate, not part of the
automated suite; make no other paid Claude call during validation.

Because the Studio delta is server-rendered and the repository has no
Studio-specific browser harness, ask the user to validate the Harvest history
row manually. Do not add a Playwright dependency for this static rendering
change; the existing `tests/browser_smoke.rs` covers the separate plan viewer,
not Studio.

## Rollback

Revert the implementation commits. New run-log keys are additive and optional,
so no state migration or log rewrite is required. Existing failure records stay
readable. Do not reset learning watermarks during rollback.

## First implementation step

After explicit approval for a small metered compatibility check, run the live
Claude smoke described in Preferred design §1 and record only the contract facts
in this design artifact. Then, in the existing `fix/learn-fenced-output`
worktree, update the failing tests
`invoke_claude_flags_env_stdin_and_result_unwrap` and
`invoke_claude_is_error_true_is_a_failure` against the observed contract. Assert
the exact `--json-schema` argument, return a Claude envelope containing
`structured_output`, and prove `.result` prose is ignored. Implement only the
Claude adapter change and get that focused test group green before adding
diagnostics.
