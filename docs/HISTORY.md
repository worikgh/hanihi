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

The pattern works like this:

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
   named actions the LLM is allowed to ask for, each with a description and a
   description of what arguments it takes.
2. If the LLM replies with ordinary text, the turn is over.
3. If the LLM replies asking for a tool call, the harness runs that tool, then
   sends the *result* back to the LLM and goes around again.
4. This repeats until the LLM answers with text.

**Hānihi is such a harness.** The word is a loan from English into Māori and
means "harness". It is written in **Rust**, a programming language. It talks to
LLMs over an **OpenAI-compatible chat-completions** interface, which is a widely
used convention for sending prompts to a model over the network.

Two more terms you will need:

- **MCP** — the *Model Context Protocol*. A standard way for a harness to attach
  extra tools that live in a *separate process*. The harness launches a small
  program called an **MCP server**, asks it "what tools do you offer?", and
  forwards calls to it. This keeps optional tools out of the main program.
- **A session** — one saved conversation. Hānihi writes everything that happens
  in a session to a file so the conversation can be picked up again later.

Now the history can begin.

---

## Chapter 1 — The scaffold (10–11 August 2026)

**First commit:** `da02c82 agent-harness: tokio + rig + rmcp + reedline agent
scaffold`, dated 2026-08-10.

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
> the README and the program's display name. That is why you will see both
> spellings throughout this document. They refer to the same thing.

By the end of 11 August, the README had grown a name section, a link to the
repository, and a table of configuration options.

**Why this counts as a milestone:** after these two days the project had a name,
a language, a set of four dependencies, and the skeleton of a working agent. The
rest of the history is filling that skeleton in.

---

## Chapter 2 — Memory: sessions and the event log (11–12 August 2026)

### The problem

The scaffold could hold a conversation, but only for as long as the program was
running. Close it, and everything was gone. Worse, there was no record of what
had happened — you could not look back and ask "what did it actually do?"

Plan 001 (`plans/001-sessions.md`, created 2026-08-11) fixed this with one
idea: **every interaction happens inside a session, and every session is an
append-only log on disk.**

### What "append-only" means

An **append-only** file is one you may only add to. You never edit or delete
existing lines. This is a very common design for logs, and it has properties
worth understanding:

- Editing a file in place risks corrupting it if the program crashes halfway
  through a write. Appending a whole line is much safer.
- Because nothing is erased, the file is a complete record. You can always
  reconstruct what happened by reading from the start.
- Two readers can read it at the same time without interfering.

The format chosen was **JSON Lines**, often written `.jsonl`. In a JSON Lines
file, each line is a complete piece of **JSON** — a standard, human-readable text
format for structured data. Each line is independent: you can parse line 500
without having read lines 1 to 499. Compare that with a single giant JSON
document, where one missing bracket breaks the whole file.

### What a session looks like on disk

Sessions live under a **working directory**, which defaults to `./working`. Each
session gets its own folder:

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

Three details here are worth pausing on.

**`session.json` never changes.** It records only facts that are true from
creation: the session's unique ID (a UUID — a long random string that is
practically guaranteed to be unique), its name, when it was created, which model
it uses, and its system prompt. Anything that changes over time is *derived* from
the log instead. This is a deliberate simplification: there is exactly one source
of truth, and it is append-only.

**The `.lock` file prevents a subtle bug.** Imagine opening the same session in
two terminals. Both programs would append to the same file, interleaving their
lines, and the resulting log would describe a conversation that never happened.
A **lock file** is a small file a program creates to say "I am using this". A
second program sees it and refuses to start. This is the same idea as a bathroom
door lock, and like a bathroom door lock it fails safe: you cannot get in, rather
than getting in and causing a mess.

**The log records reasoning the program otherwise discards.** When the model
responds, it may include *reasoning* content — text it "thought" before answering.
The agent loop threw that away. Plan 001 made a point of capturing it in the log
anyway, on the grounds that the log's job is to record everything received from
the model, whether or not the program uses it.

### The event kinds

Each line of `events.jsonl` is one **event**. Every event has a timestamp, a turn
number, a `kind` (what sort of event it is), and a `data` payload. The original
set of kinds describes a turn from start to finish:

| `kind` | When it is written |
|---|---|
| `session_created` | the session folder is first made |
| `session_opened` | the session is opened (including right after creation) |
| `session_closed` | the program exits cleanly |
| `user_input` | you press Enter |
| `llm_prompt` | just before a request is sent to the model |
| `llm_response` | just after the model replies |
| `tool_execution` | after a tool has run |
| `turn_complete` | the turn finished with a final answer |
| `error` | something failed |

Notice that a single turn can contain **many** `llm_prompt` / `llm_response`
pairs. That is the agent loop from Chapter 0: ask the model, run a tool, ask
again, run another tool, ask again — until it finally answers in words.

### Derived properties

Some useful numbers are *not* stored at all, because they can be calculated from
the log:

- **Total tokens used** — add up the token counts from every `llm_response`.
- **How long each model call took** — subtract the `llm_prompt` timestamp from
  the matching `llm_response` timestamp.
- **How many turns the session has had** — read the turn number off the last
  `turn_complete`.

Storing derived data is a classic source of bugs: the stored copy and the real
data drift apart. Computing it on demand cannot drift.

> **Two more tools arrive.** Commit `7d3479a` (11 August) added `read_file` and
> `list_dir`: the agent could at last *look* at the code it was supposed to be
> working on. These were filtered by the repository's ignore rules, so that build
> output and temporary files never reached the model. A separate commit,
> `a537940`, wired the session into the REPL, adding the `--working-dir`,
> `--session`, and `--new-session` command-line options.

**Why this counts as a milestone:** it is the moment the program gained a
memory, and — just as importantly — an *audit trail*. Almost every later
improvement in this history was diagnosed by reading `events.jsonl`. Without
Chapter 2, the rest of the story would have been guesswork.

---

## Chapter 3 — Streaming and durable execution (12 August 2026)

Version 0.3.0 (`5cc9c93 Bump to 0.3.0 — streaming, durable execution, eval
harness`) bundled two features that look unrelated but share a root cause: both
are about *time*.

### Streaming: showing work as it happens

LLMs do not produce their whole answer instantly. They generate it one piece at
a time, called a **token**. A token is roughly a word-fragment. "Streaming" means
each token is displayed as soon as it exists.

Plan 004 (`plans/004-streaming.md`) introduced this. Before it, you asked a
question and stared at a blank screen until the entire answer was ready —
possibly for a minute. After it, text appears to type itself out.

The technical shape is worth knowing, because it is a standard pattern:

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
**spawned task** — a unit of work the runtime schedules independently — so it can
keep generating while the display keeps printing.

The events it emits include `TextDelta` (a piece of text arrived),
`ToolCallStart` (the model wants to use a tool), `ToolCallReady` (the tool call is
complete and its arguments are known), `ToolResult`, and `TurnComplete`.

### Durable execution: surviving a restart

Plan 003 (`plans/003-durable-execution.md`) made sessions survive being closed.
Open a session tomorrow and the agent sees the whole prior conversation.

The mechanism is called **replay**. Hānihi reads the event log and reconstructs
the conversation it would have had:

1. Each `user_input` becomes a user message.
2. Each `llm_response` becomes an assistant message. If it contained tool calls,
   it becomes an assistant message *with* those tool calls attached.
3. Each `tool_execution` becomes a tool result, linked back to its tool call by a
   shared ID.
4. `turn_complete` and `error` mark the end of a complete turn.

The subtle part is what to do about **an interrupted turn**. Suppose the program
is killed midway through: the log ends with a user message, then a model reply,
then nothing. If you replayed all of that literally, the conversation would end
with the model having asked for a tool that never ran — an incomplete exchange
that confuses the API. So replay *truncates back to the last completed turn* and
discards the partial one. It is the same instinct as a database transaction: an
unfinished operation is rolled back rather than half-applied.

### Two small changes that matter

Plan 003 also added a `call_id` to tool executions. Tool calls carry two
identifiers, and the replay code needed the second one to rebuild the link
correctly. Old logs, written before the field existed, were handled with a
fallback — an early example of a theme that recurs constantly in this history:
**the format changes, but old files must keep working.**

Finally, `72abd8e` persisted the REPL's command history per session, so your
up-arrow recall is not lost between runs.

**Why this counts as a milestone:** before this, Hānihi was a single sitting. It
could hold a conversation, but it could not be *left* and come back to. Streaming
made it feel alive while you watched; durable execution made it persist while you
were not watching. Together they turned it from a toy into something you could
actually work with across days.

---

## Chapter 4 — It learns to change its own code (19 August 2026)

This is the largest single jump in the project, and the one that reframes
everything after it.

### The gap

Up to this point the agent could **read** code and **talk** about it. It could not
compile it, change it, or run it. Plan 005
(`plans/005-self-improvement.md`, created 2026-08-19) gave it those abilities and
then — crucially — turned them on itself: the agent would work on Hānihi's own
source code.

Three new capabilities arrived in three commits:

- `c314a31` — **`run_command`**. Runs a command and reports what happened.
- `1fffcd4` — **`apply_patch`** and **`write_file`**. Edits files, behind a
  `--write` flag. Also **`grep`** (search file contents) and
  **`read_session_log`** (read its own past conversations).
- `faae0ec` — the **eval harness** extensions and `scripts/self-improve.sh`, a
  script that drives the whole cycle from outside.

### The safety design, clause by clause

Giving a program the ability to rewrite programs is exactly as dangerous as it
sounds. Plan 005 laid down four principles, and they are the most interesting
part of the plan. Each one closes a specific way the idea can go wrong.

**1. Read-only by default; writes gated.**

The agent's normal state is observation. `read_file`, `list_dir`, `grep`, and
read-only commands are always available. The write tools are *not registered at
all* unless you pass `--write`.

This is stronger than what it might sound like. The tools are not merely
*refused* when `--write` is absent — they are never added to the list of tools
the model is shown. A tool that was never offered cannot be called. It cannot
even be *hallucinated*, because the model has no way to name something it has
never seen.

The reasoning is about which way mistakes fall. A wrong read costs nothing; you
just read again. A wrong write can destroy work permanently. So the default is
chosen so that the free mistakes are the ones you make.

**2. The gate turns changes into reviewable artefacts.**

Writes are not blocked, they are *gated*. The point is that every change becomes
something you can look at, undo, and attribute to a reason. Two layers:

- The `--write` switch (does this session have the capability at all?).
- Per-operation checks — for a patch, validate it first (`git apply --check`),
  then apply it, then optionally commit it.

The stated reasoning: the danger with an autonomous coding assistant is not bad
code, it is bad code *at scale*. Gating converts a potential catastrophe into a
pile of small diffs, each one reviewable.

**3. Every change is a Git commit.**

Git is used as three things at once: memory (the commit messages are the
agent's own notes), an undo button (`git revert` un-does any change), and an
audit trail (you can see exactly what changed and why). Commits are local only —
**never pushed** — which limits the damage to this one machine until a human
looks.

**4. Improvement is measured, never self-reported.**

This is the subtlest principle, and the most important. An LLM is very good at
confident, fluent prose. "I have improved the code" is not evidence of anything.
So the rule is that the agent never gets to declare success. Instead, an
external referee decides: the **eval harness**, which runs predefined tests and
reports pass or fail.

**What is an eval?** An eval is a test case that exercises the *whole agent*, not
just one function. Plan 002 (`plans/002-evals.md`, 12 August) had already built
this. A case lives in a directory with a `case.toml` file:

```toml
user_input = "What time is it right now? Use the get_time tool to find out, then tell me."

[[assertions]]
type = "tool_called"
name = "get_time"

[[assertions]]
type = "no_error"
```

The runner sends the `user_input` to a real model, lets the agent work, then —
this is the key step — reads the resulting session log and checks whether the
assertions actually held. "Did it call the tool?" is answered by looking at the
log, not by trusting the agent's summary of what it did.

This is why Chapter 2 matters so much downstream. The event log became the
evidence base for judging whether the agent was behaving.

> **A candid note on the plan's own design.** Plan 005 includes a *corollary*
> admitting a flaw in its own logic: the eval cases live inside the repository,
> so an agent with write access could weaken its own tests. The plan says this
> must be addressed. Recording that limitation *in the plan* rather than
> discovering it later is good practice.

> **Naming matters.** Commit `e953502` renamed the flag from `--self-improve` to
> `--write`. The reasoning given is precise: the flag names a *capability*
> (you may edit this repository), not a *use case* (you are improving yourself).
> Self-improvement is one application of the capability, not its definition. This
> is a small commit with a real lesson in how to name things.

**Why this counts as a milestone:** Hānihi went from a program that *talks about*
code to a program that *changes* code, and from a program with no quality signal
to a program with an external one. It also marks the point where the subject of
the work and the tool doing the work became the same thing — which is why the
sessions in `working/` from this date onward are Hānihi working on Hānihi.

---
