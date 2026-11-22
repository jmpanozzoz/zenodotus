//! The advisory lock that makes `sync` the only writer.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::JobsError;

/// The lock file in the data directory.
pub const LOCK_FILE: &str = "sync.lock";

/// Holds the sync lock until dropped.
#[derive(Debug)]
pub struct SyncLock {
    _file: File,
}

impl SyncLock {
    /// Takes the sync lock of `data_dir`, creating the directory and the lock file when needed.
    ///
    /// # Errors
    ///
    /// [`JobsError::Busy`] when another process holds the lock, or [`JobsError::Io`].
    pub fn acquire(data_dir: &Path) -> Result<Self, JobsError> {
        std::fs::create_dir_all(data_dir).map_err(JobsError::io(data_dir))?;
        let path = data_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(JobsError::io(&path))?;
        let locked = file.try_lock();
        if matches!(locked, Err(TryLockError::WouldBlock)) {
            return Err(JobsError::Busy { path });
        }
        locked.map_err(std::io::Error::from).map_err(JobsError::io(&path))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let first = SyncLock::acquire(&data_dir).unwrap();
        let error = SyncLock::acquire(&data_dir).unwrap_err();
        assert!(matches!(error, JobsError::Busy { .. }), "{error}");
        drop(first);
        assert!(SyncLock::acquire(&data_dir).is_ok());
    }

    #[test]
    fn an_unusable_data_directory_is_an_io_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let error = SyncLock::acquire(&file).unwrap_err();
        assert!(matches!(error, JobsError::Io { .. }), "{error}");
        std::fs::create_dir(dir.path().join("data")).unwrap();
        std::fs::create_dir(dir.path().join("data").join(LOCK_FILE)).unwrap();
        let error = SyncLock::acquire(&dir.path().join("data")).unwrap_err();
        assert!(error.to_string().contains(LOCK_FILE), "{error}");
    }
}
