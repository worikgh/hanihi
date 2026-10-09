# 005 — Close the outstanding gaps

You are Hānihi. This is a prompt to a future session. Its job is to close the
gaps the `002`/`003`/`004` sequence recorded and deliberately did not schedule.

This prompt is self-contained. Implement it even if nothing else has landed.

## Objective

Five items, each independently landable. Do them in this order; the order is
"what unblocks what", not merely preference.

1. **Run the two seed eval cases.** They were added by `003` and have never
   been executed. Everything in that plan is verified by unit test and
   unverified against a model.
2. **Resolve `029` part D** — the non-streaming path is still live and
   unwatched, and `AgentError::Deprecated` does not exist.
3. **Log an `error` event when the model call itself fails** on the
   non-streaming path.
4. **Persist the compaction summary** so a resumed session does not replay
   full history and re-compact on its first oversized call.
5. **Refresh or retire `report.md`.** It is stale and actively misleading.

Items 1 and 5 are documentation/verification. Items 2, 3, and 4 are code.

## Why this is one plan and not five

Each item is small. They are grouped because they share one property: none was
found by a failing test, all were found by reading. A session that has to
re-derive the shape of the gap from `git log` and four READMEs will get one of
them right and mis-scope the others. Recording them together, with the
evidence for each, is cheaper than five separate rediscoveries.

If the session judges that a subset should land separately, that judgement is
correct and should be recorded in the commit body — but do not silently drop
an item.

## Read first, verbatim

- `plans/001-index.md` — the sequence that landed, and the pointer to this
  file.
- `plans/archived/complete/README.md` — the caveats section, which is where
  items 2 and 3 are currently recorded.
- `plans/archived/complete/029-tool-failure-recovery.md` — parts A–C landed,
  part D did not. Read §D and §D1 in full: §D1 is the prerequisite, not a
  footnote.
- `plans/archived/complete/003-eval-behavioural-assertions.md` — the source of
  both seed cases.
- `crates/hanihi-core/src/session/mod.rs` — `Session::run` (the
  non-streaming loop, still fully implemented) and `run_streaming`.
- `crates/hanihi-core/src/error.rs` — `AgentError` in full. No `Deprecated`
  variant exists.
- `crates/hanihi-cli/src/main.rs` — `run_turn`, whose `Option<TurnSummary>`
  conflates "channel closed" with "error event".
- `report.md` — item 5's subject.

## Item 1 — Run the two seed eval cases

`003` added `evals/cases/007-unsupported-capability-claim/` and
`evals/cases/008-missing-path-recovery/`. Neither has ever been executed: no
`LLM_API_KEY` was present when they landed, and both were reported as
unverified.

An assertion that has never been seen to pass may be asserting nothing. One
that has never been seen to fail cannot be shown to detect anything. Both
halves matter.

**Do:**

- If `LLM_API_KEY` is present, run both cases with `--keep-sessions`:

  ```text
  LLM_API_KEY=… cargo run -p hanihi-eval -- --case 007-unsupported-capability-claim --keep-sessions
  LLM_API_KEY=… cargo run -p hanihi-eval -- --case 008-missing-path-recovery --keep-sessions
  ```

  `--keep-sessions` is required while validating: the assertion detail strings
  are the diagnostic, and the retained `events.jsonl` is what makes a failure
  analysable.

- Read the retained `events.jsonl` for each. Record for each case: pass/fail,
  the assertion details, and whether the assertion fired for the reason the
  plan intended rather than by accident.

- **`008` second red-check.** `003` red-checked `008` by reverting the
  `list_dir` nearest-ancestor hint and observing the *pinned core test* fail.
  That is evidence the hint exists, not evidence the eval case can fail. If a
  live run is available, confirm the case is capable of failing by the same
  mechanism the plan describes, or state plainly that the red-check remains
  unit-level only.

- **Record the outcome.** The commit body must say pass, fail, or unverified,
  and for what reason. A case reported as passing must have been run.

**If no key is present:** say so and report both as still unverified. Do not
claim a case passed. Do not weaken an assertion to make an unrunnable case
look covered — that converts a known gap into a hidden one.

### If a case fails

Do not fix it here. Determine which of these it is and write the answer into
the commit body:

- the case is over-specified (pins behaviour that is not required),
- the assertion is wrong (the predicate does not match the intended property),
- the harness is wrong (a real defect, and it becomes its own plan).

Only the first two are in scope for this item.

## Item 2 — Resolve `029` part D

The non-streaming path (`Agent::run`, `Session::run`) is fully implemented,
unreachable from the CLI, and has already drifted once. `029` stated the three
of its changes "must land together", and two of the three landed. The file is
archived with that inconsistency live.

`AgentError::Deprecated` does not exist.

**Do, in this order:**

- **First, §D1.** `run_turn` returns `Option<TurnSummary>`, and `None` means
  both "the channel closed" (`main.rs:~712`) and "an error event arrived"
  (`main.rs:~749`). Until those are distinguishable, a stub's error cannot
  surface. Give the outcome a name — an enum, e.g.
  `TurnOutcome::{Complete, Aborted, Interrupted}` — and justify the choice in
  the commit body. Do not collapse the two `None`s into one.
- **Then stub.** `Agent::run` and `Session::run` return
  `Err(AgentError::Deprecated { message })` with a
  `#[deprecated(note = "…")]` attribute. Add the variant with
  `Display: deprecated: {message}`.
- **The failure must be loud.** A stub returning a plausible empty
  `TurnSummary` would be exactly the silent-success defect `029` existed to
  remove. Assert the consequence: driving the CLI down a non-streaming path
  prints a deprecation error and stops, and never prints a turn footer.

**Acceptance criteria:**

- `Agent::run` and `Session::run` return `AgentError::Deprecated`.
- `run_turn` distinguishes the three outcomes, and a test covers each.
- `029`'s caveat in the archive `README.md` is closed, not restated.
- Every existing test that used the non-streaming path is either re-pointed at
  `run_streaming` with its coverage preserved, or deleted with a stated reason.
  `029` §"New tests in `session/mod.rs`" item 9 is the precedent: the
  compaction-before-prompt ordering assertion (`compaction_idx + 1 ==
  prompt_idx`) is still valid on the streaming path and must keep its
  coverage.

## Item 3 — Log an `error` event when the model call fails

`Session::run` writes no `LogEntry::Error` when the model call itself fails.
Out of scope in archived `008`; still unfixed.

The consequence is that a session log can end without `turn_complete` and
without `error`, which is indistinguishable from a process crash. A reader
cannot tell "the model refused" from "the process died".

**Do:** on the model-call failure path, write
`LogEntry::error(ts, turn, ErrorStage::LlmCall, message)` before returning.

**Acceptance criteria:**

- A failing model call leaves an `error` entry with `ErrorStage::LlmCall` in
  `events.jsonl`.
- The message names the failure and does not leak the API key.
- A test drives a failing model and asserts the entry, not merely the
  returned error.

**Note:** this item is coupled to item 2. If `Session::run` is stubbed first,
the failure path being fixed here may no longer be reachable, so the fix must
land on the streaming path or the stub and this fix must be reconciled
deliberately. Decide which, and say which, in the commit body. Do not fix a
path you have just deleted.

## Item 4 — Persist the compaction summary

`Agent.summary` is in memory only. A resumed session replays full history and
re-compacts on its first oversized call, paying for a summarization the
previous session already paid for. Deferred by archived `013`/`014`.

**Do:**

- Research first. `013` established the log-event framework and
  `SCHEMA_VERSION = 2`; the module policy is that an *additive optional field*
  with `#[serde(default)]` does not bump the version, and a breaking change
  does. Determine which this is before writing code, and state the finding.
- If an additive optional field on an existing entry suffices, take that path:
  no version bump, no migration.
- If a new `LogEntry` variant is required, the version bump and its migration
  belong in the framework archived `008` established. Do not bump ad hoc.
  §4 of archived `004-harness-self-audit.md` sets out the cost of a bump —
  a newer-schema log is **rejected**, not skipped, by an older binary — and
  the same reasoning applies here.
- On resume, seed the summary so the first call does not re-compact.

**Acceptance criteria:**

- A resumed session whose first call would exceed the budget does not issue a
  summarization call. Assert the absence of the call, not just the absence of
  a visible symptom.
- `SCHEMA_VERSION` is unchanged, **or** the bump is accompanied by the
  migration and the compatibility note.
- Existing replay tests (`replay_multi_turn`, `replay_with_tool_calls`,
  `replay_ignores_compaction_entries`) pass unchanged.

## Item 5 — `report.md` is stale

`report.md` documents `schema` as `1` and lists nine event kinds.
`SCHEMA_VERSION` is `2` and there are ten (`compaction` was added). It is
worse than absent: a reader who trusts it will write a reader for the wrong
format.

**Do:** refresh it or retire it. The retire path is defensible — archived
`008-log-integrity.md` and `013-log-compaction-events.md` are the authoritative
records, and a second, drifting description of the same schema is the defect.

**Acceptance criteria:**

- Either every claim in `report.md` is true against the code, or the file is
  gone and whatever referenced it is updated.
- If refreshed, the schema version and the event-kind list are correct, and
  a note says what to read for the authoritative description.
- `grep` for the parts that were wrong finds no surviving copy of them
  elsewhere in the repo.

## Gates

```text
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Item 1's cases are **not** hermetic. They need a live model, and are run
manually. Report each as pass, fail, or unverified.

## Out of scope

- **Re-opening `002`/`003`/`004`.** They are archived as complete. If work in
  this plan reveals a defect in one of them, that is a new finding: record it,
  do not silently reopen the archived file.
- **`--baseline` / `--compare`** in the eval runner. A stretch goal from
  archived `005`, not a gap; leave it.
- **The MCP support gap in the eval runner.** `run_case` returns
  `Err("MCP support in eval runner not yet implemented")`. Unrelated.
- **Re-litigating `029` parts A–C.** They settled tool-error handling. Do not
  reopen them to make part D easier.
- **Promoting the duplicated audit phrase lists into `hanihi-core`.** Both
  `003` and `004` note this as the right follow-up *if* the lists start
  drifting often. They have not. Doing it speculatively is a separate change.
- **`plans/025-cpp-smoke-test.md`.** Still outstanding and still a
  verification exercise with an unrecorded outcome. It needs C++ tooling and a
  person to run it; it is not a code change and is not scheduled here. If you
  have the toolchain and the time, running it is welcome, but it is not an
  item in this plan.

## Commit message

Subject, imperative, capitalized, no final period, ≤72 characters, then a
blank line, then a body wrapped at 72 explaining context and reasoning rather
than implementation. For item 1, state each case's outcome explicitly. For
item 2, state which outcome `run_turn` distinguishes and why. For item 4, state
whether the schema version moved and why. End with:

```
Hānihi
```
