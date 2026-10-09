//! Built-in tools for the agent.
//!
//! Tools are rig [`PortableDynamicTool`]s: name + description + JSON schema +
//! an async callback taking raw `serde_json::Value` arguments.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, Sink, SinkMatch};
use rig::tool::{PortableDynamicTool, ToolExecutionError, ToolOutput};
use serde_json::json;
use tokio::io::AsyncReadExt as _;

use crate::source::{SourceError, SourceTree};

/// Map a [`SourceError`] onto a rig tool error with the right kind.
pub(crate) fn map_source_err(e: SourceError) -> ToolExecutionError {
    match e {
        SourceError::NotFound(p) => {
            ToolExecutionError::not_found(format!("no such path: {}", p.display()))
        }
        SourceError::Ignored(p) => {
            ToolExecutionError::permission_denied(format!("path is git-ignored: {}", p.display()))
        }
        SourceError::Escape(p) => ToolExecutionError::permission_denied(format!(
            "path escapes the repository: {}",
            p.display()
        )),
        other => ToolExecutionError::provider(other.to_string()),
    }
}

/// Maximum bytes of any rendered tool result fed back to the model.
///
/// This is the agent-layer backstop. Per-tool caps (file reads, grep,
/// command output, and the MCP `read_file` per-file cap) are the primary
/// policy; this catches any path that renders an unbounded result, in
/// particular MCP tools. For `read_file` specifically, the MCP server
/// serializes the version fields before the (large) `content` field, so this
/// backstop can only ever clip body text, never the usable token.
pub(crate) const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// Cap a rendered tool result, appending a note with the original byte count
/// when truncated. Used after `ToolOutput::render()` at the agent dispatch so
/// every result (built-in or MCP) shares one truncation format.
pub(crate) fn truncate_tool_output(s: &str) -> String {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        s.to_string()
    } else {
        tracing::debug!(
            bytes = s.len(),
            limit = MAX_TOOL_RESULT_BYTES,
            "truncated tool output"
        );
        let cut = s.floor_char_boundary(MAX_TOOL_RESULT_BYTES);
        let mut out = String::with_capacity(cut + 64);
        out.push_str(&s[..cut]);
        out.push_str(&format!("\n…[truncated, {} bytes total]", s.len()));
        out
    }
}

/// Tool: report the current local date and time.
pub fn builtin_get_time() -> PortableDynamicTool {
    PortableDynamicTool::new(
        "get_time",
        "Get the current local date and time in RFC 3339 format.",
        json!({
            "type": "object",
            "properties": {}
        }),
        |_args: serde_json::Value| {
            Box::pin(async move {
                let now = chrono::Local::now().to_rfc3339();
                Ok(ToolOutput::text(now))
            })
        },
    )
}

/// Tool: list files and directories in the git repository.
pub fn builtin_list_dir(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "list_dir",
        "List files and directories in the git repository. `path` is relative to the repo root \
	 (default: the root itself). `depth` controls recursion (default 1, max 4). \
	 Git-ignored paths are never listed. One line per entry: type + path.",
        json!({
            "type": "object",
            "properties": {
            "path": { "type": "string", "description": "Directory relative to repo root" },
            "depth": { "type": "integer", "description": "Recursion depth (1-4)" }
            }
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let depth = args
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 4) as usize;

                // Distinguish "does not exist" from every other failure, and give the
                // caller the nearest existing ancestor so a wrong path is one step
                // away from correct rather than a dead end.
                let rel_path = Path::new(rel);
                if rel != "." {
                    // `list_dir` is root-scoped: refuse `..` and absolute
                    // components before touching the filesystem, matching
                    // `SourceTree::resolve_for_write`. Without this the
                    // existence probe and the ancestor hint would stat and name
                    // paths outside the repository.
                    if rel_path.components().any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    }) {
                        return Err(ToolExecutionError::permission_denied(format!(
                            "path escapes the repository: {rel}"
                        )));
                    }
                    let abs = tree.root().join(rel_path);
                    if !abs.exists() {
                        return Err(missing_directory_error(tree.root(), rel_path));
                    }
                    if !abs.is_dir() {
                        return Err(ToolExecutionError::invalid_args(format!(
                            "not a directory: {rel}"
                        )));
                    }
                }

                let walk = tree.walk(Path::new(rel), depth).map_err(map_source_err)?;
                let mut lines = Vec::new();
                for entry in walk {
                    let entry = entry.map_err(|e| ToolExecutionError::provider(e.to_string()))?;
                    if entry.depth() == 0 {
                        continue;
                    }
                    let ty = match entry.file_type() {
                        Some(t) if t.is_dir() => "dir ",
                        Some(t) if t.is_file() => "file",
                        Some(t) if t.is_symlink() => "link",
                        _ => "oth ",
                    };
                    let p = entry
                        .path()
                        .strip_prefix(tree.root())
                        .unwrap_or(entry.path());
                    lines.push(format!("{ty} {}", p.display()));
                }
                Ok(ToolOutput::text(lines.join("\n")))
            })
        },
    )
}
/// Builds the `list_dir` error for a path that does not exist, naming the
/// nearest existing ancestor so the caller can correct the path in one step.
fn missing_directory_error(root: &Path, rel: &Path) -> ToolExecutionError {
    let message = match nearest_existing_ancestor(root, rel) {
        Some(ancestor) => format!(
            "no such path: {} (nearest existing ancestor: {})",
            rel.display(),
            ancestor.display()
        ),
        None => format!("no such path: {}", rel.display()),
    };
    ToolExecutionError::not_found(message)
}

/// The longest prefix of `rel` (joined onto `root`) that exists as a
/// directory. Returns `None` when even the root is unusable.
fn nearest_existing_ancestor<'a>(root: &Path, rel: &'a Path) -> Option<&'a Path> {
    let mut candidate = rel;
    loop {
        let parent = candidate.parent()?;
        let joined = root.join(parent);
        if joined.is_dir() {
            return (!parent.as_os_str().is_empty()).then_some(parent);
        }
        candidate = parent;
    }
}

// ── run_command ──────────────────────────────────────────────────

/// Default timeout for `run_command`.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Maximum allowed timeout for `run_command`.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Per-process sequence number for trace filenames. The tool cannot see the
/// session turn number, so unix-ms + a per-process counter disambiguates
/// traces within a session directory.
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of executing a command (spawn, wait, capture, timeout).
struct CommandOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    stdout: String,
    stderr: String,
}

/// Validate a whitespace-split command against the allowlist.
///
/// `argv[0]` must be `cargo` or `git`. Cargo subcommands are limited to
/// `check`, `build`, `test`, `clippy`, `fmt`, `doc`, and `run -p hanihi-eval`.
/// Git is limited to the read-only verbs `status`, `diff`, `log`, `show`, and
/// `apply --check`. Flags that would change the working directory
/// (`--manifest-path`, `-C`/`--directory`) are rejected outright — the cwd is
/// pinned to the repo root and nothing may escape it.
const NON_MUTATING: &[&str] = &[
    "apropos",
    "awk",
    "base64",
    "basename",
    "cal",
    "cat",
    "cmp",
    "column",
    "comm",
    "cut",
    "date",
    "df",
    "diff",
    "dig",
    "dirname",
    "du",
    "echo",
    "env",
    "false",
    "file",
    "find",
    "fmt",
    "fold",
    "free",
    "grep",
    "head",
    "hexdump",
    "host",
    "hostname",
    "htop",
    "id",
    "info",
    "iostat",
    "join",
    "ldd",
    "less",
    "ls",
    "lsof",
    "man",
    "more",
    "mount",
    "netstat",
    "nl",
    "nm",
    "nslookup",
    "objdump",
    "od",
    "paste",
    "ping",
    "pr",
    "pgrep",
    "printenv",
    "printf",
    "ps",
    "pwd",
    "readelf",
    "readlink",
    "realpath",
    "rev",
    "sha256sum",
    "sed",
    "seq",
    "sort",
    "ss",
    "stat",
    "strings",
    "tac",
    "tail",
    "test",
    "top",
    "tracepath",
    "traceroute",
    "tree",
    "tr",
    "true",
    "type",
    "uname",
    "uniq",
    "uptime",
    "users",
    "vmstat",
    "wc",
    "whatis",
    "whereis",
    "which",
    "who",
    "whoami",
    "xxd",
    "zipinfo",
];

/// Which allowlist `check_command_argv` applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandMode {
    /// Analysis/build only: git stays read-only.
    ReadOnly,
    /// Write tools are active: bounded git housekeeping is admitted.
    Write,
}

#[cfg(test)]
fn check_command_argv(argv: &[String]) -> Result<(), String> {
    check_command_argv_mode(argv, CommandMode::ReadOnly)
}

/// Validate a whitespace-split command against the allowlist for `mode`.
fn check_command_argv_mode(argv: &[String], mode: CommandMode) -> Result<(), String> {
    let Some(program) = argv.first() else {
        return Err("empty command".into());
    };
    // TODO Make this a constant and review to ensure none of these can mutate files
    let allowed_non_mutating = NON_MUTATING;
    let allowed: Vec<&str> = allowed_non_mutating
        .iter()
        .chain(
            [
                "cargo", "git", "find", "cmake", "ctest", "g++", "gcc", "clang++", "clang",
            ]
            .iter(),
        )
        .copied()
        .collect();

    if allowed.iter().find(|&p| p == program).is_none() {
        Err(format!(
            "command '{program}' is not allowed (only {})",
            allowed
                .iter()
                .fold("".to_string(), |a, b| format!("{a} {b}"))
        ))
    } else {
        match program.as_str() {
            val if allowed_non_mutating.contains(&program.as_str()) && val == program.as_str() => {
                Ok(())
            }
            "cmake" => check_cmake_argv(argv),
            "ctest" => check_ctest_argv(argv),
            "g++" | "gcc" | "clang++" | "clang" => check_compiler_argv(argv),
            "cargo" => {
                let Some(sub) = argv.get(1) else {
                    return Err(
                        "cargo requires a subcommand (check, build, test, clippy, fmt, doc, run)"
                            .into(),
                    );
                };
                if argv.iter().any(|a| a == "--manifest-path") {
                    return Err(
                        "cargo --manifest-path is not allowed (cwd is pinned to the repo root)"
                            .into(),
                    );
                }
                // TODO Make this a constant and review to ensure none of these can mutate files
                let allowed_non_mutating = [
                    "check", "build", "test", "clippy", "fmt", "doc", "metadata", "tree",
                ];

                match sub.as_str() {
                    val if allowed_non_mutating.contains(&sub.as_str()) && val == sub.as_str() => {
                        Ok(())
                    }
                    "run" => {
                        // Only `cargo run -p hanihi-eval` is permitted.
                        let mut package: Option<&str> = None;
                        let mut iter = argv[1..].iter().peekable();
                        while let Some(a) = iter.next() {
                            if a == "--" {
                                break;
                            }
                            if a == "-p" || a == "--package" {
                                package = iter.peek().map(|s| s.as_str());
                            }
                        }
                        match package {
                            Some("hanihi-eval") => Ok(()),
                            Some(other) => Err(format!(
                                "cargo run is restricted to -p hanihi-eval (got -p {other})"
                            )),
                            None => Err("cargo run requires -p hanihi-eval".into()),
                        }
                    }
                    other => Err(format!("cargo subcommand '{other}' is not allowed")),
                }
            }
            "git" => {
                let Some(sub) = argv.get(1) else {
                    return Err(
                        "git requires a subcommand (status, diff, log, show, apply, check-ignore)"
                            .into(),
                    );
                };
                if argv.iter().any(|a| a == "-C" || a == "--directory") {
                    return Err(
                        "git -C/--directory is not allowed (cwd is pinned to the repo root)".into(),
                    );
                }
                // TODO Make this a constant and review to ensure none of these can mutate files
                let allowed_non_mutating = [
                    "annotate",
                    "blame",
                    "cat-file",
                    "check-attr",
                    "check-ignore",
                    "check-mailmap",
                    "check-ref-format",
                    "cherry",
                    "column",
                    "count-objects",
                    "describe",
                    "diff",
                    "diff-files",
                    "diff-index",
                    "diff-tree",
                    "fast-export",
                    "for-each-ref",
                    "for-each-reflog",
                    "fsck",
                    "grep",
                    "help",
                    "log",
                    "ls-files",
                    "ls-tree",
                    "mailinfo",
                    "merge-base",
                    "merge-tree",
                    "name-rev",
                    "pack-redundant",
                    "range-diff",
                    "rev-parse",
                    "shortlog",
                    "show",
                    "show-branch",
                    "show-ref",
                    "status",
                    "stripspace",
                    "version",
                    "verify-commit",
                    "verify-pack",
                    "verify-tag",
                    "whatchanged",
                ];
                match sub.as_str() {
                    val if allowed_non_mutating.contains(&sub.as_str()) && val == sub.as_str() => {
                        Ok(())
                    }
                    // `archive` to stdout only; forbid writing a file or hitting a remote.
                    "archive" => {
                        let forbidden_flags = ["--output", "-o", "--remote"];
                        if argv.iter().any(|a| forbidden_flags.contains(&a.as_str())) {
                            Err("git archive is restricted to stdout (no --output/--remote)".into())
                        } else {
                            Ok(())
                        }
                    }
                    "apply" => {
                        if !argv.iter().any(|a| a == "--check") {
                            return Err("git apply is restricted to --check".into());
                        }
                        Ok(())
                    }
                    "hash-object" => {
                        if argv.iter().any(|a| a == "-w") {
                            Err("git hash-object is restricted to not using -w".into())
                        } else {
                            Ok(())
                        }
                    }
                    "reflog" => {
                        // Read-only: bare `git reflog` defaults to `show HEAD`; also
                        // allow explicit `show`/`list`. `delete`/`expire` mutate.
                        let subcmd = argv.get(2).map(String::as_str);
                        match subcmd {
                            None | Some("show") | Some("list") => Ok(()),
                            _ => Err("git reflog is restricted to show/list".into()),
                        }
                    }
                    // `branch` listing only: no create/delete/move/copy/rename.
                    "branch" => {
                        let mutating = [
                            "-d",
                            "-D",
                            "-m",
                            "-M",
                            "-c",
                            "-C",
                            "--delete",
                            "--move",
                            "--copy",
                            "--edit-description",
                            "--set-upstream-to",
                        ];
                        // A bare `git branch` or flags only (`-a`, `-r`, `-l`, `-v`) is a listing.
                        let args = &argv[2..];
                        let has_name = args.iter().any(|a| !a.starts_with('-'));
                        if has_name || args.iter().any(|a| mutating.contains(&a.as_str())) {
                            Err(
                                "git branch is restricted to listing (no create/delete/move); \
                                 use `git log`, `git show`, or `git status` to inspect branches"
                                    .into(),
                            )
                        } else {
                            Ok(())
                        }
                    }
                    // `tag` listing only: no create/delete/sign/etc.
                    "tag" => {
                        let mutating = ["-a", "-s", "-d", "-f", "--delete", "--sign"];
                        let args = &argv[2..];
                        let has_name = args.iter().any(|a| !a.starts_with('-'));
                        if has_name || args.iter().any(|a| mutating.contains(&a.as_str())) {
                            Err("git tag is restricted to listing (no create/delete/sign); \
                                 use `git tag --list`, `git log`, or `git show` to inspect tags"
                                .into())
                        } else {
                            Ok(())
                        }
                    }
                    // `remote` listing/show only: no add/remove/rename/set-url/prune.
                    "remote" => match argv.get(2).map(String::as_str) {
                        None | Some("-v") | Some("--verbose") | Some("show") => Ok(()),
                        _ => Err("git remote is restricted to list/show".into()),
                    },
                    // `worktree` list only: no add/remove/lock/unlock/move/prune.
                    "worktree" => match argv.get(2).map(String::as_str) {
                        Some("list") => Ok(()),
                        _ => Err("git worktree is restricted to list".into()),
                    },
                    // `submodule` status/summary only: no add/update/deinit/sync.
                    "submodule" => match argv.get(2).map(String::as_str) {
                        Some("status") | Some("summary") => Ok(()),
                        _ => Err("git submodule is restricted to status/summary".into()),
                    },
                    // `config` read-only only: list/get, never a key=value write.
                    "config" => {
                        let read_only = [
                            "--list",
                            "-l",
                            "--get",
                            "--get-regexp",
                            "--get-all",
                            "--get-urlmatch",
                        ];
                        let args = &argv[2..];
                        if args.is_empty() || args.iter().any(|a| read_only.contains(&a.as_str())) {
                            Ok(())
                        } else {
                            Err(format!(
                                "git config is restricted to read operations: {args:?}"
                            ))
                        }
                    }
                    "add" => check_git_add(mode),
                    "restore" => check_git_restore(argv, mode),
                    "rm" => check_git_rm(argv, mode),
                    "commit" => check_git_commit(argv, mode),
                    other => Err(format!("git subcommand '{other}' is not allowed")),
                }
            }
            "find" => {
                if argv.get(1).is_none() {
                    Err("find requires arguments)".into())
                } else {
                    let forbidden_args: [&str; 9] = [
                        "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fls", "-fprint",
                        "-fprint0", "-fprintf",
                    ];
                    if argv.iter().any(|a| forbidden_args.contains(&a.as_str())) {
                        Err("Arg {a} not allowed with find".to_string())
                    } else {
                        Ok(())
                    }
                }
            }
            other => Err(format!(
                "command '{other}' is not allowed (only cargo, git, cmake, ctest, and g++/gcc/clang)"
            )),
        }
    }
}

/// Flags permitted on `git commit --amend`, besides `-m`/`--message` values.
const GIT_COMMIT_AMEND_OK: &[&str] = &["--amend", "--no-edit"];
/// `git commit` message flags; each consumes the following argv element.
const GIT_COMMIT_MESSAGE_FLAGS: &[&str] = &["-m", "--message"];
/// `git commit` message-from-file flags; each consumes the following element.
const GIT_COMMIT_FILE_FLAGS: &[&str] = &["-F", "--file"];

/// The directory the harness owns for its own stray writes: traces, session
/// logs, and hand-written commit-message files.
///
/// A message file must live here. `git commit -F` reads the path from inside
/// the repository, so bounding it is what stops `-F /etc/passwd` (arbitrary
/// read) and `-F ../secrets` (escape from the tree).
const WORKING_DIR: &str = "working";

/// Paths the agent may never write, relative to the repo root.
///
/// Duplicated from `write.rs`'s `is_protected` rather than shared: that
/// function is private to the write-tools module and is stated in terms of
/// what may be *written*, while this one guards a path git will *read*. The
/// two policies agree today; if they must diverge, they can.
fn is_protected_repo_path(rel: &str) -> bool {
    rel == ".ignore" || rel == ".gitignore" || rel.starts_with(".git/") || rel == ".git"
}

/// True when `arg` is `working` or a relative path whose first component is
/// `working`.
///
/// Mirrors [`is_under_build_dir`]: the path is inspected as components, so
/// `working/../../etc` is rejected before any filesystem access.
fn is_under_working_dir(arg: &str) -> bool {
    let mut components = Path::new(arg).components();
    match components.next() {
        Some(Component::Normal(first)) if first == WORKING_DIR => {}
        _ => return false,
    }
    components.all(|component| matches!(component, Component::Normal(_)))
}

// ── cmake / ctest / compilers ────────────────────────────────────

/// The only build directory the agent may touch. Out-of-source builds are the
/// supported layout, and this is enforced, not merely documented: a `-B` (or a
/// `--build`) argument that is not this directory, or a path under it, is
/// refused.
const BUILD_DIR: &str = "build";

/// Numeric argument accepted by `-j`/`--parallel`.
fn is_positive_integer(arg: &str) -> bool {
    !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit())
}

/// True when `arg` is `build` or a relative path whose first component is
/// `build`.
///
/// Rejects absolute paths, `..`, and any other escape attempt before the value
/// reaches CMake. This mirrors `SourceTree::resolve_for_write`: the path is
/// inspected as components, so `build/../../etc` cannot slip through a
/// string-matching check. A backslash is treated as an ordinary character;
/// only `/` separates components on the target platforms.
fn is_under_build_dir(arg: &str) -> bool {
    let mut components = Path::new(arg).components();
    match components.next() {
        Some(Component::Normal(first)) if first == BUILD_DIR => {}
        _ => return false,
    }
    components.all(|component| matches!(component, Component::Normal(_)))
}

/// Refuse a `-D` cache variable that names a program or an install location.
fn check_cmake_define(value: &str) -> Result<(), String> {
    const FORBIDDEN: [&str; 4] = [
        "CMAKE_INSTALL_PREFIX=",
        "CMAKE_CXX_COMPILER=",
        "CMAKE_C_COMPILER=",
        "CMAKE_MAKE_PROGRAM=",
    ];
    for name in FORBIDDEN {
        if value.starts_with(name) || value.starts_with(&format!("{name}:")) {
            return Err(format!(
                "cmake -D {name}... is not allowed (it names a program or an install path)"
            ));
        }
    }
    Ok(())
}

/// A read-only version probe: `--version` and nothing else.
///
/// `cmake --version` is how a caller asks which CMake it is talking to. It is
/// admitted only as the sole argument, so it can never be combined with a flag
/// that writes, and it does not have to be threaded through the configure loop
/// below (which would otherwise have to special-case a non-configuring flag).
/// `-v` is deliberately not accepted: for CMake it is `--version`, and for the
/// compilers `-v` is handled by their own checker.
fn is_sole_version_probe(args: &[String]) -> bool {
    args.len() == 1 && args[0] == "--version"
}

/// Validate a `cmake` argv: configure into `build/`, or build `build/`.
///
/// Only the out-of-source `build/` convention is supported, driven from the
/// pinned cwd. Anything that runs arbitrary CMake code (`-P`, `--script`),
/// redirects the source or build root (`-S`, `--source`, and any `-B`/`--build`
/// outside `build/`), selects a generator, or writes outside the repository
/// (`--install`, `--prefix`, the `CMAKE_*` program/prefix variables) is refused
/// with a message naming the reason.
fn check_cmake_argv(argv: &[String]) -> Result<(), String> {
    let args = &argv[1..];

    if is_sole_version_probe(args) {
        return Ok(());
    }

    let Some(first) = args.first().map(String::as_str) else {
        return Err(
	    "cmake requires arguments (configure with `cmake -B build`, build with `cmake --build build`)"
		.into(),
	);
    };

    if first == "--build" {
        return check_cmake_build_args(&args[1..]);
    }

    // Configure: every argument is either a validated flag or refused.
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        match arg {
            "-B" => {
                let Some(dir) = iter.next().map(String::as_str) else {
                    return Err("cmake -B requires a directory argument".into());
                };
                if !is_under_build_dir(dir) {
                    return Err(format!(
                        "cmake -B {dir} is not allowed (the build directory must be `{BUILD_DIR}` or a path under it)"
                    ));
                }
            }
            _ if arg.starts_with("-B") && arg.len() > 2 => {
                let dir = &arg[2..];
                if !is_under_build_dir(dir) {
                    return Err(format!(
                        "cmake -B {dir} is not allowed (the build directory must be `{BUILD_DIR}` or a path under it)"
                    ));
                }
            }
            "-D" => {
                let Some(definition) = iter.next().map(String::as_str) else {
                    return Err("cmake -D requires a NAME=VALUE argument".into());
                };
                check_cmake_define(definition)?;
            }
            _ if arg.starts_with("-D") && arg.len() > 2 => check_cmake_define(&arg[2..])?,
            "-P" | "--script" => {
                return Err(format!(
                    "cmake {arg} is not allowed (it runs arbitrary CMake code)"
                ));
            }
            "-S" | "--source" => {
                return Err(format!(
                    "cmake {arg} is not allowed (out-of-source builds run from the repository root)"
                ));
            }
            "--install" => {
                return Err(
                    "cmake --install is not allowed (it writes outside the repository)".into(),
                );
            }
            "--prefix" => {
                return Err(
                    "cmake --prefix is not allowed (it writes outside the repository)".into(),
                );
            }
            "-G" => {
                return Err("cmake -G is not allowed (the default generator is used)".into());
            }
            "-E" | "-H" | "--fresh" | "-U" => {
                return Err(format!("cmake {arg} is not allowed"));
            }
            _ => return Err(format!("cmake argument '{arg}' is not allowed")),
        }
    }

    Ok(())
}

/// Validate the arguments of `cmake --build <dir>`.
fn check_cmake_build_args(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first().map(String::as_str) else {
        return Err(format!(
            "cmake --build requires a build directory (`cmake --build {BUILD_DIR}`)"
        ));
    };
    if !is_under_build_dir(dir) {
        return Err(format!(
            "cmake --build {dir} is not allowed (the build directory must be `{BUILD_DIR}` or a path under it)"
        ));
    }

    let mut iter = args[1..].iter().peekable();
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        match arg {
            "--target" | "-t" => {
                if iter.next().is_none() {
                    return Err("cmake --build --target requires a name".into());
                }
            }
            "--config" | "-C" => {
                if iter.next().is_none() {
                    return Err("cmake --build --config requires a name".into());
                }
            }
            "-j" | "--parallel" => {
                match iter.peek().map(|s| s.as_str()) {
                    Some(n) if is_positive_integer(n) => {
                        iter.next();
                    }
                    // `-j` without a count means "all cores", which is bounded.
                    Some(_) => {
                        return Err("cmake --build -j requires a numeric argument".into());
                    }
                    None => {}
                }
            }
            "--install" => {
                return Err(
                    "cmake --build --install is not allowed (it writes outside the repository)"
                        .into(),
                );
            }
            _ => return Err(format!("cmake --build argument '{arg}' is not allowed")),
        }
    }

    Ok(())
}

/// Validate a `ctest` argv: run the tests in `build/`.
fn check_ctest_argv(argv: &[String]) -> Result<(), String> {
    let args = &argv[1..];

    if is_sole_version_probe(args) {
        return Ok(());
    }

    if args.is_empty() {
        return Err(format!(
            "ctest requires arguments (`ctest --test-dir {BUILD_DIR}`)"
        ));
    }

    let mut iter = args.iter().peekable();
    let mut saw_test_dir = false;
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        match arg {
            "--test-dir" | "-T" => {
                let Some(dir) = iter.next().map(|s| s.as_str()) else {
                    return Err("ctest --test-dir requires a directory argument".into());
                };
                if !is_under_build_dir(dir) {
                    return Err(format!(
                        "ctest --test-dir {dir} is not allowed (the test directory must be `{BUILD_DIR}` or a path under it)"
                    ));
                }
                saw_test_dir = true;
            }
            "--output-on-failure" => {}
            "-j" | "--parallel" => match iter.peek().map(|s| s.as_str()) {
                Some(n) if is_positive_integer(n) => {
                    iter.next();
                }
                Some(_) => return Err("ctest -j requires a numeric argument".into()),
                None => {}
            },
            _ => return Err(format!("ctest argument '{arg}' is not allowed")),
        }
    }

    if !saw_test_dir {
        return Err(format!(
            "ctest requires --test-dir {BUILD_DIR} (the cwd is the repository root)"
        ));
    }
    Ok(())
}

/// Flags that make a compiler load, run, or write to a caller-named path.
///
/// Both the joined (`-MFfoo`) and separated (`-MF foo`) forms are rejected, as
/// is `@file` (a response file pulls arbitrary flags from disk).
fn compiler_argv_is_forbidden(arg: &str) -> bool {
    const FORBIDDEN_PREFIXES: [&str; 5] = ["-MF", "-MT", "-MQ", "-MJ", "-Wl,"];
    const FORBIDDEN_EXACT: [&str; 9] = [
        "-o",
        "-Xclang",
        "-Xlinker",
        "-wrapper",
        "--serialize-diagnostics",
        "-fplugin=",
        "-B",
        "-Xpreprocessor",
        "-Xassembler",
    ];

    arg.starts_with('@')
        || arg.starts_with("-fplugin=")
        || FORBIDDEN_PREFIXES.iter().any(|p| arg.starts_with(p))
        || FORBIDDEN_EXACT.contains(&arg)
}

/// Validate a single-translation-unit compile: `<compiler> -c <file>`.
///
/// `-c` is mandatory. Compile-and-link is what `cmake --build` is for, and a
/// bare `g++ foo.cpp -o /tmp/x` would produce an executable at an arbitrary
/// path. Everything that names an output file, loads a plugin, or hands the
/// compiler another program is refused; every other flag is admitted, since it
/// can only affect the compilation of the one input file.
///
/// The input operand must be inside the repository: the compiler can already
/// read a header outside it via `-I`, but compiling an arbitrary file and
/// writing its object next to it is a wider capability than that.
fn check_compiler_argv(argv: &[String]) -> Result<(), String> {
    let args = &argv[1..];

    // A bare version probe compiles nothing, so it is admitted before the
    // `-c` requirement below. Both spellings are accepted: `--version` prints
    // the version, `-v` additionally prints the configured search paths.
    if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-v") {
        return Ok(());
    }

    if !args.iter().any(|a| a == "-c") {
        return Err(
	    "-c is required (build the project with `cmake --build build`; this tool only compiles one translation unit)"
		.into(),
	);
    }

    let mut inputs = 0usize;
    // `-c` and the input file are positional-free; everything else is either a
    // flag or the input operand.
    for arg in args {
        let arg = arg.as_str();
        if arg == "-c" || arg == "-fsyntax-only" {
            continue;
        }
        if compiler_argv_is_forbidden(arg) {
            return Err(format!("compiler argument '{arg}' is not allowed"));
        }

        // A bare `-` names stdin. It is checked before the generic flag branch
        // below, because it starts with `-` but is an input, not a flag.
        if arg == "-" {
            return Err("compiling from stdin ('-') is not allowed".into());
        }

        if arg.starts_with('-') {
            // A compilation flag: `-I`, `-isystem`, `-D`, `-std=`, `-O2`, ...
            // admitted because it affects only this translation unit.
            continue;
        }

        // An input operand.
        if Path::new(arg).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(format!(
                "compiler input '{arg}' is not allowed (it escapes the repository)"
            ));
        }
        inputs += 1;
    }

    if inputs == 0 {
        return Err("compiling requires an input file".into());
    }
    Ok(())
}

/// True when `arg` is `flag` exactly, or `flag=value`.
fn arg_is_flag(arg: &str, flag: &str) -> bool {
    arg == flag
        || arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

fn check_git_add(mode: CommandMode) -> Result<(), String> {
    if mode != CommandMode::Write {
        return Err("git add is restricted to write mode".into());
    }
    Ok(())
}

fn check_git_restore(argv: &[String], mode: CommandMode) -> Result<(), String> {
    if mode != CommandMode::Write {
        return Err("git restore is restricted to write mode".into());
    }
    if argv.get(2).map(String::as_str) != Some("--staged") {
        return Err("git restore is restricted to --staged (unstaging only)".into());
    }
    if argv[3..]
        .iter()
        .any(|a| arg_is_flag(a, "--source") || a == "-s")
    {
        return Err("git restore --source is not allowed".into());
    }
    Ok(())
}

fn check_git_rm(argv: &[String], mode: CommandMode) -> Result<(), String> {
    if mode != CommandMode::Write {
        return Err("git rm is restricted to write mode".into());
    }
    if !argv[2..].iter().any(|a| a == "--cached") {
        return Err("git rm is restricted to --cached (index only)".into());
    }
    Ok(())
}

fn check_git_commit(argv: &[String], mode: CommandMode) -> Result<(), String> {
    if mode != CommandMode::Write {
        return Err("git commit is restricted to write mode".into());
    }
    let args = &argv[2..];

    // `-m` and `-F` are mutually exclusive to git; refuse the combination
    // here so the caller gets a named rule instead of git's own message.
    let message_flag = args
        .iter()
        .any(|a| GIT_COMMIT_MESSAGE_FLAGS.contains(&a.as_str()) || arg_is_flag(a, "--message"));
    let file_flag = args
        .iter()
        .any(|a| GIT_COMMIT_FILE_FLAGS.contains(&a.as_str()) || arg_is_flag(a, "--file"));
    if message_flag && file_flag {
        return Err(
            "git commit -m/--message and -F/--file are mutually exclusive; use -F for a \
             multi-line message"
                .into(),
        );
    }

    check_commit_message_args(args)?;

    if args.iter().any(|a| a == "--amend") {
        check_commit_amend_args(args)?;
    }
    Ok(())
}

/// Validate every `-m`/`--message` and `-F`/`--file` value in a `git commit`
/// argv.
///
/// The harness splits its input on whitespace and does no quote processing,
/// so a caller who writes `-m "a b"` sends `"a`, `b"` and git reports a
/// baffling pathspec error. A value that is exactly a quote character (or
/// empty) is the signature of that mistake, so it is refused with guidance.
/// The check is deliberately narrow: a legitimate one-token message such as
/// `it's` is untouched, because a matcher broad enough to catch a typed
/// sentence would also refuse valid messages.
fn check_commit_message_args(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        let a = a.as_str();

        if GIT_COMMIT_FILE_FLAGS.contains(&a) {
            let Some(value) = iter.next().map(String::as_str) else {
                return Err(
                    "git commit -F/--file requires a file path (e.g. -F working/commit-msg.txt)"
                        .into(),
                );
            };
            check_commit_message_file(value)?;
            continue;
        }

        if let Some(value) = a.strip_prefix("--file=") {
            check_commit_message_file(value)?;
            continue;
        }

        if GIT_COMMIT_MESSAGE_FLAGS.contains(&a) || arg_is_flag(a, "--message") {
            // A joined `--message=value` carries its own value.
            let value = match a.strip_prefix("--message=") {
                Some(value) => Some(value.to_string()),
                None => iter.next().cloned(),
            };
            if let Some(value) = value
                && (value.is_empty() || value == "\"" || value == "'")
            {
                return Err(format!(
                    "git commit message '{value}' is not usable: the harness does not process \
                     quotes and splits the command on whitespace, so a multi-line message must \
                     be passed with -F <path> (write the message with write_file first)"
                ));
            }
        }
    }
    Ok(())
}

/// Validate the path given to `git commit -F/--file`.
///
/// git reads this file from inside the repository, so the content channel is
/// the path, not argv. An absolute path would read an arbitrary file
/// (`-F /etc/passwd`), and a `..` component would leave the tree
/// (`-F ../secrets`); both are refused. The path must lie under `working/`,
/// the directory the harness already owns for stray writes.
fn check_commit_message_file(value: &str) -> Result<(), String> {
    if value == "-" {
        return Err(
            "git commit -F - is not allowed: stdin is not a file (the harness sets stdin \
             to /dev/null); write the message to working/<name>.txt first"
                .into(),
        );
    }

    // Inspect the path as components, so `working/../../etc/motd` cannot slip
    // through a string-prefix check. Mirrors the input-operand rule in
    // `check_compiler_argv` and the root-scoping rule in `list_dir`.
    if Path::new(value).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!(
            "git commit message file '{value}' is not allowed (the path must be relative and \
             must stay inside the repository, under working/)"
        ));
    }

    if is_protected_repo_path(value) {
        return Err(format!(
            "git commit message file '{value}' is not allowed (the path is protected); \
             put the message under working/"
        ));
    }

    if !is_under_working_dir(value) {
        return Err(format!(
            "git commit message file '{value}' is not allowed (the message file must live \
             under working/, e.g. working/commit-msg.txt)"
        ));
    }

    Ok(())
}

/// Reject any `git commit --amend` argument outside the whitelisted shapes.
fn check_commit_amend_args(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        let a = a.as_str();

        if GIT_COMMIT_MESSAGE_FLAGS.contains(&a) {
            iter.next();
            continue;
        }
        if arg_is_flag(a, "--message") {
            continue;
        }
        // `-F`/`--file` is admitted by `check_commit_message_args`, which has
        // already bounded the path. Consume its value here so it is not read
        // as a stray argument.
        if GIT_COMMIT_FILE_FLAGS.contains(&a) {
            iter.next();
            continue;
        }
        if arg_is_flag(a, "--file") {
            continue;
        }
        if GIT_COMMIT_AMEND_OK.contains(&a) {
            continue;
        }
        return Err(format!("git commit --amend does not allow argument '{a}'"));
    }
    Ok(())
}

/// Build the minimal environment passed to child processes: PATH, HOME, and
/// CARGO_* / RUSTUP_* (needed for cargo/rustup to resolve the toolchain).
/// Everything else (API keys, etc.) is dropped.
pub(crate) fn scrubbed_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| {
            k == "PATH" || k == "HOME" || k.starts_with("CARGO_") || k.starts_with("RUSTUP_")
        })
        .collect()
}

/// Read a pipe to EOF as a lossy UTF-8 string.
async fn read_all<R>(mut reader: R) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Cap a string at `MAX_TOOL_RESULT_BYTES`, appending a truncation note.
/// Kept as a thin alias so run_command/read_session_log callers share the
/// agent-layer format.
fn cap_output(s: &str) -> String {
    truncate_tool_output(s)
}

/// Spawn `argv`, capture stdout/stderr, enforce a timeout (killing the child
/// on expiry), and return the outcome. The allowlist check happens in the
/// tool wrapper; this runs whatever argv it is given (used directly by tests).
async fn execute_captured(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutcome, String> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env_clear();
    for (k, v) in scrubbed_env() {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn '{}': {e}", argv[0]))?;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    // Read both pipes concurrently with the wait so a large stream on one
    // pipe cannot deadlock the other.
    let stdout_task = tokio::spawn(read_all(stdout_pipe));
    let stderr_task = tokio::spawn(read_all(stderr_pipe));

    let started = Instant::now();
    let status_result = tokio::time::timeout(timeout, child.wait()).await;

    let (exit_code, timed_out) = match status_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(format!("waiting for '{}': {e}", argv[0])),
        Err(_elapsed) => {
            // Kill the child on expiry; the readers hit EOF and complete.
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|e| format!("stdout task failed: {e}"))?
        .map_err(|e| format!("reading stdout: {e}"))?;
    let stderr = stderr_task
        .await
        .map_err(|e| format!("stderr task failed: {e}"))?
        .map_err(|e| format!("reading stderr: {e}"))?;

    Ok(CommandOutcome {
        exit_code,
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout,
        stderr,
    })
}

/// Persist a command execution to `<traces-dir>/<unix-ms>-<seq>.cmd.json`.
fn write_trace(
    traces_dir: &Path,
    command: &str,
    outcome: &CommandOutcome,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(traces_dir)?;
    let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = Utc::now().timestamp_millis();
    let name = format!("{ms}-{seq}.cmd.json");
    let path = traces_dir.join(name);
    let obj = json!({
    "ts": Utc::now().to_rfc3339(),
    "command": command,
    "exit_code": outcome.exit_code,
    "duration_ms": outcome.duration_ms,
    "stdout": outcome.stdout,
    "stderr": outcome.stderr,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&obj)?)?;
    Ok(path)
}

/// Render the tool result: exit code + duration + truncated output + trace path.
fn render_command_result(command: &str, outcome: &CommandOutcome, trace_path: &Path) -> String {
    let code = match outcome.exit_code {
        Some(c) => c.to_string(),
        None if outcome.timed_out => "killed (timeout)".to_string(),
        None => "killed (signal)".to_string(),
    };
    let mut out = format!(
        "command: {command}\nexit code: {code}\nduration: {}ms\n",
        outcome.duration_ms
    );
    if !outcome.stdout.is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(&cap_output(&outcome.stdout));
        out.push('\n');
    }
    if !outcome.stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&cap_output(&outcome.stderr));
        out.push('\n');
    }
    out.push_str(&format!("trace: {}", trace_path.display()));
    out
}

/// `run_command` description when write tools are active.
const RUN_COMMAND_DESC_WRITE: &str = "Run an allowlisted command inside the repository root. \
No shell: the command is split on whitespace into argv. cargo subcommands: check, build, test, \
clippy, fmt, doc, run -p hanihi-eval. git subcommands: status, diff, log, show, apply --check; \
write mode also allows git add, git restore --staged, git rm --cached, git commit, and git \
commit --amend. cmake: `cmake -B build` to configure and `cmake --build build` to build; ctest: \
`ctest --test-dir build`. `cmake --version`, `ctest --version`, and `<compiler> --version` probe \
the toolchain. Compilers (g++/gcc/clang++/clang) with `-c <file>` compile one \
translation unit; add `-fsyntax-only` for a fast parse check. cwd is pinned to the repo root; \
the environment is scrubbed (PATH, HOME, CARGO_* only); output is capped at 64 KiB; the full \
output is written to a trace file. Returns exit code, duration, stdout/stderr (truncated), and \
the trace path.";

/// Tool: run an allowlisted `cargo`/`git` command inside the repo root.
///
/// Registered always (it is an analysis/build tool, not a write tool). The
/// command is split on whitespace (no shell), validated against an allowlist,
/// executed with a scrubbed environment and a timeout, and the full output is
/// persisted to a trace file. The result carries exit code + duration +
/// truncated stdout/stderr + the trace path.
pub fn builtin_run_command(tree: Arc<SourceTree>, traces_dir: PathBuf) -> PortableDynamicTool {
    builtin_run_command_for(tree, traces_dir, CommandMode::ReadOnly)
}

/// Write-mode variant of [`builtin_run_command`]: admits bounded git
/// housekeeping verbs (`add`, `restore --staged`, `rm --cached`, `commit`,
/// `commit --amend`).
pub fn builtin_run_command_write(
    tree: Arc<SourceTree>,
    traces_dir: PathBuf,
) -> PortableDynamicTool {
    builtin_run_command_for(tree, traces_dir, CommandMode::Write)
}

fn builtin_run_command_for(
    tree: Arc<SourceTree>,
    traces_dir: PathBuf,
    mode: CommandMode,
) -> PortableDynamicTool {
    // Read-only keeps its original description verbatim; only write mode
    // advertises the extra housekeeping verbs.
    let description = match mode {
        CommandMode::ReadOnly => {
            "Run an allowlisted command inside the repository root. No shell: the command is \
	     split on whitespace into argv. cargo subcommands: check, build, test, clippy, fmt, \
	     doc, run -p hanihi-eval. git subcommands: status, diff, log, show, apply --check. \
	     cmake: `cmake -B build` to configure and `cmake --build build` to build; ctest: \
	     `ctest --test-dir build`. `cmake --version`, `ctest --version`, and `<compiler> \
	     --version` probe the toolchain. Compilers (g++/gcc/clang++/clang) with `-c <file>` \
	     compile one translation unit; add `-fsyntax-only` for a fast parse check. cwd is \
	     pinned to the repo root; the environment is scrubbed (PATH, HOME, CARGO_* only); \
	     output is capped at 64 KiB; the full output is written to a trace file. Returns \
	     exit code, duration, stdout/stderr (truncated), and the trace path."
        }
        CommandMode::Write => RUN_COMMAND_DESC_WRITE,
    };
    PortableDynamicTool::new(
        "run_command",
        description,
        json!({
            "type": "object",
            "properties": {
            "command": {
            "type": "string",
            "description": "Allowlisted command, e.g. \"cargo check --workspace\""
            },
            "timeout_secs": {
            "type": "integer",
            "description": "Max seconds (default 120, max 600)"
            }
            },
            "required": ["command"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            let traces_dir = traces_dir.clone();
            Box::pin(async move {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'command'")
                    })?
                    .to_string();
                let timeout_secs = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .clamp(1, MAX_TIMEOUT_SECS);

                let argv: Vec<String> = command.split_whitespace().map(String::from).collect();
                if argv.is_empty() {
                    return Err(ToolExecutionError::invalid_args("empty command"));
                }
                check_command_argv_mode(&argv, mode)
                    .map_err(ToolExecutionError::permission_denied)?;
                tracing::debug!(argv = %argv.join(" "), "run_command");
                let outcome =
                    execute_captured(&argv, tree.root(), Duration::from_secs(timeout_secs))
                        .await
                        .map_err(ToolExecutionError::provider)?;
                let trace_path = write_trace(&traces_dir, &command, &outcome)
                    .map_err(|e| ToolExecutionError::provider(format!("writing trace: {e}")))?;
                Ok(ToolOutput::text(render_command_result(
                    &command,
                    &outcome,
                    &trace_path,
                )))
            })
        },
    )
}

/// Maximum number of matches `grep` returns before truncating.
const MAX_GREP_MATCHES: usize = 200;
/// Maximum total bytes of `grep` output before truncating.
const MAX_GREP_BYTES: usize = 64 * 1024;

/// Collects `path:line: text` matches for [`builtin_grep`], capped at
/// [`MAX_GREP_MATCHES`] matches / [`MAX_GREP_BYTES`] bytes.
///
/// `grep-searcher`'s `SinkMatch` carries no path, so the caller sets
/// [`MatchSink::current_path`] before each single-file search.
struct MatchSink {
    current_path: PathBuf,
    lines: Vec<String>,
    bytes: usize,
    capped: bool,
}

impl Sink for MatchSink {
    type Error = Box<dyn std::error::Error>;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        lines: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let path = self.current_path.display();
        let line_no = lines.line_number().unwrap_or(0);
        let text = String::from_utf8_lossy(lines.bytes());
        let entry = format!("{path}:{line_no}: {}", text.trim_end());
        self.bytes += entry.len();
        self.lines.push(entry);
        if self.lines.len() >= MAX_GREP_MATCHES || self.bytes >= MAX_GREP_BYTES {
            self.capped = true;
            return Ok(false);
        }
        Ok(true)
    }
}

/// Tool: regex-search file contents inside the git repository.
///
/// Walks the repo honouring ignore rules (via [`SourceTree::walk`]) and
/// searches each file with ripgrep's searcher. Results are
/// `path:line: text` entries capped at `MAX_GREP_MATCHES` matches /
/// `MAX_GREP_BYTES` bytes. Binary files are skipped.
pub fn builtin_grep(tree: Arc<SourceTree>) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "grep",
        "Search file contents in the git repository with a regular expression. `pattern` is a \
	 regex (ripgrep syntax). `path` is a directory relative to the repo root (default: the \
	 root). `ignore_case` makes the match case-insensitive. Git-ignored paths are never \
	 searched. Results are `path:line: text` entries, capped at 200 matches.",
        json!({
            "type": "object",
            "properties": {
            "pattern": { "type": "string", "description": "Regular expression to search for" },
            "path": { "type": "string", "description": "Directory relative to repo root (default: root)" },
            "ignore_case": { "type": "boolean", "description": "Case-insensitive match" }
            },
            "required": ["pattern"]
        }),
        move |args: serde_json::Value| {
            let tree = tree.clone();
            Box::pin(async move {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolExecutionError::invalid_args("missing string field 'pattern'")
                    })?;
                let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let ignore_case = args
                    .get("ignore_case")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let matcher = if ignore_case {
                    RegexMatcherBuilder::new()
                        .case_insensitive(true)
                        .build(pattern)
                } else {
                    RegexMatcher::new(pattern)
                }
                .map_err(|e| ToolExecutionError::invalid_args(format!("invalid regex: {e}")))?;

                let mut sink = MatchSink {
                    current_path: PathBuf::new(),
                    lines: Vec::new(),
                    bytes: 0,
                    capped: false,
                };
                let mut searcher = Searcher::new();
                searcher.set_binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'));

                let walk = tree
                    .walk(Path::new(rel), usize::MAX)
                    .map_err(map_source_err)?;
                for entry in walk {
                    let entry = entry.map_err(|e| ToolExecutionError::provider(e.to_string()))?;
                    if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        continue;
                    }
                    let path = entry.path();
                    sink.current_path = path.to_path_buf();
                    searcher
                        .search_path(matcher.clone(), path, &mut sink)
                        .map_err(|e| {
                            ToolExecutionError::provider(format!(
                                "searching {}: {e}",
                                path.display()
                            ))
                        })?;
                    if sink.capped {
                        break;
                    }
                }

                let mut out = if sink.lines.is_empty() {
                    "no matches".to_string()
                } else {
                    sink.lines.join("\n")
                };
                if sink.capped {
                    out.push_str("\n…[truncated: too many matches]");
                }
                Ok(ToolOutput::text(out))
            })
        },
    )
}

/// Tool: read entries from this session's event log (the agent's own trace).
///
/// The log lives outside the repo (under the working dir), so the source-tree
/// tools cannot see it. This tool gives the agent a window into its own past
/// executions — the raw material for studying traces of execution.
pub fn builtin_read_session_log(log_path: PathBuf) -> PortableDynamicTool {
    PortableDynamicTool::new(
        "read_session_log",
        "Read entries from this session's event log. `kind` filters by event kind \
	 (user_input, llm_prompt, llm_response, tool_execution, turn_complete, error, \
	 session_created, ...). `turn` filters by turn number. `tail` returns only the last N \
	 entries (default 50, max 1000). Entries are rendered as compact JSON, one per line.",
        json!({
            "type": "object",
            "properties": {
            "kind": { "type": "string", "description": "Event kind filter" },
            "turn": { "type": "integer", "description": "Turn number filter" },
            "tail": { "type": "integer", "description": "Last N entries (default 50, max 1000)" }
            }
        }),
        move |args: serde_json::Value| {
            let log_path = log_path.clone();
            Box::pin(async move {
                let kind = args.get("kind").and_then(|v| v.as_str()).map(String::from);
                let turn = args.get("turn").and_then(|v| v.as_u64());
                let tail = args
                    .get("tail")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .clamp(1, 1000) as usize;

                let content = std::fs::read_to_string(&log_path).map_err(|e| {
                    ToolExecutionError::provider(format!("reading {}: {e}", log_path.display()))
                })?;
                let mut entries: Vec<serde_json::Value> = Vec::new();
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if let Some(k) = &kind
                        && v.get("kind").and_then(|x| x.as_str()) != Some(k.as_str())
                    {
                        continue;
                    }
                    if let Some(t) = turn
                        && v.get("turn").and_then(|x| x.as_u64()) != Some(t)
                    {
                        continue;
                    }
                    entries.push(v);
                }
                let start = entries.len().saturating_sub(tail);
                let mut rendered = entries[start..]
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                if rendered.is_empty() {
                    rendered = "no log entries match".to_string();
                }
                Ok(ToolOutput::text(cap_output(&rendered)))
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::Fixture;

    #[tokio::test]
    async fn list_dir_tool_lists_only_visible() {
        let fx = Fixture::new();
        let tool = builtin_list_dir(fx.tree());
        let out = tool
            .execute(serde_json::json!({ "path": ".", "depth": 2 }))
            .await
            .expect("list succeeds")
            .render();
        assert!(out.contains("src"), "got: {out}");
        assert!(out.contains("Cargo.toml"), "got: {out}");
        assert!(!out.contains("target"), "got: {out}");
        assert!(!out.contains("junk"), "got: {out}");
    }

    /// A missing nested directory must name the nearest existing ancestor so
    /// the model can correct the path in one step instead of guessing.
    #[tokio::test]
    async fn list_dir_reports_missing_path_with_nearest_ancestor() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.dir.join("working/traces")).unwrap();
        let tool = builtin_list_dir(fx.tree());

        let err = tool
            .execute(serde_json::json!({
            "path": "working/traces/2026-09-27-remove-old-params"
            }))
            .await
            .expect_err("missing path must fail");

        let message = err.to_string();
        assert!(message.contains("no such path"), "got: {message}");
        assert!(
            message.contains("nearest existing ancestor: working/traces"),
            "got: {message}"
        );
    }

    /// An existing but empty directory is a success with an empty listing —
    /// distinct from the missing-path error above.
    #[tokio::test]
    async fn list_dir_accepts_existing_empty_directory() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.dir.join("empty")).unwrap();
        let tool = builtin_list_dir(fx.tree());

        let out = tool
            .execute(serde_json::json!({ "path": "empty" }))
            .await
            .expect("existing empty directory must succeed")
            .render();
        assert!(
            out.trim().is_empty(),
            "expected an empty listing, got: {out}"
        );
    }

    /// `list_dir` is root-scoped: `..` must be refused before any filesystem
    /// probe, so the error can never name a path outside the repository.
    #[tokio::test]
    async fn list_dir_refuses_paths_escaping_the_repository() {
        let fx = Fixture::new();
        let tool = builtin_list_dir(fx.tree());

        let err = tool
            .execute(serde_json::json!({ "path": "../outside" }))
            .await
            .expect_err("escaping path must fail");
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    // ── grep ──

    #[tokio::test]
    async fn grep_finds_matches_and_honours_ignores() {
        let fx = Fixture::new();
        let tool = builtin_grep(fx.tree());

        let out = tool
            .execute(serde_json::json!({ "pattern": "fn main" }))
            .await
            .expect("grep succeeds");
        let rendered = out.render();
        assert!(rendered.contains("src/main.rs:1"), "got: {rendered}");

        // The only file containing "junk" is git-ignored (target/) — the
        // search must never see it.
        let out = tool
            .execute(serde_json::json!({ "pattern": "junk" }))
            .await
            .expect("grep succeeds");
        assert!(out.render().contains("no matches"), "got: {}", out.render());
    }

    #[tokio::test]
    async fn grep_rejects_invalid_regex() {
        let fx = Fixture::new();
        let tool = builtin_grep(fx.tree());
        let err = tool
            .execute(serde_json::json!({ "pattern": "(unclosed" }))
            .await
            .expect_err("invalid regex must fail");
        assert!(err.to_string().contains("regex"), "got: {err}");
    }

    // ── read_session_log ──

    #[tokio::test]
    async fn read_session_log_filters_kind_turn_tail() {
        use crate::session::log::LogEntry;

        let path =
            std::env::temp_dir().join(format!("hanihi-logtool-{}.jsonl", uuid::Uuid::new_v4()));
        let mut writer = crate::session::log::LogWriter::open(&path).expect("open log");
        let now = chrono::Utc::now();
        writer
            .write_entry(&LogEntry::user_input(now, 1, "hello".into()))
            .expect("write");
        writer
            .write_entry(&LogEntry::turn_complete(now, 1, "hi there".into(), 0))
            .expect("write");
        writer
            .write_entry(&LogEntry::user_input(now, 2, "bye".into()))
            .expect("write");
        drop(writer);

        let tool = builtin_read_session_log(path.clone());

        let out = tool
            .execute(serde_json::json!({ "kind": "user_input" }))
            .await
            .expect("log read");
        let rendered = out.render();
        assert!(
            rendered.contains("hello") && rendered.contains("bye"),
            "got: {rendered}"
        );
        assert!(!rendered.contains("turn_complete"), "got: {rendered}");

        let out = tool
            .execute(serde_json::json!({ "turn": 2 }))
            .await
            .expect("log read");
        let rendered = out.render();
        assert!(rendered.contains("bye"), "got: {rendered}");
        assert!(!rendered.contains("hello"), "got: {rendered}");

        let out = tool
            .execute(serde_json::json!({ "tail": 1 }))
            .await
            .expect("log read");
        let rendered = out.render();
        assert!(rendered.contains("bye"), "got: {rendered}");
        assert!(!rendered.contains("hi there"), "got: {rendered}");

        std::fs::remove_file(&path).unwrap_or(());
    }

    // ── run_command ──

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_allowlist_accepts_cargo_and_git() {
        for cmd in [
            argv(&["cargo", "check", "--workspace"]),
            argv(&["cargo", "build"]),
            argv(&["cargo", "test", "-p", "hanihi-core"]),
            argv(&["cargo", "clippy", "--", "-D", "warnings"]),
            argv(&["cargo", "fmt"]),
            argv(&["cargo", "doc"]),
            argv(&["cargo", "tree"]),
            argv(&["cargo", "run", "-p", "hanihi-eval"]),
            argv(&["git", "status", "--short"]),
            argv(&["git", "diff"]),
            argv(&["git", "log", "--oneline", "-20"]),
            argv(&["git", "show", "HEAD"]),
            argv(&["git", "grep", "fn main"]),
            argv(&["git", "apply", "--check"]),
        ] {
            check_command_argv(&cmd).expect("must be allowed");
        }
    }

    /// `pgrep` is a read-only process query in the same family as the
    /// already-allowed `ps`/`top`. It is admitted in both modes, so it is
    /// asserted as an accepted command, not a denied one.
    #[test]
    fn command_allowlist_accepts_pgrep() {
        for cmd in [
            argv(&["pgrep", "cargo"]),
            argv(&["pgrep", "-f", "cargo"]),
            argv(&["pgrep", "-a", "cargo"]),
        ] {
            check_command_argv(&cmd).expect("must be allowed");
        }
    }

    #[test]
    fn command_allowlist_denies_unknown_and_disallowed() {
        let cases = [
            argv(&[]),
            argv(&["rm", "-rf", "/"]),
            argv(&["sh", "-c", "echo hi"]),
            argv(&["cargo", "publish"]),
            argv(&["cargo", "install", "x"]),
            argv(&["cargo", "run"]),
            argv(&["cargo", "run", "-p", "hanihi-cli"]),
            argv(&["git", "push", "origin", "main"]),
            argv(&["git", "reset", "--hard"]),
            argv(&["git", "apply"]),
        ];
        for cmd in cases {
            assert!(check_command_argv(&cmd).is_err(), "should deny: {cmd:?}");
        }
    }

    #[test]
    fn command_allowlist_denies_cwd_escapes() {
        // -C / --directory (git) and --manifest-path (cargo) escape the
        // pinned repo root.
        assert!(check_command_argv(&argv(&["git", "-C", "/tmp", "status"])).is_err());
        assert!(check_command_argv(&argv(&["git", "--directory", "/tmp", "status"])).is_err());
        assert!(
            check_command_argv(&argv(&[
                "cargo",
                "check",
                "--manifest-path",
                "/etc/Cargo.toml"
            ]))
            .is_err()
        );
    }

    #[test]
    fn command_allowlist_read_only_denies_housekeeping() {
        let cases = [
            argv(&["git", "add"]),
            argv(&["git", "add", "-A"]),
            argv(&["git", "add", "--all"]),
            argv(&["git", "restore", "--staged", "src/main.rs"]),
            argv(&["git", "rm", "--cached", "src/main.rs"]),
            argv(&["git", "commit"]),
            argv(&["git", "commit", "--amend", "--no-edit"]),
        ];
        for cmd in cases {
            assert!(
                check_command_argv_mode(&cmd, CommandMode::ReadOnly).is_err(),
                "read-only mode must deny: {cmd:?}"
            );
        }
    }

    #[test]
    fn command_allowlist_write_accepts_housekeeping() {
        let cases = [
            argv(&["git", "add"]),
            argv(&["git", "add", "-A"]),
            argv(&["git", "add", "--all"]),
            argv(&["git", "restore", "--staged", "src/main.rs"]),
            argv(&["git", "restore", "--staged", "src/lib.rs", "Cargo.toml"]),
            argv(&["git", "rm", "--cached", "src/main.rs"]),
            argv(&["git", "commit"]),
            argv(&["git", "commit", "--amend"]),
            argv(&["git", "commit", "--amend", "--no-edit"]),
            argv(&["git", "commit", "--no-edit", "--amend"]),
            argv(&["git", "commit", "--amend", "-m", "reword"]),
            argv(&["git", "commit", "--amend", "--message", "reword"]),
        ];
        for cmd in cases {
            check_command_argv_mode(&cmd, CommandMode::Write)
                .unwrap_or_else(|e| panic!("write mode must allow {cmd:?}: {e}"));
        }
    }

    #[test]
    fn command_allowlist_write_still_denies_history_verbs() {
        let cases = [
            argv(&["git", "push", "origin", "main"]),
            argv(&["git", "reset"]),
            argv(&["git", "reset", "--hard"]),
            argv(&["git", "rebase", "main"]),
            argv(&["git", "filter-branch", "--all"]),
            argv(&["git", "filter-repo", "--path", "x"]),
            argv(&["git", "merge", "main"]),
            argv(&["git", "cherry-pick", "HEAD~1"]),
            argv(&["git", "revert", "HEAD"]),
        ];
        for cmd in cases {
            assert!(
                check_command_argv_mode(&cmd, CommandMode::Write).is_err(),
                "write mode must deny: {cmd:?}"
            );
        }
    }

    #[test]
    fn command_allowlist_write_bounds_commit_amend() {
        let allowed = [
            argv(&["git", "commit", "--amend"]),
            argv(&["git", "commit", "--amend", "--no-edit"]),
            argv(&["git", "commit", "--amend", "-m", "msg"]),
            argv(&["git", "commit", "--amend", "--message", "msg"]),
        ];
        for cmd in allowed {
            check_command_argv_mode(&cmd, CommandMode::Write)
                .unwrap_or_else(|e| panic!("bounded amend must allow {cmd:?}: {e}"));
        }

        let denied = [
            argv(&["git", "commit", "--amend", "-c", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "--reedit-message", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "-C", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "--reuse-message", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "--fixup", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "--fixup=HEAD~2"]),
            argv(&["git", "commit", "--amend", "--squash", "HEAD~2"]),
            argv(&["git", "commit", "--amend", "--squash=HEAD~2"]),
            argv(&["git", "commit", "--amend", "--author", "X <x@y>"]),
            argv(&["git", "commit", "--amend", "--author=X <x@y>"]),
            argv(&["git", "commit", "--amend", "--date", "2020-01-01"]),
            argv(&["git", "commit", "--amend", "--date=2020-01-01"]),
            argv(&[
                "git",
                "commit",
                "--amend",
                "--committer-date-is-author-date",
            ]),
            argv(&["git", "commit", "--amend", "--reset-author"]),
        ];
        for cmd in denied {
            assert!(
                check_command_argv_mode(&cmd, CommandMode::Write).is_err(),
                "must deny: {cmd:?}"
            );
        }
    }

    #[test]
    fn command_allowlist_write_denies_cwd_escapes() {
        let cases = [
            argv(&["git", "-C", "/tmp", "add", "-A"]),
            argv(&["git", "--directory", "/tmp", "restore", "--staged", "x"]),
            argv(&["git", "-C", "/tmp", "rm", "--cached", "x"]),
            argv(&["git", "-C", "/tmp", "commit", "--amend", "--no-edit"]),
        ];
        for cmd in cases {
            assert!(
                check_command_argv_mode(&cmd, CommandMode::Write).is_err(),
                "write mode must deny cwd escape: {cmd:?}"
            );
        }
        assert!(
            check_command_argv_mode(
                &argv(&["cargo", "check", "--manifest-path", "/etc/Cargo.toml"]),
                CommandMode::Write
            )
            .is_err()
        );
    }

    // ── cmake / ctest / compilers ──
    //
    // These admissions are toolchain- and mode-independent: the gate is a pure
    // function of argv, so every allowed command must hold in both modes and
    // every refusal must too.

    /// Assert `cmd` is admitted in both modes.
    fn assert_allowed_both_modes(cmd: &[String]) {
        for mode in [CommandMode::ReadOnly, CommandMode::Write] {
            check_command_argv_mode(cmd, mode)
                .unwrap_or_else(|e| panic!("must allow {cmd:?} in {mode:?}: {e}"));
        }
    }

    /// Assert `cmd` is refused in both modes, and that the refusal names
    /// `expected` — the specific rule that should have fired. Asserting the
    /// message (rather than merely that it is not the generic fallback) is what
    /// makes a deny-test fail if the rule is deleted: an unknown binary, or a
    /// rule that stopped matching, produces a different message.
    fn assert_denied_both_modes(cmd: &[String], expected: &str) {
        for mode in [CommandMode::ReadOnly, CommandMode::Write] {
            let err = check_command_argv_mode(cmd, mode)
                .expect_err(&format!("must deny {cmd:?} in {mode:?}"));
            assert!(
                err.contains(expected),
                "refusal for {cmd:?} should mention {expected:?}, got: {err}"
            );
        }
    }

    #[test]
    fn command_allowlist_accepts_cmake_configure_and_build() {
        let cases = [
            argv(&["cmake", "-B", "build"]),
            argv(&["cmake", "-B", "build/Debug"]),
            argv(&["cmake", "--build", "build"]),
            argv(&["cmake", "--build", "build", "--target", "mylib"]),
            argv(&["cmake", "--build", "build", "-j", "8"]),
            argv(&["cmake", "--build", "build", "--parallel", "8"]),
            argv(&["cmake", "--build", "build", "--config", "Release"]),
            argv(&["cmake", "--build", "build/Debug", "--target", "mylib"]),
        ];
        for cmd in cases {
            assert_allowed_both_modes(&cmd);
        }
    }

    #[test]
    fn command_allowlist_accepts_ctest() {
        let cases = [
            argv(&["ctest", "--test-dir", "build"]),
            argv(&["ctest", "--test-dir", "build", "--output-on-failure"]),
            argv(&["ctest", "--test-dir", "build", "-j", "4"]),
        ];
        for cmd in cases {
            assert_allowed_both_modes(&cmd);
        }
    }

    /// A bare version probe answers "which toolchain is this?" without
    /// building anything. It is the first thing a caller wants when it meets
    /// an unfamiliar C++ project, and refusing it forced a detour through
    /// `--help` or a throwaway compile.
    #[test]
    fn command_allowlist_accepts_sole_version_probe() {
        let cases = [
            argv(&["cmake", "--version"]),
            argv(&["ctest", "--version"]),
            argv(&["g++", "--version"]),
            argv(&["gcc", "--version"]),
            argv(&["clang++", "--version"]),
            argv(&["clang", "--version"]),
            // GCC/Clang answer `-v` with the version and their configured
            // search paths; CMake does not, so `cmake -v` is not included.
            argv(&["g++", "-v"]),
            argv(&["clang++", "-v"]),
        ];
        for cmd in cases {
            assert_allowed_both_modes(&cmd);
        }
    }

    /// The version probe is admitted only when it is the *sole* argument, so
    /// it can never smuggle a write alongside it. These cases are what keep
    /// the rule narrow: a looser "contains --version" check would admit them.
    #[test]
    fn command_allowlist_bounds_the_version_probe() {
        // `cmake` refuses anything it does not recognise, so a probe combined
        // with another argument must be refused with the specific rule.
        assert_denied_both_modes(
            &argv(&["cmake", "--version", "-B", "build"]),
            "is not allowed",
        );
        assert_denied_both_modes(
            &argv(&["cmake", "-B", "build", "--version"]),
            "is not allowed",
        );
        // `-v` is not CMake's version flag.
        assert_denied_both_modes(&argv(&["cmake", "-v"]), "is not allowed");
        // A compiler probe may not carry an input operand: `-c` is still
        // required for anything that compiles.
        assert_denied_both_modes(
            &argv(&["g++", "--version", "src/foo.cpp"]),
            "-c is required",
        );
        // `ctest --version` is admitted, but a probe with a test directory is
        // not a probe.
        assert_denied_both_modes(
            &argv(&["ctest", "--version", "--test-dir", "build"]),
            "is not allowed",
        );
    }

    #[test]
    fn command_allowlist_accepts_per_file_compile() {
        let cases = [
            argv(&[
                "g++",
                "-c",
                "src/foo.cpp",
                "-Iinclude",
                "-std=c++20",
                "-Wall",
                "-Werror",
            ]),
            argv(&["g++", "-c", "src/foo.cpp", "-fsyntax-only"]),
            argv(&["clang++", "-c", "src/foo.cpp", "-DNDEBUG"]),
            argv(&["gcc", "-c", "src/foo.c"]),
            argv(&["clang", "-c", "src/foo.c"]),
        ];
        for cmd in cases {
            assert_allowed_both_modes(&cmd);
        }
    }

    #[test]
    fn command_allowlist_denies_cmake_escapes_and_scripts() {
        // Each case pairs the argv with the rule that must refuse it, so a
        // passing deny-test cannot be a test of "the binary is unknown".
        let cases: &[(&[&str], &str)] = &[
            (&["cmake"], "cmake requires arguments"),
            (
                &["cmake", "-P", "script.cmake"],
                "runs arbitrary CMake code",
            ),
            (
                &["cmake", "--script", "script.cmake"],
                "runs arbitrary CMake code",
            ),
            (
                &["cmake", "-P", "../../evil.cmake"],
                "runs arbitrary CMake code",
            ),
            (
                &["cmake", "-S", ".", "-B", "build"],
                "out-of-source builds run from",
            ),
            (
                &["cmake", "-S", "/etc", "-B", "build"],
                "out-of-source builds run from",
            ),
            (&["cmake", "--source", "."], "out-of-source builds run from"),
            (&["cmake", "-B", "/tmp/build"], "build directory must be"),
            (&["cmake", "-B", "../build"], "build directory must be"),
            (
                &["cmake", "-B", "build/../../etc"],
                "build directory must be",
            ),
            (
                &["cmake", "-B", "build", "-B", "/tmp"],
                "build directory must be",
            ),
            (&["cmake", "--build", "/etc"], "build directory must be"),
            (&["cmake", "--build", "../build"], "build directory must be"),
            (
                &["cmake", "--build", "build", "--install", "build"],
                "writes outside the repository",
            ),
            (
                &["cmake", "--install", "build"],
                "writes outside the repository",
            ),
            (
                &["cmake", "--install", "build", "--prefix", "/usr"],
                "writes outside the repository",
            ),
            (
                &["cmake", "-D", "CMAKE_INSTALL_PREFIX=/usr", "-B", "build"],
                "names a program or an install path",
            ),
            (
                &[
                    "cmake",
                    "-D",
                    "CMAKE_CXX_COMPILER=/usr/bin/evil",
                    "-B",
                    "build",
                ],
                "names a program or an install path",
            ),
            (
                &["cmake", "-D", "CMAKE_C_COMPILER=evil", "-B", "build"],
                "names a program or an install path",
            ),
            (
                &["cmake", "-D", "CMAKE_MAKE_PROGRAM=evil", "-B", "build"],
                "names a program or an install path",
            ),
            (
                &["cmake", "-G", "Ninja", "-B", "build"],
                "default generator is used",
            ),
            (
                &["cmake", "-E", "rm", "-rf", "build"],
                "cmake -E is not allowed",
            ),
            (
                &["cmake", "--fresh", "-B", "build"],
                "cmake --fresh is not allowed",
            ),
            (
                &["cmake", "-H.", "-Bbuild"],
                "cmake argument '-H.' is not allowed",
            ),
        ];
        for (args, expected) in cases {
            assert_denied_both_modes(&argv(args), expected);
        }
    }

    #[test]
    fn command_allowlist_denies_compiler_passthrough_and_outputs() {
        let cases: &[(&[&str], &str)] = &[
            (&["g++", "src/foo.cpp"], "-c is required"),
            (
                &["g++", "-c", "src/foo.cpp", "-o", "/tmp/x.o"],
                "compiler argument '-o'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-o", "foo.o"],
                "compiler argument '-o'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-MF", "dep.d"],
                "compiler argument '-MF'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-MT", "target"],
                "compiler argument '-MT'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-MQ", "target"],
                "compiler argument '-MQ'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-MJ", "out.json"],
                "compiler argument '-MJ'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-fplugin=evil.so"],
                "compiler argument '-fplugin=evil.so'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-B", "/usr/lib/evil"],
                "compiler argument '-B'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "-wrapper", "evil"],
                "compiler argument '-wrapper'",
            ),
            (
                &["g++", "-c", "src/foo.cpp", "@args.rsp"],
                "compiler argument '@args.rsp'",
            ),
            (&["g++", "-c", "-"], "stdin"),
            (&["g++", "-c"], "requires an input file"),
            (
                &["clang++", "-c", "src/foo.cpp", "-Xclang", "-load"],
                "compiler argument '-Xclang'",
            ),
            (
                &["clang++", "-c", "src/foo.cpp", "-Xlinker", "-e"],
                "compiler argument '-Xlinker'",
            ),
            (
                &["clang++", "-c", "src/foo.cpp", "-Wl,-rpath,/tmp"],
                "compiler argument '-Wl,-rpath,/tmp'",
            ),
            (
                &[
                    "clang++",
                    "-c",
                    "src/foo.cpp",
                    "--serialize-diagnostics",
                    "out.dia",
                ],
                "compiler argument '--serialize-diagnostics'",
            ),
            (&["g++", "-c", "../outside.cpp"], "escapes the repository"),
            (&["g++", "-c", "/etc/passwd.cpp"], "escapes the repository"),
            (&["gcc", "-c", "../../outside.c"], "escapes the repository"),
        ];
        for (args, expected) in cases {
            assert_denied_both_modes(&argv(args), expected);
        }
    }

    /// `-B` means "build directory" to CMake and "program search prefix" to
    /// GCC/Clang. The two live in different arms; this test exists so a future
    /// refactor cannot merge them.
    #[test]
    fn cmake_and_compiler_disagree_about_dash_b() {
        assert_allowed_both_modes(&argv(&["cmake", "-B", "build"]));
        assert_denied_both_modes(
            &argv(&["g++", "-c", "src/foo.cpp", "-B", "build"]),
            "compiler argument '-B'",
        );
    }

    #[test]
    fn cap_output_truncates_at_limit() {
        let big = "x".repeat(crate::source::MAX_READ_BYTES + 4096);
        let out = cap_output(&big);
        assert!(out.contains("[truncated"), "got: {out}");
        assert!(out.len() < big.len());
    }

    #[test]
    fn truncate_tool_output_reports_original_size() {
        let big = "y".repeat(MAX_TOOL_RESULT_BYTES + 2048);
        let out = truncate_tool_output(&big);
        assert!(
            out.contains(&format!("{} bytes total", big.len())),
            "got: {out}"
        );
        assert!(out.len() <= MAX_TOOL_RESULT_BYTES + 64);
    }

    #[tokio::test]
    async fn execute_captured_times_out_and_kills_child() {
        let cwd = std::env::temp_dir();
        let outcome = execute_captured(&argv(&["sleep", "30"]), &cwd, Duration::from_secs(1))
            .await
            .expect("sleep runs");
        assert!(outcome.timed_out, "expected a timeout");
        assert_eq!(outcome.exit_code, None);
        assert!(outcome.duration_ms >= 1000);
    }

    /// A real git repository (with an initial commit) for command tests.
    fn git_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hanihi-cmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git runs")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn run_command_tool_runs_git_and_writes_trace() {
        let repo = git_repo();
        let tree = Arc::new(SourceTree::open_at(&repo).expect("open repo"));
        let traces_dir = repo.join("traces");
        let tool = builtin_run_command(tree, traces_dir.clone());

        let out = tool
            .execute(serde_json::json!({ "command": "git status --short" }))
            .await
            .expect("git status succeeds")
            .render();
        assert!(out.contains("exit code: 0"), "got: {out}");
        assert!(out.contains("trace:"), "got: {out}");

        // A trace file was written.
        let mut found = false;
        for entry in std::fs::read_dir(&traces_dir).expect("traces dir") {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().ends_with(".cmd.json") {
                let raw = std::fs::read_to_string(entry.path()).unwrap();
                assert!(raw.contains("\"command\": \"git status --short\""));
                found = true;
            }
        }
        assert!(found, "expected a trace file in {}", traces_dir.display());
        std::fs::remove_dir_all(&repo).unwrap_or(());
    }

    #[tokio::test]
    async fn run_command_tool_denies_disallowed_command() {
        let repo = git_repo();
        let tree = Arc::new(SourceTree::open_at(&repo).expect("open repo"));
        let tool = builtin_run_command(tree, repo.join("traces"));

        let err = tool
            .execute(serde_json::json!({ "command": "git push origin main" }))
            .await
            .expect_err("git push must be denied");
        assert!(err.to_string().contains("not allowed"), "got: {err}");
        std::fs::remove_dir_all(&repo).unwrap_or(());
    }

    // ── commit messages from a file ──
    //
    // `run_command` splits its input on whitespace and does no quote
    // processing, so a multi-word `-m` reaches git as several argv elements
    // and git reads the extras as pathspecs. A path is one token, so `-F`
    // carries a multi-line message through unharmed.

    #[test]
    fn command_allowlist_accepts_commit_message_from_file() {
        let cases = [
            argv(&["git", "commit", "-F", "working/commit-msg.txt"]),
            argv(&["git", "commit", "--file", "working/commit-msg.txt"]),
            argv(&["git", "commit", "-F", "working/msg.txt", "--amend"]),
            argv(&["git", "commit", "--amend", "-F", "working/msg.txt"]),
        ];
        for cmd in cases {
            check_command_argv_mode(&cmd, CommandMode::Write)
                .unwrap_or_else(|e| panic!("write mode must allow {cmd:?}: {e}"));
        }
    }

    #[test]
    fn command_allowlist_rejects_absolute_message_file() {
        let cases = [
            argv(&["git", "commit", "-F", "/tmp/msg.txt"]),
            argv(&["git", "commit", "--file", "/etc/passwd"]),
        ];
        for cmd in cases {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("absolute message file must be refused: {cmd:?}"));
            assert!(
                err.contains("working/"),
                "refusal should name the permitted location, got: {err}"
            );
        }
    }

    #[test]
    fn command_allowlist_rejects_escaping_message_file() {
        let cases = [
            argv(&["git", "commit", "-F", "../msg.txt"]),
            argv(&["git", "commit", "-F", "working/../../msg.txt"]),
            argv(&["git", "commit", "--file", "../secrets"]),
        ];
        for cmd in cases {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("escaping message file must be refused: {cmd:?}"));
            assert!(err.contains("working/"), "got: {err}");
        }
    }

    #[test]
    fn command_allowlist_rejects_protected_message_file() {
        let cases = [
            argv(&["git", "commit", "-F", ".ignore"]),
            argv(&["git", "commit", "-F", ".gitignore"]),
            argv(&["git", "commit", "-F", ".git/config"]),
        ];
        for cmd in cases {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("protected message file must be refused: {cmd:?}"));
            assert!(
                err.contains("working/") || err.contains("protected"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn command_allowlist_requires_a_file_after_dash_f() {
        for cmd in [
            argv(&["git", "commit", "-F"]),
            argv(&["git", "commit", "--file"]),
        ] {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("-F with no path must be refused: {cmd:?}"));
            assert!(
                err.contains("requires a file path"),
                "the refusal must name the missing argument, got: {err}"
            );
        }
    }

    #[test]
    fn command_allowlist_rejects_message_flag_and_file_together() {
        let cases = [
            argv(&["git", "commit", "-m", "x", "-F", "working/msg.txt"]),
            argv(&["git", "commit", "-F", "working/msg.txt", "-m", "x"]),
            argv(&[
                "git",
                "commit",
                "--message",
                "x",
                "--file",
                "working/msg.txt",
            ]),
        ];
        for cmd in cases {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("-m with -F must be refused: {cmd:?}"));
            assert!(
                err.contains("mutually exclusive"),
                "the refusal must name the conflict, got: {err}"
            );
        }
    }

    #[test]
    fn command_allowlist_rejects_stdin_message_file() {
        for cmd in [
            argv(&["git", "commit", "-F", "-"]),
            argv(&["git", "commit", "--file", "-"]),
        ] {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("-F - must be refused: {cmd:?}"));
            assert!(
                err.contains("stdin"),
                "the refusal must say stdin is not a file, got: {err}"
            );
        }
    }

    /// Every `git commit` shape admitted before `-F` existed must still be
    /// admitted. The `-F` work extends the gate; it must not narrow it.
    #[test]
    fn command_allowlist_keeps_bare_and_message_commits() {
        let cases = [
            argv(&["git", "commit"]),
            argv(&["git", "commit", "-m", "subject"]),
            argv(&["git", "commit", "--message", "subject"]),
            argv(&["git", "commit", "--amend"]),
            argv(&["git", "commit", "--amend", "--no-edit"]),
            argv(&["git", "commit", "--amend", "-m", "reword"]),
            argv(&["git", "commit", "--amend", "--message", "reword"]),
            argv(&["git", "commit", "--no-edit", "--amend"]),
        ];
        for cmd in cases {
            check_command_argv_mode(&cmd, CommandMode::Write)
                .unwrap_or_else(|e| panic!("must stay admitted {cmd:?}: {e}"));
        }
        // Read-only mode still refuses every commit shape.
        for cmd in [
            argv(&["git", "commit"]),
            argv(&["git", "commit", "-F", "working/msg.txt"]),
        ] {
            assert!(
                check_command_argv_mode(&cmd, CommandMode::ReadOnly).is_err(),
                "read-only mode must deny: {cmd:?}"
            );
        }
    }

    /// A literal quote in an `-m` value is a positive sign that the caller
    /// expected the harness to strip quotes. Refuse it with guidance toward
    /// `-F` instead of letting git report a baffling pathspec error.
    ///
    /// Deliberately narrow: only a value that *is* a quote character is
    /// refused, so a legitimate one-token message is never blocked.
    #[test]
    fn commit_with_a_literal_quote_in_message_is_refused_with_guidance() {
        let cases = [
            argv(&["git", "commit", "-m", "\""]),
            argv(&["git", "commit", "-m", "'"]),
            argv(&["git", "commit", "--message", "\""]),
        ];
        for cmd in cases {
            let err = check_command_argv_mode(&cmd, CommandMode::Write)
                .expect_err(&format!("quoted -m value must be refused: {cmd:?}"));
            assert!(
                err.contains("does not process quotes") && err.contains("-F"),
                "the refusal must explain the limitation and name -F, got: {err}"
            );
        }

        // An empty value is the same mistake and gets the same guidance.
        let err = check_command_argv_mode(&argv(&["git", "commit", "-m", ""]), CommandMode::Write)
            .expect_err("empty -m value must be refused");
        assert!(err.contains("-F"), "got: {err}");

        // A one-token message is untouched, even one containing a quote.
        check_command_argv_mode(&argv(&["git", "commit", "-m", "it's"]), CommandMode::Write)
            .expect("an ordinary one-token message must stay admitted");
    }

    /// The point of the whole plan: a message with a subject, a blank line, a
    /// body, and a trailing line survives the round trip through `-F` with its
    /// newlines intact. Verified by reading the message back from `git log`,
    /// not by inspecting argv.
    #[tokio::test]
    async fn commit_message_from_file_lands_a_multi_line_message() {
        let repo = git_repo();
        std::fs::create_dir_all(repo.join("working")).unwrap();
        let message = "Add a staged-text channel for commit messages\n\
                       \n\
                       The harness splits run_command on whitespace and does no quote\n\
                       processing, so a multi-word -m arrives as several pathspecs.\n\
                       \n\
                       Hānihi\n";
        std::fs::write(repo.join("working/commit-msg.txt"), message).unwrap();

        let tree = Arc::new(SourceTree::open_at(&repo).expect("open repo"));
        let tool = builtin_run_command_write(tree, repo.join("traces"));
        std::fs::write(repo.join("src/extra.rs"), "// extra\n").unwrap();

        let out = tool
            .execute(serde_json::json!({ "command": "git add -A" }))
            .await
            .expect("add succeeds");
        assert!(
            out.render().contains("exit code: 0"),
            "got: {}",
            out.render()
        );

        let out = tool
            .execute(serde_json::json!({
                "command": "git commit -F working/commit-msg.txt"
            }))
            .await
            .expect("commit succeeds");
        assert!(
            out.render().contains("exit code: 0"),
            "got: {}",
            out.render()
        );

        let shown = std::process::Command::new("git")
            .args(["log", "-1", "--format=%B"])
            .current_dir(&repo)
            .output()
            .expect("git log runs");
        let body = String::from_utf8_lossy(&shown.stdout).into_owned();
        assert!(
            body.contains("Add a staged-text channel for commit messages\n\n"),
            "subject and blank line must survive, got: {body:?}"
        );
        assert!(
            body.contains("arrives as several pathspecs.\n\nHānihi"),
            "body paragraphs and the closing line must keep their newlines, got: {body:?}"
        );
        std::fs::remove_dir_all(&repo).unwrap_or(());
    }
}
