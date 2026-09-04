//! Best-effort file logging for `dbg!`-style instrumentation.
//!
//! The agent runs with stdout/stderr owned by the terminal/REPL, so debug
//! markers from library code are easier to inspect when appended to a file.
//! The target path comes from `HANIHI_DEBUG_FILE`, defaulting to
//! `./hanihi-debug.log` resolved against the process cwd. All I/O failures are
//! swallowed: a debug log must never break tool execution.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::Utc;

/// Environment variable that selects the debug-log target file.
pub const HANIHI_DEBUG_ENV: &str = "HANIHI_DEBUG_FILE";
/// Default target when the environment variable is unset.
const HANIHI_DEBUG_DEFAULT: &str = "./hanihi-debug.log";

/// Append one timestamped debug line for `tag` + `value` to the log file.
///
/// Never panics. If the file cannot be opened or written the event is
/// dropped.
pub fn log_to_file(tag: &str, value: impl core::fmt::Display) {
    let path = std::env::var(HANIHI_DEBUG_ENV).unwrap_or_else(|_| HANIHI_DEBUG_DEFAULT.to_string());
    append_line(Path::new(&path), tag, &value);
}

/// Open `path` (creating it) and append the rendered line. Errors are dropped.
fn append_line(path: &Path, tag: &str, value: &dyn core::fmt::Display) {
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let line = format!("[{}] {tag}: {value}\n", Utc::now().to_rfc3339());
    let _ = writeln!(file, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hanihi-debug-{}.log", uuid::Uuid::new_v4()))
    }

    #[test]
    fn append_creates_file_and_stores_tagged_lines() {
        let path = temp_log();
        let _ = std::fs::remove_file(&path);

        append_line(&path, "tag-one", &"hello");
        append_line(&path, "tag-two", &42);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tag-one: hello"), "got: {content}");
        assert!(content.contains("tag-two: 42"), "got: {content}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn log_to_file_honours_env_target() {
        // SAFETY: single-threaded test binary here; no concurrent env reads.
        let path = temp_log();
        unsafe { std::env::set_var(HANIHI_DEBUG_ENV, &path) };

        log_to_file("env-test", "routed via env");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("env-test: routed via env"),
            "got: {content}"
        );
        unsafe { std::env::remove_var(HANIHI_DEBUG_ENV) };
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn log_to_file_is_best_effort_on_bad_target() {
        // A directory cannot be appended to; the call must not panic.
        let path = std::env::temp_dir();
        unsafe { std::env::set_var(HANIHI_DEBUG_ENV, &path) };
        log_to_file("panic-check", "value");
        unsafe { std::env::remove_var(HANIHI_DEBUG_ENV) };
    }
}
