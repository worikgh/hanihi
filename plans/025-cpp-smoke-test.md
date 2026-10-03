# Plan 025 — Manual smoke test for the C++ command allowlist

## Purpose

Verify that the allowlist added in plan 021 actually admits a real CMake
workflow, end to end, outside the unit tests. The `tool::` tests call
`check_command_argv_mode` directly. They prove the gate says "yes"; they do
not prove that a command reaches a process, that `execute_captured` runs it
with the pinned cwd and the scrubbed environment, or that the result the
agent sees is usable. Those are different claims, and only the first is
currently covered.

This is a verification exercise, not a feature. No production code changes
unless step 6 fails, and then the correct action is to stop and report, not
to fix.

## What is being tested

| Claim | Evidence |
|---|---|
| `cmake -B build` configures | `build/CMakeCache.txt` exists afterwards |
| `cmake --build build` compiles | a binary exists in `build/`; output shows the compiler running |
| `ctest --test-dir build` runs tests | ctest reports a test as passed |
| `g++ -c ... -fsyntax-only` returns diagnostics | stderr carries a diagnostic, exit code non-zero for a broken file |
| `build/` is ignored | `git status --short` shows no `build/` entries |
| the environment is scrubbed | no `CC`/`CXX`/`CFLAGS` leak; the toolchain resolves from PATH |

## Preconditions

```sh
git log --oneline -3          # both plan-021 commits present
git status --short            # clean apart from untracked plans/ and working/
cargo build --workspace       # binary built from the current tree
cmake --version               # cmake on PATH
g++ --version                 # g++ on PATH (or clang++, and adjust)
ctest --version               # ctest on PATH
```

If `cmake`, `g++`, or `ctest` is missing, the smoke test cannot run. That is
a finding in itself: the plan admits commands the deployment environment may
not have, and the failure mode should be observed rather than assumed. If a
tool is absent, note it and skip the corresponding step; do not install
anything.

## The scratch project

Create the project **outside** the repository under test. This is important:
the point is to drive the agent against a foreign CMake project, not to build
Hānihi itself. Use a temp directory.

```
$SCRATCH=$(mktemp -d)/cpp-smoke
mkdir -p "$SCRATCH/src"
cd "$SCRATCH"
git init -q
git config user.email smoke@example.com
git config user.name smoke
```

`CMakeLists.txt`:

```cmake
cmake_minimum_required(VERSION 3.16)
project(smoke CXX)

add_library(greet STATIC src/greet.cpp)
target_include_directories(greet PUBLIC include)

add_executable(hello src/main.cpp)
target_link_libraries(hello PRIVATE greet)

enable_testing()
add_test(NAME hello_runs COMMAND hello)
```

`include/greet.hpp`:

```cpp
#pragma once
#include <string>
std::string greeting(const std::string& who);
```

`src/greet.cpp`:

```cpp
#include "greet.hpp"
std::string greeting(const std::string& who) { return "hello, " + who; }
```

`src/main.cpp`:

```cpp
#include "greet.hpp"
#include <iostream>
int main() { std::cout << greeting("world") << "\n"; }
```

Commit it:

```sh
git add -A && git commit -q -m "scratch cmake project"
```

Then open Hānihi with `$SCRATCH` as its repository root, so the tool's pinned
cwd is the scratch project and the allowlist sees `argv[0]=cmake` inside a
CMake repository. The agent has no shell, so the commands below are issued as
`run_command` calls with the exact argv shown — not typed into a terminal.

## The steps

Each is a `run_command` invocation with the argv given. Record for each: the
exit code the tool reports, whether the command reached a real process, and
whether output was truncated at 64 KiB.

### 1. Configure

```
cmake -B build
```

Expect: exit code 0; `build/CMakeCache.txt` exists; output mentions the
generator and the compiler it selected. Record which generator was chosen —
plan 021's risks section notes that a multi-config generator makes `--config`
relevant and `--target` still work, so this documents the case.

Sanity checks on the environment:

- the configure output names a compiler path resolved from `PATH`, not from an
  inherited `CXX`;
- `CXXFLAGS` is *not* mentioned anywhere in the configure log. Plan 021
  forbids exporting it from `scrubbed_env`, and this is where a leak would
  show.

### 2. Build

```
cmake --build build
```

Expect: exit code 0; `build/hello` and `build/libgreet.a` exist; the build log
shows compiler invocations.

Then run the same command a second time: expect a no-op build, exit code 0,
"nothing to do" style output. This confirms the command is not deduplicated by
a cache — plan 021 requires `run_command` stay out of the read-only cache set,
and a second invocation must actually re-run.

### 3. Targeted build

```
cmake --build build --target greet
```

Expect: exit code 0. This exercises the `--target` form and confirms it builds
one target rather than the default.

### 4. Test

```
ctest --test-dir build
```

Expect: exit code 0; output names `hello_runs` and reports 1 test passed. Then:

```
ctest --test-dir build --output-on-failure
```

Expect: the same result; the flag is admitted.

Deliberately break the test to confirm the failure path is visible: edit
`src/main.cpp` to `return 1;`, rebuild, rerun `ctest`. Expect a non-zero exit
code and, with `--output-on-failure`, the program's output in the tool result.
Restore the file afterwards. This matters because a smoke test that only ever
sees success does not prove the agent can *observe* failure, which is the whole
point of the loop.

### 5. Per-file compile

Syntax-only, on a file that is fine:

```
g++ -c src/greet.cpp -fsyntax-only
```

Expect: exit code 0, no diagnostics.

Now the case that carries the real information — a file with an error.
Introduce one:

```cpp
std::string greeting(const std::string& who) { return "hello, " + who }   // missing semicolon
```

and run the same command. Expect:

- exit code non-zero;
- stderr contains a diagnostic naming the file and line;
- the diagnostic survives the 64 KiB cap intact.

Restore the file. Then confirm the admitted-but-unflagged form works too:

```
g++ -c src/greet.cpp -Iinclude -std=c++20 -Wall -Werror
```

Expect exit code 0 and a `.o` file written **in the cwd**, since `-o` is
refused by design. Note where it landed; it should be beside the source or in
the repository root, and it must not be an escape.

### 6. The ignore check — the one that can fail

```
git status --short
```

Expect: no line mentioning `build/`. Nothing else should be listed either,
since the scratch project was committed clean and the build went into
`build/`.

**This is the step plan 021 flags as possibly failing.** The generated
`.ignore` is produced by `SourceTree::open_at` via `ensure_ignore_file`, using
the language template. For a C/C++ project the template includes `build/`, so
it should be covered — but the template is selected by `detect_languages`,
which scans two levels deep for C-family extensions and marker files, and that
is exactly the sort of thing that has an off-by-one.

If `build/` **does** appear in `git status --short`:

1. **Stop.** Do not attempt a fix.
2. Capture the evidence: the `git status --short` output, and the contents of
   `.ignore` in the scratch repository.
3. Report it. Per plan 021, gaps in the C/C++ `.ignore` template are plan 024's
   responsibility, and fixing it here would conflate two changes.
4. The smoke test is **not** to be reported as passing. Steps 1–5 may all have
   succeeded; step 6 failing still means the build tree pollutes the repository
   listing, which is a real defect in the agent's view of the repository.

Also check:

```sh
cat .ignore | head -20
```

to confirm the file was created at all and carries the hānihi header.

### 7. Read-back

The agent's workflow is edit → build → read the result. Confirm the loop
closes:

```
grep -n "greeting" src/greet.cpp
cmake --build build
```

and that the tool result for the build is legible — exit code, duration, and
the compiler's own output, not a wall of noise. That is the observable payoff
of the whole change, and it is worth looking at rather than assuming.

## What to record

For each of the seven steps:

- the exact argv issued;
- the exit code reported by `run_command`;
- whether the command actually ran (timestamp in the trace file, or a side
  effect on disk);
- for step 5, the diagnostic text;
- for step 6, pass or fail, with the raw `git status --short` output.

Write it up as a short report. Plan 021 asks for evidence, not a summary.

## Failure handling

| Symptom | Meaning | Action |
|---|---|---|
| `command 'cmake' is not allowed` | the gate rejected a command plan 021 admits | bug in plan 021; report the exact argv |
| command admitted but `failed to spawn` | binary not on `PATH` in the deployment environment | environment finding, not a code defect |
| `build/` in `git status --short` | `.ignore` template gap | stop and report; plan 024's job |
| `CXX`/`CXXFLAGS` visible in configure output | `scrubbed_env` is leaking | security-relevant; report immediately |
| a second `cmake --build build` skipped entirely | tool-result cache interfering | bug; plan 021 requires `run_command` stay uncached |

## Scope boundaries

Explicitly **not** part of this smoke test:

- fixing the `.ignore` template (plan 024);
- making the system preamble mention the new commands (plan 022);
- any eval-harness assertion (plan 023);
- building Hānihi's own repository with `cmake` — there is nothing to build;
- `make`, `ninja`, in-source builds, `cmake --install`, or any wrapper script.
  Those are out of scope by design in plan 021, and a smoke test that tries
  them would be testing the wrong thing.

## Deliverable

A report with the seven steps' evidence, a clear pass/fail for step 6, and a
statement of which of the acceptance criteria in plan 021 are now verified end
to end rather than only by unit test.

If every step passes including step 6, plan 021 is complete. If step 6 fails,
plan 021 is complete except for a documented, deferred defect, and it must be
reported as such.
