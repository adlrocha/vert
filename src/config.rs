//! Command-line configuration.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::labels::{LabelError, Labeler};

/// Understand what your agents are building: a live, navigable view of a git repo's changes.
#[derive(Debug, Clone, Parser)]
#[command(name = "vert", version, about)]
pub struct Config {
    /// Any path inside the git repository to explore.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum number of commits loaded into the feed (and used for hotspots).
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,

    /// Directory depth that defines a module (1 = top-level folders).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    pub module_depth: u16,

    /// Changes touching at least this many modules are labelled "wide".
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..))]
    pub wide_modules: u16,

    /// Changed lines (added + removed) at which a change is "medium".
    #[arg(long, default_value_t = 50)]
    pub medium_lines: u64,

    /// Changed lines at which a change is "large".
    #[arg(long, default_value_t = 200)]
    pub large_lines: u64,

    /// Changed lines at which a change is "xl".
    #[arg(long, default_value_t = 1000)]
    pub xl_lines: u64,

    /// Quiet period before reacting to file-system changes, in milliseconds.
    #[arg(long, default_value_t = 300)]
    pub debounce_ms: u64,

    /// Maximum number of content-search matches to collect.
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_search_results: u32,

    /// Files or diffs longer than this many lines are shown without syntax colours.
    #[arg(long, default_value_t = 20_000)]
    pub max_highlight_lines: usize,

    /// Files larger than this many KiB are not opened in the viewer.
    #[arg(long, default_value_t = 4096)]
    pub max_file_kb: u64,

    /// Disable live file watching.
    #[arg(long)]
    pub no_watch: bool,

    /// Branch that other branches are compared against (default: the remote's
    /// default branch, else main or master).
    #[arg(long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Maximum branches without a worktree to show (most recent first).
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_branches: u32,

    /// How often the Branches tab rescans branches and worktrees, in seconds.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    pub branch_refresh_secs: u32,

    /// How often the Branches tab refreshes open pull requests, in seconds.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..))]
    pub pr_refresh_secs: u32,

    /// Maximum open pull requests to list.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
    pub max_prs: u16,

    /// GitHub REST API base URL (for GitHub Enterprise: `https://HOST/api/v3`).
    #[arg(long, default_value = "https://api.github.com", value_name = "URL")]
    pub github_api: String,

    /// Seconds before a GitHub request gives up.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    pub github_timeout_secs: u64,

    /// Never contact GitHub (pull requests are not shown).
    #[arg(long)]
    pub no_network: bool,

    /// Never ask GitHub whether a newer vert exists.
    #[arg(long)]
    pub no_update_check: bool,
}

impl Config {
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
            endpoint: crate::github::Endpoint::new("https://api.github.com"),
            timeout: Duration::from_secs(self.github_timeout_secs),
            cache: cache_dir().join("vert/update-check"),
        })
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
    fn rejects_zero_limit() {
        assert!(Config::try_parse_from(["vert", "--limit", "0"]).is_err());
    }
}
