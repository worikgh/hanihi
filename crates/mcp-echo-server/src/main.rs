//! Minimal MCP stdio server exposing one `echo` tool.
//!
//! Used to exercise the harness end-to-end:
//! `hanihi-cli --mcp-command ./target/debug/mcp-echo-server`

use hanihi_core::debug;
use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer, ServiceExt};
use rmcp::transport;
use std::future::Future;
mod echo_tool;
/// Echo server: replies with the `text` argument verbatim.
#[derive(Debug, Clone, Default)]
struct McpServer;

impl ServerHandler for McpServer {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![echo_tool::new()])))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        debug::log_to_file("mcp-serve call_tool: name", &request.name);
        let request = request.clone();
        echo_tool::call(request, _context)
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
    let service = McpServer.serve(transport::stdio()).await?;
    let _reason = service.waiting().await?;
    Ok(())
}
