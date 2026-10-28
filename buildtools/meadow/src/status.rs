//! What `meadow` says while it works, in the manner of cargo:
//!
//! ```text
//!     Updating git repository `https://github.com/someone/meadow-json`
//!        Fetch [==============>          ]  58.00%, 1.2 MiB/s
//!    Compiling json v0.3.0 (https://github.com/someone/meadow-json#a1b2c3d4)
//!    Compiling app v0.1.0 (/home/me/app)
//!     Building [=================>       ] 2/3: app
//!     Finished `debug` profile [O1, jit] in 0.42s
//!      Running `main`
//! ```
//!
//! A verb, right-aligned to twelve columns and in bold green, then what it is
//! about. A bar, when there is one, is the last line and is redrawn in place;
//! a status line printed while it is up goes above it.
//!
//! Everything goes to stderr, so that what a program prints is all there is on
//! stdout. Nothing is said at all until [`enable`] is called: the build is also
//! run by the language server and by tests, and neither wants a narration.
//! Bars are drawn only on a terminal, and colour follows the same rule --
//! `NO_COLOR` turns it off, as `CARGO_TERM_COLOR` would for cargo.
//! On Windows it also needs a console that draws ANSI escapes, which it is
//! asked to do first (`meadow_core::console::ansi`).

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// The bar currently on screen, if any: its text, and how many columns it
/// takes, so that a status line can clear it and draw it again below.
static BAR: Mutex<Option<(String, usize)>> = Mutex::new(None);

/// Start saying things. The command line calls this; nothing else should.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn terminal() -> bool {
    std::io::stderr().is_terminal()
}

fn colour() -> bool {
    terminal() && std::env::var_os("NO_COLOR").is_none() && meadow_compiler::core::console::ansi()
}

/// `word` padded to twelve columns, then coloured -- in that order, since an
/// escape takes no columns but would count towards the padding.
fn verb(word: &str, code: &str) -> String {
    let padded = format!("{word:>12}");
    if colour() {
        format!("\x1b[1;{code}m{padded}\x1b[0m")
    } else {
        padded
    }
}

fn label(word: &str, code: &str) -> String {
    if colour() {
        format!("\x1b[1;{code}m{word}\x1b[0m")
    } else {
        word.to_string()
    }
}

/// Write `line` to stderr above whatever bar is showing.
fn above_bar(line: &str) {
    let bar = BAR.lock().unwrap_or_else(|e| e.into_inner());
    let mut err = std::io::stderr().lock();
    if let Some((_, width)) = bar.as_ref() {
        let _ = write!(err, "\r{}\r", " ".repeat(*width));
    }
    let _ = writeln!(err, "{line}");
    if let Some((text, _)) = bar.as_ref() {
        let _ = write!(err, "{text}");
    }
    let _ = err.flush();
}

/// Write `line` to standard output, over the bar rather than through it.
///
/// What a test run reports belongs on standard output -- it is the answer, not
/// narration about how the answer is coming along -- and the bar lives on
/// standard error. So one is taken off the screen while the other is written,
/// and put back afterwards.
pub fn say(line: &str) {
    let bar = BAR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, width)) = bar.as_ref() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{}\r", " ".repeat(*width));
        let _ = err.flush();
    }
    println!("{line}");
    if let Some((text, _)) = bar.as_ref() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "{text}");
        let _ = err.flush();
    }
}

/// `   Compiling app v0.1.0 (/home/me/app)`, in bold green.
pub fn status(word: &str, msg: impl AsRef<str>) {
    if enabled() {
        above_bar(&format!("{} {}", verb(word, "32"), msg.as_ref()));
    }
}

/// The same in bold cyan, for what is noted rather than done: `Fresh`,
/// `Unchanged`.
pub fn note(word: &str, msg: impl AsRef<str>) {
    if enabled() {
        above_bar(&format!("{} {}", verb(word, "36"), msg.as_ref()));
    }
}

/// `warning: …`, in bold yellow.
pub fn warning(msg: impl AsRef<str>) {
    above_bar(&format!("{} {}", label("warning:", "33"), msg.as_ref()));
}

/// `error: …`, in bold red. Said whether or not narration is on: an error is
/// never narration.
pub fn error(msg: impl AsRef<str>) {
    above_bar(&format!("{} {}", label("error:", "31"), msg.as_ref()));
}

/// A compile error: its message, and where it is in the file, when that file
/// can be read -- as a snippet with the place underlined.
pub fn diagnostic(d: &meadow_compiler::diagnostics::Diagnostic) {
    match std::fs::read_to_string(&d.filename) {
        Ok(text) if (d.label.1.end as usize) <= text.len() => {
            let rendered = meadow_compiler::diagnostics::render(d, &text, colour());
            above_bar(rendered.trim_end());
        }
        _ => error(format!("{}: {}", d.filename, d.msg)),
    }
}

/// `text` in the colour `code`, for something written to **stdout** -- a test's
/// `ok` or `FAILED` -- and so coloured only when stdout is a terminal.
pub fn paint(text: &str, code: &str) -> String {
    let wanted = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && meadow_compiler::core::console::ansi();
    if wanted {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// A size as a person reads one: `512 B`, `8.2 KiB`.
pub fn size(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024.0 {
        return format!("{bytes:.0} B");
    }
    let mut value = bytes / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Elapsed time as cargo gives it: `0.42s`, `12.30s`, `1m 05s`.
pub fn elapsed(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.2}s")
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

const WIDTH: usize = 25;

/// `[==========>              ]`, `fraction` of the way.
fn bar(fraction: f64, colour: bool) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * WIDTH as f64).round() as usize;
    let (head, rest) = if filled >= WIDTH {
        ("=".repeat(WIDTH), String::new())
    } else {
        let filled = filled.max(1);
        (
            format!("{}>", "=".repeat(filled - 1)),
            " ".repeat(WIDTH - filled),
        )
    };
    if colour {
        format!("[\x1b[1;36m{head}\x1b[0m{rest}]")
    } else {
        format!("[{head}{rest}]")
    }
}

/// `    Building [=====>      ] 2/3: app` -- kept apart from the drawing so
/// that it can be tested.
pub fn building_line(done: usize, total: usize, current: &str, colour: bool) -> String {
    let fraction = if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    };
    let word = format!("{:>12}", "Building");
    let word = if colour {
        format!("\x1b[1;36m{word}\x1b[0m")
    } else {
        word
    };
    let tail = if current.is_empty() {
        String::new()
    } else {
        format!(": {current}")
    };
    format!("{word} {} {done}/{total}{tail}", bar(fraction, colour))
}

/// `     Testing [=====>      ] 12/40: Vector.mapKeepsLength`
///
/// The count of what has failed is shown only once something has: a run that
/// is going well should say so by not mentioning it.
pub fn testing_line(
    done: usize,
    total: usize,
    failed: usize,
    current: &str,
    colour: bool,
) -> String {
    let fraction = if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    };
    let word = format!("{:>12}", "Testing");
    let word = if colour {
        format!("\x1b[1;36m{word}\x1b[0m")
    } else {
        word
    };
    let failed = match failed {
        0 => String::new(),
        n if colour => format!(" \x1b[1;31m{n} failed\x1b[0m"),
        n => format!(" {n} failed"),
    };
    let tail = if current.is_empty() {
        String::new()
    } else {
        format!(": {current}")
    };
    format!(
        "{word} {} {done}/{total}{failed}{tail}",
        bar(fraction, colour)
    )
}

/// `       Fetch [=====>      ]  45.00%, 1.2 MiB/s`
pub fn fetch_line(percent: f64, rate: Option<&str>, colour: bool) -> String {
    let word = format!("{:>12}", "Fetch");
    let word = if colour {
        format!("\x1b[1;36m{word}\x1b[0m")
    } else {
        word
    };
    let rate = rate.map(|r| format!(", {r}")).unwrap_or_default();
    format!(
        "{word} {} {percent:>6.2}%{rate}",
        bar(percent / 100.0, colour)
    )
}

/// Put `line` on screen as the bar, replacing whatever bar was there.
fn show_bar(line: String) {
    if !enabled() || !terminal() {
        return;
    }
    let width = visible(&line);
    let mut bar = BAR.lock().unwrap_or_else(|e| e.into_inner());
    let old = bar.as_ref().map_or(0, |(_, w)| *w);
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r{line}{}", " ".repeat(old.saturating_sub(width)));
    let _ = err.flush();
    *bar = Some((line, width));
}

/// Take the bar off the screen.
fn clear_bar() {
    let mut bar = BAR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, width)) = bar.take() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{}\r", " ".repeat(width));
        let _ = err.flush();
    }
}

/// The bar a build shows while it works through its packages.
pub struct Building {
    total: usize,
    done: usize,
}

impl Building {
    pub fn new(total: usize) -> Building {
        Building { total, done: 0 }
    }

    /// `name` is being compiled.
    pub fn working_on(&mut self, name: &str) {
        show_bar(building_line(self.done, self.total, name, colour()));
    }

    /// One more package is finished, compiled or found up to date.
    pub fn step(&mut self) {
        self.done += 1;
    }
}

impl Drop for Building {
    fn drop(&mut self) {
        clear_bar();
    }
}

/// The bar a test run shows while it works through its tests.
pub struct Testing {
    total: usize,
    done: usize,
    failed: usize,
}

impl Testing {
    pub fn new(total: usize) -> Testing {
        Testing {
            total,
            done: 0,
            failed: 0,
        }
    }

    /// `name` is running.
    pub fn working_on(&mut self, name: &str) {
        show_bar(testing_line(
            self.done,
            self.total,
            self.failed,
            name,
            colour(),
        ));
    }

    /// One more test is finished, and whether it passed.
    pub fn step(&mut self, passed: bool) {
        self.done += 1;
        if !passed {
            self.failed += 1;
        }
    }
}

impl Drop for Testing {
    fn drop(&mut self) {
        clear_bar();
    }
}

/// The bar `git` progress is shown on.
pub struct Fetching {
    last: Option<Instant>,
}

impl Fetching {
    pub fn new() -> Fetching {
        Fetching { last: None }
    }

    /// Redraw, at most ten times a second.
    pub fn update(&mut self, percent: f64, rate: Option<&str>) {
        if self
            .last
            .is_some_and(|t| t.elapsed() < Duration::from_millis(100))
        {
            return;
        }
        self.last = Some(Instant::now());
        show_bar(fetch_line(percent, rate, colour()));
    }
}

impl Default for Fetching {
    fn default() -> Self {
        Fetching::new()
    }
}

impl Drop for Fetching {
    fn drop(&mut self) {
        clear_bar();
    }
}

/// What a line of `git --progress` output says: how far through, and how
/// fast, when it is one of the lines that says.
///
/// ```text
/// Receiving objects:  45% (450/1000), 1.20 MiB | 2.30 MiB/s
/// Resolving deltas: 100% (80/80), done.
/// ```
///
/// Receiving is most of the time a clone takes, so it is the first 90% of the
/// bar and resolving the last 10%.
pub fn git_progress(line: &str) -> Option<(f64, Option<String>)> {
    let (phase, rest) = line.split_once(':')?;
    let (low, span) = match phase.trim() {
        "Receiving objects" => (0.0, 90.0),
        "Resolving deltas" => (90.0, 10.0),
        _ => return None,
    };
    let percent: f64 = rest.trim_start().split('%').next()?.trim().parse().ok()?;
    let rate = rest
        .split('|')
        .nth(1)
        .map(|r| r.trim().trim_end_matches(", done.").trim().to_string())
        .filter(|r| !r.is_empty());
    Some((low + span * percent / 100.0, rate))
}

/// How many columns `line` takes: its characters, less any colour escapes.
fn visible(line: &str) -> usize {
    let mut n = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_building_line_reads_like_cargos() {
        let line = building_line(2, 4, "app", false);
        assert_eq!(line, "    Building [============>            ] 2/4: app");
    }

    #[test]
    fn a_coloured_line_takes_the_columns_a_plain_one_does() {
        let plain = building_line(1, 3, "json", false);
        let coloured = building_line(1, 3, "json", true);
        assert_ne!(plain, coloured);
        assert_eq!(visible(&coloured), plain.chars().count());
    }

    #[test]
    fn nothing_to_build_is_a_full_bar_not_a_division_by_zero() {
        let line = building_line(0, 0, "", false);
        assert!(line.contains(&format!("[{}]", "=".repeat(WIDTH))), "{line}");
    }

    #[test]
    fn a_fetch_line_says_how_far_and_how_fast() {
        let line = fetch_line(58.0, Some("1.2 MiB/s"), false);
        assert!(line.starts_with("       Fetch ["), "{line}");
        assert!(line.ends_with(" 58.00%, 1.2 MiB/s"), "{line}");
    }

    #[test]
    fn git_progress_is_read_out_of_its_lines() {
        let (p, rate) =
            git_progress("Receiving objects:  50% (500/1000), 1.20 MiB | 2.30 MiB/s").unwrap();
        assert_eq!(p, 45.0);
        assert_eq!(rate.as_deref(), Some("2.30 MiB/s"));

        let (p, rate) = git_progress("Resolving deltas: 100% (80/80), done.").unwrap();
        assert_eq!(p, 100.0);
        assert_eq!(rate, None);

        // A finished receive says `done` after its rate.
        let (_, rate) =
            git_progress("Receiving objects: 100% (1000/1000), 2.4 MiB | 3.0 MiB/s, done.")
                .unwrap();
        assert_eq!(rate.as_deref(), Some("3.0 MiB/s"));
    }

    #[test]
    fn lines_that_are_not_progress_are_ignored() {
        assert_eq!(git_progress("Cloning into bare repository 'x'..."), None);
        assert_eq!(git_progress("remote: Enumerating objects: 12, done."), None);
    }

    #[test]
    fn verbs_line_up_at_twelve_columns() {
        // Checked without colour, which is what a test's stderr gets.
        assert_eq!(format!("{:>12}", "Compiling"), "   Compiling");
        assert_eq!(elapsed(Duration::from_millis(420)), "0.42s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn sizes_read_as_a_person_would_write_them() {
        assert_eq!(size(512.0), "512 B");
        assert_eq!(size(1536.0), "1.5 KiB");
    }
}
