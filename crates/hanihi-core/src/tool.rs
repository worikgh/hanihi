//! Built-in tools for the agent.
//!
//! Tools are rig [`PortableDynamicTool`]s: name + description + JSON schema +
//! an async callback taking raw `serde_json::Value` arguments.

use std::path::Path;
use std::sync::Arc;

use rig::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde_json::json;

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
}
