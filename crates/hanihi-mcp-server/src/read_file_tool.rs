//! `read_file` tool for Hānihi: reads a workspace file and returns its
//! content plus the token `apply_patch` expects as `base_token`.
//!
//! Use this token when you have not written the file since reading it; a
//! successful `apply_patch` returns its own token for the repeated-edit case.

use crate::workspace_fs::{self, ToolError};
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "read_file",
    "description": "Reads the file at the given workspace-relative path and returns its content, byte size, and SHA-256. The `token` value is the exact `base_token` to pass to `apply_patch` for this path. Pass this read token only if you have not edited the file since reading it; after your own successful `apply_patch`, use the `token` that tool returned instead of this one.",
    "inputSchema": {
        "type": "object",
        "properties": {
        "path": {
            "type": "string",
            "description": "Path of the file to read, relative to the workspace root."
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
        Err(error) => workspace_fs::failure(id, error.code, error.message),
    }
}

fn run(arguments: &Value) -> Result<String, ToolError> {
    let path = workspace_fs::required_non_empty_string(arguments, "path")?;
    eprintln!("{}:{}: run path: {path}", file!(), line!(),);

    let root = workspace_fs::workspace_root()?;
    let resolved =
        workspace_fs::resolve_workspace_path(&root, path).map_err(workspace_fs::invalid)?;

    eprintln!("{}:{}: run resolved: {resolved:?}", file!(), line!(),);
    read_file(&resolved, path)
}

fn read_file(path: &Path, display_path: &str) -> Result<String, ToolError> {
    eprintln!("{}:{}: read_file path:{path:?}", file!(), line!(),);
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
    eprintln!("{}:{}: read_file", file!(), line!(),);

    // Invariant: hash the raw file bytes, not the decoded String. `apply_patch`
    // verifies against `fs::read` bytes, so hashing the String here would
    // mismatch on BOMs, CRLF, or any non-canonical encoding.
    let sha256 = workspace_fs::sha256_hex(&bytes);
    let size = bytes.len();
    let content = String::from_utf8(bytes)
        .map_err(|_| workspace_fs::internal(format!("file is not valid UTF-8: {display_path}")))?;

    eprintln!("{}:{}: read_file", file!(), line!(),);
    // `token` is the forward-compatible "copy this" slot. At Level 0 it is
    // byte-identical to `sha256`; a Level 1 implementation would replace it
    // with an HMAC while keeping `sha256` as the content hash.
    let report = json!({
    "path": display_path,
    "token": sha256,
    "sha256": sha256,
    "size": size,
    "content": content,
    });
    eprintln!("{}:{}: read_file", file!(), line!(),);
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
}
