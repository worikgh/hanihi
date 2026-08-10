# agent-harness

A minimal tool-calling agent harness in Rust, built on:

- **rig** (`rig-core` 0.41) — OpenAI-compatible chat completions client, tool
  definitions, completion loop
- **rmcp** (3.1) — Model Context Protocol client: attach tools from MCP stdio
  servers
- **reedline** (0.49) — readline-style REPL

## Layout

```
crates/
├── agent-core/         # library: Agent (loop, history, dispatch), built-in tools, MCP client
├── agent-cli/          # binary: clap CLI + reedline REPL + --once mode
└── mcp-echo-server/    # binary: minimal MCP stdio server exposing one tool (demo)
```

## Run

```bash
# One-shot turn (also used for smoke tests)
./target/debug/agent-cli --once "What time is it? Use the get_time tool." \
    --api-key "$DEEPSEEK_API_KEY"

# Attach an MCP server and talk interactively
./target/debug/agent-cli --mcp-command "./target/debug/mcp-echo-server"

# REPL commands: /help /tools /clear /quit
```

Configuration: `--base-url` (default `https://api.deepseek.com/v1`), `--api-key`
(or `LLM_API_KEY`), `--model` (default `deepseek-chat`), `--mcp-command` (repeatable).

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

## Testing

- Unit tests use rig's `MockCompletionModel` (scripted turns) — no network.
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings`

## Known TODOs

- Tool name collisions: first registration wins (builtin `echo` shadows an MCP
  `echo`). Namespacing MCP tools is a future concern.
- No streaming, no durable execution/checkpointing, no evals yet — the natural
  next layers.
