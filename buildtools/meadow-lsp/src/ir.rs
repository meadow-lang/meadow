//! A definition of a document in the compiler's IRs -- core, Cut, AxCut --
//! as text, with what the parts of the text are and where the name under the
//! cursor is in it: what an editor shows beside the source.
//!
//! Core is the definition as the front end left it. Cut is that lowered by
//! `meadow_seq::cut`, which says which declaration and variable of the Cut
//! each variable of the source is. AxCut is what the runtimes are handed:
//! the program the definition is part of lowered whole, in which a variable
//! of the source keeps its number, and so is found by it.

use crate::analysis::Analysis;
use meadow_compiler::intern::InternedString;
use meadow_compiler::{CompiledPackage, core, hir::VarId};
use std::collections::HashMap;
use std::rc::Rc;

/// What a document's definitions are lowered from, kept with its analysis.
#[derive(Default)]
pub struct IrSource {
    /// The package's definitions, as core.
    pub defs: Vec<core::Def>,
    pub variants: meadow_compiler::infer::VariantEnv,
    pub ctor_fields: HashMap<InternedString, Vec<InternedString>>,
    /// The definitions of other packages that the package's mention: where
    /// each is, and its type.
    pub outside: HashMap<VarId, (InternedString, InternedString, core::Poly)>,
    /// The packages it depends on, besides the standard library.
    pub deps: Vec<Rc<CompiledPackage>>,
}

impl IrSource {
    /// Of `pkg`, compiled against `others` -- of which `deps` are the ones
    /// that are not the standard library.
    pub fn of<'a>(
        pkg: &CompiledPackage,
        others: impl IntoIterator<Item = &'a CompiledPackage>,
        deps: Vec<Rc<CompiledPackage>>,
    ) -> IrSource {
        let own: std::collections::HashSet<VarId> = pkg.defs.iter().map(|d| d.var).collect();
        let mut wanted = Vec::new();
        for d in &pkg.defs {
            meadow_seq::cut::mentioned(&d.term, &mut wanted);
        }
        let wanted: std::collections::HashSet<VarId> =
            wanted.into_iter().filter(|v| !own.contains(v)).collect();
        let mut outside = HashMap::new();
        for other in others {
            for d in &other.defs {
                if wanted.contains(&d.var) {
                    outside.insert(d.var, (d.module, d.name, d.poly.clone()));
                }
            }
        }
        IrSource {
            defs: pkg.defs.clone(),
            variants: pkg.variants.clone(),
            ctor_fields: pkg.ctor_fields.clone(),
            outside,
            deps,
        }
    }
}

/// A definition in one IR.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The definition's name, as the source spells it.
    pub name: String,
    pub text: String,
    pub segments: Vec<Segment>,
    /// Where in `text` the name under the cursor is: bytes `start..end`.
    pub focus: Vec<(usize, usize)>,
}

/// Bytes `start..end` of a view's text, and what is written there.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    /// `declaration`, `variable` or `symbol`.
    pub kind: &'static str,
    pub name: String,
}

impl Analysis {
    /// The definition `offset` is in, and the variable there, if it is on
    /// one: a name's own definition, the definition a local name is bound
    /// in, or the one written last before `offset`.
    fn definition_around(&self, offset: usize) -> Option<(&core::Def, Option<VarId>)> {
        let by_var = |v: VarId| self.ir.defs.iter().find(|d| d.var == v);
        if let Some(v) = self.var_at(offset) {
            if let Some(d) = by_var(v) {
                // On a definition's own name where it is written; a mention
                // of it elsewhere is part of the definition it is in.
                let at_home = self.functions.iter().any(|f| {
                    f.var == v && f.span.start as usize <= offset && offset <= f.span.end as usize
                });
                if at_home {
                    return Some((d, Some(v)));
                }
            }
            if let Some(d) = self.ir.defs.iter().find(|d| {
                d.var != v && meadow_seq::cut::names(&d.term, v) && self.encloses(d, offset)
            }) {
                return Some((d, Some(v)));
            }
        }
        let f = self
            .functions
            .iter()
            .filter(|f| f.span.start as usize <= offset)
            .max_by_key(|f| f.span.start)?;
        Some((by_var(f.var)?, self.var_at(offset)))
    }

    /// Whether `d` is the definition written last before `offset`: the one
    /// `offset` is inside, of those of this document.
    fn encloses(&self, d: &core::Def, offset: usize) -> bool {
        self.functions
            .iter()
            .filter(|f| f.span.start as usize <= offset)
            .max_by_key(|f| f.span.start)
            .is_some_and(|f| f.var == d.var)
    }

    /// The definition around `offset` in the IR called `which` -- `core`,
    /// `cut` or `axcut` -- with where the name at `offset` is in it. `std`
    /// is the standard library's packages, which a program is lowered with.
    pub fn ir_at(&self, offset: usize, which: &str, std: &[CompiledPackage]) -> Option<View> {
        let (def, focus) = self.definition_around(offset)?;
        let name = meadow_compiler::hir::spell_name(&def.name).to_string();
        match which {
            "core" => Some(View {
                name,
                text: core::Program {
                    defs: vec![def.clone()],
                    entry: None,
                    ctor_fields: Default::default(),
                    variants: Default::default(),
                    origins: Default::default(),
                    migrations: Default::default(),
                }
                .pretty(),
                segments: Vec::new(),
                focus: Vec::new(),
            }),
            "cut" => Some(self.cut_of(def, focus, name)),
            "axcut" => Some(self.axcut_of(def, focus, name, std)),
            _ => None,
        }
    }

    fn cut_of(&self, def: &core::Def, focus: Option<VarId>, name: String) -> View {
        use meadow_cut::print::Part;
        use meadow_seq::cut::{Outside, symbol_of, to_cut_in};
        let mut outside: HashMap<VarId, Outside> = self
            .ir
            .outside
            .iter()
            .map(|(v, (module, name, poly))| {
                (
                    *v,
                    Outside {
                        symbol: symbol_of(module, name),
                        poly: poly.clone(),
                    },
                )
            })
            .collect();
        for d in self.ir.defs.iter().filter(|d| d.var != def.var) {
            outside.insert(
                d.var,
                Outside {
                    symbol: symbol_of(&d.module, &d.name),
                    poly: d.poly.clone(),
                },
            );
        }
        let lowered = to_cut_in(
            &core::Program {
                defs: vec![def.clone()],
                entry: None,
                ctor_fields: self.ir.ctor_fields.clone(),
                variants: self.ir.variants.clone(),
                origins: Default::default(),
                migrations: Default::default(),
            },
            &outside,
        );
        let listing = meadow_cut::print::listing(&lowered.program);
        let own = &lowered.symbols[&def.var];
        // Its own declaration, and those made of its local functions, which
        // are named for it.
        let local = own.path.last().map(|n| format!("{n}#l"));
        let its = |of: &meadow_cut::Symbol| {
            of == own
                || (of.package == own.package
                    && of.path.len() == own.path.len()
                    && of.path[..of.path.len() - 1] == own.path[..own.path.len() - 1]
                    && of
                        .path
                        .last()
                        .zip(local.as_ref())
                        .is_some_and(|(n, l)| n.starts_with(l.as_str())))
        };
        // What the cursor is on, in the Cut's terms: a variable of one of
        // these declarations, or a definition by its symbol.
        let on_var = focus.and_then(|v| lowered.names.get(&v));
        let on_symbol = focus.and_then(|v| lowered.symbols.get(&v));
        let mut view = View {
            name,
            text: String::new(),
            segments: Vec::new(),
            focus: Vec::new(),
        };
        for decl in &listing.segments {
            let Part::Decl(of) = &decl.part else { continue };
            if !its(of) {
                continue;
            }
            if !view.text.is_empty() {
                view.text.push_str("\n\n");
            }
            let base = view.text.len();
            view.text.push_str(&listing.text[decl.start..decl.end]);
            for s in &listing.segments {
                if s.start < decl.start || s.end > decl.end {
                    continue;
                }
                let (start, end) = (base + s.start - decl.start, base + s.end - decl.start);
                let (kind, named, focused) = match &s.part {
                    Part::Decl(d) => ("declaration", d.to_string(), false),
                    Part::Var { of, name } => (
                        "variable",
                        name.clone(),
                        on_var.is_some_and(|(o, n)| o == of && n == name),
                    ),
                    Part::Symbol { symbol, .. } => {
                        ("symbol", symbol.to_string(), on_symbol == Some(symbol))
                    }
                };
                if focused {
                    view.focus.push((start, end));
                }
                view.segments.push(Segment {
                    start,
                    end,
                    kind,
                    name: named,
                });
            }
        }
        view
    }

    fn axcut_of(
        &self,
        def: &core::Def,
        focus: Option<VarId>,
        name: String,
        std: &[CompiledPackage],
    ) -> View {
        // The program the definition is part of: what it reaches, of this
        // package, of what the package depends on, and of the library.
        let packages = || std.iter().chain(self.ir.deps.iter().map(|p| p.as_ref()));
        let mut deps = core::prune::Deps::new(&self.ir.defs);
        let mut ctor_fields = self.ir.ctor_fields.clone();
        let mut variants = self.ir.variants.clone();
        for p in packages() {
            deps.add(&p.defs);
            ctor_fields.extend(p.ctor_fields.clone());
            variants.extend(p.variants.clone());
        }
        let keep = core::prune::reach_all(&[&deps], Some(def.var));
        let program = core::Program {
            defs: packages()
                .flat_map(|p| p.defs.iter())
                .chain(self.ir.defs.iter())
                .filter(|d| keep.contains(&d.var))
                .cloned()
                .collect(),
            entry: None,
            ctor_fields,
            variants,
            origins: Default::default(),
            migrations: Default::default(),
        };
        let lowered = meadow_seq::lower_program(&program, meadow_compiler::Options::debug().opt);
        let listing = lowered.program.listing();
        // What is the definition's own: the variables it binds, which a
        // block made of one of its local functions still has by number.
        let own: std::collections::HashSet<VarId> = meadow_seq::cut::variables(&def.term)
            .into_iter()
            .filter(|v| !keep.contains(v))
            .collect();
        let mut view = View {
            name,
            text: String::new(),
            segments: Vec::new(),
            focus: Vec::new(),
        };
        for (start, end, label) in &listing.defs {
            let Some(block) = lowered.program.defs.iter().find(|d| d.label == *label) else {
                continue;
            };
            let its = block.name == def.name
                || listing
                    .names
                    .iter()
                    .any(|(s, e, n)| start <= s && e <= end && own.contains(n));
            if !its {
                continue;
            }
            if !view.text.is_empty() {
                view.text.push('\n');
            }
            let base = view.text.len();
            view.text.push_str(&listing.text[*start..*end]);
            view.segments.push(Segment {
                start: base,
                end: view.text.len(),
                kind: "declaration",
                name: block.name.to_string(),
            });
            for (s, e, n) in &listing.names {
                if s < start || e > end {
                    continue;
                }
                let (s, e) = (base + s - start, base + e - start);
                if focus == Some(*n) {
                    view.focus.push((s, e));
                }
                view.segments.push(Segment {
                    start: s,
                    end: e,
                    kind: "variable",
                    name: format!("v{}", n.0),
                });
            }
        }
        if view.text.is_empty() {
            view.text = format!(
                "-- `{}` is lowered to no block of its own: it was inlined where it is used\n",
                view.name
            );
        }
        view
    }
}
