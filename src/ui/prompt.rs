//! The box text is typed into for an agent: feedback to fix, or a
//! follow-up question.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::{centered, panel, wrap};
use crate::theme;

/// Widest the box grows to.
const MAX_WIDTH: u16 = 76;
/// Most lines of typed text shown; earlier ones scroll out of view.
const MAX_TEXT_LINES: usize = 6;
/// The usual keys, for text sent to an agent.
pub(super) const SEND: &str = "⏎ send · esc cancel";

/// Draws a box over everything else, centred on `area`, where text is
/// typed for an agent: the lines `head` makes for the inner width, a blank
/// line, then `typed` with a cursor, its last lines when it is long, and
/// `hint` (the keys) on the bottom border.
pub(super) fn draw_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    (title, hint): (&str, &str),
    typed: &str,
    head: impl FnOnce(usize) -> Vec<Line<'static>>,
) {
    let box_w = MAX_WIDTH.min(area.width);
    let inner = usize::from(box_w.saturating_sub(4)).max(1);
    let mut lines = head(inner);
    lines.push(Line::raw(""));
    let typed = format!("{typed}▏");
    let wrapped = wrap(&typed, inner);
    let skip = wrapped.len().saturating_sub(MAX_TEXT_LINES);
    lines.extend(
        wrapped
            .into_iter()
            .skip(skip)
            .map(|l| Line::from(Span::styled(l, theme::text()))),
    );
    let box_h = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centered(area, box_w, box_h);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(true)
                .title(title.to_owned())
                .title_bottom(Line::from(format!(" {hint} ")).alignment(Alignment::Right))
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}
