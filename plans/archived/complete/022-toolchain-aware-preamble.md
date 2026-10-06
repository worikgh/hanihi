You are Hānihi working on this Rust repository. You may edit tracked files
and make local git commits — never push. This is plan 3 of 5 in a series
that makes Hānihi work well in a C++ project. This plan is self-contained:
implement it even if none of the other four exist yet, and do not modify
anything in anticipation of them.

## Objective

The agent's system preamble hardcodes a Rust verification workflow — run
`cargo fmt`, `cargo test`, `cargo build`, `cargo clippy -- -D warnings`. In a
C++ repository the model reads those instructions and either calls a tool
whose command is not in the allowlist (wasting turns on refusals) or
discards the whole preamble and loses the workflow guidance with it.

Make the verification paragraph of the preamble depend on the repository's
build toolchain, using the same conditional-text pattern the codebase
already uses for the two `run_command` descriptions.

## Read first, verbatim

- `crates/hanihi-core/src/agent.rs`
  - The system preamble constant, around lines 30-60. Read the whole string.
    Note the current workflow sentences: "run `cargo fmt` before staging
    changes; `cargo test` before committing; `cargo build` must pass; run
    `cargo clippy -- -D warnings` before finishing. Make changes as small git
    commits with ...". Identify the exact span to be made conditional and
    what must stay unconditional.
  - How the preamble is assembled into a `CompletionRequest` — find every
    read of the preamble constant. `Agent::prepare_context` returns
    `PreparedContext { preamble, messages, user_input, log_messages }`, so
    the preamble is a computed value, not necessarily a `const`; determine
    which it is before designing.
  - `Agent`'s constructor and fields — does the agent already hold a
    `SourceTree` (directly or via `Arc`)? This determines whether the
    toolchain is reachable when the preamble is built.
  - Where the effective preamble gets the `## Summary of the conversation so
    far:` heading appended (compaction) — the new text must compose with
    that without duplicating or reordering it.
  - Existing tests around the preamble (`test_over_budget_compacts_history`
    and any assertion on preamble content) — these may pin the current
    string.
- `crates/hanihi-core/src/tool.rs`
  - `RUN_COMMAND_DESC_WRITE` and the read-only inline description inside
    `builtin_run_command_for` — the established pattern for one description
    with two variants selected by a `match` on a mode enum. Mirror this
    shape exactly; do not invent a second idiom.
  - `CommandMode` (~283-292) — the enum the descriptions match on.
- `crates/hanihi-core/src/session/mod.rs` — how `system_prompt` is stored in
  `session.json` at creation and replayed on resume. This matters (see D).
- `crates/hanihi-core/src/lib.rs` — re-export style.
- `plans/012-token-budgeting.md` and `plans/014-log-compaction-events-prompt.md`
  — prior reasoning about preamble size. Any added text costs input tokens on
  every request; keep the addition small.

## Findings (confirmed)

1. The preamble is a single Rust-workflow string. It names `cargo` four
   times and no other tool.
2. `tool.rs` already solves the identical problem — one tool, two
   descriptions — with two constants (or a constant plus an inline literal)
   selected by `match mode`. There is no reason for the preamble to solve it
   differently.
3. The preamble is persisted into `session.json` as `system_prompt` when a
   session is created, and replayed on resume. A resumed Rust session opened
   in a C++ repository (or vice versa) will therefore carry the prompt from
   creation time unless something actively rebuilds it. This is the central
   design hazard of this plan.
4. `Session` — not `Agent` — is what reads and writes `session.json`; the
   agent receives the prompt. So the toolchain must be threaded from the
   session/CLI layer into the agent, or the preamble must be built at a layer
   that can see both.

## Design

### A. Where the conditional text lives

Split the preamble into two parts:

- **Invariant text** — identity, the tool inventory's role, the
  never-push/commit-only contract, the "small focused change" guidance,
  everything that is true regardless of language. Unchanged, still one
  constant.
- **Workflow text** — the verification sequence, per toolchain. One variant
  per `Toolchain` (if plan 1 landed) or per locally-defined equivalent.

Structure it as a function, named for what it produces:

```rust
/// The build/verify workflow paragraph for `toolchain`, appended to the
/// invariant preamble.
fn verification_preamble(toolchain: Toolchain) -> &'static str
```

returning a `&'static str` per variant so there is no allocation on a path
that runs once per request. Match exhaustively; `Toolchain::Unknown` gets a
variant that names no build tool at all.

**Dependency note.** If plan 1 (`Toolchain` in `source.rs`) has not landed,
define a private local enum with the same three variants and a
`From`-style conversion, and leave a single clearly-marked line to change
when plan 1 arrives. Do **not** implement plan 1 here, and do **not** add a
marker-file detection function to `agent.rs` — detection belongs in
`source.rs`, and a second implementation would drift. If the agent cannot
see a `SourceTree`, prefer stopping and reporting that as the blocker over
inventing a parallel detection path.

### B. The Cargo variant

Keep the existing Rust sentences **verbatim**. The goal is that a Rust
session's preamble is byte-identical to today's after this change. Copy the
current text exactly; do not "improve" its wording, reorder its clauses, or
fix its punctuation while moving it. Any diff in the Rust variant is a
regression against the "preserve existing behaviour" requirement.

### C. The CMake variant

Write it as the honest C++ counterpart, and be explicit about the weaker
guarantees rather than pretending `clang-format` equals `clippy`:

- configure and build: `cmake -B build`, then `cmake --build build`
- fast per-file check: the compiler with `-c` and `-fsyntax-only`, which is
  cheaper than a build and catches parse and type errors immediately
- tests: `ctest --test-dir build` when the project defines tests
- formatting: **do not** instruct the agent to run `clang-format`
  unconditionally. A `.clang-format` may not exist, and no formatter is
  admitted by the command allowlist. Phrase this as "match the surrounding
  style; do not reformat code you did not change" rather than naming a tool
  that cannot be invoked.
- linting: **do not** name `clang-tidy`. It needs a `compile_commands.json`
  from a successful configure, and it is not in the allowlist. Same treatment.

This is a real capability difference from Rust and the prompt should not
paper over it. The C++ variant should tell the agent that *compiles* and
*tests pass* are the gates it can verify, and that style is a matter of
matching the surrounding code.

Keep it short. The whole added cost is paid on every request for every C++
session, and the existing plans already worry about preamble size.

### D. The `Unknown` variant

Name no build system. State that no build or test command is available for
this repository and that the agent must say so rather than guess at one.
A model that invents `make` or `./configure` in a repo with no recognised
build system produces confusing refusals; a prompt that admits the gap is
strictly better.

### E. Composition with compaction and with `--prompt`/`--prompt-file`

- The compaction summary heading is appended to the *effective* preamble.
  The new variant text must be part of the base preamble that the summary
  heading is appended to, not appended after the summary. Verify the
  ordering by reading `prepare_context`; the summary must remain last.
- `--prompt`, `--prompt-file`, and `--new-prompt` append to or replace the
  stored prompt. Do not change that machinery. Note in your write-up whether
  a user-supplied `--prompt` that names `cargo` in a C++ repo is now
  contradictory — it is, and it is the user's text, so it wins. Do not
  filter user text.

## Work order, test-first

Write the failing tests first, then implement A-E, then run the gates.

### Tests in `agent.rs` (or wherever the preamble is tested today)

Use `rig`'s `MockCompletionModel` and the existing scripted-turn pattern —
no network. If the preamble is observable through `prepare_context`, test it
there rather than through a full scripted run.

1. `preamble_rust_names_cargo_workflow` — `Toolchain::Cargo` (or the local
   equivalent) produces text containing `cargo fmt`, `cargo test`,
   `cargo build`, and `cargo clippy -- -D warnings`.
2. `preamble_rust_is_unchanged` — assert the Cargo variant equals the exact
   pre-change preamble string, as a literal in the test. This is the
   regression guard for the "byte-identical" requirement, and it is the most
   important test in this plan. Capture the current string from the source
   before you edit it.
3. `preamble_cmake_names_cmake_workflow` — contains `cmake -B build`,
   `cmake --build build`, and `-fsyntax-only`.
4. `preamble_cmake_does_not_name_cargo` — asserts the C++ variant contains
   no occurrence of `cargo` and no occurrence of `clippy`. This is the
   test that actually prevents the reported problem: a model reading a C++
   prompt that says `cargo` will try to run `cargo`.
5. `preamble_cmake_does_not_name_unavailable_linters` — asserts the C++
   variant does not contain `clang-format` or `clang-tidy`, since neither is
   invocable. If a future plan admits them, this test fails and forces the
   decision to be revisited, which is the intent.
6. `preamble_unknown_names_no_build_tool` — asserts none of `cargo`,
   `cmake`, `ctest`, `make`, or `ninja` appear.
7. `preamble_variants_share_the_invariant_text` — for each variant, assert a
   couple of invariant sentences (the commit-only/never-push contract, the
   small-change guidance) are present. Prevents a future edit from dropping
   invariants in one variant only.
8. If compaction composition is observable at this layer, assert the
   summary heading appears **after** the workflow text for every variant.

Do not assert on total preamble length; that would make every future wording
tweak a test failure. Assert on content.

### Wiring tests

Whatever layer constructs the agent with a `SourceTree` (CLI, eval runner,
or session) must be exercised by at least one test proving the toolchain
reaches the preamble. If the only such seam is the CLI's `main`, do not add
a test that spawns a process; instead extract the selection into a pure
helper (`verification_preamble(toolchain)`) and rely on its unit tests, then
verify the wiring by inspection and state that in your write-up. Do not
weaken production structure to manufacture a testable seam for this.

## Verification gates

```text
cargo fmt
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acceptance criteria

- A repository with a `Cargo.toml` produces a preamble byte-identical to the
  current one.
- A repository with a `CMakeLists.txt` produces a preamble that names
  `cmake -B build`, `cmake --build build`, and `-fsyntax-only`, and that
  contains no occurrence of `cargo`, `clippy`, `clang-format`, or
  `clang-tidy`.
- A repository with neither marker produces a preamble that names no build
  tool and instructs the agent to report the gap.
- The compaction summary heading still composes after the workflow text.
- `--prompt` / `--prompt-file` / `--new-prompt` behaviour is unchanged.
- No new dependencies. No change to `SourceTree`, the command allowlist, or
  any eval assertion type.

## Out of scope

- Toolchain detection itself — plan 1. If it has not landed, use the local
  fallback in A and say so.
- Adding `cmake`/compiler commands to the allowlist — plan 2. **This plan
  does not make the C++ prompt's commands actually runnable.** If plan 2 has
  not landed, the C++ preamble will name commands the allowlist refuses, and
  that is expected and temporary. State it plainly in your write-up rather
  than softening the prompt to avoid the mismatch; softening it would make
  the prompt wrong once plan 2 lands.
- A `--toolchain` override flag.
- Per-repository prompt files, `.hanihi/prompt.md`, or any prompt-discovery
  mechanism beyond the existing `--prompt` flags.
- Rewriting the invariant part of the preamble, or any wording change to the
  Rust variant.
- Fixing the `.ignore` template — plan 5.

## Assumptions and risks

- **`session.json` stores the prompt at creation time.** A session created in
  a Rust repo and resumed in a C++ one keeps the Rust workflow text. This
  plan does not fix that; fixing it means either rebuilding the preamble on
  every resume (changing what `session.json`'s `system_prompt` means) or
  detecting the mismatch and warning. **Flag this to the user as an open
  question in your write-up** and do not silently change resume semantics.
- **Preamble text is a soft constraint.** A model can ignore it entirely. The
  value here is removing a *contradiction* — the model being told to run a
  tool it does not have — not guaranteeing behaviour. Do not describe this
  change as making the agent use CMake.
- **Ordering dependency on plan 2.** The C++ variant names commands that are
  only runnable once plan 2 lands. The plans are ordered for this reason; if
  they are applied out of order, the C++ prompt is aspirational in the
  interim. This is deliberate: prompt text and allowlist should converge on
  the same command set, and writing the prompt first means the allowlist
  lands against a stated intent.
- **Token cost.** Every C++ session pays for the added text on every request.
  Keep the C++ variant to roughly the length of the Rust one.
- **Test 2 pins a string.** A deliberate wording change to the Rust variant
  now requires updating a test literal. That friction is the point.
