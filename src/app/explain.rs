//! The explain view: an agent's walkthrough of the open change, stacked over
//! its diff. `x` opens it, `⏎` jumps from a claim to the hunk it is about.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, Focus, ListPos, View, half_page, max_scroll};
use crate::explain::{Answer, Diagram, Explainer, Explanation, Note, Ref, diff_key, tree};
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
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

    fn select(&mut self, delta: isize) {
        let len = self.targets().len();
        self.target = if len == 0 {
            0
        } else {
            self.target.saturating_add_signed(delta).min(len - 1)
        };
    }
}

/// The outline of `e`: the overview, each step, then concerns and the
/// unexplained hunks when there are any.
#[must_use]
pub fn items(e: &Explanation) -> Vec<Item> {
    std::iter::once(Item::Overview)
        .chain((0..e.steps.len()).map(Item::Step))
        .chain((!e.concerns.is_empty()).then_some(Item::Concerns))
        .chain((!e.unexplained.is_empty()).then_some(Item::Unexplained))
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
    }
}

/// The concern that [`targets`]`(e, Item::Concerns)[target]` belongs to;
/// the only concern when none has a pointer. Kept beside `targets` so the
/// two agree on how concerns flatten into pointers.
#[must_use]
pub fn concern_at(e: &Explanation, target: usize) -> Option<&Note> {
    let mut seen = 0;
    e.concerns
        .iter()
        .find(|n| {
            seen += n.refs.len();
            target < seen
        })
        .or(match e.concerns.as_slice() {
            [only] => Some(only),
            _ => None,
        })
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
    /// The last explain view left, to come back to where it was.
    parked: Option<Box<ExplainView>>,
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
            (KeyCode::Char('r'), _) => {
                v.status = Status::Asking(self.now);
                let (k, entry, files) = (v.key, v.entry.clone(), v.files.clone());
                self.explain.answers.remove(&k);
                return self.ask(k, &entry, &files, true);
            }
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
                if let Some(r) = v.targets().get(v.target).map(|r| (*r).clone()) {
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

        let again = asked(&press(&mut a, KeyCode::Char('r')), true);
        assert_eq!(again, key);
        assert_eq!(view(&a).status, Status::Asking(100));
        assert_eq!(press(&mut a, KeyCode::Char('r')), [], "already asking");
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
