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
fn viewer_selftest_passes_under_opaque_origin_sandbox() {
    let chrome = chrome().expect("no Chrome found; set CHROME_BIN");
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/plan/kitchen-sink.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let plan = loadout::plan::model::parse(&raw, false).unwrap().plan;
    let html = loadout::plan::render::render(&plan);

    // Opaque-origin proof via <iframe sandbox="allow-scripts" srcdoc=…> in a
    // file:// harness — the sandbox attribute sets the exact flag set the
    // studio's `Content-Security-Policy: sandbox allow-scripts` response
    // header does (spec-identical origin semantics). Header DELIVERY is
    // pinned end-to-end by tests/studio.rs
    // (recents_artifact_is_served_with_sandbox_csp_over_tcp) and the route()
    // tests; this test's job is "does the page FUNCTION under the opaque
    // origin".
    //
    // Why this shape — every simpler variant fails on some Chrome build:
    //  * real-socket serving + --virtual-time-budget: CI's google-chrome
    //    dumps an EMPTY dom (virtual time races real I/O — network AND the
    //    clipboard IPC the copy probe depends on);
    //  * --timeout / paced responses: --dump-dom fires at load-complete,
    //    BEFORE the async selftest marker attaches, and the production CSP
    //    (default-src 'none') blocks every subresource instantly, so the
    //    page's own load event cannot be delayed;
    //  * <iframe src="file:…">: a sandboxed (opaque) frame may not load
    //    file: URLs, hence srcdoc; an about:srcdoc document has no URL
    //    fragment, hence the window.name="loadout-selftest" trigger.
    //
    // The dump must wait for the frame's ASYNC selftest on a REAL clock
    // (the clipboard probe needs real IPC + an 800ms fallback timer), so
    // the harness holds its own load event open with a subresource Chrome
    // cannot finish reading: a FIFO. The test writes to the pipe after the
    // selftest window has passed; only then does load fire and --dump-dom
    // serialize — with the relayed verdict in the top document.
    // (--dump-dom serializes only the top document; a sandboxed iframe's
    // DOM is invisible to it, which is why plan.js relays its marker to
    // the parent via postMessage when framed.)
    let dir = tempfile::tempdir().unwrap();
    let escaped = html.replace('&', "&amp;").replace('"', "&quot;");
    let harness = format!(
        r#"<!doctype html>
<html><body>
<iframe name="loadout-selftest" sandbox="allow-scripts" srcdoc="{escaped}"></iframe>
<iframe src="hold.pipe" style="display:none"></iframe>
<script>
  window.addEventListener("message", function (e) {{
    var pre = document.createElement("pre");
    pre.id = "selftest-relay";
    pre.textContent = String(e.data);
    document.body.appendChild(pre);
  }});
</script>
</body></html>
"#
    );
    let harness_path = dir.path().join("harness.html");
    std::fs::write(&harness_path, harness).unwrap();
    let pipe = dir.path().join("hold.pipe");
    let mkfifo = Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("mkfifo runs");
    assert!(mkfifo.success(), "mkfifo failed");

    // An empty/relay-less dump retries (launch hiccups); a dump WITH the
    // relay is the page's actual verdict and is asserted immediately, never
    // retried (retrying a real FAIL would mask it). stderr rides along in
    // the panic so a CI failure names its cause.
    let mut dom = String::new();
    let mut stderr = String::new();
    for attempt in 0..3 {
        // Release the load event 4s after launch: enough real time for the
        // frame to parse (~0.5s) and the selftest's slowest probe (an 800ms
        // clipboard-fallback timer) to settle. fs::write blocks until Chrome
        // opens the pipe for reading, then EOFs the subresource.
        let pipe_writer = pipe.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(4));
            let _ = std::fs::write(&pipe_writer, "done\n");
        });
        let out = Command::new(&chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--dump-dom",
            ])
            .arg(format!("file://{}", harness_path.display()))
            .output()
            .expect("chrome runs");
        dom = String::from_utf8_lossy(&out.stdout).into_owned();
        stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if dom.contains("id=\"selftest-relay\"") {
            break;
        }
        eprintln!("attempt {attempt}: no relay in dump; stderr:\n{stderr}");
    }
    // Anchor every assertion on the RELAY element's text, never the bare
    // literals: the srcdoc attribute embeds plan.js's own source (where
    // "LOADOUT_SELFTEST_PASS" exists as a ternary literal), so unanchored
    // contains() would be satisfied by a page that never ran — the same
    // tautology class the file:// test's comment warns about. The relay
    // prefix + REAL newline below cannot occur in the attribute (there the
    // \n is two source characters, not a newline).
    assert!(
        dom.contains("id=\"selftest-relay\">LOADOUT_SELFTEST_RELAY"),
        "no selftest relay from the sandboxed iframe; DOM tail:\n{}\nchrome stderr tail:\n{}",
        char_boundary_tail(&dom, 2000),
        char_boundary_tail(&stderr, 2000)
    );
    let relay_at = dom.find("id=\"selftest-relay\"").unwrap();
    let relay = &dom[relay_at..];
    assert!(
        relay.contains("LOADOUT_SELFTEST_RELAY\nLOADOUT_SELFTEST_PASS"),
        "sandboxed selftest failed; relay:\n{}",
        char_boundary_tail(relay, 2000)
    );
    for probe in [
        "PASS fetch blocked by CSP",
        "PASS copy terminates handled",
        "PASS storage guarded",
    ] {
        assert!(
            relay.contains(probe),
            "missing '{probe}' in relay:\n{}",
            char_boundary_tail(relay, 2000)
        );
    }
}
