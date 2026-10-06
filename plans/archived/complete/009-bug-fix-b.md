The tool `apply_patch` is broken.  So complete this session with out using it

Action this: 

# This error was seen in a Hānihi run

```sh
2026-09-04T00:41:13.632474Z ERROR chat{gen_ai.operation.name="chat" gen_ai.provider.name="openai" gen_ai.request.model="deepseek-v4-pro"}: rig_core::providers::internal::openai_chat_completions_compatible: SSE error error=InvalidStatusCodeWithMessage(400, "{\"error\":{\"message\":\"An assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id', The following tool_call_ids did not have response messages: call_00_CA39fJHosSbZSC09Cmkt0017\",\"type\":\"invalid_request_error\",\"param\":null,\"code\":\"invalid_request_error\"}}")

error: HttpError: Invalid status code 400 Bad Request with message: {"error":{"message":"An assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id', The following tool_call_ids did not have response messages: call_00_CA39fJHosSbZSC09Cmkt0017","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}
```


## Root cause

The 400 is real, but the generic explanation mis-locates the bug. In-process, hānihi builds the transcript correctly. It breaks on **resume**, in `cra
tes/hanihi-core/src/session/mod.rs::replay_history()`.                                                                                                                                                                                                                                                      The two log paths write events in different orders:

- `Session::run` (non-streaming): `llm_response` (with `tool_calls`) → `tool_execution`*.                                                             - `run_streaming_loop` (agent.rs): `tool_execution`* → `llm_response`. Tool results are emitted during the stream; `CompletionResponse`/`llm_response`
 is emitted after the stream ends.
																																					  `replay_history()` was written for the first order. It uses a single `last_had_tool_calls` flag to decide whether a `tool_execution` needs a synthetic
 assistant message.

Turn 1 of `self-improvement-14-apply-patch` logged (streaming order):

```
3  user_input
4  llm_prompt # model call A
5  tool_execution list_dir JQX
6  tool_execution grep      MZX
7  llm_response tool_calls=[JQX, MZX]
8  llm_prompt                         # model call B
9  tool_execution read_file CA39
10 llm_response tool_calls=[CA39]
11 llm_prompt                         # model call C
12 llm_response text
13 turn_complete
```

Replay mis-assembles this into:

```
user
assistant(tool_call JQX)          # synthetic — result already seen
tool_result JQX
assistant(tool_call MZX)          # synthetic
tool_result MZX
assistant(tool_calls JQX, MZX)    # real llm_response — now UNANSWERED
tool_result CA39                  # orphaned, attached after the wrong assistant
assistant(tool_call CA39)         # real llm_response — UNANSWERED
assistant(text)
```

The final `assistant(tool_call CA39)` is immediately followed by `assistant(text)`, so the API reports `call_00_CA39fJHosSbZSC09Cmkt0017` has no tool response. The `assistant(tool_calls JQX, MZX)` is also mismatched. The turn completed fine originally; only the replayed transcript is corrupt, which is why the same session fails on every reopen (events 22 and 27 are the two 400s after two resume attempts).

## Recovery plan

1. **Test first (red).** Add a session test that writes the exact streaming-order log above and asserts `replay_history()` returns the canonical sequence:
   `user, assistant(JQX,MZX), tool JQX, tool MZX, assistant(CA39), tool CA39, assistant(text)` — and a validator test asserting no assistant `tool_calls` is immediately followed by a non-matching message.
2. **Fix (green).** Rework `replay_history()` to be order-independent: pair each `tool_execution.tool_call_id` with the matching `llm_response.tool_calls[].id`, regardless of which entry comes first. Keep the synthetic-assistant fallback only for `tool_execution` entries with no corresponding `llm_response` (legacy streaming logs).
3. **Defense in depth.**
   - Validate the transcript before each completion request; for any assistant `tool_calls` id with no following tool message, append an error tool result (or drop the incomplete assistant turn) instead of sending a 400-bound request.
   - On tool execution failure (both `run` and `run_streaming`), append an error tool result rather than bailing with an unmatched assistant message — currently a failed tool can leave the same unanswered-`tool_calls` shape in the log.
4. **Verify.** `cargo test -p hanihi-core`; then confirm the repaired `replay_history()` makes `self-improvement-14-apply-patch` replay cleanly and the next model call succeeds.

Rollback is low-risk: the fix is internal to replay reconstruction; existing non-streaming replay tests (`replay_with_tool_calls`, `replay_old_log_missing_call_id`) must continue to pass unchanged.
