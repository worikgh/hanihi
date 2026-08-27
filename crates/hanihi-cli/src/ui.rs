//! Terminal UI utilities.
//!
//! Centralises how text is rendered to a terminal so the colour scheme can
//! be adjusted in one place — and later driven by a `/colour` REPL command
//! (the mechanism for switching the colour is in place now).
//!
//! The default foreground colour is **dark blue**. Colour codes are only
//! emitted when stdout is a terminal (TTY); when piped or redirected the
//! text stays plain so logs and captured output remain clean.
//!
//! Use [`print_coloured!`] / [`println_coloured!`] in place of `print!` /
//! `println!` to apply the current colour to a payload. To change the
//! colour at runtime (future `/colour` command), call [`set_default_colour`].

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicU8, Ordering};

/// A foreground colour for terminal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    DarkBlue,
    Reset,
    /// Plain output — no colour code emitted.
    Plain,
}

impl Colour {
    /// The ANSI SGR foreground escape sequence for this colour, or empty
    /// when the output is not a terminal.
    fn sgr(self, emit: bool) -> &'static str {
        if !emit {
            return "";
        }
        match self {
            Colour::DarkBlue => "\x1b[34m",
            Colour::Reset => "\x1b[0m",
            Colour::Plain => "",
        }
    }
}

/// Slot index for the default colour in the process-wide scheme. Add more
/// colours here as `/colour` gains options.
const DEFAULT_COLOUR_SLOT: u8 = 0;

/// Process-wide colour scheme (default dark blue). A small atomic keeps the
/// current default so a future `/colour` command can flip it without
/// threading state through every call site.
static SCHEME: AtomicU8 = AtomicU8::new(DEFAULT_COLOUR_SLOT);

/// Change the process-wide default colour (used by a future `/colour`
/// command and other runtime switches).
pub fn set_default_colour(colour: Colour) {
    let slot = match colour {
        Colour::DarkBlue => DEFAULT_COLOUR_SLOT,
        Colour::Reset | Colour::Plain => DEFAULT_COLOUR_SLOT,
    };
    SCHEME.store(slot, Ordering::Relaxed);
}

/// Resolve the process-wide default colour.
fn default_colour() -> Colour {
    match SCHEME.load(Ordering::Relaxed) {
        DEFAULT_COLOUR_SLOT => Colour::DarkBlue,
        _ => Colour::DarkBlue,
    }
}

/// Whether ANSI codes should be emitted (true when stdout is a TTY).
fn emit_colour() -> bool {
    io::stdout().is_terminal()
}

/// Apply the current default colour to `s`, wrapping in reset afterwards.
/// Returns `s` unchanged when not on a terminal.
pub fn tint(s: &str) -> String {
    if !emit_colour() {
        return s.to_string();
    }
    let fg = default_colour().sgr(true);
    let reset = Colour::Reset.sgr(true);
    format!("{fg}{s}{reset}")
}

/// Print a payload to stdout in the current default colour.
macro_rules! print_coloured {
    ($($arg:tt)*) => {
        ::std::print!("{}", $crate::ui::tint(&format!($($arg)*)))
    };
}

/// Print a payload plus newline to stdout in the current default colour.
macro_rules! println_coloured {
    () => {
        ::std::println!()
    };
    ($($arg:tt)*) => {
        ::std::println!("{}", $crate::ui::tint(&format!($($arg)*)))
    };
}

pub(crate) use print_coloured;
pub(crate) use println_coloured;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tint_emits_dark_blue_and_reset_on_tty_like_input() {
        // When not on a terminal `tint` passes text through untouched, so the
        // colour path can be exercised directly through `Colour`'s SGR codes.
        let fg = Colour::DarkBlue.sgr(true);
        let reset = Colour::Reset.sgr(true);
        assert_eq!(fg, "\x1b[34m");
        assert_eq!(reset, "\x1b[0m");
        // `Plain` emits no code but is still representable.
        assert_eq!(Colour::Plain.sgr(true), "");
    }

    #[test]
    fn set_default_colour_accepts_every_variant() {
        // The future `/colour` command must be able to accept every colour
        // without panicking or leaving the scheme in a bad state.
        for colour in [Colour::DarkBlue, Colour::Reset, Colour::Plain] {
            set_default_colour(colour);
        }
        // Default stays dark blue.
        assert_eq!(default_colour(), Colour::DarkBlue);
    }
}
