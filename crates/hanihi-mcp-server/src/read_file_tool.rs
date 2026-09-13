//! The `read_file` tool, served over MCP. Mirrors
//! `hanihi_core::tool::builtin_read_file`.

use std::path::Path;
use std::sync::Arc;

use hanihi_core::{SourceError, SourceTree};
use rmcp::ErrorData;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Tool};
use rmcp::service::{RequestContext, RoleServer};

pub(crate) fn new() -> Tool {
    Tool::new(
        "mcp_read_file",
        "Read a text file from the git repository. `path` is relative to the repo root. \
         Git-ignored paths cannot be read. Returns up to 64 KiB.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the repo root" }
            },
            "required": ["path"]
        })
        .as_object()
        .expect("static schema is an object")
        .clone(),
    )
}

pub(crate) async fn call(
    request: CallToolRequestParams,
    tree: Arc<SourceTree>,
    _context: RequestContext<RoleServer>,
) -> Result<CallToolResponse, ErrorData> {
    let args = request.arguments.unwrap_or_default();
    let rel = path_arg(&args)?;
    read(tree, rel)
}

/// Caller-visible result for when the server was started outside a git repo.
pub(crate) fn unavailable() -> CallToolResponse {
    tool_error("read_file unavailable: no git repository".to_string())
}

fn path_arg(args: &serde_json::Map<String, serde_json::Value>) -> Result<&str, ErrorData> {
    args.get("path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| ErrorData::invalid_params("missing string field 'path'", None))
}

/// Reads one file. Tool-level failures are `Ok(CallToolResult::error(...))`;
/// infrastructure failures are `Err(ErrorData)`.
fn read(tree: Arc<SourceTree>, rel: &str) -> Result<CallToolResponse, ErrorData> {
    match tree.read(Path::new(rel)) {
        Ok(text) => Ok(CallToolResponse::from(CallToolResult::success(vec![
            ContentBlock::text(text),
        ]))),
        Err(SourceError::NotFound(p)) => Ok(tool_error(format!("no such path: {}", p.display()))),
        Err(SourceError::Ignored(p)) => {
            Ok(tool_error(format!("path is git-ignored: {}", p.display())))
        }
        Err(SourceError::Escape(p)) => Ok(tool_error(format!(
            "path escapes the repository: {}",
            p.display()
        ))),
        Err(e) => Err(ErrorData::internal_error(
            format!("reading {rel}: {e}"),
            None,
        )),
    }
}

fn tool_error(message: String) -> CallToolResponse {
    CallToolResponse::from(CallToolResult::error(vec![ContentBlock::text(message)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// Throwaway git repo with an ignored `target/` dir.
    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("hanihi-mcp-src-{}-{seq}", std::process::id()));
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::create_dir_all(dir.join("target/debug")).unwrap();
            fs::write(dir.join(".gitignore"), "target/\n").unwrap();
            fs::write(dir.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
            fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
            fs::write(dir.join("target/debug/junk.rs"), "junk\n").unwrap();
            Self { dir }
        }

        fn tree(&self) -> Arc<SourceTree> {
            Arc::new(SourceTree::open_at(&self.dir).expect("fixture is a git repo"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap_or(());
        }
    }

    fn response_text(resp: &CallToolResponse) -> String {
        match resp {
            CallToolResponse::Complete(result) => result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("expected Complete response"),
        }
    }

    fn is_error(resp: &CallToolResponse) -> bool {
        matches!(resp, CallToolResponse::Complete(result) if result.is_error == Some(true))
    }

    #[test]
    fn new_reports_read_file_schema() {
        let tool = new();
        assert_eq!(tool.name.as_ref(), "mcp_read_file");
        assert!(
            tool.description
                .as_deref()
                .unwrap_or_default()
                .contains("Read a text file")
        );
        let schema = &*tool.input_schema;
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "path"));
    }

    #[test]
    fn path_arg_extracts_and_rejects() {
        let mut with = serde_json::Map::new();
        with.insert("path".into(), serde_json::json!("src/main.rs"));
        assert_eq!(path_arg(&with).unwrap(), "src/main.rs");

        let empty = serde_json::Map::new();
        assert!(path_arg(&empty).is_err());

        let mut non_string = serde_json::Map::new();
        non_string.insert("path".into(), serde_json::json!(42));
        assert!(path_arg(&non_string).is_err());
    }

    #[test]
    fn read_returns_file_contents() {
        let fx = Fixture::new();
        let tree = fx.tree();
        let resp = read(tree, "src/main.rs").unwrap();
        assert!(!is_error(&resp));
        assert!(response_text(&resp).contains("fn main"));
    }

    #[test]
    fn read_reports_missing() {
        let fx = Fixture::new();
        let tree = fx.tree();
        let resp = read(tree, "nope.rs").unwrap();
        assert!(is_error(&resp));
        assert!(response_text(&resp).contains("no such path:"));
    }

    #[test]
    fn read_rejects_ignored() {
        let fx = Fixture::new();
        let tree = fx.tree();
        let resp = read(tree, "target/debug/junk.rs").unwrap();
        assert!(is_error(&resp));
        assert!(response_text(&resp).contains("git-ignored"));
    }

    #[test]
    fn read_rejects_escape() {
        let fx = Fixture::new();
        let tree = fx.tree();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("hanihi-mcp-outside-{}-{seq}", std::process::id());
        let outside = std::env::temp_dir().join(&name);
        fs::write(&outside, "secret").unwrap();
        let rel = format!("../{name}");
        let resp = read(tree, &rel).unwrap();
        assert!(is_error(&resp));
        assert!(response_text(&resp).contains("escapes"));
        fs::remove_file(&outside).unwrap_or(());
    }

    #[test]
    fn unavailable_reports_no_repo() {
        let resp = unavailable();
        assert!(is_error(&resp));
        assert!(response_text(&resp).contains("no git repository"));
    }
}
