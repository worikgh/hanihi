# Plan 017 — Harness self-audit for unsupported claims

**Status:** draft | **Created:** 2026-09-29 | **Scope:** `crates/hanihi-core` (+ CLI, session)

## Overview

The agent can contradict its own tool log in its final answer: assert that a
tool does not exist, or that a tool call failed, when the log shows the
opposite. In the motivating session the agent claimed it had no write tools
while the session log recorded 16 registered tools including `apply_patch`;
it then argued the point across several turns instead of doing the work.

The information needed to detect this is already in scope inside the agent
loop: the turn's assembled text, the tool calls it made, and the results and
errors it received. This plan adds a **diagnostic** check at the turn
boundary. It never fails a turn.

## Goals

- Detect, mechanically, two named contradictions between a turn's text and
  its own tool activity.
- Emit the finding as a stream event, persist it in the session log, and
  expose a count on `TurnSummary`.
- Keep the predicate pure and unit-testable with no model and no I/O.

## Non-goals (stated explicitly)

- **Not a truthfulness checker.** It catches two specific, enumerated
  contradictions. It must never be described or relied upon as general claim
  verification.
- **Never fails a turn.** A heuristic must not be able to kill a run. That
  would be a worse outcome than the bug it detects.
- **No new prompt-only mitigation.** Prompt text was already added to
  `DEFAULT_SYSTEM_PROMPT`; this is the mechanical backstop for when prompt
  text does not fire.

---

## Current shape (verified)

`crates/hanihi-core/src/agent.rs`:

- `run` (non-streaming) accumulates `turn_messages: Vec<Message>` and
  `tool_calls_total`, and returns `TurnSummary` via `self.commit_turn(...)`.
- `run_streaming_loop` mirrors that shape: it accumulates `text_buf`,
  `pending_tool_calls: Vec<ToolCall>`, and `pending_results: Vec<(ToolCall,
  String)>`, then on the terminal path builds `TurnComplete { summary }`.
- On a tool failure the streaming loop sends
  `StreamEvent::Error { message }` and returns `Err(e)` immediately, so a
  failed tool call currently ends the turn rather than being recorded and
  continued. **This matters**: `UnreportedToolError` as specified below
  cannot observe a turn that aborted. See "Risks" — this plan does not
  change that behaviour, and the check must be written to be correct under
  it.
- `StreamEvent` is a public enum with a `type_name()` method; every variant
  is currently listed there, so a new variant must be added to that match.
- `TurnSummary` is public with public fields (`text`, `tool_calls`,
  `usage`, `final_history`, `final_summary`).

`crates/hanihi-core/src/session/log.rs` (schema v2):

- `SCHEMA_VERSION = 2`. Policy in the module doc: *additive changes (a new
  optional field with `#[serde(default)]`) do not bump the version; breaking
  changes bump it and add a migration.*
- `LogEntry` is `#[serde(tag = "kind")]`, i.e. internally tagged, with
  `kind()` returning the wire tag.
- `ErrorStage` is `LlmCall | ToolExecution`.
- `parse_entry_line` rejects a line whose `schema` exceeds `SCHEMA_VERSION`
  ("schema N is newer than supported schema M"), strictly and tolerantly.
  This is the constraint that decides the design in §4.

---

## Design

### 1. A new stream event

```rust
/// A turn whose assistant text made a checkable claim that the turn's own
/// tool activity does not support. Diagnostic only: never fails the turn.
SelfAudit { finding: SelfAuditFinding },
```

`SelfAuditFinding` is a public struct in `agent.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct SelfAuditFinding {
    /// Which contradiction was detected.
    pub kind: SelfAuditKind,
    /// The offending sentence, verbatim, for the human reading the log.
    pub detail: String,
}

/// The enumerated contradictions this audit can detect.
///
/// An enum, not a bool: more checks are expected to accrete, and callers
/// that want to fail a build on one specific kind need to match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfAuditKind {
    /// Claimed a tool was unavailable with no tool error to cite.
    UnsupportedCapabilityClaim,
    /// Claimed a tool failed, with no failing tool result in the turn.
    UnreportedToolError,
}
```

`type_name()` gains `Self::SelfAudit { .. } => "SelfAudit"`.

### 2. The predicate — a pure function

Lives in `agent.rs` next to the loop, or in a new
`crates/hanihi-core/src/audit.rs` if the phrase lists make `agent.rs` feel
crowded. Either way it takes no `self`, does no I/O, and is `fn`, not
`async fn`:

```rust
/// Detect contradictions between a turn's final text and its tool activity.
///
/// Pure and total: no panics, no I/O, no model. Returns one finding per
/// distinct kind detected, not per occurrence.
pub(crate) fn audit_turn(
    text: &str,
    calls: &[ToolCall],
    failures: &[ToolCall],
) -> Vec<SelfAuditFinding>;
```

`failures` is the list of calls whose result was an error. Under the current
loop this is always empty on a terminal path (a tool error aborts the turn),
so `UnreportedToolError` initially fires for the "named a tool it never
called" case rather than the "said it failed when it succeeded" case. That
limitation is honest and must be documented on the function, not hidden.

Matching rules, deliberately narrow (same reasoning as plan 016: a broad
matcher fails honest prose and gets ignored):

- Split `text` into sentences on `.`/`!`/`?`/newline before matching, so
  `"I don't have the write tools."` is isolated from surrounding text.
- `UnsupportedCapabilityClaim`: a negation frame adjacent to a capability
  term within one sentence, **and** zero entries in `failures`. If the turn
  has a genuine failure, a claim of unavailability is plausibly grounded and
  the check stays silent.
- `UnreportedToolError`: the sentence references a tool name from `calls`
  or from a known tool-name list, using a failure verb, **and** that name is
  absent from `failures`.
- Case-insensitive throughout.

### 3. Wiring into both loops

**`run_streaming_loop`**, on the terminal path (the `pending_tool_calls
.is_empty()` branch), immediately before building `TurnComplete`:

```rust
let findings = audit_turn(&text_buf, &pending_tool_calls, &[]);
for finding in &findings {
    let _ = tx.send(StreamEvent::SelfAudit { finding: finding.clone() }).await;
}
```

and record the count on the summary. This placement matters: it runs after
the text is final (no partial deltas to misjudge) and before the caller sees
`TurnComplete`, so a consumer that stops reading on `TurnComplete` still
receives the audit first.

**`run`**, symmetrically, on its terminal path, before `commit_turn`.

### 4. Persistence — and the schema problem

There are two candidate designs, and the choice is forced by
`parse_entry_line`'s future-schema rejection.

**Option A — add a `self_audit` kind to `LogEntry` and bump
`SCHEMA_VERSION` to 3.**

Bumping the version is the module's stated policy for anything but an
additive optional field, and a new enum variant is not a field. The cost is
real: a log written with `schema: 3` is **rejected outright** by any older
binary, including the currently installed `analyse` and
`hanihi-session-analyser` — not skipped, rejected. Under tolerant reads it
degrades to "line skipped, error reported", which is survivable; under
strict reads it fails the read.

**Option B — do not persist a new kind at all in v1.**

The finding is already in the stream event, and the CLI can print it. The
session log records the same information indirectly: the offending text is
in `turn_complete`, and the tool activity is in `tool_execution`. An off-line
reader can apply the identical predicate to those entries without a new
event kind or a schema bump. That is exactly what plan 016's eval assertions
do.

**Decision: Option B for this plan.** Persisting findings as first-class log
entries is a separate change with a schema-version cost that should be taken
deliberately, not as a side effect of adding a diagnostic. If first-class
persistence is wanted later, it is Option A plus a migration note — and at
that point the v2 → v3 transition should be planned in `plans/008`'s
framework rather than ad hoc.

### 5. Surfacing

| Consumer | Change |
|---|---|
| `StreamEvent::SelfAudit` | New variant; `type_name()` updated |
| CLI (`crates/hanihi-cli/src/main.rs`) | Match arm printing a warning line, visually distinct from `Error`; never changes the exit status |
| `TurnSummary` | New field `self_audit_findings: usize` |
| Session log | **Unchanged** (Option B) |

`TurnSummary` is public and constructed by callers, so adding a field is a
breaking change for any external constructor. Grep for `TurnSummary {`
before editing; `agent.rs` itself has at least two construction sites
(`run`, `run_streaming_loop`) plus test fixtures.

---

## Implementation order (test-first)

1. **`audit_turn` unit tests, no loop changes.** In `agent.rs`'s `mod tests`:
   - `"I don't have the write tools"` with no failures → one
     `UnsupportedCapabilityClaim`.
   - The same text **with** a failing call in `failures` → zero findings.
   - `"I don't have the file contents. I do have write tools."` → zero
     findings (sentence isolation).
   - `"read_file failed"` where `read_file` is not in `failures` → one
     `UnreportedToolError`.
   - `"I read the file."` → zero findings.
   - Both phrase lists non-empty (guards against a matcher that silently
     always returns `[]`).
2. **`SelfAudit` event emission**, using the existing
   `MockCompletionModel` pattern already proven by
   `test_tool_call_limit_enforced`. Assert the event is sent before
   `TurnComplete`.
3. **Non-fatality**: a turn that produces a finding still returns
   `Ok(TurnSummary)` with the finding counted. This is the load-bearing
   test — it pins the non-goal.
4. **CLI printing.** Warning line only; assert nothing about exit codes.

## Gates

```text
cargo fmt
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
```

`cargo test --workspace` matters here specifically: `StreamEvent` is
exhaustively matched in `crates/hanihi-cli`, so a missing arm is a compile
error, and `TurnSummary`'s new field surfaces in every constructor.

## Risks and tradeoffs

- **False positives are the real risk.** A misfiring audit trains the reader
  to ignore it. Mitigation: the narrow phrase lists, sentence isolation, and
  the "silent when the turn has a real failure" rule.
- **`UnreportedToolError` is weak under the current loop.** As noted in §2,
  a tool error aborts the turn, so the check cannot see the case it was
  designed for. If that check is wanted at full strength, the loop must first
  be changed to *record* a tool failure and continue rather than return early
  — which is a behavioural change to tool-error handling and belongs in its
  own plan, not this one.
- **Narrow matching means false negatives.** A differently-worded false
  claim passes. Accepted; documented on the function.
- **`TurnSummary` field addition is a public API change.** Justified by the
  goal but must be called out in the commit message.
- **No log persistence in v1** means post-hoc analysis depends on re-running
  the predicate rather than reading a recorded finding. That is a deliberate
  trade against the schema bump; revisit if forensic value turns out to
  matter more than the version cost.

## Assumptions

- `MockCompletionModel` can script a turn that emits text asserting a
  missing capability; if it cannot, the non-fatality test falls back to
  asserting on `audit_turn` plus a direct construction of the terminal path.
- The CLI's event loop can add a match arm without restructuring.
- No schema change in `hanihi-core` (Option B).

## Out of scope

- Persisting audit findings as `LogEntry` variants (Option A; see §4).
- Changing tool-error handling so the turn continues after a failure.
- Eval-based assertions on the same predicate — that is
  `plans/016-eval-behavioural-assertions.md`, which implements the matching
  half of this design off-line over `events.jsonl`.

## Sequencing

`plans/016` first. It gives the property a pass/fail signal that costs
nothing to run and does not touch the agent. This plan then adds the runtime
warning, and its phrase lists should be copied from `016`'s
`audit.rs` rather than invented twice — one definition of "capability claim"
across the eval harness and the agent, so the two cannot drift.
