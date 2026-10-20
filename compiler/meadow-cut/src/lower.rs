//! Cut lowered to AxCut: the upper IR to the lower, which the runtimes
//! compile.
//!
//! Cut names its values and says where each goes. AxCut has no names to
//! speak of: an environment, in order, which every block's parameters list
//! whole, and seven statements that are each a cut with the rule it meets.
//! So lowering is deciding what is in the environment at each point, and
//! writing each cut as the statement it is:
//!
//! ```text
//!   <K(x, ..) | μ̃ y. s>          let y = K#t(x, ..); s
//!   <cocase { .. } | μ̃ f. s>     new f [what it mentions] { .. }; s
//!   <x | case { K(..) => s }>     switch x { #t (fields, env) => s }
//!   <f | m(x, ..; k, ..)>         substitute [f, x, .., k, ..]; invoke f#m
//!   <x | k>                       substitute [k, x]; invoke k#0
//!   f(x, ..; k, ..)               substitute [x, .., k, ..]; jump f
//!   prim op(x, ..; c)             extern op(x, ..) -> (r, env); r to c
//! ```
//!
//! A consumer that is not a name already -- a `μ̃`, a `case` -- and has to be
//! one, to be passed, is made an object of one method that takes the value:
//! what it mentions from around it, captured. An object captures only what
//! its methods mention, in the order the environment has them.
//!
//! A top-level value is a global of the machine's, by its place among the
//! program's: the block a program starts at computes each in turn and then
//! calls the entry.
//!
//! # Effects: evidence passing
//!
//! AxCut has no handlers. Every definition and every method takes one
//! argument more than it is written with: the handlers in scope where it
//! was called, a list of `#ev(key, clause, target, rest)` entries, newest
//! first, that ends in `#evnone`. A consumer made an object captures it.
//!
//! `handle` keeps where its value goes in a `Ref`, the `target`; makes an
//! object of each clause, and an entry for it on the evidence; and runs its
//! body under that, answering a continuation that reads the target and runs
//! the `return` clause there. `perform` looks its operation up -- a jump to
//! a block of the program's that walks the list -- and enters the clause
//! found with its arguments, a resumption, and what the target holds. The
//! resumption is a function: called with a value and a continuation, it
//! makes that continuation the target and goes on from the `perform`, which
//! is what makes a handler deep. An operation no entry answers is the
//! runtime's, if the `native` table binds it, and an error if not.
//!
//! In a program where an operation is marked `@many`, every continuation
//! is an object on the heap and a resumption is not checked to be called
//! once: one called again goes on from the `perform` again, which is what
//! such an operation wants.
//!
//! In any other program -- every one of Meadow's -- the continuation a
//! call is made with to answer through is a **frame** (`Program::frames`),
//! which the back ends keep on a stack: the native one, on Silo. So a
//! handled body runs on a segment of the stack of its own (`Enter`), an
//! operation whose clause is given its continuation cuts the segments
//! between it and its handler off (`Detach`), and the resumption, which is
//! checked to be called once, puts them back (`Reattach`): `meadow_seq`'s
//! lowering of the same, `docs/SILO.md`'s "Effects: stack segments".
//! `MEADOW_CUT_FRAMES=none` keeps everything on the heap, for telling a
//! fault in this from one anywhere else.
//!
//! # Representations: descriptors
//!
//! A definition generic in a representation takes, for each variable, a
//! descriptor: a number that says which representation the variable is
//! where it was called. A name represented as a variable says so by its
//! descriptor's name, and wherever such a name is, its descriptor is too:
//! `meadow_axcut::describe` adds it to each environment that lacks it, as
//! it does for this compiler's own lowering.
//!
//! **Not lowered**: a `μ` where a value is wanted, anywhere but as the
//! argument of a definition whose parameter says how it is represented. A
//! program with one is refused, saying so.

use std::collections::{HashMap, HashSet};

use meadow_axcut as ax;
use meadow_axcut::{Block, Extern, Label, Name, Statement as S, Tag, VarId};
use meadow_intern::InternedString;
use meadow_rt::{Lit, Prim, desc};

use crate::{
    Answer, Arm, Binder, Clause, Consumer, Def, Handle, Method, Pattern, Producer, Program, Rep,
    Statement, Symbol,
};

type R<T> = Result<T, String>;

/// The name the evidence -- the handlers in scope -- is in scope by. No
/// variable of Cut's is written so.
const EV: &str = "#ev";

/// `p` as a program of AxCut, or why it is not one yet. What its entry
/// answers is what the program answers: a machine that runs it has it back.
pub fn lower(p: &Program) -> R<ax::Program> {
    lowered(p, false).map(|l| l.program)
}

/// The same, as a program to run for what it does: one whose answer is a
/// string writes it to standard output, as `answer str` says, and answers
/// `unit`.
pub fn executable(p: &Program) -> R<ax::Program> {
    lowered(p, true).map(|l| l.program)
}

/// [`lower`], with what each part of the AxCut came from.
pub fn lower_mapped(p: &Program) -> R<Lowered> {
    lowered(p, false)
}

/// [`executable`], with what each part of the AxCut came from.
pub fn executable_mapped(p: &Program) -> R<Lowered> {
    lowered(p, true)
}

/// A program lowered, and where each part of it came from.
#[derive(Debug, Default)]
pub struct Lowered {
    pub program: ax::Program,
    pub map: Map,
}

/// What the AxCut of a program came from in its Cut: which declaration each
/// block is, and which declaration and variable each name.
#[derive(Debug, Default)]
pub struct Map {
    /// The definition each block is. The block a program starts at, which
    /// computes its top-level values, is no definition's.
    pub blocks: HashMap<Label, Symbol>,
    pub names: HashMap<Name, Origin>,
}

/// Where a name of the AxCut came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Origin {
    /// The definition or top-level value it was made lowering, if any.
    pub of: Option<Symbol>,
    /// The variables of Cut's it is: none for a name the lowering made of
    /// its own -- a literal's, an object's, a consumer's made one -- and
    /// more than one where a `μ̃` or a `let` names again what has a name.
    pub vars: Vec<String>,
}

impl Lowered {
    /// The AxCut as text, with what each part of it is in the Cut it came
    /// from: each definition's block, and each name that is a variable of
    /// Cut's, as [`crate::print::Listing`] says a program of Cut's own.
    pub fn listing(&self) -> crate::print::Listing {
        use crate::print::{Part, Segment};
        let listed = self.program.listing();
        let mut segments = Vec::new();
        for (start, end, label) in &listed.defs {
            if let Some(symbol) = self.map.blocks.get(label) {
                segments.push(Segment {
                    start: *start,
                    end: *end,
                    part: Part::Decl(symbol.clone()),
                });
            }
        }
        for (start, end, name) in &listed.names {
            let Some(Origin { of: Some(of), vars }) = self.map.names.get(name) else {
                continue;
            };
            for var in vars {
                segments.push(Segment {
                    start: *start,
                    end: *end,
                    part: Part::Var {
                        of: of.clone(),
                        name: var.clone(),
                    },
                });
            }
        }
        segments.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
        crate::print::Listing {
            text: listed.text,
            segments,
        }
    }
}

/// What a program to run is lowered from: `p` copied for the
/// representations its generic definitions are called at -- see
/// [`crate::specialize`] -- and its small definitions inlined -- see
/// [`crate::inline`]. `MEADOW_CUT_GENERIC=1` leaves them generic, and
/// `MEADOW_CUT_INLINE=0` leaves the calls, to measure each by.
pub fn optimized(p: &Program) -> Program {
    let copied = if std::env::var_os("MEADOW_CUT_GENERIC").is_none() {
        crate::specialize::program(p)
    } else {
        p.clone()
    };
    if std::env::var("MEADOW_CUT_INLINE").as_deref() == Ok("0") {
        copied
    } else {
        crate::inline::program(&copied)
    }
}

fn lowered(p: &Program, prints: bool) -> R<Lowered> {
    let copied;
    let p = if prints {
        copied = optimized(p);
        &copied
    } else {
        p
    };
    let mut lazy = lazily(p);
    started(p, &mut lazy);
    let whole = p;
    let p = &lazy;
    let mut l = Lower {
        map: Map::default(),
        current: None,
        handles: false,
        search: Label(0),
        rep_vars: HashMap::new(),
        descs: HashSet::new(),
        params: HashMap::new(),
        expected: None,
        prints,
        natives: HashMap::new(),
        answers: HashMap::new(),
        out: ax::Program::default(),
        next: 0,
        next_tag: 0,
        defs: HashMap::new(),
        vals: HashMap::new(),
        literals: HashMap::new(),
        frames: if p.effects.iter().any(|e| e.ops.iter().any(|o| o.many)) {
            None
        } else {
            frame_kinds()
        },
        methods: Vec::new(),
    };
    l.declare(p)?;
    for (i, v) in whole.vals.iter().enumerate() {
        l.vals.insert(v.symbol.clone(), (i as i64, l.rep(&v.rep)?));
        if let Statement::Cut(x, Consumer::Halt) = &v.body
            && matches!(
                x,
                Producer::Int(_)
                    | Producer::Float(_)
                    | Producer::Char(_)
                    | Producer::Bool(_)
                    | Producer::Unit
                    | Producer::Str(_)
            )
        {
            l.literals.insert(v.symbol.clone(), x.clone());
        }
    }
    // `MEADOW_CUT_IMPURE=1` says of no definition that it is pure, to
    // measure what saying so is worth.
    if std::env::var_os("MEADOW_CUT_IMPURE").is_none() {
        for i in crate::pure::defs(p) {
            l.out.pure.insert(Label(i as u32));
        }
    }
    for (i, d) in p.defs.iter().enumerate() {
        // What makes a value is the value's, to whoever asks what a block
        // or a name came from.
        let of = forced(&d.symbol);
        l.current = Some(of.clone());
        l.map.blocks.insert(Label(i as u32), of);
        let mut sc = Scope::default();
        // Each parameter's name first: one may be represented as a variable
        // whose descriptor is a parameter after it.
        let names: Vec<Name> = d.params.iter().map(|_| l.fresh(ax::Rep::Ref)).collect();
        l.rep_vars.clear();
        for (var, descriptor) in &d.rep_vars {
            let Some(at) = d.params.iter().position(|b| b.name == *descriptor) else {
                return Err(format!(
                    "`{}`: `'{var}` is described by `{descriptor}`, which is not a parameter",
                    d.symbol
                ));
            };
            l.rep_vars.insert(var.clone(), names[at]);
            l.descs.insert(names[at]);
        }
        for (b, n) in d.params.iter().zip(names) {
            let rep = l.rep(&b.rep)?;
            l.out.reps.insert(n, rep);
            sc.bind(&b.name, n);
            l.called(n, &b.name);
        }
        for k in &d.conts {
            let n = l.fresh(ax::Rep::Ref);
            sc.bind(k, n);
            l.called(n, k);
            l.out.returns.insert(n);
        }
        let ev = l.fresh(ax::Rep::Ref);
        sc.bind(EV, ev);
        let params = sc.env.clone();
        let body = l.statement(&d.body, &sc)?;
        l.out.defs.push(ax::Def {
            label: Label(i as u32),
            name: InternedString::from(d.symbol.to_string().as_str()),
            module: InternedString::from(""),
            block: Block { params, body },
        });
    }
    l.current = None;
    l.rep_vars.clear();
    l.start(p)?;
    l.exports(p)?;
    if l.handles {
        l.search_block();
    }
    // Every value represented as a variable has its descriptor wherever it
    // is: each environment that lacks one is given it.
    let unmet = ax::describe::close(&mut l.out.defs, &l.out.reps, &l.out.threads, &l.descs);
    if !unmet.is_empty() {
        return Err(
            "a value is represented as a variable that nothing in scope describes".to_string(),
        );
    }
    Ok(Lowered {
        program: l.out,
        map: l.map,
    })
}

struct Lower {
    /// Whether the program handles an effect anywhere: one that does not
    /// has no evidence to search, and performs straight to the runtime.
    handles: bool,
    /// The block that finds an operation's entry in the evidence.
    search: Label,
    /// Each definition's parameters' representations, as written.
    params: HashMap<Symbol, Vec<Rep>>,
    /// How the `μ` about to be lowered as an argument is represented.
    expected: Option<ax::Rep>,
    /// The representation variables of the definition being lowered, each
    /// with the name its descriptor is in scope by.
    rep_vars: HashMap<String, Name>,
    /// Every name that holds a descriptor.
    descs: HashSet<Name>,
    /// What each block and name made so far came from.
    map: Map,
    /// The declaration being lowered, which the names made now are of.
    current: Option<Symbol>,
    out: ax::Program,
    next: u32,
    next_tag: Tag,
    /// Each definition's label, and how many values and continuations it
    /// takes.
    defs: HashMap<Symbol, (Label, usize)>,
    /// Each top-level value's place among the machine's globals, and how it
    /// is represented.
    vals: HashMap<Symbol, (i64, ax::Rep)>,
    /// The values that are a literal written out: a mention of one is the
    /// literal, which costs nothing to write again -- what
    /// `meadow_core::globals::inline_literals` does to a program of core.
    literals: HashMap<Symbol, Producer>,
    /// The kinds of continuation that are frames -- none named is every
    /// kind -- or `None` where none is: a program with an operation that may
    /// be resumed more than once keeps every continuation on the heap, where
    /// resuming twice is invoking an object twice (`docs/CUT.md`).
    frames: Option<Vec<String>>,
    /// Every method any object of the program has, by name: a method's tag
    /// is its place here, so that a call need not know which object it has.
    methods: Vec<String>,
    /// Whether an answer that is a string is written out.
    prints: bool,
    /// Each operation the runtime performs, and the runtime's name for it:
    /// its effect and its operation.
    natives: HashMap<Symbol, (InternedString, InternedString)>,
    /// How what each declared operation is resumed with is represented.
    answers: HashMap<Symbol, ax::Rep>,
}

/// What is in scope: the environment, in order, and what each name of Cut's
/// -- a value's or a continuation's -- is in it. `halt` is where a val's
/// value goes.
#[derive(Clone, Default)]
struct Scope {
    env: Vec<Name>,
    vars: HashMap<String, Name>,
    halt: Option<Name>,
}

impl Scope {
    fn bind(&mut self, name: &str, n: Name) {
        self.env.push(n);
        self.vars.insert(name.to_string(), n);
    }

    /// One more name, where a statement that makes a value puts it: first.
    fn push(&mut self, n: Name) {
        self.env.insert(0, n);
    }

    fn var(&self, name: &str) -> R<Name> {
        self.vars
            .get(name)
            .copied()
            .ok_or_else(|| format!("`{name}` is not bound"))
    }
}

/// A statement that binds one more name, at the front of the environment --
/// where the machine puts what a statement makes -- and goes on: what is
/// still to come is its `rest`.
enum Step {
    Let(Name, Tag, InternedString, Vec<Name>),
    New(Name, Vec<Name>, Vec<Block>),
    /// A primitive with one continuation, whose parameters are these.
    Extern(Extern, Vec<Name>, Vec<Name>),
    /// `μ k. s` where a value is wanted: `s` runs, with `k` an object of
    /// what is in scope, and what follows is that object's method, taking
    /// the value `s` gives it. The object, what it captures, the method's
    /// parameters, and `s`.
    Mu(Name, Vec<Name>, Vec<Name>, S),
}

fn wrap(steps: Vec<Step>, mut rest: S) -> S {
    for step in steps.into_iter().rev() {
        rest = match step {
            Step::Let(name, tag, ctor, fields) => S::Let {
                name,
                tag,
                ctor,
                fields,
                rest: Box::new(rest),
            },
            Step::New(name, captures, methods) => S::New {
                name,
                captures,
                methods,
                rest: Box::new(rest),
            },
            Step::Extern(op, args, params) => S::Extern {
                op,
                args,
                blocks: vec![Block { params, body: rest }],
            },
            Step::Mu(name, captures, params, body) => S::New {
                name,
                captures,
                methods: vec![Block { params, body: rest }],
                rest: Box::new(body),
            },
        };
    }
    rest
}

impl Lower {
    fn fresh(&mut self, rep: ax::Rep) -> Name {
        let n = VarId(self.next);
        self.next += 1;
        self.out.reps.insert(n, rep);
        self.map.names.insert(
            n,
            Origin {
                of: self.current.clone(),
                vars: Vec::new(),
            },
        );
        n
    }

    /// `n` is the variable `name` of Cut's, in the declaration being
    /// lowered: one more of its names, where it had one.
    fn called(&mut self, n: Name, name: &str) {
        let origin = self.map.names.entry(n).or_default();
        if !origin.vars.iter().any(|v| v == name) {
            origin.vars.push(name.to_string());
        }
    }

    fn tag_of(&mut self, ctor: &str) -> Tag {
        let ctor = InternedString::from(ctor_name(ctor).as_str());
        if let Some(t) = self.out.tags.get(&ctor) {
            return *t;
        }
        let t = self.next_tag;
        self.next_tag += 1;
        self.out.tags.insert(ctor, t);
        t
    }

    fn method(&self, name: &str) -> R<Tag> {
        let name = if name == "resume" { "apply" } else { name };
        self.methods
            .iter()
            .position(|m| m == name)
            .map(|i| i as Tag)
            .ok_or_else(|| format!("no object of the program has a method `{name}`"))
    }

    /// What the program declares: its constructors' tags, its roles, its
    /// definitions' labels, its values' places and its objects' methods.
    fn declare(&mut self, p: &Program) -> R<()> {
        for d in &p.datas {
            for (name, _) in &d.ctors {
                self.tag_of(&d.symbol.child(name).to_string());
            }
        }
        for (role, ctor) in &p.roles {
            let Some(r) = ax::Role::named(role) else {
                return Err(format!("`{role}` is not a role a runtime knows"));
            };
            self.out.roles.insert(
                r,
                InternedString::from(ctor_name(&ctor.to_string()).as_str()),
            );
        }
        for (i, d) in p.defs.iter().enumerate() {
            self.params.insert(
                d.symbol.clone(),
                d.params.iter().map(|b| b.rep.clone()).collect(),
            );
            self.defs.insert(
                d.symbol.clone(),
                (Label(i as u32), d.params.len() + d.conts.len()),
            );
        }
        for (i, v) in p.vals.iter().enumerate() {
            self.vals
                .insert(v.symbol.clone(), (i as i64, self.rep(&v.rep)?));
        }
        for (op, runtime) in &p.natives {
            let Some((effect, name)) = runtime.split_once('.') else {
                return Err(format!("`{runtime}` is not an operation of a runtime's"));
            };
            self.natives.insert(
                op.clone(),
                (InternedString::from(effect), InternedString::from(name)),
            );
        }
        for e in &p.effects {
            for op in &e.ops {
                self.answers.insert(
                    e.symbol.child(&op.name),
                    self.rep(&op.result).unwrap_or(ax::Rep::Ref),
                );
            }
        }
        let mut names = HashSet::new();
        let mut seen = Free::default();
        for d in &p.defs {
            methods_of(&d.body, &mut names);
            seen.statement(&d.body);
        }
        for v in &p.vals {
            methods_of(&v.body, &mut names);
            seen.statement(&v.body);
        }
        self.handles = seen.handles;
        self.search = Label(p.defs.len() as u32 + u32::from(p.entry.is_some()));
        if self.handles {
            // A resumption is a function.
            names.insert("apply".to_string());
        }
        // `resume` is `apply` by another name (`docs/CUT.md`, "Handlers").
        if names.remove("resume") {
            names.insert("apply".to_string());
        }
        self.methods = names.into_iter().collect();
        self.methods.sort();
        Ok(())
    }

    /// The block a program starts at: each top-level value computed, in the
    /// order written, and kept; then the entry, answering the continuation
    /// the block was given.
    /// A block for each export, which C enters as a program's entry is
    /// entered: its arguments and the continuation it answers through. It
    /// starts with no handlers, as the program does, and is the exported
    /// definition from there.
    fn exports(&mut self, p: &Program) -> R<()> {
        for e in &p.exports {
            let Some(&(label, _)) = self.defs.get(&e.symbol) else {
                return Err(format!(
                    "`{}` exports `{}`, which is not a definition",
                    e.name, e.symbol
                ));
            };
            let Some(d) = p.defs.iter().find(|d| d.symbol == e.symbol) else {
                return Err(format!(
                    "`{}` exports `{}`, which is not here",
                    e.name, e.symbol
                ));
            };
            if !d.rep_vars.is_empty() || d.conts.len() != 1 {
                return Err(format!(
                    "`{}` exports `{}`, which is generic or takes other than one continuation",
                    e.name, e.symbol
                ));
            }
            let mut params = Vec::new();
            let mut args = Vec::new();
            for b in &d.params {
                let rep = self.rep(&b.rep)?;
                params.push(rep);
                args.push(self.fresh(rep));
            }
            let result = self.rep(&e.result)?;
            let k = self.fresh(ax::Rep::Ref);
            self.out.returns.insert(k);
            let ev = self.fresh(ax::Rep::Ref);
            let mut given = args.clone();
            given.push(k);
            let mut passed = given.clone();
            passed.push(ev);
            let none = meadow_rt::roles::evidence::NONE;
            let body = S::Let {
                name: ev,
                tag: self.tag_of(none),
                ctor: InternedString::from(none),
                fields: Vec::new(),
                rest: Box::new(S::Substitute(
                    passed.clone(),
                    Box::new(Block {
                        params: passed,
                        body: S::Jump(label),
                    }),
                )),
            };
            let entry = Label(self.out.defs.len() as u32);
            self.out.defs.push(ax::Def {
                label: entry,
                name: InternedString::from(format!("#export {}", e.name).as_str()),
                module: InternedString::from(""),
                block: Block {
                    params: given,
                    body,
                },
            });
            self.out.results.insert(entry, result);
            self.out.exports.push(ax::Export {
                name: e.name.clone(),
                label: entry,
                params,
                result,
            });
        }
        Ok(())
    }

    fn start(&mut self, p: &Program) -> R<()> {
        let Some(entry) = &p.entry else {
            return Ok(());
        };
        let Some(&(label, takes)) = self.defs.get(entry) else {
            return Err(format!("the entry `{entry}` is not a definition"));
        };
        if takes != 1 {
            return Err(format!(
                "the entry `{entry}` takes one continuation and no values"
            ));
        }
        let k = self.fresh(ax::Rep::Ref);
        self.out.returns.insert(k);
        // No handlers yet: the evidence a program starts with.
        let ev = self.fresh(ax::Rep::Ref);
        // Built from the last value back: what follows each is the method of
        // the continuation its body answers.
        let mut sc = Scope {
            env: vec![ev, k],
            ..Scope::default()
        };
        sc.vars.insert(EV.to_string(), ev);
        // What each value's turn binds: the continuation that keeps it; in
        // that, the value, its place, and the unit keeping it answers.
        struct Turn<'a> {
            body: Scope,
            keep: Name,
            before: Vec<Name>,
            with_value: Vec<Name>,
            with_place: Vec<Name>,
            with_done: Vec<Name>,
            value: Name,
            place: Name,
            s: &'a Statement,
            symbol: &'a Symbol,
        }
        let mut turns: Vec<Turn> = Vec::new();
        for v in &p.vals {
            self.current = Some(v.symbol.clone());
            let rep = self.rep(&v.rep)?;
            let keep = self.fresh(ax::Rep::Ref);
            self.out.continuations.insert(keep);
            let before = sc.env.clone();
            let mut body = sc.clone();
            body.push(keep);
            body.halt = Some(keep);
            let value = self.fresh(rep);
            let place = self.fresh(ax::Rep::Int);
            let done = self.fresh(ax::Rep::Bits(desc::UNIT));
            let mut with_value = before.clone();
            with_value.push(value);
            let mut with_place = with_value.clone();
            with_place.insert(0, place);
            let mut with_done = with_place.clone();
            with_done.insert(0, done);
            sc.env = with_done.clone();
            turns.push(Turn {
                body,
                keep,
                before,
                with_value,
                with_place,
                with_done,
                value,
                place,
                s: &v.body,
                symbol: &v.symbol,
            });
        }
        self.current = None;
        let mut rest = S::Substitute(
            vec![k, ev],
            Box::new(Block {
                params: vec![k, ev],
                body: S::Jump(label),
            }),
        );
        let printed = self.prints && p.answer == Answer::Str;
        if printed {
            // The entry answers a continuation that writes the string out
            // and answers the program's own with `unit`.
            let write = self.fresh(ax::Rep::Ref);
            self.out.continuations.insert(write);
            let text = self.fresh(ax::Rep::Ref);
            let done = self.fresh(ax::Rep::Bits(desc::UNIT));
            rest = S::New {
                name: write,
                captures: vec![k],
                methods: vec![Block {
                    params: vec![k, text],
                    body: S::Extern {
                        op: Extern::Native(
                            InternedString::from("Console"),
                            InternedString::from("writeOutput"),
                        ),
                        args: vec![text],
                        blocks: vec![Block {
                            params: vec![done, k, text],
                            body: S::Substitute(
                                vec![k, done],
                                Box::new(Block {
                                    params: vec![k, done],
                                    body: S::Invoke(k, 0),
                                }),
                            ),
                        }],
                    },
                }],
                rest: Box::new(S::Substitute(
                    vec![write, ev],
                    Box::new(Block {
                        params: vec![write, ev],
                        body: S::Jump(label),
                    }),
                )),
            };
        }
        for (i, t) in turns.into_iter().enumerate().rev() {
            let kept = S::Extern {
                op: Extern::Lit(Lit::Int(i as i64)),
                args: Vec::new(),
                blocks: vec![Block {
                    params: t.with_place,
                    body: S::Extern {
                        op: Extern::Prim(Prim::GlobalSet),
                        args: vec![t.place, t.value],
                        blocks: vec![Block {
                            params: t.with_done,
                            body: rest,
                        }],
                    },
                }],
            };
            self.current = Some(t.symbol.clone());
            let body = self.statement(t.s, &t.body)?;
            self.current = None;
            rest = S::New {
                name: t.keep,
                captures: t.before,
                methods: vec![Block {
                    params: t.with_value,
                    body: kept,
                }],
                rest: Box::new(body),
            };
        }
        let none = meadow_rt::roles::evidence::NONE;
        let rest = S::Let {
            name: ev,
            tag: self.tag_of(none),
            ctor: InternedString::from(none),
            fields: Vec::new(),
            rest: Box::new(rest),
        };
        let label = Label(self.out.defs.len() as u32);
        self.out.defs.push(ax::Def {
            label,
            name: InternedString::from("#start"),
            module: InternedString::from(""),
            block: Block {
                params: vec![k],
                body: rest,
            },
        });
        self.out.entry = Some(label);
        // A program run for what it does answers nothing to show, whether
        // its string was written out or it says it has no answer: taken for
        // a reference, the `unit` it ends with was printed as one.
        self.out.results.insert(
            label,
            if self.prints {
                ax::Rep::Bits(desc::UNIT)
            } else {
                ax::Rep::Ref
            },
        );
        Ok(())
    }

    fn statement(&mut self, s: &Statement, sc: &Scope) -> R<S> {
        let mut sc = sc.clone();
        let mut steps = Vec::new();
        let last = match s {
            // The value `body` gives to `k`: `k` is the consumer, by a name.
            Statement::Cut(Producer::Mu(k, body), c) => {
                let frame = self.framing("join") && self.always_enters(body, k, &sc);
                let kn = self.reify_as(c, &mut sc, &mut steps, frame)?;
                sc.vars.insert(k.clone(), kn);
                self.called(kn, k);
                self.statement(body, &sc)?
            }
            Statement::Cut(p, c) => {
                let v = self.atom(p, &mut sc, &mut steps)?;
                self.give(v, c, &sc)?
            }
            Statement::Let(b, p, rest) => {
                let v = self.atom(p, &mut sc, &mut steps)?;
                sc.vars.insert(b.name.clone(), v);
                self.called(v, &b.name);
                self.statement(rest, &sc)?
            }
            Statement::Call(f, args, conts) => {
                let Some(&(label, takes)) = self.defs.get(f) else {
                    return Err(format!("`{f}` is not a definition of this program"));
                };
                if takes != args.len() + conts.len() {
                    return Err(format!(
                        "`{f}` is called with the wrong number of arguments"
                    ));
                }
                let wanted = self.params.get(f).cloned().unwrap_or_default();
                let mut sel = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    // A `μ` is represented as the parameter it is given for.
                    self.expected = match (a, wanted.get(i)) {
                        (Producer::Mu(..), Some(r)) => self.rep(r).ok(),
                        _ => None,
                    };
                    sel.push(self.atom(a, &mut sc, &mut steps)?);
                }
                self.expected = None;
                for c in conts {
                    sel.push(self.reify_as(
                        c,
                        &mut sc,
                        &mut steps,
                        conts.len() == 1 && self.framing("call"),
                    )?);
                }
                sel.push(sc.var(EV)?);
                let params = self.distinct(&sel);
                S::Substitute(
                    sel,
                    Box::new(Block {
                        params,
                        body: S::Jump(label),
                    }),
                )
            }
            // A record's field read, or set: the label is a name the back
            // end is given, not a value the program computes.
            Statement::Prim(op, args, conts) if op == "select" || op == "extend" => {
                let (Some(Producer::Str(label)), [c]) = (args.get(1), &conts[..]) else {
                    return Err(format!(
                        "`prim {op}` takes a record, a label written as a string, {}and one continuation",
                        if op == "extend" { "a value, " } else { "" }
                    ));
                };
                let label = InternedString::from(label.as_str());
                let record = self.atom(&args[0], &mut sc, &mut steps)?;
                let (extern_op, names, rep) = match (op.as_str(), args.get(2)) {
                    ("extend", Some(value)) => {
                        let value = self.atom(value, &mut sc, &mut steps)?;
                        (
                            Extern::Extend(label, None),
                            vec![record, value],
                            ax::Rep::Ref,
                        )
                    }
                    ("select", None) => {
                        let rep = match c {
                            Consumer::MuTilde(b, _) => self.rep(&b.rep)?,
                            _ => ax::Rep::Ref,
                        };
                        (Extern::Select(label, None), vec![record], rep)
                    }
                    _ => return Err(format!("`prim {op}` is given the wrong number of values")),
                };
                let r = self.fresh(rep);
                let mut inner = sc.clone();
                inner.push(r);
                let body = self.give(r, c, &inner)?;
                S::Extern {
                    op: extern_op,
                    args: names,
                    blocks: vec![Block {
                        params: inner.env,
                        body,
                    }],
                }
            }
            Statement::Prim(op, args, conts) => {
                // A spawn says first how what its thread answers is
                // represented -- `prim threadSpawn(desc(i64), f; k)` -- which
                // the machine that runs the thread has to be told, and
                // nothing else here says: a task is a pointer.
                let (answers, args) = match (op.as_str(), &args[..]) {
                    ("threadSpawn", [Producer::Desc(rep), rest @ ..]) => {
                        (Some(self.rep(rep)?), rest)
                    }
                    _ => (None, &args[..]),
                };
                let mut names = Vec::new();
                for a in args {
                    names.push(self.atom(a, &mut sc, &mut steps)?);
                }
                let lowered = self.prim(op, names, conts, &sc)?;
                if let (Some(rep), S::Extern { blocks, .. }) = (answers, &lowered)
                    && let Some(task) = blocks.first().and_then(|b| b.params.first())
                {
                    self.out.threads.insert(*task, rep);
                }
                lowered
            }
            Statement::Error(msg) => S::Error(Box::leak(msg.clone().into_boxed_str())),
            Statement::Perform(op, args, c) => self.perform(op, args, c, &mut sc, &mut steps)?,
            Statement::Handle(h) => self.handle(h, &mut sc, &mut steps)?,
        };
        Ok(wrap(steps, last))
    }

    /// How a representation of Cut's is one of AxCut's: a variable by the
    /// name its descriptor has in the definition being lowered.
    fn rep(&self, r: &Rep) -> R<ax::Rep> {
        use meadow_rt::num::Width;
        Ok(match r {
            Rep::I64 => ax::Rep::Int,
            Rep::F64 => ax::Rep::Float,
            Rep::F32 => ax::Rep::Bits(desc::FLOAT32),
            Rep::I8 => ax::Rep::Bits(desc::word(Width::I8)),
            Rep::I16 => ax::Rep::Bits(desc::word(Width::I16)),
            Rep::I32 => ax::Rep::Bits(desc::word(Width::I32)),
            Rep::U8 => ax::Rep::Bits(desc::word(Width::U8)),
            Rep::U16 => ax::Rep::Bits(desc::word(Width::U16)),
            Rep::U32 => ax::Rep::Bits(desc::word(Width::U32)),
            Rep::U64 => ax::Rep::Bits(desc::word(Width::U64)),
            Rep::Bool => ax::Rep::Bits(desc::BOOL),
            Rep::Char => ax::Rep::Bits(desc::CHAR),
            Rep::Unit => ax::Rep::Bits(desc::UNIT),
            // A string is an object on the heap. AxCut's `Str` is a symbol's:
            // an interned name, whose word is its key.
            Rep::Str | Rep::Ptr | Rep::Any => ax::Rep::Ref,
            Rep::Sym => ax::Rep::Str,
            // A descriptor is a number: which representation.
            Rep::Desc => ax::Rep::Int,
            Rep::Var(a) => match self.rep_vars.get(a) {
                Some(n) => ax::Rep::Var(n.0),
                None => {
                    return Err(format!(
                        "`'{a}` is not a representation variable of the definition it is in"
                    ));
                }
            },
        })
    }

    /// `names`, each once, in the order given.
    fn once(names: &[Name]) -> Vec<Name> {
        let mut seen = HashSet::new();
        names.iter().copied().filter(|n| seen.insert(*n)).collect()
    }

    /// An object with the one method `apply`, at its place among the
    /// program's methods.
    fn function(&self, apply: Block) -> R<Vec<Block>> {
        let tag = self.method("apply")? as usize;
        // Every method's parameters begin with the object's captures, the
        // ones that can never be called as well: shaped as `apply` is.
        let mut blocks: Vec<Block> = (0..self.methods.len())
            .map(|_| Block {
                params: apply.params.clone(),
                body: S::Error("the object has no such method"),
            })
            .collect();
        blocks[tag] = apply;
        Ok(blocks)
    }

    /// The runtime performing `op`, of `args`, and giving what it answers to
    /// the continuation named `k`: with `env` in scope, which holds them.
    fn native(
        &mut self,
        effect: InternedString,
        name: InternedString,
        args: &[Name],
        k: Name,
        rep: ax::Rep,
        env: &[Name],
    ) -> S {
        let r = self.fresh(rep);
        let answered = |arg: Name, env: Vec<Name>| {
            let mut after = env;
            after.insert(0, r);
            S::Extern {
                op: Extern::Native(effect, name),
                args: vec![arg],
                blocks: vec![Block {
                    params: after,
                    body: S::Substitute(
                        vec![k, r],
                        Box::new(Block {
                            params: vec![k, r],
                            body: S::Invoke(k, 0),
                        }),
                    ),
                }],
            }
        };
        match args {
            [one] => answered(*one, env.to_vec()),
            // Nothing given is `unit`; several are the tuple the runtime's
            // operation takes.
            [] => {
                let u = self.fresh(ax::Rep::Bits(desc::UNIT));
                let mut with = env.to_vec();
                with.insert(0, u);
                S::Extern {
                    op: Extern::Lit(Lit::Unit),
                    args: Vec::new(),
                    blocks: vec![Block {
                        params: with.clone(),
                        body: answered(u, with),
                    }],
                }
            }
            several => {
                let t = self.fresh(ax::Rep::Ref);
                let mut with = env.to_vec();
                with.insert(0, t);
                S::Let {
                    name: t,
                    tag: self.tag_of(meadow_rt::roles::TUPLE),
                    ctor: InternedString::from(meadow_rt::roles::TUPLE),
                    fields: several.to_vec(),
                    rest: Box::new(answered(t, with)),
                }
            }
        }
    }

    /// `perform op(args; c)`: the clause the evidence has for `op` entered,
    /// or the runtime asked.
    fn perform(
        &mut self,
        op: &Symbol,
        args: &[Producer],
        c: &Consumer,
        sc: &mut Scope,
        steps: &mut Vec<Step>,
    ) -> R<S> {
        let native = self.natives.get(op).copied();
        let resumed = match c {
            Consumer::MuTilde(b, _) => self.rep(&b.rep)?,
            _ => self.answers.get(op).copied().unwrap_or(ax::Rep::Ref),
        };
        let mut xs = Vec::new();
        for a in args {
            xs.push(self.atom(a, sc, steps)?);
        }
        let kc = self.reify_as(c, sc, steps, self.framing("perform"))?;
        // A program that handles nothing has nothing to search.
        if !self.handles {
            let Some((effect, name)) = native else {
                return Err(format!(
                    "`{op}` is performed, and nothing handles it or binds it to the runtime"
                ));
            };
            return Ok(self.native(effect, name, &xs, kc, resumed, &sc.env));
        }
        let ev = sc.var(EV)?;
        let key = self.fresh(ax::Rep::Str);
        sc.push(key);
        steps.push(Step::Extern(
            Extern::Lit(Lit::Sym(InternedString::from(op.to_string().as_str()))),
            Vec::new(),
            sc.env.clone(),
        ));
        // What both of the search's answers hold: the arguments, and where
        // what the operation is resumed with goes.
        let mut held = xs.clone();
        held.push(kc);
        let held = Self::once(&held);

        // Found: the entry, taken apart; a resumption made; the clause
        // entered with the arguments, the resumption, and what its handler's
        // value goes to now.
        let entry = self.fresh(ax::Rep::Ref);
        let (k2, clause, target, rest) = (
            self.fresh(ax::Rep::Str),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
        );
        let mut in_entry = vec![k2, clause, target, rest];
        in_entry.extend(held.iter().copied());
        in_entry.push(entry);
        let resumption = self.fresh(ax::Rep::Ref);
        let (v, after, unused, done) = (
            self.fresh(resumed),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Bits(desc::UNIT)),
        );
        // Called with a value and a continuation: that continuation is where
        // the handler's value goes from now on, and the code that performed
        // goes on with the value.
        let now = self.fresh(ax::Rep::Ref);
        let mut enter = vec![clause];
        enter.extend(xs.iter().copied());
        enter.extend([resumption, now]);
        let enter_params = self.distinct(&enter);
        let entered = if self.segmented() {
            // As `meadow_seq` has it: the operation's continuation is the
            // frames between here and its handler, which `Detach` cuts off
            // the stack and the resumption's `Reattach` puts back -- once,
            // which the flag it is made with says.
            let (flag, taken, seg) = (
                self.fresh(ax::Rep::Bits(desc::UNIT)),
                self.fresh(ax::Rep::Ref),
                self.fresh(ax::Rep::Ref),
            );
            let (first, back) = (
                self.fresh(ax::Rep::Bits(desc::BOOL)),
                self.fresh(ax::Rep::Bits(desc::UNIT)),
            );
            let resume = vec![taken, kc, target, seg, v, after, unused];
            let mut at_first = vec![first];
            at_first.extend(resume.iter().copied());
            let mut at_done = vec![done];
            at_done.extend(at_first.iter().copied());
            let mut at_back = vec![back];
            at_back.extend(at_done.iter().copied());
            let apply = Block {
                params: resume,
                body: S::Extern {
                    op: Extern::Prim(Prim::TakeOnce),
                    args: vec![taken],
                    blocks: vec![Block {
                        params: at_first.clone(),
                        body: S::Extern {
                            op: Extern::Branch,
                            args: vec![first],
                            blocks: vec![
                                Block {
                                    params: at_first.clone(),
                                    body: S::Error("continuation resumed more than once"),
                                },
                                Block {
                                    params: at_first,
                                    body: S::Extern {
                                        op: Extern::Prim(Prim::SetRef),
                                        args: vec![target, after],
                                        blocks: vec![Block {
                                            params: at_done,
                                            body: S::Extern {
                                                op: Extern::Prim(Prim::Reattach),
                                                args: vec![seg],
                                                blocks: vec![Block {
                                                    params: at_back,
                                                    body: S::Substitute(
                                                        vec![kc, v],
                                                        Box::new(Block {
                                                            params: vec![kc, v],
                                                            body: S::Invoke(kc, 0),
                                                        }),
                                                    ),
                                                }],
                                            },
                                        }],
                                    },
                                },
                            ],
                        },
                    }],
                },
            };
            let methods = self.function(apply)?;
            let mut with_flag = in_entry.clone();
            with_flag.insert(0, flag);
            let mut with_taken = with_flag.clone();
            with_taken.insert(0, taken);
            let mut with_seg = with_taken.clone();
            with_seg.insert(0, seg);
            let mut with_resumption = with_seg.clone();
            with_resumption.insert(0, resumption);
            let mut with_now = with_resumption.clone();
            with_now.insert(0, now);
            S::Extern {
                op: Extern::Lit(Lit::Unit),
                args: Vec::new(),
                blocks: vec![Block {
                    params: with_flag,
                    body: S::Extern {
                        op: Extern::Prim(Prim::Once),
                        args: vec![flag],
                        blocks: vec![Block {
                            params: with_taken,
                            body: S::Extern {
                                op: Extern::Prim(Prim::Detach),
                                args: vec![target],
                                blocks: vec![Block {
                                    params: with_seg,
                                    body: S::New {
                                        name: resumption,
                                        captures: vec![taken, kc, target, seg],
                                        methods,
                                        rest: Box::new(S::Extern {
                                            op: Extern::Prim(Prim::GetRef),
                                            args: vec![target],
                                            blocks: vec![Block {
                                                params: with_now,
                                                body: S::Substitute(
                                                    enter,
                                                    Box::new(Block {
                                                        params: enter_params,
                                                        body: S::Invoke(clause, 0),
                                                    }),
                                                ),
                                            }],
                                        }),
                                    },
                                }],
                            },
                        }],
                    },
                }],
            }
        } else {
            let apply = Block {
                params: vec![kc, target, v, after, unused],
                body: S::Extern {
                    op: Extern::Prim(Prim::SetRef),
                    args: vec![target, after],
                    blocks: vec![Block {
                        params: vec![done, kc, target, v, after, unused],
                        body: S::Substitute(
                            vec![kc, v],
                            Box::new(Block {
                                params: vec![kc, v],
                                body: S::Invoke(kc, 0),
                            }),
                        ),
                    }],
                },
            };
            let methods = self.function(apply)?;
            let mut with_resumption = in_entry.clone();
            with_resumption.insert(0, resumption);
            let mut with_now = with_resumption.clone();
            with_now.insert(0, now);
            S::New {
                name: resumption,
                captures: vec![kc, target],
                methods,
                rest: Box::new(S::Extern {
                    op: Extern::Prim(Prim::GetRef),
                    args: vec![target],
                    blocks: vec![Block {
                        params: with_now,
                        body: S::Substitute(
                            enter,
                            Box::new(Block {
                                params: enter_params,
                                body: S::Invoke(clause, 0),
                            }),
                        ),
                    }],
                }),
            }
        };
        let mut found_params = held.clone();
        found_params.push(entry);
        let entry_tag = self.tag_of(meadow_rt::roles::evidence::ENTRY);
        let found = self.fresh(ax::Rep::Ref);
        self.out.continuations.insert(found);
        steps.push(Step::New(
            found,
            held.clone(),
            vec![Block {
                params: found_params.clone(),
                body: S::Switch {
                    scrutinee: entry,
                    arms: vec![(
                        entry_tag,
                        Block {
                            params: in_entry,
                            body: entered,
                        },
                    )],
                    default: Box::new(Block {
                        params: found_params,
                        body: S::Error("the evidence holds what is not an entry"),
                    }),
                },
            }],
        ));
        sc.push(found);

        // Not found: the runtime's, if it is one of its operations.
        let nothing = self.fresh(ax::Rep::Bits(desc::UNIT));
        let mut missing_params = held.clone();
        missing_params.push(nothing);
        let body = match native {
            Some((effect, name)) => self.native(effect, name, &xs, kc, resumed, &missing_params),
            None => S::Error(Box::leak(format!("unhandled effect {op}").into_boxed_str())),
        };
        let missing = self.fresh(ax::Rep::Ref);
        self.out.continuations.insert(missing);
        steps.push(Step::New(
            missing,
            held,
            vec![Block {
                params: missing_params,
                body,
            }],
        ));
        sc.push(missing);

        let sel = vec![ev, key, found, missing];
        Ok(S::Substitute(
            sel.clone(),
            Box::new(Block {
                params: sel,
                body: S::Jump(self.search),
            }),
        ))
    }

    /// The block that looks an operation up: `(evidence, key, found,
    /// missing)`, giving `found` the first entry whose key is `key`, or
    /// `missing` `unit` if the evidence ends first.
    fn search_block(&mut self) {
        let (cur, key, found, missing) = (
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Str),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
        );
        let (k2, clause, target, rest) = (
            self.fresh(ax::Rep::Str),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
            self.fresh(ax::Rep::Ref),
        );
        let params = vec![cur, key, found, missing];
        let in_entry = vec![k2, clause, target, rest, cur, key, found, missing];
        let u = self.fresh(ax::Rep::Bits(desc::UNIT));
        let ended = S::Extern {
            op: Extern::Lit(Lit::Unit),
            args: Vec::new(),
            blocks: vec![Block {
                params: vec![u, cur, key, found, missing],
                body: S::Substitute(
                    vec![missing, u],
                    Box::new(Block {
                        params: vec![missing, u],
                        body: S::Invoke(missing, 0),
                    }),
                ),
            }],
        };
        let next = vec![rest, key, found, missing];
        let body = S::Switch {
            scrutinee: cur,
            arms: vec![(
                self.tag_of(meadow_rt::roles::evidence::ENTRY),
                Block {
                    params: in_entry.clone(),
                    body: S::Extern {
                        op: Extern::BranchPrim(Prim::Eq),
                        args: vec![k2, key],
                        blocks: vec![
                            Block {
                                params: in_entry.clone(),
                                body: S::Substitute(
                                    next.clone(),
                                    Box::new(Block {
                                        params: next,
                                        body: S::Jump(self.search),
                                    }),
                                ),
                            },
                            Block {
                                params: in_entry,
                                body: S::Substitute(
                                    vec![found, cur],
                                    Box::new(Block {
                                        params: vec![found, cur],
                                        body: S::Invoke(found, 0),
                                    }),
                                ),
                            },
                        ],
                    },
                },
            )],
            default: Box::new(Block {
                params: params.clone(),
                body: ended,
            }),
        };
        debug_assert_eq!(self.search, Label(self.out.defs.len() as u32));
        self.out.defs.push(ax::Def {
            label: self.search,
            name: InternedString::from("#perform"),
            module: InternedString::from(""),
            block: Block { params, body },
        });
    }

    /// `handle { clauses; return } in μ b. body ; c`.
    fn handle(&mut self, h: &crate::Handle, sc: &mut Scope, steps: &mut Vec<Step>) -> R<S> {
        let outer = sc.var(EV)?;
        let kc = self.reify_as(&h.cont, sc, steps, self.framing("handle"))?;
        // Where the handler's value goes, which a resumption changes.
        let target = self.fresh(ax::Rep::Ref);
        sc.push(target);
        steps.push(Step::Extern(
            Extern::Prim(Prim::NewRef),
            vec![kc],
            sc.env.clone(),
        ));
        // The handled body on a segment of the stack of its own, which an
        // operation that captures its continuation cuts off whole
        // (`docs/SILO.md`, "Effects: stack segments"). With every
        // continuation on the heap there is nothing on the stack to cut.
        if self.segmented() {
            let entered = self.fresh(ax::Rep::Bits(desc::UNIT));
            sc.push(entered);
            steps.push(Step::Extern(
                Extern::Prim(Prim::Enter),
                vec![target],
                sc.env.clone(),
            ));
        }
        let entry = meadow_rt::roles::evidence::ENTRY;
        let entry_tag = self.tag_of(entry);
        let mut ev = outer;
        for c in &h.clauses {
            // The clause: an object of what it mentions from around the
            // handler, the evidence outside it among that.
            let mut free = Free::default();
            let bound: Vec<&str> = c
                .params
                .iter()
                .map(|b| b.name.as_str())
                .chain([c.resumption.as_str(), c.cont.as_str()])
                .collect();
            free.under(&bound, |f| f.statement(&c.body));
            free.names.insert(EV.to_string());
            let mut inside = sc.clone();
            inside.vars.insert(EV.to_string(), outer);
            let captures = free.among(&inside);
            let mut inner = Scope {
                env: captures.clone(),
                vars: inside.vars.clone(),
                halt: sc.halt,
            };
            for b in &c.params {
                let n = self.fresh(self.rep(&b.rep)?);
                inner.bind(&b.name, n);
                self.called(n, &b.name);
            }
            for k in [&c.resumption, &c.cont] {
                let n = self.fresh(ax::Rep::Ref);
                inner.bind(k, n);
                self.called(n, k);
            }
            let params = inner.env.clone();
            let body = self.statement(&c.body, &inner)?;
            let clause = self.fresh(ax::Rep::Ref);
            steps.push(Step::New(clause, captures, vec![Block { params, body }]));
            sc.push(clause);
            let key = self.fresh(ax::Rep::Str);
            sc.push(key);
            steps.push(Step::Extern(
                Extern::Lit(Lit::Sym(InternedString::from(c.op.to_string().as_str()))),
                Vec::new(),
                sc.env.clone(),
            ));
            let pushed = self.fresh(ax::Rep::Ref);
            steps.push(Step::Let(
                pushed,
                entry_tag,
                InternedString::from(entry),
                vec![key, clause, target, ev],
            ));
            sc.push(pushed);
            ev = pushed;
        }
        // What the body answers: the `return` clause, run where the target
        // says the handler's value goes now, under the evidence outside.
        let (x, k, ret) = &h.ret;
        let mut free = Free::default();
        free.under(&[&x.name, k], |f| f.statement(ret));
        free.names.insert(EV.to_string());
        let mut outside = sc.clone();
        outside.vars.insert(EV.to_string(), outer);
        let mut captures = free.among(&outside);
        if !captures.contains(&target) {
            captures.push(target);
        }
        let value = self.fresh(self.rep(&x.rep)?);
        self.called(value, &x.name);
        let now = self.fresh(ax::Rep::Ref);
        self.called(now, k);
        let mut inner = Scope {
            env: captures.clone(),
            vars: outside.vars.clone(),
            halt: sc.halt,
        };
        inner.bind(&x.name, value);
        let returned_params = inner.env.clone();
        inner.push(now);
        inner.vars.insert(k.clone(), now);
        let after = inner.env.clone();
        let returned = S::Extern {
            op: Extern::Prim(Prim::GetRef),
            args: vec![target],
            blocks: vec![Block {
                params: after,
                body: self.statement(ret, &inner)?,
            }],
        };
        let answers = self.fresh(ax::Rep::Ref);
        self.out.continuations.insert(answers);
        self.called(answers, &h.body_cont);
        steps.push(Step::New(
            answers,
            captures,
            vec![Block {
                params: returned_params,
                body: returned,
            }],
        ));
        sc.push(answers);
        let mut body = sc.clone();
        body.vars.insert(h.body_cont.clone(), answers);
        body.vars.insert(EV.to_string(), ev);
        self.statement(&h.body, &body)
    }

    /// Whether continuations of `kind` are frames.
    fn framing(&self, kind: &str) -> bool {
        self.frames
            .as_ref()
            .is_some_and(|ks| ks.is_empty() || ks.iter().any(|k| k == kind))
    }

    /// Whether a handler's body runs on a stack segment of its own: wherever
    /// any continuation is a frame, since a frame is the native stack's.
    fn segmented(&self) -> bool {
        self.frames.is_some()
    }

    /// Whether `body`, which a `μ` names `k` for, can only go on by `k`: it
    /// mentions no continuation from around it, so nothing in it answers past
    /// `k`. What `k` stands for is then entered exactly once, with whatever
    /// was pushed after it dead -- a frame's due. A `μ` whose body may go
    /// round it -- an arm that answers the function's caller -- is not: its
    /// frame would be left behind each time.
    fn always_enters(&self, body: &Statement, k: &str, sc: &Scope) -> bool {
        let mut free = Free::default();
        free.statement(body);
        !free.halt
            && free
                .names
                .iter()
                .filter(|n| n.as_str() != k)
                .filter_map(|n| sc.vars.get(n))
                .all(|n| !self.out.continuations.contains(n) && !self.out.returns.contains(n))
    }

    /// `sel` as a block's parameters: each name once, a second mention under
    /// a name of its own. What an `invoke` enters is first, and so keeps its
    /// name.
    fn distinct(&mut self, sel: &[Name]) -> Vec<Name> {
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(sel.len());
        for &n in sel {
            if seen.insert(n) {
                out.push(n);
            } else {
                let rep = self.out.reps.get(&n).copied().unwrap_or(ax::Rep::Ref);
                out.push(self.fresh(rep));
            }
        }
        out
    }

    /// A primitive: a test, which takes two continuations and gives each
    /// `unit`, or one that answers a value to its one.
    fn prim(&mut self, op: &str, args: Vec<Name>, conts: &[Consumer], sc: &Scope) -> R<S> {
        if op == "if" {
            let [no, yes] = conts else {
                return Err("`prim if` takes two continuations".to_string());
            };
            return Ok(S::Extern {
                op: Extern::Branch,
                args,
                blocks: vec![self.branch(no, sc)?, self.branch(yes, sc)?],
            });
        }
        let Some(p) = prim_named(op) else {
            return Err(format!("`{op}` is not a primitive of the runtimes"));
        };
        match conts {
            [no, yes] => Ok(S::Extern {
                op: Extern::BranchPrim(p),
                args,
                blocks: vec![self.branch(no, sc)?, self.branch(yes, sc)?],
            }),
            [c] => {
                let rep = match c {
                    Consumer::MuTilde(b, _) => self.rep(&b.rep)?,
                    _ => self.answer_of(p, &args),
                };
                let r = self.fresh(rep);
                let mut inner = sc.clone();
                inner.push(r);
                let body = self.give(r, c, &inner)?;
                Ok(S::Extern {
                    op: Extern::Prim(p),
                    args,
                    blocks: vec![Block {
                        params: inner.env,
                        body,
                    }],
                })
            }
            _ => Err(format!("`prim {op}` takes one continuation, or two")),
        }
    }

    /// How what `p` answers is represented, where the continuation it goes
    /// to does not say: a name, which has no representation written.
    fn answer_of(&self, p: Prim, args: &[Name]) -> ax::Rep {
        use Prim::*;
        match p {
            Eq | Ne | Lt | Gt | Le | Ge | LtF | GtF | LeF | GeF => ax::Rep::Bits(desc::BOOL),
            ArrayLen | StringByteLength | StringCompare | CharCode | Hash | PopCount | BitWidth
            | ToInt | StArrayLen | CompactSize => ax::Rep::Int,
            ToWord(w) => ax::Rep::Bits(desc::word(w)),
            ToFloat32 => ax::Rep::Bits(desc::FLOAT32),
            CharFromCode => ax::Rep::Bits(desc::CHAR),
            ToFloat | Floor => ax::Rep::Float,
            Add | Sub | Mul | Div | Mod | Pow | Neg | Shl | Shr | Ushr | BitAnd | BitOr
            | BitXor | BitNot | AddF | SubF | MulF | DivF => args
                .first()
                .and_then(|a| self.out.reps.get(a))
                .copied()
                .unwrap_or(ax::Rep::Int),
            SetRef | StSetArray => ax::Rep::Bits(desc::UNIT),
            _ => ax::Rep::Ref,
        }
    }

    /// A test's continuation, as the block the test goes to: it receives
    /// `unit`, which is made only if something reads it.
    fn branch(&mut self, c: &Consumer, sc: &Scope) -> R<Block> {
        let body = match c {
            Consumer::MuTilde(b, s) if !mentions(s, &b.name) => self.statement(s, sc)?,
            _ => {
                let u = self.fresh(ax::Rep::Bits(desc::UNIT));
                let mut inner = sc.clone();
                inner.push(u);
                let body = self.give(u, c, &inner)?;
                S::Extern {
                    op: Extern::Lit(Lit::Unit),
                    args: Vec::new(),
                    blocks: vec![Block {
                        params: inner.env,
                        body,
                    }],
                }
            }
        };
        Ok(Block {
            params: sc.env.clone(),
            body,
        })
    }

    /// The value named `v` given to `c`.
    fn give(&mut self, v: Name, c: &Consumer, sc: &Scope) -> R<S> {
        match c {
            Consumer::Var(_) | Consumer::Halt => {
                let k = self.named(c, sc)?;
                let sel = vec![k, v];
                let params = self.distinct(&sel);
                Ok(S::Substitute(
                    sel,
                    Box::new(Block {
                        params,
                        body: S::Invoke(k, 0),
                    }),
                ))
            }
            Consumer::MuTilde(b, s) => {
                let mut inner = sc.clone();
                inner.vars.insert(b.name.clone(), v);
                self.called(v, &b.name);
                self.statement(s, &inner)
            }
            Consumer::Case(arms) => self.switch(v, arms, sc),
            Consumer::Method(m, args, conts) => {
                let tag = self.method(m)?;
                let mut sc = sc.clone();
                let mut steps = Vec::new();
                let mut sel = vec![v];
                for a in args {
                    sel.push(self.atom(a, &mut sc, &mut steps)?);
                }
                for k in conts {
                    sel.push(self.reify_as(
                        k,
                        &mut sc,
                        &mut steps,
                        conts.len() == 1 && self.framing("method"),
                    )?);
                }
                sel.push(sc.var(EV)?);
                let params = self.distinct(&sel);
                Ok(wrap(
                    steps,
                    S::Substitute(
                        sel,
                        Box::new(Block {
                            params,
                            body: S::Invoke(v, tag),
                        }),
                    ),
                ))
            }
        }
    }

    fn switch(&mut self, v: Name, arms: &[Arm], sc: &Scope) -> R<S> {
        let mut out = Vec::new();
        let mut default = None;
        for arm in arms {
            let tag = match &arm.pattern {
                Pattern::Con(k) => self.tag_of(&k.to_string()),
                Pattern::Tuple => self.tag_of(meadow_rt::roles::TUPLE),
                Pattern::Default => {
                    default = Some(Block {
                        params: sc.env.clone(),
                        body: self.statement(&arm.body, sc)?,
                    });
                    continue;
                }
            };
            // An arm's parameters are the fields, then the environment.
            let mut inner = Scope {
                env: Vec::new(),
                vars: sc.vars.clone(),
                halt: sc.halt,
            };
            for f in &arm.fields {
                let n = self.fresh(self.rep(&f.rep)?);
                inner.bind(&f.name, n);
                self.called(n, &f.name);
            }
            inner.env.extend(sc.env.iter().copied());
            let body = self.statement(&arm.body, &inner)?;
            out.push((
                tag,
                Block {
                    params: inner.env,
                    body,
                },
            ));
        }
        let default = default.unwrap_or_else(|| Block {
            params: sc.env.clone(),
            body: S::Error("no arm of the case matches"),
        });
        Ok(S::Switch {
            scrutinee: v,
            arms: out,
            default: Box::new(default),
        })
    }

    /// The name of a consumer that is one.
    fn named(&self, c: &Consumer, sc: &Scope) -> R<Name> {
        match c {
            Consumer::Var(k) => sc.var(k),
            Consumer::Halt => sc
                .halt
                .ok_or_else(|| "`halt` outside a top-level value".to_string()),
            _ => Err("not a name".to_string()),
        }
    }

    /// `c` by a name: itself, or an object of one method made of it, which
    /// captures what it mentions.
    /// `c` as a continuation by a name. One made for a call to return
    /// through -- `frame` -- is entered once, by that call, and everything
    /// pushed after it is dead by then: the back end keeps it on the thread's
    /// frame stack and not the heap (`Program::frames`). One a `μ` names is
    /// not: a branch may never reach it.
    fn reify_as(
        &mut self,
        c: &Consumer,
        sc: &mut Scope,
        steps: &mut Vec<Step>,
        frame: bool,
    ) -> R<Name> {
        if matches!(c, Consumer::Var(_) | Consumer::Halt) {
            return self.named(c, sc);
        }
        let mut free = Free::default();
        free.consumer(c);
        free.names.insert(EV.to_string());
        let captures = free.among(sc);
        let rep = match c {
            Consumer::MuTilde(b, _) => self.rep(&b.rep)?,
            _ => ax::Rep::Ref,
        };
        let x = self.fresh(rep);
        let mut inner = Scope {
            env: captures.clone(),
            vars: sc.vars.clone(),
            halt: sc.halt,
        };
        inner.env.push(x);
        let body = self.give(x, c, &inner)?;
        let k = self.fresh(ax::Rep::Ref);
        self.out.continuations.insert(k);
        // Off unless `MEADOW_CUT_FRAMES` is set: a program this lowers with
        // frames runs on Glade and spins on Silo, for a reason not yet found.
        if frame {
            self.out.frames.insert(k);
        }
        steps.push(Step::New(
            k,
            captures,
            vec![Block {
                params: inner.env,
                body,
            }],
        ));
        sc.push(k);
        Ok(k)
    }

    /// `p` by a name: one it has, or one a step binds it to.
    fn atom(&mut self, p: &Producer, sc: &mut Scope, steps: &mut Vec<Step>) -> R<Name> {
        let lit = |l: &mut Lower, sc: &mut Scope, steps: &mut Vec<Step>, lit: Lit, rep: ax::Rep| {
            let n = l.fresh(rep);
            sc.push(n);
            steps.push(Step::Extern(Extern::Lit(lit), Vec::new(), sc.env.clone()));
            n
        };
        Ok(match p {
            Producer::Var(x) => sc.var(x)?,
            Producer::Int(i) => lit(self, sc, steps, Lit::Int(*i), ax::Rep::Int),
            Producer::Float(x) => lit(self, sc, steps, Lit::Float(*x), ax::Rep::Float),
            Producer::Char(c) => lit(self, sc, steps, Lit::Char(*c), ax::Rep::Bits(desc::CHAR)),
            Producer::Bool(b) => lit(self, sc, steps, Lit::Bool(*b), ax::Rep::Bits(desc::BOOL)),
            Producer::Unit => lit(self, sc, steps, Lit::Unit, ax::Rep::Bits(desc::UNIT)),
            Producer::Str(s) => lit(
                self,
                sc,
                steps,
                Lit::Str(InternedString::from(s.as_str())),
                ax::Rep::Ref,
            ),
            // A value is asked of the definition that makes it the first
            // time this thread wants it (`lazily`): what follows is what it
            // is given to.
            Producer::Val(v) => {
                if let Some(literal) = self.literals.get(v).cloned() {
                    return self.atom(&literal, sc, steps);
                }
                let Some(&(_, rep)) = self.vals.get(v) else {
                    return Err(format!("`{v}` is not a value of this program"));
                };
                let k = format!("#v{}", self.next);
                self.expected = Some(rep);
                self.atom(
                    &Producer::Mu(
                        k.clone(),
                        Box::new(Statement::Call(
                            forcing(v),
                            Vec::new(),
                            vec![Consumer::Var(k)],
                        )),
                    ),
                    sc,
                    steps,
                )?
            }
            Producer::Con(k, args) => {
                let name = k.to_string();
                self.data(&name, args, sc, steps)?
            }
            Producer::Tuple(items) => self.data(meadow_rt::roles::TUPLE, items, sc, steps)?,
            Producer::Array(items) => {
                let mut args = Vec::new();
                for x in items {
                    args.push(self.atom(x, sc, steps)?);
                }
                let n = self.fresh(ax::Rep::Ref);
                sc.push(n);
                steps.push(Step::Extern(Extern::Array, args, sc.env.clone()));
                n
            }
            Producer::Record(fields) => {
                let mut args = Vec::new();
                for (_, x) in fields {
                    args.push(self.atom(x, sc, steps)?);
                }
                let labels = fields
                    .iter()
                    .map(|(l, _)| InternedString::from(l.as_str()))
                    .collect();
                let n = self.fresh(ax::Rep::Ref);
                sc.push(n);
                steps.push(Step::Extern(Extern::Record(labels), args, sc.env.clone()));
                n
            }
            Producer::Cocase(methods) => {
                let mut free = Free::default();
                for m in methods {
                    free.method(m);
                }
                let captures = free.among(sc);
                // A method's parameters begin with the object's captures,
                // those it lacks as well.
                let mut blocks: Vec<Block> = (0..self.methods.len())
                    .map(|_| Block {
                        params: captures.clone(),
                        body: S::Error("the object has no such method"),
                    })
                    .collect();
                for m in methods {
                    let mut inner = Scope {
                        env: captures.clone(),
                        vars: sc.vars.clone(),
                        halt: sc.halt,
                    };
                    for b in &m.params {
                        let n = self.fresh(self.rep(&b.rep)?);
                        inner.bind(&b.name, n);
                        self.called(n, &b.name);
                    }
                    for k in &m.conts {
                        let n = self.fresh(ax::Rep::Ref);
                        inner.bind(k, n);
                        self.called(n, k);
                        self.out.returns.insert(n);
                    }
                    let ev = self.fresh(ax::Rep::Ref);
                    inner.bind(EV, ev);
                    let params = inner.env.clone();
                    let body = self.statement(&m.body, &inner)?;
                    let tag = self.method(&m.name)? as usize;
                    blocks[tag] = Block { params, body };
                }
                let n = self.fresh(ax::Rep::Ref);
                sc.push(n);
                steps.push(Step::New(n, captures, blocks));
                n
            }
            Producer::Mu(k, body) => {
                let Some(rep) = self.expected.take() else {
                    return Err(
                        "a `μ` is lowered only as a definition's argument: elsewhere nothing says how its value is represented"
                            .to_string(),
                    );
                };
                // What follows captures everything in scope, and goes on
                // with the value.
                let captures = sc.env.clone();
                let object = self.fresh(ax::Rep::Ref);
                self.out.continuations.insert(object);
                // What a call is made with to answer through -- a value read
                // of its definition is one -- is a frame, as any call's is.
                if self.framing("join") && self.always_enters(body, k, sc) {
                    self.out.frames.insert(object);
                }
                self.called(object, k);
                let mut inside = sc.clone();
                inside.push(object);
                inside.vars.insert(k.clone(), object);
                let lowered = self.statement(body, &inside)?;
                let value = self.fresh(rep);
                sc.env.push(value);
                steps.push(Step::Mu(object, captures, sc.env.clone(), lowered));
                value
            }
            // A descriptor: the one in scope for a variable, a constant for
            // a representation that is known.
            Producer::Desc(Rep::Var(a)) => match self.rep_vars.get(a) {
                Some(n) => *n,
                None => {
                    return Err(format!(
                        "`'{a}` is not a representation variable of the definition it is in"
                    ));
                }
            },
            Producer::Desc(r) => {
                let Some(code) = self.rep(r)?.desc() else {
                    return Err(format!("`{r}` has no descriptor"));
                };
                let n = lit(self, sc, steps, Lit::Int(i64::from(code)), ax::Rep::Int);
                self.descs.insert(n);
                n
            }
        })
    }

    fn data(
        &mut self,
        ctor: &str,
        args: &[Producer],
        sc: &mut Scope,
        steps: &mut Vec<Step>,
    ) -> R<Name> {
        let mut fields = Vec::new();
        for a in args {
            fields.push(self.atom(a, sc, steps)?);
        }
        let tag = self.tag_of(ctor);
        let n = self.fresh(ax::Rep::Ref);
        sc.push(n);
        steps.push(Step::Let(
            n,
            tag,
            InternedString::from(ctor_name(ctor).as_str()),
            fields,
        ));
        Ok(n)
    }
}

/// The primitive Cut calls `name`: `meadow_rt::Prim`'s name, its first
/// letter small, and a conversion to a sized integer by its type's.
fn prim_named(name: &str) -> Option<Prim> {
    if let Some(p) = Prim::from_name(name)
        && matches!(p, Prim::ToWord(_))
    {
        return Some(p);
    }
    (0..u16::MAX).map_while(Prim::from_code).find(|p| {
        let shown = format!("{p:?}");
        let mut cs = shown.chars();
        cs.next().is_some_and(|c| {
            !shown.contains('(') && name == format!("{}{}", c.to_lowercase(), cs.as_str())
        })
    })
}

/// Whether `s` mentions the value or continuation named `name`.
fn mentions(s: &Statement, name: &str) -> bool {
    let mut free = Free::default();
    free.statement(s);
    free.names.contains(name)
}

/// The names something mentions and does not bind, and whether `halt` is
/// one of them.
#[derive(Default)]
struct Free {
    names: HashSet<String>,
    bound: Vec<String>,
    halt: bool,
    /// Whether a `handle` was met.
    handles: bool,
}

impl Free {
    /// What was found, of the environment: in its order, each once.
    fn among(&self, sc: &Scope) -> Vec<Name> {
        let mut wanted: HashSet<Name> = self
            .names
            .iter()
            .filter_map(|n| sc.vars.get(n).copied())
            .collect();
        if self.halt {
            wanted.extend(sc.halt);
        }
        let mut seen = HashSet::new();
        sc.env
            .iter()
            .copied()
            .filter(|n| wanted.contains(n) && seen.insert(*n))
            .collect()
    }

    fn name(&mut self, n: &str) {
        if !self.bound.iter().any(|b| b == n) {
            self.names.insert(n.to_string());
        }
    }

    fn under(&mut self, names: &[&str], f: impl FnOnce(&mut Free)) {
        let mark = self.bound.len();
        self.bound.extend(names.iter().map(|n| n.to_string()));
        f(self);
        self.bound.truncate(mark);
    }

    fn method(&mut self, m: &crate::Method) {
        let names: Vec<&str> = m
            .params
            .iter()
            .map(|b| b.name.as_str())
            .chain(m.conts.iter().map(String::as_str))
            .collect();
        self.under(&names, |f| f.statement(&m.body));
    }

    fn producer(&mut self, p: &Producer) {
        match p {
            Producer::Var(x) => self.name(x),
            Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
                xs.iter().for_each(|x| self.producer(x))
            }
            Producer::Record(fs) => fs.iter().for_each(|(_, x)| self.producer(x)),
            Producer::Mu(k, s) => self.under(&[k], |f| f.statement(s)),
            Producer::Cocase(ms) => ms.iter().for_each(|m| self.method(m)),
            Producer::Val(_)
            | Producer::Int(_)
            | Producer::Float(_)
            | Producer::Char(_)
            | Producer::Str(_)
            | Producer::Bool(_)
            | Producer::Unit
            | Producer::Desc(_) => {}
        }
    }

    fn consumer(&mut self, c: &Consumer) {
        match c {
            Consumer::Var(k) => self.name(k),
            Consumer::Halt => self.halt = true,
            Consumer::MuTilde(b, s) => self.under(&[&b.name], |f| f.statement(s)),
            Consumer::Case(arms) => {
                for arm in arms {
                    let names: Vec<&str> = arm.fields.iter().map(|b| b.name.as_str()).collect();
                    self.under(&names, |f| f.statement(&arm.body));
                }
            }
            Consumer::Method(_, args, conts) => {
                args.iter().for_each(|a| self.producer(a));
                conts.iter().for_each(|k| self.consumer(k));
            }
        }
    }

    fn statement(&mut self, s: &Statement) {
        match s {
            Statement::Cut(p, c) => {
                self.producer(p);
                self.consumer(c);
            }
            Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
                args.iter().for_each(|a| self.producer(a));
                conts.iter().for_each(|k| self.consumer(k));
            }
            Statement::Let(b, p, rest) => {
                self.producer(p);
                self.under(&[&b.name], |f| f.statement(rest));
            }
            Statement::Perform(_, args, c) => {
                args.iter().for_each(|a| self.producer(a));
                self.consumer(c);
            }
            Statement::Handle(h) => {
                self.handles = true;
                for c in &h.clauses {
                    let names: Vec<&str> = c
                        .params
                        .iter()
                        .map(|b| b.name.as_str())
                        .chain([c.resumption.as_str(), c.cont.as_str()])
                        .collect();
                    self.under(&names, |f| f.statement(&c.body));
                }
                let (x, k, body) = &h.ret;
                self.under(&[&x.name, k], |f| f.statement(body));
                self.under(&[&h.body_cont], |f| f.statement(&h.body));
                self.consumer(&h.cont);
            }
            Statement::Error(_) => {}
        }
    }
}

/// Every method an object of `s` has or a call in it names.
fn methods_of(s: &Statement, out: &mut HashSet<String>) {
    fn producer(p: &Producer, out: &mut HashSet<String>) {
        match p {
            Producer::Con(_, xs) | Producer::Tuple(xs) | Producer::Array(xs) => {
                xs.iter().for_each(|x| producer(x, out))
            }
            Producer::Record(fs) => fs.iter().for_each(|(_, x)| producer(x, out)),
            Producer::Mu(_, s) => methods_of(s, out),
            Producer::Cocase(ms) => {
                for m in ms {
                    out.insert(m.name.clone());
                    methods_of(&m.body, out);
                }
            }
            _ => {}
        }
    }
    fn consumer(c: &Consumer, out: &mut HashSet<String>) {
        match c {
            Consumer::Var(_) | Consumer::Halt => {}
            Consumer::MuTilde(_, s) => methods_of(s, out),
            Consumer::Case(arms) => arms.iter().for_each(|a| methods_of(&a.body, out)),
            Consumer::Method(m, args, conts) => {
                out.insert(m.clone());
                args.iter().for_each(|a| producer(a, out));
                conts.iter().for_each(|k| consumer(k, out));
            }
        }
    }
    match s {
        Statement::Cut(p, c) => {
            producer(p, out);
            consumer(c, out);
        }
        Statement::Call(_, args, conts) | Statement::Prim(_, args, conts) => {
            args.iter().for_each(|a| producer(a, out));
            conts.iter().for_each(|k| consumer(k, out));
        }
        Statement::Let(_, p, rest) => {
            producer(p, out);
            methods_of(rest, out);
        }
        Statement::Perform(_, args, c) => {
            args.iter().for_each(|a| producer(a, out));
            consumer(c, out);
        }
        Statement::Handle(h) => {
            h.clauses.iter().for_each(|c| methods_of(&c.body, out));
            methods_of(&h.ret.2, out);
            methods_of(&h.body, out);
            consumer(&h.cont, out);
        }
        Statement::Error(_) => {}
    }
}

/// A constructor's name as a runtime has it, of its symbol as text. A
/// runtime hashes a value of a data type by its constructor's name, and
/// Meadow's rule is the name the compiler's own lowering hands a runtime: the
/// package and then the path, `Json.Value.Null` -- the standard library's
/// are named from its root, `List.Cons`, with no package -- so that is what
/// one of Meadow's is called. Any other language's is its whole symbol.
fn ctor_name(symbol: &str) -> String {
    let Some((package, path)) = symbol
        .strip_prefix("meadow:")
        .and_then(|s| s.split_once('/'))
    else {
        return symbol.to_string();
    };
    let package = package.split('@').next().unwrap_or(package);
    if package == "Std" {
        path.to_string()
    } else {
        format!("{package}.{path}")
    }
}

/// Which continuations `MEADOW_CUT_FRAMES` asks to be frames: every kind
/// unless it says otherwise -- `none`, or the kinds wanted, of `call`,
/// `method`, `perform`, `handle` and `join`.
fn frame_kinds() -> Option<Vec<String>> {
    match std::env::var("MEADOW_CUT_FRAMES") {
        Err(_) => Some(Vec::new()),
        Ok(v) if v == "none" => None,
        Ok(v) if v == "all" || v == "1" || v.is_empty() => Some(Vec::new()),
        Ok(v) => Some(v.split(',').map(str::to_string).collect()),
    }
}

/// The definition that answers a value: its symbol, by the value's.
fn forcing(value: &Symbol) -> Symbol {
    value.child("#force")
}

/// The value a definition answers, if it is one `forcing` named; itself
/// otherwise.
fn forced(def: &Symbol) -> Symbol {
    match def.path.split_last() {
        Some((last, rest)) if last == "#force" => Symbol {
            path: rest.to_vec(),
            ..def.clone()
        },
        _ => def.clone(),
    }
}

/// `p` with each of its values a definition that makes it when it is first
/// wanted, and keeps it: a thread has globals of its own, so one that was
/// made when the program started is made for the thread that started it and
/// no other. What `meadow_core::globals` does to a program of core. The
/// definition asks whether the value's place is filled, answers what is
/// there if it is, and otherwise runs the value's statement -- `halt` in it
/// the continuation that fills the place and answers. A value that is a
/// function written out has no place: its definition answers the function.
fn lazily(p: &Program) -> Program {
    let mut out = p.clone();
    out.vals.clear();
    let unit = |name: &str| Binder {
        name: name.to_string(),
        rep: Rep::Unit,
    };
    for (i, v) in p.vals.iter().enumerate() {
        let place = || Producer::Int(i as i64);
        let k = || Consumer::Var("#k".to_string());
        let made = Binder {
            name: "#made".to_string(),
            rep: v.rep.clone(),
        };
        let kept = Statement::Prim(
            "globalSet".to_string(),
            vec![place(), Producer::Var(made.name.clone())],
            vec![Consumer::MuTilde(
                unit("#set"),
                Box::new(Statement::Cut(Producer::Var(made.name.clone()), k())),
            )],
        );
        let make = Statement::Cut(
            Producer::Mu("#halt".to_string(), Box::new(rehalted(&v.body))),
            Consumer::MuTilde(made.clone(), Box::new(kept)),
        );
        let have = Statement::Prim(
            "globalGet".to_string(),
            vec![place()],
            vec![Consumer::MuTilde(
                Binder {
                    name: "#had".to_string(),
                    rep: v.rep.clone(),
                },
                Box::new(Statement::Cut(Producer::Var("#had".to_string()), k())),
            )],
        );
        // A function is made again where it is wanted, and has no place: it
        // holds nothing, so making it is one object, where asking whether a
        // place is filled and reading it are two calls into the runtime --
        // and a program mentions its functions far more than its values.
        if let Statement::Cut(Producer::Cocase(methods), Consumer::Halt) = &v.body {
            out.defs.push(Def {
                symbol: forcing(&v.symbol),
                rep_vars: Vec::new(),
                effect_vars: Vec::new(),
                params: Vec::new(),
                conts: vec!["#k".to_string()],
                body: Statement::Cut(Producer::Cocase(methods.clone()), k()),
            });
            continue;
        }
        let body = Statement::Prim(
            "globalReady".to_string(),
            vec![place()],
            vec![Consumer::MuTilde(
                Binder {
                    name: "#ready".to_string(),
                    rep: Rep::Bool,
                },
                Box::new(Statement::Prim(
                    "if".to_string(),
                    vec![Producer::Var("#ready".to_string())],
                    vec![
                        // `prim if` takes what to do when it is not, first.
                        Consumer::MuTilde(unit("#no"), Box::new(make)),
                        Consumer::MuTilde(unit("#yes"), Box::new(have)),
                    ],
                )),
            )],
        );
        out.defs.push(Def {
            symbol: forcing(&v.symbol),
            rep_vars: Vec::new(),
            effect_vars: Vec::new(),
            params: Vec::new(),
            conts: vec!["#k".to_string()],
            body,
        });
    }
    out
}

/// `lazy`, with an entry that first makes each value that might perform, in
/// the order `whole` writes them, and then runs the entry: a value's effects
/// happen when the program starts, once (`docs/CUT.md`). The rest are made
/// when they are first read, which a program cannot tell from their being
/// made at the start -- and so one that is never read is never made.
fn started(whole: &Program, lazy: &mut Program) {
    let Some(entry) = lazy.entry.clone() else {
        return;
    };
    let pure = crate::pure::defs(lazy);
    let performing: Vec<(Symbol, Rep)> = whole
        .vals
        .iter()
        .filter(|v| {
            let made = forcing(&v.symbol);
            lazy.defs
                .iter()
                .position(|d| d.symbol == made)
                .is_some_and(|i| !pure.contains(&i))
        })
        .map(|v| (v.symbol.clone(), v.rep.clone()))
        .collect();
    if performing.is_empty() {
        return;
    }
    let mut body = Statement::Call(
        entry.clone(),
        Vec::new(),
        vec![Consumer::Var("#k".to_string())],
    );
    for (symbol, rep) in performing.into_iter().rev() {
        let made = Binder {
            name: "#made".to_string(),
            rep,
        };
        body = Statement::Call(
            forcing(&symbol),
            Vec::new(),
            vec![Consumer::MuTilde(made, Box::new(body))],
        );
    }
    let symbol = entry.child("#values");
    lazy.defs.push(Def {
        symbol: symbol.clone(),
        rep_vars: Vec::new(),
        effect_vars: Vec::new(),
        params: Vec::new(),
        conts: vec!["#k".to_string()],
        body,
    });
    lazy.entry = Some(symbol);
}

/// `s` with `halt` the continuation `#halt`.
fn rehalted(s: &Statement) -> Statement {
    fn c(x: &Consumer) -> Consumer {
        match x {
            Consumer::Halt => Consumer::Var("#halt".to_string()),
            Consumer::Var(_) => x.clone(),
            Consumer::MuTilde(b, s) => Consumer::MuTilde(b.clone(), Box::new(rehalted(s))),
            Consumer::Case(arms) => Consumer::Case(
                arms.iter()
                    .map(|a| Arm {
                        pattern: a.pattern.clone(),
                        fields: a.fields.clone(),
                        body: rehalted(&a.body),
                    })
                    .collect(),
            ),
            Consumer::Method(m, ps, cs) => Consumer::Method(
                m.clone(),
                ps.iter().map(p).collect(),
                cs.iter().map(c).collect(),
            ),
        }
    }
    fn p(x: &Producer) -> Producer {
        match x {
            Producer::Con(k, xs) => Producer::Con(k.clone(), xs.iter().map(p).collect()),
            Producer::Tuple(xs) => Producer::Tuple(xs.iter().map(p).collect()),
            Producer::Array(xs) => Producer::Array(xs.iter().map(p).collect()),
            Producer::Record(fs) => {
                Producer::Record(fs.iter().map(|(l, x)| (l.clone(), p(x))).collect())
            }
            Producer::Mu(k, s) => Producer::Mu(k.clone(), Box::new(rehalted(s))),
            Producer::Cocase(ms) => Producer::Cocase(
                ms.iter()
                    .map(|m| Method {
                        name: m.name.clone(),
                        params: m.params.clone(),
                        conts: m.conts.clone(),
                        body: rehalted(&m.body),
                    })
                    .collect(),
            ),
            _ => x.clone(),
        }
    }
    match s {
        Statement::Cut(x, k) => Statement::Cut(p(x), c(k)),
        Statement::Call(f, xs, ks) => Statement::Call(
            f.clone(),
            xs.iter().map(p).collect(),
            ks.iter().map(c).collect(),
        ),
        Statement::Prim(op, xs, ks) => Statement::Prim(
            op.clone(),
            xs.iter().map(p).collect(),
            ks.iter().map(c).collect(),
        ),
        Statement::Let(b, x, rest) => Statement::Let(b.clone(), p(x), Box::new(rehalted(rest))),
        Statement::Handle(h) => Statement::Handle(Box::new(Handle {
            clauses: h
                .clauses
                .iter()
                .map(|cl| Clause {
                    op: cl.op.clone(),
                    params: cl.params.clone(),
                    resumption: cl.resumption.clone(),
                    cont: cl.cont.clone(),
                    body: rehalted(&cl.body),
                })
                .collect(),
            ret: (h.ret.0.clone(), h.ret.1.clone(), rehalted(&h.ret.2)),
            body_cont: h.body_cont.clone(),
            body: rehalted(&h.body),
            cont: c(&h.cont),
        })),
        Statement::Perform(op, xs, k) => {
            Statement::Perform(op.clone(), xs.iter().map(p).collect(), c(k))
        }
        Statement::Error(_) => s.clone(),
    }
}
