//! Rendering. Everything here is a pure function of [`App`] state, except
//! that list offsets and viewport heights are written back for scrolling.

pub mod detail;
mod feed;
mod hotspots;
mod search;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Tab, View, WatchStatus};
use crate::theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draws the whole interface.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    let hints = match app.views.last_mut() {
        Some(View::Detail(d)) => {
            draw_crumbs(frame, tabs, &["Feed", &d.entry.short]);
            detail::draw_detail(frame, body, d, app.now);
            detail::DETAIL_HINTS
        }
        Some(View::File(f)) => {
            draw_crumbs(frame, tabs, &["File", split_path(&f.path).1]);
            detail::draw_file(frame, body, f);
            detail::FILE_HINTS
        }
        None => {
            draw_tabs(frame, tabs, app);
            match app.tab {
                Tab::Feed => feed::draw(frame, body, app),
                Tab::Hotspots => hotspots::draw(frame, body, app),
                Tab::Search => search::draw(frame, body, app),
            }
        }
    };
    draw_footer(frame, footer, hints, app.notice.as_deref());
    if app.help {
        draw_help(frame);
    }
}

/// The current spinner frame.
fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut left = vec![
        Span::styled(" ◆ vert ", theme::accent_bold()),
        Span::styled("│ ", theme::muted()),
        Span::styled(app.repo.name.clone(), theme::bold()),
    ];
    if let Some(branch) = &app.repo.branch {
        left.push(Span::styled(format!("  ⎇ {branch}"), theme::muted()));
    }
    let mut right = Vec::new();
    if !app.feed.fresh.is_empty() {
        right.push(Span::styled(
            format!("✦ {} new  ", app.feed.fresh.len()),
            Style::default().fg(theme::HIGHLIGHT),
        ));
    }
    if app.feed.loading() {
        right.push(Span::styled(
            format!("{} syncing ", spinner(app.tick)),
            theme::muted(),
        ));
    } else {
        right.push(match &app.watch {
            WatchStatus::Live => Span::styled("● live ", Style::default().fg(theme::ADDED)),
            WatchStatus::Disabled => Span::styled("○ paused ", theme::muted()),
            WatchStatus::Failed(_) => {
                Span::styled("○ not watching ", Style::default().fg(theme::WARN))
            }
        });
    }
    frame.render_widget(Line::from(left), area);
    frame.render_widget(Line::from(right).alignment(Alignment::Right), area);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let active = *tab == app.tab;
        spans.push(Span::styled(format!(" {} ", i + 1), theme::muted()));
        let style = if active {
            theme::accent_bold().add_modifier(Modifier::UNDERLINED)
        } else {
            theme::text()
        };
        spans.push(Span::styled(tab.title(), style));
        spans.push(Span::raw("   "));
    }
    let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Line::from(spans), row);
}

fn draw_crumbs(frame: &mut Frame<'_>, area: Rect, parts: &[&str]) {
    let mut spans = vec![Span::raw("  ")];
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        let style = if i == last {
            theme::bold()
        } else {
            theme::muted()
        };
        spans.push(Span::styled((*part).to_owned(), style));
        if i != last {
            spans.push(Span::styled("  ›  ", theme::muted()));
        }
    }
    let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Line::from(spans), row);
}

/// Key hints shown in the footer: `(keys, description)`.
pub type Hints = &'static [(&'static str, &'static str)];

fn draw_footer(frame: &mut Frame<'_>, area: Rect, hints: Hints, notice: Option<&str>) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (keys, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", theme::muted()));
        }
        spans.push(Span::styled(*keys, theme::accent()));
        spans.push(Span::styled(format!(" {desc}"), theme::muted()));
    }
    frame.render_widget(Line::from(spans), area);
    if let Some(notice) = notice {
        frame.render_widget(
            Line::from(Span::styled(
                format!("{notice} "),
                Style::default().fg(theme::WARN),
            ))
            .alignment(Alignment::Right),
            area,
        );
    }
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Everywhere",
        &[
            ("1 2 3 / tab", "switch tab"),
            ("j k ↑ ↓", "move"),
            ("ctrl-d ctrl-u", "half page"),
            ("g G", "top / bottom"),
            ("⏎", "open"),
            ("esc q", "back / quit"),
            ("r", "reload"),
        ],
    ),
    ("Feed", &[("n", "next new change"), ("esc", "clear filter")]),
    (
        "Hotspots",
        &[
            ("w", "time window"),
            ("m", "files / modules"),
            ("⏎", "filter feed"),
            ("o e", "view / edit file"),
        ],
    ),
    (
        "Change",
        &[
            ("tab h l", "files / diff pane"),
            ("J K", "scroll diff"),
            ("n N", "next / prev hunk"),
            ("← →", "scroll sideways"),
            ("⏎ e", "view / edit file"),
        ],
    ),
    (
        "Search",
        &[
            ("type", "filter"),
            ("tab", "files / text"),
            ("ctrl-w ctrl-u", "delete word / all"),
            ("ctrl-e", "edit result"),
        ],
    ),
];

fn draw_help(frame: &mut Frame<'_>) {
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        lines.push(Line::from(Span::styled(*section, theme::accent_bold())));
        for (k, desc) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<16}"), theme::bold()),
                Span::styled(*desc, theme::muted()),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines.pop();
    let height = u16::try_from(lines.len() + 4).unwrap_or(u16::MAX);
    let area = centered(frame.area(), 48, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(true)
                .title(" Keys ")
                .title_bottom(Line::from(" any key to close ").alignment(Alignment::Right))
                .padding(Padding::new(2, 2, 1, 1)),
        ),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// A rounded panel, accented when focused.
fn panel(focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title_style(if focused {
            theme::accent_bold()
        } else {
            theme::bold()
        })
}

/// Renders a centred, friendly message inside `area`.
fn empty_state(frame: &mut Frame<'_>, area: Rect, title: &str, detail: &str) {
    let text = Text::from(vec![
        Line::from(Span::styled(title.to_owned(), theme::bold())),
        Line::from(Span::styled(detail.to_owned(), theme::muted())),
    ])
    .alignment(Alignment::Center);
    let top = area.height.saturating_sub(2) / 2;
    let [_, mid] = Layout::vertical([Constraint::Length(top), Constraint::Min(2)]).areas(area);
    frame.render_widget(Paragraph::new(text), mid);
}

/// Display width of `s` in terminal columns.
#[must_use]
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncates `s` to at most `max` columns, ending in `…` when cut.
#[must_use]
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Truncates from the left (keeping the end, which matters most for paths).
#[must_use]
pub fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// Pads `s` with spaces to exactly `w` columns (truncating if longer).
#[must_use]
pub fn pad(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let fill = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(fill))
}

/// Splits a path into (directory with trailing slash, file name).
#[must_use]
pub fn split_path(path: &str) -> (&str, &str) {
    path.rfind('/')
        .map_or(("", path), |i| (&path[..=i], &path[i + 1..]))
}

/// A path rendered with a muted directory and a bold file name, fitted to `max` columns.
fn path_spans(path: &str, max: usize, name_style: Style) -> Vec<Span<'static>> {
    let (dir, name) = split_path(path);
    let name_w = width(name);
    if name_w >= max {
        return vec![Span::styled(truncate(name, max), name_style)];
    }
    vec![
        Span::styled(truncate_left(dir, max - name_w), theme::muted()),
        Span::styled(name.to_owned(), name_style),
    ]
}

/// GitHub-style churn bar: `cells` squares split between additions and removals.
fn churn_bar(added: u64, removed: u64, cells: usize) -> Vec<Span<'static>> {
    let total = added + removed;
    if total == 0 {
        return vec![Span::styled("■".repeat(cells), theme::muted())];
    }
    let cells_u64 = cells as u64;
    let mut green = usize::try_from((added * cells_u64).div_ceil(total)).unwrap_or(cells);
    if removed > 0 && green == cells {
        green = cells - 1;
    }
    vec![
        Span::styled("■".repeat(green), Style::default().fg(theme::ADDED)),
        Span::styled(
            "■".repeat(cells - green),
            Style::default().fg(theme::REMOVED),
        ),
    ]
}

/// `+12 −4` with colours, omitting zero sides.
fn stat_spans(added: u64, removed: u64) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if added > 0 || removed == 0 {
        spans.push(Span::styled(
            format!("+{added}"),
            Style::default().fg(theme::ADDED),
        ));
    }
    if removed > 0 {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("−{removed}"),
            Style::default().fg(theme::REMOVED),
        ));
    }
    spans
}

/// `"1 file"`, `"3 files"`.
#[must_use]
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Module names for display, with the repository root shown as `(root)`.
#[must_use]
pub fn module_list(modules: &[String], sep: &str) -> String {
    modules
        .iter()
        .map(|m| if m == "." { "(root)" } else { m.as_str() })
        .collect::<Vec<_>>()
        .join(sep)
}

/// Total display width of spans.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// Left selection marker.
fn marker(selected: bool) -> Span<'static> {
    if selected {
        Span::styled("▌", theme::accent())
    } else {
        Span::raw(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語テキスト", 5), "日本…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate_left("src/deep/path/file.rs", 10), "…h/file.rs");
        assert_eq!(truncate_left("short", 10), "short");
        assert_eq!(truncate_left("abc", 0), "");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abc…");
    }

    #[test]
    fn paths_split_and_fit() {
        assert_eq!(split_path("src/ui/mod.rs"), ("src/ui/", "mod.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
        let spans = path_spans("src/ui/mod.rs", 9, theme::bold());
        assert_eq!(spans_width(&spans), 9);
        let tight = path_spans("src/averyveryverylongname.rs", 6, theme::bold());
        assert_eq!(tight.len(), 1);
    }

    #[test]
    fn churn_bars_and_stats() {
        let text = |spans: Vec<Span<'_>>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(churn_bar(10, 0, 5)), ["■■■■■", ""]);
        assert_eq!(text(churn_bar(10, 1, 5)), ["■■■■", "■"]);
        assert_eq!(text(churn_bar(1, 10, 5)), ["■", "■■■■"]);
        assert_eq!(text(churn_bar(0, 0, 3)), ["■■■"]);
        assert_eq!(text(stat_spans(3, 0)), ["+3"]);
        assert_eq!(text(stat_spans(0, 2)), ["−2"]);
        assert_eq!(text(stat_spans(3, 2)), ["+3", " ", "−2"]);
        assert_eq!(text(stat_spans(0, 0)), ["+0"]);
    }

    #[test]
    fn wording_helpers() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
        let modules = vec![".".to_owned(), "src".to_owned()];
        assert_eq!(module_list(&modules, ", "), "(root), src");
    }

    #[test]
    fn centering_and_spinner() {
        let r = centered(Rect::new(0, 0, 100, 50), 40, 10);
        assert_eq!(r, Rect::new(30, 20, 40, 10));
        let clipped = centered(Rect::new(0, 0, 20, 5), 40, 10);
        assert_eq!(clipped, Rect::new(0, 0, 20, 5));
        assert_eq!(spinner(0), spinner(SPINNER.len()));
    }
}

#[cfg(test)]
mod render_tests {
    //! Renders every view at several terminal sizes to catch layout panics
    //! and check that the key content appears.
    use std::collections::BTreeSet;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::draw;
    use crate::app::{App, Msg, WatchStatus};
    use crate::git::RepoInfo;
    use crate::git::testutil::TestRepo;
    use crate::labels::Labeler;
    use crate::search::{TextMatch, TextResults};
    use crate::{git, search};

    const SIZES: [(u16, u16); 4] = [(160, 40), (100, 30), (60, 16), (20, 6)];

    fn press(app: &mut App, code: KeyCode) {
        app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// An app loaded from a real repository with history and dirty files.
    fn real_app() -> (TestRepo, App) {
        let t = TestRepo::new();
        t.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
        t.write("README.md", "# demo\n");
        t.commit_at("init: scaffold", 1_000);
        t.write(
            "src/main.rs",
            "fn main() {\n    println!(\"hello\");\n}\n\nfn extra() {}\n",
        );
        t.write("docs/guide.md", "guide\n");
        t.write("lib/a.rs", "pub fn a() {}\n");
        t.commit_at("feat: a wide change across modules", 2_000);
        t.write("src/main.rs", "fn main() {}\n");
        let labeler = Labeler::new(2, 4, 8, 3, 1).expect("labeler");
        let (mut app, _) = App::new(
            RepoInfo {
                name: "demo".to_owned(),
                branch: Some("main".to_owned()),
            },
            1,
            1000,
            3_000,
        );
        app.update(Msg::Commits(
            git::feed::load_commits(&t.repo, 10, &labeler).map_err(|e| e.to_string()),
        ));
        app.update(Msg::Worktree(
            git::feed::load_worktree(&t.repo, &labeler, 3_000).map_err(|e| e.to_string()),
        ));
        (t, app)
    }

    #[test]
    fn feed_renders_at_all_sizes() {
        let (_t, mut app) = real_app();
        for (w, h) in SIZES {
            let s = screen(&mut app, w, h);
            if w >= 100 {
                assert!(s.contains("working"), "{w}x{h}:\n{s}");
                assert!(s.contains("a wide change"), "{w}x{h}:\n{s}");
                assert!(s.contains("wide"), "{w}x{h}:\n{s}");
            }
        }
        let wide = screen(&mut app, 160, 40);
        assert!(wide.contains("Preview"));
        assert!(wide.contains("live"));
        assert!(wide.contains("Modules"));
    }

    #[test]
    fn feed_states_render() {
        let (mut app, _) = App::new(
            RepoInfo {
                name: "x".to_owned(),
                branch: None,
            },
            1,
            100,
            0,
        );
        assert!(screen(&mut app, 100, 20).contains("Reading history"));
        assert!(screen(&mut app, 100, 20).contains("syncing"));
        app.update(Msg::Commits(Ok(Vec::new())));
        app.update(Msg::Worktree(Ok(None)));
        assert!(screen(&mut app, 160, 20).contains("Nothing here yet"));
        press(&mut app, KeyCode::Char('2'));
        assert!(screen(&mut app, 100, 20).contains("No activity"));
        press(&mut app, KeyCode::Char('1'));
        app.update(Msg::Commits(Err("disk on fire".to_owned())));
        assert!(screen(&mut app, 100, 20).contains("disk on fire"));
        app.watch = WatchStatus::Disabled;
        assert!(screen(&mut app, 100, 20).contains("paused"));
        app.watch = WatchStatus::Failed("limit".to_owned());
        assert!(screen(&mut app, 100, 20).contains("not watching"));
        app.update(Msg::Notice("heads up".to_owned()));
        assert!(screen(&mut app, 100, 20).contains("heads up"));
        press(&mut app, KeyCode::Char('?'));
        assert!(screen(&mut app, 100, 40).contains("any key to close"));
    }

    #[test]
    fn filtered_feed_and_new_markers_render() {
        let (_t, mut app) = real_app();
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Enter);
        let s = screen(&mut app, 120, 20);
        assert!(s.contains("esc clears"), "{s}");
        app.feed.filter = Some(crate::app::Filter {
            key: "nope".to_owned(),
            grouping: crate::hotspots::Grouping::Files,
        });
        assert!(screen(&mut app, 120, 20).contains("No changes touch nope"));
        app.feed.filter = None;
        let id = app.feed.commits[0].id.clone();
        app.feed.fresh.insert(id);
        let s = screen(&mut app, 120, 20);
        assert!(s.contains("✦ 1 new"), "{s}");
    }

    #[test]
    fn hotspots_render() {
        let (_t, mut app) = real_app();
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('w'));
        for (w, h) in SIZES {
            screen(&mut app, w, h);
        }
        let s = screen(&mut app, 120, 20);
        assert!(s.contains("main.rs"), "{s}");
        assert!(s.contains("changes"), "{s}");
        press(&mut app, KeyCode::Char('m'));
        assert!(screen(&mut app, 120, 20).contains("src/"));
    }

    #[test]
    fn search_renders_both_modes() {
        let (t, mut app) = real_app();
        press(&mut app, KeyCode::Char('/'));
        assert!(screen(&mut app, 100, 20).contains("file name…"));
        app.update(Msg::Files(search::list_files(t.path())));
        let all = screen(&mut app, 100, 20);
        assert!(
            all.contains("README.md") && all.contains("4 of 4 files"),
            "{all}"
        );
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('a'));
        let s = screen(&mut app, 100, 20);
        assert!(s.contains("main.rs"), "{s}");
        assert!(s.contains("of 4 files"), "{s}");
        for c in "zzzz".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(screen(&mut app, 100, 20).contains("No matches"));

        press(&mut app, KeyCode::Tab);
        assert!(screen(&mut app, 100, 20).contains("searching"));
        let generation = app.search.generation;
        let result = TextResults {
            matches: vec![
                TextMatch {
                    path: "src/main.rs".to_owned(),
                    line: 1,
                    text: "    fn main() {}".to_owned(),
                    ranges: vec![(7, 11)],
                },
                TextMatch {
                    path: "src/main.rs".to_owned(),
                    line: 5,
                    text: "fn extra() {}".to_owned(),
                    ranges: vec![(0, 2)],
                },
            ],
            literal: true,
            truncated: true,
            cancelled: false,
        };
        app.update(Msg::Text {
            generation,
            result: Ok(result),
        });
        for (w, h) in SIZES {
            screen(&mut app, w, h);
        }
        let s = screen(&mut app, 100, 20);
        assert!(s.contains("2 matches · literal · capped"), "{s}");
        assert!(s.contains("│"), "grouped path column:\n{s}");
        app.update(Msg::Text {
            generation,
            result: Err("bad regex".to_owned()),
        });
        assert!(screen(&mut app, 100, 20).contains("bad regex"));
        app.search.query.clear();
        app.search.error = None;
        app.search.text = TextResults::default();
        assert!(screen(&mut app, 100, 20).contains("Search file contents"));
    }

    #[test]
    fn detail_and_file_views_render() {
        let (t, mut app) = real_app();
        let entry = app.feed.commits[0].clone();
        let files = git::diff::load(&t.repo, entry.target).map_err(|e| e.to_string());
        app.update(Msg::Diff {
            entry: Box::new(entry),
            result: files,
            refresh: false,
        });
        for (w, h) in SIZES {
            screen(&mut app, w, h);
        }
        let s = screen(&mut app, 140, 30);
        assert!(s.contains("Files 3"), "{s}");
        assert!(s.contains("a wide change"), "{s}");
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('J'));
        screen(&mut app, 140, 30);

        app.update(Msg::File {
            path: "src/main.rs".to_owned(),
            content: Ok("fn main() {}\n\tlet x = 1;\n".to_owned()),
            line: 2,
            marks: BTreeSet::from([2]),
        });
        for (w, h) in SIZES {
            screen(&mut app, w, h);
        }
        let s = screen(&mut app, 100, 20);
        assert!(s.contains("Rust"), "{s}");
        assert!(s.contains("1 changed lines"), "{s}");
        assert!(s.contains("›"), "{s}");
        press(&mut app, KeyCode::Esc);

        app.update(Msg::File {
            path: "empty.txt".to_owned(),
            content: Ok(String::new()),
            line: 1,
            marks: BTreeSet::new(),
        });
        assert!(screen(&mut app, 100, 20).contains("Empty file"));
        press(&mut app, KeyCode::Esc);
        app.update(Msg::File {
            path: "b.bin".to_owned(),
            content: Err("It's a binary file.".to_owned()),
            line: 1,
            marks: BTreeSet::new(),
        });
        assert!(screen(&mut app, 100, 20).contains("binary"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);

        app.update(Msg::Diff {
            entry: Box::new(app.feed.commits[0].clone()),
            result: Ok(Vec::new()),
            refresh: false,
        });
        assert!(screen(&mut app, 100, 20).contains("No changes"));
    }

    #[test]
    fn labels_preview_and_file_kinds_render() {
        use crate::git::feed::Entry;
        use crate::git::{FileStat, FileStatus, Target};
        use crate::labels::Size;

        let file = |path: &str, status, binary| FileStat {
            path: path.to_owned(),
            old_path: None,
            status,
            added: 3,
            removed: 1,
            binary,
        };
        let mk = |id: &str, size, reverted, is_revert| Entry {
            target: Target::Commit(git2::Oid::ZERO_SHA1),
            id: id.to_owned(),
            short: id.to_owned(),
            summary: format!("change {id}"),
            body: "Why this change matters.\nSecond line.".to_owned(),
            author: "claude".to_owned(),
            time: 0,
            files: (0..40)
                .map(|i| match i % 4 {
                    0 => file(&format!("src/f{i}.rs"), FileStatus::Deleted, false),
                    1 => file(&format!("src/f{i}.rs"), FileStatus::Renamed, false),
                    2 => file(&format!("img/f{i}.png"), FileStatus::Added, true),
                    _ => file(&format!("f{i}.rs"), FileStatus::Modified, false),
                })
                .collect(),
            added: 3,
            removed: 1,
            modules: vec![".".to_owned(), "src".to_owned()],
            size,
            wide: true,
            reverted,
            is_revert,
        };
        let (mut app, _) = App::new(
            RepoInfo {
                name: "x".to_owned(),
                branch: None,
            },
            1,
            100,
            0,
        );
        app.update(Msg::Commits(Ok(vec![
            mk("aaaaaaa", Size::Xl, true, false),
            mk("bbbbbbb", Size::Medium, false, true),
            mk("ccccccc", Size::Small, false, false),
        ])));
        app.update(Msg::Worktree(Ok(None)));
        let s = screen(&mut app, 200, 30);
        for needle in [
            "↺ reverted",
            "↩ revert",
            "▲ xl",
            "▴ medium",
            "⇄ wide",
            "claude",
        ] {
            assert!(s.contains(needle), "missing {needle}:\n{s}");
        }
        assert!(s.contains("Why this change matters."), "{s}");
        assert!(s.contains("(root)"), "{s}");
        assert!(s.contains("more"), "file overflow line:\n{s}");
        assert!(s.contains("bin"), "{s}");

        app.feed.filter = Some(crate::app::Filter {
            key: "src".to_owned(),
            grouping: crate::hotspots::Grouping::Modules,
        });
        assert!(screen(&mut app, 200, 30).contains("module"));
        app.feed.filter = None;

        let mut renamed = mk("ddddddd", Size::Small, false, false);
        renamed.files[0].old_path = Some("src/old.rs".to_owned());
        app.update(Msg::Diff {
            entry: Box::new(renamed.clone()),
            result: Ok(vec![crate::git::diff::FileDiff {
                stat: renamed.files[0].clone(),
                hunks: Vec::new(),
            }]),
            refresh: false,
        });
        assert!(screen(&mut app, 140, 20).contains("← src/old.rs"));
        app.views.clear();

        let mut wt = mk("worktree", Size::Small, false, false);
        wt.target = Target::WorkingTree;
        app.update(Msg::Diff {
            entry: Box::new(wt),
            result: Ok(Vec::new()),
            refresh: false,
        });
        assert!(screen(&mut app, 140, 20).contains("Uncommitted changes"));
    }

    #[test]
    fn search_shows_indexing_before_files_arrive() {
        let (_t, mut app) = real_app();
        press(&mut app, KeyCode::Char('/'));
        assert!(screen(&mut app, 100, 20).contains("indexing"));
        app.update(Msg::Files(Vec::new()));
        assert!(screen(&mut app, 100, 20).contains("Find a file"));
    }
}
