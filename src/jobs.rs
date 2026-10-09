//! Background jobs: the I/O behind each [`Effect`], turned into a [`Msg`].
//!
//! Each job is a plain blocking function so it can run on a worker thread in
//! the binary and be tested directly here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::app::{Effect, Msg};
use crate::labels::Labeler;
use crate::{age, editor, git, search};

/// Everything a job needs, cheap to clone onto a worker thread.
#[derive(Debug, Clone)]
pub struct Context {
    /// Repository working directory.
    pub workdir: PathBuf,
    /// Change labelling thresholds.
    pub labeler: Labeler,
    /// Maximum commits to load.
    pub commit_limit: usize,
    /// Maximum content-search matches.
    pub search_limit: usize,
    /// Largest file the viewer opens, in bytes.
    pub max_file_bytes: u64,
    /// Generation of the newest content search; older searches cancel.
    pub search_generation: Arc<AtomicU64>,
    /// Commits already diffed, shared across reloads.
    pub commit_cache: Arc<Mutex<git::feed::CommitCache>>,
}

/// Formats an error with its chain of causes.
#[must_use]
pub fn describe(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// Whether `effect` is background I/O handled by [`run`] (as opposed to
/// quitting or launching the editor, which need the terminal).
#[must_use]
pub const fn is_background(effect: &Effect) -> bool {
    !matches!(effect, Effect::Quit | Effect::Edit { .. })
}

/// Records a new content search as the latest; call before spawning [`run`].
pub fn begin(effect: &Effect, ctx: &Context) {
    if let Effect::SearchText { generation, .. } = effect {
        ctx.search_generation.store(*generation, Ordering::Release);
    }
}

/// Performs a background effect, returning the message with its outcome.
///
/// Returns `None` for effects that are not background work (see [`is_background`]).
#[must_use]
pub fn run(effect: Effect, ctx: &Context) -> Option<Msg> {
    let open = || git::open(&ctx.workdir);
    Some(match effect {
        Effect::Quit | Effect::Edit { .. } => return None,
        Effect::LoadCommits => {
            // Loads are serialised by the app, so this lock is uncontended.
            let mut cache = ctx
                .commit_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Msg::Commits(
                open()
                    .and_then(|repo| {
                        git::feed::load_commits_cached(
                            &repo,
                            ctx.commit_limit,
                            &ctx.labeler,
                            &mut cache,
                        )
                    })
                    .map_err(|e| describe(&e)),
            )
        }
        Effect::LoadWorktree => Msg::Worktree(
            open()
                .and_then(|repo| git::feed::load_worktree(&repo, &ctx.labeler, age::now()))
                .map_err(|e| describe(&e)),
        ),
        Effect::LoadFiles => Msg::Files(search::list_files(&ctx.workdir)),
        Effect::SearchText { query, generation } => {
            let latest = Arc::clone(&ctx.search_generation);
            let cancelled = move || latest.load(Ordering::Acquire) != generation;
            let result = search::search_text(&ctx.workdir, &query, ctx.search_limit, &cancelled)
                .map_err(|e| describe(&e));
            Msg::Text { generation, result }
        }
        Effect::LoadDiff { entry, refresh } => {
            let result = open()
                .and_then(|repo| git::diff::load(&repo, entry.target))
                .map_err(|e| describe(&e));
            Msg::Diff {
                entry,
                result,
                refresh,
            }
        }
        Effect::OpenFile { path, line, marks } => {
            let content = editor::read_text(&ctx.workdir.join(&path), ctx.max_file_bytes);
            Msg::File {
                path,
                content,
                line,
                marks,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::git::testutil::TestRepo;

    fn ctx(t: &TestRepo) -> Context {
        Context {
            workdir: t.path().to_path_buf(),
            labeler: Labeler::new(5, 20, 100, 2, 1).expect("labeler"),
            commit_limit: 10,
            search_limit: 100,
            max_file_bytes: 1 << 20,
            search_generation: Arc::new(AtomicU64::new(0)),
            commit_cache: Arc::default(),
        }
    }

    fn repo() -> TestRepo {
        let t = TestRepo::new();
        t.write("src/main.rs", "fn main() {}\n");
        t.commit("init");
        t.write("src/main.rs", "fn main() { todo!() }\n");
        t
    }

    #[test]
    fn classifies_effects() {
        assert!(!is_background(&Effect::Quit));
        assert!(!is_background(&Effect::Edit {
            path: String::new(),
            line: 1
        }));
        assert!(is_background(&Effect::LoadFiles));
        let t = repo();
        assert!(run(Effect::Quit, &ctx(&t)).is_none());
    }

    #[test]
    fn loads_commits_worktree_and_files() {
        let t = repo();
        let c = ctx(&t);
        let Some(Msg::Commits(Ok(commits))) = run(Effect::LoadCommits, &c) else {
            panic!("commits")
        };
        assert_eq!(commits.len(), 1);
        let Some(Msg::Worktree(Ok(Some(wt)))) = run(Effect::LoadWorktree, &c) else {
            panic!("worktree")
        };
        assert_eq!(wt.files.len(), 1);
        let Some(Msg::Files(files)) = run(Effect::LoadFiles, &c) else {
            panic!("files")
        };
        assert_eq!(files, ["src/main.rs"]);

        let Some(Msg::Diff {
            result: Ok(diff),
            refresh: true,
            ..
        }) = run(
            Effect::LoadDiff {
                entry: Box::new(wt),
                refresh: true,
            },
            &c,
        )
        else {
            panic!("diff")
        };
        assert_eq!(diff[0].stat.path, "src/main.rs");
    }

    #[test]
    fn reports_repository_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut c = ctx(&repo());
        c.workdir = dir.path().join("missing");
        let Some(Msg::Commits(Err(e))) = run(Effect::LoadCommits, &c) else {
            panic!("error expected")
        };
        assert!(e.contains("not inside a git repository"), "{e}");
        assert!(matches!(
            run(Effect::LoadWorktree, &c),
            Some(Msg::Worktree(Err(_)))
        ));
    }

    #[test]
    fn searches_and_cancels_stale_generations() {
        let t = repo();
        let c = ctx(&t);
        let effect = Effect::SearchText {
            query: "todo".to_owned(),
            generation: 7,
        };
        begin(&effect, &c);
        let Some(Msg::Text {
            generation: 7,
            result: Ok(found),
        }) = run(effect, &c)
        else {
            panic!("search")
        };
        assert_eq!(found.matches.len(), 1);

        // A newer search has started: this one reports itself cancelled.
        c.search_generation.store(9, Ordering::Release);
        let stale = Effect::SearchText {
            query: "todo".to_owned(),
            generation: 8,
        };
        let Some(Msg::Text {
            result: Ok(res), ..
        }) = run(stale, &c)
        else {
            panic!("search")
        };
        assert!(res.cancelled);
        begin(&Effect::LoadFiles, &c);
        assert_eq!(c.search_generation.load(Ordering::Acquire), 9);
    }

    #[test]
    fn opens_files() {
        let t = repo();
        let Some(Msg::File {
            content, line: 3, ..
        }) = run(
            Effect::OpenFile {
                path: "src/main.rs".to_owned(),
                line: 3,
                marks: BTreeSet::new(),
            },
            &ctx(&t),
        )
        else {
            panic!("file")
        };
        assert!(content.expect("text").contains("todo"));
    }

    #[test]
    fn describes_error_chains() {
        let Err(err) = git::open(std::path::Path::new("/definitely/not/here")) else {
            panic!("expected an error")
        };
        let text = describe(&err);
        assert!(text.contains(": "), "includes the cause: {text}");
    }
}
