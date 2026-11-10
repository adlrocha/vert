use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Effect, ExplainView, Focus, Msg, Pane, Status, View};
use crate::explain::agent::Agent;
use crate::explain::{Explanation, Note, Ref};
use crate::fix::{Fixer, Request};
use crate::git::branches::{Branch, Branches, Worktree};
use crate::git::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::git::feed::Entry;
use crate::git::{FileStat, FileStatus, RepoInfo, Target};
use crate::github::PullRequest;

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn ctrl(app: &mut App, c: char) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char(c),
        KeyModifiers::CONTROL,
    )))
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// A file with two hunks: a context line then an added line at 1–2, and a
/// removed line then an added line at 40.
fn file(path: &str) -> FileDiff {
    let line = |kind, old_no, new_no, content: &str| DiffLine {
        kind,
        old_no,
        new_no,
        content: content.to_owned(),
    };
    FileDiff {
        stat: FileStat {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            added: 2,
            removed: 1,
            binary: false,
        },
        hunks: vec![
            Hunk {
                header: "@@ -1 +1,2 @@".to_owned(),
                lines: vec![
                    line(LineKind::Context, Some(1), Some(1), "keep"),
                    line(LineKind::Added, None, Some(2), "first"),
                ],
            },
            Hunk {
                header: "@@ -40 +41 @@".to_owned(),
                lines: vec![
                    line(LineKind::Removed, Some(40), None, "old"),
                    line(LineKind::Added, None, Some(41), "new"),
                ],
            },
        ],
    }
}

fn fixer() -> Fixer {
    Fixer {
        agent: Agent::Claude,
        model: None,
        timeout: Duration::from_secs(1),
        max_excerpt_bytes: 1 << 10,
    }
}

fn blank_app() -> App {
    let (mut app, _) = App::new(
        RepoInfo {
            name: "r".to_owned(),
            branch: None,
        },
        1,
        1000,
        100,
        false,
    );
    app.set_fixer(&Ok(fixer()));
    // The loads App::new started are done, so new ones go out at once.
    app.update(Msg::Commits(Ok(Vec::new())));
    app.update(Msg::Worktree(Ok(None)));
    app
}

/// An app showing the diff of `entry`: `src/a.rs` and `docs/x.md`.
fn showing(entry: Entry) -> App {
    let mut app = blank_app();
    app.update(Msg::Diff {
        entry: Box::new(entry),
        result: Ok(vec![file("src/a.rs"), file("docs/x.md")]),
        refresh: false,
        file: None,
    });
    app
}

fn commit() -> Entry {
    let mut e = Entry::blank(Target::Commit(git2::Oid::ZERO_SHA1));
    "Add login".clone_into(&mut e.summary);
    e
}

fn range(name: &str) -> Entry {
    let mut e = Entry::blank(Target::Range {
        base: None,
        head: git2::Oid::ZERO_SHA1,
    });
    e.id = format!("branch:{name}");
    e
}

fn branch(name: &str, worktree: Option<(&str, bool)>) -> Branch {
    Branch {
        name: name.to_owned(),
        branch: Some(name.to_owned()),
        remote: false,
        ahead_behind: Some((1, 0)),
        changes: Some(range(name)),
        worktree: worktree.map(|(dir, current)| Worktree {
            name: dir.to_owned(),
            path: PathBuf::from(format!("/wt/{dir}")),
            current,
            main: false,
            dirty: None,
        }),
    }
}

fn pull(number: u64, head: &str) -> PullRequest {
    PullRequest {
        number,
        title: format!("PR {number}"),
        author: "claude".to_owned(),
        draft: false,
        head: head.to_owned(),
        fork: false,
        updated: 1,
        added: 1,
        removed: 1,
        checks: None,
    }
}

/// Loads lines of work: `here` is vert's checkout, `agent` a linked
/// worktree, `loose` a branch without one; #7 is `agent`'s pull request and
/// #8 `loose`'s.
fn with_lines_of_work(app: &mut App) {
    app.update(Msg::Branches(Ok(Branches {
        base: Some("main".to_owned()),
        branches: vec![
            branch("here", Some(("here", true))),
            branch("agent", Some(("agent-dir", false))),
            branch("loose", None),
        ],
    })));
    app.set_refresh(crate::app::Refresh {
        branches: Some(5),
        pulls: Some(60),
    });
    app.update(Msg::Pulls(Ok((
        "o/r".to_owned(),
        vec![pull(7, "agent"), pull(8, "loose")],
    ))));
}

fn draft(app: &App) -> &crate::app::Draft {
    app.fix.draft.as_ref().expect("a draft is open")
}

/// The only fix request in `effects`.
fn fix_of(effects: &[Effect]) -> (u64, Option<PathBuf>, Request) {
    match effects {
        [Effect::Fix { id, root, request }] => (*id, root.clone(), request.clone()),
        other => panic!("expected one fix, got {other:?}"),
    }
}

#[test]
fn fixing_needs_an_agent() {
    let mut app = showing(commit());
    app.fix.agent = None;
    assert_eq!(press(&mut app, KeyCode::Char('f')), []);
    assert_eq!(app.notice.as_deref(), Some("Can't fix: no agent is set up"));
    app.set_fixer(&Err("network access is off (--no-network)".to_owned()));
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(
        app.notice.as_deref(),
        Some("Can't fix: network access is off (--no-network)")
    );
    assert!(app.fix.draft.is_none());
}

#[test]
fn feedback_on_a_hunk_is_written_sent_and_reported() {
    let mut app = showing(commit());
    // From the file list: the whole file.
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs"));
    assert_eq!(
        draft(&app).request.excerpt,
        crate::fix::excerpt(&file("src/a.rs"), None)
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.fix.draft.is_none());
    assert!(
        matches!(app.views.last(), Some(View::Detail(_))),
        "esc only closed the prompt"
    );

    // From the diff, scrolled to the second hunk.
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    d.focus = Focus::Diff;
    d.scroll = d.rendered.hunk_starts[1];
    press(&mut app, KeyCode::Char('f'));
    let d = draft(&app);
    assert_eq!(d.root, None);
    assert_eq!(d.checkout, "this checkout");
    assert_eq!(d.request.change, "0000000 Add login");
    assert_eq!(d.request.place.as_deref(), Some("src/a.rs:41"));
    assert_eq!(d.request.excerpt, "@@ -40 +41 @@\n-old\n+new\n");

    // Typing: keys go to the prompt, not the diff underneath.
    assert_eq!(press(&mut app, KeyCode::Enter), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("Write what the agent should do first")
    );
    typed(&mut app, "use a constant herx");
    press(&mut app, KeyCode::Backspace);
    typed(&mut app, "e  ");
    assert_eq!(draft(&app).text, "use a constant here  ");
    ctrl(&mut app, 'w');
    assert_eq!(draft(&app).text, "use a constant ");
    ctrl(&mut app, 'u');
    assert_eq!(draft(&app).text, "");
    // A space wider than a byte, pasted from a page, ends a word too.
    typed(&mut app, "keep\u{a0}drop");
    ctrl(&mut app, 'w');
    assert_eq!(draft(&app).text, "keep\u{a0}");
    ctrl(&mut app, 'u');
    typed(&mut app, " name the 41 ");
    ctrl(&mut app, 'x');
    assert_eq!(
        draft(&app).text,
        " name the 41 ",
        "other shortcuts do nothing"
    );

    let (id, root, request) = fix_of(&press(&mut app, KeyCode::Enter));
    assert_eq!(root, None);
    assert_eq!(request.feedback, "name the 41");
    assert_eq!(request.place.as_deref(), Some("src/a.rs:41"));
    assert!(app.fix.draft.is_none());
    assert_eq!(app.fix.running.len(), 1);
    assert_eq!(
        app.notice.as_deref(),
        Some("Asked the agent to fix this in this checkout")
    );

    reports_and_one_per_checkout(app, id);
}

/// Continues [`feedback_on_a_hunk_is_written_sent_and_reported`] with fix
/// `id` running in vert's checkout.
fn reports_and_one_per_checkout(mut app: App, id: u64) {
    // One fix per checkout at a time.
    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "more");
    assert_eq!(press(&mut app, KeyCode::Enter), []);
    assert!(
        app.notice
            .as_deref()
            .is_some_and(|n| n.contains("already fixing this checkout")),
        "{:?}",
        app.notice
    );
    assert!(app.fix.draft.is_some(), "the feedback is kept");
    press(&mut app, KeyCode::Esc);

    // The answer is reported, and what it may have changed reloads.
    assert_eq!(
        app.update(Msg::Fixed {
            id: id + 100,
            result: Ok("x".to_owned())
        }),
        []
    );
    let effects = app.update(Msg::Fixed {
        id,
        result: Ok("named it LIMIT".to_owned()),
    });
    assert!(effects.contains(&Effect::LoadWorktree), "{effects:?}");
    assert!(effects.contains(&Effect::LoadBranches), "{effects:?}");
    assert_eq!(
        app.notice.as_deref(),
        Some("Fixed in this checkout: named it LIMIT")
    );
    assert_eq!(app.fix.running, []);

    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "again");
    let (second, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
    assert_ne!(second, id);
    app.update(Msg::Fixed {
        id: second,
        result: Err("claude failed: \u{1b}[31mred".to_owned()),
    });
    assert_eq!(
        app.notice.as_deref(),
        Some("Couldn't fix in this checkout: claude failed: ·[31mred"),
        "escape sequences never reach the terminal"
    );
}

#[test]
fn fixes_go_to_the_checkout_of_the_line_of_work() {
    // Where each change is fixed (root and name), or why it can't be.
    type Expected<'a> = Result<(Option<&'a str>, &'a str), &'a str>;
    // A branch sharing no history with the base: its changes are a commit.
    let mut unrelated = Entry::blank(Target::Commit(git2::Oid::ZERO_SHA1));
    unrelated.id = "branch:loose".to_owned();
    let mut unnamed = range("x");
    unnamed.id = "a..b".to_owned();
    let cases: [(Entry, Expected<'_>); 9] = [
        (unrelated, Err("loose isn't checked out")),
        (unnamed, Err("0000000 isn't a line of work")),
        (range("here"), Ok((None, "here"))),
        (range("agent"), Ok((Some("/wt/agent-dir"), "agent-dir"))),
        (range("loose"), Err("loose isn't checked out")),
        (range("gone"), Err("gone isn't a line of work")),
        (
            crate::app::pull_entry(&pull(7, "agent")),
            Ok((Some("/wt/agent-dir"), "agent-dir")),
        ),
        (
            crate::app::pull_entry(&pull(8, "loose")),
            Err("loose isn't checked out"),
        ),
        (
            Entry::blank(Target::PullRequest(99)),
            Err("#99 isn't checked out here"),
        ),
    ];
    for (entry, expected) in cases {
        let id = entry.id.clone();
        let mut app = showing(entry);
        with_lines_of_work(&mut app);
        press(&mut app, KeyCode::Char('f'));
        match expected {
            Ok((root, name)) => {
                let d = draft(&app);
                assert_eq!(d.root, root.map(PathBuf::from), "{id}");
                assert_eq!(d.checkout, name, "{id}");
            }
            Err(why) => {
                assert!(app.fix.draft.is_none(), "{id}");
                let notice = app.notice.clone().unwrap_or_default();
                assert!(
                    notice.starts_with("Can't fix: ") && notice.contains(why),
                    "{id}: {notice}"
                );
            }
        }
    }

    // A linked worktree's own uncommitted changes, named after its directory.
    let mut dirty = Entry::blank(Target::WorkingTree);
    dirty.root = Some(PathBuf::from("/wt/agent-dir"));
    let mut app = showing(dirty.clone());
    with_lines_of_work(&mut app);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).checkout, "agent-dir");
    assert_eq!(
        draft(&app).request.change,
        "working Uncommitted changes in the working tree"
    );
    // One vert no longer lists is named by its path.
    dirty.root = Some(PathBuf::from("/elsewhere"));
    let mut app = showing(dirty);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).checkout, "/elsewhere");
}

#[test]
fn the_place_and_the_excerpt_name_the_same_hunk() {
    let mut app = showing(commit());
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    d.focus = Focus::Diff;
    // The last row before the second hunk starts: no line of its own in the
    // first hunk, and the next new line is the second hunk's.
    d.scroll = d.rendered.hunk_starts[1] - 1;
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(
        draft(&app).request.excerpt,
        "@@ -1 +1,2 @@\n keep\n+first\n"
    );
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:2"));
    press(&mut app, KeyCode::Esc);
    // Above the first hunk: the first hunk.
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    d.scroll = 0;
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:1"));
}

#[test]
fn a_pull_request_finds_its_checkout_whichever_list_loads_first() {
    let mut app = showing(crate::app::pull_entry(&pull(7, "agent")));
    app.set_refresh(crate::app::Refresh {
        branches: Some(5),
        pulls: Some(60),
    });
    app.update(Msg::Pulls(Ok(("o/r".to_owned(), vec![pull(7, "agent")]))));
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(
        app.notice.as_deref(),
        Some("Can't fix: the lines of work are still loading; try again in a moment")
    );
    with_lines_of_work(&mut app);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).checkout, "agent-dir");
    press(&mut app, KeyCode::Esc);

    // A fork's pull request with the same head name is not that branch.
    let mut fork = pull(9, "agent");
    fork.fork = true;
    let mut app = showing(crate::app::pull_entry(&fork));
    with_lines_of_work(&mut app);
    app.update(Msg::Pulls(Ok(("o/r".to_owned(), vec![fork]))));
    press(&mut app, KeyCode::Char('f'));
    assert!(
        app.notice
            .as_deref()
            .is_some_and(|n| n.contains("#9 isn't checked out"))
    );

    // A branch only on origin, checked out nowhere.
    let mut app = showing(crate::app::pull_entry(&pull(5, "remote")));
    with_lines_of_work(&mut app);
    let mut remote = branch("origin/remote", None);
    remote.remote = true;
    remote.branch = Some("remote".to_owned());
    app.branches.list.branches.push(remote);
    app.branches.pulls.push(pull(5, "remote"));
    press(&mut app, KeyCode::Char('f'));
    assert!(
        app.notice
            .as_deref()
            .is_some_and(|n| n.contains("origin/remote isn't checked out")),
        "{:?}",
        app.notice
    );
}

#[test]
fn fixes_in_different_checkouts_run_side_by_side() {
    let mut app = showing(range("agent"));
    with_lines_of_work(&mut app);
    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "one");
    fix_of(&press(&mut app, KeyCode::Enter));
    app.update(Msg::Diff {
        entry: Box::new(range("here")),
        result: Ok(vec![file("src/a.rs")]),
        refresh: false,
        file: None,
    });
    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "two");
    fix_of(&press(&mut app, KeyCode::Enter));
    let names: Vec<_> = app
        .fix
        .running
        .iter()
        .map(|r| r.checkout.as_str())
        .collect();
    assert_eq!(names, ["agent-dir", "here"]);
}

#[test]
fn an_open_diff_of_vert_s_checkout_empties_once_it_is_clean() {
    let mut app = showing(Entry::blank(Target::WorkingTree));
    let effects = app.update(Msg::Worktree(Ok(None)));
    let [
        Effect::LoadDiff {
            entry,
            refresh: true,
            file: None,
        },
    ] = effects.as_slice()
    else {
        panic!("expected a refresh, got {effects:?}");
    };
    assert!(entry.is_own_worktree());
    assert_eq!(entry.files, []);
    app.update(Msg::Diff {
        entry: entry.clone(),
        result: Ok(Vec::new()),
        refresh: true,
        file: None,
    });
    let Some(View::Detail(d)) = app.views.last() else {
        panic!("diff");
    };
    assert_eq!(d.files, []);
}

fn r(path: &str, hunk: Option<usize>) -> Ref {
    Ref {
        path: path.to_owned(),
        hunk,
        label: String::new(),
    }
}

/// Opens an explanation over the diff with two concerns, the first with two
/// pointers, and one step.
fn explaining(concerns: Vec<Note>) -> App {
    let mut app = showing(commit());
    let Some(View::Detail(d)) = app.views.last() else {
        panic!("diff");
    };
    let (entry, files) = (d.entry.clone(), d.files.clone());
    let explanation = Explanation {
        intent: "Do it".to_owned(),
        steps: vec![crate::explain::Step {
            title: "Read".to_owned(),
            detail: String::new(),
            refs: vec![r("docs/x.md", Some(0))],
        }],
        diagram: None,
        concerns,
        unexplained: Vec::new(),
        dropped: 0,
    };
    app.views.push(View::Explain(Box::new(ExplainView {
        key: 1,
        entry,
        files,
        agent: "pi".to_owned(),
        status: Status::Ready(Box::new(explanation), false),
        pos: crate::app::ListPos::default(),
        pane: Pane::Outline,
        target: 0,
        scroll: 0,
        height: 0,
        rows: 0,
    })));
    app
}

fn select(app: &mut App, item: usize, target: usize) {
    let Some(View::Explain(v)) = app.views.last_mut() else {
        panic!("explaining");
    };
    v.pos.selected = item;
    v.target = target;
}

#[test]
fn a_concern_becomes_the_feedback() {
    let concerns = vec![
        Note {
            text: "Off by one".to_owned(),
            refs: vec![r("src/a.rs", Some(1)), r("docs/x.md", None)],
        },
        Note {
            text: "Missing test".to_owned(),
            refs: vec![r("src/a.rs", Some(0))],
        },
    ];
    // Outline: overview, the step, concerns.
    let mut app = explaining(concerns.clone());
    select(&mut app, 2, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "Off by one");
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:41"));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.views.last(), Some(View::Explain(_))));

    select(&mut app, 2, 1);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "Off by one");
    assert_eq!(draft(&app).request.place.as_deref(), Some("docs/x.md"));
    press(&mut app, KeyCode::Esc);

    select(&mut app, 2, 2);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "Missing test");
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:2"));
    press(&mut app, KeyCode::Esc);

    // A step: its code, and no words to start from.
    select(&mut app, 1, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "");
    assert_eq!(draft(&app).request.place.as_deref(), Some("docs/x.md:2"));
    press(&mut app, KeyCode::Esc);

    // The overview has no code here: the change as a whole.
    select(&mut app, 0, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.place, None);
    assert_eq!(draft(&app).request.excerpt, "");
}

#[test]
fn a_concern_without_code_still_becomes_the_feedback() {
    let lone = Note {
        text: "Is this wanted?".to_owned(),
        refs: Vec::new(),
    };
    let mut app = explaining(vec![lone.clone()]);
    select(&mut app, 2, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "Is this wanted?");
    assert_eq!(draft(&app).request.place, None);
    press(&mut app, KeyCode::Esc);

    // Several without code: nothing to pick from.
    let mut app = explaining(vec![lone.clone(), lone]);
    select(&mut app, 2, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).text, "");
}
