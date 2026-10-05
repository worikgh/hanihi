# Plan 028 — A staged-text channel for commit messages

You are Hānihi. This plan is a prompt to a future session. Its job is to let a
caller set a **multi-line** commit message, which `run_command` cannot express
today because the harness splits its input on whitespace and does no shell
quoting.

This plan is self-contained. Implement it even if no other numbered plan has
landed. Do not modify anything in anticipation of other plans.

## Objective

Make `git commit -F <path>` an admitted form, so a commit message lives in a
file — one argv token — instead of being flattened into a single hyphenated
slug by whitespace splitting.

## Context

Observed twice in one session, with exact reproductions:

```text
git commit -m "Cover the CMake build tree in the C/C++ ignore template"
  -> error: pathspec 'the' did not match any file(s) known to git
     error: pathspec 'CMake' did not match any file(s) known to git
     ... one error per word of the subject

git commit -m Cover-the-CMake-build-tree-in-the-C-Cpp-ignore-template
  -> [main 56410f5] Cover-the-CMake-build-tree-in-the-C-Cpp-ignore-template
```

The second form succeeds, which is the diagnosis: the harness performs **no
quote processing**. `"` and `'` are ordinary characters. An `-m` value is
whatever single whitespace-delimited token follows it, so `-m "a b c"` becomes
`-m`, `"a`, `b`, `c"` and git reads `b`, `c"` as pathspecs.

Plan 026 recorded the same limitation as a deferred gap and attributed the fix
to "plan 027". No such plan exists: 027 was written for tool-error
diagnosability. This plan is that staged-text channel.

The consequence of not fixing it is not cosmetic. The repository's convention
is a capitalized imperative subject plus a wrapped body ending in `Hānihi`
(see `git log` — every recent commit follows it, and `working/commit-msg-*.txt`
are hand-written bodies from earlier sessions working around this exact gap).
A harness that cannot produce a body forces either a slug subject or a bodyless
commit, and both are visible convention violations.

## Read first, verbatim

- `crates/hanihi-core/src/tool.rs`
  - `check_git_commit` (~991) — the gate being extended. Note it currently
    inspects `argv[2..]` for `--amend` and passes everything else through.
  - `check_commit_amend_args` (~1003) and `GIT_COMMIT_AMEND_OK` (~599),
    `GIT_COMMIT_MESSAGE_FLAGS` (~601) — the whitelist style to match.
  - `run_command`'s `PortableDynamicTool` (`builtin_run_command_for`) — note
    `command.split_whitespace().map(String::from).collect::<Vec<String>>()`,
    which is the mechanism this plan routes around. Do **not** change it.
  - `write_trace` and the `traces_dir` plumbing — precedent for the harness
    writing into a subdirectory of the working tree.
- `crates/hanihi-core/src/write.rs` — `write_file`'s path policy: protected
  paths, ignored paths, escapes. A message file must satisfy the same rules.
- `plans/007-git-write-tools.md` (~109-117) — `git_commit` there runs
  `git commit -m <message>` via `git_run`, a trusted fixed-argv executor. That
  is a *different* path with no whitespace-splitting problem; do not conflate
  the two, but note that `-F` would also serve it.
- `working/commit-msg-023.txt`, `working/commit-msg.txt` — real examples of the
  message shape this plan must be able to emit.

## Design

### A. Admit `-F` / `--file` in `check_git_commit`

Accept a commit whose message comes from a file:

```text
git commit -F working/commit-msg.txt
git commit --file working/commit-msg.txt
git commit -F working/commit-msg.txt --amend
```

Rules:

- `-F` and `--file` each consume the following argv element.
- The path must be **relative**, must not contain `..`, and must resolve inside
  the repository root. Reuse the existing component check used by
  `check_compiler_argv`'s input-operand branch and `list_dir` — do not write a
  third copy of that predicate; extract the shared one if that is what it takes.
- An absolute path or any `Path::Component::ParentDir` refuses.
- `-F` with no following element refuses, naming the missing argument.
- Supplying both `-F`/`--file` and `-m`/`--message` refuses: git rejects it
  anyway, but a named refusal is what this repository does.

### B. Bound where the message file may live

The file is read by git from inside the repository, so the content channel is
the file — not argv. Constrain it:

- The resolved path must lie under the working directory the harness already
  owns for stray writes. `working/` is the established location (traces and
  session logs live there); prefer it and say so in the refusal message.
- Refuse the protected paths from `write.rs` (`.ignore`, `.git*`).
- Refuse a path that is git-ignored, using the same policy `write_file` uses.

This is the security-relevant half of the plan. `-F /etc/passwd` would let a
caller read an arbitrary file into a commit message, and `-F ../secrets` would
let it leave the tree. Both are refused by the rules above; write a test for
each.

### C. Do not touch `run_command`'s splitting

The whitespace split is the documented contract (`"No shell: the command is
split on whitespace into argv"` appears in the tool description) and is what
keeps the tool shell-injection-free. Making it quote-aware is a much larger
change with its own risks and is explicitly out of scope.

`-F` is the correct fix precisely because it needs no argv change: a path is
one token.

### D. Make the failure legible while you are here

If a caller passes a multi-word `-m` and the words parse as pathspecs, the
current outcome is `pathspec '<word>' did not match` — accurate and
uninformative, the same diagnosability defect plan 027 addresses for other
tools.

Add a targeted refinement: when `git commit` carries `-m`/`--message` whose
value is empty or looks like a quote character (`"` or `'`), refuse with a
message stating that the harness does not process quotes and that a multi-line
message must be passed with `-F <path>`. Keep it narrow — do not attempt to
detect "the caller meant one long message", which is not decidable.

### E. Keep existing behaviour

Bare `git commit`, `git commit -m <token>`, `git commit --amend`, and the
existing amend whitelist must all keep working. `check_git_commit` currently
returns `Ok(())` for anything that is not `--amend`; that permissiveness must
not become *less* permissive for inputs it already admits.

## Work order, test-first

Write the failing tests first, then implement A-D.

### New tests in `crates/hanihi-core/src/tool.rs` `mod tests`

Follow the existing allowlist-test style: `argv(&[...])`, `assert_allowed_both_modes`,
`assert_denied_both_modes`, with the deny cases naming the specific rule.

1. `command_allowlist_accepts_commit_message_from_file` — `git commit -F
   working/commit-msg.txt` and the `--file` spelling are admitted in write mode.
2. `command_allowlist_rejects_absolute_message_file` — `-F /tmp/msg.txt`
   refuses, naming the escape.
3. `command_allowlist_rejects_escaping_message_file` — `-F ../msg.txt` refuses.
4. `command_allowlist_rejects_protected_message_file` — `-F .gitignore` and
   `-F .ignore` refuse.
5. `command_allowlist_requires_a_file_after_dash_f` — `git commit -F` alone
   refuses, naming the missing argument.
6. `command_allowlist_rejects_message_flag_and_file_together` — `-m x -F
   working/msg.txt` refuses.
7. `command_allowlist_keeps_bare_and_message_commits` — regression guard: the
   forms admitted before this change are still admitted.
8. `commit_message_from_file_lands_a_multi_line_message` — end-to-end on a real
   temp repo (mirror the `git_repo()` helper in `tool.rs`): write a message
   file with a subject, a blank line, a body, and a trailing line, run the
   commit, then assert `git log -1 --format=%B` returns the body **with its
   newlines intact**. This is the test that proves the whole point of the plan.
9. `commit_with_a_literal_quote_in_message_is_refused_with_guidance` — item D.

### Gates

```text
cargo fmt --check
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acceptance criteria

- `git commit -F <relative-path>` and `--file` are admitted in write mode; the
  path is bounded to a non-escaping location inside the repository and outside
  the protected set.
- A commit can carry a multi-line message, verified by reading it back from
  `git log`, not by inspecting argv.
- Absolute, escaping, and protected message paths are all refused with a
  message naming the rule.
- `-F` without a following element, and `-F` combined with `-m`, both refuse.
- Every form admitted before the change is still admitted.
- `run_command`'s whitespace splitting is unchanged, and its tool description
  still says "No shell".
- `cargo fmt --check` and both clippy invocations are clean.
- No new dependencies.

## Out of scope

- **Making the whitespace split quote-aware.** See item C. Also out of scope:
  supporting embedded newlines in an `-m` value, which no argv encoding can
  carry through this tool.
- **A `--message-file` flag on the CLI**, or any harness-level change to how
  the agent emits a commit. The agent can already produce the file with
  `write_file`.
- **Amending published history.** `--amend` remains local-only, and the
  allowlist still refuses every history-rewriting verb it refuses today.
- **`git_commit` in the write-tool set** (`plans/007-git-write-tools.md`). It
  has no split problem; leave it alone.
- Reconciling 026's dangling "plan 027" reference — that is a documentation fix
  in `plans/026-deterministic-apply-patch.md`, not part of the implementation
  here.

## Assumptions and risks

- **`-F` reads the file as UTF-8 and strips comment lines.** `git commit -F`
  treats the file like an editor buffer: lines beginning `#` are stripped
  unless `--cleanup=verbatim` is given. A message whose body legitimately
  starts a line with `#` will lose that line. Decide deliberately whether to
  document this or to require `--cleanup=verbatim` alongside `-F`; do not let
  it be discovered by a caller whose message silently lost a heading.
- **The message file is a write into the repository.** It shows up in
  `git status` as an untracked file unless `working/` is ignored. Check
  whether `working/` is in `.gitignore` / `.ignore`; if it is not, note that a
  commit made this way leaves a stray file and say what the caller should do.
- **The narrowness of item D is the design.** Any heuristic broad enough to
  catch "the caller typed a sentence" is also broad enough to refuse a
  legitimate one-token message containing a quote character. Prefer a
  false-negative that keeps working over a false-positive that blocks a valid
  commit.
- **`-F` is not the only reading channel git offers** (`--file`, `-F`, and
  stdin via `-`). `-F -` reads stdin, which the harness sets to `Stdio::null()`
  — it would silently commit an empty message. Add it to the refused set
  explicitly, with a test; a bare `-` must not be treated as a filename.
