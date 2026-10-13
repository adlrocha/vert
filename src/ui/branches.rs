//! The Branches tab: one row per worktree, branch or open pull request.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::feed::chips;
use super::{
    Hints, empty_state, marker, pad, panel, spans_width, spinner, stat_spans, truncate, width,
};
use crate::age::relative;
use crate::app::{App, PullStatus, Row, RowFilter};
use crate::github::Checks;
use crate::theme;

const HINTS: Hints = &[
    ("j/k", "move"),
    ("⏎", "open"),
    ("b", "branch diff"),
    ("p", "PR diff"),
    ("f", "filter"),
    ("r", "refresh"),
    ("?", "keys"),
];
/// Width of the kind column (`worktree`, `branch`, …).
const KIND_W: usize = 9;
/// Widest the branch-name column grows.
const MAX_NAME_W: usize = 32;
/// Narrowest the description column gets before it is dropped.
const MIN_DESC_W: usize = 12;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Hints {
    let rows = app.branches.rows();
    let mut title = vec![
        Span::raw(" Branches "),
        Span::styled(format!("{} ", rows.len()), theme::muted()),
    ];
    if app.branches.filter != RowFilter::All {
        title.push(Span::styled(
            format!("· {} ", app.branches.filter.title()),
            Style::default().fg(theme::HIGHLIGHT),
        ));
    }
    if app.branches.loading() {
        title.push(Span::styled(
            format!("{} ", spinner(app.tick)),
            theme::muted(),
        ));
    }
    let block = panel(true)
        .title(Line::from(title))
        .title_bottom(status_line(app).alignment(Alignment::Right));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !app.branches.loaded && app.branches.error.is_none() {
        let text = format!("{} Scanning branches and worktrees", spinner(app.tick));
        empty_state(frame, inner, &text, "");
        return HINTS;
    }
    if let Some(err) = &app.branches.error {
        empty_state(frame, inner, "Couldn't read branches", err);
        return HINTS;
    }
    if rows.is_empty() {
        let detail = if app.branches.filter == RowFilter::All {
            "Branches, worktrees and open pull requests will appear here."
        } else {
            "f changes the filter"
        };
        empty_state(frame, inner, "Nothing to show", detail);
        return HINTS;
    }

    let height = usize::from(inner.height);
    let total = rows.len();
    drop(rows);
    let offset = app.branches.pos.scrolled(height, total);
    let selected = app.branches.pos.selected;
    let rows = app.branches.rows();
    let on_screen = &rows[offset..(offset + height).min(total)];
    let columns = Columns::fit(on_screen);
    let lines: Vec<Line<'_>> = on_screen
        .iter()
        .enumerate()
        .map(|(i, r)| {
            row(
                r,
                columns,
                offset + i == selected,
                usize::from(inner.width),
                app.now,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    HINTS
}

/// Base branch and pull request status, for the bottom border.
fn status_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(base) = &app.branches.list.base {
        spans.push(Span::styled(" vs ", theme::muted()));
        spans.push(Span::styled(base.clone(), theme::bold()));
        spans.push(Span::styled(" ·", theme::muted()));
    }
    let (text, style) = match &app.branches.pull_status {
        PullStatus::Off => ("PRs off (--no-network)".to_owned(), theme::muted()),
        PullStatus::Waiting => ("PRs loading…".to_owned(), theme::muted()),
        PullStatus::Ready(repo) => (
            format!("{} open PRs on {repo}", app.branches.pulls.len()),
            theme::muted(),
        ),
        PullStatus::NoToken => (
            "PRs: set GH_TOKEN or run `gh auth login`".to_owned(),
            Style::default().fg(theme::WARN),
        ),
        PullStatus::Unsupported(why) => (format!("No PRs: {why}"), theme::muted()),
        PullStatus::Failed(why) => (format!("PRs: {why}"), Style::default().fg(theme::WARN)),
    };
    spans.push(Span::styled(format!(" {text} "), style));
    Line::from(spans)
}

/// Widths of the variable columns, sized to the rows on screen.
#[derive(Debug, Clone, Copy)]
struct Columns {
    name: usize,
    dirty: usize,
    track: usize,
    chips: usize,
    counts: usize,
}

impl Columns {
    fn fit(rows: &[Row<'_>]) -> Self {
        let max = |f: &dyn Fn(&Row<'_>) -> usize| rows.iter().map(f).max().unwrap_or(0);
        Self {
            name: max(&|r| width(&name(r))).min(MAX_NAME_W),
            dirty: max(&|r| width(&dirty(r))),
            track: max(&|r| width(&tracking(r))),
            chips: max(&|r| r.changes().map_or(0, |e| spans_width(&chips(e)))),
            counts: max(&|r| {
                let (a, d) = counts(r);
                spans_width(&stat_spans(a, d))
            }),
        }
    }
}

fn kind(r: &Row<'_>) -> (&'static str, Style) {
    match r.branch {
        Some(b) => match &b.worktree {
            Some(w) if w.current => ("● here", Style::default().fg(theme::ADDED)),
            Some(_) => ("worktree", Style::default().fg(theme::INFO)),
            None if b.remote => ("remote", theme::muted()),
            None => ("branch", theme::text()),
        },
        None => ("pr", Style::default().fg(theme::HIGHLIGHT)),
    }
}

fn name(r: &Row<'_>) -> String {
    match (r.branch, r.pull) {
        (Some(b), _) => b.name.clone(),
        (None, Some(p)) => p.head.clone(),
        (None, None) => String::new(),
    }
}

fn dirty(r: &Row<'_>) -> String {
    r.dirty()
        .map(|e| format!("●{}", e.files.len()))
        .unwrap_or_default()
}

fn tracking(r: &Row<'_>) -> String {
    match r.branch.and_then(|b| b.ahead_behind) {
        Some((0, _)) => "merged".to_owned(),
        Some((ahead, 0)) => format!("↑{ahead}"),
        Some((ahead, behind)) => format!("↑{ahead} ↓{behind}"),
        None => String::new(),
    }
}

/// How a pull request's CI state is drawn: ✓ passing, ✗ failing, ● running.
fn check_glyph(checks: Checks) -> (&'static str, ratatui::style::Color) {
    match checks {
        Checks::Passing => ("✓", theme::ADDED),
        Checks::Failing => ("✗", theme::REMOVED),
        Checks::Pending => ("●", theme::WARN),
    }
}

fn counts(r: &Row<'_>) -> (u64, u64) {
    match (r.changes(), r.pull) {
        (Some(e), _) => (e.added, e.removed),
        (None, Some(p)) => (p.added, p.removed),
        (None, None) => (0, 0),
    }
}

fn row(r: &Row<'_>, c: Columns, selected: bool, width_total: usize, now: i64) -> Line<'static> {
    let (kind_text, kind_style) = kind(r);
    let name_style = if selected {
        theme::accent_bold()
    } else {
        theme::bold()
    };
    let merged = r.branch.is_some_and(crate::git::branches::Branch::merged);
    let mut spans = vec![
        marker(selected),
        Span::styled(pad(kind_text, KIND_W), kind_style),
        Span::styled(pad(&name(r), c.name), name_style),
        Span::raw("  "),
    ];
    if c.dirty > 0 {
        spans.push(Span::styled(
            pad(&dirty(r), c.dirty + 2),
            Style::default().fg(theme::WARN),
        ));
    }
    if c.track > 0 {
        let style = if merged {
            theme::muted().add_modifier(Modifier::ITALIC)
        } else {
            theme::muted()
        };
        spans.push(Span::styled(pad(&tracking(r), c.track + 2), style));
    }

    let (added, removed) = counts(r);
    let stats = stat_spans(added, removed);
    let mut right = vec![Span::raw(
        " ".repeat(2 + c.counts.saturating_sub(spans_width(&stats))),
    )];
    right.extend(stats);
    right.push(Span::styled(
        format!("  {:>4} ", relative(now, r.time())),
        theme::muted(),
    ));

    let mut desc_w = width_total.saturating_sub(spans_width(&spans) + spans_width(&right));
    let chip_spans = r.changes().map(chips).unwrap_or_default();
    let show_chips = c.chips > 0 && desc_w >= MIN_DESC_W + c.chips + 2;
    if show_chips {
        desc_w -= c.chips + 2;
    }
    let mut desc = Vec::new();
    if let Some(p) = r.pull {
        desc.push(Span::styled(
            format!("#{} ", p.number),
            Style::default().fg(theme::HIGHLIGHT),
        ));
        if let Some(checks) = p.checks {
            let (glyph, colour) = check_glyph(checks);
            desc.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(colour),
            ));
        }
        if p.draft {
            desc.push(Span::styled("draft ", theme::muted()));
        }
    }
    let text = r.pull.map_or_else(
        || r.changes().map(|e| e.summary.clone()).unwrap_or_default(),
        |p| p.title.clone(),
    );
    let room = desc_w.saturating_sub(spans_width(&desc));
    desc.push(Span::styled(truncate(&text, room), theme::text()));
    let used = spans_width(&desc);
    if desc_w >= MIN_DESC_W {
        spans.extend(desc);
        spans.push(Span::raw(" ".repeat(desc_w.saturating_sub(used))));
    } else {
        spans.push(Span::raw(" ".repeat(desc_w)));
    }
    if show_chips {
        let w = spans_width(&chip_spans);
        spans.push(Span::raw("  "));
        spans.extend(chip_spans);
        spans.push(Span::raw(" ".repeat(c.chips.saturating_sub(w))));
    }
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_glyphs() {
        assert_eq!(check_glyph(Checks::Passing), ("✓", theme::ADDED));
        assert_eq!(check_glyph(Checks::Failing), ("✗", theme::REMOVED));
        assert_eq!(check_glyph(Checks::Pending), ("●", theme::WARN));
    }
}
