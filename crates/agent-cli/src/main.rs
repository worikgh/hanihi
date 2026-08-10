//! # agent-cli
//!
//! REPL front-end for the agent harness: reedline line editor on top of
//! [`agent_core::Agent`]. Also supports one-shot turns (`--once`) for
//! scripting and smoke tests, and attaching MCP stdio servers.

use agent_core::Agent;
use agent_core::error::AgentError;
use agent_core::{McpClient, builtin_echo, builtin_get_time, connect_chat_model};
use clap::Parser;
use reedline::{DefaultPrompt, Reedline, Signal};
use rig::completion::CompletionModel;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "agent",
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

    /// MCP stdio server command(s) to attach, e.g.
    /// `--mcp-command "./target/debug/mcp-echo-server"`. May be repeated.
    #[arg(long = "mcp-command", value_name = "CMD")]
    mcp_commands: Vec<String>,

    /// Run a single turn and exit (no REPL).
    #[arg(long, value_name = "PROMPT")]
    once: Option<String>,
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

    let mut agent = connect_chat_model(&args.base_url, &api_key, &args.model)?;
    agent.add_tool(builtin_get_time());
    agent.add_tool(builtin_echo());

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
        "agent ready: model={} tools={}",
        args.model,
        agent.tool_count()
    );

    if let Some(prompt) = args.once {
        let summary = agent.run(&prompt).await?;
        println!("{}", summary.text);
        println!(
            "[tool calls: {} | tokens: {} in / {} out]",
            summary.tool_calls, summary.usage.input_tokens, summary.usage.output_tokens
        );
        return Ok(());
    }

    repl(agent).await
}

/// Interactive readline loop.
async fn repl<M: CompletionModel>(mut agent: Agent<M>) -> Result<(), AgentError> {
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
                            "commands: /help /tools /clear /quit — anything else is sent to the model"
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
                    _ => {}
                }
                match agent.run(&line).await {
                    Ok(summary) => {
                        println!("{}", summary.text);
                        println!(
                            "[tool calls: {} | tokens: {} in / {} out]",
                            summary.tool_calls,
                            summary.usage.input_tokens,
                            summary.usage.output_tokens
                        );
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
    Ok(())
}
