//! Branches and worktrees: every line of work in the repository, with what it
//! changes against the base branch and any uncommitted work in its checkout.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use git2::{BranchType, Oid, Repository};

use super::feed::{Entry, WORKTREE_ID, file_stats, load_worktree, lossy};
use super::{Context, FileStat, GitError, Target, short_id};
use crate::labels::Labeler;

/// Remote and branch names tried, in order, when no base branch is given.
const BASE_CANDIDATES: [&str; 4] = ["origin/main", "origin/master", "main", "master"];

/// A checkout of the repository: the main working directory or a linked worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Directory name, for display.
    pub name: String,
    /// Absolute path of the checkout.
    pub path: PathBuf,
    /// Whether this is the checkout vert is running in.
    pub current: bool,
    /// Uncommitted changes, or `None` when clean.
    pub dirty: Option<Entry>,
}

/// One line of work: a branch, a detached checkout, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Display name: `feat/x`, `origin/feat/x` for a branch that only exists on
    /// a remote, or the worktree name for a detached checkout.
    pub name: String,
    /// The branch name without any remote prefix, used to match pull requests.
    /// `None` for a detached checkout.
    pub branch: Option<String>,
    /// Whether the branch only exists on a remote.
    pub remote: bool,
    /// Commits the branch has that the base lacks, and the reverse.
    /// `None` without a base branch or a shared history.
    pub ahead_behind: Option<(usize, usize)>,
    /// What the branch adds since it left the base branch, or `None` when the
    /// checkout has no commits yet.
    pub changes: Option<Entry>,
    /// The checkout this branch is checked out in, if any.
    pub worktree: Option<Worktree>,
}

impl Branch {
    /// Whether the branch has nothing the base branch lacks.
    #[must_use]
    pub fn merged(&self) -> bool {
        matches!(self.ahead_behind, Some((0, _)))
    }

    /// Time of the latest activity: uncommitted edits or the tip commit.
    #[must_use]
    pub fn time(&self) -> i64 {
        let dirty = self
            .worktree
            .as_ref()
            .and_then(|w| w.dirty.as_ref())
            .map(|e| e.time);
        let tip = self.changes.as_ref().map(|e| e.time);
        dirty.max(tip).unwrap_or(0)
    }
}

/// Every branch and worktree, worktrees first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branches {
    /// The base branch changes are measured against, if one was found.
    pub base: Option<String>,
    /// Worktrees (the current one first), then other branches, newest first.
    pub branches: Vec<Branch>,
}

/// Changed files per `(merge base, tip)`, shared across reloads. Both commits
/// are immutable, so a reload only diffs branches that moved.
pub type RangeCache = HashMap<(Option<Oid>, Oid), Vec<FileStat>>;

/// What to load.
#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// Branch to compare against; detected when `None`.
    pub base: Option<&'a str>,
    /// Maximum branches without a worktree to include (most recent first).
    pub limit: usize,
    /// Change labelling thresholds.
    pub labeler: &'a Labeler,
    /// Current time, used as the fallback timestamp when a checkout's changed
    /// files have no readable modification time.
    pub now: i64,
}

/// Loads every worktree and up to `opts.limit` other branches of `repo`.
///
/// # Errors
/// Returns [`GitError`] if the repository cannot be read, or if `opts.base`
/// names a branch that does not exist.
pub fn load(
    repo: &Repository,
    opts: Options<'_>,
    cache: &mut RangeCache,
) -> Result<Branches, GitError> {
    let base = resolve_base(repo, opts.base)?;
    let base_oid = base.as_ref().map(|(_, oid)| *oid);
    let worktrees = list_worktrees(repo, opts)?;
    let mut used = RangeCache::new();
    let mut tip_of = |head: Oid, root: Option<&Path>, name: &str, branch: Option<&str>| {
        changes(
            repo,
            head,
            base_oid,
            opts.labeler,
            (cache, &mut used),
            (name, root),
        )
        .map(|(entry, ahead_behind)| Branch {
            name: name.to_owned(),
            branch: branch.map(str::to_owned),
            remote: false,
            ahead_behind,
            changes: Some(entry),
            worktree: None,
        })
    };

    let mut out = Vec::new();
    let mut checked_out = HashSet::new();
    for (wt, head) in worktrees {
        let branch = head.as_ref().and_then(|h| h.branch.clone());
        let name = branch.clone().unwrap_or_else(|| wt.name.clone());
        let root = (!wt.current).then_some(wt.path.as_path());
        let mut row = match head.as_ref() {
            Some(h) => tip_of(h.oid, root, &name, branch.as_deref())?,
            None => Branch {
                name: name.clone(),
                branch: branch.clone(),
                remote: false,
                ahead_behind: None,
                changes: None,
                worktree: None,
            },
        };
        if let Some(b) = branch {
            checked_out.insert(b);
        }
        row.worktree = Some(wt);
        out.push(row);
    }

    let mut others = other_branches(repo, &checked_out, base.as_ref().map(|(n, _)| n.as_str()))?;
    others.sort_by_key(|b| std::cmp::Reverse(b.time));
    for b in others.into_iter().take(opts.limit) {
        let mut row = tip_of(b.oid, None, &b.display, Some(&b.branch))?;
        row.remote = b.remote;
        out.push(row);
    }
    *cache = used;
    Ok(Branches {
        base: base.map(|(name, _)| name),
        branches: out,
    })
}

/// The branch to compare against: `requested`, else the remote's default
/// branch, else the first of [`BASE_CANDIDATES`] that exists.
fn resolve_base(
    repo: &Repository,
    requested: Option<&str>,
) -> Result<Option<(String, Oid)>, GitError> {
    let peel = |spec: &str| {
        repo.revparse_single(spec)
            .and_then(|o| o.peel_to_commit())
            .map(|c| c.id())
    };
    if let Some(name) = requested {
        return peel(name)
            .map(|oid| Some((name.to_owned(), oid)))
            .ctx("find the --base branch");
    }
    let remote_default = repo
        .find_reference("refs/remotes/origin/HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_owned))
        .and_then(|t| t.strip_prefix("refs/remotes/").map(str::to_owned));
    Ok(remote_default
        .into_iter()
        .chain(BASE_CANDIDATES.iter().map(|s| (*s).to_owned()))
        .find_map(|name| peel(&name).ok().map(|oid| (name, oid))))
}

/// The commit a checkout is on.
struct Head {
    oid: Oid,
    branch: Option<String>,
}

/// The main checkout plus every valid linked worktree, the current one first.
fn list_worktrees(
    repo: &Repository,
    opts: Options<'_>,
) -> Result<Vec<(Worktree, Option<Head>)>, GitError> {
    let current = canonical(&super::workdir(repo)?);
    let main = Repository::open(repo.commondir()).ctx("open the main checkout")?;
    let mut checkouts: Vec<Repository> = Vec::new();
    if main.workdir().is_some() {
        checkouts.push(main);
    }
    let names = repo.worktrees().ctx("list worktrees")?;
    for name in names.iter().filter_map(|n| n.ok().flatten()) {
        let Ok(wt) = repo.find_worktree(name) else {
            continue;
        };
        // Worktrees whose directory was deleted are skipped until pruned.
        if wt.validate().is_ok()
            && let Ok(r) = Repository::open_from_worktree(&wt)
        {
            checkouts.push(r);
        }
    }
    let mut out = checkouts
        .iter()
        .map(|r| {
            let path = canonical(&super::workdir(r)?);
            let is_current = path == current;
            let name = path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
            let mut dirty = load_worktree(r, opts.labeler, opts.now)?;
            if let Some(e) = dirty.as_mut().filter(|_| !is_current) {
                e.id = format!("{WORKTREE_ID}:{}", path.display());
                e.summary = format!("Uncommitted changes in {name}");
                e.root = Some(path.clone());
            }
            let head = r.head().ok().and_then(|h| {
                let oid = h.peel_to_commit().ok()?.id();
                let branch = h
                    .is_branch()
                    .then(|| String::from_utf8_lossy(h.shorthand_bytes()).into_owned());
                Some(Head { oid, branch })
            });
            let wt = Worktree {
                name,
                path,
                current: is_current,
                dirty,
            };
            Ok((wt, head))
        })
        .collect::<Result<Vec<_>, GitError>>()?;
    out.sort_by(|(a, _), (b, _)| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// A branch not checked out anywhere.
struct Candidate {
    display: String,
    branch: String,
    oid: Oid,
    time: i64,
    remote: bool,
}

/// Local and remote branches that are not checked out and are not the base.
/// A remote branch is dropped when it is a local branch's upstream, or when an
/// untracked local branch has the same name. A local branch counts as the base
/// when it is the base, tracks it, or is an untracked branch of the same name.
fn other_branches(
    repo: &Repository,
    checked_out: &HashSet<String>,
    base: Option<&str>,
) -> Result<Vec<Candidate>, GitError> {
    let base_local = base.map(|b| b.split_once('/').map_or(b, |(_, rest)| rest));
    let mut upstreams = HashSet::new();
    let mut untracked = HashSet::new();
    let mut out = Vec::new();
    for kind in [BranchType::Local, BranchType::Remote] {
        for item in repo.branches(Some(kind)).ctx("list branches")? {
            let (branch, _) = item.ctx("read branch")?;
            let reference = branch.get();
            let display = String::from_utf8_lossy(reference.shorthand_bytes()).into_owned();
            let Some(commit) = reference.peel_to_commit().ok() else {
                continue;
            };
            let remote = kind == BranchType::Remote;
            let plain = if remote {
                match display.split_once('/') {
                    Some((_, "HEAD")) | None => continue,
                    Some((_, rest)) => rest.to_owned(),
                }
            } else {
                display.clone()
            };
            let is_base = if remote {
                Some(display.as_str()) == base
            } else {
                let upstream = branch
                    .upstream()
                    .ok()
                    .map(|u| String::from_utf8_lossy(u.get().shorthand_bytes()).into_owned());
                let is_base = Some(plain.as_str()) == base
                    || match &upstream {
                        Some(u) => Some(u.as_str()) == base,
                        None => Some(plain.as_str()) == base_local,
                    };
                match upstream {
                    Some(u) => upstreams.insert(u),
                    None => untracked.insert(plain.clone()),
                };
                is_base
            };
            let duplicate = remote && (upstreams.contains(&display) || untracked.contains(&plain));
            if is_base || duplicate || (!remote && checked_out.contains(&plain)) {
                continue;
            }
            out.push(Candidate {
                display,
                branch: plain,
                oid: commit.id(),
                time: commit.time().seconds(),
                remote,
            });
        }
    }
    Ok(out)
}

/// The entry describing what `head` adds since it left `base`, and how far
/// ahead and behind it is. Without a shared base, the entry is the tip commit.
fn changes(
    repo: &Repository,
    head: Oid,
    base: Option<Oid>,
    labeler: &Labeler,
    (cache, used): (&RangeCache, &mut RangeCache),
    (name, root): (&str, Option<&Path>),
) -> Result<(Entry, Option<(usize, usize)>), GitError> {
    let commit = repo.find_commit(head).ctx("read branch tip")?;
    let merge_base = base.and_then(|b| repo.merge_base(head, b).ok());
    let ahead_behind = match (merge_base, base) {
        (Some(_), Some(b)) => Some(
            repo.graph_ahead_behind(head, b)
                .ctx("count commits ahead and behind")?,
        ),
        _ => None,
    };
    let target = match merge_base {
        Some(mb) => Target::Range {
            base: Some(mb),
            head,
        },
        None => Target::Commit(head),
    };
    let key = (merge_base, head);
    let files = match cache.get(&key) {
        Some(files) => files.clone(),
        None => file_stats(repo, target)?,
    };
    used.insert(key, files.clone());
    let mut entry = Entry::new(target, files, labeler);
    entry.id = format!("branch:{name}");
    entry.short = if merge_base.is_some() {
        name.to_owned()
    } else {
        short_id(head)
    };
    entry.summary = commit
        .summary_bytes()
        .map_or_else(|| "(no message)".to_owned(), lossy);
    entry.author = lossy(commit.author().name_bytes());
    entry.time = commit.time().seconds();
    entry.root = root.map(Path::to_path_buf);
    Ok((entry, ahead_behind))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::TestRepo;

    fn labeler() -> Labeler {
        Labeler::new(5, 20, 100, 2, 1).expect("labeler")
    }

    fn opts<'a>(labeler: &'a Labeler, base: Option<&'a str>) -> Options<'a> {
        Options {
            base,
            limit: 10,
            labeler,
            now: 50_000,
        }
    }

    /// Commits `file = content` on top of `branch` (created from `HEAD` if new)
    /// without touching the working tree.
    fn commit_on(t: &TestRepo, branch: &str, file: &str, content: &str, time: i64) -> Oid {
        let parent = t
            .repo
            .find_branch(branch, BranchType::Local)
            .map_or_else(
                |_| t.repo.head().and_then(|h| h.peel_to_commit()),
                |b| b.get().peel_to_commit(),
            )
            .expect("parent");
        let blob = t.repo.blob(content.as_bytes()).expect("blob");
        let mut tb = t
            .repo
            .treebuilder(Some(&parent.tree().expect("tree")))
            .expect("treebuilder");
        tb.insert(file, blob, 0o100_644).expect("insert");
        let tree = t.repo.find_tree(tb.write().expect("write")).expect("tree");
        let sig = git2::Signature::new("Agent", "a@x", &git2::Time::new(time, 0)).expect("sig");
        let oid = t
            .repo
            .commit(None, &sig, &sig, &format!("edit {file}"), &tree, &[&parent])
            .expect("commit");
        t.repo
            .reference(&format!("refs/heads/{branch}"), oid, true, "test")
            .expect("ref");
        oid
    }

    fn base_repo() -> TestRepo {
        let t = TestRepo::new();
        t.write("a.txt", "a\n");
        let oid = t.commit_at("init", 1_000);
        let name = t
            .repo
            .head()
            .expect("head")
            .shorthand()
            .expect("name")
            .to_owned();
        if name != "main" {
            let commit = t.repo.find_commit(oid).expect("commit");
            t.repo.branch("main", &commit, true).expect("main");
            t.repo.set_head("refs/heads/main").expect("set head");
            t.repo
                .find_branch(&name, BranchType::Local)
                .expect("old")
                .delete()
                .expect("delete");
        }
        t
    }

    fn find<'a>(b: &'a Branches, name: &str) -> &'a Branch {
        b.branches
            .iter()
            .find(|x| x.name == name)
            .unwrap_or_else(|| panic!("no branch {name}: {b:#?}"))
    }

    #[test]
    fn lists_branches_against_the_base() {
        let t = base_repo();
        commit_on(&t, "feat", "b.txt", "one\ntwo\n", 2_000);
        commit_on(&t, "feat", "c.txt", "three\n", 3_000);
        commit_on(&t, "main", "m.txt", "m\n", 2_500);
        commit_on(&t, "old", "o.txt", "o\n", 1_500);
        let l = labeler();
        let mut cache = RangeCache::new();
        let b = load(&t.repo, opts(&l, None), &mut cache).expect("load");
        assert_eq!(b.base.as_deref(), Some("main"));

        let current = &b.branches[0];
        assert_eq!(current.name, "main");
        let wt = current.worktree.as_ref().expect("worktree");
        assert!(wt.current);
        assert_eq!(current.ahead_behind, Some((0, 0)));
        assert!(current.merged());

        let feat = find(&b, "feat");
        assert_eq!(feat.ahead_behind, Some((2, 1)));
        assert!(!feat.merged() && !feat.remote && feat.worktree.is_none());
        let changes = feat.changes.as_ref().expect("changes");
        assert_eq!(changes.files.len(), 2, "only the branch's own files");
        assert_eq!((changes.added, changes.removed), (3, 0));
        assert_eq!(changes.summary, "edit c.txt");
        assert_eq!(changes.id, "branch:feat");
        assert_eq!(changes.short, "feat");
        assert_eq!(feat.time(), 3_000);
        assert!(matches!(
            changes.target,
            Target::Range { base: Some(_), .. }
        ));
        let files = crate::git::diff::load(&t.repo, changes.target).expect("diff");
        assert_eq!(files.len(), 2);

        // Newest first after the worktrees.
        let order: Vec<_> = b.branches.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(order, ["main", "feat", "old"]);
        assert_eq!(cache.len(), 3);

        let limited = load(
            &t.repo,
            Options {
                limit: 1,
                ..opts(&l, None)
            },
            &mut cache,
        )
        .expect("load");
        assert_eq!(limited.branches.len(), 2);
        assert_eq!(cache.len(), 2, "pruned to what was loaded");
    }

    #[test]
    fn reuses_cached_ranges() {
        let t = base_repo();
        commit_on(&t, "feat", "b.txt", "b\n", 2_000);
        let l = labeler();
        let mut cache = RangeCache::new();
        load(&t.repo, opts(&l, None), &mut cache).expect("load");
        for files in cache.values_mut() {
            files.clear();
        }
        let b = load(&t.repo, opts(&l, None), &mut cache).expect("load");
        let feat = find(&b, "feat").changes.as_ref().expect("changes");
        assert!(feat.files.is_empty(), "served from the cache");
    }

    #[test]
    fn remote_branches_and_detected_base() {
        let t = base_repo();
        let tip = commit_on(&t, "main", "m.txt", "m\n", 2_000);
        let r = &t.repo;
        r.reference("refs/remotes/origin/main", tip, true, "t")
            .expect("ref");
        r.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
            true,
            "t",
        )
        .expect("sym");
        let feat = commit_on(&t, "feat", "f.txt", "f\n", 3_000);
        r.reference("refs/remotes/origin/feat", feat, true, "t")
            .expect("ref");
        let pr = commit_on(&t, "tmp", "p.txt", "p\n", 4_000);
        r.reference("refs/remotes/origin/agent/pr", pr, true, "t")
            .expect("ref");
        r.find_branch("tmp", BranchType::Local)
            .expect("tmp")
            .delete()
            .expect("delete");

        let l = labeler();
        let b = load(r, opts(&l, None), &mut RangeCache::new()).expect("load");
        assert_eq!(b.base.as_deref(), Some("origin/main"));
        let names: Vec<_> = b.branches.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            ["main", "origin/agent/pr", "feat"],
            "no duplicates or base"
        );
        let remote = find(&b, "origin/agent/pr");
        assert!(remote.remote);
        assert_eq!(remote.branch.as_deref(), Some("agent/pr"));
        assert_eq!(remote.ahead_behind, Some((1, 0)));
    }

    #[test]
    fn fork_remotes_keep_their_own_rows() {
        let t = base_repo();
        let tip = commit_on(&t, "main", "m.txt", "m\n", 2_000);
        let r = &t.repo;
        for name in ["origin", "upstream"] {
            r.remote(name, &format!("https://example.com/{name}/r.git"))
                .expect("remote");
        }
        r.reference("refs/remotes/origin/main", tip, true, "t")
            .expect("ref");
        r.find_branch("main", BranchType::Local)
            .expect("main")
            .set_upstream(Some("origin/main"))
            .expect("track");
        let theirs = commit_on(&t, "tmp", "u.txt", "u\n", 3_000);
        r.reference("refs/remotes/upstream/main", theirs, true, "t")
            .expect("ref");
        let dev = commit_on(&t, "dev", "d.txt", "d\n", 4_000);
        r.reference("refs/remotes/upstream/dev", dev, true, "t")
            .expect("ref");
        r.find_branch("dev", BranchType::Local)
            .expect("dev")
            .set_upstream(Some("upstream/dev"))
            .expect("track");
        r.find_branch("tmp", BranchType::Local)
            .expect("tmp")
            .delete()
            .expect("delete");

        let l = labeler();
        let b = load(r, opts(&l, Some("origin/main")), &mut RangeCache::new()).expect("load");
        let names: Vec<_> = b.branches.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            ["main", "dev", "upstream/main"],
            "upstream/main is its own line of work; upstream/dev is dev's upstream"
        );
    }

    #[test]
    fn explicit_base_and_missing_base() {
        let t = base_repo();
        commit_on(&t, "feat", "f.txt", "f\n", 2_000);
        let l = labeler();
        let b = load(&t.repo, opts(&l, Some("feat")), &mut RangeCache::new()).expect("load");
        assert_eq!(b.base.as_deref(), Some("feat"));
        assert_eq!(b.branches[0].ahead_behind, Some((0, 1)));
        assert_eq!(b.branches.len(), 1, "the base itself is not listed");

        let err = load(&t.repo, opts(&l, Some("nope")), &mut RangeCache::new())
            .expect_err("unknown base");
        assert!(err.to_string().contains("--base"), "{err}");
    }

    #[test]
    fn without_a_base_shows_the_tip_commit() {
        let t = TestRepo::new();
        t.write("a.txt", "a\n");
        t.commit_at("init", 1_000);
        let head = t
            .repo
            .head()
            .expect("head")
            .shorthand()
            .expect("n")
            .to_owned();
        t.repo
            .branch(
                "trunk",
                &t.repo.head().expect("h").peel_to_commit().expect("c"),
                true,
            )
            .expect("branch");
        t.repo.set_head("refs/heads/trunk").expect("head");
        t.repo
            .find_branch(&head, BranchType::Local)
            .expect("old")
            .delete()
            .expect("delete");
        let l = labeler();
        let b = load(&t.repo, opts(&l, None), &mut RangeCache::new()).expect("load");
        assert_eq!(b.base, None);
        let row = &b.branches[0];
        assert_eq!(row.ahead_behind, None);
        assert!(!row.merged());
        let changes = row.changes.as_ref().expect("changes");
        assert!(matches!(changes.target, Target::Commit(_)));
        assert_eq!(changes.short.len(), 7);
    }

    #[test]
    fn linked_worktrees_carry_their_dirty_state() {
        let t = base_repo();
        commit_on(&t, "agent", "x.txt", "x\n", 2_000);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-wt");
        let reference = t.repo.find_reference("refs/heads/agent").expect("ref");
        let mut add = git2::WorktreeAddOptions::new();
        add.reference(Some(&reference));
        t.repo
            .worktree("agent-wt", &path, Some(&add))
            .expect("worktree");
        std::fs::write(path.join("x.txt"), "x\ny\n").expect("dirty");
        // A worktree whose directory was removed is skipped; its branch stays.
        let gone = dir.path().join("gone");
        t.repo.worktree("gone", &gone, None).expect("worktree");
        std::fs::remove_dir_all(&gone).expect("rm");
        t.write("a.txt", "changed\n");

        let l = labeler();
        let b = load(&t.repo, opts(&l, None), &mut RangeCache::new()).expect("load");
        let names: Vec<_> = b.branches.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["main", "agent", "gone"]);
        assert!(find(&b, "gone").worktree.is_none());

        let main_dirty = b.branches[0]
            .worktree
            .as_ref()
            .and_then(|w| w.dirty.as_ref());
        let main_dirty = main_dirty.expect("main is dirty");
        assert_eq!(main_dirty.id, WORKTREE_ID);
        assert_eq!(main_dirty.root, None);

        let agent = find(&b, "agent");
        let wt = agent.worktree.as_ref().expect("worktree");
        assert!(!wt.current);
        assert_eq!(wt.name, "agent-wt");
        let dirty = wt.dirty.as_ref().expect("dirty");
        assert_eq!(dirty.summary, "Uncommitted changes in agent-wt");
        assert!(dirty.id.starts_with("worktree:"));
        let root = dirty.root.clone().expect("root");
        assert_eq!(root, canonical(&path));
        assert_eq!(
            agent.changes.as_ref().and_then(|c| c.root.clone()),
            Some(root)
        );
        assert!(agent.time() >= 2_000);

        // Opened from inside the linked worktree, that checkout is current.
        let inside = Repository::open(&path).expect("open");
        let b = load(&inside, opts(&l, None), &mut RangeCache::new()).expect("load");
        assert_eq!(b.branches[0].name, "agent");
        let wt = b.branches[0].worktree.as_ref().expect("wt");
        assert!(wt.current);
        assert_eq!(wt.dirty.as_ref().map(|d| d.id.as_str()), Some(WORKTREE_ID));
    }

    #[test]
    fn detached_and_unborn_checkouts() {
        let t = TestRepo::new();
        let l = labeler();
        let b = load(&t.repo, opts(&l, None), &mut RangeCache::new()).expect("unborn");
        assert_eq!(b.branches.len(), 1);
        assert!(b.branches[0].changes.is_none());
        assert_eq!(b.branches[0].time(), 0);

        t.write("a.txt", "a\n");
        let oid = t.commit_at("init", 1_000);
        t.repo.set_head_detached(oid).expect("detach");
        let b = load(&t.repo, opts(&l, None), &mut RangeCache::new()).expect("detached");
        let row = &b.branches[0];
        assert_eq!(row.branch, None);
        assert_eq!(
            Some(row.name.as_str()),
            t.path()
                .canonicalize()
                .expect("c")
                .file_name()
                .and_then(|n| n.to_str())
        );
    }
}
