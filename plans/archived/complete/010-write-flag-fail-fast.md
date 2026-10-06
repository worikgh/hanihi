# Plan: fail fast when `--write` cannot open a git repo

## Problem

`crates/hanihi-cli/src/main.rs` silently degrades when `SourceTree::open()`
fails: it prints "source tools disabled (no git repository)" and continues.
When the user passed `--write`, the session then runs with only `get_time`
and `echo`; `write_file` and `apply_patch` are never registered, so the
agent cannot do the job it was asked to do.

Observed in `example_session_01/`: every `llm_prompt` records only
`get_time` and `echo`, and the model reports there is no `write_file`.

## Root cause

All source-tree tools (read and write) are registered inside one block:

```rust
match SourceTree::open() {
    Ok(tree) => {
        // read_file, list_dir, grep, run_command, read_session_log
        // + write tools when args.write
    }
    Err(e) => println!("source tools disabled (no git repository): {e}"),
}
```

The `Err` arm ignores `args.write`. `SourceTree::open()` walks up from the
process cwd, not `--working-dir`, so launching outside a repo triggers it.

## Change

Make a missing repo fatal only when `--write` was requested. Keep the
read-only soft fallback unchanged.

1. Add `SourceError` to the `hanihi_core` imports in
   `crates/hanihi-cli/src/main.rs`.

2. Add a private, testable seam:

```rust
enum SourceTreePolicy {
    Ready(SourceTree),
    Skipped(String),
}

fn source_tree_policy(
    open: Result<SourceTree, SourceError>,
    require_repo: bool,
) -> Result<SourceTreePolicy, AgentError> {
    match open {
        Ok(tree) => Ok(SourceTreePolicy::Ready(tree)),
        Err(e) if require_repo => Err(AgentError::Rig(format!(
            "--write requested but no git repository found: {e} \
             (run from inside the repo)"
        ))),
        Err(e) => Ok(SourceTreePolicy::Skipped(e.to_string())),
    }
}
```

3. Replace the existing `match SourceTree::open()` block in `main` with:

```rust
match source_tree_policy(SourceTree::open(), args.write)? {
    SourceTreePolicy::Ready(tree) => {
        // existing registration block, unchanged;
        // write tools stay gated on args.write
    }
    SourceTreePolicy::Skipped(e) => {
        println!("source tools disabled (no git repository): {e}");
    }
}
```

`hanihi-eval` already behaves this way ("source-tree requested but
unavailable"), so this aligns the CLI with the eval runner.

## Test prompt

Hand the following to the agent in another session, verbatim:

```
Implement fail-fast behaviour in `crates/hanihi-cli/src/main.rs` so
that `--write` without an enclosing git repository is a hard error
instead of a silently degraded session.

Context
`SourceTree::open()` walks up from the process cwd. When it fails, the
current `match` in `main` prints "source tools disabled" and continues
— even when `--write` was passed. The agent then runs with only
`get_time` and `echo`, and `write_file`/`apply_patch` are never
registered. See `example_session_01/` and
`plans/010-write-flag-fail-fast.md`.

Required change
1. In `crates/hanihi-cli/src/main.rs`, add `SourceError` to the
   `hanihi_core` imports and add this private seam:

   ```rust
   enum SourceTreePolicy {
       Ready(SourceTree),
       Skipped(String),
   }

   fn source_tree_policy(
       open: Result<SourceTree, SourceError>,
       require_repo: bool,
   ) -> Result<SourceTreePolicy, AgentError> {
       match open {
           Ok(tree) => Ok(SourceTreePolicy::Ready(tree)),
           Err(e) if require_repo => Err(AgentError::Rig(format!(
               "--write requested but no git repository found: {e} \
                (run from inside the repo)"
           ))),
           Err(e) => Ok(SourceTreePolicy::Skipped(e.to_string())),
       }
   }
   ```

2. Replace the existing `match SourceTree::open() { ... }` block in
   `main` with:

   ```rust
   match source_tree_policy(SourceTree::open(), args.write)? {
       SourceTreePolicy::Ready(tree) => {
           // move the existing registration block here unchanged;
           // write tools stay gated on args.write
       }
       SourceTreePolicy::Skipped(e) => {
           println!("source tools disabled (no git repository): {e}");
       }
   }
   ```

3. Do not change read-only behaviour: without `--write`, a missing repo
   must still print the message and continue.

Tests
Add `#[cfg(test)] mod tests` at the bottom of
`crates/hanihi-cli/src/main.rs`:

- `write_without_repo_is_fatal`: `source_tree_policy` given
  `Err(SourceError::NotARepository(...))` and `require_repo = true`
  returns `Err(AgentError::Rig(msg))` where `msg` contains `--write`.
- `readonly_without_repo_degrades`: same `Err`, `require_repo = false`,
  returns `Ok(SourceTreePolicy::Skipped(_))`.
- `ready_when_repo_opens`: create a temp dir containing a `.git` entry,
  open it with `SourceTree::open_at(&dir)`, and assert
  `source_tree_policy(Ok(tree), false)` returns `Ready`. Use a
  process-unique temp path such as
  `format!("hanihi-cli-tree-{}", std::process::id())`; do not add a
  dependency.

Verification
Run and require green:

- `cargo fmt`
- `cargo test -p hanihi-cli`
- `cargo check --workspace`
- `cargo clippy -p hanihi-cli --all-targets -- -D warnings`

Out of scope
Do not modify `hanihi-core`, session replay, or tool registration beyond
moving the existing block under the `Ready` arm. Keep all new items
private.

Report back: the diff, the test results, and the exact error line that
now appears when `--write` is run outside a repo.
```

## Verification

- `cargo test -p hanihi-cli` — the two error-path tests plus the
  `Ready` test pass.
- `cargo check --workspace` and
  `cargo clippy --workspace --all-targets -- -D warnings` stay clean.
- Manual: `cargo run -p hanihi-cli -- --write --once "hi"` from a
  non-repo directory exits with the new error instead of starting a
  2-tool session.

## Rollback

Low risk. The change is internal to the CLI registration path; read-only
behaviour is preserved and core tool tests are untouched.

## Out of scope

The replay-order corruption documented in `plans/009-bug-fix-b.md` is a
separate defect and is not addressed here.
