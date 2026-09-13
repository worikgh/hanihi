//! Append-only JSONL event log for session persistence.
//!
//! [`LogWriter`] appends one line of JSON per event to an `events.jsonl` file.
//! Each line is a complete, independently parseable JSON object carrying a
//! `schema` version so readers can detect old and future formats.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use rig::completion::Message;
use serde::{Deserialize, Serialize};

/// Current log-line schema version.
///
/// Policy: additive changes (a new optional field with `#[serde(default)]`)
/// do not bump this. Breaking changes (rename, remove, restructure) bump it
/// and add a migration in [`migrate`].
pub const SCHEMA_VERSION: u32 = 1;

/// One message in a logged completion request.
///
/// The three variants are carried as typed data through the agent loop so the
/// JSON form is only produced when the prompt leaves the process — written to
/// the log, or handed to a model client. The wire shape is
/// `{"role": <role>, "content": <content>}` on both sides of the round trip.
///
/// Note on round-tripping: a `rig` tool-result message also renders as
/// `role: "user"` with a *string* content, so a re-read log line cannot
/// distinguish it from a plain [`ContextMessage::User`]. That ambiguity is
/// accepted: `llm_prompt` entries are log-only, and [`crate::session::Session::replay_history`]
/// rebuilds transcripts from `llm_response`/`tool_execution`, never from
/// `llm_prompt`.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextMessage {
    /// System preamble. Not a `rig` `Message`: it travels as `preamble()`.
    System(String),
    /// A prior-turn or in-progress-turn message (`rig::completion::Message`).
    Message(Message),
    /// The current turn's user input, appended last on every model call.
    User(String),
}

impl ContextMessage {
    /// JSON shape written to the `llm_prompt` log entry: `{role, content}`.
    ///
    /// `content` is the plain string for system/user messages and the
    /// serialized body of the `Message` otherwise. Only `role` and `content`
    /// are emitted; every other field serde produces for a `Message` is
    /// deliberately dropped, matching the historical log format.
    pub(crate) fn to_log_json(&self) -> serde_json::Value {
        let (role, content) = match self {
            ContextMessage::System(text) => (
                "system".to_string(),
                serde_json::Value::String(text.clone()),
            ),
            ContextMessage::User(text) => {
                ("user".to_string(), serde_json::Value::String(text.clone()))
            }
            ContextMessage::Message(msg) => {
                let value = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
                let role = value["role"].as_str().unwrap_or("unknown").to_string();
                (role, value["content"].clone())
            }
        };
        serde_json::json!({ "role": role, "content": content })
    }

    /// Rebuild a `ContextMessage` from its `{role, content}` log shape.
    ///
    /// A `system` role is always the preamble; a `user` role is the plain
    /// current-turn input when `content` is a string, and otherwise a
    /// serialized `rig` message (the ambiguous tool-result case — see the
    /// type-level note).
    fn from_log_json(value: serde_json::Value) -> Result<Self, String> {
        let role = value
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| "context message is missing a string 'role'".to_string())?;
        let content = value
            .get("content")
            .cloned()
            .ok_or_else(|| "context message is missing 'content'".to_string())?;

        match role {
            "system" => match content {
                serde_json::Value::String(text) => Ok(ContextMessage::System(text)),
                other => Ok(ContextMessage::Message(
                    serde_json::from_value(rebuild_message_json("system", other))
                        .map_err(|e| e.to_string())?,
                )),
            },
            "user" => match content {
                serde_json::Value::String(text) => Ok(ContextMessage::User(text)),
                other => Ok(ContextMessage::Message(
                    serde_json::from_value(rebuild_message_json("user", other))
                        .map_err(|e| e.to_string())?,
                )),
            },
            other => Ok(ContextMessage::Message(
                serde_json::from_value(rebuild_message_json(other, content))
                    .map_err(|e| e.to_string())?,
            )),
        }
    }
}

/// Reassemble the `rig` `Message` JSON from a split `role`/`content` pair, as
/// [`ContextMessage::to_log_json`] strips everything but those two fields.
fn rebuild_message_json(role: &str, content: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "role": role, "content": content })
}

impl Serialize for ContextMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_log_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContextMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_log_json(value).map_err(serde::de::Error::custom)
    }
}

/// One entry in the session event log.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionCreatedData {
    pub session_id: String,
    pub name: String,
    pub model: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionOpenedData {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionClosedData {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UserInputData {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct LlmPromptData {
    pub provider: String,
    pub model: String,
    pub messages: Vec<ContextMessage>,
    pub tool_definitions: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ToolCallData {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UsageData {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl fmt::Display for UsageData {
    /// Token counts on one line, e.g. `12 input tokens, 34 output tokens`.
    ///
    /// Used by the `LlmResponse` variant of [`LogEntry`] and by report
    /// renderers that summarise a turn's cost.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} input tokens, {} output tokens",
            self.input_tokens, self.output_tokens
        )
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ToolExecutionData {
    pub tool_call_id: String,
    #[serde(default)]
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TurnCompleteData {
    pub text: String,
    pub tool_calls: usize,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ErrorData {
    pub stage: ErrorStage,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
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
        messages: Vec<ContextMessage>,
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

    /// Append one entry as a JSON line, injecting the current schema version.
    pub fn write_entry(&mut self, entry: &LogEntry) -> std::io::Result<()> {
        let mut value = serde_json::to_value(entry)?;
        value
            .as_object_mut()
            .expect("LogEntry serializes as a JSON object")
            .insert("schema".into(), serde_json::json!(SCHEMA_VERSION));
        let line = serde_json::to_string(&value)?;
        writeln!(self.inner, "{line}")?;
        self.inner.flush()
    }
}

// --- Reading ---

/// A single line-level error from reading a session log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogReadError {
    /// 1-based line number in the log file.
    pub line: usize,
    /// Human-readable description of the failure.
    pub message: String,
}

impl fmt::Display for LogReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for LogReadError {}

/// Outcome of a tolerant log read: valid entries plus any bad lines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogReadResult {
    /// Entries from lines that parsed successfully.
    pub entries: Vec<LogEntry>,
    /// Errors for lines that were skipped, in file order.
    pub errors: Vec<LogReadError>,
}

/// Apply known migrations for log lines older than [`SCHEMA_VERSION`].
///
/// Hook for future breaking changes. There are no migrations yet: version 0
/// (legacy, no `schema` field) parses through the existing lenient
/// `#[serde(default)]` fields.
fn migrate(_value: &mut serde_json::Value, _from: u32) -> Result<(), String> {
    Ok(())
}

/// Parse one non-blank log line into a [`LogEntry`], enforcing schema checks.
fn parse_entry_line(line: &str) -> Result<LogEntry, String> {
    let mut value: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;

    let schema = match value.get("schema") {
        None => 0,
        Some(schema) => schema
            .as_u64()
            .ok_or_else(|| "schema must be an integer".to_string())? as u32,
    };

    if schema > SCHEMA_VERSION {
        return Err(format!(
            "schema {schema} is newer than supported schema {SCHEMA_VERSION}"
        ));
    }
    if schema < SCHEMA_VERSION {
        migrate(&mut value, schema)?;
    }

    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Parse every log line, returning the first error.
pub fn parse_log_strict(contents: &str) -> Result<Vec<LogEntry>, LogReadError> {
    let mut entries = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_entry_line(line) {
            Ok(entry) => entries.push(entry),
            Err(message) => {
                return Err(LogReadError {
                    line: index + 1,
                    message,
                });
            }
        }
    }
    Ok(entries)
}

/// Parse every log line, collecting valid entries and reporting bad lines.
pub fn parse_log_tolerant(contents: &str) -> LogReadResult {
    let mut result = LogReadResult::default();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_entry_line(line) {
            Ok(entry) => result.entries.push(entry),
            Err(message) => result.errors.push(LogReadError {
                line: index + 1,
                message,
            }),
        }
    }
    result
}

/// Read a log file from disk, collecting valid entries and reporting bad lines.
pub fn read_log_tolerant(path: &Path) -> std::io::Result<LogReadResult> {
    let contents = std::fs::read_to_string(path)?;
    Ok(parse_log_tolerant(&contents))
}

// Display for `LogEntry`
use std::fmt::{Display, Formatter};

impl Display for LogEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionCreated { ts, turn, data } => {
                writeln!(f, "[{ts}] Session created (turn {turn})")?;
                writeln!(f, "  ID: {id}", id = data.session_id)?;
                writeln!(f, "  Name: {name}", name = data.name)?;
                writeln!(f, "  Model: {model}", model = data.model)?;
                write!(f, "  System prompt: {}", data.system_prompt)
            }

            Self::SessionOpened { ts, turn, data } => {
                write!(
                    f,
                    "[{ts}] Session opened (turn {turn}) — {} ({})",
                    data.name, data.session_id
                )
            }

            Self::SessionClosed { ts, turn, data } => {
                write!(
                    f,
                    "[{ts}] Session closed (turn {turn}) — {} ({})",
                    data.name, data.session_id
                )
            }

            Self::UserInput { ts, turn, data } => {
                writeln!(f, "[{ts}] User input (turn {turn})")?;
                write_indented(f, &data.text, "  ")
            }

            Self::LlmPrompt { ts, turn, data } => {
                writeln!(f, "[{ts}] LLM prompt (turn {turn})")?;
                writeln!(f, "  Provider: {}", data.provider)?;
                writeln!(f, "  Model: {}", data.model)?;
                let json = serde_json::to_string_pretty(&data.messages).map_err(|_| fmt::Error)?;
                writeln!(f, "  Messages: {} characters", json.len())?;
                writeln!(f)?;
                writeln!(f, "  Tool definitions:")?;
                write_json_indented(f, &data.tool_definitions, "    ")
            }

            Self::LlmResponse { ts, turn, data } => {
                writeln!(f, "[{ts}] LLM response (turn {turn})")?;

                if let Some(message_id) = &data.message_id {
                    writeln!(f, "  Message ID: {message_id}")?;
                }

                if let Some(text) = &data.text {
                    writeln!(f, "  Text:")?;
                    write_indented(f, text, "    ")?;
                    writeln!(f)?;
                }

                if let Some(reasoning) = &data.reasoning {
                    writeln!(f, "  Reasoning:")?;
                    write_indented(f, reasoning, "    ")?;
                    writeln!(f)?;
                }

                if let Some(tool_calls) = &data.tool_calls
                    && !tool_calls.is_empty()
                {
                    writeln!(f, "  Tool calls:")?;

                    for call in tool_calls {
                        writeln!(f, "    - {} ({})", call.name, call.id)?;
                        writeln!(f, "      Arguments:")?;
                        write_json_indented(f, &call.arguments, "        ")?;
                        writeln!(f)?;
                    }
                }

                write!(f, "  Usage: {}", data.usage)
            }

            Self::ToolExecution { ts, turn, data } => {
                writeln!(f, "[{ts}] Tool execution (turn {turn})")?;
                writeln!(f, "  Tool: {}", data.name)?;
                writeln!(f, "  Call ID: {}", tool_call_id(data))?;
                writeln!(f, "  Arguments:")?;
                write_json_indented(f, &data.arguments, "    ")?;
                writeln!(f)?;
                writeln!(f, "  Result:")?;
                write_indented(f, &data.result, "    ")
            }

            Self::TurnComplete { ts, turn, data } => {
                writeln!(f, "[{ts}] Turn complete (turn {turn})")?;
                writeln!(f, "  Tool calls: {}", data.tool_calls)?;
                write_indented(f, &data.text, "  ")
            }

            Self::Error { ts, turn, data } => {
                write!(
                    f,
                    "[{ts}] Error during {} (turn {turn}): {}",
                    data.stage, data.message
                )
            }
        }
    }
}

impl Display for ErrorStage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LlmCall => write!(f, "LLM call"),
            Self::ToolExecution => write!(f, "tool execution"),
        }
    }
}

fn write_indented(f: &mut Formatter<'_>, text: &str, indent: &str) -> fmt::Result {
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            writeln!(f)?;
        }

        write!(f, "{indent}{line}")?;
    }

    Ok(())
}

fn write_json_indented(
    f: &mut Formatter<'_>,
    value: &serde_json::Value,
    indent: &str,
) -> fmt::Result {
    let json = serde_json::to_string_pretty(value).map_err(|_| fmt::Error)?;

    for (index, line) in json.lines().enumerate() {
        if index > 0 {
            writeln!(f)?;
        }

        write!(f, "{indent}{line}")?;
    }

    Ok(())
}

fn tool_call_id(data: &ToolExecutionData) -> &str {
    if data.call_id.is_empty() {
        &data.tool_call_id
    } else {
        &data.call_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn log_path() -> PathBuf {
        std::env::temp_dir().join(format!("hanihi-log-test-{}.jsonl", uuid::Uuid::new_v4()))
    }

    fn valid_user_input_line() -> String {
        serde_json::json!({
            "schema": SCHEMA_VERSION,
            "kind": "user_input",
            "ts": "2026-01-01T00:00:00Z",
            "turn": 1,
            "data": {"text": "hi"}
        })
        .to_string()
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

    #[test]
    fn writer_injects_schema() {
        let path = log_path();
        let mut writer = LogWriter::open(&path).expect("open");
        writer
            .write_entry(&LogEntry::user_input(Utc::now(), 1, "hi".into()))
            .expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(contents.trim()).expect("parse json");
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["kind"], "user_input");

        std::fs::remove_file(&path).unwrap_or(());
    }

    #[test]
    fn strict_parses_schema_and_legacy_lines() {
        let legacy =
            r#"{"kind":"user_input","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"text":"hi"}}"#;
        let contents = format!("{}\n{}\n", valid_user_input_line(), legacy);
        let entries = parse_log_strict(&contents).expect("parse strict");
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], LogEntry::UserInput { .. }));
        assert!(matches!(entries[1], LogEntry::UserInput { .. }));
    }

    #[test]
    fn strict_reports_first_error_with_line_number() {
        let contents = format!(
            "{}\nnot json\n{}\n",
            valid_user_input_line(),
            valid_user_input_line()
        );
        let err = parse_log_strict(&contents).expect_err("bad line must fail");
        assert_eq!(err.line, 2);
        assert!(!err.message.is_empty(), "error message must be non-empty");
    }

    #[test]
    fn tolerant_collects_valid_and_reports_bad_lines() {
        let future = serde_json::json!({
            "schema": SCHEMA_VERSION + 1,
            "kind": "user_input",
            "ts": "2026-01-01T00:00:00Z",
            "turn": 1,
            "data": {"text": "future"}
        })
        .to_string();
        let contents = format!(
            "\n{}\nnot json\n{}\n{}\n",
            valid_user_input_line(),
            future,
            valid_user_input_line()
        );
        let result = parse_log_tolerant(&contents);

        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.errors.len(), 2);

        assert_eq!(result.errors[0].line, 3);
        assert!(!result.errors[0].message.is_empty());

        assert_eq!(result.errors[1].line, 4);
        assert!(
            result.errors[1].message.contains("newer"),
            "unexpected: {}",
            result.errors[1].message
        );
    }

    #[test]
    fn tolerant_empty_input() {
        let result = parse_log_tolerant("");
        assert!(result.entries.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn strict_reports_future_schema() {
        let future = serde_json::json!({
            "schema": SCHEMA_VERSION + 1,
            "kind": "user_input",
            "ts": "2026-01-01T00:00:00Z",
            "turn": 1,
            "data": {"text": "future"}
        })
        .to_string();
        let err = parse_log_strict(&future).expect_err("future schema must fail");
        assert!(err.message.contains("newer"), "unexpected: {}", err.message);
    }

    #[test]
    fn strict_rejects_non_integer_schema() {
        let bad = r#"{"schema":"one","kind":"user_input","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"text":"hi"}}"#;
        let err = parse_log_strict(bad).expect_err("non-integer schema must fail");
        assert!(
            err.message.contains("integer"),
            "unexpected: {}",
            err.message
        );
    }

    #[test]
    fn usage_display_formats_both_counts() {
        let usage = UsageData {
            input_tokens: 12,
            output_tokens: 34,
        };
        assert_eq!(usage.to_string(), "12 input tokens, 34 output tokens");
    }

    #[test]
    fn usage_display_formats_zero_counts() {
        let usage = UsageData {
            input_tokens: 0,
            output_tokens: 0,
        };
        assert_eq!(usage.to_string(), "0 input tokens, 0 output tokens");
    }

    #[test]
    fn llm_response_display_uses_usage_display() {
        let data = LlmResponseData {
            message_id: None,
            text: None,
            reasoning: None,
            tool_calls: None,
            usage: UsageData {
                input_tokens: 12,
                output_tokens: 34,
            },
        };
        let entry = LogEntry::LlmResponse {
            ts: "2026-01-01T00:00:00Z".parse().expect("valid timestamp"),
            turn: 1,
            data,
        };
        assert!(
            entry
                .to_string()
                .contains("  Usage: 12 input tokens, 34 output tokens"),
            "unexpected: {entry}"
        );
    }

    /// The on-disk shape of a prompt is frozen: writing a typed context list
    /// and reading it back must reproduce the same JSON a pre-typed log had —
    /// `[{role, content}, …]`, with no extra fields.
    #[test]
    fn llm_prompt_messages_serialize_to_role_and_content() {
        let data = LlmPromptData {
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            messages: vec![
                ContextMessage::System("sys".into()),
                ContextMessage::Message(Message::user("earlier")),
                ContextMessage::Message(Message::assistant("partial")),
                ContextMessage::User("now".into()),
            ],
            tool_definitions: serde_json::json!([]),
        };

        let value = serde_json::to_value(&data).expect("serialize");
        let messages = value["messages"].as_array().expect("messages is an array");

        assert_eq!(messages.len(), 4);
        // Only `role` and `content` are emitted — no extra `Message` fields.
        assert_eq!(messages[0].as_object().expect("object").len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "sys");
        // A `rig::Message` carries content as a typed block array, not as a
        // bare string — only `ContextMessage::{System, User}` are plain.
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][0]["text"], "earlier");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "now");
    }

    /// A prompt round-trips through JSON, so the log stays parseable by the
    /// writer that produced it.
    ///
    /// The round trip is *shape*-preserving, not lossless: `to_log_json`
    /// keeps only `role` + `content`, so a `Message::Assistant`'s `id` is
    /// dropped. This is the pre-existing log format — the test pins it so a
    /// future change to `ContextMessage` cannot silently widen the wire form.
    #[test]
    fn llm_prompt_messages_round_trip() {
        let call = rig::completion::message::ToolCall::new(
            "call_1".to_string(),
            rig::completion::message::ToolFunction {
                name: "get_time".to_string(),
                arguments: serde_json::json!({}),
            },
        );
        let messages = vec![
            ContextMessage::System("sys".into()),
            ContextMessage::Message(Message::Assistant {
                id: None,
                content: rig::OneOrMany::one(rig::completion::AssistantContent::ToolCall(call)),
            }),
            ContextMessage::User("now".into()),
        ];
        let entry = LogEntry::llm_prompt(
            "2026-01-01T00:00:00Z".parse().expect("valid timestamp"),
            1,
            "deepseek".into(),
            "deepseek-chat".into(),
            messages.clone(),
            serde_json::json!([]),
        );

        let line = serde_json::to_string(&entry).expect("serialize");
        let parsed: LogEntry = serde_json::from_str(&line).expect("parse");
        assert_eq!(parsed, entry);

        // The tool call survives as a `Message`, not as a plain user string.
        match &parsed {
            LogEntry::LlmPrompt { data, .. } => {
                assert_eq!(data.messages, messages);
                assert!(matches!(data.messages[1], ContextMessage::Message(_)));
            }
            other => panic!("expected LlmPrompt, got {other:?}"),
        }
    }

    /// A log line written before the typed context existed must still parse:
    /// string content means the plain system/user variants.
    #[test]
    fn llm_prompt_parses_legacy_role_content_lines() {
        // One physical line: `parse_log_strict` is line-oriented.
        let legacy = r#"{"schema":1,"kind":"llm_prompt","ts":"2026-01-01T00:00:00Z","turn":1,"data":{"provider":"d","model":"m","tool_definitions":[],"messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"}]}}"#;
        let entry = parse_log_strict(legacy).expect("legacy line must parse");

        match &entry[0] {
            LogEntry::LlmPrompt { data, .. } => {
                assert_eq!(
                    data.messages,
                    vec![
                        ContextMessage::System("sys".into()),
                        ContextMessage::User("hi".into()),
                    ]
                );
            }
            other => panic!("expected LlmPrompt, got {other:?}"),
        }
    }

    /// The `id` a `Message::Assistant` carries is not part of the wire form,
    /// so it does not survive the round trip. Documented, not a bug: the log
    /// is a rendering of the prompt, not a serialization of the transcript.
    #[test]
    fn llm_prompt_round_trip_drops_message_id() {
        let original = Message::Assistant {
            id: Some("msg_1".to_string()),
            content: rig::OneOrMany::one(rig::completion::AssistantContent::Text(
                rig::completion::message::Text::new("hello"),
            )),
        };
        let entry = LogEntry::llm_prompt(
            "2026-01-01T00:00:00Z".parse().expect("valid timestamp"),
            1,
            "d".into(),
            "m".into(),
            vec![ContextMessage::Message(original)],
            serde_json::json!([]),
        );

        let parsed: LogEntry =
            serde_json::from_str(&serde_json::to_string(&entry).expect("serialize"))
                .expect("parse");

        match &parsed {
            LogEntry::LlmPrompt { data, .. } => match &data.messages[0] {
                ContextMessage::Message(Message::Assistant { id, .. }) => assert_eq!(*id, None),
                other => panic!("expected an assistant message, got {other:?}"),
            },
            other => panic!("expected LlmPrompt, got {other:?}"),
        }
    }

    #[test]
    fn llm_prompt_display_counts_serialized_messages() {
        let entry = LogEntry::llm_prompt(
            "2026-01-01T00:00:00Z".parse().expect("valid timestamp"),
            1,
            "deepseek".into(),
            "deepseek-chat".into(),
            vec![ContextMessage::System("sys".into())],
            serde_json::json!([]),
        );

        let rendered = entry.to_string();
        assert!(rendered.contains("  Messages: "), "got: {rendered}");
        assert!(rendered.contains(" characters"), "got: {rendered}");
    }
}
