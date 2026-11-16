//! Fixes: a coding agent already set up on the machine acts on one piece of
//! review feedback, editing files in the checkout of the line of work under
//! review.
//!
//! vert sends the agent the change it is looking at, the place the feedback
//! is about (a file and line, with the hunk around it) and the feedback in
//! the reviewer's words. The agent edits files and never commits. Once it is
//! done, vert commits what it changed (and only that) on the checkout's
//! branch, unless that is turned off or can't be done safely (see
//! [`crate::git::commit`]); the edits then stay uncommitted.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::explain::Note;
use crate::explain::agent::{self, Access, Agent, Groups};
use crate::git::commit::{self, Outcome};
use crate::git::diff::FileDiff;
use crate::highlight::sanitize;

/// Bumped whenever the prompt changes.
const PROMPT_VERSION: &str = "vert-fix-4";
/// The line an agent starts its one-line summary with.
const SUMMARY: &str = "SUMMARY:";
/// Most characters of an agent's summary repeated to the user.
const SUMMARY_CHARS: usize = 300;
/// Most characters of a fix commit's subject line, `fix: ` included: git's
/// usual width, so logs and pull requests show it whole.
const SUBJECT_CHARS: usize = 72;

const INSTRUCTIONS: &str = r"A person is reviewing a code change in this repository, in the current directory, and asked you to act on their feedback: one request, or a numbered list of concerns, each naming the places it is about.

Rules:
- Do what the feedback asks with the smallest edit that does it. Don't refactor, reformat or touch unrelated code.
- When the feedback is a numbered list, deal with every concern in it, not just the first.
- Edit files in the current directory only. Don't run git commands that commit, stage, stash, reset, check out, clean, push or switch branches: the person reviews your edits once you are done, next to any uncommitted work they already have.
- The files on disk are the source of truth. The excerpt shows where the person was looking when they wrote the feedback; it may be out of date.
- If the feedback (or one concern in the list) is unclear, wrong, or already addressed, leave it and say why.
- End your answer with one line that starts with SUMMARY: and says what you changed, or why you changed nothing; for a numbered list, say it for each concern by its number.";

/// One piece of feedback for an agent to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The change under review, as the person sees it: `"a1b2c3d Add login"`.
    pub change: String,
    /// Where the feedback points (see [`place`]): `"src/auth.rs:42"`, a
    /// file, or `None` for the change as a whole, as for several concerns.
    pub place: Option<String>,
    /// The hunk around that place, in diff form, or for several concerns
    /// (see [`concerns_request`]) each hunk they point at under its file's
    /// `diff --git` header; empty when there is none.
    pub excerpt: String,
    /// The feedback, in the person's words.
    pub feedback: String,
}

/// A place in the change as the agent is told it: `path:line`, or the
/// path alone for a whole file.
///
/// ```
/// assert_eq!(vert::fix::place("src/a.rs", Some(42)), "src/a.rs:42");
/// assert_eq!(vert::fix::place("src/a.rs", None), "src/a.rs");
/// ```
#[must_use]
pub fn place(path: &str, line: Option<u32>) -> String {
    line.map_or_else(|| path.to_owned(), |l| format!("{path}:{l}"))
}

/// The feedback and excerpt asking to fix several `concerns` of a change
/// whose diff is `files`: the concerns as a numbered list, each with the
/// places it points at, and each hunk there once, under its file's header.
#[must_use]
pub fn concerns_request(concerns: &[&Note], files: &[FileDiff]) -> (String, String) {
    let mut text =
        "Deal with each of these concerns a reviewer raised about the change:\n".to_owned();
    // Each file pointed at, in order, with the hunks pointed at in it.
    let mut shown: Vec<(&FileDiff, Vec<Option<usize>>)> = Vec::new();
    for (n, note) in concerns.iter().enumerate() {
        let _ = write!(text, "\n{}. {}", n + 1, note.text.trim());
        let places: Vec<String> = note
            .refs
            .iter()
            .map(|r| {
                let file = files.iter().find(|f| f.stat.path == r.path);
                let line = file
                    .zip(r.hunk)
                    .and_then(|(f, h)| f.hunks.get(h)?.first_changed_line());
                if let Some(f) = file {
                    match shown.iter_mut().find(|(s, _)| s.stat.path == r.path) {
                        Some((_, hunks)) if !hunks.contains(&r.hunk) => hunks.push(r.hunk),
                        Some(_) => {}
                        None => shown.push((f, vec![r.hunk])),
                    }
                }
                place(&r.path, line)
            })
            .collect();
        if !places.is_empty() {
            let _ = write!(text, " (at {})", places.join(", "));
        }
    }
    let mut excerpts = String::new();
    for (f, hunks) in shown {
        let _ = writeln!(excerpts, "diff --git a/{0} b/{0}", f.stat.path);
        // The whole file covers any of its hunks.
        if hunks.contains(&None) {
            excerpts.push_str(&excerpt(f, None));
        } else {
            for hunk in hunks {
                excerpts.push_str(&excerpt(f, hunk));
            }
        }
    }
    (text, excerpts)
}

/// The hunk `hunk` of `file` in diff form (`+`, `-` and ` ` prefixes), or
/// every hunk of the file when `hunk` is `None`.
///
/// ```
/// let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
/// let files = vert::git::diff::parse(diff).unwrap();
/// assert_eq!(vert::fix::excerpt(&files[0], Some(0)), "@@ -1 +1 @@\n-old\n+new\n");
/// ```
#[must_use]
pub fn excerpt(file: &FileDiff, hunk: Option<usize>) -> String {
    let hunks: Vec<_> = match hunk {
        Some(i) => file.hunks.get(i).into_iter().collect(),
        None => file.hunks.iter().collect(),
    };
    hunks.iter().fold(String::new(), |mut out, h| {
        out.push_str(&h.header);
        out.push('\n');
        out.push_str(&h.body());
        out
    })
}

/// Builds the prompt for `request`, keeping at most `max_excerpt_bytes` of
/// its excerpt (cut at a line boundary); the agent can read the file itself.
#[must_use]
pub fn prompt(request: &Request, max_excerpt_bytes: usize) -> String {
    let mut out = format!("{PROMPT_VERSION}\n\n{INSTRUCTIONS}\n\n# The change\n\n");
    let _ = writeln!(out, "{}", request.change.trim());
    out.push_str("\n# Where\n\n");
    let _ = writeln!(
        out,
        "{}",
        request.place.as_deref().unwrap_or("The change as a whole")
    );
    if !request.excerpt.is_empty() {
        let mut kept = String::new();
        let mut cut = false;
        for line in request.excerpt.split_inclusive('\n') {
            if kept.len() + line.len() > max_excerpt_bytes {
                cut = true;
                break;
            }
            kept.push_str(line);
        }
        out.push_str("\n```diff\n");
        out.push_str(&kept);
        if !kept.ends_with('\n') && !kept.is_empty() {
            out.push('\n');
        }
        out.push_str("```\n");
        if cut {
            out.push_str("(the rest of the excerpt was left out to save space: read the file)\n");
        }
    }
    out.push_str("\n# Feedback\n\n");
    let _ = writeln!(out, "{}", request.feedback.trim());
    out
}

/// What the agent said it did: its `SUMMARY:` line, else its last line, on
/// one line and safe to show in the terminal.
///
/// ```
/// assert_eq!(vert::fix::summary("Edited a.rs\nSUMMARY: Renamed x to y\n"), "Renamed x to y");
/// assert_eq!(vert::fix::summary("done\n\n"), "done");
/// assert_eq!(vert::fix::summary(" "), "the agent said nothing");
/// ```
#[must_use]
pub fn summary(out: &str) -> String {
    let lines: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tagged = lines.iter().rev().find_map(|l| {
        l.get(..SUMMARY.len())
            .filter(|head| head.eq_ignore_ascii_case(SUMMARY))
            .map(|_| l[SUMMARY.len()..].trim())
    });
    let text = sanitize(tagged.or_else(|| lines.last().copied()).unwrap_or_default())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return "the agent said nothing".to_owned();
    }
    if text.chars().count() <= SUMMARY_CHARS {
        return text;
    }
    let kept: String = text.chars().take(SUMMARY_CHARS).collect();
    format!("{kept}…")
}

/// The commit message for `request` fixed by `agent`, which said `summary`:
/// a `fix:` subject from the summary, the feedback in the body, and the
/// agent as a co-author when it has an address to credit.
///
/// ```
/// use vert::explain::agent::Agent;
/// use vert::fix::{Request, message};
/// let request = Request {
///     change: "a1b2c3d Add login".into(),
///     place: Some("src/auth.rs:42".into()),
///     excerpt: String::new(),
///     feedback: "Use a named constant".into(),
/// };
/// let m = message(&request, &Agent::Claude, "Named the timeout");
/// assert!(m.starts_with("fix: Named the timeout\n\n"));
/// assert!(m.contains("Use a named constant"));
/// assert!(m.ends_with("Co-authored-by: Claude <noreply@anthropic.com>\n"));
/// ```
#[must_use]
pub fn message(request: &Request, agent: &Agent, summary: &str) -> String {
    let head = format!("fix: {}", summary.trim());
    let subject = if head.chars().count() <= SUBJECT_CHARS {
        head
    } else {
        let kept: String = head.chars().take(SUBJECT_CHARS - 1).collect();
        format!("{}…", kept.trim_end())
    };
    let mut out = format!("{subject}\n\n");
    let _ = writeln!(
        out,
        "{} acted on this feedback in vert, on {}{}:\n",
        agent.name(),
        request.change.trim(),
        request
            .place
            .as_deref()
            .map_or_else(String::new, |p| format!(" at {p}")),
    );
    let _ = writeln!(out, "{}\n", request.feedback.trim());
    let _ = writeln!(out, "It said: {}", summary.trim());
    if let Some(who) = agent.co_author() {
        let _ = write!(out, "\nCo-authored-by: {who}\n");
    }
    out
}

/// What came of a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// The agent's one-line summary of what it did.
    pub summary: String,
    /// What became of its edits: committed, none, or left uncommitted.
    pub outcome: Outcome,
}

/// How to ask for fixes.
#[derive(Debug, Clone)]
pub struct Fixer {
    /// The agent to ask.
    pub agent: Agent,
    /// The model the agent should use, or `None` for its own default.
    pub model: Option<String>,
    /// How long the agent may take.
    pub timeout: Duration,
    /// Excerpt bytes sent; the agent reads the rest itself.
    pub max_excerpt_bytes: usize,
    /// Whether the agent's edits are committed once it is done.
    pub commit: bool,
}

impl Fixer {
    /// The agent, and its model when one was chosen, as shown to the user;
    /// it says so when the agent's edits aren't kept inside the checkout.
    #[must_use]
    pub fn label(&self) -> String {
        self.agent.label(Access::Edit, self.model.as_deref())
    }

    /// Asks the agent to act on `request`, editing files in `dir` and
    /// tracked in `groups`, then commits its edits when [`Fixer::commit`]
    /// is on. Returns its one-line summary and what became of the edits.
    ///
    /// # Errors
    /// Returns a message for the user when the agent fails or times out.
    /// Its edits then stay uncommitted.
    pub fn fix(&self, request: &Request, dir: &Path, groups: &Groups) -> Result<Done, String> {
        // Taken before the agent starts, so only what changes from here on
        // is committed; without it the edits stay uncommitted.
        let before = self.commit.then(|| commit::snapshot(dir));
        // A custom command is the user's own and runs unconfined; vert never
        // configures one for fixes (see `agent::select_editor`), and tests
        // use one to fake an agent.
        let how = agent::Run {
            access: Access::Edit,
            model: self.model.as_deref(),
            dir,
            timeout: self.timeout,
            groups,
        };
        let out = agent::run(&self.agent, how, &prompt(request, self.max_excerpt_bytes))
            .map_err(|e| e.to_string())?;
        let summary = summary(&out);
        let outcome = match before {
            None => Outcome::Kept("committing fixes is off with --no-fix-commit".to_owned()),
            Some(Err(e)) => Outcome::Kept(format!(
                "couldn't record the checkout first: {}",
                crate::jobs::describe(&e)
            )),
            Some(Ok(before)) => {
                commit::commit_edits(dir, &before, &message(request, &self.agent, &summary))
                    .unwrap_or_else(|e| Outcome::Kept(crate::jobs::describe(&e)))
            }
        };
        Ok(Done { summary, outcome })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            change: "a1b2c3d Add login".to_owned(),
            place: Some("src/auth.rs:42".to_owned()),
            excerpt: "@@ -40,3 +40,3 @@\n ctx\n-old\n+new\n".to_owned(),
            feedback: "  Use a named constant here  ".to_owned(),
        }
    }

    #[test]
    fn the_prompt_carries_the_change_place_excerpt_and_feedback() {
        let p = prompt(&request(), 1 << 10);
        assert!(p.starts_with(PROMPT_VERSION));
        // Marked concerns go as one numbered list: each must be handled.
        assert!(p.contains("deal with every concern in it"));
        assert!(p.contains("Don't run git commands that commit, stage, stash, reset"));
        assert!(p.contains("# The change\n\na1b2c3d Add login\n"));
        assert!(p.contains(
            "# Where\n\nsrc/auth.rs:42\n\n```diff\n@@ -40,3 +40,3 @@\n ctx\n-old\n+new\n```\n"
        ));
        assert!(
            p.ends_with("# Feedback\n\nUse a named constant here\n"),
            "{p}"
        );
        assert!(!p.contains("left out"));

        let whole = prompt(
            &Request {
                place: None,
                excerpt: String::new(),
                ..request()
            },
            1 << 10,
        );
        assert!(whole.contains("# Where\n\nThe change as a whole\n\n# Feedback"));
        assert!(!whole.contains("```"));
    }

    #[test]
    fn long_excerpts_are_cut_at_a_line() {
        let p = prompt(&request(), 23);
        assert!(p.contains("```diff\n@@ -40,3 +40,3 @@\n ctx\n```\n"), "{p}");
        assert!(p.contains("left out to save space"));
        let none = prompt(&request(), 1);
        assert!(none.contains("```diff\n```\n"), "{none}");
        let unterminated = prompt(
            &Request {
                excerpt: "+one\n+two".to_owned(),
                ..request()
            },
            1 << 10,
        );
        assert!(unterminated.contains("+one\n+two\n```"), "{unterminated}");
    }

    #[test]
    fn excerpts_come_from_the_diff() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n@@ -9 +9 @@\n-x\n+y\n";
        let files = crate::git::diff::parse(diff).expect("diff");
        assert_eq!(excerpt(&files[0], Some(1)), "@@ -9 +9 @@\n-x\n+y\n");
        assert_eq!(
            excerpt(&files[0], None),
            "@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n@@ -9 +9 @@\n-x\n+y\n"
        );
        assert_eq!(excerpt(&files[0], Some(5)), "");
    }

    #[test]
    fn several_concerns_go_as_a_numbered_list_with_each_hunk_once() {
        use crate::explain::Ref;
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n@@ -9 +9 @@\n-x\n+y\n";
        let files = crate::git::diff::parse(diff).expect("diff");
        let at = |path: &str, hunk| Ref {
            path: path.to_owned(),
            hunk,
            label: String::new(),
        };
        let first = Note {
            text: " Off by one ".to_owned(),
            refs: vec![at("a.rs", Some(1)), at("gone.rs", None)],
        };
        let second = Note {
            text: "Same file".to_owned(),
            refs: vec![at("a.rs", Some(1)), at("a.rs", Some(0))],
        };
        let third = Note {
            text: "No code".to_owned(),
            refs: Vec::new(),
        };
        let (text, excerpt) = concerns_request(&[&first, &second, &third], &files);
        assert_eq!(
            text,
            "Deal with each of these concerns a reviewer raised about the change:\n\
             \n1. Off by one (at a.rs:9, gone.rs)\
             \n2. Same file (at a.rs:9, a.rs:2)\
             \n3. No code"
        );
        // One header per file, each hunk once.
        assert_eq!(
            excerpt,
            "diff --git a/a.rs b/a.rs\n@@ -9 +9 @@\n-x\n+y\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n"
        );
        let whole = Note {
            text: "Whole".to_owned(),
            refs: vec![at("a.rs", Some(1)), at("a.rs", None)],
        };
        let (_, excerpt_whole) = concerns_request(&[&whole], &files);
        assert_eq!(
            excerpt_whole,
            "diff --git a/a.rs b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n@@ -9 +9 @@\n-x\n+y\n"
        );
        // And the prompt carries both, with no single place.
        let p = prompt(
            &Request {
                place: None,
                excerpt,
                feedback: text,
                ..request()
            },
            1 << 20,
        );
        assert!(p.contains("1. Off by one") && p.contains("diff --git a/a.rs"));
    }

    #[test]
    fn summaries_are_one_safe_line() {
        assert_eq!(summary("a\nsummary:   did it  \nafter"), "did it");
        assert_eq!(summary("SUMMARY: first\nSUMMARY: last"), "last");
        assert_eq!(summary("plain\x1b[31m red"), "plain·[31m red");
        let long = summary(&format!("SUMMARY: {}", "z".repeat(400)));
        assert_eq!(long.chars().count(), SUMMARY_CHARS + 1);
        assert!(long.ends_with('…'));
        assert_eq!(summary("SUMMARY:"), "the agent said nothing");
    }

    #[test]
    fn commit_messages_credit_the_feedback_and_the_agent() {
        let m = message(&request(), &Agent::Claude, " Named the timeout ");
        assert_eq!(
            m,
            "fix: Named the timeout\n\n\
             claude acted on this feedback in vert, on a1b2c3d Add login at src/auth.rs:42:\n\n\
             Use a named constant here\n\n\
             It said: Named the timeout\n\n\
             Co-authored-by: Claude <noreply@anthropic.com>\n"
        );
        let whole = message(
            &Request {
                place: None,
                ..request()
            },
            &Agent::Codex,
            &"word ".repeat(30),
        );
        let subject = whole.lines().next().expect("subject");
        assert_eq!(subject.chars().count(), SUBJECT_CHARS, "{subject}");
        assert!(subject.ends_with('…'), "{subject}");
        assert!(whole.contains("on a1b2c3d Add login:\n"), "{whole}");
        assert!(
            !whole.contains("Co-authored-by"),
            "codex has no address to credit"
        );
    }

    #[test]
    fn fixes_run_the_agent_in_the_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixer = Fixer {
            agent: Agent::Custom(
                "grep -q 'named constant' && echo edited > out.txt && echo 'SUMMARY: wrote out.txt'"
                    .to_owned(),
            ),
            model: None,
            timeout: Duration::from_secs(10),
            max_excerpt_bytes: 1 << 10,
            commit: false,
        };
        assert_eq!(fixer.label(), "--explain-cmd, unconfined");
        assert_eq!(
            fixer.fix(&request(), dir.path(), &Groups::default()),
            Ok(Done {
                summary: "wrote out.txt".to_owned(),
                outcome: Outcome::Kept("committing fixes is off with --no-fix-commit".to_owned()),
            })
        );
        assert!(dir.path().join("out.txt").exists());

        let failing = Fixer {
            agent: Agent::Custom("echo nope >&2; exit 2".to_owned()),
            model: Some("m".to_owned()),
            ..fixer
        };
        assert_eq!(failing.label(), "--explain-cmd, unconfined · m");
        let err = failing
            .fix(&request(), dir.path(), &Groups::default())
            .expect_err("fails");
        assert!(err.ends_with("nope"), "{err}");

        let pi = Fixer {
            agent: Agent::Pi,
            ..failing
        };
        assert_eq!(pi.label(), "pi, unconfined · m", "the user is told");
    }
}
