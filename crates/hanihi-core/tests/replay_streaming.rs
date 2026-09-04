//! Regression test: `replay_history` must reconstruct a canonical transcript
//! from a streaming-order event log, where `tool_execution` entries precede
//! the `llm_response` that declares them.

use std::path::Path;

use chrono::Utc;
use hanihi_core::session::SessionManager;
use hanihi_core::session::log::{LogEntry, LogWriter, ToolCallData, UsageData};
use rig::completion::Message;

fn write_log(root: &Path, entries: &[LogEntry]) {
    let mut writer = LogWriter::open(&root.join("events.jsonl")).expect("open log");
    for entry in entries {
        writer.write_entry(entry).expect("write entry");
    }
}

#[test]
fn replay_streaming_order_reconstructs_canonical_history() {
    let dir = std::env::temp_dir().join(format!("hanihi-replay-{}", uuid::Uuid::new_v4()));
    let mut mgr = SessionManager::new(&dir);
    let now = Utc::now();

    let history = {
        let session = mgr.create("stream", "deepseek-chat", "p").expect("create");

        write_log(
            session.root(),
            &[
                LogEntry::user_input(now, 1, "apply patch".into()),
                LogEntry::llm_prompt(
                    now,
                    1,
                    "d".into(),
                    "m".into(),
                    serde_json::json!([]),
                    serde_json::json!([]),
                ),
                // Streaming order: tool results are emitted before the
                // llm_response that declares their tool calls.
                LogEntry::tool_execution(
                    now,
                    1,
                    "JQX".into(),
                    "JQX".into(),
                    "list_dir".into(),
                    serde_json::json!({}),
                    "dir listing".into(),
                ),
                LogEntry::tool_execution(
                    now,
                    1,
                    "MZX".into(),
                    "MZX".into(),
                    "grep".into(),
                    serde_json::json!({"pattern": "x"}),
                    "matches".into(),
                ),
                LogEntry::llm_response(
                    now,
                    1,
                    Some("msg_a".into()),
                    None,
                    None,
                    Some(vec![
                        ToolCallData {
                            id: "JQX".into(),
                            name: "list_dir".into(),
                            arguments: serde_json::json!({}),
                        },
                        ToolCallData {
                            id: "MZX".into(),
                            name: "grep".into(),
                            arguments: serde_json::json!({"pattern": "x"}),
                        },
                    ]),
                    UsageData {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ),
                LogEntry::llm_prompt(
                    now,
                    1,
                    "d".into(),
                    "m".into(),
                    serde_json::json!([]),
                    serde_json::json!([]),
                ),
                LogEntry::tool_execution(
                    now,
                    1,
                    "CA39".into(),
                    "CA39".into(),
                    "read_file".into(),
                    serde_json::json!({"path": "f"}),
                    "file".into(),
                ),
                LogEntry::llm_response(
                    now,
                    1,
                    Some("msg_b".into()),
                    None,
                    None,
                    Some(vec![ToolCallData {
                        id: "CA39".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "f"}),
                    }]),
                    UsageData {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ),
                LogEntry::llm_prompt(
                    now,
                    1,
                    "d".into(),
                    "m".into(),
                    serde_json::json!([]),
                    serde_json::json!([]),
                ),
                LogEntry::llm_response(
                    now,
                    1,
                    Some("msg_c".into()),
                    Some("done".into()),
                    None,
                    None,
                    UsageData {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                ),
                LogEntry::turn_complete(now, 1, "done".into(), 3),
            ],
        );

        session.replay_history().expect("replay")
    };

    // user, assistant(JQX,MZX), tool JQX, tool MZX, assistant(CA39),
    // tool CA39, assistant(text) = 7 messages.
    assert_eq!(history.len(), 7, "history: {history:#?}");
    assert!(matches!(history[0], Message::User { .. }));

    mgr.close("stream").expect("close");
    std::fs::remove_dir_all(&dir).unwrap_or(());
}
