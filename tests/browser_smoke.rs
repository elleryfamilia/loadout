//! Headless-Chrome smoke test for the plan viewer's JS (`#selftest` mode).
//! `#[ignore]`d locally (needs Chrome); CI runs it explicitly.

use std::process::Command;

fn chrome() -> Option<String> {
    if let Ok(c) = std::env::var("CHROME_BIN") {
        return Some(c);
    }
    for c in [
        "google-chrome",
        "chromium",
        "chromium-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        if Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(c.to_string());
        }
    }
    None
}

#[test]
#[ignore = "needs Chrome; CI runs it via `cargo test --test browser_smoke -- --ignored`"]
fn viewer_selftest_passes_in_headless_chrome() {
    let chrome = chrome().expect("no Chrome found; set CHROME_BIN");
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/plan/kitchen-sink.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let plan = loadout::plan::model::parse(&raw, false).unwrap().plan;
    let html = loadout::plan::render::render(&plan);
    let dir = tempfile::tempdir().unwrap();
    let page = dir.path().join("plan.html");
    std::fs::write(&page, html).unwrap();

    let out = Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--virtual-time-budget=5000",
            "--dump-dom",
        ])
        .arg(format!("file://{}#selftest", page.display()))
        .output()
        .expect("chrome runs");
    let dom = String::from_utf8_lossy(&out.stdout);
    // Anchor on the marker *element* the selftest injects into the DOM
    // (`<pre id="selftest-result">LOADOUT_SELFTEST_PASS…</pre>`), not the
    // bare string `LOADOUT_SELFTEST_PASS` -- that literal also sits in
    // plan.js's own source, which `--dump-dom` serializes into the page
    // regardless of whether the selftest ran or passed.
    assert!(
        dom.contains("id=\"selftest-result\">LOADOUT_SELFTEST_PASS"),
        "selftest failed; DOM tail:\n{}",
        char_boundary_tail(&dom, 2000)
    );
}

/// The last `max_len` bytes of `s`, trimmed back to the nearest char
/// boundary so the slice never panics on a multibyte codepoint.
fn char_boundary_tail(s: &str, max_len: usize) -> &str {
    let start = s.len().saturating_sub(max_len);
    let start = (start..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    &s[start..]
}

#[test]
#[ignore = "needs Chrome; CI runs it via `cargo test --test browser_smoke -- --ignored`"]
fn viewer_selftest_passes_when_served_with_sandbox_csp() {
    let chrome = chrome().expect("no Chrome found; set CHROME_BIN");
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/plan/kitchen-sink.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let plan = loadout::plan::model::parse(&raw, false).unwrap().plan;
    let html = loadout::plan::render::render(&plan).into_bytes();

    // A bare localhost server replaying the EXACT production header set
    // (loadout::recents::serve_header_pairs — single source with the studio
    // handler). Guard behavior is route()-tested; this test's only job is
    // "does the page work under the opaque origin".
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        while let Ok(req) = server.recv() {
            let mut resp = tiny_http::Response::from_data(html.clone());
            for (k, v) in loadout::recents::serve_header_pairs() {
                resp.add_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
            }
            let _ = req.respond(resp);
        }
    });

    let out = Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--virtual-time-budget=5000",
            "--dump-dom",
        ])
        .arg(format!("http://127.0.0.1:{port}/plan.html#selftest"))
        .output()
        .expect("chrome runs");
    let dom = String::from_utf8_lossy(&out.stdout);
    assert!(
        dom.contains("id=\"selftest-result\">LOADOUT_SELFTEST_PASS"),
        "sandboxed selftest failed; DOM tail:\n{}",
        char_boundary_tail(&dom, 2000)
    );
    // The served-context probes actually ran (they are gated off file://).
    assert!(
        dom.contains("PASS fetch blocked by CSP"),
        "{}",
        char_boundary_tail(&dom, 2000)
    );
    assert!(
        dom.contains("PASS copy terminates handled"),
        "{}",
        char_boundary_tail(&dom, 2000)
    );
    assert!(
        dom.contains("PASS storage guarded"),
        "{}",
        char_boundary_tail(&dom, 2000)
    );
}
