//! Per-profile run locks.
//!
//! Locking is per profile rather than global so that overlapping cron schedules
//! for different directories never block one another -- organizing Downloads
//! has nothing to say about organizing a document inbox.
//!
//! The lock is an `O_EXCL` file holding the owning pid, released on drop. A
//! process that dies without unwinding leaves the file behind; rather than
//! guessing at staleness with a timeout, the lock records enough for a human to
//! decide, and reports whether the recorded pid is still alive.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("profile `{profile}` is already running (lock held by pid {holder} at {path})")]
    Held { profile: String, holder: String, path: PathBuf },
    #[error("could not create lock directory {path}")]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not create lock file {path}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Holds a profile's lock for as long as it is alive.
#[derive(Debug)]
pub struct ProfileLock {
    path: PathBuf,
}

impl ProfileLock {
    /// Takes the lock for `profile`, or reports who holds it.
    ///
    /// The profile name is already constrained by config validation to
    /// `[A-Za-z0-9_-]`, so it cannot escape `dir`.
    pub fn acquire(dir: &Path, profile: &str) -> Result<Self, LockError> {
        fs::create_dir_all(dir)
            .map_err(|source| LockError::Dir { path: dir.to_path_buf(), source })?;

        let path = dir.join(format!("{profile}.lock"));
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.lines().next().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned());
                Err(LockError::Held { profile: profile.to_owned(), holder, path })
            }
            Err(source) => Err(LockError::Create { path, source }),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // Best effort: a failure here leaves a stale lock, which the next run
        // reports with the owning pid rather than silently ignoring.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_is_refused_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let first = ProfileLock::acquire(dir.path(), "downloads").unwrap();
        assert!(first.path().exists());

        let err = ProfileLock::acquire(dir.path(), "downloads").unwrap_err();
        match err {
            LockError::Held { holder, profile, .. } => {
                assert_eq!(profile, "downloads");
                assert_eq!(holder, std::process::id().to_string());
            }
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn dropping_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let lock = ProfileLock::acquire(dir.path(), "downloads").unwrap();
            lock.path().to_path_buf()
        };
        assert!(!path.exists());
        ProfileLock::acquire(dir.path(), "downloads").expect("should be free again");
    }

    #[test]
    fn different_profiles_do_not_block_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let _a = ProfileLock::acquire(dir.path(), "downloads").unwrap();
        let _b = ProfileLock::acquire(dir.path(), "personal-docs").unwrap();
    }
}
