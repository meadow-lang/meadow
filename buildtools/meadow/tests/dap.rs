//! The debugger: stepping, packages and entries, and speaking the protocol an
//! editor drives it with. What a stop shows -- breakpoints, frames, values --
//! is `tests/dap_silo.rs`'s.

use meadow::dap::session::{Mode, Stop, Variable};
use meadow::dap::silo::SiloSession as Session;
use std::path::{Path, PathBuf};

/// A program in a directory of its own, and a session over it.
fn launch(who: &str, src: &str) -> (Session, PathBuf) {
    let dir = std::env::temp_dir().join(format!("meadow-dap-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.mw");
    std::fs::write(&file, src).unwrap();
    let s = Session::launch_at(&file, None).unwrap_or_else(|e| panic!("launch: {e}"));
    (s, file)
}

/// Run until something other than a slice boundary stops it.
fn go(s: &mut Session, mode: Mode) -> Stop {
    s.resume(mode);
    loop {
        if let Some(stop) = s.run(10_000) {
            return stop;
        }
    }
}

/// `(function, line)` of each frame, innermost first.
fn stack(s: &mut Session) -> Vec<(String, u32)> {
    s.frames(50)
        .into_iter()
        .map(|f| {
            let line = f
                .loc
                .and_then(|l| s.file(l).map(|file| file.position(l.span.start).0))
                .unwrap_or(0);
            (f.name, line)
        })
        .collect()
}

fn locals(s: &mut Session, frame: usize) -> Vec<Variable> {
    let scope = s.scopes(frame)[0].reference;
    s.variables(scope)
}

fn local(s: &mut Session, frame: usize, name: &str) -> Variable {
    locals(s, frame)
        .into_iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no `{name}` in frame {frame}"))
}

const PROGRAM: &str = "\
fun double n =
  n * 2

fun sum xs =
  match xs with
  | [;] -> 0
  | x :: rest -> x + sum rest

fun main () =
  let a = double 21 in
  let b = sum [1; 2; 3] in
  let u = println (a + b) in
  ()
";

#[test]
fn stepping_in_enters_the_call_and_out_returns_from_it() {
    let (mut s, file) = launch("step", PROGRAM);
    s.set_breakpoints(&file, &[10]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    assert_eq!(stack(&mut s)[0].0, "main");

    assert_eq!(go(&mut s, Mode::StepIn), Stop::Step);
    let inside = stack(&mut s);
    assert_eq!(inside[0].0, "double", "{inside:?}");

    assert_eq!(go(&mut s, Mode::StepOut), Stop::Step);
    let back = stack(&mut s);
    assert_eq!(back[0].0, "main", "{back:?}");
    assert_eq!(back.len(), 1, "nothing left above main: {back:?}");
}

#[test]
fn stepping_over_a_call_stays_in_the_function() {
    let (mut s, file) = launch("over", PROGRAM);
    s.set_breakpoints(&file, &[10]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    for _ in 0..4 {
        match go(&mut s, Mode::StepOver) {
            Stop::Step => assert_eq!(stack(&mut s)[0].0, "main", "{:?}", stack(&mut s)),
            Stop::Exited(r) => {
                assert_eq!(r, Ok("()".to_string()));
                return;
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn output_is_captured_rather_than_printed() {
    let (mut s, _) = launch("out", "fun main () =\n  println \"hello\"\n");
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("()".to_string()))
    );
    assert_eq!(s.take_output(), "hello\n");
}

// --- the protocol ----------------------------------------------------------------

mod protocol {
    use super::*;
    use meadow::dap::Adapter;
    use serde_json::{Value as Json, json};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        /// Every message written so far, and forget them.
        fn take(&self) -> Vec<Json> {
            let bytes = std::mem::take(&mut *self.0.borrow_mut());
            let mut cursor = std::io::Cursor::new(bytes);
            let mut out = Vec::new();
            while let Some(m) = meadow::dap::read_message(&mut cursor) {
                out.push(m);
            }
            out
        }
    }

    fn request(seq: i64, command: &str, arguments: Json) -> Json {
        json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })
    }

    fn event<'a>(msgs: &'a [Json], name: &str) -> Option<&'a Json> {
        msgs.iter()
            .find(|m| m["type"] == "event" && m["event"] == name)
    }

    fn response<'a>(msgs: &'a [Json], command: &str) -> &'a Json {
        msgs.iter()
            .find(|m| m["type"] == "response" && m["command"] == command)
            .unwrap_or_else(|| panic!("no response to {command}: {msgs:?}"))
    }

    #[test]
    fn a_session_from_launch_to_exit() {
        let dir = std::env::temp_dir().join(format!("meadow-dap-{}-protocol", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.mw");
        std::fs::write(&file, PROGRAM).unwrap();
        let path = file.display().to_string();

        let sink = Sink::default();
        let mut a = Adapter::new(Box::new(sink.clone()));

        a.handle(request(1, "initialize", json!({ "adapterID": "meadow" })));
        assert_eq!(response(&sink.take(), "initialize")["success"], true);

        a.handle(request(2, "launch", json!({ "program": path })));
        let msgs = sink.take();
        assert_eq!(response(&msgs, "launch")["success"], true, "{msgs:?}");
        assert!(event(&msgs, "initialized").is_some());

        a.handle(request(
            3,
            "setBreakpoints",
            json!({
                "source": { "path": path },
                "breakpoints": [{ "line": 2 }],
            }),
        ));
        let msgs = sink.take();
        assert_eq!(
            response(&msgs, "setBreakpoints")["body"]["breakpoints"][0]["verified"],
            true
        );

        a.handle(request(4, "configurationDone", json!({})));
        a.settle();
        let msgs = sink.take();
        let stopped = event(&msgs, "stopped").expect("stopped");
        assert_eq!(stopped["body"]["reason"], "breakpoint");

        a.handle(request(5, "stackTrace", json!({ "threadId": 1 })));
        let msgs = sink.take();
        let frames = &response(&msgs, "stackTrace")["body"]["stackFrames"];
        assert_eq!(frames[0]["name"], "double");
        assert_eq!(frames[0]["line"], 2);
        assert!(
            frames[0]["source"]["path"]
                .as_str()
                .unwrap()
                .ends_with("main.mw")
        );

        a.handle(request(6, "scopes", json!({ "frameId": 0 })));
        let msgs = sink.take();
        let locals = response(&msgs, "scopes")["body"]["scopes"][0]["variablesReference"].clone();
        a.handle(request(
            7,
            "variables",
            json!({ "variablesReference": locals }),
        ));
        let msgs = sink.take();
        let vars = &response(&msgs, "variables")["body"]["variables"];
        assert_eq!(vars[0]["name"], "n");
        assert_eq!(vars[0]["value"], "21");

        a.handle(request(8, "continue", json!({ "threadId": 1 })));
        a.settle();
        let msgs = sink.take();
        let output: String = msgs
            .iter()
            .filter(|m| m["event"] == "output")
            .map(|m| m["body"]["output"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(output.contains("48"), "{output}");
        assert!(event(&msgs, "terminated").is_some());
    }

    #[test]
    fn a_program_that_does_not_compile_fails_to_launch_and_says_why() {
        let dir = std::env::temp_dir().join(format!("meadow-dap-{}-broken", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.mw");
        std::fs::write(&file, "fun main () = nope\n").unwrap();

        let sink = Sink::default();
        let mut a = Adapter::new(Box::new(sink.clone()));
        a.handle(request(
            1,
            "launch",
            json!({ "program": file.display().to_string() }),
        ));
        let msgs = sink.take();
        let r = response(&msgs, "launch");
        assert_eq!(r["success"], false);
        assert!(
            r["message"]
                .as_str()
                .unwrap()
                .contains("undefined variable"),
            "{r}"
        );
    }

    #[allow(dead_code)]
    fn unused(_: &Path) {}
}

#[test]
fn a_call_inside_a_call_is_still_one_frame() {
    // `main` waits for `inc` and then for `double`: two continuations, one call.
    let src =
        "fun inc n =\n  n + 1\n\nfun double n = n * 2\n\nfun main () = println (double (inc 20))\n";
    let (mut s, file) = launch("nested", src);
    s.set_breakpoints(&file, &[2]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    let names: Vec<String> = stack(&mut s).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["inc", "main"]);
}

#[test]
fn stepping_in_can_reach_the_standard_library() {
    let src = "use Std.Collections.List as L\n\nfun main () =\n  println (L.length [1; 2; 3])\n";
    let (mut s, file) = launch("std", src);
    s.set_breakpoints(&file, &[4]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    assert_eq!(go(&mut s, Mode::StepIn), Stop::Step);
    let top = s.frames(1).remove(0);
    assert_eq!(top.name, "length");
    let file = top.loc.and_then(|l| s.file(l)).expect("a source for Std");
    assert!(file.name.ends_with("Collections/List.mw"), "{}", file.name);
}

/// mini-ml, with `main` swapped for something that evaluates: the example
/// itself reads its console, which a debugger answers with end of input.
#[test]
fn a_package_of_several_modules_debugs_across_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/MiniML");
    let dir = std::env::temp_dir().join(format!("meadow-dap-{}-miniml", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::copy(root.join("Meadow.toml"), dir.join("Meadow.toml")).unwrap();
    for f in ["Syntax.mw", "Parser.mw", "Infer.mw", "Eval.mw", "Main.mw"] {
        let mut text = std::fs::read_to_string(root.join("src").join(f)).unwrap();
        if f == "Main.mw" {
            let at = text.find("fun main () ").expect("main");
            let end = text[at..].find("\n\n").map_or(text.len(), |e| at + e);
            text.replace_range(
                at..end,
                "fun main () = println \"${runSource \"(fun x -> x + 1) 41\":?}\"",
            );
        }
        std::fs::write(dir.join("src").join(f), text).unwrap();
    }
    let eval = dir.join("src").join("Eval.mw");
    let line = std::fs::read_to_string(&eval)
        .unwrap()
        .lines()
        .position(|l| l.contains("Expr.Lam param body ->"))
        .expect("the lambda arm") as u32
        + 1;

    let mut s = Session::launch_at(&dir, None).unwrap_or_else(|e| panic!("launch: {e}"));
    assert_eq!(s.set_breakpoints(&eval, &[line]), vec![Some(line)]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    let st = stack(&mut s);
    assert_eq!(st[0].0, "eval", "{st:?}");
    // `main` calls `runSource`, which calls `run`, which calls `toResult`, each
    // in tail position: each call replaced its caller's frame, and under them
    // is `toResult`'s `handle`, still waiting, and the handler's frames.
    let names: Vec<&str> = st.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names[..4], ["eval", "eval", "run", "toResult"], "{st:?}");
    // What the arm bound, from a module of its own: `e` itself is taken
    // apart by the match, and gone.
    let param = local(&mut s, 0, "param");
    assert_eq!(param.value, "\"x\"", "{param:?}");
    assert_eq!(param.ty.as_deref(), Some("String"));

    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("()".to_string()))
    );
    assert_eq!(s.take_output(), "Ok(\"42\")\n");
}

// --- starting at a function --------------------------------------------------------

/// A package on disk: `(relative path, text)` pairs, and its root.
fn package(who: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-dap-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        format!("[package]\nname = \"{who}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

fn entry(module: &Path, expression: &str) -> meadow::dap::session::Entry {
    meadow::dap::session::Entry {
        module: module.to_path_buf(),
        expression: expression.to_string(),
    }
}

#[test]
fn a_private_function_in_a_module_can_be_the_entry() {
    let dir = package(
        "Entry",
        &[
            (
                "src/Main.mw",
                "use Maths (twice)\n\nfun main () = println (twice 1)\n",
            ),
            (
                "src/Maths.mw",
                "fun secret n =\n  n * 10\n\n@pub(pkg) fun twice n = secret n + secret n\n",
            ),
        ],
    );
    let maths = dir.join("src/Maths.mw");
    let mut s = Session::launch_at(&dir, Some(&entry(&maths, "secret 4"))).unwrap();
    assert!(s.resume_until_in("secret", &maths));
    let stop = loop {
        if let Some(stop) = s.run(10_000) {
            break stop;
        }
    };
    assert_eq!(stop, Stop::Entry);
    let st = stack(&mut s);
    assert_eq!(st[0], ("secret".to_string(), 2), "{st:?}");
    // The entry is a call in tail position, so `secret` is the whole stack.
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!(local(&mut s, 0, "n").value, "4");
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("40".to_string()))
    );
}

#[test]
fn a_package_with_no_main_can_still_be_debugged_at_a_function() {
    let dir = package("NoMain", &[("src/Main.mw", "fun square n = n * n\n")]);
    let main = dir.join("src/Main.mw");
    assert!(
        Session::launch_at(&dir, None).is_err(),
        "nothing to run without an entry"
    );
    let mut s = Session::launch_at(&dir, Some(&entry(&main, "square 9"))).unwrap();
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("81".to_string()))
    );
}

#[test]
fn a_bad_argument_is_reported_as_the_arguments_fault() {
    let dir = package("BadArg", &[("src/Main.mw", "fun square n = n * n\n")]);
    let main = dir.join("src/Main.mw");
    let err = Session::launch_at(&dir, Some(&entry(&main, "square \"nine\"")))
        .err()
        .expect("a type error");
    assert!(err.contains("in `square \"nine\"`"), "{err}");
    assert!(err.contains("does not implement `Mul`"), "{err}");
}

#[test]
fn mini_ml_eval_can_be_debugged_without_touching_main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/MiniML");
    let eval = root.join("src/Eval.mw");
    let expr = "eval [;] (Expr.App (Expr.Lam \"x\" (Expr.Var \"x\")) (Expr.Int 7))";
    let mut s = Session::launch_at(&root, Some(&entry(&eval, expr))).unwrap();
    assert!(s.resume_until_in("eval", &eval));
    let stop = loop {
        if let Some(stop) = s.run(10_000) {
            break stop;
        }
    };
    assert_eq!(stop, Stop::Entry);
    assert!(
        local(&mut s, 0, "e").value.starts_with("App(Lam"),
        "{:?}",
        locals(&mut s, 0)
    );
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("Int(7)".to_string()))
    );
}
