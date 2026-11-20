use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, ConcernFix, Effect, ExplainView, Finished, Focus, Msg, Pane, Status, View};
use crate::explain::agent::Agent;
use crate::explain::{Explanation, Note, Ref};
use crate::fix::{Done, Fixer, Request};
use crate::git::branches::{Branch, Branches, Worktree};
use crate::git::commit::Outcome;
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
        commit: true,
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

    // From the diff with nothing selected: the whole file too.
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    d.focus = Focus::Diff;
    d.cursor = d.rendered.hunk_starts[1];
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs"));
    press(&mut app, KeyCode::Esc);

    // The second hunk's header selected: that hunk.
    press(&mut app, KeyCode::Char('v'));
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
    assert_eq!(draft(&app).request.feedback, "use a constant here  ");
    ctrl(&mut app, 'w');
    assert_eq!(draft(&app).request.feedback, "use a constant ");
    ctrl(&mut app, 'u');
    assert_eq!(draft(&app).request.feedback, "");
    // A space wider than a byte, pasted from a page, ends a word too.
    typed(&mut app, "keep\u{a0}drop");
    ctrl(&mut app, 'w');
    assert_eq!(draft(&app).request.feedback, "keep\u{a0}");
    ctrl(&mut app, 'u');
    typed(&mut app, " name the 41 ");
    ctrl(&mut app, 'x');
    assert_eq!(
        draft(&app).request.feedback,
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
            result: Ok(done(Outcome::Unchanged))
        }),
        []
    );
    let effects = app.update(Msg::Fixed {
        id,
        result: Ok(done(Outcome::Committed {
            id: git2::Oid::from_str("a1b2c3d4e5").expect("oid"),
            branch: "feat-x".to_owned(),
            stale_index: false,
        })),
    });
    assert!(effects.contains(&Effect::LoadWorktree), "{effects:?}");
    assert!(
        effects.contains(&Effect::LoadCommits),
        "the fix is a commit: {effects:?}"
    );
    assert!(effects.contains(&Effect::LoadBranches), "{effects:?}");
    assert_eq!(
        app.notice.as_deref(),
        Some("Committed a1b2c3d to feat-x: named it LIMIT")
    );
    assert_eq!(app.fix.running, []);

    // Edits left uncommitted, and none at all, say so; neither is a commit.
    let sent = |app: &mut App, outcome: Outcome| {
        press(app, KeyCode::Char('f'));
        typed(app, "again");
        let (next, _, _) = fix_of(&press(app, KeyCode::Enter));
        assert_ne!(next, id);
        let effects = app.update(Msg::Fixed {
            id: next,
            result: Ok(done(outcome)),
        });
        assert!(!effects.contains(&Effect::LoadCommits), "{effects:?}");
        next
    };
    sent(
        &mut app,
        Outcome::Kept("HEAD is \u{1b}[31mdetached".to_owned()),
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("Fixed in this checkout, left uncommitted (HEAD is ·[31mdetached): named it LIMIT")
    );
    sent_commit(&mut app);
    assert_eq!(
        app.notice.as_deref(),
        Some(
            "Committed b2c3d4e to feat-x (the index couldn't be updated, so git status may be off for the fixed files): named it LIMIT"
        )
    );
    sent(&mut app, Outcome::Unchanged);
    assert_eq!(
        app.notice.as_deref(),
        Some("Nothing changed in this checkout: named it LIMIT")
    );

    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "again");
    let (second, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
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

/// Sends one more fix and reports it committed, with an index that
/// couldn't be updated.
fn sent_commit(app: &mut App) {
    press(app, KeyCode::Char('f'));
    typed(app, "again");
    let (next, _, _) = fix_of(&press(app, KeyCode::Enter));
    app.update(Msg::Fixed {
        id: next,
        result: Ok(done(Outcome::Committed {
            id: git2::Oid::from_str("b2c3d4e5f6").expect("oid"),
            branch: "feat-x".to_owned(),
            stale_index: true,
        })),
    });
}

/// A fix that said "named it LIMIT", with `outcome`.
fn done(outcome: Outcome) -> Done {
    Done {
        summary: "named it LIMIT".to_owned(),
        outcome,
    }
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
            Err("#99 isn't an open pull request vert lists"),
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
fn a_line_of_work_opens_and_edits_its_files_in_its_checkout() {
    // Where `o`, `e` and `⏎` open each change's files, and whether the
    // notice says its branch isn't checked out.
    let cases = [
        (
            crate::app::pull_entry(&pull(7, "agent")),
            Some("/wt/agent-dir"),
            false,
        ),
        (range("agent"), Some("/wt/agent-dir"), false),
        (range("here"), None, false),
        (crate::app::pull_entry(&pull(8, "loose")), None, true),
        (range("loose"), None, true),
        (commit(), None, false),
        (Entry::blank(Target::PullRequest(99)), None, true),
    ];
    for (entry, expected, missing) in cases {
        let id = entry.id.clone();
        let mut app = showing(entry);
        with_lines_of_work(&mut app);
        for key in [KeyCode::Char('o'), KeyCode::Char('e'), KeyCode::Enter] {
            let effects = press(&mut app, key);
            let root = match effects.as_slice() {
                [Effect::OpenFile { root, path, .. } | Effect::Edit { root, path, .. }] => {
                    assert_eq!(path, "src/a.rs", "{id}");
                    root.clone()
                }
                other => panic!("{id}: expected a file to open, got {other:?}"),
            };
            assert_eq!(root, expected.map(PathBuf::from), "{id} {key:?}");
            let warned = app
                .notice
                .as_deref()
                .is_some_and(|n| n.ends_with(", so this is the file in this checkout"));
            assert_eq!(warned, missing, "{id} {key:?}: {:?}", app.notice);
        }
        // Moving on opens nothing, and leaves the notice to clear.
        assert_eq!(press(&mut app, KeyCode::Char('j')), []);
        assert_eq!(app.notice, None, "{id}");
    }
}

#[test]
fn a_file_opened_from_a_worktree_follows_its_diff() {
    let pr7 = crate::app::pull_entry(&pull(7, "agent"));
    let mut app = showing(pr7.clone());
    with_lines_of_work(&mut app);
    let root = match press(&mut app, KeyCode::Char('o')).as_slice() {
        [Effect::OpenFile { root, .. }] => root.clone(),
        other => panic!("expected a file to open, got {other:?}"),
    };
    assert_eq!(root, Some(PathBuf::from("/wt/agent-dir")));
    app.update(Msg::File {
        path: "src/a.rs".to_owned(),
        content: Ok("keep\nfirst\n".to_owned()),
        line: 1,
        marks: std::collections::BTreeSet::new(),
        root,
    });
    // The diff refreshes: the file's marks follow it from the worktree.
    app.update(Msg::Diff {
        entry: Box::new(pr7),
        result: Ok(vec![file("src/a.rs")]),
        refresh: true,
        file: None,
    });
    let Some(View::File(f)) = app.views.last() else {
        panic!("file view");
    };
    assert_eq!(f.marks, std::collections::BTreeSet::from([2, 41]));
}

#[test]
fn the_origin_names_the_branch_and_where_it_is_checked_out() {
    use crate::app::Checkout;
    let mut app = blank_app();
    let origin = |app: &App, entry: &Entry| app.origin(entry);
    let pr7 = crate::app::pull_entry(&pull(7, "agent"));
    // Before the lines of work load, vert can't tell where #7 is.
    let early = origin(&app, &pr7).expect("#7");
    assert_eq!(early.branch, None);
    assert!(matches!(&early.checkout, Checkout::Unknown(why) if why.contains("still loading")));
    assert_eq!(
        early.fallback("#7").as_deref(),
        Some("vert can't tell where #7 is checked out, so this is the file in this checkout")
    );
    with_lines_of_work(&mut app);
    let agent = origin(&app, &pr7).expect("#7");
    assert_eq!(agent.branch.as_deref(), Some("agent"));
    assert_eq!(
        agent.checkout,
        Checkout::At(Some(PathBuf::from("/wt/agent-dir")), "agent-dir".to_owned())
    );
    assert_eq!(
        (agent.root(), agent.fallback("#7")),
        (Some("/wt/agent-dir".into()), None)
    );
    let here = origin(&app, &range("here")).expect("here");
    assert_eq!(here.checkout, Checkout::At(None, "here".to_owned()));
    let loose = origin(&app, &range("loose")).expect("loose");
    assert!(matches!(loose.checkout, Checkout::Missing(_)));
    assert_eq!(
        loose.fallback("loose").as_deref(),
        Some("loose isn't checked out, so this is the file in this checkout")
    );
    // A closed or unlisted pull request: no branch, and it says so.
    let closed = origin(&app, &Entry::blank(Target::PullRequest(99))).expect("#99");
    assert_eq!(closed.branch, None);
    assert!(matches!(closed.checkout, Checkout::Unknown(_)));
    assert_eq!(origin(&app, &commit()), None);
}

#[test]
fn a_selection_names_its_lines_and_sends_only_them() {
    let mut app = showing(commit());
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    d.focus = Focus::Diff;
    // From the added line of the first hunk down to the blank row before
    // the second: the blank holds nothing of the diff.
    let first_added = d.rendered.hunk_starts[0] + 2;
    d.cursor = first_added;
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.excerpt, "@@ -1 +1,2 @@\n+first\n");
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:2"));
    press(&mut app, KeyCode::Esc);
    assert!(
        matches!(app.views.last(), Some(View::Detail(d)) if d.selection.is_some()),
        "closing the prompt keeps the selection"
    );
    // Only the blank row: nothing of the diff, so the whole file.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Esc);
    let Some(View::Detail(d)) = app.views.last_mut() else {
        panic!("diff");
    };
    assert_eq!(d.selection, None, "esc clears it");
    d.cursor = d.rendered.hunk_starts[1] - 1;
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs"));
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
    // The Branches tab's filters don't hide the pull request's branch.
    "nothing-matches".clone_into(&mut app.branches.name_filter);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).checkout, "agent-dir");
    press(&mut app, KeyCode::Esc);

    // A branch scan that failed is not one still loading.
    let mut failed = showing(crate::app::pull_entry(&pull(7, "agent")));
    failed.update(Msg::Branches(Err("bad base".to_owned())));
    press(&mut failed, KeyCode::Char('f'));
    assert_eq!(
        failed.notice.as_deref(),
        Some("Can't fix: couldn't list the lines of work: bad base")
    );

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
        questions: Vec::new(),
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
        question: None,
        marked: std::collections::BTreeSet::new(),
        reload: None,
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
    assert_eq!(draft(&app).request.feedback, "Off by one");
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:41"));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.views.last(), Some(View::Explain(_))));

    select(&mut app, 2, 1);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.feedback, "Off by one");
    assert_eq!(draft(&app).request.place.as_deref(), Some("docs/x.md"));
    press(&mut app, KeyCode::Esc);

    select(&mut app, 2, 2);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.feedback, "Missing test");
    assert_eq!(draft(&app).request.place.as_deref(), Some("src/a.rs:2"));
    press(&mut app, KeyCode::Esc);

    // A step: its code, and no words to start from.
    select(&mut app, 1, 0);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.feedback, "");
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
    assert_eq!(draft(&app).request.feedback, "Is this wanted?");
    assert_eq!(draft(&app).request.place, None);
    press(&mut app, KeyCode::Esc);

    // Several without code: each is a stop of its own.
    let other = Note {
        text: "And this?".to_owned(),
        ..lone.clone()
    };
    let mut app = explaining(vec![lone, other]);
    select(&mut app, 2, 1);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.feedback, "And this?");
    assert_eq!(
        draft(&app).concerns,
        Some((1, vec!["And this?".to_owned()]))
    );
}

fn view(app: &App) -> &ExplainView {
    match app.views.last() {
        Some(View::Explain(v)) => v,
        _ => panic!("explaining"),
    }
}

/// Two concerns with code (the first at two places, the second sharing one
/// of them) and one without.
fn three_concerns() -> App {
    explaining(vec![
        Note {
            text: "Off by one".to_owned(),
            refs: vec![r("src/a.rs", Some(1)), r("docs/x.md", None)],
        },
        Note {
            text: "Unbounded".to_owned(),
            refs: vec![r("src/a.rs", Some(1))],
        },
        Note {
            text: "Is this wanted?".to_owned(),
            refs: Vec::new(),
        },
    ])
}

#[test]
fn concerns_are_marked_with_space() {
    let mut app = three_concerns();
    // Stops: Off by one ×2, Unbounded, Is this wanted?
    select(&mut app, 2, 1);
    press(&mut app, KeyCode::Char(' '));
    select(&mut app, 2, 3);
    press(&mut app, KeyCode::Char(' '));
    let (off, wanted) = (
        "Off by one\n@src/a.rs#1\n@docs/x.md".to_owned(),
        "Is this wanted?".to_owned(),
    );
    assert_eq!(view(&app).marked, [off, wanted.clone()].into());
    select(&mut app, 2, 0);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(
        view(&app).marked,
        [wanted.clone()].into(),
        "a second press unmarks"
    );

    // Only concerns can be marked.
    select(&mut app, 1, 0);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(view(&app).marked, [wanted].into());
    assert_eq!(
        app.notice.as_deref(),
        Some("Marking is for concerns: select one first")
    );
}

#[test]
fn a_mark_whose_concern_is_gone_sends_nothing() {
    let mut app = three_concerns();
    // Marked on an earlier answer whose concerns this one lacks.
    if let Some(View::Explain(v)) = app.views.last_mut() {
        v.marked.insert("Gone\n@src/a.rs#0".to_owned());
    }
    select(&mut app, 2, 2);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(
        draft(&app).request.feedback,
        "Unbounded",
        "the selected concern, not an empty list"
    );
}

#[test]
fn marked_concerns_are_fixed_in_one_request_and_tracked() {
    let mut app = three_concerns();
    select(&mut app, 2, 3);
    press(&mut app, KeyCode::Char(' '));
    select(&mut app, 2, 0);
    press(&mut app, KeyCode::Char(' '));
    select(&mut app, 2, 2);
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('f'));
    let request = &draft(&app).request;
    assert_eq!(
        request.feedback,
        "Deal with each of these concerns a reviewer raised about the change:\n\
         \n1. Off by one (at src/a.rs:41, docs/x.md)\
         \n2. Unbounded (at src/a.rs:41)\
         \n3. Is this wanted?"
    );
    assert_eq!(request.place, None, "the change as a whole");
    assert_eq!(
        request.excerpt,
        "diff --git a/src/a.rs b/src/a.rs\n@@ -40 +41 @@\n-old\n+new\n\
         diff --git a/docs/x.md b/docs/x.md\n@@ -1 +1,2 @@\n keep\n+first\n@@ -40 +41 @@\n-old\n+new\n",
        "each hunk once"
    );
    // Kept by text and code, so two concerns worded alike stay apart.
    let texts = [
        "Off by one\n@src/a.rs#1\n@docs/x.md",
        "Unbounded\n@src/a.rs#1",
        "Is this wanted?",
    ]
    .map(str::to_owned);
    assert_eq!(draft(&app).concerns, Some((1, texts.to_vec())));

    app.now = 200;
    let (id, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
    assert_eq!(view(&app).marked, [].into(), "sent, so unmarked");
    for text in &texts {
        assert_eq!(
            app.explain.concern_fix(1, text),
            Some(&ConcernFix::Fixing { since: 200 })
        );
    }
    assert_eq!(app.fix.running[0].since, 200);
    // A concern being fixed can't be marked again.
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(view(&app).marked, [].into());
    assert_eq!(
        app.notice.as_deref(),
        Some("The agent is already fixing that concern")
    );

    let committed = Outcome::Committed {
        id: git2::Oid::from_str("a1b2c3d4e5").expect("oid"),
        branch: "feat-x".to_owned(),
        stale_index: false,
    };
    app.update(Msg::Fixed {
        id,
        result: Ok(done(committed.clone())),
    });
    assert_eq!(
        app.explain.concern_fix(1, "Is this wanted?"),
        Some(&ConcernFix::Done(done(committed)))
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("Committed a1b2c3d to feat-x (c changes, r explain again): named it LIMIT"),
        "the keys come before the agent's words"
    );
}

#[test]
fn the_selected_concern_alone_is_tracked_and_failures_are_kept() {
    let mut app = three_concerns();
    select(&mut app, 2, 2);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(draft(&app).request.feedback, "Unbounded");
    assert_eq!(
        draft(&app).concerns,
        Some((1, vec!["Unbounded\n@src/a.rs#1".to_owned()]))
    );
    let (id, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
    app.update(Msg::Fixed {
        id,
        result: Err("pi: \u{1b}[31mboom".to_owned()),
    });
    assert_eq!(
        app.explain.concern_fix(1, "Unbounded\n@src/a.rs#1"),
        Some(&ConcernFix::Failed("pi: ·[31mboom".to_owned()))
    );
    assert_eq!(
        app.explain
            .concern_fix(1, "Off by one\n@src/a.rs#1\n@docs/x.md"),
        None,
        "only the one sent"
    );
    assert_eq!(
        app.fix.last.as_ref().map(|l| &l.outcome),
        Some(&Err("pi: ·[31mboom".to_owned())),
        "a failure of its own, not kept edits"
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("Couldn't fix in this checkout (r explain again): pi: ·[31mboom"),
        "nothing for `c` to show"
    );
    assert_eq!(press(&mut app, KeyCode::Char('c')), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("The last fix in this checkout failed: there is nothing of it to show")
    );

    // Nothing changed: there is no fix for `c` to show.
    press(&mut app, KeyCode::Char('f'));
    let (id, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
    app.update(Msg::Fixed {
        id,
        result: Ok(done(Outcome::Unchanged)),
    });
    assert_eq!(
        app.notice.as_deref(),
        Some("Nothing changed in this checkout (r explain again): named it LIMIT")
    );

    // Away from an explanation, the notice has no keys to offer.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('f'));
    typed(&mut app, "x");
    let (id, _, _) = fix_of(&press(&mut app, KeyCode::Enter));
    app.update(Msg::Fixed {
        id,
        result: Ok(done(Outcome::Unchanged)),
    });
    assert_eq!(
        app.notice.as_deref(),
        Some("Nothing changed in this checkout: named it LIMIT")
    );
}

#[test]
fn c_opens_what_the_last_fix_changed() {
    let mut app = three_concerns();
    assert_eq!(press(&mut app, KeyCode::Char('c')), []);
    assert_eq!(app.notice.as_deref(), Some("No fix has finished yet"));

    let finish = |app: &mut App, root: Option<&str>, outcome: Outcome| {
        app.fix.last = Some(Finished {
            root: root.map(PathBuf::from),
            checkout: root.unwrap_or("this checkout").to_owned(),
            outcome: Ok(outcome),
        });
        press(app, KeyCode::Char('c'))
    };
    let opened = |effects: &[Effect]| match effects {
        [
            Effect::LoadDiff {
                entry,
                refresh: false,
                file: None,
            },
        ] => (**entry).clone(),
        other => panic!("expected a diff to open, got {other:?}"),
    };

    // A commit vert lists opens as listed; another as a bare commit, in its
    // checkout.
    let id = git2::Oid::from_str("a1b2c3d4e5").expect("oid");
    let mut listed = Entry::blank(Target::Commit(id));
    "fix: named it".clone_into(&mut listed.summary);
    app.update(Msg::Commits(Ok(vec![listed.clone()])));
    let committed = || Outcome::Committed {
        id,
        branch: "main".to_owned(),
        stale_index: false,
    };
    assert_eq!(opened(&finish(&mut app, None, committed())), listed);
    app.update(Msg::Commits(Ok(Vec::new())));
    let bare = opened(&finish(&mut app, Some("/wt/x"), committed()));
    assert_eq!(bare.target, Target::Commit(id));
    assert_eq!(bare.root, Some(PathBuf::from("/wt/x")));

    // A fix that changed nothing has nothing to show.
    assert_eq!(finish(&mut app, None, Outcome::Unchanged), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("The last fix in this checkout changed nothing")
    );
    // Uncommitted edits open as the checkout's uncommitted changes, read
    // from disk even before a scan has seen them.
    assert_eq!(
        opened(&finish(&mut app, None, Outcome::Kept("why".to_owned()))),
        Entry::uncommitted()
    );
    let mut dirty = Entry::blank(Target::WorkingTree);
    dirty.summary = "Uncommitted changes".to_owned();
    app.update(Msg::Worktree(Ok(Some(dirty.clone()))));
    let kept = || Outcome::Kept("why".to_owned());
    assert_eq!(opened(&finish(&mut app, None, kept())), dirty);

    let mut linked = branch("feat", Some(("x", false)));
    let mut wt_dirty = Entry::blank(Target::WorkingTree);
    wt_dirty.root = Some(PathBuf::from("/wt/x"));
    if let Some(w) = linked.worktree.as_mut() {
        w.dirty = Some(wt_dirty.clone());
    }
    app.update(Msg::Branches(Ok(Branches {
        base: None,
        branches: vec![linked],
    })));
    assert_eq!(opened(&finish(&mut app, Some("/wt/x"), kept())), wt_dirty);
    // Before a scan sees its edits, a linked worktree's are read from disk.
    app.update(Msg::Branches(Ok(Branches {
        base: None,
        branches: vec![branch("feat", Some(("x", false)))],
    })));
    let fresh = opened(&finish(&mut app, Some("/wt/x"), kept()));
    assert_eq!(fresh.target, Target::WorkingTree);
    assert_eq!(fresh.id, "worktree:/wt/x");
    assert_eq!(fresh.root, Some(PathBuf::from("/wt/x")));
    assert_eq!(finish(&mut app, Some("/wt/gone"), kept()), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("/wt/gone has no uncommitted changes")
    );
}

#[test]
fn the_file_viewer_opened_from_a_diff_fixes_its_lines_or_the_file() {
    let mut app = showing(commit());
    app.update(Msg::File {
        path: "src/a.rs".to_owned(),
        content: Ok("one\ntwo\nthree\n".to_owned()),
        line: 2,
        marks: std::collections::BTreeSet::new(),
        root: None,
    });
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('f'));
    let request = &draft(&app).request;
    assert_eq!(request.change, "0000000 Add login");
    assert_eq!(request.place.as_deref(), Some("src/a.rs:2-3"));
    assert_eq!(
        request.excerpt,
        "@@ src/a.rs:2-3, as on disk @@\n two\n three\n"
    );
    press(&mut app, KeyCode::Esc);
    // esc clears the selection; then f is about the whole file.
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.views.last(), Some(View::File(f)) if f.selection.is_none()));
    press(&mut app, KeyCode::Char('f'));
    let request = &draft(&app).request;
    assert_eq!(request.place.as_deref(), Some("src/a.rs"));
    assert_eq!(
        request.excerpt,
        crate::fix::excerpt(&file("src/a.rs"), None),
        "the file's hunks"
    );
}
