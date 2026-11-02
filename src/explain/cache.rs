//! Agents' answers on disk, so a change is explained once.
//!
//! An answer is stored under the hash of the agent and the exact prompt,
//! which holds the whole diff: a commit keeps its answer forever, and a
//! working tree that moved on simply misses.

use std::path::{Path, PathBuf};

/// A directory of cached answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// A cache kept in `dir`, created on first write.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Where the answer for `key` lives.
    fn path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}.json"))
    }

    /// The answer stored for `key`, if any.
    #[must_use]
    pub fn get(&self, key: u64) -> Option<String> {
        std::fs::read_to_string(self.path(key)).ok()
    }

    /// Stores `answer` for `key`. Failing only means asking the agent again
    /// next time, so errors are ignored.
    pub fn put(&self, key: u64, answer: &str) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = write_atomic(&self.path(key), answer);
    }
}

/// Writes through a temporary file, so a reader never sees half an answer.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(dir.path().join("explain"));
        assert_eq!(cache.get(7), None);
        cache.put(7, "{\"intent\":\"x\"}");
        assert_eq!(cache.get(7).as_deref(), Some("{\"intent\":\"x\"}"));
        assert!(dir.path().join("explain/0000000000000007.json").exists());
        cache.put(7, "newer");
        assert_eq!(cache.get(7).as_deref(), Some("newer"));
    }

    #[test]
    fn unwritable_caches_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, "").expect("write");
        let cache = Cache::new(file.join("sub"));
        cache.put(1, "x");
        assert_eq!(cache.get(1), None);
    }
}
