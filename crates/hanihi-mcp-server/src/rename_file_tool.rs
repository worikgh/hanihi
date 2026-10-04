//! `rename_file` tool for Hānihi: renames or moves a workspace file.

use crate::workspace_fs::{self, ToolError};
use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
        "name": "rename_file",
        "description": "Renames or moves a file from one workspace-relative path to another. Parent directories of the destination are created as needed. Refuses to overwrite an existing destination and refuses paths that escape the workspace or target protected files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Current path of the file, relative to the workspace root."
                },
                "destination": {
                    "type": "string",
                    "description": "New path for the file, relative to the workspace root."
                }
            },
            "required": ["source", "destination"],
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
    let source = workspace_fs::required_non_empty_string(arguments, "source")?;
    let destination = workspace_fs::required_non_empty_string(arguments, "destination")?;

    let root = workspace_fs::workspace_root()?;
    let resolved_source =
        workspace_fs::resolve_workspace_path(&root, source).map_err(workspace_fs::invalid)?;
    let resolved_destination =
        workspace_fs::resolve_workspace_path(&root, destination).map_err(workspace_fs::invalid)?;

    rename_file(&resolved_source, source, &resolved_destination, destination)
}

fn rename_file(
    source: &Path,
    source_display: &str,
    destination: &Path,
    destination_display: &str,
) -> Result<String, ToolError> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(workspace_fs::invalid(format!(
                "source file does not exist: {source_display}"
            )));
        }
        Err(error) => {
            return Err(workspace_fs::internal(format!(
                "cannot inspect {source_display}: {error}"
            )));
        }
    };

    if metadata.is_dir() {
        return Err(workspace_fs::invalid(format!(
            "cannot rename: {source_display} is a directory"
        )));
    }

    if source == destination {
        return Err(workspace_fs::invalid(
            "source and destination are the same path",
        ));
    }

    if destination.exists() {
        return Err(workspace_fs::invalid(format!(
            "destination already exists: {destination_display}"
        )));
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            workspace_fs::internal(format!(
                "cannot create destination directories for {destination_display}: {error}"
            ))
        })?;
    }

    fs::rename(source, destination).map_err(|error| {
        workspace_fs::internal(format!(
            "cannot rename {source_display} to {destination_display}: {error}"
        ))
    })?;

    let report = json!({
        "source": source_display,
        "destination": destination_display,
        "renamed": true,
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
        assert_eq!(schema["name"], json!("rename_file"));
        assert_eq!(
            schema["inputSchema"]["required"],
            json!(["source", "destination"])
        );
    }

    #[test]
    fn run_requires_source_and_destination() {
        let error = run(&json!({ "destination": "b.txt" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("source"));

        let error = run(&json!({ "source": "a.txt" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("destination"));
    }

    #[test]
    fn run_rejects_empty_paths() {
        let error = run(&json!({ "source": "", "destination": "b.txt" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty"));

        let error = run(&json!({ "source": "a.txt", "destination": "" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("empty"));
    }

    #[test]
    fn renames_a_file() {
        let dir = temp_dir("rename_file_ok");
        let source = dir.join("a.txt");
        let destination = dir.join("b.txt");
        std::fs::write(&source, "move me").unwrap();
        let result = rename_file(&source, "a.txt", &destination, "b.txt").unwrap();
        assert!(result.contains("\"renamed\": true"));
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "move me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_destination_parent_directories() {
        let dir = temp_dir("rename_file_parents");
        let source = dir.join("a.txt");
        let destination = dir.join("nested/deep/b.txt");
        std::fs::write(&source, "move me").unwrap();
        rename_file(&source, "a.txt", &destination, "nested/deep/b.txt").unwrap();
        assert!(destination.exists());
        assert!(!source.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_source_is_an_error() {
        let dir = temp_dir("rename_file_missing_source");
        let error =
            rename_file(&dir.join("a.txt"), "a.txt", &dir.join("b.txt"), "b.txt").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("does not exist"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_an_existing_destination() {
        let dir = temp_dir("rename_file_dest_exists");
        let source = dir.join("a.txt");
        let destination = dir.join("b.txt");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&destination, "dest").unwrap();
        let error = rename_file(&source, "a.txt", &destination, "b.txt").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("already exists"));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "dest");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_same_source_and_destination() {
        let dir = temp_dir("rename_file_same");
        let path = dir.join("a.txt");
        std::fs::write(&path, "same").unwrap();
        let error = rename_file(&path, "a.txt", &path, "a.txt").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("same path"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_directory_source() {
        let dir = temp_dir("rename_file_dir");
        let source = dir.join("sub");
        std::fs::create_dir_all(&source).unwrap();
        let error = rename_file(&source, "sub", &dir.join("b.txt"), "b.txt").unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
