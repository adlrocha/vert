//! The feedback prompt: what the agent will be asked to fix, and where.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::prompt::{SEND, draw_prompt};
use super::{truncate, wrap};
use crate::app::App;
use crate::theme;

/// Draws the feedback prompt over everything else, when one is open.
pub(super) fn draw_draft(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(draft) = &app.fix.draft else {
        return;
    };
    let agent = match &app.fix.agent {
        Some(Ok(agent)) => agent.as_str(),
        _ => "agent",
    };
    let place = draft
        .request
        .place
        .as_deref()
        .unwrap_or("the change as a whole");
    let unconfined = app.fix.unconfined;
    draw_prompt(
        frame,
        area,
        (" Fix ", SEND),
        &draft.request.feedback,
        |inner| {
            let mut lines = vec![
                Line::from(Span::styled(
                    truncate(&format!("{agent} edits {}", draft.checkout), inner),
                    theme::bold(),
                )),
                Line::from(Span::styled(truncate(place, inner), theme::muted())),
            ];
            if unconfined {
                lines.extend(
                wrap(
                    "it can't be kept inside the checkout: its edits can land anywhere you can write",
                    inner,
                )
                .into_iter()
                .map(|l| Line::from(Span::styled(l, Style::default().fg(theme::WARN)))),
            );
            }
            lines
        },
    );
}
