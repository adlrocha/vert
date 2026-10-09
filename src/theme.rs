//! Semantic colour tokens.
//!
//! vert only uses the terminal's 16 ANSI colours, so it inherits whatever
//! theme the terminal is configured with instead of imposing its own palette.

use ratatui::style::{Color, Modifier, Style};

/// Primary accent: selection, focus, brand.
pub const ACCENT: Color = Color::Blue;
/// Secondary accent: fuzzy-match highlights, "new" markers.
pub const HIGHLIGHT: Color = Color::Magenta;
/// De-emphasised text and borders.
pub const MUTED: Color = Color::DarkGray;
/// Added lines, live status.
pub const ADDED: Color = Color::Green;
/// Removed lines, reverts, errors.
pub const REMOVED: Color = Color::Red;
/// Warnings and breadth.
pub const WARN: Color = Color::Yellow;
/// Informational emphasis.
pub const INFO: Color = Color::Cyan;

/// Default text.
#[must_use]
pub fn text() -> Style {
    Style::default()
}

/// Dimmed text.
#[must_use]
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

/// Bold text.
#[must_use]
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Accent text.
#[must_use]
pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Bold accent text.
#[must_use]
pub fn accent_bold() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

/// Border style, highlighted when focused.
#[must_use]
pub fn border(focused: bool) -> Style {
    if focused { accent() } else { muted() }
}

/// Matched-character style in search results.
#[must_use]
pub fn matched() -> Style {
    Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)
}
