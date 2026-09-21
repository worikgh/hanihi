//! Token budgeting and context compaction.
//!
//! Phase 1 keeps compaction strictly in memory: [`crate::agent::Agent`]
//! holds a rolling `summary` and a trimmed `history`. Nothing is written to
//! the session log, so a resumed session replays the full prior history and
//! re-compacts on its first oversized call. A schema-bumped `compaction`
//! event and replay support are deferred to a later phase.
//!
//! Token counts use `tiktoken-rs`'s `cl100k_base` encoding. DeepSeek does
//! not publish its tokenizer, so `cl100k_base` (OpenAI's) is a conservative,
//! compatible default. If the tokenizer cannot be built at runtime, every
//! estimate falls back to `chars / 4`, so a missing tokenizer can never fail
//! a model call.

use std::sync::OnceLock;

use rig::completion::Message;
use tiktoken_rs::CoreBPE;

/// Default input budget for models without a specific limit (1M tokens).
pub(crate) const DEFAULT_CONTEXT_LIMIT_TOKENS: usize = 1_048_576;
/// Tokens reserved for the model's response, excluded from the input budget.
pub(crate) const RESERVE_OUTPUT_TOKENS: usize = 16_384;
/// How many recent tokens to keep verbatim when compacting.
pub(crate) const KEEP_RECENT_TOKENS: usize = 20_000;
/// Cap on the size of a generated summary.
pub(crate) const MAX_SUMMARY_TOKENS: usize = 8_000;

/// Max characters of a single tool result retained in a summary transcript.
const TOOL_RESULT_SUMMARY_CHARS: usize = 2_000;

/// Lazily built, shared `cl100k_base` tokenizer. `None` when construction
/// fails; callers then fall back to the chars/4 heuristic.
fn tokenizer() -> &'static Option<CoreBPE> {
    static TOKENIZER: OnceLock<Option<CoreBPE>> = OnceLock::new();
    TOKENIZER.get_or_init(|| tiktoken_rs::cl100k_base().ok())
}

/// Estimate the number of tokens in `text`.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    match tokenizer() {
        Some(bpe) => bpe.encode_with_special_tokens(text).len(),
        None => text.chars().count().div_ceil(4),
    }
}

/// Sum the token estimate of serialized messages.
fn messages_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let json = serde_json::to_string(m).unwrap_or_default();
            estimate_tokens(&json)
        })
        .sum()
}

/// Estimate the total input tokens for one completion call.
pub(crate) fn estimate_context(
    system_prompt: &str,
    summary: Option<&str>,
    history: &[Message],
    turn_messages: &[Message],
    user_input: &str,
    tool_defs_json: &str,
) -> usize {
    let mut total = estimate_tokens(system_prompt);
    if let Some(summary) = summary {
        total += estimate_tokens(summary);
    }
    total += messages_tokens(history);
    total += messages_tokens(turn_messages);
    total += estimate_tokens(user_input);
    total += estimate_tokens(tool_defs_json);
    total
}

/// Context window for `model`, in tokens.
pub(crate) fn context_limit_for(_model: &str) -> usize {
    // Every supported DeepSeek model (deepseek-v4-pro, deepseek-v4-flash,
    // deepseek-v4-flash-vision-exp, deepseek-chat) accepts the same 1M-token
    // window. A per-model table belongs here once a smaller limit is known.
    DEFAULT_CONTEXT_LIMIT_TOKENS
}

/// Prompt that asks the model to distill old context into a durable summary.
pub(crate) const COMPACTION_PROMPT: &str = "\
Summarize the conversation below so work can continue after the older \
messages are dropped. Output plain markdown with these sections: \
## Goal, ## Constraints & Preferences, ## Progress, ## Key Decisions, \
## Current State, ## Next Steps, ## Critical Context. \
Preserve exact paths, identifiers, versions, and error text verbatim where \
they matter. Do not answer the conversation; summarize it only.";

/// Render messages as a plain-text transcript for the summarization model.
pub(crate) fn serialize_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
        let role = value
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        let Some(content) = value.get("content") else {
            continue;
        };
        match role {
            "user" => render_user_content(&mut out, content),
            "assistant" => render_assistant_content(&mut out, content),
            "system" => {
                if let Some(text) = extract_text(content) {
                    out.push_str("[System]: ");
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            other => {
                if let Some(text) = extract_text(content) {
                    out.push_str(&format!("[{other}]: {text}\n"));
                }
            }
        }
    }
    out
}

fn render_user_content(out: &mut String, content: &serde_json::Value) {
    for block in content_blocks(content) {
        let is_tool_result = block
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "toolresult");
        if is_tool_result {
            let mut text = extract_text(block).unwrap_or_default();
            let original_len = text.len();
            if original_len > TOOL_RESULT_SUMMARY_CHARS {
                text.truncate(TOOL_RESULT_SUMMARY_CHARS);
                text.push_str(&format!(
                    "…[truncated for summary, {} chars total]",
                    original_len
                ));
            }
            out.push_str("[Tool result]: ");
            out.push_str(&text);
            out.push('\n');
        } else if let Some(text) = extract_text(block) {
            out.push_str("[User]: ");
            out.push_str(&text);
            out.push('\n');
        }
    }
}

fn render_assistant_content(out: &mut String, content: &serde_json::Value) {
    for block in content_blocks(content) {
        if let (Some(name), Some(args)) = (
            block
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str()),
            block.get("function").and_then(|f| f.get("arguments")),
        ) {
            out.push_str(&format!("[Assistant tool calls]: {name}({args})\n"));
        } else if let Some(text) = extract_text(block) {
            out.push_str("[Assistant]: ");
            out.push_str(&text);
            out.push('\n');
        }
    }
}

/// Iterate content blocks: a single object or an array of objects.
fn content_blocks(content: &serde_json::Value) -> Vec<&serde_json::Value> {
    match content {
        serde_json::Value::Array(items) => items.iter().collect(),
        single => vec![single],
    }
}

/// Extract readable text from a content block (string, `text` field, or a
/// nested `content` array as used by tool-result blocks).
fn extract_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(extract_text).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
                return Some(text.to_string());
            }
            map.get("content").and_then(extract_text)
        }
        _ => None,
    }
}

/// Truncate `text` to at most `max_tokens` tokens, decoding back to a string.
pub(crate) fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    match tokenizer() {
        Some(bpe) => {
            let tokens = bpe.encode_with_special_tokens(text);
            if tokens.len() <= max_tokens {
                return text.to_string();
            }
            bpe.decode(tokens[..max_tokens].to_vec())
                .unwrap_or_else(|_| text.chars().take(max_tokens.saturating_mul(4)).collect())
        }
        None => text.chars().take(max_tokens.saturating_mul(4)).collect(),
    }
}

/// Split `history` at a turn boundary.
///
/// Returns `(old, kept)` where `kept` is the newest suffix starting at a
/// user message whose estimated tokens fit within [`KEEP_RECENT_TOKENS`].
/// Never splits inside a turn: if the newest turn alone exceeds the budget,
/// everything is returned as `old` (nothing kept) so compaction summarizes
/// it rather than breaking the assistant/tool-result pairing.
pub(crate) fn split_history(history: &[Message]) -> (&[Message], &[Message]) {
    if history.is_empty() {
        return (&[], &[]);
    }

    let boundaries: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| is_turn_start(m))
        .map(|(i, _)| i)
        .collect();

    for boundary in boundaries.iter().rev().copied() {
        if messages_tokens(&history[boundary..]) <= KEEP_RECENT_TOKENS {
            return (&history[..boundary], &history[boundary..]);
        }
    }

    // The newest turn dominates: summarize the whole history.
    (history, &[])
}

/// Whether `message` starts a turn: a `role: "user"` message whose content
/// is plain text, not a tool result (which also renders as `role: "user"`).
fn is_turn_start(message: &Message) -> bool {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    if value.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let Some(content) = value.get("content") else {
        return false;
    };
    match content {
        serde_json::Value::String(_) => true,
        serde_json::Value::Array(items) => items
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")),
        serde_json::Value::Object(map) => map.get("type").and_then(|t| t.as_str()) == Some("text"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_is_monotonic_with_text() {
        let short = estimate_tokens("hello world");
        let long = estimate_tokens(&"word ".repeat(200));
        assert!(short > 0);
        assert!(long > short);
    }

    #[test]
    fn context_limit_defaults_to_one_million() {
        assert_eq!(context_limit_for("deepseek-v4-pro"), 1_048_576);
        assert_eq!(context_limit_for("deepseek-flash"), 1_048_576);
        assert_eq!(context_limit_for("unknown-model"), 1_048_576);
    }

    #[test]
    fn split_history_empty() {
        let (old, kept) = split_history(&[]);
        assert!(old.is_empty());
        assert!(kept.is_empty());
    }

    #[test]
    fn split_history_cuts_at_user_boundary() {
        let mut history = Vec::new();
        for i in 0..3 {
            history.push(Message::user(format!("question {i}")));
            history.push(Message::assistant(format!("answer {i}")));
        }
        let (old, kept) = split_history(&history);
        assert!(!kept.is_empty());
        assert!(is_turn_start(&kept[0]));
        assert_eq!(kept[0], Message::user("question 2"));
        assert_eq!(old.len(), 4);
    }

    #[test]
    fn split_history_single_turn_dominates() {
        // A newest turn far larger than KEEP_RECENT_TOKENS forces the whole
        // history into `old` (never split inside a turn).
        let huge = "word ".repeat(50_000);
        let history = vec![
            Message::user("earlier"),
            Message::assistant("a"),
            Message::user(huge),
            Message::assistant("b"),
        ];
        let (old, kept) = split_history(&history);
        assert_eq!(old.len(), history.len());
        assert!(kept.is_empty());
    }

    #[test]
    fn truncate_to_token_budget_leaves_short_text() {
        let text = "short text";
        assert_eq!(truncate_to_token_budget(text, 100), text);
    }

    #[test]
    fn truncate_to_token_budget_reduces_long_text() {
        let text = "token ".repeat(2_000);
        let out = truncate_to_token_budget(&text, 10);
        assert!(out.len() < text.len());
        if let Some(bpe) = tokenizer() {
            assert!(bpe.encode_with_special_tokens(&out).len() <= 10);
        }
    }

    #[test]
    fn serialize_for_summary_renders_roles() {
        let messages = vec![Message::user("hello"), Message::assistant("hi")];
        let out = serialize_for_summary(&messages);
        assert!(out.contains("[User]: hello"), "got: {out}");
        assert!(out.contains("[Assistant]: hi"), "got: {out}");
    }

    #[test]
    fn serialize_for_summary_truncates_tool_results() {
        let call = rig::completion::message::ToolCall::new(
            "call_1".to_string(),
            rig::completion::message::ToolFunction {
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
            },
        );
        let messages = vec![
            Message::Assistant {
                id: None,
                content: rig::OneOrMany::one(rig::completion::AssistantContent::ToolCall(call)),
            },
            Message::tool_result_with_call_id(
                "call_1".to_string(),
                None,
                "z".repeat(TOOL_RESULT_SUMMARY_CHARS + 500),
            ),
        ];
        let out = serialize_for_summary(&messages);
        assert!(out.contains("[Tool result]:"), "got: {out}");
        assert!(out.contains("[truncated for summary"), "got: {out}");
    }
}
