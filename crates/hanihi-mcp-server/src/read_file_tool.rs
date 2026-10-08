//! `read_file` tool for Hānihi: reads a workspace file and returns its
//! content plus the version `apply_patch` expects as `base_token`.
//!
//! The `version` object is always present, even when `content` is truncated:
//! its digest is the SHA-256 of the *whole* file, so it is a valid
//! `base_token` regardless. `truncated_at` is `Some(total_len)` whenever
//! `content` is only a prefix of the file.
//!
//! `offset`/`limit` are **byte** quantities, not line numbers. A caller that
//! wants a line-anchored read passes `line` instead: it is mutually exclusive
//! with `offset` and resolves to a byte offset before the shared read path
//! runs, so `limit`, truncation accounting, and the version ledger behave
//! identically for both selectors.

use crate::version_ledger::{FileVersion, VersionLedger};
use crate::workspace_fs::{self, ToolError, version_to_json};
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// Maximum bytes of a file's content returned by default.
///
/// This is the per-file read cap: `read_file` returns at most this much body
/// text in a single call, while the version digest is always computed over
/// the whole file. The agent-layer `MAX_TOOL_RESULT_BYTES` backstop still
/// applies to the rendered result and may clip body text further, but the
/// version fields are serialized before `content`, so they survive.
const MAX_READ_BYTES: usize = 128 * 1024;

/// Lines are 1-based in the tool's interface, matching every editor and
/// `grep`. The first line of a file is line 1, never line 0.
const FIRST_LINE: u64 = 1;

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "read_file",
    "description": "Reads the file at the given workspace-relative path and returns its content plus a `version` object whose digest is the SHA-256 of the whole file — a valid `base_token` even when `content` is truncated. Use `limit` (bytes, not lines) with either `offset` (bytes, not lines) or `line` (1-based) to read a large file in ranges without relying on truncation.",
    "inputSchema": {
    "type": "object",
    "properties": {
    "path": {
    "type": "string",
    "description": "Path of the file to read, relative to the workspace root."
    },
    "offset": {
    "type": "integer",
    "description": "Byte offset (not a line number) at which to start the returned content. Defaults to 0. Mutually exclusive with `line`."
    },
    "line": {
    "type": "integer",
    "description": "1-based line number at which to start the returned content. Mutually exclusive with `offset`; the first line of a file is line 1."
    },
    "limit": {
    "type": "integer",
    "description": "Maximum number of bytes to return (not lines). Defaults to the per-file read cap."
    }
    },
    "required": ["path"],
    "additionalProperties": false
    }
    })
}

/// Implements the tool. `params` carries the MCP tool call; its `arguments`
/// object holds the request.
pub(crate) fn exec(params: &Value, id: Value) -> Value {
    let result = workspace_fs::arguments(params).and_then(run);
    match result {
        Ok(text) => workspace_fs::success(id, text),
        Err(error) => workspace_fs::failure(id, &error),
    }
}

fn run(arguments: &Value) -> Result<String, ToolError> {
    let path = workspace_fs::required_non_empty_string(arguments, "path")?;
    let offset = optional_unsigned(arguments, "offset", 0)?;
    let limit = optional_unsigned(arguments, "limit", MAX_READ_BYTES as u64)?;
    let line = optional_unsigned(arguments, "line", FIRST_LINE)?;
    let has_offset = present(arguments, "offset");
    let has_line = present(arguments, "line");

    if has_offset && has_line {
        return Err(workspace_fs::invalid(
            "arguments `line` and `offset` are mutually exclusive: pass `line` (1-based) or `offset` (bytes), not both",
        ));
    }
    if has_line && line < FIRST_LINE {
        return Err(workspace_fs::invalid(
            "argument `line` must be a 1-based line number: the first line of a file is line 1, not line 0",
        ));
    }

    let root = workspace_fs::workspace_root()?;
    let resolved =
        workspace_fs::resolve_workspace_path(&root, path).map_err(workspace_fs::invalid)?;
    let ledger = VersionLedger::for_root(&root).map_err(workspace_fs::internal)?;

    let selection = if has_line {
        Selection::Lines(line)
    } else {
        Selection::Bytes(offset)
    };
    eprintln!(
        "{}:{}: Read file: {path} selection: {selection:?} limit: {limit}",
        file!(),
        line!()
    );
    read_file_selected(&resolved, path, Some(&ledger), selection, limit)
}

/// True when `name` is explicitly present in `arguments`, even as `null`.
/// Distinguishes "caller supplied `offset`" from "caller omitted `offset`",
/// which is what the `line`/`offset` exclusivity check needs: `offset: 0` is a
/// meaningful request that must still conflict with `line`.
fn present(arguments: &Value, name: &str) -> bool {
    matches!(arguments.get(name), Some(value) if !value.is_null())
}

/// Resolves a 1-based line number to the byte offset where that line starts.
///
/// A line is delimited by `\n` and the newline belongs to the line it
/// terminates, so line 1 of `"alpha\nbravo\n"` is `"alpha\n"` and starts at
/// byte 0. The final line needs no trailing newline to be counted, which is
/// what makes the count agree with an editor's.
///
/// A line past the end of the file is refused with the line count: a caller
/// cannot correct a line number without knowing how many lines exist.
fn line_start_offset(content: &str, line: u64, display_path: &str) -> Result<u64, ToolError> {
    if line <= FIRST_LINE {
        return Ok(0);
    }

    let mut lines_seen = FIRST_LINE;
    for (index, byte) in content.bytes().enumerate() {
        if byte != b'\n' {
            continue;
        }
        lines_seen += 1;
        if lines_seen == line {
            return Ok(index as u64 + 1);
        }
    }

    // `lines_seen` counts newline-delimited lines, so a file not ending in a
    // newline has one more line than it has newlines, and an empty file none.
    let total_lines = if content.is_empty() || content.ends_with('\n') {
        lines_seen - 1
    } else {
        lines_seen
    };
    Err(workspace_fs::invalid(format!(
        "argument `line` is past the end of {display_path}: line {line} was requested but the file has {total_lines} line(s)"
    )))
}

/// Reads a non-negative integer argument, or `default` when absent.
fn optional_unsigned(arguments: &Value, name: &str, default: u64) -> Result<u64, ToolError> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().ok_or_else(|| {
            workspace_fs::invalid(format!("argument `{name}` must be a non-negative integer"))
        }),
    }
}

/// How the caller asked to be positioned in the file. Both variants resolve to
/// a byte offset before the read runs, so `limit`, truncation accounting, and
/// the version ledger cannot diverge between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// Start at this byte offset.
    Bytes(u64),
    /// Start at this 1-based line.
    Lines(u64),
}

/// Test-facing wrapper: a default full read without a ledger, so the returned
/// version carries no opaque id.
#[cfg(test)]
fn read_file(path: &Path, display_path: &str) -> Result<String, ToolError> {
    read_file_with(path, display_path, None, 0, MAX_READ_BYTES as u64)
}

/// Test-facing wrapper for a line-anchored read. `limit` of `None` means the
/// default per-file cap.
#[cfg(test)]
fn read_file_with_line(
    path: &Path,
    display_path: &str,
    ledger: Option<&VersionLedger>,
    line: u64,
    limit: Option<u64>,
) -> Result<String, ToolError> {
    read_file_selected(
        path,
        display_path,
        ledger,
        Selection::Lines(line),
        limit.unwrap_or(MAX_READ_BYTES as u64),
    )
}

/// Test-facing wrapper for a byte-offset read, so the offset path keeps its
/// own coverage independent of the `line` selector.
#[cfg(test)]
fn read_file_with(
    path: &Path,
    display_path: &str,
    ledger: Option<&VersionLedger>,
    offset: u64,
    limit: u64,
) -> Result<String, ToolError> {
    read_file_selected(path, display_path, ledger, Selection::Bytes(offset), limit)
}

fn read_file_selected(
    path: &Path,
    display_path: &str,
    ledger: Option<&VersionLedger>,
    selection: Selection,
    limit: u64,
) -> Result<String, ToolError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(workspace_fs::invalid(format!(
                "file does not exist: {display_path}"
            )));
        }
        Err(error) if error.kind() == ErrorKind::NotADirectory => {
            return Err(workspace_fs::invalid(format!(
                "argument `path` is not a valid file path: {display_path} (a parent component is not a directory)"
            )));
        }
        Err(error) if error.kind() == ErrorKind::IsADirectory => {
            return Err(workspace_fs::invalid(format!(
                "{display_path} is a directory, not a file"
            )));
        }
        Err(error) => {
            return Err(workspace_fs::internal(format!(
                "cannot read {display_path}: {error}"
            )));
        }
    };

    // Invariant: hash the raw file bytes, not the decoded String. `apply_patch`
    // verifies against `fs::read` bytes, so hashing the String here would
    // mismatch on BOMs, CRLF, or any non-canonical encoding.
    let digest = workspace_fs::sha256_hex(&bytes);
    let total_len = bytes.len() as u64;
    let content = String::from_utf8(bytes)
        .map_err(|_| workspace_fs::internal(format!("file is not valid UTF-8: {display_path}")))?;

    // Both selectors collapse to a byte offset here, so everything below —
    // `limit`, truncation accounting, the ledger — is shared verbatim.
    let (offset, line_field) = match selection {
        Selection::Bytes(offset) => (offset, None),
        Selection::Lines(line) => (line_start_offset(&content, line, display_path)?, Some(line)),
    };

    // Slice the requested byte range, keeping UTF-8 boundaries intact.
    let start = content.floor_char_boundary((offset as usize).min(content.len()));
    let end = content.floor_char_boundary(start.saturating_add(limit as usize).min(content.len()));
    let visible = &content[start..end];
    let truncated_at = (end < content.len()).then_some(total_len);

    // Complete reads record into the ledger so `base_token: "auto"` resolves
    // without a re-read. Truncated or offset reads still return a correct
    // whole-file digest but do not mint a handle, since the caller has not
    // observed the whole file.
    let version = match (ledger, offset, truncated_at) {
        (Some(ledger), 0, None) => ledger
            .record(display_path, &digest, total_len)
            .unwrap_or_else(|_| FileVersion::new(digest.clone(), total_len)),
        _ => FileVersion::new(digest.clone(), total_len),
    };

    // The version fields precede `content` so the agent-layer result backstop
    // can only ever clip body text, never the usable token.
    //
    // `line` is inserted after the fact rather than given a `None` value: a
    // byte-anchored read must not report a line it never resolved, and
    // `json!({"line": None})` would serialize an explicit `null`.
    let mut report = serde_json::Map::new();
    report.insert("path".to_string(), json!(display_path));
    report.insert("version".to_string(), version_to_json(&version));
    report.insert("token".to_string(), json!(digest));
    report.insert("sha256".to_string(), json!(digest));
    report.insert("size".to_string(), json!(total_len));
    report.insert("truncated_at".to_string(), json!(truncated_at));
    report.insert("offset".to_string(), json!(start));
    if let Some(line) = line_field {
        report.insert("line".to_string(), json!(line));
    }
    report.insert("content".to_string(), json!(visible));

    let report = Value::Object(report);
    serde_json::to_string_pretty(&report)
        .map_err(|error| workspace_fs::internal(format!("failed to serialize result: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::test_support::temp_dir;

    #[test]
    fn json_exposes_required_arguments() {
        let schema = json();
        assert_eq!(schema["name"], json!("read_file"));
        assert_eq!(schema["inputSchema"]["required"], json!(["path"]));
    }

    #[test]
    fn run_requires_path() {
        let error = run(&json!({})).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("path"));
    }

    #[test]
    fn run_rejects_empty_path() {
        let error = run(&json!({ "path": "" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty"));
    }

    #[test]
    fn reads_file_and_returns_hash() {
        let dir = temp_dir("read_file_hash");
        let path = dir.join("a.txt");
        std::fs::write(&path, "abc").unwrap();

        let result = read_file(&path, "a.txt").unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(report["path"], json!("a.txt"));
        assert_eq!(report["size"], json!(3));
        assert_eq!(report["content"], json!("abc"));
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(report["sha256"], json!(expected));
        assert_eq!(report["token"], json!(expected));
        assert_eq!(report["version"]["digest"], json!(expected));
        assert_eq!(report["version"]["len"], json!(3));
        assert!(report["version"].get("id").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_invalid_params() {
        let dir = temp_dir("read_file_missing");
        let error = read_file(&dir.join("missing.txt"), "missing.txt").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("does not exist"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_through_regular_file_is_invalid_params() {
        let dir = temp_dir("read_file_through_regular_file");
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let error = read_file(&file.join("anything"), "a.txt/anything").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("a.txt/anything"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_is_invalid_params() {
        let dir = temp_dir("read_file_directory");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let error = read_file(&sub, "sub").unwrap_err();
        assert_eq!(error.code, -32602);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_utf8_file_reports_internal_error() {
        let dir = temp_dir("read_file_non_utf8");
        let path = dir.join("a.bin");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();

        let error = read_file(&path, "a.bin").unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("UTF-8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_read_still_returns_a_usable_token() {
        let dir = temp_dir("read_file_truncated");
        let path = dir.join("big.txt");
        let total = MAX_READ_BYTES + 4096;
        let payload = "x".repeat(total);
        std::fs::write(&path, &payload).unwrap();

        let result = read_file(&path, "big.txt").unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        let full_digest = workspace_fs::sha256_hex(payload.as_bytes());
        assert_eq!(report["version"]["digest"], json!(full_digest));
        assert_eq!(report["version"]["len"], json!(total));
        assert_eq!(report["truncated_at"], json!(total));
        assert_eq!(report["token"], json!(full_digest));
        assert!(report["content"].as_str().unwrap().len() <= MAX_READ_BYTES);

        // The version must be serialized before the (large) content so the
        // agent-layer backstop can never clip it.
        let version_pos = result.find("\"version\"").expect("version present");
        let content_pos = result.find("\"content\"").expect("content present");
        assert!(version_pos < content_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offset_and_limit_read_a_range() {
        let dir = temp_dir("read_file_range");
        let path = dir.join("a.txt");
        std::fs::write(&path, "abcdef").unwrap();

        let result = read_file_with(&path, "a.txt", None, 1, 3).unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(report["content"], json!("bcd"));
        assert_eq!(report["offset"], json!(1));
        assert_eq!(report["size"], json!(6));
        assert_eq!(
            report["version"]["digest"],
            json!(workspace_fs::sha256_hex(b"abcdef"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Item A: the units must be unmissable -------------------------------

    /// Item A is a loudness fix, not a correction: the schema must *deny* the
    /// wrong unit, so a skimming caller cannot read `offset` as a line number.
    #[test]
    fn offset_description_names_bytes_not_lines() {
        let schema = json();
        let offset = schema["inputSchema"]["properties"]["offset"]["description"]
            .as_str()
            .expect("offset description");
        assert!(offset.contains("not a line number"), "got: {offset}");

        let limit = schema["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .expect("limit description");
        assert!(limit.contains("not lines"), "got: {limit}");
    }

    /// The tool-level description must carry the same unit warning, so the
    /// caller sees it even when it never reads the per-field schema.
    #[test]
    fn tool_description_names_bytes_not_lines() {
        let schema = json();
        let description = schema["description"].as_str().expect("description");
        assert!(description.contains("not lines"), "got: {description}");
    }

    // --- Item B: the `line` selector ---------------------------------------

    /// Writes a 3-line file whose lines are `alpha`, `bravo`, `charlie`.
    fn write_three_line_file(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("lines.txt");
        std::fs::write(&path, "alpha\nbravo\ncharlie\n").unwrap();
        path
    }

    #[test]
    fn line_reads_from_the_requested_line() {
        let dir = temp_dir("read_file_line_from");
        let path = write_three_line_file(&dir);

        let result = read_file_with_line(&path, "lines.txt", None, 3, None).unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        // Line 3 is `charlie\n`, which starts at byte 12 in `alpha\nbravo\n`.
        assert_eq!(report["content"], json!("charlie\n"));
        assert_eq!(report["line"], json!(3));
        assert_eq!(report["offset"], json!(12));
        // The body is a suffix, so the whole-file digest still applies.
        assert_eq!(
            report["version"]["digest"],
            json!(workspace_fs::sha256_hex(b"alpha\nbravo\ncharlie\n"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_with_limit_reads_a_line_anchored_range() {
        let dir = temp_dir("read_file_line_limit");
        let path = write_three_line_file(&dir);

        let result = read_file_with_line(&path, "lines.txt", None, 2, Some(5)).unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(report["content"], json!("bravo"));
        assert_eq!(report["line"], json!(2));
        assert_eq!(report["offset"], json!(6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_absent_omits_the_line_field() {
        let dir = temp_dir("read_file_line_absent");
        let path = write_three_line_file(&dir);

        let result = read_file(&path, "lines.txt").unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();
        assert!(report.get("line").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_and_offset_are_mutually_exclusive() {
        let error = run(&json!({ "path": "a.txt", "line": 2, "offset": 4 })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("line"), "got: {}", error.message);
        assert!(error.message.contains("offset"), "got: {}", error.message);
    }

    #[test]
    fn line_zero_is_invalid() {
        let error = run(&json!({ "path": "a.txt", "line": 0 })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(
            error.message.contains("1-based") || error.message.contains("line 1"),
            "got: {}",
            error.message
        );
    }

    #[test]
    fn line_past_eof_refuses_with_the_line_count() {
        let dir = temp_dir("read_file_line_past_eof");
        let path = write_three_line_file(&dir);

        let error = read_file_with_line(&path, "lines.txt", None, 9, None).unwrap_err();
        assert_eq!(error.code, -32602);
        // A caller cannot fix "line 9" without knowing how many lines exist.
        assert!(error.message.contains('3'), "got: {}", error.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line-anchored read is a partial read, so it must still carry a
    /// correct whole-file token even when the body is truncated.
    #[test]
    fn line_read_truncated_still_returns_a_usable_token() {
        let dir = temp_dir("read_file_line_truncated");
        let path = dir.join("big.txt");
        let payload = format!("first\n{}\n", "x".repeat(MAX_READ_BYTES + 4096));
        std::fs::write(&path, &payload).unwrap();

        let result = read_file_with_line(&path, "big.txt", None, 2, None).unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(report["line"], json!(2));
        assert_eq!(report["offset"], json!(6));
        assert_eq!(report["truncated_at"], json!(payload.len() as u64));
        assert_eq!(
            report["version"]["digest"],
            json!(workspace_fs::sha256_hex(payload.as_bytes()))
        );
        assert!(report["content"].as_str().unwrap().len() <= MAX_READ_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_one_reads_the_whole_file() {
        let dir = temp_dir("read_file_line_one");
        let path = write_three_line_file(&dir);

        let result = read_file_with_line(&path, "lines.txt", None, 1, None).unwrap();
        let report: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(report["content"], json!("alpha\nbravo\ncharlie\n"));
        assert_eq!(report["offset"], json!(0));
        assert_eq!(report["line"], json!(1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
