//! The change feed: commits plus the live working tree, labelled.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use git2::{Oid, Repository, Sort};

use super::{Context, FileStat, GitError, Target, delta_stat, head_tree, make_diff, short_id};
use crate::labels::{Labeler, Size, reverted_target};

/// One row of the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What this entry describes.
    pub target: Target,
    /// Stable identifier: full hash, or `"worktree"`.
    pub id: String,
    /// Short hash, or `"working"`.
    pub short: String,
    /// First line of the message.
    pub summary: String,
    /// Rest of the message.
    pub body: String,
    /// Author name.
    pub author: String,
    /// Unix seconds: commit time, or last modification of a changed file.
    pub time: i64,
    /// Changed files.
    pub files: Vec<FileStat>,
    /// Total lines added.
    pub added: u64,
    /// Total lines removed.
    pub removed: u64,
    /// Distinct modules touched.
    pub modules: Vec<String>,
    /// Size bucket.
    pub size: Size,
    /// Touches many modules.
    pub wide: bool,
    /// Undone by a later commit in the feed.
    pub reverted: bool,
    /// This commit is itself a revert.
    pub is_revert: bool,
    /// The linked worktree whose files this change belongs to, or `None` for
    /// vert's own working directory.
    pub root: Option<PathBuf>,
}

/// The identifier used for the working-tree entry.
pub const WORKTREE_ID: &str = "worktree";

impl Entry {
    /// Builds an unnamed entry for `target` from its changed files, labelled
    /// by `labeler`. Callers fill in the message, author and time.
    #[must_use]
    pub fn new(target: Target, files: Vec<FileStat>, labeler: &Labeler) -> Self {
        let added = files.iter().map(|f| f.added).sum();
        let removed = files.iter().map(|f| f.removed).sum();
        let modules: Vec<String> = files
            .iter()
            .map(|f| labeler.module_of(&f.path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let (id, short) = match target {
            Target::WorkingTree => (WORKTREE_ID.to_owned(), "working".to_owned()),
            Target::Commit(oid) => (oid.to_string(), short_id(oid)),
            Target::Range { base, head } => (
                format!(
                    "{}..{head}",
                    base.map(|b| b.to_string()).unwrap_or_default()
                ),
                short_id(head),
            ),
            Target::PullRequest(number) => (format!("pr:{number}"), format!("#{number}")),
        };
        Self {
            target,
            id,
            short,
            summary: String::new(),
            body: String::new(),
            author: String::new(),
            time: 0,
            size: labeler.size(added + removed),
            wide: labeler.is_wide(modules.len()),
            files,
            added,
            removed,
            modules,
            reverted: false,
            is_revert: false,
            root: None,
        }
    }

    /// Whether this is the uncommitted working-tree entry.
    #[must_use]
    pub fn is_worktree(&self) -> bool {
        self.target == Target::WorkingTree
    }

    /// Whether any changed file has `path`.
    #[must_use]
    pub fn touches_path(&self, path: &str) -> bool {
        self.files
            .iter()
            .any(|f| f.path == path || f.old_path.as_deref() == Some(path))
    }
}

pub(crate) fn file_stats(repo: &Repository, target: Target) -> Result<Vec<FileStat>, GitError> {
    let diff = make_diff(repo, target)?;
    (0..diff.deltas().len())
        .map(|i| delta_stat(&diff, i).map(|o| o.map(|(stat, _)| stat)))
        .filter_map(Result::transpose)
        .collect()
}

/// Loads up to `limit` commits reachable from `HEAD`, newest first.
///
/// Returns an empty feed for a repository without commits.
///
/// # Errors
/// Returns [`GitError`] if the history cannot be read.
pub fn load_commits(
    repo: &Repository,
    limit: usize,
    labeler: &Labeler,
) -> Result<Vec<Entry>, GitError> {
    load_commits_cached(repo, limit, labeler, &mut CommitCache::default())
}

/// Commits already turned into entries, keyed by hash. Commits are immutable,
/// so a reload only needs to diff the commits it hasn't seen before.
pub type CommitCache = HashMap<Oid, Entry>;

/// Like [`load_commits`], reusing and refreshing `cache`.
///
/// After the call, `cache` holds exactly the returned commits, so it never
/// grows beyond `limit` entries.
///
/// # Errors
/// Returns [`GitError`] if the history cannot be read.
pub fn load_commits_cached(
    repo: &Repository,
    limit: usize,
    labeler: &Labeler,
    cache: &mut CommitCache,
) -> Result<Vec<Entry>, GitError> {
    if head_tree(repo)?.is_none() {
        cache.clear();
        return Ok(Vec::new());
    }
    let mut walk = repo.revwalk().ctx("start history walk")?;
    walk.push_head().ctx("walk history from HEAD")?;
    walk.set_sorting(Sort::TIME).ctx("sort history")?;
    let mut entries = walk
        .take(limit)
        .map(|oid| {
            let oid = oid.ctx("walk history")?;
            match cache.get(&oid) {
                Some(entry) => Ok(entry.clone()),
                None => commit_entry(repo, oid, labeler),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    mark_reverts(&mut entries);
    *cache = entries
        .iter()
        .filter_map(|e| match e.target {
            Target::Commit(oid) => Some((oid, e.clone())),
            _ => None,
        })
        .collect();
    Ok(entries)
}

fn commit_entry(repo: &Repository, oid: Oid, labeler: &Labeler) -> Result<Entry, GitError> {
    let commit = repo.find_commit(oid).ctx("read commit")?;
    let files = file_stats(repo, Target::Commit(oid))?;
    let mut entry = Entry::new(Target::Commit(oid), files, labeler);
    entry.summary = commit
        .summary_bytes()
        .map_or_else(|| "(no message)".to_owned(), lossy);
    commit
        .body_bytes()
        .map(lossy)
        .unwrap_or_default()
        .trim()
        .clone_into(&mut entry.body);
    entry.author = lossy(commit.author().name_bytes());
    entry.time = commit.time().seconds();
    entry.is_revert = reverted_target(&lossy(commit.message_bytes())).is_some();
    Ok(entry)
}

pub(crate) fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Flags commits that a later commit in `entries` reverts.
fn mark_reverts(entries: &mut [Entry]) {
    let targets: Vec<String> = entries
        .iter()
        .filter(|e| e.is_revert)
        .filter_map(|e| reverted_target(&format!("{}\n\n{}", e.summary, e.body)))
        .collect();
    for entry in entries.iter_mut() {
        entry.reverted = targets.iter().any(|t| entry.id.starts_with(t.as_str()));
    }
}

/// Loads uncommitted changes (including untracked files), or `None` if clean.
///
/// The entry's time is the most recent modification among changed files, so
/// its age shows when an agent last touched the tree.
///
/// # Errors
/// Returns [`GitError`] if the working tree cannot be diffed.
pub fn load_worktree(
    repo: &Repository,
    labeler: &Labeler,
    now: i64,
) -> Result<Option<Entry>, GitError> {
    let files = file_stats(repo, Target::WorkingTree)?;
    if files.is_empty() {
        return Ok(None);
    }
    let workdir = super::workdir(repo)?;
    let mut entry = Entry::new(Target::WorkingTree, files, labeler);
    "Uncommitted changes".clone_into(&mut entry.summary);
    entry.time = entry
        .files
        .iter()
        .filter_map(|f| modified_secs(&workdir.join(&f.path)))
        .max()
        .unwrap_or(now);
    Ok(Some(entry))
}

pub(crate) fn modified_secs(path: &Path) -> Option<i64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::FileStatus;
    use crate::git::testutil::TestRepo;

    fn labeler() -> Labeler {
        Labeler::new(5, 20, 100, 2, 1).expect("labeler")
    }

    #[test]
    fn empty_repo_has_empty_feed() {
        let t = TestRepo::new();
        assert!(
            load_commits(&t.repo, 10, &labeler())
                .expect("load")
                .is_empty()
        );
        assert!(load_worktree(&t.repo, &labeler(), 0).expect("wt").is_none());
    }

    #[test]
    fn commits_are_newest_first_with_stats() {
        let t = TestRepo::new();
        t.write("src/a.rs", "one\ntwo\n");
        t.commit_at("first", 1_000);
        t.write("src/a.rs", "one\nTWO\nthree\n");
        t.write("docs/readme.md", "hi\n");
        t.commit_at("second\n\nlonger body", 2_000);

        let feed = load_commits(&t.repo, 10, &labeler()).expect("load");
        assert_eq!(feed.len(), 2);
        let second = &feed[0];
        assert_eq!(second.summary, "second");
        assert_eq!(second.body, "longer body");
        assert_eq!(second.author, "Agent Smith");
        assert_eq!(second.time, 2_000);
        assert_eq!(second.added, 3);
        assert_eq!(second.removed, 1);
        assert_eq!(second.modules, ["docs", "src"]);
        assert!(second.wide);
        assert_eq!(second.short.len(), 7);
        assert!(!second.is_worktree());
        assert!(second.touches_path("docs/readme.md"));
        assert!(!second.touches_path("nope"));

        let first = &feed[1];
        assert_eq!(first.files[0].status, FileStatus::Added);
        assert_eq!(first.added, 2);
        assert!(!first.wide);
    }

    #[test]
    fn limit_truncates_history() {
        let t = TestRepo::new();
        for i in 0..5 {
            t.write("f.txt", &format!("{i}\n"));
            t.commit_at(&format!("c{i}"), 1_000 + i);
        }
        let feed = load_commits(&t.repo, 3, &labeler()).expect("load");
        assert_eq!(
            feed.iter().map(|e| e.summary.as_str()).collect::<Vec<_>>(),
            ["c4", "c3", "c2"]
        );
    }

    #[test]
    fn cache_reuses_entries_and_tracks_history() {
        let t = TestRepo::new();
        t.write("f.txt", "a\n");
        let first = t.commit_at("first", 1_000);
        let mut cache = CommitCache::default();
        let feed = load_commits_cached(&t.repo, 10, &labeler(), &mut cache).expect("load");
        assert_eq!(feed.len(), 1);
        assert!(cache.contains_key(&first));

        // A poisoned cache entry proves cached commits are not recomputed.
        cache.get_mut(&first).expect("cached").summary = "from cache".to_owned();
        t.write("f.txt", "b\n");
        let bad = t.commit_at("second", 2_000);
        let feed = load_commits_cached(&t.repo, 10, &labeler(), &mut cache).expect("load");
        assert_eq!(feed[0].summary, "second");
        assert_eq!(feed[1].summary, "from cache");
        assert_eq!(cache.len(), 2);

        // Revert marks are recomputed even for cached commits.
        t.write("f.txt", "a\n");
        t.commit_at(&format!("Revert\n\nThis reverts commit {bad}."), 3_000);
        let feed = load_commits_cached(&t.repo, 2, &labeler(), &mut cache).expect("load");
        assert!(feed[1].reverted);
        assert_eq!(cache.len(), 2, "pruned to the returned commits");
        assert!(!cache.contains_key(&first));
    }

    #[test]
    fn detects_reverts() {
        let t = TestRepo::new();
        t.write("f.txt", "a\n");
        t.commit_at("init", 1_000);
        t.write("f.txt", "b\n");
        let bad = t.commit_at("bad change", 2_000);
        t.write("f.txt", "a\n");
        t.commit_at(
            &format!("Revert \"bad change\"\n\nThis reverts commit {bad}."),
            3_000,
        );

        let feed = load_commits(&t.repo, 10, &labeler()).expect("load");
        assert!(feed[0].is_revert);
        assert!(!feed[0].reverted);
        assert!(feed[1].reverted);
        assert!(!feed[2].reverted);
    }

    #[test]
    fn worktree_includes_untracked_deleted_and_renamed() {
        let t = TestRepo::new();
        t.write("keep.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
        t.write("gone.txt", "x\n");
        t.commit("init");
        t.remove("gone.txt");
        t.write("new/file.txt", "fresh\n");
        t.write("keep.txt", "1\n2\n3\n4\n5\n6\n7\n9\n");

        let wt = load_worktree(&t.repo, &labeler(), 42)
            .expect("load")
            .expect("dirty");
        assert!(wt.is_worktree());
        assert_eq!(wt.id, WORKTREE_ID);
        assert_eq!(wt.summary, "Uncommitted changes");
        assert!(wt.time > 42, "uses file mtime");
        let statuses: Vec<_> = wt
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status))
            .collect();
        assert!(statuses.contains(&("gone.txt", FileStatus::Deleted)));
        assert!(statuses.contains(&("new/file.txt", FileStatus::Added)));
        assert!(statuses.contains(&("keep.txt", FileStatus::Modified)));
    }

    #[test]
    fn worktree_time_falls_back_to_now_when_only_deletions() {
        let t = TestRepo::new();
        t.write("gone.txt", "x\n");
        t.commit("init");
        t.remove("gone.txt");
        let wt = load_worktree(&t.repo, &labeler(), 42)
            .expect("load")
            .expect("dirty");
        assert_eq!(wt.time, 42);
    }

    #[test]
    fn detects_renames_and_binaries() {
        let t = TestRepo::new();
        t.write("old.txt", "same content here\nline two\nline three\n");
        t.commit("init");
        t.remove("old.txt");
        t.write("renamed.txt", "same content here\nline two\nline three\n");
        std::fs::write(t.path().join("blob.bin"), [0_u8, 159, 146, 150, 0]).expect("bin");
        let oid = t.commit("rename");
        let feed = load_commits(&t.repo, 1, &labeler()).expect("load");
        assert_eq!(feed[0].target, Target::Commit(oid));
        let renamed = feed[0]
            .files
            .iter()
            .find(|f| f.path == "renamed.txt")
            .expect("renamed");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        assert!(feed[0].touches_path("old.txt"));
        let bin = feed[0]
            .files
            .iter()
            .find(|f| f.path == "blob.bin")
            .expect("bin");
        assert!(bin.binary);
        assert_eq!((bin.added, bin.removed), (0, 0));
    }
}
