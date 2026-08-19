# Plan 005 — Self-Improvement

**Status:** draft | **Created:** 2026-08-19 | **Scope:** core + CLI + eval

## Overview

Give hānihi the tools to improve its own code: analyse the source, build it,
run it, study the traces, patch it, and verify the result. The read side
(`SourceTree`, session event log, eval harness) already exists; what's
missing is **command execution**, **a write path**, and the **loop** that
ties them together.

The design principle: **read-only by default, writes gated, changes are git
commits, improvement is measured by the eval harness — never self-reported.**

---

## Architecture

### 1. New tool: `run_command` — build & run code (the critical gap)

Execute a command inside the repo root, capture stdout + stderr + exit code
+ duration, return a truncated preview, persist the full output to a trace
file.

```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "Allowlisted command, e.g. \"cargo check --workspace\"" },
    "timeout_secs": { "type": "integer", "description": "Max seconds (default 120, max 600)" }
  },
  "required": ["command"]
}
```

**Safety model (non-negotiable):**

- **No shell.** Split the command string into argv (`split_whitespace`, same
  as the CLI already does for `--mcp-command`). `sh -c` is never used — no
  `;`, `&&`, pipes, or redirection reach the OS.
- **argv[0] allowlist**: `cargo`, `git`. Cargo subcommands limited to
  `check`, `build`, `test`, `clippy`, `fmt`, `doc`, `run -p hanihi-eval`.
  Git limited to read-only verbs: `status`, `diff`, `log`, `show`, `apply
  --check` (see §2). Everything else → `permission_denied`.
- **cwd pinned to repo root** (from `SourceTree::root()`). No `--manifest-path`
  or `-C` that escapes it.
- **Timeout kills the child** (tokio `process` + `time` features already in
  workspace deps). No PTY, no stdin.
- **Environment scrubbed**: pass a minimal env (PATH, HOME, CARGO_*); drop
  API keys.
- **Output capped** at `MAX_READ_BYTES` (64 KiB) in the tool result, with a
  truncation note like `read_file` uses.

**Trace persistence:** every run is written to
`<working-dir>/traces/<session>/<turn>-<n>.cmd.json`:

```json
{
  "ts": "...", "turn": 3, "command": "cargo build --workspace",
  "exit_code": 101, "duration_ms": 18402,
  "stdout": "...", "stderr": "..."
}
```

The tool result returns exit code + duration + truncated stdout/stderr + the
trace file path. The agent can then ask for more of the trace (see §5)
without blowing context.

### 2. New tools: `apply_patch` + `write_file` — the write path

The agent today can read but never touch source. Both tools reuse
`SourceTree::resolve` (escape refusal) and `is_ignored` (ignore rules).

**`apply_patch` (preferred — diff-shaped, reviewable, token-efficient):**

```json
{
  "type": "object",
  "properties": {
    "diff": { "type": "string", "description": "Unified diff (git diff format) against the current working tree" },
    "message": { "type": "string", "description": "Commit message; omit to leave the change uncommitted" }
  },
  "required": ["diff"]
}
```

Implementation: `git apply --check` (validate) → `git apply` (apply) →
optionally `git commit` with the message. Any check failure returns the full
`git apply` stderr to the model so it can fix the patch.

**`write_file` (fallback for new files / full rewrites):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path relative to the repo root" },
    "content": { "type": "string" },
    "message": { "type": "string", "description": "Optional commit message" }
  },
  "required": ["path", "content"]
}
```

Refusals: escapes, git-ignored paths, `.ignore`, anything under `.git/`.
**Never push** — commits only; git is the undo button.

**Registration gating:** write tools are registered **only** when
`--self-improve` is passed (CLI flag). Analysis + build tools are always on.

### 3. New tool: `grep` — content search

`read_file`/`list_dir` find files by name only. Add content search using the
`grep` crate family (already in the Rust ecosystem; `ignore::WalkBuilder`
reuse for the same filtering):

```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string", "description": "Regex" },
    "path": { "type": "string", "description": "Directory to search, relative to repo root (default: root)" },
    "ignore_case": { "type": "boolean" }
  },
  "required": ["pattern"]
}
```

Result: `file:line: text` lines, capped at ~200 matches / 64 KiB. Ignored
paths never searched.

### 4. New tools: `git_status` / `git_diff` (or fold into `run_command`)

The agent needs to see what it changed and what the tree looks like before
patching. Decision: **fold into `run_command`** — `git status --short`,
`git diff`, `git log --oneline -20`, `git show <rev>` are already on the
allowlist (§1). No separate tool definitions needed; fewer schemas, same
safety model.

### 5. New tool: `read_session_log` — study traces of execution

The event log is the agent's own trace, but it lives under
`working/sessions/<name>/` — invisible to `SourceTree`. Give the agent
a window into it:

```json
{
  "type": "object",
  "properties": {
    "kind": { "type": "string", "description": "Filter by event kind: user_input, llm_response, tool_execution, error, ..." },
    "turn": { "type": "integer", "description": "Filter by turn number" },
    "tail": { "type": "integer", "description": "Last N entries (default 50)" }
  }
}
```

Registered with the session's log path at startup (both are known before the
first turn). This is what lets a future session learn from past failures.

**Prerequisite fix — streaming log gap:** `Session::run_streaming` currently
logs `"(streamed)"` placeholders for tool args/result (session/mod.rs:628).
Fix it to log the real values before this tool is useful.

### 6. Eval extensions — the improvement gate

Extend `hanihi-eval` so "did it improve?" is measurable:

- **Case field `repo`**: optional path to a git repo. When set, the case
  enables source tools + `run_command` against that repo.
- **New assertions:**

| type | checks |
|------|--------|
| `build_succeeds` | `cargo check` (or per-case `build_command`) exits 0 |
| `tests_pass` | `cargo test` exits 0 |
| `clippy_clean` | `cargo clippy -- -D warnings` exits 0 |
| `no_diff` | working tree matches HEAD (agent must not leave junk) |

- **Compare mode** (stretch): `--baseline <results.json>` / `--compare` —
  diff pass/fail, token usage, latency between two runs. This is the
  before/after measurement for self-improvement.

### 7. Task mode — the loop

A long-horizon turn for self-improvement work:

- CLI: `--task "description"` + `--max-turns N` (agent API already supports
  `set_max_turns`; CLI doesn't expose it yet).
- Uses durable sessions (plan 003) so a multi-hour improvement session
  survives restarts.
- System prompt for task mode encodes the workflow gates (mirrors the rust
  skill): `cargo fmt` before staging → `cargo test` → `cargo build` →
  `cargo clippy -- -D warnings` → commit. Output is a sequence of
  `run_command` + `apply_patch` + `read_session_log` calls.

**How it rebuilds itself (safety):** the running hānihi process never
replaces its own binary mid-flight. The loop is: patch → `cargo build` →
run `cargo run -p hanihi-eval -- --cases-dir ...` against the *new* build →
compare with baseline. The current process adopts new code only on a normal
restart. A driver script (`scripts/self-improve.sh`) can orchestrate:
task mode → rebuild → eval → report, and it's the only component that ever
launches a fresh binary.

### Safety rails (summary)

| Rail | Where |
|------|-------|
| No shell; argv-split | `run_command` |
| argv[0] + subcommand allowlist | `run_command` |
| cwd pinned to repo root | `run_command` |
| Env scrubbed (no API keys) | `run_command` |
| Timeout + output caps | `run_command`, all tools |
| Escape + ignore refusal on reads and writes | `SourceTree` (existing + reused) |
| `.ignore`, `.git/`, session dir off-limits to writes | write tools |
| Writes only with `--self-improve` | CLI registration |
| Every change = commit; never push | write tools |
| Improvement measured by evals, not self-report | §6 |
| Human approval in REPL mode; unattended only via explicit task mode | CLI |

---

## Implementation order

### Step 1 — `run_command` + trace persistence
- `builtin_run_command` in `tool.rs` (tokio `Command`, argv-split, allowlist,
  timeout, cap)
- `working/traces/<session>/` writer; tool returns trace path
- Fix streaming log `"(streamed)"` gap (session/mod.rs)
- Tests: allowlist denials, escape denial, timeout, truncation

### Step 2 — Write path
- `apply_patch` via `git apply --check` + `git apply` (+ optional commit)
- `write_file` with `SourceTree` guards
- `--self-improve` flag gates registration
- Tests: patch round-trip, malformed diff, refused paths

### Step 3 — `grep`
- `grep` crate over `ignore` walk; cap matches/bytes
- Tests: ignore rules honoured, caps

### Step 4 — `read_session_log`
- Tool + streaming-log fix from Step 1 verified end-to-end
- Tests: filter by kind/turn, tail

### Step 5 — Eval extensions
- `repo` case field; `build_succeeds` / `tests_pass` / `clippy_clean` /
  `no_diff` assertions
- New eval cases: `003-self-build` (agent runs `cargo check` and reports),
  `004-self-patch` (agent applies a trivial patch, build still passes)

### Step 6 — Task mode
- `--task`, `--max-turns` CLI flags; task-mode system prompt with workflow
  gates

### Step 7 — Driver script
- `scripts/self-improve.sh`: task mode → rebuild → `hanihi-eval` → report;
  never touches the running binary

---

## What this is NOT

- **Not autonomous self-modification without oversight.** Writes require
  `--self-improve`; in REPL mode changes are human-approved; git history is
  the audit trail; push is impossible.
- **Not a full coding agent yet.** No tree-sitter symbol analysis, no
  multi-file refactoring intelligence, no persistent background workers.
  This plan adds the *loop*; the model does the thinking.
- **Not a benchmark.** Eval assertions are correctness gates (build passes,
  tests pass, no junk left behind), not "did the code get better" — that
  judgment stays with Fook.
