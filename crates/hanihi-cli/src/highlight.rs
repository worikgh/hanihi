//! Custom reedline `Highlighter` so typed input at the prompt uses our
//! colour instead of reedline's default grey.

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

/// Highlights the whole input line in a single colour.
pub struct MonoHighlighter {
    style: Style,
}

impl MonoHighlighter {
    /// Build a highlighter that paints everything in `colour`.
    /// Pass e.g. `Color::Green`, `Color::Cyan`, `Color::White`, ...
    pub fn new(colour: Color) -> Self {
        MonoHighlighter {
            style: Style::new().fg(colour),
        }
    }
}

impl Highlighter for MonoHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut ret = StyledText::new();
        ret.push((self.style, line.to_string()));
        ret
    }
}
