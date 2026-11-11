//! Fixes: a coding agent already set up on the machine acts on one piece of
//! review feedback, editing files in the checkout of the line of work under
//! review.
//!
//! vert sends the agent the change it is looking at, the place the feedback
//! is about (a file and line, with the hunk around it) and the feedback in
//! the reviewer's words. The agent edits files and never commits, so its
//! edits are that checkout's uncommitted changes, which an open diff of them
//! shows as they are made.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::explain::agent::{self, Access, Agent, Groups};
use crate::git::diff::FileDiff;
use crate::highlight::sanitize;

/// Bumped whenever the prompt changes.
const PROMPT_VERSION: &str = "vert-fix-2";
/// The line an agent starts its one-line summary with.
const SUMMARY: &str = "SUMMARY:";
/// Most characters of an agent's summary repeated to the user.
const SUMMARY_CHARS: usize = 300;

const INSTRUCTIONS: &str = r"A person is reviewing a code change in this repository, in the current directory, and asked you to act on one piece of feedback.

Rules:
- Do what the feedback asks with the smallest edit that does it. Don't refactor, reformat or touch unrelated code.
- Edit files in the current directory only. Don't run git commands that commit, stage, stash, reset, check out, clean, push or switch branches: the person reviews your edit as uncommitted changes, next to any they already have.
- The files on disk are the source of truth. The excerpt shows where the person was looking when they wrote the feedback; it may be out of date.
- If the feedback is unclear, wrong, or already addressed, change nothing and say why.
- End your answer with one line that starts with SUMMARY: and says what you changed, or why you changed nothing.";

/// One piece of feedback for an agent to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The change under review, as the person sees it: `"a1b2c3d Add login"`.
    pub change: String,
    /// Where the feedback points: `"src/auth.rs:42"`, a file, or `None`
    /// for the change as a whole.
    pub place: Option<String>,
    /// The hunk around that place, in diff form; empty when there is none.
    pub excerpt: String,
    /// The feedback, in the person's words.
    pub feedback: String,
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
}

impl Fixer {
    /// The agent, and its model when one was chosen, as shown to the user.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.model {
            Some(model) => format!("{} · {model}", self.agent.name()),
            None => self.agent.name().to_owned(),
        }
    }

    /// Asks the agent to act on `request`, editing files in `dir` and
    /// tracked in `groups`, and returns its one-line summary of what it did.
    ///
    /// # Errors
    /// Returns a message for the user when the agent fails or times out.
    pub fn fix(&self, request: &Request, dir: &Path, groups: &Groups) -> Result<String, String> {
        // pi can't be confined to `dir`. A custom command is the user's own
        // and runs unconfined; vert never configures one for fixes (see
        // `agent::select_editor`), and tests use one to fake an agent.
        if self.agent == Agent::Pi {
            return Err("pi can't be kept inside the checkout, so it doesn't fix".to_owned());
        }
        let how = agent::Run {
            access: Access::Edit,
            model: self.model.as_deref(),
            dir,
            timeout: self.timeout,
            groups,
        };
        let out = agent::run(&self.agent, how, &prompt(request, self.max_excerpt_bytes))
            .map_err(|e| e.to_string())?;
        Ok(summary(&out))
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
        };
        assert_eq!(fixer.label(), "--explain-cmd");
        assert_eq!(
            fixer.fix(&request(), dir.path(), &Groups::default()),
            Ok("wrote out.txt".to_owned())
        );
        assert!(dir.path().join("out.txt").exists());

        let failing = Fixer {
            agent: Agent::Custom("echo nope >&2; exit 2".to_owned()),
            model: Some("m".to_owned()),
            ..fixer
        };
        assert_eq!(failing.label(), "--explain-cmd · m");
        let err = failing
            .fix(&request(), dir.path(), &Groups::default())
            .expect_err("fails");
        assert!(err.ends_with("nope"), "{err}");

        let pi = Fixer {
            agent: Agent::Pi,
            ..failing
        };
        let refused = pi
            .fix(&request(), dir.path(), &Groups::default())
            .expect_err("pi");
        assert!(
            refused.contains("can't be kept inside the checkout"),
            "{refused}"
        );
    }
}
