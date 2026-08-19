//! Built-in tools for the agent.
//!
//! Tools are rig [`PortableDynamicTool`]s: name + description + JSON schema +
//! an async callback taking raw `serde_json::Value` arguments.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use rig::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde_json::json;
use tokio::io::AsyncReadExt as _;

use crate::source::{SourceError, SourceTree};

/// Map a [`SourceError`] onto a rig tool error with the right kind.
fn map_source_err(e: SourceError) -> ToolExecutionError {
    match e {
        SourceError::NotFound(p) => {
            ToolExecutionError::not_found(format!("no such path: {}", p.display()))
        }
        SourceError::Ignored(p) => {
            ToolExecutionError::permission_denied(format!("path is git-ignored: {}", p.display()))
        }
        SourceError::Escape(p) => ToolExecutionError::permission_denied(format!(
            "path escapes the repository: {}",
            p.display()
        )),
        other => ToolExecutionError::provider(other.to_string()),
    }
}

/// Tool: report the current local date and time.
pub fn builtin_get_time() -> PortableDynamicTool {
    PortableDynamicTool::new(
        "get_time",
        "Get the current local date and time in RFC 3339 format.",
        json!({
            "type": "object",
            "properties": {}
        }),
        |_args: serde_json::Value| {
            Box::pin(async move {
                let now = chrono::Local::now().to_rfc3339();
                Ok(ToolOutput::text(now))
            })
        },
    )
}

/// Tool: echo the provided text back verbatim.
pub fn builtin_echo() -> PortableDynamicTool {
    PortableDynamicTool::new(
        "echo",
        "Echo the provided text back verbatim.",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo" }
            },
            "required": ["text"]
        }),
        |args: serde_json::Value| {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(ToolOutput::text(text))
            })
        },
    )
}

/// Tool: read a text file from the git repository.
pub fn builtin_read_file(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "read_file",
        "Read a text file from the git repository. `path` is relative to the repo root. \
         Git-ignored paths cannot be read. Returns up to 64 KiB.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the repo root" }
            },
            "required": ["path"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let rel = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolExecutionError::invalid_args("missing string field 'path'")
                })?;
                tree.read(Path::new(rel))
                    .map(ToolOutput::text)
                    .map_err(map_source_err)
            })
        },
    )
}

/// Tool: list files and directories in the git repository.
pub fn builtin_list_dir(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "list_dir",
        "List files and directories in the git repository. `path` is relative to the repo root \
         (default: the root itself). `depth` controls recursion (default 1, max 4). \
         Git-ignored paths are never listed. One line per entry: type + path.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory relative to repo root" },
                "depth": { "type": "integer", "description": "Recursion depth (1-4)" }
            }
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let depth = args
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 4) as usize;
                let walk = tree.walk(Path::new(rel), depth).map_err(map_source_err)?;
                let mut lines = Vec::new();
                for entry in walk {
                    let entry = entry.map_err(|e| ToolExecutionError::provider(e.to_string()))?;
                    if entry.depth() == 0 {
                        continue;
                    }
                    let ty = match entry.file_type() {
                        Some(t) if t.is_dir() => "dir ",
                        Some(t) if t.is_file() => "file",
                        Some(t) if t.is_symlink() => "link",
                        _ => "oth ",
                    };
                    let p = entry
                        .path()
                        .strip_prefix(tree.root())
                        .unwrap_or(entry.path());
                    lines.push(format!("{ty} {}", p.display()));
                }
                Ok(ToolOutput::text(lines.join("\n")))
            })
        },
    )
}

// ── run_command ──────────────────────────────────────────────────

/// Default timeout for `run_command`.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Maximum allowed timeout for `run_command`.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Per-process sequence number for trace filenames. The tool cannot see the
/// session turn number, so unix-ms + a per-process counter disambiguates
/// traces within a session directory.
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of executing a command (spawn, wait, capture, timeout).
struct CommandOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    stdout: String,
    stderr: String,
}

/// Validate a whitespace-split command against the allowlist.
///
/// `argv[0]` must be `cargo` or `git`. Cargo subcommands are limited to
/// `check`, `build`, `test`, `clippy`, `fmt`, `doc`, and `run -p hanihi-eval`.
/// Git is limited to the read-only verbs `status`, `diff`, `log`, `show`, and
/// `apply --check`. Flags that would change the working directory
/// (`--manifest-path`, `-C`/`--directory`) are rejected outright — the cwd is
/// pinned to the repo root and nothing may escape it.
fn check_command_argv(argv: &[String]) -> Result<(), String> {
    let Some(program) = argv.first() else {
        return Err("empty command".into());
    };
    match program.as_str() {
        "cargo" => {
            let Some(sub) = argv.get(1) else {
                return Err(
                    "cargo requires a subcommand (check, build, test, clippy, fmt, doc, run)"
                        .into(),
                );
            };
            if argv.iter().any(|a| a == "--manifest-path") {
                return Err(
                    "cargo --manifest-path is not allowed (cwd is pinned to the repo root)".into(),
                );
            }
            match sub.as_str() {
                "check" | "build" | "test" | "clippy" | "fmt" | "doc" => Ok(()),
                "run" => {
                    // Only `cargo run -p hanihi-eval` is permitted.
                    let mut package: Option<&str> = None;
                    let mut iter = argv[1..].iter().peekable();
                    while let Some(a) = iter.next() {
                        if a == "--" {
                            break;
                        }
                        if a == "-p" || a == "--package" {
                            package = iter.peek().map(|s| s.as_str());
                        }
                    }
                    match package {
                        Some("hanihi-eval") => Ok(()),
                        Some(other) => Err(format!(
                            "cargo run is restricted to -p hanihi-eval (got -p {other})"
                        )),
                        None => Err("cargo run requires -p hanihi-eval".into()),
                    }
                }
                other => Err(format!("cargo subcommand '{other}' is not allowed")),
            }
        }
        "git" => {
            let Some(sub) = argv.get(1) else {
                return Err("git requires a subcommand (status, diff, log, show, apply)".into());
            };
            if argv.iter().any(|a| a == "-C" || a == "--directory") {
                return Err(
                    "git -C/--directory is not allowed (cwd is pinned to the repo root)".into(),
                );
            }
            match sub.as_str() {
                "status" | "diff" | "log" | "show" => Ok(()),
                "apply" => {
                    if !argv.iter().any(|a| a == "--check") {
                        return Err("git apply is restricted to --check".into());
                    }
                    Ok(())
                }
                other => Err(format!("git subcommand '{other}' is not allowed")),
            }
        }
        other => Err(format!(
            "command '{other}' is not allowed (only cargo and git)"
        )),
    }
}

/// Build the minimal environment passed to child processes: PATH, HOME, and
/// CARGO_* / RUSTUP_* (needed for cargo/rustup to resolve the toolchain).
/// Everything else (API keys, etc.) is dropped.
fn scrubbed_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| {
            k == "PATH" || k == "HOME" || k.starts_with("CARGO_") || k.starts_with("RUSTUP_")
        })
        .collect()
}

/// Read a pipe to EOF as a lossy UTF-8 string.
async fn read_all<R>(mut reader: R) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Cap a string at `MAX_READ_BYTES`, appending a truncation note.
fn cap_output(s: &str) -> String {
    if s.len() <= crate::source::MAX_READ_BYTES {
        s.to_string()
    } else {
        let cut = s.floor_char_boundary(crate::source::MAX_READ_BYTES);
        let mut out = String::with_capacity(cut + 64);
        out.push_str(&s[..cut]);
        out.push_str(&format!("\n…[truncated, {} bytes total]", s.len()));
        out
    }
}

/// Spawn `argv`, capture stdout/stderr, enforce a timeout (killing the child
/// on expiry), and return the outcome. The allowlist check happens in the
/// tool wrapper; this runs whatever argv it is given (used directly by tests).
async fn execute_captured(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutcome, String> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env_clear();
    for (k, v) in scrubbed_env() {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn '{}': {e}", argv[0]))?;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    // Read both pipes concurrently with the wait so a large stream on one
    // pipe cannot deadlock the other.
    let stdout_task = tokio::spawn(read_all(stdout_pipe));
    let stderr_task = tokio::spawn(read_all(stderr_pipe));

    let started = Instant::now();
    let status_result = tokio::time::timeout(timeout, child.wait()).await;

    let (exit_code, timed_out) = match status_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(format!("waiting for '{}': {e}", argv[0])),
        Err(_elapsed) => {
            // Kill the child on expiry; the readers hit EOF and complete.
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|e| format!("stdout task failed: {e}"))?
        .map_err(|e| format!("reading stdout: {e}"))?;
    let stderr = stderr_task
        .await
        .map_err(|e| format!("stderr task failed: {e}"))?
        .map_err(|e| format!("reading stderr: {e}"))?;

    Ok(CommandOutcome {
        exit_code,
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout,
        stderr,
    })
}

/// Persist a command execution to `<traces-dir>/<unix-ms>-<seq>.cmd.json`.
fn write_trace(
    traces_dir: &Path,
    command: &str,
    outcome: &CommandOutcome,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(traces_dir)?;
    let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = Utc::now().timestamp_millis();
    let name = format!("{ms}-{seq}.cmd.json");
    let path = traces_dir.join(name);
    let obj = json!({
        "ts": Utc::now().to_rfc3339(),
        "command": command,
        "exit_code": outcome.exit_code,
        "duration_ms": outcome.duration_ms,
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&obj)?)?;
    Ok(path)
}

/// Render the tool result: exit code + duration + truncated output + trace path.
fn render_command_result(command: &str, outcome: &CommandOutcome, trace_path: &Path) -> String {
    let code = match outcome.exit_code {
        Some(c) => c.to_string(),
        None if outcome.timed_out => "killed (timeout)".to_string(),
        None => "killed (signal)".to_string(),
    };
    let mut out = format!(
        "command: {command}\nexit code: {code}\nduration: {}ms\n",
        outcome.duration_ms
    );
    if !outcome.stdout.is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(&cap_output(&outcome.stdout));
        out.push('\n');
    }
    if !outcome.stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&cap_output(&outcome.stderr));
        out.push('\n');
    }
    out.push_str(&format!("trace: {}", trace_path.display()));
    out
}

/// Tool: run an allowlisted `cargo`/`git` command inside the repo root.
///
/// Registered always (it is an analysis/build tool, not a write tool). The
/// command is split on whitespace (no shell), validated against an allowlist,
/// executed with a scrubbed environment and a timeout, and the full output is
/// persisted to a trace file. The result carries exit code + duration +
/// truncated stdout/stderr + the trace path.
pub fn builtin_run_command(tree: Arc<SourceTree>, traces_dir: PathBuf) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "run_command",
        "Run an allowlisted command inside the repository root. No shell: the command is \
         split on whitespace into argv. cargo subcommands: check, build, test, clippy, fmt, \
         doc, run -p hanihi-eval. git subcommands: status, diff, log, show, apply --check. \
         cwd is pinned to the repo root; the environment is scrubbed (PATH, HOME, CARGO_* \
         only); output is capped at 64 KiB; the full output is written to a trace file. \
         Returns exit code, duration, stdout/stderr (truncated), and the trace path.",
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Allowlisted command, e.g. \"cargo check --workspace\""
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Max seconds (default 120, max 600)"
                }
            },
            "required": ["command"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            let traces_dir = traces_dir.clone();
            Box::pin(async move {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'command'")
                    })?
                    .to_string();
                let timeout_secs = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .clamp(1, MAX_TIMEOUT_SECS);

                let argv: Vec<String> = command.split_whitespace().map(String::from).collect();
                if argv.is_empty() {
                    return Err(ToolExecutionError::invalid_args("empty command"));
                }
                check_command_argv(&argv).map_err(ToolExecutionError::permission_denied)?;

                let outcome =
                    execute_captured(&argv, tree.root(), Duration::from_secs(timeout_secs))
                        .await
                        .map_err(ToolExecutionError::provider)?;
                let trace_path = write_trace(&traces_dir, &command, &outcome)
                    .map_err(|e| ToolExecutionError::provider(format!("writing trace: {e}")))?;
                Ok(ToolOutput::text(render_command_result(
                    &command,
                    &outcome,
                    &trace_path,
                )))
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::source::testutil::Fixture;
    use rig::test_utils::{MockCompletionModel, MockTurn};

    #[tokio::test]
    async fn read_file_tool_returns_contents() {
        let fx = Fixture::new();
        let tool = builtin_read_file(fx.tree());
        let out = tool
            .execute(serde_json::json!({ "path": "src/main.rs" }))
            .await
            .expect("read succeeds");
        assert!(out.render().contains("fn main"));
    }

    #[tokio::test]
    async fn read_file_tool_denies_ignored_and_escapes() {
        let fx = Fixture::new();
        let tool = builtin_read_file(fx.tree());
        let err = tool
            .execute(serde_json::json!({ "path": "target/debug/junk.rs" }))
            .await
            .expect_err("ignored path must fail");
        assert!(err.to_string().contains("git-ignored"), "got: {err}");

        let name = format!("hanihi-outside-{}", uuid::Uuid::new_v4());
        let outside = std::env::temp_dir().join(&name);
        std::fs::write(&outside, "secret").unwrap();
        let err = tool
            .execute(serde_json::json!({ "path": format!("../{name}") }))
            .await
            .expect_err("escape must fail");
        assert!(err.to_string().contains("escapes"), "got: {err}");
        std::fs::remove_file(&outside).unwrap_or(());
    }

    #[tokio::test]
    async fn read_file_tool_rejects_bad_args() {
        let fx = Fixture::new();
        let tool = builtin_read_file(fx.tree());
        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("missing path must fail");
        assert!(err.to_string().contains("path"), "got: {err}");
    }

    #[tokio::test]
    async fn list_dir_tool_lists_only_visible() {
        let fx = Fixture::new();
        let tool = builtin_list_dir(fx.tree());
        let out = tool
            .execute(serde_json::json!({ "path": ".", "depth": 2 }))
            .await
            .expect("list succeeds")
            .render();
        assert!(out.contains("src"), "got: {out}");
        assert!(out.contains("Cargo.toml"), "got: {out}");
        assert!(!out.contains("target"), "got: {out}");
        assert!(!out.contains("junk"), "got: {out}");
    }

    #[tokio::test]
    async fn read_file_round_trip_through_agent() {
        let fx = Fixture::new();
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call(
                "call_1",
                "read_file",
                serde_json::json!({ "path": "src/main.rs" }),
            ),
            MockTurn::text("read the file"),
        ]);
        let mut agent = Agent::new(model, "test system");
        agent.add_tool(builtin_read_file(fx.tree()));

        let summary = agent.run("read src/main.rs").await.expect("run succeeds");
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.text, "read the file");
    }

    // ── run_command ──

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_allowlist_accepts_cargo_and_git() {
        for cmd in [
            argv(&["cargo", "check", "--workspace"]),
            argv(&["cargo", "build"]),
            argv(&["cargo", "test", "-p", "hanihi-core"]),
            argv(&["cargo", "clippy", "--", "-D", "warnings"]),
            argv(&["cargo", "fmt"]),
            argv(&["cargo", "doc"]),
            argv(&["cargo", "run", "-p", "hanihi-eval"]),
            argv(&["git", "status", "--short"]),
            argv(&["git", "diff"]),
            argv(&["git", "log", "--oneline", "-20"]),
            argv(&["git", "show", "HEAD"]),
            argv(&["git", "apply", "--check"]),
        ] {
            check_command_argv(&cmd).expect("must be allowed");
        }
    }

    #[test]
    fn command_allowlist_denies_unknown_and_disallowed() {
        let cases = [
            argv(&[]),
            argv(&["rm", "-rf", "/"]),
            argv(&["sh", "-c", "echo hi"]),
            argv(&["cargo", "publish"]),
            argv(&["cargo", "install", "x"]),
            argv(&["cargo", "run"]),
            argv(&["cargo", "run", "-p", "hanihi-cli"]),
            argv(&["git", "push", "origin", "main"]),
            argv(&["git", "reset", "--hard"]),
            argv(&["git", "apply"]),
        ];
        for cmd in cases {
            assert!(check_command_argv(&cmd).is_err(), "should deny: {cmd:?}");
        }
    }

    #[test]
    fn command_allowlist_denies_cwd_escapes() {
        // -C / --directory (git) and --manifest-path (cargo) escape the
        // pinned repo root.
        assert!(check_command_argv(&argv(&["git", "-C", "/tmp", "status"])).is_err());
        assert!(check_command_argv(&argv(&["git", "--directory", "/tmp", "status"])).is_err());
        assert!(
            check_command_argv(&argv(&[
                "cargo",
                "check",
                "--manifest-path",
                "/etc/Cargo.toml"
            ]))
            .is_err()
        );
    }

    #[test]
    fn cap_output_truncates_at_limit() {
        let big = "x".repeat(crate::source::MAX_READ_BYTES + 4096);
        let out = cap_output(&big);
        assert!(out.contains("[truncated"), "got: {out}");
        assert!(out.len() < big.len());
    }

    #[tokio::test]
    async fn execute_captured_times_out_and_kills_child() {
        let cwd = std::env::temp_dir();
        let outcome = execute_captured(&argv(&["sleep", "30"]), &cwd, Duration::from_secs(1))
            .await
            .expect("sleep runs");
        assert!(outcome.timed_out, "expected a timeout");
        assert_eq!(outcome.exit_code, None);
        assert!(outcome.duration_ms >= 1000);
    }

    /// A real git repository (with an initial commit) for command tests.
    fn git_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hanihi-cmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git runs")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn run_command_tool_runs_git_and_writes_trace() {
        let repo = git_repo();
        let tree = Arc::new(SourceTree::open_at(&repo).expect("open repo"));
        let traces_dir = repo.join("traces");
        let tool = builtin_run_command(tree, traces_dir.clone());

        let out = tool
            .execute(serde_json::json!({ "command": "git status --short" }))
            .await
            .expect("git status succeeds")
            .render();
        assert!(out.contains("exit code: 0"), "got: {out}");
        assert!(out.contains("trace:"), "got: {out}");

        // A trace file was written.
        let mut found = false;
        for entry in std::fs::read_dir(&traces_dir).expect("traces dir") {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().ends_with(".cmd.json") {
                let raw = std::fs::read_to_string(entry.path()).unwrap();
                assert!(raw.contains("\"command\": \"git status --short\""));
                found = true;
            }
        }
        assert!(found, "expected a trace file in {}", traces_dir.display());
        std::fs::remove_dir_all(&repo).unwrap_or(());
    }

    #[tokio::test]
    async fn run_command_tool_denies_disallowed_command() {
        let repo = git_repo();
        let tree = Arc::new(SourceTree::open_at(&repo).expect("open repo"));
        let tool = builtin_run_command(tree, repo.join("traces"));

        let err = tool
            .execute(serde_json::json!({ "command": "git push origin main" }))
            .await
            .expect_err("git push must be denied");
        assert!(err.to_string().contains("not allowed"), "got: {err}");
        std::fs::remove_dir_all(&repo).unwrap_or(());
    }
}
