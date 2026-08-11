//! Session management: named, persistent conversation containers with
//! append-only JSONL event logs.
//!
//! A [`Session`] holds the on-disk state (metadata, event log, lock) for a
//! named conversation. It does not own an [`Agent`]; the caller passes the
//! agent to [`Session::run`], which wraps the agent loop with full logging.
//!
//! [`SessionManager`] owns the working directory and handles creation,
//! opening, and closing of sessions.
//!
//! [`Agent`]: crate::agent::Agent

pub mod lock;
pub mod log;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rig::completion::message::ToolCall;
use rig::completion::{AssistantContent, CompletionModel, Message};
use serde::Serialize;
use uuid::Uuid;

use self::lock::SessionGuard;
use self::log::{ErrorStage, LogEntry, LogWriter, ToolCallData, UsageData};
use crate::agent::{Agent, TurnSummary};
use crate::error::AgentError;

/// Errors produced by session operations.
#[derive(Debug)]
pub enum SessionError {
    /// The session name is reserved and cannot be created manually.
    ReservedName(String),
    /// A session with this name already exists.
    AlreadyExists(String),
    /// The requested session was not found.
    NotFound(String),
    /// The session is locked by another process.
    Locked(String),
    /// An I/O error occurred.
    Io(io::Error),
    /// A JSON (de)serialization error occurred.
    Json(serde_json::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::ReservedName(name) => {
                write!(f, "'{name}' is reserved and cannot be created manually")
            }
            SessionError::AlreadyExists(name) => {
                write!(f, "session '{name}' already exists")
            }
            SessionError::NotFound(name) => {
                write!(
                    f,
                    "session '{name}' not found — use --new-session to create it"
                )
            }
            SessionError::Locked(name) => {
                write!(f, "session '{name}' is locked by another process")
            }
            SessionError::Io(e) => write!(f, "io error: {e}"),
            SessionError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Io(e) => Some(e),
            SessionError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(e: io::Error) -> Self {
        SessionError::Io(e)
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(e: serde_json::Error) -> Self {
        SessionError::Json(e)
    }
}

impl From<AgentError> for SessionError {
    fn from(e: AgentError) -> Self {
        SessionError::Io(io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

/// Manages the set of active sessions within a working directory.
#[derive(Debug)]
pub struct SessionManager {
    working_dir: PathBuf,
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    /// Create a new manager. `working_dir` is resolved relative to the
    /// process cwd at startup (the caller must resolve it).
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            sessions: HashMap::new(),
        }
    }

    /// Path to the sessions directory: `<working_dir>/sessions/`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.working_dir.join("sessions")
    }

    /// Check whether a session exists (by name) on disk.
    pub fn exists(&self, name: &str) -> bool {
        self.session_dir(name).exists()
    }

    /// List session names found on disk.
    pub fn list(&self) -> io::Result<Vec<String>> {
        let dir = self.sessions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create a new session. Fails if the name is reserved or already exists.
    pub fn create(
        &mut self,
        name: &str,
        model: &str,
        system_prompt: &str,
    ) -> Result<&mut Session, SessionError> {
        if name == "default-session" {
            return Err(SessionError::ReservedName(name.into()));
        }
        if self.exists(name) {
            return Err(SessionError::AlreadyExists(name.into()));
        }
        let session = Session::create(&self.sessions_dir(), name, model, system_prompt)?;
        self.sessions.insert(name.to_string(), session);
        Ok(self.sessions.get_mut(name).expect("just inserted"))
    }

    /// Open an existing session. Fails if it doesn't exist or is locked.
    pub fn open(&mut self, name: &str) -> Result<&mut Session, SessionError> {
        if self.sessions.contains_key(name) {
            return self
                .sessions
                .get_mut(name)
                .ok_or_else(|| SessionError::NotFound(name.into()));
        }
        if !self.exists(name) {
            return Err(SessionError::NotFound(name.into()));
        }
        let session = Session::open(&self.sessions_dir(), name)?;
        self.sessions.insert(name.to_string(), session);
        Ok(self.sessions.get_mut(name).expect("just inserted"))
    }

    /// Close a session: writes `session_closed`, flushes the log, and
    /// releases the filesystem lock.
    pub fn close(&mut self, name: &str) -> Result<(), SessionError> {
        let mut session = self
            .sessions
            .remove(name)
            .ok_or_else(|| SessionError::NotFound(name.into()))?;
        let turn = session.turn;
        session.log_entry(&LogEntry::session_closed(
            Utc::now(),
            turn,
            session.id.to_string(),
            session.name.clone(),
        ))?;
        drop(session); // releases lock
        Ok(())
    }

    /// Path to a specific session directory.
    fn session_dir(&self, name: &str) -> PathBuf {
        self.sessions_dir().join(name)
    }
}

/// A named, persistent session that holds on-disk state and a log writer.
///
/// Created and managed via [`SessionManager`]; not constructed directly.
/// Call [`Session::run`] with an agent to execute a turn with full logging.
#[derive(Debug)]
pub struct Session {
    /// Unique identifier for this session (stable across restarts).
    pub id: Uuid,
    /// Human-readable session name.
    pub name: String,
    /// When the session was first created.
    pub created_at: chrono::DateTime<Utc>,
    /// Model name in use.
    pub model: String,
    /// Current turn number (monotonically increasing).
    pub turn: u64,

    /// Path to the session directory on disk.
    root: PathBuf,
    /// Append-only event log writer.
    log: LogWriter,
    /// Filesystem lock held for the lifetime of this session.
    _guard: SessionGuard,
}

impl Session {
    /// Create a new session on disk. Internal — use `SessionManager::create`.
    fn create(
        sessions_dir: &Path,
        name: &str,
        model: &str,
        system_prompt: &str,
    ) -> Result<Self, SessionError> {
        let dir = sessions_dir.join(name);
        fs::create_dir_all(&dir)?;

        let guard = SessionGuard::acquire(&dir).map_err(|_| SessionError::Locked(name.into()))?;

        let id = Uuid::new_v4();
        let created_at = Utc::now();

        // Write session.json.
        let meta = serde_json::json!({
            "id": id.to_string(),
            "name": name,
            "created_at": created_at.to_rfc3339(),
            "model": model,
            "system_prompt": system_prompt,
        });
        fs::write(
            dir.join("session.json"),
            serde_json::to_string_pretty(&meta)?,
        )?;

        // Open the event log.
        let log_path = dir.join("events.jsonl");
        let mut log = LogWriter::open(&log_path)?;

        // Log creation and first open.
        let now = Utc::now();
        log.write_entry(&LogEntry::session_created(
            now,
            0,
            id.to_string(),
            name.to_string(),
            model.to_string(),
            system_prompt.to_string(),
        ))?;
        log.write_entry(&LogEntry::session_opened(
            now,
            0,
            id.to_string(),
            name.to_string(),
        ))?;

        Ok(Self {
            id,
            name: name.to_string(),
            created_at,
            model: model.to_string(),
            turn: 0,
            root: dir,
            log,
            _guard: guard,
        })
    }

    /// Open an existing session. Internal — use `SessionManager::open`.
    fn open(sessions_dir: &Path, name: &str) -> Result<Self, SessionError> {
        let dir = sessions_dir.join(name);
        if !dir.exists() {
            return Err(SessionError::NotFound(name.into()));
        }

        let guard = SessionGuard::acquire(&dir).map_err(|_| SessionError::Locked(name.into()))?;

        // Read session.json.
        let meta_path = dir.join("session.json");
        let meta_bytes = fs::read(&meta_path)?;
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes)?;
        let id: Uuid = meta["id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Uuid::new_v4);
        let created_at: chrono::DateTime<Utc> = meta["created_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let model = meta["model"].as_str().unwrap_or("unknown").to_string();

        let log_path = dir.join("events.jsonl");
        let mut log = LogWriter::open(&log_path)?;

        let now = Utc::now();
        log.write_entry(&LogEntry::session_opened(
            now,
            0,
            id.to_string(),
            name.to_string(),
        ))?;

        Ok(Self {
            id,
            name: name.to_string(),
            created_at,
            model,
            turn: 0,
            root: dir,
            log,
            _guard: guard,
        })
    }

    /// Run one user turn with full logging.
    ///
    /// Wraps the agent loop: for each model call and tool execution, an
    /// event is written to the session log. Returns the same `TurnSummary`
    /// as [`Agent::run`].
    pub async fn run<M: CompletionModel>(
        &mut self,
        agent: &mut Agent<M>,
        provider: &str,
        model_name: &str,
        user_input: &str,
    ) -> Result<TurnSummary, AgentError> {
        self.turn += 1;

        // Log user input.
        self.log_entry(&LogEntry::user_input(
            Utc::now(),
            self.turn,
            user_input.to_string(),
        ))
        .map_err(|e| AgentError::Rig(e.to_string()))?;

        let mut turn_messages: Vec<Message> = Vec::new();
        let mut tool_calls_total = 0usize;
        let mut usage_total = rig::completion::Usage::new();

        for _ in 0..agent.max_turns() {
            // Build and log the prompt.
            let tools = agent.tool_definitions();
            let messages_json = self.messages_for_log(
                agent.system_prompt(),
                agent.history(),
                &turn_messages,
                user_input,
            )?;
            let tools_json =
                serde_json::to_value(&tools).map_err(|e| AgentError::Rig(e.to_string()))?;
            self.log_entry(&LogEntry::llm_prompt(
                Utc::now(),
                self.turn,
                provider.to_string(),
                model_name.to_string(),
                messages_json,
                tools_json,
            ))
            .map_err(|e| AgentError::Rig(e.to_string()))?;

            // Call the model.
            let response = agent.single_completion(user_input, &turn_messages).await?;
            usage_total += response.usage;

            // Extract content from the response.
            let mut text_parts: Vec<rig::completion::message::Text> = Vec::new();
            let mut reasoning_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            for content in response.choice.iter() {
                match content {
                    AssistantContent::Text(t) => text_parts.push(t.clone()),
                    AssistantContent::ToolCall(call) => tool_calls.push(call.clone()),
                    AssistantContent::Reasoning(r) => {
                        reasoning_parts.push(format!("{r:?}"));
                    }
                    AssistantContent::Image(_) => {}
                }
            }

            // Log the response.
            let response_text = if text_parts.is_empty() {
                None
            } else {
                Some(
                    text_parts
                        .iter()
                        .map(|t| t.text())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            };
            let response_reasoning = if reasoning_parts.is_empty() {
                None
            } else {
                Some(reasoning_parts.join("\n"))
            };
            let response_tool_calls: Option<Vec<ToolCallData>> = if tool_calls.is_empty() {
                None
            } else {
                Some(
                    tool_calls
                        .iter()
                        .map(|c| ToolCallData {
                            id: c.id.clone(),
                            name: c.function.name.clone(),
                            arguments: c.function.arguments.clone(),
                        })
                        .collect(),
                )
            };
            self.log_entry(&LogEntry::llm_response(
                Utc::now(),
                self.turn,
                response.message_id.clone(),
                response_text.clone(),
                response_reasoning,
                response_tool_calls.clone(),
                UsageData {
                    input_tokens: response.usage.input_tokens as u32,
                    output_tokens: response.usage.output_tokens as u32,
                },
            ))
            .map_err(|e| AgentError::Rig(e.to_string()))?;

            // No tool calls: turn complete.
            if tool_calls.is_empty() {
                let text = response_text.unwrap_or_default();
                turn_messages.push(Message::assistant(text.clone()));
                agent.commit_turn(user_input, turn_messages);

                self.log_entry(&LogEntry::turn_complete(
                    Utc::now(),
                    self.turn,
                    text.clone(),
                    tool_calls_total,
                ))
                .map_err(|e| AgentError::Rig(e.to_string()))?;

                return Ok(TurnSummary {
                    text,
                    tool_calls: tool_calls_total,
                    usage: usage_total,
                });
            }

            // Record the assistant message and execute tools.
            let mut contents: Vec<AssistantContent> =
                text_parts.into_iter().map(AssistantContent::Text).collect();
            contents.extend(tool_calls.iter().cloned().map(AssistantContent::ToolCall));
            turn_messages.push(Message::Assistant {
                id: response.message_id,
                content: rig::OneOrMany::from_iter_optional(contents)
                    .expect("assistant message has at least one tool call"),
            });

            for call in &tool_calls {
                let tool_args = call.function.arguments.clone();
                match agent.execute_tool(call).await {
                    Ok(output) => {
                        tool_calls_total += 1;
                        self.log_entry(&LogEntry::tool_execution(
                            Utc::now(),
                            self.turn,
                            call.id.clone(),
                            call.function.name.clone(),
                            tool_args,
                            output.clone(),
                        ))
                        .map_err(|e| AgentError::Rig(e.to_string()))?;
                        turn_messages.push(Message::tool_result_with_call_id(
                            call.id.clone(),
                            call.call_id.clone(),
                            output,
                        ));
                    }
                    Err(e) => {
                        // Log error and bail.
                        self.log_entry(&LogEntry::error(
                            Utc::now(),
                            self.turn,
                            ErrorStage::ToolExecution,
                            e.to_string(),
                        ))
                        .map_err(|se| AgentError::Rig(se.to_string()))?;
                        agent.commit_turn(user_input, turn_messages);
                        return Err(e);
                    }
                }
            }
        }

        // Max turns exceeded.
        self.log_entry(&LogEntry::error(
            Utc::now(),
            self.turn,
            ErrorStage::LlmCall,
            format!("exceeded maximum of {} model turns", agent.max_turns()),
        ))
        .map_err(|e| AgentError::Rig(e.to_string()))?;
        agent.commit_turn(user_input, turn_messages);
        Err(AgentError::MaxTurns {
            turns: agent.max_turns(),
        })
    }

    /// Build a JSON representation of the messages sent to the model,
    /// for inclusion in the `llm_prompt` log entry.
    fn messages_for_log(
        &self,
        system_prompt: &str,
        history: &[Message],
        turn_messages: &[Message],
        user_input: &str,
    ) -> Result<serde_json::Value, AgentError> {
        #[derive(Serialize)]
        struct LogMessage {
            role: String,
            content: serde_json::Value,
        }

        let mut msgs: Vec<LogMessage> = Vec::new();

        // System preamble.
        msgs.push(LogMessage {
            role: "system".into(),
            content: serde_json::Value::String(system_prompt.to_string()),
        });

        // Serialize each message via serde.
        fn msg_to_value(msg: &Message) -> Result<LogMessage, AgentError> {
            let v = serde_json::to_value(msg).map_err(|e| AgentError::Rig(e.to_string()))?;
            let role = v["role"].as_str().unwrap_or("unknown").to_string();
            Ok(LogMessage {
                role,
                content: v["content"].clone(),
            })
        }

        for m in history.iter().chain(turn_messages.iter()) {
            msgs.push(msg_to_value(m)?);
        }
        // Current user message.
        msgs.push(LogMessage {
            role: "user".into(),
            content: serde_json::Value::String(user_input.to_string()),
        });

        serde_json::to_value(&msgs).map_err(|e| AgentError::Rig(e.to_string()))
    }

    /// Append an entry to the event log.
    pub fn log_entry(&mut self, entry: &LogEntry) -> Result<(), SessionError> {
        self.log.write_entry(entry)?;
        Ok(())
    }

    /// Read the full session metadata from `session.json`.
    pub fn read_meta(&self) -> Result<serde_json::Value, SessionError> {
        let meta_path = self.root.join("session.json");
        let bytes = fs::read(&meta_path)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok(value)
    }

    /// Path to the events log file.
    pub fn events_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// Path to the session metadata file.
    pub fn meta_path(&self) -> PathBuf {
        self.root.join("session.json")
    }

    // --- Derived property accessors (Step 4) ---

    /// Parse all events from the log file and return them.
    pub fn events(&self) -> Result<Vec<LogEntry>, SessionError> {
        let path = self.events_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&path)?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: LogEntry = serde_json::from_str(line).map_err(SessionError::Json)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Compute cumulative token usage from all `llm_response` events.
    pub fn cumulative_usage(&self) -> Result<(u64, u64), SessionError> {
        let mut total_in: u64 = 0;
        let mut total_out: u64 = 0;
        for entry in self.events()? {
            if let LogEntry::LlmResponse { data, .. } = &entry {
                total_in += data.usage.input_tokens as u64;
                total_out += data.usage.output_tokens as u64;
            }
        }
        Ok((total_in, total_out))
    }

    /// Timestamp of the most recent `session_opened` event.
    pub fn last_opened_at(&self) -> Result<Option<chrono::DateTime<Utc>>, SessionError> {
        let events = self.events()?;
        let last = events
            .iter()
            .rev()
            .find(|e| matches!(e, LogEntry::SessionOpened { .. }));
        Ok(last.map(|e| e.ts()))
    }

    /// Total turns from the most recent `turn_complete` event.
    pub fn total_turns(&self) -> Result<u64, SessionError> {
        let events = self.events()?;
        let last = events
            .iter()
            .rev()
            .find(|e| matches!(e, LogEntry::TurnComplete { .. }));
        Ok(last.map(|e| e.turn()).unwrap_or(0))
    }

    /// Per-call model latencies computed from prompt/response timestamp
    /// pairs. Returns a vector of (ts, latency_ms) for each completed call.
    pub fn latencies(&self) -> Result<Vec<(chrono::DateTime<Utc>, i64)>, SessionError> {
        let events = self.events()?;
        let mut latencies = Vec::new();
        let mut pending_prompt_ts: Option<chrono::DateTime<Utc>> = None;

        for entry in &events {
            match entry {
                LogEntry::LlmPrompt { ts, .. } => {
                    pending_prompt_ts = Some(*ts);
                }
                LogEntry::LlmResponse { ts, .. } => {
                    if let Some(prompt_ts) = pending_prompt_ts.take() {
                        let latency_ms = (*ts - prompt_ts).num_milliseconds();
                        latencies.push((*ts, latency_ms));
                    }
                }
                _ => {}
            }
        }

        Ok(latencies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_working_dir() -> PathBuf {
        std::env::temp_dir().join(format!("hanihi-session-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn create_and_open_session() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);

        let session = mgr
            .create("test-session", "deepseek-chat", "you are helpful")
            .expect("create");
        assert_eq!(session.name, "test-session");
        assert_eq!(session.turn, 0);
        assert!(session.root.join("session.json").exists());
        assert!(session.root.join("events.jsonl").exists());

        mgr.close("test-session").expect("close");

        let session2 = mgr.open("test-session").expect("open");
        assert_eq!(session2.name, "test-session");

        mgr.close("test-session").expect("close");
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn reserved_name_rejected() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);
        let err = mgr
            .create("default-session", "deepseek-chat", "prompt")
            .expect_err("should fail");
        assert!(matches!(err, SessionError::ReservedName(_)));
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn duplicate_name_rejected() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);
        mgr.create("dup", "deepseek-chat", "prompt")
            .expect("first create");
        let err = mgr
            .create("dup", "deepseek-chat", "prompt")
            .expect_err("second create");
        assert!(matches!(err, SessionError::AlreadyExists(_)));
        mgr.close("dup").expect("close");
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn not_found_error() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);
        let err = mgr.open("nonexistent").expect_err("should fail");
        assert!(matches!(err, SessionError::NotFound(_)));
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn list_sessions() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);
        mgr.create("alpha", "deepseek-chat", "p").expect("create");
        mgr.create("beta", "deepseek-chat", "p").expect("create");

        mgr.close("alpha").expect("close");
        mgr.close("beta").expect("close");

        let names = mgr.list().expect("list");
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));

        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn derived_properties_on_empty_session() {
        let dir = tmp_working_dir();
        let mut mgr = SessionManager::new(&dir);
        let session = mgr
            .create("derive-test", "deepseek-chat", "p")
            .expect("create");

        // session has only created+opened events, no turn_complete.
        assert_eq!(session.total_turns().expect("total_turns"), 0);
        let (tin, tout) = session.cumulative_usage().expect("usage");
        assert_eq!(tin, 0);
        assert_eq!(tout, 0);
        let latencies = session.latencies().expect("latencies");
        assert!(latencies.is_empty());

        mgr.close("derive-test").expect("close");
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }
}
