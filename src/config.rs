//! Command-line configuration.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::labels::{LabelError, Labeler};

/// Grok what your agents are building: a live, navigable view of a git repo's changes.
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

    /// Search-result cap as a `usize`.
    #[must_use]
    pub fn search_limit(&self) -> usize {
        usize::try_from(self.max_search_results).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let config = Config::parse_from(["vert"]);
        assert_eq!(config.path, PathBuf::from("."));
        assert!(config.labeler().is_ok());
        assert_eq!(config.debounce(), Duration::from_millis(300));
        assert_eq!(config.commit_limit(), 500);
        assert_eq!(config.search_limit(), 2000);
        assert_eq!(config.max_file_bytes(), 4096 * 1024);
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
