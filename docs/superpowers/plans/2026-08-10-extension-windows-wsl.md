# Extension Windows/WSL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Target repo:** `loadout-tools/vscode` — local checkout `~/_git/loadout-vscode`. **Not** rosita. The plan lives here so it sits beside its spec; every path below is relative to the extension repo.

**Goal:** Replace the "unix only today" dead end Windows users hit with a guided path into WSL, where the extension and the bundled Linux `load` already work.

**Architecture:** Nothing is ported. A workspace extension already runs *inside* WSL in a WSL remote window, and the `linux-x64` VSIX bundles a working `load`. The work is a pure decision function that classifies the situation, an `extension.ts` branch that acts on it, one manifest change that guarantees the extension lands in the remote, and a genuine remote-URL bug fix in `studio.ts`.

**Tech Stack:** TypeScript, esbuild, vitest (unit, `vscode` aliased to a hand-written mock), `@vscode/test-electron` (integration), pnpm, GitHub Actions.

## Global Constraints

- **pnpm only** — never `npm` or `yarn`.
- Unit tests must not require a real VS Code host. Anything needing the `vscode` API goes through `test/mocks/vscode.ts`, which is extended as units need it (its own header says so).
- **Marketplace copy, exact string:** `Requires macOS, Linux, or Windows with WSL2.`
- Do not change `src/binary.ts` — it already handles `load.exe` and is not on the WSL path.
- Before any task is done: `pnpm lint` (runs `tsc --noEmit` + eslint), `pnpm test`, `pnpm build`.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/platform.ts` **(create)** | One pure function classifying (has binary?, platform, remote, WSL ext present) into an action. No `vscode` import, so it is trivially testable. |
| `test/platform.test.ts` **(create)** | Table-driven cases for that function |
| `test/mocks/vscode.ts` **(modify)** | Add `env.remoteName`, `env.asExternalUri`, `Uri.parse`, `extensions.getExtension` |
| `src/extension.ts` **(modify)** | Replace the `showUnsupported()` dead end with the decision branch |
| `src/studio.ts` **(modify)** | Route the studio URL through `asExternalUri` |
| `package.json` **(modify)** | `extensionKind`, description copy |
| `.github/workflows/ci.yml` **(modify)** | `windows-latest` matrix leg |

Why a separate `platform.ts` rather than a helper inside `extension.ts`: `extension.ts` is the one file unit tests cannot exercise (it activates the whole extension). Keeping the decision outside it is what makes this testable at all.

---

### Task 1: The platform decision function

**Files:**
- Create: `src/platform.ts`
- Create: `test/platform.test.ts`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: nothing (no imports)
- Produces:
  - `export type PlatformAction = { kind: 'ok' } | { kind: 'offer-wsl'; needsWslExtension: boolean } | { kind: 'unsupported' }`
  - `export interface PlatformInput { hasBinary: boolean; platform: string; remoteName: string | undefined; wslExtensionInstalled: boolean }`
  - `export function platformAction(input: PlatformInput): PlatformAction`
  - `export const WSL_EXTENSION_ID = 'ms-vscode-remote.remote-wsl'`
  - `export const WSL_REOPEN_COMMAND = 'remote-wsl.reopenInWSL'`

- [ ] **Step 1: Write the failing test**

Create `test/platform.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { platformAction } from '../src/platform';

const base = { hasBinary: false, platform: 'linux', remoteName: undefined, wslExtensionInstalled: false };

describe('platformAction', () => {
  it('is ok whenever the binary resolved, on any platform', () => {
    for (const platform of ['darwin', 'linux', 'win32']) {
      expect(platformAction({ ...base, hasBinary: true, platform })).toEqual({ kind: 'ok' });
    }
  });

  it('offers WSL on a local Windows window with no binary', () => {
    expect(platformAction({ ...base, platform: 'win32' })).toEqual({
      kind: 'offer-wsl',
      needsWslExtension: true,
    });
  });

  it('does not ask to install the WSL extension when it is already there', () => {
    expect(
      platformAction({ ...base, platform: 'win32', wslExtensionInstalled: true })
    ).toEqual({ kind: 'offer-wsl', needsWslExtension: false });
  });

  it('is unsupported inside a WSL remote with no binary — reopening would not help', () => {
    // Already in the remote and still no `load` means the linux VSIX failed to
    // install, which "Reopen in WSL" cannot fix.
    expect(platformAction({ ...base, platform: 'win32', remoteName: 'wsl' })).toEqual({
      kind: 'unsupported',
    });
  });

  it('is unsupported on a non-Windows platform with no binary', () => {
    expect(platformAction({ ...base, platform: 'darwin' })).toEqual({ kind: 'unsupported' });
    expect(platformAction({ ...base, platform: 'linux' })).toEqual({ kind: 'unsupported' });
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd ~/_git/loadout-vscode && pnpm vitest run test/platform.test.ts`
Expected: FAIL — `Failed to resolve import "../src/platform"`.

- [ ] **Step 3: Write the implementation**

Create `src/platform.ts`:

```ts
/**
 * What the extension should do when it cannot find a `load` binary.
 *
 * Windows support is WSL support: a workspace extension runs *inside* the WSL
 * remote, where the bundled linux-x64 `load` works unmodified. So a Windows
 * user with no binary is not unsupported — they are one "Reopen in WSL" away.
 *
 * Pure and vscode-free on purpose: `extension.ts` cannot be unit tested, so the
 * decision lives out here where it can be.
 */

export const WSL_EXTENSION_ID = 'ms-vscode-remote.remote-wsl';
export const WSL_REOPEN_COMMAND = 'remote-wsl.reopenInWSL';

export type PlatformAction =
  | { kind: 'ok' }
  | { kind: 'offer-wsl'; needsWslExtension: boolean }
  | { kind: 'unsupported' };

export interface PlatformInput {
  /** `resolveLoad` found a binary. */
  hasBinary: boolean;
  /** `process.platform`. */
  platform: string;
  /** `vscode.env.remoteName` — undefined in a local window, 'wsl' in a WSL remote. */
  remoteName: string | undefined;
  wslExtensionInstalled: boolean;
}

export function platformAction(input: PlatformInput): PlatformAction {
  if (input.hasBinary) return { kind: 'ok' };
  // Only a *local* Windows window can be moved into WSL. Inside the remote
  // already, a missing binary is a packaging failure, not a placement problem.
  if (input.platform === 'win32' && input.remoteName === undefined) {
    return { kind: 'offer-wsl', needsWslExtension: !input.wslExtensionInstalled };
  }
  return { kind: 'unsupported' };
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `pnpm vitest run test/platform.test.ts`
Expected: PASS, 5 tests.

- [ ] **Step 5: Add the Windows CI leg**

In `.github/workflows/ci.yml`, change the matrix line:

```yaml
        matrix: { os: [ubuntu-latest, macos-latest, windows-latest] }
```

The two integration steps are already guarded by `if: runner.os == 'Linux'` and `if: runner.os == 'macOS'`, so Windows runs lint, unit tests, and build only. That is deliberate — `@vscode/test-electron` on Windows adds a slow, flaky download for no coverage this plan needs.

- [ ] **Step 6: Verify green**

Run: `pnpm lint && pnpm test && pnpm build`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/platform.ts test/platform.test.ts .github/workflows/ci.yml
git commit -m "feat: platform decision for Windows/WSL

A Windows user with no load binary is one Reopen-in-WSL away, not
unsupported. Pure function so it is testable outside a VS Code host."
```

---

### Task 2: Wire the decision into activation

**Files:**
- Modify: `src/extension.ts`
- Modify: `test/mocks/vscode.ts`
- Modify: `package.json`

**Interfaces:**
- Consumes: `platformAction`, `WSL_EXTENSION_ID`, `WSL_REOPEN_COMMAND` from Task 1
- Produces: no new exports; changes behaviour of the four `if (!bin)` guards in `extension.ts`

- [ ] **Step 1: Extend the vscode mock**

The mock's header says to extend it as units need it. Add to `test/mocks/vscode.ts`:

```ts
export const extensions = {
  installed: [] as string[],
  getExtension: (id: string) => (extensions.installed.includes(id) ? { id } : undefined),
};
```

Add `remoteName` and `asExternalUri` to the existing `env` export, replacing that line:

```ts
export const env = {
  appName: 'Visual Studio Code',
  remoteName: undefined as string | undefined,
  openExternal: async (_u: unknown) => true,
  /** Identity by default; tests that care about port forwarding override it. */
  asExternalUri: async (u: unknown) => u,
};
```

Add `parse` to the existing `Uri` class, keeping `file`:

```ts
export class Uri {
  static file(p: string) { return { fsPath: p, toString: () => p }; }
  static parse(s: string) { return { fsPath: s, toString: () => s }; }
}
```

- [ ] **Step 2: Replace the dead end in `extension.ts`**

Add the import beside the existing ones:

```ts
import { platformAction, WSL_EXTENSION_ID, WSL_REOPEN_COMMAND } from './platform';
```

Replace the existing `showUnsupported` definition:

```ts
  const showUnsupported = async () => {
    void vscode.window.showInformationMessage('Loadout does not support this platform yet (unix only today).');
  };
```

with:

```ts
  /** What to do about a missing `load`, decided once per activation. */
  const action = platformAction({
    hasBinary: bin !== null,
    platform: process.platform,
    remoteName: vscode.env.remoteName,
    wslExtensionInstalled: vscode.extensions.getExtension(WSL_EXTENSION_ID) !== undefined,
  });

  /**
   * Called from every entry point that needs a binary. On Windows this is not a
   * dead end: the extension and its bundled linux `load` both work once the
   * window is reopened in WSL, so offer exactly that.
   */
  const showUnsupported = async () => {
    if (action.kind !== 'offer-wsl') {
      void vscode.window.showInformationMessage(
        'Loadout does not support this platform yet (macOS and Linux only).'
      );
      return;
    }
    const choice = await vscode.window.showInformationMessage(
      'Loadout runs inside WSL on Windows. Reopen this folder in WSL to set it up.',
      'Reopen in WSL',
      'Not now'
    );
    if (choice !== 'Reopen in WSL') return;
    try {
      if (action.needsWslExtension) {
        await vscode.commands.executeCommand('workbench.extensions.installExtension', WSL_EXTENSION_ID);
      }
      await vscode.commands.executeCommand(WSL_REOPEN_COMMAND);
    } catch (e) {
      out.appendLine(`reopen in wsl: ${String(e)}`);
      void vscode.window.showErrorMessage(
        'Could not reopen in WSL. Install the "WSL" extension, then run "WSL: Reopen Folder in WSL" from the Command Palette.'
      );
    }
  };
```

The four existing `if (!bin) { await showUnsupported(); return; }` guards, and the `if (!bin)` block near the end of `activate`, need no change — they all route through `showUnsupported`.

- [ ] **Step 3: Make the extension a workspace extension**

In `package.json`, add after `"main"`:

```json
  "extensionKind": ["workspace"],
```

This is what guarantees the extension host runs inside WSL rather than on the Windows side. An extension with a `main` already defaults to workspace; stating it means the default can never drift.

- [ ] **Step 4: Update the marketplace copy**

In `package.json`, change `description` — the trailing sentence only:

```json
  "description": "Your personal AI-agent context, equipped automatically in every repo — for Copilot in VS Code and for Cursor. Requires macOS, Linux, or Windows with WSL2.",
```

- [ ] **Step 5: Verify green**

Run: `pnpm lint && pnpm test && pnpm build`
Expected: all pass. `pnpm lint` runs `tsc --noEmit`, which is what catches a mock missing a member `extension.ts` now uses.

- [ ] **Step 6: Commit**

```bash
git add src/extension.ts test/mocks/vscode.ts package.json
git commit -m "feat: offer Reopen in WSL instead of a dead end on Windows

Windows users saw 'unix only today' with nowhere to go. They now get the
one action that works, and extensionKind pins the extension to the
remote so the bundled linux load is what activates."
```

---

### Task 3: Make the studio URL remote-safe

**Files:**
- Modify: `src/extension.ts` (the `showStudio` helper)
- Test: `test/studio.test.ts`

This is a real bug independent of Windows: the raw `127.0.0.1` URL is handed to a webview that runs on the *client*, so it breaks SSH remotes and Codespaces today. WSL happens to survive it because WSL2 forwards localhost by default — luck, not correctness.

**Interfaces:**
- Consumes: `parseStudioUrl` from `src/studio.ts` (unchanged)
- Produces: no new exports

- [ ] **Step 1: Write the failing test**

The URL mapping happens in `extension.ts`'s `showStudio`, which unit tests cannot reach. Extract it into `src/studio.ts` where `parseStudioUrl` already lives, and test it there. Add to `test/studio.test.ts`:

```ts
import { externalStudioUrl } from '../src/studio';

describe('externalStudioUrl', () => {
  it('passes the URL through asExternalUri, preserving path and query', async () => {
    const seen: string[] = [];
    const mapped = await externalStudioUrl('http://127.0.0.1:5309/__studio/bootstrap?token=abc', async (u) => {
      seen.push(u);
      return 'https://forwarded.example/__studio/bootstrap?token=abc';
    });
    expect(seen).toEqual(['http://127.0.0.1:5309/__studio/bootstrap?token=abc']);
    expect(mapped).toBe('https://forwarded.example/__studio/bootstrap?token=abc');
  });

  it('falls back to the original URL when mapping throws', async () => {
    const url = 'http://127.0.0.1:5309/__studio/bootstrap?token=abc';
    const mapped = await externalStudioUrl(url, async () => {
      throw new Error('no remote');
    });
    expect(mapped).toBe(url);
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `pnpm vitest run test/studio.test.ts`
Expected: FAIL — `externalStudioUrl` is not exported.

- [ ] **Step 3: Implement**

Add to `src/studio.ts`:

```ts
/**
 * Map studio's loopback URL to one the *client* can reach.
 *
 * In a remote window (WSL, SSH, Codespaces) the Simple Browser webview runs on
 * the client while `load studio` binds 127.0.0.1 inside the remote, so the raw
 * URL is wrong there. `vscode.env.asExternalUri` sets up the port forward and
 * hands back a reachable URL; locally it is the identity.
 *
 * The bootstrap path and its `token` query MUST survive the mapping — the bare
 * root answers 403 (see `parseStudioUrl`). Any failure falls back to the
 * original URL, which is exactly today's behaviour.
 */
export async function externalStudioUrl(
  url: string,
  asExternalUri: (u: string) => Promise<string>
): Promise<string> {
  try {
    return await asExternalUri(url);
  } catch {
    return url;
  }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `pnpm vitest run test/studio.test.ts`
Expected: PASS.

- [ ] **Step 5: Use it in `extension.ts`**

Add `externalStudioUrl` to the existing `./studio` import, then replace `showStudio`:

```ts
  const showStudio = async (url: string) => {
    const external = await externalStudioUrl(url, async (u) =>
      (await vscode.env.asExternalUri(vscode.Uri.parse(u))).toString()
    );
    try {
      await vscode.commands.executeCommand('simpleBrowser.show', external);
    } catch {
      await vscode.env.openExternal(vscode.Uri.parse(external));
    }
  };
```

- [ ] **Step 6: Verify green**

Run: `pnpm lint && pnpm test && pnpm build`

- [ ] **Step 7: Commit**

```bash
git add src/studio.ts src/extension.ts test/studio.test.ts
git commit -m "fix: map the studio URL through asExternalUri

The raw 127.0.0.1 URL went to a webview running on the client, so studio
was broken in every remote except WSL, which survives only because WSL2
forwards localhost by default."
```

---

## Discovery: Cursor on Windows

**Not a task — it cannot be planned until it is answered.** Cursor cannot ship Microsoft's Remote-WSL extension and provides its own WSL support, so `WSL_EXTENSION_ID` and `WSL_REOPEN_COMMAND` are likely wrong there.

Answer it on proxmox VM 100 (Windows 11 Pro, `192.168.1.210`, RDP enabled, WSL 2.7.11.0 already installed):

1. Install Cursor, open a local Windows folder.
2. Command Palette → search "WSL". Record the exact command id (Developer: Show All Commands, or inspect via `Developer: Generate Command Reference`).
3. Check whether a WSL remote extension is installed by default and what its id is.

Then either extend `platformAction` with a `cursor` branch returning the right ids, or — if Cursor has no equivalent — have `showUnsupported` print Cursor-specific manual instructions. `agentForAppName` in `src/refresh.ts:8` already distinguishes the two hosts, so the branch has a home.

## Manual verification

On VM 100, after Task 3 lands:

1. Open a Windows folder in VS Code. Confirm the notification offers **Reopen in WSL** rather than "unix only today".
2. Accept it. Confirm the window reopens in WSL, the extension activates there, and the status bar leaves the unsupported state.
3. Confirm `load` resolves to the bundled `linux-x64` binary — check the Loadout output channel.
4. Open Studio. Confirm it opens in Simple Browser and the page loads rather than 403ing (that would mean the token query was lost in `asExternalUri`).
5. Complete setup, then clone a config repo from the studio sync card (Workstream B) and confirm skills land in WSL's `~/.claude/skills`.

## Out of scope

- Adding `win32-x64` / `win32-arm64` to the VSIX matrix in `.github/workflows/publish.yml`. Those targets bundle a **Windows** `load`, which does not exist — the native CLI port is a rejected alternative in the spec. A WSL remote installs the `linux-x64` VSIX, which is already built.
