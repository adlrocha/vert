//! Where lines of work collide: files two of them both changed, and the ones
//! that would really fail to merge.
//!
//! Each pair is compared from the point the two diverged, so commits they
//! share (a branch stacked on another) never count as a collision, and only
//! files a line of work changed itself count, so commits picked up from the
//! base branch are never blamed on it. Uncommitted changes only reach the
//! [`Level::Overlap`] level: there is no commit to merge yet.
//!
//! Without a base branch, a line of work is only known by its tip commit.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use git2::{ErrorCode, MergeOptions, Oid, Repository};

use super::branches::{Branch, Branches};
use super::feed::Entry;
use super::{Context, GitError, Target, make_diff, path_string};

/// Precedence of the in-memory object store over the repository's own, which
/// libgit2 fixes at 1 and 2. Writes go to the highest one that accepts them.
const SCRATCH_PRIORITY: i32 = 1000;

/// How seriously two lines of work collide on a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Both changed the file. It may still merge cleanly.
    Overlap,
    /// Merging the two fails on this file.
    Conflict,
}

/// A file two lines of work collide on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFile {
    /// Repository-relative path.
    pub path: String,
    /// How seriously they collide on it.
    pub level: Level,
}

/// Two lines of work that collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// Display name of one line of work.
    pub first: String,
    /// Display name of the other.
    pub second: String,
    /// The most serious level among `files`.
    pub level: Level,
    /// The files they collide on, in path order.
    pub files: Vec<SharedFile>,
}

/// Every collision between the lines of work of a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overlaps {
    /// Collisions, the most serious first, then by name.
    pub collisions: Vec<Collision>,
    /// Pairs of lines of work left for a later pass because
    /// [`Options::max_checks`] ran out. They reach [`Level::Overlap`] at most.
    pub unchecked: usize,
}

/// What to compare.
#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// Files that never count, by file name or repository-relative path.
    pub ignore: &'a [String],
    /// Maximum merge checks in one pass, one per distinct pair of tips. Results
    /// are cached, so the next pass carries on with the pairs left over.
    pub max_checks: usize,
}

/// What a merge check learned about a pair of commits, the lower id first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    lower: BTreeSet<String>,
    higher: BTreeSet<String>,
    conflicts: BTreeSet<String>,
}

/// Merge checks per pair of tips, the lower id first, shared across reloads.
/// `None` means the two share no history. Both commits are immutable, so a
/// reload only checks pairs that moved.
pub type MergeCache = HashMap<(Oid, Oid), Option<Merge>>;

/// One line of work, reduced to what a comparison needs.
struct Side<'a> {
    branch: &'a Branch,
    /// The tip to merge, or `None` when nothing is committed beyond the base.
    tip: Option<Oid>,
    /// Files changed by commits since the line of work left the base branch.
    committed: BTreeSet<String>,
    /// Files with uncommitted changes.
    dirty: BTreeSet<String>,
}

impl<'a> Side<'a> {
    fn new(branch: &'a Branch, ignore: &[String]) -> Self {
        let entry = branch.changes.as_ref().filter(|_| !branch.merged());
        let dirty = branch.worktree.as_ref().and_then(|w| w.dirty.as_ref());
        Self {
            branch,
            tip: entry.and_then(|e| match e.target {
                Target::Range { head, .. } => Some(head),
                Target::Commit(oid) => Some(oid),
                Target::WorkingTree | Target::PullRequest(_) => None,
            }),
            committed: entry.map_or_else(BTreeSet::new, |e| paths(e, ignore)),
            dirty: dirty.map_or_else(BTreeSet::new, |e| paths(e, ignore)),
        }
    }

    fn touched(&self) -> BTreeSet<&String> {
        self.committed.union(&self.dirty).collect()
    }

    /// What this side changes in a comparison whose committed part is `since`.
    fn changes(&self, since: &BTreeSet<String>) -> BTreeSet<String> {
        self.committed
            .intersection(since)
            .chain(&self.dirty)
            .cloned()
            .collect()
    }
}

/// The paths `entry` touches, both sides of a rename included.
fn paths(entry: &Entry, ignore: &[String]) -> BTreeSet<String> {
    entry
        .files
        .iter()
        .flat_map(|f| std::iter::once(&f.path).chain(&f.old_path))
        .filter(|p| !ignored(p, ignore))
        .cloned()
        .collect()
}

fn ignored(path: &str, ignore: &[String]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    ignore.iter().any(|i| i == path || i == name)
}

/// Finds every collision between `branches`.
///
/// Merge checks run on a separate handle whose writes stay in memory, because
/// libgit2 stores merged file contents as it goes and vert must leave the
/// repository as it found it.
///
/// # Errors
/// Returns [`GitError`] if the repository cannot be read.
pub fn find(
    repo: &Repository,
    branches: &Branches,
    opts: Options<'_>,
    cache: &mut MergeCache,
) -> Result<Overlaps, GitError> {
    let scratch = open_scratch(repo)?;
    let sides: Vec<_> = branches
        .branches
        .iter()
        .map(|b| Side::new(b, opts.ignore))
        .collect();
    let mut pass = Pass {
        repo: &scratch,
        cache,
        used: MergeCache::new(),
        checks_left: opts.max_checks,
        unchecked: 0,
    };
    let mut collisions = Vec::new();
    for (i, a) in sides.iter().enumerate() {
        for b in &sides[i + 1..] {
            collisions.extend(pass.compare(a, b)?);
        }
    }
    collisions.sort_by(|a: &Collision, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| a.first.cmp(&b.first))
            .then_with(|| a.second.cmp(&b.second))
    });
    let unchecked = pass.unchecked;
    let used = pass.used;
    *cache = used;
    Ok(Overlaps {
        collisions,
        unchecked,
    })
}

/// Opens `repo` again with an in-memory object store in front of its own.
fn open_scratch(repo: &Repository) -> Result<Repository, GitError> {
    let scratch = Repository::open(repo.path()).ctx("open the repository for merge checks")?;
    scratch
        .odb()
        .and_then(|odb| odb.add_new_mempack_backend(SCRATCH_PRIORITY).map(drop))
        .ctx("keep merge checks in memory")?;
    Ok(scratch)
}

/// The state of one [`find`] run.
struct Pass<'a> {
    repo: &'a Repository,
    cache: &'a MergeCache,
    used: MergeCache,
    checks_left: usize,
    unchecked: usize,
}

impl Pass<'_> {
    fn compare(&mut self, a: &Side<'_>, b: &Side<'_>) -> Result<Option<Collision>, GitError> {
        if a.touched().is_disjoint(&b.touched()) {
            return Ok(None);
        }
        let none = BTreeSet::new();
        let merge = match (a.tip, b.tip) {
            (Some(x), Some(y)) if x == y => {
                let changes = (a.changes(&none), b.changes(&none));
                return Ok(collision(a, b, &changes, &none));
            }
            (Some(x), Some(y)) => self.merge(x, y)?.map(|m| (x < y, m)),
            _ => None,
        };
        let Some((in_order, m)) = merge else {
            let changes = (a.changes(&a.committed), b.changes(&b.committed));
            return Ok(collision(a, b, &changes, &none));
        };
        let (a_since, b_since) = if in_order {
            (&m.lower, &m.higher)
        } else {
            (&m.higher, &m.lower)
        };
        let changes = (a.changes(a_since), b.changes(b_since));
        Ok(collision(a, b, &changes, &m.conflicts))
    }

    /// The merge check of a pair of tips: cached, or run if any are left. Past
    /// the limit, what each side changed still comes back, without conflicts.
    /// `None` when the two share no history.
    fn merge(&mut self, x: Oid, y: Oid) -> Result<Option<Merge>, GitError> {
        let key = (x.min(y), x.max(y));
        if let Some(known) = self.used.get(&key) {
            return Ok(known.clone());
        }
        if let Some(known) = self.cache.get(&key) {
            self.used.insert(key, known.clone());
            return Ok(known.clone());
        }
        if self.checks_left == 0 {
            self.unchecked += 1;
            return diverged(self.repo, key.0, key.1);
        }
        self.checks_left -= 1;
        let merge = check_merge(self.repo, key.0, key.1)?;
        self.used.insert(key, merge.clone());
        Ok(merge)
    }
}

fn collision(
    a: &Side<'_>,
    b: &Side<'_>,
    (a_changes, b_changes): &(BTreeSet<String>, BTreeSet<String>),
    conflicts: &BTreeSet<String>,
) -> Option<Collision> {
    let levels: BTreeMap<_, _> = a_changes
        .intersection(b_changes)
        .map(|path| {
            let level = if conflicts.contains(path) {
                Level::Conflict
            } else {
                Level::Overlap
            };
            (path.clone(), level)
        })
        .collect();
    let level = *levels.values().max()?;
    Some(Collision {
        first: a.branch.name.clone(),
        second: b.branch.name.clone(),
        level,
        files: levels
            .into_iter()
            .map(|(path, level)| SharedFile { path, level })
            .collect(),
    })
}

/// What `lower` and `higher` changed since they diverged, with no conflicts
/// looked for yet, or `None` without shared history.
fn diverged(repo: &Repository, lower: Oid, higher: Oid) -> Result<Option<Merge>, GitError> {
    let base = match repo.merge_base(lower, higher) {
        Ok(base) => base,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(source) => {
            return Err(GitError::Op {
                context: "find where two branches diverged",
                source,
            });
        }
    };
    Ok(Some(Merge {
        lower: changed_since(repo, base, lower)?,
        higher: changed_since(repo, base, higher)?,
        conflicts: BTreeSet::new(),
    }))
}

/// Merges `lower` and `higher` in memory, or `None` without shared history.
fn check_merge(repo: &Repository, lower: Oid, higher: Oid) -> Result<Option<Merge>, GitError> {
    let Some(merge) = diverged(repo, lower, higher)? else {
        return Ok(None);
    };
    let ours = repo.find_commit(lower).ctx("read branch tip")?;
    let theirs = repo.find_commit(higher).ctx("read branch tip")?;
    let mut opts = MergeOptions::new();
    opts.find_renames(true);
    let index = repo
        .merge_commits(&ours, &theirs, Some(&opts))
        .ctx("check whether two branches merge")?;
    let mut conflicts = BTreeSet::new();
    for conflict in index.conflicts().ctx("list merge conflicts")? {
        let conflict = conflict.ctx("read merge conflict")?;
        conflicts.extend(
            [conflict.ancestor, conflict.our, conflict.their]
                .into_iter()
                .flatten()
                .map(|e| String::from_utf8_lossy(&e.path).into_owned()),
        );
    }
    Ok(Some(Merge { conflicts, ..merge }))
}

/// The paths changed between `base` and `head`, both sides of a rename included.
fn changed_since(repo: &Repository, base: Oid, head: Oid) -> Result<BTreeSet<String>, GitError> {
    let target = Target::Range {
        base: Some(base),
        head,
    };
    Ok(make_diff(repo, target)?
        .deltas()
        .flat_map(|d| {
            [
                path_string(d.old_file().path()),
                path_string(d.new_file().path()),
            ]
        })
        .flatten()
        .collect())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use git2::BranchType;

    use super::*;
    use crate::git::branches::{self, RangeCache};
    use crate::git::testutil::TestRepo;
    use crate::labels::Labeler;

    const LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";

    /// [`LINES`] with line `number` replaced.
    fn with_line(number: usize, text: &str) -> String {
        let lines: Vec<_> = LINES
            .lines()
            .enumerate()
            .map(|(i, l)| if i + 1 == number { text } else { l })
            .collect();
        lines.join("\n") + "\n"
    }

    fn repo_with_base() -> TestRepo {
        let t = TestRepo::new();
        t.write("shared.txt", LINES);
        t.write("other.txt", "other\n");
        t.commit("base");
        t.rename_head_to_main();
        t
    }

    /// Commits `file = content` on `branch` (created from `HEAD` if new)
    /// without touching the working tree. `None` removes the file.
    fn commit_on(t: &TestRepo, branch: &str, file: &str, content: Option<&str>) -> Oid {
        let parent = t
            .repo
            .find_branch(branch, BranchType::Local)
            .map_or_else(
                |_| t.repo.head().and_then(|h| h.peel_to_commit()),
                |b| b.get().peel_to_commit(),
            )
            .expect("parent");
        let mut tb = t
            .repo
            .treebuilder(Some(&parent.tree().expect("tree")))
            .expect("treebuilder");
        match content {
            Some(c) => {
                let blob = t.repo.blob(c.as_bytes()).expect("blob");
                tb.insert(file, blob, 0o100_644).expect("insert");
            }
            None => tb.remove(file).expect("remove"),
        }
        let tree = t.repo.find_tree(tb.write().expect("write")).expect("tree");
        let sig = git2::Signature::new("Agent", "a@x", &git2::Time::new(1000, 0)).expect("sig");
        let oid = t
            .repo
            .commit(None, &sig, &sig, "edit", &tree, &[&parent])
            .expect("commit");
        t.repo
            .reference(&format!("refs/heads/{branch}"), oid, true, "test")
            .expect("ref");
        oid
    }

    fn load(repo: &Repository) -> Branches {
        let labeler = Labeler::new(5, 20, 100, 2, 1).expect("labeler");
        let opts = branches::Options {
            base: None,
            limit: 10,
            labeler: &labeler,
            now: 50_000,
        };
        branches::load(repo, opts, &mut RangeCache::new()).expect("branches")
    }

    fn scan(t: &TestRepo, ignore: &[String]) -> Overlaps {
        let opts = Options {
            ignore,
            max_checks: 100,
        };
        find(&t.repo, &load(&t.repo), opts, &mut MergeCache::new()).expect("find")
    }

    fn only(found: &Overlaps) -> &Collision {
        assert_eq!(found.collisions.len(), 1, "{found:?}");
        &found.collisions[0]
    }

    fn level_of(c: &Collision, path: &str) -> Option<Level> {
        c.files.iter().find(|f| f.path == path).map(|f| f.level)
    }

    fn object_files(dir: &Path) -> usize {
        fs::read_dir(dir).expect("read dir").fold(0, |n, e| {
            let path = e.expect("entry").path();
            n + if path.is_dir() {
                object_files(&path)
            } else {
                1
            }
        })
    }

    #[test]
    fn edits_to_the_same_lines_conflict() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("one\n2\n"));
        commit_on(&t, "b", "shared.txt", Some("uno\n2\n"));
        let found = scan(&t, &[]);
        let c = only(&found);
        assert_eq!((c.first.as_str(), c.second.as_str()), ("a", "b"));
        assert_eq!(c.level, Level::Conflict);
        assert_eq!(level_of(c, "shared.txt"), Some(Level::Conflict));
    }

    #[test]
    fn edits_far_apart_in_a_file_only_overlap() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some(&with_line(2, "two")));
        commit_on(&t, "b", "shared.txt", Some(&with_line(11, "x")));
        let found = scan(&t, &[]);
        assert_eq!(only(&found).level, Level::Overlap);
    }

    #[test]
    fn a_file_level_is_kept_per_file() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some(&with_line(2, "two")));
        commit_on(&t, "b", "shared.txt", Some(&with_line(11, "x")));
        commit_on(&t, "a", "other.txt", Some("from a\n"));
        commit_on(&t, "b", "other.txt", Some("from b\n"));
        let found = scan(&t, &[]);
        let c = only(&found);
        assert_eq!(c.level, Level::Conflict);
        assert_eq!(level_of(c, "other.txt"), Some(Level::Conflict));
        assert_eq!(level_of(c, "shared.txt"), Some(Level::Overlap));
    }

    #[test]
    fn separate_files_do_not_collide() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "other.txt", Some("b\n"));
        assert_eq!(scan(&t, &[]), Overlaps::default());
    }

    #[test]
    fn a_branch_stacked_on_another_does_not_collide_with_it() {
        let t = repo_with_base();
        let tip = commit_on(&t, "a", "shared.txt", Some("a\n"));
        t.repo
            .branch("b", &t.repo.find_commit(tip).expect("tip"), false)
            .expect("branch");
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        assert_eq!(scan(&t, &[]), Overlaps::default());
    }

    #[test]
    fn branches_on_the_same_commit_do_not_collide() {
        let t = repo_with_base();
        let tip = commit_on(&t, "a", "shared.txt", Some("a\n"));
        t.repo
            .branch("b", &t.repo.find_commit(tip).expect("tip"), false)
            .expect("branch");
        assert_eq!(scan(&t, &[]), Overlaps::default());
    }

    #[test]
    fn a_merged_branch_does_not_collide() {
        let t = repo_with_base();
        let head = t.repo.head().expect("head").peel_to_commit().expect("head");
        t.repo.branch("done", &head, false).expect("branch");
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        assert_eq!(scan(&t, &[]), Overlaps::default());
    }

    #[test]
    fn work_on_the_base_branch_is_not_blamed_on_a_branch_that_merged_it() {
        let t = repo_with_base();
        let start = t.repo.head().expect("head").peel_to_commit().expect("head");
        t.repo.branch("old", &start, false).expect("branch");
        commit_on(&t, "old", "shared.txt", Some("old\n"));
        t.write("shared.txt", "newer\n");
        t.commit("base moves on");
        let moved = t.repo.head().expect("head").peel_to_commit().expect("head");
        t.repo.branch("fresh", &moved, false).expect("branch");
        commit_on(&t, "fresh", "other.txt", Some("fresh\n"));
        assert_eq!(scan(&t, &[]), Overlaps::default());
    }

    #[test]
    fn uncommitted_changes_overlap_but_never_conflict() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        t.write("shared.txt", "uncommitted\n");
        let found = scan(&t, &[]);
        let c = only(&found);
        assert_eq!(c.level, Level::Overlap);
        assert_eq!(level_of(c, "shared.txt"), Some(Level::Overlap));
    }

    #[test]
    fn uncommitted_changes_count_against_a_branch_stacked_on_the_checkout() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        t.repo
            .set_head("refs/heads/a")
            .expect("switch to the branch");
        t.repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("checkout");
        commit_on(&t, "b", "other.txt", Some("b\n"));
        t.write("other.txt", "uncommitted\n");
        let found = scan(&t, &[]);
        assert_eq!(level_of(only(&found), "other.txt"), Some(Level::Overlap));
    }

    #[test]
    fn ignored_files_never_count_by_name_or_path() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        assert_eq!(scan(&t, &["shared.txt".to_owned()]), Overlaps::default());
        assert!(ignored("deep/down/Cargo.lock", &["Cargo.lock".to_owned()]));
        assert!(ignored("deep/x.rs", &["deep/x.rs".to_owned()]));
        assert!(!ignored("deep/x.rs", &["down/x.rs".to_owned()]));
    }

    #[test]
    fn branches_of_one_name_on_two_remotes_are_compared() {
        let t = repo_with_base();
        let ours = commit_on(&t, "ours", "shared.txt", Some("ours\n"));
        let theirs = commit_on(&t, "theirs", "shared.txt", Some("theirs\n"));
        for (name, oid) in [("origin", ours), ("upstream", theirs)] {
            t.repo
                .reference(&format!("refs/remotes/{name}/feat"), oid, true, "test")
                .expect("remote ref");
        }
        for local in ["ours", "theirs"] {
            t.repo
                .find_branch(local, BranchType::Local)
                .and_then(|mut b| b.delete())
                .expect("delete");
        }
        assert_eq!(only(&scan(&t, &[])).level, Level::Conflict);
    }

    #[test]
    fn a_rename_merges_with_an_edit_of_the_old_name() {
        let t = repo_with_base();
        commit_on(&t, "edit", "shared.txt", Some(&with_line(2, "two")));
        commit_on(&t, "move", "moved.txt", Some(LINES));
        commit_on(&t, "move", "shared.txt", None);
        let found = scan(&t, &[]);
        let c = only(&found);
        assert_eq!(c.level, Level::Overlap);
        assert_eq!(level_of(c, "shared.txt"), Some(Level::Overlap));
    }

    #[test]
    fn deleting_a_file_another_branch_edits_conflicts() {
        let t = repo_with_base();
        commit_on(&t, "edit", "other.txt", Some("edited\n"));
        commit_on(&t, "gone", "other.txt", None);
        let found = scan(&t, &[]);
        assert_eq!(level_of(only(&found), "other.txt"), Some(Level::Conflict));
    }

    #[test]
    fn binary_edits_of_one_file_conflict() {
        let t = repo_with_base();
        commit_on(&t, "a", "other.txt", Some("a\0binary\n"));
        commit_on(&t, "b", "other.txt", Some("b\0binary\n"));
        let found = scan(&t, &[]);
        assert_eq!(level_of(only(&found), "other.txt"), Some(Level::Conflict));
    }

    #[test]
    fn unrelated_histories_are_compared_by_file() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        let blob = t.repo.blob(b"orphan\n").expect("blob");
        let mut tb = t.repo.treebuilder(None).expect("treebuilder");
        tb.insert("shared.txt", blob, 0o100_644).expect("insert");
        let tree = t.repo.find_tree(tb.write().expect("write")).expect("tree");
        let sig = git2::Signature::new("Agent", "a@x", &git2::Time::new(1000, 0)).expect("sig");
        let orphan = t
            .repo
            .commit(None, &sig, &sig, "orphan", &tree, &[])
            .expect("commit");
        t.repo
            .reference("refs/heads/orphan", orphan, true, "test")
            .expect("ref");
        let found = scan(&t, &[]);
        assert_eq!(only(&found).level, Level::Overlap);
    }

    #[test]
    fn checks_leave_the_repository_untouched() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some(&with_line(2, "two")));
        commit_on(&t, "b", "shared.txt", Some(&with_line(11, "x")));
        commit_on(&t, "c", "shared.txt", Some(&with_line(2, "dos")));
        let objects = t.repo.path().join("objects");
        let before = object_files(&objects);
        let found = scan(&t, &[]);
        assert_eq!(found.collisions.len(), 3);
        assert_eq!(object_files(&objects), before);
    }

    #[test]
    fn checks_work_from_a_linked_worktree() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = t
            .repo
            .worktree("linked", &dir.path().join("linked"), None)
            .expect("worktree");
        let linked = Repository::open_from_worktree(&wt).expect("open");
        let opts = Options {
            ignore: &[],
            max_checks: 100,
        };
        let list = load(&linked);
        let found = find(&linked, &list, opts, &mut MergeCache::new()).expect("find");
        assert_eq!(only(&found).level, Level::Conflict);
    }

    #[test]
    fn pairs_past_the_limit_fall_back_to_files_and_finish_on_the_next_pass() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        let list = load(&t.repo);
        let mut cache = MergeCache::new();
        let mut pass = |max_checks| {
            let opts = Options {
                ignore: &[],
                max_checks,
            };
            find(&t.repo, &list, opts, &mut cache).expect("find")
        };
        let first = pass(0);
        assert_eq!(first.unchecked, 1);
        assert_eq!(only(&first).level, Level::Overlap);
        let second = pass(1);
        assert_eq!(second.unchecked, 0);
        assert_eq!(only(&second).level, Level::Conflict);
        let cached = pass(0);
        assert_eq!(cached.unchecked, 0);
        assert_eq!(only(&cached).level, Level::Conflict);
    }

    #[test]
    fn a_stacked_branch_past_the_limit_is_not_blamed_for_what_it_inherited() {
        let t = repo_with_base();
        let tip = commit_on(&t, "lower", "shared.txt", Some("lower\n"));
        let commit = t.repo.find_commit(tip).expect("commit");
        t.repo.branch("upper", &commit, false).expect("branch");
        commit_on(&t, "upper", "other.txt", Some("upper\n"));
        let opts = Options {
            ignore: &[],
            max_checks: 0,
        };
        let list = load(&t.repo);
        let found = find(&t.repo, &list, opts, &mut MergeCache::new()).expect("find");
        assert_eq!(found.collisions, []);
        assert_eq!(found.unchecked, 1);
    }

    #[test]
    fn the_cache_only_keeps_pairs_still_in_use() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        let opts = Options {
            ignore: &[],
            max_checks: 100,
        };
        let mut cache = MergeCache::new();
        find(&t.repo, &load(&t.repo), opts, &mut cache).expect("find");
        let stale: Vec<_> = cache.keys().copied().collect();
        commit_on(&t, "b", "shared.txt", Some("b2\n"));
        find(&t.repo, &load(&t.repo), opts, &mut cache).expect("find");
        assert_eq!(cache.len(), 1);
        assert!(stale.iter().all(|k| !cache.contains_key(k)));
    }

    #[test]
    fn branches_on_one_tip_share_a_merge_check() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        let tip = commit_on(&t, "b", "shared.txt", Some("b\n"));
        t.repo
            .branch("c", &t.repo.find_commit(tip).expect("tip"), false)
            .expect("branch");
        let opts = Options {
            ignore: &[],
            max_checks: 1,
        };
        let mut cache = MergeCache::new();
        let found = find(&t.repo, &load(&t.repo), opts, &mut cache).expect("find");
        assert_eq!(found.unchecked, 0);
        assert_eq!(found.collisions.len(), 2);
        assert!(found.collisions.iter().all(|c| c.level == Level::Conflict));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_row_without_a_tip_is_compared_by_file() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some("a\n"));
        commit_on(&t, "b", "shared.txt", Some("b\n"));
        let mut list = load(&t.repo);
        let row = list
            .branches
            .iter_mut()
            .find(|b| b.name == "b")
            .and_then(|b| b.changes.as_mut())
            .expect("row");
        row.target = Target::PullRequest(1);
        let opts = Options {
            ignore: &[],
            max_checks: 100,
        };
        let found = find(&t.repo, &list, opts, &mut MergeCache::new()).expect("find");
        assert_eq!(only(&found).level, Level::Overlap);
    }

    #[test]
    fn the_most_serious_collisions_come_first() {
        let t = repo_with_base();
        commit_on(&t, "a", "shared.txt", Some(&with_line(2, "two")));
        commit_on(&t, "b", "shared.txt", Some(&with_line(11, "x")));
        commit_on(&t, "c", "shared.txt", Some(&with_line(11, "y")));
        let found = scan(&t, &[]);
        let order: Vec<_> = found
            .collisions
            .iter()
            .map(|c| (c.first.as_str(), c.second.as_str(), c.level))
            .collect();
        assert_eq!(
            order,
            [
                ("b", "c", Level::Conflict),
                ("a", "b", Level::Overlap),
                ("a", "c", Level::Overlap),
            ]
        );
    }
}
