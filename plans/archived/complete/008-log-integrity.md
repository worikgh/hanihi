# Plan 008 — Log schema, streaming completeness, tolerant reads

**Status:** draft | **Created:** 2026-09-01 | **Scope:** core + session + eval + analyser + `analyse`

## Overview

Three defects in the session event log:

1. **No schema.** `LogEntry` serialization has no version field
   (`crates/hanihi-core/src/session/log.rs`). Format drift is already real:
   `ToolExecutionData.call_id` was added with `#[serde(default)]` so old
   logs still parse. A reader cannot tell an old log from a new one, or
   detect a log written by a newer version.
2. **Streaming logs are incomplete.** `Session::run_streaming` only writes
   `tool_execution`, `turn_complete`, `error`
   (`crates/hanihi-core/src/session/mod.rs`). `llm_prompt`/`llm_response`
   are missing because `run_streaming_loop` never emits request/response
   data; `_provider`/`_model_name` are unused; per-call usage never reaches
   the log; reasoning is absorbed (`crates/hanihi-core/src/agent.rs`).
3. **Readers fail fast.** One malformed line aborts the whole read with no
   recovery: `Session::events()`, `hanihi-session-analyser/src/read.rs`,
   `hanihi-eval/src/main.rs` `parse_event_log`, and the current `analyse`
   binary all behave this way.

This plan fixes all three and migrates `analyse` to read the log with
`LogEntry`.

## Goals

- Version every log line; detect old and future formats explicitly.
- Make streaming sessions emit the same event kinds as `Session::run`.
- Add a tolerant reader that collects valid entries and reports bad lines.
- Remove duplicated JSONL parsing across crates; one core implementation.
- `analyse` prints `kind<TAB>ts` per entry using `LogEntry` and the
  tolerant reader.

## Design

### 1. Schema version, per line

Keep each JSONL line self-contained. Add an integer `schema` field:

```json
{"schema":1,"kind":"user_input","ts":"…","turn":1,"data":{…}}
```

- `SCHEMA_VERSION: u32 = 1` in `hanihi_core::session::log`.
- `LogWriter::write_entry` builds `serde_json::Value`, injects `schema`,
  then writes.
- Lines without `schema` are legacy (version 0); they keep parsing through
  the existing lenient `#[serde(default)]` fields.
- `schema > SCHEMA_VERSION` → the line came from a newer build; the reader
  reports it and skips (tolerant) or fails (strict).
- `schema < SCHEMA_VERSION` → run known migrations before deserializing.
  The hook is `fn migrate(value: &mut serde_json::Value, from: u32) -> …`.
  There are no migrations yet.
- `LogEntry` itself is unchanged: internally-tagged enum, so the extra
  `schema` key is ignored on deserialize and old readers ignore it too.

Policy: additive changes (new optional field with `#[serde(default)]`) do
not bump the version. Breaking changes (rename, remove, restructure) bump
it and add a migration.

### 2. Complete streaming logs

Extend the agent stream with two log-data events. This follows the existing
pattern (`ToolResult.result` already carries data purely for the log). The
agent stays free of session types: it emits neutral data, `Session` converts
to `LogEntry`.

```rust
pub struct StreamToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

pub enum StreamEvent {
    // … existing variants …
    CompletionRequest {
        ts: chrono::DateTime<chrono::Utc>,
        messages: serde_json::Value,
        tool_definitions: serde_json::Value,
    },
    CompletionResponse {
        ts: chrono::DateTime<chrono::Utc>,
        message_id: Option<String>,
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<StreamToolCall>>,
        input_tokens: u64,
        output_tokens: u64,
    },
}
```

In `run_streaming_loop`:

- Emit `CompletionRequest` after assembling the request and **before**
  `model.stream(...).await`, with its own `ts`.
- Accumulate reasoning deltas (currently absorbed) into a buffer.
- Capture per-call usage from each stream's `Final` (`r.token_usage()`);
  today only the turn-level `usage_total` is accumulated.
- After the stream exhausts, emit `CompletionResponse` with text,
  reasoning, tool calls, `message_id`, and per-call usage.

`Session::run_streaming`:

- Drop the `_` prefixes on `provider`/`model_name`; use them to build
  `llm_prompt`.
- Translate `CompletionRequest` → `LogEntry::llm_prompt` and
  `CompletionResponse` → `LogEntry::llm_response`.
- Replace `let _ = log_writer.write_entry(…)` and
  `.expect("re-open session log for streaming")` with proper handling:
  on writer failure, send `StreamEvent::Error` and stop logging.

Move `Session::messages_for_log` into a `pub(crate)` helper in `agent.rs`
(agent-level data). Both `Session::run` and the streaming loop call it, so
prompt JSON is built in one place.

### 3. Fault-tolerant reader

New API in `hanihi_core::session::log`:

```rust
pub struct LogReadError {
    pub line: usize,     // 1-based
    pub message: String,
}

pub struct LogReadResult {
    pub entries: Vec<LogEntry>,
    pub errors: Vec<LogReadError>,
}

pub fn parse_log_tolerant(contents: &str) -> LogReadResult;
pub fn read_log_tolerant(path: &Path) -> io::Result<LogReadResult>;
pub fn parse_log_strict(contents: &str) -> Result<Vec<LogEntry>, LogReadError>;
```

Per line: skip blanks; on invalid JSON, unsupported schema, or future
schema, record the error and continue (tolerant) or return the first error
(strict). Add `LogEntry::kind(&self) -> &'static str` next to `ts()` and
`turn()`.

Call sites:

- `analyse` → `read_log_tolerant`; valid entries to stdout, warnings to
  stderr.
- `hanihi-session-analyser` (`read.rs`) → delegate to the core tolerant
  reader; the CLI reports skipped lines.
- `hanihi-eval` (`parse_event_log`) → core strict reader (assertions need
  exact logs).
- `hanihi-session/src/main.rs` → replace its hand-rolled
  `serde_json::Value` parse with the strict `LogEntry` reader.
- `Session::events()` → core strict reader with line numbers; add a
  `SessionError::LogLine { line, message }` variant for the mapping.

### 4. `analyse` typed read

`analyse` drops the raw `Value` extraction:

```rust
let outcome = read_log_tolerant(&path)?;
for entry in &outcome.entries {
    println!("{}\t{}", entry.kind(), entry.ts());
}
for err in &outcome.errors {
    eprintln!("warning: {} line {}: {}", path.display(), err.line, err.message);
}
```

Exit codes: `2` usage error, `1` unreadable file, `0` otherwise (even with
skipped lines — they are reported on stderr).

## Compatibility / migration

- Old logs without `schema` keep working (version 0, lenient defaults).
- New logs read by old binaries: old `LogEntry` ignores the extra `schema`
  key, so lines still round-trip for additive reads.
- Breaking format changes require a `migrate` step before deserialization.

## Implementation order

1. `log.rs`: `SCHEMA_VERSION`, schema injection in `LogWriter`,
   `LogEntry::kind()`, strict + tolerant readers, schema checks and the
   `migrate` hook. Tests.
2. `agent.rs`: `StreamToolCall`, `CompletionRequest`/`CompletionResponse`,
   reasoning accumulation, per-call usage, shared `messages_for_log`, stream
   emissions. Tests with `MockCompletionModel`.
3. `session/mod.rs`: `run_streaming` translates the new events to
   `llm_prompt`/`llm_response`, uses provider/model, propagates writer
   errors as `StreamEvent::Error`. Tests.
4. Migrate call sites to core readers (`analyse`,
   `hanihi-session/src/main.rs`, `hanihi-session-analyser/src/read.rs`,
   `hanihi-eval/src/main.rs`, `Session::events()`).
5. `analyse`: print `kind<TAB>ts` via `LogEntry` + tolerant reader;
   warnings to stderr.
6. Gates: `cargo fmt`, `cargo check --workspace`,
   `cargo clippy -- -D warnings`,
   `cargo test -p hanihi-core -p hanihi-session -p hanihi-session-analyser -p hanihi-eval`.

## Tests

- Schema: writer emits `schema`; round-trip preserves it; legacy line
  without schema parses; future schema reported; `migrate` unit-tested once
  the first migration lands.
- Tolerant reader: blanks skipped; mixed valid/invalid/future-schema input
  collects valid entries and errors with 1-based line numbers; empty input;
  strict reader reports the first error.
- Streaming: with `MockCompletionModel` streaming turns, `run_streaming`
  produces `user_input`, `llm_prompt`, `llm_response`, `tool_execution`,
  `turn_complete`; `llm_response` usage equals the mock usage; tool-call
  entries keep real arguments/result (regression for
  `streaming_logs_real_tool_args_and_result`).
- `analyse`: unit tests for the parse/print path; golden `kind<TAB>ts`
  output.

## Out of scope (known, not fixed here)

- `Session::run` writes no `error` entry when a model call itself fails
  (only tool-execution errors and max-turns are logged); streaming logs
  stream errors. Decide whether LLM-call failures should be events.
- Streaming history seeding: callers must still call `set_history` from
  `TurnComplete.final_history`. Unchanged.

## Risks / tradeoffs

- Two new `StreamEvent` variants become visible to CLI consumers; their
  matches must ignore them. Precedent: `ToolResult.result` already carries
  log data in the display stream.
- Strict vs tolerant: correctness-critical readers stay strict; analysis
  tools become tolerant, so summaries may omit bad lines but always say so
  on stderr.
- Per-line `schema` is redundant but preserves JSONL self-containment — a
  header line would break "each line independently parseable".
