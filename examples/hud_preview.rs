//! Dev-only preview of the `load run` startup HUD, so the animation can be
//! eyeballed without launching an agent.
//!
//! ```text
//! cargo run --example hud_preview
//! LOADOUT_THEME=light cargo run --example hud_preview
//! LOADOUT_THEME=dark  cargo run --example hud_preview
//! ```
//!
//! The HUD stays off (and this prints nothing) when stdout is not a TTY,
//! `NO_COLOR` is set, or the terminal is narrower than the grid.

use std::thread::sleep;
use std::time::Duration;

use loadout::progress::{EquipHud, Glyph, Phase};

fn main() {
    let hud = EquipHud::start(false);
    // Long enough to see each box cycle its reel, then land.
    let dwell = Duration::from_millis(450);

    let steps = [
        (Phase::Sync, Glyph::Ok, "up to date"),
        (Phase::Loadout, Glyph::Ok, "rust"),
        (Phase::Gear, Glyph::Ok, "hooks · skills"),
        (Phase::Render, Glyph::Ok, "rust → opencode"),
        (Phase::Flow, Glyph::Ok, "Superpowers"),
        (Phase::Launch, Glyph::Go, "opencode"),
    ];

    for (phase, glyph, detail) in steps {
        hud.begin(phase);
        sleep(dwell);
        hud.settle(phase, glyph, Some(detail.to_string()), None);
    }
    hud.finish();
}
