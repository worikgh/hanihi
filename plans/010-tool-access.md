You are Hānihi working on this Rust repository. You are run with
`--write`, so you may edit tracked files and make local git commits —
never push.

## Objective

Grant the git "housekeeping" verbs that `run_command` currently denies,
but **only** when write mode is active (`--write` in `hanihi-cli`, or an
eval case with `write_tools = true`). With `--write` off, `run_command`
must behave byte-for-byte as it does today: read-only cargo/git only.

## Why

A completed change was committed but swept ~1100 unrelated files into the
repo tree (commit `f4720df` on this branch). The two doc edits in it were
correct; the tree around them was not. Repairing it takes two steps, and
neither moves the branch:

1. `git restore --staged <paths>` — drop everything but the two docs from
   the index.
2. `git commit --amend` — rewrite `HEAD` in place with just those files.

Neither is reachable now: `check_command_argv` only admits read-only git
verbs, and the write tools refuse `.git*`. `git reset` is deliberately not
in scope — it only moves the branch pointer and does no unstaging, so it is
never needed for this repair.

## Read these first, verbatim

- `crates/hanihi-core/src/tool.rs`:
  - `check_command_argv` (~line 275), the gate you extend.
  - `builtin_run_command` (~line 684), registered unconditionally.
  - tests `command_allowlist_*` and the tool smoke tests asserting
    `git push` is denied.
- `crates/hanihi-core/src/write.rs` — how write tools refuse `.git*` and
  internally call `git add` / `git commit`.
- `crates/hanihi-cli/src/main.rs` — `args.write` (declared ~89), write
  registration behind that flag (~368).
- `crates/hanihi-eval/src/main.rs` — `case.write_tools` gating
  `builtin_run_command` registration (~688) alongside `apply_patch` /
  `write_file` (~690).

## Verbs to grant in write mode only

| allowed (write mode) | still denied, even in write mode |
|---|---|
| `git add` / `add -A` (already implicitly used by write tools) | `push` |
| `git restore --staged` | `git reset` |
| `git rm --cached` | `rebase`, `filter-branch`, `filter-repo` |
| `git commit` / `git commit --amend` | `merge`, `cherry-pick`, `revert` |

Bound `git commit --amend`: reject flag shapes that could target a
non-`HEAD` commit or manufacture its identity/time. Whitelist the exact argv
forms you permit and encode them as a named constant, matching the house
rule against magic strings.

## Work order, test-first

Add failing unit tests in `tool.rs` `mod tests` first, then implement:

1. Without write intent, all of the `allowed` column above are denied —
   proves the read-only default is unchanged.
2. With write intent, the `allowed` column is accepted and the `denied`
   column is refused.
3. `git commit --amend` in the bounded local shapes only; reject
   `-c/--fixup` to another target and explicit `--author=` / `--date=` /
   `--committer-date-is-author-date`.
4. The cwd-escape guard (`git -C`, `--directory`, cargo `--manifest-path`)
   still rejects the new verbs when they try it.

Then:
- Choose a mechanism: either pass an explicit mode through to
  `check_command_argv` (prefer a named enum over a bare bool, per house
  rule), or add a write-mode variant of the tool factory selected by the
  caller. Pick whichever keeps the read-only path simplest and unchanged,
  and state the choice.
- Wire it so `hanihi-cli` selects the write-mode allowlist on `args.write`
  and `hanihi-eval` does so on `case.write_tools` (at the registration
  point, mirroring `apply_patch` / `write_file`).
- Update only the write-mode variant's description text to advertise the
  new verbs; leave the read-only default description untouched.
- Do not touch unrelated code. Keep the edit count minimal.

## Verification gates

- `cargo fmt`
- `cargo test --workspace`  (new tests red first, then green)
- `cargo clippy --workspace --all-targets -- -D warnings`

In your final message report: which verbs each mode permits and denies, the
exact whitelisted `git commit --amend` argv shapes, and confirmation that
the read-only path behaviour is unchanged (its tests still pass).

Do not apply any history operation to the working tree while implementing.

---

This change exists so an agent that already has `--write` can repair an
over-broad commit it created: staged-only then amend, no branch rewind.
