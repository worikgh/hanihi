//! `search_text` tool for Hānihi.
//!
//! Searches repository files with a regex or literal pattern, restricted by
//! file globs, and returns each matching line together with a small window
//! of surrounding context.

use crate::workspace_fs;
use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Default number of context lines shown on each side of a match.
const DEFAULT_CONTEXT: usize = 2;
/// Default maximum number of matching lines reported.
const DEFAULT_MAX_MATCHES: usize = 200;
/// Upper bound for the context window, keeping responses small.
const MAX_CONTEXT: usize = 50;
/// Upper bound for the number of matches, keeping responses bounded.
const MAX_MATCHES: usize = 1_000;
/// Directories that are never descended into.
const SKIPPED_DIRECTORIES: [&str; 2] = [".git", "target"];

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
    "name": "search_text",
    "description": "Regex or literal search with file globs. Returns matching lines plus a small context window.",
    "inputSchema": {
    "type": "object",
    "properties": {
    "pattern": {
    "type": "string",
    "description": "The text to search for. Treated as a regular expression unless `literal` is true."
    },
    "literal": {
    "type": "boolean",
    "description": "Treat `pattern` as a fixed string instead of a regular expression.",
    "default": false
    },
    "globs": {
    "type": "array",
    "items": { "type": "string" },
    "description": "File globs limiting which files are searched, e.g. \"*.rs\", \"src/**/*.rs\", \"tests/**/*.rs\". Defaults to all files."
    },
    "path": {
    "type": "string",
    "description": "File or directory to search, relative to the workspace root. Defaults to the workspace root. When it names an existing file, only that file is searched."
    },
    "context": {
    "type": "integer",
    "description": "Number of context lines to show before and after each match.",
    "default": DEFAULT_CONTEXT,
    "minimum": 0,
    "maximum": MAX_CONTEXT
    },
    "max_matches": {
    "type": "integer",
    "description": "Maximum number of matching lines to report.",
    "default": DEFAULT_MAX_MATCHES,
    "minimum": 1,
    "maximum": MAX_MATCHES
    }
    },
    "required": ["pattern"],
    "additionalProperties": false
    }
    })
}

/// Implements the tool. `params` carries the MCP tool call; its `arguments`
/// object holds the search options.
pub(crate) fn exec(params: &Value, id: Value) -> Value {
    let arguments = params.get("arguments").unwrap_or(params);
    match run(arguments) {
        Ok(text) => success(id, text),
        Err(error) => failure(id, error.code, error.message),
    }
}

#[derive(Debug)]
struct ToolError {
    code: i64,
    message: String,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError {
        code: -32602,
        message: message.into(),
    }
}

fn internal(message: impl Into<String>) -> ToolError {
    ToolError {
        code: -32603,
        message: message.into(),
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

fn failure(id: Value, code: i64, message: String) -> Value {
    json!({
    "jsonrpc": "2.0",
    "id": id,
    "error": { "code": code, "message": message }
    })
}

fn run(arguments: &Value) -> Result<String, ToolError> {
    let pattern = required_string(arguments, "pattern")?;

    let literal = arguments
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let matcher = Matcher::compile(pattern, literal)?;

    let globs = Globs::from_arguments(arguments)?;
    let context = bounded_unsigned(arguments, "context", DEFAULT_CONTEXT, MAX_CONTEXT)?;
    let max_matches = bounded_unsigned(arguments, "max_matches", DEFAULT_MAX_MATCHES, MAX_MATCHES)?;

    let base = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
    let target = resolve_target(&base, arguments)?;

    let mut output = SearchOutput::new();
    match target {
        SearchTarget::File { path, relative } => {
            search_file(
                &path,
                &relative,
                &matcher,
                context,
                max_matches,
                &mut output,
            )
            .map_err(|error| internal(format!("search failed: {error}")))?;
        }
        SearchTarget::Directory(root) => {
            walk(
                &base,
                &root,
                &globs,
                &matcher,
                context,
                max_matches,
                &mut output,
            )
            .map_err(|error| internal(format!("search failed: {error}")))?;
        }
    }

    serde_json::to_string_pretty(&output.to_value())
        .map_err(|error| internal(format!("failed to serialize results: {error}")))
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("missing required string argument: {name}")))
}

fn bounded_unsigned(
    arguments: &Value,
    name: &str,
    default: usize,
    max: usize,
) -> Result<usize, ToolError> {
    let Some(value) = arguments.get(name) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }

    let number = value
        .as_u64()
        .ok_or_else(|| invalid(format!("argument `{name}` must be a non-negative integer")))?;
    Ok(if number > max as u64 {
        max
    } else {
        number as usize
    })
}

/// Resolved search scope: either a single file or a directory to walk.
#[derive(Debug)]
enum SearchTarget {
    /// An existing file to search directly; `relative` is the caller's path.
    File { path: PathBuf, relative: String },
    /// A directory to walk recursively.
    Directory(PathBuf),
}

/// Resolves `arguments.path` against the workspace root into either a single
/// file or a directory, refusing paths that do not exist or escape the root.
fn resolve_target(base: &Path, arguments: &Value) -> Result<SearchTarget, ToolError> {
    match arguments.get("path").and_then(Value::as_str) {
        None | Some("") => Ok(SearchTarget::Directory(base.to_path_buf())),
        Some(relative) => {
            let resolved = workspace_fs::resolve_existing_path(base, relative).map_err(invalid)?;
            if resolved.is_file() {
                Ok(SearchTarget::File {
                    path: resolved,
                    relative: relative.to_string(),
                })
            } else {
                Ok(SearchTarget::Directory(resolved))
            }
        }
    }
}

#[derive(Debug)]
enum Matcher {
    Literal(String),
    Regex(Regex),
}

impl Matcher {
    fn compile(pattern: &str, literal: bool) -> Result<Self, ToolError> {
        if literal {
            return Ok(Self::Literal(pattern.to_string()));
        }

        Regex::new(pattern)
            .map(Self::Regex)
            .map_err(|error| invalid(format!("invalid regular expression: {error}")))
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Literal(needle) => line.contains(needle.as_str()),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

struct Globs {
    patterns: Vec<Regex>,
}

impl Globs {
    fn from_arguments(arguments: &Value) -> Result<Self, ToolError> {
        let mut globs: Vec<&str> = Vec::new();
        if let Some(array) = arguments.get("globs").and_then(Value::as_array) {
            for item in array {
                if let Some(glob) = item.as_str() {
                    globs.push(glob);
                }
            }
        }

        let mut patterns = Vec::with_capacity(globs.len());
        for glob in globs {
            let regex = Regex::new(&glob_to_regex(glob))
                .map_err(|error| invalid(format!("invalid glob {glob:?}: {error}")))?;
            patterns.push(regex);
        }

        Ok(Self { patterns })
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn matches(&self, path: &str) -> bool {
        self.patterns.iter().any(|pattern| pattern.is_match(path))
    }
}

/// Translates a glob into an equivalent regular expression.
///
/// Supported syntax: `*` matches within a path segment, `**` matches across
/// segments, and `?` matches a single character within a segment. A leading
/// `**/` also matches zero path segments so that `src/**/*.rs` matches
/// `src/main.rs` as well as `src/a/b.rs`. Other characters are matched
/// literally.
fn glob_to_regex(glob: &str) -> String {
    let mut out = String::from("^");
    let chars: Vec<char> = glob.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                let double = i + 1 < chars.len() && chars[i + 1] == '*';
                if double {
                    i += 1;
                    if i + 1 < chars.len() && chars[i + 1] == '/' {
                        out.push_str("(?:.*/)?");
                        i += 1;
                    } else {
                        out.push_str(".*");
                    }
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(chars[i]);
            }
            other => out.push(other),
        }
        i += 1;
    }

    out.push('$');
    out
}

struct SearchOutput {
    matches: Vec<Match>,
    truncated: bool,
}

impl SearchOutput {
    fn new() -> Self {
        Self {
            matches: Vec::new(),
            truncated: false,
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "matches": self.matches.iter().map(|m| m.to_value()).collect::<Vec<_>>(),
            "match_count": self.matches.len(),
            "truncated": self.truncated,
        })
    }
}

struct Match {
    path: String,
    line_number: usize,
    line: String,
    before: Vec<ContextLine>,
    after: Vec<ContextLine>,
}

impl Match {
    fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "line_number": self.line_number,
            "line": self.line,
            "before": self.before.iter().map(|line| line.to_value()).collect::<Vec<_>>(),
            "after": self.after.iter().map(|line| line.to_value()).collect::<Vec<_>>(),
        })
    }
}

struct ContextLine {
    line_number: usize,
    line: String,
}

impl ContextLine {
    fn to_value(&self) -> Value {
        json!({
            "line_number": self.line_number,
            "line": self.line,
        })
    }
}

/// Recursively searches `current`, reporting every match relative to `root`.
///
/// `root` and `current` are deliberately separate: `current` is the caller's
/// search *scope* (which may be a subdirectory named by `path`), while `root`
/// is the workspace root that matched paths and globs are expressed against.
/// Relativizing to the scope instead would return paths the caller cannot act
/// on, since every other workspace tool reports root-relative paths.
fn walk(
    root: &Path,
    current: &Path,
    globs: &Globs,
    matcher: &Matcher,
    context: usize,
    max_matches: usize,
    output: &mut SearchOutput,
) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, globs, matcher, context, max_matches, output)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !globs.is_empty() && !globs.matches(&relative) {
                continue;
            }
            search_file(&path, &relative, matcher, context, max_matches, output)?;
        }

        if output.matches.len() >= max_matches {
            output.truncated = true;
            break;
        }
    }

    Ok(())
}

fn search_file(
    path: &Path,
    relative: &str,
    matcher: &Matcher,
    context: usize,
    max_matches: usize,
    output: &mut SearchOutput,
) -> Result<(), std::io::Error> {
    // Search text files only: skip binary or otherwise unreadable files the
    // way grep skips binary input, instead of failing the whole search.
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(());
    };
    let lines: Vec<&str> = contents.lines().collect();

    for (index, line) in lines.iter().enumerate() {
        if !matcher.is_match(line) {
            continue;
        }

        output.matches.push(Match {
            path: relative.to_string(),
            line_number: index + 1,
            line: (*line).to_string(),
            before: context_lines(&lines, index, context, true),
            after: context_lines(&lines, index, context, false),
        });

        if output.matches.len() >= max_matches {
            break;
        }
    }

    Ok(())
}

fn context_lines(lines: &[&str], index: usize, count: usize, before: bool) -> Vec<ContextLine> {
    let start = if before {
        index.saturating_sub(count)
    } else {
        index.saturating_add(1)
    };
    let end = if before {
        index
    } else {
        (index + 1 + count).min(lines.len())
    };

    (start..end)
        .map(|i| ContextLine {
            line_number: i + 1,
            line: lines[i].to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::test_support::temp_dir;

    #[test]
    fn literal_search_is_exact() {
        let matcher = Matcher::compile("fn.*", true).unwrap();
        assert!(matcher.is_match("fn.* is literal"));
        assert!(!matcher.is_match("fn main()"));
    }

    #[test]
    fn regex_search_matches_pattern() {
        let matcher = Matcher::compile(r"fn\s+\w+", false).unwrap();
        assert!(matcher.is_match("fn main()"));
        assert!(!matcher.is_match("fn()"));
    }

    #[test]
    fn invalid_regex_is_reported() {
        let error = Matcher::compile("(unclosed", false).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("invalid regular expression"));
    }

    #[test]
    fn globs_match_single_segment_files() {
        let globs = Globs::from_arguments(&json!({ "globs": ["*.rs"] })).unwrap();
        assert!(globs.matches("main.rs"));
        assert!(!globs.matches("src/main.rs"));
    }

    #[test]
    fn globs_match_recursive_paths() {
        let globs = Globs::from_arguments(&json!({ "globs": ["src/**/*.rs"] })).unwrap();
        assert!(globs.matches("src/main.rs"));
        assert!(globs.matches("src/a/b.rs"));
        assert!(!globs.matches("lib.rs"));
    }

    #[test]
    fn empty_globs_match_any_file() {
        let globs = Globs::from_arguments(&json!({})).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn context_lines_capture_surrounding_window() {
        let lines: Vec<&str> = "a\nb\nc\nd\ne".lines().collect();

        let before = context_lines(&lines, 2, 2, true);
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].line, "a");
        assert_eq!(before[1].line, "b");

        let after = context_lines(&lines, 2, 2, false);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].line, "d");
        assert_eq!(after[1].line, "e");
    }

    #[test]
    fn context_lines_clamp_at_boundaries() {
        let lines: Vec<&str> = "a\nb\nc".lines().collect();

        assert!(context_lines(&lines, 0, 5, true).is_empty());
        assert!(context_lines(&lines, 2, 5, false).is_empty());
    }

    #[test]
    fn glob_to_regex_escapes_regex_metacharacters() {
        let globs = Globs::from_arguments(&json!({ "globs": ["file.rs"] })).unwrap();
        assert!(globs.matches("file.rs"));
        assert!(!globs.matches("fileXrs"));
    }

    #[test]
    fn resolve_target_defaults_to_the_workspace_root() {
        let base = temp_dir("search_target_default");
        match resolve_target(&base, &json!({})).unwrap() {
            SearchTarget::Directory(root) => assert_eq!(root, base),
            SearchTarget::File { .. } => panic!("expected a directory target"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_target_accepts_an_existing_file() {
        let base = temp_dir("search_target_file");
        let file = base.join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();

        match resolve_target(&base, &json!({ "path": "notes.txt" })).unwrap() {
            SearchTarget::File { path, relative } => {
                assert_eq!(path, file.canonicalize().unwrap());
                assert_eq!(relative, "notes.txt");
            }
            SearchTarget::Directory(_) => panic!("expected a file target"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_target_accepts_an_existing_directory() {
        let base = temp_dir("search_target_dir");
        let sub = base.join("src");
        std::fs::create_dir_all(&sub).unwrap();

        match resolve_target(&base, &json!({ "path": "src" })).unwrap() {
            SearchTarget::Directory(root) => assert_eq!(root, sub.canonicalize().unwrap()),
            SearchTarget::File { .. } => panic!("expected a directory target"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_target_rejects_missing_and_escaping_paths() {
        let base = temp_dir("search_target_bad");
        for path in ["missing.txt", "../outside", "/etc/passwd"] {
            let error = resolve_target(&base, &json!({ "path": path })).unwrap_err();
            assert_eq!(error.code, -32602, "{path}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn search_file_reports_only_that_file() {
        let base = temp_dir("search_file_only");
        let file = base.join("notes.txt");
        std::fs::write(&file, "first\nneedle here\nthird\n").unwrap();

        let matcher = Matcher::compile("needle", true).unwrap();
        let mut output = SearchOutput::new();
        search_file(&file, "notes.txt", &matcher, 1, 200, &mut output).unwrap();

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "notes.txt");
        assert_eq!(output.matches[0].line_number, 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Runs `walk` the way `run` does: `base` is the workspace root used to
    /// relativize reported paths, `scope` is the resolved `path` argument.
    fn walk_fixture(base: &Path, scope: &Path, globs: &Globs, pattern: &str) -> SearchOutput {
        let matcher = Matcher::compile(pattern, true).unwrap();
        let mut output = SearchOutput::new();
        walk(base, scope, globs, &matcher, 1, 200, &mut output).unwrap();
        output
    }

    /// A `path` naming a subdirectory scopes the search to it, but reported
    /// paths stay relative to the workspace root so callers can feed them
    /// straight back into `read_file` or `apply_patch`.
    #[test]
    fn walk_reports_paths_relative_to_the_workspace_root() {
        let base = temp_dir("search_walk_root_relative");
        let scope = base.join("crates/core");
        std::fs::create_dir_all(scope.join("src")).unwrap();
        std::fs::write(scope.join("src/lib.rs"), "needle here\n").unwrap();
        std::fs::write(base.join("top.rs"), "needle here\n").unwrap();

        let output = walk_fixture(
            &base,
            &scope,
            &Globs::from_arguments(&json!({})).unwrap(),
            "needle",
        );

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "crates/core/src/lib.rs");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Globs are matched against workspace-root-relative paths, so the
    /// documented `src/**/*.rs` form means the same thing regardless of which
    /// subdirectory `path` scopes the search to.
    #[test]
    fn walk_matches_globs_against_workspace_relative_paths() {
        let base = temp_dir("search_walk_globs");
        let scope = base.join("crates/core");
        std::fs::create_dir_all(scope.join("src")).unwrap();
        std::fs::write(scope.join("src/lib.rs"), "needle here\n").unwrap();
        std::fs::write(scope.join("notes.txt"), "needle here\n").unwrap();

        let globs = Globs::from_arguments(&json!({ "globs": ["crates/**/*.rs"] })).unwrap();
        let output = walk_fixture(&base, &scope, &globs, "needle");

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "crates/core/src/lib.rs");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// With no `path`, the scope is the workspace root itself, so relativizing
    /// against the root still yields plain repo-relative paths.
    #[test]
    fn walk_at_the_workspace_root_keeps_repo_relative_paths() {
        let base = temp_dir("search_walk_at_root");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/lib.rs"), "needle here\n").unwrap();

        let output = walk_fixture(
            &base,
            &base,
            &Globs::from_arguments(&json!({})).unwrap(),
            "needle",
        );

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "src/lib.rs");
        let _ = std::fs::remove_dir_all(&base);
    }
}
