//! **Macro expansion**: `name!(…)`.
//!
//! A macro call is parsed as a name and an opaque argument -- balanced brackets
//! and nothing more -- so this is where it first means anything. Expansion runs
//! after [`crate::cfg`] and before name resolution, and every call is gone by
//! the time it finishes: replaced by what the macro produced, or reported and
//! replaced by something harmless so the rest of the module still compiles.
//!
//! What a macro produces is **tokens**, which are then parsed with the entry
//! point for the position the call was in ([`meadow_parser::parse_expr`] and
//! friends). Nothing here builds a tree directly. That is what keeps a macro
//! unable to say anything the language could not: if it does not parse, it is
//! an error, exactly as it would be written out by hand.
//!
//! A module's `macro` declarations are read first and are gone before anything
//! else looks: a macro is not a value, so there is nowhere past here to put
//! one. Reading them all up front is also what lets a macro be called above
//! where it is written, as every other top-level name can be.
//!
//! These built-ins are always available, and a `macro` may not take one of
//! their names:
//!
//! | call | answers |
//! |---|---|
//! | `line!()` | the line the call is written on |
//! | `file!()` | the name of the file it is written in |
//! | `stringify!(…)` | its argument, written back as text |
//! | `concat!(a, b, …)` | its literal arguments, joined into one string |
//! | `quote! { … $x … }` | an expression building those tokens, `$x` spliced (see [`quote`]) |

/// The first thing a parser said about a macro's output, and where. A macro
/// rather than a function: the parser's error type is not one this crate can
/// name.
macro_rules! first_error {
    ($errs:expr) => {
        $errs.first().map(|e| {
            let d = meadow_diagnostics::from_parse_error("", e);
            (d.msg, d.label.1)
        })
    };
}

pub mod datum;
mod derive;
mod hygiene;
pub mod proc;
mod quote;
pub mod record;
mod rules;

pub use datum::{Binding, Datum};

use meadow_ast as ast;
use meadow_diagnostics::Diagnostic;
use meadow_intern::InternedString;
use meadow_lexer::{LToken, Token, tt};
use meadow_span::{Located, Span};
use rules::Matcher;
use std::collections::HashMap;

/// How many times a macro may expand into another before we call it a loop.
/// Rust's default is 128; a macro that needs more is almost always recursive
/// without a base case, and the limit is what turns a hang into an error.
const MAX_DEPTH: usize = 128;

/// The macros a built-in name takes, which a `macro` may not.
const BUILT_IN: &[&str] = &["line", "file", "stringify", "concat", "quote"];

/// A macro as it is stored and shared: its rules as token trees, unread.
///
/// A dependent expands a macro without re-parsing the package it came from, so
/// what crosses a package boundary is the tokens the rules were written with.
/// Reading them into matchers is cheap and happens wherever they are used.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rules {
    pub name: InternedString,
    /// The module it is written in, as a path within its package.
    pub module: Vec<InternedString>,
    /// The package it is written in: what `$pkg` stands for, and what a
    /// dependent names it by.
    pub package: InternedString,
    pub vis: Vis,
    /// `(matcher, template)` for each rule, in the order they are tried.
    pub rules: Vec<(tt::Group, tt::Group)>,
}

/// How far a macro can be seen. The same four a declaration has, read from the
/// same `@pub` attribute -- a macro is a declaration like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Vis {
    /// Its own module, and nowhere else.
    Private,
    /// The module that holds the one it is in, and below.
    Super,
    /// Anywhere in its package.
    Package,
    /// Anywhere at all.
    Exported,
}

impl Rules {
    /// Whether a module at `path` in package `pkg` may name this.
    fn visible_to(&self, pkg: InternedString, path: &[InternedString]) -> bool {
        visible(self.vis, self.package, &self.module, pkg, path)
    }
}

/// Whether something `vis` in `module` of `package` may be named by a module
/// at `path` in package `pkg`: the rule a macro and a compile-time binding
/// share with every other declaration.
fn visible(
    vis: Vis,
    package: InternedString,
    module: &[InternedString],
    pkg: InternedString,
    path: &[InternedString],
) -> bool {
    match vis {
        Vis::Exported => true,
        Vis::Package => pkg == package,
        Vis::Super => pkg == package && path.starts_with(&module[..module.len().saturating_sub(1)]),
        Vis::Private => pkg == package && path == module,
    }
}

/// One expansion that happened: what a diagnostic landing in it came from.
///
/// Every token a template produced carries the call's span, so a later pass
/// reports at the call -- which is the only place it *can* report, the
/// expansion not being in the file. What that leaves out is which macro wrote
/// it, and this is what says so.
#[derive(Debug, Clone)]
pub struct Expansion {
    /// The span the produced tokens carry: the call's argument.
    pub at: Span,
    /// The module the call is in.
    pub filename: String,
    /// The macro, as the call names it.
    pub name: String,
}

/// Say which macro wrote the code a diagnostic is about.
///
/// A diagnostic whose label is *exactly* an expansion's span is about what the
/// template produced: an argument keeps its own, narrower span, so an error in
/// one is still reported as the caller's own and is left alone here.
pub fn blame(diags: &mut [Diagnostic], expansions: &[Expansion]) {
    for d in diags {
        let mut from: Vec<&Expansion> = expansions
            .iter()
            .filter(|e| e.at == d.label.1 && e.filename == d.filename)
            .collect();
        // Innermost first: `outer!` expanding to `inner!` blames `inner!`, and
        // then says where that came from.
        from.reverse();
        for e in from.iter().take(3) {
            d.extra_labels
                .push((format!("`{}!` wrote this", e.name), e.at));
        }
    }
}

/// A macro definition, with its matchers read and checked.
struct Macro {
    /// The package that wrote it, which is what `$pkg` stands for in its
    /// template -- the point of `$pkg` being that it means the same thing
    /// wherever the macro is expanded.
    package: InternedString,
    rules: Vec<(Matcher, tt::Group)>,
}

/// Expand every macro call in `module`.
///
/// Every module of the unit at once, because a macro may be used in a module
/// other than the one that wrote it: the definitions are read from all of them
/// first, and only then is anything expanded.
///
/// Expansion runs in rounds, because a procedural macro may `lookup` a
/// compile-time binding that another call has not defined yet. Such a call is
/// set aside where it stands and tried again in the next round, after every
/// call that could run has. Rounds go on while they define something; when one
/// defines nothing and calls are still waiting, the next is *settled* -- a
/// lookup of a name nothing defined is answered `None` rather than waited on,
/// so every call finishes.
///
/// `deps` are the packages this one depends on, each with the macros it
/// exports, under the name this unit calls it by. What comes back is the
/// unit's own macros and compile-time bindings, for its
/// [`CompiledPackage`](crate::CompiledPackage) to carry, and a record of every
/// expansion, for [`blame`].
pub fn expand_unit(
    package: InternedString,
    modules: &mut Vec<crate::AstModule>,
    deps: &[crate::Dep<'_>],
    procs: Option<&dyn proc::Runner>,
    filename: &str,
    diags: &mut Vec<Diagnostic>,
) -> (Vec<Rules>, Vec<Binding>, Vec<Expansion>) {
    // Visibility works as it does for everything else: a unit that never says
    // `@pub` exports all of it, and one that says it anywhere means it
    // everywhere.
    let gated = modules.iter().any(|m| says_pub(&m.ast.value));
    // Read off the source rather than from anything resolved: this runs before
    // resolution, and all it is for is a better error.
    let own_macros: Vec<InternedString> = modules
        .iter()
        .flat_map(|m| m.ast.value.decls.iter())
        .filter_map(marked_macro)
        .collect();
    let mut mine: Vec<Rules> = Vec::new();
    let mut from: Vec<Expansion> = Vec::new();
    // The values written out by hand, `@compileTime def`, which are there
    // before any macro runs.
    let mut bindings: Vec<Binding> = Vec::new();
    for m in modules.iter_mut() {
        let here = crate::unit::module_filename(filename, m.source);
        let text = m.source.content.to_string();
        let mut ex = Expander::new(
            &text,
            &here,
            diags,
            package,
            procs,
            own_macros.clone(),
            &m.path,
            gated,
            &[],
        );
        ex.collect(&mut m.ast.value.decls, &m.path, gated, &mut mine);
        ex.compile_time(&mut m.ast.value.decls, &mut bindings);
    }
    // A mark per module that lasts across rounds: two expansions in one module
    // must never share one, whichever round each happened in.
    let mut marks = vec![0u32; modules.len()];
    let mut settled = false;
    for _ in 0..MAX_ROUNDS {
        let before = bindings.len();
        let mut waiting = 0;
        let mut made = Vec::new();
        for (i, m) in modules.iter_mut().enumerate() {
            let here = crate::unit::module_filename(filename, m.source);
            let text = m.source.content.to_string();
            let mut ex = Expander::new(
                &text,
                &here,
                diags,
                package,
                procs,
                own_macros.clone(),
                &m.path,
                gated,
                &bindings,
            );
            ex.expansions = marks[i];
            ex.settled = settled;
            ex.import(&m.ast.value, &m.path, &mine, deps, false);
            ex.decls(&mut m.ast.value.decls);
            marks[i] = ex.expansions;
            waiting += ex.waiting;
            from.extend(ex.from);
            made.extend(ex.made);
        }
        bindings.extend(made);
        // A `mod Foo { … }` a macro produced is a module now, and what it
        // holds is expanded in the next round. Its marks start clear of the
        // ones of the module it was written in, whose file it shares.
        let parents = crate::unit::lift_inline_modules(modules);
        for (k, parent) in parents.iter().enumerate() {
            marks.push(marks[*parent].wrapping_add(1_000_000 * (k as u32 + 1)));
        }
        if waiting == 0 && parents.is_empty() {
            break;
        }
        // Nothing new, and something still waiting: whatever it waits for is
        // not coming, so the next round answers it `None`.
        if bindings.len() == before {
            settled = true;
        }
    }
    // What is wrong with a `use` is said once, now that nothing more will be
    // defined: a binding it names that is missing now is missing for good.
    for m in modules.iter() {
        let here = crate::unit::module_filename(filename, m.source);
        let text = m.source.content.to_string();
        let mut ex = Expander::new(
            &text,
            &here,
            diags,
            package,
            procs,
            own_macros.clone(),
            &m.path,
            gated,
            &bindings,
        );
        ex.import(&m.ast.value, &m.path, &mine, deps, true);
    }
    (mine, bindings, from)
}

/// How many rounds expansion may take. Every round but a settled one defines
/// something, and a call that defines runs once, so this is never reached by a
/// unit that can finish; it is a guard, not a limit anyone should meet.
const MAX_ROUNDS: usize = 1024;

/// Whether anything in `module` carries a `@pub`.
fn says_pub(module: &ast::Module) -> bool {
    module.decls.iter().any(|d| match &*d.value {
        ast::Decl::Attributed(attrs, _) => attrs.iter().any(|a| &**a.name.value() == "pub"),
        _ => false,
    })
}

struct Expander<'a> {
    text: &'a str,
    filename: &'a str,
    diags: &'a mut Vec<Diagnostic>,
    depth: usize,
    /// The depth limit was reached: every call still to expand fails at once,
    /// so a macro that expands to two calls of itself stops after one report
    /// rather than after trying every branch to the limit.
    overflowed: bool,
    /// The package being compiled, for `$pkg` in a macro of its own.
    package: InternedString,
    /// What a call may name here, by the name it is called by: `vec` for one
    /// this module can see plainly, `V.vec` for one reached through a `use …
    /// as V`.
    macros: HashMap<String, Macro>,
    /// How many expansions have happened, which is where a mark comes from:
    /// two expansions of the same macro must not share one.
    expansions: u32,
    /// What each of them was, for the diagnostics that land in one.
    from: Vec<Expansion>,
    /// What can run a procedural macro, when anything can.
    procs: Option<&'a dyn proc::Runner>,
    /// The procedural macros in scope: the package that exports each, and the
    /// name it is exported under.
    proc_macros: HashMap<String, (InternedString, InternedString)>,
    /// What this unit marks `@macro` of its own -- which cannot be run here,
    /// but is worth recognising to say why.
    own_macros: Vec<InternedString>,
    /// The module being expanded, as a path within its package.
    path: Vec<InternedString>,
    /// Whether the unit says `@pub` anywhere, which decides what a binding
    /// with no `@pub` of its own may be seen by.
    gated: bool,
    /// Every compile-time binding this unit had when the round began.
    known: &'a [Binding],
    /// The bindings this module's calls defined in this round.
    made: Vec<Binding>,
    /// What a macro called here can `lookup`, by the name this module knows
    /// each by.
    scope: Vec<(String, Datum)>,
    /// Whether nothing more can be defined, so a lookup of an unknown name is
    /// answered rather than waited on.
    settled: bool,
    /// How many calls were set aside in this round, waiting for a name.
    waiting: usize,
    /// Whether the call just run was set aside -- and so is to be left where
    /// it is, not replaced by anything.
    waited: bool,
    /// How far what the call being expanded defines may be seen: what the
    /// `@pub` written on it says.
    call_vis: Option<Vis>,
}

impl<'a> Expander<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        text: &'a str,
        filename: &'a str,
        diags: &'a mut Vec<Diagnostic>,
        package: InternedString,
        procs: Option<&'a dyn proc::Runner>,
        own_macros: Vec<InternedString>,
        path: &[InternedString],
        gated: bool,
        known: &'a [Binding],
    ) -> Expander<'a> {
        Expander {
            text,
            filename,
            diags,
            depth: 0,
            overflowed: false,
            macros: HashMap::new(),
            expansions: 0,
            from: Vec::new(),
            package,
            procs,
            proc_macros: HashMap::new(),
            own_macros,
            path: path.to_vec(),
            gated,
            known,
            made: Vec::new(),
            scope: Vec::new(),
            settled: false,
            waiting: 0,
            waited: false,
            call_vis: None,
        }
    }

    /// Whether the call just run was set aside, clearing the flag.
    fn take_waited(&mut self) -> bool {
        std::mem::take(&mut self.waited)
    }

    // --- compile-time bindings ------------------------------------------------

    /// Take every `@compileTime def` out of `decls` and add what it stands for
    /// to `into`.
    ///
    /// Its right-hand side is data, read and never evaluated (see
    /// [`datum::of_expr`]): the compiler runs nothing of the package it is
    /// compiling, which is why a macro has to live in a dependency.
    fn compile_time(&mut self, decls: &mut Vec<ast::LDecl>, into: &mut Vec<Binding>) {
        let mut kept = Vec::with_capacity(decls.len());
        for d in std::mem::take(decls) {
            let ast::Decl::Attributed(attrs, inner) = &*d.value else {
                kept.push(d);
                continue;
            };
            if !attrs.iter().any(|a| &**a.name.value() == "compileTime") {
                kept.push(d);
                continue;
            }
            let named = match &*inner.value {
                ast::Decl::Bind(ast::Bind::Pat(p, e)) => {
                    let mut p = p;
                    while let ast::Pat::Ann(under, _) = &*p.value {
                        p = under;
                    }
                    match &*p.value {
                        ast::Pat::Var(n) => Some((n.clone(), e)),
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some((name, value)) = named else {
                self.error(
                    "`@compileTime` is for a `def` of one name".to_string(),
                    "this is not one".to_string(),
                    inner.span,
                    vec![],
                );
                continue;
            };
            match datum::of_expr(value) {
                Ok(v) => {
                    let taken = into
                        .iter()
                        .any(|b| b.name == *name.value() && b.module == self.path);
                    if taken {
                        self.error(
                            format!("`{}` is defined twice at compile time", name.value()),
                            "this module already has one with this name".to_string(),
                            name.span,
                            vec![],
                        );
                        continue;
                    }
                    into.push(Binding {
                        name: *name.value(),
                        module: self.path.clone(),
                        package: self.package,
                        vis: vis_of(attrs, self.gated),
                        value: v,
                    });
                }
                Err((msg, span)) => self.error(
                    msg,
                    "a compile-time value is read, not run".to_string(),
                    span,
                    vec![],
                ),
            }
        }
        *decls = kept;
    }

    /// Record what a call defined, where it was called.
    fn defined(&mut self, defs: Vec<(String, Datum)>, at: Span) {
        let vis = self.call_vis.unwrap_or_else(|| vis_of(&[], self.gated));
        for (name, value) in defs {
            let name = InternedString::from(name.as_str());
            let taken = self
                .known
                .iter()
                .chain(self.made.iter())
                .any(|b| b.name == name && b.package == self.package && b.module == self.path);
            if taken {
                self.error(
                    format!("`{name}` is defined twice at compile time"),
                    "this call defines it again in the same module".to_string(),
                    at,
                    vec![],
                );
                continue;
            }
            // Seen by what is expanded after it in this module at once, and by
            // every other module from the next round.
            self.scope.push((name.to_string(), value.clone()));
            self.made.push(Binding {
                name,
                module: self.path.clone(),
                package: self.package,
                vis,
                value,
            });
        }
    }

    /// The compile-time bindings of the module a `use` path names, wherever
    /// it lives -- the same places [`Self::module_at`] looks for macros.
    fn bindings_at(&self, segs: &[InternedString], deps: &[crate::Dep<'_>]) -> Vec<Binding> {
        if segs.is_empty() {
            return Vec::new();
        }
        let local = if segs[0] == self.package {
            &segs[1..]
        } else {
            segs
        };
        let mut out: Vec<Binding> = self
            .known
            .iter()
            .filter(|b| b.module == local)
            .cloned()
            .collect();
        for d in deps {
            let name = d.spelled.to_string();
            if dotted(local) == name || dotted(segs) == name {
                out.extend(d.bindings.iter().cloned());
            } else if segs[0] == d.spelled {
                out.extend(d.bindings.iter().filter(|b| b.module == segs[1..]).cloned());
            }
        }
        out
    }
    // --- reading the definitions --------------------------------------------

    /// Take every `macro` declaration out of `decls`, check its rules, and add
    /// it to the unit's.
    fn collect(
        &mut self,
        decls: &mut Vec<ast::LDecl>,
        path: &[InternedString],
        gated: bool,
        into: &mut Vec<Rules>,
    ) {
        let mut kept = Vec::with_capacity(decls.len());
        for d in std::mem::take(decls) {
            // A macro carries attributes like anything else: `@cfg` has already
            // had its say, and `@pub` is read here.
            let (attrs, def) = match &*d.value {
                ast::Decl::Macro(m) => (&[][..], Some(m)),
                ast::Decl::Attributed(attrs, inner) => match &*inner.value {
                    ast::Decl::Macro(m) => (&attrs[..], Some(m)),
                    _ => (&attrs[..], None),
                },
                _ => (&[][..], None),
            };
            let Some(def) = def else {
                kept.push(d);
                continue;
            };
            self.define(def, d.span, path, vis_of(attrs, gated), into);
        }
        *decls = kept;
    }

    /// Read one definition, reporting a rule that could not work where it is
    /// written rather than at every call.
    fn define(
        &mut self,
        def: &ast::MacroDef,
        span: Span,
        path: &[InternedString],
        vis: Vis,
        into: &mut Vec<Rules>,
    ) {
        let name = *def.name.value();
        if BUILT_IN.contains(&&*name.to_string()) {
            self.error(
                format!("`{name}!` is a built-in macro"),
                "this name is taken".to_string(),
                def.name.span,
                vec![],
            );
            return;
        }
        if into
            .iter()
            .any(|r| r.name == name && r.module == path && r.package == self.package)
        {
            self.error(
                format!("the macro `{name}!` is defined twice"),
                "this module already has one with this name".to_string(),
                def.name.span,
                vec![],
            );
            return;
        }
        // Read once here so that a matcher that could never work is reported
        // where it was written. What is stored is the tokens, which is what a
        // dependent gets.
        for rule in &def.rules {
            if let Err(bad) = Matcher::read(&rule.matcher.trees) {
                self.invalid(bad, span);
                return;
            }
        }
        into.push(Rules {
            name,
            module: path.to_vec(),
            package: self.package,
            vis,
            rules: def
                .rules
                .iter()
                .map(|r| (r.matcher.clone(), r.template.clone()))
                .collect(),
        });
    }

    /// Work out what this module may call, by the name it calls it by.
    ///
    /// Its own macros are there without asking; everything else arrives through
    /// a `use`, exactly as a value does -- a bare `use M` brings what `M` can
    /// show it, `use M (vec!)` brings that one, and `use M as V` puts them
    /// behind `V.`.
    ///
    /// Run every round, since a round may define bindings a `use` names; so
    /// what is wrong with a `use` is only said when `report` is, which is once,
    /// after the last round -- when a name still missing is missing for good.
    fn import(
        &mut self,
        module: &ast::Module,
        path: &[InternedString],
        mine: &[Rules],
        deps: &[crate::Dep<'_>],
        report: bool,
    ) {
        let said = self.diags.len();
        self.import_all(module, path, mine, deps);
        if !report {
            self.diags.truncate(said);
        }
    }

    fn import_all(
        &mut self,
        module: &ast::Module,
        path: &[InternedString],
        mine: &[Rules],
        deps: &[crate::Dep<'_>],
    ) {
        for r in mine {
            if r.package == self.package && r.module == path {
                self.take(r, r.name.to_string());
            }
        }
        // A module's own compile-time bindings are there without asking, as
        // its macros are.
        for b in self.known {
            if b.package == self.package && b.module == path {
                self.scope.push((b.name.to_string(), b.value.clone()));
            }
        }
        for d in &module.decls {
            let Some(u) = peel_use(d) else { continue };
            let segs: Vec<InternedString> = u.path.iter().map(|s| *s.value()).collect();
            // Compile-time bindings share the macro namespace, and arrive the
            // way a macro does: all of them with a bare `use M`, one with
            // `use M (name!)`, behind `A.` with `use M as A`.
            let bound: Vec<Binding> = self
                .bindings_at(&segs, deps)
                .into_iter()
                .filter(|b| b.visible_to(self.package, path))
                .collect();
            let called = |name: &str| match &u.alias {
                Some(a) => format!("{}.{name}", a.value()),
                None => name.to_string(),
            };
            if u.macros.is_empty() && u.names.is_empty() && !u.glob {
                for b in &bound {
                    self.scope.push((called(&b.name), b.value.clone()));
                }
            }
            let there = self.module_at(&segs, mine, deps);
            let reachable: Vec<&Rules> = there
                .into_iter()
                .filter(|r| r.visible_to(self.package, path))
                .collect();
            let under = |r: &Rules| match &u.alias {
                Some(a) => format!("{}.{}", a.value(), r.name),
                None => r.name.to_string(),
            };
            // A function whose type is a macro's is one too: that is all a
            // procedural macro is.
            let written: Vec<(InternedString, InternedString, &meadow_infer::Scheme)> =
                self.procs_at(&segs, deps);
            if u.macros.is_empty() {
                // A bare `use M` brings every name; one that selects values
                // says nothing about macros.
                if u.names.is_empty() && !u.glob {
                    for r in reachable {
                        self.take(r, under(r));
                    }
                    for (pkg, name, scheme) in &written {
                        if proc::signature(scheme).is_ok() {
                            let called = match &u.alias {
                                Some(a) => format!("{}.{}", a.value(), name),
                                None => name.to_string(),
                            };
                            self.proc_macros.insert(called, (*pkg, *name));
                        }
                    }
                }
                continue;
            }
            for want in &u.macros {
                let name = *want.value();
                if let Some(r) = reachable.iter().find(|r| r.name == name) {
                    self.take(r, under(r));
                    continue;
                }
                if let Some(b) = bound.iter().find(|b| b.name == name) {
                    self.scope.push((called(&b.name), b.value.clone()));
                    continue;
                }
                if let Some((pkg, _, scheme)) = written.iter().find(|(_, n, _)| *n == name) {
                    match proc::signature(scheme) {
                        Ok(()) => {
                            let called = match &u.alias {
                                Some(a) => format!("{}.{name}", a.value()),
                                None => name.to_string(),
                            };
                            self.proc_macros.insert(called, (*pkg, name));
                        }
                        Err(why) => self.error(
                            format!("`{name}` cannot be a macro: {why}"),
                            "a macro is `[TokenTree] -> [TokenTree]`".to_string(),
                            want.span,
                            vec![],
                        ),
                    }
                    continue;
                }
                // A function of that name, but not marked: that is the
                // likely mistake, and it is fixed where the function is.
                if self.exported_plainly(&segs, name, deps) {
                    self.error(
                        format!("`{name}` is not a macro"),
                        format!(
                            "mark it `@macro` where it is defined, in `{}`",
                            dotted(&segs)
                        ),
                        want.span,
                        vec![],
                    );
                    continue;
                }
                // One in this very unit: a macro has to be compiled before it
                // can run, so it cannot be one of its own package's.
                if self.own_macros.contains(&name) {
                    self.error(
                        format!("`{name}` is a macro of this package"),
                        "a macro has to be compiled before it can run, so it belongs to a \
                         package the one using it depends on"
                            .to_string(),
                        want.span,
                        vec![],
                    );
                    continue;
                }
                self.error(
                    format!(
                        "`{}` does not export a macro `{}!`",
                        dotted(&segs),
                        want.value()
                    ),
                    "no such macro".to_string(),
                    want.span,
                    vec![],
                );
            }
        }
    }

    /// The macros of the module a `use` path names, wherever it lives.
    fn module_at<'r>(
        &self,
        segs: &[InternedString],
        mine: &'r [Rules],
        deps: &'r [crate::Dep<'_>],
    ) -> Vec<&'r Rules> {
        if segs.is_empty() {
            return Vec::new();
        }
        // `use Pkg.a.b` inside `Pkg` is the local module `a.b`, and so is a
        // bare `use a.b` -- the same rule module resolution itself follows.
        let local = if segs[0] == self.package {
            &segs[1..]
        } else {
            segs
        };
        let mut out: Vec<&Rules> = mine.iter().filter(|r| r.module == local).collect();
        for d in deps {
            let name = d.spelled.to_string();
            if dotted(local) == name || dotted(segs) == name {
                // A module of this package compiled as a unit of its own, named
                // by its path rather than by the package -- how `Std` is built.
                out.extend(d.macros.iter());
            } else if segs[0] == d.spelled {
                out.extend(d.macros.iter().filter(|r| r.module == segs[1..]));
            }
        }
        out
    }

    /// The functions exported by the module a `use` path names, with the
    /// package each belongs to: the candidates for a procedural macro.
    fn procs_at<'d>(
        &self,
        segs: &[InternedString],
        deps: &'d [crate::Dep<'_>],
    ) -> Vec<(InternedString, InternedString, &'d meadow_infer::Scheme)> {
        let local = if segs[0] == self.package {
            &segs[1..]
        } else {
            segs
        };
        let mut out = Vec::new();
        for d in deps {
            let name = d.spelled.to_string();
            // A sub-unit of this package is named by its module path; anything
            // else by the package, with the module after it.
            let whole = dotted(local) == name || dotted(segs) == name;
            for e in &d.exports {
                if e.is_macro && (whole || (segs[0] == d.spelled && e.module == segs[1..])) {
                    out.push((d.name, e.name, &e.scheme));
                }
            }
        }
        out
    }

    /// Whether the module a `use` path names exports `want` as an ordinary
    /// function -- which is what a `use … (name!)` that found nothing was
    /// probably reaching for.
    fn exported_plainly(
        &self,
        segs: &[InternedString],
        want: InternedString,
        deps: &[crate::Dep<'_>],
    ) -> bool {
        let local = if segs[0] == self.package {
            &segs[1..]
        } else {
            segs
        };
        deps.iter().any(|d| {
            let name = d.spelled.to_string();
            let whole = dotted(local) == name || dotted(segs) == name;
            d.exports.iter().any(|e| {
                e.name == want && (whole || (segs[0] == d.spelled && e.module == segs[1..]))
            })
        })
    }

    /// Put a macro in the table under the name a call would write.
    fn take(&mut self, r: &Rules, called: String) {
        let mut rules = Vec::with_capacity(r.rules.len());
        for (matcher, template) in &r.rules {
            // A macro that is here at all was checked where it was written.
            let Ok(m) = Matcher::read(&matcher.trees) else {
                return;
            };
            rules.push((m, template.clone()));
        }
        self.macros.insert(
            called,
            Macro {
                package: r.package,
                rules,
            },
        );
    }

    /// Report something wrong with how a macro is written.
    fn invalid(&mut self, bad: rules::Invalid, fallback: Span) {
        let span = if bad.span == Span::default() {
            fallback
        } else {
            bad.span
        };
        self.error(bad.msg, bad.label, span, vec![]);
    }

    // --- the built-ins ------------------------------------------------------

    /// Run `call`, answering the tokens it produced, or say why it cannot be
    /// run. `None` means the call is gone and an error has been reported.
    ///
    /// Every built-in answers a single literal, so they all take the call's own
    /// span: what they produced is not written anywhere, and the call is the
    /// nearest thing in the file to point at.
    fn run(&mut self, call: &ast::MacCall) -> Option<Vec<LToken>> {
        let out = self.run_call(call)?;
        if record::producing() {
            record::produced(
                self.filename,
                call.name().as_str(),
                Span::new(call.path_span().start, call.arg.span().end),
                call.arg.span(),
                tt::flatten(&call.arg.trees).len(),
                &out,
            );
        }
        Some(out)
    }

    /// [`Expander::run`], before what it produced is written down.
    fn run_call(&mut self, call: &ast::MacCall) -> Option<Vec<LToken>> {
        let span = call.arg.span();
        let one = |t: Token| Some(vec![LToken::new(t, span)]);
        // A qualified name waits for macros that can be exported, which is what
        // there would be to qualify.
        match call.name().as_str() {
            "line" => {
                self.no_argument(call)?;
                let line = line_of(self.text, call.path_span().start);
                one(Token::Int(line as i64))
            }
            "file" => {
                self.no_argument(call)?;
                one(Token::String(InternedString::from(self.filename)))
            }
            "stringify" => one(Token::String(InternedString::from(tt::render(
                &call.arg.trees,
            )))),
            "concat" => {
                let text = self.concat(call)?;
                one(Token::String(InternedString::from(text)))
            }
            "quote" => match quote::expression(&call.arg.trees) {
                Ok(text) => {
                    let lexed = meadow_lexer::tokenize(meadow_source::Source::new(
                        meadow_source::SourceKind::Interactive,
                        InternedString::from(text),
                    ));
                    // What the quote wrote stands where the quote is.
                    Some(
                        lexed
                            .tokens
                            .iter()
                            .map(|t| LToken::new(t.value().clone(), span))
                            .collect(),
                    )
                }
                Err((msg, at)) => {
                    self.error(msg, "in this quote".to_string(), at, vec![]);
                    None
                }
            },
            _ => match self.proc_macros.get(&call.name()).copied() {
                Some((pkg, name)) => self.run_proc(call, pkg, name),
                None => self.run_rules(call),
            },
        }
    }

    /// Run a procedural macro: a function, compiled, called with the tokens of
    /// this call and answering with the tokens that replace it.
    fn run_proc(
        &mut self,
        call: &ast::MacCall,
        pkg: InternedString,
        name: InternedString,
    ) -> Option<Vec<LToken>> {
        let at = call.arg.span();
        let Some(runner) = self.procs else {
            self.error(
                format!(
                    "`{}!` is a procedural macro, which cannot run here",
                    call.name()
                ),
                "running one means building the package that defines it".to_string(),
                call.path_span(),
                vec![],
            );
            return None;
        };
        let scope = proc::Scope {
            visible: &self.scope,
            settled: self.settled,
        };
        let outcome = runner.run(pkg, name, &call.arg.trees, at, &scope);
        if let Ok(proc::Outcome::Waiting(_)) = outcome {
            // Set aside, to be run again once something is defined: nothing
            // happened, so there is nothing to record.
            self.waiting += 1;
            self.waited = true;
            return None;
        }
        self.from.push(Expansion {
            at,
            filename: self.filename.to_string(),
            name: call.name(),
        });
        match outcome {
            Ok(proc::Outcome::Answered { trees, defined, .. }) => {
                self.defined(defined, at);
                Some(tt::flatten(&trees))
            }
            Ok(proc::Outcome::Waiting(_)) => unreachable!("handled above"),
            Err(why) => {
                self.failed(
                    &format!("`{}!`", call.name()),
                    why,
                    at,
                    "this call is what it was given",
                );
                None
            }
        }
    }

    /// Run a `macro`: the first rule whose matcher fits the call's argument,
    /// with its template written out.
    fn run_rules(&mut self, call: &ast::MacCall) -> Option<Vec<LToken>> {
        let name = call.name();
        let Some(mac) = self.macros.get(&name) else {
            let known = self.known();
            self.error(
                format!("there is no macro `{}!`", call.name()),
                "unknown macro".to_string(),
                call.path_span(),
                vec![(known, call.path_span())],
            );
            return None;
        };
        // Each expansion gets a mark of its own, so that two of the same macro
        // do not share the locals their templates introduce.
        self.expansions += 1;
        let id = self.expansions;
        let at = call.arg.span();
        self.from.push(Expansion {
            at,
            filename: self.filename.to_string(),
            name: name.clone(),
        });

        // Cloned out of the table: writing the template is `&mut self` work,
        // since a failure in it is reported.
        let rules: Vec<_> = mac.rules.iter().map(|(_, t)| t.clone()).collect();
        let package = mac.package;
        let matched = mac
            .rules
            .iter()
            .position(|(m, _)| m.match_trees(&call.arg.trees).is_some());
        let bound = matched.and_then(|i| mac.rules[i].0.match_trees(&call.arg.trees));

        let (Some(i), Some(bound)) = (matched, bound) else {
            self.error(
                format!("no rule of `{}!` matches this call", call.name()),
                "this argument fits none of them".to_string(),
                at,
                vec![],
            );
            return None;
        };
        match rules::substitute(&rules[i].trees, &bound, at, package, &|t| {
            hygiene::mark(t, id)
        }) {
            Ok(tokens) => Some(tokens),
            Err(bad) => {
                self.invalid(bad, at);
                None
            }
        }
    }

    /// What to suggest when a macro is not found.
    fn known(&self) -> String {
        let mut names: Vec<String> = self
            .macros
            .keys()
            .chain(self.proc_macros.keys())
            .map(|called| format!("`{called}!`"))
            .collect();
        names.sort();
        for b in BUILT_IN {
            names.push(format!("`{b}!`"));
        }
        format!("the macros in scope are {}", names.join(", "))
    }

    /// Check that `call` was given nothing, for the macros that take nothing.
    fn no_argument(&mut self, call: &ast::MacCall) -> Option<()> {
        if call.arg.trees.is_empty() {
            return Some(());
        }
        self.error(
            format!("`{}!` takes no arguments", call.name()),
            "this is not read".to_string(),
            call.arg.span(),
            vec![],
        );
        None
    }

    /// `concat!(a, b, c)`: the text of each literal argument, joined.
    fn concat(&mut self, call: &ast::MacCall) -> Option<String> {
        let mut out = String::new();
        let mut bad = false;
        for piece in split(&call.arg.trees, Token::Comma) {
            // A trailing comma leaves an empty piece, which is no argument at
            // all rather than a bad one.
            if piece.is_empty() {
                continue;
            }
            match piece {
                [tt::TokenTree::Token(t)] if let Some(text) = literal_text(t.value()) => {
                    out.push_str(&text)
                }
                _ => {
                    let span = piece
                        .first()
                        .expect("a piece that is not empty")
                        .span()
                        .extend(piece.last().expect("the same piece").span());
                    self.error(
                        "`concat!` takes literals".to_string(),
                        "this is not a literal".to_string(),
                        span,
                        vec![],
                    );
                    bad = true;
                }
            }
        }
        if bad { None } else { Some(out) }
    }

    // --- expanding in each position -----------------------------------------

    /// Expand `call` and parse what it produced as an expression.
    ///
    /// Marks come off the names that are not variables once it is parsed: a
    /// label and a variable are the same token, and only a tree tells them
    /// apart (see [`hygiene`]).
    fn as_expr(&mut self, call: &ast::MacCall, span: Span) -> Option<ast::LExpr> {
        let out = self.run(call)?;
        let (parsed, errs) = meadow_parser::parse_expr(&out, span);
        let mut e = self.parsed(call, span, "an expression", parsed, first_error!(errs))?;
        hygiene::strip_in_expr(&mut e);
        Some(e)
    }

    fn as_pat(&mut self, call: &ast::MacCall, span: Span) -> Option<ast::LPat> {
        let out = self.run(call)?;
        let (parsed, errs) = meadow_parser::parse_pat(&out, span);
        let mut p = self.parsed(call, span, "a pattern", parsed, first_error!(errs))?;
        hygiene::strip_in_pat(&mut p);
        Some(p)
    }

    fn as_decls(&mut self, call: &ast::MacCall, span: Span) -> Option<Vec<ast::LDecl>> {
        let out = self.run(call)?;
        let (parsed, errs) = meadow_parser::parse_decls(&out, span);
        let mut ds = self.parsed(call, span, "declarations", parsed, first_error!(errs))?;
        hygiene::strip_items(&mut ds);
        Some(ds)
    }

    /// Report a macro whose output did not parse where it was called.
    ///
    /// The error is at the call rather than inside the expansion, because the
    /// expansion is not in the file: there is nothing to point at. It names the
    /// macro, which is the part the reader can act on.
    fn parsed<T>(
        &mut self,
        call: &ast::MacCall,
        span: Span,
        wanted: &str,
        parsed: Option<T>,
        failed: Option<(String, Span)>,
    ) -> Option<T> {
        match (parsed, failed) {
            (Some(t), None) => Some(t),
            // Where the parser stopped, when that is a token the caller wrote
            // and the macro passed through: the mistake is there.
            (_, Some((why, at))) if at != span && span.start <= at.start && at.end <= span.end => {
                self.error(
                    format!("`{}!` did not expand to {wanted}: {why}", call.name()),
                    "here".to_string(),
                    at,
                    vec![],
                );
                None
            }
            (_, why) => {
                let why = why.map(|(w, _)| format!(": {w}")).unwrap_or_default();
                self.error(
                    format!("`{}!` did not expand to {wanted}{why}", call.name()),
                    format!("this call is where {wanted} belongs"),
                    span,
                    vec![],
                );
                None
            }
        }
    }

    // --- walking --------------------------------------------------------------

    fn decls(&mut self, decls: &mut Vec<ast::LDecl>) {
        // Rebuilt rather than edited in place: one call may expand to several
        // declarations, or to none.
        let mut out = Vec::with_capacity(decls.len());
        for mut d in std::mem::take(decls) {
            // A call, perhaps with attributes: its `@pub` says how far what it
            // defines at compile time may be seen.
            if let Some((attrs, call)) = attributed_call(&d) {
                let vis = vis_of(attrs, self.gated);
                let outer = self.call_vis.replace(vis);
                // What it made is expanded again one level deeper, so a macro
                // that expands to a call of itself counts every step.
                let made = self.deeper(d.span, |ex| {
                    let made = ex.as_decls(call, d.span);
                    ex.call_vis = outer;
                    made.map(|mut made| {
                        ex.decls(&mut made);
                        made
                    })
                });
                self.call_vis = outer;
                match made {
                    Some(made) => {
                        record::decls(self.filename, d.span, "decls", &made);
                        out.extend(made)
                    }
                    // Waiting for a name: left as it is, for a later round.
                    None if self.take_waited() => out.push(d),
                    None => {}
                }
                continue;
            }
            match &*d.value {
                // `@derive(Show)`: the declaration stays as it is, and what
                // the macro wrote about it follows. A derive that is waiting
                // for a name stays on it, to be run in a later round.
                ast::Decl::Attributed(attrs, _) if attrs.iter().any(is_derive) => {
                    let (made, pending, ran) = self.derived(&d);
                    // What the compiler's own derives wrote is not recorded:
                    // MeadowBoot writes that itself (`Derive`), and reads a
                    // record only of what a procedural macro had a hand in.
                    if ran {
                        record::decls(self.filename, d.span, "derived", &made);
                    }
                    self.decl(&mut d);
                    out.push(keep_derives(d, &pending));
                    out.extend(made);
                }
                _ => {
                    self.decl(&mut d);
                    out.push(d);
                }
            }
        }
        *decls = out;
    }

    /// Run every `@derive` on `d`, with the declaration itself as the argument.
    ///
    /// The tokens are taken from the source rather than written back out of
    /// the tree, so a macro sees exactly what was written -- attributes and
    /// all, on the declaration and on its variants, which is where a derive of
    /// any substance keeps what it needs.
    ///
    /// Answers what they wrote, the derives that are waiting for a
    /// compile-time binding and are to be run again in a later round, and
    /// whether any of them is a procedural macro.
    fn derived(&mut self, d: &ast::LDecl) -> (Vec<ast::LDecl>, Vec<ast::Ident>, bool) {
        let ast::Decl::Attributed(attrs, inner) = &*d.value else {
            return (Vec::new(), Vec::new(), false);
        };
        let mut out = Vec::new();
        let mut pending = Vec::new();
        let mut ran = false;
        for want in attrs.iter().filter(|a| is_derive(a)).flat_map(|a| &a.args) {
            let Some((pkg, name)) = self.deriving(want) else {
                // No procedural macro of that name: one of the compiler's own,
                // or nothing that can derive it.
                match derive::builtin(want.value(), inner) {
                    Some(Ok(text)) => out.extend(self.derived_text(want, &text)),
                    Some(Err(why)) => self.error(
                        format!("`{}` cannot be derived for this", want.value()),
                        why,
                        want.span,
                        vec![],
                    ),
                    None => {
                        let mut lower = want.value().to_string();
                        lower.replace_range(..1, &lower[..1].to_lowercase());
                        self.error(
                            format!("there is no macro to derive `{}` with", want.value()),
                            format!("nothing in scope is `{lower}!`"),
                            want.span,
                            vec![],
                        );
                    }
                }
                continue;
            };
            ran = true;
            let Some(argument) = self.source_trees(d.span) else {
                continue;
            };
            let Some(runner) = self.procs else {
                self.error(
                    format!(
                        "`{}` is a procedural macro, which cannot run here",
                        want.value()
                    ),
                    "running one means building the package that defines it".to_string(),
                    want.span,
                    vec![],
                );
                continue;
            };
            let scope = proc::Scope {
                visible: &self.scope,
                settled: self.settled,
            };
            let outcome = runner.run(pkg, name, &argument, inner.span, &scope);
            if let Ok(proc::Outcome::Waiting(_)) = outcome {
                self.waiting += 1;
                pending.push(want.clone());
                continue;
            }
            self.from.push(Expansion {
                at: inner.span,
                filename: self.filename.to_string(),
                name: want.value().to_string(),
            });
            match outcome {
                Ok(proc::Outcome::Waiting(_)) => unreachable!("handled above"),
                Ok(proc::Outcome::Answered { trees, defined, .. }) => {
                    self.defined(defined, inner.span);
                    let tokens = tt::flatten(&trees);
                    let (parsed, errs) = meadow_parser::parse_decls(&tokens, inner.span);
                    match parsed {
                        Some(mut made) if errs.is_empty() => {
                            self.decls(&mut made);
                            out.extend(made);
                        }
                        _ => self.error(
                            format!("`{}` did not write declarations", want.value()),
                            "a derive writes what goes beside the declaration".to_string(),
                            want.span,
                            vec![],
                        ),
                    }
                }
                Err(why) => self.failed(
                    &format!("`{}`", want.value()),
                    why,
                    want.span,
                    "this declaration is what it was given",
                ),
            }
        }
        (out, pending, ran)
    }

    /// What a built-in derive wrote, parsed where the derive was written.
    fn derived_text(&mut self, want: &ast::Ident, text: &str) -> Vec<ast::LDecl> {
        let source = meadow_source::Source::new(
            meadow_source::SourceKind::Interactive,
            InternedString::from(text),
        );
        let lexed = meadow_lexer::tokenize(source);
        // Every token is the derive's, at an empty span where it was written:
        // an error in what it wrote is reported at the `@derive` that asked
        // for it, and none of it claims the text of the name -- which an
        // editor still resolves to the trait, not to what the `impl` calls.
        //
        // And every name it writes carries the prelude's mark: what the derive
        // binds, it binds under the mark, and what it only mentions -- `get`,
        // `fst`, `debug` -- is the prelude's, not whatever the scope it is
        // expanded in calls that (see `meadow_rename`). The marks come off the
        // names that are not variables once it is parsed, as a template's do.
        let at = Span::new(want.span.start, want.span.start);
        let tokens: Vec<LToken> = lexed
            .tokens
            .iter()
            .map(|t| {
                let marked = match t.value() {
                    // The one type a derive names that a module need not
                    // have: the standard library's `Datum`, which `Reflect`'s
                    // is written in.
                    Token::UpperIdent(n) if &**n == "Datum" => {
                        Token::UpperIdent(ast::hygiene::mark(*n, ast::hygiene::PRELUDE))
                    }
                    other => hygiene::mark(other, ast::hygiene::PRELUDE),
                };
                LToken::new(marked, at)
            })
            .collect();
        match meadow_parser::parse_decls(&tokens, at) {
            (Some(mut made), errs) if errs.is_empty() => {
                hygiene::strip_items(&mut made);
                self.decls(&mut made);
                made
            }
            _ => {
                self.error(
                    format!(
                        "the derive of `{}` wrote something that does not parse",
                        want.value()
                    ),
                    "the compiler's own derive; a bug in it".to_string(),
                    want.span,
                    vec![],
                );
                Vec::new()
            }
        }
    }

    /// The procedural macro a `@derive(Name)` names, if one does. `None` is a
    /// derive the compiler has built in -- see [`derive`] -- or none at all.
    ///
    /// A macro is a function and functions are lower-case, so `@derive(Lexer)`
    /// finds `lexer`; the name as written is tried first, for a derive that
    /// was written the way it is defined.
    fn deriving(&mut self, want: &ast::Ident) -> Option<(InternedString, InternedString)> {
        let written = want.value().to_string();
        let mut lower = written.clone();
        lower.replace_range(..1, &written[..1].to_lowercase());
        for name in [&written, &lower] {
            if let Some(found) = self.proc_macros.get(name.as_str()) {
                return Some(*found);
            }
        }
        None
    }

    /// The token trees of the source between `span`, as they were written.
    fn source_trees(&mut self, span: Span) -> Option<Vec<tt::TokenTree>> {
        let text = self
            .text
            .get(span.start as usize..span.end as usize)?
            .to_string();
        let src = meadow_source::Source::new(
            meadow_source::SourceKind::Interactive,
            InternedString::from(text),
        );
        let lexed = meadow_lexer::tokenize(src);
        // Every token stands where it was written, so an error in one is
        // reported there.
        let moved: Vec<LToken> = lexed
            .tokens
            .iter()
            .map(|t| {
                LToken::new(
                    t.value().clone(),
                    Span::new(span.start + t.span.start, span.start + t.span.end),
                )
            })
            .collect();
        let (trees, _) = tt::trees(&moved, self.filename);
        Some(trees)
    }

    fn decl(&mut self, d: &mut ast::LDecl) {
        match &mut *d.value {
            ast::Decl::Bind(b) => self.bind(b),
            ast::Decl::Attributed(_, inner) => self.decl(inner),
            ast::Decl::Trait(td) => td.defaults.iter_mut().for_each(|b| self.bind(b)),
            ast::Decl::Impl(id) => id.methods.iter_mut().for_each(|b| self.bind(b)),
            // Nothing else holds an expression or a pattern: a type is not a
            // place a macro may be called (see `docs/MACROS.md`).
            ast::Decl::MacCall(_)
            | ast::Decl::Macro(_)
            | ast::Decl::Mod(_)
            // A module of its own once it is lifted out, and expanded then.
            | ast::Decl::Module(..)
            | ast::Decl::Use(_)
            | ast::Decl::Data(_)
            | ast::Decl::Record(_)
            | ast::Decl::Effect(_)
            | ast::Decl::TypeAlias(_)
            | ast::Decl::EffectAlias(_)
            | ast::Decl::Fixity(..)
            | ast::Decl::Sig(..) => {}
        }
    }

    fn bind(&mut self, b: &mut ast::Bind) {
        match b {
            ast::Bind::Pat(p, e) => {
                self.pat(p);
                self.expr(e);
            }
            ast::Bind::Fun(_, params, _, body) => {
                for p in params {
                    self.pat(p);
                }
                self.expr(body);
            }
        }
    }

    fn pat(&mut self, p: &mut ast::LPat) {
        if let ast::Pat::MacCall(call) = &*p.value {
            let span = p.span;
            // Re-expanded, in case what came out holds a call of its own --
            // one level deeper, so a recursive macro counts every step.
            let made = self.deeper(span, |ex| {
                ex.as_pat(call, span).map(|mut made| {
                    ex.pat(&mut made);
                    made
                })
            });
            match made {
                Some(made) => {
                    record::pat(self.filename, span, &made);
                    *p = made
                }
                // Waiting for a name: left as it is, for a later round.
                None if self.take_waited() => {}
                // Left as a wildcard: it matches, binds nothing, and lets the
                // rest of the arm be checked instead of collapsing after one
                // error.
                None => *p = Located::new(ast::Pat::Wildcard, span),
            }
            return;
        }
        match &mut *p.value {
            ast::Pat::Ann(inner, _) => self.pat(inner),
            ast::Pat::As(_, inner) => self.pat(inner),
            ast::Pat::View(f, inner) => {
                self.expr(f);
                self.pat(inner);
            }
            ast::Pat::Cons(_, ps)
            | ast::Pat::QualCons(_, _, ps)
            | ast::Pat::Tuple(ps)
            | ast::Pat::Array(ps)
            | ast::Pat::Vector(ps)
            | ast::Pat::List(ps) => {
                for p in ps {
                    self.pat(p);
                }
            }
            ast::Pat::Record(fields, _) => {
                for (_, p) in fields {
                    self.pat(p);
                }
            }
            ast::Pat::MacCall(_)
            | ast::Pat::Wildcard
            | ast::Pat::Var(_)
            | ast::Pat::Lit(_)
            | ast::Pat::Unit => {}
        }
    }

    fn expr(&mut self, e: &mut ast::LExpr) {
        if let ast::Expr::MacCall(call) = &*e.value {
            let span = e.span;
            let made = self.deeper(span, |ex| {
                ex.as_expr(call, span).map(|mut made| {
                    ex.expr(&mut made);
                    made
                })
            });
            match made {
                Some(made) => {
                    record::expr(self.filename, span, &made);
                    *e = made
                }
                // Waiting for a name: left as it is, for a later round.
                None if self.take_waited() => {}
                // `()` in its place: the module keeps its shape, so everything
                // around the failed call is still checked.
                None => *e = Located::new(ast::Expr::Unit, span),
            }
            return;
        }
        match &mut *e.value {
            ast::Expr::Lam(ps, body) => {
                for p in ps {
                    self.pat(p);
                }
                self.expr(body);
            }
            ast::Expr::App(f, args) => {
                self.expr(f);
                for a in args {
                    self.expr(a);
                }
            }
            ast::Expr::Let(binds, body) => {
                for b in binds {
                    self.bind(b);
                }
                self.expr(body);
            }
            ast::Expr::If(c, t, f) => {
                self.expr(c);
                self.expr(t);
                self.expr(f);
            }
            ast::Expr::Match(scrutinee, arms) => {
                self.expr(scrutinee);
                for (p, guard, body) in arms {
                    self.pat(p);
                    if let Some(g) = guard {
                        self.expr(g);
                    }
                    self.expr(body);
                }
            }
            ast::Expr::UnOp(_, x) => self.expr(x),
            ast::Expr::BinOp(_, l, r) => {
                self.expr(l);
                self.expr(r);
            }
            ast::Expr::Infix(first, rest) => {
                self.expr(first);
                for (_, x) in rest {
                    self.expr(x);
                }
            }
            ast::Expr::Interp(_, holes) => {
                for (h, _) in holes {
                    self.expr(h);
                }
            }
            ast::Expr::Tuple(xs)
            | ast::Expr::Array(xs)
            | ast::Expr::List(xs)
            | ast::Expr::Cons(_, xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            ast::Expr::Record(fields, base) => {
                for (_, v) in fields {
                    self.expr(v);
                }
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            ast::Expr::Update(base, fields) => {
                self.expr(base);
                for (_, v) in fields {
                    self.expr(v);
                }
            }
            ast::Expr::Field(o, _) => self.expr(o),
            ast::Expr::Handle(body, arms, ret) => {
                self.expr(body);
                for arm in arms {
                    self.pat(&mut arm.param);
                    self.expr(&mut arm.body);
                }
                if let Some((p, e)) = ret {
                    self.pat(p);
                    self.expr(e);
                }
            }
            ast::Expr::MacCall(_)
            | ast::Expr::Var(_)
            | ast::Expr::Lit(_)
            | ast::Expr::Qual(_, _)
            | ast::Expr::Unit
            | ast::Expr::Hole => {}
        }
    }

    // --- the depth limit --------------------------------------------------

    /// Run `f` one expansion deeper, or report a macro that never stops.
    ///
    /// `f` expands the call at `span` and then what that made, so the depth is
    /// how many expansions are under way at once: a macro that expands into a
    /// call of itself goes one deeper each time, and is stopped here rather
    /// than by the compiler's stack. Reported once, where it gave up; the
    /// expansions around it see a failed call and go on.
    fn deeper<T>(&mut self, span: Span, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.overflowed {
            return None;
        }
        if self.depth >= MAX_DEPTH {
            self.overflowed = true;
            self.error(
                format!(
                    "macro expansion went {MAX_DEPTH} deep: a macro that expands to a call of \
                     itself needs a case that does not"
                ),
                "expanded too deeply".to_string(),
                span,
                vec![],
            );
            return None;
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    /// Report a macro that gave no answer: at the token it said was wrong
    /// when it said one, and at `fallback` -- the call -- when it did not.
    fn failed(&mut self, who: &str, why: proc::Failure, fallback: Span, label: &str) {
        match why.at {
            Some(at) => self.error(why.msg, format!("{who} stopped here"), at, vec![]),
            None => self.error(
                format!("{who} did not answer: {}", why.msg),
                label.to_string(),
                fallback,
                vec![],
            ),
        }
    }

    fn error(&mut self, msg: String, label: String, span: Span, extra: Vec<(String, Span)>) {
        self.diags.push(Diagnostic::new(
            msg,
            self.filename.to_string(),
            (label, span),
            extra,
        ));
    }
}

/// The 1-based line `at` is on in `text`.
/// The `use` inside a declaration, looking through any `@attr` wrapper.
fn peel_use(decl: &ast::LDecl) -> Option<&ast::UseDecl> {
    match &*decl.value {
        ast::Decl::Use(u) => Some(u),
        ast::Decl::Attributed(_, inner) => peel_use(inner),
        _ => None,
    }
}

/// How far a `@pub` says a macro can be seen. `gated` is false when the unit
/// never says `@pub` at all, and then everything is exported -- the rule the
/// rest of a package's surface follows.
fn vis_of(attrs: &[ast::Attr], gated: bool) -> Vis {
    if !gated {
        return Vis::Exported;
    }
    let Some(a) = attrs.iter().find(|a| &**a.name.value() == "pub") else {
        return Vis::Private;
    };
    match a.args.first().map(|arg| arg.value().to_string()) {
        None => Vis::Exported,
        Some(w) if w == "super" => Vis::Super,
        // `pkg`, and anything unknown: narrower than guessing it was public.
        Some(_) => Vis::Package,
    }
}

/// The name of a function written `@macro`, if that is what `d` is.
fn marked_macro(d: &ast::LDecl) -> Option<InternedString> {
    let ast::Decl::Attributed(attrs, inner) = &*d.value else {
        return None;
    };
    if !attrs.iter().any(|a| &**a.name.value() == "macro") {
        return None;
    }
    match &*inner.value {
        ast::Decl::Bind(ast::Bind::Fun(name, ..)) => Some(*name.value()),
        _ => None,
    }
}

/// Whether an attribute is a `@derive`.
fn is_derive(a: &ast::Attr) -> bool {
    &**a.name.value() == "derive"
}

/// `d` with only the derives in `pending` left on it: the rest have had their
/// say, and nothing after this knows what one is. A pending one is waiting for
/// a compile-time binding, and stays to be run again in a later round.
fn keep_derives(d: ast::LDecl, pending: &[ast::Ident]) -> ast::LDecl {
    let span = d.span;
    match *d.value {
        ast::Decl::Attributed(attrs, inner) => {
            let mut kept: Vec<ast::Attr> = attrs.into_iter().filter(|a| !is_derive(a)).collect();
            if !pending.is_empty() {
                kept.push(ast::Attr {
                    name: ast::Ident::new(InternedString::from("derive"), span),
                    args: pending.to_vec(),
                    meta: Vec::new(),
                });
            }
            if kept.is_empty() {
                *inner
            } else {
                ast::LDecl::new(ast::Decl::Attributed(kept, inner), span)
            }
        }
        other => ast::LDecl::new(other, span),
    }
}

/// The macro call `d` is, and the attributes written on it, if it is one.
fn attributed_call(d: &ast::LDecl) -> Option<(&[ast::Attr], &ast::MacCall)> {
    match &*d.value {
        ast::Decl::MacCall(call) => Some((&[], call)),
        ast::Decl::Attributed(attrs, inner) => match &*inner.value {
            ast::Decl::MacCall(call) => Some((&attrs[..], call)),
            _ => None,
        },
        _ => None,
    }
}

fn dotted(segs: &[InternedString]) -> String {
    segs.iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn line_of(text: &str, at: u32) -> usize {
    let at = (at as usize).min(text.len());
    text[..at].bytes().filter(|&b| b == b'\n').count() + 1
}

/// `trees` split on every top-level `sep`, separators dropped. A separator
/// inside a bracket belongs to that group, not to this split.
fn split(trees: &[tt::TokenTree], sep: Token) -> Vec<&[tt::TokenTree]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, t) in trees.iter().enumerate() {
        if matches!(t, tt::TokenTree::Token(t) if *t.value() == sep) {
            out.push(&trees[start..i]);
            start = i + 1;
        }
    }
    out.push(&trees[start..]);
    out
}

/// The text a literal token stands for: what `concat!` joins. `None` for a
/// token that is not a literal.
fn literal_text(t: &Token) -> Option<String> {
    match t {
        // The string's contents, not the quotes around it.
        Token::String(s) => Some(s.to_string()),
        Token::Char(c) => Some(c.to_string()),
        Token::Int(_) | Token::Real(_) => Some(t.text()),
        _ => None,
    }
}
