//! `find_symbol` tool for Hānihi.
//!
//! Finds Rust symbol definitions and references across the workspace. It uses
//! a lightweight lexical scanner: comments and string literals are masked,
//! then definition keywords (`fn`, `struct`, `enum`, `trait`, `mod`,
//! `macro_rules!`, and `macro`) introduce definitions and every remaining
//! identifier occurrence counts as a reference.
//!
//! This is a text-based approximation; a future version should delegate to
//! rust-analyzer for scope-accurate results.

use crate::workspace_fs;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Default maximum combined definitions and references reported.
const DEFAULT_MAX_RESULTS: usize = 200;
/// Hard upper bound for reported results, keeping responses bounded.
const MAX_RESULTS: usize = 1_000;
/// Directories that are never descended into.
const SKIPPED_DIRECTORIES: [&str; 2] = [".git", "target"];

/// `tools/list` entry for this tool.
pub(crate) fn json() -> Value {
    json!({
        "name": "find_symbol",
        "description": "Find definitions and references for functions, structs, traits, enums, modules, and macros in Rust source files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "The symbol name to find, for example \"handle_message\" or \"Value\"."
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "function", "struct", "enum", "trait", "module", "macro"],
                    "description": "Restrict reported definitions to one symbol kind. References are reported regardless of kind.",
                    "default": "any"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search, relative to the workspace root. Defaults to the workspace root."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum combined definitions and references to report.",
                    "default": DEFAULT_MAX_RESULTS,
                    "minimum": 1,
                    "maximum": MAX_RESULTS
                }
            },
            "required": ["symbol"],
            "additionalProperties": false
        }
    })
}

/// Implements the tool. `params` carries the MCP tool call; its `arguments`
/// object holds the search options.
pub(crate) fn exec(params: &Value, id: Value) -> Value {
    eprintln!("{}:{}: exec", file!(), line!());
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
    let symbol = required_identifier(arguments, "symbol")?;
    let kind_filter = optional_kind(arguments)?;
    let max_results = bounded_unsigned(arguments, "max_results", DEFAULT_MAX_RESULTS, MAX_RESULTS)?;
    let root = resolve_root(arguments)?;

    let escaped = regex::escape(symbol);
    let identifier_pattern = format!(r"\b{escaped}\b");
    let identifier_regex = Regex::new(&identifier_pattern)
        .map_err(|error| internal(format!("failed to compile identifier pattern: {error}")))?;
    let patterns = DefinitionPatterns::compile();

    let mut report = SymbolReport::new(symbol);
    walk(
        &root,
        &root,
        &identifier_regex,
        &patterns,
        kind_filter,
        max_results,
        &mut report,
    )
    .map_err(|error| internal(format!("find_symbol failed: {error}")))?;

    serde_json::to_string_pretty(&report.to_value())
        .map_err(|error| internal(format!("failed to serialize results: {error}")))
}

fn required_identifier<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    let value = arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing required string argument: {name}")))?;

    if is_identifier(value) {
        Ok(value)
    } else {
        Err(invalid(format!(
            "argument `{name}` must be a Rust identifier, got {value:?}"
        )))
    }
}

/// True when `value` looks like a Rust identifier: an alphabetic character or
/// underscore followed by alphanumeric characters or underscores.
fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_alphanumeric())
}

fn optional_kind(arguments: &Value) -> Result<Option<SymbolKind>, ToolError> {
    match arguments.get("kind").and_then(Value::as_str) {
        None | Some("") | Some("any") => Ok(None),
        Some(value) => match SymbolKind::from_argument(value) {
            Some(kind) => Ok(Some(kind)),
            None => Err(invalid(format!(
                "argument `kind` must be one of any, function, struct, enum, trait, module, macro; got {value:?}"
            ))),
        },
    }
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
    let base = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
    resolve_root_from(&base, arguments)
}

fn resolve_root_from(base: &Path, arguments: &Value) -> Result<PathBuf, ToolError> {
    match arguments.get("path").and_then(Value::as_str) {
        None | Some("") => Ok(base.to_path_buf()),
        // A file is a legitimate scope: scanning one file is a valid request,
        // not an error. Resolution accepts either and `walk` decides how to
        // traverse the result.
        Some(relative) => workspace_fs::resolve_existing_path(base, relative).map_err(invalid),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Module,
    Macro,
}

impl SymbolKind {
    fn all() -> [SymbolKind; 6] {
        [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Trait,
            SymbolKind::Module,
            SymbolKind::Macro,
        ]
    }

    fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Module => "module",
            SymbolKind::Macro => "macro",
        }
    }

    fn from_argument(value: &str) -> Option<Self> {
        match value {
            "function" => Some(SymbolKind::Function),
            "struct" => Some(SymbolKind::Struct),
            "enum" => Some(SymbolKind::Enum),
            "trait" => Some(SymbolKind::Trait),
            "module" => Some(SymbolKind::Module),
            "macro" => Some(SymbolKind::Macro),
            _ => None,
        }
    }

    fn definition_pattern(self) -> &'static [&'static str] {
        match self {
            SymbolKind::Function => &[r"fn\s+(\w+)"],
            SymbolKind::Struct => &[r"struct\s+(\w+)"],
            SymbolKind::Enum => &[r"enum\s+(\w+)"],
            SymbolKind::Trait => &[r"trait\s+(\w+)"],
            SymbolKind::Module => &[r"mod\s+(\w+)"],
            SymbolKind::Macro => &[r"macro_rules!\s*(\w+)", r"macro\s+(\w+)\s*\{"],
        }
    }
}

struct DefinitionPatterns {
    entries: Vec<(SymbolKind, Vec<Regex>)>,
}

impl DefinitionPatterns {
    fn compile() -> Self {
        let entries = SymbolKind::all()
            .into_iter()
            .map(|kind| {
                let regexes = kind
                    .definition_pattern()
                    .iter()
                    .map(|pattern| {
                        // The patterns are literal constants maintained in this
                        // file, so compilation cannot fail.
                        Regex::new(pattern).expect("static definition patterns must compile")
                    })
                    .collect();
                (kind, regexes)
            })
            .collect();
        Self { entries }
    }
}

struct SymbolReport<'a> {
    symbol: &'a str,
    definitions: Vec<Entry>,
    references: Vec<Entry>,
    truncated: bool,
}

impl<'a> SymbolReport<'a> {
    fn new(symbol: &'a str) -> Self {
        Self {
            symbol,
            definitions: Vec::new(),
            references: Vec::new(),
            truncated: false,
        }
    }

    fn total(&self) -> usize {
        self.definitions.len() + self.references.len()
    }

    fn add(&mut self, entry: Entry, is_definition: bool, max_results: usize) {
        if self.total() >= max_results {
            self.truncated = true;
            return;
        }
        if is_definition {
            self.definitions.push(entry);
        } else {
            self.references.push(entry);
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "symbol": self.symbol,
            "definitions": self.definitions.iter().map(Entry::to_value).collect::<Vec<_>>(),
            "references": self.references.iter().map(Entry::to_value).collect::<Vec<_>>(),
            "definition_count": self.definitions.len(),
            "reference_count": self.references.len(),
            "truncated": self.truncated,
        })
    }
}

struct Entry {
    kind: Option<SymbolKind>,
    path: String,
    line: usize,
    column: usize,
    text: String,
}

impl Entry {
    fn to_value(&self) -> Value {
        match self.kind {
            Some(kind) => json!({
                "kind": kind.as_str(),
                "path": self.path,
                "line": self.line,
                "column": self.column,
                "text": self.text,
            }),
            None => json!({
                "path": self.path,
                "line": self.line,
                "column": self.column,
                "text": self.text,
            }),
        }
    }
}

fn walk(
    root: &Path,
    current: &Path,
    identifier_regex: &Regex,
    patterns: &DefinitionPatterns,
    kind_filter: Option<SymbolKind>,
    max_results: usize,
    report: &mut SymbolReport<'_>,
) -> Result<(), std::io::Error> {
    // A scope may name a single file rather than a directory. Scanning it
    // directly avoids `read_dir`, which fails with ENOTDIR on a file. No
    // extension filter is applied here: an explicitly named file is scanned
    // as given, matching the caller's stated intent.
    if current.is_file() {
        let relative = current
            .strip_prefix(root)
            .unwrap_or(current)
            .to_string_lossy()
            .replace('\\', "/");
        scan_file(
            current,
            &relative,
            identifier_regex,
            patterns,
            kind_filter,
            max_results,
            report,
        )?;
        return Ok(());
    }

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
            walk(
                root,
                &path,
                identifier_regex,
                patterns,
                kind_filter,
                max_results,
                report,
            )?;
        } else if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("rs")
        {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            scan_file(
                &path,
                &relative,
                identifier_regex,
                patterns,
                kind_filter,
                max_results,
                report,
            )?;
        }

        if report.truncated {
            break;
        }
    }

    Ok(())
}

fn scan_file(
    path: &Path,
    relative: &str,
    identifier_regex: &Regex,
    patterns: &DefinitionPatterns,
    kind_filter: Option<SymbolKind>,
    max_results: usize,
    report: &mut SymbolReport<'_>,
) -> Result<(), std::io::Error> {
    let source = fs::read_to_string(path)?;
    analyze_source(
        &source,
        relative,
        identifier_regex,
        patterns,
        kind_filter,
        max_results,
        report,
    );
    Ok(())
}

/// Pure analysis over one source file's text; separated from file I/O so it
/// can be unit tested with inline Rust snippets.
fn analyze_source(
    source: &str,
    path: &str,
    identifier_regex: &Regex,
    patterns: &DefinitionPatterns,
    kind_filter: Option<SymbolKind>,
    max_results: usize,
    report: &mut SymbolReport<'_>,
) {
    let masked = mask_non_code(source);
    let line_starts = line_starts(&masked);
    let lines: Vec<&str> = source.lines().collect();

    let mut definition_positions: HashSet<(usize, usize)> = HashSet::new();

    for (kind, regexes) in &patterns.entries {
        for regex in regexes {
            for captures in regex.captures_iter(&masked) {
                let name = captures.get(1).expect("definition patterns capture a name");
                if name.as_str() != report.symbol {
                    continue;
                }

                let offset = name.start();
                let (line, column) = line_column(&line_starts, &masked, offset);
                definition_positions.insert((line, column));

                if kind_filter.is_none_or(|filter| filter == *kind) {
                    let entry = Entry {
                        kind: Some(*kind),
                        path: path.to_string(),
                        line,
                        column,
                        text: line_text(&lines, line),
                    };
                    report.add(entry, true, max_results);
                    if report.truncated {
                        return;
                    }
                }
            }
        }
    }

    for matched in identifier_regex.find_iter(&masked) {
        let (line, column) = line_column(&line_starts, &masked, matched.start());
        if definition_positions.contains(&(line, column)) {
            continue;
        }

        let entry = Entry {
            kind: None,
            path: path.to_string(),
            line,
            column,
            text: line_text(&lines, line),
        };
        report.add(entry, false, max_results);
        if report.truncated {
            return;
        }
    }
}

/// Returns the byte offsets of the start of each line (the offset immediately
/// after each `\n`, plus the initial zero).
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// Converts a byte offset (at a character boundary) into 1-based line and
/// column numbers, where the column counts characters.
fn line_column(line_starts: &[usize], text: &str, offset: usize) -> (usize, usize) {
    let line_index = line_starts
        .partition_point(|&start| start <= offset)
        .saturating_sub(1);
    let line_start = line_starts[line_index];
    let column = text[line_start..offset].chars().count() + 1;
    (line_index + 1, column)
}

fn line_text(lines: &[&str], line: usize) -> String {
    lines
        .get(line.saturating_sub(1))
        .unwrap_or(&"")
        .trim()
        .to_string()
}

/// Masks comments and string literals by replacing each of their characters
/// with a space while preserving newlines and every code character. Each input
/// character maps to exactly one output character, so line and character
/// positions line up with the original source.
fn mask_non_code(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '/' if chars.get(i + 1) == Some(&'/') => {
                out.push_str("  ");
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                out.push_str("  ");
                i += 2;
                let mut depth = 1usize;
                while i < chars.len() && depth > 0 {
                    match (chars[i], chars.get(i + 1).copied()) {
                        ('/', Some('*')) => {
                            out.push_str("  ");
                            i += 2;
                            depth += 1;
                        }
                        ('*', Some('/')) => {
                            out.push_str("  ");
                            i += 2;
                            depth -= 1;
                        }
                        (c, _) => {
                            out.push(if c == '\n' { '\n' } else { ' ' });
                            i += 1;
                        }
                    }
                }
            }
            '"' => {
                out.push(' ');
                i += 1;
                let mut escaped = false;
                while i < chars.len() {
                    let c = chars[i];
                    out.push(if c == '\n' { '\n' } else { ' ' });
                    i += 1;
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        break;
                    }
                }
            }
            '\'' => {
                // Distinguish a character literal ('a', '\n', '"') from a
                // lifetime ('a, 'static). A closing quote one character (or one
                // escape) after the opening quote marks a character literal.
                let is_char_literal = matches!(
                    (chars.get(i + 1).copied(), chars.get(i + 2).copied()),
                    (Some('\\'), _) | (Some(_), Some('\''))
                );

                if is_char_literal {
                    out.push(' ');
                    i += 1;
                    let mut escaped = false;
                    while i < chars.len() {
                        let c = chars[i];
                        out.push(if c == '\n' { '\n' } else { ' ' });
                        i += 1;
                        if escaped {
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '\'' {
                            break;
                        }
                    }
                } else {
                    out.push('\'');
                    i += 1;
                }
            }
            'r' => match raw_string_hashes(&chars, i) {
                Some(hashes) => i = mask_raw_string(&chars, &mut out, hashes, i),
                None => {
                    out.push('r');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }

    out
}

/// Returns the number of `#` between an `r` and its opening quote, or `None`
/// when `chars[start]` does not begin a raw string literal.
fn raw_string_hashes(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut hashes = 0;
    while i < chars.len() && chars[i] == '#' {
        hashes += 1;
        i += 1;
    }
    if i < chars.len() && chars[i] == '"' {
        Some(hashes)
    } else {
        None
    }
}

/// Masks a raw string literal starting at `chars[start] == 'r'` and returns
/// the index just past the closing quote and hash run.
fn mask_raw_string(chars: &[char], out: &mut String, hashes: usize, start: usize) -> usize {
    out.push(' ');
    for _ in 0..hashes {
        out.push(' ');
    }
    out.push(' ');

    let mut i = start + 1 + hashes + 1;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut count = 0;
            let mut j = i + 1;
            while j < chars.len() && count < hashes && chars[j] == '#' {
                count += 1;
                j += 1;
            }
            if count == hashes {
                out.push(' ');
                for _ in 0..hashes {
                    out.push(' ');
                }
                return j;
            }
        }
        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
        i += 1;
    }

    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::test_support::temp_dir;

    #[test]
    fn is_identifier_accepts_rust_identifiers() {
        assert!(is_identifier("Foo"));
        assert!(is_identifier("_private"));
        assert!(is_identifier("type_1"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("1abc"));
        assert!(!is_identifier("has space"));
    }

    #[test]
    fn required_identifier_reports_missing_and_invalid_values() {
        let missing = required_identifier(&json!({}), "symbol").unwrap_err();
        assert_eq!(missing.code, -32602);
        assert!(missing.message.contains("missing required string argument"));

        let invalid = required_identifier(&json!({ "symbol": "1abc" }), "symbol").unwrap_err();
        assert_eq!(invalid.code, -32602);
        assert!(invalid.message.contains("must be a Rust identifier"));
    }

    #[test]
    fn optional_kind_parses_each_value() {
        assert_eq!(optional_kind(&json!({})).unwrap(), None);
        assert_eq!(optional_kind(&json!({ "kind": "any" })).unwrap(), None);
        assert_eq!(
            optional_kind(&json!({ "kind": "function" })).unwrap(),
            Some(SymbolKind::Function)
        );

        let error = optional_kind(&json!({ "kind": "field" })).unwrap_err();
        assert_eq!(error.code, -32602);
    }

    #[test]
    fn line_column_reports_one_based_positions() {
        let text = "abc\ndef";
        let starts = line_starts(text);
        assert_eq!(line_column(&starts, text, 0), (1, 1));
        assert_eq!(line_column(&starts, text, 4), (2, 1));
        assert_eq!(line_column(&starts, text, 5), (2, 2));
    }

    #[test]
    fn mask_non_code_hides_comments_and_strings() {
        let source = "fn main() { // fn phantom\n let s = \"struct Ghost\"; /* enum G */\n}";
        let masked = mask_non_code(source);
        assert!(masked.contains("fn main"));
        assert!(!masked.contains("phantom"));
        assert!(!masked.contains("Ghost"));
        assert!(!masked.contains("enum G"));
    }

    #[test]
    fn mask_non_code_handles_nested_block_comments() {
        let source = "/* outer /* inner */ still */ fn kept()";
        let masked = mask_non_code(source);
        assert!(masked.contains("fn kept"));
        assert!(!masked.contains("outer"));
        assert!(!masked.contains("inner"));
        assert!(!masked.contains("still"));
    }

    #[test]
    fn mask_non_code_handles_raw_strings() {
        let source = "let s = r#\"fn ghost\"#;\nlet t = r\"struct G\";\nfn kept() {}";
        let masked = mask_non_code(source);
        assert!(masked.contains("fn kept"));
        assert!(!masked.contains("ghost"));
        assert!(!masked.contains("struct G"));
    }

    #[test]
    fn mask_non_code_does_not_mistake_quote_char_literal_for_string() {
        let source = "let c = '\"';\nfn kept() {}";
        let masked = mask_non_code(source);
        assert!(masked.contains("fn kept"));
    }

    #[test]
    fn analyze_source_finds_definition_and_references() {
        let symbol = "helper";
        let identifier_regex = Regex::new(r"\bhelper\b").unwrap();
        let patterns = DefinitionPatterns::compile();
        let mut report = SymbolReport::new(symbol);

        analyze_source(
            "fn helper() {}\nfn other() { helper(); helper(); }",
            "src/lib.rs",
            &identifier_regex,
            &patterns,
            None,
            100,
            &mut report,
        );

        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].kind, Some(SymbolKind::Function));
        assert_eq!(report.definitions[0].line, 1);
        assert_eq!(report.references.len(), 2);
        assert!(!report.truncated);
    }

    #[test]
    fn analyze_source_respects_kind_filter_but_keeps_references() {
        let symbol = "Widget";
        let identifier_regex = Regex::new(r"\bWidget\b").unwrap();
        let patterns = DefinitionPatterns::compile();
        let mut report = SymbolReport::new(symbol);

        analyze_source(
            "struct Widget;\nfn use_it(w: Widget) {}",
            "src/lib.rs",
            &identifier_regex,
            &patterns,
            Some(SymbolKind::Function),
            100,
            &mut report,
        );

        assert!(report.definitions.is_empty());
        assert_eq!(report.references.len(), 1);
        assert_eq!(report.references[0].line, 2);
    }

    #[test]
    fn analyze_source_finds_macro_definition() {
        let symbol = "make_it";
        let identifier_regex = Regex::new(r"\bmake_it\b").unwrap();
        let patterns = DefinitionPatterns::compile();
        let mut report = SymbolReport::new(symbol);

        analyze_source(
            "macro_rules! make_it { () => {} }\nfn f() { make_it!(); }",
            "src/lib.rs",
            &identifier_regex,
            &patterns,
            None,
            100,
            &mut report,
        );

        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].kind, Some(SymbolKind::Macro));
        assert_eq!(report.references.len(), 1);
    }

    #[test]
    fn analyze_source_honors_max_results_and_flags_truncation() {
        let symbol = "item";
        let identifier_regex = Regex::new(r"\bitem\b").unwrap();
        let patterns = DefinitionPatterns::compile();
        let mut report = SymbolReport::new(symbol);

        analyze_source(
            "fn item() {}\nitem(); item(); item();",
            "src/lib.rs",
            &identifier_regex,
            &patterns,
            None,
            2,
            &mut report,
        );

        assert_eq!(report.total(), 2);
        assert!(report.truncated);
    }

    #[test]
    fn resolve_root_from_defaults_to_base() {
        let base = temp_dir("find_symbol_root_default");
        assert_eq!(resolve_root_from(&base, &json!({})).unwrap(), base);
        assert_eq!(
            resolve_root_from(&base, &json!({ "path": "" })).unwrap(),
            base
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_root_from_resolves_existing_subdirectory() {
        let base = temp_dir("find_symbol_root_sub");
        let sub = base.join("src");
        std::fs::create_dir_all(&sub).unwrap();

        assert_eq!(
            resolve_root_from(&base, &json!({ "path": "src" })).unwrap(),
            sub.canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_root_from_rejects_bad_paths_as_invalid() {
        let base = temp_dir("find_symbol_root_bad");
        for path in ["missing", "../outside", "/etc"] {
            let error = resolve_root_from(&base, &json!({ "path": path })).unwrap_err();
            assert_eq!(error.code, -32602, "{path}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file path is a legitimate scope, not an error. This is the
    /// regression guard for the reported failure: `find_symbol` on
    /// `crates/hanihi-core/src/source.rs` returned
    /// `-32602: search path does not exist or is not a directory`.
    #[test]
    fn resolve_root_from_accepts_an_existing_file() {
        let base = temp_dir("find_symbol_root_file");
        let file = base.join("source.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        assert_eq!(
            resolve_root_from(&base, &json!({ "path": "source.rs" })).unwrap(),
            file.canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// No extension discrimination: a named file is scanned as given, so a
    /// file without a `.rs` extension still resolves.
    #[test]
    fn resolve_root_from_accepts_a_file_without_a_rust_extension() {
        let base = temp_dir("find_symbol_root_file_no_ext");
        let file = base.join("module.inc");
        std::fs::write(&file, "fn helper() {}\n").unwrap();

        assert_eq!(
            resolve_root_from(&base, &json!({ "path": "module.inc" })).unwrap(),
            file.canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The error text for a non-existent path is unchanged by the file-scope
    /// support, so callers see no difference between the two scope checks.
    #[test]
    fn resolve_root_from_keeps_the_missing_path_message() {
        let base = temp_dir("find_symbol_root_missing_msg");
        let error = resolve_root_from(&base, &json!({ "path": "missing" })).unwrap_err();
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.message,
            "search path does not exist or is not a directory: missing"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// End-to-end through `run`: naming a single file scopes the search to it
    /// and does not reach sibling files in the same directory.
    ///
    /// Driven through the pure `walk` seam rather than `run`: `run` resolves
    /// its scope against the Cargo workspace root, so a temp-dir fixture is not
    /// reachable from it.
    #[test]
    fn walk_scopes_to_a_single_named_file() {
        let base = temp_dir("find_symbol_walk_file_scope");
        let scoped = base.join("a.rs");
        std::fs::write(&scoped, "fn target() {}\n").unwrap();
        std::fs::write(base.join("b.rs"), "fn target() {}\n").unwrap();

        let report = walk_report(&base, &scoped, "target");

        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].path, "a.rs");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A directory scope still recurses, and the existing `.rs` filter on
    /// discovered files is unchanged by the file-scope support.
    #[test]
    fn walk_still_recurses_a_directory_scope() {
        let base = temp_dir("find_symbol_walk_dir_scope");
        std::fs::create_dir_all(base.join("nested")).unwrap();
        std::fs::write(base.join("nested/deep.rs"), "fn target() {}\n").unwrap();
        std::fs::write(base.join("ignored.txt"), "fn target() {}\n").unwrap();

        let report = walk_report(&base, &base, "target");

        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.definitions[0].path, "nested/deep.rs");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Runs `walk` over `current` with `base` as the path relativization root,
    /// the same way `run` invokes it.
    fn walk_report(base: &Path, current: &Path, symbol: &str) -> SymbolReport<'static> {
        let identifier_regex = Regex::new(&format!(r"\b{}\b", regex::escape(symbol))).unwrap();
        let patterns = DefinitionPatterns::compile();
        let mut report = SymbolReport::new(Box::leak(symbol.to_string().into_boxed_str()));
        walk(
            base,
            current,
            &identifier_regex,
            &patterns,
            None,
            DEFAULT_MAX_RESULTS,
            &mut report,
        )
        .expect("walk succeeds");
        report
    }
}
