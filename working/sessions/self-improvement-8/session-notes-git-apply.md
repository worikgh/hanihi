# Session log: git_apply failures and decision

The git-apply-based write tool (here named `apply_patch`, which applies
unified diffs via `git apply --3way --recount`) failed repeatedly in this
session and was rejected in favour of `write_file`.

## Failures observed (from this session's events.jsonl)

1. Turn 1 — `error: no such path: crates/hanihi-cli/src`
   Path-resolution failure surfaced while reading/listing the tree. First
   write-path problem of the session.

2. Turn 6 — 
   `git apply failed: error: repository lacks the necessary blob to perform
   3-way merge. Falling back to direct application... error: patch failed:
   crates/hanihi-cli/src/main.rs:652 error: crates/hanihi-cli/src/main.rs:
   patch does not apply`
   `--3way` could not run (repo lacks required blobs for index/HEAD), and
   the direct fallback failed to apply the hunk at main.rs:652.

3. Turn 7 — `git apply failed: error: No valid patches in input (allow with
   "--allow-empty")`

4. Turn 8 — `git apply failed: error: No valid patches in input (allow with
   "--allow-empty")`
   The unified diff was rejected as empty / not containing valid patches.

## Decision not to use git_apply

The git-apply-based write path is unreliable in this environment: the 3-way
merge needs blobs this repo does not have, and the direct fallback fails to
apply the working-tree hunks. Both error modes break patch application.

Decision: do not use the git-apply-based tool. Use `write_file` (direct
file write; no git apply / 3-way machinery) for all source changes.

## Consequence

All changes this session — including the `/session` turn-footer feature
(emit `[turn N | tool calls: ... | tokens: ... in / ... out | max_turns: ...]`
from `/session`) — were made with `write_file`, not with the git-apply tool.
