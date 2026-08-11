//! Mandatory filesystem lock for session directories.
//!
//! Uses `fs2::FileExt` for cross-platform file locking. The lock is
//! exclusive and held for the lifetime of the `SessionGuard`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// Holds an exclusive lock on a session directory's `.lock` file.
///
/// The lock is released when the guard is dropped.
#[derive(Debug)]
pub struct SessionGuard {
    _file: File,
    _path: PathBuf,
}

impl SessionGuard {
    /// Acquire an exclusive lock on `<dir>/.lock`.
    ///
    /// Fails immediately if another process holds the lock.
    pub fn acquire(dir: &Path) -> io::Result<Self> {
        let lock_path = dir.join(".lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        file.try_lock_exclusive()?;
        Ok(Self {
            _file: file,
            _path: lock_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hanihi-lock-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acquire_and_drop() {
        let dir = tmp_dir();
        let guard = SessionGuard::acquire(&dir).expect("acquire");
        let lock_path = dir.join(".lock");
        assert!(lock_path.exists());
        drop(guard);
        // After drop the lock file still exists on disk (we don't delete it),
        // but the lock should be released so a second acquire succeeds.
        let _guard2 = SessionGuard::acquire(&dir).expect("second acquire after drop");
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }

    #[test]
    fn exclusive_lock_fails() {
        let dir = tmp_dir();
        let _guard = SessionGuard::acquire(&dir).expect("first acquire");
        let result = SessionGuard::acquire(&dir);
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).unwrap_or(());
    }
}
