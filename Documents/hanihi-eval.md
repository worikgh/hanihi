# Using `hanihi-eval`

`hanihi-eval` is the eval harness for hānihi. It sends a prompt to a **live
LLM**, lets the agent run to completion, then checks assertions against the
resulting session event log. It answers one question: *did the agent behave
correctly in this scenario?*

It is a diagnostic tool, not a CI gate. Cases need an API key and network
access, and a model's output is non-deterministic, so the suite is run
manually — before a release, or while investigating a regression.

---

## What it is for

| Question                                             | How the harness answers it                                       |
|------------------------------------------------------|------------------------------------------------------------------|
| Did the agent call the tool it was supposed to?      | `tool_called` / `tool_not_called` against `ToolExecution` events |
| Did it do so without wasting turns, tokens, or time? | `max_turns`, `token_budget`, `latency_ms`                        |
| Did it say what it was asked to say?                 | `text_contains`, `text_not_contains`, `text_regex`               |
| Did anything break along the way?                    | `no_error`                                                       |
| Is the artifact it produced actually sound?          | `build_succeeds`, `tests_pass`, `lint_clean`, `no_diff`          |

The eval is a **regression guard on agent behaviour**. When a prompt,
tool description, or preamble changes, these cases are the evidence that the
change did not make the agent worse.

---

## Running it

```sh
# Everything (needs a key)
LLM_API_KEY=*** cargo run -p hanihi-eval -- --cases-dir ./evals/cases

# See what would run, without calling a model
cargo run -p hanihi-eval -- --list

# One case
LLM_API_KEY=*** cargo run -p hanihi-eval -- --case 004-self-patch

# Keep the temp session logs — essential when diagnosing a failure
LLM_API_KEY=*** cargo run -p hanihi-eval -- --case 004-self-patch --keep-sessions
```

Exit code is 0 when every case passes and 1 when any case fails, errors, or
times out.

### Flags

| Flag                  | Default                       | Meaning                                                      |
|-----------------------|-------------------------------|--------------------------------------------------------------|
| `--cases-dir <DIR>`   | `./evals/cases`               | Directory holding case subdirectories                        |
| `--case <NAME>`       | all                           | Run a single case by directory name                          |
| `--list`              | —                             | Print discovered cases and exit; no model call               |
| `--base-url <URL>`    | `https://api.deepseek.com/v1` | OpenAI-compatible endpoint (env `LLM_BASE_URL`)              |
| `--api-key <KEY>`     | —                             | Required; env `LLM_API_KEY`                                  |
| `--model <MODEL>`     | `deepseek-chat`               | Default model; a case may override it (env `LLM_MODEL`)      |
| `--mcp-command <CMD>` | —                             | Repeatable; **currently rejected** — see *Known limitations* |
| `--keep-sessions`     | off                           | Keep temp session dirs and fixture repos                     |
| `--timeout <SECS>`    | `120`                         | Per-case timeout, which also bounds each gate command        |

---

## Anatomy of a case

A case is a directory under `evals/cases/` containing a `case.toml` and a
`README.md`. The directory name is the case id.

```
evals/cases/
├── 001-basic-echo/
│   ├── README.md        # prose: what this case guards, and why
│   └── case.toml        # machine-readable: prompt + assertions
├── 002-get-time/
├── 003-self-build/
├── 004-self-patch/
└── 006-cpp-build/
    └── fixture/         # a self-contained CMake project
```

A minimal case:

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

Every assertion must pass for the case to pass. `type` selects the assertion
variant. An unrecognised `type` is a hard parse error at load time — a typo
fails the case loudly rather than silently disabling a check.

### Case fields

| Field | Default | Meaning |
|-------|---------|---------|
| `user_input` | *required* | The prompt sent to the agent |
| `assertions` | *required* | List of checks; all must pass |
| `model` | runner default | Per-case model override |
| `system_prompt` | `DEFAULT_SYSTEM_PROMPT` | Per-case system prompt override |
| `source_tree` | `false` | Register source-tree tools (`list_dir`, `grep`, `run_command`, `read_session_log`) |
| `write_tools` | `false` | Register `apply_patch` and `write_file`, and make `run_command` write-enabled |
| `repo` | none | Git repo for the case, resolved **relative to the case directory** |
| `fixture` | `false` | Synthesise a throwaway Rust git repo and use it as `repo` |
| `configure_command` | none | Run before `build_command` (e.g. `cmake -B build`) |
| `build_command` | `cargo check` | Overrides the build gate |
| `test_command` | `cargo test` | Overrides the test gate |
| `lint_command` | none | Drives `lint_clean`. **No default for a non-Cargo case** |

Commands are **argv vectors, not shell strings**. No shell is involved, so no
metacharacters are interpreted. Paths must be relative to `repo`, because a
`fixture` is copied to a temp directory and absolute paths would not survive
that move.

### Assertion reference

| `type` | Fields | Checks |
|--------|--------|--------|
| `tool_called` | `name`, `min` (1), `max` | `ToolExecution` count for `name` |
| `tool_not_called` | `name` | Count is zero |
| `text_contains` | `value` | Final answer contains the substring |
| `text_not_contains` | `value` | Final answer does not contain it |
| `text_regex` | `pattern` | Final answer matches the regex |
| `no_error` | — | No `Error` events in the log |
| `max_turns` | `max` | Highest `turn_complete` turn ≤ `max` |
| `latency_ms` | `max` | Every `llm_prompt` → `llm_response` pair ≤ `max` ms |
| `token_budget` | `max_input`, `max_output` | Cumulative usage within budget |
| `build_succeeds` | — | Build command exits 0 in `repo` |
| `tests_pass` | — | Test command exits 0 in `repo` |
| `lint_clean` | — | Lint command exits 0 in `repo` |
| `no_diff` | — | `git status --porcelain` is empty in `repo` |

`clippy_clean` is accepted as a **deprecated alias** for `lint_clean` and
keeps working unchanged in existing cases.

The four repo-backed assertions (`build_succeeds`, `tests_pass`,
`lint_clean`, `no_diff`) fail with a "no repo configured" detail unless the
case sets `repo` or `fixture`.

---

## The gates, and the honest asymmetry

`build_succeeds`, `tests_pass`, and `lint_clean` run a case-authored command
with cwd set to the resolved `repo`, capture both streams, and report whether
it exited 0. `build_succeeds` runs `configure_command` first when present; a
configure failure skips the build and reports the configure output, so the
diagnostic names the step that actually broke.

Three properties matter:

- **Failures carry real output.** A failing gate surfaces the captured
  stderr and stdout, truncated at 2000 bytes on a char boundary. The whole
  value of a build gate is the compiler's own text — a failure saying only
  "exit code 1" would be useless.
- **The environment is inherited, not scrubbed.** Unlike the agent's
  `run_command` tool, a gate may legitimately need the ambient toolchain
  (a C++ compiler on `PATH`). Case authors are trusted; models are not.
- **Gates bypass the agent's allowlist.** A case can pass its build gate even
  if the agent could never have run that command itself. The asymmetry is
  intentional: **the eval verifies the artifact, not the agent's process.**

`lint_clean` is the weak spot, and the vocabulary does not hide it.
`clang-tidy` needs a `compile_commands.json` from a successful configure, and
there is no canonical C++ equivalent of `clippy`. So there is no default
`lint_command` for a CMake case: `lint_clean` without one is a
**configuration error that fails**, never a silent pass. Most C++ cases omit
the assertion rather than fake coverage. See `evals/cases/006-cpp-build/` for
the worked example.

---

## Guardrails for writing cases

**Never point `write_tools` at the hānihi checkout.** Use `fixture = true` so
the agent edits a disposable repo, or an explicitly declared `repo`. This is
the gameability guardrail: an agent that can edit the harness it is being
graded by is not being graded.

**Prefer substrings and regexes to exact text.** Model output is
non-deterministic. Assertions should pin the property, not the phrasing.

**Right-size the fixture.** `fixture = true` synthesises a minimal Rust crate.
For anything else — C++ via CMake, a project with a custom test runner — ship
a self-contained fixture directory and point `repo` at it.

**Build the fixture by hand first.** Run its configure, build, and test
commands outside the harness and confirm all three succeed. A fixture that
has never been built is not a fixture.

**Make the case able to fail.** A case that has never gone red is not
evidence. Deliberately break the fixture, or revert the change the case
guards, and confirm the case fails — this is how `006-cpp-build` and the
ancestor-hint case were validated.

---

## What the harness deliberately does not do

- **No CI gate.** Cases need a key and the network. The hermetic half of the
  coverage lives in `cargo test -p hanihi-eval`, which exercises the
  assertion engine directly against temp repos with **no model in the loop**.
- **No benchmark.** These are correctness tests. Latency assertions catch
  regressions; they do not measure performance.
- **Not immune to non-determinism.** Treat a single failure as a signal to
  read the session log, not as proof. Flaky cases should have their
  thresholds or prompts adjusted.
- **No general truthfulness checking.** The assertion set grew a narrow
  behavioural family for contradictions between the answer and the tool log;
  it is not general-purpose claim verification.

## Known limitations

- **MCP is unsupported.** Passing `--mcp-command` returns
  `MCP support in eval runner not yet implemented`. `001-basic-echo` documents
  this; no case may depend on MCP-served tools.
- **Behavioural assertions are phrase-list based.** They match a small,
  explicit set of English phrasings, so a differently-worded false claim
  passes. That false negative is accepted, because a broader matcher would
  trip on honest prose.
- **A passing case is weak evidence on its own.** An assertion with a named
  shape invites behaviour that satisfies the shape.
- **C++ cases need `cmake` and a compiler.** The gate tests fail loudly,
  naming the missing tool, rather than silently skipping.

## Diagnosing a failure

1. Re-run with `--keep-sessions`.
2. Read the retained `events.jsonl` in the temp session directory — the
   assertion detail strings are the pointer, the log is the evidence.
3. Check the per-assertion block in the report: each line shows the label,
   pass/fail, and a detail (a tool count, the offending text, or the gate's
   captured output).
4. Distinguish a real regression from model noise by re-running the case a
   few times before changing anything.

---

## Related documents

- `plans/002-evals.md` — original design of the runner and assertion set.
- `plans/016-eval-behavioural-assertions.md` — the behavioural assertion family.
- `plans/023-cpp-eval-assertions.md` — per-case commands and `lint_clean`.
- `README.md`, section *Eval runner* — the reference table.
