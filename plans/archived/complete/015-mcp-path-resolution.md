You are Hānihi working on this Rust repository. You may edit tracked files
and make local git commits — never push. Fix the path-resolution defects in
`crates/hanihi-mcp-server` and add regression tests. Do not modify
`hanihi-core`.

## Objective

1. Make `workspace_fs::workspace_root()` return the actual Cargo workspace
   root instead of assuming it equals the process working directory.
2. Make the `path` argument of `search_text` and `find_symbol` refuse paths
   that escape the workspace: absolute paths, `..` traversal, and symlinks
   that resolve outside the root.
3. Reclassify caller mistakes in the file tools from internal error
   (`-32603`) to invalid params (`-32602`).

## Read first, verbatim

- `crates/hanihi-mcp-server/src/workspace_fs.rs`
  - `workspace_root` (~81-84) — the CWD conflation.
  - `resolve_workspace_path` (~89-125) — the existing file-path resolver
    whose validation pattern the new directory resolver should mirror.
  - `normalize_relative` — for the lexical path-component handling already
    in use.
- `crates/hanihi-mcp-server/src/search_text_tool.rs`
  - `resolve_root` (~186-207) — unsanitized `base.join(relative)`.
- `crates/hanihi-mcp-server/src/find_symbol_tool.rs`
  - `resolve_root` (~200-220) — same defect, duplicated.
- `crates/hanihi-mcp-server/src/read_file_tool.rs` (~43-45) — already uses
  `workspace_fs::workspace_root()`.
- `crates/hanihi-mcp-server/src/create_file_tool.rs` (~55-57, 69-76),
  `delete_file_tool.rs` (~41-43, 50-65), `rename_file_tool.rs` (~46-50,
  61-90) — the error-code misclassifications.
- `crates/hanihi-mcp-server/src/apply_patch_tool.rs`
  - `apply_edits` (~209-211) — uses `std::env::current_dir()` directly.
- `crates/hanihi-mcp-server/src/workspace_info_tool.rs`
  - `cargo_metadata` / `workspace_root` (~74-82) — the existing authority
    for the true Cargo workspace root.
- `crates/hanihi-core/src/source.rs`
  - `find_repo_root` and `resolve_for_write` — refusal patterns to mirror;
    read only, do not modify.

## Findings (confirmed)

1. `workspace_fs::workspace_root()` returns `std::env::current_dir()`
   unchanged. This is only correct when the MCP server is spawned with its
   cwd at the repo root. `workspace_info_tool` already derives the real root
   from `cargo metadata`, so a server started from a subdirectory would make
   the file tools resolve against the wrong base while `workspace_info`
   reports the true root.

2. `search_text_tool::resolve_root` and `find_symbol_tool::resolve_root`
   compute `base.join(relative)` and only check `is_dir()`. `PathBuf::join`
   replaces the base for absolute paths and does not sanitize `..`, and
   `is_dir()` does not reject symlinks that point outside the workspace.
   Consequences: `path: "/etc"` searches `/etc`, `path: ".."` searches
   above the repo, and a symlinked directory inside the repo can walk
   outside it.

3. `create_file`, `delete_file`, and `rename_file` report caller mistakes
   ("is a directory", "file already exists", "does not exist") as internal
   errors (`-32603`). They are invalid parameters (`-32602`); `read_file`
   already classifies the analogous cases correctly.

## Design

### A. Workspace root discovery (`workspace_fs.rs`)

Add a cached `workspace_root()`:

```rust
use std::sync::OnceLock;

static WORKSPACE_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn workspace_root() -> Result<PathBuf, ToolError> {
    if let Some(root) = WORKSPACE_ROOT.get() {
        return Ok(root.clone());
    }
    let root = discover_workspace_root()?;
    // A racing duplicate set cannot produce a different value: the process
    // cwd and the workspace layout are fixed for the server's lifetime.
    let _ = WORKSPACE_ROOT.set(root.clone());
    Ok(root)
}
```

`discover_workspace_root()`:

1. `std::env::current_dir()` (map to `internal` on error).
2. Run `cargo metadata --no-deps --format-version 1` — the same invocation
   as `workspace_info_tool::cargo_metadata` — and extract `workspace_root`;
   canonicalize it and require `is_dir`.
3. If cargo fails (missing binary, not a Cargo workspace), fall back to
   `marker_root(&cwd)`.
4. Otherwise return `internal("cannot discover workspace root: ...")`.

`marker_root(start: &Path) -> Option<PathBuf>` is pure and testable: walk up
from `start` to the nearest ancestor containing `Cargo.toml`; if none, the
nearest ancestor containing `.git` (directory or file, covering worktrees);
if none, `start` itself; canonicalize the result.

Rationale:

- `cargo metadata` is already the authority for the workspace root in this
  crate, and it correctly resolves a member-crate cwd to the enclosing
  workspace root — marker-file walking alone cannot do that reliably.
- `OnceLock` makes the subprocess cost one-time per server process. The
  server never changes cwd, so the cached value stays correct.
- The marker fallback keeps a usable root when cargo is unavailable, at the
  cost of being approximate (documented below).

### B. Search/find root resolution (`workspace_fs.rs` + both tools)

Add a shared resolver for the existing-directory case:

```rust
pub(crate) fn resolve_directory(root: &Path, relative: &str) -> Result<PathBuf, String>
```

Behaviour, in order:

1. Reject absolute paths.
2. Reject empty/whitespace-only paths.
3. Reject any `ParentDir`, `RootDir`, or `Prefix` component. Rejecting all
   `..` is stricter than allowing internal normalization; it matches
   `hanihi-core`'s `resolve_for_write` and keeps the rule simple.
4. `candidate = root.join(relative)`; require `candidate.is_dir()`, else
   the existing message "search path does not exist or is not a directory:
   {relative}".
5. `canonicalize()` both `root` and `candidate`; require
   `canonical.starts_with(&canonical_root)`, else "path escapes the
   workspace: {relative}".
6. Return the canonical candidate.

In both `search_text_tool.rs` and `find_symbol_tool.rs`, replace the
`resolve_root` body with a thin env wrapper plus a pure helper, mirroring
the existing `apply_edits`/`apply_edits_in` seam:

```rust
use crate::workspace_fs;

fn resolve_root(arguments: &Value) -> Result<PathBuf, ToolError> {
    let base = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
    resolve_root_from(&base, arguments)
}

fn resolve_root_from(base: &Path, arguments: &Value) -> Result<PathBuf, ToolError> {
    match arguments.get("path").and_then(Value::as_str) {
        None | Some("") => Ok(base.to_path_buf()),
        Some(relative) => workspace_fs::resolve_directory(base, relative).map_err(invalid),
    }
}
```

`resolve_root_from` is pure so it can be unit-tested against a temporary
base without touching the real cwd or the `OnceLock` cache.

### C. Apply the shared root to `apply_patch`

In `apply_patch_tool.rs::apply_edits`, replace
`std::env::current_dir()` with:

```rust
let root = workspace_fs::workspace_root().map_err(|error| internal(error.message))?;
```

The file tools (`read_file`, `create_file`, `delete_file`, `rename_file`)
already call `workspace_fs::workspace_root()`; they need no root-source
change.

### D. Error-code reclassification

Change these from `workspace_fs::internal` (`-32603`) to
`workspace_fs::invalid` (`-32602`):

- `create_file_tool.rs`
  - `{display_path} is a directory`
  - `file already exists: {display_path} (pass overwrite=true ...)`
- `delete_file_tool.rs`
  - `file does not exist: {display_path}`
  - `cannot delete: {display_path} is a directory`
- `rename_file_tool.rs`
  - `source file does not exist: {source_display}`
  - `cannot rename: {source_display} is a directory`
  - `destination already exists: {destination_display}`

Keep as `internal` the genuine server/infra failures: cannot inspect
(symlink metadata/IO), cannot create parent directories, cannot write /
remove / rename, serialization failures.

Optionally update the `path` descriptions in `search_text_tool::json` and
`find_symbol_tool::json` from "repository root" to "workspace root" so the
schema matches the new resolution authority. No schema `required` changes.

## Work order, test-first

Add the failing tests first (red), then implement A–D, then run the gates.

### New tests in `workspace_fs.rs` `mod tests`

Use `crate::workspace_fs::test_support::temp_dir` for filesystem fixtures.

1. `marker_root_prefers_nearest_cargo_toml_ancestor` — create
   `ws/Cargo.toml` + `ws/.git` + `ws/src/deep`; from `ws/src/deep` expect
   `ws`.
2. `marker_root_falls_back_to_git_then_start` — no `Cargo.toml` anywhere,
   `.git` present → `.git` ancestor; neither → the canonicalized start dir.
3. `resolve_directory_rejects_absolute_paths` — `/etc`, `/tmp/x` fail.
4. `resolve_directory_rejects_parent_traversal` — `../outside`,
   `a/../../outside`, and `src/../src` fail (the last proves all `..` are
   refused, not just escaping ones).
5. `resolve_directory_requires_an_existing_directory` — a regular-file
   target and a missing target both fail with "not a directory".
6. `resolve_directory_returns_canonical_subdirectory` — create `base/sub`;
   expect the canonicalized `base/sub`.
7. `#[cfg(unix)] resolve_directory_rejects_symlink_escape` — `base/link`
   symlinked to a directory outside `base` fails with "escapes".

### New tests in `search_text_tool.rs` and `find_symbol_tool.rs`

Test the pure seam with a temp base (no env, no cache):

- missing/empty `path` returns the base;
- `path: "sub"` returns the canonical subdirectory;
- `path: "/etc"`, `path: "../outside"`, and `path: "missing"` return an
  `invalid` (`-32602`) error.

### Updated tests

- `create_file_tool::refuses_an_existing_file_without_overwrite`:
  `-32603` → `-32602`; add a `refuses_a_directory_path` test.
- `delete_file_tool::a_missing_file_is_an_error` and
  `refuses_to_delete_a_directory`: `-32603` → `-32602`.
- `rename_file_tool::a_missing_source_is_an_error`,
  `refuses_an_existing_destination`, `refuses_a_directory_source`:
  `-32603` → `-32602`.

Do **not** add unit tests that call the cached `workspace_root()` directly;
test `marker_root`, `resolve_directory`, and `resolve_root_from` instead so
the tests stay hermetic and independent of the real cwd.

## Files to change

- `crates/hanihi-mcp-server/src/workspace_fs.rs`
  - add `OnceLock` cache, `discover_workspace_root`, `marker_root`,
    `resolve_directory`; add tests.
- `crates/hanihi-mcp-server/src/search_text_tool.rs`
  - `use crate::workspace_fs;`; split and reimplement `resolve_root`; add
    tests; optional description tweak.
- `crates/hanihi-mcp-server/src/find_symbol_tool.rs`
  - same as search_text.
- `crates/hanihi-mcp-server/src/apply_patch_tool.rs`
  - `apply_edits` uses `workspace_fs::workspace_root()`.
- `crates/hanihi-mcp-server/src/create_file_tool.rs`
  - error-code changes + one test.
- `crates/hanihi-mcp-server/src/delete_file_tool.rs`
  - error-code changes + test updates.
- `crates/hanihi-mcp-server/src/rename_file_tool.rs`
  - error-code changes + test updates.

No changes to `workspace_info_tool.rs` are required; it already reports the
cargo workspace root and may later be refactored to reuse the shared
discovery helper (optional cleanup, not in scope).

## Verification gates

```text
cargo fmt
cargo test -p hanihi-mcp-server
cargo clippy -p hanihi-mcp-server --all-targets -- -D warnings
cargo test --workspace
```

Manual smoke test after building:

1. From the repo root, run the `-ro` binary normally and confirm
   `read_file` / `search_text` behave as before for in-repo paths.
2. Run the binary with cwd set to `crates/hanihi-mcp-server` and confirm
   `read_file` for a repo-root-relative path still succeeds (proves root
   discovery).
3. Confirm `search_text` with `path: "/etc"`, `path: "../.."`, and a
   symlinked-out directory each returns an `invalid` (`-32602`) error.

## Assumptions and risks

- **Cargo metadata is the primary discovery.** First tool call spawns one
  `cargo metadata` subprocess; the result is cached for the process
  lifetime. If cargo is unavailable, the marker fallback is used, which is
  approximate for a cwd inside a member crate (nearest `Cargo.toml` is the
  member, not the workspace root). This is a documented fallback, not the
  expected path.
- **Rejecting all `..`** in `search_text`/`find_symbol` `path` is a
  deliberate strictness increase. A previously accepted internal traversal
  such as `crates/../crates/x` is now refused; clients should pass
  normalized relative paths.
- **Error-code change is observable.** Clients that distinguish `-32602`
  from `-32603` will now see invalid params for bad paths, which matches MCP
  semantics (caller mistake vs server fault). All in-repo tests asserting
  the old codes must be updated in the same change.
- **`OnceLock` and cwd.** The cache is only correct because the server never
  changes its working directory. Do not add `set_current_dir` anywhere in
  this crate.
- **Platform.** The symlink regression test is `#[cfg(unix)]`; Windows
  symlink creation needs privileges and is out of scope for this repo's CI.
- **Scope.** `hanihi-core` is untouched. `workspace_info_tool` keeps its own
  `cargo metadata` call because it needs the full metadata for crate/edition
  reporting; deduplicating it into `workspace_fs` is an optional follow-up,
  not part of this fix.
