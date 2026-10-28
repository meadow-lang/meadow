//! What a debugging session is made of, whatever runs it: the program built
//! for debugging ([`build`]), its sources and the names in them ([`sources`]),
//! and the answers a debugger gives -- why it stopped, its frames, its
//! variables. [`super::silo::SiloSession`] runs the program on the VM.
//!
//! Nothing here speaks the protocol. [`super`] turns requests into calls on a
//! session and its answers back into messages, which keeps this testable
//! with ordinary Rust and keeps the protocol code dull.

use crate::pipeline;
use crate::profile::{Profile, Resolved};
use meadow_compiler::core::Loc;
use meadow_compiler::hir;
use meadow_compiler::infer::Renderer;
use meadow_compiler::intern::InternedString;
use meadow_compiler::source::SourceKind;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A source file the program was built from.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Where it is on disk, if it is anywhere: a `Std` module is, once the
    /// editor support has extracted the library.
    pub path: Option<PathBuf>,
    /// The name it was compiled under -- a path, or `Std/Maybe.mw`.
    pub name: String,
    content: InternedString,
    /// Byte offset of each line's start.
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub(super) fn new(path: Option<PathBuf>, name: String, content: InternedString) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile {
            path,
            name,
            content,
            line_starts,
        }
    }

    /// 1-based line and column of a byte offset. Columns count characters,
    /// which is what an editor shows for everything but astral-plane text.
    pub fn position(&self, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= offset).max(1) - 1;
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.content.len());
        let col = self
            .content
            .get(start..end)
            .map_or(0, |s| s.chars().count());
        (line as u32 + 1, col as u32 + 1)
    }

    pub(super) fn line_of(&self, offset: u32) -> u32 {
        self.position(offset).0
    }
}

/// A name a person wrote, as a register or a capture holds it.
#[derive(Debug, Clone)]
pub(super) struct VarInfo {
    pub(super) name: String,
    pub(super) ty: String,
}

/// Why the program is not running.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// Paused before its first statement, because the launch asked for that.
    Entry,
    Breakpoint,
    /// A step finished.
    Step,
    /// Someone asked it to stop.
    Pause,
    /// It failed. It can be inspected, but not resumed.
    Exception(String),
    /// It finished, with the value `main` produced or the error it stopped on.
    Exited(Result<String, String>),
}

/// How to run until the next stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Until a breakpoint.
    Continue,
    /// To the next boundary anywhere.
    StepIn,
    /// To the next boundary in this function or the one it returns to.
    StepOver,
    /// To the next boundary in the function this one returns to.
    StepOut,
    /// To the first boundary inside the top-level definition of this name, in
    /// the source with this id.
    Enter(InternedString, u32),
}

/// One frame of the reconstructed call stack.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The definition it is part of.
    pub name: String,
    /// Where it is -- for a pending frame, where it will resume.
    pub loc: Option<Loc>,
}

/// A row in a variables view.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub ty: Option<String>,
    /// Non-zero when it can be expanded: pass it to [`Session::variables`].
    pub children: u32,
}

/// The scopes a frame offers.
#[derive(Debug, Clone)]
pub struct Scope {
    pub name: &'static str,
    pub reference: u32,
    /// Something to leave collapsed until asked.
    pub expensive: bool,
}

/// Why a launch could not start.
pub type LaunchError = String;

/// What to run instead of `main`: an expression, in the scope of a module.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The module the expression is written in -- usually the one defining the
    /// function it calls.
    pub module: PathBuf,
    /// A Meadow expression: `eval [;] (Int 1)`.
    pub expression: String,
}

/// The name the entry definition gets. It cannot be a name anyone writes by
/// accident, and it has to be one the lexer accepts.
pub(super) const ENTRY: &str = "debug'entry'";

/// `path` built for debugging, starting at `entry` if there is one: the
/// program as AxCut, with where everything was written kept, and the packages
/// it was built from. Lowered -- if `reloadable` -- so that a new build may be
/// loaded into it as it runs, as an interpreted build is (`aot::lowered_for`).
pub(super) fn build_reloadable(
    path: &Path,
    entry: Option<&Entry>,
    reloadable: bool,
) -> Result<(meadow_seq::Program, Vec<meadow_compiler::CompiledPackage>), LaunchError> {
    let path =
        std::fs::canonicalize(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut options = Resolved::new(Profile::Debug).options;
    options.debug_info = true;
    // The entry below is run, like `main`, so it may perform effects.
    options.entry_name = Some(ENTRY);

    // The entry is a definition added to the end of the function's own
    // module, so the expression sees exactly what that module sees --
    // private functions, and whatever it `use`s.
    let added = entry.map(|e| format!("def {ENTRY} =\n  {}", e.expression));
    let original_len = entry
        .and_then(|e| std::fs::read_to_string(&e.module).ok())
        .map_or(usize::MAX, |t| t.len());
    let addition = entry
        .zip(added.as_deref())
        .map(|(e, text)| pipeline::Addition {
            file: &e.module,
            text,
        });
    let out = pipeline::build_with(&path, options, addition);
    if !out.diagnostics.is_empty() {
        let shown: Vec<String> = out
            .diagnostics
            .iter()
            .take(10)
            .map(|d| {
                // Past the end of the file is the text we added.
                if d.label.1.start as usize >= original_len {
                    format!(
                        "in `{}`: {}",
                        entry.map_or("", |e| e.expression.as_str()),
                        d.msg
                    )
                } else {
                    format!("{}: {}", plain_path(&d.filename), d.msg)
                }
            })
            .collect();
        return Err(format!(
            "{} did not compile:\n{}",
            path_display(&path),
            shown.join("\n")
        ));
    }
    let mut linked = out.linked.ok_or("the build produced no program")?;
    if entry.is_some() {
        let var = linked
            .program
            .defs
            .iter()
            .find(|d| &*d.name == ENTRY)
            .map(|d| d.var)
            .ok_or("the entry definition went missing in the build")?;
        linked.program.entry = Some(var);
    }
    if linked.program.entry.is_none() {
        return Err(format!("{} has no `main` to run", path_display(&path)));
    }
    let lowered = meadow_seq::lower_program_with(&linked.program, options.opt, reloadable);
    if !lowered.unsupported.is_empty() {
        return Err(format!(
            "the back end cannot compile this program: {:?}",
            lowered.unsupported
        ));
    }
    Ok((lowered.program, linked.packages))
}

/// Where each of `packages`' sources is, and what each name it binds is
/// called and typed: the debug information a program's own does not hold.
/// `origins` maps a specialized copy's variables to the originals'.
pub(super) fn sources(
    packages: &[meadow_compiler::CompiledPackage],
    origins: impl IntoIterator<Item = (u32, u32)>,
) -> (HashMap<u32, SourceFile>, HashMap<u32, VarInfo>) {
    let std_root = crate::stdlib::extract_sources();
    let mut files = HashMap::new();
    let mut vars = HashMap::new();
    for pkg in packages {
        for m in &pkg.modules {
            let name = match m.source.kind {
                SourceKind::File(n) => n.to_string(),
                SourceKind::Interactive => "<interactive>".to_string(),
            };
            let path = {
                let p = Path::new(&name);
                if p.is_absolute() {
                    Some(PathBuf::from(plain_path(&name)))
                } else {
                    std_root.as_ref().map(|r| r.join(p)).filter(|p| p.is_file())
                }
            };
            files.insert(m.source.id, SourceFile::new(path, name, m.source.content));
            collect_names(&m.hir, &m.source.content, &pkg.types, &mut vars);
        }
    }
    // A specialized copy's variables go by the names of the originals.
    for (copy, original) in origins {
        if let Some(info) = vars.get(&original).cloned() {
            vars.insert(copy, info);
        }
    }
    (files, vars)
}

/// Every name a module binds: its spelling, from the source, and its type.
fn collect_names(
    module: &hir::LModule,
    text: &str,
    types: &meadow_compiler::infer::TypeTable,
    out: &mut HashMap<u32, VarInfo>,
) {
    let mut names = Names { text, types, out };
    for d in &module.value().decls {
        if let hir::Decl::Bind(b) = d.value() {
            names.bind(b);
        }
    }
}

struct Names<'a> {
    text: &'a str,
    types: &'a meadow_compiler::infer::TypeTable,
    out: &'a mut HashMap<u32, VarInfo>,
}

impl Names<'_> {
    fn ident(&mut self, id: &hir::Ident) {
        let span = id.span;
        let Some(name) = self.text.get(span.start as usize..span.end as usize) else {
            return;
        };
        let ty = self
            .types
            .get(id.id)
            .map(|t| Renderer::for_table(self.types).render(t))
            .unwrap_or_default();
        self.out.insert(
            id.value().0,
            VarInfo {
                name: name.to_string(),
                ty,
            },
        );
    }

    fn bind(&mut self, b: &hir::Bind) {
        match b {
            hir::Bind::Fun(name, params, _, body) => {
                self.ident(name);
                params.iter().for_each(|p| self.pat(p));
                self.expr(body);
            }
            hir::Bind::Pat(p, e) => {
                self.pat(p);
                self.expr(e);
            }
            hir::Bind::Error => {}
        }
    }

    fn pat(&mut self, p: &hir::LPat) {
        match p.value() {
            hir::Pat::Var(id) => {
                // A pattern variable's type is recorded on the pattern.
                let span = id.span;
                if let Some(name) = self.text.get(span.start as usize..span.end as usize) {
                    let ty = self
                        .types
                        .get(p.id)
                        .or_else(|| self.types.get(id.id))
                        .map(|t| Renderer::for_table(self.types).render(t))
                        .unwrap_or_default();
                    self.out.insert(
                        id.value().0,
                        VarInfo {
                            name: name.to_string(),
                            ty,
                        },
                    );
                }
            }
            hir::Pat::As(id, sub) => {
                self.ident(id);
                self.pat(sub);
            }
            hir::Pat::Ann(inner, _) => self.pat(inner),
            hir::Pat::Cons(_, ps)
            | hir::Pat::Tuple(ps)
            | hir::Pat::Array(ps)
            | hir::Pat::List(ps) => ps.iter().for_each(|x| self.pat(x)),
            hir::Pat::Record(fs, _) => fs.iter().for_each(|(_, x)| self.pat(x)),
            hir::Pat::View(f, x) => {
                self.expr(f);
                self.pat(x);
            }
            hir::Pat::Wildcard | hir::Pat::Unit | hir::Pat::Lit(_) | hir::Pat::Error => {}
        }
    }

    fn expr(&mut self, e: &hir::LExpr) {
        match e.value() {
            hir::Expr::Lam(ps, body) => {
                ps.iter().for_each(|p| self.pat(p));
                self.expr(body);
            }
            hir::Expr::Let(binds, body) => {
                binds.iter().for_each(|b| self.bind(b));
                self.expr(body);
            }
            hir::Expr::Match(s, arms) => {
                self.expr(s);
                for (p, g, b) in arms {
                    self.pat(p);
                    if let Some(g) = g {
                        self.expr(g);
                    }
                    self.expr(b);
                }
            }
            hir::Expr::Handle(body, arms, ret) => {
                self.expr(body);
                for arm in arms {
                    self.pat(&arm.param);
                    self.ident(&arm.resume);
                    self.expr(&arm.body);
                }
                if let Some((p, b)) = ret {
                    self.pat(p);
                    self.expr(b);
                }
            }
            hir::Expr::App(f, args) => {
                self.expr(f);
                args.iter().for_each(|a| self.expr(a));
            }
            hir::Expr::If(c, t, f) => {
                self.expr(c);
                self.expr(t);
                self.expr(f);
            }
            hir::Expr::Tuple(xs)
            | hir::Expr::Array(xs)
            | hir::Expr::List(xs)
            | hir::Expr::Cons(_, xs) => xs.iter().for_each(|x| self.expr(x)),
            hir::Expr::Record(fs, base) => {
                fs.iter().for_each(|(_, x)| self.expr(x));
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            hir::Expr::Field(o, _) => self.expr(o),
            hir::Expr::Update(base, fs) => {
                self.expr(base);
                fs.iter().for_each(|(_, e)| self.expr(e));
            }
            hir::Expr::Var(_) | hir::Expr::Lit(_) | hir::Expr::Unit | hir::Expr::Error => {}
        }
    }
}

/// A definition's name as a stack frame shows it.
pub(super) fn display_name(name: &str) -> String {
    if name == ENTRY {
        "(debug entry)".to_string()
    } else {
        name.to_string()
    }
}

/// A path as a person writes it: without the `\\?\` prefix `canonicalize`
/// gives on Windows, which an editor would not recognise as the same file.
pub fn plain_path(p: &str) -> String {
    match p.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        },
        None => p.to_string(),
    }
}

pub(super) fn path_display(p: &Path) -> String {
    plain_path(&p.display().to_string())
}

/// `a`, `n1`: a type that is only a variable.
pub(super) fn is_type_variable(ty: &str) -> bool {
    let mut chars = ty.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}
