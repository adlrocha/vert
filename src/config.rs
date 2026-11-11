//! Command-line configuration.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::app::Start;
use crate::explain::agent::{self, Choice};
use crate::explain::{Explainer, cache::Cache};
use crate::fix::Fixer;
use crate::labels::{LabelError, Labeler};

/// Understand what your agents are building: a live, navigable view of a git repo's changes.
#[derive(Debug, Clone, Parser)]
#[command(name = "vert", version, about)]
pub struct Config {
    /// Open straight on one line of work instead of the feed.
    #[command(subcommand)]
    pub open: Option<Open>,

    /// Any path inside the git repository to explore.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum number of commits loaded into the feed (and used for hotspots).
    #[arg(long, global = true, default_value_t = 500, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,

    /// Directory depth that defines a module (1 = top-level folders).
    #[arg(long, global = true, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    pub module_depth: u16,

    /// Changes touching at least this many modules are labelled "wide".
    #[arg(long, global = true, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..))]
    pub wide_modules: u16,

    /// Changed lines (added + removed) at which a change is "medium".
    #[arg(long, global = true, default_value_t = 50)]
    pub medium_lines: u64,

    /// Changed lines at which a change is "large".
    #[arg(long, global = true, default_value_t = 200)]
    pub large_lines: u64,

    /// Changed lines at which a change is "xl".
    #[arg(long, global = true, default_value_t = 1000)]
    pub xl_lines: u64,

    /// Quiet period before reacting to file-system changes, in milliseconds.
    #[arg(long, global = true, default_value_t = 300)]
    pub debounce_ms: u64,

    /// Maximum number of content-search matches to collect.
    #[arg(long, global = true, default_value_t = 2000, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_search_results: u32,

    /// Files or diffs longer than this many lines are shown without syntax colours.
    #[arg(long, global = true, default_value_t = 20_000)]
    pub max_highlight_lines: usize,

    /// Files larger than this many KiB are not opened in the viewer.
    #[arg(long, global = true, default_value_t = 4096)]
    pub max_file_kb: u64,

    /// Disable live file watching.
    #[arg(long, global = true)]
    pub no_watch: bool,

    /// Branch that other branches are compared against (default: the remote's
    /// default branch, else main or master).
    #[arg(long, global = true, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Maximum branches without a worktree to show (most recent first).
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_branches: u32,

    /// Files that never count as a collision between branches: file names or
    /// repository-relative paths, separated by commas.
    #[arg(
        long,
        global = true,
        value_name = "FILES",
        value_delimiter = ',',
        default_value = "Cargo.lock,package-lock.json,pnpm-lock.yaml,yarn.lock,go.sum,uv.lock,poetry.lock"
    )]
    pub overlap_ignore: Vec<String>,

    /// Maximum pairs of branches to run a merge check on in one pass. Pairs
    /// past the limit show as overlaps until a later pass reaches them.
    #[arg(long, global = true, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_overlap_checks: u32,

    /// How often the Branches tab rescans branches and worktrees, in seconds.
    #[arg(long, global = true, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    pub branch_refresh_secs: u32,

    /// How often the Branches tab refreshes open pull requests, in seconds.
    #[arg(long, global = true, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..))]
    pub pr_refresh_secs: u32,

    /// Maximum open pull requests to list.
    #[arg(long, global = true, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
    pub max_prs: u16,

    /// GitHub REST API base URL (for GitHub Enterprise: `https://HOST/api/v3`).
    #[arg(
        long,
        global = true,
        default_value = "https://api.github.com",
        value_name = "URL"
    )]
    pub github_api: String,

    /// Seconds before a GitHub request gives up.
    #[arg(long, global = true, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    pub github_timeout_secs: u64,

    /// Never contact GitHub (pull requests are not shown), and explain or fix no changes.
    #[arg(long, global = true)]
    pub no_network: bool,

    /// Never ask GitHub whether a newer vert exists.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    /// Coding agent that explains (`x` in a diff) and fixes (`f`) changes;
    /// `auto` picks the first of claude, codex and pi found on PATH to
    /// explain, and of claude and codex to fix. pi fixes only when named,
    /// and its edits aren't kept inside the checkout.
    #[arg(long, global = true, value_enum, default_value_t = Choice::Auto, value_name = "AGENT", env = "VERT_EXPLAIN_AGENT")]
    pub explain_agent: Choice,

    /// Model the agent explains (`x`) and fixes (`f`) with, in the agent's
    /// own naming (passed as its `--model`, or to `--explain-cmd` in
    /// `VERT_EXPLAIN_MODEL`); unset uses the agent's default.
    #[arg(long, global = true, value_name = "MODEL", env = "VERT_EXPLAIN_MODEL")]
    pub explain_model: Option<String>,

    /// Shell command that explains changes instead of a known agent: it reads
    /// the prompt on stdin and prints its answer.
    #[arg(long, global = true, value_name = "CMD", env = "VERT_EXPLAIN_CMD")]
    pub explain_cmd: Option<String>,

    /// Seconds an agent may take to explain or fix a change.
    #[arg(long, global = true, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..), env = "VERT_EXPLAIN_TIMEOUT_SECS")]
    pub explain_timeout_secs: u64,

    /// KiB of diff sent to the agent in full: to explain, later hunks are
    /// listed by header only; to fix, the excerpt stops at a line. Either
    /// way the agent reads the rest itself.
    #[arg(long, global = true, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..), env = "VERT_EXPLAIN_MAX_DIFF_KB")]
    pub explain_max_diff_kb: u32,
}

/// A line of work to open on start, with the diff already showing.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Open {
    /// Open a pull request's diff on the Branches tab.
    Pr {
        /// The pull request's number.
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        number: u64,
        /// Any path inside the git repository.
        path: Option<PathBuf>,
    },
    /// Open a branch or worktree on the Branches tab: its uncommitted work,
    /// else what it adds since it left the base branch, else its pull request.
    Branch {
        /// The branch, remote branch or worktree directory name.
        name: String,
        /// Any path inside the git repository.
        path: Option<PathBuf>,
    },
}

impl Config {
    /// The path to explore: the one given after `pr`/`branch`, else the main one.
    #[must_use]
    pub fn repo_path(&self) -> &Path {
        match &self.open {
            Some(Open::Pr { path: Some(p), .. } | Open::Branch { path: Some(p), .. }) => p,
            _ => &self.path,
        }
    }

    /// The line of work to open on start, if one was asked for.
    ///
    /// # Errors
    /// Explains why it can't be opened: a pull request needs GitHub.
    pub fn start(&self) -> Result<Option<Start>, String> {
        match &self.open {
            None => Ok(None),
            Some(Open::Pr { .. }) if self.no_network => {
                Err("`vert pr` reads the pull request from GitHub; drop --no-network".to_owned())
            }
            Some(Open::Pr { number, .. }) => Ok(Some(Start::Pull(*number))),
            Some(Open::Branch { name, .. }) => {
                let name = name.trim();
                if name.is_empty() {
                    Err("`vert branch` needs a branch or worktree name".to_owned())
                } else {
                    Ok(Some(Start::Branch(name.to_owned())))
                }
            }
        }
    }

    /// Builds the [`Labeler`] described by this configuration.
    ///
    /// # Errors
    /// Returns [`LabelError`] when the thresholds are inconsistent.
    pub fn labeler(&self) -> Result<Labeler, LabelError> {
        Labeler::new(
            self.medium_lines,
            self.large_lines,
            self.xl_lines,
            usize::from(self.wide_modules),
            usize::from(self.module_depth),
        )
    }

    /// The configured debounce as a [`Duration`].
    #[must_use]
    pub const fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }

    /// Commit limit as a `usize`.
    #[must_use]
    pub fn commit_limit(&self) -> usize {
        usize::try_from(self.limit).unwrap_or(usize::MAX)
    }

    /// Viewer size cap in bytes.
    #[must_use]
    pub const fn max_file_bytes(&self) -> u64 {
        self.max_file_kb.saturating_mul(1024)
    }

    /// Merge check cap as a `usize`.
    #[must_use]
    pub fn max_overlap_checks(&self) -> usize {
        usize::try_from(self.max_overlap_checks).unwrap_or(usize::MAX)
    }

    /// Branch cap as a `usize`.
    #[must_use]
    pub fn branch_limit(&self) -> usize {
        usize::try_from(self.max_branches).unwrap_or(usize::MAX)
    }

    /// GitHub settings, or `None` when network access is off.
    #[must_use]
    pub fn github(&self) -> Option<crate::jobs::GitHub> {
        (!self.no_network).then(|| crate::jobs::GitHub {
            endpoint: crate::github::Endpoint::new(&self.github_api),
            timeout: Duration::from_secs(self.github_timeout_secs),
            max_prs: self.max_prs,
        })
    }

    /// Refresh intervals for the Branches tab.
    #[must_use]
    pub fn refresh(&self) -> crate::app::Refresh {
        crate::app::Refresh {
            branches: Some(i64::from(self.branch_refresh_secs)),
            pulls: (!self.no_network).then(|| i64::from(self.pr_refresh_secs)),
        }
    }

    /// Whether vert may ask GitHub about its own latest commit.
    #[must_use]
    pub const fn update_check(&self) -> bool {
        !self.no_update_check && !self.no_network
    }

    /// How to check for a newer vert, or `None` when the check is off.
    #[must_use]
    pub fn update(&self) -> Option<crate::jobs::Update> {
        self.update_check().then(|| crate::jobs::Update {
            // vert itself lives on public GitHub; `--github-api` describes the
            // viewed repository's Enterprise host, not vert's home.
            endpoint: crate::github::Endpoint::new("https://api.github.com"),
            timeout: Duration::from_secs(self.github_timeout_secs),
            cache: cache_dir().join("vert/update-check"),
        })
    }

    /// How to explain changes, given the `PATH` to look for agents in.
    ///
    /// # Errors
    /// Explains, for the user, why explanations are unavailable.
    pub fn explainer(&self, path: Option<&OsStr>) -> Result<Explainer, String> {
        if self.no_network {
            return Err("network access is off (--no-network)".to_owned());
        }
        let agent = agent::select(self.explain_agent, self.explain_cmd.as_deref(), path)?;
        Ok(Explainer {
            agent,
            model: self.model(),
            timeout: Duration::from_secs(self.explain_timeout_secs),
            max_diff_bytes: self.max_diff_bytes(),
            cache: Some(Cache::new(cache_dir().join("vert/explain"))),
        })
    }

    /// How to fix changes, given the `PATH` to look for agents in. Fixes use
    /// the agent and model explanations use: `auto` picks the first agent
    /// that keeps its edits inside the checkout
    /// ([`agent::Agent::confines_edits`]), claude or codex; pi, which
    /// doesn't, fixes only when named; `--explain-cmd` is never used.
    ///
    /// # Errors
    /// Explains, for the user, why fixes are unavailable.
    pub fn fixer(&self, path: Option<&OsStr>) -> Result<Fixer, String> {
        if self.no_network {
            return Err("network access is off (--no-network)".to_owned());
        }
        let agent = agent::select_editor(self.explain_agent, path)?;
        Ok(Fixer {
            agent,
            model: self.model(),
            timeout: Duration::from_secs(self.explain_timeout_secs),
            max_excerpt_bytes: self.max_diff_bytes(),
        })
    }

    /// The model asked for, unless blank.
    fn model(&self) -> Option<String> {
        self.explain_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
    }

    /// `--explain-max-diff-kb` in bytes.
    fn max_diff_bytes(&self) -> usize {
        usize::try_from(self.explain_max_diff_kb)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024)
    }

    /// Search-result cap as a `usize`.
    #[must_use]
    pub fn search_limit(&self) -> usize {
        usize::try_from(self.max_search_results).unwrap_or(usize::MAX)
    }
}

/// The user's cache directory: `XDG_CACHE_HOME`, else `~/.cache`.
fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_check_can_be_disabled() {
        let on = Config::parse_from(["vert"]);
        assert!(on.update_check());
        let update = on.update().expect("update settings");
        assert_eq!(
            update.endpoint,
            crate::github::Endpoint::new("https://api.github.com")
        );
        assert_eq!(update.timeout, Duration::from_secs(15));
        assert!(
            update.cache.ends_with("vert/update-check"),
            "{:?}",
            update.cache
        );

        for args in [["vert", "--no-update-check"], ["vert", "--no-network"]] {
            let config = Config::parse_from(args);
            assert!(!config.update_check(), "{args:?}");
            assert!(config.update().is_none(), "{args:?}");
        }
    }

    #[test]
    fn defaults_are_valid() {
        let config = Config::parse_from(["vert"]);
        assert_eq!(config.path, PathBuf::from("."));
        assert!(config.labeler().is_ok());
        assert_eq!(config.debounce(), Duration::from_millis(300));
        assert_eq!(config.commit_limit(), 500);
        assert_eq!(config.search_limit(), 2000);
        assert_eq!(config.max_file_bytes(), 4096 * 1024);
        assert_eq!(config.branch_limit(), 30);
        assert_eq!(config.max_overlap_checks(), 100);
        assert!(config.overlap_ignore.iter().any(|f| f == "Cargo.lock"));
        assert_eq!(config.base, None);
        let github = config.github().expect("network on by default");
        assert_eq!(github.endpoint.web_host(), "github.com");
        assert_eq!(github.timeout, Duration::from_secs(15));
        assert_eq!(github.max_prs, 50);
        let refresh = config.refresh();
        assert_eq!((refresh.branches, refresh.pulls), (Some(5), Some(60)));
    }

    #[test]
    fn no_network_turns_off_pull_requests() {
        let config = Config::parse_from(["vert", "--no-network", "--base", "develop"]);
        assert!(config.github().is_none());
        assert_eq!(config.refresh().pulls, None);
        assert_eq!(config.base.as_deref(), Some("develop"));
        assert!(Config::try_parse_from(["vert", "--max-prs", "101"]).is_err());
    }

    #[test]
    fn rejects_inconsistent_thresholds() {
        let config = Config::parse_from(["vert", "--medium-lines", "500", "--large-lines", "100"]);
        assert!(config.labeler().is_err());
    }

    #[test]
    fn explanations_pick_an_agent_unless_offline() {
        let config =
            // Explicit flags, so `VERT_EXPLAIN_*` in the shell doesn't matter.
            Config::parse_from([
                "vert",
                "--explain-cmd",
                "llm",
                "--explain-max-diff-kb",
                "2",
                "--explain-timeout-secs",
                "600",
                "--explain-model",
                "",
            ]);
        let ex = config.explainer(None).expect("custom command");
        assert_eq!(ex.agent, agent::Agent::Custom("llm".to_owned()));
        assert_eq!(ex.max_diff_bytes, 2048);
        assert_eq!(ex.timeout, Duration::from_secs(600));
        assert!(ex.cache.is_some());
        assert_eq!(ex.model, None);
        let model = |m: &str| {
            Config::parse_from(["vert", "--explain-cmd", "llm", "--explain-model", m])
                .explainer(None)
                .expect("custom command")
                .model
        };
        assert_eq!(model(" opus "), Some("opus".to_owned()));
        assert_eq!(model(" "), None, "blank means the agent's default");

        let none = Config::parse_from(["vert", "--explain-agent", "codex", "--explain-cmd", ""])
            .explainer(None)
            .expect_err("codex is not on an empty PATH");
        assert!(none.contains("codex"), "{none}");
        let off = Config::parse_from(["vert", "--no-network", "--explain-cmd", "llm"])
            .explainer(None)
            .expect_err("offline");
        assert!(off.contains("--no-network"), "{off}");
        assert!(Config::try_parse_from(["vert", "--explain-agent", "gpt"]).is_err());
        assert!(Config::try_parse_from(["vert", "--explain-timeout-secs", "0"]).is_err());
    }

    #[test]
    fn opens_a_pull_request_or_branch_on_start() {
        let plain = Config::parse_from(["vert", "~/code/app"]);
        assert_eq!(plain.start(), Ok(None));
        assert_eq!(plain.repo_path(), Path::new("~/code/app"));

        let pr = Config::parse_from(["vert", "pr", "12"]);
        assert_eq!(pr.start(), Ok(Some(Start::Pull(12))));
        assert_eq!(pr.repo_path(), Path::new("."));
        // Options and the repository path work after the subcommand too.
        let pr = Config::parse_from(["vert", "pr", "12", "../app", "--limit", "7"]);
        assert_eq!(pr.repo_path(), Path::new("../app"));
        assert_eq!(pr.limit, 7);
        let pr = Config::parse_from(["vert", "--limit", "8", "pr", "3"]);
        assert_eq!((pr.limit, pr.start()), (8, Ok(Some(Start::Pull(3)))));
        let before = Config::parse_from(["vert", "../app", "pr", "4"]);
        assert_eq!(before.repo_path(), Path::new("../app"));
        assert_eq!(before.start(), Ok(Some(Start::Pull(4))));

        let offline = Config::parse_from(["vert", "pr", "12", "--no-network"]);
        assert!(offline.start().is_err_and(|e| e.contains("--no-network")));
        assert!(Config::try_parse_from(["vert", "pr", "0"]).is_err());
        assert!(Config::try_parse_from(["vert", "pr", "abc"]).is_err());
        assert!(Config::try_parse_from(["vert", "pr"]).is_err());

        let branch = Config::parse_from(["vert", "branch", " feat/x ", "--no-network"]);
        assert_eq!(branch.start(), Ok(Some(Start::Branch("feat/x".to_owned()))));
        assert!(Config::parse_from(["vert", "branch", " "]).start().is_err());
        assert!(Config::try_parse_from(["vert", "branch"]).is_err());
    }

    /// A directory holding fake agents named `names`.
    fn agents(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        dir
    }

    #[test]
    fn fixes_use_an_agent_that_stays_in_the_checkout() {
        let fixer_with = |installed: &[&str], args: &[&str]| {
            let dir = agents(installed);
            let config = Config::parse_from(["vert"].iter().chain(args));
            config.fixer(Some(dir.path().as_os_str()))
        };
        let explicit = ["--explain-cmd", "llm", "--explain-agent"];
        let auto = [&explicit[..], &["auto"]].concat();
        let fixer = fixer_with(
            &["pi", "codex", "claude"],
            &[
                &auto[..],
                &[
                    "--explain-model",
                    " x/y ",
                    "--explain-timeout-secs",
                    "60",
                    "--explain-max-diff-kb",
                    "3",
                ],
            ]
            .concat(),
        )
        .expect("claude is installed");
        assert_eq!(
            fixer.agent,
            agent::Agent::Claude,
            "--explain-cmd is skipped"
        );
        assert_eq!(fixer.model.as_deref(), Some("x/y"));
        assert_eq!(fixer.timeout, Duration::from_secs(60));
        assert_eq!(fixer.max_excerpt_bytes, 3072);
        let codex = fixer_with(&["pi", "codex"], &auto).expect("codex");
        assert_eq!(codex.agent, agent::Agent::Codex, "pi is passed over");
        let none = fixer_with(&["pi"], &auto).expect_err("only pi");
        assert!(none.contains("install Claude Code or Codex"), "{none}");
        assert!(none.contains("VERT_EXPLAIN_AGENT=pi"), "{none}");
        let chosen = fixer_with(&["claude", "codex"], &[&explicit[..], &["codex"]].concat());
        assert_eq!(chosen.map(|f| f.agent), Ok(agent::Agent::Codex));
        let missing = fixer_with(&["codex"], &[&explicit[..], &["claude"]].concat());
        assert!(missing.is_err_and(|e| e.contains("claude is not on PATH")));
        let pi = fixer_with(&["claude", "pi"], &[&explicit[..], &["pi"]].concat());
        assert_eq!(pi.map(|f| f.agent), Ok(agent::Agent::Pi), "pi when named");
        let offline = fixer_with(&["claude"], &["--no-network"]);
        assert!(offline.is_err_and(|e| e.contains("--no-network")));
    }

    #[test]
    fn rejects_zero_limit() {
        assert!(Config::try_parse_from(["vert", "--limit", "0"]).is_err());
    }
}
