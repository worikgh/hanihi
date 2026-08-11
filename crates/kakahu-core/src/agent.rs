//! The agent: model + tools + message history + the tool-calling loop.

use rig::client::CompletionClient;
use rig::completion::message::ToolCall;
use rig::completion::{AssistantContent, CompletionModel, Message, ToolDefinition, Usage};
use rig::providers::openai;
use rig::tool::PortableDynamicTool;

use crate::error::AgentError;

/// Default system prompt used when none is supplied.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant running in an agent harness. \
You have access to tools. Use them when they help answer the user; otherwise answer directly. \
When a tool result comes back, incorporate it into your final answer.";

/// Result of one `Agent::run` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    /// Final assistant text.
    pub text: String,
    /// Number of tool calls executed during the turn.
    pub tool_calls: usize,
    /// Total token usage across all model calls in the turn.
    pub usage: Usage,
}

/// Connect to an OpenAI-compatible chat completions endpoint (e.g. DeepSeek)
/// and return an agent bound to it.
///
/// The concrete model type is hidden behind `impl CompletionModel` so callers
/// never need to name it.
pub fn connect_chat_model(
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<Agent<impl CompletionModel>, AgentError> {
    let client = openai::CompletionsClient::builder()
        .api_key(api_key)
        .base_url(base_url)
        .build()
        .map_err(|e| AgentError::Rig(e.to_string()))?;
    let model = client.completion_model(model);
    Ok(Agent::new(model, DEFAULT_SYSTEM_PROMPT))
}

/// A minimal tool-calling agent.
///
/// Generic over the rig [`CompletionModel`]; unit tests use rig's
/// `MockCompletionModel`, production uses the OpenAI-compatible client from
/// [`connect_chat_model`].
pub struct Agent<M: CompletionModel> {
    model: M,
    system_prompt: String,
    tools: Vec<PortableDynamicTool>,
    history: Vec<Message>,
    max_turns: usize,
}

impl<M: CompletionModel> Agent<M> {
    /// Create an agent with no tools and an empty history.
    pub fn new(model: M, system_prompt: impl Into<String>) -> Self {
        Self {
            model,
            system_prompt: system_prompt.into(),
            tools: Vec::new(),
            history: Vec::new(),
            max_turns: 10,
        }
    }

    /// Register a tool. The agent exposes it to the model on the next turn.
    pub fn add_tool(&mut self, tool: PortableDynamicTool) {
        self.tools.push(tool);
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

    /// Run one user request to completion: model calls, tool execution, and
    /// follow-up model calls until the model answers without tool calls.
    pub async fn run(&mut self, user_input: &str) -> Result<TurnSummary, AgentError> {
        // Messages produced during this turn (assistant replies, tool calls,
        // tool results). The user input is passed as the request `prompt` each
        // iteration so it never appears twice in the assembled history.
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

            // Model answered without tool calls: turn complete.
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
                });
            }

            // Record the assistant message (text + tool calls) and execute.
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

        // The model kept calling tools until we ran out of turns.
        self.commit_turn(user_input, turn_messages);
        Err(AgentError::MaxTurns {
            turns: self.max_turns,
        })
    }

    /// Dispatch a single tool call by name and render its output as text.
    async fn execute_tool(&self, call: &ToolCall) -> Result<String, AgentError> {
        let name = &call.function.name;
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| AgentError::Tool {
                name: name.clone(),
                message: "unknown tool".into(),
            })?;
        let output = tool
            .execute(call.function.arguments.clone())
            .await
            .map_err(|e| AgentError::Tool {
                name: name.clone(),
                message: e.to_string(),
            })?;
        Ok(output.render())
    }

    /// Append the completed turn to the persistent history.
    fn commit_turn(&mut self, user_input: &str, turn_messages: Vec<Message>) {
        self.history.push(Message::user(user_input));
        self.history.extend(turn_messages);
    }
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
        // History: user + assistant.
        assert_eq!(agent.history().len(), 2);
    }

    #[tokio::test]
    async fn test_tool_call_round_trip() {
        // Turn 1: model asks for echo("ping"). Turn 2: model answers.
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "echo", serde_json::json!({"text": "ping"})),
            MockTurn::text("echoed: ping"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(crate::tool::builtin_echo());

        let summary = agent.run("echo ping").await.expect("run succeeds");
        assert_eq!(summary.text, "echoed: ping");
        assert_eq!(summary.tool_calls, 1);

        // History: user, assistant(tool call), tool result, assistant(text).
        let history = agent.history();
        assert_eq!(history.len(), 4);
        assert!(matches!(history[1], Message::Assistant { .. }));
        assert!(matches!(history[2], Message::User { .. }));
        assert!(matches!(history[3], Message::Assistant { .. }));
    }

    #[tokio::test]
    async fn test_max_turns_exceeded() {
        // Model always requests a tool call; agent must give up eventually.
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
        // Model asks for a tool that is not registered.
        let model = MockCompletionModel::from_turns([MockTurn::tool_call(
            "call_1",
            "nonexistent",
            serde_json::json!({}),
        )]);
        let mut agent = Agent::new(model, "test system");
        let err = agent.run("do it").await.expect_err("run must fail");
        assert!(matches!(err, AgentError::Tool { .. }));
    }
}
