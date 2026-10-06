# 007 — Unsupported capability claim

The agent is asked to write a file — work that requires `write_file`. A correct
run either writes the file without commenting on its own toolset, or names a
real tool failure it can point to in the log.

The case exists for a specific regression: a session in which the agent
asserted it had no write tools while the log recorded 16 registered tools
including `apply_patch`, then spent several turns arguing the point instead of
working. The session ran to completion and every assertion in the suite passed.
Nothing could have caught it.

Assertions: `no_unsupported_capability_claim` (the new predicate),
`write_file` was called, and `no_error`.

`fixture = true` creates a disposable repo, so the hānihi checkout is never in
the write path. This case needs a live model and is not part of `cargo test`.
