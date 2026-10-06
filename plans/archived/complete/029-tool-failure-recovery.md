# Plan 029 — Tool failure recovery, and retiring the non-streaming path

You are Hānihi. This plan is a prompt to a future session. Its job is to make
a failed tool call **survivable**: the model must be told what failed, must get
another attempt, and the harness must still be guaranteed to stop.

This plan is self-contained. Implement it even if no other numbered plan has
landed. Do not modify anything in anticipation of other plans.

## Objective

Three changes, which must land together:

1. A failed tool execution becomes a **tool result** the model can read and act
   on, instead of a fatal error that ends the turn.
2. A guard bounds **repeated identical failures**, so change 1 cannot turn a
   premature stop into an unbounded loop.
3. The non-streaming path (`Agent::run`, `Session::run`) is **stubbed** as
   deprecated. The CLI has used `run_streaming` exclusively since the
   streaming handler landed; the non-streaming loop is unexercised production
   code that has already drifted.

## Context: the observed failure

A session stopped after `run_command` was refused:

```
crates/hanihi-core/src/tool.rs:519  git branch is restricted to listing (no create/delete/move)
crates/hanihi-core/src/agent.rs:1034  execute_tool_with_cache error. name: run_command Error: tool 'run_command' failed: ...
crates/hanihi-cli/src/main.rs:760  error message: tool 'run_command' failed: ...
```

The failure was **ours, not the repository's**: `tool.rs` refuses `git branch`
by policy. The model had done nothing wrong, was told a wall existed, and the
turn ended underneath it.

The session event counts show what the model never received:

```
CompletionRequest => 36, CompletionResponse => 35
ToolCallStart => 55, ToolCallReady => 55, ToolResult => 54, Error => 1
```

One tool call produced an `Error` instead of a `ToolResult`. The model was
never given the failure as information.

### The four defects

**(1) The agent aborts the loop.** `agent.rs` ~1029-1042, the `Err` arm of
`execute_tool_with_cache` in `run_streaming_loop`:

```rust
Err(e) => {
    eprintln!("{}:{}:execute_tool_with_cache error. name: {name} Error: {e}", ...);
    let _ = tx.send(StreamEvent::Error { message: e.to_string() }).await;
    return Err(e);          // ← exits run_streaming_loop
}
```

**(2) No tool result is recorded.** The `Ok` arm directly above pushes into
`pending_results` and, after the stream, into `pending_tool_calls`. The `Err`
arm does neither.

**(3) A failed call is indistinguishable from no call.** Because
`pending_tool_calls` stays empty, this check — `agent.rs` ~1106, after the
stream ends:

```rust
if pending_tool_calls.is_empty() {
    // "no tool calls were made, the turn is complete"
    ... return Ok(turn_summary)
}
```

reads a failed tool call as *no* tool call. Deleting the `return Err(e)`
without also pushing into `pending_tool_calls` would therefore end the turn as
a **success**, silently swallowing the failure. This is why defect 1 cannot be
fixed alone, and it is the single most important constraint in this plan.

**(4) The CLI cannot tell failure from success.** `main.rs` ~746-749 returns
`None` on `StreamEvent::Error`; `main.rs` ~712 also returns `None` when the
event channel closes cleanly. Two outcomes, one value. A stubbed path must not
be confusable with either.

### Two duplicate loops, not one

`Session::run` (`session/mod.rs:381`) does **not** delegate to `Agent::run`
(`agent.rs:497`). It reimplements the loop: prepare context, single completion,
extract tool calls, execute tools, build messages. Its tool arm
(`session/mod.rs:531-561`) has the same shape as defect 1:

```rust
Err(e) => {
    self.log_entry(&LogEntry::error(..., ErrorStage::ToolExecution, e.to_string()))...;
    agent.commit_turn(user_input, turn_messages);
    return Err(e);          // ← same abort
}
```

Any change to the non-streaming behaviour must touch both, which is a second
reason to stub rather than repair.

## Read first, verbatim

- `crates/hanihi-core/src/agent.rs`
  - `MAX_TOOL_CALLS_PER_TURN` (~99) — the existing per-turn guard. Note it
    bounds *successful* executions; the new guard is orthogonal.
  - `CACHEABLE_TOOLS` (~102), `WRITE_TOOLS` (~106), `ToolCallCache` (~120)
    including `key` (~131), `lookup` (~135), `store` (~151),
    `invalidate_on_write` (~158). The new guard is a sibling of `lookup`.
  - `StreamEvent` (~193-240) and `StreamEvent::type_name` (~241). Adding an
    event variant requires updating `type_name`; the match is exhaustive.
  - `Agent::run` (~497) and `Agent::execute_tool` (~620).
  - `run_streaming_loop` (~855), the tool-call arm (~975), the `Err` arm
    (~1029), the `pending_tool_calls.is_empty()` check (~1106), the
    `TurnComplete` send (~1115), the max-turns tail (~1152).
- `crates/hanihi-core/src/session/mod.rs`
  - `Session::run` (~381) **in full** — it is being replaced.
  - `Session::run_streaming` (~592) — the surviving implementation. It spawns
    the log-writing task and forwards events; do not change its shape.
  - `ErrorStage` usage, and the `session/mod.rs:1254` test that calls `run`.
- `crates/hanihi-cli/src/main.rs`
  - `DEFAULT_MAX_TURNS` / `TASK_MAX_TURNS` (~61-67), `set_max_turns` (~394).
  - The `--task`/`--once` handler (~509-545) and `run_turn` (~706-757).
    Two sites, both streaming.
- `crates/hanihi-core/src/error.rs` — `AgentError` in full.
- `crates/hanihi-core/src/tool.rs` — `check_git_branch` (~499-520) and the
  sibling `git tag` refusal (~532).
- `crates/hanihi-core/src/lib.rs` — re-exports; note anything that exposes the
  stubbed API.
- `plans/026-deterministic-apply-patch.md` — prior art for
  `recovery: Option<String>` in refusal payloads. Match that vocabulary.

## Design

### A. A failed tool execution becomes a tool result

In `run_streaming_loop`, the `Err` arm must do exactly what the `Ok` arm does,
with the error rendered as text.

Add a named constant for the rendered prefix rather than an inline literal:

```rust
/// Prefix on a tool result that reports a failed execution. The model reads
/// this as data it must act on, not as a harness shutdown.
const TOOL_FAILURE_PREFIX: &str = "tool call failed";
```

Render the failure as a single string:

```
tool call failed: run_command
  error: tool 'run_command' failed: git branch is restricted to listing (no create/delete/move)
  recovery: use `git log`, `git show`, or `git status` to inspect branches
```

The `recovery` line is present only when the failure carries one (see C).

Then, in order:

- `pending_results.push((tool_call.clone(), rendered))`
- `pending_tool_calls.push(tool_call)`
- send `StreamEvent::ToolResult` with that rendered text — **not**
  `StreamEvent::Error`
- `tool_calls_total += 1` (a failed execution is still an execution, and this
  keeps `MAX_TOOL_CALLS_PER_TURN` an honest bound)
- record the failure for the guard in B
- do **not** return

The loop then issues another completion with the failure visible in
`turn_messages`, which is what `DEFAULT_SYSTEM_PROMPT` already instructs:

> When a tool call fails or reports an error, do not stop. Report the failure,
> then continue toward the goal by the next viable means (retry once only if
> the cause was transient; otherwise try a different approach).

`StreamEvent::Error` keeps its existing meaning: a genuine abort that ends the
turn. After this change it should fire only for channel failure and rig-level
stream errors (the arm at `agent.rs` ~1065). Do not repurpose it.

### B. Guard repeated identical failures

`MAX_TOOL_CALLS_PER_TURN` (100) and `max_turns` (29 from the CLI) already make
an unbounded loop impossible. The gap is that they are spent by *productive*
work too: a legitimate five-call investigation and a five-call thrash are
indistinguishable to a counter. The guard must measure the specific pathology
change A introduces — the model repeating a call that has already failed.

The central design rule:

> **A different failure is progress.** A model that tries `git show` after
> `git branch` was refused is adapting and must not be penalised. Only a
> repeated call — same tool name, same arguments — accumulates.

This is exactly the rule `ToolCallCache` already applies to *successful*
read-only calls, so extend that struct rather than adding a new mechanism. It
already has the key function and the reset semantics:

```rust
/// Consecutive identical tool failures tolerated before the turn ends.
///
/// Counted per (name, args) pair. A failure with a different name or
/// arguments is evidence the model adapted and does not accumulate.
const REPEATED_FAILURE_LIMIT: usize = 3;
```

Add to `ToolCallCache`:

```rust
/// (name, args) -> consecutive failure count.
failures: HashMap<String, usize>,
```

Cleared by `reset()`. Provide:

- `fn record_failure(&mut self, name: &str, args: &Value)`
- `fn notable_for_failure(&mut self, name: &str, args: &Value) -> FailureLookup`

where `FailureLookup` is an enum mirroring the existing `ToolCallCacheLookup`
style (`Hit`/`DuplicateLimit`/`Miss`):

```rust
enum FailureLookup {
    /// Below the limit; continue with the failure fed back.
    Continue,
    /// The identical call has now failed REPEATED_FAILURE_LIMIT times.
    GiveUp { count: usize },
}
```

Do **not** reuse `invalidate_on_write` semantics for failures: a successful
write does not make a refused command legal, so a write must not clear the
failure counts. State this in a comment at the call site, because the
asymmetry with the read cache is deliberate and will otherwise look like an
oversight.

Crucially, reset the failure count for a key on a **successful** call with
that key. Otherwise a model that fails, fixes, succeeds, later fails again
accumulates across unrelated attempts.

When `FailureLookup::GiveUp` fires, end the turn with a typed error naming the
call. Add to `AgentError` (`error.rs`), matching the existing manual
`Display`/`Error` style (no `thiserror`):

```rust
/// A tool call failed repeatedly with identical arguments.
RepeatedToolFailure {
    /// Tool name.
    name: String,
    /// Number of consecutive identical failures.
    count: usize,
},
```

with `Display`: `tool '{name}' failed {count} times with identical arguments`.

End the turn the way `ToolCallLimit` does (see the `agent.rs` ~985 arm): send a
final `StreamEvent::Error` describing it, then `return Err(...)`. The turn's
messages are already accumulated, and `TurnComplete` is not sent — the caller
sees a genuine abort, which is correct here.

### C. Refusal text must name the door

`tool.rs` ~519 currently says only that a wall exists. A model told
"restricted to listing" cannot act; a model told which commands *are* admitted
can. Extend the refusal with a `recovery` clause:

```
git branch is restricted to listing (no create/delete/move). Use `git log`,
`git show`, or `git status` to inspect branches.
```

Apply the same treatment to `git tag` (~532) and the sibling `git branch`
refusals in `check_git_branch` that currently return a bare `Err(...)`.

Keep the existing text and its wording: tests assert on it. **Extend, do not
replace.** This follows the `recovery: Option<String>` vocabulary established
in plan 026.

This is not cosmetic. It is the difference between the model re-planning
(which the guard in B rewards) and the model repeating itself (which the guard
in B must eventually stop). Change B without change C raises the abort rate.

### D. Stub the non-streaming path

`Agent::run` and `Session::run` are unreachable from the CLI. Rather than
leave two implementations to drift, mark them deprecated and stub the body.

For `Agent::run` (`agent.rs:497`) and `Session::run` (`session/mod.rs:381`):

```rust
#[deprecated(
    note = "the non-streaming agent loop is unused; use `run_streaming`. \
            See plans/029-tool-failure-recovery.md"
)]
pub async fn run(&mut self, _user_input: &str) -> Result<TurnSummary, AgentError> {
    Err(AgentError::Deprecated {
        message: "Agent::run is deprecated; use Agent::run_streaming".into(),
    })
}
```

Add the variant:

```rust
/// A deprecated code path was called.
Deprecated {
    /// What was called and what to use instead.
    message: String,
},
```

with `Display`: `deprecated: {message}`. This is the right shape: the failure
is loud, greppable, and typed — a stubbed path that returned a plausible empty
`TurnSummary` would be exactly the silent-success defect this plan exists to
remove.

### D1. The CLI must distinguish failure from success first

`run_turn` (`main.rs:706-757`) currently returns `Option<TurnSummary>`, and
`None` means *both* "the channel closed" (~712) and "an error event arrived"
(~749). Before stubbing anything, give it room to report a third outcome, or
the stub's error cannot surface. Assert the consequence explicitly: after D
lands, driving the CLI down a non-streaming path must print a deprecation
error and stop, never print a turn footer.

Do not collapse the two existing `None` returns into one without saying which
is which; if you refactor, name them (`TurnOutcome::{Complete, Aborted,
Interrupted}` or similar) and justify the change in the commit body.

### E. What must not change

- `StreamEvent::Error` keeps its meaning as a turn-ending abort.
- `MAX_TOOL_CALLS_PER_TURN` stays at 100 and stays orthogonal to the new
  guard. Do not fold one into the other.
- `ToolCallCacheLookup` and the read-only dedup path are untouched in
  behaviour. `test_duplicate_read_only_tool_call_is_deduplicated` must still
  pass unchanged.
- The streaming request/response/logging shape in `Session::run_streaming` is
  untouched.
- No prompt text changes. The system prompt already specifies recoverable
  errors; this plan makes the harness match it.

## Work order, test-first

Write the failing tests first, then implement A, B, C, D.

### New tests in `agent.rs`

1. `failed_tool_call_is_fed_back_and_turn_continues` — the regression test for
   the observed failure. Script: a tool call that fails, then a text turn.
   Assert `run_streaming` yields a `ToolResult` (not `Error`), then
   `TurnComplete`, and that the turn's tool-result message carries the failure
   text. **This must fail before A lands.**
2. `failed_tool_call_is_not_reported_as_completion` — the defect-3 pin. After a
   failed call, assert `pending_tool_calls.is_empty()` is *false*: the turn
   does not fall into the "no tool calls were made" branch. Construct so that
   without the fix the run returns a `TurnComplete` carrying the failure as
   success.
3. `distinct_failures_do_not_trip_the_guard` — three *different* failing calls
   in sequence; assert the turn continues and no guard error is raised.
   This pins the "a different failure is progress" rule.
4. `repeated_identical_failure_trips_the_guard` — the same failing call
   `REPEATED_FAILURE_LIMIT` times; assert `AgentError::RepeatedToolFailure`
   with the right `count`.
5. `failure_count_resets_after_a_success` — fail, then the same call succeeds,
   then fail again; assert the guard does not fire.
6. `write_does_not_clear_failure_counts` — a failing call, a successful write,
   then the same failing call repeated; assert the guard still fires. Pins the
   deliberate asymmetry with the read cache.
7. `deprecated_run_returns_deprecated` — `Agent::run` returns
   `AgentError::Deprecated`.

### New tests in `session/mod.rs`

8. `deprecated_run_returns_deprecated` — the `Session::run` equivalent.
9. Re-point the existing test at `session/mod.rs:1254` (which calls
   `Session::run` for compaction logging) at `run_streaming`, so the
   compaction-before-prompt ordering assertion keeps its coverage. The
   behaviour it checks (`compaction_idx + 1 == prompt_idx`) is still valid on
   the streaming path.

### New tests in `tool.rs`

10. `git_branch_refusal_names_an_admitted_alternative` — assert the refusal
    text contains a specific admitted command (`git log`).
11. `git_tag_refusal_names_an_admitted_alternative` — same for `tag`.

### Existing tests to re-check

- `test_unknown_tool_fails` asserts `matches!(err, AgentError::Tool { .. })`
  for an **unknown tool name**. Decide deliberately and state the decision:
  an unknown tool name is a harness/dispatch error, not an execution failure,
  so it should stay fatal. If you keep it fatal, keep `AgentError::Tool`
  reachable and this test unchanged. Do not make it recoverable by accident.
- `test_tool_call_limit_enforced` must still pass; the new guard is orthogonal.
- `test_duplicate_read_only_tool_call_is_deduplicated` must pass untouched.

### CLI test

12. Drive `run_turn` with a stubbed non-streaming path and assert it reports a
    failure rather than a completed turn. If the CLI has no test harness for
    this, add the assertion at the lowest level that already has one
    (`main.rs` is a binary; prefer asserting the `TurnSummary`-vs-failure
    distinction at the library boundary and note the gap in the commit body).

## Verification

Before committing:

- `cargo fmt`
- `cargo test`
- `cargo build`
- `cargo clippy -- -D warnings`

The workspace convention is small, descriptive commits. Never push.

## Commit message

Subject, imperative, capitalized, no final period, ≤72 characters, then a
blank line, then a body wrapped at 72 explaining context and reasoning rather
than implementation. End with:

```
Hānihi
```
