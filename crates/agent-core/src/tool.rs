//! Built-in tools for the agent.
//!
//! Tools are rig [`PortableDynamicTool`]s: name + description + JSON schema +
//! an async callback taking raw `serde_json::Value` arguments.

use rig::tool::{PortableDynamicTool, ToolOutput};
use serde_json::json;

/// Tool: report the current local date and time.
pub fn builtin_get_time() -> PortableDynamicTool {
    PortableDynamicTool::new(
        "get_time",
        "Get the current local date and time in RFC 3339 format.",
        json!({
            "type": "object",
            "properties": {}
        }),
        |_args: serde_json::Value| {
            Box::pin(async move {
                let now = chrono::Local::now().to_rfc3339();
                Ok(ToolOutput::text(now))
            })
        },
    )
}

/// Tool: echo the provided text back verbatim.
pub fn builtin_echo() -> PortableDynamicTool {
    PortableDynamicTool::new(
        "echo",
        "Echo the provided text back verbatim.",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo" }
            },
            "required": ["text"]
        }),
        |args: serde_json::Value| {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(ToolOutput::text(text))
            })
        },
    )
}
