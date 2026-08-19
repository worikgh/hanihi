//! # hanihi-eval
//!
//! Eval runner for the hānihi agent harness. Discovers test cases from
//! `evals/cases/`, runs them against a live LLM, and checks assertions
//! against the session event log.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hanihi_core::agent::DEFAULT_SYSTEM_PROMPT;
use hanihi_core::connect_chat_model;
use hanihi_core::session::SessionManager;
use hanihi_core::session::log::LogEntry;
use hanihi_core::{
    SourceTree, builtin_apply_patch, builtin_echo, builtin_get_time, builtin_grep,
    builtin_list_dir, builtin_read_file, builtin_read_session_log, builtin_run_command,
    builtin_write_file,
};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

// ── CLI ───────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "hanihi-eval",
    version,
    about = "Eval harness for hānihi: run test cases against a live LLM"
)]
struct Args {
    /// Directory containing case subdirectories.
    #[arg(long, default_value = "./evals/cases")]
    cases_dir: PathBuf,

    /// Run a single case by directory name (e.g. "001-basic-echo").
    #[arg(long)]
    case: Option<String>,

    /// List all discovered cases and exit.
    #[arg(long)]
    list: bool,

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

    /// Default model (individual cases may override).
    #[arg(long, env = "LLM_MODEL", default_value = "deepseek-chat")]
    model: String,

    /// MCP stdio server command(s) to attach for all cases. Repeatable.
    #[arg(long = "mcp-command", value_name = "CMD")]
    mcp_commands: Vec<String>,

    /// Keep temporary session directories after the run.
    #[arg(long)]
    keep_sessions: bool,

    /// Per-case timeout in seconds.
    #[arg(long, default_value = "120")]
    timeout: u64,
}

// ── Case definition ───────────────────────────────────────────────

/// A single eval test case loaded from case.toml.
#[derive(Debug, Deserialize)]
struct Case {
    /// Case directory name (populated after discovery, not from TOML).
    #[serde(skip)]
    dir_name: String,

    /// Case directory path (populated after discovery, not from TOML).
    #[serde(skip)]
    case_dir: PathBuf,

    /// Optional model override for this case.
    #[serde(default)]
    model: Option<String>,

    /// Optional system prompt override.
    #[serde(default)]
    system_prompt: Option<String>,

    /// The user input to send.
    user_input: String,

    /// Enable source-tree tools (read_file, list_dir).
    #[serde(default)]
    source_tree: bool,

    /// Git repo for this case, resolved relative to the case directory.
    /// When set, source tools and run_command are bound to that repo.
    #[serde(default)]
    repo: Option<PathBuf>,

    /// Register write tools (apply_patch, write_file) for this case.
    #[serde(default)]
    write_tools: bool,

    /// Create a throwaway trivial Rust git repo for this case (used as
    /// `repo`). Keeps the hānihi checkout itself out of the write path.
    #[serde(default)]
    fixture: bool,

    /// Assertions that must all pass.
    assertions: Vec<Assertion>,
}

/// One assertion to evaluate against the event log.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Assertion {
    /// A specific tool was called at least `min` (default 1) / at most `max` times.
    #[serde(rename = "tool_called")]
    ToolCalled {
        name: String,
        #[serde(default = "default_min")]
        min: usize,
        max: Option<usize>,
    },
    /// A specific tool was never called.
    #[serde(rename = "tool_not_called")]
    ToolNotCalled { name: String },
    /// Final answer contains a substring.
    #[serde(rename = "text_contains")]
    TextContains { value: String },
    /// Final answer does NOT contain a substring.
    #[serde(rename = "text_not_contains")]
    TextNotContains { value: String },
    /// Final answer matches a regex.
    #[serde(rename = "text_regex")]
    TextRegex { pattern: String },
    /// No error events in the log.
    #[serde(rename = "no_error")]
    NoError,
    /// Turn count ≤ max.
    #[serde(rename = "max_turns")]
    MaxTurns { max: usize },
    /// Each llm_prompt → llm_response latency ≤ max milliseconds.
    #[serde(rename = "latency_ms")]
    LatencyMs { max: u64 },
    /// Cumulative token usage within budget.
    #[serde(rename = "token_budget")]
    TokenBudget {
        max_input: Option<u32>,
        max_output: Option<u32>,
    },
    /// `cargo check` exits 0 in the case's repo.
    #[serde(rename = "build_succeeds")]
    BuildSucceeds,
    /// `cargo test` exits 0 in the case's repo.
    #[serde(rename = "tests_pass")]
    TestsPass,
    /// `cargo clippy -- -D warnings` exits 0 in the case's repo.
    #[serde(rename = "clippy_clean")]
    ClippyClean,
    /// Working tree matches HEAD in the case's repo (no uncommitted junk).
    #[serde(rename = "no_diff")]
    NoDiff,
}

fn default_min() -> usize {
    1
}

// ── Assertion result ──────────────────────────────────────────────

#[derive(Debug)]
struct AssertionResult {
    /// Human-readable description of this assertion.
    label: String,
    passed: bool,
    detail: String,
}

#[derive(Debug)]
struct CaseResult {
    #[allow(dead_code)]
    dir_name: String,
    passed: bool,
    assertions: Vec<AssertionResult>,
    /// Final answer text (for context).
    answer: String,
    /// Cumulative token usage.
    tokens_in: u64,
    tokens_out: u64,
    /// Total wall-clock duration.
    duration_ms: u64,
}

// ── Case discovery ────────────────────────────────────────────────

/// Discover cases by scanning `cases_dir` for subdirectories containing `case.toml`.
fn discover_cases(cases_dir: &Path) -> Result<Vec<Case>, String> {
    if !cases_dir.exists() {
        return Err(format!(
            "cases directory not found: {}",
            cases_dir.display()
        ));
    }
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(cases_dir).map_err(|e| format!("read_dir: {e}"))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let case_toml = entry.path().join("case.toml");
        if !case_toml.exists() {
            continue;
        }
        let dir_name = entry.file_name().to_str().unwrap_or("unknown").to_string();
        let raw = std::fs::read_to_string(&case_toml)
            .map_err(|e| format!("read {}: {e}", case_toml.display()))?;
        let mut case: Case =
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", case_toml.display()))?;
        case.dir_name = dir_name;
        case.case_dir = entry.path();
        cases.push(case);
    }
    cases.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    Ok(cases)
}

// ── Log parsing ───────────────────────────────────────────────────

/// Parse an events.jsonl file into a `Vec<LogEntry>`.
fn parse_event_log(path: &Path) -> Result<Vec<LogEntry>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read log: {e}"))?;
    let mut entries = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: LogEntry =
            serde_json::from_str(line).map_err(|e| format!("log line {}: {e}", i + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

// ── Assertion engine ──────────────────────────────────────────────

/// Evaluate all assertions against the parsed event log. Repo-backed
/// assertions (build/tests/clippy/no_diff) run against `repo_dir`.
async fn evaluate(
    case: &Case,
    log: &[LogEntry],
    repo_dir: Option<&Path>,
    start: std::time::Instant,
) -> Vec<AssertionResult> {
    let mut results = Vec::with_capacity(case.assertions.len() + 1);
    for assertion in &case.assertions {
        results.push(evaluate_one(assertion, log, repo_dir).await);
    }
    results.push(duration_result(start));
    results
}

/// Duration is always reported (not an assertion, informational).
fn duration_result(start: std::time::Instant) -> AssertionResult {
    let ms = start.elapsed().as_millis() as u64;
    AssertionResult {
        label: "duration".into(),
        passed: true,
        detail: format!("{ms}ms"),
    }
}

async fn evaluate_one(
    assertion: &Assertion,
    log: &[LogEntry],
    repo_dir: Option<&Path>,
) -> AssertionResult {
    match assertion {
        Assertion::ToolCalled { name, min, max } => {
            let count = log
                .iter()
                .filter(|e| matches!(e, LogEntry::ToolExecution { data, .. } if data.name == *name))
                .count();
            let max_str = max.map(|m| format!(" ≤ {m}")).unwrap_or_default();
            let label = format!("tool_called({name}) ≥ {min}{max_str}");
            let passed = count >= *min && max.is_none_or(|m| count <= m);
            AssertionResult {
                label,
                passed,
                detail: format!("called {count} time(s)"),
            }
        }
        Assertion::ToolNotCalled { name } => {
            let count = log
                .iter()
                .filter(|e| matches!(e, LogEntry::ToolExecution { data, .. } if data.name == *name))
                .count();
            AssertionResult {
                label: format!("tool_not_called({name})"),
                passed: count == 0,
                detail: format!("called {count} time(s)"),
            }
        }
        Assertion::TextContains { value } => {
            let answer = final_answer(log);
            let passed = answer.contains(value.as_str());
            AssertionResult {
                label: format!("text_contains({value:?})"),
                passed,
                detail: if passed {
                    "found".into()
                } else {
                    format!("not found in: {}", truncate(&answer, 120))
                },
            }
        }
        Assertion::TextNotContains { value } => {
            let answer = final_answer(log);
            let passed = !answer.contains(value.as_str());
            AssertionResult {
                label: format!("text_not_contains({value:?})"),
                passed,
                detail: if passed {
                    "ok".into()
                } else {
                    format!("found in: {}", truncate(&answer, 120))
                },
            }
        }
        Assertion::TextRegex { pattern } => {
            let answer = final_answer(log);
            let re = regex::Regex::new(pattern);
            match re {
                Ok(re) => {
                    let passed = re.is_match(&answer);
                    AssertionResult {
                        label: format!("text_regex({pattern:?})"),
                        passed,
                        detail: if passed {
                            "matched".into()
                        } else {
                            format!("no match in: {}", truncate(&answer, 120))
                        },
                    }
                }
                Err(e) => AssertionResult {
                    label: format!("text_regex({pattern:?})"),
                    passed: false,
                    detail: format!("invalid regex: {e}"),
                },
            }
        }
        Assertion::NoError => {
            let errors: Vec<_> = log
                .iter()
                .filter(|e| matches!(e, LogEntry::Error { .. }))
                .collect();
            AssertionResult {
                label: "no_error".into(),
                passed: errors.is_empty(),
                detail: if errors.is_empty() {
                    "ok".into()
                } else {
                    format!(
                        "{} error(s): {}",
                        errors.len(),
                        errors
                            .iter()
                            .map(|e| {
                                if let LogEntry::Error { data, .. } = e {
                                    data.message.clone()
                                } else {
                                    String::new()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                },
            }
        }
        Assertion::MaxTurns { max } => {
            let max_turn = log
                .iter()
                .filter_map(|e| {
                    if matches!(e, LogEntry::TurnComplete { .. }) {
                        Some(e.turn())
                    } else {
                        None
                    }
                })
                .max()
                .unwrap_or(0) as usize;
            let passed = max_turn <= *max;
            AssertionResult {
                label: format!("max_turns(≤ {max})"),
                passed,
                detail: format!("{max_turn} turn(s)"),
            }
        }
        Assertion::LatencyMs { max } => {
            // Pair llm_prompt → llm_response by scanning.
            let mut prompts: Vec<(usize, chrono::DateTime<chrono::Utc>)> = Vec::new();
            let mut latencies: Vec<i64> = Vec::new();
            for entry in log {
                match entry {
                    LogEntry::LlmPrompt { ts, .. } => {
                        prompts.push((entry.turn() as usize, *ts));
                    }
                    LogEntry::LlmResponse { ts, .. } => {
                        // Match to the most recent unmatched prompt.
                        if let Some((_turn, prompt_ts)) = prompts.pop() {
                            let ms = (*ts - prompt_ts).num_milliseconds();
                            latencies.push(ms);
                        }
                    }
                    _ => {}
                }
            }
            let all_ok = latencies.iter().all(|&ms| ms >= 0 && (ms as u64) <= *max);
            let detail = if latencies.is_empty() {
                "no model calls".into()
            } else {
                latencies
                    .iter()
                    .map(|ms| format!("{ms}ms"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            AssertionResult {
                label: format!("latency_ms(≤ {max}ms)"),
                passed: all_ok,
                detail,
            }
        }
        Assertion::TokenBudget {
            max_input,
            max_output,
        } => {
            let (tin, tout) = cumulative_usage(log);
            let in_ok = max_input.is_none_or(|max| tin <= max);
            let out_ok = max_output.is_none_or(|max| tout <= max);
            let passed = in_ok && out_ok;
            let parts: Vec<String> = [
                Some(format!("{tin} in")),
                Some(format!("{tout} out")),
                max_input.map(|m| format!("≤{m} in")),
                max_output.map(|m| format!("≤{m} out")),
            ]
            .into_iter()
            .flatten()
            .collect();
            AssertionResult {
                label: "token_budget".into(),
                passed,
                detail: parts.join(", "),
            }
        }
        Assertion::BuildSucceeds => {
            cargo_gate(repo_dir, "build_succeeds", &["check"], "cargo check").await
        }
        Assertion::TestsPass => cargo_gate(repo_dir, "tests_pass", &["test"], "cargo test").await,
        Assertion::ClippyClean => {
            cargo_gate(
                repo_dir,
                "clippy_clean",
                &["clippy", "--", "-D", "warnings"],
                "cargo clippy -- -D warnings",
            )
            .await
        }
        Assertion::NoDiff => {
            let label = "no_diff".into();
            match repo_dir {
                Some(dir) => {
                    let output = tokio::process::Command::new("git")
                        .args(["status", "--porcelain"])
                        .current_dir(dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output()
                        .await;
                    match output {
                        Ok(o) => {
                            let out = String::from_utf8_lossy(&o.stdout);
                            let dirty = out.trim();
                            AssertionResult {
                                label,
                                passed: dirty.is_empty(),
                                detail: if dirty.is_empty() {
                                    "working tree clean".into()
                                } else {
                                    format!("dirty: {}", truncate(dirty, 300))
                                },
                            }
                        }
                        Err(e) => AssertionResult {
                            label,
                            passed: false,
                            detail: format!("git status: {e}"),
                        },
                    }
                }
                None => AssertionResult {
                    label,
                    passed: false,
                    detail: "no repo configured for this case".into(),
                },
            }
        }
    }
}

/// Run a cargo subcommand in the case's repo and report pass/fail.
async fn cargo_gate(
    repo_dir: Option<&Path>,
    label: &str,
    args: &[&str],
    command_desc: &str,
) -> AssertionResult {
    let Some(dir) = repo_dir else {
        return AssertionResult {
            label: label.into(),
            passed: false,
            detail: "no repo configured for this case".into(),
        };
    };
    let output = tokio::process::Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => AssertionResult {
            label: label.into(),
            passed: true,
            detail: format!("{command_desc} exited 0"),
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let msg = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            AssertionResult {
                label: label.into(),
                passed: false,
                detail: format!("{command_desc} failed: {}", truncate(msg, 300)),
            }
        }
        Err(e) => AssertionResult {
            label: label.into(),
            passed: false,
            detail: format!("spawn {command_desc}: {e}"),
        },
    }
}

/// Extract the final answer text from a turn_complete event.
fn final_answer(log: &[LogEntry]) -> String {
    log.iter()
        .rev()
        .find_map(|e| {
            if let LogEntry::TurnComplete { data, .. } = e {
                Some(data.text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Compute cumulative token usage from llm_response events.
fn cumulative_usage(log: &[LogEntry]) -> (u32, u32) {
    let mut tin = 0u64;
    let mut tout = 0u64;
    for entry in log {
        if let LogEntry::LlmResponse { data, .. } = entry {
            tin += data.usage.input_tokens as u64;
            tout += data.usage.output_tokens as u64;
        }
    }
    (tin as u32, tout as u32)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ── Runner ────────────────────────────────────────────────────────

/// Run a single case and return the result.
async fn run_case(
    case: &Case,
    base_url: &str,
    api_key: &str,
    default_model: &str,
    _mcp_commands: &[String],
    keep: bool,
    timeout_secs: u64,
) -> Result<CaseResult, String> {
    let model = case.model.as_deref().unwrap_or(default_model);
    let system_prompt = case
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);

    // Temp session directory.
    let temp_root = std::env::temp_dir().join(format!("hanihi-eval-{}", uuid::Uuid::new_v4()));
    let mut mgr = SessionManager::new(&temp_root);

    let session_name = format!("eval-{}", case.dir_name);

    // Create session.
    mgr.create(&session_name, model, system_prompt)
        .map_err(|e| format!("create session: {e}"))?;

    // Repo for this case: throwaway fixture, explicit `repo` path (relative
    // to the case directory), or the cwd repo when source tools are wanted.
    let mut fixture_dir: Option<PathBuf> = None;
    let repo_dir: Option<PathBuf> = if case.fixture {
        let fx = create_fixture_repo().await?;
        fixture_dir = Some(fx.clone());
        Some(fx)
    } else if let Some(repo) = &case.repo {
        let resolved = if repo.is_absolute() {
            repo.clone()
        } else {
            case.case_dir.join(repo)
        };
        let canon = resolved
            .canonicalize()
            .map_err(|e| format!("repo path {}: {e}", resolved.display()))?;
        if !canon.join(".git").exists() {
            return Err(format!("repo {} is not a git repository", canon.display()));
        }
        Some(canon)
    } else {
        None
    };

    // Build agent.
    let mut agent =
        connect_chat_model(base_url.to_string(), api_key.to_string(), model.to_string())
            .map_err(|e| format!("connect model: {e}"))?;
    agent.add_tool(builtin_get_time());
    agent.add_tool(builtin_echo());

    // Source-tree tools bound to the case repo (or cwd when none given).
    let tree = match &repo_dir {
        Some(dir) => Some(Arc::new(
            SourceTree::open_at(dir).map_err(|e| format!("open repo {}: {e}", dir.display()))?,
        )),
        None if case.source_tree || case.write_tools => match SourceTree::open() {
            Ok(t) => Some(Arc::new(t)),
            Err(e) => return Err(format!("source-tree requested but unavailable: {e}")),
        },
        None => None,
    };

    if let Some(tree) = &tree {
        let traces_dir = temp_root.join("traces").join(&session_name);
        let log_path = temp_root
            .join("sessions")
            .join(&session_name)
            .join("events.jsonl");
        agent.add_tool(builtin_read_file(tree.clone()));
        agent.add_tool(builtin_list_dir(tree.clone()));
        agent.add_tool(builtin_grep(tree.clone()));
        agent.add_tool(builtin_run_command(tree.clone(), traces_dir));
        agent.add_tool(builtin_read_session_log(log_path));
        if case.write_tools {
            agent.add_tool(builtin_apply_patch(tree.clone()));
            agent.add_tool(builtin_write_file(tree.clone()));
        }
    }

    // TODO: attach MCP servers from _mcp_commands once McpClient is re-exported.
    if !_mcp_commands.is_empty() {
        return Err("MCP support in eval runner not yet implemented".into());
    }

    let provider = provider_from_url(base_url);

    // Re-open to get a mutable reference after agent construction.
    let session = mgr
        .open(&session_name)
        .map_err(|e| format!("re-open session: {e}"))?;

    let start = std::time::Instant::now();

    // Run with timeout.
    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        session.run(&mut agent, provider, model, &case.user_input),
    )
    .await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(summary)) => {
            // Read the event log.
            let log_path = session.root().join("events.jsonl");
            let log = parse_event_log(&log_path)?;

            let assertions = evaluate(case, &log, repo_dir.as_deref(), start).await;

            // Clean up.
            let _ = mgr.close(&session_name);
            cleanup(&temp_root, fixture_dir.as_deref(), keep);

            Ok(CaseResult {
                dir_name: case.dir_name.clone(),
                passed: assertions.iter().all(|a| a.passed),
                assertions,
                answer: summary.text,
                tokens_in: summary.usage.input_tokens,
                tokens_out: summary.usage.output_tokens,
                duration_ms,
            })
        }
        Ok(Err(e)) => {
            let _ = mgr.close(&session_name);
            cleanup(&temp_root, fixture_dir.as_deref(), keep);
            Err(format!("agent error: {e}"))
        }
        Err(_elapsed) => {
            let _ = mgr.close(&session_name);
            cleanup(&temp_root, fixture_dir.as_deref(), keep);
            Err(format!("timed out after {timeout_secs}s"))
        }
    }
}

/// Create a throwaway git repo with a trivial Rust project (for `fixture`
/// cases). Configured with a local git identity so commits work.
async fn create_fixture_repo() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("hanihi-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("src")).map_err(|e| format!("fixture mkdir: {e}"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .map_err(|e| format!("fixture Cargo.toml: {e}"))?;
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .map_err(|e| format!("fixture main.rs: {e}"))?;
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "eval@hanihi.local"][..],
        &["config", "user.name", "hanihi-eval"][..],
        &["add", "."][..],
        &["commit", "-q", "-m", "fixture init"][..],
    ] {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("spawn git {}: {e}", args[0]))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
        }
    }
    Ok(dir)
}

/// Remove the temp session root and any fixture repo unless `keep` is set.
fn cleanup(temp_root: &Path, fixture_dir: Option<&Path>, keep: bool) {
    if keep {
        return;
    }
    let _ = std::fs::remove_dir_all(temp_root);
    if let Some(fx) = fixture_dir {
        let _ = std::fs::remove_dir_all(fx);
    }
}

/// Extract a short provider name from a base URL hostname.
fn provider_from_url(url: &str) -> &str {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("unknown");
    host.trim_start_matches("api.")
        .trim_start_matches("api-")
        .split('.')
        .next()
        .unwrap_or("unknown")
}

// ── Main ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse();

    // Discover cases.
    let all_cases = match discover_cases(&args.cases_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if all_cases.is_empty() {
        eprintln!(
            "no cases found in {} (expected subdirs with case.toml)",
            args.cases_dir.display()
        );
        std::process::exit(1);
    }

    // --list mode.
    if args.list {
        println!(
            "{} case(s) in {}:",
            all_cases.len(),
            args.cases_dir.display()
        );
        for case in &all_cases {
            let model_note = case
                .model
                .as_ref()
                .map(|m| format!(" [model={m}]"))
                .unwrap_or_default();
            println!("  {} ({}){}", case.dir_name, case.user_input, model_note);
        }
        return;
    }

    // Filter to a single case if requested.
    let cases: Vec<&Case> = if let Some(ref target) = args.case {
        all_cases.iter().filter(|c| c.dir_name == *target).collect()
    } else {
        all_cases.iter().collect()
    };

    if cases.is_empty() {
        if let Some(ref target) = args.case {
            eprintln!("case '{target}' not found");
        }
        std::process::exit(1);
    }

    let api_key = match &args.api_key {
        Some(k) if !k.is_empty() => k.clone(),
        _ => {
            eprintln!("error: LLM_API_KEY is required (set env or pass --api-key)");
            std::process::exit(1);
        }
    };

    let mut results: Vec<CaseResult> = Vec::new();

    for case in &cases {
        println!("═══ {} ═══", case.dir_name);
        println!("  prompt: {}", case.user_input);

        match run_case(
            case,
            &args.base_url,
            &api_key,
            &args.model,
            &args.mcp_commands,
            args.keep_sessions,
            args.timeout,
        )
        .await
        {
            Ok(result) => {
                let status = if result.passed {
                    "✅ PASS"
                } else {
                    "❌ FAIL"
                };
                println!("  {status} ({:.1}s)", result.duration_ms as f64 / 1000.0);
                println!(
                    "  tokens: {} in / {} out",
                    result.tokens_in, result.tokens_out
                );
                println!("  answer: {}", truncate(&result.answer, 300));
                for ar in &result.assertions {
                    let mark = if ar.passed { "  ✓" } else { "  ✗" };
                    println!("{mark} {} — {}", ar.label, ar.detail);
                }
                println!();
                results.push(result);
            }
            Err(e) => {
                println!("  ❌ ERROR: {e}\n");
                results.push(CaseResult {
                    dir_name: case.dir_name.clone(),
                    passed: false,
                    assertions: vec![AssertionResult {
                        label: "fatal".into(),
                        passed: false,
                        detail: e,
                    }],
                    answer: String::new(),
                    tokens_in: 0,
                    tokens_out: 0,
                    duration_ms: 0,
                });
            }
        }
    }

    // Summary.
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let failed = total - passed;
    println!("───");
    println!("results: {total} total, {passed} passed, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}
