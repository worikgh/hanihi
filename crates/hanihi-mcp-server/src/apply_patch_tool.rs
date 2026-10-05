//! `apply_patch` tool for Hānihi.
//!
//! Applies unified diffs or whole-file replacements to workspace files after
//! verifying each file's current contents against a caller-supplied version:
//! an opaque `{ "id": n }` handle, the legacy 64-hex digest, or `"auto"` for
//! the ledger's current version of the path. Every file is validated before
//! any write happens, so a failed patch never leaves a partial edit behind,
//! and a stale version never silently overwrites a concurrent change.
//!
//! A successful call returns each file's post-write version, so a caller
//! editing the same file repeatedly can chain edits without re-reading. The
//! precondition is unchanged: the version must describe the file's current
//! contents, and only the tool can vouch for what it just wrote.

use crate::version_ledger::{FileVersion, VersionLedger};
use crate::workspace_fs::{self, LineMismatch, ToolError, failure, internal, invalid, success};
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

/// How far a matching hunk may sit from its anchor before the drift report is
/// suppressed. Chosen against `DIFF_CONTEXT`: a hunk whose context is 3 lines
/// wide can plausibly be re-anchored by hand within a few lines of where the
/// caller expected it, but a match hundreds of lines away is a coincidence
/// rather than a location, and naming it would be noise the caller cannot act
/// on. The tool reports the distance; it never re-anchors the hunk itself.
const MAX_ACTIONABLE_DRIFT: usize = 5;

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
    "description": "Applies unified diffs or whole-file replacements to workspace files after verifying each file's current version. `base_token` accepts `\"auto\"` (the ledger's latest version for the path), an opaque `{ \"id\": n }` handle, or the legacy 64-hex SHA-256 digest. `dry_run: true` runs every check and returns the would-be result without writing. Returns each file's post-write version plus the resulting diff, and refuses paths outside the workspace or protected files.",
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
            "type": ["string", "object"],
            "description": "Version the file must currently have: `\"auto\"` to use the ledger's latest observed version for this path, an opaque `{ \"id\": n }` handle minted by `read_file` or a previous `apply_patch` to this same path, or the legacy 64-hex SHA-256 digest. For a new file, use the empty-content hash (e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855); never substitute a placeholder such as all zeros."
        },
        "dry_run": {
            "type": "boolean",
            "description": "When true, perform every check (path resolution, version verification, patch parsing, hunk matching, diff computation) and return the would-be result without writing anything."
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
                "type": ["string", "object"],
                "description": "Version the file must currently have: `\"auto\"`, an opaque `{ \"id\": n }` handle minted by `read_file` or a previous `apply_patch` to this same path, or the legacy 64-hex SHA-256 digest."
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
        Err(error) => failure(id, &error),
    }
}

/// One requested edit: either a unified diff to apply or a whole-file
/// replacement. The two forms are mutually exclusive.
#[derive(Debug)]
enum EditOp {
    Patch { patch: String },
    Replace { content: String },
}

/// The version the caller claims a file currently has.
#[derive(Debug)]
enum BaseVersion {
    /// Resolve to the ledger's most recent observed version for the path.
    Auto,
    /// An opaque handle minted by `read_file` or a prior `apply_patch`.
    Handle { id: u64 },
    /// Legacy form: a 64-hex SHA-256 digest.
    Digest(String),
}

#[derive(Debug)]
struct Edit {
    path: String,
    base_token: BaseVersion,
    op: EditOp,
}

/// Legacy result field carrying the post-write SHA-256 digest, so existing
/// callers that chain the raw digest keep working.
const NEW_TOKEN_FIELD: &str = "token";
/// Structured post-write version: `{ id, digest, len }`.
const VERSION_FIELD: &str = "version";

fn run(arguments: &Value) -> Result<String, ToolError> {
    let edits = parse_edits(arguments)?;
    if edits.is_empty() {
        return Err(invalid("at least one file edit is required"));
    }
    let dry_run = arguments
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let report = apply_edits(&edits, dry_run)?;
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
    let base = base_version(arguments)?;
    let op = edit_op(arguments)?;
    Ok(Edit {
        path: path.to_string(),
        base_token: base,
        op,
    })
}

/// Parses the `base_token` argument: a string (`auto` or a 64-hex digest) or
/// an opaque object handle `{ "id": n }`.
fn base_version(arguments: &Value) -> Result<BaseVersion, ToolError> {
    let value = arguments
        .get("base_token")
        .ok_or_else(|| invalid("missing required argument: base_token"))?;

    if let Some(text) = value.as_str() {
        if text.is_empty() {
            return Err(invalid("argument `base_token` must not be empty"));
        }
        if text == "auto" {
            return Ok(BaseVersion::Auto);
        }
        return Ok(BaseVersion::Digest(text.to_string()));
    }

    if let Some(object) = value.as_object() {
        let id = object
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("argument `base_token` object must have a numeric `id`"))?;
        return Ok(BaseVersion::Handle { id });
    }

    Err(invalid(
        "argument `base_token` must be a string (`auto` or a 64-hex digest) or an object `{ \"id\": n }`",
    ))
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

fn apply_edits(edits: &[Edit], dry_run: bool) -> Result<Value, ToolError> {
    let root = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
    let ledger = VersionLedger::for_root(&root).map_err(internal)?;
    apply_edits_with(&root, &ledger, edits, dry_run)
}

#[cfg(test)]
fn apply_edits_in(root: &Path, edits: &[Edit]) -> Result<Value, ToolError> {
    let ledger = VersionLedger::for_root(root).map_err(internal)?;
    apply_edits_with(root, &ledger, edits, false)
}

#[cfg(test)]
fn apply_edits_in_dry_run(root: &Path, edits: &[Edit]) -> Result<Value, ToolError> {
    let ledger = VersionLedger::for_root(root).map_err(internal)?;
    apply_edits_with(root, &ledger, edits, true)
}

/// Verifies the caller-supplied version against the file's current bytes.
/// The legacy digest form keeps its placeholder checks; `auto` and handles
/// resolve through the ledger, which is authoritative because the harness
/// wrote it.
fn verify_version(
    ledger: &VersionLedger,
    edit: &Edit,
    bytes: &[u8],
    actual: &str,
) -> Result<(), ToolError> {
    let expected = match &edit.base_token {
        BaseVersion::Digest(text) => {
            let normalized = workspace_fs::normalize_hash(text).ok_or_else(|| {
                invalid(format!(
                    "argument `base_token` must be a 64-character hex digest, got {text:?}"
                ))
            })?;
            if is_sentinel_version(&normalized) {
                return Err(invalid(format!(
                    "argument `base_token` for {} is a placeholder ({normalized}), not a file version. Copy the `token` returned by `read_file` for this path (current: {actual}), or use the empty-content hash ({EMPTY_CONTENT_HASH}) for a new file.",
                    edit.path
                )));
            }
            if normalized == EMPTY_CONTENT_HASH && !bytes.is_empty() {
                return Err(invalid(format!(
                    "argument `base_token` for {} is the empty-content hash, but the file has content ({} bytes). For an existing file, copy the `token` returned by `read_file` (current: {actual}); the empty-content hash is only valid for a new or empty file.",
                    edit.path,
                    bytes.len()
                )));
            }
            normalized
        }
        BaseVersion::Auto => match ledger.lookup(&edit.path).map_err(internal)? {
            Some(version) => version.digest,
            None => {
                return Err(invalid(format!(
                    "no observed version for {}: base_token auto has nothing to resolve",
                    edit.path
                ))
                .with_file(edit.path.clone())
                .with_recovery("read the file first"));
            }
        },
        BaseVersion::Handle { id } => match ledger.lookup(&edit.path).map_err(internal)? {
            Some(version) if version.id == Some(*id) => version.digest,
            Some(version) => {
                let current = version
                    .id
                    .map_or_else(|| "none".to_string(), |value| value.to_string());
                return Err(invalid(format!(
                    "handle {id} does not belong to {}: the ledger's current version has id {current}",
                    edit.path
                ))
                .with_file(edit.path.clone())
                .with_recovery("use base_token auto, or read the file first"));
            }
            None => {
                return Err(invalid(format!(
                    "handle {id} does not belong to {}: no version is recorded for this file",
                    edit.path
                ))
                .with_file(edit.path.clone())
                .with_recovery("read the file first"));
            }
        },
    };

    if actual != expected {
        return Err(internal(format!(
            "base_token mismatch for {}: expected {expected}, computed {actual}; refusing to overwrite concurrent changes. The token does not describe the file's current contents — it is stale or wrong. If you edited this file yourself, re-read it (or use the `token` echoed by your previous successful `apply_patch` to it) and retry with the fresh token.",
            edit.path
        ))
        .with_file(edit.path.clone())
        .with_actual(FileVersion::new(actual.to_string(), bytes.len() as u64))
        .with_recovery("pass this actual value as base_token, or use base_token auto"));
    }

    Ok(())
}

fn apply_edits_with(
    root: &Path,
    ledger: &VersionLedger,
    edits: &[Edit],
    dry_run: bool,
) -> Result<Value, ToolError> {
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

    // Phase 1: resolve, read, version-verify, and parse every edit. Nothing
    // is written yet, so one bad edit cannot leave a partial change behind.
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

        let actual = workspace_fs::sha256_hex(&bytes);
        verify_version(ledger, edit, &bytes, &actual)?;

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
                let new_lines = apply_hunks(&item.lines, patch, &item.display_path)
                    .map_err(|failure| failure.into_tool_error(&item.display_path))?;
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
    if !dry_run {
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
    }

    // The post-write digest is computed from the exact bytes that would be or
    // were written. A real write also records the version into the ledger so a
    // later `base_token: "auto"` resolves without a re-read; a dry run returns
    // the would-be version without minting an id.
    let mut versions = Vec::with_capacity(applied.len());
    for entry in &applied {
        let digest = workspace_fs::sha256_hex(entry.new_content.as_bytes());
        let len = entry.new_content.len() as u64;
        if dry_run {
            versions.push(FileVersion::new(digest, len));
            continue;
        }
        versions.push(
            ledger
                .record(&entry.display_path, &digest, len)
                .unwrap_or_else(|_| FileVersion::new(digest, len)),
        );
    }

    if dry_run {
        let would_change = applied
            .iter()
            .any(|entry| entry.original_bytes.as_slice() != entry.new_content.as_bytes());
        return Ok(json!({
            "would_change": would_change,
            "files": applied
                .iter()
                .zip(&versions)
                .map(|(entry, version)| json!({
                    "path": entry.display_path,
                    VERSION_FIELD: workspace_fs::version_to_json(version),
                    "diff": entry.diff,
                }))
                .collect::<Vec<_>>(),
        }));
    }

    Ok(json!({
    "files_changed": applied.len(),
    "files": applied
        .iter()
        .zip(&versions)
        .map(|(entry, version)| json!({
        "path": entry.display_path,
        "applied": true,
        NEW_TOKEN_FIELD: version.digest,
        VERSION_FIELD: workspace_fs::version_to_json(version),
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

impl Hunk {
    /// The old/new line counts implied by the hunk body: `(old, new)`, where
    /// the old side is every non-added line and the new side every
    /// non-removed line.
    fn line_counts(&self) -> (usize, usize) {
        let old = self
            .lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Add))
            .count();
        let new = self
            .lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Remove))
            .count();
        (old, new)
    }

    /// Fills in the counts of a hunk whose header omitted its ranges. A hunk
    /// with explicit counts is left untouched, so only `@@`-style headers are
    /// affected. The new side starts where the old side does; this is what
    /// makes the header-less form unambiguous for a pure replacement.
    fn derive_counts_from_body(&mut self) {
        if self.old_count != 0 || self.new_count != 0 {
            return;
        }
        let (old, new) = self.line_counts();
        self.old_count = old;
        self.new_count = new;
        self.new_start = self.old_start;
    }
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

    // Line where a range-less hunk's old side starts: one past the end of the
    // previous hunk, so consecutive `@@`-only headers stay unambiguous.
    let mut next_old_start = 1usize;

    for (index, raw) in patch.lines().enumerate() {
        let line_number = index + 1;

        if let Some(rest) = raw.strip_prefix("@@") {
            if rest.starts_with('@') {
                return Err(invalid_hunk_header(
                    path,
                    line_number,
                    raw,
                    "a header starts with exactly two `@` characters",
                ));
            }
            if let Some(hunk) = current.take() {
                next_old_start = hunk.old_start + hunk.old_count.max(hunk.line_counts().0);
                hunks.push(hunk);
            }
            let header = parse_hunk_header(rest).ok_or_else(|| {
                invalid_hunk_header(
                    path,
                    line_number,
                    raw,
                    "the ranges are not `<n>` or `<n>,<n>`",
                )
            })?;
            current = Some(match header {
                HunkHeader::Ranges {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                } => Hunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    lines: Vec::new(),
                },
                // Ranges omitted: anchored after the previous hunk and filled
                // in from the body once the body has been collected.
                HunkHeader::Derive => Hunk {
                    old_start: next_old_start,
                    old_count: 0,
                    new_start: next_old_start,
                    new_count: 0,
                    lines: Vec::new(),
                },
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

    for hunk in &mut hunks {
        hunk.derive_counts_from_body();
    }

    if hunks.is_empty() {
        return Err(format!("patch for {path} contains no hunks"));
    }
    Ok(ParsedPatch { hunks })
}

/// Renders a diagnostic for a malformed hunk header, naming the offending
/// line and reason, and teaching the required range syntax.
fn invalid_hunk_header(path: &str, line_number: usize, raw: &str, reason: &str) -> String {
    format!(
        "invalid hunk header in patch for {path} at line {line_number}: `{raw}` — {reason}. \
         Expected `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@`, for example \
         `@@ -1,1 +1,1 @@`; a range-less `@@` is also accepted and its ranges are derived \
         from the hunk body."
    )
}

/// A parsed hunk header: either explicit ranges, or a bare `@@` whose ranges
/// are derived from the body (matching `patch(1)` and `git apply` for
/// hand-written diffs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkHeader {
    Ranges {
        old_start: usize,
        old_count: usize,
        new_start: usize,
        new_count: usize,
    },
    Derive,
}

/// Parses the `-old,count +new,count` part of a hunk header (without the
/// leading `@@`). `Ok(HunkHeader::Derive)` means the header carried no ranges.
fn parse_hunk_header(rest: &str) -> Option<HunkHeader> {
    let mut parts = rest.split_whitespace();
    let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
        return Some(HunkHeader::Derive);
    };
    let (old_start, old_count) = parse_hunk_range(old)?;
    let (new_start, new_count) = parse_hunk_range(new)?;
    Some(HunkHeader::Ranges {
        old_start,
        old_count,
        new_start,
        new_count,
    })
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
///
/// A refusal carries the pre-existing message plus, where one was located, the
/// first file line that disagreed with the hunk. Reporting the difference is
/// the whole point: a tab typed as spaces is otherwise invisible.
fn apply_hunks(
    lines: &[String],
    patch: &ParsedPatch,
    path: &str,
) -> Result<Vec<String>, HunkFailure> {
    let mut current = lines.to_vec();
    let mut offset_shift: isize = 0;

    for hunk in &patch.hunks {
        let anchor = (hunk.old_start as isize - 1 + offset_shift).max(0) as usize;
        let message = || {
            format!(
                "patch for {path} does not apply: hunk @@ -{},{} +{},{} @@ does not match the current file contents",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            )
        };

        let position = anchor;
        if !hunk_matches(&current, anchor, hunk) {
            return Err(refusal_for(&current, hunk, anchor, message));
        }

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

/// Why a hunk was refused, carrying the unchanged historical message plus any
/// diagnostic the refusal path managed to locate.
#[derive(Debug)]
struct HunkFailure {
    /// The historical refusal sentence. Kept verbatim; existing callers and
    /// tests match on it.
    message: String,
    /// Set when the hunk matched nowhere and the first differing line could be
    /// named. Absent when nothing more could be determined.
    line_mismatch: Option<LineMismatch>,
    /// One imperative sentence naming the recovery.
    recovery: Option<String>,
}

impl HunkFailure {
    fn message(message: String) -> Self {
        Self {
            message,
            line_mismatch: None,
            recovery: None,
        }
    }

    /// The hunk matches the file, but `distance` lines away from the anchor.
    fn drifted(message: String, found: HunkMatch) -> Self {
        Self {
            recovery: Some(format!(
                "the hunk matches {} line(s) {} the anchor; re-read the file and re-anchor the hunk \
                 (the tool does not relocate a hunk for you)",
                found.distance,
                if found.offset > 0 { "below" } else { "above" }
            )),
            ..Self::message(message)
        }
    }

    /// The hunk matches nowhere; its line `hunk_index` differs from the file's
    /// line `file_index`. Both sides are rendered escaped so a whitespace-only
    /// difference stays visible.
    fn line_mismatch(
        message: String,
        hunk_index: usize,
        file_index: usize,
        lines: &[String],
        hunk: &Hunk,
    ) -> Self {
        let expected = hunk.lines.get(hunk_index).map(|line| &line.text);
        let found = lines.get(file_index);
        let recovery = Some(format!(
            "the context line differs at line {}: re-read the file and rebuild the hunk from its \
             actual text (expected and found are shown escaped, so `\\t` is a tab and a literal \
             space is a space)",
            file_index + 1
        ));

        Self {
            line_mismatch: Some(LineMismatch {
                // 1-based, matching how the rest of the tool reports lines.
                line: file_index + 1,
                expected: expected.map_or_else(String::new, |text| render_for_diagnostic(text)),
                found: found.map_or_else(String::new, |text| render_for_diagnostic(text)),
            }),
            recovery,
            ..Self::message(message)
        }
    }

    /// Converts the refusal into the caller-facing error, attaching the file
    /// the caller was editing so the payload always names its subject.
    fn into_tool_error(self, file: &str) -> ToolError {
        let error = invalid(self.message).with_file(file);
        let error = match self.recovery {
            Some(recovery) => error.with_recovery(recovery),
            None => error,
        };
        match self.line_mismatch {
            Some(mismatch) => {
                error.with_line_mismatch(mismatch.line, mismatch.expected, mismatch.found)
            }
            None => error,
        }
    }
}

/// Builds the refusal for a hunk that does not match at `anchor`. Prefers the
/// most specific explanation available, in order:
///
/// 1. The hunk matches elsewhere in the file, close enough that the caller can
///    re-anchor it by hand — report the drift.
/// 2. The hunk matches nowhere, but its context differs from the file at the
///    anchor — report the first differing line so a whitespace change is
///    visible.
/// 3. Otherwise the historical message alone.
fn refusal_for(
    lines: &[String],
    hunk: &Hunk,
    anchor: usize,
    message: impl Fn() -> String,
) -> HunkFailure {
    // A hunk that matches a few lines from the anchor is a stale anchor: the
    // caller moved the code, or re-read a stale copy. Report the distance so
    // it can re-anchor by hand. Beyond the threshold the closest match is a
    // coincidence rather than a location, so prefer the per-line report.
    if let Some(found) = find_hunk_position(lines, hunk, anchor)
        && found.distance > 0
        && found.distance <= MAX_ACTIONABLE_DRIFT
    {
        return HunkFailure::drifted(message(), found);
    }

    refusal_for_line(lines, hunk, anchor, message)
}

/// Reports the first file line whose content disagrees with the hunk, so a
/// whitespace-only difference is visible. Falls back to the message alone when
/// the hunk agrees at the anchor but still failed, which should not happen and
/// is therefore reported without speculation.
fn refusal_for_line(
    lines: &[String],
    hunk: &Hunk,
    anchor: usize,
    message: impl Fn() -> String,
) -> HunkFailure {
    match first_differing_line(lines, hunk, anchor) {
        Some((hunk_index, file_index)) => {
            HunkFailure::line_mismatch(message(), hunk_index, file_index, lines, hunk)
        }
        None => HunkFailure::message(message()),
    }
}

/// A hunk that matched somewhere in the file, and how far that position sits
/// from the anchor the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkMatch {
    /// Index of the file line where the hunk's first context or removed line
    /// matched.
    offset: usize,
    /// Absolute line distance between [`HunkMatch::offset`] and the anchor.
    /// Zero means the hunk matched exactly where it was anchored.
    distance: usize,
}

/// Returns the closest offset at which the hunk's context and removed lines
/// match, searching the whole file but preferring `anchor`. The distance is
/// carried alongside the offset so a caller can report drift rather than
/// silently relocating the hunk.
fn find_hunk_position(lines: &[String], hunk: &Hunk, anchor: usize) -> Option<HunkMatch> {
    let anchor = anchor.min(lines.len());
    let mut best: Option<HunkMatch> = None;

    for offset in 0..=lines.len() {
        if hunk_matches(lines, offset, hunk) {
            let distance = offset.abs_diff(anchor);
            let better = match best {
                Some(best) => distance < best.distance,
                None => true,
            };
            if better {
                best = Some(HunkMatch { offset, distance });
            }
        }
    }

    best
}

/// Renders a line for a refusal so whitespace is visible. `{:?}` escapes tabs
/// and newlines and quotes the result, which is what makes a tab-versus-spaces
/// difference legible instead of invisible. Both sides of a mismatch go
/// through this so the caller compares like with like.
fn render_for_diagnostic(line: &str) -> String {
    format!("{line:?}")
}

/// Finds the first line where a hunk's context or removed lines disagree with
/// the file at `offset`, returning the 0-based index within the hunk and the
/// file line that should have matched. `None` when every compared line
/// matches, which is what distinguishes drift from a content difference.
///
/// A position past the end of the file yields `None` as well: there is no file
/// line to name there, and reporting a line number that does not exist would
/// send the caller looking for text that is not in the file.
fn first_differing_line(lines: &[String], hunk: &Hunk, offset: usize) -> Option<(usize, usize)> {
    let mut index = offset;
    for (hunk_index, line) in hunk.lines.iter().enumerate() {
        match line.kind {
            LineKind::Context | LineKind::Remove => {
                if index >= lines.len() {
                    return None;
                }
                if lines[index] != line.text {
                    return Some((hunk_index, index));
                }
                index += 1;
            }
            LineKind::Add => {}
        }
    }
    None
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
        // `@@@` is not a header: it is a line that merely starts with `@@`, so
        // it must be reported rather than mistaken for a hunk.
        let error = parse_patch("@@@\n-old\n+new\n", "src/lib.rs").unwrap_err();
        assert!(error.contains("invalid hunk header"), "got: {error}");
        assert!(error.contains("line 1"), "got: {error}");
        assert!(error.contains("exactly two `@`"), "got: {error}");
        assert!(error.contains("@@ -1,1 +1,1 @@"), "got: {error}");
    }

    /// A malformed range names the offending line, so the caller can find it
    /// without counting lines in the patch by hand.
    #[test]
    fn parse_patch_reports_malformed_range_with_line_number() {
        let error = parse_patch("@@ -1,x +1,1 @@\n-old\n+new\n", "src/lib.rs").unwrap_err();
        assert!(error.contains("invalid hunk header"), "got: {error}");
        assert!(error.contains("line 1"), "got: {error}");
        assert!(error.contains("-1,x"), "got: {error}");
    }

    /// A range-less `@@` header is legal: the ranges come from the body. This
    /// is the failure that motivated the change — a hand-written or
    /// string-escaped patch that lost its ranges must still apply.
    #[test]
    fn parse_patch_derives_ranges_from_a_range_less_header() {
        let parsed = parse_patch(
            "@@\n fn main() {\n-    old();\n+    new();\n }\n",
            "src/lib.rs",
        )
        .unwrap();
        assert_eq!(parsed.hunks.len(), 1);
        let hunk = &parsed.hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.old_count, 3);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.new_count, 3);

        let (lines, _) = split_lines("fn main() {\n    old();\n}\n");
        let result = apply_hunks(&lines, &parsed, "src/lib.rs").unwrap();
        assert_eq!(result, vec!["fn main() {", "    new();", "}"]);
    }

    /// A second range-less hunk is anchored after the first, so consecutive
    /// header-less hunks do not collide at line 1.
    #[test]
    fn parse_patch_anchors_a_second_range_less_hunk_after_the_first() {
        let parsed = parse_patch(
            "@@\n-fn a() {}\n+fn a() { x(); }\n@@\n-fn c() {}\n+fn c() { x(); }\n",
            "src/lib.rs",
        )
        .unwrap();
        assert_eq!(parsed.hunks.len(), 2);
        assert_eq!(parsed.hunks[0].old_start, 1);
        assert_eq!(parsed.hunks[1].old_start, 2);
    }

    /// An explicit header is never rewritten by the derivation pass.
    #[test]
    fn parse_patch_keeps_explicit_ranges() {
        let parsed = parse_patch("@@ -7,2 +7,2 @@\n-old\n+new\n", "src/lib.rs").unwrap();
        assert_eq!(parsed.hunks[0].old_start, 7);
        assert_eq!(parsed.hunks[0].old_count, 2);
        assert_eq!(parsed.hunks[0].new_start, 7);
        assert_eq!(parsed.hunks[0].new_count, 2);
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
        assert!(error.message.contains("does not apply"));
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

    /// A stale token — the failure mode that motivated the token echo: a
    /// caller edits a file, then reuses the pre-edit token for a second edit
    /// without re-reading.
    #[test]
    fn apply_edits_in_rejects_a_token_from_before_its_own_write() {
        let root = temp_dir("apply_edits_in_rejects_a_token_from_before_its_own_write");
        fs::write(root.join("a.txt"), "old\n").unwrap();
        let stale = workspace_fs::sha256_hex(b"old\n");

        let first = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(stale.clone()),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-old\n+first\n".to_string(),
            },
        }];
        apply_edits_in(&root, &first).unwrap();

        // Same token again: the file is now "first\n", so this must be refused.
        let second = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(stale),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-first\n+second\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &second).unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("mismatch"), "got: {}", error.message);
        assert!(
            error.message.contains("re-read"),
            "the message must name the recovery step, got: {}",
            error.message
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "first\n",
            "the refused edit must not have been applied"
        );
    }

    /// The echoed token is the hash of the bytes the tool actually wrote, so
    /// chaining it into a second edit succeeds without a re-read.
    #[test]
    fn apply_edits_in_returns_a_token_that_chains_into_the_next_edit() {
        let root = temp_dir("apply_edits_in_returns_a_token_that_chains_into_the_next_edit");
        fs::write(root.join("a.txt"), "old\n").unwrap();

        let first = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"old\n")),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-old\n+first\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &first).unwrap();
        let token = report["files"][0][NEW_TOKEN_FIELD]
            .as_str()
            .expect("a successful apply reports the new token")
            .to_string();
        assert_eq!(token, workspace_fs::sha256_hex(b"first\n"));

        let second = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(token),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-first\n+second\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &second).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(
            report["files"][0][NEW_TOKEN_FIELD],
            json!(workspace_fs::sha256_hex(b"second\n"))
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "second\n");
    }

    /// Every file in a multi-file call carries its own forward token.
    #[test]
    fn apply_edits_in_returns_a_token_per_file() {
        let root = temp_dir("apply_edits_in_returns_a_token_per_file");

        let edits = [
            Edit {
                path: "a.txt".to_string(),
                base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"")),
                op: EditOp::Replace {
                    content: "a\n".to_string(),
                },
            },
            Edit {
                path: "b.txt".to_string(),
                base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"")),
                op: EditOp::Replace {
                    content: "b\n".to_string(),
                },
            },
        ];
        let report = apply_edits_in(&root, &edits).unwrap();
        assert_eq!(report["files_changed"], json!(2));
        assert_eq!(
            report["files"][0][NEW_TOKEN_FIELD],
            json!(workspace_fs::sha256_hex(b"a\n"))
        );
        assert_eq!(
            report["files"][1][NEW_TOKEN_FIELD],
            json!(workspace_fs::sha256_hex(b"b\n"))
        );
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
            base_token: BaseVersion::Digest(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            ),
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
            base_token: BaseVersion::Digest(ALL_F_HASH.to_string()),
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
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"")),
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
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"name = \"hanihi\"\n")),
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
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"old\n")),
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
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"old\n")),
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
            base_token: BaseVersion::Digest(NULL_HASH.to_string()),
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
            base_token: BaseVersion::Digest(ALL_F_HASH.to_string()),
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
            base_token: BaseVersion::Digest(ALL_F_HASH.to_ascii_uppercase()),
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
            base_token: BaseVersion::Digest(EMPTY_CONTENT_HASH.to_string()),
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
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"different\n")),
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
            base_token: BaseVersion::Digest(EMPTY_CONTENT_HASH.to_string()),
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

    /// Two edits in a row, the second using `auto`, must both land: the ledger
    /// records the first write, so `auto` resolves without a re-read.
    #[test]
    fn apply_patch_chains_two_edits_with_auto() {
        let root = temp_dir("apply_patch_chains_two_edits_with_auto");
        fs::write(root.join("a.txt"), "old\n").unwrap();

        let first = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"old\n")),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-old\n+first\n".to_string(),
            },
        }];
        apply_edits_in(&root, &first).unwrap();

        let second = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Auto,
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-first\n+second\n".to_string(),
            },
        }];
        let report = apply_edits_in(&root, &second).unwrap();
        assert_eq!(report["files_changed"], json!(1));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "second\n");
    }

    /// A handle minted for one path must refuse to verify a different path.
    #[test]
    fn opaque_handle_is_bound_to_its_path() {
        let root = temp_dir("opaque_handle_is_bound_to_its_path");
        fs::write(root.join("a.txt"), "one\n").unwrap();
        fs::write(root.join("b.txt"), "two\n").unwrap();

        let ledger = VersionLedger::for_root(&root).unwrap();
        let handle = ledger
            .record("a.txt", &workspace_fs::sha256_hex(b"one\n"), 4)
            .unwrap();

        let edits = [Edit {
            path: "b.txt".to_string(),
            base_token: BaseVersion::Handle {
                id: handle.id.unwrap(),
            },
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-two\n+three\n".to_string(),
            },
        }];
        let error = apply_edits_with(&root, &ledger, &edits, false).unwrap_err();
        assert!(
            error.message.contains("does not belong to b.txt"),
            "got: {}",
            error.message
        );
        assert_eq!(error.file.as_deref(), Some("b.txt"));
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "two\n");
    }

    /// A hunk whose context differs from the file names the first differing
    /// line, escaped on both sides, so the caller can see what disagrees.
    #[test]
    fn mismatch_names_the_first_differing_line() {
        let root = temp_dir("mismatch_names_the_first_differing_line");
        // Line 2 is "BRAVO"; the patch below expects "bravo".
        fs::write(root.join("a.txt"), "alpha\nBRAVO\ncharlie\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"alpha\nBRAVO\ncharlie\n")),
            op: EditOp::Patch {
                // A removed line that the file does not contain at line 2.
                patch: "@@ -1,3 +1,3 @@\n alpha\n-bravo\n+BRAVO\n charlie\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(
            error.message.contains("does not apply"),
            "got: {}",
            error.message
        );
        let mismatch = error
            .line_mismatch
            .as_deref()
            .expect("a differing line was located");
        assert_eq!(mismatch.line, 2);
        assert_eq!(mismatch.expected, "\"bravo\"");
        assert_eq!(mismatch.found, "\"BRAVO\"");
        assert!(
            error
                .recovery
                .as_deref()
                .is_some_and(|text| text.contains("line 2")),
            "recovery must name the line, got: {:?}",
            error.recovery
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\nBRAVO\ncharlie\n",
            "the refused edit must not have been applied"
        );
    }

    /// The regression test for the observed failure: the file has a tab where
    /// the patch has spaces. Both sides are escaped, so the two renderings are
    /// distinguishable rather than visually identical.
    #[test]
    fn mismatch_escapes_whitespace_in_both_lines() {
        let root = temp_dir("mismatch_escapes_whitespace_in_both_lines");
        // Line 2 begins with a real tab.
        fs::write(root.join("a.txt"), "fn main() {\n\tcall();\n}\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(
                b"fn main() {\n\tcall();\n}\n",
            )),
            op: EditOp::Patch {
                // The patch claims four spaces, not a tab.
                patch: "@@ -1,3 +1,3 @@\n fn main() {\n-    call();\n+    other();\n }\n"
                    .to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        let mismatch = error
            .line_mismatch
            .as_deref()
            .expect("a differing line was located");
        assert_eq!(mismatch.line, 2);
        let expected = mismatch.expected.as_str();
        let found = mismatch.found.as_str();
        assert_ne!(
            expected, found,
            "escaped renderings must differ for a tab-versus-spaces difference"
        );
        // The file's tab renders as the two-character escape; the patch's four
        // spaces stay literal spaces. That difference is the whole point.
        assert!(
            found.contains("\\t"),
            "file line must show a tab escape: {found}"
        );
        assert!(
            expected.contains("    "),
            "patch line must show literal spaces: {expected}"
        );
    }

    /// When no line of the file matches, there is nothing to compare against,
    /// so the per-line fields are absent and the message is unchanged.
    #[test]
    fn mismatch_with_no_matching_line_omits_the_line_field() {
        let root = temp_dir("mismatch_with_no_matching_line_omits_the_line_field");
        fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"one\ntwo\nthree\nfour\n")),
            op: EditOp::Patch {
                // Anchored past the end of the file: no comparable line exists.
                patch: "@@ -40,2 +40,2 @@\n-alpha\n+ALPHA\n-bravo\n+BRAVO\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert!(
            error.message.contains("does not apply"),
            "got: {}",
            error.message
        );
        assert!(error.line_mismatch.is_none());
    }

    /// Item D: a hunk that matches a few lines below its anchor reports the
    /// drift rather than relocating itself.
    #[test]
    fn drifted_hunk_reports_the_candidate_distance() {
        let root = temp_dir("drifted_hunk_reports_the_candidate_distance");
        fs::write(
            root.join("a.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        )
        .unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
            )),
            op: EditOp::Patch {
                // Claims line 1 ("one") but the body actually matches line 4.
                patch: "@@ -1,1 +1,1 @@\n-four\n+FOUR\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert!(
            error.message.contains("does not apply"),
            "got: {}",
            error.message
        );
        assert!(
            error
                .recovery
                .as_deref()
                .is_some_and(|text| text.contains("3 line(s) below the anchor")),
            "recovery must name the drift, got: {:?}",
            error.recovery
        );
    }

    /// Guards item D against being implemented as an auto-correct: the hunk is
    /// reported, never applied somewhere other than where the caller anchored
    /// it.
    #[test]
    fn drifted_hunk_is_not_relocated() {
        let root = temp_dir("drifted_hunk_is_not_relocated");
        let original = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        fs::write(root.join("a.txt"), original).unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(original.as_bytes())),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-four\n+FOUR\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert!(
            error.recovery.is_some(),
            "a drifted hunk must carry a recovery"
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            original,
            "a drifted hunk must be reported, never relocated"
        );
    }

    #[test]
    fn mismatch_returns_structured_refusal_payload() {
        let root = temp_dir("mismatch_returns_structured_refusal_payload");
        fs::write(root.join("a.txt"), "now\n").unwrap();
        let now_digest = workspace_fs::sha256_hex(b"now\n");

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"stale\n")),
            op: EditOp::Replace {
                content: "other\n".to_string(),
            },
        }];
        let error = apply_edits_in(&root, &edits).unwrap_err();
        assert_eq!(error.code, -32603);
        assert_eq!(error.file.as_deref(), Some("a.txt"));
        assert_eq!(
            error.actual.as_ref().map(|v| v.digest.as_str()),
            Some(now_digest.as_str())
        );
        assert_eq!(
            error.recovery.as_deref(),
            Some("pass this actual value as base_token, or use base_token auto")
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "now\n");
    }

    /// A dry run performs every check and reports the would-be version, but
    /// writes nothing.
    #[test]
    fn dry_run_writes_nothing_but_reports_the_would_be_version() {
        let root = temp_dir("dry_run_writes_nothing_but_reports_the_would_be_version");
        fs::write(root.join("a.txt"), "old\n").unwrap();

        let edits = [Edit {
            path: "a.txt".to_string(),
            base_token: BaseVersion::Digest(workspace_fs::sha256_hex(b"old\n")),
            op: EditOp::Patch {
                patch: "@@ -1,1 +1,1 @@\n-old\n+new\n".to_string(),
            },
        }];
        let report = apply_edits_in_dry_run(&root, &edits).unwrap();
        assert_eq!(report["would_change"], json!(true));
        assert_eq!(
            report["files"][0]["version"]["digest"],
            json!(workspace_fs::sha256_hex(b"new\n"))
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "old\n");
    }
}
