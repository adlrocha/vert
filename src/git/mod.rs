//! Reading changes out of a git repository.

pub mod diff;
pub mod feed;

use std::path::{Path, PathBuf};

use git2::{Diff, DiffFindOptions, DiffOptions, ErrorCode, Oid, Patch, Repository, Tree};
use thiserror::Error;

/// Errors raised while reading the repository.
#[derive(Debug, Error)]
pub enum GitError {
    /// The path is not inside a git repository.
    #[error("{path} is not inside a git repository")]
    NotARepo {
        /// Path that was searched.
        path: PathBuf,
        /// Underlying libgit2 error.
        #[source]
        source: git2::Error,
    },
    /// Bare repositories have no working tree to explore.
    #[error("bare repositories are not supported (no working tree)")]
    Bare,
    /// A git operation failed.
    #[error("failed to {context}")]
    Op {
        /// What vert was trying to do.
        context: &'static str,
        /// Underlying libgit2 error.
        #[source]
        source: git2::Error,
    },
}

/// Attaches a human-readable context to libgit2 errors.
pub(crate) trait Context<T> {
    fn ctx(self, context: &'static str) -> Result<T, GitError>;
}

impl<T> Context<T> for Result<T, git2::Error> {
    fn ctx(self, context: &'static str) -> Result<T, GitError> {
        self.map_err(|source| GitError::Op { context, source })
    }
}

/// Opens the repository containing `path`.
///
/// # Errors
/// Returns [`GitError::NotARepo`] if no repository is found, or
/// [`GitError::Bare`] for bare repositories.
pub fn open(path: &Path) -> Result<Repository, GitError> {
    let repo = Repository::discover(path).map_err(|source| GitError::NotARepo {
        path: path.to_path_buf(),
        source,
    })?;
    if repo.is_bare() {
        return Err(GitError::Bare);
    }
    Ok(repo)
}

/// The working directory of a non-bare repository.
///
/// # Errors
/// Returns [`GitError::Bare`] if the repository has no working tree.
pub fn workdir(repo: &Repository) -> Result<PathBuf, GitError> {
    repo.workdir().map(Path::to_path_buf).ok_or(GitError::Bare)
}

/// Display information about the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Name of the working directory.
    pub name: String,
    /// Current branch, short detached hash, or `None` when nothing is committed yet.
    pub branch: Option<String>,
}

/// Reads [`RepoInfo`] for the header.
#[must_use]
pub fn info(repo: &Repository) -> RepoInfo {
    let name = repo.workdir().and_then(|p| p.file_name()).map_or_else(
        || "repository".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let branch = repo.head().ok().and_then(|head| {
        if head.is_branch() {
            Some(String::from_utf8_lossy(head.shorthand_bytes()).into_owned())
        } else {
            head.target().map(short_id)
        }
    });
    RepoInfo { name, branch }
}

/// Abbreviated 7-character hash.
#[must_use]
pub fn short_id(oid: Oid) -> String {
    let mut s = oid.to_string();
    s.truncate(7);
    s
}

/// What a diff is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Uncommitted changes (staged, unstaged and untracked) relative to `HEAD`.
    WorkingTree,
    /// A commit relative to its first parent.
    Commit(Oid),
}

/// The status of a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// New file.
    Added,
    /// Modified in place.
    Modified,
    /// Removed.
    Deleted,
    /// Moved or copied from another path.
    Renamed,
}

impl FileStatus {
    /// One-letter code, as in `git status --short`.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
        }
    }

    fn from_delta(delta: git2::Delta) -> Self {
        match delta {
            git2::Delta::Added | git2::Delta::Untracked => Self::Added,
            git2::Delta::Deleted => Self::Deleted,
            git2::Delta::Renamed | git2::Delta::Copied => Self::Renamed,
            _ => Self::Modified,
        }
    }
}

/// Per-file statistics of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    /// Current path (the old path for deletions), `/`-separated.
    pub path: String,
    /// Previous path when renamed.
    pub old_path: Option<String>,
    /// Kind of change.
    pub status: FileStatus,
    /// Lines added.
    pub added: u64,
    /// Lines removed.
    pub removed: u64,
    /// Whether the content is binary.
    pub binary: bool,
}

fn path_string(path: Option<&Path>) -> Option<String> {
    path.map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn head_tree(repo: &Repository) -> Result<Option<Tree<'_>>, GitError> {
    match repo.head() {
        Ok(head) => head.peel_to_tree().map(Some).ctx("read HEAD tree"),
        Err(e) if matches!(e.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => Ok(None),
        Err(source) => Err(GitError::Op {
            context: "resolve HEAD",
            source,
        }),
    }
}

/// Builds the diff for `target`, with rename detection.
pub(crate) fn make_diff(repo: &Repository, target: Target) -> Result<Diff<'_>, GitError> {
    let mut opts = DiffOptions::new();
    let mut diff = match target {
        Target::WorkingTree => {
            opts.include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true);
            let tree = head_tree(repo)?;
            repo.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut opts))
                .ctx("diff the working tree")?
        }
        Target::Commit(oid) => {
            let commit = repo.find_commit(oid).ctx("find commit")?;
            let tree = commit.tree().ctx("read commit tree")?;
            let parent_tree = match commit.parent(0) {
                Ok(parent) => Some(parent.tree().ctx("read parent tree")?),
                Err(_) => None,
            };
            repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
                .ctx("diff commit against its parent")?
        }
    };
    diff.find_similar(Some(DiffFindOptions::new().renames(true)))
        .ctx("detect renames")?;
    Ok(diff)
}

/// Computes the [`FileStat`] of delta `index` along with its patch (if textual).
pub(crate) fn delta_stat<'d>(
    diff: &'d Diff<'_>,
    index: usize,
) -> Result<Option<(FileStat, Option<Patch<'d>>)>, GitError> {
    let Some(delta) = diff.get_delta(index) else {
        return Ok(None);
    };
    let status = FileStatus::from_delta(delta.status());
    let new_path = path_string(delta.new_file().path());
    let old_path = path_string(delta.old_file().path());
    let path = match status {
        FileStatus::Deleted => old_path.clone().or(new_path),
        _ => new_path.or_else(|| old_path.clone()),
    }
    .unwrap_or_default();
    let textual = Patch::from_diff(diff, index).ctx("build patch")?;
    let binary = delta.flags().is_binary() || textual.is_none();
    let (added, removed) = match &textual {
        Some(p) if !binary => {
            let (_, add, del) = p.line_stats().ctx("count changed lines")?;
            (add as u64, del as u64)
        }
        _ => (0, 0),
    };
    let stat = FileStat {
        old_path: (status == FileStatus::Renamed)
            .then_some(old_path)
            .flatten()
            .filter(|old| *old != path),
        path,
        status,
        added,
        removed,
        binary,
    };
    Ok(Some((stat, textual)))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Helpers for building throwaway repositories in tests.
    use std::fs;
    use std::path::Path;

    use git2::{Oid, Repository, Signature, Time};
    use tempfile::TempDir;

    /// A temporary repository.
    pub struct TestRepo {
        pub dir: TempDir,
        pub repo: Repository,
    }

    impl TestRepo {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = Repository::init(dir.path()).expect("init");
            Self { dir, repo }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn write(&self, rel: &str, content: &str) {
            let path = self.dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(path, content).expect("write");
        }

        pub fn remove(&self, rel: &str) {
            fs::remove_file(self.dir.path().join(rel)).expect("remove");
        }

        /// Stages everything and commits with the given message at `time`.
        pub fn commit_at(&self, message: &str, time: i64) -> Oid {
            let mut index = self.repo.index().expect("index");
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .expect("add");
            index.update_all(["*"], None).expect("update removed files");
            index.write().expect("write index");
            let tree_id = index.write_tree().expect("tree");
            let tree = self.repo.find_tree(tree_id).expect("find tree");
            let sig = Signature::new("Agent Smith", "agent@example.com", &Time::new(time, 0))
                .expect("sig");
            let parents: Vec<_> = self
                .repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .into_iter()
                .collect();
            let parent_refs: Vec<_> = parents.iter().collect();
            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
                .expect("commit")
        }

        pub fn commit(&self, message: &str) -> Oid {
            self.commit_at(message, 1_700_000_000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TestRepo;
    use super::*;

    #[test]
    fn open_rejects_non_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A fresh temp dir may still sit under a repo on some machines; use a ceiling.
        let err = Repository::open_ext(
            dir.path(),
            git2::RepositoryOpenFlags::NO_SEARCH,
            std::iter::empty::<&std::ffi::OsStr>(),
        );
        assert!(err.is_err());
        let res = open(&dir.path().join("does-not-exist"));
        assert!(matches!(res, Err(GitError::NotARepo { .. })));
    }

    #[test]
    fn open_rejects_bare() {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init_bare(dir.path()).expect("bare");
        assert!(matches!(open(dir.path()), Err(GitError::Bare)));
    }

    #[test]
    fn info_reports_branch_and_name() {
        let t = TestRepo::new();
        let unborn = info(&t.repo);
        assert_eq!(unborn.branch, None);
        t.write("a.txt", "a\n");
        let oid = t.commit("init");
        let i = info(&t.repo);
        assert!(matches!(i.branch.as_deref(), Some("master" | "main")));
        assert_eq!(
            i.name,
            t.path().file_name().expect("name").to_string_lossy()
        );
        t.repo.set_head_detached(oid).expect("detach");
        assert_eq!(info(&t.repo).branch, Some(short_id(oid)));
        assert_eq!(
            workdir(&t.repo).expect("workdir").canonicalize().ok(),
            t.path().canonicalize().ok()
        );
    }

    #[test]
    fn status_letters() {
        let letters: String = [
            FileStatus::Added,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Renamed,
        ]
        .iter()
        .map(|s| s.letter())
        .collect();
        assert_eq!(letters, "AMDR");
        assert_eq!(
            FileStatus::from_delta(git2::Delta::Typechange),
            FileStatus::Modified
        );
        assert_eq!(
            FileStatus::from_delta(git2::Delta::Copied),
            FileStatus::Renamed
        );
    }

    #[test]
    fn context_wraps_errors() {
        let res: Result<(), git2::Error> = Err(git2::Error::from_str("boom"));
        let err = res.ctx("do a thing").expect_err("error");
        assert_eq!(err.to_string(), "failed to do a thing");
    }
}
