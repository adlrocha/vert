//! Agents' answers on disk, so a change is explained once.
//!
//! An answer is stored under a hash of the agent, the model, the full diff
//! and the prompt (see `Explainer::explain`). The diff is hashed on its own
//! because the prompt leaves hunk bodies out past `--explain-max-diff-kb`, so
//! two changes can share a prompt but never an answer. A commit keeps its
//! answer forever, and a working tree that moved on simply misses.
//!
//! Answers describe private code, so the directory and its files are made
//! readable by their owner only.

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
        let _ = private_dir(&self.dir);
        let _ = write_atomic(&self.path(key), answer);
    }
}

/// Creates `dir` readable by its owner only.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Writes through a temporary file readable by its owner only, so a reader
/// never sees half an answer.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(text.as_bytes())?;
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
        let saved = dir.path().join("explain/0000000000000007.json");
        assert!(saved.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join("explain")), 0o700);
            assert_eq!(mode(&saved), 0o600);
        }
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
