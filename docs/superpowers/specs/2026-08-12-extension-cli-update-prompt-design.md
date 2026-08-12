# The extension tells you when your `load` is behind what it expects

**Target repo:** `loadout-tools/vscode` (local `~/_git/loadout-vscode`). The spec
lives here so it sits beside its siblings; every path below is relative to the
extension repo unless it says otherwise.

## The gap

The extension bundles a `load` binary but never uses it when one is on `PATH` —
`resolveLoad` (`src/binary.ts:9`) walks `PATH` first and only falls back to the
bundled copy. That is deliberate: the extension must not overwrite a binary the
user manages.

The consequence is that an extension update can carry fixes a user never
receives. Studio's entire UI is served by the CLI binary, so a studio fix ships
in the CLI and nowhere else. Someone whose `PATH` `load` is two releases old sees
none of it, and nothing tells them why.

This is not hypothetical. It happened to loadout's own author, on his own
machine, while building the fix — he installed extension 0.2.1, still saw the bug
0.26.0 had fixed, and had no way to know his CLI was the reason.

The CLI already checks for updates: `[update] check = always|daily|off`, a
throttled network check, an `LOADOUT_NO_UPDATE_CHECK` kill switch, and
`load update` for self-installs (rosita `src/update.rs`). But the nudge prints to
a terminal — it is gated on `IsTerminal` — so someone who lives in the IDE and
opens studio from the sidebar never sees it. The surface is wrong, not the logic.

## What we are building

On activation, when `load` came from `PATH` and its version is older than the one
this extension bundles, offer to update it. Accepting opens a VS Code terminal
and runs `load update` there.

That is the whole feature. It does not check the network, does not update
anything itself, and never touches a binary silently.

## Decisions

**Compare against the bundled pin, not the latest release.** `package.json`'s
`loadout.cliVersion` is the version this extension was built and tested against,
so the claim is "this extension expects 0.26.0 and you have 0.25.0" — a
compatibility statement the extension can make honestly and offline. Comparing
against the newest GitHub release would mean a network call on activation and a
prompt naming a version the extension has never seen.

**Only when the binary came from `PATH`.** When `resolveLoad` returns
`source: 'bundled'`, the extension owns that binary and the fix for a stale one is
to update the extension. Prompting there would be nonsense.

**Run `load update` in a visible terminal.** The alternative — running it in the
background and reporting — reads tidier but forces the extension to translate
failures it cannot fully diagnose. `load update` works off the cargo-dist install
receipt and refuses to act when there is none (`Outcome::NotManaged`), which is
correct: a Homebrew- or `cargo install`-provided binary belongs to that tool, and
overwriting it would leave the real owner lying about what is installed. In a
terminal, that refusal explains itself in the CLI's own words.

Note that "installed separately from the extension" and "self-updatable" are
different sets. A `curl | bash` install has a receipt and updates cleanly; a
`cargo install` or package-manager install does not. The extension cannot tell
these apart before running the command, and should not try.

**Do not verify afterwards.** The user watched it run. A follow-up notification
would either repeat what they just read, or interrupt them about a subtlety —
a shadowed binary earlier on `PATH`, say — that the terminal output already hints
at.

**Prompt once per expected version.** Keyed in `globalState` by the version being
asked for, following `UNSUPPORTED_NOTICE_KEY` in `src/extension.ts`. "Not now"
holds until the extension bumps its own pin, at which point the claim is genuinely
new and worth making again.

**Honour the CLI's opt-outs — both of them.** `LOADOUT_NO_UPDATE_CHECK` (any
value) and `[update] check = "off"` in the global config. Someone who turned off
update nudges meant it, whichever surface is asking.

## Architecture

| File | Responsibility |
|---|---|
| `src/cliUpdate.ts` **(create)** | Pure decision + version parsing + opt-out reading. No `vscode` import. |
| `test/cliUpdate.test.ts` **(create)** | Table-driven cases |
| `src/extension.ts` **(modify)** | Call it on activation; own the notification and the terminal |
| `package.json` **(modify)** | Add `smol-toml` |

`src/extension.ts` is the one file unit tests cannot exercise, because it
activates the whole extension. Keeping the decision outside it is what makes any
of this testable — the same reasoning that produced `src/platform.ts`.

### The decision function

Takes the installed version string, the expected version, the binary source, and
the two opt-out inputs. Returns whether to prompt, and the two versions to name
if so. Every branch below is a test case:

- installed older than expected → prompt
- installed equal or newer → silent
- source is `bundled` → silent
- either version unparseable → silent
- `LOADOUT_NO_UPDATE_CHECK` set to anything → silent
- `[update] check = "off"` → silent

**Silence is the safe default.** A prompt the user cannot act on is worse than no
prompt, so anything unclear resolves to saying nothing.

### Version comparison

Parse `load X.Y.Z` from `load --version` (run through the existing `runLoad`
helper in `src/exec.ts`). Compare numerically by major, minor, patch. Anything
that does not match that shape — a git build, a `-rc` suffix, empty output — is
unparseable and therefore silent. Do not add a semver dependency for three
integers.

### Reading the opt-out

Parse the global config with `smol-toml` and read `update.check`. Chosen over a
hand-rolled scan because TOML has several legal spellings of the same key and a
regex would silently miss some; over `@iarna/toml` because it is maintained and
TOML 1.0 compliant; and over adding a CLI query surface because that would mean
shipping a rosita change to make an extension feature work.

esbuild inlines it into `dist/extension.js`, so `vsce package --no-dependencies`
still ships one file and the VSIX gains no `node_modules`.

The config path is the same one `hasConfig()` in `src/onboarding.ts` already
resolves. A missing or malformed config means no opt-out — not an error, and not
a reason to stay silent.

## Error handling

Every failure is silent and non-blocking: `load --version` failing to run,
timing out, or printing something unexpected; the config being unreadable; the
terminal failing to open. None of it blocks activation, and none of it produces
an error notification. This feature is a courtesy; it must never be the reason
something else did not work.

The version check runs asynchronously. Activation does not wait for it.

## Testing

`test/cliUpdate.test.ts` covers the decision table above, version parsing
(including the unparseable shapes), and opt-out reading from a TOML fixture
(present and `"off"`, present and `"always"`, absent, malformed).

The notification and terminal wiring live in `src/extension.ts` and stay
uncovered, consistent with the rest of that file. That is a known limitation of
this codebase, not a new one — and it is why the decision lives outside.

## Risks

**The prompt is wrong when a user pins their CLI deliberately.** Someone holding
an older `load` on purpose gets one notification per extension version bump.
Mitigated by the once-per-version key and by honouring both opt-outs; not
eliminated.

**`load update` may update a binary that is not the one on `PATH`.** The receipt
names an install prefix, which can differ from whatever is first on `PATH`. We
chose not to verify, so this surfaces as the user running the update and seeing
no change. The terminal output is the only clue. Accepted, and worth revisiting
if it turns out to bite anyone.

## Out of scope

- Checking the network for the latest release. The CLI already does that.
- Updating the CLI without asking, or updating the bundled binary.
- Any change to rosita. This is entirely an extension feature.
- Verifying the outcome of `load update`.
