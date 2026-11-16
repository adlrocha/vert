//! Fixes: `f` in a diff or an explanation asks the coding agent to act on a
//! piece of feedback, in the checkout of the line of work under review.
//!
//! The feedback is typed (or taken from an explanation's concern) in a
//! small prompt. Sending it runs the agent in the background; an open diff
//! of the checkout's uncommitted changes shows its edits as they are made.
//! When it is done its edits are committed on the checkout's branch (or left
//! uncommitted, and the footer says why), and its one-line summary goes in
//! the footer.
//!
//! In an explanation, `space` marks concerns and `f` sends them all as one
//! request. Each concern sent shows how its fix is going, and `c` opens what
//! the last fix changed.

use std::fmt::Write as _;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, Focus, Item, View, concern_at, delete_word};
use crate::explain::{Note, Ref};
use crate::fix::{Done, Fixer, Request, excerpt, place};
use crate::git::Target;
use crate::git::branches::Branch;
use crate::git::commit::Outcome;
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;
use crate::git::short_id;
use crate::highlight::sanitize;

/// The concerns of one explained diff a fix is about: the diff's key and
/// each concern's [`concern_key`].
pub type Concerns = (u64, Vec<String>);

/// What a concern's fix status is kept by: its text and the code it points
/// at. Not its position, so the status stays with its concern when the
/// diff is explained again and the concerns reorder; not its text alone, so
/// two concerns worded alike about different code each keep their own.
///
/// ```
/// use vert::app::concern_key;
/// use vert::explain::{Note, Ref};
///
/// let at = |path: &str| Note {
///     text: "Unchecked index".into(),
///     refs: vec![Ref { path: path.into(), hunk: Some(0), label: String::new() }],
/// };
/// assert_ne!(concern_key(&at("a.rs")), concern_key(&at("b.rs")));
/// assert_eq!(concern_key(&at("a.rs")), concern_key(&at("a.rs")));
/// ```
#[must_use]
pub fn concern_key(note: &Note) -> String {
    note.refs.iter().fold(note.text.clone(), |mut key, r| {
        let _ = write!(key, "\n@{}", r.path);
        if let Some(hunk) = r.hunk {
            let _ = write!(key, "#{hunk}");
        }
        key
    })
}

/// How the fix asked for a concern went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConcernFix {
    /// The agent is working on it, since this Unix time.
    Fixing {
        /// When it was sent.
        since: i64,
    },
    /// The agent is done: what it said, and what became of its edits.
    Done(Done),
    /// The agent failed, for this reason (sanitized).
    Failed(String),
}

/// A fix that has finished: where, and what became of its edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The checkout it edited.
    pub root: Option<PathBuf>,
    /// That checkout, named for the user.
    pub checkout: String,
    /// What became of its edits, or why the agent failed (it may have
    /// edited nothing).
    pub outcome: Result<Outcome, String>,
}

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
    /// The concerns of an explanation it is about, if any.
    pub concerns: Option<Concerns>,
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
    /// When it was sent (Unix time).
    pub since: i64,
    /// The concerns of an explanation it is about, if any.
    pub concerns: Option<Concerns>,
}

/// Fixes: which agent makes them, the one being written and those running.
#[derive(Debug, Default)]
pub struct FixState {
    /// The agent's name, or why fixes are unavailable.
    pub agent: Option<Result<String, String>>,
    /// Whether the agent's edits can land outside the checkout it fixes.
    pub unconfined: bool,
    /// Feedback being written.
    pub draft: Option<Draft>,
    /// Fixes in progress, oldest first.
    pub running: Vec<Running>,
    /// The fix that finished last, which `c` opens.
    pub last: Option<Finished>,
    next: u64,
}

impl App {
    /// Sets the agent fixes come from, or why there is none.
    pub fn set_fixer(&mut self, fixer: &Result<Fixer, String>) {
        self.fix.agent = Some(fixer.as_ref().map(Fixer::label).map_err(Clone::clone));
        self.fix.unconfined = fixer.as_ref().is_ok_and(|f| !f.agent.confines_edits());
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
    /// `line` (or the whole file, or the whole change), prefilled with `text`
    /// and fixing `concerns`, if it comes from some.
    fn draft_fix(
        &mut self,
        entry: &Entry,
        (file, hunk, line): (Option<&FileDiff>, Option<usize>, Option<u32>),
        text: String,
        concerns: Option<Concerns>,
    ) {
        let place = file.map(|f| place(&f.stat.path, line));
        let excerpt = file.map(|f| excerpt(f, hunk)).unwrap_or_default();
        self.start_draft(entry, place, excerpt, text, concerns);
    }

    /// Starts writing feedback on `entry` about `place`, showing `excerpt`,
    /// prefilled with `text`, unless fixes are unavailable or `entry` has no
    /// checkout to fix.
    fn start_draft(
        &mut self,
        entry: &Entry,
        place: Option<String>,
        excerpt: String,
        text: String,
        concerns: Option<Concerns>,
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
        self.fix.draft = Some(Draft {
            root,
            checkout,
            request: Request {
                change: entry.headline(),
                place,
                excerpt,
                feedback: text,
            },
            concerns,
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
        self.draft_fix(&entry, (file.as_ref(), hunk, line), String::new(), None);
        Vec::new()
    }

    /// `f` in an explanation: feedback on the concerns marked with `space`,
    /// all in one request; else on the selected code pointer, prefilled with
    /// the selected concern.
    pub(super) fn draft_fix_from_explain(&mut self) -> Vec<Effect> {
        let Some(View::Explain(v)) = self.views.last() else {
            return Vec::new();
        };
        let entry = v.entry.clone();
        // The marked concerns this explanation has, in its order: a mark
        // names a concern by its key, never by position.
        let marked: Vec<&Note> = v.explanation().map_or_else(Vec::new, |e| {
            e.concerns
                .iter()
                .filter(|n| v.marked.contains(&concern_key(n)))
                .collect()
        });
        if !marked.is_empty() {
            let (text, excerpt) = crate::fix::concerns_request(&marked, &v.files);
            let keys = marked.iter().map(|n| concern_key(n)).collect();
            let concerns = Some((v.key, keys));
            self.start_draft(&entry, None, excerpt, text, concerns);
            return Vec::new();
        }
        let stop = v.stop();
        let target = stop.and_then(|s| s.pointer).cloned();
        let concern = match (v.item(), v.explanation()) {
            (Some(Item::Concerns), Some(e)) => concern_at(e, v.target),
            _ => None,
        };
        let text = concern.map(|(_, n)| n.text.clone()).unwrap_or_default();
        let concerns = concern.map(|(_, n)| (v.key, vec![concern_key(n)]));
        let file = target
            .as_ref()
            .and_then(|r: &Ref| v.files.iter().find(|f| f.stat.path == r.path))
            .cloned();
        let hunk = target.as_ref().and_then(|r| r.hunk);
        let line = file
            .as_ref()
            .zip(hunk)
            .and_then(|(f, h)| f.hunks.get(h)?.first_changed_line());
        self.draft_fix(&entry, (file.as_ref(), hunk, line), text, concerns);
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
        if let Some((key, concerns)) = &draft.concerns {
            self.explain
                .set_concern_fixes(*key, concerns, &ConcernFix::Fixing { since: self.now });
            for view in self.explain_views(*key) {
                view.marked.clear();
            }
        }
        self.fix.running.push(Running {
            id,
            root: draft.root.clone(),
            checkout: draft.checkout.clone(),
            since: self.now,
            concerns: draft.concerns,
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

    /// The agent finished fix `id`: says what it did and where its edits
    /// went, and reloads what it may have changed.
    pub(super) fn on_fixed(&mut self, id: u64, result: Result<Done, String>) -> Vec<Effect> {
        let Some(i) = self.fix.running.iter().position(|r| r.id == id) else {
            return Vec::new();
        };
        let run = self.fix.running.remove(i);
        let committed = matches!(&result, Ok(d) if matches!(d.outcome, Outcome::Committed { .. }));
        let concern = match &result {
            Ok(done) => ConcernFix::Done(done.clone()),
            Err(why) => ConcernFix::Failed(sanitize(why)),
        };
        if let Some((key, concerns)) = &run.concerns {
            self.explain.set_concern_fixes(*key, concerns, &concern);
        }
        self.fix.last = Some(Finished {
            root: run.root.clone(),
            checkout: run.checkout.clone(),
            outcome: match &result {
                Ok(done) => Ok(done.outcome.clone()),
                Err(why) => Err(sanitize(why)),
            },
        });
        // In an explanation, the keys to see the fix and explain again come
        // before the agent's words, which may be too long for the footer.
        // With nothing changed, or a failed agent, there is no fix for `c`
        // to show.
        let shown = matches!(&result, Ok(d) if d.outcome != Outcome::Unchanged);
        let hint = match self.views.last() {
            Some(View::Explain(_)) if !shown => " (r explain again)",
            Some(View::Explain(_)) => " (c changes, r explain again)",
            _ => "",
        };
        self.notice = Some(match result {
            Ok(Done { summary, outcome }) => match outcome {
                Outcome::Committed {
                    id,
                    branch,
                    stale_index,
                } => {
                    let index = if stale_index {
                        " (the index couldn't be updated, so git status may be off for the fixed files)"
                    } else {
                        ""
                    };
                    format!(
                        "Committed {} to {}{index}{hint}: {summary}",
                        short_id(id),
                        sanitize(&branch)
                    )
                }
                Outcome::Unchanged => {
                    format!("Nothing changed in {}{hint}: {summary}", run.checkout)
                }
                Outcome::Kept(why) => format!(
                    "Fixed in {}, left uncommitted ({}){hint}: {summary}",
                    run.checkout,
                    sanitize(&why)
                ),
            },
            // The agent's own error output: untrusted, like all it prints.
            Err(why) => format!("Couldn't fix in {}{hint}: {}", run.checkout, sanitize(&why)),
        });
        let mut effects = self.request_worktree();
        if committed {
            effects.extend(self.request_commits());
        }
        effects.extend(self.request_branches());
        effects
    }
}

impl App {
    /// `c` in an explanation: opens what the last fix changed, its commit or
    /// the uncommitted changes of the checkout it edited.
    pub(super) fn open_last_fix(&mut self) -> Vec<Effect> {
        let Some(last) = self.fix.last.as_ref() else {
            self.notice = Some("No fix has finished yet".to_owned());
            return Vec::new();
        };
        let entry = match &last.outcome {
            Err(_) => {
                self.notice = Some(format!(
                    "The last fix in {} failed: there is nothing of it to show",
                    last.checkout
                ));
                return Vec::new();
            }
            Ok(Outcome::Unchanged) => {
                self.notice = Some(format!("The last fix in {} changed nothing", last.checkout));
                return Vec::new();
            }
            Ok(Outcome::Committed { id, .. }) => {
                let id = *id;
                let known = self
                    .feed
                    .commits
                    .iter()
                    .find(|e| e.target == Target::Commit(id))
                    .cloned();
                known.unwrap_or_else(|| {
                    let mut entry = Entry::blank(Target::Commit(id));
                    entry.root.clone_from(&last.root);
                    entry
                })
            }
            Ok(Outcome::Kept(_)) => {
                let dirty = match &last.root {
                    // Its files and totals come with the diff, read from
                    // disk, even before a scan has seen the fix's edits.
                    None => Some(
                        self.feed
                            .worktree
                            .clone()
                            .unwrap_or_else(Entry::uncommitted),
                    ),
                    // The last scan may predate the fix's edits, and linked
                    // worktrees aren't watched: the diff is read from disk
                    // as it opens either way.
                    Some(root) => self
                        .branches
                        .list
                        .branches
                        .iter()
                        .filter_map(|b| b.worktree.as_ref())
                        .find(|w| &w.path == root)
                        .map(|w| {
                            w.dirty.clone().unwrap_or_else(|| {
                                // Its files and totals come with the diff.
                                let mut e = Entry::uncommitted();
                                e.for_linked_worktree(root, &w.name);
                                e
                            })
                        }),
                };
                let Some(entry) = dirty else {
                    self.notice = Some(format!("{} has no uncommitted changes", last.checkout));
                    return Vec::new();
                };
                entry
            }
        };
        vec![Effect::LoadDiff {
            entry: Box::new(entry),
            refresh: false,
            file: None,
        }]
    }
}

#[cfg(test)]
mod tests;
