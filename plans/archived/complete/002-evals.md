# Plan 002 — Evals

**Status:** draft | **Created:** 2026-08-12 | **Scope:** new binary crate

## Overview

An eval runner that feeds hānihi prompts against a real LLM and checks
assertions against the session event log. Separate from `cargo test` because
it needs API keys and network access — it's a diagnostic harness, not a CI
gate.

---

## Architecture

### New crate: `crates/hanihi-eval`

Binary crate depending on `hanihi-core`. Discovers test cases from
`evals/cases/`, runs them against a live model, and reports pass/fail.

```
crates/hanihi-eval/
├── Cargo.toml
└── src/
    └── main.rs          # CLI, runner, assertion engine (single file while small)
```

### Test case layout

```
evals/cases/
├── 001-basic-echo/
│   ├── README.md        # human description
│   └── case.toml        # machine-readable
├── 002-get-time/
│   ├── README.md
│   └── case.toml
└── ...
```

### `case.toml` schema

```toml
# Optional: override default model
model = "deepseek-chat"

# Optional: override system prompt
system_prompt = "You are a helpful assistant."

# Required: the user input to send
user_input = "Echo back: hello world"

# Optional: enable source-tree tools (read_file, list_dir)
source_tree = false

# All assertions must pass for the case to pass
[[assertions]]
type = "tool_called"
name = "echo"

[[assertions]]
type = "text_contains"
value = "hello world"

[[assertions]]
type = "no_error"
```

### Assertion types

| type | fields | checks |
|------|--------|--------|
| `tool_called` | `name`, `min?` (default 1), `max?` | count of `ToolExecution` events matching `name` |
| `tool_not_called` | `name` | count is zero |
| `text_contains` | `value` | final answer contains substring |
| `text_not_contains` | `value` | final answer does NOT contain substring |
| `text_regex` | `pattern` | final answer matches regex |
| `no_error` | (none) | no `Error` events in log |
| `max_turns` | `max` | turn count on `turn_complete` ≤ max |
| `latency_ms` | `max` | each `llm_prompt` → `llm_response` ≤ N ms |
| `token_budget` | `max_input?`, `max_output?` | cumulative tokens ≤ budget |

### Runner flow

1. Parse CLI args (model, API key, cases dir, case filter)
2. Discover cases (scan `evals/cases/` for `case.toml` files)
3. For each case:
   a. Create a temporary session
   b. Build agent with standard builtins (+ source-tree if `source_tree = true`)
   c. Call `Session::run` — writes full event log to temp session
   d. Read and parse `events.jsonl`
   e. Evaluate assertions against parsed log
   f. Print result: PASS/FAIL + per-assertion detail + token usage + latency
   g. Clean up temp session (or keep if `--keep-sessions`)
4. Print summary: N total, M passed, F failed
5. Exit 0 if all pass, nonzero if any fail

### CLI

```
cargo run -p hanihi-eval -- [OPTIONS]

Options:
  --cases-dir <DIR>      Directory containing cases/ (default: ./evals/cases)
  --case <NAME>          Run a single case by directory name (e.g. "001-basic-echo")
  --list                 List all discovered cases and exit
  --base-url <URL>       LLM base URL (env: LLM_BASE_URL)
  --api-key <KEY>        API key (env: LLM_API_KEY)
  --model <MODEL>        Default model (env: LLM_MODEL, default: deepseek-chat)
  --mcp-command <CMD>    Attach an MCP server (repeatable)
  --keep-sessions        Don't clean up temp session directories
  --timeout <SECS>       Per-case timeout in seconds (default: 120)
```

### Dependencies

| crate | purpose |
|-------|---------|
| `hanihi-core` (path) | Session, Agent, LogEntry, tools |
| `toml` | parse case.toml |
| `regex` | `text_regex` assertions |
| `clap` | CLI (derive + env) |
| `tokio` | runtime |
| `serde` / `serde_json` | deserialization |
| `rig-core` | connect chat model |

---

## What this is NOT

- **Not runnable in CI without API keys.** Cases need a real LLM. They're
  diagnostic tools you run locally before releases or during development.
- **Not a benchmark suite.** These are correctness tests: "does the agent
  behave correctly?" not "how fast is it?" (though latency assertions let
  you catch regressions).
- **Not immune to model non-determinism.** Assertions use substring/regex
  matching, not exact text comparison. Flaky cases are possible — adjust
  thresholds or rewrite prompts if a case fails intermittently.

---

## Implementation order

### Step 1 — Scaffold
- `crates/hanihi-eval/Cargo.toml` + `main.rs` skeleton
- Workspace `Cargo.toml` updated

### Step 2 — Case loading + CLI
- `Case` struct with TOML deserialization
- `Assertion` enum with tag-based deserialization
- CLI flags (clap derive)

### Step 3 — Assertion engine
- Parse events.jsonl into `Vec<LogEntry>`
- Evaluate each assertion type against the log
- `AssertionResult { passed, detail }` per assertion

### Step 4 — Runner orchestration
- Temp session creation + agent build + `Session::run`
- Log parsing + assertion evaluation
- Report printing (per-case + summary)

### Step 5 — Initial test cases
- `001-basic-echo` — verify echo tool works
- `002-get-time` — verify get_time tool returns plausible output

### Step 6 — Smoke test
- Build, run against DeepSeek, verify both cases pass
