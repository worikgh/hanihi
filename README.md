# hānihi

A minimal tool-calling agent harness in Rust — the core of a future coding
agent. Built on:

- **rig** (`rig-core` 0.41) — OpenAI-compatible chat completions client, tool
  definitions, completion loop
- **rmcp** (3.1) — Model Context Protocol client: attach tools from MCP stdio
  servers
- **reedline** (0.49) — readline-style REPL

Repository: <https://github.com/worikgh/hanihi>

## Layout

```
crates/
├── hānihi-core/        # library: Agent (loop, history, dispatch), built-in tools, MCP client
├── hānihi-cli/         # binary: clap CLI + reedline REPL + --once mode
└── mcp-echo-server/    # binary: minimal MCP stdio server (demo, one `mcp_echo` tool)
```

> Crate package names keep the macron (`hānihi-core`, `hānihi-cli`). The lib
> target and dependency key are ASCII (`hanihi_core`) because `rustc` requires
> ASCII identifiers for `--extern`.

## Name 

**`hānihi`** is a loan word from English into Māori and means ["harness"](https://maori_en_new.en-academic.com/2763/h%C4%81nihi)

## Features

- **Agent loop** — system preamble + persistent history + tool definitions,
  tool-call dispatch, `max_turns` guard (default 10), per-turn usage tracking
- **Built-in tools** — `get_time` (local RFC 3339 timestamp), `echo`
- **MCP client** — spawn an MCP stdio server, wrap each of its tools as an
  agent tool dispatching over `tools/call`
- **CLI** — interactive reedline REPL (`/help /tools /clear /quit`), `--once`
  one-shot mode for scripting and smoke tests, repeatable `--mcp-command`

## Run

```bash
# One-shot turn (also used for smoke tests)
cargo run -p hānihi-cli -- --once "What time is it? Use the get_time tool."

# Attach an MCP server and talk interactively
cargo run -p hānihi-cli -- --mcp-command "./target/debug/mcp-echo-server"

# REPL commands: /help /tools /clear /quit (or /exit)
```

Configuration — every flag has an environment variable:

| Flag | Env | Default |
|---|---|---|
| `--base-url` | `LLM_BASE_URL` | `https://api.deepseek.com/v1` |
| `--api-key` | `LLM_API_KEY` | — (required) |
| `--model` | `LLM_MODEL` | `deepseek-chat` |
| `--mcp-command CMD` | — | none (repeatable) |
| `--once PROMPT` | — | none |

## How it works

`Agent::run` is the loop:

1. Build a completion request: system preamble + persistent history + the new
   user input (passed as the request prompt, so it never duplicates) + tool
   definitions.
2. If the model replies with text only → turn complete, history committed.
3. If the model requests tool calls → record the assistant message, execute
   each tool (built-in or MCP), append results as `tool_result` messages,
   loop back to 1.
4. `max_turns` (default 10) guards runaway tool-call loops.

Tools are rig `PortableDynamicTool`s: name + description + JSON schema + an
async callback over raw `serde_json::Value`. MCP tools get wrapped into this
shape, dispatching over `tools/call` on the connected service.

The library is model-agnostic: `Agent<M: CompletionModel>` works with rig's
`MockCompletionModel` in tests and any OpenAI-compatible chat-completions
endpoint in production (see `connect_chat_model`).

## Status

- 7 unit tests (rig `MockCompletionModel`, scripted turns — no network),
  `cargo clippy --workspace --all-targets -- -D warnings` clean
- Smoke-tested against DeepSeek (`deepseek-chat`): `get_time` round trip ✔,
  MCP `mcp_echo` round trip ✔
- History is in-memory only; no streaming, no durable execution, no evals yet
  (see TODOs)

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Known TODOs

- Tool name collisions: first registration wins (builtin `echo` shadows an MCP
  `echo`). Namespacing MCP tools is a future concern.
- No streaming, no durable execution/checkpointing, no evals yet — the natural
  next layers.
