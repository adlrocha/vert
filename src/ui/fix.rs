//! The feedback prompt: what the agent will be asked to fix, and where.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::{centered, panel, truncate, wrap};
use crate::app::App;
use crate::theme;

/// Widest the prompt grows to.
const MAX_WIDTH: u16 = 76;
/// Most lines of feedback shown; earlier ones scroll out of view.
const MAX_TEXT_LINES: usize = 6;
const HINT: &str = "⏎ send · esc cancel";

/// Draws the feedback prompt over everything else, when one is open.
pub(super) fn draw_draft(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(draft) = &app.fix.draft else {
        return;
    };
    let box_w = MAX_WIDTH.min(area.width);
    let inner = usize::from(box_w.saturating_sub(4)).max(1);
    let agent = match &app.fix.agent {
        Some(Ok(agent)) => agent.as_str(),
        _ => "agent",
    };
    let place = draft
        .request
        .place
        .as_deref()
        .unwrap_or("the change as a whole");
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(&format!("{agent} edits {}", draft.checkout), inner),
            theme::bold(),
        )),
        Line::from(Span::styled(truncate(place, inner), theme::muted())),
        Line::raw(""),
    ];
    let typed = format!("{}▏", draft.text);
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
                .title(" Fix ")
                .title_bottom(Line::from(format!(" {HINT} ")).alignment(Alignment::Right))
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        rect,
    );
}
