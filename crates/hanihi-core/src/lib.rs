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
pub mod tool;

pub use agent::{Agent, TurnSummary, connect_chat_model};
pub use error::AgentError;
pub use mcp::McpClient;
pub use tool::{builtin_echo, builtin_get_time};
