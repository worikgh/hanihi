//! # hanihi-eval
//!
//! Eval runner for the hānihi agent harness. Discovers test cases from
//! `evals/cases/`, runs them against a live LLM, and checks assertions
//! against the session event log.

mod gate;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hanihi_core::agent::{Agent, DEFAULT_SYSTEM_PROMPT, StreamEvent, TurnSummary};
use hanihi_core::connect_chat_model;
use hanihi_core::error::AgentError;
use hanihi_core::session::log::LogEntry;
use hanihi_core::session::{Session, SessionManager};
use hanihi_core::{
    SourceTree, builtin_get_time, builtin_grep, builtin_list_dir, builtin_read_session_log,
    builtin_run_command, builtin_run_command_write, builtin_write_file,
};
use rig::completion::CompletionModel;
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

    /// Optional. Overrides the build command for this case. Defaults to
    /// `cargo check`. An argv vector: no shell is involved, so no
    /// metacharacters are interpreted. Paths should be relative to `repo`.
    #[serde(default)]
    build_command: Option<Vec<String>>,

    /// Optional. Overrides the test command for this case. Defaults to
    /// `cargo test`.
    #[serde(default)]
    test_command: Option<Vec<String>>,

    /// Optional. Overrides the lint command for this case. There is no
    /// default for a non-Rust case: `lint_clean` without this field is a
    /// configuration error, not a silent pass.
    #[serde(default)]
    lint_command: Option<Vec<String>>,

    /// Optional. Run before the build step to configure the project
    /// (e.g. `cmake -B build`). Absent means a single-step build.
    #[serde(default)]
    configure_command: Option<Vec<String>>,

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
    /// The case's build command exits 0 in the case's repo. Defaults to
    /// `cargo check`; overridden by `build_command`, preceded by
    /// `configure_command` when present.
    #[serde(rename = "build_succeeds")]
    BuildSucceeds,
    /// The case's test command exits 0 in the case's repo. Defaults to
    /// `cargo test`; overridden by `test_command`.
    #[serde(rename = "tests_pass")]
    TestsPass,
    /// The case's lint command exits 0 in the case's repo. Driven by
    /// `lint_command`; absent for a case that is not Rust, which is a
    /// configuration error. `clippy_clean` is accepted as a deprecated
    /// alias that implies the cargo clippy default.
    #[serde(rename = "lint_clean", alias = "clippy_clean")]
    LintClean,
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
    timeout: Duration,
) -> Vec<AssertionResult> {
    let mut results = Vec::with_capacity(case.assertions.len() + 1);
    for assertion in &case.assertions {
        results.push(evaluate_one(assertion, case, log, repo_dir, timeout).await);
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
    case: &Case,
    log: &[LogEntry],
    repo_dir: Option<&Path>,
    timeout: Duration,
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
            let Some(dir) = repo_dir else {
                return no_repo("build_succeeds");
            };
            let build = case
                .build_command
                .clone()
                .unwrap_or_else(|| default_argv(gate::DEFAULT_BUILD_COMMAND));
            let outcome = gate::run_configure_and_build(
                dir,
                case.configure_command.as_deref(),
                &build,
                timeout,
            )
            .await;
            AssertionResult {
                label: "build_succeeds".into(),
                passed: outcome.passed,
                detail: outcome.detail,
            }
        }
        Assertion::TestsPass => {
            let Some(dir) = repo_dir else {
                return no_repo("tests_pass");
            };
            let test = case
                .test_command
                .clone()
                .unwrap_or_else(|| default_argv(gate::DEFAULT_TEST_COMMAND));
            let desc = gate::describe(&test);
            let outcome = gate::run_gate(dir, &test, &desc, timeout).await;
            AssertionResult {
                label: "tests_pass".into(),
                passed: outcome.passed,
                detail: outcome.detail,
            }
        }
        Assertion::LintClean => {
            let Some(dir) = repo_dir else {
                return no_repo("lint_clean");
            };
            // An explicit lint_command always wins. Otherwise the cargo
            // clippy default applies only when the case really builds with
            // cargo; there is no defensible default for CMake, and a gate
            // that quietly succeeds when unconfigured would let a C++ case
            // claim lint coverage it never had.
            let lint = match case.lint_command.clone() {
                Some(lint) => lint,
                None if case.is_cargo() => default_argv(gate::DEFAULT_LINT_COMMAND),
                None => {
                    return AssertionResult {
                        label: "lint_clean".into(),
                        passed: false,
                        detail: "configuration error: lint_clean needs a lint_command \
                                 (no default exists for a non-Cargo case)"
                            .into(),
                    };
                }
            };
            let desc = gate::describe(&lint);
            let outcome = gate::run_gate(dir, &lint, &desc, timeout).await;
            AssertionResult {
                label: "lint_clean".into(),
                passed: outcome.passed,
                detail: outcome.detail,
            }
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

/// A gate assertion with no `repo` (or `fixture`) configured cannot run.
fn no_repo(label: &str) -> AssertionResult {
    AssertionResult {
        label: label.into(),
        passed: false,
        detail: "no repo configured for this case".into(),
    }
}

/// Materialise a `&[&str]` default command as an owned argv vector.
fn default_argv(command: &[&str]) -> Vec<String> {
    command.iter().map(|s| (*s).to_string()).collect()
}

impl Case {
    /// Whether this case builds with Cargo.
    ///
    /// Inferred from the build command's program rather than from a declared
    /// toolchain, because the eval runner deliberately does not model
    /// toolchains (see the plan's "four fields, not one enum" decision). This
    /// is used only to decide whether the Cargo lint default is defensible —
    /// never to pick a different command than the case asked for.
    fn is_cargo(&self) -> bool {
        let program = match self.build_command.as_deref() {
            Some([program, ..]) => program.as_str(),
            _ => gate::DEFAULT_BUILD_COMMAND[0],
        };
        program == "cargo"
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

/// Drive one turn on the streaming path and return its [`TurnSummary`].
///
/// The eval runner has no interest in incremental output — it wants the same
/// summary `Session::run` used to return so the assertions below are unchanged.
/// Draining the channel here keeps the non-streaming path retired: the runner
/// now exercises exactly the code the CLI exercises.
async fn run_turn_streaming<M: CompletionModel + 'static>(
    session: &mut Session,
    agent: &mut Agent<M>,
    provider: &str,
    model: &str,
    user_input: &str,
) -> Result<TurnSummary, AgentError>
where
    M::StreamingResponse: Send,
{
    let mut rx = session
        .run_streaming(agent, provider, model, user_input)
        .await?;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TurnComplete { summary } => {
                agent.set_history(summary.final_history.clone());
                agent.set_summary(summary.final_summary.clone());
                return Ok(summary);
            }
            // A turn-ending abort. The channel closes next; report it as the
            // error it is rather than falling through to the closed-channel
            // case below, which would misreport an abort as a completed turn.
            StreamEvent::Error { message, .. } => return Err(AgentError::Rig(message)),
            _ => {}
        }
    }

    Err(AgentError::Rig(
        "streaming turn ended without a TurnComplete event".into(),
    ))
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
        agent.add_tool(builtin_list_dir(tree.clone()));
        agent.add_tool(builtin_grep(tree.clone()));
        if case.write_tools {
            agent.add_tool(builtin_run_command_write(tree.clone(), traces_dir));
        } else {
            agent.add_tool(builtin_run_command(tree.clone(), traces_dir));
        }
        agent.add_tool(builtin_read_session_log(log_path));
        if case.write_tools {
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
        run_turn_streaming(session, &mut agent, provider, model, &case.user_input),
    )
    .await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(summary)) => {
            // Read the event log.
            let log_path = session.root().join("events.jsonl");
            let log = parse_event_log(&log_path)?;

            // Gates share the per-case timeout: a build that hangs must not
            // outlive the case that owns it.
            let assertions = evaluate(
                case,
                &log,
                repo_dir.as_deref(),
                start,
                Duration::from_secs(timeout_secs),
            )
            .await;

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

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds a temp fixture directory and removes it on drop.
    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create temp fixture");
            Self { path }
        }

        fn write(&self, rel: &str, contents: &str) {
            let full = self.path.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(full, contents).expect("write fixture file");
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A minimal Cargo project that builds and tests cleanly.
    fn rust_repo() -> TempRepo {
        let repo = TempRepo::new("eval-rust");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/main.rs", "fn main() {}\n");
        repo
    }

    /// A minimal CMake project with one test registered via `add_test`.
    fn cmake_repo() -> TempRepo {
        let repo = TempRepo::new("eval-cmake");
        repo.write(
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.16)\n\
             project(fixture CXX)\n\
             enable_testing()\n\
             add_executable(fixture src/foo.cpp)\n\
             add_test(NAME fixture_runs COMMAND fixture)\n",
        );
        repo.write(
            "src/foo.cpp",
            "#include <cstdio>\n\
             int add(int a, int b) { return a + b; }\n\
             int main() {\n\
                 if (add(1, 2) != 3) { std::fprintf(stderr, \"bad sum\\n\"); return 1; }\n\
                 std::printf(\"ok\\n\");\n\
                 return 0;\n\
             }\n",
        );
        repo
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// Fail loudly when a required external tool is missing.
    ///
    /// A silently-skipped test is indistinguishable from a passing one, so
    /// this names the missing tool instead of `return`ing early.
    fn require_tool(tool: &str) {
        let found = std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()));
        assert!(
            found,
            "{tool} not found on PATH: install a C++ toolchain (cmake + a C++ \
             compiler) to run the C++ gate tests"
        );
    }

    fn cmake_case(repo: &TempRepo) -> Case {
        let toml = "\
            user_input = \"probe\"\n\
            configure_command = [\"cmake\", \"-B\", \"build\"]\n\
            build_command = [\"cmake\", \"--build\", \"build\"]\n\
            test_command = [\"ctest\", \"--test-dir\", \"build\", \"--output-on-failure\"]\n\
            [[assertions]]\n\
            type = \"build_succeeds\"\n";
        let mut case: Case = toml::from_str(toml).expect("parse cmake case");
        case.case_dir = repo.path().to_path_buf();
        case
    }

    /// Evaluate a single assertion against an empty log.
    async fn eval(case: &Case, assertion: &Assertion, repo: Option<&Path>) -> AssertionResult {
        evaluate_one(assertion, case, &[], repo, Duration::from_secs(120)).await
    }

    // 1
    #[tokio::test]
    async fn build_succeeds_defaults_to_cargo_check() {
        require_tool("cargo");
        let repo = rust_repo();
        let toml = "\
            user_input = \"probe\"\n\
            [[assertions]]\n\
            type = \"build_succeeds\"\n";
        let case: Case = toml::from_str(toml).expect("parse");
        let result = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(result.passed, "detail: {}", result.detail);
        assert!(
            result.detail.contains("build exited 0"),
            "{}",
            result.detail
        );
    }

    // 2
    #[tokio::test]
    async fn build_succeeds_uses_the_case_build_command() {
        require_tool("cmake");
        let repo = cmake_repo();
        let case = cmake_case(&repo);
        let result = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(result.passed, "detail: {}", result.detail);
    }

    // 3
    #[tokio::test]
    async fn build_succeeds_fails_with_compiler_output() {
        require_tool("cmake");
        let repo = cmake_repo();
        // Deliberate C++ error: a missing semicolon, which clang++/g++ report
        // with the source text and the expected token.
        repo.write("src/foo.cpp", "int main() { int x = 1 return 0; }\n");
        let case = cmake_case(&repo);
        let result = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(!result.passed, "expected failure, got {}", result.detail);
        assert!(
            result.detail.contains("error"),
            "failure must carry the compiler diagnostic, got: {}",
            result.detail
        );
        assert!(
            !result.detail.trim_end().ends_with("exit code 1"),
            "failure must not be a bare exit code: {}",
            result.detail
        );
    }

    // 4
    #[tokio::test]
    async fn configure_failure_skips_the_build_step() {
        require_tool("cmake");
        let repo = cmake_repo();
        let toml = "\
            user_input = \"probe\"\n\
            configure_command = [\"cmake\", \"-B\", \"build\", \"-DSOME_UNKNOWN_OPTION=1\", \"--bad-flag\"]\n\
            build_command = [\"cmake\", \"--build\", \"build\"]\n\
            [[assertions]]\n\
            type = \"build_succeeds\"\n";
        let mut case: Case = toml::from_str(toml).expect("parse");
        case.case_dir = repo.path().to_path_buf();
        let result = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(!result.passed);
        assert!(
            result.detail.contains("configure"),
            "diagnostic must name configure, not build: {}",
            result.detail
        );
        assert!(
            !result.detail.contains("build failed"),
            "build step must be skipped after a configure failure: {}",
            result.detail
        );
    }

    // 5
    #[tokio::test]
    async fn tests_pass_uses_the_case_test_command() {
        require_tool("cmake");
        let repo = cmake_repo();
        let case = cmake_case(&repo);
        // The test command needs a configured build tree.
        let build = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(build.passed, "setup build failed: {}", build.detail);
        let result = eval(&case, &Assertion::TestsPass, Some(repo.path())).await;
        assert!(result.passed, "detail: {}", result.detail);
    }

    // 6
    #[tokio::test]
    async fn tests_pass_fails_when_a_test_fails() {
        require_tool("cmake");
        let repo = cmake_repo();
        // The test binary now exits non-zero, so ctest reports a failure.
        repo.write(
            "src/foo.cpp",
            "#include <cstdio>\n\
             int main() { std::fprintf(stderr, \"deliberate test failure\\n\"); return 3; }\n",
        );
        let case = cmake_case(&repo);
        let build = eval(&case, &Assertion::BuildSucceeds, Some(repo.path())).await;
        assert!(build.passed, "setup build failed: {}", build.detail);
        let result = eval(&case, &Assertion::TestsPass, Some(repo.path())).await;
        assert!(!result.passed, "expected failure, got {}", result.detail);
        assert!(
            result.detail.contains("deliberate test failure") || result.detail.contains("Failed"),
            "test output must be surfaced, got: {}",
            result.detail
        );
    }

    // 7
    #[test]
    fn clippy_clean_still_parses_as_the_cargo_lint_gate() {
        let toml = "\
            user_input = \"probe\"\n\
            [[assertions]]\n\
            type = \"clippy_clean\"\n";
        let case: Case = toml::from_str(toml).expect("clippy_clean must still parse");
        assert_eq!(case.assertions.len(), 1);
        assert!(
            matches!(case.assertions[0], Assertion::LintClean),
            "alias must map to LintClean"
        );
    }

    #[test]
    fn lint_clean_spelling_parses_too() {
        let toml = "\
            user_input = \"probe\"\n\
            [[assertions]]\n\
            type = \"lint_clean\"\n";
        let case: Case = toml::from_str(toml).expect("lint_clean must parse");
        assert!(matches!(case.assertions[0], Assertion::LintClean));
    }

    // 8
    #[tokio::test]
    async fn lint_clean_without_lint_command_is_a_configuration_error() {
        let repo = cmake_repo();
        let case = cmake_case(&repo);
        let result = eval(&case, &Assertion::LintClean, Some(repo.path())).await;
        assert!(
            !result.passed,
            "unconfigured lint gate must fail, not pass silently: {}",
            result.detail
        );
        assert!(
            result.detail.contains("lint_command"),
            "message must name the missing field: {}",
            result.detail
        );
    }

    // 9
    #[tokio::test]
    async fn lint_clean_uses_the_case_lint_command() {
        let repo = cmake_repo();
        let ok = "\
            user_input = \"probe\"\n\
            lint_command = [\"true\"]\n\
            [[assertions]]\n\
            type = \"lint_clean\"\n";
        let mut case: Case = toml::from_str(ok).expect("parse");
        case.case_dir = repo.path().to_path_buf();
        let result = eval(&case, &Assertion::LintClean, Some(repo.path())).await;
        assert!(
            result.passed,
            "lint_command that exits 0 must pass: {}",
            result.detail
        );

        let bad = "\
            user_input = \"probe\"\n\
            lint_command = [\"false\"]\n\
            [[assertions]]\n\
            type = \"lint_clean\"\n";
        let mut case: Case = toml::from_str(bad).expect("parse");
        case.case_dir = repo.path().to_path_buf();
        let result = eval(&case, &Assertion::LintClean, Some(repo.path())).await;
        assert!(!result.passed, "lint_command that exits 1 must fail");
    }

    // 10
    #[tokio::test]
    async fn gate_commands_run_in_the_case_repo() {
        let repo = cmake_repo();
        // Write a marker into the repo; the gate must observe it relative to
        // the repo, proving cwd is the case repo and not the runner's cwd.
        repo.write("marker.txt", "present\n");
        let test = "\
            user_input = \"probe\"\n\
            test_command = [\"test\", \"-f\", \"marker.txt\"]\n\
            [[assertions]]\n\
            type = \"tests_pass\"\n";
        let mut case: Case = toml::from_str(test).expect("parse");
        case.case_dir = repo.path().to_path_buf();
        let result = eval(&case, &Assertion::TestsPass, Some(repo.path())).await;
        assert!(
            result.passed,
            "gate must run with cwd = case repo: {}",
            result.detail
        );

        // The same command from the wrong cwd would not find the marker.
        let elsewhere = TempRepo::new("eval-elsewhere");
        let result = eval(&case, &Assertion::TestsPass, Some(elsewhere.path())).await;
        assert!(
            !result.passed,
            "sanity: the marker is absent from an unrelated directory"
        );
    }

    // 11
    #[test]
    fn case_toml_parses_the_new_optional_fields() {
        let full = "\
            user_input = \"probe\"\n\
            configure_command = [\"cmake\", \"-B\", \"build\"]\n\
            build_command = [\"cmake\", \"--build\", \"build\"]\n\
            test_command = [\"ctest\", \"--test-dir\", \"build\", \"--output-on-failure\"]\n\
            lint_command = [\"clang-tidy\", \"-p\", \"build\", \"src/foo.cpp\"]\n\
            [[assertions]]\n\
            type = \"build_succeeds\"\n";
        let case: Case = toml::from_str(full).expect("parse full case");
        assert_eq!(
            case.build_command,
            Some(argv(&["cmake", "--build", "build"]))
        );
        assert_eq!(
            case.test_command,
            Some(argv(&[
                "ctest",
                "--test-dir",
                "build",
                "--output-on-failure"
            ]))
        );
        assert_eq!(
            case.lint_command,
            Some(argv(&["clang-tidy", "-p", "build", "src/foo.cpp"]))
        );
        assert_eq!(
            case.configure_command,
            Some(argv(&["cmake", "-B", "build"]))
        );

        let bare = "\
            user_input = \"probe\"\n\
            [[assertions]]\n\
            type = \"no_error\"\n";
        let case: Case = toml::from_str(bare).expect("parse bare case");
        assert!(case.build_command.is_none());
        assert!(case.test_command.is_none());
        assert!(case.lint_command.is_none());
        assert!(case.configure_command.is_none());
    }
}
