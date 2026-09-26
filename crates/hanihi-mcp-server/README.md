# MCP Server

For [Hānihi](https://github.com/worikgh/hanihi)

Produces two binaries:
1. `hanihi-mcp-server-ro`  no tools that write to disc
2. `hanihi-mcp-server-rw`  tools that write to disc


## Tools TODO for a Rust Coding Harness

A small set of **high-level, typed, Rust-aware tools**

1. **Repository inspection**
2. **Precise file editing**
3. **Compilation and tests**
4. **Semantic code navigation**
5. **Diagnostics and dependency inspection**
6. **Explicitly gated version-control operations**

## Recommended tools

### Repository and file context

- `workspace_info`
  - Returns workspace root, crate list, Rust edition, active toolchain, target triples, and important configuration files.
  **Done**

- `list_files`
  - Supports glob filters, exclusions, maximum depth, and result limits.
  - Exclude `.git`, `target`, generated files, and vendored dependencies by default.

- `read_file`
  - Read a file or line range.
  - Include line numbers and optionally a hash/version ID.

- `search_text`
  - Regex or literal search with file globs.
  - Return matching lines plus a small context window.
  **Done**

- `find_symbol`
  - Find definitions and references for functions, structs, traits, enums, modules, and macros.
  - Ideally backed by `rust-analyzer` rather than text search.
  **Done**

A useful response from `read_file` should look like:

```text
src/parser.rs:41-68
41  pub fn parse(input: &str) -> Result<Ast, Error> {
...
```

Line-numbered output substantially improves edit reliability.

### Editing

Prefer one structured editing tool over several ambiguous ones:

```text
apply_patch({
  file: "src/parser.rs",
  expected_version: "...",
  patch: "..."
})
```

It should:

- Require the expected file hash or version.
- Fail if the patch context does not match.
- Return the resulting diff.
- Refuse edits outside the workspace.
- Support multiple files atomically if possible.
- Never silently overwrite concurrent changes.
  **Done**

You may also want:

- `create_file`
  **Done**
- `delete_file`
  **Done**
- `rename_file`
  **Done**

But make these separate and clearly destructive. Avoid a generic `write_file` initially; models are much more likely to accidentally replace a whole file with one.

### Rust-aware analysis

- `rust_analyzer_diagnostics`
  - Return errors, warnings, locations, severity, and diagnostic messages.
  - Support a specific file or the whole workspace.

- `rust_analyzer_hover`
  - Type information and documentation at a file position.

- `rust_analyzer_definition`
  - Jump to the definition at a position.

- `rust_analyzer_references`
  - Find usages of a symbol.

- `rust_analyzer_code_actions`
  - Offer or apply actions such as import insertion, qualification fixes, and other safe refactorings.

If implementing the full `rust-analyzer` protocol is too much, start with diagnostics, definitions, references, and symbol search. Those provide most of the value.

### Build, test, and validation

Expose Cargo operations as typed tools rather than making the model construct arbitrary commands:

- `cargo_check`
  - Parameters: package, features, target, all-features, all-targets, locked/offline mode.

- `cargo_test`
  - Parameters: package, test name/filter, features, `--no-fail-fast`, target.
  - Return structured test results as well as stdout/stderr.

- `cargo_clippy`
  - Support package, features, all-targets, and optionally `-D warnings`.

- `cargo_fmt_check`
  - Check formatting without modifying files.

- `cargo_fmt`
  - Separate write-capable formatting tool.

- `cargo_build`
  - Useful when the task involves binaries, release builds, target-specific compilation, or build scripts.

- `cargo_doc_check`
  - Run documentation tests and optionally check missing documentation.

Each tool should return:

```json
{
  "success": false,
  "exit_code": 101,
  "diagnostics": [
	{
	  "file": "src/lib.rs",
	  "line": 27,
	  "column": 13,
	  "severity": "error",
	  "message": "mismatched types"
	}
  ],
  "stdout": "...",
  "stderr": "..."
}
```

Parsing compiler output into structured diagnostics is much better than returning raw terminal output alone.

A particularly useful compound tool is:

```text
validate_changes({
  checks: ["fmt", "check", "clippy", "test"],
  package: "...",
  features: [...]
})
```

It should run only requested checks and stop or continue according to an explicit policy. This gives the harness a standard “verify my changes” operation.

### Dependency and project inspection

- `cargo_metadata`
  - Return packages, targets, features, workspace members, and dependency relationships.

- `cargo_tree`
  - Inspect dependency versions and feature activation.

- `cargo_audit`
  - Optional, if security review is in scope.

- `read_cargo_manifest`
  - A specialized, parsed view of `Cargo.toml` is easier for a model to use than raw text.

- `query_docs`
  - Search local rustdoc output, project documentation, or selected dependency documentation.

Do not automatically give the model unrestricted internet access for crate documentation. A controlled documentation tool is easier to audit and keeps the context focused.

### Git and change management

Useful read-only tools:

- `git_status`
- `git_diff`
- `git_diff_file`
- `git_log`
- `git_blame`

Potentially useful but gated write tools:

- `git_apply_patch`
- `git_stage`
- `git_commit`
- `git_restore`

I would not expose `git_reset`, force-push, branch deletion, or arbitrary Git commands through the initial server. Let the harness or human handle those.

`git_diff` should be one of the model’s easiest tools to call. A good coding loop is:

1. Inspect repository.
2. Read relevant code.
3. Apply a small patch.
4. Inspect the diff.
5. Run diagnostics.
6. Run focused tests.
7. Run broader validation.
8. Summarize remaining issues.

## Avoid starting with a generic shell tool

A tool like this is convenient:

```text
run_command(command: string)
```

…but it creates several problems:

- The model can invoke commands unrelated to the task.
- Shell quoting and platform differences become your problem.
- It can access files outside the workspace.
- It can exfiltrate credentials or environment variables.
- It makes permissions and auditing vague.
- The model receives less structured feedback.

If you need one for escape hatches, make it tightly constrained:

```text
run_allowed_command({
  program: "cargo",
  args: ["test", "-p", "parser"],
  cwd: "workspace",
  timeout_ms: 120000
})
```

Use an executable allowlist such as `cargo`, `rustc`, `rustfmt`, and perhaps a few project-approved scripts. Reject shell syntax, pipes, redirections, command substitution, absolute paths, and environment overrides by default. Run it in a sandbox with resource limits.

## Tool annotations and permissions

Classify tools clearly:

| Category | Examples | Permission |
|---|---|---|
| Read-only | `read_file`, `search_text`, `git_diff` | Automatic |
| Local write | `apply_patch`, `cargo_fmt` | Usually automatic or user-approved |
| Resource-intensive | `cargo_test`, `cargo_build`, `cargo_clippy` | Timeout and resource limits |
| Destructive | `delete_file`, `git_restore` | Explicit approval |
| External side effect | `git_push`, publishing crates | Do not expose initially |

MCP tool annotations such as read-only and destructive hints are useful metadata, but enforce permissions in your server rather than relying on the client to interpret them. Current MCP guidance also recommends precise schemas, least privilege, input validation, and accurate tool annotations. <citation src="1"></citation>

## Resources and prompts

Not everything should be a tool.

Expose stable context as **resources**, for example:

- `rust-project://workspace`
- `rust-project://cargo-metadata`
- `rust-project://git-diff`
- `rust-project://diagnostics`
- `rust-project://file/src/lib.rs`

Resources are appropriate for data the model reads. Tools are appropriate for actions.

Useful **prompts** include:

- `implement_issue`
- `debug_failing_test`
- `review_current_diff`
- `refactor_symbol`
- `prepare_pull_request`

For example, `debug_failing_test` could instruct the harness to gather the failing test output, relevant source, recent diff, and diagnostics before asking the model to propose a fix.

## A good first version

I would start with these 14 capabilities:

```text
workspace_info
list_files
read_file
search_text
apply_patch
git_status
git_diff
rust_analyzer_diagnostics
cargo_metadata
cargo_check
cargo_test
cargo_clippy
cargo_fmt_check
validate_changes
```

Then add:

```text
find_symbol
rust_analyzer_definition
rust_analyzer_references
cargo_tree
cargo_fmt
create_file
rename_file
```

Only later consider:

```text
run_allowed_command
git_commit
cargo_audit
networked documentation search
```

The key architectural choice is to make the server return **structured, bounded results**. Limit file sizes, search results, diagnostic counts, test output, and command duration. Include truncation indicators and continuation parameters so the model can deliberately request more context instead of receiving an enormous response.

Finally, treat the server as capable of executing code and accessing the repository. Run it with minimal OS privileges, restrict filesystem access to the workspace, isolate build processes where practical, and keep network access disabled unless a specific tool needs it. Local MCP servers can access files and execute commands, so sandboxing and least privilege are important design requirements. <citation src="2"></citation>
