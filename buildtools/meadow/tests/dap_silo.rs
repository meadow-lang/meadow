//! The debugger on Silo's interpreter: the same programs and the same
//! questions as `tests/dap.rs` asks of Glade's session, answered the same way
//! by a VM in a process of its own.
//!
//! These need Silo's interpreter built (`meadow-silo-vm`, which
//! `meadow::aot::interpreter` finds); without it they say so and pass.

use meadow::dap::session::{Mode, Stop, Variable};
use meadow::dap::silo::SiloSession;
use std::path::PathBuf;

/// A program in a directory of its own, and a session over it -- or `None`
/// when there is no interpreter to run it on.
fn launch(who: &str, src: &str) -> Option<(SiloSession, PathBuf)> {
    if let Err(e) = meadow::aot::interpreter() {
        eprintln!("skipped: {e}");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("meadow-dap-silo-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.mw");
    std::fs::write(&file, src).unwrap();
    let s = SiloSession::launch_at(&file, None).unwrap_or_else(|e| panic!("launch: {e}"));
    Some((s, file))
}

fn go(s: &mut SiloSession, mode: Mode) -> Stop {
    s.resume(mode);
    loop {
        if let Some(stop) = s.run(10_000) {
            return stop;
        }
    }
}

fn stack(s: &mut SiloSession) -> Vec<(String, u32)> {
    let frames = s.frames(50);
    frames
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

fn locals(s: &mut SiloSession, frame: usize) -> Vec<Variable> {
    let scope = s.scopes(frame)[0].reference;
    s.variables(scope)
}

fn local(s: &mut SiloSession, frame: usize, name: &str) -> Variable {
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
fn it_runs_to_the_end_with_nothing_set() {
    let Some((mut s, _)) = launch("end", PROGRAM) else {
        return;
    };
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("()".to_string()))
    );
    assert_eq!(s.take_output(), "48\n");
}

#[test]
fn a_breakpoint_stops_with_the_caller_underneath() {
    let Some((mut s, file)) = launch("bp", PROGRAM) else {
        return;
    };
    assert_eq!(s.set_breakpoints(&file, &[2]), vec![Some(2)]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    let st = stack(&mut s);
    assert_eq!(st[0], ("double".to_string(), 2), "{st:?}");
    assert_eq!(
        st[1].0, "main",
        "the caller, waiting for the answer: {st:?}"
    );
    assert_eq!(st[1].1, 10, "it resumes at the call: {st:?}");
    let n = local(&mut s, 0, "n");
    assert_eq!(n.value, "21");
}

#[test]
fn a_recursive_function_has_a_frame_per_call() {
    let Some((mut s, file)) = launch("rec", PROGRAM) else {
        return;
    };
    s.set_breakpoints(&file, &[6]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    let names: Vec<String> = stack(&mut s).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["sum", "sum", "sum", "sum", "main"], "{names:?}");
    assert_eq!(local(&mut s, 1, "x").value, "3");
    assert_eq!(local(&mut s, 3, "x").value, "1");
}

#[test]
fn a_line_with_no_code_moves_to_the_next_one_that_has_some() {
    let Some((mut s, file)) = launch("move", PROGRAM) else {
        return;
    };
    let landed = s.set_breakpoints(&file, &[3])[0];
    assert!(matches!(landed, Some(l) if l > 3), "{landed:?}");
}

#[test]
fn stepping_over_a_call_stays_in_the_function_and_into_it_goes_inside() {
    let Some((mut s, file)) = launch("step", PROGRAM) else {
        return;
    };
    s.set_breakpoints(&file, &[10]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    s.set_breakpoints(&file, &[]);
    assert_eq!(go(&mut s, Mode::StepIn), Stop::Step);
    assert_eq!(stack(&mut s)[0].0, "double", "into the call");
    assert_eq!(go(&mut s, Mode::StepOut), Stop::Step);
    assert_eq!(stack(&mut s)[0].0, "main", "out again");
    let before = stack(&mut s)[0].1;
    assert_eq!(go(&mut s, Mode::StepOver), Stop::Step);
    let after = stack(&mut s);
    assert_eq!(after[0].0, "main", "over `sum`, still in main: {after:?}");
    assert!(after[0].1 > before, "on a later line: {after:?}");
}

#[test]
fn a_value_opens_into_its_parts() {
    let Some((mut s, file)) = launch("parts", PROGRAM) else {
        return;
    };
    s.set_breakpoints(&file, &[5]);
    assert_eq!(go(&mut s, Mode::Continue), Stop::Breakpoint);
    let xs = local(&mut s, 0, "xs");
    assert_eq!(xs.value, "[1; 2; 3]");
    assert!(xs.children != 0, "a list has parts");
    let parts = s.variables(xs.children);
    assert!(!parts.is_empty(), "{parts:?}");
    assert_eq!(
        s.evaluate(0, "xs").map(|v| v.value),
        Some("[1; 2; 3]".to_string())
    );
}

#[test]
fn a_failure_stops_where_it_happened() {
    let src = "\
fun boom (n : Int) : Int =
  n / 0

fun main () =
  let x = boom 7 in
  println x
";
    let Some((mut s, _)) = launch("fail", src) else {
        return;
    };
    match go(&mut s, Mode::Continue) {
        Stop::Exception(m) => assert!(m.contains("zero"), "{m}"),
        other => panic!("{other:?}"),
    }
    let st = stack(&mut s);
    assert_eq!(st[0].0, "boom", "{st:?}");
    assert_eq!(local(&mut s, 0, "n").value, "7");
    assert!(s.is_finished(), "it cannot be resumed");
}

#[test]
fn stop_on_entry_stops_before_anything_runs() {
    let Some((mut s, _)) = launch("entry", PROGRAM) else {
        return;
    };
    assert_eq!(s.stop_at_entry(), Some(Stop::Entry));
    assert_eq!(s.take_output(), "", "nothing printed yet");
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("()".to_string()))
    );
}

#[test]
fn a_deep_recursion_is_followed_on_a_growing_stack() {
    let src = "\
fun down (n : Int) : Int =
  if n == 0 then 0 else 1 + down (n - 1)

fun main () = println (down 2000000)
";
    let Some((mut s, _)) = launch("deep", src) else {
        return;
    };
    assert_eq!(
        go(&mut s, Mode::Continue),
        Stop::Exited(Ok("()".to_string()))
    );
    assert_eq!(s.take_output(), "2000000\n");
}

// --- the protocol ----------------------------------------------------------------

/// The same session as `tests/dap.rs` has, through the adapter, launched on
/// Silo: the editor sees no difference.
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
    fn a_session_from_launch_to_exit_on_silo() {
        if meadow::aot::interpreter().is_err() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("meadow-dap-silo-{}-protocol", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.mw");
        std::fs::write(&file, PROGRAM).unwrap();
        let path = file.display().to_string();

        let sink = Sink::default();
        let mut a = Adapter::new(Box::new(sink.clone()));
        a.handle(request(1, "initialize", json!({ "adapterID": "meadow" })));
        assert_eq!(response(&sink.take(), "initialize")["success"], true);
        a.handle(request(
            2,
            "launch",
            json!({ "program": path, "runtime": "silo" }),
        ));
        let msgs = sink.take();
        assert_eq!(response(&msgs, "launch")["success"], true, "{msgs:?}");
        a.handle(request(
            3,
            "setBreakpoints",
            json!({ "source": { "path": path }, "breakpoints": [{ "line": 2 }] }),
        ));
        let msgs = sink.take();
        assert_eq!(
            response(&msgs, "setBreakpoints")["body"]["breakpoints"][0]["verified"],
            true
        );
        a.handle(request(4, "configurationDone", json!({})));
        a.settle();
        let msgs = sink.take();
        assert_eq!(
            event(&msgs, "stopped").expect("stopped")["body"]["reason"],
            "breakpoint"
        );

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
    fn a_pause_stops_a_program_that_runs_on() {
        if meadow::aot::interpreter().is_err() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("meadow-dap-silo-{}-pause", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.mw");
        std::fs::write(
            &file,
            "fun spin (n : Int) : Int = if n < 0 then n else spin (n + 1)\n\nfun main () = println (spin 0)\n",
        )
        .unwrap();
        let path = file.display().to_string();
        let sink = Sink::default();
        let mut a = Adapter::new(Box::new(sink.clone()));
        a.handle(request(1, "initialize", json!({})));
        a.handle(request(
            2,
            "launch",
            json!({ "program": path, "runtime": "silo" }),
        ));
        a.handle(request(3, "configurationDone", json!({})));
        sink.take();
        // A slice or two of running, and then the pause.
        a.handle(request(4, "threads", json!({})));
        std::thread::sleep(std::time::Duration::from_millis(200));
        a.handle(request(5, "pause", json!({ "threadId": 1 })));
        let msgs = sink.take();
        assert_eq!(
            event(&msgs, "stopped").expect("stopped")["body"]["reason"],
            "pause"
        );
        a.handle(request(6, "stackTrace", json!({ "threadId": 1 })));
        let msgs = sink.take();
        assert_eq!(
            response(&msgs, "stackTrace")["body"]["stackFrames"][0]["name"],
            "spin"
        );
        a.handle(request(7, "terminate", json!({})));
    }
}
