# 004 — Harness self-audit for unsupported claims

You are Hānihi. This is a prompt to a future session. Its job is to detect, at
runtime, the same contradiction `003` detects off-line: a turn whose assistant
text claims something the turn's own tool activity does not support.

This prompt is self-contained in the sense that every file it names exists
today. It is **not** independent of `003`: it reuses `003`'s phrase lists
rather than inventing a second copy. Land `003` first. If `003` has not
landed, stop and say so rather than duplicating the matcher — one definition
of "capability claim" across the eval harness and the agent is the entire
point of the ordering.

Scope: `crates/hanihi-core` (`agent.rs`) + `crates/hanihi-cli`.

## Context

The agent can contradict its own tool log in its final answer: assert that a
tool does not exist, or that a tool call failed, when the log shows the
opposite. In the motivating session the agent claimed it had no write tools
while the session log recorded 16 registered tools including `apply_patch`; it
then argued the point across several turns instead of doing the work.

The information needed to detect this is already in scope inside the agent
loop: the turn's assembled text, the tool calls it made, and the results and
errors it received.

This plan adds a **diagnostic** check at the turn boundary. It never fails a
turn.

## Goals

- Detect, mechanically, two named contradictions between a turn's text and its
  own tool activity.
- Emit the finding as a stream event and expose a count on `TurnSummary`.
- Keep the predicate pure and unit-testable with no model and no I/O.

## Non-goals (stated explicitly)

- **Not a truthfulness checker.** It catches two specific, enumerated
  contradictions. Never describe or rely on it as general claim verification.
- **Never fails a turn.** A heuristic must not be able to kill a run. That
  would be a worse outcome than the bug it detects.
- **No new prompt-only mitigation.** Prompt text was already added to
  `DEFAULT_SYSTEM_PROMPT`; this is the mechanical backstop for when prompt text
  does not fire.

## Read first, verbatim

- `crates/hanihi-core/src/agent.rs`
  - `run_streaming_loop` (~871). It accumulates `text_buf`, `pending_tool_calls:
    Vec<ToolCall>`, `pending_results: Vec<(ToolCall, String)>`, and on the
    terminal path (`pending_tool_calls.is_empty()`) builds `TurnComplete {
    summary }` (~1187).
  - The tool-failure arm (~1079): since archived `029` parts A–C landed, a
    failed tool execution is rendered with `TOOL_FAILURE_PREFIX` and pushed
    into `pending_results` / `pending_tool_calls` rather than aborting the
    turn. **This matters** — it is what makes `UnreportedToolError` observable
    at all. Confirm it is still the case before designing around it.
  - `REPEATED_FAILURE_LIMIT` (~124), `TOOL_FAILURE_PREFIX` (~128),
    `ToolCallCache` (~147) including its `failures` map (~147-183).
  - `StreamEvent` (~243) and `StreamEvent::type_name` (~325). **The match is
    exhaustive**, so a new variant forces every consumer to compile-fail until
    updated.
  - `TurnSummary` (~218): public fields `text`, `tool_calls`, `usage`,
    `final_history`, `final_summary`.
  - `Agent::run` (~580) — the non-streaming path. Archived `029` part D (stub
    it) did **not** land, so it is still live and must be handled.
- `crates/hanihi-core/src/session/log.rs` (schema v2):
  - `SCHEMA_VERSION = 2`. Policy in the module doc: *additive changes (a new
    optional field with `#[serde(default)]`) do not bump the version; breaking
    changes bump it and add a migration.*
  - `parse_entry_line` rejects a line whose `schema` exceeds `SCHEMA_VERSION`
    ("schema N is newer than supported schema M"), strictly and tolerantly.
    **This constraint decides §4.**
- `crates/hanihi-eval/src/audit.rs` — the module `003` created. Your phrase
  lists are copies of its constants, not new ones.

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
/// An enum, not a bool: more checks are expected to accrete, and callers that
/// want to fail a build on one specific kind need to match on it.
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

Place it in a new `crates/hanihi-core/src/audit.rs`, or in `agent.rs` beside
the loop if the module would be trivially thin. Either way: no `self`, no I/O,
`fn` not `async fn`.

```rust
/// Detect contradictions between a turn's final text and its tool activity.
///
/// Pure and total: no panics, no I/O, no model. Returns one finding per
/// distinct kind detected, not per occurrence.
///
/// Phrase lists are deliberately narrow and are duplicated from
/// `hanihi-eval`'s `audit.rs` by design (see `plans/001-index.md`): a broad
/// matcher fails honest prose and gets ignored.
pub(crate) fn audit_turn(
    text: &str,
    calls: &[ToolCall],
    failures: &[ToolCall],
) -> Vec<SelfAuditFinding>;
```

Matching rules, deliberately narrow — same reasoning as `003`:

- Split `text` into sentences on `.`/`!`/`?`/newline before matching, so
  `"I don't have the write tools."` is isolated from surrounding text.
- `UnsupportedCapabilityClaim`: a negation frame adjacent to a capability term
  within one sentence, **and** zero entries in `failures`. If the turn has a
  genuine failure, a claim of unavailability is plausibly grounded and the
  check stays silent.
- `UnreportedToolError`: the sentence references a tool name from `calls` or
  from a known tool-name list, using a failure verb, **and** that name is
  absent from `failures`.
- Case-insensitive throughout.

**Copy the phrase lists from `crates/hanihi-eval/src/audit.rs` verbatim.** Do
not improve them here. If you find the two need to diverge, that is a design
change — stop and report it instead of forking the definition silently.

### 3. Wiring into both loops

**`run_streaming_loop`**, on the terminal path, immediately before building
`TurnComplete`:

```rust
let findings = audit_turn(&text_buf, &pending_tool_calls, &failures);
for finding in &findings {
    let _ = tx.send(StreamEvent::SelfAudit { finding: finding.clone() }).await;
}
```

and record the count on the summary. This placement matters: it runs after the
text is final (no partial deltas to misjudge) and before the caller sees
`TurnComplete`, so a consumer that stops reading on `TurnComplete` still
receives the audit first.

**`Agent::run`**, symmetrically, on its terminal path, before `commit_turn`.

### 4. Persistence — and the schema problem

Two candidate designs, and the choice is forced by `parse_entry_line`'s
future-schema rejection.

**Option A — add a `self_audit` kind to `LogEntry` and bump `SCHEMA_VERSION`
to 3.**

Bumping the version is the module's stated policy for anything but an additive
optional field, and a new enum variant is not a field. The cost is real: a log
written with `schema: 3` is **rejected outright** by any older binary,
including the currently installed `analyse` and `hanihi-session-analyser` —
not skipped, rejected. Under tolerant reads it degrades to "line skipped, error
reported", which is survivable; under strict reads it fails the read.

**Option B — do not persist a new kind at all.**

The finding is already in the stream event, and the CLI can print it. The
session log records the same information indirectly: the offending text is in
`turn_complete`, and the tool activity is in `tool_execution`. An off-line
reader can apply the identical predicate to those entries without a new event
kind or a schema bump. That is exactly what `003`'s eval assertions do.

**Decision: Option B.** Persisting findings as first-class log entries is a
separate change with a schema-version cost that should be taken deliberately,
not as a side effect of adding a diagnostic. If first-class persistence is
wanted later, it is Option A plus a migration note — and at that point the
v2 → v3 transition belongs in the framework archived `008` established, not
ad hoc.

Do not bump `SCHEMA_VERSION` in this plan.

### 5. Surfacing

| Consumer | Change |
|---|---|
| `StreamEvent::SelfAudit` | New variant; `type_name()` updated |
| CLI (`crates/hanihi-cli/src/main.rs`) | Match arm printing a warning line, visually distinct from `Error`; never changes the exit status |
| `TurnSummary` | New field `self_audit_findings: usize` |
| Session log | **Unchanged** (Option B) |

`TurnSummary` is public and constructed by callers, so adding a field is a
breaking change for any external constructor. Grep for `TurnSummary {` before
editing; `agent.rs` itself has construction sites plus test fixtures.
`crates/hanihi-core/src/agent.rs:~1373` already does
`agent.set_history(summary.final_history.clone())` and
`agent.set_summary(summary.final_summary.clone())` — that loop is the model for
how a `TurnSummary` field flows outward.

## Work order, test-first

Write the failing tests first, then implement.

1. **`audit_turn` unit tests, no loop changes.** Mirror `003`'s audit tests so
   the two matchers are pinned to identical inputs:
   - `"I don't have the write tools"` with no failures → one
     `UnsupportedCapabilityClaim`.
   - The same text **with** a failing call in `failures` → zero findings.
   - `"I don't have the file contents. I do have write tools."` → zero
     findings (sentence isolation).
   - `"read_file failed"` where `read_file` is not in `failures` → one
     `UnreportedToolError`.
   - `"I read the file."` → zero findings.
   - Both phrase lists non-empty (guards against a matcher that silently always
     returns `[]`).
2. **`SelfAudit` event emission**, using the `MockCompletionModel` pattern
   already proven by `test_tool_call_limit_enforced`. Assert the event is sent
   before `TurnComplete`.
3. **Non-fatality**: a turn that produces a finding still returns
   `Ok(TurnSummary)` with the finding counted. **This is the load-bearing
   test** — it pins the non-goal.
4. **CLI printing.** Warning line only; assert nothing about exit codes.

## Gates

```text
cargo fmt --check
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test --workspace` matters here specifically: `StreamEvent` is
exhaustively matched in `crates/hanihi-cli`, so a missing arm is a compile
error, and `TurnSummary`'s new field surfaces in every constructor.

## Acceptance criteria

- `audit_turn` is pure, total, unit-tested with no model, and its phrase lists
  are byte-identical to `crates/hanihi-eval/src/audit.rs`'s.
- A turn that contradicts itself emits `StreamEvent::SelfAudit` **before**
  `TurnComplete`, and the turn still completes successfully.
- `TurnSummary` carries `self_audit_findings`; the CLI prints a warning and
  does not change its exit status.
- `SCHEMA_VERSION` is still `2`; no new `LogEntry` variant exists.
- No new dependencies.
- All gates clean, `--workspace` included.

## Risks and tradeoffs

- **False positives are the real risk.** A misfiring audit trains the reader to
  ignore it. Mitigation: the narrow phrase lists, sentence isolation, and the
  "silent when the turn has a real failure" rule.
- **The lists are duplicated by design and can drift.** `003` owns them;
  `004` copies them. There is no compile-time link between the two crates. If
  you find yourself changing one, change both in the same change and say so —
  and if that starts to happen often, promoting the lists into `hanihi-core`
  and having `hanihi-eval` depend on them is the right follow-up. Do not do
  that promotion speculatively in this plan.
- **`UnreportedToolError` is now observable, but only just.** It depends on
  archived `029` part A having landed (failed tool calls are recorded and the
  turn continues). Verify that before relying on the test, and say in the
  commit body whether you confirmed it.
- **Narrow matching means false negatives.** A differently-worded false claim
  passes. Accepted; document it on the function.
- **`TurnSummary` field addition is a public API change.** Justified by the
  goal but must be called out in the commit message.
- **No log persistence** means post-hoc analysis depends on re-running the
  predicate rather than reading a recorded finding. Deliberate trade against
  the schema bump; revisit if forensic value turns out to matter more.

## Out of scope

- **Persisting audit findings as `LogEntry` variants** (Option A; see §4).
- **Stubbing the non-streaming path** (archived `029` part D). `Agent::run`
  stays live; you are adding a finding to it, not removing it. That stub is a
  separate, unresolved change — see `plans/001-index.md`.
- **Eval-based assertions on the same predicate.** That is `003`, already
  landed by the time you start.
- **Any change to the log schema, the streaming request/response shape, or
  `Session::run_streaming`'s logging.**
- **Changing tool-error handling.** Archived `029` settled it; do not reopen.

## Commit message

Subject, imperative, capitalized, no final period, ≤72 characters, then a blank
line, then a body wrapped at 72 explaining context and reasoning rather than
implementation. Note the `TurnSummary` API change, whether you confirmed the
`029` part A prerequisite, and that `SCHEMA_VERSION` is unchanged. End with:

```
Hānihi
```

Use `git commit -F working/commit-msg-004.txt` if `002` has landed; the body
above the closing line is the file's contents.
