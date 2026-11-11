//! Fixes: `f` in a diff or an explanation asks the coding agent to act on a
//! piece of feedback, in the checkout of the line of work under review.
//!
//! The feedback is typed (or taken from an explanation's concern) in a
//! small prompt. Sending it runs the agent in the background; its edits are
//! the checkout's uncommitted changes, which an open diff of them shows as
//! they are made, and its one-line summary goes in the footer.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, Focus, Item, View, concern_at, delete_word};
use crate::explain::Ref;
use crate::fix::{Fixer, Request, excerpt};
use crate::git::Target;
use crate::git::branches::Branch;
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::highlight::sanitize;

/// Feedback being written, before it goes to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The checkout the agent will edit: a linked worktree, or `None` for
    /// vert's own.
    pub root: Option<PathBuf>,
    /// That checkout, named for the user.
    pub checkout: String,
    /// What the agent will be told: the change, where, and the feedback
    /// as typed so far (trimmed on sending).
    pub request: Request,
}

/// A fix the agent is working on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// Identifies the request.
    pub id: u64,
    /// The checkout being edited.
    pub root: Option<PathBuf>,
    /// That checkout, named for the user.
    pub checkout: String,
}

/// Fixes: which agent makes them, the one being written and those running.
#[derive(Debug, Default)]
pub struct FixState {
    /// The agent's name, or why fixes are unavailable.
    pub agent: Option<Result<String, String>>,
    /// Feedback being written.
    pub draft: Option<Draft>,
    /// Fixes in progress, oldest first.
    pub running: Vec<Running>,
    next: u64,
}

impl App {
    /// Sets the agent fixes come from, or why there is none.
    pub fn set_fixer(&mut self, fixer: &Result<Fixer, String>) {
        self.fix.agent = Some(fixer.as_ref().map(Fixer::label).map_err(Clone::clone));
    }

    /// The checkout a fix to `entry` goes to, and its name for the user.
    fn fix_checkout(&self, entry: &Entry) -> Result<(Option<PathBuf>, String), String> {
        let checkout_of = |b: &Branch| {
            b.worktree
                .as_ref()
                .map(|w| ((!w.current).then(|| w.path.clone()), w.name.clone()))
                .ok_or_else(|| {
                    format!(
                        "{} isn't checked out: check it out in a worktree to fix it there",
                        b.name
                    )
                })
        };
        let named = |root: &Option<PathBuf>| {
            root.as_ref().map_or_else(
                || "this checkout".to_owned(),
                |path| {
                    self.branches
                        .list
                        .branches
                        .iter()
                        .filter_map(|b| b.worktree.as_ref())
                        .find(|w| &w.path == path)
                        .map_or_else(|| path.display().to_string(), |w| w.name.clone())
                },
            )
        };
        // A branch's changes are a range, or a bare commit when it shares no
        // history with the base; either way the branch decides where it lives.
        if let Some(name) = entry.branch() {
            return self
                .branches
                .list
                .branches
                .iter()
                .find(|b| b.name == name)
                .map_or_else(
                    || Err(format!("{name} isn't a line of work vert knows")),
                    checkout_of,
                );
        }
        match entry.target {
            Target::WorkingTree | Target::Commit(_) => Ok((entry.root.clone(), named(&entry.root))),
            Target::Range { .. } => Err(format!("{} isn't a line of work vert knows", entry.short)),
            Target::PullRequest(number) => {
                if !self.branches.loaded {
                    return Err(self.branches.error.as_ref().map_or_else(
                        || "the lines of work are still loading; try again in a moment".to_owned(),
                        |e| format!("couldn't list the lines of work: {e}"),
                    ));
                }
                if !self.branches.pulls.iter().any(|p| p.number == number) {
                    return Err(format!(
                        "#{number} isn't an open pull request vert lists, so it can't tell its branch: open the branch (vert branch NAME) to fix it there"
                    ));
                }
                // Both lists have loaded, so the rows link the pull request
                // to its branch (never a fork's), whatever the tab's filters.
                let branch = self
                    .branches
                    .all_rows()
                    .into_iter()
                    .find(|r| r.pull.is_some_and(|p| p.number == number))
                    .and_then(|r| r.branch);
                branch.map_or_else(
                    || {
                        Err(format!(
                            "#{number} isn't checked out here: check out its branch in a worktree to fix it"
                        ))
                    },
                    checkout_of,
                )
            }
        }
    }

    /// Starts writing feedback on `entry`, about `file`'s hunk `hunk` at
    /// `line` (or the whole file, or the whole change), prefilled with `text`.
    fn draft_fix(
        &mut self,
        entry: &Entry,
        file: Option<&FileDiff>,
        hunk: Option<usize>,
        line: Option<u32>,
        text: String,
    ) {
        match &self.fix.agent {
            Some(Ok(_)) => {}
            Some(Err(why)) => {
                self.notice = Some(format!("Can't fix: {why}"));
                return;
            }
            None => {
                self.notice = Some("Can't fix: no agent is set up".to_owned());
                return;
            }
        }
        let (root, checkout) = match self.fix_checkout(entry) {
            Ok(found) => found,
            Err(why) => {
                self.notice = Some(format!("Can't fix: {why}"));
                return;
            }
        };
        let place = file.map(|f| match line {
            Some(line) => format!("{}:{line}", f.stat.path),
            None => f.stat.path.clone(),
        });
        self.fix.draft = Some(Draft {
            root,
            checkout,
            request: Request {
                change: entry.headline(),
                place,
                excerpt: file.map(|f| excerpt(f, hunk)).unwrap_or_default(),
                feedback: text,
            },
        });
    }

    /// `f` in a diff: feedback on the hunk at the top of the diff pane, or on
    /// the selected file from the file list.
    pub(super) fn draft_fix_from_detail(&mut self) -> Vec<Effect> {
        let Some(View::Detail(d)) = self.views.last() else {
            return Vec::new();
        };
        let entry = d.entry.clone();
        let file = d.files.get(d.pos.selected).cloned();
        // The hunk the top row is in, and a line of that same hunk: the row's
        // own line, or the hunk's first changed one when the row has none in
        // it (a separator, a trailing removed line).
        let (hunk, line) = match (&file, d.focus) {
            (Some(f), Focus::Diff) => {
                let hunk = d
                    .rendered
                    .hunk_starts
                    .iter()
                    .rposition(|&s| s <= d.scroll)
                    .unwrap_or(0);
                let line = f.hunks.get(hunk).and_then(|h| {
                    d.rendered
                        .new_line_at(d.scroll)
                        .filter(|&l| h.holds_new_line(l))
                        .or_else(|| h.first_changed_line())
                });
                (Some(hunk), line)
            }
            _ => (None, None),
        };
        self.draft_fix(&entry, file.as_ref(), hunk, line, String::new());
        Vec::new()
    }

    /// `f` in an explanation: feedback on the selected code pointer,
    /// prefilled with the selected concern.
    pub(super) fn draft_fix_from_explain(&mut self) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last() else {
            return Vec::new();
        };
        let target = v.targets().get(v.target).map(|r| (*r).clone());
        let text = match (v.item(), v.explanation()) {
            (Some(Item::Concerns), Some(e)) => concern_at(e, v.target)
                .map(|n| n.text.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let entry = v.entry.clone();
        let file = target
            .as_ref()
            .and_then(|r: &Ref| v.files.iter().find(|f| f.stat.path == r.path))
            .cloned();
        let hunk = target.as_ref().and_then(|r| r.hunk);
        let line = file
            .as_ref()
            .zip(hunk)
            .and_then(|(f, h)| f.hunks.get(h)?.first_changed_line());
        self.draft_fix(&entry, file.as_ref(), hunk, line, text);
        Vec::new()
    }

    /// Keys while feedback is being written: text edits it, `⏎` sends it,
    /// `esc` drops it.
    pub(super) fn on_draft_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(draft) = self.fix.draft.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.fix.draft = None,
            KeyCode::Enter => return self.send_fix(),
            KeyCode::Backspace => {
                draft.request.feedback.pop();
            }
            KeyCode::Char('u') if ctrl => draft.request.feedback.clear(),
            KeyCode::Char('w') if ctrl => {
                delete_word(&mut draft.request.feedback, char::is_whitespace);
            }
            KeyCode::Char(c) if !ctrl => draft.request.feedback.push(c),
            _ => {}
        }
        Vec::new()
    }

    /// Sends the feedback being written, unless it is empty or the agent is
    /// already editing that checkout.
    fn send_fix(&mut self) -> Vec<Effect> {
        let Some(draft) = self.fix.draft.as_ref() else {
            return Vec::new();
        };
        if draft.request.feedback.trim().is_empty() {
            self.notice = Some("Write what the agent should do first".to_owned());
            return Vec::new();
        }
        if self.fix.running.iter().any(|r| r.root == draft.root) {
            self.notice = Some(format!(
                "An agent is already fixing {}; wait for it to finish",
                draft.checkout
            ));
            return Vec::new();
        }
        let Some(draft) = self.fix.draft.take() else {
            return Vec::new();
        };
        self.fix.next += 1;
        let id = self.fix.next;
        self.fix.running.push(Running {
            id,
            root: draft.root.clone(),
            checkout: draft.checkout.clone(),
        });
        self.notice = Some(format!("Asked the agent to fix this in {}", draft.checkout));
        vec![Effect::Fix {
            id,
            root: draft.root,
            request: Request {
                feedback: draft.request.feedback.trim().to_owned(),
                ..draft.request
            },
        }]
    }

    /// The agent finished fix `id`: says what it did, and reloads what it
    /// may have changed.
    pub(super) fn on_fixed(&mut self, id: u64, result: Result<String, String>) -> Vec<Effect> {
        let Some(i) = self.fix.running.iter().position(|r| r.id == id) else {
            return Vec::new();
        };
        let done = self.fix.running.remove(i);
        self.notice = Some(match result {
            Ok(summary) => format!("Fixed in {}: {summary}", done.checkout),
            // The agent's own error output: untrusted, like all it prints.
            Err(why) => format!("Couldn't fix in {}: {}", done.checkout, sanitize(&why)),
        });
        let mut effects = self.request_worktree();
        effects.extend(self.request_branches());
        effects
    }
}

#[cfg(test)]
mod tests;
