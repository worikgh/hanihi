//! Session management: named, persistent conversation containers with
//! append-only JSONL event logs.
//!
//! A [`Session`] wraps an [`Agent`] and logs every prompt, response, tool
//! execution, and lifecycle event to disk. [`SessionManager`] owns the
//! working directory and handles creation, opening, and closing.

pub mod lock;
pub mod log;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;
use uuid::Uuid;

use self::lock::SessionGuard;
use self::log::{ErrorStage, LogEntry, LogWriter, ToolCallData, UsageData};

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
        if let Some(_session) = self.sessions.get(name) {
            // Already open in this manager.
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

/// A named, persistent session that wraps an agent and logs events to disk.
///
/// Created and managed via [`SessionManager`]; not constructed directly.
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

        // Create.
        let session = mgr
            .create("test-session", "deepseek-chat", "you are helpful")
            .expect("create");
        assert_eq!(session.name, "test-session");
        assert_eq!(session.turn, 0);
        assert!(session.root.join("session.json").exists());
        assert!(session.root.join("events.jsonl").exists());

        // Close.
        mgr.close("test-session").expect("close");

        // Re-open.
        let session2 = mgr.open("test-session").expect("open");
        assert_eq!(session2.name, "test-session");

        // Clean up.
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

        // Close before listing (list scans disk).
        mgr.close("alpha").expect("close");
        mgr.close("beta").expect("close");

        let names = mgr.list().expect("list");
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));

        std::fs::remove_dir_all(&dir).unwrap_or(());
    }
}
