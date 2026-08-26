# Plan 007 — Dedicated git write tools: `git_add` + `git_commit`

**Status:** proposed | **Created:** 2026-08-26 | **Scope:** core + CLI + eval + tests

## Problem

Today the agent has **no working git commit path**. The write tools
(`write_file`, `apply_patch`) commit as a side effect of a single tool call,
and `run_command`'s allowlist forbids `git add` / `git commit` entirely
(write verbs are deliberately absent). So the agent cannot:

- stage files it already changed in the working tree, then commit them;
- commit a working tree that git considers "clean" but whose uncommitted
  edits predate this feature;
- commit with a chosen message without re-patching the files.

The immediate trigger is this very session: asked to "commit the uncommitted
code", the agent had no first-class commit tool, mis-routed through
`apply_patch`, and the patch failed because the change already existed in
the tree. A dedicated `git_add` / `git_commit` pair closes that gap with an
explicit, reviewable step.

## Scope & relationship to 006

Plans `006-dedicated-git-tool.md` / `006-dedicated-git-tools.md` (both still
**proposed**, unmerged) cover a **read-only** file/dir of git verbs
(`git_status`, `git_diff`, `git_log`, `git_show`, `git_check_ignore`) and
explicitly exclude write verbs:

> "Write git tools (commit/add) — the allowlist and `--write` gating are
> deliberately unchanged; this plan adds only a read-only verb."

007 deliberately adds the two **write** verbs 006 left out. The two plans do
not conflict — 006 builds the shared fixed-argv executor; 007 builds the
gated write pair on the same patterns. If 006 lands first, 007 should reuse
its `run_argv` helper where convenient; if not, 007 stands alone using the
existing `git_run` helper from `write.rs`.

This plan does **not** implement the read-only set (006's job).

## Design principles (unchanged from 005)

- **Writes gated.** `git_add` / `git_commit` are registered **only** with
  `--write` (CLI) / `write_tools = true` (eval), exactly like `write_file`
  and `apply_patch`. Off by default.
- **Changes are git commits — never pushed.** Both tools only stage and
  commit locally; the existing `scrubbed_env` + pinned `SourceTree::root()`
  environment is reused verbatim.
- **No shell.** argv is trusted entries built by the tool; no `;`, `&&`,
  pipes, or redirection ever reach the OS.

## Tools

### 1. `git_add`

Stage paths in the working tree. Readiness: safe with `--write`; it does not
itself create a commit.

```
{
  "type": "object",
  "properties": {
    "paths": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Repo-relative paths to stage. Empty = stage all (equivalent to `git add -A`)."
    }
  }
}
```

Behaviour:

- Default `paths` empty → `git add -A` (mirrors what `apply_patch`/`write_file`
  already do internally when committing). One empty-array arg lets the agent
  stage everything in one call.
- Non-empty `paths` → each path is validated **before** any git call:
  - resolved with `tree.resolve_for_write` (refuses escapes, `..`, absolute
    paths, symlink escapes);
  - refused if `tree.is_ignored` (git-ignored paths cannot be added);
  - refused if `is_protected` (`.ignore`, `.gitignore`, `.git/*`).
  - Any invalid path → `permission_denied`, nothing staged (all-or-nothing).
- **Never stages protected paths even via `git add -A`:** `git add -A` is
  run in the repo root where `.gitignore` already excludes ignored files,
  and `.ignore`/`.git*` are covered by `.gitignore`/git's own rules — but to
  be safe and explicit, `git add -A` is **refused when the repo has no
  `.gitignore`-covered context we can verify**? No — keep it simple:
  `git add -A` is permitted; protected paths are already excluded by git's
  native ignore handling (`.git/` and `.gitignore` are never staged by git).
  The explicit path list is where the extra checks matter.
- Uses the existing `git_run` helper (reused from `write.rs`) with
  `["add", "--", <paths...>]` or `["add", "-A"]` for the empty case.
- Returns the raw `git add` stdout/stderr trimmed (e.g. the list of staged
  paths after a dry side effect — git prints nothing on success by default).

Reasoning for `git add` as a first-class tool: the agent can inspect the
working tree (`git diff`, `git status`) read-only, then stage *precisely*
the files it wants, then commit — a granular, reviewable flow instead of the
all-or-nothing commit baked into `write_file`/`apply_patch`.

### 2. `git_commit`

Create a commit from whatever is staged.

```
{
  "type": "object",
  "properties": {
    "message": { "type": "string", "description": "Commit message (required)" }
  }
}
```

Behaviour:

- Requires non-empty `message` → `invalid_args` otherwise.
- Runs `git commit -m <message>` via `git_run`.
- **Refuses to run a no-op/short-circuit** — but let git decide: if nothing
  is staged, `git commit` itself says "nothing to commit" and exits 1, which
  we surface as a normal tool result (exit code 1 + stderr), **not** a tool
  error — the agent needs to see "nothing staged" to know to add first.
- Returns the git output (the commit summary line, e.g. `[main abc1234]`).
- Guards: `git_commit` **never** runs `git add` implicitly — staging is
  `git_add`'s job. This keeps add/commit orthogonal and reviewable and
  avoids the surprise all-or-nothing commit of the single-call tools.

### 3. Additional tools — recommendation

The minimal pair above lets the agent commit uncommitted code. Two more
**read-only** verbs make the flow observable and are cheap (they only read):

- `git_log` (`git log --oneline -20`) — verify what was committed and the
  resulting history. Already proposed in 006; if 006 lands, this comes free.
- `git_diff` (`git diff` + `git diff --cached`) — see unstaged **and staged**
  changes before committing. The `--cached` variant is essential to review
  what `git_add` staged.

But **006 already covers `git_log`/`git_diff`/`git_status` as read-only
tools.** This plan should not duplicate them. Recommendation for 007:

- **In scope:** `git_add`, `git_commit` (this plan).
- **Deferred to 006:** the read-only set (`git_status`, `git_diff`,
  `git_log`, `git_show`, `git_check_ignore`).
- If `git_diff --cached` is wanted before committing without waiting for
  006, add a single `git_staged` tool (`git diff --cached`) — but I lean
  against it; 006 is the right home and the agent already has
  `run_command`-reachable `git diff --cached`? **No** — the allowlist does
  not permit `diff --cached`. See Notes below.

**Not in scope for 007:** `git reset`, `git checkout`, `git revert`,
`git push`, `git branch`, `git merge`, `git stash`. These mutate history or
the index in ways that erode the "every change is a reviewable commit, never
push" model. `git reset`/`git checkout` would let the agent discard work —
deliberately excluded. Push stays impossible by design.

So the answer to "what other tools should I add": **just `git_add` and
`git_commit` in this plan; the read-only set belongs to 006, and every other
write verb is deliberately out of scope.**

## Allowlist note

`run_command`'s allowlist keeps rejecting `git add` / `git commit` — that is
intentional and unchanged. The dedicated tools do **not** go through
`check_command_argv`; they use `git_run` (write.rs), which is the same
pinned-cwd + scrubbed-env path the write tools already use. We are **not**
broadening what `run_command` can do; we are adding better-named, fixed-verb
tools that shell the same two verbs `write_file`/`apply_patch` already shell
internally.

One consequence: after this change the agent can stage and commit **arbitrary
already-present working-tree changes** (including files written outside the
harness). That is the intended capability — "commit the uncommitted code" —
and is still gated behind `--write`, still commit-only, never pushed.

## Tests

Reuse the existing `Fixture` (write.rs) and add a real-repo helper or reuse
the `git_repo()` pattern from `tool.rs` (a temp repo with an identity and an
initial commit). Existing `git_run` makes git-in-test trivial.

### `git_add`

1. **Stages explicit paths** — make an unstaged edit to `src/main.rs`, run
   `git_add` with `paths: ["src/main.rs"]`, assert `git status --porcelain`
   shows it staged (`M ` prefix, index column) and unstaged nothing.
2. **Empty paths stages all** — edit two files + add an untracked file, run
   `git_add` with `paths: []`, assert `git status --porcelain` shows all
   staged (including `A ` for the new file).
3. **Refuses ignored path** — `git_add` with `paths: ["target/debug/junk.rs"]`
   → `permission_denied` ("git-ignored").
4. **Refuses escape** — `paths: ["../outside.rs"]` → `permission_denied`
   ("escapes").
5. **Refuses protected path** — `paths: [".ignore"]` → `permission_denied`
   ("protected").
6. **All-or-nothing** — with one good and one bad path, nothing is staged.

### `git_commit`

7. **Commits staged changes** — stage an edit, commit with a message, assert
   exit 0, the commit exists (`git log --oneline` shows the message), and the
   tree is clean.
8. **Requires message** — empty/missing `message` → `invalid_args`.
9. **Nothing staged is not an error** — fresh repo, commit with no staged
   changes → tool result with exit code 1 + "nothing to commit", **not** a
   tool error; assert the tool returns `Ok` with a non-zero exit surfaced.
10. **Does not implicitly stage** — unstaged edit, commit → "nothing to
    commit" (proves commit is orthogonal to add).

### Pair end-to-end

11. **Uncommitted-code commit flow** — pre-existing working-tree edit (no
    staging by harness), `git_add` (empty) then `git_commit`, assert the
    commit lands and the tree is clean. This is the exact scenario that
    failed in this session.

## Registration

- `crates/hanihi-core/src/write.rs` — add `builtin_git_add(tree)` and
  `builtin_git_commit(tree)` (same signature shape as `builtin_write_file`,
  minus `traces_dir` — these are write tools, no trace file needed; they
  report via `git_run`'s captured output).
- `crates/hanihi-core/src/lib.rs` — re-export both.
- `crates/hanihi-cli/src/main.rs` — register both under `if args.write { ... }`
  alongside `apply_patch`/`write_file`.
- `crates/hanihi-eval/src/main.rs` — register both under
  `if case.write_tools { ... }` alongside the existing write tools.

## Eval cases

Add one case demonstrating the write pair:

- `005-git-commit`: `repo` + `write_tools`, prompt "stage and commit the
  change in src/main.rs with message 'fix'". Assertions:
  - `tool_called("git_add")`, `tool_called("git_commit")`;
  - `build_succeeds` (repo still builds);
  - `no_error`.

## Acceptance criteria

- `cargo test --workspace` passes (all existing + new tests).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- With `--write`, the agent can stage and commit uncommitted working-tree
  changes by name (`git_add`, `git_commit`) — no `unknown tool`, no
  mis-routing through `apply_patch`.
- Without `--write`, neither tool is registered (callable or hallucinable).
- No other git verb (write or mutating) becomes reachable: `run_command`
  allowlist unchanged; `reset`/`push`/`checkout`/etc. remain impossible.
- Nothing is ever pushed.

## Out of scope

- Read-only dedicated tools (`git_status`, `git_diff`, `git_log`,
  `git_show`, `git_check_ignore`) — 006's job.
- `git_diff --cached` / staged review — defer to 006 (add `--cached` to its
  `git_diff` spec); the agent can still see staged state via `git_status`
  after `git_add`.
- All history-mutating / force / network verbs (`reset`, `checkout`,
  `revert`, `stash`, `branch`, `merge`, `push`, `pull`, `fetch`).
- Multi-file `apply_patch`-style plan application — unchanged.

## Safety

| Rail                                                  | Where                                                             |
|-------------------------------------------------------|-------------------------------------------------------------------|
| Writes only with `--write` / `write_tools`            | CLI + eval registration                                           |
| No shell; trusted argv                                | both tools via `git_run`                                          |
| cwd pinned to repo root                               | `git_run` (`SourceTree::root()`)                                  |
| Env scrubbed (PATH/HOME/CARGO_*), no keys             | `git_run` uses `scrubbed_env`                                     |
| Escape / ignore / protected refusal on explicit paths | `git_add` via `resolve_for_write` + `is_ignored` + `is_protected` |
| Commit-only, never push                               | `git_commit` is `git commit` only                                 |
| Add/commit orthogonal                                 | `git_commit` never implicitly stages                              |
