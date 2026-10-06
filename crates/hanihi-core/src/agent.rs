//! The agent: model + tools + message history + the tool-calling loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures::StreamExt as _;
use rig::client::CompletionClient;
use rig::completion::message::ToolCall;
use rig::completion::{
    AssistantContent, CompletionModel, GetTokenUsage, Message, ToolDefinition, Usage,
};
use rig::providers::openai;
use rig::tool::PortableDynamicTool;
use tokio::sync::mpsc;

use crate::audit;
use crate::context::{
    COMPACTION_PROMPT, DEFAULT_CONTEXT_LIMIT_TOKENS, MAX_SUMMARY_TOKENS, RESERVE_OUTPUT_TOKENS,
    context_limit_for, estimate_context, serialize_for_summary, split_history,
    truncate_to_token_budget,
};
use crate::error::AgentError;
use crate::session::log::{ContextMessage, UsageData};
use crate::source::Toolchain;
use crate::tool::truncate_tool_output;

/// Default system prompt used when none is supplied.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant running in an agent harness. \
You have access to tools. Use them when they help answer the user; otherwise answer directly. \
When a tool result comes back, incorporate it into your final answer. \
Your tool list is whatever the harness gives you, and it is authoritative: never claim you \
lack a tool, or that a tool is unavailable, unless a tool call has actually failed and you \
are quoting its error. \
Do not re-run the same read-only tool with the same arguments within a turn: reuse the result \
you already have, because the result cannot have changed and re-running wastes resources. \
When a tool call fails or reports an error, do not stop. Report the failure, then continue \
toward the goal by the next viable means (retry once only if the cause was transient; \
otherwise try a different approach). Stop only when no way to continue remains, and then \
explain what blocked you.";

/// System prompt for task mode: long-horizon self-improvement work with
/// explicit workflow gates (mirrors the project's Rust workflow rules).
///
/// This is the invariant part of the task preamble — everything that is true
/// regardless of the repository's build toolchain. The verification sequence
/// is appended per toolchain by [`task_system_prompt`].
pub const TASK_SYSTEM_PROMPT: &str = "You are hānihi in task mode: a coding agent. \
Work in small steps and verify with the build before declaring success. ";

/// Task-mode preamble suffix, identical for every toolchain: retry discipline,
/// tool-list authority, and the never-push contract.
const TASK_SYSTEM_PROMPT_SUFFIX: &str = " \
Study command output and trace files before retrying: if a \
command fails, read the error and fix the cause rather than repeating it. Verify your work with \
the build/test gates — do not assert success by eye. Before calling a tool, check whether an \
identical read-only call with a usable result already appears in this turn; reuse it instead of \
re-running. Your tool list is whatever the harness gives you, and it is authoritative: never \
claim you lack a tool, or that a tool is unavailable, unless a tool call has actually failed \
and you are quoting its error. When a tool call fails or reports an error, do not stop. Report \
the failure, then \
continue toward the goal by the next viable means (retry once only if the cause was transient; \
otherwise try a different approach). Stop only when no way to continue remains, and then \
explain what blocked you.";

/// Rust (Cargo) verification workflow. Byte-identical to the workflow
/// sentences the preamble carried before they became toolchain-dependent.
const CARGO_VERIFICATION_PREAMBLE: &str = "Workflow gates: run `cargo fmt` before staging changes; \
`cargo test` before committing; `cargo build` must pass; \
run `cargo clippy -- -D warnings` before finishing. Make changes as small git commits with \
descriptive messages. Never push.";

/// C++ (CMake) verification workflow.
///
/// Deliberately weaker than the Rust variant: no formatter and no linter are
/// nameable here. No formatter is admitted by the command allowlist, and
/// `clang-tidy` additionally needs a `compile_commands.json` from a
/// successful configure. Rather than name tools that cannot be invoked, this
/// variant states which gates *are* checkable — compiles and tests pass — and
/// leaves style to matching the surrounding code.
const CMAKE_VERIFICATION_PREAMBLE: &str = "Workflow gates: configure with `cmake -B build`, \
then build with `cmake --build build`. For a fast per-file check, compile the file with the \
compiler's `-c -fsyntax-only` flags — it is cheaper than a full build and catches parse and \
type errors immediately. Run the test suite with `ctest --test-dir build` when the project \
defines tests. Compiles and tests pass are the gates you can verify here; no formatter or \
linter is available, so match the surrounding style and do not reformat code you did not \
change. Make changes as small git commits with descriptive messages. Never push.";

/// No recognised build system. Naming no tool is the point: a model that
/// invents `make` or `./configure` in a repository with no marker file
/// produces confusing refusals.
const UNKNOWN_VERIFICATION_PREAMBLE: &str = "Workflow gates: no build or test command is \
available for this repository — no recognised build system was detected. Say so rather than \
guessing at one: commands you invent will be refused. Match the surrounding style, keep changes \
small, and commit your work as small git commits with descriptive messages. Never push.";

/// Hard cap on tool executions within a single turn. Additional to
/// [`Agent::max_turns`]: a turn may legally make many tool calls across
/// model turns, and this guard bounds that runaway loop.
const MAX_TOOL_CALLS_PER_TURN: usize = 1000;

/// Read-only, deterministic tools whose results may be reused within a turn.
const CACHEABLE_TOOLS: &[&str] = &["read_file", "list_dir", "grep", "read_session_log", "echo"];

/// Tools that mutate the repository. A successful call invalidates the
/// per-turn read-only tool cache.
const WRITE_TOOLS: &[&str] = &["apply_patch", "write_file"];

/// Result of looking up a tool call in the per-turn cache.
enum ToolCallCacheLookup {
    /// An earlier identical call succeeded; reuse this rendered result.
    Hit(String),
    /// The identical read-only call has already been made too many times.
    DuplicateLimit,
    /// Not cacheable, or the first time this call has been seen.
    Miss,
}

/// Consecutive identical tool failures tolerated before the turn ends.
///
/// Counted per (name, args) pair. A failure with a different name or
/// arguments is evidence the model adapted and does not accumulate, so this
/// measures the specific pathology of repeating a call that already failed,
/// rather than raw tool-call volume (which [`MAX_TOOL_CALLS_PER_TURN`] bounds).
const REPEATED_FAILURE_LIMIT: usize = 3;

/// Prefix on a tool result that reports a failed execution. The model reads
/// this as data it must act on, not as a harness shutdown.
const TOOL_FAILURE_PREFIX: &str = "tool call failed";

/// Result of checking whether a repeated failure has exhausted its budget.
enum FailureLookup {
    /// Below the limit; continue with the failure fed back.
    Continue,
    /// The identical call has now failed `REPEATED_FAILURE_LIMIT` times.
    GiveUp { count: usize },
}

/// Per-turn cache of read-only tool results keyed by `name + '\u{1}' + args`.
#[derive(Debug, Default)]
struct ToolCallCache {
    results: HashMap<String, String>,
    counts: HashMap<String, usize>,
    /// `(name, args)` -> consecutive failure count. Deliberately *not* cleared
    /// by [`Self::invalidate_on_write`]: a successful write does not make a
    /// refused command legal, so the asymmetry with the read cache is
    /// intentional.
    failures: HashMap<String, usize>,
}

impl ToolCallCache {
    fn reset(&mut self) {
        self.results.clear();
        self.counts.clear();
        self.failures.clear();
    }

    fn key(name: &str, args: &serde_json::Value) -> String {
        format!("{name}\u{1}{args}")
    }

    /// Record one failed execution of `name` with `args`.
    fn record_failure(&mut self, name: &str, args: &serde_json::Value) {
        *self.failures.entry(Self::key(name, args)).or_insert(0) += 1;
    }

    /// Decide whether the failure just recorded exhausts the guard.
    fn notable_for_failure(&self, name: &str, args: &serde_json::Value) -> FailureLookup {
        let count = self
            .failures
            .get(&Self::key(name, args))
            .copied()
            .unwrap_or(0);
        if count >= REPEATED_FAILURE_LIMIT {
            FailureLookup::GiveUp { count }
        } else {
            FailureLookup::Continue
        }
    }

    /// Clear the failure count for `name`/`args` after a successful call, so
    /// fail-then-fix-then-fail does not accumulate across unrelated attempts.
    fn clear_failure(&mut self, name: &str, args: &serde_json::Value) {
        self.failures.remove(&Self::key(name, args));
    }

    fn lookup(&mut self, name: &str, args: &serde_json::Value) -> ToolCallCacheLookup {
        if !CACHEABLE_TOOLS.contains(&name) {
            return ToolCallCacheLookup::Miss;
        }
        let key = Self::key(name, args);
        let count = self.counts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count >= 3 {
            return ToolCallCacheLookup::DuplicateLimit;
        }
        match self.results.get(&key) {
            Some(rendered) => ToolCallCacheLookup::Hit(rendered.clone()),
            None => ToolCallCacheLookup::Miss,
        }
    }

    fn store(&mut self, name: &str, args: &serde_json::Value, rendered: &str) {
        if CACHEABLE_TOOLS.contains(&name) {
            let key = Self::key(name, args);
            self.results.insert(key, rendered.to_string());
        }
    }

    fn invalidate_on_write(&mut self, name: &str) {
        if WRITE_TOOLS.contains(&name) {
            self.reset();
        }
    }
}

/// Result of one `Agent::run` invocation.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    /// Final assistant text.
    pub text: String,
    /// Number of tool calls executed during the turn.
    pub tool_calls: usize,
    /// Total token usage across all model calls in the turn.
    pub usage: Usage,
    /// Final message history after the turn (for streaming — the agent's
    /// history is updated in the spawned task; the caller seeds it back).
    pub final_history: Vec<Message>,
    /// Compaction summary produced during the turn (for streaming — the
    /// caller seeds it back so compaction is cumulative across turns).
    pub final_summary: Option<String>,
    /// Findings from the turn-boundary self-audit (see [`crate::audit`]).
    /// Diagnostic: a non-zero count never fails the turn.
    pub self_audit_findings: usize,
}

/// A tool call carried in a [`StreamEvent::CompletionResponse`].
#[derive(Debug, Clone)]
pub struct StreamToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Events emitted during a streaming agent turn.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Delta of assistant text.
    TextDelta { text: String },
    /// Model has started a tool call (name known, arguments assembling).
    ToolCallStart { id: String, name: String },
    /// Fragment of tool call arguments (partial JSON).
    ToolCallArgs { id: String, args_delta: String },
    /// Tool call is complete and about to be executed.
    ToolCallReady {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// Tool has executed successfully.
    ToolResult {
        id: String,
        name: String,
        /// Truncated preview of the result (for inline display).
        result_preview: String,
        /// Full rendered result (for session-log persistence).
        result: String,
    },
    /// Context was compacted before the upcoming completion request.
    Compaction {
        ts: chrono::DateTime<Utc>,
        before_tokens: usize,
        after_tokens: usize,
        summary: String,
        dropped_messages: usize,
        kept_messages: usize,
        summarization_usage: Option<UsageData>,
    },
    /// A completion request was assembled and is about to be sent.
    CompletionRequest {
        ts: chrono::DateTime<Utc>,
        messages: Vec<ContextMessage>,
        tool_definitions: serde_json::Value,
    },
    /// A streaming completion response finished.
    CompletionResponse {
        ts: chrono::DateTime<Utc>,
        message_id: Option<String>,
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<StreamToolCall>>,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Turn completed successfully.
    TurnComplete { summary: TurnSummary },
    /// The turn's assistant text made a checkable claim that the turn's own
    /// tool activity does not support. Diagnostic only: the turn still
    /// completes.
    ///
    /// Emitted immediately before [`StreamEvent::TurnComplete`], so a consumer
    /// that stops reading on completion has still seen the finding.
    SelfAudit {
        finding: crate::audit::SelfAuditFinding,
    },
    /// An error occurred during the turn.
    ///
    /// `kind` names the failure class so a caller can react to it without
    /// re-parsing `message`; the message stays the human-facing rendering.
    Error {
        message: String,
        kind: TurnErrorKind,
    },
}

/// Why a streaming turn aborted.
///
/// Carried alongside [`StreamEvent::Error::message`] because the loop already
/// knows the concrete [`AgentError`] it is about to return — rendering it to a
/// string and dropping the structure loses information the caller needs (for
/// example, whether a tool-call limit or an unknown tool caused the abort).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnErrorKind {
    /// A tool name with no registered tool — a harness/dispatch error.
    UnknownTool,
    /// A tool call was executed after the per-turn bound was already spent.
    ToolCallLimit { calls: usize },
    /// The same `(name, args)` tool call failed `count` times in a row.
    RepeatedToolFailure { name: String, count: usize },
    /// No terminal event before the per-turn model-turn bound.
    MaxTurns { turns: usize },
    /// The model provider or stream failed.
    Provider,
    /// Writing or re-opening the session log failed.
    LogWrite,
}
impl StreamEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::TextDelta { .. } => "TextDelta",
            Self::ToolCallStart { .. } => "ToolCallStart",
            Self::ToolCallArgs { .. } => "ToolCallArgs",
            Self::ToolCallReady { .. } => "ToolCallReady",
            Self::ToolResult { .. } => "ToolResult",
            Self::Compaction { .. } => "Compaction",
            Self::CompletionRequest { .. } => "CompletionRequest",
            Self::CompletionResponse { .. } => "CompletionResponse",
            Self::TurnComplete { .. } => "TurnComplete",
            Self::SelfAudit { .. } => "SelfAudit",
            Self::Error { .. } => "Error",
        }
    }
}
/// Metadata for one actual compaction, produced by [`compact_if_needed`] and
/// carried to the session log via [`PreparedContext`] (non-streaming) or
/// [`StreamEvent::Compaction`] (streaming).
pub(crate) struct CompactionRecord {
    pub(crate) before_tokens: usize,
    pub(crate) after_tokens: usize,
    pub(crate) summary: String,
    pub(crate) dropped_messages: usize,
    pub(crate) kept_messages: usize,
    pub(crate) summarization_usage: Option<UsageData>,
}

/// A prepared completion context: exactly what is sent to the model, plus the
/// log-shaped rendering of the same messages.
///
/// Produced once per model call by [`Agent::prepare_context`] so the logged
/// prompt and the sent prompt can never diverge (including after compaction).
pub(crate) struct PreparedContext {
    pub(crate) preamble: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) user_input: String,
    pub(crate) log_messages: Vec<ContextMessage>,
    /// Set when preparing this context triggered a compaction, so the session
    /// layer can log a `compaction` entry immediately before the `llm_prompt`
    /// it caused.
    pub(crate) compaction: Option<CompactionRecord>,
}

/// Connect to an OpenAI-compatible chat completions endpoint (e.g. DeepSeek)
/// and return an agent bound to it.
///
/// The concrete model type is hidden behind `impl CompletionModel` so callers
/// never need to name it.
pub fn connect_chat_model(
    base_url: String,
    api_key: String,
    model: String,
) -> Result<Agent<impl CompletionModel + use<>>, AgentError> {
    connect_chat_model_with_prompt(base_url, api_key, model, DEFAULT_SYSTEM_PROMPT)
}

/// Like [`connect_chat_model`], but with a custom system prompt (e.g. the
/// task-mode prompt for self-improvement work).
pub fn connect_chat_model_with_prompt(
    base_url: String,
    api_key: String,
    model: String,
    system_prompt: &str,
) -> Result<Agent<impl CompletionModel + use<>>, AgentError> {
    let client = openai::CompletionsClient::builder()
        .api_key(&api_key)
        .base_url(&base_url)
        .build()
        .map_err(|e| AgentError::Rig(e.to_string()))?;
    let context_limit = context_limit_for(&model);
    let model = client.completion_model(&model);
    eprintln!("{}:{}: Created model", file!(), line!(),);
    let mut agent = Agent::new(model, system_prompt);
    agent.context_limit_tokens = context_limit;
    Ok(agent)
}

/// A minimal tool-calling agent.
///
/// Generic over the rig [`CompletionModel`]; unit tests use rig's
/// `MockCompletionModel`, production uses the OpenAI-compatible client from
/// [`connect_chat_model`].
pub struct Agent<M: CompletionModel> {
    model: M,
    system_prompt: String,
    tools: Arc<Vec<PortableDynamicTool>>,
    history: Vec<Message>,
    max_turns: usize,
    tool_cache: Arc<Mutex<ToolCallCache>>,
    /// Per-model input budget (tokens). Defaults to 1M.
    context_limit_tokens: usize,
    /// In-memory compaction summary, injected into the effective preamble.
    summary: Option<String>,
}

impl<M: CompletionModel> Agent<M> {
    /// Create an agent with no tools and an empty history.
    pub fn new(model: M, system_prompt: impl Into<String>) -> Self {
        Self {
            model,
            system_prompt: system_prompt.into(),
            tools: Arc::new(Vec::new()),
            history: Vec::new(),
            max_turns: 10,
            tool_cache: Arc::new(Mutex::new(ToolCallCache::default())),
            context_limit_tokens: DEFAULT_CONTEXT_LIMIT_TOKENS,
            summary: None,
        }
    }

    /// The system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Register a tool. The agent exposes it to the model on the next turn.
    pub fn add_tool(&mut self, tool: PortableDynamicTool) {
        Arc::make_mut(&mut self.tools).push(tool);
    }

    /// Clone of the shared tool registry (for use in spawned tasks).
    fn tools_arc(&self) -> Arc<Vec<PortableDynamicTool>> {
        self.tools.clone()
    }

    /// Tool inventory (name, description, JSON schema).
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// Number of registered tools.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Persistent message history (prior turns only).
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Maximum model turns per `run` invocation.
    pub fn max_turns(&self) -> usize {
        self.max_turns
    }

    /// Set the maximum model turns per `run` invocation.
    pub fn set_max_turns(&mut self, max_turns: usize) {
        self.max_turns = max_turns;
    }

    /// Current per-model input token budget.
    pub fn context_limit_tokens(&self) -> usize {
        self.context_limit_tokens
    }

    /// Override the per-model input token budget (tests and future CLI flag).
    pub fn set_context_limit_tokens(&mut self, limit: usize) {
        self.context_limit_tokens = limit;
    }

    /// Current compaction summary, if any.
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Replace the in-memory compaction summary (seeds a streaming turn).
    pub fn set_summary(&mut self, summary: Option<String>) {
        self.summary = summary;
    }

    /// Clear the persistent message history.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    // The per-turn read-only tool cache is reset by `run_streaming_loop` at
    // the start of every turn; there is no separate public entry point.

    /// Replace the persistent message history (e.g. from session replay).
    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// System preamble actually sent: base prompt plus, after compaction, a
    /// clearly delimited summary of the conversation so far.
    pub(crate) fn effective_preamble(&self) -> String {
        build_preamble(&self.system_prompt, self.summary.as_deref())
    }

    /// Prepare one completion context: compact if needed, then build the
    /// exact message list that is both sent and logged.
    pub(crate) async fn prepare_context(
        &mut self,
        user_input: &str,
        turn_messages: &[Message],
    ) -> Result<PreparedContext, AgentError> {
        let tool_defs_json = serde_json::to_string(&self.tool_definitions())?;

        let compaction = compact_if_needed(
            &self.model,
            &self.system_prompt,
            &mut self.history,
            &mut self.summary,
            self.context_limit_tokens,
            user_input,
            turn_messages,
            &tool_defs_json,
        )
        .await?;

        let preamble = self.effective_preamble();
        let messages: Vec<Message> = self
            .history
            .iter()
            .chain(turn_messages.iter())
            .cloned()
            .collect();
        let log_messages = build_context(&preamble, &self.history, turn_messages, user_input);

        Ok(PreparedContext {
            preamble,
            messages,
            user_input: user_input.to_string(),
            log_messages,
            compaction,
        })
    }

    /// Send the already-prepared context to the model.
    pub(crate) async fn single_completion_with(
        &self,
        prepared: &PreparedContext,
    ) -> Result<rig::completion::CompletionResponse<M::Response>, AgentError> {
        let request = self
            .model
            .completion_request(Message::user(prepared.user_input.clone()))
            .preamble(prepared.preamble.clone())
            .messages(prepared.messages.iter().cloned())
            .tools(self.tool_definitions())
            .build();
        let response = self.model.completion(request).await?;
        Ok(response)
    }

    /// Run one user request to completion: model calls, tool execution, and
    /// follow-up model calls until the model answers without tool calls.
    ///
    /// Deprecated: the CLI has used [`Agent::run_streaming`] exclusively since
    /// the streaming handler landed, so this loop was unexercised production
    /// code that had already drifted. It is stubbed rather than repaired so a
    /// caller cannot silently get a plausible-looking result from a path no
    /// one drives. Returning an empty `TurnSummary` here would be exactly the
    /// silent-success defect that motivated the stub.
    #[deprecated(note = "the non-streaming agent loop is unused; use `run_streaming`. \
		See plans/029-tool-failure-recovery.md")]
    pub async fn run(&mut self, user_input: &str) -> Result<TurnSummary, AgentError> {
        let _ = user_input;
        Err(AgentError::Deprecated {
            message: "Agent::run is deprecated; use Agent::run_streaming".into(),
        })
    }

    /// Run one user turn with streaming output.
    ///
    /// Returns a channel receiver. The caller reads events as they arrive.
    /// The agent loop runs on a spawned task. After the stream completes,
    /// the caller should extract `final_history` and `final_summary` from the
    /// `TurnComplete` event and call `set_history` / `set_summary` to persist
    /// the new state.
    pub async fn run_streaming(
        &self,
        user_input: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, AgentError>
    where
        M: 'static,
        M::StreamingResponse: Send,
    {
        let (tx, rx) = mpsc::channel(32);
        let model = self.model.clone();
        let tools = self.tools_arc();
        let tool_cache = self.tool_cache.clone();
        let max_turns = self.max_turns;
        let system_prompt = self.system_prompt.clone();
        let context_limit_tokens = self.context_limit_tokens;
        let summary = self.summary.clone();
        let mut history = self.history.clone();
        let user_input = user_input.to_string();

        tokio::spawn(async move {
            let result = run_streaming_loop(
                model,
                tools,
                &mut history,
                user_input,
                system_prompt,
                context_limit_tokens,
                summary,
                max_turns,
                &tx,
                tool_cache,
            )
            .await;
            // The agent's persistent history is returned to the caller via
            // `TurnComplete.final_history`; the caller seeds it back.
            let _ = result;
        });

        Ok(rx)
    }

    /// Dispatch a single tool call by name and render its output as text.
    pub(crate) async fn execute_tool(&self, call: &ToolCall) -> Result<String, AgentError> {
        execute_tool_with_cache(
            self.tools.as_slice(),
            &self.tool_cache,
            &call.function.name,
            call.function.arguments.clone(),
        )
        .await
    }

    /// Append the completed turn to the persistent history.
    pub(crate) fn commit_turn(&mut self, user_input: &str, turn_messages: Vec<Message>) {
        self.history.push(Message::user(user_input));
        self.history.extend(turn_messages);
    }
}

/// Build the effective system preamble: base prompt plus the compaction
/// summary block when one exists.
fn build_preamble(system_prompt: &str, summary: Option<&str>) -> String {
    match summary {
        Some(summary) => {
            format!("{system_prompt}\n\n## Summary of the conversation so far:\n{summary}")
        }
        None => system_prompt.to_string(),
    }
}

/// The build/verify workflow paragraph for `toolchain`, appended to the
/// invariant preamble.
///
/// One `&'static str` per variant: this runs once per model request, so the
/// text is borrowed rather than allocated. `Toolchain::Unknown` names no
/// build tool at all, so a model in an unrecognised repository reports the
/// gap instead of guessing at `make` or `./configure`.
fn verification_preamble(toolchain: Toolchain) -> &'static str {
    match toolchain {
        Toolchain::Cargo => CARGO_VERIFICATION_PREAMBLE,
        Toolchain::CMake => CMAKE_VERIFICATION_PREAMBLE,
        Toolchain::Unknown => UNKNOWN_VERIFICATION_PREAMBLE,
    }
}

/// The task-mode system prompt for a repository using `toolchain`: the
/// invariant task preamble, the per-toolchain verification workflow, then the
/// toolchain-independent suffix.
pub fn task_system_prompt(toolchain: Toolchain) -> String {
    format!(
        "{TASK_SYSTEM_PROMPT}{}{TASK_SYSTEM_PROMPT_SUFFIX}",
        verification_preamble(toolchain)
    )
}

#[allow(clippy::too_many_arguments)]
/// Compact history when the estimated input exceeds the budget.
///
/// Returns `Some(record)` when a summarization call was made and
/// `history`/`summary` were updated. Under budget, or when there is no old
/// history to summarize (a single dominating recent turn), it returns `None`
/// and leaves state untouched — callers then fall through to the tool-output
/// truncation path.
async fn compact_if_needed<M: CompletionModel>(
    model: &M,
    system_prompt: &str,
    history: &mut Vec<Message>,
    summary: &mut Option<String>,
    context_limit_tokens: usize,
    user_input: &str,
    turn_messages: &[Message],
    tool_defs_json: &str,
) -> Result<Option<CompactionRecord>, AgentError> {
    let limit = context_limit_tokens.saturating_sub(RESERVE_OUTPUT_TOKENS);
    let before = estimate_context(
        system_prompt,
        summary.as_deref(),
        history,
        turn_messages,
        user_input,
        tool_defs_json,
    );
    eprintln!(
        "{}:{}: context before: {before} {:0.0}%",
        file!(),
        line!(),
        100_f32 * before as f32 / limit as f32
    );
    if before <= limit {
        return Ok(None);
    }

    let (old, kept) = split_history(history);
    if old.is_empty() {
        return Ok(None);
    }

    let mut prompt = String::from(COMPACTION_PROMPT);
    if let Some(previous) = summary.as_deref() {
        prompt.push_str("\n\nPrevious summary:\n");
        prompt.push_str(previous);
    }
    prompt.push_str("\n\nConversation to summarize:\n");
    prompt.push_str(&serialize_for_summary(old));

    let request = model
        .completion_request(Message::user(prompt))
        .preamble(system_prompt.to_string())
        .build();
    let response = model.completion(request).await?;

    let mut text = String::new();
    for content in response.choice.iter() {
        if let AssistantContent::Text(t) = content {
            text.push_str(t.text());
        }
    }

    let new_summary = truncate_to_token_budget(&text, MAX_SUMMARY_TOKENS);
    let summarization_usage = UsageData {
        input_tokens: response.usage.input_tokens as u32,
        output_tokens: response.usage.output_tokens as u32,
    };
    let dropped_messages = old.len();
    let kept_messages = kept.len();

    *summary = Some(new_summary.clone());
    *history = kept.to_vec();

    let after = estimate_context(
        system_prompt,
        summary.as_deref(),
        history,
        turn_messages,
        user_input,
        tool_defs_json,
    );
    eprintln!("{}:{}: context after: {after}", file!(), line!(),);
    tracing::info!(
        before_tokens = before,
        after_tokens = after,
        "compacted conversation context"
    );

    Ok(Some(CompactionRecord {
        before_tokens: before,
        after_tokens: after,
        summary: new_summary,
        dropped_messages,
        kept_messages,
        summarization_usage: Some(summarization_usage),
    }))
}

/// Build the typed message list for one completion request.
///
/// Order is the order the model sees: system preamble, persistent history,
/// in-progress turn messages, then the current user input. Serialization is
/// deferred to the log writer; see [`ContextMessage`].
pub(crate) fn build_context(
    system_prompt: &str,
    history: &[Message],
    turn_messages: &[Message],
    user_input: &str,
) -> Vec<ContextMessage> {
    let mut msgs: Vec<ContextMessage> = Vec::new();

    // System preamble.
    msgs.push(ContextMessage::System(system_prompt.to_string()));

    for m in history.iter().chain(turn_messages.iter()) {
        msgs.push(ContextMessage::Message(Box::new(m.clone())));
    }

    // Current user message.
    msgs.push(ContextMessage::User(user_input.to_string()));

    msgs
}

/// Execute one tool call, reusing cached results for repeated identical
/// read-only calls within the same turn.
async fn execute_tool_with_cache(
    tools: &[PortableDynamicTool],
    cache: &Mutex<ToolCallCache>,
    name: &str,
    args: serde_json::Value,
) -> Result<String, AgentError> {
    {
        let mut cache = cache.lock().expect("tool call cache lock");

        match cache.lookup(name, &args) {
            ToolCallCacheLookup::Hit(rendered) => return Ok(rendered),
            ToolCallCacheLookup::DuplicateLimit => {
                let target = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| name.to_string());
                return Ok(format!(
                    "duplicate read of {target} this turn: reuse the earlier result, or vary the arguments (for example request a different offset/limit range)"
                ));
            }
            ToolCallCacheLookup::Miss => {}
        }
    }

    let tool = tools
        .iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| AgentError::Tool {
            name: name.to_string(),
            message: "unknown tool".into(),
        })?;
    let output = tool
        .execute(args.clone())
        .await
        .map_err(|e| AgentError::Tool {
            name: name.to_string(),
            message: e.to_string(),
        })?;

    // Backstop: never feed an unbounded tool result back to the model. The
    // per-tool caps remain the primary policy; this is the last line.
    let rendered = truncate_tool_output(&output.render());

    let mut cache = cache.lock().expect("tool call cache lock");
    cache.store(name, &args, &rendered);
    cache.invalidate_on_write(name);
    Ok(rendered)
}

#[allow(clippy::too_many_arguments)]
/// The inner streaming loop, run on a spawned task.
///
/// Consumes the model stream, executes tools when complete tool calls
/// arrive, and sends [`StreamEvent`]s to the caller.
async fn run_streaming_loop<M: CompletionModel>(
    model: M,
    tools: Arc<Vec<PortableDynamicTool>>,
    history: &mut Vec<Message>,
    user_input: String,
    system_prompt: String,
    context_limit_tokens: usize,
    mut summary: Option<String>,
    max_turns: usize,
    tx: &mpsc::Sender<StreamEvent>,
    tool_cache: Arc<Mutex<ToolCallCache>>,
) -> Result<TurnSummary, AgentError>
where
    M::StreamingResponse: Send,
{
    tool_cache.lock().expect("tool call cache lock").reset();
    let mut turn_messages: Vec<Message> = Vec::new();
    let mut tool_calls_total: usize = 0;
    let mut usage_total = Usage::new();
    // Every tool call made this turn, and every one that failed, accumulated
    // across all model turns. `pending_tool_calls` and `pending_results` are
    // per model call, so a call from an earlier model turn would be gone by
    // the time the turn boundary audits the text.
    let mut turn_calls: Vec<ToolCall> = Vec::new();
    let mut turn_failures: Vec<ToolCall> = Vec::new();

    for _turn in 0..max_turns {
        eprintln!("{}:{}: turn:{_turn}", file!(), line!(),);
        // Build the request and report it before sending.
        let tool_definitions = tools.iter().map(|t| t.definition()).collect::<Vec<_>>();
        let tool_definitions_json = serde_json::to_value(&tool_definitions)?;
        let tool_defs_json_str = tool_definitions_json.to_string();

        // One token check + possible compaction before every call.
        if let Some(record) = compact_if_needed(
            &model,
            &system_prompt,
            history,
            &mut summary,
            context_limit_tokens,
            &user_input,
            &turn_messages,
            &tool_defs_json_str,
        )
        .await?
        {
            eprintln!(
                "{}:{}: Compaction {} -> {}",
                file!(),
                line!(),
                record.before_tokens,
                record.after_tokens
            );
            let _ = tx
                .send(StreamEvent::Compaction {
                    ts: Utc::now(),
                    before_tokens: record.before_tokens,
                    after_tokens: record.after_tokens,
                    summary: record.summary,
                    dropped_messages: record.dropped_messages,
                    kept_messages: record.kept_messages,
                    summarization_usage: record.summarization_usage,
                })
                .await;
        }

        let preamble = build_preamble(&system_prompt, summary.as_deref());
        let messages = build_context(&preamble, history, &turn_messages, &user_input);
        let _ = tx
            .send(StreamEvent::CompletionRequest {
                ts: Utc::now(),
                messages,
                tool_definitions: tool_definitions_json,
            })
            .await;

        let request = model
            .completion_request(Message::user(user_input.clone()))
            .preamble(preamble)
            .messages(history.iter().chain(turn_messages.iter()).cloned())
            .tools(tool_definitions)
            .build();

        let mut stream = model
            .stream(request)
            .await
            .map_err(|e| AgentError::Rig(e.to_string()))?;

        // Track tool calls, their results, and per-call text/reasoning/usage
        // during this model call.
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
        let mut pending_results: Vec<(ToolCall, String)> = Vec::new();
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut call_usage = Usage::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::streaming::StreamedAssistantContent::Text(t)) => {
                    let delta = t.text().to_string();
                    text_buf.push_str(&delta);
                    let _ = tx.send(StreamEvent::TextDelta { text: delta }).await;
                }
                Ok(rig::streaming::StreamedAssistantContent::ToolCallDelta {
                    id, content, ..
                }) => {
                    use rig::streaming::ToolCallDeltaContent;
                    match content {
                        ToolCallDeltaContent::Name(name) => {
                            let _ = tx
                                .send(StreamEvent::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await;
                        }
                        ToolCallDeltaContent::Delta(args) => {
                            let _ = tx
                                .send(StreamEvent::ToolCallArgs {
                                    id: id.clone(),
                                    args_delta: args,
                                })
                                .await;
                        }
                    }
                }
                Ok(rig::streaming::StreamedAssistantContent::ToolCall { tool_call, .. }) => {
                    let _ = tx
                        .send(StreamEvent::ToolCallReady {
                            id: tool_call.id.clone(),
                            name: tool_call.function.name.clone(),
                            arguments: tool_call.function.arguments.clone(),
                        })
                        .await;

                    // Hard guard: bound the number of tool calls per turn.
                    if tool_calls_total >= MAX_TOOL_CALLS_PER_TURN {
                        tracing::error!(
                            calls = tool_calls_total,
                            "tool call limit exceeded in one turn"
                        );
                        let _ = tx
                            .send(StreamEvent::Error {
                                message: format!(
                                    "tool call limit exceeded: {tool_calls_total} calls in one turn"
                                ),
                                kind: TurnErrorKind::ToolCallLimit {
                                    calls: tool_calls_total,
                                },
                            })
                            .await;
                        return Err(AgentError::ToolCallLimit {
                            calls: tool_calls_total,
                        });
                    }

                    // Execute the tool, reusing cached results for repeated
                    // identical read-only calls within this turn.
                    let name = tool_call.function.name.clone();
                    match execute_tool_with_cache(
                        tools.as_slice(),
                        &tool_cache,
                        &name,
                        tool_call.function.arguments.clone(),
                    )
                    .await
                    {
                        Ok(rendered) => {
                            let preview = if rendered.len() > 200 {
                                format!("{}…", rendered.chars().take(200).collect::<String>())
                            } else {
                                rendered.clone()
                            };
                            // eprintln!(
                            //	"{}:{}: tool '{name}' returned {} chars",
                            //	file!(),
                            //	line!(),
                            //	rendered.len()
                            // );
                            let _ = tx
                                .send(StreamEvent::ToolResult {
                                    id: tool_call.id.clone(),
                                    name: name.clone(),
                                    result_preview: preview,
                                    result: rendered.clone(),
                                })
                                .await;
                            tool_calls_total += 1;
                            // A success clears the failure count for this exact
                            // call, so fail-fix-fail does not accumulate.
                            tool_cache
                                .lock()
                                .expect("tool call cache lock")
                                .clear_failure(&name, &tool_call.function.arguments);
                            pending_results.push((tool_call.clone(), rendered));
                        }
                        Err(e) => {
                            eprintln!(
                                "{}:{}:execute_tool_with_cache error. name: {name} Error: {e}",
                                file!(),
                                line!(),
                            );

                            // An unknown tool name is a harness/dispatch error,
                            // not an execution failure the model can adapt to,
                            // so it stays fatal.
                            if matches!(&e, AgentError::Tool { message, .. } if message == "unknown tool")
                            {
                                let _ = tx
                                    .send(StreamEvent::Error {
                                        message: e.to_string(),
                                        kind: TurnErrorKind::UnknownTool,
                                    })
                                    .await;
                                return Err(e);
                            }

                            // Surface the failure as information: render it as
                            // a tool result, feed it back, and let the model
                            // try the next viable means.
                            let rendered = format!("{TOOL_FAILURE_PREFIX}: {name}\n  error: {e}");
                            let _ = tx
                                .send(StreamEvent::ToolResult {
                                    id: tool_call.id.clone(),
                                    name: name.clone(),
                                    result_preview: rendered.clone(),
                                    result: rendered.clone(),
                                })
                                .await;

                            // A failed execution is still an execution, so it
                            // counts against the per-turn bound.
                            tool_calls_total += 1;
                            pending_results.push((tool_call.clone(), rendered));
                            turn_failures.push(tool_call.clone());

                            let give_up = {
                                let mut cache = tool_cache.lock().expect("tool call cache lock");
                                cache.record_failure(&name, &tool_call.function.arguments);
                                cache.notable_for_failure(&name, &tool_call.function.arguments)
                            };
                            if let FailureLookup::GiveUp { count } = give_up {
                                let _ = tx
                                    .send(StreamEvent::Error {
                                        message: AgentError::RepeatedToolFailure {
                                            name: name.clone(),
                                            count,
                                        }
                                        .to_string(),
                                        kind: TurnErrorKind::RepeatedToolFailure {
                                            name: name.clone(),
                                            count,
                                        },
                                    })
                                    .await;
                                return Err(AgentError::RepeatedToolFailure { name, count });
                            }
                        }
                    }
                    pending_tool_calls.push(tool_call);
                    turn_calls.push(pending_tool_calls.last().expect("just pushed").clone());
                }
                Ok(rig::streaming::StreamedAssistantContent::Final(r)) => {
                    let usage = r.token_usage();
                    usage_total += usage;
                    call_usage = usage;
                }
                Ok(rig::streaming::StreamedAssistantContent::ReasoningDelta {
                    reasoning, ..
                }) => {
                    reasoning_buf.push_str(&reasoning);
                }
                Ok(rig::streaming::StreamedAssistantContent::Reasoning(r)) => {
                    if reasoning_buf.is_empty() {
                        reasoning_buf = r.display_text();
                    } else {
                        reasoning_buf.push('\n');
                        reasoning_buf.push_str(&r.display_text());
                    }
                }
                Ok(rig::streaming::StreamedAssistantContent::Unknown(_)) => {}
                Err(e) => {
                    eprintln!("{}:{}:execute_tool_with_cache error.", file!(), line!(),);
                    let _ = tx
                        .send(StreamEvent::Error {
                            message: e.to_string(),
                            kind: TurnErrorKind::Provider,
                        })
                        .await;
                    return Err(AgentError::Rig(e.to_string()));
                }
            }
        }

        // Capture message_id and report the completed response.
        let message_id = stream.message_id.clone();
        let response_text = (!text_buf.is_empty()).then(|| text_buf.clone());
        let response_reasoning = (!reasoning_buf.is_empty()).then(|| reasoning_buf.clone());
        let response_tool_calls = (!pending_tool_calls.is_empty()).then(|| {
            pending_tool_calls
                .iter()
                .map(|call| StreamToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                })
                .collect()
        });
        let _ = tx
            .send(StreamEvent::CompletionResponse {
                ts: Utc::now(),
                message_id: message_id.clone(),
                text: response_text,
                reasoning: response_reasoning,
                tool_calls: response_tool_calls,
                input_tokens: call_usage.input_tokens,
                output_tokens: call_usage.output_tokens,
            })
            .await;

        // If no tool calls were made, the turn is complete.
        if pending_tool_calls.is_empty() {
            let findings = audit::audit_turn(&text_buf, &turn_calls, &turn_failures);

            turn_messages.push(Message::assistant(text_buf.clone()));
            history.push(Message::user(user_input));
            history.extend(turn_messages);
            let turn_summary = TurnSummary {
                text: text_buf,
                tool_calls: tool_calls_total,
                usage: usage_total,
                final_history: history.clone(),
                final_summary: summary.clone(),
                self_audit_findings: findings.len(),
            };
            // Emit the audit before TurnComplete. The text is final here (no
            // partial deltas to misjudge), and a consumer that stops reading
            // on TurnComplete still receives the finding first.
            for finding in &findings {
                let _ = tx
                    .send(StreamEvent::SelfAudit {
                        finding: finding.clone(),
                    })
                    .await;
            }
            let _ = tx
                .send(StreamEvent::TurnComplete {
                    summary: turn_summary.clone(),
                })
                .await;
            return Ok(turn_summary);
        }

        // Tool calls were executed. Build the assistant message and loop.
        let mut contents: Vec<AssistantContent> = Vec::new();
        if !text_buf.is_empty() {
            contents.push(AssistantContent::Text(rig::completion::message::Text::new(
                text_buf,
            )));
        }
        contents.extend(
            pending_tool_calls
                .iter()
                .cloned()
                .map(AssistantContent::ToolCall),
        );
        turn_messages.push(Message::Assistant {
            id: message_id,
            content: rig::OneOrMany::from_iter_optional(contents)
                .expect("assistant message has content"),
        });

        // Push tool results AFTER the assistant message.
        for (call, rendered) in pending_results {
            turn_messages.push(Message::tool_result_with_call_id(
                call.id,
                call.call_id,
                rendered,
            ));
        }
    }

    // Max turns exceeded.
    history.push(Message::user(user_input));
    history.extend(turn_messages);
    let _ = tx
        .send(StreamEvent::Error {
            message: format!("exceeded maximum of {max_turns} model turns"),
            kind: TurnErrorKind::MaxTurns { turns: max_turns },
        })
        .await;
    Err(AgentError::MaxTurns { turns: max_turns })
}

/// A mock model whose streaming script and plain-completion script are
/// independent.
///
/// `MockCompletionModel` keeps two separate script cursors — one for
/// `stream()` and one for `completion()` — and each constructor (`from_turns`,
/// `from_stream_turns`) fills in exactly one of them. A turn that compacts
/// needs both: `compact_if_needed` calls `completion()` for the summarization
/// while the turn itself is driven by `stream()`. This wrapper delegates each
/// entry point to its own mock so a test can script both at once.
#[cfg(test)]
pub(crate) struct SplitScriptModel {
    completion: rig::test_utils::MockCompletionModel,
    streaming: rig::test_utils::MockCompletionModel,
}

#[cfg(test)]
impl SplitScriptModel {
    /// Script `completion_turns` for plain completions (summarization) and
    /// `streaming_turns` for streamed turns (the agent loop).
    pub(crate) fn new(
        completion_turns: impl IntoIterator<Item = rig::test_utils::MockTurn>,
        streaming_turns: impl IntoIterator<
            Item = impl IntoIterator<Item = rig::test_utils::MockStreamEvent>,
        >,
    ) -> Self {
        Self {
            completion: rig::test_utils::MockCompletionModel::from_turns(completion_turns),
            streaming: rig::test_utils::MockCompletionModel::from_stream_turns(streaming_turns),
        }
    }
}

#[cfg(test)]
impl Clone for SplitScriptModel {
    fn clone(&self) -> Self {
        Self {
            completion: self.completion.clone(),
            streaming: self.streaming.clone(),
        }
    }
}

#[cfg(test)]
impl rig::completion::CompletionModel for SplitScriptModel {
    type Response =
        <rig::test_utils::MockCompletionModel as rig::completion::CompletionModel>::Response;
    type StreamingResponse =
	<rig::test_utils::MockCompletionModel as rig::completion::CompletionModel>::StreamingResponse;
    type Client =
        <rig::test_utils::MockCompletionModel as rig::completion::CompletionModel>::Client;

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self::new(
            [rig::test_utils::MockTurn::text("unused")],
            [vec![
                rig::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]],
        )
    }

    async fn completion(
        &self,
        request: rig::completion::CompletionRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, rig::completion::CompletionError>
    {
        self.completion.completion(request).await
    }

    async fn stream(
        &self,
        request: rig::completion::CompletionRequest,
    ) -> Result<
        rig::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
        rig::completion::CompletionError,
    > {
        self.streaming.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};

    /// Build a mock model from streaming turns, one inner slice per model call.
    ///
    /// Every call must end with `final_response_with_default_usage`: the
    /// streaming driver only closes a turn on that chunk, so a slice without it
    /// leaves the turn hanging and the caller panics.
    fn stream_model(turns: impl IntoIterator<Item = Vec<MockStreamEvent>>) -> MockCompletionModel {
        MockCompletionModel::from_stream_turns(turns)
    }

    /// A single model call that answers with `text` and nothing else.
    fn text_turn(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }

    /// A single model call that requests one tool with the given arguments.
    fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::tool_call(id, name, arguments),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }

    /// Outcome of draining one streaming turn, mirroring what a real caller
    /// sees: either the turn completed (with its summary) or it aborted.
    #[derive(Debug)]
    enum TurnOutcome {
        Complete(TurnSummary),
        Aborted(AgentError),
    }

    /// Drive one turn on the streaming path and drain it to completion.
    ///
    /// The non-streaming entry point is deprecated, so every test that used it
    /// goes through here instead. The agent's history and summary are seeded
    /// back on `TurnComplete`, matching what the CLI and eval runner do.
    async fn run_turn<M>(agent: &mut Agent<M>, input: &str) -> TurnOutcome
    where
        M: rig::completion::CompletionModel + 'static,
        M::StreamingResponse: Send,
    {
        let mut rx = agent
            .run_streaming(input)
            .await
            .expect("streaming turn starts");

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TurnComplete { summary } => {
                    agent.set_history(summary.final_history.clone());
                    agent.set_summary(summary.final_summary.clone());
                    return TurnOutcome::Complete(summary);
                }
                StreamEvent::Error { message, kind } => {
                    return TurnOutcome::Aborted(match kind {
                        TurnErrorKind::UnknownTool => AgentError::Tool {
                            name: String::from("unknown"),
                            message,
                        },
                        TurnErrorKind::ToolCallLimit { calls } => {
                            AgentError::ToolCallLimit { calls }
                        }
                        TurnErrorKind::RepeatedToolFailure { name, count } => {
                            AgentError::RepeatedToolFailure { name, count }
                        }
                        TurnErrorKind::MaxTurns { turns } => AgentError::MaxTurns { turns },
                        TurnErrorKind::Provider | TurnErrorKind::LogWrite => {
                            AgentError::Rig(message)
                        }
                    });
                }
                _ => {}
            }
        }

        panic!("streaming turn ended without TurnComplete or Error");
    }

    /// Drive one streaming turn, collecting every event until the channel
    /// closes. Unlike `run_turn` this keeps going past `TurnComplete`, so a
    /// test can assert on ordering.
    async fn collect_events<M>(agent: &mut Agent<M>, input: &str) -> Vec<StreamEvent>
    where
        M: rig::completion::CompletionModel + 'static,
        M::StreamingResponse: Send,
    {
        let mut rx = agent
            .run_streaming(input)
            .await
            .expect("streaming turn starts");
        let mut events = Vec::new();

        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        events
    }

    /// An agent with one text-only turn scripted, for the audit tests.
    fn text_agent(text: &str) -> Agent<MockCompletionModel> {
        Agent::new(stream_model([text_turn(text)]), "test system")
    }

    /// The audit is a diagnostic, so a contradicting turn must still complete
    /// and report the finding. This is the load-bearing test: it pins the
    /// non-goal that the heuristic never kills a run.
    #[tokio::test]
    async fn a_self_audit_finding_does_not_fail_the_turn() {
        let mut agent = text_agent("I don't have the write tools.");

        let summary = match run_turn(&mut agent, "write a file").await {
            TurnOutcome::Complete(summary) => summary,
            TurnOutcome::Aborted(e) => panic!("the audit must not abort the turn: {e}"),
        };

        assert_eq!(
            summary.self_audit_findings, 1,
            "the finding must be counted on the summary"
        );
    }

    /// A consumer that reads until the channel closes must see the finding
    /// before the turn-complete event.
    #[tokio::test]
    async fn a_self_audit_event_precedes_turn_complete() {
        let mut agent = text_agent("I don't have the write tools.");
        let events = collect_events(&mut agent, "write a file").await;
        let types: Vec<&str> = events.iter().map(StreamEvent::type_name).collect();

        let audit_at = types
            .iter()
            .position(|t| *t == "SelfAudit")
            .expect("an audit event must be emitted");
        let complete_at = types
            .iter()
            .position(|t| *t == "TurnComplete")
            .expect("the turn must complete");

        assert!(
            audit_at < complete_at,
            "the audit must arrive before completion: {types:?}"
        );
    }

    #[tokio::test]
    async fn a_self_audit_event_carries_the_offending_sentence() {
        let mut agent = text_agent("I don't have the write tools.");
        let events = collect_events(&mut agent, "write a file").await;

        let finding = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::SelfAudit { finding } => Some(finding),
                _ => None,
            })
            .expect("an audit event must be emitted");

        assert_eq!(
            finding.kind,
            crate::audit::SelfAuditKind::UnsupportedCapabilityClaim
        );
        assert_eq!(finding.detail, "I don't have the write tools");
    }

    /// A clean turn emits no audit event and counts no finding.
    #[tokio::test]
    async fn a_clean_turn_reports_no_finding() {
        let mut agent = text_agent("I read the file and it looks fine.");
        let events = collect_events(&mut agent, "read the file").await;

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::SelfAudit { .. })),
            "a clean turn must not be audited"
        );
        let Some(StreamEvent::TurnComplete { summary }) = events.last() else {
            panic!("the turn must complete");
        };
        assert_eq!(summary.self_audit_findings, 0);
    }

    /// A turn whose tool call genuinely failed is not audited: the claim of
    /// unavailability has a real failure to cite, so the check stays silent.
    #[tokio::test]
    async fn a_turn_with_a_real_tool_failure_is_not_audited() {
        let model = stream_model([
            tool_turn("call_1", "always_fails", serde_json::json!({})),
            text_turn("I don't have the write tools."),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(failing_tool());

        let summary = match run_turn(&mut agent, "do the work").await {
            TurnOutcome::Complete(summary) => summary,
            TurnOutcome::Aborted(e) => panic!("the turn must complete: {e}"),
        };

        assert_eq!(
            summary.self_audit_findings, 0,
            "a real failure grounds the claim"
        );
    }

    fn get_time_agent() -> Agent<MockCompletionModel> {
        let mut agent = Agent::new(stream_model([text_turn("unused")]), "test system");
        agent.add_tool(crate::tool::builtin_get_time());
        agent
    }

    #[tokio::test]
    async fn test_simple_text_reply() {
        let mut agent = get_time_agent();
        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "hi").await else {
            panic!("turn must complete");
        };
        assert_eq!(summary.text, "unused");
        assert_eq!(summary.tool_calls, 0);
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_round_trip() {
        let model = stream_model([
            tool_turn("call_1", "get_time", serde_json::json!({})),
            text_turn("got the time"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(crate::tool::builtin_get_time());

        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "what time is it").await else {
            panic!("turn must complete");
        };
        assert_eq!(summary.text, "got the time");
        assert_eq!(summary.tool_calls, 1);

        let history = agent.history();
        assert_eq!(history.len(), 4);
    }

    #[tokio::test]
    async fn test_unknown_tool_fails() {
        let model = stream_model([
            tool_turn("call_1", "nonexistent", serde_json::json!({})),
            text_turn("unused"),
        ]);
        let mut agent = Agent::new(model, "test system");
        // An unknown tool name is a harness/dispatch error, not an execution
        // failure the model can adapt to, so it stays fatal.
        let TurnOutcome::Aborted(err) = run_turn(&mut agent, "do it").await else {
            panic!("unknown tool must abort the turn");
        };
        assert!(matches!(err, AgentError::Tool { .. }));
    }

    #[tokio::test]
    async fn test_under_budget_does_not_compact() {
        let model = stream_model([text_turn("answer")]);
        let mut agent = Agent::new(model, "test system");
        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "hi").await else {
            panic!("turn must complete");
        };
        assert_eq!(summary.text, "answer");
        assert!(agent.summary().is_none());
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_over_budget_compacts_history() {
        // Compaction calls `completion()` while the turn itself is driven by
        // `stream()`, so the two scripts are independent: a plain
        // summarization turn and one streamed answer turn.
        let model = SplitScriptModel::new(
            [MockTurn::text("compacted summary")],
            [text_turn("final answer")],
        );
        let mut agent = Agent::new(model, "test system");
        agent.set_context_limit_tokens(500);

        let mut history = Vec::new();
        for i in 0..20 {
            history.push(Message::user(format!("question {i}")));
            history.push(Message::assistant(format!(
                "answer {i} {}",
                "x".repeat(200)
            )));
        }
        agent.set_history(history);
        let original_len = agent.history().len();

        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "hi").await else {
            panic!("turn must complete");
        };
        assert_eq!(summary.text, "final answer");
        assert!(agent.summary().is_some());
        assert!(agent.history().len() < original_len);
    }

    #[tokio::test]
    async fn test_prepare_context_matches_log_messages() {
        let mut agent = Agent::new(MockCompletionModel::text("unused"), "sys");
        agent.set_history(vec![
            Message::user("earlier"),
            Message::assistant("earlier-a"),
        ]);
        let turn = vec![Message::assistant("partial")];
        let prepared = agent
            .prepare_context("now", &turn)
            .await
            .expect("prepare succeeds");

        assert_eq!(prepared.messages.len(), 3);
        assert_eq!(prepared.log_messages.len(), 5);
        assert!(prepared.compaction.is_none());
        assert_eq!(
            prepared.log_messages[0],
            ContextMessage::System("sys".into())
        );
        assert_eq!(
            prepared.log_messages[1],
            ContextMessage::Message(Box::new(Message::user("earlier")))
        );
        assert_eq!(
            prepared.log_messages[2],
            ContextMessage::Message(Box::new(Message::assistant("earlier-a")))
        );
        assert_eq!(
            prepared.log_messages[3],
            ContextMessage::Message(Box::new(Message::assistant("partial")))
        );
        assert_eq!(prepared.log_messages[4], ContextMessage::User("now".into()));
    }

    #[tokio::test]
    async fn test_prepare_context_under_budget_has_no_compaction_record() {
        let mut agent = Agent::new(MockCompletionModel::text("unused"), "sys");
        agent.set_history(vec![
            Message::user("earlier"),
            Message::assistant("earlier-a"),
        ]);
        let prepared = agent
            .prepare_context("now", &[])
            .await
            .expect("prepare succeeds");
        assert!(prepared.compaction.is_none());
    }

    #[tokio::test]
    async fn test_prepare_context_over_budget_returns_compaction_record() {
        // [summarize, answer]: the summarization completion runs first.
        let model = MockCompletionModel::from_turns([
            MockTurn::text("compacted summary"),
            MockTurn::text("final answer"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.set_context_limit_tokens(500);

        let mut history = Vec::new();
        for i in 0..20 {
            history.push(Message::user(format!("question {i}")));
            history.push(Message::assistant(format!(
                "answer {i} {}",
                "x".repeat(200)
            )));
        }
        agent.set_history(history);

        let prepared = agent
            .prepare_context("hi", &[])
            .await
            .expect("prepare succeeds");

        let record = prepared.compaction.expect("over budget must compact");
        assert!(record.before_tokens > record.after_tokens);
        assert!(record.dropped_messages > 0);
        assert!(record.kept_messages > 0);
        assert_eq!(record.summary, "compacted summary");
    }

    #[tokio::test]
    async fn test_tool_output_truncated_at_agent_layer() {
        let big = "x".repeat(70 * 1024);
        let big_for_tool = big.clone();
        let tool = PortableDynamicTool::new(
            "big_tool",
            "returns a big string",
            serde_json::json!({ "type": "object", "properties": {} }),
            move |_args: serde_json::Value| {
                let big = big_for_tool.clone();
                Box::pin(async move { Ok(rig::tool::ToolOutput::text(big)) })
            },
        );
        let cache = Arc::new(Mutex::new(ToolCallCache::default()));
        let rendered = execute_tool_with_cache(
            std::slice::from_ref(&tool),
            &cache,
            "big_tool",
            serde_json::json!({}),
        )
        .await
        .expect("execute succeeds");
        assert!(
            rendered.contains("[truncated"),
            "got len {}",
            rendered.len()
        );
        assert!(rendered.len() < big.len());
    }

    #[tokio::test]
    async fn test_tool_call_limit_enforced() {
        let mut turns: Vec<Vec<MockStreamEvent>> = (0..=MAX_TOOL_CALLS_PER_TURN)
            .map(|i| tool_turn(&format!("call_{i}"), "get_time", serde_json::json!({})))
            .collect();
        turns.push(text_turn("done"));
        let model = stream_model(turns);
        let mut agent = Agent::new(model, "test system");
        agent.set_max_turns(200);
        agent.add_tool(crate::tool::builtin_get_time());

        let TurnOutcome::Aborted(err) = run_turn(&mut agent, "go").await else {
            panic!("tool call limit must abort the turn");
        };
        assert!(
            matches!(err, AgentError::ToolCallLimit { calls } if calls == MAX_TOOL_CALLS_PER_TURN)
        );
    }

    /// A read-only, deterministic tool whose results can be reused within a
    /// turn. Two identical calls on the same turn must execute the underlying
    /// tool only once.
    #[tokio::test]
    async fn test_duplicate_read_only_tool_call_is_deduplicated() {
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = executions.clone();

        // The tool is named after a real builtin ("read_file") so the
        // cacheable-tool whitelist applies, but the callback counts
        // executions without touching the filesystem.
        let counting_tool = PortableDynamicTool::new(
            "read_file",
            "A read-only tool that counts executions.",
            serde_json::json!({
            "type": "object",
            "properties": {
            "path": { "type": "string" }
            },
            "required": ["path"]
            }),
            move |args: serde_json::Value| {
                let counter = counter.clone();
                Box::pin(async move {
                    use std::sync::atomic::Ordering;
                    counter.fetch_add(1, Ordering::SeqCst);
                    use rig::tool::ToolOutput;
                    Ok(ToolOutput::text(
                        args.get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default(),
                    ))
                })
            },
        );

        let model = stream_model([
            tool_turn("call_1", "read_file", serde_json::json!({"path": "a"})),
            tool_turn("call_2", "read_file", serde_json::json!({"path": "a"})),
            text_turn("done"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(counting_tool);

        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "read the same file twice").await
        else {
            panic!("turn must complete");
        };
        // The two identical calls are deduplicated but still counted once.
        assert_eq!(summary.tool_calls, 2);
        use std::sync::atomic::Ordering;
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    /// A tool that always fails, so the failure feed-back path is exercised
    /// without touching the filesystem.
    fn failing_tool() -> PortableDynamicTool {
        PortableDynamicTool::new(
            "always_fails",
            "A tool whose execution always errors.",
            serde_json::json!({
            "type": "object",
            "properties": {},
            }),
            move |_args: serde_json::Value| {
                Box::pin(async move {
                    Err(rig::tool::ToolExecutionError::provider(
                        "deliberate failure",
                    ))
                })
            },
        )
    }

    /// One failing call followed by a text turn must not end the turn: the
    /// failure is reported back to the model as an ordinary tool result, so it
    /// can try the next viable means.
    #[tokio::test]
    async fn tool_failure_is_fed_back_and_the_turn_continues() {
        let model = stream_model([
            tool_turn("call_1", "always_fails", serde_json::json!({})),
            text_turn("recovered"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(failing_tool());

        let TurnOutcome::Complete(summary) = run_turn(&mut agent, "do it").await else {
            panic!("a single tool failure must not abort the turn");
        };
        assert_eq!(summary.text, "recovered");
        // The failed execution still counts as an execution.
        assert_eq!(summary.tool_calls, 1);

        let failed = agent
            .history()
            .iter()
            .filter_map(|m| match m {
                Message::User { content, .. } => Some(format!("{content:?}")),
                _ => None,
            })
            .find(|text| text.contains(TOOL_FAILURE_PREFIX))
            .expect("the failure must appear in the transcript as a tool result");
        assert!(
            failed.contains("deliberate failure"),
            "the model must see why it failed, got: {failed}"
        );
    }

    /// Repeating the identical failing call is a pathology, not adaptation: the
    /// turn ends after `REPEATED_FAILURE_LIMIT` attempts.
    #[tokio::test]
    async fn repeating_an_identical_failure_ends_the_turn() {
        let mut turns: Vec<Vec<MockStreamEvent>> = (0..REPEATED_FAILURE_LIMIT)
            .map(|i| tool_turn(&format!("call_{i}"), "always_fails", serde_json::json!({})))
            .collect();
        turns.push(text_turn("done"));
        let mut agent = Agent::new(stream_model(turns), "test system");
        agent.add_tool(failing_tool());

        let TurnOutcome::Aborted(err) = run_turn(&mut agent, "retry forever").await else {
            panic!("repeating an identical failure must abort the turn");
        };
        assert!(
            matches!(
            err,
            AgentError::RepeatedToolFailure { ref name, count }
            if name == "always_fails" && count == REPEATED_FAILURE_LIMIT
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn build_context_carries_system_history_and_user() {
        let history = vec![Message::user("earlier")];
        let turn_messages = vec![Message::assistant("partial")];
        let messages = build_context("system", &history, &turn_messages, "now");

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], ContextMessage::System("system".into()));
        assert_eq!(
            messages[1],
            ContextMessage::Message(Box::new(history[0].clone()))
        );
        assert_eq!(
            messages[2],
            ContextMessage::Message(Box::new(turn_messages[0].clone()))
        );
        assert_eq!(messages[3], ContextMessage::User("now".into()));
    }

    // ── toolchain-aware verification preamble ──

    /// The Rust workflow sentences must survive the split into
    /// `verification_preamble` unchanged. This literal is the pre-change
    /// text, copied verbatim; a failure here means the Rust variant drifted.
    #[test]
    fn preamble_rust_is_unchanged() {
        assert_eq!(
            verification_preamble(Toolchain::Cargo),
            "Workflow gates: run `cargo fmt` before staging changes; \
`cargo test` before committing; `cargo build` must pass; \
run `cargo clippy -- -D warnings` before finishing. Make changes as small git commits with \
descriptive messages. Never push."
        );
    }

    #[test]
    fn preamble_rust_names_cargo_workflow() {
        let text = verification_preamble(Toolchain::Cargo);
        for expected in [
            "cargo fmt",
            "cargo test",
            "cargo build",
            "cargo clippy -- -D warnings",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
    }

    #[test]
    fn preamble_cmake_names_cmake_workflow() {
        let text = verification_preamble(Toolchain::CMake);
        for expected in [
            "cmake -B build",
            "cmake --build build",
            "-fsyntax-only",
            "ctest",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
    }

    /// The point of this plan: a C++ prompt that names `cargo` sends the model
    /// after a tool the repository does not have.
    #[test]
    fn preamble_cmake_does_not_name_cargo() {
        let text = verification_preamble(Toolchain::CMake);
        assert!(!text.contains("cargo"), "C++ variant names cargo: {text:?}");
        assert!(
            !text.contains("clippy"),
            "C++ variant names clippy: {text:?}"
        );
    }

    /// `clang-format` and `clang-tidy` are not invocable — no formatter is
    /// admitted by the allowlist, and clang-tidy needs a compile database.
    /// If a future plan admits them, this fails deliberately.
    #[test]
    fn preamble_cmake_does_not_name_unavailable_linters() {
        let text = verification_preamble(Toolchain::CMake);
        for tool in ["clang-format", "clang-tidy"] {
            assert!(!text.contains(tool), "C++ variant names {tool}: {text:?}");
        }
    }

    #[test]
    fn preamble_unknown_names_no_build_tool() {
        let text = verification_preamble(Toolchain::Unknown);
        for tool in ["cargo", "cmake", "ctest", "make", "ninja"] {
            assert!(
                !text.contains(tool),
                "Unknown variant names {tool}: {text:?}"
            );
        }
    }

    /// A future edit must not drop an invariant in one variant only.
    #[test]
    fn preamble_variants_share_the_invariant_text() {
        for toolchain in [Toolchain::Cargo, Toolchain::CMake, Toolchain::Unknown] {
            let text = verification_preamble(toolchain);
            assert!(
                text.contains("Never push"),
                "{toolchain:?} lost the never-push contract: {text:?}"
            );
            assert!(
                text.contains("small git commits"),
                "{toolchain:?} lost the small-change guidance: {text:?}"
            );
        }
    }

    /// The compaction summary must compose *after* the workflow text, not
    /// before it, for every variant.
    #[test]
    fn preamble_summary_follows_the_workflow_text() {
        for toolchain in [Toolchain::Cargo, Toolchain::CMake, Toolchain::Unknown] {
            let prompt = task_system_prompt(toolchain);
            let preamble = build_preamble(&prompt, Some("earlier context"));
            let workflow_at = preamble
                .find(verification_preamble(toolchain))
                .expect("workflow text present");
            let summary_at = preamble
                .find("## Summary of the conversation so far:")
                .expect("summary heading present");
            assert!(
                workflow_at < summary_at,
                "{toolchain:?}: summary must follow the workflow text"
            );
        }
    }
}
