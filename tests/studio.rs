//! Socket-level smoke test for `rosita studio`: the real binary binds
//! 127.0.0.1, prints a bootstrap URL, and serves the secured spine over TCP.
//!
//! The other studio coverage drives the router directly (see `src/studio/*`);
//! this exercises the actual server loop, the bootstrap→cookie→shell flow, and
//! the Host-header guard end-to-end, then kills the child.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Spawn `rosita studio --no-open --port 0` in an isolated tempdir and parse the
/// bound port + session token from its startup output.
fn spawn_studio() -> (Child, tempfile::TempDir, u16, String) {
    let dir = tempfile::tempdir().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("rosita");
    let mut child = Command::new(bin)
        .args(["--cwd"])
        .arg(dir.path())
        .args(["studio", "--no-open", "--port", "0"])
        // Isolate the global config dir so the test never touches ~/.config.
        .env("ROSITA_CONFIG_DIR", dir.path().join("empty-global"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rosita studio");

    // Rust's stdout is line-buffered, so the banner lines arrive immediately.
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut url = String::new();
    for _ in 0..8 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(idx) = line.find("http://127.0.0.1:") {
            url = line[idx..].trim().to_string();
            break;
        }
    }
    assert!(!url.is_empty(), "studio did not print a bootstrap URL");

    // http://127.0.0.1:<port>/__studio/bootstrap?token=<token>
    let after_host = url.strip_prefix("http://127.0.0.1:").unwrap();
    let port: u16 = after_host
        .split('/')
        .next()
        .unwrap()
        .parse()
        .expect("parse port");
    let token = url.split("token=").nth(1).unwrap().to_string();
    (child, dir, port, token)
}

/// Send one raw HTTP/1.1 request and return the full response text.
fn http(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to studio");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn studio_binds_and_serves_secured_spine() {
    let (mut child, _dir, port, token) = spawn_studio();

    // 1. Bootstrap with the token → 302 + Set-Cookie.
    let boot = http(
        port,
        &format!(
            "GET /__studio/bootstrap?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        ),
    );

    // 2. The shell, with the session cookie → 200 + the page.
    let shell = http(
        port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nConnection: close\r\n\r\n"
        ),
    );

    // 3. The shell without the cookie → 403 (guard wraps GETs too).
    let no_cookie = http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );

    // 4. A forged Host header (DNS-rebinding) → 403.
    let bad_host = http(
        port,
        &format!(
            "GET / HTTP/1.1\r\nHost: evil.test\r\nCookie: rosita_studio={token}\r\nConnection: close\r\n\r\n"
        ),
    );

    // 5. A write over the real socket: stage a capability (exercises POST-body
    //    parsing + the Origin guard end-to-end), then apply it. Capabilities are
    //    global-only, so it lands in the global config dir (ROSITA_CONFIG_DIR).
    let body = "name=smoke&kind=markdown&guidance=hello&visibility=public";
    let create = http(
        port,
        &format!(
            "POST /capabilities HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    let apply = http(
        port,
        &format!(
            "POST /apply HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    let written =
        std::fs::read_to_string(_dir.path().join("empty-global/config.toml")).unwrap_or_default();

    // A POST without Origin must be refused (CSRF guard) over the socket too.
    let no_origin = http(
        port,
        &format!(
            "POST /apply HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );

    // Kill before asserting so a failure never leaks the server process.
    child.kill().ok();
    child.wait().ok();

    assert!(
        boot.starts_with("HTTP/1.1 302"),
        "bootstrap → 302; got:\n{boot}"
    );
    assert!(
        boot.to_lowercase().contains("set-cookie: rosita_studio="),
        "bootstrap sets the session cookie; got:\n{boot}"
    );

    assert!(
        shell.starts_with("HTTP/1.1 200"),
        "shell → 200; got head:\n{}",
        head(&shell)
    );
    assert!(shell.contains("rosita studio"), "shell renders the page");
    assert!(
        shell.contains("Profiles") && shell.contains("Capabilities"),
        "shell renders the Profiles/Capabilities tabs"
    );

    assert!(
        no_cookie.starts_with("HTTP/1.1 403"),
        "no cookie → 403; got:\n{}",
        head(&no_cookie)
    );
    assert!(
        bad_host.starts_with("HTTP/1.1 403"),
        "bad Host → 403; got:\n{}",
        head(&bad_host)
    );

    assert!(
        create.starts_with("HTTP/1.1 200"),
        "create → 200; got:\n{}",
        head(&create)
    );
    assert!(
        create.contains("staged capability"),
        "create stages the capability; got:\n{create}"
    );
    assert!(
        apply.starts_with("HTTP/1.1 200"),
        "apply → 200; got:\n{}",
        head(&apply)
    );
    assert!(
        written.contains("id = \"smoke\""),
        "apply wrote the capability to disk; got:\n{written}"
    );
    assert!(
        no_origin.starts_with("HTTP/1.1 403"),
        "POST without Origin → 403; got:\n{}",
        head(&no_origin)
    );
}

#[test]
fn studio_packs_gallery_applies_a_pack_over_socket() {
    let (mut child, dir, port, token) = spawn_studio();

    // Bootstrap (the session cookie is just the token).
    let _ = http(
        port,
        &format!(
            "GET /__studio/bootstrap?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        ),
    );

    // The starter-pack gallery renders with per-pack apply actions.
    let gallery = http(
        port,
        &format!(
            "GET /packs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nConnection: close\r\n\r\n"
        ),
    );

    // Apply the everyday pack (stages caps + profile), then commit to disk.
    let apply_pack = http(
        port,
        &format!(
            "POST /packs/everyday/apply HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    let apply = http(
        port,
        &format!(
            "POST /apply HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: rosita_studio={token}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    let written =
        std::fs::read_to_string(dir.path().join("empty-global/config.toml")).unwrap_or_default();

    // Kill before asserting so a failure never leaks the server process.
    child.kill().ok();
    child.wait().ok();

    assert!(
        gallery.starts_with("HTTP/1.1 200"),
        "gallery → 200; got:\n{}",
        head(&gallery)
    );
    assert!(
        gallery.contains("Starter packs"),
        "gallery renders; got head:\n{}",
        head(&gallery)
    );
    assert!(
        gallery.contains("/packs/everyday/apply"),
        "gallery offers the everyday pack"
    );
    assert!(
        apply_pack.starts_with("HTTP/1.1 200"),
        "apply-pack → 200; got:\n{}",
        head(&apply_pack)
    );
    assert!(
        apply_pack.contains("staged the"),
        "apply-pack stages the pack; got:\n{apply_pack}"
    );
    assert!(
        apply.starts_with("HTTP/1.1 200"),
        "apply → 200; got:\n{}",
        head(&apply)
    );
    assert!(
        written.contains("name = \"everyday\""),
        "apply wrote the everyday profile; got:\n{written}"
    );
    assert!(
        written.contains("id = \"terse-comms\""),
        "apply wrote the pack's capabilities; got:\n{written}"
    );
}

fn head(resp: &str) -> String {
    resp.lines().take(3).collect::<Vec<_>>().join("\n")
}
