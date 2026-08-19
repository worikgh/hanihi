# hānihi

A minimal tool-calling agent harness in Rust — the core of a future coding
agent. Built on:

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
└── mcp-echo-server/    # binary: minimal MCP stdio server (demo, one
                        #   `mcp_echo` tool; not published)
evals/
└── cases/              # eval test cases (TOML + README per case)
```

> Crate *package* names are ASCII (`hanihi-core`, `hanihi-cli`, `hanihi-eval`)
> because crates.io only accepts ASCII names. The project name keeps the macron
> (`hānihi`), as does the CLI's display name. The lib target is
> `hanihi_core` (rustc requires ASCII identifiers for `--extern`).

## Features

- **Agent loop** — system preamble + persistent history + tool definitions,
  tool-call dispatch, `max_turns` guard (default 10), per-turn usage tracking
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
- **Built-in tools** — `get_time` (local RFC 3339 timestamp), `echo`,
  `read_file` + `list_dir` (source-tree access, see below)
- **Source tree access** — the agent can read and list the enclosing git
  repository (found by walking up from the cwd). Everything is filtered by
  the repo's ignore rules via the `ignore` crate: `.gitignore` and
  `.git/info/exclude` are respected and never written; hānihi maintains its
  own `.ignore` file (same syntax, git-agnostic) at the repo root with
  generated-artifact templates for the languages it detects (`Cargo.toml` →
  Rust; CMake/Makefile/C-family sources → C/C++). Reads are capped at
  64 KiB, escapes outside the repo are refused, and `target/`-style noise
  never reaches the model.
- **MCP client** — spawn an MCP stdio server, wrap each of its tools as an
  agent tool dispatching over `tools/call`
- **CLI** — interactive reedline REPL (`/help /tools /clear /session /quit`),
  `--once` one-shot mode for scripting and smoke tests, repeatable
  `--mcp-command`, session management (`--session` / `--new-session`)
- **Eval harness** (`hanihi-eval`) — run TOML-based test cases against a
  live LLM, assert tool calls / text content / error-free completion /
  latency / token budgets against the session log. Separate from
  `cargo test` because it needs API keys.

## Run

```bash
# One-shot turn (streaming output)
cargo run -p hanihi-cli -- --once "What time is it? Use the get_time tool."

# Create a named session and have a conversation
cargo run -p hanihi-cli -- --new-session my-chat
# Later, resume:
cargo run -p hanihi-cli -- --session my-chat

# Attach an MCP server and talk interactively
cargo run -p hanihi-cli -- --mcp-command "./target/debug/mcp-echo-server"

# Run the eval suite against DeepSeek
LLM_API_KEY=*** cargo run -p hanihi-eval -- --cases-dir ./evals/cases

# REPL commands: /help /tools /clear /session /quit (or /exit)
```

Configuration — every flag has an environment variable:

| Flag | Env | Default |
|---|---|---|
| `--base-url` | `LLM_BASE_URL` | `https://api.deepseek.com/v1` |
| `--api-key` | `LLM_API_KEY` | — (required) |
| `--model` | `LLM_MODEL` | `deepseek-chat` |
| `--session NAME` | — | `default-session` |
| `--new-session NAME` | — | none (auto-creates `default-session` on first run) |
| `--working-dir DIR` | `HANIHI_WORKING_DIR` | `./working` |
| `--mcp-command CMD` | — | none (repeatable) |
| `--once PROMPT` | — | none |

## How it works

### Agent loop

`Agent::run` is the synchronous loop:

1. Build a completion request: system preamble + persistent history + the new
   user input + tool definitions.
2. If the model replies with text only → turn complete, history committed.
3. If the model requests tool calls → record the assistant message, execute
   each tool (built-in or MCP), append results as `tool_result` messages,
   loop back to 1.
4. `max_turns` (default 10) guards runaway tool-call loops.

`Agent::run_streaming` does the same but yields events through a
`tokio::sync::mpsc` channel: text arrives token-by-token, tool calls are
announced as they start and complete, and results are reported as they
execute. The agent loop runs on a spawned task so the caller can read events
in real time.

### Sessions

`SessionManager` owns a working directory (`./working` by default). Each
session is a subdirectory under `working/sessions/<name>/`:

```
session.json    — static metadata (id, name, created_at, model, system_prompt)
events.jsonl    — append-only JSONL log: user_input, llm_prompt, llm_response,
                  tool_execution, turn_complete, error, lifecycle events
.lock           — filesystem lock (one process per session)
```

On open, `replay_history()` scans the log and reconstructs the agent's
message history from completed turns. Partial turns (log ends without
`turn_complete`) are dropped. Streaming sessions that lack `llm_response`
entries get synthetic assistant messages inserted during replay.

### Eval runner

Each case is a directory under `evals/cases/` containing a `case.toml`:

```toml
user_input = "Use the echo tool to repeat back: hello world"

[[assertions]]
type = "tool_called"
name = "echo"

[[assertions]]
type = "text_contains"
value = "hello world"

[[assertions]]
type = "no_error"
```

The runner creates a temp session, runs the prompt against a live LLM, then
checks each assertion against the `events.jsonl` log. Assertion types:
`tool_called`, `tool_not_called`, `text_contains`, `text_not_contains`,
`text_regex`, `no_error`, `max_turns`, `latency_ms`, `token_budget`.

Tools are rig `PortableDynamicTool`s: name + description + JSON schema + an
async callback over raw `serde_json::Value`. MCP tools get wrapped into this
shape, dispatching over `tools/call` on the connected service.

The library is model-agnostic: `Agent<M: CompletionModel>` works with rig's
`MockCompletionModel` in tests and any OpenAI-compatible chat-completions
endpoint in production (see `connect_chat_model`).

## Status

- 39 unit tests (rig `MockCompletionModel`, scripted turns, temp-repo
  fixtures, session replay — no network), `cargo clippy --workspace --all-targets -- -D warnings` clean
- Smoke-tested against DeepSeek (`deepseek-chat`): `get_time` round trip ✔,
  MCP `mcp_echo` round trip ✔, streaming output ✔, session replay across
  restarts ✔, eval runner against live model ✔
- 0.2.0 on crates.io

## Testing

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Eval runner (needs API key):
LLM_API_KEY=*** cargo run -p hanihi-eval -- --cases-dir ./evals/cases
LLM_API_KEY=*** cargo run -p hanihi-eval -- --list
LLM_API_KEY=*** cargo run -p hanihi-eval -- --case 001-basic-echo
```

## Publishing (crates.io)

`hanihi-core` and `hanihi-cli` are publishable (both inherit version,
license, repository, and readme from the workspace). `hanihi-eval` and
`mcp-echo-server` are not published (`publish = false`).

Publish order matters: **`hanihi-core` first**, then `hanihi-cli` (it depends
on the published `hanihi-core`):

```bash
cargo login                      # once: paste token from https://crates.io/settings/tokens
cargo publish -p hanihi-core
cargo publish -p hanihi-cli
```

Sanity-check locally before publishing:

```bash
cargo package -p hanihi-core --list     # inspect tarball contents
cargo publish -p hanihi-core --dry-run  # full verification, no upload
```

## Known TODOs

- Tool name collisions: first registration wins (builtin `echo` shadows an MCP
  `echo`). Namespacing MCP tools is a future concern.
- `run_command` tool — let the agent run `cargo build` / `cargo test` inside
  the enclosing repo. (Specified in plan 005.)
- **Write tools** (plan 005) — `apply_patch` / `write_file`, registered only
  with `--write`; repo-scoped via `SourceTree` guards; changes land as
  commits, never pushed.
- **LSP via MCP** — bridge an LSP server (goto-definition, references,
  hover) through the existing MCP client. Cheaper first step than
  tree-sitter for symbol-level code intelligence.
- **Tree-sitter symbol analysis** — `tree-sitter` + Rust grammar deps; a
  `symbols` module (definitions, signatures, kinds, line numbers); a
  `symbols(path)` tool or startup index under `working/`; reference-finding
  to support multi-file refactoring.
- **Multi-file refactoring** — agent emits a plan applied as one multi-file
  unified diff via `apply_patch` (plan 005), verified by workflow gates +
  before/after evals.
- **Background workers** — in-process task layer (durable task state in the
  event log, `read_task` tool, file-change watcher). The plan-005 driver
  script is the minimal external version; do that first.
- `add_ignore` tool / `--regenerate-ignore` — grow `.ignore` from within the
  agent.
