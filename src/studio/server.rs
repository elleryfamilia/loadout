//! The `tiny_http` spine: a blocking, synchronous server bound to 127.0.0.1,
//! a small `(method, path)` router, and the localhost security model (§10).
//!
//! The routing/guard logic lives in [`route`], a pure function over a
//! [`Req`]/[`Resp`] boundary, so it's unit-testable without a socket. The
//! [`serve`] loop just adapts `tiny_http` requests to that boundary.
//!
//! Security: bind 127.0.0.1 only; a one-time **bootstrap-token** route is the
//! sole route reachable without the session cookie — it sets an
//! `HttpOnly; SameSite=Strict` cookie and redirects to a tokenless URL. Every
//! other route (assets and GETs included) requires that cookie; a **Host-header
//! allowlist** defeats DNS-rebinding; state-changing methods additionally require
//! an exact **Origin/Referer** match. No CORS headers are ever emitted.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context as _};

use crate::cli::StudioArgs;
use crate::commands::Runtime;
use crate::config::{self, Config};
use crate::context;
use crate::dynamic::DynamicMode;
use crate::fragment::{palette, Layer};
use crate::pack::Pack;
use crate::profile::LoadoutConfig;
use crate::studio::assets;
use crate::studio::edit::{Session, StagedOp};
use crate::studio::state::{self, LibraryView, PreviewOutcome, StudioState};
use crate::studio::views;

/// The sole route reachable without the session cookie (carries the token).
pub const BOOTSTRAP_PATH: &str = "/__studio/bootstrap";

/// A normalized inbound request (decoupled from `tiny_http` for testing).
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Header names lowercased.
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// A response to write back.
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    fn html(s: impl Into<String>) -> Resp {
        Resp {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body: s.into().into_bytes(),
        }
    }
    /// Like [`Resp::html`] but retargets the swap via htmx's `HX-Retarget` /
    /// `HX-Reswap` headers, so a modal form's error lands inside the modal
    /// (`target`) instead of replacing the page behind it.
    fn html_retarget(s: impl Into<String>, target: &str) -> Resp {
        let mut r = Resp::html(s);
        r.headers.push(("HX-Retarget".into(), target.to_string()));
        r.headers.push(("HX-Reswap".into(), "innerHTML".into()));
        r
    }
    fn asset(body: Vec<u8>, content_type: &str) -> Resp {
        Resp {
            status: 200,
            headers: vec![
                ("content-type".into(), content_type.into()),
                ("cache-control".into(), "no-store".into()),
            ],
            body,
        }
    }
    fn forbidden(msg: &str) -> Resp {
        Resp {
            status: 403,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: format!("403 forbidden: {msg}\n").into_bytes(),
        }
    }
    fn not_found() -> Resp {
        Resp {
            status: 404,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: b"404 not found\n".to_vec(),
        }
    }
    fn redirect(location: &str, set_cookie: Option<&str>) -> Resp {
        let mut headers = vec![("location".into(), location.to_string())];
        if let Some(c) = set_cookie {
            headers.push(("set-cookie".into(), c.to_string()));
        }
        Resp {
            status: 302,
            headers,
            body: Vec::new(),
        }
    }
}

/// Route + guard a request. Pure over the [`Req`]/[`Resp`] boundary.
pub fn route(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (token, port) = {
        let s = state.lock().unwrap();
        (s.token.clone(), s.port)
    };

    // 1. Host-header allowlist (DNS-rebinding defense) — all routes.
    if !host_ok(req, port) {
        return Resp::forbidden("unexpected Host header");
    }
    // 2. Bootstrap — the only route reachable without the session cookie.
    if req.path == BOOTSTRAP_PATH {
        return bootstrap(req, &token);
    }
    // 3. Session cookie — required for everything else (assets + GETs too).
    if cookie_token(req).as_deref() != Some(token.as_str()) {
        return Resp::forbidden(
            "missing or invalid session token — open the bootstrap URL printed by `load studio`",
        );
    }
    // 4. Origin/Referer — required for state-changing methods (no CORS).
    let state_changing = matches!(req.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    if state_changing && !origin_ok(req, port) {
        return Resp::forbidden("bad Origin/Referer");
    }

    // 5. Dispatch.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => handle_shell(state),
        ("GET", "/tab/profiles") => handle_tab(state, "profiles"),
        ("GET", "/tab/library") => handle_library(state, "fragments"),
        ("GET", "/tab/fragments") => handle_library(state, "fragments"),
        ("GET", "/tab/targets") => handle_library(state, "targets"),
        ("GET", "/tab/workflows") => handle_library(state, "workflows"),
        ("GET", "/staged") => handle_staged(state),
        ("GET", "/close") => Resp::html(String::new()),
        ("GET", "/diff") => handle_diff(state),
        ("POST", "/apply") => handle_apply(state),
        ("POST", "/discard") => handle_discard(state),
        ("GET", "/fragments/new") => {
            Resp::html(views::fragment_dialog(None, Layer::Global, true, None, &[]))
        }
        ("POST", "/fragments") => handle_fragment_save(state, req),
        ("POST", "/fragments/try") => handle_fragment_try(req),
        ("GET", "/targets/new") => Resp::html(views::target_dialog(None, Layer::Global)),
        ("POST", "/targets") => handle_target_save(state, req),
        ("POST", "/targets/try") => handle_target_try(state, req),
        ("GET", "/workflows/new") => Resp::html(views::workflow_editor(None, false)),
        ("POST", "/workflows") => handle_workflow_save(state, req),
        ("GET", "/packs") => handle_packs(state),
        ("GET", "/skills/card") => handle_skill_card(),
        ("POST", "/skills/install") => handle_skill_install(),
        ("GET", "/onboarding/welcome") => handle_onboarding_welcome(state),
        ("POST", "/onboarding/quickstart") => handle_quickstart(state),
        ("GET", "/profiles/new") => handle_profile_new(state),
        ("POST", "/profiles") => handle_profile_save(state, req),
        ("POST", "/profiles/preview") => handle_editor_preview(state, req),
        ("POST", "/profiles/draft") => handle_profile_draft(state, req),
        ("GET", p) if p.starts_with("/assets/") => match assets::get(p) {
            Some((body, ct)) => Resp::asset(body, ct),
            None => Resp::not_found(),
        },
        ("GET", p) if p.starts_with("/library/") => {
            handle_library(state, p.strip_prefix("/library/").unwrap_or("fragments"))
        }
        (_, p) if p.starts_with("/fragments/") => handle_fragment_param(state, req),
        (_, p) if p.starts_with("/targets/") => handle_target_param(state, req),
        (_, p) if p.starts_with("/workflows/") => handle_workflow_param(state, req),
        (_, p) if p.starts_with("/profiles/") => handle_profile_param(state, req),
        (_, p) if p.starts_with("/packs/") => handle_pack_param(state, req),
        _ => Resp::not_found(),
    }
}

/// Split `/<prefix>/<id>[/action]` into the decoded id and the action.
fn id_and_action<'a>(path: &'a str, prefix: &str) -> (String, &'a str) {
    let rest = path.strip_prefix(prefix).unwrap_or("");
    match rest.split_once('/') {
        Some((id, action)) => (state::percent_decode(id), action),
        None => (state::percent_decode(rest), ""),
    }
}

fn handle_fragment_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (id, action) = id_and_action(&req.path, "/fragments/");
    match (req.method.as_str(), action) {
        ("GET", "edit") | ("GET", "view") => {
            // A `?profile=` carries the profile to return to when editing a cap
            // from inside its detail (so Save re-renders that profile, not the
            // Fragments tab).
            let profile = field(&req.query, "profile");
            handle_fragment_edit(state, &id, &profile)
        }
        ("DELETE", "") => handle_fragment_delete(state, &id),
        ("POST", "duplicate") => handle_fragment_duplicate(state, &id),
        ("POST", "run") => handle_fragment_run(state, &id, &field(&req.query, "profile")),
        _ => Resp::not_found(),
    }
}

fn handle_profile_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (name, action) = id_and_action(&req.path, "/profiles/");
    let (head, rest) = action.split_once('/').unwrap_or((action, ""));
    match (req.method.as_str(), head, rest) {
        ("GET", "edit", "") => handle_profile_edit(state, &name),
        // `select` renders the Create-a-Loadout board; `preview` swaps in the
        // composed-document view (the rendered guidance).
        ("GET", "select", "") => board_resp(state, &name),
        ("GET", "preview", "") => handle_profile_select(state, &name),
        ("POST", "disable", "") => handle_profile_disable(state, &name),
        ("POST", "run", "") => handle_profile_run(state, &name),
        ("DELETE", "", "") => handle_profile_delete(state, &name),
        // Applies-to (targets) slot.
        ("GET", "targets", "new") => handle_target_picker(state, &name),
        ("POST", "targets", id) if !id.is_empty() => {
            handle_profile_target_add(state, &name, &state::percent_decode(id))
        }
        ("DELETE", "targets", id) if !id.is_empty() => {
            handle_profile_target_remove(state, &name, &state::percent_decode(id))
        }
        // Fragments slots.
        ("GET", "fragments", "new") => handle_fragment_picker(state, &name),
        ("POST", "fragments", id) if !id.is_empty() => {
            handle_profile_fragment_add(state, &name, &state::percent_decode(id))
        }
        ("DELETE", "fragments", id) if !id.is_empty() => {
            handle_profile_fragment_remove(state, &name, &state::percent_decode(id))
        }
        // Workflow slot (one per loadout).
        ("GET", "workflow", "new") => handle_workflow_picker(state, &name),
        ("POST", "workflow", id) if !id.is_empty() => {
            handle_profile_workflow_set(state, &name, &state::percent_decode(id))
        }
        ("DELETE", "workflow", "") => handle_profile_workflow_clear(state, &name),
        _ => Resp::not_found(),
    }
}

/// Re-render the board into `#profile-main` after a slot edit, closing any open
/// picker modal and refreshing the staged-changes indicator.
fn board_resp(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    match state::board_view(&snap, name) {
        Ok(b) => {
            let mut html = views::loadout_board_fragment(&b);
            html.push_str(&views::modal_close_loader());
            html.push_str(&views::staged_indicator_loader());
            Resp::html(html)
        }
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Current targets / fragment ids / workflow of the staged loadout `name`.
fn staged_profile<T>(
    state: &Arc<Mutex<StudioState>>,
    name: &str,
    f: impl FnOnce(&crate::profile::LoadoutConfig) -> T,
    default: T,
) -> T {
    let snap = state.lock().unwrap().snapshot();
    state::staged_config(&snap)
        .ok()
        .and_then(|c| {
            c.profiles
                .into_iter()
                .find(|p| p.name == name)
                .map(|p| f(&p))
        })
        .unwrap_or(default)
}

fn handle_target_picker(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let current = staged_profile(state, name, |p| p.targets.clone(), Vec::new());
    let options: Vec<_> = lib
        .targets
        .into_iter()
        .filter(|t| !current.iter().any(|c| c == &t.id))
        .collect();
    Resp::html(views::target_picker(name, &options))
}

fn handle_fragment_picker(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let equipped = staged_profile(
        state,
        name,
        |p| p.fragments.iter().map(|fr| fr.id().to_string()).collect(),
        Vec::new(),
    );
    Resp::html(views::fragment_picker(name, &lib, &equipped))
}

fn handle_workflow_picker(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let options = state::board_workflow_options(&snap);
    let current = staged_profile(state, name, |p| p.workflow.clone(), None);
    Resp::html(views::workflow_picker(name, &options, current.as_deref()))
}

fn handle_profile_target_add(state: &Arc<Mutex<StudioState>>, name: &str, id: &str) -> Resp {
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| {
            if !p.targets.iter().any(|t| t == id) {
                p.targets.push(id.to_string());
            }
        })
    };
    finish_slot_edit(state, name, r)
}

fn handle_profile_target_remove(state: &Arc<Mutex<StudioState>>, name: &str, id: &str) -> Resp {
    // Guard the single-default invariant (the UI also disables the ✕): dropping
    // the last target would make a second no-targets default.
    {
        let snap = state.lock().unwrap().snapshot();
        if let Ok(cfg) = state::staged_config(&snap) {
            if let Some(p) = cfg.profiles.iter().find(|p| p.name == name) {
                let last = p.targets.len() == 1 && p.targets.iter().any(|t| t == id);
                let other_default = cfg
                    .profiles
                    .iter()
                    .any(|q| q.name != name && !q.disabled && q.targets.is_empty());
                if last && other_default {
                    return Resp::html(views::error_fragment(
                        "a loadout needs at least one target — a default loadout already exists",
                    ));
                }
            }
        }
    }
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| p.targets.retain(|t| t != id))
    };
    finish_slot_edit(state, name, r)
}

fn handle_profile_fragment_add(state: &Arc<Mutex<StudioState>>, name: &str, id: &str) -> Resp {
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| {
            if !p.fragments.iter().any(|fr| fr.id() == id) {
                p.fragments
                    .push(crate::profile::FragmentRef::Id(id.to_string()));
            }
        })
    };
    finish_slot_edit(state, name, r)
}

fn handle_profile_fragment_remove(state: &Arc<Mutex<StudioState>>, name: &str, id: &str) -> Resp {
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| {
            p.fragments.retain(|fr| fr.id() != id)
        })
    };
    finish_slot_edit(state, name, r)
}

fn handle_profile_workflow_set(state: &Arc<Mutex<StudioState>>, name: &str, id: &str) -> Resp {
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| p.workflow = Some(id.to_string()))
    };
    finish_slot_edit(state, name, r)
}

fn handle_profile_workflow_clear(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let r = {
        let mut s = state.lock().unwrap();
        state::edit_profile(&mut s.session, name, |p| p.workflow = None)
    };
    finish_slot_edit(state, name, r)
}

/// Re-render the board on a successful slot edit, or surface the staging error.
fn finish_slot_edit(state: &Arc<Mutex<StudioState>>, name: &str, r: crate::Result<()>) -> Resp {
    match r {
        Ok(()) => board_resp(state, name),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Read a single decoded form field from a urlencoded body.
fn field(body: &str, key: &str) -> String {
    state::parse_pairs(body)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

// --- handlers (snapshot under the lock, render outside it) -------------------

/// `GET /` — the full page. Lands on the Profiles tab (the unit of value: "what
/// guidance does my agent get here?"); a fresh config shows the first-launch
/// welcome there. Fragments live one tab over as the parts drawer.
fn handle_shell(state: &Arc<Mutex<StudioState>>) -> Resp {
    let staged = state.lock().unwrap().session.ops().len();
    match profiles_tab_main(state, None, None) {
        Ok((main, armed)) => {
            if armed {
                state.lock().unwrap().onboarding_active = true;
            }
            Resp::html(views::shell(main, staged, "profiles"))
        }
        Err(e) => Resp::html(views::error_page(&e)),
    }
}

/// The Loadouts destination (the only one routed through here now). Library
/// categories go through [`handle_library`].
fn handle_tab(state: &Arc<Mutex<StudioState>>, tab: &str) -> Resp {
    state.lock().unwrap().active_tab = tab.to_string();
    profiles_tab_resp(state, None, None, false)
}

/// Render a Library category body (`fragments` | `targets` | `workflows`). Each
/// body carries its own pill sub-nav (`library_nav`), so every render — full
/// navigation or post-mutation refresh — keeps the Library chrome. `active_tab`
/// stays `"library"` so the top-nav Library button remains lit while browsing.
fn handle_library(state: &Arc<Mutex<StudioState>>, cat: &str) -> Resp {
    state.lock().unwrap().active_tab = "library".to_string();
    let snap = state.lock().unwrap().snapshot();
    match cat {
        "targets" => Resp::html(views::targets_tab_fragment(&state::targets_view(&snap))),
        "workflows" => Resp::html(views::workflows_tab_fragment(&state::workflows_view(
            &snap, None,
        ))),
        _ => match state::library_view(&snap) {
            Ok(lib) => Resp::html(views::fragments_tab_fragment(&lib, None)),
            Err(e) => Resp::html(views::error_fragment(&e.to_string())),
        },
    }
}

/// Render the full Profiles tab (rail + detail). A detail pane shows only when
/// `selected` names an existing profile; otherwise the rail renders with a
/// "pick a profile" prompt (no profile is auto-selected). `flash` shows a
/// banner; `with_staged` appends the staged-indicator refresh loader (used after
/// a mutation re-renders `#main`).
/// Build the Profiles tab's `#main` markup, plus whether the first-launch welcome
/// was shown (`armed` — which arms the guided onboarding flow). Shared by the
/// full-page shell (`GET /`) and the htmx tab swap so both land identically.
fn profiles_tab_main(
    state: &Arc<Mutex<StudioState>>,
    selected: Option<&str>,
    flash: Option<&str>,
) -> Result<(maud::Markup, bool), String> {
    let snap = state.lock().unwrap().snapshot();
    let lib = state::library_view(&snap).map_err(|e| e.to_string())?;
    // No auto-selection: show a detail pane only when a profile was explicitly
    // requested (and still exists). On a bare tab open — or after delete/apply —
    // the rail renders with a "pick a profile" prompt instead of defaulting to
    // the bound or first profile.
    let effective: Option<String> = selected
        .filter(|n| lib.profiles.iter().any(|p| p.name == *n))
        .map(str::to_string);
    let detail = effective.map(|name| {
        let disabled = lib
            .profiles
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.disabled)
            .unwrap_or(false);
        let outcome = state::render_profile(&snap, &name, "", DynamicMode::ReadOnly)
            .unwrap_or_else(|e| empty_preview(format!("preview error: {e}")));
        (name, outcome, disabled)
    });
    // On a fresh config (no profiles and no own caps), the empty Profiles tab
    // shows the first-launch welcome instead of the bare "no profiles" prompt.
    let onboarding = (detail.is_none() && lib.profiles.is_empty() && lib.yours.is_empty())
        .then(|| state::onboarding(&snap.base_context));
    let armed = onboarding.is_some();
    // The starter-pack gallery is only rendered inside the first-launch welcome.
    let packs = if armed {
        state::pack_views(&snap).unwrap_or_default()
    } else {
        Vec::new()
    };
    let markup = match &detail {
        Some((name, outcome, disabled)) => views::profiles_tab(
            &lib,
            Some(views::ProfileDetail {
                name,
                outcome,
                disabled: *disabled,
                expand: views::Expand::None,
                failed: None,
            }),
            flash,
            None,
            &[],
        ),
        None => views::profiles_tab(&lib, None, flash, onboarding.as_ref(), &packs),
    };
    Ok((markup, armed))
}

fn profiles_tab_resp(
    state: &Arc<Mutex<StudioState>>,
    selected: Option<&str>,
    flash: Option<&str>,
    with_staged: bool,
) -> Resp {
    match profiles_tab_main(state, selected, flash) {
        Ok((markup, armed)) => {
            if armed {
                state.lock().unwrap().onboarding_active = true;
            }
            let mut html = markup.into_string();
            if with_staged {
                html.push_str(&views::staged_indicator_loader());
            }
            Resp::html(html)
        }
        Err(e) => Resp::html(views::error_fragment(&e)),
    }
}

/// Render just the selected profile's detail (swapped into `#profile-main`),
/// composed for `agent` (empty ⇒ the configured default). `mode` is ReadOnly for
/// the normal preview, or Live when re-rendering after a "Run".
fn handle_profile_detail(
    state: &Arc<Mutex<StudioState>>,
    name: &str,
    agent: &str,
    mode: DynamicMode,
    expand: views::Expand,
    failed: Option<(String, String)>,
) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let disabled = state::staged_config(&snap)
        .ok()
        .and_then(|cfg| {
            cfg.profiles
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.disabled)
        })
        .unwrap_or(false);
    match state::render_profile(&snap, name, agent, mode) {
        Ok(outcome) => Resp::html(views::profile_detail_fragment(&views::ProfileDetail {
            name,
            outcome: &outcome,
            disabled,
            expand,
            failed,
        })),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Run all of a profile's dynamic caps now (Live), then re-render its detail so
/// every script/provider shows real output. Scripts run (subject to `allow_exec`)
/// and their (redacted) output is cached, so the read-only preview keeps it.
fn handle_profile_run(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    handle_profile_detail(
        state,
        name,
        "",
        DynamicMode::Live,
        views::Expand::AllDynamic,
        None,
    )
}

/// Run one dynamic cap now (Live) so its output caches, then re-render the
/// profile detail (ReadOnly) — only the run cap shows fresh output; the rest keep
/// their placeholder until they're run too. `profile` scopes the render context.
/// The run fragment re-renders expanded so its output stays visible.
fn handle_fragment_run(state: &Arc<Mutex<StudioState>>, id: &str, profile: &str) -> Resp {
    let failed = {
        let snap = state.lock().unwrap().snapshot();
        match state::run_fragment(&snap, profile, id) {
            Ok(err) => err.map(|msg| (id.to_string(), msg)),
            Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
        }
    };
    handle_profile_detail(
        state,
        profile,
        "",
        DynamicMode::ReadOnly,
        views::Expand::One(id),
        failed,
    )
}

/// An empty preview outcome (used when a profile can't be composed/rendered).
fn empty_preview(note: String) -> PreviewOutcome {
    PreviewOutcome {
        agent: String::new(),
        context_summary: String::new(),
        fragment_count: 0,
        overlay: String::new(),
        caps: Vec::new(),
        note: Some(note),
    }
}

fn handle_staged(state: &Arc<Mutex<StudioState>>) -> Resp {
    let staged = state.lock().unwrap().session.ops().len();
    Resp::html(views::staged_indicator_fragment(staged))
}

/// Build the library view from the current session (helper for result fragments).
fn library_now(state: &Arc<Mutex<StudioState>>) -> crate::Result<LibraryView> {
    let snap = state.lock().unwrap().snapshot();
    state::library_view(&snap)
}

// --- fragment handlers -----------------------------------------------------

fn handle_fragment_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    // `?as=copy` → save as a new fragment (don't overwrite the original);
    // `return_profile` (hidden) → the user is editing from a profile's detail.
    let as_copy = field(&req.query, "as") == "copy";
    let return_profile = field(&req.body, "return_profile");

    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    // The existing fragment (if editing) so the simple editor's merge preserves
    // fields it doesn't expose (requires/agents/cache/…).
    let base = state::editor_fragment_id(&pairs)
        .and_then(|id| cfg.fragments.iter().find(|c| c.id == id).cloned());
    let mut cap = match state::fragment_from_form(base.as_ref(), &pairs) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if as_copy {
        // Duplicate under a new name: a fresh id derived from the title, distinct
        // from the original and from every existing fragment.
        let new_id = state::slug(&field(&req.body, "name"));
        if new_id.is_empty() {
            return Resp::html(views::error_fragment(
                "give the copy a name with letters or digits",
            ));
        }
        if cfg.fragments.iter().any(|c| c.id == new_id) {
            return Resp::html(views::error_fragment(&format!(
                "“{new_id}” already exists — choose a new name for the copy"
            )));
        }
        cap.id = new_id;
    }
    let layer = state::layer_from_form(&pairs);
    let id = cap.id.clone();
    // EditFragment upserts by id (creates if absent), so save covers new+edit.
    let res = state.lock().unwrap().session.stage(StagedOp::EditFragment {
        layer,
        id: id.clone(),
        cap: Box::new(cap),
    });
    if let Err(e) = res {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    let flash = if as_copy {
        format!("saved copy “{id}”")
    } else {
        format!("staged fragment “{id}”")
    };
    // Edited from a profile → re-render that profile's detail (so an in-place
    // save shows the updated guidance) and close the modal. Otherwise re-render
    // the Fragments tab.
    if !return_profile.is_empty() {
        let mut resp = profiles_tab_resp(state, Some(&return_profile), Some(&flash), true);
        if resp.status == 200 {
            resp.body
                .extend_from_slice(views::modal_close_loader().as_bytes());
        }
        return resp;
    }
    match library_now(state) {
        Ok(lib) => Resp::html(views::fragment_result(&lib, &flash)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_fragment_edit(state: &Arc<Mutex<StudioState>>, id: &str, return_profile: &str) -> Resp {
    let rp = (!return_profile.is_empty()).then_some(return_profile);
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if let Some(c) = cfg.fragments.iter().find(|c| c.id == id) {
        let used_by = profiles_using(&cfg, id);
        Resp::html(views::fragment_dialog(
            Some(c),
            c.origin,
            true,
            rp,
            &used_by,
        ))
    } else if let Some(c) = palette().into_iter().find(|c| c.id == id) {
        Resp::html(views::fragment_dialog(
            Some(&c),
            Layer::Global,
            false,
            rp,
            &[],
        ))
    } else {
        Resp::html(views::error_fragment(&format!("unknown fragment '{id}'")))
    }
}

/// Names of the profiles whose `fragments` reference `fragment_id`.
fn profiles_using(cfg: &Config, fragment_id: &str) -> Vec<String> {
    cfg.profiles
        .iter()
        .filter(|p| p.fragments.iter().any(|r| r.id() == fragment_id))
        .map(|p| p.name.clone())
        .collect()
}

fn handle_fragment_delete(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    // Deleting a composed fragment would leave dangling references in the
    // profiles that use it (silently dropped at render). Instead, stage the
    // deletion AND remove the id from every profile that composed it, and report
    // the cleanup so it isn't invisible.
    let res = (|| -> crate::Result<(Vec<String>, Vec<String>)> {
        let mut s = state.lock().unwrap();
        let Some(layer) = s.session.fragment_layer(id) else {
            return Err(anyhow!(
                "“{id}” isn't in your library — palette items can't be deleted"
            ));
        };
        let cfg = s.session.staged_config()?;
        let affected = profiles_using(&cfg, id);
        let cleaned: Vec<LoadoutConfig> = cfg
            .profiles
            .iter()
            .filter(|p| affected.contains(&p.name))
            .map(|p| {
                let mut p = p.clone();
                p.fragments.retain(|r| r.id() != id);
                p
            })
            .collect();

        s.session.stage(StagedOp::DeleteFragment {
            layer,
            id: id.to_string(),
        })?;
        let mut emptied = Vec::new();
        for p in cleaned {
            if p.fragments.is_empty() {
                emptied.push(p.name.clone());
            }
            let player = s.session.profile_layer(&p.name).unwrap_or(Layer::Global);
            s.session.stage(StagedOp::EditProfile {
                layer: player,
                name: p.name.clone(),
                profile: Box::new(p),
            })?;
        }
        Ok((affected, emptied))
    })();

    let (affected, emptied) = match res {
        Ok(v) => v,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };

    let mut msg = format!("staged deletion of “{id}”");
    if !affected.is_empty() {
        let names = affected
            .iter()
            .map(|n| format!("“{n}”"))
            .collect::<Vec<_>>()
            .join(", ");
        let those = if affected.len() == 1 {
            "profile"
        } else {
            "profiles"
        };
        msg.push_str(&format!(" — and removed it from {those} {names}"));
        if !emptied.is_empty() {
            let e = emptied
                .iter()
                .map(|n| format!("“{n}”"))
                .collect::<Vec<_>>()
                .join(", ");
            let now_has = if emptied.len() == 1 {
                "now has no fragments"
            } else {
                "now have no fragments"
            };
            msg.push_str(&format!(" ({e} {now_has})"));
        }
    }

    match library_now(state) {
        Ok(lib) => Resp::html(views::fragment_result(&lib, &msg)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

// --- custom targets ----------------------------------------------------------

fn handle_target_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (id, action) = id_and_action(&req.path, "/targets/");
    match (req.method.as_str(), action) {
        ("GET", "edit") => handle_target_edit(state, &id),
        ("DELETE", "") => handle_target_delete(state, &id),
        _ => Resp::not_found(),
    }
}

fn handle_workflow_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    state.lock().unwrap().active_tab = "workflows".to_string();
    let (id, action) = id_and_action(&req.path, "/workflows/");
    match (req.method.as_str(), action) {
        // Focus a workflow's slots in the gallery (read).
        ("GET", "") => {
            let snap = state.lock().unwrap().snapshot();
            Resp::html(views::workflows_tab_fragment(&state::workflows_view(
                &snap,
                Some(&id),
            )))
        }
        // Edit an owned workflow in place.
        ("GET", "edit") => handle_workflow_open(state, &id, false),
        // Duplicate a workflow into a new, editable copy (original untouched).
        ("GET", "customize") => handle_workflow_open(state, &id, true),
        // Stage removal of an owned workflow.
        ("DELETE", "") => handle_workflow_delete(state, &id),
        _ => Resp::not_found(),
    }
}

/// Open the editor for an existing workflow — to edit an owned one in place, or
/// (`customize`) to duplicate it into a new, editable copy.
fn handle_workflow_open(state: &Arc<Mutex<StudioState>>, id: &str, customize: bool) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    match cfg.effective_workflows().iter().find(|w| w.id == id) {
        Some(w) => Resp::html(views::workflow_editor(Some(w), customize)),
        None => Resp::html(views::error_fragment(&format!("unknown workflow '{id}'"))),
    }
}

/// Stage a create (new id) or edit/adopt (existing/built-in id) of an owned
/// workflow from the editor form, then re-render the tab focused on it.
fn handle_workflow_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    // Errors from this modal form render INSIDE the editor (its `#wf-editor-msg`
    // slot), not by replacing the tab behind the still-open modal.
    let err = |msg: String| Resp::html_retarget(views::error_fragment(&msg), "#wf-editor-msg");

    let pairs = state::parse_pairs(&req.body);
    let is_new = field(&req.body, "mode") == "new";
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return err(e.to_string()),
    };
    // The workflow this one starts from: `from` names it (the built-in being
    // customized, or the owned one being edited). It supplies the carried-over
    // handoffs + provenance, and for a customize it differs from the new id.
    let effective = cfg.effective_workflows();
    let from = field(&req.body, "from");
    let base = (!from.is_empty())
        .then(|| effective.iter().find(|w| w.id == from))
        .flatten();
    let workflow = match state::workflow_from_form(base, &pairs) {
        Ok(w) => w,
        Err(e) => return err(e.to_string()),
    };
    let id = workflow.id.clone();

    let op = if is_new {
        // A new workflow can't claim an id already in the catalog (owned or
        // built-in) — point the user at Customize for those instead.
        if effective.iter().any(|w| w.id == id) {
            return err(format!(
                "a workflow “{id}” already exists — open it and Customize instead"
            ));
        }
        StagedOp::CreateWorkflow {
            layer: Layer::Global,
            workflow: Box::new(workflow),
        }
    } else {
        StagedOp::EditWorkflow {
            layer: Layer::Global,
            id: id.clone(),
            workflow: Box::new(workflow),
        }
    };

    if let Err(e) = state.lock().unwrap().session.stage(op) {
        return err(e.to_string());
    }
    // Success: re-render the tab focused on the new workflow and close the modal.
    let snap = state.lock().unwrap().snapshot();
    let mut resp = Resp::html(views::workflows_result(
        &state::workflows_view(&snap, Some(&id)),
        &format!("staged workflow “{id}” — Apply to save"),
    ));
    resp.body
        .extend_from_slice(views::modal_close_loader().as_bytes());
    resp
}

/// Stage removal of an owned workflow (built-ins can't be deleted).
fn handle_workflow_delete(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let res = (|| -> crate::Result<()> {
        let mut s = state.lock().unwrap();
        let Some(layer) = s.session.workflow_layer(id) else {
            return Err(anyhow!(
                "“{id}” isn't one of your own workflows — built-ins can't be deleted"
            ));
        };
        s.session.stage(StagedOp::DeleteWorkflow {
            layer,
            id: id.to_string(),
        })
    })();
    if let Err(e) = res {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    let snap = state.lock().unwrap().snapshot();
    Resp::html(views::workflows_result(
        &state::workflows_view(&snap, None),
        &format!("staged removal of workflow “{id}”"),
    ))
}

/// Re-render the Targets tab (with a flash) after a staged change.
fn target_result(state: &Arc<Mutex<StudioState>>, flash: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    Resp::html(views::target_result(&state::targets_view(&snap), flash))
}

fn handle_target_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    // Existing target when editing (its id is fixed); None for a new one.
    let base = state::editor_target_id(&pairs)
        .and_then(|id| cfg.targets.iter().find(|t| t.id == id).cloned());
    let target = match state::target_from_form(base.as_ref(), &pairs) {
        Ok(t) => t,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if base.is_none() {
        // A new target can't claim a built-in/reserved id, nor an existing id.
        if crate::target::reserved_target_ids().contains(&target.id) {
            return Resp::html(views::error_fragment(&format!(
                "“{}” is a built-in target — choose a different name",
                target.id
            )));
        }
        if cfg.targets.iter().any(|t| t.id == target.id) {
            return Resp::html(views::error_fragment(&format!(
                "a target “{}” already exists",
                target.id
            )));
        }
    }
    let layer = state::layer_from_form(&pairs);
    let id = target.id.clone();
    let res = state.lock().unwrap().session.stage(StagedOp::EditTarget {
        layer,
        id: id.clone(),
        target: Box::new(target),
    });
    if let Err(e) = res {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    target_result(state, &format!("staged target “{id}”"))
}

/// Run a draft script predicate once against the repo (cwd = repo base), so the
/// editor can show what it does before saving. Nothing is staged or cached.
fn handle_target_try(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let command = field(&req.body, "command");
    if command.trim().is_empty() {
        return Resp::html(views::script_tryout_empty());
    }
    let lang = field(&req.body, "script_lang");
    let lang = (!lang.is_empty()).then_some(lang.as_str());
    let repo_base = state
        .lock()
        .unwrap()
        .snapshot()
        .base_context
        .repo_base
        .clone();
    let out = crate::providers::run_once_in(&command, lang, &repo_base);
    Resp::html(views::script_tryout(&out))
}

fn handle_target_edit(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    match cfg.targets.iter().find(|t| t.id == id) {
        Some(t) => Resp::html(views::target_dialog(Some(t), t.origin)),
        None => Resp::html(views::error_fragment(&format!("unknown target '{id}'"))),
    }
}

fn handle_target_delete(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let res = (|| -> crate::Result<()> {
        let mut s = state.lock().unwrap();
        let Some(layer) = s.session.target_layer(id) else {
            return Err(anyhow!(
                "“{id}” isn't one of your custom targets — built-ins can't be deleted"
            ));
        };
        s.session.stage(StagedOp::DeleteTarget {
            layer,
            id: id.to_string(),
        })
    })();
    if let Err(e) = res {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    target_result(state, &format!("staged removal of target “{id}”"))
}

fn handle_fragment_duplicate(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let res = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::DuplicatePaletteItem {
            id: id.to_string(),
            to_layer: Layer::Global,
        });
    match res.and_then(|()| library_now(state)) {
        Ok(lib) => Resp::html(views::fragment_result(
            &lib,
            &format!("duplicated “{id}” into your library"),
        )),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

// --- profile handlers --------------------------------------------------------

fn handle_profile_new(state: &Arc<Mutex<StudioState>>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let draft = state::draft_profile_from_form(&[]);
    let preview = profile_preview_or_empty(&snap, &draft, "");
    Resp::html(views::profile_editor(
        &draft, true, None, &lib, &preview, None,
    ))
}

// --- starter packs -----------------------------------------------------------

/// `GET /packs` — the starter-pack gallery (swapped into `#main`). Reachable from
/// the welcome screen and the Profiles tab's "Starter packs" button.
fn handle_packs(state: &Arc<Mutex<StudioState>>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    match state::pack_views(&snap) {
        Ok(packs) => Resp::html(views::packs_gallery_fragment(&packs)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_pack_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (id, action) = id_and_action(&req.path, "/packs/");
    match (req.method.as_str(), action) {
        ("POST", "apply") => handle_pack_apply(state, &id),
        ("GET", "preview") => handle_pack_preview(state, &id),
        _ => Resp::not_found(),
    }
}

/// `GET /packs/<id>/preview` — render the pack's profile as a full document
/// (each fragment demarcated) in a modal, before applying.
fn handle_pack_preview(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let Some(pack) = crate::pack::packs().into_iter().find(|p| p.id == id) else {
        return Resp::html(views::error_fragment(&format!("unknown pack '{id}'")));
    };
    let snap = state.lock().unwrap().snapshot();
    match state::render_pack(&snap, &pack, "", DynamicMode::ReadOnly) {
        Ok(out) => Resp::html(views::pack_preview(&pack, &out)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// `POST /packs/<id>/apply` — stage a pack (duplicate its caps + create its
/// profile), then show the Profiles tab with the new profile selected.
fn handle_pack_apply(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    match crate::pack::packs().into_iter().find(|p| p.id == id) {
        Some(p) => apply_pack_and_show(state, &p),
        None => Resp::html(views::error_fragment(&format!("unknown pack '{id}'"))),
    }
}

/// First-launch quick start: apply the pack recommended for the detected context.
/// Falls back to the full gallery when nothing is recommended (e.g. a repo whose
/// stack loadout doesn't recognize).
fn handle_quickstart(state: &Arc<Mutex<StudioState>>) -> Resp {
    let pack = {
        let snap = state.lock().unwrap().snapshot();
        state::recommended_pack(&snap)
    };
    match pack {
        Some(p) => apply_pack_and_show(state, &p),
        None => handle_packs(state),
    }
}

// --- agent skill card ----------------------------------------------------------

/// Derive the skill card's state from the real filesystem + decision store,
/// aggregated over every shipped skill (the card offers them as one bundle).
/// Deliberately not part of the studio snapshot: skill install is a direct,
/// immediate action on `~/.agents/skills`, not a staged config edit.
fn skill_card_state() -> views::SkillCardState {
    use crate::skills::SkillState;
    let Some(home) = crate::config::home_dir() else {
        return views::SkillCardState::HandsOff;
    };
    let states: Vec<SkillState> = crate::skills::all()
        .iter()
        .map(|s| crate::skills::status(&home, s).state)
        .collect();
    if states.contains(&SkillState::NotInstalled) {
        return views::SkillCardState::Offer;
    }
    if states.iter().any(|s| {
        matches!(
            s,
            SkillState::Managed {
                user_modified: false,
                upgrade_available: true,
                ..
            }
        )
    }) {
        return views::SkillCardState::UpgradeAvailable;
    }
    if states.iter().all(|s| {
        matches!(
            s,
            SkillState::Managed {
                user_modified: false,
                ..
            }
        )
    }) {
        return views::SkillCardState::Installed;
    }
    views::SkillCardState::HandsOff
}

fn skill_ids() -> Vec<&'static str> {
    crate::skills::all().iter().map(|s| s.id).collect()
}

/// `GET /skills/card` — the lazily-loaded agent-skill card on the welcome screen.
fn handle_skill_card() -> Resp {
    Resp::html(views::skill_card(&skill_ids(), &skill_card_state()))
}

/// `POST /skills/install` — install (or upgrade) every embedded skill NOW and
/// record the accepted decisions, then re-render the card. Gated client-side by
/// `hx-confirm`; this is studio's one immediate-side-effect action (everything
/// config-shaped stays in the staged session).
fn handle_skill_install() -> Resp {
    let Some(home) = crate::config::home_dir() else {
        return Resp::html(views::error_fragment("cannot resolve $HOME"));
    };
    for skill in crate::skills::all() {
        if let Err(e) = crate::skills::install(&home, skill) {
            return Resp::html(views::error_fragment(&format!(
                "skill install failed for '{}': {e:#}",
                skill.id
            )));
        }
        if let Err(e) =
            crate::binding::write_skill_decision(skill.id, crate::binding::SkillDecision::Accepted)
        {
            return Resp::html(views::error_fragment(&format!(
                "skills installed, but recording the decision failed: {e:#}"
            )));
        }
    }
    Resp::html(views::skill_card(&skill_ids(), &skill_card_state()))
}

/// `GET /onboarding/welcome` — (re)show the first-launch welcome on demand (the
/// "?" tour button). Arms the guided flow so applying a pack from here runs the
/// review → you're-set beats, just like a fresh config.
fn handle_onboarding_welcome(state: &Arc<Mutex<StudioState>>) -> Resp {
    state.lock().unwrap().onboarding_active = true;
    let snap = state.lock().unwrap().snapshot();
    let onboarding = state::onboarding(&snap.base_context);
    let packs = state::pack_views(&snap).unwrap_or_default();
    Resp::html(views::welcome_fragment(&onboarding, &packs))
}

/// Stage `pack` into the session, then either run the guided "review" beat (when
/// the onboarding flow is armed) or drop into the Profiles tab with the new
/// profile selected. Shared by the gallery's per-pack Apply and the
/// recommended-pack quick start.
fn apply_pack_and_show(state: &Arc<Mutex<StudioState>>, pack: &Pack) -> Resp {
    let armed = {
        let mut s = state.lock().unwrap();
        if let Err(e) = state::apply_pack(&mut s.session, pack) {
            return Resp::html(views::error_fragment(&e.to_string()));
        }
        s.onboarding_active
    };
    // Guided first-run: show the friendly "review what will change" beat instead
    // of dumping into the Profiles tab. Refresh the staged count and close the
    // preview modal if the pack was applied from it.
    if armed {
        let summary = {
            let s = state.lock().unwrap();
            state::staged_summary(&s.session)
        };
        let mut html = views::onboarding_review(&summary);
        html.push_str(&views::staged_indicator_loader());
        html.push_str(&views::modal_close_loader());
        return Resp::html(html);
    }
    let flash = format!("staged the “{}” pack — review and Apply", pack.name);
    let mut resp = profiles_tab_resp(state, Some(pack.profile_name), Some(&flash), true);
    // Applying may have been triggered from the preview modal — close it. (A
    // harmless no-op when applied from the gallery card, where #modal is empty.)
    if resp.status == 200 {
        resp.body
            .extend_from_slice(views::modal_close_loader().as_bytes());
    }
    resp
}

fn handle_profile_edit(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    match cfg.profiles.iter().find(|p| p.name == name) {
        Some(p) => {
            let preview = profile_preview_or_empty(&snap, p, "");
            Resp::html(views::profile_editor(
                p,
                false,
                Some(p.name.as_str()),
                &lib,
                &preview,
                None,
            ))
        }
        None => Resp::html(views::error_fragment(&format!("unknown loadout '{name}'"))),
    }
}

fn handle_profile_select(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    handle_profile_detail(
        state,
        name,
        "",
        DynamicMode::ReadOnly,
        views::Expand::None,
        None,
    )
}

fn handle_profile_disable(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let res = {
        let mut s = state.lock().unwrap();
        let snap = s.snapshot();
        let cfg = match state::staged_config(&snap) {
            Ok(c) => c,
            Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
        };
        match cfg.profiles.iter().find(|p| p.name == name) {
            Some(p) => {
                let mut next = p.clone();
                next.disabled = !next.disabled;
                let layer = s.session.profile_layer(name).unwrap_or(Layer::Global);
                s.session.stage(StagedOp::EditProfile {
                    layer,
                    name: name.to_string(),
                    profile: Box::new(next),
                })
            }
            None => return Resp::html(views::error_fragment(&format!("unknown loadout '{name}'"))),
        }
    };
    match res {
        Ok(()) => profiles_tab_resp(state, Some(name), Some(&format!("toggled “{name}”")), true),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// The original profile name carried by the editor form (the upsert key for a
/// rename); `None` for a new profile or an empty/whitespace field.
fn original_profile_name(pairs: &[(String, String)]) -> Option<&str> {
    pairs
        .iter()
        .find(|(k, _)| k == "original_name")
        .map(|(_, v)| v.trim())
        .filter(|s| !s.is_empty())
}

fn handle_profile_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let profile = match state::profile_from_form(&pairs) {
        // A profile needs a name (and ≥1 fragment). On failure, stay in the
        // editor with the draft preserved and the reason shown inline — never
        // replace the whole form with a bare error.
        Ok(p) => p,
        Err(e) => return profile_editor_with_error(state, &pairs, &e.to_string()),
    };
    let name = profile.name.clone();
    // The upsert key: the original name when editing (so a rename finds and
    // replaces the right profile in place), else the new name (a fresh create).
    let original = original_profile_name(&pairs);
    // The name must be free unless we're keeping it (editing the same profile
    // under its current name). Creating, or renaming, onto a *different* existing
    // profile would silently clobber it via upsert — refuse, keeping the user in
    // the editor with their draft intact.
    let collides = {
        let snap = state.lock().unwrap().snapshot();
        state::staged_config(&snap)
            .map(|cfg| {
                cfg.profiles
                    .iter()
                    .any(|p| p.name == name && Some(p.name.as_str()) != original)
            })
            .unwrap_or(false)
    };
    if collides {
        return profile_editor_with_error(
            state,
            &pairs,
            &format!("a loadout named “{name}” already exists — choose another name"),
        );
    }
    // Single-default invariant: at most one no-targets default loadout. A new (or
    // renamed) loadout with no targets is refused when another default exists.
    if profile.targets.is_empty() {
        let snap = state.lock().unwrap().snapshot();
        let other_default = state::staged_config(&snap)
            .map(|cfg| {
                cfg.profiles.iter().any(|p| {
                    p.targets.is_empty() && !p.disabled && Some(p.name.as_str()) != original
                })
            })
            .unwrap_or(false);
        if other_default {
            return profile_editor_with_error(
                state,
                &pairs,
                "a default loadout (no targets) already exists — give this one at least one target",
            );
        }
    }
    let key = original.unwrap_or(name.as_str()).to_string();
    // Profiles are global-only — always authored into the global config.
    let res = state.lock().unwrap().session.stage(StagedOp::EditProfile {
        layer: Layer::Global,
        name: key,
        profile: Box::new(profile),
    });
    match res {
        Ok(()) => profiles_tab_resp(
            state,
            Some(&name),
            Some(&format!("staged loadout “{name}”")),
            true,
        ),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Re-render the profile editor with the in-progress draft preserved and an
/// inline error (missing name / no fragments), so a failed save keeps the
/// user in the form rather than dropping them onto a bare error banner.
fn profile_editor_with_error(
    state: &Arc<Mutex<StudioState>>,
    pairs: &[(String, String)],
    error: &str,
) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let draft = state::draft_profile_from_form(pairs);
    let is_new = pairs.iter().any(|(k, v)| k == "new" && v == "1");
    let preview = profile_preview_or_empty(&snap, &draft, "");
    Resp::html(views::profile_editor(
        &draft,
        is_new,
        original_profile_name(pairs),
        &lib,
        &preview,
        Some(error),
    ))
}

/// Live preview for the editor (POST /profiles/preview) — composes the unsaved
/// draft and renders it; never stages.
fn handle_editor_preview(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let draft = state::draft_profile_from_form(&pairs);
    let snap = state.lock().unwrap().snapshot();
    match state::render_profile_config(&snap, &draft, "", DynamicMode::ReadOnly) {
        Ok(p) => Resp::html(views::editor_preview_fragment(&p)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Inline cap create from the profile editor (POST /profiles/draft): stage the
/// new fragment, then re-render the editor with the draft preserved and the
/// new cap added + checked.
fn handle_profile_draft(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let Some((cap, layer)) = state::inline_fragment_from_form(&pairs) else {
        return Resp::html(views::error_fragment(
            "give the new fragment a name before adding it",
        ));
    };
    let new_id = cap.id.clone();
    if let Err(e) = state.lock().unwrap().session.stage(StagedOp::EditFragment {
        layer,
        id: new_id.clone(),
        cap: Box::new(cap),
    }) {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    // Re-render the editor preserving the in-progress profile + the new cap.
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let mut draft = state::draft_profile_from_form(&pairs);
    if !draft.fragments.iter().any(|r| r.id() == new_id) {
        draft
            .fragments
            .push(crate::profile::FragmentRef::Id(new_id));
    }
    let is_new = state::parse_pairs(&req.body)
        .iter()
        .any(|(k, v)| k == "new" && v == "1");
    let preview = profile_preview_or_empty(&snap, &draft, "");
    Resp::html(format!(
        "{}{}",
        views::profile_editor(
            &draft,
            is_new,
            original_profile_name(&pairs),
            &lib,
            &preview,
            None
        ),
        views::staged_indicator_loader(),
    ))
}

fn handle_profile_delete(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let res = {
        let mut s = state.lock().unwrap();
        match s.session.profile_layer(name) {
            Some(layer) => s.session.stage(StagedOp::DeleteProfile {
                layer,
                name: name.to_string(),
            }),
            None => return Resp::html(views::error_fragment(&format!("unknown loadout '{name}'"))),
        }
    };
    match res {
        Ok(()) => profiles_tab_resp(
            state,
            None,
            Some(&format!("staged deletion of “{name}”")),
            true,
        ),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Render a draft/profile for the editor preview, or an empty outcome on error.
fn profile_preview_or_empty(
    snap: &state::Snapshot,
    profile: &crate::profile::LoadoutConfig,
    agent: &str,
) -> PreviewOutcome {
    state::render_profile_config(snap, profile, agent, DynamicMode::ReadOnly).unwrap_or_else(|e| {
        PreviewOutcome {
            agent: agent.to_string(),
            context_summary: String::new(),
            fragment_count: 0,
            overlay: String::new(),
            caps: Vec::new(),
            note: Some(format!("preview error: {e}")),
        }
    })
}

fn handle_diff(state: &Arc<Mutex<StudioState>>) -> Resp {
    let (diffs, texts, staged, fs_changed) = {
        let s = state.lock().unwrap();
        (
            s.session.diff(),
            s.session.staged_layer_texts(),
            s.session.ops().len(),
            s.session.external_edits(),
        )
    };
    // Leak-lint the full staged PUBLIC config (the sync-safety guard, §7) — what
    // would land in the shareable config.toml after apply, not just the diff.
    let mut leaks: Vec<String> = texts
        .iter()
        .filter(|(layer, _, _)| matches!(layer, Layer::Repo | Layer::Global))
        .flat_map(|(_, _, text)| crate::lint::find_in_text(text))
        .collect();
    leaks.sort();
    leaks.dedup();
    Resp::html(views::diff_view(&diffs, &leaks, &fs_changed, staged))
}

/// `POST /fragments/try` — run the *draft* script from the editor form right now
/// and return its output as a panel. Reuses the render-time executor so what you
/// see is what would be embedded. Stateless: nothing is staged, cached, or
/// written. A manual test always executes (the `allow_exec` toggle only governs
/// automatic render-time execution). CSRF-guarded like every other POST.
fn handle_fragment_try(req: &Req) -> Resp {
    let command = field(&req.body, "command");
    if command.trim().is_empty() {
        return Resp::html(views::script_tryout_empty());
    }
    let lang = field(&req.body, "script_lang");
    let lang = (!lang.is_empty()).then_some(lang.as_str());
    let out = crate::providers::run_once(&command, lang);
    Resp::html(views::script_tryout(&out))
}

fn handle_discard(state: &Arc<Mutex<StudioState>>) -> Resp {
    if let Err(e) = state.lock().unwrap().session.discard() {
        return Resp::html(views::error_fragment(&format!("discard failed: {e}")));
    }
    profiles_tab_resp(state, None, Some("discarded staged changes"), true)
}

fn handle_apply(state: &Arc<Mutex<StudioState>>) -> Resp {
    // Capture the guided-onboarding summary *before* applying (apply clears the
    // staged ops), so the "you're set" beat can name what just landed.
    let onboarding = {
        let s = state.lock().unwrap();
        s.onboarding_active
            .then(|| state::staged_summary(&s.session))
    };
    // Apply mutates + writes atomically; it's the one serialized operation, so
    // holding the lock across its (brief, small-file) I/O is correct here.
    let result = state.lock().unwrap().session.apply();
    match result {
        Ok(written) => {
            // Guided first-run: a profile actually landed → show the "you're set"
            // finish card (names `load run <agent>`), then disarm the flow. If
            // nothing composed a profile, fall through to the normal flash.
            if let Some(summary) = onboarding {
                state.lock().unwrap().onboarding_active = false;
                if !summary.profiles.is_empty() {
                    let agent = config::Config::load(&state.lock().unwrap().repo_base)
                        .map(|c| c.default_agent)
                        .unwrap_or_else(|_| "claude".to_string());
                    let mut html = views::onboarding_done(&summary, &agent);
                    html.push_str(&views::staged_indicator_loader());
                    return Resp::html(html);
                }
            }
            let mut msg = format!("applied {} file change(s)", written.len());
            // Best-effort auto-push of the (now-changed) global config so edits
            // propagate to your other machines. Never blocks the apply.
            if let Some(note) = auto_push_after_apply(state) {
                msg.push_str(" · ");
                msg.push_str(&note);
            }
            // Stay on the Workflows tab when that's where the user is (selecting a
            // workflow shouldn't bounce them to Profiles); other tabs land on
            // Profiles as before.
            if state.lock().unwrap().active_tab == "workflows" {
                let snap = state.lock().unwrap().snapshot();
                let mut html =
                    views::workflows_tab(&state::workflows_view(&snap, None), Some(&msg))
                        .into_string();
                html.push_str(&views::staged_indicator_loader());
                return Resp::html(html);
            }
            profiles_tab_resp(state, None, Some(&msg), true)
        }
        Err(e) => Resp::html(views::error_fragment(&format!("apply failed: {e}"))),
    }
}

/// Commit + push the global config after a studio apply, if `[sync] auto_push`
/// is on and the config dir is a synced repo. Returns a short status to append
/// to the flash (or `None` when sync isn't configured / nothing to push).
fn auto_push_after_apply(state: &Arc<Mutex<StudioState>>) -> Option<String> {
    let repo_base = state.lock().unwrap().repo_base.clone();
    let cfg = crate::config::Config::load(&repo_base).ok()?;
    if !cfg.sync.auto_push {
        return None;
    }
    let dir = crate::sync::config_dir().ok()?;
    if !crate::sync::is_synced(&dir) {
        return None;
    }
    match crate::sync::commit_push(&dir, "load studio: edit config", cfg.sync.timeout) {
        Ok(crate::sync::PushOutcome::Pushed) => Some("synced ✓".to_string()),
        Ok(crate::sync::PushOutcome::NothingToPush) => None,
        Ok(crate::sync::PushOutcome::Diverged) => {
            Some("remote moved ahead — run `load sync`".to_string())
        }
        Err(_) => Some("saved locally, push pending".to_string()),
    }
}

// --- security guards ---------------------------------------------------------

fn host_ok(req: &Req, port: u16) -> bool {
    match req.headers.get("host") {
        Some(h) => h == &format!("127.0.0.1:{port}") || h == &format!("localhost:{port}"),
        None => false,
    }
}

fn cookie_token(req: &Req) -> Option<String> {
    let cookies = req.headers.get("cookie")?;
    cookies.split(';').find_map(|kv| {
        kv.trim()
            .strip_prefix("loadout_studio=")
            .map(|v| v.to_string())
    })
}

fn origin_ok(req: &Req, port: u16) -> bool {
    let allowed = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];
    if let Some(o) = req.headers.get("origin") {
        return allowed.iter().any(|a| a == o);
    }
    if let Some(r) = req.headers.get("referer") {
        return allowed.iter().any(|a| r.starts_with(a.as_str()));
    }
    false // require an explicit, matching Origin/Referer on state-changing calls
}

fn bootstrap(req: &Req, token: &str) -> Resp {
    let provided = state::parse_pairs(&req.query)
        .into_iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v);
    if provided.as_deref() == Some(token) {
        // Token stays out of history/Referer: set the cookie, redirect to `/`.
        let cookie = format!("loadout_studio={token}; HttpOnly; SameSite=Strict; Path=/");
        Resp::redirect("/", Some(&cookie))
    } else {
        Resp::forbidden("invalid or missing bootstrap token")
    }
}

// --- socket loop -------------------------------------------------------------

/// Entry point for `load studio`: bind, print the bootstrap URL, open the
/// browser (unless `--no-open`), and serve until the process is interrupted.
pub fn serve(rt: &Runtime, args: &StudioArgs) -> crate::Result<()> {
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    // Passive hook bootstrap: studio is many users' first loadout command, so
    // wire the IDE freshness hooks of installed agents (e.g. Cursor) here too.
    for note in crate::adapters::bootstrap_hook_registrations(&config, rt.dry_run) {
        println!("load studio → {note}");
    }
    let base_context = context::detect_context(&rt.cwd, &config).context("detecting context")?;
    let global_dir = config::global_config_dir();
    let session = Session::open(&repo_base, global_dir.as_deref())?;
    let token = make_token()?;

    let server = match tiny_http::Server::http(("127.0.0.1", args.port)) {
        Ok(s) => s,
        Err(e) => {
            // The port is taken. If a load studio is already serving there,
            // re-attach to it (recover its session token) and just open the
            // browser instead of failing. Anything else → the original error.
            if let Some(token) = try_attach_running(args.port) {
                let url = format!(
                    "http://127.0.0.1:{}{BOOTSTRAP_PATH}?token={token}",
                    args.port
                );
                println!(
                    "load studio → already running on 127.0.0.1:{}; re-using it",
                    args.port
                );
                println!("load studio → open  {url}");
                println!("(restart that instance to pick up config changes since it started)");
                if !args.no_open {
                    open_browser(&url);
                }
                return Ok(());
            }
            return Err(anyhow!("binding 127.0.0.1:{}: {e}", args.port));
        }
    };
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(args.port);

    let state = Arc::new(Mutex::new(StudioState {
        session,
        base_context,
        repo_base,
        token: token.clone(),
        port,
        onboarding_active: false,
        active_tab: "profiles".to_string(),
    }));

    // `0`/`0s` disables the idle shutdown; anything else is the inactivity window.
    let idle = crate::providers::parse_duration(&args.idle_timeout).ok_or_else(|| {
        anyhow!(
            "invalid --idle-timeout '{}': use e.g. 30m, 90s, 2h, or 0 to disable",
            args.idle_timeout
        )
    })?;

    // Record where we're serving (port + token) so a second `load studio` on
    // this port can recover the token and re-open the browser instead of failing
    // to bind. Best-effort: if it can't be written, we just lose that nicety.
    write_runtime_file(port, &token);

    let url = format!("http://127.0.0.1:{port}{BOOTSTRAP_PATH}?token={token}");
    println!("load studio → open  {url}");
    if idle.is_zero() {
        println!("(serving on 127.0.0.1:{port}; Ctrl-C to stop)");
    } else {
        println!(
            "(serving on 127.0.0.1:{port}; Ctrl-C to stop, or auto-exit after {} idle)",
            args.idle_timeout
        );
    }
    if !args.no_open {
        open_browser(&url);
    }

    serve_loop(&server, &state, idle);
    remove_runtime_file(port);
    Ok(())
}

// --- runtime file: re-attach to an already-running instance ------------------

/// A studio instance's coordinates, written on launch and read by a second
/// `load studio` so it can re-open the browser into the running instance
/// rather than dying on a port-in-use bind error.
#[derive(serde::Serialize, serde::Deserialize)]
struct StudioRuntime {
    port: u16,
    token: String,
    pid: i32,
}

/// Per-port runtime file. Lives under the global config dir (`…/loadout/run`),
/// falling back to the OS temp dir, and is keyed by port so two instances on
/// different ports don't collide.
fn studio_runtime_path(port: u16) -> PathBuf {
    let dir = config::global_config_dir()
        .map(|d| d.join("run"))
        .unwrap_or_else(|| std::env::temp_dir().join("loadout").join("run"));
    dir.join(format!("studio-{port}.json"))
}

/// Best-effort: record this instance's port + token in a user-only (0600) file.
fn write_runtime_file(port: u16, token: &str) {
    write_runtime_to(&studio_runtime_path(port), port, token);
}

/// The write half, with the path injected so tests don't touch the real config
/// dir or env.
fn write_runtime_to(path: &Path, port: u16, token: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let rt = StudioRuntime {
        port,
        token: token.to_string(),
        pid: std::process::id() as i32,
    };
    let Ok(json) = serde_json::to_string(&rt) else {
        return;
    };
    // 0600 so the token (which gates the studio) isn't world-readable. It's no
    // more exposed than the URL already printed to the terminal.
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    else {
        return;
    };
    // `mode()` only applies when the file is *created*; an existing file keeps
    // its old permissions through an overwrite. Re-assert 0600 on the open
    // handle before writing so the token never lands behind looser perms.
    if f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .is_err()
    {
        return;
    }
    let _ = f.write_all(json.as_bytes());
}

/// Best-effort cleanup on clean exit. A leaked file (e.g. after Ctrl-C) is
/// harmless: [`try_attach_running`] re-validates against the live server before
/// trusting it.
fn remove_runtime_file(port: u16) {
    let _ = std::fs::remove_file(studio_runtime_path(port));
}

/// If a load studio is already serving on `port`, return its session token.
/// Returns `None` when there's no runtime file, it's stale, or the server on
/// that port doesn't answer as loadout — so the caller falls back to the normal
/// bind error rather than opening a browser at some unrelated process.
fn try_attach_running(port: u16) -> Option<String> {
    if port == 0 {
        return None; // `0` means "OS picks a free port" — never a fixed target.
    }
    attach_from(&studio_runtime_path(port), port)
}

/// The read + validate half, with the path injected for testing.
fn attach_from(path: &Path, port: u16) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let rt: StudioRuntime = serde_json::from_str(&data).ok()?;
    if rt.port != port {
        return None;
    }
    if probe_studio(port, &rt.token) {
        Some(rt.token)
    } else {
        None
    }
}

/// Confirm a live load studio on `port` accepts `token`: hit the bootstrap
/// route and check for its signature (a 302 that sets the `loadout_studio`
/// cookie). This doubles as a liveness + identity check, so a stale token or a
/// foreign process on the port both fail closed.
fn probe_studio(port: u16, token: &str) -> bool {
    use std::io::Write as _;
    use std::net::TcpStream;

    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let request = format!(
        "GET {BOOTSTRAP_PATH}?token={token} HTTP/1.0\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Connection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 8192 {
                    break; // bootstrap replies are tiny; cap defensively.
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    // Only inspect the header block (before the blank line), so a foreign server
    // that happens to echo "loadout_studio=" in its body can't be mistaken for us.
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let mut lines = head.lines();
    let status_ok = lines
        .next()
        .is_some_and(|l| l.starts_with("HTTP/") && l.contains(" 302"));
    // A real bootstrap reply sets the session cookie via a Set-Cookie header.
    let sets_cookie = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("set-cookie:") && l.contains("loadout_studio=")
    });
    status_ok && sets_cookie
}

/// The request loop. With a zero `idle` window it blocks until Ctrl-C; otherwise
/// it polls so it can notice inactivity and shut the server down on its own. Any
/// handled request resets the clock — there's no background browser polling, so
/// "no requests" genuinely means the user has stepped away.
fn serve_loop(server: &tiny_http::Server, state: &Arc<Mutex<StudioState>>, idle: Duration) {
    use std::time::Instant;
    let handle = |request: &mut tiny_http::Request| {
        let req = read_request(request);
        route(state, &req)
    };

    if idle.is_zero() {
        for mut request in server.incoming_requests() {
            let resp = handle(&mut request);
            let _ = respond(request, resp);
        }
        return;
    }

    // Wake at least once a minute (and never coarser than the window itself) so a
    // 30-minute timeout fires within ~a minute of the deadline.
    let poll = idle.min(Duration::from_secs(60));
    let mut last = Instant::now();
    loop {
        match server.recv_timeout(poll) {
            Ok(Some(mut request)) => {
                last = Instant::now();
                let resp = handle(&mut request);
                let _ = respond(request, resp);
            }
            Ok(None) => {
                if last.elapsed() >= idle {
                    println!("load studio: idle — shutting down.");
                    return;
                }
            }
            // A receive error means the listener is unusable; stop rather than spin.
            Err(e) => {
                eprintln!("load studio: server error ({e}); shutting down.");
                return;
            }
        }
    }
}

fn read_request(request: &mut tiny_http::Request) -> Req {
    let method = request.method().to_string().to_uppercase();
    let raw = request.url().to_string();
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (raw, String::new()),
    };
    let mut headers = HashMap::new();
    for h in request.headers() {
        headers.insert(
            h.field.to_string().to_ascii_lowercase(),
            h.value.as_str().to_string(),
        );
    }
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    Req {
        method,
        path,
        query,
        headers,
        body,
    }
}

fn respond(request: tiny_http::Request, resp: Resp) -> std::io::Result<()> {
    let mut response = tiny_http::Response::from_data(resp.body).with_status_code(resp.status);
    for (k, v) in &resp.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            response = response.with_header(h);
        }
    }
    request.respond(response)
}

/// A 256-bit session token from the OS CSPRNG (`/dev/urandom`).
///
/// Failure is **fatal** — the server refuses to start rather than ever minting a
/// guessable token (no time/pid fallback). This token gates every request, so a
/// predictable value would defeat the whole localhost auth model. loadout is
/// unix-targeted (unix `exec`, `libc`); a Windows port would read its CSPRNG via
/// `getrandom`/`OsRng` here instead.
fn make_token() -> crate::Result<String> {
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom for the studio session token")?;
    f.read_exact(&mut buf)
        .context("reading /dev/urandom for the studio session token")?;
    Ok(hex(&buf))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub(crate) fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = url;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        // A real git repo → Repo scope → binding reads stay inside the tempdir.
        let _ = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status();
        d
    }

    fn state_for(repo: &std::path::Path, cfg_toml: Option<&str>) -> Arc<Mutex<StudioState>> {
        // Fragments and profiles are global-only, so the fixture's config is
        // authored into a global dir (a subdir of the repo tempdir, cleaned up
        // with it) and the session is opened with that global layer.
        let gdir = repo.join("global");
        std::fs::create_dir_all(&gdir).unwrap();
        if let Some(c) = cfg_toml {
            std::fs::write(gdir.join("config.toml"), c).unwrap();
        }
        let gcfg = gdir.join("config.toml");
        let config = Config::load_from(Some(&gcfg), repo).unwrap();
        let base_context = context::detect_context(repo, &config).unwrap();
        let session = Session::open(repo, Some(&gdir)).unwrap();
        Arc::new(Mutex::new(StudioState {
            session,
            base_context,
            repo_base: repo.to_path_buf(),
            token: "testtoken".into(),
            port: 7777,
            onboarding_active: false,
            active_tab: "profiles".into(),
        }))
    }

    /// The global `config.toml` the fixture authors into — where caps/profiles
    /// land on apply (they are global-only).
    fn global_config_path(repo: &std::path::Path) -> std::path::PathBuf {
        repo.join("global").join("config.toml")
    }

    fn req(method: &str, path: &str, query: &str, headers: &[(&str, &str)], body: &str) -> Req {
        let mut h = HashMap::new();
        for (k, v) in headers {
            h.insert(k.to_ascii_lowercase(), v.to_string());
        }
        Req {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            headers: h,
            body: body.into(),
        }
    }

    const HOST: (&str, &str) = ("Host", "127.0.0.1:7777");
    const COOKIE: (&str, &str) = ("Cookie", "loadout_studio=testtoken");
    const ORIGIN: (&str, &str) = ("Origin", "http://127.0.0.1:7777");

    #[test]
    fn bootstrap_sets_cookie_and_redirects() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(
            &st,
            &req("GET", BOOTSTRAP_PATH, "token=testtoken", &[HOST], ""),
        );
        assert_eq!(r.status, 302);
        let (_, cookie) = r.headers.iter().find(|(k, _)| k == "set-cookie").unwrap();
        assert!(cookie.contains("loadout_studio=testtoken"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn bootstrap_rejects_bad_token() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", BOOTSTRAP_PATH, "token=wrong", &[HOST], ""));
        assert_eq!(r.status, 403);
    }

    #[test]
    fn guards_reject_missing_cookie_and_bad_host() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        // No cookie → forbidden.
        assert_eq!(route(&st, &req("GET", "/", "", &[HOST], "")).status, 403);
        // Bad Host (DNS-rebinding) → forbidden even with a valid cookie.
        assert_eq!(
            route(
                &st,
                &req("GET", "/", "", &[("Host", "evil.test"), COOKIE], "")
            )
            .status,
            403
        );
    }

    #[test]
    fn shell_served_with_cookie() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("Loadout studio"));
        // The shell renders the Loadouts tab by default; the top nav is the two
        // destinations Loadouts | Library.
        assert!(body.contains("Loadouts"));
        assert!(body.contains("Library"));
    }

    #[test]
    fn fresh_config_shows_welcome_onboarding() {
        // No config at all → no profiles and no own caps → the Profiles tab
        // greets a first-time user instead of a bare "no profiles" prompt.
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(
            body.contains("Welcome to Loadout studio"),
            "welcome missing"
        );
        // The welcome embeds the starter-pack gallery; the detected stack's pack
        // is the recommended one with an Apply action.
        assert!(
            body.contains("/packs/rust/apply"),
            "rust pack action missing"
        );
        assert!(body.contains("recommended"), "recommended badge missing");
        // The detection readout names the detected stack as a chip.
        assert!(body.contains(">rust<"), "detected-stack chip missing");
    }

    #[test]
    fn quickstart_applies_recommended_pack() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(
            &st,
            &req(
                "POST",
                "/onboarding/quickstart",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        );
        assert_eq!(r.status, 200);
        // Applied the recommended Rust pack: its 13 fragments are duplicated and
        // its profile is created (13 + 1 staged ops).
        assert_eq!(st.lock().unwrap().session.ops().len(), 14);
        let body = String::from_utf8(r.body).unwrap();
        // The Profiles tab now shows the staged "rust" profile and its caps.
        assert!(body.contains("staged the"), "pack flash missing");
        assert!(body.contains("Rust conventions"));
        assert!(body.contains("Communication style"));
    }

    #[test]
    fn packs_gallery_lists_packs_recommended_first() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/packs", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("Starter packs"));
        assert!(
            body.contains("/packs/rust/apply"),
            "rust pack action missing"
        );
        assert!(
            body.contains("/packs/everyday/apply"),
            "everyday pack action missing"
        );
        // In a Rust repo the Rust pack is recommended → badged + ordered first.
        let rust_at = body.find("/packs/rust/apply").unwrap();
        let everyday_at = body.find("/packs/everyday/apply").unwrap();
        assert!(rust_at < everyday_at, "recommended pack should come first");
        assert!(body.contains("recommended"));
    }

    #[test]
    fn apply_pack_stages_caps_and_profile() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(
            &st,
            &req(
                "POST",
                "/packs/everyday/apply",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        );
        assert_eq!(r.status, 200);
        // The 14 everyday caps are duplicated and the "everyday" profile is created.
        assert_eq!(st.lock().unwrap().session.ops().len(), 15);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("staged the"), "pack flash missing");
        assert!(body.contains("Communication style"));
    }

    #[test]
    fn applying_a_pack_twice_does_not_re_duplicate_caps() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let apply = || {
            route(
                &st,
                &req(
                    "POST",
                    "/packs/everyday/apply",
                    "",
                    &[HOST, COOKIE, ORIGIN],
                    "",
                ),
            )
        };
        assert_eq!(apply().status, 200);
        let after_first = st.lock().unwrap().session.ops().len();
        assert_eq!(apply().status, 200);
        let after_second = st.lock().unwrap().session.ops().len();
        // The second apply owns every cap already, so it re-stages only the
        // profile (EditProfile) — exactly one new op, no re-duplication.
        assert_eq!(after_first, 15);
        assert_eq!(after_second, 16);
    }

    #[test]
    fn targets_tab_lists_builtins_with_rules() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/tab/targets", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("Targets"), "tab heading");
        // Built-in targets and their detection rules are shown.
        assert!(body.contains("nextjs"), "lists the nextjs target");
        assert!(
            body.contains("Cargo.toml exists"),
            "shows rust's detection rule"
        );
        // The synthetic machine scope row is present.
        assert!(body.contains("machine"), "lists the machine scope");
        // In a Rust repo, the rust target matches.
        assert!(body.contains("matches here"), "flags a detected target");
    }

    #[test]
    fn workflows_gallery_focus_and_binding() {
        let d = rust_repo();
        // A loadout pins spec-driven — the per-loadout Workflow slot is the only
        // way to "use" a workflow now (no global active workflow).
        let st = state_for(
            d.path(),
            Some(
                "[[fragments]]\nid = \"rc\"\nguidance = \"Rust.\"\n\n\
                 [[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\nworkflow = \"spec-driven\"\n",
            ),
        );

        // The Library's Workflows category: a gallery of named cards.
        let r = route(&st, &req("GET", "/tab/workflows", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("Workflows"), "tab heading");
        for name in ["Superpowers", "Spec-driven", "Compound engineering"] {
            assert!(body.contains(name), "gallery lists '{name}'");
        }
        assert!(
            body.contains("Every's loop where each cycle makes the next one easier."),
            "card blurb shown"
        );
        // The bound workflow shows an "equipped on a loadout" marker — there's no
        // global "active workflow" concept anymore.
        assert!(
            body.contains("equipped on"),
            "binding marker for spec-driven"
        );
        assert!(
            body.contains(r#"<span class="cmd-name">explore</span>"#),
            "the canonical `explore` slot command is shown"
        );

        // Focusing a card shows the SAME fixed spine filled by ITS stages, with no
        // "Use this workflow" action (the Library is browse/edit only).
        let sp = String::from_utf8(
            route(
                &st,
                &req("GET", "/workflows/superpowers", "", &[HOST, COOKIE], ""),
            )
            .body,
        )
        .unwrap();
        assert!(
            !sp.contains("Use this workflow"),
            "no global activate action"
        );
        for (cmd, name) in [
            ("explore", "Explore"),
            ("brainstorm", "Brainstorm"),
            ("plan", "Plan"),
            ("implement", "Implement"),
            ("verify", "Verify"),
            ("ship", "Ship"),
        ] {
            assert!(
                sp.contains(&format!(r#"<span class="cmd-name">{cmd}</span>"#)),
                "fixed spine shows `{cmd}`"
            );
            assert!(
                sp.contains(&format!(r#"<span class="wf-slot-name">{name}</span>"#)),
                "slot shows its step name `{name}`"
            );
        }
        assert!(
            sp.contains("skipped"),
            "an unfilled canonical slot is skipped"
        );
        assert!(
            sp.contains("wf-value-mark"),
            "workflow icon marks the value"
        );
        assert!(
            sp.contains("Refine the rough idea"),
            "superpowers' take on the brainstorm slot is shown when focused"
        );
        assert!(sp.contains("design.md"), "design.md handoff chip shown");
        assert!(sp.contains("plan.md"), "plan.md handoff chip shown");

        // Focusing the bound workflow shows which loadout pins it.
        let spec = String::from_utf8(
            route(
                &st,
                &req("GET", "/workflows/spec-driven", "", &[HOST, COOKIE], ""),
            )
            .body,
        )
        .unwrap();
        assert!(
            spec.contains("Pinned on loadout") && spec.contains("rust"),
            "the bound workflow names its loadout"
        );
        // The deprecated global activate route is gone.
        assert_eq!(
            route(
                &st,
                &req(
                    "POST",
                    "/workflows/superpowers/activate",
                    "",
                    &[HOST, COOKIE, ORIGIN],
                    ""
                )
            )
            .status,
            404
        );
    }

    #[test]
    fn create_custom_target_stages_and_applies() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        // Author a "deno" target via the editor form, with a chosen glyph icon.
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/targets",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=Deno&kind=file_exists&paths=deno.json&icon=database&visibility=public",
            ),
        ));
        assert!(r.contains("staged target"), "save flash: got {r}");
        assert!(r.contains("deno"), "the new target shows in the tab");
        // Apply it; the [[targets]] entry (incl. the icon) lands in the config.
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            on_disk.contains("id = \"deno\""),
            "target written: {on_disk}"
        );
        assert!(on_disk.contains("deno.json"), "rule written");
        assert!(
            on_disk.contains("icon = \"database\""),
            "icon round-trips: {on_disk}"
        );
    }

    #[test]
    fn create_script_target_stages_and_applies() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/targets",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=Bazel&kind=script&command=test+-f+WORKSPACE&script_lang=bash&allow_exec=on&visibility=public",
            ),
        ));
        assert!(r.contains("staged target"), "save flash: {r}");
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            on_disk.contains("id = \"bazel\""),
            "target written: {on_disk}"
        );
        assert!(on_disk.contains("kind = \"script\""), "script rule written");
        assert!(on_disk.contains("WORKSPACE"), "command written");
    }

    #[test]
    fn create_customize_and_delete_workflow() {
        let d = rust_repo();
        let st = state_for(d.path(), None);

        // Build your own: each step is just markdown (one purpose box per slot).
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/workflows",
                "",
                &[HOST, COOKIE, ORIGIN],
                "mode=new&from=&name=My+Flow&description=mine&icon=bolt\
                 &s_plan_purpose=Think+first&s_implement_purpose=Build+it",
            ),
        ));
        assert!(r.contains("staged workflow"), "save flash: {r}");
        assert!(r.contains("My Flow"), "new workflow shows in the gallery");

        // Apply: a clean [[workflows]] with [[workflows.stages]] sub-tables lands.
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            on_disk.contains("id = \"my-flow\""),
            "workflow written: {on_disk}"
        );
        assert!(
            on_disk.contains("[[workflows.stages]]"),
            "stages as sub-tables"
        );

        // The owned workflow's detail offers a Delete; a built-in's never does.
        let mine = body_of(route(
            &st,
            &req("GET", "/workflows/my-flow", "", &[HOST, COOKIE], ""),
        ));
        assert!(
            mine.contains(r#"hx-delete="/workflows/my-flow""#),
            "custom workflow shows a Delete action"
        );
        let builtin = body_of(route(
            &st,
            &req("GET", "/workflows/spec-driven", "", &[HOST, COOKIE], ""),
        ));
        assert!(
            !builtin.contains(r#"hx-delete="/workflows/spec-driven""#),
            "a built-in is never deletable"
        );

        // Customize a built-in: its editor opens as "Customize" and creates a
        // SEPARATE copy under a new id (the built-in `spec-driven` is left intact).
        let ed = body_of(route(
            &st,
            &req(
                "GET",
                "/workflows/spec-driven/customize",
                "",
                &[HOST, COOKIE],
                "",
            ),
        ));
        assert!(
            ed.contains("Customize Spec-driven"),
            "built-in opens as Customize"
        );
        let saved = body_of(route(
            &st,
            &req(
                "POST",
                "/workflows",
                "",
                &[HOST, COOKIE, ORIGIN],
                // New id ("Spec-driven copy" → spec-driven-copy); `from=spec-driven`
                // carries the handoffs over. Only the prose changes.
                "mode=new&from=spec-driven&name=Spec-driven+copy\
                 &s_brainstorm_purpose=Spec+first\
                 &s_plan_purpose=My+own+take+on+planning\
                 &s_implement_purpose=Build+it&s_verify_purpose=Check+it",
            ),
        ));
        assert!(
            saved.contains("staged workflow"),
            "customize staged: {saved}"
        );

        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            on_disk.contains("id = \"spec-driven-copy\""),
            "copy created under a new id: {on_disk}"
        );
        assert!(
            on_disk.contains("writes = \"plan.md\""),
            "plan's handoff carried over from spec-driven without re-entry"
        );

        // Delete the owned workflow (built-ins refuse).
        let del = body_of(route(
            &st,
            &req(
                "DELETE",
                "/workflows/my-flow",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(del.contains("staged removal"), "delete flash: {del}");
        let nope = body_of(route(
            &st,
            &req(
                "DELETE",
                "/workflows/spec-driven",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(
            nope.contains("built-ins can't be deleted"),
            "built-in delete refused"
        );
    }

    #[test]
    fn empty_workflow_save_errors_inside_the_modal() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        // A new workflow with a name but no step content is invalid (no stages).
        let r = route(
            &st,
            &req(
                "POST",
                "/workflows",
                "",
                &[HOST, COOKIE, ORIGIN],
                "mode=new&from=&name=Empty+Flow",
            ),
        );
        let body = String::from_utf8(r.body.clone()).unwrap();
        assert!(
            body.contains("at least one step"),
            "friendly validation error shown: {body}"
        );
        // HX-Retarget routes the error into the editor's slot, not #main behind it.
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("HX-Retarget") && v == "#wf-editor-msg"),
            "error retargeted into the modal, got headers {:?}",
            r.headers
        );
    }

    #[test]
    fn reserved_target_id_is_rejected() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/targets",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=rust&kind=file_exists&paths=x",
            ),
        ));
        assert!(
            r.contains("built-in target"),
            "must refuse a built-in id: {r}"
        );
    }

    #[test]
    fn profile_editor_lists_all_builtin_and_custom_targets() {
        // A config with a custom `deno` target. The profile editor's target
        // checklist is derived from the catalog, so it must offer every built-in
        // (regression: `bun` was missing from a hardcoded list) plus the custom
        // target plus the `machine` scope.
        let cfg =
            "[[targets]]\nid = \"deno\"\nrule = { kind = \"file_exists\", path = \"deno.json\" }\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/new", "", &[HOST, COOKIE], ""),
        ));
        for id in [
            "rust", "node", "bun", "nextjs", "go", "python", "java", "ruby", "php", "swift",
            "dotnet", "machine", "deno",
        ] {
            assert!(
                body.contains(&format!("value=\"{id}\"")),
                "editor target list must offer `{id}`: {body}"
            );
        }
    }

    #[test]
    fn welcome_hidden_once_a_profile_exists() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"Use clippy.\"\n\n\
                   [[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let r = route(&st, &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(!body.contains("Welcome to load studio"));
    }

    #[test]
    fn assets_are_guarded_then_served() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        assert_eq!(
            route(&st, &req("GET", "/assets/studio.css", "", &[HOST], "")).status,
            403
        );
        let r = route(
            &st,
            &req("GET", "/assets/studio.css", "", &[HOST, COOKIE], ""),
        );
        assert_eq!(r.status, 200);
        // The embedded stylesheet carries the light theme palette + the toggle
        // glyph rules (guards against a stale/empty embed).
        let css = String::from_utf8(r.body).unwrap();
        assert!(css.contains(r#"[data-theme="light"]"#));
        assert!(css.contains(".theme-toggle"));
    }

    #[test]
    fn profile_detail_shows_per_fragment_cards() {
        let cfg = "[[fragments]]\n\
             id = \"rc\"\n\
             description = \"Rust conv\"\n\
             guidance = \"Use clippy here.\"\n\
             \n\
             [[fragments]]\n\
             id = \"tc\"\n\
             description = \"Terse\"\n\
             guidance = \"Be terse.\"\n\
             \n\
             [[loadouts]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             fragments = [\"rc\", \"tc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Preview renders the composed document: one expandable card per
        // composed fragment, each carrying its rendered guidance. (Plain select
        // renders the board; Preview is the composed-doc view.)
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/preview", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("<h1>rust</h1>")); // the detail names the profile
        assert!(body.contains("fragment-detail")); // expandable cards
        assert!(body.contains("Rust conv") && body.contains("Use clippy here."));
        assert!(body.contains("Terse") && body.contains("Be terse."));
        // The rendered/raw toggle is gone in the new design.
        assert!(!body.contains("overlay-toggle") && !body.contains("ov-raw"));

        // A state-changing POST is CSRF-guarded (no Origin → rejected).
        let r = route(
            &st,
            &req("POST", "/profiles/rust/disable", "", &[HOST, COOKIE], ""),
        );
        assert_eq!(r.status, 403);
    }

    #[test]
    fn profiles_tab_empty_state_when_none() {
        let d = rust_repo();
        let st = state_for(d.path(), None); // no profiles configured
        let r = route(&st, &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("No loadouts yet"));
    }

    fn body_of(r: Resp) -> String {
        assert_eq!(r.status, 200);
        String::from_utf8(r.body).unwrap()
    }

    #[test]
    fn create_fragment_then_diff_then_apply_writes_disk() {
        let d = rust_repo();
        let st = state_for(d.path(), None);

        // Stage a new fragment via the editor POST.
        let saved = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=rc&kind=markdown&guidance=Use+clippy&scope=repo&visibility=public",
            ),
        ));
        assert!(saved.contains("staged fragment"));

        // Review shows the staged addition against the (empty) on-disk bytes.
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("rc"));
        assert!(diff.contains("Use clippy"));

        // Apply writes it to disk, comment-preservingly via toml_edit.
        let applied = body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(applied.contains("applied"));

        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(on_disk.contains("id = \"rc\""));
        assert!(on_disk.contains("Use clippy"));

        // Baseline reset: nothing staged now.
        let diff2 = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff2.contains("No staged changes"));
    }

    #[test]
    fn discard_clears_staged_without_writing_disk() {
        let d = rust_repo();
        let st = state_for(d.path(), None);

        // Stage a new fragment, then throw it away with Discard.
        let saved = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=rc&kind=markdown&guidance=Use+clippy&scope=repo&visibility=public",
            ),
        ));
        assert!(saved.contains("staged fragment"));
        assert!(!st.lock().unwrap().session.ops().is_empty());

        let discarded = body_of(route(
            &st,
            &req("POST", "/discard", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(discarded.contains("discarded staged changes"));
        // Staged ops are gone and nothing was written to disk.
        assert!(st.lock().unwrap().session.ops().is_empty());
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("No staged changes"));
        assert!(!std::path::Path::new(&global_config_path(d.path())).exists());
    }

    #[test]
    fn discard_is_csrf_guarded() {
        // A mutating POST without an Origin header is rejected (no clearing).
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("POST", "/discard", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 403);
    }

    #[test]
    fn script_test_run_executes_draft_side_effect_free() {
        let d = rust_repo();
        let st = state_for(d.path(), None);

        // A draft script runs and its stdout + exit code come back.
        let out = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/try",
                "",
                &[HOST, COOKIE, ORIGIN],
                "command=echo+hello-draft&script_lang=bash",
            ),
        ));
        assert!(out.contains("hello-draft"));
        assert!(out.contains("exit 0"));
        // Nothing was staged or written — a test run is side-effect-free.
        assert!(st.lock().unwrap().session.ops().is_empty());
        assert!(!std::path::Path::new(&global_config_path(d.path())).exists());

        // A non-zero exit surfaces as an error badge.
        let failed = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/try",
                "",
                &[HOST, COOKIE, ORIGIN],
                "command=exit+2&script_lang=bash",
            ),
        ));
        assert!(failed.contains("exit 2"));

        // Empty script → a note, no execution.
        let empty = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/try",
                "",
                &[HOST, COOKIE, ORIGIN],
                "command=&script_lang=bash",
            ),
        ));
        assert!(empty.to_lowercase().contains("nothing to run"));
    }

    #[test]
    fn script_test_run_is_csrf_guarded() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(
            &st,
            &req(
                "POST",
                "/fragments/try",
                "",
                &[HOST, COOKIE],
                "command=echo+x&script_lang=bash",
            ),
        );
        assert_eq!(r.status, 403);
    }

    #[test]
    fn dynamic_fragment_card_starts_collapsed() {
        // A script/dynamic fragment used to auto-open in the profile detail; now
        // every card starts collapsed (no `open` attribute).
        let cfg = "[[fragments]]\n\
             id = \"deploy\"\n\
             description = \"Deploy status\"\n\
             command = \"echo green\"\n\
             \n\
             [[loadouts]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             fragments = [\"deploy\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/preview", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("fragment-detail"), "renders the card");
        assert!(
            !body.contains(" open>"),
            "fragment cards must start collapsed"
        );
    }

    #[test]
    fn running_a_dynamic_fragment_re_renders_it_open() {
        let cfg = "[[fragments]]\n\
             id = \"deploy\"\n\
             description = \"Deploy status\"\n\
             command = \"echo green\"\n\
             \n\
             [[loadouts]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             fragments = [\"deploy\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Selecting the profile leaves the card collapsed...
        let sel = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(!sel.contains(" open>"));

        // ...but running it re-renders that card expanded so its output shows.
        let ran = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/deploy/run",
                "profile=rust",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(ran.contains(" open>"), "the run fragment stays open");
        assert!(ran.contains("fragment-output"), "and shows its output");
    }

    #[test]
    fn a_failing_dynamic_fragment_shows_error_and_retry() {
        let cfg = "[[fragments]]\n\
             id = \"deploy\"\n\
             description = \"Deploy status\"\n\
             command = \"echo boom >&2; exit 7\"\n\
             \n\
             [[loadouts]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             fragments = [\"deploy\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let ran = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/deploy/run",
                "profile=rust",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(ran.contains("Script failed:"), "shows the failure");
        assert!(
            ran.contains("exited 7") && ran.contains("boom"),
            "with the message"
        );
        assert!(ran.contains("Retry"), "and a retry button");
        assert!(!ran.contains("fragment-output"), "no (blank) output panel");
    }

    #[test]
    fn delete_fragment_stages_and_applies_removal() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"keep clippy\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let r = body_of(route(
            &st,
            &req("DELETE", "/fragments/rc", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(r.contains("staged deletion"));

        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(!on_disk.contains("id = \"rc\""));
    }

    #[test]
    fn deleting_a_composed_fragment_warns_and_cleans_up_the_profile() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\
                   \n[[fragments]]\nid = \"keep\"\nguidance = \"y\"\n\
                   \n[[loadouts]]\nname = \"p\"\ntargets = [\"rust\"]\nfragments = [\"rc\", \"keep\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // The edit dialog warns up front that the cap is composed by a profile.
        let dialog = body_of(route(
            &st,
            &req("GET", "/fragments/rc/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(dialog.contains("composed by") && dialog.contains("“p”"));

        // Deleting it stages the removal AND cleans the reference, and says so.
        let r = body_of(route(
            &st,
            &req("DELETE", "/fragments/rc", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(r.contains("staged deletion"));
        assert!(r.contains("removed it from") && r.contains("“p”"));

        // Apply → on disk the cap is gone and the profile no longer references it.
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            !on_disk.contains("\"rc\""),
            "no dangling ref; got:\n{on_disk}"
        );
        assert!(on_disk.contains("id = \"keep\""));
        assert!(on_disk.contains("fragments = [\"keep\"]"));
    }

    #[test]
    fn duplicate_palette_item_stages_into_global() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/rust-conventions/duplicate",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(r.contains("duplicated"));
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("rust-conventions"));
    }

    #[test]
    fn profile_save_enforces_at_least_one_fragment() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // No fragment selected → rejected with the ≥1 rule, nothing staged.
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=p&targets=rust",
            ),
        ));
        assert!(err.contains("at least one fragment"));

        // With a fragment → staged.
        let ok = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=p&targets=rust&fragments=rc&scope=repo",
            ),
        ));
        assert!(ok.contains("staged loadout"));
    }

    #[test]
    fn profile_save_requires_a_name() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // A fragment but no name → rejected; the editor comes back with the
        // reason inline (not a bare error banner), and nothing is staged.
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "new=1&targets=rust&fragments=rc&scope=repo",
            ),
        ));
        assert!(err.contains("name is required")); // inline error
        assert!(err.contains("banner error")); // shown in the editor, not a fragment
        assert!(err.contains("New loadout")); // re-rendered in new-profile mode
        assert!(err.contains("name=\"name\"")); // with the editable name field

        // Nothing was staged by the failed save.
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("No staged changes"));
    }

    #[test]
    fn profile_edit_form_offers_an_editable_name() {
        // Opening an existing profile's editor presents the name editable (not
        // readonly) and carries the original name as the rename key.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\n\
                   [[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(!body.contains("name=\"name\" value=\"rust\" readonly"));
        assert!(body.contains(r#"name="original_name" value="rust""#));
    }

    #[test]
    fn profile_rename_via_editor_stages_and_applies() {
        // Editing a profile with a changed name renames it in place: the old
        // entry is replaced, not duplicated.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\n\
                   [[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "original_name=rust&name=rust-web&targets=rust&fragments=rc",
            ),
        ));
        assert!(r.contains("staged loadout"));
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(
            on_disk.contains("name = \"rust-web\""),
            "renamed: {on_disk}"
        );
        assert!(
            !on_disk.contains("name = \"rust\""),
            "old name gone (no duplicate): {on_disk}"
        );
    }

    #[test]
    fn profile_rename_onto_existing_name_is_rejected() {
        // Renaming `a` onto the existing `b` would clobber it — refused inline.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\n\
                   [[loadouts]]\nname = \"a\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n\n\
                   [[loadouts]]\nname = \"b\"\ntargets = [\"go\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "original_name=a&name=b&targets=rust&fragments=rc",
            ),
        ));
        assert!(err.contains("already exists"), "rename collision: {err}");
        // Nothing staged.
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("No staged changes"));
    }

    #[test]
    fn new_profile_onto_existing_name_is_rejected() {
        // Creating a *new* profile whose name already exists must not silently
        // clobber the existing one (no `original_name` → it's a create).
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\n\
                   [[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "new=1&name=rust&targets=go&fragments=rc",
            ),
        ));
        assert!(err.contains("already exists"), "create collision: {err}");
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("No staged changes"));
    }

    #[test]
    fn machine_profile_is_pinned_to_top_of_rail() {
        // `machine` is declared *second*, but the rail pins the machine-scope
        // profile to the top regardless of config order.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\n\
                   [[loadouts]]\nname = \"web\"\ntargets = [\"nextjs\"]\nfragments = [\"rc\"]\n\n\
                   [[loadouts]]\nname = \"machine\"\ntargets = [\"machine\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let r = route(&st, &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""));
        let body = body_of(r);
        let machine_at = body.find(r#"data-profile="machine""#);
        let web_at = body.find(r#"data-profile="web""#);
        assert!(machine_at.is_some(), "machine profile in rail");
        assert!(machine_at < web_at, "machine renders before web");
    }

    #[test]
    fn profiles_tab_does_not_auto_select() {
        // A rust profile that *would* be the bound candidate in this repo.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let body = body_of(route(
            &st,
            &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""),
        ));
        // The rail lists the profile, but the main pane shows the pick prompt —
        // no profile is auto-selected, so no detail/cards render.
        assert!(body.contains("Select a loadout to see what it composes."));
        assert!(!body.contains("fragment-detail"));
        assert!(!body.contains("<h1>rust</h1>"));
        // Explicitly selecting one renders its board (Applies to / Fragments /
        // Workflow slots), not the composed-document cards.
        let detail = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(detail.contains("<h1>rust</h1>") && detail.contains("lo-board"));
        assert!(detail.contains("Applies to") && detail.contains("Workflow"));
        assert!(!detail.contains("fragment-detail"));
    }

    #[test]
    fn board_inline_edits_stage_changes() {
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\
             \n[[fragments]]\nid = \"tc\"\nguidance = \"y\"\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Select renders the board: the three slot sections + the one equipped
        // fragment (with its remove control).
        let board = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(board.contains("lo-board"));
        assert!(
            board.contains("Applies to")
                && board.contains("Fragments")
                && board.contains("Workflow")
        );
        assert!(board.contains("/profiles/rust/fragments/rc")); // rc chip's remove

        // Equip the second fragment → it stages and the readout counts both.
        let after = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles/rust/fragments/tc",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(after.contains("/profiles/rust/fragments/tc"));
        assert!(after.contains("2 fragments"));

        // Bind a workflow (a plain string ref; resolution is lazy) → slot fills.
        let bound = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles/rust/workflow/superpowers",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(
            bound.contains("superpowers"),
            "workflow slot fills; got:\n{bound}"
        );
        // Clear it → the empty "Equip a workflow" slot returns.
        let cleared = body_of(route(
            &st,
            &req(
                "DELETE",
                "/profiles/rust/workflow",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(cleared.contains("Equip a workflow"));

        // Add then remove a target.
        let added = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles/rust/targets/go",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(added.contains("/profiles/rust/targets/go"));
        let removed = body_of(route(
            &st,
            &req(
                "DELETE",
                "/profiles/rust/targets/go",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(!removed.contains("/profiles/rust/targets/go"));

        // State-changing slot edits are CSRF-guarded (no Origin → rejected).
        assert_eq!(
            route(
                &st,
                &req(
                    "POST",
                    "/profiles/rust/fragments/tc",
                    "",
                    &[HOST, COOKIE],
                    ""
                )
            )
            .status,
            403
        );
    }

    #[test]
    fn default_loadout_is_pinned_and_locked() {
        // `base` has no targets → the catch-all default; `rust` is targeted.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\
             \n[[loadouts]]\nname = \"base\"\nfragments = [\"rc\"]\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // The default's board: locked "Applies to", a Default badge, and no
        // rename/delete (it's always-present).
        let base = body_of(route(
            &st,
            &req("GET", "/profiles/base/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(base.contains("Applies everywhere"));
        assert!(base.contains("chip-default"));
        assert!(
            !base.contains("/profiles/base/edit"),
            "default isn't renamable"
        );
        assert!(
            !base.contains("hx-delete=\"/profiles/base\""),
            "default isn't deletable"
        );

        // The rail pins the default (its own class) above a separator.
        let tab = body_of(route(
            &st,
            &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""),
        ));
        assert!(tab.contains("rail-item default"));
        assert!(tab.contains("rail-sep"));

        // `rust`'s lone target can't be removed while a default exists: the ✕ is
        // disabled in the UI, and a direct DELETE is refused.
        let rustb = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(rustb.contains("tx disabled"));
        let blocked = body_of(route(
            &st,
            &req(
                "DELETE",
                "/profiles/rust/targets/rust",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(blocked.contains("at least one target"));
    }

    #[test]
    fn last_target_clears_to_create_the_default_when_none_exists() {
        // No default yet → clearing rust's only target converts it to the default.
        let cfg = "[[fragments]]\nid = \"rc\"\nguidance = \"x\"\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        // The lone target's ✕ is enabled (no default exists to conflict).
        let before = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(!before.contains("tx disabled"));
        // Remove it → rust becomes the no-targets default (locked Applies-to).
        let after = body_of(route(
            &st,
            &req(
                "DELETE",
                "/profiles/rust/targets/rust",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(after.contains("Applies everywhere"));
    }

    #[test]
    fn diff_surfaces_leak_warning_for_public_config() {
        // A fragment whose guidance carries a machine-specific literal in the
        // public (global) config.toml is leak-linted before apply.
        let cfg = "[[fragments]]\n\
             id = \"deploy\"\n\
             command = \"echo hi\"\n\
             guidance = \"ssh to build-box.corp.example.com\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        // Leak-lint flags the private-looking hostname in the public layer.
        assert!(diff.to_lowercase().contains("leak check"));
        assert!(diff.contains("build-box.corp.example.com"));
    }

    #[test]
    fn fragment_editor_form_loads_for_palette_and_owned() {
        let cfg = "[[fragments]]\nid = \"mine\"\nguidance = \"owned\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Owned cap → editable form.
        let owned = body_of(route(
            &st,
            &req("GET", "/fragments/mine/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(owned.contains("Edit fragment"));
        // Editing offers Save (in place) + Save as a copy + Delete.
        assert!(owned.contains("Save"));
        assert!(owned.contains("Save as a copy"));
        assert!(owned.contains("Delete"));
        assert!(owned.contains("/fragments/mine"));
        // The editor exposes the category metadata field.
        assert!(owned.contains("name=\"category\""));

        // Opened from a profile's detail, the form carries a return_profile so
        // Save re-renders that profile rather than the Fragments tab.
        let from_profile = body_of(route(
            &st,
            &req(
                "GET",
                "/fragments/mine/edit",
                "profile=rust",
                &[HOST, COOKIE],
                "",
            ),
        ));
        assert!(from_profile.contains("name=\"return_profile\""));
        assert!(from_profile.contains("value=\"rust\""));

        // Palette cap → read-only dialog with a duplicate action.
        let palette = body_of(route(
            &st,
            &req(
                "GET",
                "/fragments/rust-conventions/view",
                "",
                &[HOST, COOKIE],
                "",
            ),
        ));
        assert!(palette.contains("Palette fragment"));
        assert!(palette.contains("Duplicate"));
    }

    #[test]
    fn script_fragment_is_editable_not_advanced() {
        // A command cap with no guidance template opens in the editor (script
        // field + highlight overlay), not the read-only "advanced — edit in TOML"
        // dialog. (Pairing a command with a guidance template is what makes a cap
        // advanced; a plain script is fully editable.)
        let cfg = "[[fragments]]\nid = \"host\"\ndescription = \"Host\"\n\
             script_lang = \"bash\"\ncommand = \"uname -a\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/fragments/host/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("Edit fragment"));
        assert!(!body.contains("Advanced fragment"));
        // The highlight-overlay script field is present, prefilled with the command.
        assert!(body.contains("code-edit") && body.contains("name=\"command\""));
        assert!(body.contains("uname -a"));
    }

    #[test]
    fn dynamic_fragment_without_cache_shows_run_prompt() {
        // A provider cap doesn't run in the read-only preview; with nothing cached
        // the overlay drops its section, but the profile detail keeps the card and
        // offers a centered "Run" prompt (so it's still listed, openable, runnable).
        let cfg = "[[fragments]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/preview", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("fragment-detail")); // the card is present
        assert!(body.contains("fragment-run-prompt") && body.contains("Run script")); // centered prompt
        assert!(body.contains("/fragments/host/run?profile=rust")); // wired to run
    }

    #[test]
    fn run_all_executes_dynamic_caps_live() {
        // The `host` provider is a safe built-in probe, so a Live run produces output.
        let cfg = "[[fragments]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[loadouts]]\nname = \"m\"\ntargets = [\"rust\"]\nfragments = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Read-only preview: a run prompt, no real output yet.
        let before = body_of(route(
            &st,
            &req("GET", "/profiles/m/preview", "", &[HOST, COOKIE], ""),
        ));
        assert!(before.contains("fragment-run-prompt") && !before.contains("fragment-output"));

        // Run all → live render executes the provider and shows verbatim output.
        let after = body_of(route(
            &st,
            &req("POST", "/profiles/m/run", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(!after.contains("runs at render"));
        assert!(after.contains("fragment-output")); // rendered as preformatted output

        // Run is state-changing → CSRF-guarded (no Origin → rejected).
        assert_eq!(
            route(
                &st,
                &req("POST", "/profiles/m/run", "", &[HOST, COOKIE], "")
            )
            .status,
            403
        );
    }

    #[test]
    fn per_card_run_executes_one_fragment() {
        let cfg = "[[fragments]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[loadouts]]\nname = \"m\"\ntargets = [\"rust\"]\nfragments = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Running one cap caches its output; the re-rendered detail shows it.
        let after = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments/host/run",
                "profile=m",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(!after.contains("runs at render"));
        assert!(after.contains("fragment-output"));
    }

    #[test]
    fn save_as_copy_creates_a_new_fragment_under_a_new_name() {
        let cfg = "[[fragments]]\nid = \"rc\"\ndescription = \"Rust conv\"\nguidance = \"Old.\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Save as a copy with a new title → a new id; the original is untouched.
        let body = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments",
                "as=copy",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&name=Rust+strict&kind=markdown&guidance=New+body&scope=repo&visibility=public",
            ),
        ));
        assert!(body.contains("saved copy"));

        // Apply and confirm the copy landed under a new id with the original
        // fragment left intact.
        body_of(route(
            &st,
            &req("POST", "/apply", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        let on_disk = std::fs::read_to_string(global_config_path(d.path())).unwrap();
        assert!(on_disk.contains("id = \"rust-strict\"")); // the copy's derived id
        assert!(on_disk.contains("New body"));
        assert!(on_disk.contains("id = \"rc\"") && on_disk.contains("Old.")); // original kept

        // Copying onto an existing id is rejected.
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments",
                "as=copy",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&name=rc&kind=markdown&guidance=x&scope=repo",
            ),
        ));
        assert!(err.contains("already exists"));
    }

    #[test]
    fn editing_a_fragment_from_a_profile_returns_the_profile_detail() {
        let cfg = "[[fragments]]\n\
             id = \"rc\"\nguidance = \"Old guidance.\"\n\
             \n[[loadouts]]\nname = \"rust\"\ntargets = [\"rust\"]\nfragments = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Saving with return_profile set re-renders the profile's detail (the
        // updated guidance shows in a cap card) rather than the Fragments tab.
        let body = body_of(route(
            &st,
            &req(
                "POST",
                "/fragments",
                "",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&return_profile=rust&name=rc&kind=markdown&guidance=Fresh+guidance&scope=repo",
            ),
        ));
        assert!(body.contains("<h1>rust</h1>")); // the profile detail, not the caps tab
        assert!(body.contains("fragment-detail"));
        assert!(body.contains("Fresh guidance"));
        assert!(body.contains("/close")); // modal-close loader appended
    }

    #[test]
    fn welcome_embeds_the_lazy_skill_card_loader() {
        // Fresh config → first-launch welcome → the skill card placeholder is
        // present and wired to load itself from /skills/card.
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let body = body_of(route(
            &st,
            &req("GET", "/onboarding/welcome", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("id=\"skill-card\""));
        assert!(body.contains("hx-get=\"/skills/card\""));
    }

    #[test]
    fn skill_card_route_serves_a_card() {
        // Read-only against the real $HOME, so the install state varies by
        // machine — every card state must name the skills (the assertion below
        // is what keeps this test environment-independent).
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(&st, &req("GET", "/skills/card", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("loadout-migrate"));
    }

    #[test]
    fn skill_install_requires_origin_like_all_mutations() {
        // POST without Origin/Referer is rejected before the handler runs, so
        // the CSRF guard covers the new immediate-side-effect route too.
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = route(
            &st,
            &req("POST", "/skills/install", "", &[HOST, COOKIE], ""),
        );
        assert_eq!(r.status, 403);
    }

    // --- re-attach to an already-running instance ----------------------------

    /// Start a real studio server on an OS-chosen port in a background thread,
    /// serving via the production `route`. Returns the bound port; the returned
    /// `TempDir` keeps the fixture's global config dir alive for the test.
    fn spawn_studio(token: &str) -> (u16, tempfile::TempDir) {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        {
            let mut s = st.lock().unwrap();
            s.token = token.to_string();
            s.port = port; // host_ok checks the Host header against this
        }
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let req = read_request(&mut request);
                let resp = route(&st, &req);
                let _ = respond(request, resp);
            }
        });
        (port, d)
    }

    #[test]
    fn probe_recognizes_running_studio_only_with_right_token() {
        let (port, _d) = spawn_studio("goodtoken");
        // Correct token → bootstrap 302 + loadout_studio cookie → recognized.
        assert!(probe_studio(port, "goodtoken"));
        // Wrong token → 403, no cookie → not recognized (fails closed).
        assert!(!probe_studio(port, "badtoken"));
    }

    #[test]
    fn probe_false_when_nothing_listening() {
        // Bind a port, learn it, then drop the listener so the port is free.
        let port = {
            let s = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
            s.server_addr().to_ip().unwrap().port()
        };
        assert!(!probe_studio(port, "whatever"));
    }

    #[test]
    fn attach_recovers_token_from_runtime_file() {
        let (port, _d) = spawn_studio("filetoken");
        let rt_dir = tempfile::tempdir().unwrap();
        let path = rt_dir.path().join(format!("studio-{port}.json"));

        write_runtime_to(&path, port, "filetoken");
        assert_eq!(attach_from(&path, port), Some("filetoken".to_string()));

        // Stale token in the file → live server rejects it → no attach.
        write_runtime_to(&path, port, "stale");
        assert_eq!(attach_from(&path, port), None);

        // Port recorded in the file must match the port we're attaching to.
        write_runtime_to(&path, port, "filetoken");
        assert_eq!(attach_from(&path, port + 1), None);
    }

    #[test]
    fn attach_none_when_file_missing() {
        let rt_dir = tempfile::tempdir().unwrap();
        let path = rt_dir.path().join("studio-1.json");
        assert_eq!(attach_from(&path, 1), None);
    }

    #[test]
    fn attach_running_refuses_port_zero() {
        // Port 0 means "let the OS pick"; it's never a re-attach target.
        assert_eq!(try_attach_running(0), None);
    }

    #[test]
    fn runtime_file_is_user_only_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let rt_dir = tempfile::tempdir().unwrap();
        let path = rt_dir.path().join("studio-7777.json");
        write_runtime_to(&path, 7777, "secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn runtime_file_overwrite_tightens_loose_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let rt_dir = tempfile::tempdir().unwrap();
        let path = rt_dir.path().join("studio-7777.json");
        // Pre-existing file with world-readable perms (e.g. a stale leak).
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Overwriting must re-assert 0600 (mode() alone wouldn't on an existing file).
        write_runtime_to(&path, 7777, "secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
