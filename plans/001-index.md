# Hānihi plan sequence

The active plan set. Read it before starting work.

Completed work lives in `plans/archived/complete/` and is **not** part of this
sequence — see its `README.md`. Reference material (not tasks) is in
`plans/reference/`.

## Order

| # | File | Scope | Depends on |
|---|---|---|---|
| 001 | this file | — | — |
| 005 | `005-outstanding-gaps.md` | `hanihi-core`, `hanihi-cli`, docs | nothing |

## What the previous sequence did

The `002`/`003`/`004` sequence is **complete** and archived:

- **002 — `git commit -F`** (commit `04b5e93`). A multi-line commit message can
  now be written from a file instead of being flattened into a slug by
  whitespace splitting. Archived as
  `archived/complete/002-commit-message-file.md`.
- **003 — behavioural eval assertions** (commit `fe1182c`). The eval harness
  can now express "the agent said something its own tool log contradicts", as
  `no_unsupported_capability_claim` and `reported_tool_errors_are_real`.
  Archived as `archived/complete/003-eval-behavioural-assertions.md`.
- **004 — harness self-audit** (commit `4495bd5`). The same contradiction is
  detected at runtime, at the turn boundary, as a diagnostic that never fails
  a turn. Archived as `archived/complete/004-harness-self-audit.md`.

Their verified state is recorded in the archive `README.md`. Do not re-open
them; they are background, not tasks.

## What `005` is for

The previous sequence carried a list of known gaps and, deliberately, did not
schedule them: each was recorded so it would not be rediscovered as a
surprise. `005` is that list, promoted to a task now that the sequence it was
attached to has landed.

`005` is also where the two seed eval cases are actually run. `003` added
`007-unsupported-capability-claim` and `008-missing-path-recovery` but could
not execute them — no `LLM_API_KEY` was present — so both ship **unverified**.
An assertion never seen to pass is not evidence that anything works, and one
never seen to fail is not evidence that it can detect anything. Closing that
loop is the first item in `005`.

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
