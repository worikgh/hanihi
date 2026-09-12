use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use clap::{ArgGroup, Parser};
use hanihi_core::session::log::read_log_tolerant;
use hanihi_core::session::{SessionManager, log::LogEntry};

const DEFAULT_WORKING_DIR: &str = "./working";

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("output")
	.args(["cost", "verbose", "prompts", "messages", ])
	.multiple(false)
))]
#[command(name = "analyse", about = "Inspect hānihi session logs")]
struct Args {
    /// Analyse this session: If no other arguments print kind and timestamp per log entry.
    #[arg(short = 's', long, value_name = "SESSION")]
    session: Option<String>,

    #[arg(long="working-directory", short='d', default_value = DEFAULT_WORKING_DIR)]
    working_dir: String,

    #[arg(long="cost", short='c',  action = clap::ArgAction::SetTrue)]
    cost: bool,

    #[arg(long="verbose", short='v',  action = clap::ArgAction::SetTrue)]
    verbose: bool,

    #[arg(long="prompts", short='p',  action = clap::ArgAction::SetTrue)]
    prompts: bool,

    #[arg(long="messages", short='m',  action = clap::ArgAction::SetTrue)]
    messages: bool,
}

enum Action {
    /// Defrault action, list available sessions
    ListSessions,

    /// If only the session is declared then list the time stamp and
    /// kind of each entry in the session
    SessionBrief(String),

    /// The argument "--cost" or "-c" is supplied, and a session.
    /// Display a three column display: Timestamp, tokens in, tokes out
    CostOfSession(String),

    /// Display verbose session information
    Verbose(String),

    /// Display prompt history (prompts and replies)
    Prompts(String),

    /// Display all the messages sent to the LLM
    Messages(String),
}
impl Args {
    fn action(&self) -> Result<Action, String> {
        if self.session.is_none() {
            // `self.cost`
            if self.cost || self.verbose {
                Err("Must specify a session".to_string())
            } else {
                Ok(Action::ListSessions)
            }
        } else {
            let session = self.session.clone().unwrap();
            if self.cost {
                Ok(Action::CostOfSession(session))
            } else if self.verbose {
                Ok(Action::Verbose(session))
            } else if self.prompts {
                Ok(Action::Prompts(session))
            } else if self.messages {
                Ok(Action::Messages(session))
            } else {
                Ok(Action::SessionBrief(session))
            }
        }
    }
}
fn main() -> ExitCode {
    let args = Args::parse();
    let working_dir = PathBuf::from(DEFAULT_WORKING_DIR);

    // Calculate what the user wants to display.
    match args.action() {
        Ok(Action::SessionBrief(session)) => {
            match analyse_session(&working_dir, session.as_str()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Action::ListSessions) => match list_sessions(&working_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Ok(Action::CostOfSession(session)) => {
            if let Err(error) = analyse_cost(&working_dir, &session) {
                eprintln!("{error}");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Action::Verbose(session)) => {
            if let Err(e) = verbose_session(&working_dir, &session) {
                eprintln!("{e}");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Action::Prompts(session)) => {
            if let Err(e) = prompt_reply(&working_dir, &session) {
                eprintln!("{e}");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Action::Messages(session)) => {
            if let Err(e) = messages(&working_dir, &session) {
                eprintln!("{e}");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
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

/// Helper function to get the session event file
fn session_events(working_dir: &Path, session: &str) -> Result<PathBuf, String> {
    Ok(working_dir
        .join("sessions")
        .join(session)
        .join("events.jsonl"))
}

/// Helper function to get time string
fn get_wait(ts: &DateTime<Utc>, last_send_ts: &Option<DateTime<Utc>>) -> String {
    match &last_send_ts {
        Some(ts_last) => {
            let diff = *ts - ts_last;
            match diff.to_std() {
                Ok(d) => humantime::format_duration(d).to_string(),
                Err(e) => format!("{e}"),
            }
        }
        None => "No time".to_string(),
    }
}

/// Helper function to get events
fn events(working_dir: &Path, session: &str) -> Result<Vec<LogEntry>, String> {
    let path = session_events(working_dir, session)?;
    let logs =
        read_log_tolerant(&path).map_err(|e| format!("error reading {}: {e}", path.display()))?;
    Ok(logs.entries)
}
/// A report on the costs (in tokens) of LLM_Prompts
fn analyse_cost(working_dir: &Path, session: &str) -> Result<(), String> {
    for entry in &events(working_dir, session)? {
        if let LogEntry::LlmResponse { ts, turn: _, data } = entry {
            println!(
                "{}\t{}\t{}",
                ts.to_rfc3339(),
                data.usage.input_tokens,
                data.usage.output_tokens
            );
        }
    }
    Ok(())
}

/// Print `kind<TAB>ts` for every entry in one session's `events.jsonl`.
///
/// Bad lines are reported as warnings on stderr but do not abort the read.
fn analyse_session(working_dir: &Path, session: &str) -> Result<(), String> {
    let path = session_events(working_dir, session)?;
    let logs =
        read_log_tolerant(&path).map_err(|e| format!("error reading {}: {e}", path.display()))?;

    for entry in &events(working_dir, session)? {
        println!("{}\t{}", entry.kind(), entry.ts().to_rfc3339());
    }
    for err in &logs.errors {
        eprintln!(
            "warning: {} line {}: {}",
            path.display(),
            err.line,
            err.message
        );
    }
    Ok(())
}

/// `--messages` `-m` The messages send back to the LLM
fn messages(working_dir: &Path, session: &str) -> Result<(), String> {
    for event in events(working_dir, session)? {
        if let LogEntry::LlmPrompt { ts, turn, data } = event {
            let messages =
                serde_json::to_string_pretty(&data.messages).map_err(|e| format!("{e}"))?;
            println!("Messages: {ts} T({turn}) {} characters", messages.len());
            println!("{messages}");
        }
    }
    Ok(())
}

/// `--prompts` `-p`: The prompts and replies.
fn prompt_reply(working_dir: &Path, session: &str) -> Result<(), String> {
    let mut last_send_ts: Option<DateTime<Utc>> = None;
    for entry in &events(working_dir, session)? {
        match entry {
            LogEntry::UserInput { ts, turn, data } => {
                println!("User Input: {ts} T({turn})");
                println!("{}", data.text);
                println!("---");
                last_send_ts = Some(*ts);
            }
            LogEntry::LlmPrompt { ts, turn, data } => {
                let time = get_wait(ts, &last_send_ts);
                println!("LLM Prompt: {ts} T({turn}) {time} {}", data.model);
            }
            LogEntry::LlmResponse { ts, turn, data } => {
                let time = get_wait(ts, &last_send_ts);
                println!("LLM Response: {ts} T({turn})  {time}",);
                let reasoning = if let Some(reasoning) = &data.reasoning {
                    reasoning
                } else {
                    "No reasoning"
                };
                let text = if let Some(text) = &data.text {
                    text
                } else {
                    "No data"
                };
                println!("Usage: {}", data.usage);
                println!("Reasoning: {reasoning}");
                println![];
                println!("Text: {}", text);
                println!("---");
            }
            LogEntry::ToolExecution { ts, turn, data } => {
                let time = get_wait(ts, &last_send_ts);
                println!("LLM Tool Execution: {ts} T({turn}) {time} {}", data.name);
            }
            _ => (),
        }
    }

    Ok(())
}

/// Print everything from the session
fn verbose_session(working_dir: &Path, session: &str) -> Result<(), String> {
    for entry in &events(working_dir, session)? {
        println!("{}", entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hanihi_core::session::log::LogEntry;

    const USER_INPUT: &str = r#"{"schema":1,"kind":"user_input","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"text":"hi"}}"#;
    const TURN_COMPLETE: &str = r#"{"schema":1,"kind":"turn_complete","ts":"2026-01-01T00:00:01Z","turn":1,"data":{"text":"x","tool_calls":0}}"#;

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
    fn parses_valid_entries_with_tolerant_reader() {
        let log = format!("{USER_INPUT}\n{TURN_COMPLETE}\n");
        let outcome = hanihi_core::session::log::parse_log_tolerant(&log);
        assert_eq!(outcome.entries.len(), 2);
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn reports_bad_lines_as_errors_not_failures() {
        let log = format!("{USER_INPUT}\nnot json\n");
        let outcome = hanihi_core::session::log::parse_log_tolerant(&log);
        assert_eq!(outcome.entries.len(), 1);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].line, 2);
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
