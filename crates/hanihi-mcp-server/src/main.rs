//! Minimal MCP stdio server exposing `echo` and `read_file` tools.
//!
//! Used to exercise the harness end-to-end:
//! `hanihi-cli --mcp-command ./target/debug/hanihi-mcp-server`

use hanihi_core::debug;
use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ListToolsResult, PaginatedRequestParams,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer, ServiceExt};
use rmcp::transport;
use std::future::Future;
use std::sync::Arc;

use hanihi_core::SourceTree;

mod echo_tool;
mod read_file_tool;

/// MCP stdio server exposing `echo` and `read_file` tools.
#[derive(Debug, Clone, Default)]
struct McpServer {
    tree: Option<Arc<SourceTree>>,
}

impl McpServer {
    fn new(tree: Option<Arc<SourceTree>>) -> Self {
        Self { tree }
    }
}

impl ServerHandler for McpServer {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        let mut tools = vec![echo_tool::new()];
        if self.tree.is_some() {
            tools.push(read_file_tool::new());
        }
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        debug::log_to_file("mcp-serve call_tool: name", &request.name);
        let name = request.name.to_string();
        let tree = self.tree.clone();

        async move {
            match name.as_str() {
                "mcp_echo" => echo_tool::call(request, context).await,
                "mcp_read_file" => match tree {
                    Some(tree) => read_file_tool::call(request, tree, context).await,
                    None => Ok(read_file_tool::unavailable()),
                },
                other => Err(ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool '{other}'"),
                    None,
                )),
            }
        }
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
    let tree = match SourceTree::open() {
        Ok(tree) => Some(Arc::new(tree)),
        Err(e) => {
            eprintln!("mcp_read_file disabled: {e}");
            None
        }
    };
    let service = McpServer::new(tree).serve(transport::stdio()).await?;
    let _reason = service.waiting().await?;
    Ok(())
}
