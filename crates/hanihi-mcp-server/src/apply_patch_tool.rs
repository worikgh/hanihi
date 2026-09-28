//! `apply_patch` tool for Hānihi.
//!
//! Applies unified diffs to workspace files after verifying each file's
//! current contents against a caller-supplied `base_token` (a SHA-256
//! digest issued by `read_file`). Every file is validated before any write
//! happens, so a failed patch never leaves a partial edit behind, and a
//! stale token never silently overwrites a concurrent change.

use crate::workspace_fs;
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Number of context lines shown around each changed region in the returned
/// diff.
const DIFF_CONTEXT: usize = 3;
/// Maximum LCS table size before falling back to a whole-file replacement
/// diff, keeping memory bounded for very large files.
const LCS_CELL_LIMIT: usize = 4_000_000;

/// SHA-256 of empty content; the documented `base_token` for a new file.
/// Keep in sync with the same literal in `json()`'s description.
const EMPTY_CONTENT_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Well-known null sentinels that are never a real file version. These are
/// exactly the values a caller fabricates when it cannot observe the hash.
const NULL_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const ALL_F_HASH: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

/// True when `hash` is a placeholder rather than a version that could have
/// come from a read.
fn is_sentinel_version(hash: &str) -> bool {
    hash == NULL_HASH || hash == ALL_F_HASH
}

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "apply_patch",
    "description": "Applies unified diffs or whole-file replacements to workspace files after verifying each file's current SHA-256 hash. Returns the resulting diff and refuses paths outside the workspace or protected files.",
    "inputSchema": {
        "type": "object",
        "properties": {
        "file": {
            "type": "string",
            "description": "Path to edit, relative to the workspace root. Use together with `patch` or `content` and `base_token` for a single-file edit."
        },
        "patch": {
            "type": "string",
            "description": "Unified diff to apply. Use together with `file` and `base_token`. Each hunk header must carry ranges, e.g. \"@@ -1,1 +1,1 @@\". For whole-file replacement, use `content` instead."
        },
        "content": {
            "type": "string",
            "description": "Replace the entire file with this UTF-8 text. Use together with `file` and `base_token`. Mutually exclusive with `patch`; the returned diff is computed by the server. An empty string truncates the file."
        },
        "base_token": {
            "type": "string",
            "description": "SHA-256 hex digest (64 hex characters) of the file's current contents. For an existing file, copy the exact `token` returned by `read_file` for this path. For a new file, use the SHA-256 of empty content (e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855). Never substitute a placeholder such as all zeros."
        },
        "files": {
            "type": "array",
            "items": {
            "type": "object",
            "properties": {
                "path": {
                "type": "string",
                "description": "Path to edit, relative to the workspace root."
                },
                "base_token": {
                "type": "string",
                "description": "SHA-256 hex digest (64 hex characters) of the file's current contents. Copy the `token` returned by `read_file` for this path."
                },
                "patch": {
                "type": "string",
                "description": "Unified diff to apply to this file. Each hunk header must carry ranges, e.g. \"@@ -1,1 +1,1 @@\". Use `content` instead for whole-file replacement."
                },
                "content": {
                "type": "string",
                "description": "Replace the entire file with this UTF-8 text. Mutually exclusive with `patch`."
                }
            },
            "required": ["path", "base_token"],
            "additionalProperties": false
            },
            "description": "Edits to apply together. Use either `files` or the single-file `file`/`patch`/`content`/`base_token` form."
        }
        },
        "required": [],
        "additionalProperties": false
    }
    })
}

/// Implements the tool. `params` carries the MCP tool call; its `arguments`
/// object holds the edit request.
pub(crate) fn exec(params: &Value, id: Value) -> Value {
    eprintln!("{}:{}: exec", file!(), line!());
    let arguments = params.get("arguments").unwrap_or(params);
    match run(arguments) {
        Ok(text) => success(id, text),
        Err(error) => failure(id, error.code, error.message),
    }
}

#[derive(Debug)]
struct ToolError {
    code: i64,
    message: String,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError {
        code: -32602,
        message: message.into(),
    }
}

fn internal(message: impl Into<String>) -> ToolError {
    ToolError {
        code: -32603,
        message: message.into(),
    }
}

fn success(id: Value, text: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "result": {
        "content": [{ "type": "text", "text": text }],
        "isError": false
    }
    })
}

fn failure(id: Value, code: i64, message: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "error": { "code": code, "message": message }
    })
}

/// One requested edit: either a unified diff to apply or a whole-file
/// replacement. The two forms are mutually exclusive.
#[derive(Debug)]
enum EditOp {
    Patch { patch: String },
    Replace { content: String },
}

#[derive(Debug)]
struct Edit {
    path: String,
    base_token: String,
    op: EditOp,
}

fn run(arguments: &Value) -> Result<String, ToolError> {
    let edits = parse_edits(arguments)?;
    if edits.is_empty() {
        return Err(invalid("at least one file edit is required"));
    }

    let report = apply_edits(&edits)?;
    serde_json::to_string_pretty(&report)
        .map_err(|error| internal(format!("failed to serialize results: {error}")))
}

fn parse_edits(arguments: &Value) -> Result<Vec<Edit>, ToolError> {
    let single_file = arguments.get("file").is_some()
        || arguments.get("patch").is_some()
        || arguments.get("content").is_some()
        || arguments.get("base_token").is_some();
    let multi_file = arguments.get("files").is_some();

    if single_file && multi_file {
        return Err(invalid(
            "use either the single-file `file`/`patch`/`content`/`base_token` form or `files`, not both",
        ));
    }

    if multi_file {
        let array = arguments
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("argument `files` must be an array of edit objects"))?;

        let mut edits = Vec::with_capacity(array.len());
        for item in array {
            edits.push(edit_from_arguments(item, "path")?);
        }
        Ok(edits)
    } else if single_file {
        Ok(vec![edit_from_arguments(arguments, "file")?])
    } else {
        Err(invalid(
            "provide either `files` or the single-file `file`/`patch`/`content`/`base_token` form",
        ))
    }
}

fn edit_from_arguments(arguments: &Value, path_name: &str) -> Result<Edit, ToolError> {
    let path = required_string(arguments, path_name)?;
    let base_token = required_string(arguments, "base_token")?;
    let op = edit_op(arguments)?;
    Ok(Edit {
        path: path.to_string(),
        base_token: base_token.to_string(),
        op,
    })
}

/// Parses the edit operation from a single-file or `files[]` item object.
/// `patch` must be non-empty; `content` may be empty (truncate the file).
fn edit_op(arguments: &Value) -> Result<EditOp, ToolError> {
    let has_patch = arguments.get("patch").is_some();
    let has_content = arguments.get("content").is_some();

    match (has_patch, has_content) {
        (true, true) => Err(invalid("use either `patch` or `content`, not both")),
        (true, false) => Ok(EditOp::Patch {
            patch: required_string(arguments, "patch")?.to_string(),
        }),
        (false, true) => {
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("argument `content` must be a string"))?;
            Ok(EditOp::Replace {
                content: content.to_string(),
            })
        }
        (false, false) => Err(invalid("each edit needs either `patch` or `content`")),
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("missing required string argument: {name}")))
}

fn apply_edits(edits: &[Edit]) -> Result<Value, ToolError> {
    let root = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
    apply_edits_in(&root, edits)
}

fn apply_edits_in(root: &Path, edits: &[Edit]) -> Result<Value, ToolError> {
    struct Prepared {
        path: PathBuf,
        display_path: String,
        original: String,
        original_bytes: Vec<u8>,
        lines: Vec<String>,
        ends_with_newline: bool,
        patch: Option<ParsedPatch>,
        replacement: Option<String>,
    }

    // Phase 1: resolve, read, hash-verify, and parse every edit. Nothing is
    // written yet, so one bad edit cannot leave a partial change behind.
    let mut prepared = Vec::with_capacity(edits.len());
    for edit in edits {
        let path = resolve_within_root(root, &edit.path).map_err(invalid)?;

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) if error.kind() == ErrorKind::NotADirectory => {
                return Err(invalid(format!(
                    "argument `file` is not a valid file path: {} (a parent component is not a directory)",
                    edit.path
                )));
            }
            Err(error) => return Err(internal(format!("cannot read {}: {error}", edit.path))),
        };

        let expected = workspace_fs::normalize_hash(&edit.base_token).ok_or_else(|| {
            invalid(format!(
                "argument `base_token` must be a 64-character hex digest, got {:?}",
                edit.base_token
            ))
        })?;
        let actual = workspace_fs::sha256_hex(&bytes);

        if is_sentinel_version(&expected) {
            return Err(invalid(format!(
                "argument `base_token` for {} is a placeholder ({expected}), not a file version. Copy the `token` returned by `read_file` for this path (current: {actual}), or use the empty-content hash ({EMPTY_CONTENT_HASH}) for a new file.",
                edit.path
            )));
        }

        if expected == EMPTY_CONTENT_HASH && !bytes.is_empty() {
            return Err(invalid(format!(
                "argument `base_token` for {} is the empty-content hash, but the file has content ({} bytes). For an existing file, copy the `token` returned by `read_file` (current: {actual}); the empty-content hash is only valid for a new or empty file.",
                edit.path,
                bytes.len()
            )));
        }

        if actual != expected {
            return Err(internal(format!(
                "base_token mismatch for {}: expected {expected}, computed {actual}; refusing to overwrite concurrent changes",
                edit.path
            )));
        }

        let original = String::from_utf8(bytes.clone())
            .map_err(|_| internal(format!("file is not valid UTF-8: {}", edit.path)))?;

        let (replacement, patch) = match &edit.op {
            EditOp::Replace { content } => (Some(content.clone()), None),
            EditOp::Patch { patch } => {
                (None, Some(parse_patch(patch, &edit.path).map_err(invalid)?))
            }
        };

        let (lines, ends_with_newline) = split_lines(&original);

        prepared.push(Prepared {
            path,
            display_path: edit.path.clone(),
            original,
            original_bytes: bytes,
            lines,
            ends_with_newline,
            patch,
            replacement,
        });
    }

    struct Applied {
        path: PathBuf,
        display_path: String,
        new_content: String,
        original_bytes: Vec<u8>,
        diff: String,
    }

    // Phase 2: apply every edit in memory. Any failure aborts before writes.
    let mut applied = Vec::with_capacity(prepared.len());
    for item in prepared {
        let new_content = match (&item.replacement, &item.patch) {
            (Some(content), _) => content.clone(),
            (None, Some(patch)) => {
                let new_lines =
                    apply_hunks(&item.lines, patch, &item.display_path).map_err(internal)?;
                join_lines(&new_lines, item.ends_with_newline)
            }
            (None, None) => {
                return Err(internal(format!(
                    "internal error: edit for {} has neither a patch nor replacement content",
                    item.display_path
                )));
            }
        };
        let diff = unified_diff(&item.original, &new_content, &item.display_path);
        applied.push(Applied {
            path: item.path,
            display_path: item.display_path,
            new_content,
            original_bytes: item.original_bytes,
            diff,
        });
    }

    // Phase 3: commit all writes. Validation above is all-or-nothing; if a
    // write fails partway, earlier writes are rolled back to their originals.
    for (index, entry) in applied.iter().enumerate() {
        if let Err(error) = fs::write(&entry.path, entry.new_content.as_bytes()) {
            for previous in &applied[..index] {
                let _ = fs::write(&previous.path, &previous.original_bytes);
            }
            return Err(internal(format!(
                "cannot write {}: {error}; rolled back {index} earlier write(s)",
                entry.display_path
            )));
        }
    }

    Ok(json!({
    "files_changed": applied.len(),
    "files": applied
        .iter()
        .map(|entry| json!({
        "path": entry.display_path,
        "applied": true,
        "diff": entry.diff,
        }))
        .collect::<Vec<_>>(),
    }))
}

/// Resolves a workspace-relative path to an absolute path, refusing paths
/// that escape the workspace (absolutes, `..` traversal, or symlinks).
fn resolve_within_root(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let (candidate, normalized) = normalize_relative(root, relative)?;
    if is_refused_path(&normalized) {
        return Err(format!("refusing to edit protected path: {relative}"));
    }

    // Guard against symlinks pointing outside the workspace: resolve the
    // nearest existing ancestor and verify it stays under the canonical root.
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;
    // The anchor must be a directory because this node is used solely as a
    // canonicalization anchor. The pre-existing degenerate case where
    // `candidate` exists and *is* the file no longer arises: `candidate` is
    // never a directory, so for an existing file target the walk ascends one
    // level to the parent directory, leaving a non-empty tail, and `resolved`
    // names the file.
    let mut existing = candidate.as_path();
    while !existing.is_dir() {
        existing = existing
            .parent()
            .ok_or_else(|| format!("path has no existing ancestor: {relative}"))?;
    }
    let canonical_existing = existing
        .canonicalize()
        .map_err(|error| format!("cannot resolve path ancestor: {error}"))?;
    let tail = candidate
        .strip_prefix(existing)
        .map_err(|_| format!("path resolution failed: {relative}"))?;
    let resolved = canonical_existing.join(tail);

    if !resolved.starts_with(&canonical_root) {
        return Err(format!("path escapes the workspace: {relative}"));
    }
    Ok(resolved)
}

/// Lexically normalizes a workspace-relative path, returning the absolute
/// candidate and the normalized relative path. Pure, so it can be unit tested
/// without touching the filesystem.
fn normalize_relative(root: &Path, relative: &str) -> Result<(PathBuf, PathBuf), String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        return Err(format!(
            "path must be relative to the workspace root: {relative}"
        ));
    }
    if relative.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }

    let mut candidate = root.to_path_buf();
    let mut normalized = PathBuf::new();
    for component in relative_path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                candidate.pop();
                normalized.pop();
                if !candidate.starts_with(root) {
                    return Err(format!("path escapes the workspace: {relative}"));
                }
            }
            Component::Normal(part) => {
                candidate.push(part);
                normalized.push(part);
            }
            _ => return Err(format!("unsupported path component in: {relative}")),
        }
    }

    Ok((candidate, normalized))
}

/// Paths that must never be edited, matching the repository protection rules.
fn is_refused_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == ".ignore" || name.starts_with(".git")
    })
}

/// Splits file contents into lines without their newline characters, and
/// reports whether the contents ended with a newline.
fn split_lines(content: &str) -> (Vec<String>, bool) {
    if content.is_empty() {
        return (Vec::new(), false);
    }
    let ends_with_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    if ends_with_newline {
        lines.pop();
    }
    (lines, ends_with_newline)
}

/// Rejoins lines, restoring the single trailing newline when the original had
/// one. An empty line list always produces empty contents.
fn join_lines(lines: &[String], ends_with_newline: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut content = lines.join("\n");
    if ends_with_newline {
        content.push('\n');
    }
    content
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Remove,
    Add,
}

#[derive(Debug)]
struct DiffLine {
    kind: LineKind,
    text: String,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<DiffLine>,
}

#[derive(Debug)]
struct ParsedPatch {
    hunks: Vec<Hunk>,
}

/// Parses a unified diff into hunks. Lines before the first `@@` (file
/// headers such as `---`, `+++`, and `diff --git`) are ignored.
fn parse_patch(patch: &str, path: &str) -> Result<ParsedPatch, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            let (old_start, old_count, new_start, new_count) =
                parse_hunk_header(rest).ok_or_else(|| invalid_hunk_header(path, raw))?;
            current = Some(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            continue;
        };

        // `\ No newline at end of file` is a hint about the final line. The
        // tool preserves the file-level trailing newline instead, so skip it.
        if raw == "\\ No newline at end of file" {
            continue;
        }

        let (kind, text) = if let Some(rest) = raw.strip_prefix(' ') {
            (LineKind::Context, rest)
        } else if let Some(rest) = raw.strip_prefix('-') {
            (LineKind::Remove, rest)
        } else if let Some(rest) = raw.strip_prefix('+') {
            (LineKind::Add, rest)
        } else {
            continue;
        };
        hunk.lines.push(DiffLine {
            kind,
            text: text.to_string(),
        });
    }

    if let Some(hunk) = current {
        hunks.push(hunk);
    }

    if hunks.is_empty() {
        return Err(format!("patch for {path} contains no hunks"));
    }
    Ok(ParsedPatch { hunks })
}

/// Renders a diagnostic for a malformed hunk header, teaching the caller the
/// required range syntax and a minimal example.
fn invalid_hunk_header(path: &str, raw: &str) -> String {
    format!(
        "invalid hunk header in patch for {path}: `{raw}` — expected `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@`, for example `@@ -1,1 +1,1 @@`"
    )
}

/// Parses the `-old,count +new,count` part of a hunk header (without the
/// leading `@@`).
fn parse_hunk_header(rest: &str) -> Option<(usize, usize, usize, usize)> {
    let mut parts = rest.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let (old_start, old_count) = parse_hunk_range(old)?;
    let (new_start, new_count) = parse_hunk_range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_hunk_range(part: &str) -> Option<(usize, usize)> {
    let part = part.strip_prefix('-').or_else(|| part.strip_prefix('+'))?;
    match part.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => {
            let start: usize = part.parse().ok()?;
            if start == 0 {
                Some((0, 0))
            } else {
                Some((start, 1))
            }
        }
    }
}

/// Applies all hunks in order, locating each hunk by content match around its
/// expected position. A running offset accounts for earlier hunks changing
/// line numbers.
fn apply_hunks(lines: &[String], patch: &ParsedPatch, path: &str) -> Result<Vec<String>, String> {
    let mut current = lines.to_vec();
    let mut offset_shift: isize = 0;

    for hunk in &patch.hunks {
        let anchor = (hunk.old_start as isize - 1 + offset_shift).max(0) as usize;
        let position = find_hunk_position(&current, hunk, anchor).ok_or_else(|| {
            format!(
                "patch for {path} does not apply: hunk @@ -{},{} +{},{} @@ does not match the current file contents",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            )
        })?;

        current = apply_one_hunk(&current, position, hunk);

        let old_count = hunk
            .lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Add))
            .count() as isize;
        let new_count = hunk
            .lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Remove))
            .count() as isize;
        offset_shift += new_count - old_count;
    }

    Ok(current)
}

/// Returns the closest offset at which the hunk's context and removed lines
/// match, searching the whole file but preferring `anchor`.
fn find_hunk_position(lines: &[String], hunk: &Hunk, anchor: usize) -> Option<usize> {
    let anchor = anchor.min(lines.len());
    let mut best: Option<(usize, usize)> = None;

    for offset in 0..=lines.len() {
        if hunk_matches(lines, offset, hunk) {
            let distance = offset.abs_diff(anchor);
            let better = match best {
                Some((best_distance, _)) => distance < best_distance,
                None => true,
            };
            if better {
                best = Some((distance, offset));
            }
        }
    }

    best.map(|(_, offset)| offset)
}

fn hunk_matches(lines: &[String], offset: usize, hunk: &Hunk) -> bool {
    let mut index = offset;
    for line in &hunk.lines {
        match line.kind {
            LineKind::Context | LineKind::Remove => {
                if index >= lines.len() || lines[index] != line.text {
                    return false;
                }
                index += 1;
            }
            LineKind::Add => {}
        }
    }
    true
}

fn apply_one_hunk(lines: &[String], offset: usize, hunk: &Hunk) -> Vec<String> {
    let mut result = Vec::with_capacity(lines.len() + hunk.lines.len());
    result.extend_from_slice(&lines[..offset]);

    let mut index = offset;
    for line in &hunk.lines {
        match line.kind {
            LineKind::Context => {
                result.push(line.text.clone());
                index += 1;
            }
            LineKind::Remove => {
                index += 1;
            }
            LineKind::Add => {
                result.push(line.text.clone());
            }
        }
    }

    result.extend_from_slice(&lines[index..]);
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Remove,
    Add,
}

/// Produces a unified diff between `old` and `new` with `DIFF_CONTEXT` lines
/// of context around each changed region.
fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let (old_lines, _) = split_lines(old);
    let (new_lines, _) = split_lines(new);
    let ops = diff_ops(&old_lines, &new_lines);

    struct Step<'a> {
        op: Op,
        text: &'a str,
        old_index: Option<usize>,
        new_index: Option<usize>,
    }

    let mut steps = Vec::with_capacity(ops.len());
    let (mut old_index, mut new_index) = (0, 0);
    for op in ops {
        match op {
            Op::Keep => {
                steps.push(Step {
                    op,
                    text: &old_lines[old_index],
                    old_index: Some(old_index),
                    new_index: Some(new_index),
                });
                old_index += 1;
                new_index += 1;
            }
            Op::Remove => {
                steps.push(Step {
                    op,
                    text: &old_lines[old_index],
                    old_index: Some(old_index),
                    new_index: None,
                });
                old_index += 1;
            }
            Op::Add => {
                steps.push(Step {
                    op,
                    text: &new_lines[new_index],
                    old_index: None,
                    new_index: Some(new_index),
                });
                new_index += 1;
            }
        }
    }

    let changed: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| step.op != Op::Keep)
        .map(|(index, _)| index)
        .collect();

    if changed.is_empty() {
        return String::new();
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(DIFF_CONTEXT);
        let end = (index + 1 + DIFF_CONTEXT).min(steps.len());
        if let Some(last) = ranges.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        ranges.push((start, end));
    }

    let mut output = String::new();
    output.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));

    for (start, end) in ranges {
        let slice = &steps[start..end];
        let old_count = slice.iter().filter(|step| step.old_index.is_some()).count();
        let new_count = slice.iter().filter(|step| step.new_index.is_some()).count();
        let old_start = slice
            .iter()
            .find_map(|step| step.old_index)
            .map(|index| index + 1)
            .unwrap_or(0);
        let new_start = slice
            .iter()
            .find_map(|step| step.new_index)
            .map(|index| index + 1)
            .unwrap_or(0);

        output.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for step in slice {
            match step.op {
                Op::Keep => {
                    output.push(' ');
                    output.push_str(step.text);
                    output.push('\n');
                }
                Op::Remove => {
                    output.push('-');
                    output.push_str(step.text);
                    output.push('\n');
                }
                Op::Add => {
                    output.push('+');
                    output.push_str(step.text);
                    output.push('\n');
                }
            }
        }
    }

    output
}

fn diff_ops(old: &[String], new: &[String]) -> Vec<Op> {
    if old.len().saturating_mul(new.len()) > LCS_CELL_LIMIT {
        return whole_file_ops(old, new);
    }

    let mut table = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            table[old_index][new_index] = if old[old_index] == new[new_index] {
                table[old_index + 1][new_index + 1] + 1
            } else {
                table[old_index + 1][new_index].max(table[old_index][new_index + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(old.len() + new.len());
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            ops.push(Op::Keep);
            old_index += 1;
            new_index += 1;
        } else if table[old_index + 1][new_index] >= table[old_index][new_index + 1] {
            ops.push(Op::Remove);
            old_index += 1;
        } else {
            ops.push(Op::Add);
            new_index += 1;
        }
    }
    while old_index < old.len() {
        ops.push(Op::Remove);
        old_index += 1;
    }
    while new_index < new.len() {
        ops.push(Op::Add);
        new_index += 1;
    }
    ops
}

fn whole_file_ops(old: &[String], new: &[String]) -> Vec<Op> {
    let mut ops = Vec::with_capacity(old.len() + new.len());
    for _ in old {
        ops.push(Op::Remove);
    }
    for _ in new {
        ops.push(Op::Add);
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::test_support::temp_dir;

    #[test]
    fn split_and_join_round_trip() {
        for content in ["", "a", "a\n", "a\nb", "a\nb\n", "\n"] {
            let (lines, ends_with_newline) = split_lines(content);
            assert_eq!(join_lines(&lines, ends_with_newline), content);
        }
    }

    #[test]
    fn normalize_relative_rejects_escapes_and_absolutes() {
        let root = Path::new("/workspace");
        assert!(normalize_relative(root, "/etc/passwd").is_err());
        assert!(normalize_relative(root, "../outside").is_err());
        assert!(normalize_relative(root, "a/../../outside").is_err());
        assert!(normalize_relative(root, "").is_err());
    }

    #[test]
    fn normalize_relative_collapses_dot_components() {
        let root = Path::new("/workspace");
        let (candidate, normalized) = normalize_relative(root, "src/../lib.rs").unwrap();
        assert_eq!(candidate, PathBuf::from("/workspace/lib.rs"));
        assert_eq!(normalized, PathBuf::from("lib.rs"));
    }

    #[test]
    fn refused_paths_are_protected() {
        assert!(is_refused_path(Path::new(".git/config")));
        assert!(is_refused_path(Path::new(".gitignore")));
        assert!(is_refused_path(Path::new("src/.gitattributes")));
        assert!(is_refused_path(Path::new(".ignore")));
        assert!(!is_refused_path(Path::new("src/main.rs")));
    }

    #[test]
    fn parse_hunk_range_parses_counts() {
        assert_eq!(parse_hunk_range("-1,4"), Some((1, 4)));
        assert_eq!(parse_hunk_range("+5"), Some((5, 1)));
        assert_eq!(parse_hunk_range("-0,0"), Some((0, 0)));
        assert_eq!(parse_hunk_range("junk"), None);
    }

    #[test]
    fn parse_patch_extracts_hunks() {
        let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }\n";
        let parsed = parse_patch(patch, "src/lib.rs").unwrap();
        assert_eq!(parsed.hunks.len(), 1);
        let kinds: Vec<LineKind> = parsed.hunks[0].lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Context,
                LineKind::Remove,
                LineKind::Add,
                LineKind::Context
            ]
        );
    }

    #[test]
    fn parse_patch_requires_a_hunk() {
        let error = parse_patch("no hunks here", "src/lib.rs").unwrap_err();
        assert!(error.contains("no hunks"));
    }

    #[test]
    fn parse_patch_rejects_bare_hunk_header_with_helpful_message() {
        let error = parse_patch("@@\n-old\n+new\n", "src/lib.rs").unwrap_err();
        assert!(error.contains("invalid hunk header"));
        assert!(error.contains("@@ -1,1 +1,1 @@"));
    }

    #[test]
    fn apply_hunks_replaces_matching_lines() {
        let (lines, _) = split_lines("fn main() {\n    old();\n}\n");
        let patch = parse_patch(
            "@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }\n",
            "src/lib.rs",
        )
        .unwrap();
        let result = apply_hunks(&lines, &patch, "src/lib.rs").unwrap();
        assert_eq!(result, vec!["fn main() {", "    new();", "}"]);
    }

    #[test]
    fn apply_hunks_rejects_non_matching_context() {
        let (lines, _) = split_lines("fn main() {\n    untouched();\n}\n");
        let patch = parse_patch(
            "@@ -1,3 +1,3 @@\n fn main() {\n-    old();\n+    new();\n }\n",
            "src/lib.rs",
        )
        .unwrap();
        let error = apply_hunks(&lines, &patch, "src/lib.rs").unwrap_err();
        assert!(error.contains("does not apply"));
    }

    #[test]
    fn apply_hunks_adds_to_empty_file() {
        let lines: Vec<String> = Vec::new();
        let patch = parse_patch("@@ -0,0 +1,2 @@\n+fn main() {\n+}\n", "src/lib.rs").unwrap();
        let result = apply_hunks(&lines, &patch, "src/lib.rs").unwrap();
        assert_eq!(result, vec!["fn main() {", "}"]);
    }

    #[test]
    fn apply_hunks_handles_multiple_hunks() {
        let (lines, _) = split_lines("fn a() {}\nfn b() {}\nfn c() {}\n");
        let patch = parse_patch(
            "@@ -1,1 +1,1 @@\n-fn a() {}\n+fn a() { eprintln!(); }\n@@ -3,1 +3,1 @@\n-fn c() {}\n+fn c() { eprintln!(); }\n",
            "src/lib.rs",
        )
        .unwrap();
        let result = apply_hunks(&lines, &patch, "src/lib.rs").unwrap();
        assert_eq!(
            result,
            vec![
                "fn a() { eprintln!(); }",
                "fn b() {}",
                "fn c() { eprintln!(); }"
            ]
        );
    }

    #[test]
    fn unified_diff_reports_change() {
        let diff = unified_diff("a\nb\nc\n", "a\nx\nc\n", "src/lib.rs");
        assert!(diff.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"));
        assert!(diff.contains("-b\n"));
        assert!(diff.contains("+x\n"));
    }

    #[test]
    fn unified_diff_is_empty_without_changes() {
        assert_eq!(unified_diff("a\nb\n", "a\nb\n", "src/lib.rs"), "");
    }

    #[test]
    fn parse_edits_accepts_single_file_form() {
        let edits = parse_edits(&json!({
            "file": "src/lib.rs",
            "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "patch": "@@ -0,0 +1,1 @@\n+fn main() {}\n"
        }))
        .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "src/lib.rs");
    }

    #[test]
    fn parse_edits_accepts_content_form() {
        let edits = parse_edits(&json!({
            "file": "src/lib.rs",
            "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "content": "fn main() {}\n"
        }))
        .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "src/lib.rs");
        assert!(matches!(&edits[0].op, EditOp::Replace { .. }));
    }

    #[test]
    fn parse_edits_allows_empty_content() {
        let edits = parse_edits(&json!({
            "file": "src/lib.rs",
            "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "content": ""
        }))
        .unwrap();
        assert_eq!(edits.len(), 1);
        match &edits[0].op {
            EditOp::Replace { content } => assert_eq!(content, ""),
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn parse_edits_accepts_multi_file_form() {
        let edits = parse_edits(&json!({
            "files": [
                {
                    "path": "src/a.rs",
                    "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                    "patch": "@@ -0,0 +1,1 @@\n+fn a() {}\n"
                },
                {
                    "path": "src/b.rs",
                    "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                    "content": "fn b() {}\n"
                }
            ]
        }))
        .unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[1].path, "src/b.rs");
        assert!(matches!(&edits[1].op, EditOp::Replace { .. }));
    }

    #[test]
    fn parse_edits_rejects_mixed_forms() {
        let error = parse_edits(&json!({
            "file": "src/lib.rs",
            "files": []
        }))
        .unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("not both"));
    }

    #[test]
    fn parse_edits_rejects_both_patch_and_content() {
        let error = parse_edits(&json!({
            "file": "src/lib.rs",
            "base_token": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "patch": "@@ -0,0 +1,1 @@\n+fn main() {}\n",
            "content": "fn main() {}\n"
        }))
        .unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("not both"));
    }

    #[test]
    fn parse_edits_rejects_missing_required_arguments() {
        let error = parse_edits(&json!({
            "file": "src/lib.rs",
            "patch": "@@ -0,0 +1,1 @@\n+fn main() {}\n"
        }))
        .unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("base_token"));
    }

    #[test]
    fn apply_edits_in_rejects_path_through_regular_file_as_invalid() {
        let root = temp_dir("apply_edits_in_rejects_path_through_regular_file_as_invalid");
        fs::write(root.join("Cargo.toml"), "name = \"hanihi\"\n").unwrap();

        let edits = [Edit {
            path: "Cargo.toml/anything".to_string(),
            base_token: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
            op: EditOp::Patch {
                patch: "@@ -0,0 +1,1 @@\n+fn main() {}\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(!error.message.contains("internal"));
        assert!(error.message.contains("Cargo.toml/anything"));
    }

    #[test]
    fn apply_edits_in_classifies_path_before_hash_verification() {
        let root = temp_dir("apply_edits_in_classifies_path_before_hash_verification");
        fs::write(root.join("Cargo.toml"), "name = \"hanihi\"\n").unwrap();

        let edits = [Edit {
            path: "Cargo.toml/anything".to_string(),
            base_token: ALL_F_HASH.to_string(),
            op: EditOp::Patch {
                patch: "@@ -0,0 +1,1 @@\n+fn main() {}\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(!error.message.contains("mismatch"));
        assert!(!error.message.contains("placeholder"));
        assert!(error.message.contains("Cargo.toml/anything"));
    }

    #[test]
    fn resolve_through_regular_file_uses_directory_anchor() {
        let root = temp_dir("resolve_through_regular_file_uses_directory_anchor");
        fs::write(root.join("Cargo.toml"), "name = \"hanihi\"\n").unwrap();

        let resolved = resolve_within_root(&root, "Cargo.toml/anything").unwrap();
        assert_eq!(resolved, root.join("Cargo.toml/anything"));

        let mut anchor = resolved.parent().unwrap();
        while !anchor.is_dir() {
            anchor = anchor
                .parent()
                .expect("resolved path must have a directory ancestor");
        }
        assert_eq!(anchor, root.as_path());
    }

    #[test]
    fn apply_edits_in_creates_missing_file() {
        let root = temp_dir("apply_edits_in_creates_missing_file");

        let edits = [Edit {
            path: "new.txt".to_string(),
            base_token: workspace_fs::sha256_hex(b""),
            op: EditOp::Patch {
                patch: "@@ -0,0 +1,2 @@\n+fn main() {\n+}\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).unwrap(),
            "fn main() {\n}"
        );
    }

    #[test]
    fn apply_edits_in_edits_existing_file() {
        let root = temp_dir("apply_edits_in_edits_existing_file");
        fs::write(root.join("Cargo.toml"), "name = \"hanihi\"\n").unwrap();

        let edits = [Edit {
            path: "Cargo.toml".to_string(),
            base_token: workspace_fs::sha256_hex(b"name = \"hanihi\"\n"),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-name = \"hanihi\"\n+name = \"hanihi-mcp\"\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(
            fs::read_to_string(root.join("Cargo.toml")).unwrap(),
            "name = \"hanihi-mcp\"\n"
        );
    }

    #[test]
    fn apply_edits_in_replaces_whole_file_via_content() {
        let root = temp_dir("apply_edits_in_replaces_whole_file_via_content");
        fs::write(root.join("a.txt"), "old\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: workspace_fs::sha256_hex(b"old\n"),
            op: EditOp::Replace {
                content: "new\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "new\n");
    }

    #[test]
    fn apply_edits_in_replaces_with_empty_content() {
        let root = temp_dir("apply_edits_in_replaces_with_empty_content");
        fs::write(root.join("a.txt"), "old\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: workspace_fs::sha256_hex(b"old\n"),
            op: EditOp::Replace {
                content: String::new(),
            },
        }];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "");
    }

    #[test]
    fn apply_edits_in_rejects_null_placeholder() {
        let root = temp_dir("apply_edits_in_rejects_null_placeholder");
        fs::write(root.join("a.txt"), "line one\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: NULL_HASH.to_string(),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-line one\n+line two\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("placeholder"));
        assert!(
            error
                .message
                .contains(&workspace_fs::sha256_hex(b"line one\n"))
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "line one\n"
        );
    }

    #[test]
    fn apply_edits_in_rejects_all_f_placeholder() {
        let root = temp_dir("apply_edits_in_rejects_all_f_placeholder");
        fs::write(root.join("a.txt"), "line one\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: ALL_F_HASH.to_string(),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-line one\n+line two\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("placeholder"));
    }

    #[test]
    fn apply_edits_in_rejects_uppercase_all_f_placeholder() {
        let root = temp_dir("apply_edits_in_rejects_uppercase_all_f_placeholder");
        fs::write(root.join("a.txt"), "line one\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: ALL_F_HASH.to_ascii_uppercase(),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-line one\n+line two\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("placeholder"));
    }

    #[test]
    fn apply_edits_in_rejects_empty_content_hash_on_nonempty_file() {
        let root = temp_dir("apply_edits_in_rejects_empty_content_hash_on_nonempty_file");
        fs::write(root.join("a.txt"), "line one\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: EMPTY_CONTENT_HASH.to_string(),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-line one\n+line two\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty-content"));
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "line one\n"
        );
    }

    #[test]
    fn apply_edits_in_rejects_content_with_wrong_base_token() {
        let root = temp_dir("apply_edits_in_rejects_content_with_wrong_base_token");
        fs::write(root.join("a.txt"), "line one\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: workspace_fs::sha256_hex(b"different\n"),
            op: EditOp::Replace {
                content: "line two\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("mismatch"));
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "line one\n"
        );
    }

    #[test]
    fn apply_edits_in_applies_empty_content_hash_to_empty_file() {
        let root = temp_dir("apply_edits_in_applies_empty_content_hash_to_empty_file");
        fs::write(root.join("a.txt"), "").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: EMPTY_CONTENT_HASH.to_string(),
            op: EditOp::Patch {
                patch: "@@ -0,0 +1,2 @@\n+fn main() {\n+}\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "fn main() {\n}"
        );
    }
}
