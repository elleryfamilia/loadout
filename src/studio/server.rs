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

use crate::cli::StudioArgs;
use crate::commands::Runtime;
use crate::config::{self, Config};
use crate::context;
use crate::studio::edit::Session;
use crate::studio::state::{self, PreviewOutcome, Simulated, StudioState};
use crate::studio::{assets, views};

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

    // 5. Dispatch (Slice 1 is read-only).
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => handle_shell(state),
        ("GET", "/library") => handle_library(state),
        ("POST", "/preview") => handle_preview(state, req),
        ("GET", "/fs-status") => handle_fs_status(state),
        ("GET", p) if p.starts_with("/assets/") => match assets::get(p) {
            Some((body, ct)) => Resp::asset(body, ct),
            None => Resp::not_found(),
        },
        _ => Resp::not_found(),
    }
}

// --- handlers (snapshot under the lock, render outside it) -------------------

fn handle_shell(state: &Arc<Mutex<StudioState>>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
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
        overlay: String::new(),
        note: Some(format!("preview error: {e}")),
    });
    Resp::html(views::shell(&lib, &snap.sim, &agents, &preview))
}

fn handle_library(state: &Arc<Mutex<StudioState>>) -> Resp {
    let snap = state.lock().unwrap().snapshot();
    match state::library_view(&snap) {
        Ok(l) => Resp::html(views::library_fragment(&l)),
        Err(e) => Resp::html(views::error_fragment(&e.to_string())),
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
}
