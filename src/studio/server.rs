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
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};

use crate::capability::{palette, Layer};
use crate::cli::StudioArgs;
use crate::commands::Runtime;
use crate::config::{self, Config};
use crate::context;
use crate::dynamic::DynamicMode;
use crate::profile::ProfileConfig;
use crate::studio::assets;
use crate::studio::edit::{Session, StagedOp};
use crate::studio::state::{
    self, BindingState, LibraryView, PreviewOutcome, Simulated, StudioState,
};
use crate::studio::views::{self, TrustBanner};
use crate::trust;

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
            "missing or invalid session token — open the bootstrap URL printed by `rosita studio`",
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
        ("GET", "/tab/capabilities") => handle_tab(state, "capabilities"),
        ("GET", "/staged") => handle_staged(state),
        ("GET", "/close") => Resp::html(String::new()),
        ("GET", "/fs-status") => handle_fs_status(state),
        ("GET", "/diff") => handle_diff(state),
        ("POST", "/apply") => handle_apply(state),
        ("POST", "/trust/allow") => handle_trust(state, true),
        ("POST", "/trust/deny") => handle_trust(state, false),
        ("GET", "/capabilities/new") => {
            Resp::html(views::cap_dialog(None, Layer::Global, true, None, &[]))
        }
        ("POST", "/capabilities") => handle_cap_save(state, req),
        ("POST", "/onboarding/quickstart") => handle_quickstart(state),
        ("GET", "/profiles/new") => handle_profile_new(state),
        ("POST", "/profiles") => handle_profile_save(state, req),
        ("POST", "/profiles/preview") => handle_editor_preview(state, req),
        ("POST", "/profiles/draft") => handle_profile_draft(state, req),
        ("GET", p) if p.starts_with("/assets/") => match assets::get(p) {
            Some((body, ct)) => Resp::asset(body, ct),
            None => Resp::not_found(),
        },
        (_, p) if p.starts_with("/capabilities/") => handle_cap_param(state, req),
        (_, p) if p.starts_with("/profiles/") => handle_profile_param(state, req),
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

fn handle_cap_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (id, action) = id_and_action(&req.path, "/capabilities/");
    match (req.method.as_str(), action) {
        ("GET", "edit") | ("GET", "view") => {
            // A `?profile=` carries the profile to return to when editing a cap
            // from inside its detail (so Save re-renders that profile, not the
            // Capabilities tab).
            let profile = field(&req.query, "profile");
            handle_cap_edit(state, &id, &profile)
        }
        ("DELETE", "") => handle_cap_delete(state, &id),
        ("POST", "duplicate") => handle_cap_duplicate(state, &id),
        ("POST", "run") => handle_cap_run(state, &id, &field(&req.query, "profile")),
        _ => Resp::not_found(),
    }
}

fn handle_profile_param(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let (name, action) = id_and_action(&req.path, "/profiles/");
    match (req.method.as_str(), action) {
        ("GET", "edit") => handle_profile_edit(state, &name),
        ("GET", "select") => handle_profile_select(state, &name),
        ("GET", "preview") => handle_profile_preview(state, &name, ""),
        ("POST", "preview") => {
            let agent = field(&req.body, "agent");
            handle_profile_preview(state, &name, &agent)
        }
        ("POST", "disable") => handle_profile_disable(state, &name),
        ("POST", "run") => handle_profile_run(state, &name),
        ("DELETE", "") => handle_profile_delete(state, &name),
        _ => Resp::not_found(),
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

fn handle_shell(state: &Arc<Mutex<StudioState>>) -> Resp {
    let (snap, staged) = snap_and_staged(state);
    match state::library_view(&snap) {
        Ok(lib) => Resp::html(views::shell(
            views::capabilities_tab(&lib, None),
            staged,
            "capabilities",
        )),
        Err(e) => Resp::html(views::error_page(&e.to_string())),
    }
}

/// Snapshot + staged-op count, taken together under the session lock.
fn snap_and_staged(state: &Arc<Mutex<StudioState>>) -> (state::Snapshot, usize) {
    let s = state.lock().unwrap();
    (s.snapshot(), s.session.ops().len())
}

fn handle_tab(state: &Arc<Mutex<StudioState>>, tab: &str) -> Resp {
    if tab == "profiles" {
        return profiles_tab_resp(state, None, None, false);
    }
    let snap = state.lock().unwrap().snapshot();
    match state::library_view(&snap) {
        Ok(lib) => Resp::html(views::capabilities_tab_fragment(&lib, None)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Render the full Profiles tab (rail + detail). A detail pane shows only when
/// `selected` names an existing profile; otherwise the rail renders with a
/// "pick a profile" prompt (no profile is auto-selected). `flash` shows a
/// banner; `with_staged` appends the staged-indicator refresh loader (used after
/// a mutation re-renders `#main`).
fn profiles_tab_resp(
    state: &Arc<Mutex<StudioState>>,
    selected: Option<&str>,
    flash: Option<&str>,
    with_staged: bool,
) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
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
            .unwrap_or_else(|e| empty_preview(&name, format!("preview error: {e}")));
        (name, outcome, disabled)
    });
    // On a fresh config (no profiles and no own caps), the empty Profiles tab
    // shows the first-launch welcome instead of the bare "no profiles" prompt.
    let onboarding = (detail.is_none() && lib.profiles.is_empty() && lib.yours.is_empty())
        .then(|| state::onboarding(&snap.base_context));
    let markup = match &detail {
        Some((name, outcome, disabled)) => views::profiles_tab(
            &lib,
            Some(views::ProfileDetail {
                name,
                outcome,
                disabled: *disabled,
            }),
            flash,
            None,
        ),
        None => views::profiles_tab(&lib, None, flash, onboarding.as_ref()),
    };
    let mut html = markup.into_string();
    if with_staged {
        html.push_str(&views::staged_indicator_loader());
    }
    Resp::html(html)
}

/// Render just the selected profile's detail (swapped into `#profile-main`),
/// composed for `agent` (empty ⇒ the configured default). `mode` is ReadOnly for
/// the normal preview, or Live when re-rendering after a "Run".
fn handle_profile_detail(
    state: &Arc<Mutex<StudioState>>,
    name: &str,
    agent: &str,
    mode: DynamicMode,
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
        })),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Run all of a profile's dynamic caps now (Live), then re-render its detail so
/// every script/provider shows real output. Scripts run trust-gated and their
/// (redacted) output is cached, so the read-only preview keeps showing it.
fn handle_profile_run(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    handle_profile_detail(state, name, "", DynamicMode::Live)
}

/// Run one dynamic cap now (Live) so its output caches, then re-render the
/// profile detail (ReadOnly) — only the run cap shows fresh output; the rest keep
/// their placeholder until they're run too. `profile` scopes the render context.
fn handle_cap_run(state: &Arc<Mutex<StudioState>>, id: &str, profile: &str) -> Resp {
    {
        let snap = state.lock().unwrap().snapshot();
        if let Err(e) = state::run_capability(&snap, profile, id) {
            return Resp::html(views::error_fragment(&e.to_string()));
        }
    }
    handle_profile_detail(state, profile, "", DynamicMode::ReadOnly)
}

/// An empty preview outcome (used when a profile can't be composed/rendered).
fn empty_preview(name: &str, note: String) -> PreviewOutcome {
    PreviewOutcome {
        agent: String::new(),
        profile_label: name.to_string(),
        binding: BindingState::None,
        context_summary: String::new(),
        cap_count: 0,
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

// --- capability handlers -----------------------------------------------------

fn handle_cap_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    // `?as=copy` → save as a new capability (don't overwrite the original);
    // `return_profile` (hidden) → the user is editing from a profile's detail.
    let as_copy = field(&req.query, "as") == "copy";
    let return_profile = field(&req.body, "return_profile");

    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    // The existing capability (if editing) so the simple editor's merge preserves
    // fields it doesn't expose (tags/risk/requires/agents/cache/…).
    let base = state::editor_cap_id(&pairs)
        .and_then(|id| cfg.capabilities.iter().find(|c| c.id == id).cloned());
    let mut cap = match state::capability_from_form(base.as_ref(), &pairs) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if as_copy {
        // Duplicate under a new name: a fresh id derived from the title, distinct
        // from the original and from every existing capability.
        let new_id = state::slug(&field(&req.body, "name"));
        if new_id.is_empty() {
            return Resp::html(views::error_fragment(
                "give the copy a name with letters or digits",
            ));
        }
        if cfg.capabilities.iter().any(|c| c.id == new_id) {
            return Resp::html(views::error_fragment(&format!(
                "“{new_id}” already exists — choose a new name for the copy"
            )));
        }
        cap.id = new_id;
    }
    let layer = state::layer_from_form(&pairs);
    let id = cap.id.clone();
    // EditCapability upserts by id (creates if absent), so save covers new+edit.
    let res = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::EditCapability {
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
        format!("staged capability “{id}”")
    };
    // Edited from a profile → re-render that profile's detail (so an in-place
    // save shows the updated guidance) and close the modal. Otherwise re-render
    // the Capabilities tab.
    if !return_profile.is_empty() {
        let mut resp = profiles_tab_resp(state, Some(&return_profile), Some(&flash), true);
        if resp.status == 200 {
            resp.body
                .extend_from_slice(views::modal_close_loader().as_bytes());
        }
        return resp;
    }
    match library_now(state) {
        Ok(lib) => Resp::html(views::cap_result(&lib, &flash)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_cap_edit(state: &Arc<Mutex<StudioState>>, id: &str, return_profile: &str) -> Resp {
    let rp = (!return_profile.is_empty()).then_some(return_profile);
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if let Some(c) = cfg.capabilities.iter().find(|c| c.id == id) {
        let used_by = profiles_using(&cfg, id);
        Resp::html(views::cap_dialog(Some(c), c.origin, true, rp, &used_by))
    } else if let Some(c) = palette().into_iter().find(|c| c.id == id) {
        Resp::html(views::cap_dialog(Some(&c), Layer::Global, false, rp, &[]))
    } else {
        Resp::html(views::error_fragment(&format!("unknown capability '{id}'")))
    }
}

/// Names of the profiles whose `capabilities` reference `cap_id`.
fn profiles_using(cfg: &Config, cap_id: &str) -> Vec<String> {
    cfg.profiles
        .iter()
        .filter(|p| p.capabilities.iter().any(|r| r.id() == cap_id))
        .map(|p| p.name.clone())
        .collect()
}

fn handle_cap_delete(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    // Deleting a composed capability would leave dangling references in the
    // profiles that use it (silently dropped at render). Instead, stage the
    // deletion AND remove the id from every profile that composed it, and report
    // the cleanup so it isn't invisible.
    let res = (|| -> crate::Result<(Vec<String>, Vec<String>)> {
        let mut s = state.lock().unwrap();
        let Some(layer) = s.session.capability_layer(id) else {
            return Err(anyhow!(
                "“{id}” isn't in your library — palette items can't be deleted"
            ));
        };
        let cfg = s.session.staged_config()?;
        let affected = profiles_using(&cfg, id);
        let cleaned: Vec<ProfileConfig> = cfg
            .profiles
            .iter()
            .filter(|p| affected.contains(&p.name))
            .map(|p| {
                let mut p = p.clone();
                p.capabilities.retain(|r| r.id() != id);
                p
            })
            .collect();

        s.session.stage(StagedOp::DeleteCapability {
            layer,
            id: id.to_string(),
        })?;
        let mut emptied = Vec::new();
        for p in cleaned {
            if p.capabilities.is_empty() && p.guidance.is_none() {
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
                "now has no capabilities"
            } else {
                "now have no capabilities"
            };
            msg.push_str(&format!(" ({e} {now_has})"));
        }
    }

    match library_now(state) {
        Ok(lib) => Resp::html(views::cap_result(&lib, &msg)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_cap_duplicate(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let res = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::DuplicatePaletteItem {
            id: id.to_string(),
            to_layer: Layer::Global,
        });
    match res.and_then(|()| library_now(state)) {
        Ok(lib) => Resp::html(views::cap_result(
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
    Resp::html(views::profile_editor(&draft, true, &lib, &preview, None))
}

/// First-launch quick start: duplicate the suggested palette starter caps into
/// the library (staged), then open the composer pre-filled with a starter
/// profile (detected target + those caps). Everything is staged — the user
/// reviews and Applies, or discards, like any other edit.
fn handle_quickstart(state: &Arc<Mutex<StudioState>>) -> Resp {
    let onb = {
        let s = state.lock().unwrap();
        state::onboarding(&s.base_context)
    };
    // Duplicate each suggested palette cap that isn't already owned. `onboarding`
    // already filtered `caps` to real palette ids, so each is duplicate-able.
    {
        let mut s = state.lock().unwrap();
        let owned: std::collections::HashSet<String> = s
            .session
            .staged_config()
            .map(|cfg| cfg.capabilities.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default();
        for id in &onb.caps {
            if owned.contains(id) {
                continue;
            }
            if let Err(e) = s.session.stage(StagedOp::DuplicatePaletteItem {
                id: id.clone(),
                to_layer: Layer::Global,
            }) {
                return Resp::html(views::error_fragment(&e.to_string()));
            }
        }
    }
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let draft = state::quickstart_draft(&onb);
    let preview = profile_preview_or_empty(&snap, &draft, "");
    // The editor swaps `#main`; append the staged-indicator refresh so the top
    // bar reflects the just-staged capability duplications.
    let mut html = views::profile_editor(&draft, true, &lib, &preview, None);
    html.push_str(&views::staged_indicator_loader());
    Resp::html(html)
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
            Resp::html(views::profile_editor(p, false, &lib, &preview, None))
        }
        None => Resp::html(views::error_fragment(&format!("unknown profile '{name}'"))),
    }
}

fn handle_profile_select(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    handle_profile_detail(state, name, "", DynamicMode::ReadOnly)
}

fn handle_profile_preview(state: &Arc<Mutex<StudioState>>, name: &str, agent: &str) -> Resp {
    handle_profile_detail(state, name, agent, DynamicMode::ReadOnly)
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
            None => return Resp::html(views::error_fragment(&format!("unknown profile '{name}'"))),
        }
    };
    match res {
        Ok(()) => profiles_tab_resp(state, Some(name), Some(&format!("toggled “{name}”")), true),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_profile_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let profile = match state::profile_from_form(&pairs) {
        // A profile needs a name (and ≥1 capability). On failure, stay in the
        // editor with the draft preserved and the reason shown inline — never
        // replace the whole form with a bare error.
        Ok(p) => p,
        Err(e) => return profile_editor_with_error(state, &pairs, &e.to_string()),
    };
    let name = profile.name.clone();
    // Profiles are global-only — always authored into the global config.
    let res = state.lock().unwrap().session.stage(StagedOp::EditProfile {
        layer: Layer::Global,
        name: name.clone(),
        profile: Box::new(profile),
    });
    match res {
        Ok(()) => profiles_tab_resp(
            state,
            Some(&name),
            Some(&format!("staged profile “{name}”")),
            true,
        ),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Re-render the profile editor with the in-progress draft preserved and an
/// inline error (missing name / no capabilities), so a failed save keeps the
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
/// new capability, then re-render the editor with the draft preserved and the
/// new cap added + checked.
fn handle_profile_draft(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let Some((cap, layer)) = state::inline_capability_from_form(&pairs) else {
        return Resp::html(views::error_fragment(
            "give the new capability a name before adding it",
        ));
    };
    let new_id = cap.id.clone();
    if let Err(e) = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::EditCapability {
            layer,
            id: new_id.clone(),
            cap: Box::new(cap),
        })
    {
        return Resp::html(views::error_fragment(&e.to_string()));
    }
    // Re-render the editor preserving the in-progress profile + the new cap.
    let snap = state.lock().unwrap().snapshot();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let mut draft = state::draft_profile_from_form(&pairs);
    if !draft.capabilities.iter().any(|r| r.id() == new_id) {
        draft
            .capabilities
            .push(crate::profile::CapabilityRef::Id(new_id));
    }
    let is_new = state::parse_pairs(&req.body)
        .iter()
        .any(|(k, v)| k == "new" && v == "1");
    let preview = profile_preview_or_empty(&snap, &draft, "");
    Resp::html(format!(
        "{}{}",
        views::profile_editor(&draft, is_new, &lib, &preview, None),
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
            None => return Resp::html(views::error_fragment(&format!("unknown profile '{name}'"))),
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
    profile: &crate::profile::ProfileConfig,
    agent: &str,
) -> PreviewOutcome {
    state::render_profile_config(snap, profile, agent, DynamicMode::ReadOnly).unwrap_or_else(|e| {
        PreviewOutcome {
            agent: agent.to_string(),
            profile_label: profile.name.clone(),
            binding: BindingState::None,
            context_summary: String::new(),
            cap_count: 0,
            overlay: String::new(),
            caps: Vec::new(),
            note: Some(format!("preview error: {e}")),
        }
    })
}

fn handle_diff(state: &Arc<Mutex<StudioState>>) -> Resp {
    let (diffs, texts, staged, fs_changed, repo_base) = {
        let s = state.lock().unwrap();
        (
            s.session.diff(),
            s.session.staged_layer_texts(),
            s.session.ops().len(),
            s.session.external_edits(),
            s.repo_base.clone(),
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
    let trust = match Config::from_layer_strs(texts) {
        Ok(cfg) => trust_banner(&cfg, &repo_base),
        Err(_) => TrustBanner {
            command_caps: vec![],
            status: "unknown".into(),
            trusted: false,
        },
    };
    Resp::html(views::diff_view(
        &diffs,
        &leaks,
        &fs_changed,
        &trust,
        staged,
    ))
}

fn handle_apply(state: &Arc<Mutex<StudioState>>) -> Resp {
    // Apply mutates + writes atomically; it's the one serialized operation, so
    // holding the lock across its (brief, small-file) I/O is correct here.
    let result = state.lock().unwrap().session.apply();
    match result {
        Ok(written) => profiles_tab_resp(
            state,
            None,
            Some(&format!("applied {} file change(s)", written.len())),
            true,
        ),
        Err(e) => Resp::html(views::error_fragment(&format!("apply failed: {e}"))),
    }
}

fn handle_trust(state: &Arc<Mutex<StudioState>>, allow: bool) -> Resp {
    let repo_base = state.lock().unwrap().repo_base.clone();
    let res = if allow {
        trust::allow(&repo_base)
    } else {
        trust::deny(&repo_base).map(|_| ())
    };
    match res {
        // Re-render the review so the trust banner reflects the new state.
        Ok(()) => handle_diff(state),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn trust_banner(cfg: &Config, repo_base: &std::path::Path) -> TrustBanner {
    let command_caps: Vec<String> = cfg
        .capabilities
        .iter()
        .filter(|c| c.command.is_some() && matches!(c.origin, Layer::Repo | Layer::RepoLocal))
        .map(|c| c.id.clone())
        .collect();
    let status = trust::status(repo_base);
    TrustBanner {
        command_caps,
        status: status.label().to_string(),
        trusted: status == trust::Status::Trusted,
    }
}

fn handle_fs_status(state: &Arc<Mutex<StudioState>>) -> Resp {
    // A light read of ≤4 small files; the heavy work (render) is kept off-lock.
    let changed = state.lock().unwrap().session.external_edits();
    Resp::html(views::fs_status_fragment(&changed))
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
            .strip_prefix("rosita_studio=")
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
        let cookie = format!("rosita_studio={token}; HttpOnly; SameSite=Strict; Path=/");
        Resp::redirect("/", Some(&cookie))
    } else {
        Resp::forbidden("invalid or missing bootstrap token")
    }
}

// --- socket loop -------------------------------------------------------------

/// Entry point for `rosita studio`: bind, print the bootstrap URL, open the
/// browser (unless `--no-open`), and serve until the process is interrupted.
pub fn serve(rt: &Runtime, args: &StudioArgs) -> crate::Result<()> {
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let base_context = context::detect_context(&rt.cwd, &config).context("detecting context")?;
    let global_dir = config::global_config_dir();
    let session = Session::open(&repo_base, global_dir.as_deref())?;
    let token = make_token()?;

    let server = tiny_http::Server::http(("127.0.0.1", args.port))
        .map_err(|e| anyhow!("binding 127.0.0.1:{}: {e}", args.port))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(args.port);

    let state = Arc::new(Mutex::new(StudioState {
        sim: Simulated {
            agent: config.default_agent.clone(),
            lang: None,
            scope: None,
        },
        session,
        base_context,
        repo_base,
        token: token.clone(),
        port,
    }));

    let url = format!("http://127.0.0.1:{port}{BOOTSTRAP_PATH}?token={token}");
    println!("rosita studio → open  {url}");
    println!("(serving on 127.0.0.1:{port}; Ctrl-C to stop)");
    if !args.no_open {
        open_browser(&url);
    }

    for mut request in server.incoming_requests() {
        let req = read_request(&mut request);
        let resp = route(&state, &req);
        let _ = respond(request, resp);
    }
    Ok(())
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
/// predictable value would defeat the whole localhost auth model. rosita is
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

fn open_browser(url: &str) {
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
        // Capabilities and profiles are global-only, so the fixture's config is
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
            sim: Simulated {
                agent: config.default_agent.clone(),
                lang: None,
                scope: None,
            },
            session,
            base_context,
            repo_base: repo.to_path_buf(),
            token: "testtoken".into(),
            port: 7777,
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
    const COOKIE: (&str, &str) = ("Cookie", "rosita_studio=testtoken");
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
        assert!(cookie.contains("rosita_studio=testtoken"));
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
        assert!(body.contains("rosita studio"));
        // The shell renders the Profiles tab (dashboard) by default.
        assert!(body.contains("Profiles"));
        assert!(body.contains("Capabilities"));
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
        assert!(body.contains("Welcome to rosita studio"), "welcome missing");
        assert!(
            body.contains("/onboarding/quickstart"),
            "quick-start action missing"
        );
        // The detection readout names the detected stack as a chip.
        assert!(body.contains(">rust<"), "detected-stack chip missing");
    }

    #[test]
    fn quickstart_stages_starters_and_prefills_composer() {
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
        // Duplicated the three suggested starters (rust + the two universals).
        assert_eq!(st.lock().unwrap().session.ops().len(), 3);
        let body = String::from_utf8(r.body).unwrap();
        // The composer opened, pre-filled with the detected target as the name…
        assert!(body.contains("New profile"));
        assert!(body.contains("name=\"name\" value=\"rust\""));
        // …and the suggested capabilities are present in the picker to compose.
        assert!(body.contains("Rust conventions"));
        assert!(body.contains("Terse communication"));
        assert!(body.contains("Conventional commits"));
    }

    #[test]
    fn welcome_hidden_once_a_profile_exists() {
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"Use clippy.\"\n\n\
                   [[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let r = route(&st, &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(!body.contains("Welcome to rosita studio"));
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
    }

    #[test]
    fn profile_detail_shows_per_capability_cards() {
        let cfg = "[[capabilities]]\n\
             id = \"rc\"\n\
             description = \"Rust conv\"\n\
             guidance = \"Use clippy here.\"\n\
             \n\
             [[capabilities]]\n\
             id = \"tc\"\n\
             description = \"Terse\"\n\
             guidance = \"Be terse.\"\n\
             \n\
             [[profiles]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             capabilities = [\"rc\", \"tc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Selecting a profile renders the detail: one expandable card per
        // composed capability, each carrying its rendered guidance.
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("<h1>rust</h1>")); // the detail names the profile
        assert!(body.contains("cap-detail")); // expandable cards
        assert!(body.contains("Rust conv") && body.contains("Use clippy here."));
        assert!(body.contains("Terse") && body.contains("Be terse."));
        // The rendered/raw toggle is gone in the new design.
        assert!(!body.contains("overlay-toggle") && !body.contains("ov-raw"));

        // The agent-change POST is CSRF-guarded (no Origin → rejected).
        let r = route(
            &st,
            &req(
                "POST",
                "/profiles/rust/preview",
                "",
                &[HOST, COOKIE],
                "agent=claude",
            ),
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
        assert!(body.contains("No profiles yet"));
    }

    fn body_of(r: Resp) -> String {
        assert_eq!(r.status, 200);
        String::from_utf8(r.body).unwrap()
    }

    #[test]
    fn create_capability_then_diff_then_apply_writes_disk() {
        let d = rust_repo();
        let st = state_for(d.path(), None);

        // Stage a new capability via the editor POST.
        let saved = body_of(route(
            &st,
            &req(
                "POST",
                "/capabilities",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=rc&kind=markdown&guidance=Use+clippy&scope=repo&visibility=public",
            ),
        ));
        assert!(saved.contains("staged capability"));

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
    fn delete_capability_stages_and_applies_removal() {
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"keep clippy\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let r = body_of(route(
            &st,
            &req(
                "DELETE",
                "/capabilities/rc",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
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
    fn deleting_a_composed_capability_warns_and_cleans_up_the_profile() {
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"x\"\n\
                   \n[[capabilities]]\nid = \"keep\"\nguidance = \"y\"\n\
                   \n[[profiles]]\nname = \"p\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\", \"keep\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // The edit dialog warns up front that the cap is composed by a profile.
        let dialog = body_of(route(
            &st,
            &req("GET", "/capabilities/rc/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(dialog.contains("composed by") && dialog.contains("“p”"));

        // Deleting it stages the removal AND cleans the reference, and says so.
        let r = body_of(route(
            &st,
            &req(
                "DELETE",
                "/capabilities/rc",
                "",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
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
        assert!(on_disk.contains("capabilities = [\"keep\"]"));
    }

    #[test]
    fn duplicate_palette_item_stages_into_global() {
        let d = rust_repo();
        let st = state_for(d.path(), None);
        let r = body_of(route(
            &st,
            &req(
                "POST",
                "/capabilities/rust-conventions/duplicate",
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
    fn profile_save_enforces_at_least_one_capability() {
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"x\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // No capability selected → rejected with the ≥1 rule, nothing staged.
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
        assert!(err.contains("at least one capability"));

        // With a capability → staged.
        let ok = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "name=p&targets=rust&capabilities=rc&scope=repo",
            ),
        ));
        assert!(ok.contains("staged profile"));
    }

    #[test]
    fn profile_save_requires_a_name() {
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"x\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // A capability but no name → rejected; the editor comes back with the
        // reason inline (not a bare error banner), and nothing is staged.
        let err = body_of(route(
            &st,
            &req(
                "POST",
                "/profiles",
                "",
                &[HOST, COOKIE, ORIGIN],
                "new=1&targets=rust&capabilities=rc&scope=repo",
            ),
        ));
        assert!(err.contains("name is required")); // inline error
        assert!(err.contains("banner error")); // shown in the editor, not a fragment
        assert!(err.contains("New profile")); // re-rendered in new-profile mode
        assert!(err.contains("name=\"name\"")); // with the editable name field

        // Nothing was staged by the failed save.
        let diff = body_of(route(&st, &req("GET", "/diff", "", &[HOST, COOKIE], "")));
        assert!(diff.contains("No staged changes"));
    }

    #[test]
    fn profiles_tab_does_not_auto_select() {
        // A rust profile that *would* be the bound candidate in this repo.
        let cfg = "[[capabilities]]\nid = \"rc\"\nguidance = \"x\"\n\
             \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        let body = body_of(route(
            &st,
            &req("GET", "/tab/profiles", "", &[HOST, COOKIE], ""),
        ));
        // The rail lists the profile, but the main pane shows the pick prompt —
        // no profile is auto-selected, so no detail/cards render.
        assert!(body.contains("Select a profile to see what it composes."));
        assert!(!body.contains("cap-detail"));
        assert!(!body.contains("<h1>rust</h1>"));
        // Explicitly selecting one still renders its detail.
        let detail = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(detail.contains("<h1>rust</h1>") && detail.contains("cap-detail"));
    }

    #[test]
    fn diff_surfaces_leak_warning_for_public_config() {
        // A capability whose guidance carries a machine-specific literal in the
        // public (global) config.toml is leak-linted before apply. The command
        // is global-authored → trusted, so there is no trust banner (that gate
        // is dormant now that repos can't author commands).
        let cfg = "[[capabilities]]\n\
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
    fn capability_editor_form_loads_for_palette_and_owned() {
        let cfg = "[[capabilities]]\nid = \"mine\"\nguidance = \"owned\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Owned cap → editable form.
        let owned = body_of(route(
            &st,
            &req("GET", "/capabilities/mine/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(owned.contains("Edit capability"));
        // Editing offers Save (in place) + Save as a copy + Delete.
        assert!(owned.contains("Save"));
        assert!(owned.contains("Save as a copy"));
        assert!(owned.contains("Delete"));
        assert!(owned.contains("/capabilities/mine"));

        // Opened from a profile's detail, the form carries a return_profile so
        // Save re-renders that profile rather than the Capabilities tab.
        let from_profile = body_of(route(
            &st,
            &req(
                "GET",
                "/capabilities/mine/edit",
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
                "/capabilities/rust-conventions/view",
                "",
                &[HOST, COOKIE],
                "",
            ),
        ));
        assert!(palette.contains("Palette capability"));
        assert!(palette.contains("Duplicate"));
    }

    #[test]
    fn script_capability_is_editable_not_advanced() {
        // A command cap with no guidance template opens in the editor (script
        // field + highlight overlay), not the read-only "advanced — edit in TOML"
        // dialog. (Pairing a command with a guidance template is what makes a cap
        // advanced; a plain script is fully editable.)
        let cfg = "[[capabilities]]\nid = \"host\"\ndescription = \"Host\"\n\
             script_lang = \"bash\"\ncommand = \"uname -a\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/capabilities/host/edit", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("Edit capability"));
        assert!(!body.contains("Advanced capability"));
        // The highlight-overlay script field is present, prefilled with the command.
        assert!(body.contains("code-edit") && body.contains("name=\"command\""));
        assert!(body.contains("uname -a"));
    }

    #[test]
    fn dynamic_cap_without_cache_shows_run_prompt() {
        // A provider cap doesn't run in the read-only preview; with nothing cached
        // the overlay drops its section, but the profile detail keeps the card and
        // offers a centered "Run" prompt (so it's still listed, openable, runnable).
        let cfg = "[[capabilities]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));
        let body = body_of(route(
            &st,
            &req("GET", "/profiles/rust/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(body.contains("cap-detail")); // the card is present
        assert!(body.contains("cap-run-prompt") && body.contains("Run script")); // centered prompt
        assert!(body.contains("/capabilities/host/run?profile=rust")); // wired to run
    }

    #[test]
    fn run_all_executes_dynamic_caps_live() {
        // The `host` provider is always trusted, so a Live run produces output.
        let cfg = "[[capabilities]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[profiles]]\nname = \"m\"\ntargets = [\"rust\"]\ncapabilities = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Read-only preview: a run prompt, no real output yet.
        let before = body_of(route(
            &st,
            &req("GET", "/profiles/m/select", "", &[HOST, COOKIE], ""),
        ));
        assert!(before.contains("cap-run-prompt") && !before.contains("cap-output"));

        // Run all → live render executes the provider and shows verbatim output.
        let after = body_of(route(
            &st,
            &req("POST", "/profiles/m/run", "", &[HOST, COOKIE, ORIGIN], ""),
        ));
        assert!(!after.contains("runs at render"));
        assert!(after.contains("cap-output")); // rendered as preformatted output

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
    fn per_card_run_executes_one_capability() {
        let cfg = "[[capabilities]]\nid = \"host\"\ndescription = \"Host\"\nprovider = \"host\"\n\
             \n[[profiles]]\nname = \"m\"\ntargets = [\"rust\"]\ncapabilities = [\"host\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Running one cap caches its output; the re-rendered detail shows it.
        let after = body_of(route(
            &st,
            &req(
                "POST",
                "/capabilities/host/run",
                "profile=m",
                &[HOST, COOKIE, ORIGIN],
                "",
            ),
        ));
        assert!(!after.contains("runs at render"));
        assert!(after.contains("cap-output"));
    }

    #[test]
    fn save_as_copy_creates_a_new_capability_under_a_new_name() {
        let cfg =
            "[[capabilities]]\nid = \"rc\"\ndescription = \"Rust conv\"\nguidance = \"Old.\"\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Save as a copy with a new title → a new id; the original is untouched.
        let body = body_of(route(
            &st,
            &req(
                "POST",
                "/capabilities",
                "as=copy",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&name=Rust+strict&kind=markdown&guidance=New+body&scope=repo&visibility=public",
            ),
        ));
        assert!(body.contains("saved copy"));

        // Apply and confirm the copy landed under a new id with the original
        // capability left intact.
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
                "/capabilities",
                "as=copy",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&name=rc&kind=markdown&guidance=x&scope=repo",
            ),
        ));
        assert!(err.contains("already exists"));
    }

    #[test]
    fn editing_a_cap_from_a_profile_returns_the_profile_detail() {
        let cfg = "[[capabilities]]\n\
             id = \"rc\"\nguidance = \"Old guidance.\"\n\
             \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // Saving with return_profile set re-renders the profile's detail (the
        // updated guidance shows in a cap card) rather than the Capabilities tab.
        let body = body_of(route(
            &st,
            &req(
                "POST",
                "/capabilities",
                "",
                &[HOST, COOKIE, ORIGIN],
                "id=rc&return_profile=rust&name=rc&kind=markdown&guidance=Fresh+guidance&scope=repo",
            ),
        ));
        assert!(body.contains("<h1>rust</h1>")); // the profile detail, not the caps tab
        assert!(body.contains("cap-detail"));
        assert!(body.contains("Fresh guidance"));
        assert!(body.contains("/close")); // modal-close loader appended
    }
}
