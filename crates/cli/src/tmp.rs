//! A throwaway directory for `demo --offline` and for the tests.
//!
//! Why hand-rolled instead of the `tempfile` crate: `tempfile` is already in
//! the workspace lock file, but adding a dependency edge from this crate would
//! make cargo rewrite the shared `Cargo.lock`, and several agents build this
//! workspace concurrently — the lock is deliberately off-limits to everyone but
//! the workspace owner. Twenty lines of `mkdir`/`remove_dir_all` keep the same
//! guarantee (a unique path, cleaned up on drop) without touching it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Hands out unique suffixes even when two calls land in the same nanosecond
/// (parallel tests in one process share the pid, so the counter is the real
/// uniqueness guarantee, not the clock).
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory that removes itself when it goes out of scope.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `<tmp>/swarmbot-<tag>-<pid>-<nanos>-<n>` and own it.
    pub fn new(tag: &str) -> std::io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let safe_tag: String = tag
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let path = std::env::temp_dir().join(format!(
            "swarmbot-{safe_tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover demo directory under /tmp is untidy, not
        // wrong, and there is nobody to tell in a destructor.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dirs_are_unique_and_cleaned_up() {
        let path = {
            let a = TempDir::new("test").expect("a temp dir");
            let b = TempDir::new("test").expect("another temp dir");
            assert_ne!(a.path(), b.path());
            assert!(a.path().exists());
            a.path().to_path_buf()
        };
        assert!(!path.exists(), "dropping removes the directory");
    }

    #[test]
    fn tags_are_sanitized() {
        let dir = TempDir::new("weird/../tag").expect("a temp dir");
        let name = dir
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(name.starts_with("swarmbot-weird----tag-"), "{name}");
    }
}
