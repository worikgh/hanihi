//! Write-path tools: `apply_patch` and `write_file`.
//!
//! These are registered **only** when the CLI is invoked with `--write`.
//! They are scoped to the enclosing git repository via [`SourceTree`]:
//! escapes, git-ignored paths, `.ignore`, and anything under `.git/` are
//! refused. Changes land as local git commits — never pushed. Git is the
//! undo button and the audit trail.

use std::path::{Path, PathBuf};
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

/// A single body line of a unified-diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyLine {
    Context(String),
    Del(String),
    Add(String),
}

impl BodyLine {
    fn text(&self) -> &str {
        match self {
            BodyLine::Context(s) | BodyLine::Del(s) | BodyLine::Add(s) => s,
        }
    }

    fn is_del(&self) -> bool {
        matches!(self, BodyLine::Del(_))
    }

    fn is_add(&self) -> bool {
        matches!(self, BodyLine::Add(_))
    }
}

/// One hunk of a file patch. The `@@` counts are parsed but deliberately
/// ignored: line counts are recomputed from the body (the `--recount`
/// behaviour of `git apply`), so hand-composed hunks with slightly off
/// numbers still apply.
#[derive(Debug)]
struct Hunk {
    lines: Vec<BodyLine>,
}

/// A single file touched by a unified diff.
#[derive(Debug)]
struct FilePatch {
    old_path: Option<String>,
    new_path: Option<String>,
    is_new: bool,
    is_delete: bool,
    hunks: Vec<Hunk>,
}

impl FilePatch {
    /// Repo-relative path this patch writes to.
    fn target_rel(&self) -> Result<String, String> {
        match (&self.old_path, &self.new_path) {
            (_, Some(new)) => Ok(new.clone()),
            (Some(old), None) => Ok(old.clone()),
            (None, None) => Err("file patch has neither an old nor a new path".into()),
        }
    }

    /// The same patch with additions and deletions swapped. Applying the
    /// reverse of an already-applied patch succeeds exactly when the change
    /// is present in the working tree.
    #[cfg(test)]
    fn reversed(&self) -> FilePatch {
        FilePatch {
            old_path: self.new_path.clone(),
            new_path: self.old_path.clone(),
            is_new: self.is_delete,
            is_delete: self.is_new,
            hunks: self
                .hunks
                .iter()
                .map(|h| Hunk {
                    lines: h
                        .lines
                        .iter()
                        .map(|l| match l {
                            BodyLine::Context(s) => BodyLine::Context(s.clone()),
                            BodyLine::Del(s) => BodyLine::Add(s.clone()),
                            BodyLine::Add(s) => BodyLine::Del(s.clone()),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Result of applying one file patch, before anything is written.
struct AppliedFile {
    abs: PathBuf,
    /// `None` means the file is to be deleted.
    content: Option<String>,
}

fn is_file_marker(a: &str, b: &str) -> bool {
    (a.starts_with("--- /dev/null") || a.starts_with("--- a/")) && b.starts_with("+++ ")
}

/// Split `diff` into per-file blocks. Each block starts at a `--- ` marker
/// line whose following line is `+++ `, and runs to just before the next
/// such pair (or the end of the input).
fn file_blocks(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        if is_file_marker(lines[i], lines[i + 1]) {
            let start = i;
            let mut j = i + 2;
            while j < lines.len() {
                if j + 1 < lines.len() && is_file_marker(lines[j], lines[j + 1]) {
                    break;
                }
                j += 1;
            }
            blocks.push((start, j));
            i = j;
        } else {
            i += 1;
        }
    }
    blocks
}

/// Parse the path from a `--- ` or `+++ ` marker line. Returns `None` for
/// `/dev/null` (the "file did not exist" side) and strips git's standard
/// `a/`/`b/` prefixes plus any trailing tab metadata.
fn parse_side_path(marker: &str) -> Option<String> {
    let rest = marker.split('\t').next().unwrap_or(marker).trim();
    if rest == "/dev/null" {
        return None;
    }
    let rel = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    Some(rel.to_string())
}

/// Parse a unified diff into per-file patches. Header lines that carry no
/// content (`index`, `new file mode`, `old/new mode`, `similarity index`,
/// ...) are skipped; binary and rename/copy patches are refused.
fn parse_diff(diff: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = diff.lines().collect();
    let blocks = file_blocks(&lines);
    if blocks.is_empty() {
        return Err("no `---`/`+++` file markers found".into());
    }

    let mut patches = Vec::with_capacity(blocks.len());
    for (start, end) in blocks {
        let old_path = parse_side_path(&lines[start]["--- ".len()..]);
        let new_path = parse_side_path(&lines[start + 1]["+++ ".len()..]);
        let mut patch = FilePatch {
            is_new: old_path.is_none(),
            is_delete: new_path.is_none(),
            old_path,
            new_path,
            hunks: Vec::new(),
        };

        let mut i = start + 2;
        while i < end {
            let line = lines[i];
            if let Some(rest) = line.strip_prefix("@@") {
                if !rest.contains("@@") {
                    return Err(format!("malformed hunk header: @@{rest}"));
                }
                i += 1;
                let mut body = Vec::new();
                while i < end && !lines[i].starts_with("@@") {
                    let l = lines[i];
                    if l.starts_with('\\') {
                        // "\ No newline at end of file" — line-level
                        // application does not need it; skip.
                        i += 1;
                        continue;
                    }
                    body.push(parse_body_line(l)?);
                    i += 1;
                }
                patch.hunks.push(Hunk { lines: body });
                continue;
            }
            if line.starts_with("Binary files ") || line == "GIT binary patch" {
                return Err("binary patches are not supported".into());
            }
            if line.starts_with("rename from ") || line.starts_with("rename to ") {
                return Err("rename/copy patches are not supported".into());
            }
            i += 1;
        }
        patches.push(patch);
    }
    Ok(patches)
}

/// Classify one hunk body line by its leading marker character.
fn parse_body_line(line: &str) -> Result<BodyLine, String> {
    if let Some(rest) = line.strip_prefix(' ') {
        Ok(BodyLine::Context(rest.to_string()))
    } else if let Some(rest) = line.strip_prefix('+') {
        Ok(BodyLine::Add(rest.to_string()))
    } else if let Some(rest) = line.strip_prefix('-') {
        Ok(BodyLine::Del(rest.to_string()))
    } else {
        Err(format!("unrecognized hunk line: {line}"))
    }
}

/// Split content into lines for hunk application. The empty element produced
/// by `str::split('\n')` when the content ends in a newline is dropped, so
/// `["a", "b"]` is two lines regardless of the final newline.
fn split_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// First index `i` where `lines[i..i+needle.len()]` equals `needle`.
fn find_subslice(lines: &[String], needle: &[&str]) -> Option<usize> {
    if needle.len() > lines.len() {
        return None;
    }
    (0..=lines.len() - needle.len()).find(|&i| {
        needle
            .iter()
            .enumerate()
            .all(|(k, want)| lines[i + k] == *want)
    })
}

/// Apply one file patch against the current working-tree contents.
///
/// Reads the file (if any), matches each hunk's old-side lines (`context` +
/// `-`) against a contiguous block, and replaces it with the new-side lines
/// (`context` + `+`). This is the reliable path that `git apply --3way` was
/// supposed to provide: the patch merges onto whatever is in the working tree
/// now, regardless of the index or object database, and a single context line
/// is enough to anchor an insertion.
fn apply_file_patch(tree: &SourceTree, patch: &FilePatch) -> Result<AppliedFile, String> {
    let rel = patch.target_rel()?;
    if is_protected(&rel) {
        return Err(format!("patch touches protected path: {rel}"));
    }
    let abs = tree
        .resolve_for_write(Path::new(&rel))
        .map_err(|e| e.to_string())?;
    if tree.is_ignored(&abs) {
        return Err(format!("path is git-ignored: {}", abs.display()));
    }

    let existing = abs.exists();
    if patch.is_new {
        if existing {
            return Err(format!(
                "refusing to create {}: file already exists",
                abs.display()
            ));
        }
        let mut lines: Vec<String> = Vec::new();
        for hunk in &patch.hunks {
            let new_side: Vec<&str> = hunk
                .lines
                .iter()
                .filter(|l| !l.is_del())
                .map(BodyLine::text)
                .collect();
            lines.extend(new_side.into_iter().map(str::to_string));
        }
        let content = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        return Ok(AppliedFile {
            abs,
            content: Some(content),
        });
    }

    if !existing {
        return Err(format!(
            "cannot apply patch: {} does not exist",
            abs.display()
        ));
    }
    let content =
        std::fs::read_to_string(&abs).map_err(|e| format!("reading {}: {e}", abs.display()))?;
    let had_trailing_newline = content.ends_with('\n');
    let mut lines = split_lines(&content);

    for hunk in &patch.hunks {
        let old_side: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| !l.is_add())
            .map(BodyLine::text)
            .collect();
        let new_side: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| !l.is_del())
            .map(BodyLine::text)
            .collect();

        if old_side.is_empty() {
            // Pure insertion with no context (e.g. a new file). Only
            // meaningful when there is no content to anchor against.
            if lines.is_empty() {
                lines.extend(new_side.into_iter().map(str::to_string));
                continue;
            }
            return Err(format!(
                "cannot apply hunk to {}: hunk has no context but the file is not empty",
                abs.display()
            ));
        }

        let n = old_side.len();
        let Some(pos) = find_subslice(&lines, &old_side) else {
            return Err(format!(
                "cannot apply hunk to {}: context does not match",
                abs.display()
            ));
        };
        let mut applied = Vec::with_capacity(lines.len() - n + new_side.len());
        applied.extend_from_slice(&lines[..pos]);
        applied.extend(new_side.into_iter().map(str::to_string));
        applied.extend_from_slice(&lines[pos + n..]);
        lines = applied;
    }

    if patch.is_delete {
        if !lines.is_empty() {
            return Err(format!(
                "deletion patch for {} leaves content behind",
                abs.display()
            ));
        }
        return Ok(AppliedFile { abs, content: None });
    }

    let mut out = lines.join("\n");
    if had_trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(AppliedFile {
        abs,
        content: Some(out),
    })
}

/// Apply a unified diff to the working tree in pure Rust.
///
/// Why not `git apply`: the `--3way` merge depends on index/HEAD blobs that
/// are frequently unavailable here (hand-composed patches carry synthetic
/// `index` hashes), and the direct fallback refuses a hunk whose insertion
/// point is not anchored by trailing context — the exact "patch does not
/// apply" failure observed when patching a file that already has uncommitted
/// edits. Applying hunks directly against the current working-tree contents
/// handles the dirty-tree case naturally and never consults the object
/// database.
///
/// Matching is all-or-nothing: every hunk of every file is first matched
/// against in-memory copies; only when the whole diff applies are any files
/// written.
fn apply_unified_diff(tree: &SourceTree, diff: &str) -> Result<(), String> {
    let patches = parse_diff(diff).map_err(|e| format!("failed to parse patch: {e}"))?;
    let mut applied = Vec::with_capacity(patches.len());
    for patch in &patches {
        applied.push(apply_file_patch(tree, patch)?);
    }
    for file in &applied {
        match &file.content {
            Some(content) => {
                if let Some(parent) = file.abs.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("creating parent dirs: {e}"))?;
                }
                std::fs::write(&file.abs, content)
                    .map_err(|e| format!("writing {}: {e}", file.abs.display()))?;
            }
            None => {
                std::fs::remove_file(&file.abs)
                    .map_err(|e| format!("removing {}: {e}", file.abs.display()))?;
            }
        }
    }
    Ok(())
}

/// Is the patch's change already present in the working tree? True exactly
/// when applying the reversed patch succeeds — the "change already exists,
/// stop re-patching it" signal, computed in pure Rust.
#[cfg(test)]
fn diff_already_applied(tree: &SourceTree, diff: &str) -> bool {
    let Ok(patches) = parse_diff(diff) else {
        return false;
    };
    patches
        .iter()
        .all(|p| apply_file_patch(tree, &p.reversed()).is_ok())
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
        "Apply a unified diff (git diff format) to the repository working tree. The diff is \
         parsed and applied directly against the current working-tree contents, so it works on \
         top of other uncommitted changes — no clean tree or `git apply` needed. Application is \
         all-or-nothing: if any hunk fails to match, nothing is written. If `message` is given, \
         the change is committed with that message. Diffs that touch `.ignore` or `.git*` paths \
         are refused. Changes are local commits only — never pushed.",
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
                    .ok_or_else(|| ToolExecutionError::invalid_args("missing string field 'diff'"))?
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

                apply_unified_diff(&tree, &diff)
                    .map_err(|e| ToolExecutionError::provider(format!("apply failed: {e}")))?;

                let mut out = String::from("patch applied");
                if let Some(msg) = message {
                    // Stage everything so the commit captures the patch (this
                    // also covers new files and initial commits).
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
    /// `src/main.rs`. The synthetic `index` hashes are ignored by the
    /// pure-Rust applier.
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

        // The case plain `git apply` refused: an uncommitted edit already
        // sits in the target file and the hunk has no trailing context. The
        // applier must insert the patch on top of the local edit instead of
        // refusing.
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
    async fn apply_patch_adds_new_file_on_clean_tree() {
        let fx = Fixture::new();
        init_committed_repo(&fx).await;

        // Adding a brand-new file (not already present) must work.
        let add = "diff --git a/src/extra.rs b/src/extra.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/extra.rs\n@@ -0,0 +1 @@\n+pub fn extra() {}\n";

        let tool = builtin_apply_patch(fx.tree());
        tool.execute(serde_json::json!({ "diff": add }))
            .await
            .unwrap_or_else(|e| panic!("apply must succeed adding a new file: {e}"));

        let extra = std::fs::read_to_string(fx.dir.join("src/extra.rs")).unwrap();
        assert_eq!(extra, "pub fn extra() {}\n");
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
        let tree = fx.tree();

        // Nothing applied yet: the change is not present.
        assert!(!diff_already_applied(&tree, header_diff()));

        // Apply it, then the change is present.
        apply_unified_diff(&tree, header_diff()).expect("apply");
        assert!(diff_already_applied(&tree, header_diff()));
    }
}
