//! `core` → AxCut.
//!
//! The translation is the classical one: **a value is returned by invoking a
//! continuation**. A continuation is codata with a single method, so returning
//! needs no mechanism of its own —
//!
//! ```text
//!   substitute [k, v] in {(k, v) => invoke k#0}
//! ```
//!
//! — and a function is codata too, with one method taking an argument and the
//! continuation to answer with. Calling and returning are one operation seen
//! from its two sides, which is the duality the IR is named for.
//!
//! Everything in `core` is covered. [`Unsupported`] has one variant left, for a
//! variable that is neither in scope nor a definition, which is a bug upstream
//! rather than a gap here.
//!
//! # The environment is positional, and this pass has to know its shape
//!
//! This is the part that makes the IR worth having and the pass awkward to
//! write. A block's parameters are not "the new bindings" — they name *the whole
//! environment* at that point, in order. So the translation cannot emit a
//! statement without knowing exactly what is live and in what order, and every
//! function here threads that list explicitly.
//!
//! The payoff is that nothing downstream has to reconstruct it. `substitute` is
//! literally the move sequence; `jump` and `invoke` carry no arguments because
//! there is nothing left to pass. A register allocator reads its answer off the
//! IR instead of computing liveness.
//!
//! The price is paid here, in [`Lower::bind`], which must be handed the set of
//! names that survive the subexpression it is sequencing — and that set is
//! computed from the free variables of everything still to come.
//!
//! # Definitions are labels, and so are `letrec` bindings
//!
//! A reference to a global lowers to `substitute [k] in {(k) => jump L}`: `jump`
//! leaves the environment alone, and the definition's block takes exactly the
//! continuation, so the two line up with no shuffling at all. Recursion needs no
//! back-patching, which is the reason to do it this way.
//!
//! `letrec` is the same mechanism with the free variables made explicit — plain
//! lambda lifting. A group's bindings share one parameter list, the variables
//! they capture from the enclosing scope, and a reference to one becomes
//! `substitute [a, b, k] in {(a, b, k) => jump L}`. Mutual recursion then costs
//! nothing extra, and no object has to point at itself, which is what a
//! closure-based encoding would need and which no `Rc` graph should have to do.
//!
//! It does mean such a binding is re-evaluated at every reference rather than
//! once, the way the CEK machine does it. For a right-hand side that is a lambda
//! or a literal — nearly all of them — that is unobservable. For one that
//! performs an effect it is not, and fixing it means giving each definition a
//! memoising thunk. See the note in [`crate::machine`].
//!
//! # Pattern matching
//!
//! `match` compiles to a chain of failure continuations: one codata object per
//! arm, each capturing the next, and a pattern that fails invokes it. That is
//! backtracking, not a decision tree — arms can retest what an earlier arm
//! already tested, so a wide `match` on one scrutinee does more work than it
//! needs to. It is correct and it is small.
//!
//! At [`OptLevel::O2`] a `match` whose patterns are constructors, literals,
//! tuples and variables -- nearly all of them -- is a decision tree instead,
//! [`Lower::case_matrix`]: every value is tested once, nothing is built to
//! backtrack with, and each `switch` can consume what it takes apart, which is
//! what lets the reference-counted backend build in the blocks a match took
//! apart. An arm reached from more than one leaf is copied to each while it is
//! small and becomes a label they jump to when it is not. What the tree does not
//! take -- arrays, records, a tree too big -- falls to [`Lower::case_tree`], one
//! `switch` over a prefix of distinct constructors, and then to the chain. It is
//! gated because a decision tree is a code-size trade in general, and because
//! the chain is what the arms fall back to, so all of them have to keep working.
//!
//! # Where this departs from the paper
//!
//! The paper's environment is **linear**: `let` and `new` consume the prefix
//! they build from. Here they prepend and leave the rest alone, so a name may be
//! used twice without an explicit duplication. `substitute` is still the only
//! thing that shrinks the environment, and it appears at every call and return,
//! so environments stay bounded — but the linearity that lets the paper's
//! backend place registers without analysis is not yet enforced.
//!
//! # Effects: evidence passing
//!
//! AxCut has no handlers, and after this pass Meadow's has none either. A
//! `handle` and a `perform` lower to ordinary objects, data and jumps, so every
//! engine below -- the reference machine, the bytecode VM, native code -- runs
//! effects without knowing they exist.
//!
//! **The evidence.** Every function takes one parameter more than it is
//! written with: the handlers installed where it was called, as a list of
//! `#ev(key, clause, target, rest)` entries, newest first. `key` names the
//! operation (`"State.get"`), `clause` is an object whose one method is that
//! operation's clause, and `target` is a `Ref` holding where the whole `handle`
//! expression's value goes. The empty list is a `#evnone` object. A continuation does not
//! take it: it captures the evidence it needs like any other name, which is
//! also why resuming a continuation needs no evidence restored.
//!
//! **`handle body with clauses`** allocates `target = Ref k`, one clause object
//! per clause, and one entry per clause on top of the current evidence, then
//! runs `body` with that as its evidence and a return continuation `H` as its
//! continuation. `H` reads `target` and runs the `return` clause there, under
//! the evidence outside the handler.
//!
//! **`perform E.op x`** searches the evidence for `"E.op"` -- a jump to a small
//! block of its own, [`Lower::perform`] -- and then, with the entry found,
//! builds a one-shot resumption `R` and enters the clause with `x`, `R`, and
//! the value in `target`. With none found, the operation goes to the runtime
//! ([`Extern::Native`]): `Console`, `Fs` and the others the real world answers.
//!
//! **A tail-resumptive clause** -- `op x k -> k e`, which is most of them --
//! is entered with `x` and the performing code's own continuation instead, and
//! answers it with `e`: no resumption, no flag, nothing written. `k e` would
//! have done exactly that and sent what came of it where the clause's value
//! goes, which is the same place.
//!
//! **`resume v`**, called as `R` with continuation `c`, checks and sets `R`'s
//! flag, sets `target := c`, and continues the performing code with `v`. That
//! write is what makes handlers deep and the clause's `resume` return: when the
//! resumed body finishes, `H` reads the resume site out of `target`. It is safe
//! because a resumption runs at most once.
//!
//! Clauses and the `return` clause capture the evidence from outside the
//! handler, so an operation they perform goes past it, as it must.
//!
//! # Types: descriptors
//!
//! A value whose type is a type variable is represented however the variable
//! is instantiated, and something at run time has to say how -- the collector,
//! once values stop saying it themselves. So type abstraction and application,
//! which erase to nothing in most compilers, pass *descriptors* here
//! (`meadow_core::desc`): one per variable over types, none for rows and
//! effects.
//!
//! * **A generic definition** takes its descriptors first, in both of its
//!   blocks; a `letrec` binding takes them after what its group captures.
//! * **A generic `let`** is an object whose one method takes the descriptors
//!   and a continuation and evaluates the right-hand side -- once per
//!   instantiation, which is unobservable because only a pure right-hand side
//!   is generalized.
//! * **An instantiation** `f [T, …]` passes a constant for each known type and
//!   the descriptor in scope for each variable ([`Lower::with_descs`]).
//!
//! Every name's [`Rep::Var`] is the name of its variable's descriptor where it
//! is bound. Liveness here ignores descriptors; `meadow_axcut::describe` then adds
//! each one to every environment holding a value it describes.

use crate::{Block, Def, Extern, Label, NO_DESC, Name, Program, Rep, Role, Statement, Tag};
use meadow_core as core;
use meadow_core::{OptLevel, Pat, Term, Var};
use meadow_hir::VarId;
use meadow_infer::VarKind;
use meadow_intern::InternedString;
use std::collections::{HashMap, HashSet};

/// A `core` construct this pass does not translate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unsupported {
    /// A variable that is neither in scope nor a definition — an unsaturated
    /// primitive operator, most likely, which the front end should have
    /// eta-expanded.
    UnboundVar,
}

pub struct Lowered {
    pub program: Program,
    /// Empty means the program is fully covered.
    pub unsupported: HashSet<Unsupported>,
}

/// `MEADOW_TIME_PASSES=1`: how long each pass over the program took, on
/// stderr, for finding the one that does not scale.
struct Timing(bool);

impl Timing {
    fn from_env() -> Timing {
        Timing(std::env::var_os("MEADOW_TIME_PASSES").is_some())
    }

    fn pass<T>(&self, name: &str, f: impl FnOnce() -> T) -> T {
        let at = std::time::Instant::now();
        let out = f();
        self.since(name, at);
        out
    }

    fn since(&self, name: &str, at: std::time::Instant) {
        if self.0 {
            eprintln!("  {name:<14} {:>10.1?}", at.elapsed());
        }
    }
}

/// Lower a whole program.
///
/// Fresh names continue above the highest [`VarId`] the front end used, so an
/// invented name can never collide with a real one.
pub fn lower_program(program: &core::Program, opt: OptLevel) -> Lowered {
    lower_program_with(program, opt, false)
}

/// [`lower_program`], for a program that may take a new build of itself
/// while it runs, when `reloadable`: its definitions, constructors and
/// effects written with the versions of the types they carry (see
/// `meadow_core::versions`), in [`Program::types`] and
/// [`Program::ctor_types`] and its effects' evidence keys. Silo's
/// interpreter loads a new build by these, and refuses one that has none.
/// Without it, nothing is versioned and nothing is written down: what a
/// program built to run as it is, natively above all, has no use for.
pub fn lower_program_with(program: &core::Program, opt: OptLevel, reloadable: bool) -> Lowered {
    // AxCut is untyped. Core's type abstractions and applications go first,
    // in one pass, so that nothing below has to see through one — see
    // [`core::erase`].
    //
    // Number-generic definitions are copied per number type first, while the
    // types that say which are still there -- see [`core::specialize`].
    // And each top-level value gets its cache, so it is evaluated once -- see
    // [`core::globals`].
    //
    // Types are *not* erased: a name's representation comes from its type, a
    // call's type from the instantiation core wrote down, and an abstraction
    // over types takes descriptors -- see the module docs.
    //
    // Before either, while a call still says exactly which types and which
    // dictionaries it is at: a function that takes dictionaries is copied for
    // the ones it is known to be given -- see [`core::dictionaries`].
    let timing = Timing::from_env();
    let skipped = std::env::var("MEADOW_SKIP_PASSES").unwrap_or_default();
    let skip = |name: &str| skipped.split(',').any(|s| s == name);
    let program = &timing.pass("dictionaries", || core::dictionaries::program(program, opt));
    let specialized = timing.pass("specialize", || {
        if opt.specializes() && !skip("release") {
            core::specialize::release(program)
        } else {
            core::specialize::program(program)
        }
    });
    // `joins` before `globals`: a mention of a global becomes a jump to its
    // definition there, and a join point wants to be found while the calls to
    // it still look like calls. `simplify` between the two, for the same
    // reason on one side -- it needs join points to put a pushed-in context in
    // -- and because on the other a jump to a definition is not a term it can
    // look into.
    let literals = timing.pass("literals", || {
        core::globals::inline_literals(&core::bools::program(&specialized))
    });
    // `MEADOW_SKIP_PASSES=joins,simplify`: leave the named passes out, to
    // measure what each is worth to a program -- of `inline`, `joins`,
    // `simplify`, `lift` and `trmc`.
    let inlined = timing.pass("inline", || {
        if opt.inlines() && !skip("inline") {
            core::inline::program(&literals)
        } else {
            literals.clone()
        }
    });
    // Tail recursion modulo cons after `simplify`, which would otherwise see a
    // cell's placeholder as the value of its field -- see [`core::trmc`].
    // Local functions are lifted to the top level before it, so that a local
    // loop is a definition with a direct entry rather than a closure, and is a
    // candidate for TRMC like any other -- see [`core::lift`].
    let joined = timing.pass("joins", || {
        if skip("joins") {
            inlined.clone()
        } else {
            core::joins::program(&inlined)
        }
    });
    let simplified = timing.pass("simplify", || {
        if skip("simplify") {
            joined.clone()
        } else {
            core::simplify::program(&joined)
        }
    });
    let lifted = timing.pass("lift", || {
        if skip("lift") {
            simplified.clone()
        } else {
            core::lift::program(&simplified, opt)
        }
    });
    let trmc = timing.pass("trmc", || {
        if skip("trmc") {
            lifted.clone()
        } else {
            core::trmc::program(&lifted, opt)
        }
    });
    let program = &timing.pass("globals", || core::globals::program(&trmc));
    let lowering = std::time::Instant::now();
    if let Ok(want) = std::env::var("MEADOW_DUMP_CORE") {
        for (stage, p) in [("before", &literals), ("after", program)] {
            for d in &p.defs {
                if &*d.name.to_string() == want {
                    eprintln!("== {stage} {} {:?}\n{:#?}", d.name, d.poly, d.term);
                }
            }
        }
    }

    let mut globals = HashMap::new();
    for (i, d) in program.defs.iter().enumerate() {
        globals.insert(d.var, (Label(i as u32), Vec::new()));
    }
    let versions = if reloadable {
        timing.pass("versions", || core::versions::Versions::of(program))
    } else {
        core::versions::Versions::default()
    };

    let mut lower = Lower {
        versions: versions.clone(),
        module: InternedString::default(),
        next_name: max_var(program) + 1,
        next_label: program.defs.len() as u32,
        globals,
        workers: HashMap::new(),
        joins: HashMap::new(),
        defs: Vec::new(),
        tags: HashMap::new(),
        next_tag: 0,
        opt,
        unsupported: HashSet::new(),
        returns: HashSet::new(),
        continuations: HashSet::new(),
        frames: HashSet::new(),
        ev: VarId(0),
        letrecs: HashSet::new(),
        worker_ev: HashSet::new(),
        polys: HashMap::new(),
        types: HashMap::new(),
        reps: HashMap::new(),
        variants: program.variants.clone(),
        tscope: Vec::new(),
        tylams: HashMap::new(),
        tyabs: HashMap::new(),
        descs: HashSet::new(),
        binder_reps: HashMap::new(),
        origins: program.origins.clone(),
        results: HashMap::new(),
        threads: HashMap::new(),
        pure: HashSet::new(),
    };
    for d in &program.defs {
        lower.polys.insert(d.var, d.poly.clone());
        if let Some((_, vs, _)) = ty_abs(&d.term) {
            lower.tyabs.insert(d.var, described(vs));
        }
        lower.scan(&d.term);
    }
    lower.ev = lower.fresh_ref();
    // Always a constructor of the program, used or not: a runtime starting a
    // function on a thread of its own has to hand it empty evidence, and
    // finds the tag by name.
    lower.tag_of(InternedString::from(EV_NONE));
    // And so is every record a runtime builds by name -- `Fs.metadata`'s
    // `FileMeta` -- for a program that only reads its fields, and so never
    // names its constructor, to read them by.
    for ctor in RUNTIME_RECORDS {
        lower.tag_of(InternedString::from(*ctor));
    }

    // A definition that is a lambda gets a second entry point, taking its
    // arguments directly. Registered before anything is lowered, so a call can
    // be compiled as a jump to a block that does not exist yet — including a
    // recursive one.
    for d in &program.defs {
        let (params, _) = lam_spine(&d.term);
        if !params.is_empty() {
            let label = lower.fresh_label();
            lower.workers.insert(d.var, (label, params.len()));
        }
    }

    // Which direct entry points need the evidence: none, to begin with, and
    // then every one whose body needs it given the ones found so far, until
    // nothing changes. Only ever adds, so it stops.
    loop {
        let mut changed = false;
        for d in &program.defs {
            if lower.workers.contains_key(&d.var) && !lower.worker_ev.contains(&d.var) {
                let (_, body) = lam_spine(&d.term);
                if lower.needs_ev(body) {
                    lower.worker_ev.insert(d.var);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Each definition is a block of one parameter: the continuation to answer
    // with. That is also exactly the environment a `jump` to it arrives with.
    // A definition takes no evidence: top-level values are pure, so a body
    // that needs some starts from none.
    let mut entry = None;
    for (i, d) in program.defs.iter().enumerate() {
        let label = Label(i as u32);
        lower.module = d.module;
        // A generic definition takes its descriptors first, and its body is
        // what is under the abstraction.
        let (descs, term) = match ty_abs(&d.term) {
            Some((node, _, body)) => (lower.tylams[&address(node)].clone(), body),
            None => (Vec::new(), &d.term),
        };
        let desc_names: Vec<Name> = descs.iter().map(|(_, n)| *n).collect();
        lower.tscope = descs;
        let k = lower.function_return();
        let mut params = desc_names.clone();
        params.push(k);
        if !lower.needs_ev(term) {
            lower.pure.insert(label);
        }
        let body = if lower.needs_ev(term) {
            let ev = lower.fresh_ref();
            lower.ev = ev;
            let mut env = vec![ev];
            env.extend_from_slice(&params);
            let body = lower.expr(term, &env, k);
            Statement::Let {
                name: ev,
                tag: lower.tag_of(InternedString::from(EV_NONE)),
                ctor: InternedString::from(EV_NONE),
                fields: vec![],
                rest: Box::new(body),
            }
        } else {
            lower.expr(term, &params, k)
        };
        lower.defs.push(Def {
            label,
            name: d.name,
            module: d.module,
            block: Block { params, body },
        });
        // An untyped definition -- one built by hand -- says what it answers
        // through its term, as far as that can be read.
        let result = match known(rep_of(&d.poly.ty)) {
            Rep::Unknown => lower
                .type_of(term)
                .map_or(Rep::Unknown, |t| known(rep_of(&t))),
            rep => rep,
        };
        lower.results.insert(label, result);

        // The direct entry point. The block above still exists and is still
        // reached whenever the function is used as a *value* — passed to `map`,
        // partially applied, stored — where a real closure is the only answer.
        if let Some(&(worker, _)) = lower.workers.get(&d.var) {
            let (params, body) = lam_spine(&d.term);
            let k = lower.function_return();
            let ev = lower.fresh_ref();
            lower.ev = ev;
            let mut block_params = desc_names.clone();
            block_params.extend(params);
            block_params.push(k);
            if lower.worker_ev.contains(&d.var) {
                block_params.push(ev);
            } else {
                lower.pure.insert(worker);
            }
            let body = lower.expr(body, &block_params, k);
            lower.defs.push(Def {
                label: worker,
                name: d.name,
                module: d.module,
                block: Block {
                    params: block_params,
                    body,
                },
            });
        }

        lower.tscope.clear();
        if Some(d.var) == program.entry {
            entry = Some(label);
        }
    }

    // A generic entry point -- `def it = compact (Just (\x -> x))` in the REPL -- is
    // started like any other, with only a continuation, so it gets a block that
    // instantiates it at no type in particular.
    if let Some(i) = program
        .defs
        .iter()
        .position(|d| Some(d.var) == program.entry)
        && let Some(tys) = lower.described_args(program.defs[i].var, None)
    {
        let label = lower.fresh_label();
        let k = lower.function_return();
        let body = lower.with_descs(
            &tys,
            vec![k],
            Box::new(move |_, ds, _| {
                let mut sel = ds;
                sel.push(k);
                Statement::Substitute(
                    sel.clone(),
                    Box::new(Block {
                        params: sel,
                        body: Statement::Jump(Label(i as u32)),
                    }),
                )
            }),
        );
        lower.defs.push(Def {
            label,
            name: program.defs[i].name,
            module: program.defs[i].module,
            block: Block {
                params: vec![k],
                body,
            },
        });
        let generic = lower
            .results
            .get(&Label(i as u32))
            .copied()
            .unwrap_or(Rep::Unknown);
        lower.results.insert(label, generic);
        entry = Some(label);
    }

    // After everything else, so the program's own constructors keep the tags
    // they had.
    lower.tag_of(InternedString::from(core::roles::TUPLE));
    for (_, ctor) in MEADOW_ROLES {
        lower.tag_of(InternedString::from(*ctor));
    }

    // Lifted `letrec` blocks are pushed as they are discovered, so a definition
    // arrives after the blocks lifted out of it. Sorting by label puts the
    // table back in the order the labels read, which is what a dump should show.
    let mut defs = lower.defs;
    defs.sort_by_key(|d| d.label);

    let mut reps = lower.reps;
    for (v, poly) in &lower.polys {
        reps.entry(*v)
            .or_insert_with(|| match lower.binder_reps.get(v) {
                Some(rep) => *rep,
                // A definition: a label, never a value.
                None => match rep_of(&poly.ty) {
                    Rep::Var(_) => Rep::Var(NO_DESC),
                    rep => rep,
                },
            });
    }
    let unmet = meadow_axcut::describe::close(&mut defs, &reps, &lower.threads, &lower.descs);
    // A definition is entered knowing only its own descriptors.
    let entered: HashSet<Label> = (0..program.defs.len() as u32)
        .map(Label)
        .chain(lower.workers.values().map(|(l, _)| *l))
        .collect();
    debug_assert!(
        unmet.keys().all(|l| !entered.contains(l)),
        "definitions needing descriptors they are not passed: {unmet:?}"
    );

    // Each definition's type, which its direct entry point has too -- but a
    // local function's, lifted out of the definition it was written in: it
    // is that definition's code, and has no version of its own. Only for a
    // program that may be reloaded.
    let mut types = HashMap::new();
    for (i, d) in program.defs.iter().enumerate() {
        if !reloadable || core::lift::is_lifted(program, d) {
            continue;
        }
        let sig = InternedString::from(core::signature(&d.poly, &versions).as_str());
        types.insert(Label(i as u32), sig);
        if let Some(&(worker, _)) = lower.workers.get(&d.var) {
            types.insert(worker, sig);
        }
    }
    // Each constructor as its type's version: a value made by one build's is
    // told from another's by its tag.
    let ctor_types = program
        .variants
        .iter()
        .filter(|_| reloadable)
        .flat_map(|(ty, vs)| vs.iter().map(move |v| (ty, v)))
        .map(|(ty, v)| {
            let version = versions.types.get(ty).map_or("", String::as_str);
            let sig = core::variant_signature(v);
            (
                v.name,
                InternedString::from(format!("{ty}#{version}{sig}").as_str()),
            )
        })
        .collect();

    timing.since("lower", lowering);
    Lowered {
        program: Program {
            exports: Vec::new(),
            defs,
            entry,
            tags: lower.tags,
            continuations: lower
                .continuations
                .difference(&lower.returns)
                .copied()
                .collect(),
            returns: lower.returns,
            frames: lower.frames,
            ctor_fields: program.ctor_fields.clone(),
            roles: MEADOW_ROLES
                .iter()
                .map(|(role, ctor)| (*role, InternedString::from(*ctor)))
                .collect(),
            origins: program.origins.clone(),
            reps,
            results: lower.results,
            threads: lower.threads,
            pure: lower.pure,
            types,
            ctor_types,
        },
        unsupported: lower.unsupported,
    }
}

/// The highest variable the program mentions, so fresh names can start above it.
fn max_var(program: &core::Program) -> u32 {
    let mut seen = HashSet::new();
    let mut hi = 0;
    for d in &program.defs {
        hi = hi.max(d.var.0);
        seen.clear();
        mentions(&d.term, &mut seen);
        for v in &seen {
            hi = hi.max(v.0);
        }
    }
    hi
}

/// Split a binary primitive's arguments into "one operand, and a literal".
///
/// `n - 1` and `x == 0` are most of what arithmetic in a loop looks like, and a
/// literal operand needs neither a name nor a register — the folded `extern`
/// forms carry it. The literal has to end up on the *right*, which is where it
/// usually already is; a left-hand one is only moved across when the operation
/// does not care (see [`crate::commutes`]).
fn const_operand(p: core::Prim, args: &[Term]) -> Option<(Term, core::Lit)> {
    let [x, y] = args else {
        return None;
    };
    if let Term::Lit(l) = y {
        return Some((x.clone(), l.clone()));
    }
    match x {
        Term::Lit(l) if crate::commutes(p) => Some((y.clone(), l.clone())),
        _ => None,
    }
}

struct Lower {
    /// The module of the definition being lowered, which every block made
    /// out of it is of too.
    module: InternedString,
    next_name: u32,
    next_label: u32,
    /// A definition or a `letrec` binding: where to jump, and the environment
    /// names its block expects before the continuation.
    globals: HashMap<Var, (Label, Vec<Name>)>,
    /// A top-level definition that is a lambda gets a second block: one that
    /// takes its arguments directly instead of returning a closure per argument.
    /// `(label, arity)` — a call with exactly that many arguments becomes a jump.
    workers: HashMap<Var, (Label, usize)>,
    /// A join point in scope: where to jump, and what its block takes before
    /// its own parameters. A [`core::Term::Jump`] is a `substitute` into that
    /// shape and a `jump` -- no object and no invoke, because a join point
    /// never escapes the body it is written in and so is never a value.
    joins: HashMap<Var, (Label, Vec<Name>)>,
    defs: Vec<Def>,
    tags: HashMap<InternedString, Tag>,
    next_tag: Tag,
    /// What the back end is allowed to do beyond the unconditional minimum.
    opt: OptLevel,
    unsupported: HashSet<Unsupported>,
    /// See [`Program::returns`].
    returns: HashSet<Name>,
    /// See [`Program::continuations`].
    continuations: HashSet<Name>,
    /// See [`Program::frames`].
    frames: HashSet<Name>,
    /// The versions of the program's types and effects, when it is built to
    /// take a new build of itself: what its effects' evidence keys carry.
    versions: core::versions::Versions,
    /// The name holding the evidence -- the handlers in scope -- where lowering
    /// has got to. See the module docs.
    ev: Name,
    /// `letrec` bindings, whose blocks take the evidence.
    letrecs: HashSet<Var>,
    /// The definitions whose direct entry point takes the evidence. One that
    /// never performs, handles, or calls anything that might does not, and
    /// neither do calls to it -- so a first-order loop pays nothing for
    /// effects it does not use.
    worker_ev: HashSet<Var>,
    /// Every core variable's type, from where it is bound. Variables are
    /// unique, so this needs no scoping.
    polys: HashMap<Var, core::Poly>,
    /// The type of a name lowering invented to hold a core value, where known.
    types: HashMap<Name, core::Ty>,
    /// See [`Program::reps`]: the names lowering invented.
    reps: HashMap<Name, Rep>,
    /// Constructors' field types, for the fields a pattern binds.
    variants: meadow_infer::VariantEnv,
    /// The descriptors of the type variables in scope where lowering has got
    /// to, innermost last: which name holds each. See the module docs.
    tscope: Vec<(u32, Name)>,
    /// The names each type abstraction binds its descriptors to, by the
    /// abstraction's address in the program being lowered.
    tylams: HashMap<usize, Vec<(u32, Name)>>,
    /// The bindings whose value is a type abstraction taking descriptors, and
    /// which of their type arguments -- positions among the binders -- take
    /// one.
    tyabs: HashMap<Var, Vec<usize>>,
    /// Every name a type abstraction binds a descriptor to.
    descs: HashSet<Name>,
    /// The representation of each variable the program binds, read from its
    /// type in the scope it is bound in.
    binder_reps: HashMap<Var, Rep>,
    /// Names that are copies of another, and which: a specialized
    /// definition's original, for the type a mention of it has.
    origins: HashMap<Var, Var>,
    /// See [`Program::results`].
    results: HashMap<Label, Rep>,
    /// See [`Program::threads`].
    threads: HashMap<Name, Rep>,
    /// See [`Program::pure`].
    pure: HashSet<Label>,
}

/// The constructor of an evidence entry: `#ev(key, clause, target, rest)`.
const EV: &str = core::roles::evidence::ENTRY;

/// The evidence with no handlers in it: an object, so that evidence is always
/// a reference, whatever it holds.
pub const EV_NONE: &str = core::roles::evidence::NONE;

/// The records a runtime builds by their constructor's name: see
/// `meadow_rt::roles::RECORDS`.
pub const RUNTIME_RECORDS: &[&str] = meadow_rt::roles::RECORDS;

/// Meadow's constructor for each role a runtime builds or recognizes by
/// itself (`meadow_rt::roles`): what a native answers (`Result.Ok`,
/// `Maybe.Just`, a `List` or a `Vector` it made), what reading a line gives
/// back, what `show` prints as a sequence. Every program declares them all, and
/// gets a tag for each -- and for a tuple's -- whether or not its own code uses
/// them: a pruned program (see `meadow_core::prune`) may well not, and a
/// runtime that asks for a tag the program never assigned has nothing to build
/// with.
pub const MEADOW_ROLES: &[(Role, &str)] = &[
    (Role::None, "Maybe.None"),
    (Role::Some, "Maybe.Just"),
    (Role::Ok, "Result.Ok"),
    (Role::Err, "Result.Err"),
    (Role::Nil, "List.Nil"),
    (Role::Cons, "List.Cons"),
    (Role::VectorEmpty, "Vector.Empty"),
    (Role::VectorSingle, "Vector.Single"),
    (Role::VectorFull, "Vector.Full"),
    (Role::VNodeLeaf, "VNode.Leaf"),
    (Role::VNodeBranch, "VNode.Branch"),
    (Role::False, "Bool.False"),
    (Role::FileMeta, "FileMeta.FileMeta"),
];

/// The same for a tail-resumptive clause -- `op x k -> k e` -- whose object
/// takes the argument and the performing code's own continuation, and so
/// needs no resumption at all.
const EV_TAIL: &str = core::roles::evidence::TAIL;

/// If a clause only ever resumes, straight away, with a value computed without
/// the resumption -- `op x k -> k e` -- that value.
fn tail_resumptive(c: &core::HClause) -> Option<&Term> {
    let Term::App(f, e) = c.body.peel() else {
        return None;
    };
    if !matches!(f.peel(), Term::Var(v) if *v == c.resume) {
        return None;
    }
    let mut free = HashSet::new();
    core::free_vars_into(e, &mut free);
    (!free.contains(&c.resume)).then_some(&**e)
}

/// The continuation of a [`Lower::bind`]: given the name the value was bound to
/// and the environment that now holds, produce what runs next.
type Then<'a> = Box<dyn FnOnce(&mut Lower, Name, Vec<Name>) -> Statement + 'a>;

/// What to do once a pattern has matched, in the environment it left behind.
type Success<'a> = Box<dyn FnOnce(&mut Lower, Vec<Name>) -> Statement + 'a>;

/// A `case` arm: its pattern, its guard, and its body.
type Arm = (Pat, Option<Term>, Term);

impl Lower {
    fn fresh(&mut self) -> Name {
        let v = VarId(self.next_name);
        self.next_name += 1;
        v
    }

    /// A fresh name holding `rep`.
    fn fresh_as(&mut self, rep: Rep) -> Name {
        let v = self.fresh();
        self.reps.insert(v, rep);
        v
    }

    /// A fresh name holding a heap object: a closure, a continuation, data.
    fn fresh_ref(&mut self) -> Name {
        self.fresh_as(Rep::Ref)
    }

    /// A fresh name for a value of type `ty`, which may not be known.
    fn fresh_typed(&mut self, ty: Option<core::Ty>) -> Name {
        let v = self.fresh();
        self.set_type(v, ty);
        v
    }

    /// Say `n` holds a value of type `ty`, which may not be known.
    fn set_type(&mut self, n: Name, ty: Option<core::Ty>) {
        match ty {
            Some(ty) => {
                let rep = self.resolve(rep_of(&ty));
                self.reps.insert(n, rep);
                self.note_thread(n, &ty);
                self.types.insert(n, ty);
            }
            None => {
                self.reps.entry(n).or_insert(Rep::Unknown);
            }
        }
    }

    /// The type of the value `n` holds, if it is known.
    fn type_of_name(&self, n: Name) -> Option<core::Ty> {
        match self.types.get(&n) {
            Some(ty) => Some(ty.clone()),
            None => self.polys.get(&n).map(|p| p.ty.clone()),
        }
    }

    /// The type of `t`, as far as a walk down its spine can tell -- enough to
    /// know how its value is represented.
    fn type_of(&self, t: &Term) -> Option<core::Ty> {
        use core::Ty;
        Some(match t {
            Term::Loc(_, inner) | Term::TyLam(_, inner) => return self.type_of(inner),
            Term::Var(v) => self.polys.get(v)?.ty.clone(),
            Term::TyApp(f, args) => match f.peel() {
                // A specialized copy, mentioned with the arguments of the
                // definition it is a copy of: its type as that says.
                Term::Var(v) => {
                    let poly = self.polys.get(v)?;
                    match self.origins.get(v).and_then(|o| self.polys.get(o)) {
                        Some(original)
                            if poly.binders.len() != args.len()
                                && original.binders.len() == args.len() =>
                        {
                            original.instantiate(args)
                        }
                        _ => poly.instantiate(args),
                    }
                }
                other => return self.type_of(other),
            },
            Term::Lit(l) => lit_type(l),
            Term::Lam(_, param, body) => Ty::Fun(
                vec![param.clone()],
                Box::new(self.type_of(body).unwrap_or_else(core::unknown)),
                Box::new(Ty::RowEmpty),
            ),
            Term::App(f, _) => match self.type_of(f)? {
                Ty::Fun(params, ret, eff) if params.len() > 1 => {
                    Ty::Fun(params[1..].to_vec(), ret, eff)
                }
                Ty::Fun(_, ret, _) => *ret,
                _ => return None,
            },
            Term::Let(_, _, _, body) | Term::LetRec(_, body) => return self.type_of(body),
            Term::Join { ty, .. } | Term::Jump(_, _, ty) => ty.clone(),
            Term::If(_, then, _) => return self.type_of(then),
            Term::Tuple(items) => Ty::Tuple(
                items
                    .iter()
                    .map(|x| self.type_of(x).unwrap_or_else(core::unknown))
                    .collect(),
            ),
            Term::Proj(t, i) => match self.type_of(t)? {
                Ty::Tuple(items) => items.get(*i)?.clone(),
                _ => return None,
            },
            Term::Array(_, elem) => Ty::Con(InternedString::from("Array"), vec![elem.clone()]),
            // A literal's fields, each at its own type where that is known:
            // a `match` on one -- which is what a record accessor becomes once
            // it is inlined at a literal -- takes its fields' representations
            // from here.
            Term::Record(fields) => Ty::Record(Box::new(fields.iter().rev().fold(
                Ty::RowEmpty,
                |rest, (label, x)| {
                    Ty::RowExtend(
                        *label,
                        Box::new(self.type_of(x).unwrap_or_else(core::unknown)),
                        Box::new(rest),
                    )
                },
            ))),
            Term::Extend(x, label, v) => {
                let rest = match self.type_of(x) {
                    Some(Ty::Record(row)) => *row,
                    _ => Ty::RowEmpty,
                };
                Ty::Record(Box::new(Ty::RowExtend(
                    *label,
                    Box::new(self.type_of(v).unwrap_or_else(core::unknown)),
                    Box::new(rest),
                )))
            }
            Term::Sel(_, _, ty)
            | Term::Ctor(_, ty, _)
            | Term::Case(_, _, ty)
            | Term::Prim(_, _, ty)
            | Term::Perform(_, _, _, ty)
            | Term::Handle { ty, .. } => ty.clone(),
            Term::Error => return None,
        })
    }

    /// The types of constructor `ctor`'s fields, in a value of type `of`.
    ///
    /// Where `of` is not the type `ctor` belongs to -- a release copy's
    /// `#Ref`, or nothing known -- the declaration still says what it can:
    /// its fields at unknown type arguments, so `Computed (Memo k v)` has a
    /// `Memo`, a reference, whatever `k` and `v` are. A field whose type is
    /// one of those arguments is left [`core::unknown`] for the pattern to
    /// say. Matching `Just (Computed m)` on an inlined copy's `Maybe #Ref`
    /// used to leave `m` unknown, which counts nothing: sharing it did
    /// nothing, and the `Memo` it was taken apart as if it were the only
    /// reference to it.
    fn field_types(&self, ctor: InternedString, of: Option<&core::Ty>) -> Option<Vec<core::Ty>> {
        let bare = ctor.rsplit('.').next().unwrap_or(&ctor);
        let find = |name: &InternedString| {
            self.variants
                .get(name)?
                .iter()
                .find(|v| *v.name == *ctor || *v.name == *bare)
        };
        if let Some(core::Ty::Con(name, args)) = of
            && let Some(sig) = find(name)
        {
            return Some(
                sig.fields
                    .iter()
                    .map(|f| meadow_infer::subst_bound(f, args))
                    .collect(),
            );
        }
        let (owner, _) = ctor.rsplit_once('.')?;
        let sig = find(&InternedString::from(owner))?;
        let unknowns = vec![core::unknown(); sig.fields.iter().map(bound_arity).max().unwrap_or(0)];
        Some(
            sig.fields
                .iter()
                .map(|f| meadow_infer::subst_bound(f, &unknowns))
                .collect(),
        )
    }

    /// Names for the fields of constructor `ctor` in a value of type `of`
    /// matched against `subs`, each holding what the constructor's
    /// declaration says it does -- or what the pattern says, where the
    /// declaration cannot be read at `of`.
    fn field_names(
        &mut self,
        ctor: InternedString,
        subs: &[Pat],
        of: Option<&core::Ty>,
    ) -> Vec<Name> {
        let types = self.field_types(ctor, of);
        subs.iter()
            .enumerate()
            .map(|(i, p)| {
                let ty = types.as_ref().and_then(|ts| ts.get(i).cloned());
                let ty = field_type(ty, p).or_else(|| self.pattern_type(p));
                self.fresh_typed(ty)
            })
            .collect()
    }

    /// What a pattern that takes a value apart says of it, when nothing else
    /// does: a constructor's is its type's, a tuple's a tuple, a literal's its
    /// own. Its type arguments are unknown, and need not be known: the
    /// representation is what this is for, and a data value is a reference
    /// whatever it holds.
    fn pattern_type(&self, p: &Pat) -> Option<core::Ty> {
        match p {
            Pat::Ctor(ctor, _) => {
                let (owner, _) = ctor.rsplit_once('.')?;
                let owner = InternedString::from(owner);
                let sig = self.variants.get(&owner)?;
                let arity = sig
                    .iter()
                    .flat_map(|v| v.fields.iter())
                    .map(bound_arity)
                    .max()
                    .unwrap_or(0);
                Some(core::Ty::Con(owner, vec![core::unknown(); arity]))
            }
            Pat::Tuple(items) => Some(core::Ty::Tuple(vec![core::unknown(); items.len()])),
            Pat::Array(_) => Some(core::Ty::Con(
                InternedString::from("Array"),
                vec![core::unknown()],
            )),
            Pat::Record(_) => Some(core::Ty::Record(Box::new(core::Ty::RowEmpty))),
            Pat::Lit(l) => Some(lit_type(l)),
            Pat::Var(..) | Pat::As(..) | Pat::Wild => None,
        }
    }

    /// The type of field `label` of a record of type `of`.
    fn label_type(of: Option<&core::Ty>, label: InternedString) -> Option<core::Ty> {
        let core::Ty::Record(row) = of? else {
            return None;
        };
        let mut cur = &**row;
        while let core::Ty::RowExtend(l, ty, rest) = cur {
            if *l == label {
                return Some((**ty).clone());
            }
            cur = rest;
        }
        None
    }

    /// Where a value of the named type `of` keeps its field `label`, if every
    /// constructor of the type has that field in the same place: a `record`
    /// type's, or a `data` type's whose constructors name their fields. Its
    /// selection is then a field read, not a search of the labels at run time.
    fn field_index(&self, of: Option<&core::Ty>, label: InternedString) -> Option<usize> {
        let Some(core::Ty::Con(name, _)) = of else {
            return None;
        };
        let mut at = None;
        for v in self.variants.get(name)? {
            let i = v.labels.as_ref()?.iter().position(|l| *l == label)?;
            if at.is_some_and(|a| a != i) {
                return None;
            }
            at = Some(i);
        }
        at
    }

    /// `.label` of the value `r` holds, which a term of type `of` computed: a
    /// field read where the type says where the field is.
    fn select(&self, of: Option<core::Ty>, r: Name, label: InternedString) -> Extern {
        let of = of.or_else(|| self.type_of_name(r));
        match self.field_index(of.as_ref(), label) {
            Some(i) => Extern::Field(i),
            None => Extern::Select(label, place(of.as_ref(), label)),
        }
    }

    /// `{ r | label = v }` of the value `r` holds, which a term of type `of`
    /// computed.
    fn extend(&self, of: Option<core::Ty>, r: Name, label: InternedString) -> Extern {
        let of = of.or_else(|| self.type_of_name(r));
        Extern::Extend(label, place(of.as_ref(), label))
    }

    /// A fresh name for a function's own return continuation -- see
    /// [`Program::returns`].
    fn function_return(&mut self) -> Name {
        let k = self.fresh_ref();
        self.returns.insert(k);
        k
    }

    fn fresh_label(&mut self) -> Label {
        let l = Label(self.next_label);
        self.next_label += 1;
        l
    }

    // --- descriptors ------------------------------------------------------

    /// `rep` with a type variable's representation made the name of the
    /// descriptor in scope for it -- or [`NO_DESC`], for a variable no
    /// enclosing abstraction binds.
    fn resolve(&self, rep: Rep) -> Rep {
        match rep {
            Rep::Var(v) => match self.desc_name(v) {
                Some(n) => Rep::Var(n.0),
                None => Rep::Var(NO_DESC),
            },
            rep => rep,
        }
    }

    /// If `n` holds a thread -- a `Task a` -- remember how `a` is represented.
    fn note_thread(&mut self, n: Name, ty: &core::Ty) {
        if let core::Ty::Con(name, args) = ty
            && &**name == "Task"
            && let [a] = args.as_slice()
        {
            let rep = self.resolve(rep_of(a));
            self.threads.insert(n, rep);
        }
    }

    /// The name holding type variable `v`'s descriptor, where lowering is.
    fn desc_name(&self, v: u32) -> Option<Name> {
        self.tscope
            .iter()
            .rev()
            .find(|(id, _)| *id == v)
            .map(|(_, n)| *n)
    }

    /// Record `v`'s type, and its representation in the scope it is bound in.
    fn bind_poly(&mut self, v: Var, poly: core::Poly) {
        let rep = self.resolve(rep_of(&poly.ty));
        self.binder_reps.insert(v, rep);
        self.note_thread(v, &poly.ty);
        self.polys.insert(v, poly);
    }

    /// Every variable `t` binds, with its type and representation, and a name
    /// for every descriptor a type abstraction in it binds.
    fn scan(&mut self, t: &Term) {
        match t {
            Term::Var(_) | Term::Lit(_) | Term::Error => {}
            Term::TyLam(vs, b) => {
                let depth = self.tscope.len();
                if vs.iter().any(|v| v.kind == VarKind::Type) {
                    let names = match self.tylams.get(&address(t)) {
                        Some(names) => names.clone(),
                        None => {
                            let names: Vec<(u32, Name)> = vs
                                .iter()
                                .filter(|v| v.kind == VarKind::Type)
                                .map(|v| (v.id, self.fresh_as(Rep::Int)))
                                .collect();
                            self.descs.extend(names.iter().map(|(_, n)| *n));
                            self.tylams.insert(address(t), names.clone());
                            names
                        }
                    };
                    self.tscope.extend(names);
                }
                self.scan(b);
                self.tscope.truncate(depth);
            }
            Term::Loc(_, b) | Term::TyApp(b, _) | Term::Proj(b, _) | Term::Sel(b, _, _) => {
                self.scan(b)
            }
            Term::Lam(v, ty, b) => {
                self.bind_poly(*v, core::Poly::mono(ty.clone()));
                self.scan(b);
            }
            Term::App(a, b) | Term::Extend(a, _, b) => {
                self.scan(a);
                self.scan(b);
            }
            Term::Let(v, poly, r, b) => {
                self.bind_poly(*v, poly.clone());
                // A generic value is an object that makes instances of it.
                if let Some((_, vs, _)) = ty_abs(r) {
                    self.tyabs.insert(*v, described(vs));
                    self.binder_reps.insert(*v, Rep::Ref);
                }
                self.scan(r);
                self.scan(b);
            }
            Term::Join {
                var,
                params,
                ty,
                rhs,
                body,
            } => {
                for (v, t) in params {
                    self.bind_poly(*v, core::Poly::mono(t.clone()));
                }
                self.bind_poly(*var, core::Poly::mono(ty.clone()));
                self.scan(rhs);
                self.scan(body);
            }
            Term::Jump(_, args, _) => args.iter().for_each(|a| self.scan(a)),
            Term::LetRec(binds, body) => {
                for (v, poly, t) in binds {
                    self.bind_poly(*v, poly.clone());
                    if let Some((_, vs, _)) = ty_abs(t) {
                        self.tyabs.insert(*v, described(vs));
                    }
                    self.scan(t);
                }
                self.scan(body);
            }
            Term::If(a, b, c) => {
                self.scan(a);
                self.scan(b);
                self.scan(c);
            }
            Term::Tuple(xs) | Term::Array(xs, _) | Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
                xs.iter().for_each(|x| self.scan(x))
            }
            Term::Record(fs) => fs.iter().for_each(|(_, x)| self.scan(x)),
            Term::Perform(_, _, a, _) => self.scan(a),
            Term::Case(s, arms, _) => {
                self.scan(s);
                for (p, g, b) in arms {
                    self.scan_pat(p);
                    if let Some(g) = g {
                        self.scan(g);
                    }
                    self.scan(b);
                }
            }
            Term::Handle {
                body, clauses, ret, ..
            } => {
                self.scan(body);
                for c in clauses {
                    self.bind_poly(c.param, core::Poly::mono(c.param_ty.clone()));
                    // A resumption is a closure, whatever core managed to say its
                    // type is.
                    let resume = match &c.resume_ty {
                        ty @ core::Ty::Fun(..) => ty.clone(),
                        _ => core::Ty::Fun(
                            vec![c.param_ty.clone()],
                            Box::new(core::unknown()),
                            Box::new(core::Ty::RowEmpty),
                        ),
                    };
                    self.bind_poly(c.resume, core::Poly::mono(resume));
                    self.scan(&c.body);
                }
                if let Some((v, ty, b)) = ret {
                    self.bind_poly(*v, core::Poly::mono(ty.clone()));
                    self.scan(b);
                }
            }
        }
    }

    fn scan_pat(&mut self, p: &Pat) {
        match p {
            Pat::Wild | Pat::Lit(_) => {}
            Pat::Var(v, ty) => self.bind_poly(*v, core::Poly::mono(ty.clone())),
            Pat::As(v, ty, sub) => {
                self.bind_poly(*v, core::Poly::mono(ty.clone()));
                self.scan_pat(sub);
            }
            Pat::Tuple(ps) | Pat::Array(ps) | Pat::Ctor(_, ps) => {
                ps.iter().for_each(|p| self.scan_pat(p))
            }
            Pat::Record(fs) => fs.iter().for_each(|(_, p)| self.scan_pat(p)),
        }
    }

    /// Names holding the descriptors of `tys`, then `f` with them: a constant
    /// for a type that is known, the descriptor in scope for a variable.
    fn with_descs<'a>(
        &mut self,
        tys: &[core::Ty],
        env: Vec<Name>,
        f: Box<dyn FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement + 'a>,
    ) -> Statement {
        let plan: Vec<Result<Name, core::desc::Desc>> = tys
            .iter()
            .map(|ty| match core::desc::of(ty) {
                Some(d) => Err(d),
                None => match ty {
                    core::Ty::Var(v) => self.desc_name(*v).ok_or(core::desc::ANY),
                    _ => Err(core::desc::ANY),
                },
            })
            .collect();
        self.emit_descs(plan, 0, env, Vec::new(), f)
    }

    fn emit_descs<'a>(
        &mut self,
        plan: Vec<Result<Name, core::desc::Desc>>,
        i: usize,
        env: Vec<Name>,
        mut done: Vec<Name>,
        f: Box<dyn FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement + 'a>,
    ) -> Statement {
        match plan.get(i).copied() {
            None => f(self, done, env),
            Some(Ok(n)) => {
                done.push(n);
                self.emit_descs(plan, i + 1, env, done, f)
            }
            Some(Err(d)) => self.produces(
                Extern::Lit(core::Lit::Int(d)),
                vec![],
                &env,
                Some(con("Int")),
                move |this, x, env1| {
                    done.push(x);
                    this.emit_descs(plan, i + 1, env1, done, f)
                },
            ),
        }
    }

    /// The type arguments of `v`'s instantiation at `tys` that take
    /// descriptors.
    fn described_args(&self, v: Var, tys: Option<&Vec<core::Ty>>) -> Option<Vec<core::Ty>> {
        let at = self.tyabs.get(&v)?;
        Some(
            at.iter()
                .map(|i| {
                    tys.and_then(|tys| tys.get(*i).cloned())
                        .unwrap_or_else(core::unknown)
                })
                .collect(),
        )
    }

    /// `v`, a binding that is a type abstraction, instantiated with the
    /// descriptors `ds`, answering `k`.
    fn instantiate(&mut self, v: Var, ds: Vec<Name>, env: &[Name], k: Name) -> Statement {
        // A local one is an object: its method makes the instance.
        if env.contains(&v) {
            let mut sel = vec![v];
            sel.extend(ds);
            sel.push(k);
            return Statement::Substitute(
                sel.clone(),
                Box::new(Block {
                    params: sel,
                    body: Statement::Invoke(v, 0),
                }),
            );
        }
        match self.globals.get(&v) {
            Some((label, extra)) => {
                let label = *label;
                let mut sel = extra.clone();
                sel.extend(ds);
                sel.push(k);
                if self.letrecs.contains(&v) {
                    sel.push(self.ev);
                }
                Statement::Substitute(
                    sel.clone(),
                    Box::new(Block {
                        params: sel,
                        body: Statement::Jump(label),
                    }),
                )
            }
            None => {
                self.unsupported.insert(Unsupported::UnboundVar);
                Statement::Error("unbound variable")
            }
        }
    }

    /// A local type abstraction as an object: one method, taking the
    /// descriptors and a continuation, which evaluates `body` under them.
    fn ty_closure(&mut self, node: &Term, body: &Term, env: &[Name]) -> (Vec<Name>, Block) {
        let captures = restrict(env, &self.wants(&[body], &[]));
        let kk = self.function_return();
        let names = self.tylams[&address(node)].clone();
        let mut params = captures.clone();
        params.extend(names.iter().map(|(_, n)| *n));
        params.push(kk);
        let depth = self.tscope.len();
        self.tscope.extend(names);
        let inner = self.expr(body, &params, kk);
        self.tscope.truncate(depth);
        (
            captures,
            Block {
                params,
                body: inner,
            },
        )
    }

    fn tag_of(&mut self, ctor: InternedString) -> Tag {
        if let Some(t) = self.tags.get(&ctor) {
            return *t;
        }
        let t = self.next_tag;
        self.next_tag += 1;
        self.tags.insert(ctor, t);
        t
    }

    // --- liveness ---------------------------------------------------------

    /// Replace every label-bound name by the environment it needs.
    ///
    /// A `letrec` binding is not a value in the environment; reaching it means
    /// arranging its captured variables and jumping. So "this term mentions `f`"
    /// really means "this term needs whatever `f`'s block takes", and liveness
    /// has to be computed on the latter or a capture list comes out short.
    fn expand(&self, want: HashSet<Var>) -> HashSet<Var> {
        let mut out = HashSet::new();
        for v in want {
            // A join point's name stands for its environment the same way a
            // `letrec` binding's does: a jump to it needs what its block
            // takes, and mentions none of it itself.
            let extra = self
                .globals
                .get(&v)
                .map(|(_, e)| e)
                .or_else(|| self.joins.get(&v).map(|(_, e)| e));
            match extra {
                Some(extra) => out.extend(extra.iter().copied()),
                None => {
                    out.insert(v);
                }
            }
        }
        out
    }

    /// The names `terms` will need, minus `bound`, with labels expanded -- and
    /// the evidence, if any of them calls, performs or handles.
    fn wants(&self, terms: &[&Term], bound: &[Var]) -> HashSet<Var> {
        let mut want = HashSet::new();
        for t in terms {
            core::free_vars_into(t, &mut want);
        }
        for v in bound {
            want.remove(v);
        }
        let mut want = self.expand(want);
        if terms.iter().any(|t| self.needs_ev(t)) {
            want.insert(self.ev);
        }
        want
    }

    /// Does lowering `t` refer to the evidence in scope? A call passes it on, a
    /// `perform` searches it and a `handle` extends it. A lambda does not: its
    /// body gets the evidence its caller passes.
    fn needs_ev(&self, t: &Term) -> bool {
        match t {
            // A call passes the evidence on, unless it is a jump to a direct
            // entry point that does not take it.
            Term::App(..) => match self.direct_worker(t) {
                Some((f, args)) if !self.worker_ev.contains(&f) => {
                    args.iter().any(|a| self.needs_ev(a))
                }
                _ => true,
            },
            Term::Perform(..) | Term::Handle { .. } | Term::LetRec(..) => true,
            // A jump reaches a block that takes the evidence from whoever
            // enters it, exactly as a call to a `letrec` binding does.
            Term::Join { .. } | Term::Jump(..) => true,
            Term::Var(v) => self.letrecs.contains(v),
            Term::Lam(..) | Term::Lit(_) | Term::Error => false,
            Term::TyLam(_, b)
            | Term::TyApp(b, _)
            | Term::Loc(_, b)
            | Term::Proj(b, _)
            | Term::Sel(b, _, _) => self.needs_ev(b),
            Term::Let(_, _, r, b) | Term::Extend(r, _, b) => self.needs_ev(r) || self.needs_ev(b),
            Term::If(a, b, c) => self.needs_ev(a) || self.needs_ev(b) || self.needs_ev(c),
            Term::Tuple(xs) | Term::Array(xs, _) | Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
                xs.iter().any(|x| self.needs_ev(x))
            }
            Term::Record(fs) => fs.iter().any(|(_, x)| self.needs_ev(x)),
            Term::Case(s, arms, _) => {
                self.needs_ev(s)
                    || arms.iter().any(|(_, g, b)| {
                        g.as_ref().is_some_and(|g| self.needs_ev(g)) || self.needs_ev(b)
                    })
            }
        }
    }

    /// Lower a lambda's body, whose parameters are `captures ++ [param, k, ev]`:
    /// the evidence is the caller's, not the one around the lambda.
    fn lambda(&mut self, param: Var, body: &Term, env: &[Name]) -> (Vec<Name>, Block) {
        let mut want = self.wants(&[body], &[param]);
        want.remove(&self.ev);
        let captures = restrict(env, &want);
        let ik = self.function_return();
        let iev = self.fresh_ref();
        let mut params = captures.clone();
        params.push(param);
        params.push(ik);
        params.push(iev);
        let outer = std::mem::replace(&mut self.ev, iev);
        let inner = self.expr(body, &params, ik);
        self.ev = outer;
        (
            captures,
            Block {
                params,
                body: inner,
            },
        )
    }

    /// What must stay live across the evaluation of something: whatever `terms`
    /// still need, plus `extra` (the continuation, values already computed).
    fn keep(&self, env: &[Name], terms: &[&Term], extra: &[Name]) -> Vec<Name> {
        let mut want = self.wants(terms, &[]);
        want.extend(extra.iter().copied());
        restrict(env, &want)
    }

    // --- the universal shapes --------------------------------------------

    /// `return v to k` — arrange the environment as exactly `[k, v]`, then
    /// invoke.
    ///
    /// The invoke carries no arguments. `k`'s method sees its own captures
    /// followed by whatever the substitution left behind it, which is `[v]` —
    /// which is why every continuation built by [`Lower::bind`] has parameters
    /// `captures ++ [x]`.
    fn ret(&mut self, k: Name, v: Name) -> Statement {
        Statement::Substitute(
            vec![k, v],
            Box::new(Block {
                params: vec![k, v],
                body: Statement::Invoke(k, 0),
            }),
        )
    }

    /// `invoke f` with nothing to hand it — how a failure continuation is
    /// entered, and the smallest possible use of the whole calling convention.
    fn enter(&mut self, f: Name) -> Statement {
        Statement::Substitute(
            vec![f],
            Box::new(Block {
                params: vec![f],
                body: Statement::Invoke(f, 0),
            }),
        )
    }

    /// An `extern` that produces one value, binds it, and continues.
    fn produces(
        &mut self,
        op: Extern,
        args: Vec<Name>,
        env: &[Name],
        ty: Option<core::Ty>,
        f: impl FnOnce(&mut Self, Name, Vec<Name>) -> Statement,
    ) -> Statement {
        self.produces_as(op, args, env, ty, None, f)
    }

    /// The same, with the bound name forced — for a `let` whose right-hand side
    /// needs no continuation.
    fn produces_as(
        &mut self,
        op: Extern,
        args: Vec<Name>,
        env: &[Name],
        ty: Option<core::Ty>,
        name: Option<Name>,
        f: impl FnOnce(&mut Self, Name, Vec<Name>) -> Statement,
    ) -> Statement {
        let x = match name {
            Some(n) => n,
            None => self.fresh_typed(ty),
        };
        let mut params = vec![x];
        params.extend_from_slice(env);
        let body = f(self, x, params.clone());
        Statement::Extern {
            op,
            args,
            blocks: vec![Block { params, body }],
        }
    }

    /// Can `e` produce a value without transferring control?
    ///
    /// If it can, whatever comes next is simply the `rest` of the statement that
    /// produced it, and [`Lower::bind`] needs no continuation object at all.
    /// That is the difference between an allocation and an indirect jump per
    /// comparison, per argument, per operand — which was most of what the
    /// machine did — and one `extern`.
    ///
    /// A variable is only simple when it is *in scope*: a reference to a
    /// top-level definition is a `jump`, which is exactly a transfer of control.
    /// `if` is excluded for a different reason. It does not transfer control
    /// away, but each of its branches would need its own copy of everything that
    /// follows, and duplicating the rest of a function per condition is how a
    /// compiler runs out of memory on real code.
    fn simple(&self, e: &Term, env: &[Name]) -> bool {
        match e {
            Term::Loc(_, inner) => self.simple(inner, env),
            Term::Var(v) => env.contains(v),
            Term::Lit(_) => true,
            // Building a closure allocates, but it does not go anywhere -- nor
            // does the object a generic `let` is. Instantiating one calls it.
            Term::TyLam(..) => ty_abs(e).is_some(),
            Term::TyApp(..) => false,
            Term::Lam(..) => true,
            Term::Prim(_, xs, _) | Term::Ctor(_, _, xs) | Term::Tuple(xs) | Term::Array(xs, _) => {
                xs.iter().all(|x| self.simple(x, env))
            }
            Term::Record(fs) => fs.iter().all(|(_, t)| self.simple(t, env)),
            Term::Sel(t, _, _) | Term::Proj(t, _) => self.simple(t, env),
            Term::Extend(t, _, v) => self.simple(t, env) && self.simple(v, env),
            Term::Let(x, _, rhs, body) => {
                let mut inner = env.to_vec();
                inner.push(*x);
                self.simple(rhs, env) && self.simple(body, &inner)
            }
            _ => false,
        }
    }

    /// Lower a [`Lower::simple`] expression in place and continue.
    ///
    /// `f` receives the name its value was bound to and the environment that now
    /// holds — the whole of the old one plus the new binding, since nothing here
    /// truncates.
    fn direct(&mut self, e: &Term, env: &[Name], name: Option<Name>, f: Then<'_>) -> Statement {
        let ty = self.type_of(e);
        match e {
            Term::TyLam(_, inner) => match ty_abs(e) {
                Some((node, _, body)) => {
                    let (captures, method) = self.ty_closure(node, body, env);
                    let x = name.unwrap_or_else(|| self.fresh_ref());
                    let mut after: Vec<Name> = vec![x];
                    after.extend_from_slice(env);
                    let rest = f(self, x, after);
                    Statement::New {
                        name: x,
                        captures,
                        methods: vec![method],
                        rest: Box::new(rest),
                    }
                }
                None => self.direct(inner, env, name, f),
            },
            Term::TyApp(inner, _) => self.direct(inner, env, name, f),
            Term::Loc(loc, inner) => {
                Statement::Mark(*loc, Box::new(self.direct(inner, env, name, f)))
            }
            // Already a value. With no name forced there is nothing at all to
            // emit; with one, a `substitute` gives it its second name.
            Term::Var(v) => match name {
                None => f(self, *v, env.to_vec()),
                Some(x) => {
                    let mut sel = env.to_vec();
                    sel.push(*v);
                    let mut params = env.to_vec();
                    params.push(x);
                    let body = f(self, x, params.clone());
                    Statement::Substitute(sel, Box::new(Block { params, body }))
                }
            },

            Term::Lit(l) => self.produces_as(Extern::Lit(l.clone()), vec![], env, ty, name, f),

            Term::Lam(param, _, body) => {
                let (captures, method) = self.lambda(*param, body, env);
                let x = name.unwrap_or_else(|| self.fresh_ref());
                let mut after: Vec<Name> = vec![x];
                after.extend_from_slice(env);
                let rest = f(self, x, after);
                Statement::New {
                    name: x,
                    captures,
                    methods: vec![method],
                    rest: Box::new(rest),
                }
            }

            Term::Prim(p, args, _) => {
                let p = *p;
                // A literal operand rides along inside the `extern`, so it never
                // becomes a name and never occupies a register.
                if let Some((x, l)) = const_operand(p, args) {
                    return self.direct_all(&[x], env.to_vec(), move |this, xs, env1| {
                        this.produces_as(Extern::PrimK(p, l), xs, &env1, ty, name, f)
                    });
                }
                self.direct_all(args, env.to_vec(), move |this, xs, env1| {
                    this.produces_as(Extern::Prim(p), xs, &env1, ty, name, f)
                })
            }

            Term::Ctor(ctor, _, args) => {
                let ctor = *ctor;
                let tag = self.tag_of(ctor);
                self.direct_all(args, env.to_vec(), move |this, fields, env1| {
                    this.builds(ctor, tag, fields, &env1, ty, name, f)
                })
            }

            Term::Tuple(items) => {
                let ctor = InternedString::from("#tuple");
                let tag = self.tag_of(ctor);
                self.direct_all(items, env.to_vec(), move |this, fields, env1| {
                    this.builds(ctor, tag, fields, &env1, ty, name, f)
                })
            }

            Term::Array(items, _) => self.direct_all(items, env.to_vec(), move |this, xs, env1| {
                this.produces_as(Extern::Array, xs, &env1, ty, name, f)
            }),

            Term::Record(fields) => {
                let labels: Vec<InternedString> = fields.iter().map(|(n, _)| *n).collect();
                let terms: Vec<Term> = fields.iter().map(|(_, t)| t.clone()).collect();
                self.direct_all(&terms, env.to_vec(), move |this, xs, env1| {
                    this.produces_as(Extern::Record(labels), xs, &env1, ty, name, f)
                })
            }

            Term::Sel(rec, label, _) => {
                let label = *label;
                let of = self.type_of(rec);
                self.direct(
                    rec,
                    env,
                    None,
                    Box::new(move |this, r, env1| {
                        let op = this.select(of, r, label);
                        this.produces_as(op, vec![r], &env1, ty, name, f)
                    }),
                )
            }

            Term::Proj(t, i) => {
                let i = *i;
                self.direct(
                    t,
                    env,
                    None,
                    Box::new(move |this, x, env1| {
                        this.produces_as(Extern::Field(i), vec![x], &env1, ty, name, f)
                    }),
                )
            }

            Term::Extend(rec, label, val) => {
                let label = *label;
                let terms = vec![(**rec).clone(), (**val).clone()];
                let of = self.type_of(rec);
                self.direct_all(&terms, env.to_vec(), move |this, xs, env1| {
                    let op = this.extend(of, xs[0], label);
                    this.produces_as(op, xs, &env1, ty, name, f)
                })
            }

            Term::Let(x, _, rhs, body) => {
                let x = *x;
                self.direct(
                    rhs,
                    env,
                    Some(x),
                    Box::new(move |this, _x, env1| this.direct(body, &env1, name, f)),
                )
            }

            other => unreachable!("direct on something that transfers control: {other:?}"),
        }
    }

    /// `let x = K(fields); rest` — the data half of [`Lower::produces_as`].
    #[allow(clippy::too_many_arguments)]
    fn builds(
        &mut self,
        ctor: InternedString,
        tag: Tag,
        fields: Vec<Name>,
        env: &[Name],
        ty: Option<core::Ty>,
        name: Option<Name>,
        f: Then<'_>,
    ) -> Statement {
        let x = match name {
            Some(n) => n,
            None => self.fresh_typed(ty),
        };
        let mut after: Vec<Name> = vec![x];
        after.extend_from_slice(env);
        let rest = f(self, x, after);
        Statement::Let {
            name: x,
            tag,
            ctor,
            fields,
            rest: Box::new(rest),
        }
    }

    /// Several simple terms, left to right.
    fn direct_all(
        &mut self,
        es: &[Term],
        env: Vec<Name>,
        f: impl FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement,
    ) -> Statement {
        fn go(
            this: &mut Lower,
            es: &[Term],
            env: Vec<Name>,
            done: Vec<Name>,
            f: Box<dyn FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement + '_>,
        ) -> Statement {
            match es.split_first() {
                None => f(this, done, env),
                Some((head, rest)) => this.direct(
                    head,
                    &env,
                    None,
                    Box::new(move |this, x, env1| {
                        let mut done = done;
                        done.push(x);
                        go(this, rest, env1, done, f)
                    }),
                ),
            }
        }
        go(self, es, env, Vec::new(), Box::new(f))
    }

    /// Evaluate `e`, bind its value, and continue.
    ///
    /// `env` is the environment right now; `keep` is the subset of it that must
    /// still be live once `e` has produced a value — everything the continuation
    /// will refer to, including the outer continuation itself. Getting `keep`
    /// wrong is the one way to build an ill-formed program here, which is why
    /// every caller computes it from free variables rather than by hand.
    ///
    /// `name` forces the bound variable's name, for `let`.
    fn bind(
        &mut self,
        e: &Term,
        env: &[Name],
        keep: &[Name],
        name: Option<Name>,
        f: Then<'_>,
    ) -> Statement {
        // Anything that cannot transfer control is lowered where it stands, and
        // `keep` is not needed there: nothing is truncated, so everything that
        // was live still is.
        if self.simple(e, env) {
            return self.direct(e, env, name, f);
        }
        let kk = self.fresh_ref();
        // `e` runs, returns through `kk` once, and everything it pushed
        // meanwhile is dead: a frame. See [`Program::frames`].
        self.frames.insert(kk);
        let x = match name {
            Some(n) => n,
            None => {
                let ty = self.type_of(e);
                self.fresh_typed(ty)
            }
        };

        // On entry to the method: captures, then what the `substitute` in `ret`
        // left after the object itself — the single returned value.
        let mut inner: Vec<Name> = keep.to_vec();
        inner.push(x);
        let body = f(self, x, inner.clone());

        // `new` prepends the object to the environment.
        let mut outer: Vec<Name> = vec![kk];
        outer.extend_from_slice(env);
        let rest = self.expr(e, &outer, kk);

        Statement::New {
            name: kk,
            captures: keep.to_vec(),
            methods: vec![Block {
                params: inner,
                body,
            }],
            rest: Box::new(rest),
        }
    }

    /// Several terms, left to right — the order a strict language promises, made
    /// explicit rather than left to an evaluator.
    ///
    /// At each step the names that must survive are the caller's `base`, the
    /// values already computed, and the free variables of the terms still to
    /// come.
    fn bind_all(
        &mut self,
        es: &[Term],
        env: &[Name],
        base: &HashSet<Var>,
        f: Box<dyn FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement + '_>,
    ) -> Statement {
        fn go(
            this: &mut Lower,
            es: &[Term],
            env: Vec<Name>,
            base: &HashSet<Var>,
            done: Vec<Name>,
            f: Box<dyn FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement + '_>,
        ) -> Statement {
            match es.split_first() {
                None => f(this, done, env),
                Some((head, rest)) => {
                    let mut want = base.clone();
                    want.extend(done.iter().copied());
                    let rest_refs: Vec<&Term> = rest.iter().collect();
                    want.extend(this.wants(&rest_refs, &[]));
                    let keep = restrict(&env, &want);
                    this.bind(
                        head,
                        &env,
                        &keep,
                        None,
                        Box::new(move |this, x, env1| {
                            let mut done = done;
                            done.push(x);
                            go(this, rest, env1, base, done, f)
                        }),
                    )
                }
            }
        }
        go(self, es, env.to_vec(), base, Vec::new(), f)
    }

    // --- expressions ------------------------------------------------------

    /// `⟦e⟧ k` in environment `env` — run `e`, answer `k`.
    ///
    /// `env` must be the exact environment the machine will have, in order, and
    /// must contain `k`.
    fn expr(&mut self, e: &Term, env: &[Name], k: Name) -> Statement {
        self.continuations.insert(k);
        match e {
            Term::Loc(loc, inner) => Statement::Mark(*loc, Box::new(self.expr(inner, env, k))),
            // A type abstraction over types takes their descriptors, and an
            // instantiation of one passes them. One over rows or effects only
            // takes nothing at run time.
            Term::TyLam(_, inner) => match ty_abs(e) {
                Some((node, _, body)) => {
                    let (captures, method) = self.ty_closure(node, body, env);
                    let f = self.fresh_ref();
                    let rest = self.ret(k, f);
                    Statement::New {
                        name: f,
                        captures,
                        methods: vec![method],
                        rest: Box::new(rest),
                    }
                }
                None => self.expr(inner, env, k),
            },
            Term::TyApp(inner, tys) => match inner.peel() {
                Term::Var(v) if self.tyabs.contains_key(v) => {
                    let v = *v;
                    let tys = self.described_args(v, Some(tys)).expect("checked");
                    self.with_descs(
                        &tys,
                        env.to_vec(),
                        Box::new(move |this, ds, env1| this.instantiate(v, ds, &env1, k)),
                    )
                }
                _ => self.expr(inner, env, k),
            },
            Term::Var(v) if env.contains(v) => self.ret(k, *v),

            // A label: arrange exactly what its block takes and jump. `jump`
            // leaves the environment alone, so the substitution is the entire
            // calling sequence.
            Term::Var(v) => match self.globals.get(v) {
                Some((label, extra)) => {
                    let label = *label;
                    let mut sel = extra.clone();
                    sel.push(k);
                    if self.letrecs.contains(v) {
                        sel.push(self.ev);
                    }
                    Statement::Substitute(
                        sel.clone(),
                        Box::new(Block {
                            params: sel,
                            body: Statement::Jump(label),
                        }),
                    )
                }
                None => {
                    self.unsupported.insert(Unsupported::UnboundVar);
                    Statement::Error("unbound variable")
                }
            },

            Term::Lit(l) => {
                let ty = Some(lit_type(l));
                self.produces(Extern::Lit(l.clone()), vec![], env, ty, |this, x, _| {
                    this.ret(k, x)
                })
            }

            // Codata with one method: the argument, and where to send the answer.
            Term::Lam(param, _, body) => {
                let (captures, method) = self.lambda(*param, body, env);
                let f = self.fresh_ref();
                let rest = self.ret(k, f);
                Statement::New {
                    name: f,
                    captures,
                    methods: vec![method],
                    rest: Box::new(rest),
                }
            }

            // A saturated call to a known function is a jump. The arguments go
            // where its block wants them and control leaves — no closure per
            // argument, no continuation to receive one. This is what makes a
            // loop a loop rather than a sequence of allocations.
            Term::App(..) if self.direct_call(e).is_some() => {
                let (worker, args) = self.direct_call(e).expect("checked");
                let (f, _) = self.direct_worker(e).expect("checked");
                let args: Vec<Term> = args.into_iter().cloned().collect();
                let ev = self.worker_ev.contains(&f).then_some(self.ev);
                let base: HashSet<Var> = [k].into_iter().chain(ev).collect();
                let tys = self.described_args(f, call_tys(e));
                self.bind_all(
                    &args,
                    env,
                    &base,
                    Box::new(move |this, names, env1| {
                        let jump = move |_this: &mut Lower, ds: Vec<Name>, _env: Vec<Name>| {
                            let mut sel = ds;
                            sel.extend(names);
                            sel.push(k);
                            sel.extend(ev);
                            Statement::Substitute(
                                sel.clone(),
                                Box::new(Block {
                                    params: sel,
                                    body: Statement::Jump(worker),
                                }),
                            )
                        };
                        match tys {
                            Some(tys) => this.with_descs(&tys, env1, Box::new(jump)),
                            None => jump(this, Vec::new(), env1),
                        }
                    }),
                )
            }

            // Calling is arranging `[f, arg, k]` and invoking: the object drops
            // off the front and its method sees `captures ++ [arg, k]`.
            Term::App(fun, arg) => {
                let ev = self.ev;
                let keep = self.keep(env, &[arg], &[k, ev]);
                self.bind(
                    fun,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, fv, env1| {
                        let want: HashSet<Var> = [k, fv, ev].into_iter().collect();
                        let keep = restrict(&env1, &want);
                        this.bind(
                            arg,
                            &env1,
                            &keep,
                            None,
                            Box::new(move |_this, av, _env2| {
                                Statement::Substitute(
                                    vec![fv, av, k, ev],
                                    Box::new(Block {
                                        params: vec![fv, av, k, ev],
                                        body: Statement::Invoke(fv, 0),
                                    }),
                                )
                            }),
                        )
                    }),
                )
            }

            Term::Let(x, _, rhs, body) => {
                let mut want = self.wants(&[body], &[*x]);
                want.insert(k);
                let keep = restrict(env, &want);
                self.bind(
                    rhs,
                    env,
                    &keep,
                    Some(*x),
                    Box::new(move |this, _x, env1| this.expr(body, &env1, k)),
                )
            }

            // Lambda lifting: one label per binding, all sharing the group's
            // captured environment. See the module docs.
            // A join point is a block and nothing else. It captures nothing:
            // it is entered only from inside the body it was written in, so
            // whatever it needs from around it is still in the environment
            // there and is passed at the jump, like a `letrec` binding's.
            //
            // The continuation goes in its parameters too. Every jump is in
            // tail position of the body, so every jump answers the same `k`
            // the body would have -- which is what makes this a *join*.
            Term::Join {
                var,
                params,
                rhs,
                body,
                ..
            } => {
                let ps: Vec<Var> = params.iter().map(|(v, _)| *v).collect();
                let mut want = self.wants(&[rhs], &ps);
                want.remove(&self.ev);
                let fvs = restrict(env, &want);

                let label = self.fresh_label();
                self.joins.insert(*var, (label, fvs.clone()));

                let kk = self.function_return();
                let iev = self.fresh_ref();
                let mut block: Vec<Name> = fvs.clone();
                block.extend(ps.iter().copied());
                block.push(kk);
                block.push(iev);
                let outer = std::mem::replace(&mut self.ev, iev);
                let made = self.expr(rhs, &block, kk);
                self.ev = outer;
                self.defs.push(Def {
                    label,
                    name: InternedString::from("<join>"),
                    module: self.module,
                    block: Block {
                        params: block,
                        body: made,
                    },
                });

                self.expr(body, env, k)
            }

            // Entering one: put the environment into the shape its block
            // expects, then jump. No object is made and none is invoked.
            Term::Jump(j, args, _) => {
                let Some((label, fvs)) = self.joins.get(j).cloned() else {
                    return Statement::Error("a jump to a join point not in scope");
                };
                let ev = self.ev;
                let args: Vec<Term> = args.clone();
                let base: HashSet<Var> = [k, ev].into_iter().chain(fvs.iter().copied()).collect();
                self.bind_all(
                    &args,
                    env,
                    &base,
                    Box::new(move |_this, names, _env1| {
                        let mut sel = fvs;
                        sel.extend(names);
                        sel.push(k);
                        sel.push(ev);
                        Statement::Substitute(
                            sel.clone(),
                            Box::new(Block {
                                params: sel,
                                body: Statement::Jump(label),
                            }),
                        )
                    }),
                )
            }

            Term::LetRec(binds, body) => {
                let bound: Vec<Var> = binds.iter().map(|(v, _, _)| *v).collect();
                let rhs: Vec<&Term> = binds.iter().map(|(_, _, t)| t).collect();
                // Each binding's block takes the evidence of whoever refers to
                // it, so it is not captured.
                let mut want = self.wants(&rhs, &bound);
                want.remove(&self.ev);
                let fvs = restrict(env, &want);

                // Register every label before lowering any right-hand side, so
                // the group can refer to itself in any direction.
                let labels: Vec<Label> = binds.iter().map(|_| self.fresh_label()).collect();
                for (v, l) in bound.iter().zip(&labels) {
                    self.globals.insert(*v, (*l, fvs.clone()));
                    self.letrecs.insert(*v);
                }

                for ((_, _, term), label) in binds.iter().zip(&labels) {
                    let kk = self.function_return();
                    let iev = self.fresh_ref();
                    // A generic binding's descriptors come after what the group
                    // captures, from whoever refers to it.
                    let (descs, term) = match ty_abs(term) {
                        Some((node, _, body)) => (self.tylams[&address(node)].clone(), body),
                        None => (Vec::new(), term),
                    };
                    let mut params = fvs.clone();
                    params.extend(descs.iter().map(|(_, n)| *n));
                    params.push(kk);
                    params.push(iev);
                    let depth = self.tscope.len();
                    self.tscope.extend(descs);
                    let outer = std::mem::replace(&mut self.ev, iev);
                    let body = self.expr(term, &params, kk);
                    self.ev = outer;
                    self.tscope.truncate(depth);
                    self.defs.push(Def {
                        label: *label,
                        name: InternedString::from("<letrec>"),
                        module: self.module,
                        block: Block { params, body },
                    });
                }

                self.expr(body, env, k)
            }

            // Not a statement of its own: a branching primitive with two
            // continuation blocks is all `if` ever was. Neither branch changes
            // the environment, so both blocks take it as it stands.
            Term::If(c, t, e) => {
                let keep = self.keep(env, &[t, e], &[k]);

                // `if x < y` is one test, not a comparison whose result is
                // bound, immediately tested, and then never looked at again.
                // Fusing is only available when the condition cannot itself
                // transfer control — otherwise its operands are not values yet —
                // which is exactly the case `bind` would have handled without a
                // continuation anyway, so nothing else changes.
                if let Term::Prim(p, cargs, _) = &**c {
                    let p = *p;
                    if p.compares() && self.simple(c, env) {
                        let (op, operands) = match const_operand(p, cargs) {
                            Some((x, l)) => (Extern::BranchPrimK(p, l), vec![x]),
                            None => (Extern::BranchPrim(p), cargs.to_vec()),
                        };
                        return self.direct_all(&operands, env.to_vec(), move |this, xs, env1| {
                            let then = this.expr(t, &env1, k);
                            let els = this.expr(e, &env1, k);
                            Statement::Extern {
                                op,
                                args: xs,
                                blocks: vec![
                                    Block {
                                        params: env1.clone(),
                                        body: els,
                                    },
                                    Block {
                                        params: env1,
                                        body: then,
                                    },
                                ],
                            }
                        });
                    }
                }

                self.bind(
                    c,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, cv, env1| {
                        let then = this.expr(t, &env1, k);
                        let els = this.expr(e, &env1, k);
                        Statement::Extern {
                            op: Extern::Branch,
                            args: vec![cv],
                            blocks: vec![
                                Block {
                                    params: env1.clone(),
                                    body: els,
                                },
                                Block {
                                    params: env1,
                                    body: then,
                                },
                            ],
                        }
                    }),
                )
            }

            Term::Prim(prim, args, ty) => {
                let prim = *prim;
                let ty = Some(ty.clone());
                if let Some((x, l)) = const_operand(prim, args) {
                    return self.sequence(&[x], env, k, move |this, names, env1| {
                        this.produces(Extern::PrimK(prim, l), names, &env1, ty, |this, out, _| {
                            this.ret(k, out)
                        })
                    });
                }
                self.sequence(args, env, k, move |this, names, env1| {
                    this.produces(Extern::Prim(prim), names, &env1, ty, |this, out, _| {
                        this.ret(k, out)
                    })
                })
            }

            Term::Ctor(name, ty, args) => {
                let ctor = *name;
                let tag = self.tag_of(ctor);
                let ty = Some(ty.clone());
                self.sequence(args, env, k, move |this, fields, _| {
                    let x = this.fresh_typed(ty);
                    let rest = this.ret(k, x);
                    Statement::Let {
                        name: x,
                        tag,
                        ctor,
                        fields,
                        rest: Box::new(rest),
                    }
                })
            }

            Term::Tuple(items) => {
                let ctor = InternedString::from("#tuple");
                let tag = self.tag_of(ctor);
                let ty = self.type_of(e);
                self.sequence(items, env, k, move |this, fields, _| {
                    let x = this.fresh_typed(ty);
                    let rest = this.ret(k, x);
                    Statement::Let {
                        name: x,
                        tag,
                        ctor,
                        fields,
                        rest: Box::new(rest),
                    }
                })
            }

            Term::Array(items, _) => {
                let ty = self.type_of(e);
                self.sequence(items, env, k, move |this, xs, env1| {
                    this.produces(Extern::Array, xs, &env1, ty, |this, out, _| {
                        this.ret(k, out)
                    })
                })
            }

            Term::Record(fields) => {
                let labels: Vec<InternedString> = fields.iter().map(|(n, _)| *n).collect();
                let terms: Vec<Term> = fields.iter().map(|(_, t)| t.clone()).collect();
                let ty = self.type_of(e);
                self.sequence(&terms, env, k, move |this, xs, env1| {
                    this.produces(Extern::Record(labels), xs, &env1, ty, |this, out, _| {
                        this.ret(k, out)
                    })
                })
            }

            Term::Sel(rec, label, ty) => {
                let label = *label;
                let ty = Some(ty.clone());
                let keep = restrict(env, &[k].into_iter().collect());
                let of = self.type_of(rec);
                self.bind(
                    rec,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, r, env1| {
                        let op = this.select(of, r, label);
                        this.produces(op, vec![r], &env1, ty, |this, out, _| this.ret(k, out))
                    }),
                )
            }

            Term::Extend(rec, label, val) => {
                let label = *label;
                let terms = vec![(**rec).clone(), (**val).clone()];
                let ty = self.type_of(e);
                let of = self.type_of(rec);
                self.sequence(&terms, env, k, move |this, xs, env1| {
                    let op = this.extend(of, xs[0], label);
                    this.produces(op, xs, &env1, ty, |this, out, _| this.ret(k, out))
                })
            }

            // Tuple projection cannot be a `switch`: the arity a `switch` arm
            // would have to name is not in the term.
            Term::Proj(t, i) => {
                let i = *i;
                let ty = self.type_of(e);
                let keep = restrict(env, &[k].into_iter().collect());
                self.bind(
                    t,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, x, env1| {
                        this.produces(Extern::Field(i), vec![x], &env1, ty, |this, out, _| {
                            this.ret(k, out)
                        })
                    }),
                )
            }

            Term::Case(scrutinee, arms, _) => {
                let mut want = HashSet::new();
                for (p, guard, body) in arms {
                    let mut bound = Vec::new();
                    core::pat_vars(p, &mut bound);
                    want.extend(self.wants(&[body], &bound));
                    if let Some(g) = guard {
                        want.extend(self.wants(&[g], &bound));
                    }
                }
                want.insert(k);
                if self.opt.case_trees()
                    && let Term::Tuple(items) = scrutinee.peel()
                    && let Some(tree) = self.case_of_tuple(items, arms, env, &want, k)
                {
                    return tree;
                }
                let keep = restrict(env, &want);
                self.bind(
                    scrutinee,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, s, env1| this.case(s, arms, env1, k)),
                )
            }

            Term::Perform(effect, op, arg, ty) => {
                let (effect, op) = (*effect, *op);
                let tys = (self.type_of(arg), Some(ty.clone()));
                let ev = self.ev;
                let keep = restrict(env, &[k, ev].into_iter().collect());
                self.bind(
                    arg,
                    env,
                    &keep,
                    None,
                    Box::new(move |this, av, _env1| this.perform(effect, op, av, k, ev, tys)),
                )
            }

            Term::Handle {
                body, clauses, ret, ..
            } => self.handle(body, clauses, ret.as_ref(), env, k),

            // A term the front end could not build. Lowering it to a statement
            // that fails is the faithful translation, not a gap.
            Term::Error => Statement::Error("ill-formed term"),
        }
    }

    /// Is this a saturated call to a definition with a direct entry point?
    ///
    /// Exact arity only. An under-applied call has to build a closure — that is
    /// what a partial application *is* — and an over-applied one returns
    /// something that is then called again, which the general path already
    /// handles correctly.
    fn direct_call<'t>(&self, e: &'t Term) -> Option<(Label, Vec<&'t Term>)> {
        let (f, args) = self.direct_worker(e)?;
        Some((self.workers[&f].0, args))
    }

    /// The same, answering which definition rather than where it starts.
    fn direct_worker<'t>(&self, e: &'t Term) -> Option<(Var, Vec<&'t Term>)> {
        let (head, args) = call_spine(e);
        let Term::Var(v) = head else { return None };
        let &(_, arity) = self.workers.get(v)?;
        (arity == args.len()).then_some((*v, args))
    }
    /// Evaluate a list of terms left to right, keeping `k` alive throughout.
    fn sequence(
        &mut self,
        terms: &[Term],
        env: &[Name],
        k: Name,
        f: impl FnOnce(&mut Lower, Vec<Name>, Vec<Name>) -> Statement,
    ) -> Statement {
        let base: HashSet<Var> = [k].into_iter().collect();
        self.bind_all(terms, env, &base, Box::new(f))
    }

    // --- pattern matching -------------------------------------------------

    /// A chain of failure continuations, one per arm.
    ///
    /// `f_i` captures everything live plus `f_{i+1}`, so an arm that fails
    /// half-way through a nested pattern can restore the whole environment by
    /// invoking one object — including the scrutinee, which a `switch` in the
    /// middle of the arm will have consumed.
    fn case(&mut self, s: Name, arms: &[Arm], live: Vec<Name>, k: Name) -> Statement {
        // An arm that cannot fail needs nothing to fail to: no chain, no
        // objects, just its bindings and its body. `let (a, b) = p in …` is a
        // `match` of exactly this shape, and it used to build two objects.
        if let Some((pat, None, term)) = arms.first()
            && irrefutable(pat)
        {
            let unused = self.fresh_ref();
            return self.match_pat(
                pat,
                s,
                live,
                unused,
                Box::new(move |this, env| this.expr(term, &env, k)),
            );
        }
        if let Some(chain) = self.case_literals(s, arms, &live, k) {
            return chain;
        }
        if let Some(switch) = self.case_total(s, arms, &live, k) {
            return switch;
        }
        if let Some(switch) = self.case_switch(s, arms, &live, k) {
            return switch;
        }
        if self.opt.case_trees()
            && let Some(tree) = self.case_matrix(s, arms, &live, k)
        {
            return tree;
        }
        if let Some(tree) = self
            .opt
            .case_trees()
            .then(|| self.case_tree(s, arms, &live, k))
            .flatten()
        {
            return tree;
        }
        self.case_chain(s, arms, live, k)
    }

    /// A `match` with an arm for every constructor of its type, none guarded
    /// and none looking inside a field in a way that can fail: one `switch`,
    /// and nothing to fall back to, so no failure object is built. Most
    /// `match`es are this, and it is smaller code than the chain as well as
    /// faster, so it is not gated.
    fn case_total(&mut self, s: Name, arms: &[Arm], live: &[Name], k: Name) -> Option<Statement> {
        // Every constructor of the type, each arm unguarded and matching all of
        // its fields: nothing can fail, so there is no fallback to build. This
        // is most `match`es there are, and it saves an object on each.
        let scrutinee_ty = self.type_of_name(s);
        let covered = match &scrutinee_ty {
            Some(core::Ty::Con(name, _)) => self.variants.get(name).map(|vs| vs.len()),
            _ => None,
        };
        let n = arms.len();
        let distinct: HashSet<InternedString> = arms
            .iter()
            .filter_map(|(p, _, _)| match p {
                Pat::Ctor(name, _) => Some(*name),
                _ => None,
            })
            .collect();
        if n >= 1
            && covered == Some(n)
            && distinct.len() == n
            && arms.iter().all(|(p, g, _)| {
                g.is_none() && matches!(p, Pat::Ctor(_, subs) if subs.iter().all(irrefutable))
            })
        {
            let unused = self.fresh_ref();
            let mut switch_arms = Vec::with_capacity(n);
            for (pat, _, term) in arms {
                let Pat::Ctor(ctor, subs) = pat else {
                    unreachable!("every arm is a constructor pattern");
                };
                let tag = self.tag_of(*ctor);
                let fields = self.field_names(*ctor, subs, scrutinee_ty.as_ref());
                let mut arm_env = fields.clone();
                arm_env.extend_from_slice(live);
                let pairs: Vec<(&Pat, Name)> = subs.iter().zip(fields.iter().copied()).collect();
                let body = self.match_all(
                    pairs,
                    arm_env.clone(),
                    unused,
                    Box::new(move |this, env1| this.expr(term, &env1, k)),
                );
                switch_arms.push((
                    tag,
                    Block {
                        params: arm_env,
                        body,
                    },
                ));
            }
            Some(Statement::Switch {
                scrutinee: s,
                arms: switch_arms,
                default: Box::new(Block {
                    params: live.to_vec(),
                    body: Statement::Error("non-exhaustive pattern match"),
                }),
            })
        } else {
            None
        }
    }

    /// The leading arms that each name a different constructor, unguarded and
    /// with nothing inside that can fail, as one `switch` -- and whatever
    /// follows them as its default: `match k with | A -> 1 | B x -> 2 | _ -> 0`.
    ///
    /// Once such an arm's tag has matched the arm is taken, so there is
    /// nothing to fall back to and no failure object is built; a value of
    /// none of those constructors goes on to the arms after, which is the
    /// default's block, compiled as any `match`. Like [`Lower::case_total`],
    /// which is this with nothing after, it is smaller than the chain as well
    /// as faster, so it is not gated.
    ///
    /// The chain made an object an arm before testing anything -- each
    /// holding everything live and the one after it -- and a function an arm
    /// to test in: a `match` on a token's kind with 78 arms and a `_` was
    /// eighty allocations a time it ran, and half a megabyte of LLVM IR.
    fn case_switch(&mut self, s: Name, arms: &[Arm], live: &[Name], k: Name) -> Option<Statement> {
        let mut seen: HashSet<InternedString> = HashSet::new();
        let n = arms
            .iter()
            .take_while(|(p, g, _)| match p {
                Pat::Ctor(name, subs) => {
                    g.is_none() && subs.iter().all(irrefutable) && seen.insert(*name)
                }
                _ => false,
            })
            .count();
        // Two at least: one is what the arms after it are for.
        if n < 2 {
            return None;
        }
        let scrutinee_ty = self.type_of_name(s);
        let unused = self.fresh_ref();
        let mut switch_arms = Vec::with_capacity(n);
        for (pat, _, term) in &arms[..n] {
            let Pat::Ctor(ctor, subs) = pat else {
                unreachable!("the prefix is constructor patterns");
            };
            let tag = self.tag_of(*ctor);
            let fields = self.field_names(*ctor, subs, scrutinee_ty.as_ref());
            let mut arm_env = fields.clone();
            arm_env.extend_from_slice(live);
            let pairs: Vec<(&Pat, Name)> = subs.iter().zip(fields.iter().copied()).collect();
            let body = self.match_all(
                pairs,
                arm_env.clone(),
                unused,
                Box::new(move |this, env1| this.expr(term, &env1, k)),
            );
            switch_arms.push((
                tag,
                Block {
                    params: arm_env,
                    body,
                },
            ));
        }
        let rest = if n == arms.len() {
            Statement::Error("non-exhaustive pattern match")
        } else {
            self.case(s, &arms[n..], live.to_vec(), k)
        };
        Some(Statement::Switch {
            scrutinee: s,
            arms: switch_arms,
            default: Box::new(Block {
                params: live.to_vec(),
                body: rest,
            }),
        })
    }

    /// The leading arms that test a literal, as a chain of compare-and-branch.
    ///
    /// A literal pattern has nothing inside it to fail half-way through, so an
    /// arm's failure is its own comparison coming out false, and the next test
    /// can simply be the false branch. The chain builds no failure objects --
    /// it used to build one per arm before testing anything, which made a
    /// `match` on a byte cost twenty allocations -- and LLVM turns a run of
    /// equality tests on one value into a `switch`. Whatever follows the
    /// literals is the false branch of the last test, compiled as any `match`.
    ///
    /// An arm with a guard ends the run: failing a guard means going on to the
    /// next arm, which is what the chain's objects are for.
    fn case_literals(
        &mut self,
        s: Name,
        arms: &[Arm],
        live: &[Name],
        k: Name,
    ) -> Option<Statement> {
        let n = arms
            .iter()
            .take_while(|(p, g, _)| matches!(p, Pat::Lit(_)) && g.is_none())
            .count();
        if n == 0 {
            return None;
        }
        let mut stmt = if n == arms.len() {
            Statement::Error("non-exhaustive pattern match")
        } else {
            self.case(s, &arms[n..], live.to_vec(), k)
        };
        for (pat, _, term) in arms[..n].iter().rev() {
            let Pat::Lit(lit) = pat else {
                unreachable!("the prefix is literal patterns");
            };
            let yes = self.expr(term, live, k);
            stmt = Statement::Extern {
                op: Extern::BranchPrimK(core::Prim::Eq, lit.clone()),
                args: vec![s],
                blocks: vec![
                    Block {
                        params: live.to_vec(),
                        body: stmt,
                    },
                    Block {
                        params: live.to_vec(),
                        body: yes,
                    },
                ],
            };
        }
        Some(stmt)
    }

    /// One `switch` over the arms that dispatch on a constructor, instead of one
    /// per arm.
    ///
    /// The chain below tests `Cons`, then — having failed — tests `Nil`, then
    /// tests whatever is next, and builds a failure object before any of it. A
    /// `match` over a wide type does that work per arm even though a value has
    /// exactly one tag. This takes the longest **prefix** of arms that are
    /// constructor patterns on distinct constructors and gives them one `switch`
    /// with one shared fallback, which is the whole of the common case; anything
    /// the prefix does not cover — a wildcard, a literal, a repeated constructor
    /// — is the fallback, compiled as a chain exactly as before, so arm order is
    /// preserved without having to reason about it.
    ///
    /// Returns `None` when there is nothing to gain: fewer than two arms would
    /// join the switch.
    ///
    /// An arm with a guard stays in the switch, and failing its guard is
    /// failing its sub-patterns: the shared fallback. That skips the rest of
    /// the prefix, which is right because none of it could match -- each arm
    /// there is for a different constructor.
    ///
    /// This is the trade [`OptLevel::case_trees`] gates. Nested patterns are not
    /// distributed across the arms — `Just (Cons x xs)` still falls back to the
    /// chain when its inner pattern fails, and so retests the outer `Just` —
    /// because that is where a real decision tree starts duplicating the code
    /// its arms share.
    fn case_tree(&mut self, s: Name, arms: &[Arm], live: &[Name], k: Name) -> Option<Statement> {
        let mut seen: HashSet<InternedString> = HashSet::new();
        let n = arms
            .iter()
            .take_while(|(p, _, _)| match p {
                Pat::Ctor(name, _) => seen.insert(*name),
                _ => false,
            })
            .count();
        if n < 2 {
            return None;
        }

        // One object for "none of these matched", shared by every arm — both by
        // the `default`, and by an arm whose *sub*-patterns fail after its tag
        // matched. It captures the environment as it stands, which includes the
        // scrutinee: the arms that follow still have to look at it.
        // `new` *prepends*, so this is the environment the switch runs in — the
        // order matters, and getting it wrong is an ill-formed program rather
        // than a slow one.
        let rest = self.fresh_ref();
        let scrutinee_ty = self.type_of_name(s);
        let mut env = vec![rest];
        env.extend_from_slice(live);

        let mut switch_arms = Vec::with_capacity(n);
        for (pat, guard, term) in &arms[..n] {
            let Pat::Ctor(ctor, subs) = pat else {
                unreachable!("the prefix is constructor patterns");
            };
            let tag = self.tag_of(*ctor);
            let fields = self.field_names(*ctor, subs, scrutinee_ty.as_ref());
            let mut arm_env = fields.clone();
            arm_env.extend_from_slice(&env);
            let pairs: Vec<(&Pat, Name)> = subs.iter().zip(fields.iter().copied()).collect();
            let body = self.match_all(
                pairs,
                arm_env.clone(),
                rest,
                Box::new(move |this, env1| match guard {
                    None => this.expr(term, &env1, k),
                    Some(g) => this.guarded(g, term, rest, &env1, k),
                }),
            );
            switch_arms.push((
                tag,
                Block {
                    params: arm_env,
                    body,
                },
            ));
        }

        let miss = self.enter(rest);
        let switch = Statement::Switch {
            scrutinee: s,
            arms: switch_arms,
            default: Box::new(Block {
                params: env,
                body: miss,
            }),
        };

        let body = if n == arms.len() {
            Statement::Error("non-exhaustive pattern match")
        } else {
            self.case(s, &arms[n..], live.to_vec(), k)
        };
        Some(Statement::New {
            name: rest,
            captures: live.to_vec(),
            methods: vec![Block {
                params: live.to_vec(),
                body,
            }],
            rest: Box::new(switch),
        })
    }

    fn case_chain<'t>(&mut self, s: Name, arms: &'t [Arm], live: Vec<Name>, k: Name) -> Statement {
        let n = arms.len();
        let fails: Vec<Name> = (0..=n).map(|_| self.fresh_ref()).collect();

        // The chain is entered by invoking the first arm's object.
        let mut stmt = self.enter(fails[0]);

        // Wrap innermost-first, so `f_n` — the one nobody captures — ends up
        // outermost and is therefore built first at run time.
        for i in 0..=n {
            let (captures, body) = if i == n {
                (
                    live.clone(),
                    Statement::Error("non-exhaustive pattern match"),
                )
            } else {
                let fail = fails[i + 1];
                let mut caps = live.clone();
                caps.push(fail);
                let (pat, guard, term) = &arms[i];
                let body = self.match_pat(
                    pat,
                    s,
                    caps.clone(),
                    fail,
                    Box::new(move |this, env| match guard {
                        None => this.expr(term, &env, k),
                        Some(g) => this.guarded(g, term, fail, &env, k),
                    }),
                );
                (caps, body)
            };
            stmt = Statement::New {
                name: fails[i],
                captures: captures.clone(),
                methods: vec![Block {
                    params: captures,
                    body,
                }],
                rest: Box::new(stmt),
            };
        }
        stmt
    }

    /// An arm whose pattern has matched, in `env`: `term` if `guard` holds,
    /// and the next arm -- by invoking `fail`, exactly as a pattern that did
    /// not match does -- if it does not.
    fn guarded(
        &mut self,
        guard: &Term,
        term: &Term,
        fail: Name,
        env: &[Name],
        k: Name,
    ) -> Statement {
        let keep = self.keep(env, &[term], &[k, fail]);
        let term = term.clone();
        self.bind(
            guard,
            env,
            &keep,
            None,
            Box::new(move |this, cv, env1| {
                let taken = this.expr(&term, &env1, k);
                let passed = this.enter(fail);
                Statement::Extern {
                    op: Extern::Branch,
                    args: vec![cv],
                    blocks: vec![
                        Block {
                            params: env1.clone(),
                            body: passed,
                        },
                        Block {
                            params: env1,
                            body: taken,
                        },
                    ],
                }
            }),
        )
    }

    /// Match `p` against `subject`; on success run `ok`, on failure invoke
    /// `fail`.
    fn match_pat<'t>(
        &mut self,
        p: &'t Pat,
        subject: Name,
        env: Vec<Name>,
        fail: Name,
        ok: Success<'t>,
    ) -> Statement {
        match p {
            Pat::Wild => ok(self, env),

            // Binding is a rename: put the value at the end of the environment
            // under the pattern's name. `VarId`s are unique per binding site, so
            // this can never collide with something already there.
            Pat::Var(v, _) => self.rebind(subject, *v, env, ok),

            Pat::As(v, _, sub) => {
                let v = *v;
                self.rebind(
                    subject,
                    v,
                    env,
                    Box::new(move |this, env1| this.match_pat(sub, subject, env1, fail, ok)),
                )
            }

            // One statement: the literal rides in the `extern`, and the boolean
            // it is compared against never exists. This used to be three — load
            // the literal, compare, test — and it is the commonest pattern
            // there is.
            Pat::Lit(l) => {
                let no = self.enter(fail);
                let yes = ok(self, env.clone());
                Statement::Extern {
                    op: Extern::BranchPrimK(core::Prim::Eq, l.clone()),
                    args: vec![subject],
                    blocks: vec![
                        Block {
                            params: env.clone(),
                            body: no,
                        },
                        Block {
                            params: env,
                            body: yes,
                        },
                    ],
                }
            }

            // The one place a real `switch` appears. The constructor's fields go
            // on the front; the scrutinee stays, because an arm body may name it
            // — `match o with | Just x -> o` is ordinary.
            Pat::Ctor(name, subs) => {
                let tag = self.tag_of(*name);
                let of = self.type_of_name(subject);
                let fields = self.field_names(*name, subs, of.as_ref());
                let mut arm_env = fields.clone();
                arm_env.extend_from_slice(&env);
                let pairs: Vec<(&Pat, Name)> = subs.iter().zip(fields.iter().copied()).collect();
                let body = self.match_all(pairs, arm_env.clone(), fail, ok);
                let miss = self.enter(fail);
                Statement::Switch {
                    scrutinee: subject,
                    arms: vec![(
                        tag,
                        Block {
                            params: arm_env,
                            body,
                        },
                    )],
                    default: Box::new(Block {
                        params: env,
                        body: miss,
                    }),
                }
            }

            Pat::Tuple(subs) => self.fields(subs, subject, env, fail, ok),

            Pat::Array(subs) => {
                // `#[p, …]` matches an array of exactly this length, so the
                // length test comes first and the extractions follow.
                let want = subs.len() as i64;
                self.produces(
                    Extern::Prim(core::Prim::ArrayLen),
                    vec![subject],
                    &env,
                    Some(con("Int")),
                    move |this, n, env1| {
                        let no = this.enter(fail);
                        let yes = this.fields(subs, subject, env1.clone(), fail, ok);
                        Statement::Extern {
                            op: Extern::BranchPrimK(core::Prim::Eq, core::Lit::Int(want)),
                            args: vec![n],
                            blocks: vec![
                                Block {
                                    params: env1.clone(),
                                    body: no,
                                },
                                Block {
                                    params: env1,
                                    body: yes,
                                },
                            ],
                        }
                    },
                )
            }

            // A record pattern names the fields it wants and ignores the rest,
            // which is what row polymorphism means here.
            Pat::Record(fields) => {
                fn go<'t>(
                    this: &mut Lower,
                    fields: &'t [(InternedString, Pat)],
                    subject: Name,
                    env: Vec<Name>,
                    fail: Name,
                    ok: Success<'t>,
                ) -> Statement {
                    match fields.split_first() {
                        None => ok(this, env),
                        Some(((label, p), rest)) => {
                            let of = this.type_of_name(subject);
                            let ty = field_type(Lower::label_type(of.as_ref(), *label), p);
                            let op = this.select(of, subject, *label);
                            this.produces(op, vec![subject], &env, ty, |this, x, env1| {
                                this.match_pat(
                                    p,
                                    x,
                                    env1,
                                    fail,
                                    Box::new(move |this, env2| {
                                        go(this, rest, subject, env2, fail, ok)
                                    }),
                                )
                            })
                        }
                    }
                }
                go(self, fields, subject, env, fail, ok)
            }
        }
    }

    /// Extract `subs.len()` positional fields from `subject` and match each.
    fn fields<'t>(
        &mut self,
        subs: &'t [Pat],
        subject: Name,
        env: Vec<Name>,
        fail: Name,
        ok: Success<'t>,
    ) -> Statement {
        fn go<'t>(
            this: &mut Lower,
            subs: &'t [Pat],
            i: usize,
            subject: Name,
            env: Vec<Name>,
            got: Vec<(usize, Name)>,
            fail: Name,
            ok: Success<'t>,
        ) -> Statement {
            if i == subs.len() {
                let pairs: Vec<(&Pat, Name)> =
                    got.into_iter().map(|(j, n)| (&subs[j], n)).collect();
                return this.match_all(pairs, env, fail, ok);
            }
            // A field nothing looks at is not read: `_` binds no name, so
            // there is nothing to hold it -- and nothing to know its type by,
            // where the value's own type says nothing.
            if matches!(subs[i], Pat::Wild) {
                return go(this, subs, i + 1, subject, env, got, fail, ok);
            }
            let ty = match this.type_of_name(subject) {
                Some(core::Ty::Tuple(items)) => items.get(i).cloned(),
                Some(core::Ty::Con(_, args)) => args.first().cloned(),
                _ => None,
            };
            let ty = field_type(ty, &subs[i]);
            this.produces(
                Extern::Field(i),
                vec![subject],
                &env,
                ty,
                move |this, x, env1| {
                    let mut got = got;
                    got.push((i, x));
                    go(this, subs, i + 1, subject, env1, got, fail, ok)
                },
            )
        }
        go(self, subs, 0, subject, env, Vec::new(), fail, ok)
    }

    /// Match a list of patterns against a list of values, left to right.
    fn match_all<'t>(
        &mut self,
        pairs: Vec<(&'t Pat, Name)>,
        env: Vec<Name>,
        fail: Name,
        ok: Success<'t>,
    ) -> Statement {
        match pairs.split_first() {
            None => ok(self, env),
            Some(((p, x), rest)) => {
                let (p, x) = (*p, *x);
                let rest = rest.to_vec();
                self.match_pat(
                    p,
                    x,
                    env,
                    fail,
                    Box::new(move |this, env1| this.match_all(rest, env1, fail, ok)),
                )
            }
        }
    }

    /// Give `subject`'s value a second name at the end of the environment.
    fn rebind(
        &mut self,
        subject: Name,
        as_name: Name,
        env: Vec<Name>,
        ok: Success<'_>,
    ) -> Statement {
        let mut sel = env.clone();
        sel.push(subject);
        let mut params = env;
        params.push(as_name);
        let body = ok(self, params.clone());
        Statement::Substitute(sel, Box::new(Block { params, body }))
    }

    // --- effects ----------------------------------------------------------

    /// The evidence key of an operation: with its effect's version, where it
    /// has one, so that a handler of another build's version of the effect
    /// is not what it finds (see `meadow_core::versions`).
    fn key(&self, effect: InternedString, op: InternedString) -> InternedString {
        let key = match self.versions.effects.get(&effect) {
            Some(v) => format!("{effect}#{v}.{op}"),
            None => format!("{effect}.{op}"),
        };
        InternedString::from(key.as_str())
    }

    /// `perform effect.op av` answering `k`, with evidence `ev`.
    ///
    /// Each `perform` gets a block of its own that walks the evidence, lifted
    /// out the way a `letrec` binding is, with the argument and the
    /// continuation riding along: `[ev, av, k]`. Its key is a constant of the
    /// comparison, so the search allocates nothing and names nothing, and the
    /// innermost handler -- nearly always the one -- is one test away.
    fn perform(
        &mut self,
        effect: InternedString,
        op: InternedString,
        av: Name,
        k: Name,
        ev: Name,
        (arg_ty, res_ty): (Option<core::Ty>, Option<core::Ty>),
    ) -> Statement {
        let label = self.fresh_label();
        let (e, a, kk) = (self.fresh_ref(), self.fresh_typed(arg_ty), self.fresh_ref());
        let params = vec![e, a, kk];
        let key = core::Lit::Sym(self.key(effect, op));
        let mut arms = Vec::new();
        for (ctor, tail) in [(EV, false), (EV_TAIL, true)] {
            let ekey = self.fresh_as(Rep::Str);
            let (clause, target, rest) = (self.fresh_ref(), self.fresh_ref(), self.fresh_ref());
            let mut fields = vec![ekey, clause, target, rest];
            fields.extend_from_slice(&params);
            let hit = self.dispatch(tail, clause, target, a, kk, &fields, res_ty.clone());
            let next = Statement::Substitute(
                vec![rest, a, kk],
                Box::new(Block {
                    params: params.clone(),
                    body: Statement::Jump(label),
                }),
            );
            arms.push((
                self.tag_of(InternedString::from(ctor)),
                Block {
                    params: fields.clone(),
                    body: Statement::Extern {
                        op: Extern::BranchPrimK(core::Prim::Eq, key.clone()),
                        args: vec![ekey],
                        blocks: vec![
                            Block {
                                params: fields.clone(),
                                body: next,
                            },
                            Block {
                                params: fields,
                                body: hit,
                            },
                        ],
                    },
                },
            ));
        }
        let none = self.native(effect, op, a, kk, &params, res_ty);
        self.defs.push(Def {
            label,
            name: InternedString::from(format!("<perform {effect}.{op}>").as_str()),
            module: self.module,
            block: Block {
                params: params.clone(),
                body: Statement::Switch {
                    scrutinee: e,
                    arms,
                    default: Box::new(Block { params, body: none }),
                },
            },
        });
        Statement::Substitute(
            vec![ev, av, k],
            Box::new(Block {
                params: vec![ev, av, k],
                body: Statement::Jump(label),
            }),
        )
    }

    /// The operation is `clause`'s, in `env`. A tail-resumptive clause is a
    /// call answering `k` directly; any other gets a one-shot resumption.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &mut self,
        tail: bool,
        clause: Name,
        target: Name,
        av: Name,
        k: Name,
        env: &[Name],
        res_ty: Option<core::Ty>,
    ) -> Statement {
        if tail {
            return Statement::Substitute(
                vec![clause, av, k],
                Box::new(Block {
                    params: vec![clause, av, k],
                    body: Statement::Invoke(clause, 0),
                }),
            );
        }
        let flag = self.fresh_as(Rep::Bits(core::desc::UNIT));
        let taken = self.fresh_ref();
        let seg = self.fresh_ref();
        let r = self.fresh_ref();
        let kh = self.fresh_ref();
        let mut env1 = vec![flag];
        env1.extend_from_slice(env);
        let mut env2 = vec![taken];
        env2.extend_from_slice(&env1);
        let mut env2s = vec![seg];
        env2s.extend_from_slice(&env2);
        let mut env3 = vec![r];
        env3.extend_from_slice(&env2s);
        let mut env4 = vec![kh];
        env4.extend_from_slice(&env3);
        let enter = Statement::Substitute(
            vec![clause, av, r, kh],
            Box::new(Block {
                params: vec![clause, av, r, kh],
                body: Statement::Invoke(clause, 0),
            }),
        );
        let resume = self.resumption(k, target, taken, seg, res_ty);
        Statement::Extern {
            op: Extern::Lit(core::Lit::Unit),
            args: vec![],
            blocks: vec![Block {
                params: env1,
                body: Statement::Extern {
                    op: Extern::Prim(core::Prim::Once),
                    args: vec![flag],
                    blocks: vec![Block {
                        params: env2,
                        // The frames between here and the handler, cut off the
                        // stack: what the resumption puts back. Cut before the
                        // handler's continuation is read, because the clause
                        // runs below the cut.
                        body: Statement::Extern {
                            op: Extern::Prim(core::Prim::Detach),
                            args: vec![target],
                            blocks: vec![Block {
                                params: env2s,
                                body: Statement::New {
                                    name: r,
                                    // The flag first: it is what a thread send
                                    // or a `compact` meets first, and what
                                    // makes it refuse this as a continuation.
                                    captures: vec![taken, k, target, seg],
                                    methods: vec![resume],
                                    rest: Box::new(Statement::Extern {
                                        op: Extern::Prim(core::Prim::GetRef),
                                        args: vec![target],
                                        blocks: vec![Block {
                                            params: env4,
                                            body: enter,
                                        }],
                                    }),
                                },
                            }],
                        },
                    }],
                },
            }],
        }
    }

    /// No handler in the program answers: the runtime does.
    fn native(
        &mut self,
        effect: InternedString,
        op: InternedString,
        av: Name,
        k: Name,
        env: &[Name],
        res_ty: Option<core::Ty>,
    ) -> Statement {
        self.produces(
            Extern::Native(effect, op),
            vec![av],
            env,
            res_ty,
            |this, out, _| this.ret(k, out),
        )
    }

    /// The method of a resumption capturing `[taken, k, target]`, called with a
    /// value, a continuation and evidence it has no use for: run once, send
    /// the handler's value to the caller from now on, and continue at `k`.
    fn resumption(
        &mut self,
        k: Name,
        target: Name,
        taken: Name,
        seg: Name,
        ty: Option<core::Ty>,
    ) -> Block {
        let (v, c, ev) = (self.fresh_typed(ty), self.fresh_ref(), self.fresh_ref());
        self.returns.insert(c);
        let params = vec![taken, k, target, seg, v, c, ev];
        let first = self.fresh_as(Rep::Bits(core::desc::BOOL));
        let mut at_first = vec![first];
        at_first.extend_from_slice(&params);
        let u = self.fresh_as(Rep::Bits(core::desc::UNIT));
        let mut at_u = vec![u];
        at_u.extend_from_slice(&at_first);
        let back = self.fresh_as(Rep::Bits(core::desc::UNIT));
        let mut at_back = vec![back];
        at_back.extend_from_slice(&at_u);
        let go = self.ret(k, v);
        Block {
            params,
            body: Statement::Extern {
                op: Extern::Prim(core::Prim::TakeOnce),
                args: vec![taken],
                blocks: vec![Block {
                    params: at_first.clone(),
                    body: Statement::Extern {
                        op: Extern::Branch,
                        args: vec![first],
                        blocks: vec![
                            Block {
                                params: at_first.clone(),
                                body: Statement::Error("continuation resumed more than once"),
                            },
                            Block {
                                params: at_first,
                                body: Statement::Extern {
                                    op: Extern::Prim(core::Prim::SetRef),
                                    args: vec![target, c],
                                    blocks: vec![Block {
                                        params: at_u,
                                        // The segment back on the stack, and
                                        // then into `k`, its topmost frame.
                                        body: Statement::Extern {
                                            op: Extern::Prim(core::Prim::Reattach),
                                            args: vec![seg],
                                            blocks: vec![Block {
                                                params: at_back,
                                                body: go,
                                            }],
                                        },
                                    }],
                                },
                            },
                        ],
                    },
                }],
            },
        }
    }

    /// `handle body with { … }` answering `k`, in `env`. See the module docs.
    fn handle(
        &mut self,
        body: &Term,
        clauses: &[core::HClause],
        ret: Option<&(Var, core::Ty, std::sync::Arc<Term>)>,
        env: &[Name],
        k: Name,
    ) -> Statement {
        let outer = self.ev;

        // Each step binds one name in front of the environment; the statements
        // are assembled afterwards, innermost first.
        enum Step {
            Target(Name),
            Clause(Name, Vec<Name>, Block),
            Key(Name, InternedString),
            Entry(Name, &'static str, Vec<Name>),
            Return(Name, Vec<Name>, Block),
        }
        let mut steps: Vec<(Step, Vec<Name>)> = Vec::new();
        let mut cur: Vec<Name> = env.to_vec();
        let bind = |cur: &mut Vec<Name>, n: Name| {
            let before = cur.clone();
            cur.insert(0, n);
            before
        };

        let target = self.fresh_ref();
        let before = bind(&mut cur, target);
        steps.push((Step::Target(target), before));

        let mut ev = outer;
        for c in clauses {
            // A tail-resumptive clause computes the value to resume with and
            // hands it straight to the performing code's continuation: `k e`
            // would have done exactly that, and then sent whatever came of it
            // where this clause's value goes, which is the same place.
            let tail = tail_resumptive(c);
            let (captures, params, clause_body) = match tail {
                Some(e) => {
                    let captures = restrict(&cur, &self.wants(&[e], &[c.param]));
                    let kh = self.fresh_ref();
                    self.continuations.insert(kh);
                    let mut params = captures.clone();
                    params.push(c.param);
                    params.push(kh);
                    let body = self.expr(e, &params, kh);
                    (captures, params, body)
                }
                None => {
                    let captures = restrict(&cur, &self.wants(&[&c.body], &[c.param, c.resume]));
                    let kh = self.function_return();
                    let mut params = captures.clone();
                    params.push(c.param);
                    params.push(c.resume);
                    params.push(kh);
                    let body = self.expr(&c.body, &params, kh);
                    (captures, params, body)
                }
            };
            let obj = self.fresh_ref();
            let before = bind(&mut cur, obj);
            steps.push((
                Step::Clause(
                    obj,
                    captures,
                    Block {
                        params,
                        body: clause_body,
                    },
                ),
                before,
            ));

            let key = self.fresh_as(Rep::Str);
            let before = bind(&mut cur, key);
            steps.push((Step::Key(key, self.key(c.effect, c.op)), before));

            let entry = self.fresh_ref();
            let before = bind(&mut cur, entry);
            let ctor = if tail.is_some() { EV_TAIL } else { EV };
            steps.push((Step::Entry(entry, ctor, vec![key, obj, target, ev]), before));
            ev = entry;
        }

        // The body's continuation: wherever `target` says, through the
        // `return` clause, outside the handler.
        let h = self.fresh_ref();
        let (x, ret_body) = match ret {
            Some((p, _, t)) => (*p, Some(&**t)),
            None => {
                let ty = self.type_of(body);
                (self.fresh_typed(ty), None)
            }
        };
        let mut want = match ret_body {
            Some(t) => self.wants(&[t], &[x]),
            None => HashSet::new(),
        };
        want.insert(target);
        let caps = restrict(&cur, &want);
        let mut params = caps.clone();
        params.push(x);
        let kk = self.fresh_ref();
        let mut at_kk = vec![kk];
        at_kk.extend_from_slice(&params);
        let inner = match ret_body {
            Some(t) => self.expr(t, &at_kk, kk),
            None => self.ret(kk, x),
        };
        let h_method = Block {
            params: params.clone(),
            body: Statement::Extern {
                op: Extern::Prim(core::Prim::GetRef),
                args: vec![target],
                blocks: vec![Block {
                    params: at_kk,
                    body: inner,
                }],
            },
        };
        let before = bind(&mut cur, h);
        steps.push((Step::Return(h, caps, h_method), before));

        self.ev = ev;
        let mut stmt = self.expr(body, &cur, h);
        self.ev = outer;

        for (step, before) in steps.into_iter().rev() {
            let with = |n: Name| {
                let mut p = vec![n];
                p.extend_from_slice(&before);
                p
            };
            stmt = match step {
                // The target, and then a chunk of the frame stack for the
                // body: what lets a general clause cut the body's frames off
                // at a chunk boundary rather than copy them.
                Step::Target(n) => {
                    let u = self.fresh_as(Rep::Bits(core::desc::UNIT));
                    let mut with_u = vec![u];
                    with_u.extend(with(n));
                    // `stmt` was lowered for the environment without the
                    // unit `Enter` answers, so that is dropped again first.
                    let dropped = Statement::Substitute(
                        with(n),
                        Box::new(Block {
                            params: with(n),
                            body: stmt,
                        }),
                    );
                    Statement::Extern {
                        op: Extern::Prim(core::Prim::NewRef),
                        args: vec![k],
                        blocks: vec![Block {
                            params: with(n),
                            body: Statement::Extern {
                                op: Extern::Prim(core::Prim::Enter),
                                args: vec![n],
                                blocks: vec![Block {
                                    params: with_u,
                                    body: dropped,
                                }],
                            },
                        }],
                    }
                }
                Step::Clause(n, captures, method) | Step::Return(n, captures, method) => {
                    Statement::New {
                        name: n,
                        captures,
                        methods: vec![method],
                        rest: Box::new(stmt),
                    }
                }
                Step::Key(n, key) => Statement::Extern {
                    op: Extern::Lit(core::Lit::Sym(key)),
                    args: vec![],
                    blocks: vec![Block {
                        params: with(n),
                        body: stmt,
                    }],
                },
                Step::Entry(n, ctor, fields) => Statement::Let {
                    name: n,
                    tag: self.tag_of(InternedString::from(ctor)),
                    ctor: InternedString::from(ctor),
                    fields,
                    rest: Box::new(stmt),
                },
            };
        }
        stmt
    }
}

/// A type abstraction with at least one binder that takes a descriptor: the
/// abstraction itself, its binders, and its body.
fn ty_abs(t: &Term) -> Option<(&Term, &[core::TyVar], &Term)> {
    let mut cur = t;
    while let Term::Loc(_, inner) = cur {
        cur = inner;
    }
    match cur {
        Term::TyLam(vs, body) if vs.iter().any(|v| v.kind == VarKind::Type) => {
            Some((cur, vs.as_slice(), &**body))
        }
        _ => None,
    }
}

/// A representation as a runtime starting a block knows it: with no
/// descriptors to hand, a type variable's is not known.
fn known(rep: Rep) -> Rep {
    match rep {
        Rep::Var(_) => Rep::Unknown,
        rep => rep,
    }
}

/// Which of `vs` take a descriptor: the ones over types, not rows or effects.
fn described(vs: &[core::TyVar]) -> Vec<usize> {
    vs.iter()
        .enumerate()
        .filter(|(_, v)| v.kind == VarKind::Type)
        .map(|(i, _)| i)
        .collect()
}

/// Where a term is, as a key: the program being lowered does not move.
fn address(t: &Term) -> usize {
    t as *const Term as usize
}

/// The type arguments of the instantiation at the head of a call.
fn call_tys(t: &Term) -> Option<&Vec<core::Ty>> {
    let mut cur = t.peel();
    while let Term::App(f, _) = cur {
        cur = f.peel();
    }
    match cur {
        Term::TyApp(_, tys) => Some(tys),
        _ => None,
    }
}

/// A type with no parameters, by name.
fn con(name: &str) -> core::Ty {
    core::Ty::Con(InternedString::from(name), Vec::new())
}

/// Where `label` is in a record of type `of`, if that is a closed row that has
/// it -- and so exactly the labels it lists, whatever built the record.
fn place(of: Option<&core::Ty>, label: InternedString) -> Option<crate::Place> {
    let core::Ty::Record(row) = of? else {
        return None;
    };
    let mut labels = Vec::new();
    let mut cur = &**row;
    while let core::Ty::RowExtend(l, _, rest) = cur {
        labels.push(*l);
        cur = rest;
    }
    if !matches!(cur, core::Ty::RowEmpty) || !labels.contains(&label) {
        return None;
    }
    // A record holds each label once: a row that lists one twice -- which is
    // what `type_of` makes of `{ r | x = 1 }` where `r` has an `x` -- means
    // the one field.
    labels.sort();
    labels.dedup();
    Some(crate::Place {
        at: labels.iter().position(|l| *l == label)? as u32,
        of: labels.len() as u32,
    })
}

/// The type of a field a pattern matches: `derived` from the value's type,
/// unless that says nothing -- because the value's type is not known, or is
/// a specialized copy's `#Ref`, which is a representation and not a type --
/// in which case what the pattern itself was checked at, where it names the
/// field.
fn field_type(derived: Option<core::Ty>, p: &Pat) -> Option<core::Ty> {
    match derived {
        Some(ty) if !matches!(ty, core::Ty::Var(_)) && !core::is_unknown(&ty) => Some(ty),
        derived => match p {
            Pat::Var(_, ty) | Pat::As(_, ty, _) => Some(ty.clone()),
            _ => derived.filter(|ty| !core::is_unknown(ty)),
        },
    }
}

/// How many type arguments a field type written over `Bound` indices needs:
/// one more than the largest index it mentions.
fn bound_arity(ty: &meadow_infer::Type) -> usize {
    use meadow_infer::Type;
    match ty {
        Type::Bound(i) => *i as usize + 1,
        Type::Con(_, args) | Type::Tuple(args) => args.iter().map(bound_arity).max().unwrap_or(0),
        Type::Fun(args, ret, eff) => args
            .iter()
            .chain([&**ret, &**eff])
            .map(bound_arity)
            .max()
            .unwrap_or(0),
        Type::Record(row) => bound_arity(row),
        Type::RowExtend(_, field, rest) => bound_arity(field).max(bound_arity(rest)),
        _ => 0,
    }
}

fn lit_type(l: &core::Lit) -> core::Ty {
    use core::{Lit, Ty};
    let con = |n: &str| Ty::Con(InternedString::from(n), Vec::new());
    match l {
        Lit::Int(_) => con("Int"),
        Lit::BigInt(_) => con("BigInt"),
        Lit::Float(_) => con("Float"),
        Lit::Word(w, _) => con(w.name()),
        Lit::Float32(_) => con("Float32"),
        Lit::AnyInt(_, v) | Lit::AnyFloat(_, v) => Ty::Var(*v),
        Lit::Str(_) => con("String"),
        Lit::Sym(_) => con(core::desc::SYMBOL_TYPE),
        Lit::Char(_) => con("Char"),
        Lit::Bool(_) => con("Bool"),
        Lit::Unit => con("Unit"),
    }
}

/// The names of `env`, in order, that are in `want` — the environment shape a
/// continuation should capture.
///
/// Order comes from the environment rather than from the set, so lowering is
/// deterministic: a `HashSet`'s iteration order is not.
fn restrict(env: &[Name], want: &HashSet<Var>) -> Vec<Name> {
    let mut seen = HashSet::new();
    env.iter()
        .filter(|n| want.contains(n) && seen.insert(**n))
        .copied()
        .collect()
}

/// Every variable a term mentions, bound or free — only for sizing the fresh
/// counter, where the distinction does not matter.
fn mentions(t: &Term, out: &mut HashSet<Var>) {
    match t {
        Term::TyLam(_, b) | Term::TyApp(b, _) | Term::Loc(_, b) => mentions(b, out),
        Term::Var(v) => {
            out.insert(*v);
        }
        Term::Lit(_) | Term::Error => {}
        Term::Lam(p, _, b) => {
            out.insert(*p);
            mentions(b, out);
        }
        Term::App(f, a) => {
            mentions(f, out);
            mentions(a, out);
        }
        Term::Let(x, _, r, b) => {
            out.insert(*x);
            mentions(r, out);
            mentions(b, out);
        }
        Term::Join {
            var,
            params,
            rhs,
            body,
            ..
        } => {
            out.insert(*var);
            out.extend(params.iter().map(|(v, _)| *v));
            mentions(rhs, out);
            mentions(body, out);
        }
        Term::Jump(j, args, _) => {
            out.insert(*j);
            args.iter().for_each(|a| mentions(a, out));
        }
        Term::LetRec(binds, body) => {
            for (v, _, t) in binds {
                out.insert(*v);
                mentions(t, out);
            }
            mentions(body, out);
        }
        Term::If(a, b, c) => {
            mentions(a, out);
            mentions(b, out);
            mentions(c, out);
        }
        Term::Tuple(xs) | Term::Array(xs, _) => {
            for x in xs {
                mentions(x, out);
            }
        }
        Term::Ctor(_, _, xs) | Term::Prim(_, xs, _) => {
            for x in xs {
                mentions(x, out);
            }
        }
        Term::Proj(t, _) | Term::Sel(t, _, _) => mentions(t, out),
        Term::Extend(t, _, u) => {
            mentions(t, out);
            mentions(u, out);
        }
        Term::Record(fs) => {
            for (_, t) in fs {
                mentions(t, out);
            }
        }
        Term::Perform(_, _, a, _) => mentions(a, out),
        Term::Case(s, arms, _) => {
            mentions(s, out);
            for (p, g, t) in arms {
                let mut vs = Vec::new();
                core::pat_vars(p, &mut vs);
                out.extend(vs);
                if let Some(g) = g {
                    mentions(g, out);
                }
                mentions(t, out);
            }
        }
        Term::Handle {
            body, clauses, ret, ..
        } => {
            mentions(body, out);
            for c in clauses {
                out.insert(c.param);
                out.insert(c.resume);
                mentions(&c.body, out);
            }
            if let Some((v, _, t)) = ret {
                out.insert(*v);
                mentions(t, out);
            }
        }
    }
}
/// The parameters a term binds as nested lambdas, and what is left underneath.
///
/// `fun f a b = e` is `Lam(a, Lam(b, e))`, so this is how a definition's arity is
/// recovered — which is what lets a saturated call to it become a jump.
/// Whether `p` matches every value of its type: binding it can bind names and
/// take a tuple or a record apart, but never fail.
fn irrefutable(p: &Pat) -> bool {
    match p {
        Pat::Wild | Pat::Var(..) => true,
        Pat::As(_, _, sub) => irrefutable(sub),
        Pat::Tuple(subs) => subs.iter().all(irrefutable),
        Pat::Record(fields) => fields.iter().all(|(_, p)| irrefutable(p)),
        Pat::Lit(_) | Pat::Array(_) | Pat::Ctor(..) => false,
    }
}

fn lam_spine(t: &Term) -> (Vec<Var>, &Term) {
    let mut params = Vec::new();
    let mut cur = t;
    // Through a polymorphic definition's type abstraction, which takes no
    // argument at run time.
    while let Term::TyLam(_, body) = cur {
        cur = body;
    }
    while let Term::Lam(p, _, body) = cur {
        params.push(*p);
        cur = body;
    }
    (params, cur)
}

/// A call, flattened: `f a b c` is `App(App(App(f, a), b), c)`.
fn call_spine(t: &Term) -> (&Term, Vec<&Term>) {
    let mut args = Vec::new();
    // Through positions: a debug build marks calls, and a call it marked
    // should still be recognised as the known call it is.
    let mut cur = t.peel();
    while let Term::App(f, a) = cur {
        args.push(&**a);
        cur = f.peel();
    }
    // Through an instantiation, which passes nothing at run time.
    while let Term::TyApp(f, _) = cur {
        cur = f.peel();
    }
    args.reverse();
    (cur, args)
}

// --- decision trees --------------------------------------------------------

/// A row of a `match`'s pattern matrix: what is still to be tested of one arm
/// -- each test a value and a pattern it must match -- and what its variables
/// have been bound to so far.
#[derive(Clone)]
struct Row<'t> {
    tests: Vec<(Name, &'t Pat)>,
    binds: Vec<(Var, Name)>,
    arm: usize,
}

/// A decision tree over a `match`'s arms, built whole before anything is
/// emitted so that an arm reached from more than one leaf is known to be.
enum Dt<'t> {
    Fail,
    /// Arm `arm` matched, its variables bound by `binds`. `otherwise` is what
    /// is left to try when its guard is false.
    Leaf {
        arm: usize,
        binds: Vec<(Var, Name)>,
        otherwise: Option<Box<Dt<'t>>>,
    },
    /// Switch on `occ`, a case per constructor some row names there, each
    /// binding its fields; `default` for the others, unless there are none.
    Switch {
        occ: Name,
        cases: Vec<(InternedString, Vec<Name>, Dt<'t>)>,
        default: Option<Box<Dt<'t>>>,
    },
    Lits {
        occ: Name,
        cases: Vec<(&'t core::Lit, Dt<'t>)>,
        default: Box<Dt<'t>>,
    },
    /// Take the fields of tuple `occ` that some row looks at.
    Tuple {
        occ: Name,
        fields: Vec<(usize, Name)>,
        next: Box<Dt<'t>>,
    },
}

/// An arm reached from more than one leaf and too big to copy to each: a
/// label its leaves jump to, as a join point is.
struct SharedArm {
    label: Label,
    fvs: Vec<Name>,
    vars: Vec<Var>,
    ev: bool,
}

/// The most nodes an arm's body may have and still be copied to every leaf
/// that reaches it. Copying is what keeps the arm in the same function as the
/// switches above it -- and so able to build in the blocks they took apart.
const ARM_COPY_BUDGET: usize = 48;

/// Whether the decision tree can take `p` apart.
fn tree_pat(p: &Pat) -> bool {
    match p {
        Pat::Wild | Pat::Var(..) | Pat::Lit(_) => true,
        Pat::As(_, _, sub) => tree_pat(sub),
        Pat::Ctor(_, subs) | Pat::Tuple(subs) => subs.iter().all(tree_pat),
        Pat::Array(_) | Pat::Record(_) => false,
    }
}

/// `row` with its variables bound and its wildcards gone: what is left are
/// tests that can fail, or take a tuple apart.
fn settle(mut row: Row<'_>) -> Row<'_> {
    let mut out = Vec::with_capacity(row.tests.len());
    let mut stack: Vec<(Name, &Pat)> = row.tests.drain(..).rev().collect();
    while let Some((x, p)) = stack.pop() {
        match p {
            Pat::Wild => {}
            Pat::Var(v, _) => row.binds.push((*v, x)),
            Pat::As(v, _, sub) => {
                row.binds.push((*v, x));
                stack.push((x, sub));
            }
            _ => out.push((x, p)),
        }
    }
    row.tests = out;
    row
}

/// `row` where `occ` is known to be built by `ctor`, whose fields are
/// `fields`: its test of `occ` becomes tests of the fields, in its place, and
/// a row that wanted another constructor there is gone.
fn specialize<'t>(
    row: &Row<'t>,
    occ: Name,
    ctor: InternedString,
    fields: &[Name],
) -> Option<Row<'t>> {
    let Some(j) = row.tests.iter().position(|(x, _)| *x == occ) else {
        return Some(row.clone());
    };
    match row.tests[j].1 {
        Pat::Ctor(c, subs) if *c == ctor => {
            let mut row = row.clone();
            row.tests.remove(j);
            for (i, sub) in subs.iter().enumerate().rev() {
                if !matches!(sub, Pat::Wild) {
                    row.tests.insert(j, (fields[i], sub));
                }
            }
            Some(row)
        }
        _ => None,
    }
}

fn count_leaves(dt: &Dt<'_>, out: &mut HashMap<usize, usize>) {
    match dt {
        Dt::Fail => {}
        Dt::Leaf { arm, otherwise, .. } => {
            *out.entry(*arm).or_default() += 1;
            if let Some(o) = otherwise {
                count_leaves(o, out);
            }
        }
        Dt::Switch { cases, default, .. } => {
            for (_, _, c) in cases {
                count_leaves(c, out);
            }
            if let Some(d) = default {
                count_leaves(d, out);
            }
        }
        Dt::Lits { cases, default, .. } => {
            for (_, c) in cases {
                count_leaves(c, out);
            }
            count_leaves(default, out);
        }
        Dt::Tuple { next, .. } => count_leaves(next, out),
    }
}

impl Lower {
    /// A `match` as a decision tree: every value tested once, and no failure
    /// objects -- a tree never backtracks, so nothing has to be kept to
    /// backtrack with. That is also what lets a `switch` consume what it
    /// takes apart, and so hand its block to what the arm builds (see
    /// `meadow_llvm::linear`): the chain's failure objects captured the
    /// scrutinee, which was then never the last reference.
    ///
    /// Arms are tried in order, left to right within a pattern: a row whose
    /// pattern is fully matched is the answer, and its guard failing goes on
    /// to the rows after it. An arm reached from several leaves is copied to
    /// each while it is small, and is a label they jump to when it is not.
    ///
    /// `None` when some pattern is not one the tree takes apart -- an array,
    /// a record -- or the tree would be too big; the chain handles those.
    fn case_matrix(&mut self, s: Name, arms: &[Arm], live: &[Name], k: Name) -> Option<Statement> {
        if !arms.iter().all(|(p, _, _)| tree_pat(p)) {
            return None;
        }
        let rows: Vec<Row<'_>> = arms
            .iter()
            .enumerate()
            .map(|(i, (p, _, _))| Row {
                tests: vec![(s, p)],
                binds: Vec::new(),
                arm: i,
            })
            .collect();
        let dt = self.plan(rows, arms)?;
        Some(self.emit_planned(dt, arms, live, k))
    }

    /// `match (e1, …, en) with …` where every arm takes the tuple apart: each
    /// `ei` is bound to a name of its own and the tree tests those, so the
    /// tuple -- built only to be taken apart -- is never built. Building it
    /// was more than an allocation: a field taken out of a tuple still alive
    /// is shared with it, and so is never the last reference -- Okasaki's
    /// `balance`, which matches on `(colour, left, right)`, could reuse
    /// nothing it took apart.
    fn case_of_tuple(
        &mut self,
        items: &[Term],
        arms: &[Arm],
        env: &[Name],
        want: &HashSet<Var>,
        k: Name,
    ) -> Option<Statement> {
        let n = items.len();
        let takes_apart = |p: &Pat| match p {
            Pat::Wild => true,
            Pat::Tuple(ps) => ps.len() == n && tree_pat(p),
            _ => false,
        };
        if !arms.iter().all(|(p, _, _)| takes_apart(p)) {
            return None;
        }
        // Named before they are bound, so that the tree can be planned --
        // and given up on -- before anything is lowered.
        let names: Vec<Name> = items
            .iter()
            .map(|e| {
                let ty = self.type_of(e);
                self.fresh_typed(ty)
            })
            .collect();
        let rows: Vec<Row<'_>> = arms
            .iter()
            .enumerate()
            .map(|(i, (p, _, _))| Row {
                tests: match p {
                    Pat::Tuple(ps) => names
                        .iter()
                        .copied()
                        .zip(ps)
                        .filter(|(_, p)| !matches!(p, Pat::Wild))
                        .collect(),
                    _ => Vec::new(),
                },
                binds: Vec::new(),
                arm: i,
            })
            .collect();
        let dt = self.plan(rows, arms)?;
        Some(self.bind_all(
            items,
            env,
            want,
            Box::new(move |this, got, env1| {
                let binds: Vec<(Var, Name)> = names.iter().copied().zip(got).collect();
                this.bind_vars(
                    &binds,
                    env1,
                    Box::new(move |this, env2| this.emit_planned(dt, arms, &env2, k)),
                )
            }),
        ))
    }

    /// The tree for `rows`, unless it would be too big to be worth it.
    fn plan<'t>(&mut self, rows: Vec<Row<'t>>, arms: &'t [Arm]) -> Option<Dt<'t>> {
        let mut budget = 256 + 32 * arms.len();
        self.build_tree(rows, arms, &mut budget)
    }

    /// Emit a planned tree in `live`: the arms it reaches from more than one
    /// leaf, and cannot copy to each, made labels first.
    fn emit_planned<'t>(
        &mut self,
        dt: Dt<'t>,
        arms: &'t [Arm],
        live: &[Name],
        k: Name,
    ) -> Statement {
        let mut counts = HashMap::new();
        count_leaves(&dt, &mut counts);
        let mut shared = HashMap::new();
        for (i, arm) in arms.iter().enumerate() {
            if counts.get(&i).copied().unwrap_or(0) > 1
                && core::inline::size(&arm.2, ARM_COPY_BUDGET) > ARM_COPY_BUDGET
            {
                let sa = self.shared_arm(arm, live);
                shared.insert(i, sa);
            }
        }
        self.emit_tree(dt, arms, live.to_vec(), k, &shared)
    }

    fn build_tree<'t>(
        &mut self,
        rows: Vec<Row<'t>>,
        arms: &'t [Arm],
        budget: &mut usize,
    ) -> Option<Dt<'t>> {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let mut rows: Vec<Row<'t>> = rows.into_iter().map(settle).collect();
        let Some(first) = rows.first() else {
            return Some(Dt::Fail);
        };
        let Some(&(occ, pat)) = first.tests.first() else {
            let row = rows.remove(0);
            let otherwise = match arms[row.arm].1 {
                Some(_) => Some(Box::new(self.build_tree(rows, arms, budget)?)),
                None => None,
            };
            return Some(Dt::Leaf {
                arm: row.arm,
                binds: row.binds,
                otherwise,
            });
        };
        let at = |r: &Row<'t>| r.tests.iter().find(|(x, _)| *x == occ).map(|(_, p)| *p);
        let untested = |rows: &[Row<'t>]| -> Vec<Row<'t>> {
            rows.iter().filter(|r| at(r).is_none()).cloned().collect()
        };
        match pat {
            Pat::Ctor(..) => {
                let mut ctors: Vec<(InternedString, &'t [Pat])> = Vec::new();
                for r in &rows {
                    if let Some(Pat::Ctor(c, subs)) = at(r)
                        && !ctors.iter().any(|(d, _)| d == c)
                    {
                        ctors.push((*c, subs));
                    }
                }
                let of = self.type_of_name(occ);
                let mut cases = Vec::with_capacity(ctors.len());
                for (c, subs) in &ctors {
                    let fields = self.field_names(*c, subs, of.as_ref());
                    let spec: Vec<Row<'t>> = rows
                        .iter()
                        .filter_map(|r| specialize(r, occ, *c, &fields))
                        .collect();
                    let sub = self.build_tree(spec, arms, budget)?;
                    cases.push((*c, fields, sub));
                }
                let covered = match &of {
                    Some(core::Ty::Con(name, _)) => self.variants.get(name).map(|vs| vs.len()),
                    _ => None,
                };
                let default = if covered == Some(ctors.len()) {
                    None
                } else {
                    Some(Box::new(self.build_tree(untested(&rows), arms, budget)?))
                };
                Some(Dt::Switch {
                    occ,
                    cases,
                    default,
                })
            }
            Pat::Lit(_) => {
                let mut lits: Vec<&'t core::Lit> = Vec::new();
                for r in &rows {
                    if let Some(Pat::Lit(l)) = at(r)
                        && !lits.contains(&l)
                    {
                        lits.push(l);
                    }
                }
                let mut cases = Vec::with_capacity(lits.len());
                for l in lits {
                    let spec: Vec<Row<'t>> = rows
                        .iter()
                        .filter_map(|r| match r.tests.iter().position(|(x, _)| *x == occ) {
                            None => Some(r.clone()),
                            Some(j) => match r.tests[j].1 {
                                Pat::Lit(m) if m == l => {
                                    let mut r = r.clone();
                                    r.tests.remove(j);
                                    Some(r)
                                }
                                _ => None,
                            },
                        })
                        .collect();
                    cases.push((l, self.build_tree(spec, arms, budget)?));
                }
                let default = Box::new(self.build_tree(untested(&rows), arms, budget)?);
                Some(Dt::Lits {
                    occ,
                    cases,
                    default,
                })
            }
            Pat::Tuple(subs) => {
                let of = self.type_of_name(occ);
                let mut names: Vec<Option<Name>> = vec![None; subs.len()];
                for r in &rows {
                    if let Some(Pat::Tuple(subs)) = at(r) {
                        for (i, sub) in subs.iter().enumerate() {
                            if names[i].is_none() && !matches!(sub, Pat::Wild) {
                                let ty = match &of {
                                    Some(core::Ty::Tuple(items)) => items.get(i).cloned(),
                                    _ => None,
                                };
                                // What the pattern says, where the tuple's
                                // type does not: a literal's own, as for a
                                // constructor's fields.
                                let ty = field_type(ty, sub).or_else(|| self.pattern_type(sub));
                                names[i] = Some(self.fresh_typed(ty));
                            }
                        }
                    }
                }
                let spec: Vec<Row<'t>> = rows
                    .into_iter()
                    .map(|mut r| {
                        if let Some(j) = r.tests.iter().position(|(x, _)| *x == occ)
                            && let (_, Pat::Tuple(subs)) = r.tests.remove(j)
                        {
                            for (i, sub) in subs.iter().enumerate().rev() {
                                if let Some(n) = names[i]
                                    && !matches!(sub, Pat::Wild)
                                {
                                    r.tests.insert(j, (n, sub));
                                }
                            }
                        }
                        r
                    })
                    .collect();
                let fields = names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| n.map(|n| (i, n)))
                    .collect();
                Some(Dt::Tuple {
                    occ,
                    fields,
                    next: Box::new(self.build_tree(spec, arms, budget)?),
                })
            }
            _ => unreachable!("a settled row tests only constructors, literals and tuples"),
        }
    }

    /// Arm `arm`'s body as a label of its own, taking what it needs of `live`
    /// and its pattern's variables.
    fn shared_arm(&mut self, arm: &Arm, live: &[Name]) -> SharedArm {
        let (pat, _, body) = arm;
        let mut vars = Vec::new();
        core::pat_vars(pat, &mut vars);
        let mut want = self.wants(&[body], &vars);
        let ev = want.remove(&self.ev);
        let fvs = restrict(live, &want);
        let label = self.fresh_label();
        let kk = self.function_return();
        let mut block = fvs.clone();
        block.extend(vars.iter().copied());
        block.push(kk);
        let outer = self.ev;
        if ev {
            let iev = self.fresh_ref();
            block.push(iev);
            self.ev = iev;
        }
        let made = self.expr(body, &block, kk);
        self.ev = outer;
        self.defs.push(Def {
            label,
            name: InternedString::from("<arm>"),
            module: self.module,
            block: Block {
                params: block,
                body: made,
            },
        });
        SharedArm {
            label,
            fvs,
            vars,
            ev,
        }
    }

    fn emit_tree<'t>(
        &mut self,
        dt: Dt<'t>,
        arms: &'t [Arm],
        env: Vec<Name>,
        k: Name,
        shared: &'t HashMap<usize, SharedArm>,
    ) -> Statement {
        match dt {
            Dt::Fail => Statement::Error("non-exhaustive pattern match"),
            Dt::Leaf {
                arm,
                binds,
                otherwise,
            } => self.bind_vars(
                &binds,
                env,
                Box::new(move |this, env1| {
                    let run: Success<'t> = Box::new(move |this, env2| match shared.get(&arm) {
                        None => this.expr(&arms[arm].2, &env2, k),
                        Some(sa) => {
                            let mut sel = sa.fvs.clone();
                            sel.extend(sa.vars.iter().copied());
                            sel.push(k);
                            if sa.ev {
                                sel.push(this.ev);
                            }
                            Statement::Substitute(
                                sel.clone(),
                                Box::new(Block {
                                    params: sel,
                                    body: Statement::Jump(sa.label),
                                }),
                            )
                        }
                    });
                    match (&arms[arm].1, otherwise) {
                        (Some(guard), Some(otherwise)) => this.bind(
                            guard,
                            &env1,
                            &env1,
                            None,
                            Box::new(move |this, cv, env2| {
                                let no = this.emit_tree(*otherwise, arms, env2.clone(), k, shared);
                                let yes = run(this, env2.clone());
                                Statement::Extern {
                                    op: Extern::Branch,
                                    args: vec![cv],
                                    blocks: vec![
                                        Block {
                                            params: env2.clone(),
                                            body: no,
                                        },
                                        Block {
                                            params: env2,
                                            body: yes,
                                        },
                                    ],
                                }
                            }),
                        ),
                        _ => run(this, env1),
                    }
                }),
            ),
            Dt::Switch {
                occ,
                cases,
                default,
            } => {
                let mut switch_arms = Vec::with_capacity(cases.len());
                for (ctor, fields, sub) in cases {
                    let tag = self.tag_of(ctor);
                    let mut arm_env = fields;
                    arm_env.extend_from_slice(&env);
                    let body = self.emit_tree(sub, arms, arm_env.clone(), k, shared);
                    switch_arms.push((
                        tag,
                        Block {
                            params: arm_env,
                            body,
                        },
                    ));
                }
                let otherwise = match default {
                    Some(d) => self.emit_tree(*d, arms, env.clone(), k, shared),
                    None => Statement::Error("non-exhaustive pattern match"),
                };
                Statement::Switch {
                    scrutinee: occ,
                    arms: switch_arms,
                    default: Box::new(Block {
                        params: env,
                        body: otherwise,
                    }),
                }
            }
            Dt::Lits {
                occ,
                cases,
                default,
            } => {
                let mut stmt = self.emit_tree(*default, arms, env.clone(), k, shared);
                for (lit, sub) in cases.into_iter().rev() {
                    let yes = self.emit_tree(sub, arms, env.clone(), k, shared);
                    stmt = Statement::Extern {
                        op: Extern::BranchPrimK(core::Prim::Eq, lit.clone()),
                        args: vec![occ],
                        blocks: vec![
                            Block {
                                params: env.clone(),
                                body: stmt,
                            },
                            Block {
                                params: env.clone(),
                                body: yes,
                            },
                        ],
                    };
                }
                stmt
            }
            Dt::Tuple { occ, fields, next } => {
                self.take_fields(occ, &fields, env, *next, arms, k, shared)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn take_fields<'t>(
        &mut self,
        occ: Name,
        fields: &[(usize, Name)],
        env: Vec<Name>,
        next: Dt<'t>,
        arms: &'t [Arm],
        k: Name,
        shared: &'t HashMap<usize, SharedArm>,
    ) -> Statement {
        match fields.split_first() {
            None => self.emit_tree(next, arms, env, k, shared),
            Some(((i, name), rest)) => {
                let rest = rest.to_vec();
                self.produces_as(
                    Extern::Field(*i),
                    vec![occ],
                    &env,
                    None,
                    Some(*name),
                    move |this, _x, env1| this.take_fields(occ, &rest, env1, next, arms, k, shared),
                )
            }
        }
    }

    /// Give each variable of `binds` the value it names, then `then`.
    fn bind_vars<'a>(
        &mut self,
        binds: &[(Var, Name)],
        env: Vec<Name>,
        then: Success<'a>,
    ) -> Statement {
        match binds.split_first() {
            None => then(self, env),
            Some(((v, x), rest)) => {
                let rest = rest.to_vec();
                self.rebind(
                    *x,
                    *v,
                    env,
                    Box::new(move |this, env1| this.bind_vars(&rest, env1, then)),
                )
            }
        }
    }
}

/// The representation of a value of type `ty`.
fn rep_of(ty: &meadow_core::Ty) -> Rep {
    use meadow_core::desc;
    match ty {
        meadow_core::Ty::Var(v) => Rep::Var(*v),
        ty => match desc::of(ty) {
            Some(desc::REF) => Rep::Ref,
            Some(desc::INT) => Rep::Int,
            Some(desc::FLOAT) => Rep::Float,
            Some(desc::STR) => Rep::Str,
            Some(desc::ANY) | None => Rep::Unknown,
            Some(d) => Rep::Bits(d),
        },
    }
}
