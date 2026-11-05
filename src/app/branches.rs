//! The Branches tab: worktrees, branches and open pull requests merged into
//! one list of rows, one per line of work.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, ListPos, Loader, Tab, View};
use crate::git::Target;
use crate::git::branches::{Branch, Branches};
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::git::overlap::{Level, Overlaps};
use crate::github::{ORIGIN, PullRequest, PullsError};
use crate::search::fuzzy_match;
use git2::Oid;

/// How often the Branches tab reloads while on screen, in seconds.
/// `None` turns that reload off; for pull requests it also means GitHub is
/// never contacted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    /// Branches and worktrees.
    pub branches: Option<i64>,
    /// Open pull requests.
    pub pulls: Option<i64>,
}

/// Which rows the Branches tab shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowFilter {
    /// Everything.
    #[default]
    All,
    /// Checkouts only.
    Worktrees,
    /// Rows with a local or remote branch.
    Branches,
    /// Rows with an open pull request.
    Pulls,
}

impl RowFilter {
    /// Display name.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Worktrees => "worktrees",
            Self::Branches => "branches",
            Self::Pulls => "pull requests",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::All => Self::Worktrees,
            Self::Worktrees => Self::Branches,
            Self::Branches => Self::Pulls,
            Self::Pulls => Self::All,
        }
    }

    fn keeps(self, row: &Row<'_>) -> bool {
        match self {
            Self::All => true,
            Self::Worktrees => row.branch.is_some_and(|b| b.worktree.is_some()),
            Self::Branches => row.branch.is_some_and(|b| b.branch.is_some()),
            Self::Pulls => row.pull.is_some(),
        }
    }
}

/// Whether open pull requests can be shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PullStatus {
    /// Network access is off.
    #[default]
    Off,
    /// Not loaded yet.
    Waiting,
    /// Loaded for the named repository.
    Ready(String),
    /// No token: the user needs to log in.
    NoToken,
    /// `origin` is not a GitHub repository.
    Unsupported(String),
    /// The last request failed.
    Failed(String),
}

/// One line of work: a branch or checkout, its pull request, or both.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    /// The local or remote branch, or checkout.
    pub branch: Option<&'a Branch>,
    /// The matching open pull request.
    pub pull: Option<&'a PullRequest>,
}

impl Row<'_> {
    /// Stable identity used to keep the selection across reloads.
    #[must_use]
    pub fn key(&self) -> String {
        match (self.branch, self.pull) {
            (Some(b), _) => format!("b:{}", b.name),
            (None, Some(p)) => format!("p:{}", p.number),
            (None, None) => String::new(),
        }
    }

    /// The name this row goes by: a branch or pull request head. A checkout's
    /// directory name is matched separately, in [`named_like`].
    #[must_use]
    pub fn name(&self) -> &str {
        match (self.branch, self.pull) {
            (Some(b), _) => &b.name,
            (None, Some(p)) => &p.head,
            (None, None) => "",
        }
    }

    /// Latest activity: on the branch, its checkout, or its pull request.
    #[must_use]
    pub fn time(&self) -> i64 {
        let branch = self.branch.map(Branch::time);
        let pull = self.pull.map(|p| p.updated);
        branch.max(pull).unwrap_or(0)
    }

    /// Uncommitted changes in this row's checkout.
    #[must_use]
    pub fn dirty(&self) -> Option<&Entry> {
        self.branch?.worktree.as_ref()?.dirty.as_ref()
    }

    /// What the branch adds since it left the base branch.
    #[must_use]
    pub fn changes(&self) -> Option<&Entry> {
        self.branch?.changes.as_ref()
    }
}

/// One file a line of work collides on, with one other line of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionRow {
    /// The other line of work.
    pub other: String,
    /// Repository-relative path.
    pub path: String,
    /// How seriously they collide on it.
    pub level: Level,
}

/// What the selected line of work collides with, open from the `c` key. It
/// is a snapshot, so rows don't move while a reload lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionList {
    /// The line of work.
    pub name: String,
    /// Its collisions, the most serious first.
    pub rows: Vec<CollisionRow>,
    /// Selection.
    pub pos: ListPos,
}

/// A branch a confirmed removal deletes, snapshotted when it was confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRemoval {
    /// The branch's name.
    pub name: String,
    /// Its tip when confirmed; the deletion aborts if it moved.
    pub tip: Option<Oid>,
}

/// A removal waiting for the `d` key's confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// The linked worktree to remove: its name and path.
    pub worktree: Option<(String, PathBuf)>,
    /// The local branch to delete afterwards.
    pub branch: Option<BranchRemoval>,
    /// Commits the branch has that the base branch lacks, when compared.
    pub unmerged: Option<usize>,
    /// The open pull request on the branch, if any.
    pub pull: Option<u64>,
}

/// Branches tab state.
#[derive(Debug, Default)]
pub struct BranchesState {
    /// Branches and worktrees, as last loaded.
    pub list: Branches,
    /// Open pull requests, as last loaded.
    pub pulls: Vec<PullRequest>,
    /// Whether pull requests can be shown.
    pub pull_status: PullStatus,
    /// Which rows are shown.
    pub filter: RowFilter,
    /// Narrows rows to those whose name fuzzily contains this.
    pub name_filter: String,
    /// Whether `name_filter` is being typed into.
    pub filter_editing: bool,
    /// A removal waiting for the user's yes.
    pub confirm: Option<Removal>,
    /// Selection.
    pub pos: ListPos,
    /// Whether branches loaded at least once.
    pub loaded: bool,
    /// Last branch load error.
    pub error: Option<String>,
    /// Where lines of work collide, as last found.
    pub overlaps: Overlaps,
    /// Why collisions could not be found the last time.
    pub overlap_error: Option<String>,
    /// The open list of collisions.
    pub collisions: Option<CollisionList>,
    /// The change about to open, and the file to select in it.
    focus_file: Option<(String, String)>,
    load: Loader,
    overlap_load: Loader,
    pulls_load: Loader,
    /// When branches were last requested (Unix seconds).
    last_load: Option<i64>,
    /// When pull requests were last requested (Unix seconds).
    last_pulls: Option<i64>,
}

/// The change of `branch` that touches `path`: its uncommitted work first.
fn change_holding(branch: &Branch, path: &str) -> Option<Entry> {
    let dirty = branch.worktree.as_ref().and_then(|w| w.dirty.as_ref());
    dirty
        .into_iter()
        .chain(&branch.changes)
        .find(|e| {
            e.files
                .iter()
                .any(|f| f.path == path || f.old_path.as_deref() == Some(path))
        })
        .cloned()
}

/// Whether a row's branch, checkout directory or pull request name fuzzily
/// contains `query`.
fn named_like(row: &Row<'_>, query: &str) -> bool {
    fuzzy_match(row.name(), query)
        || row
            .branch
            .and_then(|b| b.worktree.as_ref())
            .is_some_and(|w| fuzzy_match(&w.name, query))
}

/// The tip commit a branch's changes were measured at, when there were any.
fn tip_of(entry: Option<&Entry>) -> Option<Oid> {
    match entry?.target {
        Target::Range { head, .. } | Target::Commit(head) => Some(head),
        Target::WorkingTree | Target::PullRequest(_) => None,
    }
}

impl BranchesState {
    /// Rows shown under the current filter: checkouts first, then branches
    /// and pull requests by latest activity. A pull request joins at most one
    /// branch row: a local branch or an `origin` one with its head's name.
    #[must_use]
    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut matched = vec![false; self.pulls.len()];
        let mut checkouts = Vec::new();
        let mut rest = Vec::new();
        for branch in &self.list.branches {
            let pull = branch
                .branch
                .as_deref()
                .filter(|name| !branch.remote || branch.name == format!("{ORIGIN}/{name}"))
                .and_then(|name| {
                    let i = self
                        .pulls
                        .iter()
                        .zip(&matched)
                        .position(|(p, m)| !m && !p.fork && p.head == name)?;
                    matched[i] = true;
                    Some(&self.pulls[i])
                });
            let row = Row {
                branch: Some(branch),
                pull,
            };
            if branch.worktree.is_some() {
                checkouts.push(row);
            } else {
                rest.push(row);
            }
        }
        rest.extend(
            self.pulls
                .iter()
                .zip(matched)
                .filter(|(_, m)| !m)
                .map(|(p, _)| Row {
                    branch: None,
                    pull: Some(p),
                }),
        );
        rest.sort_by_key(|r| std::cmp::Reverse(r.time()));
        checkouts
            .into_iter()
            .chain(rest)
            .filter(|r| self.filter.keeps(r))
            .filter(|r| named_like(r, &self.name_filter))
            .collect()
    }

    /// Whether a load is in flight.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.load.in_flight || self.pulls_load.in_flight
    }

    fn collisions_of(&self, name: &str) -> CollisionList {
        let mut rows: Vec<_> = self
            .overlaps
            .of(name)
            .flat_map(|c| {
                c.files.iter().map(|f| CollisionRow {
                    other: c.other(name).to_owned(),
                    path: f.path.clone(),
                    level: f.level,
                })
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.level));
        CollisionList {
            name: name.to_owned(),
            rows,
            pos: ListPos::default(),
        }
    }

    /// Where to start in the diff of change `id`: the file asked for, if the
    /// diff has it, else the first.
    pub(super) fn take_focus(&mut self, id: &str, files: &[FileDiff]) -> usize {
        self.focus_file
            .take()
            .filter(|(wanted, _)| wanted == id)
            .and_then(|(_, path)| {
                files.iter().position(|f| {
                    f.stat.path == path || f.stat.old_path.as_deref() == Some(path.as_str())
                })
            })
            .unwrap_or(0)
    }

    fn selected_key(&self) -> Option<String> {
        self.rows().get(self.pos.selected).map(Row::key)
    }

    fn reselect(&mut self, key: Option<&str>) {
        if let Some(i) = key.and_then(|k| self.rows().iter().position(|r| r.key() == k)) {
            self.pos.selected = i;
        }
    }
}

/// The entry describing pull request `p`; its files and labels arrive with
/// the diff.
#[must_use]
pub fn pull_entry(p: &PullRequest) -> Entry {
    let mut e = Entry::blank(Target::PullRequest(p.number));
    e.summary.clone_from(&p.title);
    e.author.clone_from(&p.author);
    e.time = p.updated;
    e.added = p.added;
    e.removed = p.removed;
    e
}

impl App {
    /// Sets how often the Branches tab reloads, and whether GitHub is used.
    pub fn set_refresh(&mut self, refresh: Refresh) {
        self.refresh = refresh;
        self.branches.pull_status = if refresh.pulls.is_some() {
            PullStatus::Waiting
        } else {
            PullStatus::Off
        };
    }

    pub(super) fn request_branches(&mut self) -> Vec<Effect> {
        self.branches.last_load = Some(self.now);
        if self.branches.load.request() {
            vec![Effect::LoadBranches]
        } else {
            Vec::new()
        }
    }

    pub(super) fn request_pulls(&mut self) -> Vec<Effect> {
        if self.refresh.pulls.is_none() {
            return Vec::new();
        }
        self.branches.last_pulls = Some(self.now);
        if self.branches.pulls_load.request() {
            vec![Effect::LoadPulls]
        } else {
            Vec::new()
        }
    }

    /// Reloads whatever is due while the Branches tab is on screen.
    pub(super) fn poll_branches(&mut self) -> Vec<Effect> {
        if self.tab != Tab::Branches {
            return Vec::new();
        }
        let now = self.now;
        let due = |last: Option<i64>, every: Option<i64>| match (last, every) {
            (None, _) => true,
            (Some(last), Some(every)) => now - last >= every,
            (Some(_), None) => false,
        };
        let mut effects = Vec::new();
        if due(self.branches.last_load, self.refresh.branches) {
            effects.extend(self.request_branches());
        }
        // Linked worktrees are invisible to the file watcher, so branches keep
        // polling behind an open view; pull requests wait until it closes.
        if self.views.is_empty() && due(self.branches.last_pulls, self.refresh.pulls) {
            effects.extend(self.request_pulls());
        }
        effects
    }

    pub(super) fn on_branches(&mut self, result: Result<Branches, String>) -> Vec<Effect> {
        let follow_up = self.branches.load.finish();
        let mut effects = Vec::new();
        match result {
            Ok(list) => {
                let selected = self.branches.selected_key();
                let stale = list != self.branches.list || self.branches.overlaps.unchecked > 0;
                self.branches.list = list;
                self.branches.loaded = true;
                self.branches.error = None;
                self.branches.reselect(selected.as_deref());
                effects.extend(self.refresh_open_worktree());
                if stale {
                    effects.extend(self.request_overlaps());
                }
            }
            Err(e) => self.branches.error = Some(e),
        }
        if follow_up {
            effects.push(Effect::LoadBranches);
        }
        effects
    }

    /// Starts looking for collisions, unless there is nothing to compare.
    fn request_overlaps(&mut self) -> Option<Effect> {
        if self.branches.list.branches.len() < 2 {
            self.branches.overlaps = Overlaps::default();
            self.branches.overlap_error = None;
            return None;
        }
        self.branches
            .overlap_load
            .request()
            .then(|| self.overlaps_effect())
    }

    fn overlaps_effect(&self) -> Effect {
        Effect::LoadOverlaps(Box::new(self.branches.list.clone()))
    }

    /// Takes in collisions, unless a newer list is already waiting for its own
    /// or too few lines of work remain for them to mean anything.
    pub(super) fn on_overlaps(&mut self, result: Result<Overlaps, String>) -> Vec<Effect> {
        if self.branches.overlap_load.finish() {
            return vec![self.overlaps_effect()];
        }
        if self.branches.list.branches.len() < 2 {
            return Vec::new();
        }
        match result {
            Ok(overlaps) => {
                self.branches.overlaps = overlaps;
                self.branches.overlap_error = None;
            }
            Err(e) => self.branches.overlap_error = Some(e),
        }
        Vec::new()
    }

    /// Reloads an open linked-worktree diff: with its latest changes, or
    /// empty once the worktree is clean. A removed worktree is left alone.
    fn refresh_open_worktree(&self) -> Option<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return None;
        };
        let root = d.entry.root.as_ref().filter(|_| d.entry.is_worktree())?;
        let checkout = self
            .branches
            .list
            .branches
            .iter()
            .filter_map(|b| b.worktree.as_ref())
            .find(|w| &w.path == root)?;
        let entry = checkout.dirty.clone().unwrap_or_else(|| {
            let mut clean = d.entry.clone();
            clean.clear_files();
            clean
        });
        Some(Effect::LoadDiff {
            entry: Box::new(entry),
            refresh: true,
        })
    }

    pub(super) fn on_pulls(
        &mut self,
        result: Result<(String, Vec<PullRequest>), PullsError>,
    ) -> Vec<Effect> {
        let follow_up = self.branches.pulls_load.finish();
        let selected = self.branches.selected_key();
        self.branches.pull_status = match result {
            Ok((repo, pulls)) => {
                self.branches.pulls = pulls;
                PullStatus::Ready(repo)
            }
            Err(PullsError::NoToken) => {
                self.branches.pulls.clear();
                PullStatus::NoToken
            }
            Err(PullsError::Unsupported(why)) => {
                self.branches.pulls.clear();
                PullStatus::Unsupported(why)
            }
            // Keep showing the last list; it is only stale.
            Err(PullsError::Failed(why)) => PullStatus::Failed(why),
        };
        self.branches.reselect(selected.as_deref());
        if follow_up {
            vec![Effect::LoadPulls]
        } else {
            Vec::new()
        }
    }

    pub(super) fn enter_branches(&mut self) -> Vec<Effect> {
        self.tab = Tab::Branches;
        self.poll_branches()
    }

    pub(super) fn on_branches_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let len = self.branches.rows().len();
        if self.branches.pos.navigate(key, len) {
            return Vec::new();
        }
        if key.code == KeyCode::Char('f') {
            self.branches.filter = self.branches.filter.next();
            self.branches.pos = ListPos::default();
            return Vec::new();
        }
        if key.code == KeyCode::Char('d') {
            return self.ask_remove();
        }
        if key.code == KeyCode::Char('c') {
            self.open_collisions();
            return Vec::new();
        }
        if key.code == KeyCode::Esc && !self.branches.name_filter.is_empty() {
            self.branches.name_filter.clear();
            self.branches.pos = ListPos::default();
            return Vec::new();
        }
        self.open_selected(key.code)
    }

    /// Lists what the selected row collides with, or says there is nothing.
    fn open_collisions(&mut self) {
        let list = self
            .branches
            .rows()
            .get(self.branches.pos.selected)
            .and_then(|r| r.branch)
            .map(|b| self.branches.collisions_of(&b.name))
            .filter(|l| !l.rows.is_empty());
        if list.is_none() {
            self.notice = Some("No collisions on this line of work".to_owned());
        }
        self.branches.collisions = list;
    }

    /// Moves through the open list; `⏎` opens the file, anything else closes.
    pub(super) fn on_collisions_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(list) = self.branches.collisions.as_mut() else {
            return Vec::new();
        };
        if list.pos.navigate(key, list.rows.len()) {
            return Vec::new();
        }
        let list = self.branches.collisions.take();
        if key.code == KeyCode::Enter {
            return list.map_or_else(Vec::new, |l| self.open_collision(&l));
        }
        Vec::new()
    }

    /// Opens the diff of `list`'s line of work that holds the selected file.
    fn open_collision(&mut self, list: &CollisionList) -> Vec<Effect> {
        let Some(row) = list.rows.get(list.pos.selected) else {
            return Vec::new();
        };
        let entry = self
            .branches
            .list
            .branches
            .iter()
            .find(|b| b.name == list.name)
            .and_then(|b| change_holding(b, &row.path));
        let Some(entry) = entry else {
            self.notice = Some(format!("{} has moved on; nothing to open", list.name));
            return Vec::new();
        };
        self.branches.focus_file = Some((entry.id.clone(), row.path.clone()));
        vec![Effect::LoadDiff {
            entry: Box::new(entry),
            refresh: false,
        }]
    }

    /// Opens a diff of the selected row: `code` picks which, mirroring the
    /// keys that reach this.
    fn open_selected(&mut self, code: KeyCode) -> Vec<Effect> {
        let Some((dirty, changes, pull)) = self
            .branches
            .rows()
            .get(self.branches.pos.selected)
            .map(|r| {
                (
                    r.dirty().cloned(),
                    r.changes().cloned(),
                    r.pull.map(pull_entry),
                )
            })
        else {
            return Vec::new();
        };
        let entry = match code {
            KeyCode::Enter | KeyCode::Char('l') => dirty.or(changes).or(pull),
            KeyCode::Char('b') => {
                changes.or_else(|| self.notice_none("No branch here; p opens the pull request"))
            }
            KeyCode::Char('p') => {
                pull.or_else(|| self.notice_none("No open pull request for this branch"))
            }
            _ => None,
        };
        entry
            .map(|e| {
                vec![Effect::LoadDiff {
                    entry: Box::new(e),
                    refresh: false,
                }]
            })
            .unwrap_or_default()
    }

    /// Keys while the name filter is being typed: characters edit the query,
    /// navigation and `⏎` still act, `esc` stops typing.
    pub(super) fn on_branch_filter_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.branches.rows().len();
        let nav = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) || (ctrl && matches!(key.code, KeyCode::Char('n' | 'p' | 'j' | 'k' | 'd')));
        if nav && self.branches.pos.navigate(key, len) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => {
                self.branches.filter_editing = false;
                return Vec::new();
            }
            KeyCode::Enter => {
                self.branches.filter_editing = false;
                return self.open_selected(KeyCode::Enter);
            }
            KeyCode::Backspace => {
                self.branches.name_filter.pop();
            }
            KeyCode::Char('u') if ctrl => self.branches.name_filter.clear(),
            KeyCode::Char(c) if !ctrl => self.branches.name_filter.push(c),
            _ => return Vec::new(),
        }
        self.branches.pos = ListPos::default();
        Vec::new()
    }

    /// Asks whether to remove the selected row, refusing the risky ones.
    pub(super) fn ask_remove(&mut self) -> Vec<Effect> {
        enum Ask {
            /// Nothing is selected.
            Nothing,
            /// Tell the user why not.
            Refuse(String),
            /// Offer this removal.
            Offer(Removal),
        }
        let ask = match self.branches.rows().get(self.branches.pos.selected) {
            None => Ask::Nothing,
            Some(row) => match row.branch {
                None => {
                    Ask::Refuse("Pull requests live on GitHub; nothing to remove here".to_owned())
                }
                Some(b) if b.worktree.as_ref().is_some_and(|w| w.current) => {
                    Ask::Refuse("This is the checkout vert is running in".to_owned())
                }
                Some(b) if b.worktree.as_ref().is_some_and(|w| w.main) => {
                    Ask::Refuse("This is the repository's main checkout".to_owned())
                }
                Some(b) if b.remote => Ask::Refuse(
                    "Remote branches follow their remote; nothing to remove locally".to_owned(),
                ),
                Some(b) if b.worktree.as_ref().and_then(|w| w.dirty.as_ref()).is_some() => {
                    Ask::Refuse(format!(
                        "{} has uncommitted changes; commit or discard them first",
                        b.name
                    ))
                }
                Some(b) => Ask::Offer(Removal {
                    worktree: b
                        .worktree
                        .as_ref()
                        .map(|w| (w.name.clone(), w.path.clone())),
                    branch: b.branch.clone().map(|name| BranchRemoval {
                        name,
                        tip: tip_of(b.changes.as_ref()),
                    }),
                    unmerged: b.ahead_behind.map(|(ahead, _)| ahead),
                    pull: row.pull.map(|p| p.number),
                }),
            },
        };
        match ask {
            Ask::Nothing => Vec::new(),
            Ask::Refuse(why) => self.refuse(&why),
            Ask::Offer(removal) => {
                self.branches.confirm = Some(removal);
                Vec::new()
            }
        }
    }

    /// Answers the removal confirmation: `true` removes, anything else cancels.
    pub(super) fn answer_remove(&mut self, confirmed: bool) -> Vec<Effect> {
        match (self.branches.confirm.take(), confirmed) {
            (Some(removal), true) => vec![Effect::Remove {
                worktree: removal.worktree,
                branch: removal.branch,
            }],
            _ => Vec::new(),
        }
    }

    /// Tells the user why nothing will be removed.
    fn refuse(&mut self, why: &str) -> Vec<Effect> {
        self.notice = Some(why.to_owned());
        Vec::new()
    }
    fn notice_none(&mut self, text: &str) -> Option<Entry> {
        self.notice = Some(text.to_owned());
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::Msg;
    use crate::git::RepoInfo;
    use crate::git::branches::Worktree;
    use crate::git::diff::FileDiff;
    use crate::git::overlap::{Collision, SharedFile};
    use crate::git::{FileStat, FileStatus};

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn change(id: &str, time: i64, target: Target) -> Entry {
        let mut e = pull_entry(&pull(0, "", 0));
        e.target = target;
        id.clone_into(&mut e.id);
        e.time = time;
        e.files = vec![FileStat {
            path: "a.rs".to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            binary: false,
        }];
        e
    }

    fn branch(name: &str, time: i64) -> Branch {
        Branch {
            name: name.to_owned(),
            branch: Some(name.to_owned()),
            remote: false,
            ahead_behind: Some((1, 0)),
            changes: Some(change(
                &format!("branch:{name}"),
                time,
                Target::Range {
                    base: None,
                    head: git2::Oid::ZERO_SHA1,
                },
            )),
            worktree: None,
        }
    }

    fn list_of(names: &[&str]) -> Branches {
        Branches {
            base: None,
            branches: names.iter().map(|n| branch(n, 100)).collect(),
        }
    }

    fn overlap_effect(names: &[&str]) -> Effect {
        Effect::LoadOverlaps(Box::new(list_of(names)))
    }

    fn collisions(first: &str, second: &str) -> Overlaps {
        Overlaps {
            collisions: vec![Collision {
                first: first.to_owned(),
                second: second.to_owned(),
                level: Level::Conflict,
                files: Vec::new(),
            }],
            unchecked: 0,
        }
    }

    fn with_files(mut b: Branch, paths: &[&str]) -> Branch {
        let files = paths.iter().map(|p| stat(p)).collect();
        if let Some(e) = b.changes.as_mut() {
            e.files = files;
        }
        b
    }

    fn stat(path: &str) -> FileStat {
        FileStat {
            path: (*path).to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            binary: false,
        }
    }

    fn file_diff(path: &str) -> FileDiff {
        FileDiff {
            stat: stat(path),
            hunks: Vec::new(),
        }
    }

    /// An app on the Branches tab with `a` selected: it conflicts with `b` on
    /// `x.rs`, and overlaps it on `a.rs` and with `c` on `y.rs`.
    fn colliding_app() -> App {
        let mut app = app(false);
        press(&mut app, KeyCode::Char('4'));
        app.update(Msg::Branches(Ok(Branches {
            base: None,
            branches: vec![
                with_files(branch("a", 300), &["a.rs", "x.rs", "y.rs"]),
                branch("b", 200),
                branch("c", 100),
            ],
        })));
        let file = |path: &str, level| SharedFile {
            path: path.to_owned(),
            level,
        };
        app.update(Msg::Overlaps(Ok(Overlaps {
            collisions: vec![
                Collision {
                    first: "a".to_owned(),
                    second: "b".to_owned(),
                    level: Level::Conflict,
                    files: vec![file("a.rs", Level::Overlap), file("x.rs", Level::Conflict)],
                },
                Collision {
                    first: "c".to_owned(),
                    second: "a".to_owned(),
                    level: Level::Overlap,
                    files: vec![file("y.rs", Level::Overlap)],
                },
            ],
            unchecked: 0,
        })));
        app
    }

    fn listed(app: &App) -> Vec<(String, String)> {
        app.branches
            .collisions
            .iter()
            .flat_map(|l| &l.rows)
            .map(|r| (r.other.clone(), r.path.clone()))
            .collect()
    }

    #[test]
    fn c_lists_the_collisions_of_a_row_most_serious_first() {
        let mut app = colliding_app();
        assert_eq!(press(&mut app, KeyCode::Char('c')), []);
        let pair = |o: &str, p: &str| (o.to_owned(), p.to_owned());
        assert_eq!(
            listed(&app),
            [pair("b", "x.rs"), pair("b", "a.rs"), pair("c", "y.rs")]
        );
        press(&mut app, KeyCode::Char('j'));
        let selected = app.branches.collisions.as_ref().map(|l| l.pos.selected);
        assert_eq!(selected, Some(1));
        assert_eq!(press(&mut app, KeyCode::Esc), []);
        assert_eq!(app.branches.collisions, None);
    }

    #[test]
    fn c_says_so_when_a_row_collides_with_nothing() {
        let mut app = app(false);
        press(&mut app, KeyCode::Char('4'));
        app.update(Msg::Branches(Ok(list_of(&["a"]))));
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.branches.collisions, None);
        assert_eq!(
            app.notice.as_deref(),
            Some("No collisions on this line of work")
        );
    }

    #[test]
    fn enter_opens_the_diff_on_the_colliding_file() {
        let mut app = colliding_app();
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char('j'));
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(app.branches.collisions, None);
        let [Effect::LoadDiff { entry, refresh }] = effects.as_slice() else {
            panic!("expected a diff: {effects:?}");
        };
        assert_eq!(entry.id, "branch:a");
        assert!(!refresh);
        app.update(Msg::Diff {
            entry: entry.clone(),
            result: Ok(vec![file_diff("x.rs"), file_diff("a.rs")]),
            refresh: false,
        });
        let detail = app.views.last().and_then(View::as_detail).expect("detail");
        assert_eq!(detail.pos.selected, 1);
    }

    #[test]
    fn a_focus_only_applies_to_the_diff_it_was_meant_for() {
        let mut state = BranchesState {
            focus_file: Some(("one".to_owned(), "x.rs".to_owned())),
            ..BranchesState::default()
        };
        let files = [file_diff("a.rs"), file_diff("x.rs")];
        assert_eq!(state.take_focus("other", &files), 0);
        assert_eq!(state.take_focus("one", &files), 0, "used up");
        state.focus_file = Some(("one".to_owned(), "gone.rs".to_owned()));
        assert_eq!(state.take_focus("one", &files), 0);
        state.focus_file = Some(("one".to_owned(), "old.rs".to_owned()));
        let moved = FileDiff {
            stat: FileStat {
                old_path: Some("old.rs".to_owned()),
                ..stat("new.rs")
            },
            hunks: Vec::new(),
        };
        assert_eq!(state.take_focus("one", &[file_diff("a.rs"), moved]), 1);
    }

    #[test]
    fn uncommitted_work_is_opened_when_it_holds_the_file() {
        let mut dirty = checkout("a", false, true);
        if let Some(e) = dirty.worktree.as_mut().and_then(|w| w.dirty.as_mut()) {
            e.files = vec![stat("x.rs")];
        }
        let dirty = with_files(dirty, &["y.rs"]);
        let id = |path| change_holding(&dirty, path).map(|e| e.id);
        assert_eq!(id("x.rs").as_deref(), Some("worktree:a"));
        assert_eq!(id("y.rs").as_deref(), Some("branch:a"));
        assert_eq!(id("z.rs"), None);
    }

    #[test]
    fn a_line_of_work_that_moved_on_opens_nothing() {
        let mut app = colliding_app();
        press(&mut app, KeyCode::Char('c'));
        app.update(Msg::Branches(Ok(list_of(&["b", "c"]))));
        assert_eq!(press(&mut app, KeyCode::Enter), []);
        assert_eq!(
            app.notice.as_deref(),
            Some("a has moved on; nothing to open")
        );
    }

    #[test]
    fn a_changed_list_looks_for_collisions_once() {
        let mut app = app(false);
        let names = ["a", "b"];
        assert_eq!(
            app.update(Msg::Branches(Ok(list_of(&names)))),
            [overlap_effect(&names)]
        );
        assert_eq!(app.update(Msg::Overlaps(Ok(collisions("a", "b")))), []);
        assert_eq!(app.branches.overlaps, collisions("a", "b"));
        assert_eq!(app.update(Msg::Branches(Ok(list_of(&names)))), []);
    }

    #[test]
    fn pairs_left_unchecked_carry_on_at_the_next_reload() {
        let mut app = app(false);
        let names = ["a", "b"];
        app.update(Msg::Branches(Ok(list_of(&names))));
        app.update(Msg::Overlaps(Ok(Overlaps {
            unchecked: 1,
            ..Overlaps::default()
        })));
        assert_eq!(
            app.update(Msg::Branches(Ok(list_of(&names)))),
            [overlap_effect(&names)]
        );
    }

    #[test]
    fn a_lone_line_of_work_has_nothing_to_collide_with() {
        let mut app = app(false);
        app.update(Msg::Branches(Ok(list_of(&["a", "b"]))));
        app.update(Msg::Overlaps(Ok(collisions("a", "b"))));
        assert_eq!(app.update(Msg::Branches(Ok(list_of(&["a"])))), []);
        assert_eq!(app.branches.overlaps, Overlaps::default());
    }

    #[test]
    fn a_result_is_dropped_once_the_list_shrinks_to_one() {
        let mut app = app(false);
        app.update(Msg::Branches(Ok(list_of(&["a", "b"]))));
        app.branches.overlap_error = Some("boom".to_owned());
        assert_eq!(app.update(Msg::Branches(Ok(list_of(&["a"])))), []);
        assert_eq!(app.branches.overlap_error, None);
        assert_eq!(app.update(Msg::Overlaps(Ok(collisions("a", "b")))), []);
        assert_eq!(app.branches.overlaps, Overlaps::default());
        let names = ["a", "b"];
        assert_eq!(
            app.update(Msg::Branches(Ok(list_of(&names)))),
            [overlap_effect(&names)]
        );
    }

    #[test]
    fn a_result_for_an_old_list_gives_way_to_the_newest_list() {
        let mut app = app(false);
        app.update(Msg::Branches(Ok(list_of(&["a", "b"]))));
        assert_eq!(app.update(Msg::Branches(Ok(list_of(&["a", "c"])))), []);
        assert_eq!(
            app.update(Msg::Overlaps(Ok(collisions("a", "b")))),
            [overlap_effect(&["a", "c"])]
        );
        assert_eq!(app.branches.overlaps, Overlaps::default());
        assert_eq!(app.update(Msg::Overlaps(Ok(collisions("a", "c")))), []);
        assert_eq!(app.branches.overlaps, collisions("a", "c"));
    }

    #[test]
    fn a_failed_search_keeps_the_last_collisions_until_one_works() {
        let mut app = app(false);
        app.update(Msg::Branches(Ok(list_of(&["a", "b"]))));
        app.update(Msg::Overlaps(Ok(collisions("a", "b"))));
        app.update(Msg::Branches(Ok(list_of(&["a", "b", "c"]))));
        app.update(Msg::Overlaps(Err("boom".to_owned())));
        assert_eq!(app.branches.overlap_error.as_deref(), Some("boom"));
        assert_eq!(app.branches.overlaps, collisions("a", "b"));
        app.update(Msg::Branches(Ok(list_of(&["a", "b"]))));
        app.update(Msg::Overlaps(Ok(Overlaps::default())));
        assert_eq!(app.branches.overlap_error, None);
    }

    fn checkout(name: &str, current: bool, dirty: bool) -> Branch {
        let path = PathBuf::from(format!("/wt/{name}"));
        let mut b = branch(name, 100);
        b.worktree = Some(Worktree {
            name: name.to_owned(),
            path: path.clone(),
            current,
            main: false,
            dirty: dirty.then(|| {
                let mut e = change(&format!("worktree:{name}"), 200, Target::WorkingTree);
                e.root = (!current).then_some(path);
                e
            }),
        });
        b
    }

    fn pull(number: u64, head: &str, updated: i64) -> PullRequest {
        PullRequest {
            number,
            title: format!("PR {number}"),
            author: "claude".to_owned(),
            draft: false,
            head: head.to_owned(),
            fork: false,
            updated,
            added: 4,
            removed: 2,
            checks: None,
        }
    }

    fn app(pulls: bool) -> App {
        let (mut app, _) = App::new(
            RepoInfo {
                name: "r".to_owned(),
                branch: None,
            },
            1,
            100,
            1_000,
            false,
        );
        app.set_refresh(Refresh {
            branches: Some(5),
            pulls: pulls.then_some(60),
        });
        app
    }

    /// An app on the Branches tab with a current checkout, a dirty linked
    /// worktree, two branches and three pull requests (one from a fork).
    fn loaded() -> App {
        let mut app = app(true);
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            [Effect::LoadBranches, Effect::LoadPulls]
        );
        app.update(Msg::Branches(Ok(Branches {
            base: Some("main".to_owned()),
            branches: vec![
                checkout("main", true, false),
                checkout("agent", false, true),
                branch("old", 10),
                branch("feat", 500),
            ],
        })));
        let mut fork = pull(9, "old", 900);
        fork.fork = true;
        app.update(Msg::Pulls(Ok((
            "o/r".to_owned(),
            vec![pull(7, "feat", 50), pull(8, "remote-only", 300), fork],
        ))));
        app
    }

    fn keys(app: &App) -> Vec<String> {
        app.branches.rows().iter().map(Row::key).collect()
    }

    #[test]
    fn a_pull_request_matches_one_branch_on_origin() {
        let mut app = app(true);
        let remote = |name: &str, time| {
            let mut b = branch(name, time);
            b.branch = name.split_once('/').map(|(_, n)| n.to_owned());
            b.remote = true;
            b
        };
        app.update(Msg::Branches(Ok(Branches {
            base: None,
            branches: vec![
                branch("feat", 100),
                remote("upstream/feat", 90),
                remote("origin/x", 80),
                remote("upstream/y", 70),
            ],
        })));
        app.update(Msg::Pulls(Ok((
            "o/r".to_owned(),
            vec![pull(1, "feat", 10), pull(2, "x", 10), pull(3, "y", 10)],
        ))));
        let rows = app.branches.rows();
        let pulls: Vec<_> = rows
            .iter()
            .map(|r| (r.key(), r.pull.map(|p| p.number)))
            .collect();
        assert_eq!(
            pulls,
            [
                ("b:feat".to_owned(), Some(1)),
                ("b:upstream/feat".to_owned(), None),
                ("b:origin/x".to_owned(), Some(2)),
                ("b:upstream/y".to_owned(), None),
                ("p:3".to_owned(), Some(3)),
            ],
            "only local or origin branches carry a pull request, once"
        );
    }

    #[test]
    fn rows_merge_branches_and_pull_requests() {
        let app = loaded();
        assert_eq!(
            keys(&app),
            ["b:main", "b:agent", "p:9", "b:feat", "p:8", "b:old"],
            "checkouts first, then newest activity"
        );
        let rows = app.branches.rows();
        assert_eq!(rows[3].pull.map(|p| p.number), Some(7), "matched by head");
        assert!(rows[5].pull.is_none(), "fork PRs never match local names");
        assert_eq!(rows[3].time(), 500);
        assert_eq!(
            rows[1].dirty().map(|e| e.id.as_str()),
            Some("worktree:agent")
        );
        assert!(rows[4].changes().is_none() && rows[4].dirty().is_none());
        assert_eq!(rows[4].time(), 300);
        assert_eq!(
            app.branches.pull_status,
            PullStatus::Ready("o/r".to_owned())
        );
        let empty = Row {
            branch: None,
            pull: None,
        };
        assert_eq!((empty.key(), empty.time()), (String::new(), 0));
    }

    #[test]
    fn filter_cycles() {
        let mut app = loaded();
        let mut seen = Vec::new();
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('f'));
            seen.push((app.branches.filter.title(), keys(&app).len()));
        }
        assert_eq!(
            seen,
            [
                ("worktrees", 2),
                ("branches", 4),
                ("pull requests", 3),
                ("all", 6)
            ]
        );
    }

    #[test]
    fn enter_opens_the_most_relevant_diff() {
        let mut app = loaded();
        let opened = |effects: Vec<Effect>| match effects.as_slice() {
            [
                Effect::LoadDiff {
                    entry,
                    refresh: false,
                },
            ] => entry.id.clone(),
            other => panic!("expected a diff, got {other:?}"),
        };
        // Clean current checkout: its branch.
        assert_eq!(opened(press(&mut app, KeyCode::Enter)), "branch:main");
        press(&mut app, KeyCode::Char('j'));
        // Dirty worktree: uncommitted first, b for the branch.
        assert_eq!(opened(press(&mut app, KeyCode::Enter)), "worktree:agent");
        assert_eq!(opened(press(&mut app, KeyCode::Char('b'))), "branch:agent");
        assert_eq!(press(&mut app, KeyCode::Char('p')), []);
        assert_eq!(
            app.notice.as_deref(),
            Some("No open pull request for this branch")
        );
        // A pull request without a local branch.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(opened(press(&mut app, KeyCode::Char('l'))), "pr:9");
        assert_eq!(press(&mut app, KeyCode::Char('b')), []);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("No branch"))
        );
        // A branch with a pull request: both diffs are available.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(opened(press(&mut app, KeyCode::Enter)), "branch:feat");
        assert_eq!(opened(press(&mut app, KeyCode::Char('p'))), "pr:7");
        assert_eq!(press(&mut app, KeyCode::Char('x')), []);

        app.branches.pulls.clear();
        app.branches.list.branches.clear();
        assert!(
            press(&mut app, KeyCode::Enter).is_empty(),
            "nothing selected"
        );
    }

    #[test]
    fn polls_only_while_visible_and_due() {
        let mut app = loaded();
        assert!(!app.branches.loading());
        assert!(app.update(Msg::Tick(1_004)).is_empty(), "not due yet");
        assert_eq!(app.update(Msg::Tick(1_005)), [Effect::LoadBranches]);
        assert!(
            app.update(Msg::Tick(1_011)).is_empty(),
            "one load at a time"
        );
        assert!(app.branches.loading());
        assert_eq!(
            app.update(Msg::Branches(Err("boom".to_owned()))),
            [Effect::LoadBranches],
            "a request made while loading follows up"
        );
        assert_eq!(app.branches.error.as_deref(), Some("boom"));
        app.update(Msg::Branches(Ok(Branches::default())));
        assert_eq!(
            app.update(Msg::Tick(1_060)),
            [Effect::LoadBranches, Effect::LoadPulls]
        );

        press(&mut app, KeyCode::Char('1'));
        app.update(Msg::Branches(Ok(Branches::default())));
        app.update(Msg::Pulls(Ok(("o/r".to_owned(), Vec::new()))));
        assert!(app.update(Msg::Tick(5_000)).is_empty(), "other tab");
    }

    #[test]
    fn without_network_pull_requests_are_never_requested() {
        let mut app = app(false);
        assert_eq!(app.branches.pull_status, PullStatus::Off);
        assert_eq!(press(&mut app, KeyCode::Char('4')), [Effect::LoadBranches]);
        app.update(Msg::Branches(Ok(Branches::default())));
        let effects = press(&mut app, KeyCode::Char('r'));
        assert!(effects.contains(&Effect::LoadBranches));
        assert!(!effects.contains(&Effect::LoadPulls));

        // Never configured: no polling at all after the first load.
        let (mut bare, _) = App::new(
            RepoInfo {
                name: "r".to_owned(),
                branch: None,
            },
            1,
            100,
            0,
            false,
        );
        assert_eq!(press(&mut bare, KeyCode::Char('4')), [Effect::LoadBranches]);
        bare.update(Msg::Branches(Ok(Branches::default())));
        assert_eq!(bare.update(Msg::Tick(1_000_000)), []);
    }

    #[test]
    fn pull_request_outcomes() {
        let mut app = loaded();
        app.update(Msg::Pulls(Err(PullsError::Failed("offline".to_owned()))));
        assert_eq!(
            app.branches.pull_status,
            PullStatus::Failed("offline".to_owned())
        );
        assert_eq!(app.branches.pulls.len(), 3, "stale list kept");
        app.update(Msg::Pulls(Err(PullsError::NoToken)));
        assert_eq!(app.branches.pull_status, PullStatus::NoToken);
        assert_eq!(app.branches.pulls, []);
        app.update(Msg::Pulls(Err(PullsError::Unsupported(
            "gitlab".to_owned(),
        ))));
        assert_eq!(
            app.branches.pull_status,
            PullStatus::Unsupported("gitlab".to_owned())
        );
    }

    #[test]
    fn selection_survives_reloads() {
        let mut app = loaded();
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(keys(&app)[app.branches.pos.selected], "b:feat");
        // A newer branch appears above it.
        app.update(Msg::Branches(Ok(Branches {
            base: None,
            branches: vec![
                checkout("main", true, false),
                branch("new", 2_000),
                branch("feat", 500),
            ],
        })));
        assert_eq!(keys(&app)[app.branches.pos.selected], "b:feat");
        app.update(Msg::Pulls(Ok((
            "o/r".to_owned(),
            vec![pull(1, "x", 9_999)],
        ))));
        assert_eq!(keys(&app)[app.branches.pos.selected], "b:feat");
    }

    #[test]
    fn repository_changes_and_refresh_reload_branches() {
        let mut app = loaded();
        let changes = [crate::watch::Change::Workdir].into_iter().collect();
        assert!(
            app.update(Msg::Changed(changes))
                .contains(&Effect::LoadBranches)
        );
        app.update(Msg::Branches(Ok(Branches::default())));
        let effects = press(&mut app, KeyCode::Char('r'));
        assert!(effects.contains(&Effect::LoadBranches) && effects.contains(&Effect::LoadPulls));
    }

    #[test]
    fn open_worktree_diffs_stay_live() {
        let mut app = loaded();
        press(&mut app, KeyCode::Char('j'));
        let Some(Effect::LoadDiff { entry, .. }) = press(&mut app, KeyCode::Enter).pop() else {
            panic!("diff")
        };
        let file = |path: &str| FileDiff {
            stat: FileStat {
                path: path.to_owned(),
                old_path: None,
                status: FileStatus::Added,
                added: 1,
                removed: 0,
                binary: false,
            },
            hunks: Vec::new(),
        };
        app.update(Msg::Diff {
            entry: entry.clone(),
            result: Ok(vec![file("a.rs")]),
            refresh: false,
        });
        // The watcher can't see linked worktrees, so polling keeps going
        // behind the open diff; pull requests wait until it closes.
        assert_eq!(app.update(Msg::Tick(9_999)), [Effect::LoadBranches]);
        let reload = app.update(Msg::Branches(Ok(Branches {
            base: None,
            branches: vec![checkout("agent", false, true)],
        })));
        let [
            Effect::LoadDiff {
                entry: fresh,
                refresh: true,
            },
        ] = reload.as_slice()
        else {
            panic!("refresh expected: {reload:?}")
        };
        assert_eq!(fresh.id, entry.id);

        // A refresh for some other change is ignored.
        let mut other = (**fresh).clone();
        other.id = "worktree:else".to_owned();
        app.update(Msg::Diff {
            entry: Box::new(other),
            result: Ok(vec![file("zzz.rs")]),
            refresh: true,
        });
        app.update(Msg::Diff {
            entry: fresh.clone(),
            result: Ok(vec![file("b.rs"), file("c.rs")]),
            refresh: true,
        });
        let Some(View::Detail(d)) = app.views.last() else {
            panic!("detail")
        };
        assert_eq!(d.files.len(), 2);

        // Once the worktree is clean, the open diff is reloaded empty.
        let reload = app.update(Msg::Branches(Ok(Branches {
            base: None,
            branches: vec![checkout("agent", false, false)],
        })));
        let [
            Effect::LoadDiff {
                entry: clean,
                refresh: true,
            },
        ] = reload.as_slice()
        else {
            panic!("clean refresh expected: {reload:?}")
        };
        assert_eq!(clean.id, entry.id);
        assert!(clean.files.is_empty() && clean.added == 0 && clean.removed == 0);

        // A removed worktree has nothing left to diff.
        let reload = app.update(Msg::Branches(Ok(Branches::default())));
        assert_eq!(reload, []);

        // Diffs opened from other tabs don't poll branches.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('1'));
        app.update(Msg::File {
            path: "a.rs".to_owned(),
            content: Ok(String::new()),
            line: 1,
            marks: std::collections::BTreeSet::new(),
            root: None,
        });
        assert_eq!(app.update(Msg::Tick(99_999)), []);
    }

    #[test]
    fn files_open_in_the_change_s_worktree() {
        let mut app = loaded();
        press(&mut app, KeyCode::Char('j'));
        let Some(Effect::LoadDiff { entry, .. }) = press(&mut app, KeyCode::Enter).pop() else {
            panic!("diff")
        };
        let root = entry.root.clone();
        assert_eq!(root, Some(PathBuf::from("/wt/agent")));
        app.update(Msg::Diff {
            entry,
            result: Ok(vec![FileDiff {
                stat: FileStat {
                    path: "a.rs".to_owned(),
                    old_path: None,
                    status: FileStatus::Added,
                    added: 1,
                    removed: 0,
                    binary: false,
                },
                hunks: Vec::new(),
            }]),
            refresh: false,
        });
        let effects = press(&mut app, KeyCode::Char('e'));
        assert!(
            matches!(&effects[..], [Effect::Edit { root: r, .. }] if *r == root),
            "{effects:?}"
        );
        let effects = press(&mut app, KeyCode::Enter);
        let [Effect::OpenFile { root: r, .. }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(*r, root);
        app.update(Msg::File {
            path: "a.rs".to_owned(),
            content: Ok("x\n".to_owned()),
            line: 1,
            marks: std::collections::BTreeSet::new(),
            root: root.clone(),
        });
        let effects = press(&mut app, KeyCode::Char('e'));
        assert!(matches!(&effects[..], [Effect::Edit { root: r, .. }] if *r == root));
    }

    /// A Branches tab seen from a linked worktree, with a clean linked
    /// worktree, a detached one, the main checkout and a remote branch: the
    /// shapes `d` can act on.
    fn removal_app() -> App {
        let mut app = app(true);
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            [Effect::LoadBranches, Effect::LoadPulls]
        );
        let detached = Branch {
            name: "probe".to_owned(),
            branch: None,
            worktree: Some(Worktree {
                name: "probe".to_owned(),
                path: PathBuf::from("/wt/probe"),
                current: false,
                main: false,
                dirty: None,
            }),
            ..branch("unused", 7)
        };
        // vert runs in a linked worktree, so the main checkout is not current.
        let main_row = Branch {
            name: "proj".to_owned(),
            worktree: Some(Worktree {
                name: "proj".to_owned(),
                path: PathBuf::from("/proj"),
                current: false,
                main: true,
                dirty: None,
            }),
            ..checkout("main", false, false)
        };
        let remote = Branch {
            name: "origin/gone".to_owned(),
            remote: true,
            ..branch("gone", 5)
        };
        app.update(Msg::Branches(Ok(Branches {
            base: Some("main".to_owned()),
            branches: vec![
                checkout("runner", true, false),
                checkout("agent", false, false),
                main_row,
                detached,
                remote,
            ],
        })));
        app
    }

    #[test]
    fn removal_needs_confirmation() {
        let mut app = loaded();
        // The running checkout is refused.
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.branches.confirm, None);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("running in"))
        );
        // So is a worktree with uncommitted changes.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("uncommitted"))
        );
        // A pull request on its own has nothing local to remove.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("GitHub")));
        // A branch asks first, naming its unmerged commits and open PR.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.notice, None);
        assert_eq!(
            app.branches.confirm,
            Some(Removal {
                worktree: None,
                branch: Some(BranchRemoval {
                    name: "feat".to_owned(),
                    tip: Some(Oid::ZERO_SHA1),
                }),
                unmerged: Some(1),
                pull: Some(7),
            })
        );
        // Any other key cancels — ⏎ included, since it opens elsewhere;
        // y confirms and the outcome reloads.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.branches.confirm, None);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            press(&mut app, KeyCode::Char('x')),
            Vec::<Effect>::new(),
            "x cancels"
        );
        assert_eq!(app.branches.confirm, None);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            vec![Effect::Remove {
                worktree: None,
                branch: Some(BranchRemoval {
                    name: "feat".to_owned(),
                    tip: Some(Oid::ZERO_SHA1),
                })
            }]
        );
        assert_eq!(
            app.update(Msg::Removed {
                removed: vec!["branch feat".to_owned()],
                error: None,
            }),
            vec![Effect::LoadBranches]
        );
        assert_eq!(app.notice.as_deref(), Some("Removed branch feat"));
    }

    #[test]
    fn worktree_rows_offer_worktree_and_branch() {
        let mut app = removal_app();
        assert_eq!(
            keys(&app),
            ["b:runner", "b:agent", "b:proj", "b:probe", "b:origin/gone"]
        );
        // The main checkout is refused, even seen from a linked worktree.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.branches.confirm, None);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("main checkout"))
        );
        press(&mut app, KeyCode::Char('k'));
        // A clean linked worktree takes its branch with it.
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.branches.confirm,
            Some(Removal {
                worktree: Some(("agent".to_owned(), PathBuf::from("/wt/agent"))),
                branch: Some(BranchRemoval {
                    name: "agent".to_owned(),
                    tip: Some(Oid::ZERO_SHA1),
                }),
                unmerged: Some(1),
                pull: None,
            })
        );
        press(&mut app, KeyCode::Esc);
        // A detached checkout offers the worktree alone.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.branches.confirm,
            Some(Removal {
                worktree: Some(("probe".to_owned(), PathBuf::from("/wt/probe"))),
                branch: None,
                unmerged: Some(1),
                pull: None,
            })
        );
        press(&mut app, KeyCode::Char('n'));
        // Remote-tracking rows follow their remote.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("Remote branches"))
        );
    }

    #[test]
    fn removal_failures_reload_and_report() {
        let mut app = loaded();
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('j'));
        }
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        let why = "removed worktree agent, but failed to delete the branch: nope".to_owned();
        app.update(Msg::Removed {
            removed: vec!["worktree agent".to_owned()],
            error: Some(why.clone()),
        });
        assert_eq!(app.notice.as_deref(), Some(why.as_str()));
        assert!(app.branches.loading(), "the list reloads either way");
    }

    #[test]
    fn typing_narrows_rows_and_esc_clears() {
        let mut app = loaded();
        // / starts filtering instead of opening the Search tab.
        press(&mut app, KeyCode::Char('/'));
        assert!(app.branches.filter_editing);
        assert_eq!(app.tab, Tab::Branches);
        for c in "feat".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(keys(&app), ["b:feat"]);
        assert_eq!(
            app.branches.pos.selected, 0,
            "selection resets while typing"
        );
        // Shortcuts become text while editing: d opens no dialog.
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.branches.confirm, None);
        assert_eq!(app.branches.name_filter, "featd");
        // ? types too instead of opening help.
        press(&mut app, KeyCode::Char('?'));
        assert!(!app.help);
        assert_eq!(app.branches.name_filter, "featd?");
        // ctrl-u clears the query, still editing.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.branches.filter_editing);
        assert_eq!(app.branches.name_filter, "");
        assert_eq!(keys(&app).len(), 6);
        // Backspace pops; esc stops editing but keeps the filter.
        for c in "age".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.branches.name_filter, "ag");
        press(&mut app, KeyCode::Esc);
        assert!(!app.branches.filter_editing);
        assert_eq!(keys(&app), ["b:agent"], "the filter persists");
        // A second esc clears it.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.branches.name_filter, "");
        assert_eq!(keys(&app).len(), 6);
        // 3 still reaches the Search tab.
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.tab, Tab::Search);
    }

    #[test]
    fn enter_opens_the_filtered_row() {
        let mut app = loaded();
        press(&mut app, KeyCode::Char('/'));
        for c in "agent".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        // Arrows still navigate while editing; j and k type.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.branches.pos.selected, 0, "one row, clamped");
        assert_eq!(app.branches.name_filter, "agent");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(!app.branches.filter_editing);
        let [Effect::LoadDiff { entry, .. }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(entry.id, "worktree:agent");
    }

    #[test]
    fn kind_and_name_filters_intersect() {
        let mut app = loaded();
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(keys(&app), ["b:main", "b:agent"]);
        press(&mut app, KeyCode::Char('/'));
        for c in "agent".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(keys(&app), ["b:agent"]);
    }

    #[test]
    fn an_open_view_takes_keys_over_the_name_filter() {
        let mut app = loaded();
        press(&mut app, KeyCode::Char('/'));
        assert!(app.branches.filter_editing);
        // A diff arrives and opens on top of the tab mid-typing.
        let entry = app
            .branches
            .rows()
            .first()
            .and_then(|r| r.changes().cloned())
            .expect("changes");
        app.update(Msg::Diff {
            entry: Box::new(entry),
            result: Ok(Vec::new()),
            refresh: false,
        });
        assert!(!app.views.is_empty());
        // esc leaves the view; the (invisible) filter does not swallow it.
        press(&mut app, KeyCode::Esc);
        assert!(app.views.is_empty());
        assert!(app.branches.filter_editing, "still typing");
        // Back on the tab, typing continues where it left off.
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.branches.name_filter, "a");
    }

    #[test]
    fn the_filter_matches_worktree_directory_names() {
        let mut app = app(true);
        press(&mut app, KeyCode::Char('4'));
        // The checkout's directory is named after the branch here.
        let mut b = checkout("feat", false, false);
        let mut wt = b.worktree.clone().expect("worktree");
        wt.name = "agent-wt".to_owned();
        wt.path = PathBuf::from("/wts/agent-wt");
        b.worktree = Some(wt);
        app.update(Msg::Branches(Ok(Branches {
            base: Some("main".to_owned()),
            branches: vec![checkout("main", true, false), b],
        })));
        press(&mut app, KeyCode::Char('/'));
        for c in "agent".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(
            keys(&app),
            ["b:feat"],
            "the directory name narrows to its row"
        );
        for c in "feat".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(keys(&app).is_empty(), "neither name matches");
    }
}
