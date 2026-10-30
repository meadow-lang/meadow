//! **Lowering: HIR to core.**
//!
//! Where the types come from. Inference has already annotated every HIR node
//! and recorded what each binding generalized over
//! ([`meadow_infer::Generalized`]); this pass copies that onto the core term,
//! so that what comes out is checkable on its own — see [`crate::lint`].
//!
//! Two things need explaining, because they are the only places the
//! translation is not a transcription.
//!
//! * **A generalized binding becomes a [`Term::TyLam`].** Its binders are the
//!   arena variables inference quantified over, and the annotations inside its
//!   body mention exactly those, because both come from the same inference run.
//!
//! * **Every mention of a polymorphic name becomes a [`Term::TyApp`].** The
//!   type arguments are not recorded anywhere by inference — instantiation
//!   replaced them with fresh variables and unified those — so they are
//!   recovered by matching the binding's scheme against the type the mention
//!   was inferred at. That is a one-way match, not unification: the occurrence
//!   type is already a substitution instance of the scheme, and the match
//!   reads the substitution back off it.

use crate::*;

// ===========================================================================
// Lowering: hir -> core
// ===========================================================================

pub struct Lowerer<'a> {
    prims: &'a HashMap<Var, Prim>,
    names: &'a HashMap<Var, InternedString>,
    /// Operation `VarId` -> `(effect, op)` — a reference to one lowers to
    /// `\x -> perform Effect.op x`.
    effect_ops: &'a HashMap<Var, (InternedString, InternedString)>,
    /// Inferred types, keyed by `NodeId` — consulted so an integer literal lowers
    /// at the type its context gave it: [`Lit::BigInt`], [`Lit::Word`] or [`Lit::Int`].
    types: &'a TypeTable,
    /// Declared arity per data constructor, so an under-applied one can be
    /// eta-expanded into a function.
    ctor_arity: &'a HashMap<InternedString, usize>,
    /// What each binding generalized over, for the bindings of this unit —
    /// the binders of the `TyLam` it becomes.
    generalized: &'a HashMap<Var, Generalized>,
    /// Every polymorphic name in scope, this unit's and its dependencies',
    /// so a mention of one can be given its type arguments.
    schemes: &'a HashMap<Var, Scheme>,
    /// Every type's constructors and their fields, from inference: what a
    /// record update rebuilds its value with. Without it, an update of a
    /// record type lowers as an anonymous record's would.
    pub variants: Option<&'a VariantEnv>,
    /// The dictionaries each mention of a name with a `where` is applied to,
    /// and the traits there are -- from inference. See `meadow_infer::traits`.
    pub evidence: Option<&'a HashMap<hir::NodeId, Vec<meadow_infer::Evidence>>>,
    pub traits: Option<&'a meadow_infer::TraitEnv>,
    /// The dictionary parameters of the definition being lowered, and their
    /// types: what [`meadow_infer::Evidence::Given`] counts into.
    dicts: Vec<(Var, Ty)>,
    /// Named-field order per constructor, accumulated across `lower_module` calls.
    pub ctor_fields: HashMap<InternedString, Vec<InternedString>>,
    /// The constructors given their fields out of the declared order, and the
    /// order they were written in -- see [`hir::Module::written_orders`].
    written_orders: HashMap<hir::NodeId, Vec<u32>>,
    /// Desugaring invents variables -- a scrutinee to bind, an eta-expansion's
    /// parameters -- and they belong to the unit being lowered, so this carries
    /// on from where resolution left off rather than starting anywhere.
    vars: hir::VarIdGen,
    /// The source of the module being lowered, when positions are wanted --
    /// see [`Term::Loc`]. `None`, the default, lowers exactly as before.
    pub locations: Option<u32>,
}

impl<'a> Lowerer<'a> {
    pub fn new(
        prims: &'a HashMap<Var, Prim>,
        names: &'a HashMap<Var, InternedString>,
        effect_ops: &'a HashMap<Var, (InternedString, InternedString)>,
        types: &'a TypeTable,
        ctor_arity: &'a HashMap<InternedString, usize>,
        generalized: &'a HashMap<Var, Generalized>,
        schemes: &'a HashMap<Var, Scheme>,
        vars: hir::VarIdGen,
    ) -> Self {
        Lowerer {
            prims,
            names,
            effect_ops,
            types,
            ctor_arity,
            generalized,
            schemes,
            variants: None,
            evidence: None,
            traits: None,
            dicts: Vec::new(),
            ctor_fields: HashMap::new(),
            written_orders: HashMap::new(),
            vars,
            locations: None,
        }
    }

    /// `term`, marked as written at `span` if positions are being kept.
    fn at(&self, span: meadow_span::Span, term: Term) -> Term {
        match self.locations {
            Some(source) => Term::Loc(Loc { source, span }, Arc::new(term)),
            None => term,
        }
    }

    /// One past the last `VarId` this unit has handed out, resolution and
    /// lowering together. The unit records it so the next one can stack above.
    pub fn var_end(&self) -> u32 {
        self.vars.end()
    }

    /// The type inference gave the node at `id`.
    ///
    /// A node with no entry is one inference never reached, which happens only
    /// in a unit that already has errors. [`unknown`] stands in, and the
    /// checker lets it pass rather than reporting a second problem caused by
    /// the first.
    fn ty(&self, id: hir::NodeId) -> Ty {
        self.types.get(id).cloned().unwrap_or_else(unknown)
    }

    /// The polytype of a binding of this unit: its `TyLam` binders and the
    /// type over them.
    ///
    /// Inference records a scheme whose quantifiers are numbered, plus the
    /// arena variable behind each one. Core wants the *variables*, because the
    /// annotations inside the binding's body are written in terms of them.
    fn poly_of(&self, v: Var, fallback: hir::NodeId) -> Poly {
        match self.generalized.get(&v) {
            Some(g) => {
                let binders: Vec<TyVar> = g
                    .vars
                    .iter()
                    .zip(&g.scheme.quant)
                    .map(|(id, kind)| TyVar {
                        id: *id,
                        kind: *kind,
                    })
                    .collect();
                let map: HashMap<u32, Ty> = binders
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i as u32, InferType::Var(b.id)))
                    .collect();
                // A `where` is a parameter per trait, before anything else.
                let ty =
                    g.scheme
                        .preds
                        .iter()
                        .rev()
                        .fold(subst_bound(&g.scheme.ty, &map), |acc, p| {
                            InferType::Fun(
                                vec![subst_bound(&meadow_infer::pred_type(p), &map)],
                                Box::new(acc),
                                Box::new(InferType::RowEmpty),
                            )
                        });
                Poly { ty, binders }
            }
            None => Poly::mono(self.ty(fallback)),
        }
    }

    /// Wrap a generalized binding's body in the type abstraction its polytype
    /// promises. A monomorphic one is left alone.
    fn ty_lam(poly: &Poly, term: Term) -> Term {
        if poly.binders.is_empty() {
            term
        } else {
            Term::TyLam(poly.binders.clone(), Arc::new(term))
        }
    }

    /// A mention of `v`, with its type arguments when it is polymorphic.
    ///
    /// The arguments are recovered by matching the binding's scheme against
    /// the type this mention was inferred at — see the module docs.
    fn mention(&self, v: Var, at: hir::NodeId) -> Term {
        let term = Term::Var(v);
        let Some(scheme) = self.schemes.get(&v) else {
            return term;
        };
        if scheme.quant.is_empty() {
            return term;
        }
        let occurrence = self.ty(at);
        if occurrence.references_error() || crate::mentions_unknown(&occurrence) {
            // Only a program with a reported error gets here -- the mention is
            // of something inference could not type, or of something whose own
            // type arguments were already given up on -- and it will not be run
            // through the checker, so there is nothing to recover.
            return term;
        }
        let evidence: &[meadow_infer::Evidence] = self
            .evidence
            .and_then(|e| e.get(&at))
            .map_or(&[], |e| e.as_slice());
        let dict_tys: Vec<Option<Ty>> = evidence.iter().map(|e| self.evidence_type(e)).collect();
        let args = meadow_infer::match_scheme_with(scheme, &occurrence, &dict_tys).unwrap_or_else(|| {
            // Every occurrence *is* an instance, so a failure here is a bug in
            // the matcher rather than in the program. Say so where a test will
            // see it, and carry on with something the checker will wave past.
            debug_assert!(
                false,
                "could not recover type arguments for a mention of `{}`\n  scheme: {:?}\n  at: {:?}",
                self.name_of(v),
                scheme,
                occurrence
            );
            scheme.quant.iter().map(|_| unknown()).collect()
        });
        let term = Term::TyApp(Arc::new(term), args);
        if scheme.preds.len() != evidence.len() {
            // Only in a program with a reported error.
            return term;
        }
        evidence.iter().fold(term, |f, e| {
            Term::App(Arc::new(f), Arc::new(self.evidence_term(e)))
        })
    }

    /// The type of the dictionary `e` is, where it can be said.
    fn evidence_type(&self, e: &meadow_infer::Evidence) -> Option<Ty> {
        use meadow_infer::Evidence;
        match e {
            Evidence::Given(i) => self.dicts.get(*i).map(|(_, t)| t.clone()),
            Evidence::Super { ty, .. } | Evidence::Impl { ty, .. } => Some(ty.clone()),
            Evidence::Missing => None,
        }
    }

    /// The dictionary `e` describes, as a term.
    fn evidence_term(&self, e: &meadow_infer::Evidence) -> Term {
        use meadow_infer::Evidence;
        match e {
            Evidence::Given(i) => match self.dicts.get(*i) {
                Some((v, _)) => Term::Var(*v),
                None => Term::Error,
            },
            Evidence::Super { of, label, ty } => {
                Term::Sel(Arc::new(self.evidence_term(of)), *label, ty.clone())
            }
            Evidence::Impl { dict, ty, args } => {
                let dict_tys: Vec<Option<Ty>> =
                    args.iter().map(|a| self.evidence_type(a)).collect();
                let head = self.mention_as(*dict, ty, &dict_tys);
                args.iter().fold(head, |f, a| {
                    Term::App(Arc::new(f), Arc::new(self.evidence_term(a)))
                })
            }
            Evidence::Missing => Term::Error,
        }
    }

    /// A mention of `v` at the type `ty`, not counting what its `where` takes.
    fn mention_as(&self, v: Var, ty: &Ty, dict_tys: &[Option<Ty>]) -> Term {
        let term = Term::Var(v);
        match self.schemes.get(&v) {
            Some(scheme) if !scheme.quant.is_empty() => {
                let args = meadow_infer::match_scheme_with(scheme, ty, dict_tys)
                    .unwrap_or_else(|| scheme.quant.iter().map(|_| unknown()).collect());
                Term::TyApp(Arc::new(term), args)
            }
            _ => term,
        }
    }

    /// Give the definition `v` its dictionary parameters, around the body
    /// `build` makes with them in scope: `\(d1 : Show a) (d2 : Ord a) -> body`.
    fn with_dicts(&mut self, v: Var, poly: &Poly, build: impl FnOnce(&mut Self) -> Term) -> Term {
        let count = self.generalized.get(&v).map_or(0, |g| g.scheme.preds.len());
        let mut tys = Vec::with_capacity(count);
        let mut rest = &poly.ty;
        for _ in 0..count {
            let InferType::Fun(params, ret, _) = rest else {
                break;
            };
            tys.push(params[0].clone());
            rest = &**ret;
        }
        let dicts: Vec<(Var, Ty)> = tys.into_iter().map(|t| (self.vars.fresh(), t)).collect();
        let saved = std::mem::replace(&mut self.dicts, dicts.clone());
        let body = build(self);
        self.dicts = saved;
        dicts
            .into_iter()
            .rev()
            .fold(body, |acc, (d, t)| Term::Lam(d, t, Arc::new(acc)))
    }

    /// A trait's methods: each the function from a dictionary to its field.
    fn lower_trait(&mut self, td: &hir::TraitDecl, out: &mut Vec<Def>) {
        let Some(shape) = self.traits.and_then(|t| t.get(&td.name)).cloned() else {
            return;
        };
        self.ctor_fields.insert(td.dict, shape.labels.clone());
        for m in &td.methods {
            let v = *m.var.value();
            let poly = self.poly_of(v, m.var.id);
            let InferType::Fun(params, result, _) = &poly.ty else {
                continue;
            };
            let d = self.vars.fresh();
            let body = Term::Sel(Arc::new(Term::Var(d)), m.name, (**result).clone());
            let term = Self::ty_lam(&poly, Term::Lam(d, params[0].clone(), Arc::new(body)));
            out.push(Def {
                var: v,
                name: self.name_of(v),
                module: InternedString::default(),
                poly,
                term,
            });
            if let Some(hir::DefaultMethod {
                body: Some(body), ..
            }) = &m.default
            {
                self.lower_bind_toplevel(body, out);
            }
        }
    }

    /// An `impl`: its methods, and the dictionary that holds them -- a value,
    /// or a function of the dictionaries its own `where` asks for.
    fn lower_impl(&mut self, id: &hir::ImplDecl, out: &mut Vec<Def>) {
        for (_, body) in &id.methods {
            self.lower_bind_toplevel(body, out);
        }
        let dict = *id.dict.value();
        let (Some(shape), Some(variants)) = (
            self.traits.and_then(|t| t.get(id.tr.value())).cloned(),
            self.variants,
        ) else {
            return;
        };
        let poly = self.poly_of(dict, id.dict.id);
        let count = self
            .generalized
            .get(&dict)
            .map_or(0, |g| g.scheme.preds.len());
        let mut dict_ty = &poly.ty;
        for _ in 0..count {
            if let InferType::Fun(_, ret, _) = dict_ty {
                dict_ty = &**ret;
            }
        }
        let dict_ty = dict_ty.clone();
        let InferType::Con(_, targs) = &dict_ty else {
            return;
        };
        let at: HashMap<u32, Ty> = targs
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u32, t.clone()))
            .collect();
        let field_tys: Vec<Ty> = variants
            .get(id.tr.value())
            .and_then(|vs| vs.first())
            .map(|sig| sig.fields.iter().map(|f| subst_bound(f, &at)).collect())
            .unwrap_or_default();
        let supers: Vec<meadow_infer::Evidence> = self
            .evidence
            .and_then(|e| e.get(&id.dict.id))
            .cloned()
            .unwrap_or_default();
        let methods: HashMap<InternedString, Var> = id
            .methods
            .iter()
            .map(|(n, b)| (*n, b.bound_vars()[0]))
            .collect();

        let body_poly = poly.clone();
        let term = self.with_dicts(dict, &body_poly, |this| {
            let given: Vec<Term> = this.dicts.iter().map(|(d, _)| Term::Var(*d)).collect();
            let given_tys: Vec<Option<Ty>> =
                this.dicts.iter().map(|(_, t)| Some(t.clone())).collect();
            let apply = |f: Term, args: &[Term]| {
                args.iter()
                    .fold(f, |f, a| Term::App(Arc::new(f), Arc::new(a.clone())))
            };
            let mut fields: Vec<Term> = Vec::new();
            for k in 0..shape.supers.len() {
                fields.push(match supers.get(k) {
                    Some(e) => this.evidence_term(e),
                    None => Term::Error,
                });
            }
            for (k, (name, _, default)) in shape.methods.iter().enumerate() {
                let field_ty = field_tys
                    .get(shape.supers.len() + k)
                    .cloned()
                    .unwrap_or_else(unknown);
                let field = match (methods.get(name), default) {
                    (Some(m), _) => {
                        // Its own `where` is the `impl`'s, when it has one: a
                        // method with no parameters is a value, and has none.
                        let takes = this.schemes.get(m).map_or(0, |s| s.preds.len());
                        let head = this.mention_as(*m, &field_ty, &given_tys);
                        apply(head, &given[..takes.min(given.len())])
                    }
                    (None, Some(default)) => {
                        // `\x -> default self x`: under a function, so that
                        // the dictionary is not needed to make itself.
                        let me = apply(this.mention_as(dict, &dict_ty, &given_tys), &given);
                        let head = this.mention_as(*default, &field_ty, &[Some(dict_ty.clone())]);
                        let call = Term::App(Arc::new(head), Arc::new(me));
                        match &field_ty {
                            InferType::Fun(params, _, _) => {
                                let x = this.vars.fresh();
                                Term::Lam(
                                    x,
                                    params[0].clone(),
                                    Arc::new(Term::App(Arc::new(call), Arc::new(Term::Var(x)))),
                                )
                            }
                            _ => call,
                        }
                    }
                    (None, None) => Term::Error,
                };
                fields.push(field);
            }
            Term::Ctor(shape.dict, dict_ty.clone(), fields)
        });
        out.push(Def {
            var: dict,
            name: self.name_of(dict),
            module: InternedString::default(),
            term: Self::ty_lam(&poly, term),
            poly,
        });
    }

    /// Lower an integer literal as the type inference gave it: `BigInt`, a sized
    /// type (wrapped to it), or `Int`. A literal whose type is still a variable
    /// -- one inside a function generic over its integer type -- is a
    /// [`Lit::AnyInt`], which the primitives let take its neighbour's type
    /// (`crate::num`).
    fn int_lit(&self, id: hir::NodeId, value: i64) -> Lit {
        match self.types.get(id) {
            Some(InferType::Con(n, args)) if args.is_empty() => match &**n {
                "BigInt" => Lit::BigInt(value),
                name => match crate::num::Width::from_type(name) {
                    Some(w) => Lit::Word(w, w.wrap(value as i128)),
                    None => Lit::Int(value),
                },
            },
            Some(InferType::Var(v)) => Lit::AnyInt(value, *v),
            _ => Lit::Int(value),
        }
    }

    /// A float literal as its type: `Float32`, or `Float`.
    fn float_lit(&self, id: hir::NodeId, bits: u64) -> Lit {
        let x = f64::from_bits(bits);
        match self.types.get(id) {
            Some(InferType::Con(n, args)) if args.is_empty() && &**n == "Float32" => {
                Lit::Float32(x as f32)
            }
            Some(InferType::Var(v)) => Lit::AnyFloat(x, *v),
            _ => Lit::Float(x),
        }
    }

    pub fn lower_module(&mut self, module: &hir::LModule) -> Vec<Def> {
        let mut defs = Vec::new();
        self.written_orders
            .extend(module.value().written_orders.iter().cloned());
        for decl in &module.value().decls {
            match decl.value() {
                hir::Decl::Bind(bind) => self.lower_bind_toplevel(bind, &mut defs),
                hir::Decl::Data(dd) => {
                    for v in &dd.variants {
                        if let hir::VariantFields::Named(fs) = &v.fields {
                            self.ctor_fields
                                .insert(v.name, fs.iter().map(|(n, _)| *n).collect());
                        }
                    }
                }
                hir::Decl::Record(rd) => {
                    self.ctor_fields
                        .insert(rd.ctor, rd.fields.iter().map(|(n, _)| *n).collect());
                }
                hir::Decl::Trait(td) => self.lower_trait(td, &mut defs),
                hir::Decl::Impl(id) => self.lower_impl(id, &mut defs),
                _ => {}
            }
        }
        defs
    }

    fn name_of(&self, v: Var) -> InternedString {
        self.names.get(&v).copied().unwrap_or_default()
    }

    fn lower_bind_toplevel(&mut self, bind: &hir::Bind, out: &mut Vec<Def>) {
        match bind {
            hir::Bind::Fun(name, params, _, body) => {
                let v = *name.value();
                let poly = self.poly_of(v, name.id);
                let inner = self.with_dicts(v, &poly, |this| this.curry_lam(params, body));
                let term = Self::ty_lam(&poly, inner);
                out.push(Def {
                    var: v,
                    name: self.name_of(v),
                    module: InternedString::default(),
                    poly,
                    term,
                });
            }
            hir::Bind::Pat(pat, expr) => {
                let rhs = self.lower_expr(expr);
                // Not around a lambda: a definition that is one gets a second,
                // direct entry point, and the back end finds it by its shape.
                let rhs = if matches!(rhs, Term::Lam(..)) {
                    rhs
                } else {
                    self.at(expr.span, rhs)
                };
                // `fun f : a -> a = e` is the binding `(f : a -> a) = e`: the
                // annotation has to be seen through here, or a polymorphic
                // definition is taken apart as a pattern and loses the type
                // abstraction every mention of it supplies arguments to.
                let bare = unannotated(pat);
                match bare.value() {
                    hir::Pat::Var(id) => {
                        let v = *id.value();
                        let poly = self.poly_of(v, bare.id);
                        out.push(Def {
                            var: v,
                            name: self.name_of(v),
                            module: InternedString::default(),
                            term: Self::ty_lam(&poly, rhs),
                            poly,
                        });
                    }
                    hir::Pat::Wildcard => {
                        let v = self.vars.fresh();
                        out.push(Def {
                            var: v,
                            name: InternedString::from("_"),
                            module: InternedString::default(),
                            poly: Poly::mono(self.ty(pat.id)),
                            term: rhs,
                        });
                    }
                    _ => {
                        // `def (a, b) = e` etc.: bind the value once, then project.
                        let scrut = self.vars.fresh();
                        out.push(Def {
                            var: scrut,
                            name: InternedString::from("$bind"),
                            module: InternedString::default(),
                            poly: Poly::mono(self.ty(pat.id)),
                            term: rhs,
                        });
                        let mut binds = Vec::new();
                        self.bind_pat(Term::Var(scrut), pat, &mut binds);
                        for (v, ty, t) in binds {
                            out.push(Def {
                                var: v,
                                name: self.name_of(v),
                                module: InternedString::default(),
                                poly: Poly::mono(ty),
                                term: t,
                            });
                        }
                    }
                }
            }
            hir::Bind::Error => {}
        }
    }

    /// `\p1 p2 -> body` for irrefutable parameter patterns: each parameter binds
    /// one fresh variable, and anything structural is destructured by `let`s at
    /// the top of the body.
    fn curry_lam(&mut self, params: &[hir::LPat], body: &hir::LExpr) -> Term {
        let binders: Vec<_> = params.iter().map(|p| self.pat_binder(p)).collect();
        let lowered = self.lower_expr(body);
        let mut term = self.at(body.span, lowered);
        for (v, ty, structured) in binders.into_iter().rev() {
            term = self.with_pat_prelude_term(v, structured, term);
            term = Term::Lam(v, ty, Arc::new(term));
        }
        term
    }

    /// Prefix `term` with the `let`s that destructure `var` according to `pat`.
    fn with_pat_prelude_term(&mut self, var: Var, pat: Option<&hir::LPat>, mut term: Term) -> Term {
        if let Some(p) = pat {
            let mut binds = Vec::new();
            self.bind_pat(Term::Var(var), p, &mut binds);
            for (bv, bty, bt) in binds.into_iter().rev() {
                term = Term::Let(bv, Poly::mono(bty), Arc::new(bt), Arc::new(term));
            }
        }
        term
    }

    fn lower_expr(&mut self, expr: &hir::LExpr) -> Term {
        match expr.value() {
            hir::Expr::Lit(hir::Lit::Int(i)) => Term::Lit(self.int_lit(expr.id, *i)),
            hir::Expr::Lit(hir::Lit::Float(b)) => Term::Lit(self.float_lit(expr.id, *b)),
            hir::Expr::Lit(hir::Lit::String(s)) => Term::Lit(Lit::Str(*s)),
            hir::Expr::Lit(hir::Lit::Char(c)) => Term::Lit(Lit::Char(*c)),
            hir::Expr::Unit => Term::Lit(Lit::Unit),

            hir::Expr::Var(id) => {
                let v = *id.value();
                if let Some(&op) = self.prims.get(&v) {
                    self.eta_prim(op, self.ty(id.id))
                } else if let Some(&(eff, opname)) = self.effect_ops.get(&v) {
                    // `get` becomes `\x -> perform Effect.get x`
                    let x = self.vars.fresh();
                    let (arg, ret) = match self.ty(id.id) {
                        InferType::Fun(args, ret, _) => {
                            (args.first().cloned().unwrap_or_else(unknown), *ret)
                        }
                        _ => (unknown(), unknown()),
                    };
                    Term::Lam(
                        x,
                        arg,
                        Arc::new(Term::Perform(eff, opname, Arc::new(Term::Var(x)), ret)),
                    )
                } else {
                    self.mention(v, id.id)
                }
            }

            hir::Expr::Lam(params, body) => self.curry_lam(params, body),

            hir::Expr::App(func, args) => {
                if let hir::Expr::Var(id) = func.value() {
                    if let Some(&op) = self.prims.get(&*id.value()) {
                        // `runSt f` is `f ()`: the rule that keeps its state
                        // from escaping is all in the types.
                        if op == Prim::RunSt && args.len() == 1 {
                            let f = self.lower_expr(&args[0]);
                            let call = Term::App(Arc::new(f), Arc::new(Term::Lit(Lit::Unit)));
                            return self.at(expr.span, call);
                        }
                        if op.arity() == args.len() {
                            let a = args.iter().map(|a| self.lower_expr(a)).collect();
                            return Term::Prim(op, a, self.ty(expr.id));
                        }
                    }
                }
                let mut term = self.lower_expr(func);
                for a in args {
                    term = Term::App(Arc::new(term), Arc::new(self.lower_expr(a)));
                }
                self.at(expr.span, term)
            }

            hir::Expr::Let(binds, body) => {
                // Every layer of a `let` produces what the whole expression
                // does, which is what a structural binder's `case` needs.
                let result = self.ty(expr.id);
                let mut inner = self.lower_expr(body);
                for bind in binds.iter().rev() {
                    inner = self.lower_let_bind(bind, inner, &result);
                }
                self.at(expr.span, inner)
            }

            // The branches, never the condition: a comparison there is fused
            // into the branch that tests it, and has to stay recognisable.
            hir::Expr::If(c, t, e) => {
                let c = self.lower_expr(c);
                let lt = self.lower_expr(t);
                let le = self.lower_expr(e);
                Term::If(
                    Arc::new(c),
                    Arc::new(self.at(t.span, lt)),
                    Arc::new(self.at(e.span, le)),
                )
            }

            hir::Expr::Match(scrut, arms) => {
                let s = self.lower_expr(scrut);
                let arms = arms
                    .iter()
                    .map(|(p, g, e)| {
                        let guard = g.as_ref().map(|g| {
                            let lowered = self.lower_expr(g);
                            self.at(g.span, lowered)
                        });
                        let body = self.lower_expr(e);
                        (Some(p), guard, self.at(e.span, body))
                    })
                    .collect();
                self.lower_case(s, self.ty(scrut.id), arms, self.ty(expr.id))
            }

            hir::Expr::Tuple(items) => {
                Term::Tuple(items.iter().map(|e| self.lower_expr(e)).collect())
            }
            hir::Expr::Array(items) => {
                let elem = match self.ty(expr.id) {
                    InferType::Con(n, args) if &*n == "Array" && args.len() == 1 => args[0].clone(),
                    _ => unknown(),
                };
                Term::Array(items.iter().map(|e| self.lower_expr(e)).collect(), elem)
            }
            // `[a; b; c]` is sugar for `Cons a (Cons b (Cons c Nil))` — `List` is
            // an ordinary `Std` data type, so it lowers to plain constructors.
            hir::Expr::List(items) => {
                // Every cell of the chain has the type of the literal itself.
                let ty = self.ty(expr.id);
                let nil = Term::Ctor(InternedString::from("List.Nil"), ty.clone(), vec![]);
                items.iter().rev().fold(nil, |acc, e| {
                    let head = self.lower_expr(e);
                    Term::Ctor(
                        InternedString::from("List.Cons"),
                        ty.clone(),
                        vec![head, acc],
                    )
                })
            }
            hir::Expr::Cons(label, args) => {
                let name = *label.value();
                let ty = self.ty(expr.id);
                // `Point { y = …, x = … }`: the fields in the order written,
                // then the constructor of them in the order it holds them.
                if let Some(order) = self.written_orders.get(&expr.id).cloned() {
                    let values = args
                        .iter()
                        .map(|e| (self.lower_expr(e), self.ty(e.id)))
                        .collect();
                    return self.in_order(values, &order, |this, args| this.ctor(name, args, ty));
                }
                let lowered: Vec<Term> = args.iter().map(|e| self.lower_expr(e)).collect();
                match (&*name, lowered.len()) {
                    ("Bool.True", 0) => Term::Lit(Lit::Bool(true)),
                    ("Bool.False", 0) => Term::Lit(Lit::Bool(false)),
                    _ => self.ctor(name, lowered, ty),
                }
            }

            hir::Expr::Record(fields, base) => match base {
                None => Term::Record(
                    fields
                        .iter()
                        .map(|(l, e)| (*l.value(), self.lower_expr(e)))
                        .collect(),
                ),
                // `{ x = v | r }`: the fields first, as they are written
                // first, and then the record they extend.
                Some(b) => {
                    let labels: Vec<InternedString> =
                        fields.iter().map(|(l, _)| *l.value()).collect();
                    let mut values: Vec<(Term, Ty)> = fields
                        .iter()
                        .map(|(_, e)| (self.lower_expr(e), self.ty(e.id)))
                        .collect();
                    let n = values.len();
                    values.push((self.lower_expr(b), self.ty(b.id)));
                    let order: Vec<u32> = (0..=n as u32).collect();
                    self.in_order(values, &order, |_, mut parts| {
                        let base = parts.pop().expect("the record extended");
                        labels.into_iter().zip(parts).fold(base, |term, (l, e)| {
                            Term::Extend(Arc::new(term), l, Arc::new(e))
                        })
                    })
                }
            },
            hir::Expr::Update(base, fields) => self.lower_update(base, fields),
            hir::Expr::Field(obj, label) => Term::Sel(
                Arc::new(self.lower_expr(obj)),
                *label.value(),
                self.ty(expr.id),
            ),

            hir::Expr::Handle(body, arms, ret) => {
                let lbody = Arc::new(self.lower_expr(body));
                let clauses = arms
                    .iter()
                    .map(|arm| {
                        let (param, param_ty, refutable) = self.pat_binder(&arm.param);
                        let body = self.with_pat_prelude(param, refutable, &arm.body);
                        HClause {
                            effect: arm.effect,
                            op: arm.op,
                            param,
                            param_ty,
                            resume: *arm.resume.value(),
                            resume_ty: self.ty(arm.resume.id),
                            body,
                        }
                    })
                    .collect();
                let ret = ret.as_ref().map(|(pat, rbody)| {
                    let (v, vty, refutable) = self.pat_binder(pat);
                    (v, vty, Arc::new(self.with_pat_prelude(v, refutable, rbody)))
                });
                Term::Handle {
                    body: lbody,
                    clauses,
                    ret,
                    ty: self.ty(expr.id),
                }
            }

            hir::Expr::Error => Term::Error,
        }
    }

    /// `(binder var, its type, Some(pat) if the pattern is refutable /
    /// structured)`.
    ///
    /// The type is the pattern's, whether the binder was written or invented:
    /// `\(a, b) -> e` binds one variable of the pair's type and takes it apart
    /// underneath.
    fn pat_binder<'p>(&mut self, pat: &'p hir::LPat) -> (Var, Ty, Option<&'p hir::LPat>) {
        let ty = self.ty(pat.id);
        // An annotation says nothing at run time, so `(n : Int)` binds exactly
        // as `n` does -- rather than as a pattern to take apart, which cost an
        // annotated parameter a closure and a match on every call.
        match unannotated(pat).value() {
            hir::Pat::Var(id) => (*id.value(), ty, None),
            hir::Pat::Wildcard => (self.vars.fresh(), ty, None),
            _ => (self.vars.fresh(), ty, Some(pat)),
        }
    }

    /// Lower `body`, prefixing `let`s that destructure `var` per `pat`.
    fn with_pat_prelude(&mut self, var: Var, pat: Option<&hir::LPat>, body: &hir::LExpr) -> Term {
        let lowered = self.lower_expr(body);
        let term = self.at(body.span, lowered);
        self.with_pat_prelude_term(var, pat, term)
    }

    fn lower_let_bind(&mut self, bind: &hir::Bind, body: Term, result: &Ty) -> Term {
        match bind {
            hir::Bind::Fun(name, params, _, fbody) => {
                let v = *name.value();
                let poly = self.poly_of(v, name.id);
                let term = Self::ty_lam(&poly, self.curry_lam(params, fbody));
                Term::LetRec(vec![(v, poly, term)], Arc::new(body))
            }
            hir::Bind::Pat(pat, expr) => {
                let rhs = self.lower_expr(expr);
                let bare = unannotated(pat);
                match bare.value() {
                    hir::Pat::Var(id) => {
                        let v = *id.value();
                        let poly = self.poly_of(v, bare.id);
                        let rhs = Self::ty_lam(&poly, rhs);
                        Term::Let(v, poly, Arc::new(rhs), Arc::new(body))
                    }
                    hir::Pat::Wildcard => Term::Let(
                        self.vars.fresh(),
                        Poly::mono(self.ty(pat.id)),
                        Arc::new(rhs),
                        Arc::new(body),
                    ),
                    // A structural binder: one arm, and the `case` produces
                    // whatever the body does.
                    _ => self.lower_case(
                        rhs,
                        self.ty(pat.id),
                        vec![(Some(pat), None, body)],
                        result.clone(),
                    ),
                }
            }
            hir::Bind::Error => body,
        }
    }

    /// Build `(v, access)` pairs for every variable an irrefutable pattern binds,
    /// where `access` is a term extracting that piece from `scrut`.
    fn bind_pat(&mut self, scrut: Term, pat: &hir::LPat, out: &mut Vec<(Var, Ty, Term)>) {
        match pat.value() {
            hir::Pat::Wildcard | hir::Pat::Unit | hir::Pat::Lit(_) => {}
            hir::Pat::Var(id) => out.push((*id.value(), self.ty(pat.id), scrut)),
            // Types are erased here; the pattern under the annotation is all
            // that binds anything.
            hir::Pat::Ann(inner, _) => self.bind_pat(scrut, inner, out),
            hir::Pat::As(id, sub) => {
                out.push((*id.value(), self.ty(pat.id), scrut.clone()));
                self.bind_pat(scrut, sub, out);
            }
            hir::Pat::Tuple(items) => {
                for (i, p) in items.iter().enumerate() {
                    self.bind_pat(Term::Proj(Arc::new(scrut.clone()), i), p, out);
                }
            }
            hir::Pat::Record(fields, _) => {
                for (label, p) in fields {
                    let field = Term::Sel(Arc::new(scrut.clone()), *label.value(), self.ty(p.id));
                    self.bind_pat(field, p, out);
                }
            }
            hir::Pat::Error => {}
            // `(f -> p)`: `f` applied once, and its answer taken apart.
            hir::Pat::View(f, p) => {
                let answer = self.vars.fresh();
                let lf = self.lower_expr(f);
                let applied = self.at(f.span, Term::App(Arc::new(lf), Arc::new(scrut)));
                out.push((answer, self.ty(p.id), applied));
                self.bind_pat(Term::Var(answer), p, out);
            }
            // Refutable in an irrefutable position: fall back to a single-arm Case.
            hir::Pat::Cons(..) | hir::Pat::List(..) | hir::Pat::Array(..) => {
                let mut inner = Vec::new();
                collect_pat_vars(pat, &mut inner);
                if has_view(pat) {
                    for (v, vty) in inner {
                        let ty = self.ty(vty);
                        let case = self.lower_case(
                            scrut.clone(),
                            self.ty(pat.id),
                            vec![(Some(pat), None, Term::Var(v))],
                            ty.clone(),
                        );
                        out.push((v, ty, case));
                    }
                    return;
                }
                let core_pat = self.lower_pat(pat);
                for (v, vty) in inner {
                    let ty = self.ty(vty);
                    out.push((
                        v,
                        ty.clone(),
                        Term::Case(
                            Arc::new(scrut.clone()),
                            vec![(core_pat.clone(), None, Term::Var(v))],
                            ty,
                        ),
                    ));
                }
            }
        }
    }

    /// A pattern with no view in it.
    fn lower_pat(&mut self, pat: &hir::LPat) -> Pat {
        let mut views = Vec::new();
        let lowered = self.lower_pat_views(pat, &mut views);
        debug_assert!(views.is_empty(), "a view is matched by `lower_case`");
        lowered
    }

    /// `pat`, with each view in it -- not those inside a view's own pattern
    /// -- a fresh variable, in `views` with its type, what it applies and
    /// the pattern its answer has to match, in the order they are written.
    fn lower_pat_views<'h>(&mut self, pat: &'h hir::LPat, views: &mut Vec<View<'h>>) -> Pat {
        let mut lower_pat = |this: &mut Self, p: &'h hir::LPat| this.lower_pat_views(p, views);
        match pat.value() {
            hir::Pat::View(f, p) => {
                let v = self.vars.fresh();
                let ty = self.ty(pat.id);
                views.push((v, f, p));
                Pat::Var(v, ty)
            }
            hir::Pat::Wildcard => Pat::Wild,
            hir::Pat::Unit => Pat::Lit(Lit::Unit),
            hir::Pat::Var(id) => Pat::Var(*id.value(), self.ty(pat.id)),
            // Types are gone by here; the annotation did its work in inference.
            hir::Pat::Ann(inner, _) => lower_pat(self, inner),
            hir::Pat::As(id, sub) => {
                let ty = self.ty(pat.id);
                Pat::As(*id.value(), ty, Box::new(lower_pat(self, sub)))
            }
            hir::Pat::Lit(hir::Lit::Int(i)) => Pat::Lit(self.int_lit(pat.id, *i)),
            hir::Pat::Lit(hir::Lit::Float(b)) => Pat::Lit(self.float_lit(pat.id, *b)),
            hir::Pat::Lit(hir::Lit::String(s)) => Pat::Lit(Lit::Str(*s)),
            hir::Pat::Lit(hir::Lit::Char(c)) => Pat::Lit(Lit::Char(*c)),
            hir::Pat::Tuple(items) => {
                Pat::Tuple(items.iter().map(|p| lower_pat(self, p)).collect())
            }
            hir::Pat::Array(items) => {
                Pat::Array(items.iter().map(|p| lower_pat(self, p)).collect())
            }
            // `[a; b; c]` — the same `Cons`/`Nil` chain as the expression form.
            // Its elements in order, so that their views are.
            hir::Pat::List(items) => {
                let heads: Vec<Pat> = items.iter().map(|p| lower_pat(self, p)).collect();
                let nil = Pat::Ctor(InternedString::from("List.Nil"), vec![]);
                heads.into_iter().rev().fold(nil, |acc, head| {
                    Pat::Ctor(InternedString::from("List.Cons"), vec![head, acc])
                })
            }
            hir::Pat::Cons(label, args) => {
                let name = *label.value();
                let lowered: Vec<Pat> = args.iter().map(|p| lower_pat(self, p)).collect();
                match (&*name, lowered.len()) {
                    ("Bool.True", 0) => Pat::Lit(Lit::Bool(true)),
                    ("Bool.False", 0) => Pat::Lit(Lit::Bool(false)),
                    _ => Pat::Ctor(name, lowered),
                }
            }
            hir::Pat::Record(fields, _) => Pat::Record(
                fields
                    .iter()
                    .map(|(l, p)| (*l.value(), lower_pat(self, p)))
                    .collect(),
            ),
            hir::Pat::Error => Pat::Wild,
        }
    }

    /// `match scrut with arms`, where an arm's pattern is `None` for `_` and
    /// its guard and body are lowered already.
    ///
    /// Views are what a core `case` cannot say: a pattern matches without
    /// running anything. So an arm with views matches the rest of its pattern
    /// with a variable where each view is, and then, in its body, applies
    /// each view in turn and matches its answer. A view that does not match
    /// -- or the guard, after them all -- goes on to the arms after this one:
    /// a join, `k ()`, whose body matches the scrutinee against those, and
    /// which the `case` enters too when this arm's pattern does not match.
    fn lower_case(
        &mut self,
        scrut: Term,
        scrut_ty: Ty,
        arms: Vec<(Option<&hir::LPat>, Option<Term>, Term)>,
        result: Ty,
    ) -> Term {
        let Some(at) = arms.iter().position(|(p, ..)| p.is_some_and(has_view)) else {
            let arms = arms
                .into_iter()
                .map(|(p, g, b)| (p.map_or(Pat::Wild, |p| self.lower_pat(p)), g, b))
                .collect();
            return Term::Case(Arc::new(scrut), arms, result);
        };
        // The scrutinee is matched again by the arms after: evaluated once.
        // A tuple of names or literals, `match (a, e) with`, is written out
        // again instead, where it costs nothing: taken apart where it is
        // made, it is never built.
        let atom = |t: &Term| matches!(t, Term::Var(_) | Term::Lit(_));
        let (s, bound) = match scrut {
            Term::Tuple(ref xs) if xs.iter().all(atom) => (scrut.clone(), None),
            Term::Var(v) => (Term::Var(v), None),
            other => {
                let v = self.vars.fresh();
                (Term::Var(v), Some((v, other)))
            }
        };
        let mut arms = arms;
        let rest: Vec<_> = arms.split_off(at + 1);
        let (pat, guard, body) = arms.pop().expect("the arm with the view");
        // What failing does: the arms after, or -- when that is one `_` whose
        // body is only a jump -- that jump, with no join of its own.
        let (fail, join) = match rest.as_slice() {
            [(None, None, jump @ Term::App(f, a))]
                if matches!(**f, Term::Var(_)) && matches!(**a, Term::Lit(Lit::Unit)) =>
            {
                (jump.clone(), None)
            }
            _ => {
                let k = self.vars.fresh();
                let jump = Term::App(Arc::new(Term::Var(k)), Arc::new(Term::Lit(Lit::Unit)));
                let others = self.lower_case(s.clone(), scrut_ty.clone(), rest, result.clone());
                (jump, Some((k, others)))
            }
        };
        let mut views = Vec::new();
        let head = self.lower_pat_views(pat.expect("an arm with a view has a pattern"), &mut views);
        let body = self.after_views(&views, guard, body, &fail, &result);
        let mut out: Vec<(Pat, Option<Term>, Term)> = arms
            .into_iter()
            .map(|(p, g, b)| (p.map_or(Pat::Wild, |p| self.lower_pat(p)), g, b))
            .collect();
        out.push((head, None, body));
        out.push((Pat::Wild, None, fail));
        let mut term = Term::Case(Arc::new(s), out, result.clone());
        if let Some((k, others)) = join {
            let u = self.vars.fresh();
            let kty = InferType::Fun(
                vec![InferType::unit()],
                Box::new(result.clone()),
                Box::new(InferType::RowEmpty),
            );
            let lam = Term::Lam(u, InferType::unit(), Arc::new(others));
            term = Term::Let(k, Poly::mono(kty), Arc::new(lam), Arc::new(term));
        }
        if let Some((v, rhs)) = bound {
            term = Term::Let(v, Poly::mono(scrut_ty), Arc::new(rhs), Arc::new(term));
        }
        term
    }

    /// An arm's body once its pattern has matched: each of `views` applied and
    /// its answer matched in turn, then the guard, and `fail` wherever one of
    /// them says no.
    fn after_views(
        &mut self,
        views: &[View<'_>],
        guard: Option<Term>,
        body: Term,
        fail: &Term,
        result: &Ty,
    ) -> Term {
        let Some(((v, f, p), later)) = views.split_first() else {
            return match guard {
                Some(g) => Term::If(Arc::new(g), Arc::new(body), Arc::new(fail.clone())),
                None => body,
            };
        };
        let inner = self.after_views(later, guard, body, fail, result);
        let lf = self.lower_expr(f);
        let applied = self.at(f.span, Term::App(Arc::new(lf), Arc::new(Term::Var(*v))));
        self.lower_case(
            applied,
            self.ty(p.id),
            vec![(Some(*p), None, inner), (None, None, fail.clone())],
            result.clone(),
        )
    }

    /// Build a constructor application, eta-expanding an under-applied one so a
    /// bare `Cons` / `Just` can still be passed around as a function.
    fn ctor(&mut self, name: InternedString, args: Vec<Term>, ty: Ty) -> Term {
        let arity = self.ctor_arity.get(&name).copied().unwrap_or(args.len());
        if args.len() >= arity {
            return Term::Ctor(name, ty, args);
        }
        // Under-applied: `\(x : T) … -> K … x …`, with the parameter types and
        // the result read off the function type this mention was inferred at.
        let (params, result) = peel_arrows(&ty, arity - args.len());
        let extra: Vec<Var> = (args.len()..arity).map(|_| self.vars.fresh()).collect();
        let mut all = args;
        all.extend(extra.iter().map(|v| Term::Var(*v)));
        let body = Term::Ctor(name, result, all);
        extra
            .into_iter()
            .zip(params)
            .rev()
            .fold(body, |acc, (v, t)| Term::Lam(v, t, Arc::new(acc)))
    }

    /// `build` of `values`, in their own order, with them evaluated in the
    /// order `order` lists their places in: each but the last bound to a
    /// variable first, in that order -- unless it is a value already, a
    /// variable, a literal or a function, whose place makes no difference --
    /// and the last evaluated in its own place, after all of them.
    fn in_order(
        &mut self,
        values: Vec<(Term, Ty)>,
        order: &[u32],
        build: impl FnOnce(&mut Self, Vec<Term>) -> Term,
    ) -> Term {
        let mut parts: Vec<Option<(Term, Ty)>> = values.into_iter().map(Some).collect();
        let mut args: Vec<Term> = Vec::with_capacity(parts.len());
        let mut bound: Vec<(usize, Var, Ty, Term)> = Vec::new();
        let early = order.len().saturating_sub(1);
        for &at in &order[..early] {
            let at = at as usize;
            let Some((term, ty)) = parts.get_mut(at).and_then(Option::take) else {
                continue;
            };
            if matches!(term, Term::Var(_) | Term::Lit(_) | Term::Lam(..)) {
                parts[at] = Some((term, ty));
            } else {
                bound.push((at, self.vars.fresh(), ty, term));
            }
        }
        for (at, part) in parts.into_iter().enumerate() {
            match part {
                Some((term, _)) => args.push(term),
                None => {
                    let v = bound.iter().find(|b| b.0 == at).expect("bound").1;
                    args.push(Term::Var(v));
                }
            }
        }
        let body = build(self, args);
        bound.into_iter().rev().fold(body, |acc, (_, v, ty, e)| {
            Term::Let(v, Poly::mono(ty), Arc::new(e), Arc::new(acc))
        })
    }

    /// `{ base | x = v }`.
    ///
    /// A record type's value is built again: `let b = base; let x' = v in
    /// T { x = x', y = b.y }` -- the values in the order they were written, and
    /// every other field read from the one value. An anonymous record's
    /// fields are replaced one after another, as extending it with a field it
    /// has does.
    fn lower_update(&mut self, base: &hir::LExpr, fields: &[(hir::Label, hir::LExpr)]) -> Term {
        let bty = self.ty(base.id);
        let lbase = self.lower_expr(base);
        let sig = match (&bty, self.variants) {
            (InferType::Con(tyname, _), Some(variants)) => match variants.get(tyname) {
                Some(vs) if vs.len() == 1 && vs[0].labels.is_some() => Some(vs[0].clone()),
                _ => None,
            },
            _ => None,
        };
        let Some(sig) = sig else {
            // Every value first, and then the fields replaced: the record is
            // used for the last time by the first replacement, rather than
            // held while the values after it are computed -- which, since
            // they read from it, they usually are -- so a back end can
            // replace the field in place when nothing else holds the record.
            let b = self.vars.fresh();
            let values: Vec<(InternedString, Var, Ty, Term)> = fields
                .iter()
                .map(|(l, e)| {
                    (
                        *l.value(),
                        self.vars.fresh(),
                        self.ty(l.id),
                        self.lower_expr(e),
                    )
                })
                .collect();
            let updated = values.iter().fold(Term::Var(b), |term, (l, v, ..)| {
                Term::Extend(Arc::new(term), *l, Arc::new(Term::Var(*v)))
            });
            let body = values.into_iter().rev().fold(updated, |acc, (_, v, t, e)| {
                Term::Let(v, Poly::mono(t), Arc::new(e), Arc::new(acc))
            });
            return Term::Let(b, Poly::mono(bty), Arc::new(lbase), Arc::new(body));
        };
        let InferType::Con(_, targs) = &bty else {
            unreachable!("only a named type has constructors")
        };
        let map: HashMap<u32, Ty> = targs
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u32, t.clone()))
            .collect();
        let b = self.vars.fresh();
        let values: Vec<(InternedString, Var, Ty, Term)> = fields
            .iter()
            .map(|(l, e)| {
                (
                    *l.value(),
                    self.vars.fresh(),
                    self.ty(l.id),
                    self.lower_expr(e),
                )
            })
            .collect();
        let labels = sig.labels.clone().unwrap_or_default();
        let args = labels
            .iter()
            .zip(&sig.fields)
            .map(
                |(name, fty)| match values.iter().find(|(n, ..)| n == name) {
                    Some((_, v, ..)) => Term::Var(*v),
                    None => Term::Sel(Arc::new(Term::Var(b)), *name, subst_bound(fty, &map)),
                },
            )
            .collect();
        let rebuilt = Term::Ctor(sig.name, bty.clone(), args);
        let body = values.into_iter().rev().fold(rebuilt, |acc, (_, v, t, e)| {
            Term::Let(v, Poly::mono(t), Arc::new(e), Arc::new(acc))
        });
        Term::Let(b, Poly::mono(bty), Arc::new(lbase), Arc::new(body))
    }

    /// `\a. \b. prim(a, b)` — used when a primitive is referenced without (or with
    /// the wrong number of) arguments.
    fn eta_prim(&mut self, op: Prim, ty: Ty) -> Term {
        let (params, result) = peel_arrows(&ty, op.arity());
        if op == Prim::RunSt {
            // `\f -> f ()`, as the saturated case in `lower_expr` builds it.
            let f = self.vars.fresh();
            let fty = params.into_iter().next().unwrap_or_else(unknown);
            let call = Term::App(Arc::new(Term::Var(f)), Arc::new(Term::Lit(Lit::Unit)));
            return Term::Lam(f, fty, Arc::new(call));
        }
        let vars: Vec<Var> = (0..op.arity()).map(|_| self.vars.fresh()).collect();
        let body = Term::Prim(op, vars.iter().map(|v| Term::Var(*v)).collect(), result);
        vars.into_iter()
            .zip(params)
            .rev()
            .fold(body, |acc, (v, t)| Term::Lam(v, t, Arc::new(acc)))
    }
}

/// A view taken out of a pattern by `lower_pat_views`: the variable standing
/// where it was, what it applies, and the pattern its answer has to match.
type View<'h> = (Var, &'h hir::LExpr, &'h hir::LPat);

/// Whether `pat` has a view anywhere in it.
fn has_view(pat: &hir::LPat) -> bool {
    match pat.value() {
        hir::Pat::View(..) => true,
        hir::Pat::Ann(p, _) => has_view(p),
        hir::Pat::As(_, p) => has_view(p),
        hir::Pat::Tuple(ps) | hir::Pat::Array(ps) | hir::Pat::List(ps) | hir::Pat::Cons(_, ps) => {
            ps.iter().any(has_view)
        }
        hir::Pat::Record(fields, _) => fields.iter().any(|(_, p)| has_view(p)),
        hir::Pat::Wildcard
        | hir::Pat::Var(_)
        | hir::Pat::Lit(_)
        | hir::Pat::Unit
        | hir::Pat::Error => false,
    }
}

/// Every variable an irrefutable pattern binds, with the node whose inferred
/// type is that variable's.
fn collect_pat_vars(pat: &hir::LPat, out: &mut Vec<(Var, hir::NodeId)>) {
    match pat.value() {
        hir::Pat::Var(id) => out.push((*id.value(), pat.id)),
        hir::Pat::As(id, sub) => {
            out.push((*id.value(), pat.id));
            collect_pat_vars(sub, out);
        }
        hir::Pat::Tuple(items)
        | hir::Pat::List(items)
        | hir::Pat::Array(items)
        | hir::Pat::Cons(_, items) => items.iter().for_each(|p| collect_pat_vars(p, out)),
        hir::Pat::Record(fields, _) => fields.iter().for_each(|(_, p)| collect_pat_vars(p, out)),
        hir::Pat::Ann(p, _) => collect_pat_vars(p, out),
        hir::Pat::View(_, p) => collect_pat_vars(p, out),
        _ => {}
    }
}

/// Split a function type into `n` parameter types and what is left.
///
/// Used where lowering invents a lambda — an under-applied constructor, a
/// primitive mentioned without arguments — and has to annotate parameters it
/// did not get from the source.
fn peel_arrows(ty: &Ty, n: usize) -> (Vec<Ty>, Ty) {
    let mut params = Vec::with_capacity(n);
    let mut rest = ty.clone();
    for _ in 0..n {
        match rest {
            InferType::Fun(args, ret, _) => {
                params.push(args.first().cloned().unwrap_or_else(unknown));
                rest = *ret;
            }
            _ => {
                params.push(unknown());
                rest = unknown();
            }
        }
    }
    (params, rest)
}

/// `pat` with its annotations taken off: `((n : Int) : Int)` is `n`.
///
/// An annotation constrains inference and means nothing afterwards, so every
/// question lowering asks of a pattern's *shape* -- is it one variable? -- is
/// asked of what is under it.
fn unannotated(pat: &hir::LPat) -> &hir::LPat {
    let mut bare = pat;
    while let hir::Pat::Ann(inner, _) = bare.value() {
        bare = inner;
    }
    bare
}
