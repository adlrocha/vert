//! Application state and input handling.
//!
//! [`App::update`] is a pure state transition: it consumes a [`Msg`] and
//! returns the [`Effect`]s (I/O) the runtime should perform. This keeps all
//! interaction logic testable without a terminal or repository.

mod agents;
mod ask;
mod branches;
mod explain;
mod fix;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub use agents::{AgentsState, Picker, model_hint, pick_name};
pub use ask::{AnswerView, Answered, AskDraft, AskState, Pending};
pub use branches::{
    BranchRemoval, BranchesState, CollisionList, CollisionRow, PullStatus, Refresh, Removal, Row,
    RowFilter, Start, pull_entry,
};
#[cfg(test)]
pub(crate) use explain::pi_for_tests;
pub use explain::{
    ExplainState, ExplainView, Item, Pane, Reload, Status, Stop, concern_at, items, row_pointer,
    stops, targets,
};
pub use fix::{ConcernFix, Concerns, Draft, Finished, FixState, Running, concern_key};

use crate::explain::Answer;
use crate::git::RepoInfo;
use crate::git::branches::Branches;
use crate::git::diff::{FileDiff, LineKind};
use crate::git::feed::Entry;
use crate::git::overlap::Overlaps;
use crate::github::{PullRequest, PullsError};
use crate::highlight::{Fragment, Highlighter};
use crate::hotspots::{self, Grouping, Hotspot, Window};
use crate::labels::module_of;
use crate::search::{self, FileMatch, TextResults};
use crate::ui::detail::{RenderedDiff, build_diff};
use crate::watch::Change;

/// Maximum fuzzy file results kept for display.
pub const FILE_RESULTS: usize = 500;
/// Columns moved per horizontal scroll step.
pub const H_SCROLL_STEP: usize = 8;

/// Top-level tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Change feed.
    Feed,
    /// Hotspots.
    Hotspots,
    /// Search.
    Search,
    /// Branches, worktrees and pull requests.
    Branches,
}

impl Tab {
    /// All tabs in display order.
    pub const ALL: [Self; 4] = [Self::Feed, Self::Hotspots, Self::Search, Self::Branches];

    /// Display name.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Feed => "Feed",
            Self::Hotspots => "Hotspots",
            Self::Search => "Search",
            Self::Branches => "Branches",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Feed => Self::Hotspots,
            Self::Hotspots => Self::Search,
            Self::Search => Self::Branches,
            Self::Branches => Self::Feed,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Feed => Self::Branches,
            Self::Hotspots => Self::Feed,
            Self::Search => Self::Hotspots,
            Self::Branches => Self::Search,
        }
    }
}

/// Search flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Fuzzy file names.
    Files,
    /// File contents.
    Text,
}

/// Which pane of the detail view has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// File list.
    Files,
    /// Diff.
    Diff,
}

/// State of live watching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchStatus {
    /// Watching for changes.
    Live,
    /// Turned off by the user.
    Disabled,
    /// Failed to start.
    Failed(String),
}

/// Restricts the feed to changes touching a file or module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// File path or module name.
    pub key: String,
    /// Whether `key` is a file or module.
    pub grouping: Grouping,
}

/// A scrollable list position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListPos {
    /// Selected index.
    pub selected: usize,
    /// First visible index.
    pub offset: usize,
    /// Rows visible in the last frame.
    pub height: usize,
}

impl ListPos {
    fn move_by(&mut self, delta: isize, len: usize) {
        self.selected = if len == 0 {
            0
        } else {
            self.selected.saturating_add_signed(delta).min(len - 1)
        };
    }

    fn page(&self) -> isize {
        isize::try_from(self.height.max(2) / 2).unwrap_or(1)
    }

    /// Keeps the selection visible, returning the offset to render from.
    pub fn scrolled(&mut self, height: usize, len: usize) -> usize {
        self.height = height;
        self.selected = self.selected.min(len.saturating_sub(1));
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if height > 0 && self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset = self.offset.min(len.saturating_sub(height));
        self.offset
    }

    /// Handles common navigation keys; returns whether the key was consumed.
    fn navigate(&mut self, key: &KeyEvent, len: usize) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.move_by(1, len),
            KeyCode::Char('n' | 'j') if ctrl => self.move_by(1, len),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.move_by(-1, len),
            KeyCode::Char('p' | 'k') if ctrl => self.move_by(-1, len),
            KeyCode::PageDown => self.move_by(self.page() * 2, len),
            KeyCode::PageUp => self.move_by(-self.page() * 2, len),
            KeyCode::Char('d') if ctrl => self.move_by(self.page(), len),
            KeyCode::Char('u') if ctrl => self.move_by(-self.page(), len),
            KeyCode::Home | KeyCode::Char('g') if !ctrl => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = len.saturating_sub(1),
            _ => return false,
        }
        true
    }
}

/// Tracks one kind of background load so requests never pile up: while a
/// load is in flight, further requests collapse into a single follow-up.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Loader {
    in_flight: bool,
    pending: bool,
}

impl Loader {
    /// Returns `true` if a load should start now.
    fn request(&mut self) -> bool {
        if self.in_flight {
            self.pending = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// Marks the load finished; returns `true` if a follow-up should start.
    fn finish(&mut self) -> bool {
        self.in_flight = false;
        std::mem::take(&mut self.pending) && self.request()
    }
}

/// Feed tab state.
#[derive(Debug, Default)]
pub struct FeedState {
    /// Commits, newest first.
    pub commits: Vec<Entry>,
    /// Uncommitted changes, if any.
    pub worktree: Option<Entry>,
    /// Selection.
    pub pos: ListPos,
    /// Active filter.
    pub filter: Option<Filter>,
    /// Ids seen in the first load or opened since.
    known: HashSet<String>,
    /// Ids that arrived while vert was running and have not been opened.
    pub fresh: HashSet<String>,
    /// Whether the first load completed.
    pub loaded: bool,
    /// Last load error.
    pub error: Option<String>,
    commits_load: Loader,
    worktree_load: Loader,
}

impl FeedState {
    /// Entries shown, after filtering: the working tree first, then commits.
    #[must_use]
    pub fn visible(&self, depth: usize) -> Vec<&Entry> {
        self.worktree
            .iter()
            .chain(&self.commits)
            .filter(|e| {
                self.filter.as_ref().is_none_or(|f| match f.grouping {
                    Grouping::Files => e.touches_path(&f.key),
                    Grouping::Modules => e
                        .files
                        .iter()
                        .any(|file| module_of(&file.path, depth) == f.key),
                })
            })
            .collect()
    }

    /// Whether any load is in flight.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.commits_load.in_flight || self.worktree_load.in_flight
    }
}

/// Hotspots tab state.
#[derive(Debug)]
pub struct HotspotState {
    /// Time window.
    pub window: Window,
    /// File or module grouping.
    pub grouping: Grouping,
    /// Ranked hotspots.
    pub spots: Vec<Hotspot>,
    /// Selection.
    pub pos: ListPos,
}

/// Search tab state.
#[derive(Debug)]
pub struct SearchState {
    /// Current query.
    pub query: String,
    /// Files or text.
    pub mode: SearchMode,
    /// All files, once listed.
    pub files: Option<Vec<String>>,
    /// Whether the file list must be reloaded before the next file search.
    files_stale: bool,
    files_loading: bool,
    /// Fuzzy file results.
    pub file_hits: Vec<FileMatch>,
    /// Content results.
    pub text: TextResults,
    /// A content search is running.
    pub text_running: bool,
    /// Last content-search error.
    pub error: Option<String>,
    /// Generation of the latest content search request.
    pub generation: u64,
    /// Selection.
    pub pos: ListPos,
}

impl SearchState {
    /// Number of selectable results in the current mode.
    #[must_use]
    pub const fn len(&self) -> usize {
        match self.mode {
            SearchMode::Files => self.file_hits.len(),
            SearchMode::Text => self.text.matches.len(),
        }
    }

    /// Whether there are no results.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The selected result as `(path, line)`.
    #[must_use]
    pub fn selected(&self) -> Option<(String, u32)> {
        match self.mode {
            SearchMode::Files => self
                .file_hits
                .get(self.pos.selected)
                .map(|h| (h.path.clone(), 1)),
            SearchMode::Text => self
                .text
                .matches
                .get(self.pos.selected)
                .map(|m| (m.path.clone(), u32::try_from(m.line).unwrap_or(u32::MAX))),
        }
    }

    /// Whether the file list is being loaded.
    #[must_use]
    pub const fn files_loading(&self) -> bool {
        self.files_loading
    }
}

/// Rows selected with `v` in a view with a cursor (the diff pane, the file
/// viewer), for `a` and `f` to be about: every row from `anchor` to `end`,
/// both included.
///
/// The first `v` starts a selection at the cursor and leaves it open: the
/// end follows the cursor. The second `v` closes it there, so the cursor
/// moves on without changing what is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The row the selection started at.
    pub anchor: usize,
    /// The row it ends at: the cursor's, while it is open.
    pub end: usize,
    /// Whether the end still follows the cursor.
    pub open: bool,
}

impl Selection {
    /// The first and last selected rows.
    ///
    /// ```
    /// let s = vert::app::Selection { anchor: 7, end: 3, open: false };
    /// assert_eq!(s.rows(), (3, 7));
    /// assert!(s.contains(5) && !s.contains(8));
    /// ```
    #[must_use]
    pub fn rows(self) -> (usize, usize) {
        (self.anchor.min(self.end), self.anchor.max(self.end))
    }

    /// Whether `row` is selected.
    #[must_use]
    pub fn contains(self, row: usize) -> bool {
        let (first, last) = self.rows();
        (first..=last).contains(&row)
    }

    /// `v` with the cursor at `cursor`: closes an open selection there, or
    /// starts a new one, replacing a closed one.
    fn toggled(selection: Option<Self>, cursor: usize) -> Self {
        match selection {
            Some(s) if s.open => Self {
                end: cursor,
                open: false,
                ..s
            },
            _ => Self {
                anchor: cursor,
                end: cursor,
                open: true,
            },
        }
    }

    /// The cursor moved to `cursor`: an open selection's end follows it.
    fn follow(selection: &mut Option<Self>, cursor: usize) {
        if let Some(s) = selection.as_mut().filter(|s| s.open) {
            s.end = cursor;
        }
    }
}

/// The diff detail view.
#[derive(Debug)]
pub struct DetailView {
    /// The change being inspected.
    pub entry: Entry,
    /// Per-file diffs.
    pub files: Vec<FileDiff>,
    /// File list selection.
    pub pos: ListPos,
    /// Focused pane.
    pub focus: Focus,
    /// First visible diff line.
    pub scroll: usize,
    /// The diff pane's cursor: the row `j`/`k` move and `v` selects from.
    pub cursor: usize,
    /// Horizontal scroll in columns.
    pub hscroll: usize,
    /// Visible diff lines in the last frame.
    pub height: usize,
    /// Rendered lines of the selected file.
    pub rendered: RenderedDiff,
    /// Lines selected in the diff pane (`v`), if any.
    pub selection: Option<Selection>,
    /// Narrows the file list to the paths that fuzzily match it (`/`).
    pub filter: String,
    /// Whether the filter is being typed.
    pub filter_editing: bool,
}

impl DetailView {
    /// Indices of the files the filter shows, in order: every file when it
    /// is empty.
    #[must_use]
    pub fn shown(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| crate::search::fuzzy_match(&f.stat.path, &self.filter))
            .map(|(i, _)| i)
            .collect()
    }

    /// Where the selected file sits among those [`DetailView::shown`]:
    /// the list position to draw and move from.
    #[must_use]
    pub fn shown_pos(&self, shown: &[usize]) -> ListPos {
        ListPos {
            selected: shown
                .iter()
                .position(|&i| i == self.pos.selected)
                .unwrap_or(0),
            ..self.pos
        }
    }

    /// Moves the file selection among the files shown for `key`; returns
    /// whether `key` was a move.
    fn navigate_files(&mut self, key: &KeyEvent) -> bool {
        let shown = self.shown();
        let mut pos = self.shown_pos(&shown);
        if !pos.navigate(key, shown.len()) {
            return false;
        }
        if let Some(&i) = shown.get(pos.selected) {
            self.pos.selected = i;
        }
        true
    }

    /// Clears the filter when it hides the selected file, so the list
    /// always shows the file whose diff is open.
    fn reveal(&mut self) {
        if !self.shown().contains(&self.pos.selected) {
            self.filter.clear();
        }
    }

    /// The file shown after (`forward`) or before the selected one.
    fn next_shown(&self, forward: bool) -> Option<usize> {
        let shown = self.shown();
        let selected = self.pos.selected;
        if forward {
            shown.into_iter().find(|&i| i > selected)
        } else {
            shown.into_iter().rev().find(|&i| i < selected)
        }
    }
}

/// What `a` and `f` in the file viewer act on (see [`App::file_pick`]).
struct FilePick {
    /// The change the file was opened from.
    entry: Entry,
    /// Its diff.
    files: Vec<FileDiff>,
    /// The file's diff in it, if it has one.
    file: Option<FileDiff>,
    /// The file.
    path: String,
    /// The place and excerpt of the lines selected, if any.
    lines: Option<(String, String)>,
}

/// The file viewer.
#[derive(Debug)]
pub struct FileView {
    /// Repository-relative path.
    pub path: String,
    /// Detected language name.
    pub language: String,
    /// Highlighted lines, for drawing.
    pub lines: Vec<Vec<Fragment>>,
    /// The lines as they are in the file, for the agent (`a`, `f`): the
    /// highlighted ones are made safe to draw, with tabs expanded.
    pub text: Vec<String>,
    /// Line (1-based) to emphasise.
    pub target: u32,
    /// Lines (1-based) added by the change the file was opened from.
    pub marks: BTreeSet<u32>,
    /// First visible line (0-based).
    pub scroll: usize,
    /// The cursor's line (0-based): the line `j`/`k` move and `v` selects
    /// from.
    pub cursor: usize,
    /// Lines selected (`v`), if any, as 0-based rows.
    pub selection: Option<Selection>,
    /// Horizontal scroll in columns.
    pub hscroll: usize,
    /// Visible lines in the last frame.
    pub height: usize,
    /// Shown instead of content (binary, missing, unreadable).
    pub notice: Option<String>,
    /// Linked worktree the file was read from, or `None` for vert's own.
    pub root: Option<PathBuf>,
}

/// A view stacked above the tabs.
#[derive(Debug)]
pub enum View {
    /// Diff of one change.
    Detail(Box<DetailView>),
    /// A file's contents.
    File(Box<FileView>),
    /// An agent's explanation of the change underneath.
    Explain(Box<ExplainView>),
    /// Answers to questions asked from the diff underneath (`a`).
    Answer(Box<AnswerView>),
}

impl View {
    /// The diff, when this view is one.
    #[must_use]
    pub fn as_detail(&self) -> Option<&DetailView> {
        match self {
            Self::Detail(d) => Some(d),
            _ => None,
        }
    }

    /// The diff, when this view is one, to change.
    pub fn as_detail_mut(&mut self) -> Option<&mut DetailView> {
        match self {
            Self::Detail(d) => Some(d),
            _ => None,
        }
    }
}

/// Inputs to [`App::update`].
#[derive(Debug)]
pub enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// Periodic tick (spinner, relative times).
    Tick(i64),
    /// Commit history loaded.
    Commits(Result<Vec<Entry>, String>),
    /// Working tree loaded.
    Worktree(Result<Option<Entry>, String>),
    /// The repository changed on disk.
    Changed(BTreeSet<Change>),
    /// Branches and worktrees loaded.
    Branches(Result<Branches, String>),
    /// Collisions between lines of work were found.
    Overlaps(Result<Overlaps, String>),
    /// Open pull requests loaded: the repository and its pull requests.
    Pulls(Result<(String, Vec<PullRequest>), PullsError>),
    /// The file list for search was built.
    Files(Vec<String>),
    /// A content search finished.
    Text {
        /// Request generation, to discard stale results.
        generation: u64,
        /// Outcome.
        result: Result<TextResults, String>,
    },
    /// A diff for the detail view was loaded.
    Diff {
        /// Change the diff belongs to.
        entry: Box<Entry>,
        /// Outcome.
        result: Result<Vec<FileDiff>, String>,
        /// Whether this refreshes an open detail view rather than opening one.
        refresh: bool,
        /// The file to start on, if not the first.
        file: Option<String>,
    },
    /// The diff an explanation asked again (`r`) reloads was loaded.
    Reloaded {
        /// Which reload it answers: [`Reload::id`].
        reload: u64,
        /// Change the diff belongs to.
        entry: Box<Entry>,
        /// Outcome.
        result: Result<Vec<FileDiff>, String>,
    },
    /// A file for the viewer was read.
    File {
        /// Path relative to the repository.
        path: String,
        /// File text, or a notice explaining why it cannot be shown.
        content: Result<String, String>,
        /// Line to emphasise.
        line: u32,
        /// Added lines to mark.
        marks: BTreeSet<u32>,
        /// Linked worktree the file was read from.
        root: Option<PathBuf>,
    },
    /// Something to tell the user.
    Notice(String),
    /// A removal finished: what went, and any failure after that.
    Removed {
        /// What was removed, named for the user (`"worktree agent"`).
        removed: Vec<String>,
        /// Why the removal stopped, naming what had already gone.
        error: Option<String>,
    },
    /// The update check answered.
    Update(UpdateStatus),
    /// Open this line of work as soon as it has loaded (`vert pr`, `vert branch`).
    Start(Start),
    /// An agent answered about a change.
    Explained {
        /// The diff it was asked about (see [`crate::explain::diff_key`]).
        key: u64,
        /// The explanation, or why there is none.
        result: Result<Answer, String>,
    },
    /// An agent answered a follow-up question about a change.
    Answered {
        /// The diff it was asked about (see [`crate::explain::diff_key`]).
        key: u64,
        /// The answer, or why there is none.
        result: Result<crate::explain::question::Question, String>,
    },
    /// An agent finished acting on feedback.
    Fixed {
        /// The request (see [`Effect::Fix`]).
        id: u64,
        /// What the agent said it did and what became of its edits, or
        /// why it failed.
        result: Result<crate::fix::Done, String>,
    },
}

/// Side effects requested by [`App::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Exit.
    Quit,
    /// Reload commit history.
    LoadCommits,
    /// Reload uncommitted changes.
    LoadWorktree,
    /// List files for search.
    LoadFiles,
    /// Reload branches and worktrees.
    LoadBranches,
    /// Reload open pull requests.
    LoadPulls,
    /// Find where these lines of work collide.
    LoadOverlaps(Box<Branches>),
    /// Run a content search.
    SearchText {
        /// Query.
        query: String,
        /// Request generation.
        generation: u64,
    },
    /// Load the diff of an entry.
    LoadDiff {
        /// The change.
        entry: Box<Entry>,
        /// Whether to refresh the open detail view.
        refresh: bool,
        /// The file to start on, if not the first.
        file: Option<String>,
    },
    /// Load the diff of an entry for an explanation asked again (`r`); it
    /// also refreshes the open detail view.
    ReloadDiff {
        /// Which reload this is: [`Reload::id`].
        reload: u64,
        /// The change.
        entry: Box<Entry>,
    },
    /// Read a file for the viewer.
    OpenFile {
        /// Repository-relative path.
        path: String,
        /// Line to emphasise.
        line: u32,
        /// Added lines to mark.
        marks: BTreeSet<u32>,
        /// Linked worktree to read from, or `None` for vert's own.
        root: Option<PathBuf>,
    },
    /// Open the user's editor.
    Edit {
        /// Repository-relative path.
        path: String,
        /// Line to open at.
        line: u32,
        /// Linked worktree the path is in, or `None` for vert's own.
        root: Option<PathBuf>,
    },
    /// Remove a worktree and/or a local branch, after confirmation.
    Remove {
        /// The linked worktree: its name and path.
        worktree: Option<(String, PathBuf)>,
        /// The local branch to delete, with the tip it must still point at.
        branch: Option<BranchRemoval>,
    },
    /// Ask GitHub whether a newer vert exists.
    CheckUpdate,
    /// Ask a coding agent to explain a change.
    Explain {
        /// Identifies the exact diff (see [`crate::explain::diff_key`]).
        key: u64,
        /// The change.
        entry: Box<Entry>,
        /// Its diff.
        files: Vec<FileDiff>,
        /// Ask even if an answer is cached.
        fresh: bool,
    },
    /// Ask a coding agent a follow-up question about a change it explained.
    Ask(Box<crate::explain::question::Ask>),
    /// Use this agent and model from now on (`M`): the runtime builds them
    /// and reports back with [`App::agents_changed`].
    SetAgent(crate::explain::agent::Setting),
    /// Ask a coding agent to act on feedback, editing files in a checkout.
    Fix {
        /// Identifies the request in [`Msg::Fixed`].
        id: u64,
        /// The linked worktree to edit, or `None` for vert's own checkout.
        root: Option<PathBuf>,
        /// The feedback and where it points.
        request: crate::fix::Request,
    },
}

/// What the once-a-day update check found out about vert itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// This build is at or past `main`.
    Current,
    /// `main` has commits past this build.
    Available,
}

/// Whole-application state.
#[derive(Debug)]
pub struct App {
    /// Repository name and branch.
    pub repo: RepoInfo,
    /// Active tab.
    pub tab: Tab,
    previous_tab: Tab,
    /// Feed state.
    pub feed: FeedState,
    /// Hotspots state.
    pub hotspots: HotspotState,
    /// Search state.
    pub search: SearchState,
    /// Branches state.
    pub branches: BranchesState,
    /// Branches-tab reload intervals.
    refresh: Refresh,
    /// Explanations of changes.
    pub explain: ExplainState,
    /// Fixes asked of the agent.
    pub fix: FixState,
    /// Questions asked from diffs.
    pub ask: AskState,
    /// The agent and model in use, and the picker (`M`).
    pub agents: AgentsState,
    /// Stacked views (detail, file); the last one is on screen.
    pub views: Vec<View>,
    /// Whether the help overlay is shown.
    pub help: bool,
    /// Live-watch status.
    pub watch: WatchStatus,
    /// What the update check found, once it answered.
    pub update: Option<UpdateStatus>,
    /// Transient message for the footer.
    pub notice: Option<String>,
    /// Animation counter.
    pub tick: usize,
    /// Current time (Unix seconds).
    pub now: i64,
    /// Module depth for grouping.
    pub depth: usize,
    /// Highlighting cap.
    pub max_highlight: usize,
}

impl App {
    /// Creates the application in its initial (loading) state, returning the
    /// effects that kick off loading. `updates` asks GitHub (at most once a
    /// day) whether a newer vert exists.
    #[must_use]
    pub fn new(
        repo: RepoInfo,
        depth: usize,
        max_highlight: usize,
        now: i64,
        updates: bool,
    ) -> (Self, Vec<Effect>) {
        let mut app = Self {
            repo,
            tab: Tab::Feed,
            previous_tab: Tab::Feed,
            feed: FeedState::default(),
            hotspots: HotspotState {
                window: Window::Week,
                grouping: Grouping::Files,
                spots: Vec::new(),
                pos: ListPos::default(),
            },
            search: SearchState {
                query: String::new(),
                mode: SearchMode::Files,
                files: None,
                files_stale: false,
                files_loading: false,
                file_hits: Vec::new(),
                text: TextResults::default(),
                text_running: false,
                error: None,
                generation: 0,
                pos: ListPos::default(),
            },
            branches: BranchesState::default(),
            refresh: Refresh::default(),
            explain: ExplainState::default(),
            fix: FixState::default(),
            ask: AskState::default(),
            agents: AgentsState::default(),
            views: Vec::new(),
            help: false,
            watch: WatchStatus::Live,
            update: None,
            notice: None,
            tick: 0,
            now,
            depth,
            max_highlight,
        };
        let mut effects = app.request_commits();
        effects.extend(app.request_worktree());
        if updates && crate::BUILD_SHA.is_some() {
            effects.push(Effect::CheckUpdate);
        }
        (app, effects)
    }

    fn request_commits(&mut self) -> Vec<Effect> {
        if self.feed.commits_load.request() {
            vec![Effect::LoadCommits]
        } else {
            Vec::new()
        }
    }

    fn request_worktree(&mut self) -> Vec<Effect> {
        if self.feed.worktree_load.request() {
            vec![Effect::LoadWorktree]
        } else {
            Vec::new()
        }
    }

    fn request_files(&mut self) -> Vec<Effect> {
        if self.search.files_loading {
            Vec::new()
        } else {
            self.search.files_loading = true;
            vec![Effect::LoadFiles]
        }
    }

    /// Applies `msg`, returning the effects to perform.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Tick(now) => {
                self.tick = self.tick.wrapping_add(1);
                self.now = now;
                self.poll_branches()
            }
            Msg::Branches(result) => self.on_branches(result),
            Msg::Overlaps(result) => self.on_overlaps(result),
            Msg::Pulls(result) => self.on_pulls(result),
            Msg::Commits(result) => self.on_commits(result),
            Msg::Worktree(result) => self.on_worktree(result),
            Msg::Changed(changes) => self.on_changed(&changes),
            Msg::Files(files) => {
                self.search.files_loading = false;
                self.search.files_stale = false;
                self.search.files = Some(files);
                self.refresh_file_hits();
                Vec::new()
            }
            Msg::Text { generation, result } => {
                if generation == self.search.generation {
                    self.search.text_running = false;
                    match result {
                        Ok(text) => {
                            self.search.text = text;
                            self.search.error = None;
                        }
                        Err(e) => self.search.error = Some(e),
                    }
                    self.search.pos = ListPos::default();
                }
                Vec::new()
            }
            Msg::Diff {
                entry,
                result,
                refresh,
                file,
            } => {
                match result {
                    Ok(files) if refresh => self.refresh_detail(*entry, files),
                    Ok(files) => self.open_detail(*entry, files, file.as_deref()),
                    Err(e) => self.notice = Some(format!("Could not load diff: {e}")),
                }
                Vec::new()
            }
            Msg::Reloaded {
                reload,
                entry,
                result,
            } => {
                let effects = self.on_reloaded(reload, &entry, &result);
                match result {
                    Ok(files) => self.refresh_detail(*entry, files),
                    Err(e) => self.notice = Some(format!("Could not load diff: {e}")),
                }
                effects
            }
            Msg::File {
                path,
                content,
                line,
                marks,
                root,
            } => {
                self.open_file(path, content, line, marks, root);
                Vec::new()
            }
            Msg::Notice(text) => {
                self.notice = Some(text);
                Vec::new()
            }
            Msg::Update(status) => {
                self.update = Some(status);
                Vec::new()
            }
            Msg::Start(start) => self.start_at(start),
            Msg::Explained { key, result } => {
                self.on_explained(key, result);
                Vec::new()
            }
            Msg::Answered { key, result } => {
                self.on_answered(key, result);
                Vec::new()
            }
            Msg::Fixed { id, result } => self.on_fixed(id, result),
            Msg::Removed { removed, error } => {
                let effects = self.request_branches();
                self.notice =
                    Some(error.unwrap_or_else(|| format!("Removed {}", removed.join(" and "))));
                effects
            }
        }
    }

    fn on_commits(&mut self, result: Result<Vec<Entry>, String>) -> Vec<Effect> {
        let follow_up = self.feed.commits_load.finish();
        match result {
            Ok(commits) => {
                let selected = self.selected_entry_id();
                if self.feed.loaded {
                    for c in &commits {
                        if !self.feed.known.contains(&c.id) {
                            self.feed.fresh.insert(c.id.clone());
                        }
                    }
                }
                self.feed.known.extend(commits.iter().map(|c| c.id.clone()));
                self.feed.commits = commits;
                self.feed.loaded = true;
                self.feed.error = None;
                self.reselect(selected.as_deref());
                self.recompute_hotspots();
            }
            Err(e) => self.feed.error = Some(e),
        }
        if follow_up {
            vec![Effect::LoadCommits]
        } else {
            Vec::new()
        }
    }

    fn on_worktree(&mut self, result: Result<Option<Entry>, String>) -> Vec<Effect> {
        let follow_up = self.feed.worktree_load.finish();
        let mut effects = Vec::new();
        match result {
            Ok(worktree) => {
                let selected = self.selected_entry_id();
                self.feed.worktree = worktree;
                self.reselect(selected.as_deref());
                self.recompute_hotspots();
                // The diff stays live under views stacked on it (an
                // explanation), not only while it is on top, and empties
                // once the checkout is clean (a fix that undid the change).
                if let Some(d) = self.views.iter().rev().find_map(View::as_detail)
                    && d.entry.is_own_worktree()
                {
                    let entry = self
                        .feed
                        .worktree
                        .clone()
                        .unwrap_or_else(|| d.entry.without_files());
                    effects.push(Effect::LoadDiff {
                        entry: Box::new(entry),
                        refresh: true,
                        file: None,
                    });
                }
            }
            Err(e) => self.feed.error = Some(e),
        }
        if follow_up {
            effects.push(Effect::LoadWorktree);
        }
        effects
    }

    fn on_changed(&mut self, changes: &BTreeSet<Change>) -> Vec<Effect> {
        let mut effects = Vec::new();
        if changes.contains(&Change::Refs) {
            effects.extend(self.request_commits());
        }
        effects.extend(self.request_worktree());
        if self.branches.loaded {
            effects.extend(self.request_branches());
        }
        if changes.contains(&Change::Workdir) && self.search.files.is_some() {
            self.search.files_stale = true;
            if self.tab == Tab::Search && self.search.mode == SearchMode::Files {
                effects.extend(self.request_files());
            }
        }
        effects
    }

    fn selected_entry_id(&self) -> Option<String> {
        self.feed
            .visible(self.depth)
            .get(self.feed.pos.selected)
            .map(|e| e.id.clone())
    }

    fn reselect(&mut self, id: Option<&str>) {
        if let Some(id) = id
            && let Some(i) = self
                .feed
                .visible(self.depth)
                .iter()
                .position(|e| e.id == id)
        {
            self.feed.pos.selected = i;
        }
    }

    fn recompute_hotspots(&mut self) {
        let entries: Vec<Entry> = self
            .feed
            .worktree
            .iter()
            .chain(&self.feed.commits)
            .cloned()
            .collect();
        self.hotspots.spots = hotspots::compute(
            &entries,
            self.now,
            self.hotspots.window,
            self.hotspots.grouping,
            self.depth,
        );
        let len = self.hotspots.spots.len();
        self.hotspots.pos.move_by(0, len);
    }

    fn refresh_file_hits(&mut self) {
        if let Some(files) = &self.search.files {
            self.search.file_hits = search::fuzzy_files(files, &self.search.query, FILE_RESULTS);
        }
        self.search.pos = ListPos::default();
    }

    /// Opens the diff of `entry` on `file` if it has that file, else the first.
    fn open_detail(&mut self, entry: Entry, files: Vec<FileDiff>, file: Option<&str>) {
        let selected = file
            .and_then(|path| files.iter().position(|f| f.stat.touches(path)))
            .unwrap_or(0);
        let rendered = files
            .get(selected)
            .map(|f| build_diff(f, self.max_highlight))
            .unwrap_or_default();
        self.views.push(View::Detail(Box::new(DetailView {
            entry,
            files,
            pos: ListPos {
                selected,
                ..ListPos::default()
            },
            focus: Focus::Files,
            scroll: 0,
            cursor: 0,
            hscroll: 0,
            height: 0,
            rendered,
            selection: None,
            filter: String::new(),
            filter_editing: false,
        })));
    }

    /// Replaces the diff of the open detail view if it still shows `entry`.
    fn refresh_detail(&mut self, entry: Entry, files: Vec<FileDiff>) {
        let max = self.max_highlight;
        if let Some(d) = self.views.iter_mut().rev().find_map(View::as_detail_mut)
            && d.entry.id == entry.id
        {
            let current = d.files.get(d.pos.selected).cloned();
            d.pos.selected = current
                .as_ref()
                .and_then(|c| files.iter().position(|f| f.stat.path == c.stat.path))
                .unwrap_or(0)
                .min(files.len().saturating_sub(1));
            // Selected rows only mean the same lines in the same diff.
            if files.get(d.pos.selected) != current.as_ref() {
                d.selection = None;
            }
            d.files = files;
            d.rendered = d
                .files
                .get(d.pos.selected)
                .map(|f| build_diff(f, max))
                .unwrap_or_default();
            d.scroll = d.scroll.min(max_scroll(d.rendered.len(), d.height));
            d.cursor = d.cursor.min(d.rendered.len().saturating_sub(1));
            d.entry = entry;
            d.reveal();
        }
    }

    fn open_file(
        &mut self,
        path: String,
        content: Result<String, String>,
        line: u32,
        marks: BTreeSet<u32>,
        root: Option<PathBuf>,
    ) {
        let h = Highlighter::global();
        let (lines, text, notice, language) = match content {
            Ok(content) => {
                let raw: Vec<&str> = content.lines().collect();
                let language = h.language(&path, raw.first().copied()).to_owned();
                let lines = h.highlight(&path, &raw, self.max_highlight);
                let text = raw.into_iter().map(str::to_owned).collect();
                (lines, text, None, language)
            }
            Err(notice) => (Vec::new(), Vec::new(), Some(notice), String::new()),
        };
        let target = line.max(1);
        let scroll = usize::try_from(target).unwrap_or(0).saturating_sub(4);
        // The cursor starts on the line the file was opened at.
        let cursor = usize::try_from(target - 1)
            .unwrap_or(0)
            .min(lines.len().saturating_sub(1));
        self.views.push(View::File(Box::new(FileView {
            path,
            language,
            lines,
            text,
            target,
            marks,
            scroll,
            cursor,
            selection: None,
            hscroll: 0,
            height: 0,
            notice,
            root,
        })));
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return vec![Effect::Quit];
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        if self.branches.confirm.is_some() {
            let confirmed = matches!(key.code, KeyCode::Char('y' | 'Y'));
            return self.answer_remove(confirmed);
        }
        self.notice = None;
        if self.fix.draft.is_some() {
            return self.on_draft_key(&key);
        }
        if self.ask.draft.is_some() {
            return self.on_ask_draft_key(&key);
        }
        if self.branches.collisions.is_some() {
            return self.on_collisions_key(&key);
        }
        if matches!(self.views.last(), Some(View::Explain(v)) if v.question.is_some()) {
            return self.on_question_key(&key);
        }
        if self.agents.picker.is_some() {
            return self.on_picker_key(&key);
        }
        // Everywhere but where text is typed (search, the name filters).
        let typing = match self.views.last() {
            Some(View::Detail(d)) => d.filter_editing,
            Some(_) => false,
            None => {
                self.tab == Tab::Search
                    || (self.tab == Tab::Branches && self.branches.filter_editing)
            }
        };
        if key.code == KeyCode::Char('M') && !typing {
            return self.open_picker();
        }
        if !self.views.is_empty() {
            return self.on_view_key(&key);
        }
        if self.tab == Tab::Branches && self.branches.filter_editing {
            return self.on_branch_filter_key(&key);
        }
        if self.tab == Tab::Search {
            return self.on_search_key(&key);
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.tab = Tab::Feed,
            KeyCode::Char('2') => self.tab = Tab::Hotspots,
            KeyCode::Char('/') if self.tab == Tab::Branches => self.branches.filter_editing = true,
            KeyCode::Char('3' | '/') => return self.enter_search(),
            KeyCode::Char('4') => return self.enter_branches(),
            KeyCode::Tab => return self.switch_tab(self.tab.next()),
            KeyCode::BackTab => return self.switch_tab(self.tab.prev()),
            KeyCode::Char('r') => {
                let mut effects = self.request_commits();
                effects.extend(self.request_worktree());
                if self.tab == Tab::Branches {
                    effects.extend(self.request_branches());
                    effects.extend(self.request_pulls());
                }
                return effects;
            }
            _ => match self.tab {
                Tab::Feed => return self.on_feed_key(&key),
                Tab::Hotspots => return self.on_hotspots_key(&key),
                Tab::Branches => return self.on_branches_key(&key),
                Tab::Search => {}
            },
        }
        Vec::new()
    }

    fn switch_tab(&mut self, tab: Tab) -> Vec<Effect> {
        match tab {
            Tab::Search => self.enter_search(),
            Tab::Branches => self.enter_branches(),
            Tab::Feed | Tab::Hotspots => {
                self.tab = tab;
                Vec::new()
            }
        }
    }

    fn enter_search(&mut self) -> Vec<Effect> {
        if self.tab != Tab::Search {
            self.previous_tab = self.tab;
        }
        self.tab = Tab::Search;
        if self.search.mode == SearchMode::Files
            && (self.search.files.is_none() || self.search.files_stale)
        {
            self.request_files()
        } else {
            Vec::new()
        }
    }

    fn on_feed_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let len = self.feed.visible(self.depth).len();
        if self.feed.pos.navigate(key, len) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') => {
                let entry = self
                    .feed
                    .visible(self.depth)
                    .get(self.feed.pos.selected)
                    .map(|e| (*e).clone());
                if let Some(entry) = entry {
                    self.feed.fresh.remove(&entry.id);
                    return vec![Effect::LoadDiff {
                        entry: Box::new(entry),
                        refresh: false,
                        file: None,
                    }];
                }
            }
            KeyCode::Esc => {
                if self.feed.filter.take().is_some() {
                    self.feed.pos = ListPos::default();
                }
            }
            KeyCode::Char('n') => {
                let visible = self.feed.visible(self.depth);
                let start = (self.feed.pos.selected + 1).min(visible.len());
                // Search after the selection, then wrap around to the top.
                if let Some(i) = (start..visible.len())
                    .chain(0..start)
                    .find(|&i| self.feed.fresh.contains(&visible[i].id))
                {
                    self.feed.pos.selected = i;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_hotspots_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let len = self.hotspots.spots.len();
        if self.hotspots.pos.navigate(key, len) {
            return Vec::new();
        }
        let selected = self
            .hotspots
            .spots
            .get(self.hotspots.pos.selected)
            .map(|s| s.key.clone());
        let is_file = self.hotspots.grouping == Grouping::Files;
        match key.code {
            KeyCode::Char('w') => {
                self.hotspots.window = self.hotspots.window.next();
                self.recompute_hotspots();
            }
            KeyCode::Char('m') => {
                self.hotspots.grouping = self.hotspots.grouping.toggle();
                self.hotspots.pos = ListPos::default();
                self.recompute_hotspots();
            }
            KeyCode::Enter => {
                if let Some(key) = selected {
                    self.feed.filter = Some(Filter {
                        key,
                        grouping: self.hotspots.grouping,
                    });
                    self.feed.pos = ListPos::default();
                    self.tab = Tab::Feed;
                }
            }
            KeyCode::Char('o' | 'e') if !is_file => {
                self.notice =
                    Some("Modules can't be opened; press m to switch to files".to_owned());
            }
            KeyCode::Char('o') => {
                if let Some(path) = selected {
                    return vec![Effect::OpenFile {
                        path,
                        line: 1,
                        marks: BTreeSet::new(),
                        root: None,
                    }];
                }
            }
            KeyCode::Char('e') => {
                if let Some(path) = selected {
                    return vec![Effect::Edit {
                        path,
                        line: 1,
                        root: None,
                    }];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_search_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.search.len();
        let nav = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) || (ctrl && matches!(key.code, KeyCode::Char('n' | 'p' | 'j' | 'k' | 'd')));
        if nav && self.search.pos.navigate(key, len) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => {
                self.tab = self.previous_tab;
                Vec::new()
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.search.mode = match self.search.mode {
                    SearchMode::Files => SearchMode::Text,
                    SearchMode::Text => SearchMode::Files,
                };
                self.query_changed()
            }
            KeyCode::Enter => self
                .search
                .selected()
                .map(|(path, line)| {
                    vec![Effect::OpenFile {
                        path,
                        line,
                        marks: BTreeSet::new(),
                        root: None,
                    }]
                })
                .unwrap_or_default(),
            KeyCode::Char('e') if ctrl => self
                .search
                .selected()
                .map(|(path, line)| {
                    vec![Effect::Edit {
                        path,
                        line,
                        root: None,
                    }]
                })
                .unwrap_or_default(),
            KeyCode::Char('u') if ctrl => {
                self.search.query.clear();
                self.query_changed()
            }
            KeyCode::Char('w') if ctrl => {
                delete_word(&mut self.search.query, |c| c.is_whitespace() || c == '/');
                self.query_changed()
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.query_changed()
            }
            KeyCode::Char(c) if !ctrl => {
                self.search.query.push(c);
                self.query_changed()
            }
            _ => Vec::new(),
        }
    }

    fn query_changed(&mut self) -> Vec<Effect> {
        self.search.pos = ListPos::default();
        match self.search.mode {
            SearchMode::Files => {
                if self.search.files.is_none() || self.search.files_stale {
                    self.request_files()
                } else {
                    self.refresh_file_hits();
                    Vec::new()
                }
            }
            SearchMode::Text => {
                self.search.generation += 1;
                self.search.error = None;
                if self.search.query.is_empty() {
                    self.search.text = TextResults::default();
                    self.search.text_running = false;
                    Vec::new()
                } else {
                    self.search.text_running = true;
                    vec![Effect::SearchText {
                        query: self.search.query.clone(),
                        generation: self.search.generation,
                    }]
                }
            }
        }
    }

    /// What `a` and `f` in the file viewer act on: the change whose diff
    /// the file was opened from (`⏎` there), right under the viewer, and
    /// the lines selected. Says why not when the file wasn't opened from a
    /// diff (search, hotspots): there is no change to ask about.
    fn file_pick(&mut self) -> Option<FilePick> {
        let pick = match self.views.as_slice() {
            [.., View::Detail(d), View::File(f)] => Some(FilePick {
                entry: d.entry.clone(),
                file: d.files.iter().find(|x| x.stat.path == f.path).cloned(),
                files: d.files.clone(),
                path: f.path.clone(),
                lines: selected_file_lines(f),
            }),
            _ => None,
        };
        if pick.is_none() {
            self.notice =
                Some("Open the file from a change's diff (⏎) to ask or fix about it".to_owned());
        }
        pick
    }

    fn on_view_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let max = self.max_highlight;
        if let Some(View::Detail(d)) = self.views.last_mut()
            && d.filter_editing
        {
            return detail_filter_key(d, key, max);
        }
        if key.code == KeyCode::Esc
            && let Some(View::Detail(d)) = self.views.last_mut()
            && d.selection.is_none()
            && !d.filter.is_empty()
        {
            // Back to every file before leaving the diff.
            d.filter.clear();
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            let selection = match self.views.last_mut() {
                Some(View::Detail(d)) => Some(&mut d.selection),
                Some(View::File(f)) => Some(&mut f.selection),
                _ => None,
            };
            if selection.and_then(Option::take).is_some() {
                return Vec::new();
            }
        }
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace
        ) {
            if matches!(self.views.last(), Some(View::Explain(_))) {
                self.close_explain();
            } else if matches!(self.views.last(), Some(View::Answer(_))) {
                self.close_answers();
            } else {
                self.views.pop();
            }
            return Vec::new();
        }
        if key.code == KeyCode::Char('?') {
            self.help = true;
            return Vec::new();
        }
        match self.views.last_mut() {
            Some(View::Detail(_)) if key.code == KeyCode::Char('x') => self.open_explain(),
            Some(View::Detail(_)) if key.code == KeyCode::Char('f') => self.draft_fix_from_detail(),
            Some(View::Detail(_)) if key.code == KeyCode::Char('a') => self.draft_ask_from_detail(),
            Some(View::Detail(_)) if key.code == KeyCode::Char('A') => self.reopen_answers(),
            Some(View::File(_)) if key.code == KeyCode::Char('A') => {
                // Only a file opened from a diff has answers to bring back.
                if self.file_pick().is_some() {
                    self.reopen_answers()
                } else {
                    Vec::new()
                }
            }
            Some(View::File(_)) if key.code == KeyCode::Char('a') => self.draft_ask_from_file(),
            Some(View::File(_)) if key.code == KeyCode::Char('f') => self.draft_fix_from_file(),
            Some(View::Explain(_)) if key.code == KeyCode::Char('f') => {
                self.draft_fix_from_explain()
            }
            Some(View::Detail(d)) => detail_key(d, key, max),
            Some(View::File(f)) => file_key(f, key),
            Some(View::Explain(_)) => self.on_explain_key(key),
            Some(View::Answer(_)) => self.on_answer_key(key),
            None => Vec::new(),
        }
    }
}

/// Deletes the last word of `text` (`ctrl-w`): trailing whitespace, then
/// back to just after the last character `ends_word` matches.
fn delete_word(text: &mut String, ends_word: impl Fn(char) -> bool) {
    let trimmed = text.trim_end().len();
    let cut = text[..trimmed]
        .char_indices()
        .rev()
        .find(|&(_, c)| ends_word(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    text.truncate(cut);
}

/// Furthest scroll position: the last line sits at the bottom of the viewport.
fn max_scroll(len: usize, height: usize) -> usize {
    len.saturating_sub(height.max(1))
}

fn half_page(height: usize) -> usize {
    (height / 2).max(1)
}

fn scroll_by(value: usize, delta: isize, max: usize) -> usize {
    value.saturating_add_signed(delta).min(max)
}

/// The scroll that keeps row `cursor` in a view of `height` rows that
/// starts at `scroll`.
const fn follow(cursor: usize, scroll: usize, height: usize) -> usize {
    if cursor < scroll {
        cursor
    } else if height > 0 && cursor >= scroll + height {
        cursor + 1 - height
    } else {
        scroll
    }
}

/// A view with a cursor over `len` rows, `height` of them visible from
/// `scroll`: the diff pane or the file viewer.
struct CursorView<'a> {
    len: usize,
    height: usize,
    scroll: &'a mut usize,
    cursor: &'a mut usize,
    selection: &'a mut Option<Selection>,
}

impl CursorView<'_> {
    /// Puts the cursor on `row` (within the rows), scrolls to keep it in
    /// view, and has an open selection follow it.
    fn move_to(&mut self, row: usize) {
        *self.cursor = row.min(self.len.saturating_sub(1));
        *self.scroll = follow(*self.cursor, *self.scroll, self.height);
        Selection::follow(self.selection, *self.cursor);
    }

    /// The view scrolled: brings the cursor back into it.
    fn keep_in_view(&mut self) {
        let bottom = (*self.scroll + self.height.max(1)).saturating_sub(1);
        let row = (*self.cursor).clamp(*self.scroll, bottom.max(*self.scroll));
        self.move_to(row);
    }

    /// Cursor keys and `v`; returns whether `key` was one. Page keys move
    /// the view by as much as the cursor.
    fn on_key(&mut self, key: &KeyEvent) -> bool {
        if self.len == 0 {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = isize::try_from(half_page(self.height)).unwrap_or(1);
        let by = |delta: isize| (delta, self.cursor.saturating_add_signed(delta));
        let (shift, row) = match key.code {
            KeyCode::Char('v') if !ctrl => {
                *self.selection = Some(Selection::toggled(*self.selection, *self.cursor));
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => (0, by(1).1),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => (0, by(-1).1),
            KeyCode::Char('d') if ctrl => by(half),
            KeyCode::Char('u') if ctrl => by(-half),
            KeyCode::PageDown => by(half * 2),
            KeyCode::PageUp => by(-half * 2),
            KeyCode::Char('g') | KeyCode::Home => (0, 0),
            KeyCode::Char('G') | KeyCode::End => (0, self.len - 1),
            _ => return false,
        };
        *self.scroll = scroll_by(*self.scroll, shift, max_scroll(self.len, self.height));
        self.move_to(row);
        true
    }
}

impl DetailView {
    /// The diff pane as a view with a cursor.
    fn pane(&mut self) -> CursorView<'_> {
        CursorView {
            len: self.rendered.len(),
            height: self.height,
            scroll: &mut self.scroll,
            cursor: &mut self.cursor,
            selection: &mut self.selection,
        }
    }
}

impl FileView {
    /// The file as a view with a cursor.
    fn pane(&mut self) -> CursorView<'_> {
        CursorView {
            len: self.lines.len(),
            height: self.height,
            scroll: &mut self.scroll,
            cursor: &mut self.cursor,
            selection: &mut self.selection,
        }
    }
}

fn detail_key(d: &mut DetailView, key: &KeyEvent, max_highlight: usize) -> Vec<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let max_scroll = max_scroll(d.rendered.len(), d.height);
    let half = isize::try_from(half_page(d.height)).unwrap_or(1);
    let previous = d.pos.selected;
    if key.code == KeyCode::Char('v') && !d.rendered.is_empty() {
        // `v` from the file list selects in the diff.
        d.focus = Focus::Diff;
    }
    if d.focus == Focus::Diff && d.pane().on_key(key) {
        return Vec::new();
    }
    // Scrolling the view (from either pane) takes the cursor along.
    let scrolled = match key.code {
        KeyCode::Char('J') => Some(1),
        KeyCode::Char('K') => Some(-1),
        KeyCode::Char('d') if ctrl => Some(half),
        KeyCode::Char('u') if ctrl => Some(-half),
        KeyCode::PageDown => Some(half * 2),
        KeyCode::PageUp => Some(-half * 2),
        _ => None,
    };
    if let Some(delta) = scrolled {
        d.scroll = scroll_by(d.scroll, delta, max_scroll);
        d.pane().keep_in_view();
        return Vec::new();
    }
    match (key.code, d.focus) {
        (KeyCode::Tab | KeyCode::BackTab, _) => {
            d.focus = match d.focus {
                Focus::Files => Focus::Diff,
                Focus::Diff => Focus::Files,
            };
        }
        (KeyCode::Char('h'), _) => d.focus = Focus::Files,
        (KeyCode::Char('l'), _) => d.focus = Focus::Diff,
        (KeyCode::Left, _) => d.hscroll = d.hscroll.saturating_sub(H_SCROLL_STEP),
        (KeyCode::Right, _) => d.hscroll += H_SCROLL_STEP,
        // Hunk jumps move the cursor too, so an open selection grows to
        // the hunk; going on to another file drops it (below).
        (KeyCode::Char('n' | ']'), _) => {
            if let Some(&next) = d.rendered.hunk_starts.iter().find(|&&s| s > d.cursor) {
                d.scroll = next.min(max_scroll);
                d.pane().move_to(next);
            } else if let Some(next) = d.next_shown(true) {
                d.pos.selected = next;
            }
        }
        (KeyCode::Char('N' | '['), _) => {
            if let Some(&prev) = d.rendered.hunk_starts.iter().rev().find(|&&s| s < d.cursor) {
                d.scroll = prev.min(max_scroll);
                d.pane().move_to(prev);
            } else if let Some(prev) = d.next_shown(false).filter(|_| d.cursor == 0) {
                d.pos.selected = prev;
            } else {
                d.scroll = 0;
                d.pane().move_to(0);
            }
        }
        (KeyCode::Enter | KeyCode::Char('o'), _) => {
            let root = d.entry.root.clone();
            return open_from_detail(d).map_or_else(Vec::new, |(path, line, marks)| {
                vec![Effect::OpenFile {
                    path,
                    line,
                    marks,
                    root,
                }]
            });
        }
        (KeyCode::Char('e'), _) => {
            let root = d.entry.root.clone();
            return open_from_detail(d).map_or_else(Vec::new, |(path, line, _)| {
                vec![Effect::Edit { path, line, root }]
            });
        }
        (KeyCode::Char('/'), _) => {
            d.filter_editing = true;
            d.focus = Focus::Files;
        }
        (_, Focus::Files) => {
            d.navigate_files(key);
        }
        _ => {}
    }
    if d.focus == Focus::Files {
        // A selection is of the diff pane's lines; the file list's `a` and
        // `f` are about the whole file.
        d.selection = None;
    }
    file_changed(d, previous, max_highlight);
    Vec::new()
}

/// Shows the selected file's diff from its top if it isn't file `previous`.
fn file_changed(d: &mut DetailView, previous: usize, max_highlight: usize) {
    if d.pos.selected != previous {
        d.scroll = 0;
        d.cursor = 0;
        d.hscroll = 0;
        d.selection = None;
        d.rendered = d
            .files
            .get(d.pos.selected)
            .map(|f| build_diff(f, max_highlight))
            .unwrap_or_default();
    }
}

/// Keys while the file filter (`/`) is typed: characters narrow the file
/// list and select its first match, list moves still act, `⏎` goes to the
/// selected file's diff and `esc` stops typing, keeping the filter.
fn detail_filter_key(d: &mut DetailView, key: &KeyEvent, max_highlight: usize) -> Vec<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let previous = d.pos.selected;
    let nav = matches!(
        key.code,
        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
    ) || (ctrl && matches!(key.code, KeyCode::Char('n' | 'p' | 'j' | 'k' | 'd')));
    if nav {
        d.navigate_files(key);
    } else {
        match key.code {
            // Leaving with nothing matched clears the filter (`reveal`).
            KeyCode::Esc => {
                d.filter_editing = false;
                d.reveal();
            }
            KeyCode::Enter => {
                d.filter_editing = false;
                if !d.shown().is_empty() {
                    d.focus = Focus::Diff;
                }
                d.reveal();
            }
            KeyCode::Backspace => {
                d.filter.pop();
            }
            KeyCode::Char('u') if ctrl => d.filter.clear(),
            KeyCode::Char('w') if ctrl => {
                delete_word(&mut d.filter, |c| c == '/' || c.is_whitespace());
            }
            KeyCode::Char(c) if !ctrl => d.filter.push(c),
            _ => return Vec::new(),
        }
        if !matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            d.pos.offset = 0;
            if let Some(&first) = d.shown().first() {
                d.pos.selected = first;
            }
        }
    }
    file_changed(d, previous, max_highlight);
    Vec::new()
}

/// What `a` and `f` in a diff are about: the rows selected with `v`, as a
/// place and those lines in diff form, or `None` when nothing is selected
/// (or the selection holds nothing of the diff), for the whole file.
pub(super) fn selected_lines(d: &DetailView) -> Option<(String, String)> {
    let selection = d.selection.filter(|_| d.focus == Focus::Diff)?;
    let file = d.files.get(d.pos.selected)?;
    let (first, last) = selection.rows();
    let picks: Vec<(usize, Option<usize>)> = d
        .rendered
        .sources
        .get(first..=last.min(d.rendered.sources.len().saturating_sub(1)))?
        .iter()
        .filter_map(|s| match *s {
            crate::ui::detail::Source::Header(h) => Some((h, None)),
            crate::ui::detail::Source::Line(h, i) => Some((h, Some(i))),
            crate::ui::detail::Source::None => None,
        })
        .collect();
    crate::fix::picked(file, &picks)
}

/// Path, line and added-line marks for opening the selected file of a detail view.
fn open_from_detail(d: &DetailView) -> Option<(String, u32, BTreeSet<u32>)> {
    let file = d.files.get(d.pos.selected)?;
    let line = if d.focus == Focus::Diff {
        d.rendered
            .new_line_at(d.cursor)
            .unwrap_or_else(|| file.first_changed_line())
    } else {
        file.first_changed_line()
    };
    let marks = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == LineKind::Added)
        .filter_map(|l| l.new_no)
        .collect();
    Some((file.stat.path.clone(), line, marks))
}

fn file_key(f: &mut FileView, key: &KeyEvent) -> Vec<Effect> {
    // `space` pages down, as in a pager.
    let key = if key.code == KeyCode::Char(' ') {
        &KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
    } else {
        key
    };
    if f.pane().on_key(key) {
        return Vec::new();
    }
    let max_scroll = max_scroll(f.lines.len(), f.height);
    let cursor_line = u32::try_from(f.cursor + 1).unwrap_or(u32::MAX);
    // A change's first line, put a few lines below the top.
    let jump = |f: &mut FileView, line: u32| {
        let row = usize::try_from(line).unwrap_or(0).saturating_sub(1);
        f.scroll = row.saturating_sub(4).min(max_scroll);
        f.pane().move_to(row);
    };
    match key.code {
        KeyCode::Left | KeyCode::Char('h') => f.hscroll = f.hscroll.saturating_sub(H_SCROLL_STEP),
        KeyCode::Right | KeyCode::Char('l') => f.hscroll += H_SCROLL_STEP,
        KeyCode::Char('n' | ']') => {
            if let Some(&next) = f
                .marks
                .range(cursor_line + 1..)
                .find(|&&m| !f.marks.contains(&(m - 1)))
            {
                jump(f, next);
            }
        }
        KeyCode::Char('N' | '[') => {
            if let Some(&prev) = f
                .marks
                .range(..cursor_line)
                .rev()
                .find(|&&m| !f.marks.contains(&(m - 1)))
            {
                jump(f, prev);
            }
        }
        KeyCode::Char('e') => {
            return vec![Effect::Edit {
                path: f.path.clone(),
                line: cursor_line,
                root: f.root.clone(),
            }];
        }
        _ => {}
    }
    Vec::new()
}

/// What `a` and `f` in the file viewer are about: the lines selected with
/// `v`, as a place and those lines (see [`crate::fix::file_lines`]), or
/// `None` when nothing is selected, for the whole file.
pub(super) fn selected_file_lines(f: &FileView) -> Option<(String, String)> {
    let (first, last) = f.selection?.rows();
    let picked = f
        .text
        .get(first..=last.min(f.text.len().checked_sub(1)?))?
        .to_vec();
    let first = u32::try_from(first + 1).ok()?;
    Some(crate::fix::file_lines(&f.path, first, &picked))
}

#[cfg(test)]
mod tests;
