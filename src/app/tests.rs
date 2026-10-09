use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::*;
use crate::git::diff::{DiffLine, Hunk};
use crate::git::{FileStat, FileStatus, Target};
use crate::labels::Size;

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn stat(path: &str) -> FileStat {
    FileStat {
        path: path.to_owned(),
        old_path: None,
        status: FileStatus::Modified,
        added: 2,
        removed: 1,
        binary: false,
    }
}

fn entry(id: &str, time: i64, paths: &[&str]) -> Entry {
    let worktree = id == crate::git::feed::WORKTREE_ID;
    Entry {
        target: if worktree {
            Target::WorkingTree
        } else {
            Target::Commit(git2::Oid::ZERO_SHA1)
        },
        id: id.to_owned(),
        short: id.chars().take(7).collect(),
        summary: format!("summary {id}"),
        body: String::new(),
        author: "agent".to_owned(),
        time,
        files: paths.iter().map(|p| stat(p)).collect(),
        added: 2,
        removed: 1,
        modules: Vec::new(),
        size: Size::Small,
        wide: false,
        reverted: false,
        is_revert: false,
    }
}

fn file_diff(path: &str) -> FileDiff {
    let line = |kind, old_no, new_no, content: &str| DiffLine {
        kind,
        old_no,
        new_no,
        content: content.to_owned(),
    };
    FileDiff {
        stat: stat(path),
        hunks: vec![
            Hunk {
                header: "@@ -1,2 +1,2 @@".to_owned(),
                lines: vec![
                    line(LineKind::Context, Some(1), Some(1), "a"),
                    line(LineKind::Removed, Some(2), None, "b"),
                    line(LineKind::Added, None, Some(2), "B"),
                ],
            },
            Hunk {
                header: "@@ -20 +20 @@".to_owned(),
                lines: vec![line(LineKind::Added, None, Some(20), "z")],
            },
        ],
    }
}

fn info() -> RepoInfo {
    RepoInfo {
        name: "repo".to_owned(),
        branch: Some("main".to_owned()),
    }
}

/// An app with three commits loaded and no working-tree changes.
fn loaded() -> App {
    let (mut app, effects) = App::new(info(), 1, 1000, 10_000);
    assert_eq!(effects, [Effect::LoadCommits, Effect::LoadWorktree]);
    app.update(Msg::Commits(Ok(vec![
        entry("c3", 9_000, &["src/a.rs", "docs/x.md"]),
        entry("c2", 8_000, &["src/a.rs"]),
        entry("c1", 1_000, &["src/b.rs"]),
    ])));
    app.update(Msg::Worktree(Ok(None)));
    app
}

#[test]
fn initial_load_marks_nothing_fresh() {
    let app = loaded();
    assert!(app.feed.loaded);
    assert!(!app.feed.loading());
    assert!(app.feed.fresh.is_empty());
    assert_eq!(app.feed.visible(1).len(), 3);
    assert_eq!(app.hotspots.spots[0].key, "src/a.rs");
}

#[test]
fn new_commits_are_fresh_until_opened() {
    let mut app = loaded();
    let effects = app.update(Msg::Changed(BTreeSet::from([Change::Refs])));
    assert_eq!(effects, [Effect::LoadCommits, Effect::LoadWorktree]);
    app.update(Msg::Commits(Ok(vec![
        entry("c4", 9_500, &["src/new.rs"]),
        entry("c3", 9_000, &["src/a.rs"]),
    ])));
    assert!(app.feed.fresh.contains("c4"));
    // Selection stays on the previously selected entry (c3 moved to index 1).
    assert_eq!(app.feed.pos.selected, 1);
    app.update(ch('n'));
    assert_eq!(app.feed.pos.selected, 0);
    let effects = app.update(key(KeyCode::Enter));
    assert!(
        matches!(&effects[..], [Effect::LoadDiff { entry, refresh: false }] if entry.id == "c4")
    );
    assert!(app.feed.fresh.is_empty());
}

#[test]
fn loads_coalesce_while_in_flight() {
    let (mut app, _) = App::new(info(), 1, 1000, 0);
    let workdir = BTreeSet::from([Change::Workdir]);
    assert!(
        app.update(Msg::Changed(workdir.clone())).is_empty(),
        "worktree already loading"
    );
    assert!(app.update(Msg::Changed(workdir)).is_empty());
    assert_eq!(
        app.update(Msg::Worktree(Ok(None))),
        [Effect::LoadWorktree],
        "one follow-up"
    );
    assert!(app.update(Msg::Worktree(Ok(None))).is_empty());
    assert!(
        app.update(Msg::Changed(BTreeSet::from([Change::Refs])))
            .iter()
            .all(|e| *e != Effect::LoadCommits)
    );
    assert_eq!(app.update(Msg::Commits(Ok(vec![]))), [Effect::LoadCommits]);
    assert!(
        app.update(key(KeyCode::Char('r'))).is_empty(),
        "both busy: queued, not duplicated"
    );
    assert_eq!(app.update(Msg::Worktree(Ok(None))), [Effect::LoadWorktree]);
    assert_eq!(app.update(Msg::Commits(Ok(vec![]))), [Effect::LoadCommits]);
}

#[test]
fn load_errors_are_shown() {
    let (mut app, _) = App::new(info(), 1, 1000, 0);
    app.update(Msg::Commits(Err("boom".to_owned())));
    assert_eq!(app.feed.error.as_deref(), Some("boom"));
    app.update(Msg::Worktree(Err("bad".to_owned())));
    assert_eq!(app.feed.error.as_deref(), Some("bad"));
}

#[test]
fn worktree_entry_comes_first_and_refreshes_open_detail() {
    let mut app = loaded();
    let wt = entry(crate::git::feed::WORKTREE_ID, 9_999, &["src/live.rs"]);
    app.update(Msg::Worktree(Ok(Some(wt.clone()))));
    assert!(app.feed.visible(1)[0].is_worktree());
    app.feed.pos.selected = 0;
    app.update(Msg::Diff {
        entry: Box::new(wt.clone()),
        result: Ok(vec![file_diff("src/a.rs"), file_diff("src/live.rs")]),
        refresh: false,
    });
    if let Some(View::Detail(d)) = app.views.last_mut() {
        d.pos.selected = 1;
    }
    app.update(Msg::Changed(BTreeSet::from([Change::Workdir])));
    let effects = app.update(Msg::Worktree(Ok(Some(wt.clone()))));
    assert!(matches!(
        &effects[..],
        [Effect::LoadDiff { refresh: true, .. }]
    ));
    app.update(Msg::Diff {
        entry: Box::new(wt),
        result: Ok(vec![file_diff("src/live.rs")]),
        refresh: true,
    });
    let Some(View::Detail(d)) = app.views.last() else {
        panic!("detail")
    };
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.pos.selected, 0, "kept selection on src/live.rs");
}

#[test]
fn feed_navigation() {
    let mut app = loaded();
    app.update(ch('j'));
    assert_eq!(app.feed.pos.selected, 1);
    app.update(ch('G'));
    assert_eq!(app.feed.pos.selected, 2);
    app.update(ch('j'));
    assert_eq!(app.feed.pos.selected, 2, "clamped");
    app.update(ch('g'));
    assert_eq!(app.feed.pos.selected, 0);
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Up));
    assert_eq!(app.feed.pos.selected, 0);
    app.update(ctrl('d'));
    app.update(ctrl('u'));
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::End));
    assert_eq!(app.feed.pos.selected, 2);
    app.update(key(KeyCode::Home));
    assert_eq!(app.feed.pos.selected, 0);
}

#[test]
fn list_pos_keeps_selection_visible() {
    let mut pos = ListPos {
        selected: 9,
        ..ListPos::default()
    };
    assert_eq!(pos.scrolled(5, 20), 5);
    pos.selected = 2;
    assert_eq!(pos.scrolled(5, 20), 2);
    pos.selected = 50;
    assert_eq!(pos.scrolled(5, 20), 15);
    assert_eq!(pos.selected, 19);
    assert_eq!(pos.scrolled(5, 0), 0);
    pos.move_by(1, 0);
    assert_eq!(pos.selected, 0);
}

#[test]
fn tabs_help_and_quit() {
    let mut app = loaded();
    app.update(ch('2'));
    assert_eq!(app.tab, Tab::Hotspots);
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Search);
    app.update(key(KeyCode::Esc));
    assert_eq!(app.tab, Tab::Hotspots, "esc returns to the previous tab");
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Feed);
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Search);
    app.update(key(KeyCode::Esc));
    app.update(ch('1'));
    app.update(ch('?'));
    assert!(app.help);
    assert!(app.update(ch('q')).is_empty(), "any key closes help");
    assert!(!app.help);
    assert_eq!(app.update(ch('q')), [Effect::Quit]);
    assert_eq!(app.update(ctrl('c')), [Effect::Quit]);
    let release = KeyEvent::new_with_kind(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    );
    assert!(app.update(Msg::Key(release)).is_empty());
    let tab_names: Vec<_> = Tab::ALL.iter().map(|t| t.title()).collect();
    assert_eq!(tab_names, ["Feed", "Hotspots", "Search"]);
}

#[test]
fn hotspots_filter_feed_and_open_files() {
    let mut app = loaded();
    app.update(ch('2'));
    app.update(ch('w'));
    assert_eq!(app.hotspots.window, Window::Month);
    app.update(ch('w'));
    assert_eq!(app.hotspots.window, Window::All);
    assert_eq!(app.hotspots.spots.len(), 3);
    assert_eq!(
        app.update(ch('o')),
        [Effect::OpenFile {
            path: "src/a.rs".to_owned(),
            line: 1,
            marks: BTreeSet::new()
        }]
    );
    assert_eq!(
        app.update(ch('e')),
        [Effect::Edit {
            path: "src/a.rs".to_owned(),
            line: 1
        }]
    );
    app.update(key(KeyCode::Enter));
    assert_eq!(app.tab, Tab::Feed);
    assert_eq!(app.feed.visible(1).len(), 2);
    app.update(key(KeyCode::Esc));
    assert_eq!(app.feed.visible(1).len(), 3);

    app.update(ch('2'));
    app.update(ch('m'));
    assert_eq!(app.hotspots.grouping, Grouping::Modules);
    assert_eq!(app.hotspots.spots[0].key, "src");
    assert!(app.update(ch('o')).is_empty());
    assert!(app.notice.is_some());
    app.update(ch('j'));
    app.update(key(KeyCode::Enter));
    assert_eq!(
        app.feed.filter.as_ref().map(|f| f.key.as_str()),
        Some("docs")
    );
    assert_eq!(app.feed.visible(1).len(), 1);
}

#[test]
fn file_search_flow() {
    let mut app = loaded();
    assert_eq!(app.update(ch('/')), [Effect::LoadFiles]);
    assert_eq!(app.tab, Tab::Search);
    assert!(app.search.files_loading());
    assert!(app.update(ch('i')).is_empty(), "list already loading");
    app.update(Msg::Files(vec![
        "src/main.rs".to_owned(),
        "README.md".to_owned(),
    ]));
    assert_eq!(app.search.file_hits.len(), 1);
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        [Effect::OpenFile {
            path: "src/main.rs".to_owned(),
            line: 1,
            marks: BTreeSet::new()
        }]
    );
    assert_eq!(
        app.update(ctrl('e')),
        [Effect::Edit {
            path: "src/main.rs".to_owned(),
            line: 1
        }]
    );
    app.update(key(KeyCode::Backspace));
    assert_eq!(app.search.file_hits.len(), 2);
    app.update(key(KeyCode::Down));
    assert_eq!(app.search.pos.selected, 1);
    app.update(ctrl('p'));
    assert_eq!(app.search.pos.selected, 0);
    // 'q' and digits are typed, not commands, while searching.
    assert!(app.update(ch('q')).is_empty());
    assert_eq!(app.search.query, "q");
    app.update(ch('/'));
    assert_eq!(app.search.query, "q/");

    // A working-tree change makes the list stale and reloads it.
    let effects = app.update(Msg::Changed(BTreeSet::from([Change::Workdir])));
    assert!(effects.contains(&Effect::LoadFiles));
}

#[test]
fn query_editing_keys() {
    let mut app = loaded();
    app.update(ch('/'));
    app.update(Msg::Files(Vec::new()));
    for c in "src/app foo".chars() {
        app.update(ch(c));
    }
    app.update(ctrl('w'));
    assert_eq!(app.search.query, "src/app ");
    app.update(ctrl('w'));
    assert_eq!(app.search.query, "src/");
    app.update(ctrl('u'));
    assert_eq!(app.search.query, "");
}

#[test]
fn text_search_discards_stale_results() {
    let mut app = loaded();
    app.update(ch('/'));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.search.mode, SearchMode::Text);
    let first = app.update(ch('a'));
    assert_eq!(
        first,
        [Effect::SearchText {
            query: "a".to_owned(),
            generation: 2
        }]
    );
    app.update(ch('b'));
    assert!(app.search.text_running);
    let stale = TextResults {
        matches: vec![crate::search::TextMatch {
            path: "x".to_owned(),
            line: 1,
            text: "a".to_owned(),
            ranges: vec![(0, 1)],
        }],
        ..TextResults::default()
    };
    app.update(Msg::Text {
        generation: 2,
        result: Ok(stale.clone()),
    });
    assert!(
        app.search.text.matches.is_empty(),
        "stale generation ignored"
    );
    app.update(Msg::Text {
        generation: 3,
        result: Ok(stale),
    });
    assert_eq!(app.search.len(), 1);
    assert!(!app.search.is_empty());
    assert_eq!(app.search.selected(), Some(("x".to_owned(), 1)));
    app.update(Msg::Text {
        generation: 3,
        result: Err("bad".to_owned()),
    });
    assert_eq!(app.search.error.as_deref(), Some("bad"));
    app.update(key(KeyCode::Backspace));
    app.update(key(KeyCode::Backspace));
    assert!(!app.search.text_running);
    assert!(app.search.text.matches.is_empty());
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.search.mode, SearchMode::Files);
}

fn open_detail(app: &mut App) {
    app.update(Msg::Diff {
        entry: Box::new(entry("c3", 9_000, &["src/a.rs", "docs/x.md"])),
        result: Ok(vec![file_diff("src/a.rs"), file_diff("docs/x.md")]),
        refresh: false,
    });
}

#[test]
fn detail_view_navigation() {
    let mut app = loaded();
    open_detail(&mut app);
    let detail = |app: &App| match app.views.last() {
        Some(View::Detail(d)) => (d.pos.selected, d.focus, d.scroll, d.hscroll),
        _ => panic!("not in detail"),
    };
    assert_eq!(detail(&app), (0, Focus::Files, 0, 0));
    app.update(ch('j'));
    assert_eq!(detail(&app).0, 1);
    app.update(ch('k'));
    app.update(key(KeyCode::Tab));
    assert_eq!(detail(&app).1, Focus::Diff);
    app.update(ch('j'));
    assert_eq!(detail(&app).2, 1);
    app.update(ch('k'));
    app.update(ch('n'));
    assert_eq!(detail(&app).2, 5, "jumps to second hunk");
    app.update(ch('N'));
    assert_eq!(detail(&app).2, 0);
    app.update(ch('G'));
    assert_eq!(
        detail(&app).2,
        6,
        "last row; viewport height unknown before first draw"
    );
    app.update(ch('g'));
    app.update(ch('J'));
    app.update(ch('K'));
    app.update(ctrl('d'));
    app.update(ctrl('u'));
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::Right));
    assert_eq!(detail(&app).3, H_SCROLL_STEP);
    app.update(key(KeyCode::Left));
    app.update(ch('h'));
    assert_eq!(detail(&app).1, Focus::Files);
    app.update(ch('l'));
    // From the last hunk, `n` moves on to the next file.
    app.update(ch('G'));
    app.update(ch('n'));
    assert_eq!(detail(&app).0, 1);
    app.update(ch('N'));
    assert_eq!(detail(&app).0, 0, "N at the top goes to the previous file");
    app.update(ch('j'));
    app.update(ch('N'));
    assert_eq!(detail(&app).2, 0);

    let marks = BTreeSet::from([2, 20]);
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        [Effect::OpenFile {
            path: "src/a.rs".to_owned(),
            line: 1,
            marks
        }]
    );
    app.update(ch('h'));
    assert_eq!(
        app.update(ch('e')),
        [Effect::Edit {
            path: "src/a.rs".to_owned(),
            line: 2
        }]
    );
    app.update(ch('?'));
    assert!(app.help);
    app.update(ch('x'));
    app.update(key(KeyCode::Esc));
    assert!(app.views.is_empty());
}

#[test]
fn diff_errors_become_notices() {
    let mut app = loaded();
    app.update(Msg::Diff {
        entry: Box::new(entry("c1", 0, &[])),
        result: Err("nope".to_owned()),
        refresh: false,
    });
    assert!(app.views.is_empty());
    assert!(app.notice.as_deref().is_some_and(|n| n.contains("nope")));
    app.update(ch('j'));
    assert!(app.notice.is_none(), "cleared on next key");
    app.update(Msg::Notice("hi".to_owned()));
    assert_eq!(app.notice.as_deref(), Some("hi"));
}

#[test]
fn file_view_navigation() {
    let mut app = loaded();
    let content = (1..=100).fold(String::new(), |mut acc, i| {
        use std::fmt::Write;
        let _ = writeln!(acc, "line {i}");
        acc
    });
    app.update(Msg::File {
        path: "src/a.rs".to_owned(),
        content: Ok(content),
        line: 30,
        marks: BTreeSet::from([10, 11, 50, 80]),
    });
    let file = |app: &App| match app.views.last() {
        Some(View::File(f)) => (f.scroll, f.hscroll, f.lines.len()),
        _ => panic!("not in file view"),
    };
    assert_eq!(file(&app), (26, 0, 100));
    if let Some(View::File(f)) = app.views.last_mut() {
        f.height = 20;
        assert_eq!(f.language, "Rust");
    }
    app.update(ch('j'));
    assert_eq!(file(&app).0, 27);
    app.update(ch('k'));
    app.update(ch('n'));
    assert_eq!(file(&app).0, 45, "next change block starts at 50");
    app.update(ch('n'));
    assert_eq!(file(&app).0, 75);
    app.update(ch('N'));
    assert_eq!(file(&app).0, 45);
    app.update(ch('N'));
    assert_eq!(file(&app).0, 5);
    app.update(ch('G'));
    assert_eq!(
        file(&app).0,
        80,
        "last line at the bottom of a 20-row viewport"
    );
    app.update(ch('g'));
    app.update(ctrl('d'));
    assert_eq!(file(&app).0, 10);
    app.update(ctrl('u'));
    app.update(key(KeyCode::PageDown));
    assert_eq!(file(&app).0, 20);
    app.update(key(KeyCode::PageUp));
    app.update(ch('l'));
    assert_eq!(file(&app).1, H_SCROLL_STEP);
    app.update(ch('h'));
    assert_eq!(
        app.update(ch('e')),
        [Effect::Edit {
            path: "src/a.rs".to_owned(),
            line: 1
        }]
    );
    app.update(ch('G'));
    app.update(ch('g'));
    for _ in 0..25 {
        app.update(ch('j'));
    }
    assert_eq!(
        app.update(ch('e')),
        [Effect::Edit {
            path: "src/a.rs".to_owned(),
            line: 30
        }],
        "target line is visible"
    );
    app.update(ch('q'));
    assert!(app.views.is_empty());
}

#[test]
fn unreadable_files_show_a_notice() {
    let mut app = loaded();
    app.update(Msg::File {
        path: "x.bin".to_owned(),
        content: Err("binary".to_owned()),
        line: 0,
        marks: BTreeSet::new(),
    });
    let Some(View::File(f)) = app.views.last() else {
        panic!("file view")
    };
    assert_eq!(f.notice.as_deref(), Some("binary"));
    assert_eq!(f.target, 1);
}

#[test]
fn tick_advances_time() {
    let mut app = loaded();
    app.update(Msg::Tick(42));
    assert_eq!((app.tick, app.now), (1, 42));
}

#[test]
fn tab_cycle_and_ctrl_navigation() {
    let mut app = loaded();
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Hotspots);
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.tab, Tab::Feed);
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Search);
    app.update(key(KeyCode::Esc));
    app.update(ch('3'));
    app.update(key(KeyCode::BackTab));
    assert_eq!(
        app.tab,
        Tab::Search,
        "BackTab toggles search mode inside search"
    );
    app.update(key(KeyCode::Esc));
    app.update(ch('1'));
    app.update(ctrl('n'));
    assert_eq!(app.feed.pos.selected, 1);
    app.update(ctrl('k'));
    assert_eq!(app.feed.pos.selected, 0);
    assert!(app.update(ch('z')).is_empty(), "unbound keys are ignored");
    app.update(ch('2'));
    assert!(app.update(ch('z')).is_empty());
    app.update(ch('3'));
    assert!(app.update(ctrl('x')).is_empty());
}

#[test]
fn empty_lists_ignore_actions() {
    let (mut app, _) = App::new(info(), 1, 1000, 0);
    app.update(Msg::Commits(Ok(Vec::new())));
    app.update(Msg::Worktree(Ok(None)));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    app.update(ch('n'));
    app.update(ch('2'));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.update(ch('o')).is_empty());
    assert!(app.update(ch('e')).is_empty());
    app.update(ch('3'));
    app.update(Msg::Files(Vec::new()));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.update(ctrl('e')).is_empty());
}

#[test]
fn detail_view_edge_keys() {
    let mut app = loaded();
    open_detail(&mut app);
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    let focus = |app: &App| match app.views.last() {
        Some(View::Detail(d)) => d.focus,
        _ => panic!("detail"),
    };
    assert_eq!(focus(&app), Focus::Files);
    assert!(app.update(ch('z')).is_empty());
    app.update(ch('l'));
    assert!(app.update(ch('z')).is_empty());
    // Scrolling within the diff, then `N` returns to the hunk above.
    app.update(ch('J'));
    app.update(ch('J'));
    app.update(ch('N'));
    let Some(View::Detail(d)) = app.views.last() else {
        panic!("detail")
    };
    assert_eq!(d.scroll, 0);
}

#[test]
fn file_view_ignores_unbound_keys() {
    let mut app = loaded();
    app.update(Msg::File {
        path: "a.txt".to_owned(),
        content: Ok("x\n".to_owned()),
        line: 1,
        marks: BTreeSet::new(),
    });
    assert!(app.update(ch('z')).is_empty());
    app.views.push(View::File(Box::new(FileView {
        path: String::new(),
        language: String::new(),
        lines: Vec::new(),
        target: 1,
        marks: BTreeSet::new(),
        scroll: 0,
        hscroll: 0,
        height: 0,
        notice: None,
    })));
    assert_eq!(app.views.len(), 2);
}
