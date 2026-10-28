//! Core comes out of lowering typed, and the lint agrees with it.
//!
//! The lint itself runs inside `compile_unit` whenever debug assertions are
//! on, so every one of these compiles is already checked — a failure shows up
//! as a panic rather than as a wrong answer here. What these tests pin is the
//! part the lint cannot see on its own: that the *instantiations* lowering
//! recovers are the right ones, including across a unit boundary, which is
//! the shape every REPL line has.

use meadow_compiler::{
    AstModule, CompiledPackage, Options, compile_unit, core,
    intern::InternedString,
    lexer::tokenize,
    parser,
    source::{Source, SourceKind},
};

fn unit(name: &str, src: &str, deps: &[&CompiledPackage]) -> CompiledPackage {
    let name = InternedString::from(name);
    let source = Source::new(SourceKind::Interactive, InternedString::from(src));
    let lex = tokenize(source);
    let (ast, perrs) = parser::parse(name, source, &lex.tokens);
    assert!(perrs.is_empty(), "parse errors: {perrs:?}");
    let modules = vec![AstModule {
        path: vec![],
        name,
        ast: ast.expect("parses"),
        source,
    }];
    let deps: Vec<meadow_compiler::Dep<'_>> = deps
        .iter()
        .copied()
        .map(meadow_compiler::Dep::new)
        .collect();
    let (pkg, diags) = compile_unit(name, 0, modules, &deps, Options::debug());
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    pkg
}

/// Every type argument written anywhere in a program, rendered.
fn type_args(pkg: &CompiledPackage) -> Vec<String> {
    fn go(t: &core::Term, out: &mut Vec<String>) {
        use core::Term as T;
        match t {
            T::TyApp(f, args) => {
                for a in args {
                    out.push(format!("{a:?}"));
                }
                go(f, out);
            }
            T::TyLam(_, b) | T::Lam(_, _, b) | T::Proj(b, _) | T::Sel(b, _, _) | T::Loc(_, b) => {
                go(b, out)
            }
            T::App(f, a) => {
                go(f, out);
                go(a, out);
            }
            T::Let(_, _, r, b) => {
                go(r, out);
                go(b, out);
            }
            T::Join { rhs, body, .. } => {
                go(rhs, out);
                go(body, out);
            }
            T::Jump(_, args, _) => args.iter().for_each(|a| go(a, out)),
            T::LetRec(binds, b) => {
                for (_, _, r) in binds {
                    go(r, out);
                }
                go(b, out);
            }
            T::If(c, a, b) => {
                go(c, out);
                go(a, out);
                go(b, out);
            }
            T::Tuple(xs) | T::Array(xs, _) | T::Ctor(_, _, xs) | T::Prim(_, xs, _) => {
                xs.iter().for_each(|x| go(x, out))
            }
            T::Record(fs) => fs.iter().for_each(|(_, x)| go(x, out)),
            T::Extend(a, _, b) => {
                go(a, out);
                go(b, out);
            }
            T::Case(s, arms, _) => {
                go(s, out);
                arms.iter().for_each(|(_, g, b)| {
                    if let Some(g) = g {
                        go(g, out);
                    }
                    go(b, out)
                });
            }
            T::Perform(_, _, a, _) => go(a, out),
            T::Handle {
                body, clauses, ret, ..
            } => {
                go(body, out);
                clauses.iter().for_each(|c| go(&c.body, out));
                if let Some((_, _, r)) = ret {
                    go(r, out);
                }
            }
            T::Var(_) | T::Lit(_) | T::Error => {}
        }
    }
    let mut out = Vec::new();
    for d in &pkg.defs {
        go(&d.term, &mut out);
    }
    out
}

#[test]
fn a_polymorphic_definition_binds_its_type_variables() {
    let pkg = unit("m", "fun ident x = x\ndef n = ident 3\n", &[]);
    let id = pkg
        .defs
        .iter()
        .find(|d| &*d.name == "ident")
        .expect("the definition");
    // Two binders, not one: the argument's type, and the arrow's latent
    // effect — `ident : forall a e. a -> a ! e`.
    let kinds: Vec<_> = id.poly.binders.iter().map(|b| b.kind).collect();
    assert!(
        kinds.contains(&meadow_compiler::infer::VarKind::Type)
            && kinds.contains(&meadow_compiler::infer::VarKind::Effect),
        "expected a type and an effect binder, got {kinds:?}"
    );
    assert!(
        matches!(id.term, core::Term::TyLam(..)),
        "a generalized binding is a type abstraction"
    );
}

#[test]
fn one_definition_used_at_two_types_is_instantiated_at_each() {
    let pkg = unit(
        "m",
        "fun ident x = x\ndef n = ident 3\ndef s = ident \"hi\"\n",
        &[],
    );
    let args = type_args(&pkg);
    assert!(
        args.iter().any(|a| a.contains("Int")),
        "no instantiation at `Int`: {args:?}"
    );
    assert!(
        args.iter().any(|a| a.contains("String")),
        "no instantiation at `String`: {args:?}"
    );
}

#[test]
fn a_mention_across_a_unit_boundary_is_instantiated_too() {
    // The shape of a REPL line: this unit's mention refers to a binding whose
    // scheme arrived from somewhere else, so the type arguments have to be
    // recovered from that scheme rather than from anything local.
    let base = unit("base", "@pub fun ident x = x\n", &[]);
    let top = unit("top", "def n = ident 3\ndef s = ident \"hi\"\n", &[&base]);
    let args = type_args(&top);
    assert!(
        args.iter().any(|a| a.contains("Int")) && args.iter().any(|a| a.contains("String")),
        "a dependency's binding was not instantiated at both uses: {args:?}"
    );
}

#[test]
fn a_monomorphic_definition_has_no_type_abstraction() {
    let pkg = unit("m", "def n = 1 + 2\n", &[]);
    let d = &pkg.defs[0];
    assert!(d.poly.binders.is_empty(), "nothing to generalize over");
    assert!(!matches!(d.term, core::Term::TyLam(..)));
}

#[test]
fn erasure_removes_every_type_abstraction() {
    let pkg = unit("m", "fun ident x = x\ndef n = ident 3\n", &[]);
    let program = core::Program {
        defs: pkg.defs.clone(),
        entry: None,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
        migrations: Vec::new(),
    };
    let erased = core::erase::program(&program);
    fn any_types(t: &core::Term) -> bool {
        use core::Term as T;
        match t {
            T::TyLam(..) | T::TyApp(..) => true,
            T::Lam(_, _, b) => any_types(b),
            T::App(f, a) => any_types(f) || any_types(a),
            T::Let(_, _, r, b) => any_types(r) || any_types(b),
            _ => false,
        }
    }
    assert!(
        erased.defs.iter().all(|d| !any_types(&d.term)),
        "a type abstraction survived erasure"
    );
    // And a `TyLam`'d definition still has its lambda underneath, which is
    // what keeps the back end's known-arity fast path working.
    let id = erased.defs.iter().find(|d| &*d.name == "ident").unwrap();
    assert!(matches!(id.term, core::Term::Lam(..)));
}
