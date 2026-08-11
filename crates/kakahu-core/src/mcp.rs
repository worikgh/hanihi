//! MCP client support: attach tools from Model Context Protocol servers.
//!
//! [`McpClient`] spawns an MCP server as a child process (stdio transport),
//! lists its tools, and exposes each one to the agent as a rig
//! [`PortableDynamicTool`] whose callback dispatches over the MCP protocol.

use std::sync::Arc;

use rig::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::serve_client;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;

use crate::error::AgentError;

/// A connected MCP server (stdio transport).
pub struct McpClient {
    service: Arc<RunningService<RoleClient, ()>>,
}

impl McpClient {
    /// Spawn `command` with `args` as an MCP stdio server and initialize the
    /// protocol handshake.
    pub async fn connect_stdio(command: &str, args: &[String]) -> Result<Self, AgentError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        let transport = TokioChildProcess::new(cmd)?;
        let service = serve_client((), transport).await?;
        Ok(Self {
            service: Arc::new(service),
        })
    }

    /// List the server's tools and wrap each as a rig [`PortableDynamicTool`].
    pub async fn tool_defs(&self) -> Result<Vec<PortableDynamicTool>, AgentError> {
        let result = self.service.list_tools(None).await?;
        Ok(result
            .tools
            .into_iter()
            .map(|tool| self.wrap_tool(tool))
            .collect())
    }

    /// Wrap one MCP tool definition in a rig dynamic tool that dispatches via
    /// `tools/call` on the connected service.
    fn wrap_tool(&self, tool: rmcp::model::Tool) -> PortableDynamicTool {
        let name = tool.name.to_string();
        let description = tool
            .description
            .as_deref()
            .unwrap_or("MCP tool")
            .to_string();
        let schema = serde_json::to_value((*tool.input_schema).clone())
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
        let service = Arc::clone(&self.service);
        let call_name = name.clone();

        PortableDynamicTool::new(name, description, schema, move |args: serde_json::Value| {
            let service = Arc::clone(&service);
            let call_name = call_name.clone();
            Box::pin(async move {
                let params = CallToolRequestParams::new(call_name.clone())
                    .with_arguments(args.as_object().cloned().unwrap_or_default());
                let result = service
                    .call_tool(params)
                    .await
                    .map_err(ToolExecutionError::from_error)?;
                if result.is_error == Some(true) {
                    return Err(ToolExecutionError::from_error(AgentError::Tool {
                        name: call_name,
                        message: render_call_result(&result),
                    }));
                }
                Ok(ToolOutput::text(render_call_result(&result)))
            })
        })
    }
}

/// Render an MCP call result as plain text: text content blocks joined, plus
/// structured content serialized as JSON when present.
fn render_call_result(result: &CallToolResult) -> String {
    // Structured content is the canonical result when present.
    if let Some(structured) = &result.structured_content {
        return structured.to_string();
    }
    let parts: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        String::from("(no output)")
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_call_result_text() {
        let result = CallToolResult::success(vec![ContentBlock::text("hello")]);
        assert_eq!(render_call_result(&result), "hello");
    }

    #[test]
    fn test_render_call_result_structured() {
        let result = CallToolResult::structured(json!({"ok": true}));
        assert_eq!(render_call_result(&result), "{\"ok\":true}");
    }

    #[test]
    fn test_render_call_result_empty() {
        let result = CallToolResult::success(vec![]);
        assert_eq!(render_call_result(&result), "(no output)");
    }
}
