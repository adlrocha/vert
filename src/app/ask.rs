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

use super::{App, Effect, View, delete_word, half_page, max_scroll};
use crate::explain::Ref;
use crate::explain::diff_key;
use crate::explain::question::{About, Ask, Question};
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::highlight::sanitize;

/// A question being written, before it goes to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskDraft {
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
    /// The agent asked, as shown to the user.
    pub agent: String,
}

/// A question answered in the panel, and the agent that answered it: the
/// agent can change between questions (`M`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    /// The question and its answer.
    pub question: Question,
    /// The agent that answered, as shown to the user.
    pub agent: String,
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
    /// Where a follow-up (`a`) points: where the last question did.
    pub about: About,
    /// Questions answered here, oldest first.
    pub answered: Vec<Answered>,
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
    /// An empty panel for the diff `key` of `entry`, asking about `about`.
    #[must_use]
    pub const fn new(key: u64, entry: Entry, files: Vec<FileDiff>, about: About) -> Self {
        Self {
            key,
            entry,
            files,
            about,
            answered: Vec::new(),
            pending: None,
            failed: None,
            target: 0,
            scroll: 0,
            follow: false,
            height: 0,
            rows: 0,
        }
    }

    /// Every answer's code pointers, oldest answer first.
    #[must_use]
    pub fn targets(&self) -> Vec<&Ref> {
        self.answered
            .iter()
            .flat_map(|a| &a.question.refs)
            .collect()
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
            .map(|a| a.question.refs.len())
            .sum();
        self.target = earlier.min(self.targets().len().saturating_sub(1));
        self.scroll = usize::MAX;
        self.follow = false;
    }
}

/// Questions asked from diffs: the one being written, and the answer
/// panels left, one per change, to come back to.
#[derive(Debug, Default)]
pub struct AskState {
    /// A question being written.
    pub draft: Option<AskDraft>,
    parked: Vec<AnswerView>,
}

impl AskState {
    /// Takes the parked answer panel of the change `id`, if there is one.
    fn unpark(&mut self, id: &str) -> Option<Box<AnswerView>> {
        let i = self.parked.iter().position(|v| v.entry.id == id)?;
        Some(Box::new(self.parked.remove(i)))
    }
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
        let about = super::selected_lines(d).map_or_else(
            || About {
                place: file.stat.path.clone(),
                excerpt: String::new(),
            },
            |(place, excerpt)| About { place, excerpt },
        );
        let (entry, files) = (d.entry.clone(), d.files.clone());
        self.start_ask(entry, files, about);
        Vec::new()
    }

    /// Starts writing a question about `about` in `entry`, whose diff is
    /// `files`, unless there is no agent to ask or it is still answering
    /// about that change.
    fn start_ask(&mut self, entry: Entry, files: Vec<FileDiff>, about: About) {
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
        let pending = self
            .answer_view(&entry.id)
            .is_some_and(|v| v.pending.is_some());
        if pending
            || self
                .explain
                .question_asked(diff_key(&entry, &files))
                .is_some()
        {
            self.notice = Some(format!(
                "{agent} is still answering your last question about this change"
            ));
            return;
        }
        self.ask.draft = Some(AskDraft {
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
            entry,
            files,
            about,
            ..
        }) = self.ask.draft.take()
        else {
            return Vec::new();
        };
        // The diff as it is now: a live one (uncommitted work) may have
        // refreshed under the prompt. The place and lines stay as the
        // person saw them.
        let (entry, files) = self
            .views
            .iter()
            .rev()
            .find_map(View::as_detail)
            .filter(|d| d.entry.id == entry.id && !d.files.is_empty())
            .map_or((entry, files), |d| (d.entry.clone(), d.files.clone()));
        let key = diff_key(&entry, &files);
        let explanation = self.explain.explanation(key);
        // The questions so far: the explanation holds every one answered
        // while vert runs; without one, those of this panel (the agent's
        // run adds those kept from earlier runs).
        let earlier = explanation.as_ref().map_or_else(
            || {
                self.answer_view(&entry.id)
                    .map(|v| v.answered.iter().map(|a| a.question.clone()).collect())
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
            agent,
        };
        if !matches!(self.views.last(), Some(View::Answer(v)) if v.entry.id == entry.id) {
            let view = self.ask.unpark(&entry.id).unwrap_or_else(|| {
                Box::new(AnswerView::new(
                    key,
                    entry.clone(),
                    files.clone(),
                    about.clone(),
                ))
            });
            self.views.push(View::Answer(view));
        }
        if let Some(View::Answer(v)) = self.views.last_mut() {
            // Its answer comes back under this key.
            v.key = key;
            v.entry = entry;
            v.files = files;
            v.about = about;
            v.pending = Some(pending);
            v.failed = None;
            v.scroll = usize::MAX;
            v.follow = false;
        }
        vec![effect]
    }

    /// The answer panel of the change `id`, open or parked. It belongs to
    /// the change, not to one version of its diff, so a live diff that
    /// moved on keeps it.
    fn answer_view(&self, id: &str) -> Option<&AnswerView> {
        self.views
            .iter()
            .filter_map(|v| match v {
                View::Answer(a) if a.entry.id == id => Some(a.as_ref()),
                _ => None,
            })
            .chain(self.ask.parked.iter().filter(|v| v.entry.id == id))
            .next()
    }

    /// The agent answered a question about the diff `key`: the answer
    /// panel waiting for it, if there is one, shows it. Returns whether there
    /// is one, and whether it is on screen.
    pub(super) fn on_panel_answer(
        &mut self,
        key: u64,
        result: &Result<Question, String>,
    ) -> Option<bool> {
        // Only a panel waiting for this answer: one asked from the
        // explain view about the same diff isn't its.
        let waiting = |v: &AnswerView| v.key == key && v.pending.is_some();
        let on_top = matches!(self.views.last(), Some(View::Answer(v)) if waiting(v));
        let open = self.views.iter_mut().filter_map(|v| match v {
            View::Answer(a) if waiting(a) => Some(a.as_mut()),
            _ => None,
        });
        let mut found = false;
        let parked = self.ask.parked.iter_mut().filter(|v| waiting(v));
        for view in open.chain(parked) {
            let Some(pending) = view.pending.take() else {
                continue;
            };
            found = true;
            match result {
                Ok(q) => {
                    view.answered.push(Answered {
                        question: q.clone(),
                        agent: pending.agent,
                    });
                    view.failed = None;
                    view.show_newest();
                }
                // The agent's own error output: untrusted, like all it prints.
                Err(why) => view.failed = Some(sanitize(why)),
            }
        }
        found.then_some(on_top)
    }

    /// `A` in a diff: opens the answer panel of this change again.
    pub(super) fn reopen_answers(&mut self) -> Vec<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return Vec::new();
        };
        let id = d.entry.id.clone();
        match self.ask.unpark(&id) {
            Some(view) => self.views.push(View::Answer(view)),
            None => {
                self.notice =
                    Some("No questions asked about this change yet: press a to ask one".to_owned());
            }
        }
        Vec::new()
    }

    /// Leaves the answer panel on top, keeping it to come back to.
    pub(super) fn close_answers(&mut self) {
        if let Some(View::Answer(v)) = self.views.pop() {
            // One per change, so an answer on its way always has its panel.
            self.ask.parked.retain(|p| p.entry.id != v.entry.id);
            self.ask.parked.push(*v);
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
                let (entry, files, about) = (v.entry.clone(), v.files.clone(), v.about.clone());
                self.start_ask(entry, files, about);
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
