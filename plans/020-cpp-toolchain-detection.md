You are Hānihi working on this Rust repository. You may edit tracked files
and make local git commits — never push. This is plan 1 of 5 in a series
that makes Hānihi work well in a C++ project. This plan is self-contained:
implement it even if none of the other four exist yet, and do not modify
anything in anticipation of them.

## Objective

Teach `SourceTree` to record which build toolchain the enclosing repository
uses, so later work can branch on it. Today `SourceTree` knows only how to
pick an `.ignore` template from a language guess, and nothing else in the
harness can ask "is this a Rust repo or a C++ repo?". Add exactly one piece
of new state: a detected toolchain, exposed on `SourceTree`.

Do **not** change the command allowlist, the system preamble, or the eval
assertion types in this plan. Those are separate plans. The only deliverable
here is detection plus the accessor.

## Read first, verbatim

- `crates/hanihi-core/src/source.rs`
  - `Language` enum (~28-52) and `Language::template` (~34-50) — the existing
    marker-file vocabulary and `.ignore` templates. `C` currently means
    "C or C++".
  - `detect_languages` (~330-372) — the existing two-level marker/extension
    scan. It already runs on every `SourceTree::open_at` via
    `ensure_ignore_file`.
  - `ensure_ignore_file` (~296-328) — the caller of `detect_languages`.
  - `SourceTree` struct (~98-102) and `open_at` (~110-122) — where new state
    would live and where the detection pass already happens.
  - `SourceTree::root` (~125-127) — the accessor shape to mirror.
  - `mod testutil` (~376-412) — the `Fixture` temp-dir pattern all new tests
    must use.
- `crates/hanihi-core/src/lib.rs` — what `source` re-exports publicly.
- `crates/hanihi-core/src/tool.rs`
  - `builtin_list_dir` / `builtin_grep` — the only current consumers of
    `SourceTree`; confirm neither needs to change.

## Findings (confirmed)

1. `detect_languages` walks with `WalkBuilder`, `max_depth(2)`,
   `standard_filters(true)`, and matches `Cargo.toml` against the `Rust`
   language plus `CMakeLists.txt | Makefile | meson.build` and C-family
   extensions against a single `has_c` flag. C++ is not distinguished from C.
2. `SourceTree` holds only `root` and `matcher`. There is no place to ask
   about build tooling.
3. `ensure_ignore_file` calls `detect_languages` and then discards the
   result. That pass is already paid for on every open, so reusing it costs
   nothing extra — but note it currently runs *before* the matcher is built
   and its result is not returned from `open_at`.
4. Detection is entirely filesystem-driven and has no notion of precedence:
   if both `Cargo.toml` and `CMakeLists.txt` exist, `detect_languages`
   returns `[Rust, C]` and both templates are emitted.

## Design

### A. `Toolchain` enum

Add to `source.rs`, next to `Language`:

```rust
/// The build toolchain a repository uses, as detected from marker files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    /// `Cargo.toml` present — Rust workspace, driven by `cargo`.
    Cargo,
    /// `CMakeLists.txt` present — CMake project, driven by `cmake`.
    CMake,
    /// No recognised build system. Read/edit tools still work; build
    /// commands are not offered.
    Unknown,
}
```

`Cargo` is listed first and `derive`d `PartialEq`; do not implement `Ord`
and do not rely on declaration order anywhere — write an explicit precedence
function instead (see B).

Add a doc-commented constant for the marker file names so detection and any
future consumer share one list:

```rust
/// Marker file that identifies a Cargo workspace.
const CARGO_MARKER: &str = "Cargo.toml";
/// Marker file that identifies a CMake project.
const CMAKE_MARKER: &str = "CMakeLists.txt";
```

`Makefile` and `meson.build` must **not** imply `CMake`. A bare `Makefile`
is not a CMake project, and a `meson.build` is a third toolchain this plan
deliberately does not model. Keep them in `detect_languages`'s C/C++
*language* detection (which is about which `.ignore` template to emit) and
leave them out of `Toolchain` entirely.

### B. Precedence

If both `CARGO_MARKER` and `CMAKE_MARKER` exist, Cargo wins. Rationale: a
`Cargo.toml` at or above the repo root means the project is a Cargo
workspace; a `CMakeLists.txt` sitting alongside it is almost always a
vendored dependency, a test fixture, or a C wrapper crate. Cargo also has the
richer and safer command surface, so the conservative choice is the one that
admits fewer commands.

Encode this as a small pure function, not an `if` buried in the walk:

```rust
/// Detect the repository's build toolchain from marker files.
///
/// Both markers present is resolved in favour of Cargo (see the plan's
/// rationale): a CMakeLists.txt inside a Cargo workspace is usually a
/// vendored dependency or fixture, not the project's build system.
fn detect_toolchain(root: &Path) -> Toolchain
```

`detect_toolchain` must be **pure and independently testable** — it takes a
`&Path`, performs its own filesystem check, and has no dependency on
`SourceTree`, `Language`, or any walker state. Keep it simple: check for the
two marker files. Do not extend the recursive two-level scan to toolchain
detection. A `CMakeLists.txt` two levels down inside `vendor/` must not
reclassify a Cargo repo, and a top-level check is both sufficient for real
projects and far easier to reason about.

Use `root.join(CARGO_MARKER).exists()` style checks (the pattern already used
for `.git` in `find_repo_root` and for the ignore files in `build_matcher`).
Do not canonicalize here; `open_at` has already canonicalized `root`.

### C. Store it on `SourceTree`

Add a private field and a public accessor, mirroring `root`:

```rust
pub struct SourceTree {
    root: PathBuf,
    matcher: Gitignore,
    toolchain: Toolchain,
}

impl SourceTree {
    /// The build toolchain detected for this repository.
    pub fn toolchain(&self) -> Toolchain {
        self.toolchain
    }
}
```

In `open_at`, compute the toolchain via `detect_toolchain(&root)` after
canonicalization and store it. Order relative to `ensure_ignore_file` does
not matter for correctness; put the detection first so the struct's fields
are all known before any filesystem writes happen.

### D. Do not touch `detect_languages`

`detect_languages` keeps its current behaviour and its `Language::C`
meaning ("C or C++"). Refactoring it to reuse `detect_toolchain` would
change the `.ignore` template emitted for C++ repos, which is plan 5's job.
Leave it alone. If you find yourself wanting to "unify" the two detections,
stop — they answer different questions (which ignore patterns to write vs.
which build commands to offer) and merging them couples two independent
policies.

### E. Re-export

Export `Toolchain` from `hanihi_core` alongside the existing `source` items
so plans 2-4 can name it without reaching into internals. Check
`crates/hanihi-core/src/lib.rs` for the established re-export style and
follow it exactly. Do not export `detect_toolchain` — it stays private.

## Work order, test-first

Write the failing tests first, then implement A-E, then run the gates.

### New tests in `source.rs` `mod tests`

Extend `testutil::Fixture` if convenient, but prefer new small local helpers
that create only the markers a given test needs — `Fixture::new` writes a
`Cargo.toml` unconditionally, which would make a CMake case impossible.
Add a minimal helper such as:

```rust
/// A git repo containing only the given marker files.
fn repo_with_markers(markers: &[&str]) -> PathBuf
```

creating a temp dir with `.git/` plus each named marker file (empty contents
are fine), using the `uuid` + `std::env::temp_dir()` pattern already in the
module, and removing it on completion the way the existing tests do.

Tests:

1. `detect_toolchain_finds_cargo` — only `Cargo.toml` → `Toolchain::Cargo`.
2. `detect_toolchain_finds_cmake` — only `CMakeLists.txt` → `Toolchain::CMake`.
3. `detect_toolchain_prefers_cargo_when_both_present` — both markers →
   `Toolchain::Cargo`. This encodes the precedence decision so a future
   change cannot silently flip it.
4. `detect_toolchain_is_unknown_without_markers` — `.git/` and a stray
   `src/main.rs` but no marker → `Toolchain::Unknown`.
5. `detect_toolchain_ignores_a_makefile` — a bare `Makefile` → `Unknown`.
   This is the assertion that stops a non-CMake project from being handed
   `cmake` commands in plan 2.
6. `detect_toolchain_ignores_a_nested_cmake_lists` — `Cargo.toml` at the
   root plus `vendor/dep/CMakeLists.txt` → `Cargo`. Proves the check is
   top-level and that a vendored CMake project cannot reclassify the repo.
7. `open_at_records_the_detected_toolchain` — build a repo with each marker
   set in turn, call `SourceTree::open_at`, assert `tree.toolchain()`. This
   is the only test that exercises the wiring, and it must not assume
   `Fixture` (which always writes `Cargo.toml`).

Also assert the accessor does not disturb existing behaviour: the existing
`ensure_ignore_file_bootstraps_rust_template` and
`walk_excludes_ignored_paths` tests must pass unchanged.

## Verification gates

```text
cargo fmt
cargo test -p hanihi-core
cargo clippy -p hanihi-core --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acceptance criteria

- `SourceTree::toolchain()` returns the detected `Toolchain` for both marker
  types, `Unknown` otherwise, and `Cargo` when both markers are present.
- `detect_toolchain` is private, pure, and directly unit-tested; the
  accessor is public and documented.
- A bare `Makefile` or `meson.build` does **not** produce `Toolchain::CMake`.
- `detect_languages`, the `.ignore` templates, `Language`, the command
  allowlist, the system preamble, and every eval assertion type are
  byte-for-byte unchanged.
- No new dependencies.
- No behaviour change for Rust repositories: every existing test passes.

## Out of scope

- Emitting `cmake`/`make`/compiler commands — plan 2.
- Changing the `.ignore` template for C/C++ repos — plan 5.
- Making the system preamble toolchain-aware — plan 3.
- Toolchain-parameterised eval assertions — plan 4.
- Detecting a compiler, a CMake version, an existing build directory, or a
  `compile_commands.json`. Detection is marker-file-only.
- Modelling Meson, Bazel, Autotools, or any toolchain other than Cargo and
  CMake. `Unknown` is the correct answer for those.
- A CLI flag to override the detected toolchain. Detection is authoritative
  in this series; an override flag is a future concern and would need its
  own plan.

## Assumptions and risks

- **Top-level-only detection is a deliberate simplification.** A CMake
  project with its only `CMakeLists.txt` in a subdirectory is misdetected as
  `Unknown`. Accepted: the safe failure mode is *fewer* commands offered,
  and the marker at the repo root is the overwhelmingly common layout. Note
  this in the `detect_toolchain` doc comment.
- **Precedence is a policy choice, not a fact.** Cargo-wins is documented
  and test-locked so it can be revisited deliberately rather than
  accidentally.
- **`open_at` gains one more filesystem probe.** Two `exists()` calls on a
  path that is already being walked by `ensure_ignore_file`. Negligible, and
  no new I/O is introduced.
- **`Toolchain` is a new public enum.** Adding a variant later is a breaking
  change for exhaustive matches in downstream crates. Keep it to three
  variants; plans 2-4 should match on it non-exhaustively where practical
  and must not depend on variant *order*.
