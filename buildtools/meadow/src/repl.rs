//! The interactive driver.
//!
//! The compiler has no notion of a REPL. We fake one here: every entry is compiled
//! as a throwaway one-module package whose dependencies are all the previous
//! entries (plus, transitively, whatever they depended on). The "repl prefix" is
//! literally handed back to the compiler as ordinary dependency packages.
//!
//! Running an entry is the same trick one level down: its program is every
//! definition in the prefix and its own. But only the definitions the entry
//! reaches are lowered and compiled (see `meadow_core::prune`), so a
//! line costs what it uses -- a handful of definitions, where the whole prefix
//! is the thousand in `Std` and everything typed since. What finding those
//! needs from the prefix -- each definition's references, and the constructor
//! tables -- is kept in a [`Prefix`], built for `Std` once and added to as
//! entries join.

use itertools::Either;
use meadow::runtime;
use meadow::tour;
use meadow::{complete, finder, stdlib};
use meadow_compiler::{
    AstModule, CompiledPackage, Options, ast, compile_unit, core, diagnostics, hir,
    intern::InternedString,
    lexer::tokenize,
    parser,
    source::{Source, SourceKind},
    span::{Located, Span},
};
use meadow_fmt as fmt;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Editor, Event, EventContext,
    EventHandler, Helper, Hinter, KeyCode, KeyEvent, Modifiers, RepeatCount,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::{CmdKind, Highlighter},
    validate::{ValidationResult, Validator},
};
use std::borrow::Cow;
use std::sync::{Arc, Mutex, RwLock};

/// The rustyline helper: multi-line validation plus namespace-aware completion.
///
/// [`Session`] refreshes `names` after every entry, so a `def` or a `use` is
/// completable on the next line.
#[derive(Helper, Hinter, Default)]
struct TermValidator {
    names: complete::Names,
    /// The session's compiled entries, lent to the prompt while a line is
    /// read: what an entry being typed is compiled against to colour it. The
    /// session takes them back when the line is in.
    prefix: Vec<CompiledPackage>,
    /// The session's `use`s, which an entry is compiled under.
    uses: Vec<ast::LDecl>,
    /// Which of the values the prefix exports are functions, for an entry
    /// that does not parse yet: kept up with `prefix`.
    known: std::collections::HashSet<meadow_compiler::hir::VarId>,
    /// The trait methods and effect operations that are functions, by name:
    /// values a program calls that the session's names do not carry an id
    /// for.
    methods: std::collections::HashSet<String>,
    /// The last line coloured and what it came to. The line is asked for
    /// again whenever the cursor moves, and an answer costs a compile.
    seen: std::cell::RefCell<Option<(String, String)>>,
}

impl TermValidator {
    /// Take the session's entries and `use`s for as long as a line is read.
    fn lend(&mut self, prefix: Vec<CompiledPackage>, uses: &[ast::LDecl]) {
        self.known = prefix
            .iter()
            .flat_map(|p| p.exports.iter())
            .filter(|e| meadow_lsp::tokens::is_function_type(&e.scheme.to_string()))
            .map(|e| e.var)
            .collect();
        // A trait's methods and an effect's operations are values a program
        // calls too, and are not among the exports: a method is a function
        // when its declared type is one, and an operation always takes its
        // argument.
        for d in prefix.iter().flat_map(|p| p.data_decls.iter()) {
            match d.value() {
                hir::Decl::Trait(td) => {
                    for m in &td.methods {
                        if matches!(m.ty.value(), hir::TypeExpr::Fun(..)) {
                            self.known.insert(*m.var.value());
                            self.methods.insert(m.name.to_string());
                        }
                    }
                }
                hir::Decl::Effect(ed) => {
                    for (op, id, _) in &ed.ops {
                        self.known.insert(*id.value());
                        self.methods.insert(op.to_string());
                    }
                }
                _ => {}
            }
        }
        self.prefix = prefix;
        self.uses = uses.to_vec();
        *self.seen.borrow_mut() = None;
    }

    /// What `line` means, as far as can be told: compiled against the
    /// session as the entry it will be, which is what the language server
    /// does with a document, when it parses; and by what the session knows
    /// of each spelling when it does not yet.
    fn meaning(&self, line: &str) -> Meaning<'_> {
        let mut meaning = Meaning::spelled(&self.names, &self.known);
        meaning.methods = Some(&self.methods);
        let src = Source::new(SourceKind::Interactive, InternedString::from(line));
        let lex = tokenize(src);
        let (parsed, _) = parser::parse_repl(src, &lex.tokens);
        let Some(item) = parsed else { return meaning };
        let entered = match item {
            Either::Left(decls) => decls,
            Either::Right(expr) => vec![synth_def("it", expr)],
        };
        let mut decls = self.uses.clone();
        decls.extend(entered);
        let name = InternedString::from("repl");
        let module = AstModule {
            path: vec![],
            name,
            ast: Located::new(
                ast::Module {
                    name,
                    decls,
                    attrs: Vec::new(),
                },
                Span::default(),
            ),
            source: src,
        };
        let deps: Vec<meadow_compiler::Dep<'_>> =
            self.prefix.iter().map(meadow_compiler::Dep::new).collect();
        // What is typed is checked here only to colour it: whatever it gets
        // wrong is said when it is entered, and a compiler that gives up on
        // half an entry leaves it coloured by spelling.
        let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compile_unit(
                InternedString::from("repl:prompt"),
                self.prefix.len() + 1,
                vec![module],
                &deps,
                Options {
                    entry_name: Some("it"),
                    ..Options::debug()
                },
            )
        }));
        let Ok((compiled, _)) = compiled else {
            return meaning;
        };
        let analysis = meadow_lsp::analysis::of_unit(&compiled, line, src);
        meaning.resolved = analysis
            .name_refs
            .iter()
            .map(|(span, _, ns)| (span.start, *ns))
            .collect();
        meaning.functions = meadow_lsp::tokens::functions(&analysis);
        meaning.typed = analysis
            .typed
            .iter()
            .map(|(span, _)| (span.start as usize, span.end as usize))
            .collect();
        meaning
    }
}

/// What is known of an entry's names, for colouring them.
struct Meaning<'a> {
    names: &'a complete::Names,
    /// The exported values that are functions, by what they are.
    known: &'a std::collections::HashSet<meadow_compiler::hir::VarId>,
    /// The methods and operations that are functions, by name.
    methods: Option<&'a std::collections::HashSet<String>>,
    /// What a capitalised word resolved to, by where it starts.
    resolved: std::collections::HashMap<u32, meadow_lsp::analysis::Namespace>,
    /// The names that are functions, by where each is written.
    functions: std::collections::HashSet<(usize, usize)>,
    /// Every name inference gave a type, by where it is written: one of
    /// these that is not among `functions` is not a function, whatever else
    /// is spelled that way.
    typed: std::collections::HashSet<(usize, usize)>,
}

impl<'a> Meaning<'a> {
    /// What the session's names alone say: nothing resolved, nothing typed.
    fn spelled(
        names: &'a complete::Names,
        known: &'a std::collections::HashSet<meadow_compiler::hir::VarId>,
    ) -> Meaning<'a> {
        Meaning {
            names,
            known,
            methods: None,
            resolved: Default::default(),
            functions: Default::default(),
            typed: Default::default(),
        }
    }

    /// Whether the name written at `from..to` of `line` is a function.
    fn is_function(&self, line: &str, from: usize, to: usize) -> bool {
        if self.typed.contains(&(from, to)) {
            return self.functions.contains(&(from, to));
        }
        // Not typed: by its spelling, through its qualifier if it has one.
        let Some(name) = line.get(from..to) else {
            return false;
        };
        let before = &line[..from];
        let var = match before.strip_suffix('.') {
            Some(path) => {
                let start = path
                    .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                    .map_or(0, |i| i + 1);
                self.names
                    .qualified_vars
                    .get(&path[start..])
                    .and_then(|vs| vs.get(name))
            }
            None => self.names.value_vars.get(name),
        };
        match var {
            Some(v) => self.known.contains(v),
            // A method, an operation, or one of the language's own: `show`.
            None => {
                self.methods.is_some_and(|ms| ms.contains(name))
                    || meadow_compiler::infer::primitive_scheme(name)
                        .is_some_and(|s| meadow_lsp::tokens::is_function_type(&s.to_string()))
            }
        }
    }
}

/// The prompt, in bold green — the same colour as the logo.
///
/// Two gates agree here: rustyline only asks for a highlighted prompt once it
/// has decided colour is wanted, and `yansi` emits nothing when *it* thinks
/// otherwise. Layout is measured from the raw prompt and rustyline's width
/// calculation skips escape sequences, so the colour costs no display width and
/// the cursor stays put.
impl Highlighter for TermValidator {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        use yansi::Paint as _;
        if default {
            Cow::Owned(prompt.green().bold().to_string())
        } else {
            // Not ours — rustyline's own search prompts, say.
            Cow::Borrowed(prompt)
        }
    }

    /// What is being typed, coloured by what each token is: see [`coloured`].
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if let Some((seen, painted)) = &*self.seen.borrow()
            && seen == line
        {
            return Cow::Owned(painted.clone());
        }
        let painted = coloured(line, &self.meaning(line));
        *self.seen.borrow_mut() = Some((line.to_string(), painted.clone()));
        Cow::Owned(painted)
    }

    /// Every key can change what a token is -- `le` is a name and `let` a
    /// keyword -- so the line is coloured again after each.
    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        true
    }
}

/// What a stretch of an entry is, for colouring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Keyword,
    /// A type's name.
    Type,
    /// A data constructor.
    Constructor,
    /// A module, or an alias for one.
    Module,
    /// A name that is a function.
    Function,
    /// Any other name.
    Variable,
    Number,
    String,
    Comment,
    /// A name, an operator, punctuation, white space: left as it is.
    Plain,
}

/// `line` cut into stretches, each with its class, that together are the
/// whole of it in order.
///
/// The classes are the language server's -- `meadow_lsp::tokens::classified`,
/// what an editor's semantic tokens are made from -- so the prompt and an
/// editor colour a token alike, and by the compiler's own lexer: what is
/// coloured as a keyword is what the language takes for one. A capitalised
/// word is a type, a constructor or a module by what `names`, the session,
/// has in scope under that spelling. A comment never reaches the lexer, so
/// what lies between two tokens is looked at for one: `--` to the end of its
/// line. Text the lexer cannot read -- a string not closed yet -- is plain.
fn classes(line: &str, meaning: &Meaning<'_>) -> Vec<(std::ops::Range<usize>, Class)> {
    let names = meaning.names;
    use meadow_lsp::tokens::{LEGEND, Scope, classified};
    let mut out = Vec::new();
    let between = |from: usize, to: usize, out: &mut Vec<(std::ops::Range<usize>, Class)>| {
        let mut at = from;
        while at < to {
            let rest = &line[at..to];
            match rest.find("--") {
                Some(dashes) => {
                    if dashes > 0 {
                        out.push((at..at + dashes, Class::Plain));
                    }
                    let end = rest[dashes..].find('\n').map_or(to, |n| at + dashes + n);
                    out.push((at + dashes..end, Class::Comment));
                    at = end;
                }
                None => {
                    out.push((at..to, Class::Plain));
                    at = to;
                }
            }
        }
    };
    // What the entry resolved to where it compiled, and what is in scope
    // under a spelling where it did not.
    let scope = Scope {
        resolved: &meaning.resolved,
        is_ctor: &|n| names.ctors.iter().any(|c| c == n),
        is_type: &|n| names.types.iter().any(|t| t == n),
        is_function: &|from, to| meaning.is_function(line, from, to),
    };
    let mut at = 0;
    for (start, end, kind) in classified(line, &scope) {
        // A token made up for the layout, or out of order: nothing to colour.
        if start < at || end > line.len() || start >= end {
            continue;
        }
        between(at, start, &mut out);
        let class = match LEGEND.get(kind as usize).copied() {
            Some("keyword") => Class::Keyword,
            Some("type") => Class::Type,
            Some("enumMember") => Class::Constructor,
            Some("namespace") => Class::Module,
            Some("function") => Class::Function,
            Some("variable") => Class::Variable,
            Some("number") => Class::Number,
            Some("string") => Class::String,
            _ => Class::Plain,
        };
        out.push((start..end, class));
        at = end;
    }
    between(at, line.len(), &mut out);
    out
}

/// `line` with its keywords bold, its types, constructors, modules, numbers
/// and strings a colour each and its comments dim, and names and operators
/// as they are. A line at a time, so that a string over several lines does
/// not colour what starts the next; and nothing at all where `yansi` says
/// there is to be no colour.
fn coloured(line: &str, meaning: &Meaning<'_>) -> String {
    use yansi::Paint as _;
    let mut out = String::with_capacity(line.len() + 32);
    for (range, class) in classes(line, meaning) {
        let text = &line[range];
        if class == Class::Plain {
            out.push_str(text);
            continue;
        }
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if piece.is_empty() {
                continue;
            }
            let painted = match class {
                Class::Keyword => piece.magenta().bold().to_string(),
                Class::Type => piece.cyan().to_string(),
                Class::Constructor => piece.blue().to_string(),
                Class::Module => piece.cyan().dim().to_string(),
                Class::Function => piece.bright_yellow().to_string(),
                Class::Variable => piece.bright_blue().to_string(),
                Class::Number => piece.yellow().to_string(),
                Class::String => piece.green().to_string(),
                Class::Comment => piece.dim().to_string(),
                Class::Plain => piece.to_string(),
            };
            out.push_str(&painted);
        }
    }
    out
}

/// `name : scheme`, as the prompt says what an entry bound: the name bold,
/// and the type coloured as a type is where it is typed. Every capitalised
/// word of a type is a type's name, whatever else is spelled that way.
fn signature(name: &str, scheme: &str) -> String {
    use yansi::Paint as _;
    let all_types = complete::Names::default();
    let none = std::collections::HashSet::new();
    let mut out = String::new();
    for (range, class) in classes(scheme, &Meaning::spelled(&all_types, &none)) {
        let text = &scheme[range];
        match class {
            Class::Type | Class::Constructor | Class::Module => {
                out.push_str(&text.cyan().to_string())
            }
            Class::Keyword => out.push_str(&text.magenta().bold().to_string()),
            _ => out.push_str(text),
        }
    }
    format!("{} : {out}", name.bold())
}

impl Completer for TermValidator {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let start = complete::word_start(line, pos);
        let ctx = complete::context(&line[..pos]);
        let prefix = &line[start..pos];
        let candidates = self
            .names
            .candidates(&ctx, prefix)
            .into_iter()
            .map(|c| Pair {
                display: c.clone(),
                replacement: c,
            })
            .collect();
        Ok((start, candidates))
    }
}

impl Validator for TermValidator {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<ValidationResult> {
        Ok(if needs_continuation(ctx.input()) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

/// The prompt. Only the first line of an entry carries it, which is why
/// continuation lines are padded by [`PAD`].
const PROMPT: &str = "> ";

/// As many spaces as the [`PROMPT`] is wide.
const PAD: &str = "  ";

/// Opens a continuation line already indented to where the formatter says the
/// next line belongs, so a `match` arm or a `let` body does not have to be
/// spaced in by hand.
///
/// Bound to Enter, where it only fires while the entry is unfinished — a
/// complete one still falls through to the validator and is submitted — and to
/// the explicit line-break keys, where it always fires.
struct AutoIndent {
    always: bool,
}

impl ConditionalEventHandler for AutoIndent {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !self.always && !needs_continuation(ctx.line()) {
            return None;
        }
        // Indent for where the cursor is, not for the whole buffer: Enter in the
        // middle of an entry breaks the line at that point.
        let before = &ctx.line()[..ctx.pos()];
        let mut indent = auto_indent(before);
        // A line break asked for by name carries the entry on, whatever the
        // line above looks like: after a whole `fun f x = e` it is for the
        // next clause, `| f y = …`, which stands a unit in. Never the margin,
        // where a line would read as the start of something else.
        if self.always && indent == PAD && !before.trim().is_empty() {
            indent = format!("{PAD}{}", " ".repeat(fmt::UNIT));
        }
        Some(Cmd::Insert(1, format!("\n{indent}")))
    }
}

/// `Ctrl-F`: the declaration finder, over the prompt. What is chosen goes on
/// the prompt at the cursor; a `use` it needs is kept for when the line is
/// submitted, since nothing can be run from inside the line editor.
struct FindKey {
    catalog: Arc<RwLock<finder::Catalog>>,
    needs: Arc<Mutex<Vec<String>>>,
}

impl ConditionalEventHandler for FindKey {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let catalog = self.catalog.read().ok()?;
        match finder::pick(&catalog) {
            Ok(Some(choice)) => {
                if let (Some(line), Ok(mut needs)) = (choice.needs, self.needs.lock()) {
                    needs.push(line);
                }
                Some(Cmd::Insert(1, choice.text))
            }
            // Closed without choosing, or the terminal would not have it:
            // the line goes back as it was.
            _ => Some(Cmd::Repaint),
        }
    }
}

/// The whitespace a continuation line opens with.
///
/// The formatter works in file columns, where every line starts at the left
/// margin. Here the *first* line starts after the prompt and the rest do not, so
/// a continuation needs the prompt's width on top of the formatter's indent —
/// without it the second line of `fun f x =` lands level with the `fun` rather
/// than inside it, and the arms of a bare `match` sit to the left of it.
fn auto_indent(before: &str) -> String {
    format!("{PAD}{}", fmt::continuation_indent(&unpad(before)))
}

/// Take [`PAD`] back off the continuation lines, so the formatter reads the
/// buffer as the file it is pretending to be.
fn unpad(buf: &str) -> String {
    let mut out = String::with_capacity(buf.len());
    for (i, line) in buf.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(line.strip_prefix(PAD).unwrap_or(line));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Decide whether the current buffer should keep accepting lines instead of being
/// submitted.
///
/// - `:` commands and a trailing `\` are handled up front.
/// - On the **first** line, submit as soon as it is a complete parse (so `1 + 2`
///   needs a single Enter); otherwise keep editing.
/// - Once the entry spans multiple lines it is **blank-line terminated** (like
///   GHCi's `:set +m`), except that a buffer which already parses cleanly and has
///   nothing dangling is accepted early. Match/`data` arm lines (`| …`) always wait
///   for the blank line, since `match x with | A -> a` is itself a valid parse.
fn needs_continuation(input: &str) -> bool {
    if input.trim_start().starts_with(':') {
        return false;
    }
    if input.trim_end_matches([' ', '\t']).ends_with('\\') {
        return true;
    }

    let code = strip_strings_and_comments(input);
    let multiline = input.contains('\n');
    // A blank line submits a multi-line entry. "Blank" has to mean whitespace,
    // not just empty: auto-indent puts spaces on the line before you press Enter.
    if multiline
        && input
            .rsplit('\n')
            .next()
            .is_none_or(|l| l.trim().is_empty())
    {
        return false;
    }
    // A signature parses on its own, but it is only half a declaration: the
    // clauses that define the name come under it. Keep reading, or the entry
    // would be submitted as a signature for something nothing defines.
    let last_code = code
        .rsplit('\n')
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    if opens_a_signature(last_code) {
        return true;
    }

    if !multiline {
        return !parses_ok(input);
    }

    let last_line = input
        .rsplit('\n')
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    if last_line.trim_start().starts_with('|') {
        return true; // writing match / data arms — end with a blank line
    }

    if bracket_depth(&code) > 0 {
        return true;
    }
    if matches!(
        code.split_whitespace().last().unwrap_or(""),
        "->" | "="
            | "=="
            | "!="
            | "<="
            | ">="
            | "&&"
            | "||"
            | "let"
            | "in"
            | "if"
            | "then"
            | "else"
            | "match"
            | "with"
            | "fun"
            | "def"
            | "|"
            | "\\"
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "^"
            | "<"
            | ">"
            | "."
            | ","
    ) {
        return true;
    }

    !parses_ok(input)
}

/// Whether `line` is a signature with nothing defined yet: `fun f : T`, or
/// `def x : T`, with no `=` of its own.
///
/// `=>` is not an `=`: `fun f : Show a => a -> String` is still a signature
/// waiting for its clauses, while `fun f : T = e` is a whole declaration.
fn opens_a_signature(line: &str) -> bool {
    matches!(line.split_whitespace().next(), Some("fun") | Some("def"))
        && line.contains(':')
        && !line.replace("=>", "").contains('=')
}

/// Whether `input` lexes and parses (as one decl or one expression) with no errors.
fn parses_ok(input: &str) -> bool {
    let src = Source::new(
        SourceKind::Interactive,
        InternedString::from(input.to_string()),
    );
    let lex = tokenize(src);
    if !lex.errors.is_empty() {
        return false;
    }
    let (parsed, errors) = parser::parse_repl(src, &lex.tokens);
    parsed.is_some() && errors.is_empty()
}

/// What a string literal being skipped is in the middle of.
#[derive(Clone, Copy)]
enum Lit {
    /// The text of a `"…"` literal.
    Text,
    /// A `${…}` hole of one, this many braces deep.
    Hole(usize),
    /// A raw string, `r#"…"#`, closed by a quote and this many `#`s.
    Raw(usize),
}

/// How many `#`s the raw string starting at `i` opens with, if one does.
fn opens_raw(chars: &[char], i: usize) -> Option<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '\'';
    if chars.get(i) != Some(&'r') || (i > 0 && word(chars[i - 1])) {
        return None;
    }
    let hashes = chars[i + 1..].iter().take_while(|&&c| c == '#').count();
    (chars.get(i + 1 + hashes) == Some(&'"')).then_some(hashes)
}

fn strip_strings_and_comments(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A string literal -- with the `${…}` holes in it, whose braces and
        // strings are their own and do not end it -- or a raw string, blanked.
        let opened = match c {
            '"' => Some((Lit::Text, 1)),
            'r' => opens_raw(&chars, i).map(|h| (Lit::Raw(h), 2 + h)),
            _ => None,
        };
        if let Some((first, skip)) = opened {
            out.push(' ');
            i += skip;
            let mut nest = vec![first];
            while let (Some(&top), Some(&d)) = (nest.last(), chars.get(i)) {
                i += 1;
                match (top, d) {
                    (Lit::Raw(h), '"')
                        if chars
                            .get(i..i + h)
                            .is_some_and(|s| s.iter().all(|&c| c == '#')) =>
                    {
                        nest.pop();
                        i += h;
                    }
                    (Lit::Raw(_), _) => {}
                    (Lit::Text, '\\') => i += 1,
                    (Lit::Text, '"') => {
                        nest.pop();
                    }
                    (Lit::Text, '$') if chars.get(i) == Some(&'{') => {
                        i += 1;
                        nest.push(Lit::Hole(0));
                    }
                    (Lit::Hole(_), '"') => nest.push(Lit::Text),
                    (Lit::Hole(_), 'r') if opens_raw(&chars, i - 1).is_some() => {
                        let h = opens_raw(&chars, i - 1).expect("just checked");
                        i += 1 + h;
                        nest.push(Lit::Raw(h));
                    }
                    (Lit::Hole(depth), '{') => {
                        *nest.last_mut().expect("in a hole") = Lit::Hole(depth + 1)
                    }
                    (Lit::Hole(0), '}') => {
                        nest.pop();
                    }
                    (Lit::Hole(depth), '}') => {
                        *nest.last_mut().expect("in a hole") = Lit::Hole(depth - 1)
                    }
                    _ => {}
                }
            }
            out.push(' ');
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while chars.get(i).is_some_and(|&d| d != '\n') {
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Turn `foo \<newline> bar` into `foo <newline> bar` (drop the continuation marker,
/// keep the newline so spans stay sane).
fn join_continuations(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('\\') {
        let after = &rest[pos + 1..];
        let ws_end = after.len() - after.trim_start_matches([' ', '\t']).len();
        if after[ws_end..].starts_with('\n') {
            out.push_str(&rest[..pos]);
            rest = &after[ws_end..]; // keep the '\n'
        } else {
            out.push_str(&rest[..pos + 1]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn bracket_depth(code: &str) -> i32 {
    let mut depth = 0i32;
    for c in code.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// When to colour REPL output, in precedence order: `NO_COLOR` off, then
/// `CLICOLOR_FORCE` on, then "stdout is a terminal and `CLICOLOR` allows it".
///
/// Deliberately not yansi's `TTY_AND_COLOR`, which additionally insists *stderr*
/// is a terminal — `meadow 2> log` would come out monochrome for no good reason.
static COLOUR: yansi::Condition = yansi::Condition::from(|| {
    // Someone who sets `NO_COLOR` wants none, whatever else is set.
    if env_flag("NO_COLOR") {
        return false;
    }
    if env_flag("CLICOLOR_FORCE") {
        return true;
    }
    yansi::Condition::stdout_is_tty() && yansi::Condition::clicolor()
});

/// A `NO_COLOR`-style flag: present, non-empty, and not `"0"`.
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

const LOGO: &str = r#"      __  ___               __
     /  |/  /__  ____ _____/ /___ _      __
    / /|_/ / _ \/ __ `/ __  / __ \ | /| / /
   / /  / /  __/ /_/ / /_/ / /_/ / |/ |/ /
  /_/  /_/\___/\__,_/\__,_/\____/|__/|__/"#;

/// The `:` commands, as `(invocation, what it does)`.
const COMMANDS: &[(&str, &str)] = &[
    (":q", "quit"),
    (":t <expr>", "type-check without evaluating"),
    (
        ":module",
        "list what is defined, and what a later definition shadowed",
    ),
    (
        ":ir <name>",
        "a definition in each of the compiler's IRs: core, Cut and AxCut",
    ),
    (
        ":ir <which> <name>",
        "in one of them: `core`, `cut` or `axcut`",
    ),
    (":reset", "forget everything defined so far"),
    (
        ":jit / :vm / :cek",
        "switch machines (the JIT is the default)",
    ),
    (
        ":silo",
        "evaluate on the merged runtime's VM, as `meadow run --runtime silo --interpret` does",
    ),
    (":time", "time every entry from now on, or stop"),
    (":time <expr>", "time just this entry"),
    (":tour", "a guided tour of the language, one step at a time"),
    (
        ":next / :back",
        "move through it; `:try` puts its example on the prompt",
    ),
];

/// Print the startup banner.
///
/// Unlike the prompt — which rustyline only asks to highlight when it has
/// already decided colour is wanted — this goes straight to stdout before
/// rustyline has set anything up. `yansi` makes that decision instead: it checks
/// for a TTY, honours `NO_COLOR` / `CLICOLOR`, and on Windows turns on virtual
/// terminal processing itself, emitting nothing at all when it cannot.
fn print_banner() {
    // Scoped deliberately: `Paint` blanket-impls onto every type, and its
    // deprecated `clear()` shadows `Vec::clear` at any wider scope.
    use yansi::Paint as _;

    // Short, so that the name is still on the screen when the prompt is:
    // everything else there is to say is `:help`'s.
    println!();
    println!("{}", LOGO.green().bold());
    println!();
    println!(
        "  {}",
        "Welcome to the Meadow interactive environment!".bold()
    );
    println!();
    println!(
        "  {} commands and keys   {} a tour   {} quit",
        ":help".cyan().bold(),
        ":tour".cyan().bold(),
        ":q".cyan().bold()
    );
    println!(
        "  {} completes   {} finds a declaration   {} starts a new line",
        "Tab".cyan().bold(),
        "Ctrl+F".cyan().bold(),
        "Ctrl+J".cyan().bold()
    );
    println!();
}

/// `:help`: every command, and the keys, at the length the banner is not.
fn print_help() {
    use yansi::Paint as _;

    println!();
    println!("  {}  {}", format!("{:<17}", ":help").cyan().bold(), "this");
    for (cmd, what) in COMMANDS {
        // Pad *before* styling: a width applies to the whole formatted value,
        // escape bytes included, so padding afterwards would misalign the column.
        println!("  {}  {}", format!("{cmd:<17}").cyan().bold(), what);
    }
    println!();
    println!(
        "  {} completes names — values, types, constructors and `use` paths,",
        "Tab".cyan().bold()
    );
    println!("  each in the namespace the cursor is actually in.");
    println!(
        "  {} finds a declaration — by name, or by a type like {}.",
        "Ctrl+F".cyan().bold(),
        "[a] -> Int".cyan()
    );
    println!(
        "  {}: {} / {} insert a newline; an unfinished line (open bracket,",
        "Multi-line".bold(),
        "Alt+Enter".cyan(),
        "Ctrl+J".cyan()
    );
    println!("  dangling operator, trailing `\\`, a signature, `| ...` arms) keeps reading.");
    println!("  End a multi-line entry with a blank line.");
    // On a Mac `Alt` is the Option key, and a terminal sends it as text
    // unless it is told to send it as Meta: say which key always works.
    if cfg!(target_os = "macos") {
        println!(
            "  On macOS: {} always works. {} ({}) does once the terminal",
            "Ctrl+J".cyan(),
            "Option+Return".cyan(),
            "Alt+Enter".cyan()
        );
        println!("  sends Option as Meta: Terminal > Settings > Profiles > Keyboard >");
        println!("  \"Use Option as Meta key\"; in iTerm2, Profiles > Keys > Left Option: Esc+;");
        println!("  in VS Code's terminal, the setting `terminal.integrated.macOptionIsMeta`.");
        println!("  Until then Option+Return is an ordinary Return, and enters what is typed.");
    }
    println!();
}

pub struct Session {
    line: u32,
    /// The REPL is an edit-run loop, so it compiles under the debug profile.
    opts: Options,
    /// Compiled prior entries, oldest first — the dependency chain. The first
    /// `std_len` entries are the embedded `Std` package and survive `:reset`.
    prefix: Vec<CompiledPackage>,
    std_len: usize,
    /// Every `use` typed so far. A qualifier is activated per compilation unit,
    /// and each REPL line is its own unit, so they are replayed ahead of each new
    /// line to keep `use Std.Collections.List` in effect for the rest of the
    /// session.
    uses: Vec<ast::LDecl>,
    /// Which machine evaluates a line. `:cek` and `:vm` switch it, which is the
    /// quickest way to decide whether something odd is the language or the VM.
    engine: meadow::Engine,
    /// Whether a line runs on the merged runtime's VM instead -- Silo's
    /// interpreter, in a process of its own -- as `:silo` says, until one of
    /// the others is chosen again.
    silo: bool,
    /// Report how long every entry took -- `:time` turns it on and off.
    timing: bool,
    /// Which step of the tour is showing, if any. The tour is not a mode: the
    /// prompt between steps is the ordinary one, and this is only what `:next`
    /// counts from.
    tour: Option<usize>,
    /// Text to put on the prompt rather than an empty line -- what `:try`
    /// leaves there, ready to be read, edited and run.
    pending: Option<String>,
    /// How many of the current step's lines `:try` has handed over, so that
    /// the next one is the next one.
    tried: usize,
    /// What building an entry's program needs from the prefix, kept up with it.
    cache: Prefix,
    /// The same for `Std` alone, which `:reset` goes back to.
    std_cache: Prefix,
    /// What `Ctrl-F` searches, shared with the key binding that shows it and
    /// brought up to date after every entry. See [`meadow::finder`].
    finder: Arc<RwLock<finder::Catalog>>,
    /// The library's declarations, collected once: `Std` does not change
    /// under a session.
    std_decls: Vec<meadow_find::Decl>,
    /// The `use`s that names chosen with `Ctrl-F` need, to run ahead of the
    /// line they were chosen for.
    needs: Arc<Mutex<Vec<String>>>,
}

/// What every entry's program is built from, kept between entries: the
/// references of each definition in the prefix, and its constructor tables
/// merged. So building the next program walks the edges the entry reaches,
/// rather than the whole prefix.
#[derive(Clone, Default)]
struct Prefix {
    /// How many of the prefix's packages this has taken in.
    packages: usize,
    deps: core::prune::Deps,
    ctor_fields: std::collections::HashMap<InternedString, Vec<InternedString>>,
    variants: meadow_compiler::infer::VariantEnv,
}

impl Prefix {
    /// Take in the packages of `prefix` not yet seen.
    fn catch_up(&mut self, prefix: &[CompiledPackage]) {
        for p in &prefix[self.packages.min(prefix.len())..] {
            self.deps.add(&p.defs);
            self.ctor_fields.extend(p.ctor_fields.clone());
            self.variants.extend(p.variants.clone());
        }
        self.packages = prefix.len();
    }
}

impl Session {
    pub fn new() -> Self {
        let opts = Options::debug();
        let (std_pkgs, diags) = stdlib::std_packages(opts);
        if !diags.is_empty() {
            // A broken embedded prelude is a compiler bug, not a user error.
            for d in &diags {
                eprintln!("internal error compiling Std: {}: {}", d.filename, d.msg);
            }
        }
        let std_len = std_pkgs.len();
        let mut std_cache = Prefix::default();
        std_cache.catch_up(&std_pkgs);
        let std_decls = std_pkgs
            .iter()
            .flat_map(|p| meadow_find::collect::package(p, Default::default()))
            .collect();
        Session {
            line: std_len as u32,
            opts,
            prefix: std_pkgs,
            std_len,
            uses: Vec::new(),
            // What a debug build runs on.
            engine: meadow::Engine::Jit,
            silo: false,
            timing: false,
            tour: None,
            pending: None,
            tried: 0,
            cache: std_cache.clone(),
            std_cache,
            finder: Arc::default(),
            std_decls,
            needs: Arc::default(),
        }
    }

    /// Bring what `Ctrl-F` searches up to what the session has now: the
    /// library, everything defined so far that a line can still name, and
    /// what each name in scope means.
    fn refresh_finder(&self, names: &complete::Names) {
        let mut decls = self.std_decls.clone();
        for pkg in &self.prefix[self.std_len.min(self.prefix.len())..] {
            for mut d in meadow_find::collect::package(pkg, Default::default()) {
                // An expression entered is compiled as `def it = ...`, which
                // is how it is kept, not something anybody wrote.
                if d.name == "it" {
                    continue;
                }
                // Defined again since: the older one has no name left to be
                // reached by, so offering it would only insert the newer.
                if d.var.is_some() && names.value_vars.get(&d.name) != d.var.as_ref() {
                    continue;
                }
                // `repl:12` to the compiler; to a person, what they typed here.
                d.module = "repl".into();
                d.package = "repl".into();
                decls.push(d);
            }
        }
        if let Ok(mut catalog) = self.finder.write() {
            catalog.index = meadow_find::Index::new(decls);
            catalog.scope = names.value_vars.clone();
            catalog.aliases = names.qualified_vars.clone();
        }
    }

    /// What the tour has to offer: its sections, and where each begins.
    ///
    /// Shown by `:tour` before anything has started, because a tour whose
    /// shape you cannot see is one you have to take in order.
    fn tour_contents(&mut self) {
        use yansi::Paint as _;
        println!();
        println!("  {}", "A tour of the language".green().bold());
        println!();
        for section in [tour::Section::Basics, tour::Section::Advanced] {
            let steps: Vec<(usize, &tour::Step)> = tour::STEPS
                .iter()
                .enumerate()
                .filter(|(_, s)| s.section == section)
                .collect();
            let Some((first, _)) = steps.first() else {
                continue;
            };
            println!(
                "  {} {} steps, from {}",
                format!("{:<14}", section.title()).cyan().bold(),
                steps.len(),
                format!(":tour {}", first + 1).cyan()
            );
            for (i, step) in &steps {
                println!("    {:>3}. {}", i + 1, step.title);
            }
            println!();
        }
        println!(
            "  {} starts at the beginning; {} goes straight to the second part.",
            ":next".cyan(),
            ":tour advanced".cyan()
        );
        println!();
    }

    /// Show step `at`, and remember that it is where we are.
    ///
    /// What follows is the ordinary prompt: a step is something to read and
    /// then try, not a question to answer.
    fn show_tour(&mut self, at: usize) {
        use yansi::Paint as _;
        let step = &tour::STEPS[at];
        println!();
        println!(
            "  {}  {}",
            tour::heading(at).green().bold(),
            format!("({})", step.section.title()).dim()
        );
        println!();
        for line in step.text {
            if line.is_empty() {
                println!();
            } else {
                println!("  {line}");
            }
        }
        println!();
        for (i, line) in step.code.iter().enumerate() {
            // Numbered, because `:try` hands them over one at a time and the
            // order is the point: define, then run.
            println!("  {} {line}", format!("{}.", i + 1).cyan().bold());
        }
        println!();
        println!(
            "  {} puts the next of those on the prompt, {} moves on, {} goes back.",
            ":try".cyan(),
            ":next".cyan(),
            ":back".cyan()
        );
        println!();
        self.tour = Some(at);
        self.tried = 0;
    }

    pub fn run(&mut self) {
        let _ = env_logger::try_init();

        // One colour policy for the whole REPL. On Windows this also turns on
        // virtual terminal processing, which rustyline would otherwise only do
        // once the first `readline` starts — too late for the banner.
        yansi::whenever(COLOUR);

        // `List` completion prints the candidates instead of cycling silently,
        // which matters when a prefix matches a dozen prelude names.
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .build();
        let mut rl: Editor<TermValidator, rustyline::history::FileHistory> =
            Editor::with_config(config).expect("failed to create editor");
        let names = complete::snapshot(&self.prefix, &self.uses);
        self.refresh_finder(&names);
        rl.set_helper(Some(TermValidator {
            names,
            ..Default::default()
        }));

        // `Ctrl-F`: find a declaration, by name or by type, and put it on the
        // prompt. See `meadow::finder`.
        rl.bind_sequence(
            KeyEvent::ctrl('F'),
            EventHandler::Conditional(Box::new(FindKey {
                catalog: Arc::clone(&self.finder),
                needs: Arc::clone(&self.needs),
            })),
        );

        // Insert a literal newline instead of submitting. `Alt+Enter` and `Ctrl+J`
        // are portable and distinguishable; `Shift+Enter` only reaches us on
        // terminals that report the modifier (many send it identical to Enter, in
        // which case the validator's heuristics still give multi-line editing).
        for key in [
            KeyEvent(KeyCode::Enter, Modifiers::ALT),
            KeyEvent(KeyCode::Enter, Modifiers::SHIFT),
            KeyEvent::ctrl('J'),
        ] {
            rl.bind_sequence(
                key,
                EventHandler::Conditional(Box::new(AutoIndent { always: true })),
            );
        }
        // Plain Enter continues an unfinished entry — indented — and otherwise
        // falls through to the validator, which submits it.
        rl.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::NONE),
            EventHandler::Conditional(Box::new(AutoIndent { always: false })),
        );

        let _ = rl.load_history(".repl_history");

        print_banner();

        loop {
            // `:try` leaves an example on the prompt instead of running it, so
            // that it can be read and changed before it is.
            // The prompt colours what is typed by compiling it against the
            // session, so it has the session's entries while a line is read.
            if let Some(h) = rl.helper_mut() {
                h.lend(std::mem::take(&mut self.prefix), &self.uses);
            }
            let read = match self.pending.take() {
                Some(text) => rl.readline_with_initial(PROMPT, (&text, "")),
                None => rl.readline(PROMPT),
            };
            if let Some(h) = rl.helper_mut() {
                self.prefix = std::mem::take(&mut h.prefix);
            }
            match read {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(line.as_str());
                    // A name chosen with `Ctrl-F` that was not in scope asked
                    // for a `use`, and it goes first -- said out loud, so that
                    // nothing comes into scope without it being on the screen.
                    let needs = self
                        .needs
                        .lock()
                        .map(|mut n| std::mem::take(&mut *n))
                        .unwrap_or_default();
                    for u in needs {
                        use yansi::Paint as _;
                        println!("{}", u.as_str().dim());
                        self.handle(&u, Mode::Run, false);
                    }
                    // Fold `\`-continued lines back together, then work with the
                    // whole (possibly multi-line) entry.
                    let entry = join_continuations(&line);
                    let trimmed = entry.trim();
                    match trimmed {
                        ":q" | ":quit" => break,
                        ":help" | ":h" | ":?" => print_help(),
                        ":reset" => {
                            self.prefix.truncate(self.std_len);
                            self.cache = self.std_cache.clone();
                            self.line = self.std_len as u32;
                            self.uses.clear();
                            println!("(reset)");
                        }
                        ":module" => self.list_module(),
                        ":vm" | ":cek" | ":jit" => {
                            self.engine = match trimmed {
                                ":cek" => meadow::Engine::Cek,
                                ":jit" => meadow::Engine::Jit,
                                _ => meadow::Engine::Vm,
                            };
                            self.silo = false;
                            println!("(evaluating with the {})", self.engine);
                        }
                        ":silo" => match meadow::aot::interpreter() {
                            Ok(_) => {
                                self.silo = true;
                                println!("(evaluating on the merged runtime's VM)");
                            }
                            Err(e) => println!("(no VM to evaluate on: {e})"),
                        },
                        ":time" => {
                            self.timing = !self.timing;
                            let now = if self.timing { "on" } else { "off" };
                            println!("(timing every entry: {now})");
                        }
                        ":tour" => match self.tour {
                            Some(at) => self.show_tour(at),
                            None => self.tour_contents(),
                        },
                        ":next" | ":n" => match self.tour {
                            // The end of a section is somewhere to stop: what
                            // follows the basics is there for whoever wants it,
                            // and is not the rest of a queue.
                            Some(at)
                                if at + 1 < tour::STEPS.len()
                                    && tour::STEPS[at + 1].section != tour::STEPS[at].section =>
                            {
                                let next = tour::STEPS[at + 1].section;
                                self.tour = None;
                                println!(
                                    "(that is {}. `:tour advanced` carries on with {}, and \
                                     everything you defined is still here)",
                                    tour::STEPS[at].section.title(),
                                    next.title()
                                );
                            }
                            Some(at) if at + 1 < tour::STEPS.len() => self.show_tour(at + 1),
                            Some(_) => {
                                self.tour = None;
                                println!("(that was the last step; the tour is over)");
                            }
                            None => self.show_tour(0),
                        },
                        ":back" | ":b" => match self.tour {
                            Some(at) if at > 0 => self.show_tour(at - 1),
                            _ => self.show_tour(0),
                        },
                        ":try" => match self.tour {
                            Some(at) => {
                                let step = &tour::STEPS[at];
                                match step.code.get(self.tried) {
                                    Some(line) => {
                                        self.tried += 1;
                                        self.pending = Some((*line).to_string());
                                    }
                                    // Round again rather than refusing: a step
                                    // is worth running twice.
                                    None => {
                                        self.tried = 1;
                                        self.pending = step.code.first().map(|l| (*l).to_string());
                                    }
                                }
                            }
                            None => println!("(`:tour` first, and `:try` will offer what to type)"),
                        },
                        ":tour off" | ":tour end" => {
                            self.tour = None;
                            println!("(the tour is over; everything you defined is still here)");
                        }
                        _ if trimmed.starts_with(":tour ") => {
                            let what = trimmed[6..].trim();
                            // A section, a step, or a word that is neither.
                            match (tour::Section::named(what), what.parse::<usize>()) {
                                (Some(section), _) => match section.first() {
                                    Some(at) => self.show_tour(at),
                                    None => println!("(that section has no steps)"),
                                },
                                (_, Ok(n)) if n >= 1 && n <= tour::STEPS.len() => {
                                    self.show_tour(n - 1)
                                }
                                _ => self.tour_contents(),
                            }
                        }
                        _ if trimmed.starts_with(":time ") || trimmed.starts_with(":time\n") => {
                            self.handle(trimmed[5..].trim(), Mode::Run, true);
                        }
                        _ if trimmed.starts_with(":ir ") => self.show_ir(trimmed[3..].trim()),
                        ":ir" => println!(
                            "(:ir <name>, or :ir core|cut|axcut <name>: the name of something defined)"
                        ),
                        _ if trimmed.starts_with(":t ") || trimmed.starts_with(":t\n") => {
                            self.handle(trimmed[2..].trim(), Mode::TypeOnly, false);
                        }
                        _ => self.handle(trimmed, Mode::Run, self.timing),
                    }
                    // A new `def`, `data` or `use` should be completable -- and
                    // findable -- now.
                    let names = complete::snapshot(&self.prefix, &self.uses);
                    self.refresh_finder(&names);
                    if let Some(h) = rl.helper_mut() {
                        h.names = names;
                    }
                }
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
                Err(e) => {
                    eprintln!("error: {e:?}");
                    break;
                }
            }
        }

        let _ = rl.save_history(".repl_history");
    }

    fn list_module(&self) {
        use yansi::Paint as _;
        let listed = self.bindings();
        if listed.is_empty() {
            println!("(no bindings yet)");
            return;
        }
        let shadowed = listed.iter().filter(|(_, _, s)| *s).count();
        for (name, scheme, is_shadowed) in &listed {
            if *is_shadowed {
                println!("{}", format!("{name} : {scheme}  (shadowed)").dim());
            } else {
                println!("{name} : {scheme}");
            }
        }
        if shadowed > 0 {
            println!();
            println!(
                "({} definition{} shadowed: a name defined again is a new one, and \
                 what was written before it goes on using the old)",
                shadowed,
                if shadowed == 1 { " is" } else { "s are" }
            );
        }
    }

    /// Everything defined so far: its name, its type, and whether a later
    /// definition has taken the name.
    ///
    /// Each entry is a compiled unit of its own, so defining a name twice does
    /// not replace the first -- it shadows it. What was compiled against the
    /// first still uses the first, which is why the older one is listed rather
    /// than quietly dropped.
    fn bindings(&self) -> Vec<(String, String, bool)> {
        let user = &self.prefix[self.std_len.min(self.prefix.len())..];
        let mut out: Vec<(String, String, bool)> = Vec::new();
        for pkg in user {
            for e in &pkg.exports {
                let name = hir::spell_name(&e.name).to_string();
                // Anything of this name already listed is now behind this one.
                for (had, _, shadowed) in out.iter_mut() {
                    if *had == name {
                        *shadowed = true;
                    }
                }
                out.push((name, e.scheme.to_string(), false));
            }
        }
        out
    }

    /// The scheme of the binding an entry names, when the entry is nothing but
    /// a name -- `def it = fib`, with the `fib` resolved by the compiler, so a
    /// name defined twice, or brought in by a `use`, or written `S.length`, is
    /// the binding it really is. `None` for any other entry.
    fn named_scheme(&self, compiled: &CompiledPackage) -> Option<String> {
        let module = compiled.modules.first()?;
        let var = module
            .hir
            .value
            .decls
            .iter()
            .rev()
            .find_map(|d| match &*d.value {
                hir::Decl::Bind(hir::Bind::Pat(_, body)) => match &*body.value {
                    hir::Expr::Var(id) => Some(*id.value),
                    _ => None,
                },
                _ => None,
            })?;
        self.prefix.iter().rev().find_map(|p| {
            p.exports
                .iter()
                .find(|e| e.var == var)
                .map(|e| e.scheme.to_string())
                .or_else(|| p.generalized.get(&var).map(|s| s.to_string()))
        })
    }

    /// Check, compile and -- in [`Mode::Run`] -- run one entry, and with `timed`
    /// say how long each of those took.
    fn handle(&mut self, input: &str, mode: Mode, timed: bool) {
        let started = std::time::Instant::now();
        let src = Source::new(
            SourceKind::Interactive,
            InternedString::from(input.to_string()),
        );

        let lex = tokenize(src);
        let (parsed, perrs) = parser::parse_repl(src, &lex.tokens);

        // Surface lex / parse problems with a source snippet, then bail if the
        // entry didn't parse at all.
        let mut front_errors = lex.errors.clone();
        front_errors.extend(
            perrs
                .iter()
                .map(|e| diagnostics::from_parse_error("repl", e)),
        );
        diagnostics::emit(&front_errors, "repl", input);
        let Some(item) = parsed else { return };

        // A bare expression is compiled as `def it = <expr>` so later lines can
        // still refer to `it`, but `it` is an implementation detail and a poor
        // label. Report it under the name the user actually asked about, or `_`
        // when the expression has no name of its own.
        // An entry is one declaration, which is one node unless it is a
        // signature written over the clauses that define it -- then it is the
        // signature and the binding, and both go into the line's module.
        let (entered, label) = match item {
            Either::Left(decls) => (decls, None),
            Either::Right(expr) => {
                let label = expr_label(&expr);
                (vec![synth_def("it", expr)], Some(label))
            }
        };

        // Replay every `use` seen so far, so a qualifier stays active for the rest
        // of the session rather than only for the line that introduced it.
        let mut decls = self.uses.clone();
        decls.extend(entered.iter().cloned());

        let module = Located::new(
            ast::Module {
                name: InternedString::from("repl"),
                decls,
                attrs: Vec::new(),
            },
            Span::default(),
        );
        let ast_mod = AstModule {
            path: vec![],
            name: InternedString::from("repl"),
            ast: module,
            source: src,
        };

        let deps: Vec<meadow_compiler::Dep<'_>> =
            self.prefix.iter().map(meadow_compiler::Dep::new).collect();
        let (compiled, diags) = compile_unit(
            InternedString::from(format!("repl:{}", self.line)),
            self.line as usize,
            vec![ast_mod],
            &deps,
            // A bare expression is run, not defined, so it may do anything.
            Options {
                entry_name: Some("it"),
                cfg: meadow_compiler::Cfg {
                    backend: match self.engine {
                        _ if self.silo => "silo",
                        meadow::Engine::Vm => "vm",
                        meadow::Engine::Cek => "cek",
                        meadow::Engine::Jit => "jit",
                    },
                    ..self.opts.cfg
                },
                ..self.opts
            },
        );

        let had_error = !diags.is_empty();
        diagnostics::emit(&diags, "repl", input);

        // An entry that is only a name is asking what that name is. It runs
        // as `def it = fib`, and running a value settles its constraints --
        // `Add n` becomes `Add Int` -- so `it`'s own type says less than
        // `fib`'s. Show the binding the name means instead.
        let named = label.as_ref().and_then(|_| self.named_scheme(&compiled));
        for e in &compiled.exports {
            // `label` is set only for a synthesized expression, so a real
            // `def it = …` still prints as `it`.
            match &label {
                Some(l) => match &named {
                    Some(scheme) => println!("{}", signature(l, &scheme.to_string())),
                    None => println!("{}", signature(l, &e.scheme.to_string())),
                },
                None => println!(
                    "{}",
                    signature(&hir::spell_name(&e.name).to_string(), &e.scheme.to_string())
                ),
            }
        }
        if compiled.exports.is_empty() && !had_error {
            for d in &compiled.data_decls {
                match d.value() {
                    hir::Decl::Data(dd) => println!("defined type {}", dd.name),
                    hir::Decl::Record(rd) => println!("defined type {}", rd.name),
                    hir::Decl::Alias(ad) => println!("defined type {}", ad.name),
                    _ => {}
                }
            }
        }

        if mode == Mode::Run && !had_error {
            let entry = compiled.exports.last().map(|e| e.var);
            if entry.is_some() {
                let program = self.program_for(&compiled, entry);
                let checked = started.elapsed();
                // Every line is compiled and run as a whole program, so the REPL
                // gets the bytecode VM for free — and `:cek` switches it, the
                // same as the flag on `meadow run`.
                let (result, times) = if self.silo {
                    // A closure that captures nothing is a word Silo cannot
                    // tell from a constructor by itself, so it is shown as
                    // its type says, as Glade shows one.
                    let function = compiled
                        .exports
                        .last()
                        .is_some_and(|e| e.scheme.ty.is_fun());
                    let (result, times) = runtime::run_on_vm(&program, self.opts.opt);
                    let result = result.map(|v| if function { "<closure>".into() } else { v });
                    (result, times)
                } else {
                    runtime::run_timed(&program, self.engine, self.opts.opt)
                };
                match result {
                    Ok(value) => println!("= {value}"),
                    Err(e) => eprintln!("{e}"),
                }
                if timed {
                    println!("{}", timing_line(checked, times));
                }
            }
        }

        if mode == Mode::Run && !had_error {
            // Thread this entry into later lines (including redefinitions of `it`).
            self.prefix.push(compiled);
            self.line += 1;
            // Remember a good `use` so later lines still see its qualifier.
            // Retyping the identical line replaces it; the same module under a new
            // alias is a separate entry, so both qualifiers stay live — which is
            // what the same two lines in a file would do.
            for decl in entered {
                if let Some(u) = complete::peel_use(&decl) {
                    let key = use_key(u);
                    self.uses
                        .retain(|d| complete::peel_use(d).map(use_key) != Some(key.clone()));
                    self.uses.push(decl);
                }
            }
        }
    }

    /// The definition called `name`: the last of that name, as a mention of
    /// it now would mean -- an entry's before the library's.
    fn definition(&self, name: &str) -> Option<&core::Def> {
        self.prefix
            .iter()
            .rev()
            .flat_map(|p| p.defs.iter().rev())
            .find(|d| &*d.name == name || hir::spell_name(&d.name) == name)
    }

    /// `:ir name`, or `:ir which name`: the definition in each of the
    /// compiler's IRs, in the order a program passes through them, or in the
    /// one asked for.
    fn show_ir(&mut self, asked: &str) {
        let (which, name) = match asked.split_once(char::is_whitespace) {
            Some((w @ ("core" | "cut" | "axcut"), name)) => (Some(w), name.trim()),
            _ => (None, asked),
        };
        if self.definition(name).is_none() {
            println!("(nothing called `{name}` is defined)");
            return;
        }
        use yansi::Paint as _;
        for ir in ["core", "cut", "axcut"] {
            if which.is_some_and(|w| w != ir) {
                continue;
            }
            if which.is_none() {
                println!("{}", format!("-- {ir}").dim());
            }
            match ir {
                "core" => self.show_core(name),
                "cut" => self.show_cut(name),
                _ => self.show_axcut(name),
            }
        }
    }

    /// The definition as the front end left it, before the
    /// passes that run on a whole program.
    fn show_core(&self, name: &str) {
        let Some(d) = self.definition(name) else {
            println!("(nothing called `{name}` is defined)");
            return;
        };
        let alone = core::Program {
            defs: vec![d.clone()],
            entry: None,
            ctor_fields: Default::default(),
            variants: Default::default(),
            origins: Default::default(),
            migrations: Vec::new(),
        };
        print!("{}", alone.pretty());
    }

    /// The definition as Cut -- its value, and a definition to
    /// each local function of it that calls itself -- as core is lowered to
    /// it for showing. A program is not compiled through Cut yet: what the
    /// runtimes are handed is `:axcut`'s.
    fn show_cut(&mut self, name: &str) {
        let Some(d) = self.definition(name) else {
            println!("(nothing called `{name}` is defined)");
            return;
        };
        let var = d.var;
        self.cache.catch_up(&self.prefix);
        let keep = core::prune::reach_all(&[&self.cache.deps], Some(var));
        let program = core::Program {
            defs: self
                .prefix
                .iter()
                .flat_map(|p| &p.defs)
                .filter(|d| keep.contains(&d.var))
                .cloned()
                .collect(),
            entry: None,
            ctor_fields: self.cache.ctor_fields.clone(),
            variants: self.cache.variants.clone(),
            origins: Default::default(),
            migrations: Vec::new(),
        };
        let lowered = meadow_seq::cut::to_cut(&program);
        let Some(symbol) = lowered.symbols.get(&var) else {
            return;
        };
        let listing = meadow_cut::print::listing(&lowered.program);
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
                println!("{}\n", &listing.text[s.start..s.end]);
            }
        }
    }

    /// Every block the definition is lowered to as AxCut, in the
    /// program that reaches it -- as the runtimes are handed it, after the
    /// passes over core, at this session's optimisation level.
    fn show_axcut(&mut self, name: &str) {
        let Some(d) = self.definition(name) else {
            println!("(nothing called `{name}` is defined)");
            return;
        };
        let (var, canonical) = (d.var, d.name);
        self.cache.catch_up(&self.prefix);
        let keep = core::prune::reach_all(&[&self.cache.deps], Some(var));
        let program = core::Program {
            defs: self
                .prefix
                .iter()
                .flat_map(|p| &p.defs)
                .filter(|d| keep.contains(&d.var))
                .cloned()
                .collect(),
            entry: None,
            ctor_fields: self.cache.ctor_fields.clone(),
            variants: self.cache.variants.clone(),
            origins: Default::default(),
            migrations: Vec::new(),
        };
        let lowered = meadow_seq::lower_program(&program, self.opts.opt);
        let mut shown = false;
        for block in lowered.program.defs.iter().filter(|b| b.name == canonical) {
            println!("-- {} (L{})\n{}", block.name, block.label.0, block.block);
            shown = true;
        }
        if !shown {
            println!(
                "(`{name}` is lowered to no block of its own: it was inlined where it is used)"
            );
        }
    }

    /// The program that runs `entry`: the definitions of the prefix and of
    /// `current` that it reaches, in order, and every constructor table.
    fn program_for(
        &mut self,
        current: &CompiledPackage,
        entry: Option<core::Var>,
    ) -> core::Program {
        self.cache.catch_up(&self.prefix);
        let own = core::prune::Deps::new(&current.defs);
        let keep = core::prune::reach_all(&[&own, &self.cache.deps], entry);
        let defs = self
            .prefix
            .iter()
            .flat_map(|p| &p.defs)
            .chain(&current.defs)
            .filter(|d| keep.contains(&d.var))
            .cloned()
            .collect();
        let mut ctor_fields = self.cache.ctor_fields.clone();
        ctor_fields.extend(current.ctor_fields.clone());
        let mut variants = self.cache.variants.clone();
        variants.extend(current.variants.clone());
        core::Program {
            defs,
            entry,
            ctor_fields,
            variants,
            origins: Default::default(),
            migrations: Vec::new(),
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// `time: 12.3ms (check 1.20ms, compile 3.45ms, run 7.65ms)` -- the whole
/// entry, then its parts: the front end, lowering and code generation where
/// the machine has them, and the program running.
fn timing_line(check: std::time::Duration, times: runtime::Timings) -> String {
    use yansi::Paint as _;
    let total = check + times.compile.unwrap_or_default() + times.run;
    let mut parts = vec![format!("check {}", runtime::format_duration(check))];
    if let Some(compile) = times.compile {
        parts.push(format!("compile {}", runtime::format_duration(compile)));
    }
    parts.push(format!("run {}", runtime::format_duration(times.run)));
    format!(
        "{} {}",
        format!("time: {}", runtime::format_duration(total)).bold(),
        format!("({})", parts.join(", ")).dim()
    )
}

#[derive(PartialEq)]
enum Mode {
    Run,
    TypeOnly,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name defined twice is two definitions, and `:module` says which one a
    /// new mention would reach.
    ///
    /// The older one is not dead: anything compiled against it still uses it,
    /// which is the thing worth knowing and the reason it is listed at all.
    #[test]
    fn a_redefined_name_is_listed_as_shadowing_the_first() {
        let mut session = Session::new();
        session.handle("def answer = 1", Mode::Run, false);
        session.handle("fun useAnswer u = answer", Mode::Run, false);
        session.handle("def answer = 2", Mode::Run, false);

        let listed = session.bindings();
        let answers: Vec<&(String, String, bool)> =
            listed.iter().filter(|(n, _, _)| n == "answer").collect();
        assert_eq!(answers.len(), 2, "both definitions are there: {listed:?}");
        assert!(answers[0].2, "the first is shadowed");
        assert!(!answers[1].2, "the second is what a new mention reaches");
    }

    /// A name on its own is a question about that name. Running it settles its
    /// constraints, so `it` is `Int -> Int`; what `fib` is, is still general.
    #[test]
    fn a_name_on_its_own_shows_its_binding_not_what_running_it_settled() {
        let mut session = Session::new();
        session.handle(
            "fun fib (n : Int) =\n    let rec loop a b i =\n      if i == 0 then a else loop b (a + b) (i - 1)\n     in loop 0 1 n",
            Mode::Run,
            false,
        );
        session.handle("fib", Mode::Run, false);
        let entry = session.prefix.last().expect("the entry");
        assert_eq!(
            entry.exports[0].scheme.to_string(),
            "Int -> Int",
            "what running it settled"
        );
        assert_eq!(
            session.named_scheme(entry).as_deref(),
            Some("forall n. Add n => Int -> n"),
        );

        // A name from the library is the library's binding.
        session.handle("length", Mode::Run, false);
        let entry = session.prefix.last().expect("the entry");
        assert_eq!(
            session.named_scheme(entry).as_deref(),
            Some("forall a. [a] -> Int")
        );

        // Anything more than a name has no binding to show: its value is it.
        session.handle("fib 10", Mode::Run, false);
        let entry = session.prefix.last().expect("the entry");
        assert_eq!(session.named_scheme(entry), None);
    }

    /// Both colour states in one test: `yansi::whenever` is process-global, so
    /// splitting these would let them race under the default parallel runner.
    #[test]
    fn the_prompt_follows_the_colour_setting() {
        let h = TermValidator::default();

        // Tests do not run on a TTY, so ask for colour explicitly.
        yansi::whenever(yansi::Condition::ALWAYS);
        let got = h.highlight_prompt("> ", true);
        assert!(
            got.starts_with('\x1b'),
            "expected an escape sequence: {got:?}"
        );
        assert!(got.ends_with("\x1b[0m"), "must reset: {got:?}");
        // Colour must not change the *visible* width, or the cursor drifts.
        assert_eq!(strip_ansi(&got), "> ");

        yansi::whenever(yansi::Condition::NEVER);
        assert_eq!(h.highlight_prompt("> ", true), "> ");
        assert_eq!(h.highlight("let x = 1", 0), "let x = 1");

        // What is typed is coloured, and is the same text under the colour.
        yansi::whenever(yansi::Condition::ALWAYS);
        let line = "let s = \"a\nb\" in 7 -- done";
        let got = h.highlight(line, 0);
        assert!(got.contains('\x1b'), "coloured: {got:?}");
        assert_eq!(strip_ansi(&got), line);
        // A string over two lines is coloured a line at a time.
        assert!(
            got.contains("\x1b[0m\n"),
            "reset before the line ends: {got:?}"
        );

        // rustyline's own prompts are never ours to style.
        yansi::whenever(yansi::Condition::ALWAYS);
        assert_eq!(
            h.highlight_prompt("(i-search)`': ", false),
            "(i-search)`': "
        );
    }

    /// What is typed is cut into the stretches the lexer sees, with comments
    /// found between them, and loses nothing: the stretches are the line.
    #[test]
    fn an_entry_is_classed_by_what_the_lexer_sees() {
        let line = "let x = 42 in -- the answer\n  if x == 1.5 then \"a\" else f x";
        let names = complete::Names::default();
        let known = std::collections::HashSet::new();
        let spelled = Meaning::spelled(&names, &known);
        let got = classes(line, &spelled);
        let whole: String = got.iter().map(|(r, _)| &line[r.clone()]).collect();
        assert_eq!(whole, line, "every byte, once, in order");
        let of = |class: Class| -> Vec<&str> {
            got.iter()
                .filter(|(_, c)| *c == class)
                .map(|(r, _)| &line[r.clone()])
                .collect()
        };
        assert_eq!(of(Class::Keyword), ["let", "in", "if", "then", "else"]);
        assert_eq!(of(Class::Number), ["42", "1.5"]);
        assert_eq!(of(Class::String), ["\"a\""]);
        assert_eq!(of(Class::Comment), ["-- the answer"]);
        // A name that starts as a keyword is a name, and a string not closed
        // yet is not coloured as anything.
        assert_eq!(classes("letter", &spelled), vec![(0..6, Class::Variable)]);
        let open: String = classes("f \"abc", &spelled)
            .iter()
            .map(|(r, _)| &"f \"abc"[r.clone()])
            .collect();
        assert_eq!(open, "f \"abc");
    }

    /// A capitalised word is what the session has in scope under that
    /// spelling -- the language server's rule, from its classifier.
    #[test]
    fn a_capital_is_a_type_a_constructor_or_a_module_by_what_is_in_scope() {
        let names = complete::Names {
            types: vec!["Maybe".to_string(), "Int".to_string()],
            ctors: vec!["Just".to_string()],
            ..Default::default()
        };
        let line = "def x : Maybe Int = V.map Just Unknown";
        let known = std::collections::HashSet::new();
        let got = classes(line, &Meaning::spelled(&names, &known));
        let of = |class: Class| -> Vec<&str> {
            got.iter()
                .filter(|(_, c)| *c == class)
                .map(|(r, _)| &line[r.clone()])
                .collect()
        };
        assert_eq!(of(Class::Type), ["Maybe", "Int"]);
        assert_eq!(of(Class::Constructor), ["Just"]);
        // A qualifier, and a capital nothing in scope explains.
        assert_eq!(of(Class::Module), ["V", "Unknown"]);
        assert_eq!(of(Class::Keyword), ["def"]);
    }

    /// The prompt compiles what is typed against the session, as the
    /// language server does a document, so a name is a function or not by
    /// its type -- a parameter that is one included -- and an entry that
    /// does not parse yet still has what the session knows of each spelling.
    #[test]
    fn a_name_is_a_function_by_its_type_and_by_the_session_until_it_has_one() {
        let (prefix, _) = stdlib::std_packages(Options::debug());
        let mut h = TermValidator {
            names: complete::snapshot(&prefix, &[]),
            ..Default::default()
        };
        h.lend(prefix, &[]);
        let of = |line: &str, class: Class| -> Vec<String> {
            classes(line, &h.meaning(line))
                .iter()
                .filter(|(_, c)| *c == class)
                .map(|(r, _)| line[r.clone()].to_string())
                .collect()
        };

        let line = "fun twice f x = f (f x)";
        assert_eq!(of(line, Class::Function), ["twice", "f", "f", "f"]);
        assert_eq!(of(line, Class::Variable), ["x", "x"]);

        // A local that shadows a function is what its own type says.
        let line = "let show = 1 in show + 1";
        assert_eq!(of(line, Class::Function), Vec::<String>::new());
        assert_eq!(of(line, Class::Variable), ["show", "show"]);

        // Not an entry yet: `show` is the session's, a function.
        let line = "let n = show (";
        assert_eq!(of(line, Class::Function), ["show"]);
        assert_eq!(of(line, Class::Variable), ["n"]);

        let started = std::time::Instant::now();
        for _ in 0..20 {
            h.meaning("fun twice f x = f (f x)");
        }
        let each = started.elapsed() / 20;
        assert!(
            // A release build takes two or three milliseconds; a debug one,
            // which is what the tests are, some tens.
            each < std::time::Duration::from_millis(500),
            "colouring an entry took {each:?}, which is too long for a key"
        );
    }

    /// With no colour to be had, what a binding's line says is what it said.
    #[test]
    fn a_signature_is_the_name_and_the_type() {
        yansi::disable();
        assert_eq!(
            signature("xs", "Vector (Maybe a) -> Int"),
            "xs : Vector (Maybe a) -> Int"
        );
    }

    #[test]
    fn env_flag_follows_the_no_color_convention() {
        // Set, non-empty, not "0".
        for (val, want) in [("1", true), ("yes", true), ("", false), ("0", false)] {
            unsafe { std::env::set_var("MEADOW_TEST_FLAG", val) };
            assert_eq!(env_flag("MEADOW_TEST_FLAG"), want, "for {val:?}");
        }
        unsafe { std::env::remove_var("MEADOW_TEST_FLAG") };
        assert!(!env_flag("MEADOW_TEST_FLAG"));
    }

    /// Drop CSI sequences, so a styled string can be measured as displayed.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // `ESC [ … <final byte in @..~>`
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        }
        out
    }

    // --- auto-indent ---------------------------------------------------------

    /// What plain Enter does to a buffer: `None` means "submit", `Some(next)` is
    /// the buffer after the newline and its indent. Mirrors [`AutoIndent`], which
    /// cannot be driven directly without a terminal.
    fn press_enter(buf: &str) -> Option<String> {
        if !needs_continuation(buf) {
            return None;
        }
        Some(format!("{buf}\n{}", auto_indent(buf)))
    }

    /// What the terminal shows for a buffer, prompt included — the thing the
    /// indentation is actually supposed to line up.
    fn screen(buf: &str) -> String {
        format!("{PROMPT}{buf}")
    }

    #[test]
    fn a_continuation_sits_one_unit_inside_the_keyword_above() {
        // On screen `fun` starts at column 2, so its body has to start at 4 — an
        // indent of 2 would only draw it level with the keyword.
        let buf = press_enter("fun f x =").unwrap();
        assert_eq!(buf, "fun f x =\n    ");
        assert_eq!(screen(&buf), "> fun f x =\n    ");
    }

    #[test]
    fn match_arms_line_up_with_the_match_on_screen() {
        // A bare `match`: the arms belong under the `m`, at column 2.
        let buf = press_enter("match x with").unwrap();
        assert_eq!(screen(&buf), "> match x with\n  ");

        // Nested under a `fun`, both move right together.
        let buf = press_enter("fun f x =\n    match x with").unwrap();
        assert_eq!(screen(&buf), "> fun f x =\n    match x with\n    ");
    }

    #[test]
    fn an_arm_body_hangs_under_its_arm() {
        let buf = press_enter("fun f x =\n    match x with\n    | A ->").unwrap();
        assert_eq!(
            screen(&buf),
            "> fun f x =\n    match x with\n    | A ->\n        "
        );
    }

    #[test]
    fn a_match_introduced_by_an_equals_puts_its_arms_under_the_keyword() {
        let buf = press_enter("def x = match y with").unwrap();
        assert_eq!(screen(&buf), "> def x = match y with\n          ");
    }

    #[test]
    fn enter_submits_a_finished_entry() {
        assert_eq!(press_enter("1 + 2"), None);
        assert_eq!(press_enter("def x = 1"), None);
        assert_eq!(press_enter(":t map"), None);
        // The quote and bracket inside the hole do not leave anything open.
        assert_eq!(press_enter(r#""a ${f "(" [1]} b""#), None);
        // Nor do the quote and bracket in a raw string, which escapes nothing.
        assert_eq!(press_enter(r##"r#"C:\ "(" "#"##), None);
    }

    #[test]
    fn an_unclosed_hole_keeps_the_entry_open() {
        assert!(press_enter(r#"def x = "sum: ${1 +"#).is_some());
    }

    #[test]
    fn a_line_of_only_indent_still_submits() {
        // The blank line that ends a multi-line entry is not empty any more —
        // auto-indent already put spaces on it.
        assert_eq!(press_enter("fun f x =\n    x\n    "), None);
        assert_eq!(press_enter("fun f x =\n    x\n"), None);
    }

    #[test]
    fn brackets_keep_the_entry_open_and_indent_it() {
        assert_eq!(press_enter("def r = {").as_deref(), Some("def r = {\n    "));
    }

    #[test]
    fn the_padding_is_not_applied_twice_as_the_entry_grows() {
        // Each line's indent is derived from the buffer, which already carries the
        // padding — so it has to come back off before the formatter sees it.
        let mut buf = press_enter("fun f x =").unwrap();
        buf.push_str("match x with");
        buf = press_enter(&buf).unwrap();
        buf.push_str("| A ->");
        buf = press_enter(&buf).unwrap();
        assert_eq!(
            screen(&buf),
            "> fun f x =\n    match x with\n    | A ->\n        "
        );
    }
}

/// How to label a bare expression in REPL output.
///
/// An expression that names something — a variable, a qualified reference, a
/// nullary constructor — is reported under that name, which is what `:t foo`
/// wants. Anything computed has no name to give, so it prints as `_`.
fn expr_label(expr: &ast::LExpr) -> String {
    match expr.value() {
        ast::Expr::Var(name) => name.value().to_string(),
        ast::Expr::Qual(module, name) => format!("{}.{}", module.value(), name.value()),
        ast::Expr::Cons(name, args) if args.is_empty() => name.value().to_string(),
        _ => "_".to_string(),
    }
}

/// What makes two `use` decls the same line: the path and the alias, by name.
/// (`Located` compares spans too, which differ on every REPL line.)
fn use_key(u: &ast::UseDecl) -> (Vec<InternedString>, Option<InternedString>) {
    (
        u.path.iter().map(|s| *s.value()).collect(),
        u.alias.as_ref().map(|a| *a.value()),
    )
}

fn synth_def(name: &str, expr: ast::LExpr) -> ast::LDecl {
    let span = expr.span;
    let pat = Located::new(
        ast::Pat::Var(Located::new(InternedString::from(name), span)),
        span,
    );
    Located::new(ast::Decl::Bind(ast::Bind::Pat(pat, expr)), span)
}
