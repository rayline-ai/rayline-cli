//! Test directories under the system temp directory, removed when dropped.

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A new, empty directory `<temp>/<name>-<pid>-<n>`, created by this call
/// and removed with everything in it when this is dropped: when its test
/// ends, whether it passed or panicked. Dereferences to its path.
#[derive(Debug)]
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        // A counter, not the clock: parallel tests can read the same time.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // The temp directory itself may not exist yet, e.g. a TMPDIR set to
        // a fresh path.
        std::fs::create_dir_all(std::env::temp_dir()).expect("create temp directory");
        loop {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("{name}-{}-{unique}", std::process::id()));
            match std::fs::create_dir(&dir) {
                Ok(()) => return Self(dir),
                // Another run with the same process id has it, or an earlier
                // run left it: not ours, so leave it and try the next name.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create test directory {}: {error}", dir.display()),
            }
        }
    }

    /// Removes the directory now and returns the error if that fails, for a
    /// test that checks what it made can be deleted. Once it is removed,
    /// dropping this does nothing more.
    pub(crate) fn remove(mut self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.0)?;
        self.0 = PathBuf::new();
        Ok(())
    }
}

impl Deref for TestDir {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
