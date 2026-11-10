//! MeadowBoot's prompt -- `bootstrap/src/Repl.mw`, declared with Lingua's
//! `repl!` -- against the Rust compiler's, `meadow repl`
//! (`buildtools/meadow/src/cli/repl.rs`).
//!
//! Both are started with no command, which is the prompt, their input a
//! file and their output -- standard output and standard error together,
//! in the order written -- another, and typed the same sessions: values of
//! each type, `it`, a name on its own, definitions and the redefinition of
//! one, what is wrong (a name nothing binds, types that do not agree, a
//! parse error, a division by zero), entries of several lines, every `:`
//! command, the functions of the repository's examples, and every line of
//! the tour as the tour gives them.
//!
//! Between two entries each is typed `:t ((), ((), ()), ())`, which changes
//! nothing and answers a line no entry does: what comes between two of
//! those answers is what one entry printed. What is printed first is the
//! banner. Each entry's output is compared after only what the two may
//! write differently is made the same: an error is compared by its
//! message, the line that starts `Error:`, and not by the drawing of the
//! source under it, whose labels each compiler words its own way; a time
//! is any time; and white space at the end of a line or of the output is
//! none.
//!
//! Agreement is reported by kind of entry. The test passes only when every
//! entry agrees, and it does not yet: see the numbers on
//! [`meadowboot_s_prompt_answers_as_meadow_repl_answers`].
//!
//! `MEADOWBOOT_EXE` runs a MeadowBoot already built rather than building
//! one; `MEADOWBOOT_PATIENCE` is how many seconds a session may take (1800);
//! `MEADOW_REPL_SESSIONS` (comma-separated) runs only the sessions named.
//! What each side printed, where they differ, is written to
//! `repl-differences.txt` in `CARGO_TARGET_TMPDIR`, or `MEADOW_REPL_OUT`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository")
}

/// The Rust compiler's `meadow`: the one this test was built with, unless
/// `MEADOW_REPL_RUST` names another.
fn meadow() -> PathBuf {
    std::env::var_os("MEADOW_REPL_RUST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meadow")))
}

/// What builds MeadowBoot: `MEADOWBOOT_MEADOW`, or the `meadow` under test.
fn builder() -> PathBuf {
    std::env::var_os("MEADOWBOOT_MEADOW")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meadow")))
}

/// MeadowBoot, native, as `bootstrap.rs` builds it -- or where
/// `MEADOWBOOT_EXE` says one already is.
fn meadowboot_exe() -> PathBuf {
    if let Some(exe) = std::env::var_os("MEADOWBOOT_EXE") {
        return PathBuf::from(exe);
    }
    let dir = repo().join("bootstrap");
    let build = |offline: bool| {
        let mut c = Command::new(builder());
        c.arg("build").arg("--release");
        if offline {
            c.arg("--offline");
        }
        c.arg(&dir).output().expect("meadow runs")
    };
    let mut out = build(true);
    if !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("not in the cache") {
        out = build(false);
    }
    assert!(
        out.status.success(),
        "MeadowBoot did not build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join("target/release/native/silo")
        .join(format!("MeadowBoot{}", std::env::consts::EXE_SUFFIX))
}

/// Where what the test writes goes: `MEADOW_REPL_OUT`, or the test's own
/// scratch directory.
fn out_dir() -> PathBuf {
    std::env::var_os("MEADOW_REPL_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
}

/// How long a session may take, in seconds: MeadowBoot's first entry builds
/// the standard library, and its first run makes the library's core.
fn patience() -> u64 {
    std::env::var("MEADOWBOOT_PATIENCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800)
}

// --- the tour --------------------------------------------------------------------

/// `text` as a string literal of Meadow's.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A list of strings, a line each, so far in.
fn listed(items: &[&str], indent: &str) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let inner = items
        .iter()
        .map(|s| format!("{indent}  {}", quoted(s)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("[\n{inner}\n{indent}]")
}

/// `bootstrap/src/Tour.mw` as the tour is now.
fn tour_source() -> String {
    use meadow::tour::{STEPS, Section};
    let mut out = String::from(
        "-- `:tour`'s steps, for MeadowBoot's prompt (`Repl`): the Rust prompt's\n\
         -- (`buildtools/meadow/src/tour.rs`), written out by `cargo test -p meadow\n\
         -- --test repl_differential write_tour -- --ignored` whenever the tour\n\
         -- changes, so that both take the same one. `the_tour_is_written_out`\n\
         -- fails while this file is behind it.\n\
         \n\
         -- A step: its title, the part of the tour it is in -- 0 the basics, 1\n\
         -- going further -- what it says, a line each, and what to type, in order.\n\
         @pub record Step = {\n  title : String,\n  section : Int,\n  text : [String],\n  code : [String]\n}\n\
         \n\
         @pub def steps : [Step] =\n  [\n",
    );
    let steps = STEPS
        .iter()
        .map(|s| {
            format!(
                "    Step {{\n      title = {},\n      section = {},\n      text = {},\n      code = {}\n    }}",
                quoted(s.title),
                if s.section == Section::Basics { 0 } else { 1 },
                listed(s.text, "      "),
                listed(s.code, "      "),
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    out.push_str(&steps);
    out.push_str("\n  ]\n");
    formatted(&out)
}

/// `source` laid out as `meadow fmt` lays it out.
fn formatted(source: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("repl-tour");
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let file = dir.join("Tour.mw");
    std::fs::write(&file, source).expect("the tour");
    let out = Command::new(meadow())
        .arg("fmt")
        .arg("--stdout")
        .arg(&file)
        .output()
        .expect("meadow fmt runs");
    assert!(
        out.status.success(),
        "meadow fmt failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// Write `bootstrap/src/Tour.mw` from the tour as it is now: `cargo test -p
/// meadow --test repl_differential write_tour -- --ignored`.
#[test]
#[ignore]
fn write_tour() {
    std::fs::write(repo().join("bootstrap/src/Tour.mw"), tour_source()).expect("written");
}

/// MeadowBoot's prompt takes the tour the Rust prompt takes.
#[test]
fn the_tour_is_written_out() {
    let written = std::fs::read_to_string(repo().join("bootstrap/src/Tour.mw"))
        .expect("bootstrap/src/Tour.mw")
        .replace("\r\n", "\n");
    assert!(
        written == tour_source(),
        "bootstrap/src/Tour.mw is not the tour: run `cargo test -p meadow --test \
         repl_differential write_tour -- --ignored`"
    );
}

// --- the sessions ----------------------------------------------------------------

/// What both prompts are typed between two entries: it changes nothing, and
/// what it answers no entry does.
const MARK: &str = ":t ((), ((), ()), ())";
const MARK_SAID: &str = "_ : ((), ((), ()), ())";

/// An entry: what kind it is, for the tally, and what is typed -- a line,
/// or several, ending with the blank line that enters them where the Rust
/// prompt needs one.
struct Entry {
    kind: &'static str,
    text: String,
}

fn e(kind: &'static str, text: &str) -> Entry {
    Entry {
        kind,
        text: text.to_string(),
    }
}

/// The top-level declarations of `text` that `heads` start, in that order:
/// each from its first line to the next line at the margin that is not a
/// comment, as written. A declaration of several lines is followed by a
/// blank line, which is what enters it.
fn declarations(text: &str, heads: &[&str]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for head in heads {
        let Some(start) = lines.iter().position(|l| l.starts_with(head)) else {
            panic!("no declaration starts `{head}`");
        };
        let mut end = start + 1;
        while end < lines.len()
            && (lines[end].starts_with([' ', '\t', '}', ')', ']']) || lines[end].is_empty())
        {
            end += 1;
        }
        while end > start + 1 && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        let lines = &lines[start..end];
        // `data Shape` is a whole declaration on its own, which the prompt
        // enters before its constructors are typed: such a one is typed on
        // one line.
        if head.contains("data ") {
            found.push(lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "));
            continue;
        }
        let decl = lines.join("\n");
        found.push(if lines.len() > 1 {
            format!("{decl}\n")
        } else {
            decl
        });
    }
    found
}

fn example(path: &str) -> String {
    std::fs::read_to_string(repo().join(path))
        .expect("an example")
        .replace("\r\n", "\n")
}

/// The sessions, each a name and its entries.
fn sessions() -> Vec<(&'static str, Vec<Entry>)> {
    let mut all = Vec::new();

    all.push((
        "values",
        vec![
            e("expression", "1 + 2"),
            e("it", "it * 10"),
            e("it", "it + it"),
            e("expression", "\"hi\""),
            e("expression", "\"tab\\there, a quote \\\" and ünïcödé\""),
            e("expression", "'c'"),
            e("expression", "'\\n'"),
            e("expression", "3.5"),
            e("expression", "1.0 / 3.0"),
            e("expression", "True"),
            e("expression", "not True"),
            e("expression", "()"),
            e("expression", "-7"),
            e("expression", "10000000000 * 10000000000"),
            e("expression", "(1, \"a\", True)"),
            e("expression", "[1, 2, 3]"),
            e("expression", "[]"),
            e("expression", "[1..=5]"),
            e("expression", "Just 4"),
            e("expression", "None"),
            e("expression", "Ok 1"),
            e("expression", "{ name = \"ada\", age = 36 }"),
            e("expression", "\\x -> x + 1"),
            e("expression", "\"a\" ++ \"b\""),
            e("expression", "show 42"),
            e("expression", "length [1, 2, 3]"),
            e("expression", "map (\\n -> n * 2) [1, 2, 3]"),
            e("expression", "[1, 2, 3] |> filter (\\n -> n > 1) |> sum"),
            e("expression", "if 1 < 2 then \"yes\" else \"no\""),
            e("expression", "let x = 4 in x * x"),
            e("expression", "match Just 3 with | Just n -> n | None -> 0"),
            e("name", "map"),
            e("name", "length"),
            e("name", "show"),
            e("name", "fst"),
            e("name", "it"),
            e("name", "True"),
        ],
    ));

    all.push((
        "definitions",
        vec![
            e("definition", "def x = 5"),
            e("name", "x"),
            e("expression", "x + 1"),
            e("definition", "fun f n = n + 1"),
            e("name", "f"),
            e("expression", "f 41"),
            e(
                "definition",
                "fun fib (n : Int) =\n    let rec loop a b i =\n      if i == 0 then a else loop b (a + b) (i - 1)\n     in loop 0 1 n\n",
            ),
            e("name", "fib"),
            e("expression", "fib 30"),
            e("definition", "fun swap : (a, b) -> (b, a)\n  | swap (x, y) = (y, x)\n"),
            e("expression", "swap (1, \"one\")"),
            e("definition", "fun useX u = x"),
            e("definition", "def x = 6"),
            e("expression", "x"),
            e("expression", "useX ()"),
            e("definition", "data Colour = Red | Green | Blue"),
            e("expression", "Colour.Red"),
            e("definition", "use Colour.*"),
            e("expression", "Green"),
            e("expression", "[Red, Blue]"),
            e("definition", "record Point = { px : Int, py : Int }"),
            e("expression", "Point { px = 1, py = 2 }"),
            e("expression", "(Point { px = 1, py = 2 }).px"),
            e("definition", "type Name = String"),
            e("definition", "def ada : Name = \"ada\""),
            e("definition", "@derive(Debug) data Pair = Pair Int Int"),
            e("expression", "Pair 1 2"),
            e("definition", "use Std.String as S"),
            e("expression", "S.toUpper \"shout\""),
            e("expression", "S.join \", \" [\"a\", \"b\", \"c\"]"),
            e("name", "S.toUpper"),
            e("definition", "def it = 99"),
            e("expression", "it"),
            e("command", ":module"),
        ],
    ));

    all.push((
        "errors",
        vec![
            e("error", "foo"),
            e("error", "1 + \"a\""),
            e("error", "Red"),
            e("error", "fun g x = y"),
            // What never parses is read on until a blank line enters it.
            e("error", "let = 1 in x\n"),
            e("error", ")\n"),
            e("error", "1 / 0"),
            e("error", "head []"),
            e("error", "def x : Int = \"no\""),
            e("error", "data D = D Unknown"),
            e("error", "Nope.thing"),
            e("expression", "1 + 1"),
            e("error", ":x"),
        ],
    ));

    all.push((
        "multi-line",
        vec![
            e("multi-line", "1 +\n  2"),
            e(
                "multi-line",
                "fun g x =\n    match x with\n    | 0 -> 1\n    | _ -> 2\n",
            ),
            e("expression", "g 0"),
            e("expression", "g 5"),
            e("multi-line", "[1,\n 2,\n 3]"),
            // The prompt asks whether the text parses with the `\` still in
            // it, where it is a lambda's: a blank line enters it.
            e("multi-line", "def y = \\\n  10\n"),
            e("expression", "y"),
            e("multi-line", "fun h : Int -> Int\n  | h n = n * 2\n"),
            e("expression", "h 21"),
            e("multi-line", "let a = 1 in\n  let b = 2 in\n  a + b"),
            e(
                "multi-line",
                "fun describe n =\n    if n < 0 then \"negative\"\n    else if n == 0 then \"zero\"\n    else \"positive\"\n",
            ),
            e("expression", "map describe [-1, 0, 1]"),
            e("multi-line", "\"a string with ( in it\""),
            e("multi-line", "\"${1 + 2} holes\""),
        ],
    ));

    all.push((
        "commands",
        vec![
            e("command", ":help"),
            e("command", ":h"),
            e("command", ":?"),
            e("command", ":module"),
            e("command", ":ir"),
            e("command", ":t 1 + 2"),
            e("command", ":t map"),
            e("command", ":t foo"),
            e("command", ":t \\x -> x"),
            e("definition", "def a = 1"),
            e("definition", "def a = 2"),
            e("command", ":module"),
            e("command", ":ir core a"),
            e("command", ":ir cut a"),
            e("command", ":ir zzz"),
            e("command", ":reset"),
            e("command", ":module"),
            e("error", "a"),
            e("command", ":time"),
            e("expression", "1 + 1"),
            e("command", ":time"),
            e("command", ":time 2 + 2"),
            e("command", ":cek"),
            e("expression", "3 + 3"),
            e("command", ":vm"),
            e("expression", "4 + 4"),
            e("command", ":jit"),
            e("expression", "5 + 5"),
            e("command", ":try"),
            e("command", ":tour"),
            e("command", ":tour"),
            e("command", ":next"),
            e("command", ":try"),
            e("command", ":try"),
            e("command", ":try"),
            e("command", ":try"),
            e("command", ":n"),
            e("command", ":back"),
            e("command", ":b"),
            e("command", ":b"),
            e("command", ":tour 3"),
            e("command", ":tour 10"),
            e("command", ":next"),
            e("command", ":tour advanced"),
            e("command", ":tour 20"),
            e("command", ":next"),
            e("command", ":tour basics"),
            e("command", ":tour off"),
            e("command", ":tour 99"),
            e("command", ":tour 2"),
            e("command", ":tour end"),
            e("command", ":quit"),
            e("expression", "6 + 6"),
        ],
    ));

    all.push((
        "quit",
        vec![
            e("expression", "1"),
            e("command", ":q"),
            e("expression", "2"),
        ],
    ));

    // The functions of the examples, typed in an order each can be in:
    // what one uses is typed before it.
    let tour_main = example("examples/Tour/src/Main.mw");
    let shapes = example("examples/Tour/src/Shapes.mw");
    let effects = example("examples/Effects/src/Main.mw");
    let p5 = example("examples/Euler/src/P5.mw");
    let mut ex = vec![
        e("definition", "use Std.St as St"),
        e("definition", "use Std.Collections.Vector as V"),
        e("definition", "use Std.Collections.HashMap as H"),
        e("definition", "use Std.String as S"),
    ];
    for d in declarations(
        &tour_main,
        &["fun factorial", "fun primesBelow", "fun wordCounts"],
    ) {
        ex.push(Entry {
            kind: "example",
            text: d,
        });
    }
    ex.push(e("example", "factorial 20"));
    ex.push(e("example", "primesBelow 30"));
    ex.push(e(
        "example",
        "H.getOr 0 \"the\" (wordCounts \"the cat and the hat\")",
    ));
    let shape_decls = declarations(
        &shapes,
        &[
            "@pub data Shape",
            "@pub record Point",
            "fun sqrt",
            "fun area'",
            "@pub fun area ",
            "@pub fun describe",
            "@pub fun degenerate",
            "@pub fun distance",
        ],
    );
    let mut shape_decls = shape_decls.into_iter();
    ex.push(Entry {
        kind: "example",
        text: shape_decls.next().expect("Shape"),
    });
    ex.push(e("example", "use Shape.*"));
    for d in shape_decls {
        ex.push(Entry {
            kind: "example",
            text: d,
        });
    }
    ex.push(e("example", "area (Rect 2.0 3.0)"));
    ex.push(e("example", "describe (Rect 2.0 2.0)"));
    ex.push(e("example", "degenerate (Rect 0.0 4.0)"));
    ex.push(e(
        "example",
        "distance (Point { x = 0.0, y = 0.0 }) (Point { x = 3.0, y = 4.0 })",
    ));
    ex.push(e("example", "use Std.Exn (raise, toResult)"));
    ex.push(e("example", "use Std.State (get, put, runState)"));
    for d in declarations(
        &effects,
        &[
            "effect Log",
            "effect Ask",
            "fun printingLogs",
            "fun collectingLogs",
            "fun withSettings",
            "fun checkout",
            "fun runCheckout",
        ],
    ) {
        ex.push(Entry {
            kind: "example",
            text: d,
        });
    }
    ex.push(e(
        "example",
        "runCheckout [(\"taxPercent\", 10), (\"limit\", 100)] [10, 20, 30]",
    ));
    ex.push(e(
        "example",
        "runCheckout [(\"taxPercent\", 10), (\"limit\", 10)] [10, 20, 30]",
    ));
    ex.push(e(
        "example",
        "printingLogs (\\() -> let _ = log \"hello\" in 1)",
    ));
    for d in declarations(&p5, &["@pub(pkg) def e5"]) {
        ex.push(Entry {
            kind: "example",
            text: d,
        });
    }
    ex.push(e("example", "e5"));
    all.push(("examples", ex));

    // The tour's lines, as the tour gives them: each part a session, as
    // each is a stretch of the tour.
    for (name, section) in [
        ("tour", meadow::tour::Section::Basics),
        ("tour-further", meadow::tour::Section::Advanced),
    ] {
        let lines = meadow::tour::STEPS
            .iter()
            .filter(|s| s.section == section)
            .flat_map(|s| s.code.iter())
            .map(|line| e("tour", line))
            .collect();
        all.push((name, lines));
    }

    if let Ok(only) = std::env::var("MEADOW_REPL_SESSIONS") {
        let only: Vec<&str> = only.split(',').collect();
        all.retain(|(name, _)| only.contains(name));
    }
    all
}

// --- running them ------------------------------------------------------------------

/// What a prompt printed for `input`, standard output and standard error as
/// one, or `None` where it took longer than anyone would wait.
fn printed(mut c: Command, input: &str, dir: &Path) -> Option<String> {
    std::fs::create_dir_all(dir).expect("its directory");
    let typed = dir.join("typed.txt");
    let said = dir.join("said.txt");
    std::fs::write(&typed, input).expect("the input");
    let out = std::fs::File::create(&said).expect("the output");
    let mut child = c
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .stdin(std::fs::File::open(&typed).expect("the input"))
        .stdout(Stdio::from(out.try_clone().expect("the output")))
        .stderr(Stdio::from(out))
        .spawn()
        .expect("the prompt starts");
    let until = Instant::now() + Duration::from_secs(patience());
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    Some(String::from_utf8_lossy(&std::fs::read(&said).expect("the output")).replace("\r\n", "\n"))
}

/// What is typed for `entries`: a mark before each, and one after the last.
fn typed(entries: &[Entry]) -> String {
    let mut out = format!("{MARK}\n");
    for e in entries {
        out.push_str(&e.text);
        out.push('\n');
        out.push_str(MARK);
        out.push('\n');
    }
    out
}

/// What was printed, cut at each mark's answer: the banner, then what each
/// entry printed.
fn cut(out: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    for line in out.split('\n') {
        if line.trim_end() == MARK_SAID {
            parts.push(String::new());
        } else if let Some(last) = parts.last_mut() {
            last.push_str(line);
            last.push('\n');
        }
    }
    parts
}

/// `s` without colour.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Whether `line` is part of the drawing under an error's message: its
/// frame, the source, the labels.
fn drawn(line: &str) -> bool {
    let t = line.trim_start();
    let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
    t.starts_with(['╭', '│', '╰', '─', '┬', '├', '·'])
        || (digits.len() < t.len() && digits.trim_start().starts_with('│'))
}

/// A time as any time: `time: 3.36ms (check 3.06ms, run 33.4µs)` is
/// `time: T (check T, run T)`.
fn timeless(line: &str) -> String {
    if !line.starts_with("time: ") {
        return line.to_string();
    }
    line.split(' ')
        .map(|w| {
            let tail = w.trim_end_matches([',', ')']);
            if tail.starts_with(|c: char| c.is_ascii_digit()) {
                format!("T{}", &w[tail.len()..])
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        // A time of two words, `6m 07.2s`, is one time.
        .replace("T T", "T")
}

/// What an entry printed, as it is compared.
fn normal(part: &str) -> String {
    let lines: Vec<String> = plain(part)
        .lines()
        .filter(|l| !drawn(l))
        .map(|l| timeless(l.trim_end()))
        .collect();
    lines.join("\n").trim_end().to_string()
}

/// How far the two agree, by kind of entry, and what differed.
#[derive(Default)]
struct Tally {
    kinds: BTreeMap<String, (usize, usize)>,
    log: String,
}

impl Tally {
    fn add(&mut self, kind: &str, what: &str, rust: &str, boot: &str) {
        let k = self.kinds.entry(kind.to_string()).or_default();
        k.0 += 1;
        if rust == boot {
            k.1 += 1;
        } else {
            self.log.push_str(&format!(
                "=== {kind}: {what}\n--- meadow\n{rust}\n--- MeadowBoot\n{boot}\n\n"
            ));
        }
    }

    fn report(&self) -> String {
        let mut out = String::new();
        let (mut all, mut same) = (0, 0);
        for (kind, (n, ok)) in &self.kinds {
            out.push_str(&format!("{kind:<15} {ok:>6} / {n:<6}\n"));
            all += n;
            same += ok;
        }
        out.push_str(&format!("{:<15} {same:>6} / {all:<6}\n", "all"));
        out
    }
}

fn progress(line: &str) {
    eprintln!("{line}");
    let out = out_dir().join("repl-progress.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// MeadowBoot's prompt answers every entry as `meadow repl` answers it.
///
/// Not yet, so it is ignored: run it with `--ignored` to see how far. On
/// 2026-11-09 the two agreed on 240 of 265 entries:
///
/// ```text
/// banner           9 / 9      it               2 / 2
/// command         40 / 47     multi-line       9 / 9
/// definition      19 / 20     name            10 / 10
/// error           10 / 13     tour            54 / 65
/// example         30 / 33     expression      57 / 57
/// ```
///
/// What is left is of four kinds. MeadowBoot's CEK machine has no threads
/// (`ThreadSpawn`), which ten lines of the tour's second part need, and
/// `:ir` shows none of the compiler's IRs (two). A parse error is worded by
/// each parser its own way (three, one of them a `:` line that is no
/// command, which both prompts hand to the parser). And the Rust prompt does what is better not copied: `:try` with no
/// terminal does nothing, since rustyline takes no initial text then (four);
/// an `effect` entry shows its operations and then runs one, `program has no
/// entry point` (three); `@derive(Debug) data` shows the derived `impl`'s
/// bindings and runs its dictionary, `= #dict(<closure>)`, and `:module`
/// lists them (two); and a `@pub(pkg) def` entry is compiled and shown as
/// nothing (one).
#[test]
#[ignore = "MeadowBoot's prompt does not agree with meadow repl yet: see the doc comment"]
fn meadowboot_s_prompt_answers_as_meadow_repl_answers() {
    let exe = meadowboot_exe();
    let scratch = out_dir().join("repl-sessions");
    let _ = std::fs::remove_dir_all(&scratch);
    let tmp = scratch.join("tmp");
    std::fs::create_dir_all(&tmp).expect("scratch");
    let mut t = Tally::default();
    for (name, entries) in sessions() {
        let started = Instant::now();
        let input = typed(&entries);
        let rust = printed(
            Command::new(meadow()),
            &input,
            &scratch.join(name).join("meadow"),
        );
        let mut c = Command::new(&exe);
        c.env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
            .env("MEADOW", meadow())
            .env("MEADOWBOOT_SET", "macros=Glade")
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .env("TMPDIR", &tmp)
            .env_remove("MEADOWBOOT_EXPANSIONS");
        let boot = printed(c, &input, &scratch.join(name).join("meadowboot"));
        let (Some(rust), Some(boot)) = (rust, boot) else {
            progress(&format!(
                "{name}: a prompt took longer than {}s",
                patience()
            ));
            for e in &entries {
                t.add(e.kind, &e.text, "", "(did not finish)");
            }
            continue;
        };
        let (r, b) = (cut(&rust), cut(&boot));
        let at = |parts: &[String], i: usize| normal(parts.get(i).map_or("", String::as_str));
        t.add("banner", name, &at(&r, 0), &at(&b, 0));
        for (i, entry) in entries.iter().enumerate() {
            t.add(
                entry.kind,
                &format!("{name}: {}", entry.text),
                &at(&r, i + 1),
                &at(&b, i + 1),
            );
        }
        progress(&format!("{name}: {:.0?}", started.elapsed()));
    }
    let report = t.report();
    let out = out_dir().join("repl-differences.txt");
    let _ = std::fs::write(&out, format!("{report}\n{}", t.log));
    eprintln!("{report}\nwhat differs: {}", out.display());
    assert!(
        t.kinds.values().all(|(n, ok)| n == ok),
        "the prompts answered differently:\n{report}"
    );
}
