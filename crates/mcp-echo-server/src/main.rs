//! Minimal MCP stdio server exposing one `echo` tool.
//!
//! Used to exercise the harness end-to-end:
//! `agent-cli --mcp-command ./target/debug/mcp-echo-server`

use std::future::Future;

use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer, ServiceExt};
use rmcp::transport;

/// Echo server: replies with the `text` argument verbatim.
#[derive(Debug, Clone, Default)]
struct EchoServer;

impl ServerHandler for EchoServer {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "mcp_echo",
            "Echo the provided text back verbatim. Served over MCP.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo" }
                },
                "required": ["text"]
            })
            .as_object()
            .expect("static schema is an object")
            .clone(),
        )])))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        let text = request
            .arguments
            .and_then(|mut args| args.remove("text"))
            .and_then(|value| value.as_str().map(String::from))
            .unwrap_or_default();
        std::future::ready(Ok(CallToolResponse::from(CallToolResult::success(vec![
            ContentBlock::text(text),
        ]))))
    }
}

/// Errors from the echo server binary.
#[derive(Debug)]
enum ServerError {
    /// Protocol initialization failed.
    Init(Box<rmcp::service::ServerInitializeError>),
    /// The service task failed.
    Join(Box<tokio::task::JoinError>),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::Init(e) => write!(f, "server init failed: {e}"),
            ServerError::Join(e) => write!(f, "server task failed: {e}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<rmcp::service::ServerInitializeError> for ServerError {
    fn from(e: rmcp::service::ServerInitializeError) -> Self {
        ServerError::Init(Box::new(e))
    }
}

impl From<tokio::task::JoinError> for ServerError {
    fn from(e: tokio::task::JoinError) -> Self {
        ServerError::Join(Box::new(e))
    }
}

#[tokio::main]
async fn main() -> Result<(), ServerError> {
    let service = EchoServer.serve(transport::stdio()).await?;
    let _reason = service.waiting().await?;
    Ok(())
}
