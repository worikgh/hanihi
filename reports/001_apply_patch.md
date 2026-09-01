# Report: `apply_patch` failures — what was learned                                                                                 11:38:34 [66/1925]

## 1. The tool in question

`apply_patch` is the agent's unified-diff write tool. In this repository it was implemented in `crates/hanihi-core/src/write.rs`, originally by shelling out to:
																																					  ```
git apply --3way --recount
```

## 2. Failure modes observed

### Failure A — `--3way` blob missing (Turn 6)
`git apply --3way` requires the blob named by the patch's 
`index <old>..<new>` header to exist in the object database so it can do a
3-way merge against the index/HEAD state.

- Observed error: `repository lacks the necessary blob to perform  3-way merge`.
- Fallback to direct application then failed with `patch does not apply`.
- Root cause: hand-composed patches (and patches produced against a dirty tree) carry **synthetic or stale `index` hashes**. Those blobs are not in 
`.git/objects`, so `--3way` cannot even start. `--3way` also refused with `src/main.rs: does not match index` when the worktree content no longer 
matched the named index blob.

### Failure B — direct application refuses hunks without trailing context (Turns 7–8, and the dirty-tree test)
Even *without* `--3way`, plain `git apply` (with or without `--recount`) fails on a very common hand-written hunk shape:

```
@@ -1 +1,2 @@
 fn main() {}
+// patched
```

against a file that is:

```
fn main() {}
// local
```

- Observed error: `error: patch failed: src/main.rs:1` / `patch does not apply`.
- `git apply` searches for the single context line `fn main() {}` and, because the insertion point has **no trailing context**, it treats the location
 as ambiguous/unsafe when the file has since changed. The local uncommitted edit (`// local`) is what makes it refuse — exactly the dirty-tree case 
 that `--3way` was supposed to solve.
- Reproduction confirmed by hand with `git apply --recount --check --verbose`: it searches only for `fn main() {}` and rejects the patch, while the 
identical hunk with the local line included as trailing context applies cleanly.

### Failure C — "No valid patches in input" (Turns 7–8)
- Observed when the supplied diff had no `---`/`+++` file markers at all, or was otherwise empty of parseable patches.
- This is input malformation, not a git/object-store problem — but the old tool surfaced only git's terse message.

## 3. The reliable fix that was implemented

`git apply` was abandoned entirely. `write.rs` now contains a **pure-Rust unified-diff applier**:

1. Parse the diff into per-file patches (markers, hunk headers, `+`/`-`/space body lines).
2. Resolve each target path through `SourceTree` (escape/ignore/protected-path checks).
3. For each hunk, match the old side (`context` + `-` lines) against the **current working-tree contents** and substitute the new side (`context` + `+` lines).
4. Apply atomically: all hunks are matched against in-memory copies first; files are written only if every hunk applies.
5. `--recount` semantics are reproduced by ignoring the `@@` counts and computing line counts from the body.

Because a **single leading context line** now anchors the insertion, the dirty-tree case succeeds: `fn main() {}` is found at position 0 and 
`// patched` is inserted immediately after it, preserving `// local`.

`git_run` remains only for the legitimate follow-on `git add -A` / `git commit` step.

## 4. Verification status

- `cargo test -p hanihi-core --lib`: **60 passed, 0 failed**, including the previously failing `write::tests::apply_patch_works_on_dirty_tree`, plus 
`apply_patch_applies_and_commits`, `apply_patch_adds_new_file_on_clean_tree`, and the protected-path/bad-diff rejection tests.
- `cargo check --workspace`: passes. Only pre-existing, unrelated warnings remain (unused assignments in `hanihi-cli/src/main.rs`).
- The pure-Rust rewrite is committed as `30a6952` (`Replace-git-apply-with-pure-Rust-patch-applier`).

## 5. Remaining loose ends in the working tree

Not yet cleaned up (left for your instructions):

- Modified but uncommitted:
  - `crates/hanihi-cli/src/main.rs` (debug `eprintln!`/`dbg_report` experiment and unused tool-event variables — not part of the apply_patch work)
  - `crates/hanihi-core/src/source.rs` (`#[derive(Debug)]` on `Fixture`, and a tab-indentation-only change in the C template)
  - `crates/hanihi-core/src/tool.rs` (whitespace/formatting only; substantive allowlist changes were already committed as `7eb185a`)
- Untracked scratch/notes: `crates/hanihi-core/applytest.patch`, `crates/hanihi-core/src/tool.patch`, `crates/hanihi-core/src/write.diff`, `crates/han
ihi-core/src/write.patch`, `crates/hanihi-cli/src/#main.rs#`, `prompts/self-improvement.md`, and the `working/` tree.

## 6. Bottom line

`apply_patch` was unreliable because it depended on `git apply`'s object-database-backed `--3way` merge and on git's strict context matching. Both are
 unsuitable for hand-composed patches against a dirty working tree. The committed replacement applies hunks directly against the current working-tree
text, matching how the agent actually reads files, and the full `hanihi-core` test suite now passes.

Waiting for further instructions.
[turn 1 | tool calls: 0 | tokens: 255087 in / 1371 out | max_turns: 1000]
