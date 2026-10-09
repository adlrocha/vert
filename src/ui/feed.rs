//! The change feed: one row per change, plus a preview pane on wide terminals.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{
    Hints, churn_bar, empty_state, marker, module_list, pad, panel, path_spans, plural,
    spans_width, spinner, stat_spans, truncate,
};
use crate::age::{ago, relative};
use crate::app::App;
use crate::git::FileStatus;
use crate::git::feed::Entry;
use crate::hotspots::Grouping;
use crate::labels::Size;
use crate::theme;

/// Minimum terminal width at which the preview pane is shown.
const PREVIEW_MIN_WIDTH: u16 = 130;
const HINTS: Hints = &[
    ("j/k", "move"),
    ("⏎", "open"),
    ("n", "next new"),
    ("/", "search"),
    ("?", "keys"),
    ("q", "quit"),
];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Hints {
    let (list_area, preview_area) = if area.width >= PREVIEW_MIN_WIDTH {
        let preview_w = (area.width / 3).clamp(40, 56);
        let [l, p] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(preview_w)]).areas(area);
        (l, Some(p))
    } else {
        (area, None)
    };

    let depth = app.depth;
    let total = app.feed.visible(depth).len();
    let mut block = panel(true).title(Line::from(vec![
        Span::raw(" Changes "),
        Span::styled(format!("{total} "), theme::muted()),
    ]));
    if let Some(filter) = &app.feed.filter {
        let kind = match filter.grouping {
            Grouping::Files => "file",
            Grouping::Modules => "module",
        };
        block = block.title_bottom(
            Line::from(vec![
                Span::styled(format!(" {kind} "), theme::muted()),
                Span::styled(filter.key.clone(), Style::default().fg(theme::HIGHLIGHT)),
                Span::styled("  esc clears ", theme::muted()),
            ])
            .alignment(Alignment::Right),
        );
    }
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);

    if !app.feed.loaded && app.feed.error.is_none() {
        empty_state(
            frame,
            inner,
            &format!("{} Reading history", spinner(app.tick)),
            "",
        );
        return HINTS;
    }
    if let Some(err) = &app.feed.error {
        empty_state(frame, inner, "Couldn't read the repository", err);
        return HINTS;
    }
    if total == 0 {
        let (title, detail) = app.feed.filter.as_ref().map_or(
            (
                "Nothing here yet".to_owned(),
                "Commits and uncommitted work will appear as your agents make them.".to_owned(),
            ),
            |f| {
                (
                    format!("No changes touch {}", f.key),
                    "esc clears the filter".to_owned(),
                )
            },
        );
        empty_state(frame, inner, &title, &detail);
        if let Some(p) = preview_area {
            frame.render_widget(panel(false).title(" Preview "), p);
        }
        return HINTS;
    }

    let height = usize::from(inner.height);
    let offset = app.feed.pos.scrolled(height, total);
    let selected = app.feed.pos.selected;
    let visible = app.feed.visible(depth);
    let on_screen = &visible[offset..(offset + height).min(visible.len())];
    let columns = Columns::fit(on_screen);
    let lines: Vec<Line<'_>> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, e)| {
            row(
                e,
                columns,
                i == selected,
                app.feed.fresh.contains(&e.id),
                usize::from(inner.width),
                app.now,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    if let (Some(p), Some(entry)) = (preview_area, visible.get(selected)) {
        draw_preview(frame, p, entry, app.now);
    }
    HINTS
}

/// Coloured label chips for an entry.
pub(super) fn chips(entry: &Entry) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut push = |text: &str, style: Style| {
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(text.to_owned(), style));
    };
    if entry.reverted {
        push("↺ reverted", Style::default().fg(theme::REMOVED));
    }
    if entry.is_revert {
        push(
            "↩ revert",
            Style::default()
                .fg(theme::REMOVED)
                .add_modifier(Modifier::DIM),
        );
    }
    if entry.wide {
        push("⇄ wide", Style::default().fg(theme::INFO));
    }
    match entry.size {
        Size::Small => {}
        Size::Medium => push("▴ medium", theme::muted()),
        Size::Large => push("▲ large", Style::default().fg(theme::WARN)),
        Size::Xl => push(
            "▲ xl",
            Style::default()
                .fg(theme::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
        ),
    }
    out
}

/// Widths of the variable columns, sized to the rows on screen.
#[derive(Debug, Clone, Copy)]
struct Columns {
    /// Label chips, including leading gap (0 when no row has labels).
    chips: usize,
    /// `+added −removed` text.
    counts: usize,
}

impl Columns {
    fn fit(entries: &[&Entry]) -> Self {
        let chips = entries
            .iter()
            .map(|e| spans_width(&chips(e)))
            .max()
            .unwrap_or(0);
        let counts = entries
            .iter()
            .map(|e| spans_width(&stat_spans(e.added, e.removed)))
            .max()
            .unwrap_or(0);
        Self {
            chips: if chips == 0 { 0 } else { chips + 2 },
            counts,
        }
    }
}
/// Width of the author column.
const AUTHOR_W: usize = 14;
/// Narrowest subject worth showing before optional columns are dropped.
const MIN_SUBJECT_W: usize = 24;

fn row(
    entry: &Entry,
    columns: Columns,
    selected: bool,
    fresh: bool,
    width: usize,
    now: i64,
) -> Line<'static> {
    let mut left = vec![
        marker(selected),
        if fresh {
            Span::styled("✦ ", Style::default().fg(theme::HIGHLIGHT))
        } else {
            Span::raw("  ")
        },
        Span::styled(
            format!("{:>4}  ", relative(now, entry.time)),
            theme::muted(),
        ),
        if entry.is_worktree() {
            Span::styled(
                "working",
                Style::default()
                    .fg(theme::ADDED)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                entry.short.clone(),
                if selected {
                    theme::accent()
                } else {
                    Style::default().fg(theme::WARN)
                },
            )
        },
        Span::raw("  "),
    ];

    // Right-hand fixed columns: stats, churn bar, file count.
    let counts = stat_spans(entry.added, entry.removed);
    let mut right = vec![Span::raw(
        " ".repeat(2 + columns.counts.saturating_sub(spans_width(&counts))),
    )];
    right.extend(counts);
    right.push(Span::raw("  "));
    right.extend(churn_bar(entry.added, entry.removed, 5));
    let files = entry.files.len();
    right.push(Span::styled(
        format!(
            "  {files:>3} {} ",
            if files == 1 { "file " } else { "files" }
        ),
        theme::muted(),
    ));

    let fixed = spans_width(&left) + spans_width(&right);
    let mut subject_w = width.saturating_sub(fixed);
    let show_chips = columns.chips > 0 && subject_w >= MIN_SUBJECT_W + columns.chips;
    if show_chips {
        subject_w -= columns.chips;
    }
    let show_author = !entry.author.is_empty() && subject_w >= MIN_SUBJECT_W + AUTHOR_W + 20;
    if show_author {
        subject_w -= AUTHOR_W;
    }

    let subject = if entry.is_worktree() {
        let n = entry.modules.len();
        format!(
            "Uncommitted changes across {n} module{}",
            if n == 1 { "" } else { "s" }
        )
    } else {
        entry.summary.clone()
    };
    let subject_style = if selected {
        theme::accent_bold()
    } else {
        theme::text()
    };
    let mut spans = left.split_off(0);
    spans.push(Span::styled(pad(&subject, subject_w), subject_style));
    if show_chips {
        let chip_spans = chips(entry);
        let used = spans_width(&chip_spans);
        spans.push(Span::raw("  "));
        spans.extend(chip_spans);
        spans.push(Span::raw(
            " ".repeat(columns.chips.saturating_sub(2 + used)),
        ));
    }
    if show_author {
        spans.push(Span::styled(pad(&entry.author, AUTHOR_W), theme::muted()));
    }
    spans.extend(right);
    Line::from(spans)
}

fn status_style(status: FileStatus) -> Style {
    Style::default().fg(match status {
        FileStatus::Added => theme::ADDED,
        FileStatus::Modified => theme::WARN,
        FileStatus::Deleted => theme::REMOVED,
        FileStatus::Renamed => theme::INFO,
    })
}

/// File row used by the preview and the detail file list.
pub(super) fn file_row(
    path: &str,
    status: FileStatus,
    added: u64,
    removed: u64,
    binary: bool,
    width: usize,
    selected: bool,
) -> Line<'static> {
    let counts = if binary {
        vec![Span::styled("bin", theme::muted())]
    } else {
        stat_spans(added, removed)
    };
    let counts_w = spans_width(&counts);
    let name_style = if selected {
        theme::accent_bold()
    } else {
        theme::text()
    };
    let path_w = width.saturating_sub(5 + counts_w + 1);
    let path = path_spans(path, path_w, name_style);
    let used = spans_width(&path);
    let mut spans = vec![
        marker(selected),
        Span::styled(format!("{} ", status.letter()), status_style(status)),
        Span::raw(" "),
    ];
    spans.extend(path);
    spans.push(Span::raw(" ".repeat(path_w.saturating_sub(used) + 1)));
    spans.extend(counts);
    Line::from(spans)
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, entry: &Entry, now: i64) {
    let block = panel(false).title(" Preview ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let w = usize::from(inner.width.saturating_sub(2));

    let mut lines = vec![Line::from(Span::styled(
        if entry.is_worktree() {
            "Uncommitted changes".to_owned()
        } else {
            entry.summary.clone()
        },
        theme::bold(),
    ))];
    let mut meta = vec![Span::styled(
        entry.short.clone(),
        Style::default().fg(theme::WARN),
    )];
    if !entry.author.is_empty() {
        meta.push(Span::styled(format!(" · {}", entry.author), theme::muted()));
    }
    meta.push(Span::styled(
        format!(" · {}", ago(now, entry.time)),
        theme::muted(),
    ));
    lines.push(Line::from(meta));
    lines.push(Line::raw(""));

    let mut summary = stat_spans(entry.added, entry.removed);
    summary.push(Span::styled(
        format!(
            "  in {} · {}",
            plural(entry.files.len(), "file"),
            plural(entry.modules.len(), "module")
        ),
        theme::muted(),
    ));
    lines.push(Line::from(summary));
    let chip_spans = chips(entry);
    if !chip_spans.is_empty() {
        lines.push(Line::from(chip_spans));
    }

    if !entry.body.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(
            entry
                .body
                .lines()
                .take(8)
                .map(|l| Line::from(Span::styled(truncate(l, w), theme::muted()))),
        );
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("Modules", theme::accent_bold())));
    let modules = module_list(&entry.modules, "  ");
    lines.push(Line::from(Span::styled(
        format!("  {}", truncate(&modules, w.saturating_sub(2))),
        Style::default().fg(theme::INFO),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("Files", theme::accent_bold())));
    let used = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(lines);
    let [top, rest] = Layout::vertical([
        Constraint::Length(used.min(inner.height)),
        Constraint::Min(0),
    ])
    .areas(inner);
    let pad = |r: Rect| Rect::new(r.x + 1, r.y, r.width.saturating_sub(2), r.height);
    frame.render_widget(paragraph, pad(top));

    let max_files = usize::from(rest.height);
    let mut file_lines: Vec<Line<'_>> = entry
        .files
        .iter()
        .take(max_files)
        .map(|f| file_row(&f.path, f.status, f.added, f.removed, f.binary, w, false))
        .collect();
    if entry.files.len() > max_files && max_files > 0 {
        file_lines.pop();
        file_lines.push(Line::from(Span::styled(
            format!("   … and {} more", entry.files.len() - max_files + 1),
            theme::muted(),
        )));
    }
    frame.render_widget(Paragraph::new(file_lines), pad(rest));
}
