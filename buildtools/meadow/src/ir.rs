//! **A definition in every form the compiler puts it in**: the pages of the
//! REPL's `:ir`.
//!
//! In the order a program passes through them: core, as the front end leaves
//! it; Cut, the sequent calculus a front end hands the back end; AxCut, what
//! the runtimes are handed; then each runtime's own -- Glade's bytecode, the
//! machine code Glade's JIT makes of it, and the LLVM IR a Silo build
//! compiles. Every form but core is of the whole program the definition is
//! part of, and the page is the part of it that is the definition's own.
//!
//! Which part that is is found by name: the definitions shown are renamed,
//! with a mark no name can have, before the program is lowered, and every
//! block lowering makes of one -- its worker, its continuations, a copy a
//! pass made -- carries the mark; so does a block that binds one of the
//! definition's own variables, as the language server counts one. A name
//! alone would not do: an entry of the REPL that mentions `it` reaches the
//! `it` of the entry before it.

use crate::viewer::{Notation, Page};
use meadow_compiler::intern::InternedString;
use meadow_compiler::{OptLevel, core, hir};
use std::collections::HashSet;
use std::fmt::Write;

/// The forms, by what `:ir <which> <name>` calls them, in order.
pub const FORMS: [&str; 6] = ["core", "cut", "axcut", "bytecode", "native", "llvm"];

/// What marks a definition being shown, at the start of its name.
const MARK: &str = "\u{1}";

/// The definitions `shown` in each of the forms -- or only in the one called
/// `only` -- out of `program`, which holds them and everything they reach.
/// `opt` is the optimisation level the program is lowered at.
pub fn pages(
    program: &core::Program,
    shown: &[core::Var],
    opt: OptLevel,
    only: Option<&str>,
) -> Vec<Page> {
    let wanted = |form: &str| only.is_none_or(|o| o == form);
    let mut pages = Vec::new();
    if wanted("core") {
        pages.push(Page {
            title: "Core".into(),
            text: core_text(program, shown),
            notation: Notation::Core,
        });
    }
    if wanted("cut") {
        pages.push(Page {
            title: "Cut".into(),
            text: cut_text(program, shown),
            notation: Notation::Cut,
        });
    }
    if FORMS[2..].iter().any(|f| wanted(f)) {
        let lowered = Lowered::new(program, shown, opt);
        if wanted("axcut") {
            pages.push(Page {
                title: "AxCut".into(),
                text: lowered.axcut(),
                notation: Notation::AxCut,
            });
        }
        let image = (wanted("bytecode") || wanted("native")).then(|| lowered.image());
        if let Some(image) = &image {
            if wanted("bytecode") {
                pages.push(Page {
                    title: "Bytecode".into(),
                    text: match image {
                        Ok(image) => bytecode_text(image),
                        Err(e) => format!("; {e}\n"),
                    },
                    notation: Notation::Bytecode,
                });
            }
            // Glade's JIT compiles for the two machines it knows.
            if wanted("native")
                && let Some(arch) = meadow_glade::codegen::Arch::host()
            {
                pages.push(Page {
                    title: format!(
                        "Machine code ({})",
                        match arch {
                            meadow_glade::codegen::Arch::X86_64 => "x86-64",
                            meadow_glade::codegen::Arch::Aarch64 => "AArch64",
                        }
                    ),
                    text: match image {
                        Ok(image) => native_text(image, arch, opt),
                        Err(e) => format!("; {e}\n"),
                    },
                    notation: Notation::Asm,
                });
            }
        }
        if wanted("llvm") {
            pages.push(Page {
                title: "LLVM IR".into(),
                text: lowered.llvm(),
                notation: Notation::Llvm,
            });
        }
    }
    pages
}

fn of<'a>(program: &'a core::Program, shown: &[core::Var]) -> Vec<&'a core::Def> {
    shown
        .iter()
        .filter_map(|v| program.defs.iter().find(|d| d.var == *v))
        .collect()
}

/// The definitions as the front end left them, before the passes that run
/// on a whole program.
fn core_text(program: &core::Program, shown: &[core::Var]) -> String {
    core::Program {
        defs: of(program, shown).into_iter().cloned().collect(),
        entry: None,
        ctor_fields: Default::default(),
        variants: Default::default(),
        origins: Default::default(),
    }
    .pretty()
}

/// Each definition as Cut -- its value, and a definition for each local
/// function of it that calls itself -- as core is lowered to it for showing.
fn cut_text(program: &core::Program, shown: &[core::Var]) -> String {
    let lowered = meadow_seq::cut::to_cut(program);
    let listing = meadow_cut::print::listing(&lowered.program);
    let mut out = String::new();
    for d in of(program, shown) {
        let Some(symbol) = lowered.symbols.get(&d.var) else {
            continue;
        };
        // Its own declaration, and those made of its local functions, which
        // are named for it.
        let local = symbol.path.last().map(|n| format!("{n}#l"));
        let its = |of: &meadow_cut::Symbol| {
            of == symbol
                || (of.package == symbol.package
                    && of.path.len() == symbol.path.len()
                    && of.path[..of.path.len().saturating_sub(1)]
                        == symbol.path[..symbol.path.len().saturating_sub(1)]
                    && of
                        .path
                        .last()
                        .zip(local.as_ref())
                        .is_some_and(|(n, l)| n.starts_with(l.as_str())))
        };
        for s in &listing.segments {
            if let meadow_cut::print::Part::Decl(of) = &s.part
                && its(of)
            {
                let _ = writeln!(out, "{}\n", &listing.text[s.start..s.end]);
            }
        }
    }
    out
}

/// The program lowered to AxCut with the definitions shown marked, and which
/// of its blocks are theirs.
struct Lowered {
    lowered: meadow_seq::Lowered,
    /// Indices into the program's definitions.
    own: Vec<usize>,
}

impl Lowered {
    fn new(program: &core::Program, shown: &[core::Var], opt: OptLevel) -> Lowered {
        let mut marked = program.clone();
        for d in &mut marked.defs {
            if shown.contains(&d.var) {
                d.name = InternedString::from(format!("{MARK}{}", d.name));
            }
        }
        // The program as the REPL runs it: its entry is the definition the
        // entry made last.
        marked.entry = shown.last().copied();
        let lowered = meadow_seq::lower_program(&marked, opt);
        // What is the definitions' own besides what carries the mark: a
        // block that binds a variable of theirs.
        let reached: HashSet<core::Var> = program.defs.iter().map(|d| d.var).collect();
        let vars: HashSet<core::Var> = of(program, shown)
            .into_iter()
            .flat_map(|d| meadow_seq::cut::variables(&d.term))
            .filter(|v| !reached.contains(v))
            .collect();
        let listing = lowered.program.listing();
        let own = lowered
            .program
            .defs
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.name.starts_with(MARK)
                    || listing.defs.iter().any(|(start, end, label)| {
                        *label == b.label
                            && listing
                                .names
                                .iter()
                                .any(|(s, e, n)| start <= s && e <= end && vars.contains(n))
                    })
            })
            .map(|(i, _)| i)
            .collect();
        Lowered { lowered, own }
    }

    /// Every block of the definitions', under a line naming it.
    fn axcut(&self) -> String {
        let mut out = String::new();
        for &i in &self.own {
            let b = &self.lowered.program.defs[i];
            let _ = writeln!(
                out,
                "-- {} (L{})\n{}",
                unmarked(&b.name),
                b.label.0,
                b.block
            );
        }
        if out.is_empty() {
            out.push_str("-- lowered to no block of its own: it was inlined where it is used\n");
        }
        out
    }

    /// The program as Glade's bytecode, with what each block is of: the
    /// definitions' own marked, the blocks found by their variables too, so
    /// that every region compiled of one carries the mark.
    fn image(&self) -> Result<meadow_bytecode::Program, String> {
        if !self.lowered.unsupported.is_empty() {
            return Err(format!(
                "the back end cannot translate {:?} yet",
                self.lowered.unsupported
            ));
        }
        let mut program = self.lowered.program.clone();
        for &i in &self.own {
            let d = &mut program.defs[i];
            if !d.name.starts_with(MARK) {
                d.name = InternedString::from(format!("{MARK}{}", d.name));
            }
        }
        meadow_codegen::compile_with_debug_info(&program).map_err(|e| e.msg)
    }

    /// The LLVM IR a Silo build compiles the definitions' blocks to.
    fn llvm(&self) -> String {
        let mut program = self.lowered.program.clone();
        for d in &mut program.defs {
            d.name = InternedString::from(unmarked(&d.name));
        }
        let Some(entry) = program.entry else {
            return "; the program has no entry point\n".into();
        };
        let arch = match crate::aot::Target::host() {
            Ok(t) => t.call_conv(),
            Err(_) => meadow_llvm::CallConv::for_arch("x86_64"),
        };
        let cycles = crate::aot::cycles().unwrap_or_default();
        let units = match meadow_llvm::with_cycles(cycles, || {
            meadow_llvm::compile_split(&program, meadow_llvm::UNIT, arch)
        }) {
            Ok(units) => units,
            Err(e) => return format!("; the LLVM back end cannot compile this: {}\n", e.msg),
        };
        // A block's function is named for its definition, the `k`th of its
        // name in its module in the order the blocks are made linear, and its
        // methods and frames for it in turn: see
        // `meadow_llvm::emit::Module::name_labels`.
        let blocks = match meadow_llvm::linear::program(&program, &[entry]) {
            Ok(blocks) => blocks,
            Err(e) => return format!("; the LLVM back end cannot compile this: {}\n", e.msg),
        };
        let own: HashSet<meadow_seq::Label> = self
            .own
            .iter()
            .map(|&i| self.lowered.program.defs[i].label)
            .collect();
        let mut count: std::collections::HashMap<String, usize> = Default::default();
        let mut stems = Vec::new();
        for (i, label, _) in &blocks {
            let d = &program.defs[*i];
            let stem = format!(".{}.{}", symbol(&d.module), symbol(&d.name));
            let k = count.entry(stem.clone()).or_default();
            *k += 1;
            if own.contains(label) {
                stems.push(format!("{stem}.{k}"));
            }
        }
        let ours = |name: &str| {
            stems.iter().any(|s| {
                name.strip_prefix("@mw.L") == Some(s.as_str())
                    || ["@mw.M", "@mw.F"].iter().any(|p| {
                        name.strip_prefix(p)
                            .and_then(|n| n.strip_prefix(s.as_str()))
                            .is_some_and(|n| n.starts_with('.'))
                    })
            })
        };
        let mut out = String::new();
        let mut seen = HashSet::new();
        for u in &units {
            for f in functions(&u.text) {
                if ours(f.name) && seen.insert(f.name) {
                    out.push_str(f.text);
                    if !f.text.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
        }
        if out.is_empty() {
            out.push_str("; lowered to no function of its own: it was inlined where it is used\n");
        }
        out
    }
}

/// A name as it was before it was marked.
fn unmarked(name: &str) -> String {
    hir::spell_name(name.strip_prefix(MARK).unwrap_or(name)).to_string()
}

/// A name as LLVM's functions are named for it: `meadow_llvm::emit`'s.
fn symbol(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A function of a module of LLVM IR: its name, and its text, whole.
struct Function<'a> {
    name: &'a str,
    text: &'a str,
}

/// The functions an LLVM module defines, in order.
fn functions(module: &str) -> Vec<Function<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = module[at..].find("define ") {
        let start = at + found;
        // A definition starts a line.
        if start > 0 && !module[..start].ends_with('\n') {
            at = start + 1;
            continue;
        }
        let end = module[start..]
            .find("\n}")
            .map_or(module.len(), |e| start + e + 2);
        let header = &module[start..end];
        let name = header
            .find('@')
            .map(|a| {
                let rest = &header[a..];
                let len = if rest[1..].starts_with('"') {
                    1 + rest[1..][1..].find('"').map_or(rest.len() - 1, |q| q + 2)
                } else {
                    rest.find('(').unwrap_or(rest.len())
                };
                &rest[..len]
            })
            .unwrap_or("");
        out.push(Function {
            name,
            text: &module[start..end],
        });
        at = end;
    }
    out
}

/// The image's blocks that are the definitions', in address order, each
/// under a line naming it.
fn bytecode_text(image: &meadow_bytecode::Program) -> String {
    let mut out = String::new();
    for r in own_regions(image) {
        let _ = writeln!(out, "; {}", unmarked(&r.name));
        for pc in r.entry..r.end {
            let _ = writeln!(
                out,
                "{pc:>6}  {}",
                image.show_at(pc as usize, image.code[pc as usize])
            );
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("; compiled to no block of its own: it was inlined where it is used\n");
    }
    out
}

fn own_regions(image: &meadow_bytecode::Program) -> Vec<&meadow_bytecode::Region> {
    let Some(debug) = &image.debug else {
        return Vec::new();
    };
    let mut regions: Vec<&meadow_bytecode::Region> = debug
        .regions
        .iter()
        .filter(|r| r.name.starts_with(MARK))
        .collect();
    regions.sort_by_key(|r| r.entry);
    regions
}

/// The machine code Glade's JIT compiles the definitions' blocks to.
fn native_text(
    image: &meadow_bytecode::Program,
    arch: meadow_glade::codegen::Arch,
    opt: OptLevel,
) -> String {
    let own: std::collections::HashMap<u32, String> = own_regions(image)
        .into_iter()
        .map(|r| (r.entry, unmarked(&r.name)))
        .collect();
    let listing = crate::listing::asm(image, arch, opt);
    let mut out = String::new();
    // The listing's sections, each a function of a block at a pc: keep
    // those of the definitions' blocks.
    let mut keeping = false;
    for line in listing.lines() {
        if let Some(pc) = line.strip_prefix("fn_pc").and_then(|l| l.strip_suffix(':')) {
            keeping = match pc.parse::<u32>().ok().and_then(|pc| own.get(&pc)) {
                Some(name) => {
                    // The listing puts a blank line before each function.
                    if !out.is_empty() {
                        out.truncate(out.trim_end().len());
                        out.push_str("\n\n");
                    }
                    let _ = writeln!(out, "; {name}");
                    true
                }
                None => false,
            };
        } else if line == "; shared" {
            keeping = false;
        }
        if keeping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    if out.is_empty() {
        out.push_str("; compiled to no function of its own: it was inlined where it is used");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_llvm_module_is_cut_into_its_functions() {
        let module = "; head\n\
            define hidden tailcc i64 @mw.L.repl.f.1(i64 %a0) noinline {\nentry:\n  ret i64 %a0\n}\n\n\
            declare i64 @g()\n\
            define dllexport i64 @\"a b\"() {\n  ret i64 0\n}\n";
        let fs = functions(module);
        let names: Vec<&str> = fs.iter().map(|f| f.name).collect();
        assert_eq!(names, ["@mw.L.repl.f.1", "@\"a b\""]);
        assert!(fs[0].text.ends_with("ret i64 %a0\n}"), "{}", fs[0].text);
    }

    #[test]
    fn a_marked_name_is_shown_as_it_was() {
        assert_eq!(unmarked(&format!("{MARK}sumTo")), "sumTo");
        assert_eq!(unmarked("sumTo"), "sumTo");
    }
}
