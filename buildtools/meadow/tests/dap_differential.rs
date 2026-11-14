//! MeadowBoot's debug adapter -- `bootstrap/src/Dap.mw`, declared with
//! Lingua's `dap!` -- against the Rust compiler's, `meadow dap`
//! (`buildtools/meadow/src/dap/`).
//!
//! Both are started over standard input and output and driven through the
//! same sessions, program by program. Each program is debugged twice:
//!
//! - **stepping**: launched to stop on entry, then stepped in, over and out
//!   in a fixed pattern until it ends or forty stops have been made;
//! - **breakpoints**: a breakpoint asked for on every line of every file of
//!   the program, then continued from one to the next until it ends or forty
//!   stops have been made.
//!
//! At each stop both are asked the same: the stack, each of the first three
//! frames' scopes and its locals, and every local of the innermost frame --
//! and a name that is none -- evaluated there.
//!
//! The answers are compared after only what the two may say differently by
//! right is made the same: a request's number, a path's spelling (a frame's
//! source is compared by its file's name), a refusal's wording (a refusal is
//! compared as one), the numbers variables references are, and the text of
//! an exception (its stop is compared by its reason). A frame's column is
//! not compared: the Rust adapter places a pending frame at the expression
//! its continuation was made in, MeadowBoot's at the call itself, which are
//! on the same line.
//!
//! Agreement is reported by kind of request. The test passes only when
//! every answer agrees, and it does not yet: see the numbers on
//! [`meadowboot_s_debugger_answers_as_meadow_dap_answers`].
//!
//! `MEADOWBOOT_EXE` runs a MeadowBoot already built rather than building
//! one; `MEADOWBOOT_PATIENCE` is how many seconds an answer may take (600);
//! `MEADOW_DAP_PROGRAMS` (comma-separated) runs only the programs named.
//! What each side answered, where they differ, is written to
//! `dap-differences.txt` in `CARGO_TARGET_TMPDIR`, or `MEADOW_DAP_OUT`.

use serde_json::{Value, json};
use std::collections::BTreeMap;
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
/// `MEADOW_DAP_RUST` names another.
fn meadow() -> PathBuf {
    std::env::var_os("MEADOW_DAP_RUST")
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

/// Where what the test writes goes: `MEADOW_DAP_OUT`, or the test's own
/// scratch directory.
fn out_dir() -> PathBuf {
    std::env::var_os("MEADOW_DAP_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
}

/// How long an answer may take, in seconds: a launch builds the program, and
/// MeadowBoot's makes the core of everything it depends on.
fn patience() -> u64 {
    std::env::var("MEADOWBOOT_PATIENCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
}

// --- a client ----------------------------------------------------------------

/// A debug adapter, started: what it has sent that answered nothing yet --
/// its events -- and what its program wrote.
struct Adapter {
    child: Child,
    stdin: ChildStdin,
    inbox: Receiver<Value>,
    seq: i64,
    events: Vec<Value>,
    output: String,
    /// Whether it has stopped answering: died, or took longer than anyone
    /// would wait.
    gone: bool,
}

impl Adapter {
    fn start(mut c: Command) -> Adapter {
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().expect("the adapter starts");
        let stdin = child.stdin.take().expect("its input");
        let stdout = child.stdout.take().expect("its output");
        let (tx, inbox) = channel();
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
                if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                    if tx.send(v).is_err() {
                        return;
                    }
                }
            }
        });
        Adapter {
            child,
            stdin,
            inbox,
            seq: 0,
            events: Vec::new(),
            output: String::new(),
            gone: false,
        }
    }

    fn send(&mut self, command: &str, args: Value) -> i64 {
        self.seq += 1;
        let body = json!({
            "seq": self.seq,
            "type": "request",
            "command": command,
            "arguments": args,
        })
        .to_string();
        let _ = write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = self.stdin.flush();
        self.seq
    }

    /// The next message, or nothing once it has gone quiet for longer than
    /// [`patience`].
    fn next(&mut self) -> Option<Value> {
        if self.gone {
            return None;
        }
        match self.inbox.recv_timeout(Duration::from_secs(patience())) {
            Ok(v) => Some(v),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                self.gone = true;
                None
            }
        }
    }

    /// What arrived that was an event, kept; the program's output, added up.
    fn keep(&mut self, v: Value) {
        if v["event"] == "output" {
            if let Some(text) = v["body"]["output"].as_str() {
                self.output.push_str(text);
            }
        }
        self.events.push(v);
    }

    /// `command` asked, and its response.
    fn ask(&mut self, command: &str, args: Value) -> Option<Value> {
        let n = self.send(command, args);
        while let Some(v) = self.next() {
            if v["type"] == "response" && v["request_seq"] == n {
                return Some(v);
            }
            self.keep(v);
        }
        None
    }

    /// The next stop or end: a `stopped` event, or `terminated`. One that
    /// arrived already is taken first.
    fn stopped(&mut self) -> Option<Value> {
        if let Some(i) = self
            .events
            .iter()
            .position(|e| e["event"] == "stopped" || e["event"] == "terminated")
        {
            let e = self.events.remove(i);
            return Some(e);
        }
        while let Some(v) = self.next() {
            if v["event"] == "stopped" || v["event"] == "terminated" {
                return Some(v);
            }
            self.keep(v);
        }
        None
    }

    /// The exit code an `exited` event gave, if one came.
    fn exit_code(&self) -> Option<Value> {
        self.events
            .iter()
            .find(|e| e["event"] == "exited")
            .map(|e| e["body"]["exitCode"].clone())
    }

    fn stop(mut self) {
        if !self.gone {
            self.send("disconnect", json!({}));
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- comparing -----------------------------------------------------------------

/// What of an answer is compared, by kind.
fn normal(kind: &str, v: &Value) -> Value {
    if v["type"] == "response" && v["success"] == false {
        return json!({ "refused": true });
    }
    let body = &v["body"];
    match kind {
        "launch" | "step" => json!({ "success": v["success"] }),
        "setBreakpoints" => Value::Array(
            body["breakpoints"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|b| json!({ "verified": b["verified"], "line": b["line"] }))
                .collect(),
        ),
        "stopped" => json!({ "event": v["event"], "reason": body["reason"] }),
        "stackTrace" => json!({
            "frames": body["stackFrames"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|f| json!({
                    "name": f["name"],
                    "line": f["line"],
                    "file": f["source"]["path"]
                        .as_str()
                        .and_then(|p| Path::new(&p.replace('\\', "/")).file_name().map(|n| n.to_string_lossy().to_lowercase())),
                }))
                .collect::<Vec<_>>(),
            "total": body["totalFrames"],
        }),
        "scopes" => Value::Array(
            body["scopes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|s| s["name"].clone())
                .collect(),
        ),
        "variables" => Value::Array(
            body["variables"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|r| {
                    json!({
                        "name": r["name"],
                        "value": r["value"],
                        "type": r["type"],
                        "opens": r["variablesReference"].as_i64().unwrap_or(0) > 0,
                    })
                })
                .collect(),
        ),
        // Not its type: the Rust adapter gives an evaluated name the type of
        // the first binding of that name anywhere in the program, its
        // libraries' too, rather than of the one in scope.
        "evaluate" => json!({
            "result": body["result"],
            "opens": body["variablesReference"].as_i64().unwrap_or(0) > 0,
        }),
        _ => v.clone(),
    }
}

/// How many answers of each kind were compared, and how many of those were
/// answered alike.
#[derive(Default)]
struct Tally {
    kinds: BTreeMap<String, (usize, usize)>,
    log: String,
    shown: BTreeMap<String, usize>,
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
        let shown = self.shown.entry(kind.to_string()).or_default();
        *shown += 1;
        if *shown <= 300 {
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
            self.log.push_str(&format!(
                "== {kind} {what}\nmeadow:     {}\nmeadowboot: {}\n",
                cut(&r),
                cut(&b)
            ));
        }
    }

    fn report(&self) -> String {
        let mut out = String::from("kind            agree / asked\n");
        let (mut all, mut same) = (0, 0);
        for (k, (n, ok)) in &self.kinds {
            out.push_str(&format!("{k:<15} {ok:>6} / {n:<6}\n"));
            all += n;
            same += ok;
        }
        out.push_str(&format!("{:<15} {same:>6} / {all:<6}\n", "all"));
        out
    }
}

// --- the programs -----------------------------------------------------------------

/// The programs written for the test, each a package of one file: a name and
/// its `src/Main.mw`.
const WRITTEN: &[(&str, &str)] = &[
    (
        "calls",
        "\
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
",
    ),
    (
        "loop",
        "\
fun count (n : Int) (acc : Int) : Int =
  if n == 0 then acc
  else count (n - 1) (acc + n)

fun main () =
  let total = count 5 0 in
  println total
",
    ),
    (
        "values",
        "\
data Shape = Circle Float | Rect Float Float

use Shape.*

fun area s =
  match s with
  | Circle r -> 3.0 * r * r
  | Rect w h -> w * h

fun describe (p : (Int, String)) =
  let (n, word) = p in
  let point = { x = n, y = word } in
  point.y

fun main () =
  let shapes = [Circle 1.0, Rect 2.0 3.0] in
  let total = area (Rect 2.0 3.0) in
  let pair = describe (7, \"seven\") in
  let more = Just [1; 2] in
  let u = println total in
  println pair
",
    ),
    (
        "closures",
        "\
fun twice (f : Int -> Int) (x : Int) : Int =
  f (f x)

fun main () =
  let k = 10 in
  let add = \\y -> y + k in
  let r = twice add 1 in
  println r
",
    ),
    (
        "failure",
        "\
fun f (n : Int) : Int =
  n / 0

fun main () =
  let x = 7 in
  println (f x)
",
    ),
];

/// The repository's examples debugged as well: packages whose `main` the
/// CEK machine runs as it is.
const EXAMPLES: &[&str] = &["Euler", "Tour"];

/// Every program: its name, its package, and the files to set breakpoints
/// in.
fn programs() -> Vec<(String, PathBuf, Vec<PathBuf>)> {
    let only: Option<Vec<String>> = std::env::var("MEADOW_DAP_PROGRAMS")
        .ok()
        .map(|s| s.split(',').map(str::to_string).collect());
    let wanted = |n: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == n));
    let mut out = Vec::new();
    let base = out_dir().join("dap-programs");
    for (name, source) in WRITTEN {
        if !wanted(name) {
            continue;
        }
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join("src")).expect("a scratch directory");
        std::fs::write(
            dir.join("Meadow.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\n",
                capitalised(name)
            ),
        )
        .expect("a manifest");
        let main = dir.join("src").join("Main.mw");
        std::fs::write(&main, source).expect("the program");
        out.push((name.to_string(), dir, vec![main]));
    }
    for name in EXAMPLES {
        if !wanted(name) {
            continue;
        }
        let dir = repo().join("examples").join(name);
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("src"))
            .expect("the example's sources")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "mw"))
            .collect();
        files.sort();
        out.push((name.to_string(), dir, files));
    }
    out
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

// --- the sessions -----------------------------------------------------------------

fn rust_adapter(root: &Path) -> Adapter {
    let mut c = Command::new(meadow());
    c.arg("dap").current_dir(root);
    Adapter::start(c)
}

/// MeadowBoot's adapter, debugging on Glade through `meadow-host` -- or,
/// where `MEADOWBOOT_DAP=cek` asks, on its CEK machine, with the Rust `meadow`
/// run for what it hands on.
fn boot_adapter(root: &Path, exe: &Path) -> Adapter {
    let mut c = Command::new(exe);
    c.arg("dap")
        .current_dir(root)
        .env("MEADOWBOOT_STD", repo().join("lib").join("Std"))
        .env("MEADOWBOOT_SET", "macros=Glade")
        .env_remove("MEADOWBOOT_EXPANSIONS");
    if std::env::var("MEADOWBOOT_DAP").as_deref() == Ok("cek") {
        c.env("MEADOW", meadow());
    } else {
        c.env("MEADOW_HOST", meadow_host()).env_remove("MEADOW");
    }
    Adapter::start(c)
}

/// `meadow-host`, the Rust `meadow` as a library, built beside the `meadow`
/// this test runs -- or the one `MEADOWBOOT_HOST` names.
fn meadow_host() -> PathBuf {
    if let Some(given) = std::env::var_os("MEADOWBOOT_HOST") {
        return PathBuf::from(given);
    }
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = meadow().parent().expect("meadow's directory").to_path_buf();
            let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            c.args(["build", "-q", "-p", "meadow-host", "--manifest-path"])
                .arg(repo().join("buildtools").join("Cargo.toml"));
            if dir.file_name().is_some_and(|n| n == "release") {
                c.arg("--release");
            }
            let out = c.output().expect("cargo runs");
            assert!(
                out.status.success(),
                "meadow-host did not build:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            dir.join(format!(
                "{}meadow_host{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ))
        })
        .clone()
}

/// Both adapters asked the same thing, and what they answered tallied.
fn both(
    t: &mut Tally,
    a: &mut (Adapter, Adapter),
    kind: &str,
    what: &str,
    command: &str,
    args: Value,
) -> (Option<Value>, Option<Value>) {
    let r = a.0.ask(command, args.clone());
    let b = a.1.ask(command, args);
    t.add(kind, what, r.clone(), b.clone());
    (r, b)
}

/// How many stops a session makes at most.
const STOPS: usize = 40;

/// Where each stood, looked at alike: the stack, the first frames' scopes
/// and locals, and the innermost frame's locals evaluated.
fn inspect(t: &mut Tally, a: &mut (Adapter, Adapter), at: &str) {
    let (r, b) = both(
        t,
        a,
        "stackTrace",
        at,
        "stackTrace",
        json!({ "threadId": 1 }),
    );
    let count = |v: &Option<Value>| {
        v.as_ref()
            .and_then(|v| v["body"]["stackFrames"].as_array().map(Vec::len))
            .unwrap_or(0)
    };
    let frames = count(&r).min(count(&b)).min(3);
    for f in 0..frames {
        let what = format!("{at} frame {f}");
        let (rs, bs) = both(t, a, "scopes", &what, "scopes", json!({ "frameId": f }));
        let locals = |v: &Option<Value>| {
            v.as_ref().and_then(|v| {
                v["body"]["scopes"]
                    .as_array()?
                    .iter()
                    .find(|s| s["name"] == "Locals")
                    .and_then(|s| s["variablesReference"].as_i64())
            })
        };
        let (Some(rl), Some(bl)) = (locals(&rs), locals(&bs)) else {
            continue;
        };
        let rv = a.0.ask("variables", json!({ "variablesReference": rl }));
        let bv = a.1.ask("variables", json!({ "variablesReference": bl }));
        t.add("variables", &what, rv.clone(), bv.clone());
        if f == 0 {
            let names: Vec<String> = rv
                .iter()
                .chain(bv.iter())
                .flat_map(|v| {
                    v["body"]["variables"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .fold(Vec::new(), |mut acc, n| {
                    if !acc.contains(&n) {
                        acc.push(n);
                    }
                    acc
                });
            for name in names
                .iter()
                .take(6)
                .map(String::as_str)
                .chain(["noSuchName"])
            {
                both(
                    t,
                    a,
                    "evaluate",
                    &format!("{at} {name}"),
                    "evaluate",
                    json!({ "expression": name, "frameId": 0, "context": "hover" }),
                );
            }
        }
    }
}

/// Both adapters launched on `root`, `files`' lines given breakpoints where
/// `every_line`, and configured: what each answered tallied, and whether
/// both launched it.
fn launched(
    t: &mut Tally,
    root: &Path,
    files: &[PathBuf],
    exe: &Path,
    name: &str,
    every_line: bool,
) -> ((Adapter, Adapter), bool) {
    let mut a = (rust_adapter(root), boot_adapter(root, exe));
    let init = json!({
        "clientID": "dap-differential",
        "adapterID": "meadow",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "pathFormat": "path",
    });
    both(t, &mut a, "initialize", name, "initialize", init);
    let program = root.display().to_string();
    let (r, b) = both(
        t,
        &mut a,
        "launch",
        name,
        "launch",
        json!({ "program": program, "stopOnEntry": !every_line }),
    );
    if every_line {
        for file in files {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            let lines: Vec<Value> = (1..=text.lines().count())
                .map(|l| json!({ "line": l }))
                .collect();
            both(
                t,
                &mut a,
                "setBreakpoints",
                &format!("{name} {}", file.display()),
                "setBreakpoints",
                json!({
                    "source": { "path": file.display().to_string() },
                    "breakpoints": lines,
                }),
            );
        }
    }
    both(
        t,
        &mut a,
        "configurationDone",
        name,
        "configurationDone",
        json!({}),
    );
    (a, ok(&r) && ok(&b))
}

/// Whether a request was answered, and answered yes.
fn ok(v: &Option<Value>) -> bool {
    v.as_ref().is_some_and(|v| v["success"] == true)
}

/// One session: launched, then stopped and looked at again and again, each
/// time going on as `next` says, until it ends or has stopped [`STOPS`]
/// times. What it wrote, and how it ended, compared last.
fn session(
    t: &mut Tally,
    root: &Path,
    files: &[PathBuf],
    exe: &Path,
    name: &str,
    every_line: bool,
    next: &dyn Fn(usize) -> &'static str,
) {
    let (mut a, running) = launched(t, root, files, exe, name, every_line);
    // A program one of them could not launch never stops: there is nothing
    // to wait for.
    for i in 0..if running { STOPS } else { 0 } {
        let r = a.0.stopped();
        let b = a.1.stopped();
        let at = format!("{name} stop {i}");
        t.add("stopped", &at, r.clone(), b.clone());
        let ended = |v: &Option<Value>| v.as_ref().is_none_or(|v| v["event"] == "terminated");
        if ended(&r) || ended(&b) {
            break;
        }
        inspect(t, &mut a, &at);
        let command = next(i);
        let args = json!({ "threadId": 1 });
        let (r, b) = both(t, &mut a, "step", &format!("{at} {command}"), command, args);
        // A program that has failed is not run on: there is no stop to wait
        // for from an adapter that refused to.
        if !ok(&r) || !ok(&b) {
            break;
        }
    }
    t.add(
        "output",
        name,
        Some(json!({ "output": a.0.output.replace("\r\n", "\n"), "exitCode": a.0.exit_code() })),
        Some(json!({ "output": a.1.output.replace("\r\n", "\n"), "exitCode": a.1.exit_code() })),
    );
    a.0.stop();
    a.1.stop();
}

fn progress(line: &str) {
    eprintln!("{line}");
    let out = out_dir().join("dap-progress.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The steps a stepping session takes, over and over.
const STEPPING: &[&str] = &[
    "stepIn", "stepIn", "next", "stepIn", "stepOut", "next", "next", "stepIn", "stepOut", "next",
];

/// MeadowBoot's debug adapter answers every request as `meadow dap`
/// answers it.
///
/// Not yet, so it is ignored: run it with `--ignored` to see how far.
/// MeadowBoot's adapter debugs on Glade, through `meadow-host`, with the
/// Rust adapter's own session, and the program MeadowBoot built for
/// debugging says where it was written in its Cut (`docs/CUT.md`, "Positions
/// and names"). `MEADOWBOOT_DAP=cek` runs it on MeadowBoot's CEK machine
/// instead, as it did before. On 2026-11-13, over the five programs written
/// here and the `Euler` and `Tour` examples, the two agreed on 961 of 1220
/// answers, in about six minutes -- on the CEK machine they agreed on 894 of
/// 1235:
///
/// ```text
/// configurationDone   14 / 14     output       10 / 14
/// evaluate           223 / 322    scopes      234 / 234
/// initialize          14 / 14     setBreakpoints 7 / 9
/// launch              14 / 14     stackTrace   64 / 118
/// step               118 / 118    stopped     125 / 129
/// variables          138 / 234
/// ```
///
/// What is left is mostly not the debugger's. Of the 259 answers that
/// differ, 193 are `Tour`'s and 23 `Euler`'s, whose sessions go another way
/// from the first stop: the back end computes a top-level value that may
/// perform -- by its analysis, most of them -- when the program starts
/// (`docs/CUT.md`, "Top-level values"), the Rust core when it is first read,
/// so MeadowBoot's starts in a value's definition before `main`. A function of
/// the standard library that MeadowBoot copies into the package for the
/// dictionaries it is given (`println`) is a copy with no positions, so a step
/// into it stops in what it calls. And the two lowerings keep different
/// things in a frame: a closure's captures are its locals here, and a type is
/// written as MeadowBoot's inference has it.
#[test]
#[ignore = "MeadowBoot's debug adapter does not agree with meadow dap yet: see the doc comment"]
fn meadowboot_s_debugger_answers_as_meadow_dap_answers() {
    let exe = meadowboot_exe();
    let mut t = Tally::default();
    for (name, root, files) in programs() {
        let started = std::time::Instant::now();
        progress(&format!("{name}: stepping"));
        session(
            &mut t,
            &root,
            &files,
            &exe,
            &format!("{name} stepping"),
            false,
            &|i| STEPPING[i % STEPPING.len()],
        );
        progress(&format!("{name}: breakpoints"));
        session(
            &mut t,
            &root,
            &files,
            &exe,
            &format!("{name} breakpoints"),
            true,
            &|_| "continue",
        );
        progress(&format!("{name}: {:.0?}", started.elapsed()));
    }
    let report = t.report();
    let out = out_dir().join("dap-differences.txt");
    let _ = std::fs::write(&out, format!("{report}\n{}", t.log));
    eprintln!("{report}\nwhat differs: {}", out.display());
    assert!(
        t.kinds.values().all(|(n, ok)| n == ok),
        "the adapters answered differently:\n{report}"
    );
}
