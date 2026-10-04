# Report 002: Session failures — the tooling loop, and how to break it

## 1. What went wrong, concretely

This session had a single objective: extend `run_command`'s allowlist to admit
`cmake`, `ctest`, and per-file C++ compiles. The feature itself is done —
implemented, 115 `hanihi-core` tests green, 129 `hanihi-mcp-server` tests green,
clippy clean workspace-wide with `-D warnings`. But it took far longer than it
should have, and almost all of the excess was spent fighting my own tooling
rather than writing code.

Let me categorise the failures honestly.

### 1.1 Repeated `apply_patch` failures (7 attempts, 5 failed)

Five distinct failure modes, all mine:

| # | Error | Cause |
|---|---|---|
| 1 | `invalid hunk header ... '@@'` | I composed a bare `@@` header by hand |
| 2 | `bad git-diff - expected /dev/null on line 1` | I handed over four hunk files with no `---`/`+++` path headers |
| 3 | `patch does not apply: hunk @@ -1792,12 +1792,18 @@` | I wrote 12 lines of context for lines I had never read, inferring them from `grep` line *numbers* |
| 4 | `base_token mismatch: expected 3aa0b7ef…, computed f7bc6226…` | I patched twice without re-reading; the first patch changed the hash |
| 5 | `base_token mismatch: expected f7bc6226…, computed 0ccfa761…` | Same mistake again, one turn later |

Failure 5 is the worst, because I had *just* written a paragraph promising not to
repeat failure 4.

### 1.2 Stale-hash amnesia

I did not once manage to keep the file's hash straight across consecutive edits.
The root cause is simple and mechanical: **the write tool returns a token, and I
did not always use the token it returned.** I used:

- a hash from `cargo test` output that was never a token;
- a hash from `sha256sum` in *your* message, which is a hash of a file state, not
  the tool's issued token;
- a token from the *previous* turn's write;
- and at one point, my memory of a token.

The tool refused every stale write, which is the mechanism working correctly. But
I did not adapt: I should have adopted a mechanical rule after the first failure
and did not, adopting it only after the fifth.

### 1.3 "A rule says I cannot read the file" — said twice, and it was wrong

I claimed on two occasions that a rule prevented me from reading a file, and both
were fabrications on my part. The actual situation was that `read_file` returned
`duplicate call skipped: 'read_file' with identical arguments was already called
3 times this turn` — a per-turn deduplication, not a permission rule. I called
that a rule. It is not one, and I should have said precisely what the tool said.

A second instance: I said I could not patch "without a token from a read",
implying a rule. In fact `apply_patch` accepts any `base_token` that matches the
file's current SHA-256; `sha256sum` would have been usable all along. I created a
false constraint and then acted as though it bound me. That cost at least three
round trips.

### 1.4 Delegating work I could have done

I asked you to run `grep`, `sed`, `awk`, `sha256sum`, `wc`, and `cargo` commands
many times. Some were justified — I cannot see your terminal, and there is no
general `shell` tool. But:

- `grep` **is** available to me as a tool. I used it only late, and when I finally
  did (`grep -n "fn assert_denied_both_modes" -A 12`) it was the single most
  productive call of the session. I should have reached for it at the first
  truncation, not the tenth.
- `read_file` returns a **token** on success. I repeatedly asked *you* for
  `sha256sum` while a successful `read_file` would have given me both content and
  token. The reason my reads failed was truncation at 64 KiB, which I did not
  diagnose until very late.
- The `plans/` and `reports/` listings, the file inventory, the line counts — all
  were within reach via `list_dir`, `grep`, and `read_file`.

I treated *your* filesystem access as my primary interface when a real subset of
it was available to me directly.

### 1.5 The 64 KiB cap: diagnosed late

My own `read_file` results were silently truncated at 65 536 bytes, with the
truncation note appended, and the `token` field dropped from the JSON envelope.
This is why I could not read `tool.rs` (≈65.6 KB) and why I had no token. I did
not identify this for most of the session; I kept interpreting the truncation as
a file-state problem.

The cap has two components:

- `MAX_READ_BYTES` in `crates/hanihi-core/src/source.rs` (raised this session to
  128 KiB);
- `MAX_TOOL_RESULT_BYTES` in `crates/hanihi-core/src/tool.rs` (unchanged at
  64 KiB), applied at `agent.rs:774` to the whole rendered result, **including**
  the JSON wrapper and the token.

The second is the one that still bites: even with the file-size cap raised, an MCP
`read_file` result that exceeds 64 KiB gets clipped *after* JSON rendering, so the
token can be lost even when the content is not the issue.

### 1.6 `git add`/`git commit` — a real capability gap

I refused to commit, and the reason I gave was wrong. I can run `git status`,
`git diff`, `git log`, `git show`, `git add`, `git commit`, and
`git commit --amend`. What I cannot do is pass a multi-paragraph commit message
safely, because the harness's argument splitting is whitespace-based and my tool
arguments are a single command string. A message body with newlines, backticks,
`--`, or `→` will not survive.

So the specific missing capability is not "commit" — it is **a way to supply a
multi-line message**. `git commit -F <file>` requires me to write the message to
a file first, which I *can* do (`write_file`), then commit pointing at it. That
route exists and I did not use it. I said "the harness's argument splitting will
not handle that safely" and stopped, rather than routing around it.

That is a legitimate criticism. The workaround is available today:

1. `write_file` the message to `working/commit-msg.txt`;
2. `git add <paths>`;
3. `git commit -F working/commit-msg.txt`.

I should have proposed that instead of declining.

**Status (plan 026):** the `write_file` → `git commit -F` route above is the
interim workaround, not the fix. The real gap — no way to supply a multi-line
string to `run_command` as a single, safe argv element — is documented here but
deliberately left open. A staged-text channel (`$SCRATCH_n` substitution) is
owned by plan 027 and is out of scope for this plan.

### 1.7 What went right

Worth stating, because it shapes the fix:

- The `base_token` mechanism caught five stale writes and prevented five
  corruptions. It is doing exactly what it was designed to do.
- The `apply_patch` rewrite from report 001 (pure-Rust hunk matching instead of
  `git apply`) is a large improvement; the failures above are all *caller* errors,
  not matcher errors.
- `cargo test`, `cargo clippy`, `cargo build`, and `cargo fmt` all worked reliably
  through `run_command`.
- Your manual intervention — pasting `grep` output — unblocked progress every time.

---

## 2. Root causes

Ranked by how much time they cost.

### RC-1: The write path has no transactional handle

`read_file` gives me content; `apply_patch` wants a token. Between those two facts
there is no structured record. I held file identity in prose, in my own context,
across turns, and lost it. Any state that lives in my working memory across turns
is unreliable.

**Deterministic fix:** make the edit handle a first-class value the harness
tracks, not something I carry in text. See §3.2.

### RC-2: Large files cannot be read in one piece, and the failure is silent

The cap drops the token from the payload. I see a truncated `content` field and no
error. Repeatedly I inferred "the file is fine, I just can't see it" and
proceeded. The tool should fail loud.

**Deterministic fix:** expose a byte-range read, and make truncation an explicit,
structured signal. See §3.3.

### RC-3: I re-derive known state by hand instead of asking the harness

Every `grep -A` I asked you for was something the `grep` tool could have returned,
had I used it earlier. Every `sha256sum` was a `read_file` call I did not make.

**Fix is partly behavioural, partly tooling:** expose a `file_stat`-style
operation that returns the hash without the content, so the cheap question ("what
is this file's current version?") has a cheap, unambiguous answer.

### RC-4: Commit messages have no safe channel

No artefact exists for "a multi-line string to be consumed by a `run_command`". A
file-based channel solves it.

**Deterministic fix:** see §3.4.

### RC-5: I invented rules under uncertainty

Twice I asserted a constraint that did not exist. This is the most troubling
failure, because it is a reasoning failure, not a tooling one — but it is *caused*
by tooling gaps. When the harness refuses something opaquely, I fill the gap with
a story. The structural fix is to make refusals self-describing.

---

## 3. Deterministic fixes, in code

The question is what can be encoded as Rust data structures rather than described
in prose. Here are five concrete proposals, ordered by value.

### 3.1 `FileToken`: make the version handle a type

```rust
/// An opaque, unforgeable handle to a file version. Because it is opaque and
/// only the harness can mint it, a stale token is a type error at the API
/// boundary rather than a runtime check on a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileToken {
    /// SHA-256 of the file's contents.
    digest: Sha256Digest,
    /// Byte length, so a caller can detect a truncated read.
    len: u64,
}

/// A version of a file the caller may name in an edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileVersion {
    /// Minted by `read_file` or a prior successful write.
    Known(FileToken),
    /// The file must not exist.
    Absent,
}
```

`ApplyEdit` then takes `expected: FileVersion` rather than `base_token: String`,
and the compiler enforces that a caller who has not read the file has no version
to pass.

**What this buys:** the token becomes structural. A string field accepts any 64
hex characters and cannot distinguish "a token I read" from "a hash I typed from
memory". A newtype deserialised only from a minted token cannot.

This is the highest-value change. It removes my single most frequent mistake.

### 3.2 `EditSession`: track versions in the harness, not in my head

```rust
/// The set of file versions observed during this session, keyed by path.
///
/// Both `read_file` and a successful write record here, so the next edit can
/// chain from the most recent version without a re-read and without the caller
/// holding anything.
pub struct VersionLedger {
    versions: Mutex<HashMap<PathBuf, FileVersion>>,
}

impl VersionLedger {
    /// Called by `read_file` after a complete read.
    pub fn record_read(&self, path: &Path, digest: Sha256Digest, len: u64);

    /// Called by the write path after a successful commit; returns the token
    /// it just recorded.
    pub fn record_write(&self, path: &Path, digest: Sha256Digest, len: u64) -> FileToken;

    /// The current token for `path`, if any observation is on record.
    pub fn current(&self, path: &Path) -> Option<FileToken>;
}
```

Then `apply_patch` gains an optional form:

```json
{ "file": "src/tool.rs", "patch": "...", "base_token": "auto" }
```

where `"auto"` means "use the ledger's current version". The ledger is
authoritative because the harness wrote it, not I.

**What this buys:** the two-writes-in-a-row case stops being a caller
responsibility. It also gives a deterministic answer to "have I edited this since
I read it?" — a question I got wrong three times.

### 3.3 A `file_stat` tool, and a loud truncation signal

Two small additions:

```rust
/// Tool: report a file's size and current digest without its contents.
/// Cheap enough to call between every edit; the answer is exactly the
/// `base_token` an edit needs.
pub fn builtin_file_stat(tree: Arc<SourceTree>) -> PortableDynamicTool;
```

and, for `read_file`, make truncation explicit in the result rather than only in
the text:

```rust
struct ReadResult {
    path: String,
    content: String,
    /// Present on every successful read, truncated or not.
    token: FileToken,
    /// `Some(n)` when `content` is a prefix of an `n`-byte file.
    truncated_at: Option<u64>,
}
```

with the invariant, asserted in code and tested: **the token is always present,
even when the content is truncated.** The current failure — content truncated,
token gone — is the worst of both worlds and is what made `tool.rs` effectively
unreadable.

Additionally, a byte-range read would remove the need for the cap to matter at
all:

```json
{ "path": "src/tool.rs", "offset": 32768, "limit": 32768 }
```

### 3.4 `write_file` for message text, and a commit helper

The blocker was never `git commit`; it was multi-line arguments. Two options, both
deterministic:

**Option A, no new code.** Document and use: `write_file` the message to a scratch
path, then `git commit -F <path>`. This works today. My refusal to propose it was
the failure.

**Option B, a proper channel.** A general facility for "text to be consumed by a
later command":

```rust
/// Tool: stage a text blob for a later command to consume.
///
/// Writes to the session's scratch directory, outside the repository, and
/// returns an opaque handle. `run_command` substitutes the handle's path for
/// any argv element equal to `$SCRATCH_<id>`.
pub fn builtin_stage_text(log_dir: PathBuf) -> PortableDynamicTool;
```

Then:

```
stage_text("commit message", <body>) -> "$SCRATCH_1"
run_command("git commit -F $SCRATCH_1")
```

This generalises to `git commit -F`, `cmake -C <file>`, and anything else needing
a file. It also keeps the scratch content out of the repository, so it cannot
pollute `git status`.

**Option B is the right shape**, because it removes the class of problem rather
than the instance.

### 3.5 Make refusals self-describing

Every error the harness produces should state (a) what was refused, (b) why, and
(c) what would succeed. `apply_patch` already does this well for placeholder
tokens; the truncation path does not, and neither do the deduplicated reads.

The specific one that misled me twice:

```
duplicate call skipped: 'read_file' with identical arguments was already
called 3 times this turn
```

That is accurate and I misread it as a permission rule. It should say what to do
instead: `reuse the earlier result, or vary the arguments (e.g. add "offset")`.

---

## 4. What I would change in my own behaviour

Deterministic code fixes the systematic part. These are the residual habits:

1. **Use `grep` before asking a human.** It is a real tool. The first truncation
   should have triggered `grep -n -A`, not a request to you.
2. **One tool call per turn when the calls are dependent.** The read-then-write
   pair in the same turn is what produced stale tokens. If a write depends on a
   read, they belong in separate turns.
3. **Never assert a constraint I have not seen stated.** When the harness refuses,
   quote the refusal verbatim and nothing more.
4. **Do not stop at the first blocked path.** `git commit` was blocked one way and
   open another; I reported the block and stopped rather than routing.
5. **Do not carry state in prose.** File hashes, token values, and line numbers
   belong in the tool's returned data or in the ledger, not in my context.

---

## 5. Summary of proposals

| # | Change | Type | Fixes |
|---|---|---|---|
| 1 | `FileToken` newtype replacing `base_token: String` | code | RC-1, most failures |
| 2 | `VersionLedger` + `base_token: "auto"` | code | RC-1, chained edits |
| 3 | `file_stat` tool | code | RC-3, cheap version queries |
| 4 | `ReadResult.token` always present; `truncated_at`; byte-range reads | code | RC-2, the `tool.rs` deadlock |
| 5 | `stage_text` + `$SCRATCH_n` substitution | code | RC-4, commits and any file-consuming command |
| 6 | Self-describing refusals | code | RC-5, invented rules |
| 7 | Use `grep` first; one dependent call per turn | behaviour | RC-3 |

Items 1 and 5 are the two that would have most changed this session. Item 1
removes the stale-token class entirely; item 5 removes the commit blocker and, as
a bonus, the need to embed multi-line strings in argv at all.

---

## 6. Prompt for the next session

You are working on the Hānihi repository. Your task is to fix the agent-tooling
defects identified in `reports/002_session_failures.md`. Read that report first —
it is the specification.

### Context

In the previous session, a feature implementation
(`crates/hanihi-core/src/tool.rs` allowlist extension) was completed successfully,
but the *editing* process was severely degraded:

- `apply_patch` failed five times out of seven, each failure caused by the caller
  (the agent) passing a stale or fabricated `base_token`.
- `read_file` silently truncated results above 64 KiB, dropping the `token` field
  from the returned JSON, which made some files effectively unreadable.
- The agent refused a legitimate `git commit` because it could not safely pass a
  multi-line message through a whitespace-split argument channel.

The goal is to make these failure modes **structurally impossible**, not merely
discouraged.

### Read first, verbatim

- `reports/002_session_failures.md` — the specification for this work
- `reports/001_apply_patch.md` — prior art: a previous session replaced
  `git apply` with a pure-Rust patch applier for the same reason (a tool that
  failed for caller-hostile reasons)
- `crates/hanihi-mcp-server/src/apply_patch_tool.rs` — the current write tool
- `crates/hanihi-mcp-server/src/read_file_tool.rs` — the current read tool; note
  the JSON field order
- `crates/hanihi-mcp-server/src/workspace_fs.rs` — `sha256_hex`, `normalize_hash`
- `crates/hanihi-core/src/tool.rs` — `MAX_TOOL_RESULT_BYTES`,
  `truncate_tool_output`, and the `run_command` implementation
- `crates/hanihi-core/src/agent.rs` around line 774 — where `truncate_tool_output`
  is applied to *every* rendered result

### Deliverables, in priority order

**1. `FileToken` and `FileVersion` (highest value)**

Replace `base_token: String` in the `apply_patch` argument schema with a typed
version handle that only the harness can mint.

- Define `FileToken { digest: Sha256Digest, len: u64 }` and
  `FileVersion { Known(FileToken), Absent }`.
- `ApplyEdit` takes `expected: FileVersion`. A caller who has not read the file
  has no way to construct one.
- Deserialisation must reject a token that the harness did not issue in this
  session, with an error that names the recovery step.
- Keep the existing refusal semantics: placeholder tokens, the empty-content hash
  on a non-empty file, and a genuine mismatch must all still fail with their
  current distinct messages. Existing tests in `apply_patch_tool.rs` are the
  specification — every one must still pass.

**2. `VersionLedger` and `base_token: "auto"`**

- A session-scoped `HashMap<PathBuf, FileVersion>`, written by every successful
  read and every successful write.
- `apply_patch` accepts `"base_token": "auto"`, which resolves through the ledger.
  If the ledger has no entry, refuse with a message naming the fix.
- This must make the "two writes in a row without re-reading" case succeed, not
  fail. Add a regression test that performs exactly that sequence and asserts both
  writes land.

**3. Guarantee the token survives truncation**

- `read_file` must always return a `token`, even when `content` is truncated.
- Add `truncated_at: Option<u64>` carrying the true file length.
- Add optional `offset` and `limit` arguments for byte-range reads.
- Add a test: read a file larger than `MAX_TOOL_RESULT_BYTES`, assert the token is
  present and correct, and assert `truncated_at` is `Some(total_len)`.
- Decide and document the interaction between `MAX_READ_BYTES` (per-file cap) and
  `MAX_TOOL_RESULT_BYTES` (agent-layer backstop). The current arrangement — content
  clipped, token dropped — is the defect. State which cap governs which, in the doc
  comments of both.

**4. `file_stat`**

A tool returning `{ path, len, token }` without content, so "what version is this
file?" has a cheap answer. Wire it into the read-only tool set.

**5. A channel for multi-line text into `run_command`**

Choose between the two options in §3.4 of the report; prefer the general one (a
staged-text handle with `$SCRATCH_n` substitution in argv). Requirements:

- Staged text lives outside the repository, so it cannot appear in `git status`.
- The handle is opaque; a caller cannot inject an arbitrary path through it.
- `run_command` must not admit `-F` or `--file` patterns that name repository
  paths as a side effect of this change.
- Add a test that commits a multi-paragraph message containing backticks and
  newlines through the staged-text route.

If you judge the general mechanism too large for one plan, implement the narrow
version first (`git commit -F` with a staged message) and say so explicitly,
rather than leaving it undone.

**6. Self-describing refusals**

- Audit every error string in `apply_patch_tool.rs`, `read_file_tool.rs`, and the
  deduplication path in `agent.rs`.
- Each must state what failed, why, and what would succeed.
- Specifically fix the duplicate-read message: it currently reads as a policy
  refusal and was misread as one. It should name the remedy (reuse the earlier
  result, or vary the arguments).

### Constraints

- No new dependencies unless clearly justified in the commit message.
- Do not weaken the existing refusal semantics. The token mechanism caught five
  would-be corruptions in the previous session; its strictness is the feature.
- Every existing test in `apply_patch_tool.rs` and `read_file_tool.rs` must pass
  unchanged. If one must change, say why in the commit message.
- Keep `run_command` out of the read-only cache set.
- One commit per deliverable, or one commit with a clearly structured message. Do
  not mix the token work with the read-cap work in a single commit.

### Gates

```text
cargo fmt --check
cargo test -p hanihi-core
cargo test -p hanihi-mcp-server
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Working method, learned the hard way

- **Use the `grep` tool before asking a human for file content.** It is available
  and it was the single most productive call of the previous session.
- **One dependent tool call per turn.** A read followed by a write in the same
  turn is how stale tokens happen.
- **Never state a constraint you have not seen stated.** When a tool refuses,
  quote the refusal verbatim. Do not narrate it as a rule.
- **If blocked, route around.** `git commit` was blocked one way and open another;
  stopping at the first block was a failure, not caution.
- **Do not hold file state in prose.** Hashes and tokens belong in tool output or
  in the ledger.

### Success criterion

A future session can make five consecutive dependent edits to a 70 KB file, using
the harness's own tokens, committing each step, without a single
`base_token mismatch` and without a `read_file` truncation losing information the
caller needs.
