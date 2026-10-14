//! vert: understand what your agents are building.
//!
//! A terminal UI that shows a live, labelled feed of a git repository's
//! changes (including uncommitted work), syntax-highlighted diffs, fast
//! file and content search, change hotspots, and every branch, worktree and
//! open pull request.

pub mod age;
pub mod app;
pub mod config;
pub mod editor;
pub mod git;
pub mod github;
pub mod highlight;
pub mod hotspots;
pub mod jobs;
pub mod labels;
pub mod search;
pub mod theme;
pub mod ui;
pub mod watch;

/// The commit this build was made from, when git was available at build
/// time; the update check compares it with `main`.
pub const BUILD_SHA: Option<&str> = option_env!("VERT_SHA");
/// That commit's time, as Unix seconds in text.
pub const BUILD_TIME: Option<&str> = option_env!("VERT_TIME");
