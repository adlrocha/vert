//! Search: fuzzy file names and content matches.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Hints, empty_state, marker, panel, spinner, truncate, truncate_left, width};
use crate::app::{App, SearchMode};
use crate::highlight::sanitize;
use crate::search::{FileMatch, TextMatch};
use crate::theme;

const HINTS: Hints = &[
    ("type", "search"),
    ("tab", "files/text"),
    ("↑/↓", "move"),
    ("⏎", "open"),
    ("ctrl-e", "edit"),
    ("esc", "back"),
];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Hints {
    let [input_area, results_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    draw_input(frame, input_area, app);
    draw_results(frame, results_area, app);
    HINTS
}

fn pill(label: &str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            format!(" {label} "),
            theme::accent_bold().add_modifier(Modifier::REVERSED),
        )
    } else {
        Span::styled(format!(" {label} "), theme::muted())
    }
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mode = app.search.mode;
    let block = panel(true).title(" Search ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let placeholder = match mode {
        SearchMode::Files => "file name…",
        SearchMode::Text => "regex or text…",
    };
    let mut spans = vec![Span::styled(" › ", theme::accent_bold())];
    if app.search.query.is_empty() {
        spans.push(Span::styled("▏", theme::accent()));
        spans.push(Span::styled(placeholder, theme::muted()));
    } else {
        let max = usize::from(inner.width).saturating_sub(20);
        spans.push(Span::styled(
            truncate_left(&app.search.query, max),
            theme::bold(),
        ));
        spans.push(Span::styled("▏", theme::accent()));
    }
    frame.render_widget(Line::from(spans), inner);
    frame.render_widget(
        Line::from(vec![
            pill("files", mode == SearchMode::Files),
            Span::raw(" "),
            pill("text", mode == SearchMode::Text),
            Span::raw(" "),
        ])
        .alignment(Alignment::Right),
        inner,
    );
}

fn status(app: &App) -> Line<'static> {
    let s = &app.search;
    let text = match s.mode {
        SearchMode::Files => match &s.files {
            _ if s.files_loading() => format!("{} indexing", spinner(app.tick)),
            Some(files) => format!("{} of {} files", s.file_hits.len(), files.len()),
            None => String::new(),
        },
        SearchMode::Text if s.text_running => format!("{} searching", spinner(app.tick)),
        SearchMode::Text => {
            let mut t = format!(
                "{} match{}",
                s.text.matches.len(),
                if s.text.matches.len() == 1 { "" } else { "es" }
            );
            if s.text.literal {
                t.push_str(" · literal");
            }
            if s.text.truncated {
                t.push_str(" · capped");
            }
            t
        }
    };
    Line::from(Span::styled(format!(" {text} "), theme::muted())).alignment(Alignment::Right)
}

fn draw_results(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let block = panel(false).title(" Results ").title_bottom(status(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let s = &mut app.search;
    if let Some(err) = &s.error {
        empty_state(frame, inner, "Search failed", err);
        return;
    }
    if s.is_empty() {
        let (title, detail) = match s.mode {
            _ if s.files_loading() || s.text_running => {
                (format!("{} working", spinner(app.tick)), "")
            }
            SearchMode::Text if s.query.is_empty() => (
                "Search file contents".to_owned(),
                "regex · smart case · respects .gitignore",
            ),
            SearchMode::Files if s.query.is_empty() => {
                ("Find a file".to_owned(), "fuzzy · respects .gitignore")
            }
            _ => (
                "No matches".to_owned(),
                "tab switches between files and text",
            ),
        };
        empty_state(frame, inner, &title, detail);
        return;
    }

    let height = usize::from(inner.height);
    let cols = usize::from(inner.width);
    let offset = s.pos.scrolled(height, s.len());
    let selected = s.pos.selected;
    let lines: Vec<Line<'_>> = match s.mode {
        SearchMode::Files => s
            .file_hits
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(i, hit)| file_line(hit, i == selected, cols))
            .collect(),
        SearchMode::Text => {
            let path_w = s
                .text
                .matches
                .iter()
                .map(|m| width(&m.path))
                .max()
                .unwrap_or(0)
                .min(cols / 3);
            s.text
                .matches
                .iter()
                .enumerate()
                .skip(offset)
                .take(height)
                .map(|(i, m)| {
                    let same_file = i > offset && s.text.matches[i - 1].path == m.path;
                    text_line(m, i == selected, same_file, path_w, cols)
                })
                .collect()
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn file_line(hit: &FileMatch, selected: bool, max: usize) -> Line<'static> {
    let dir_end = hit.path.rfind('/').map_or(0, |i| i + 1);
    let mut spans = vec![marker(selected), Span::raw("  ")];
    let shown = truncate(&hit.path, max.saturating_sub(4));
    for (ci, (bi, c)) in shown.char_indices().enumerate() {
        let style = if hit.indices.binary_search(&ci).is_ok() {
            theme::matched()
        } else if bi < dir_end {
            theme::muted()
        } else if selected {
            theme::accent_bold()
        } else {
            theme::text()
        };
        spans.push(Span::styled(c.to_string(), style));
    }
    Line::from(spans)
}

fn text_line(
    m: &TextMatch,
    selected: bool,
    same_file: bool,
    path_w: usize,
    max: usize,
) -> Line<'static> {
    let path = if same_file {
        format!("{:>path_w$}", "│")
    } else {
        format!("{:>path_w$}", truncate_left(&m.path, path_w))
    };
    let path_style = if selected {
        theme::accent()
    } else {
        theme::muted()
    };
    let mut spans = vec![
        marker(selected),
        Span::raw(" "),
        Span::styled(path, path_style),
        Span::styled(
            format!(" {:>5}  ", m.line),
            Style::default().fg(theme::WARN),
        ),
    ];
    let indent = m.text.len() - m.text.trim_start().len();
    let budget = max.saturating_sub(path_w + 10);
    let mut cursor = indent;
    let mut used = 0;
    let base = if selected {
        theme::bold()
    } else {
        theme::text()
    };
    let mut push = |text: &str, style: Style, used: &mut usize| {
        let t = truncate(&sanitize(text), budget.saturating_sub(*used));
        *used += width(&t);
        spans.push(Span::styled(t, style));
    };
    for &(start, end) in &m.ranges {
        let (start, end) = (start.max(cursor), end.max(cursor));
        if let (Some(before), Some(hit)) = (m.text.get(cursor..start), m.text.get(start..end)) {
            push(before, base, &mut used);
            push(hit, theme::matched(), &mut used);
            cursor = end;
        }
    }
    if let Some(rest) = m.text.get(cursor..) {
        push(rest, base, &mut used);
    }
    Line::from(spans)
}
