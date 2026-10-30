//! **The page viewer**: what the REPL's `:ir` opens.
//!
//! A full-screen view of a few pages of text with a tab for each -- the same
//! entry in each of the forms the compiler puts it in, say -- and `Tab` to
//! turn from one to the next. It takes the whole screen, on the alternate
//! screen, so that the prompt and what was printed before it come back as
//! they were when it is closed.
//!
//! A page is a title, its text, and the [`Notation`] it is written in, which
//! says how to colour it: each IR has its own keywords, its own way of
//! writing a definition's name, its own comments. The colouring goes by the
//! shape of the text alone -- no IR is parsed -- because it is there to make
//! the text easier to scan, and a page that does not parse is still worth
//! reading.
//!
//! A long line is not wrapped: an IR's indentation is its structure, and a
//! wrapped line hides it. The view scrolls sideways instead.
//!
//! # Why a state machine and a driver
//!
//! As with the finder, [`Viewer`] takes keys and draws frames and knows
//! nothing of a terminal; [`view`] is the few lines that give it one, and
//! [`print`] is what is shown when there is no terminal to draw on.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::ops::Range;

/// One page: what its tab says, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub text: String,
    pub notation: Notation,
}

/// What a page is written in, for colouring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation {
    /// The typed IR every pass works on: `(\(v1 : Int). (letrec ...))`.
    Core,
    /// The sequent calculus a front end hands the back end: `docs/CUT.md`.
    Cut,
    /// What the runtimes compile: `substitute`, `jump`, `invoke`.
    AxCut,
    /// Glade's bytecode, as `meadow dis` lists it.
    Bytecode,
    /// Machine code, as `meadow dis --asm` lists it.
    Asm,
    /// LLVM IR, as `meadow cut --emit llvm` writes it.
    Llvm,
    /// Nothing in particular: numbers and strings.
    Text,
}

/// What a piece of a line is, as far as its shape says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A word of the notation, not a name.
    Keyword,
    /// A type, a constructor, a primitive: a capitalised name, or one of the
    /// notation's own type words (`i64`, `ptr`).
    Type,
    /// A top-level definition: Cut's `meadow:Std/Ops."+"`, LLVM's `@f`.
    Global,
    /// A register or a local the notation marks as one: `r3`, `%t1`.
    Local,
    /// Somewhere to jump to: `#20`, `@14`, `entry:`.
    Label,
    Number,
    /// A string or a character.
    Quoted,
    Comment,
    /// An address, the bytes of an instruction: there to be skipped over.
    Address,
    Punctuation,
    Plain,
}

/// The words each notation reserves, and its comments.
struct Rules {
    keywords: &'static [&'static str],
    /// Lower-case words that name types.
    types: &'static [&'static str],
    comment: Option<&'static str>,
}

const CORE: Rules = Rules {
    keywords: &[
        "let", "letrec", "in", "if", "case", "of", "join", "jump", "tup", "perform", "handle",
        "with", "return", "forall", "entry", "true", "false",
    ],
    types: &[],
    comment: None,
};

const CUT: Rules = Rules {
    keywords: &[
        "cut", "entry", "answer", "export", "data", "roles", "native", "effect", "val", "def",
        "prim", "let", "perform", "error", "handle", "with", "cocase", "case", "record", "halt",
        "desc", "true", "false", "none", "μ", "μ̃",
    ],
    types: &[
        "i64", "i32", "i16", "i8", "u64", "u32", "u16", "u8", "f64", "f32", "ptr", "bool", "unit",
        "str", "any", "char", "int",
    ],
    comment: Some("--"),
};

const AXCUT: Rules = Rules {
    keywords: &[
        "def",
        "substitute",
        "in",
        "jump",
        "invoke",
        "new",
        "let",
        "switch",
        "extern",
        "lit",
        "branch",
        "else",
        "error",
        "extend",
        "field",
        "native",
        "record",
        "select",
        "entry",
    ],
    types: &[],
    comment: Some("--"),
};

const BYTECODE: Rules = Rules {
    keywords: &["is", "else", "live"],
    types: &[],
    comment: Some(";"),
};

const ASM: Rules = Rules {
    keywords: &["ptr", "byte", "word", "dword", "qword", "xmmword"],
    types: &[],
    comment: Some(";"),
};

const LLVM: Rules = Rules {
    keywords: &[
        "define",
        "declare",
        "internal",
        "hidden",
        "private",
        "external",
        "global",
        "constant",
        "dllexport",
        "tailcc",
        "fastcc",
        "ccc",
        "ghccc",
        "preserve_nonecc",
        "tail",
        "musttail",
        "call",
        "ret",
        "br",
        "switch",
        "label",
        "unreachable",
        "store",
        "load",
        "alloca",
        "getelementptr",
        "inbounds",
        "inttoptr",
        "ptrtoint",
        "bitcast",
        "zext",
        "sext",
        "trunc",
        "lshr",
        "ashr",
        "shl",
        "and",
        "or",
        "xor",
        "add",
        "sub",
        "mul",
        "sdiv",
        "udiv",
        "srem",
        "urem",
        "fadd",
        "fsub",
        "fmul",
        "fdiv",
        "icmp",
        "fcmp",
        "eq",
        "ne",
        "slt",
        "sle",
        "sgt",
        "sge",
        "ult",
        "ule",
        "ugt",
        "uge",
        "phi",
        "select",
        "to",
        "align",
        "noinline",
        "alwaysinline",
        "nounwind",
        "attributes",
        "type",
        "zeroinitializer",
        "undef",
        "poison",
        "null",
        "true",
        "false",
        "nsw",
        "nuw",
        "exact",
        "volatile",
        "atomic",
        "unnamed_addr",
        "sitofp",
        "fptosi",
        "uitofp",
        "fptoui",
        "fpext",
        "fptrunc",
        "extractvalue",
        "insertvalue",
        "cmpxchg",
        "atomicrmw",
        "fence",
        "seq_cst",
        "monotonic",
        "acquire",
        "release",
    ],
    types: &["void", "ptr", "double", "float", "half", "metadata"],
    comment: Some(";"),
};

impl Notation {
    fn rules(self) -> &'static Rules {
        const TEXT: Rules = Rules {
            keywords: &[],
            types: &[],
            comment: None,
        };
        match self {
            Notation::Core => &CORE,
            Notation::Cut => &CUT,
            Notation::AxCut => &AXCUT,
            Notation::Bytecode => &BYTECODE,
            Notation::Asm => &ASM,
            Notation::Llvm => &LLVM,
            Notation::Text => &TEXT,
        }
    }

    /// The pieces of `line`, each with what it is: every byte of the line is
    /// in exactly one, in order.
    pub fn pieces(self, line: &str) -> Vec<(Class, Range<usize>)> {
        let mut out = Vec::new();
        let mut at = 0;
        // A listing of instructions starts each line with where the
        // instruction is -- and, for machine code, its bytes -- and then
        // what it does: the first word of that is the instruction's name.
        match self {
            Notation::Bytecode => {
                if let Some(end) = digits_then_gap(line) {
                    out.push((Class::Address, 0..end));
                    at = end;
                    at = mnemonic(line, at, &mut out);
                }
            }
            Notation::Asm => {
                if let Some(end) = asm_prefix(line) {
                    out.push((Class::Address, 0..end));
                    at = end;
                    at = mnemonic(line, at, &mut out);
                }
            }
            _ => {}
        }
        self.rest(line, at, &mut out);
        out
    }

    fn rest(self, line: &str, mut at: usize, out: &mut Vec<(Class, Range<usize>)>) {
        let rules = self.rules();
        let bytes = line.as_bytes();
        while at < line.len() {
            let rest = &line[at..];
            let c = rest.chars().next().expect("not at the end");
            let start = at;
            if let Some(mark) = rules.comment
                && rest.starts_with(mark)
            {
                out.push((Class::Comment, at..line.len()));
                return;
            }
            if c.is_whitespace() {
                at += span(rest, char::is_whitespace);
                out.push((Class::Plain, start..at));
                continue;
            }
            if c == '"' || (c == '\'' && self == Notation::Core) {
                at += quoted(rest, c);
                out.push((Class::Quoted, start..at));
                continue;
            }
            // A name with a sigil: LLVM's `@global` and `%local`, the
            // bytecode's `@pc` to jump to, core's `@Int` -- a type applied.
            if (c == '@' || c == '%') && rest.len() > 1 {
                let after = &rest[1..];
                let len = if after.starts_with('"') {
                    quoted(after, '"')
                } else {
                    span(after, |c| is_word(c) || c == '.' || c == '$')
                };
                if len > 0 {
                    at += 1 + len;
                    let class = match (self, c) {
                        (Notation::Llvm, '@') => Class::Global,
                        (Notation::Llvm, _) => Class::Local,
                        (Notation::Core, '@') => Class::Type,
                        (_, '@') => Class::Label,
                        _ => Class::Plain,
                    };
                    out.push((class, start..at));
                    continue;
                }
            }
            // A label by number: AxCut's `#20`, Cut's `#tuple`.
            if c == '#' && rest.len() > 1 {
                let len = span(&rest[1..], is_word);
                if len > 0 {
                    at += 1 + len;
                    let class = match self {
                        Notation::Core => Class::Quoted,
                        Notation::Cut => Class::Keyword,
                        _ => Class::Label,
                    };
                    out.push((class, start..at));
                    continue;
                }
            }
            if c.is_ascii_digit()
                || (c == '-'
                    && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
                    && !self.after_word(line, at))
            {
                at += 1 + span(&rest[1..], |c| {
                    c.is_ascii_alphanumeric() || c == '.' || c == '_'
                });
                out.push((Class::Number, start..at));
                continue;
            }
            if is_word(c) {
                if self == Notation::Cut
                    && let Some(len) = cut_symbol(rest)
                {
                    at += len;
                    out.push((Class::Global, start..at));
                    continue;
                }
                at += span(rest, is_word);
                let word = &line[start..at];
                // A label of LLVM's or of machine code's: a word at the start
                // of a line, and a colon.
                if matches!(self, Notation::Llvm | Notation::Asm)
                    && line[..start].trim().is_empty()
                    && line[at..].starts_with(':')
                {
                    at += 1;
                    out.push((
                        if self == Notation::Asm {
                            Class::Global
                        } else {
                            Class::Label
                        },
                        start..at,
                    ));
                    continue;
                }
                out.push((self.word(word), start..at));
                continue;
            }
            at += c.len_utf8();
            out.push((Class::Punctuation, start..at));
        }
    }

    /// Whether the byte before `at` continues a word, so that a `-` there is
    /// an operator and not a sign.
    fn after_word(self, line: &str, at: usize) -> bool {
        line[..at].chars().next_back().is_some_and(is_word)
    }

    fn word(self, word: &str) -> Class {
        let rules = self.rules();
        if rules.keywords.contains(&word) {
            return Class::Keyword;
        }
        if rules.types.contains(&word) {
            return Class::Type;
        }
        if self == Notation::Llvm
            && word.len() > 1
            && word.starts_with('i')
            && word[1..].bytes().all(|b| b.is_ascii_digit())
        {
            return Class::Type;
        }
        if self == Notation::Bytecode && is_register(word) {
            return Class::Local;
        }
        if word.chars().next().is_some_and(char::is_uppercase) {
            return Class::Type;
        }
        Class::Plain
    }
}

/// A character a name can be made of. A combining mark goes with the letter
/// before it, so that Cut's `μ̃` is one word.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '\'' || ('\u{300}'..='\u{36f}').contains(&c)
}

fn is_register(word: &str) -> bool {
    word.len() > 1 && word.starts_with('r') && word[1..].bytes().all(|b| b.is_ascii_digit())
}

/// How many bytes at the start of `s` are characters `keep` says to.
fn span(s: &str, keep: impl Fn(char) -> bool) -> usize {
    s.char_indices()
        .find(|&(_, c)| !keep(c))
        .map_or(s.len(), |(i, _)| i)
}

/// The length of the quoted text at the start of `s`, which opens with
/// `quote`: through the closing one, or to the end of the line.
fn quoted(s: &str, quote: char) -> usize {
    let mut escaped = false;
    for (i, c) in s.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return i + c.len_utf8();
        }
    }
    s.len()
}

/// A symbol of Cut's at the start of `s`: a package, a colon and a path, as
/// in `meadow:repl-1/sumTo` or `meadow:Std/Ops."+"`, whose parts may be
/// quoted.
fn cut_symbol(s: &str) -> Option<usize> {
    let package = span(s, |c| is_word(c) || c == '-' || c == '.');
    if !s[package..].starts_with(':') {
        return None;
    }
    let mut at = package + 1;
    let mut slash = false;
    while at < s.len() {
        let c = s[at..].chars().next().expect("not at the end");
        if c == '"' {
            at += quoted(&s[at..], '"');
        } else if is_word(c) || matches!(c, '/' | '.' | '-' | '#') {
            slash |= c == '/';
            at += c.len_utf8();
        } else {
            break;
        }
    }
    slash.then_some(at)
}

/// Where the instruction starts on a line of bytecode: after the number of
/// the instruction and the two spaces after it.
fn digits_then_gap(line: &str) -> Option<usize> {
    let lead = span(line, |c| c == ' ');
    let digits = span(&line[lead..], |c| c.is_ascii_digit());
    let end = lead + digits;
    (digits > 0 && line[end..].starts_with("  ")).then_some(end + 2)
}

/// Where the instruction's bytes end on a line of machine code: after its
/// offset in hex, two spaces, and the bytes, two hex digits and a space each.
fn asm_prefix(line: &str) -> Option<usize> {
    let lead = span(line, |c| c == ' ');
    let hex = span(&line[lead..], |c| c.is_ascii_hexdigit());
    if hex < 6 || !line[lead + hex..].starts_with("  ") {
        return None;
    }
    let mut end = lead + hex + 2;
    while let [a, b, b' ', ..] = &line.as_bytes()[end..] {
        if !(a.is_ascii_hexdigit() && b.is_ascii_hexdigit()) {
            break;
        }
        end += 3;
    }
    Some(end)
}

/// The instruction's name at `at`, if one is there, as a keyword.
fn mnemonic(line: &str, at: usize, out: &mut Vec<(Class, Range<usize>)>) -> usize {
    let space = span(&line[at..], char::is_whitespace);
    if space > 0 {
        out.push((Class::Plain, at..at + space));
    }
    let start = at + space;
    let len = span(&line[start..], |c| {
        c.is_ascii_alphanumeric() || c == '.' || c == '_'
    });
    if len > 0 {
        out.push((Class::Keyword, start..start + len));
    }
    start + len
}

fn colour(class: Class) -> Style {
    match class {
        Class::Keyword => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        Class::Type => Style::new().fg(Color::Cyan),
        Class::Global => Style::new().fg(Color::Green),
        Class::Local => Style::new().fg(Color::Blue),
        Class::Label => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Class::Number | Class::Quoted => Style::new().fg(Color::Yellow),
        Class::Comment => Style::new()
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::ITALIC),
        Class::Address | Class::Punctuation => Style::new().add_modifier(Modifier::DIM),
        Class::Plain => Style::new(),
    }
}

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Close,
}

/// How far a key moves a page sideways.
const SIDEWAYS: usize = 4;

/// The rows the viewer takes for itself: the tabs and a rule above the page,
/// a rule and the keys below it.
const CHROME: u16 = 4;

/// The viewer, apart from any terminal.
pub struct Viewer {
    heading: String,
    pages: Vec<Page>,
    /// Each page's lines, with tabs made spaces, and the widest of them.
    lines: Vec<Vec<String>>,
    widths: Vec<usize>,
    /// Where each page is scrolled to -- its top line and its leftmost
    /// column -- so that each keeps its place while another is showing.
    places: Vec<(usize, usize)>,
    current: usize,
    /// The size of the page's part of the screen.
    width: usize,
    height: usize,
    /// No colour: `NO_COLOR` is set, or a test is reading the text.
    plain: bool,
}

impl Viewer {
    /// A viewer of `pages`, the first showing, under `heading`: what the
    /// pages are of. There must be a page.
    pub fn new(heading: &str, pages: Vec<Page>) -> Viewer {
        assert!(!pages.is_empty(), "a viewer has a page to show");
        let lines: Vec<Vec<String>> = pages
            .iter()
            .map(|p| {
                let mut lines: Vec<String> =
                    p.text.lines().map(|l| l.replace('\t', "    ")).collect();
                // An empty page still has a line, to put nothing on.
                if lines.is_empty() {
                    lines.push(String::new());
                }
                lines
            })
            .collect();
        let widths = lines
            .iter()
            .map(|ls| {
                ls.iter()
                    .map(|l| Line::raw(l.as_str()).width())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        Viewer {
            // One line of it: the tabs go beside it.
            heading: match heading.trim().split_once('\n') {
                Some((first, _)) => format!("{} …", first.trim_end()),
                None => heading.trim().to_string(),
            },
            places: vec![(0, 0); pages.len()],
            pages,
            lines,
            widths,
            current: 0,
            width: 80,
            height: 20,
            plain: std::env::var_os("NO_COLOR").is_some(),
        }
    }

    /// Without colour, whatever the environment says.
    pub fn plain(mut self) -> Self {
        self.plain = true;
        self
    }

    /// The page showing.
    pub fn current(&self) -> &Page {
        &self.pages[self.current]
    }

    /// Its top line and its leftmost column, from zero.
    pub fn place(&self) -> (usize, usize) {
        self.places[self.current]
    }

    /// Fit the view to a screen of this size.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width as usize;
        self.height = height.saturating_sub(CHROME) as usize;
        for page in 0..self.pages.len() {
            let (top, left) = self.places[page];
            self.places[page] = (top.min(self.bottom(page)), left.min(self.rightmost(page)));
        }
    }

    /// The furthest down page `page` scrolls: until its last line is at the
    /// bottom of the screen.
    fn bottom(&self, page: usize) -> usize {
        self.lines[page].len().saturating_sub(self.height)
    }

    /// The furthest right: until its widest line ends at the screen's edge.
    fn rightmost(&self, page: usize) -> usize {
        self.widths[page].saturating_sub(self.width)
    }

    fn down(&mut self, by: isize) {
        let page = self.current;
        let (top, left) = self.places[page];
        let top = (top as isize + by).clamp(0, self.bottom(page) as isize) as usize;
        self.places[page] = (top, left);
    }

    fn right(&mut self, by: isize) {
        let page = self.current;
        let (top, left) = self.places[page];
        let left = (left as isize + by).clamp(0, self.rightmost(page) as isize) as usize;
        self.places[page] = (top, left);
    }

    /// Turn `by` pages on, going round at the ends.
    fn turn(&mut self, by: isize) {
        self.current = (self.current as isize + by).rem_euclid(self.pages.len() as isize) as usize;
    }

    pub fn key(&mut self, key: KeyEvent) -> Step {
        if key.kind == KeyEventKind::Release {
            return Step::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Step::Close,
            KeyCode::Char('c') if ctrl => return Step::Close,
            KeyCode::BackTab => self.turn(-1),
            KeyCode::Tab if shift => self.turn(-1),
            KeyCode::Tab => self.turn(1),
            KeyCode::Down | KeyCode::Char('j') => self.down(1),
            KeyCode::Up | KeyCode::Char('k') => self.down(-1),
            KeyCode::Right | KeyCode::Char('l') => self.right(SIDEWAYS as isize),
            KeyCode::Left | KeyCode::Char('h') => self.right(-(SIDEWAYS as isize)),
            KeyCode::PageDown | KeyCode::Char(' ') => self.down(page),
            KeyCode::Char('f') if ctrl => self.down(page),
            KeyCode::PageUp | KeyCode::Char('b') => self.down(-page),
            KeyCode::Home | KeyCode::Char('g') => self.down(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.down(isize::MAX / 2),
            _ => {}
        }
        Step::Continue
    }

    // --- drawing -------------------------------------------------------------

    fn style(&self, s: Style) -> Style {
        if self.plain { Style::default() } else { s }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.resize(area.width, area.height);
        let [tabs, above, body, below, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_tabs(frame, tabs);
        let dim = self.style(Style::new().add_modifier(Modifier::DIM));
        for rule in [above, below] {
            frame.render_widget(
                Paragraph::new("─".repeat(rule.width as usize)).style(dim),
                rule,
            );
        }
        self.draw_page(frame, body);
        self.draw_keys(frame, keys);
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let dim = self.style(Style::new().add_modifier(Modifier::DIM));
        let mut spans = Vec::new();
        // The tabs are what the row is for: the heading has what they leave,
        // and is cut short to fit in it.
        let tabs: usize = self.pages.iter().map(|p| p.title.chars().count() + 2).sum();
        let room = (area.width as usize).saturating_sub(tabs + 2);
        let count = self.heading.chars().count();
        if count > 0 && room >= 8 {
            let heading = if count <= room {
                self.heading.clone()
            } else {
                self.heading.chars().take(room - 1).collect::<String>() + "…"
            };
            spans.push(Span::styled(format!("{heading}  "), dim));
        }
        for (i, p) in self.pages.iter().enumerate() {
            let label = format!(" {} ", p.title);
            if i == self.current {
                // Reversed even without colour: it is what says which page this is.
                spans.push(Span::styled(
                    label,
                    Style::new()
                        .add_modifier(Modifier::REVERSED)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(label));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_page(&self, frame: &mut Frame, area: Rect) {
        let page = &self.pages[self.current];
        let (top, left) = self.places[self.current];
        let shown: Vec<Line> = self.lines[self.current]
            .iter()
            .skip(top)
            .take(area.height as usize)
            .map(|line| {
                Line::from(
                    page.notation
                        .pieces(line)
                        .into_iter()
                        .map(|(class, range)| {
                            Span::styled(line[range].to_string(), self.style(colour(class)))
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(shown).scroll((0, left.min(u16::MAX as usize) as u16)),
            area,
        );
    }

    fn draw_keys(&self, frame: &mut Frame, area: Rect) {
        let dim = self.style(Style::new().add_modifier(Modifier::DIM));
        let (top, _) = self.places[self.current];
        let count = self.lines[self.current].len();
        let last = (top + self.height).min(count);
        let at = format!(" {}-{} of {} ", top + 1, last.max(top + 1), count);
        let [keys, place] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(at.chars().count() as u16),
        ])
        .areas(area);
        // All of the keys where there is room for them, and the ones that
        // matter most where there is not.
        const LONG: &str =
            "Tab next page  Shift-Tab previous  ↑↓ PgUp PgDn Home End scroll  ←→ sideways  q close";
        const SHORT: &str = "Tab page  ↑↓ scroll  ←→ sideways  q close";
        let help = if LONG.chars().count() <= keys.width as usize {
            LONG
        } else {
            SHORT
        };
        frame.render_widget(Paragraph::new(help).style(dim), keys);
        frame.render_widget(Paragraph::new(at).style(dim), place);
    }
}

/// The pages one after another, each under its title: what is shown in
/// place of the viewer when there is no terminal to draw on -- the output is
/// a pipe, or a test is reading it.
pub fn print(pages: &[Page]) -> String {
    let mut out = String::new();
    for p in pages {
        out.push_str(&format!("=== {} ===\n", p.title));
        out.push_str(&p.text);
        if !p.text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Show `pages` under `heading` until the viewer is closed.
///
/// Called between entries, when the line editor has given the terminal back:
/// so this puts it in raw mode itself, and back as it was afterwards, and it
/// draws on the alternate screen, which is how what was on the screen before
/// comes back exactly as it was.
pub fn view(heading: &str, pages: Vec<Page>) -> std::io::Result<()> {
    use ratatui::crossterm::{cursor, event, execute, terminal};
    if pages.is_empty() {
        return Ok(());
    }

    /// Puts the terminal back however the viewer ends, a panic included.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = execute!(
                std::io::stdout(),
                cursor::Show,
                terminal::LeaveAlternateScreen
            );
            let _ = terminal::disable_raw_mode();
        }
    }

    terminal::enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        cursor::Hide
    )?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let mut viewer = Viewer::new(heading, pages);
    loop {
        // Drawing fits the viewer to the screen as it is now, so a resize
        // needs nothing more than to be drawn again.
        terminal.draw(|f| viewer.draw(f))?;
        if let event::Event::Key(k) = event::read()?
            && viewer.key(k) == Step::Close
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(notation: Notation, line: &str) -> Vec<(Class, String)> {
        notation
            .pieces(line)
            .into_iter()
            .map(|(c, r)| (c, line[r].to_string()))
            .filter(|(_, s)| !s.trim().is_empty())
            .collect()
    }

    fn class_of(notation: Notation, line: &str, piece: &str) -> Class {
        classes(notation, line)
            .into_iter()
            .find(|(_, s)| s == piece)
            .unwrap_or_else(|| panic!("no `{piece}` in {:?}", classes(notation, line)))
            .0
    }

    #[test]
    fn the_pieces_of_a_line_are_the_whole_line() {
        for (n, line) in [
            (
                Notation::Cut,
                "  <meadow:Std/Ops.\"+\" | μ̃ f12: ptr. -- a comment",
            ),
            (
                Notation::Llvm,
                "  %t3 = tail call tailcc i64 @mw.L.repl.f.1(i64 %a0) ; x",
            ),
            (
                Notation::Asm,
                "  000010  48 89 c7                 mov rdi, rax",
            ),
            (Notation::Bytecode, "    12  move           r1 <- r2"),
            (Notation::Core, "v0 = (\\(v1 : Int). \"a\\\"b\" 'c' -3)"),
        ] {
            let pieces = n.pieces(line);
            let mut at = 0;
            for (_, r) in &pieces {
                assert_eq!(r.start, at, "{line}");
                at = r.end;
            }
            assert_eq!(at, line.len(), "{line}");
        }
    }

    #[test]
    fn core_marks_its_keywords_types_and_literals() {
        let line =
            "v0 = (letrec v2 : forall a. (Int -> Int) = (/\\a. (if v3 @Int \"s\" 12)) in v2)";
        let n = Notation::Core;
        assert_eq!(class_of(n, line, "letrec"), Class::Keyword);
        assert_eq!(class_of(n, line, "forall"), Class::Keyword);
        assert_eq!(class_of(n, line, "if"), Class::Keyword);
        assert_eq!(class_of(n, line, "Int"), Class::Type);
        assert_eq!(class_of(n, line, "@Int"), Class::Type);
        assert_eq!(class_of(n, line, "\"s\""), Class::Quoted);
        assert_eq!(class_of(n, line, "12"), Class::Number);
        assert_eq!(class_of(n, line, "v2"), Class::Plain);
    }

    #[test]
    fn cut_marks_symbols_binders_and_types() {
        let n = Notation::Cut;
        let line = "      meadow:repl-1/\"sumTo#l20076\"(; μ̃ v20076: ptr.";
        assert_eq!(
            class_of(n, line, "meadow:repl-1/\"sumTo#l20076\""),
            Class::Global
        );
        assert_eq!(class_of(n, line, "μ̃"), Class::Keyword);
        assert_eq!(class_of(n, line, "ptr"), Class::Type);
        let line = "val meadow:repl-2/it : i64 = <meadow:Std/Ops.\"+\" | halt> -- done";
        assert_eq!(class_of(n, line, "val"), Class::Keyword);
        assert_eq!(class_of(n, line, "meadow:Std/Ops.\"+\""), Class::Global);
        assert_eq!(class_of(n, line, "halt"), Class::Keyword);
        assert_eq!(class_of(n, line, "-- done"), Class::Comment);
        // A colon that is a type's is not a symbol's.
        assert_eq!(class_of(n, "a57: i64", "a57"), Class::Plain);
    }

    #[test]
    fn axcut_marks_statements_labels_and_comments() {
        let n = Notation::AxCut;
        assert_eq!(
            class_of(n, "-- sumTo (L10)", "-- sumTo (L10)"),
            Class::Comment
        );
        let line = "  substitute [v1, v2] in (v1, v2) jump #20";
        assert_eq!(class_of(n, line, "substitute"), Class::Keyword);
        assert_eq!(class_of(n, line, "jump"), Class::Keyword);
        assert_eq!(class_of(n, line, "#20"), Class::Label);
        let line = "  extern lit Int(0)() -> (v7)";
        assert_eq!(class_of(n, line, "extern"), Class::Keyword);
        assert_eq!(class_of(n, line, "Int"), Class::Type);
        assert_eq!(class_of(n, line, "0"), Class::Number);
    }

    #[test]
    fn bytecode_marks_addresses_instructions_and_registers() {
        let n = Notation::Bytecode;
        let line = "    12  jumpunless     r3 @40";
        assert_eq!(class_of(n, line, "    12  "), Class::Address);
        assert_eq!(class_of(n, line, "jumpunless"), Class::Keyword);
        assert_eq!(class_of(n, line, "r3"), Class::Local);
        assert_eq!(class_of(n, line, "@40"), Class::Label);
        assert_eq!(class_of(n, "; sumTo", "; sumTo"), Class::Comment);
    }

    #[test]
    fn machine_code_marks_addresses_mnemonics_and_labels() {
        let n = Notation::Asm;
        let line = "  000010  48 89 c7                 mov rdi, 0x10";
        assert_eq!(class_of(n, line, "mov"), Class::Keyword);
        assert_eq!(class_of(n, line, "0x10"), Class::Number);
        assert_eq!(class_of(n, "fn_pc12:", "fn_pc12:"), Class::Global);
    }

    #[test]
    fn llvm_marks_globals_locals_types_and_labels() {
        let n = Notation::Llvm;
        let line = "  %t3 = tail call tailcc i64 @mw.L.repl.f.1(i64 %a0) ; tail";
        assert_eq!(class_of(n, line, "%t3"), Class::Local);
        assert_eq!(class_of(n, line, "call"), Class::Keyword);
        assert_eq!(class_of(n, line, "i64"), Class::Type);
        assert_eq!(class_of(n, line, "@mw.L.repl.f.1"), Class::Global);
        assert_eq!(class_of(n, line, "; tail"), Class::Comment);
        assert_eq!(class_of(n, "entry:", "entry:"), Class::Label);
        assert_eq!(
            class_of(n, "define hidden ptr @\"a b\"()", "@\"a b\""),
            Class::Global
        );
    }
}
