//! Headless-Chrome smoke tests: the plan viewer's JS (`#selftest` mode) and
//! studio's topbar layout. `#[ignore]`d locally (needs Chrome); CI runs them
//! explicitly.

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

// --- studio topbar responsiveness -------------------------------------------

/// The widths the topbar is required to handle without overlap or
/// horizontal overflow (R2 brief): 1400px down to 380px. 1360/1361 are the
/// tightest sampled points straddling the compact-mode breakpoint
/// (`studio.css`, currently 1360px) -- 1360 is the last compact width, 1361
/// the first full-label one, so together they catch the breakpoint being
/// set too low. 1301/1310 are kept from an earlier (lower) breakpoint value
/// as a regression check that they stay comfortably inside compact mode.
/// Everything else here is round.
const TOPBAR_TEST_WIDTHS: [u32; 12] = [
    1400, 1361, 1360, 1310, 1301, 1200, 1024, 900, 768, 600, 480, 380,
];

/// Expected count of measured pieces per group (brand: mark + wordmark;
/// tabs: the two nav buttons; topbar-right: the staged count + its three
/// action buttons + the four icon buttons). Asserted at every width so a
/// selector rename can't silently empty a group -- an empty group can never
/// overlap anything, which would make the overlap assertion pass having
/// measured nothing.
const TOPBAR_EXPECTED_PIECE_COUNTS: [(&str, u32); 3] =
    [("brand", 2), ("tabs", 2), ("topbar-right", 8)];

/// Injected into the harness page below (`__WIDTHS__` is substituted with the
/// literal widths array first). Runs once every iframe has loaded, measures
/// each of `.brand`/`.tabs`/`.topbar-right` inside each one, and writes a
/// single `{width: {innerWidth, scrollWidth, overlaps, counts}}` JSON blob
/// into a `<pre id="topbar-check-result">` for `--dump-dom` to capture --
/// the same marker-element pattern the `#selftest-result` probe above uses,
/// rather than grepping the dump for a bare string that could also appear
/// in inlined source.
///
/// Each "group" is a *list* of its meaningful rendered pieces, not one
/// `getBoundingClientRect()` on the group's own wrapper: a flex item that
/// can't shrink to fit (unhidden label text, a nowrap button) keeps its own
/// natural size and paints outside its shrunk *parent* box without ever
/// enlarging that parent's own rect, so comparing only the three wrapper
/// boxes misses exactly the collision this bug produces (confirmed by
/// running this test against the pre-fix CSS: the wrapper boxes stayed
/// clear at every width while brand/tab/button text visibly overlapped on
/// screen). Comparing every piece against every other group's pieces
/// catches that. `counts` (each group's piece count) guards against the
/// same check passing vacuously if a selector ever stops matching anything.
const TOPBAR_HARNESS_JS: &str = r#"
<script>
window.addEventListener('load', function () {
  function pieces(doc, sels) {
    var out = [];
    sels.forEach(function (sel) {
      doc.querySelectorAll(sel).forEach(function (el) {
        var r = el.getBoundingClientRect();
        var label = (el.title || el.textContent || '').trim().slice(0, 24);
        out.push({ sel: sel, label: label, left: r.left, right: r.right, top: r.top, bottom: r.bottom });
      });
    });
    return out;
  }
  function overlaps(a, b) {
    return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
  }
  var widths = [__WIDTHS__];
  var results = {};
  widths.forEach(function (w) {
    var frame = document.getElementById('w-' + w);
    var doc = frame.contentDocument;
    var win = frame.contentWindow;
    var groups = {
      brand: pieces(doc, ['.brand-mark', '.brand-name']),
      tabs: pieces(doc, ['.tab']),
      'topbar-right': pieces(doc, ['.staged-count', '#staged .btn', '.icon-btn'])
    };
    var names = Object.keys(groups);
    var hits = [];
    var counts = {};
    names.forEach(function (n) { counts[n] = groups[n].length; });
    for (var i = 0; i < names.length; i++) {
      for (var j = i + 1; j < names.length; j++) {
        groups[names[i]].forEach(function (a) {
          groups[names[j]].forEach(function (b) {
            if (overlaps(a, b)) {
              hits.push(names[i] + ' [' + a.label + '] + ' + names[j] + ' [' + b.label + ']');
            }
          });
        });
      }
    }
    results[w] = { innerWidth: win.innerWidth, scrollWidth: doc.documentElement.scrollWidth, overlaps: hits, counts: counts };
  });
  var pre = document.createElement('pre');
  pre.id = 'topbar-check-result';
  pre.textContent = JSON.stringify(results);
  document.body.appendChild(pre);
});
</script>
"#;

/// A minimal base64 encoder (standard alphabet, `=`-padded) -- just enough
/// to turn the two embedded woff2 fonts into `data:` URIs below. No new
/// dependency: this repo has no base64 crate, and pulling one in for a
/// dozen lines of test-only encoding isn't worth it.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The staged count rendered in the test below. "N staged" grows a digit
/// once this crosses into double digits, which is where the string's width
/// actually jumps -- so the test needs a plausibly *wide* count, not a
/// small one. `staged` is `Session::ops().len()` (`src/studio/edit.rs`),
/// which is uncapped and never deduplicated (re-saving the same fragment
/// three times pushes three ops, not one); applying a single starter pack
/// already stages 14-15 ops (`server.rs`'s pack-apply tests), and a heavier
/// single sitting -- a pack apply plus hand-adopting several more palette
/// fragments, a few edits redone, a profile toggle or two -- plausibly
/// reaches well into double digits. 99 is used here: the widest count still
/// inside that plausible double-digit range, without assuming an
/// implausible triple-digit session.
const TOPBAR_TEST_STAGED_COUNT: usize = 99;

/// Renders the studio shell with a wide staged count (see
/// [`TOPBAR_TEST_STAGED_COUNT`]) -- the busiest the topbar ever gets, since
/// that's what puts the "N staged" text plus the Review/Discard/Apply
/// buttons in the right-hand group (the cluster that actually collided with
/// the tabs; see R2 brief) -- inlines the real stylesheet (fonts included;
/// see below), and checks at a spread of widths from 1400px down to 380px
/// that no rendered piece of the brand, tabs, or top-right controls ever
/// overlaps a piece from a different group, and that the page never scrolls
/// horizontally.
///
/// Each width gets its own `<iframe>` (a nested browsing context whose CSS
/// viewport is exactly its own rendered box) rather than relaunching Chrome
/// with `--window-size=<width>`: this build of Chrome silently clamps
/// `--window-size` widths below ~500px up to 500px (verified empirically --
/// a real OS/window-manager minimum, not a bug in this test), which would
/// have made the 480px and 380px cases secretly re-test 500px. The iframe
/// approach sidesteps that floor entirely and needs only one Chrome launch
/// for all twelve widths.
#[test]
#[ignore = "needs Chrome; CI runs it via `cargo test --test browser_smoke -- --ignored`"]
fn topbar_never_overlaps_or_overflows_from_1400px_to_380px() {
    let chrome = chrome().expect("no Chrome found; set CHROME_BIN");

    let shell = loadout::studio::views::shell(maud::html! {}, TOPBAR_TEST_STAGED_COUNT, "profiles");

    // The shell's own `<link rel="stylesheet" href="/assets/studio.css">`
    // 404s under `file://`/`srcdoc` (a rooted path doesn't resolve on disk),
    // so styling would silently not apply without inlining it. The CSS is
    // embedded via rust-embed and reachable through `studio::assets::get`,
    // so no server is needed to fetch it.
    let (css_bytes, _) =
        loadout::studio::assets::get("/assets/studio.css").expect("studio.css must be embedded");
    let mut css = String::from_utf8(css_bytes).unwrap();
    // The two self-hosted fonts (`@font-face { src: url(/assets/fonts/…) }`)
    // would 404 under `file://`/`srcdoc` same as the stylesheet link, and
    // font-display: swap never recovers from a font that never loads -- the
    // page would silently fall back to a system font for good. That matters
    // here: the fallback for "Alfa Slab One" measured ~11px narrower than
    // the real face for the "LOADOUT" wordmark, which is exactly the kind of
    // slack that could hide a real near-miss at the 1300px breakpoint. Inline
    // both as `data:` URIs so the test measures the font production ships.
    for (path, mime) in [
        ("/assets/fonts/inter.woff2", "font/woff2"),
        ("/assets/fonts/alfa-slab-one.woff2", "font/woff2"),
    ] {
        let (bytes, _) =
            loadout::studio::assets::get(path).unwrap_or_else(|| panic!("{path} must be embedded"));
        let data_uri = format!("data:{mime};base64,{}", base64_encode(&bytes));
        css = css.replacen(path, &data_uri, 1);
    }
    let styled = shell.replacen("</head>", &format!("<style>{css}</style></head>"), 1);
    // Escaped for embedding in each iframe's `srcdoc` attribute (the same
    // technique `viewer_selftest_passes_under_opaque_origin_sandbox` uses
    // above). No `sandbox` attribute here -- unlike that test, this harness
    // needs same-origin access to each frame to read its layout.
    let escaped = styled.replace('&', "&amp;").replace('"', "&quot;");

    let mut harness = String::from("<!doctype html>\n<html><body>\n");
    for w in TOPBAR_TEST_WIDTHS {
        harness.push_str(&format!(
            "<iframe id=\"w-{w}\" style=\"display:block;width:{w}px;height:220px;border:0\" scrolling=\"no\" srcdoc=\"{escaped}\"></iframe>\n"
        ));
    }
    let widths_js = TOPBAR_TEST_WIDTHS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    harness.push_str(&TOPBAR_HARNESS_JS.replace("__WIDTHS__", &widths_js));
    harness.push_str("</body></html>\n");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("topbar.html");
    std::fs::write(&path, harness).unwrap();

    let out = Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--virtual-time-budget=5000",
            "--dump-dom",
            // Comfortably above every tested width plus the widest frame, so
            // the *outer* harness page never itself has to shrink anything.
            "--window-size=1600,1200",
        ])
        .arg(format!("file://{}", path.display()))
        .output()
        .expect("chrome runs");
    let dom = String::from_utf8_lossy(&out.stdout);

    let marker = "id=\"topbar-check-result\"";
    let tag_start = dom.find(marker).unwrap_or_else(|| {
        panic!(
            "no topbar-check-result in DOM; tail:\n{}",
            char_boundary_tail(&dom, 2000)
        )
    });
    let body_start = dom[tag_start..]
        .find('>')
        .map(|i| tag_start + i + 1)
        .unwrap_or_else(|| panic!("unterminated result tag"));
    let body_end = dom[body_start..]
        .find("</pre>")
        .map(|i| body_start + i)
        .unwrap_or_else(|| panic!("unterminated result element"));
    let json = &dom[body_start..body_end];
    let v: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|e| panic!("bad result JSON {json:?}: {e}"));

    for w in TOPBAR_TEST_WIDTHS {
        let r = &v[w.to_string()];
        assert!(!r.is_null(), "width {w}px: missing from results {v}");

        // Guards against the overlap check below passing vacuously: an
        // empty group (a renamed selector matching nothing) can never
        // overlap anything. This also covers "no functionality may be
        // hidden" -- every control's piece is still present at every width.
        for (name, expected) in TOPBAR_EXPECTED_PIECE_COUNTS {
            let got = r["counts"][name].as_u64().unwrap_or_else(|| {
                panic!("width {w}px: no piece count reported for group {name:?}")
            });
            assert_eq!(
                got, expected as u64,
                "width {w}px: group {name:?} has {got} rendered pieces, expected {expected} \
                 -- a selector likely stopped matching"
            );
        }

        let overlaps: Vec<&str> = r["overlaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(
            overlaps.is_empty(),
            "width {w}px: overlap between {overlaps:?}"
        );

        let inner = r["innerWidth"].as_u64().unwrap();
        let scroll = r["scrollWidth"].as_u64().unwrap();
        assert!(
            scroll <= inner,
            "width {w}px: horizontal overflow (scrollWidth {scroll} > innerWidth {inner})"
        );
    }
}
