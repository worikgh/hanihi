//! Terminal UI utilities.
//!
//! Centralises how text is rendered to a terminal so the colour scheme can
//! be adjusted in one place — and later driven by a `/colour` REPL command.
//!
//! The default foreground colour is **dark blue**. Colour codes are only
//! emitted when stdout is a terminal (TTY); when piped or redirected the
//! text stays plain so logs and captured output remain clean.
//!
//! Use [`print_coloured!`] / [`println_coloured!`] in place of `print!` /
//! `println!` to apply the colour to a payload.

use std::io::{self, IsTerminal};

/// Top-level foregreound ANSI SGR escape sequence, or empty when the output
/// is not a terminal.
fn fg() -> &'static str {
    if io::stdout().is_terminal() {
        "\x1b[34m" // dark blue
    } else {
        ""
    }
}

/// Reset ANSI SGR escape sequence, or empty when the output is not a
/// terminal.
fn reset() -> &'static str {
    if io::stdout().is_terminal() {
        "\x1b[0m"
    } else {
        ""
    }
}

/// Apply the dark-blue foreground colour to `s`, wrapping in reset
/// afterwards. Returns `s` unchanged when not on a terminal.
pub fn tint(s: &str) -> String {
    let fg = fg();
    let reset = reset();
    if fg.is_empty() {
        s.to_string()
    } else {
        format!("{fg}{s}{reset}")
    }
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
    fn tint_passes_through_when_not_a_terminal() {
        // When stdout isn't a TTY the tint is identity (no ANSI codes).
        assert_eq!(tint("hello"), {
            if io::stdout().is_terminal() {
                "\x1b[34mhello\x1b[0m".to_string()
            } else {
                "hello".to_string()
            }
        });
    }

    #[test]
    fn fg_and_reset_are_consistent() {
        // Either both emit codes or neither does.
        assert_eq!(fg().is_empty(), reset().is_empty());
    }
}
