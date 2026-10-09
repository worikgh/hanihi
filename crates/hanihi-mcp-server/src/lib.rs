//! Shared MCP protocol server for Hānihi.
//!
//! The crate builds two binaries around this server loop:
//!
//! - `hanihi-mcp-server-ro` serves the non-destructive tools.
//! - `hanihi-mcp-server-rw` serves the tools that may modify files.
//!
//! MCP is stateless. See the [Model Context Protocol
//! specification](https://modelcontextprotocol.io/specification/2026-07-28/)
//! and the [schema](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/2026-07-28/schema.ts).

use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing_subscriber::EnvFilter;

mod apply_patch_tool;
mod create_file_tool;
mod delete_file_tool;
mod echo_tool;
mod find_symbol_tool;
mod noop_tool;
mod read_file_tool;
mod rename_file_tool;
mod search_text_tool;
mod tool;
mod version_ledger;
mod workspace_fs;
mod workspace_info_tool;

const SERVER_NAME: &str = "minimal-mcp-server";
const SERVER_VERSION: &str = "0.1.0";

// Change this if the client requires another MCP protocol version.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// One registered MCP tool: its `tools/list` entry and its `tools/call`
/// handler.
struct Tool {
    name: &'static str,
    json: fn() -> Value,
    exec: fn(&Value, Value) -> Value,
}

/// Runs the MCP server that exposes only non-destructive tools.
pub fn run_read_only_server() -> io::Result<()> {
    run_server(&read_only_tools(), "minimal-mcp-ro.log")
}

/// Runs the MCP server that exposes tools which may modify files.
pub fn run_write_server() -> io::Result<()> {
    run_server(&write_tools(), "minimal-mcp-rw.log")
}

/// Installs the stderr subscriber that renders tool `tracing` events.
///
/// Tools emit at `debug` and the default filter is `warn`, so the markers stay
/// silent unless `RUST_LOG` opts in. Diagnostics go to stderr because stdout is
/// owned exclusively by the JSON-RPC conversation.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
}

/// Tools that never modify the repository.
fn read_only_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "read_file",
            json: read_file_tool::json,
            exec: read_file_tool::exec,
        },
        Tool {
            name: "search_text",
            json: search_text_tool::json,
            exec: search_text_tool::exec,
        },
        Tool {
            name: "workspace_info",
            json: workspace_info_tool::json,
            exec: workspace_info_tool::exec,
        },
        Tool {
            name: "echo",
            json: echo_tool::json,
            exec: echo_tool::exec,
        },
        Tool {
            name: "noop",
            json: noop_tool::json,
            exec: noop_tool::exec,
        },
        Tool {
            name: "find_symbol",
            json: find_symbol_tool::json,
            exec: find_symbol_tool::exec,
        },
    ]
}

/// Tools that may modify repository files. Kept disjoint from the read-only
/// set so no tool appears in both servers.
fn write_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "apply_patch",
            json: apply_patch_tool::json,
            exec: apply_patch_tool::exec,
        },
        Tool {
            name: "create_file",
            json: create_file_tool::json,
            exec: create_file_tool::exec,
        },
        Tool {
            name: "delete_file",
            json: delete_file_tool::json,
            exec: delete_file_tool::exec,
        },
        Tool {
            name: "rename_file",
            json: rename_file_tool::json,
            exec: rename_file_tool::exec,
        },
    ]
}

/// Rudimentary file logging
struct Logger {
    file: std::fs::File,
}

impl Logger {
    fn new(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self { file })
    }

    fn log(&mut self, message: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);

        // Ignore logging failures so logging cannot terminate the server.
        let _ = writeln!(self.file, "[{timestamp}] {message}");
        let _ = self.file.flush();
    }
}

fn error_response(e: &serde_json::Error, logger: &mut Logger) -> serde_json::Value {
    logger.log(&format!("parse error: {e}"));

    json!({
    "jsonrpc": "2.0",
    "id": null,
    "error": {
    "code": -32700,
    "message": format!("Parse error: {e}")
    }
    })
}

fn run_server(tools: &[Tool], log_path: &str) -> io::Result<()> {
    let mut logger = Logger::new(log_path)?;

    init_tracing();

    logger.log("server started");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line_result in stdin.lock().lines() {
        let line = line_result?;

        if line.trim().is_empty() {
            continue;
        }

        logger.log(&format!("received: {}", line.trim()));

        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                let response = error_response(&error, &mut logger);
                write_response(&mut stdout, &response)?;
                logger.log(&format!("sent: {response}"));

                continue;
            }
        };

        let is_notification = message.get("id").is_none();

        if let Some(response) = handle_message(tools, &message) {
            if !is_notification {
                logger.log(&format!("sent: {response}"));
                write_response(&mut stdout, &response)?;
            } else {
                logger.log("notification handled without response");
            }
        }
    }

    logger.log("server stopped");

    Ok(())
}

fn handle_message(tools: &[Tool], message: &Value) -> Option<Value> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str);

    match method {
        Some("initialize") => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
            "tools": {}
            },
            "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
            }
            }
        })),

        // The client usually sends this as a notification after initialize.
        Some("notifications/initialized") => None,

        // ping checks whether the other side is alive.
        Some("ping") => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),

        // tools/list discovers available tools.
        Some("tools/list") => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tools.iter().map(|tool| (tool.json)()).collect::<Vec<_>>()
            }
        })),

        // tools/call invokes a tool.
        Some("tools/call") => Some(handle_tool_call(tools, id, message)),

        // resources/list lists concrete resources.
        Some("resources/list") => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
            "resources": []
            }
        })),

        // prompts/list discovers reusable prompts.
        Some("prompts/list") => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
            "prompts": []
            }
        })),

        Some(unknown_method) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
            "code": -32601,
            "message": format!("Method not found: {unknown_method}")
            }
        })),

        None => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
            "code": -32600,
            "message": "Invalid Request"
            }
        })),
    }
}

fn handle_tool_call(tools: &[Tool], id: Value, message: &Value) -> Value {
    let params = message.get("params").unwrap_or(&Value::Null);
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");

    if tool_name.is_empty() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
            "code": -32602,
            "message": "Missing tool name"
            }
        });
    }

    match tools.iter().find(|tool| tool.name == tool_name) {
        Some(tool) => (tool.exec)(params, id),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
            "code": -32602,
            "message": format!("Unknown tool: {tool_name}")
            }
        }),
    }
}

fn write_response(stdout: &mut impl Write, response: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, response)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_sets_do_not_overlap() {
        let read_only = read_only_tools();
        let write = write_tools();

        for read_tool in &read_only {
            assert!(
                !write
                    .iter()
                    .any(|write_tool| write_tool.name == read_tool.name),
                "tool appears in both the read-only and read-write sets: {}",
                read_tool.name
            );
        }
    }

    #[test]
    fn read_only_set_excludes_write_tools() {
        let read_only = read_only_tools();
        assert!(!read_only.iter().any(|tool| tool.name == "apply_patch"));
        assert!(!read_only.iter().any(|tool| tool.name == "create_file"));
        assert!(!read_only.iter().any(|tool| tool.name == "delete_file"));
        assert!(!read_only.iter().any(|tool| tool.name == "rename_file"));
    }

    #[test]
    fn read_only_set_contains_read_file() {
        let read_only = read_only_tools();
        let write = write_tools();

        assert!(read_only.iter().any(|tool| tool.name == "read_file"));
        assert!(!write.iter().any(|tool| tool.name == "read_file"));
    }

    #[test]
    fn write_set_contains_apply_patch() {
        let write = write_tools();
        assert!(write.iter().any(|tool| tool.name == "apply_patch"));
    }

    #[test]
    fn write_set_contains_file_tools() {
        let write = write_tools();
        for name in ["create_file", "delete_file", "rename_file"] {
            assert!(
                write.iter().any(|tool| tool.name == name),
                "missing write tool: {name}"
            );
        }
    }
}
