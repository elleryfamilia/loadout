# @ellery/loadout

npx installer and launcher for [loadout](https://github.com/elleryfamilia/loadout) —
the adaptive context layer for AI coding agents (Claude Code, Cursor, Codex,
Gemini, opencode).

```bash
npx @ellery/loadout studio
```

If the `load` binary is already installed, this checks the latest release
first and, when yours is older, offers to run `load update` before delegating —
so running npx means you're on the latest, but never without being asked. If
`load` isn't installed, it explains what the official installer will do —
download the prebuilt `load` binary from GitHub Releases, place it in
`~/.cargo/bin`, add that directory to your PATH if needed, and write an install
receipt so `load update` works — and asks for consent before doing anything.
In non-interactive terminals it never installs or updates: it prints the manual
`curl` command (or a one-line update hint) instead. The version check times out
after a couple of seconds and is skipped entirely when offline.

After the one-time install, use `load` directly — no npx needed:

```bash
load studio    # set up your loadout in the browser
load claude    # launch Claude Code with your context equipped
```

This package contains no binaries and has no dependencies; it is a thin
bootstrapper for the real CLI. macOS and Linux only (on Windows, use WSL).

Docs, source, and issues: https://github.com/elleryfamilia/loadout
