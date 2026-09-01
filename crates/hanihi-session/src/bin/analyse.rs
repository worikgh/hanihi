use std::path::{Path, PathBuf};

use hanihi_core::session::log::LogEntry;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session_name = match args.as_slice() {
        [name] => name,
        _ => {
            eprintln!("usage: analyse SESSION_NAME");
            std::process::exit(2);
        }
    };

    let path = PathBuf::from("./working")
        .join("sessions")
        .join(session_name)
        .join("events.jsonl");

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("error reading {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    if let Err(e) = print_entries(&content, &path) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Parse each JSONL line as a [`LogEntry`] and print `kind<TAB>ts`.
fn print_entries(content: &str, path: &Path) -> Result<(), String> {
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: LogEntry = serde_json::from_str(line)
            .map_err(|e| format!("error parsing {} line {}: {e}", path.display(), i + 1))?;

        println!("{}\t{}", kind_str(&entry), entry.ts().to_rfc3339());
    }
    Ok(())
}

/// Serialized tag of a [`LogEntry`], matching the `kind` field in the log.
fn kind_str(entry: &LogEntry) -> &'static str {
    match entry {
        LogEntry::SessionCreated { .. } => "session_created",
        LogEntry::SessionOpened { .. } => "session_opened",
        LogEntry::SessionClosed { .. } => "session_closed",
        LogEntry::UserInput { .. } => "user_input",
        LogEntry::LlmPrompt { .. } => "llm_prompt",
        LogEntry::LlmResponse { .. } => "llm_response",
        LogEntry::ToolExecution { .. } => "tool_execution",
        LogEntry::TurnComplete { .. } => "turn_complete",
        LogEntry::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_INPUT: &str =
        r#"{"kind":"user_input","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"text":"hi"}}"#;
    const TURN_COMPLETE: &str = r#"{"kind":"turn_complete","ts":"2026-01-01T00:00:01Z","turn":1,"data":{"text":"x","tool_calls":0}}"#;

    #[test]
    fn maps_kind_for_parsed_entry() {
        let entry: LogEntry = serde_json::from_str(USER_INPUT).expect("parse");
        assert_eq!(kind_str(&entry), "user_input");
        assert_eq!(entry.ts().to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn skips_blank_lines_and_parses_valid_entries() {
        let content = format!("\n{USER_INPUT}\n\n{TURN_COMPLETE}\n");
        assert!(print_entries(&content, Path::new("events.jsonl")).is_ok());
    }

    #[test]
    fn reports_line_number_for_bad_json() {
        let content = format!("{USER_INPUT}\nnot json\n");
        let err = print_entries(&content, Path::new("events.jsonl")).expect_err("bad line");
        assert!(err.contains("line 2"), "unexpected: {err}");
    }
}
