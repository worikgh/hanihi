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
const MAX_TOOL_CALLS_PER_TURN: usize = 100;

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

/// Per-turn cache of read-only tool results keyed by `name + '\u{1}' + args`.
#[derive(Debug, Default)]
struct ToolCallCache {
    results: HashMap<String, String>,
    counts: HashMap<String, usize>,
}

impl ToolCallCache {
    fn reset(&mut self) {
        self.results.clear();
        self.counts.clear();
    }

    fn key(name: &str, args: &serde_json::Value) -> String {
        format!("{name}\u{1}{args}")
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
    /// An error occurred during the turn.
    Error { message: String },
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

    /// Clear the per-turn read-only tool cache (start of a new turn).
    pub(crate) fn clear_tool_cache(&self) {
        self.tool_cache
            .lock()
            .expect("tool call cache lock")
            .reset();
    }

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
    pub async fn run(&mut self, user_input: &str) -> Result<TurnSummary, AgentError> {
        self.clear_tool_cache();
        let mut turn_messages: Vec<Message> = Vec::new();
        let mut tool_calls_total = 0usize;
        let mut usage_total = Usage::new();

        for _ in 0..self.max_turns {
            let prepared = self.prepare_context(user_input, &turn_messages).await?;
            let response = self.single_completion_with(&prepared).await?;
            usage_total += response.usage;

            let mut text_parts = Vec::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            for content in response.choice.iter() {
                match content {
                    AssistantContent::Text(t) => text_parts.push(t.clone()),
                    AssistantContent::ToolCall(call) => tool_calls.push(call.clone()),
                    AssistantContent::Reasoning(_) | AssistantContent::Image(_) => {}
                }
            }

            if tool_calls.is_empty() {
                let text = text_parts
                    .iter()
                    .map(|t| t.text())
                    .collect::<Vec<_>>()
                    .join("\n");
                turn_messages.push(Message::assistant(text.clone()));
                self.commit_turn(user_input, turn_messages);
                return Ok(TurnSummary {
                    text,
                    tool_calls: tool_calls_total,
                    usage: usage_total,
                    final_history: self.history.clone(),
                    final_summary: self.summary.clone(),
                });
            }

            let mut contents: Vec<AssistantContent> =
                text_parts.into_iter().map(AssistantContent::Text).collect();
            contents.extend(tool_calls.iter().cloned().map(AssistantContent::ToolCall));
            turn_messages.push(Message::Assistant {
                id: response.message_id,
                content: rig::OneOrMany::from_iter_optional(contents)
                    .expect("assistant message has at least one tool call"),
            });

            for call in &tool_calls {
                if tool_calls_total >= MAX_TOOL_CALLS_PER_TURN {
                    tracing::error!(
                        calls = tool_calls_total,
                        "tool call limit exceeded in one turn"
                    );
                    self.commit_turn(user_input, turn_messages);
                    return Err(AgentError::ToolCallLimit {
                        calls: tool_calls_total,
                    });
                }
                let output = self.execute_tool(call).await?;
                tool_calls_total += 1;
                turn_messages.push(Message::tool_result_with_call_id(
                    call.id.clone(),
                    call.call_id.clone(),
                    output,
                ));
            }
        }

        self.commit_turn(user_input, turn_messages);
        Err(AgentError::MaxTurns {
            turns: self.max_turns,
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
                            let _ = tx
                                .send(StreamEvent::ToolResult {
                                    id: tool_call.id.clone(),
                                    name: name.clone(),
                                    result_preview: preview,
                                    result: rendered.clone(),
                                })
                                .await;
                            tool_calls_total += 1;
                            pending_results.push((tool_call.clone(), rendered));
                        }
                        Err(e) => {
                            eprintln!(
                                "{}:{}:execute_tool_with_cache error. name: {name} Error: {e}",
                                file!(),
                                line!(),
                            );
                            let _ = tx
                                .send(StreamEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return Err(e);
                        }
                    }
                    pending_tool_calls.push(tool_call);
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
            turn_messages.push(Message::assistant(text_buf.clone()));
            history.push(Message::user(user_input));
            history.extend(turn_messages);
            let turn_summary = TurnSummary {
                text: text_buf,
                tool_calls: tool_calls_total,
                usage: usage_total,
                final_history: history.clone(),
                final_summary: summary.clone(),
            };
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
        })
        .await;
    Err(AgentError::MaxTurns { turns: max_turns })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::test_utils::{MockCompletionModel, MockTurn};

    fn get_time_agent() -> Agent<MockCompletionModel> {
        let mut agent = Agent::new(MockCompletionModel::text("unused"), "test system");
        agent.add_tool(crate::tool::builtin_get_time());
        agent
    }

    #[tokio::test]
    async fn test_simple_text_reply() {
        let mut agent = get_time_agent();
        let summary = agent.run("hi").await.expect("run succeeds");
        assert_eq!(summary.text, "unused");
        assert_eq!(summary.tool_calls, 0);
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_round_trip() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "get_time", serde_json::json!({})),
            MockTurn::text("got the time"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(crate::tool::builtin_get_time());

        let summary = agent.run("what time is it").await.expect("run succeeds");
        assert_eq!(summary.text, "got the time");
        assert_eq!(summary.tool_calls, 1);

        let history = agent.history();
        assert_eq!(history.len(), 4);
    }

    #[tokio::test]
    async fn test_unknown_tool_fails() {
        let model = MockCompletionModel::from_turns([MockTurn::tool_call(
            "call_1",
            "nonexistent",
            serde_json::json!({}),
        )]);
        let mut agent = Agent::new(model, "test system");
        let err = agent.run("do it").await.expect_err("run must fail");
        assert!(matches!(err, AgentError::Tool { .. }));
    }

    #[tokio::test]
    async fn test_under_budget_does_not_compact() {
        let model = MockCompletionModel::from_turns([MockTurn::text("answer")]);
        let mut agent = Agent::new(model, "test system");
        let summary = agent.run("hi").await.expect("run succeeds");
        assert_eq!(summary.text, "answer");
        assert!(agent.summary().is_none());
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_over_budget_compacts_history() {
        // Scripted turns: [summarize, answer]. The first call is the plain
        // summarization completion; the second is the real answer.
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
        let original_len = agent.history().len();

        let summary = agent.run("hi").await.expect("run succeeds");
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
        let mut turns: Vec<MockTurn> = (0..=MAX_TOOL_CALLS_PER_TURN)
            .map(|i| MockTurn::tool_call(format!("call_{i}"), "get_time", serde_json::json!({})))
            .collect();
        turns.push(MockTurn::text("done"));
        let model = MockCompletionModel::from_turns(turns);
        let mut agent = Agent::new(model, "test system");
        agent.set_max_turns(200);
        agent.add_tool(crate::tool::builtin_get_time());

        let err = agent.run("go").await.expect_err("must hit the limit");
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

        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "read_file", serde_json::json!({"path": "a"})),
            MockTurn::tool_call("call_2", "read_file", serde_json::json!({"path": "a"})),
            MockTurn::text("done"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(counting_tool);

        let summary = agent
            .run("read the same file twice")
            .await
            .expect("run succeeds");
        // The two identical calls are deduplicated but still counted once.
        assert_eq!(summary.tool_calls, 2);
        use std::sync::atomic::Ordering;
        assert_eq!(executions.load(Ordering::SeqCst), 1);
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
