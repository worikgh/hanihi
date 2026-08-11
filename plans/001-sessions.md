# Plan 001 — Sessions

**Status:** draft | **Created:** 2026-08-11 | **Scope:** core + CLI

## Overview

Sessions give Hānihi persistent, named conversation containers. Every
interaction happens inside a session. Sessions are stored on disk with an
append-only structured event log that captures every prompt, every model
response, every tool call, and every tool result — all timestamped.

---

## Architecture

### New module: `hanihi_core::session`

```
crates/hanihi-core/src/session/
├── mod.rs          # Session, SessionManager, public API
├── log.rs          # LogEntry enum, serialization, LogWriter
└── lock.rs         # filesystem lock (prevent concurrent use)
```

### `SessionManager`

Owns the set of active sessions. Creates, opens, closes.

```rust
pub struct SessionManager {
    root: PathBuf,               // ~/.local/share/hanihi/sessions/
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn create(&mut self, name: &str, model: &str, system_prompt: &str) -> Result<&mut Session>;
    pub fn open(&mut self, name: &str) -> Result<&mut Session>;
    pub fn close(&mut self, name: &str) -> Result<()>;
    pub fn list(&self) -> Vec<String>;
    pub fn exists(&self, name: &str) -> bool;
}
```

### `Session`

Binds a name, an `Agent`, and a log writer together.

```rust
pub struct Session {
    pub name: String,
    pub id: Uuid,               // unique, stable across restarts
    pub created_at: DateTime<Utc>,
    pub model: String,
    root: PathBuf,              // ~/.local/share/hanihi/sessions/<name>/
    log: LogWriter,             // append-only writer on events.jsonl
    agent: Agent<…>,            // the agent bound to this session
}

impl Session {
    /// Run one user turn, logging everything.
    pub async fn run(&mut self, user_input: &str) -> Result<TurnSummary>;

    /// Tool inventory, history, clear, etc. — delegate to agent.
    pub fn tool_definitions(&self) -> …;
    pub fn history(&self) -> …;
    pub fn clear_history(&mut self);
}
```

`Session::run` wraps `Agent::run` and intercepts at the right points to log
each event. The `Agent` does not know about logging; all logging lives in
`Session`.

---

## Session storage layout

```
~/.local/share/hanihi/sessions/
├── default-session/
│   ├── session.json          # { id, name, created_at, model, system_prompt }
│   └── events.jsonl          # append-only structured log
├── my-project/
│   ├── session.json
│   └── events.jsonl
└── …
```

- `session.json` — written at creation, read at open. Contains only the
  static properties that do not change over the session's lifetime (id, name,
  created_at, model, system_prompt). Nothing derived.
- `events.jsonl` — one JSON object per line, append-only, never mutated.
- Filesystem lock: a `.lock` file (mandatory, fail-fast). Two processes must
  not write to the same session concurrently.

---

## Log entries (events.jsonl)

Format: JSON Lines. One complete JSON object per line, separated by newlines.
Each line is independently valid JSON and can be parsed without reading the
rest of the file.

Every entry has:

```json
{
  "ts": "2026-08-11T06:37:00.123456Z",
  "turn": 3,
  "kind": "<kind>",
  "data": { … }
}
```

`ts` is the UTC timestamp of the event. `turn` is the turn number within the
session (monotonically increasing, starts at 1).

### Event kinds

| kind | `data` fields | when |
|------|---------------|------|
| `session_created` | `session_id`, `name`, `model`, `system_prompt` | on `SessionManager::create` |
| `session_opened` | `session_id`, `name` | on `SessionManager::open` |
| `session_closed` | `session_id`, `name` | on `SessionManager::close` |
| `user_input` | `text` | start of each `Session::run` |
| `llm_prompt` | `messages` (the full `Vec<Message>` sent to the model), `tool_definitions` (the tool schemas exposed) | before each model API call |
| `llm_response` | `message_id`, `text`, `reasoning`, `tool_calls: [{id, name, arguments}]`, `usage: {input_tokens, output_tokens}` | after each model API call |
| `tool_execution` | `tool_call_id`, `name`, `arguments`, `result` | after each tool executes |
| `turn_complete` | `text` (final answer), `tool_calls` (count) | end of a successful `Session::run` |
| `error` | `turn`, `stage` (one of `"llm_call"` / `"tool_execution"`), `message` | on any failure during a turn |

### On `llm_prompt` and `llm_response`

These replace the previously-named `completion_request` / `completion_response`.
They are the raw prompt sent to the LLM API and the raw response received
from it.

- `llm_prompt` contains the assembled messages (preamble + history + current
  user message) and the tool definitions. Always logged in full — this is
  the canonical record of what was sent.
- `llm_response` contains the model's reply: text, reasoning content, tool
  calls (if any), and token usage. The current agent loop silently drops
  `AssistantContent::Reasoning(_)` — the log captures it explicitly. We don't
  feed reasoning back to the model, but we record it.

A single turn may produce multiple prompt/response pairs (model asks for a
tool → tool executes → model is called again with the tool result).

---

## Reasoning content

The current `Agent::run` matches on `AssistantContent` and ignores
`Reasoning(_)`. The `Session::run` wrapper must capture it before it is
discarded. This is a hard requirement — reasoning is part of "all data
received from the LLM."

---

## Session lifecycle events

Three events track the session's lifetime across processes:

- `session_created` — written once, when the session directory is first
  created.
- `session_opened` — written every time the session is opened (including the
  first time, immediately after `session_created`).
- `session_closed` — written when the process cleanly exits (flush + close).

Together these let you reconstruct usage patterns: when was a session active,
how many sessions did it span, etc.

---

## Error context

Every `error` event includes:

- `turn` — which turn was in progress when the error occurred.
- `stage` — what was happening: `"llm_call"` (the model returned an error) or
  `"tool_execution"` (a tool failed or was not found).
- `message` — the error text.

This is enough to pinpoint the failure without needing to scan surrounding
events. MaxTurns is logged as an `error` with stage `"llm_call"` and a
message like "exceeded maximum of 10 model turns."

---

## Derived properties

The following are NOT stored in the log. They are computed programmatically
from the raw events. The plan must ensure the log contains enough data to
compute them.

### Cumulative token usage

Sum `llm_response.data.usage.input_tokens` and
`llm_response.data.usage.output_tokens` across all events in the session.
Trivial O(n) scan or a cached counter maintained in memory during the
session.

### Model latency (per call)

`llm_response.ts - llm_prompt.ts` for each prompt/response pair. Requires
that the `ts` fields have sufficient precision (ISO 8601 with microseconds,
as shown in the example schema). This gives wall-clock latency for each model
call — useful for performance tuning.

### Session metadata (last_opened_at, total_turns)

- `last_opened_at` — the `ts` of the most recent `session_opened` event.
- `total_turns` — the `turn` field of the most recent `turn_complete` event
  (or 0 if none).

These can be computed by scanning the event log. For convenience, a
`Session::summary()` method can return them without requiring callers to
parse JSONL manually.

### session.json is static

`session.json` contains only the fields set at creation (`id`, `name`,
`created_at`, `model`, `system_prompt`). It is never updated. All derived
metadata lives in the event log or is computed from it.

---

## What is NOT logged

- **State mutations.** Clearing history is not an event. The log records what
  happened (prompts, responses, tool executions), not internal state changes.
- **Cumulative/derived data.** See above.

---

## CLI changes

### New args

```
--session <NAME>       Use an existing session (default: "default-session")
--new-session <NAME>   Create a new session with this name and use it
```

### Rules

1. `--session` defaults to `"default-session"`.
2. `--session` requires the session to exist. If it doesn't, error out with a
   helpful message.
3. `--new-session <NAME>`:
   - `NAME` must not be `"default-session"` (reserved — created automatically
     on first run with no `--new-session`).
   - `NAME` must not already exist.
   - Creates the session, writes `session.json`, then proceeds.
4. `--session` and `--new-session` are mutually exclusive.
5. On first-ever run with no args, `default-session` is auto-created.
6. Session root: `$XDG_DATA_HOME/hanihi/sessions/` (falls back to
   `~/.local/share/hanihi/sessions/`).

### REPL integration

The `hanihi-cli` REPL holds a `Session` instead of a bare `Agent`. Commands:

```
/help     — also shows current session name
/tools    — unchanged
/clear    — clears agent history (no log event)
/quit     — closes session (flushes log, writes session_closed)
/session  — print current session info (name, id, turn count)
```

---

## What does NOT change (yet)

- **No session-switching inside the REPL.** One session per process.
- **No session listing from the CLI.** Future feature.
- **No log replay / resumption of agent state.** The event log is a record,
  not a replay source. When you reopen a session, the agent starts with an
  empty history.
- **No multi-session concurrency.** One session per process.
- **`Agent` API stays unchanged.** `Agent` is unaware of sessions. `Session`
  wraps it and adds logging.

---

## Implementation order

### Step 1 — Storage foundation
- `LogEntry` enum with serde (all event kinds)
- `LogWriter` (append-only JSONL, flush on each write)
- `Session` struct (name, id, paths, log writer)
- `SessionManager` (create, open, close, exists, list)
- Mandatory filesystem lock
- Unit tests for create/open/close/lock/append

### Step 2 — Wire logging into `Session::run`
- Wrap `Agent::run` with log entries at each stage
- Capture reasoning content from `AssistantContent::Reasoning`
- Log `llm_prompt`, `llm_response`, `tool_execution`, `turn_complete`,
  `user_input`, `error`
- Session lifecycle entries (created, opened, closed)
- Turn counter (increment on each `Session::run`)

### Step 3 — CLI integration
- Add `--session` / `--new-session` clap args with validation
- Auto-create `default-session` on first run
- REPL uses `Session` instead of bare `Agent`
- `/session` command for current session info
- `/quit` closes session gracefully (flushes and writes `session_closed`)

### Step 4 — Derived property accessors
- `Session::cumulative_usage()` — scan log, sum token counts
- `Session::last_opened_at()` — from most recent `session_opened` event
- `Session::total_turns()` — from most recent `turn_complete` event
- `Session::latencies()` — compute per-call latency from prompt/response
  pairs
- Integration tests (smoke test with real LLM call + verify log output)

---

## Dependencies to add

| crate | purpose |
|-------|---------|
| `uuid` (with `v4` + `serde` features) | unique session IDs |
| `fs2` | cross-platform filesystem lock (or raw `libc::flock` since Linux is primary target) |
| `dirs` | XDG data directory |

`chrono` is already a dep of `hanihi-core`.
