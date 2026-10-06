# Completed plans

Plans whose entire stated scope has been implemented and verified in the
working tree. They are kept for the reasoning they record — why a decision was
taken, which alternatives were rejected, which failure motivated the change —
not because any work remains.

**Verification basis:** each entry below was checked against the code, not
against the plan's own status header. Many of these files still say
`**Status:** draft` or `proposed`; those headers are stale. The moving
commands are `cargo check --workspace` (green), `cargo test --workspace`, and
a read of the named source files.

## Contents

| Plan | Implemented by | Where |
|---|---|---|
| `001-sessions.md` | `Session`, `SessionManager`, `LogWriter`, `.lock`, lifecycle events | `crates/hanihi-core/src/session/` |
| `002-evals.md` | Eval runner: case discovery, assertion engine, CLI flags | `crates/hanihi-eval/src/main.rs` |
| `003-durable-execution.md` | `replay_history()`, `ToolExecutionData.call_id` with `#[serde(default)]`, `Agent::set_history` | `crates/hanihi-core/src/session/mod.rs` |
| `004-streaming.md` | `StreamEvent`, `Arc<Vec<PortableDynamicTool>>`, `run_streaming`, CLI streaming handlers | `crates/hanihi-core/src/agent.rs` |
| `005-self-improvement.md` | `run_command` + trace persistence, `apply_patch`/`write_file`, `grep`, `read_session_log`, `--write` gating, build/test/lint/no-diff assertions, `--task`, `scripts/self-improve.sh` | `crates/hanihi-core/src/tool.rs`, `crates/hanihi-cli/src/main.rs` |
| `006-dedicated-git-tools.md` | `builtin_git_status`, `run_captured_argv`, dedicated read-only git verbs | `crates/hanihi-core/src/tool.rs` |
| `007-git-write-tools.md` | `builtin_git_add`, `builtin_git_commit` (commit `3f40db6`) | `crates/hanihi-core/src/write.rs` |
| `008-log-integrity.md` | Per-line `schema`, tolerant + strict readers, `LogEntry::kind()`, streaming emits `llm_prompt`/`llm_response`, `analyse` migrated | `crates/hanihi-core/src/session/log.rs` |
| `009-bug-fix-b.md` | Order-independent `replay_history` (commit `cf76e16`) | `crates/hanihi-core/src/session/mod.rs` |
| `010-write-flag-fail-fast.md` | `source_tree_policy` seam | `crates/hanihi-cli/src/main.rs` |
| `011-mcp-read-file-tool.md` | `read_file_tool.rs`, `McpServer { tree: Option<Arc<SourceTree>> }` | `crates/hanihi-mcp-server/src/` |
| `012-token-budgeting.md` | `context.rs` estimator, `split_history`, bounded `SourceTree::read`, `MAX_TOOL_CALLS_PER_TURN`, `MAX_TOOL_RESULT_BYTES` | `crates/hanihi-core/src/context.rs` |
| `013-log-compaction-events.md` | `LogEntry::Compaction`, `SCHEMA_VERSION = 2`, both `Session` paths log it, CLI seeds `set_summary` | `crates/hanihi-core/src/session/log.rs` |
| `014-log-compaction-events-prompt.md` | Executable prompt version of 013; same landed result | see 013 |
| `015-mcp-path-resolution.md` | `OnceLock` workspace root, `discover_workspace_root`, `marker_root`, `resolve_directory`, `-32602`/`-32603` reclassification | `crates/hanihi-mcp-server/src/workspace_fs.rs` |
| `020-cpp-toolchain-detection.md` | `Toolchain` enum, `detect_toolchain`, `SourceTree::toolchain()`, all seven tests | `crates/hanihi-core/src/source.rs` |
| `021-cpp-command-allowlist.md` | `check_cmake_argv`, `check_cmake_build_args`, `check_compiler_argv` | `crates/hanihi-core/src/tool.rs` |
| `022-toolchain-aware-preamble.md` | `verification_preamble` + three variant constants and their pinning tests | `crates/hanihi-core/src/agent.rs` |
| `023-cpp-eval-assertions.md` | `build_command`/`test_command`/`configure_command`/`lint_command`; `lint_clean` with `clippy_clean` alias | `crates/hanihi-eval/src/main.rs` |
| `024-cpp-ignore-template.md` | `CMakeCache.txt`/`Testing/` in the C template; `compile_commands.json` deliberately still visible (commit `56410f5`); non-retroactivity documented at `source.rs:65` and `:714` | `crates/hanihi-core/src/source.rs` |
| `026-deterministic-apply-patch.md` | Version ledger, `base_token: "auto"`, structured refusals (commit `031f1a7`) | `crates/hanihi-mcp-server/src/version_ledger.rs` |
| `027-tool-error-diagnosability.md` | `line` selector, byte-offset wording, escaped first-differing-line reporting | `crates/hanihi-mcp-server/src/read_file_tool.rs`, `apply_patch_tool.rs` |
| `029-tool-failure-recovery.md` | **Parts A–C only** — see caveat below | `crates/hanihi-core/src/agent.rs`, `tool.rs` |

## Caveats recorded at archive time

Two entries above are not cleanly complete. They are archived anyway because
the remaining work is tracked elsewhere, not because it was finished.

- **`029-tool-failure-recovery.md`** — parts A (failure fed back as a tool
  result), B (`REPEATED_FAILURE_LIMIT` guard) and C (refusal text names the
  admitted alternative) landed. **Part D did not:** `Agent::run` and
  `Session::run` are still fully implemented rather than stubbed, and
  `AgentError::Deprecated` does not exist. The plan states the three changes
  "must land together", so this file is archived with that inconsistency
  still live. Follow-up: stub the non-streaming path or amend the plan.

- **`026-deterministic-apply-patch.md`** — §7 defers the multi-line commit
  message channel and attributes the fix to "plan 027". That attribution is
  wrong (027 is tool-error diagnosability). The real owner is
  `plans/028-commit-message-file.md`, which is **not yet implemented**. The
  apply_patch work itself is complete.

## Deleted, not archived

- `006-dedicated-git-tool.md` — an earlier draft specifying only
  `git_status`, a strict subset of `006-dedicated-git-tools.md`. Deleted
  rather than archived: two files sharing one plan number, one a subset of
  the other, is a trap for a future session that cannot tell which is
  authoritative.

## Not here

Plans with work outstanding stay in `plans/`:

- `016-eval-behavioural-assertions.md` — not started
- `017-harness-self-audit.md` — not started
- `025-cpp-smoke-test.md` — verification exercise; outcome unrecorded
- `028-commit-message-file.md` — not started
- `wc_compaction.md` — reference material, not a plan
