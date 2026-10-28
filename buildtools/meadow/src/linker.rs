//! The linker stitches compiled packages into one program.
//!
//! With no code generation yet its job is modest: concatenate every package's
//! [`core::Def`]s in dependency order, build one global symbol table, and locate
//! the `main` entry point. It also keeps the per-package type tables around so the
//! driver can print a fully annotated tree.
//!
//! A program runs by calling `main`, which is `() -> () ! Eff`. So the entry
//! the runtimes are given is one more definition, `start = main ()`, and they
//! evaluate it as they would any other: they never learn which definitions are
//! functions.

use meadow_compiler::{
    CompiledPackage, core, hir,
    hir::VarId,
    infer::{Scheme, Type, VarKind},
    intern::InternedString,
};
use std::fmt::Write;
use std::sync::Arc;

pub struct GlobalSymbol {
    pub package: InternedString,
    pub name: InternedString,
    pub var: VarId,
    pub scheme: Scheme,
}

/// A `@test` function, and the package and module it came from.
pub struct TestCase {
    pub package: InternedString,
    /// `Module.name`, or the bare name in the root module -- see
    /// [`meadow_compiler::TestSite::qualified`]. What `meadow test` prints and
    /// filters on.
    pub name: InternedString,
    pub var: VarId,
}

pub struct LinkedProgram {
    pub program: core::Program,
    pub symbols: Vec<GlobalSymbol>,
    pub packages: Vec<CompiledPackage>,
    /// Every `@test` in the linked packages, in package then declaration order.
    pub tests: Vec<TestCase>,
}

pub struct Linker;

impl Linker {
    pub fn link(packages: Vec<CompiledPackage>) -> LinkedProgram {
        let mut defs = Vec::new();
        let mut symbols = Vec::new();
        let mut tests = Vec::new();
        let mut ctor_fields = std::collections::HashMap::new();
        let mut variants = std::collections::HashMap::new();
        let mut migrations = Vec::new();
        let mut entry = None;

        for pkg in &packages {
            defs.extend(pkg.defs.iter().cloned());
            ctor_fields.extend(pkg.ctor_fields.clone());
            variants.extend(pkg.variants.clone());
            migrations.extend(pkg.migrations.iter().copied());
            tests.extend(pkg.test_sites().into_iter().map(|t| TestCase {
                package: pkg.name,
                name: InternedString::from(t.qualified()),
                var: t.var,
            }));
            // A package's own `main`, which needs no export — see
            // `CompiledPackage::entry`. The last one wins, and packages arrive
            // in dependency order, so that is the root package's.
            if let Some(var) = pkg.entry {
                entry = Some(Entry::Main(var));
            }
            if let Some(var) = pkg.value_entry {
                entry = Some(Entry::Value(var));
            }
            for e in &pkg.exports {
                symbols.push(GlobalSymbol {
                    package: pkg.name,
                    name: e.name,
                    var: e.var,
                    scheme: e.scheme.clone(),
                });
            }
        }

        let mut program = core::Program {
            defs,
            entry: None,
            ctor_fields,
            variants,
            origins: Default::default(),
            migrations,
        };
        program.entry = match entry {
            Some(Entry::Main(main)) => Some(start(&mut program, main)),
            Some(Entry::Value(value)) => Some(value),
            None => None,
        };
        LinkedProgram {
            program,
            symbols,
            packages,
            tests,
        }
    }
}

/// What a program runs.
#[derive(Clone, Copy)]
enum Entry {
    /// `main`, which is called.
    Main(VarId),
    /// A value the build asked for by name, which is evaluated and shown.
    Value(VarId),
}

/// `start = main ()`, added to `program`: what runs it.
///
/// `main` is `() -> () ! Eff`, so a type it is general in can only be `()`, and
/// an effect or a row it is general in can only be empty -- which is what it is
/// instantiated at here.
fn start(program: &mut core::Program, main: VarId) -> VarId {
    let binders = program
        .defs
        .iter()
        .find(|d| d.var == main)
        .map(|d| d.poly.binders.clone())
        .unwrap_or_default();
    let mut callee = core::Term::Var(main);
    if !binders.is_empty() {
        let tys = binders
            .iter()
            .map(|b| match b.kind {
                VarKind::Row | VarKind::Effect => Type::RowEmpty,
                VarKind::Type | VarKind::Num | VarKind::Frac => Type::unit(),
            })
            .collect();
        callee = core::Term::TyApp(Arc::new(callee), tys);
    }
    let var = VarId(core::simplify::max_var(program) + 1);
    program.defs.push(core::Def {
        var,
        name: InternedString::from("start"),
        module: Default::default(),
        poly: core::Poly::mono(Type::unit()),
        term: core::Term::App(Arc::new(callee), Arc::new(core::Term::Lit(core::Lit::Unit))),
    });
    var
}

impl LinkedProgram {
    /// Human-readable summary: every exported symbol with its inferred scheme, plus
    /// how many HIR nodes each package managed to annotate, plus the entry point.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        // The embedded standard library is left out: it is hundreds of lines of
        // signatures nobody here wrote.
        for pkg in self
            .packages
            .iter()
            .filter(|p| &*p.name != crate::stdlib::PACKAGE_NAME)
        {
            let _ = writeln!(out, "=== package {} ===", pkg.name);
            for sym in self.symbols.iter().filter(|s| s.package == pkg.name) {
                let _ = writeln!(out, "  {} : {}", hir::spell_name(&sym.name), sym.scheme);
            }
            let annotated = pkg.types.rendered().len();
            let _ = writeln!(out, "  ({annotated} annotated nodes)");
        }
        match self.program.entry {
            Some(v) => {
                // An entry point need not be exported, so the symbol table
                // may not know it; it is `main` either way.
                let name = self
                    .symbols
                    .iter()
                    .find(|s| s.var == v)
                    .map(|s| s.name.to_string())
                    .unwrap_or_else(|| "main".to_string());
                let _ = writeln!(out, "entry: {name}");
            }
            None => {
                let _ = writeln!(out, "entry: (none)");
            }
        }
        out
    }

    /// Every annotated node, as `pkg#id : type`. Useful for eyeballing the fully
    /// typed tree.
    pub fn annotations(&self) -> String {
        let mut out = String::new();
        for pkg in &self.packages {
            for (id, ty) in pkg.types.rendered() {
                let _ = writeln!(out, "{}#{} : {}", pkg.name, id.0, ty);
            }
        }
        out
    }
}
