//! `create_file` tool for Hānihi: creates or replaces a workspace file.

use crate::workspace_fs::{self, ToolError};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
        "name": "create_file",
        "description": "Creates a new file at the given workspace-relative path with the supplied UTF-8 content, or replaces it when overwrite is true. Parent directories are created as needed. Refuses paths that escape the workspace or target protected files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to create, relative to the workspace root."
                },
                "content": {
                    "type": "string",
                    "description": "UTF-8 text content to write to the file."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace the file if it already exists. Defaults to false."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }
    })
}

/// Implements the tool. `params` carries the MCP tool call; its `arguments`
/// object holds the request.
pub(crate) fn exec(params: &Value, id: Value) -> Value {
    eprintln!("{}:{}: exec", file!(), line!(),);
    let result = workspace_fs::arguments(params).and_then(run);
    match result {
        Ok(text) => workspace_fs::success(id, text),
        Err(error) => workspace_fs::failure(id, &error),
    }
}

fn run(arguments: &Value) -> Result<String, ToolError> {
    eprintln!("{}:{}: run", file!(), line!(),);
    let path = workspace_fs::required_non_empty_string(arguments, "path")?;
    let content = workspace_fs::required_string(arguments, "content")?;
    let overwrite = match arguments.get("overwrite") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| workspace_fs::invalid("argument `overwrite` must be a boolean"))?,
        None => false,
    };
    eprintln!(
        "{}:{}: run  path: {path} content: {} chars overwrite {overwrite}",
        file!(),
        line!(),
        content.len()
    );

    let root = workspace_fs::workspace_root()?;
    let resolved =
        workspace_fs::resolve_workspace_path(&root, path).map_err(workspace_fs::invalid)?;

    create_file(&resolved, path, content, overwrite)
}

fn create_file(
    path: &Path,
    display_path: &str,
    content: &str,
    overwrite: bool,
) -> Result<String, ToolError> {
    eprintln!("{}:{}: create_file {path:?} ", file!(), line!(),);
    if path.is_dir() {
        return Err(workspace_fs::invalid(format!(
            "cannot create file: {display_path} is a directory"
        )));
    }

    let existed = path.exists();
    if existed && !overwrite {
        return Err(workspace_fs::invalid(format!(
            "file already exists: {display_path} (pass overwrite=true to replace it)"
        )));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            workspace_fs::internal(format!(
                "cannot create parent directories for {display_path}: {error}"
            ))
        })?;
    }

    let bytes = content.as_bytes();
    fs::write(path, bytes)
        .map_err(|error| workspace_fs::internal(format!("cannot write {display_path}: {error}")))?;

    let report = json!({
        "path": display_path,
        "created": !existed,
        "bytes_written": bytes.len(),
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
        assert_eq!(schema["name"], json!("create_file"));
        assert_eq!(
            schema["inputSchema"]["required"],
            json!(["path", "content"])
        );
    }

    #[test]
    fn run_requires_path() {
        let error = run(&json!({ "content": "hello" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("path"));
    }

    #[test]
    fn run_requires_content() {
        let error = run(&json!({ "path": "src/a.txt" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("content"));
    }

    #[test]
    fn run_rejects_empty_path() {
        let error = run(&json!({ "path": "", "content": "hello" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty"));
    }

    #[test]
    fn run_rejects_non_boolean_overwrite() {
        let error = run(&json!({
            "path": "src/a.txt",
            "content": "hello",
            "overwrite": "yes"
        }))
        .unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("overwrite"));
    }

    #[test]
    fn creates_a_new_file() {
        let dir = temp_dir("create_file_new");
        let path = dir.join("a.txt");
        let result = create_file(&path, "a.txt", "hello", false).unwrap();
        assert!(result.contains("\"created\": true"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_parent_directories() {
        let dir = temp_dir("create_file_parents");
        let path = dir.join("nested/deep/a.txt");
        let result = create_file(&path, "nested/deep/a.txt", "", false).unwrap();
        assert!(result.contains("\"created\": true"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_an_existing_file_without_overwrite() {
        let dir = temp_dir("create_file_exists");
        let path = dir.join("a.txt");
        std::fs::write(&path, "old").unwrap();
        let error = create_file(&path, "a.txt", "new", false).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("already exists"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_when_requested() {
        let dir = temp_dir("create_file_overwrite");
        let path = dir.join("a.txt");
        std::fs::write(&path, "old").unwrap();
        let result = create_file(&path, "a.txt", "new", true).unwrap();
        assert!(result.contains("\"created\": false"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_directory_path() {
        let dir = temp_dir("create_file_directory");
        let path = dir.join("sub");
        std::fs::create_dir_all(&path).unwrap();
        let error = create_file(&path, "sub", "hello", false).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("is a directory"));
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
