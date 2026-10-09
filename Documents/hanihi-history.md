# The history of hānihi

*A guide for a first-year computer science student, reconstructed from the Git
log and the session logs under `working/`.*

---

## How to read this document

This is a history, not a manual. It tries to answer one question: **how did
this program get to be the way it is?**

The evidence comes from two places:

1. **The Git log** — ~200 commits between 10 August and 6 October 2026. Git is
   a version control system: it records a snapshot of every change ever made to
   the source code, along with who made it and when. Reading a project's Git
   log is like reading the margin notes in a very long essay.
2. **The session logs under `working/`** — hānihi is a program that talks to a
   large language model (LLM) and carries out tasks. Every conversation it has
   is recorded, event by event, in a file called `events.jsonl`. These logs are
   unusual evidence: they are the program's own record of its own work, written
   by the program itself.

You do not need to know Rust to follow this. You do need a little vocabulary,
which each chapter introduces the first time it is used.

A word of warning about the source material. Almost all of the code and prose
in this repository was written with AI assistance — the `README.md` says so in
its second line. That means the history includes not only what was built, but
what went wrong and how the humans and the AI reasoned about it. The failures
turn out to be as informative as the successes, and several chapters are
mostly about them.

---

## Chapter 1 — Beginnings: a scaffold, and three names

**10–11 August 2026**

The very first commit is this:

```
da02c82  2026-08-10  Deepthought
agent-harness: tokio + rig + rmcp + reedline agent scaffold
```

Everything starts there. A "scaffold" is the skeleton of a program: the
structure, the dependencies, and enough code to compile and run, but not yet
anything useful. Four pieces were bolted together:

- **rig** — a library for talking to LLMs through an OpenAI-compatible chat
  interface.
- **tokio** — Rust's asynchronous runtime. "Asynchronous" here means the
  program can wait for something slow (a network reply) without freezing;
  other work continues in the meantime.
- **rmcp** — an implementation of the Model Context Protocol, a standard way
  for a program to offer *tools* to a model. A tool is a named function with a
  description and a list of arguments, which the model can ask to have called.
- **reedline** — a library for building an interactive prompt, the kind where
  you type a line, press Enter, and get a response.

Note the author of that first commit: `Deepthought`. Every commit after it is
by `Worik`. The initial scaffold came from a different source than all the work
that followed.

### The name changes

The project was not always called hānihi. Three commits in a single day record
two renames:

```
3c68319  Rename project to kākahu; agent-core/agent-cli → kakahu-core/kakahu-cli
eb36e92  Use macron everywhere acceptable: kākahu-core, kākahu-cli
5a208d2  Rename kākahu → hānihi (crates hānihi-core/hānihi-cli)
```

So: `agent-harness` → `kākahu` → `hānihi`. To "rename" a project in a Rust
workspace you must rename its *crates* (the separately compiled packages) and
update every place the old name appears. The middle commit is about the
**macron** — the horizontal bar over a vowel, as in *ā*. Māori is written with
macrons to mark long vowels, and *hānihi* is a loan word from English meaning
"harness". The name is apt: a harness does not do the running, it holds things
in place while something else does.

One commit later, reality intervened:

```
a2a687c  Prepare for crates.io: ASCII package names (hanihi-core/hanihi-cli)
```

crates.io is Rust's public library repository, and it accepts only ASCII
package names — plain unaccented characters. So the *display* name keeps the
macron (`hānihi`) while the *package* names are plain ASCII (`hanihi-core`,
`hanihi-cli`). This is why you will see both spellings throughout the
repository, and why that is not an inconsistency.

By the end of 11 August the project had a README, a description of its
environment variables, and a version number — 0.2.0 — ready for publication.

---

## Chapter 2 — Sessions: giving the program a memory

**11–12 August 2026**

An LLM has no memory. Each request you send it is independent; it does not
remember the last thing you said. To hold a conversation you must send the
entire conversation back with every request. A **session** is a named container
that does this bookkeeping: it holds the conversation history and knows which
files on disk store it.

The design was written down *before* the code, in a file called
`plans/001-sessions.md`. Writing a plan first, and keeping it, is a habit this
repository follows throughout — and it is why the history is reconstructable
at all. The plan set the shape:

```
./working/
└── sessions/
    ├── default-session/
    │   ├── session.json          # id, name, created_at, model, system_prompt
    │   └── events.jsonl          # append-only structured log
    └── my-project/
        ├── session.json
        └── events.jsonl
```

Three ideas in that layout are worth naming.

**`events.jsonl`** — JSONL stands for "JSON Lines". Each line of the file is a
complete, independently readable JSON object:

```json
{"kind":"user_input","ts":"2026-10-06T21:24:46Z","turn":1,"data":{"text":"..."}}
{"kind":"llm_response","ts":"2026-10-06T21:24:48Z","turn":1,"data":{"text":"..."}}
```

The advantage over one big JSON array is that you can append a new record by
writing one line, and read any single record without parsing the whole file.
If the process crashes mid-write, you lose at most one line.

**Append-only** — the log is only ever added to, never edited. An append-only
record is trustworthy in a way a mutable one is not: if a record exists, it
was written at that moment and has not been silently changed since.

**The lock file (`.lock`)** — if two copies of the program opened the same
session at once, they would interleave their writes and corrupt the log. The
lock file prevents this, and fails immediately rather than waiting.

Then came three commits implementing the plan, one per step:

```
87b1dc5  Step 1: session storage foundation (LogEntry, LogWriter, Session, SessionManager, lock)
39aa48e  Step 2: wire logging into Session::run (wraps Agent loop, captures reasoning, logs all events)
a537940  Step 3: CLI integration — --working-dir, --session, --new-session, session-aware REPL
```

This pattern — a numbered plan, then "Step 1", "Step 2", "Step 3" commits — is
the single most visible rhythm in the Git log. If you only take one thing from
this history, take this: **the work was planned in writing, and the plan is
still in the repository.**

### Streaming, and the arrival of 0.3.0

On 12 August, version 0.3.0 landed with a commit message that lists its own
three headline features:

```
5cc9c93  Bump to 0.3.0 — streaming, durable execution, eval harness
```

"Streaming" deserves a note. Without it, a user asks a question and stares at a
blank screen until the whole answer is ready. With it, the answer appears
word by word as the model produces it. The model already sends its reply
incrementally; streaming means not hiding that from the user. `plans/004-streaming.md`
records the mechanism: the agent loop runs on a separate concurrent task and
sends events through a channel, which the display code reads as they arrive.

---

## Chapter 3 — Tools, read-only by default

**19 August – 1 September 2026**

This is the chapter where hānihi stops being a chat program and becomes an
*agent*: something that acts on the world rather than only talking about it.

The turning point is `plans/005-self-improvement.md`, written on 19 August,
which states four design principles that still govern the code today. They are
worth reading carefully, because they are arguments, not descriptions:

> **Read-only by default.** The agent's baseline posture is observational:
> read, search, list, and read-only commands are always available; write tools
> are registered **only** under `--write`. A wrong read costs nothing and is
> re-runnable; a wrong write can be destructive and never is.

> **Writes gated.** Gate, not block: the point is to turn every mutation into a
> visible, reversible, attributable artifact.

> **Changes are git commits.** Git is memory, undo button, and audit trail in
> one.

> **Improvement measured by the eval harness — never self-reported.** LLMs are
> fluent, confident self-advocates — "I think this is better" is not evidence.

That fourth principle deserves to be underlined. If you are building a program
that modifies its own code, the program is the *last* thing you should trust to
tell you whether it succeeded. So the plan builds an **eval harness**: a set of
fixed test cases, each with assertions checked mechanically against the session
log. Later chapters will show just how necessary this turned out to be.

An **allowlist** also enters here. Rather than letting the agent run any command
and trying to block dangerous ones, hānihi permits only a listed set
(`cargo check`, `git status`, and so on) and refuses everything else. The commit
`run_command` implementation states three further constraints: no shell (so
characters like `;` and `&&` never reach the operating system), the working
directory is pinned to the repository root, and the environment is scrubbed so
API keys cannot leak into a subprocess.

The implementation arrived as three commits in one day:

```
c314a31  Step 1: run_command tool (allowlisted cargo/git, no shell, timeout, trace persistence) + fix streaming log gap
1fffcd4  Steps 2-4, 6: write tools (apply_patch/write_file, --write flag), grep, read_session_log, task mode (--task/--max-turns), CLI wiring
faae0ec  Steps 5, 7: eval repo/write_tools/fixture fields + build/test/clippy/no_diff assertions, cases 003-self-build & 004-self-patch; scripts/self-improve.sh driver; README + plan 005 status
```

### `apply_patch` and the first real trouble

The write tool `apply_patch` takes a **unified diff** — the format Git uses to
describe a change: lines to be removed are prefixed `-`, lines to be added `+`,
and surrounding unchanged lines give the context that anchors where the change
belongs.

The first implementation shelled out to `git apply --3way`. It did not work
reliably, and `reports/001_apply_patch.md` explains why with unusual clarity.
Two failure modes:

**Failure A — the `--3way` blob is missing.** The `--3way` mode does a
three-way merge, which requires the *old* version of the file to exist in Git's
internal object store. Hand-written patches carry made-up or stale identifiers,
so the old version is simply not there and the merge cannot begin.

**Failure B — hunks without trailing context are refused.** A hunk is one
contiguous block of change. If a hunk inserts lines at the end of a file with
no unchanged lines *after* the insertion point, `git apply` treats the location
as ambiguous and refuses when the file has changed since. The report gives the
reproduction and the diagnosis:

> `git apply` searches for the single context line `fn main() {}` and, because
> the insertion point has **no trailing context**, it treats the location as
> ambiguous/unsafe when the file has since changed.

The resolution was to abandon `git apply` entirely and write a patch applier in
plain Rust (commit `30a6952`, `Replace-git-apply-with-pure-Rust-patch-applier`).
The new applier matches the old side of each hunk against the *current* contents
of the file, exactly as the agent itself is reading it, and writes files only
if *every* hunk matches. This is the first instance of a pattern that recurs
throughout this history: **when a tool fails for reasons the caller cannot
control, replace the tool rather than teaching the caller to work around it.**

### Other durable ideas from this stretch

- `read_session_log` (commit `1fffcd4`) — gives the agent a window into *its
  own* event log. This is what makes it possible for a later session to learn
  from an earlier one's mistakes.
- The **read-only cache** (commit `fb695c9`, `Deduplicate-identical-read-only-tool-calls-per-turn`)
  — within a single turn, repeating an identical read returns the earlier
  result instead of re-executing. Reading the same file twice cannot produce a
  different answer, so the second read is wasted.
- `analyse` (commit `c30ffae`) and later `hanihi-session`, a separate program
  that reads session logs *without* needing a model. Offline introspection is a
  recurring theme.

---

## Chapter 4 — The C++ turn: generalising beyond Rust

**2–5 October 2026**

Through September the tooling had assumed one language: Rust, built with
`cargo`. The allowlist admitted `cargo` subcommands. The ignore-file template
knew about `target/`. The system preamble told the model to run `cargo fmt` and
`cargo clippy`.

Then came a plan series numbered 020–025 whose explicit purpose was to make all
of that work for a C++ project built with CMake. Five commits mark it:

```
7922e79  2026-10-02  Detect the repository build toolchain in SourceTree
42bc7c1  2026-10-03  Admit cmake, ctest, and per-file compiles in run_command
cb110d9  2026-10-05  Make the verification preamble depend on the build toolchain
16c1922  2026-10-05  Let eval cases name their own build, test, and lint commands
56410f5  2026-10-05  Cover-the-CMake-build-tree-in-the-C-Cpp-ignore-template
```

Each line addresses a different layer of the same assumption:

- **Detection** — the program inspects the repository for marker files
  (`Cargo.toml`, `CMakeLists.txt`) and decides which toolchain is in play.
- **Permission** — `cmake -B build`, `cmake --build build`, `ctest --test-dir build`,
  and per-file compiles (`g++ -c file.cpp -fsyntax-only`) join the allowlist.
- **Instruction** — the paragraph the model receives is chosen to match the
  repository, so a C++ project is never told to run `cargo clippy`.
- **Verification** — eval cases may name their own build, test, and lint
  commands, so the eval harness can grade a CMake project as readily as a Cargo one.
- **Housekeeping** — `build/` directories are ignored so they never appear in a
  listing the agent sees.

Note the fifth commit's subject:
`Cover-the-CMake-build-tree-in-the-C-Cpp-ignore-template`. Every word is joined
by hyphens. That is not a stylistic choice — see the next chapter.

### The smoke test that was never finished

`plans/025-cpp-smoke-test.md` sets out a manual verification exercise: build a
throwaway CMake project *outside* the repository, then drive the agent through
seven steps — configure, build, targeted build, test, per-file compile, the
ignore check, and a read-back. The plan is emphatic that this is verification
rather than a feature, and it names step 6 as the one that might fail:

> **This is the step plan 021 flags as possibly failing.** […] If `build/`
> **does** appear in `git status --short`: **Stop.** Do not attempt a fix.

The evidence that this ran at all survives as `working/smoke-025/cpp-smoke/`,
containing exactly the fixture the plan specifies — `CMakeLists.txt`,
`include/greet.hpp`, `src/greet.cpp`, `src/main.cpp` — plus eleven recorded
traces under `working/traces/025-cpp-smoke-test/`. But no written report
exists, and the archived-plans README records the outcome as
"verification exercise; outcome unrecorded." The fixture is there, the traces
are there, the conclusion is not. **An experiment that ran but was never
written up leaves the question open**, and this history is honest about that.

---

## Chapter 5 — Learning from failure: the sessions that went wrong

**Throughout, but especially 1–4 October 2026**

Here this history stops being a list of features. Four reports in `reports/`
record sessions that went badly, in the agent's own words, because somebody
decided that the failures were worth writing down rather than hiding.

### The commit messages that lost their spaces

The most legible failure is visible directly in the Git log. From Chapter 4:

```
56410f5  Cover-the-CMake-build-tree-in-the-C-Cpp-ignore-template
18e2897  Amend-the-commit-subject-and-record-plan-028
```

and elsewhere, a scattering of other hyphenated subjects. Meanwhile almost
every other commit subject in the log is normal prose:

```
7922e79  Detect the repository build toolchain in SourceTree
```

The explanation is in `plans/archived/complete/002-commit-message-file.md`. The
tool that runs commands splits its input **on whitespace**, with no quote
processing — `"` and `'` are ordinary characters. So this:

```
git commit -m "Cover the CMake build tree in the C/C++ ignore template"
```

becomes the argument list `-m`, `"Cover`, `the`, `CMake`, `build`, … and Git
reads the middle words as filenames that do not exist:

```
error: pathspec 'the' did not match any file(s) known to git
error: pathspec 'CMake' did not match any file(s) known to git
```

The workaround somebody reached for was to delete the spaces. And the fix,
landed much later, was to stop trying to pass the message through the argument
list at all: put the message in a *file* and pass the file's path, which is a
single token with no spaces in it.

```
04b5e93  2026-10-06  Admit git commit -F as the multi-line message channel
```

The `working/` directory preserves the fossil record of the workaround:
`commit-msg-002.txt`, `commit-msg-003.txt`, `commit-msg-004.txt`,
`commit-msg-022.txt`, `commit-msg-023.txt`, `commit-msg-024.txt`,
`commit-msg-027.txt`, `commit-msg.txt`, and others. These are hand-written
message files, staged on disk precisely because the argument list could not
carry them. Even the *filenames* tell you which plan needed one.

This is a good first lesson in software engineering: **a bad interface does not
produce bad work, it produces corrupted work that looks deliberate.** Every
hyphenated commit subject in this log is a person or a model working around a
tool.

### The capability claim

`reports/003_preamble_session_failures.md` recounts a session in which the agent
had two tool servers attached, one of them providing write tools, and
nonetheless announced that it had no write tools and asked the human to make
the edits by hand.

> I had inferred tool availability from a partial `tools/list` payload I had read
> out of `minimal-mcp-rw.log` — a log belonging to a *different* server, in a
> *different* directory — and presented that inference as fact.

The report's own summary of the rule it broke is worth quoting:

> **Rule that would have prevented it:** before claiming a capability is absent,
> probe for it. Reading another process's log is not a probe.

That single false claim turned out to shape the next two days of work.

### The stale token

`reports/002_session_failures.md` counts the damage in one session: seven
`apply_patch` attempts, five failures. The cause was a mechanism the harness had
introduced deliberately. To protect against editing a file that had changed
since you read it, `apply_patch` requires a **`base_token`** — effectively a
fingerprint of the file's contents, computed with a hash function called
SHA-256. (A hash function turns any input into a fixed-length string; change
one character of the input and the string changes completely. So a hash is a
compact way of saying "exactly this content".) If the file's current hash does
not match the token you supplied, the write is refused.

The mechanism worked correctly five times out of five. The *caller* kept
supplying stale tokens — a hash from a previous turn, a hash from a terminal
command, a hash from memory:

> Failure 5 is the worst, because I had *just* written a paragraph promising not
> to repeat failure 4.

The report's conclusion is a design idea rather than a scolding:

> **Deterministic fix:** make the edit handle a first-class value the harness
> tracks, not something I carry in text.

That fix landed on 4 October:

```
031f1a7  Add deterministic version ledger and structured apply_patch refusals
```

A **ledger** is a record the harness keeps itself: every successful read and
every successful write updates the ledger's entry for that file. The caller can
then ask for `base_token: "auto"` and let the harness fill in the current value.
State that used to live in the model's memory now lives where it cannot be
forgotten.

### The green tick that meant nothing

The smallest failure is the most instructive. When the agent called a tool, the
interface printed a green tick `✅`. But a tick meant only that the call had made
a round trip to the tool server — *not* that the tool had succeeded. A refused
call also round-tripped, so it also got a tick. The report describes the
consequence: the original `find_symbol` question "was answerable, and it recurred
before `search_text` and `read_file`."

The fix is a small, precise piece of engineering:

```
9d41b08  Make tool results legible and observable at the harness edge
```

> The CLI printed a green tick for every completed tool event. […] The tick
> therefore marked transport success as tool success, which is how a refused
> `find_symbol` came to be read as a successful call.

**A status indicator that cannot distinguish success from failure is worse than
no indicator at all**, because people trust it.

---

## Chapter 6 — Making the program check itself

**5–6 October 2026**

Chapter 3 quoted the fourth design principle: improvement must be measured, never
self-reported. The last two days of the log are the payoff — and also the moment
the project turns its attention to the agent's *honesty* about its own work.

Four commits, in order:

```
fe1182c  2026-10-06  Assert the answer against the agent's own tool log
4495bd5  2026-10-06  Audit each turn against its own tool activity
00fbf43  2026-10-06  Make tool failures self-explaining
cf76e16  2026-10-06  Fix bug with string splitting during context compaction
```

The first two are the important ones, and they attack the failure from Chapter 5
directly. The problem: the agent said it had no write tools while its own log
recorded them. No existing test could catch that, because every test checked
either the *text* of the answer or the *structure* of the log, and this
contradiction lives in the relationship between the two.

Two new assertions were added to the eval harness, and they are best understood
as a matched pair:

| assertion | passes when |
|---|---|
| `no_unsupported_capability_claim` | the answer claims no missing capability, **or** the log holds a tool-execution error the claim can cite |
| `reported_tool_errors_are_real` | the answer claims no tool failure, **or** the log holds at least one tool-execution error |

Then the same check was added at *runtime*, so a contradiction is noticed during
the session rather than only afterwards. The commit body records two decisions
about it that are worth internalising:

> The check is deliberately narrow — sentence-split text, an explicit English
> phrase list, case-folded — so it stays silent on honest prose such as "I don't
> have the file contents yet". A differently-worded false claim passes; that
> false negative is the accepted cost of an audit a reader will not learn to
> ignore.

And:

> The check is a **diagnostic**: it never fails or aborts a turn, so a heuristic
> can never kill a run.

Both of those are engineering judgement, not correctness. A broader matcher
would catch more lies *and* start flagging honest answers, at which point people
stop reading the warnings. A checker that can abort a run is a checker that can
be wrong in the expensive direction. Choosing the narrow version, and saying so
in writing, is the mature call — and note that it was written down, so a future
session encountering a false negative knows it was a decision rather than an
oversight.

### Context compaction, and a bug in the middle of it

One commit in this group looks out of place:

```
cf76e16  Fix bug with string splitting during context compaction
```

**Context compaction** is the solution to a hard constraint: a model can only
accept a limited amount of text at once (its "context"), but a long session
exceeds that. When the history grows too large, hānihi summarises the older part
into a short rolling summary and drops the original messages. The summary is
carried forward and updated as the session continues.

The bug was in splitting a string — precisely the class of error a first-year
student meets in their first week, in a component whose whole purpose is to keep
a long conversation inside a size limit. It is a useful reminder that the
sophisticated parts of a system are not immune to elementary mistakes.

---

## Chapter 7 — Where things stand, and what is unfinished

**6 October 2026 — the present**

### The shape of the repository

```
crates/
├── hanihi-core/        # the library: agent loop, tools, sessions
├── hanihi-cli/         # the command-line program and interactive prompt
├── hanihi-eval/        # runs test cases against a live model
├── hanihi-mcp-server/  # tool servers (read-only and read-write)
└── hanihi-session/     # reads session logs offline, no model needed
```

Five crates, workspace version 0.3.0, Rust edition 2024. The split matters: the
*logic* lives in `hanihi-core`, and the *interfaces* — command line, eval
harness, session inspector — are separate programs built on top of it. A
different interface could be added without touching the logic.

### The rule that holds it together

If you had to compress this whole history into one sentence, it would be the
README's own description of the source-tree tools:

> Everything is filtered by the repo's ignore rules […] Reads are capped at
> 64 KiB, escapes outside the repo are refused, and `target/`-style noise never
> reaches the model.

Read-only by default. Writes gated behind a flag. Boundaries enforced in code
rather than by instruction. Commits, never pushes. Every one of those is a
decision that a mistake should be cheap.

### What is genuinely unfinished

The archived-plans README is unusually honest about this, and the honesty is
itself a milestone. Three items stand out:

**1. The non-streaming path was never stubbed.** Plan 029 said three changes
"must land together"; two did.

> **Part D did not:** `Agent::run` and `Session::run` are still fully implemented
> rather than stubbed. […] this file is archived with that inconsistency still live.

**2. A plan was misattributed.** The archived README records that plan 026
attributes the commit-message fix to "plan 027", which was written for something
else entirely. Rather than quietly editing the old plan, the correction is
recorded as a caveat — and the real owner, plan 028, is now implemented.

**3. A document contradicts the code.** `plans/005-outstanding-gaps.md` is
blunt about it:

> `report.md` documents `schema` as `1` and lists nine event kinds.
> `SCHEMA_VERSION` is `2` and there are ten (`compaction` was added). It is
> worse than absent: a reader who trusts it will write a reader for the wrong
> format.

A stale document is not neutral. Somebody — probably an AI, probably within
days — will read it and believe it.

### What changed in how the work is done

The last commit in the log is not a feature. It is this:

```
7102999  2026-10-06  Rewrite plan sequence and archive completed plans
```

Fifteen plans marked "draft" were in fact fully implemented. Two different files
shared the number 006, one a strict subset of the other. The plans directory had
drifted out of step with the code it described. The fix was to archive the
completed plans under `plans/archived/complete/` with a README recording what
each one became, to delete the duplicate, and to rewrite the remaining plans as
a numbered *sequence* with a stated reading order.

And the commit that reordered that sequence gives the reason:

> The commit-message plan leads because it is the only one whose defect
> reproduces in this repository's own history: the slug subjects at `18e2897`
> and `56410f5` are exactly what whitespace splitting produces.

That is the whole history in one sentence. The bug from Chapter 5 is not
described in the abstract; it is *cited by commit hash*, because the evidence
was sitting in the log the entire time.

---

## Appendix: the tools you need to read a log like this

If you want to explore the history yourself, four Git commands will take you a
long way. All of them are read-only.

```sh
# Every commit, oldest first, one line each
git log --oneline --reverse

# Every commit with its date and author
git log --pretty=format:'%h|%ad|%an|%s' --date=short

# What a single commit changed
git show 30a6952

# Which commits touched one file
git log --oneline -- crates/hanihi-core/src/tool.rs
```

And for the session logs, which are plain text:

```sh
# One afternoon's worth of commits
git log --since=2026-10-05 --until=2026-10-06

# Every tool the agent called in a logged session
grep '"kind":"tool_execution"' working/sessions/<name>/events.jsonl
```

The last command is the interesting one. It shows you what the program *did*, as
opposed to what it *said* — and the difference between those two things is what
this entire history is about.

---

*Reconstructed from the Git log (10 August – 6 October 2026) and the session
logs, traces, plans, and reports under `working/`.*
