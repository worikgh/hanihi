//! Source tree access: read and list code inside a git repository,
//! filtered by the repository's ignore rules (`.gitignore`, `.ignore`,
//! `.git/info/exclude`) via the [`ignore`] crate.
//!
//! The agent's view of a repository is restricted to what git itself
//! would track: ignored paths (`target/`, build directories, ...) are
//! invisible to both directory listing and targeted reads.
//!
//! hānihi maintains its own `.ignore` file at the repo root (see
//! [`ensure_ignore_file`]): same syntax as `.gitignore`, but git-agnostic
//! and managed by hānihi. The repo's `.gitignore` is respected but never
//! written.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, Walk, WalkBuilder};

/// Maximum number of bytes [`SourceTree::read`] returns for one file.
pub const MAX_READ_BYTES: usize = 64 * 1024;

/// Marker line used to recognise an existing hānihi-managed `.ignore`.
const HANIHI_HEADER: &str = "# hānihi-managed ignore file (agent read policy)\n";

/// Programming languages hānihi can recognise and write templates for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Rust — detected by `Cargo.toml`.
    Rust,
    /// C or C++ — detected by `CMakeLists.txt`/`Makefile`/C-family sources.
    C,
}

impl Language {
    /// `.ignore`-syntax patterns for generated artifacts of this language.
    pub fn template(self) -> &'static str {
        match self {
            Language::Rust => "target/\n**/*.rs.bk\n",
            Language::C => {
                "build/\ncmake-build-*/\nCMakeFiles/\n\
                 *.o\n*.obj\n*.a\n*.so\n*.dylib\n*.exe\n\
                 *.gcda\n*.gcno\n\
                 a.out\n.cache/\n\
                 # compile_commands.json — sometimes committed; useful for the agent\n"
            }
        }
    }
}

/// Errors produced by [`SourceTree`].
#[derive(Debug)]
pub enum SourceError {
    /// No repository found (no `.git` at or above the start path).
    NotARepository(PathBuf),
    /// The requested path does not exist.
    NotFound(PathBuf),
    /// The requested path resolves outside the repository root.
    Escape(PathBuf),
    /// The requested path is excluded by an ignore rule.
    Ignored(PathBuf),
    /// I/O failure.
    Io(io::Error),
    /// Error from the `ignore` crate (parsing, walking).
    Ignore(ignore::Error),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NotARepository(p) => {
                write!(f, "no git repository found at or above {}", p.display())
            }
            SourceError::NotFound(p) => write!(f, "no such path: {}", p.display()),
            SourceError::Escape(p) => write!(f, "path escapes the repository: {}", p.display()),
            SourceError::Ignored(p) => write!(f, "path is ignored: {}", p.display()),
            SourceError::Io(e) => write!(f, "io error: {e}"),
            SourceError::Ignore(e) => write!(f, "ignore error: {e}"),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SourceError::Io(e) => Some(e),
            SourceError::Ignore(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SourceError {
    fn from(e: io::Error) -> Self {
        SourceError::Io(e)
    }
}

impl From<ignore::Error> for SourceError {
    fn from(e: ignore::Error) -> Self {
        SourceError::Ignore(e)
    }
}

/// A git repository the agent may read, filtered by ignore rules.
#[derive(Debug)]
pub struct SourceTree {
    root: PathBuf,
    matcher: Gitignore,
}

impl SourceTree {
    /// Locate the repository containing the current directory and open it.
    pub fn open() -> Result<Self, SourceError> {
        let cwd = std::env::current_dir().map_err(SourceError::Io)?;
        let root = find_repo_root(&cwd).ok_or(SourceError::NotARepository(cwd))?;
        Self::open_at(&root)
    }

    /// Open the repository rooted at `root` (must contain `.git`).
    pub fn open_at(root: &Path) -> Result<Self, SourceError> {
        let root = root.canonicalize().map_err(SourceError::Io)?;
        if !root.join(".git").exists() {
            return Err(SourceError::NotARepository(root));
        }
        // Maintain `.ignore` first so the matcher below picks it up.
        ensure_ignore_file(&root)?;
        let matcher = build_matcher(&root)?;
        Ok(Self { root, matcher })
    }

    /// The canonical repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `abs` (an absolute path) is excluded by ignore rules.
    /// Paths outside the repository count as ignored.
    pub fn is_ignored(&self, abs: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(&self.root) else {
            return true;
        };
        let is_dir = abs.is_dir();
        matches!(
            self.matcher.matched_path_or_any_parents(rel, is_dir),
            Match::Ignore(_)
        )
    }

    /// Iterate visible entries under `rel`, honouring ignore rules and a
    /// recursion `max_depth` (0 = just the directory itself). The walker
    /// never descends into `.git`.
    pub fn walk(&self, rel: &Path, max_depth: usize) -> Result<Walk, SourceError> {
        let base = self.resolve(rel)?;
        Ok(WalkBuilder::new(&base)
            .standard_filters(true)
            .hidden(false) // dotfiles stay visible; ignore rules are the policy
            .require_git(true)
            .max_depth(Some(max_depth))
            .sort_by_file_path(|a, b| a.cmp(b))
            .filter_entry(|e| e.file_name() != ".git")
            .build())
    }

    /// Read a text file under `rel`, honouring ignore rules and the size
    /// cap. Returns the (possibly truncated) contents.
    pub fn read(&self, rel: &Path) -> Result<String, SourceError> {
        let canon = self.resolve(rel)?;
        if !canon.is_file() {
            return Err(SourceError::NotFound(rel.to_path_buf()));
        }
        if self.is_ignored(&canon) {
            return Err(SourceError::Ignored(rel.to_path_buf()));
        }
        let bytes = fs::read(&canon).map_err(SourceError::Io)?;
        let text = String::from_utf8_lossy(&bytes);
        let cut = text.floor_char_boundary(MAX_READ_BYTES);
        let mut out = String::with_capacity(cut + 64);
        out.push_str(&text[..cut]);
        if bytes.len() > MAX_READ_BYTES {
            let note = format!("\n…[truncated, {} bytes total]", bytes.len());
            out.push_str(&note);
        }
        Ok(out)
    }

    /// Resolve `rel` inside the repository root to a canonical absolute
    /// path, refusing escapes.
    fn resolve(&self, rel: &Path) -> Result<PathBuf, SourceError> {
        let abs = self.root.join(rel);
        let canon = abs
            .canonicalize()
            .map_err(|_| SourceError::NotFound(rel.to_path_buf()))?;
        if !canon.starts_with(&self.root) {
            return Err(SourceError::Escape(rel.to_path_buf()));
        }
        Ok(canon)
    }
}

/// Nearest ancestor of `start` that contains `.git` (a directory, or a
/// file — the latter covers worktrees).
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Build the ignore matcher from `.gitignore`, `.ignore`, and
/// `.git/info/exclude` (existing files only — `GitignoreBuilder::add`
/// errors on missing files).
fn build_matcher(root: &Path) -> Result<Gitignore, SourceError> {
    let mut builder = GitignoreBuilder::new(root);
    for name in [".gitignore", ".ignore"] {
        let path = root.join(name);
        if path.exists()
            && let Some(err) = builder.add(&path)
        {
            return Err(SourceError::Ignore(err));
        }
    }
    let exclude = root.join(".git/info/exclude");
    if exclude.exists()
        && let Some(err) = builder.add(&exclude)
    {
        return Err(SourceError::Ignore(err));
    }
    builder.build().map_err(SourceError::Ignore)
}

/// Ensure the repo has a `.ignore` file describing hānihi's read policy.
///
/// Never clobbers existing content: the language template header is only
/// prepended when absent, and everything the user wrote is preserved.
fn ensure_ignore_file(root: &Path) -> Result<(), SourceError> {
    let path = root.join(".ignore");
    let existing = if path.exists() {
        fs::read_to_string(&path).map_err(SourceError::Io)?
    } else {
        String::new()
    };

    let mut changed = false;
    let mut content = existing;
    if !content.contains(HANIHI_HEADER) {
        let mut tpl = String::from(HANIHI_HEADER);
        for lang in detect_languages(root) {
            tpl.push_str(lang.template());
        }
        tpl.push('\n');
        content = format!("{tpl}{content}");
        changed = true;
    }
    if changed {
        fs::write(&path, content).map_err(SourceError::Io)?;
    }
    Ok(())
}

/// Detect programming languages from marker files and source extensions,
/// scanning up to two levels deep (catches workspace members).
fn detect_languages(root: &Path) -> Vec<Language> {
    let mut langs = Vec::new();
    let mut has_c = false;
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(false)
        .require_git(true)
        .max_depth(Some(2))
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        match name.as_ref() {
            "Cargo.toml" => push_lang(&mut langs, Language::Rust),
            "CMakeLists.txt" | "Makefile" | "meson.build" => has_c = true,
            _ => {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    ext.as_str(),
                    "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh"
                ) {
                    has_c = true;
                }
            }
        }
    }
    if has_c {
        push_lang(&mut langs, Language::C);
    }
    langs
}

fn push_lang(langs: &mut Vec<Language>, lang: Language) {
    if !langs.contains(&lang) {
        langs.push(lang);
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    /// Throwaway git repo with an ignored `target/` dir.
    pub(crate) struct Fixture {
        pub(crate) dir: PathBuf,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("hanihi-src-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::create_dir_all(dir.join("target/debug")).unwrap();
            fs::write(dir.join(".gitignore"), "target/\n").unwrap();
            fs::write(dir.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
            fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
            fs::write(dir.join("target/debug/junk.rs"), "junk\n").unwrap();
            Self { dir }
        }

        pub(crate) fn tree(&self) -> Arc<SourceTree> {
            Arc::new(SourceTree::open_at(&self.dir).expect("fixture is a git repo"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap_or(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn walk_excludes_ignored_paths() {
        let fx = testutil::Fixture::new();
        let tree = fx.tree();
        let paths: Vec<PathBuf> = tree
            .walk(Path::new("."), 4)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path().to_path_buf())
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("src/main.rs")));
        assert!(paths.iter().any(|p| p.ends_with("Cargo.toml")));
        assert!(!paths.iter().any(|p| p.ends_with("target")));
        assert!(!paths.iter().any(|p| p.ends_with("junk.rs")));
        assert!(
            !paths
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == ".git"))
        );
    }

    #[test]
    fn walk_is_shallow_at_depth_one() {
        let fx = testutil::Fixture::new();
        let tree = fx.tree();
        let paths: Vec<PathBuf> = tree
            .walk(Path::new("."), 1)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path().to_path_buf())
            .collect();
        // src/ is listed, src/main.rs is not.
        assert!(paths.iter().any(|p| p.ends_with("src")));
        assert!(!paths.iter().any(|p| p.ends_with("src/main.rs")));
    }

    #[test]
    fn read_honours_ignore_rules() {
        let fx = testutil::Fixture::new();
        let tree = fx.tree();
        let main = tree.read(Path::new("src/main.rs")).unwrap();
        assert!(main.contains("fn main"));
        match tree.read(Path::new("target/debug/junk.rs")) {
            Err(SourceError::Ignored(_)) => {}
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn read_refuses_escapes() {
        let fx = testutil::Fixture::new();
        let tree = fx.tree();
        let name = format!("hanihi-outside-{}", uuid::Uuid::new_v4());
        let outside = std::env::temp_dir().join(&name);
        fs::write(&outside, "secret").unwrap();
        let rel = Path::new("..").join(&name);
        match tree.read(&rel) {
            Err(SourceError::Escape(_)) => {}
            other => panic!("expected Escape, got {other:?}"),
        }
        fs::remove_file(&outside).unwrap_or(());
    }

    #[test]
    fn read_truncates_large_files() {
        let fx = testutil::Fixture::new();
        let tree = fx.tree();
        fs::write(fx.dir.join("big.rs"), "x".repeat(MAX_READ_BYTES + 4096)).unwrap();
        let out = tree.read(Path::new("big.rs")).unwrap();
        assert!(out.contains("[truncated"));
        assert!(out.len() < MAX_READ_BYTES + 4096);
    }

    #[test]
    fn open_requires_a_repository() {
        let dir = std::env::temp_dir().join(format!("hanihi-norepo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let err = SourceTree::open_at(&dir).unwrap_err();
        assert!(matches!(err, SourceError::NotARepository(_)));
        fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn find_repo_root_walks_up() {
        let fx = testutil::Fixture::new();
        assert_eq!(find_repo_root(&fx.dir.join("src")), Some(fx.dir.clone()));
    }

    #[test]
    fn find_repo_root_returns_none_outside_repos() {
        let dir = std::env::temp_dir().join(format!("hanihi-norepo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_repo_root(&dir), None);
        fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn ensure_ignore_file_bootstraps_rust_template() {
        let fx = testutil::Fixture::new();
        fx.tree(); // open_at runs ensure_ignore_file
        let content = fs::read_to_string(fx.dir.join(".ignore")).unwrap();
        assert!(content.contains(HANIHI_HEADER));
        assert!(content.contains("target/"));
    }

    #[test]
    fn ensure_ignore_file_preserves_user_content() {
        let fx = testutil::Fixture::new();
        fs::write(fx.dir.join(".ignore"), "vendor/\n").unwrap();
        fx.tree();
        let content = fs::read_to_string(fx.dir.join(".ignore")).unwrap();
        assert!(content.contains("vendor/"));
        assert!(content.contains(HANIHI_HEADER));
        // Opening again must not duplicate the template.
        fx.tree();
        let content2 = fs::read_to_string(fx.dir.join(".ignore")).unwrap();
        assert_eq!(content, content2);
    }
}
