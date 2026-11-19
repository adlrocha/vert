use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Effect, Focus, Msg, Selection, View, pi_for_tests};
use crate::explain::question::{About, Question};
use crate::explain::{Answer, Explanation, Ref, diff_key};
use crate::git::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::git::feed::Entry;
use crate::git::{FileStat, FileStatus, RepoInfo, Target};

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// A file with two hunks: context and an added line at 1–2, and a removed
/// and an added line at 40.
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

fn files() -> Vec<FileDiff> {
    vec![file("src/a.rs"), file("docs/x.md")]
}

fn commit() -> Entry {
    let mut e = Entry::blank(Target::Commit(git2::Oid::ZERO_SHA1));
    "Add login".clone_into(&mut e.summary);
    e
}

/// An app with pi to ask, showing the diff of [`commit`]; its diff key.
fn showing() -> (App, u64) {
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
    app.set_explainer(&Ok(pi_for_tests()));
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(files()),
        refresh: false,
        file: None,
    });
    (app, diff_key(&commit(), &files()))
}

fn diff_mut(app: &mut App) -> &mut crate::app::DetailView {
    match app.views.last_mut() {
        Some(View::Detail(d)) => d,
        other => panic!("expected the diff, got {other:?}"),
    }
}

fn panel(app: &App) -> &crate::app::AnswerView {
    match app.views.last() {
        Some(View::Answer(v)) => v,
        other => panic!("expected the answer panel, got {other:?}"),
    }
}

fn ask_of(effects: &[Effect]) -> &crate::explain::question::Ask {
    match effects {
        [Effect::Ask(asked)] => asked,
        other => panic!("expected one question, got {other:?}"),
    }
}

/// A pointer at hunk `hunk` of `src/a.rs`.
fn at(hunk: usize) -> Ref {
    Ref {
        path: "src/a.rs".to_owned(),
        hunk: Some(hunk),
        label: String::new(),
    }
}

fn answer(text: &str, refs: Vec<Ref>, about: &str) -> Question {
    Question {
        question: "q".to_owned(),
        answer: text.to_owned(),
        refs,
        about: Some(about.to_owned()),
    }
}

#[test]
fn asking_needs_an_agent_and_a_file() {
    let (mut app, _) = showing();
    app.explain.agent = None;
    assert_eq!(press(&mut app, KeyCode::Char('a')), []);
    assert_eq!(app.notice.as_deref(), Some("Can't ask: no agent is set up"));
    app.set_explainer(&Err("network access is off (--no-network)".to_owned()));
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(
        app.notice.as_deref(),
        Some("Can't ask: network access is off (--no-network)")
    );
    assert!(app.ask.draft.is_none());

    app.set_explainer(&Ok(pi_for_tests()));
    diff_mut(&mut app).files.clear();
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(
        app.notice.as_deref(),
        Some("Nothing to ask about: the change is empty")
    );
}

#[test]
fn a_question_about_the_selection_is_written_sent_and_answered() {
    let (mut app, key) = showing();
    // Nothing selected: the whole file.
    press(&mut app, KeyCode::Char('a'));
    let draft = app.ask.draft.as_ref().expect("draft");
    assert_eq!(
        draft.about,
        About {
            place: "src/a.rs".to_owned(),
            excerpt: String::new(),
        }
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.ask.draft.is_none());

    // The second hunk's lines.
    let d = diff_mut(&mut app);
    d.scroll = d.rendered.hunk_starts[1] + 1;
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(
        app.ask.draft.as_ref().map(|d| d.about.clone()),
        Some(About {
            place: "src/a.rs:41".to_owned(),
            excerpt: "@@ -40 +41 @@\n-old\n+new\n".to_owned(),
        })
    );
    // Keys go to the prompt, not the diff.
    assert_eq!(press(&mut app, KeyCode::Enter), []);
    assert_eq!(app.notice.as_deref(), Some("Write a question first"));
    typed(&mut app, " is this safe? ");
    let effects = press(&mut app, KeyCode::Enter);
    let asked = ask_of(&effects);
    assert_eq!(asked.key, key);
    assert_eq!(asked.question, "is this safe?");
    assert_eq!(asked.explanation, None, "no explanation is needed");
    assert_eq!(asked.earlier, Vec::new());
    assert_eq!(
        asked.about.as_ref().map(|a| a.place.as_str()),
        Some("src/a.rs:41")
    );
    assert_eq!(asked.files, files());

    // The panel shows the agent working; one question at a time.
    let v = panel(&app);
    assert_eq!(
        v.pending.as_ref().map(|p| p.question.as_str()),
        Some("is this safe?")
    );
    assert_eq!(app.explain.question_asked(key), Some(100));
    assert_eq!(press(&mut app, KeyCode::Char('a')), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("pi is still answering your last question about this change")
    );

    let pointer = at(1);
    app.notice = None;
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Yes.", vec![pointer.clone()], "src/a.rs:41")),
    });
    let v = panel(&app);
    assert_eq!(v.pending, None);
    assert_eq!(v.answered.len(), 1);
    assert_eq!(app.notice, None, "the panel on screen shows it");
    assert_eq!(app.explain.question_asked(key), None);
    // It joins the change's questions for a later explanation.
    app.update(Msg::Explained {
        key,
        result: Ok(Answer {
            explanation: Explanation::validate(r#"{"intent": "x"}"#, &files()).expect("valid"),
            cached: false,
            agent: None,
        }),
    });
    assert_eq!(
        app.explain.explanation(key).map(|e| e.questions.len()),
        Some(1)
    );

    // A follow-up: about the same lines, replaying what was asked, with the
    // explanation that has come since.
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "and then?");
    let effects = press(&mut app, KeyCode::Enter);
    let asked = ask_of(&effects);
    assert_eq!(
        asked.about.as_ref().map(|a| a.place.as_str()),
        Some("src/a.rs:41")
    );
    assert_eq!(asked.earlier.len(), 1);
    assert!(asked.explanation.is_some());
    assert!(
        matches!(app.views.as_slice(), [View::Detail(_), View::Answer(_)]),
        "one panel"
    );

    // ⏎ goes to the code the first answer points at.
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Then.", Vec::new(), "src/a.rs:41")),
    });
    assert_eq!(panel(&app).target, 0, "the newest answer has no code");
    press(&mut app, KeyCode::Enter);
    let d = diff_mut(&mut app);
    assert_eq!(d.focus, Focus::Diff);
    assert_eq!(d.scroll, d.rendered.hunk_starts[1].min(d.rendered.len()));
    assert!(d.selection.is_some(), "same file: the selection stays");
}

#[test]
fn a_left_panel_comes_back_and_says_when_an_answer_is_ready() {
    let (mut app, key) = showing();
    assert_eq!(press(&mut app, KeyCode::Char('A')), []);
    assert_eq!(
        app.notice.as_deref(),
        Some("No questions asked about this change yet: press a to ask one")
    );
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "why?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.views.last(), Some(View::Detail(_))), "parked");
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Because.", Vec::new(), "src/a.rs")),
    });
    assert_eq!(
        app.notice.as_deref(),
        Some("An answer is ready: open the change and press A to read it")
    );
    press(&mut app, KeyCode::Char('A'));
    assert_eq!(panel(&app).answered.len(), 1);

    // A failure on screen is shown in the panel, not the footer.
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "more?");
    press(&mut app, KeyCode::Enter);
    app.update(Msg::Answered {
        key,
        result: Err("pi: timed out \u{1b}[31m".to_owned()),
    });
    assert_eq!(app.notice, None);
    assert_eq!(panel(&app).failed.as_deref(), Some("pi: timed out ·[31m"));
    assert_eq!(panel(&app).pending, None);
    // Off screen, the footer says so.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('A'));
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "again?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    app.update(Msg::Answered {
        key,
        result: Err("nope".to_owned()),
    });
    assert_eq!(app.notice.as_deref(), Some("Couldn't answer: nope"));
}

#[test]
fn the_panel_moves_between_pointers_and_scrolls() {
    let (mut app, key) = showing();
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "why?");
    press(&mut app, KeyCode::Enter);
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Two.", vec![at(0), at(1)], "src/a.rs")),
    });
    assert!(!panel(&app).follow);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(panel(&app).target, 1);
    assert!(panel(&app).follow);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(panel(&app).target, 1, "stays on the last");
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(panel(&app).target, 0);
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(panel(&app).target, 1);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(panel(&app).target, 0);
    if let Some(View::Answer(v)) = app.views.last_mut() {
        v.rows = 50;
        v.height = 10;
        v.scroll = 0;
    }
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(panel(&app).scroll, 1);
    assert!(!panel(&app).follow, "scrolling by hand stops following");
    press(&mut app, KeyCode::PageDown);
    assert_eq!(panel(&app).scroll, 11);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(panel(&app).scroll, 6);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    press(&mut app, KeyCode::PageUp);
    press(&mut app, KeyCode::Char('K'));
    assert_eq!(panel(&app).scroll, 0);
}

#[test]
fn selecting_lines_moves_a_cursor_and_clears_when_the_lines_go() {
    let (mut app, _) = showing();
    let d = diff_mut(&mut app);
    d.height = 3;
    press(&mut app, KeyCode::Char('v'));
    let d = diff_mut(&mut app);
    assert_eq!(
        d.focus,
        Focus::Diff,
        "v from the file list selects in the diff"
    );
    assert_eq!(
        d.selection,
        Some(Selection {
            anchor: 0,
            cursor: 0
        })
    );
    press(&mut app, KeyCode::Char('G'));
    let d = diff_mut(&mut app);
    let last = d.rendered.len() - 1;
    assert_eq!(
        d.selection,
        Some(Selection {
            anchor: 0,
            cursor: last
        })
    );
    assert_eq!(d.scroll, last + 1 - 3, "the view follows the cursor");
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(diff_mut(&mut app).scroll, 0);
    for code in [
        KeyCode::Down,
        KeyCode::PageDown,
        KeyCode::Up,
        KeyCode::PageUp,
    ] {
        press(&mut app, code);
    }
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(diff_mut(&mut app).selection.map(|s| s.cursor), Some(0));
    // v again ends it; so does going to the file list.
    press(&mut app, KeyCode::Char('v'));
    assert_eq!(diff_mut(&mut app).selection, None);
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Tab);
    assert_eq!(diff_mut(&mut app).selection, None);
    // And another file.
    press(&mut app, KeyCode::Char('v'));
    let d = diff_mut(&mut app);
    d.scroll = d.rendered.hunk_starts[1];
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(
        diff_mut(&mut app).selection,
        None,
        "n went on to the next file"
    );
    // A refresh that changes the file's diff drops it; one that doesn't keeps it.
    press(&mut app, KeyCode::Char('v'));
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(files()),
        refresh: true,
        file: None,
    });
    assert!(diff_mut(&mut app).selection.is_some());
    let mut changed = files();
    changed[1].hunks[0].lines[1].content = "other".to_owned();
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(changed),
        refresh: true,
        file: None,
    });
    assert_eq!(diff_mut(&mut app).selection, None);
    // Nothing to select in an empty diff.
    diff_mut(&mut app).rendered = crate::ui::detail::RenderedDiff::default();
    press(&mut app, KeyCode::Char('v'));
    assert_eq!(diff_mut(&mut app).selection, None);
}

#[test]
fn the_question_is_edited_like_any_prompt() {
    let (mut app, _) = showing();
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "why this herx");
    press(&mut app, KeyCode::Backspace);
    let text = |app: &App| app.ask.draft.as_ref().map(|d| d.text.clone());
    assert_eq!(text(&app).as_deref(), Some("why this her"));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(text(&app).as_deref(), Some("why this "));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
    )));
    press(&mut app, KeyCode::Left);
    assert_eq!(text(&app).as_deref(), Some("why this "), "nothing typed");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(text(&app).as_deref(), Some(""));
}

#[test]
fn a_follow_up_from_the_explanation_on_screen_needs_no_footer() {
    let (mut app, key) = showing();
    // A panel parked for this diff.
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "why?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Because.", Vec::new(), "src/a.rs")),
    });
    // The explanation, open, asks a follow-up of its own.
    press(&mut app, KeyCode::Char('x'));
    app.update(Msg::Explained {
        key,
        result: Ok(Answer {
            explanation: Explanation::validate(r#"{"intent": "x"}"#, &files()).expect("valid"),
            cached: false,
            agent: None,
        }),
    });
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "and?");
    press(&mut app, KeyCode::Enter);
    app.notice = None;
    app.update(Msg::Answered {
        key,
        result: Ok(Question {
            about: None,
            ..answer("Also.", Vec::new(), "")
        }),
    });
    assert_eq!(app.notice, None, "the explanation on screen shows it");
    let Some(View::Explain(v)) = app.views.last() else {
        panic!("explanation");
    };
    assert_eq!(v.explanation().map(|e| e.questions.len()), Some(2));
}

#[test]
fn a_diff_that_moves_under_the_prompt_is_asked_as_it_is_now() {
    let (mut app, old) = showing();
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "why?");
    // The live diff refreshes while the question is written.
    let mut now = files();
    now[0].hunks[1].lines[1].content = "newer".to_owned();
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(now.clone()),
        refresh: true,
        file: None,
    });
    let effects = press(&mut app, KeyCode::Enter);
    let asked = ask_of(&effects);
    let key = diff_key(&commit(), &now);
    assert_ne!(key, old);
    assert_eq!((asked.key, &asked.files), (key, &now));
    assert_eq!(panel(&app).key, key);
    // It moves again while the agent works: no second question meanwhile.
    press(&mut app, KeyCode::Esc);
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(files()),
        refresh: true,
        file: None,
    });
    press(&mut app, KeyCode::Char('a'));
    assert!(app.ask.draft.is_none());
    assert_eq!(
        app.notice.as_deref(),
        Some("pi is still answering your last question about this change")
    );
    app.update(Msg::Answered {
        key,
        result: Ok(answer("Because.", Vec::new(), "src/a.rs")),
    });
    // The panel belongs to the change, whichever version is open now.
    press(&mut app, KeyCode::Char('A'));
    assert_eq!(panel(&app).answered.len(), 1);
}

#[test]
fn hunk_jumps_drop_the_selection() {
    let (mut app, _) = showing();
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(diff_mut(&mut app).selection, None);
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('N'));
    assert_eq!(diff_mut(&mut app).selection, None);
}

#[test]
fn each_change_keeps_its_own_panel_until_its_answer_lands() {
    let (mut app, key_a) = showing();
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "about a?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    // Another change, asked about and left before the first answers.
    let mut other = commit();
    other.id = "other".to_owned();
    app.update(Msg::Diff {
        entry: Box::new(other.clone()),
        result: Ok(files()),
        refresh: false,
        file: None,
    });
    press(&mut app, KeyCode::Char('a'));
    typed(&mut app, "about b?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    app.update(Msg::Answered {
        key: key_a,
        result: Ok(answer("For a.", Vec::new(), "src/a.rs")),
    });
    assert_eq!(
        app.notice.as_deref(),
        Some("An answer is ready: open the change and press A to read it")
    );
    // Back on the first change: its panel has its answer.
    press(&mut app, KeyCode::Esc);
    app.update(Msg::Diff {
        entry: Box::new(commit()),
        result: Ok(files()),
        refresh: false,
        file: None,
    });
    press(&mut app, KeyCode::Char('A'));
    let v = panel(&app);
    assert_eq!(v.answered.len(), 1);
    assert_eq!(v.answered[0].agent, "pi", "credited to the agent asked");
    assert_eq!(v.answered[0].question.answer, "For a.");
    // Leaving it again keeps one panel per change.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('A'));
    assert_eq!(panel(&app).answered.len(), 1);
}
