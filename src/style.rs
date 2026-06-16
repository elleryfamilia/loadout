//! Minimal terminal styling for command output. Colors only when stdout is a
//! real terminal and `NO_COLOR` is unset; otherwise returns plain strings, so
//! piped/redirected output stays clean.

use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};

/// Whether ANSI color should be emitted (stdout is a TTY and `NO_COLOR` unset).
pub fn enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
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
}
