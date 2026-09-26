//! `search_text` tool for Hānihi.
//!
//! Searches repository files with a regex or literal pattern, restricted by
//! file globs, and returns each matching line together with a small window
//! of surrounding context.

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
        "description": "Directory to search, relative to the repository root. Defaults to the repository root."
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
    eprintln!("{}:{}: exec", file!(), line!());
    let arguments = params.get("arguments").unwrap_or(params);
    eprintln!("{}:{}: exec", file!(), line!());
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
    eprintln!("{}:{}: run pattern:{pattern}", file!(), line!());

    let literal = arguments
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let matcher = Matcher::compile(pattern, literal)?;

    let globs = Globs::from_arguments(arguments)?;
    let context = bounded_unsigned(arguments, "context", DEFAULT_CONTEXT, MAX_CONTEXT)?;
    let max_matches = bounded_unsigned(arguments, "max_matches", DEFAULT_MAX_MATCHES, MAX_MATCHES)?;
    eprintln!("{}:{}: run arguments:{arguments:?}", file!(), line!());
    let root = resolve_root(arguments)?;
    eprintln!("{}:{}: run ", file!(), line!());

    let mut output = SearchOutput::new();
    eprintln!("{}:{}: run ", file!(), line!());
    walk(
        &root,
        &root,
        &globs,
        &matcher,
        context,
        max_matches,
        &mut output,
    )
    .map_err(|error| internal(format!("search failed: {error}")))?;

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

fn resolve_root(arguments: &Value) -> Result<PathBuf, ToolError> {
    eprintln!("{}:{}: resolve_root ", file!(), line!());
    let base = std::env::current_dir()
        .map_err(|error| internal(format!("cannot read current directory: {error}")))?;

    match arguments.get("path").and_then(Value::as_str) {
        None | Some("") => Ok(base),
        Some(relative) => {
            eprintln!(
                "{}:{}: resolve_root base: {base:?} relative: {relative}",
                file!(),
                line!()
            );
            let root = base.join(relative);
            if root.is_dir() {
                eprintln!("{}:{}: resolve_root Ok: {root:?}", file!(), line!());
                Ok(root)
            } else {
                eprintln!("{}:{}: resolve_root Error: {root:?}", file!(), line!());
                Err(invalid(format!(
                    "search path does not exist or is not a directory: {relative}"
                )))
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

fn walk(
    root: &Path,
    current: &Path,
    globs: &Globs,
    matcher: &Matcher,
    context: usize,
    max_matches: usize,
    output: &mut SearchOutput,
) -> Result<(), std::io::Error> {
    eprintln!("{}:{}: walk {root:?} ", file!(), line!(),);
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
}
