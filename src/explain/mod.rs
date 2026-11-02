//! Explanations of a change, written by a coding agent already set up on the
//! machine (Claude Code, Codex, pi or a custom command).
//!
//! vert sends the agent the change's message and its diff, with every hunk
//! numbered, and asks for one JSON object back: the intent, a reading order of
//! steps, an optional diagram, and the concerns worth a human look. Every claim
//! points at hunks. [`Explanation::validate`] keeps only the pointers that name
//! real hunks and lists the hunks no step or diagram node covers, which is
//! where a reviewer should look first.

pub mod agent;
pub mod cache;
pub mod tree;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::git::FileStatus;
use crate::git::diff::{FileDiff, LineKind};
use crate::git::feed::Entry;

/// Bumped whenever the prompt changes, so cached answers to the old one are
/// not reused.
const PROMPT_VERSION: &str = "vert-explain-1";

/// What went wrong turning an agent's answer into an [`Explanation`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The output held no JSON object.
    #[error("the agent's answer contains no JSON object: {0}")]
    NoJson(String),
    /// The JSON did not have the expected shape.
    #[error("the agent's answer is not in the expected shape: {0}")]
    Shape(String),
    /// The answer left out the intent.
    #[error("the agent's answer has no intent")]
    NoIntent,
}

/// How a diagram element relates to the change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Change {
    /// New in this change.
    Added,
    /// Modified by this change.
    Changed,
    /// Removed by this change.
    Removed,
    /// Untouched context, shown so the rest makes sense.
    #[default]
    Same,
}

impl From<String> for Change {
    fn from(s: String) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "added" | "add" | "new" | "created" => Self::Added,
            "changed" | "change" | "modified" | "updated" => Self::Changed,
            "removed" | "remove" | "deleted" | "gone" => Self::Removed,
            _ => Self::Same,
        }
    }
}

/// A pointer from a claim to the code it is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref {
    /// Path of the changed file.
    pub path: String,
    /// Hunk index (0-based) within the file, or `None` for the whole file.
    pub hunk: Option<usize>,
    /// What to show for the pointer: the hunk header, or a note for a whole file.
    pub label: String,
}

/// One step of the reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Short title.
    pub title: String,
    /// The logic, in a few sentences.
    pub detail: String,
    /// The code the step covers.
    pub refs: Vec<Ref>,
}

/// A box in the diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Name shown.
    pub label: String,
    /// How the change affects it.
    pub change: Change,
    /// Where it is implemented.
    pub refs: Vec<Ref>,
}

/// An arrow in the diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// Index of the source node.
    pub from: usize,
    /// Index of the target node.
    pub to: usize,
    /// What the arrow means (a call, an event, a transition).
    pub label: String,
    /// How the change affects it.
    pub change: Change,
}

/// A small picture of the change: components, states, a flow or data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagram {
    /// What the picture shows, in the agent's words (`components`, `states`, …).
    pub kind: String,
    /// One-line caption.
    pub title: String,
    /// Boxes.
    pub nodes: Vec<Node>,
    /// Arrows between boxes.
    pub edges: Vec<Edge>,
}

/// Something a reviewer should double-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// What to check and why.
    pub text: String,
    /// The code it is about.
    pub refs: Vec<Ref>,
}

/// An agent's explanation of a change, checked against the change's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    /// What the change is for and how it gets there.
    pub intent: String,
    /// Reading order.
    pub steps: Vec<Step>,
    /// A picture, when the agent judged one useful.
    pub diagram: Option<Diagram>,
    /// Things worth a human look.
    pub concerns: Vec<Note>,
    /// Hunks no step or diagram node covers.
    pub unexplained: Vec<Ref>,
    /// Pointers the agent gave that name no hunk of this change.
    pub dropped: usize,
}

#[derive(Debug, Deserialize)]
struct RawExplanation {
    #[serde(default)]
    intent: String,
    #[serde(default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    diagram: Option<RawDiagram>,
    #[serde(default)]
    concerns: Vec<RawNote>,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    refs: Vec<RawRef>,
}

#[derive(Debug, Deserialize)]
struct RawRef {
    file: String,
    #[serde(default)]
    hunk: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawDiagram {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default)]
    edges: Vec<RawEdge>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default = "same")]
    change: String,
    #[serde(default)]
    refs: Vec<RawRef>,
}

#[derive(Debug, Deserialize)]
struct RawEdge {
    from: String,
    to: String,
    #[serde(default)]
    label: String,
    #[serde(default = "same")]
    change: String,
}

fn same() -> String {
    "same".to_owned()
}

#[derive(Debug, Deserialize)]
struct RawNote {
    text: String,
    #[serde(default)]
    refs: Vec<RawRef>,
}

/// Resolves the agent's pointers against the diff, counting the ones that
/// name nothing.
struct Resolver<'a> {
    files: &'a [FileDiff],
    dropped: usize,
}

impl Resolver<'_> {
    fn file(&self, name: &str) -> Option<&FileDiff> {
        let name = name.trim();
        // Agents sometimes copy git's `a/` and `b/` prefixes.
        let stripped = name.strip_prefix("a/").or_else(|| name.strip_prefix("b/"));
        [Some(name), stripped].into_iter().flatten().find_map(|n| {
            self.files
                .iter()
                .find(|f| f.stat.path == n || f.stat.old_path.as_deref() == Some(n))
        })
    }

    fn one(&mut self, raw: &RawRef) -> Option<Ref> {
        let Some(file) = self.file(&raw.file) else {
            self.dropped += 1;
            return None;
        };
        let hunk = match raw.hunk {
            // A file without hunks (binary, rename) can only be named whole.
            _ if file.hunks.is_empty() => None,
            Some(n) if (1..=file.hunks.len()).contains(&n) => Some(n - 1),
            Some(_) => {
                self.dropped += 1;
                return None;
            }
            None => None,
        };
        Some(make_ref(file, hunk))
    }

    fn many(&mut self, raw: &[RawRef]) -> Vec<Ref> {
        let mut seen = BTreeSet::new();
        raw.iter()
            .filter_map(|r| self.one(r))
            .filter(|r| seen.insert((r.path.clone(), r.hunk)))
            .collect()
    }
}

fn make_ref(file: &FileDiff, hunk: Option<usize>) -> Ref {
    let label = match hunk.and_then(|h| file.hunks.get(h)) {
        Some(h) => h.header.clone(),
        None if file.hunks.is_empty() => "whole file".to_owned(),
        None => format!("whole file · {} hunks", file.hunks.len()),
    };
    Ref {
        path: file.stat.path.clone(),
        hunk,
        label,
    }
}

impl Explanation {
    /// Parses an agent's answer and checks it against `files`, the diff the
    /// agent was shown. Text around the JSON object (prose, code fences) is
    /// ignored.
    ///
    /// ```
    /// let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    /// let files = vert::git::diff::parse(diff).unwrap();
    /// let answer = r#"{"intent": "Rename old to new",
    ///   "steps": [{"title": "Rename", "refs": [{"file": "a.rs", "hunk": 1}, {"file": "b.rs"}]}]}"#;
    /// let e = vert::explain::Explanation::validate(answer, &files).unwrap();
    /// assert_eq!(e.steps[0].refs.len(), 1);
    /// assert_eq!(e.dropped, 1, "b.rs is not part of the change");
    /// assert!(e.unexplained.is_empty());
    /// ```
    ///
    /// # Errors
    /// Returns [`ParseError`] when there is no JSON object, it has the wrong
    /// shape, or it has no intent.
    pub fn validate(answer: &str, files: &[FileDiff]) -> Result<Self, ParseError> {
        let json = extract_json(answer).ok_or_else(|| ParseError::NoJson(snippet(answer)))?;
        let raw: RawExplanation =
            serde_json::from_str(json).map_err(|e| ParseError::Shape(e.to_string()))?;
        let intent = raw.intent.trim().to_owned();
        if intent.is_empty() {
            return Err(ParseError::NoIntent);
        }
        let mut resolve = Resolver { files, dropped: 0 };
        let steps: Vec<Step> = raw
            .steps
            .iter()
            .filter(|s| !s.title.trim().is_empty() || !s.detail.trim().is_empty())
            .map(|s| Step {
                title: s.title.trim().to_owned(),
                detail: s.detail.trim().to_owned(),
                refs: resolve.many(&s.refs),
            })
            .collect();
        let diagram = raw.diagram.and_then(|d| diagram(d, &mut resolve));
        let concerns = raw
            .concerns
            .iter()
            .filter(|n| !n.text.trim().is_empty())
            .map(|n| Note {
                text: n.text.trim().to_owned(),
                refs: resolve.many(&n.refs),
            })
            .collect();
        let covered = steps.iter().flat_map(|s| &s.refs).chain(
            diagram
                .iter()
                .flat_map(|d| d.nodes.iter().flat_map(|n| &n.refs)),
        );
        let unexplained = unexplained(files, covered);
        Ok(Self {
            intent,
            steps,
            diagram,
            concerns,
            unexplained,
            dropped: resolve.dropped,
        })
    }
}

fn diagram(raw: RawDiagram, resolve: &mut Resolver<'_>) -> Option<Diagram> {
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut nodes = Vec::new();
    for n in &raw.nodes {
        let id = n.id.trim();
        if id.is_empty() || ids.contains_key(id) {
            continue;
        }
        ids.insert(id.to_owned(), nodes.len());
        let label = n.label.trim();
        nodes.push(Node {
            label: if label.is_empty() { id } else { label }.to_owned(),
            change: Change::from(n.change.clone()),
            refs: resolve.many(&n.refs),
        });
    }
    if nodes.is_empty() {
        return None;
    }
    let edges = raw
        .edges
        .into_iter()
        .filter_map(|e| {
            Some(Edge {
                from: *ids.get(e.from.trim())?,
                to: *ids.get(e.to.trim())?,
                label: e.label.trim().to_owned(),
                change: Change::from(e.change),
            })
        })
        .collect();
    Some(Diagram {
        kind: raw.kind.trim().to_owned(),
        title: raw.title.trim().to_owned(),
        nodes,
        edges,
    })
}

/// Every hunk (or hunk-less file) of `files` that no ref in `covered` names.
fn unexplained<'a>(files: &[FileDiff], covered: impl Iterator<Item = &'a Ref>) -> Vec<Ref> {
    let mut whole = BTreeSet::new();
    let mut hunks = BTreeSet::new();
    for r in covered {
        match r.hunk {
            Some(h) => {
                hunks.insert((r.path.as_str(), h));
            }
            None => {
                whole.insert(r.path.as_str());
            }
        }
    }
    files
        .iter()
        .filter(|f| !whole.contains(f.stat.path.as_str()))
        .flat_map(|f| {
            let units: Vec<Option<usize>> = if f.hunks.is_empty() {
                vec![None]
            } else {
                (0..f.hunks.len()).map(Some).collect()
            };
            units
                .into_iter()
                .filter(|u| u.is_none_or(|h| !hunks.contains(&(f.stat.path.as_str(), h))))
                .map(|u| make_ref(f, u))
        })
        .collect()
}

/// The JSON object inside an agent's answer: the whole answer, the inside of
/// a code fence, or the span from the first `{` to the last `}`.
fn extract_json(answer: &str) -> Option<&str> {
    let trimmed = answer.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end).then(|| &trimmed[start..=end])
}

/// The start of a long text, for error messages.
fn snippet(text: &str) -> String {
    const MAX: usize = 160;
    let text = text.trim();
    if text.is_empty() {
        return "it was empty".to_owned();
    }
    let cut: String = text.chars().take(MAX).collect();
    if cut.len() < text.len() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// A stable 64-bit FNV-1a hash of `parts`, used as a cache key. Unlike
/// `std`'s hasher it never changes between Rust releases.
#[must_use]
pub fn fingerprint<'a>(parts: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Identifies one version of a change: its id plus the exact diff, so a
/// working tree that moved on gets a new key.
#[must_use]
pub fn diff_key(entry: &Entry, files: &[FileDiff]) -> u64 {
    let parts = files.iter().flat_map(|f| {
        std::iter::once(f.stat.path.as_str()).chain(f.hunks.iter().flat_map(|h| {
            std::iter::once(h.header.as_str()).chain(h.lines.iter().flat_map(|l| {
                [
                    match l.kind {
                        LineKind::Added => "+",
                        LineKind::Removed => "-",
                        LineKind::Context => " ",
                    },
                    l.content.as_str(),
                ]
            }))
        }))
    });
    fingerprint(std::iter::once(entry.id.as_str()).chain(parts))
}

const INSTRUCTIONS: &str = r#"You are helping a person review a code change in this repository. They want to understand the high-level intent, architecture and logic of the change, not line-by-line style.

You may read files in the current directory to understand the context. Do not modify anything. The diff below is the source of truth for what changed; the files on disk may be at a different version.

Answer with ONLY one JSON object, no prose and no code fences, in this shape:
{
  "intent": "string",
  "steps": [{"title": "string", "detail": "string", "refs": [REF]}],
  "diagram": null or {
    "kind": "components | states | flow | data",
    "title": "string",
    "nodes": [{"id": "string", "label": "string", "change": "added | changed | removed | same", "refs": [REF]}],
    "edges": [{"from": "node id", "to": "node id", "label": "string", "change": "added | changed | removed | same"}]
  },
  "concerns": [{"text": "string", "refs": [REF]}]
}
REF is {"file": "path exactly as listed below", "hunk": hunk number as listed below}. Leave out "hunk" to mean the whole file.

Rules:
- intent: one or two plain sentences. What the change is for, and how it gets there.
- steps: the order in which a person should read the change, from 1 to 7 steps. Each has a short title, one to three sentences on the logic (why, not a restatement of the code), and refs to the hunks it covers. Between them the steps should cover every hunk that matters.
- diagram: include one only when a picture makes the change easier to understand; otherwise null. Pick the kind that fits: "components" for parts and how they talk, "states" when states or transitions change, "flow" for a sequence of calls or data flow, "data" for data shapes. Use at most 8 nodes. Mark what the change added, changed or removed; use "same" for unchanged context that the picture needs. Node refs point at where the node is implemented in this diff.
- concerns: what a reviewer should double-check, such as behaviour that does not match the stated intent, changes nobody asked for, or risky edge cases. Use an empty list when there are none. Do not invent problems.
- Only refer to the files and hunks listed below."#;

/// How to ask for explanations.
#[derive(Debug, Clone)]
pub struct Explainer {
    /// The agent to ask.
    pub agent: agent::Agent,
    /// The model the agent should use, or `None` for its own default.
    pub model: Option<String>,
    /// How long the agent may take.
    pub timeout: Duration,
    /// Diff bytes sent in full; later hunks are listed by header only.
    pub max_diff_bytes: usize,
    /// Where answers are kept, or `None` to always ask.
    pub cache: Option<cache::Cache>,
}

/// An explanation, and whether it came from the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The checked explanation.
    pub explanation: Explanation,
    /// Whether it was read from the cache rather than asked for.
    pub cached: bool,
}

impl Explainer {
    /// The agent, and its model when one was chosen, as shown to the user.
    ///
    /// ```
    /// use std::time::Duration;
    /// use vert::explain::{Explainer, agent::Agent};
    /// let mut ex = Explainer {
    ///     agent: Agent::Pi,
    ///     model: None,
    ///     timeout: Duration::from_secs(1),
    ///     max_diff_bytes: 1,
    ///     cache: None,
    /// };
    /// assert_eq!(ex.label(), "pi");
    /// ex.model = Some("sonnet".into());
    /// assert_eq!(ex.label(), "pi · sonnet");
    /// ```
    #[must_use]
    pub fn label(&self) -> String {
        match &self.model {
            Some(model) => format!("{} · {model}", self.agent.name()),
            None => self.agent.name().to_owned(),
        }
    }

    /// Explains `entry`, whose diff is `files`, running the agent in `dir`.
    /// A cached answer is used unless `fresh`.
    ///
    /// # Errors
    /// Returns a message for the user when the agent fails or its answer
    /// cannot be used.
    pub fn explain(
        &self,
        entry: &Entry,
        files: &[FileDiff],
        dir: &Path,
        fresh: bool,
    ) -> Result<Answer, String> {
        let prompt = prompt(entry, files, self.max_diff_bytes);
        let agent = format!("{:?}", self.agent);
        let model = self.model.as_deref().unwrap_or_default();
        let key = fingerprint([agent.as_str(), model, prompt.as_str()]);
        if !fresh
            && let Some(cache) = &self.cache
            && let Some(saved) = cache.get(key)
            && let Ok(explanation) = Explanation::validate(&saved, files)
        {
            return Ok(Answer {
                explanation,
                cached: true,
            });
        }
        let out = agent::run(
            &self.agent,
            self.model.as_deref(),
            dir,
            &prompt,
            self.timeout,
        )
        .map_err(|e| crate::jobs::describe(&e))?;
        let explanation = Explanation::validate(&out, files)
            .map_err(|e| format!("{}: {e}", self.agent.name()))?;
        if let Some(cache) = &self.cache {
            cache.put(key, extract_json(&out).unwrap_or(&out));
        }
        Ok(Answer {
            explanation,
            cached: false,
        })
    }
}

/// Builds the prompt for `entry`, whose diff is `files`. Hunk bodies stop
/// once the diff passes `max_diff_bytes`; their headers are still listed so
/// the agent can refer to them and read the files itself.
#[must_use]
pub fn prompt(entry: &Entry, files: &[FileDiff], max_diff_bytes: usize) -> String {
    let mut out = String::with_capacity(max_diff_bytes.min(1 << 20) + INSTRUCTIONS.len() + 1024);
    out.push_str(PROMPT_VERSION);
    out.push_str("\n\n");
    out.push_str(INSTRUCTIONS);
    out.push_str("\n\n# The change\n\n");
    let title = if entry.is_worktree() {
        "Uncommitted changes in the working tree"
    } else {
        entry.summary.as_str()
    };
    let _ = writeln!(out, "{} {title}", entry.short);
    if !entry.body.trim().is_empty() {
        let _ = writeln!(out, "\n{}", entry.body.trim());
    }
    let _ = writeln!(
        out,
        "\n# Diff: {} files, hunks numbered per file\n",
        files.len()
    );
    let mut budget = max_diff_bytes;
    let mut omitted = false;
    for file in files {
        let s = &file.stat;
        let status = match s.status {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
        };
        let _ = write!(out, "=== {} ({status}, +{} -{}", s.path, s.added, s.removed);
        if let Some(old) = &s.old_path {
            let _ = write!(out, ", from {old}");
        }
        if s.binary {
            out.push_str(", binary");
        }
        out.push_str(")\n");
        for (i, hunk) in file.hunks.iter().enumerate() {
            let _ = writeln!(out, "--- hunk {} {}", i + 1, hunk.header);
            let body = hunk.lines.iter().fold(String::new(), |mut body, l| {
                let sign = match l.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                let _ = writeln!(body, "{sign}{}", l.content);
                body
            });
            if body.len() <= budget {
                budget -= body.len();
                out.push_str(&body);
            } else {
                budget = 0;
                omitted = true;
                out.push_str("(content left out to save space: read the file)\n");
            }
        }
    }
    if omitted {
        out.push_str(
            "\nSome hunks were left out to save space. Read those files to understand them.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DiffLine, Hunk};
    use crate::git::{FileStat, Target};

    fn hunk(header: &str, lines: &[(LineKind, &str)]) -> Hunk {
        Hunk {
            header: header.to_owned(),
            lines: lines
                .iter()
                .map(|(kind, content)| DiffLine {
                    kind: *kind,
                    old_no: None,
                    new_no: None,
                    content: (*content).to_owned(),
                })
                .collect(),
        }
    }

    fn file(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            stat: FileStat {
                path: path.to_owned(),
                old_path: None,
                status: FileStatus::Modified,
                added: 1,
                removed: 1,
                binary: false,
            },
            hunks,
        }
    }

    pub(crate) fn sample() -> Vec<FileDiff> {
        let mut renamed = file("src/new.rs", Vec::new());
        renamed.stat.old_path = Some("src/old.rs".to_owned());
        renamed.stat.status = FileStatus::Renamed;
        vec![
            file(
                "src/a.rs",
                vec![
                    hunk(
                        "@@ -1 +1 @@ fn a",
                        &[(LineKind::Removed, "old"), (LineKind::Added, "new")],
                    ),
                    hunk("@@ -9 +9 @@", &[(LineKind::Added, "more")]),
                ],
            ),
            file(
                "src/b.rs",
                vec![hunk("@@ -3 +3 @@", &[(LineKind::Context, "ctx")])],
            ),
            renamed,
        ]
    }

    fn entry() -> Entry {
        let mut e = Entry::blank(Target::Commit(git2::Oid::ZERO_SHA1));
        "Add retries".clone_into(&mut e.summary);
        "Because GitHub flakes.".clone_into(&mut e.body);
        e
    }

    #[test]
    fn validates_refs_and_finds_unexplained_hunks() {
        let answer = r#"Here you go:
```json
{
  "intent": "  Retry failed calls.  ",
  "steps": [
    {"title": "Policy", "detail": "Adds it.", "refs": [
      {"file": "src/a.rs", "hunk": 1},
      {"file": "b/src/a.rs", "hunk": 1},
      {"file": "src/a.rs", "hunk": 7},
      {"file": "nope.rs"}
    ]},
    {"title": "", "detail": "", "refs": []},
    {"title": "Rename", "refs": [{"file": "src/old.rs", "hunk": 3}]}
  ],
  "diagram": {
    "kind": "components", "title": "Who calls what",
    "nodes": [
      {"id": "a", "label": "Client", "change": "modified", "refs": [{"file": "src/b.rs"}]},
      {"id": "b", "change": "new"},
      {"id": "a", "label": "duplicate"},
      {"id": " "}
    ],
    "edges": [
      {"from": "a", "to": "b", "label": "uses", "change": "added"},
      {"from": "a", "to": "ghost"}
    ]
  },
  "concerns": [{"text": "Unbounded retries?", "refs": [{"file": "src/a.rs", "hunk": 2}]}, {"text": " "}]
}
```"#;
        let files = sample();
        let e = Explanation::validate(answer, &files).expect("valid");
        assert_eq!(e.intent, "Retry failed calls.");
        assert_eq!(e.steps.len(), 2, "the empty step is dropped");
        assert_eq!(
            e.steps[0].refs,
            [Ref {
                path: "src/a.rs".to_owned(),
                hunk: Some(0),
                label: "@@ -1 +1 @@ fn a".to_owned()
            }],
            "duplicates merge, a/ b/ prefixes are tolerated"
        );
        assert_eq!(e.dropped, 2, "hunk 7 and nope.rs name nothing");
        assert_eq!(
            e.steps[1].refs[0].path, "src/new.rs",
            "old paths of renames resolve"
        );
        assert_eq!(e.steps[1].refs[0].hunk, None, "a hunk-less file is whole");
        assert_eq!(e.steps[1].refs[0].label, "whole file");

        let d = e.diagram.as_ref().expect("diagram");
        assert_eq!(d.nodes.len(), 2);
        assert_eq!(d.nodes[0].change, Change::Changed);
        assert_eq!(d.nodes[1].label, "b", "label falls back to the id");
        assert_eq!(d.nodes[1].change, Change::Added);
        assert_eq!(d.edges.len(), 1, "edges to unknown nodes go");
        assert_eq!(d.edges[0].change, Change::Added);
        assert_eq!(e.concerns.len(), 1);

        // a.rs hunk 2 is only named by a concern, which explains nothing.
        assert_eq!(e.unexplained.len(), 1);
        assert_eq!(e.unexplained[0].path, "src/a.rs");
        assert_eq!(e.unexplained[0].hunk, Some(1));
    }

    #[test]
    fn whole_file_refs_cover_every_hunk() {
        let answer =
            r#"{"intent": "x", "steps": [{"title": "all", "refs": [{"file": "src/a.rs"}]}]}"#;
        let e = Explanation::validate(answer, &sample()).expect("valid");
        assert_eq!(e.steps[0].refs[0].label, "whole file · 2 hunks");
        let left: Vec<_> = e.unexplained.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(left, ["src/b.rs", "src/new.rs"]);
        assert!(e.diagram.is_none());
    }

    #[test]
    fn rejects_answers_without_json_shape_or_intent() {
        let files = sample();
        assert!(matches!(
            Explanation::validate("I could not do it", &files),
            Err(ParseError::NoJson(_))
        ));
        assert_eq!(
            Explanation::validate("  ", &files),
            Err(ParseError::NoJson("it was empty".to_owned()))
        );
        assert!(matches!(
            Explanation::validate(r#"{"intent": 3}"#, &files),
            Err(ParseError::Shape(_))
        ));
        assert_eq!(
            Explanation::validate(r#"{"intent": " "}"#, &files),
            Err(ParseError::NoIntent)
        );
        assert!(
            Explanation::validate(r#"{"intent": "x", "diagram": {"nodes": []}}"#, &files)
                .expect("valid")
                .diagram
                .is_none(),
            "an empty diagram is no diagram"
        );
        let long = "x".repeat(500);
        let Err(ParseError::NoJson(why)) = Explanation::validate(&long, &files) else {
            panic!("no json")
        };
        assert!(why.ends_with('…') && why.len() < 200, "{why}");
        assert!(ParseError::NoIntent.to_string().contains("intent"));
    }

    #[test]
    fn change_words_map_loosely() {
        for (word, change) in [
            ("ADDED", Change::Added),
            ("new", Change::Added),
            ("modified", Change::Changed),
            ("deleted", Change::Removed),
            ("unchanged", Change::Same),
            ("whatever", Change::Same),
        ] {
            assert_eq!(Change::from(word.to_owned()), change, "{word}");
        }
    }

    #[test]
    fn prompts_number_hunks_and_respect_the_budget() {
        let files = sample();
        let p = prompt(&entry(), &files, 1 << 20);
        assert!(p.starts_with(PROMPT_VERSION));
        assert!(p.contains("0000000 Add retries"), "{p}");
        assert!(p.contains("Because GitHub flakes."));
        assert!(p.contains("=== src/a.rs (modified, +1 -1)"));
        assert!(p.contains("--- hunk 2 @@ -9 +9 @@\n+more\n"));
        assert!(p.contains("=== src/new.rs (renamed, +1 -1, from src/old.rs)"));
        assert!(!p.contains("left out"));

        let tight = prompt(&entry(), &files, 10);
        assert!(tight.contains("-old\n+new\n"), "the first hunk fits");
        assert!(tight.contains("--- hunk 2 @@ -9 +9 @@\n(content left out"));
        assert!(tight.contains("Some hunks were left out"));

        let mut wt = Entry::blank(Target::WorkingTree);
        let mut bin = file("logo.png", Vec::new());
        bin.stat.binary = true;
        bin.stat.status = FileStatus::Added;
        wt.body.clear();
        let p = prompt(&wt, &[bin], 100);
        assert!(p.contains("Uncommitted changes in the working tree"));
        assert!(p.contains("(added, +1 -1, binary)"));
        let deleted = FileDiff {
            stat: FileStat {
                status: FileStatus::Deleted,
                ..sample()[0].stat.clone()
            },
            hunks: Vec::new(),
        };
        assert!(prompt(&wt, &[deleted], 100).contains("(deleted"));
    }

    #[test]
    fn explains_through_the_agent_and_caches_the_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = sample();
        let answer = dir.path().join("answer.json");
        std::fs::write(
            &answer,
            r#"{"intent": "Retry", "steps": [{"title": "all", "refs": [{"file": "src/a.rs"}]}]}"#,
        )
        .expect("write");
        let calls =
            || std::fs::read_to_string(dir.path().join("calls")).map_or(0, |s| s.lines().count());
        let mut ex = Explainer {
            agent: agent::Agent::Custom(
                "cat > /dev/null; echo x >> calls; echo Sure:; cat answer.json".to_owned(),
            ),
            model: None,
            timeout: Duration::from_secs(10),
            max_diff_bytes: 1 << 20,
            cache: Some(cache::Cache::new(dir.path().join("cache"))),
        };
        let run = |ex: &Explainer, fresh| ex.explain(&entry(), &files, dir.path(), fresh);
        let first = run(&ex, false).expect("explained");
        assert!(!first.cached);
        assert_eq!(first.explanation.intent, "Retry");
        let second = run(&ex, false).expect("cached");
        assert!(second.cached);
        assert_eq!(second.explanation, first.explanation);
        assert_eq!(calls(), 1, "the cache answered");
        assert!(!run(&ex, true).expect("fresh").cached);
        assert_eq!(calls(), 2, "fresh asks again");

        std::fs::write(&answer, "I'd rather not").expect("write");
        let bad = run(&ex, true).expect_err("no json");
        assert!(
            bad.starts_with("--explain-cmd: the agent's answer contains no JSON"),
            "{bad}"
        );
        assert!(run(&ex, false).expect("old answer kept").cached);
        ex.model = Some("other".to_owned());
        std::fs::write(&answer, r#"{"intent": "Other"}"#).expect("write");
        let other = run(&ex, false).expect("another model asks again");
        assert!(!other.cached, "each model has its own answers");
        assert_eq!(other.explanation.intent, "Other");

        ex.agent = agent::Agent::Custom("exit 2".to_owned());
        ex.cache = None;
        let failed = run(&ex, false).expect_err("fails");
        assert!(failed.contains("failed (exit status: 2)"), "{failed}");
    }

    #[test]
    fn keys_follow_the_exact_diff() {
        let files = sample();
        let e = entry();
        let key = diff_key(&e, &files);
        assert_eq!(key, diff_key(&e, &files), "stable");
        let mut edited = files.clone();
        edited[0].hunks[0].lines[1].content.push('!');
        assert_ne!(key, diff_key(&e, &edited));
        let mut kinds = files;
        kinds[1].hunks[0].lines[0].kind = LineKind::Added;
        assert_ne!(key, diff_key(&e, &kinds));
        assert_ne!(fingerprint(["ab", "c"]), fingerprint(["a", "bc"]));
        assert_eq!(fingerprint([]), 0xcbf2_9ce4_8422_2325);
    }
}
