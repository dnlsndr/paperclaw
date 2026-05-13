//! Process-level mutual exclusion for ingest runs.
//!
//! Two `paperclaw ingest` invocations against the same library would race
//! on collision-resolution and could file two copies under the same stem.
//! The lock here is an advisory exclusive lock on a sentinel file inside
//! the library root. The OS releases it when the process exits, so a
//! crash never leaves the library wedged.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Filename of the lock sentinel inside the library root. Starts with a
/// dot so it doesn't visually clutter the library listing.
const LOCK_FILE: &str = ".paperclaw.lock";

/// RAII guard for an exclusive lock on the library's ingest sentinel.
/// Drop releases the lock; the underlying file is left in place for the
/// next run (it's a sentinel, not transient state).
#[derive(Debug)]
pub struct IngestLock {
    // `File`'s Drop implementation releases the lock for us. Field is
    // never read directly — it just keeps the FD alive.
    _file: File,
    path: PathBuf,
}

impl IngestLock {
    /// Acquire the ingest lock for `library_root`. Creates the sentinel
    /// file if missing and the library root itself if missing.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::AlreadyHeld`] when another process already
    /// owns the lock (most likely another `paperclaw ingest`). Returns
    /// [`LockError::Io`] for filesystem failures (permission denied, etc).
    pub fn acquire(library_root: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(library_root).map_err(|e| LockError::from_io(&e))?;

        let path = library_root.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| LockError::from_io(&e))?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::AlreadyHeld { path }),
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::from_io(&e)),
        }
    }

    /// Path of the sentinel file. Useful in error messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Errors raised when acquiring an [`IngestLock`].
#[derive(Debug, Error)]
pub enum LockError {
    /// Another process holds the lock — most likely a concurrent
    /// `paperclaw ingest`.
    #[error("another paperclaw ingest is already running (lock: {path})", path = path.display())]
    AlreadyHeld {
        /// Sentinel file we tried to lock.
        path: PathBuf,
    },
    /// Filesystem error while preparing or acquiring the lock.
    #[error("ingest lock I/O error: {0}")]
    Io(String),
}

impl LockError {
    fn from_io(e: &io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_and_release_roundtrip() {
        let dir = TempDir::new().unwrap();
        let lock = IngestLock::acquire(dir.path()).unwrap();
        assert!(lock.path().exists());
        drop(lock);

        // Re-acquire after drop — should succeed.
        let _again = IngestLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn second_acquire_while_first_held_returns_already_held() {
        let dir = TempDir::new().unwrap();
        let _first = IngestLock::acquire(dir.path()).unwrap();
        let err = IngestLock::acquire(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::AlreadyHeld { .. }));
    }

    #[test]
    fn acquire_creates_library_root_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("does/not/exist/yet");
        let lock = IngestLock::acquire(&nested).unwrap();
        assert!(lock.path().exists());
    }
}
