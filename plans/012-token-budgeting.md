# Token budgeting, context compaction, and tool-output caps

## Goal

Prevent the 400 `maximum context length` failure (12.2M tokens requested vs
1M allowed) by:

1. Estimating input tokens before every model call.
2. Enforcing a per-model context limit (default 1M tokens) and compacting
   history when the estimate exceeds it.
3. Capping tool outputs at the agent layer.
4. Bounding file reads so huge files are never fully loaded.
5. Keeping the existing turn limit and adding a hard tool-call-per-turn
   guard.

## Verified ground truth

From the code as it stands:

- `crates/hanihi-core/src/agent.rs` assembles requests in three places that
  must stay consistent:
  - `Agent::single_completion` (used by `Session::run`)
  - `Agent::run` (loop over `self.history` + `turn_messages`)
  - `run_streaming_loop` (owns a `&mut Vec<Message>` in a spawned task)
- `Session::run` (`crates/hanihi-core/src/session/mod.rs`) builds
  `build_context(...)` for logging and then calls `agent.single_completion`,
  which rebuilds the messages. After compaction these two can diverge, so
  they must share one prepared context.
- `build_context` (`agent.rs`) already produces the log-shaped
  `Vec<ContextMessage>`.
- `SourceTree::read` (`crates/hanihi-core/src/source.rs`) uses `fs::read` —
  it loads the whole file into memory before truncating to `MAX_READ_BYTES`
  (64 KiB). This is the unbounded-read hazard.
- Tool-output caps already exist per-tool:
  - `read_file`: 64 KiB (`MAX_READ_BYTES`)
  - `grep`: 200 matches / 64 KiB (`MAX_GREP_BYTES`)
  - `run_command`: `cap_output` at 64 KiB, full output in a trace file
  - `read_session_log`: `cap_output`
  - **Gap:** `mcp.rs render_call_result` is uncapped, and the agent dispatch
    (`execute_tool_with_cache`) stores/returns `output.render()` raw. MCP
    output is the main unbounded path.
- The loop guard `max_turns` already exists (`Agent::max_turns`, CLI
  `--max-turns`). The CLI default is currently `29`
  (`crates/hanihi-cli/src/main.rs`), which disagrees with the README — note
  it, do not change semantics in this task.
- No token estimator dependency exists today (`tiktoken-rs`, `tokenizers`,
  etc. are absent from `Cargo.lock`). This plan adds `tiktoken-rs`; no other
  tokenizer is introduced.
- `plans/wts_compaction.md` describes pi's compaction design; reuse its
  trigger shape (`context > window - reserve`) and summary format ideas, but
  adapt to this codebase.
- Log schema is versioned (`session::log::SCHEMA_VERSION`). Adding a new
  `LogEntry` variant is a breaking change for old readers (tagged enum).
  Phase 1 must not add one.

## Key decisions

1. **Token estimation = `tiktoken-rs` BPE count.** Use the `cl100k_base`
   encoding as a shared, lazily built tokenizer.
   `estimate_tokens(text)` returns
   `bpe.encode_with_special_tokens(text).len()`. Cache the `CoreBPE` in a
   `std::sync::OnceLock` so the rank data is parsed once per process. If
   tokenizer construction fails, fall back to the chars/4 heuristic rather
   than failing the call. Document the encoding choice: DeepSeek does not
   publish its tokenizer, so `cl100k_base` is a conservative
   OpenAI-compatible default.
2. **Budget check before every call:**
   `estimated_input_tokens > context_limit - reserve_tokens` triggers
   compaction. Defaults:
   - `DEFAULT_CONTEXT_LIMIT_TOKENS = 1_048_576` (1M)
   - `RESERVE_OUTPUT_TOKENS = 16_384`
   - `KEEP_RECENT_TOKENS = 20_000`
   - `MAX_SUMMARY_TOKENS = 8_000`
3. **Per-model limit table** `context_limit_for(model: &str) -> usize`.
   Default 1M. Hard-coded entries for `deepseek-v4-pro` and `deepseek-flash`
   (1M). Add `Agent::set_context_limit_tokens` for tests and a future CLI
   override.
   - **Open item:** "nownership" appears in `working/.p.md` but is not
     defined anywhere in the repo. Confirm what it refers to before writing
     the table; until then, only the two DeepSeek names above are hard-coded.
4. **Compaction is an extra plain `completion` call** to the same model
   (never a stream), using a dedicated summarization prompt. Keep the summary
   **in memory** as `Agent.summary: Option<String>`, injected into the
   effective system prompt. Do **not** add a log entry yet — see step 8.
5. **Cut points are turn boundaries only** in phase 1. The kept history must
   begin at a user message so the OpenAI contract "assistant `tool_calls`
   followed by tool results" is never broken. If a single turn is larger than
   `KEEP_RECENT_TOKENS`, keep the whole turn and fall back to the truncation
   path (step 6).
6. **Summary goes into the preamble**, not a `Message`, unless implementation
   confirms rig 0.41 `Message` has a `System` variant that can be placed
   mid-conversation. Preamble injection is the safe, idiomatic path here.
7. **Log prompt == sent prompt.** One preparation helper must produce the
   exact message list that is both sent and logged.
8. **No schema change in phase 1.** Replay (`replay_history`) reconstructs
   the full prior history, so a resumed session re-compacts on its first
   oversized call. That is safe and durable. Log compaction with
   `tracing::info!`. Phase 2 can add a schema-bumped `compaction` event and
   replay support.

## Work items (in order)

### 1. Add the `tiktoken-rs` dependency

In `crates/hanihi-core/Cargo.toml`:

- Add `tiktoken-rs` to `[dependencies]`. Pin the minor version at
  implementation time and record it in the plan/commit message.
- Enable only the features needed for `cl100k_base`; disable unused encodings
  if the crate exposes feature flags.
- Justification: exact BPE token counts for budget checks, replacing the
  chars/4 heuristic.

Verify before implementing whether `tiktoken-rs` bundles its rank data in the
published crate or fetches it at build time. If it fetches, document the
offline-build constraint and keep the chars/4 fallback wired in.

### 2. New module `crates/hanihi-core/src/context.rs`

- `pub(crate) const DEFAULT_CONTEXT_LIMIT_TOKENS: usize = 1_048_576;`
- `pub(crate) const RESERVE_OUTPUT_TOKENS: usize = 16_384;`
- `pub(crate) const KEEP_RECENT_TOKENS: usize = 20_000;`
- `pub(crate) const MAX_SUMMARY_TOKENS: usize = 8_000;`
- `fn tokenizer() -> &'static CoreBPE` — `OnceLock`; builds `cl100k_base()`
  once and caches it. Returns a fallback marker when construction fails.
- `pub(crate) fn estimate_tokens(text: &str) -> usize` — tokenizer
  `encode_with_special_tokens(text).len()`; empty text → 0. When the
  tokenizer is unavailable, fall back to `text.chars().count().div_ceil(4)`.
- `pub(crate) fn estimate_context(...) -> usize` — sum of token estimates for
  system prompt + summary + history + turn messages + current user input +
  serialized tool definitions.
- `pub(crate) fn context_limit_for(model: &str) -> usize` — table described
  above.
- `pub(crate) const COMPACTION_PROMPT: &str` — instruct the model to preserve
  goal, constraints, decisions, current state, next steps, and **exact**
  paths/identifiers/versions/error text; output plain markdown.
- `pub(crate) fn serialize_for_summary(messages: &[Message]) -> String` —
  render `[User]`, `[Assistant]`, `[Tool result]` lines; truncate each tool
  result to 2 000 chars with a marker.
- `pub(crate) fn truncate_to_token_budget(text: &str, max_tokens: usize) ->
  String` — encode, truncate the token vector to `max_tokens`, decode back;
  chars approximation when the tokenizer is unavailable.
- `pub(crate) fn split_history(history: &[Message]) -> (old: &[Message],
  kept: &[Message])` — walk backward from the newest user-message boundary,
  accumulating `estimate_tokens` until `KEEP_RECENT_TOKENS`; return the newest
  boundary that fits. Never split inside a turn.

Unit-test every pure function here (see Test plan).

### 3. Wire the limit into `Agent`

In `agent.rs`:

- Add fields `context_limit_tokens: usize` (default
  `DEFAULT_CONTEXT_LIMIT_TOKENS`) and `summary: Option<String>`.
- `connect_chat_model_with_prompt` sets
  `context_limit_tokens = context_limit_for(&model)`.
- Add `pub fn set_context_limit_tokens(&mut self, usize)` and
  `pub fn context_limit_tokens(&self) -> usize`.
- Add `pub(crate) fn effective_preamble(&self) -> String` = base system
  prompt, then a clearly-delimited `Summary of the conversation so far:`
  block when `summary` is `Some`.

### 4. Add compaction

In `agent.rs` (or `context.rs` with an async helper in `agent.rs`):

- `async fn compact_if_needed(&mut self, turn_messages: &[Message],
  tool_defs_json_len: usize) -> Result<bool, AgentError>`
  - Estimate; if under `limit - reserve`, return `Ok(false)`.
  - Else `split_history(&self.history)`.
  - If `old` is empty, return `Ok(false)` (nothing to summarize; fall through
    to truncation).
  - Build the summarization request: `COMPACTION_PROMPT` + previous summary
    (if any) + `serialize_for_summary(old)`.
  - Run a plain `self.model.completion(request)`; take the text; cap it to
    `MAX_SUMMARY_TOKENS` with `truncate_to_token_budget`.
  - Set `self.summary = Some(new_summary)` and `self.history = kept.to_vec()`.
  - Re-estimate. If still over budget (one giant recent turn), leave the
    state but continue — the next layer is truncation (step 6).
  - `tracing::info!` with before/after token estimates.
  - Return `Ok(true)`.

### 5. One prepared-context path

Add a single builder so `run`, `run_streaming_loop`, and `Session::run` all
call it:

- `struct PreparedContext { messages: Vec<Message>, log_messages:
  Vec<ContextMessage>, compacted: bool }`
- `Agent::prepare_context(&mut self, user_input, turn_messages) ->
  Result<PreparedContext, AgentError>`:
  1. `compact_if_needed(...)`
  2. build the rig `Vec<Message>` actually sent (`self.history` +
     `turn_messages`),
  3. build the matching `Vec<ContextMessage>` via the existing `build_context`
     shape (system preamble = `effective_preamble`, history, turn messages,
     user last).

Then update:

- `Agent::single_completion` → `&mut self`, use `prepare_context`, send
  `messages`, and return the prepared context alongside the response (or have
  callers prepare first and pass it in). Prefer: callers prepare, then call a
  `single_completion_with(messages, tools)`.
- `Agent::run` → prepare once per loop iteration and send exactly those
  messages.
- `run_streaming_loop` → prepare once per iteration (it owns `history` and can
  hold a local `summary`), then use the prepared messages.
- `Session::run` → call `agent.prepare_context(...)` once per iteration, log
  `PreparedContext.log_messages`, then send `PreparedContext.messages`.
  Remove the duplicated `build_context` call.
- `Session::run_streaming` already logs the `CompletionRequest` event emitted
  by the streaming loop; keep that as the single source of truth.

This removes the current divergence risk and gives every call one token
check.

### 6. Cap tool outputs at the agent layer

In `tool.rs`:

- Add `pub(crate) const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;`
- In `execute_tool_with_cache` (agent.rs), apply a shared
  `truncate_tool_output(&rendered)` after `output.render()` and before
  caching/storing. Append a note with the original byte count when truncated.
  This is a **backstop**; keep the per-tool caps as the primary policy.
- In `mcp.rs`, cap `render_call_result` with the same helper. MCP text blocks
  are the largest uncapped path today.
- Reuse or generalize the existing `cap_output` in `tool.rs` so there is one
  truncation note format.

Do not change the `run_command` trace-file behavior: full output still goes
to the trace file; only the inline result is capped.

### 7. Bound `SourceTree::read`

In `source.rs`:

- Replace `fs::read(&canon)` with `File::open` +
  `Read::take(MAX_READ_BYTES + 1)`.
- Take file size from `metadata.len()` so the truncation note reports the true
  total without loading the file.
- Keep the `String::from_utf8_lossy` conversion on the bounded bytes only.
- Keep the existing return type and truncation-note format so
  `builtin_read_file` and tests are unaffected except for the memory bound.

This prevents a multi-GB file from being read into memory.

### 8. Hard tool-call-per-turn guard

In `agent.rs`:

- Add `const MAX_TOOL_CALLS_PER_TURN: usize = 100;`
- Before each tool execution in `run` and `run_streaming_loop`, check
  `tool_calls_total >= MAX_TOOL_CALLS_PER_TURN`; on breach, log an error and
  return a new `AgentError::ToolCallLimit { calls }` (add to `error.rs` with
  `Display`).
- Keep `max_turns` exactly as it is; this guard is additional, not a
  replacement.
- The existing `ToolCallCache` duplicate-limit for read-only tools stays; do
  not extend it to write tools.

### 9. Explicitly defer schema changes

- Do not add `LogEntry::Compaction` in phase 1.
- Document in the module comment that compaction is in-memory and re-derived
  after resume.
- Phase 2 (separate task): add a `compaction` event, bump `SCHEMA_VERSION`,
  add the migration, and teach `replay_history` to start from the last
  compaction boundary.

## Test plan

All tests must be network-free, using `rig`'s `MockCompletionModel`.

`context.rs`:

- `estimate_tokens`: empty → 0; a fixed corpus matches a hand-checked
  `cl100k_base` count; unicode handled; monotonic with text length.
- `context_limit_for`: default 1M; `deepseek-v4-pro` and `deepseek-flash` →
  1M.
- `split_history`: cuts at a user boundary; keeps recent within
  `KEEP_RECENT_TOKENS`; returns everything as `old` when a single turn
  dominates; empty history.
- `truncate_to_token_budget`: reduces text to at most `max_tokens`; leaves
  short text unchanged.

`agent.rs`:

- Under budget: `MockCompletionModel` scripted for one turn; assert no extra
  summarization call was made (call-count == 1).
- Over budget: scripted turns `[summarize, answer]`; assert `history` is
  trimmed, `summary` set, the second request's messages contain the summary
  preamble, and the run succeeds.
- `prepare_context` produces `log_messages` that match the sent `messages`
  (role/content parity).
- Tool result > 64 KiB is truncated with a note before being cached/returned.

`source.rs`:

- `read` on a file larger than `MAX_READ_BYTES` returns ≤ cap + note; add a
  test file of a few MB (not GB) to prove bounded behavior.

`tool.rs` / `mcp.rs`:

- MCP render path: text blocks exceeding the cap are truncated with a note.
- Existing `cap_output` tests stay green.

`session/mod.rs`:

- `Session::run` logs the compacted (prepared) prompt, not the pre-compaction
  one.
- Replay of a session with oversized history still yields a valid run because
  compaction re-triggers.

## Acceptance

Run, in order:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

New dependency: `tiktoken-rs` only, pinned and justified. No schema bump. No
changes to `max_turns` semantics.

## Risks / assumptions to confirm before implementing

1. **`nownership`** — undefined in the repo. Confirm the exact model names and
   their limits before finalizing `context_limit_for`.
2. **Encoding match** — DeepSeek's real tokenizer may differ from
   `cl100k_base`, so estimates are approximate. Mitigate with
   `RESERVE_OUTPUT_TOKENS` and verify the budget check against a real
   provider on a near-limit session.
3. **Tokenization cost** — BPE-encoding a multi-MB context before every call
   costs CPU. Acceptable at 1M scale, but profile if a call's pre-send work
   becomes significant.
4. **Data availability** — verify whether `tiktoken-rs` bundles its rank data
   or fetches it at build time. If fetched, document the offline-build
   constraint and keep the chars/4 fallback.
5. **rig `Message::System`** — verify availability; otherwise preamble
   injection is the committed path.
6. **Summary loss** — compaction is lossy; the prompt must demand exact
   preservation of paths, IDs, and error text, and the most recent turns stay
   verbatim to limit loss.
