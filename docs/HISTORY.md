# The History of Hānihi

*A guided walk through how this program was built, written for someone in their
first year of a computer science degree.*

---

## How to read this document

This is a history of a piece of software called **Hānihi**. It was built over
about two months, from 10 August 2026 to 6 October 2026. You can see the whole
story in a few places:

- The **Git log** — Git is a *version control system*. It records every change
  ever made to the code, who made it, and when. Each recorded change is called a
  **commit**. This project has 182 commits.
- The **`plans/` directory** — numbered design documents, written *before* the
  work was done. Plan 001, Plan 002, and so on.
- The **`reports/` directory** — write-ups written *after* something went wrong,
  explaining what failed and why.
- The **`working/` directory** — the program's own working files. Most
  interestingly, it contains the program's own saved conversations with itself.

You do not need to know Rust, or anything about AI, to follow this. Every piece
of jargon is explained the first time it appears.

---

## Chapter 0 — What is an agent harness?

Before the history, you need one idea.

A **large language model** (LLM) is a program that predicts text. You give it
some text, called a **prompt**, and it produces more text in response. On its
own, an LLM is a sealed box: it can talk, but it cannot *do* anything. It cannot
read a file, run a program, or check its own work.

An **agent harness** is a program that wraps around an LLM to give it hands.

```
    ┌──────────────┐
    │   the user   │
    └──────┬───────┘
           │ "what time is it?"
           ▼
    ┌──────────────┐       ┌─────────────────┐
    │    HARNESS   │──────▶│   the LLM       │
    │              │◀──────│  (the "brain")  │
    └──────┬───────┘       └─────────────────┘
           │  "call the tool `get_time`"
           ▼
    ┌──────────────┐
    │    TOOLS     │   get_time, read_file,
    │              │   run_command, ...
    └──────┬───────┘
           │ "the time is 10:41"
           ▼
      (back to the LLM, which now
       answers the user)
```

The loop is:

1. The harness sends the LLM the user's question **plus** a list of **tools** —
   named actions the LLM is allowed to ask for.
2. If the LLM replies with ordinary text, the turn is over.
3. If the LLM replies asking for a tool call, the harness runs that tool, then
   sends the *result* back to the LLM and goes around again.
4. This repeats until the LLM answers with text.

**Hānihi is such a harness.** The word is a loan from English into Māori and
means "harness". It is written in **Rust**, a programming language. It talks to
LLMs over an **OpenAI-compatible chat-completions** interface.

Two more terms you will need:

- **MCP** — the *Model Context Protocol*. A standard way for a harness to attach
  extra tools that live in a *separate process*. The harness launches a small
  program called an **MCP server**, asks it "what tools do you offer?", and
  forwards calls to it.
- **A session** — one saved conversation. Hānihi writes everything that happens
  in a session to a file so the conversation can be picked up again later.

Now the history can begin.

---

## Chapter 1 — The scaffold (10–11 August 2026)

**First commit:** `da02c82`, dated 2026-08-10.

A **scaffold** is a bare skeleton — the shape of the program with most of the
furniture missing. This first commit set out the four building blocks Hānihi
would be assembled from, and they have not changed since:

| Library | What it does |
|---|---|
| **tokio** | Lets the program do several things "at once" (asynchronous programming) |
| **rig** (`rig-core`) | Talks to the LLM; provides the tool-calling loop |
| **rmcp** | Implements MCP, so extra tools can be attached |
| **reedline** | Draws the interactive prompt you type into (the read-eval-print loop, or **REPL**) |

The very next day, 11 August 2026, the project was **renamed twice**.

1. `3c68319 Rename project to kākahu` — the crates `agent-core` and `agent-cli`
   became `kakahu-core` and `kakahu-cli`.
2. `eb36e92 Use macron everywhere acceptable` — adding the macron (the little
   bar over a vowel, as in *kākahu*).
3. `5a208d2 Rename kākahu → hānihi` — the final name.

> **A practical wrinkle worth knowing.** A **crate** is Rust's word for a package
> or library. Commit `a2a687c` records a real-world constraint: *crates.io* (the
> public Rust package registry) only accepts ASCII names. So the package names
> are ASCII — `hanihi-core`, `hanihi-cli` — while the project keeps its macron in
> the README and the program's display name.

**Why this counts as a milestone:** after these two days the project had a name,
a language, a set of four dependencies, and the skeleton of a working agent.

---

## Chapter 2 — Memory: sessions and the event log (11–12 August 2026)

### The problem

The scaffold could hold a conversation, but only for as long as the program was
running. Close it, and everything was gone. Worse, there was no record of what
had happened.

Plan 001 (`plans/001-sessions.md`, created 2026-08-11) fixed this with one
idea: **every interaction happens inside a session, and every session is an
append-only log on disk.**

### What "append-only" means

An **append-only** file is one you may only add to. You never edit or delete
existing lines. This is a very common design for logs:

- Editing a file in place risks corrupting it if the program crashes halfway
  through a write. Appending a whole line is much safer.
- Because nothing is erased, the file is a complete record.
- Two readers can read it at the same time without interfering.

The format chosen was **JSON Lines**, often written `.jsonl`. In a JSON Lines
file, each line is a complete piece of **JSON** — a standard, human-readable text
format for structured data. Each line is independent: you can parse line 500
without having read lines 1 to 499. In a single giant JSON document, one missing
bracket breaks the whole file.

### What a session looks like on disk

```
working/
└── sessions/
    ├── default-session/
    │   ├── session.json     ← fixed facts: id, name, created, model, prompt
    │   ├── events.jsonl     ← the append-only log of everything that happened
    │   ├── history.txt      ← what you typed at the REPL (up-arrow recall)
    │   └── .lock            ← a claim, so two programs can't share one session
    └── my-chat/
        └── ...
```

**`session.json` never changes.** It records only facts true from creation: a
UUID (a long random string that is practically guaranteed to be unique), the
name, the creation time, the model, and the system prompt. Anything that changes
over time is *derived* from the log instead. There is exactly one source of
truth, and it is append-only.

**The `.lock` file prevents a subtle bug.** Imagine opening the same session in
two terminals. Both programs would append to the same file, interleaving their
lines, and the resulting log would describe a conversation that never happened.
A **lock file** says "I am using this"; a second program refuses to start. This
is the same idea as a bathroom door lock, and like a bathroom door lock it fails
safe: you cannot get in, rather than getting in and causing a mess.

**The log records reasoning the program otherwise discards.** When the model
responds, it may include *reasoning* content — text it "thought" before answering.
The agent loop threw that away. Plan 001 captured it in the log anyway, on the
grounds that the log's job is to record everything received from the model.

### The event kinds

Each line of `events.jsonl` is one **event**, with a timestamp, a turn number, a
`kind`, and a `data` payload:

| `kind` | When it is written |
|---|---|
| `session_created` | the session folder is first made |
| `session_opened` | the session is opened |
| `session_closed` | the program exits cleanly |
| `user_input` | you press Enter |
| `llm_prompt` | just before a request is sent to the model |
| `llm_response` | just after the model replies |
| `tool_execution` | after a tool has run |
| `turn_complete` | the turn finished with a final answer |
| `error` | something failed |

Notice that a single turn can contain **many** `llm_prompt` / `llm_response`
pairs. That is the agent loop from Chapter 0.

### Derived properties

Some useful numbers are *not* stored, because they can be calculated:

- **Total tokens used** — add up the counts from every `llm_response`.
- **How long each model call took** — subtract the `llm_prompt` timestamp from
  the matching `llm_response` timestamp.
- **Turn count** — read the number off the last `turn_complete`.

Storing derived data is a classic source of bugs: the stored copy and the real
data drift apart. Computing it on demand cannot drift.

> **Two more tools arrive.** Commit `7d3479a` (11 August) added `read_file` and
> `list_dir`, filtered by the repository's ignore rules so that build output
> never reached the model. Commit `a537940` wired the session into the REPL,
> adding `--working-dir`, `--session`, and `--new-session`.

**Why this counts as a milestone:** the program gained a memory, and — just as
importantly — an *audit trail*. Almost every later improvement in this history
was diagnosed by reading `events.jsonl`.

---

## Chapter 3 — Streaming and durable execution (12 August 2026)

Version 0.3.0 (`5cc9c93`) bundled two features that share a root cause: both are
about *time*.

### Streaming: showing work as it happens

LLMs generate their answer one piece at a time, called a **token** (roughly a
word-fragment). "Streaming" means each token is displayed as soon as it exists.

Plan 004 (`plans/004-streaming.md`) introduced this. Before it, you stared at a
blank screen until the entire answer was ready. After it, text appears to type
itself out:

```
   ┌──────────────────────┐
   │  agent task (spawned)│  ← runs the loop, produces events
   └───────────┬──────────┘
               │  sends into a channel
               ▼
   ┌──────────────────────┐
   │  mpsc channel        │  ← a one-way pipe between tasks
   └───────────┬──────────┘
               │
               ▼
   ┌──────────────────────┐
   │  the REPL / display  │  ← reads events, prints them
   └──────────────────────┘
```

An **`mpsc` channel** is "multi-producer, single-consumer": many senders, one
receiver. It is how one part of a program hands data to another part running at
the same time, without them tripping over each other. The agent loop runs on a
**spawned task** — a unit of work the runtime schedules independently.

### Durable execution: surviving a restart

Plan 003 (`plans/003-durable-execution.md`) made sessions survive being closed.

The mechanism is called **replay**. Hānihi reads the event log and reconstructs
the conversation:

1. Each `user_input` becomes a user message.
2. Each `llm_response` becomes an assistant message, with tool calls attached if
   it made any.
3. Each `tool_execution` becomes a tool result, linked back by a shared ID.
4. `turn_complete` and `error` mark the end of a complete turn.

The subtle part is **an interrupted turn**. If the program is killed midway, the
log ends with the model having asked for a tool that never ran. Replaying that
literally would produce an incomplete exchange that confuses the API. So replay
*truncates back to the last completed turn*, discarding the partial one. It is
the same instinct as a database transaction: an unfinished operation is rolled
back rather than half-applied.

Plan 003 also added a `call_id` to tool executions, needed to rebuild the link
correctly, with a fallback for old logs — an early example of a theme that
recurs constantly: **the format changes, but old files must keep working.**

**Why this counts as a milestone:** Hānihi stopped being a single sitting.
Streaming made it feel alive while you watched; durable execution made it persist
while you were not watching.

---

## Chapter 4 — It learns to change its own code (19 August 2026)

Plan 005 (`plans/005-self-improvement.md`) gave the agent the ability to compile,
change, and run code — and then turned those abilities on Hānihi itself.

- `c314a31` — **`run_command`**. Runs a command and reports what happened.
- `1fffcd4` — **`apply_patch`** and **`write_file`** behind a `--write` flag,
  plus **`grep`** and **`read_session_log`**.
- `faae0ec` — eval harness extensions and `scripts/self-improve.sh`.

### The safety design, clause by clause

**1. Read-only by default; writes gated.** The write tools are *not registered at
all* unless you pass `--write`. This is stronger than refusing them: a tool that
was never offered cannot be called, and cannot even be *hallucinated*, because
the model has no way to name something it has never seen. The reasoning is about
which way mistakes fall — a wrong read costs nothing; a wrong write can destroy
work permanently.

**2. The gate turns changes into reviewable artefacts.** Writes are not blocked,
they are *gated*, so every change becomes something you can look at and undo.
The danger with an autonomous coding assistant is not bad code, it is bad code
*at scale*; gating converts a catastrophe into a pile of small diffs.

**3. Every change is a Git commit.** Git serves as memory, undo button, and audit
trail. Commits are local only — **never pushed** — limiting damage to this machine
until a human looks.

**4. Improvement is measured, never self-reported.** An LLM is fluent and
confident. "I have improved the code" is not evidence. So the agent never
declares success; an external referee — the **eval harness** — decides.

An **eval** is a test that exercises the *whole agent*. Plan 002 built this:

```toml
user_input = "What time is it right now? Use the get_time tool to find out, then tell me."

[[assertions]]
type = "tool_called"
name = "get_time"

[[assertions]]
type = "no_error"
```

The runner sends the input to a real model, lets the agent work, then reads the
resulting session log and checks the assertions. "Did it call the tool?" is
answered by the log, not by trusting the agent's summary. This is why Chapter 2
matters so much downstream: the event log became the evidence base.

> **A candid note.** Plan 005 admits a flaw in its own logic: the eval cases live
> inside the repository, so an agent with write access could weaken its own tests.
> Recording a limitation *in the plan* rather than discovering it later is good
> practice.

> **Naming matters.** `e953502` renamed `--self-improve` to `--write`: the flag
> names a *capability*, not a *use case*. Self-improvement is one application of
> the capability, not its definition.

**Why this counts as a milestone:** Hānihi went from talking about code to
changing it, and gained an external quality signal. The subject of the work and
the tool doing the work became the same thing — which is why the sessions in
`working/` from this date onward are Hānihi working on Hānihi.

---

## Chapter 5 — When the tools fought back (late August – early September 2026)

Not every commit is a feature. A large amount of effort went into the agent's
*tools* rather than its capabilities, and the reason is instructive.

### The patch applier, and why it kept failing

`apply_patch` changes source code. You describe the change as a **diff** — lines
removed (marked `-`) and added (marked `+`), with surrounding context so the
location can be found.

The first implementation called Git's own patch program:

```
git apply --3way --recount
```

It did not work well. `reports/001_apply_patch.md` documents three groups of
failures.

**Failure A — the 3-way merge could not start.** `--3way` asks Git to merge the
patch against a saved version of the file. Hand-written patches carry made-up or
out-of-date version markers, and the versions they name did not exist. Git
refused: *"repository lacks the necessary blob to perform 3-way merge"*.

**Failure B — patches without trailing context were rejected.**

```
@@ -1 +1,2 @@
 fn main() {}
+// patched
```

There is context *before* the insertion point, but none *after*. If the file has
any other local modification, Git cannot decide where the patch belongs and
refuses. The report records that the identical patch applies cleanly once a
trailing context line is added.

**Failure C — malformed input.** A diff with no file headers produced *"No valid
patches in input"*, surfaced as a terse, unhelpful message.

### The fix: stop calling Git

`30a6952 Replace-git-apply-with-pure-Rust-patch-applier` rewrote the tool to
apply diffs itself, in Rust:

1. Parse the diff into per-file pieces.
2. Check each target path for safety (no escapes, ignored, or protected files).
3. For each hunk, find its *old* lines in the file's **current** contents — the
   version the agent has actually been reading — and swap in the *new* lines.
4. Apply **atomically**: match everything in memory first, and write only if
   *every* hunk matched. Either the whole patch lands or nothing does.
5. Reproduce `--recount` by counting the actual lines.

The decisive change is step 3: anchoring on a single leading context line makes
the previously impossible case succeed, leaving the user's own local edit intact.

> **Atomicity is a general principle.** "All or nothing" is the same guarantee
> databases give with transactions: a half-applied change can never exist, so you
> never have to reason about what a partial failure left behind.

Result: `cargo test -p hanihi-core` went to **60 passed, 0 failed**.

### Deduplication, allowlists, housekeeping

**`fb695c9` — deduplicate identical read-only tool calls.** Reading the same file
three times in one turn cannot give a different answer, so the first result is
reused and third-and-later identical calls are refused with *"duplicate call
skipped"*. This saves time and tokens. It also caused a mistake later — see
Chapter 8.

**`7eb185a` — expand the `run_command` allowlist.** An **allowlist** is a list of
explicitly permitted things; anything else is refused. It is the opposite of a
blocklist. Allowlists are safer: you only lose if you forget to add something,
whereas a blocklist loses if you forget to ban something. `run_command` gained
more read-only Git verbs here.

**`f9f5c78` — grant write-mode Git housekeeping.** With `--write` active,
`run_command` also accepts `git add`, `git restore --staged`, `git rm --cached`,
and `git commit --amend` — the verbs needed to tidy up a commit you just made.
Without `--write`, `run_command` behaves exactly as before.

### The session inspector

`206fdcb` added `hanihi-session`, which prints a session summary **without needing
an API key or a model**. `c30ffae` added `analyse`, which prints the event log's
structure. Once you have 65 saved sessions, inspecting them without spending money
on model calls is what makes the log useful rather than merely present. `analyse`
grew steadily: `--prompts` (`13791a2`), token usage (`1b6a7b5`), `--transcripts`
(`2b97545`), `-l`/`--last` (`f329a38`).

> **Two design details.** `Display` for a `LogEntry` was implemented (`d6426af`)
> so entries print readably. And `d48416e` is titled *"Hack the `fmt::Display` for
> `LLMPrompt` to not output all messages"* — a pragmatic exception, because
> printing an entire conversation inside one line is unusable. Sometimes the
> honest commit message is the one that admits it is a hack.

**Why this counts as a milestone:** the project learned that *tools are the hard
part*. Writing a feature is easy compared with making a tool behave predictably
when a real model hands it imperfect input.

---

## Chapter 6 — Moving tools out: the MCP split (11–13 September 2026)

`a80d61c Start the process of moving tools all to MCP server` and `2bd34c4 Remove
builtin_read_file tool. Moved to MCP` began moving tools out of the main program.

Reasons: **separation of concerns** (the main program does the agent loop),
**independent restart** (if a file-reading server crashes, the conversation
survives), and **a natural security boundary**.

`mcp-echo-server` was renamed to `hanihi-mcp-server` (`e645a15`), and by `19361b2`
there were **two** servers — and the split is the interesting part:

| Server | Tools | Risk |
|---|---|---|
| `hanihi-mcp-server-ro` | read-only: `read_file`, `search_text`, `find_symbol`, ... | cannot change anything |
| `hanihi-mcp-server-rw` | read-write: `write_file`, `apply_patch`, `git_add`, `git_commit`, ... | can change the repository |

`ro` means read-only; `rw` means read-write. Attaching only the read-only server
gives a guarantee at the level of *which programs are running*, which is stronger
than passing a flag the program promises to respect.

### Two fixes worth understanding

Plan 011 records a naming decision: the MCP file-reading tool is called
**`mcp_read_file`**, not `read_file`, because the main program also registers a
built-in `read_file`. Two tools with the same name would collide.

Plan 015 fixed a genuine security defect. The file tools resolved their path
argument by naively joining it onto the root. `PathBuf::join` *replaces* the base
when given an absolute path, so `path: "/etc"` searched `/etc` instead of the
repository. `..` was not filtered, and a symbolic link could point outside. The
fix canonicalises both paths and requires the result to be inside the workspace.

> **Two more lessons.** Plan 015 reclassified certain errors from "internal error"
> to "invalid params". A *caller mistake* (you asked for a file that does not
> exist) is not a *server fault* (the disk failed); conflating them misleads
> whoever is debugging. And the plan argues that rejecting *all* `..` — even
> harmless ones like `crates/../crates` — is worth the strictness, because a
> simple rule is easier to reason about than a clever one.

---

## Chapter 7 — Running out of room: tokens and compaction (20–21 September 2026)

### The problem: a fixed-size window

An LLM does not remember your conversation. Every request must resend the *entire*
conversation so far. Models have a **context window** — a maximum number of tokens
per request. Exceed it and the request fails outright. Plan 012 records the
failure: **12.2 million tokens requested against a limit of 1 million**, a 400
error every time.

### The fix, in parts

Plan 012 (`plans/012-token-budgeting.md`, commit `64aecf7`) added layered
defences.

**1. Measure before sending.** `tiktoken-rs` counts tokens using the same
algorithm OpenAI's models use, replacing a crude "characters divided by four"
rule, with a fallback if the counter cannot be built.

**2. Compact when the budget is exceeded.** *Compaction* summarises the older
part of the conversation and replaces it with the summary:

```
BEFORE:
  [turn 1][turn 2][turn 3][turn 4]...[turn 20][turn 21][turn 22]
   └──────────── summarise these ────────┘     └── keep these ──┘

AFTER:
  [summary of turns 1-18][turn 19]...[turn 22]
   └── injected into the system prompt ──┘
```

Reserve 16,384 tokens for the reply, keep the most recent 20,000 tokens verbatim,
cap the summary at 8,000 tokens.

**3. Cut only at safe points.** Cuts happen at *turn boundaries* only, because
the API requires an assistant message requesting tool calls to be immediately
followed by those calls' results. This constraint reappears in Chapter 8, where
violating it caused a real bug.

**4. Cap tool outputs** at 64 KiB, with the full output still written to disk.

**5. Bound file reads.** `read_file` had loaded the *whole file* into memory
before truncating. A multi-gigabyte file would exhaust memory before the cap
applied; the fix reads at most one byte more than the cap.

**6. Limit tool calls per turn.** `MAX_TOOL_CALLS_PER_TURN` was set to 100, later
raised (`b6b5cd3 Make 1_000 tool calls per turn available`).

### Making compaction visible

Compaction is *lossy* — information is genuinely thrown away. That makes it
essential to record when it happens. Plans 013 and 014 added a `compaction` event
recording the token count before and after, the summary text, messages dropped
and kept, and the summary call's own usage. This required **bumping the schema
version from 1 to 2**.

The log had gained a `schema` field back in plan 008 precisely so old and new
formats could be told apart. The policy there is good practice:

- **Additive** changes (a new optional field) do not bump the version.
- **Breaking** changes (rename, remove, restructure) bump it and add a migration.

Plan 013 also caught a related bug: the command-line handlers restored the
conversation history after each turn but *not* the rolling summary, so in a
long-running session the summary was silently discarded every turn. The general
lesson: **persisting one half of a pair of values is a bug waiting to happen.**

---
