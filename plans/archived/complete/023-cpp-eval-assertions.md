You are Hānihi working on this Rust repository. You may edit tracked files
and make local git commits — never push. This is plan 4 of 5 in a series
that makes Hānihi work well in a C++ project. This plan is self-contained:
implement it even if none of the other four exist yet, and do not modify
anything in anticipation of them.

## Objective

Make the eval runner able to verify C++ work. Today the assertions
`build_succeeds`, `tests_pass`, and `clippy_clean` run `cargo` in the case's
`repo`, so a C++ case has no gate at all. Add a per-case way to name the
build and test commands, replace the misnamed `clippy_clean` with a
toolchain-neutral lint gate, and add a C++ fixture so the new assertions are
exercised.

The assertion vocabulary is part of the eval harness's public contract (it
appears in every `case.toml` and in the README). Getting the naming right
matters more than getting it fast.

## Read first, verbatim

- `crates/hanihi-eval/src/` — every file. In particular:
  - the assertion type enum and its `serde` representation (how
    `type = "build_succeeds"` becomes a Rust variant),
  - the assertion evaluation function, and the exact helper that shells
    `cargo`/`git` for `build_succeeds`, `tests_pass`, `clippy_clean`,
    `no_diff`,
  - how the `repo` and `fixture` fields of `Case` are parsed and resolved
    (README says `repo` is resolved relative to the case directory; confirm
    where `fixture` is used and what it does differently),
  - how `write_tools`, `source_tree`, `model`, and `system_prompt` are
    threaded into the agent for a case,
  - the `--list`, `--case`, `--keep-sessions`, `--timeout` flags and where
    argument parsing rejects `--mcp-command` ("MCP support in eval runner not
    yet implemented") — note the house style for rejecting an unsupported
    option.
- `evals/cases/` — every case directory. Read at least
  `002-get-time/case.toml` (the README quotes it as canonical) and the
  `001-basic-echo` README (it documents the MCP limitation).
- `plans/002-evals.md` and `plans/016-eval-behavioural-assertions.md` — the
  design intent behind the assertion set and the behavioural gates. Follow
  their conventions; in particular, find out whether their field naming
  (`repo`, `fixture`, assertion keys) was specified and match it.
- `README.md` sections "Eval runner", "Status", and "Testing" — the
  assertion list and the `case.toml` example are documented there and must be
  updated in the same change.

## Findings (confirmed from the README and case layout)

1. The assertion set is `tool_called`, `tool_not_called`, `text_contains`,
   `text_not_contains`, `text_regex`, `no_error`, `max_turns`, `latency_ms`,
   `token_budget`, `build_succeeds`, `tests_pass`, `clippy_clean`,
   `no_diff`. The last four "run `cargo`/`git` gates in the case's repo and
   need a `repo` (or `fixture`) field in `case.toml`".
2. `clippy_clean` is a *Rust-specific tool name* in a toolchain-neutral
   vocabulary. In a C++ case it is either unrunnable or a lie.
3. `build_succeeds` and `tests_pass` are toolchain-neutral *names* with
   Rust-specific *implementations*.
4. Case fields: `user_input` (required), `assertions` (required), optional
   `model`, `system_prompt`, `source_tree`, `write_tools`, `repo`, `fixture`.
5. The eval runner does not support MCP servers, so `001-basic-echo` cannot
   run. Any new C++ case must not depend on MCP.

## Design

### A. Keep the assertion names; parameterise the commands

`build_succeeds` and `tests_pass` are good names — they say what is checked,
not how. Do not rename them. Instead add per-case command fields so their
implementation is determined by the case, with the current `cargo`
invocations as the defaults. An existing Rust case with no new fields must
behave exactly as before; that is the compatibility bar.

Add to `Case`:

```toml
# Optional. Overrides the default build command for this case.
build_command = ["cmake", "--build", "build"]
# Optional. Overrides the default test command.
test_command = ["ctest", "--test-dir", "build", "--output-on-failure"]
# Optional. Overrides the default lint command. Absent = the gate is skipped.
lint_command = ["clang-tidy", "-p", "build", "src/foo.cpp"]
# Optional. Run before build/test to configure the project.
configure_command = ["cmake", "-B", "build"]
```

Decisions to make explicit:

- **Type is `Vec<String>` (argv), not a shell string.** A string would
  reintroduce shell parsing into a harness that deliberately has none
  (`run_command` splits on whitespace and forbids a shell; `scrubbed_env`
  scrubs the environment). Keep argv. Document that no shell metacharacters
  are interpreted.
- **Four fields, not one `toolchain = "cmake"` enum.** A toolchain enum
  requires the eval runner to know CMake's configure/build/test split, which
  is exactly the coupling this plan should avoid; and it cannot express a
  project that tests with a custom runner. Commands are more general and
  cost nothing at the call site for the common case, because the defaults
  cover it.
- **`configure_command` is separate because CMake needs two steps** where
  Cargo needs one. `build_succeeds` runs `configure_command` (if present)
  then `build_command`. If configure fails, the build step is skipped and
  the assertion fails with the configure output — the diagnostic is the
  point.
- **Defaults are the current behaviour**, in one place:
  `["cargo", "build"]`, `["cargo", "test"]`, `["cargo", "clippy", "--",
  "-D", "warnings"]`, and no configure step. Confirm the exact current argv
  before writing the defaults; the README's wording ("run `cargo`/`git`
  gates") is not precise enough to copy from.

### B. Replace `clippy_clean` with `lint_clean`

`clippy_clean` names a Rust tool. Rename it to `lint_clean` and drive it from
`lint_command`. Semantics: the gate passes when the `lint_command` exits 0,
and produces no output on stdout/stderr when it exits 0.

**Migration policy — choose option 1:**

- *Option 1 (chosen):* keep `clippy_clean` as an accepted alias that maps to
  `lint_clean` with the default `cargo clippy -- -D warnings`, and document
  it as deprecated. Existing `case.toml` files keep working untouched, and
  the vocabulary gains a neutral name.
- *Option 2:* rename outright and edit the existing cases.

Option 1 is chosen because assertion names are user-facing contract and this
plan's stated bar is that existing cases behave identically. Implement the
alias with `serde` `#[serde(alias = "...")]` on the variant if the enum
derives `Deserialize` — but **check first**: if the assertion type is parsed
from a `type = "..."` string discriminant rather than a tagged enum, the
alias may need an explicit match arm. Use whichever mechanism the file
actually uses; do not restructure the parsing to make `alias` work.

`lint_clean` with no `lint_command` is a **configuration error**, not a
silent pass. A gate that quietly succeeds when unconfigured is worse than no
gate, and it would let a C++ case claim lint coverage it never had. Fail the
assertion with a message naming the missing field.

### C. Degradation must be honest

The C++ story is genuinely weaker than Rust's: `build_succeeds` and
`tests_pass` are real gates, `lint_clean` generally is not, because
`clang-tidy` needs a `compile_commands.json` produced by a successful
configure and there is no canonical C++ equivalent of `clippy` that is
always available.

Do not paper over this. Concretely:

- Do not add a default `lint_command` for CMake cases. There is no defensible
  default; a project may have no `.clang-tidy` and no `compile_commands.json`.
- The README's assertion table must state, next to `lint_clean`, that it is
  skipped when `lint_command` is absent and that it is a configuration error
  otherwise. It must not imply `lint_clean` is C++'s `clippy_clean`.
- If it is cheap, add a `--list`-adjacent diagnostic or a startup warning for
  a case whose assertions include `lint_clean` without a `lint_command`. Only
  if it falls out naturally; do not build a validation subsystem for it.

### D. How the gate runs

Reuse the existing command-execution helper the `build_succeeds` path already
uses. Requirements, all of which the existing helper should already satisfy —
verify rather than assume:

- cwd is the resolved case `repo` directory, never the eval runner's cwd.
- the environment is scrubbed the same way `run_command` scrubs it
  (`scrubbed_env`: PATH, HOME, CARGO_*, RUSTUP_*). For CMake this is
  sufficient — CMake finds its compiler on PATH — but confirm that a
  scrubbed environment does not break configure in practice by running the
  new fixture case.
- a timeout applies, and the existing `--timeout SECS` flag bounds it.
- non-zero exit surfaces the captured stdout/stderr in the assertion failure
  message. The whole value of a build gate in an eval is the compiler error
  text; a failure that says only "exit code 1" is useless.

If the existing helper hardcodes `cargo`/`git` in a way that must be
generalised, generalise the helper — but note that `run_command`'s allowlist
lives in `hanihi-core` for the *agent's* tool and the eval runner's gates are
a separate mechanism. Do not route eval gates through the agent's allowlist;
they are configured by the case author, not chosen by a model, and requiring
`cmake` to be allowlisted would be a category error. If they *are* currently
routed through it, stop and report that as a finding before changing it.

### E. Fixture

Add a C++ fixture repository under the case directory, following whatever
`fixture` means for the existing Rust cases (read how `fixture` is resolved —
it may be a template copied into a temp dir, or a path used in place). It
must be a working CMake project:

- `CMakeLists.txt` with a `project(...)`, at least one library or executable
  target built from a `.cpp` file, and `enable_testing()` plus a test
  registered via `add_test` so `ctest` has something to run.
- A `.gitignore` or the harness-generated `.ignore` covering `build/`.
- A trivial source file and a trivial assertion so `tests_pass` is
  meaningfully green and can be made meaningfully red by a deliberate break.

**Verify by hand, outside the eval runner, before wiring the case**: run
`cmake -B build`, `cmake --build build`, and `ctest --test-dir build` in the
fixture and confirm all three succeed. A fixture that has never been built is
not a fixture.

### F. New case and README

Add one case that exercises the full C++ path, e.g.
`evals/cases/006-cpp-build/`:

```toml
user_input = "Add a function `int add(int, int)` to src/foo.cpp ..."
build_command = ["cmake", "--build", "build"]
configure_command = ["cmake", "-B", "build"]
test_command = ["ctest", "--test-dir", "build", "--output-on-failure"]
...

[[assertions]]
type = "build_succeeds"

[[assertions]]
type = "tests_pass"

[[assertions]]
type = "no_error"
```

Follow the existing cases' `README` convention (each case has one).

Update `README.md`: the assertion list, the `case.toml` field list, the
`case.toml` example, and the "Known TODOs"/"Status" wording if it claims the
gate set is Rust-only. Keep the edit tight and in the file's existing voice.

## Work order, test-first

Write the failing tests first, then implement A-F, then run the gates. The
eval runner needs a live LLM for a full case, so most of this plan's tests
must be **assertion-level unit tests that do not call a model**: construct a
`Case` in code, point its `repo` at a temp fixture, run the assertion
evaluator, and check pass/fail. That is the seam to use.

### Tests

1. `build_succeeds_defaults_to_cargo_build` — a case with no
   `build_command` in a Rust temp repo behaves as today.
2. `build_succeeds_uses_the_case_build_command` — a CMake fixture with
   `configure_command` + `build_command` passes.
3. `build_succeeds_fails_with_compiler_output` — break the fixture source
   deliberately; assert the failure message contains the compiler's
   diagnostic text, not just an exit code. This is the test that pins D's
   last requirement.
4. `configure_failure_skips_the_build_step` — a bad `configure_command`
   fails the assertion and the message names configure, not build.
5. `tests_pass_uses_the_case_test_command` — CMake fixture with
   `ctest` passes.
6. `tests_pass_fails_when_a_test_fails` — fixture with a failing assertion;
   assert failure and that the test output is surfaced.
7. `clippy_clean_still_parses_as_the_cargo_lint_gate` — an assertion written
   as `type = "clippy_clean"` in a `case.toml` deserialises and behaves as
   `lint_clean` with the cargo default. Parse from a real TOML string, not a
   constructed struct, so the alias mechanism is genuinely tested.
8. `lint_clean_without_lint_command_is_a_configuration_error` — must fail,
   and must **not** silently pass.
9. `lint_clean_uses_the_case_lint_command` — a `lint_command` that exits 0
   passes; one that exits 1 fails.
10. `gate_commands_run_in_the_case_repo` — assert cwd by using a command
    whose output reveals it, or by asserting a build artifact appears under
    the case repo and nowhere else.
11. `case_toml_parses_the_new_optional_fields` — round-trip a `case.toml`
    containing all four commands; also assert one omitting them parses with
    `None`/defaults, so the fields are genuinely optional.

Do not add a test that calls a live model. If the only way to exercise the
new case end-to-end is a live LLM run, say so in your write-up, run it only
if the `LLM_API_KEY` is actually present, and otherwise report the case as
unverified rather than claiming it passes.

## Verification gates

```text
cargo fmt
cargo test -p hanihi-eval
cargo clippy -p hanihi-eval --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

If an API key is available:

```text
cargo run -p hanihi-eval -- --list
cargo run -p hanihi-eval -- --case 006-cpp-build
```

Report the actual output of the last command, or state that it was not run.

## Acceptance criteria

- `build_succeeds`, `tests_pass`, and `lint_clean` can be driven by per-case
  commands, with the current `cargo` invocations as defaults.
- An existing Rust `case.toml` with none of the new fields behaves
  identically to before, including `clippy_clean`.
- `clippy_clean` still parses; `lint_clean` without `lint_command` fails
  loudly rather than passing silently.
- Gate commands run in the case's resolved `repo` with a scrubbed
  environment and a timeout, and a failure surfaces captured output.
- A CMake fixture builds and tests successfully outside the runner, and a
  C++ case exercises both gates.
- `README.md` documents the new fields and the honest limits of `lint_clean`.
- No new dependencies beyond what the eval crate already uses.

## Out of scope

- MCP support in the eval runner (a separate, pre-existing limitation).
- Assertions for CMake-specific facts (e.g. "configured with generator X").
- `--baseline`/`--compare` diffing (a known TODO from plan 005).
- Making the *agent's* system preamble aware of the case toolchain — plan 3.
- Adding `cmake` to the agent's command allowlist — plan 2. **This plan does
  not depend on plan 2**: eval gates are case-authored commands, not
  model-chosen tools. A C++ case can pass its build gate even if the agent
  could not have run the same command itself. That is a real asymmetry — the
  eval verifies the *artifact*, not the agent's process — and the write-up
  should say so rather than implying the agent built the project.
- Toolchain detection in `hanihi-core` — plan 1.
- Fixing the C/C++ `.ignore` template — plan 5.

## Assumptions and risks

- **The R1/R2 naming question is a judgement call.** `lint_clean` was chosen
  over keeping `clippy_clean` and adding a neutral sibling, on the grounds
  that a tool name in a toolchain-neutral vocabulary will keep spreading
  (`cargo fmt` → `fmt_clean`? `rustfmt`?). If the user prefers strict
  backward compatibility with no deprecation, the alias in B is the seam to
  change and nothing else depends on the decision.
- **`lint_command` is honestly absent for most C++ cases.** Expect most C++
  cases to use only `build_succeeds` + `tests_pass`. That is a weaker gate
  set than Rust's and the README must not obscure it.
- **Eval gates bypass the agent's allowlist.** This is intentional (case
  authors are trusted; models are not) but it means an eval case can run a
  command the agent never could. Do not "fix" this by routing gates through
  `check_command_argv_mode`.
- **Fixture cost.** Adding a CMake fixture means the eval suite now requires
  `cmake` and a C++ compiler on the machine. That is a new external
  dependency for anyone running the suite. Make the new case skippable or at
  minimum make its failure message name the missing tool clearly, so a
  developer without a C++ toolchain is not left guessing.
- **`Vec<String>` argv and platform differences.** Absolute paths in a
  `build_command` will not survive a fixture copied to a temp dir; document
  that commands should be relative to `repo`. Do not add path templating.
