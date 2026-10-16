//! Parsing, via `chumsky`.
//!
//! [`parse`] parses a whole module; [`parse_repl`] parses a single decl *or*
//! expression for the REPL. Expressions are built bottom-up: `atom` → `app`
//! (juxtaposition) → `pratt` (operators, with precedence/associativity given by
//! the `infix`/`prefix` levels). Type expressions have their own small grammar
//! ([`ty`] / [`ty_atom`]) used only inside `data` / `record` declarations.
//!
//! Errors are returned alongside a partial tree (`(Option<_>, Vec<Rich>)`); the
//! driver converts each `Rich` into a [`Diagnostic`].
//!
//! [`Diagnostic`]: meadow_diagnostics::Diagnostic

use chumsky::{
    IterParser, Parser,
    error::Rich,
    extra,
    input::{Input, ValueInput},
    primitive::*,
    recursive::recursive,
    select,
};
use itertools::Either;
use meadow_ast::*;
use meadow_intern::InternedString;
use meadow_lexer::{LToken, Token, tt};
use meadow_source::Source;
use meadow_span::{Located, Span};
use std::borrow::Cow;

/// A name as a person writes it outside an expression: an operator in the
/// parentheses it is declared, imported and exported with, `(>=)` and not `>=`.
///
/// The same rule as `meadow_hir::spell_name`, which is downstream of here and
/// so cannot be borrowed from -- and this is the layer that knows how a name is
/// spelled in source anyway.
fn spelled(name: &str) -> Cow<'_, str> {
    match name.chars().next() {
        Some(c) if !(c.is_alphanumeric() || c == '_') => format!("({name})").into(),
        _ => name.into(),
    }
}

/// Parse a whole module.
pub fn parse<'src>(
    name: InternedString,
    src: Source,
    tokens: &'src [LToken],
) -> (Option<LModule>, Vec<Rich<'src, Token, Span>>) {
    let stream = tokens.split_spanned(Span::from(0..src.len()));
    module(name).parse(stream).into_output_errors()
}

/// Parse a single REPL entry: either one declaration or one expression.
///
/// A declaration may be more than one node: `fun f : T` with its clauses under
/// it is one thing to write and two to compile (see [`bare_decl`]).
pub fn parse_repl<'src>(
    src: Source,
    tokens: &'src [LToken],
) -> (
    Option<Either<Vec<LDecl>, LExpr>>,
    Vec<Rich<'src, Token, Span>>,
) {
    let stream = tokens.split_spanned(Span::from(0..src.len()));
    let p = choice((decl().map(Either::Left), expr().map(Either::Right)));
    p.parse(stream).into_output_errors()
}

/// Parse one expression, and nothing else.
///
/// `eoi` is where errors point when the tokens run out early. The three
/// `parse_*` entry points below exist for macro expansion, which parses what a
/// template produced with the entry point for the position the call was in (see
/// `docs/MACROS.md`); the tokens are then not a source file, so there is no
/// [`Source`] to take that span from.
pub fn parse_expr<'src>(
    tokens: &'src [LToken],
    eoi: Span,
) -> (Option<LExpr>, Vec<Rich<'src, Token, Span>>) {
    expr().parse(tokens.split_spanned(eoi)).into_output_errors()
}

/// Parse one pattern, and nothing else. See [`parse_expr`].
pub fn parse_pat<'src>(
    tokens: &'src [LToken],
    eoi: Span,
) -> (Option<LPat>, Vec<Rich<'src, Token, Span>>) {
    pat().parse(tokens.split_spanned(eoi)).into_output_errors()
}

/// Parse a run of declarations -- what a module body is, without the module.
/// Unlike [`parse`] this accepts none, since a macro may expand to nothing. See
/// [`parse_expr`].
pub fn parse_decls<'src>(
    tokens: &'src [LToken],
    eoi: Span,
) -> (Option<Vec<LDecl>>, Vec<Rich<'src, Token, Span>>) {
    decl()
        .repeated()
        .collect::<Vec<_>>()
        .validate(|groups, _, emitter| joined(groups, emitter))
        .parse(tokens.split_spanned(eoi))
        .into_output_errors()
}

fn module<'tokens, I>(
    name: InternedString,
) -> impl Parser<'tokens, I, LModule, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    // `@!name(…)` is on the module, and so comes before anything in it.
    // The two characters lex as one operator.
    let inner = select! { Token::OpIdent(s) if &*s == "@!" => () }
        .ignore_then(attr_body())
        .repeated()
        .collect::<Vec<_>>();
    inner
        .then(
            decl()
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>()
                .validate(|groups, _, emitter| joined(groups, emitter)),
        )
        .map_with(move |(attrs, decls), e| Located::new(Module { name, decls, attrs }, e.span()))
}

/// `@pub`, `@attr(A, B, C)`, `@cfg(all(unix, os = "linux"))` — a `@`, a name,
/// and an optional parenthesised list of arguments (see [`meta`]).
fn attr<'tokens, I>()
-> impl Parser<'tokens, I, Attr, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    just(Token::At).ignore_then(attr_body())
}

/// What follows the `@` of an attribute, or the `@!` of one on a module.
fn attr_body<'tokens, I>()
-> impl Parser<'tokens, I, Attr, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    // `@macro` names an attribute, and `macro` is a keyword: they are
    // different namespaces and nothing else is written after an `@`, so the
    // word is taken as it is spelled.
    path_seg()
        .or(just(Token::Macro).map_with(|_, e| Ident::new(InternedString::from("macro"), e.span())))
        .then(
            meta()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .map(|(name, meta)| {
            let meta = meta.unwrap_or_default();
            let args = meta
                .iter()
                .filter_map(|m| match m {
                    Meta::Word(n) => Some(n.clone()),
                    _ => None,
                })
                .collect();
            Attr { name, args, meta }
        })
}

/// An attribute argument: `name`, `name = "text"`, or `name(arg, …)`.
fn meta<'tokens, I>()
-> impl Parser<'tokens, I, Meta, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    recursive(|meta| {
        let value = just(Token::Eq)
            .ignore_then(select! { Token::String(s) => s })
            .map_with(|s, e| Either::Left(Ident::new(s, e.span())));
        let list = meta
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(Either::Right);
        // A string on its own: `@token("+")`, which is how an attribute that
        // takes one thing reads.
        let text =
            select! { Token::String(s) => s }.map_with(|s, e| Meta::Text(Ident::new(s, e.span())));
        let named =
            path_seg()
                .then(choice((value, list)).or_not())
                .map(|(name, rest)| match rest {
                    None => Meta::Word(name),
                    Some(Either::Left(v)) => Meta::Value(name, v),
                    Some(Either::Right(args)) => Meta::List(name, args),
                });
        choice((text, named))
    })
}

/// One written declaration, as the nodes it compiles to.
///
/// Nearly always exactly one; a signature joined to the clauses that define it
/// is the exception (see [`bare_decl`]), and gives a `Sig` and a `Bind`.
/// [`joined`] then checks, over a whole run of declarations, that nobody wrote
/// those two apart.
fn decl<'tokens, I>()
-> impl Parser<'tokens, I, Vec<LDecl>, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    recursive(|decl| {
        attr()
            .repeated()
            .collect::<Vec<_>>()
            .then(bare_decl(decl))
            .map_with(attributed)
            .boxed()
    })
}

/// `attrs` on each of the nodes one written declaration is.
fn attributed<'tokens, I>(
    (attrs, ds): (Vec<Attr>, Vec<LDecl>),
    e: &mut chumsky::input::MapExtra<'tokens, '_, I, extra::Err<Rich<'tokens, Token, Span>>>,
) -> Vec<LDecl>
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    {
        {
            if attrs.is_empty() {
                return ds;
            }
            ds.into_iter()
                .map(|d| {
                    // What a binding *is* -- exported, a test, a macro -- is
                    // said on its definition, so the attributes go there and
                    // the signature is left bare. `@cfg` is the exception: it
                    // decides whether the declaration exists at all, and a
                    // signature left behind by a definition that was compiled
                    // out is a dangling signature, so it is copied across.
                    let attrs = match d.value() {
                        Decl::Sig(..) => attrs
                            .iter()
                            .filter(|a| &**a.name.value() == "cfg")
                            .cloned()
                            .collect::<Vec<_>>(),
                        _ => attrs.clone(),
                    };
                    if attrs.is_empty() {
                        d
                    } else {
                        LDecl::new(Decl::Attributed(attrs, Box::new(d)), e.span())
                    }
                })
                .collect()
        }
    }
}

fn bare_decl<'tokens, I>(
    decl: impl Parser<'tokens, I, Vec<LDecl>, extra::Err<Rich<'tokens, Token, Span>>> + Clone + 'tokens,
) -> impl Parser<'tokens, I, Vec<LDecl>, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    let bind_decl = {
        // `def x : Int = 5` — the same `: T` a `fun` writes before its `=`,
        // where with no parameters it describes the bound value itself.
        let pat_bind = just(Token::Def)
            .ignore_then(pat())
            .then(result_ty())
            .then_ignore(just(Token::Eq))
            .then(expr())
            .map(|((p, ret), e)| match ret {
                Some(t) => {
                    let span = p.span;
                    Bind::Pat(Located::new(Pat::Ann(Box::new(p), t), span), e)
                }
                None => Bind::Pat(p, e),
            });

        // `fun f a (x, y) = e`, or point-free `fun f = e` (no parameters). At
        // the top level the latter is still a function -- of nothing but the
        // trait dictionaries its type may need, as `eof : Stream s => Parser s
        // ()` is -- so it keeps the `Bind::Fun` shape; checking says its body
        // may not perform effects, as a `def`'s may not. A local one is a
        // value binding.
        let fun_bind = just(Token::Fun)
            .ignore_then(value_ident())
            .then(param_pat().repeated().collect::<Vec<_>>())
            .then(result_ty())
            .then_ignore(just(Token::Eq))
            .then(expr())
            .then(clause().repeated().collect::<Vec<_>>())
            .validate(|((((name, args), ret), body), rest), e, emitter| {
                if args.is_empty() && rest.is_empty() {
                    return Bind::Fun(name, Vec::new(), ret, body);
                }
                equations(name, args, ret, body, rest, e.span(), emitter)
            });

        fun_bind.or(pat_bind)
    };

    // `mod Foo`, whose source is a file's, or `mod Foo { … }`, whose source
    // is here. An empty one is written `mod Foo {}`.
    let mod_decl = just(Token::Mod)
        .ignore_then(path_seg())
        .then(
            decl.repeated()
                .collect::<Vec<_>>()
                .validate(|groups, _, emitter| joined(groups, emitter))
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .or_not(),
        )
        .map_with(|(name, body), e| {
            let d = match body {
                Some(decls) => Decl::Module(name, decls),
                None => Decl::Mod(name),
            };
            LDecl::new(d, e.span())
        });

    let use_decl = just(Token::Use)
        .ignore_then(
            path_seg()
                .separated_by(just(Token::Period))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        // `.*` — every constructor of the type the path ends in. The lexer
        // reads `.*` as one operator token; `. *` spaced out is the same thing.
        .then(
            select! { Token::OpIdent(s) if &*s == ".*" => () }
                .or(just(Token::Period).then(just(Token::Star)).ignored())
                .or_not()
                .map(|g| g.is_some()),
        )
        // `as C` — rename the qualifier. Upper-case, because that is what a
        // qualified reference (`C.map`) can name.
        .then(just(Token::As).ignore_then(upper_ident()).or_not())
        // An operator may be named bare in the list, `(concat, ++)`, or in
        // its own parentheses, `((++))`, as it is written everywhere else. A
        // trailing `!` names a macro, which lives in a namespace of its own.
        .then(
            path_seg()
                .or(user_op())
                .or(value_ident())
                .then(just(Token::Bang).or(just(Token::DeclBang)).or_not())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .map_with(|(((path, glob), alias), selected), e| {
            let (macros, names): (Vec<_>, Vec<_>) = selected
                .unwrap_or_default()
                .into_iter()
                .partition(|(_, bang)| bang.is_some());
            LDecl::new(
                Decl::Use(UseDecl {
                    path,
                    names: names.into_iter().map(|(n, _)| n).collect(),
                    macros: macros.into_iter().map(|(n, _)| n).collect(),
                    glob,
                    alias,
                }),
                e.span(),
            )
        });

    let variant = attr()
        .repeated()
        .collect::<Vec<_>>()
        .then(upper_ident())
        .then(choice((
            field_list().map(VariantFields::Named),
            ty_atom()
                .repeated()
                .collect::<Vec<_>>()
                .map(VariantFields::Positional),
        )))
        .map(|((attrs, name), fields)| Variant {
            attrs,
            name,
            fields,
        });

    let data_decl = just(Token::Data)
        .ignore_then(upper_ident())
        .then(lower_ident().repeated().collect::<Vec<_>>())
        .then_ignore(just(Token::Eq))
        .then(
            just(Token::Bar).or_not().ignore_then(
                variant
                    .separated_by(just(Token::Bar))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            ),
        )
        .map_with(|((name, params), variants), e| {
            LDecl::new(
                Decl::Data(DataDecl {
                    name,
                    params,
                    variants,
                }),
                e.span(),
            )
        });

    let record_decl = just(Token::Record)
        .ignore_then(upper_ident())
        .then(lower_ident().repeated().collect::<Vec<_>>())
        .then_ignore(just(Token::Eq))
        .then(field_list())
        .map_with(|((name, params), fields), e| {
            LDecl::new(
                Decl::Record(RecordDecl {
                    name,
                    params,
                    fields,
                }),
                e.span(),
            )
        });

    // `effect State s { get : () -> s }` declares an effect and its operations;
    // `effect Eff = { Console, Fs }` names effects that are declared already.
    let effect_decl = just(Token::Effect)
        .ignore_then(upper_ident())
        .then(lower_ident().repeated().collect::<Vec<_>>())
        .then(choice((
            just(Token::Eq)
                .ignore_then(effect_row())
                .map(EffectBody::Alias),
            field_list().map(EffectBody::Ops),
        )))
        .map_with(|((name, params), body), e| {
            let decl = match body {
                EffectBody::Ops(ops) => Decl::Effect(EffectDecl { name, params, ops }),
                EffectBody::Alias(row) => Decl::EffectAlias(EffectAliasDecl { name, params, row }),
            };
            LDecl::new(decl, e.span())
        });

    let type_decl = just(Token::Type)
        .ignore_then(upper_ident())
        .then(lower_ident().repeated().collect::<Vec<_>>())
        .then_ignore(just(Token::Eq))
        .then(ty())
        .map_with(|((name, params), ty), e| {
            LDecl::new(
                Decl::TypeAlias(TypeAliasDecl { name, params, ty }),
                e.span(),
            )
        });

    // `fun f : T` / `def x : T` -- a binding's type with no `=` after it. Tried
    // after a binding, which is what the same start with an `=` is. `fun f :
    // (Show a, Ord b) => a -> b -> String` says what its type variables must
    // implement.
    //
    // The clauses that define the binding come under the signature, each one
    // opening with a `|`:
    //
    // ```text
    // fun gcd : Int -> Int -> Int
    //   | gcd a 0 = a
    //   | gcd a b = gcd b (a % b)
    // ```
    //
    // Signature and clauses are *one* declaration -- what ML writes as one
    // `let`, rather than Haskell's type line followed by a second definition of
    // the same name. It compiles to two nodes, a `Sig` and a `Bind`, because
    // that is what a signature means downstream; [`joined`] is what makes sure
    // nobody wrote those two nodes by hand.
    let sig_decl = just(Token::Fun)
        .to(true)
        .or(just(Token::Def).to(false))
        .then(value_ident())
        .then_ignore(just(Token::Colon))
        .then(context())
        .then(ty())
        // The signature's own span, so that it ends where the type does rather
        // than running to the bottom of the definition.
        .map_with(|(((is_fun, name), bounds), ty), e| {
            (
                is_fun,
                name.clone(),
                LDecl::new(Decl::Sig(name, ty, bounds), e.span()),
            )
        })
        .then(clause().repeated().collect::<Vec<_>>())
        .validate(|((is_fun, name, sig), clauses), e, emitter| {
            // No clauses: a signature and nothing under it. Resolution reports
            // it, where it knows whether the name is defined at all.
            match defined_by(is_fun, name, clauses, e.span(), emitter) {
                None => vec![sig],
                Some(bind) => vec![sig, LDecl::new(Decl::Bind(bind), e.span())],
            }
        });

    // `trait Name a <: Super a { type Assoc a  fun m : T  fun m : T | m x = … }`.
    // Every item starts with a keyword, so the body needs no separators.
    //
    // A method with no default is a signature on its own -- which is most of
    // what a trait holds, and is unchanged. A default is not a second item
    // that happens to share the method's name: it belongs to the signature,
    // and is written under it in the clauses any definition is written in.
    let trait_item = choice((
        just(Token::Type)
            .ignore_then(upper_ident())
            .then(lower_ident().repeated().at_least(1).collect::<Vec<_>>())
            .map(|(name, params)| vec![TraitItem::Assoc(name, params)]),
        // `effect Reading r`: an effect each implementation chooses.
        just(Token::Effect)
            .ignore_then(upper_ident())
            .then(lower_ident().repeated().at_least(1).collect::<Vec<_>>())
            .map(|(name, params)| vec![TraitItem::Effect(name, params)]),
        bind_decl.clone().map(|b| vec![TraitItem::Default(b)]),
        // No `context()` here: a method's own bounds are the trait's, so a
        // `=>` in front of its type has nothing to say.
        just(Token::Fun)
            .to(true)
            .or(just(Token::Def).to(false))
            .then(value_ident())
            .then_ignore(just(Token::Colon))
            .then(ty())
            .then(clause().repeated().collect::<Vec<_>>())
            .validate(|(((is_fun, name), ty), clauses), e, emitter| {
                let sig = TraitItem::Sig(name.clone(), ty);
                match defined_by(is_fun, name, clauses, e.span(), emitter) {
                    None => vec![sig],
                    Some(bind) => vec![sig, TraitItem::Default(bind)],
                }
            }),
    ));
    let trait_decl = just(Token::Trait)
        .ignore_then(upper_ident())
        .then(lower_ident().repeated().at_least(1).collect::<Vec<_>>())
        .then(supertraits())
        .then(
            trait_item
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .validate(|(((name, params), supers), groups), e, emitter| {
            methods(&groups, emitter);
            let mut decl = TraitDecl {
                name,
                params,
                supers,
                assocs: Vec::new(),
                effects: Vec::new(),
                sigs: Vec::new(),
                defaults: Vec::new(),
            };
            for item in groups.into_iter().flatten() {
                match item {
                    TraitItem::Assoc(n, p) => decl.assocs.push((n, p)),
                    TraitItem::Effect(n, p) => decl.effects.push((n, p)),
                    TraitItem::Sig(n, t) => decl.sigs.push((n, t)),
                    TraitItem::Default(b) => decl.defaults.push(b),
                }
            }
            LDecl::new(Decl::Trait(decl), e.span())
        });

    // `impl Name Type where Bound a { type Assoc Type = T  fun m x = … }`
    //
    // No signature among the items, and so nothing here to join: an `impl`
    // says what its methods *do*, and what they are is the trait's to declare.
    // `fun m : T` inside one reads as a method whose result type is `T`, still
    // waiting for its `=`.
    let impl_item = choice((
        just(Token::Type)
            .ignore_then(upper_ident())
            .then(ty_atom().repeated().at_least(1).collect::<Vec<_>>())
            .then_ignore(just(Token::Eq))
            .then(ty())
            .map(|((name, at), is)| ImplItem::Assoc(name, at, is)),
        // `effect Reading (Writing s) = { St s }`
        just(Token::Effect)
            .ignore_then(upper_ident())
            .then(ty_atom().repeated().at_least(1).collect::<Vec<_>>())
            .then_ignore(just(Token::Eq))
            .then(effect_row())
            .map(|((name, at), is)| ImplItem::Effect(name, at, is)),
        bind_decl.clone().map(ImplItem::Method),
    ));
    let impl_decl = just(Token::Impl)
        .ignore_then(upper_ident())
        .then(head_ty().repeated().at_least(1).collect::<Vec<_>>())
        .then(bounds())
        .then(
            impl_item
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map_with(|(((tr, tys), context), items), e| {
            let mut decl = ImplDecl {
                tr,
                tys,
                context,
                assocs: Vec::new(),
                effects: Vec::new(),
                methods: Vec::new(),
            };
            for item in items {
                match item {
                    ImplItem::Assoc(n, at, is) => decl.assocs.push((n, at, is)),
                    ImplItem::Effect(n, at, is) => decl.effects.push((n, at, is)),
                    ImplItem::Method(b) => decl.methods.push(b),
                }
            }
            LDecl::new(Decl::Impl(decl), e.span())
        });

    // `infixl 6 +, -` / `infixr 5 ++` / `infix 4 ==`: an operator bare, or in
    // the parentheses it is named with elsewhere.
    let fixity_decl = choice((
        just(Token::Infixl).to(Assoc::Left),
        just(Token::Infixr).to(Assoc::Right),
        just(Token::Infix).to(Assoc::None),
    ))
    .then(select! { Token::Int(n) => n })
    .then(
        operator()
            .or(operator().delimited_by(just(Token::LParen), just(Token::RParen)))
            .separated_by(just(Token::Comma))
            .at_least(1)
            .collect::<Vec<_>>(),
    )
    .map_with(|((assoc, level), ops), e| {
        LDecl::new(
            Decl::Fixity(assoc, level.clamp(0, 255) as u8, ops),
            e.span(),
        )
    });

    let one = choice((
        fixity_decl,
        mod_decl,
        use_decl,
        data_decl,
        record_decl,
        effect_decl,
        type_decl,
        trait_decl,
        impl_decl,
        macro_decl().map_with(|m, e| LDecl::new(Decl::Macro(m), e.span())),
        // Before `sig_decl`, which also starts with a name: `derive! { … }`
        // would otherwise be read as the start of `derive : T`.
        mac_call(true).map_with(|m, e| LDecl::new(Decl::MacCall(m), e.span())),
        bind_decl.map_with(|bind, e| LDecl::new(Decl::Bind(bind), e.span())),
    ));

    // `pattern P x y = p` / `pattern P x y <- p [where P a b = e]` -- a
    // pattern synonym, as the functions it compiles to: see `synonym`.
    // `pattern` is a keyword only here, before a capitalized name: elsewhere
    // it is an ordinary name.
    let synonym_decl = upper_ident()
        .then(value_ident().repeated().collect::<Vec<_>>())
        .then(just(Token::Eq).to(true).or(just(Token::LArrow).to(false)))
        .then(pat())
        .then(
            just(Token::Where)
                .ignore_then(upper_ident())
                .then(param_pat().repeated().collect::<Vec<_>>())
                .then_ignore(just(Token::Eq))
                .then(expr())
                .or_not(),
        )
        .validate(|((((name, params), both), rhs), builder), e, emitter| {
            synonym(name, params, both, rhs, builder, e.span(), emitter)
        });
    // `pattern A | B | C` -- synonyms that together match every value of
    // their type, as GHC's `COMPLETE` says: see `complete`.
    let complete_decl = upper_ident()
        .separated_by(just(Token::Bar))
        .at_least(1)
        .collect::<Vec<_>>()
        .map_with(|names, e| vec![complete(names, e.span())]);
    let pattern_decl = just(Token::LowerIdent(InternedString::from("pattern")))
        .ignore_then(synonym_decl.or(complete_decl));

    // `sig_decl` last, and alone in giving more than one node back.
    one.map(|d| vec![d]).or(pattern_decl).or(sig_decl)
}

/// The functions a pattern synonym `pattern P x y <- p` is.
///
/// Its **matcher**, `$m2P` -- the number is how many parameters it has --
/// matches its argument against `p` and answers what `p` bound, as a list of
/// one tuple, `[(x, y);]`, or `[;]` when it does not match: of one value
/// `[x;]`, and of none `True` or `False`. A use of it, `P a b`, is the view
/// `($m2P -> [(a, b);])`; with the matcher inlined the list and the tuple are
/// taken apart where they are made, and nothing is built. A list, rather than
/// `Maybe`, because it is the compiler's own and there without a library.
///
/// Its **builder**, `$bP`, is what `P e1 e2` calls in an expression: `p`
/// read as an expression, when it is written `pattern P x y = p`, or the
/// equation after `where`. `pattern P x y <- p` alone has none, and only
/// matches.
///
/// Both are ordinary functions, found, imported and exported as any are.
fn synonym<'a>(
    name: Ident,
    params: Vec<Ident>,
    both: bool,
    rhs: LPat,
    builder: Option<((Ident, Vec<LPat>), LExpr)>,
    span: Span,
    emitter: &mut chumsky::input::Emitter<Rich<'a, Token, Span>>,
) -> Vec<LDecl> {
    // What `p` binds is what the synonym's parameters are, each once.
    let mut bound = Vec::new();
    pat_binds(&rhs, &mut bound);
    for p in &params {
        match bound.iter().filter(|b| b.value() == p.value()).count() {
            1 => {}
            0 => emitter.emit(Rich::custom(
                p.span,
                format!(
                    "`{}` is a parameter of `{}` that its pattern does not bind",
                    p.value(),
                    name.value()
                ),
            )),
            _ => emitter.emit(Rich::custom(
                p.span,
                format!(
                    "`{}` is bound more than once in the pattern of `{}`",
                    p.value(),
                    name.value()
                ),
            )),
        }
    }
    for b in &bound {
        if !params.iter().any(|p| p.value() == b.value()) {
            emitter.emit(Rich::custom(
                b.span,
                format!(
                    "`{}` is bound by the pattern of `{}` but is not one of its parameters",
                    b.value(),
                    name.value()
                ),
            ));
        }
    }
    let at = |e: Expr| Located::new(e, span);
    let ident = |s: String| Ident::new(InternedString::from(s.as_str()), name.span);
    let vars: Vec<LExpr> = params
        .iter()
        .map(|p| Located::new(Expr::Var(p.clone()), p.span))
        .collect();
    let (yes, no) = match vars.len() {
        0 => (
            at(Expr::Cons(ident("True".into()), vec![])),
            at(Expr::Cons(ident("False".into()), vec![])),
        ),
        1 => (at(Expr::List(vars)), at(Expr::List(vec![]))),
        _ => (
            at(Expr::List(vec![at(Expr::Tuple(vars))])),
            at(Expr::List(vec![])),
        ),
    };
    let scrutinee = ident("$scrutinee".into());
    let matcher = Bind::Fun(
        ident(format!("$m{}{}", params.len(), name.value())),
        vec![Located::new(Pat::Var(scrutinee.clone()), name.span)],
        None,
        at(Expr::Match(
            at(Expr::Var(scrutinee)),
            vec![
                (rhs.clone(), None, yes),
                (Located::new(Pat::Wildcard, span), None, no),
            ],
        )),
    );
    let mut out = vec![LDecl::new(Decl::Bind(matcher), span)];
    let builder_name = ident(format!("$b{}", name.value()));
    let built = match builder {
        Some(((again, args), body)) => {
            if both {
                emitter.emit(Rich::custom(
                    again.span,
                    format!(
                        "`{}` builds as its pattern reads: write `pattern {} … <- …` to give it a `where` of its own",
                        name.value(),
                        name.value()
                    ),
                ));
            }
            if again.value() != name.value() {
                emitter.emit(Rich::custom(
                    again.span,
                    format!(
                        "the equation under `pattern {}` defines `{}`",
                        name.value(),
                        again.value()
                    ),
                ));
            }
            Some((args, body))
        }
        None if both => match pat_expr(&rhs) {
            Ok(body) => Some((
                params
                    .iter()
                    .map(|p| Located::new(Pat::Var(p.clone()), p.span))
                    .collect(),
                body,
            )),
            Err(at) => {
                emitter.emit(Rich::custom(
                    at,
                    format!(
                        "`{}` cannot be built from its pattern, which this part of cannot be read as a value: write `pattern {} … <- …`, with a `where` to say how it is built",
                        name.value(),
                        name.value()
                    ),
                ));
                None
            }
        },
        None => None,
    };
    if let Some((args, body)) = built {
        out.push(LDecl::new(
            Decl::Bind(Bind::Fun(builder_name, args, None, body)),
            span,
        ));
    }
    out
}

/// `pattern A | B | C`: which synonyms cover their type together. What the
/// exhaustiveness check reads -- a `match` with an arm for each of them, each
/// matching whatever it binds, is exhaustive -- as a definition nothing uses,
/// `$complete:A|B|C`, of their matchers: `$m?A` is the matcher of `A`,
/// however many parameters it has.
fn complete(names: Vec<Ident>, span: Span) -> LDecl {
    let matchers: Vec<LExpr> = names
        .iter()
        .map(|n| {
            let m = Ident::new(
                InternedString::from(format!("$m?{}", n.value()).as_str()),
                n.span,
            );
            Located::new(Expr::Var(m), n.span)
        })
        .collect();
    let value = match matchers.len() {
        1 => matchers.into_iter().next().expect("one"),
        _ => Located::new(Expr::Tuple(matchers), span),
    };
    let name = Ident::new(
        InternedString::from(
            format!(
                "$complete:{}",
                names
                    .iter()
                    .map(|n| n.value().to_string())
                    .collect::<Vec<_>>()
                    .join("|")
            )
            .as_str(),
        ),
        span,
    );
    LDecl::new(
        Decl::Bind(Bind::Pat(Located::new(Pat::Var(name), span), value)),
        span,
    )
}

/// Every name a pattern binds, in order.
fn pat_binds(p: &LPat, out: &mut Vec<Ident>) {
    match p.value() {
        Pat::Var(n) => out.push(n.clone()),
        Pat::As(n, inner) => {
            out.push(n.clone());
            pat_binds(inner, out);
        }
        Pat::Ann(inner, _) => pat_binds(inner, out),
        Pat::View(_, inner) => pat_binds(inner, out),
        Pat::Cons(_, ps)
        | Pat::QualCons(_, _, ps)
        | Pat::Tuple(ps)
        | Pat::Array(ps)
        | Pat::Vector(ps)
        | Pat::List(ps) => ps.iter().for_each(|q| pat_binds(q, out)),
        Pat::Record(fields, _) => fields.iter().for_each(|(_, q)| pat_binds(q, out)),
        Pat::Wildcard | Pat::Lit(_) | Pat::Unit | Pat::MacCall(_) => {}
    }
}

/// A pattern read as the expression that builds what it matches, or where it
/// cannot be.
fn pat_expr(p: &LPat) -> Result<LExpr, Span> {
    let all = |ps: &[LPat]| ps.iter().map(pat_expr).collect::<Result<Vec<_>, _>>();
    let e = match p.value() {
        Pat::Var(n) => Expr::Var(n.clone()),
        Pat::Lit(l) => Expr::Lit(l.clone()),
        Pat::Unit => Expr::Unit,
        Pat::Ann(inner, _) => return pat_expr(inner),
        Pat::Cons(n, ps) => Expr::Cons(n.clone(), all(ps)?),
        Pat::Tuple(ps) => Expr::Tuple(all(ps)?),
        Pat::Array(ps) => Expr::Array(all(ps)?),
        Pat::List(ps) => Expr::List(all(ps)?),
        Pat::Record(fields, false) => Expr::Record(
            fields
                .iter()
                .map(|(l, q)| Ok((l.clone(), pat_expr(q)?)))
                .collect::<Result<Vec<_>, Span>>()?,
            None,
        ),
        _ => return Err(p.span),
    };
    Ok(Located::new(e, p.span))
}

/// One equation of a function written in several: `| gcd a b = …`.
///
/// The name is written again, as it is in Haskell, so that a typo in it is
/// caught rather than quietly defining something else.
fn clause<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Clause, extra::Err<Rich<'a, Token, Span>>> + Clone {
    just(Token::Bar)
        .ignore_then(value_ident())
        .then(param_pat().repeated().collect::<Vec<_>>())
        .then_ignore(just(Token::Eq))
        .then(expr())
        .map(|((name, args), body)| Clause { name, args, body })
}

/// The binding the clauses under a signature define, or `None` where there are
/// none and the signature stands alone.
///
/// Shared by a top-level declaration and a trait's method, which are written
/// alike: the signature, then its clauses. `name` is the signature's, and is
/// the one the binding takes -- so a clause that names something else is
/// reported and then measured against the signature anyway, rather than
/// quietly defining whatever it said.
fn defined_by<'a>(
    is_fun: bool,
    name: Ident,
    clauses: Vec<Clause>,
    span: Span,
    emitter: &mut chumsky::input::Emitter<Rich<'a, Token, Span>>,
) -> Option<Bind> {
    let mut clauses = clauses.into_iter();
    let head = clauses.next()?;
    if head.name.value() != name.value() {
        emitter.emit(Rich::custom(
            head.name.span,
            format!(
                "this equation defines `{}`, but the signature above it is for `{}`",
                spelled(head.name.value()),
                spelled(name.value())
            ),
        ));
    }
    let rest = clauses.collect::<Vec<_>>();
    Some(if is_fun && head.args.is_empty() && rest.is_empty() {
        // The one case `equations` would get wrong: a `fun` with no parameters
        // is still a function, of nothing but the trait dictionaries its type
        // may need, so it keeps the `Bind::Fun` shape that a `def` of the same
        // shape does not (see `fun_bind`).
        Bind::Fun(name, Vec::new(), None, head.body)
    } else {
        equations(name, head.args, None, head.body, rest, span, emitter)
    })
}

/// An item of an `impl` body: what an associated type is, what an associated
/// effect is, or a method.
enum ImplItem {
    Assoc(Ident, Vec<LType>, LType),
    Effect(Ident, Vec<LType>, EffectRow),
    Method(Bind),
}

/// An item of a `trait` body: an associated type, a method's signature, or the
/// default a method carries. One written item is one or two of these -- two
/// only when a signature has clauses under it -- which is what lets [`methods`]
/// see how the trait was written.
enum TraitItem {
    Assoc(Ident, Vec<Ident>),
    Effect(Ident, Vec<Ident>),
    Sig(Ident, LType),
    Default(Bind),
}

/// Reject a method and its default written as two items of a trait.
///
/// ```text
/// fun showTokens : s -> [Token s] -> String
/// fun showTokens input ts = S.join " " (V.map (showToken input) ts)
/// ```
///
/// A trait mostly holds methods with no default at all, and those stay
/// signatures on their own -- that is the declaration, and nothing is missing
/// from it. A *default* is part of the method rather than a second item of the
/// same name, so it goes under the signature in clauses:
///
/// ```text
/// fun showTokens : s -> [Token s] -> String
///   | showTokens input ts = S.join " " (V.map (showToken input) ts)
/// ```
///
/// Told apart the way [`joined`] tells the top-level pair apart: one written
/// item is one group, so a signature and a default that arrived in separate
/// groups were written as two items.
fn methods<'a>(
    groups: &[Vec<TraitItem>],
    emitter: &mut chumsky::input::Emitter<Rich<'a, Token, Span>>,
) {
    let mut sigs: Vec<(InternedString, Span, usize)> = Vec::new();
    let mut defaults: Vec<(InternedString, Span, usize)> = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        let [only] = &group[..] else { continue };
        match only {
            TraitItem::Sig(name, _) => sigs.push((*name.value(), name.span, i)),
            TraitItem::Default(bind) => {
                if let Some(name) = bound_name(bind) {
                    defaults.push((*name.value(), name.span, i));
                }
            }
            TraitItem::Assoc(..) | TraitItem::Effect(..) => {}
        }
    }
    for (name, sig_span, sig_at) in &sigs {
        for (_, default_span, default_at) in defaults.iter().filter(|(n, ..)| n == name) {
            let span = if default_at > sig_at {
                *default_span
            } else {
                *sig_span
            };
            let name = spelled(name);
            emitter.emit(Rich::custom(
                span,
                format!(
                    "`{name}` is named twice in this trait -- a method and its default are one \
                     item, so write the default as `| {name} … = …` under the signature rather \
                     than declaring the method again"
                ),
            ));
        }
    }
}

/// Flatten a run of written declarations, rejecting a signature and a
/// definition of the same name written as two declarations.
///
/// ```text
/// fun showItem : VisualStream s => ErrorItem s -> String
/// fun showItem item = …
/// ```
///
/// is Haskell's shape, and Meadow does not have it: a signature and the clauses
/// it stands over are one declaration, written
///
/// ```text
/// fun showItem : VisualStream s => ErrorItem s -> String
///   | showItem item = …
/// ```
///
/// Told apart here, and only here, because this is the last place that still
/// knows what was written: [`decl`] hands back the nodes of *one* declaration,
/// so a pair that arrived in two groups was two declarations, while the joined
/// form arrives as one group of two. Flattening loses that, which is why the
/// check happens on the way through.
fn joined<'a>(
    groups: Vec<Vec<LDecl>>,
    emitter: &mut chumsky::input::Emitter<Rich<'a, Token, Span>>,
) -> Vec<LDecl> {
    // Where each name was given a signature of its own, and where it was
    // defined of its own -- both by position in the run, so the error can point
    // at whichever of the two came second.
    let mut sigs: Vec<(InternedString, Span, usize)> = Vec::new();
    let mut defs: Vec<(InternedString, Span, usize)> = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        let [only] = &group[..] else { continue };
        match bare(only) {
            Decl::Sig(name, ..) => sigs.push((*name.value(), name.span, i)),
            Decl::Bind(bind) => {
                if let Some(name) = bound_name(bind) {
                    defs.push((*name.value(), name.span, i));
                }
            }
            _ => {}
        }
    }
    for (name, sig_span, sig_at) in &sigs {
        for (_, def_span, def_at) in defs.iter().filter(|(n, ..)| n == name) {
            let span = if def_at > sig_at {
                *def_span
            } else {
                *sig_span
            };
            let name = spelled(name);
            emitter.emit(Rich::custom(
                span,
                format!(
                    "`{name}` is declared twice -- a signature and the clauses that define it are \
                     one declaration, so write `| {name} … = …` under the signature rather than \
                     naming `{name}` again"
                ),
            ));
        }
    }
    groups.into_iter().flatten().collect()
}

/// A declaration with its attributes taken off.
fn bare(decl: &LDecl) -> &Decl {
    match decl.value() {
        Decl::Attributed(_, inner) => bare(inner),
        d => d,
    }
}

/// The name a binding defines, when it defines exactly one.
fn bound_name(bind: &Bind) -> Option<&Ident> {
    match bind {
        Bind::Fun(name, ..) => Some(name),
        Bind::Pat(p, _) => match p.value() {
            Pat::Var(name) => Some(name),
            Pat::Ann(inner, _) => match inner.value() {
                Pat::Var(name) => Some(name),
                _ => None,
            },
            _ => None,
        },
    }
}

/// A type after a trait's name, in an `impl` or a `where`: an atom, but never
/// one that starts with `{` -- that brace opens the body that follows, and an
/// empty body would otherwise read as an empty record type.
fn head_ty<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, LType, extra::Err<Rich<'a, Token, Span>>> + Clone {
    // A row in braces is what a trait is given for a parameter that is an
    // effect -- `impl Rows (Writing s) { St s | e } { … }`. The `impl`'s own
    // body is in braces too, and comes last, so a row is one that something
    // of the head still follows: another row, the body, a `where`, or what
    // ends a bound.
    let label = upper_ident().then(ty_atom().repeated().collect::<Vec<_>>());
    let row = label
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .then(just(Token::Bar).ignore_then(lower_ident()).or_not())
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
        .then_ignore(
            choice((
                just(Token::LBrace),
                just(Token::Where),
                just(Token::Comma),
                just(Token::FatArrow),
                just(Token::RParen),
            ))
            .rewind(),
        )
        .map_with(|(labels, tail), e| {
            Located::new(TypeExpr::Row(EffectRow { labels, tail }), e.span())
        });
    choice((row, just(Token::LBrace).not().ignore_then(ty_atom())))
}

/// One trait some types have to implement: `Show a`, `Convert a (List b)`.
fn bound<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Bound, extra::Err<Rich<'a, Token, Span>>> + Clone {
    upper_ident()
        .then(head_ty().repeated().at_least(1).collect::<Vec<_>>())
        .map(|(tr, tys)| Bound { tr, tys })
}

/// `Show a =>` / `(Show a, Ord b) =>` in front of a signature's type -- the
/// traits its variables have to implement; empty when there is no `=>`. What
/// comes before the arrow is only known to be a context once the arrow is
/// there, so without one this reads nothing and the type is read from the
/// start.
fn context<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Vec<Bound>, extra::Err<Rich<'a, Token, Span>>> + Clone {
    choice((
        bound().map(|b| vec![b]),
        bound()
            .separated_by(just(Token::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen)),
    ))
    .then_ignore(just(Token::FatArrow))
    .or_not()
    .map(Option::unwrap_or_default)
}

/// `<: Eq a, Show a` after a trait's head -- the traits it requires, its
/// supertraits; empty when there is no `<:`.
fn supertraits<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Vec<Bound>, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! { Token::OpIdent(s) if &*s == "<:" => () }
        .ignore_then(
            bound()
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .or_not()
        .map(Option::unwrap_or_default)
}

/// `where Show a, Ord (f b)` after an `impl` head -- the traits the type's own
/// parameters have to implement; empty when there is no `where`.
fn bounds<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Vec<Bound>, extra::Err<Rich<'a, Token, Span>>> + Clone {
    just(Token::Where)
        .ignore_then(
            bound()
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .or_not()
        .map(Option::unwrap_or_default)
}

/// `{ @pub name : Type, age : Type, }` — shared by `record` decls, named variant
/// fields, and `effect` operation lists. Each field may carry leading attributes.
fn field_list<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Vec<Field>, extra::Err<Rich<'a, Token, Span>>> + Clone {
    attr()
        .repeated()
        .collect::<Vec<_>>()
        .then(lower_ident())
        .then_ignore(just(Token::Colon))
        .then(ty())
        .map(|((attrs, name), ty)| Field { attrs, name, ty })
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .at_least(1)
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
}

/// A full type expression: `a`, `Vector a`, `Maybe (Vector Int)`, `(a, b)`, `[a]`,
/// `a -> b` (right-associative).
fn ty<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, LType, extra::Err<Rich<'a, Token, Span>>> + Clone {
    recursive(|ty| {
        let unit = just(Token::LParen)
            .then(just(Token::RParen))
            .map_with(|_, e| {
                Located::new(
                    TypeExpr::Con(Ident::new(InternedString::from("Unit"), e.span()), vec![]),
                    e.span(),
                )
            });

        // `[a]` is a `Vector`; `[a;]` is a `List`. The `;` is the same marker the
        // `[x; y]` literal uses, so the type reads like the values it holds.
        let seq = ty
            .clone()
            .then(just(Token::SemiColon).or_not())
            .delimited_by(just(Token::LBrack), just(Token::RBrack))
            .map_with(|(t, semi), e| {
                let kind = match semi {
                    Some(_) => TypeExpr::List(t),
                    None => TypeExpr::Vector(t),
                };
                Located::new(kind, e.span())
            });

        let paren_or_tuple = ty
            .clone()
            .separated_by(just(Token::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map_with(|mut ts, e| {
                if ts.len() == 1 {
                    ts.pop().unwrap()
                } else {
                    Located::new(TypeExpr::Tuple(ts), e.span())
                }
            });

        // `#[a]`, the builtin `Array` -- the way an array's type prints.
        let array = just(Token::Hash)
            .then(just(Token::LBrack))
            .ignore_then(ty.clone())
            .then_ignore(just(Token::RBrack))
            .map_with(|t, e| {
                Located::new(
                    TypeExpr::Con(Ident::new(InternedString::from("Array"), e.span()), vec![t]),
                    e.span(),
                )
            });

        let tvar = lower_ident().map_with(|n, e| Located::new(TypeExpr::Var(n), e.span()));
        let tcon0 = type_name().map_with(|n, e| Located::new(TypeExpr::Con(n, vec![]), e.span()));
        let record = record_ty(ty.clone());

        // A macro call written at the start of a line begins a declaration
        // (see [`Token::DeclBang`]): its name is not one more field of the
        // `data` above it.
        let line_call = path_seg()
            .separated_by(just(Token::Period))
            .at_least(1)
            .then(just(Token::DeclBang))
            .ignored();
        let atom = line_call.not().ignore_then(choice((
            unit,
            array,
            seq,
            paren_or_tuple,
            record,
            tvar,
            tcon0,
        )));

        let app = type_name()
            .then(atom.clone().repeated().at_least(1).collect::<Vec<_>>())
            .map_with(|(n, args), e| Located::new(TypeExpr::Con(n, args), e.span()));

        let head = choice((app, atom.clone()));

        // effect annotation after `!` — reuses the local `atom` for label args,
        // so it must live inside this `recursive` closure (no separate fn).
        let eff_label = upper_ident().then(atom.clone().repeated().collect::<Vec<_>>());
        let eff_braced = eff_label
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .then(just(Token::Bar).ignore_then(lower_ident()).or_not())
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(|(labels, tail)| EffectRow { labels, tail });
        let eff_bare_var = lower_ident().map(|n| EffectRow {
            labels: vec![],
            tail: Some(n),
        });
        let eff_bare_label = eff_label.map(|l| EffectRow {
            labels: vec![l],
            tail: None,
        });
        let eff_row = choice((eff_braced, eff_bare_var, eff_bare_label));

        head.clone()
            .then(
                just(Token::RArrow)
                    .ignore_then(ty.clone())
                    .then(just(Token::Bang).ignore_then(eff_row).or_not())
                    .or_not(),
            )
            .map_with(|(l, r), e| match r {
                Some((rhs, eff)) => Located::new(TypeExpr::Fun(vec![l], rhs, eff), e.span()),
                None => l,
            })
    })
}

/// A single type *atom* — used for a `data` variant's positional fields, where
/// `Leaf (Vector a) Int` is two fields, not one application.
fn ty_atom<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, LType, extra::Err<Rich<'a, Token, Span>>> + Clone {
    let inner = ty();
    let unit = just(Token::LParen)
        .then(just(Token::RParen))
        .map_with(|_, e| {
            Located::new(
                TypeExpr::Con(Ident::new(InternedString::from("Unit"), e.span()), vec![]),
                e.span(),
            )
        });
    let seq = inner
        .clone()
        .then(just(Token::SemiColon).or_not())
        .delimited_by(just(Token::LBrack), just(Token::RBrack))
        .map_with(|(t, semi), e| {
            let kind = match semi {
                Some(_) => TypeExpr::List(t),
                None => TypeExpr::Vector(t),
            };
            Located::new(kind, e.span())
        });
    let paren_or_tuple = inner
        .clone()
        .separated_by(just(Token::Comma))
        .at_least(1)
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen))
        .map_with(|mut ts, e| {
            if ts.len() == 1 {
                ts.pop().unwrap()
            } else {
                Located::new(TypeExpr::Tuple(ts), e.span())
            }
        });
    // `#[a]`, the builtin `Array` -- the way an array's type prints.
    let array = just(Token::Hash)
        .then(just(Token::LBrack))
        .ignore_then(inner.clone())
        .then_ignore(just(Token::RBrack))
        .map_with(|t, e| {
            Located::new(
                TypeExpr::Con(Ident::new(InternedString::from("Array"), e.span()), vec![t]),
                e.span(),
            )
        });
    let tvar = lower_ident().map_with(|n, e| Located::new(TypeExpr::Var(n), e.span()));
    let tcon0 = type_name().map_with(|n, e| Located::new(TypeExpr::Con(n, vec![]), e.span()));
    // A variant's own `{ ... }` is its named fields, and `variant` tries that
    // first; a positional field of record type is written in parentheses.
    let record = record_ty(inner.clone());
    // A macro call on the next line is a declaration, not one more field.
    let line_call = path_seg()
        .separated_by(just(Token::Period))
        .at_least(1)
        .then(just(Token::DeclBang))
        .ignored();
    line_call.not().ignore_then(choice((
        unit,
        array,
        seq,
        paren_or_tuple,
        record,
        tvar,
        tcon0,
    )))
}

/// `{ name : String, age : Int }` or `{ name : String | r }` -- a structural
/// record type, spelled the way one is printed.
///
/// `{}` is the empty record, and `{ | r }` is any record at all. The tail is a
/// row variable, told apart from an ordinary one only by standing here.
fn record_ty<'a, I, P>(
    ty: P,
) -> impl Parser<'a, I, LType, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
    P: Parser<'a, I, LType, extra::Err<Rich<'a, Token, Span>>> + Clone,
{
    lower_ident()
        .then_ignore(just(Token::Colon))
        .then(ty)
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .then(just(Token::Bar).ignore_then(lower_ident()).or_not())
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
        .map_with(|(fields, tail), e| Located::new(TypeExpr::Record(fields, tail), e.span()))
}

/// A type's name: `Ty`, or `M.Ty` -- the type `Ty` of the module `M`
/// qualifies, as `M.f` is its function `f`. A path is kept as one name, dots
/// and all, as [`qualifier`] keeps one.
fn type_name<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    upper_ident()
        .separated_by(just(Token::Period))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|segs: Vec<Ident>| {
            if let [only] = &segs[..] {
                return only.clone();
            }
            let text = segs
                .iter()
                .map(|s| s.value().to_string())
                .collect::<Vec<_>>()
                .join(".");
            let span = segs[1..]
                .iter()
                .fold(segs[0].span, |acc, s| acc.extend(s.span));
            Ident::new(InternedString::from(text), span)
        })
}

/// A qualifier: `Mod`, or a path of them, `Core.Expr`, each followed by its
/// `.`. A path is kept as one name, dots and all, which is what the resolver
/// knows a module under another by.
fn qualifier<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    upper_ident()
        .then_ignore(just(Token::Period))
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|segs: Vec<Ident>| {
            if let [only] = &segs[..] {
                return only.clone();
            }
            let text = segs
                .iter()
                .map(|s| s.value().to_string())
                .collect::<Vec<_>>()
                .join(".");
            let span = segs[1..]
                .iter()
                .fold(segs[0].span, |acc, s| acc.extend(s.span));
            Ident::new(InternedString::from(text), span)
        })
}

fn path_seg<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! {
        Token::LowerIdent(name) => name,
        Token::UpperIdent(name) => name,
    }
    .map_with(|name, e| Ident::new(name, e.span()))
}

/// A bracketed run of token trees, and where its brackets were.
fn group<'a, I, P>(
    tree: P,
    delim: tt::Delim,
) -> impl Parser<'a, I, tt::Group, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
    P: Parser<'a, I, tt::TokenTree, extra::Err<Rich<'a, Token, Span>>> + Clone,
{
    just(delim.open())
        .map_with(|_, e| e.span())
        .then(tree.repeated().collect::<Vec<_>>())
        .then(just(delim.close()).map_with(|_, e| e.span()))
        .map(move |((open, trees), close)| tt::Group {
            delim,
            trees,
            open,
            close,
        })
}

/// One token tree: a token, or a bracketed run of them.
///
/// Balanced brackets are the only structure required of a macro's argument, so
/// this accepts any token that is not one — the argument need not be an
/// expression, or mean anything at all until the macro is expanded.
fn token_tree<'a, I>()
-> impl Parser<'a, I, tt::TokenTree, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
{
    recursive(|tree| {
        let bracketed = choice((
            group(tree.clone(), tt::Delim::Paren),
            group(tree.clone(), tt::Delim::Brack),
            group(tree, tt::Delim::Brace),
        ))
        .map(tt::TokenTree::Group);
        let single = any()
            .filter(|t: &Token| {
                tt::Delim::opened_by(t).is_none() && tt::Delim::closed_by(t).is_none()
            })
            .map_with(|t, e| tt::TokenTree::Token(LToken::new(t, e.span())));
        bracketed.or(single)
    })
}

/// A bracketed run of token trees in any of the three brackets: what both
/// sides of a macro rule are written with, and what a call's argument is.
fn any_group<'a, I>() -> impl Parser<'a, I, tt::Group, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
{
    choice((
        group(token_tree(), tt::Delim::Paren),
        group(token_tree(), tt::Delim::Brack),
        group(token_tree(), tt::Delim::Brace),
    ))
}

/// A macro definition:
///
/// ```text
/// macro swap
///   | ($a, $b) -> { ($b, $a) }
/// ```
///
/// Rules are written the way a `match`'s arms are, and are tried in the same
/// order. Both sides are token trees, so nothing here reads what they mean.
fn macro_decl<'a, I>() -> impl Parser<'a, I, MacroDef, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
{
    let rule = just(Token::Bar)
        .ignore_then(any_group())
        .then_ignore(just(Token::RArrow))
        .then(any_group())
        .map(|(matcher, template)| MacroRule { matcher, template });
    just(Token::Macro)
        .ignore_then(lower_ident())
        .then(rule.repeated().at_least(1).collect::<Vec<_>>())
        .map(|(name, rules)| MacroDef { name, rules })
}

/// A macro call: `assertEq!(got, want)`, `vec![1; 2]`, `config! { … }`, and
/// qualified, `Std.Test.assertEq!(…)`.
///
/// The three brackets mean the same thing; which one to use is a question of
/// how the call reads. Nothing here looks inside them.
///
/// `starts` is whether the call may be one written at the start of a line
/// ([`Token::DeclBang`]): as a declaration or the head of an expression it
/// may, and as an argument it may not -- so a call on a line of its own
/// begins a new declaration instead of being applied to by the line above.
fn mac_call<'a, I>(
    starts: bool,
) -> impl Parser<'a, I, MacCall, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
{
    let bang = if starts {
        just(Token::Bang).or(just(Token::DeclBang)).boxed()
    } else {
        just(Token::Bang).boxed()
    };
    path_seg()
        .separated_by(just(Token::Period))
        .at_least(1)
        .collect::<Vec<_>>()
        // `!=` is one token, so `foo != x` can never be mistaken for a call.
        .then_ignore(bang)
        .then(any_group())
        .map(|(path, arg)| MacCall { path, arg })
}

fn expr<'tokens, I>()
-> impl Parser<'tokens, I, LExpr, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
{
    recursive(|expr| {
        let lit_expr = located(lit().map(Expr::Lit));
        let var_expr = located(value_ident().map(Expr::Var));
        let unit_expr = located(
            just(Token::LParen)
                .then(just(Token::RParen))
                .map(|_| Expr::Unit),
        );

        let tuple = expr
            .clone()
            .separated_by(just(Token::Comma))
            .at_least(2)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map_with(|es: Vec<LExpr>, e| Located::new(Expr::Tuple(es), e.span()))
            .boxed();

        // `[lo .. hi]` is the integers from `lo` up to, not including, `hi` --
        // `range lo hi` -- and `[lo ..= hi]` includes `hi`: `range lo (hi + 1)`.
        // As Rust's `..` and `..=` are.
        let range_list = expr
            .clone()
            .then(choice((
                just(Token::DoublePeriod),
                just(Token::DoublePeriodEq),
            )))
            .then(expr.clone())
            .delimited_by(just(Token::LBrack), just(Token::RBrack))
            .map_with(|((lo, kind), hi), e| {
                let span = e.span();
                let end = if kind == Token::DoublePeriodEq {
                    let one = Located::new(Expr::Lit(Lit::Int(1)), hi.span);
                    let plus = Ident::new(InternedString::from("+"), hi.span);
                    Located::new(Expr::Infix(hi, vec![(plus, one)]), span)
                } else {
                    hi
                };
                let range = Located::new(
                    Expr::Var(Located::new(InternedString::from("range"), span)),
                    span,
                );
                Located::new(Expr::App(range, vec![lo, end]), span)
            })
            .boxed();

        // `#[e, ...]` — a builtin `Array` literal (`Token::Hash` then `Token::LBrack`).
        let array_expr = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(
                just(Token::Hash).then(just(Token::LBrack)),
                just(Token::RBrack),
            )
            .map_with(|es, e| Located::new(Expr::Array(es), e.span()))
            .boxed();

        // `[a; b; c]` — a `List` literal. One `;` anywhere makes it a list, so a
        // single-element list is `[a;]`; without a `;` the brackets are a
        // `Vector`, which is why `[a]` cannot mean this.
        let list_expr = expr
            .clone()
            .then(
                just(Token::SemiColon)
                    .ignore_then(expr.clone().or_not())
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|(head, tail)| {
                let mut es = vec![head];
                es.extend(tail.into_iter().flatten());
                es
            })
            // `[;]` — the empty list. `[]` is the empty `Vector`.
            .or(just(Token::SemiColon).map(|_| vec![]))
            .delimited_by(just(Token::LBrack), just(Token::RBrack))
            .map_with(|es, e| Located::new(Expr::List(es), e.span()))
            .boxed();

        // `[a, b, c]` — an RRB `Vector` literal (comma-separated). Desugars to
        // `vecFromArray #[a, b, c]` (`vecFromArray` comes from the prelude).
        let vec_expr = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrack), just(Token::RBrack))
            .map_with(|es, e| {
                let span = e.span();
                let arr = Located::new(Expr::Array(es), span);
                let f = Located::new(
                    Expr::Var(Located::new(InternedString::from("vecFromArray"), span)),
                    span,
                );
                Located::new(Expr::App(f, vec![arr]), span)
            })
            .boxed();

        let bind = {
            // `let rec` is accepted but redundant: named bindings are already
            // self-recursive.
            let rec_prefix = just(Token::LowerIdent(InternedString::from("rec"))).or_not();

            let fun_bind = just(Token::Fun)
                .ignore_then(lower_ident())
                .then(
                    param_pat_in(expr.clone())
                        .repeated()
                        .at_least(1)
                        .collect::<Vec<_>>(),
                )
                .then(result_ty())
                .then_ignore(just(Token::Eq))
                .then(expr.clone())
                .map(|(((name, args), ret), body)| bind_of(name, args, ret, body));

            // `[rec] name args = body`  /  `[rec] name = body`
            let name_bind = rec_prefix
                .ignore_then(lower_ident())
                .then(param_pat_in(expr.clone()).repeated().collect::<Vec<_>>())
                .then(result_ty())
                .then_ignore(just(Token::Eq))
                .then(expr.clone())
                .map(|(((name, args), ret), body)| bind_of(name, args, ret, body));

            let pat_bind = pat_in(expr.clone())
                .then_ignore(just(Token::Eq))
                .then(expr.clone())
                .map(|(p, e)| Bind::Pat(p, e));

            choice((fun_bind, name_bind, pat_bind)).boxed()
        };

        let let_expr = just(Token::Let)
            .ignore_then(bind.clone().repeated().at_least(1).collect::<Vec<_>>())
            .then_ignore(just(Token::In))
            .then(expr.clone())
            .map(|(binds, body)| Expr::Let(binds, body))
            .map_with(|e, ex| Located::new(e, ex.span()))
            .boxed();

        let if_expr = just(Token::If)
            .ignore_then(expr.clone())
            .then_ignore(just(Token::Then))
            .then(expr.clone())
            .then_ignore(just(Token::Else))
            .then(expr.clone())
            .map(|((cond, then), else_)| Expr::If(cond, then, else_))
            .map_with(|e, ex| Located::new(e, ex.span()))
            .boxed();

        let lam_expr = just(Token::Backslash)
            .ignore_then(
                pat_in(expr.clone())
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then_ignore(just(Token::RArrow))
            .then(expr.clone())
            .map(|(params, body)| Expr::Lam(params, body))
            .map_with(|e, ex| Located::new(e, ex.span()))
            .boxed();

        // `| p -> e`, or `| p if guard -> e`, taken only where the guard holds.
        let match_arm = just(Token::Bar)
            .ignore_then(pat_in(expr.clone()))
            .then(just(Token::If).ignore_then(expr.clone()).or_not())
            .then_ignore(just(Token::RArrow))
            .then(expr.clone())
            .map(|((p, guard), body)| (p, guard, body));

        let match_expr = just(Token::Match)
            .ignore_then(expr.clone())
            .then_ignore(just(Token::With))
            .then(match_arm.repeated().at_least(1).collect::<Vec<_>>())
            .map(|(scrutinee, branches)| Expr::Match(scrutinee, branches))
            .map_with(|e, ex| Located::new(e, ex.span()))
            .boxed();

        // `{ x = e, y = e | base }`; `{ x }` is shorthand for `{ x = x }`.
        let record_field = lower_ident()
            .then(just(Token::Eq).ignore_then(expr.clone()).or_not())
            .map(|(name, val)| {
                let val = val.unwrap_or_else(|| Located::new(Expr::Var(name.clone()), name.span));
                (name, val)
            });

        // `{ r | x = e, y = e }`: `r` with those fields replaced. Tried before the
        // `{ x = e | r }` extension. A field may be a name alone here too,
        // `{ r | x, y = e }` for `{ r | x = x, y = e }`, with one exception:
        // `{ a | b }` is already the extension of `b` with a field `a = a`,
        // and stays that. So an update of one field writes its `=`.
        let update_field = lower_ident().then(just(Token::Eq).ignore_then(expr.clone()).or_not());
        let update_expr = expr
            .clone()
            .then_ignore(just(Token::Bar))
            .then(
                update_field
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .try_map(|(base, fields), span| {
                if fields.len() == 1 && fields[0].1.is_none() {
                    return Err(Rich::custom(span, "an extension, not an update"));
                }
                let fields = fields
                    .into_iter()
                    .map(|(name, val)| {
                        let val =
                            val.unwrap_or_else(|| Located::new(Expr::Var(name.clone()), name.span));
                        (name, val)
                    })
                    .collect();
                Ok(Located::new(Expr::Update(base, fields), span))
            })
            .boxed();

        let record_expr = record_field
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .then(just(Token::Bar).ignore_then(expr.clone()).or_not())
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map_with(|(fields, base), e| Located::new(Expr::Record(fields, base), e.span()))
            .boxed();

        // `handle e with { op p k -> body, return x -> body }`  (arms comma-separated)
        let handle_expr = {
            let ret_arm = just(Token::LowerIdent(InternedString::from("return")))
                .ignore_then(pat_in(expr.clone()))
                .then_ignore(just(Token::RArrow))
                .then(expr.clone())
                .map(Either::Right);
            let op_arm = lower_ident()
                .then(pat_in(expr.clone()))
                .then(lower_ident())
                .then_ignore(just(Token::RArrow))
                .then(expr.clone())
                .map(|(((op, param), resume), body)| {
                    Either::Left(HandlerArm {
                        op,
                        param,
                        resume,
                        body,
                    })
                });
            just(Token::Handle)
                .ignore_then(expr.clone())
                .then_ignore(just(Token::With))
                .then(
                    choice((ret_arm, op_arm))
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                )
                .map_with(|(scrut, arms), e| {
                    let mut ops = Vec::new();
                    let mut ret = None;
                    for a in arms {
                        match a {
                            Either::Left(arm) => ops.push(arm),
                            Either::Right((x, body)) => ret = Some((x, body)),
                        }
                    }
                    Located::new(Expr::Handle(scrut, ops, ret), e.span())
                })
                .boxed()
        };

        // A bare constructor (`Nil`, `True`) is an atom so it can be a function or
        // constructor argument; `cons` below gathers its arguments when it has any.
        let ctor_atom =
            upper_ident().map_with(|n, e| Located::new(Expr::Cons(n, vec![]), e.span()));

        // `Mod.name` / `Mod.Ctor` as a bare atom (0-ary); `qual` below gathers args.
        let qual_atom = qualifier()
            .then(choice((lower_ident(), upper_ident())))
            .map_with(|(q, n), e| Located::new(Expr::Qual(q, n), e.span()));

        // `"a ${e} b"` -- the lexer has already split the literal into its text
        // and the tokens of each hole. A hole that ends in `:?` is rendered by
        // `Debug` rather than `Display`, as in Rust.
        let hole = expr
            .clone()
            .then(
                select! { Token::ConOpIdent(s) if &*s == ":?" => () }
                    .or_not()
                    .map(|debug| {
                        if debug.is_some() {
                            Fmt::Debug
                        } else {
                            Fmt::Display
                        }
                    }),
            )
            .boxed();
        let interp_expr = select! { Token::InterpStart(s) => s }
            .then(hole.clone())
            .then(
                select! { Token::InterpMid(s) => s }
                    .then(hole)
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .then(select! { Token::InterpEnd(s) => s })
            .map_with(|(((first, hole), rest), last), e| {
                let mut texts = vec![first];
                let mut holes = vec![hole];
                for (text, hole) in rest {
                    texts.push(text);
                    holes.push(hole);
                }
                texts.push(last);
                Located::new(Expr::Interp(texts, holes), e.span())
            })
            .boxed();

        // `_` — an operator-section hole (see `desugar_section`).
        let hole_expr = just(Token::Wildcard).map_with(|_, e| Located::new(Expr::Hole, e.span()));

        // `( e )` — grouping, but if `e` contains `_` holes it's an operator
        // section and desugars to a lambda: `(_ + 1)` is `\h -> h + 1`.
        let section = expr
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(desugar_section)
            .boxed();

        // A call written at the start of a line begins a declaration (see
        // [`Token::DeclBang`]), so nothing reads its name as an argument: the
        // expression above it ends where it starts.
        let line_call = path_seg()
            .separated_by(just(Token::Period))
            .at_least(1)
            .then(just(Token::DeclBang))
            .ignored();
        let atom = line_call.not().ignore_then(choice((
            // Before every name: a call is a name, so the alternatives that
            // read one would take the name and leave the `!`. The probe costs
            // a token or two and never descends into a nested expression.
            located(mac_call(false).map(Expr::MacCall)),
            qual_atom,
            unit_expr,
            lit_expr,
            interp_expr,
            var_expr,
            hole_expr,
            ctor_atom,
            update_expr,
            record_expr,
            handle_expr,
            let_expr,
            if_expr,
            lam_expr,
            match_expr,
            section,
            tuple,
            range_list,
            array_expr,
            list_expr,
            vec_expr,
        )));

        // postfix `.field` selection
        let atom = atom
            .then(
                just(Token::Period)
                    .ignore_then(lower_ident())
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map_with(|(obj, fields), e| {
                fields.into_iter().fold(obj, |o, field| {
                    Located::new(Expr::Field(o, field), e.span())
                })
            })
            .boxed();

        // Zero-or-more arguments, and a bare atom when there are none — *not*
        // `at_least(1)` with `atom` as a later alternative in the `choice` below.
        //
        // That arrangement parses the head atom, discovers there is no argument,
        // throws the whole parse away and lets `atom` redo it. For a nested
        // expression that doubles the work at every level, so a `let` inside an
        // `if` inside a `let` costs 2^depth: `Std.Collections.Vector` took 3.6
        // seconds to parse, and a ten-deep nest took eighty.
        let app = atom
            .clone()
            .then(atom.clone().repeated().collect::<Vec<_>>())
            .map_with(|(f, args), e| {
                if args.is_empty() {
                    f
                } else {
                    Located::new(Expr::App(f, args), e.span())
                }
            })
            .boxed();

        // Constructor arguments are atoms, exactly like function-application
        // arguments — `Cons (f x) (map f xs)` is `Cons` applied to two args, not
        // `Cons ((f x) (map f xs))`.
        let cons = upper_ident()
            .then(
                atom.clone()
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .or_not(),
            )
            .map(|(name, args)| Expr::Cons(name, args.unwrap_or_default()))
            .map_with(|e, ex| Located::new(e, ex.span()))
            .boxed();

        // `Mod.name a b` / `Mod.Ctor a b` — a qualified name applied to arguments.
        let qual = qualifier()
            .then(choice((lower_ident(), upper_ident())))
            .then(atom.clone().repeated().collect::<Vec<_>>())
            .map_with(|((q, n), args), e| {
                let base = Located::new(Expr::Qual(q, n), e.span());
                if args.is_empty() {
                    base
                } else {
                    Located::new(Expr::App(base, args), e.span())
                }
            })
            .boxed();

        // A macro call with whatever it is applied to, before `qual`: a
        // qualified call reads as a qualified name until the `!`, and `qual`
        // would take the name and leave the `!` behind.
        let mac_app = located(mac_call(true).map(Expr::MacCall))
            .then(atom.clone().repeated().collect::<Vec<_>>())
            .map_with(|(base, args), e| {
                if args.is_empty() {
                    base
                } else {
                    Located::new(Expr::App(base, args), e.span())
                }
            })
            .boxed();

        // An operand: an application, with any number of `-` in front. A `-`
        // binds tighter than every infix operator, so `-x ^ 2` is `(-x) ^ 2`.
        let base = choice((mac_app, qual, cons, app)).boxed();
        let operand = just(Token::Minus)
            .map_with(|_, e| e.span())
            .repeated()
            .collect::<Vec<Span>>()
            .then(base)
            .map(|(minuses, e)| {
                minuses.into_iter().rev().fold(e, |exp: LExpr, at: Span| {
                    let span = at.extend(exp.span);
                    let inner_span = exp.span;
                    // Fold `-<literal>` into a signed literal so `-1.5` works
                    // without a `Float -> Float` `neg`; anything else stays
                    // `neg <expr>`.
                    match *exp.value {
                        Expr::Lit(Lit::Float(bits)) => Located::new(
                            Expr::Lit(Lit::Float((-f64::from_bits(bits)).to_bits())),
                            span,
                        ),
                        Expr::Lit(Lit::Int(n)) => Located::new(Expr::Lit(Lit::Int(-n)), span),
                        other => Located::new(
                            Expr::UnOp(
                                Located::new(UnOp::Neg, span),
                                Located::new(other, inner_span),
                            ),
                            span,
                        ),
                    }
                })
            })
            .boxed();

        // Operands and the operators between them, flat: how they group is the
        // resolver's to say, once it knows every operator's fixity. `::` is one
        // of them, the list constructor's.
        let infix_op = choice((
            operator(),
            just(Token::ColonColon)
                .map_with(|_, e| Ident::new(InternedString::from("::"), e.span())),
        ));
        let ops = operand
            .clone()
            .then(infix_op.then(operand).repeated().collect::<Vec<_>>())
            .map_with(|(first, rest), e| {
                if rest.is_empty() {
                    first
                } else {
                    Located::new(Expr::Infix(first, rest), e.span())
                }
            })
            .boxed();

        // `and` / `or` sit below every infix operator and short-circuit (the
        // resolver turns them into `if`). `and` binds tighter than `or`.
        let bin = |op: BinOp| {
            move |l: Located<Expr>, r: Located<Expr>| {
                let span = l.span.extend(r.span);
                Located::new(Expr::BinOp(Located::new(op.clone(), span), l, r), span)
            }
        };
        let and_expr = ops
            .clone()
            .foldl(
                just(Token::And).ignore_then(ops.clone()).repeated(),
                bin(BinOp::And),
            )
            .boxed();
        let or_expr = and_expr
            .clone()
            .foldl(
                just(Token::Or).ignore_then(and_expr.clone()).repeated(),
                bin(BinOp::Or),
            )
            .boxed();

        // `x |> f` == `f x` (left-assoc); `f <| x` == `f x` (right-assoc, loosest).
        let app1 = |func: Located<Expr>, arg: Located<Expr>| {
            let span = func.span.extend(arg.span);
            Located::new(Expr::App(func, vec![arg]), span)
        };
        let pipe_l = or_expr
            .clone()
            .foldl(
                just(Token::RPipe).ignore_then(or_expr.clone()).repeated(),
                move |lhs, rhs| app1(rhs, lhs),
            )
            .boxed();
        pipe_l
            .clone()
            .then(
                just(Token::LPipe)
                    .ignore_then(pipe_l)
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map(move |(head, rest)| {
                if rest.is_empty() {
                    return head;
                }
                let mut all = vec![head];
                all.extend(rest);
                let last = all.pop().unwrap();
                all.into_iter().rev().fold(last, |acc, f| app1(f, acc))
            })
            .boxed()
    })
}

/// Turn a parenthesised expression into a lambda if it contains `_` holes:
/// `(_ + _)` becomes `\_hole0 _hole1 -> _hole0 + _hole1`, `(f _ 3)` becomes
/// `\_hole0 -> f _hole0 3`. Holes are numbered left-to-right in source order.
/// Holes inside a nested `\ …` belong to that lambda and are left alone. With no
/// holes the expression is returned unchanged (plain grouping).
fn desugar_section(inner: LExpr) -> LExpr {
    let span = inner.span;
    let mut n = 0usize;
    let body = fill_holes(inner, &mut n);
    if n == 0 {
        return body;
    }
    let params = (0..n)
        .map(|i| {
            Located::new(
                Pat::Var(Located::new(
                    InternedString::from(format!("_hole{i}")),
                    span,
                )),
                span,
            )
        })
        .collect();
    Located::new(Expr::Lam(params, body), span)
}

/// Replace each `Expr::Hole` with `Expr::Var(_hole{k})`, bumping `n`. Recurses
/// through every sub-expression except the body of a nested lambda / `fun` bind.
fn fill_holes(e: LExpr, n: &mut usize) -> LExpr {
    let span = e.span;
    let go = |x, n: &mut usize| fill_holes(x, n);
    let kind = match *e.value {
        Expr::Hole => {
            let name = InternedString::from(format!("_hole{n}"));
            *n += 1;
            Expr::Var(Located::new(name, span))
        }
        Expr::Var(v) => Expr::Var(v),
        Expr::Lit(l) => Expr::Lit(l),
        // The argument is tokens, not expressions: a `_` in there is the
        // macro's to make sense of once it has expanded.
        Expr::MacCall(m) => Expr::MacCall(m),
        Expr::Interp(texts, holes) => Expr::Interp(
            texts,
            holes.into_iter().map(|(x, f)| (go(x, n), f)).collect(),
        ),
        Expr::Unit => Expr::Unit,
        // A nested lambda owns any holes in its body.
        Expr::Lam(ps, b) => Expr::Lam(ps, b),
        Expr::App(f, args) => Expr::App(go(f, n), args.into_iter().map(|a| go(a, n)).collect()),
        Expr::UnOp(op, x) => Expr::UnOp(op, go(x, n)),
        Expr::BinOp(op, l, r) => Expr::BinOp(op, go(l, n), go(r, n)),
        Expr::Infix(first, rest) => Expr::Infix(
            go(first, n),
            rest.into_iter().map(|(o, x)| (o, go(x, n))).collect(),
        ),
        Expr::Tuple(xs) => Expr::Tuple(xs.into_iter().map(|x| go(x, n)).collect()),
        Expr::Array(xs) => Expr::Array(xs.into_iter().map(|x| go(x, n)).collect()),
        Expr::List(xs) => Expr::List(xs.into_iter().map(|x| go(x, n)).collect()),
        Expr::Cons(name, xs) => Expr::Cons(name, xs.into_iter().map(|x| go(x, n)).collect()),
        Expr::Qual(q, name) => Expr::Qual(q, name),
        Expr::Field(o, l) => Expr::Field(go(o, n), l),
        Expr::Update(base, fields) => Expr::Update(
            go(base, n),
            fields.into_iter().map(|(l, v)| (l, go(v, n))).collect(),
        ),
        Expr::Record(fields, base) => Expr::Record(
            fields.into_iter().map(|(l, v)| (l, go(v, n))).collect(),
            base.map(|b| go(b, n)),
        ),
        Expr::If(c, t, f) => Expr::If(go(c, n), go(t, n), go(f, n)),
        Expr::Match(s, arms) => Expr::Match(
            go(s, n),
            arms.into_iter()
                .map(|(p, g, b)| (p, g.map(|g| go(g, n)), go(b, n)))
                .collect(),
        ),
        Expr::Let(binds, body) => Expr::Let(
            binds
                .into_iter()
                .map(|b| match b {
                    Bind::Pat(p, x) => Bind::Pat(p, go(x, n)),
                    // a `fun` bind owns its body's holes
                    other => other,
                })
                .collect(),
            go(body, n),
        ),
        Expr::Handle(s, arms, ret) => Expr::Handle(
            go(s, n),
            arms.into_iter()
                .map(|mut a| {
                    a.body = go(a.body, n);
                    a
                })
                .collect(),
            ret.map(|(p, b)| (p, go(b, n))),
        ),
    };
    Located::new(kind, span)
}

fn pat<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, LPat, extra::Err<Rich<'a, Token, Span>>> + Clone {
    pat_in(expr())
}

/// [`pat`], with `expr` for the expressions of views: inside [`expr`] its own
/// recursive parser, since building a fresh one there would never end.
fn pat_in<'a, I, E>(expr: E) -> impl Parser<'a, I, LPat, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
    E: Parser<'a, I, LExpr, extra::Err<Rich<'a, Token, Span>>> + Clone + 'a,
{
    recursive(|pat| {
        // `[a, b]` / `[a; b]` — a `List` pattern (either separator). There is no
        // A `;` anywhere makes a bracket pattern a `List`, exactly as it does for
        // the literal and the type — so `[;]` is `Nil` and `[x;]` is one element.
        // Without one the brackets are a `Vector`, of which only `[]` is
        // matchable; the resolver reports the rest.
        let list = pat
            .clone()
            .then(
                just(Token::SemiColon)
                    .ignore_then(pat.clone().or_not())
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .map(|(head, tail)| {
                let mut ps = vec![head];
                ps.extend(tail.into_iter().flatten());
                Pat::List(ps)
            })
            .or(just(Token::SemiColon).map(|_| Pat::List(vec![])))
            .or(pat
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect()
                .map(Pat::Vector))
            .delimited_by(just(Token::LBrack), just(Token::RBrack));

        // `#[p, ...]` — a builtin `Array` pattern (exact length).
        let array = just(Token::Hash)
            .ignore_then(just(Token::LBrack))
            .ignore_then(
                pat.clone()
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect(),
            )
            .then_ignore(just(Token::RBrack))
            .map(|patterns| Pat::Array(patterns));

        // `(p : T)` — an annotated pattern, which is how a parameter gets a
        // declared type. Tried before the grouping rule below, which would
        // otherwise consume the `(p` and then fail on the colon; chumsky
        // backtracks, so the ordering is all that is needed.
        //
        // The parentheses are not decoration. A pattern is a *parameter* in
        // `fun f p q = …`, so without them `fun f x : Int = …` would have no
        // way to say whether the annotation belongs to `x` or to `f`.
        let annotated = just(Token::LParen)
            .ignore_then(pat.clone())
            .then_ignore(just(Token::Colon))
            .then(ty())
            .then_ignore(just(Token::RParen))
            .map(|(p, t)| Pat::Ann(Box::new(p), t));

        // `f -> p`, a view, as what parentheses hold: `(f -> p)` on its own,
        // or a part of a tuple, `(k, lookup k -> Just v)`. Tried before a
        // plain pattern, which would take the `f` and stop at the arrow.
        let viewed = expr
            .clone()
            .then_ignore(just(Token::RArrow))
            .then(pat.clone())
            .map_with(|(f, p), e| LPat::new(Pat::View(f, p), e.span()))
            .or(pat.clone());

        // `(p)` is grouping, `(p, q)` a tuple — mirroring the type grammar.
        let tuple = just(Token::LParen)
            .ignore_then(
                viewed
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>(),
            )
            .then_ignore(just(Token::RParen))
            .map(|mut patterns| match patterns.len() {
                // `()` is the unit pattern, not a tuple of nothing — the same
                // reading `param_pat` gives it. They have to agree: a lambda's
                // parameter goes through this parser and a `fun`'s through
                // that one, and `\() -> e` would otherwise take an argument
                // that nothing could be passed to.
                0 => Pat::Unit,
                1 => *patterns.pop().unwrap().value,
                _ => Pat::Tuple(patterns),
            });

        let record_field = lower_ident()
            .then(just(Token::Eq).ignore_then(pat.clone()).or_not())
            .map(|(name, p)| {
                let p = p.unwrap_or_else(|| Located::new(Pat::Var(name.clone()), name.span));
                (name, p)
            });

        let record = just(Token::LBrace)
            .ignore_then(
                record_field
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>(),
            )
            .then(just(Token::Bar).ignore_then(just(Token::Wildcard)).or_not())
            .then_ignore(just(Token::RBrace))
            .map(|(fields, open)| Pat::Record(fields, open.is_some()));

        // What can stand on its own, and so be a constructor's argument: a
        // constructor written there takes no arguments of its own, so
        // `Node Leaf x r` is `Node` applied to three patterns, as in Haskell.
        // A constructor that does take some is parenthesized, `Just (Cons x r)`.
        let argument = mac_call(true)
            .map(Pat::MacCall)
            .or(qualifier()
                .then(upper_ident())
                .map(|(q, name)| Pat::QualCons(q, name, Vec::new())))
            .or(upper_ident().map(|name| Pat::Cons(name, Vec::new())))
            .or(record)
            .or(value_ident().map(|ident| Pat::Var(ident)))
            .or(just(Token::Wildcard).map(|_| Pat::Wildcard))
            .or(lit().map(Pat::Lit))
            .or(array)
            .or(list)
            .or(annotated)
            .or(tuple)
            .or(unit().map(|_| Pat::Unit))
            .map_with(|kind, e| LPat::new(kind, e.span()))
            .boxed();

        // `Mod.Ctor p q` — a constructor pattern qualified by a `use`d module.
        let qual_cons = qualifier()
            .then(upper_ident())
            .then(argument.clone().repeated().at_least(1).collect::<Vec<_>>())
            .map(|((q, name), args)| Pat::QualCons(q, name, args));

        let cons = upper_ident()
            .then(argument.clone().repeated().at_least(1).collect::<Vec<_>>())
            .map(|(name, args)| Pat::Cons(name, args));

        let atom = qual_cons
            .or(cons)
            .map_with(|kind, e| LPat::new(kind, e.span()))
            .or(argument)
            .boxed();

        // `a :: b :: rest` — sugar for `Cons a (Cons b rest)`, grouping to the
        // right. Its parts are atoms rather than whole patterns, so that an
        // `as` after the tail names the whole chain rather than the tail.
        let consed = atom
            .clone()
            .separated_by(just(Token::ColonColon))
            .at_least(1)
            .collect::<Vec<_>>()
            .map(|mut parts| {
                let mut tail = parts.pop().expect("at least one");
                while let Some(head) = parts.pop() {
                    let span = Span::from(head.span.start as usize..tail.span.end as usize);
                    tail = LPat::new(
                        Pat::Cons(
                            Located::new(InternedString::from("Cons"), span),
                            vec![head, tail],
                        ),
                        span,
                    );
                }
                tail
            });

        // `p as x` binds loosest, as in OCaml: `x :: rest as whole` names the
        // whole list, and `(x as y) :: rest` a part of it.
        consed
            .then(
                just(Token::As)
                    .ignore_then(value_ident())
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map_with(|(p, names), e| {
                names
                    .into_iter()
                    .fold(p, |p, name| LPat::new(Pat::As(name, p), e.span()))
            })
            .boxed()
    })
}

/// A **parameter** pattern: the unambiguous, atomic subset of [`pat`], so
/// `fun f (a, b) c = e` reads as two parameters rather than one constructor
/// application. Anything richer goes in parentheses — `fun f (Just x) = e`
/// parses fine and is then rejected by the irrefutability check, which is where
/// the useful error lives (see `meadow-exhaust`).
///
/// Atomic includes literals and constructors that take nothing, which is what a
/// function written as several equations matches on:
///
/// ```text
/// fun gcd a 0 = a
///   | gcd a b = gcd b (a % b)
/// ```
///
/// A single equation may use them too, and is then simply refutable, which the
/// exhaustiveness check reports.
fn param_pat<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, LPat, extra::Err<Rich<'a, Token, Span>>> + Clone {
    param_pat_in(expr())
}

/// [`param_pat`], with `expr` for the expressions of views: see [`pat_in`].
fn param_pat_in<'a, I, E>(
    expr: E,
) -> impl Parser<'a, I, LPat, extra::Err<Rich<'a, Token, Span>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = Span>,
    E: Parser<'a, I, LExpr, extra::Err<Rich<'a, Token, Span>>> + Clone + 'a,
{
    let inner = pat_in(expr.clone());

    // `(p : T)` — a parameter with a declared type. This parser deliberately
    // does not reuse `pat`'s outermost rules (a bare `Just x` here would read as
    // two parameters), so the annotated form has to be repeated rather than
    // inherited. Before `paren`, which would otherwise take the `(p` and fail.
    let annotated = just(Token::LParen)
        .ignore_then(inner.clone())
        .then_ignore(just(Token::Colon))
        .then(ty())
        .then_ignore(just(Token::RParen))
        .map_with(|(p, t), e| LPat::new(Pat::Ann(Box::new(p), t), e.span()));

    // `()` unit, `(p)` grouping, `(p, q)` tuple -- and a view, `(f -> p)`,
    // on its own or as a part of one: see `pat_in`.
    let paren = expr
        .then_ignore(just(Token::RArrow))
        .then(inner.clone())
        .map_with(|(f, p), e| LPat::new(Pat::View(f, p), e.span()))
        .or(inner.clone())
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen))
        .map_with(|mut ps, e| match ps.len() {
            0 => LPat::new(Pat::Unit, e.span()),
            1 => ps.pop().unwrap(),
            _ => LPat::new(Pat::Tuple(ps), e.span()),
        });

    let record_field = lower_ident()
        .then(just(Token::Eq).ignore_then(inner.clone()).or_not())
        .map(|(name, p)| {
            let p = p.unwrap_or_else(|| Located::new(Pat::Var(name.clone()), name.span));
            (name, p)
        });
    let record = record_field
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .then(just(Token::Bar).ignore_then(just(Token::Wildcard)).or_not())
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
        .map_with(|(fields, open), e| LPat::new(Pat::Record(fields, open.is_some()), e.span()));

    // `0`, `"x"`, `'c'`. A literal cannot be the head of an application, so it
    // is as atomic as a name is -- and it is what a function written as several
    // equations matches on.
    let literal = lit().map_with(|l, e| LPat::new(Pat::Lit(l), e.span()));

    // `Nothing`, `Maybe.Nothing` -- a constructor taking nothing. One that takes
    // something is written `(Just x)`, exactly as in Haskell, and for the same
    // reason: bare, it would read as two parameters.
    let nullary = qualifier()
        .then(upper_ident())
        .map_with(|(q, n), e| LPat::new(Pat::QualCons(q, n, Vec::new()), e.span()))
        .or(upper_ident().map_with(|n, e| LPat::new(Pat::Cons(n, Vec::new()), e.span())));

    // `[;]`, `[x; xs]`, `[]`, `#[a, b]`. Brackets say where these end, so the
    // whole pattern grammar can be used inside them without ambiguity.
    let bracketed = just(Token::LBrack)
        .or(just(Token::Hash))
        .rewind()
        .ignore_then(inner.clone());

    choice((
        annotated,
        paren,
        record,
        bracketed,
        nullary,
        literal,
        value_ident().map_with(|n, e| LPat::new(Pat::Var(n), e.span())),
        just(Token::Wildcard).map_with(|_, e| LPat::new(Pat::Wildcard, e.span())),
    ))
    .boxed()
}

fn unit<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, (), extra::Err<Rich<'a, Token, Span>>> + Clone {
    just(Token::LParen)
        .ignore_then(just(Token::RParen))
        .map_with(|_, _| ())
}

/// A lower-case name. Not `pattern` before a capitalized one: that begins
/// the next declaration, a pattern synonym, and is not the last argument, or
/// type variable, of this one -- declarations are not laid out, and `pattern`
/// is a keyword nowhere else.
fn lower_ident<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! {
        Token::LowerIdent(name) => name
    }
    .and_is(
        just(Token::LowerIdent(InternedString::from("pattern")))
            .then(select! { Token::UpperIdent(_) => () })
            .not(),
    )
    .map_with(|name, e| Ident::new(name, e.span()))
}

/// An operator, as the name it is: any run of symbols the lexer makes an
/// operator of, and the ones it gives tokens of their own. Not `::`, which is
/// the list constructor's, nor `|>` and `<|`, which are application written
/// backwards and forwards, nor anything that is punctuation: `=`, `->`, `|`,
/// `\\`, `@`, `.`.
fn operator<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! {
        Token::OpIdent(name) if &*name != ".*" => name,
        Token::Plus => InternedString::from("+"),
        Token::Minus => InternedString::from("-"),
        Token::Star => InternedString::from("*"),
        Token::Slash => InternedString::from("/"),
        Token::Percent => InternedString::from("%"),
        Token::Caret => InternedString::from("^"),
        Token::EqEq => InternedString::from("=="),
        Token::Neq => InternedString::from("!="),
        Token::Lt => InternedString::from("<"),
        Token::Gt => InternedString::from(">"),
        Token::Leq => InternedString::from("<="),
        Token::Geq => InternedString::from(">="),
    }
    .map_with(|name, e| Ident::new(name, e.span()))
}

/// An operator in a `use` list: bare, `(concat, ++)`, as `operator` reads it.
fn user_op<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    operator()
}

/// A name a value is bound to: `x`, or an operator in parentheses, `(++)`.
fn value_ident<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    lower_ident().or(operator().delimited_by(just(Token::LParen), just(Token::RParen)))
}

fn upper_ident<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Ident, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! {
        Token::UpperIdent(name) => name
    }
    .map_with(|name, e| Ident::new(name, e.span()))
}

fn lit<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Lit, extra::Err<Rich<'a, Token, Span>>> + Clone {
    select! {
        Token::Int(i) => Lit::Int(i),
        Token::Real(bits) => Lit::Float(bits),
        Token::String(s) => Lit::String(s),
        Token::Char(c) => Lit::Char(c),
    }
}

fn located<'tokens, I, P>(
    p: P,
) -> impl Parser<'tokens, I, LExpr, extra::Err<Rich<'tokens, Token, Span>>> + Clone
where
    I: ValueInput<'tokens, Token = Token, Span = Span>,
    P: Parser<'tokens, I, Expr, extra::Err<Rich<'tokens, Token, Span>>> + Clone,
{
    p.map_with(|v, e| Located::new(v, e.span()))
}

/// `: T` before the `=` of a binding — the declared result type.
///
/// `: R ! e` says what running the body performs as well: the effect of the
/// function's last arrow, which the parameters written inline leave nowhere
/// else to put. It is kept as a function type of no parameters -- which no
/// type written anywhere else can be -- and read apart again by inference.
/// A result that is itself a function takes its own arrow's effect first, so
/// `: a -> b ! e` is a function performing `e` when called, and the body's
/// effect is written after it: `: (a -> b ! e) ! f`.
fn result_ty<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, Option<LType>, extra::Err<Rich<'a, Token, Span>>> + Clone {
    just(Token::Colon)
        .ignore_then(ty())
        .then(just(Token::Bang).ignore_then(effect_row()).or_not())
        .map_with(|(t, eff), e| match eff {
            None => t,
            Some(eff) => Located::new(TypeExpr::Fun(Vec::new(), t, Some(eff)), e.span()),
        })
        .or_not()
}

/// An effect row as it is written after `!`: `{ Console, State Int | e }`, a
/// bare variable `e`, or one bare effect `Console`.
///
/// The same grammar the arrow of a function type takes (see [`ty`], which has
/// to keep its own copy for its label arguments to be its own atoms).
fn effect_row<'a, I: ValueInput<'a, Token = Token, Span = Span>>()
-> impl Parser<'a, I, EffectRow, extra::Err<Rich<'a, Token, Span>>> + Clone {
    let label = upper_ident().then(ty_atom().repeated().collect::<Vec<_>>());
    let braced = label
        .clone()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .then(just(Token::Bar).ignore_then(lower_ident()).or_not())
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
        .map(|(labels, tail)| EffectRow { labels, tail });
    let bare_var = lower_ident().map(|n| EffectRow {
        labels: vec![],
        tail: Some(n),
    });
    let bare_label = label.map(|l| EffectRow {
        labels: vec![l],
        tail: None,
    });
    choice((braced, bare_var, bare_label))
}

/// What follows an `effect`'s name and parameters: its operations, or the
/// effects it is another name for.
#[derive(Clone)]
enum EffectBody {
    Ops(Vec<Field>),
    Alias(EffectRow),
}

/// One equation of a function written in several: `| gcd a b = …`.
struct Clause {
    name: Ident,
    args: Vec<LPat>,
    body: LExpr,
}

/// Assemble a function written as several equations.
///
/// ```text
/// fun gcd a 0 = a
///   | gcd a b = gcd b (a % b)
/// ```
///
/// becomes one function whose body matches on its arguments:
///
/// ```text
/// fun gcd _arg0 _arg1 =
///   match (_arg0, _arg1) with
///   | (a, 0) -> a
///   | (a, b) -> gcd b (a % b)
/// ```
///
/// which is sugar and nothing more -- so the equations are checked for
/// exhaustiveness, and overlap, exactly as the `match` a person would have
/// written by hand. One equation is left alone, and compiles to what it always
/// did.
fn equations<'a>(
    name: Ident,
    args: Vec<LPat>,
    ret: Option<LType>,
    body: LExpr,
    rest: Vec<Clause>,
    span: Span,
    emitter: &mut chumsky::input::Emitter<Rich<'a, Token, Span>>,
) -> Bind {
    if rest.is_empty() {
        return bind_of(name, args, ret, body);
    }
    // Nothing to match on, so the equations could not be told apart.
    if args.is_empty() {
        emitter.emit(Rich::custom(
            span,
            format!(
                "`{}` is written as several equations but takes no arguments, so there is nothing to tell them apart by",
                name.value()
            ),
        ));
        return bind_of(name, args, ret, body);
    }
    for c in &rest {
        if c.name.value() != name.value() {
            emitter.emit(Rich::custom(
                c.name.span,
                format!(
                    "this equation defines `{}`, but the ones above it define `{}`",
                    c.name.value(),
                    name.value()
                ),
            ));
        }
        if c.args.len() != args.len() {
            emitter.emit(Rich::custom(
                c.name.span,
                format!(
                    "this equation of `{}` takes {} argument{}, but the first takes {}",
                    name.value(),
                    c.args.len(),
                    if c.args.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }
    }

    // One fresh name per argument, to match on. A name a person could write
    // would be shadowed by whatever each equation binds, so these need not be
    // unforgeable -- only unlikely, and `_` keeps them out of the way.
    let params: Vec<LPat> = (0..args.len())
        .map(|i| {
            Located::new(
                Pat::Var(Ident::new(
                    InternedString::from(format!("_arg{i}")),
                    name.span,
                )),
                name.span,
            )
        })
        .collect();
    let scrutinee = tuple_of(
        params
            .iter()
            .map(|p| match p.value() {
                Pat::Var(n) => Located::new(Expr::Var(n.clone()), n.span),
                _ => unreachable!("the parameters just made are variables"),
            })
            .collect(),
        name.span,
    );

    let arm = |pats: Vec<LPat>, body: LExpr| {
        let span = body.span;
        (pat_tuple_of(pats, span), None, body)
    };
    let mut arms = vec![arm(args, body)];
    arms.extend(rest.into_iter().map(|c| arm(c.args, c.body)));

    let matched = Located::new(Expr::Match(scrutinee, arms), span);
    Bind::Fun(name, params, ret, matched)
}

/// One expression, or a tuple of several.
fn tuple_of(mut xs: Vec<LExpr>, span: Span) -> LExpr {
    match xs.len() {
        1 => xs.pop().expect("one"),
        _ => Located::new(Expr::Tuple(xs), span),
    }
}

/// The same, for patterns.
fn pat_tuple_of(mut ps: Vec<LPat>, span: Span) -> LPat {
    match ps.len() {
        1 => ps.pop().expect("one"),
        _ => Located::new(Pat::Tuple(ps), span),
    }
}

/// Assemble a binding, which is a function only when it takes arguments.
///
/// `def x : Int = 5` has no parameters, so there is no result to declare — the
/// annotation is describing `x` itself, and the binding it belongs on is the
/// pattern. Routing it there rather than rejecting it means one syntax reads
/// the way it looks in both places.
fn bind_of(name: Ident, args: Vec<LPat>, ret: Option<LType>, body: LExpr) -> Bind {
    if !args.is_empty() {
        return Bind::Fun(name, args, ret, body);
    }
    let span = name.span;
    let var = Located::new(Pat::Var(name), span);
    match ret {
        Some(t) => Bind::Pat(Located::new(Pat::Ann(Box::new(var), t), span), body),
        None => Bind::Pat(var, body),
    }
}
