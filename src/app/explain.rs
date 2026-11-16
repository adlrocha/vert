//! The explain view: an agent's walkthrough of the open change, stacked over
//! its diff. `x` opens it, `⏎` jumps from a claim to the hunk it is about,
//! `a` asks the agent a follow-up question, `space` marks concerns for `f`
//! to fix together, and `r` explains the change again as it is now.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::fix::ConcernFix;
use super::{App, Effect, Focus, ListPos, View, half_page, max_scroll};
use crate::explain::question::{Ask, Question};
use crate::explain::{Answer, Diagram, Explainer, Explanation, Note, Ref, diff_key, tree};
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::highlight::sanitize;
use crate::ui::detail::build_diff;

/// One entry of the outline on the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// The intent and the diagram.
    Overview,
    /// A step of the reading order.
    Step(usize),
    /// Everything the agent asks a reviewer to double-check.
    Concerns,
    /// Hunks nothing explains.
    Unexplained,
    /// A follow-up question and its answer.
    Question(usize),
}

/// Where an explanation is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The agent is working on it, since this Unix time.
    Asking(i64),
    /// The answer, and whether it came from the cache.
    Ready(Box<Explanation>, bool),
    /// Why there is no answer.
    Failed(String),
}

/// Which side of the explain view has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The outline of items.
    Outline,
    /// The selected item's text and code pointers.
    Detail,
}

/// The explain view.
#[derive(Debug)]
pub struct ExplainView {
    /// Identifies the exact diff explained.
    pub key: u64,
    /// The change explained.
    pub entry: Entry,
    /// Its diff, kept to ask again.
    pub files: Vec<FileDiff>,
    /// The agent's name.
    pub agent: String,
    /// The answer, or progress towards it.
    pub status: Status,
    /// Outline selection.
    pub pos: ListPos,
    /// Focused pane.
    pub pane: Pane,
    /// Selected code pointer of the item.
    pub target: usize,
    /// First visible row of the detail pane.
    pub scroll: usize,
    /// Rows of the detail pane in the last frame.
    pub height: usize,
    /// Rows of the detail pane's content in the last frame.
    pub rows: usize,
    /// A follow-up question being written.
    pub question: Option<String>,
    /// Concerns marked to fix together, by index.
    pub marked: BTreeSet<usize>,
    /// Whether `r` is waiting for the change's diff as it is now.
    pub reloading: bool,
}

/// A place the cursor of the detail pane can stop at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop<'a> {
    /// The code it points at, if any.
    pub pointer: Option<&'a Ref>,
    /// The concern it belongs to, on *Concerns*.
    pub concern: Option<usize>,
}

impl ExplainView {
    /// The explanation, once it has arrived.
    #[must_use]
    pub fn explanation(&self) -> Option<&Explanation> {
        match &self.status {
            Status::Ready(e, _) => Some(e),
            Status::Asking(_) | Status::Failed(_) => None,
        }
    }

    /// The outline.
    #[must_use]
    pub fn items(&self) -> Vec<Item> {
        self.explanation().map_or_else(Vec::new, items)
    }

    /// The selected outline item.
    #[must_use]
    pub fn item(&self) -> Option<Item> {
        self.items().get(self.pos.selected).copied()
    }

    /// The code pointers of the selected item, in display order.
    #[must_use]
    pub fn targets(&self) -> Vec<&Ref> {
        match (self.explanation(), self.item()) {
            (Some(e), Some(item)) => targets(e, item),
            _ => Vec::new(),
        }
    }

    /// Where the detail pane's cursor can stop in the selected item.
    #[must_use]
    pub fn stops(&self) -> Vec<Stop<'_>> {
        match (self.explanation(), self.item()) {
            (Some(e), Some(item)) => stops(e, item),
            _ => Vec::new(),
        }
    }

    /// Where the detail pane's cursor is.
    #[must_use]
    pub fn stop(&self) -> Option<Stop<'_>> {
        self.stops().get(self.target).copied()
    }

    fn select(&mut self, delta: isize) {
        let len = self.stops().len();
        self.target = if len == 0 {
            0
        } else {
            self.target.saturating_add_signed(delta).min(len - 1)
        };
    }
}

/// The outline of `e`: the overview, each step, then concerns and the
/// unexplained hunks when there are any, then the follow-up questions.
#[must_use]
pub fn items(e: &Explanation) -> Vec<Item> {
    std::iter::once(Item::Overview)
        .chain((0..e.steps.len()).map(Item::Step))
        .chain((!e.concerns.is_empty()).then_some(Item::Concerns))
        .chain((!e.unexplained.is_empty()).then_some(Item::Unexplained))
        .chain((0..e.questions.len()).map(Item::Question))
        .collect()
}

/// The code pointers `item` shows, in display order. The overview offers one
/// per diagram row that draws a node with code, in [`tree::layout`] order.
#[must_use]
pub fn targets(e: &Explanation, item: Item) -> Vec<&Ref> {
    match item {
        Item::Overview => e.diagram.as_ref().map_or_else(Vec::new, |d| {
            tree::layout(d)
                .iter()
                .filter_map(|row| row_pointer(d, row))
                .collect()
        }),
        Item::Step(i) => e
            .steps
            .get(i)
            .map_or_else(Vec::new, |s| s.refs.iter().collect()),
        Item::Concerns => e.concerns.iter().flat_map(|n| &n.refs).collect(),
        Item::Unexplained => e.unexplained.iter().collect(),
        Item::Question(i) => e
            .questions
            .get(i)
            .map_or_else(Vec::new, |q| q.refs.iter().collect()),
    }
}

/// Where the cursor can stop in `item`: each of its code pointers, in
/// [`targets`] order. On *Concerns*, each pointer also names its concern,
/// and a concern that points at no code gets a stop of its own, so every
/// concern can be marked.
#[must_use]
pub fn stops(e: &Explanation, item: Item) -> Vec<Stop<'_>> {
    if item != Item::Concerns {
        return targets(e, item)
            .into_iter()
            .map(|r| Stop {
                pointer: Some(r),
                concern: None,
            })
            .collect();
    }
    e.concerns
        .iter()
        .enumerate()
        .flat_map(|(i, n)| {
            let pointers: Vec<Option<&Ref>> = if n.refs.is_empty() {
                vec![None]
            } else {
                n.refs.iter().map(Some).collect()
            };
            pointers.into_iter().map(move |pointer| Stop {
                pointer,
                concern: Some(i),
            })
        })
        .collect()
}

/// The concern the [`stops`]`(e, Item::Concerns)[target]` belongs to, by
/// index.
#[must_use]
pub fn concern_at(e: &Explanation, target: usize) -> Option<(usize, &Note)> {
    let i = stops(e, Item::Concerns).get(target)?.concern?;
    e.concerns.get(i).map(|n| (i, n))
}

/// The code pointer a diagram row offers: the first ref of a node drawn for
/// the first time. The overview's targets and the drawn highlight both use
/// it, so they stay in step.
#[must_use]
pub fn row_pointer<'a>(d: &'a Diagram, row: &tree::Row) -> Option<&'a Ref> {
    if row.repeat {
        return None;
    }
    d.nodes[row.node].refs.first()
}

/// Explanations: which agent answers, and the answers so far.
#[derive(Debug, Default)]
pub struct ExplainState {
    /// The agent's name, or why explanations are unavailable.
    pub agent: Option<Result<String, String>>,
    /// Answers by [`diff_key`], kept while vert runs.
    answers: HashMap<u64, (Explanation, bool)>,
    /// Diffs an agent is working on.
    asking: HashSet<u64>,
    /// Since when an agent has been answering a question about a diff; one
    /// at a time per diff.
    questioning: HashMap<u64, i64>,
    /// Answers that came while their diff had no explanation to join,
    /// added to the next one.
    unshown: HashMap<u64, Vec<Question>>,
    /// The last explain view left, to come back to where it was.
    parked: Option<Box<ExplainView>>,
    /// Fixes asked for concerns, by [`diff_key`] and concern text.
    pub(super) fixes: HashMap<u64, BTreeMap<String, ConcernFix>>,
}

impl ExplainState {
    /// Since when (Unix time) an agent has been answering a question about
    /// the diff `key`, if it is.
    #[must_use]
    pub fn question_asked(&self, key: u64) -> Option<i64> {
        self.questioning.get(&key).copied()
    }

    /// The fixes asked for concerns of the diff `key`, by concern.
    #[must_use]
    pub fn concern_fixes(&self, key: u64) -> Option<&BTreeMap<String, ConcernFix>> {
        self.fixes.get(&key)
    }

    /// The fix asked for the concern reading `text` of the diff `key`, if
    /// any.
    #[must_use]
    pub fn concern_fix(&self, key: u64, text: &str) -> Option<&ConcernFix> {
        self.fixes.get(&key)?.get(text)
    }
}

/// Adds `q` to `e`'s questions unless it is there already (a new
/// explanation may have read it from the cache).
fn add_question(e: &mut Explanation, q: &Question) {
    if !e.questions.contains(q) {
        e.questions.push(q.clone());
    }
}

impl App {
    /// Sets the agent explanations come from, or why there is none.
    pub fn set_explainer(&mut self, explainer: &Result<Explainer, String>) {
        self.explain.agent = Some(
            explainer
                .as_ref()
                .map(Explainer::label)
                .map_err(Clone::clone),
        );
    }

    /// Opens the explain view for the change in the open detail view.
    pub(super) fn open_explain(&mut self) -> Vec<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return Vec::new();
        };
        let (entry, files) = (d.entry.clone(), d.files.clone());
        let agent = match &self.explain.agent {
            Some(Ok(agent)) => agent.clone(),
            Some(Err(why)) => {
                self.notice = Some(format!("Can't explain: {why}"));
                return Vec::new();
            }
            None => {
                self.notice = Some("Can't explain: no agent is set up".to_owned());
                return Vec::new();
            }
        };
        if files.is_empty() {
            self.notice = Some("Nothing to explain: the change is empty".to_owned());
            return Vec::new();
        }
        let key = diff_key(&entry, &files);
        if let Some(view) = self.explain.parked.take_if(|v| v.key == key) {
            self.views.push(View::Explain(view));
            return Vec::new();
        }
        let status = match self.explain.answers.get(&key) {
            Some((e, cached)) => Status::Ready(Box::new(e.clone()), *cached),
            None => Status::Asking(self.now),
        };
        let effects = match status {
            Status::Asking(_) => self.ask(key, &entry, &files, false),
            Status::Ready(..) | Status::Failed(_) => Vec::new(),
        };
        self.views.push(View::Explain(Box::new(ExplainView {
            key,
            entry,
            files,
            agent,
            status,
            pos: ListPos::default(),
            pane: Pane::Outline,
            target: 0,
            scroll: 0,
            height: 0,
            rows: 0,
            question: None,
            marked: BTreeSet::new(),
            reloading: false,
        })));
        effects
    }

    /// Asks the agent about a diff, unless it is already working on it.
    fn ask(&mut self, key: u64, entry: &Entry, files: &[FileDiff], fresh: bool) -> Vec<Effect> {
        if self.explain.asking.insert(key) {
            vec![Effect::Explain {
                key,
                entry: Box::new(entry.clone()),
                files: files.to_vec(),
                fresh,
            }]
        } else {
            Vec::new()
        }
    }

    /// An agent answered about the diff `key`.
    pub(super) fn on_explained(&mut self, key: u64, result: Result<Answer, String>) {
        self.explain.asking.remove(&key);
        let result = result.map(|mut answer| {
            for q in self.explain.unshown.remove(&key).unwrap_or_default() {
                add_question(&mut answer.explanation, &q);
            }
            answer
        });
        if let Ok(answer) = &result {
            self.explain
                .answers
                .insert(key, (answer.explanation.clone(), answer.cached));
        }
        let status = match &result {
            Ok(a) => Status::Ready(Box::new(a.explanation.clone()), a.cached),
            Err(why) => Status::Failed(why.clone()),
        };
        let mut shown = false;
        let open = self.views.iter_mut().filter_map(|v| match v {
            View::Explain(x) if x.key == key => Some(x.as_mut()),
            _ => None,
        });
        for view in open.chain(self.explain.parked.as_deref_mut().filter(|v| v.key == key)) {
            view.status = status.clone();
            view.pos = ListPos::default();
            view.target = 0;
            view.scroll = 0;
            shown = true;
        }
        if !shown {
            self.notice = Some(match result {
                Ok(_) => "An explanation is ready: open the change and press x".to_owned(),
                Err(why) => format!("Couldn't explain a change: {why}"),
            });
        }
    }

    /// Keys of the explain view. `esc` and `?` are handled before this.
    pub(super) fn on_explain_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = isize::try_from(half_page(v.height)).unwrap_or(1);
        let bottom = max_scroll(v.rows, v.height);
        let scroll = |v: &mut ExplainView, delta: isize| {
            v.scroll = v.scroll.saturating_add_signed(delta).min(bottom);
        };
        match (key.code, v.pane) {
            (KeyCode::Char('a'), _) => return self.start_question(),
            (KeyCode::Char('r'), _) => return self.explain_again(),
            (KeyCode::Char('c'), _) => return self.open_last_fix(),
            (KeyCode::Char(' '), _) => self.toggle_mark(),
            (KeyCode::Tab | KeyCode::BackTab, _) => {
                v.pane = match v.pane {
                    Pane::Outline => Pane::Detail,
                    Pane::Detail => Pane::Outline,
                };
            }
            (KeyCode::Char('h') | KeyCode::Left, _) => v.pane = Pane::Outline,
            (KeyCode::Char('l') | KeyCode::Right, _) => v.pane = Pane::Detail,
            (KeyCode::Char('J'), _) => scroll(v, 1),
            (KeyCode::Char('K'), _) => scroll(v, -1),
            (KeyCode::Char('d'), _) if ctrl => scroll(v, half),
            (KeyCode::Char('u'), _) if ctrl => scroll(v, -half),
            (KeyCode::PageDown, _) => scroll(v, half * 2),
            (KeyCode::PageUp, _) => scroll(v, -half * 2),
            (KeyCode::Enter | KeyCode::Char('o'), _) => {
                if let Some(r) = v.stop().and_then(|s| s.pointer).cloned() {
                    self.jump(&r);
                }
            }
            (KeyCode::Char('j') | KeyCode::Down, Pane::Detail) => v.select(1),
            (KeyCode::Char('k') | KeyCode::Up, Pane::Detail) => v.select(-1),
            (KeyCode::Char('g') | KeyCode::Home, Pane::Detail) => v.target = 0,
            (KeyCode::Char('G') | KeyCode::End, Pane::Detail) => v.select(isize::MAX),
            (_, Pane::Outline) => {
                let before = v.pos.selected;
                if v.pos.navigate(key, v.items().len()) && v.pos.selected != before {
                    v.target = 0;
                    v.scroll = 0;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// `r`: explains the change again as it is now. Its diff is loaded
    /// afresh first, since a fix may have changed it (see [`App::on_reloaded`]).
    fn explain_again(&mut self) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last() else {
            return Vec::new();
        };
        if v.reloading {
            return Vec::new();
        }
        let entry = self.current_entry(&v.entry);
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        let old = std::mem::replace(&mut v.status, Status::Asking(self.now));
        v.reloading = true;
        v.marked.clear();
        // The questions join the new explanation even when the cache that
        // would hand them over can't be written.
        let k = v.key;
        let questions = self
            .explain
            .answers
            .remove(&k)
            .map(|(e, _)| e.questions)
            .or(match old {
                Status::Ready(e, _) => Some(e.questions),
                Status::Asking(_) | Status::Failed(_) => None,
            })
            .unwrap_or_default();
        self.explain.unshown.entry(k).or_default().extend(questions);
        vec![Effect::LoadDiff {
            entry: Box::new(entry),
            refresh: true,
            file: None,
        }]
    }

    /// `entry` as vert last saw it: a branch at its current tip, uncommitted
    /// work as it is now. A commit or pull request stays as it is; loading
    /// its diff again fetches what it holds now.
    fn current_entry(&self, entry: &Entry) -> Entry {
        let branch = entry.branch().and_then(|name| {
            self.branches
                .list
                .branches
                .iter()
                .find(|b| b.name == name)?
                .changes
                .clone()
        });
        let own = entry
            .is_own_worktree()
            .then(|| self.feed.worktree.clone())
            .flatten();
        branch.or(own).unwrap_or_else(|| entry.clone())
    }

    /// The diff of `entry` came: an explain view waiting for it (`r`)
    /// explains it, under the key of the diff as it is now. The questions
    /// asked about it stay with it when it is unchanged; when a fix changed
    /// it, they stay with the old diff, as they are about that code.
    pub(super) fn on_reloaded(
        &mut self,
        entry: &Entry,
        result: &Result<Vec<FileDiff>, String>,
    ) -> Vec<Effect> {
        let open = self.views.iter_mut().filter_map(|v| match v {
            View::Explain(x) => Some(x.as_mut()),
            _ => None,
        });
        let Some(v) = open
            .chain(self.explain.parked.as_deref_mut())
            .find(|v| v.reloading && v.entry.id == entry.id)
        else {
            return Vec::new();
        };
        v.reloading = false;
        let files = match result {
            Ok(files) if files.is_empty() => {
                v.status = Status::Failed("Nothing to explain: the change is empty now".to_owned());
                return Vec::new();
            }
            Ok(files) => files.clone(),
            Err(why) => {
                v.status = Status::Failed(format!("Couldn't load the change as it is now: {why}"));
                return Vec::new();
            }
        };
        let key = diff_key(entry, &files);
        v.key = key;
        v.entry = entry.clone();
        v.files.clone_from(&files);
        v.pos = ListPos::default();
        v.target = 0;
        v.scroll = 0;
        self.ask(key, entry, &files, true)
    }

    /// `space` on *Concerns*: marks the concern under the cursor to fix, or
    /// unmarks it.
    fn toggle_mark(&mut self) {
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return;
        };
        let Some(i) = v.stop().and_then(|s| s.concern) else {
            if v.explanation().is_some() {
                self.notice = Some("Marking is for concerns: select one first".to_owned());
            }
            return;
        };
        let text = v
            .explanation()
            .and_then(|e| e.concerns.get(i))
            .map(|n| n.text.as_str())
            .unwrap_or_default();
        if matches!(
            self.explain.concern_fix(v.key, text),
            Some(ConcernFix::Fixing { .. })
        ) {
            self.notice = Some("The agent is already fixing that concern".to_owned());
        } else if !v.marked.remove(&i) {
            v.marked.insert(i);
        }
    }

    /// `a`: starts writing a follow-up question, once the explanation is
    /// in and no other question about it is being answered.
    fn start_question(&mut self) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        if v.explanation().is_none() {
            self.notice = Some("Wait for the explanation before asking about it".to_owned());
        } else if self.explain.questioning.contains_key(&v.key) {
            self.notice = Some(format!("{} is still answering your last question", v.agent));
        } else {
            v.question = Some(String::new());
        }
        Vec::new()
    }

    /// Keys while a question is being written: text edits it, `⏎` sends
    /// it, `esc` drops it.
    pub(super) fn on_question_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        let Some(text) = v.question.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => v.question = None,
            KeyCode::Enter => return self.send_question(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if ctrl => text.clear(),
            KeyCode::Char('w') if ctrl => super::delete_word(text, char::is_whitespace),
            KeyCode::Char(c) if !ctrl => text.push(c),
            _ => {}
        }
        Vec::new()
    }

    /// Sends the question being written, unless it is empty.
    fn send_question(&mut self) -> Vec<Effect> {
        let now = self.now;
        let Some(View::Explain(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        let question = v.question.as_deref().map(str::trim).unwrap_or_default();
        if question.is_empty() {
            self.notice = Some("Write a question first".to_owned());
            return Vec::new();
        }
        let Some(explanation) = v.explanation() else {
            return Vec::new();
        };
        let question = question.to_owned();
        let effect = Effect::Ask(Box::new(Ask {
            key: v.key,
            entry: v.entry.clone(),
            files: v.files.clone(),
            explanation: explanation.clone(),
            question: question.clone(),
        }));
        v.question = None;
        self.explain.questioning.insert(v.key, now);
        vec![effect]
    }

    /// The agent answered a question about the diff `key`: the answer joins
    /// the explanation wherever it is kept, and is selected when its view
    /// is on top.
    pub(super) fn on_answered(&mut self, key: u64, result: Result<Question, String>) {
        self.explain.questioning.remove(&key);
        let answered = match result {
            Ok(q) => q,
            Err(why) => {
                // The agent's own error output: untrusted, like all it prints.
                self.notice = Some(format!("Couldn't answer: {}", sanitize(&why)));
                return;
            }
        };
        let mut kept = false;
        if let Some((e, _)) = self.explain.answers.get_mut(&key) {
            add_question(e, &answered);
            kept = true;
        }
        let on_top = matches!(self.views.last(), Some(View::Explain(v)) if v.key == key);
        let mut shown = false;
        for view in self.explain_views(key) {
            if let Status::Ready(e, _) = &mut view.status {
                add_question(e, &answered);
                shown = true;
            }
        }
        if !shown && !kept {
            // Being explained again: the answer joins the new explanation.
            self.explain.unshown.entry(key).or_default().push(answered);
            return;
        }
        if on_top && let Some(View::Explain(v)) = self.views.last_mut() {
            let items = v.items();
            if let Some(i) = items.iter().rposition(|it| matches!(it, Item::Question(_))) {
                v.pos.selected = i;
                v.pane = Pane::Outline;
                v.target = 0;
                v.scroll = 0;
            }
        } else {
            self.notice = Some("An answer is ready: open the change and press x".to_owned());
        }
    }

    /// The explain views of the diff `key`, open or parked.
    pub(super) fn explain_views(&mut self, key: u64) -> impl Iterator<Item = &mut ExplainView> {
        let open = self.views.iter_mut().filter_map(move |v| match v {
            View::Explain(x) if x.key == key => Some(x.as_mut()),
            _ => None,
        });
        open.chain(
            self.explain
                .parked
                .as_deref_mut()
                .filter(move |v| v.key == key),
        )
    }

    /// Leaves the explain view on top, keeping it to come back to.
    pub(super) fn close_explain(&mut self) {
        if let Some(View::Explain(v)) = self.views.pop() {
            self.explain.parked = Some(v);
        }
    }

    /// Leaves the explain view and shows the code `r` points at in the diff.
    fn jump(&mut self, r: &Ref) {
        self.close_explain();
        let max = self.max_highlight;
        let Some(View::Detail(d)) = self.views.last_mut() else {
            return;
        };
        let Some(index) = d.files.iter().position(|f| f.stat.path == r.path) else {
            self.notice = Some(format!("{} is no longer part of this change", r.path));
            return;
        };
        d.pos.selected = index;
        d.rendered = build_diff(&d.files[index], max);
        d.hscroll = 0;
        d.focus = Focus::Diff;
        let start = r
            .hunk
            .and_then(|h| d.rendered.hunk_starts.get(h))
            .copied()
            .unwrap_or(0);
        d.scroll = start.min(max_scroll(d.rendered.len(), d.height));
    }
}

/// An explainer that asks pi, for tests.
#[cfg(test)]
pub(crate) fn pi_for_tests() -> Explainer {
    Explainer {
        agent: crate::explain::agent::Agent::Pi,
        model: None,
        timeout: std::time::Duration::from_secs(1),
        max_diff_bytes: 1,
        cache: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::{Change, Diagram, Edge, Node, Note, Step};

    fn r(path: &str, hunk: Option<usize>) -> Ref {
        Ref {
            path: path.to_owned(),
            hunk,
            label: String::new(),
        }
    }

    pub(crate) fn explanation() -> Explanation {
        let node = |label: &str, refs| Node {
            label: label.to_owned(),
            change: Change::Added,
            refs,
        };
        Explanation {
            intent: "Do the thing".to_owned(),
            steps: vec![
                Step {
                    title: "First".to_owned(),
                    detail: "Why".to_owned(),
                    refs: vec![r("src/a.rs", Some(1)), r("docs/x.md", None)],
                },
                Step {
                    title: "Second".to_owned(),
                    detail: String::new(),
                    refs: Vec::new(),
                },
            ],
            diagram: Some(Diagram {
                kind: "flow".to_owned(),
                title: String::new(),
                nodes: vec![
                    node("a", vec![r("src/a.rs", Some(0))]),
                    node("b", Vec::new()),
                    node("c", vec![r("docs/x.md", Some(1)), r("src/a.rs", None)]),
                ],
                edges: vec![
                    Edge {
                        from: 0,
                        to: 2,
                        label: String::new(),
                        change: Change::Same,
                    },
                    Edge {
                        from: 2,
                        to: 0,
                        label: String::new(),
                        change: Change::Same,
                    },
                ],
            }),
            concerns: vec![Note {
                text: "Check".to_owned(),
                refs: vec![r("gone.rs", None)],
            }],
            unexplained: vec![r("docs/x.md", Some(0))],
            dropped: 0,
            questions: Vec::new(),
        }
    }

    #[test]
    fn outline_and_targets() {
        let e = explanation();
        assert_eq!(
            items(&e),
            [
                Item::Overview,
                Item::Step(0),
                Item::Step(1),
                Item::Concerns,
                Item::Unexplained
            ]
        );
        let paths = |item| {
            targets(&e, item)
                .iter()
                .map(|r| (r.path.clone(), r.hunk))
                .collect::<Vec<_>>()
        };
        // Rows a, c, a↺, then b: the repeat and b (no code) offer nothing.
        assert_eq!(
            paths(Item::Overview),
            [
                ("src/a.rs".to_owned(), Some(0)),
                ("docs/x.md".to_owned(), Some(1))
            ]
        );
        assert_eq!(paths(Item::Step(0)).len(), 2);
        assert_eq!(paths(Item::Step(1)), []);
        assert_eq!(paths(Item::Step(9)), []);
        assert_eq!(paths(Item::Concerns), [("gone.rs".to_owned(), None)]);
        assert_eq!(
            paths(Item::Unexplained),
            [("docs/x.md".to_owned(), Some(0))]
        );

        let bare = Explanation {
            diagram: None,
            concerns: Vec::new(),
            unexplained: Vec::new(),
            ..e
        };
        assert_eq!(items(&bare), [Item::Overview, Item::Step(0), Item::Step(1)]);
        assert_eq!(targets(&bare, Item::Overview), [] as [&Ref; 0]);
    }
    use crossterm::event::{KeyEventKind, KeyEventState};

    use crate::app::Msg;
    use crate::git::diff::{DiffLine, Hunk, LineKind};
    use crate::git::{FileStat, FileStatus, RepoInfo, Target};

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Msg::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn ctrl(app: &mut App, c: char) {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )));
    }

    /// A file with two one-line hunks.
    fn file(path: &str) -> FileDiff {
        let hunk = |n: u32| Hunk {
            header: format!("@@ -{n} +{n} @@"),
            lines: vec![DiffLine {
                kind: LineKind::Added,
                old_no: None,
                new_no: Some(n),
                content: "x".to_owned(),
            }],
        };
        FileDiff {
            stat: FileStat {
                path: path.to_owned(),
                old_path: None,
                status: FileStatus::Modified,
                added: 2,
                removed: 0,
                binary: false,
            },
            hunks: vec![hunk(1), hunk(40)],
        }
    }

    fn change(id: &str) -> Entry {
        let mut e = Entry::blank(Target::Commit(git2::Oid::ZERO_SHA1));
        e.id = id.to_owned();
        e
    }

    /// An app showing the diff of `c1`: `src/a.rs` and `docs/x.md`.
    fn app() -> App {
        let (mut app, _) = App::new(
            RepoInfo {
                name: "r".to_owned(),
                branch: None,
            },
            1,
            1000,
            100,
            false,
        );
        app.update(Msg::Diff {
            entry: Box::new(change("c1")),
            result: Ok(vec![file("src/a.rs"), file("docs/x.md")]),
            refresh: false,
            file: None,
        });
        app
    }

    fn view(app: &App) -> &ExplainView {
        match app.views.last() {
            Some(View::Explain(v)) => v,
            _ => panic!("not explaining"),
        }
    }

    fn view_mut(app: &mut App) -> &mut ExplainView {
        match app.views.last_mut() {
            Some(View::Explain(v)) => v,
            _ => panic!("not explaining"),
        }
    }

    fn detail(app: &App) -> &crate::app::DetailView {
        match app.views.last() {
            Some(View::Detail(d)) => d,
            _ => panic!("not in the diff"),
        }
    }

    fn answer(cached: bool) -> Answer {
        Answer {
            explanation: explanation(),
            cached,
        }
    }

    /// Presses `r`, checks it reloads the change's diff, and hands that diff
    /// back unchanged: what follows.
    fn explain_again(a: &mut App) -> Vec<Effect> {
        let effects = press(a, KeyCode::Char('r'));
        let [
            Effect::LoadDiff {
                entry,
                refresh: true,
                file: None,
            },
        ] = effects.as_slice()
        else {
            panic!("expected the diff to reload, got {effects:?}");
        };
        let files = view(a).files.clone();
        a.update(Msg::Diff {
            entry: entry.clone(),
            result: Ok(files),
            refresh: true,
            file: None,
        })
    }

    /// The key of the only explain request in `effects`.
    fn asked(effects: &[Effect], fresh_wanted: bool) -> u64 {
        match effects {
            [Effect::Explain { key, fresh, .. }] if *fresh == fresh_wanted => *key,
            other => panic!("expected one explain request, got {other:?}"),
        }
    }

    #[test]
    fn explaining_needs_an_agent_and_a_change() {
        let mut a = app();
        assert_eq!(press(&mut a, KeyCode::Char('x')), []);
        assert!(a.notice.as_deref().is_some_and(|n| n.contains("no agent")));
        a.set_explainer(&Err("network access is off".to_owned()));
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(
            a.notice.as_deref(),
            Some("Can't explain: network access is off")
        );
        assert_eq!(a.views.len(), 1, "still on the diff");

        a.set_explainer(&Ok(pi_for_tests()));
        a.update(Msg::Diff {
            entry: Box::new(change("empty")),
            result: Ok(Vec::new()),
            refresh: false,
            file: None,
        });
        press(&mut a, KeyCode::Char('x'));
        assert!(a.notice.as_deref().is_some_and(|n| n.contains("Nothing")));
        // Only a diff can be explained.
        a.views.clear();
        assert_eq!(press(&mut a, KeyCode::Char('x')), []);
    }

    #[test]
    fn asks_once_shows_the_answer_and_jumps_to_code() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let key = asked(&press(&mut a, KeyCode::Char('x')), false);
        assert_eq!(view(&a).status, Status::Asking(100));
        assert_eq!(view(&a).agent, "pi");
        assert!(view(&a).items().is_empty(), "nothing to list yet");
        assert_eq!(view(&a).targets(), [] as [&Ref; 0]);

        // Leaving and coming back while it works asks nothing new.
        press(&mut a, KeyCode::Esc);
        assert!(matches!(a.views.last(), Some(View::Detail(_))));
        assert_eq!(press(&mut a, KeyCode::Char('x')), []);
        assert_eq!(view(&a).key, key);

        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        assert!(matches!(view(&a).status, Status::Ready(_, false)));
        assert_eq!(view(&a).item(), Some(Item::Overview));
        assert_eq!(view(&a).targets().len(), 2, "diagram nodes with code");

        // Step 1 points at src/a.rs hunk 2, then the whole of docs/x.md.
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(view(&a).item(), Some(Item::Step(0)));
        press(&mut a, KeyCode::Tab);
        assert_eq!(view(&a).pane, Pane::Detail);
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(view(&a).target, 1, "stops at the last pointer");
        press(&mut a, KeyCode::Char('g'));
        assert_eq!(view(&a).target, 0);
        press(&mut a, KeyCode::Enter);
        let d = detail(&a);
        assert_eq!(d.pos.selected, 0);
        assert_eq!(d.focus, Focus::Diff);
        assert_eq!(d.scroll, d.rendered.hunk_starts[1], "at the second hunk");

        // Back in the explanation, where it was left.
        assert_eq!(press(&mut a, KeyCode::Char('x')), []);
        assert_eq!(view(&a).item(), Some(Item::Step(0)));
        press(&mut a, KeyCode::Char('G'));
        press(&mut a, KeyCode::Char('o'));
        let d = detail(&a);
        assert_eq!(
            (d.pos.selected, d.scroll),
            (1, 0),
            "a whole file opens at the top"
        );
    }

    #[test]
    fn answers_are_reused_and_can_be_asked_again() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let key = asked(&press(&mut a, KeyCode::Char('x')), false);
        press(&mut a, KeyCode::Esc);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(true)),
        });
        assert_eq!(a.notice, None, "the parked view took the answer");
        a.explain.parked = None;
        assert_eq!(press(&mut a, KeyCode::Char('x')), [], "kept in memory");
        assert!(matches!(view(&a).status, Status::Ready(_, true)));

        let again = asked(&explain_again(&mut a), true);
        assert_eq!(again, key);
        assert_eq!(view(&a).status, Status::Asking(100));
        assert!(!view(&a).reloading);
        // Asked again while the agent works: the diff reloads, but the
        // agent isn't asked twice.
        assert_eq!(explain_again(&mut a), [], "already asking");
        a.update(Msg::Explained {
            key,
            result: Err("pi failed".to_owned()),
        });
        assert_eq!(view(&a).status, Status::Failed("pi failed".to_owned()));
        assert!(view(&a).explanation().is_none());

        // Answers for changes no longer on screen become notices.
        a.views.clear();
        a.explain.parked = None;
        a.update(Msg::Explained {
            key: 1,
            result: Ok(answer(false)),
        });
        assert!(a.notice.as_deref().is_some_and(|n| n.contains("ready")));
        a.update(Msg::Explained {
            key: 2,
            result: Err("boom".to_owned()),
        });
        assert_eq!(a.notice.as_deref(), Some("Couldn't explain a change: boom"));
    }

    #[test]
    fn keys_move_scroll_and_switch_panes() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let key = asked(&press(&mut a, KeyCode::Char('x')), false);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        {
            let v = view_mut(&mut a);
            v.rows = 50;
            v.height = 10;
        }
        press(&mut a, KeyCode::Char('J'));
        assert_eq!(view(&a).scroll, 1);
        press(&mut a, KeyCode::Char('K'));
        ctrl(&mut a, 'd');
        assert_eq!(view(&a).scroll, 5);
        ctrl(&mut a, 'u');
        press(&mut a, KeyCode::PageDown);
        assert_eq!(view(&a).scroll, 10);
        press(&mut a, KeyCode::PageDown);
        press(&mut a, KeyCode::PageDown);
        press(&mut a, KeyCode::PageDown);
        assert_eq!(view(&a).scroll, 40, "stops at the end");
        press(&mut a, KeyCode::PageUp);
        assert_eq!(view(&a).scroll, 30);

        press(&mut a, KeyCode::Char('l'));
        assert_eq!(view(&a).pane, Pane::Detail);
        press(&mut a, KeyCode::Char('h'));
        assert_eq!(view(&a).pane, Pane::Outline);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(view(&a).pane, Pane::Detail);
        press(&mut a, KeyCode::Right);
        press(&mut a, KeyCode::Left);
        // Moving in the outline resets the pointer and the scroll.
        press(&mut a, KeyCode::Char('G'));
        assert_eq!(view(&a).item(), Some(Item::Unexplained));
        assert_eq!((view(&a).target, view(&a).scroll), (0, 0));
        press(&mut a, KeyCode::Char('G'));
        press(&mut a, KeyCode::Char('?'));
        assert!(a.help, "help works here too");
        press(&mut a, KeyCode::Char('z'));
        press(&mut a, KeyCode::Char('z'));
        assert_eq!(
            view(&a).item(),
            Some(Item::Unexplained),
            "unbound keys do nothing"
        );

        // An item without code has nothing to select or open.
        press(&mut a, KeyCode::Char('g'));
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(view(&a).item(), Some(Item::Step(1)));
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(view(&a).target, 0);
        press(&mut a, KeyCode::Char('z'));
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.views.last(), Some(View::Explain(_))));

        // A pointer to a file the diff no longer has says so.
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(view(&a).item(), Some(Item::Concerns));
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.views.last(), Some(View::Detail(_))));
        assert_eq!(
            a.notice.as_deref(),
            Some("gone.rs is no longer part of this change")
        );
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn reply(refs: Vec<Ref>) -> Question {
        Question {
            question: "Why retry?".to_owned(),
            answer: "GitHub flakes.".to_owned(),
            refs,
        }
    }

    /// An app with the explanation of `c1` on screen, and its key.
    fn explained() -> (App, u64) {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let key = asked(&press(&mut a, KeyCode::Char('x')), false);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        (a, key)
    }

    #[test]
    fn explaining_again_explains_a_branch_at_its_current_tip() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let tip = |n: u8| {
            let mut e = Entry::blank(Target::Range {
                base: None,
                head: git2::Oid::from_bytes(&[n; 20]).expect("oid"),
            });
            e.id = Entry::branch_id("feat");
            e
        };
        a.update(Msg::Diff {
            entry: Box::new(tip(1)),
            result: Ok(vec![file("src/a.rs")]),
            refresh: false,
            file: None,
        });
        let old = asked(&press(&mut a, KeyCode::Char('x')), false);
        a.update(Msg::Explained {
            key: old,
            result: Ok(answer(false)),
        });
        a.update(Msg::Answered {
            key: old,
            result: Ok(reply(Vec::new())),
        });
        view_mut(&mut a).marked.insert(0);

        // A fix committed on the branch moved it.
        a.update(Msg::Branches(Ok(crate::git::branches::Branches {
            base: None,
            branches: vec![crate::git::branches::Branch {
                name: "feat".to_owned(),
                branch: Some("feat".to_owned()),
                remote: false,
                ahead_behind: None,
                changes: Some(tip(2)),
                worktree: None,
            }],
        })));
        let effects = press(&mut a, KeyCode::Char('r'));
        let [
            Effect::LoadDiff {
                entry,
                refresh: true,
                ..
            },
        ] = effects.as_slice()
        else {
            panic!("expected the diff to reload, got {effects:?}");
        };
        assert_eq!(**entry, tip(2), "the branch as it is now");
        assert!(view(&a).reloading);
        assert_eq!(
            view(&a).marked,
            BTreeSet::new(),
            "marks are about the old concerns"
        );
        assert_eq!(press(&mut a, KeyCode::Char('r')), [], "already reloading");

        let effects = a.update(Msg::Diff {
            entry: entry.clone(),
            result: Ok(vec![file("src/a.rs"), file("src/fixed.rs")]),
            refresh: true,
            file: None,
        });
        let new = asked(&effects, true);
        assert_ne!(new, old, "a new diff, under its own key");
        assert_eq!(view(&a).key, new);
        assert_eq!(view(&a).files.len(), 2);
        assert_eq!(view(&a).entry, tip(2));
        a.update(Msg::Explained {
            key: new,
            result: Ok(answer(false)),
        });
        assert_eq!(
            view(&a).explanation().map(|e| e.questions.len()),
            Some(0),
            "questions stay with the code they were about"
        );
        assert_eq!(a.explain.unshown.get(&old).map(Vec::len), Some(1));
    }

    #[test]
    fn explaining_again_says_when_the_change_can_t_be_reloaded() {
        let (mut a, _) = explained();
        let reload = |a: &mut App, result: Result<Vec<FileDiff>, String>| {
            let effects = press(a, KeyCode::Char('r'));
            let [Effect::LoadDiff { entry, .. }] = effects.as_slice() else {
                panic!("expected the diff to reload, got {effects:?}");
            };
            let effects = a.update(Msg::Diff {
                entry: entry.clone(),
                result,
                refresh: true,
                file: None,
            });
            assert_eq!(effects, []);
            assert!(!view(a).reloading);
        };
        reload(&mut a, Err("gone".to_owned()));
        assert_eq!(
            view(&a).status,
            Status::Failed("Couldn't load the change as it is now: gone".to_owned())
        );
        assert_eq!(a.notice.as_deref(), Some("Could not load diff: gone"));
        reload(&mut a, Ok(Vec::new()));
        assert_eq!(
            view(&a).status,
            Status::Failed("Nothing to explain: the change is empty now".to_owned())
        );

        // A diff that reloads for another view leaves this one alone.
        let other = a.update(Msg::Diff {
            entry: Box::new(change("other")),
            result: Ok(vec![file("x.rs")]),
            refresh: true,
            file: None,
        });
        assert_eq!(other, []);
    }

    #[test]
    fn explaining_again_finds_its_parked_view() {
        let (mut a, key) = explained();
        let effects = press(&mut a, KeyCode::Char('r'));
        let [Effect::LoadDiff { entry, .. }] = effects.as_slice() else {
            panic!("expected the diff to reload, got {effects:?}");
        };
        let entry = entry.clone();
        press(&mut a, KeyCode::Esc);
        let effects = a.update(Msg::Diff {
            entry,
            result: Ok(vec![file("src/a.rs"), file("docs/x.md")]),
            refresh: true,
            file: None,
        });
        assert_eq!(asked(&effects, true), key, "the same diff");
        let parked = a.explain.parked.as_ref().expect("parked");
        assert!(!parked.reloading);
    }

    #[test]
    fn own_uncommitted_work_reloads_as_it_is_now() {
        let (mut a, _) = explained();
        let mut wt = Entry::blank(Target::WorkingTree);
        view_mut(&mut a).entry = wt.clone();
        "Uncommitted changes".clone_into(&mut wt.summary);
        a.update(Msg::Worktree(Ok(Some(wt.clone()))));
        let effects = press(&mut a, KeyCode::Char('r'));
        let [Effect::LoadDiff { entry, .. }] = effects.as_slice() else {
            panic!("expected the diff to reload, got {effects:?}");
        };
        assert_eq!(**entry, wt);
    }

    #[test]
    fn questions_wait_for_the_explanation() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(press(&mut a, KeyCode::Char('a')), []);
        assert_eq!(view(&a).question, None);
        assert!(a.notice.as_deref().is_some_and(|n| n.contains("Wait")));
    }

    #[test]
    fn a_question_is_written_sent_answered_and_selected() {
        let (mut a, key) = explained();
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(view(&a).question.as_deref(), Some(""));
        // Keys type instead of acting, esc included.
        type_text(&mut a, "jrxaf q");
        assert_eq!(view(&a).question.as_deref(), Some("jrxaf q"));
        press(&mut a, KeyCode::Backspace);
        ctrl(&mut a, 'w');
        assert_eq!(view(&a).question.as_deref(), Some(""));
        type_text(&mut a, "gone");
        ctrl(&mut a, 'u');
        assert_eq!(view(&a).question.as_deref(), Some(""));
        ctrl(&mut a, 'x');
        assert_eq!(
            view(&a).question.as_deref(),
            Some(""),
            "other ctrl keys do nothing"
        );
        press(&mut a, KeyCode::Esc);
        assert_eq!(view(&a).question, None, "esc drops the question");
        assert!(
            matches!(a.views.last(), Some(View::Explain(_))),
            "and stays"
        );

        press(&mut a, KeyCode::Char('a'));
        type_text(&mut a, "  ");
        assert_eq!(press(&mut a, KeyCode::Enter), []);
        assert_eq!(a.notice.as_deref(), Some("Write a question first"));
        assert!(view(&a).question.is_some(), "still writing");
        type_text(&mut a, "Why retry? ");
        let effects = press(&mut a, KeyCode::Enter);
        let [Effect::Ask(asking)] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(asking.key, key);
        assert_eq!(asking.question, "Why retry?");
        assert_eq!(asking.explanation, explanation());
        assert_eq!(asking.files.len(), 2);
        assert_eq!(view(&a).question, None);
        let asked_now = |a: &App| a.explain.question_asked(key);
        assert_eq!(asked_now(&a), Some(100));
        assert_eq!(press(&mut a, KeyCode::Char('a')), []);
        assert!(
            a.notice
                .as_deref()
                .is_some_and(|n| n.contains("still answering"))
        );
        assert_eq!(view(&a).question, None);

        a.update(Msg::Answered {
            key,
            result: Ok(reply(vec![r("src/a.rs", Some(1))])),
        });
        assert_eq!(asked_now(&a), None);
        assert_eq!(
            view(&a).item(),
            Some(Item::Question(0)),
            "the answer is selected"
        );
        assert_eq!(view(&a).targets(), [&r("src/a.rs", Some(1))]);
        // The same answer read back by a new explanation isn't added twice.
        a.update(Msg::Answered {
            key,
            result: Ok(reply(vec![r("src/a.rs", Some(1))])),
        });
        assert_eq!(view(&a).explanation().map(|e| e.questions.len()), Some(1));
        press(&mut a, KeyCode::Enter);
        let d = detail(&a);
        assert_eq!(d.scroll, d.rendered.hunk_starts[1], "the answer's code");

        // Kept with the change: back from the parked view, and from memory.
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(view(&a).item(), Some(Item::Question(0)));
        press(&mut a, KeyCode::Esc);
        a.explain.parked = None;
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(view(&a).items().last(), Some(&Item::Question(0)));
    }

    #[test]
    fn explaining_again_keeps_questions_without_a_cache() {
        let (mut a, key) = explained();
        a.update(Msg::Answered {
            key,
            result: Ok(reply(Vec::new())),
        });
        // The new explanation comes back without them, as when the cache
        // could not be written.
        explain_again(&mut a);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        let count = |a: &App| view(a).explanation().map(|e| e.questions.len());
        assert_eq!(count(&a), Some(1));
        // Twice over, through a failure, and without duplicates.
        explain_again(&mut a);
        a.update(Msg::Explained {
            key,
            result: Err("x".to_owned()),
        });
        explain_again(&mut a);
        a.update(Msg::Explained {
            key,
            result: Ok(Answer {
                explanation: Explanation {
                    questions: vec![reply(Vec::new())],
                    ..explanation()
                },
                cached: false,
            }),
        });
        assert_eq!(count(&a), Some(1));
    }

    #[test]
    fn answers_elsewhere_and_failures_are_told() {
        let (mut a, key) = explained();
        let asked_now = |a: &App| a.explain.question_asked(key);
        press(&mut a, KeyCode::Char('a'));
        type_text(&mut a, "Why?");
        press(&mut a, KeyCode::Enter);
        a.update(Msg::Answered {
            key,
            result: Err("pi: \u{1b}[31mboom".to_owned()),
        });
        assert_eq!(asked_now(&a), None);
        assert_eq!(a.notice.as_deref(), Some("Couldn't answer: pi: ·[31mboom"));

        // Asked, then left: the question keeps going, and blocks another
        // from a view built afresh.
        press(&mut a, KeyCode::Char('a'));
        type_text(&mut a, "Why?");
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Esc);
        a.explain.parked = None;
        press(&mut a, KeyCode::Char('x'));
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(view(&a).question, None);
        assert!(
            a.notice
                .as_deref()
                .is_some_and(|n| n.contains("still answering"))
        );
        press(&mut a, KeyCode::Esc);
        a.update(Msg::Answered {
            key,
            result: Ok(reply(Vec::new())),
        });
        assert!(
            a.notice
                .as_deref()
                .is_some_and(|n| n.contains("An answer is ready"))
        );
        let parked = a.explain.parked.as_ref().expect("parked");
        assert!(parked.items().contains(&Item::Question(0)));

        // An answer that comes while the change is explained again, even
        // when that fails, joins the next explanation.
        press(&mut a, KeyCode::Char('x'));
        press(&mut a, KeyCode::Char('a'));
        type_text(&mut a, "More?");
        press(&mut a, KeyCode::Enter);
        explain_again(&mut a);
        a.update(Msg::Explained {
            key,
            result: Err("gone".to_owned()),
        });
        assert_eq!(asked_now(&a), Some(100), "More? is still being answered");
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(view(&a).question, None, "the explanation is gone");
        let more = Question {
            question: "More?".to_owned(),
            ..reply(Vec::new())
        };
        a.notice = None;
        a.update(Msg::Answered {
            key,
            result: Ok(more.clone()),
        });
        assert_eq!(a.notice, None, "nothing to show it in yet");
        explain_again(&mut a);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        let questions = view(&a).explanation().map(|e| e.questions.clone());
        assert_eq!(questions, Some(vec![reply(Vec::new()), more]), "both kept");
    }

    #[test]
    fn jumping_without_a_diff_underneath_just_closes() {
        let mut a = app();
        a.set_explainer(&Ok(pi_for_tests()));
        let key = asked(&press(&mut a, KeyCode::Char('x')), false);
        a.update(Msg::Explained {
            key,
            result: Ok(answer(false)),
        });
        a.views.remove(0);
        press(&mut a, KeyCode::Enter);
        assert!(a.views.is_empty());
        assert_eq!(a.on_explain_key(&KeyEvent::from(KeyCode::Enter)), []);
    }
}
