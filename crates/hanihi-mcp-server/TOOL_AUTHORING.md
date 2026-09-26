# Authoring a New Hānihi Tool

This document is the complete, self-contained spec for adding a tool to this
`hanihi-mcp-server` crate. When asked to create a new tool, follow only this document plus
the two reference files listed at the end. Do not re-derive the conventions
from the rest of the crate.

If you are not in that crate stop immediately

## Crate overview

- Crate `hanihi-mcp-server`, Rust edition 2024, with one library target and
  two binaries.
- Purpose: implement MCP tools consumed by the Hānihi LLM harness in this crate.
- The two binaries share a single server implementation (`src/lib.rs`) but
  expose disjoint tool sets:
  - `hanihi-mcp-server-ro` — tools that are non-destructive: they do not
    change the repository in any way.
  - `hanihi-mcp-server-rw` — tools that **can** change files.
- Dependencies: `serde` (features `derive`, `rc`) and `serde_json`. Add a new
  dependency only when a tool genuinely requires it, and keep it narrow.

## Tool categories

Every tool belongs to exactly one of two sets. The two sets must not overlap.

### Read-only tools

Served by `hanihi-mcp-server-ro`. A read-only tool:

- May read repository files, list directories, and run read-only commands
  (for example `cargo metadata` or `rustc -vV`).
- Must never create, modify, rename, or delete files.
- Must never run a command with side effects.

Examples: `echo`, `noop`, `search_text`, `find_symbol`, `workspace_info`.

### Read-write tools

Served by `hanihi-mcp-server-rw`. A read-write tool may modify repository
files. Keep these tools clearly destructive and carefully validated.

Example: `apply_patch`.

## How a tool is wired

- One tool = one module: `src/<name>_tool.rs`, module name `<name>_tool`,
  tool name `<name>`. Names are `snake_case`.
- `src/lib.rs` declares `mod <name>_tool;` and registers the tool in exactly
  one of the two registration functions:
  - `read_only_tools()` for non-destructive tools.
  - `write_tools()` for tools that may modify files.
- The two binaries are thin wrappers over the shared server loop:
  - `src/main.rs` (`hanihi-mcp-server-ro`) calls `run_read_only_server()`.
  - `src/bin/hanihi-mcp-server-rw.rs` calls `run_write_server()`.
- Tools never read `stdin` or write `stdout` directly. They return values.

## Required API

Every tool module MUST expose exactly these two functions:

```rust
pub(crate) fn json() -> serde_json::Value;
pub(crate) fn exec(params: &serde_json::Value, id: serde_json::Value) -> serde_json::Value;
```

### `json()` — the `tools/list` entry

Return one object with `name`, `description`, and `inputSchema`:

```json
{
  "name": "my_tool",
  "description": "One-line description of what the tool does.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "message": {
        "type": "string",
        "description": "The message to echo."
      }
    },
    "required": ["message"],
    "additionalProperties": false
  }
}
```

Conventions:

- `inputSchema` is always `"type": "object"` with `"additionalProperties":
  false`.
- List required argument names in `"required"`. Omit optional ones.
- Give every property a `description`.

### `exec()` — the `tools/call` handler

`params` is the whole MCP `tools/call` params object:

```json
{
  "name": "my_tool",
  "arguments": { "message": "hello" },
  "_meta": { "progressToken": 1 }
}
```

Read inputs from `params.get("arguments")`, never from the top level. `id` is
the JSON-RPC message id and may be a number or a string; echo it back verbatim
and never assume its type.

Return the full JSON-RPC 2.0 envelope.

Success:

```json
{
  "jsonrpc": "2.0",
  "id": "<echoed id>",
  "result": {
    "content": [ { "type": "text", "text": "<result text>" } ],
    "isError": false
  }
}
```

Error:

```json
{
  "jsonrpc": "2.0",
  "id": "<echoed id>",
  "error": {
    "code": -32602,
    "message": "<why it failed>"
  }
}
```

Error codes:

- `-32602` — invalid params: a required argument is missing or has the wrong
  type. Return this directly from `exec`.
- `-32603` — internal error: the tool ran but failed (I/O, process, parsing).
  Propagate build failures up as a `Result<..., String>` and map the `Err`
  variant to this code.

Use `serde_json::json!` to build both the `tools/list` entry and the response
envelopes. Define small private helpers for the two envelopes (see
`workspace_info_tool.rs`) rather than repeating the JSON literals.

## Step-by-step recipe

1. Create `src/<name>_tool.rs` with a module-level doc comment.
2. Decide which set the tool belongs to: read-only unless it modifies files.
3. Implement `json()` with the `tools/list` entry.
4. Implement `exec(params: &Value, id: Value) -> Value`:
   - Parse `params.get("arguments")`; return `-32602` for missing/invalid
     required inputs.
   - Do the work in a private `fn build_result(...) -> Result<T, String>`
     helper (or helpers). Use `?` for propagation; never `unwrap`/`expect` on
     fallible operations.
   - Map `Ok` to the success envelope and `Err` to the `-32603` envelope.
5. Register the module: add `mod <name>_tool;` to `src/lib.rs` and add a
   `Tool` entry (`name`, `json`, `exec`) to either `read_only_tools()` or
   `write_tools()`, depending on the category chosen in step 2. Register the
   tool in only one set.
6. Add unit tests in the same file under `#[cfg(test)] mod tests`. Test pure
   helpers (argument parsing, formatting) and boundary/empty/malformed-input
   cases. Do not write tests that depend on the current directory, network,
   or filesystem layout.
7. Run `cargo fmt`, `cargo check`, `cargo clippy -- -D warnings`, and
   `cargo test`.

## Conventions checklist

- File name ends in `_tool.rs`; module name matches; tool name is the
  snake_case prefix.
- Registered in exactly one tool set: `read_only_tools()` or `write_tools()`,
  never both.
- Read-only tools perform no writes and run no side-effecting commands.
- `pub(crate)` for the two entry points; everything else private.
- Use explicit `use` imports; no wildcard imports.
- Prefer `Result<T, String>` inside a tool for recoverable failures. Never
  panic for expected runtime failures.
- Keep functions small; extract domain values into named constants where
  meaningful.
- Return useful, human-readable error messages (no secrets, no raw backtraces).
- Do not read `target/` or files under `.git/`. Repo read policy is enforced
  by the harness, not by the tool.

## Worked example

A minimal read-only tool with one required argument (`reverse`). This is
illustrative; adapt the structure rather than copying it verbatim. A
read-write tool follows the same shape but is registered in `write_tools()`.

```rust
//! `reverse` tool for Hānihi: reverses the supplied text.

use serde_json::{Value, json};

pub(crate) fn json() -> Value {
    json!({
        "name": "reverse",
        "description": "Returns the supplied message with its characters reversed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to reverse."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        }
    })
}

pub(crate) fn exec(params: &Value, id: Value) -> Value {
    let message = params
        .get("arguments")
        .and_then(|arguments| arguments.get("message"))
        .and_then(Value::as_str);

    match message {
        Some(text) => success(id, reverse(text)),
        None => invalid_params(id, "Missing required string argument: message"),
    }
}

fn reverse(text: &str) -> String {
    text.chars().rev().collect()
}

fn success(id: Value, text: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }],
            "isError": false
        }
    })
}

fn invalid_params(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32602, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_reorders_characters() {
        assert_eq!(reverse("abc"), "cba");
    }

    #[test]
    fn reverse_preserves_unicode() {
        assert_eq!(reverse("hānihi"), "ihināh");
    }
}
```

## Reference files

- `src/echo_tool.rs` — minimal tool: one required argument, success + `-32602`
  error paths.
- `src/workspace_info_tool.rs` — richer tool: `Result<Value, String>` builder,
  `-32603` error path, and a full `#[cfg(test)] mod tests` block.
- `src/lib.rs` — tool module declarations and the two registration functions
  (`read_only_tools()` and `write_tools()`).
- `src/main.rs` — the `hanihi-mcp-server-ro` binary wrapper.
- `src/bin/hanihi-mcp-server-rw.rs` — the `hanihi-mcp-server-rw` binary
  wrapper.
- `src/tool.rs` — a typed `Tool` struct. It is currently unused scaffolding;
  tools return raw JSON via `json!` and must not depend on it.
- `Cargo.toml` — crate metadata, dependency list, and the two `[[bin]]`
  targets.
