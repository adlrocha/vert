//! Background jobs: the I/O behind each [`Effect`], turned into a [`Msg`].
//!
//! Each job is a plain blocking function so it can run on a worker thread in
//! the binary and be tested directly here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::app::{Effect, Msg};
use crate::git::Target;
use crate::git::feed::Entry;
use crate::github::{self, Client, Endpoint, GhError, PullsError, Slug};
use crate::labels::Labeler;
use crate::{age, editor, git, search};

/// The GitHub CLI, asked for a token when none is in the environment.
pub const GH_PROGRAM: &str = "gh";

/// How to reach GitHub.
#[derive(Debug, Clone)]
pub struct GitHub {
    /// API location.
    pub endpoint: Endpoint,
    /// Request timeout.
    pub timeout: Duration,
    /// Maximum pull requests to list.
    pub max_prs: u16,
}

/// Where a GitHub token comes from.
#[derive(Debug, Clone)]
pub struct TokenSource {
    /// Reads an environment variable.
    pub env: fn(&str) -> Option<String>,
    /// The `gh` executable to fall back to.
    pub gh: String,
}

impl Default for TokenSource {
    fn default() -> Self {
        Self {
            env: |k| std::env::var(k).ok(),
            gh: GH_PROGRAM.to_owned(),
        }
    }
}

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
    /// Branch to compare branches against; detected when `None`.
    pub base: Option<String>,
    /// Maximum branches without a worktree to load.
    pub branch_limit: usize,
    /// Branch ranges already diffed, shared across reloads.
    pub branch_cache: Arc<Mutex<git::branches::RangeCache>>,
    /// GitHub access, or `None` when network access is off.
    pub github: Option<GitHub>,
    /// Where tokens come from.
    pub tokens: TokenSource,
    /// The last token that worked, so `gh` is not asked on every refresh.
    pub token: Arc<Mutex<Option<github::Token>>>,
}

impl Context {
    /// The GitHub client and repository for `origin`.
    fn github(&self) -> Result<(Client, Slug, &GitHub), PullsError> {
        let gh = self.github.as_ref().ok_or_else(|| {
            PullsError::Unsupported("network access is off (--no-network)".to_owned())
        })?;
        let host = gh.endpoint.web_host();
        let repo = git::open(&self.workdir).map_err(|e| PullsError::Failed(describe(&e)))?;
        let remote = repo.find_remote("origin").map_err(|_| {
            PullsError::Unsupported("this repository has no origin remote".to_owned())
        })?;
        let slug = remote
            .url()
            .ok()
            .and_then(|url| github::parse_remote(url, &host))
            .ok_or_else(|| PullsError::Unsupported(format!("origin is not on {host}")))?;
        let mut cached = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        let token = if let Some(token) = cached.clone() {
            token
        } else {
            let token =
                github::resolve_token(self.tokens.env, || github::gh_token(&self.tokens.gh, &host))
                    .ok_or(PullsError::NoToken)?;
            *cached = Some(token.clone());
            token
        };
        Ok((
            Client::new(gh.endpoint.clone(), token, gh.timeout),
            slug,
            gh,
        ))
    }

    /// Forgets a token GitHub rejected, so the next request looks again.
    fn rejected(&self, e: &GhError) {
        if *e == GhError::Unauthorized {
            *self.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// The directory `root` refers to: a linked worktree, or vert's own.
    fn dir<'a>(&'a self, root: Option<&'a Path>) -> &'a Path {
        root.unwrap_or(&self.workdir)
    }
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
        Effect::LoadDiff { mut entry, refresh } => {
            let result = load_diff(&mut entry, ctx);
            Msg::Diff {
                entry,
                result,
                refresh,
            }
        }
        Effect::LoadBranches => {
            let mut cache = ctx
                .branch_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Msg::Branches(
                open()
                    .and_then(|repo| {
                        git::branches::load(
                            &repo,
                            git::branches::Options {
                                base: ctx.base.as_deref(),
                                limit: ctx.branch_limit,
                                labeler: &ctx.labeler,
                                now: age::now(),
                            },
                            &mut cache,
                        )
                    })
                    .map_err(|e| describe(&e)),
            )
        }
        Effect::LoadPulls => Msg::Pulls(ctx.github().and_then(|(client, slug, gh)| {
            client
                .open_pulls(&slug, gh.max_prs)
                .map(|pulls| (slug.to_string(), pulls))
                .map_err(|e| {
                    ctx.rejected(&e);
                    e.into()
                })
        })),
        Effect::OpenFile {
            path,
            line,
            marks,
            root,
        } => {
            let content =
                editor::read_text(&ctx.dir(root.as_deref()).join(&path), ctx.max_file_bytes);
            Msg::File {
                path,
                content,
                line,
                marks,
                root,
            }
        }
    })
}

/// Loads the diff of `entry`: from git, or from GitHub for a pull request,
/// whose entry then gains the changed files.
fn load_diff(entry: &mut Entry, ctx: &Context) -> Result<Vec<git::diff::FileDiff>, String> {
    if let Target::PullRequest(number) = entry.target {
        let (client, slug, _) = ctx.github().map_err(|e| match e {
            PullsError::NoToken => {
                "no GitHub token: set GH_TOKEN or run `gh auth login`".to_owned()
            }
            PullsError::Unsupported(why) | PullsError::Failed(why) => why,
        })?;
        let text = client.pull_diff(&slug, number).map_err(|e| {
            ctx.rejected(&e);
            e.to_string()
        })?;
        let files = git::diff::parse(&text).map_err(|e| describe(&e))?;
        entry.relabel(files.iter().map(|f| f.stat.clone()).collect(), &ctx.labeler);
        return Ok(files);
    }
    git::open(ctx.dir(entry.root.as_deref()))
        .and_then(|repo| git::diff::load(&repo, entry.target))
        .map_err(|e| describe(&e))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::git::testutil::TestRepo;
    use crate::github::testserver::serve;

    fn ctx(t: &TestRepo) -> Context {
        Context {
            workdir: t.path().to_path_buf(),
            labeler: Labeler::new(5, 20, 100, 2, 1).expect("labeler"),
            commit_limit: 10,
            search_limit: 100,
            max_file_bytes: 1 << 20,
            search_generation: Arc::new(AtomicU64::new(0)),
            commit_cache: Arc::default(),
            base: None,
            branch_limit: 10,
            branch_cache: Arc::default(),
            github: None,
            tokens: TokenSource {
                env: |_| None,
                gh: "false".to_owned(),
            },
            token: Arc::default(),
        }
    }

    /// A context whose `origin` is `o/r` on a fake GitHub at `url`.
    fn online(t: &TestRepo, url: &str, token: Option<&str>) -> Context {
        t.repo
            .remote("origin", &format!("{url}/o/r.git"))
            .expect("remote");
        let mut c = ctx(t);
        c.github = Some(GitHub {
            endpoint: Endpoint::new(url),
            timeout: Duration::from_secs(5),
            max_prs: 10,
        });
        *c.token.lock().expect("lock") = token.map(github::Token::new);
        c
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
            line: 1,
            root: None,
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
                root: None,
            },
            &ctx(&t),
        )
        else {
            panic!("file")
        };
        assert!(content.expect("text").contains("todo"));
    }

    #[test]
    fn default_tokens_read_the_environment_and_gh() {
        let source = TokenSource::default();
        assert_eq!(source.gh, GH_PROGRAM);
        assert_eq!((source.env)("PATH"), std::env::var("PATH").ok());
    }

    #[test]
    fn loads_branches() {
        let t = repo();
        let Some(Msg::Branches(Ok(list))) = run(Effect::LoadBranches, &ctx(&t)) else {
            panic!("branches")
        };
        assert_eq!(list.branches.len(), 1);
        let mut c = ctx(&t);
        c.base = Some("missing".to_owned());
        assert!(matches!(
            run(Effect::LoadBranches, &c),
            Some(Msg::Branches(Err(_)))
        ));
    }

    #[test]
    fn diffs_and_files_of_linked_worktrees() {
        let t = repo();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wt");
        t.repo.worktree("wt", &path, None).expect("worktree");
        std::fs::write(path.join("extra.txt"), "x\n").expect("write");
        let c = ctx(&t);
        let Some(Msg::Branches(Ok(list))) = run(Effect::LoadBranches, &c) else {
            panic!("branches")
        };
        let dirty = list
            .branches
            .iter()
            .filter_map(|b| b.worktree.as_ref()?.dirty.clone())
            .find(|e| e.root.is_some())
            .expect("linked worktree is dirty");
        let root = dirty.root.clone();
        let Some(Msg::Diff {
            result: Ok(files), ..
        }) = run(
            Effect::LoadDiff {
                entry: Box::new(dirty),
                refresh: false,
            },
            &c,
        )
        else {
            panic!("diff")
        };
        assert_eq!(files[0].stat.path, "extra.txt");
        let Some(Msg::File {
            content: Ok(text),
            root: back,
            ..
        }) = run(
            Effect::OpenFile {
                path: "extra.txt".to_owned(),
                line: 1,
                marks: BTreeSet::new(),
                root: root.clone(),
            },
            &c,
        )
        else {
            panic!("file")
        };
        assert_eq!(text, "x\n");
        assert_eq!(back, root);
    }

    #[test]
    fn pull_requests_need_network_origin_and_token() {
        let t = repo();
        let Some(Msg::Pulls(Err(PullsError::Unsupported(off)))) = run(Effect::LoadPulls, &ctx(&t))
        else {
            panic!("off")
        };
        assert!(off.contains("--no-network"), "{off}");

        let c = online(&t, "http://127.0.0.1:9", None);
        assert_eq!(
            run(Effect::LoadPulls, &c).map(|m| matches!(m, Msg::Pulls(Err(PullsError::NoToken)))),
            Some(true)
        );
        t.repo
            .remote_set_url("origin", "https://gitlab.com/o/r")
            .expect("url");
        let Some(Msg::Pulls(Err(PullsError::Unsupported(why)))) = run(Effect::LoadPulls, &c) else {
            panic!("unsupported")
        };
        assert_eq!(why, "origin is not on 127.0.0.1");
        t.repo.remote_delete("origin").expect("delete");
        let Some(Msg::Pulls(Err(PullsError::Unsupported(why)))) = run(Effect::LoadPulls, &c) else {
            panic!("no origin")
        };
        assert!(why.contains("no origin"), "{why}");
    }

    #[test]
    fn tokens_come_from_the_environment_and_are_cached() {
        let t = repo();
        let (url, requests) = serve(vec![
            (
                200,
                vec![],
                r#"{"data":{"repository":{"pullRequests":{"nodes":[]}}}}"#.to_owned(),
            ),
            (401, vec![], String::new()),
        ]);
        let mut c = online(&t, &url, None);
        c.tokens.env = |k| (k == "GITHUB_TOKEN").then(|| "env-token".to_owned());
        let Some(Msg::Pulls(Ok((repo, pulls)))) = run(Effect::LoadPulls, &c) else {
            panic!("pulls")
        };
        assert_eq!(repo, "o/r");
        assert!(pulls.is_empty());
        assert!(requests.recv().expect("req").contains("env-token"));
        assert_eq!(
            c.token
                .lock()
                .expect("lock")
                .as_ref()
                .map(github::Token::expose),
            Some("env-token")
        );

        let Some(Msg::Pulls(Err(PullsError::Failed(why)))) = run(Effect::LoadPulls, &c) else {
            panic!("rejected")
        };
        assert!(why.contains("gh auth login"), "{why}");
        assert_eq!(
            *c.token.lock().expect("lock"),
            None,
            "rejected token forgotten"
        );
    }

    #[test]
    fn pull_request_diffs_come_from_github() {
        let t = repo();
        let diff = "diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..92d5444\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+fresh\n";
        let (url, _requests) = serve(vec![
            (200, vec![], diff.to_owned()),
            (401, vec![], String::new()),
            (200, vec![], "diff --git a/x b/x\n@@ broken".to_owned()),
        ]);
        let c = online(&t, &url, Some("tok"));
        let pull = crate::github::PullRequest {
            number: 3,
            title: "Add n".to_owned(),
            author: "claude".to_owned(),
            draft: false,
            head: "agent/n".to_owned(),
            fork: false,
            base: "main".to_owned(),
            updated: 5,
            added: 1,
            removed: 0,
            files: 1,
        };
        let load = |c: &Context| {
            let Some(Msg::Diff { entry, result, .. }) = run(
                Effect::LoadDiff {
                    entry: Box::new(crate::app::pull_entry(&pull)),
                    refresh: false,
                },
                c,
            ) else {
                panic!("diff msg")
            };
            (entry, result)
        };
        let (entry, files) = load(&c);
        assert_eq!(files.expect("files")[0].stat.path, "n.txt");
        assert_eq!(entry.files.len(), 1, "entry gains the files");
        assert_eq!(entry.modules, ["."], "and is labelled like any change");
        assert_eq!((entry.added, entry.removed), (1, 0));
        assert_eq!(entry.short, "#3");
        let (_, rejected) = load(&c);
        assert!(rejected.expect_err("401").contains("rejected"));
        *c.token.lock().expect("lock") = Some(github::Token::new("tok"));
        let (_, broken) = load(&c);
        assert!(broken.expect_err("bad diff").contains("parse diff"));
        *c.token.lock().expect("lock") = None;
        let (_, no_token) = load(&c);
        assert!(no_token.expect_err("no token").contains("gh auth login"));
        let (_, off) = load(&ctx(&t));
        assert!(off.expect_err("off").contains("--no-network"));
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
