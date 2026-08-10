//! Agent error type.
//!
//! Custom error enum with manual `Display`/`Error` impls, per workspace
//! conventions (no `thiserror`/`anyhow`).

use std::error::Error;
use std::fmt;

/// Errors produced by the agent harness.
#[derive(Debug)]
pub enum AgentError {
    /// No API key was supplied (CLI/env/arg all empty).
    MissingApiKey,
    /// JSON (de)serialization failure.
    Json(serde_json::Error),
    /// I/O failure (spawning MCP servers, transport setup).
    Io(std::io::Error),
    /// Error from the rig completion stack.
    Rig(String),
    /// Error from the MCP client stack.
    Mcp(String),
    /// A tool failed to execute or was not found.
    Tool {
        /// Tool name that failed.
        name: String,
        /// Failure message.
        message: String,
    },
    /// The agent loop exceeded the maximum number of model turns.
    MaxTurns {
        /// Maximum turns allowed.
        turns: usize,
    },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::MissingApiKey => {
                write!(f, "no API key supplied (set LLM_API_KEY or pass --api-key)")
            }
            AgentError::Json(e) => write!(f, "json error: {e}"),
            AgentError::Io(e) => write!(f, "io error: {e}"),
            AgentError::Rig(msg) => write!(f, "rig error: {msg}"),
            AgentError::Mcp(msg) => write!(f, "mcp error: {msg}"),
            AgentError::Tool { name, message } => write!(f, "tool '{name}' failed: {message}"),
            AgentError::MaxTurns { turns } => {
                write!(f, "agent exceeded maximum of {turns} model turns")
            }
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AgentError::Json(e) => Some(e),
            AgentError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        AgentError::Json(e)
    }
}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        AgentError::Io(e)
    }
}

impl From<rig::completion::CompletionError> for AgentError {
    fn from(e: rig::completion::CompletionError) -> Self {
        AgentError::Rig(e.to_string())
    }
}

impl From<rmcp::service::ServiceError> for AgentError {
    fn from(e: rmcp::service::ServiceError) -> Self {
        AgentError::Mcp(e.to_string())
    }
}

impl From<rmcp::service::ClientInitializeError> for AgentError {
    fn from(e: rmcp::service::ClientInitializeError) -> Self {
        AgentError::Mcp(e.to_string())
    }
}
