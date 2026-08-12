# Extension CLI-Update Prompt Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Target repo:** `loadout-tools/vscode` — local checkout `~/_git/loadout-vscode`. **Not** rosita. The plan lives here beside its spec; every path below is relative to the extension repo.

**Goal:** When `load` came from `PATH` and is older than the version this extension bundles, offer once to run `load update` in a terminal.

**Architecture:** A pure decision module with no `vscode` import (`extension.ts` cannot be unit tested, so the logic lives outside it — the same reasoning that produced `src/platform.ts`). Activation runs `load --version`, reads the global config, asks the module, and owns the notification and terminal. Every failure is silent.

**Spec:** `docs/superpowers/specs/2026-08-12-extension-cli-update-prompt-design.md` in rosita.

**Tech Stack:** TypeScript, esbuild, vitest (unit; `vscode` aliased to a hand-written mock), pnpm, `smol-toml`.

## Global Constraints

- **pnpm only** — never `npm` or `yarn`.
- Unit tests must not require a real VS Code host. Anything needing the `vscode` API goes through `test/mocks/vscode.ts`, which is extended as units need it.
- **Silence is the safe default.** Every unclear or failing case resolves to showing nothing. This feature must never block activation or produce an error notification.
- `src/cliUpdate.ts` must not import `vscode`.
- Do not change `src/binary.ts`, `src/studio.ts`, or `src/platform.ts`.
- Before any task is done: `pnpm lint` (runs `tsc --noEmit` + eslint), `pnpm test`, `pnpm build`.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/cliUpdate.ts` **(create)** | Version parsing, opt-out reading, and the offer/quiet decision. Pure except one documented file read. |
| `test/cliUpdate.test.ts` **(create)** | Table-driven cases for all three |
| `src/extension.ts` **(modify)** | Probe the version on activation, show the notification, open the terminal |
| `test/mocks/vscode.ts` **(modify)** | Add `window.createTerminal` |
| `package.json` **(modify)** | Add the `smol-toml` dependency |

---

### Task 1: The decision module

**Files:**
- Create: `src/cliUpdate.ts`
- Create: `test/cliUpdate.test.ts`
- Modify: `package.json` (dependency only)

**Interfaces:**
- Consumes: `configPath` from `./onboarding` (already exported, `src/onboarding.ts:9`)
- Produces:
  - `export interface Version { major: number; minor: number; patch: number }`
  - `export function parseVersion(text: string): Version | null`
  - `export function isOlder(a: Version, b: Version): boolean`
  - `export function updateCheckDisabled(env: NodeJS.ProcessEnv, configText: string | null): boolean`
  - `export function readGlobalConfigText(env?: NodeJS.ProcessEnv): string | null`
  - `export type UpdateDecision = { kind: 'offer'; installed: string; expected: string } | { kind: 'quiet' }`
  - `export function cliUpdateDecision(input: CliUpdateInput): UpdateDecision`
  - `export interface CliUpdateInput { source: 'path' | 'bundled'; versionOutput: string; expected: string; env: NodeJS.ProcessEnv; configText: string | null }`

- [ ] **Step 1: Add the dependency**

Run: `cd ~/_git/loadout-vscode && pnpm add smol-toml`

This adds a `dependencies` block to `package.json` (the repo has only `devDependencies` today) and updates `pnpm-lock.yaml`. Both must be committed — CI runs `pnpm install --frozen-lockfile` and fails on a stale lockfile.

esbuild inlines it into `dist/extension.js`, so `vsce package --no-dependencies` still ships one file.

- [ ] **Step 2: Write the failing test**

Create `test/cliUpdate.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { parseVersion, isOlder, updateCheckDisabled, cliUpdateDecision } from '../src/cliUpdate';

describe('parseVersion', () => {
  it('reads the version out of `load --version` output', () => {
    expect(parseVersion('load 0.26.0')).toEqual({ major: 0, minor: 26, patch: 0 });
    expect(parseVersion('  load 1.2.3\n')).toEqual({ major: 1, minor: 2, patch: 3 });
  });

  it('is null for anything that is not exactly three numbers', () => {
    // A prerelease, a git build, or an unexpected suffix must read as unknown —
    // guessing wrong here means nagging someone whose install is fine.
    for (const s of ['load 0.26.0-rc1', 'load 0.26', 'load v0.26.0 (abc123)', 'not a version', '']) {
      expect(parseVersion(s)).toBeNull();
    }
  });
});

describe('isOlder', () => {
  it('compares major, then minor, then patch', () => {
    expect(isOlder({ major: 0, minor: 25, patch: 0 }, { major: 0, minor: 26, patch: 0 })).toBe(true);
    expect(isOlder({ major: 0, minor: 26, patch: 0 }, { major: 0, minor: 26, patch: 1 })).toBe(true);
    expect(isOlder({ major: 1, minor: 0, patch: 0 }, { major: 0, minor: 99, patch: 9 })).toBe(false);
    expect(isOlder({ major: 0, minor: 26, patch: 0 }, { major: 0, minor: 26, patch: 0 })).toBe(false);
  });
});

describe('updateCheckDisabled', () => {
  it('honors the env kill switch, whatever its value', () => {
    expect(updateCheckDisabled({ LOADOUT_NO_UPDATE_CHECK: '1' }, null)).toBe(true);
    expect(updateCheckDisabled({ LOADOUT_NO_UPDATE_CHECK: '' }, null)).toBe(true);
  });

  it('honors [update] check = off (and the never alias)', () => {
    expect(updateCheckDisabled({}, '[update]\ncheck = "off"\n')).toBe(true);
    expect(updateCheckDisabled({}, '[update]\ncheck = "never"\n')).toBe(true);
    expect(updateCheckDisabled({}, '[update]\ncheck = "OFF"\n')).toBe(true);
  });

  it('reads the dotted spelling too', () => {
    expect(updateCheckDisabled({}, 'update.check = "off"\n')).toBe(true);
  });

  it('is not disabled for any other setting, or no config at all', () => {
    expect(updateCheckDisabled({}, '[update]\ncheck = "always"\n')).toBe(false);
    expect(updateCheckDisabled({}, '[update]\ncheck = "daily"\n')).toBe(false);
    expect(updateCheckDisabled({}, '')).toBe(false);
    expect(updateCheckDisabled({}, null)).toBe(false);
  });

  it('fails open on malformed TOML — an unreadable config is not an opt-out', () => {
    expect(updateCheckDisabled({}, 'this is [not valid toml')).toBe(false);
  });
});

describe('cliUpdateDecision', () => {
  const base = { source: 'path' as const, versionOutput: 'load 0.25.0', expected: '0.26.0', env: {}, configText: null };

  it('offers when the installed CLI is older', () => {
    expect(cliUpdateDecision(base)).toEqual({ kind: 'offer', installed: '0.25.0', expected: '0.26.0' });
  });

  it('is quiet when the installed CLI is equal or newer', () => {
    expect(cliUpdateDecision({ ...base, versionOutput: 'load 0.26.0' })).toEqual({ kind: 'quiet' });
    expect(cliUpdateDecision({ ...base, versionOutput: 'load 0.27.0' })).toEqual({ kind: 'quiet' });
  });

  it('is quiet for the bundled binary — the extension owns that one', () => {
    expect(cliUpdateDecision({ ...base, source: 'bundled' })).toEqual({ kind: 'quiet' });
  });

  it('is quiet when either version will not parse', () => {
    expect(cliUpdateDecision({ ...base, versionOutput: 'load 0.25.0-rc1' })).toEqual({ kind: 'quiet' });
    expect(cliUpdateDecision({ ...base, expected: 'nonsense' })).toEqual({ kind: 'quiet' });
  });

  it('is quiet when the user opted out', () => {
    expect(cliUpdateDecision({ ...base, env: { LOADOUT_NO_UPDATE_CHECK: '1' } })).toEqual({ kind: 'quiet' });
    expect(cliUpdateDecision({ ...base, configText: '[update]\ncheck = "off"\n' })).toEqual({ kind: 'quiet' });
  });
});
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cd ~/_git/loadout-vscode && pnpm vitest run test/cliUpdate.test.ts`
Expected: FAIL — `Failed to resolve import "../src/cliUpdate"`.

- [ ] **Step 4: Write the implementation**

Create `src/cliUpdate.ts`:

```ts
import * as fs from 'node:fs';
import { parse as parseToml } from 'smol-toml';
import { configPath } from './onboarding';

/**
 * Whether to offer updating the user's own `load`.
 *
 * The extension never uses its bundled binary when one is on PATH, so a CLI fix
 * can ship and never reach the user — studio's whole UI is served by that
 * binary. This tells them, once, when their `load` is older than the version
 * this extension was built against.
 *
 * Free of `vscode` on purpose: `extension.ts` cannot be unit tested, so the
 * decision lives out here where it can be.
 */

export interface Version {
  major: number;
  minor: number;
  patch: number;
}

/** `load --version` prints `load X.Y.Z`. Anything else reads as unknown. */
export function parseVersion(text: string): Version | null {
  const m = /^(?:load\s+)?(\d+)\.(\d+)\.(\d+)$/.exec(text.trim());
  if (!m) return null;
  return { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]) };
}

export function isOlder(a: Version, b: Version): boolean {
  if (a.major !== b.major) return a.major < b.major;
  if (a.minor !== b.minor) return a.minor < b.minor;
  return a.patch < b.patch;
}

/**
 * The CLI's own opt-outs, honored here because someone who turned off update
 * nudges meant it whichever surface asks: `LOADOUT_NO_UPDATE_CHECK` (any value,
 * matching the CLI's presence check) and `[update] check = "off"`.
 *
 * Fails open — a config we cannot read is not an opt-out.
 */
export function updateCheckDisabled(env: NodeJS.ProcessEnv, configText: string | null): boolean {
  if (env.LOADOUT_NO_UPDATE_CHECK !== undefined) return true;
  if (!configText) return false;
  try {
    const doc = parseToml(configText) as { update?: { check?: unknown } };
    const check = doc.update?.check;
    if (typeof check !== 'string') return false;
    const v = check.toLowerCase();
    return v === 'off' || v === 'never';
  } catch {
    return false;
  }
}

/** The only file read in this module. Missing or unreadable is `null`. */
export function readGlobalConfigText(env: NodeJS.ProcessEnv = process.env): string | null {
  try {
    return fs.readFileSync(configPath(env), 'utf8');
  } catch {
    return null;
  }
}

export type UpdateDecision =
  | { kind: 'offer'; installed: string; expected: string }
  | { kind: 'quiet' };

export interface CliUpdateInput {
  /** Where `resolveLoad` found the binary. */
  source: 'path' | 'bundled';
  /** Raw stdout of `load --version`. */
  versionOutput: string;
  /** `package.json`'s `loadout.cliVersion`. */
  expected: string;
  env: NodeJS.ProcessEnv;
  configText: string | null;
}

export function cliUpdateDecision(input: CliUpdateInput): UpdateDecision {
  // The bundled binary is ours; the fix for a stale one is a new extension.
  if (input.source !== 'path') return { kind: 'quiet' };
  if (updateCheckDisabled(input.env, input.configText)) return { kind: 'quiet' };
  const installed = parseVersion(input.versionOutput);
  const expected = parseVersion(input.expected);
  if (!installed || !expected) return { kind: 'quiet' };
  if (!isOlder(installed, expected)) return { kind: 'quiet' };
  return {
    kind: 'offer',
    installed: `${installed.major}.${installed.minor}.${installed.patch}`,
    expected: `${expected.major}.${expected.minor}.${expected.patch}`,
  };
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `pnpm vitest run test/cliUpdate.test.ts`
Expected: PASS.

If the dotted-spelling case (`update.check = "off"`) fails, that is a real finding about `smol-toml`, not a reason to delete the case — report it.

- [ ] **Step 6: Verify green**

Run: `pnpm lint && pnpm test && pnpm build`

- [ ] **Step 7: Commit**

```bash
git add src/cliUpdate.ts test/cliUpdate.test.ts package.json pnpm-lock.yaml
git commit -m "feat: decide when to offer a CLI update

The extension never uses its bundled load when one is on PATH, so a CLI
fix can ship and never reach the user. Pure so it is testable outside a
VS Code host; silence is the default for anything unclear."
```

---

### Task 2: Ask on activation

**Files:**
- Modify: `src/extension.ts`
- Modify: `test/mocks/vscode.ts`

**Interfaces:**
- Consumes: `cliUpdateDecision`, `readGlobalConfigText` from Task 1; `runLoad` from `./exec` (`src/exec.ts:50`, signature `runLoad(bin, args, cwd, storageDir, extraEnv?) => Promise<{code, stdout, stderr}>`)
- Produces: no new exports

- [ ] **Step 1: Extend the vscode mock**

`test/scaffold.test.ts` calls the real `activate()`, so the mock must carry every member activation touches or the suite throws. Add to `test/mocks/vscode.ts`, inside the existing `window` object:

```ts
  terminals: [] as { name: string; sent: string[] }[],
  createTerminal: (name: string) => {
    const t = { name, sent: [] as string[], show: () => {}, dispose: () => {}, sendText: (s: string) => { t.sent.push(s); } };
    window.terminals.push(t);
    return t;
  },
```

- [ ] **Step 2: Wire it into activation**

Add the import beside the existing ones in `src/extension.ts`:

```ts
import { cliUpdateDecision, readGlobalConfigText } from './cliUpdate';
```

Add the state key beside `UNSUPPORTED_NOTICE_KEY`:

```ts
const CLI_UPDATE_NOTICE_KEY = 'loadout.cliUpdateNoticeShown';
```

Add this helper inside `activate`, after `bin` is defined:

```ts
  /**
   * A CLI on PATH is never replaced by this extension, so a user can install an
   * extension update and still be served an old studio by their own `load`.
   * Tell them once per expected version. Everything here is best-effort: any
   * failure means saying nothing, and activation never waits on it.
   */
  const maybeOfferCliUpdate = async () => {
    if (!bin) return;
    const expected = (context.extension?.packageJSON as { loadout?: { cliVersion?: string } } | undefined)
      ?.loadout?.cliVersion;
    if (!expected) return;
    const key = `${CLI_UPDATE_NOTICE_KEY}.${expected}`;
    if (context.globalState.get(key) === true) return;

    const probe = await runLoad(bin.path, ['--version'], undefined, storage);
    if (probe.code !== 0) return;
    const decision = cliUpdateDecision({
      source: bin.source,
      versionOutput: probe.stdout,
      expected,
      env: process.env,
      configText: readGlobalConfigText(),
    });
    if (decision.kind !== 'offer') return;

    // Mark before asking: the offer fires once per expected version whether the
    // user updates, declines, or ignores it.
    await context.globalState.update(key, true);
    const choice = await vscode.window.showInformationMessage(
      `Loadout expects load ${decision.expected}, but ${decision.installed} is installed. Studio's interface comes from the CLI, so some fixes arrive only when it updates.`,
      'Update',
      'Not now'
    );
    if (choice !== 'Update') return;
    const term = vscode.window.createTerminal('Loadout update');
    term.show();
    term.sendText('load update');
  };
```

Add `runLoad` to the existing `./exec` import.

Then fire it near the end of `activate`, after the `if (!bin)` block and before the consent branch, without awaiting:

```ts
  void maybeOfferCliUpdate().catch((e) => out.appendLine(`cli update check: ${String(e)}`));
```

`context.extension` is optional in the fake context `scaffold.test.ts` builds, so the guard above makes the whole check a no-op there — no subprocess is spawned and no notification fires during unit tests.

- [ ] **Step 3: Verify green**

Run: `pnpm lint && pnpm test && pnpm build`
Expected: all pass, 51 existing tests plus Task 1's still green. `pnpm lint` runs `tsc --noEmit`, which is what catches a mock missing a member `extension.ts` now uses.

- [ ] **Step 4: Manual verification**

Build and install the VSIX locally, with a `load` on PATH older than `package.json`'s `loadout.cliVersion`:

1. Open a window. Confirm the notification names both versions.
2. Click **Update**. Confirm a terminal named "Loadout update" opens and runs `load update`.
3. Reload the window. Confirm the notification does **not** fire again.
4. Set `LOADOUT_NO_UPDATE_CHECK=1` in the environment VS Code launches with, clear the `globalState` key (or bump `cliVersion` in the installed VSIX), and confirm nothing appears.

- [ ] **Step 5: Commit**

```bash
git add src/extension.ts test/mocks/vscode.ts
git commit -m "feat: offer to update a stale load CLI on activation

Studio's interface is served by the CLI binary, so a user whose own load
is behind gets none of its fixes and nothing says why. Once per expected
version, and never for the bundled binary."
```

---

## Manual verification

Covered in Task 2 Step 4 — it needs an installed VSIX and a deliberately stale CLI, so it cannot be automated in this repo.

## Out of scope

- Checking the network for the latest release. The CLI already does that, throttled, in `rosita src/update.rs`.
- Verifying that `load update` succeeded. The terminal output is the user's feedback.
- Honoring `LOADOUT_CONFIG_DIR` when locating the config. `configPath` in `src/onboarding.ts` does not read it today, so a user who sets it gets no opt-out detection and therefore sees the prompt. Failing open is the intended direction, and changing `configPath` would affect `hasConfig` and consent detection — a separate change.
