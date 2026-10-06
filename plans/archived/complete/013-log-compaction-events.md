# Log compaction events to the session log

## Goal

Record every context compaction in `events.jsonl` so a session log shows
*when* the agent summarized old history, how much context shrank, what the
new summary is, and how many messages moved into it. Today compaction is
only reported via `tracing::info!` in `compact_if_needed`, which is lost
unless tracing is captured.

## Scope guardrails

Do **not** change compaction *behavior* in this task:

- Same trigger (`estimate_context > limit - RESERVE_OUTPUT_TOKENS`).
- Same summary prompt, `MAX_SUMMARY_TOKENS` cap, and `split_history`
  cut logic.
- Same in-memory `Agent.summary` + trimmed `Agent.history`.
- No new dependencies.
- **Defer** resume-from-compaction (persisting the summary and starting
  replay from the last compaction boundary). That is a later task and a
  `replay_history` API change. This task only makes compaction visible in
  the log.

## Ground truth

- Compaction happens in `crates/hanihi-core/src/agent.rs`:
  `compact_if_needed`, called from `Agent::prepare_context` (non-streaming)
  and `run_streaming_loop` (streaming). It currently returns
  `Result<bool, AgentError>` and both callers discard the value.
- `compact_if_needed` already computes `before` and `after` token
  estimates, and has `old`/`kept` (the message counts to log) in scope,
  plus the summarization response's `usage` before it is dropped.
- `Agent::prepare_context` returns `PreparedContext { preamble, messages,
  user_input, log_messages }`. `Session::run` consumes it and has the
  `LogWriter` + turn number, so the non-streaming path is the easy one.
- `run_streaming_loop` has no `LogWriter`; it talks to `Session::run_streaming`
  only through `StreamEvent`, so the streaming path needs a new event.
- `LogEntry` (`crates/hanihi-core/src/session/log.rs`) is a serde
  internally-tagged enum. A new variant is a breaking change for old
  readers, so `SCHEMA_VERSION` must bump from 1 to 2 per the policy in its
  doc comment. `migrate()` stays a no-op: v1 lines still parse, since no
  existing `kind` changes.
- `StreamEvent` matches are exhaustive in:
  - `crates/hanihi-cli/src/main.rs` (two `match event` blocks)
  - `crates/hanihi-core/src/session/mod.rs` (streaming log translation)
  Adding a variant forces these to compile-fail until updated.
- `replay_history` has an exhaustive `LogEntry` match with a skip arm for
  lifecycle/prompt entries; the new variant belongs there.
- `hanihi-session/src/bin/analyse.rs` and `hanihi-eval` read logs with
  `if let`/tolerant parsers, so they need no changes. `analyse` brief and
  verbose modes will show the new `kind`/`Display` automatically.

## Key decisions

1. **One event per actual compaction.** Write `LogEntry::Compaction` only
   when a summarization call was made and state changed. No event for
   under-budget runs or for the single-dominating-turn fallthrough where
   `old.is_empty()`.
2. **Wire shape** (schema 2):

   ```json
   {
     "schema": 2,
     "kind": "compaction",
     "ts": "…",
     "turn": 3,
     "data": {
       "before_tokens": 123456,
       "after_tokens": 45678,
       "summary": "## Goal …",
       "dropped_messages": 18,
       "kept_messages": 2,
       "summarization_usage": { "input_tokens": 20000, "output_tokens": 800 }
     }
   }
   ```

   `summarization_usage` is optional (`#[serde(default,
   skip_serializing_if = "Option::is_none")]`) so a mock model with no
   usage still serializes cleanly.
3. **Ordering.** The compaction entry is written immediately before the
   `llm_prompt` it caused, i.e. after `prepare_context` returns and before
   the prompt entry (non-streaming), and before the `CompletionRequest`
   event (streaming).
4. **Record type.** Put a `pub(crate) CompactionRecord` in `agent.rs`,
   produced by `compact_if_needed` and carried on `PreparedContext`, so the
   sent prompt and the compaction metadata come from one preparation step —
   same pattern as the existing "logged prompt == sent prompt" guarantee.
5. **Streaming event.** Add `StreamEvent::Compaction { … }` carrying the
   same fields as `CompactionRecord`. `Session::run_streaming` translates
   it to `LogEntry::compaction`.
6. **Count the summarization call.** Capture the summarization response's
   `usage` in the record. That model call currently contributes tokens that
   appear in no accounting anywhere; the event is the natural place to
   record it.
7. **Related two-line fix (do it while here).** The CLI streaming handlers
   seed `summary.final_history` back via `set_history` but never seed
   `summary.final_summary` via `set_summary`, so in a long-lived REPL the
   summary is dropped after every turn. Fix both `TurnComplete` handlers so
   in-process compaction is cumulative. This makes the logged events mean
   what they appear to mean.

## Work items, in order

### 1. `crates/hanihi-core/src/session/log.rs`

- Bump `SCHEMA_VERSION` to `2`. Update its doc comment: v2 adds the
  `compaction` kind; v1 lines need no migration.
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

- Add `LogEntry::Compaction` with `#[serde(rename = "compaction")]`,
  fields `ts`, `turn`, `data`.
- Add `LogEntry::compaction(ts, turn, before_tokens, after_tokens,
  summary, dropped_messages, kept_messages, summarization_usage)`.
- Extend the exhaustive helpers: `kind()` → `"compaction"`, `ts()`,
  `turn()`, and `Display for LogEntry` (before/after tokens, dropped/kept
  counts, indented summary, optional usage).
- Leave `migrate()` as a no-op; adjust its comment to say v1→v2 is
  additive.

### 2. `crates/hanihi-core/src/agent.rs`

- Import `UsageData` alongside the existing `ContextMessage` import.
- Define:

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
  - after setting `*summary` / `*history` → `Ok(Some(CompactionRecord {
    before_tokens: before, after_tokens: after, summary: new_summary,
    dropped_messages: old.len(), kept_messages: kept.len(),
    summarization_usage: Some(UsageData { input_tokens: …, output_tokens: … })
    }))`
  Keep the existing `tracing::info!` as well.
- Add `pub(crate) compaction: Option<CompactionRecord>` to
  `PreparedContext`; `prepare_context` stores the `compact_if_needed`
  return value instead of discarding it.
- Add `StreamEvent::Compaction` with the same fields as
  `CompactionRecord` (plain owned types; keep it `Clone`/`Debug` like the
  other variants).
- In `run_streaming_loop`, replace the discarded
  `compact_if_needed(...).await?` with:

  ```rust
  if let Some(record) = compact_if_needed(...).await? {
      let _ = tx.send(StreamEvent::Compaction { ts: Utc::now(), … }).await;
  }
  ```

  Emit it before `CompletionRequest`.

### 3. `crates/hanihi-core/src/session/mod.rs`

- Non-streaming `run`: after `prepare_context`, when
  `prepared.compaction` is `Some`, write
  `LogEntry::compaction(Utc::now(), self.turn, …)` via `self.log_entry`
  **before** the `llm_prompt` entry.
- Streaming `run_streaming`: add a
  `StreamEvent::Compaction { ts, before_tokens, after_tokens, summary,
  dropped_messages, kept_messages, summarization_usage }` arm that writes
  the matching `LogEntry`.
- `replay_history`: add `LogEntry::Compaction { .. }` to the skipped-entry
  arm (and mention it in the method doc).

### 4. `crates/hanihi-cli/src/main.rs`

- In the `--once` event loop and in `run_turn`, add
  `StreamEvent::Compaction { .. } => {}` (nothing to display; optionally
  print a short `[context compacted: N → M tokens]` line).
- In both `TurnComplete` handlers, add
  `agent.set_summary(summary.final_summary);` next to the existing
  `set_history` call.

## Test plan

All network-free, using `rig`'s mock model. The existing
`test_over_budget_compacts_history` (agent level, `MockCompletionModel::from_turns`)
proves the non-streaming completion-script pattern already works.

- `log.rs`:
  - `Compaction` entry serializes with `kind: "compaction"` and round-trips.
  - A schema-1 line still parses under `SCHEMA_VERSION = 2`.
  - Future-schema rejection is unchanged.
  - `Display` includes before/after and the summary.
- `agent.rs`:
  - `prepare_context` returns `compaction: None` under budget, `Some` over
    budget, with `before_tokens > after_tokens` and `dropped_messages > 0`.
  - `run_streaming_loop` emits `StreamEvent::Compaction` before
    `CompletionRequest` when over budget. **Verify at implementation time**
    whether rig's mock can serve both the plain summarization
    `completion` and the scripted `stream` turns from one instance; if not,
    factor the record→event mapping into a tiny helper and unit-test that,
    or use a hand-rolled `CompletionModel` mock for this one test.
- `session/mod.rs`:
  - Non-streaming `Session::run` with an over-budget history writes exactly
    one `compaction` entry, positioned before the `llm_prompt`, with the
    expected summary text.
  - Streaming `Session::run_streaming` writes the same entry from the
    event.
  - `replay_history` ignores `Compaction` entries.

## Acceptance

Run, in order:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

No new dependencies. `SCHEMA_VERSION` is 2. Compaction behavior is
unchanged; only its observability changes.

## Deferred (not part of this task)

- Persisting `Agent.summary` so a resumed session starts from the last
  compaction instead of replaying full history and re-compacting.
- Extending `replay_history` (or adding a sibling) to return
  `(Vec<Message>, Option<String>)` and trimming history at the last
  compaction boundary.
- A "compacted" marker in `--verbose`/`analyse` beyond what the generic
  kind/Display already provide.

## Risks

- **Mock model mixing** for the streaming compaction test (see test plan).
  Resolve before claiming the streaming test passes.
- **Schema bump ripple** is small but real: any tool that writes or reads
  `events.jsonl` must be rebuilt against the new `LogEntry`. In-repo
  consumers are covered by `cargo check --workspace`; external/old binaries
  fail cleanly on schema 2.
