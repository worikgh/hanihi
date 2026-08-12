# Plan 004 — Streaming

**Status:** in progress | **Created:** 2026-08-12 | **Scope:** core + CLI

## Overview

Stream model output token-by-token so the user sees the model "typing" in
real time instead of waiting for the full response. Uses rig 0.41's
`CompletionModel::stream()` which returns a `StreamingCompletionResponse`
implementing `futures::Stream<Item = StreamedAssistantContent>`.

---

## Architecture

### `StreamEvent` enum (in `agent.rs`)

Events emitted during a streaming agent turn, sent through a
`tokio::sync::mpsc` channel:

```rust
pub enum StreamEvent {
    TextDelta { text: String },
    ToolCallStart { id: String, name: String },
    ToolCallArgs { id: String, args_delta: String },
    ToolCallReady { id: String, name: String, arguments: serde_json::Value },
    ToolResult { id: String, name: String, result_preview: String },
    TurnComplete { summary: TurnSummary },
    Error { message: String },
}
```

### `Agent` changes

`tools` changes from `Vec<PortableDynamicTool>` to `Arc<Vec<PortableDynamicTool>>`.
`add_tool` pushes through `Arc::make_mut`. This allows cloning the tool
registry into a spawned task.

`Agent::run_streaming()`:

1. Build the completion request (same as `run()`)
2. Call `self.model.stream(request).await` → `StreamingCompletionResponse`
3. Spawn an async task that:
   a. Iterates `stream.next()` (the Stream impl on StreamingCompletionResponse)
   b. For `Text(t)` → sends `TextDelta`
   c. For `ToolCallDelta { Name(name) }` → sends `ToolCallStart`
   d. For `ToolCallDelta { Arguments(args) }` → sends `ToolCallArgs`
   e. For `ToolCall { tool_call }` → sends `ToolCallReady`, then executes the tool using the cloned tools Arc, sends `ToolResult`
   f. For `Final(r)` → captures usage
   g. On stream exhaustion → sends `TurnComplete` or `Error`
4. Returns `mpsc::Receiver<StreamEvent>`

### `Session::run_streaming()`

Same wrapper pattern as `Session::run()`: reads from the agent's streaming
receiver, writes log entries after each model call/tool execution, and
forwards events to the caller through its own channel.

The log entries for `llm_response` are written after the stream exhausts
(all deltas received and accumulated), not per-chunk.

### CLI changes

In `--once` mode: reads from the stream channel, prints text deltas as they
arrive, shows tool call progress.

In REPL mode: `tokio::select!` between user input and stream events.

---

## What does NOT change

- `Agent::run()` stays synchronous. Streaming is additive.
- The log format. `llm_response` entries are still written after model
  response is complete, not per-token.
- Tool execution. Tools still synchronously execute between model calls.

---

## Implementation order

### Step 1 — Agent: Arc tools + StreamEvent + run_streaming
### Step 2 — Session: run_streaming wrapper
### Step 3 — CLI: streaming REPL + --once
### Step 4 — Tests, build, clippy, smoke
