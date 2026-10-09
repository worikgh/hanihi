# 003 — Behavioural assertions in the eval harness

You are Hānihi. This is a prompt to a future session. Its job is to make the
eval harness able to express contradictions between what the agent *said* and
what its own tool log *shows*.

This prompt is self-contained. Implement it even if `002` has not landed.
Do **not** implement anything from `004` here — its phrase lists are supposed
to be copies of yours, so yours must exist first.

Scope: `crates/hanihi-eval` only. No `hanihi-core` change.

## Context

`hanihi-eval` already discovers `evals/cases/*/case.toml`, runs each case
against a live model, parses the resulting `events.jsonl`, and evaluates a
fixed set of assertions. Every existing assertion is either a
substring/regex test on the final answer or a structural count over the log
(`tool_called`, `max_turns`, `token_budget`, `no_error`,
`build_succeeds`, `tests_pass`, `lint_clean`, `no_diff`).

None of them can express: **the agent claimed something its own tool log
contradicts.**

The motivating regression is a session in which the agent asserted it had no
write tools while the log recorded 16 registered tools including
`apply_patch`; it then spent several turns arguing the point instead of doing
the work. The session ran to completion. Every assertion in the suite passed.
Nothing could have caught it.

This plan adds a small assertion family for that class, plus the seed cases
that exercise it. It deliberately does **not** attempt general-purpose claim
verification.

## Goals

- Express two specific contradictions between assistant text and the tool log
  as first-class, runnable assertions.
- Keep the assertion engine testable without a model, so the checks themselves
  are covered by `cargo test`.
- Seed two cases: one for the capability-claim regression, one for the
  `list_dir` ancestor-hint change committed in `1eeaf22`.

## Non-goals

- General truthfulness checking. Not decidable; not attempted.
- Making eval cases part of `cargo test`. They need an API key and the
  network.
- Replacing or renumbering existing assertion types. `lint_clean` /
  `clippy_clean` keep their meaning.

## Read first, verbatim

- `crates/hanihi-eval/src/main.rs`
  - `Case` and `discover_cases` — case loading.
  - The `Assertion` enum: `#[derive(Debug, Deserialize)] #[serde(tag =
    "type")]`, variants `ToolCalled`, `ToolNotCalled`, `TextContains`,
    `TextNotContains`, `TextRegex`, `NoError`, `MaxTurns`, `LatencyMs`,
    `TokenBudget`, `BuildSucceeds`, `TestsPass`, `LintClean` (with
    `#[serde(rename = "lint_clean", alias = "clippy_clean")]`), `NoDiff`.
  - `evaluate_one(&Assertion, &[LogEntry], Option<&Path>) -> AssertionResult`
    where `AssertionResult { label, passed, detail }`. It is `async` because
    some variants shell out to `cargo`/`git`.
  - `final_answer(log)` — reads the last `LogEntry::TurnComplete`.
  - `truncate` — caps detail strings.
  - `parse_event_log` — strict reader used by the runner.
- `crates/hanihi-core/src/session/log.rs` (schema v2):
  - `LogEntry` variants: `SessionCreated`, `SessionOpened`, `SessionClosed`,
    `UserInput`, `LlmPrompt`, `LlmResponse`, `ToolExecution`, `TurnComplete`,
    `Error`, `Compaction`.
  - `ToolExecutionData { tool_call_id, call_id, name, arguments, result }`.
    The field is `tool_call_id`; `call_id` also exists but is
    `#[serde(default)]` and is the legacy field.
  - `ErrorData { stage: ErrorStage, message }` where `ErrorStage` is
    `LlmCall | ToolExecution`.
  - `LlmResponseData.tool_calls: Option<Vec<ToolCallData>>`.
  - `TurnCompleteData.text` — the final answer.
  - `LogEntry::kind()`, `ts()`, `turn()` — public helpers.
- `evals/cases/` — existing case layout. **Note the numbering before you
  start**: `001`–`004` and `006` exist. `005` is free. The case numbers this
  plan uses are deliberately `007` and `008`; see §4.

## Constraints discovered while reading

- **The tag is `type`, not `kind`.** A new variant appears in `case.toml` as
  `type = "..."`.
- **An unrecognised `type` fails at case-load time, not evaluation time.**
  The enum derives `Deserialize` without `deny_unknown_fields`, so an unknown
  tag produces a serde error naming the valid variants. That is the correct
  behaviour for a typo and must be preserved.
- **The runner intentionally uses strict parsing.** A malformed line should
  fail the case rather than silently shrink the evidence. A tolerant reader
  exists for the `analyse` path; do not switch the eval runner to it.

## Design

### 1. New assertion variants

Add to the `Assertion` enum:

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
with hand-built `Vec<LogEntry>` inputs. Neither needs `async`; the matcher is
a plain function that `evaluate_one` calls.

### 2. New module: `crates/hanihi-eval/src/audit.rs`

Keeping the predicate logic out of `main.rs` matches the existing
"single file while small" style — but `main.rs` is no longer small, and these
predicates are the part that most needs isolated unit tests.

```rust
//! Predicates over a parsed session log that detect contradictions between
//! the assistant's text and its own tool activity.
//!
//! Scope is deliberately narrow. `NoUnsupportedCapabilityClaim` matches a
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

/// Verbs that assert a tool call failed.
const FAILURE_FRAMES: &[&str] = &[
    "failed", "errored", "error", "is broken", "returned an error",
    "did not work", "didn't work",
];

pub(crate) struct ClaimFinding {
    pub(crate) sentence: String,
    pub(crate) backing: Option<String>,
}

/// Sentences in `text` that assert a missing capability.
pub(crate) fn capability_claims(text: &str) -> Vec<ClaimFinding>;

/// Sentences in `text` that assert a tool call failed.
pub(crate) fn failure_claims(text: &str) -> Vec<ClaimFinding>;

/// True when a log entry backs a claim: a tool error the model was shown.
pub(crate) fn has_tool_error(log: &[LogEntry]) -> bool;
```

**`capability_claims` splits on sentence boundaries** (`.`/`!`/`?`/newline)
before matching, so `"I don't have the file contents. I do have write
tools."` does not register as a claim. Case-insensitive throughout.

**`has_tool_error`** is true when any `LogEntry::Error { data, .. }` has
`data.stage == ErrorStage::ToolExecution`. A session with such an entry means
the agent has a real failure to cite, so a claim of unavailability is
plausibly grounded and the assertion passes.

### 3. The two predicates

```rust
NoUnsupportedCapabilityClaim => {
    let answer = final_answer(log);
    let claims = audit::capability_claims(&answer);
    let backed = audit::has_tool_error(log);
    let passed = claims.is_empty() || backed;
    // detail: the offending sentence, or "no claims", or "backed by N error(s)"
}

ReportedToolErrorsAreReal => {
    let answer = final_answer(log);
    let claims = audit::failure_claims(&answer);
    let errors = count_tool_errors(log);
    let passed = claims.is_empty() || errors > 0;
    // detail: the offending sentence, or "no failure claims",
    //         or "backed by N tool error(s)"
}
```

`ReportedToolErrorsAreReal` is the mechanically decidable one and therefore
the more valuable of the two.

The **inverse** — the agent says it failed when the log shows success — is
the sharper check, but detecting it needs the answer to name the tool, which
is brittle. Land the "claim implies a real error" direction first. Note the
inverse as a follow-up in the commit body; do not attempt it.

### 4. Case files

Two new case directories, following the existing layout (`README.md` +
`case.toml`).

**The numbers are `007` and `008`, not `005` and `006`.** `evals/cases/` holds
`001`–`004` and `006-cpp-build`; `005` is also claimed by the git-commit case
specified in `plans/archived/complete/007-git-write-tools.md`. Using `005` or
`006` here would collide. If you find `007` or `008` taken by the time you
implement this, take the next free numbers and say so in the commit body.

**`evals/cases/007-unsupported-capability-claim/`**

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

The prompt is chosen because the correct behaviour requires writing a file, so
an agent that believes it cannot write will announce that. Pass condition is
either "it wrote the file and made no capability claim" or "it made a claim
the log backs with a real tool error". Set `write_tools = true`, and use
`fixture = true` so the hānihi checkout is never in the write path — that flag
already exists for this reason.

**`evals/cases/008-missing-path-recovery/`**

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

This exercises the ancestor hint from commit `1eeaf22`. It needs a fixture repo
that does *not* contain that path, so the error path is taken
deterministically. The assertion accepts the error text being surfaced in the
answer rather than pinning exact wording.

## Failure modes to preserve

- **Unknown `type` must fail loudly.** Serde's tagged-enum error already
  rejects an unrecognised tag at load time. Do **not** add `#[serde(other)]`
  or a catch-all variant — a typo must not silently disable an assertion.
- **No silent pass on an empty matcher.** If `CAPABILITY_TERMS`,
  `NEGATION_FRAMES`, or `FAILURE_FRAMES` is empty, the matcher always returns
  `[]` and the assertion always passes. Add a unit test asserting all three
  lists are non-empty.

## Work order, test-first

Write the failing tests first, then implement.

1. **`audit.rs` with unit tests, no runner changes.** Hand-built inputs:
   - `"I don't have the write tools"` → one capability claim.
   - `"I don't have the file contents. I do have write tools."` → **zero**
     claims (sentence isolation works).
   - `"I cannot run shell commands"` → one claim.
   - `"I read the file."` → zero claims.
   - `"the apply_patch call failed"` → one failure claim.
   - `has_tool_error` true only for `ErrorStage::ToolExecution`, not
     `LlmCall`.
   - All three constant lists non-empty.
2. **Wire the two variants into `Assertion` and `evaluate_one`**, with tests
   that construct `Vec<LogEntry>` and call `evaluate_one` directly. Cover the
   pass path (no claims), the pass path (claim backed by a tool error), and
   the fail path (unbacked claim) for both variants.
3. **Add `007-unsupported-capability-claim`**, run it against a live model,
   confirm it passes on a correct run.
4. **Red-check `008`**: temporarily revert the `list_dir` ancestor hint (or
   install a build from before `1eeaf22`) and confirm
   `008-missing-path-recovery` fails. A case that has never failed is not
   evidence of anything. Record the red-run output in the commit body.
5. **Add `008-missing-path-recovery`** and re-run.
6. **Update `README.md`**: add both assertion types to the assertion list at
   the prevailing table/paragraph, and note the two new cases. Keep the edit
   in the file's existing voice.

## Gates

```text
cargo fmt --check
cargo test -p hanihi-eval
cargo clippy -p hanihi-eval --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The above are hermetic and must pass. The two seed cases are **not** hermetic:
they need a live model. Run them only if `LLM_API_KEY` is actually present:

```text
LLM_API_KEY=… cargo run -p hanihi-eval -- --case 007-unsupported-capability-claim --keep-sessions
LLM_API_KEY=… cargo run -p hanihi-eval -- --case 008-missing-path-recovery --keep-sessions
```

`--keep-sessions` is required while validating: the assertion detail strings
are the diagnostic, and the retained `events.jsonl` is what makes a failure
analysable.

**If no API key is present, say so and report both cases as unverified.** Do
not claim a case passed that was not run.

## Acceptance criteria

- Both assertion variants exist, parse from `case.toml` as
  `type = "no_unsupported_capability_claim"` and
  `type = "reported_tool_errors_are_real"`, and are unit-tested without a
  model.
- An unrecognised `type` still fails case loading.
- All three phrase lists are asserted non-empty.
- `008-missing-path-recovery` was confirmed **failing** before the red-check
  was reverted, or the reason it could not be is stated.
- `README.md` documents both new assertion types and both new cases.
- No new dependencies. No `hanihi-core` change. `SCHEMA_VERSION` stays `2`.

## Risks and tradeoffs

- **Prompt-shaped compliance.** An assertion with a named shape invites
  behaviour that satisfies the shape. A passing case is weak evidence about
  the general property.
- **Phrase-list rot.** The lists are English and model-specific. A model change
  can silently start bypassing the matcher (false negative — acceptable) or
  start tripping it on honest prose (false positive — not acceptable, and the
  reason to keep the lists tight).
- **Non-determinism.** These cases need a live model, so they are flaky by
  construction. Treat a single failure as a signal to read the session log,
  not as proof.
- **Model-dependence of `007`.** A model that simply writes the file without
  commenting on its tools passes trivially. The case is a regression guard for
  the specific failure, not a capability probe.
- **`008` may be a weak guard.** If the ancestor hint is robust, the only way
  to make the case red is reverting the code, which is what the red-check does.
  If you conclude `008` cannot fail for any realistic regression, say so in
  the commit body rather than shipping a case that only ever passes.

## Out of scope

- **`UnreportedToolError` as an eval assertion.** It is the inverse check and
  belongs with `004-harness-self-audit.md`, which implements the matching half
  at runtime. Your phrase lists are what `004` copies; do not copy its logic
  back into the eval runner.
- **Attaching MCP servers in the eval runner.** `run_case` returns
  `Err("MCP support in eval runner not yet implemented")` when `--mcp-command`
  is passed. Unrelated, but it means neither seed case can exercise MCP-served
  tools.
- **Any change to `hanihi-core`, the log schema, or the streaming paths.**
- **Making the runner tolerant.** Strict parsing for assertions is deliberate.

## Commit message

Subject, imperative, capitalized, no final period, ≤72 characters, then a
blank line, then a body wrapped at 72 explaining context and reasoning rather
than implementation. Include the red-check evidence for `008`. End with:

```
Hānihi
```

Once `002` has landed, use `git commit -F working/commit-msg-003.txt` to write
this body. Until then, a single-token subject is the only form the harness can
express — accept that, and say so in the subject if needed.
