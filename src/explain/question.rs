//! Follow-up questions about an explained change.
//!
//! A question is a fresh read-only run of the same agent. Its prompt carries
//! the change and its diff (as the explanation's did), the explanation the
//! agent gave, the questions asked so far with their answers, and the new
//! question, so every agent and `--explain-cmd` can answer it, and so can an
//! explanation read from the cache. The answer is one JSON object, a text
//! and the hunks it is about, checked against the diff like the
//! explanation's pointers.
//!
//! Questions belong to the change, not to one explanation of it: they are
//! kept in the answer cache under the exact diff, whichever agent answered,
//! and asking for a new explanation keeps them.

use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    Change, Explainer, Explanation, RawRef, Ref, Resolver, agent, clean, describe_change, diff_key,
    extract_json, fingerprint,
};
use crate::git::diff::FileDiff;
use crate::git::feed::Entry;

/// Names the question prompt's format, for whoever reads a prompt. Stored
/// questions don't depend on it, so it isn't part of their key.
const PROMPT_VERSION: &str = "vert-question-1";

/// Bumped whenever the way questions are stored changes.
const THREAD_VERSION: &str = "vert-questions-1";

const INSTRUCTIONS: &str = r#"You explained a code change in this repository to a person reviewing it. The change, your explanation and the questions they asked so far are below. Answer their new question.

You may read files in the current directory to answer. Do not modify anything. The diff below is the source of truth for what changed; the files on disk may be at a different version.

Answer with ONLY one JSON object, no prose and no code fences, in this shape:
{"answer": "string", "refs": [REF]}
REF is {"file": "path exactly as listed below", "hunk": hunk number as listed below}. Leave out "hunk" to mean the whole file.

Rules:
- answer: plain sentences that answer the question directly, as short as it allows. Separate paragraphs with a blank line. No markdown headings or tables.
- refs: the hunks the answer is about, so the person can jump to them. Use an empty list when it is about none.
- If the change and the repository don't answer the question, say so. Do not invent.
- Only refer to the files and hunks listed below."#;

/// A follow-up question to put to the agent, with all it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// Identifies the exact diff (see [`diff_key`]).
    pub key: u64,
    /// The change.
    pub entry: Entry,
    /// Its diff.
    pub files: Vec<FileDiff>,
    /// The explanation, with the questions asked so far.
    pub explanation: Explanation,
    /// The new question.
    pub question: String,
}

/// A follow-up question and the agent's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// What the person asked.
    pub question: String,
    /// The agent's answer, safe to draw.
    pub answer: String,
    /// The code the answer is about.
    pub refs: Vec<Ref>,
}

#[derive(Debug, Deserialize)]
struct RawAnswer {
    answer: Option<String>,
    #[serde(default)]
    refs: Vec<RawRef>,
}

/// A question as stored in the cache. Pointers are kept as the agent wrote
/// them and checked again on reading.
#[derive(Debug, Deserialize, Serialize)]
struct Saved {
    question: String,
    answer: String,
    #[serde(default)]
    refs: Vec<RawRef>,
}

impl Question {
    /// Parses an agent's answer to `question` and checks its pointers
    /// against `files`. An answer that isn't a JSON object with an
    /// `answer` is taken as prose and kept whole, without pointers; one
    /// whose `answer` is blank is refused.
    ///
    /// ```
    /// let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    /// let files = vert::git::diff::parse(diff).unwrap();
    /// let answer = r#"{"answer": "It renames.", "refs": [{"file": "a.rs", "hunk": 1}]}"#;
    /// let q = vert::explain::question::Question::validate("Why?", answer, &files).unwrap();
    /// assert_eq!(q.answer, "It renames.");
    /// assert_eq!(q.refs.len(), 1);
    /// ```
    ///
    /// # Errors
    /// Says so when the answer is empty.
    pub fn validate(question: &str, answer: &str, files: &[FileDiff]) -> Result<Self, String> {
        let mut resolve = Resolver { files, dropped: 0 };
        let parsed = extract_json(answer)
            .and_then(|json| serde_json::from_str::<RawAnswer>(json).ok())
            .and_then(|raw| Some((raw.answer?, raw.refs)));
        let (text, refs) = match parsed {
            Some((text, refs)) => (clean(&text), resolve.many(&refs)),
            None => (clean(answer), Vec::new()),
        };
        if text.is_empty() {
            return Err("the answer was empty".to_owned());
        }
        Ok(Self {
            question: clean(question),
            answer: text,
            refs,
        })
    }
}

/// Where the questions about one version of a change are kept.
fn thread_key(entry: &Entry, files: &[FileDiff]) -> u64 {
    let diff = format!("{:016x}", diff_key(entry, files));
    fingerprint([THREAD_VERSION, diff.as_str()])
}

/// Reads stored questions, checking their pointers against `files`. A
/// store that can't be read holds no questions.
fn load(saved: &str, files: &[FileDiff]) -> Vec<Question> {
    let Ok(saved) = serde_json::from_str::<Vec<Saved>>(saved) else {
        return Vec::new();
    };
    saved
        .into_iter()
        .filter_map(|s| {
            let mut resolve = Resolver { files, dropped: 0 };
            let answer = clean(&s.answer);
            (!answer.is_empty()).then(|| Question {
                question: clean(&s.question),
                answer,
                refs: resolve.many(&s.refs),
            })
        })
        .collect()
}

/// The questions as stored.
fn save(questions: &[Question]) -> serde_json::Result<String> {
    let saved: Vec<Saved> = questions
        .iter()
        .map(|q| Saved {
            question: q.question.clone(),
            answer: q.answer.clone(),
            refs: q.refs.iter().map(RawRef::from).collect(),
        })
        .collect();
    serde_json::to_string(&saved)
}

/// Pointers as the prompt names them: `[a.rs hunk 2, b.rs]`.
fn pointers(refs: &[Ref]) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let named: Vec<String> = refs
        .iter()
        .map(|r| match r.hunk {
            Some(h) => format!("{} hunk {}", r.path, h + 1),
            None => r.path.clone(),
        })
        .collect();
    format!(" [{}]", named.join(", "))
}

const fn change_name(change: Change) -> &'static str {
    match change {
        Change::Added => "added",
        Change::Changed => "changed",
        Change::Removed => "removed",
        Change::Same => "same",
    }
}

/// Appends the explanation, as the agent gave it, to `out`.
fn recap(out: &mut String, e: &Explanation) {
    let _ = writeln!(out, "Intent: {}", e.intent);
    if !e.steps.is_empty() {
        out.push_str("\nSteps:\n");
        for (i, s) in e.steps.iter().enumerate() {
            let _ = writeln!(
                out,
                "{}. {}: {}{}",
                i + 1,
                s.title,
                s.detail,
                pointers(&s.refs)
            );
        }
    }
    if let Some(d) = &e.diagram {
        let _ = writeln!(out, "\nDiagram ({}): {}", d.kind, d.title);
        for n in &d.nodes {
            let _ = writeln!(
                out,
                "- {} ({}){}",
                n.label,
                change_name(n.change),
                pointers(&n.refs)
            );
        }
        for edge in &d.edges {
            let label = |i: usize| d.nodes.get(i).map_or("?", |n| n.label.as_str());
            let _ = writeln!(
                out,
                "- {} -> {}: {} ({})",
                label(edge.from),
                label(edge.to),
                edge.label,
                change_name(edge.change)
            );
        }
    }
    if !e.concerns.is_empty() {
        out.push_str("\nConcerns:\n");
        for n in &e.concerns {
            let _ = writeln!(out, "- {}{}", n.text, pointers(&n.refs));
        }
    }
}

/// Appends as much of `text` as `budget` bytes allow to `out`, saying so
/// when some is left out, and takes what it used from `budget`.
fn push_within(out: &mut String, text: &str, budget: &mut usize) {
    if text.len() <= *budget {
        *budget -= text.len();
        out.push_str(text);
        return;
    }
    let end = (0..=*budget)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    out.push_str(&text[..end]);
    out.push_str("\n(the rest was left out to save space)\n");
    *budget = 0;
}

/// Builds the prompt for `question` about `entry`, whose diff is `files`
/// and which the agent explained as `explanation` (its questions so far
/// included). Hunk bodies stop once the diff passes `max_diff_bytes`; the
/// explanation and the earlier answers get the same budget again, the
/// newest answers first.
#[must_use]
pub fn prompt(
    entry: &Entry,
    files: &[FileDiff],
    max_diff_bytes: usize,
    explanation: &Explanation,
    question: &str,
) -> String {
    let mut out = String::with_capacity(max_diff_bytes.min(1 << 20) + INSTRUCTIONS.len() + 4096);
    out.push_str(PROMPT_VERSION);
    out.push_str("\n\n");
    out.push_str(INSTRUCTIONS);
    describe_change(&mut out, entry, files, max_diff_bytes);
    out.push_str("\n# Your explanation\n\n");
    let mut budget = max_diff_bytes;
    let mut said = String::new();
    recap(&mut said, explanation);
    push_within(&mut out, &said, &mut budget);
    if !explanation.questions.is_empty() {
        let mut replayed: Vec<String> = explanation
            .questions
            .iter()
            .rev()
            .map(|q| {
                format!(
                    "\nQ: {}\nA: {}{}\n",
                    q.question,
                    q.answer,
                    pointers(&q.refs)
                )
            })
            .take_while(|turn| {
                let fits = turn.len() <= budget;
                if fits {
                    budget -= turn.len();
                }
                fits
            })
            .collect();
        replayed.reverse();
        out.push_str("\n# Earlier questions\n");
        let left_out = explanation.questions.len() - replayed.len();
        if left_out > 0 {
            let _ = writeln!(
                out,
                "\n({left_out} earlier question{} left out to save space)",
                if left_out == 1 { " was" } else { "s were" }
            );
        }
        out.push_str(&replayed.concat());
    }
    let _ = write!(out, "\n# The question\n\n{}\n", question.trim());
    out
}

impl Explainer {
    /// The questions kept for this version of `entry`, whose diff is
    /// `files`, oldest first.
    #[must_use]
    pub fn questions(&self, entry: &Entry, files: &[FileDiff]) -> Vec<Question> {
        self.cache
            .as_ref()
            .and_then(|c| c.get(thread_key(entry, files)))
            .map(|saved| load(&saved, files))
            .unwrap_or_default()
    }

    /// Puts `asked` to the agent, running it read-only in `dir` and
    /// tracking it in `groups`. The answer joins the questions kept for the
    /// change, merged with any stored since `asked` was made.
    ///
    /// # Errors
    /// Returns a message for the user when the agent fails or answers
    /// nothing.
    pub fn ask(&self, asked: &Ask, dir: &Path, groups: &agent::Groups) -> Result<Question, String> {
        let Ask {
            entry,
            files,
            explanation,
            question,
            ..
        } = asked;
        let prompt = prompt(entry, files, self.max_diff_bytes, explanation, question);
        let how = agent::Run {
            access: agent::Access::ReadOnly,
            model: self.model.as_deref(),
            dir,
            timeout: self.timeout,
            groups,
        };
        let out = agent::run(&self.agent, how, &prompt).map_err(|e| e.to_string())?;
        let answered = Question::validate(question, &out, files)
            .map_err(|e| format!("{}: {e}", self.agent.name()))?;
        if let Some(cache) = &self.cache {
            let mut all = self.questions(entry, files);
            for q in explanation.questions.iter().chain([&answered]) {
                if !all.contains(q) {
                    all.push(q.clone());
                }
            }
            if let Ok(saved) = save(&all) {
                cache.put(thread_key(entry, files), &saved);
            }
        }
        Ok(answered)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::explain::tests::{entry, sample};
    use crate::explain::{Diagram, Edge, Node, Note, Step, cache};

    fn r(path: &str, hunk: Option<usize>) -> Ref {
        Ref {
            path: path.to_owned(),
            hunk,
            label: String::new(),
        }
    }

    fn explanation() -> Explanation {
        Explanation {
            intent: "Retry failed calls".to_owned(),
            steps: vec![Step {
                title: "Policy".to_owned(),
                detail: "Adds it".to_owned(),
                refs: vec![r("src/a.rs", Some(0))],
            }],
            diagram: Some(Diagram {
                kind: "flow".to_owned(),
                title: "Calls".to_owned(),
                nodes: vec![
                    Node {
                        label: "client".to_owned(),
                        change: Change::Changed,
                        refs: vec![r("src/b.rs", None)],
                    },
                    Node {
                        label: "api".to_owned(),
                        change: Change::Same,
                        refs: Vec::new(),
                    },
                ],
                edges: vec![Edge {
                    from: 0,
                    to: 1,
                    label: "retries".to_owned(),
                    change: Change::Added,
                }],
            }),
            concerns: vec![Note {
                text: "No backoff cap".to_owned(),
                refs: Vec::new(),
            }],
            unexplained: Vec::new(),
            dropped: 0,
            questions: Vec::new(),
        }
    }

    fn asking(e: &Explanation, question: &str) -> Ask {
        Ask {
            key: 1,
            entry: entry(),
            files: sample(),
            explanation: e.clone(),
            question: question.to_owned(),
        }
    }

    fn explainer(dir: &Path, script: &str) -> Explainer {
        Explainer {
            agent: agent::Agent::Custom(script.to_owned()),
            model: None,
            timeout: Duration::from_secs(10),
            max_diff_bytes: 1 << 20,
            cache: Some(cache::Cache::new(dir.join("cache"))),
        }
    }

    #[test]
    fn validates_answers_and_their_pointers() {
        let files = sample();
        let q = Question::validate(
            " Why? ",
            r#"Sure: {"answer": "Because.\n\nAlso this.", "refs": [{"file": "src/a.rs", "hunk": 2}, {"file": "nope.rs"}, {"file": "src/a.rs", "hunk": 9}]}"#,
            &files,
        )
        .expect("valid");
        assert_eq!(q.question, "Why?");
        assert_eq!(q.answer, "Because.\n\nAlso this.", "paragraphs survive");
        assert_eq!(
            q.refs,
            vec![make(&files, 0, Some(1))],
            "bad pointers dropped"
        );
    }

    fn make(files: &[FileDiff], file: usize, hunk: Option<usize>) -> Ref {
        super::super::make_ref(&files[file], hunk)
    }

    #[test]
    fn prose_answers_are_kept_and_empty_ones_refused() {
        let files = sample();
        let q = Question::validate("Why?", "  Just \u{1b}[31mbecause\t. ", &files).expect("prose");
        assert_eq!(q.answer, "Just ·[31mbecause    .", "sanitized");
        assert_eq!(q.refs, Vec::<Ref>::new());
        let braces = Question::validate("Why?", r#"It now sends {"n": 2} first."#, &files);
        assert_eq!(
            braces.map(|q| q.answer),
            Ok(r#"It now sends {"n": 2} first."#.to_owned()),
            "prose with braces is still prose"
        );
        let shapeless = Question::validate("Why?", r#"{"text": "x"}"#, &files);
        assert_eq!(
            shapeless.map(|q| q.answer),
            Ok(r#"{"text": "x"}"#.to_owned())
        );
        assert!(Question::validate("Why?", "  \n ", &files).is_err());
        assert!(Question::validate("Why?", r#"{"answer": " "}"#, &files).is_err());
    }

    #[test]
    fn prompts_carry_the_explanation_and_earlier_questions() {
        let mut e = explanation();
        let p = prompt(&entry(), &sample(), 1 << 20, &e, "  What about timeouts? ");
        assert!(p.starts_with(PROMPT_VERSION));
        assert!(p.contains("Add retries"), "the change");
        assert!(p.contains("--- hunk 1 @@ -1 +1 @@ fn a"), "the diff");
        assert!(p.contains("Intent: Retry failed calls"));
        assert!(p.contains("1. Policy: Adds it [src/a.rs hunk 1]"));
        assert!(p.contains("Diagram (flow): Calls"));
        assert!(p.contains("- client (changed) [src/b.rs]"));
        assert!(p.contains("- client -> api: retries (added)"));
        assert!(p.contains("- No backoff cap"));
        assert!(!p.contains("# Earlier questions"));
        assert!(p.ends_with("# The question\n\nWhat about timeouts?\n"));
        e.questions.push(Question {
            question: "Why retry?".to_owned(),
            answer: "GitHub flakes.".to_owned(),
            refs: vec![r("src/a.rs", Some(1))],
        });
        let p = prompt(&entry(), &sample(), 1 << 20, &e, "And then?");
        assert!(p.contains(
            "# Earlier questions\n\nQ: Why retry?\nA: GitHub flakes. [src/a.rs hunk 2]\n"
        ));
        let bare = Explanation {
            diagram: None,
            steps: Vec::new(),
            concerns: Vec::new(),
            ..explanation()
        };
        let p = prompt(&entry(), &sample(), 1 << 20, &bare, "q");
        assert!(!p.contains("Steps:") && !p.contains("Diagram") && !p.contains("Concerns:"));
    }

    #[test]
    fn prompts_keep_the_explanation_and_newest_answers_within_budget() {
        let mut e = explanation();
        for (q, a) in [
            ("First?", "Old."),
            ("Second?", "Newer."),
            ("Third?", "Newest."),
        ] {
            e.questions.push(Question {
                question: q.to_owned(),
                answer: a.to_owned(),
                refs: Vec::new(),
            });
        }
        let mut said = String::new();
        recap(&mut said, &e);
        let turn = |q: &str, a: &str| format!("\nQ: {q}\nA: {a}\n").len();
        let room = said.len() + turn("Third?", "Newest.") + turn("Second?", "Newer.");
        let p = prompt(&entry(), &sample(), room, &e, "q");
        assert!(
            p.contains("(1 earlier question was left out to save space)"),
            "{p}"
        );
        assert!(!p.contains("First?"));
        assert!(
            p.contains("Q: Second?\nA: Newer.\n\nQ: Third?"),
            "in order: {p}"
        );
        let p = prompt(&entry(), &sample(), said.len(), &e, "q");
        assert!(p.contains("(3 earlier questions were left out"), "{p}");
        // Too little room for the explanation itself: it is cut, on a
        // character boundary.
        e.intent = "é".repeat(40);
        let p = prompt(&entry(), &sample(), 15, &e, "q");
        assert!(
            p.contains("Intent: ééé\n(the rest was left out to save space)"),
            "{p}"
        );
    }

    #[test]
    fn answers_are_asked_kept_and_survive_a_new_explanation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = sample();
        let ex = explainer(
            dir.path(),
            r#"cat > prompt; printf '{"answer": "%s", "refs": [{"file": "src/b.rs", "hunk": 1}]}' "$(cat reply)"; echo '{"intent": "Retry"}' > answer"#,
        );
        assert_eq!(ex.questions(&entry(), &files), Vec::new());
        std::fs::write(dir.path().join("reply"), "Because.").expect("write");
        let mut e = explanation();
        let groups = agent::Groups::default();
        let first = ex
            .ask(&asking(&e, "Why?"), dir.path(), &groups)
            .expect("answered");
        assert_eq!(first.answer, "Because.");
        assert_eq!(first.refs, vec![make(&files, 1, Some(0))]);
        e.questions.push(first.clone());
        std::fs::write(dir.path().join("reply"), "Then this.").expect("write");
        let second = ex
            .ask(&asking(&e, "And?"), dir.path(), &groups)
            .expect("answered");
        let sent = std::fs::read_to_string(dir.path().join("prompt")).expect("prompt");
        assert!(
            sent.contains("Q: Why?\nA: Because."),
            "the thread is replayed"
        );
        assert_eq!(
            ex.questions(&entry(), &files),
            vec![first.clone(), second.clone()]
        );

        // An answer to an older snapshot is merged with what was stored since.
        std::fs::write(dir.path().join("reply"), "Late.").expect("write");
        let late = ex
            .ask(&asking(&explanation(), "Late?"), dir.path(), &groups)
            .expect("answered");
        assert_eq!(
            ex.questions(&entry(), &files),
            vec![first, second.clone(), late]
        );

        // Another diff has its own questions.
        let mut edited = sample();
        edited[0].hunks[0].lines[1].content = "newer".to_owned();
        assert_eq!(ex.questions(&entry(), &edited), Vec::new());

        // A new explanation of the same diff keeps them.
        let explains = Explainer {
            agent: agent::Agent::Custom("cat > /dev/null; cat answer".to_owned()),
            ..ex.clone()
        };
        let fresh = explains
            .explain(&entry(), &files, dir.path(), true, &groups)
            .expect("explained");
        assert_eq!(fresh.explanation.questions.len(), 3);
        assert_eq!(fresh.explanation.questions[1], second);
        let cached = explains
            .explain(&entry(), &files, dir.path(), false, &groups)
            .expect("explained");
        assert!(cached.cached);
        assert_eq!(cached.explanation.questions.len(), 3);
    }

    #[test]
    fn failures_and_unreadable_stores_keep_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = sample();
        let groups = agent::Groups::default();
        let silent = explainer(dir.path(), "cat > /dev/null");
        let err = silent
            .ask(&asking(&explanation(), "Why?"), dir.path(), &groups)
            .expect_err("empty");
        assert!(err.contains("the answer was empty"), "{err}");
        let failing = explainer(dir.path(), "exit 3");
        assert!(
            failing
                .ask(&asking(&explanation(), "Why?"), dir.path(), &groups)
                .is_err()
        );
        assert_eq!(silent.questions(&entry(), &files), Vec::new());
        let store = cache::Cache::new(dir.path().join("cache"));
        store.put(thread_key(&entry(), &files), "not json");
        assert_eq!(silent.questions(&entry(), &files), Vec::new());
        store.put(
            thread_key(&entry(), &files),
            r#"[{"question": "a", "answer": " "}, {"question": "b", "answer": "kept", "refs": [{"file": "gone.rs"}]}]"#,
        );
        let kept = silent.questions(&entry(), &files);
        assert_eq!(kept.len(), 1, "empty answers are skipped");
        assert!(kept[0].refs.is_empty(), "stale pointers are dropped");
        let uncached = Explainer {
            cache: None,
            ..silent
        };
        assert_eq!(uncached.questions(&entry(), &files), Vec::new());
    }
}
