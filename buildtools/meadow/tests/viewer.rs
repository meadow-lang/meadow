//! The `:ir` viewer, driven with keys and read off what it drew.

use meadow::viewer::{self, Notation, Page, Step, Viewer};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(v: &mut Viewer, code: KeyCode) {
    assert_eq!(v.key(key(code)), Step::Continue, "{code:?}");
}

/// A page of `n` numbered lines, the `wide`th of them `width` long.
fn page(title: &str, n: usize) -> Page {
    Page {
        title: title.into(),
        text: (1..=n).map(|i| format!("line {i}\n")).collect(),
        notation: Notation::Text,
    }
}

fn three() -> Vec<Page> {
    vec![page("Core", 100), page("Cut", 5), page("AxCut", 30)]
}

/// What the viewer draws on a screen of this size, as text.
fn screen(v: &mut Viewer, width: u16, height: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| v.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn tab_turns_the_page_and_goes_round_at_the_ends() {
    let mut v = Viewer::new("sumTo", three()).plain();
    assert_eq!(v.current().title, "Core");
    press(&mut v, KeyCode::Tab);
    assert_eq!(v.current().title, "Cut");
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::Tab);
    assert_eq!(v.current().title, "Core", "past the last is the first");
    press(&mut v, KeyCode::BackTab);
    assert_eq!(v.current().title, "AxCut", "before the first is the last");
    // Shift-Tab as some terminals send it: a Tab with Shift held.
    assert_eq!(
        v.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
        Step::Continue
    );
    assert_eq!(v.current().title, "Cut");
}

#[test]
fn scrolling_stops_at_the_top_and_where_the_last_line_is_at_the_bottom() {
    let mut v = Viewer::new("x", three()).plain();
    // 24 rows, four of them the viewer's own: 20 lines of the page show.
    v.resize(80, 24);
    press(&mut v, KeyCode::Up);
    press(&mut v, KeyCode::Char('k'));
    assert_eq!(v.place(), (0, 0));
    press(&mut v, KeyCode::Down);
    press(&mut v, KeyCode::Char('j'));
    assert_eq!(v.place(), (2, 0));
    press(&mut v, KeyCode::PageDown);
    assert_eq!(v.place(), (22, 0), "a page is the lines that show");
    press(&mut v, KeyCode::Char(' '));
    press(&mut v, KeyCode::PageUp);
    assert_eq!(v.place(), (22, 0));
    press(&mut v, KeyCode::Char('b'));
    assert_eq!(v.place(), (2, 0));
    press(&mut v, KeyCode::End);
    assert_eq!(v.place(), (80, 0), "100 lines, the last 20 showing");
    press(&mut v, KeyCode::Down);
    press(&mut v, KeyCode::PageDown);
    assert_eq!(v.place(), (80, 0));
    press(&mut v, KeyCode::Home);
    assert_eq!(v.place(), (0, 0));
    press(&mut v, KeyCode::Char('G'));
    assert_eq!(v.place(), (80, 0));
    press(&mut v, KeyCode::Char('g'));
    assert_eq!(v.place(), (0, 0));
    // A page shorter than the screen does not scroll at all.
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::End);
    press(&mut v, KeyCode::Down);
    assert_eq!(v.place(), (0, 0));
}

#[test]
fn each_page_keeps_its_place() {
    let mut v = Viewer::new("x", three()).plain();
    v.resize(80, 24);
    press(&mut v, KeyCode::PageDown);
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::Down);
    assert_eq!(v.place(), (1, 0));
    press(&mut v, KeyCode::Tab);
    assert_eq!(v.current().title, "Core");
    assert_eq!(v.place(), (20, 0));
}

#[test]
fn a_long_line_scrolls_sideways_as_far_as_its_end() {
    let pages = vec![Page {
        title: "LLVM IR".into(),
        text: format!("short\n{}\n", "x".repeat(100)),
        notation: Notation::Llvm,
    }];
    let mut v = Viewer::new("x", pages).plain();
    v.resize(30, 10);
    press(&mut v, KeyCode::Left);
    assert_eq!(v.place(), (0, 0));
    press(&mut v, KeyCode::Right);
    press(&mut v, KeyCode::Char('l'));
    assert_eq!(v.place(), (0, 8), "four columns a key");
    for _ in 0..30 {
        press(&mut v, KeyCode::Right);
    }
    assert_eq!(v.place(), (0, 70), "until the line's end is at the edge");
    press(&mut v, KeyCode::Char('h'));
    assert_eq!(v.place(), (0, 66));
    // Not wrapped: the long line is one row, cut at the edge.
    let drawn = screen(&mut v, 30, 10);
    assert_eq!(drawn[3], "x".repeat(30));
}

#[test]
fn a_resize_keeps_every_page_within_its_bounds() {
    let mut v = Viewer::new("x", three()).plain();
    v.resize(80, 24);
    press(&mut v, KeyCode::End);
    assert_eq!(v.place(), (80, 0));
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::Tab);
    press(&mut v, KeyCode::End);
    assert_eq!(v.place(), (10, 0));
    // Taller: the end of each page comes up to meet the bottom.
    v.resize(80, 44);
    assert_eq!(v.place(), (0, 0), "30 lines fit in 40");
    press(&mut v, KeyCode::Tab);
    assert_eq!(v.place(), (60, 0));
    // Shorter: a place still in bounds is kept.
    v.resize(80, 14);
    assert_eq!(v.place(), (60, 0));
    press(&mut v, KeyCode::End);
    assert_eq!(v.place(), (90, 0));
    // Drawing fits the viewer to the screen it is drawn on.
    screen(&mut v, 80, 54);
    assert_eq!(v.place(), (50, 0));
}

#[test]
fn the_screen_has_the_tabs_the_page_and_the_keys() {
    let mut v = Viewer::new("fun sumTo m = go m 0\n  where go = 1", three()).plain();
    press(&mut v, KeyCode::Down);
    let drawn = screen(&mut v, 100, 10);
    // The entry's first line, and a tab for each page.
    assert!(drawn[0].starts_with("fun sumTo m = go m 0 …"), "{drawn:?}");
    assert!(drawn[0].contains(" Core  Cut  AxCut"), "{drawn:?}");
    assert!(drawn[1].starts_with("───"), "{drawn:?}");
    assert_eq!(drawn[2], "line 2");
    assert_eq!(drawn[7], "line 7");
    assert!(drawn[8].starts_with("───"), "{drawn:?}");
    assert!(drawn[9].starts_with("Tab next page"), "{drawn:?}");
    assert!(drawn[9].ends_with("2-7 of 100"), "{drawn:?}");
}

#[test]
fn a_long_heading_gives_way_to_the_tabs() {
    let long = "fun sumTo (m : Int) : Int = let rec go n acc = if n == 0 then acc else go (n - 1) (acc + n) in go m 0";
    let mut v = Viewer::new(long, three()).plain();
    let drawn = screen(&mut v, 60, 8);
    assert!(drawn[0].starts_with("fun sumTo"), "{drawn:?}");
    assert!(drawn[0].contains("…   Core  Cut  AxCut"), "{drawn:?}");
    // And the keys are the ones that matter most, beside where the page is.
    assert!(drawn[7].starts_with("Tab page  ↑↓ scroll"), "{drawn:?}");
    assert!(drawn[7].ends_with("1-4 of 100"), "{drawn:?}");
}

#[test]
fn q_escape_and_ctrl_c_close_it_and_other_keys_do_not() {
    for close in [
        key(KeyCode::Char('q')),
        key(KeyCode::Esc),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut v = Viewer::new("x", three());
        assert_eq!(v.key(close), Step::Close, "{close:?}");
    }
    let mut v = Viewer::new("x", three());
    assert_eq!(v.key(key(KeyCode::Char('x'))), Step::Continue);
    assert_eq!(v.key(key(KeyCode::Enter)), Step::Continue);
}

#[test]
fn an_empty_page_still_draws() {
    let pages = vec![Page {
        title: "Bytecode".into(),
        text: String::new(),
        notation: Notation::Bytecode,
    }];
    let mut v = Viewer::new("x", pages).plain();
    press(&mut v, KeyCode::End);
    let drawn = screen(&mut v, 40, 6);
    assert!(drawn[5].ends_with("1-1 of 1"), "{drawn:?}");
}

#[test]
fn without_a_terminal_the_pages_are_printed_under_their_titles() {
    let printed = viewer::print(&[page("Core", 2), page("Cut", 1)]);
    assert_eq!(
        printed,
        "=== Core ===\nline 1\nline 2\n=== Cut ===\nline 1\n"
    );
}
