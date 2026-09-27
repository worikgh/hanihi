//! Shared helpers for the workspace file tools.
//!
//! Centralizes workspace-relative path validation and the JSON-RPC response
//! envelopes so `create_file`, `delete_file`, and `rename_file` apply the
//! same safety rules.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// A recoverable tool failure carrying the JSON-RPC error code to report.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

pub(crate) fn invalid(message: impl Into<String>) -> ToolError {
    ToolError {
        code: INVALID_PARAMS,
        message: message.into(),
    }
}

pub(crate) fn internal(message: impl Into<String>) -> ToolError {
    ToolError {
        code: INTERNAL_ERROR,
        message: message.into(),
    }
}

pub(crate) fn success(id: Value, text: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "result": {
        "content": [{ "type": "text", "text": text }],
        "isError": false
    }
    })
}

pub(crate) fn failure(id: Value, code: i64, message: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "error": { "code": code, "message": message }
    })
}

/// Extracts the MCP `tools/call` arguments object from the params.
pub(crate) fn arguments(params: &Value) -> Result<&Value, ToolError> {
    params
        .get("arguments")
        .ok_or_else(|| invalid("missing arguments object"))
}

/// Returns a required string argument. An empty string is still valid, which
/// matters for values such as `create_file` content.
pub(crate) fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing required string argument: {name}")))
}

pub(crate) fn required_non_empty_string<'a>(
    arguments: &'a Value,
    name: &str,
) -> Result<&'a str, ToolError> {
    let value = required_string(arguments, name)?;
    if value.is_empty() {
        return Err(invalid(format!("argument must not be empty: {name}")));
    }
    Ok(value)
}

pub(crate) fn workspace_root() -> Result<PathBuf, ToolError> {
    std::env::current_dir()
        .map_err(|error| internal(format!("cannot read the current directory: {error}")))
}

/// Resolves a workspace-relative path to an absolute path inside the
/// workspace, refusing absolute paths, `..` traversal, protected paths, and
/// symlinks that escape the workspace.
pub(crate) fn resolve_workspace_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let (candidate, normalized) = normalize_relative(root, relative)?;
    if is_refused_path(&normalized) {
        return Err(format!("refusing to modify protected path: {relative}"));
    }

    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;

    // Canonicalize the nearest existing ancestor so symlinks pointing outside
    // the workspace are detected even when the final path does not exist yet.
    let mut existing = candidate.as_path();
    while !existing.is_dir() {
        existing = existing
            .parent()
            .ok_or_else(|| format!("path has no existing ancestor: {relative}"))?;
    }
    eprintln!(
        "{}:{}: resolve_workspace_path: existing: {existing:?}",
        file!(),
        line!(),
    );
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
    eprintln!(
        "{}:{}: resolve_workspace_path resolved: {resolved:?}",
        file!(),
        line!(),
    );
    Ok(resolved)
}

/// Lexically normalizes a workspace-relative path into an absolute candidate
/// and a normalized relative path, without touching the filesystem.
pub(crate) fn normalize_relative(
    root: &Path,
    relative: &str,
) -> Result<(PathBuf, PathBuf), String> {
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
    eprintln!(
        "{}:{}: normalize_relative root: {root:?} relative: {relative} candidate: {candidate:?} normalised: {normalized:?}",
        file!(),
        line!(),
    );
    Ok((candidate, normalized))
}

/// Paths that must never be modified, mirroring the repository protection
/// rules used by `apply_patch`.
pub(crate) fn is_refused_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == ".ignore" || name.starts_with(".git")
    })
}

/// SHA-256 of `bytes` as a lowercase hex string. Shared by `read_file` and
/// `apply_patch` so the digest a read returns is byte-identical to the value
/// the patch tool verifies.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Normalizes a caller-supplied 64-hex digest: trims whitespace and
/// lowercases. Returns `None` when the value is not exactly 64 hex digits.
pub(crate) fn normalize_hash(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn normalize_hash_accepts_hex_case_insensitively() {
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(normalize_hash(expected).as_deref(), Some(expected));
        assert_eq!(
            normalize_hash(&expected.to_ascii_uppercase()).as_deref(),
            Some(expected)
        );
        assert_eq!(normalize_hash("abc"), None);
        assert_eq!(normalize_hash(""), None);
    }

    #[test]
    fn arguments_extracts_the_arguments_object() {
        let params = json!({ "name": "create_file", "arguments": { "path": "a.txt" } });
        assert_eq!(arguments(&params).unwrap(), &json!({ "path": "a.txt" }));

        let error = arguments(&json!({ "name": "create_file" })).unwrap_err();
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[test]
    fn required_string_accepts_an_empty_string() {
        let value = json!({ "content": "" });
        assert_eq!(required_string(&value, "content").unwrap(), "");
    }

    #[test]
    fn required_string_rejects_missing_and_wrong_types() {
        let error = required_string(&json!({ "content": 42 }), "content").unwrap_err();
        assert_eq!(error.code, INVALID_PARAMS);

        let error = required_string(&json!({}), "content").unwrap_err();
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[test]
    fn required_non_empty_string_rejects_empty() {
        let error = required_non_empty_string(&json!({ "path": "" }), "path").unwrap_err();
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.contains("empty"));
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
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// Creates a unique, empty temporary directory for a test.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hanihi-mcp-{name}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create temporary directory");
        path
    }
}
