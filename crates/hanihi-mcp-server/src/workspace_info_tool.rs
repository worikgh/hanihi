//! `workspace_info` tool for Hānihi.
//!
//! Reports Cargo workspace layout and Rust toolchain state: workspace root,
//! member crates with their editions, active toolchain, installed target
//! triples, and the important configuration files found at the workspace root.

use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Configuration files that matter for a Cargo workspace, relative to the
/// workspace root.
/// Configuration files that matter for a Cargo workspace, relative to the
/// workspace root.
const CONFIG_FILE_NAMES: [&str; 8] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rust-toolchain",
    ".cargo/config.toml",
    ".cargo/config",
    "rustfmt.toml",
    "clippy.toml",
];

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "workspace_info",
    "description": "Reports the Cargo workspace layout and Rust toolchain state: workspace root, member crates and their editions, active toolchain, installed target triples, and important configuration files.",
    "inputSchema": {
    "type": "object",
    "properties": {},
    "required": [],
    "additionalProperties": false
    }
    })
}

/// Implements the tool. The workspace is discovered from the process working
/// directory, so `params` carries no arguments.
pub(crate) fn exec(_params: &Value, id: Value) -> Value {
    tracing::debug!("workspace_info: exec");
    match build_workspace_info() {
        Ok(info) => {
            let text = serde_json::to_string_pretty(&info).unwrap_or_else(|_| info.to_string());
            success(id, text)
        }
        Err(message) => failure(id, message),
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

fn failure(id: Value, message: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "error": { "code": -32603, "message": message }
    })
}

fn build_workspace_info() -> Result<Value, String> {
    let current_directory = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;

    let metadata = cargo_metadata()?;

    let workspace_root = metadata
        .get("workspace_root")
        .and_then(Value::as_str)
        .ok_or_else(|| "cargo metadata returned no workspace_root".to_string())?;

    let crates = extract_crates(&metadata);
    let edition = summarize_edition(&crates);
    let rustc = rustc_info();
    let host = rustc
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(json!({
    "workspace_root": workspace_root,
    "current_directory": current_directory.to_string_lossy(),
    "crates": crates,
    "edition": edition,
    "active_toolchain": active_toolchain(),
    "rustc": rustc,
    "target_triples": target_triples(&host),
    "config_files": config_files(Path::new(workspace_root)),
    }))
}

/// Runs `cargo metadata --no-deps` so only workspace members are reported,
/// which keeps the result small and avoids resolving dependencies.
fn cargo_metadata() -> Result<Value, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("failed to run cargo metadata: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo metadata failed: {}", stderr.trim()));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed to parse cargo metadata output: {error}"))
}

fn extract_crates(metadata: &Value) -> Vec<Value> {
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .map(|packages| {
            packages
                .iter()
                .map(|package| {
                    json!({
                    "name": package.get("name").cloned(),
                    "version": package.get("version").cloned(),
                    "edition": package.get("edition").cloned(),
                    "manifest_path": package.get("manifest_path").cloned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the workspace edition when every crate agrees, `null` when there
/// are no crates, or a list of the distinct editions in use.
fn summarize_edition(crates: &[Value]) -> Value {
    let editions: Vec<&str> = crates
        .iter()
        .filter_map(|package| package.get("edition").and_then(Value::as_str))
        .collect();

    let mut distinct: Vec<&str> = Vec::new();
    for edition in editions {
        if !distinct.contains(&edition) {
            distinct.push(edition);
        }
    }

    match distinct.as_slice() {
        [] => Value::Null,
        [single] => json!(single),
        multiple => json!(multiple),
    }
}

/// The active rustup toolchain, or `null` when rustup is unavailable.
fn active_toolchain() -> Value {
    match command_output("rustup", &["show", "active-toolchain"]) {
        Some(line) => parse_active_toolchain(&line),
        None => Value::Null,
    }
}

fn parse_active_toolchain(line: &str) -> Value {
    let line = line.trim();
    let (name, reason) = match line.split_once(" (") {
        Some((name, rest)) => (name, rest.strip_suffix(')').unwrap_or(rest)),
        None => (line, ""),
    };

    json!({
    "name": name,
    "reason": reason,
    })
}

/// `rustc -vV` fields, or `null` when rustc is unavailable.
fn rustc_info() -> Value {
    let Some(output) = command_output("rustc", &["-vV"]) else {
        return Value::Null;
    };

    let mut release = None;
    let mut host = None;
    let mut commit_hash = None;
    let mut llvm_version = None;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("release: ") {
            release = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("host: ") {
            host = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("commit-hash: ") {
            commit_hash = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("LLVM version: ") {
            llvm_version = Some(value.trim());
        }
    }

    json!({
    "release": release,
    "host": host,
    "commit_hash": commit_hash,
    "llvm_version": llvm_version,
    })
}

/// Installed rustup targets, falling back to the rustc host when no target
/// list is available.
fn target_triples(host: &str) -> Vec<String> {
    let mut targets: Vec<String> = command_output("rustup", &["target", "list", "--installed"])
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    if !host.is_empty() && !targets.iter().any(|target| target == host) {
        targets.push(host.to_string());
    }

    targets
}

fn config_files(workspace_root: &Path) -> Value {
    let files: Vec<Value> = CONFIG_FILE_NAMES
        .iter()
        .map(|name| {
            let path = workspace_root.join(name);
            let exists = path.is_file();
            let content = if exists && *name != "Cargo.lock" {
                fs::read_to_string(&path).ok()
            } else {
                None
            };

            json!({
            "path": path.to_string_lossy(),
            "exists": exists,
            "content": content,
            })
        })
        .collect();

    Value::Array(files)
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_edition_returns_null_without_crates() {
        assert_eq!(summarize_edition(&[]), Value::Null);
    }

    #[test]
    fn summarize_edition_returns_single_edition() {
        let crates = vec![json!({ "edition": "2021" }), json!({ "edition": "2021" })];
        assert_eq!(summarize_edition(&crates), json!("2021"));
    }

    #[test]
    fn summarize_edition_lists_distinct_editions() {
        let crates = vec![
            json!({ "edition": "2021" }),
            json!({ "edition": "2018" }),
            json!({ "edition": "2021" }),
        ];
        assert_eq!(summarize_edition(&crates), json!(["2021", "2018"]));
    }

    #[test]
    fn parse_active_toolchain_splits_reason() {
        let parsed = parse_active_toolchain("1.85.0-x86_64-unknown-linux-gnu (default)");
        assert_eq!(
            parsed["name"].as_str(),
            Some("1.85.0-x86_64-unknown-linux-gnu")
        );
        assert_eq!(parsed["reason"].as_str(), Some("default"));
    }

    #[test]
    fn parse_active_toolchain_without_reason() {
        let parsed = parse_active_toolchain("nightly-x86_64-unknown-linux-gnu");
        assert_eq!(
            parsed["name"].as_str(),
            Some("nightly-x86_64-unknown-linux-gnu")
        );
        assert_eq!(parsed["reason"].as_str(), Some(""));
    }
}
