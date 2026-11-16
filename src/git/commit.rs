//! Committing a fix: the edits an agent made in a checkout, and only those,
//! become one commit on the checkout's branch.
//!
//! Before the agent runs, [`snapshot`] records the checkout as it is on
//! disk (tracked and untracked files, minus ignored ones) as a tree, using
//! an in-memory index so the checkout's own index is left alone. Afterwards
//! [`commit_edits`] takes a second snapshot: the difference between the two
//! is what changed while the agent worked. That difference is applied to
//! `HEAD`'s tree and committed, so uncommitted work the checkout already
//! had stays uncommitted. The same difference is then applied to the index,
//! which keeps `git status` true and leaves what was staged staged.
//!
//! When the edits can't be committed (they overlap uncommitted work, `HEAD`
//! is detached, a rebase is in progress, commits must be signed, …) they
//! stay in the checkout as uncommitted changes, and [`Outcome::Kept`] says
//! why. Nothing is ever pushed.

use std::path::{Path, PathBuf};

use git2::{ApplyLocation, DiffOptions, IndexAddOption, Oid, Repository, RepositoryState};

use super::{Context, GitError, short_id};

/// The state of a checkout's files at one moment, stored as a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot(Oid);

/// What became of the edits made since a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// They are commit `id` (short form) on `branch`.
    Committed {
        /// The new commit, abbreviated.
        id: String,
        /// The branch it was added to.
        branch: String,
        /// The index couldn't be updated afterwards, so `git status` may
        /// show the fix as undone in the index for the files it touched.
        stale_index: bool,
    },
    /// Nothing changed.
    Unchanged,
    /// They stay uncommitted, for the reason given (for the user).
    Kept(String),
}

/// Records the files of the checkout at `dir` as a tree, without touching
/// its index.
///
/// # Errors
/// Returns an error when `dir` isn't a checkout or its files can't be read.
pub fn snapshot(dir: &Path) -> Result<Snapshot, GitError> {
    // A handle of its own: the index changed here in memory must never be
    // the one another reader of the repository sees, or written to disk.
    let repo = super::open(dir)?;
    let mut index = repo.index().ctx("read the index")?;
    index
        .add_all(["*"], IndexAddOption::DEFAULT, None)
        .ctx("record the checkout's files")?;
    index
        .update_all(["*"], None)
        .ctx("record the checkout's removed files")?;
    index
        .write_tree()
        .map(Snapshot)
        .ctx("store the checkout's files")
}

/// Commits what changed in the checkout at `dir` since `before`, on its
/// branch, with `message`. `before` must come from [`snapshot`] of the same
/// checkout.
///
/// # Errors
/// Returns an error when git fails unexpectedly; the edits are then still
/// in the checkout, uncommitted. Expected refusals are [`Outcome::Kept`].
pub fn commit_edits(dir: &Path, before: Snapshot, message: &str) -> Result<Outcome, GitError> {
    let after = snapshot(dir)?;
    if after == before {
        return Ok(Outcome::Unchanged);
    }
    let repo = super::open(dir)?;
    if let Some(why) = refusal(&repo)? {
        return Ok(Outcome::Kept(why));
    }
    let head = repo.head().ctx("read HEAD")?;
    let branch = head.shorthand().unwrap_or("HEAD").to_owned();
    let parent = head.peel_to_commit().ctx("read the HEAD commit")?;
    let old = repo.find_tree(before.0).ctx("read the first snapshot")?;
    let new = repo.find_tree(after.0).ctx("read the second snapshot")?;
    // With binary contents, so a changed binary file applies like the rest.
    let mut opts = DiffOptions::new();
    opts.show_binary(true);
    let diff = repo
        .diff_tree_to_tree(Some(&old), Some(&new), Some(&mut opts))
        .ctx("compare the snapshots")?;
    let paths = touched(&diff);
    let base = parent.tree().ctx("read the HEAD tree")?;
    let Ok(mut applied) = repo.apply_to_tree(&base, &diff, None) else {
        return Ok(Outcome::Kept(
            "the edits don't apply on top of HEAD; they likely overlap uncommitted changes the checkout already had".to_owned(),
        ));
    };
    let tree_id = applied.write_tree_to(&repo).ctx("store the fix")?;
    if tree_id == base.id() {
        return Ok(Outcome::Unchanged);
    }
    let tree = repo.find_tree(tree_id).ctx("read the fix")?;
    let Ok(sig) = repo.signature() else {
        return Ok(Outcome::Kept(
            "git has no user.name and user.email to commit as".to_owned(),
        ));
    };
    let Ok(name) = head.name() else {
        return Ok(Outcome::Kept(format!("{branch} has a name git can't read")));
    };
    let id = repo
        .commit(None, &sig, &sig, message, &tree, &[&parent])
        .ctx("store the fix's commit")?;
    // Moved only if it still points at `parent`, so a commit made meanwhile
    // is never dropped from the branch.
    let log = format!("commit: {}", message.lines().next().unwrap_or_default());
    if repo
        .reference_matching(name, id, true, parent.id(), &log)
        .is_err()
    {
        return Ok(Outcome::Kept(format!(
            "{branch} moved while the agent worked"
        )));
    }
    Ok(Outcome::Committed {
        id: short_id(id),
        branch,
        stale_index: !refresh_index(&repo, &diff, id, &paths),
    })
}

/// Brings the index up to date with commit `id` for the `paths` the fix
/// touched, keeping what was staged there staged. Returns whether it could.
fn refresh_index(repo: &Repository, diff: &git2::Diff<'_>, id: Oid, paths: &[PathBuf]) -> bool {
    // The fix applied to the index as it is: other staged hunks stay.
    if repo.apply(diff, ApplyLocation::Index, None).is_ok() {
        return true;
    }
    // It overlaps staged hunks (or the agent staged its own edits): those
    // files are reset to the commit instead.
    repo.find_object(id, None)
        .and_then(|commit| repo.reset_default(Some(&commit), paths.iter()))
        .is_ok()
}

/// Why edits in `repo` can't become a commit on its branch, if they can't.
fn refusal(repo: &Repository) -> Result<Option<String>, GitError> {
    if repo.state() != RepositoryState::Clean {
        return Ok(Some(
            "a merge, rebase or similar is in progress there".to_owned(),
        ));
    }
    if repo.head().is_err() {
        return Ok(Some("the branch has no commit yet".to_owned()));
    }
    if repo.head_detached().ctx("read HEAD")? {
        return Ok(Some("HEAD is detached, so there is no branch".to_owned()));
    }
    let signed = repo
        .config()
        .ctx("read git's config")?
        .get_bool("commit.gpgsign")
        .unwrap_or(false);
    Ok(signed.then(|| "commits must be signed (commit.gpgSign) and vert can't sign".to_owned()))
}

/// Every path `diff` adds, changes or removes.
fn touched(diff: &git2::Diff<'_>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = diff
        .deltas()
        .flat_map(|d| [d.old_file().path(), d.new_file().path()])
        .flatten()
        .map(Path::to_path_buf)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use git2::{Status, StatusOptions};

    use super::super::testutil::TestRepo;
    use super::*;

    /// A repository with a commit and an identity of its own, whatever the
    /// machine's git config says.
    fn repo() -> TestRepo {
        let t = TestRepo::new();
        let mut config = t.repo.config().expect("config");
        config.set_str("user.name", "Reviewer").expect("name");
        config
            .set_str("user.email", "reviewer@example.com")
            .expect("email");
        t.write("a.txt", "one\ntwo\nthree\n");
        t.write("b.txt", "bee\n");
        t.commit("init");
        t
    }

    fn head_file(t: &TestRepo, path: &str) -> Option<String> {
        let tree = t.repo.head().ok()?.peel_to_tree().ok()?;
        let blob = tree
            .get_path(Path::new(path))
            .ok()?
            .to_object(&t.repo)
            .ok()?;
        Some(String::from_utf8_lossy(blob.as_blob()?.content()).into_owned())
    }

    fn status(t: &TestRepo) -> Vec<(String, Status)> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true);
        let mut out: Vec<_> = t
            .repo
            .statuses(Some(&mut opts))
            .expect("status")
            .iter()
            .map(|s| (s.path().unwrap_or_default().to_owned(), s.status()))
            .collect();
        out.sort();
        out
    }

    fn committed(outcome: &Outcome) -> &str {
        match outcome {
            Outcome::Committed {
                id,
                stale_index: false,
                ..
            } => id,
            other => panic!("not committed: {other:?}"),
        }
    }

    #[test]
    fn commits_only_the_edits_made_after_the_snapshot() {
        let t = repo();
        // Work the checkout already had: a change, a staged change and an
        // untracked file. None of it may be committed.
        t.write("b.txt", "bee, mine\n");
        t.write("mine.txt", "draft\n");
        t.write("staged.txt", "staged\n");
        let mut index = t.repo.index().expect("index");
        index.add_path(Path::new("staged.txt")).expect("stage");
        index.write().expect("write");
        let before = snapshot(t.path()).expect("snapshot");

        t.write("a.txt", "one\nTWO\nthree\n");
        t.write("new.txt", "added by the agent\n");
        let outcome = commit_edits(t.path(), before, "fix: shout\n").expect("commit");
        let id = committed(&outcome);

        let head = t
            .repo
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("commit");
        assert!(head.id().to_string().starts_with(id));
        assert_eq!(head.message().ok(), Some("fix: shout\n"));
        assert_eq!(head.parent_count(), 1);
        assert_eq!(head_file(&t, "a.txt").as_deref(), Some("one\nTWO\nthree\n"));
        assert_eq!(
            head_file(&t, "new.txt").as_deref(),
            Some("added by the agent\n")
        );
        assert_eq!(head_file(&t, "b.txt").as_deref(), Some("bee\n"));
        assert_eq!(head_file(&t, "mine.txt"), None);
        assert_eq!(head_file(&t, "staged.txt"), None);
        // The fix is clean in the index; the earlier work is as it was.
        assert_eq!(
            status(&t),
            [
                ("b.txt".to_owned(), Status::WT_MODIFIED),
                ("mine.txt".to_owned(), Status::WT_NEW),
                ("staged.txt".to_owned(), Status::INDEX_NEW),
            ]
        );
    }

    #[test]
    fn what_was_staged_in_a_fixed_file_stays_staged() {
        let t = repo();
        t.write("a.txt", "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n");
        t.commit("longer");
        t.write("a.txt", "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n");
        let mut index = t.repo.index().expect("index");
        index.add_path(Path::new("a.txt")).expect("stage");
        index.write().expect("write");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n");
        committed(&commit_edits(t.path(), before, "fix\n").expect("commit"));
        assert_eq!(
            head_file(&t, "a.txt").as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n")
        );
        assert_eq!(status(&t), [("a.txt".to_owned(), Status::INDEX_MODIFIED)]);
    }

    #[test]
    fn edits_the_agent_staged_are_committed_and_clean() {
        let t = repo();
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "one\nTWO\nthree\n");
        let mut index = t.repo.index().expect("index");
        index.add_path(Path::new("a.txt")).expect("stage");
        index.write().expect("write");
        committed(&commit_edits(t.path(), before, "fix\n").expect("commit"));
        assert_eq!(head_file(&t, "a.txt").as_deref(), Some("one\nTWO\nthree\n"));
        assert_eq!(status(&t), []);
    }

    #[test]
    fn a_locked_index_still_commits_and_says_so() {
        let t = repo();
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "one\nTWO\nthree\n");
        std::fs::write(t.repo.path().join("index.lock"), "").expect("lock");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(
                &outcome,
                Outcome::Committed {
                    stale_index: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(head_file(&t, "a.txt").as_deref(), Some("one\nTWO\nthree\n"));
    }

    #[test]
    fn commits_binary_files() {
        let t = repo();
        std::fs::write(t.path().join("logo.bin"), b"\0\x01\x02old").expect("write");
        t.commit("binary");
        let before = snapshot(t.path()).expect("snapshot");
        std::fs::write(t.path().join("logo.bin"), b"\0\x01\x02new").expect("write");
        std::fs::write(t.path().join("added.bin"), b"\0\xff").expect("write");
        committed(&commit_edits(t.path(), before, "fix: bytes\n").expect("commit"));
        let blob = |path: &str| {
            let tree = t.repo.head().expect("head").peel_to_tree().expect("tree");
            let entry = tree.get_path(Path::new(path)).expect("entry");
            let object = entry.to_object(&t.repo).expect("object");
            object.as_blob().expect("blob").content().to_vec()
        };
        assert_eq!(blob("logo.bin"), b"\0\x01\x02new");
        assert_eq!(blob("added.bin"), b"\0\xff");
        assert_eq!(status(&t), []);
    }

    #[test]
    fn commits_removed_files() {
        let t = repo();
        let before = snapshot(t.path()).expect("snapshot");
        t.remove("b.txt");
        committed(&commit_edits(t.path(), before, "fix: drop b\n").expect("commit"));
        assert_eq!(head_file(&t, "b.txt"), None);
        assert_eq!(status(&t), []);
    }

    #[test]
    fn nothing_changed_is_no_commit() {
        let t = repo();
        t.write("mine.txt", "draft\n");
        let tip = t.repo.head().expect("head").target();
        let before = snapshot(t.path()).expect("snapshot");
        assert_eq!(
            commit_edits(t.path(), before, "fix\n").expect("commit"),
            Outcome::Unchanged
        );
        assert_eq!(t.repo.head().expect("head").target(), tip);
    }

    #[test]
    fn edits_on_top_of_uncommitted_lines_stay_uncommitted() {
        let t = repo();
        t.write("a.txt", "one\nmine\nthree\n");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "one\nmine, fixed\nthree\n");
        let tip = t.repo.head().expect("head").target();
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("overlap")),
            "{outcome:?}"
        );
        assert_eq!(t.repo.head().expect("head").target(), tip);
        assert_eq!(
            std::fs::read_to_string(t.path().join("a.txt")).expect("read"),
            "one\nmine, fixed\nthree\n"
        );
    }

    #[test]
    fn edits_beside_uncommitted_lines_of_the_same_file_are_committed_alone() {
        let t = repo();
        t.write("a.txt", "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n");
        t.commit("longer");
        t.write("a.txt", "mine\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "mine\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n");
        committed(&commit_edits(t.path(), before, "fix\n").expect("commit"));
        assert_eq!(
            head_file(&t, "a.txt").as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\nEIGHT\n")
        );
        assert_eq!(status(&t), [("a.txt".to_owned(), Status::WT_MODIFIED)]);
    }

    #[test]
    fn a_detached_head_keeps_the_edits() {
        let t = repo();
        let tip = t.repo.head().expect("head").target().expect("oid");
        t.repo.set_head_detached(tip).expect("detach");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "changed\n");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("detached")),
            "{outcome:?}"
        );
    }

    #[test]
    fn commits_as_the_person_and_needs_an_identity() {
        let t = repo();
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "changed\n");
        committed(&commit_edits(t.path(), before, "fix\n").expect("commit"));
        let head = t
            .repo
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("commit");
        assert_eq!(head.author().name().ok(), Some("Reviewer"));
        assert_eq!(head.committer().email().ok(), Some("reviewer@example.com"));

        t.repo
            .config()
            .expect("config")
            .set_str("user.name", "")
            .expect("unset");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "changed again\n");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("user.name")),
            "{outcome:?}"
        );
    }

    #[test]
    fn signing_keeps_the_edits() {
        let t = repo();
        t.repo
            .config()
            .expect("config")
            .set_bool("commit.gpgsign", true)
            .expect("set");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "changed\n");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("signed")),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_operation_in_progress_keeps_the_edits() {
        let t = repo();
        let tip = t.repo.head().expect("head").target().expect("oid");
        std::fs::write(t.repo.path().join("MERGE_HEAD"), format!("{tip}\n")).expect("merge");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "changed\n");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("in progress")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_branch_without_commits_keeps_the_edits() {
        let t = TestRepo::new();
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "new\n");
        let outcome = commit_edits(t.path(), before, "fix\n").expect("commit");
        assert!(
            matches!(&outcome, Outcome::Kept(why) if why.contains("no commit")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_commit_made_while_the_agent_works_becomes_the_parent() {
        let t = repo();
        t.write("b.txt", "bee, mine\n");
        let before = snapshot(t.path()).expect("snapshot");
        t.write("a.txt", "one\nTWO\nthree\n");
        // The person commits their own work meanwhile.
        let mut index = t.repo.index().expect("index");
        index.add_path(Path::new("b.txt")).expect("stage");
        index.write().expect("write");
        let tree = t
            .repo
            .find_tree(index.write_tree().expect("tree"))
            .expect("tree");
        let sig = t.repo.signature().expect("sig");
        let parent = t
            .repo
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("commit");
        let mine = t
            .repo
            .commit(Some("HEAD"), &sig, &sig, "mine\n", &tree, &[&parent])
            .expect("commit");

        committed(&commit_edits(t.path(), before, "fix\n").expect("commit"));
        let head = t
            .repo
            .head()
            .expect("head")
            .peel_to_commit()
            .expect("commit");
        assert_eq!(head.parent_id(0).expect("parent"), mine);
        assert_eq!(head_file(&t, "b.txt").as_deref(), Some("bee, mine\n"));
        assert_eq!(head_file(&t, "a.txt").as_deref(), Some("one\nTWO\nthree\n"));
        assert_eq!(status(&t), []);
    }

    #[test]
    fn snapshots_leave_the_index_alone() {
        let t = repo();
        t.write("mine.txt", "draft\n");
        let before = status(&t);
        snapshot(t.path()).expect("snapshot");
        assert_eq!(status(&t), before);
    }

    #[test]
    fn snapshots_need_a_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(snapshot(&dir.path().join("nowhere")).is_err());
    }
}
