//! The `load run` startup HUD — a game-style "equipping" panel drawn above the
//! command output. Each startup phase is a box in a 2×3 grid; while its phase
//! runs the box rapidly cycles that phase's real sub-steps (`pulling refs ⋯`,
//! `folding fragments ⋯`), then **lands** on the phase's outcome. All amber, to
//! match the release's dungeon-crawl palette.
//!
//! ## Why a background thread
//!
//! Real startup phases finish in milliseconds, so the cycling can't be driven
//! *by* the work — it would either lie (animate after the fact) or stall. A
//! render thread paints the grid at a steady frame rate while the main thread
//! does the actual startup and only marks transitions:
//!
//! - [`EquipHud::begin`] lights a box (it starts cycling its reel).
//! - [`EquipHud::settle`] gives a box its outcome; the render thread lands it
//!   once the box has cycled for at least [`MIN_DWELL`], so a phase that
//!   completed instantly still shows its reel. Boxes settle in a cascade that
//!   overlaps later work, so the only added latency is the last box's dwell.
//! - [`EquipHud::finish`] waits for the cascade, then flushes buffered notes
//!   and warnings below the settled grid.
//!
//! The render thread is the **sole writer to stdout** for the HUD's life, so
//! nothing corrupts its in-place repaint: notes are buffered ([`EquipHud::note`])
//! and `warn_user!` is captured ([`crate::report::begin_warning_capture`]),
//! both replayed after the grid ends.
//!
//! ## When it's off
//!
//! Disabled — every method degrades to the classic `  ✓ label  detail` step
//! lines, byte-for-byte as before the HUD existed — when stdout is not a TTY,
//! `NO_COLOR` is set, `TERM=dumb`, the run is `--dry-run`, or the terminal is
//! narrower than [`MIN_GRID_COLS`]. An interactive prompt mid-launch
//! ([`EquipHud::abandon`]) tears the grid down and the rest of the run prints
//! classic lines too.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::style::Painter;

/// The amber the fill bar used — cycling text and settled outcomes both use it.
const AMBER: (u8, u8, u8) = (255, 176, 0);
/// A dim amber for a settled-but-muted box (a skipped phase, e.g. no workflow).
const MUTED: (u8, u8, u8) = (150, 110, 40);
/// The unlit color for a box whose phase hasn't started.
const PENDING: (u8, u8, u8) = (78, 74, 66);

/// Inner content width of each box (3 boxes × `BOX_W + 3` border/label cells
/// must fit [`MIN_GRID_COLS`]).
const BOX_W: usize = 22;
/// Below this terminal width the grid would wrap, so the HUD stays off and the
/// classic step lines are used instead.
const MIN_GRID_COLS: u16 = 78;
/// Render cadence.
const FRAME: Duration = Duration::from_millis(45);
/// How long each cycled sub-step word shows before the next.
const WORD_EVERY: Duration = Duration::from_millis(85);
/// Minimum time a box cycles before it may land — so an instant phase still
/// shows its reel. Also the ceiling on the HUD's added latency.
const MIN_DWELL: Duration = Duration::from_millis(300);

/// A startup phase — one box in the grid, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Sync,
    Loadout,
    Gear,
    Render,
    Flow,
    Launch,
}

impl Phase {
    const ALL: [Phase; 6] = [
        Phase::Sync,
        Phase::Loadout,
        Phase::Gear,
        Phase::Render,
        Phase::Flow,
        Phase::Launch,
    ];

    fn index(self) -> usize {
        Phase::ALL.iter().position(|p| *p == self).unwrap()
    }

    /// The box title (uppercased in the grid; lowercased for the classic line).
    fn label(self) -> &'static str {
        match self {
            Phase::Sync => "sync",
            Phase::Loadout => "loadout",
            Phase::Gear => "gear",
            Phase::Render => "render",
            Phase::Flow => "flow",
            Phase::Launch => "launch",
        }
    }

    /// The real sub-steps this phase cycles while it runs.
    fn reel(self) -> &'static [&'static str] {
        match self {
            Phase::Sync => &[
                "reaching remote",
                "pulling refs",
                "fast-forward",
                "verifying",
            ],
            Phase::Loadout => &[
                "reading context",
                "matching targets",
                "selecting",
                "composing",
            ],
            Phase::Gear => &["checking hooks", "linking skills", "wiring agents"],
            Phase::Render => &["folding fragments", "writing overlay", "hashing"],
            Phase::Flow => &["resolving workflow", "mapping stages", "arming commands"],
            Phase::Launch => &["locating binary", "arming exec", "handing off"],
        }
    }
}

/// The glyph a settled grid box shows before its detail. Every glyph renders
/// amber in the grid; the choice is purely which mark reads right for the
/// outcome (the classic HUD-off line carries its own glyph, built by the caller).
#[derive(Debug, Clone, Copy)]
pub enum Glyph {
    /// `✓` — success.
    Ok,
    /// `⚠` — a warning outcome.
    Warn,
    /// `▸` — the launch handoff.
    Go,
}

impl Glyph {
    fn ch(self) -> &'static str {
        match self {
            Glyph::Ok => "✓",
            Glyph::Warn => "⚠",
            Glyph::Go => "▸",
        }
    }
}

/// A box's landed outcome. `detail == None` is a muted/skipped box (shown dim
/// in the grid).
#[derive(Debug, Clone)]
struct Settled {
    glyph: Glyph,
    detail: Option<String>,
}

/// One box's animation state.
enum BoxPhase {
    Pending,
    Active {
        since: Instant,
        settle: Option<Settled>,
    },
    Settled(Settled),
}

impl BoxPhase {
    fn is_settled(&self) -> bool {
        matches!(self, BoxPhase::Settled(_))
    }
}

/// Shared state the render thread reads and the main thread mutates.
struct HudState {
    boxes: [BoxPhase; 6],
    abandoned: bool,
}

impl HudState {
    fn new() -> Self {
        HudState {
            boxes: std::array::from_fn(|_| BoxPhase::Pending),
            abandoned: false,
        }
    }
}

/// The live-HUD half: shared state, the render thread handle, and the
/// note/warning buffers replayed when the grid ends.
struct Live {
    state: Arc<Mutex<HudState>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    alive: AtomicBool,
    notes: Mutex<Vec<String>>,
}

/// The startup HUD. Construct with [`EquipHud::start`]; drive with
/// [`EquipHud::begin`] / [`EquipHud::settle`] / [`EquipHud::note`]; end with
/// [`EquipHud::finish`] (or [`EquipHud::abandon`] before an interactive prompt).
pub struct EquipHud {
    /// `Some` when the animated grid is running; `None` on the classic path.
    live: Option<Live>,
    /// Where classic-path lines go (stdout in production; a sink in tests).
    classic_out: Mutex<Box<dyn Write + Send>>,
}

impl EquipHud {
    /// Start the HUD. The animated grid runs only on an interactive terminal
    /// (TTY + color + `TERM != dumb`) wide enough for the grid, and never on a
    /// dry run; otherwise every call degrades to the classic step lines.
    pub fn start(dry_run: bool) -> Self {
        let enabled = !dry_run
            && crate::style::enabled()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
            && term_cols().is_some_and(|c| c >= MIN_GRID_COLS);
        Self::build(enabled, Box::new(std::io::stdout()))
    }

    /// Construction seam: `enabled` forced, classic output injectable (tests).
    fn build(enabled: bool, classic_out: Box<dyn Write + Send>) -> Self {
        let live = enabled.then(|| {
            let state = Arc::new(Mutex::new(HudState::new()));
            crate::report::begin_warning_capture();
            let handle = spawn_render_thread(Arc::clone(&state), Painter::new(true));
            Live {
                state,
                handle: Mutex::new(Some(handle)),
                alive: AtomicBool::new(true),
                notes: Mutex::new(Vec::new()),
            }
        });
        EquipHud {
            live,
            classic_out: Mutex::new(classic_out),
        }
    }

    /// Whether the animated grid is currently driving the terminal.
    fn animating(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|l| l.alive.load(Ordering::Acquire))
    }

    /// Light a phase's box (begins cycling its reel). No-op off the grid.
    pub fn begin(&self, phase: Phase) {
        if !self.animating() {
            return;
        }
        let live = self.live.as_ref().unwrap();
        let mut st = live.state.lock().unwrap();
        if let BoxPhase::Pending = st.boxes[phase.index()] {
            st.boxes[phase.index()] = BoxPhase::Active {
                since: Instant::now(),
                settle: None,
            };
        }
    }

    /// Land a phase's box on its outcome. `grid_detail == None` renders a
    /// muted/skipped box. On the classic (HUD-off) path this prints
    /// `classic_line` verbatim, if given — so the caller keeps full control of
    /// the exact step-line formatting and the grid gets a compact plain detail.
    pub fn settle(
        &self,
        phase: Phase,
        glyph: Glyph,
        grid_detail: Option<String>,
        classic_line: Option<String>,
    ) {
        if !self.animating() {
            if let Some(line) = classic_line {
                self.write_classic(&line);
            }
            return;
        }
        let outcome = Settled {
            glyph,
            detail: grid_detail,
        };
        let live = self.live.as_ref().unwrap();
        let mut st = live.state.lock().unwrap();
        let slot = &mut st.boxes[phase.index()];
        match slot {
            // Never begun (e.g. a skipped phase): light it now so it still
            // shows a brief reel before landing.
            BoxPhase::Pending => {
                *slot = BoxPhase::Active {
                    since: Instant::now(),
                    settle: Some(outcome),
                };
            }
            BoxPhase::Active { settle, .. } => *settle = Some(outcome),
            BoxPhase::Settled(_) => {}
        }
    }

    /// A line to show after the grid settles (hook/skill notes, the update
    /// nudge). Off the grid it prints immediately, preserving call order.
    pub fn note(&self, line: String) {
        if self.animating() {
            self.live.as_ref().unwrap().notes.lock().unwrap().push(line);
        } else {
            self.write_classic(&line);
        }
    }

    /// Freeze the grid before an interactive prompt: stop the render thread,
    /// leave its last frame, and route the rest of the run to classic lines.
    /// Idempotent.
    pub fn abandon(&self) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if !live.alive.swap(false, Ordering::AcqRel) {
            return; // already torn down
        }
        live.state.lock().unwrap().abandoned = true;
        self.join_thread(live);
        // Buffered notes and captured warnings belong on screen now, below the
        // frozen grid; the prompt follows.
        self.flush_buffered(live);
    }

    /// Settle the cascade, join the render thread, then flush buffered notes and
    /// captured warnings below the grid. No-op off the grid.
    pub fn finish(&self) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if !live.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        // Any box still Pending/Active without an outcome would keep the render
        // thread alive forever — land them muted so the cascade completes.
        {
            let mut st = live.state.lock().unwrap();
            for b in &mut st.boxes {
                // A `None` detail renders muted regardless of glyph.
                let muted = || Settled {
                    glyph: Glyph::Ok,
                    detail: None,
                };
                match b {
                    BoxPhase::Pending => *b = BoxPhase::Settled(muted()),
                    BoxPhase::Active { settle, .. } if settle.is_none() => *settle = Some(muted()),
                    _ => {}
                }
            }
        }
        self.join_thread(live);
        self.flush_buffered(live);
    }

    fn join_thread(&self, live: &Live) {
        if let Some(handle) = live.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    /// Print the buffered notes (below the settled/frozen grid) and replay the
    /// warnings captured while the HUD owned the terminal. Idempotent: the note
    /// buffer is drained and the capture is ended, so a later call is a no-op.
    fn flush_buffered(&self, live: &Live) {
        let notes = std::mem::take(&mut *live.notes.lock().unwrap());
        for line in notes {
            self.write_classic(&line);
        }
        for w in crate::report::end_warning_capture() {
            eprintln!("{w}");
        }
    }

    fn write_classic(&self, line: &str) {
        let mut out = self.classic_out.lock().unwrap();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// Best-effort terminal width in columns via `TIOCGWINSZ`. `None` off a TTY or
/// on any platform without the ioctl → the caller treats it as "too narrow"
/// and keeps the HUD off.
#[cfg(unix)]
fn term_cols() -> Option<u16> {
    use std::os::unix::io::AsRawFd as _;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: `ws` is a valid, correctly-sized winsize; ioctl only writes it.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col)
}

#[cfg(not(unix))]
fn term_cols() -> Option<u16> {
    None
}

// --- render thread ------------------------------------------------------------

/// Spawn the painter: repaint the 6-line grid in place each [`FRAME`], landing
/// boxes whose dwell has elapsed, until every box is settled or the HUD is
/// abandoned. Always leaves the cursor below the grid.
fn spawn_render_thread(state: Arc<Mutex<HudState>>, p: Painter) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut first = true;
        loop {
            let (lines, done, abandoned) = {
                let mut st = state.lock().unwrap();
                land_due_boxes(&mut st);
                let lines = render_grid(&st, &p);
                let done = st.boxes.iter().all(BoxPhase::is_settled);
                (lines, done, st.abandoned)
            };
            paint(&lines, first);
            first = false;
            if done || abandoned {
                break;
            }
            std::thread::sleep(FRAME);
        }
    })
}

/// Flip any Active box whose outcome is known and whose dwell has elapsed to
/// Settled. Runs under the state lock each frame.
fn land_due_boxes(st: &mut HudState) {
    let now = Instant::now();
    for b in &mut st.boxes {
        if let BoxPhase::Active {
            since,
            settle: Some(_),
        } = b
        {
            if now.duration_since(*since) >= MIN_DWELL {
                let BoxPhase::Active { settle, .. } = std::mem::replace(b, BoxPhase::Pending)
                else {
                    unreachable!()
                };
                *b = BoxPhase::Settled(settle.unwrap());
            }
        }
    }
}

/// Build the 6 grid lines (2 rows × 3 boxes, 3 lines each) from the state.
fn render_grid(st: &HudState, p: &Painter) -> Vec<String> {
    let now = Instant::now();
    let cells: Vec<[String; 3]> = Phase::ALL
        .iter()
        .enumerate()
        .map(|(i, phase)| render_box(*phase, &st.boxes[i], now, p))
        .collect();
    let mut lines = Vec::with_capacity(6);
    for row in cells.chunks(3) {
        for band in 0..3 {
            let mut line = String::from("  ");
            for cell in row {
                line.push_str(&cell[band]);
            }
            lines.push(line);
        }
    }
    lines
}

/// One box → its 3 lines (top border w/ label, content, bottom border).
fn render_box(phase: Phase, state: &BoxPhase, now: Instant, p: &Painter) -> [String; 3] {
    let (color, label_bold, content) = match state {
        BoxPhase::Pending => (PENDING, false, p.rgb(&center("·", BOX_W), PENDING)),
        BoxPhase::Active { since, .. } => {
            let reel = phase.reel();
            let idx = (now.duration_since(*since).as_millis() / WORD_EVERY.as_millis()) as usize
                % reel.len();
            let word = format!("{} ⋯", reel[idx]);
            (AMBER, true, p.rgb(&pad(&word, BOX_W), AMBER))
        }
        BoxPhase::Settled(s) => match &s.detail {
            Some(d) => {
                let text = format!("{} {d}", s.glyph.ch());
                (AMBER, true, p.rgb(&pad(&text, BOX_W), AMBER))
            }
            None => (MUTED, false, p.rgb(&pad("· —", BOX_W), MUTED)),
        },
    };

    let title = {
        let up = phase.label().to_uppercase();
        let painted = p.rgb(&up, color);
        if label_bold {
            p.bold(&painted)
        } else {
            painted
        }
    };
    let dashes = BOX_W.saturating_sub(char_len(phase.label()));
    let bar = |s: &str| p.rgb(s, color);
    [
        format!(
            "{}{title}{}",
            bar("┌─"),
            bar(&format!("{}┐", "─".repeat(dashes)))
        ),
        format!("{}{content}{}", bar("│"), bar("│")),
        bar(&format!("└{}┘", "─".repeat(BOX_W + 1))),
    ]
}

/// Visible length in `char`s (a good-enough proxy for column width here; the
/// glyphs used are single-width).
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Left-justify `s` to `w` display cells, truncating by `char`s if longer.
fn pad(s: &str, w: usize) -> String {
    let n = char_len(s);
    if n >= w {
        s.chars().take(w).collect()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// Center `s` in `w` cells (used for the pending placeholder).
fn center(s: &str, w: usize) -> String {
    let n = char_len(s);
    if n >= w {
        return s.chars().take(w).collect();
    }
    let left = (w - n) / 2;
    format!("{}{s}{}", " ".repeat(left), " ".repeat(w - n - left))
}

/// Paint the grid in place: on all but the first frame, move up to the top of
/// the block first. Each line is cleared to end-of-line and terminated with a
/// newline, so the cursor always lands just below the grid.
fn paint(lines: &[String], first: bool) {
    let mut out = std::io::stdout().lock();
    let mut buf = String::new();
    if !first {
        buf.push_str(&format!("\x1b[{}F", lines.len()));
    }
    for line in lines {
        buf.push_str(line);
        buf.push_str("\x1b[K\n");
    }
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A writer tests can read back.
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
    fn read(s: &Sink) -> String {
        String::from_utf8(s.0.lock().unwrap().clone()).unwrap()
    }

    fn disabled_hud(sink: Sink) -> EquipHud {
        EquipHud::build(false, Box::new(sink))
    }

    #[test]
    fn disabled_settle_prints_the_callers_classic_line_verbatim() {
        let sink = Sink::default();
        let hud = disabled_hud(sink.clone());
        hud.settle(
            Phase::Render,
            Glyph::Ok,
            Some("rust → claude".into()),
            Some("  ✓ render  rust → claude · f05e86".into()),
        );
        assert_eq!(read(&sink), "  ✓ render  rust → claude · f05e86\n");
    }

    #[test]
    fn disabled_settle_with_no_classic_line_prints_nothing() {
        let sink = Sink::default();
        let hud = disabled_hud(sink.clone());
        // loadout/gear pass classic_line = None (they were silent ticks before).
        hud.settle(Phase::Loadout, Glyph::Ok, Some("rust".into()), None);
        hud.settle(Phase::Gear, Glyph::Ok, Some("hooks ok".into()), None);
        // A skipped flow also passes None.
        hud.settle(Phase::Flow, Glyph::Ok, None, None);
        assert_eq!(read(&sink), "", "no classic line → nothing printed");
    }

    #[test]
    fn disabled_note_prints_immediately_in_order() {
        let sink = Sink::default();
        let hud = disabled_hud(sink.clone());
        hud.settle(
            Phase::Sync,
            Glyph::Ok,
            Some("up to date".into()),
            Some("  ✓ sync    up to date".into()),
        );
        hud.note("  ↑ update  a newer loadout is available".into());
        let out = read(&sink);
        let sync_at = out.find("up to date").unwrap();
        let note_at = out.find("newer loadout").unwrap();
        assert!(sync_at < note_at, "note follows the sync line: {out:?}");
    }

    #[test]
    fn a_pending_box_renders_dim_and_settled_lands_the_outcome() {
        let p = Painter::new(false); // plain: assert on text, not escapes
        let now = Instant::now();
        let pending = render_box(Phase::Render, &BoxPhase::Pending, now, &p);
        assert!(pending[0].contains("RENDER"), "title: {pending:?}");
        assert!(pending[1].contains('·'), "pending placeholder: {pending:?}");

        let settled = render_box(
            Phase::Render,
            &BoxPhase::Settled(Settled {
                glyph: Glyph::Ok,
                detail: Some("rust → claude".into()),
            }),
            now,
            &p,
        );
        assert!(
            settled[1].contains("✓ rust → claude"),
            "outcome: {settled:?}"
        );
    }

    #[test]
    fn active_box_cycles_real_sub_steps_by_elapsed_time() {
        let p = Painter::new(false);
        let start = Instant::now() - WORD_EVERY * 2 - Duration::from_millis(5);
        let active = render_box(
            Phase::Sync,
            &BoxPhase::Active {
                since: start,
                settle: None,
            },
            Instant::now(),
            &p,
        );
        // ~2 words elapsed → the 3rd reel entry ("fast-forward"), + the cycle mark.
        assert!(
            active[1].contains("fast-forward"),
            "cycled word: {active:?}"
        );
        assert!(active[1].contains('⋯'), "cycle marker: {active:?}");
    }

    #[test]
    fn a_grid_is_six_lines_two_rows_of_three_boxes() {
        let st = HudState::new();
        let lines = render_grid(&st, &Painter::new(false));
        assert_eq!(lines.len(), 6, "2 rows × 3 box-lines");
        // Row 1 top-borders carry the first three labels.
        assert!(
            lines[0].contains("SYNC") && lines[0].contains("LOADOUT") && lines[0].contains("GEAR")
        );
        assert!(
            lines[3].contains("RENDER") && lines[3].contains("FLOW") && lines[3].contains("LAUNCH")
        );
    }

    #[test]
    fn land_due_boxes_respects_the_minimum_dwell() {
        let mut st = HudState::new();
        // Just-begun box with a known outcome: too young to land.
        st.boxes[0] = BoxPhase::Active {
            since: Instant::now(),
            settle: Some(Settled {
                glyph: Glyph::Ok,
                detail: Some("x".into()),
            }),
        };
        land_due_boxes(&mut st);
        assert!(!st.boxes[0].is_settled(), "must dwell before landing");

        // Older than the dwell: lands.
        st.boxes[0] = BoxPhase::Active {
            since: Instant::now() - MIN_DWELL - Duration::from_millis(10),
            settle: Some(Settled {
                glyph: Glyph::Ok,
                detail: Some("x".into()),
            }),
        };
        land_due_boxes(&mut st);
        assert!(st.boxes[0].is_settled(), "dwell elapsed → landed");
    }

    #[test]
    fn paint_moves_up_exactly_the_grid_height_after_the_first_frame() {
        // Not a stdout test; assert the escape math via a tiny local echo.
        let lines = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // First frame: no cursor-up.
        let first = build_paint(&lines, true);
        assert!(!first.contains("\x1b[3F"));
        // Later frame: up 3 (the height), each line cleared.
        let later = build_paint(&lines, false);
        assert!(later.contains("\x1b[3F"), "up by height: {later:?}");
        assert_eq!(later.matches("\x1b[K").count(), 3, "each line cleared");
    }

    /// Mirror of [`paint`]'s buffer construction, for the escape-math test.
    fn build_paint(lines: &[String], first: bool) -> String {
        let mut buf = String::new();
        if !first {
            buf.push_str(&format!("\x1b[{}F", lines.len()));
        }
        for line in lines {
            buf.push_str(line);
            buf.push_str("\x1b[K\n");
        }
        buf
    }
}
