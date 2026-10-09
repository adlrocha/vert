//! The change detail view (file list + diff) and the file viewer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::feed::{chips, file_row};
use super::{Hints, empty_state, module_list, panel, plural, spans_width, stat_spans, truncate};
use crate::age::ago;
use crate::app::{DetailView, FileView, Focus};
use crate::git::diff::{FileDiff, LineKind};
use crate::highlight::{Highlighter, sanitize};
use crate::theme;

/// Footer hints for the detail view.
pub const DETAIL_HINTS: Hints = &[
    ("tab", "pane"),
    ("j/k", "move"),
    ("J/K", "scroll"),
    ("n/N", "hunk"),
    ("⏎", "view file"),
    ("e", "edit"),
    ("esc", "back"),
];

/// Footer hints for the file viewer.
pub const FILE_HINTS: Hints = &[
    ("j/k", "scroll"),
    ("ctrl-d/u", "page"),
    ("n/N", "next change"),
    ("e", "edit"),
    ("esc", "back"),
];

/// A diff pre-rendered into gutter and code columns.
#[derive(Debug, Default, Clone)]
pub struct RenderedDiff {
    /// Line-number gutter, one per row.
    pub gutter: Vec<Line<'static>>,
    /// Code, one per row.
    pub code: Vec<Line<'static>>,
    /// The new-file line number of each row, if any.
    pub new_lines: Vec<Option<u32>>,
    /// Rows where hunks start.
    pub hunk_starts: Vec<usize>,
    /// Gutter width in columns.
    pub gutter_width: u16,
}

impl RenderedDiff {
    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// The first new-file line number at or after `row`.
    #[must_use]
    pub fn new_line_at(&self, row: usize) -> Option<u32> {
        self.new_lines.get(row..)?.iter().flatten().next().copied()
    }

    fn push(&mut self, gutter: Line<'static>, code: Line<'static>, new_no: Option<u32>) {
        self.gutter.push(gutter);
        self.code.push(code);
        self.new_lines.push(new_no);
    }

    fn message(text: String) -> Self {
        let mut r = Self::default();
        r.push(
            Line::raw(""),
            Line::from(Span::styled(text, theme::muted())),
            None,
        );
        r
    }
}

fn num(n: Option<u32>, w: usize) -> String {
    n.map_or_else(|| " ".repeat(w), |n| format!("{n:>w$}"))
}

/// Pre-renders `file`'s diff with syntax highlighting on the new side.
#[must_use]
pub fn build_diff(file: &FileDiff, max_highlight: usize) -> RenderedDiff {
    if file.stat.binary {
        return RenderedDiff::message("Binary file — contents not shown".to_owned());
    }
    if file.hunks.is_empty() {
        let text = file.stat.old_path.as_ref().map_or_else(
            || "No content changes".to_owned(),
            |old| format!("Renamed from {old}, contents unchanged"),
        );
        return RenderedDiff::message(text);
    }
    let all = || file.hunks.iter().flat_map(|h| &h.lines);
    let max_no = all()
        .flat_map(|l| [l.old_no, l.new_no])
        .flatten()
        .max()
        .unwrap_or(1);
    let w = max_no.to_string().len().max(3);
    let new_side: Vec<&str> = all()
        .filter(|l| l.kind != LineKind::Removed)
        .map(|l| l.content.as_str())
        .collect();
    let mut highlighted = Highlighter::global()
        .highlight(&file.stat.path, &new_side, max_highlight)
        .into_iter();

    let mut out = RenderedDiff {
        gutter_width: u16::try_from(2 * w + 4).unwrap_or(u16::MAX),
        ..RenderedDiff::default()
    };
    for (h, hunk) in file.hunks.iter().enumerate() {
        if h > 0 {
            out.push(Line::raw(""), Line::raw(""), None);
        }
        out.hunk_starts.push(out.len());
        let (range, context) = split_header(&hunk.header);
        out.push(
            Line::from(Span::styled(
                format!("{:>width$}", "⋯ ", width = 2 * w + 3),
                theme::muted(),
            )),
            Line::from(vec![
                Span::styled(range.to_owned(), theme::muted()),
                Span::styled(
                    format!(" {context}"),
                    Style::default()
                        .fg(theme::INFO)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]),
            hunk.lines.iter().find_map(|l| l.new_no),
        );
        for line in &hunk.lines {
            let (old_style, new_style, bar) = match line.kind {
                LineKind::Context => (theme::muted(), theme::muted(), Span::raw("  ")),
                LineKind::Added => (
                    theme::muted(),
                    Style::default().fg(theme::ADDED),
                    Span::styled("▌ ", Style::default().fg(theme::ADDED)),
                ),
                LineKind::Removed => (
                    Style::default().fg(theme::REMOVED),
                    theme::muted(),
                    Span::styled("▌ ", Style::default().fg(theme::REMOVED)),
                ),
            };
            let gutter = Line::from(vec![
                Span::styled(num(line.old_no, w), old_style),
                Span::raw(" "),
                Span::styled(num(line.new_no, w), new_style),
                Span::raw(" "),
                bar,
            ]);
            let code = match line.kind {
                LineKind::Removed => Line::from(Span::styled(
                    sanitize(&line.content),
                    Style::default().fg(theme::REMOVED),
                )),
                LineKind::Added | LineKind::Context => {
                    let fragments = highlighted.next().unwrap_or_default();
                    let dim = line.kind == LineKind::Context;
                    Line::from(
                        fragments
                            .into_iter()
                            .map(|(style, text)| {
                                Span::styled(
                                    text,
                                    if dim {
                                        style.add_modifier(Modifier::DIM)
                                    } else {
                                        style
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                }
            };
            out.push(gutter, code, line.new_no);
        }
    }
    out
}

/// Splits `@@ -1,2 +1,3 @@ fn ctx` into the range part and the context.
fn split_header(header: &str) -> (&str, &str) {
    header
        .get(2..)
        .and_then(|rest| rest.find("@@"))
        .map_or((header, ""), |i| {
            let end = i + 4;
            (&header[..end], header[end..].trim())
        })
}

/// Draws the detail view.
pub fn draw_detail(frame: &mut Frame<'_>, area: Rect, d: &mut DetailView, now: i64) {
    let [summary, panes] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    draw_summary(frame, summary, d, now);

    let files_w = (panes.width * 3 / 10).clamp(28, 56).min(panes.width / 2);
    let [files_area, diff_area] =
        Layout::horizontal([Constraint::Length(files_w), Constraint::Min(10)]).areas(panes);

    let block = panel(d.focus == Focus::Files).title(Line::from(vec![
        Span::raw(" Files "),
        Span::styled(format!("{} ", d.files.len()), theme::muted()),
    ]));
    let inner = block.inner(files_area);
    frame.render_widget(block, files_area);
    let height = usize::from(inner.height);
    let offset = d.pos.scrolled(height, d.files.len());
    let rows: Vec<Line<'_>> = d
        .files
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, f)| {
            let s = &f.stat;
            file_row(
                &s.path,
                s.status,
                s.added,
                s.removed,
                s.binary,
                usize::from(inner.width),
                i == d.pos.selected,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);

    let Some(file) = d.files.get(d.pos.selected) else {
        let block = panel(false);
        let inner = block.inner(diff_area);
        frame.render_widget(block, diff_area);
        empty_state(frame, inner, "No changes", "the working tree matches HEAD");
        return;
    };
    let mut title = vec![Span::raw(format!(" {} ", file.stat.path))];
    if let Some(old) = &file.stat.old_path {
        title.push(Span::styled(format!("← {old} "), theme::muted()));
    }
    let position = format!(
        " {}/{} ",
        (d.scroll + 1).min(d.rendered.len()),
        d.rendered.len()
    );
    let block = panel(d.focus == Focus::Diff)
        .title(Line::from(title))
        .title_bottom(
            Line::from(Span::styled(position, theme::muted())).alignment(Alignment::Right),
        );
    let inner = block.inner(diff_area);
    frame.render_widget(block, diff_area);
    d.height = usize::from(inner.height);
    render_columns(
        frame,
        inner,
        d.rendered.gutter_width,
        &d.rendered.gutter,
        &d.rendered.code,
        d.scroll,
        d.hscroll,
    );
}

fn render_columns(
    frame: &mut Frame<'_>,
    area: Rect,
    gutter_w: u16,
    gutter: &[Line<'static>],
    code: &[Line<'static>],
    scroll: usize,
    hscroll: usize,
) {
    let [g, c] = Layout::horizontal([Constraint::Length(gutter_w), Constraint::Min(1)]).areas(area);
    let h = usize::from(area.height);
    let slice = |lines: &[Line<'static>]| {
        lines
            .iter()
            .skip(scroll)
            .take(h)
            .cloned()
            .collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(slice(gutter)), g);
    let x = u16::try_from(hscroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(slice(code)).scroll((0, x)), c);
}

fn draw_summary(frame: &mut Frame<'_>, area: Rect, d: &DetailView, now: i64) {
    let e = &d.entry;
    let chip_spans = chips(e);
    let chip_w = spans_width(&chip_spans);
    let title_w = usize::from(area.width).saturating_sub(chip_w + 4);
    let title = if e.is_worktree() {
        "Uncommitted changes"
    } else {
        e.summary.as_str()
    };
    let first = Line::from(vec![
        Span::raw(" "),
        Span::styled(truncate(title, title_w), theme::bold()),
    ]);
    let mut meta = vec![
        Span::raw(" "),
        Span::styled(e.short.clone(), Style::default().fg(theme::WARN)),
    ];
    if !e.author.is_empty() {
        meta.push(Span::styled(format!(" · {}", e.author), theme::muted()));
    }
    meta.push(Span::styled(
        format!(" · {} · ", ago(now, e.time)),
        theme::muted(),
    ));
    meta.extend(stat_spans(e.added, e.removed));
    meta.push(Span::styled(
        format!(
            " · {} · {}",
            plural(e.files.len(), "file"),
            module_list(&e.modules, ", ")
        ),
        theme::muted(),
    ));
    let [l1, l2, _] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    frame.render_widget(first, l1);
    let mut right = chip_spans;
    right.push(Span::raw(" "));
    frame.render_widget(Line::from(right).alignment(Alignment::Right), l1);
    frame.render_widget(Line::from(meta), l2);
}

/// Draws the file viewer.
pub fn draw_file(frame: &mut Frame<'_>, area: Rect, f: &mut FileView) {
    let total = f.lines.len();
    let mut title = vec![Span::raw(format!(" {} ", f.path))];
    if !f.language.is_empty() {
        title.push(Span::styled(format!("{} ", f.language), theme::muted()));
    }
    let mut bottom = Vec::new();
    if !f.marks.is_empty() {
        bottom.push(Span::styled("▌", Style::default().fg(theme::ADDED)));
        bottom.push(Span::styled(
            format!(" {} changed lines  ", f.marks.len()),
            theme::muted(),
        ));
    }
    bottom.push(Span::styled(
        format!("{}/{} ", (f.scroll + 1).min(total), total),
        theme::muted(),
    ));
    let block = panel(true)
        .title(Line::from(title))
        .title_bottom(Line::from(bottom).alignment(Alignment::Right));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    f.height = usize::from(inner.height);
    if let Some(notice) = &f.notice {
        empty_state(frame, inner, "Can't show this file", notice);
        return;
    }
    if total == 0 {
        empty_state(frame, inner, "Empty file", "");
        return;
    }
    let w = total.to_string().len().max(3);
    let target = usize::try_from(f.target).unwrap_or(0);
    let rows = f.lines.iter().enumerate().skip(f.scroll).take(f.height);
    let (gutter, code): (Vec<_>, Vec<_>) = rows
        .map(|(i, frags)| {
            let n = i + 1;
            let mark = if u32::try_from(n).is_ok_and(|n| f.marks.contains(&n)) {
                Span::styled("▌", Style::default().fg(theme::ADDED))
            } else {
                Span::raw(" ")
            };
            let num_style = if n == target {
                theme::accent_bold()
            } else {
                theme::muted()
            };
            let pointer = if n == target { "›" } else { " " };
            let gutter = Line::from(vec![
                mark,
                Span::styled(format!("{n:>w$}"), num_style),
                Span::styled(format!(" {pointer} "), theme::accent()),
            ]);
            let code = Line::from(
                frags
                    .iter()
                    .map(|(style, text)| Span::styled(text.clone(), *style))
                    .collect::<Vec<_>>(),
            );
            (gutter, code)
        })
        .unzip();
    let gutter_w = u16::try_from(w + 4).unwrap_or(u16::MAX);
    render_columns(frame, inner, gutter_w, &gutter, &code, 0, f.hscroll);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DiffLine, Hunk};
    use crate::git::{FileStat, FileStatus};

    fn stat(binary: bool, old_path: Option<&str>) -> FileStat {
        FileStat {
            path: "src/a.rs".to_owned(),
            old_path: old_path.map(str::to_owned),
            status: FileStatus::Modified,
            added: 1,
            removed: 1,
            binary,
        }
    }

    fn line(kind: LineKind, old_no: Option<u32>, new_no: Option<u32>, content: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no,
            new_no,
            content: content.to_owned(),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn renders_hunks_with_gutters() {
        let file = FileDiff {
            stat: stat(false, None),
            hunks: vec![
                Hunk {
                    header: "@@ -1,2 +1,2 @@ fn main()".to_owned(),
                    lines: vec![
                        line(LineKind::Context, Some(1), Some(1), "fn main() {"),
                        line(LineKind::Removed, Some(2), None, "    old();"),
                        line(LineKind::Added, None, Some(2), "    new();"),
                    ],
                },
                Hunk {
                    header: "@@ -9 +9 @@".to_owned(),
                    lines: vec![line(LineKind::Added, None, Some(9), "x")],
                },
            ],
        };
        let r = build_diff(&file, 100);
        assert_eq!(r.hunk_starts, [0, 5]);
        assert_eq!(r.len(), 7);
        assert!(!r.is_empty());
        assert_eq!(text(&r.code[0]), "@@ -1,2 +1,2 @@ fn main()");
        assert_eq!(text(&r.code[2]), "    old();");
        assert_eq!(text(&r.code[3]), "    new();");
        assert_eq!(text(&r.gutter[3]), "      2 ▌ ");
        assert_eq!(r.gutter_width, 10);
        assert_eq!(r.new_line_at(2), Some(2));
        assert_eq!(r.new_line_at(4), Some(9));
        assert_eq!(r.new_line_at(99), None);
    }

    #[test]
    fn binary_and_rename_messages() {
        let bin = build_diff(
            &FileDiff {
                stat: stat(true, None),
                hunks: vec![],
            },
            100,
        );
        assert!(text(&bin.code[0]).contains("Binary"));
        let renamed = build_diff(
            &FileDiff {
                stat: stat(false, Some("old.rs")),
                hunks: vec![],
            },
            100,
        );
        assert!(text(&renamed.code[0]).contains("Renamed from old.rs"));
        let empty = build_diff(
            &FileDiff {
                stat: stat(false, None),
                hunks: vec![],
            },
            100,
        );
        assert_eq!(text(&empty.code[0]), "No content changes");
    }

    #[test]
    fn header_split() {
        assert_eq!(split_header("@@ -1 +1 @@ ctx"), ("@@ -1 +1 @@", "ctx"));
        assert_eq!(split_header("@@ -1 +1 @@"), ("@@ -1 +1 @@", ""));
        assert_eq!(split_header("weird"), ("weird", ""));
        assert_eq!(num(None, 3), "   ");
        assert_eq!(num(Some(7), 3), "  7");
    }
}
