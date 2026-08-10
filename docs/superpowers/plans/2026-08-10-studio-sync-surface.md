# Studio Sync Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let studio set up, inspect, and run config-repo sync itself, so a machine whose only entry point is the VS Code extension can get onto a synced config without a terminal.

**Architecture:** A new self-contained `src/studio/sync_card.rs` module — handler plus its own `maud` views — modelled on the existing `src/studio/settings.rs`. Every route is a thin wrapper over a function that already exists in `src/sync.rs`; no new sync logic is written. Each handler has a `*_at(dir, …)` testable core taking an explicit config-dir path, mirroring `server::skill_action_at`.

**Tech Stack:** Rust 2021, `maud` for HTML, `tiny_http` routing, htmx for the client, `tempfile` + local bare git repos for tests.

## Global Constraints

- **No new sync logic.** Every action wraps an existing `src/sync.rs` function: `is_synced`, `last_synced`, `remote_name`, `pull`, `reconcile_rebase`, `commit_push`, `init`, `clone`, `gh_available`, `gh_create_repo`, `gh_repo_url`, `wire_remote_and_push`.
- **Immediate side effects, not staged.** Sync actions follow the agent-skill card pattern (`POST` → act → re-render the card), *not* the Settings staging pipeline. Nothing sync does is undone by Discard.
- **No terminal handoff, no credential form.** An authentication failure is an error message. Never collect a token in a form or write one into git config.
- **Manual timeout is 30s**, matching `src/commands/sync.rs:18` `MANUAL_TIMEOUT`.
- MSRV 1.85, edition 2021.
- Before any task is done: `cargo test --all --locked`, `cargo clippy --all-targets`, `cargo fmt --check`.
- Views take a prepared struct; **no filesystem access inside a view function** (the rule `settings.rs` follows).

---

## File Structure

| File | Responsibility |
|---|---|
| `src/studio/sync_card.rs` **(create)** | The whole sync surface: `SyncView`, the card view, the four handlers, and their `*_at` testable cores. Self-contained like `settings.rs`. |
| `src/studio/mod.rs:19-24` **(modify)** | Add `pub mod sync_card;` |
| `src/studio/server.rs:163` **(modify)** | Four route arms |
| `src/studio/server.rs:420` **(modify)** | Widen `fn field` to `pub(crate)` so `sync_card` can read form fields |
| `src/studio/server.rs:1987-2005` **(modify)** | Retire the `run \`load sync\`` dead-end string in `auto_push_after_apply` |
| `src/studio/settings.rs:9,64-78` **(modify)** | Embed the lazy card loader; correct the module doc that says `[sync]` is not here |
| `src/sync.rs:456-461` **(modify)** | Add `clone_target_is_clear`, exposing `clone`'s own preconditions so the card can hide a button that would fail |

**Two APIs already exist — do not reinvent them.** Form fields are read with `field(body, key) -> String` at `src/studio/server.rs:420` (it wraps `state::parse_pairs`); it is private today and Task 1 widens it. And the "is this dir clonable into" knowledge belongs to `sync.rs`, which already has a nine-entry `MACHINE_LOCAL` const plus a `.git` check inside `clone` — Task 1 exposes that rather than duplicating the list, which would silently drift.

Settings is the right home: sync is global machine config, and `settings.rs:9` already documents `[sync]` as a "future tenant".

---

### Task 1: Sync status card (read-only)

**Files:**
- Create: `src/studio/sync_card.rs`
- Modify: `src/studio/mod.rs` (add `pub mod sync_card;` after `pub mod state;`)
- Modify: `src/sync.rs` (add `clone_target_is_clear`)
- Modify: `src/studio/server.rs` (route arm; widen `fn field` to `pub(crate)`)
- Modify: `src/studio/settings.rs` (embed loader, fix module doc)
- Test: inline `#[cfg(test)] mod tests` in `src/studio/sync_card.rs` and `src/sync.rs`; route test in `src/studio/server.rs` tests module

**Interfaces:**
- Consumes: `crate::sync::{is_synced, last_synced, remote_name, gh_available}`; `crate::studio::server::Resp`; `crate::studio::views::{icon, error_fragment}`
- Produces:
  - `pub(crate) fn crate::sync::clone_target_is_clear(dir: &Path) -> bool`
  - `pub(crate) struct SyncView { synced: bool, remote: String, last_synced: Option<SystemTime>, gh: bool, dir_empty: bool, notice: Option<(bool, String)> }`
  - `pub(crate) fn view_for(dir: &Path, notice: Option<(bool, String)>) -> SyncView`
  - `pub(crate) fn card_fragment(v: &SyncView) -> String`
  - `pub fn card() -> Resp`
  - `pub(crate) fn render(notice: Option<(bool, String)>) -> Resp`
  - `pub(crate) const MANUAL_TIMEOUT: Duration`

- [ ] **Step 1: Write the failing tests**

Create `src/studio/sync_card.rs` with only the test module plus stub signatures:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn view(synced: bool, dir_empty: bool) -> SyncView {
        SyncView {
            synced,
            remote: if synced { "loadout-config".into() } else { String::new() },
            last_synced: None,
            gh: false,
            dir_empty,
            notice: None,
        }
    }

    #[test]
    fn unsynced_card_offers_setup_and_clone() {
        let html = card_fragment(&view(false, true));
        assert!(html.contains("/sync/init"), "offers set-up");
        assert!(html.contains("/sync/clone"), "offers clone");
        assert!(!html.contains("/sync/now"), "nothing to sync yet");
    }

    #[test]
    fn unsynced_card_hides_clone_when_config_dir_has_content() {
        // `sync::clone` refuses a non-empty dir, so the button must not be
        // offered where it is guaranteed to fail.
        let html = card_fragment(&view(false, false));
        assert!(html.contains("/sync/init"));
        assert!(!html.contains("/sync/clone"));
    }

    #[test]
    fn synced_card_shows_remote_and_offers_sync_now() {
        let html = card_fragment(&view(true, false));
        assert!(html.contains("loadout-config"), "names the remote");
        assert!(html.contains("/sync/now"));
        assert!(!html.contains("/sync/init"), "already set up");
    }

    #[test]
    fn notice_renders_and_marks_errors() {
        let mut v = view(true, false);
        v.notice = Some((true, "pulling from the remote failed".into()));
        let html = card_fragment(&v);
        assert!(html.contains("pulling from the remote failed"));
        assert!(html.contains("is-error"));
    }

    #[test]
    fn view_for_reports_unsynced_on_a_plain_directory() {
        let d = tempfile::tempdir().unwrap();
        let v = view_for(d.path(), None);
        assert!(!v.synced);
        assert!(v.dir_empty);
    }

    #[test]
    fn view_for_reports_dir_not_empty_ignoring_machine_local_files() {
        // `loadout-receipt.json` is written by the installer before `load` ever
        // runs; `sync::clone` tolerates it, so it must not count as content.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("loadout-receipt.json"), "{}").unwrap();
        assert!(view_for(d.path(), None).dir_empty);

        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        assert!(!view_for(d.path(), None).dir_empty);
    }
}
```

Also add to the `mod tests` block in `src/sync.rs`, covering the new helper against `clone`'s real preconditions:

```rust
    #[test]
    fn clone_target_is_clear_matches_what_clone_accepts() {
        let d = tempfile::tempdir().unwrap();
        assert!(clone_target_is_clear(d.path()), "empty dir");

        // Every machine-local entry is tolerated by `clone`.
        fs::write(d.path().join("loadout-receipt.json"), "{}").unwrap();
        fs::write(d.path().join("update-check"), "123").unwrap();
        fs::write(d.path().join("local.toml"), "secret = 1\n").unwrap();
        assert!(clone_target_is_clear(d.path()), "machine-local only");

        // Real config is not.
        fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        assert!(!clone_target_is_clear(d.path()));
    }

    #[test]
    fn clone_target_is_not_clear_for_an_existing_git_repo() {
        // `clone` bails on `.git` before it looks at anything else.
        let d = tempfile::tempdir().unwrap();
        Command::new("git").args(["init", "-q"]).arg(d.path()).status().unwrap();
        assert!(!clone_target_is_clear(d.path()));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib studio::sync_card 2>&1 | tail -20`
Expected: FAIL — `cannot find type SyncView in this scope`, `cannot find function card_fragment`.

- [ ] **Step 3: Implement the module**

Write above the test module in `src/studio/sync_card.rs`:

```rust
//! The studio **Sync** card — set up, inspect, and run config-repo sync
//! without leaving the UI.
//!
//! Unlike the rest of Settings (which stages every write through the
//! [`crate::studio::edit::Session`] pipeline), every action here is an
//! **immediate side effect** on the git repo in the global config dir — the
//! same shape as the agent-skill card. Nothing is staged, and Discard undoes
//! none of it.
//!
//! Every handler is a thin wrapper over [`crate::sync`]; no sync logic lives
//! here. Each has a `*_at` core taking an explicit dir so tests drive it
//! against a temp repo instead of the developer's real config.

use std::path::Path;
use std::time::{Duration, SystemTime};

use maud::{html, Markup};

use crate::studio::server::Resp;
use crate::studio::views;

/// User-initiated git network ops get the same budget as the CLI's
/// `load sync` (`src/commands/sync.rs:18`).
pub(crate) const MANUAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the card renders, prepared by the handler — no filesystem access
/// inside the view (the rule `settings.rs` follows).
pub(crate) struct SyncView {
    /// The config dir is a git repo with a remote.
    pub synced: bool,
    /// Short display name for the remote; empty when not synced.
    pub remote: String,
    pub last_synced: Option<SystemTime>,
    /// `gh` is installed — enables the one-click GitHub set-up path.
    pub gh: bool,
    /// The config dir holds nothing but machine-local files, so `sync::clone`
    /// would be accepted. Clone is hidden otherwise: it is guaranteed to fail.
    pub dir_empty: bool,
    /// `(is_error, message)` from the action that just ran.
    pub notice: Option<(bool, String)>,
}

pub(crate) fn view_for(dir: &Path, notice: Option<(bool, String)>) -> SyncView {
    let synced = crate::sync::is_synced(dir);
    SyncView {
        synced,
        remote: if synced { crate::sync::remote_name(dir) } else { String::new() },
        last_synced: crate::sync::last_synced(dir),
        gh: crate::sync::gh_available(),
        dir_empty: crate::sync::clone_target_is_clear(dir),
        notice,
    }
}
```

And in `src/sync.rs`, immediately after `is_machine_local` (line 461), add:

```rust
/// Whether `clone` would accept `dir` as a target — the same two preconditions
/// `clone` itself checks: no `.git`, and nothing but machine-local entries.
///
/// Exposed so studio can hide a Clone button that is guaranteed to fail. It
/// deliberately reuses `MACHINE_LOCAL` rather than letting a caller keep its own
/// copy, which would drift the moment an entry is added here.
pub(crate) fn clone_target_is_clear(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return false;
    }
    match dir.read_dir() {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .all(|e| is_machine_local(&e.file_name())),
        // Absent or unreadable — `clone` creates it, and surfaces any real error.
        Err(_) => true,
    }
}

/// "3 minutes ago" / "2 days ago" — coarse on purpose; the exact second is noise.
fn ago(t: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "just now".into();
    };
    let s = d.as_secs();
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} minutes ago", s / 60),
        3600..=86399 => format!("{} hours ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

pub(crate) fn card_fragment(v: &SyncView) -> String {
    let body = html! {
        div class="cmd-block" {
            @if let Some((is_error, msg)) = &v.notice {
                div class=(if *is_error { "banner is-error" } else { "banner" }) {
                    span class="banner-icon" { (views::icon(if *is_error { "alert" } else { "check" })) }
                    span { (msg) }
                }
            }
            @if v.synced {
                span class="muted small" {
                    "Your global config syncs with " strong { (v.remote) } "."
                    @if let Some(t) = v.last_synced { " Last synced " (ago(t)) "." }
                }
                button class="btn btn-ghost"
                    hx-post="/sync/now" hx-target="#sync-card" {
                    (views::icon("bolt")) "Sync now"
                }
            } @else {
                span class="muted small" {
                    "Sync keeps your global config in a git repo so every machine you work on \
                     equips the same context. Nothing leaves this machine until you set it up."
                }
                form hx-post="/sync/init" hx-target="#sync-card" class="sync-form" {
                    input type="url" name="remote" placeholder="git remote URL (optional)" {}
                    button class="btn btn-ghost" type="submit" {
                        (views::icon("plus"))
                        @if v.gh { "Set up sync (creates a private GitHub repo)" } @else { "Set up sync" }
                    }
                }
                @if v.dir_empty {
                    form hx-post="/sync/clone" hx-target="#sync-card" class="sync-form" {
                        input type="url" name="url" placeholder="https://github.com/you/loadout-config" required {}
                        button class="btn btn-ghost" type="submit" {
                            (views::icon("bolt")) "Clone an existing config"
                        }
                    }
                }
            }
        }
    };
    body.into_string()
}

/// `GET /sync/card` — render the card against the real config dir.
pub fn card() -> Resp {
    render(None)
}

/// Shared tail of every handler: rebuild the view from disk and re-render.
pub(crate) fn render(notice: Option<(bool, String)>) -> Resp {
    match crate::sync::config_dir() {
        Ok(dir) => Resp::html(card_fragment(&view_for(&dir, notice))),
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}
```

Add to `src/studio/mod.rs`, keeping the list alphabetical:

```rust
pub mod sync_card;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib studio::sync_card`
Expected: PASS, 6 tests.

- [ ] **Step 5: Wire the route**

In `src/studio/server.rs`, directly below the `("GET", "/skills/card")` arm at line 163:

```rust
        ("GET", "/sync/card") => sync_card::card(),
```

Add `sync_card` to the `use crate::studio::{...}` list at the top of `server.rs` if the module is not already in scope.

- [ ] **Step 6: Embed the loader in Settings**

In `src/studio/settings.rs`, inside `page_fragment`, after the agent section, add:

```rust
            // Loads lazily so the settings render never blocks on git.
            div id="sync-card" hx-get="/sync/card" hx-trigger="load" hx-target="#sync-card" {}
```

And correct the module doc at `src/studio/settings.rs:9` — remove `[sync]` from the "Not here (TOML-only, by design)" list, since it now has a home:

```rust
//! Not here (TOML-only, by design): `[env]`, `[codex]`, and the trust store
//! (a future tenant). `[sync]` is surfaced by [`crate::studio::sync_card`].
```

- [ ] **Step 7: Add the route test**

In the `mod tests` block of `src/studio/server.rs`, beside `skill_card_route_serves_a_card`:

```rust
    #[test]
    fn sync_card_route_serves_a_card() {
        // Read-only against the real config dir, so the synced state varies by
        // machine — assert only on what every state renders.
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/sync/card", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("cmd-block"));
    }

    #[test]
    fn settings_embeds_the_lazy_sync_card_loader() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let body = body_of(route(&st, &req("GET", "/settings", "", &[HOST, COOKIE], "")));
        assert!(body.contains("id=\"sync-card\""));
        assert!(body.contains("hx-get=\"/sync/card\""));
    }
```

- [ ] **Step 8: Verify green**

Run: `cargo test --all --locked && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass, no warnings.

- [ ] **Step 9: Commit**

```bash
git add src/studio/sync_card.rs src/studio/mod.rs src/studio/server.rs src/studio/settings.rs
git commit -m "feat(studio): sync status card in Settings

Studio already pushed the config on every apply but never showed sync
state or offered to set it up. This adds the read-only half."
```

---

### Task 2: "Sync now" (pull + push), and retire the CLI dead end

**Files:**
- Modify: `src/studio/sync_card.rs` (add `sync_now`, `sync_now_at`, `auth_hint`)
- Modify: `src/studio/server.rs` (route arm; `auto_push_after_apply` message at line 2001)
- Test: inline tests in `src/studio/sync_card.rs`

**Interfaces:**
- Consumes: `view_for`, `render`, `MANUAL_TIMEOUT` from Task 1
- Produces:
  - `pub(crate) fn auth_hint(msg: &str, gh: bool) -> String`
  - `pub(crate) fn sync_now_at(dir: &Path, timeout: Duration) -> (bool, String)` — `(is_error, message)`
  - `pub fn sync_now() -> Resp`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/studio/sync_card.rs`:

```rust
    use std::process::Command;

    /// A bare repo on `main`, matching `sync.rs`'s own test helper.
    fn bare(parent: &Path) -> std::path::PathBuf {
        let r = parent.join("remote.git");
        Command::new("git").args(["init", "--bare", "-b", "main", "-q"]).arg(&r).status().unwrap();
        r
    }

    fn identify(dir: &Path) {
        for (k, v) in [("user.email", "t@example.test"), ("user.name", "loadout test")] {
            Command::new("git").arg("-C").arg(dir).args(["config", k, v]).status().unwrap();
        }
    }

    #[test]
    fn auth_hint_names_gh_login_when_gh_is_installed() {
        let msg = "pushing failed: fatal: Authentication failed for 'https://github.com/x'";
        let with = auth_hint(msg, true);
        assert!(with.contains("gh auth login"));
        let without = auth_hint(msg, false);
        assert!(without.contains("SSH URL"), "still actionable without gh");
    }

    #[test]
    fn auth_hint_leaves_unrelated_errors_alone() {
        let msg = "pushing failed: could not resolve host github.com";
        assert_eq!(auth_hint(msg, true), msg);
    }

    #[test]
    fn sync_now_on_an_unsynced_dir_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().unwrap();
        let (is_error, msg) = sync_now_at(d.path(), Duration::from_secs(5));
        assert!(is_error);
        assert!(msg.contains("isn't set up"));
    }

    #[test]
    fn sync_now_pushes_local_edits_to_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git").args(["init", "-q"]).arg(&a).status().unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Edit, then Sync now.
        std::fs::write(a.join("config.toml"), "x = 2\n").unwrap();
        let (is_error, msg) = sync_now_at(&a, Duration::from_secs(30));
        assert!(!is_error, "expected success, got: {msg}");
        assert!(msg.contains("synced"), "got: {msg}");

        // A fresh clone sees the edit.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("config.toml")).unwrap(), "x = 2\n");
    }

    #[test]
    fn sync_now_pulls_a_remote_edit_made_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git").args(["init", "-q"]).arg(&a).status().unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Machine B clones, edits, pushes.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        identify(&b);
        std::fs::write(b.join("config.toml"), "x = 99\n").unwrap();
        crate::sync::commit_push(&b, "b edit", Duration::from_secs(30)).unwrap();

        // Machine A presses Sync now and receives it.
        let (is_error, msg) = sync_now_at(&a, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert_eq!(std::fs::read_to_string(a.join("config.toml")).unwrap(), "x = 99\n");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib studio::sync_card 2>&1 | tail -20`
Expected: FAIL — `cannot find function auth_hint`, `cannot find function sync_now_at`.

- [ ] **Step 3: Implement**

Add to `src/studio/sync_card.rs`:

```rust
/// Substrings that mean "git wanted credentials it did not have". Matched
/// case-insensitively against the whole error chain.
const AUTH_MARKERS: &[&str] = &[
    "authentication failed",
    "could not read username",
    "could not read password",
    "permission denied",
    "terminal prompts disabled",
    "403",
];

/// Append the one-command fix when a git failure looks like an auth failure.
///
/// This is the spec's whole credential story: an auth failure is an error
/// message, not a mechanism. `gh auth login` installs a git credential helper,
/// which is the single command that unblocks the common (GitHub, HTTPS) case.
pub(crate) fn auth_hint(msg: &str, gh: bool) -> String {
    let lower = msg.to_lowercase();
    if !AUTH_MARKERS.iter().any(|m| lower.contains(m)) {
        return msg.to_string();
    }
    if gh {
        format!("{msg} — run `gh auth login` once to give git credentials, then try again")
    } else {
        format!(
            "{msg} — git has no credentials for this remote. Use an SSH URL with a loaded key, \
             or install the GitHub CLI and run `gh auth login`"
        )
    }
}

/// Pull, then commit + push. Returns `(is_error, message)` for the card notice.
///
/// A `Diverged` pull is reconciled with `sync::reconcile_rebase` — the same
/// call the CLI makes — so studio resolves divergence in place instead of
/// telling the user to open a terminal.
pub(crate) fn sync_now_at(dir: &Path, timeout: Duration) -> (bool, String) {
    if !crate::sync::is_synced(dir) {
        return (true, "sync isn't set up on this machine yet".to_string());
    }
    let gh = crate::sync::gh_available();

    let pulled = match crate::sync::pull(dir, timeout) {
        Ok(crate::sync::PullOutcome::Pulled(n)) => n,
        Ok(crate::sync::PullOutcome::Diverged) => match crate::sync::reconcile_rebase(dir, timeout)
        {
            Ok(crate::sync::ReconcileOutcome::Rebased(n)) => n,
            Ok(crate::sync::ReconcileOutcome::Conflicted) => {
                return (
                    true,
                    "your config and the remote both changed the same lines — the rebase was \
                     aborted and nothing was lost. Reconcile by hand in the config dir."
                        .to_string(),
                )
            }
            Err(e) => return (true, auth_hint(&format!("reconciling failed: {e:#}"), gh)),
        },
        Err(e) => return (true, auth_hint(&format!("pulling failed: {e:#}"), gh)),
    };

    match crate::sync::commit_push(dir, "load studio: sync now", timeout) {
        Ok(crate::sync::PushOutcome::Pushed) => (
            false,
            format!("synced ✓ — pulled {pulled}, pushed your changes"),
        ),
        Ok(crate::sync::PushOutcome::NothingToPush) => {
            (false, format!("synced ✓ — pulled {pulled}, nothing to push"))
        }
        Ok(crate::sync::PushOutcome::Diverged) => (
            true,
            "the remote moved ahead again mid-sync — press Sync now once more".to_string(),
        ),
        Err(e) => (true, auth_hint(&format!("pushing failed: {e:#}"), gh)),
    }
}

/// `POST /sync/now`
pub fn sync_now() -> Resp {
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = sync_now_at(&dir, MANUAL_TIMEOUT);
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib studio::sync_card`
Expected: PASS, 11 tests.

- [ ] **Step 5: Wire the route and retire the dead end**

Route arm in `src/studio/server.rs`, below the `/sync/card` arm:

```rust
        ("POST", "/sync/now") => sync_card::sync_now(),
```

Then fix the dead-end message in `auto_push_after_apply` at `src/studio/server.rs:2001`. Replace:

```rust
        Ok(crate::sync::PushOutcome::Diverged) => {
            Some("remote moved ahead — run `load sync`".to_string())
        }
```

with:

```rust
        Ok(crate::sync::PushOutcome::Diverged) => {
            Some("remote moved ahead — press Sync now in Settings".to_string())
        }
```

- [ ] **Step 6: Add the CSRF route test**

In the `mod tests` block of `src/studio/server.rs`:

```rust
    #[test]
    fn sync_now_requires_origin_like_all_mutations() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("POST", "/sync/now", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 403);
    }
```

- [ ] **Step 7: Verify green**

Run: `cargo test --all --locked && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src/studio/sync_card.rs src/studio/server.rs
git commit -m "feat(studio): Sync now, resolving divergence in place

Studio could only push. A diverged remote printed 'run \`load sync\`' —
studio admitting it had no pull. It now pulls, rebases via the same
reconcile the CLI uses, and pushes."
```

---

### Task 3: "Set up sync" (init)

**Files:**
- Modify: `src/studio/sync_card.rs` (add `init`, `init_at`)
- Modify: `src/studio/server.rs` (route arm; widen `fn field` at line 420 to `pub(crate)`)
- Test: inline tests in `src/studio/sync_card.rs`

**Interfaces:**
- Consumes: `auth_hint`, `render`, `MANUAL_TIMEOUT`; `crate::studio::server::Req`
- Produces:
  - `pub(crate) fn crate::studio::server::field(body: &str, key: &str) -> String` (visibility widened, signature and body unchanged)
  - `pub(crate) fn init_at(dir: &Path, remote: Option<&str>, gh: bool, timeout: Duration) -> (bool, String)`
  - `pub fn init(req: &Req) -> Resp`

Note on the `gh` parameter: it is passed in rather than probed inside, so tests pin the behaviour on machines with and without `gh` installed.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn init_with_an_explicit_remote_publishes_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git").args(["init", "-q"]).arg(&a).status().unwrap();
        identify(&a);

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert!(crate::sync::is_synced(&a));

        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        crate::sync::clone(url, &b, Duration::from_secs(30)).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("config.toml")).unwrap(), "x = 1\n");
    }

    #[test]
    fn init_without_a_remote_and_without_gh_explains_what_is_needed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        let (is_error, msg) = init_at(d.path(), None, false, Duration::from_secs(30));
        assert!(is_error);
        assert!(msg.contains("remote"), "names what's missing: {msg}");
    }

    #[test]
    fn init_is_idempotent_on_an_already_synced_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 1\n").unwrap();
        Command::new("git").args(["init", "-q"]).arg(&a).status().unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        let (is_error, msg) = init_at(&a, Some(url), false, Duration::from_secs(30));
        assert!(is_error);
        assert!(msg.contains("already"), "got: {msg}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib studio::sync_card 2>&1 | tail -20`
Expected: FAIL — `cannot find function init_at`.

- [ ] **Step 3: Implement**

```rust
/// Set sync up. With an explicit `remote`, wires it and pushes. Without one,
/// falls back to `gh` creating a private repo named `loadout-config` — the same
/// flow `src/commands/sync.rs:123` runs for the CLI.
pub(crate) fn init_at(
    dir: &Path,
    remote: Option<&str>,
    gh: bool,
    timeout: Duration,
) -> (bool, String) {
    if crate::sync::is_synced(dir) {
        return (true, "sync is already set up on this machine".to_string());
    }
    let remote = remote.map(str::trim).filter(|r| !r.is_empty());

    if let Some(url) = remote {
        return match crate::sync::init(dir, Some(url), timeout) {
            Ok(()) => (false, format!("sync set up against {url} ✓")),
            Err(e) => (true, auth_hint(&format!("setting up sync failed: {e:#}"), gh)),
        };
    }

    if !gh {
        return (
            true,
            "paste a git remote URL to sync against — create an empty private repo on your \
             host first. (With the GitHub CLI installed and `gh auth login` done, loadout can \
             create one for you.)"
                .to_string(),
        );
    }

    // gh path: local repo first, then create + push.
    if let Err(e) = crate::sync::init(dir, None, timeout) {
        return (true, format!("preparing the local repo failed: {e:#}"));
    }
    match crate::sync::gh_create_repo("loadout-config", false, dir, timeout) {
        Ok(crate::sync::GhCreate::Created { url }) => {
            (false, format!("created and pushed to {url} ✓"))
        }
        Ok(crate::sync::GhCreate::NameExists) => match crate::sync::gh_repo_url("loadout-config", dir) {
            Some(url) => match crate::sync::wire_remote_and_push(dir, &url, timeout) {
                Ok(()) => (false, format!("adopted your existing {url} ✓")),
                Err(e) => (
                    true,
                    auth_hint(
                        &format!("a repo named loadout-config exists but adopting it failed: {e:#}"),
                        gh,
                    ),
                ),
            },
            None => (
                true,
                "a repo named loadout-config already exists on your account — paste its URL above \
                 to use it"
                    .to_string(),
            ),
        },
        Ok(crate::sync::GhCreate::Failed(m)) => (true, auth_hint(&format!("gh failed: {m}"), gh)),
        Err(e) => (true, auth_hint(&format!("gh failed: {e:#}"), gh)),
    }
}

/// `POST /sync/init` — body is the form-encoded `remote` field (may be empty;
/// `init_at` trims and treats empty as "no remote given").
pub fn init(req: &Req) -> Resp {
    let remote = crate::studio::server::field(&req.body, "remote");
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = init_at(
                &dir,
                Some(remote.as_str()),
                crate::sync::gh_available(),
                MANUAL_TIMEOUT,
            );
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}
```

Add `Req` to the imports at the top of the file:

```rust
use crate::studio::server::{Req, Resp};
```

Then widen the existing form-field reader at `src/studio/server.rs:420` from private to crate-visible — signature and body unchanged:

```rust
pub(crate) fn field(body: &str, key: &str) -> String {
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib studio::sync_card`
Expected: PASS, 14 tests.

- [ ] **Step 5: Wire the route + CSRF test**

```rust
        ("POST", "/sync/init") => sync_card::init(req),
```

```rust
    #[test]
    fn sync_init_requires_origin_like_all_mutations() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("POST", "/sync/init", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 403);
    }
```

- [ ] **Step 6: Verify green**

Run: `cargo test --all --locked && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 7: Commit**

```bash
git add src/studio/sync_card.rs src/studio/server.rs
git commit -m "feat(studio): set up config sync from Settings

Wraps sync::init, with the same gh-create-repo path the CLI uses when
no remote URL is given."
```

---

### Task 4: "Clone an existing config"

**Files:**
- Modify: `src/studio/sync_card.rs` (add `clone_repo`, `clone_at`)
- Modify: `src/studio/server.rs` (route arm)
- Test: inline tests in `src/studio/sync_card.rs`

**Interfaces:**
- Consumes: `auth_hint`, `render`, `MANUAL_TIMEOUT`, `Req`
- Produces:
  - `pub(crate) fn clone_at(dir: &Path, url: &str, gh: bool, timeout: Duration) -> (bool, String)`
  - `pub fn clone_repo(req: &Req) -> Resp`

This is the task the whole workstream exists for: a fresh WSL2 machine pulling down an existing config.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn clone_brings_an_existing_config_onto_a_fresh_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = bare(tmp.path());
        let url = remote.to_str().unwrap();

        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("config.toml"), "x = 7\n").unwrap();
        Command::new("git").args(["init", "-q"]).arg(&a).status().unwrap();
        identify(&a);
        crate::sync::init(&a, Some(url), Duration::from_secs(30)).unwrap();

        // Fresh machine: config dir holds only the installer's receipt.
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("loadout-receipt.json"), r#"{"version":"0.25.0"}"#).unwrap();

        let (is_error, msg) = clone_at(&b, url, false, Duration::from_secs(30));
        assert!(!is_error, "got: {msg}");
        assert_eq!(std::fs::read_to_string(b.join("config.toml")).unwrap(), "x = 7\n");
        assert!(b.join("loadout-receipt.json").exists(), "machine-local file survives");
    }

    #[test]
    fn clone_rejects_a_blank_url() {
        let d = tempfile::tempdir().unwrap();
        let (is_error, msg) = clone_at(d.path(), "   ", false, Duration::from_secs(5));
        assert!(is_error);
        assert!(msg.contains("URL"));
    }

    #[test]
    fn clone_refuses_when_the_config_dir_already_has_content() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "x = 1\n").unwrap();
        let (is_error, msg) = clone_at(d.path(), "https://example.test/x.git", false, Duration::from_secs(5));
        assert!(is_error);
        assert!(msg.to_lowercase().contains("already has content"), "got: {msg}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib studio::sync_card 2>&1 | tail -20`
Expected: FAIL — `cannot find function clone_at`.

- [ ] **Step 3: Implement**

```rust
/// Clone an existing config repo onto this machine. `sync::clone` does the
/// real work, including tolerating the installer's machine-local files and
/// refusing a dir that already holds real config.
pub(crate) fn clone_at(dir: &Path, url: &str, gh: bool, timeout: Duration) -> (bool, String) {
    let url = url.trim();
    if url.is_empty() {
        return (true, "paste the URL of your config repo".to_string());
    }
    match crate::sync::clone(url, dir, timeout) {
        Ok(()) => (
            false,
            format!("cloned your config from {url} ✓ — reopen studio to see it"),
        ),
        Err(e) => (true, auth_hint(&format!("cloning failed: {e:#}"), gh)),
    }
}

/// `POST /sync/clone` — body is the form-encoded `url` field.
pub fn clone_repo(req: &Req) -> Resp {
    let url = crate::studio::server::field(&req.body, "url");
    match crate::sync::config_dir() {
        Ok(dir) => {
            let notice = clone_at(&dir, &url, crate::sync::gh_available(), MANUAL_TIMEOUT);
            render(Some(notice))
        }
        Err(e) => Resp::html(views::error_fragment(&format!(
            "cannot resolve the global config dir: {e:#}"
        ))),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib studio::sync_card`
Expected: PASS, 17 tests.

- [ ] **Step 5: Wire the route + CSRF test**

```rust
        ("POST", "/sync/clone") => sync_card::clone_repo(req),
```

```rust
    #[test]
    fn sync_clone_requires_origin_like_all_mutations() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("POST", "/sync/clone", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 403);
    }
```

- [ ] **Step 6: Verify green**

Run: `cargo test --all --locked && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 7: Commit**

```bash
git add src/studio/sync_card.rs src/studio/server.rs
git commit -m "feat(studio): clone an existing config repo from Settings

The fresh-machine path the WSL2 story needs: a new box can pull down an
existing config without opening a terminal."
```

---

## Manual verification

Automated tests cover the logic; these check the UI reads correctly.

1. `cargo run -- studio`, open Settings. On a synced machine the card names the remote and offers **Sync now**; on an unsynced one it offers set-up and clone.
2. Press **Sync now**; confirm the notice reports pulled/pushed counts and the card re-renders without a page reload.
3. Edit a fragment and Apply; confirm the flash still reports `synced ✓` from `auto_push_after_apply` (Task 2 must not have broken the existing push-on-apply path).
4. Point the clone form at a private repo on a machine with no git credential helper; confirm the error names `gh auth login` rather than hanging.

## Out of scope

- `load sync status --json` for the extension's tree view. The spec lists it as optional; studio is the surface, and nothing in workstream A depends on it.
