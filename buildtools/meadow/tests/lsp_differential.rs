//! MeadowBoot's language server -- `bootstrap/src/Lsp.mw`, declared with
//! Lingua's `lsp!` -- against the Rust compiler's, `meadow lsp`
//! (`buildtools/meadow-lsp`).
//!
//! Both are started over standard input and output and sent the same
//! session, package by package: `initialize`, every file of the package
//! opened, and then, of each file, its semantic tokens, inlay hints, code
//! lenses and formatting; hover, go to definition and prepare rename at
//! every name written in it; a rename of each name the first time it is
//! written; completion after each `|>`, after each `.` that follows a
//! capitalised name, and inside names; `meadow/ir` at its first
//! definitions; and workspace symbols for a few queries. An edit that makes
//! the first file of each package wrong, and the edit back, are each
//! followed by what is wrong in it.
//!
//! The answers are compared after only what the protocol leaves free is
//! made the same: request ids, and the order of lists whose order means
//! nothing -- completion items (an editor orders them by `sortText`),
//! workspace symbols, hints, lenses, a rename's edits, a file's
//! diagnostics. Diagnostics are compared as each file's last ones, since
//! when they are sent is the servers' own business: the Rust server analyses
//! a package's other files when it is idle, Lingua's after every edit, and
//! that is the incremental execution the two may do differently.
//!
//! Agreement is reported by kind of request. The test passes only when
//! every answer agrees, and it does not yet: see the numbers on
//! [`meadowboot_s_language_server_answers_as_meadow_lsp_answers`].
//!
//! `MEADOW_LSP_DIRS` (comma-separated, from the repository's root) narrows
//! the corpus; `MEADOW_LSP_EVERY=n` asks at every `n`th name only, and
//! `MEADOW_LSP_COMPLETE=n` completes at every `n`th place only; and
//! `MEADOWBOOT_EXE` runs a MeadowBoot already built rather than building
//! one. What each side answered, where they differ, is written to
//! `lsp-differences.txt` in `CARGO_TARGET_TMPDIR`, or `MEADOW_LSP_OUT`.

use meadow_compiler::lexer::{Token, tokenize};
use meadow_compiler::source::{Source, SourceKind};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository")
}

/// The Rust compiler's `meadow`: the one this test was built with, unless
/// `MEADOW_LSP_RUST` names another.
fn meadow() -> PathBuf {
    std::env::var_os("MEADOW_LSP_RUST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meadow")))
}

/// What builds MeadowBoot: `MEADOWBOOT_MEADOW`, or the `meadow` under test.
fn builder() -> PathBuf {
    std::env::var_os("MEADOWBOOT_MEADOW")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_meadow")))
}

/// MeadowBoot, native, as `bootstrap.rs` builds it -- or where
/// `MEADOWBOOT_EXE` says one already is.
fn meadowboot_exe() -> PathBuf {
    if let Some(exe) = std::env::var_os("MEADOWBOOT_EXE") {
        return PathBuf::from(exe);
    }
    let dir = repo().join("bootstrap");
    let build = |offline: bool| {
        let mut c = Command::new(builder());
        c.arg("build").arg("--release");
        if offline {
            c.arg("--offline");
        }
        c.arg(&dir).output().expect("meadow runs")
    };
    let mut out = build(true);
    if !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("not in the cache") {
        out = build(false);
    }
    assert!(
        out.status.success(),
        "MeadowBoot did not build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join("target/release/native/silo")
        .join(format!("MeadowBoot{}", std::env::consts::EXE_SUFFIX))
}

// --- a client ----------------------------------------------------------------

/// A language server, started, and what it has sent that was not the answer
/// to a request: its notifications.
struct Server {
    child: Child,
    stdin: ChildStdin,
    inbox: Receiver<Value>,
    next: i64,
    notes: Vec<Value>,
    /// Whether it has stopped answering: died, or took longer than anyone
    /// would wait.
    gone: bool,
}

impl Server {
    fn start(mut c: Command) -> Server {
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().expect("the server starts");
        let stdin = child.stdin.take().expect("its input");
        let stdout = child.stdout.take().expect("its output");
        let (tx, inbox) = channel();
        let pid = child.id();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        if length.is_some() {
                            break;
                        }
                        continue;
                    }
                    if let Some(n) = line.strip_prefix("Content-Length:") {
                        length = n.trim().parse::<usize>().ok();
                    }
                }
                let mut body = vec![0; length.unwrap_or(0)];
                if r.read_exact(&mut body).is_err() {
                    return;
                }
                trace(pid, "<-", &String::from_utf8_lossy(&body));
                if let Ok(v) = serde_json::from_slice::<Value>(&body)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        Server {
            child,
            stdin,
            inbox,
            next: 1,
            notes: Vec::new(),
            gone: false,
        }
    }

    fn send(&mut self, v: &Value) {
        if self.gone {
            return;
        }
        let body = serde_json::to_string(v).expect("json");
        trace(self.child.id(), "->", &body);
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        if self.stdin.write_all(framed.as_bytes()).is_err() || self.stdin.flush().is_err() {
            self.gone = true;
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The answer to a request -- its `result`, or its `error` under that
    /// key -- or `None` if none came.
    fn ask(&mut self, method: &str, params: Value) -> Option<Value> {
        if self.gone {
            return None;
        }
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            match self.inbox.recv_timeout(Duration::from_secs(patience())) {
                Ok(v) => {
                    if v.get("id").and_then(Value::as_i64) == Some(id) && v.get("method").is_none()
                    {
                        return Some(match v.get("error") {
                            Some(e) => json!({ "error": e }),
                            None => v.get("result").cloned().unwrap_or(Value::Null),
                        });
                    }
                    if v.get("method").is_some() {
                        self.notes.push(v);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    self.gone = true;
                    return None;
                }
            }
        }
    }

    /// Each file's diagnostics, as last published.
    fn diagnostics(&self) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        for n in &self.notes {
            if n["method"] == "textDocument/publishDiagnostics" {
                let uri = n["params"]["uri"].as_str().unwrap_or("").to_string();
                out.insert(uri, n["params"]["diagnostics"].clone());
            }
        }
        out
    }

    fn stop(mut self) {
        let _ = self.ask("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        drop(self.stdin);
        // Its own process, by its own handle: never anything by name.
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// With `MEADOW_LSP_TRACE` set, each message to and from server `pid`,
/// a line each in `lsp-trace-<pid>.txt` in `CARGO_TARGET_TMPDIR`.
fn trace(pid: u32, way: &str, body: &str) {
    if std::env::var_os("MEADOW_LSP_TRACE").is_none() {
        return;
    }
    let out = out_dir().join(format!("lsp-trace-{pid}.txt"));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        let cut = &body[..body.floor_char_boundary(400)];
        let _ = writeln!(f, "{way} {} {cut}", body.len());
    }
}

/// Where what the test writes goes: `MEADOW_LSP_OUT`, or the test's own
/// scratch directory.
fn out_dir() -> PathBuf {
    std::env::var_os("MEADOW_LSP_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
}

/// How long a request may take, in seconds: the first in a package compiles
/// it, which for a large one is minutes.
fn patience() -> u64 {
    std::env::var("MEADOW_LSP_PATIENCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(900)
}

// --- places --------------------------------------------------------------------

/// A path as both servers write a `file:` uri (`meadow_lsp`'s
/// `path_to_uri`): `/` between parts, an upper-case drive, and every byte but
/// the unreserved ones escaped.
fn uri_of(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let plain = raw.strip_prefix(r"\\?\").unwrap_or(&raw).replace('\\', "/");
    let mut out = String::from(if plain.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for b in plain.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Byte `offset` of `text` as the protocol's line and UTF-16 column.
fn position(text: &str, offset: usize) -> Value {
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let character: usize = text[start..offset].chars().map(char::len_utf16).sum();
    json!({"line": line, "character": character})
}

// --- the corpus ----------------------------------------------------------------

/// Every `.mw` file under `dirs`, but what a build wrote: the corpus the
/// other differentials use.
fn sources(dirs: &[&str]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "mw") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(&repo().join(d), &mut out);
    }
    out
}

/// The package a file is in: the nearest directory above it with a
/// `Meadow.toml`.
fn package_of(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|d| d.join("Meadow.toml").is_file())
        .map(Path::to_path_buf)
}

/// What is asked of one file: where its names are, its `|>`s and the
/// `.`s after capitalised names, and its top-level definitions' names.
struct Points {
    names: Vec<(usize, usize)>,
    pipes: Vec<usize>,
    dots: Vec<usize>,
    defs: Vec<usize>,
}

fn points(text: &str) -> Points {
    let lexed = tokenize(Source::new(SourceKind::Interactive, text.into()));
    let toks = &lexed.tokens;
    let mut p = Points {
        names: Vec::new(),
        pipes: Vec::new(),
        dots: Vec::new(),
        defs: Vec::new(),
    };
    for (i, t) in toks.iter().enumerate() {
        let (s, e) = (t.span.start as usize, t.span.end as usize);
        match t.value() {
            Token::LowerIdent(_) | Token::UpperIdent(_) => {
                p.names.push((s, e));
                let at_line_start = s == 0 || text.as_bytes()[s - 1] == b'\n';
                let _ = at_line_start;
                if i > 0 && matches!(toks[i - 1].value(), Token::Fun | Token::Def) && {
                    let st = toks[i - 1].span.start as usize;
                    st == 0 || text.as_bytes()[st - 1] == b'\n'
                } {
                    p.defs.push(s);
                }
            }
            Token::RPipe => p.pipes.push(e),
            Token::Period if i > 0 && matches!(toks[i - 1].value(), Token::UpperIdent(_)) => {
                p.dots.push(e)
            }
            _ => {}
        }
    }
    p
}

// --- comparing -----------------------------------------------------------------

/// An answer with what the protocol leaves free made the same: the order of
/// lists that are sets.
fn normal(kind: &str, v: &Value) -> Value {
    let sorted = |xs: &Vec<Value>| {
        let mut xs = xs.clone();
        xs.sort_by_key(|x| x.to_string());
        Value::Array(xs)
    };
    match (kind, v) {
        ("completion", Value::Object(o)) => {
            let mut o = o.clone();
            if let Some(Value::Array(items)) = o.get("items") {
                let mut items = items.clone();
                items.sort_by_key(|i| {
                    (
                        i["sortText"].as_str().unwrap_or("").to_string(),
                        i["label"].as_str().unwrap_or("").to_string(),
                        i.to_string(),
                    )
                });
                o.insert("items".into(), Value::Array(items));
            }
            Value::Object(o)
        }
        ("rename", Value::Object(o)) if o.contains_key("changes") => {
            let mut changes = serde_json::Map::new();
            if let Some(Value::Object(cs)) = o.get("changes") {
                for (uri, edits) in cs {
                    changes.insert(
                        uri.clone(),
                        edits.as_array().map(sorted).unwrap_or(edits.clone()),
                    );
                }
            }
            json!({ "changes": changes })
        }
        ("symbols" | "hints" | "lenses" | "diagnostics", Value::Array(xs)) => sorted(xs),
        _ => v.clone(),
    }
}

/// Semantic tokens as the protocol encodes them, each made absolute again:
/// its line, column, length and class.
fn decoded(v: &Value) -> Vec<[u64; 4]> {
    let data: Vec<u64> = v["data"]
        .as_array()
        .map(|xs| xs.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let (mut line, mut col) = (0, 0);
    data.chunks(5)
        .filter(|c| c.len() == 5)
        .map(|c| {
            if c[0] > 0 {
                col = 0;
            }
            line += c[0];
            col += c[1];
            [line, col, c[2], c[3]]
        })
        .collect()
}

/// How many of each kind of request were asked, and how many the two
/// answered alike.
#[derive(Default)]
struct Tally {
    kinds: BTreeMap<String, (usize, usize)>,
    log: String,
    /// How many differences of each kind are written out in full.
    shown: HashMap<String, usize>,
    /// Of the differences, how many are a refusal: an error from the Rust
    /// server, nothing from MeadowBoot's.
    refused: BTreeMap<String, usize>,
}

impl Tally {
    fn add(&mut self, kind: &str, what: &str, rust: Option<Value>, boot: Option<Value>) {
        let entry = self.kinds.entry(kind.to_string()).or_default();
        entry.0 += 1;
        let r = rust.as_ref().map(|v| normal(kind, v));
        let b = boot.as_ref().map(|v| normal(kind, v));
        if r.is_some() && r == b {
            entry.1 += 1;
            return;
        }
        // A refusal the Rust server answers with an error and MeadowBoot's
        // with nothing: counted as a difference, and said apart.
        if r.as_ref().is_some_and(|v| v.get("error").is_some()) && b == Some(Value::Null) {
            *self.refused.entry(kind.to_string()).or_default() += 1;
        }
        let shown = self.shown.entry(kind.to_string()).or_default();
        *shown += 1;
        if *shown <= 200 {
            let cut = |v: &Option<Value>| {
                let s = v
                    .as_ref()
                    .map_or("(no answer)".to_string(), |v| v.to_string());
                if s.len() > 1500 {
                    format!("{}…", &s[..s.floor_char_boundary(1500)])
                } else {
                    s
                }
            };
            // Two token lists are told apart where they first part: a token
            // each, as a line, a column, a length and a class.
            let (r, b) = match (kind, &r, &b) {
                ("tokens", Some(x), Some(y)) => {
                    let (x, y) = (decoded(x), decoded(y));
                    let at = x
                        .iter()
                        .zip(&y)
                        .position(|(p, q)| p != q)
                        .unwrap_or(x.len().min(y.len()));
                    let near = |t: &Vec<[u64; 4]>| {
                        json!(t[at.saturating_sub(1)..(at + 3).min(t.len())].to_vec())
                    };
                    (
                        Some(json!({"tokens": x.len(), "from": near(&x)})),
                        Some(json!({"tokens": y.len(), "from": near(&y)})),
                    )
                }
                // Two lists of offers, where they first part.
                ("completion", Some(x), Some(y)) => {
                    let none = Vec::new();
                    let (xs, ys) = (
                        x["items"].as_array().unwrap_or(&none),
                        y["items"].as_array().unwrap_or(&none),
                    );
                    let at = xs
                        .iter()
                        .zip(ys)
                        .position(|(p, q)| p != q)
                        .unwrap_or(xs.len().min(ys.len()));
                    (
                        Some(json!({"items": xs.len(), "at": at, "item": xs.get(at)})),
                        Some(json!({"items": ys.len(), "at": at, "item": ys.get(at)})),
                    )
                }
                // Two lists, where they first part.
                (_, Some(Value::Array(xs)), Some(Value::Array(ys))) => {
                    let at = xs
                        .iter()
                        .zip(ys)
                        .position(|(p, q)| p != q)
                        .unwrap_or(xs.len().min(ys.len()));
                    (
                        Some(json!({"len": xs.len(), "at": at, "item": xs.get(at)})),
                        Some(json!({"len": ys.len(), "at": at, "item": ys.get(at)})),
                    )
                }
                _ => (r, b),
            };
            self.log.push_str(&format!(
                "== {kind} {what}\nmeadow:     {}\nmeadowboot: {}\n",
                cut(&r),
                cut(&b)
            ));
        }
    }

    fn report(&self) -> String {
        let mut out = String::from(
            "kind            agree / asked   (of the rest, refusals answered with nothing)\n",
        );
        let (mut all, mut same) = (0, 0);
        for (k, (n, ok)) in &self.kinds {
            match self.refused.get(k) {
                Some(r) => out.push_str(&format!("{k:<15} {ok:>6} / {n:<6}  ({r})\n")),
                None => out.push_str(&format!("{k:<15} {ok:>6} / {n:<6}\n")),
            }
            all += n;
            same += ok;
        }
        out.push_str(&format!("{:<15} {same:>6} / {all:<6}\n", "all"));
        out
    }
}

// --- the session ---------------------------------------------------------------

fn rust_server(root: &Path) -> Server {
    let mut c = Command::new(meadow());
    c.arg("lsp").arg("--stdio").current_dir(root);
    Server::start(c)
}

fn boot_server(root: &Path, exe: &Path) -> Server {
    let mut c = Command::new(exe);
    c.arg("lsp")
        .arg("--stdio")
        .current_dir(root)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env("MEADOW", meadow())
        .env("MEADOWBOOT_SET", "macros=Glade")
        .env_remove("MEADOWBOOT_EXPANSIONS");
    Server::start(c)
}

/// Both servers asked the same thing, and what they answered tallied.
fn both(
    t: &mut Tally,
    servers: &mut (Server, Server),
    kind: &str,
    what: &str,
    method: &str,
    params: Value,
) {
    let (was_r, was_b) = (servers.0.gone, servers.1.gone);
    let r = servers.0.ask(method, params.clone());
    let b = servers.1.ask(method, params);
    if servers.0.gone && !was_r {
        progress(&format!("  meadow stopped answering at {method} {what}"));
    }
    if servers.1.gone && !was_b {
        progress(&format!(
            "  MeadowBoot stopped answering at {method} {what}"
        ));
    }
    t.add(kind, what, r, b);
}

/// The session for the files `files`, which are one package's (or one file
/// of none), as both servers are sent it.
fn session(t: &mut Tally, root: &Path, files: &[PathBuf], exe: &Path, every: usize) {
    // The Rust server analyses a package again for each completion, which in
    // a package the size of MeadowBoot's is seconds: `MEADOW_LSP_COMPLETE=n`
    // asks at every `n`th place only.
    let completing = std::env::var("MEADOW_LSP_COMPLETE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1usize)
        .max(1);
    let mut servers = (rust_server(root), boot_server(root, exe));
    let init = json!({
        "processId": null,
        "rootUri": uri_of(root),
        "capabilities": {},
    });
    let r = servers.0.ask("initialize", init.clone());
    let b = servers.1.ask("initialize", init);
    t.add(
        "initialize",
        &root.display().to_string(),
        r.map(|v| v["capabilities"].clone()),
        b.map(|v| v["capabilities"].clone()),
    );
    servers.0.notify("initialized", json!({}));
    servers.1.notify("initialized", json!({}));

    let texts: Vec<(PathBuf, String, String)> = files
        .iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(f).ok()?;
            Some((f.clone(), uri_of(f), text))
        })
        .collect();
    for (_, uri, text) in &texts {
        let open = json!({"textDocument": {"uri": uri, "languageId": "meadow", "version": 1, "text": text}});
        servers.0.notify("textDocument/didOpen", open.clone());
        servers.1.notify("textDocument/didOpen", open);
    }

    for (path, uri, text) in &texts {
        let name = path
            .strip_prefix(repo())
            .unwrap_or(path)
            .display()
            .to_string();
        let doc = json!({"textDocument": {"uri": uri}});
        both(
            t,
            &mut servers,
            "tokens",
            &name,
            "textDocument/semanticTokens/full",
            doc.clone(),
        );
        let end = position(text, text.len());
        both(
            t,
            &mut servers,
            "hints",
            &name,
            "textDocument/inlayHint",
            json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 0, "character": 0}, "end": end}}),
        );
        both(
            t,
            &mut servers,
            "lenses",
            &name,
            "textDocument/codeLens",
            doc.clone(),
        );
        both(
            t,
            &mut servers,
            "formatting",
            &name,
            "textDocument/formatting",
            json!({"textDocument": {"uri": uri}, "options": {"tabSize": 2, "insertSpaces": true}}),
        );

        let p = points(text);
        let mut renamed = std::collections::HashSet::new();
        for (k, (s, e)) in p.names.iter().enumerate() {
            if k % every != 0 {
                continue;
            }
            let at = position(text, *s);
            let word = &text[*s..*e];
            let what = format!("{name}:{}:{} `{word}`", at["line"], at["character"]);
            let here = json!({"textDocument": {"uri": uri}, "position": at});
            both(
                t,
                &mut servers,
                "hover",
                &what,
                "textDocument/hover",
                here.clone(),
            );
            both(
                t,
                &mut servers,
                "definition",
                &what,
                "textDocument/definition",
                here.clone(),
            );
            both(
                t,
                &mut servers,
                "prepareRename",
                &what,
                "textDocument/prepareRename",
                here.clone(),
            );
            if renamed.insert(word.to_string()) {
                let new = if word.starts_with(|c: char| c.is_uppercase()) {
                    "RenamedQ"
                } else {
                    "renamedQ"
                };
                let mut params = here.clone();
                params["newName"] = json!(new);
                both(
                    t,
                    &mut servers,
                    "rename",
                    &what,
                    "textDocument/rename",
                    params,
                );
            }
            // Completion inside a name: what has been typed of it so far.
            if k % (every * 7 * completing) == 0 && e - s > 2 {
                let mid = s + 2;
                if text.is_char_boundary(mid) {
                    let at = position(text, mid);
                    both(
                        t,
                        &mut servers,
                        "completion",
                        &format!("{what} after 2"),
                        "textDocument/completion",
                        json!({"textDocument": {"uri": uri}, "position": at}),
                    );
                }
            }
        }
        for (k, at) in p.pipes.iter().chain(p.dots.iter()).enumerate() {
            if k % (every * completing) != 0 {
                continue;
            }
            // After `|> ` -- the space typed -- or right after the `.`.
            let at = if text[*at..].starts_with(' ') {
                at + 1
            } else {
                *at
            };
            let pos = position(text, at);
            both(
                t,
                &mut servers,
                "completion",
                &format!("{name}:{}:{}", pos["line"], pos["character"]),
                "textDocument/completion",
                json!({"textDocument": {"uri": uri}, "position": pos}),
            );
        }
        // Core only: asked for `cut` of `orElse` in `benches/Latency`, the
        // Rust server stopped answering anything (2026-11-09), and a server
        // that does not answer would leave the rest of the package unasked.
        // `MEADOW_LSP_IRS=core,cut,axcut` asks for the others too.
        let irs = std::env::var("MEADOW_LSP_IRS").unwrap_or_else(|_| "core".to_string());
        for at in p.defs.iter().take(3) {
            let pos = position(text, *at);
            for ir in irs.split(',') {
                both(
                    t,
                    &mut servers,
                    "ir",
                    &format!("{name}:{}:{} {ir}", pos["line"], pos["character"]),
                    "meadow/ir",
                    json!({"textDocument": {"uri": uri}, "position": pos, "ir": ir}),
                );
            }
        }
    }

    for q in ["map", "fold", "Int -> Int", "String", "main"] {
        both(
            t,
            &mut servers,
            "symbols",
            q,
            "workspace/symbol",
            json!({"query": q}),
        );
    }

    // What is wrong after the server was told of every file.
    let (rd, bd) = (servers.0.diagnostics(), servers.1.diagnostics());
    for (_, uri, _) in &texts {
        t.add(
            "diagnostics",
            uri,
            Some(rd.get(uri).cloned().unwrap_or(json!([]))),
            Some(bd.get(uri).cloned().unwrap_or(json!([]))),
        );
    }

    // An edit that is wrong, and the edit back: what each says of the file
    // after each. A hover after the edit is answered after its diagnostics.
    if let Some((_, uri, text)) = texts.first() {
        for (version, changed) in [
            (
                2,
                format!("{text}\ndef lspDifferentialBroken = notDeclaredAnywhere 1\n"),
            ),
            (3, text.clone()),
        ] {
            let change = json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": changed}]});
            servers.0.notes.clear();
            servers.1.notes.clear();
            servers.0.notify("textDocument/didChange", change.clone());
            servers.1.notify("textDocument/didChange", change);
            let here =
                json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}});
            let _ = servers.0.ask("textDocument/hover", here.clone());
            let _ = servers.1.ask("textDocument/hover", here);
            let (rd, bd) = (servers.0.diagnostics(), servers.1.diagnostics());
            t.add(
                "diagnostics",
                &format!("{uri} after edit {version}"),
                Some(rd.get(uri).cloned().unwrap_or(json!([]))),
                Some(bd.get(uri).cloned().unwrap_or(json!([]))),
            );
        }
    }
    // The protocol around the requests: a method neither answers, a file not
    // open, and a file closed -- what is published when it is, and what a
    // request about it is answered after.
    if let Some((path, uri, text)) = texts.first() {
        let name = path
            .strip_prefix(repo())
            .unwrap_or(path)
            .display()
            .to_string();
        let first = points(text).names.first().map_or(0, |n| n.0);
        let here = json!({"textDocument": {"uri": uri}, "position": position(text, first), "context": {"includeDeclaration": true}});
        both(
            t,
            &mut servers,
            "protocol",
            &format!("{name} references"),
            "textDocument/references",
            here.clone(),
        );
        let nowhere = json!({"textDocument": {"uri": uri_of(&root.join("nowhere.mw"))}, "position": {"line": 0, "character": 0}});
        both(
            t,
            &mut servers,
            "protocol",
            "hover of a file not open",
            "textDocument/hover",
            nowhere,
        );
        servers.0.notes.clear();
        servers.1.notes.clear();
        let close = json!({"textDocument": {"uri": uri}});
        servers.0.notify("textDocument/didClose", close.clone());
        servers.1.notify("textDocument/didClose", close);
        both(
            t,
            &mut servers,
            "protocol",
            &format!("{name} hover after it closed"),
            "textDocument/hover",
            here,
        );
        // Lingua publishes an empty list for a document closed, since it
        // will not look at it again to say otherwise; the Rust server
        // publishes nothing. Both leave the editor showing nothing wrong in
        // it, so that one is not counted.
        let said = |s: &Server| {
            json!(
                s.notes
                    .iter()
                    .filter(|n| {
                        !(n["method"] == "textDocument/publishDiagnostics"
                            && n["params"]["uri"] == uri.as_str()
                            && n["params"]["diagnostics"]
                                .as_array()
                                .is_some_and(Vec::is_empty))
                    })
                    .map(|n| n["method"].clone())
                    .collect::<Vec<_>>()
            )
        };
        t.add(
            "protocol",
            &format!("{name} what closing it published"),
            Some(said(&servers.0)),
            Some(said(&servers.1)),
        );
    }
    servers.0.stop();
    servers.1.stop();
}

/// The corpus, package by package: each package's files together, and each
/// file of no package alone.
fn sessions(files: Vec<PathBuf>) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut by: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut alone = Vec::new();
    for f in files {
        match package_of(&f) {
            Some(root) => by.entry(root).or_default().push(f),
            None => alone.push(f),
        }
    }
    let mut out: Vec<(PathBuf, Vec<PathBuf>)> = by.into_iter().collect();
    out.extend(alone.into_iter().map(|f| {
        (
            f.parent().map(Path::to_path_buf).unwrap_or_default(),
            vec![f],
        )
    }));
    out
}

/// A line on standard error and in `lsp-progress.txt`, as each package goes.
fn progress(line: &str) {
    eprintln!("{line}");
    let out = out_dir().join("lsp-progress.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// MeadowBoot's server answers every request as `meadow lsp` answers it.
///
/// Not yet, so it is ignored: run it with `--ignored` to see how far. On
/// 2026-11-09, over `examples`, `benches` and `glade` at every name, the two
/// agreed on 15897 of 16331 answers:
///
/// ```text
/// completion       712 / 1013    hover           4610 / 4624
/// definition      4620 / 4624    initialize        13 / 13
/// diagnostics       45 / 45      ir                 0 / 47
/// formatting        19 / 19      lenses            19 / 19
/// hints             18 / 19      prepareRename   4624 / 4624
/// protocol          52 / 52      rename          1148 / 1148
/// symbols            0 / 65      tokens            17 / 19
/// ```
///
/// and over `lib` and `bootstrap` at every tenth name, completing at every
/// fifth place (`MEADOW_LSP_EVERY=10 MEADOW_LSP_COMPLETE=5`), on 53792 of
/// 56548: definition 15423 / 15995, hover 14740 / 15995, prepareRename
/// 15897 / 15995, rename 6713 / 6878, tokens 81 / 133, hints 105 / 133,
/// lenses 132 / 133, formatting 133 / 133, completion 371 / 676, diagnostics
/// 124 / 151. Most of the difference there is inside macro calls -- `lang!`,
/// `database!` -- whose expansions the two compilers type and place
/// differently.
///
/// What is left is of two kinds. MeadowBoot does not answer workspace
/// symbols (`meadow_find`'s search) or `meadow/ir` yet, and its completion
/// lacks what an `impl` and a default method export (`#impl.Trait`). And the
/// two compilers differ in a few answers: a `runSt`'s type at a use, what a
/// derive mentions, and, of a rename, which names a field is read through
/// (MeadowBoot tells a field from its syntax, the Rust server by its
/// namespace).
#[test]
#[ignore = "MeadowBoot's server does not agree with meadow lsp yet: see the doc comment"]
fn meadowboot_s_language_server_answers_as_meadow_lsp_answers() {
    let dirs: Vec<String> = std::env::var("MEADOW_LSP_DIRS")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_else(|_| {
            ["lib", "examples", "benches", "bootstrap", "glade", "silo"]
                .map(String::from)
                .to_vec()
        });
    let every = std::env::var("MEADOW_LSP_EVERY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1usize)
        .max(1);
    let exe = meadowboot_exe();
    let files = sources(&dirs.iter().map(String::as_str).collect::<Vec<_>>());
    let mut t = Tally::default();
    for (root, files) in sessions(files) {
        let started = std::time::Instant::now();
        let name = root
            .strip_prefix(repo())
            .unwrap_or(&root)
            .display()
            .to_string();
        progress(&format!("{name} ({} files): started", files.len()));
        session(&mut t, &root, &files, &exe, every);
        progress(&format!("{name}: {:.0?}", started.elapsed()));
    }
    let report = t.report();
    let out = out_dir().join("lsp-differences.txt");
    let _ = std::fs::write(&out, format!("{report}\n{}", t.log));
    eprintln!("{report}\nwhat differs: {}", out.display());
    assert!(
        t.kinds.values().all(|(n, ok)| n == ok),
        "the servers answered differently:\n{report}"
    );
}
