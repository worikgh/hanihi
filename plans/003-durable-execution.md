# Plan 003 — Durable Execution

**Status:** in progress | **Created:** 2026-08-12 | **Scope:** core + CLI

## Overview

Replay the session event log to rebuild the agent's message history on open.
When you quit hānihi and reopen a session, the agent picks up where it left
off — it sees the full conversation history from prior turns.

No new storage. No schema migration beyond adding one field to
`ToolExecutionData`. The event log already contains everything needed.

---

## Architecture

### `Session::replay_history() -> Result<Vec<Message>, SessionError>`

Scans `events.jsonl` line-by-line, parses each `LogEntry`, and reconstructs
`rig::completion::Message` values for completed turns. Stops at the last
`turn_complete` or `error` — partial turns (events after last complete turn,
no close) are skipped.

**Algorithm:**

```
for each entry in log:
  match entry:
    user_input(text, turn=N):
      messages.push(Message::user(text))
    
    llm_response(text=Some(t), tool_calls=None):
      messages.push(Message::assistant(t))
    
    llm_response(tool_calls=Some(tcs)):
      messages.push(Message::Assistant {
        id: response.message_id,
        content: [ToolCall { id: tc.id, call_id: tc.id, function: tc.function } for tc in tcs]
      })
    
    tool_execution(tool_call_id, call_id, name, args, result):
      messages.push(Message::tool_result_with_call_id(tool_call_id, Some(call_id), result))
    
    error / turn_complete:
      // End of a completed turn — keep all messages accumulated so far.
      // Continue to next turn.
    
    session_created / session_opened / session_closed / llm_prompt:
      // Skip — not needed for history reconstruction.
```

**Partial turn handling:** If the log ends with a `turn_complete` or `error`,
all messages up to that entry belong to completed turns. If the log ends
without a `turn_complete` (e.g. crash mid-turn), we stop at the last complete
turn boundary — i.e. roll back to the last `turn_complete` or `error`. The
simplest implementation: track a "last safe position" at each `turn_complete`
/ `error`, and keep a rolling `Vec<Message>` that resets when we know a full
turn is committed.

Actually, the simplest correct approach: accumulate into a single
`Vec<Message>` and always truncate back to the last `turn_complete` or
`error` boundary. Track the index just after each complete turn.

### Schema addition: `ToolExecutionData.call_id`

Currently `ToolExecutionData` stores `tool_call_id` (the rig `ToolCall.id`)
but not `ToolCall.call_id`, which is needed for `tool_result_with_call_id`.
Add the field:

```rust
pub struct ToolExecutionData {
    pub tool_call_id: String,
    pub call_id: String,      // NEW
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: String,
}
```

Old logs without `call_id` are handled gracefully: replay falls back to using
`tool_call_id` as `call_id`.

### `Agent::set_history(&mut self, history: Vec<Message>)`

A public setter so the CLI can seed replayed history into a freshly-built
agent:

```rust
pub fn set_history(&mut self, history: Vec<Message>) {
    self.history = history;
}
```

### CLI integration

After opening the session and building the agent with all tools registered,
replay history:

```rust
// After agent is fully built (tools registered):
let history = session.replay_history()
    .map_err(|e| AgentError::Rig(e.to_string()))?;
if !history.is_empty() {
    agent.set_history(history);
    println!("replayed {} messages from prior session", agent.history().len());
}
```

This must happen AFTER tools are registered (so the agent is fully built)
but BEFORE the REPL or `--once` starts.

### What this enables

- **Resume conversations across restarts** — close hānihi, come back tomorrow,
  pick up where you left off
- **Multi-session workflows** — work on different things in different
  sessions, each with persistent context
- **Agent grows context over time** — the model sees the full conversation
  each time

### Edge cases

- **Tool availability mismatch:** prior session used an MCP server that isn't
  running now. Tool results are in the log, so the model can see them. If the
  user asks a follow-up that requires that tool, the agent correctly reports
  it as unavailable.
- **Large histories:** a session with many turns grows the message history
  proportionally. The model's context window is the limit. No truncation in
  this plan — start with full replay.
- **`/clear` behavior:** clears in-memory history only, does NOT truncate the
  log. Next open replays from the full log. This is correct — the log is an
  append-only record.

---

## Implementation order

### Step 1 — Schema: add `call_id` to `ToolExecutionData`
- Add field, update `LogEntry::tool_execution` constructor
- Update `Session::run` to pass `call.call_id`
- Use `#[serde(default)]` with fallback for old logs

### Step 2 — `Session::replay_history()`
- Parse `events.jsonl`, reconstruct `Vec<Message>`
- Handle partial turns (truncate to last complete turn boundary)
- Handle old logs without `call_id`

### Step 3 — `Agent::set_history()`
- One-line public setter

### Step 4 — CLI integration
- After agent build, replay + seed
- Print message count on replay

### Step 5 — Tests
- Unit test: replay from a hand-crafted log
- Unit test: replay with tool calls
- Unit test: partial turn truncation
- Unit test: old-log fallback (missing call_id)

### Step 6 — Build, clippy, smoke test
