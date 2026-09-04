# Logging System — Data Logged

## Overview

The system writes an append-only JSONL event log to
`<working-dir>/sessions/<name>/events.jsonl`. Each line is a complete,
independently parseable JSON object with this envelope:

| Field    | Meaning                                               |
|----------|-------------------------------------------------------|
| `schema` | Log schema version (currently `1`), injected on write |
| `kind`   | Event type (one of the nine below)                    |
| `ts`     | RFC 3339 UTC timestamp of the event                   |
| `turn`   | Monotonically increasing turn number for the session  |
| `data`   | Event-type-specific payload                           |

A per-session `session.json` holds static metadata, and process-level
`tracing` output (stderr) handles CLI diagnostics. The durable record of
interest is the JSONL event log.

## Event log entries

| Event kind | When written | Data logged |
|---|---|---|
| `session_created` | Session directory first created | `session_id`, `name`, `model`, `system_prompt` |
| `session_opened` | Session opened (including first open after creation) | `session_id`, `name` |
| `session_closed` | Session closed cleanly | `session_id`, `name` |
| `user_input` | Start of each turn | `text` (the raw user message) |
| `llm_prompt` | Before every model call | `provider`, `model`, `messages` (system prompt + history + in-turn messages + current user message), `tool_definitions` (tool schemas sent to the model) |
| `llm_response` | After every model response | `message_id` (optional), `text` (optional), `reasoning` (optional), `tool_calls` (optional array of `{id, name, arguments}`), `usage` = `{input_tokens, output_tokens}` |
| `tool_execution` | After every successful tool call | `tool_call_id`, `call_id`, `name`, `arguments` (JSON arguments passed), `result` (full rendered tool output) |
| `turn_complete` | Turn finished successfully | `text` (final assistant text), `tool_calls` (total tool calls executed that turn) |
| `error` | Error during a turn | `stage` (`llm_call` or `tool_execution`), `message` (error description) |

## Tool call details (nested in `llm_response`)

| Field | Meaning |
|---|---|
| `id` | Tool call identifier |
| `name` | Tool name (e.g. `read_file`, `grep`, `apply_patch`) |
| `arguments` | JSON object of the arguments supplied by the model |

## Session metadata (`session.json`, per session)

|Field          |Meaning                     |
|---------------|----------------------------|
|`id`           |UUID, stable across restarts|
|`name`         |Session name                |
|`created_at`   |RFC 3339 creation time      |
|`model`        |Model in use                |
|`system_prompt`|System prompt in effect     |

## Derived data (computed from the log, not stored redundantly)

|Derived value     |Source                                                                                                                                |
|------------------|--------------------------------------------------------------------------------------------------------------------------------------|
|Token usage       |Sum of all `llm_response` `usage` fields                                                                                              |
|Per-call latency  |Paired `llm_prompt` → `llm_response` timestamps                                                                                       |
|Total turns       |Most recent `turn_complete` turn number                                                                                               |
|Replayable history|`user_input`, `llm_response` (text/tool calls), and `tool_execution` entries; partial turns (no `turn_complete`/`error`) are truncated|

## Streaming note

In streaming mode the log remains complete for replay: `tool_execution`
entries record the real tool arguments (captured when the tool call is ready)
and the full result.
