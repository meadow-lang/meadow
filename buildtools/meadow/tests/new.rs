//! `meadow new`: a package in a directory of its own, and the questions it
//! asks when it is given none -- driven with keys and read off what they drew.

use meadow::new::{self, Choice, Context, Kind, Repository, Step, Wizard};
use meadow::{Options, git, package::Manifest, pipeline};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

/// A directory of our own, named after the test so concurrent ones do not
/// collide. Removed first, so a previous failed run cannot make this one pass.
fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-new-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(path: PathBuf, kind: Kind, git: bool) -> new::Options {
    new::Options {
        path,
        name: None,
        kind,
        git,
    }
}

#[test]
fn a_new_application_is_named_after_its_directory_and_builds() {
    let dir = scratch("app");
    let made = new::run(&options(dir.join("my-app"), Kind::App, false)).expect("new");
    assert_eq!(made.created.name, "MyApp");
    assert_eq!(made.repository, Repository::NotAsked);
    assert_eq!(
        Manifest::load(&made.created.root).unwrap().unwrap().name,
        "MyApp"
    );
    let out = pipeline::build(&made.created.root, Options::debug().entry("result"));
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert!(out.linked.is_some(), "an application has an entry point");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A library has nothing to run, but has a test, and the test passes.
#[test]
fn a_new_library_has_a_test_that_passes() {
    let dir = scratch("lib");
    let made = new::run(&options(dir.join("Shapes"), Kind::Lib, false)).expect("new");
    let root = &made.created.root;
    assert!(root.join("src/Lib.mw").is_file());
    assert!(!root.join("src/Main.mw").exists());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
        .arg("test")
        .current_dir(root)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(said.contains("addsTwoNumbers"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_that_is_there_already_is_refused() {
    let dir = scratch("exists");
    std::fs::create_dir_all(dir.join("Taken")).unwrap();
    let e = new::run(&options(dir.join("Taken"), Kind::App, false))
        .err()
        .expect("refused");
    assert!(
        e.contains("already exists") && e.contains("meadow init"),
        "{e}"
    );
    assert!(!dir.join("Taken/Meadow.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Its own repository, unless it is in one already, as `cargo new` has it.
#[test]
fn a_new_package_is_made_a_repository_unless_it_is_in_one() {
    let dir = scratch("git");
    let made = new::run(&options(dir.join("Repo"), Kind::App, true)).expect("new");
    match git::repository_of(&dir) {
        Some(top) => assert_eq!(made.repository, Repository::Inside(top)),
        None => match &made.repository {
            Repository::Made => assert!(dir.join("Repo/.git").exists()),
            // No `git` on this machine: said, and nothing else is the worse.
            Repository::Failed(e) => assert!(e.contains("git"), "{e}"),
            other => panic!("{other:?}"),
        },
    }
    if dir.join("Repo/.git").exists() {
        let inner = new::run(&options(dir.join("Repo/Inner"), Kind::App, true)).expect("new");
        assert!(
            matches!(inner.repository, Repository::Inside(_)),
            "{:?}",
            inner.repository
        );
        assert!(!dir.join("Repo/Inner/.git").exists());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_package_made_in_a_workspace_joins_it_and_its_repository() {
    let dir = scratch("member");
    new::run(&options(dir.join("Shop"), Kind::Workspace, false)).expect("workspace");
    let made = new::run(&options(dir.join("Shop/App"), Kind::App, true)).expect("member");
    assert!(made.created.joined.is_some(), "added to the members");
    assert!(!dir.join("Shop/App/.git").exists());
    assert!(
        matches!(
            made.repository,
            Repository::Member(_) | Repository::Inside(_)
        ),
        "{:?}",
        made.repository
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- the questions -------------------------------------------------------------

/// A world with nothing in it but `Taken`, and no repository.
fn context() -> Context {
    Context {
        git_settled: None,
        exists: Box::new(|p: &Path| p == Path::new("Taken")),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn typed(w: &mut Wizard, text: &str) {
    for c in text.chars() {
        assert_eq!(w.key(key(KeyCode::Char(c))), Step::Continue);
    }
}

fn enter(w: &mut Wizard) -> Step {
    w.key(key(KeyCode::Enter))
}

/// What the wizard draws on a screen of this size, as text.
fn screen(w: &Wizard) -> String {
    let (width, height) = (80, new::HEIGHT);
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| w.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn enter_through_the_questions_makes_an_application_where_it_is_named() {
    let mut w = Wizard::new(context()).plain();
    assert_eq!(enter(&mut w), Step::Continue, "an application");
    typed(&mut w, "Demo");
    assert_eq!(enter(&mut w), Step::Continue, "the name");
    assert!(
        screen(&w).contains("? Directory     Demo"),
        "{}",
        screen(&w)
    );
    assert_eq!(
        enter(&mut w),
        Step::Continue,
        "the directory follows the name"
    );
    assert_eq!(enter(&mut w), Step::Continue, "a repository");
    assert_eq!(
        enter(&mut w),
        Step::Create(Choice {
            path: PathBuf::from("Demo"),
            name: Some("Demo".to_string()),
            kind: Kind::App,
            git: true,
        })
    );
    // What stays on the screen: every answer, and no more questions.
    let s = screen(&w);
    for line in [
        "✔ What to make  an application",
        "✔ Name          Demo",
        "✔ Directory     Demo",
        "✔ Repository    a new one",
    ] {
        assert!(s.contains(line), "{line} in\n{s}");
    }
    assert!(!s.contains('?'), "{s}");
}

#[test]
fn a_name_the_language_could_not_refer_to_is_not_taken() {
    let mut w = Wizard::new(context()).plain();
    enter(&mut w);
    typed(&mut w, "my-app");
    assert_eq!(enter(&mut w), Step::Continue);
    let s = screen(&w);
    assert!(s.contains("? Name") && s.contains("`MyApp`"), "{s}");
    // Mended, it goes on, and the problem goes with it.
    for _ in 0.."my-app".len() {
        w.key(key(KeyCode::Backspace));
    }
    typed(&mut w, "MyApp");
    assert!(!screen(&w).contains("usable"), "{}", screen(&w));
    enter(&mut w);
    assert!(
        screen(&w).contains("? Directory     MyApp"),
        "{}",
        screen(&w)
    );
}

#[test]
fn a_directory_that_is_there_already_is_asked_again() {
    let mut w = Wizard::new(context()).plain();
    enter(&mut w);
    typed(&mut w, "Taken");
    enter(&mut w);
    assert_eq!(enter(&mut w), Step::Continue);
    let s = screen(&w);
    assert!(s.contains("Taken already exists"), "{s}");
    assert!(s.contains("? Directory"), "{s}");
}

#[test]
fn escape_goes_back_keeping_the_answers_and_cancels_at_the_first() {
    let mut w = Wizard::new(context()).plain();
    enter(&mut w);
    typed(&mut w, "Demo");
    enter(&mut w);
    assert_eq!(w.key(key(KeyCode::Esc)), Step::Continue);
    assert!(
        screen(&w).contains("? Name          Demo"),
        "{}",
        screen(&w)
    );
    assert_eq!(w.key(key(KeyCode::Esc)), Step::Continue);
    assert!(screen(&w).contains("› a library") || screen(&w).contains("› an application"));
    assert_eq!(w.key(key(KeyCode::Esc)), Step::Cancel);
    let mut w = Wizard::new(context()).plain();
    enter(&mut w);
    assert_eq!(
        w.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Step::Cancel
    );
}

/// A directory typed is kept even when the name changes; one left alone
/// follows the name.
#[test]
fn the_directory_follows_the_name_until_it_is_typed() {
    let mut w = Wizard::new(context()).plain();
    enter(&mut w);
    typed(&mut w, "Demo");
    enter(&mut w);
    typed(&mut w, "s");
    w.key(key(KeyCode::Esc));
    typed(&mut w, "2");
    enter(&mut w);
    assert!(
        screen(&w).contains("? Directory     Demos"),
        "{}",
        screen(&w)
    );
}

#[test]
fn a_library_and_a_workspace_are_chosen_from_the_list() {
    let mut w = Wizard::new(context()).plain();
    w.key(key(KeyCode::Down));
    enter(&mut w);
    typed(&mut w, "Shapes");
    enter(&mut w);
    enter(&mut w);
    w.key(key(KeyCode::Char('n')));
    let s = screen(&w);
    assert!(s.contains("✔ Repository    none"), "{s}");
    assert!(s.contains("Shapes/src/Lib.mw"), "{s}");
    match enter(&mut w) {
        Step::Create(c) => {
            assert_eq!(c.kind, Kind::Lib);
            assert!(!c.git);
        }
        other => panic!("{other:?}"),
    }

    // A workspace has no name to ask for: its directory is all it is.
    let mut w = Wizard::new(context()).plain().with_kind(Kind::Workspace);
    enter(&mut w);
    assert!(screen(&w).contains("? Directory"), "{}", screen(&w));
    typed(&mut w, "shop");
    enter(&mut w);
    enter(&mut w);
    match enter(&mut w) {
        Step::Create(c) => {
            assert_eq!(c.kind, Kind::Workspace);
            assert_eq!(c.name, None);
            assert_eq!(c.path, PathBuf::from("shop"));
        }
        other => panic!("{other:?}"),
    }
}

/// Inside a repository already there is nothing to ask about one.
#[test]
fn a_repository_is_not_asked_about_inside_one() {
    let mut w = Wizard::new(Context {
        git_settled: Some("the one at /work".to_string()),
        exists: Box::new(|_: &Path| false),
    })
    .plain();
    enter(&mut w);
    typed(&mut w, "Demo");
    enter(&mut w);
    enter(&mut w);
    let s = screen(&w);
    assert!(
        s.contains("✔ Repository    the one at /work, already"),
        "{s}"
    );
    assert!(s.contains("? Make it?"), "{s}");
}

#[test]
fn every_frame_fits_the_space_kept_for_it() {
    let mut w = Wizard::new(context()).plain();
    let mut most = w.height();
    for k in [
        KeyCode::Enter,
        KeyCode::Char('x'),
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Char('X'),
        KeyCode::Enter,
        KeyCode::Enter,
        KeyCode::Enter,
    ] {
        w.key(key(k));
        most = most.max(w.height());
    }
    assert!(most < new::HEIGHT, "{most}");
}
