# git_apply tool failures

The write tool that applies unified diffs is `apply_patch`, which routes
through `git apply --3way --recount`. These are the failures observed with
it in this session (recorded as `error` events in the session's
`events.jsonl`, and repeated here for the record).

## Failures observed (in session log)

1. Turn 1 — `error: no such path: crates/hanihi-cli/src`
   Path resolution error surfaced while reading/listing; not itself a git
   apply error but the first write-path failure of the session.

2. Turn 6 — `git apply failed: error: repository lacks the necessary blob
   to perform 3-way merge. Falling back to direct application... error:
   patch failed: crates/hanihi-cli/src/main.rs:652 error:
   crates/hanihi-cli/src/main.rs: patch does not apply`
   The `--3way` merge could not run (repo missing required blobs for the
   index/HEAD states), and the direct fallback also failed to apply the
   hunk at main.rs:652.

3. Turn 7 — `git apply failed: error: No valid patches in input (allow
   with "--allow-empty")`

4. Turn 8 — `git apply failed: error: No valid patches in input (allow
   with "--allow-empty")`
   The supplied unified diff was rejected as empty / not containing valid
   patches.

## Decision: do not use the git apply-based tool

The git-apply-based write path is unreliable here: both the 3-way merge
(blob availability) and the direct-application fallback fail in this
environment. Therefore:

- Do not use `apply_patch` (the git-apply-based write tool).
- Use `write_file` (direct file write; no git apply/3way machinery) instead.

## Consequence

This session's source changes, including the `/session` turn-footer feature
(emit `[turn N | tool calls: ... | tokens: ... in / ... out | max_turns: ...]`
from `/session`), are made with `write_file`, not with the git-apply tool.
