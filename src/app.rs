//! Application state and input handling.
//!
//! [`App::update`] is a pure state transition: it consumes a [`Msg`] and
//! returns the [`Effect`]s (I/O) the runtime should perform. This keeps all
//! interaction logic testable without a terminal or repository.

mod branches;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub use branches::{BranchesState, PullStatus, Refresh, Row, RowFilter, pull_entry};

use crate::git::RepoInfo;
use crate::git::branches::Branches;
use crate::git::diff::{FileDiff, LineKind};
use crate::git::feed::{Entry, WORKTREE_ID};
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
    /// Horizontal scroll in columns.
    pub hscroll: usize,
    /// Visible diff lines in the last frame.
    pub height: usize,
    /// Rendered lines of the selected file.
    pub rendered: RenderedDiff,
}

/// The file viewer.
#[derive(Debug)]
pub struct FileView {
    /// Repository-relative path.
    pub path: String,
    /// Detected language name.
    pub language: String,
    /// Highlighted lines.
    pub lines: Vec<Vec<Fragment>>,
    /// Line (1-based) to emphasise.
    pub target: u32,
    /// Lines (1-based) added by the change the file was opened from.
    pub marks: BTreeSet<u32>,
    /// First visible line (0-based).
    pub scroll: usize,
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
    /// Stacked views (detail, file); the last one is on screen.
    pub views: Vec<View>,
    /// Whether the help overlay is shown.
    pub help: bool,
    /// Live-watch status.
    pub watch: WatchStatus,
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
    /// effects that kick off loading.
    #[must_use]
    pub fn new(
        repo: RepoInfo,
        depth: usize,
        max_highlight: usize,
        now: i64,
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
            views: Vec::new(),
            help: false,
            watch: WatchStatus::Live,
            notice: None,
            tick: 0,
            now,
            depth,
            max_highlight,
        };
        let mut effects = app.request_commits();
        effects.extend(app.request_worktree());
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
            } => {
                match result {
                    Ok(files) if refresh => self.refresh_detail(*entry, files),
                    Ok(files) => self.open_detail(*entry, files),
                    Err(e) => self.notice = Some(format!("Could not load diff: {e}")),
                }
                Vec::new()
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
                if let Some(View::Detail(d)) = self.views.last()
                    && d.entry.id == WORKTREE_ID
                    && let Some(wt) = &self.feed.worktree
                {
                    effects.push(Effect::LoadDiff {
                        entry: Box::new(wt.clone()),
                        refresh: true,
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

    fn open_detail(&mut self, entry: Entry, files: Vec<FileDiff>) {
        let rendered = files
            .first()
            .map(|f| build_diff(f, self.max_highlight))
            .unwrap_or_default();
        self.views.push(View::Detail(Box::new(DetailView {
            entry,
            files,
            pos: ListPos::default(),
            focus: Focus::Files,
            scroll: 0,
            hscroll: 0,
            height: 0,
            rendered,
        })));
    }

    /// Replaces the diff of the open detail view if it still shows `entry`.
    fn refresh_detail(&mut self, entry: Entry, files: Vec<FileDiff>) {
        let max = self.max_highlight;
        if let Some(View::Detail(d)) = self.views.last_mut()
            && d.entry.id == entry.id
        {
            let current = d.files.get(d.pos.selected).map(|f| f.stat.path.clone());
            d.pos.selected = current
                .and_then(|p| files.iter().position(|f| f.stat.path == p))
                .unwrap_or(0)
                .min(files.len().saturating_sub(1));
            d.files = files;
            d.rendered = d
                .files
                .get(d.pos.selected)
                .map(|f| build_diff(f, max))
                .unwrap_or_default();
            d.scroll = d.scroll.min(max_scroll(d.rendered.len(), d.height));
            d.entry = entry;
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
        let (lines, notice, language) = match content {
            Ok(text) => {
                let raw: Vec<&str> = text.lines().collect();
                let language = h.language(&path, raw.first().copied()).to_owned();
                (h.highlight(&path, &raw, self.max_highlight), None, language)
            }
            Err(notice) => (Vec::new(), Some(notice), String::new()),
        };
        let target = line.max(1);
        let scroll = usize::try_from(target).unwrap_or(0).saturating_sub(4);
        self.views.push(View::File(Box::new(FileView {
            path,
            language,
            lines,
            target,
            marks,
            scroll,
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
        self.notice = None;
        if !self.views.is_empty() {
            return self.on_view_key(&key);
        }
        if self.tab == Tab::Search {
            return self.on_search_key(&key);
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.tab = Tab::Feed,
            KeyCode::Char('2') => self.tab = Tab::Hotspots,
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
                let trimmed = self.search.query.trim_end().len();
                let cut = self.search.query[..trimmed]
                    .rfind(|c: char| c.is_whitespace() || c == '/')
                    .map_or(0, |i| i + 1);
                self.search.query.truncate(cut);
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

    fn on_view_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace
        ) {
            self.views.pop();
            return Vec::new();
        }
        if key.code == KeyCode::Char('?') {
            self.help = true;
            return Vec::new();
        }
        let max = self.max_highlight;
        match self.views.last_mut() {
            Some(View::Detail(d)) => detail_key(d, key, max),
            Some(View::File(f)) => file_key(f, key),
            None => Vec::new(),
        }
    }
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

fn detail_key(d: &mut DetailView, key: &KeyEvent, max_highlight: usize) -> Vec<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let max_scroll = max_scroll(d.rendered.len(), d.height);
    let half = isize::try_from(half_page(d.height)).unwrap_or(1);
    let previous = d.pos.selected;
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
        (KeyCode::Char('J'), _) | (KeyCode::Char('j') | KeyCode::Down, Focus::Diff) => {
            d.scroll = scroll_by(d.scroll, 1, max_scroll);
        }
        (KeyCode::Char('K'), _) | (KeyCode::Char('k') | KeyCode::Up, Focus::Diff) => {
            d.scroll = d.scroll.saturating_sub(1);
        }
        (KeyCode::Char('d'), _) if ctrl => d.scroll = scroll_by(d.scroll, half, max_scroll),
        (KeyCode::PageDown, _) => d.scroll = scroll_by(d.scroll, half * 2, max_scroll),
        (KeyCode::Char('u'), _) if ctrl => d.scroll = scroll_by(d.scroll, -half, max_scroll),
        (KeyCode::PageUp, _) => d.scroll = scroll_by(d.scroll, -half * 2, max_scroll),
        (KeyCode::Char('g') | KeyCode::Home, Focus::Diff) => d.scroll = 0,
        (KeyCode::Char('G') | KeyCode::End, Focus::Diff) => d.scroll = max_scroll,
        (KeyCode::Char('n' | ']'), _) => {
            if let Some(&next) = d.rendered.hunk_starts.iter().find(|&&s| s > d.scroll) {
                d.scroll = next;
            } else if d.pos.selected + 1 < d.files.len() {
                d.pos.selected += 1;
            }
        }
        (KeyCode::Char('N' | '['), _) => {
            if let Some(&prev) = d.rendered.hunk_starts.iter().rev().find(|&&s| s < d.scroll) {
                d.scroll = prev;
            } else if d.scroll == 0 && d.pos.selected > 0 {
                d.pos.selected -= 1;
            } else {
                d.scroll = 0;
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
        (_, Focus::Files) => {
            d.pos.navigate(key, d.files.len());
        }
        _ => {}
    }
    if d.pos.selected != previous {
        d.scroll = 0;
        d.hscroll = 0;
        d.rendered = d
            .files
            .get(d.pos.selected)
            .map(|f| build_diff(f, max_highlight))
            .unwrap_or_default();
    }
    Vec::new()
}

/// Path, line and added-line marks for opening the selected file of a detail view.
fn open_from_detail(d: &DetailView) -> Option<(String, u32, BTreeSet<u32>)> {
    let file = d.files.get(d.pos.selected)?;
    let line = if d.focus == Focus::Diff {
        d.rendered
            .new_line_at(d.scroll)
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
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let max_scroll = max_scroll(f.lines.len(), f.height);
    let half = isize::try_from(half_page(f.height)).unwrap_or(1);
    let top_line = u32::try_from(f.scroll + 1).unwrap_or(u32::MAX);
    match key.code {
        KeyCode::Char('j') | KeyCode::Down if !ctrl => {
            f.scroll = scroll_by(f.scroll, 1, max_scroll);
        }
        KeyCode::Char('k') | KeyCode::Up if !ctrl => f.scroll = f.scroll.saturating_sub(1),
        KeyCode::Char('d') if ctrl => f.scroll = scroll_by(f.scroll, half, max_scroll),
        KeyCode::Char('u') if ctrl => f.scroll = scroll_by(f.scroll, -half, max_scroll),
        KeyCode::PageDown | KeyCode::Char(' ') => {
            f.scroll = scroll_by(f.scroll, half * 2, max_scroll);
        }
        KeyCode::PageUp => f.scroll = scroll_by(f.scroll, -half * 2, max_scroll),
        KeyCode::Char('g') | KeyCode::Home => f.scroll = 0,
        KeyCode::Char('G') | KeyCode::End => f.scroll = max_scroll,
        KeyCode::Left | KeyCode::Char('h') => f.hscroll = f.hscroll.saturating_sub(H_SCROLL_STEP),
        KeyCode::Right | KeyCode::Char('l') => f.hscroll += H_SCROLL_STEP,
        KeyCode::Char('n' | ']') => {
            let current = top_line + 4;
            if let Some(&next) = f
                .marks
                .range(current + 1..)
                .find(|&&m| !f.marks.contains(&(m - 1)))
            {
                f.scroll = usize::try_from(next).unwrap_or(0).saturating_sub(5);
            }
        }
        KeyCode::Char('N' | '[') => {
            let current = top_line + 4;
            if let Some(&prev) = f
                .marks
                .range(..current)
                .rev()
                .find(|&&m| !f.marks.contains(&(m - 1)))
            {
                f.scroll = usize::try_from(prev).unwrap_or(0).saturating_sub(5);
            }
        }
        KeyCode::Char('e') => {
            let target_visible = usize::try_from(f.target)
                .is_ok_and(|t| t > f.scroll && t <= f.scroll + f.height.max(1));
            let line = if target_visible { f.target } else { top_line };
            return vec![Effect::Edit {
                path: f.path.clone(),
                line,
                root: f.root.clone(),
            }];
        }
        _ => {}
    }
    Vec::new()
}

#[cfg(test)]
mod tests;
