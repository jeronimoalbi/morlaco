//! Helpers shared by the tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Makes every directory name unique, even for the same `name` in one process.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A directory under the system temp dir that is removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Creates an empty directory whose name includes `name`, the process id
    /// and a counter, so tests running in parallel never share one.
    pub fn new(name: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("morlaco-app-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
