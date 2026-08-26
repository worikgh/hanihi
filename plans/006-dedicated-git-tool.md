# Plan 006 — A dedicated git tool

**Status:** proposed | **Created:** 2026-08-25 | **Scope:** core + CLI + tests

## Problem

The agent runs git only through `run_command`, whose `command` argument is a
free-form whitespace-split string naming the verb (`git status`, `git diff`,
...). The model must remember to (a) name the tool `run_command` and (b) put
the verb inside the `command` string. When a model emits a tool call named
`git status` instead, the harness fails with `unknown tool: git status`.

That is an *indirection the model must remember*: the git verb is not itself
a tool name, so nothing forces the model to get the routing right. The 005
plan folded git verbs into `run_command` to keep the schema count low; the
operational cost is false tool-call failures.

## Goal

Register **one** dedicated, read-only git tool — `git_status` — as a
first-class tool name (`builtin_git_status`) alongside `run_command`. It is
a thin, fixed-verb wrapper over the existing allowlisted execution path, so
the model can call `git_status` by name without remembering the
`run_command` indirection. `run_command` remains for `cargo` and as the git
fallback; the allowlist still guards every path.

Scope is deliberately one tool (`git_status`) to keep the change minimal and
fully tested. The same pattern extends later to `git_diff`, `git_log`, etc.

## Execution constraint: no git tools used during this work

While implementing **I will not invoke any git tool** in my own workflow to
build or observe the change. Instead I will:

- use `read_file`, `list_dir`, `write_file`, and `apply_patch` to author the
  code and tests;
- use `cargo test` / `cargo build` / `cargo clippy` (via `run_command`) to
  compile, test, and lint.

The only `git` invocations that occur are **inside the code under test** —
the existing `git_repo()`/`Fixture` test scaffolding runs `git init`, `git
add`, `git commit`, and the tool under test itself shells `git status`. Those
are part of the test harness's runtime, not my workflow, and are required to
prove the tool works. I will not, e.g., run `git status`/`git diff` to
monitor my own edits.

## Design

### 1. Extract a shared argv executor

`builtin_run_command`'s closure does: split `command` → argv →
`check_command_argv` → `execute_captured` → `write_trace` →
`render_command_result`. Refactor the tail into a reusable helper so a
*fixed* argv can run without going through the free-form string:

```
async fn run_captured_argv(
    tree: &SourceTree,
    traces_dir: &Path,
    argv: &[String],
    timeout_secs: u64,
) -> Result<String, ToolExecutionError>
```

- calls `check_command_argv(argv)` (permission_denied on deny)
- calls `execute_captured(argv, tree.root(), timeout)`
- calls `write_trace` and `render_command_result`
- returns the rendered text

`builtin_run_command` keeps its `command`-splitting behaviour and calls
`run_captured_argv`. No other tool changes. `run_captured_argv` is `async fn`
and unit-testable directly (T5).

### 2. New builder: `builtin_git_status`

In `tool.rs`, add:

```
pub fn builtin_git_status(tree: Arc<SourceTree>, traces_dir: PathBuf) -> PortableDynamicTool
```

Same signature as `builtin_run_command`. It passes a **literal** argv:

```
["git", "status", "--short"]
```

No `command` argument exists in the schema, so there is nothing for the
model to mis-prefix and no way to reach another verb. The schema:

```json
{ "type": "object", "properties": {}, "required": [] }
```

The closure calls `run_captured_argv(tree, traces_dir, &argv, DEFAULT_TIMEOUT_SECS)`.

Splitting `git status --short` on whitespace reproduces the existing allowlist
case `argv(&["git", "status", "--short"])` already accepted by
`check_command_argv` — so the allowlist needs **no** change.

### 3. Registration

- `lib.rs`: re-export `builtin_git_status`.
- `main.rs`: register `builtin_git_status(...)` in the `Some(tree)` block
  alongside `run_command` (registration is unconditional / read-only).

`traces_dir` is currently moved into `builtin_run_command(tree.clone(),
traces_dir)`. The dedicated tool also needs it, so the CLI builds the
`traces_dir` `PathBuf` once and `clone()`s it for each tool (it is an owned
`PathBuf`; cloning is cheap).

## Tests (in `tool.rs`)

Use the existing `git_repo()` fixture (a real repo with one initial
commit, user.email/name configured) and the existing `Fixture` for ignored
paths. All new tests live in the existing `tool::tests` module and reuse the
existing helpers (`git_repo`, `argv`).

### T1 — reaches the allowlist and runs

Build `builtin_git_status` over `git_repo()`, execute with `{}`, assert
rendered output contains `exit code: 0` and `trace:`. Mirrors
`run_command_tool_runs_git_and_writes_trace`.

### T2 — writes a trace file

After T1, assert a `*.cmd.json` file exists in the traces dir and the raw
contents record the `git status --short` command.

### T3 — reports a dirty tree (corner case)

In `git_repo()`, append a line to `src/main.rs` (unstaged change), run
`git_status`, assert the rendered output names the modified file. Proves the
tool surfaces real status, not just exit 0. Also add an untracked file and
assert it appears as `??` (untracked corner case).

### T4 — clean-tree empty output (corner case)

On a fresh `git_repo()` (no changes), `git status --short` prints nothing on
stdout but still exits 0. Assert the tool returns a *successful* result
(`exit code: 0`) and does not error — the empty result is not a failure.

### T5 — `run_captured_argv` still denies bad verbs (corner case)

Pass a literal `["git", "push", "origin", "main"]` to `run_captured_argv`
and assert `permission_denied`. Guards the refactored helper, not just
`check_command_argv`.

### T6 — fixed verb is structurally immune (corner case)

Assert the literal argv is exactly `["git", "status", "--short"]` and
`check_command_argv` accepts it. Assert that executing the tool with a
spurious `command` field still runs `git status --short` (the field is
ignored because the schema has no such property and the closure never reads
args). This proves no user input can redirect the verb.

### T7 — read-only: no commit created (corner case)

In `git_repo()`, record the HEAD commit hash, make an unstaged edit, run
`git_status`, then assert the HEAD hash is unchanged (no commit was made).
Uses `git rev-parse HEAD` only inside the *test* to verify; the tool itself
never stages or commits.

## Corner cases (summary)

- modified + untracked dirty tree (T3)
- clean tree / empty stdout (T4)
- deny of a non-allowlisted verb through the shared helper (T5)
- fixed-verb immunity to stray arguments (T6)
- read-only — no commit / no staging (T7)
- single-commit repo (all fixtures use `git_repo()` which has one commit)
- trace persistence (T2)
- non-existent repo — not reachable: `SourceTree::open()` fails before tool
  registration, so `git_status` is only callable inside a valid repo.
- timeout — reuses `DEFAULT_TIMEOUT_SECS`; `git status` never hangs.

## Acceptance criteria

- `cargo test` passes — all existing + new tests.
- `cargo clippy -- -D warnings` is clean.
- `cargo build` succeeds.
- The model can call `git_status` by name; the harness resolves it (no
  `unknown tool`).
- `run_command` still works for `cargo` and remains a git fallback.
- No git subcommand beyond the existing allowlist becomes reachable; the
  new tool's verb is fixed and literal.
- Implementation performed without me invoking any git tool (per the
  execution constraint above).

## Out of scope

- `git_diff`, `git_log`, `git_show`, `git_check_ignore` dedicated tools —
  same pattern, deferred.
- Write git tools (commit/add) — the allowlist and `--write` gating are
  deliberately unchanged; this plan adds only a read-only verb.

## Safety

Every rail is reused unchanged: no shell, literal/whitespace-split argv,
allowlist, cwd pinned to repo root, scrubbed env, timeout, output caps,
trace persistence. The dedicated tool adds a better-named *read-only* path —
**not** a relaxation.
