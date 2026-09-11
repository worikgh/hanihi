use rmcp::ErrorData;
/// The `echo` tool.  A basic tool that returns the prompt unchanged
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Tool};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
pub(crate) fn new() -> Tool {
    Tool::new(
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
    )
}

pub(crate) fn call<'a>(
    request: CallToolRequestParams,
    _context: RequestContext<RoleServer>,
) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + 'a {
    let text = request
        .arguments
        .clone()
        .and_then(|mut args| args.remove("text"))
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_default();
    let ret = std::future::ready(Ok(CallToolResponse::from(CallToolResult::success(vec![
        ContentBlock::text(text),
    ]))));
    ret.clone()
}
