//! Syntax highlighting with an ANSI theme that follows the terminal palette.

use std::str::FromStr;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// A highlighted fragment of a line.
pub type Fragment = (Style, String);

/// Highlights source code into ratatui styles.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// Width tabs are expanded to.
pub const TAB_WIDTH: usize = 4;

/// Encodes an ANSI palette index the way `bat` does (alpha 0 = indexed colour).
const fn ansi(index: u8) -> SynColor {
    SynColor {
        r: index,
        g: 0,
        b: 0,
        a: 0,
    }
}

/// The terminal's default foreground.
const DEFAULT_FG: SynColor = SynColor {
    r: 0,
    g: 0,
    b: 0,
    a: 1,
};

/// Scope → (ANSI index, font style) rules, most specific last.
const RULES: &[(&str, u8, FontStyle)] = &[
    (
        "comment, punctuation.definition.comment",
        8,
        FontStyle::ITALIC,
    ),
    (
        "string, constant.character, markup.inline.raw, markup.raw",
        2,
        FontStyle::empty(),
    ),
    (
        "constant.numeric, constant.language, constant.other, support.constant",
        6,
        FontStyle::empty(),
    ),
    (
        "constant.character.escape, string.regexp",
        6,
        FontStyle::empty(),
    ),
    ("keyword, storage, keyword.control", 5, FontStyle::empty()),
    ("keyword.operator", 0xFF, FontStyle::empty()),
    (
        "entity.name.function, support.function, meta.function-call variable.function, variable.function",
        4,
        FontStyle::empty(),
    ),
    (
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class, storage.type",
        3,
        FontStyle::empty(),
    ),
    ("variable.language, entity.name.tag", 1, FontStyle::empty()),
    (
        "entity.other.attribute-name, meta.attribute, meta.annotation",
        3,
        FontStyle::empty(),
    ),
    (
        "entity.name.namespace, entity.name.module",
        6,
        FontStyle::empty(),
    ),
    ("markup.heading, entity.name.section", 4, FontStyle::BOLD),
    ("markup.bold", 0xFF, FontStyle::BOLD),
    ("markup.italic", 0xFF, FontStyle::ITALIC),
    (
        "markup.underline.link, string.other.link",
        4,
        FontStyle::UNDERLINE,
    ),
    ("markup.inserted", 2, FontStyle::empty()),
    ("markup.deleted", 1, FontStyle::empty()),
];

fn ansi_theme() -> Theme {
    let scopes = RULES
        .iter()
        .filter_map(|(selector, index, font)| {
            let scope = ScopeSelectors::from_str(selector).ok()?;
            Some(ThemeItem {
                scope,
                style: StyleModifier {
                    foreground: Some(if *index == 0xFF {
                        DEFAULT_FG
                    } else {
                        ansi(*index)
                    }),
                    background: None,
                    font_style: Some(*font),
                },
            })
        })
        .collect();
    Theme {
        name: Some("vert-ansi".to_owned()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(DEFAULT_FG),
            ..ThemeSettings::default()
        },
        scopes,
    }
}

fn to_color(c: SynColor) -> Color {
    match c.a {
        0 => Color::Indexed(c.r),
        1 => Color::Reset,
        _ => Color::Rgb(c.r, c.g, c.b),
    }
}

fn to_style(s: syntect::highlighting::Style) -> Style {
    let mut style = Style::default().fg(to_color(s.foreground));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Replaces tabs with spaces and strips control characters that would corrupt the display.
#[must_use]
pub fn sanitize(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '\t' => std::iter::repeat_n(' ', TAB_WIDTH),
            c if c.is_control() => std::iter::repeat_n('·', 1),
            c => std::iter::repeat_n(c, 1),
        })
        .collect()
}

impl Highlighter {
    /// The shared instance (syntax definitions load once, lazily).
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<Highlighter> = OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: ansi_theme(),
        })
    }

    fn syntax_for(&self, path: &str, first_line: Option<&str>) -> &SyntaxReference {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.').map_or(name, |(_, ext)| ext);
        self.syntaxes
            .find_syntax_by_extension(name)
            .or_else(|| self.syntaxes.find_syntax_by_extension(ext))
            .or_else(|| first_line.and_then(|l| self.syntaxes.find_syntax_by_first_line(l)))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }

    /// Name of the language detected for `path`.
    #[must_use]
    pub fn language(&self, path: &str, first_line: Option<&str>) -> &str {
        &self.syntax_for(path, first_line).name
    }

    /// Highlights consecutive `lines` of the file at `path`.
    ///
    /// Each returned line is a list of styled fragments with tabs expanded.
    /// If more than `max_lines` lines are given, highlighting is skipped and
    /// plain text is returned to keep the UI responsive.
    #[must_use]
    pub fn highlight(&self, path: &str, lines: &[&str], max_lines: usize) -> Vec<Vec<Fragment>> {
        let plain = |l: &&str| vec![(Style::default(), sanitize(l))];
        if lines.len() > max_lines {
            return lines.iter().map(plain).collect();
        }
        let syntax = self.syntax_for(path, lines.first().copied());
        let mut h = HighlightLines::new(syntax, &self.theme);
        lines
            .iter()
            .map(|line| {
                let with_nl = format!("{line}\n");
                h.highlight_line(&with_nl, &self.syntaxes).map_or_else(
                    |_| plain(line),
                    |ranges| {
                        ranges
                            .into_iter()
                            .map(|(s, t)| (to_style(s), sanitize(t.trim_end_matches('\n'))))
                            .filter(|(_, t)| !t.is_empty())
                            .collect()
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages() {
        let h = Highlighter::global();
        assert_eq!(h.language("src/main.rs", None), "Rust");
        assert_eq!(h.language("Makefile", None), "Makefile");
        assert_eq!(
            h.language("script", Some("#!/bin/bash")),
            "Bourne Again Shell (bash)"
        );
        assert_eq!(h.language("notes.unknownext", None), "Plain Text");
    }

    #[test]
    fn highlights_keywords_with_ansi_colours() {
        let h = Highlighter::global();
        let out = h.highlight("a.rs", &["fn main() {}", "// hi"], 100);
        assert_eq!(out.len(), 2);
        let text: String = out[0].iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(text, "fn main() {}");
        assert!(
            out[0]
                .iter()
                .any(|(s, _)| matches!(s.fg, Some(Color::Indexed(_))))
        );
        assert!(out[1][0].0.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn skips_highlighting_past_limit() {
        let h = Highlighter::global();
        let out = h.highlight("a.rs", &["fn a()", "fn b()"], 1);
        assert_eq!(out[0], vec![(Style::default(), "fn a()".to_owned())]);
    }

    #[test]
    fn sanitizes_tabs_and_controls() {
        assert_eq!(sanitize("\tx\u{1b}y"), "    x·y");
    }

    #[test]
    fn colour_and_style_conversion() {
        assert_eq!(to_color(ansi(3)), Color::Indexed(3));
        assert_eq!(to_color(DEFAULT_FG), Color::Reset);
        assert_eq!(
            to_color(SynColor {
                r: 1,
                g: 2,
                b: 3,
                a: 255
            }),
            Color::Rgb(1, 2, 3)
        );
        let s = to_style(syntect::highlighting::Style {
            foreground: ansi(1),
            background: ansi(0),
            font_style: FontStyle::BOLD | FontStyle::UNDERLINE,
        });
        assert!(
            s.add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
    }
}
