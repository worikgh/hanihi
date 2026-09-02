//! Append-only JSONL event log for session persistence.
//!
//! [`LogWriter`] appends one line of JSON per event to an `events.jsonl` file.
//! Each line is a complete, independently parseable JSON object.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// One entry in the session event log.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum LogEntry {
    /// Session directory first created.
    #[serde(rename = "session_created")]
    SessionCreated {
        ts: DateTime<Utc>,
        turn: u64,
        data: SessionCreatedData,
    },
    /// Session opened (including first open after creation).
    #[serde(rename = "session_opened")]
    SessionOpened {
        ts: DateTime<Utc>,
        turn: u64,
        data: SessionOpenedData,
    },
    /// Session closed cleanly.
    #[serde(rename = "session_closed")]
    SessionClosed {
        ts: DateTime<Utc>,
        turn: u64,
        data: SessionClosedData,
    },
    /// User input at the start of a turn.
    #[serde(rename = "user_input")]
    UserInput {
        ts: DateTime<Utc>,
        turn: u64,
        data: UserInputData,
    },
    /// Prompt sent to the LLM.
    #[serde(rename = "llm_prompt")]
    LlmPrompt {
        ts: DateTime<Utc>,
        turn: u64,
        data: LlmPromptData,
    },
    /// Response received from the LLM.
    #[serde(rename = "llm_response")]
    LlmResponse {
        ts: DateTime<Utc>,
        turn: u64,
        data: LlmResponseData,
    },
    /// A tool was executed.
    #[serde(rename = "tool_execution")]
    ToolExecution {
        ts: DateTime<Utc>,
        turn: u64,
        data: ToolExecutionData,
    },
    /// A turn completed successfully.
    #[serde(rename = "turn_complete")]
    TurnComplete {
        ts: DateTime<Utc>,
        turn: u64,
        data: TurnCompleteData,
    },
    /// An error occurred during a turn.
    #[serde(rename = "error")]
    Error {
        ts: DateTime<Utc>,
        turn: u64,
        data: ErrorData,
    },
}

// --- Data structs for each variant ---

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionCreatedData {
    pub session_id: String,
    pub name: String,
    pub model: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionOpenedData {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionClosedData {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct UserInputData {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LlmPromptData {
    pub provider: String,
    pub model: String,
    pub messages: serde_json::Value,
    pub tool_definitions: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LlmResponseData {
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallData>>,
    pub usage: UsageData,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ToolCallData {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct UsageData {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ToolExecutionData {
    pub tool_call_id: String,
    #[serde(default)]
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TurnCompleteData {
    pub text: String,
    pub tool_calls: usize,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ErrorData {
    pub stage: ErrorStage,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorStage {
    LlmCall,
    ToolExecution,
}

// --- Helpers for constructing entries ---

impl LogEntry {
    pub fn session_created(
        ts: DateTime<Utc>,
        turn: u64,
        session_id: String,
        name: String,
        model: String,
        system_prompt: String,
    ) -> Self {
        LogEntry::SessionCreated {
            ts,
            turn,
            data: SessionCreatedData {
                session_id,
                name,
                model,
                system_prompt,
            },
        }
    }

    pub fn session_opened(ts: DateTime<Utc>, turn: u64, session_id: String, name: String) -> Self {
        LogEntry::SessionOpened {
            ts,
            turn,
            data: SessionOpenedData { session_id, name },
        }
    }

    pub fn session_closed(ts: DateTime<Utc>, turn: u64, session_id: String, name: String) -> Self {
        LogEntry::SessionClosed {
            ts,
            turn,
            data: SessionClosedData { session_id, name },
        }
    }

    pub fn user_input(ts: DateTime<Utc>, turn: u64, text: String) -> Self {
        LogEntry::UserInput {
            ts,
            turn,
            data: UserInputData { text },
        }
    }

    pub fn llm_prompt(
        ts: DateTime<Utc>,
        turn: u64,
        provider: String,
        model: String,
        messages: serde_json::Value,
        tool_definitions: serde_json::Value,
    ) -> Self {
        LogEntry::LlmPrompt {
            ts,
            turn,
            data: LlmPromptData {
                provider,
                model,
                messages,
                tool_definitions,
            },
        }
    }

    pub fn llm_response(
        ts: DateTime<Utc>,
        turn: u64,
        message_id: Option<String>,
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ToolCallData>>,
        usage: UsageData,
    ) -> Self {
        LogEntry::LlmResponse {
            ts,
            turn,
            data: LlmResponseData {
                message_id,
                text,
                reasoning,
                tool_calls,
                usage,
            },
        }
    }

    pub fn tool_execution(
        ts: DateTime<Utc>,
        turn: u64,
        tool_call_id: String,
        call_id: String,
        name: String,
        arguments: serde_json::Value,
        result: String,
    ) -> Self {
        LogEntry::ToolExecution {
            ts,
            turn,
            data: ToolExecutionData {
                tool_call_id,
                call_id,
                name,
                arguments,
                result,
            },
        }
    }

    pub fn turn_complete(ts: DateTime<Utc>, turn: u64, text: String, tool_calls: usize) -> Self {
        LogEntry::TurnComplete {
            ts,
            turn,
            data: TurnCompleteData { text, tool_calls },
        }
    }

    pub fn error(ts: DateTime<Utc>, turn: u64, stage: ErrorStage, message: String) -> Self {
        LogEntry::Error {
            ts,
            turn,
            data: ErrorData { stage, message },
        }
    }

    /// Wire-format `kind` tag, matching the enum's serde `rename`.
    pub fn kind(&self) -> &'static str {
        match self {
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

    /// Timestamp of this entry.
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            LogEntry::SessionCreated { ts, .. }
            | LogEntry::SessionOpened { ts, .. }
            | LogEntry::SessionClosed { ts, .. }
            | LogEntry::UserInput { ts, .. }
            | LogEntry::LlmPrompt { ts, .. }
            | LogEntry::LlmResponse { ts, .. }
            | LogEntry::ToolExecution { ts, .. }
            | LogEntry::TurnComplete { ts, .. }
            | LogEntry::Error { ts, .. } => *ts,
        }
    }

    /// Turn number of this entry.
    pub fn turn(&self) -> u64 {
        match self {
            LogEntry::SessionCreated { turn, .. }
            | LogEntry::SessionOpened { turn, .. }
            | LogEntry::SessionClosed { turn, .. }
            | LogEntry::UserInput { turn, .. }
            | LogEntry::LlmPrompt { turn, .. }
            | LogEntry::LlmResponse { turn, .. }
            | LogEntry::ToolExecution { turn, .. }
            | LogEntry::TurnComplete { turn, .. }
            | LogEntry::Error { turn, .. } => *turn,
        }
    }
}

// --- LogWriter ---

/// Append-only JSONL writer for session event logs.
///
/// Flushes after every write so events are durable on disk immediately.
#[derive(Debug)]
pub struct LogWriter {
    inner: BufWriter<File>,
}

impl LogWriter {
    /// Open (or create) the log file at `path` for appending.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            inner: BufWriter::new(file),
        })
    }

    /// Append one entry as a JSON line.
    pub fn write_entry(&mut self, entry: &LogEntry) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)?;
        writeln!(self.inner, "{line}")?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn log_path() -> PathBuf {
        std::env::temp_dir().join(format!("hanihi-log-test-{}.jsonl", uuid::Uuid::new_v4()))
    }

    #[test]
    fn write_and_read_back() {
        let path = log_path();
        let mut writer = LogWriter::open(&path).expect("open");
        let entry = LogEntry::session_created(
            Utc::now(),
            0,
            "abc".into(),
            "test".into(),
            "deepseek-chat".into(),
            "you are helpful".into(),
        );
        writer.write_entry(&entry).expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let parsed: LogEntry = serde_json::from_str(contents.trim()).expect("parse");
        match &parsed {
            LogEntry::SessionCreated { data, .. } => {
                assert_eq!(data.name, "test");
                assert_eq!(data.model, "deepseek-chat");
            }
            _ => panic!("expected SessionCreated"),
        }

        std::fs::remove_file(&path).unwrap_or(());
    }

    #[test]
    fn multiple_entries_are_lines() {
        let path = log_path();
        let mut writer = LogWriter::open(&path).expect("open");
        let now = Utc::now();
        writer
            .write_entry(&LogEntry::user_input(now, 1, "hello".into()))
            .expect("write");
        writer
            .write_entry(&LogEntry::turn_complete(now, 1, "hi".into(), 0))
            .expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        std::fs::remove_file(&path).unwrap_or(());
    }
}
