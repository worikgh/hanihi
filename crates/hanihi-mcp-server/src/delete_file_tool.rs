//! `delete_file` tool for Hānihi: deletes a workspace file.

use crate::workspace_fs::{self, ToolError};
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
        "name": "delete_file",
        "description": "Deletes the file at the given workspace-relative path. Refuses to delete directories or paths that escape the workspace or target protected files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to delete, relative to the workspace root."
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

    let root = workspace_fs::workspace_root()?;
    let resolved =
        workspace_fs::resolve_workspace_path(&root, path).map_err(workspace_fs::invalid)?;

    delete_file(&resolved, path)
}

fn delete_file(path: &Path, display_path: &str) -> Result<String, ToolError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(workspace_fs::internal(format!(
                "file does not exist: {display_path}"
            )));
        }
        Err(error) => {
            return Err(workspace_fs::internal(format!(
                "cannot inspect {display_path}: {error}"
            )));
        }
    };

    if metadata.is_dir() {
        return Err(workspace_fs::internal(format!(
            "cannot delete: {display_path} is a directory"
        )));
    }

    fs::remove_file(path).map_err(|error| {
        workspace_fs::internal(format!("cannot delete {display_path}: {error}"))
    })?;

    let report = json!({ "path": display_path, "deleted": true });
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
        assert_eq!(schema["name"], json!("delete_file"));
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
    fn deletes_an_existing_file() {
        let dir = temp_dir("delete_file_ok");
        let path = dir.join("a.txt");
        std::fs::write(&path, "bye").unwrap();
        let result = delete_file(&path, "a.txt").unwrap();
        assert!(result.contains("\"deleted\": true"));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let dir = temp_dir("delete_file_missing");
        let error = delete_file(&dir.join("missing.txt"), "missing.txt").unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("does not exist"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_delete_a_directory() {
        let dir = temp_dir("delete_file_dir");
        let path = dir.join("sub");
        std::fs::create_dir_all(&path).unwrap();
        let error = delete_file(&path, "sub").unwrap_err();
        assert_eq!(error.code, -32603);
        assert!(error.message.contains("directory"));
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
