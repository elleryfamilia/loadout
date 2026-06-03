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
use crate::studio::assets;
use crate::studio::edit::{Session, StagedOp};
use crate::studio::state::{
    self, BindingState, OnboardingStage, PreviewOutcome, Simulated, StudioState,
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
        ("GET", "/welcome") => Resp::html(views::center_placeholder_fragment()),
        ("GET", "/library") => handle_library(state),
        ("POST", "/preview") => handle_preview(state, req),
        ("GET", "/fs-status") => handle_fs_status(state),
        ("GET", "/diff") => handle_diff(state),
        ("POST", "/apply") => handle_apply(state),
        ("POST", "/trust/allow") => handle_trust(state, true),
        ("POST", "/trust/deny") => handle_trust(state, false),
        ("GET", "/capabilities/new") => Resp::html(views::capability_form(None, Layer::Repo, true)),
        ("POST", "/capabilities") => handle_cap_save(state, req),
        ("GET", "/profiles/new") => handle_profile_new(state),
        ("POST", "/profiles") => handle_profile_save(state, req),
        ("GET", p) if p.starts_with("/assets/") => match assets::get(p) {
            Some((body, ct)) => Resp::asset(body, ct),
            None => Resp::not_found(),
        },
        (m, p) if p.starts_with("/capabilities/") => handle_cap_param(state, m, p),
        (m, p) if p.starts_with("/profiles/") => handle_profile_param(state, m, p),
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

fn handle_cap_param(state: &Arc<Mutex<StudioState>>, method: &str, path: &str) -> Resp {
    let (id, action) = id_and_action(path, "/capabilities/");
    match (method, action) {
        ("GET", "edit") => handle_cap_edit(state, &id),
        ("DELETE", "") => handle_cap_delete(state, &id),
        ("POST", "duplicate") => handle_cap_duplicate(state, &id),
        _ => Resp::not_found(),
    }
}

fn handle_profile_param(state: &Arc<Mutex<StudioState>>, method: &str, path: &str) -> Resp {
    let (name, action) = id_and_action(path, "/profiles/");
    match (method, action) {
        ("GET", "edit") => handle_profile_edit(state, &name),
        ("DELETE", "") => handle_profile_delete(state, &name),
        _ => Resp::not_found(),
    }
}

// --- handlers (snapshot under the lock, render outside it) -------------------

fn handle_shell(state: &Arc<Mutex<StudioState>>) -> Resp {
    let (snap, staged) = {
        let s = state.lock().unwrap();
        (s.snapshot(), s.session.ops().len())
    };
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_page(&e.to_string())),
    };
    let agents: Vec<String> = cfg.agents.iter().map(|a| a.id.clone()).collect();
    let lib = match state::library_view(&snap) {
        Ok(l) => l,
        Err(e) => return Resp::html(views::error_page(&e.to_string())),
    };
    // A render error surfaces inline (note), never a 500.
    let preview = state::render_preview(&snap).unwrap_or_else(|e| PreviewOutcome {
        agent: snap.sim.agent.clone(),
        profile_label: "none".to_string(),
        binding: BindingState::None,
        context_summary: String::new(),
        cap_count: 0,
        overlay: String::new(),
        note: Some(format!("preview error: {e}")),
    });
    let stage = OnboardingStage::of(&lib, &preview.binding);
    Resp::html(views::shell(
        &lib, staged, &snap.sim, &agents, &preview, stage,
    ))
}

fn handle_library(state: &Arc<Mutex<StudioState>>) -> Resp {
    let (snap, staged) = {
        let s = state.lock().unwrap();
        (s.snapshot(), s.session.ops().len())
    };
    match state::library_view(&snap) {
        Ok(l) => Resp::html(views::library_fragment(&l, staged)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

// --- write handlers (Slice 2) ------------------------------------------------

fn handle_cap_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let cap = match state::capability_from_form(&pairs) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
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
    match res {
        Ok(()) => Resp::html(views::action_result(&format!("✓ staged capability “{id}”"))),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_cap_edit(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    let cfg = match state::staged_config(&snap) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    if let Some(c) = cfg.capabilities.iter().find(|c| c.id == id) {
        Resp::html(views::capability_form(Some(c), c.origin, true))
    } else if let Some(c) = palette().into_iter().find(|c| c.id == id) {
        Resp::html(views::capability_form(Some(&c), Layer::Repo, false))
    } else {
        Resp::html(views::error_fragment(&format!("unknown capability '{id}'")))
    }
}

fn handle_cap_delete(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let mut s = state.lock().unwrap();
    match s.session.capability_layer(id) {
        Some(layer) => match s.session.stage(StagedOp::DeleteCapability {
            layer,
            id: id.to_string(),
        }) {
            Ok(()) => Resp::html(views::action_result(&format!(
                "✓ staged deletion of “{id}”"
            ))),
            Err(e) => Resp::html(views::error_fragment(&e.to_string())),
        },
        None => Resp::html(views::error_fragment(&format!(
            "“{id}” isn't in your library — palette items can't be deleted"
        ))),
    }
}

fn handle_cap_duplicate(state: &Arc<Mutex<StudioState>>, id: &str) -> Resp {
    let res = state
        .lock()
        .unwrap()
        .session
        .stage(StagedOp::DuplicatePaletteItem {
            id: id.to_string(),
            to_layer: Layer::Repo,
        });
    match res {
        Ok(()) => Resp::html(views::action_result(&format!(
            "✓ duplicated “{id}” into your repo config — edit it from YOURS"
        ))),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_profile_new(state: &Arc<Mutex<StudioState>>) -> Resp {
    let texts = state.lock().unwrap().session.staged_layer_texts();
    Resp::html(views::profile_form(None, &available_capabilities(texts)))
}

fn handle_profile_edit(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let texts = state.lock().unwrap().session.staged_layer_texts();
    let available = available_capabilities(texts.clone());
    let cfg = match Config::from_layer_strs(texts) {
        Ok(c) => c,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    match cfg.profiles.iter().find(|p| p.name == name) {
        Some(p) => Resp::html(views::profile_form(Some(p), &available)),
        None => Resp::html(views::error_fragment(&format!("unknown profile '{name}'"))),
    }
}

fn handle_profile_save(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let pairs = state::parse_pairs(&req.body);
    let profile = match state::profile_from_form(&pairs) {
        Ok(p) => p,
        Err(e) => return Resp::html(views::error_fragment(&e.to_string())),
    };
    let layer = state::layer_from_form(&pairs); // profiles are public; visibility absent → Repo/Global
    let name = profile.name.clone();
    let res = state.lock().unwrap().session.stage(StagedOp::EditProfile {
        layer,
        name: name.clone(),
        profile: Box::new(profile),
    });
    match res {
        Ok(()) => Resp::html(views::action_result(&format!("✓ staged profile “{name}”"))),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

fn handle_profile_delete(state: &Arc<Mutex<StudioState>>, name: &str) -> Resp {
    let mut s = state.lock().unwrap();
    match s.session.profile_layer(name) {
        Some(layer) => match s.session.stage(StagedOp::DeleteProfile {
            layer,
            name: name.to_string(),
        }) {
            Ok(()) => Resp::html(views::action_result(&format!(
                "✓ staged deletion of “{name}”"
            ))),
            Err(e) => Resp::html(views::error_fragment(&e.to_string())),
        },
        None => Resp::html(views::error_fragment(&format!("unknown profile '{name}'"))),
    }
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
        Ok(written) => Resp::html(views::action_result(&format!(
            "✓ applied {} file change(s) — command caps may need re-allowing",
            written.len()
        ))),
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
        Ok(()) => Resp::html(views::action_result(if allow {
            "✓ repo trusted — its command capabilities can run"
        } else {
            "✓ trust revoked for this repo"
        })),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
    }
}

/// Capability ids you can compose into a profile: your library plus palette
/// items you haven't duplicated yet.
fn available_capabilities(texts: Vec<(Layer, std::path::PathBuf, String)>) -> Vec<String> {
    let mut ids: Vec<String> = Config::from_layer_strs(texts)
        .map(|c| c.capabilities.iter().map(|c| c.id.clone()).collect())
        .unwrap_or_default();
    let owned: std::collections::HashSet<String> = ids.iter().cloned().collect();
    for c in palette() {
        if !owned.contains(&c.id) {
            ids.push(c.id);
        }
    }
    ids
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

fn handle_preview(state: &Arc<Mutex<StudioState>>, req: &Req) -> Resp {
    let snap = {
        let mut s = state.lock().unwrap();
        s.sim.update_from_form(&req.body);
        s.snapshot()
    };
    match state::render_preview(&snap) {
        Ok(p) => Resp::html(views::preview_fragment(&p)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
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
        if let Some(c) = cfg_toml {
            std::fs::create_dir_all(config::repo_dir(repo)).unwrap();
            std::fs::write(config::repo_config_path(repo), c).unwrap();
        }
        let config = Config::load_from(None, repo).unwrap();
        let base_context = context::detect_context(repo, &config).unwrap();
        let session = Session::open(repo, None).unwrap();
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
        assert!(body.contains("Live overlay"));
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
    fn preview_requires_origin_then_renders_selected_profile() {
        let cfg = "[[capabilities]]\n\
             id = \"rc\"\n\
             description = \"Rust conv\"\n\
             guidance = \"Use clippy here.\"\n\
             \n\
             [[profiles]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             capabilities = [\"rc\"]\n";
        let d = rust_repo();
        let st = state_for(d.path(), Some(cfg));

        // POST without Origin → CSRF guard rejects.
        let r = route(
            &st,
            &req("POST", "/preview", "", &[HOST, COOKIE], "lang=rust"),
        );
        assert_eq!(r.status, 403);

        // With Origin → renders the selected rust profile's overlay.
        let r = route(
            &st,
            &req("POST", "/preview", "", &[HOST, COOKIE, ORIGIN], "lang=rust"),
        );
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("profile rust"));
        assert!(body.contains("Use clippy here."));
    }

    #[test]
    fn preview_empty_when_no_profile_matches() {
        let d = rust_repo();
        let st = state_for(d.path(), None); // no profiles configured
        let r = route(
            &st,
            &req("POST", "/preview", "", &[HOST, COOKIE, ORIGIN], "lang=rust"),
        );
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("profile none"));
        assert!(body.contains("No profile applies"));
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
                "id=rc&description=Rust&guidance=Use+clippy&risk=info&scope=repo&visibility=public",
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

        let on_disk = std::fs::read_to_string(config::repo_config_path(d.path())).unwrap();
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
        let on_disk = std::fs::read_to_string(config::repo_config_path(d.path())).unwrap();
        assert!(!on_disk.contains("id = \"rc\""));
    }

    #[test]
    fn duplicate_palette_item_stages_into_repo() {
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
    fn diff_surfaces_leak_warning_and_trust_banner() {
        // A repo command cap whose guidance carries a machine-specific literal.
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
        // Trust banner appears for the repo command cap (untrusted by default).
        assert!(diff.contains("trust this repo") || diff.contains("Allow this repo"));
        assert!(diff.contains("deploy"));
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
        assert!(owned.contains("Stage change"));

        // Palette cap → read-only with a duplicate action.
        let palette = body_of(route(
            &st,
            &req(
                "GET",
                "/capabilities/rust-conventions/edit",
                "",
                &[HOST, COOKIE],
                "",
            ),
        ));
        assert!(palette.contains("read-only"));
        assert!(palette.contains("Duplicate"));
    }
}
