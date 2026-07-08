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
    assert!(
        dom.contains("LOADOUT_SELFTEST_PASS"),
        "selftest failed; DOM tail:\n{}",
        &dom[dom.len().saturating_sub(2000)..]
    );
}
