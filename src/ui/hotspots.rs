//! Hotspots: where change concentrates, with smooth proportional bars.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Hints, empty_state, marker, panel, path_spans, spans_width, stat_spans};
use crate::age::relative;
use crate::app::App;
use crate::hotspots::{Grouping, Hotspot};
use crate::theme;

const HINTS: Hints = &[
    ("w", "window"),
    ("m", "files/modules"),
    ("⏎", "filter feed"),
    ("o", "view"),
    ("e", "edit"),
    ("?", "keys"),
];

/// Narrowest and widest the change bar may be.
const MIN_BAR_W: usize = 8;
const MAX_BAR_W: usize = 40;

const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// A bar of `value / max` filling up to `cells`, with 1/8-cell resolution.
pub(super) fn bar(value: usize, max: usize, cells: usize) -> String {
    if max == 0 || cells == 0 {
        return String::new();
    }
    let eighths = (value * cells * 8).div_ceil(max).max(1);
    let full = eighths / 8;
    let rest = eighths % 8;
    let mut s = "█".repeat(full.min(cells));
    if rest > 0 && full < cells {
        s.push_str(EIGHTHS[rest - 1]);
    }
    s
}

/// Column widths shared by every hotspot row.
struct RowLayout {
    rank_w: usize,
    path_w: usize,
    bar_w: usize,
    max_changes: usize,
    grouping: Grouping,
    now: i64,
}

impl RowLayout {
    fn row(&self, index: usize, spot: &Hotspot, selected: bool) -> Line<'static> {
        let name_style = if selected {
            theme::accent_bold()
        } else {
            theme::text()
        };
        let label = match self.grouping {
            Grouping::Files => spot.key.clone(),
            Grouping::Modules => format!("{}/", spot.key),
        };
        let path = path_spans(&label, self.path_w, name_style);
        let used = spans_width(&path);
        let mut spans = vec![
            marker(selected),
            Span::styled(
                format!("{:>w$}  ", index + 1, w = self.rank_w),
                theme::muted(),
            ),
        ];
        spans.extend(path);
        spans.push(Span::raw(" ".repeat(self.path_w.saturating_sub(used) + 1)));
        let b = bar(spot.changes, self.max_changes, self.bar_w);
        let bar_used = super::width(&b);
        let bar_style = if selected {
            theme::accent()
        } else {
            Style::default().fg(theme::INFO).add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(b, bar_style));
        spans.push(Span::raw(
            " ".repeat(self.bar_w.saturating_sub(bar_used) + 2),
        ));
        spans.push(Span::styled(
            format!(
                "{:>3} change{}",
                spot.changes,
                if spot.changes == 1 { " " } else { "s" }
            ),
            theme::text(),
        ));
        let churn = stat_spans(spot.added, spot.removed);
        let churn_w = spans_width(&churn);
        spans.push(Span::raw(" ".repeat(18usize.saturating_sub(churn_w))));
        spans.extend(churn);
        spans.push(Span::styled(
            format!("  {:>5}", relative(self.now, spot.last)),
            theme::muted(),
        ));
        Line::from(spans)
    }
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Hints {
    let state = &mut app.hotspots;
    let title = Line::from(vec![
        Span::raw(" Hotspots "),
        Span::styled(
            format!("{} · {} ", state.grouping.label(), state.window.label()),
            theme::muted(),
        ),
    ]);
    let block = panel(true).title(title).title_bottom(
        Line::from(Span::styled(
            format!(
                " ranked by changes in the last {} loaded commits ",
                app.feed.commits.len()
            ),
            theme::muted(),
        ))
        .alignment(Alignment::Right),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.spots.is_empty() {
        empty_state(
            frame,
            inner,
            "No activity in this window",
            "press w to widen the time window",
        );
        return HINTS;
    }

    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let len = state.spots.len();
    let offset = state.pos.scrolled(height, len);
    let max_changes = state.spots.first().map_or(1, |s| s.changes);
    let rank_w = len.to_string().len();
    // Path column fits the longest visible path; the bar takes what is left.
    let stats_w = 1 + 11 + 2 + 16 + 2 + 5;
    let longest = state
        .spots
        .iter()
        .skip(offset)
        .take(height)
        .map(|s| super::width(&s.key) + 1)
        .max()
        .unwrap_or(10);
    let room = width.saturating_sub(1 + rank_w + 2 + stats_w + 2);
    let path_w = longest.min(room.saturating_sub(MIN_BAR_W)).max(10);
    let bar_w = room.saturating_sub(path_w).clamp(MIN_BAR_W, MAX_BAR_W);

    let layout = RowLayout {
        rank_w,
        path_w,
        bar_w,
        max_changes,
        grouping: state.grouping,
        now: app.now,
    };
    let lines: Vec<Line<'_>> = state
        .spots
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, spot)| layout.row(i, spot, i == state.pos.selected))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    HINTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_scale_smoothly() {
        assert_eq!(bar(10, 10, 4), "████");
        assert_eq!(bar(5, 10, 4), "██");
        assert_eq!(bar(1, 16, 2), "▏");
        assert_eq!(bar(3, 16, 2), "▍");
        assert_eq!(bar(0, 10, 4), "▏", "non-empty for visibility");
        assert_eq!(bar(1, 0, 4), "");
        assert_eq!(bar(1, 1, 0), "");
    }
}
