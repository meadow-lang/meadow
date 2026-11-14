//! What MeadowBoot's `build` and `run` do with each of their flags, against
//! what `meadow`'s do (`buildtools/meadow/src/cli.rs`).
//!
//! Each case is a small package, copied once for each compiler -- a build
//! writes under the package's `target`, and the two must not read what the
//! other wrote -- and built or run by both with the same flags. What the
//! front end says is compared as it is: the type of every binding the
//! packages export (`--types`, `--annotations`), and what a program prints
//! run by either compiler's CEK machine (`--cek`). What the back end makes
//! is MeadowBoot's program, not the Rust compiler's, so of it the shape is
//! compared: which files `--emit` writes and that the image and the
//! executable run as the other's do, what `--gc-stats` and `--profile-to`
//! report but for the numbers, and what `--train` writes and the `aot`
//! build after it reads. How many nodes inference gave a type is each
//! compiler's own count of its own tree, and is not compared.
//!
//! What MeadowBoot does not act on yet it says it does not, on standard
//! error, and that is checked too.
//!
//! `MEADOWBOOT_EXE` runs a MeadowBoot already built, and `MEADOWBOOT_HOST`
//! names the `meadow-host` library it hands its programs to.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository")
}

fn meadow() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_meadow"))
}

/// What builds MeadowBoot: `MEADOWBOOT_MEADOW`, or the `meadow` under test.
fn builder() -> PathBuf {
    std::env::var_os("MEADOWBOOT_MEADOW")
        .map(PathBuf::from)
        .unwrap_or_else(meadow)
}

/// MeadowBoot, native, as `bootstrap.rs` builds it -- or where
/// `MEADOWBOOT_EXE` says one already is.
fn meadowboot_exe() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
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
            if !out.status.success()
                && String::from_utf8_lossy(&out.stderr).contains("not in the cache")
            {
                out = build(false);
            }
            assert!(
                out.status.success(),
                "MeadowBoot did not build:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            dir.join("target/release/native/silo")
                .join(format!("MeadowBoot{}", std::env::consts::EXE_SUFFIX))
        })
        .clone()
}

/// `meadow-host`, built beside the `meadow` this test runs, or where
/// `MEADOWBOOT_HOST` says one already is.
fn meadow_host() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            if let Some(given) = std::env::var_os("MEADOWBOOT_HOST") {
                return PathBuf::from(given);
            }
            let dir = meadow().parent().expect("meadow's directory").to_path_buf();
            let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            c.args(["build", "-q", "-p", "meadow-host", "--manifest-path"])
                .arg(repo().join("buildtools").join("Cargo.toml"));
            if dir.file_name().is_some_and(|n| n == "release") {
                c.arg("--release");
            }
            let out = c.output().expect("cargo runs");
            assert!(
                out.status.success(),
                "meadow-host did not build:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            dir.join(format!(
                "{}meadow_host{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ))
        })
        .clone()
}

/// What a run said: its exit status, standard output and standard error.
struct Said {
    code: Option<i32>,
    out: String,
    err: String,
}

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {:?}\n--- stdout\n{}--- stderr\n{}",
            self.code, self.out, self.err
        )
    }
}

fn said(out: std::process::Output) -> Said {
    Said {
        code: out.status.code(),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A package for each compiler, of `files` -- each a path under `src` and
/// its text -- named `name`: the Rust compiler's, and MeadowBoot's.
struct Pair {
    rust: meadow_scratch::Scratch,
    boot: meadow_scratch::Scratch,
    name: String,
}

fn pair(name: &str, files: &[(&str, &str)]) -> Pair {
    let make = |side: &str| {
        let dir = meadow_scratch::scratch!("flags-{side}-{}-{}", name, std::process::id());
        let root = dir.join(name);
        std::fs::create_dir_all(root.join("src")).expect("a package directory");
        std::fs::write(
            root.join("Meadow.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .expect("a manifest");
        for (path, text) in files {
            std::fs::write(root.join("src").join(path), text).expect("a source file");
        }
        dir
    };
    Pair {
        rust: make("rust"),
        boot: make("boot"),
        name: name.to_string(),
    }
}

impl Pair {
    fn rust_root(&self) -> PathBuf {
        self.rust.join(&self.name)
    }

    fn boot_root(&self) -> PathBuf {
        self.boot.join(&self.name)
    }

    /// `meadow COMMAND ARGS… PACKAGE`.
    fn rust(&self, command: &str, args: &[&str]) -> Said {
        said(
            Command::new(meadow())
                .arg(command)
                .args(args)
                .arg(self.rust_root())
                .output()
                .expect("meadow runs"),
        )
    }

    /// `meadowboot COMMAND ARGS… PACKAGE`, the back end loaded into it.
    fn boot(&self, command: &str, args: &[&str]) -> Said {
        said(
            Command::new(meadowboot_exe())
                .arg(command)
                .args(args)
                .arg(self.boot_root())
                .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
                .env("MEADOW_HOST", meadow_host())
                .env_remove("MEADOW")
                .env_remove("MEADOWBOOT_EXPANSIONS")
                .env_remove("MEADOWBOOT_SET")
                .output()
                .expect("MeadowBoot runs"),
        )
    }
}

/// What `--types` printed: from the first package's line to the entry's.
/// Before it, MeadowBoot says what it compiled. The count of annotated nodes
/// is each compiler's own.
fn listing(out: &str) -> String {
    let from = out.find("=== package").unwrap_or(out.len());
    let mut lines = Vec::new();
    for l in out[from..].lines() {
        if l.trim_end().ends_with(" annotated nodes)") {
            lines.push("  (N annotated nodes)".to_string());
        } else {
            lines.push(l.to_string());
        }
        if l.starts_with("entry: ") {
            break;
        }
    }
    lines.join("\n")
}

/// A line with its numbers, and the units they are in, left out, and a
/// plural made singular: what a report says, and not how much.
fn shape(line: &str) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "ns", "us", "ms", "s"];
    line.split_whitespace()
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .filter(|w| !UNITS.contains(&w.trim_end_matches([',', ')'])))
        .map(|w| {
            let w = w.trim_end_matches([',', ')']);
            w.strip_suffix('s').unwrap_or(w).to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The lines of `text` that start `prefix`, each as its `shape`.
fn shapes(text: &str, prefix: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with(prefix))
        .map(shape)
        .collect()
}

const SUMS: &str = "@pub fun double x = x * 2\n\n@pub fun twice f x = f (f x)\n";

const GATED: &str = "mod Sums\n\nuse Sums (double)\n\n\
@derive(Debug)\ndata Shape = Circle Int | Square Int\n\n\
fun area s = match s with\n  | Shape.Circle r -> 3 * r * r\n  | Shape.Square w -> w * w\n\n\
fun main () =\n  let u = println \"area ${show (area (Shape.Circle (double 2)))}\" in\n  \
println (show [1, 2, 3])\n";

const OPEN: &str = "trait Size a {\n  fun size : a -> Int\n  fun twice : a -> Int | twice x = 2 * size x\n}\n\n\
data Box a = Box a\n\n\
impl Size Int {\n  fun size n = n\n}\n\n\
impl Size (Box a) where Size a {\n  fun size b = match b with | Box.Box x -> 1 + size x\n}\n\n\
@derive(Debug)\ndata Shape = Circle Int | Square Int\n\n\
fun (<+>) a b = a + b\n\n\
fun main () = println (show (twice (Box.Box (3 <+> 4))))\n";

/// A program that allocates enough to be collected and sampled, `rounds`
/// times over.
fn busy(rounds: u32) -> String {
    format!(
        "fun build (n : Int) (acc : List Int) : List Int = if n == 0 then acc else build (n - 1) (n :: acc)\n\n\
fun total (xs : List Int) (acc : Int) : Int = match xs with | [;] -> acc | x :: rest -> total rest (acc + x)\n\n\
fun rounds (k : Int) (acc : Int) : Int = if k == 0 then acc else rounds (k - 1) (acc + total (build 2000 [;]) 0)\n\n\
fun main () = println (show (rounds {rounds} 0))\n"
    )
}

/// `build --types` and `run --types` write the type of every value each
/// package of the build exports, as `meadow` writes them -- `@pub` ones in
/// a package that marks them, every one in a package that does not, the
/// dictionaries and methods of its `impl`s and its traits' default methods
/// -- and the program's entry; `--annotations` writes that and then every
/// node's type, each package's numbered from nought, the standard library
/// first.
#[test]
fn types_and_annotations_are_written_as_meadow_writes_them() {
    let gated = pair("Gated", &[("Main.mw", GATED), ("Sums.mw", SUMS)]);
    let open = pair("Open", &[("Main.mw", OPEN)]);
    for p in [&gated, &open] {
        for (command, args) in [("build", &["--types"][..]), ("run", &["--types"][..])] {
            let theirs = p.rust(command, args);
            let ours = p.boot(command, args);
            assert_eq!(theirs.code, Some(0), "{theirs}");
            assert_eq!(ours.code, Some(0), "{ours}");
            assert_eq!(listing(&ours.out), listing(&theirs.out), "{ours}\n{theirs}");
        }
    }
    let theirs = gated.rust("build", &["--annotations"]);
    let ours = gated.boot("build", &["--annotations"]);
    assert_eq!(ours.code, Some(0), "{ours}");
    assert_eq!(listing(&ours.out), listing(&theirs.out), "{ours}");
    // Every node's line: `P#n : type`, each package's in a block of its own,
    // numbered from nought, as many of the package's as its count says.
    let blocks = |out: &str| {
        let mut blocks: Vec<(String, usize)> = Vec::new();
        for line in out.lines().filter(|l| !l.starts_with(' ')) {
            let Some((node, ty)) = line.split_once(" : ") else {
                continue;
            };
            let Some((pkg, n)) = node.split_once('#') else {
                continue;
            };
            let n: usize = n.parse().unwrap_or_else(|_| panic!("not a node: {line}"));
            assert!(!ty.trim().is_empty(), "no type: {line}");
            match blocks.last_mut() {
                Some((p, count)) if p == pkg => {
                    assert!(n >= *count, "{line} after #{count}");
                    *count = n + 1;
                }
                _ => blocks.push((pkg.to_string(), n + 1)),
            }
        }
        blocks.into_iter().map(|(p, _)| p).collect::<Vec<_>>()
    };
    assert_eq!(blocks(&ours.out), blocks(&theirs.out));
    assert_eq!(blocks(&ours.out), ["Std", "Gated"]);
    let counted = ours
        .out
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix('(')
                .and_then(|r| r.strip_suffix(" annotated nodes)"))
        })
        .and_then(|n| n.parse::<usize>().ok())
        .expect("a count");
    assert_eq!(
        ours.out.lines().filter(|l| l.starts_with("Gated#")).count(),
        counted,
        "{ours}"
    );
}

/// `run --cek` runs the program on MeadowBoot's own CEK machine, which says
/// so, and prints what `meadow run --cek` prints; with `--gc-stats`, that
/// the machine has no collector to report on.
#[test]
fn a_program_run_on_the_cek_machine_says_what_meadow_s_says() {
    let p = pair("Cek", &[("Main.mw", GATED), ("Sums.mw", SUMS)]);
    for args in [&["--cek"][..], &["--cek", "--gc-stats"][..]] {
        let theirs = p.rust("run", args);
        let ours = p.boot("run", args);
        assert_eq!(theirs.code, Some(0), "{theirs}");
        assert_eq!(ours.code, Some(0), "{ours}");
        assert!(ours.out.ends_with(&theirs.out), "{ours}\n{theirs}");
        for line in [
            "Running `Cek::main` on the CEK machine",
            "gc: the CEK machine reference-counts, so there is nothing to report",
        ] {
            assert_eq!(
                theirs.err.contains(line),
                ours.err.contains(line),
                "`{line}`:\n{ours}\n{theirs}"
            );
        }
    }
}

/// What the back end is told for a run: the collector (`--gc`), and what
/// it reports of it after (`--gc-stats`); the interpreter alone
/// (`--backend vm`); a profile of where the program spends itself
/// (`--profile-to`, of work with `--sample-every` and of time at
/// `--sample-hz`); the size of the nursery (`--nursery`); and, with
/// `--no-prune`, a program of every definition. Each run prints what
/// `meadow`'s does, and reports what it does but for the numbers.
#[test]
fn what_a_run_is_told_reaches_the_back_end_as_meadow_s_does() {
    let short = pair("Busy", &[("Main.mw", &busy(300))]);
    // A sampler of time ticks as often as the system's timer lets it (every
    // 15.6 ms on Windows, whatever `--sample-hz` asks), and a JIT runs the
    // short program in less: it has to run long enough to be sampled at all.
    let long = pair("Timed", &[("Main.mw", &busy(3000))]);
    let profile = |side: &Path, name: &str| side.join(name).display().to_string();
    let cases: Vec<Vec<String>> = vec![
        vec!["--gc-stats".into()],
        vec!["--gc-stats".into(), "--gc".into(), "copying".into()],
        vec!["--backend".into(), "vm".into()],
        vec!["--nursery".into(), "8M".into(), "--gc-stats".into()],
        vec!["--no-prune".into()],
        vec!["--leaks".into()],
        vec![
            "--profile-to".into(),
            "PROFILE".into(),
            "--sample-every".into(),
            "50".into(),
        ],
        vec![
            "--profile-to".into(),
            "PROFILE".into(),
            "--sample-hz".into(),
            "5000".into(),
        ],
    ];
    for case in &cases {
        let p = if case.iter().any(|a| a == "--sample-hz") {
            &long
        } else {
            &short
        };
        let on = |side: &Path| -> Vec<String> {
            case.iter()
                .map(|a| {
                    if a == "PROFILE" {
                        profile(side, "profile.folded")
                    } else {
                        a.clone()
                    }
                })
                .collect()
        };
        let theirs_args = on(&p.rust);
        let ours_args = on(&p.boot);
        let theirs = p.rust(
            "run",
            &theirs_args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let ours = p.boot(
            "run",
            &ours_args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert_eq!(theirs.code, Some(0), "{case:?}: {theirs}");
        assert_eq!(ours.code, Some(0), "{case:?}: {ours}");
        assert!(
            ours.out.ends_with(&theirs.out),
            "{case:?}:\n{ours}\n{theirs}"
        );
        assert_eq!(
            shapes(&ours.err, "gc:"),
            shapes(&theirs.err, "gc:"),
            "{case:?}:\n{ours}\n{theirs}"
        );
        // How many samples saw one frame is each program's own.
        assert_eq!(
            shapes(&ours.err, "profile:").first(),
            shapes(&theirs.err, "profile:").first(),
            "{case:?}:\n{ours}\n{theirs}"
        );
        assert!(!ours.err.contains("not acted on"), "{case:?}: {ours}");
        if case.contains(&"--profile-to".to_string()) {
            // Folded stacks: frames, a space, and how many samples.
            for side in [&p.rust, &p.boot] {
                let text = std::fs::read_to_string(side.join("profile.folded"))
                    .unwrap_or_else(|e| panic!("{case:?}: no profile: {e}\n{ours}"));
                assert!(!text.is_empty(), "{case:?}: an empty profile");
                for line in text.lines() {
                    let (_, count) = line.rsplit_once(' ').expect("a count");
                    count.parse::<u64>().expect("a count of samples");
                }
                let _ = std::fs::remove_file(side.join("profile.folded"));
            }
        }
    }
}

/// `run --train` runs the program on the JIT and writes what each call site
/// entered where `meadow` writes it, for the profile it ran as; and an
/// `aot` build of that profile is guided by it, saying so.
#[test]
fn a_training_run_guides_the_aot_build_after_it_as_meadow_s_does() {
    let p = pair("Trained", &[("Main.mw", &busy(300))]);
    let theirs = p.rust("run", &["--release", "--train"]);
    let ours = p.boot("run", &["--release", "--train"]);
    assert_eq!(theirs.code, Some(0), "{theirs}");
    assert_eq!(ours.code, Some(0), "{ours}");
    assert!(ours.out.ends_with(&theirs.out), "{ours}\n{theirs}");
    assert!(ours.err.contains("Trained"), "{ours}");
    for root in [p.rust_root(), p.boot_root()] {
        let calls = root.join("target").join("release").join("calls.pgo");
        let text = std::fs::read_to_string(&calls)
            .unwrap_or_else(|e| panic!("no {}: {e}", calls.display()));
        let mut lines = text.lines();
        assert!(
            lines
                .next()
                .is_some_and(|l| l.starts_with("meadow-calls 1 ")),
            "{text}"
        );
        for line in lines {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert!(
                words.len() == 3 && matches!(words[1], "frame" | "closure"),
                "{line}"
            );
        }
    }
    let theirs = p.rust("build", &["--release"]);
    let ours = p.boot("build", &["--release"]);
    assert_eq!(theirs.code, Some(0), "{theirs}");
    assert_eq!(ours.code, Some(0), "{ours}");
    assert!(theirs.err.contains("Guided by"), "{theirs}");
    assert!(ours.err.contains("Guided by"), "{ours}");
}

/// `build --emit` writes what each kind names, where `meadow` writes it and
/// nothing else of those: the image, which `meadow exec` runs; the image as
/// text; the machine code as text; and the executable, which runs as the
/// other's does. What is in them is each compiler's program's.
#[test]
fn what_a_build_emits_is_written_where_meadow_writes_it() {
    let p = pair("Emitted", &[("Main.mw", GATED), ("Sums.mw", SUMS)]);
    let exe = format!("Emitted{}", std::env::consts::EXE_SUFFIX);
    let wanted = |kinds: &str| -> Vec<String> {
        let mut files = Vec::new();
        for k in kinds.split(',') {
            match k {
                "image" => files.push("bytecode/Emitted.mbc".to_string()),
                "bytecode" => files.push("bytecode/Emitted.mbc.txt".to_string()),
                "asm" => files.push("native/Emitted.s".to_string()),
                "exe" => files.push(format!("native/{exe}")),
                _ => {}
            }
        }
        files.sort();
        files
    };
    let written = |root: &Path| -> Vec<String> {
        let mut files = Vec::new();
        for dir in ["bytecode", "native"] {
            let at = root.join("target").join("debug").join(dir);
            for e in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if ["Emitted.mbc", "Emitted.mbc.txt", "Emitted.s", exe.as_str()]
                    .contains(&name.as_str())
                {
                    files.push(format!("{dir}/{name}"));
                }
            }
        }
        files.sort();
        files
    };
    for kinds in ["image", "bytecode", "asm", "exe", "image,bytecode,asm,exe"] {
        for root in [p.rust_root(), p.boot_root()] {
            let target = root.join("target").join("debug");
            let _ = std::fs::remove_dir_all(target.join("bytecode"));
            let _ = std::fs::remove_dir_all(target.join("native"));
        }
        let theirs = p.rust("build", &["--emit", kinds]);
        let ours = p.boot("build", &["--emit", kinds]);
        assert_eq!(theirs.code, Some(0), "{kinds}: {theirs}");
        assert_eq!(ours.code, Some(0), "{kinds}: {ours}");
        assert_eq!(written(&p.rust_root()), wanted(kinds), "{kinds}: {theirs}");
        assert_eq!(written(&p.boot_root()), wanted(kinds), "{kinds}: {ours}");
        for word in ["bytecode: ", "asm: ", "Executable "] {
            assert_eq!(
                theirs.err.contains(word),
                ours.err.contains(word),
                "{kinds}: `{word}`\n{ours}\n{theirs}"
            );
        }
        let ran = |exe: &Path, args: &[&std::ffi::OsStr]| {
            said(Command::new(exe).args(args).output().expect("it runs"))
        };
        if kinds.contains("image") {
            let image = |root: &Path| root.join("target/debug/bytecode/Emitted.mbc");
            let theirs = ran(
                &meadow(),
                &["exec".as_ref(), image(&p.rust_root()).as_os_str()],
            );
            let ours = ran(
                &meadow(),
                &["exec".as_ref(), image(&p.boot_root()).as_os_str()],
            );
            assert_eq!(ours.out, theirs.out, "{ours}\n{theirs}");
        }
        if kinds.contains("exe") {
            let theirs = ran(&p.rust_root().join("target/debug/native").join(&exe), &[]);
            let ours = ran(&p.boot_root().join("target/debug/native").join(&exe), &[]);
            assert_eq!(ours.out, theirs.out, "{ours}\n{theirs}");
        }
        if kinds.contains("bytecode") {
            for root in [p.rust_root(), p.boot_root()] {
                let text =
                    std::fs::read_to_string(root.join("target/debug/bytecode/Emitted.mbc.txt"))
                        .expect("the image as text");
                assert_eq!(
                    shape(text.lines().next().unwrap_or("")),
                    "; instruction register entry",
                    "{text}"
                );
            }
        }
        if kinds.contains("asm") {
            for root in [p.rust_root(), p.boot_root()] {
                let text = std::fs::read_to_string(root.join("target/debug/native/Emitted.s"))
                    .expect("the machine code as text");
                assert!(text.starts_with("; meadow native code: "), "{text}");
                assert!(text.contains("\nfn_pc"), "no function in the listing");
            }
        }
    }
}

/// What MeadowBoot's `build` and `run` take and do not act on yet, they say
/// they do not: a package's features, the members of a workspace, and the
/// sources with each macro call expanded.
#[test]
fn what_meadowboot_does_not_act_on_yet_it_says_it_does_not() {
    let p = pair("Unheeded", &[("Main.mw", SUMS)]);
    for (command, args, flag) in [
        ("build", &["--emit", "expanded"][..], "--emit expanded"),
        (
            "build",
            &["--no-default-features"][..],
            "--no-default-features",
        ),
        ("build", &["--all-features"][..], "--all-features"),
        ("build", &["-p", "Unheeded"][..], "--package"),
        ("run", &["-p", "Unheeded"][..], "--package"),
    ] {
        let ours = p.boot(command, args);
        assert!(
            ours.err
                .contains(&format!("meadowboot: `{flag}` is not acted on yet")),
            "{command} {args:?}: {ours}"
        );
    }
}
