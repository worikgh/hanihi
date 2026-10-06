# hānihi

**A lot of the documents and code herein are written using AI and other algorithmic systems**


A tool-calling agent harness in Rust. Built on:

- **rig** (`rig-core` 0.41) — OpenAI-compatible chat completions client, tool
  definitions, completion loop
- **rmcp** (3.1) — Model Context Protocol client: attach tools from MCP stdio
  servers
- **reedline** (0.49) — readline-style REPL

Repository: <https://github.com/worikgh/hanihi>

## Name

**`hānihi`** is a loan word from English into Māori and means ["harness"](https://maori_en_new.en-academic.com/2763/h%C4%81nihi)

## Layout

```
crates/
├── hanihi-core/        # library: Agent (loop, history, dispatch), built-in
│                       #   tools, source-tree access, MCP client, sessions
├── hanihi-cli/         # binary: clap CLI + reedline REPL + --once mode
├── hanihi-eval/        # binary: eval runner — run test cases against a live
│                       #   LLM and check assertions against the session log
├── hanihi-mcp-server/  # binaries: MCP stdio servers (ro + rw)
└── hanihi-session/     # binary: read-only session inspection (no model
                        #   needed); also an `analyse` sub-binary
evals/
└── cases/              # eval test cases (case.toml + README per case)

prompts/                # reusable system prompts (rust_coding, self-improvement)
plans/                  # design notes, numbered
reports/                # write-ups of applied changes
scripts/                # self-improve.sh — minimal external self-improvement loop
```

> Crate *package* names are ASCII (`hanihi-core`, `hanihi-cli`, `hanihi-eval`,
> `hanihi-mcp-server`, `hanihi-session`) because crates.io only accepts ASCII
> names. The project name keeps the macron (`hānihi`), as does the CLI's
> display name. The lib target is `hanihi_core` (rustc requires ASCII
> identifiers for `--extern`).

> Workspace version is **0.3.0** (the MCP server crate is versioned separately
> at 0.1.0).

## Features

- **Agent loop** — system preamble + persistent history + tool definitions,
  tool-call dispatch, `max_turns` guard (default 29), per-turn usage tracking
- **Streaming output** — model text arrives token-by-token in both `--once`
  and REPL modes. Tool calls show as `🔧 tool_name … ✅` while the model is
  still generating. Under the hood: `tokio::spawn` + `mpsc` channel — the
  agent loop runs concurrently, the caller reads `StreamEvent`s as they
  arrive.
- **Durable sessions** — every turn is logged to an append-only JSONL event
  log (`events.jsonl`) under `<working-dir>/sessions/<name>/`. Reopen a
  session and the agent replays all prior turns to pick up where it left off.
  Filesystem-locked for safety. Cumulative token usage and per-call latencies
  are computable from the log.
- **Source tree access** — the agent can read and list the enclosing git
  repository (found by walking up from the cwd). Everything is filtered by
  the repo's ignore rules via the `ignore` crate: `.gitignore` and
  `.git/info/exclude` are respected and never written; hānihi maintains its
  own `.ignore` file (same syntax, git-agnostic) at the repo root with
  generated-artifact templates for the languages it detects (`Cargo.toml` →
  Rust; CMake/Makefile/C-family sources → C/C++). Reads are capped at
  64 KiB, escapes outside the repo are refused, and `target/`-style noise
  never reaches the model.
- **Code tools** — `grep` (regex content search, ripgrep syntax, capped at
  200 matches), `run_command` (allowlisted `cargo`/`git` commands inside the
  repo root: no shell, scrubbed env, timeout, full output persisted to a
  trace file under `working/traces/`), and `read_session_log` (a window
  into the session's own `events.jsonl` — the agent can study its own
  traces). All always registered.
- **Write tools (opt-in, `--write`)** — `apply_patch` (unified diff,
  validated with `git apply --check`) and `write_file`. Scoped to the
  enclosing repo (escapes, ignored paths, `.ignore`, `.git*` refused);
  changes land as local git commits — never pushed. Off by default: without
  `--write` the tools are not even registered.
- **MCP client** — spawn an MCP stdio server, wrap each of its tools as an
  agent tool dispatching over `tools/call`
    * Two MCP servers: One for read only tools (`hānihi-mcp-server-ro` and one for read/write tools `hānihi-mcp-server-rw`
- **CLI** — interactive reedline REPL (`/help /tools /clear /session /file
  /quit`), `--once` one-shot mode for scripting and smoke tests, repeatable
  `--mcp-command`, session management (`--session` / `--new-session`)
  * System-prompt control: `--prompt TEXT` and `--prompt-file PATH`
    (both repeatable; appended to the prompt and persisted to the session on
    resume), and `--new-prompt` to replace the stored prompt outright.
- **Eval harness** (`hanihi-eval`) — run TOML-based test cases against a
  live LLM, assert tool calls / text content / error-free completion /
  latency / token budgets against the session log. Separate from
  `cargo test` because it needs API keys.
  * Toolchain gates (`build_succeeds`, `tests_pass`, `lint_clean`) run
    per-case commands, so a C++ (CMake) project is verified as readily as a
    Cargo one. See "Eval runner" below.
  * Also `--list`, `--case NAME`, `--keep-sessions`, and `--timeout SECS`.
  * MCP servers are **not** yet supported here: passing `--mcp-command`
    fails with "MCP support in eval runner not yet implemented".

> **Tool inventory.** Built-in tools are `get_time`, `list_dir`, `grep`,
> `run_command`, and `read_session_log` (always registered), plus
> `write_file` and `apply_patch` under `--write`. There is no built-in
> `echo`; `echo` exists only as an MCP tool served by
> `hānihi-mcp-server-ro`.

## Run

```bash
# Create a named session and have a conversation
cargo run  -p hanihi-cli -- --mcp-command hānihi-mcp-server-ro  -- --new-session my-chat

# Later, resume:
cargo run -p hanihi-cli -- --session my-chat

# Create a named session and have a conversation and edit code
cargo run -- --mcp-command hānihi-mcp-server-ro --mcp-command hānihi-mcp-server-rw   -p hanihi-cli --new-session my-chat

# Run the eval suite against DeepSeek
LLM_API_KEY=*** cargo run -p hanihi-eval -- --cases-dir ./evals/cases

# REPL commands: /help /tools /clear /session /file /quit (or /exit)
```

Configuration — every flag has an environment variable:

| Flag                 | Env                  | Default                                            |
|----------------------|----------------------|----------------------------------------------------|
| `--base-url`         | `LLM_BASE_URL`       | `https://api.deepseek.com/v1`                      |
| `--api-key`          | `LLM_API_KEY`        | — (required)                                       |
| `--model`            | `LLM_MODEL`          | `deepseek-chat`                                    |
| `--session NAME`     | —                    | `default-session`                                  |
| `--new-session NAME` | —                    | none (auto-creates `default-session` on first run) |
| `--working-dir DIR`  | `HANIHI_WORKING_DIR` | `./working`                                        |
| `--mcp-command CMD`  | —                    | none (repeatable)                                  |
| `--once PROMPT`      | —                    | none                                               |
| `--write`            | —                    | write tools NOT registered                         |
| `--task PROMPT`      | —                    | none (takes precedence over `--once`)              |
| `--max-turns N`      | —                    | 29                                                 |
| `--prompt TEXT`      | —                    | none (repeatable; appends to system prompt)        |
| `--prompt-file PATH` | —                    | none (repeatable; appends file contents)           |
| `--new-prompt`       | —                    | off (replace stored prompt with appends)           |

## REPL commands

At the interactive prompt, any line starting with `/` that matches one of the
commands below is handled by the harness; anything else is sent to the model
as a normal message.

| Command | Effect |
|---|---|
| `/help` | Print the list of available commands. |
| `/tools` | List every registered tool (name + description). |
| `/clear` | Clear the in-session message history (`history cleared`). |
| `/session` | Show session metadata: name, id, current turn, and `max_turns`. |
| `/file <PATH>` | Read the file at `<PATH>` (relative to the working dir, or absolute) and send its contents to the model as the prompt. Lets multi-line content — a prompt, a plan, or code — be loaded from disk in one go when only a single line can be typed. |
| `/quit` (or `/exit`) | Exit the REPL. |

`/session` also prints the last turn's footer (model, turn, tool calls, tokens
in/out, `max_turns`), or `No TurnSummary` if no turn has run yet.

`/file` requires a non-empty path; an empty or unreadable file prints an error and does not run a turn.

## How it works

### Agent loop

`Agent::run` is the synchronous loop:

1. Build a completion request: system preamble + persistent history + the new
   user input + tool definitions.
2. If the model replies with text only → turn complete, history committed.
3. If the model requests tool calls → record the assistant message, execute
   each tool (built-in or MCP), append results as `tool_result` messages,
   loop back to 1.
4. `max_turns` (default 29) guards runaway tool-call loops. A separate
   `MAX_TOOL_CALLS_PER_TURN` (100) bounds tool executions within one turn.

`Agent::run_streaming` does the same but yields events through a
`tokio::sync::mpsc` channel: text arrives token-by-token, tool calls are
announced as they start and complete, and results are reported as they
execute. The agent loop runs on a spawned task so the caller can read events
in real time.

#### Context compaction

Before every completion request the agent estimates the input size and, if it
exceeds the model's context budget (`context_limit_for(model)`, minus a
reserve for output tokens), summarizes the older portion of the history into
a rolling summary and drops those messages. The summary is injected into the
effective preamble under a `## Summary of the conversation so far:` heading,
and is cumulative across turns. Non-streaming runs log a `compaction` entry
immediately before the `llm_prompt` it produced; streaming runs emit
`StreamEvent::Compaction`.

If there is no old history to summarize (a single dominating recent turn),
compaction is skipped and tool-output truncation is the backstop instead.

#### Read-only tool cache

Within a single turn, repeated identical calls to a read-only tool
(`read_file`, `list_dir`, `grep`, `read_session_log`, `echo`) reuse the first
result instead of re-executing; the third and later identical calls are
refused with a "duplicate call skipped" note. Any successful `apply_patch` or
`write_file` invalidates the cache, so writes are never presented with stale
reads. The cache is cleared at the start of each turn.

### Sessions

`SessionManager` owns a working directory (`./working` by default). Each
session is a subdirectory under `working/sessions/<name>/`:

```
session.json    — static metadata (id, name, created_at, model, system_prompt)
events.jsonl    — append-only JSONL log: user_input, llm_prompt, llm_response,
                  tool_execution, turn_complete, compaction, error, lifecycle
                  events
history.txt     — reedline line-editing history (REPL only)
.lock           — filesystem lock (one process per session)
```

On open, `replay_history()` scans the log and reconstructs the agent's
message history from completed turns. Partial turns (log ends without
`turn_complete`) are dropped. Streaming sessions that lack `llm_response`
entries get synthetic assistant messages inserted during replay. `compaction`
and `llm_prompt` entries are skipped — they are not needed to rebuild history.
Tool calls and their results are paired by `tool_call_id`, not by log
position, so streaming and non-streaming logs replay identically.

### Eval runner

Each case is a directory under `evals/cases/` containing a `case.toml`:

```toml
user_input = "What time is it right now? Use the get_time tool to find out, then tell me."

[[assertions]]
type = "tool_called"
name = "get_time"

[[assertions]]
type = "text_regex"
pattern = "20\\d{2}"

[[assertions]]
type = "no_error"
```

(That is `evals/cases/002-get-time/case.toml` verbatim. `001-basic-echo`
asserts on the MCP `echo` tool, so it requires an attached MCP server — which
the eval runner does not yet support.)

The runner creates a temp session, runs the prompt against a live LLM, then
checks each assertion against the `events.jsonl` log. Assertion types:
`tool_called`, `tool_not_called`, `text_contains`, `text_not_contains`,
`text_regex`, `no_error`, `max_turns`, `latency_ms`, `token_budget`,
`build_succeeds`, `tests_pass`, `lint_clean`, `no_diff`,
`no_unsupported_capability_claim`, `reported_tool_errors_are_real`.

The last four run commands in the case's repo and need a `repo` (or
`fixture`) field in `case.toml`. `build_succeeds`, `tests_pass`, and
`lint_clean` are toolchain-neutral names whose implementations come from
per-case command fields; `no_diff` is always `git status --porcelain`.

| assertion | default command | notes |
|---|---|---|
| `build_succeeds` | `cargo check` | Override with `build_command`. Runs `configure_command` first when present. |
| `tests_pass` | `cargo test` | Override with `test_command`. |
| `lint_clean` | `cargo clippy -- -D warnings` | Override with `lint_command`. **For a non-Cargo case there is no default**: `lint_clean` without a `lint_command` fails as a configuration error rather than passing silently. There is no canonical C++ equivalent of `clippy` (`clang-tidy` needs a `compile_commands.json`), so most C++ cases omit this assertion rather than fake it. |
| `no_diff` | `git status --porcelain` | Working tree must match HEAD. |

`clippy_clean` is still accepted as a deprecated alias for `lint_clean`
(implying the `cargo clippy` default), so existing cases keep working.

The last two assertion types in that list are the behavioural pair: they
compare the assistant's final answer against its own tool log, which no
substring or structural assertion can do.

| assertion | passes when | notes |
|---|---|---|
| `no_unsupported_capability_claim` | the answer makes no claim of a missing capability, **or** the log holds an `Error { stage: ToolExecution }` the claim can cite | Guards the regression where the agent said it had no write tools while the log recorded `apply_patch` as registered. |
| `reported_tool_errors_are_real` | the answer claims no tool failure, **or** the log holds at least one `Error { stage: ToolExecution }` | The mechanically decidable direction: a cited failure must exist in the log. |

Both predicates live in `crates/hanihi-eval/src/audit.rs` and are matched by
a small, explicit list of English phrasings over sentence-split text. Scope is
deliberately narrow: a differently-worded false claim passes. That
false-negative is accepted, because a broader matcher would fail honest
answers such as "I don't have the file contents yet" and destroy trust in the
suite. The inverse check — the agent reports a failure the log does not
support — is not implemented here.

`evals/cases/007-unsupported-capability-claim/` and
`evals/cases/008-missing-path-recovery/` are the seed cases for these two
predicates. Both need a live model, so neither is part of `cargo test`.

Tools are rig `PortableDynamicTool`s: name + description + JSON schema + an
async callback over raw `serde_json::Value`. MCP tools get wrapped into this
shape, dispatching over `tools/call` on the connected service.

Case fields: `user_input` (required), `assertions` (required), plus the
optional `model`, `system_prompt`, `source_tree`, `write_tools`, `repo`, and
`fixture`. `repo` is resolved relative to the case directory.

Four optional command fields parameterise the toolchain gates. Each is an
argv vector, not a shell string — no shell is involved, so no metacharacters
are interpreted — and paths must be relative to the resolved `repo`:

```toml
configure_command = ["cmake", "-B", "build"]
build_command = ["cmake", "--build", "build"]
test_command = ["ctest", "--test-dir", "build", "--output-on-failure"]
lint_command = ["clang-tidy", "-p", "build", "src/foo.cpp"]
```

Cases that name none of them behave exactly as before (cargo defaults).
`evals/cases/006-cpp-build/` is a worked C++ example. Gate commands bypass the
agent's command allowlist: they are authored by the case author, not chosen
by the model, so the eval verifies the artifact rather than the agent's
process.

The library is model-agnostic: `Agent<M: CompletionModel>` works with rig's
`MockCompletionModel` in tests and any OpenAI-compatible chat-completions
endpoint in production (see `connect_chat_model`).

## Status

- Unit and integration tests (rig `MockCompletionModel`, scripted turns,
  temp-repo fixtures, session replay — no network), including the
  `replay_streaming` integration test
- Smoke-tested against DeepSeek (`deepseek-chat`): `get_time` round trip ✔,
  MCP `mcp_echo` round trip ✔, streaming output ✔, session replay across
  restarts ✔, eval runner against live model ✔
- Workspace version 0.3.0

The eval runner's toolchain gates are covered by `cargo test -p hanihi-eval`
with no model in the loop: the assertion engine is exercised directly against
temp fixtures (Cargo and CMake). The C++ path needs `cmake` and a C++
compiler on `PATH`; those tests fail loudly, naming the missing tool, rather
than skipping silently.

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Eval runner (needs API key):
LLM_API_KEY=*** cargo run -p hanihi-eval -- --cases-dir ./evals/cases
LLM_API_KEY=*** cargo run -p hanihi-eval -- --list
LLM_API_KEY=*** cargo run -p hanihi-eval -- --case 001-basic-echo
```

`cargo clippy --workspace --all-targets -- -D warnings` is intended to be
clean, and is clean as of this writing. Treat any new warning as a regression
to fix rather than a baseline to tolerate.

## Known TODOs

- Tool name collisions: first registration wins (the built-in `get_time`
  shadows an MCP `get_time`). Namespacing MCP tools is a future concern.
- `add_ignore` tool / `--regenerate-ignore` — grow `.ignore` from within the
  agent.
- **LSP via MCP** — bridge an LSP server (goto-definition, references,
  hover) through the existing MCP client. Cheaper first step than
  tree-sitter for symbol-level code intelligence.
- **Tree-sitter symbol analysis** — `tree-sitter` + Rust grammar deps; a
  `symbols` module (definitions, signatures, kinds, line numbers); a
  `symbols(path)` tool or startup index under `working/`; reference-finding
  to support multi-file refactoring.
- **Multi-file refactoring** — agent emits a plan applied as one multi-file
  unified diff via `apply_patch`, verified by workflow gates + before/after
  evals.
- **Background workers** — in-process task layer (durable task state in the
  event log, `read_task` tool, file-change watcher). The `self-improve.sh`
  driver script is the minimal external version; do that first.
- **Eval compare mode** — `--baseline`/`--compare` in `hanihi-eval` to diff
  pass/fail + token usage between runs (stretch goal from plan 005).
