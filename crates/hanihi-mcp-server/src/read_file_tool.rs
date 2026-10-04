//! `read_file` tool for Hānihi: reads a workspace file and returns its
//! content plus the version `apply_patch` expects as `base_token`.
//!
//! The `version` object is always present, even when `content` is truncated:
//! its digest is the SHA-256 of the *whole* file, so it is a valid
//! `base_token` regardless. `truncated_at` is `Some(total_len)` whenever
//! `content` is only a prefix of the file.

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

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "read_file",
    "description": "Reads the file at the given workspace-relative path and returns its content plus a `version` object whose digest is the SHA-256 of the whole file — a valid `base_token` even when `content` is truncated. Use `offset` and `limit` (byte offsets) to read a large file in ranges without relying on truncation.",
    "inputSchema": {
    "type": "object",
    "properties": {
    "path": {
        "type": "string",
        "description": "Path of the file to read, relative to the workspace root."
    },
    "offset": {
        "type": "integer",
        "description": "Byte offset at which to start the returned content. Defaults to 0."
    },
    "limit": {
        "type": "integer",
        "description": "Maximum bytes of content to return. Defaults to 131072."
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
    eprintln!("{}:{}: exec", file!(), line!());
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

    let root = workspace_fs::workspace_root()?;
    let resolved =
        workspace_fs::resolve_workspace_path(&root, path).map_err(workspace_fs::invalid)?;
    let ledger = VersionLedger::for_root(&root).map_err(workspace_fs::internal)?;

    read_file_with(&resolved, path, Some(&ledger), offset, limit)
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

/// Test-facing wrapper: a default full read without a ledger, so the returned
/// version carries no opaque id.
#[cfg(test)]
fn read_file(path: &Path, display_path: &str) -> Result<String, ToolError> {
    read_file_with(path, display_path, None, 0, MAX_READ_BYTES as u64)
}

fn read_file_with(
    path: &Path,
    display_path: &str,
    ledger: Option<&VersionLedger>,
    offset: u64,
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
    let report = json!({
    "path": display_path,
    "version": version_to_json(&version),
    "token": digest,
    "sha256": digest.clone(),
    "size": total_len,
    "truncated_at": truncated_at,
    "offset": start,
    "content": visible,
    });
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
}
