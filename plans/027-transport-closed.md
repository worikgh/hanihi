# 027 — Fix the `Transport closed` tool failure

You are Hānihi. This is a prompt to a future session. It is self-contained:
implement it even if nothing else in the sequence has landed.

**Priority: urgent.** This bug destroys turns. A tool call that fails this way
is a *harness* fault, not a model mistake, and it has been observed repeatedly
across sessions. It is the single highest-value fix available: every other plan
in this sequence is blocked whenever it fires, because writes stop working and
the agent cannot recover by retrying.

## Status of the prior attempt

No fix was attempted. One session misdiagnosed it twice and produced nothing.
Both misdiagnoses are recorded below because they are the traps to avoid:

1. **"The MCP server process died."** Wrong. A live `ps aux` shows both servers
   (`hānihi-mcp-server-ro`, `hānihi-mcp-server-rw`) still running as children of
   the parent CLI, started on a previous day, still resident. The process did
   not exit.
2. **"Stale `base_token` / bad arguments."** Wrong, and demonstrably so: the
   identical error text appeared for *different tools* (`apply_patch`,
   `delete_file`) while `echo`, `read_file`, `run_command` and `write_file`
   kept working in the same window. An argument-dependent error cannot behave
   that way.

Do not repeat either. Start from the evidence collected below.

## Objective

Find the actual cause of `Transport closed` and fix it, so that a tool call
either succeeds or fails with a message that names the real fault. A retry must
never be the only way forward, and a harness-side fault must be distinguishable
from a model-side mistake.

## Findings (verified; do not re-derive)

### The error text is not Hānihi's

`grep` for the literal across `crates/` returns **no matches**. The string does
not appear anywhere in this repository's Rust source. It is produced by the MCP
client library (`rmcp`) and surfaced by Hānihi wrapped in `AgentError::Tool`:

```text
crates/hanihi-core/src/agent.rs:971: execute_tool_with_cache error. name: search_text
Error: tool 'search_text' failed: Transport closed
```

The `agent.rs:971` line number is from an older revision of that file; treat it
as a hint, not a fact. The wrapping site is `execute_tool_with_cache`, which
maps any tool error with `e.to_string()` into `AgentError::Tool { name, message }`.

### Where the string is produced

`rmcp`'s service layer logs its own lifecycle. The trace below is the only
instance with full surrounding instrumentation, and it is from session
`working/sessions/022-toolchain-aware-preamble/events.jsonl` (lines 469-470),
a **different session from the ones currently failing**:

```text
INFO serve_inner: rmcp::service: input stream terminated
INFO serve_inner: rmcp::transport::child_process: Child exited gracefully signal: 15 (SIGTERM)
INFO serve_inner: rmcp::service: serve finished quit_reason=Closed
... Error: tool 'search_text' failed: Transport closed
```

That trace shows one *mechanism* — the server's stdio stream ended, so its
serve loop finished and the in-flight call could never be answered. It does
**not** show that this is what is happening now: the processes are alive. Treat
`quit_reason=Closed` as a symptom to look for, not the current diagnosis.

### The live-process evidence

From a `ps aux | grep mcp` taken minutes before this plan was written:

```text
worik 1896535  ... Sl+  Oct11  1:00  hānihi-cli --mcp-command hānihi-mcp-server-ro
                                     --mcp-command hānihi-mcp-server-rw ... \
                                     --new-session 021-fix-jackd-ignored-tests
worik 1896540  ... S+   Oct11  0:00  hānihi-mcp-server-ro
worik 1896541  ... S+   Oct11  0:00  hānihi-mcp-server-rw
```

Read this carefully before theorising:

- `START` is `Oct11` for all three, i.e. they started on a **previous day** and
  were not restarted. `TIME` is accumulated CPU time, not wall time.
- Both servers are `S+` — sleeping, in the **foreground process group** of
  `pts/22`. The parent is `Sl+` (multithreaded, also foreground).
- The parent was launched with `--yolo`, which appends a "keep running, do not
  stop to ask" paragraph to the system prompt. That is prompt text only; it
  does not change process lifecycle, and it is not the cause — but it does mean
  the affected sessions run long and unattended, which is why the fault is
  frequent.

So the fault occurs **while the server process is alive**. The candidates that
survive this evidence are all about the *connection*, not the process.

## Hypotheses to test

Ordered by likelihood, with the discriminating test for each. Do not skip to a
fix; the whole point of this plan is that two sessions guessed.

### H1 — A second connection to the same stdio server

Each MCP server is a single child process speaking JSON-RPC on one stdio pair.
If two connections are established to the same server (for example both the
read-only and read-write tool sets handed to the same process, or a reconnect
that does not tear down the first), the second initialize can cause one side to
close, and every call routed over the closed one fails identically — which
matches "two tools broken, others fine".

**Discriminating test:** log the number of live MCP clients and the tool name →
client mapping at `Agent` construction. If two `McpClient`s wrap one PID, H1 is
confirmed.

### H2 — The connection is closed by teardown while the process lingers

The child may be signalled or its pipe closed independently of the process
exiting (a stopped foreground group, a `SIGTSTP`, a `shutdown` on one side of
the pipe). The process stays listed and responsive to `ps` but its stdin is at
EOF, which is exactly `input stream terminated` with a live PID.

**Discriminating test:** reproduce while capturing `rmcp` debug logs (see
Instrumentation) and watch whether `input stream terminated` fires at the
moment of failure, and what `quit_reason` is. A `quit_reason=Closed` with a
live PID confirms H2.

### H3 — An idle or capacity timeout in the transport

A long unattended session (`--yolo`, `--max-turns 100`) may hit a timeout on
the transport that closes the pipe without killing the child.

**Discriminating test:** correlate failure timestamps against session elapsed
time and against `rmcp::transport::child_process` log lines. If failures
cluster at a fixed elapsed offset, H3 is confirmed.

### H4 — The write path only

The failures observed so far are on `apply_patch` and `delete_file`, both
mutating tools. Read tools kept working. If the read-write server is the one
affected, H1/H2 apply to it specifically.

**Discriminating test:** attempt a read tool and a write tool back to back at
the moment of failure and record which side fails. Note however that a
`delete_file` call in one session reported "Transport closed" while `read_file`
in the same session worked — so if H4 holds, the split is per-server, not
per-mutating-tool.

## Instrumentation to add first

Do **not** start by changing behaviour. Start by making the fault observable,
because the previous two sessions each theorised from an absence of evidence.

1. **Fail loudly and distinguishably.** `AgentError::Tool` currently carries a
   rendered `String`, which erases the difference between "the tool refused"
   and "the transport died". Add a way to tell them apart — either a
   `TurnErrorKind::Transport`/`AgentError::Transport` variant, or an explicit
   classification at the `execute_tool_with_cache` call site. The message must
   name the transport, not merely repeat the library's wording.

2. **Log the client wiring.** At construction, log each MCP server's PID and
   which tools were registered from it. At failure, log which client the call
   was routed to. This is the single measurement that separates H1 from H2/H3.

3. **Run with transport logging.** The CLI reads
   `EnvFilter::try_from_default_env()`, so `RUST_LOG=rmcp=debug` (or
   `rmcp::transport=debug`) is available without a code change. Reproduce under
   that filter and capture the output to a trace file.

**Never** read or grep `minimal-mcp-ro.log` or `minimal-mcp-rw.log` at the repo
root: ~1.4 GB and ~13 MB respectively, and one of them records this session's
own traffic, which makes its contents actively misleading as evidence.

## Fix

Write the fix only after a hypothesis is confirmed. The shape depends on which
one it is:

- **H1:** make client construction own one connection per server, with an
  explicit lifetime, and fail at startup if a server is handed two clients.
  This is a correctness constraint, not a retry policy.
- **H2:** find what closes the connection, and stop it. If the close is
  external, detect it and reconnect *once*, deliberately, with the reconnect
  visible in the log — not as a silent retry loop.
- **H3:** make the timeout explicit and diagnosed, and either extend it or
  surface it as a timeout rather than as a closed transport.

Universal requirement whichever it is: **a transport failure must be reported
as a transport failure.** Right now it arrives as an ordinary tool error, the
model reads it as "that tool does not work", and it spends the rest of the turn
trying alternatives — or, as observed, retries the identical call. That
misreporting is itself the bug's most damaging symptom and must be fixed even
if the root cause proves hard to reach.

## Tests

Test-first. The assertions below are written to be runnable without a live
model, using the existing scripted-turn pattern.

1. `transport_failure_is_not_reported_as_a_tool_error` — a tool whose
   execution returns a transport-level failure produces a distinguishable error
   (the new variant or kind), not a bare `AgentError::Tool`.
2. `transport_failure_message_names_the_transport` — the rendered message
   identifies the failure as transport-level, and does not merely echo
   `Transport closed`.
3. `a_transport_failure_does_not_retry_an_identical_call` — the failure path
   must not re-issue the same call automatically. Assert the call count.
4. `one_client_per_server` — constructing the toolset for the two-server
   configuration yields exactly one connection per server, with no server
   reached twice. Fails loudly if H1 is possible by construction.
5. Regression test for whichever hypothesis is confirmed, named after it, so a
   future change that reintroduces it fails here.

If a test cannot be written without a live MCP server, say so plainly and mark
it unverified. Do not weaken an assertion to make an unrunnable case look
covered.

## Verification gates

```text
cargo fmt
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Then a manual reproduction: run a write-tool turn under
`RUST_LOG=rmcp=debug`, confirm the failure is either gone or correctly
diagnosed, and report which. A fix that cannot be shown to change the observed
behaviour is not a fix.

## Acceptance criteria

- The cause is named, with the measurement that identified it recorded in the
  commit body — not a theory.
- A transport failure is reported as a transport failure, and is
  distinguishable from a tool refusal by a caller.
- No automatic identical retry on the transport path.
- The two misdiagnoses in "Status of the prior attempt" are not repeated: the
  fix must survive the live-process evidence above.
- Existing tests pass unchanged.

## Out of scope

- Re-opening `026`, `005`, or the archived `002`/`003`/`004` sequence.
- Rewriting the MCP servers themselves in `crates/hanihi-mcp-server/`. The
  bug is in the client side's handling; if investigation shows otherwise, stop
  and record that finding rather than expanding this plan silently.
- Any retry/backoff framework. One deliberate reconnect is the ceiling.
- Changing the command allowlist, the system preamble, or eval assertions.

## Commit message

Subject, imperative, capitalized, no final period, ≤72 characters, then a
blank line, then a body wrapped at 72 explaining context and reasoning rather
than implementation. State the confirmed cause, the measurement that identified
it, and which hypotheses were eliminated. End with:

```
Hānihi
```
