# Plan 027 — Tool error diagnosability

You are Hānihi. This plan is a prompt to a future session. Its job is to make
two tool failures *self-explaining*, so a caller (the agent) that prepared a
call wrongly can fix the call from the error text alone.

This plan is self-contained. Implement it even if no other numbered plan has
landed. Do not modify anything in anticipation of other plans.

## Objective

Two observed failures, both of the same kind: the tool did the right thing and
refused or mis-served the call correctly, but the reply did not say *why*, so
the caller could not recover without a human.

1. `read_file`'s `offset`/`limit` are **byte** offsets. A caller that assumes
   line numbers gets a plausible-looking slice of the wrong region and no
   error at all.
2. `apply_patch`'s hunk-mismatch refusal names the hunk but not the reason. A
   context line whose leading whitespace differs (a tab typed as spaces)
   produces a refusal indistinguishable from "the file changed underneath
   you".

Neither is a correctness bug. Both are diagnosability defects, and the cost
lands on every future caller.

## Read first, verbatim

- `crates/hanihi-mcp-server/src/read_file_tool.rs`
  - the `READ_FILE_DESCRIPTION` constant (~29) and the `offset`/`limit` schema
    descriptions (~37-41)
  - `read_file_with` (~97-158) — note `floor_char_boundary` at 133 and the
    version-ledger logic at 142
  - `offset_and_limit_read_a_range` (~284) — the test that pins current
    behaviour
- `crates/hanihi-mcp-server/src/apply_patch_tool.rs`
  - `parse_patch` (~706)
  - `apply_hunks` / the anchor loop (~861-888) and the mismatch message (~871)
  - `find_hunk_offset` (~894-913) — **it already computes a nearest candidate
    and discards it**
  - `hunk_matches` (~916), `apply_one_hunk` (~932)
  - `mismatch_returns_structured_refusal_payload` (~1856)
- `crates/hanihi-mcp-server/src/workspace_fs.rs` — `ToolError`, `failure`,
  `ToolResult`, and how a refusal is serialized
- `plans/026-deterministic-apply-patch.md` — deliverable 1 there defines the
  structured refusal payload this plan extends. If 026 has landed, reuse it;
  if not, implement only what this plan needs and say so.
- `reports/002_session_failures.md` — prior art on refusal legibility.

## Findings (confirmed by reading the code)

1. `read_file_tool.rs:29` already says "(byte offsets)" and `:39` already says
   "Byte offset at which to start the returned content." The implementation is
   correct and self-consistent. The defect is that a caller skimming the
   schema reads "offset" as lines; the documentation is right and
   insufficiently loud.
2. An out-of-range or wrong-unit offset **does not error**. `:133` clamps with
   `.min(content.len())` and floors to a character boundary. A wrong offset
   silently yields a wrong-but-plausible slice.
3. `apply_patch_tool.rs:871` is the entire mismatch message. It interpolates
   the hunk ranges and nothing else.
4. `find_hunk_offset` (`:894-913`) already tracks `best` as
   `(distance, offset)` and returns only the offset; the distance is computed
   and thrown away. The nearest-candidate information already exists at the
   failure site.
5. The mismatch path does **not** currently distinguish "context lines differ"
   from "no line of the file matches this hunk at all". Both produce `:871`.

## Design

### A. Make the units unmissable (item 1, documentation)

Change the `offset` and `limit` schema descriptions to name the unit and deny
the wrong one explicitly. Target wording:

```json
"offset": {
  "type": "integer",
  "description": "Byte offset (not a line number) at which to start the returned content. Defaults to 0."
},
"limit": {
  "type": "integer",
  "description": "Maximum number of bytes to return (not lines). Defaults to the per-file read cap."
}
```

Mirror the unit in `READ_FILE_DESCRIPTION` (~29) rather than adding a second
copy of the schema text. Keep the existing sentence structure; this is a
wording change, not a rewrite.

Do **not** change `offset`/`limit` semantics. Existing callers that use bytes
must keep working, and `offset_and_limit_read_a_range` pins that.

### B. Add a `line` parameter (item 2, API)

The honest fix. `offset` stays byte-based; add an alternative selector.

- `line` (1-based, integer). Mutually exclusive with `offset`.
- Supplying both must refuse with `code: -32602` and a message naming both
  fields and stating that only one may be given.
- `line: 0` must refuse as invalid (lines are 1-based).
- A `line` past the last line must refuse `-32602` naming the file's line
  count, rather than clamping. This is the one place where clamping is the
  wrong call: the caller asked for something that does not exist.
- Resolution: scan `content` for the `line`-th newline and derive the byte
  offset. Then reuse the existing byte path unchanged, so `limit`, truncation
  accounting, and the version ledger behave identically.
- The response report must echo the **resolved byte** `offset` (as it does
  today at `:158`) and additionally echo the requested `line`. A caller that
  used `line` needs to see what it resolved to.

Add a named constant for the 1-based convention rather than a bare `1` inline.

### C. Report the first differing line on a hunk mismatch (item 3)

This is the highest-value change: it turns an invisible whitespace difference
into a visible one.

When a hunk fails to match, walk its context and removed lines against the
file starting at the expected anchor and find the first position where they
differ. Extend the refusal with:

- `file` — the path, matching the field name used by the refusal payload.
- `line` — the 1-based file line of the first mismatch, when one was found
  against the anchor.
- `expected` — the hunk's line, rendered **escaped** (`\t`, `\n`, trailing
  spaces made visible).
- `actual` — the file's line at that position, rendered the same way.
- `recovery` — one imperative sentence, e.g. "the context line differs at
  line 71: the file has a tab where the patch has spaces; re-read the file and
  rebuild the hunk from its actual text".

Escape rendering is the point. An unescaped dump of `\t` versus four spaces
looks identical in most terminals; `{:?}`-style escaping does not. Use the
same escaping on both sides so they are directly comparable.

Keep the existing `message` prose and its wording, since
`mismatch_returns_structured_refusal_payload` asserts the current shape.
Extend, do not replace.

### D. Name the nearest candidate when the anchor is wrong (item 4)

`find_hunk_offset` already computes `best`. Return it, and use it.

- Change the return type to carry the winning offset and its distance from the
  anchor (e.g. `Option<(usize, usize)>`), or add a sibling function — decide
  when you see the call sites and say which you chose.
- When no hunk matches at all, keep the current behaviour.
- When a hunk matches but *not* at the anchored position, refuse with the
  current message plus a `recovery` naming the drift: "the hunk matches 3
  lines below the anchor; re-read the file and re-anchor the hunk".
- Threshold the drift report: only mention it when the distance is small
  enough to be actionable. Define that threshold as a named constant and
  justify the value in a comment; an unbounded "closest match is 900 lines
  away" is noise.

Do not auto-correct the offset. Silently applying a hunk somewhere other than
where the caller anchored it is exactly the class of corruption the token
mechanism exists to prevent. Report, never relocate.

### E. Do not weaken any refusal

The strictness is the feature. Placeholder tokens, the empty-content hash on a
non-empty file, and genuine version mismatches must all still refuse with
their current distinct messages. This plan changes *what the caller is told*,
not *what is refused*.

## Work order, test-first

Write the failing tests first, then implement A-D.

### New tests in `read_file_tool.rs`

1. `offset_description_names_bytes_not_lines` — assert the schema description
   for `offset` contains "not a line number". Pins item A against a silent
   revert.
2. `line_reads_from_the_requested_line` — a file with known line content; read
   with `line: 3`; assert the returned content starts at line 3 and the report
   echoes both `line: 3` and the resolved byte `offset`.
3. `line_and_offset_are_mutually_exclusive` — supplying both refuses `-32602`
   naming both fields.
4. `line_zero_is_invalid` — refuses `-32602`.
5. `line_past_eof_refuses_with_the_line_count` — refuses `-32602` naming the
   number of lines actually present.
6. `line_read_truncated_still_returns_a_usable_token` — mirror of the existing
   `truncated_read_still_returns_a_usable_token` but entered via `line`;
   asserts the digest is still the whole-file hash.

### New tests in `apply_patch_tool.rs`

7. `mismatch_names_the_first_differing_line` — build a file and a patch whose
   context line differs; assert the refusal carries `line`, `expected`, and
   `actual`.
8. `mismatch_escapes_whitespace_in_both_lines` — the regression test for the
   observed failure: file has a tab, patch has spaces at the same position.
   Assert `expected` renders the whitespace distinguishably from `actual`.
   This test must fail before item C lands.
9. `mismatch_with_no_matching_line_omits_the_line_field` — when no line of the
   file matches, the `line`/`expected`/`actual` fields are absent (or null)
   and the message is unchanged.
10. `drifted_hunk_reports_the_candidate_distance` — a hunk that matches a few
    lines below the anchor; assert `recovery` names the drift.
11. `drifted_hunk_is_not_relocated` — assert the file is **unchanged** after
    the refusal. Guards against item D being implemented as an auto-correct.
12. `mismatch_returns_structured_refusal_payload` — must still pass with its
    existing assertions (see Constraints).

### Gates

```text
cargo fmt --check
cargo test -p hanihi-mcp-server
cargo clippy -p hanihi-mcp-server --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acceptance criteria

- The `offset`/`limit` schema descriptions state the unit and deny the wrong
  one; a `line` selector exists and is mutually exclusive with `offset`.
- A line-past-EOF read refuses rather than clamping.
- A hunk mismatch reports the first differing line with both sides escaped, so
  a tab-versus-spaces difference is visible in the refusal.
- A drifted hunk reports the distance and is never auto-relocated.
- Every pre-existing refusal still refuses with its current message; no test
  that pins a message was weakened.
- `cargo fmt --check` and both clippy invocations are clean.
- No new dependencies.

## Out of scope

- **Fuzzy hunk matching** (whitespace-insensitive or similarity-based). A
  matcher that tolerates a difference cannot report which difference it
  tolerated. Strict matching plus a precise report is the design.
- **Auto-relocating a drifted hunk.** See item D.
- **Changing `offset` semantics.** Byte offsets stay; `line` is added beside
  them.
- **Line numbers in `grep` output.** `grep` already emits them; whether the
  agent should use them to compose hunks is a prompt question, not this plan.
- The staged-text commit channel owned by plan 026.

## Assumptions and risks

- **The documentation was already right.** Item A is a loudness fix, not a
  correction. Do not present it as fixing a wrong doc; the failure is a
  skimming failure, and the mitigation is redundancy in the schema text.
- **`line` adds a second way to do one thing.** Rejected alternative: make
  `offset` accept a `"line"`-prefixed string. Rejected because it overloads a
  numeric field with a union type and breaks the existing integer schema.
  Two fields with an explicit exclusivity check is the smaller wart.
- **Escaping may surprise.** Rendering `expected`/`actual` escaped means a
  human reading the refusal sees `\t` rather than a tab. That is the intent,
  but a caller that compares the strings programmatically must compare escaped
  forms. State this in the field documentation.
- **Item D's threshold is a judgement call.** There is no principled value for
  "small enough to be actionable". Pick one, name it, and comment the
  reasoning rather than presenting it as derived.
- **Interaction with plan 026.** If 026 has landed, its refusal payload may
  already define `recovery`; reuse that field and shape rather than inventing a
  parallel one. If 026 has not landed, add only the fields this plan requires
  and note in the commit message that 026 should reconcile them.
