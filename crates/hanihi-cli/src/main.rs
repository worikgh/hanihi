//! # hanihi-cli
//!
//! REPL front-end for the agent harness: reedline line editor on top of
//! [`hanihi_core::Agent`]. Supports sessions (`--session` / `--new-session`),
//! one-shot turns (`--once`), and MCP stdio servers.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use hanihi_core::agent::Agent;
use hanihi_core::error::AgentError;
use hanihi_core::session::SessionManager;
use hanihi_core::{
    McpClient, SourceTree, StreamEvent, builtin_echo, builtin_get_time, builtin_list_dir,
    builtin_read_file, connect_chat_model,
};
use reedline::{DefaultPrompt, Reedline, Signal};
use rig::completion::CompletionModel;
use tracing_subscriber::EnvFilter;

const DEFAULT_WORKING_DIR: &str = "./working";

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
        args.working_dir
    } else {
        std::env::current_dir()?.join(&args.working_dir)
    };

    let mut mgr = SessionManager::new(&working_dir);
    let provider = provider_from_url(&args.base_url);

    // Determine session name and whether to auto-create.
    let (session_name, is_new) = if let Some(ref name) = args.new_session {
        (name.clone(), true)
    } else {
        let name = args.session.clone();
        // Auto-create default-session if it doesn't exist.
        let is_new = name == "default-session" && !mgr.exists(&name);
        (name, is_new)
    };

    // Create or open the session.
    if is_new {
        if session_name == "default-session" {
            mgr.create_default(&args.model, hanihi_core::agent::DEFAULT_SYSTEM_PROMPT)
                .map_err(|e| AgentError::Rig(e.to_string()))?;
            println!("auto-created session 'default-session'");
        } else {
            mgr.create(
                &session_name,
                &args.model,
                hanihi_core::agent::DEFAULT_SYSTEM_PROMPT,
            )
            .map_err(|e| AgentError::Rig(e.to_string()))?;
            println!("created session '{session_name}'");
        }
    } else {
        mgr.open(&session_name)
            .map_err(|e| AgentError::Rig(e.to_string()))?;
    }

    // Build the agent.
    let mut agent = connect_chat_model(args.base_url.clone(), api_key.clone(), args.model.clone())?;
    agent.add_tool(builtin_get_time());
    agent.add_tool(builtin_echo());

    // Source-tree tools: read/list the enclosing git repository. Ignore
    // rules (.gitignore, .ignore) filter what the agent can see.
    match SourceTree::open() {
        Ok(tree) => {
            let tree = Arc::new(tree);
            agent.add_tool(builtin_read_file(tree.clone()));
            agent.add_tool(builtin_list_dir(tree));
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

    println!(
        "agent ready: model={} tools={} session='{session_name}'",
        args.model,
        agent.tool_count(),
    );

    // Get a mutable reference to the open session.
    let session = mgr
        .open(&session_name)
        .map_err(|_e| AgentError::Rig("failed to re-open session".into()))?;

    // Replay prior turns from the event log so the agent remembers
    // previous conversations in this session.
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

    if let Some(prompt) = args.once {
        let mut rx = session
            .run_streaming(&mut agent, provider, &args.model, &prompt)
            .await
            .map_err(|e| AgentError::Rig(e.to_string()))?;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta { text } => print!("{text}"),
                StreamEvent::ToolCallStart { name, .. } => print!("\n[🔧 {name}"),
                StreamEvent::ToolCallArgs { .. } => {}
                StreamEvent::ToolCallReady { .. } => {}
                StreamEvent::ToolResult { .. } => println!(" ✅]"),
                StreamEvent::TurnComplete { summary } => {
                    println!();
                    println!(
                        "[tool calls: {} | tokens: {} in / {} out]",
                        summary.tool_calls, summary.usage.input_tokens, summary.usage.output_tokens
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

    repl(session, agent, provider, &args.model).await?;

    mgr.close(&session_name)
        .map_err(|e| AgentError::Rig(e.to_string()))?;
    Ok(())
}

/// Interactive readline loop.
async fn repl<M: CompletionModel + 'static>(
    session: &mut hanihi_core::session::Session,
    mut agent: Agent<M>,
    provider: &str,
    model_name: &str,
) -> Result<(), AgentError> {
    let mut editor = Reedline::create();
    let prompt = DefaultPrompt::default();

    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line) | Signal::HostCommand(line)) => {
                let line = line.trim().to_string();
                match line.as_str() {
                    "" => continue,
                    "/quit" | "/exit" => break,
                    "/help" => {
                        println!(
                            "commands: /help /tools /clear /session /quit — anything else is sent to the model"
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
                            "session: '{}' (id={}) turn={}",
                            session.name, session.id, session.turn
                        );
                        continue;
                    }
                    _ => {}
                }
                match session
                    .run_streaming(&mut agent, provider, model_name, &line)
                    .await
                {
                    Ok(mut rx) => {
                        while let Some(event) = rx.recv().await {
                            match event {
                                StreamEvent::TextDelta { text } => print!("{text}"),
                                StreamEvent::ToolCallStart { name, .. } => {
                                    print!("\n[🔧 {name}")
                                }
                                StreamEvent::ToolCallArgs { .. } => {}
                                StreamEvent::ToolCallReady { .. } => {}
                                StreamEvent::ToolResult { .. } => println!(" ✅]"),
                                StreamEvent::TurnComplete { summary } => {
                                    println!();
                                    println!(
                                        "[tool calls: {} | tokens: {} in / {} out]",
                                        summary.tool_calls,
                                        summary.usage.input_tokens,
                                        summary.usage.output_tokens
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
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
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

    // Clean shutdown — session closed by caller.
    Ok(())
}
