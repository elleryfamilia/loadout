# Windows support via WSL Remote, and sync setup in studio

Date: 2026-08-10
Status: approved, not yet planned

## Objective

Make the VS Code extension work for Windows users, and give studio the ability
to set up and run config sync on its own.

These are two workstreams in two repositories. They ship together because the
Windows path is what exposes the sync gap: a Windows user's first loadout
machine is a fresh WSL2 install, and the first thing they need is to pull down
the config repo they already have on another machine. Today that is only
possible from a terminal.

Explicit non-goal: **a native Windows build of the `load` CLI.** The CLI stays
unix-only and requires WSL2 on Windows. An audit of that port is recorded in
"Rejected alternatives" below so the work is not re-derived later.

## Context

### Repositories

| Repo | Local path | Workstream |
|---|---|---|
| `elleryfamilia/loadout` (rosita) | `~/_git/rosita` | B — studio sync |
| `loadout-tools/vscode` | `~/_git/loadout-vscode` | A — Windows/WSL |

### Why WSL Remote is the whole Windows story

VS Code extensions that declare a `main` entry point default to *workspace*
extensions, so in a WSL window the extension host runs inside WSL as a Linux
process. The extension's VSIX matrix already builds `linux-x64` and
`linux-arm64` with the CLI bundled at `bin/load`, and VS Code installs the
remote's platform VSIX into the remote. A Windows user who opens a folder in
WSL therefore gets the Linux extension with a working Linux `load` — with no
separate CLI install.

Everything downstream follows from that. Skills land in WSL's
`~/.claude/skills`, and the agent extensions (Claude Code, Cursor) are also
workspace extensions running in that same WSL host, so they read that same
home. `load sync` clones into WSL's `~/.config/loadout`. The experience is
simply the Linux experience.

### What the extension does today on Windows

`resolveLoad` finds no binary, so `src/extension.ts` shows a dead end:
"Loadout does not support this platform yet (unix only today)." The marketplace
description says "Requires macOS or Linux."

### What studio does with sync today

Studio already runs sync as a backend operation. `auto_push_after_apply` at
`src/studio/server.rs:1987`, called from line 1963 on every successful apply,
commits and pushes the global config headless and reports the outcome into the
UI flash ("synced ✓", "remote moved ahead — run `load sync`", "saved locally,
push pending").

What is missing is narrower than "sync is not in studio":

1. **No setup.** `auto_push_after_apply` early-returns when
   `sync::is_synced()` is false, so on a fresh machine it is a silent no-op.
   There is no `init` and no `clone` path in the UI.
2. **No pull.** `sync::auto_pull` is called from `src/commands/apply.rs:104`,
   which is the render path used by `refresh` and `run`. Studio never reaches
   it. The `"remote moved ahead — run \`load sync\`"` string at
   `src/studio/server.rs:2001` is studio admitting it cannot pull.
3. **No status.** Nothing in the UI shows whether sync is configured, what the
   remote is, or when it last ran.

`src/sync.rs` already exposes everything a UI needs: `is_synced`,
`last_synced`, `remote_name`, `pull`, `commit_push`, `init`, `clone`,
`gh_available`, `gh_create_repo`, `gh_repo_url`, `wire_remote_and_push`.
Studio is in the same crate and calls these directly. No new CLI plumbing and
no new sync logic are required.

## Approach

### Workstream A — Windows via WSL Remote (`loadout-tools/vscode`)

**A1. Manifest.** Add `"extensionKind": ["workspace"]` to `package.json`
explicitly, so the extension can never be pinned to the Windows UI host. Change
the `description` from "Requires macOS or Linux." to "Requires macOS, Linux, or
Windows with WSL2."

**A2. Replace the dead end.** In `src/extension.ts`, `showUnsupported()`
currently fires whenever `resolveLoad` returns null. Branch on platform:

- `process.platform === 'win32'` and `vscode.env.remoteName === undefined` →
  offer **Reopen in WSL**. Check for `ms-vscode-remote.remote-wsl`, offer to
  install it if absent, then invoke its reopen command.
- Anything else → the existing message.

Factor the decision into a pure function (input: platform, `remoteName`,
whether the WSL extension is present; output: a tagged action) so it is unit
testable without a VS Code host.

**A3. Remote-safe studio URL.** `src/studio.ts` hands a raw
`http://127.0.0.1:PORT/...` URL to `simpleBrowser.show`. In a remote window the
webview runs on the client side, so the URL must go through
`vscode.env.asExternalUri()` first. This is a latent bug beyond Windows: it
breaks SSH remotes and Codespaces today. WSL2 enables localhost forwarding by
default, so WSL may work without this — that is luck, not correctness.

**A4. Cursor on Windows.** Cursor cannot ship Microsoft's Remote-WSL extension
and provides its own WSL support. Whether the reopen command exists there, and
under what id, is unknown and must be determined on a real Windows machine. The
extension already branches on `agentForAppName`, so there is a place to put the
difference. Treat this as a discovery task, not an implementation task.

**A5. CI.** Add `windows-latest` to the extension's test matrix so the
platform-decision function from A2 is exercised on the platform it describes.

### Workstream B — Sync setup and status in studio (rosita)

A sync card in studio, following the existing skills pattern (`/skills/card`,
`/skills/install`, `/skills/<id>/<action>` at `src/studio/server.rs:163-193`).

| Action | Wraps | Shown when |
|---|---|---|
| Status | `is_synced`, `remote_name`, `last_synced` | always |
| Set up sync | `init`, plus `gh_available` + `gh_create_repo` + `wire_remote_and_push` | not synced |
| Clone existing config | `clone` | not synced and config dir is empty |
| Sync now | `pull` then `commit_push` | synced |

The `gh`-assisted setup path mirrors what `src/commands/sync.rs:85-142` already
does for the CLI.

"Sync now" also retires the dead-end string at `src/studio/server.rs:2001`:
once studio can pull, a divergence is resolvable in place rather than punted to
a terminal.

### Credentials

Sync runs entirely in studio's backend. There is no terminal handoff, no
credential entry form, and no new surface.

`sync::clone` and `sync::init` shell out to git, and git resolves credentials
through its configured helper. On a machine that has pushed before, a helper
exists and these are non-interactive — which is why `commit_push` already works
headless in studio today. The one case that fails is a genuinely fresh machine
cloning a **private** repo over HTTPS with no helper configured: git cannot
prompt from a headless child, so the clone fails on the timeout.

That is an error path, handled with an error message. When the failure looks
like authentication and `gh_available()` is true, name `gh auth login` as the
fix, since it installs a git credential helper and resolves the case in one
command. Public repos and SSH URLs with a loaded key are unaffected.

Deliberately rejected: collecting a token in a studio form and writing it into
git's remote config. That cuts against loadout's secret-redaction posture.

## Assumptions

1. Extensions with a `main` entry point default to `extensionKind:
   ["workspace"]`. A1 makes this explicit regardless, so the design does not
   depend on the default being what we believe.
2. VS Code installs the remote's platform VSIX into a WSL remote, so the
   bundled `linux-x64` binary is what activates there.
3. A Windows user willing to use loadout is willing to open their project in
   WSL. Users who keep code on `C:\` and never touch WSL are not served by this
   design; they get a signpost instead of a dead end.
4. `gh` being installed and authenticated is the common path for private config
   repos, making headless clone work without further machinery.

## Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Cursor has no Remote-WSL equivalent, or a differently-named command | Windows Cursor users get no guided path | A4 discovery task on the VM before committing to the UX; worst case the message names the manual steps |
| Fresh-machine private HTTPS clone fails on credentials | Setup blocked for exactly the WSL bootstrap case | `gh auth login` guidance in the error; SSH and public URLs unaffected |
| `asExternalUri` changes the URL shape and breaks studio's token path | Studio fails to open in remote windows | The bootstrap URL carries a token in its path and query (`src/studio.ts` `parseStudioUrl`); verify the full path and query survive the mapping |
| Nested virtualization not pinned on the proxmox host | Test VM stops booting WSL2 after a host reboot | Pin `nested=1` in `/etc/modprobe.d/` while setting the VM up |
| Studio gaining `pull` introduces a rebase path that can conflict | A studio action leaves the config repo mid-rebase | `sync::reconcile_rebase` already exists and is what the CLI uses; reuse it rather than writing new reconciliation |

## Validation

**Workstream A.** Vitest unit tests over the pure platform-decision function
from A2. Add `windows-latest` to the extension CI matrix.

**Workstream B.** Rust tests. `src/sync.rs` already has tests that stand up
local bare repos (`bare()`, `clone_tolerates_machine_local_files` at
`src/sync.rs:920`); extend that pattern to cover the new routes. Add studio
route tests following the existing `/skills/*` cases at
`src/studio/server.rs:4556-4648`. Run `cargo test`, `cargo clippy
--all-targets`, and `cargo fmt` before calling either workstream done.

**End to end**, on proxmox VM 100: fresh WSL2, install the extension, confirm
the bundled `linux-x64` binary activates, complete setup, clone a config repo
from studio, and verify skills land in WSL's `~/.claude/skills`.

## Test environment

Proxmox VM 100 `windows-backup` — Windows 11 (`ostype: win11`), 4 cores, 8 GB
RAM, 96 GB disk, `cpu: host`, QEMU guest agent enabled, virtio-win ISO
attached. Currently stopped. Host has 28 GB of 62 GB free.

Nested virtualization is enabled on the host
(`/sys/module/kvm_intel/parameters/nested` = `Y`), which is the hard
requirement for WSL2 inside a guest.

**Open, pending Ellery:** the VM has no snapshots and no backups
(`qm listsnapshot 100` shows only "current"; `/var/lib/vz/dump/` is empty), and
its contents are unknown. It must be confirmed disposable, and snapshotted,
before use. Nothing has been started or modified.

## Rollback

Both workstreams are additive and land on branches.

- Workstream A: revert the branch. The extension returns to showing the
  unsupported notice on Windows. No user data is touched.
- Workstream B: revert the branch. Studio returns to push-on-apply only.
  `src/sync.rs` is not modified by this design — only called — so the CLI's
  sync behavior is unaffected either way.

The one action with side effects outside the repos is a studio-initiated
`sync init` creating a remote repository via `gh`. That is user-initiated,
matches what the CLI already does, and is undone by deleting the created repo.

## Rejected alternatives

**Native Windows CLI port.** Audited on 2026-08-10 and deferred. Seven files
fail to compile for `x86_64-pc-windows-msvc`: `src/tui.rs:11` (termios raw
mode), `src/context/system.rs:52` (`libc::getppid` + `ps`),
`src/studio/server.rs:2192` (`OpenOptionsExt`/`PermissionsExt`),
`src/studio/server.rs:2316` (`libc::signal`), `src/providers/tailnet.rs:39`
(`PermissionsExt`), `src/recents.rs:72` (`OsStrExt`), and `tests/cli.rs:886`
and `:3101`. Beyond compilation, `src/config.rs:766,784,797` resolve
`global_config_dir`, `state_dir`, and `home_dir` from `$HOME`, which Windows
does not set, so all three return `None`; `src/providers/mod.rs:319` runs every
script fragment through `sh -c` or `bash -c` and all eight shipped script
fragments are POSIX shell; `src/studio/server.rs:2444` shells to `open` /
`xdg-open`; `src/skills.rs:491` cannot create symlinks; and
`Command::new("claude")` will not resolve the `.cmd` shims npm installs.
`dist-workspace.toml` and `RELEASING.md:16-19` document the omission.

**Native Windows extension host bridging into WSL** (extension runs as a
Windows process, invokes `wsl.exe -- load ...`). Rejected because it breaks the
two things this project exists to preserve. Skills would install to WSL's
`~/.claude/skills` while native-Windows Claude Code and Cursor read
`%USERPROFILE%\.claude\skills`, and the config repo would land in WSL's home,
invisible to anything Windows-side. Correcting that requires forcing `HOME` and
`LOADOUT_CONFIG_DIR` to `/mnt/c/...` on every invocation plus `wslpath`
translation of every path — substantial fragile machinery for a worse result.

**Terminal handoff for sync setup.** Rejected. It inflated an error path into
an architectural component; see "Credentials".

## First implementation step

Workstream B, studio sync status. Add a read-only sync card — `is_synced`,
`remote_name`, `last_synced` — as a `/sync/card` route following the
`/skills/card` pattern at `src/studio/server.rs:163`, with a route test
alongside the existing skills cases. It is the smallest change that puts a sync
surface in studio, and every later action hangs off it.
