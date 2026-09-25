//! `hanihi-mcp-server-rw` binary: the MCP server with file-modifying tools.

fn main() -> std::io::Result<()> {
    hanihi_mcp_server::run_write_server()
}
