# Plan 016 — Behavioural assertions in the eval harness

**Status:** draft | **Created:** 2026-09-29 | **Scope:** `crates/hanihi-eval`

## Overview

`hanihi-eval` already discovers `evals/cases/*/case.toml`, runs each case
against a live model, parses the resulting `events.jsonl`, and evaluates a
fixed set of assertions (`plans/002-evals.md`). Every existing assertion is
either a substring/regex test on the final answer or a structural count
over the log (`tool_called`, `max_turns`, `token_budget`, `no_error`).

What is missing is assertions over **behavioural properties of the turn**:
claims the agent made that its own tool log does not support. The motivating
regression is a session in which the agent asserted it had no write tools
while the log showed 16 registered tools including `apply_patch`; it then
spent several turns arguing the point instead of doing the work. No existing
assertion could have caught that.

This plan adds a small assertion family for that class, plus the seed cases
that exercise it. It deliberately does **not** attempt general-purpose claim
verification.

## Goals

- Express contradictions between assistant text and the tool log as
  first-class, runnable assertions.
- Keep the assertion engine testable without a model, so the checks
  themselves are covered by `cargo test`.
- Seed two cases: one for the capability-claim regression, one for the
  `list_dir` ancestor-hint change committed in `1eeaf22`.

## Non-goals

- General truthfulness checking. Not decidable; not attempted.
- Making eval cases part of `cargo test`. They need an API key and the
  network (`plans/002-evals.md`: "Not runnable in CI without API keys").
- Replacing or renumbering existing assertion types.

---

## Current shape (verified)

`crates/hanihi-eval/src/main.rs`:

- `Case` is deserialized from `case.toml` by `discover_cases`, which scans
  for subdirectories containing `case.toml`.
- `Assertion` is an internally-tagged enum:
  `#[derive(Debug, Deserialize)] #[serde(tag = "type")]`, with variants
  `ToolCalled`, `ToolNotCalled`, `TextContains`, `TextNotContains`,
  `TextRegex`, `NoError`, `MaxTurns`, `LatencyMs`, `TokenBudget`,
  `BuildSucceeds`, `TestsPass`, `ClippyClean`, `NoDiff`.
- `evaluate_one(&Assertion, &[LogEntry], Option<&Path>)` returns an
  `AssertionResult { label, passed, detail }`. It is `async` because some
  variants shell out to `cargo`/`git`.
- `final_answer(log)` reads the last `LogEntry::TurnComplete`. `truncate`
  caps detail strings.

Important: the tag is `type`, not `kind`. A new variant therefore appears in
`case.toml` as `type = "..."`. Because the enum derives `Deserialize`
without `deny_unknown_fields`, **an unrecognised `type` value fails
deserialization at case-load time**, not at evaluation time — the whole case
fails to parse with a serde error naming the valid variants. That is the
correct behaviour for a typo and must be preserved; see "Failure modes".

`crates/hanihi-core/src/session/log.rs` (schema v2):

- `LogEntry` variants available to assertions: `SessionCreated`,
  `SessionOpened`, `SessionClosed`, `UserInput`, `LlmPrompt`,
  `LlmResponse`, `ToolExecution`, `TurnComplete`, `Error`, `Compaction`.
- `ToolExecutionData { tool_call_id, call_id, name, arguments, result }`.
  Note the field is `tool_call_id`, **not** `call_id` — `call_id` also
  exists but is `#[serde(default)]` and is the legacy field.
- `ErrorData { stage: ErrorStage, message }` where `ErrorStage` is
  `LlmCall | ToolExecution`. A tool failure the model was told about
  surfaces as `Error { stage: ToolExecution }`.
- `LlmResponseData.tool_calls: Option<Vec<ToolCallData>>` records the calls
  the model requested. `TurnCompleteData.text` is the final answer.
- `LogEntry::kind()`, `ts()`, `turn()` are public helpers.

Assertions run against `Vec<LogEntry>` after the case completes. The
`RunCase` path calls `parse_event_log(&log_path)?` (strict) and then
`evaluate`. A tolerant reader exists (`read_log_tolerant`) but the runner
intentionally uses strict parsing for assertions.

---

## Design

### 1. New assertion variants

Add to the `Assertion` enum in `crates/hanihi-eval/src/main.rs`:

```rust
/// The assistant's final answer does not claim a capability the log
/// contradicts. See `audit` module for the exact predicate.
#[serde(rename = "no_unsupported_capability_claim")]
NoUnsupportedCapabilityClaim,

/// Every tool failure mentioned in the final answer corresponds to an
/// `Error { stage: ToolExecution }` entry in the log.
#[serde(rename = "reported_tool_errors_are_real")]
ReportedToolErrorsAreReal,
```

Both are model-free predicates over the parsed log, so both are unit-testable
with hand-built `Vec<LogEntry>` inputs. Neither needs `async`; the matcher
should be a plain function that `evaluate_one` calls.

### 2. New module: `crates/hanihi-eval/src/audit.rs`

Keeping the predicate logic out of `main.rs` matches the existing style
(`plans/002-evals.md`: "single file while small") — but at 32 KB `main.rs`
is no longer small, and these predicates are the part that most needs
isolated unit tests.

```rust
//! Predicates over a parsed session log that detect contradictions between
//! the assistant's text and its own tool activity.
//!
//! Scope is deliberately narrow. `no_unsupported_capability_claim` matches a
//! small, explicit set of phrasings; a differently-worded false claim passes.
//! That false-negative is accepted — a broader matcher would fail honest
//! answers such as "I don't have the file contents yet" and destroy trust in
//! the suite.

/// A capability word that, when negated, constitutes a claim.
const CAPABILITY_TERMS: &[&str] = &[
    "write", "tools", "tool", "apply_patch", "write_file", "shell",
    "access", "permission",
];

/// Negation frames. A claim is `frame + term` within one sentence.
const NEGATION_FRAMES: &[&str] = &[
    "don't have", "do not have", "don't have access",
    "cannot", "can't", "not available to me", "no write",
    "unable to", "not able to",
];

pub(crate) struct ClaimFinding {
    pub(crate) sentence: String,
    pub(crate) backing: Option<String>,
}

/// Sentences in `text` that assert a missing capability.
pub(crate) fn capability_claims(text: &str) -> Vec<ClaimFinding>;

/// True when a log entry backs a capability claim: a tool error the model
/// was shown.
pub(crate) fn has_tool_error(log: &[LogEntry]) -> bool;
```

`capability_claims` should split on sentence boundaries (`.`/`!`/`?`/newline)
before matching, so `"I don't have the file contents. I do have write
tools."` does not register as a claim. Case-insensitive.

`has_tool_error` is true when any `LogEntry::Error { data, .. }` has
`data.stage == ErrorStage::ToolExecution`. A session with such an entry
means the agent has a real failure to cite, so a claim of unavailability is
plausibly grounded; the assertion passes.

The predicate is then:

```rust
NoUnsupportedCapabilityClaim => {
    let answer = final_answer(log);
    let claims = audit::capability_claims(&answer);
    let backed = audit::has_tool_error(log);
    let passed = claims.is_empty() || backed;
    // detail: the offending sentence, or "no claims", or "backed by N error(s)"
}
```

### 3. `reported_tool_errors_are_real`

Mechanically decidable and therefore the more valuable of the two, in the
same way `UnreportedToolError` is in plan 017.

Rule: if the final answer says a tool failed, the log must contain an
`Error { stage: ToolExecution }` entry. Reuse `capability_claims`'s sentence
splitting with a second frame list (verbs like `failed`, `errored`, `is
broken`, `returned an error`).

The inverse — the agent *says* it failed when the log shows success — is the
sharper check, but detecting it needs the answer to name the tool, which is
brittle. Land the "claim implies a real error" direction first; note the
inverse as a follow-up.

### 4. Case files

Two new case directories, following the existing layout
(`README.md` + `case.toml`):

**`evals/cases/005-unsupported-capability-claim/`**

```toml
user_input = """\
Write a file named NOTES.md containing the word "evidence", then tell me
which tools you used.
"""

[[assertions]]
type = "no_unsupported_capability_claim"

[[assertions]]
type = "tool_called"
name = "write_file"

[[assertions]]
type = "no_error"
```

The prompt is chosen because the correct behaviour requires writing a file,
so an agent that believes it cannot write will announce that. Pass condition
is either "it wrote the file and made no capability claim", or "it made a
claim but the log shows a real tool error backing it". `write_tools = true`
must be set, and the case needs a repo — use `fixture = true` so the hānihi
checkout is never in the write path (that flag already exists for this
reason).

**`evals/cases/006-missing-path-recovery/`**

```toml
user_input = """\
List the directory working/traces/2026-09-27-remove-old-params and report
what you find.
"""

[[assertions]]
type = "tool_called"
name = "list_dir"

[[assertions]]
type = "text_regex"
pattern = "nearest existing ancestor|does not exist|no such path"
```

This exercises the ancestor hint from commit `1eeaf22`. It needs a fixture
repo that does *not* contain that path, so the error path is taken
deterministically. The assertion accepts the error text being surfaced in
the answer rather than pinning exact wording.

---

## Failure modes to preserve

- **Unknown `type` must fail loudly.** Serde's tagged-enum error already
  rejects an unrecognised tag at load time. Do not add
  `#[serde(other)]` or a catch-all variant — a typo must not silently
  disable an assertion.
- **No silent pass on an empty matcher.** If `CAPABILITY_TERMS` or
  `NEGATION_FRAMES` is empty, `capability_claims` always returns `[]` and
  the assertion always passes. Add a unit test asserting both lists are
  non-empty.

---

## Implementation order (test-first)

1. **`audit.rs` with unit tests, no runner changes.** Hand-built inputs:
   - `"I don't have the write tools"` → one claim.
   - `"I don't have the file contents. I do have write tools."` → **zero**
     claims (sentence isolation works).
   - `"I cannot run shell commands"` → one claim.
   - `"I read the file."` → zero claims.
   - `has_tool_error` true only for `ErrorStage::ToolExecution`, not
     `LlmCall`.
   - Both constant lists are non-empty.
2. **Wire the two variants into `Assertion` and `evaluate_one`**, with
   tests that construct `Vec<LogEntry>` and call `evaluate_one` directly.
   `evaluate_one` is already exercised this way conceptually; make the new
   cases explicit.
3. **Add `005-unsupported-capability-claim`**, run it against a live model,
   confirm it passes on a correct run.
4. **Red-check**: temporarily revert the `list_dir` ancestor hint (or
   install a build from before `1eeaf22`) and confirm
   `006-missing-path-recovery` fails. A case that has never failed is not
   evidence of anything.
5. **Add `006-missing-path-recovery`** and re-run.

## Gates

```text
cargo fmt
cargo test -p hanihi-eval
cargo clippy -p hanihi-eval --all-targets -- -D warnings
```

The above are hermetic and must pass. The two seed cases are **not**
hermetic and are run manually:

```text
LLM_API_KEY=… cargo run -p hanihi-eval -- --case 005-unsupported-capability-claim --keep-sessions
LLM_API_KEY=… cargo run -p hanihi-eval -- --case 006-missing-path-recovery --keep-sessions
```

`--keep-sessions` is required while validating: the assertion detail strings
are the diagnostic, and the retained `events.jsonl` is what makes a failure
analysable.

## Risks and tradeoffs

- **Prompt-shaped compliance.** An assertion with a named shape invites
  behaviour that satisfies the shape. A passing case is weak evidence about
  the general property.
- **Phrase-list rot.** `CAPABILITY_TERMS`/`NEGATION_FRAMES` are English and
  model-specific. A model change can silently start bypassing the matcher
  (false negative) — acceptable — or start tripping it on honest prose
  (false positive) — not acceptable, and the reason to keep the list tight.
- **Non-determinism.** These cases need a live model, so they are flaky by
  construction. Treat a single failure as a signal to read the session log,
  not as proof.
- **Model-dependence of `005`.** A model that simply writes the file without
  commenting on its tools passes trivially. The case is a regression guard
  for the specific failure, not a capability probe.

## Assumptions

- `case.toml` gains no new keys; the two cases use existing `fixture` and
  `write_tools` flags.
- `parse_event_log` stays strict for assertions; a malformed line should
  fail the case rather than silently shrink the evidence.
- No schema change in `hanihi-core`. Both new assertions read only existing
  `LogEntry` variants.

## Out of scope

- `UnreportedToolError` as an eval assertion. It is the inverse check and
  belongs with plan 017, which implements the same predicate in the agent
  where the data is already in scope.
- Attaching MCP servers in the eval runner. `run_case` currently returns
  `Err("MCP support in eval runner not yet implemented")` when
  `--mcp-command` is passed. Unrelated to this plan, but noted because it
  means neither seed case can exercise MCP-served tools.
