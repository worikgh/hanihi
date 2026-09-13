# Tools required to finish the committed-but-polluted change

Commit `f4720df` swept ~1100 unrelated runtime/log/scratch files into the
repo along with the two intended doc edits (`README.md`, listed the four
repo-backed eval assertions; `prompts/code_generation.md`, rebuilt the
apply_patch guidance line). The content change is correct and committed;
the commit is not clean and mislabels the real diff.

## Correction: `git reset` is NOT required

`git reset` only moves the branch pointer; it changes no working-tree or
index file contents. The junk must be *unstaged*, not un-committed, so the
correct path is `git restore --staged` plus `git commit --amend` — never a
`git reset`.

## Tools needed

1. `git restore --staged <path>...` — unstage every file except the two
   docs, so they drop out of the next commit while staying on disk.
2. `git commit --amend` — rewrite HEAD in place to contain only `README.md`
   and `prompts/code_generation.md`, with an accurate message.

Optional, only if the stray artifacts should also be physically removed
(debug logs, `example_session_01/`, `working/applytest`, `working/notes`,
scratch dirs, trace trees): `git restore` / `git clean`.

## Why current tools are insufficient

- `run_command` allowlist is limited to `status, diff, log, show, apply
  --check` — no restore, commit, amend, add, or clean.
- `apply_patch` / `write_file` refuse paths under `.git/`, so the index,
  HEAD, and refs are untouchable.

## Minimal allowlist addition to finish

Add `git restore --staged` and `git commit` (amend) to `run_command`, then:

```
git restore --staged ./working ./example_session_01 ./gitignore.test \
    ./hanihi-debug.log ./crates/hanihi-core/hanihi-debug.log  # and the rest
git commit --amend     # → clean two-file commit
```

No further working-tree mods were made beyond this note.
