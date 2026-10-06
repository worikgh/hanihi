# Hānihi plan sequence

The active plan set. Four files, in order. Each of the three prompts is
self-contained: implement it even if the others have not landed.

Completed work lives in `plans/archived/complete/` and is **not** part of this
sequence — see its `README.md`. Reference material (not tasks) is in
`plans/reference/`.

## Order

| # | File | Scope | Depends on |
|---|---|---|---|
| 001 | this file | — | — |
| 002 | `002-commit-message-file.md` | `hanihi-core` (`tool.rs`) | nothing |
| 003 | `003-eval-behavioural-assertions.md` | `hanihi-eval` | nothing |
| 004 | `004-harness-self-audit.md` | `hanihi-core` (`agent.rs`) + CLI | 003 (phrase lists) |

Run them in number order. The dependency in 004 is a *code* dependency —
004 reuses 003's phrase-list module — so landing 004 first would duplicate
the matcher definition, which is the specific drift 004 §2 exists to
prevent.

## What each one is for

**002 — a staged-text channel for commit messages.**
`run_command` splits its input on whitespace and does no quote processing, so
`git commit -m "a b c"` reaches git as four argv elements and git reads three
of them as pathspecs. The repository's own convention is a capitalized
imperative subject plus a wrapped body ending in `Hānihi`; the harness cannot
produce one. The workaround is visible in `git log` as slug subjects
(`18e2897`, `56410f5`) and in `working/commit-msg-*.txt` as hand-written
bodies from earlier sessions. Fix: admit `git commit -F <path>`, so the
message lives in a file — one argv token — instead of being flattened.

**003 — behavioural assertions in the eval harness.**
Every existing eval assertion is a substring test on the final answer or a
structural count over the log. None can express "the agent claimed something
its own tool log contradicts". The motivating regression is a session where
the agent asserted it had no write tools while the log recorded 16 registered
tools including `apply_patch`, then spent several turns arguing instead of
working. 003 adds two model-free predicates and the seed cases that exercise
them.

**004 — harness self-audit.**
The same property, detected at runtime instead of off-line: a diagnostic at
the turn boundary that emits a stream event and a count on `TurnSummary`. It
never fails a turn. 004 is deliberately second because a heuristic that can
kill a run should not be the first place a property is expressed — 003 gives
it a signal that costs nothing to run first.

## Conventions every prompt in this sequence follows

- **Test-first.** Each prompt lists failing tests before the implementation
  that satisfies them. A new admitted form without a paired refusal test is an
  incomplete change.
- **Gates are stated verbatim and must pass.**
  `cargo fmt --check`, the package tests, and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- **Non-hermetic tests are named as such.** Cases needing a live model are run
  manually and reported as unverified if no key is present. Do not claim a
  test passed that was not run.
- **Never push.** Commits only.
- **Do not modify anything in anticipation of a later plan.** Each prompt
  states its own out-of-scope set; respect it rather than pre-building for
  the next file.

## Known gaps not covered by this sequence

Recorded so they are not rediscovered as surprises:

- **`029` part D, unresolved.** `Agent::run` (`agent.rs:580`) and
  `Session::run` are still fully implemented rather than stubbed, and
  `AgentError::Deprecated` does not exist. `run_turn`'s `Option<TurnSummary>`
  conflates "channel closed" with "error event" and must distinguish them
  first. This is a design decision, not an execution task, so it is not a
  file in this sequence. See
  `plans/archived/complete/README.md` §Caveats.
- **`Session::run` logs no `error` event** when the model call itself fails.
  Out of scope in archived `008`; still unfixed.
- **Resume-from-compaction.** `Agent.summary` is not persisted, so a resumed
  session replays full history and re-compacts on its first oversized call.
  Deferred by archived `013`/`014`.
- **`--baseline` / `--compare`** in the eval runner. Stretch goal from
  archived `005`.
- **`report.md` is stale.** It documents `schema` as `1` and lists nine event
  kinds. `SCHEMA_VERSION` is `2` and there are ten (`compaction` was added).
  Either refresh it or retire it in favour of
  `plans/archived/complete/008-log-integrity.md` and `013`.
