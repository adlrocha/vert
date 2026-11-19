//! The agent picker (`M`): which coding agent and model vert asks.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::prompt::draw_prompt;
use super::{truncate, wrap};
use crate::app::{App, model_hint, pick_name};
use crate::theme;

const HINT: &str = "←→ agent · type the model · ⏎ use · esc cancel";

/// Draws the agent picker over everything else, when it is open.
pub(super) fn draw_picker(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.agents.picker else {
        return;
    };
    let picks = app.agents.picks();
    let chosen = picks.get(picker.selected).copied();
    draw_prompt(frame, area, (" Agent ", HINT), &picker.model, |inner| {
        let mut row = Vec::new();
        for (i, pick) in picks.iter().enumerate() {
            if i > 0 {
                row.push(Span::raw("  "));
            }
            row.push(if i == picker.selected {
                Span::styled(format!("[{}]", pick_name(*pick)), theme::accent_bold())
            } else {
                Span::styled(format!(" {} ", pick_name(*pick)), theme::muted())
            });
        }
        let mut lines = vec![
            Line::from(Span::styled(
                truncate("Explain, ask and fix with", inner),
                theme::bold(),
            )),
            Line::from(row),
            Line::raw(""),
            Line::from(Span::styled(
                truncate("Model (blank for the agent's default)", inner),
                theme::bold(),
            )),
        ];
        if let Some(pick) = chosen {
            lines.extend(
                wrap(model_hint(pick), inner)
                    .into_iter()
                    .map(|l| Line::from(Span::styled(l, theme::muted()))),
            );
        }
        lines.extend(
            wrap(
                "Until vert exits; VERT_EXPLAIN_AGENT and VERT_EXPLAIN_MODEL set what it starts with",
                inner,
            )
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme::muted()))),
        );
        lines
    });
}
