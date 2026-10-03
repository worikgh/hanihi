# Plan 026 — Deterministic apply_patch preparation

You are Hānihi. This plan is a prompt to a future session. Its job is to
make `apply_patch` safe to use **without the agent remembering any file
state**, by moving the preparation of an edit out of prose and into code.

Read `reports/002_session_failures.md` before starting; this plan is the
implementation half of that report.

## Context

In the previous session, the allowlist feature landed, but the editing
process almost did not. `apply_patch` failed five times out of seven.
Every failure had the same shape: the caller (the agent) prepared the edit
wrong, and the tool refused — correctly, but in a way that did not tell the
caller how to recover.

The specific preparation failures:

1. The caller handed over a `base_token` it had not read this turn, or had
   read two edits ago, or had typed from memory of a `sha256sum`.
2. The caller composed hunks whose context lines it had never read, using
   `grep` line *numbers* instead of file *text*.
3. `read_file` silently truncated a 70 KB file at 64 KiB and dropped the
   `token` field, so the caller could neither see the file nor obtain a
   valid version handle.
4. Refusals came back as prose. The caller then invented rules about what
   it was and was not allowed to do, because nothing in the reply said what
   had failed, why, and what would succeed.

The goal is to make these **structurally impossible**. Not "be more
careful". Not "always re-read". Code.

## Principle

The agent must never be required to hold file state in its context across
turns. Anything an edit needs — the current version, the lines being
changed, the result of a would-be write — must be obtainable as data, from
a tool result, in the same turn it is used.

Everything below is a Rust data structure or a machine-readable field, not
a prompt instruction. Where a choice is offered, prefer the more
deterministic option and say why.

## Read first, verbatim

- `reports/002_session_failures.md` — the specification for this work.
- `reports/001_apply_patch.md` — prior art: the previous rewrite of
  `apply_patch` away from `git apply` for the same reason.
- `crates/hanihi-mcp-server/src/apply_patch_tool.rs` — the tool to change.
  Note the current types: `Edit { path, base_token: String, op }`,
  `EditOp::{Patch, Replace}`, `NEW_TOKEN_FIELD`, `ToolError { code,
  message }`, `apply_edits_in`, `parse_patch`, `HunkHeader::{Ranges,
  Derive}`, `apply_hunks`, `resolve_within_root`.
- `crates/hanihi-mcp-server/src/workspace_fs.rs` — `sha256_hex`,
  `normalize_hash`, `workspace_root`.
- `crates/hanihi-core/src/tool.rs` — `MAX_TOOL_RESULT_BYTES` and
  `truncate_tool_output`, applied to every rendered result.
- `crates/hanihi-core/src/agent.rs` near the read-only deduplication — the
  message that reads like a policy refusal.

## Deliverables

### 1. Structured refusal payload

Today a refusal is `ToolError { code: i64, message: String }`, and the
caller must parse prose to recover. Replace it with data.

```rust
struct ToolError {
    code: i64,
    message: String,
    /// Present when the failure names a file the caller can act on.
    file: Option<String>,
    /// Present on a token mismatch: the version the tree actually has.
    /// This is the field the caller copies into its next `read_file` or
    /// `apply_patch` retry.
    actual: Option<FileVersion>,
    /// One imperative sentence stating what would make the call succeed.
    recovery: Option<String>,
}
```

The mismatch path must set:

```json
{
  "code": -32603,
  "message": "base_token mismatch for src/tool.rs",
  "file": "src/tool.rs",
  "actual": { "digest": "0ccfa7...", "len": 74281 },
  "recovery": "pass this actual value as base_token, or use base_token auto"
}
```

The agent then has a mechanical retry: copy `actual` back. No prose to
misread.

Do not weaken the existing refusals. Placeholder tokens, the empty-content
hash on a non-empty file, and a genuine mismatch must still be refused, with
their current distinct messages preserved where tests assert them.

### 2. `base_token: "auto"` — chained edits without a re-read

The single highest-leverage change. The session's recurring failure was two
edits in a row: the second used the token from before the first.

Add a session-scoped ledger:

```rust
/// Versions the harness has observed, keyed by path.
struct VersionLedger {
    /// Path -> most recent observed (digest, len), plus an opaque id.
    versions: Mutex<HashMap<PathBuf, LedgerEntry>>,
}

struct LedgerEntry {
    /// Opaque, session-unique. The agent copies it; it does not appear as a
    /// hash the caller can fabricate.
    id: u64,
    digest: [u8; 32],
    len: u64,
}
```

Both `read_file` (complete reads only — see deliverable 4) and a successful
`apply_patch` write record into the ledger. Then `apply_patch` accepts:

```json
{ "file": "src/tool.rs", "patch": "...", "base_token": "auto" }
```

`auto` resolves to the ledger's current entry for `file`. If there is no
entry, refuse with `recovery: "read the file first"`.

Regression test, named after the failure it prevents:
`apply_patch_chains_two_edits_with_auto` — write once, then write again
using `auto`, assert both landed and the file has the second result.

### 3. An opaque version handle, minted only by the harness

`base_token` as a bare 64-hex string is the fabricated-hash vector: the
caller can type any hex it likes, and the tool cannot tell "read this turn"
from "typed from memory". Make the recommended form an opaque handle.

- `read_file` and `apply_patch` results return `version: { "id": 7,
  "digest": "...", "len": 74281 }`.
- `apply_patch` accepts `"base_token": { "id": 7 }` or `"base_token":
  "auto"` (the string form `base_token: "<64-hex>"` stays supported for
  the existing tests, but is documented as the legacy form).
- A handle is bound to its path: using the handle minted for `a.txt` on
  `b.txt` must refuse with a message saying the handle does not belong to
  that file.

The determinism comes from the binding, not from secrecy. The agent copies
the handle; it cannot construct a *bound* one, and it cannot substitute a
hash it only half-remembers.

Regression test: `opaque_handle_is_bound_to_its_path`.

### 4. `read_file` always returns a version, truncated or not

The previous session could not read a 70 KB file, and the truncation hid
the token. Fix both.

```rust
struct ReadResult {
    path: String,
    content: String,
    /// Always present, even when `content` is truncated. The digest is of
    /// the *whole* file, so it is a valid `base_token` regardless.
    version: FileVersion,
    /// `Some(total_len)` when `content` is a prefix of a larger file.
    truncated_at: Option<u64>,
}
```

- Add `offset` and `limit` arguments so a large file is readable in ranges
  without depending on the truncation path.
- Decide and document the interaction between `MAX_READ_BYTES` (per-file)
  and `MAX_TOOL_RESULT_BYTES` (agent-layer). The invariant to enforce and
  test: **a truncated read still yields a correct, usable token**; the only
  thing truncation may cost is body text.

Regression test: `truncated_read_still_returns_a_usable_token` — read a file
larger than the backstop, assert `version.digest` equals the full-file hash
and `truncated_at == Some(len)`.

### 5. `dry_run`: observe the result before writing

Make "get ready to apply a patch" an explicit, observable phase. Add:

```json
{ "file": "src/tool.rs", "patch": "...", "base_token": "auto",
  "dry_run": true }
```

A dry run performs **every** check — path resolution, version verification,
patch parsing, hunk matching, diff computation — and returns the would-be
result without writing:

```json
{
  "would_change": true,
  "files": [
    {
      "path": "src/tool.rs",
      "version": { "digest": "...", "len": 74291 },
      "diff": "--- a/...\n+++ b/...\n@@ ..."
    }
  ]
}
```

The agent can call it before the real write; a failure at this stage names
exactly which input to fix, deterministically, with no side effects.

Regression test: `dry_run_writes_nothing_but_reports_the_would_be_version`.

### 6. Self-describing refusals, everywhere

Audit every error string in `apply_patch_tool.rs`, `read_file_tool.rs`, and
the read-only deduplication path. Each must state:

- what was refused;
- why;
- what would succeed.

Specifically, the duplicate-read message currently reads as a policy rule
and was misread as one twice. It must say:

```
duplicate read of src/tool.rs this turn: reuse the earlier result, or vary
the arguments (for example request a different offset/limit range)
```

### 7. Commit the recommendation

The previous session declined to commit because it could not pass a
multi-line message through argv. That is a real gap, but the fix is out of
scope here; plan 027 owns a staged-text channel. This plan only requires
that the gap be documented in the report it updates, not solved.

## Constraints

- No new dependencies unless justified in the commit message.
- Keep the legacy `base_token: "<64-hex>"` path working. Existing tests in
  `apply_patch_tool.rs` are the specification and must pass unchanged. If
  one must change, name it in the commit message and say why.
- `run_command` stays out of the read-only cache set.
- Do not weaken refusal semantics. The token mechanism caught five
  would-be corruptions in the previous session; its strictness is the
  feature. The change is to make the *recovery* mechanical, not the checks
  lenient.
- One commit per deliverable, or one structured commit message.

## Gates

```text
cargo fmt --check
cargo test -p hanihi-core
cargo test -p hanihi-mcp-server
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Working method, learned the hard way

These are constraints on *you*, not on the user:

- Use the `grep` tool before asking a human for file content.
- One dependent tool call per turn. A read followed by a write in the same
  turn is how stale tokens happen; if a write depends on a read, put them in
  different turns.
- Never state a constraint you have not seen stated. When a tool refuses,
  quote the refusal verbatim and nothing more.
- Do not stop at the first blocked path; route around it.
- Do not hold file state in prose. It belongs in tool output or in the
  ledger you are building.

## Success criteria

A future session can make five consecutive dependent edits to a 70 KB file
using `base_token: "auto"` and the returned handles, dry-running before the
last one, without a single `base_token mismatch`, without asking a human for
a hash or a `grep`, and without reading the same large file twice by hand.
