//! Questions from a diff: `a` asks the agent about the lines selected with
//! `v`, or about the whole file, without asking for an explanation first.
//!
//! The question is written in a small prompt. Sending it opens the answer
//! panel over the diff, which shows the agent working and then its answer,
//! with code pointers that `⏎` jumps to. `a` there asks a follow-up about
//! the same place. Leaving the panel keeps it, and `A` in the diff opens it
//! again. Answers also join the change's questions, so its explanation
//! (`x`) lists them too.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, DetailView, Effect, Focus, View, delete_word, half_page, max_scroll};
use crate::explain::Ref;
use crate::explain::diff_key;
use crate::explain::question::{About, Ask, Question};
use crate::fix::picked;
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::highlight::sanitize;
use crate::ui::detail::Source;

/// A question being written, before it goes to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskDraft {
    /// Identifies the exact diff (see [`diff_key`]).
    pub key: u64,
    /// The change.
    pub entry: Entry,
    /// Its diff.
    pub files: Vec<FileDiff>,
    /// Where in the change the question points.
    pub about: About,
    /// The question as typed so far (trimmed on sending).
    pub text: String,
}

/// A question the agent is answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// What was asked.
    pub question: String,
    /// Where it points ([`About::place`]).
    pub place: String,
    /// When it was sent (Unix time).
    pub since: i64,
}

/// The answer panel: the questions asked from one diff and their answers.
#[derive(Debug)]
pub struct AnswerView {
    /// Identifies the exact diff the questions are about.
    pub key: u64,
    /// The change.
    pub entry: Entry,
    /// Its diff.
    pub files: Vec<FileDiff>,
    /// The agent's name.
    pub agent: String,
    /// Where a follow-up (`a`) points: where the last question did.
    pub about: About,
    /// Questions answered here, oldest first.
    pub answered: Vec<Question>,
    /// The question the agent is working on, if any.
    pub pending: Option<Pending>,
    /// Why the last question got no answer (sanitized), if it didn't.
    pub failed: Option<String>,
    /// The selected code pointer, among [`AnswerView::targets`].
    pub target: usize,
    /// First visible row; past the end means the end.
    pub scroll: usize,
    /// Whether the view scrolls to keep the selected pointer in sight
    /// (after `j`/`k`), rather than staying where it was put.
    pub follow: bool,
    /// Rows visible in the last frame.
    pub height: usize,
    /// Rows of content in the last frame.
    pub rows: usize,
}

impl AnswerView {
    /// Every answer's code pointers, oldest answer first.
    #[must_use]
    pub fn targets(&self) -> Vec<&Ref> {
        self.answered.iter().flat_map(|q| &q.refs).collect()
    }

    fn select(&mut self, delta: isize) {
        self.follow = true;
        let len = self.targets().len();
        self.target = if len == 0 {
            0
        } else {
            self.target.saturating_add_signed(delta).min(len - 1)
        };
    }

    /// Selects the newest answer's first pointer and scrolls to the end, so
    /// the newest answer is what shows.
    fn show_newest(&mut self) {
        let earlier: usize = self
            .answered
            .iter()
            .rev()
            .skip(1)
            .map(|q| q.refs.len())
            .sum();
        self.target = earlier.min(self.targets().len().saturating_sub(1));
        self.scroll = usize::MAX;
        self.follow = false;
    }
}

/// Questions asked from diffs: the one being written, and the answer panel
/// left last, to come back to.
#[derive(Debug, Default)]
pub struct AskState {
    /// A question being written.
    pub draft: Option<AskDraft>,
    parked: Option<Box<AnswerView>>,
}

/// What `a` and `f` in a diff are about: the rows selected with `v`, as a
/// place and those lines in diff form, or `None` when nothing is selected
/// (or the selection holds nothing of the diff), for the whole file.
pub(super) fn selected(d: &DetailView) -> Option<(String, String)> {
    let selection = d.selection.filter(|_| d.focus == Focus::Diff)?;
    let file = d.files.get(d.pos.selected)?;
    let (first, last) = selection.rows();
    let picks: Vec<(usize, Option<usize>)> = d
        .rendered
        .sources
        .get(first..=last.min(d.rendered.sources.len().saturating_sub(1)))?
        .iter()
        .filter_map(|s| match *s {
            Source::Header(h) => Some((h, None)),
            Source::Line(h, i) => Some((h, Some(i))),
            Source::None => None,
        })
        .collect();
    picked(file, &picks)
}

impl App {
    /// `a` in a diff: starts writing a question about the lines selected,
    /// else about the whole selected file.
    pub(super) fn draft_ask_from_detail(&mut self) -> Vec<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return Vec::new();
        };
        let Some(file) = d.files.get(d.pos.selected) else {
            self.notice = Some("Nothing to ask about: the change is empty".to_owned());
            return Vec::new();
        };
        let about = selected(d).map_or_else(
            || About {
                place: file.stat.path.clone(),
                excerpt: String::new(),
            },
            |(place, excerpt)| About { place, excerpt },
        );
        let (entry, files) = (d.entry.clone(), d.files.clone());
        let key = diff_key(&entry, &files);
        self.start_ask(key, entry, files, about);
        Vec::new()
    }

    /// Starts writing a question about `about` in the diff `key`, unless
    /// there is no agent to ask or it is still answering about that diff.
    fn start_ask(&mut self, key: u64, entry: Entry, files: Vec<FileDiff>, about: About) {
        let agent = match &self.explain.agent {
            Some(Ok(agent)) => agent.clone(),
            Some(Err(why)) => {
                self.notice = Some(format!("Can't ask: {why}"));
                return;
            }
            None => {
                self.notice = Some("Can't ask: no agent is set up".to_owned());
                return;
            }
        };
        if self.explain.question_asked(key).is_some() {
            self.notice = Some(format!(
                "{agent} is still answering your last question about this change"
            ));
            return;
        }
        self.ask.draft = Some(AskDraft {
            key,
            entry,
            files,
            about,
            text: String::new(),
        });
    }

    /// Keys while a question from a diff is being written: text edits it,
    /// `⏎` sends it, `esc` drops it.
    pub(super) fn on_ask_draft_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(draft) = self.ask.draft.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.ask.draft = None,
            KeyCode::Enter => return self.send_ask(),
            KeyCode::Backspace => {
                draft.text.pop();
            }
            KeyCode::Char('u') if ctrl => draft.text.clear(),
            KeyCode::Char('w') if ctrl => delete_word(&mut draft.text, char::is_whitespace),
            KeyCode::Char(c) if !ctrl => draft.text.push(c),
            _ => {}
        }
        Vec::new()
    }

    /// Sends the question being written, unless it is empty, and shows the
    /// answer panel while the agent works on it.
    fn send_ask(&mut self) -> Vec<Effect> {
        let Some(draft) = self.ask.draft.as_ref() else {
            return Vec::new();
        };
        let question = draft.text.trim().to_owned();
        if question.is_empty() {
            self.notice = Some("Write a question first".to_owned());
            return Vec::new();
        }
        let Some(AskDraft {
            key,
            entry,
            files,
            about,
            ..
        }) = self.ask.draft.take()
        else {
            return Vec::new();
        };
        let explanation = self.explain.explanation(key);
        // The questions so far: the explanation holds every one answered
        // while vert runs; without one, those of this panel.
        let earlier = explanation.as_ref().map_or_else(
            || {
                self.answer_view(key)
                    .map(|v| v.answered.clone())
                    .unwrap_or_default()
            },
            |e| e.questions.clone(),
        );
        let effect = Effect::Ask(Box::new(Ask {
            key,
            entry: entry.clone(),
            files: files.clone(),
            explanation,
            earlier,
            about: Some(about.clone()),
            question: question.clone(),
        }));
        self.explain.start_question(key, self.now);
        let agent = match &self.explain.agent {
            Some(Ok(agent)) => agent.clone(),
            _ => "the agent".to_owned(),
        };
        let pending = Pending {
            question,
            place: about.place.clone(),
            since: self.now,
        };
        if !matches!(self.views.last(), Some(View::Answer(v)) if v.key == key) {
            let view = self
                .ask
                .parked
                .take_if(|v| v.key == key)
                .unwrap_or_else(|| {
                    Box::new(AnswerView {
                        key,
                        entry,
                        files,
                        agent: agent.clone(),
                        about: about.clone(),
                        answered: Vec::new(),
                        pending: None,
                        failed: None,
                        target: 0,
                        scroll: 0,
                        follow: false,
                        height: 0,
                        rows: 0,
                    })
                });
            self.views.push(View::Answer(view));
        }
        if let Some(View::Answer(v)) = self.views.last_mut() {
            v.agent = agent;
            v.about = about;
            v.pending = Some(pending);
            v.failed = None;
            v.scroll = usize::MAX;
            v.follow = false;
        }
        vec![effect]
    }

    /// The answer panel of the diff `key`, open or parked.
    fn answer_view(&self, key: u64) -> Option<&AnswerView> {
        self.views
            .iter()
            .filter_map(|v| match v {
                View::Answer(a) if a.key == key => Some(a.as_ref()),
                _ => None,
            })
            .chain(self.ask.parked.as_deref().filter(|v| v.key == key))
            .next()
    }

    /// The agent answered a question about the diff `key`: the answer
    /// panel of that diff, if there is one, shows it. Returns whether there
    /// is one, and whether it is on screen.
    pub(super) fn on_panel_answer(
        &mut self,
        key: u64,
        result: &Result<Question, String>,
    ) -> Option<bool> {
        let on_top = matches!(self.views.last(), Some(View::Answer(v)) if v.key == key);
        let open = self.views.iter_mut().filter_map(|v| match v {
            View::Answer(a) if a.key == key => Some(a.as_mut()),
            _ => None,
        });
        let mut found = false;
        for view in open.chain(self.ask.parked.as_deref_mut().filter(|v| v.key == key)) {
            found = true;
            view.pending = None;
            match result {
                Ok(q) => {
                    view.answered.push(q.clone());
                    view.failed = None;
                    view.show_newest();
                }
                // The agent's own error output: untrusted, like all it prints.
                Err(why) => view.failed = Some(sanitize(why)),
            }
        }
        found.then_some(on_top)
    }

    /// `A` in a diff: opens the answer panel of this diff again.
    pub(super) fn reopen_answers(&mut self) -> Vec<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return Vec::new();
        };
        let key = diff_key(&d.entry, &d.files);
        match self.ask.parked.take_if(|v| v.key == key) {
            Some(view) => self.views.push(View::Answer(view)),
            None => {
                self.notice =
                    Some("No questions asked about this diff yet: press a to ask one".to_owned());
            }
        }
        Vec::new()
    }

    /// Leaves the answer panel on top, keeping it to come back to.
    pub(super) fn close_answers(&mut self) {
        if let Some(View::Answer(v)) = self.views.pop() {
            self.ask.parked = Some(v);
        }
    }

    /// Keys of the answer panel. `esc` and `?` are handled before this.
    pub(super) fn on_answer_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(View::Answer(v)) = self.views.last_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = isize::try_from(half_page(v.height)).unwrap_or(1);
        let bottom = max_scroll(v.rows, v.height);
        let scroll = |v: &mut AnswerView, delta: isize| {
            v.follow = false;
            v.scroll = v
                .scroll
                .min(bottom)
                .saturating_add_signed(delta)
                .min(bottom);
        };
        match key.code {
            KeyCode::Char('a') => {
                let (key, entry, files, about) =
                    (v.key, v.entry.clone(), v.files.clone(), v.about.clone());
                self.start_ask(key, entry, files, about);
            }
            KeyCode::Char('j') | KeyCode::Down => v.select(1),
            KeyCode::Char('k') | KeyCode::Up => v.select(-1),
            KeyCode::Char('g') | KeyCode::Home => v.select(isize::MIN),
            KeyCode::Char('G') | KeyCode::End => v.select(isize::MAX),
            KeyCode::Char('J') => scroll(v, 1),
            KeyCode::Char('K') => scroll(v, -1),
            KeyCode::Char('d') if ctrl => scroll(v, half),
            KeyCode::Char('u') if ctrl => scroll(v, -half),
            KeyCode::PageDown => scroll(v, half * 2),
            KeyCode::PageUp => scroll(v, -half * 2),
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(r) = v.targets().get(v.target).map(|r| (*r).clone()) {
                    self.close_answers();
                    self.show_in_diff(&r);
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
