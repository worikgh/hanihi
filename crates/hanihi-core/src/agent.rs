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
use serde::Serialize;
use tokio::sync::mpsc;

use crate::error::AgentError;

/// Default system prompt used when none is supplied.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant running in an agent harness. \
You have access to tools. Use them when they help answer the user; otherwise answer directly. \
When a tool result comes back, incorporate it into your final answer. \
Do not re-run the same read-only tool with the same arguments within a turn: reuse the result \
you already have, because the result cannot have changed and re-running wastes resources.";

/// System prompt for task mode: long-horizon self-improvement work with
/// explicit workflow gates (mirrors the project's Rust workflow rules).
pub const TASK_SYSTEM_PROMPT: &str = "You are hānihi in task mode: a coding agent working on a Rust codebase. \
Work in small steps and verify with the build before declaring success. Workflow gates: \
run `cargo fmt` before staging changes; `cargo test` before committing; `cargo build` must pass; \
run `cargo clippy -- -D warnings` before finishing. Make changes as small git commits with \
descriptive messages. Never push. Study command output and trace files before retrying: if a \
command fails, read the error and fix the cause rather than repeating it. Verify your work with \
the build/test gates — do not assert success by eye. Before calling a tool, check whether an \
identical read-only call with a usable result already appears in this turn; reuse it instead of \
re-running.";

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
    DuplicateLimit { count: usize },
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
            return ToolCallCacheLookup::DuplicateLimit { count: *count };
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
    /// A completion request was assembled and is about to be sent.
    CompletionRequest {
        ts: chrono::DateTime<Utc>,
        messages: serde_json::Value,
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
    let model = client.completion_model(&model);
    Ok(Agent::new(model, system_prompt))
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

    /// Run a single completion call against the model.
    ///
    /// Assembles the request from `user_input` + persistent history +
    /// in-progress turn messages + tool definitions, sends it to the
    /// model, and returns the response. `Session` uses this to interpose
    /// logging between calls.
    pub(crate) async fn single_completion(
        &self,
        user_input: &str,
        turn_messages: &[Message],
    ) -> Result<rig::completion::CompletionResponse<M::Response>, AgentError> {
        let request = self
            .model
            .completion_request(Message::user(user_input))
            .preamble(self.system_prompt.clone())
            .messages(self.history.iter().chain(turn_messages.iter()).cloned())
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
            let request = self
                .model
                .completion_request(Message::user(user_input))
                .preamble(self.system_prompt.clone())
                .messages(self.history.iter().chain(turn_messages.iter()).cloned())
                .tools(self.tool_definitions())
                .build();

            let response = self.model.completion(request).await?;
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
    /// the caller should extract `final_history` from the `TurnComplete`
    /// event and call `set_history` to persist the new state.
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
        let mut history = self.history.clone();
        let user_input = user_input.to_string();

        tokio::spawn(async move {
            let result = run_streaming_loop(
                model,
                tools,
                &mut history,
                user_input,
                system_prompt,
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

/// Build the JSON message list sent to the model, for `llm_prompt` log entries.
pub(crate) fn messages_for_log(
    system_prompt: &str,
    history: &[Message],
    turn_messages: &[Message],
    user_input: &str,
) -> Result<serde_json::Value, AgentError> {
    #[derive(Serialize)]
    struct LogMessage {
        role: String,
        content: serde_json::Value,
    }

    let mut msgs: Vec<LogMessage> = Vec::new();

    // System preamble.
    msgs.push(LogMessage {
        role: "system".into(),
        content: serde_json::Value::String(system_prompt.to_string()),
    });

    // Serialize each message via serde.
    fn msg_to_value(msg: &Message) -> Result<LogMessage, AgentError> {
        let v = serde_json::to_value(msg).map_err(|e| AgentError::Rig(e.to_string()))?;
        let role = v["role"].as_str().unwrap_or("unknown").to_string();
        Ok(LogMessage {
            role,
            content: v["content"].clone(),
        })
    }

    for m in history.iter().chain(turn_messages.iter()) {
        msgs.push(msg_to_value(m)?);
    }
    // Current user message.
    msgs.push(LogMessage {
        role: "user".into(),
        content: serde_json::Value::String(user_input.to_string()),
    });

    serde_json::to_value(&msgs).map_err(|e| AgentError::Rig(e.to_string()))
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
            ToolCallCacheLookup::DuplicateLimit { count } => {
                return Ok(format!(
                    "duplicate call skipped: '{name}' with identical arguments was already called {count} times this turn; reuse an earlier result"
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
    let rendered = output.render();

    let mut cache = cache.lock().expect("tool call cache lock");
    cache.store(name, &args, &rendered);
    cache.invalidate_on_write(name);
    Ok(rendered)
}

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
        // Build the request and report it before sending.
        let tool_definitions = tools.iter().map(|t| t.definition()).collect::<Vec<_>>();
        let messages_json = messages_for_log(&system_prompt, history, &turn_messages, &user_input)?;
        let tool_definitions_json = serde_json::to_value(&tool_definitions)?;
        let _ = tx
            .send(StreamEvent::CompletionRequest {
                ts: Utc::now(),
                messages: messages_json,
                tool_definitions: tool_definitions_json,
            })
            .await;

        let request = model
            .completion_request(Message::user(user_input.clone()))
            .preamble(system_prompt.clone())
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
            let summary = TurnSummary {
                text: text_buf,
                tool_calls: tool_calls_total,
                usage: usage_total,
                final_history: history.clone(),
            };
            let _ = tx
                .send(StreamEvent::TurnComplete {
                    summary: summary.clone(),
                })
                .await;
            return Ok(summary);
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

    fn echo_agent() -> Agent<MockCompletionModel> {
        let mut agent = Agent::new(MockCompletionModel::text("unused"), "test system");
        agent.add_tool(crate::tool::builtin_echo());
        agent
    }

    #[tokio::test]
    async fn test_simple_text_reply() {
        let mut agent = echo_agent();
        let summary = agent.run("hi").await.expect("run succeeds");
        assert_eq!(summary.text, "unused");
        assert_eq!(summary.tool_calls, 0);
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_round_trip() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "echo", serde_json::json!({"text": "ping"})),
            MockTurn::text("echoed: ping"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(crate::tool::builtin_echo());

        let summary = agent.run("echo ping").await.expect("run succeeds");
        assert_eq!(summary.text, "echoed: ping");
        assert_eq!(summary.tool_calls, 1);

        let history = agent.history();
        assert_eq!(history.len(), 4);
    }

    #[tokio::test]
    async fn test_max_turns_exceeded() {
        let model = MockCompletionModel::from_turns(std::iter::repeat_n(
            MockTurn::tool_call("call_x", "echo", serde_json::json!({"text": "x"})),
            50,
        ));
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(crate::tool::builtin_echo());
        agent.set_max_turns(3);

        let err = agent.run("loop").await.expect_err("run must fail");
        assert!(matches!(err, AgentError::MaxTurns { turns: 3 }));
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
    fn messages_for_log_includes_system_history_and_user() {
        let history = vec![Message::user("earlier")];
        let turn_messages = vec![Message::assistant("partial")];
        let value =
            messages_for_log("system", &history, &turn_messages, "now").expect("build messages");

        let arr = value.as_array().expect("messages is an array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "system");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[2]["role"], "assistant");
    }
}
