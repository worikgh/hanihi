//! # hanihi-cli
//!
//! REPL front-end for the agent harness: reedline line editor on top of
//! [`hanihi_core::Agent`]. Supports sessions (`--session` / `--new-session`),
//! one-shot turns (`--once`), and MCP stdio servers.

mod highlight;
mod ui;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::highlight::MonoHighlighter;
use clap::Parser;
use hanihi_core::agent::Agent;
use hanihi_core::error::AgentError;
use hanihi_core::session::SessionManager;
use hanihi_core::{
    McpClient, SourceTree, StreamEvent, builtin_apply_patch, builtin_echo, builtin_get_time,
    builtin_grep, builtin_list_dir, builtin_read_file, builtin_read_session_log,
    builtin_run_command, builtin_write_file, connect_chat_model_with_prompt,
};
use nu_ansi_term::Color;
use reedline::{DefaultPrompt, FileBackedHistory, Reedline, Signal};
use rig::completion::CompletionModel;
use tracing_subscriber::EnvFilter;

pub use hanihi_core::agent::{TurnSummary, connect_chat_model};

use crate::ui::{print_coloured, println_coloured};

const DEFAULT_WORKING_DIR: &str = "./working";
/// Default cap on model turns per request. Effectively unlimited: a sane
/// upper bound so runaway tool-call loops still terminate, but far above any
/// realistic working session.
const DEFAULT_MAX_TURNS: usize = 1000;
/// Default cap on model turns in task mode (long-horizon self-improvement).
const TASK_MAX_TURNS: usize = 1000;

#[derive(Parser, Debug)]
#[command(
    name = "hānihi",
    version,
    about = "Rust agent harness: rig + rmcp + reedline"
)]
struct Args {
    /// OpenAI-compatible chat completions base URL.
    #[arg(
        long,
        env = "LLM_BASE_URL",
        default_value = "https://api.deepseek.com/v1"
    )]
    base_url: String,

    /// API key (or set LLM_API_KEY).
    #[arg(long, env = "LLM_API_KEY")]
    api_key: Option<String>,

    /// Model name (or set LLM_MODEL).
    #[arg(long, env = "LLM_MODEL", default_value = "deepseek-chat")]
    model: String,

    /// Working directory for sessions (default: ./working, relative to cwd).
    #[arg(long, env = "HANIHI_WORKING_DIR", default_value = DEFAULT_WORKING_DIR)]
    working_dir: PathBuf,

    /// Use an existing session by name (default: "default-session").
    #[arg(long, default_value = "default-session")]
    session: String,

    /// Create a new session with this name and use it.
    #[arg(long, conflicts_with = "session")]
    new_session: Option<String>,

    /// MCP stdio server command(s) to attach, e.g.
    /// `--mcp-command "./target/debug/mcp-echo-server"`. May be repeated.
    #[arg(long = "mcp-command", value_name = "CMD")]
    mcp_commands: Vec<String>,

    /// Run a single turn and exit (no REPL).
    #[arg(long, value_name = "PROMPT")]
    once: Option<String>,

    /// Register write tools (apply_patch, write_file) — enables editing the
    /// enclosing repository. Off by default; analysis and build tools are
    /// always on.
    #[arg(long)]
    write: bool,

    /// Run a single long-horizon turn with the task-mode system prompt
    /// (workflow gates: fmt → test → build → clippy → commit). Takes
    /// precedence over --once.
    #[arg(long, value_name = "PROMPT")]
    task: Option<String>,

    /// Maximum model turns per request (default: 1000; task mode: 1000).
    #[arg(long)]
    max_turns: Option<usize>,

    /// Append this text to the system prompt. Repeatable. When used with an
    /// existing session, the appended text is persisted to the session so it
    /// applies on future resumes too.
    #[arg(long = "prompt", value_name = "TEXT")]
    prompt_append: Vec<String>,

    /// Append the content of this file to the system prompt. Repeatable.
    /// When used with an existing session, the appended content is persisted
    /// to the session so it applies on future resumes too.
    #[arg(long = "prompt-file", value_name = "PATH")]
    prompt_files: Vec<PathBuf>,

    /// Replace the entire system prompt with the additions (no base prompt
    /// is prepended). For use with --session to rewrite the stored prompt,
    /// or with --new-session to override the default. Mutually exclusive
    /// with --prompt/--prompt-file append semantics (see below).
    #[arg(long)]
    new_prompt: bool,
}

/// Extract a short provider name from a base URL hostname.
fn provider_from_url(url: &str) -> &str {
    // Strip scheme.
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("unknown");
    // e.g. "api.deepseek.com" -> "deepseek", "api.openai.com" -> "openai"
    host.trim_start_matches("api.")
        .trim_start_matches("api-")
        .split('.')
        .next()
        .unwrap_or("unknown")
}

/// Read the given prompt files and return their concatenated contents.
fn read_prompt_files(paths: &[PathBuf]) -> Result<String, AgentError> {
    let mut out = String::new();
    for path in paths {
        let content = fs::read_to_string(path)?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&content);
    }
    Ok(out)
}

/// Read a single file's contents for use as a prompt, resolved against the
/// given base directory when the path is not absolute.
fn read_prompt_file(base: &Path, path: &str) -> Result<String, AgentError> {
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    if !resolved.is_file() {
        return Err(AgentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no such file: {}", resolved.display()),
        )));
    }
    fs::read_to_string(&resolved).map_err(AgentError::Io)
}

/// Concatenate the explicit additions (`--prompt` strings and the contents
/// of `--prompt-file` files) into one block.
fn collect_extras(
    prompt_appends: &[String],
    prompt_files: &[PathBuf],
) -> Result<String, AgentError> {
    let file_text = read_prompt_files(prompt_files)?;
    let mut parts: Vec<String> = Vec::new();
    parts.extend(prompt_appends.iter().cloned());
    if !file_text.is_empty() {
        parts.push(file_text);
    }
    Ok(parts.join("\n\n"))
}

/// Whether any explicit prompt additions were supplied.
fn has_extras(prompt_appends: &[String], prompt_files: &[PathBuf]) -> bool {
    !prompt_appends.is_empty() || !prompt_files.is_empty()
}

/// Read the system prompt stored in an existing session's session.json.
fn stored_session_prompt(working_dir: &std::path::Path, session_name: &str) -> Option<String> {
    let meta_path = working_dir
        .join("sessions")
        .join(session_name)
        .join("session.json");
    let bytes = fs::read_to_string(meta_path).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&bytes).ok()?;
    meta.get("system_prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Persist a system prompt into an existing session's session.json. Reads
/// the file, updates the field, writes it back.
fn write_session_prompt(
    working_dir: &std::path::Path,
    session_name: &str,
    prompt: &str,
) -> Result<(), AgentError> {
    let meta_path = working_dir
        .join("sessions")
        .join(session_name)
        .join("session.json");
    let bytes = fs::read(&meta_path).map_err(AgentError::Io)?;
    let mut meta: serde_json::Value = serde_json::from_slice(&bytes).map_err(AgentError::Json)?;
    meta["system_prompt"] = serde_json::Value::String(prompt.to_string());
    fs::write(&meta_path, serde_json::to_string_pretty(&meta)?).map_err(AgentError::Io)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let api_key = args
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or(AgentError::MissingApiKey)?;

    // Resolve working directory relative to cwd.
    let working_dir = if args.working_dir.is_absolute() {
        args.working_dir.clone()
    } else {
        std::env::current_dir()?.join(&args.working_dir)
    };

    let mut mgr = SessionManager::new(&working_dir);
    let provider = provider_from_url(&args.base_url);

    let task_mode = args.task.is_some();
    let base_prompt: &str = if task_mode {
        hanihi_core::agent::TASK_SYSTEM_PROMPT
    } else {
        hanihi_core::agent::DEFAULT_SYSTEM_PROMPT
    };

    // Determine session name and whether to auto-create.
    let (session_name, is_new) = if let Some(ref name) = args.new_session {
        (name.clone(), true)
    } else {
        let name = args.session.clone();
        // Auto-create default-session if it doesn't exist.
        let is_new = name == "default-session" && !mgr.exists(&name);
        (name, is_new)
    };

    let had_extras = has_extras(&args.prompt_append, &args.prompt_files);

    // Resolve the system prompt this invocation will use, and whether it
    // must be persisted back to the session's metadata.
    //
    // Prompt resolution rules:
    //  - `--new-prompt` on any session: the additions (collected from
    //    --prompt / --prompt-file) become the ENTIRE prompt. On an existing
    //    session this replaces and persists the stored prompt.
    //  - Otherwise `--prompt` / `--prompt-file` append to the base prompt
    //    (new session) or to the stored prompt (existing session), and the
    //    appended result is persisted so it survives a resume.
    //  - With no additions at all, the existing stored prompt is used
    //    unchanged (or the base prompt for a fresh session).
    let (session_prompt, do_persist) = if args.new_prompt {
        // Replace/set the entire prompt from the additions only.
        let prompt = collect_extras(&args.prompt_append, &args.prompt_files)?;
        let effective = if prompt.trim().is_empty() {
            // --new-prompt with nothing supplied: use base as the new prompt.
            base_prompt.to_string()
        } else {
            prompt
        };
        if is_new {
            (effective, false) // creation persists it
        } else {
            (effective, true) // replace + persist the stored prompt
        }
    } else if is_new {
        // Fresh session: base + any appends. persisted by creation.
        let prompt = if had_extras {
            let extras = collect_extras(&args.prompt_append, &args.prompt_files)?;
            format!("{base_prompt}\n\n{extras}")
        } else {
            base_prompt.to_string()
        };
        (prompt, false)
    } else if had_extras {
        // Existing session with appends: stored + extras, persisted.
        let stored = stored_session_prompt(&working_dir, &session_name)
            .unwrap_or_else(|| base_prompt.to_string());
        let extras = collect_extras(&args.prompt_append, &args.prompt_files)?;
        let prompt = format!("{stored}\n\n{extras}");
        (prompt, true)
    } else {
        // Existing session, no additions: use stored prompt unchanged.
        let stored = stored_session_prompt(&working_dir, &session_name)
            .unwrap_or_else(|| base_prompt.to_string());
        (stored, false)
    };

    // Create or open the session.
    if is_new {
        if session_name == "default-session" {
            mgr.create_default(&args.model, &session_prompt)
                .map_err(|e| AgentError::Rig(e.to_string()))?;
            println!("auto-created session 'default-session'");
        } else {
            mgr.create(&session_name, &args.model, &session_prompt)
                .map_err(|e| AgentError::Rig(e.to_string()))?;
            println!("created session '{session_name}'");
        }
    } else {
        mgr.open(&session_name)
            .map_err(|e| AgentError::Rig(e.to_string()))?;
        if do_persist {
            write_session_prompt(&working_dir, &session_name, &session_prompt)?;
            if args.new_prompt {
                println!("session '{session_name}' system prompt replaced");
            } else {
                println!("session '{session_name}' system prompt extended");
            }
        }
    }

    // Build the agent with the resolved system prompt.
    let mut agent = connect_chat_model_with_prompt(
        args.base_url.clone(),
        api_key.clone(),
        args.model.clone(),
        &session_prompt,
    )?;
    agent.set_max_turns(args.max_turns.unwrap_or(if task_mode {
        TASK_MAX_TURNS
    } else {
        DEFAULT_MAX_TURNS
    }));
    agent.add_tool(builtin_get_time());
    agent.add_tool(builtin_echo());

    // Source-tree tools: read/list/grep/run-command over the enclosing git
    // repository, plus a window into this session's own event log. Ignore
    // rules (.gitignore, .ignore) filter what the agent can see. Write tools
    // (apply_patch, write_file) are registered only with --write.
    match SourceTree::open() {
        Ok(tree) => {
            let tree = Arc::new(tree);
            let traces_dir = working_dir.join("traces").join(&session_name);
            let log_path = working_dir
                .join("sessions")
                .join(&session_name)
                .join("events.jsonl");
            agent.add_tool(builtin_read_file(tree.clone()));
            agent.add_tool(builtin_list_dir(tree.clone()));
            agent.add_tool(builtin_grep(tree.clone()));
            agent.add_tool(builtin_run_command(tree.clone(), traces_dir));
            agent.add_tool(builtin_read_session_log(log_path));
            if args.write {
                agent.add_tool(builtin_apply_patch(tree.clone()));
                agent.add_tool(builtin_write_file(tree));
            }
        }
        Err(e) => println!("source tools disabled (no git repository): {e}"),
    }

    for command in &args.mcp_commands {
        let parts: Vec<String> = command.split_whitespace().map(String::from).collect();
        let (program, rest) = parts.split_first().ok_or_else(|| AgentError::Tool {
            name: "<mcp>".into(),
            message: "empty --mcp-command".into(),
        })?;
        let client = McpClient::connect_stdio(program, rest).await?;
        let tools = client.tool_defs().await?;
        let count = tools.len();
        for tool in tools {
            agent.add_tool(tool);
        }
        println!("attached MCP server '{program}': {count} tool(s)");
    }

    // Get a mutable reference to the open session so we can read its turn
    // count for the "session renewed" metadata below.
    let session = mgr
        .open(&session_name)
        .map_err(|_e| AgentError::Rig("failed to re-open session".into()))?;

    // Replay prior turns from the event log so the agent remembers
    // previous conversations in this session.
    let mut prior_turns = None;
    match session.replay_history() {
        Ok(history) if !history.is_empty() => {
            let msg_count = history.len();
            agent.set_history(history);
            println!("replayed {msg_count} messages from prior session");
        }
        Ok(_) => {} // empty history, nothing to replay
        Err(e) => {
            tracing::warn!("failed to replay session history: {e}");
        }
    }
    // How many turns has this session accumulated so far (including from
    // prior sessions)? Use the session's own counter; it is monotonically
    // increasing across restarts of the same session name.
    if let Ok(turns) = session.total_turns() {
        prior_turns = Some(turns);
    }

    println!(
        "agent ready: model={} tools={} session='{session_name}' max_turns={}{}",
        args.model,
        agent.tool_count(),
        agent.max_turns(),
        if args.write {
            " (write tools enabled)"
        } else {
            ""
        },
    );
    if let Some(t) = prior_turns {
        println!(
            "session '{}' renewed: total turns so far = {t}",
            session_name
        );
    }
    println!("system prompt: {} bytes", session_prompt.len());

    let prompt = args.task.clone().or_else(|| args.once.clone());
    if let Some(prompt) = prompt {
        let mut rx = session
            .run_streaming(&mut agent, provider, &args.model, &prompt)
            .await
            .map_err(|e| AgentError::Rig(e.to_string()))?;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta { text } => print_coloured!("{text}"),
                StreamEvent::ToolCallStart { name, .. } => print_coloured!("\n[🔧 {name}"),
                StreamEvent::ToolCallArgs { .. } => {}
                StreamEvent::ToolCallReady { .. } => {}
                StreamEvent::ToolResult { .. } => println_coloured!(" ✅]"),
                StreamEvent::TurnComplete { summary } => {
                    println!();
                    println_coloured!(
                        "[turn {} | tool calls: {} | tokens: {} in / {} out | max_turns: {}]",
                        session.turn,
                        summary.tool_calls,
                        summary.usage.input_tokens,
                        summary.usage.output_tokens,
                        agent.max_turns()
                    );
                    agent.set_history(summary.final_history);
                    break;
                }
                StreamEvent::Error { message } => {
                    eprintln!();
                    eprintln!("error: {message}");
                    break;
                }
            }
        }
        mgr.close(&session_name)
            .map_err(|e| AgentError::Rig(e.to_string()))?;
        return Ok(());
    }

    let cwd = std::env::current_dir().map_err(AgentError::Io)?;
    repl(session, agent, provider, &args.model, &cwd, &working_dir).await?;

    mgr.close(&session_name)
        .map_err(|e| AgentError::Rig(e.to_string()))?;
    Ok(())
}

/// Render the per-turn completion footer shown after (and by /session for) a turn.
fn turn_footer(
    turn: u64,
    tool_calls: usize,
    tokens_in: u64,
    tokens_out: u64,
    max_turns: usize,
) -> String {
    format!(
        "[turn {turn} | tool calls: {tool_calls} | tokens: {tokens_in} in / {tokens_out} out | max_turns: {max_turns}]"
    )
}

/// Interactive readline loop.
async fn repl<M: CompletionModel + 'static>(
    session: &mut hanihi_core::session::Session,
    mut agent: Agent<M>,
    provider: &str,
    model_name: &str,
    cwd: &Path,
    working_dir: &Path,
) -> Result<(), AgentError> {
    let history_path = session.root().join("history.txt");
    let history: Box<dyn reedline::History> =
        match FileBackedHistory::with_file(10_000, history_path) {
            Ok(h) => Box::new(h),
            Err(e) => {
                eprintln!("history file disabled: {e}");
                Box::new(FileBackedHistory::new(10_000).expect("in-memory history"))
            }
        };
    let edit_mode = reedline::Emacs::default();
    let mut editor = Reedline::create()
        .with_edit_mode(Box::new(edit_mode))
        .with_history(history)
        .with_highlighter(Box::new(MonoHighlighter::new(Color::Cyan)));
    let prompt = DefaultPrompt::default();
    let mut last_summary: Option<TurnSummary> = None;
    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line) | Signal::HostCommand(line)) => {
                let line = line.trim().to_string();
                match line.as_str() {
                    "" => continue,
                    "/quit" | "/exit" => break,
                    "/help" => {
                        println!(
                            "commands: /help /tools /clear /session /file /quit — anything else is sent to the model"
                        );
                        continue;
                    }
                    "/tools" => {
                        for def in agent.tool_definitions() {
                            println!("- {}: {}", def.name, def.description);
                        }
                        continue;
                    }
                    "/clear" => {
                        agent.clear_history();
                        println!("history cleared");
                        continue;
                    }
                    "/session" => {
                        println!(
                            "session: '{}' (id={}) turn={} max_turns={}",
                            session.name,
                            session.id,
                            session.turn,
                            agent.max_turns()
                        );
                        if let Some(s) = &last_summary {
                            println!(
                                "{}",
                                turn_footer(
                                    session.turn,
                                    s.tool_calls,
                                    s.usage.input_tokens,
                                    s.usage.output_tokens,
                                    agent.max_turns(),
                                )
                            );
                        } else {
                            println!("No TurnSummary");
                        }
                        continue;
                    }
                    _ => {}
                }

                // Handle `/file <PATH>`: read the file's contents relative to
                // the working directory (or absolute) and use it as the prompt.
                // Only a single line can be typed at the REPL prompt, so this
                // lets multi-line content (a prompt, a plan, code) be loaded
                // from disk and sent to the model in one go.
                if let Some(rest) = line.strip_prefix("/file ") {
                    let path = rest.trim();
                    if path.is_empty() {
                        eprintln!("/file requires a path");
                        continue;
                    }
                    let content = match read_prompt_file(working_dir, path) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("error reading '{path}': {e}");
                            continue;
                        }
                    };
                    if content.trim().is_empty() {
                        eprintln!("/file '{path}' is empty");
                        continue;
                    }
                    println!(
                        "using contents of '{}' ({}, {} bytes) as prompt",
                        path,
                        cwd.display(),
                        content.len()
                    );
                    last_summary =
                        run_turn(session, &mut agent, provider, model_name, &content).await;
                    continue;
                }

                last_summary = run_turn(session, &mut agent, provider, model_name, &line).await;
            }
            Ok(Signal::CtrlC) | Ok(Signal::ExternalBreak(_)) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }

    // Sync history to disk before shutting down.
    if let Err(e) = editor.sync_history() {
        eprintln!("warning: failed to sync history: {e}");
    }
    Ok(())
}

/// Run a single turn with the given prompt and stream the model's output.
async fn run_turn<M: CompletionModel + 'static>(
    session: &mut hanihi_core::session::Session,
    agent: &mut Agent<M>,
    provider: &str,
    model_name: &str,
    prompt: &str,
) -> Option<TurnSummary> {
    match session
        .run_streaming(agent, provider, model_name, prompt)
        .await
    {
        Ok(mut rx) => {
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::TextDelta { text } => print_coloured!("{text}"),
                    StreamEvent::ToolCallStart { name, .. } => print_coloured!("\n[🔧 {name}"),
                    StreamEvent::ToolCallArgs { .. } => {}
                    StreamEvent::ToolCallReady { .. } => {}
                    StreamEvent::ToolResult { .. } => println_coloured!(" ✅]"),
                    StreamEvent::TurnComplete { summary } => {
                        println!();
                        println_coloured!(
                            "[turn {} | tool calls: {} | tokens: {} in / {} out | max_turns: {}]",
                            session.turn,
                            summary.tool_calls,
                            summary.usage.input_tokens,
                            summary.usage.output_tokens,
                            agent.max_turns()
                        );
                        // TODO: Do I need this `clone`?
                        agent.set_history(summary.final_history.clone());
                        return Some(summary);
                    }
                    StreamEvent::Error { message } => {
                        eprintln!();
                        eprintln!("error: {message}");
                        return None;
                    }
                }
            }
            None
        }
        Err(e) => {
            eprintln!("error: {e}");
            None
        }
    }
}
