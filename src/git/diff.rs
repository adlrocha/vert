//! Structured, line-level diffs for the detail view.

use git2::{Diff, Repository};

use super::{Context, FileStat, GitError, Target, delta_stat, make_diff};

/// The role of a line in a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context.
    Context,
    /// Added in the new version.
    Added,
    /// Removed from the old version.
    Removed,
}

/// A single diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Role of the line.
    pub kind: LineKind,
    /// Line number in the old file.
    pub old_no: Option<u32>,
    /// Line number in the new file.
    pub new_no: Option<u32>,
    /// Text without the trailing newline.
    pub content: String,
}

/// A contiguous block of changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ … @@` header, including any function context.
    pub header: String,
    /// Lines in order.
    pub lines: Vec<DiffLine>,
}

/// The diff of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// File statistics.
    pub stat: FileStat,
    /// Hunks (empty for binary files).
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The best line to jump to in the new file: the first added line, else
    /// the first line of the first hunk, else 1.
    #[must_use]
    pub fn first_changed_line(&self) -> u32 {
        let lines = || self.hunks.iter().flat_map(|h| &h.lines);
        lines()
            .find(|l| l.kind == LineKind::Added)
            .or_else(|| lines().find(|l| l.new_no.is_some()))
            .and_then(|l| l.new_no)
            .unwrap_or(1)
    }
}

/// Loads the full diff of `target`.
///
/// # Errors
/// Returns [`GitError`] if the diff cannot be computed.
pub fn load(repo: &Repository, target: Target) -> Result<Vec<FileDiff>, GitError> {
    read_files(&make_diff(repo, target)?)
}

/// Parses a unified diff (as `git diff` or GitHub produce it).
///
/// ```
/// let text = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n";
/// let files = vert::git::diff::parse(text).unwrap();
/// assert_eq!(files[0].stat.path, "a.txt");
/// assert_eq!((files[0].stat.added, files[0].stat.removed), (1, 1));
/// ```
///
/// # Errors
/// Returns [`GitError`] if the text is not a valid diff.
pub fn parse(text: &str) -> Result<Vec<FileDiff>, GitError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    read_files(&Diff::from_buffer(text.as_bytes()).ctx("parse diff")?)
}

fn read_files(diff: &Diff<'_>) -> Result<Vec<FileDiff>, GitError> {
    let mut files = Vec::with_capacity(diff.deltas().len());
    for index in 0..diff.deltas().len() {
        let Some((stat, patch)) = delta_stat(diff, index)? else {
            continue;
        };
        let hunks = match patch {
            Some(patch) if !stat.binary => read_hunks(&patch)?,
            _ => Vec::new(),
        };
        files.push(FileDiff { stat, hunks });
    }
    Ok(files)
}

fn read_hunks(patch: &git2::Patch<'_>) -> Result<Vec<Hunk>, GitError> {
    (0..patch.num_hunks())
        .map(|h| {
            let (hunk, count) = patch.hunk(h).ctx("read hunk")?;
            let header = String::from_utf8_lossy(hunk.header()).trim_end().to_owned();
            let lines = (0..count)
                .map(|l| patch.line_in_hunk(h, l).ctx("read diff line"))
                .filter_map(|line| line.map(|l| to_line(&l)).transpose())
                .collect::<Result<_, _>>()?;
            Ok(Hunk { header, lines })
        })
        .collect()
}

fn to_line(line: &git2::DiffLine<'_>) -> Option<DiffLine> {
    let kind = match line.origin() {
        '+' => LineKind::Added,
        '-' => LineKind::Removed,
        ' ' => LineKind::Context,
        _ => return None,
    };
    let content = String::from_utf8_lossy(line.content())
        .trim_end_matches(['\n', '\r'])
        .to_owned();
    Some(DiffLine {
        kind,
        old_no: line.old_lineno(),
        new_no: line.new_lineno(),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::TestRepo;

    #[test]
    fn commit_diff_has_hunks_and_line_numbers() {
        let t = TestRepo::new();
        t.write("a.txt", "one\ntwo\nthree\n");
        t.commit("init");
        t.write("a.txt", "one\n2\nthree\nfour\n");
        let oid = t.commit("edit");

        let files = load(&t.repo, Target::Commit(oid)).expect("diff");
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.stat.path, "a.txt");
        assert_eq!(file.hunks.len(), 1);
        assert!(file.hunks[0].header.starts_with("@@"));
        let kinds: Vec<_> = file.hunks[0]
            .lines
            .iter()
            .map(|l| (l.kind, l.content.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (LineKind::Context, "one"),
                (LineKind::Removed, "two"),
                (LineKind::Added, "2"),
                (LineKind::Context, "three"),
                (LineKind::Added, "four"),
            ]
        );
        assert_eq!(file.hunks[0].lines[1].old_no, Some(2));
        assert_eq!(file.hunks[0].lines[1].new_no, None);
        assert_eq!(file.first_changed_line(), 2);
    }

    #[test]
    fn root_commit_diffs_against_empty_tree() {
        let t = TestRepo::new();
        t.write("a.txt", "hello\n");
        let oid = t.commit("init");
        let files = load(&t.repo, Target::Commit(oid)).expect("diff");
        assert_eq!(files[0].hunks[0].lines[0].kind, LineKind::Added);
        assert_eq!(files[0].first_changed_line(), 1);
    }

    #[test]
    fn worktree_diff_and_missing_newline_marker() {
        let t = TestRepo::new();
        t.write("a.txt", "x\n");
        t.commit("init");
        t.write("a.txt", "x\ny");
        let files = load(&t.repo, Target::WorkingTree).expect("diff");
        let lines = &files[0].hunks[0].lines;
        assert!(lines.iter().all(|l| !l.content.contains("No newline")));
        assert_eq!(lines.last().map(|l| l.content.as_str()), Some("y"));
    }

    #[test]
    fn deletions_jump_to_context_and_binaries_have_no_hunks() {
        let t = TestRepo::new();
        t.write("a.txt", "keep\ndrop\n");
        std::fs::write(t.path().join("b.bin"), [0_u8, 1, 2]).expect("bin");
        t.commit("init");
        t.write("a.txt", "keep\n");
        std::fs::write(t.path().join("b.bin"), [0_u8, 3, 4]).expect("bin");
        let files = load(&t.repo, Target::WorkingTree).expect("diff");
        let text = files.iter().find(|f| f.stat.path == "a.txt").expect("a");
        assert_eq!(text.first_changed_line(), 1);
        let bin = files.iter().find(|f| f.stat.path == "b.bin").expect("b");
        assert!(bin.stat.binary);
        assert!(bin.hunks.is_empty());
        assert_eq!(bin.first_changed_line(), 1);
    }

    #[test]
    fn parses_patch_text() {
        assert!(parse("  \n").expect("empty").is_empty());
        let text = "diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..92d5444\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+fresh\ndiff --git a/old.txt b/new.txt\nsimilarity index 76%\nrename from old.txt\nrename to new.txt\nindex 302c88e..ecaf925 100644\n--- a/old.txt\n+++ b/new.txt\n@@ -1,4 +1,4 @@\n keep\n-gone\n+here\n more\n lines\n";
        let files = parse(text).expect("parse");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].stat.status, crate::git::FileStatus::Added);
        assert_eq!(files[1].stat.path, "new.txt");
        assert_eq!(files[1].stat.old_path.as_deref(), Some("old.txt"));
        assert_eq!((files[1].stat.added, files[1].stat.removed), (1, 1));
        assert_eq!(files[1].hunks[0].lines.len(), 5);
        assert!(parse("diff --git a/x b/x\n@@ nonsense").is_err());
    }

    #[test]
    fn ranges_and_pull_requests() {
        let t = TestRepo::new();
        t.write("a.txt", "a\n");
        let base = t.commit("init");
        t.write("b.txt", "b\n");
        t.commit("one");
        t.write("c.txt", "c\n");
        let head = t.commit("two");
        let range = load(
            &t.repo,
            Target::Range {
                base: Some(base),
                head,
            },
        )
        .expect("range");
        assert_eq!(range.len(), 2);
        let all = load(&t.repo, Target::Range { base: None, head }).expect("all");
        assert_eq!(all.len(), 3);
        let err = load(&t.repo, Target::PullRequest(1)).expect_err("not local");
        assert!(matches!(err, GitError::NotLocal));
    }
}
