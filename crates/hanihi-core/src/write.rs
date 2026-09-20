//! Write-path tools: `apply_patch` and `write_file`.
//!
//! These are registered **only** when the CLI is invoked with `--write`.
//! They are scoped to the enclosing git repository via [`SourceTree`]:
//! escapes, git-ignored paths, `.ignore`, and anything under `.git/` are
//! refused. Changes land as local git commits — never pushed. Git is the
//! undo button and the audit trail.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use rig::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde_json::json;

use crate::source::SourceTree;
use crate::tool::{map_source_err, scrubbed_env};

/// Paths the agent may never write, relative to the repo root.
fn is_protected(rel: &str) -> bool {
    rel == ".ignore" || rel == ".gitignore" || rel.starts_with(".git/") || rel == ".git"
}

/// Run `git` with `args` in `root`, with the scrubbed environment, capturing
/// output. Returns `Ok((stdout, stderr))` on success, `Err(message)` on
/// failure (the more informative of stdout/stderr, trimmed).
///
/// Used only for `git add`/`git commit` after a patch is applied; the
/// unreliable `git apply` machinery is deliberately not used.
async fn git_run(root: &Path, args: &[&str]) -> Result<(String, String), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(root);
    cmd.env_clear();
    for (k, v) in scrubbed_env() {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.args(args);
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("spawn git {}: {e}", args[0]))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(msg.to_string());
    }
    Ok((stdout, stderr))
}

/// Tool: write a text file inside the repository.
pub fn builtin_write_file(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "write_file",
        "Write a text file inside the git repository. `path` is relative to the repo root. \
	 Refused: paths outside the repo, git-ignored paths, `.ignore`, `.gitignore`, anything \
	 under `.git/`. If `message` is given, the change is committed with that message. \
	 Local commits only — never pushed.",
        json!({
            "type": "object",
            "properties": {
            "path": {
            "type": "string",
            "description": "Path relative to the repo root"
            },
            "content": {
            "type": "string",
            "description": "Full file contents"
            },
            "message": {
            "type": "string",
            "description": "Optional commit message"
            }
            },
            "required": ["path", "content"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let rel = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolExecutionError::invalid_args("missing string field 'path'"))?
                    .to_string();
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'content'")
                    })?
                    .to_string();
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.trim().is_empty())
                    .map(String::from);

                if is_protected(&rel) {
                    return Err(ToolExecutionError::permission_denied(format!(
                        "path is protected: {rel}"
                    )));
                }

                // Escape/ignore checks via SourceTree (handles `..`, absolute
                // paths, symlink escapes, and ignore rules).
                let abs = tree
                    .resolve_for_write(Path::new(&rel))
                    .map_err(map_source_err)?;
                if tree.is_ignored(&abs) {
                    return Err(ToolExecutionError::permission_denied(format!(
                        "path is git-ignored: {}",
                        abs.display()
                    )));
                }

                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        ToolExecutionError::provider(format!("creating parent dirs: {e}"))
                    })?;
                }
                std::fs::write(&abs, &content).map_err(|e| {
                    ToolExecutionError::provider(format!("writing {}: {e}", abs.display()))
                })?;

                let mut out = format!("wrote {} ({} bytes)", abs.display(), content.len());
                if let Some(msg) = message {
                    git_run(tree.root(), &["add", "--", &rel])
                        .await
                        .map_err(ToolExecutionError::provider)?;
                    let (_, stderr) = git_run(tree.root(), &["commit", "-m", &msg])
                        .await
                        .map_err(ToolExecutionError::provider)?;
                    let note = if stderr.trim().is_empty() {
                        "committed".to_string()
                    } else {
                        format!("committed ({})", stderr.trim())
                    };
                    out.push_str(&format!("\n{note}"));
                }
                Ok(ToolOutput::text(out))
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::Fixture;

    #[tokio::test]
    async fn write_file_writes_new_file() {
        let fx = Fixture::new();
        let tool = builtin_write_file(fx.tree());
        let out = tool
            .execute(serde_json::json!({
            "path": "src/new.rs",
            "content": "pub fn added() {}\n"
            }))
            .await
            .expect("write succeeds");
        let rendered = out.render();
        assert!(rendered.contains("src/new.rs"), "got: {rendered}");
        let written = std::fs::read_to_string(fx.dir.join("src/new.rs")).unwrap();
        assert_eq!(written, "pub fn added() {}\n");
    }

    #[tokio::test]
    async fn write_file_refuses_protected_and_ignored() {
        let fx = Fixture::new();
        let tool = builtin_write_file(fx.tree());

        let err = tool
            .execute(serde_json::json!({
            "path": ".ignore",
            "content": "junk\n"
            }))
            .await
            .expect_err("protected path must fail");
        assert!(err.to_string().contains("protected"), "got: {err}");

        let err = tool
            .execute(serde_json::json!({
            "path": "target/debug/junk.rs",
            "content": "junk\n"
            }))
            .await
            .expect_err("ignored path must fail");
        assert!(err.to_string().contains("git-ignored"), "got: {err}");
    }

    #[tokio::test]
    async fn write_file_refuses_escapes() {
        let fx = Fixture::new();
        let tool = builtin_write_file(fx.tree());
        let name = format!("hanihi-write-outside-{}", uuid::Uuid::new_v4());
        let err = tool
            .execute(serde_json::json!({
            "path": format!("../{name}"),
            "content": "x"
            }))
            .await
            .expect_err("escape must fail");
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }
}
