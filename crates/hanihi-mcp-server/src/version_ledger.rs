//! Session-scoped version ledger shared by the workspace file tools.
//!
//! `read_file` and a successful `apply_patch` record the versions they
//! observe. `apply_patch` resolves `base_token: "auto"` and opaque handle
//! ids through this ledger, so the agent never has to carry file state in
//! prose across turns.
//!
//! The read-only and read-write MCP servers run as separate processes, so
//! the ledger is persisted to a JSON file outside the repository: under the
//! system temp directory by default, or `HANIHI_LEDGER_DIR` when set. The
//! file is keyed by the canonical workspace root path, so both servers
//! resolve the same ledger for the same workspace.
//!
//! Updates are read-modify-write with a temp-file rename. The agent issues
//! tool calls sequentially, so a full file lock is unnecessary; a
//! concurrent writer can lose an update, never corrupt an existing one.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const LEDGER_DIR_ENV: &str = "HANIHI_LEDGER_DIR";
const DEFAULT_LEDGER_DIR: &str = "hanihi-version-ledger";
const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;

/// A version handle the harness mints when it observes a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileVersion {
    /// SHA-256 of the file's full contents, lowercase hex.
    pub(crate) digest: String,
    /// Byte length of the file's full contents.
    pub(crate) len: u64,
    /// Opaque ledger id; present only when the ledger recorded this version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<u64>,
}

impl FileVersion {
    pub(crate) fn new(digest: impl Into<String>, len: u64) -> Self {
        Self {
            digest: digest.into(),
            len,
            id: None,
        }
    }

    fn with_id(digest: impl Into<String>, len: u64, id: u64) -> Self {
        Self {
            digest: digest.into(),
            len,
            id: Some(id),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct LedgerFile {
    next_id: u64,
    versions: HashMap<String, FileVersion>,
}

/// A file-backed, workspace-scoped record of observed file versions.
pub(crate) struct VersionLedger {
    path: PathBuf,
}

impl VersionLedger {
    /// The ledger for `root`, stored under the default shared location.
    pub(crate) fn for_root(root: &Path) -> Result<Self, String> {
        let dir = std::env::var_os(LEDGER_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(DEFAULT_LEDGER_DIR));
        Self::at(&dir, root)
    }

    /// The ledger for `root`, stored under an explicit directory. Tests use
    /// this to keep ledger state isolated and disposable.
    pub(crate) fn at(dir: &Path, root: &Path) -> Result<Self, String> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        fs::create_dir_all(dir).map_err(|error| {
            format!("cannot create ledger directory {}: {error}", dir.display())
        })?;
        let key = digest_hex(root.to_string_lossy().as_bytes());
        Ok(Self {
            path: dir.join(format!("{key}.json")),
        })
    }

    fn load(&self) -> Result<LedgerFile, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(LedgerFile::default()),
            Err(error) => {
                return Err(format!(
                    "cannot read ledger {}: {error}",
                    self.path.display()
                ));
            }
        };
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(format!(
                "ledger file {} is unexpectedly large",
                self.path.display()
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot parse ledger {}: {error}", self.path.display()))
    }

    fn save(&self, ledger: &LedgerFile) -> Result<(), String> {
        let bytes = serde_json::to_vec(ledger)
            .map_err(|error| format!("cannot serialize ledger: {error}"))?;
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &bytes)
            .map_err(|error| format!("cannot write ledger {}: {error}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|error| format!("cannot commit ledger {}: {error}", self.path.display()))
    }

    /// Record `(path, digest, len)` and mint a fresh opaque id for it.
    pub(crate) fn record(&self, path: &str, digest: &str, len: u64) -> Result<FileVersion, String> {
        let mut ledger = self.load()?;
        let version = FileVersion::with_id(digest, len, ledger.next_id);
        ledger.next_id = ledger.next_id.saturating_add(1);
        ledger.versions.insert(path.to_string(), version.clone());
        self.save(&ledger)?;
        Ok(version)
    }

    /// The most recent version recorded for `path`, if any.
    pub(crate) fn lookup(&self, path: &str) -> Result<Option<FileVersion>, String> {
        Ok(self.load()?.versions.get(path).cloned())
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::test_support::temp_dir;

    #[test]
    fn record_then_lookup_returns_the_same_entry() {
        let dir = temp_dir("ledger_record_lookup");
        let ledger = VersionLedger::at(&dir, &dir).unwrap();
        let version = ledger.record("a.txt", "abc", 3).unwrap();
        assert_eq!(version.id, Some(0));
        assert_eq!(ledger.lookup("a.txt").unwrap(), Some(version));
    }

    #[test]
    fn entries_persist_across_ledger_instances() {
        let dir = temp_dir("ledger_persists");
        let first = VersionLedger::at(&dir, &dir).unwrap();
        let version = first.record("a.txt", "abc", 3).unwrap();

        let second = VersionLedger::at(&dir, &dir).unwrap();
        assert_eq!(second.lookup("a.txt").unwrap(), Some(version));
    }

    #[test]
    fn ids_increase_monotonically() {
        let dir = temp_dir("ledger_ids");
        let ledger = VersionLedger::at(&dir, &dir).unwrap();
        let a = ledger.record("a.txt", "a", 1).unwrap();
        let b = ledger.record("b.txt", "b", 1).unwrap();
        assert!(b.id > a.id);
    }

    #[test]
    fn file_version_omits_id_when_unset() {
        let version = FileVersion::new("abc", 3);
        let value = serde_json::to_value(&version).unwrap();
        assert!(value.get("id").is_none());
        assert_eq!(value["digest"], "abc");
        assert_eq!(value["len"], 3);
    }
}
