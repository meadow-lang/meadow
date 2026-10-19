//! The server over a real LSP connection: the handshake, live diagnostics as the
//! document changes, and each request answered in the protocol's own shapes.

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::*;
use serde_json::{Value, json};

/// A client talking to the server on a background thread.
struct Client {
    conn: Connection,
    handle: Option<std::thread::JoinHandle<()>>,
    next_id: i32,
    opened: bool,
    capabilities: Option<Value>,
}

impl Client {
    fn start() -> Client {
        Client::start_with(None)
    }

    /// The same, with the standard library's sources written out somewhere --
    /// which is what lets a definition inside `Std` be named as a file.
    fn start_with(std_src_root: Option<std::path::PathBuf>) -> Client {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || {
            let opts = meadow::Options::debug();
            let (packages, _) = meadow::stdlib::std_packages(opts);
            let modules = meadow::stdlib::std_modules(opts)
                .0
                .into_iter()
                .map(|(dotted, pkg)| (dotted.to_string(), pkg))
                .collect();
            meadow_lsp::server::serve(
                &server,
                packages,
                modules,
                std_src_root,
                meadow::stdlib::MODULES,
                None,
            )
            .expect("server");
        });

        let mut c = Client {
            conn: client,
            handle: Some(handle),
            next_id: 0,
            opened: false,
            capabilities: None,
        };
        // `Connection::initialize` waits for the request, then for `initialized`.
        let caps = c.request("initialize", json!({"capabilities": {}}));
        c.notify("initialized", json!({}));
        c.capabilities = Some(caps);
        c
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.conn
            .sender
            .send(Message::Request(Request {
                id: id.into(),
                method: method.into(),
                params,
            }))
            .unwrap();
        loop {
            match self.conn.receiver.recv().unwrap() {
                Message::Response(Response {
                    response_result, ..
                }) => match response_result {
                    Ok(v) => return v,
                    Err(e) => panic!("{method} failed: {e:?}"),
                },
                // Diagnostics can arrive between a request and its response.
                Message::Notification(_) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    fn notify(&self, method: &str, params: Value) {
        self.conn
            .sender
            .send(Message::Notification(Notification {
                method: method.into(),
                params,
            }))
            .unwrap();
    }

    /// Open or replace a document and return the diagnostics it produced.
    fn set(&mut self, text: &str) -> Vec<Diagnostic> {
        if self.opened {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": URI, "version": 2},
                    "contentChanges": [{"text": text}]
                }),
            );
        } else {
            self.opened = true;
            self.notify(
                "textDocument/didOpen",
                json!({"textDocument": {
                    "uri": URI, "languageId": "meadow", "version": 1, "text": text
                }}),
            );
        }
        loop {
            if let Message::Notification(n) = self.conn.receiver.recv().unwrap() {
                if n.method == "textDocument/publishDiagnostics" {
                    let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                    return p.diagnostics;
                }
            }
        }
    }

    fn at(&mut self, method: &str, line: u32, character: u32) -> Value {
        self.request(
            method,
            json!({
                "textDocument": {"uri": URI},
                "position": {"line": line, "character": character}
            }),
        )
    }
}

const URI: &str = "file:///main.mw";

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        drop(std::mem::replace(&mut self.conn, Connection::memory().0));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

const SRC: &str = "\
-- Doubles its argument.
fun double n = n * 2

def demo = double 21
";

#[test]
fn the_handshake_advertises_what_we_implement() {
    let c = Client::start();
    let caps = c.capabilities.as_ref().unwrap();
    let s = &caps["capabilities"];
    assert_eq!(s["hoverProvider"], json!(true));
    assert_eq!(s["definitionProvider"], json!(true));
    assert_eq!(s["inlayHintProvider"], json!(true));
    assert!(s["semanticTokensProvider"].is_object());
    assert_eq!(s["documentFormattingProvider"], json!(true));
    assert_eq!(s["textDocumentSync"], json!(1), "full sync");
}

/// Format-on-save of a document with text that is not ASCII -- a name, a
/// string, a comment, a byte-order mark -- once panicked in the formatter
/// and took the server with it. It formats, and the server goes on.
#[test]
fn formatting_text_that_is_not_ascii_answers() {
    let mut c = Client::start();
    for doc in [
        "def caf\u{e9} = 1   \n",
        "def s = \"\u{1f600}\"\n",
        "def x = 1 -- \u{2192} fine\n",
        "\u{feff}def x = 1   \n",
    ] {
        c.set(doc);
        let edits = c.request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": URI},
                "options": {"tabSize": 2, "insertSpaces": true}
            }),
        );
        assert!(edits.is_array() || edits.is_null(), "{doc:?}: {edits}");
    }
    c.set("def demo = 1\n");
    let edits = c.request(
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": URI},
            "options": {"tabSize": 2, "insertSpaces": true}
        }),
    );
    assert!(
        edits.is_array() || edits.is_null(),
        "the server still answers: {edits}"
    );
}

/// `textDocument/formatting` -- what format-on-save sends -- answers with the
/// edits `meadow fmt` would make, computed from the document as the editor
/// holds it rather than as it is on disk.
#[test]
fn formatting_answers_with_what_meadow_fmt_would_do() {
    let mut c = Client::start();
    let messy = "def one = 1\n\nfun f x =\n        x + 1\n\n\n\ndef demo =   \n   f 1\n";
    c.set(messy);
    let edits = c.request(
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": URI},
            "options": {"tabSize": 8, "insertSpaces": false}
        }),
    );
    let edits = edits.as_array().expect("an array of edits");
    assert_eq!(
        edits.len(),
        1,
        "one edit, over the lines that change: {edits:?}"
    );

    // Applied the way an editor applies it.
    let e = &edits[0];
    let line_start = |line: u64| {
        messy
            .split_inclusive('\n')
            .take(line as usize)
            .map(str::len)
            .sum::<usize>()
    };
    let at = |p: &Value| {
        line_start(p["line"].as_u64().unwrap()) + p["character"].as_u64().unwrap() as usize
    };
    let mut applied = messy.to_string();
    applied.replace_range(
        at(&e["range"]["start"])..at(&e["range"]["end"]),
        e["newText"].as_str().unwrap(),
    );
    assert_eq!(applied, meadow_lsp::format::formatted(messy));
    assert_eq!(
        e["range"]["start"]["line"],
        json!(2),
        "the first two lines were already right"
    );

    // Once formatted, saving again asks for nothing -- and a client sending a
    // different tab size and tab preference changes nothing either: a file
    // formats one way, in any editor and on the command line.
    c.set(&applied);
    let again = c.request(
        "textDocument/formatting",
        json!({"textDocument": {"uri": URI}, "options": {"tabSize": 2, "insertSpaces": true}}),
    );
    assert_eq!(again, json!([]));
}

#[test]
fn diagnostics_arrive_on_open_and_update_on_every_edit() {
    let mut c = Client::start();

    assert!(c.set(SRC).is_empty(), "a good file reports nothing");

    // Break it: diagnostics must appear without any request being made.
    let broken = c.set("def demo = 1 + \"oops\"\n");
    assert_eq!(broken.len(), 1);
    assert!(
        broken[0].message.contains("type mismatch"),
        "{:?}",
        broken[0]
    );
    assert_eq!(broken[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(broken[0].range.start.line, 0);

    // …and disappear again when it is fixed.
    assert!(c.set(SRC).is_empty(), "the error should clear");
}

#[test]
fn a_file_changing_on_disk_analyses_the_open_documents_again() {
    // What a build writes -- a lockfile, having fetched what the editor never
    // fetches -- can change what an open document means, and nothing is typed
    // in the editor to say so.
    let mut c = Client::start();
    let broken = c.set(
        "def demo = 1 + \"oops\"
",
    );
    assert_eq!(broken.len(), 1);
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": "file:///Meadow.lock", "type": 2}]}),
    );
    loop {
        if let Message::Notification(n) = c.conn.receiver.recv().unwrap()
            && n.method == "textDocument/publishDiagnostics"
        {
            let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
            assert_eq!(p.uri.as_str(), URI);
            assert_eq!(p.diagnostics.len(), 1, "analysed again, as it stands");
            break;
        }
    }
}

#[test]
fn a_parse_error_is_reported_rather_than_crashing_the_server() {
    let mut c = Client::start();
    let diags = c.set("def demo = let in\n");
    assert!(!diags.is_empty());
    // The server is still answering afterwards.
    assert!(c.set(SRC).is_empty());
}

#[test]
fn hover_shows_the_signature_and_the_doc_comment() {
    let mut c = Client::start();
    c.set(SRC);
    let hover = c.at("textDocument/hover", 3, 12);
    let text = hover["contents"]["value"].as_str().unwrap();
    assert!(
        text.contains("double : forall n. Mul n => n -> n"),
        "{text}"
    );
    assert!(text.contains("Doubles its argument."), "{text}");
}

#[test]
fn hover_on_empty_space_is_null_not_an_error() {
    let mut c = Client::start();
    c.set(SRC);
    assert_eq!(c.at("textDocument/hover", 2, 0), Value::Null);
}

#[test]
fn go_to_definition_points_at_the_binder() {
    let mut c = Client::start();
    c.set(SRC);
    let def = c.at("textDocument/definition", 3, 12);
    assert_eq!(def["uri"], json!(URI));
    assert_eq!(def["range"]["start"], json!({"line": 1, "character": 4}));
    assert_eq!(def["range"]["end"], json!({"line": 1, "character": 10}));
}

#[test]
fn inlay_hints_annotate_parameters() {
    let mut c = Client::start();
    c.set(SRC);
    let hints = c.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": URI},
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 9, "character": 0}}
        }),
    );
    let hints = hints.as_array().unwrap();
    assert!(!hints.is_empty(), "expected a hint for `n`");

    // The label is a *sequence* of parts, not a string: a type name in it has
    // to be able to carry a `location`, which is what an editor turns into a
    // ctrl-click. Flattened, the hints read as the annotation you could have
    // written — `fun double (n : n) : n = n * 2`.
    let flat: Vec<(u64, u64, String)> = hints
        .iter()
        .map(|h| {
            let parts = h["label"].as_array().expect("label parts");
            let text: String = parts
                .iter()
                .map(|p| p["value"].as_str().unwrap_or_default())
                .collect();
            (
                h["position"]["line"].as_u64().unwrap(),
                h["position"]["character"].as_u64().unwrap(),
                text,
            )
        })
        .collect();

    // `fun double n = n * 2` — an open paren before `n`, and the rest after it.
    assert!(flat.contains(&(1, 11, "(".to_string())), "got {flat:?}");
    assert!(
        flat.contains(&(1, 12, " : n) : n".to_string())),
        "the closing paren and the result type share a position: {flat:?}"
    );
}

/// A type name inside a hint carries a `location`, which is what makes it
/// ctrl-clickable — and a builtin, which is declared nowhere, does not.
#[test]
fn a_type_name_in_a_hint_is_a_link() {
    let root = std_sources("hint-link");
    let mut c = Client::start_with(Some(root.clone()));
    c.set("fun pick m = match m with | Just v -> v | None -> toInt 0\n");
    let hints = c.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": URI},
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 9, "character": 0}}
        }),
    );

    let mut linked = Vec::new();
    let mut plain = Vec::new();
    for h in hints.as_array().unwrap() {
        for p in h["label"].as_array().unwrap() {
            let v = p["value"].as_str().unwrap_or_default().to_string();
            if p.get("location").is_some() {
                linked.push(v);
            } else {
                plain.push(v);
            }
        }
    }
    assert!(linked.contains(&"Maybe".to_string()), "linked: {linked:?}");
    assert!(plain.contains(&"Int".to_string()), "plain: {plain:?}");

    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

/// A type a module declares is indexed by its canonical name,
/// `Std.String.Parse.Parser`, and a hint spells it `Parser`: the link is
/// found all the same.
#[test]
fn a_modules_type_in_a_hint_is_a_link() {
    let root = std_sources("hint-link-module");
    let mut c = Client::start_with(Some(root.clone()));
    c.set("use Std.String.Parse as P\n\nfun twice p = P.thenSkip p p\n");
    let hints = c.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": URI},
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 9, "character": 0}}
        }),
    );
    let linked: Vec<String> = hints
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|h| h["label"].as_array().unwrap().clone())
        .filter(|p| p.get("location").is_some())
        .map(|p| p["value"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(linked.contains(&"Parser".to_string()), "linked: {linked:?}");

    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn semantic_tokens_come_back_as_deltas() {
    let mut c = Client::start();
    c.set(SRC);
    let st = c.request(
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": URI}}),
    );
    let data = st["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert_eq!(data.len() % 5, 0, "five numbers per token");
    // The first token is `fun` on line 1 (line 0 is a comment, which the lexer
    // discards), so the first delta-line is 1 and the type is `keyword` (0).
    assert_eq!(data[0], json!(1));
    assert_eq!(data[3], json!(0));
}

#[test]
fn an_unknown_request_is_an_error_not_a_panic() {
    let mut c = Client::start();
    c.set(SRC);
    c.next_id += 1;
    let id = c.next_id;
    c.conn
        .sender
        .send(Message::Request(Request {
            id: id.into(),
            method: "textDocument/nonsense".into(),
            params: json!({}),
        }))
        .unwrap();
    loop {
        if let Message::Response(r) = c.conn.receiver.recv().unwrap() {
            assert!(r.response_result.is_err(), "should report method not found");
            break;
        }
    }
}

// --- the handshake ---------------------------------------------------------

/// Drive the handshake by hand, sending `chatter` between the `initialize`
/// response and `initialized`, and answer whether the server survived.
fn survives_chatter_before_initialized(chatter: &[(&str, Value)]) -> bool {
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || {
        let opts = meadow::Options::debug();
        let (packages, _) = meadow::stdlib::std_packages(opts);
        let modules = meadow::stdlib::std_modules(opts)
            .0
            .into_iter()
            .map(|(dotted, pkg)| (dotted.to_string(), pkg))
            .collect();
        let _ = meadow_lsp::server::serve(
            &server,
            packages,
            modules,
            None,
            meadow::stdlib::MODULES,
            None,
        );
    });

    client
        .sender
        .send(Message::Request(Request {
            id: 1.into(),
            method: "initialize".into(),
            params: json!({"capabilities": {}}),
        }))
        .unwrap();
    // Wait for the initialize response before the chatter, which is the window
    // the strict handshake used to die in.
    loop {
        match client.receiver.recv().unwrap() {
            Message::Response(_) => break,
            _ => continue,
        }
    }
    for (method, params) in chatter {
        client
            .sender
            .send(Message::Notification(Notification {
                method: (*method).into(),
                params: params.clone(),
            }))
            .unwrap();
    }
    client
        .sender
        .send(Message::Notification(Notification {
            method: "initialized".into(),
            params: json!({}),
        }))
        .unwrap();

    // If the server is alive it will answer this; if it died, the channel
    // closes and `recv` fails.
    client
        .sender
        .send(Message::Notification(Notification {
            method: "textDocument/didOpen".into(),
            params: json!({"textDocument": {
                "uri": "file:///x.mw", "languageId": "meadow", "version": 1,
                "text": "def demo = 1\n"
            }}),
        }))
        .unwrap();
    let alive = loop {
        match client.receiver.recv() {
            Ok(Message::Notification(n)) if n.method == "textDocument/publishDiagnostics" => {
                break true;
            }
            Ok(_) => continue,
            Err(_) => break false,
        }
    };

    drop(client);
    let _ = handle.join();
    alive
}

#[test]
fn the_handshake_tolerates_what_vs_code_actually_sends() {
    // `lsp_server::Connection::initialize` insists `initialized` is the very
    // next message and treats anything else as fatal. VS Code sends `$/setTrace`
    // and `workspace/didChangeConfiguration` around it, and the server used to
    // exit during startup — which reached the user as `write EPIPE, shutting
    // down server`, a message about the client's failed write rather than about
    // anything the server did.
    assert!(
        survives_chatter_before_initialized(&[("$/setTrace", json!({"value": "off"}))]),
        "$/setTrace before `initialized` killed the server"
    );
    assert!(
        survives_chatter_before_initialized(&[(
            "workspace/didChangeConfiguration",
            json!({"settings": {}})
        )]),
        "didChangeConfiguration before `initialized` killed the server"
    );
    assert!(
        survives_chatter_before_initialized(&[]),
        "the ordinary handshake should still work"
    );
}

#[test]
fn a_request_before_initialized_is_answered_rather_than_dropped() {
    // Otherwise the client waits on it forever.
    let (server, client) = Connection::memory();
    let handle = std::thread::spawn(move || {
        let opts = meadow::Options::debug();
        let (packages, _) = meadow::stdlib::std_packages(opts);
        let modules = meadow::stdlib::std_modules(opts)
            .0
            .into_iter()
            .map(|(dotted, pkg)| (dotted.to_string(), pkg))
            .collect();
        let _ = meadow_lsp::server::serve(
            &server,
            packages,
            modules,
            None,
            meadow::stdlib::MODULES,
            None,
        );
    });
    client
        .sender
        .send(Message::Request(Request {
            id: 1.into(),
            method: "initialize".into(),
            params: json!({"capabilities": {}}),
        }))
        .unwrap();
    loop {
        if let Message::Response(_) = client.receiver.recv().unwrap() {
            break;
        }
    }
    client
        .sender
        .send(Message::Request(Request {
            id: 2.into(),
            method: "textDocument/hover".into(),
            params: json!({}),
        }))
        .unwrap();
    let answered = match client.receiver.recv() {
        Ok(Message::Response(r)) => r.response_result.is_err(),
        _ => false,
    };
    drop(client);
    let _ = handle.join();
    assert!(
        answered,
        "an early request should get an error, not silence"
    );
}

/// Go-to-definition answers with *another file's* URI.
///
/// The handler used to echo back the URI it was asked about, so the only way to
/// tell a working cross-module jump from a broken one is over the protocol,
/// where the `uri` field is visible. Everything below the protocol could be
/// right and this still wrong.
#[test]
fn definition_crosses_into_the_standard_library() {
    let root = std_sources("definition");
    let mut c = Client::start_with(Some(root.clone()));
    c.set("use Std.String (concatAll)\ndef demo = concatAll\n");
    // Line 1, character 12 is inside `concatAll` in `def demo = concatAll`.
    let def = c.at("textDocument/definition", 1, 12);

    let uri = def["uri"].as_str().expect("a uri");
    assert_ne!(uri, URI, "the definition is not in the open document");
    assert!(
        uri.starts_with("file:///"),
        "expected a file URI, got {uri}"
    );
    assert!(
        uri.ends_with("/Std/String.mw"),
        "expected Std/String.mw, got {uri}"
    );
    // And it points at a real line in that file, not at 0:0.
    assert!(
        def["range"]["start"]["line"].as_u64().unwrap() > 0,
        "got {:?}",
        def["range"]
    );

    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

/// The standard library's sources, written somewhere an editor could open them.
///
/// Deliberately not `stdlib::extract_sources`: that writes to the user's home,
/// and what is under test here is the server, not where the files are kept.
/// `who` keeps concurrent tests out of each other's directory.
fn std_sources(who: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("meadow-lsp-std-{}-{who}", std::process::id()));
    for (dotted, src) in meadow::stdlib::MODULES {
        let path = root.join(format!("Std/{}.mw", dotted.replace('.', "/")));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, src).unwrap();
    }
    root
}

#[test]
fn rename_is_offered_and_answers_with_an_edit_per_occurrence() {
    let mut c = Client::start();
    let caps = c.capabilities.clone().unwrap();
    assert!(
        caps["capabilities"]["renameProvider"]["prepareProvider"] == json!(true),
        "rename is not advertised: {caps}"
    );
    c.set(SRC);

    // The box the editor opens is over the identifier itself.
    let prepared = c.at("textDocument/prepareRename", 1, 5);
    assert_eq!(prepared["start"], json!({"line": 1, "character": 4}));
    assert_eq!(prepared["end"], json!({"line": 1, "character": 10}));

    let edit = c.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 1, "character": 5},
            "newName": "twice"
        }),
    );
    let edits = edit["changes"][URI]
        .as_array()
        .expect("edits for this file");
    // The declaration and the one call.
    assert_eq!(edits.len(), 2, "{edit}");
    for e in edits {
        assert_eq!(e["newText"], json!("twice"));
    }
}

#[test]
fn rename_refuses_a_name_from_the_standard_library() {
    let mut c = Client::start();
    c.set("def demo = println \"hi\"\n");
    // `println` is `Std`'s, and this server is not editing `Std`.
    let prepared = c.at("textDocument/prepareRename", 0, 12);
    assert_eq!(prepared, Value::Null, "offered a rename it cannot do");
}

/// A `@test` gets a "Test" lens, before its "Debug" one, naming the test the
/// way `meadow test --exact` takes it. A plain function gets only "Debug".
#[test]
fn a_test_gets_a_test_lens_before_its_debug_lens() {
    let mut c = Client::start();
    c.set(
        "use Std.Test (assertEq)\n\
         \n\
         fun helper x = x\n\
         \n\
         @test fun checks u = assertEq (helper 1) 1 \"x\"\n",
    );
    let lenses = c.request(
        "textDocument/codeLens",
        json!({"textDocument": {"uri": URI}}),
    );
    let lenses = lenses.as_array().expect("lenses");

    // (line, title, command) in the order the server gave them.
    let shape: Vec<(u64, String, String)> = lenses
        .iter()
        .map(|l| {
            (
                l["range"]["start"]["line"].as_u64().unwrap(),
                l["command"]["title"].as_str().unwrap().to_string(),
                l["command"]["command"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (2, "▶ Debug".to_string(), "meadow.debugFunction".to_string()),
            (4, "▶ Test".to_string(), "meadow.testFunction".to_string()),
            (4, "▶ Debug".to_string(), "meadow.debugFunction".to_string()),
        ]
    );

    let test = &lenses[1]["command"]["arguments"][0];
    assert_eq!(test["test"], json!("checks"), "{test}");
    assert_eq!(test["name"], json!("checks"));
    assert_eq!(test["uri"], json!(URI));
}

/// What an editor actually does after `|>`: ask, then ask again with each
/// letter typed. The list has to narrow rather than empty.
#[test]
fn completion_after_a_pipe_narrows_as_the_name_is_typed() {
    let mut c = Client::start();
    let ask = |c: &mut Client, text: &str| -> Value {
        c.set(text);
        let last = text.lines().count() - 1;
        let character = text.lines().next_back().unwrap().chars().count();
        c.request(
            "textDocument/completion",
            json!({
                "textDocument": {"uri": URI},
                "position": {"line": last, "character": character},
            }),
        )
    };
    let names = |v: &Value| -> Vec<String> {
        let items = v["items"].as_array().cloned().unwrap_or_default();
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    let empty = ask(&mut c, "def xs = [11..20]\n\ndef demo = xs |> ");
    let all = names(&empty);
    assert!(!all.is_empty(), "a pipe offers something");
    assert!(all.contains(&"foldl".to_string()), "{all:?}");

    // Each keystroke: the editor asks again, because the list said it was
    // incomplete, and what comes back still has the name in it.
    for typed in ["f", "fo", "fol", "fold", "foldl"] {
        let got = ask(
            &mut c,
            &format!("def xs = [11..20]\n\ndef demo = xs |> {typed}"),
        );
        let names = names(&got);
        assert!(
            names.contains(&"foldl".to_string()),
            "after typing {typed:?}: {names:?}"
        );
    }
}

/// Completing just after a character wider than one byte -- an arrow in a
/// comment, a no-break space, an emoji -- once panicked, and took the server
/// with it. It answers, and goes on answering.
#[test]
fn completion_after_a_wide_character_answers() {
    let mut c = Client::start();
    for line in [
        "def demo = 1 -- \u{2192}",
        "def demo =\u{a0}",
        "def demo = 1 -- \u{1f600} ",
        "def caf\u{e9} = 1\ndef demo = caf\u{e9}",
    ] {
        c.set(line);
        let last = line.lines().count() - 1;
        let tail = line.lines().next_back().unwrap();
        // The editor counts UTF-16 units.
        let character = tail.encode_utf16().count();
        let got = c.request(
            "textDocument/completion",
            json!({
                "textDocument": {"uri": URI},
                "position": {"line": last, "character": character},
            }),
        );
        assert!(
            got.is_object() || got.is_null() || got.is_array(),
            "{line:?}: {got}"
        );
    }
    // Still there: an ordinary question gets an ordinary answer.
    c.set("def xs = [11..20]\n\ndef demo = xs |> fol");
    let got = c.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 2, "character": 20},
        }),
    );
    let items = got["items"].as_array().expect("the server still answers");
    assert!(items.iter().any(|i| i["label"] == json!("foldl")), "{got}");
}

/// The edit an offer carries replaces the word being typed, and nothing else.
#[test]
fn an_offer_replaces_the_half_written_name() {
    let mut c = Client::start();
    let text = "def xs = [11..20]\n\ndef demo = xs |> fol";
    c.set(text);
    let got = c.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 2, "character": 20},
        }),
    );
    let items = got["items"].as_array().expect("items");
    let foldl = items
        .iter()
        .find(|i| i["label"] == json!("foldl"))
        .unwrap_or_else(|| panic!("no foldl among {:?}", items.len()));
    let range = &foldl["textEdit"]["range"];
    assert_eq!(
        range["start"],
        json!({"line": 2, "character": 17}),
        "{foldl}"
    );
    assert_eq!(range["end"], json!({"line": 2, "character": 20}), "{foldl}");
    assert_eq!(foldl["filterText"], json!("foldl"));
    assert_eq!(
        got["isIncomplete"],
        json!(true),
        "the editor must ask again"
    );
}

/// A path completes like a name: the list is what sits under what was written,
/// and taking an offer replaces only the segment being typed.
#[test]
fn completion_after_a_dot_offers_what_is_under_the_path() {
    let mut c = Client::start();
    c.set("data Shape = Circle Int | Square Int\n\ndef demo = Shape.Ci");
    let got = c.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 2, "character": 19},
        }),
    );
    let items = got["items"].as_array().expect("items");
    let labels: Vec<&str> = items
        .iter()
        .map(|i| i["label"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(labels, vec!["Circle"], "{labels:?}");
    let circle = &items[0];
    // The dot stays: only `Ci` is replaced.
    assert_eq!(
        circle["textEdit"]["range"]["start"],
        json!({"line": 2, "character": 17}),
        "{circle}"
    );
    assert_eq!(
        circle["textEdit"]["range"]["end"],
        json!({"line": 2, "character": 19}),
        "{circle}"
    );
}

#[test]
fn completion_in_a_use_offers_the_modules_under_the_path() {
    let mut c = Client::start();
    c.set("use Std.\n\ndef demo = 1");
    let got = c.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 0, "character": 8},
        }),
    );
    let labels: Vec<String> = got["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["label"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(labels.contains(&"Collections".to_string()), "{labels:?}");
    assert!(labels.contains(&"Maybe".to_string()), "{labels:?}");
}

/// `workspace/symbol`: the editor's own picker over every declaration there
/// is -- what the REPL's `Ctrl-F` searches, found the same way.
#[test]
fn workspace_symbols_are_advertised() {
    let c = Client::start();
    let caps = c.capabilities.as_ref().unwrap();
    assert_eq!(caps["capabilities"]["workspaceSymbolProvider"], json!(true));
}

fn symbols(c: &mut Client, query: &str) -> Vec<Value> {
    c.request("workspace/symbol", json!({ "query": query }))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_workspace_symbol_in_the_open_document_points_into_it() {
    let mut c = Client::start();
    c.set("-- Twice as much.\nfun doubleIt : Int -> Int\n  | doubleIt n = n * 2\n\nfun helper x = x\n");
    let found = symbols(&mut c, "doubleIt");
    let first = found.first().expect("doubleIt is found");
    assert_eq!(first["name"], json!("doubleIt : Int -> Int"), "{first}");
    assert_eq!(first["location"]["uri"], json!(URI));
    assert_eq!(first["kind"], json!(12), "a function"); // SymbolKind::FUNCTION
    // A private one is the author's to find too.
    let helper = symbols(&mut c, "helper");
    assert!(
        helper
            .iter()
            .any(|s| s["name"].as_str().unwrap().starts_with("helper :")),
        "{helper:?}"
    );
}

#[test]
fn a_workspace_symbol_from_the_library_points_at_its_file() {
    let root = std_sources("symbols");
    let mut c = Client::start_with(Some(root.clone()));
    c.set("def demo = 1\n");
    let found = symbols(&mut c, "Path.components");
    let s = found.first().expect("Std.Path.components");
    assert_eq!(s["name"], json!("components : String -> [String]"), "{s}");
    assert_eq!(s["containerName"], json!("Std.Path"));
    let uri = s["location"]["uri"].as_str().unwrap();
    assert!(uri.ends_with("/Std/Path.mw"), "{uri}");
    assert!(s["location"]["range"]["start"]["line"].as_u64().unwrap() > 0);
    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_workspace_symbol_query_that_is_a_type_searches_by_type() {
    let root = std_sources("typed-symbols");
    let mut c = Client::start_with(Some(root.clone()));
    c.set("def demo = 1\n");
    let found = symbols(&mut c, "String -> [String]");
    let names: Vec<&str> = found
        .iter()
        .take(5)
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("lines :") || n.starts_with("components :")),
        "{names:?}"
    );
    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_definition_is_answered_in_an_ir_with_where_the_name_at_the_cursor_is() {
    let mut c = Client::start();
    c.set("fun twice (n : Int) : Int =\n  let m = n + n in\n  m\n");
    // On `m`, where it is answered.
    let ask = |c: &mut Client, ir: &str| {
        c.request(
            "meadow/ir",
            json!({
                "textDocument": {"uri": URI},
                "position": {"line": 2, "character": 2},
                "ir": ir,
            }),
        )
    };
    let cut = ask(&mut c, "cut");
    assert_eq!(cut["ir"], "cut");
    assert_eq!(cut["name"], "twice");
    let text = cut["text"].as_str().unwrap();
    assert!(text.starts_with("val meadow:"), "{text}");
    // Where it is bound and where it is read, each a range of the text that
    // a segment says is a variable.
    let focus = cut["focus"].as_array().unwrap();
    assert_eq!(focus.len(), 2, "{cut}");
    let segments = cut["segments"].as_array().unwrap();
    for f in focus {
        assert!(
            segments
                .iter()
                .any(|g| g["range"] == *f && g["kind"] == "variable"),
            "{f}"
        );
    }
    assert!(segments.iter().any(|g| g["kind"] == "declaration"));
    // The other two, and one there is not.
    assert!(ask(&mut c, "core")["text"].as_str().unwrap().contains("v0"));
    assert!(
        ask(&mut c, "axcut")["text"]
            .as_str()
            .unwrap()
            .contains("def #")
    );
    assert_eq!(ask(&mut c, "llvm"), Value::Null);
}
