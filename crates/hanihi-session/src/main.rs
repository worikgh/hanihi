//! # hanihi-session
//!
//! Read-only session introspection for hānihi. Prints the same
//! `/session` summary the REPL shows — session name/id, current turn,
//! the `[turn N | tool calls | tokens in/out]` line, and cumulative
//! token usage — by reading a session directory's `session.json` and
//! `events.jsonl` directly, without connecting to any model or taking
//! the session lock.

use std::path::{Path, PathBuf};

/// Data needed to reproduce a session summary line.
struct SessionInfo {
    id: String,
    name: String,
    model: String,
    turn: u64,
    /// Most recent completed turn's token usage (input, output).
    last_in: u64,
    last_out: u64,
    /// Tool calls in the most recent completed turn.
    last_tool_calls: u64,
    /// Cumulative token usage across all turns.
    cum_in: u64,
    cum_out: u64,
}

/// Raw fields we need out of `session.json`.
#[derive(serde::Deserialize)]
struct Meta {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
}

fn main() {
    // Positional: working dir (default ./working) then session name
    // (default default-session). Simplest possible CLI, no clap dependency.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (working_dir, session_name) = match args.as_slice() {
        [] => (PathBuf::from("./working"), "default-session".to_string()),
        [name] => (PathBuf::from("./working"), name.clone()),
        [dir, name] => (PathBuf::from(dir), name.clone()),
        _ => {
            eprintln!("usage: hanihi-session [WORKING_DIR] [SESSION_NAME]");
            std::process::exit(2);
        }
    };

    match print_session(&working_dir, &session_name) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn print_session(working_dir: &Path, name: &str) -> Result<(), String> {
    let dir = working_dir.join("sessions").join(name);
    let meta = read_meta(&dir)?;
    let events = read_events(&dir)?;

    let info = summarize(&meta, &events);

    // Mirror the REPL's `/session` output.
    println!(
        "session: '{}' (id={}) turn={} max_turns=(n/a outside REPL)",
        info.name, info.id, info.turn
    );
    println!(
        "[turn {} | tool calls: {} | tokens: {} in / {} out]",
        info.turn, info.last_tool_calls, info.last_in, info.last_out
    );
    println!(
        "cumulative tokens: {} in / {} out | model: {}",
        info.cum_in, info.cum_out, info.model
    );
    Ok(())
}

/// Read and parse `session.json`.
fn read_meta(dir: &Path) -> Result<Meta, String> {
    let path = dir.join("session.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// Parse each `events.jsonl` line into the minimal shape we need.
fn read_events(dir: &Path) -> Result<Vec<serde_json::Value>, String> {
    let path = dir.join("events.jsonl");
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("events.jsonl line {}: {e}", i + 1))?;
        entries.push(value);
    }
    Ok(entries)
}

/// Fold the events into a [`SessionInfo`].
fn summarize(meta: &Meta, events: &[serde_json::Value]) -> SessionInfo {
    let mut info = SessionInfo {
        id: meta.id.clone(),
        name: meta.name.clone(),
        model: meta.model.clone(),
        turn: 0,
        last_in: 0,
        last_out: 0,
        last_tool_calls: 0,
        cum_in: 0,
        cum_out: 0,
    };

    for entry in events {
        let Some(kind) = entry.get("kind").and_then(|k| k.as_str()) else {
            continue;
        };
        let Some(turn) = entry.get("turn").and_then(|t| t.as_u64()) else {
            continue;
        };
        let Some(data) = entry.get("data") else {
            continue;
        };

        match kind {
            "turn_complete" => {
                info.turn = turn;
                info.last_in = 0;
                info.last_out = 0;
                info.last_tool_calls = data.get("tool_calls").and_then(|t| t.as_u64()).unwrap_or(0);
            }
            "llm_response" if turn == info.turn => {
                if let Some(usage) = data.get("usage") {
                    info.last_in += usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    info.last_out += usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    info.cum_in += usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    info.cum_out += usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            "llm_response" => {
                if let Some(usage) = data.get("usage") {
                    info.cum_in += usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    info.cum_out += usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    info
}
