# Implement: log context compaction events to the session log

You are implementing observability for context compaction in the hānihi
codebase. Compaction already happens in
`crates/hanihi-core/src/agent.rs::compact_if_needed` and is reported only
through `tracing::info!`. Make every actual compaction also write a
`compaction` entry to the session's `events.jsonl` (schema 2). Follow the
detailed analysis in `plans/013-log-compaction-events.md`; this prompt is
the executable version.

## Guardrails

- Do not change compaction *behavior*: same trigger, summary prompt, caps,
  split logic, and in-memory `Agent.summary` / trimmed `Agent.history`.
- No new dependencies.
- Keep the diff minimal and idiomatic; follow the existing
  constructor/`kind`/`ts`/`turn`/`Display` pattern for every `LogEntry`
  variant.
- `Agent::run` (direct, no session) may keep ignoring the record. Only the
  `Session` paths write the log.

## Steps

### 1. `crates/hanihi-core/src/session/log.rs`

- Bump `SCHEMA_VERSION` from `1` to `2`. Update its doc comment: v2 adds
  the `compaction` kind; v1 lines still parse unchanged; `migrate` remains
  a no-op.
- Add:

  ```rust
  #[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
  pub struct CompactionData {
      pub before_tokens: usize,
      pub after_tokens: usize,
      pub summary: String,
      pub dropped_messages: usize,
      pub kept_messages: usize,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub summarization_usage: Option<UsageData>,
  }
  ```

- Add `LogEntry::Compaction { ts, turn, data }` with
  `#[serde(rename = "compaction")]`.
- Add `LogEntry::compaction(ts, turn, before_tokens, after_tokens,
  summary, dropped_messages, kept_messages, summarization_usage) ->
  Self`.
- Extend the exhaustive helpers for the new variant: `kind()` returns
  `"compaction"`; add arms to `ts()`, `turn()`, and `Display for
  LogEntry` (before/after tokens, dropped/kept counts, indented summary,
  optional usage line).

### 2. `crates/hanihi-core/src/agent.rs`

- Import `UsageData` beside the existing `ContextMessage` import.
- Add:

  ```rust
  pub(crate) struct CompactionRecord {
      pub(crate) before_tokens: usize,
      pub(crate) after_tokens: usize,
      pub(crate) summary: String,
      pub(crate) dropped_messages: usize,
      pub(crate) kept_messages: usize,
      pub(crate) summarization_usage: Option<UsageData>,
  }
  ```

- Change `compact_if_needed` to return
  `Result<Option<CompactionRecord>, AgentError>`:
  - under budget → `Ok(None)`
  - `old.is_empty()` → `Ok(None)`
  - after setting `*summary` / `*history`, return `Ok(Some(record))`
    using `old.len()`, `kept.len()`, and the summarization response's
    usage cast to `u32` for `UsageData`. Keep the existing
    `tracing::info!` call.
- Add `pub(crate) compaction: Option<CompactionRecord>` to
  `PreparedContext`; `prepare_context` stores the `compact_if_needed`
  result instead of discarding it.
- Add `StreamEvent::Compaction { ts, before_tokens, after_tokens,
  summary, dropped_messages, kept_messages, summarization_usage }`.
- In `run_streaming_loop`, emit `StreamEvent::Compaction` before
  `CompletionRequest` when compaction happened.

### 3. `crates/hanihi-core/src/session/mod.rs`

- Non-streaming `run`: after `prepare_context`, write
  `LogEntry::compaction(...)` **before** the `llm_prompt` entry when
  `prepared.compaction` is `Some`.
- Streaming `run_streaming`: add a `StreamEvent::Compaction { .. }` arm
  in the log-translation task that writes the matching `LogEntry`.
- `replay_history`: add `LogEntry::Compaction { .. }` to the
  skipped-entry arm and mention it in the method doc.

### 4. `crates/hanihi-cli/src/main.rs`

- Add `StreamEvent::Compaction { .. } => {}` to both exhaustive event
  matches (the `--once` loop and `run_turn`), or print a short
  `[context compacted: N → M tokens]` note.
- In both `TurnComplete` handlers, seed the summary back next to the
  existing `set_history` call:
  `agent.set_summary(summary.final_summary);` (clone in `run_turn`,
  where `summary` is returned afterwards).

### 5. Tests

- `log.rs`: `Compaction` round-trips; a schema-1 line still parses under
  `SCHEMA_VERSION = 2`; `Display` includes before/after and the summary.
- `agent.rs`: `prepare_context` returns `None` under budget and `Some`
  over budget with `before_tokens > after_tokens` and
  `dropped_messages > 0`.
- `session/mod.rs`: non-streaming `Session::run` over budget writes
  exactly one `compaction` entry positioned before `llm_prompt` with the
  expected summary; `replay_history` ignores it.
- Streaming: before claiming the streaming test passes, confirm whether
  rig's mock model can serve both the plain summarization `completion`
  and the scripted `stream` turns from one instance. If not, factor the
  record→event mapping into a small helper and unit-test that, or use a
  hand-rolled `CompletionModel` mock. Do not report a streaming test as
  passing until it has run.

## Acceptance

Run, in order:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

All pass. No new dependencies. `SCHEMA_VERSION` is `2`.

## Out of scope (defer)

- Resume-from-compaction: persisting `Agent.summary` and replaying
  history from the last compaction boundary.
- Changing `replay_history` to return a summary alongside the messages.

Hānihi
