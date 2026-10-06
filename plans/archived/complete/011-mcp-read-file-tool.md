# Plan: expose `read_file` through the MCP server

Status: **plan only** — this document is the only deliverable of this task.
No source code is changed here.

## Goal

Add a `read_file` tool to the MCP stdio server in
`crates/hanihi-mcp-server`, mirroring the behavior of
`hanihi_core::tool::builtin_read_file`, and wire it into the server's
`list_tools` / `call_tool` handler methods.

## Non-goals

- Keep `builtin_read_file` and every other built-in tool exactly as-is.
- Make no changes to `hanihi-core`.
- Only touch the MCP server crate: one new module plus the scaffolding in
  `main.rs`.
- Do not remove or rename the existing `mcp_echo` tool.

## Current shape

- `crates/hanihi-mcp-server/src/main.rs` is a minimal stdio server whose
  `McpServer` (a unit struct) implements `rmcp::handler::server::ServerHandler`
  with `list_tools` returning only `echo_tool::new()` and `call_tool`
  dispatching only to `echo_tool::call`.
- `crates/hanihi-mcp-server/src/echo_tool.rs` is the house pattern for a
  hand-written tool module: `pub(crate) fn new() -> Tool` (schema) and
  `pub(crate) fn call(...) -> impl Future + MaybeSendFuture`.
- `hanihi_core::tool::builtin_read_file(Arc<SourceTree>)` reads a repo-relative
  text file through `SourceTree::read`, honoring ignore rules and the 64 KiB
  cap, and maps `SourceError` onto rig tool errors.
- `hanihi_core::McpClient::tool_defs()` (`crates/hanihi-core/src/mcp.rs:37`)
  calls the server's `list_tools` and wraps each returned `Tool`; call
  results with `is_error == Some(true)` are surfaced to the agent as tool
  errors.

## rmcp API facts confirmed against `target/doc` (rmcp 3.3.0, matching `Cargo.lock`)

- `Tool::new<N, D, S>(name, description, input_schema)` with
  `S: Into<Arc<JsonObject>>`; `echo_tool::new` already proves the
  `serde_json::json!({...}).as_object().expect(...).clone()` pattern compiles.
- `ListToolsResult::with_all_items(Vec<Tool>)`.
- `CallToolRequestParams` has `name: Cow<'static, str>` and
  `arguments: Option<JsonObject>`; `CallToolRequestParams::new(name).with_arguments(...)`
  is used by `McpClient`.
- `CallToolResponse` is a non-exhaustive enum (`Complete` / `InputRequired` /
  `Task`) with `impl From<CallToolResult>`.
- `CallToolResult::success(Vec<ContentBlock>)` and
  `CallToolResult::error(Vec<ContentBlock>)` (sets `is_error = Some(true)`);
  `ContentBlock::text(...)`.
- `ErrorData::invalid_params(msg, None)`, `ErrorData::internal_error(msg, None)`,
  and `ErrorData::new(ErrorCode::METHOD_NOT_FOUND, msg, None)`; `ErrorCode` is
  `rmcp::model::ErrorCode`.
- `ServerHandler` methods return
  `impl Future<Output = Result<...>> + MaybeSendFuture + '_`
  (`rmcp::service::MaybeSendFuture`).
- `ServerInfo` is a type alias for `InitializeResult`
  (`target/doc/rmcp/model/type.ServerInfo.html`). See the
  "ServerInfo / capabilities" section below.

## Decisions

### 1. Tool name: `mcp_read_file`, not `read_file`

Follow the crate's existing convention (`mcp_echo`) and avoid a name
collision: the CLI always registers the built-in `read_file`, and if the MCP
server also advertised `read_file`, attaching it would produce a duplicate
tool name. `mcp_read_file` keeps the two unambiguous. If name parity is ever
required instead, this is a one-constant change.

Description and JSON schema mirror `builtin_read_file` verbatim:

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path relative to the repo root" }
  },
  "required": ["path"]
}
```

### 2. Source tree ownership: inject `Arc<SourceTree>` at startup

`McpServer` becomes a struct holding `tree: Option<Arc<SourceTree>>`, opened
once in `main()` via `SourceTree::open()`. Reasons:

- Mirrors `builtin_read_file(Arc<SourceTree>)` exactly.
- Opens once at startup (`.ignore` bootstrap + matcher build happen once, the
  same way the CLI opens its tree once), rather than per call.
- Makes the tool testable: the read path takes the tree as a parameter, so a
  test can pass a fixture tree built with `SourceTree::open_at`.
- `Option` preserves today's behavior when the process is not inside a git
  repo: `read_file` is simply not advertised, while `mcp_echo` keeps working.
  `#[derive(Debug, Clone, Default)]` continues to work unchanged, because
  `Option<Arc<SourceTree>>` is `Debug + Clone + Default` (`Default` = `None`,
  i.e. echo-only).

`SourceTree` is `Send + Sync` in practice: `hanihi-core` already moves
`Arc<SourceTree>` into `PortableDynamicTool` closures that must be `Send`
(and `McpServer` must satisfy `ServerHandler: Send + Sync`). `Arc<T>` is
`Send + Sync` iff `T: Send + Sync`, so this holds.

The server inherits the parent's cwd (the CLI's `TokioChildProcess::new` does
not set `current_dir`), so `SourceTree::open()` resolves the same repo the
harness pins its cwd to. If launched manually with another cwd, `read_file`
is disabled and echo still works — the `Option` design covers this.

### 3. Error mapping

Two distinct failure kinds, matching rmcp semantics:

| case | MCP shape |
|---|---|
| missing / non-string `path` | `Err(ErrorData::invalid_params(...))` — malformed request |
| `SourceError::NotFound` | `Ok(CallToolResult::error(...))` — caller-visible tool error |
| `SourceError::Ignored` | `Ok(CallToolResult::error(...))` — caller-visible tool error |
| `SourceError::Escape` | `Ok(CallToolResult::error(...))` — caller-visible tool error |
| tree unavailable (`None`) | `Ok(CallToolResult::error(...))` — caller-visible tool error |
| `SourceError::Io` / `SourceError::Ignore` | `Err(ErrorData::internal_error(...))` — server infrastructure |
| unknown tool name | `Err(ErrorData::new(ErrorCode::METHOD_NOT_FOUND, ...))` |

Caller-visible messages match `builtin_read_file` / `map_source_err`:

- `no such path: <path>`
- `path is git-ignored: <path>`
- `path escapes the repository: <path>`

`CallToolResult::error` sets `is_error = Some(true)`, so `McpClient` turns it
into a rig tool error for the agent — the MCP equivalent of the builtin's
`ToolExecutionError`.

### 4. ServerInfo / capabilities (`get_info`)

The user pointed at `ServerInfo.html`; the relevant finding is that **no
`get_info` change is required for this task's goal**:

- `ServerHandler::get_info` is a provided method returning
  `ServerInfo::default()` (confirmed in `trait.ServerHandler.html`).
- `ServerInfo` is `InitializeResult`; `ServerCapabilities::default()` leaves
  `tools: None`, so the default server does **not** advertise the tools
  capability.
- `hanihi_core::McpClient` ignores that advertisement and calls `list_tools`
  directly, which is why the existing echo tool already works end-to-end.

Leave `get_info` untouched to keep the change minimal. If third-party,
strictly capability-gated clients need to be supported later, add the
doc-confirmed override as a follow-up:

```rust
fn get_info(&self) -> ServerInfo {
    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
}
```

(`ServerCapabilities::builder().enable_tools().build()` and
`ServerInfo::new(ServerCapabilities) -> Self` are both confirmed in
`target/doc/rmcp/model/struct.ServerCapabilities.html` and
`struct.InitializeResult.html`.)

### 5. `call_tool` dispatch and future-type unification

`call_tool` returns `impl Future`, and `echo_tool::call` / `read_file_tool::call`
return **different opaque future types** — they cannot be the two arms of a
`match` directly. Unify with a single `async move` block that awaits each
tool's future:

```rust
async move {
    match name.as_str() {
        "mcp_echo" => echo_tool::call(request, context).await,
        "mcp_read_file" => match tree { ... }.await,  // see sketch
        other => Err(ErrorData::new(ErrorCode::METHOD_NOT_FOUND, format!("unknown tool '{other}'"), None)),
    }
}
```

Clone `request.name` to a `String` before the block so the arms can move
`request` freely (no borrow of `request` lingers into the match).

## Changes

### New file: `crates/hanihi-mcp-server/src/read_file_tool.rs`

```rust
//! The `read_file` tool, served over MCP. Mirrors
//! `hanihi_core::tool::builtin_read_file`.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use hanihi_core::{SourceError, SourceTree};
use rmcp::ErrorData;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Tool};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};

pub(crate) fn new() -> Tool {
    Tool::new(
        "mcp_read_file",
        "Read a text file from the git repository. `path` is relative to the repo root. \
         Git-ignored paths cannot be read. Returns up to 64 KiB.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the repo root" }
            },
            "required": ["path"]
        })
        .as_object()
        .expect("static schema is an object")
        .clone(),
    )
}

pub(crate) fn call(
    request: CallToolRequestParams,
    tree: Arc<SourceTree>,
    _context: RequestContext<RoleServer>,
) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
    async move {
        let args = request.arguments.unwrap_or_default();
        let rel = path_arg(&args)?;
        read(tree, rel)
    }
}

/// Caller-visible result for when the server was started outside a git repo.
pub(crate) fn unavailable() -> CallToolResponse {
    tool_error("read_file unavailable: no git repository".to_string())
}

fn path_arg(args: &serde_json::Map<String, serde_json::Value>) -> Result<&str, ErrorData> {
    args.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorData::invalid_params("missing string field 'path'", None))
}

/// Reads one file. Tool-level failures are `Ok(CallToolResult::error(...))`;
/// infrastructure failures are `Err(ErrorData)`.
fn read(tree: Arc<SourceTree>, rel: &str) -> Result<CallToolResponse, ErrorData> {
    match tree.read(Path::new(rel)) {
        Ok(text) => Ok(CallToolResponse::from(CallToolResult::success(vec![
            ContentBlock::text(text),
        ]))),
        Err(SourceError::NotFound(p)) => {
            Ok(tool_error(format!("no such path: {}", p.display())))
        }
        Err(SourceError::Ignored(p)) => {
            Ok(tool_error(format!("path is git-ignored: {}", p.display())))
        }
        Err(SourceError::Escape(p)) => {
            Ok(tool_error(format!("path escapes the repository: {}", p.display())))
        }
        Err(e) => Err(ErrorData::internal_error(format!("reading {rel}: {e}"), None)),
    }
}

fn tool_error(message: String) -> CallToolResponse {
    CallToolResponse::from(CallToolResult::error(vec![ContentBlock::text(message)]))
}
```

`path_arg` and `read` are plain functions so they can be unit-tested without
constructing a `RequestContext<RoleServer>`.

### `crates/hanihi-mcp-server/src/main.rs`

```rust
mod echo_tool;
mod read_file_tool;

use std::sync::Arc;

use hanihi_core::{SourceTree, debug};
use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ListToolsResult,
    PaginatedRequestParams,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer, ServiceExt};
use rmcp::transport;
use std::future::Future;

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
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        debug::log_to_file("mcp-serve call_tool: name", &request.name);
        let name = request.name.to_string();
        let tree = self.tree.clone();

        async move {
            match name.as_str() {
                "mcp_echo" => echo_tool::call(request, _context).await,
                "mcp_read_file" => match tree {
                    Some(tree) => read_file_tool::call(request, tree, _context).await,
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
```

`main()` opens the tree once before serving; stdout stays reserved for the
protocol, so the diagnostic goes to stderr:

```rust
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
```

The `ServerError` enum and its `From` impls remain unchanged.

## Tests

Unit tests live in `read_file_tool.rs` under `#[cfg(test)] mod tests`. There
is no `hanihi-core` fixture reuse (`source::testutil` is `pub(crate)`), so the
test module creates a throwaway repo the same way `tool.rs` tests do:
`create_dir_all(".git")`, write `.gitignore` (`target/`), `Cargo.toml`, and
`src/main.rs`, then `SourceTree::open_at(&dir)`.

Cover:

1. `new()` reports name `mcp_read_file`, the same description/schema shape,
   and `required: ["path"]`.
2. `path_arg` returns the string for `{"path": "src/main.rs"}` and
   `Err` (INVALID_PARAMS) for `{}` and `{"path": 42}`.
3. `read` on an existing file returns `Complete(success(...))` whose text
   contains the file contents.
4. `read` on a missing path returns an error result (`is_error == Some(true)`)
   whose text contains `no such path:`.
5. `read` on `target/...` returns an error result containing `git-ignored`.
6. `read` on a `../outside` path returns an error result containing
   `escapes`.
7. `unavailable()` returns an error result mentioning "no git repository".

`CallToolResponse` is non-exhaustive, so tests match
`CallToolResponse::Complete(result)` with a wildcard `_` arm.

## Verification

```text
cargo fmt
cargo check -p hanihi-mcp-server
cargo test -p hanihi-mcp-server
cargo clippy -p hanihi-mcp-server --all-targets -- -D warnings
```

Optionally exercise end-to-end after building:

```text
hanihi-cli --mcp-command ./target/debug/hanihi-mcp-server
```

and confirm the agent's tool list includes `mcp_read_file`, that reading a
tracked file works, and that an ignored/escaping path surfaces as a tool
error.

## Assumptions and risks

- **Name** `mcp_read_file` is chosen for collision avoidance; if exact name
  parity with the builtin is wanted, change one constant (but then attaching
  the server to the CLI risks a duplicate `read_file`).
- **cwd** — the server must be spawned with its cwd inside the target repo
  for `SourceTree::open()` to find it. The CLI already does this. Outside a
  repo, `read_file` is gracefully disabled and echo still works.
- **No capability advertisement change** — `get_info` stays default. The
  hanihi harness client does not gate on `capabilities.tools`; a
  spec-strict third-party client would need the optional `get_info` override
  noted above.
- **`SourceTree: Send + Sync`** — inferred from `hanihi-core` already moving
  `Arc<SourceTree>` into `Send` tool closures; verify at compile time with
  `cargo check`.
