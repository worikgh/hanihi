# Plan 006 — Dedicated git tools

**Status:** proposed | **Created:** 2026-08-25 | **Scope:** core + CLI + tests

## Problem

`run_command` is a free-form, single tool that takes a whitespace-split
command string in its `command` argument. The model must remember to route
every git verb through `run_command` and pass the verb as its `command`
argument. When a model instead emits a tool call named `git status` or
`git diff`, the harness reports `unknown tool: <name>` and the turn fails.

The root cause is an *indirection the model must remember*: the git verb is
not a tool name, so there is nothing forcing the model to get it right.
The plan in `005` (§4) deliberately folded git verbs into `run_command` to
avoid extra schemas; in practice that choice depends on reliable model
behaviour that does not hold.

## Goal

Make git verbs first-class tool names so the model calls them directly by
name and cannot mis-prefix them. Concretely, register dedicated read-only
git tools alongside `run_command`:

- `git_status`
- `git_diff`
- `git_log`
- `git_show`
- `git_check_ignore`

Each is a thin, fixed-verb wrapper over the existing allowlisted
`execute_captured` machinery, with a **fixed** `git <verb>` argv — no
free-form command string that could point at another verb.

`run_command` stays for `cargo` (and for git verbs as a fallback) — the
`check_command_argv` allowlist still guards it, so the new tools merely
add a *better-named* path, not a relaxation.

## Design

### 1. Shared executor without a tool name

`run_command` currently builds `argv` by splitting the `command` string, then
passes it to `check_command_argv` and `execute_captured`. Refactor the
execution path so a fixed argv can run directly:

- Extract the body of `builtin_run_command`'s closure into a helper
  `run_argv(tree, traces_dir, argv, timeout_secs) -> Result<String, ToolExecutionError>`
  that calls `check_command_argv` + `execute_captured` + `write_trace` +
  `render_command_result`.
- `builtin_run_command` splits its `command` string into argv and calls
  `run_argv`.
- Each dedicated git tool constructs a **literal** `vec!["git", verb, ..]`
  argv and calls `run_argv`.

This reuses every safety rail (allowlist, cwd pin, scrubbed env, timeout,
output cap, trace persistence) with no duplication.

### 2. New tool builders

Add to `tool.rs`:

```
pub fn builtin_git_status(tree, traces_dir) -> PortableDynamicTool
pub fn builtin_git_diff(tree, traces_dir)   -> PortableDynamicTool
pub fn builtin_git_log(tree, traces_dir)    -> PortableDynamicTool
pub fn builtin_git_show(tree, traces_dir)   -> PortableDynamicTool
pub fn builtin_git_check_ignore(tree, traces_dir) -> PortableDynamicTool
```

Each takes the same `(tree: Arc<SourceTree>, traces_dir: PathBuf)` as
`builtin_run_command` and passes a fixed argv:

| tool         | argv                          |
|--------------|-------------------------------|
| `git_status` | `["git", "status", "--short"]` |
| `git_diff`   | `["git", "diff"]`            |
| `git_log`    | `["git", "log", "--oneline", "-20"]` |
| `git_show`   | `["git", "show", "HEAD"]`    |
| `git_check_ignore` | accepts a `path` arg → `["git", "check-ignore", "-v", <paths...>]` |

The schemas hold no free-form command string; the only free-form input is
`git_check_ignore`'s `paths` list, which is constrained to `check-ignore`
verb semantics. `git_check_ignore` needs a small extra argument so the model
can ask "is this path ignored?" — it is the read-only twin of the write-side
ignore checks.

### 3. Registration in the CLI / lib

- `lib.rs`: re-export the new builders.
- `main.rs`: register the dedicated git tools whenever the source tree is
  open (same block as `read_file`/`list_dir`/`grep`/`run_command`).

Registration is unconditional (read-only), consistent with `run_command`.

### 4. `git check-ignore` handling

`check_command_argv` currently allows the verb `check-ignore` (added in a
prior session). `git_check_ignore` passes `-v` and one or more repo-relative
paths from its `paths` argument. Paths are taken verbatim as argv elements
(no shell), and the cwd is pinned to the repo root, so a path cannot escape
via `-C`. The verb is fixed to `check-ignore`, so no other git verb is
reachable.

## Tests

### Unit tests in `tool.rs` (the allowlist already covers verbs):

1. **Each dedicated git tool reaches the allowlist and runs** — for every
   new tool, build it over a `git_repo()` fixture and assert the rendered
   output contains `exit code: 0` and `trace:` (mirroring the existing
   `run_command_tool_runs_git_and_writes_trace`).

2. **`git_check_ignore` reports ignored paths** — over a fixture with an
   ignored `target/`, run `git_check_ignore` with
   `paths: ["target/debug/junk.rs"]` and assert it identifies the path as
   ignored. Also check a non-ignored path (`src/main.rs`) reports not
   ignored (exit code 1 — verify the tool surfaces the non-zero exit, not
   an error).

3. **Fixed-verb invariance** — assert that the argv a dedicated tool
   constructs is exactly `["git", <verb>, ..]` and cannot be influenced by
   any argument. Because the argv is literal, there is no injection via
   `command`. Test that `check_command_argv` on the constructed argv for
   each verb returns `Ok`.

4. **`run_argv` still denies bad verbs** — a literal `["git", "push", ..]`
   argv passed to `run_argv` yields `permission_denied` (guards the
   refactored helper, not just `check_command_argv`).

### CLI-level

No new CLI test harness exists; the tool-level tests above cover the
behaviour. Verify the CLI registers the tools by running `cargo build` and
(optionally) `--tools`-style smoke, but a full REPL test is out of scope.

### Corner cases covered

- Non-zero `git check-ignore` exit (path not ignored) is surfaced as a
  successful tool run carrying the non-zero exit code, **not** an error —
  the agent must see "not ignored", not a harness failure.
- Empty `paths` for `git_check_ignore` → invalid_args (no argv, nothing to
  check).
- `git_log`/`git_show`/`git_diff` over a repo with a single commit — must
  not error.
- Dedicated tools run with the `git_repo()` fixture missing any config that
  would make `git status`/`diff`/`log` fail — the fixture configures
  user.email/name and an initial commit.
- The new tools write a trace file (same as `run_command`).

## Acceptance criteria

- `cargo test` passes (all existing + new tests).
- `cargo clippy -- -D warnings` is clean.
- The model can call `git_status`, `git_diff`, `git_log`, `git_show`,
  `git_check_ignore` by name and the harness resolves them (no
  `unknown tool`).
- `run_command` still works for `cargo` and remains as a git fallback.
- No git subcommand beyond the existing allowlist becomes reachable.

## What this is NOT

- **Not** a relaxation of the safety model — every rail (allowlist, cwd pin,
  scrubbed env, timeout, caps) is reused unchanged.
- **Not** a shell — argv is still whitespace-split / literal; no `;`, `&&`,
  pipes, or redirection.
- **Not** write access — all dedicated tools are read-only verbs on the
  existing allowlist.
