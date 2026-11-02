//! The explain view: an outline of the agent's walkthrough on the left, the
//! selected part on the right, with the diagram drawn as a tree of arrows.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Hints, empty_state, marker, panel, plural, spinner, truncate, width, wrap};
use crate::app::{ExplainView, Item, Pane, Status, items};
use crate::explain::{Change, Diagram, Explanation, Ref, tree};
use crate::theme;

/// Footer hints for the explain view.
pub const EXPLAIN_HINTS: Hints = &[
    ("tab", "pane"),
    ("j/k", "move"),
    ("⏎", "go to code"),
    ("J/K", "scroll"),
    ("r", "ask again"),
    ("esc", "back"),
];

/// Draws the explain view.
pub fn draw(frame: &mut Frame<'_>, area: Rect, v: &mut ExplainView, now: i64, tick: usize) {
    let [summary, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    draw_summary(frame, summary, v, now, tick);
    match &v.status {
        Status::Asking(since) => {
            let block = panel(true);
            let inner = block.inner(body);
            frame.render_widget(block, body);
            empty_state(
                frame,
                inner,
                &format!(
                    "{} Asking {} to explain this change…",
                    spinner(tick),
                    v.agent
                ),
                &format!(
                    "{} so far · esc goes back to the diff; the answer is kept when it comes",
                    clock(now - since)
                ),
            );
        }
        Status::Failed(why) => {
            let block = panel(true);
            let inner = block.inner(body);
            frame.render_widget(block, body);
            let w = usize::from(inner.width).saturating_sub(4).max(1);
            let mut lines = vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  Couldn't explain this change",
                    theme::bold(),
                )),
                Line::raw(""),
            ];
            lines.extend(
                wrap(&crate::highlight::sanitize(why), w)
                    .into_iter()
                    .map(|l| Line::from(Span::styled(format!("  {l}"), theme::muted()))),
            );
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("  r", theme::accent()),
                Span::styled(" tries again", theme::muted()),
            ]));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Status::Ready(..) => draw_ready(frame, body, v),
    }
}

/// `m:ss` for a number of seconds.
fn clock(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn draw_summary(frame: &mut Frame<'_>, area: Rect, v: &ExplainView, now: i64, tick: usize) {
    let e = &v.entry;
    let title = if e.is_own_worktree() {
        "Uncommitted changes"
    } else {
        e.summary.as_str()
    };
    let [l1, l2, _] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    frame.render_widget(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(title, usize::from(area.width).saturating_sub(2)),
                theme::bold(),
            ),
        ]),
        l1,
    );
    let state = match &v.status {
        Status::Asking(since) => format!("{} asking {}", spinner(tick), clock(now - since)),
        Status::Ready(_, true) => "from cache · r asks again".to_owned(),
        Status::Ready(_, false) => "just now".to_owned(),
        Status::Failed(_) => "failed".to_owned(),
    };
    frame.render_widget(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(e.short.clone(), Style::default().fg(theme::WARN)),
            Span::styled(
                format!(" · explained by {} · {state}", v.agent),
                theme::muted(),
            ),
        ]),
        l2,
    );
}

fn draw_ready(frame: &mut Frame<'_>, area: Rect, v: &mut ExplainView) {
    // Borrowed by field, so the scroll positions stay writable.
    let Status::Ready(e, _) = &v.status else {
        return;
    };
    let outline = items(e);
    let item = outline
        .get(v.pos.selected)
        .copied()
        .unwrap_or(Item::Overview);
    let outline_w = (area.width * 3 / 10).clamp(24, 40).min(area.width / 2);
    let [left, right] =
        Layout::horizontal([Constraint::Length(outline_w), Constraint::Min(10)]).areas(area);

    let mut block = panel(v.pane == Pane::Outline).title(" Walkthrough ");
    if e.dropped > 0 {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {} dropped ", plural(e.dropped, "bad pointer")),
                theme::muted(),
            ))
            .alignment(Alignment::Right),
        );
    }
    let inner = block.inner(left);
    frame.render_widget(block, left);
    let height = usize::from(inner.height);
    let offset = v.pos.scrolled(height, outline.len());
    let w = usize::from(inner.width).saturating_sub(1);
    let rows: Vec<Line<'_>> = outline
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, it)| {
            let selected = i == v.pos.selected;
            let (text, style) = outline_row(e, *it);
            let style = if selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            };
            Line::from(vec![
                marker(selected),
                Span::styled(truncate(&text, w), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);

    let block = panel(v.pane == Pane::Detail).title(format!(" {} ", item_title(e, item)));
    let inner = block.inner(right);
    frame.render_widget(block, right);
    let selected = (v.pane == Pane::Detail).then_some(v.target);
    let (lines, target_rows) = detail_lines(e, item, usize::from(inner.width), selected);
    let h = usize::from(inner.height);
    v.height = h;
    v.rows = lines.len();
    if let Some(&row) = selected.and_then(|t| target_rows.get(t)) {
        if row < v.scroll {
            v.scroll = row;
        } else if h > 0 && row >= v.scroll + h {
            v.scroll = row + 1 - h;
        }
    }
    v.scroll = v.scroll.min(lines.len().saturating_sub(h.max(1)));
    let visible: Vec<Line<'static>> = lines.into_iter().skip(v.scroll).take(h).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn outline_row(e: &Explanation, item: Item) -> (String, Style) {
    match item {
        Item::Overview => ("◆ Overview".to_owned(), theme::accent()),
        Item::Step(i) => (
            format!(
                "{:>2} {}",
                i + 1,
                e.steps.get(i).map_or("", |s| s.title.as_str())
            ),
            theme::text(),
        ),
        Item::Concerns => (
            format!("⚠ Concerns {}", e.concerns.len()),
            Style::default().fg(theme::WARN),
        ),
        Item::Unexplained => (
            format!("? Unexplained {}", e.unexplained.len()),
            Style::default().fg(theme::HIGHLIGHT),
        ),
    }
}

fn item_title(e: &Explanation, item: Item) -> String {
    match item {
        Item::Overview => "Overview".to_owned(),
        Item::Step(i) => format!("Step {} of {}", i + 1, e.steps.len()),
        Item::Concerns => "Worth a second look".to_owned(),
        Item::Unexplained => "Not covered by the walkthrough".to_owned(),
    }
}

/// Builds the right pane: text lines, and the row of each code pointer in
/// the order [`crate::app::targets`] lists them.
fn detail_lines(
    e: &Explanation,
    item: Item,
    pane_width: usize,
    selected: Option<usize>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut out = Detail {
        lines: Vec::new(),
        targets: Vec::new(),
        selected,
        width: pane_width.saturating_sub(2).max(8),
    };
    match item {
        Item::Overview => overview(&mut out, e),
        Item::Step(i) => {
            if let Some(step) = e.steps.get(i) {
                out.text(&step.title, theme::bold());
                if !step.detail.is_empty() {
                    out.blank();
                    out.text(&step.detail, theme::text());
                }
                out.blank();
                out.heading("Code");
                if step.refs.is_empty() {
                    out.text(
                        "The agent pointed at no code for this step.",
                        theme::muted(),
                    );
                }
                for r in &step.refs {
                    out.reference(r);
                }
            }
        }
        Item::Concerns => {
            for (i, note) in e.concerns.iter().enumerate() {
                if i > 0 {
                    out.blank();
                }
                out.prefixed("⚠ ", Style::default().fg(theme::WARN), &note.text);
                for r in &note.refs {
                    out.reference(r);
                }
            }
        }
        Item::Unexplained => {
            out.text(
                "No step or diagram box covers these hunks. They may be harmless, or work \
                 nobody asked for: read them yourself.",
                theme::muted(),
            );
            out.blank();
            for r in &e.unexplained {
                out.reference(r);
            }
        }
    }
    (out.lines, out.targets)
}

/// Accumulates the right pane's lines. Every line starts with a one-column
/// selection marker.
struct Detail {
    lines: Vec<Line<'static>>,
    targets: Vec<usize>,
    selected: Option<usize>,
    width: usize,
}

impl Detail {
    fn push(&mut self, spans: Vec<Span<'static>>) {
        let mut line = vec![Span::raw(" ")];
        line.extend(spans);
        self.lines.push(Line::from(line));
    }

    fn blank(&mut self) {
        self.lines.push(Line::raw(""));
    }

    fn heading(&mut self, text: &str) {
        self.push(vec![Span::styled(text.to_owned(), theme::accent_bold())]);
    }

    fn text(&mut self, text: &str, style: Style) {
        for l in wrap(text, self.width) {
            self.push(vec![Span::styled(l, style)]);
        }
    }

    /// Wrapped text after a marker, continuation lines indented to match.
    fn prefixed(&mut self, mark: &str, style: Style, text: &str) {
        let indent = width(mark);
        for (i, l) in wrap(text, self.width.saturating_sub(indent).max(1))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 {
                Span::styled(mark.to_owned(), style)
            } else {
                Span::raw(" ".repeat(indent))
            };
            self.push(vec![lead, Span::styled(l, theme::text())]);
        }
    }

    /// Marks the next line as the code pointer with the next target index,
    /// returning whether it is the selected one.
    fn target(&mut self) -> bool {
        let index = self.targets.len();
        self.targets.push(self.lines.len());
        self.selected == Some(index)
    }

    /// Starts the last pushed line with the selection marker.
    fn mark_last(&mut self) {
        if let Some(line) = self.lines.last_mut()
            && let Some(first) = line.spans.first_mut()
        {
            *first = marker(true);
        }
    }

    fn reference(&mut self, r: &Ref) {
        let selected = self.target();
        let path_w = width(&r.path);
        let label = truncate(&r.label, self.width.saturating_sub(path_w + 4));
        let style = if selected {
            theme::accent_bold()
        } else {
            theme::bold()
        };
        self.push(vec![
            Span::styled("› ", theme::accent()),
            Span::styled(truncate(&r.path, self.width.saturating_sub(2)), style),
            Span::styled(format!("  {label}"), theme::muted()),
        ]);
        if selected {
            self.mark_last();
        }
    }
}

fn overview(out: &mut Detail, e: &Explanation) {
    out.heading("Intent");
    out.text(&e.intent, theme::text());
    out.blank();
    match &e.diagram {
        Some(d) => diagram(out, d),
        None => out.text(
            "No diagram: the agent judged the change clear enough without one.",
            theme::muted(),
        ),
    }
    out.blank();
    let mut counts = vec![plural(e.steps.len(), "step")];
    if !e.concerns.is_empty() {
        counts.push(plural(e.concerns.len(), "concern"));
    }
    counts.push(if e.unexplained.is_empty() {
        "every hunk explained".to_owned()
    } else {
        format!("{} unexplained", plural(e.unexplained.len(), "hunk"))
    });
    out.text(&counts.join(" · "), theme::muted());
}

/// The marker and colour of a node or arrow, by how the change affects it.
fn change_style(change: Change) -> (&'static str, Style) {
    match change {
        Change::Added => ("+", Style::default().fg(theme::ADDED)),
        Change::Changed => ("~", Style::default().fg(theme::WARN)),
        Change::Removed => (
            "−",
            Style::default()
                .fg(theme::REMOVED)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        Change::Same => ("", theme::muted()),
    }
}

fn diagram(out: &mut Detail, d: &Diagram) {
    let heading = if d.kind.is_empty() {
        "Diagram".to_owned()
    } else {
        format!("Diagram · {}", d.kind)
    };
    out.heading(&heading);
    if !d.title.is_empty() {
        out.text(&d.title, theme::muted());
    }
    out.blank();
    for row in tree::layout(d) {
        let node = &d.nodes[row.node];
        let arrow = row.edge.map(|i| &d.edges[i]);
        let (_, arrow_style) = change_style(arrow.map_or(Change::Same, |a| a.change));
        let (mark, style) = change_style(node.change);
        if row.chained {
            // The only arrow out of the node above runs straight down.
            let label = arrow.map_or("", |a| a.label.as_str());
            let text = truncate(label, out.width.saturating_sub(width(&row.prefix) + 3));
            out.push(vec![
                Span::styled(row.prefix.clone(), theme::muted()),
                Span::styled("↓", arrow_style),
                Span::styled(format!("  {text}"), arrow_style),
            ]);
        }
        let target = crate::app::row_pointer(d, &row).is_some();
        let selected = target && out.target();
        let mut spans = Vec::new();
        // The tree lines stay muted; the arrow head takes the arrow's colour.
        let (stem, head) = row
            .prefix
            .strip_suffix("▶ ")
            .map_or((row.prefix.as_str(), ""), |s| (s, "▶ "));
        spans.push(Span::styled(stem.to_owned(), theme::muted()));
        spans.push(Span::styled(head.to_owned(), arrow_style));
        let label_style = match (node.change, selected) {
            (_, true) => theme::accent_bold(),
            (Change::Same, false) => theme::text(),
            (_, false) => style.add_modifier(Modifier::BOLD),
        };
        spans.push(Span::styled(node.label.clone(), label_style));
        if !mark.is_empty() {
            spans.push(Span::styled(format!(" {mark}"), style));
        }
        if row.repeat {
            spans.push(Span::styled(" ↺", theme::muted()));
        }
        if let Some(a) = arrow.filter(|a| !a.label.is_empty() && !row.chained) {
            spans.push(Span::styled(format!("  {}", a.label), arrow_style));
        }
        let used: usize = spans.iter().map(|s| width(&s.content)).sum();
        if used > out.width {
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            spans = vec![Span::styled(truncate(&text, out.width), label_style)];
        }
        out.push(spans);
        if selected {
            out.mark_last();
        }
    }
    out.blank();
    let legend: Vec<Span<'static>> = [Change::Added, Change::Changed, Change::Removed]
        .into_iter()
        .zip(["added", "changed", "removed"])
        .flat_map(|(c, word)| {
            let (mark, style) = change_style(c);
            [
                Span::styled(
                    mark.to_owned(),
                    style.remove_modifier(Modifier::CROSSED_OUT),
                ),
                Span::styled(format!(" {word}   "), theme::muted()),
            ]
        })
        .chain([Span::styled("↺ drawn above", theme::muted())])
        .collect();
    out.push(legend);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_count_minutes() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(-3), "0:00");
    }
}
