//! # hanihi-core
//!
//! A minimal tool-calling agent harness built on:
//!
//! - **rig** (`rig-core`) — model clients, tool definitions, completion loop
//! - **rmcp** — Model Context Protocol client for attaching external tools
//!
//! The agent keeps a persistent message history, exposes tools to the model,
//! executes requested tool calls, and feeds results back until the model
//! answers. See [`agent::Agent`] and [`agent::connect_chat_model`].

pub mod agent;
pub mod error;
pub mod mcp;
pub mod session;
pub mod source;
pub mod tool;
pub mod write;

pub use agent::{
    Agent, StreamEvent, TurnSummary, connect_chat_model, connect_chat_model_with_prompt,
};
pub use error::AgentError;
pub use mcp::McpClient;
pub use session::{Session, SessionError, SessionManager};
pub use source::{Language, MAX_READ_BYTES, SourceError, SourceTree};
pub use tool::{
    builtin_echo, builtin_get_time, builtin_grep, builtin_list_dir, builtin_read_file,
    builtin_read_session_log, builtin_run_command,
};
pub use write::{builtin_apply_patch, builtin_write_file};
