//! The protocol loop: `meadow lsp` speaks LSP over stdin/stdout.
//!
//! Synchronous and single-threaded on purpose. A request is answered by
//! re-analysing the document it names, and analysis is fast enough that a queue
//! and a cancellation protocol would be more machinery than the problem needs.
//! `Std` is compiled once at startup, which is the only slow part.

use crate::analysis::{Analysis, HintPart, Loc, PackageLoader, PathKind, Std};
use crate::pos::LineIndex;
use crate::tokens;
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument, Exit,
    Initialized, Notification, PublishDiagnostics,
};
use lsp_types::request::{
    CodeLensRequest, Completion, Formatting, GotoDefinition, HoverRequest, InlayHintRequest,
    PrepareRenameRequest, Rename, Request as LspRequest, SemanticTokensFullRequest,
    WorkspaceSymbolRequest,
};
use lsp_types::*;
use meadow_compiler::{CompiledPackage, source::Source, span::Span};
use std::collections::HashMap;
use std::error::Error;

/// `meadow/ir`: the definition at a position in one of the compiler's IRs
/// -- `core`, `cut` or `axcut` -- as text, with what each part of the text
/// is and where in it the name at the position is. Not of the protocol: an
/// editor that knows of it shows the IR beside the source, and marks in it
/// what the cursor is on.
pub enum IrRequest {}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IrParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    /// `core`, `cut` or `axcut`.
    pub ir: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IrResult {
    pub ir: String,
    /// The definition shown, as the source spells it.
    pub name: String,
    pub text: String,
    pub segments: Vec<IrSegment>,
    /// Where in `text` the name at the position is.
    pub focus: Vec<Range>,
}

/// A range of an IR's text, and what is written there: a `declaration`, a
/// `variable` or a `symbol`, by its name in that IR.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct IrSegment {
    pub range: Range,
    pub kind: String,
    pub name: String,
}

impl LspRequest for IrRequest {
    type Params = IrParams;
    type Result = Option<IrResult>;
    const METHOD: &'static str = "meadow/ir";
}

pub fn run(
    std_packages: Vec<CompiledPackage>,
    std_modules: Vec<(String, CompiledPackage)>,
    std_src_root: Option<std::path::PathBuf>,
    std_sources: &[(&str, &str)],
    load_package: Option<PackageLoader>,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let (connection, io_threads) = Connection::stdio();
    serve(
        &connection,
        std_packages,
        std_modules,
        std_src_root,
        std_sources,
        load_package,
    )?;
    // The writer thread runs until its channel disconnects, which only happens
    // when the last `Connection` is gone. Joining while this one is still in
    // scope hangs the process — an editor would leave a stray server behind on
    // every close.
    drop(connection);
    io_threads.join()?;
    Ok(())
}

/// Handshake, then answer requests until the client says to stop.
///
/// Separate from [`run`] so a test can drive it over `Connection::memory()`
/// rather than a real pair of pipes.
pub fn serve(
    connection: &Connection,
    std_packages: Vec<CompiledPackage>,
    std_modules: Vec<(String, CompiledPackage)>,
    // Where the embedded `Std` sources were written out, if anywhere — what
    // makes a definition inside the library a file an editor can open.
    std_src_root: Option<std::path::PathBuf>,
    // The library's own source, which completion counts names in.
    std_sources: &[(&str, &str)],
    // How to find the rest of the package a document belongs to. `None` — as in
    // a test that has no files — analyses every document on its own.
    load_package: Option<PackageLoader>,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    handshake(connection)?;
    let mut server = Server::new(
        std_packages,
        std_modules,
        std_src_root,
        std_sources,
        load_package,
    );
    // Two threads. This one analyses, which for a package with macros to
    // expand is seconds of work for each change; the other reads what the
    // editor sends and answers what needs no analysis -- formatting -- at
    // once, so that saving a file does not wait for the analysis of the
    // keystrokes before it. See [`route`].
    let (forward, inbox) = std::sync::mpsc::channel::<Message>();
    std::thread::scope(|threads| {
        threads.spawn(move || route(connection, forward));
        server.main_loop(connection, inbox)
    })
}

/// What the server says of each of `files` -- read from disk, and opened one
/// after another as an editor opens them, through the same analysis -- with
/// what it was given as [`serve`] is. What `meadow lsp --check` reports: a
/// file the editor would mark is one this finds.
pub fn check(
    std_packages: Vec<CompiledPackage>,
    std_modules: Vec<(String, CompiledPackage)>,
    std_src_root: Option<std::path::PathBuf>,
    std_sources: &[(&str, &str)],
    load_package: Option<PackageLoader>,
    files: &[std::path::PathBuf],
    each: &mut dyn FnMut(&std::path::Path, &[Diagnostic]),
) {
    let mut server = Server::new(
        std_packages,
        std_modules,
        std_src_root,
        std_sources,
        load_package,
    );
    // A package's files, analysed together: each file's analysis compiles
    // the whole package, and with every file as it is on disk each would
    // come to the same, so the one compile answers for all of them.
    let mut known: HashMap<std::path::PathBuf, Vec<Diagnostic>> = HashMap::new();
    for file in files {
        let path = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        if let Some(ds) = known.remove(&path) {
            each(file, &ds);
            continue;
        }
        let in_std = path_to_uri(&path).is_some_and(|u| server.std.module_at(u.as_str()).is_some());
        if !in_std
            && let Some(load) = server.load_package
            && let Some(Ok(sources)) = load(&path)
        {
            for (f, ds) in server.std.package_diagnostics(&sources) {
                let text = sources
                    .modules
                    .iter()
                    .find(|m| m.file == f)
                    .map_or("", |m| m.text.as_str());
                let index = LineIndex::new(text);
                let ds = ds
                    .iter()
                    .map(|d| {
                        let (start, end) = index.range(d.label.1);
                        Diagnostic {
                            range: Range {
                                start: Position::new(start.0, start.1),
                                end: Position::new(end.0, end.1),
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("meadow".into()),
                            message: d.msg.clone(),
                            ..Default::default()
                        }
                    })
                    .collect();
                let f = std::fs::canonicalize(&f).unwrap_or(f);
                known.insert(f, ds);
            }
            if let Some(ds) = known.remove(&path) {
                each(file, &ds);
                continue;
            }
        }
        let (Some(uri), Ok(text)) = (path_to_uri(&path), std::fs::read_to_string(&path)) else {
            each(
                file,
                &[Diagnostic {
                    message: "this file cannot be read".into(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    ..Default::default()
                }],
            );
            continue;
        };
        server.set(uri.clone(), text);
        each(file, &server.publish(&uri));
        // Closed again, as an editor closes it: nothing of it is kept for the
        // next.
        server.docs.remove(&uri);
    }
}

/// Read what the editor sends, answer a formatting request from the text of
/// the document alone, and pass everything else on to be analysed.
///
/// The formatter reads only the text, so this keeps each open document's text
/// as the editor last gave it: the messages arrive here in the order the
/// editor sent them, which is what makes it the text the request is about.
fn route(connection: &Connection, forward: std::sync::mpsc::Sender<Message>) {
    let mut texts: HashMap<Uri, String> = HashMap::new();
    for msg in &connection.receiver {
        match &msg {
            Message::Notification(note) => match note.method.as_str() {
                DidOpenTextDocument::METHOD => {
                    if let Ok(p) =
                        serde_json::from_value::<DidOpenTextDocumentParams>(note.params.clone())
                    {
                        texts.insert(p.text_document.uri, p.text_document.text);
                    }
                }
                DidChangeTextDocument::METHOD => {
                    if let Ok(p) =
                        serde_json::from_value::<DidChangeTextDocumentParams>(note.params.clone())
                        && let Some(change) = p.content_changes.into_iter().last()
                    {
                        texts.insert(p.text_document.uri, change.text);
                    }
                }
                DidCloseTextDocument::METHOD => {
                    if let Ok(p) =
                        serde_json::from_value::<DidCloseTextDocumentParams>(note.params.clone())
                    {
                        texts.remove(&p.text_document.uri);
                    }
                }
                _ => {}
            },
            // `meadow fmt`, on the document as the editor holds it. The
            // formatter has no settings, so the client's tab size and
            // spaces-or-tabs are not consulted: a file formats the same way
            // in every editor and on the command line.
            Message::Request(req) if req.method == Formatting::METHOD => {
                let id = req.id.clone();
                let response = match cast::<Formatting>(req.clone()) {
                    Ok((id, p)) => Response::new_ok(
                        id,
                        texts
                            .get(&p.text_document.uri)
                            .map(|text| crate::format::edits(text)),
                    ),
                    Err(e) => Response::new_err(
                        id,
                        lsp_server::ErrorCode::InvalidParams as i32,
                        e.to_string(),
                    ),
                };
                if connection.sender.send(Message::Response(response)).is_err() {
                    return;
                }
                continue;
            }
            _ => {}
        }
        let last = matches!(&msg, Message::Notification(n) if n.method == Exit::METHOD);
        if forward.send(msg).is_err() || last {
            return;
        }
    }
}

/// The document a change is to, when `msg` is one that replaces its text.
fn changed(msg: &Message) -> Option<Uri> {
    let Message::Notification(note) = msg else {
        return None;
    };
    (note.method == DidChangeTextDocument::METHOD)
        .then(|| {
            serde_json::from_value::<DidChangeTextDocumentParams>(note.params.clone())
                .ok()
                .map(|p| p.text_document.uri)
        })
        .flatten()
}

/// Answer `initialize`, then wait for `initialized`.
///
/// `lsp_server::Connection::initialize` does this too, but insists that
/// `initialized` be the *very next* message and treats anything else as fatal.
/// VS Code does not always oblige: `$/setTrace` and
/// `workspace/didChangeConfiguration` can arrive first, and then the server
/// exits during startup. What the user sees is not that — it is
/// `write EPIPE, shutting down server` from a client writing to a process that
/// is already gone, which says nothing about why.
///
/// So: wait for `initialized`, and let anything else past. A request that
/// arrives this early is answered with `ServerNotInitialized` rather than
/// dropped, so the client is not left waiting on it forever.
fn handshake(connection: &Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
    let (id, _params) = connection.initialize_start()?;
    let result = serde_json::json!({
        "capabilities": serde_json::to_value(server_capabilities())?,
    });
    connection
        .sender
        .send(Message::Response(Response::new_ok(id, result)))?;

    loop {
        match connection.receiver.recv()? {
            Message::Notification(n) if n.method == Initialized::METHOD => return Ok(()),
            // The client gave up before it finished starting us.
            Message::Notification(n) if n.method == Exit::METHOD => {
                return Err("client exited during initialization".into());
            }
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Err("client shut us down during initialization".into());
                }
                connection.sender.send(Message::Response(Response::new_err(
                    req.id,
                    lsp_server::ErrorCode::ServerNotInitialized as i32,
                    "the server is still initializing".to_string(),
                )))?;
            }
            _ => {}
        }
    }
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // Full sync: documents are small and analysis re-runs from scratch, so
        // applying incremental edits ourselves would buy nothing.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        // `|>` is a trigger of its own: an editor asks after `>`, which is
        // where the list stops being every name and starts being what this
        // value can be piped into. A `.` is the same thing for a path -- after
        // it the list is what sits under whatever was named.
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![">".to_string(), " ".to_string(), ".".to_string()]),
            ..Default::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        // What an editor's format-on-save asks for.
        document_formatting_provider: Some(OneOf::Left(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        // `prepare_provider` is what lets the editor put the cursor in a box
        // with the old name in it, and refuse before asking on something that
        // cannot be renamed.
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: tokens::LEGEND
                        .iter()
                        .map(|t| SemanticTokenType::new(t))
                        .collect(),
                    token_modifiers: vec![],
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

struct Doc {
    text: String,
    analysis: Analysis,
    index: LineIndex,
}

struct Server {
    std: Std,
    /// Everything the library declares, for a workspace symbol search.
    /// Collected once, as the library is compiled once.
    std_decls: Vec<meadow_find::Decl>,
    docs: HashMap<Uri, Doc>,
    /// Line indexes for sources that are not open documents -- the modules a
    /// definition can land in. Keyed by source id, and never invalidated: see
    /// [`Server::range_in`].
    indexes: HashMap<u32, LineIndex>,
    /// How to find the package a document belongs to, when it belongs to one.
    load_package: Option<PackageLoader>,
}

impl Server {
    fn new(
        std_packages: Vec<CompiledPackage>,
        std_modules: Vec<(String, CompiledPackage)>,
        std_src_root: Option<std::path::PathBuf>,
        std_sources: &[(&str, &str)],
        load_package: Option<PackageLoader>,
    ) -> Server {
        let std_decls = std_packages
            .iter()
            .flat_map(|p| meadow_find::collect::package(p, Default::default()))
            .collect();
        Server {
            std_decls,
            std: Std::new(std_packages, std_modules, std_src_root, std_sources),
            docs: HashMap::new(),
            indexes: HashMap::new(),
            load_package,
        }
    }

    fn main_loop(
        &mut self,
        c: &Connection,
        inbox: std::sync::mpsc::Receiver<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        // What has arrived and is not dealt with yet, oldest first.
        let mut waiting: std::collections::VecDeque<Message> = Default::default();
        let mut stopping = false;
        loop {
            waiting.extend(inbox.try_iter());
            let msg = match waiting.pop_front() {
                Some(msg) => msg,
                None => match inbox.recv() {
                    Ok(msg) => msg,
                    Err(_) => return Ok(()),
                },
            };
            // A change to a document that a later change replaces is not
            // analysed: a full sync carries the whole text each time, and the
            // one typed before it is nobody's question any more. This is what
            // keeps typing from queueing an analysis for every keystroke.
            if let Some(uri) = changed(&msg)
                && waiting
                    .iter()
                    .any(|later| changed(later).as_ref() == Some(&uri))
            {
                continue;
            }
            match msg {
                Message::Request(req) => {
                    if req.method == lsp_types::request::Shutdown::METHOD {
                        stopping = true;
                        c.sender
                            .send(Message::Response(Response::new_ok(req.id, ())))?;
                        continue;
                    }
                    if stopping {
                        c.sender.send(Message::Response(Response::new_err(
                            req.id,
                            lsp_server::ErrorCode::InvalidRequest as i32,
                            "the server is shutting down".to_string(),
                        )))?;
                        continue;
                    }
                    let response = self.request(req);
                    c.sender.send(Message::Response(response))?;
                }
                Message::Notification(note) if note.method == Exit::METHOD => return Ok(()),
                Message::Notification(note) => {
                    for uri in self.notification(note) {
                        let diagnostics = self.publish(&uri);
                        c.sender
                            .send(Message::Notification(lsp_server::Notification {
                                method: PublishDiagnostics::METHOD.to_string(),
                                params: serde_json::to_value(PublishDiagnosticsParams {
                                    uri,
                                    diagnostics,
                                    version: None,
                                })?,
                            }))?;
                    }
                }
                Message::Response(_) => {}
            }
        }
    }

    /// Returns the documents to re-publish diagnostics for.
    fn notification(&mut self, note: lsp_server::Notification) -> Vec<Uri> {
        match note.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(note.params) else {
                    return Vec::new();
                };
                self.set(p.text_document.uri.clone(), p.text_document.text);
                vec![p.text_document.uri]
            }
            DidChangeTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(note.params)
                else {
                    return Vec::new();
                };
                // Full sync, so the last change carries the whole document.
                let Some(change) = p.content_changes.into_iter().last() else {
                    return Vec::new();
                };
                self.set(p.text_document.uri.clone(), change.text);
                vec![p.text_document.uri]
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(note.params) {
                    self.docs.remove(&p.text_document.uri);
                }
                Vec::new()
            }
            // A file changed on disk: a sibling module, a manifest, or the
            // lockfile a build wrote having fetched what the editor could
            // not. Any of them can change what an open document means, so
            // every one is analysed again, as it stands in the editor.
            DidChangeWatchedFiles::METHOD => {
                let open: Vec<(Uri, String)> = self
                    .docs
                    .iter()
                    .map(|(uri, doc)| (uri.clone(), doc.text.clone()))
                    .collect();
                open.into_iter()
                    .map(|(uri, text)| {
                        self.set(uri.clone(), text);
                        uri
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn set(&mut self, uri: Uri, text: String) {
        // A file that *is* a `Std` module is analysed in its own place. Analysed
        // the ordinary way it would declare every one of its own types a second
        // time — once here, once in the `Std` it depends on — and the editor
        // would fill with `already defined`.
        let analysis = match self.std.module_at(uri.as_str()) {
            Some(i) => self.std.analyse_module(i, &text),
            // A file inside a package is analysed as part of it, so that a
            // `use` of a sibling module resolves. Everything else — a scratch
            // file, a document with no package around it — stands alone.
            None => self
                .in_package(&uri, &text)
                .unwrap_or_else(|| self.std.analyse(&text)),
        };
        let index = LineIndex::new(&text);
        self.docs.insert(
            uri,
            Doc {
                text,
                analysis,
                index,
            },
        );
    }

    /// Analyse `text` as the module of its package that it is, if it is one.
    fn in_package(&self, uri: &Uri, text: &str) -> Option<Analysis> {
        let load = self.load_package?;
        let path = uri_to_path(uri)?;
        // The same spelling the loader uses, or the open document will not
        // match the module it is.
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        match load(&path)? {
            Ok(sources) => self.std.analyse_package(&sources, &path, text),
            // In a package, but one that cannot be loaded. Analyse the file on
            // its own so it keeps what colouring and hover it can, and put the
            // reason first: otherwise every `use` of a sibling fails, and those
            // errors are all anyone would see.
            Err(why) => {
                let mut a = self.std.analyse(text);
                a.diagnostics.insert(
                    0,
                    meadow_compiler::diagnostics::Diagnostic {
                        msg: why,
                        filename: path.display().to_string(),
                        label: ("this package cannot be loaded".to_string(), Span::new(0, 0)),
                        extra_labels: vec![],
                    },
                );
                Some(a)
            }
        }
    }

    fn publish(&self, uri: &Uri) -> Vec<Diagnostic> {
        let Some(doc) = self.docs.get(uri) else {
            return Vec::new();
        };
        doc.analysis
            .diagnostics
            .iter()
            .map(|d| {
                let (start, end) = doc.index.range(d.label.1);
                Diagnostic {
                    range: Range {
                        start: Position::new(start.0, start.1),
                        end: Position::new(end.0, end.1),
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("meadow".into()),
                    message: d.msg.clone(),
                    ..Default::default()
                }
            })
            .collect()
    }

    /// What to offer at a position.
    ///
    /// After a `|>` the document does not parse, and one that does not parse
    /// has no types, so the value being piped would have none either. The text
    /// is repaired -- a placeholder where the function will go -- and analysed
    /// again for this one question; that analysis is thrown away, so nothing
    /// the editor sees is built on a document nobody wrote.
    fn completions(&self, p: &TextDocumentPositionParams) -> (Vec<crate::complete::Offer>, Range) {
        let Some(doc) = self.docs.get(&p.text_document.uri) else {
            return (Vec::new(), Range::default());
        };
        let offset = doc.index.offset(p.position.line, p.position.character);
        // What an offer replaces: the word typed so far, which may be empty.
        let start = crate::complete::word_start(&doc.text, offset);
        let (line, character) = doc.index.position(start);
        let range = Range {
            start: Position { line, character },
            end: p.position,
        };
        let offers = match crate::complete::ask(&doc.text, offset) {
            crate::complete::Ask::Name { word } => crate::complete::names(&doc.analysis, &word),
            // A name is being invented, or a record field selected -- neither is
            // something this knows a list for.
            crate::complete::Ask::Nothing { .. } => Vec::new(),
            // `Q.` parses once the name after it is there, so the placeholder
            // that stands in for the half-written word is repair enough.
            crate::complete::Ask::Member { qualifier, word } => {
                let repaired = crate::complete::repaired(&doc.text, start, offset);
                let analysis = self.analysed(&p.text_document.uri, &repaired);
                crate::complete::member(&analysis, &qualifier, &word)
            }
            crate::complete::Ask::UsePath { segments, word } => {
                let text = crate::complete::without_line(&doc.text, start);
                let analysis = self.analysed(&p.text_document.uri, &text);
                crate::complete::use_path(&analysis, &segments, &word)
            }
            crate::complete::Ask::UseNames { segments, word } => {
                let text = crate::complete::without_line(&doc.text, start);
                let analysis = self.analysed(&p.text_document.uri, &text);
                crate::complete::use_names(&analysis, &segments, &word)
            }
            crate::complete::Ask::Piped { pipe_at, word } => {
                // The half-written pipe is finished where the *word* starts, so
                // that what has been typed of the function's name is not part
                // of the repaired document: `xs |> fol` asks the same question
                // as `xs |> `.
                let repaired = crate::complete::repaired(&doc.text, start, offset);
                let analysis = self.analysed(&p.text_document.uri, &repaired);
                match crate::complete::subject(&analysis, pipe_at) {
                    Some(subject) => crate::complete::piped(&analysis, &subject.clone(), &word),
                    // Nothing is known about the value -- an expression with an
                    // error in it, say. Every name is still better than none.
                    None => crate::complete::names(&analysis, &word),
                }
            }
        };
        (offers, range)
    }

    /// `text` analysed as the document at `uri` would be.
    fn analysed(&self, uri: &Uri, text: &str) -> Analysis {
        match self.std.module_at(uri.as_str()) {
            Some(i) => self.std.analyse_module(i, text),
            None => self
                .in_package(uri, text)
                .unwrap_or_else(|| self.std.analyse(text)),
        }
    }

    fn request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        match req.method.as_str() {
            // The editor's own fuzzy picker over every declaration there is --
            // VS Code's Ctrl+T, Helix's Space S -- by name, or by type when
            // the query is one: see `meadow_find`.
            WorkspaceSymbolRequest::METHOD => {
                self.answer::<WorkspaceSymbolRequest, _>(req, |s, p| {
                    Some(WorkspaceSymbolResponse::Nested(
                        s.workspace_symbols(&p.query),
                    ))
                })
            }
            IrRequest::METHOD => self.answer::<IrRequest, _>(req, |s, p| {
                let doc = s.docs.get(&p.text_document.uri)?;
                let offset = doc.index.offset(p.position.line, p.position.character);
                let view = doc.analysis.ir_at(offset, &p.ir, s.std.packages())?;
                let index = LineIndex::new(&view.text);
                let range = |start: usize, end: usize| {
                    let ((l1, c1), (l2, c2)) = index.range(Span::new(start as u32, end as u32));
                    Range::new(Position::new(l1, c1), Position::new(l2, c2))
                };
                Some(IrResult {
                    ir: p.ir,
                    name: view.name,
                    segments: view
                        .segments
                        .iter()
                        .map(|g| IrSegment {
                            range: range(g.start, g.end),
                            kind: g.kind.to_string(),
                            name: g.name.clone(),
                        })
                        .collect(),
                    focus: view.focus.iter().map(|(a, b)| range(*a, *b)).collect(),
                    text: view.text,
                })
            }),
            HoverRequest::METHOD => self.answer::<HoverRequest, _>(req, |s, p| {
                let (doc, offset) = s.at(&p.text_document_position_params)?;
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: doc.analysis.hover_at(offset)?,
                    }),
                    range: None,
                })
            }),
            Completion::METHOD => self.answer::<Completion, _>(req, |s, p| {
                let position = &p.text_document_position;
                let (offers, range) = s.completions(position);
                Some(CompletionResponse::List(CompletionList {
                    // The list depends on what is being typed -- a longer word
                    // reaches different functions -- so the editor asks again
                    // rather than filtering the first answer for ever.
                    is_incomplete: true,
                    items: offers
                        .into_iter()
                        .map(|o| CompletionItem {
                            label: o.name.clone(),
                            detail: Some(o.detail),
                            label_details: Some(CompletionItemLabelDetails {
                                description: Some(o.from),
                                ..Default::default()
                            }),
                            kind: Some(match o.kind {
                                PathKind::Value => CompletionItemKind::FUNCTION,
                                PathKind::Module => CompletionItemKind::MODULE,
                                PathKind::Type => CompletionItemKind::STRUCT,
                                PathKind::Ctor => CompletionItemKind::ENUM_MEMBER,
                            }),
                            // Say what is being replaced. Without a range the
                            // editor guesses, and a guess that disagrees is a
                            // list that empties as soon as anyone types.
                            filter_text: Some(o.name),
                            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                                range,
                                new_text: o.insert,
                            })),
                            insert_text_format: Some(if o.snippet {
                                InsertTextFormat::SNIPPET
                            } else {
                                InsertTextFormat::PLAIN_TEXT
                            }),
                            sort_text: Some(o.sort),
                            ..Default::default()
                        })
                        .collect(),
                }))
            }),
            GotoDefinition::METHOD => self.answer::<GotoDefinition, _>(req, |s, p| {
                let here = p.text_document_position_params.text_document.uri.clone();
                let (doc, offset) = s.at(&p.text_document_position_params)?;
                let loc = doc.analysis.definition_at(
                    offset,
                    s.std.definitions(),
                    s.std.declared_names(),
                )?;

                // The definition may be in a file that is not open — so the
                // position has to be measured against *that* source's text, not
                // this document's. The two agree only in the case this used to
                // be able to answer.
                let mine =
                    (loc.source.id == doc.analysis.source_id).then(|| doc.index.range(loc.span));

                let (uri, (start, end)) = match mine {
                    Some(range) => (here, range),
                    None => {
                        let path = s.std.path_of(loc.source)?;
                        (path_to_uri(&path)?, s.range_in(loc.source, loc.span))
                    }
                };
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range: Range {
                        start: Position::new(start.0, start.1),
                        end: Position::new(end.0, end.1),
                    },
                }))
            }),
            PrepareRenameRequest::METHOD => self.answer::<PrepareRenameRequest, _>(req, |s, p| {
                let (doc, offset) = s.at(&p)?;
                let span = doc.analysis.renameable_at(offset, &s.std)?;
                let (start, end) = doc.index.range(span);
                Some(PrepareRenameResponse::Range(Range {
                    start: Position::new(start.0, start.1),
                    end: Position::new(end.0, end.1),
                }))
            }),
            Rename::METHOD => {
                let id = req.id.clone();
                match cast::<Rename>(req) {
                    Ok((id, p)) => match self.rename(&p) {
                        Ok(edit) => Response::new_ok(id, Some(edit)),
                        // A refusal the user asked for -- "this is in the
                        // standard library", "that would read as a
                        // constructor" -- belongs in front of them as a
                        // message, which is what an error response becomes.
                        Err(msg) => {
                            Response::new_err(id, lsp_server::ErrorCode::RequestFailed as i32, msg)
                        }
                    },
                    Err(e) => Response::new_err(
                        id,
                        lsp_server::ErrorCode::InvalidParams as i32,
                        e.to_string(),
                    ),
                }
            }
            // A "Debug" above each top-level definition, and a "Test" above each
            // `@test`. The command is the
            // editor's to run -- it asks for arguments and starts `meadow dap`
            // -- so the lens only has to say what it would be starting.
            CodeLensRequest::METHOD => self.answer::<CodeLensRequest, _>(req, |s, p| {
                let doc = s.docs.get(&p.text_document.uri)?;
                let lenses = doc
                    .analysis
                    .functions
                    .iter()
                    .flat_map(|f| {
                        let (start, end) = doc.index.range(f.span);
                        let range = Range {
                            start: Position::new(start.0, start.1),
                            end: Position::new(end.0, end.1),
                        };
                        // A `@test` also gets "Test", first, the way a runner
                        // lists running before debugging. It runs `meadow test`
                        // itself, so it is exactly what the command line would
                        // do -- same build, same engine, same output.
                        let test = f.test.as_ref().map(|qualified| CodeLens {
                            range,
                            command: Some(Command {
                                title: "▶ Test".to_string(),
                                command: "meadow.testFunction".to_string(),
                                arguments: Some(vec![serde_json::json!({
                                    "uri": p.text_document.uri.as_str(),
                                    "name": f.name,
                                    "test": qualified,
                                })]),
                            }),
                            data: None,
                        });
                        test.into_iter().chain(std::iter::once(CodeLens {
                            range,
                            command: Some(Command {
                                title: "▶ Debug".to_string(),
                                command: "meadow.debugFunction".to_string(),
                                arguments: Some(vec![serde_json::json!({
                                    "uri": p.text_document.uri.as_str(),
                                    "name": f.name,
                                    "params": f.params,
                                    "signature": f.signature,
                                    "line": start.0,
                                })]),
                            }),
                            data: None,
                        }))
                    })
                    .collect();
                Some(lenses)
            }),
            InlayHintRequest::METHOD => self.answer::<InlayHintRequest, _>(req, |s, p| {
                let from = {
                    let doc = s.docs.get(&p.text_document.uri)?;
                    doc.index
                        .offset(p.range.start.line, p.range.start.character)
                };
                let to = {
                    let doc = s.docs.get(&p.text_document.uri)?;
                    doc.index.offset(p.range.end.line, p.range.end.character)
                };
                // Cloned out of the document because resolving a type name to
                // its declaration wants the whole server, and that file may not
                // be one this one is borrowed from.
                let wanted: Vec<_> = {
                    let doc = s.docs.get(&p.text_document.uri)?;
                    doc.analysis
                        .binders
                        .iter()
                        .filter(|h| (h.offset as usize) >= from && (h.offset as usize) <= to)
                        .cloned()
                        .collect()
                };

                let mut out = Vec::with_capacity(wanted.len());
                for hint in wanted {
                    let at = {
                        let doc = s.docs.get(&p.text_document.uri)?;
                        doc.index.position(hint.offset as usize)
                    };
                    let parts = s.label_parts(&p.text_document.uri, &hint.parts)?;
                    out.push(InlayHint {
                        position: Position::new(at.0, at.1),
                        label: InlayHintLabel::LabelParts(parts),
                        kind: Some(InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: None,
                        padding_right: None,
                        data: None,
                    });
                }
                Some(out)
            }),
            SemanticTokensFullRequest::METHOD => {
                self.answer::<SemanticTokensFullRequest, _>(req, |s, p| {
                    let doc = s.docs.get(&p.text_document.uri)?;
                    let raw = tokens::tokens(&doc.text, &doc.analysis);
                    Some(SemanticTokensResult::Tokens(SemanticTokens {
                        result_id: None,
                        data: tokens::encode(&raw)
                            .chunks(5)
                            .map(|c| SemanticToken {
                                delta_line: c[0],
                                delta_start: c[1],
                                length: c[2],
                                token_type: c[3],
                                token_modifiers_bitset: c[4],
                            })
                            .collect(),
                    }))
                })
            }
            _ => Response::new_err(
                id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("unhandled request: {}", req.method),
            ),
        }
    }

    /// Decode a request's parameters, run `f`, and encode what it returns.
    ///
    /// Every result type here is itself an `Option`, which is what lets the
    /// bodies use `?` freely: "nothing at this position" is a normal answer, and
    /// serialises to the `null` the protocol expects.
    fn answer<R, F>(&mut self, req: Request, f: F) -> Response
    where
        R: LspRequest,
        F: FnOnce(&mut Server, R::Params) -> R::Result,
    {
        let id: RequestId = req.id.clone();
        match cast::<R>(req) {
            Ok((id, params)) => Response::new_ok(id, f(self, params)),
            Err(e) => Response::new_err(
                id,
                lsp_server::ErrorCode::InvalidParams as i32,
                e.to_string(),
            ),
        }
    }

    /// The declarations `query` finds: the library's, and everything the
    /// package of each open document declares and depends on.
    ///
    /// A value is listed as its signature -- `length : [a] -> Int` -- rather
    /// than its bare name. Most editors show a workspace symbol's name and
    /// little else, and a list of `map`s is no use without their types; and an
    /// editor that filters the answers again by what was typed then keeps the
    /// ones a search by type found, because what was typed is in the label.
    fn workspace_symbols(&mut self, query: &str) -> Vec<WorkspaceSymbol> {
        // A package open in two documents declares the same things twice, and
        // an open `Std` module the same things the library does.
        let mut seen = std::collections::HashSet::new();
        let mut decls: Vec<meadow_find::Decl> = Vec::new();
        let mut homes: Vec<Option<Uri>> = Vec::new();
        let std_decls = self.std_decls.iter().map(|d| (d, None));
        let doc_decls = self.docs.iter().flat_map(|(uri, doc)| {
            doc.analysis
                .declarations
                .iter()
                .map(move |d| (d, Some(uri)))
        });
        for (d, home) in doc_decls.chain(std_decls) {
            if seen.insert((d.qualified(), d.kind, d.detail.clone())) {
                decls.push(d.clone());
                homes.push(home.cloned());
            }
        }
        let index = meadow_find::Index::new(decls);
        let hits = index.search(query, 256);
        hits.into_iter()
            .filter_map(|h| {
                let d = &index.decls[h.decl];
                let location = self.place(d, homes[h.decl].as_ref())?;
                Some(WorkspaceSymbol {
                    name: if d.kind.is_value() {
                        d.headline()
                    } else {
                        d.name.clone()
                    },
                    kind: symbol_kind(d.kind),
                    tags: None,
                    container_name: Some(d.module.clone()),
                    location: OneOf::Left(location),
                    data: None,
                })
            })
            .collect()
    }

    /// Where a declaration is, for the editor to go to: in the document's own
    /// text when it is in an open document -- which may be newer than the file
    /// -- and in the file otherwise.
    fn place(&mut self, d: &meadow_find::Decl, home: Option<&Uri>) -> Option<Location> {
        let site = d.site?;
        if let Some(uri) = home {
            let doc = self.docs.get(uri)?;
            if site.source.id == doc.analysis.source_id {
                let (start, end) = doc.index.range(site.span);
                return Some(Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position::new(start.0, start.1),
                        end: Position::new(end.0, end.1),
                    },
                });
            }
        }
        let path = self.std.path_of(site.source)?;
        let (start, end) = self.range_in(site.source, site.span);
        Some(Location {
            uri: path_to_uri(&path)?,
            range: Range {
                start: Position::new(start.0, start.1),
                end: Position::new(end.0, end.1),
            },
        })
    }

    fn at(&self, p: &TextDocumentPositionParams) -> Option<(&Doc, usize)> {
        let doc = self.docs.get(&p.text_document.uri)?;
        Some((doc, doc.index.offset(p.position.line, p.position.character)))
    }

    /// An inlay hint's label, with every type name in it linked.
    ///
    /// The link is what makes a hint navigable: `location` on a label part is
    /// how an editor offers ctrl-click and a hover on something that is not in
    /// the file at all. A name with nowhere to go -- a builtin like `Int`, or a
    /// type variable -- stays plain text rather than becoming a dead link.
    fn label_parts(&mut self, here: &Uri, parts: &[HintPart]) -> Option<Vec<InlayHintLabelPart>> {
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                HintPart::Text(t) => out.push(plain(t.clone())),
                HintPart::Name(name) => {
                    let loc = {
                        let doc = self.docs.get(here)?;
                        // The hint spells a type as its source does; the index
                        // knows it by its canonical name.
                        let name = doc.analysis.hint_names.get(name).unwrap_or(name);
                        doc.analysis
                            .names
                            .types
                            .get(name)
                            .or_else(|| doc.analysis.wider_names.types.get(name))
                            .copied()
                            .or_else(|| self.std.declared_names().types.get(name).copied())
                    };
                    let mut p = plain(name.to_string());
                    if let Some(loc) = loc {
                        p.location = self.locate(here, loc);
                        p.tooltip =
                            Some(InlayHintLabelPartTooltip::String(format!("type `{name}`")));
                    }
                    out.push(p);
                }
            }
        }
        Some(out)
    }

    /// A [`Loc`] as a protocol `Location`, wherever it lives.
    /// Rewrite every occurrence of one name, in every file of its package.
    ///
    /// The edits come from name resolution rather than from matching text, so
    /// a rename touches the binding the cursor is on and nothing else that
    /// happens to be spelled the same -- and it reaches the other modules of
    /// the package, which is where the rest of the occurrences usually are.
    fn rename(&mut self, p: &RenameParams) -> Result<WorkspaceEdit, String> {
        let here = p.text_document_position.text_document.uri.clone();
        let edits = {
            let doc = self
                .docs
                .get(&here)
                .ok_or_else(|| "that file is not open".to_string())?;
            let offset = doc.index.offset(
                p.text_document_position.position.line,
                p.text_document_position.position.character,
            );
            doc.analysis.rename_at(offset, &p.new_name, &self.std)?
        };

        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (source, span) in edits {
            let (uri, (start, end)) = match source {
                // This document: measured against the buffer, which is newer
                // than anything on disk.
                None => {
                    let doc = self.docs.get(&here).ok_or("that file is not open")?;
                    (here.clone(), doc.index.range(span))
                }
                Some(src) => {
                    let path = self
                        .std
                        .path_of(src)
                        .ok_or_else(|| "one of the files has no path".to_string())?;
                    let uri = path_to_uri(&path).ok_or("one of the files has no URI")?;
                    // An open document is the authority on its own text, even
                    // when the analysis read it from disk.
                    let range = match self.docs.get(&uri) {
                        Some(doc) => doc.index.range(span),
                        None => self.range_in(src, span),
                    };
                    (uri, range)
                }
            };
            changes.entry(uri).or_default().push(TextEdit {
                range: Range {
                    start: Position::new(start.0, start.1),
                    end: Position::new(end.0, end.1),
                },
                new_text: p.new_name.clone(),
            });
        }
        Ok(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    fn locate(&mut self, here: &Uri, loc: Loc) -> Option<Location> {
        let mine = {
            let doc = self.docs.get(here)?;
            (loc.source.id == doc.analysis.source_id).then(|| doc.index.range(loc.span))
        };
        let (uri, (start, end)) = match mine {
            Some(range) => (here.clone(), range),
            None => {
                let path = self.std.path_of(loc.source)?;
                (path_to_uri(&path)?, self.range_in(loc.source, loc.span))
            }
        };
        Some(Location {
            uri,
            range: Range {
                start: Position::new(start.0, start.1),
                end: Position::new(end.0, end.1),
            },
        })
    }

    /// `span` as a line/character range within `source`, which need not be a
    /// document the editor has open.
    ///
    /// The line index is kept because the text it is built from never changes:
    /// these are dependencies, and an edit to one recompiles it into a new
    /// [`Source`] with a new id rather than mutating this one.
    fn range_in(&mut self, source: Source, span: Span) -> ((u32, u32), (u32, u32)) {
        // A source id is minted per compile, and a package is recompiled on
        // every keystroke, so these accumulate — slowly, and forever. Nothing
        // here is worth remembering that long: drop the lot and rebuild the
        // few that are asked for again.
        if self.indexes.len() > 64 {
            self.indexes.clear();
        }
        self.indexes
            .entry(source.id)
            .or_insert_with(|| LineIndex::new(&source.content))
            .range(span)
    }
}

/// What an editor draws beside a declaration it lists.
fn symbol_kind(kind: meadow_find::Kind) -> SymbolKind {
    use meadow_find::Kind;
    match kind {
        Kind::Function | Kind::Macro | Kind::Operation => SymbolKind::FUNCTION,
        Kind::Method => SymbolKind::METHOD,
        Kind::Value => SymbolKind::CONSTANT,
        Kind::Constructor | Kind::Pattern => SymbolKind::ENUM_MEMBER,
        Kind::Type => SymbolKind::ENUM,
        Kind::Record => SymbolKind::STRUCT,
        Kind::Alias => SymbolKind::TYPE_PARAMETER,
        Kind::Trait => SymbolKind::INTERFACE,
        // What happens, as an event does, and what a handler answers.
        Kind::Effect => SymbolKind::EVENT,
    }
}

/// The filesystem path a `file://` URI names, or `None` for anything else.
///
/// The leading slash is the part that matters. A `file://` URI always has an
/// absolute path after the authority, so a Windows path arrives as
/// `file:///C:/x` and strips to `/C:/x` — which is not a path Windows can open,
/// and every lookup built on it silently finds nothing rather than failing.
/// That is [`path_to_uri`] read backwards, and the two have to agree.
fn uri_to_path(uri: &Uri) -> Option<std::path::PathBuf> {
    let s = uri.as_str().strip_prefix("file://")?;
    // Percent-decoding, on bytes rather than characters: an editor escapes a
    // non-ASCII path one UTF-8 byte at a time, so decoding per character would
    // reassemble it wrong.
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(b);
        }
    }
    let path = String::from_utf8(out).ok()?;
    // `/C:/x` -> `C:/x`, and only there: a POSIX path keeps its root. After
    // decoding, not before, because VS Code escapes the colon — it sends
    // `file:///c%3A/x` — so a check on the raw text never sees a drive.
    let path = match path.strip_prefix('/') {
        Some(rest) if starts_with_drive(rest) => rest.to_string(),
        _ => path,
    };
    Some(std::path::PathBuf::from(path))
}

/// `C:` or `c:` at the front of `s` — a Windows drive.
fn starts_with_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// A `file://` URI for a path on this machine.
///
/// Hand-rolled because the alternative is a dependency, and because the two
/// things that go wrong here are worth being explicit about: a Windows path
/// needs its separators flipped and a leading slash before the drive letter,
/// and anything outside the unreserved set has to be percent-encoded — an
/// install under a user whose name contains a space is not exotic.
///
/// A third: `std::fs::canonicalize` on Windows answers in the verbatim form,
/// `\\?\C:\...`, and the package loader canonicalizes. Encoded as it stands
/// that became `file:////%3F/C:/...`, which VS Code refuses -- and a refused
/// location in one inlay hint loses every hint in the reply, so a module whose
/// hints named a sibling's type showed none at all.
fn path_to_uri(path: &std::path::Path) -> Option<Uri> {
    let raw = path.to_str()?;
    let plain = match raw.strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share\x` is the share `\\server\share\x`.
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        },
        None => raw.to_string(),
    };
    let text = plain.replace('\\', "/");
    let mut out = String::from("file:");
    match text.strip_prefix("//") {
        // A share: its server is the URI's authority, `file://server/share/x`.
        Some(_) => {}
        None if text.starts_with('/') => out.push_str("//"),
        // A drive path: an empty authority, then `/C:/...`.
        None => out.push_str("///"),
    }
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out.parse().ok()
}

fn cast<R: LspRequest>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>> {
    req.extract(R::METHOD).map(|(id, p)| (id, p))
}

/// A label part with nothing attached — the common case, and the base every
/// other one is built from.
fn plain(value: String) -> InlayHintLabelPart {
    InlayHintLabelPart {
        value,
        tooltip: None,
        location: None,
        command: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{path_to_uri, uri_to_path};
    use lsp_types::Uri;
    use std::path::PathBuf;

    fn uri(s: &str) -> Uri {
        s.parse().expect("a valid uri")
    }

    /// The URI an editor sends for a Windows file names a path Windows can open.
    ///
    /// The bug this exists for: `file:///C:/x` stripped to `/C:/x`, which is not a
    /// Windows path, so finding the package a document belonged to silently
    /// found nothing and every sibling `use` was reported as a missing module.
    #[test]
    fn a_windows_uri_loses_the_slash_before_its_drive() {
        assert_eq!(
            uri_to_path(&uri("file:///C:/Users/me/pkg/src/Eval.mw")),
            Some(PathBuf::from("C:/Users/me/pkg/src/Eval.mw"))
        );
    }

    /// VS Code's own spelling: lower-case drive, colon percent-encoded. A check
    /// for the drive made *before* decoding sees `c%3A` and misses it, which is
    /// the version of this bug that survives testing with hand-written URIs.
    #[test]
    fn the_drive_is_found_after_percent_decoding() {
        assert_eq!(
            uri_to_path(&uri("file:///c%3A/Users/me/pkg/src/Eval.mw")),
            Some(PathBuf::from("c:/Users/me/pkg/src/Eval.mw"))
        );
    }

    #[test]
    fn a_posix_path_keeps_its_root() {
        assert_eq!(
            uri_to_path(&uri("file:///home/me/pkg/src/Eval.mw")),
            Some(PathBuf::from("/home/me/pkg/src/Eval.mw"))
        );
    }

    #[test]
    fn escapes_elsewhere_in_the_path_still_decode() {
        assert_eq!(
            uri_to_path(&uri("file:///c%3A/Users/Bob%20Smith/pkg/main.mw")),
            Some(PathBuf::from("c:/Users/Bob Smith/pkg/main.mw"))
        );
    }

    /// What `canonicalize` hands back on Windows is still an ordinary file URI.
    #[test]
    fn a_verbatim_path_becomes_an_ordinary_uri() {
        let got = path_to_uri(std::path::Path::new(r"\\?\C:\Users\me\pkg\src\Syntax.mw"));
        assert_eq!(
            got.map(|u| u.as_str().to_string()).as_deref(),
            Some("file:///C:/Users/me/pkg/src/Syntax.mw")
        );
    }

    #[test]
    fn a_share_keeps_its_server_as_the_authority() {
        for path in [
            r"\\?\UNC\server\share\pkg\main.mw",
            r"\\server\share\pkg\main.mw",
        ] {
            let got = path_to_uri(std::path::Path::new(path));
            assert_eq!(
                got.map(|u| u.as_str().to_string()).as_deref(),
                Some("file://server/share/pkg/main.mw"),
                "{path}"
            );
        }
    }

    #[test]
    fn a_posix_path_becomes_a_uri_with_an_empty_authority() {
        let got = path_to_uri(std::path::Path::new("/home/me/pkg/main.mw"));
        assert_eq!(
            got.map(|u| u.as_str().to_string()).as_deref(),
            Some("file:///home/me/pkg/main.mw")
        );
    }

    /// `path_to_uri` and `uri_to_path` have to agree, or a location the server
    /// hands out is one it cannot read back.
    #[cfg(windows)]
    #[test]
    fn the_two_conversions_round_trip() {
        let path = PathBuf::from(r"C:\Users\Bob Smith\pkg\src\Eval.mw");
        let back = uri_to_path(&path_to_uri(&path).expect("a uri")).expect("a path");
        assert_eq!(back, PathBuf::from("C:/Users/Bob Smith/pkg/src/Eval.mw"));
    }
}
