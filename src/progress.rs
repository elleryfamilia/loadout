//! The `load run` startup progress bar — the "equipping" sweep drawn above the
//! step lines. Dungeon-crawl themed: amber ▓ fill with a dim ▒ leading edge on
//! a dark ░ track, `EQUIPPED` when full.
//!
//! ## How it stays above the steps
//!
//! The bar prints first, then every step line is printed *through*
//! [`EquipBar::println`], which keeps a count of lines below the bar. A
//! [`EquipBar::tick`] redraws in place by moving the cursor up that many
//! lines and back (`CSI F` / `CSI E`) — no full-screen control, no flicker.
//! The count is only correct while all output flows through the bar, so:
//!
//! - Any path about to prompt the user (profile chooser, missing-fragment
//!   dialog, skill offer) calls [`EquipBar::abandon`] first: the bar freezes
//!   at its last state and every later call degrades to a plain `println!`.
//!   A frozen half-bar during a rare interactive run is fine; a redraw
//!   overwriting a prompt is not.
//! - A stray `warn_user!` to stderr is not counted and can misalign one
//!   redraw by a line. Accepted: warnings mid-`run` are rare, the damage is
//!   cosmetic, and the alternative (threading the bar into every warning
//!   site) isn't worth it.
//!
//! Disabled entirely (all methods degrade to plain prints) when stdout is not
//! a TTY, `NO_COLOR` is set, `TERM=dumb`, or the run is `--dry-run` — so
//! piped/scripted/test output is byte-identical to before the bar existed.

use std::io::Write;

use crate::style::Painter;

/// Bar width in cells (fits comfortably beside step lines at 80 cols).
const WIDTH: usize = 34;

/// Amber, the dungeon-crawl phosphor. Truecolor when the terminal declares it.
const AMBER: (u8, u8, u8) = (255, 176, 0);
/// The ▒ leading edge — darker amber.
const EMBER: (u8, u8, u8) = (160, 110, 0);
/// The unlit ░ track.
const TRACK: (u8, u8, u8) = (60, 45, 10);

/// The startup progress bar. Construct with [`EquipBar::start`]; drive with
/// [`EquipBar::println`] / [`EquipBar::tick`]; end with [`EquipBar::finish`].
pub struct EquipBar {
    /// `None` → disabled: `println` passes through, everything else no-ops.
    out: Option<Box<dyn Write>>,
    p: Painter,
    total: u8,
    done: u8,
    /// Step lines printed below the bar since it was drawn.
    below: u16,
}

impl EquipBar {
    /// Start the bar on stdout with `total` ticks, drawing it at zero. Enabled
    /// only on an interactive terminal (TTY + no `NO_COLOR` + `TERM != dumb`)
    /// and never on dry runs.
    pub fn start(total: u8, dry_run: bool) -> Self {
        let on = !dry_run
            && crate::style::enabled()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
        let mut bar = Self::with_writer(
            on.then(|| Box::new(std::io::stdout()) as Box<dyn Write>),
            total,
        );
        bar.draw_initial();
        bar
    }

    /// Injectable-writer constructor (tests drive a buffer instead of stdout).
    fn with_writer(out: Option<Box<dyn Write>>, total: u8) -> Self {
        EquipBar {
            out,
            p: Painter::auto(),
            total: total.max(1),
            done: 0,
            below: 0,
        }
    }

    fn draw_initial(&mut self) {
        let line = self.render();
        if let Some(out) = self.out.as_mut() {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// Print one step line below the bar (plain `println!` when disabled).
    pub fn println(&mut self, line: &str) {
        match self.out.as_mut() {
            Some(out) => {
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
                self.below = self.below.saturating_add(1);
            }
            None => println!("{line}"),
        }
    }

    /// Advance one tick and redraw the bar in place.
    pub fn tick(&mut self) {
        self.done = (self.done + 1).min(self.total);
        self.redraw();
    }

    /// Fill to 100% (`EQUIPPED`) whatever the tick count — called right before
    /// the launch line, so the flash is the last frame before the agent owns
    /// the terminal.
    pub fn finish(&mut self) {
        self.done = self.total;
        self.redraw();
    }

    /// Freeze the bar before interactive input: no more redraws, and later
    /// `println`s stop counting (they pass straight through). Idempotent.
    pub fn abandon(&mut self) {
        if let Some(mut out) = self.out.take() {
            let _ = out.flush();
        }
    }

    /// Move up to the bar line, repaint it, move back. `CSI {n}F` = cursor to
    /// the start of the line n above; `CSI {n}E` = to the start of the line n
    /// below — so column state never leaks.
    fn redraw(&mut self) {
        let line = self.render();
        let up = u32::from(self.below) + 1;
        if let Some(out) = self.out.as_mut() {
            let _ = write!(out, "\x1b[{up}F{line}\x1b[K\x1b[{up}E");
            let _ = out.flush();
        }
    }

    /// One bar frame: `  ▓▓▓▓▒▒░░░░  62%` (or `EQUIPPED` when full).
    fn render(&self) -> String {
        let frac = f64::from(self.done) / f64::from(self.total);
        let filled = ((WIDTH as f64) * frac).round() as usize;
        let lead = if filled == 0 || filled == WIDTH {
            0
        } else {
            (WIDTH - filled).min(2)
        };
        let track = WIDTH - filled - lead;
        let bar = format!(
            "{}{}{}",
            self.p.rgb(&"▓".repeat(filled), AMBER),
            self.p.rgb(&"▒".repeat(lead), EMBER),
            self.p.rgb(&"░".repeat(track), TRACK),
        );
        let label = if self.done == self.total {
            self.p.bold(&self.p.rgb("EQUIPPED", AMBER))
        } else {
            self.p
                .rgb(&format!("{:>3}%", (frac * 100.0).round() as u32), AMBER)
        };
        format!("  {bar} {label}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A writer the test can read back after the bar is done with it.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn bar_with_sink(total: u8) -> (EquipBar, Sink) {
        let sink = Sink::default();
        let mut bar = EquipBar::with_writer(Some(Box::new(sink.clone())), total);
        bar.draw_initial();
        (bar, sink)
    }

    fn output(sink: &Sink) -> String {
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn renders_fill_lead_and_track_summing_to_width() {
        let mut bar = EquipBar::with_writer(None, 7);
        bar.done = 4;
        let frame = bar.render();
        let cells = frame.chars().filter(|c| "▓▒░".contains(*c)).count();
        assert_eq!(cells, WIDTH, "fill + lead + track always spans the bar");
        assert!(frame.contains('▓') && frame.contains('▒') && frame.contains('░'));
        assert!(frame.contains("57%"), "4/7 → 57%: {frame}");
    }

    #[test]
    fn full_bar_says_equipped_with_no_lead_or_track() {
        let mut bar = EquipBar::with_writer(None, 7);
        bar.done = 7;
        let frame = bar.render();
        assert!(frame.contains("EQUIPPED"), "{frame}");
        assert_eq!(frame.chars().filter(|c| *c == '▓').count(), WIDTH);
        assert!(!frame.contains('▒') && !frame.contains('░'));
    }

    #[test]
    fn redraw_reaches_up_past_exactly_the_printed_lines() {
        let (mut bar, sink) = bar_with_sink(7);
        bar.println("  ✓ sync   up to date");
        bar.println("  ✓ render  rust → claude");
        bar.tick();
        let out = output(&sink);
        // 2 lines below the bar → the redraw must move up 3 and back down 3.
        assert!(
            out.contains("\x1b[3F"),
            "cursor up to the bar line: {out:?}"
        );
        assert!(out.contains("\x1b[3E"), "cursor restored below: {out:?}");
    }

    #[test]
    fn abandon_freezes_redraws_but_keeps_printing() {
        let (mut bar, sink) = bar_with_sink(7);
        bar.println("  ✓ sync");
        bar.abandon();
        let frozen = output(&sink).len();
        bar.tick();
        bar.finish();
        assert_eq!(
            output(&sink).len(),
            frozen,
            "no bytes may reach the terminal after abandon"
        );
        // println after abandon degrades to plain stdout (not the sink) — the
        // observable contract is simply: the sink sees nothing more.
        bar.println("  ✓ render");
        assert_eq!(output(&sink).len(), frozen);
    }

    #[test]
    fn disabled_bar_never_emits_escapes() {
        let mut bar = EquipBar::with_writer(None, 7);
        bar.tick();
        bar.finish();
        // Nothing to assert on a writer (there is none); the invariant is that
        // these calls are no-ops and println falls through to plain stdout.
        bar.abandon();
    }
}
