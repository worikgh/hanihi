use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use hanihi_core::session::SessionManager;
use hanihi_core::session::log::LogEntry;

const DEFAULT_WORKING_DIR: &str = "./working";

#[derive(Debug, Parser)]
#[command(name = "analyse", about = "Inspect hānihi session logs")]
struct Args {
    /// Analyse this session: print kind and timestamp per log entry.
    #[arg(long, value_name = "SESSION")]
    session: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let working_dir = PathBuf::from(DEFAULT_WORKING_DIR);

    match args.session {
        Some(name) => match analyse_session(&working_dir, &name) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        None => match list_sessions(&working_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Print names of all sessions on disk, one per line.
fn list_sessions(working_dir: &Path) -> Result<(), String> {
    let names = session_names(working_dir)?;
    if names.is_empty() {
        println!("no sessions");
    } else {
        for name in names {
            println!("{name}");
        }
    }
    Ok(())
}

/// Names of all session directories under `working_dir/sessions`.
fn session_names(working_dir: &Path) -> Result<Vec<String>, String> {
    SessionManager::new(working_dir)
        .list()
        .map_err(|e| format!("error listing sessions: {e}"))
}

/// Print `kind<TAB>ts` for every entry in one session's `events.jsonl`.
fn analyse_session(working_dir: &Path, name: &str) -> Result<(), String> {
    let path = working_dir.join("sessions").join(name).join("events.jsonl");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("error reading {}: {e}", path.display()))?;
    print_entries(&content, &path)
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

        println!("{}\t{}", entry.kind(), entry.ts().to_rfc3339());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_INPUT: &str =
        r#"{"kind":"user_input","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"text":"hi"}}"#;
    const TURN_COMPLETE: &str = r#"{"kind":"turn_complete","ts":"2026-01-01T00:00:01Z","turn":1,"data":{"text":"x","tool_calls":0}}"#;

    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hanihi-analyse-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn cli_defaults_to_listing_sessions() {
        let args = Args::try_parse_from(["analyse"]).expect("parse");
        assert_eq!(args.session, None);
    }

    #[test]
    fn cli_accepts_session_flag() {
        let args = Args::try_parse_from(["analyse", "--session", "foo"]).expect("parse");
        assert_eq!(args.session.as_deref(), Some("foo"));
    }

    #[test]
    fn maps_kind_for_parsed_entry() {
        let entry: LogEntry = serde_json::from_str(USER_INPUT).expect("parse");
        assert_eq!(entry.kind(), "user_input");
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

    #[test]
    fn session_names_lists_existing_dirs_sorted() {
        let dir = tmp_dir("list");
        std::fs::create_dir_all(dir.join("sessions").join("alpha")).expect("create alpha");
        std::fs::create_dir_all(dir.join("sessions").join("beta")).expect("create beta");

        let names = session_names(&dir).expect("list");
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn session_names_empty_without_sessions_dir() {
        let dir = tmp_dir("empty");
        let names = session_names(&dir).expect("list");
        assert!(names.is_empty());
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }
}
