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
use tokio::io::AsyncWriteExt as _;

use crate::source::SourceTree;
use crate::tool::{map_source_err, scrubbed_env};

/// Paths the agent may never write, relative to the repo root.
fn is_protected(rel: &str) -> bool {
    rel == ".ignore" || rel == ".gitignore" || rel.starts_with(".git/") || rel == ".git"
}

/// Run `git` with `args` in `root`, with the scrubbed environment, capturing
/// output. Returns `Ok((stdout, stderr))` on success, `Err(message)` on
/// failure (the more informative of stdout/stderr, trimmed).
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

/// Run `git apply` over the diff on stdin with the given extra args,
/// returning `Ok(())` on success or the trimmed git error.
async fn git_apply(root: &Path, diff: &str, extra: &[&str]) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(root);
    cmd.env_clear();
    for (k, v) in scrubbed_env() {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.arg("apply");
    cmd.args(extra);
    cmd.arg("-");

    let mut child = cmd.spawn().map_err(|e| format!("spawn git apply: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git apply stdin unavailable")?;
    stdin
        .write_all(diff.as_bytes())
        .await
        .map_err(|e| format!("writing diff to git apply: {e}"))?;
    drop(stdin); // EOF

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("waiting for git apply: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(msg.to_string());
    }
    Ok(())
}

/// Validate a unified diff against the repo (`git apply --check`), then
/// apply it (`git apply`). `--recount` makes git recompute hunk line counts
/// from the bodies instead of trusting the `@@ -a,b +c,d @@` numbers, so
/// hand-composed diffs with slightly off counts still apply.
///
/// `git apply` uses a 3-way merge against the index, so it already tolerates
/// a dirty working tree — a patch applies cleanly on top of uncommitted
/// edits to the same file, exactly as the model's self-improvement loop
/// needs. There is no pre-emptive "tree must be clean" refusal here; if the
/// diff genuinely does not apply, `--check` reports precisely why.
async fn git_apply_diff(root: &Path, diff: &str) -> Result<(), String> {
    git_apply(root, diff, &["--check", "--recount"])
        .await
        .map_err(|msg| format!("git apply --check failed: {msg}"))?;
    git_apply(root, diff, &["--recount"])
        .await
        .map_err(|msg| format!("git apply failed: {msg}"))?;
    Ok(())
}

/// Is the patch's change already present in the working tree? Git applies
/// the reversed diff cleanly exactly when the current tree already contains
/// the result the patch produces — the signal the old dirty-target guard was
/// really after ("the change already exists, stop re-patching it").
async fn diff_already_applied(root: &Path, diff: &str) -> bool {
    git_apply(root, diff, &["--check", "--reverse", "--recount"]).await.is_ok()
}

/// Extract the repo-relative path from a `--- a/…` or `+++ b/…` line,
/// stripping any trailing tab metadata (timestamps from `git diff`).
fn diff_path_from_marker(rest: &str) -> String {
    rest.split('\t').next().unwrap_or(rest).trim().to_string()
}

/// Reject diffs that touch protected paths (`.ignore`, `.git*`).
fn diff_touches_protected(diff: &str) -> Option<String> {
    for line in diff.lines() {
        for prefix in ["+++ b/", "--- a/"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let rel = diff_path_from_marker(rest);
                if is_protected(&rel) {
                    return Some(rel);
                }
            }
        }
    }
    None
}

/// Tool: apply a unified diff to the repository working tree.
pub fn builtin_apply_patch(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "apply_patch",
        "Apply a unified diff (git diff format) to the repository working tree. Validates with \
         `git apply --check --recount` first; on failure the git error is returned so the patch \
         can be fixed. Applies via a 3-way merge, so it works on top of other uncommitted \
         changes — no need for a clean tree first. If `message` is given, the change is \
         committed with that message. Diffs that touch `.ignore` or `.git*` paths are \
         refused. Changes are local commits only — never pushed.",
        json!({
            "type": "object",
            "properties": {
                "diff": {
                    "type": "string",
                    "description": "Unified diff against the current working tree"
                },
                "message": {
                    "type": "string",
                    "description": "Optional commit message"
                }
            },
            "required": ["diff"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let diff = args
                    .get("diff")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'diff'")
                    })?
                    .to_string();
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.trim().is_empty())
                    .map(String::from);

                if let Some(rel) = diff_touches_protected(&diff) {
                    return Err(ToolExecutionError::permission_denied(format!(
                        "diff touches protected path: {rel}"
                    )));
                }

                git_apply_diff(tree.root(), &diff)
                    .await
                    .map_err(ToolExecutionError::provider)?;

                let mut out = String::from("patch applied");
                if let Some(msg) = message {
                    // `git apply` only touches the working tree; stage
                    // everything so the commit captures the patch (this also
                    // covers new files and initial commits).
                    git_run(tree.root(), &["add", "-A"])
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
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'path'")
                    })?
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

    /// Make a real git repo out of `fx` with a configured identity and one
    /// initial commit, so `git status --porcelain` reflects a clean tree.
    async fn init_committed_repo(fx: &Fixture) {
        git_run(&fx.dir, &["init", "-q"]).await.expect("git init");
        git_run(&fx.dir, &["config", "user.email", "test@hanihi.local"])
            .await
            .expect("config email");
        git_run(&fx.dir, &["config", "user.name", "hanihi-test"])
            .await
            .expect("config name");
        git_run(&fx.dir, &["add", "-A"]).await.expect("git add");
        git_run(&fx.dir, &["commit", "-q", "-m", "init"])
            .await
            .expect("git commit");
    }

    /// A minimal single-file diff (with `diff --git` header) against
    /// `src/main.rs`.
    fn header_diff() -> &'static str {
        "diff --git a/src/main.rs b/src/main.rs\nindex 8b13789..0000000 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1,2 @@\n fn main() {}\n+// patched\n"
    }

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

    #[tokio::test]
    async fn apply_patch_applies_and_commits() {
        let fx = Fixture::new();
        init_committed_repo(&fx).await;
        let tool = builtin_apply_patch(fx.tree());
        let out = tool
            .execute(serde_json::json!({
                "diff": header_diff(),
                "message": "test patch"
            }))
            .await
            .unwrap_or_else(|e| panic!("apply failed: {e}"));
        let rendered = out.render();
        assert!(rendered.contains("patch applied"), "got: {rendered}");
        assert!(rendered.contains("committed"), "got: {rendered}");
        let main = std::fs::read_to_string(fx.dir.join("src/main.rs")).unwrap();
        assert!(main.contains("// patched"));

        // Committed: working tree must be clean.
        let (stdout, _) = git_run(&fx.dir, &["status", "--porcelain"]).await.unwrap();
        assert_eq!(stdout.trim(), "", "expected clean tree, got: {stdout}");
    }

    #[tokio::test]
    async fn apply_patch_works_on_dirty_tree() {
        let fx = Fixture::new();
        init_committed_repo(&fx).await;

        // The common self-improvement case: an unrelated uncommitted edit
        // already sits in the target file. The patch must still apply on top
        // of it — no "clean tree first" refusal.
        std::fs::write(fx.dir.join("src/main.rs"), "fn main() {}\n// local\n").unwrap();

        let tool = builtin_apply_patch(fx.tree());
        tool.execute(serde_json::json!({ "diff": header_diff() }))
            .await
            .unwrap_or_else(|e| panic!("apply over dirty tree must succeed: {e}"));

        let main = std::fs::read_to_string(fx.dir.join("src/main.rs")).unwrap();
        assert!(main.contains("// patched"), "got: {main}");
        // The pre-existing uncommitted edit is preserved alongside the patch.
        assert!(main.contains("// local"), "got: {main}");
    }

    #[tokio::test]
    async fn apply_patch_applies_to_new_file_created_earlier() {
        let fx = Fixture::new();
        init_committed_repo(&fx).await;

        // A new file the agent created in a prior turn (untracked). A patch
        // that adds it must not be refused just because it is untracked.
        std::fs::write(fx.dir.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
        let add = "diff --git a/src/extra.rs b/src/extra.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/extra.rs\n@@ -0,0 +1 @@\n+pub fn extra() {}\n";

        let tool = builtin_apply_patch(fx.tree());
        tool.execute(serde_json::json!({ "diff": add }))
            .await
            .unwrap_or_else(|e| panic!("apply must succeed on untracked target: {e}"));
    }

    #[tokio::test]
    async fn apply_patch_rejects_bad_diff_and_protected_paths() {
        let fx = Fixture::new();
        let tool = builtin_apply_patch(fx.tree());

        let err = tool
            .execute(serde_json::json!({ "diff": "not a diff at all" }))
            .await
            .expect_err("bad diff must fail");
        assert!(err.to_string().contains("failed"), "got: {err}");

        let err = tool
            .execute(serde_json::json!({ "diff": "--- a/.ignore\n+++ b/.ignore\n@@ -1 +1 @@\n-x\n+y\n" }))
            .await
            .expect_err("protected diff must fail");
        assert!(err.to_string().contains("protected"), "got: {err}");
    }

    #[test]
    fn protected_path_detection() {
        assert!(is_protected(".ignore"));
        assert!(is_protected(".gitignore"));
        assert!(is_protected(".git/config"));
        assert!(is_protected(".git"));
        assert!(!is_protected("src/main.rs"));
        assert!(!is_protected("docs/.ignore.md"));
    }

    #[test]
    fn diff_protected_scan() {
        assert_eq!(
            diff_touches_protected("--- a/src/main.rs\n+++ b/src/main.rs\n"),
            None
        );
        assert_eq!(
            diff_touches_protected("+++ b/.ignore\n"),
            Some(".ignore".to_string())
        );
        // Path with tab (git diff metadata) is still caught.
        assert_eq!(
            diff_touches_protected("+++ b/.git/config\t2026-01-01\n"),
            Some(".git/config".to_string())
        );
    }

    #[tokio::test]
    async fn diff_already_applied_detects_existing_change() {
        let fx = Fixture::new();
        init_committed_repo(&fx).await;

        // Nothing applied yet: the change is not present.
        assert!(!diff_already_applied(&fx.dir, header_diff()).await);

        // Apply it, then the change is present.
        git_apply_diff(&fx.dir, header_diff()).await.expect("apply");
        assert!(diff_already_applied(&fx.dir, header_diff()).await);
    }
}
