//! `hanihi-mcp-server-ro` binary: the MCP server with non-destructive tools.

fn main() -> std::io::Result<()> {
    hanihi_mcp_server::run_read_only_server()
}
