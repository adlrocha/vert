//! The answer panel: questions asked from a diff (`a`) and the agent's
//! answers, over the diff; and the prompt such a question is written in.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::explain::Detail;
use super::prompt::{SEND, draw_prompt};
use super::{Hints, clock, panel, spinner, truncate};
use crate::app::{AnswerView, App};
use crate::explain::question::Question;
use crate::theme;

/// Footer hints for the answer panel.
pub const ANSWER_HINTS: Hints = &[
    ("j/k", "pointer"),
    ("⏎", "go to code"),
    ("J/K", "scroll"),
    ("a", "ask more"),
    ("esc", "back"),
];

/// Most lines of the selection the prompt previews.
const PREVIEW_LINES: usize = 4;

/// Draws the answer panel.
pub fn draw(frame: &mut Frame<'_>, area: Rect, v: &mut AnswerView, now: i64, tick: usize) {
    let [summary, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    let e = &v.entry;
    let title = if e.is_own_worktree() {
        "Uncommitted changes"
    } else {
        e.summary.as_str()
    };
    let [l1, l2, _] = Layout::vertical([Constraint::Length(1); 3]).areas(summary);
    let w = usize::from(area.width).saturating_sub(2);
    frame.render_widget(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(truncate(title, w), theme::bold()),
        ]),
        l1,
    );
    let state = v.pending.as_ref().map_or_else(String::new, |p| {
        format!(" · {} answering {}", spinner(tick), clock(now - p.since))
    });
    frame.render_widget(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(e.short.clone(), Style::default().fg(theme::WARN)),
            Span::styled(format!(" · asked {}{state}", v.agent), theme::muted()),
        ]),
        l2,
    );

    let block = panel(true).title(" Answers ");
    let inner = block.inner(body);
    frame.render_widget(block, body);
    let mut out = Detail::new(usize::from(inner.width), Some(v.target));
    for (i, q) in v.answered.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        answered(&mut out, q);
    }
    if let Some(p) = &v.pending {
        if !v.answered.is_empty() {
            out.blank();
        }
        asked(&mut out, &p.question, Some(&p.place));
        out.blank();
        out.text(
            &format!(
                "{} {} is answering… {} · esc goes back to the diff; A brings this back",
                spinner(tick),
                v.agent,
                clock(now - p.since)
            ),
            theme::muted(),
        );
    }
    if let Some(why) = &v.failed {
        out.blank();
        out.text(
            &format!("Couldn't answer: {why}"),
            Style::default().fg(theme::REMOVED),
        );
        out.text("a asks again", theme::muted());
    }
    let h = usize::from(inner.height);
    v.height = h;
    v.rows = out.lines.len();
    let bottom = out.lines.len().saturating_sub(h.max(1));
    v.scroll = v.scroll.min(bottom);
    if v.follow
        && let Some(&row) = out.targets.get(v.target)
    {
        if row < v.scroll {
            v.scroll = row;
        } else if h > 0 && row >= v.scroll + h {
            v.scroll = row + 1 - h;
        }
    }
    let visible: Vec<Line<'static>> = out.lines.into_iter().skip(v.scroll).take(h).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// A question and where it pointed.
fn asked(out: &mut Detail, question: &str, place: Option<&str>) {
    out.prefixed("Q ", Style::default().fg(theme::INFO), question);
    if let Some(place) = place {
        out.indented(2, &format!("about {place}"), theme::muted());
    }
}

/// A question, its answer and the code the answer points at.
fn answered(out: &mut Detail, q: &Question) {
    asked(out, &q.question, q.about.as_deref());
    out.blank();
    for (n, paragraph) in q.answer.split("\n\n").enumerate() {
        if n > 0 {
            out.blank();
        }
        out.text(paragraph, theme::text());
    }
    if !q.refs.is_empty() {
        out.blank();
        out.heading("Code");
    }
    for r in &q.refs {
        out.reference(r);
    }
}

/// Draws the question being written from a diff over everything else.
pub(super) fn draw_draft(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(draft) = &app.ask.draft else {
        return;
    };
    let agent = match &app.explain.agent {
        Some(Ok(agent)) => agent.as_str(),
        _ => "the agent",
    };
    let about = &draft.about;
    draw_prompt(frame, area, (" Ask ", SEND), &draft.text, |inner| {
        let mut lines = vec![
            Line::from(Span::styled(
                truncate(&format!("Ask {agent} about {}", about.place), inner),
                theme::bold(),
            )),
            Line::from(Span::styled(
                truncate(
                    if about.excerpt.is_empty() {
                        "It sees the whole diff and can read the file; it changes nothing"
                    } else {
                        "It sees the whole diff and these lines; it changes nothing"
                    },
                    inner,
                ),
                theme::muted(),
            )),
        ];
        let excerpt: Vec<&str> = about
            .excerpt
            .lines()
            .filter(|l| !l.starts_with("@@"))
            .collect();
        for l in excerpt.iter().take(PREVIEW_LINES) {
            let style = match l.chars().next() {
                Some('+') => Style::default().fg(theme::ADDED),
                Some('-') => Style::default().fg(theme::REMOVED),
                _ => theme::muted(),
            };
            lines.push(Line::from(Span::styled(
                truncate(&crate::highlight::sanitize(l), inner),
                style,
            )));
        }
        if excerpt.len() > PREVIEW_LINES {
            lines.push(Line::from(Span::styled(
                format!("… {} more lines", excerpt.len() - PREVIEW_LINES),
                theme::muted(),
            )));
        }
        lines
    });
}
