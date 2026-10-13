//! MeadowBoot -- the compiler written in Meadow, in `bootstrap/` -- checked
//! against this one, pass by pass.
//!
//! Each pass of MeadowBoot writes what it made as text, one item a line, and
//! the same pass here is written the same way; a file whose two texts differ
//! is a difference between the compilers, shown at its first line. The inputs
//! are every Meadow source in the repository and `bootstrap/tests/`, where the
//! cases nothing else exercises are kept.
//!
//! MeadowBoot is run by the `meadow` this test is built with. A debug `meadow`
//! builds it slowly; `MEADOWBOOT_MEADOW` names another -- a release build --
//! to run it with instead.

use meadow_compiler::core::{self, Lit, Pat, Term};
use meadow_compiler::lexer::{Token, tokenize};
use meadow_compiler::source::{Source, SourceKind};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository")
}

fn meadow() -> PathBuf {
    std::env::var_os("MEADOWBOOT_MEADOW")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meadow")))
}

/// How the Rust compiler is asked to build what MeadowBoot's answer is
/// compared with: for the one platform MeadowBoot decides a `@cfg` for --
/// Windows on x86-64, a debug build (`Scopes.enabled`, `Infer.metaHolds`) --
/// and not for whichever machine the test is run on. Otherwise a declaration
/// under `@cfg(unix)` is there on one side and not the other wherever the
/// test runs on a Unix, as `Std.Ffi`'s tests were.
fn as_meadowboot_builds() -> meadow::Options {
    let mut opts = meadow::Options::debug().entry("result");
    opts.cfg.os = "windows";
    opts.cfg.arch = "x86_64";
    opts
}

/// MeadowBoot, built once for the whole run as `meadow build --release` builds
/// it -- offline when its dependencies are fetched already, fetching them when
/// not -- and the executable that made. Its runs are of the executable, so
/// that none of them pays for a build, and as many as there is room for can
/// run at once.
fn meadowboot_exe() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = repo().join("bootstrap");
            let build = |offline: bool| {
                let mut c = Command::new(meadow());
                c.arg("build").arg("--release");
                if offline {
                    c.arg("--offline");
                }
                c.arg(&dir).output().expect("meadow runs")
            };
            let mut out = build(true);
            // Online again only for what offline could not have: a dependency
            // not fetched yet.
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

/// What MeadowBoot prints for `args`.
fn meadowboot(args: &[String]) -> String {
    let mut c = Command::new(meadowboot_exe());
    c.args(args);
    // Where the standard library's sources are, for the passes that build a
    // file's unit and the units it depends on.
    // `run` and `cut` are of programs written with no library, as the glade
    // cases are.
    if !matches!(args.first().map(String::as_str), Some("run" | "cut")) {
        c.env("MEADOWBOOT_STD", repo().join("lib").join("Std"));
    }
    // MeadowBoot runs the procedural macros itself, as a program it compiles
    // and the back end runs on Glade, so what it makes of them is part of
    // what is compared. `MEADOWBOOT_RECORDED` goes back to handing it what
    // the Rust compiler's macros expanded to, where the rename test wrote
    // it: what there was before MeadowBoot could run one, and what tells a
    // fault in running them from one in what is done with what they wrote.
    if std::env::var_os("MEADOWBOOT_RECORDED").is_some() {
        c.env("MEADOWBOOT_EXPANSIONS", expansions_dir());
    } else {
        c.env_remove("MEADOWBOOT_EXPANSIONS");
        c.env("MEADOWBOOT_SET", "macros=Glade");
        c.env("MEADOW", meadow());
    }
    let out = c.output().expect("MeadowBoot runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !stdout.is_empty(),
        "MeadowBoot printed nothing:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A unit that did not build is said so in place of what was asked of it,
    // and why is on stderr: without it a failure reads only "not compiled".
    if stdout.contains("\nnot compiled\n") {
        eprintln!(
            "MeadowBoot did not compile something of {:?}:\n{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    stdout
}

/// Every `.mw` file under `dirs`, but what a build wrote.
fn sources(dirs: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "mw") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(&repo().join(d), &mut out);
    }
    out
}

/// MeadowBoot's output for one command over `files`, split by the `== path`
/// line it writes before each: in batches, so a command line stays short.
fn per_file(command: &str, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
    per_file_with(&[command.to_string()], files)
}

/// The same, with `args` -- a command and its options -- before the files.
///
/// A package's files go to one run, since a pass that builds a unit builds
/// all of it; the files of no package are shared out between twice as many
/// runs as there are jobs, so that every job has some and none is left with
/// the long tail. The runs go at once, as many as `MEADOWBOOT_JOBS` says --
/// eight, unless it says otherwise, each MeadowBoot holding a gigabyte or so
/// -- and a run whose inputs are what they were the last time is answered
/// with what it printed then, unless `MEADOWBOOT_FRESH` is set: see
/// [`run_key`].
fn per_file_with(args: &[String], files: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let started = std::time::Instant::now();
    let mut by_package: Vec<(Option<PathBuf>, Vec<PathBuf>)> = Vec::new();
    for p in files {
        let root = package_of(p);
        match by_package.iter_mut().find(|(r, _)| *r == root) {
            Some((_, fs)) => fs.push(p.clone()),
            None => by_package.push((root, vec![p.clone()])),
        }
    }
    let jobs = std::env::var("MEADOWBOOT_JOBS")
        .ok()
        .and_then(|j| j.parse::<usize>().ok())
        .unwrap_or(8)
        .max(1);
    let mut runs: Vec<Vec<PathBuf>> = Vec::new();
    for (root, fs) in by_package {
        match root {
            Some(_) => runs.push(fs),
            None => {
                let each = fs.len().div_ceil(2 * jobs).max(1);
                runs.extend(fs.chunks(each).map(|c| c.to_vec()));
            }
        }
    }
    let fresh = std::env::var_os("MEADOWBOOT_FRESH").is_some();
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("meadowboot-runs");
    let _ = std::fs::create_dir_all(&cache);
    let exe = meadowboot_exe();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let reused = std::sync::atomic::AtomicUsize::new(0);
    let printed: Vec<std::sync::Mutex<String>> = runs
        .iter()
        .map(|_| std::sync::Mutex::new(String::new()))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(runs.len()) {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(run) = runs.get(k) else { return };
                    let at = cache.join(format!(
                        "{}-{:016x}.txt",
                        args.join("-"),
                        run_key(&exe, args, run)
                    ));
                    let text = match std::fs::read_to_string(&at) {
                        Ok(text) if !fresh => {
                            reused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            text
                        }
                        _ => {
                            let mut argv = args.to_vec();
                            argv.extend(run.iter().map(|p| p.display().to_string()));
                            let text = meadowboot(&argv);
                            // What did not build is not kept: the next
                            // run tries it again, and says why once more.
                            if !text.contains("\nnot compiled\n") {
                                let _ = std::fs::write(&at, &text);
                            }
                            text
                        }
                    };
                    *printed[k].lock().unwrap_or_else(|e| e.into_inner()) = text;
                }
            });
        }
    });
    eprintln!(
        "MeadowBoot {}: {} files in {} runs, {} of them as last time, in {:.0?}",
        args.join(" "),
        files.len(),
        runs.len(),
        reused.load(std::sync::atomic::Ordering::Relaxed),
        started.elapsed()
    );
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for (run, text) in runs.iter().zip(printed) {
        let text = text.into_inner().unwrap_or_else(|e| e.into_inner());
        // A line of a dump never starts with `== `: its lines start with a
        // number, or `! `.
        let mut sections: Vec<String> = Vec::new();
        for line in text.split_inclusive('\n') {
            if line.starts_with("== ") {
                sections.push(String::new());
            } else if let Some(s) = sections.last_mut() {
                s.push_str(line);
            }
        }
        let mut sections = sections.into_iter();
        for p in run {
            out.push((p.clone(), sections.next().unwrap_or_default()));
        }
    }
    // In the order asked for.
    files
        .iter()
        .map(|p| {
            out.iter()
                .find(|(q, _)| q == p)
                .cloned()
                .unwrap_or((p.clone(), String::new()))
        })
        .collect()
}

/// What a run of MeadowBoot answers depends on: the executable, the command,
/// the files, and -- for a file of a package -- that package's sources,
/// manifest and lock, and those of every package it names by path; the
/// standard library; and what the Rust compiler's macros expanded to. A
/// package a lock pins by git is its lock line, since a pinned commit is what
/// it is.
fn run_key(exe: &Path, args: &[String], files: &[PathBuf]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    if let Ok(m) = std::fs::metadata(exe) {
        feed(&m.len().to_le_bytes());
        if let Some(t) = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        {
            feed(&t.as_nanos().to_le_bytes());
        }
    }
    for a in args {
        feed(a.as_bytes());
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for f in files {
        // A file of the repository by where it is; one written to a scratch
        // directory, whose name changes from run to run, by its own name.
        match f.strip_prefix(repo()) {
            Ok(within) => feed(within.display().to_string().as_bytes()),
            Err(_) => feed(f.file_name().unwrap_or_default().as_encoded_bytes()),
        }
        feed(&std::fs::read(f).unwrap_or_default());
        if let Some(root) = package_of(f) {
            package_inputs(&root, &mut seen);
        }
    }
    let mut inputs = seen.clone();
    inputs.push(repo().join("lib").join("Std"));
    inputs.push(expansions_dir());
    for dir in inputs {
        let mut every = Vec::new();
        sources_under(&dir, &mut every);
        every.sort();
        for p in every {
            feed(p.display().to_string().as_bytes());
            feed(&std::fs::read(&p).unwrap_or_default());
        }
    }
    h
}

/// `root`, and every package it names by path, each once.
fn package_inputs(root: &Path, seen: &mut Vec<PathBuf>) {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if seen.contains(&root) {
        return;
    }
    seen.push(root.clone());
    let manifest = std::fs::read_to_string(root.join("Meadow.toml")).unwrap_or_default();
    for line in manifest.lines() {
        if let Some(at) = line.find("path = \"") {
            let rest = &line[at + "path = \"".len()..];
            if let Some(end) = rest.find('"') {
                package_inputs(&root.join(&rest[..end]), seen);
            }
        }
    }
}

/// Every file under `dir` a run reads: sources, manifests, locks and the
/// expansions' JSON -- but nothing a build wrote.
fn sources_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for p in entries.flatten().map(|e| e.path()) {
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "target" && n != ".git") {
                sources_under(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "mw" || e == "json")
            || p.file_name()
                .is_some_and(|n| n == "Meadow.toml" || n == "Meadow.lock" || n == "meadow.lock")
        {
            out.push(p);
        }
    }
}

/// The files whose two texts differ, each at its first differing line.
fn differences(
    files: &[(PathBuf, String)],
    mut reference: impl FnMut(&Path) -> String,
) -> Vec<String> {
    let mut bad = Vec::new();
    for (path, theirs) in files {
        let ours = reference(path);
        if &ours == theirs {
            continue;
        }
        let (a, b): (Vec<&str>, Vec<&str>) = (ours.lines().collect(), theirs.lines().collect());
        let at = a
            .iter()
            .zip(&b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        let show = |ls: &[&str]| {
            ls.get(at.saturating_sub(2)..(at + 3).min(ls.len()))
                .unwrap_or(&[])
                .join("\n    ")
        };
        bad.push(format!(
            "{}: first difference at line {}\n  Rust:\n    {}\n  MeadowBoot:\n    {}",
            path.display(),
            at + 1,
            show(&a),
            show(&b)
        ));
    }
    bad
}

// --- lexing --------------------------------------------------------------------

/// `text`'s tokens as `meadow-lexer` reads them, written as MeadowBoot's `lex`
/// writes its own: `from to Kind payload` a line, then `! from to message`.
fn lexed(text: &str) -> String {
    let source = Source::new(SourceKind::Interactive, text.into());
    let result = tokenize(source);
    let mut out = String::new();
    for t in &result.tokens {
        let (from, to) = (t.span.start as usize, t.span.end as usize);
        out.push_str(&format!(
            "{from} {to} {}\n",
            described(t.value(), &text[from..to])
        ));
    }
    for d in &result.errors {
        let span = d.label.1;
        out.push_str(&format!(
            "! {} {} {}\n",
            span.start,
            span.end,
            escape(&d.msg)
        ));
    }
    out
}

fn described(t: &Token, text: &str) -> String {
    match t {
        Token::Int(n) => format!("Int {n}"),
        // Its text: what it is as a number is the parser's to check.
        Token::Real(_) => format!("Real {text}"),
        Token::String(s) => format!("String {}", escape(s)),
        Token::InterpStart(s) => format!("InterpStart {}", escape(s)),
        Token::InterpMid(s) => format!("InterpMid {}", escape(s)),
        Token::InterpEnd(s) => format!("InterpEnd {}", escape(s)),
        Token::Char(c) => format!("Char {}", *c as u32),
        Token::LowerIdent(s) => format!("LowerIdent {s}"),
        Token::UpperIdent(s) => format!("UpperIdent {s}"),
        Token::OpIdent(s) => format!("OpIdent {s}"),
        Token::ConOpIdent(s) => format!("ConOpIdent {s}"),
        other => format!("{other:?}"),
    }
}

/// Every control byte and backslash as `\xNN`, as MeadowBoot's `escape` has it.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let n = c as u32;
        if n < 32 || n == 127 || c == '\\' {
            out.push_str(&format!("\\x{n:02X}"));
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn every_source_lexes_as_meadow_lexer_lexes_it() {
    let files = sources(&["lib", "examples", "benches", "bootstrap", "glade", "silo"]);
    assert!(files.len() > 50, "only {} sources found", files.len());
    let theirs = per_file("lex", &files);
    let bad = differences(&theirs, |p| {
        lexed(&std::fs::read_to_string(p).unwrap_or_default())
    });
    assert!(
        bad.is_empty(),
        "{} of {} files lex differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

// --- parsing -------------------------------------------------------------------

use meadow_compiler::ast::{
    Attr, Bind, Decl, EffectRow, Expr, Field, HandlerArm, Ident, LDecl, LExpr, LPat, LType,
    Lit as ALit, MacCall, Meta, Pat as APat, TypeExpr, VariantFields,
};
use meadow_compiler::lexer::tt::{Delim, Group, TokenTree};
use meadow_compiler::span::Span;

/// `text`'s tree as `meadow-parser` reads it, written as MeadowBoot's `parse`
/// writes its own: a node a line, its children indented under it, each with
/// the bytes it spans -- or `parse failed`, when it does not parse.
fn parsed(text: &str) -> String {
    let source = Source::new(SourceKind::Interactive, text.into());
    let lexed = tokenize(source);
    let (ast, _) = meadow_compiler::parser::parse("boot".into(), source, &lexed.tokens);
    match ast {
        None => "parse failed\n".to_string(),
        Some(m) => {
            let mut w = Tree {
                out: String::new(),
                depth: 0,
                text,
                fixities: None,
                chains: Vec::new(),
                grouping: Vec::new(),
            };
            w.node("Module", m.span, |w| {
                for d in &m.value.decls {
                    w.decl(d);
                }
            });
            w.out
        }
    }
}

/// A tree being written: a line a node, indented two spaces a level.
struct Tree<'t> {
    out: String,
    depth: usize,
    /// The source, for what a float literal says.
    text: &'t str,
    /// The unit's fixities, when each chain of operators is also grouped by
    /// them, as `meadow-rename` groups it.
    fixities: Option<&'t HashMap<InternedString, hir::Fixity>>,
    /// Each chain so grouped: where its first operator is, and it bracketed.
    chains: Vec<(u32, String)>,
    /// What grouping them found wrong, and where.
    grouping: Vec<(String, Span)>,
}

/// A chain of operators, grouped: an operand, or an operator applied to two.
enum Grouped<'a> {
    Leaf(&'a LExpr),
    Bin(Ident, Span, Box<Grouped<'a>>, Box<Grouped<'a>>),
}

impl Grouped<'_> {
    fn span(&self) -> Span {
        match self {
            Grouped::Leaf(e) => e.span,
            Grouped::Bin(_, s, _, _) => *s,
        }
    }

    /// Bracketed as MeadowBoot's `Fixity.chainsOf` writes it: each operator
    /// with where it is, each operand `_`.
    fn bracketed(&self) -> String {
        match self {
            Grouped::Leaf(_) => "_".to_string(),
            Grouped::Bin(op, _, l, r) => format!(
                "({} {}@{} {})",
                l.bracketed(),
                op.value(),
                at(op.span),
                r.bracketed()
            ),
        }
    }
}

fn at(s: Span) -> String {
    format!("{}..{}", s.start, s.end)
}

impl Tree<'_> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn under(&mut self, head: &str, f: impl FnOnce(&mut Self)) {
        self.line(head);
        self.depth += 1;
        f(self);
        self.depth -= 1;
    }

    fn node(&mut self, tag: &str, span: Span, f: impl FnOnce(&mut Self)) {
        self.under(&format!("{tag} {}", at(span)), f);
    }

    fn ident(&mut self, i: &Ident) {
        self.line(&format!("ident {} {}", escape(i.value()), at(i.span)));
    }

    fn idents(&mut self, name: &str, is: &[Ident]) {
        self.under(&format!("{name} {}", is.len()), |w| {
            for i in is {
                w.ident(i);
            }
        });
    }

    fn opt<T>(&mut self, name: &str, x: Option<&T>, f: impl FnOnce(&mut Self, &T)) {
        match x {
            None => self.line(&format!("{name} none")),
            Some(x) => self.under(name, |w| f(w, x)),
        }
    }

    fn list<T>(&mut self, name: &str, xs: &[T], mut f: impl FnMut(&mut Self, &T)) {
        self.under(&format!("{name} {}", xs.len()), |w| {
            for x in xs {
                f(w, x);
            }
        });
    }

    fn decl(&mut self, d: &LDecl) {
        let s = d.span;
        match d.value() {
            Decl::Bind(b) => self.node("Bind", s, |w| w.bind(b)),
            Decl::Mod(n) => self.node("Mod", s, |w| w.ident(n)),
            Decl::Module(n, decls) => self.node("Module", s, |w| {
                w.ident(n);
                w.list("decls", decls, |w, d| w.decl(d));
            }),
            Decl::Use(u) => self.node("Use", s, |w| {
                w.idents("path", &u.path);
                w.line(&format!("glob {}", u.glob));
                w.opt("alias", u.alias.as_ref(), |w, a| w.ident(a));
                w.idents("names", &u.names);
                w.idents("macros", &u.macros);
            }),
            Decl::Data(d) => self.node("Data", s, |w| {
                w.ident(&d.name);
                w.idents("params", &d.params);
                w.list("variants", &d.variants, |w, v| {
                    w.under("Variant", |w| {
                        w.list("attrs", &v.attrs, |w, a| w.attr(a));
                        w.ident(&v.name);
                        match &v.fields {
                            VariantFields::Positional(ts) => {
                                w.list("positional", ts, |w, t| w.ty(t))
                            }
                            VariantFields::Named(fs) => w.list("named", fs, |w, f| w.field(f)),
                        }
                    })
                });
            }),
            Decl::Record(r) => self.node("Record", s, |w| {
                w.ident(&r.name);
                w.idents("params", &r.params);
                w.list("fields", &r.fields, |w, f| w.field(f));
            }),
            Decl::Effect(e) => self.node("Effect", s, |w| {
                w.ident(&e.name);
                w.idents("params", &e.params);
                w.list("ops", &e.ops, |w, f| w.field(f));
            }),
            Decl::TypeAlias(t) => self.node("TypeAlias", s, |w| {
                w.ident(&t.name);
                w.idents("params", &t.params);
                w.ty(&t.ty);
            }),
            Decl::EffectAlias(e) => self.node("EffectAlias", s, |w| {
                w.ident(&e.name);
                w.idents("params", &e.params);
                w.row(&e.row);
            }),
            Decl::Sig(n, t, bounds) => self.node("Sig", s, |w| {
                w.ident(n);
                w.ty(t);
                w.list("bounds", bounds, |w, b| w.bound(b));
            }),
            Decl::Trait(t) => self.node("Trait", s, |w| {
                w.ident(&t.name);
                w.idents("params", &t.params);
                w.list("supers", &t.supers, |w, b| w.bound(b));
                w.list("assocs", &t.assocs, |w, (n, ps)| {
                    w.under("Assoc", |w| {
                        w.ident(n);
                        w.idents("params", ps);
                    })
                });
                w.list("effects", &t.effects, |w, (n, ps)| {
                    w.under("Effect", |w| {
                        w.ident(n);
                        w.idents("params", ps);
                    })
                });
                w.list("sigs", &t.sigs, |w, (n, ty)| {
                    w.under("Sig", |w| {
                        w.ident(n);
                        w.ty(ty);
                    })
                });
                w.list("defaults", &t.defaults, |w, b| {
                    w.under("Default", |w| w.bind(b))
                });
            }),
            Decl::Impl(i) => self.node("Impl", s, |w| {
                w.ident(&i.tr);
                w.list("tys", &i.tys, |w, t| w.ty(t));
                w.list("context", &i.context, |w, b| w.bound(b));
                w.list("assocs", &i.assocs, |w, (n, args, ty)| {
                    w.under("Assoc", |w| {
                        w.ident(n);
                        w.list("args", args, |w, t| w.ty(t));
                        w.ty(ty);
                    })
                });
                w.list("effects", &i.effects, |w, (n, args, is)| {
                    w.under("Effect", |w| {
                        w.ident(n);
                        w.list("args", args, |w, t| w.ty(t));
                        w.row(is);
                    })
                });
                w.list("methods", &i.methods, |w, b| {
                    w.under("Method", |w| w.bind(b))
                });
            }),
            Decl::Fixity(assoc, n, ops) => self.node(&format!("Fixity {assoc:?} {n}"), s, |w| {
                w.idents("ops", ops)
            }),
            Decl::Attributed(attrs, d) => self.node("Attributed", s, |w| {
                w.list("attrs", attrs, |w, a| w.attr(a));
                w.decl(d);
            }),
            Decl::MacCall(m) => self.node("MacCall", s, |w| w.mac(m)),
            Decl::Macro(m) => self.node("Macro", s, |w| {
                w.ident(&m.name);
                w.list("rules", &m.rules, |w, r| {
                    w.under("Rule", |w| {
                        w.group(&r.matcher);
                        w.group(&r.template);
                    })
                });
            }),
        }
    }

    fn field(&mut self, f: &Field) {
        self.under("Field", |w| {
            w.list("attrs", &f.attrs, |w, a| w.attr(a));
            w.ident(&f.name);
            w.ty(&f.ty);
        });
    }

    fn attr(&mut self, a: &Attr) {
        self.under("Attr", |w| {
            w.ident(&a.name);
            w.idents("args", &a.args);
            w.list("meta", &a.meta, |w, m| w.meta(m));
        });
    }

    fn meta(&mut self, m: &Meta) {
        match m {
            Meta::Word(n) => self.under("Word", |w| w.ident(n)),
            Meta::Text(n) => self.under("Text", |w| w.ident(n)),
            Meta::Value(n, v) => self.under("Value", |w| {
                w.ident(n);
                w.ident(v);
            }),
            Meta::List(n, ms) => self.under("List", |w| {
                w.ident(n);
                w.list("meta", ms, |w, m| w.meta(m));
            }),
        }
    }

    fn bound(&mut self, b: &meadow_compiler::ast::Bound) {
        self.under("Bound", |w| {
            w.ident(&b.tr);
            w.list("tys", &b.tys, |w, t| w.ty(t));
        });
    }

    fn bind(&mut self, b: &Bind) {
        match b {
            Bind::Pat(p, e) => self.under("PatBind", |w| {
                w.pat(p);
                w.expr(e);
            }),
            Bind::Fun(n, ps, ret, body) => self.under("FunBind", |w| {
                w.ident(n);
                w.list("params", ps, |w, p| w.pat(p));
                w.opt("ret", ret.as_ref(), |w, t| w.ty(t));
                w.expr(body);
            }),
        }
    }

    fn ty(&mut self, t: &LType) {
        let s = t.span;
        match t.value() {
            TypeExpr::Var(n) => self.node("TVar", s, |w| w.ident(n)),
            TypeExpr::Con(n, args) => self.node("TCon", s, |w| {
                w.ident(n);
                w.list("args", args, |w, t| w.ty(t));
            }),
            TypeExpr::Fun(params, ret, eff) => self.node("TFun", s, |w| {
                w.list("params", params, |w, t| w.ty(t));
                w.ty(ret);
                w.opt("effect", eff.as_ref(), |w, e| w.row(e));
            }),
            TypeExpr::Tuple(ts) => self.node("TTuple", s, |w| w.list("items", ts, |w, t| w.ty(t))),
            TypeExpr::Vector(t) => self.node("TVector", s, |w| w.ty(t)),
            TypeExpr::List(t) => self.node("TList", s, |w| w.ty(t)),
            TypeExpr::Row(r) => self.node("TRow", s, |w| w.row(r)),
            TypeExpr::Record(fields, tail) => self.node("TRecord", s, |w| {
                w.list("fields", fields, |w, (n, t)| {
                    w.under("TField", |w| {
                        w.ident(n);
                        w.ty(t);
                    })
                });
                w.opt("tail", tail.as_ref(), |w, r| w.ident(r));
            }),
        }
    }

    fn row(&mut self, e: &EffectRow) {
        self.under("Row", |w| {
            w.list("labels", &e.labels, |w, (n, args)| {
                w.under("Label", |w| {
                    w.ident(n);
                    w.list("args", args, |w, t| w.ty(t));
                })
            });
            w.opt("tail", e.tail.as_ref(), |w, r| w.ident(r));
        });
    }

    fn lit(&mut self, l: &ALit, span: Span) {
        let s = match l {
            ALit::Int(i) => format!("Int {i}"),
            // What the literal says, and its sign: its value, less a parse.
            ALit::Float(_) => {
                let text = &self.text[span.start as usize..span.end as usize];
                let minuses = text.chars().filter(|c| *c == '-').count()
                    - text
                        .trim_start_matches(|c: char| c == '-' || c.is_whitespace())
                        .chars()
                        .filter(|c| *c == '-')
                        .count();
                let digits = text.trim_start_matches(|c: char| c == '-' || c.is_whitespace());
                format!("Float {}{digits}", if minuses % 2 == 1 { "-" } else { "" })
            }
            ALit::String(s) => format!("String {}", escape(s)),
            ALit::Char(c) => format!("Char {}", *c as u32),
        };
        self.line(&s);
    }

    fn pat(&mut self, p: &LPat) {
        let s = p.span;
        match p.value() {
            APat::Wildcard => self.node("PWild", s, |_| {}),
            APat::Var(n) => self.node("PVar", s, |w| w.ident(n)),
            APat::Ann(inner, t) => self.node("PAnn", s, |w| {
                w.pat(inner);
                w.ty(t);
            }),
            APat::Lit(l) => self.node("PLit", s, |w| w.lit(l, s)),
            APat::As(n, inner) => self.node("PAs", s, |w| {
                w.ident(n);
                w.pat(inner);
            }),
            APat::Cons(n, ps) => self.node("PCons", s, |w| {
                w.ident(n);
                w.list("args", ps, |w, p| w.pat(p));
            }),
            APat::QualCons(q, n, ps) => self.node("PQualCons", s, |w| {
                w.ident(q);
                w.ident(n);
                w.list("args", ps, |w, p| w.pat(p));
            }),
            APat::Tuple(ps) => self.node("PTuple", s, |w| w.list("items", ps, |w, p| w.pat(p))),
            APat::Array(ps) => self.node("PArray", s, |w| w.list("items", ps, |w, p| w.pat(p))),
            APat::Vector(ps) => self.node("PVector", s, |w| w.list("items", ps, |w, p| w.pat(p))),
            APat::List(ps) => self.node("PList", s, |w| w.list("items", ps, |w, p| w.pat(p))),
            APat::Record(fields, open) => self.node(&format!("PRecord {open}"), s, |w| {
                w.list("fields", fields, |w, (n, p)| {
                    w.under("PField", |w| {
                        w.ident(n);
                        w.pat(p);
                    })
                })
            }),
            APat::Unit => self.node("PUnit", s, |_| {}),
            APat::View(e, p) => self.node("PView", s, |w| {
                w.expr(e);
                w.pat(p);
            }),
            APat::MacCall(m) => self.node("PMacCall", s, |w| w.mac(m)),
        }
    }

    fn expr(&mut self, e: &LExpr) {
        let s = e.span;
        match e.value() {
            Expr::Var(n) => self.node("Var", s, |w| w.ident(n)),
            Expr::Lit(l) => self.node("Lit", s, |w| w.lit(l, s)),
            Expr::Interp(texts, holes) => self.node("Interp", s, |w| {
                w.list("texts", texts, |w, t| {
                    w.line(&format!("text {}", escape(t)))
                });
                w.list("holes", holes, |w, (e, fmt)| {
                    w.under(&format!("Hole {fmt:?}"), |w| w.expr(e))
                });
            }),
            Expr::Lam(ps, body) => self.node("Lam", s, |w| {
                w.list("params", ps, |w, p| w.pat(p));
                w.expr(body);
            }),
            Expr::App(f, args) => self.node("App", s, |w| {
                w.expr(f);
                w.list("args", args, |w, a| w.expr(a));
            }),
            Expr::Let(binds, body) => self.node("Let", s, |w| {
                w.list("binds", binds, |w, b| w.bind(b));
                w.expr(body);
            }),
            Expr::If(c, a, b) => self.node("If", s, |w| {
                w.expr(c);
                w.expr(a);
                w.expr(b);
            }),
            Expr::Match(scrut, arms) => self.node("Match", s, |w| {
                w.expr(scrut);
                w.list("arms", arms, |w, (p, g, b)| {
                    w.under("Arm", |w| {
                        w.pat(p);
                        w.opt("guard", g.as_ref(), |w, g| w.expr(g));
                        w.expr(b);
                    })
                });
            }),
            Expr::UnOp(op, x) => self.node("UnOp", s, |w| {
                w.line(&format!("op {} {}", op.value().to_string(), at(op.span)));
                w.expr(x);
            }),
            Expr::BinOp(op, a, b) => self.node("BinOp", s, |w| {
                w.line(&format!("op {} {}", op.value().to_string(), at(op.span)));
                w.expr(a);
                w.expr(b);
            }),
            Expr::Infix(first, rest) => {
                // Grouped too, when the fixities are given -- and written as
                // parsed all the same, for the chains inside its operands.
                if let Some(table) = self.fixities {
                    let mut errors = Vec::new();
                    let grouped = meadow_compiler::rename::reassociate(
                        Grouped::Leaf(first),
                        rest.iter().map(|(op, e)| (op.clone(), Grouped::Leaf(e))),
                        |op| fixity_of(table, op),
                        |op, l, r| {
                            let span = l.span().extend(r.span());
                            Grouped::Bin(op, span, Box::new(l), Box::new(r))
                        },
                        |msg, span| errors.push((msg, span)),
                    );
                    self.grouping.extend(errors);
                    // A chain the parser made up -- `[a .. b]` is `range a (b +
                    // 1)` -- was not written, and has no grouping to compare.
                    let written = rest.iter().all(|(op, _)| {
                        self.text.get(op.span.start as usize..op.span.end as usize)
                            == Some(&**op.value())
                    });
                    if written {
                        let first_op = rest.first().map_or(0, |(op, _)| op.span.start);
                        self.chains.push((first_op, grouped.bracketed()));
                    }
                }
                self.node("Infix", s, |w| {
                    w.expr(first);
                    w.list("rest", rest, |w, (op, x)| {
                        w.under("Op", |w| {
                            w.ident(op);
                            w.expr(x);
                        })
                    });
                });
            }
            Expr::Tuple(xs) => self.node("Tuple", s, |w| w.list("items", xs, |w, x| w.expr(x))),
            Expr::Array(xs) => self.node("Array", s, |w| w.list("items", xs, |w, x| w.expr(x))),
            Expr::List(xs) => self.node("List", s, |w| w.list("items", xs, |w, x| w.expr(x))),
            Expr::Cons(n, xs) => self.node("Cons", s, |w| {
                w.ident(n);
                w.list("args", xs, |w, x| w.expr(x));
            }),
            Expr::Qual(q, n) => self.node("Qual", s, |w| {
                w.ident(q);
                w.ident(n);
            }),
            Expr::Record(fields, base) => self.node("RecordE", s, |w| {
                w.list("fields", fields, |w, (n, x)| {
                    w.under("FieldE", |w| {
                        w.ident(n);
                        w.expr(x);
                    })
                });
                w.opt("base", base.as_ref(), |w, b| w.expr(b));
            }),
            Expr::Update(base, fields) => self.node("Update", s, |w| {
                w.expr(base);
                w.list("fields", fields, |w, (n, x)| {
                    w.under("FieldE", |w| {
                        w.ident(n);
                        w.expr(x);
                    })
                });
            }),
            Expr::Field(x, n) => self.node("Field", s, |w| {
                w.expr(x);
                w.ident(n);
            }),
            Expr::Handle(x, arms, ret) => self.node("Handle", s, |w| {
                w.expr(x);
                w.list("arms", arms, |w, a| w.harm(a));
                w.opt("ret", ret.as_ref(), |w, (p, b)| {
                    w.pat(p);
                    w.expr(b);
                });
            }),
            Expr::Unit => self.node("Unit", s, |_| {}),
            Expr::Hole => self.node("Hole", s, |_| {}),
            Expr::MacCall(m) => self.node("MacCallE", s, |w| w.mac(m)),
        }
    }

    fn harm(&mut self, a: &HandlerArm) {
        self.under("HArm", |w| {
            w.ident(&a.op);
            w.pat(&a.param);
            w.ident(&a.resume);
            w.expr(&a.body);
        });
    }

    fn mac(&mut self, m: &MacCall) {
        self.idents("path", &m.path);
        self.group(&m.arg);
    }

    fn group(&mut self, g: &Group) {
        let delim = match g.delim {
            Delim::Paren => "Paren",
            Delim::Brack => "Brack",
            Delim::Brace => "Brace",
        };
        self.under(
            &format!("Group {delim} {} {}", at(g.open), at(g.close)),
            |w| {
                for t in &g.trees {
                    match t {
                        TokenTree::Group(g) => w.group(g),
                        TokenTree::Token(t) => {
                            let (from, to) = (t.span.start as usize, t.span.end as usize);
                            let text = &w.text[from..to];
                            w.line(&format!(
                                "tok {} {}",
                                described(t.value(), text),
                                at(t.span)
                            ));
                        }
                    }
                }
            },
        );
    }
}

#[test]
fn every_source_parses_as_meadow_parser_parses_it() {
    let files = sources(&["lib", "examples", "benches", "bootstrap", "glade", "silo"]);
    assert!(files.len() > 50, "only {} sources found", files.len());
    let theirs = per_file("parse", &files);
    let bad = differences(&theirs, |p| {
        parsed(&std::fs::read_to_string(p).expect("a source"))
    });
    assert!(
        bad.is_empty(),
        "{} of {} files parse differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

// --- grouping operators by fixity ----------------------------------------------

use meadow_compiler::hir;
use meadow_compiler::intern::InternedString;
use std::collections::HashMap;

/// How `op` binds, as the resolver says: the language's own operators as they
/// always do, one the unit declared as declared, and anything else `infixl 9`.
fn fixity_of(table: &HashMap<InternedString, hir::Fixity>, op: InternedString) -> hir::Fixity {
    let op = meadow_compiler::ast::hygiene::strip(op);
    hir::builtin_operator(&op)
        .map(|(f, _)| f)
        .or_else(|| table.get(&op).copied())
        .unwrap_or(hir::Fixity::DEFAULT)
}

/// The package a file is in: the nearest directory above it with a
/// `Meadow.toml`.
fn package_of(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|d| d.join("Meadow.toml").is_file())
        .map(Path::to_path_buf)
}

/// The fixities the package at `root` declares, over `known`, as
/// `Resolver::declare_fixities` takes them in: a level past 9 and a language
/// operator are passed over, and the first declaration of an operator stands.
fn declared_in(
    known: &HashMap<InternedString, hir::Fixity>,
    root: &Path,
) -> HashMap<InternedString, hir::Fixity> {
    let mut table = known.clone();
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target") {
                    walk(&p, out);
                }
            } else if p.extension().is_some_and(|e| e == "mw") {
                out.push(p);
            }
        }
    }
    walk(&root.join("src"), &mut files);
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let source = Source::new(SourceKind::Interactive, text.as_str().into());
        let lexed = tokenize(source);
        let (ast, _) = meadow_compiler::parser::parse("boot".into(), source, &lexed.tokens);
        let Some(m) = ast else {
            continue;
        };
        for d in &m.value.decls {
            let mut d = d.value();
            while let Decl::Attributed(_, inner) = d {
                d = inner.value();
            }
            let Decl::Fixity(assoc, level, ops) = d else {
                continue;
            };
            if *level > 9 {
                continue;
            }
            let fixity = hir::Fixity {
                assoc: match assoc {
                    meadow_compiler::ast::Assoc::Left => hir::Assoc::Left,
                    meadow_compiler::ast::Assoc::Right => hir::Assoc::Right,
                    meadow_compiler::ast::Assoc::None => hir::Assoc::None,
                },
                level: *level,
            };
            for op in ops {
                if hir::builtin_operator(op.value()).is_none() {
                    table.entry(*op.value()).or_insert(fixity);
                }
            }
        }
    }
    table
}

/// `text`'s chains of operators grouped by `table`, as MeadowBoot's `fixity`
/// writes them: each bracketed, first operator first, then each that could
/// not be grouped.
fn grouped(text: &str, table: &HashMap<InternedString, hir::Fixity>) -> String {
    let source = Source::new(SourceKind::Interactive, text.into());
    let lexed = tokenize(source);
    let (ast, _) = meadow_compiler::parser::parse("boot".into(), source, &lexed.tokens);
    match ast {
        None => "parse failed\n".to_string(),
        Some(m) => {
            let mut w = Tree {
                out: String::new(),
                depth: 0,
                text,
                fixities: Some(table),
                chains: Vec::new(),
                grouping: Vec::new(),
            };
            w.node("Module", m.span, |w| {
                for d in &m.value.decls {
                    w.decl(d);
                }
            });
            let mut out = String::new();
            w.chains.sort_by_key(|c| c.0);
            for (_, chain) in &w.chains {
                out.push_str(&format!(
                    "chain {chain}
"
                ));
            }
            for (msg, span) in w.grouping {
                out.push_str(&format!("fixity error {} {msg}\n", at(span)));
            }
            out
        }
    }
}

#[test]
fn every_source_groups_its_operators_as_meadow_rename_groups_them() {
    let files = sources(&["lib", "examples", "benches", "bootstrap", "glade", "silo"]);
    let std = repo().join("lib").join("Std");
    let theirs = per_file("fixity", &files);
    let lib = declared_in(&HashMap::new(), &std);
    let mut tables: HashMap<PathBuf, HashMap<InternedString, hir::Fixity>> = HashMap::new();
    let bad = differences(&theirs, |p| {
        let root = package_of(p).unwrap_or_default();
        let table = tables
            .entry(root.clone())
            .or_insert_with(|| {
                if root == std {
                    declared_in(&HashMap::new(), &root)
                } else {
                    declared_in(&lib, &root)
                }
            })
            .clone();
        grouped(&std::fs::read_to_string(p).expect("a source"), &table)
    });
    assert!(
        bad.is_empty(),
        "{} of {} files group differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

// --- renaming -------------------------------------------------------------------

use meadow_lsp::analysis::{Mentioned, mentions};

/// A file as a rename dump names it: its package's name and its path in the
/// package, `Std/src/Char.mw` -- the same wherever the package was checked out.
fn file_name(source: &Source, roots: &[(PathBuf, String)]) -> Option<String> {
    let SourceKind::File(name) = source.kind else {
        return None;
    };
    named_file(&name, roots)
}

/// The same, for a module's file as the compiler names it.
fn named_file(name: &str, roots: &[(PathBuf, String)]) -> Option<String> {
    // The standard library's modules are compiled from text built into the
    // compiler, and named for their path under its `src`.
    if let Some(rest) = name.strip_prefix("Std/") {
        return Some(format!("Std/src/{rest}"));
    }
    let path = Path::new(name);
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    roots.iter().find_map(|(root, pkg)| {
        let rel = path.strip_prefix(root).ok()?;
        Some(format!(
            "{pkg}/{}",
            rel.to_string_lossy().replace('\\', "/")
        ))
    })
}

/// Every module of the package at `root` renamed, as MeadowBoot's `rename`
/// writes it: a name written a line, sorted -- `from to kind declared`, where
/// the kind is `v` for a value, `t` for a type and `c` for a constructor, and
/// `declared` is where what it names is declared, `file@offset`, or `-` for a
/// primitive -- keyed by file. The standard library's, when `root` is it.
fn renamed_package(root: &Path) -> HashMap<PathBuf, String> {
    renamed_recording(root, &mut Vec::new())
}

/// The same, with what the macros of every package the build compiled
/// expanded to (`meadow_compiler::expand::record`) added to `expanded`, each
/// with its file as a dump names it.
/// The build of the package at `root` -- of the standard library, when `root`
/// is it -- and the root of every package it compiled with its name, the
/// deepest first: a package inside another's directory is its own.
fn built(root: &Path) -> Option<(meadow::pipeline::CompiledGraph, Vec<(PathBuf, String)>)> {
    // The standard library is compiled for any build: the smallest will do.
    let entry = if is_std(root) {
        let probe = std::env::temp_dir().join("meadowboot-std-probe.mw");
        std::fs::write(&probe, "def result = 1\n").expect("a probe");
        probe
    } else {
        root.to_path_buf()
    };
    let graph = meadow::pipeline::compile_packages(&entry, as_meadowboot_builds()).ok()?;
    let mut roots: Vec<(PathBuf, String)> = graph
        .packages
        .iter()
        .filter_map(|(r, p)| {
            let r = r.canonicalize().ok()?;
            Some((r, p.as_ref()?.name.to_string()))
        })
        .collect();
    roots.sort_by_key(|(r, _)| std::cmp::Reverse(r.components().count()));
    Some((graph, roots))
}

/// Whether `root` is the standard library's directory, however it is written.
fn is_std(root: &Path) -> bool {
    let std_root = repo().join("lib").join("Std");
    root == std_root || root.canonicalize().ok() == std_root.canonicalize().ok()
}

/// The package at `root` among what `graph` compiled: the standard library,
/// when `root` is it.
fn wanted<'g>(
    graph: &'g meadow::pipeline::CompiledGraph,
    root: &Path,
) -> Vec<&'g meadow_compiler::CompiledPackage> {
    if is_std(root) {
        return graph.std.iter().collect();
    }
    let me = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    graph
        .packages
        .iter()
        .filter(|(r, _)| r.canonicalize().is_ok_and(|r| r == me))
        .filter_map(|(_, p)| p.as_ref())
        .collect()
}

/// The path of the file a dump names `file`.
fn path_named(file: &str, roots: &[(PathBuf, String)]) -> Option<PathBuf> {
    match file.strip_prefix("Std/src/") {
        Some(rest) => Some(repo().join("lib").join("Std").join("src").join(rest)),
        None => {
            let (pkg, rel) = file.split_once('/')?;
            roots
                .iter()
                .find(|(_, p)| p == pkg)
                .map(|(r, _)| r.join(rel))
        }
    }
}

fn renamed_recording(
    root: &Path,
    expanded: &mut Vec<(String, meadow_compiler::expand::record::Recorded)>,
) -> HashMap<PathBuf, String> {
    meadow_compiler::expand::record::start();
    let compiled = built(root);
    let recorded = meadow_compiler::expand::record::take();
    let Some((graph, roots)) = compiled else {
        return HashMap::new();
    };
    for r in recorded {
        if let Some(file) = named_file(&r.filename, &roots) {
            expanded.push((file, r));
        }
    }
    let everything: Vec<&meadow_compiler::CompiledPackage> = graph
        .std
        .iter()
        .chain(graph.packages.iter().filter_map(|(_, p)| p.as_ref()))
        .collect();
    let mut out = HashMap::new();
    for pkg in wanted(&graph, root) {
        for (source, found) in mentions(pkg, &everything) {
            let Some(file) = file_name(&source, &roots) else {
                continue;
            };
            let mut lines: Vec<String> = found
                .iter()
                .map(|m| {
                    let kind = match m.what {
                        Mentioned::Value(_) => 'v',
                        Mentioned::Type(_) => 't',
                        Mentioned::Ctor(_) => 'c',
                    };
                    let declared = m
                        .declared
                        .and_then(|d| {
                            Some(format!(
                                "{}@{}",
                                file_name(&d.source, &roots)?,
                                d.span.start
                            ))
                        })
                        .unwrap_or_else(|| "-".to_string());
                    format!("{} {} {kind} {declared}\n", m.span.start, m.span.end)
                })
                .collect();
            lines.sort_by_key(|l| {
                let mut n = l.split(' ').map(|x| x.parse::<usize>().unwrap_or(0));
                (n.next(), n.next(), l.clone())
            });
            lines.dedup();
            let Some(path) = path_named(&file, &roots) else {
                continue;
            };
            out.insert(path, lines.concat());
        }
    }
    out
}

// --- inferring -------------------------------------------------------------------

/// Every module of the package at `root` inferred, as MeadowBoot's `infer`
/// writes it: each name a top-level binding binds, a line, in the order they
/// are written -- `from to name : scheme`, the scheme as `meadow build --types`
/// shows it -- keyed by file. The standard library's, when `root` is it.
fn inferred_package(root: &Path) -> HashMap<PathBuf, String> {
    inferred_recording(root, &mut Vec::new())
}

/// The same, with what the macros of every package the build compiled
/// expanded to added to `expanded`, as [`renamed_recording`] adds them.
fn inferred_recording(
    root: &Path,
    expanded: &mut Vec<(String, meadow_compiler::expand::record::Recorded)>,
) -> HashMap<PathBuf, String> {
    meadow_compiler::expand::record::start();
    let compiled = built(root);
    let recorded = meadow_compiler::expand::record::take();
    let Some((graph, roots)) = compiled else {
        return HashMap::new();
    };
    for r in recorded {
        if let Some(file) = named_file(&r.filename, &roots) {
            expanded.push((file, r));
        }
    }
    let mut out = HashMap::new();
    for pkg in wanted(&graph, root) {
        for m in &pkg.modules {
            let Some(file) = file_name(&m.source, &roots) else {
                continue;
            };
            let Some(path) = path_named(&file, &roots) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let mut named = Vec::new();
            for d in &m.hir.value().decls {
                if let hir::Decl::Bind(b) = d.value() {
                    binders_of(b, &mut named);
                }
            }
            let mut lines: Vec<((u32, u32), String)> = named
                .iter()
                .map(|(var, span)| {
                    let name = text
                        .get(span.start as usize..span.end as usize)
                        .unwrap_or("?");
                    let scheme = pkg
                        .generalized
                        .get(var)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let line = format!("{} {} {name} : {scheme}\n", span.start, span.end);
                    ((span.start, span.end), line)
                })
                .collect();
            // By where each is written; those a macro's call writes alike, by
            // what they say.
            lines.sort();
            out.insert(path, lines.into_iter().map(|(_, l)| l).collect());
        }
    }
    out
}

#[test]
fn every_source_infers_as_meadow_infer_infers_it() {
    let _one_at_a_time = expansions_turn();
    let files = sources(&["lib", "examples", "benches", "bootstrap", "glade", "silo"]);
    // This compiler first, keeping what its macros expanded to, which
    // MeadowBoot is then given -- the standard library's too.
    let rust_started = std::time::Instant::now();
    let mut packages: HashMap<PathBuf, HashMap<PathBuf, String>> = HashMap::new();
    let mut expanded = Vec::new();
    meadow_compiler::expand::record::start();
    let _ = meadow::stdlib::compile_fresh(as_meadowboot_builds());
    for r in meadow_compiler::expand::record::take() {
        if let Some(file) = named_file(&r.filename, &[]) {
            expanded.push((file, r));
        }
    }
    for p in &files {
        let root = package_of(p).unwrap_or_default();
        if !packages.contains_key(&root) {
            let dumps = inferred_recording(&root, &mut expanded);
            packages.insert(root, dumps);
        }
    }
    eprintln!(
        "the Rust compiler: {} packages in {:.0?}",
        packages.len(),
        rust_started.elapsed()
    );
    write_expansions(&expanded);
    let theirs = per_file("infer", &files);
    let bad = differences(&theirs, |p| {
        let root = package_of(p).unwrap_or_default();
        let dumps = &packages[&root];
        let me = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        dumps
            .iter()
            .find(|(f, _)| f.canonicalize().is_ok_and(|f| f == me))
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| "not compiled\n".to_string())
    });
    assert!(
        bad.is_empty(),
        "{} of {} files infer differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// Each name binding `b` binds, and where it is written.
fn binders_of(b: &hir::Bind, out: &mut Vec<(hir::VarId, Span)>) {
    fn pat(p: &hir::LPat, out: &mut Vec<(hir::VarId, Span)>) {
        match p.value() {
            hir::Pat::Var(id) => out.push((*id.value(), id.span)),
            hir::Pat::As(id, sub) => {
                out.push((*id.value(), id.span));
                pat(sub, out);
            }
            hir::Pat::Ann(inner, _) => pat(inner, out),
            hir::Pat::View(_, inner) => pat(inner, out),
            hir::Pat::Tuple(ps)
            | hir::Pat::List(ps)
            | hir::Pat::Array(ps)
            | hir::Pat::Cons(_, ps) => ps.iter().for_each(|q| pat(q, out)),
            hir::Pat::Record(fields, _) => fields.iter().for_each(|(_, q)| pat(q, out)),
            _ => {}
        }
    }
    match b {
        hir::Bind::Fun(name, ..) => out.push((*name.value(), name.span)),
        hir::Bind::Pat(p, _) => pat(p, out),
        hir::Bind::Error => {}
    }
}

/// Write MeadowBoot's table of the primitives' types, `src/Prims.mw`, from
/// this compiler's (`meadow_infer::primitive_scheme`): run by hand, `cargo test
/// --test bootstrap write_prims -- --ignored`, when a primitive changes. Each
/// entry carries the scheme as this compiler writes it, which MeadowBoot's
/// test of the table holds its own writing to.
#[test]
#[ignore]
fn write_prims() {
    use meadow_compiler::infer::{Scheme, Type, VarKind, primitive_scheme};
    fn kind(k: &VarKind) -> &'static str {
        match k {
            VarKind::Type => "Plain",
            VarKind::Row => "Row",
            VarKind::Effect => "Effect",
            VarKind::Num => "Num",
            VarKind::Frac => "Frac",
        }
    }
    fn ty(t: &Type) -> String {
        let each = |ts: &[Type]| ts.iter().map(ty).collect::<Vec<_>>().join(", ");
        match t {
            Type::Var(i) => format!("(Var {i})"),
            Type::Bound(i) => format!("(Bound {i})"),
            Type::Con(n, args) => format!("(Con {:?} [{}])", n.to_string(), each(args)),
            Type::Fun(ps, r, e) => format!("(Fun [{}] {} {})", each(ps), ty(r), ty(e)),
            Type::Tuple(ts) => format!("(Tuple [{}])", each(ts)),
            Type::Record(r) => format!("(Record {})", ty(r)),
            Type::RowEmpty => "RowEmpty".to_string(),
            Type::RowExtend(l, f, r) => {
                format!("(RowExtend {:?} {} {})", l.to_string(), ty(f), ty(r))
            }
            Type::Error => "Error".to_string(),
        }
    }
    fn scheme(s: &Scheme) -> String {
        let quant = s.quant.iter().map(kind).collect::<Vec<_>>().join(", ");
        let preds = s
            .preds
            .iter()
            .map(|p| {
                let tys = p.tys.iter().map(ty).collect::<Vec<_>>().join(", ");
                let assocs = p.assocs.iter().map(ty).collect::<Vec<_>>().join(", ");
                format!(
                    "Pred {{ tr = {:?}, tys = [{tys}], assocs = [{assocs}] }}",
                    p.tr.to_string()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let lacks = s
            .lacks
            .iter()
            .map(|(i, ls)| {
                let ls = ls
                    .iter()
                    .map(|l| format!("{:?}", l.to_string()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Lacking {{ at = {i}, labels = [{ls}] }}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Scheme {{ quant = [{quant}], preds = [{preds}], ty = {}, lacks = [{lacks}] }}",
            ty(&s.ty)
        )
    }
    let mut out = String::from(
        "-- Prims: the type of every primitive, as `meadow_infer` has it -- its\n\
         -- effects opened, as a use of it sees them -- by name, in the order the\n\
         -- resolver hands out their binders (`meadow_hir::PRIMS`).\n\
         --\n\
         -- Written by `write_prims` in `buildtools/meadow/tests/bootstrap.rs`, from\n\
         -- the Rust compiler's table; not by hand. Each entry keeps the scheme as the\n\
         -- Rust compiler writes it, which the test below holds `showScheme` to.\n\n\
         use MeadowBoot.Types\n\
         use MeadowBoot.Types.Type.*\n\
         use MeadowBoot.Types.VarKind.*\n\
         use Std.Collections.Vector as V\n\
         use Std.Maybe.Maybe.*\n\n\
         -- Each primitive, its scheme -- if it has one -- and that written out: a\n\
         -- primitive to a line, which is what the formatter is told to leave.\n\
         @fmt(skip)\n\
         @pub def prims : [(String, Maybe Scheme, String)] =\n  [ ",
    );
    let entries: Vec<String> = meadow_compiler::hir::PRIMS
        .iter()
        .map(|name| match primitive_scheme(name) {
            Some(s) => format!("({name:?}, Just ({}), {:?})", scheme(&s), s.to_string()),
            None => format!("({name:?}, None, \"\")"),
        })
        .collect();
    out.push_str(&entries.join(",\n    "));
    out.push_str(
        " ]\n\n\
         -- The scheme of the primitive `name`, if it has one.\n\
         @pub fun primScheme (name : String) : Maybe Scheme =\n  \
         match V.find (\\x -> match x with | (n, _, _) -> n == name) prims with\n  \
         | Just (_, s, _) -> s\n  \
         | None -> None\n\n",
    );
    // What each is in core -- `meadow_rt::Prim`, by the name `core_text`
    // writes it with -- and how many arguments it takes at once: what
    // lowering to core makes of a mention of one.
    out.push_str(
        "-- Each primitive that is an operation of core: its name there, and how many\n\
         -- arguments it takes at once.\n\
         @fmt(skip)\n\
         @pub def primOps : [(String, String, Int)] =\n  [ ",
    );
    let ops: Vec<String> = meadow_compiler::hir::PRIMS
        .iter()
        .filter_map(|name| {
            let p = meadow_compiler::core::Prim::from_name(name)?;
            Some(format!("({name:?}, {:?}, {})", format!("{p:?}"), p.arity()))
        })
        .collect();
    out.push_str(&ops.join(",\n    "));
    out.push_str(
        " ]\n\n\
         -- --- tests ---------------------------------------------------------------------------\n\n\
         use Std.Test (assertEq)\n\n\
         @test fun everyPrimitiveIsWrittenAsTheRustCompilerWritesIt () =\n  \
         let wrong = V.concatMap (\\x -> match x with | (n, Just s, shown) -> (if showScheme s == shown then [] else [\"${n}: ${showScheme s}, not ${shown}\"]) | _ -> []) prims in\n  \
         assertEq wrong [] \"each as it is written there\"\n",
    );
    std::fs::write(repo().join("bootstrap/src/Prims.mw"), out).expect("the table");
}

/// What this compiler's `infer` dump of the file `MEADOWBOOT_INFER` is -- or,
/// for a package's directory, of each of its files, under a line naming it:
/// run by hand, `cargo test --test bootstrap infer_dump -- --ignored
/// --nocapture`, to see what MeadowBoot is to write.
#[test]
#[ignore]
fn infer_dump() {
    let Some(file) = std::env::var_os("MEADOWBOOT_INFER").map(PathBuf::from) else {
        return;
    };
    if file.is_dir() {
        let mut dumps: Vec<(PathBuf, String)> = inferred_package(&file).into_iter().collect();
        dumps.sort();
        for (f, d) in dumps {
            print!("== {}\n{d}", f.display());
        }
        return;
    }
    let file = file.canonicalize().expect("the file");
    let root = package_of(&file).expect("its package");
    let dumps = inferred_package(&root);
    let found = dumps
        .iter()
        .find(|(f, _)| f.canonicalize().is_ok_and(|f| f == file));
    match found {
        Some((_, d)) => print!("{d}"),
        None => println!("not compiled"),
    }
}

/// The two tests that hand MeadowBoot this compiler's expansions take turns.
///
/// Each turns on the compiler's record of what its macros expanded to, which
/// is one for the whole process, and each empties and fills the one
/// directory MeadowBoot reads them from. Run together, as `cargo test` runs
/// tests unless it is told otherwise, each read what the other had half
/// written: eighteen files renamed differently and twenty-nine inferred
/// differently, in a tree where none do.
fn expansions_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TURN.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where the rename test leaves what this compiler's macros expanded to, for
/// MeadowBoot: a file a package, `Pkg.json`.
fn expansions_dir() -> PathBuf {
    std::env::temp_dir().join("meadowboot-expansions")
}

/// Write `expanded` there, each package's in its file: a JSON array, an
/// expansion an element, `[file, from, to, kind, what]` -- `what` the
/// expansion as `meadow_compiler::expand::record` writes it.
fn write_expansions(expanded: &[(String, meadow_compiler::expand::record::Recorded)]) {
    let dir = expansions_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory for the expansions");
    let mut by_package: HashMap<&str, Vec<String>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for (file, r) in expanded {
        if !seen.insert((file.clone(), r.at.start, r.at.end, r.kind)) {
            continue;
        }
        let pkg = file.split('/').next().unwrap_or_default();
        by_package.entry(pkg).or_default().push(format!(
            "[{:?},{},{},{:?},{}]",
            file, r.at.start, r.at.end, r.kind, r.json
        ));
    }
    for (pkg, entries) in by_package {
        std::fs::write(
            dir.join(format!("{pkg}.json")),
            format!("[{}]", entries.join(",\n")),
        )
        .expect("the expansions");
    }
}

/// What this compiler's `rename` dump of the file `MEADOWBOOT_RENAME` is:
/// run by hand, `cargo test --test bootstrap rename_dump -- --ignored
/// --nocapture`, to see what MeadowBoot is to write.
#[test]
#[ignore]
fn rename_dump() {
    let Some(file) = std::env::var_os("MEADOWBOOT_RENAME").map(PathBuf::from) else {
        return;
    };
    let file = file.canonicalize().expect("the file");
    let root = package_of(&file).expect("its package");
    let dumps = renamed_package(&root);
    let found = dumps
        .iter()
        .find(|(f, _)| f.canonicalize().is_ok_and(|f| f == file));
    match found {
        Some((_, d)) => print!("{d}"),
        None => println!("not compiled"),
    }
}

#[test]
fn every_source_renames_as_meadow_rename_renames_it() {
    let _one_at_a_time = expansions_turn();
    let files = sources(&["lib", "examples", "benches", "bootstrap", "glade", "silo"]);
    // This compiler first, keeping what its macros expanded to, which
    // MeadowBoot is then given.
    let rust_started = std::time::Instant::now();
    let mut packages: HashMap<PathBuf, HashMap<PathBuf, String>> = HashMap::new();
    let mut expanded = Vec::new();
    // The standard library's too, which a build otherwise reads back from a
    // cache without expanding anything.
    meadow_compiler::expand::record::start();
    let _ = meadow::stdlib::compile_fresh(as_meadowboot_builds());
    for r in meadow_compiler::expand::record::take() {
        if let Some(file) = named_file(&r.filename, &[]) {
            expanded.push((file, r));
        }
    }
    for p in &files {
        let root = package_of(p).unwrap_or_default();
        if !packages.contains_key(&root) {
            let dumps = renamed_recording(&root, &mut expanded);
            packages.insert(root, dumps);
        }
    }
    eprintln!(
        "the Rust compiler: {} packages in {:.0?}",
        packages.len(),
        rust_started.elapsed()
    );
    write_expansions(&expanded);
    let theirs = per_file("rename", &files);
    let bad = differences(&theirs, |p| {
        let root = package_of(p).unwrap_or_default();
        let dumps = &packages[&root];
        let me = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        dumps
            .iter()
            .find(|(f, _)| f.canonicalize().is_ok_and(|f| f == me))
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| "not compiled\n".to_string())
    });
    assert!(
        bad.is_empty(),
        "{} of {} files rename differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

// --- evaluating: the CEK machine ------------------------------------------------

/// The programs `glade/tests/differential.rs` checks every back end with: the
/// string literal given to each `agree(…)` there. Read from that file, so a
/// case added there is one here.
fn glade_programs() -> Vec<String> {
    // Read as the compiler reads a Rust file: a checkout on Windows has
    // `\r\n` line endings, and a `\r` left in a case's text -- before the
    // newline a `\` continues a string over, say -- makes it a different
    // program, or none.
    let text = std::fs::read_to_string(repo().join("glade/tests/differential.rs"))
        .expect("the glade cases")
        .replace("\r\n", "\n");
    let mut out = Vec::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("agree(") {
        rest = &rest[at + "agree(".len()..];
        if let Some((lit, after)) = rust_string(rest.trim_start()) {
            out.push(lit);
            rest = after;
        }
    }
    out
}

/// The Rust string literal `s` starts with -- `"…"` or `r#"…"#` -- and what
/// follows it; `None` when it starts with something else.
fn rust_string(s: &str) -> Option<(String, &str)> {
    if let Some(raw) = s.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|b| *b == b'#').count();
        let body = raw[hashes..].strip_prefix('"')?;
        let close = format!("\"{}", "#".repeat(hashes));
        let end = body.find(&close)?;
        return Some((body[..end].to_string(), &body[end + close.len()..]));
    }
    let body = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &body[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                'x' => {
                    let hex: String = (0..2)
                        .filter_map(|_| chars.next().map(|(_, h)| h))
                        .collect();
                    out.push(u8::from_str_radix(&hex, 16).ok()? as char);
                }
                'u' => {
                    chars.next(); // `{`
                    let mut hex = String::new();
                    for (_, h) in chars.by_ref() {
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                // A line continued: the break and the next line's indentation
                // are not in the string.
                '\n' => {
                    while chars.peek().is_some_and(|(_, w)| w.is_whitespace()) {
                        chars.next();
                    }
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            c => out.push(c),
        }
    }
    None
}

/// `src` compiled with no library, as the glade cases are: its definitions,
/// entered at `main`.
fn compiled(src: &str) -> Result<core::Program, String> {
    let opts = meadow_compiler::Options::default().entry("result");
    let (pkg, diags) = meadow_compiler::compile_str_with("boot", src, opts);
    if !diags.is_empty() {
        return Err(diags
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("; "));
    }
    let entry = pkg.value_entry;
    Ok(core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    })
}

/// What the Rust CEK machine answers for `p`, as a line: its value as the
/// machine displays it, or `error: why`.
fn cek(p: &core::Program) -> String {
    match meadow_eval::run(p) {
        Ok(v) => v.to_string(),
        Err(e) => format!("error: {}", e.msg),
    }
}

/// `p` as MeadowBoot's `eval` reads it: the program the CEK machine runs --
/// generic definitions copied per number type, and types gone -- written in
/// `CoreText`, the grammar in `bootstrap/src/Core.mw`. A word says what each
/// thing is and comes first, a list is in brackets, a number is a number (a
/// float its bits), and names and text are quoted, with `\xNN` for a control
/// byte, a backslash or a quote.
fn core_text(p: &core::Program) -> String {
    let p = core::erase::program(&core::specialize::program(p));
    let mut out = format!("program {}\ndefs", p.entry.map_or(-1, |v| v.0 as i64));
    for d in &p.defs {
        out.push_str(&format!("\n {} ", d.var.0));
        term(&d.term, &mut out);
    }
    out.push_str("\nfields");
    let mut fields: Vec<_> = p.ctor_fields.iter().collect();
    fields.sort_by_key(|(k, _)| k.to_string());
    for (ctor, fs) in fields {
        out.push_str(&format!(" {} [", quoted(ctor)));
        for f in fs {
            out.push_str(&format!(" {}", quoted(f)));
        }
        out.push_str(" ]");
    }
    out.push('\n');
    out
}

fn quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        let n = c as u32;
        if n < 32 || n == 127 || c == '\\' || c == '"' {
            out.push_str(&format!("\\x{n:02X}"));
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// A literal, exactly: a float is its bits, since text would have to be parsed
/// back, and a parser rounds.
fn lit(l: &Lit, out: &mut String) {
    let s = match l {
        Lit::Int(i) | Lit::AnyInt(i, _) => format!("int {i}"),
        Lit::BigInt(i) => format!("bigint {i}"),
        Lit::Float(x) | Lit::AnyFloat(x, _) => format!("f64 {}", x.to_bits() as i64),
        Lit::Float32(x) => format!("f32 {}", x.to_bits()),
        Lit::Word(w, b) => format!("word {} {}", quoted(&format!("{w:?}")), *b as i64),
        Lit::Str(s) | Lit::Sym(s) => format!("str {}", quoted(s)),
        Lit::Char(c) => format!("char {}", *c as u32),
        Lit::Bool(b) => b.to_string(),
        Lit::Unit => "unit".to_string(),
    };
    out.push_str(&s);
}

fn pats(ps: &[Pat], out: &mut String) {
    out.push_str(" [");
    for q in ps {
        out.push(' ');
        pat(q, out);
    }
    out.push_str(" ]");
}

fn pat(p: &Pat, out: &mut String) {
    match p {
        Pat::Wild => out.push_str("wild"),
        Pat::Var(v, _) => out.push_str(&format!("pvar {}", v.0)),
        Pat::As(v, _, sub) => {
            out.push_str(&format!("pas {} ", v.0));
            pat(sub, out);
        }
        Pat::Lit(l) => {
            out.push_str("plit ");
            lit(l, out);
        }
        Pat::Tuple(ps) => {
            out.push_str("ptuple");
            pats(ps, out);
        }
        Pat::Array(ps) => {
            out.push_str("parray");
            pats(ps, out);
        }
        Pat::Ctor(name, ps) => {
            out.push_str(&format!("pctor {}", quoted(name)));
            pats(ps, out);
        }
        Pat::Record(fields) => {
            out.push_str("precord [");
            for (l, q) in fields {
                out.push_str(&format!(" {} ", quoted(l)));
                pat(q, out);
            }
            out.push_str(" ]");
        }
    }
}

/// `head`, then each of `ts`, a space before each.
fn node(head: &str, ts: &[&Term], out: &mut String) {
    out.push_str(head);
    for t in ts {
        out.push(' ');
        term(t, out);
    }
}

/// `head`, then `ts` in brackets.
fn list(head: &str, ts: &[Term], out: &mut String) {
    out.push_str(head);
    out.push_str(" [");
    for t in ts {
        out.push(' ');
        term(t, out);
    }
    out.push_str(" ]");
}

fn term(t: &Term, out: &mut String) {
    match t {
        Term::Var(v) => out.push_str(&format!("var {}", v.0)),
        Term::Lit(l) => {
            out.push_str("const ");
            lit(l, out);
        }
        Term::Loc(_, inner) | Term::TyLam(_, inner) | Term::TyApp(inner, _) => term(inner, out),
        Term::Lam(v, _, body) => node(&format!("lam {}", v.0), &[body], out),
        Term::App(f, a) => node("app", &[f, a], out),
        Term::Let(v, _, rhs, body) => node(&format!("let {}", v.0), &[rhs, body], out),
        Term::LetRec(binds, body) => {
            out.push_str("letrec [");
            for (v, _, rhs) in binds {
                node(&format!(" {}", v.0), &[rhs], out);
            }
            out.push_str(" ]");
            node("", &[body], out);
        }
        Term::If(c, a, b) => node("if", &[c, a, b], out),
        Term::Tuple(xs) => list("tuple", xs, out),
        Term::Array(xs, _) => list("array", xs, out),
        Term::Proj(x, i) => {
            node("proj", &[x], out);
            out.push_str(&format!(" {i}"));
        }
        Term::Record(fields) => {
            out.push_str("record [");
            for (l, x) in fields {
                node(&format!(" {}", quoted(l)), &[x], out);
            }
            out.push_str(" ]");
        }
        Term::Sel(x, l, _) => {
            node("sel", &[x], out);
            out.push_str(&format!(" {}", quoted(l)));
        }
        Term::Extend(x, l, v) => {
            node("extend", &[x], out);
            node(&format!(" {}", quoted(l)), &[v], out);
        }
        Term::Ctor(name, _, xs) => list(&format!("ctor {}", quoted(name)), xs, out),
        Term::Prim(op, xs, _) => list(&format!("prim {}", quoted(&format!("{op:?}"))), xs, out),
        Term::Case(s, arms, _) => {
            node("case", &[s], out);
            out.push_str(" [");
            for (p, g, b) in arms {
                out.push(' ');
                pat(p, out);
                if let Some(g) = g {
                    node(" when", &[g], out);
                }
                node("", &[b], out);
            }
            out.push_str(" ]");
        }
        Term::Perform(effect, op, arg, _) => node(
            &format!("perform {} {}", quoted(effect), quoted(op)),
            &[arg],
            out,
        ),
        Term::Handle {
            body, clauses, ret, ..
        } => {
            node("handle", &[body], out);
            out.push_str(" [");
            for c in clauses {
                node(
                    &format!(
                        " {} {} {} {}",
                        quoted(&c.effect),
                        quoted(&c.op),
                        c.param.0,
                        c.resume.0
                    ),
                    &[&c.body],
                    out,
                );
            }
            out.push_str(" ]");
            if let Some((v, _, b)) = ret {
                node(&format!(" return {}", v.0), &[b], out);
            }
        }
        Term::Join {
            var,
            params,
            rhs,
            body,
            ..
        } => {
            let ps: Vec<String> = params.iter().map(|(v, _)| format!(" {}", v.0)).collect();
            node(
                &format!("join {} [{} ]", var.0, ps.concat()),
                &[rhs, body],
                out,
            );
        }
        Term::Jump(j, args, _) => list(&format!("jump {}", j.0), args, out),
        Term::Error => out.push_str("error"),
    }
}

/// Programs with traits, which the glade cases -- compiled with no library
/// -- have none of: what dictionary passing has to get right. And one with
/// views in its patterns.
fn trait_programs() -> Vec<String> {
    [
        // A default, an `impl` that has its own, and a function of two
        // dictionaries.
        r#"trait Shape a {
  fun area : a -> Int
  fun twice : a -> Int
    | twice x = area x + area x
}

data Sq = Sq Int

data Rect = Rect Int Int

impl Shape Sq {
  fun area s = match s with | Sq n -> n * n
}

impl Shape Rect {
  fun area r = match r with | Rect w h -> w * h
  fun twice r = 0 - 1
}

fun total x y = area x + twice y

def result = (total (Sq 3) (Rect 2 5), twice (Sq 2), total (Rect 1 1) (Sq 1))
"#,
        // An `impl` with a `where`: a dictionary made of another.
        r#"trait Named a {
  fun name : a -> String
}

data Box a = Box a

data Dog = Dog

data Cat = Cat

impl Named Dog {
  fun name d = "dog"
}

impl Named Cat {
  fun name c = "cat"
}

impl Named (Box a) where Named a {
  fun name b = match b with | Box x -> concatStrings #["box of ", name x]
}

fun both x y = concatStrings #[name x, " and ", name y]

def result = (both (Box (Box Dog)) Cat, name (Box Cat))
"#,
        // A trait that requires one: the required trait's dictionary out of
        // the requiring one's.
        r#"trait Base a {
  fun base : a -> Int
}

trait Scaled a <: Base a {
  fun scale : a -> Int
}

data One = One

data Wrap a = Wrap a

impl Base One {
  fun base x = 7
}

impl Scaled One {
  fun scale x = 3
}

impl Base (Wrap a) where Base a {
  fun base w = match w with | Wrap x -> base x + 1
}

impl Scaled (Wrap a) where Scaled a {
  fun scale w = match w with | Wrap x -> scale x * 2
}

fun scaled x = base x * scale x

fun count n x = if n <= 0 then 0 else scaled x + count (n - 1) x

def result = (scaled One, scaled (Wrap (Wrap One)), count 3 (Wrap One))
"#,
        // Functions that call each other with one dictionary, and a method
        // as a value.
        r#"trait Step a {
  fun step : a -> a
  fun finished : a -> Bool
}

data Count = Count Int

impl Step Count {
  fun step c = match c with | Count n -> Count (n - 1)
  fun finished c = match c with | Count n -> n <= 0
}

fun evens x = if finished x then 0 else 1 + odds (step x)

fun odds x = if finished x then 0 else 1 + evens (step x)

fun apply f x = f x

def result = (evens (Count 5), match apply step (Count 3) with | Count n -> n)
"#,
        // Values that are not pointers through generic code: an `impl` at
        // `Int` and at `Float`, one generic in two types made of them, and a
        // local function used at two types.
        r#"trait Double a {
  fun double : a -> a
}

data Pair a b = Pair a b

impl Double Int {
  fun double n = n * 2
}

impl Double Float {
  fun double x = x *. 2.0
}

impl Double (Pair a b) where Double a, Double b {
  fun double p = match p with | Pair x y -> Pair (double x) (double y)
}

fun quad x = double (double x)

fun first p = match p with | Pair x y -> x

fun both p = match p with | Pair x y -> (x, y)

def result =
  let same = \x -> x in
  (quad 3, first (quad (Pair 1 2.5)), both (quad (Pair 0.5 7)), same 4, same "s")
"#,
        // Views in patterns: one under a guard, and one whose function is
        // written with a name the pattern bound to its left.
        r#"use M.*

data M = N | J Int

fun half n = if n % 2 == 0 then J (n / 2) else N

fun f x = match x with
  | (half -> J k) if k > 10 -> k + 1000
  | (half -> J k) -> k
  | _ -> 0 - 1

fun g p = match p with
  | (k, (\n -> if n > k then J (n - k) else N) -> J d) -> d
  | _ -> 0

def result = (f 8, f 7, f 40, g (3, 10), g (3, 1))
"#,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Each glade case that compiles and has a `result`, and then each of
/// [`trait_programs`], written as a package of its own under `dir` -- no library, as the case is compiled -- with what
/// the Rust CEK machine answers for it: the file to run, and the answer.
fn run_cases(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let glade = glade_programs();
    let theirs = glade.len();
    let programs: Vec<String> = glade.into_iter().chain(trait_programs()).collect();
    for (i, src) in programs.iter().enumerate() {
        let p = match compiled(src) {
            Ok(p) => p,
            Err(why) if i >= theirs => panic!("a program with traits does not compile: {why}"),
            Err(_) => continue,
        };
        if p.entry.is_none() {
            continue;
        }
        let pkg = dir.join(format!("case{i:03}"));
        std::fs::create_dir_all(pkg.join("src")).expect("a package directory");
        std::fs::write(
            pkg.join("Meadow.toml"),
            "[package]\nname = \"boot\"\nversion = \"0.1.0\"\n",
        )
        .expect("a manifest");
        let file = pkg.join("src").join("Main.mw");
        std::fs::write(&file, src).expect("a source file");
        out.push((file, cek(&p)));
    }
    out
}

/// The cases of [`run_cases`] written where `MEADOWBOOT_CASES` says, each
/// with an `expected.txt` beside its manifest: to run a build of MeadowBoot
/// on by hand, without this test building one.
#[test]
#[ignore]
fn write_run_cases() {
    let dir = PathBuf::from(std::env::var("MEADOWBOOT_CASES").expect("MEADOWBOOT_CASES"));
    for (file, want) in run_cases(&dir) {
        let pkg = file.parent().and_then(Path::parent).expect("a package");
        std::fs::write(pkg.join("expected.txt"), format!("{want}\n")).expect("an answer");
    }
}

/// How many cases MeadowBoot has to compile and run to the Rust compiler's
/// answer: what its lowering to core does today, which is not yet all of
/// them. It goes up as the lowering learns more -- the copies a number's
/// type chooses between -- and a case that stops agreeing is a failure
/// whatever the count.
const RUN_CASES_AGREEING: usize = 101;

#[test]
fn glade_cases_compiled_by_meadowboot_evaluate_as_the_rust_compiler_has_them() {
    let dir = std::env::temp_dir().join(format!("meadowboot-run-{}", std::process::id()));
    let cases = run_cases(&dir);
    let wants: std::collections::HashMap<PathBuf, String> = cases.iter().cloned().collect();
    let files: Vec<PathBuf> = cases.iter().map(|(f, _)| f.clone()).collect();
    let theirs = per_file("run", &files);
    let bad = differences(&theirs, |p| format!("{}\n", wants[p]));
    let _ = std::fs::remove_dir_all(&dir);
    let agreeing = files.len() - bad.len();
    eprintln!("{agreeing} of {} cases agree", files.len());
    if std::env::var_os("MEADOWBOOT_VERBOSE").is_some() {
        for b in &bad {
            eprintln!("{b}\n");
        }
    }
    assert!(
        agreeing >= RUN_CASES_AGREEING,
        "{agreeing} of {} cases agree, and {RUN_CASES_AGREEING} did:\n\n{}",
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// A package the program of the next test depends on: a type, functions --
/// one generic in its type -- a value, a trait with a default and two
/// `impl`s, one of them of a type that takes one, an effect, and a record.
const SHAPES: &str = r#"use Shape.*
use Crate.*

@pub data Shape = Circle Int | Rect Int Int

@pub fun area s = match s with | Circle r -> 3 * r * r | Rect w h -> w * h

@pub fun twice f x = f (f x)

@pub def unitSquare = Rect 1 1

@pub trait Measure a {
  fun measure : a -> Int
  fun more : a -> Int
    | more x = measure x + 1
}

impl Measure Shape {
  fun measure s = area s
}

@pub data Crate a = Crate a

impl Measure (Crate a) where Measure a {
  fun measure b = match b with | Crate x -> measure x * 2
}

@pub effect Tick { tick : () -> Int }

@pub fun ticks u = tick () + tick ()

@pub record Point = { x : Int, y : Int }
"#;

/// A program of two units: everything `SHAPES` exports used from another
/// package, with an `impl` of its trait for a type of this one.
const APP: &str = r#"use Shapes (Shape, area, twice, unitSquare, Measure, measure, more, Crate, Tick, tick, ticks, Point)
use Shapes.Shape.*
use Shapes.Crate.*

data Mine = Mine

impl Measure Mine {
  fun measure m = 10
}

fun total x = measure x + more x

def result =
  (
    area (Rect 2 3),
    twice (\n -> n + 1) 5,
    area unitSquare,
    total (Rect 2 3),
    total (Crate Mine.Mine),
    more Mine.Mine,
    handle ticks () with { tick u k -> k 4 },
    (Point { x = 1, y = 2 }).y
  )

"#;

/// A program of more than one unit is lowered as one: each unit's
/// definitions under its own package's symbols, a mention of another unit's
/// by its symbol, and the declarations of the units before a unit before
/// its own. What the Rust compiler answers for this one -- built with the
/// standard library, which it uses nothing of -- is written here.
#[test]
fn a_program_of_two_units_lowered_by_meadowboot_answers_as_the_rust_compiler_has_it() {
    let dir = std::env::temp_dir().join(format!("meadowboot-units-{}", std::process::id()));
    let package = |name: &str, manifest: &str, file: &str, text: &str| {
        let pkg = dir.join(name);
        std::fs::create_dir_all(pkg.join("src")).expect("a package directory");
        std::fs::write(pkg.join("Meadow.toml"), manifest).expect("a manifest");
        let path = pkg.join("src").join(file);
        std::fs::write(&path, text).expect("a source file");
        path
    };
    package(
        "Shapes",
        "[package]\nname = \"Shapes\"\nversion = \"0.1.0\"\n",
        "Lib.mw",
        SHAPES,
    );
    let main = package(
        "App",
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nShapes = { path = \"../Shapes\" }\n",
        "Main.mw",
        APP,
    );
    let lowered = per_file("cut", &[main]);
    let text = &lowered[0].1;
    let want = "(6, 7, 1, 13, 41, 11, 8, 2)";
    let (interpreted, on_axcut) = (cut_answer(text), axcut_answer(text));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(interpreted, want, "on the Cut interpreter:\n{text}");
    assert_eq!(on_axcut, want, "on the AxCut machine:\n{text}");
}

/// A program that uses the standard library: lowered with it, the whole of
/// the library's definitions before the program's.
const WITH_STD: &str = r#"use Std.Collections.Vector as V
use Std.Collections.HashMap as H
use Std.Maybe as M
use Std.String as S

fun double x = x * 2

def result =
  (
    V.map double [1, 2, 3],
    V.foldl (\a b -> a + b) 0 [1, 2, 3],
    M.map double (Just 4),
    "a" ++ "b",
    show 12,
    1 :: 2 :: Nil,
    H.lookup "b" (H.insert "b" 2 H.empty),
    S.join ", " (S.split "," "x,y"),
    "${show 3} and ${show True}"
  )
"#;

/// A program that uses the standard library is lowered with it and runs on
/// the AxCut machine: the library's traits as dictionaries, its generic
/// definitions given descriptors, and the runtime knowing its vector by the
/// roles the program declares. What the Rust compiler answers is written
/// here.
#[test]
fn a_program_with_the_standard_library_lowered_by_meadowboot_answers_as_the_rust_compiler_has_it() {
    let dir = std::env::temp_dir().join(format!("meadowboot-std-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a package directory");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"WithStd\"\nversion = \"0.1.0\"\n",
    )
    .expect("a manifest");
    let main = dir.join("src").join("Main.mw");
    std::fs::write(&main, WITH_STD).expect("a source file");
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .arg(&main)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        // With no macro expanded: what a macro of the library's writes is not
        // all lowered yet, and the program uses none of it.
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let text = stdout
        .split_once("\ncut 0\n")
        .map(|(_, rest)| format!("cut 0\n{rest}"))
        .unwrap_or_else(|| {
            panic!(
                "no program:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    assert_eq!(
        axcut_answer(&text),
        r#"([2, 4, 6], 6, Just(8), "ab", "12", [1; 2], Just(2), "x, y", "3 and True")"#
    );
}

/// Functions written once for any number type, used at several: one that
/// calls itself, one that calls another, one over fractions, and one that
/// hands a function generic in its number to the standard library.
const WITH_NUMBERS: &str = r#"use Std.Collections.Vector as V

fun factorial n = if n == 0 then 1 else n * factorial (n - 1)

fun next n = n + 1

fun twiceNext n = next n + next 10

fun half x = x *. 0.5

fun bumped xs = V.map (\x -> x + 300) xs

def result =
  (
    factorial 30,
    factorial (toBigInt 30),
    factorial (toUInt8 5),
    twiceNext (toBigInt 0),
    half (toFloat32 1.0),
    half 3.0,
    bumped [toUInt8 0, toUInt8 1],
    bumped [toBigInt 0]
  )
"#;

/// A definition generic in a number's type is copied for each number type it
/// is used at, so that a literal in it is of that type -- the `1` of
/// `factorial` a `BigInt` where its argument is one, and not the `Int` the
/// definition as it is written makes it. Without the copies the first of
/// these wraps and is right, and the rest are not numbers at all.
#[test]
fn a_function_generic_in_its_number_is_right_at_each_number_type_it_is_used_at() {
    let want = "(-8764578968847253504, 265252859812191058636308480000000, 120, 12, 0.5, 1.5, \
                [44, 45], [300])";
    for opt in ["O1", "O2", "O3"] {
        let cut = cut_of(&format!("numbers-{opt}"), WITH_NUMBERS, &["--opt", opt]);
        assert_eq!(axcut_answer(&cut), want, "at {opt}");
    }
}

/// A program each level does something to that the level under it does not:
/// an operator and `show` at a known type, a small function, and a function
/// that takes a dictionary and is too large to be copied for it below O3 --
/// its body is `show x` a hundred and sixty times over.
fn with_levels() -> String {
    format!(
        "use Std.Collections.Vector as V\n\nfun double x = x * 2\n\nfun big x = {}\n\n\
         def result = (V.map double [1, 2, 3], stringByteLength (big 7))\n",
        vec!["show x"; 160].join(" ++ ")
    )
}

/// What MeadowBoot's `cut` writes of the program `source`, told `args`: the
/// package is made in a directory named for `name`.
fn cut_of(name: &str, source: &str, args: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("meadowboot-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a package directory");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]
name = \"Levels\"
version = \"0.1.0\"
",
    )
    .expect("a manifest");
    let main = dir.join("src").join("Main.mw");
    std::fs::write(&main, source).expect("a source file");
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .args(args)
        .arg(&main)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .split_once(
            "
cut 0
",
        )
        .map(|(_, rest)| {
            format!(
                "cut 0
{rest}"
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "no program of {name}:
{stdout}
{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

/// What MeadowBoot's `cut` writes of [`with_levels`] at level `opt`.
fn cut_at(opt: &str) -> String {
    cut_of(opt, &with_levels(), &["--opt", opt])
}

/// A build told `whole` optimises its units as one program at the link, where
/// each is otherwise optimised by itself: every unit is kept as it was
/// written, the link joins them, and what is made better is the program. It
/// answers as the program a unit at a time does, at the levels that do
/// anything, with the standard library and without a call of it left out.
#[test]
fn a_program_optimised_as_one_answers_as_one_optimised_a_unit_at_a_time() {
    for opt in ["O1", "O2", "O3"] {
        let cut = cut_of(
            &format!("whole-{opt}"),
            WITH_STD,
            &["--opt", opt, "--set", "whole=true"],
        );
        assert_eq!(
            axcut_answer(&cut),
            r#"([2, 4, 6], 6, Just(8), "ab", "12", [1; 2], Just(2), "x, y", "3 and True")"#,
            "at {opt}"
        );
    }
    let cut = cut_of(
        "whole-levels",
        &with_levels(),
        &["--opt", "O2", "--set", "whole=true"],
    );
    assert_eq!(axcut_answer(&cut), "([2, 4, 6], 160)");
}

/// Each level MeadowBoot optimises at is a level of its own: O0 leaves every
/// method to be asked of its dictionary, O1 reads the known ones, O2 copies
/// small functions to their calls, and O3 copies larger functions for the types
/// they are called at. So each writes a
/// program the level under it does not -- and all four answer the same.
#[test]
fn each_level_meadowboot_optimises_at_writes_its_own_program_and_the_same_answer() {
    let levels = ["O0", "O1", "O2", "O3"];
    let cuts: Vec<String> = levels.iter().map(|o| cut_at(o)).collect();
    for (level, cut) in levels.iter().zip(&cuts) {
        assert_eq!(axcut_answer(cut), "([2, 4, 6], 160)", "at {level}");
    }
    for pair in 0..levels.len() - 1 {
        assert_ne!(
            cuts[pair],
            cuts[pair + 1],
            "{} and {} write the same program",
            levels[pair],
            levels[pair + 1]
        );
    }
}

const WITH_CFG: &str = r#"@cfg(os = "linux")
def system = "linux"

@cfg(os = "macos")
def system = "macos"

@cfg(os = "windows")
def system = "windows"

@cfg(unix)
def family = "unix"

@cfg(windows)
def family = "windows"

@cfg(debug)
def build = "debug"

@cfg(release)
def build = "release"

@cfg(any(fast, feature = "quick"))
def speed = "fast"

@cfg(not(any(fast, feature = "quick")))
def speed = "slow"

@cfg(all(opt_level = "2", backend = "aot", not(test)))
def level = "two"

@cfg(not(all(opt_level = "2", backend = "aot", not(test))))
def level = "other"

-- A field is there or not as a declaration is, and a derive sees the record
-- that is left.
@derive(Debug)
record Shape = {
  @cfg(unix)
  round : Int,
  @cfg(windows)
  square : Int
}

@cfg(unix)
def shape = Shape { round = 1 }

@cfg(windows)
def shape = Shape { square = 2 }

def result = (system, family, build, speed, level, debug shape)
"#;

/// What MeadowBoot's `cut` answers of [`WITH_CFG`], told `args`.
fn cfg_answer(name: &str, args: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("meadowboot-cfg-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a package directory");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"WithCfg\"\nversion = \"0.1.0\"\n",
    )
    .expect("a manifest");
    let main = dir.join("src").join("Main.mw");
    std::fs::write(&main, WITH_CFG).expect("a source file");
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .args(args)
        .arg(&main)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let text = stdout
        .split_once("\ncut 0\n")
        .map(|(_, rest)| format!("cut 0\n{rest}"))
        .unwrap_or_else(|| {
            panic!(
                "no program:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    axcut_answer(&text)
}

/// What a `@cfg` holds of is the build's options: the system and processor
/// MeadowBoot was itself built for unless it is told another, the profile's
/// kind and level, and the flags of `--cfg`.
#[test]
fn a_cfg_is_decided_by_the_options_meadowboot_builds_with() {
    let os = std::env::consts::OS;
    let family = if os == "windows" { "windows" } else { "unix" };
    assert_eq!(
        cfg_answer("host", &[]),
        format!(
            r#"("{os}", "{family}", "debug", "slow", "other", "Shape({})")"#,
            if os == "windows" { 2 } else { 1 }
        )
    );
    assert_eq!(
        cfg_answer(
            "told",
            &[
                "--profile",
                "release",
                "--set",
                "os=windows",
                "--cfg",
                "fast"
            ]
        ),
        r#"("windows", "windows", "release", "fast", "two", "Shape(2)")"#
    );
    assert_eq!(
        cfg_answer(
            "flag",
            &[
                "--set",
                "os=linux,test=true",
                "--opt",
                "O2",
                "--cfg",
                "feature=quick"
            ]
        ),
        r#"("linux", "unix", "debug", "fast", "other", "Shape(1)")"#
    );
    // And the value the program answers is an option of the link's.
    assert_eq!(
        cfg_answer("entry", &["--set", "entry=speed", "--cfg", "fast"]),
        r#""fast""#
    );
}

/// A package of procedural macros: one that writes its argument twice, one
/// that leaves a number at compile time under the name it is given, and one
/// that reads it back.
const MACROS_LIB: &str = r#"use Std.Macro (TokenTree, Loc, Expand, Datum, lookup, define)
use Std.Macro.TokenTree.*
use Std.Macro.Loc.*
use Std.Macro.Delim.*
use Std.Collections.Vector as V
use Std.Maybe.Maybe.*

@macro
@pub fun twice (ts : [TokenTree]) : [TokenTree] ! { Expand | e } =
  V.concat [[Group Paren ts Nowhere], [Punct "+" Nowhere], [Group Paren ts Nowhere]]

@macro
@pub fun remember (ts : [TokenTree]) : [TokenTree] ! { Expand | e } =
  match V.get ts 0 with
  | Just (Word name _) -> (let u = define (name, Datum.Int 20) in [])
  | _ -> [Fail "remember! takes a name" Nowhere]

@macro
@pub fun recalled (ts : [TokenTree]) : [TokenTree] ! { Expand | e } =
  match V.get ts 0 with
  | Just (Word name _) ->
      (match lookup name with
       | Just (Datum.Int n) -> [Num (n + 1) Nowhere]
       | _ -> [Num 0 Nowhere])
  | _ -> [Fail "recalled! takes a name" Nowhere]
"#;

/// A program that calls them: `first` looks up what a call written after it
/// defines, so it waits a round.
const MACROS_APP: &str = r#"use Mac (twice!, remember!, recalled!)

def first = recalled!(kept)

remember!(kept)

def result = (twice!(first), twice!(1 + 2))
"#;

/// What [`MACROS_APP`] answers, compiled by MeadowBoot with its macros run
/// on `engine`.
fn macros_answer(engine: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("meadowboot-macros-{engine}-{}", std::process::id()));
    for (pkg, manifest, file, text) in [
        (
            "Mac",
            "[package]\nname = \"Mac\"\nversion = \"0.1.0\"\n",
            "Lib.mw",
            MACROS_LIB,
        ),
        (
            "App",
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nMac = { path = \"../Mac\" }\n",
            "Main.mw",
            MACROS_APP,
        ),
    ] {
        std::fs::create_dir_all(dir.join(pkg).join("src")).expect("a package directory");
        std::fs::write(dir.join(pkg).join("Meadow.toml"), manifest).expect("a manifest");
        std::fs::write(dir.join(pkg).join("src").join(file), text).expect("a source file");
    }
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .arg("--set")
        .arg(format!("macros={engine}"))
        .arg(dir.join("App").join("src").join("Main.mw"))
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env("MEADOW", meadow())
        // Nothing recorded: what the macros write is what MeadowBoot runs
        // them to find out.
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let text = stdout
        .split_once("\ncut 0\n")
        .map(|(_, rest)| format!("cut 0\n{rest}"))
        .unwrap_or_else(|| {
            panic!(
                "no program:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    axcut_answer(&text)
}

/// MeadowBoot runs a procedural macro itself -- a function of a package the
/// program depends on -- on its CEK machine, and as a program it compiles
/// and the back end runs on Glade: each answers what the Rust compiler does,
/// a call that waits for a compile-time binding among them.
#[test]
fn a_procedural_macro_is_run_by_meadowboot_on_either_engine() {
    assert_eq!(macros_answer("Cek"), "(42, 6)");
    assert_eq!(macros_answer("Glade"), "(42, 6)");
}

/// The macros every module has: the line a call is on, its argument written
/// back as text, and literals joined into one string -- as
/// `meadow_compiler::expand` answers them.
#[test]
fn the_built_in_macros_answer_in_meadowboot() {
    let dir = std::env::temp_dir().join(format!("meadowboot-builtin-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a package directory");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"BuiltIn\"\nversion = \"0.1.0\"\n",
    )
    .expect("a manifest");
    let main = dir.join("src").join("Main.mw");
    std::fs::write(
        &main,
        "def first = line!()\n\ndef result =\n  (first, line!(), stringify!(a + (b), [1, 2]), concat!(\"a\", 1, \"c\"))\n",
    )
    .expect("a source file");
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .arg(&main)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let text = stdout
        .split_once("\ncut 0\n")
        .map(|(_, rest)| format!("cut 0\n{rest}"))
        .unwrap_or_else(|| {
            panic!(
                "no program:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    assert_eq!(axcut_answer(&text), r#"(1, 4, "a + (b), [1, 2]", "a1c")"#);
}

/// A built-in name of `@cfg` can only be compared with what it could be: a
/// value it never has is a mistake, said where it is written, and not a
/// condition that is quietly never true.
#[test]
fn a_cfg_that_could_never_hold_is_refused_by_meadowboot() {
    let dir = std::env::temp_dir().join(format!("meadowboot-cfg-never-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("a package directory");
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Never\"\nversion = \"0.1.0\"\n",
    )
    .expect("a manifest");
    let main = dir.join("src").join("Main.mw");
    std::fs::write(
        &main,
        "@cfg(os = \"linx\")\ndef only = 1\n\ndef result = 2\n",
    )
    .expect("a source file");
    let out = Command::new(meadowboot_exe())
        .arg("cut")
        .arg(&main)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env_remove("MEADOWBOOT_EXPANSIONS")
        .output()
        .expect("MeadowBoot runs");
    let _ = std::fs::remove_dir_all(&dir);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains("`os` is never `\"linx\"`") && said.contains("not compiled"),
        "{said}"
    );
}

/// How many glade cases MeadowBoot has to lower to Cut so that the reference
/// interpreter answers as the Rust compiler does. The rest wait on primitives
/// the interpreter does not have -- the sized integers', the floats', `show`
/// -- more than on the lowering. A case that stops agreeing is a failure
/// whatever the count.
const CUT_CASES_AGREEING: usize = 64;

/// What the reference interpreter prints of the Cut program `text`, or why
/// it printed nothing. On a thread of its own with room to spare: the
/// interpreter's closures hold all that was in scope where they were made,
/// and letting go of a long chain of them is as deep as the chain is long.
fn cut_answer(text: &str) -> String {
    let text = text.to_string();
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            let program = match meadow_cut::parse(&text) {
                Ok(p) => p,
                Err(e) => return format!("does not read: {e}"),
            };
            match meadow_cut::interp::run(&program, &meadow_cut::interp::Options::default()) {
                Ok(out) => out.output,
                Err((e, _)) => format!("fails: {e}"),
            }
        })
        .expect("a thread")
        .join()
        .unwrap_or_else(|_| "the interpreter panicked".to_string())
}

/// How many of them answer so once that Cut is lowered to AxCut and run by
/// its machine, which has every primitive the runtimes have and checks how
/// each value is represented. The rest are a function generic in what it
/// takes, whose values MeadowBoot calls a `ptr` whatever they are.
const AXCUT_CASES_AGREEING: usize = 101;

/// What the AxCut machine answers of the Cut program `text` once it is
/// lowered, or why it answered nothing.
fn axcut_answer(text: &str) -> String {
    use meadow_axcut::machine::{Machine, Value};
    let program = match meadow_cut::parse(text) {
        Ok(p) => p,
        Err(e) => return format!("does not read: {e}"),
    };
    let lowered = match meadow_cut::lower::lower(&program) {
        Ok(l) => l,
        Err(e) => return format!("not lowered: {e}"),
    };
    match Machine::run(&lowered, 500_000_000) {
        Ok(Value::Str(s)) => s.to_string(),
        Ok(v) => format!("answered {v}, not a string"),
        Err(e) => format!("fails: {e:?}"),
    }
}

#[test]
fn glade_cases_lowered_to_cut_by_meadowboot_answer_as_the_rust_compiler_has_them() {
    let dir = std::env::temp_dir().join(format!("meadowboot-cut-{}", std::process::id()));
    let cases = run_cases(&dir);
    let wants: std::collections::HashMap<PathBuf, String> = cases.iter().cloned().collect();
    let files: Vec<PathBuf> = cases.iter().map(|(f, _)| f.clone()).collect();
    let lowered = per_file("cut", &files);
    let answers: Vec<(PathBuf, String)> = lowered
        .iter()
        .map(|(p, text)| (p.clone(), format!("{}\n", cut_answer(text))))
        .collect();
    let bad = differences(&answers, |p| format!("{}\n", wants[p]));
    let on_axcut: Vec<(PathBuf, String)> = lowered
        .iter()
        .map(|(p, text)| (p.clone(), format!("{}\n", axcut_answer(text))))
        .collect();
    let bad_on_axcut = differences(&on_axcut, |p| format!("{}\n", wants[p]));
    let agreeing_on_axcut = files.len() - bad_on_axcut.len();
    eprintln!(
        "{agreeing_on_axcut} of {} cases agree on the AxCut machine",
        files.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
    let agreeing = files.len() - bad.len();
    eprintln!("{agreeing} of {} cases agree", files.len());
    if std::env::var_os("MEADOWBOOT_VERBOSE").is_some() {
        for b in &bad {
            eprintln!("{b}\n");
        }
    }
    assert!(
        agreeing >= CUT_CASES_AGREEING,
        "{agreeing} of {} cases agree, and {CUT_CASES_AGREEING} did:\n\n{}",
        files.len(),
        bad.iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    assert!(
        agreeing_on_axcut >= AXCUT_CASES_AGREEING,
        "{agreeing_on_axcut} of {} cases agree on the AxCut machine, and {AXCUT_CASES_AGREEING} did:\n\n{}",
        files.len(),
        bad_on_axcut
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

#[test]
fn every_glade_case_evaluates_as_the_cek_machine_evaluates_it() {
    let dir = std::env::temp_dir().join(format!("meadowboot-cek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let mut files = Vec::new();
    let mut wants = std::collections::HashMap::new();
    for (i, src) in glade_programs().iter().enumerate() {
        let p = match compiled(src) {
            Ok(p) => p,
            Err(e) => {
                if std::env::var_os("MEADOWBOOT_VERBOSE").is_some() {
                    eprintln!("case {i} does not compile: {e}\n{src}\n");
                }
                continue;
            }
        };
        if p.entry.is_none() {
            continue;
        }
        let path = dir.join(format!("case{i:03}.core"));
        std::fs::write(&path, core_text(&p)).expect("a core file");
        wants.insert(path.clone(), cek(&p));
        files.push(path);
    }
    assert!(
        files.len() > 90,
        "only {} glade cases compiled",
        files.len()
    );
    let theirs = per_file("eval", &files);
    let bad = differences(&theirs, |p| format!("{}\n", wants[p]));
    // `MEADOWBOOT_KEEP` keeps the core files, to run MeadowBoot on by hand.
    if std::env::var_os("MEADOWBOOT_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        eprintln!("the core files are in {}", dir.display());
    }
    assert!(
        bad.is_empty(),
        "{} of {} cases evaluate differently:\n\n{}",
        bad.len(),
        files.len(),
        bad.iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
