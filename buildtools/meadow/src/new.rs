//! `meadow new` — a package, a library or a workspace in a directory of its
//! own, as `cargo new` makes one.
//!
//! Given a path it does as it is told. The directory must not exist yet --
//! `meadow init` is for one that does -- and what goes in it is
//! [`crate::init`]'s, with a git repository around it unless it lands in one
//! already, or in a workspace, whose repository it belongs to.
//!
//! Given nothing, at a terminal, it asks: what to make, what to call it,
//! where, and whether it gets a repository. One question at a time, each
//! answer staying on the screen above the next, and a last look at what will
//! be made before anything is.
//!
//! # Why a state machine and a driver
//!
//! As in [`crate::finder`]: [`Wizard`] takes keys and draws frames and knows
//! nothing of a terminal, so it is tested by driving it with keys and reading
//! what it drew; [`ask`] is the few lines that give it one.

use crate::{git, init};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use std::path::{Path, PathBuf};

/// What to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A package with an entry point: `src/Main.mw`.
    App,
    /// A package for others to use: `src/Lib.mw`.
    Lib,
    /// A workspace, for packages made inside it to join.
    Workspace,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::App, Kind::Lib, Kind::Workspace];

    fn noun(self) -> &'static str {
        match self {
            Kind::App => "an application",
            Kind::Lib => "a library",
            Kind::Workspace => "a workspace",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Kind::App => "src/Main.mw, run with `meadow run`",
            Kind::Lib => "src/Lib.mw, for other packages to use",
            Kind::Workspace => "packages made inside it build together",
        }
    }

    /// The files it is made of, relative to its directory.
    fn files(self) -> &'static [&'static str] {
        match self {
            Kind::App => &["Meadow.toml", "src/Main.mw"],
            Kind::Lib => &["Meadow.toml", "src/Lib.mw"],
            Kind::Workspace => &["Meadow.toml", ".gitignore"],
        }
    }
}

pub struct Options {
    /// The directory to make, which must not exist yet.
    pub path: PathBuf,
    /// Overrides the name taken from the directory.
    pub name: Option<String>,
    pub kind: Kind,
    /// Make it a git repository, unless it is in one already.
    pub git: bool,
}

/// What became of the repository.
#[derive(Debug, PartialEq, Eq)]
pub enum Repository {
    /// A new one, at the new directory.
    Made,
    /// None: it was made inside this one, which keeps it.
    Inside(PathBuf),
    /// None: it joined the workspace at this root, whose repository it
    /// belongs to if it has one.
    Member(PathBuf),
    /// None: none was asked for.
    NotAsked,
    /// None, though one was asked for: what git said.
    Failed(String),
}

/// What was made.
pub struct Made {
    pub created: init::Created,
    pub repository: Repository,
}

pub fn run(opts: &Options) -> Result<Made, String> {
    let path = &opts.path;
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(format!(
            "{} already exists — `meadow init {}` makes a package of a directory that is there",
            path.display(),
            path.display()
        ));
    }
    // Asked before the directory is made, of the nearest one that is there.
    let around = if opts.git {
        nearest_existing(path).and_then(|d| git::repository_of(&d))
    } else {
        None
    };
    let created = init::run(&init::Options {
        path: path.clone(),
        name: opts.name.clone(),
        workspace: opts.kind == Kind::Workspace,
        lib: opts.kind == Kind::Lib,
    })?;
    let repository = if !opts.git {
        Repository::NotAsked
    } else if let Some(top) = around {
        Repository::Inside(top)
    } else if let Some(ws) = &created.within {
        Repository::Member(ws.clone())
    } else {
        match git::init_repository(&created.root) {
            Ok(()) => Repository::Made,
            Err(e) => Repository::Failed(e),
        }
    };
    Ok(Made {
        created,
        repository,
    })
}

/// `path`, or the nearest directory above it that exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let full = std::path::absolute(path).ok()?;
    full.ancestors().find(|d| d.is_dir()).map(Path::to_path_buf)
}

// --- the questions -------------------------------------------------------------

/// What the wizard needs to know of the world around it, and nothing else.
pub struct Context {
    /// Why no repository needs asking for, if none does: the directory is
    /// already in one, or in a workspace.
    pub git_settled: Option<String>,
    /// Whether something is at a path already.
    pub exists: Box<dyn Fn(&Path) -> bool>,
}

impl Context {
    /// The context of the current directory.
    pub fn here() -> Context {
        let here = std::env::current_dir().ok();
        let workspace = here
            .as_deref()
            .and_then(|d| init::enclosing_workspace(d).ok().flatten());
        let repository = here.as_deref().and_then(git::repository_of);
        let git_settled = match (workspace, repository) {
            (Some(ws), _) => Some(format!(
                "the workspace at {}'s",
                crate::workspace::shown(&ws.root)
            )),
            (None, Some(top)) => Some(format!("the one at {}", top.display())),
            (None, None) => None,
        };
        Context {
            git_settled,
            exists: Box::new(|p| std::fs::symlink_metadata(p).is_ok()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Question {
    Kind,
    Name,
    Directory,
    Git,
    Confirm,
    /// Answered: nothing more is asked, and the answers are drawn as they
    /// will stay on the screen.
    Done,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    Create(Choice),
    Cancel,
}

/// What was chosen: [`Options`] without the ownership, for comparing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub path: PathBuf,
    pub name: Option<String>,
    pub kind: Kind,
    pub git: bool,
}

impl From<Choice> for Options {
    fn from(c: Choice) -> Options {
        Options {
            path: c.path,
            name: c.name,
            kind: c.kind,
            git: c.git,
        }
    }
}

/// A line of text being typed, with a cursor in it.
#[derive(Default)]
struct Field {
    text: String,
    /// In characters, not bytes.
    cursor: usize,
}

impl Field {
    fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = text.chars().count();
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Answers whether the text changed.
    fn key(&mut self, key: KeyEvent) -> bool {
        let len = self.text.chars().count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.set("");
                return true;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte_at(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
                return true;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.text.remove(at);
                return true;
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte_at(self.cursor);
                self.text.remove(at);
                return true;
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => {}
        }
        false
    }
}

/// `meadow new`'s questions, apart from any terminal.
pub struct Wizard {
    context: Context,
    question: Question,
    /// Into [`Kind::ALL`].
    kind: usize,
    name: Field,
    directory: Field,
    /// Whether the directory was typed, rather than left to follow the name.
    directory_typed: bool,
    git: bool,
    /// What is wrong with the answer Enter was pressed on.
    problem: Option<String>,
    /// No colour: `NO_COLOR` is set, or a test is reading the text.
    plain: bool,
}

/// The column answers start in, after the question.
const LABEL: usize = 16;

/// The most lines a frame takes, and one more, so that what is printed after
/// it starts on a line of its own.
pub const HEIGHT: u16 = 11;

impl Wizard {
    pub fn new(context: Context) -> Wizard {
        Wizard {
            context,
            question: Question::Kind,
            kind: 0,
            name: Field::default(),
            directory: Field::default(),
            directory_typed: false,
            git: true,
            problem: None,
            plain: std::env::var_os("NO_COLOR").is_some(),
        }
    }

    /// Without colour, whatever the environment says.
    pub fn plain(mut self) -> Self {
        self.plain = true;
        self
    }

    /// Start with this kind chosen, as `--lib` or `--workspace` asks.
    pub fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = Kind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
        self
    }

    /// Start with this name typed, as `--name` asks.
    pub fn with_name(mut self, name: &str) -> Self {
        self.name.set(name);
        self
    }

    /// Start with a repository wanted or not, as `--no-git` asks.
    pub fn with_git(mut self, git: bool) -> Self {
        self.git = git;
        self
    }

    fn chosen(&self) -> Kind {
        Kind::ALL[self.kind]
    }

    /// The question after `q`, given what has been answered.
    fn after(&self, q: Question) -> Question {
        match q {
            Question::Kind if self.chosen() == Kind::Workspace => Question::Directory,
            Question::Kind => Question::Name,
            Question::Name => Question::Directory,
            Question::Directory if self.context.git_settled.is_some() => Question::Confirm,
            Question::Directory => Question::Git,
            Question::Git | Question::Confirm | Question::Done => Question::Confirm,
        }
    }

    /// The question before `q`, or `None` before the first.
    fn before(&self, q: Question) -> Option<Question> {
        Some(match q {
            Question::Kind => return None,
            Question::Name => Question::Kind,
            Question::Directory if self.chosen() == Kind::Workspace => Question::Kind,
            Question::Directory => Question::Name,
            Question::Git => Question::Directory,
            Question::Confirm if self.context.git_settled.is_some() => Question::Directory,
            Question::Confirm => Question::Git,
            Question::Done => Question::Confirm,
        })
    }

    fn go(&mut self, q: Question) {
        self.problem = None;
        if q == Question::Directory && !self.directory_typed {
            let follow = match self.chosen() {
                Kind::Workspace => "",
                _ => &self.name.text,
            };
            let follow = follow.to_string();
            self.directory.set(&follow);
        }
        self.question = q;
    }

    /// What is wrong with the answer to the question being asked, if anything.
    fn check(&self) -> Option<String> {
        match self.question {
            Question::Name => init::check_name(&self.name.text).err(),
            Question::Directory => {
                let d = self.directory.text.trim();
                if d.is_empty() {
                    Some("a directory is needed, which `meadow new` makes".to_string())
                } else if (self.context.exists)(Path::new(d)) {
                    Some(format!(
                        "{d} already exists — choose another, or run `meadow init` in it"
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn choice(&self) -> Choice {
        Choice {
            path: PathBuf::from(self.directory.text.trim()),
            name: match self.chosen() {
                Kind::Workspace => None,
                _ => Some(self.name.text.clone()),
            },
            kind: self.chosen(),
            git: self.git,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('c') && ctrl {
            return Step::Cancel;
        }
        match key.code {
            KeyCode::Esc => {
                return match self.before(self.question) {
                    Some(q) => {
                        self.go(q);
                        Step::Continue
                    }
                    None => Step::Cancel,
                };
            }
            KeyCode::Enter => {
                if let Some(p) = self.check() {
                    self.problem = Some(p);
                    return Step::Continue;
                }
                if self.question == Question::Confirm {
                    self.question = Question::Done;
                    return Step::Create(self.choice());
                }
                let next = self.after(self.question);
                self.go(next);
                return Step::Continue;
            }
            _ => {}
        }
        match self.question {
            Question::Kind => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    self.kind = (self.kind + Kind::ALL.len() - 1) % Kind::ALL.len()
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    self.kind = (self.kind + 1) % Kind::ALL.len()
                }
                _ => {}
            },
            Question::Name => {
                if self.name.key(key) {
                    self.problem = None;
                }
            }
            Question::Directory => {
                if self.directory.key(key) {
                    self.directory_typed = true;
                    self.problem = None;
                }
            }
            Question::Git => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char(' ') => self.git = !self.git,
                KeyCode::Char('y' | 'Y') => {
                    self.git = true;
                    self.go(Question::Confirm);
                }
                KeyCode::Char('n' | 'N') => {
                    self.git = false;
                    self.go(Question::Confirm);
                }
                _ => {}
            },
            Question::Confirm if key.code == KeyCode::Backspace => {
                if let Some(q) = self.before(Question::Confirm) {
                    self.go(q);
                }
            }
            Question::Confirm | Question::Done => {}
        }
        Step::Continue
    }

    // --- drawing -------------------------------------------------------------

    fn style(&self, s: Style) -> Style {
        if self.plain { Style::default() } else { s }
    }

    /// A question answered, or not needing to be.
    fn answered(&self, label: &str, answer: String) -> Line<'static> {
        Line::from(vec![
            Span::styled("✔ ", self.style(Style::new().fg(Color::Green))),
            Span::raw(format!("{label:<width$}", width = LABEL - 2)),
            Span::styled(answer, self.style(Style::new().fg(Color::Cyan))),
        ])
    }

    /// The question being asked, with what is typed so far.
    fn asking(&self, label: &str, answer: Vec<Span<'static>>) -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                "? ",
                self.style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ),
            Span::styled(
                format!("{label:<width$}", width = LABEL - 2),
                self.style(Style::new().add_modifier(Modifier::BOLD)),
            ),
        ];
        spans.extend(answer);
        Line::from(spans)
    }

    fn dim(&self, text: String) -> Span<'static> {
        Span::styled(text, self.style(Style::new().fg(Color::DarkGray)))
    }

    fn git_answer(&self) -> String {
        match &self.context.git_settled {
            Some(why) => format!("{why}, already"),
            None if self.git => "a new one".to_string(),
            None => "none".to_string(),
        }
    }

    /// The lines of the frame, and where the cursor goes in them: a line and a
    /// column.
    fn lines(&self) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let mut lines = vec![Line::from(Span::styled(
            "meadow new",
            self.style(Style::new().add_modifier(Modifier::BOLD)),
        ))];
        let mut cursor = None;
        let q = self.question;
        let past = |p: Question| (p as u8) < (q as u8);
        let kind = self.chosen();

        if past(Question::Kind) {
            lines.push(self.answered("What to make", kind.noun().to_string()));
        } else {
            lines.push(self.asking("What to make", vec![]));
            for (i, k) in Kind::ALL.iter().enumerate() {
                let here = i == self.kind;
                let (mark, style) = if here {
                    (
                        "› ",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::new())
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{mark}{:<16}", k.noun()), self.style(style)),
                    self.dim(k.detail().to_string()),
                ]));
            }
        }

        let field = |w: &Wizard, lines: &mut Vec<Line<'static>>, label: &str, f: &Field| {
            let at = (lines.len() as u16, (LABEL + f.cursor) as u16);
            lines.push(w.asking(label, vec![Span::raw(f.text.clone())]));
            at
        };

        if kind != Kind::Workspace {
            if past(Question::Name) {
                lines.push(self.answered("Name", self.name.text.clone()));
            } else if q == Question::Name {
                cursor = Some(field(self, &mut lines, "Name", &self.name));
            }
        }
        if past(Question::Directory) {
            lines.push(self.answered("Directory", self.directory.text.trim().to_string()));
        } else if q == Question::Directory {
            cursor = Some(field(self, &mut lines, "Directory", &self.directory));
        }
        if past(Question::Directory) {
            if past(Question::Git) || self.context.git_settled.is_some() {
                lines.push(self.answered("Repository", self.git_answer()));
            } else if q == Question::Git {
                let option = |on: bool, text: &str| {
                    if on {
                        Span::styled(
                            if self.plain {
                                format!("[{text}]")
                            } else {
                                format!(" {text} ")
                            },
                            self.style(Style::new().fg(Color::Black).bg(Color::Cyan)),
                        )
                    } else {
                        Span::raw(format!(" {text} "))
                    }
                };
                lines.push(self.asking(
                    "Repository",
                    vec![
                        option(self.git, "a new one"),
                        Span::raw("  "),
                        option(!self.git, "none"),
                    ],
                ));
            }
        }

        if q == Question::Confirm {
            let dir = self.directory.text.trim();
            let files = kind
                .files()
                .iter()
                .map(|f| format!("{dir}/{f}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(self.asking("Make it?", vec![self.dim(files)]));
        }

        if let Some(p) = &self.problem {
            lines.push(Line::from(Span::styled(
                format!("  {p}"),
                self.style(Style::new().fg(Color::Red)),
            )));
        }

        if q != Question::Done {
            let keys = match q {
                Question::Kind => "↑↓ choose · Enter next · Esc cancel",
                Question::Git => "←→ choose · Enter next · Esc back",
                Question::Confirm => "Enter make it · Esc back",
                _ => "Enter next · Esc back · Ctrl-C cancel",
            };
            lines.push(Line::raw(""));
            lines.push(Line::from(self.dim(keys.to_string())));
        }
        (lines, cursor)
    }

    /// How many lines the frame takes.
    pub fn height(&self) -> u16 {
        self.lines().0.len() as u16
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let (lines, cursor) = self.lines();
        // Wrapped for the problem's sake, which can be a sentence: the rest
        // is short enough not to need it.
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        if let Some((y, x)) = cursor
            && y < area.height
        {
            frame.set_cursor_position(Position::new(area.x + x.min(area.width), area.y + y));
        }
    }
}

/// Ask the questions at the terminal, below whatever is on it, and answer
/// what was chosen -- or `None`, if the asking was given up.
///
/// The answers stay on the screen, as a record of what was made.
pub fn ask(mut wizard: Wizard) -> std::io::Result<Option<Options>> {
    use ratatui::crossterm::{cursor, event, execute, terminal};
    terminal::enable_raw_mode()?;
    let mut top = 0;
    let result = (|| {
        let mut t = ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(HEIGHT),
            },
        )?;
        loop {
            // The frame's area is where on the screen the questions are; what
            // `draw` answers is the whole screen's.
            t.draw(|f| {
                top = f.area().y;
                wizard.draw(f)
            })?;
            match event::read()? {
                // A key is reported pressed and released on some platforms;
                // only the press is a keystroke.
                event::Event::Key(k) if k.kind != KeyEventKind::Release => match wizard.key(k) {
                    Step::Continue => {}
                    Step::Create(c) => {
                        t.draw(|f| wizard.draw(f))?;
                        return Ok(Some(c.into()));
                    }
                    Step::Cancel => return Ok(None),
                },
                _ => {}
            }
        }
    })();
    // Below the answers, or over the questions if they were given up, with
    // the rest of the space they took cleared.
    let keep = match &result {
        Ok(Some(_)) => wizard.height(),
        _ => 0,
    };
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        cursor::MoveTo(0, top + keep),
        terminal::Clear(terminal::ClearType::FromCursorDown),
        cursor::Show
    );
    terminal::disable_raw_mode()?;
    result
}
