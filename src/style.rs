//! Minimal terminal styling for command output. Colors only when stdout is a
//! real terminal and `NO_COLOR` is unset; otherwise returns plain strings, so
//! piped/redirected output stays clean.

use std::io::IsTerminal;

use anstyle::{Ansi256Color, AnsiColor, Color, RgbColor, Style};

/// Whether ANSI color should be emitted (stdout is a TTY and `NO_COLOR` unset).
pub fn enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Whether the terminal declares 24-bit color (`COLORTERM=truecolor`/`24bit`).
/// Anything else gets the 256-color cube approximation from [`Painter::rgb`].
fn truecolor() -> bool {
    std::env::var_os("COLORTERM").is_some_and(|v| v == "truecolor" || v == "24bit")
}

/// The terminal's background theme, used to pick foreground colors that stay
/// readable. Detection is best-effort and does no terminal I/O: an explicit
/// `LOADOUT_THEME=light|dark` wins, then the `COLORFGBG` hint many terminals
/// (iTerm2, Konsole, rxvt) export, else dark — the historical default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn detect() -> Self {
        if let Some(v) = std::env::var_os("LOADOUT_THEME").and_then(|v| v.into_string().ok()) {
            match v.trim().to_ascii_lowercase().as_str() {
                "light" => return Theme::Light,
                "dark" => return Theme::Dark,
                _ => {}
            }
        }
        if let Some(v) = std::env::var_os("COLORFGBG").and_then(|v| v.into_string().ok()) {
            if let Some(t) = theme_from_colorfgbg(&v) {
                return t;
            }
        }
        Theme::Dark
    }
}

/// Parse the background from a `COLORFGBG` value (`"fg;bg"`, palette indices
/// 0–15). Only white (7) and bright white (15) read as a light background;
/// everything else is treated as dark, which is the common case.
fn theme_from_colorfgbg(v: &str) -> Option<Theme> {
    let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || bg == 15 {
        Theme::Light
    } else {
        Theme::Dark
    })
}

/// Nearest xterm-256 color-cube index for an RGB value (the standard
/// 16 + 6×6×6 mapping; the levels are 0,95,135,175,215,255).
fn cube_index(r: u8, g: u8, b: u8) -> u8 {
    fn level(c: u8) -> u8 {
        match c {
            0..=47 => 0,
            48..=114 => 1,
            c => ((c as u16 - 35) / 40) as u8,
        }
    }
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

/// A painter capturing whether color is on, so call sites read cleanly.
#[derive(Clone, Copy)]
pub struct Painter {
    on: bool,
}

impl Painter {
    /// Auto-detect from the environment (TTY + `NO_COLOR`).
    pub fn auto() -> Self {
        Painter { on: enabled() }
    }

    /// Force on/off (used in tests / non-TTY callers).
    pub fn new(on: bool) -> Self {
        Painter { on }
    }

    fn paint(&self, s: &str, style: Style) -> String {
        if self.on {
            format!("{}{s}{}", style.render(), style.render_reset())
        } else {
            s.to_string()
        }
    }

    fn fg(&self, s: &str, c: AnsiColor) -> String {
        self.paint(s, Style::new().fg_color(Some(c.into())))
    }

    pub fn green(&self, s: &str) -> String {
        self.fg(s, AnsiColor::Green)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.fg(s, AnsiColor::Yellow)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.fg(s, AnsiColor::Cyan)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint(s, Style::new().dimmed())
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint(s, Style::new().bold())
    }

    /// An exact RGB foreground on truecolor terminals, degrading to the
    /// nearest xterm-256 cube color elsewhere (256-color support is effectively
    /// universal among TTYs that pass [`enabled`], macOS Terminal included).
    pub fn rgb(&self, s: &str, (r, g, b): (u8, u8, u8)) -> String {
        let color: Color = if truecolor() {
            RgbColor(r, g, b).into()
        } else {
            Ansi256Color(cube_index(r, g, b)).into()
        };
        self.paint(s, Style::new().fg_color(Some(color)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_when_off_styled_when_on() {
        let off = Painter::new(false);
        assert_eq!(off.green("hi"), "hi"); // no escapes when disabled
        let on = Painter::new(true);
        let s = on.green("hi");
        assert!(s.contains("hi") && s.contains('\u{1b}')); // wrapped in SGR codes
    }

    #[test]
    fn colorfgbg_white_backgrounds_read_as_light() {
        assert_eq!(theme_from_colorfgbg("0;15"), Some(Theme::Light));
        assert_eq!(theme_from_colorfgbg("0;7"), Some(Theme::Light));
        assert_eq!(theme_from_colorfgbg("15;0"), Some(Theme::Dark));
        assert_eq!(theme_from_colorfgbg("0;1"), Some(Theme::Dark));
        assert_eq!(theme_from_colorfgbg("nonsense"), None);
    }
}
