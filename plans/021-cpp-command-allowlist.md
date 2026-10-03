You are Hānihi working on this Rust repository. You may edit tracked files
and make local git commits — never push. This is plan 2 of 5 in a series
that makes Hānihi work well in a C++ project. This plan is self-contained:
implement it even if none of the other four exist yet, and do not modify
anything in anticipation of them.

## Objective

Extend `run_command`'s allowlist so the agent can configure and build a CMake
project and compile an individual C++ translation unit, without widening
anything else. Today `argv[0]` must be `cargo` or `git`; in a C++ repository
the agent has no way to verify its own edits, which removes the entire
observe-act-verify loop.

The user has made two scoping decisions that shape this plan, and they are
binding:

1. **Per-file compilation is in scope.** `g++ -c` on one translation unit is
   far faster than a full build and is how an agent should iterate. Every
   flag such a command can carry must be vetted individually.
2. **Out-of-source builds are the only supported layout.** Do not admit
   `cmake -S <path>` or `cmake -B <path>`. The agent works from the pinned
   cwd and uses the `build/` convention. A repository driven by a wrapper
   script or a `Makefile` that invokes CMake is explicitly **not** supported
   by this plan; such a repo keeps today's (no build commands) behaviour.

## Read first, verbatim

- `crates/hanihi-core/src/tool.rs`
  - `check_command_argv_mode` (~300-590) — the single gate. Read the whole
    function; the nesting and the `allowed_non_mutating` shadowing in each
    arm are load-bearing for how you will extend it.
  - `check_command_argv` (~295-298) — the `#[cfg(test)]` wrapper that pins
    tests to `CommandMode::ReadOnly`.
  - `CommandMode` (~283-292) — the existing mode enum.
  - `NON_MUTATING` (~213-282) — the flat binary allowlist. Note `find` is
    both listed here and chained in again at ~328; do not replicate that
    pattern for new entries.
  - `check_git_*` helpers (~595-660) — the established shape for a
    sub-validator that takes the full argv and returns `Result<(), String>`.
  - `scrubbed_env` (~655-665) — what the child process sees. C++ toolchains
    resolve their own headers from PATH; confirm nothing extra is needed and
    **do not** export `CXXFLAGS`, `CXX`, `CC`, or `CMAKE_*` into the child
    environment from here.
  - `execute_captured` (~670-745) and `builtin_run_command_for` (~805-900) —
    how a validated argv is run. `cwd` is `tree.root()`; that is not
    changing.
  - `mod tests` (~920-end) — the allowlist test tables. Every new allowed
    command needs a deny-test sibling; the existing
    `command_allowlist_denies_*` functions are the template.
- `crates/hanihi-core/src/source.rs` — read only. `SourceTree::root`,
  `is_ignored`, and `resolve_for_write` are the escape-refusal patterns to
  mirror. Do **not** modify this file; plan 1 owns it.
- `docs/` or `target/doc` for `ToolExecutionError` — confirm which
  constructors exist (`invalid_args`, `permission_denied`, `not_found`,
  `provider`).

## Findings (confirmed)

1. `check_command_argv_mode` dispatches on `argv[0]` over a flat
   `allowed` vector built from `NON_MUTATING` chained with
   `["cargo", "git", "find"]`. A program not in that vector is rejected
   before any subcommand logic runs.
2. Each arm re-declares a local `allowed_non_mutating` array with a
   `val if allowed_non_mutating.contains(&sub.as_str()) && val == sub.as_str()`
   guard. That guard is equivalent to a plain `contains` check; copy the
   shape for consistency but do not propagate the confusion.
3. Escape refusals are enforced at two levels: `--manifest-path` (cargo) and
   `-C`/`--directory` (git) are rejected by scanning the whole argv before
   dispatch; `git archive --output`/`--remote` and `git apply` without
   `--check` are rejected inside their arms.
4. `check_command_argv_mode` currently takes only `&[String]` and
   `CommandMode`. It has no access to the `SourceTree`, so it cannot yet ask
   "is this a CMake repo?". `builtin_run_command_for` *does* hold
   `tree: Arc<SourceTree>` in scope at the call site.
5. The allowlist is enforced before `execute_captured`, so anything admitted
   here runs with the scrubbed environment and the pinned cwd. The allowlist
   is the *only* security boundary for command execution.

## Design

### A. Do not fork the gate

Keep `run_command` as the single tool and `check_command_argv_mode` as the
single allowlist function. Do not add a parallel `run_build` tool and do not
add a second validator. The design's strength is that one function is the
sole gate; a second copy drifts, and a security boundary that exists twice is
a security boundary that is wrong once.

### B. How the gate learns the toolchain

Two options; pick the second.

- *Option 1:* `check_command_argv_mode` takes a `Toolchain` parameter, and
  `builtin_run_command_for` reads `tree.toolchain()` at the call site and
  passes it in.
- *Option 2 (chosen):* `check_command_argv_mode` enforces only what is
  *inherently* safe and independently of any repository — pure syntax and
  escape rules — and accepts the CMake/compiler commands for **every** repo.
  Repository-shape facts (is there a `build/` directory? does a
  `compile_commands.json` exist?) are **not** consulted.

Rationale for option 2: the allowlist's job is to bound what a command can
do, not to guess what a repository wants. `cmake --build build` in a Rust
repo fails immediately with a clear error and costs one turn; gating it on
detected toolchain buys nothing and couples this security-critical function
to plan 1's `Toolchain` enum, which may not exist yet. Keeping the gate
toolchain-independent is what makes this plan genuinely independent of plan
1, and it preserves the property that the gate is a pure function of the
argv.

The tradeoff is one wasted turn in a misinvoked repo, and a slightly longer
tool description to advertise the commands. Accept both. Do not add a
`--toolchain` flag.

### C. Allowed CMake commands

Add a `"cmake"` arm. Admitted subcommands, each requiring the `build/`
convention and the pinned cwd:

- `cmake -B build` — configure into `build/`. The path argument to `-B` must
  be exactly `build`, or a multi-component relative path whose first
  component is `build` (e.g. `build/Debug`). This is the one escape valve
  and it must be validated, not merely pattern-matched.
- `cmake --build build` — build. Optionally followed by `--target <name>`,
  `-j`/`--parallel` with a numeric argument, and `--config <name>`.
- `cmake --build build --target <name>` — the per-target form, same rules.
- `ctest --test-dir build` — run tests, optionally with `--output-on-failure`
  and `-j` with a numeric argument.

Explicitly **rejected**, each with a specific message rather than the generic
"not allowed", because a precise refusal teaches the model the right command:

| Rejected | Why |
|---|---|
| `-P <file>`, `--script <file>` | Arbitrary CMake code execution. This is the `sh -c` of CMake. |
| `-S <path>`, `--source <path>` | Absolute or escaping source root. Out-of-source builds work from the cwd. |
| `-B <path>` where the path is not under `build/` | Writes build trees anywhere the process can reach. |
| `--install <dir>` | Writes outside the repository. |
| `-D CMAKE_INSTALL_PREFIX=...` | Same, via configure. |
| `--prefix <dir>` | Same. |
| `-D CMAKE_CXX_COMPILER=...`, `-D CMAKE_C_COMPILER=...`, `-D CMAKE_MAKE_PROGRAM=...` | Names an arbitrary executable to run. |
| `-G <generator>` | Some generators are exotic; more importantly it is not needed for the supported layout. |
| `--fresh`, `-U`, `-E`, `-H` | Not required; `-E` in particular is a command dispatcher and must stay out. |

Validate `-B` properly: split on `/` (and `\\` on Windows, though only Linux
is a target today — handle `/` and treat a backslash as an ordinary
character), reject any `..` or empty component and any leading `/`, and
require the first component to equal `build`. Reuse the `Component`-based
refusal pattern from `resolve_for_write` rather than hand-rolled string
matching where it fits; a `Path::new(arg).components()` scan that rejects
`ParentDir`, `RootDir`, and `Prefix` plus a `starts_with("build")` check is
the expected shape.

### D. Allowed compiler commands

Add `"g++"`, `"gcc"`, `"clang++"`, and `"clang"` arms. The permitted shape is
deliberately narrow:

- **`-c` is required** for the compiler to be admissible at all. Compile and
  link is what `cmake --build` is for; a bare `g++ foo.cpp -o /tmp/x`
  produces an executable at an arbitrary path and is the exact thing this
  plan must not admit.
- `-o <path>` is **rejected** even with `-c`. Object files land next to
  their source or in the build tree per the project's own rules;
  `clang++ -c` with no `-o` writes `foo.o` in the cwd, which is inside the
  repo and is covered by the existing `.ignore` template. Admitting `-o`
  means validating an output path for no real benefit.
- `-MF`, `-MT`, `-MQ`, `-MJ`, and `--serialize-diagnostics` write files at
  named paths. **Reject all of them.**
- `-fplugin=`, `-Xclang`, `-Xlinker`, `-Wl,`, `-B` (compiler's own
  `-B <prefix>`), and `-wrapper` make the compiler load or run another
  program. **Reject all of them.** Note the collision: `-B` means
  "build directory" to CMake and "find programs here" to GCC/Clang. They are
  different arms, so the meanings do not actually clash, but the tests must
  cover both so a future refactor cannot merge them.
- `@<file>` response files pull arbitrary flags from disk. **Reject any
  argument starting with `@`.**
- Everything else — `-I`, `-isystem`, `-D`, `-U`, `-std=`, `-O`, `-g`,
  `-Wall`, `-Werror`, `-f...`, `-m...`, `-pthread`, `--target=`, `-fsyntax-only`
  — is admitted, because it only affects compilation of the one input file.
  Do not attempt to enumerate them; enumerate only the rejections.

At least one input operand is required: an invocation with no non-flag
argument is a usage error, not a compile. Reject `-` (stdin) as the input.

Validation shape: a shared `check_compiler_argv(argv) -> Result<(), String>`
that both the `g++`/`gcc` and `clang++`/`clang` arms call. Keeping stdout
capture in mind, the result of a successful `-c` is an object file the agent
never sees; the value is the diagnostics.

`-fsyntax-only` is worth calling out in the tool description as the fast
"does this still parse" check, since it is the cheapest possible iteration
step and the agent will not discover it otherwise.

### E. Description text

There are already two description constants: the read-only inline string in
`builtin_run_command_for` and `RUN_COMMAND_DESC_WRITE`. Extend both to
mention `cmake`/`ctest` and the per-file compiler check, and keep the
existing style (escaped line continuations, sentences separated by
semicolons in the summary clause). The read-only description must not
advertise `git add`/`git commit`; the write description must not advertise
`cmake -P`. Neither is a place for a flag list — the model can call
`cmake --help` if it needs one, and the description's job is to make the
*existence* of the commands discoverable.

Since the allowed commands are the same in both modes (§B), the two
descriptions differ only by the git housekeeping verbs, exactly as today.

### F. Interaction with the tool-result cache

No change needed, but confirm: `run_command` is not in the read-only cache
set (the cache covers `read_file`, `list_dir`, `grep`, `read_session_log`,
`echo`), so compile commands are never deduplicated. That is correct — a
build result can change between identical invocations after a write. Do not
add it to the cache.

## Work order, test-first

Write the failing allow/deny tests first, then implement B-E, then run the
gates. The test tables are the primary artifact of this plan; treat a new
allowed command without a paired deny-test as incomplete.

### New tests in `tool.rs` `mod tests`

Follow the existing table style — `argv(&[...])` plus a `for` loop asserting
`check_command_argv_mode(&cmd, mode)` succeeds or fails.

**Allowed (assert in *both* `ReadOnly` and `Write` mode, since the gate is
toolchain- and mode-independent for these):**

- `cmake -B build`
- `cmake -B build/Debug`
- `cmake --build build`
- `cmake --build build --target mylib`
- `cmake --build build -j 8`
- `cmake --build build --parallel 8`
- `cmake --build build --config Release`
- `ctest --test-dir build`
- `ctest --test-dir build --output-on-failure`
- `g++ -c src/foo.cpp -Iinclude -std=c++20 -Wall -Werror`
- `g++ -c src/foo.cpp -fsyntax-only`
- `clang++ -c src/foo.cpp -DNDEBUG`
- `gcc -c src/foo.c`
- `clang -c src/foo.c`

**Denied — CMake:**

- `cmake` with no subcommand
- `cmake -P script.cmake`
- `cmake --script script.cmake`
- `cmake -P ../../evil.cmake`
- `cmake -S . -B build`
- `cmake -S /etc -B build`
- `cmake --source .`
- `cmake -B /tmp/build`
- `cmake -B ../build`
- `cmake -B build/../../etc`
- `cmake --build /etc`
- `cmake --build ../build`
- `cmake --build build --install build` (note: `--install` is a `build`-mode flag)
- `cmake --install build`
- `cmake -D CMAKE_INSTALL_PREFIX=/usr -B build`
- `cmake -D CMAKE_CXX_COMPILER=/usr/bin/evil -B build`
- `cmake -D CMAKE_MAKE_PROGRAM=evil -B build`
- `cmake -G Ninja -B build`
- `cmake -E rm -rf build`
- `cmake --fresh -B build`
- `cmake -H. -Bbuild`

**Denied — compiler:**

- `g++ src/foo.cpp` (linking, no `-c`)
- `g++ -c src/foo.cpp -o /tmp/x.o`
- `g++ -c src/foo.cpp -o foo.o`
- `g++ -c src/foo.cpp -MF dep.d`
- `g++ -c src/foo.cpp -MT target`
- `g++ -c src/foo.cpp -MQ target`
- `g++ -c src/foo.cpp -MJ out.json`
- `g++ -c src/foo.cpp -fplugin=evil.so`
- `g++ -c src/foo.cpp -B /usr/lib/evil`
- `g++ -c src/foo.cpp -wrapper evil`
- `g++ -c src/foo.cpp @args.rsp`
- `g++ -c -` (stdin input)
- `g++ -c` (no input operand)
- `clang++ -c src/foo.cpp -Xclang -load -Xclang evil.so`
- `clang++ -c src/foo.cpp --serialize-diagnostics out.dia`
- `echo hi | g++ -c src/foo.cpp` — asserting the argv is `["echo", "hi"]`
  is not meaningful here; instead assert separately that an argv containing
  `-Wl,-rpath,/tmp` is denied.

**Denied — unchanged behaviour (regression):** re-run the existing
`command_allowlist_denies_unknown_and_disallowed`,
`command_allowlist_denies_cwd_escapes`,
`command_allowlist_read_only_denies_housekeeping`, and
`command_allowlist_write_still_denies_history_verbs` tables unchanged. Add
`cmake` and `g++` invocations that resemble escapes (`cmake -B ../build`,
`g++ -c ../outside.cpp` — the latter is admitted *only* if an escaping
input path is considered acceptable; decide explicitly).

### A deliberate decision to make and document

`g++ -c ../../outside.cpp` compiles a file outside the repository. The
compiler only *reads* it and writes its `.o` next to it (or in the cwd),
which is outside the repo. Choose: reject any input operand whose path
escapes the repo root (using `Path::new(arg).components()`), or accept that
reading outside the repo is already possible via `-I/../..`.

**Decision: reject escaping input operands.** It is cheap, it makes the
refusal surface consistent with `run_command`'s existing escape philosophy,
and `-I` pointing outside is a much weaker capability than compiling an
arbitrary file. Implement it and test it. Note the residual gap in the
plan's risks section: an admitted `-I/../../..` can still read headers
outside the repo, so this is a narrowing, not a seal.

### Manual smoke test

After building, in a scratch CMake project inside a git repo, drive the tool
by hand (or via the eval harness once plan 4 lands) and confirm:

1. `cmake -B build` configures and produces `build/CMakeCache.txt`.
2. `cmake --build build` compiles.
3. `ctest --test-dir build` runs.
4. `g++ -c src/foo.cpp -fsyntax-only` returns diagnostics.
5. `git status --short` shows only expected changes, i.e. `build/` is
   ignored by the generated `.ignore` template. **If it is not, stop and
   report** — that is plan 5's job and this plan must not fix it, but the
   smoke test must not be reported as passing while the build tree pollutes
   the repo listing.

## Verification gates

```text
cargo fmt
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acceptance criteria

- `check_command_argv_mode` remains the single allowlist function and the
  single entry point for command validation.
- `cmake -B build` (and `build/...` subdirectories), `cmake --build build`
  with the bounded flag set, `ctest --test-dir build`, and
  `<compiler> -c <file>` are admitted.
- Every rejection listed in C and D is asserted by a test. A new admitted
  command with no deny-test is an incomplete change.
- `-P`/`--script`, `-S`/`--source`, any `-B` outside `build/`, `--install`,
  `CMAKE_INSTALL_PREFIX`, the three `CMAKE_*_COMPILER`/`MAKE_PROGRAM`
  variables, `-G`, `-E`, `@file`, `-o`, all dependency/diagnostic output
  flags, and all plugin/linker passthrough flags are refused with a
  specific, instructive message.
- Compiler invocation requires `-c` and at least one in-repo input operand.
- Behaviour for Rust repositories is unchanged: every existing allowlist
  test passes verbatim, and `cargo`'s admissions and refusals are identical.
- No new dependencies. No change to `SourceTree`, to `execute_captured`,
  to `scrubbed_env`, or to the pinned-cwd model.
- No shell is introduced; argv remains a trusted vector of separate strings.

## Out of scope

- `make` on a validated target, or any wrapper-script-driven build. The user
  chose the `build/` convention; a `Makefile`-driven CMake project remains
  unsupported by design.
- In-source builds.
- `ninja`, `meson`, `bazel`, autotools, MSVC (`cl.exe`), or cross-compilers.
- `cmake --install`, `cpack`, and any packaging or install path.
- Reading or acting on `Toolchain` — this plan does not depend on plan 1.
- Making the system preamble mention the new commands — plan 3.
- Eval assertions that exercise a C++ build — plan 4.
- Fixing gaps in the C/C++ `.ignore` template so a build tree is ignored —
  plan 5.
- Adding `run_command` to the read-only tool cache.

## Assumptions and risks

- **The allowlist is the security boundary.** Every admitted flag is a
  capability. The enumerated rejections in C and D are the load-bearing part
  of this change and must be reviewed flag by flag, not skimmed. If a review
  finds a flag not listed here that loads, writes to, or executes a
  caller-named path, it belongs in the rejection list and needs a test.
- **Residual escape via include paths.** `-I/anywhere` and `-isystem
  /anywhere` are admitted, so a compile can read headers outside the repo.
  This is a narrowing of capability, not a sandbox. Documented, not fixed.
- **`-B` is overloaded.** CMake's `-B` is the build directory; GCC/Clang's
  `-B` is a program search prefix. They live in different arms and must not
  be unified by a future refactor. Tests cover both.
- **Toolchain-independent admission costs turns in the wrong repo.** A model
  that runs `cmake --build build` in a Rust repo gets an error instead of a
  pre-emptive refusal. Accepted in exchange for keeping this gate a pure
  function of argv and independent of plan 1.
- **Generator differences.** `cmake -B build` picks the platform default
  generator, which on some systems is multi-config. `--config` is therefore
  admitted; `--target` is admitted; `-G` is not. A project that requires a
  non-default generator cannot be built by the agent. Accepted for now.
