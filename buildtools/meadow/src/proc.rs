//! Running a procedural macro while the package that calls it is compiled.
//!
//! The compiler cannot do this on its own: a macro is a compiled function, and
//! running one means linking a program and evaluating it, which is this crate's
//! job. So the compiler asks, through
//! [`Runner`](meadow_compiler::expand::proc::Runner), and this answers.
//!
//! What crosses is `Std.Macro`'s `TokenTree`, and the `Datum`s of the
//! compile-time bindings the call can see. Going in, both are built as terms
//! -- `Array`s of constructors, handed to `Vector.fromArray`, which is how a
//! `[a]` is made without knowing how one is laid out. The macro is run under
//! `Std.Macro.expanding`, the handler of its `Expand` effect, which answers
//! its `lookup`s from those bindings and writes what happened into a `Wire`:
//! arrays all the way down, which the evaluator hands over as plain `Vec`s,
//! so nothing here has to know how a `[a]` is laid out either.
//!
//! It runs on the merged runtime's VM. Every macro the build can call is put
//! behind `Std.Macro.Serve.serve` in one image, an entry a macro, compiled the
//! first time a macro is called and run on one VM kept for the build
//! (`crate::served`); a run is handed what the call gives the macro --
//! whether the round is settled, the bindings it can see, the argument -- as
//! a `Wire` written out on its input, and writes the `Wire` it answers on its
//! output. A program the back end cannot translate, or a build with no VM,
//! has its macros run by the CEK machine instead, the way they were before:
//! the same terms, built as values rather than text.
//!
//! What each call answered is kept, beside the package that made the call, in
//! `target/<profile>/incremental/<name>-<options>.macros`, under a fingerprint
//! of every package a macro could live in: a build after an edit that touched
//! no macro, and an editor opening the package, answer every call from it
//! without running anything.
//!
//! Two things make this safe to do during a build. A macro's signature forbids
//! the effects that would let it learn anything about the world, so what it
//! answers depends only on what it was given and what it read -- which is why
//! the answers are cached here on exactly that. And it runs with a budget --
//! of steps on the CEK machine, of time on the VM -- so a macro that does not
//! stop fails the build instead of hanging it.

use meadow_compiler::expand::Datum;
use meadow_compiler::expand::proc::{Failure, Outcome, Runner, Scope};
use meadow_compiler::span::Span;
use meadow_compiler::{CompiledPackage, core, intern::InternedString, lexer::tt};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// How many steps a macro may take before the build gives up on it.
///
/// Generous -- a macro that builds a table from a hundred patterns does real
/// work -- but not unbounded: a macro that does not stop has to fail the build
/// rather than hang it, and a minute of silence is already too long to be a
/// good error. `MEADOW_MACRO_FUEL` raises or lowers it for the rare macro that
/// needs more, and for the tests that check what happens when one does not
/// stop.
const FUEL: u64 = 50_000_000;

fn fuel() -> u64 {
    std::env::var("MEADOW_MACRO_FUEL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FUEL)
}

/// A call, by the macro, its argument as written, whether the round it ran
/// in was settled, and where it is.
///
/// Where, because an answer is tokens, and a token says where it is: the
/// argument's, handed back, and those the macro made, placed at the call.
/// Without it a second call written the same way took the first's answer,
/// positions and all -- its errors reported at the other call, or at offsets
/// that are not in the file at all. The same call expanded again in a later
/// round is still found, which is what the cache is for.
type AnswerKey = (InternedString, InternedString, String, bool, Span);

/// The bindings a run read, and what each said -- `None` for one that was not
/// there.
type Reads = Vec<(String, Option<Datum>)>;

/// Where the packages came from.
///
/// A build has them to hand and lends them for as long as the compile takes; a
/// language server holds a runner for as long as a package is open, which is
/// longer than anything it could borrow from. Cloning is why this is a choice
/// and not a rule: a build compiles many packages and would pay for it every
/// time.
enum Held<'a> {
    Borrowed(Vec<&'a CompiledPackage>),
    Owned(Vec<CompiledPackage>),
}

impl Held<'_> {
    fn packages(&self) -> Vec<&CompiledPackage> {
        match self {
            Held::Borrowed(ps) => ps.clone(),
            Held::Owned(ps) => ps.iter().collect(),
        }
    }
}

/// The macros a build can run: every package compiled so far, linked on demand.
///
/// Linked only when a macro is actually called -- which in most builds is
/// never, and linking is not free.
pub struct Macros<'a> {
    /// The packages a macro could live in, and what it needs to run.
    packages: Held<'a>,
    /// How many steps any one of them may take.
    fuel: u64,
    /// The linked program, built the first time a macro is actually called --
    /// most builds call none, and linking is not free.
    program: RefCell<Option<Arc<core::Program>>>,
    /// What each call answered, by what it was asked and what it read. A
    /// macro cannot tell one build from another, so the same tokens and the
    /// same bindings always give the same answer.
    answers: RefCell<HashMap<AnswerKey, Vec<(Reads, Outcome)>>>,
    /// Every macro behind `serve`, compiled once: `None` until a macro is
    /// first called, and `Some(None)` if the back end could not translate the
    /// program.
    image: RefCell<Option<Option<Arc<Image>>>>,
    /// Where the answers are kept between runs, if anywhere; whether they
    /// have been read from there yet; and whether there are new ones to write.
    store: Option<(std::path::PathBuf, u64)>,
    loaded: std::cell::Cell<bool>,
    learned: std::cell::Cell<bool>,
}

/// The macros' image, written where the VM reads it; which of its entries
/// each macro's run is; and the VM running it, once one is started -- one
/// for every call of the build, and another after a call ends one.
struct Image {
    path: std::path::PathBuf,
    entries: Vec<(core::Var, usize)>,
    vm: RefCell<Option<crate::served::Served>>,
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl<'a> Macros<'a> {
    pub fn new(packages: Vec<&'a CompiledPackage>) -> Macros<'a> {
        Macros {
            packages: Held::Borrowed(packages),
            fuel: fuel(),
            program: RefCell::new(None),
            answers: RefCell::new(HashMap::new()),
            image: RefCell::new(None),
            store: None,
            loaded: std::cell::Cell::new(false),
            learned: std::cell::Cell::new(false),
        }
    }

    /// The same, keeping what the macros answer at `path` between runs: read
    /// from there when a macro is first called, and written back when this is
    /// dropped, if anything was learned. `print` is a fingerprint of every
    /// package a macro could live in -- see `incremental::macros_print` --
    /// and answers kept under another are not used.
    pub fn remembering(mut self, path: std::path::PathBuf, print: u64) -> Self {
        self.store = Some((path, print));
        self
    }

    /// The same, holding the packages itself: what something that outlives the
    /// compile -- a language server -- needs.
    pub fn owning(packages: Vec<CompiledPackage>) -> Macros<'static> {
        Macros {
            packages: Held::Owned(packages),
            fuel: fuel(),
            program: RefCell::new(None),
            answers: RefCell::new(HashMap::new()),
            image: RefCell::new(None),
            store: None,
            loaded: std::cell::Cell::new(false),
            learned: std::cell::Cell::new(false),
        }
    }

    /// The same, with a budget of its own: what a test that wants to see a
    /// macro run out of one uses, so it can do it in a moment.
    pub fn with_fuel(packages: Vec<&'a CompiledPackage>, fuel: u64) -> Macros<'a> {
        let mut m = Macros::new(packages);
        m.fuel = fuel;
        m
    }

    /// The linked program, linked once.
    fn program(&self) -> Arc<core::Program> {
        if let Some(p) = self.program.borrow().as_ref() {
            return p.clone();
        }
        let linked =
            crate::linker::Linker::link(self.packages.packages().into_iter().cloned().collect());
        let p = Arc::new(linked.program);
        *self.program.borrow_mut() = Some(p.clone());
        p
    }

    /// Read what was answered before, once, if it was answered by the same
    /// packages.
    fn recall(&self) {
        if self.loaded.replace(true) {
            return;
        }
        let Some((path, print)) = &self.store else {
            return;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Some(rest) = bytes.strip_prefix(ANSWERS_MAGIC) else {
            return;
        };
        let Some((kept_under, payload)) = rest.split_first_chunk::<8>() else {
            return;
        };
        if u64::from_le_bytes(*kept_under) != *print {
            return;
        }
        if let Ok(kept) = postcard::from_bytes::<Vec<(AnswerKey, Vec<(Reads, Outcome)>)>>(payload) {
            let mut answers = self.answers.borrow_mut();
            for (key, runs) in kept {
                answers.entry(key).or_insert(runs);
            }
        }
    }

    /// Write every answer back, if there are new ones: aside, and renamed into
    /// place, so that two builds at once never read half a file.
    fn keep(&self) {
        let Some((path, print)) = &self.store else {
            return;
        };
        if !self.learned.get() {
            return;
        }
        let kept: Vec<(AnswerKey, Vec<(Reads, Outcome)>)> = self
            .answers
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let Ok(payload) = postcard::to_stdvec(&kept) else {
            return;
        };
        let mut bytes = ANSWERS_MAGIC.to_vec();
        bytes.extend_from_slice(&print.to_le_bytes());
        bytes.extend_from_slice(&payload);
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_err()
        {
            return;
        }
        let partial = path.with_extension(format!("macros.{}", std::process::id()));
        if std::fs::write(&partial, &bytes).is_err() || std::fs::rename(&partial, path).is_err() {
            let _ = std::fs::remove_file(&partial);
        }
    }

    /// Every macro behind `serve`, compiled once: a definition per exported
    /// macro, after the program's own. `None` when the back end cannot
    /// translate the program -- then the CEK machine runs the macros.
    fn image(&self) -> Option<Arc<Image>> {
        if let Some(had) = self.image.borrow().as_ref() {
            return had.clone();
        }
        let built = self.build_image();
        *self.image.borrow_mut() = Some(built.clone());
        built
    }

    fn build_image(&self) -> Option<Arc<Image>> {
        let started = std::time::Instant::now();
        let built = self.compile_image();
        timed(|| match &built {
            Some(i) => format!(
                "image of {} macros in {:?}",
                i.entries.len(),
                started.elapsed()
            ),
            None => format!(
                "no image, after {:?}: the CEK machine runs the macros",
                started.elapsed()
            ),
        });
        built
    }

    fn compile_image(&self) -> Option<Arc<Image>> {
        let Some(serve) = self.std_fn(&["Macro", "Serve"], "serve") else {
            timed(|| "the standard library has no `Macro.Serve.serve`".to_string());
            return None;
        };
        let program = self.program();
        let macros: Vec<core::Var> = self
            .packages
            .packages()
            .into_iter()
            .flat_map(|p| p.exports.iter())
            .filter(|e| e.is_macro)
            .map(|e| e.var)
            .collect();
        let mut defs = program.defs.clone();
        // A macro generic in its argument -- `defineOne ts = …`, nothing
        // said of `ts` -- takes a descriptor for it, which only an
        // instantiation passes: so each is instantiated, every type variable
        // at what a macro is given, `[TokenTree]`, the type `serve` calls it at.
        let poly = |v: core::Var| {
            program
                .defs
                .iter()
                .find(|d| d.var == v)
                .map(|d| d.poly.clone())
        };
        let given = poly(serve).and_then(|p| match &p.ty {
            meadow_compiler::infer::Type::Fun(params, _, _) => match params.first() {
                Some(meadow_compiler::infer::Type::Fun(args, _, _)) => args.first().cloned(),
                _ => None,
            },
            _ => None,
        });
        let instance = |v: core::Var| -> core::Term {
            match (poly(v), &given) {
                (Some(p), Some(given)) if !p.binders.is_empty() => core::Term::TyApp(
                    Arc::new(core::Term::Var(v)),
                    p.binders
                        .iter()
                        .map(|b| match b.kind {
                            meadow_compiler::infer::VarKind::Type => given.clone(),
                            _ => core::unknown(),
                        })
                        .collect(),
                ),
                _ => core::Term::Var(v),
            }
        };
        for (i, var) in macros.iter().enumerate() {
            defs.push(core::Def {
                var: meadow_compiler::hir::VarId::synthetic(i as u32),
                name: "<macro>".into(),
                module: Default::default(),
                poly: program.result_of_calling(serve),
                term: core::Term::App(Arc::new(instance(serve)), Arc::new(instance(*var))),
            });
        }
        let whole = core::Program {
            defs,
            entry: program.entry,
            ctor_fields: program.ctor_fields.clone(),
            variants: program.variants.clone(),
            origins: Default::default(),
            migrations: Vec::new(),
        };
        // Every package the build has, linked whole, refers to things nothing
        // defined -- what a macro never reaches. Lowered, each is an error
        // raised if it is ever run, which is what the CEK machine does with
        // it: so the one thing lowering says it cannot translate is let stand.
        if let Err(why) = crate::aot::interpreter() {
            timed(|| format!("no VM to run the macros on: {why}"));
            return None;
        }
        // Only what the macros reach: the image is made linear, which is
        // the work, a definition at a time.
        let made: Vec<core::Var> = (0..macros.len())
            .map(|i| meadow_compiler::hir::VarId::synthetic(i as u32))
            .collect();
        let whole = core::prune::prune_from(&whole, made.iter().copied());
        // An entry a macro, by where its definition is now: a program of
        // entries, as one of tests is.
        let labels: Option<Vec<meadow_seq::Label>> = made
            .iter()
            .map(|v| {
                whole
                    .defs
                    .iter()
                    .position(|d| d.var == *v)
                    .map(|i| meadow_seq::Label(i as u32))
            })
            .collect();
        let Some(labels) = labels else {
            timed(|| "a macro's entry went missing".to_string());
            return None;
        };
        let lowered = meadow_seq::lower_program(&whole, meadow_compiler::OptLevel::O1);
        static IMAGES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("meadow-macros-{}", std::process::id()));
        let path = dir.join(format!(
            "macros-{}.image",
            IMAGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let config = meadow_llvm::Config {
            stack: meadow_llvm::Stack::Growable,
            ..crate::aot::config()
        };
        let written = std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| crate::aot::write_image_with(&path, lowered.program, labels, config));
        if let Err(why) = written {
            timed(|| format!("the back end cannot compile the macros: {why}"));
            return None;
        }
        let entries = macros.iter().enumerate().map(|(i, v)| (*v, i)).collect();
        Some(Arc::new(Image {
            path,
            entries,
            vm: RefCell::new(None),
        }))
    }

    /// Run the macro whose image entry is `entry` on `given`, the call as
    /// text, on the VM the image has -- or a new one, if there is none yet,
    /// or the last call ended it: what it wrote.
    fn run_on_vm(&self, image: &Image, entry: usize, given: String) -> Result<String, Failure> {
        let mut vm = image.vm.borrow_mut();
        if vm.is_none() {
            *vm =
                Some(crate::served::Served::start(&image.path, false, &[]).map_err(Failure::from)?);
        }
        let s = vm.as_mut().expect("started");
        // The call is one line, however many it holds, as `serve` reads it.
        match s.run(
            Some(entry),
            Some(&[given.as_str()]),
            Some(budget(self.fuel)),
        ) {
            Ok((_, written)) => Ok(written.unwrap_or_default()),
            Err(ended) => {
                let said = s.said();
                *vm = None;
                Err(Failure::from(match ended {
                    crate::served::Ended::TimedOut => format!(
                        "the macro did not finish within {:?}; `MEADOW_MACRO_FUEL` gives \
                         it more",
                        budget(self.fuel)
                    ),
                    _ if !said.trim().is_empty() => said.trim().to_string(),
                    other => other.said(),
                }))
            }
        }
    }

    /// The variable a package exports under `name`.
    fn exported(&self, package: InternedString, name: InternedString) -> Option<core::Var> {
        self.packages
            .packages()
            .into_iter()
            .filter(|p| p.name == package || p.ident == package)
            .flat_map(|p| p.exports.iter())
            .find(|e| e.name == name)
            .map(|e| e.var)
    }

    /// A function of `Std`, by the module it is in and its name.
    fn std_fn(&self, module: &[&str], name: &str) -> Option<core::Var> {
        let want: Vec<InternedString> = module.iter().map(|m| InternedString::from(*m)).collect();
        let name = InternedString::from(name);
        self.packages
            .packages()
            .into_iter()
            .flat_map(|p| p.exports.iter())
            .find(|e| e.name == name && e.module == want)
            .map(|e| e.var)
    }

    /// The canonical name of a constructor spelled `Type.Ctor`, which carries
    /// the package that declared it and so cannot be written out here.
    ///
    /// The standard library's are looked in first: every one this asks for is
    /// its, and a package with a `Datum` of its own must not be taken for it.
    fn ctor(&self, spelled: &str) -> Option<InternedString> {
        let mut packages = self.packages.packages();
        packages.sort_by_key(|p| p.name.to_string() != "Std");
        packages
            .into_iter()
            .flat_map(|p| p.variants.values())
            .flat_map(|vs| vs.iter())
            .map(|v| v.name)
            .find(|n| meadow_compiler::hir::ctor_spelling(&n.to_string()) == spelled)
    }
}

impl Runner for Macros<'_> {
    fn run(
        &self,
        package: InternedString,
        name: InternedString,
        input: &[tt::TokenTree],
        at: Span,
        scope: &Scope<'_>,
    ) -> Result<Outcome, Failure> {
        self.recall();
        let key = (package, name, tt::render(input), scope.settled, at);
        // An answer is good for as long as everything it read still says what
        // it said: the argument is the key, and the reads are checked here.
        if let Some(had) = self.answers.borrow().get(&key) {
            let current = |n: &str| {
                scope
                    .visible
                    .iter()
                    .rev()
                    .find(|(k, _)| k == n)
                    .map(|(_, d)| d)
            };
            if let Some((_, out)) = had
                .iter()
                .find(|(reads, _)| reads.iter().all(|(n, was)| current(n) == was.as_ref()))
            {
                return Ok(out.clone());
            }
        }
        let f = self
            .exported(package, name)
            .ok_or_else(|| format!("`{package}` does not export `{name}`"))?;
        let started = std::time::Instant::now();
        let mut engine = "vm";
        let (out, read) = match self.image().and_then(|image| {
            image
                .entries
                .iter()
                .find(|(v, _)| *v == f)
                .map(|(_, pc)| (image.clone(), *pc))
        }) {
            Some((image, entry)) => {
                let mut given = String::new();
                write_wire(&call_wire(scope, input), &mut given);
                let said = self.run_on_vm(&image, entry, given)?;
                let wire = read_wire(&said, &mut 0).ok_or("the macro did not answer")?;
                outcome(&wire, at)?
            }
            None => {
                engine = "cek";
                self.run_on_cek(f, input, at, scope)?
            }
        };
        timed(|| format!("{name}! on {engine} in {:?}", started.elapsed()));
        // Kept under every name it read, and what each said: an answer, and a
        // wait too -- under the name it waited for, as undefined -- so that the
        // next build, asking in the same round, is told to wait without the
        // macro being run.
        let current = |n: &str| {
            scope
                .visible
                .iter()
                .rev()
                .find(|(k, _)| k == n)
                .map(|(_, d)| d.clone())
        };
        let reads = read.iter().map(|n| (n.clone(), current(n))).collect();
        self.answers
            .borrow_mut()
            .entry(key)
            .or_default()
            .push((reads, out.clone()));
        self.learned.set(true);
        Ok(out)
    }
}

impl Drop for Macros<'_> {
    fn drop(&mut self) {
        self.keep();
    }
}

/// What a file of kept answers starts with, before the fingerprint of the
/// packages that answered them.
const ANSWERS_MAGIC: &[u8] = b"meadow-macro-answers-1\n";

impl Macros<'_> {
    /// A run on the CEK machine: the call built as a term and evaluated.
    fn run_on_cek(
        &self,
        f: core::Var,
        input: &[tt::TokenTree],
        at: Span,
        scope: &Scope<'_>,
    ) -> Result<(Outcome, Vec<String>), Failure> {
        let from_array = self
            .std_fn(&["Collections", "Vector"], "fromArray")
            .ok_or("the standard library has no `Vector.fromArray`")?;
        let expanding = self
            .std_fn(&["Macro"], "expanding")
            .ok_or("the standard library has no `Macro.expanding`")?;
        let mut enc = Encode {
            from_array,
            ctor: &|bare| self.ctor(bare),
            spans: true,
        };
        let argument = enc.trees(input)?;
        // Later bindings shadow earlier ones, and the handler takes the first
        // it finds: so the list goes in latest first.
        let bound: Vec<core::Term> = scope
            .visible
            .iter()
            .rev()
            .map(|(n, d)| Ok(core::Term::Tuple(vec![text(n), enc.datum(d)?])))
            .collect::<Result<_, Failure>>()?;
        let bound = core::Term::App(
            Arc::new(core::Term::Var(from_array)),
            Arc::new(core::Term::Array(bound, unknown())),
        );
        // `expanding settled scope macro argument`: the macro run under the
        // handler of its `Expand` effect, answering in a `Wire` -- arrays all
        // the way down, which is what can be read here without knowing how a
        // `[a]` is laid out.
        let call = [
            core::Term::Lit(core::Lit::Bool(scope.settled)),
            bound,
            core::Term::Var(f),
            argument,
        ]
        .into_iter()
        .fold(core::Term::Var(expanding), |g, x| {
            core::Term::App(Arc::new(g), Arc::new(x))
        });
        let value = meadow_eval::eval_with_fuel(&self.program(), Arc::new(call), self.fuel)
            .map_err(|e| Failure::from(e.msg))?;
        outcome(&wire(&value)?, at)
    }
}

/// Say how long a macro took, and where, when `MEADOW_MACRO_TIMES` is set.
fn timed(said: impl FnOnce() -> String) {
    if std::env::var_os("MEADOW_MACRO_TIMES").is_some() {
        eprintln!("macro: {}", said());
    }
}

/// How long a macro may run on the VM, for a budget of `fuel` of the CEK
/// machine's steps: about what the CEK machine would take for them, and a
/// second at least.
fn budget(fuel: u64) -> std::time::Duration {
    std::time::Duration::from_millis((fuel / 5_000).max(1_000))
}

// --- the call and its answer, as text ---------------------------------------------------
//
// What `Std.Macro.serve` reads and writes: a `Wire` written out. See it there.

/// What `serve` is handed: whether the round is settled, the bindings the call
/// can see -- latest first, since the first one found is the one that counts
/// -- and the argument, each in the shape `Std.Macro`'s `wireTree` and
/// `wireDatum` write.
fn call_wire(scope: &Scope<'_>, input: &[tt::TokenTree]) -> Wire {
    let bound = scope
        .visible
        .iter()
        .rev()
        .map(|(n, d)| Wire::Node(n.to_string(), vec![datum_wire(d)]))
        .collect();
    Wire::Node(
        String::new(),
        vec![
            Wire::Whole(scope.settled as i64),
            Wire::Node(String::new(), bound),
            trees_wire(input, true),
        ],
    )
}

fn trees_wire(trees: &[tt::TokenTree], spans: bool) -> Wire {
    Wire::Node(
        String::new(),
        trees.iter().map(|t| tree_wire(t, spans)).collect(),
    )
}

/// A token tree, as `Encode::tree` builds one: a keyword is a word, anything
/// else not a name, literal or group is punctuation by its text.
fn tree_wire(tree: &tt::TokenTree, spans: bool) -> Wire {
    use meadow_compiler::lexer::Token;
    let node = |tag: &str, fields: Vec<Wire>| Wire::Node(tag.to_string(), fields);
    let (tag, mut fields) = match tree {
        tt::TokenTree::Group(g) => {
            let opening = match g.delim {
                tt::Delim::Paren => "(",
                tt::Delim::Brack => "[",
                tt::Delim::Brace => "{",
            };
            (
                "Group",
                vec![Wire::Text(opening.into()), trees_wire(&g.trees, spans)],
            )
        }
        tt::TokenTree::Token(t) => match t.value() {
            Token::LowerIdent(s) | Token::UpperIdent(s) => {
                ("Word", vec![Wire::Text(s.to_string())])
            }
            Token::String(s) => ("Str", vec![Wire::Text(s.to_string())]),
            Token::Char(c) => ("Chr", vec![Wire::Text(c.to_string())]),
            Token::Int(n) => ("Num", vec![Wire::Whole(*n)]),
            Token::Real(bits) => ("Real", vec![Wire::Frac(f64::from_bits(*bits))]),
            other => {
                let written = other.text();
                let word = written.starts_with(|c: char| c.is_alphabetic());
                (
                    if word { "Word" } else { "Punct" },
                    vec![Wire::Text(written.to_string())],
                )
            }
        },
    };
    let span = match tree {
        tt::TokenTree::Group(g) => Span::new(g.open.start, g.close.end),
        tt::TokenTree::Token(t) => t.span,
    };
    fields.push(if spans {
        node(
            "At",
            vec![Wire::Whole(span.start as i64), Wire::Whole(span.end as i64)],
        )
    } else {
        node("Nowhere", Vec::new())
    });
    node(tag, fields)
}

/// A binding's value, as `wireDatum` writes one. Its code stands `Nowhere`:
/// it was written in whatever file defined it.
fn datum_wire(d: &Datum) -> Wire {
    let node = |tag: &str, fields: Vec<Wire>| Wire::Node(tag.to_string(), fields);
    match d {
        Datum::Sym(s) => node("Sym", vec![Wire::Text(s.to_string())]),
        Datum::Str(s) => node("Str", vec![Wire::Text(s.to_string())]),
        Datum::Int(n) => node("Int", vec![Wire::Whole(*n)]),
        Datum::Float(f) => node("Float", vec![Wire::Frac(*f)]),
        Datum::Bool(b) => node("Bool", vec![Wire::Whole(*b as i64)]),
        Datum::List(ds) => node("List", ds.iter().map(datum_wire).collect()),
        Datum::Rec(fs) => node(
            "Rec",
            fs.iter()
                .map(|(k, v)| Wire::Node(k.to_string(), vec![datum_wire(v)]))
                .collect(),
        ),
        Datum::Tag(t, ds) => node(
            "Tag",
            vec![
                Wire::Text(t.to_string()),
                Wire::Node(String::new(), ds.iter().map(datum_wire).collect()),
            ],
        ),
        Datum::Code(ts) => node("Code", vec![trees_wire(ts, false)]),
    }
}

/// `w` as text. A float goes as an integer and a power of two, exactly.
fn write_wire(w: &Wire, out: &mut String) {
    use std::fmt::Write;
    match w {
        Wire::Text(s) => {
            let _ = write!(out, "T{}:{s}", s.len());
        }
        Wire::Whole(n) => {
            let _ = write!(out, "W{n};");
        }
        Wire::Frac(x) => {
            let (m, e) = mantissa_exponent(*x);
            let _ = write!(out, "F{m}e{e};");
        }
        Wire::Node(tag, xs) => {
            let _ = write!(out, "N{}:{tag}{};", tag.len(), xs.len());
            for x in xs {
                write_wire(x, out);
            }
        }
    }
}

/// `x` as `m * 2^e` with `m` an integer: exactly, for every finite float.
/// What is not finite has no such form, and goes as zero.
fn mantissa_exponent(x: f64) -> (i64, i64) {
    if !x.is_finite() {
        return (0, 0);
    }
    let bits = x.to_bits();
    let sign = if bits >> 63 == 1 { -1 } else { 1 };
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = (bits & ((1 << 52) - 1)) as i64;
    let (m, e) = if exp == 0 {
        (frac, -1074)
    } else {
        (frac | (1 << 52), exp - 1075)
    };
    (sign * m, e)
}

/// The `Wire` written at byte `*at` of `s`, as `serve` writes one -- a float
/// as `show` writes it -- moving `at` past it.
fn read_wire(s: &str, at: &mut usize) -> Option<Wire> {
    let b = s.as_bytes();
    let upto = |from: usize, c: u8| (from..b.len()).find(|&i| b[i] == c);
    let tag = *b.get(*at)?;
    let from = *at + 1;
    match tag {
        b'T' => {
            let colon = upto(from, b':')?;
            let n: usize = s.get(from..colon)?.parse().ok()?;
            let text = s.get(colon + 1..colon + 1 + n)?.to_string();
            *at = colon + 1 + n;
            Some(Wire::Text(text))
        }
        b'W' => {
            let semi = upto(from, b';')?;
            let n = s.get(from..semi)?.parse().ok()?;
            *at = semi + 1;
            Some(Wire::Whole(n))
        }
        b'F' => {
            let semi = upto(from, b';')?;
            let x = s.get(from..semi)?.parse().ok()?;
            *at = semi + 1;
            Some(Wire::Frac(x))
        }
        b'N' => {
            let colon = upto(from, b':')?;
            let n: usize = s.get(from..colon)?.parse().ok()?;
            let name = s.get(colon + 1..colon + 1 + n)?.to_string();
            let semi = upto(colon + 1 + n, b';')?;
            let count: usize = s.get(colon + 1 + n..semi)?.parse().ok()?;
            *at = semi + 1;
            let mut kids = Vec::with_capacity(count);
            for _ in 0..count {
                kids.push(read_wire(s, at)?);
            }
            Some(Wire::Node(name, kids))
        }
        _ => None,
    }
}

/// Building the argument: token trees as a term the evaluator can run.
struct Encode<'a> {
    from_array: core::Var,
    ctor: &'a dyn Fn(&str) -> Option<InternedString>,
    /// Whether tokens go in where they were written. A binding's code does
    /// not: it was written in whatever file defined it, and where it is spliced
    /// is somewhere else, so it stands `Nowhere` -- at the call that uses it.
    spans: bool,
}

impl Encode<'_> {
    /// `fromArray #[…]` -- the only way to build a `[a]` without knowing how
    /// one is laid out.
    fn trees(&mut self, trees: &[tt::TokenTree]) -> Result<core::Term, String> {
        let items: Result<Vec<core::Term>, String> = trees.iter().map(|t| self.tree(t)).collect();
        Ok(core::Term::App(
            Arc::new(core::Term::Var(self.from_array)),
            Arc::new(core::Term::Array(items?, unknown())),
        ))
    }

    fn tree(&mut self, tree: &tt::TokenTree) -> Result<core::Term, String> {
        use meadow_compiler::lexer::Token;
        let (ctor, mut fields) = match tree {
            tt::TokenTree::Group(g) => {
                let delim = self.delim(g.delim)?;
                let inner = self.trees(&g.trees)?;
                ("Group", vec![delim, inner])
            }
            tt::TokenTree::Token(t) => match t.value() {
                Token::LowerIdent(s) | Token::UpperIdent(s) => ("Word", vec![text(&s.to_string())]),
                Token::String(s) => ("Str", vec![text(&s.to_string())]),
                Token::Char(c) => ("Chr", vec![text(&c.to_string())]),
                Token::Int(n) => ("Num", vec![core::Term::Lit(core::Lit::Int(*n))]),
                Token::Real(bits) => (
                    "Real",
                    vec![core::Term::Lit(core::Lit::Float(f64::from_bits(*bits)))],
                ),
                // A keyword is a word: `data` and `fun` are spelled like any
                // other name, and a macro reading a declaration wants them
                // that way. Everything else is punctuation, by the text it is
                // written with.
                other => {
                    let written = other.text();
                    let word = written.starts_with(|c: char| c.is_alphabetic());
                    (if word { "Word" } else { "Punct" }, vec![text(&written)])
                }
            },
        };
        let span = match tree {
            tt::TokenTree::Group(g) => Span::new(g.open.start, g.close.end),
            tt::TokenTree::Token(t) => t.span,
        };
        fields.push(self.loc(span)?);
        self.build(&format!("TokenTree.{ctor}"), fields)
    }

    /// Where a token was written, as a `Std.Macro.Loc`.
    fn loc(&mut self, span: Span) -> Result<core::Term, String> {
        if !self.spans {
            return self.build("Loc.Nowhere", Vec::new());
        }
        let at = |n: u32| core::Term::Lit(core::Lit::Int(n as i64));
        self.build("Loc.At", vec![at(span.start), at(span.end)])
    }

    fn delim(&mut self, d: tt::Delim) -> Result<core::Term, String> {
        let name = match d {
            tt::Delim::Paren => "Paren",
            tt::Delim::Brack => "Bracket",
            tt::Delim::Brace => "Brace",
        };
        self.build(&format!("Delim.{name}"), Vec::new())
    }

    /// A compile-time binding's value, as the `Std.Macro.Datum` it is.
    fn datum(&mut self, d: &Datum) -> Result<core::Term, String> {
        let (ctor, fields) = match d {
            Datum::Sym(s) => ("Sym", vec![text(s)]),
            Datum::Str(s) => ("Str", vec![text(s)]),
            Datum::Int(n) => ("Int", vec![core::Term::Lit(core::Lit::Int(*n))]),
            Datum::Float(f) => ("Float", vec![core::Term::Lit(core::Lit::Float(*f))]),
            Datum::Bool(b) => ("Bool", vec![core::Term::Lit(core::Lit::Bool(*b))]),
            Datum::List(ds) => ("List", vec![self.vector(ds)?]),
            Datum::Rec(fs) => {
                let fields: Vec<core::Term> = fs
                    .iter()
                    .map(|(k, v)| Ok(core::Term::Tuple(vec![text(k), self.datum(v)?])))
                    .collect::<Result<_, String>>()?;
                ("Rec", vec![self.array(fields)])
            }
            Datum::Tag(t, ds) => ("Tag", vec![text(t), self.vector(ds)?]),
            Datum::Code(ts) => {
                let was = std::mem::replace(&mut self.spans, false);
                let trees = self.trees(ts);
                self.spans = was;
                ("Code", vec![trees?])
            }
        };
        self.build(&format!("Datum.{ctor}"), fields)
    }

    /// `[…]` of datums.
    fn vector(&mut self, ds: &[Datum]) -> Result<core::Term, String> {
        let items = ds
            .iter()
            .map(|d| self.datum(d))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.array(items))
    }

    /// `fromArray #[…]`.
    fn array(&self, items: Vec<core::Term>) -> core::Term {
        core::Term::App(
            Arc::new(core::Term::Var(self.from_array)),
            Arc::new(core::Term::Array(items, unknown())),
        )
    }

    fn build(&mut self, spelled: &str, fields: Vec<core::Term>) -> Result<core::Term, String> {
        let name = (self.ctor)(spelled)
            .ok_or_else(|| format!("the standard library has no `{spelled}`"))?;
        Ok(core::Term::Ctor(name, unknown(), fields))
    }
}

fn text(s: &str) -> core::Term {
    core::Term::Lit(core::Lit::Str(InternedString::from(s)))
}

/// The type a constructor builds. Nothing reads it: this term is evaluated, not
/// checked -- it was built here rather than written by anyone.
fn unknown() -> core::Ty {
    meadow_compiler::infer::Type::Error
}

/// `Std.Macro.Wire`: what a run answers with, read out of the evaluator's
/// values. Arrays all the way down, so nothing here has to know how a `[a]`
/// is laid out.
enum Wire {
    Node(String, Vec<Wire>),
    Text(String),
    Whole(i64),
    Frac(f64),
}

fn wire(value: &meadow_eval::Value) -> Result<Wire, String> {
    let bad = || "it did not answer with tokens".to_string();
    let meadow_eval::Value::Ctor(name, fields) = value else {
        return Err(bad());
    };
    let spelled = meadow_compiler::hir::spelling(&name.to_string()).to_string();
    let field = |i: usize| fields.get(i).ok_or_else(bad);
    match spelled.rsplit('.').next().unwrap_or_default() {
        "Node" => {
            let meadow_eval::Value::Str(tag) = field(0)? else {
                return Err(bad());
            };
            let meadow_eval::Value::Array(items) = field(1)? else {
                return Err(bad());
            };
            Ok(Wire::Node(
                tag.to_string(),
                items.iter().map(wire).collect::<Result<_, _>>()?,
            ))
        }
        "Text" => match field(0)? {
            meadow_eval::Value::Str(s) => Ok(Wire::Text(s.to_string())),
            _ => Err(bad()),
        },
        "Whole" => match field(0)? {
            meadow_eval::Value::Int(n) => Ok(Wire::Whole(*n)),
            _ => Err(bad()),
        },
        "Frac" => match field(0)? {
            meadow_eval::Value::Float(f) => Ok(Wire::Frac(*f)),
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

/// The children of a node, whatever it is called.
fn children(w: &Wire) -> Result<&[Wire], String> {
    match w {
        Wire::Node(_, xs) => Ok(xs),
        _ => Err("a list was expected".to_string()),
    }
}

fn wire_text(w: &Wire) -> Result<String, String> {
    match w {
        Wire::Text(s) => Ok(s.clone()),
        _ => Err("text was expected".to_string()),
    }
}

/// How the run ended.
/// How a run ended, and every name it read: for an answer, what it says it
/// read; for a wait, what it read before it, and the name it waited for.
fn outcome(w: &Wire, at: Span) -> Result<(Outcome, Vec<String>), Failure> {
    match w {
        Wire::Node(tag, xs) if tag == "Answered" && xs.len() == 3 => {
            let trees = decode_trees(&xs[0], at)?;
            let defined = children(&xs[1])?
                .iter()
                .map(|d| match d {
                    Wire::Node(name, v) if v.len() == 1 => Ok((name.clone(), datum(&v[0], at)?)),
                    _ => Err(Failure::from("a definition without a value")),
                })
                .collect::<Result<_, Failure>>()?;
            let read: Vec<String> = children(&xs[2])?
                .iter()
                .map(wire_text)
                .collect::<Result<_, String>>()?;
            Ok((
                Outcome::Answered {
                    trees,
                    defined,
                    read: read.clone(),
                },
                read,
            ))
        }
        Wire::Node(tag, xs) if tag == "Waiting" && !xs.is_empty() => {
            let name = wire_text(&xs[0])?;
            let mut read: Vec<String> = match xs.get(1) {
                Some(rs) => children(rs)?
                    .iter()
                    .map(wire_text)
                    .collect::<Result<_, String>>()?,
                None => Vec::new(),
            };
            read.push(name.clone());
            Ok((Outcome::Waiting(name), read))
        }
        _ => Err(Failure::from("it did not answer with tokens")),
    }
}

/// A `Datum`, read back.
fn datum(w: &Wire, at: Span) -> Result<Datum, Failure> {
    let Wire::Node(tag, xs) = w else {
        return Err(Failure::from("a value that is not a `Datum`"));
    };
    let one = || {
        xs.first()
            .ok_or_else(|| format!("`{tag}` with nothing in it"))
    };
    Ok(match tag.as_str() {
        "Sym" => Datum::Sym(wire_text(one()?)?),
        "Str" => Datum::Str(wire_text(one()?)?),
        "Int" => match one()? {
            Wire::Whole(n) => Datum::Int(*n),
            _ => return Err(Failure::from("`Int` of something that is not a number")),
        },
        "Float" => match one()? {
            Wire::Frac(f) => Datum::Float(*f),
            _ => return Err(Failure::from("`Float` of something that is not a number")),
        },
        "Bool" => Datum::Bool(matches!(one()?, Wire::Whole(n) if *n != 0)),
        "List" => Datum::List(xs.iter().map(|x| datum(x, at)).collect::<Result<_, _>>()?),
        "Rec" => Datum::Rec(
            xs.iter()
                .map(|f| match f {
                    Wire::Node(k, v) if v.len() == 1 => Ok((k.clone(), datum(&v[0], at)?)),
                    _ => Err(Failure::from("a field without a value")),
                })
                .collect::<Result<_, Failure>>()?,
        ),
        "Tag" if xs.len() == 2 => Datum::Tag(
            wire_text(&xs[0])?,
            children(&xs[1])?
                .iter()
                .map(|x| datum(x, at))
                .collect::<Result<_, _>>()?,
        ),
        "Code" => Datum::Code(decode_trees(one()?, at)?),
        other => return Err(Failure::from(format!("`{other}` is not a `Datum`"))),
    })
}

/// Token trees, read back. `Code` is text, which is lexed here -- that is the
/// point of it -- so one tree can be several.
fn decode_trees(w: &Wire, at: Span) -> Result<Vec<tt::TokenTree>, Failure> {
    let items = children(w)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        // An interpolated string is several tokens -- its text up to each hole,
        // the hole's own tokens, and on to its end -- none of which is Meadow
        // alone. Lexed one at a time, `"a ${` said its `${` was never closed:
        // so the whole literal, holes and all, is written out and lexed once.
        if piece(&items[i]) == Some(Piece::Start) {
            let (text, end) = interpolated(items, i)?;
            let first = loc_of(&items[i]).unwrap_or(at);
            let last = loc_of(&items[end]).unwrap_or(first);
            out.extend(lex(
                &text,
                Span::new(first.start, last.end.max(first.start)),
            )?);
            i = end + 1;
        } else {
            out.extend(decode(&items[i], at)?);
            i += 1;
        }
    }
    Ok(out)
}

/// The pieces of an interpolated string, as a macro is given them: each a
/// `Punct` of the text the lexer writes it back with.
#[derive(PartialEq)]
enum Piece {
    /// `"…${`
    Start,
    /// `}…${`
    Mid,
    /// `}…"`
    End,
}

fn piece(w: &Wire) -> Option<Piece> {
    let Wire::Node(tag, xs) = w else { return None };
    if tag != "Punct" {
        return None;
    }
    let text = wire_text(xs.first()?).ok()?;
    if text.len() < 2 {
        return None;
    }
    match (
        text.starts_with('"'),
        text.starts_with('}'),
        text.ends_with("${"),
    ) {
        (true, _, true) => Some(Piece::Start),
        (_, true, true) => Some(Piece::Mid),
        (_, true, false) if text.ends_with('"') => Some(Piece::End),
        _ => None,
    }
}

/// The interpolated string starting at `items[start]`, written out whole, and
/// the index of its last piece -- the `End` that closes it, past any string
/// nested in one of its holes.
fn interpolated(items: &[Wire], start: usize) -> Result<(String, usize), Failure> {
    let mut depth = 0usize;
    let mut text = String::new();
    for (j, item) in items.iter().enumerate().skip(start) {
        match piece(item) {
            Some(Piece::Start) => depth += 1,
            Some(Piece::End) => depth -= 1,
            _ => {}
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&source_text(item)?);
        if depth == 0 {
            return Ok((text, j));
        }
    }
    Err(Failure::from("an interpolated string that does not end"))
}

/// A token tree of an answer, written back as Meadow: what `lex` reads.
fn source_text(w: &Wire) -> Result<String, Failure> {
    use meadow_compiler::lexer::Token;
    let Wire::Node(tag, xs) = w else {
        return Err(Failure::from("it did not answer with tokens"));
    };
    let first = || {
        xs.first()
            .ok_or_else(|| Failure::from(format!("`{tag}` with nothing in it")))
    };
    Ok(match tag.as_str() {
        "Word" | "Punct" | "Code" => wire_text(first()?)?,
        "Str" => Token::String(InternedString::from(wire_text(first()?)?)).text(),
        "Chr" => match wire_text(first()?)?.chars().next() {
            Some(c) => Token::Char(c).text(),
            None => return Err(Failure::from("a character has to be one character")),
        },
        "Num" => match first()? {
            Wire::Whole(n) => n.to_string(),
            _ => {
                return Err(Failure::from(
                    "`Num` was given something that is not a number",
                ));
            }
        },
        "Real" => match first()? {
            Wire::Frac(f) => Token::Real(f.to_bits()).text(),
            _ => {
                return Err(Failure::from(
                    "`Real` was given something that is not a number",
                ));
            }
        },
        "Group" if xs.len() >= 2 => {
            let open = wire_text(&xs[0])?;
            let close = match open.as_str() {
                "(" => ")",
                "[" => "]",
                "{" => "}",
                _ => return Err(Failure::from("a group with no bracket")),
            };
            let inner: Result<Vec<String>, Failure> =
                children(&xs[1])?.iter().map(source_text).collect();
            format!("{open}{}{close}", inner?.join(" "))
        }
        other => return Err(Failure::from(format!("`{other}` is not a token"))),
    })
}

/// Where a token of an answer was written, if it says.
fn loc_of(w: &Wire) -> Option<Span> {
    match w {
        Wire::Node(_, xs) => xs.last().and_then(written),
        _ => None,
    }
}

fn decode(w: &Wire, at: Span) -> Result<Vec<tt::TokenTree>, Failure> {
    use meadow_compiler::lexer::{LToken, Token};
    let Wire::Node(tag, xs) = w else {
        return Err(Failure::from("it did not answer with tokens"));
    };
    let first = || {
        xs.first()
            .ok_or_else(|| format!("`{tag}` with nothing in it"))
    };
    // Where the token stands: where it was written, or -- for one the macro
    // made up -- where the call is.
    let placed = |i: usize| xs.get(i).and_then(written).unwrap_or(at);
    let one = |t: Token, i: usize| Ok(vec![tt::TokenTree::Token(LToken::new(t, placed(i)))]);
    match tag.as_str() {
        // A word is whatever the lexer makes of it: a keyword is a keyword,
        // and an identifier's case says which kind it is.
        "Word" | "Punct" => Ok(lex(&wire_text(first()?)?, placed(1))?),
        "Code" => Ok(lex(&wire_text(first()?)?, at)?),
        "Str" => one(Token::String(InternedString::from(wire_text(first()?)?)), 1),
        "Chr" => {
            let s = wire_text(first()?)?;
            let mut cs = s.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => one(Token::Char(c), 1),
                _ => Err(Failure::from("a character has to be one character")),
            }
        }
        "Num" => match first()? {
            Wire::Whole(n) => one(Token::Int(*n), 1),
            _ => Err(Failure::from(
                "`Num` was given something that is not a number",
            )),
        },
        "Real" => match first()? {
            Wire::Frac(f) => one(Token::Real(f.to_bits()), 1),
            _ => Err(Failure::from(
                "`Real` was given something that is not a number",
            )),
        },
        // Not tokens: what the macro has to say about what it was given, and
        // where -- the token that was wrong, when it said one.
        "Fail" => Err(Failure {
            msg: wire_text(first()?)?,
            at: xs.get(1).and_then(written),
        }),
        "Group" if xs.len() >= 2 => {
            let delim = match wire_text(&xs[0])?.as_str() {
                "(" => tt::Delim::Paren,
                "[" => tt::Delim::Brack,
                "{" => tt::Delim::Brace,
                _ => return Err(Failure::from("a group with no bracket")),
            };
            let span = placed(2);
            Ok(vec![tt::TokenTree::Group(tt::Group {
                delim,
                trees: decode_trees(&xs[1], at)?,
                open: Span::new(span.start, (span.start + 1).min(span.end)),
                close: Span::new(span.end.saturating_sub(1).max(span.start), span.end),
            })])
        }
        other => Err(Failure::from(format!("`{other}` is not a token"))),
    }
}

/// A `Loc`, read back: `Some` of where a token was written, `None` for one
/// written `Nowhere`.
fn written(w: &Wire) -> Option<Span> {
    match w {
        Wire::Node(tag, xs) if tag == "At" => match (xs.first(), xs.get(1)) {
            (Some(Wire::Whole(a)), Some(Wire::Whole(b))) if *a >= 0 && *b >= *a => {
                Some(Span::new(*a as u32, *b as u32))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `text` as tokens, lexed the way a file is.
fn lex(text: &str, at: Span) -> Result<Vec<tt::TokenTree>, String> {
    use meadow_compiler::source::{Source, SourceKind};
    let src = Source::new(SourceKind::Interactive, InternedString::from(text));
    let lexed = meadow_compiler::lexer::tokenize(src);
    if let Some(e) = lexed.errors.first() {
        return Err(format!("`{text}` is not Meadow: {}", e.msg));
    }
    // Every token stands where the call is: it was not written anywhere else.
    let moved: Vec<meadow_compiler::lexer::LToken> = lexed
        .tokens
        .iter()
        .map(|t| meadow_compiler::lexer::LToken::new(t.value().clone(), at))
        .collect();
    let (trees, errs) = tt::trees(&moved, "a macro");
    if let Some(e) = errs.first() {
        return Err(format!("`{text}` does not balance: {}", e.msg));
    }
    Ok(trees)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> (InternedString, InternedString, Span) {
        ("Maker".into(), "defineOne".into(), Span::new(10, 22))
    }

    /// A runner holding no packages can run nothing: whatever it answers, it
    /// answered from what an earlier one kept.
    fn empty() -> Macros<'static> {
        Macros::new(Vec::new())
    }

    #[test]
    fn what_a_macro_answered_is_kept_for_the_next_build_under_the_same_packages() {
        let dir = std::env::temp_dir().join(format!("meadow-macro-answers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("App.macros");
        let (package, name, at) = call();
        let scope = Scope {
            visible: &[],
            settled: false,
        };
        let answered = Outcome::Answered {
            trees: Vec::new(),
            defined: Vec::new(),
            read: Vec::new(),
        };
        {
            let first = empty().remembering(path.clone(), 7);
            first.recall();
            first.answers.borrow_mut().insert(
                (package, name, tt::render(&[]), false, at),
                vec![(Vec::new(), answered.clone())],
            );
            first.learned.set(true);
        }
        let again = empty().remembering(path.clone(), 7);
        assert_eq!(
            again.run(package, name, &[], at, &scope).ok(),
            Some(answered)
        );
        // Other packages: nothing kept is theirs, and there is nothing to run.
        let other = empty().remembering(path, 8);
        assert!(other.run(package, name, &[], at, &scope).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
