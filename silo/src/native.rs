//! **What an unhandled effect operation means**: the real world's answer.
//!
//! The same set, and the same answers, as `meadow-glade`'s natives (and so as
//! the CEK machine's): `Console`, `Fs`, `Process`, `Random`, `Time`; and
//! `Test.fail`, which is a failure rather than a value.

use crate::heap::{self, Word};
use crate::show;
use crate::value::{self, Val, val};
use meadow_rt::desc;
use meadow_rt::roles::{Role, TUPLE};

fn fail<T>(msg: impl AsRef<str>) -> T {
    crate::fail(msg.as_ref())
}

/// Read exactly `n` bytes of `input`, growing the buffer as they arrive, or
/// `None` if it ends first. Reading in chunks means a length that lies about
/// how much follows costs nothing until the bytes come, rather than a
/// preallocation of `n` that aborts the process. See `Console.readExact`.
fn read_exact_bounded<R: std::io::Read>(input: &mut R, n: i64) -> std::io::Result<Option<String>> {
    let mut remaining = n.max(0) as usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match input.read(&mut chunk[..want]) {
            Ok(0) => return Ok(None),
            Ok(got) => {
                buf.extend_from_slice(&chunk[..got]);
                remaining -= got;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// A result, built bottom-up. Nothing moves, so it is built as it is made.
enum Build {
    At(Val),
    Str(String),
    /// The constructor playing a role, with its fields.
    Data(Role, Vec<Build>),
    Tuple(Vec<Build>),
    Bytes(Vec<u8>),
    Vector(Vec<Build>),
}

impl Build {
    fn unit() -> Build {
        Build::At(Val::Unit)
    }
    fn int(n: i64) -> Build {
        Build::At(Val::Int(n))
    }
    fn bool(b: bool) -> Build {
        Build::At(Val::Bool(b))
    }
    fn ok(v: Build) -> Build {
        Build::Data(Role::Ok, vec![v])
    }
    fn error(msg: String) -> Build {
        Build::Data(Role::Err, vec![Build::Str(msg)])
    }

    fn make(self) -> Val {
        match self {
            Build::At(v) => v,
            Build::Str(s) => Val::Ref(heap::string(s.as_bytes())),
            Build::Data(role, xs) => {
                let fields: Vec<Val> = xs.into_iter().map(Build::make).collect();
                Val::Ref(value::data(show::role(role), &fields))
            }
            Build::Tuple(xs) => {
                let fields: Vec<Val> = xs.into_iter().map(Build::make).collect();
                Val::Ref(value::data(TUPLE, &fields))
            }
            Build::Bytes(b) => {
                let words: Vec<Word> = b.iter().map(|x| Word::from(*x)).collect();
                Val::Ref(crate::prims::new_array(
                    heap::ARRAY,
                    &words,
                    desc::word(meadow_rt::num::Width::U8),
                ))
            }
            Build::Vector(xs) => {
                let items: Vec<Val> = xs.into_iter().map(Build::make).collect();
                Val::Ref(vector(items))
            }
        }
    }
}

/// `vWidth` in `Std.Collections.Vector`.
const VECTOR_WIDTH: usize = 32;

/// `items`, owned, laid out as `Vector.fromArray` would -- which has to stay
/// `vBuildTree` in `Std.Collections.Vector`, since that module takes it apart.
fn vector(items: Vec<Val>) -> Word {
    let arr = |xs: &[Val]| {
        let d = xs.first().map_or(desc::REF, |v| v.bits().1);
        let words: Vec<Word> = xs.iter().map(|v| v.word()).collect();
        Val::Ref(crate::prims::new_array(heap::ARRAY, &words, d))
    };
    if items.is_empty() {
        return value::data(show::role(Role::VectorEmpty), &[]);
    }
    if items.len() <= VECTOR_WIDTH {
        let a = arr(&items);
        return value::data(show::role(Role::VectorSingle), &[a]);
    }
    let n = items.len();
    let mut shift = 5i64;
    let mut count = n.div_ceil(VECTOR_WIDTH);
    while count > VECTOR_WIDTH {
        count = count.div_ceil(VECTOR_WIDTH);
        shift += 5;
    }
    let mut nodes: Vec<Val> = items
        .chunks(VECTOR_WIDTH)
        .map(|chunk| Val::Ref(value::data(show::role(Role::VNodeLeaf), &[arr(chunk)])))
        .collect();
    loop {
        let group = |kids: &[Val]| {
            let none = Val::Ref(value::data(show::role(Role::None), &[]));
            Val::Ref(value::data(
                show::role(Role::VNodeBranch),
                &[none, arr(kids)],
            ))
        };
        if nodes.len() <= VECTOR_WIDTH {
            let root = group(&nodes);
            let empty = || arr(&[]);
            return value::data(
                show::role(Role::VectorFull),
                &[
                    Val::Int(n as i64),
                    Val::Int(shift),
                    empty(),
                    empty(),
                    root,
                    empty(),
                    empty(),
                ],
            );
        }
        nodes = nodes.chunks(VECTOR_WIDTH).map(group).collect();
    }
}

/// `Std.Net`'s sockets and `Std.Tls`'s sessions: see `meadow_rt::net`.
fn net(op: &str, arg: Val) -> Option<Build> {
    use meadow_rt::net;
    let what = format!("Net.{op}");
    let int = |v: Val| match v {
        Val::Int(n) => n,
        other => fail(format!("{what}: expected an Int, got {}", shown(other))),
    };
    let answered = |r: Result<Build, String>| match r {
        Ok(b) => Build::ok(b),
        Err(e) => Build::error(e),
    };
    // What may wait is made by `sched::blocking`, with owned values: a thread
    // waits for it, rather than its worker.
    use crate::sched::blocking;
    Some(match op {
        "tcpListen" => {
            let t = tuple(arg, 2, &what);
            let (host, port) = (text(t[0], &what), int(t[1]));
            answered(net::listen(&host, port).map(Build::int))
        }
        "tcpConnect" => {
            let t = tuple(arg, 2, &what);
            let (host, port) = (text(t[0], &what), int(t[1]));
            answered(blocking(move || net::connect(&host, port)).map(Build::int))
        }
        "tcpAccept" => {
            let id = int(arg);
            answered(
                blocking(move || net::accept(id))
                    .map(|(s, peer)| Build::Tuple(vec![Build::int(s), Build::Str(peer)])),
            )
        }
        "tcpRead" => {
            let t = tuple(arg, 2, &what);
            let (id, max) = (int(t[0]), int(t[1]));
            answered(blocking(move || net::read(id, max)).map(Build::Bytes))
        }
        "tcpWrite" => {
            let t = tuple(arg, 2, &what);
            let id = int(t[0]);
            let bytes = crate::prims::byte_array(t[1], &what);
            answered(blocking(move || net::write(id, &bytes)).map(Build::int))
        }
        "tcpClose" => {
            net::close(int(arg));
            Build::unit()
        }
        "tcpTimeout" => {
            let t = tuple(arg, 2, &what);
            answered(net::set_timeout(int(t[0]), int(t[1])).map(|()| Build::unit()))
        }
        "tcpLocalPort" => answered(net::local_port(int(arg)).map(Build::int)),
        "resolve" => {
            let host = text(arg, &what);
            answered(
                blocking(move || net::resolve(&host))
                    .map(|ips| Build::Vector(ips.into_iter().map(Build::Str).collect())),
            )
        }
        "tlsConnect" | "tlsAccept" => {
            let t = tuple(arg, 3, &what);
            let (id, a, b) = (int(t[0]), text(t[1], &what), text(t[2], &what));
            let connecting = op == "tlsConnect";
            let made = blocking(move || {
                if connecting {
                    net::tls_connect(id, &a, &b)
                } else {
                    net::tls_accept(id, &a, &b)
                }
            });
            answered(made.map(Build::int))
        }
        _ => return None,
    })
}

/// `Std.Log`'s line, where no handler took it: see `meadow_rt::log`.
fn log(op: &str, arg: Val) -> Option<Build> {
    match op {
        "logAt" => {
            let what = "Log.logAt";
            let t = tuple(arg, 2, what);
            let level = match t[0] {
                Val::Int(n) => n,
                other => fail(format!("{what}: expected an Int, got {}", shown(other))),
            };
            meadow_rt::log::line(level, &text(t[1], what));
            Some(Build::unit())
        }
        _ => None,
    }
}

fn text(v: Val, what: &str) -> String {
    match value::text(v) {
        Some(b) => String::from_utf8_lossy(&b).into_owned(),
        None => fail(format!("{what}: expected a String, got {}", shown(v))),
    }
}

fn shown(v: Val) -> String {
    let (w, d) = v.bits();
    show::show(w, d)
}

/// The fields of a tuple of exactly `n`.
fn tuple(v: Val, n: usize, what: &str) -> Vec<Val> {
    match v {
        Val::Ref(w)
            if heap::is_block(w)
                && heap::kind(w) == heap::DATA
                && heap::len(w) == n
                && show::ctor_name(heap::meta(w) as usize) == TUPLE =>
        {
            (0..n)
                .map(|i| val(heap::field(w, i), heap::field_desc(w, i)))
                .collect()
        }
        other => fail(format!(
            "{what}: expected a tuple of {n}, got {}",
            shown(other)
        )),
    }
}

fn vector_arg(v: Val, what: &str) -> Vec<Val> {
    match v {
        Val::Ref(w) => match show::vector_elems(w) {
            Some(xs) => xs.into_iter().map(|(x, d)| val(x, d)).collect(),
            None => fail(format!("{what}: expected a Vector, got {}", shown(v))),
        },
        other => fail(format!("{what}: expected a Vector, got {}", shown(other))),
    }
}

fn maybe(v: Val, what: &str) -> Option<Val> {
    if let Val::Ref(w) = v {
        if w & 1 == 1 && show::role_of(&show::ctor_name((w >> 1) as usize)) == Some(Role::None) {
            return None;
        }
        if heap::is_block(w)
            && heap::kind(w) == heap::DATA
            && show::role_of(&show::ctor_name(heap::meta(w) as usize)) == Some(Role::Some)
        {
            return Some(val(heap::field(w, 0), heap::field_desc(w, 0)));
        }
    }
    fail(format!("{what}: expected a Maybe, got {}", shown(v)))
}

/// An effect operation no handler answers: the world's.
///
/// # Safety
///
/// `effect` and `op` must be NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn meadow_native(
    effect: *const std::ffi::c_char,
    op: *const std::ffi::c_char,
    arg: Word,
    d: i64,
) -> Word {
    // Safety: the caller's.
    let (effect, op) = unsafe {
        (
            std::ffi::CStr::from_ptr(effect).to_string_lossy(),
            std::ffi::CStr::from_ptr(op).to_string_lossy(),
        )
    };
    let effect = meadow_rt::spelling(&effect).to_string();
    let arg = val(arg, d);
    let built = match effect.as_str() {
        "Test" if op == "fail" => fail(show::displayed(arg.word(), d)),
        "Console" => console(&op, arg),
        "Fs" => fs(&op, arg),
        "Process" => process(&op, arg),
        "Random" => random(&op, arg),
        "Time" => time(&op, arg),
        "Log" => log(&op, arg),
        "Net" => net(&op, arg),
        _ => None,
    };
    match built {
        Some(b) => b.make().word(),
        None => fail(format!("unhandled effect {effect}.{op}")),
    }
}

fn console(op: &str, arg: Val) -> Option<Build> {
    use std::io::{BufRead, Write};
    Some(match op {
        "writeOutput" => {
            let s = show::displayed(arg.word(), arg.bits().1);
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(s.as_bytes());
            Build::unit()
        }
        // Standard error, after what was written to standard output so far:
        // a message about what a program printed comes after it.
        "writeError" => {
            let s = show::displayed(arg.word(), arg.bits().1);
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().lock().write_all(s.as_bytes());
            Build::unit()
        }
        "readLine" => {
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(0) => Build::Data(Role::None, vec![]),
                Ok(_) => {
                    let line = line.strip_suffix('\n').unwrap_or(&line);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    Build::Data(Role::Some, vec![Build::Str(line.to_string())])
                }
                Err(e) => fail(format!("Console.readLine: {e}")),
            }
        }
        "readExact" => {
            let _ = std::io::stdout().flush();
            let n = arg.word() as i64;
            // Reads in chunks, so a hostile length costs nothing until the
            // bytes actually arrive, rather than a preallocation of `n`.
            match read_exact_bounded(&mut std::io::stdin().lock(), n) {
                Ok(Some(text)) => Build::Data(Role::Some, vec![Build::Str(text)]),
                Ok(None) => Build::Data(Role::None, vec![]),
                Err(e) => fail(format!("Console.readExact: {e}")),
            }
        }
        // The terminal, for `Std.Terminal`: see `meadow_rt::terminal`.
        "rawMode" => match arg {
            Val::Bool(on) => Build::bool(meadow_rt::terminal::raw(on)),
            other => fail(format!(
                "Console.rawMode: expected a Bool, got {}",
                show::displayed(other.word(), other.bits().1)
            )),
        },
        "readKey" => {
            let (code, text, mods) = meadow_rt::terminal::read_key();
            Build::Tuple(vec![Build::int(code), Build::Str(text), Build::int(mods)])
        }
        "terminalSize" => {
            let (columns, rows) = meadow_rt::terminal::size();
            Build::Tuple(vec![Build::int(columns), Build::int(rows)])
        }
        _ => return None,
    })
}

fn fs(op: &str, arg: Val) -> Option<Build> {
    use std::fs;
    use std::path::Path;
    let what = format!("Fs.{op}");
    let ioerr = |e: std::io::Error| Build::error(e.to_string());
    let unit = |r: std::io::Result<()>| match r {
        Ok(()) => Build::ok(Build::unit()),
        Err(e) => ioerr(e),
    };
    let one = || text(arg, &what);
    let two = || {
        let t = tuple(arg, 2, &what);
        (text(t[0], &what), text(t[1], &what))
    };
    Some(match op {
        "readToString" => match fs::read_to_string(one()) {
            Ok(c) => Build::ok(Build::Str(c)),
            Err(e) => ioerr(e),
        },
        "readBytes" => match fs::read(one()) {
            Ok(b) => Build::ok(Build::Bytes(b)),
            Err(e) => ioerr(e),
        },
        "writeString" => {
            let (p, c) = two();
            unit(fs::write(p, c.as_bytes()))
        }
        "writeBytes" => {
            let t = tuple(arg, 2, &what);
            let path = text(t[0], &what);
            let bytes = crate::prims::byte_array(t[1], &what);
            unit(fs::write(path, bytes))
        }
        "appendString" => {
            use std::io::Write;
            let (p, c) = two();
            let r = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .and_then(|mut f| f.write_all(c.as_bytes()));
            unit(r)
        }
        "removeFile" => unit(fs::remove_file(one())),
        "createDir" => unit(fs::create_dir(one())),
        "createDirAll" => unit(fs::create_dir_all(one())),
        "removeDir" => unit(fs::remove_dir(one())),
        "removeDirAll" => unit(fs::remove_dir_all(one())),
        "rename" => {
            let (a, b) = two();
            unit(fs::rename(a, b))
        }
        "copy" => {
            let (a, b) = two();
            match fs::copy(a, b) {
                Ok(n) => Build::ok(Build::int(n as i64)),
                Err(e) => ioerr(e),
            }
        }
        "readDir" => match fs::read_dir(one()) {
            Ok(entries) => {
                let mut names = Vec::new();
                for e in entries {
                    match e {
                        Ok(en) => {
                            names.push(Build::Str(en.file_name().to_string_lossy().into_owned()))
                        }
                        Err(e) => return Some(ioerr(e)),
                    }
                }
                Build::ok(Build::Vector(names))
            }
            Err(e) => ioerr(e),
        },
        "metadata" => match fs::metadata(one()) {
            // `Std.Fs.FileMeta`, a record the language knows by name: its fields
            // in the order it declares them.
            Ok(md) => Build::ok(Build::Data(
                Role::FileMeta,
                vec![
                    Build::bool(md.is_file()),
                    Build::bool(md.is_dir()),
                    Build::int(md.len() as i64),
                    Build::bool(md.permissions().readonly()),
                    Build::int(meadow_rt::args::modified_millis(&md)),
                ],
            )),
            Err(e) => ioerr(e),
        },
        "exists" => Build::bool(Path::new(&one()).exists()),
        "isFile" => Build::bool(Path::new(&one()).is_file()),
        "isDir" => Build::bool(Path::new(&one()).is_dir()),
        _ => return None,
    })
}

/// A `Command` is `(program, args, cwd, envVars)`; an `Output` is
/// `(status, stdout, stderr)`.
fn process(op: &str, arg: Val) -> Option<Build> {
    use std::process::Command as Proc;
    let what = format!("Process.{op}");
    let command = || {
        let t = tuple(arg, 4, &what);
        let mut cmd = Proc::new(text(t[0], &what));
        for a in vector_arg(t[1], &what) {
            cmd.arg(text(a, &what));
        }
        if let Some(dir) = maybe(t[2], &what) {
            cmd.current_dir(text(dir, &what));
        }
        for e in vector_arg(t[3], &what) {
            let kv = tuple(e, 2, &what);
            cmd.env(text(kv[0], &what), text(kv[1], &what));
        }
        cmd
    };
    Some(match op {
        "spawn" => match command().output() {
            Ok(out) => Build::ok(Build::Tuple(vec![
                Build::int(out.status.code().unwrap_or(-1) as i64),
                Build::Str(String::from_utf8_lossy(&out.stdout).into_owned()),
                Build::Str(String::from_utf8_lossy(&out.stderr).into_owned()),
            ])),
            Err(e) => Build::error(e.to_string()),
        },
        "status" => match command().status() {
            Ok(st) => Build::ok(Build::int(st.code().unwrap_or(-1) as i64)),
            Err(e) => Build::error(e.to_string()),
        },
        // Nothing is noted of what a native program reaches: a fuzzer that
        // asks is told nothing is new, and goes on as one with no guide.
        "covered" => Build::int(0),
        "exit" => match arg {
            Val::Int(code) => {
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(code as i32)
            }
            other => fail(format!(
                "Process.exit: expected an Int, got {}",
                shown(other)
            )),
        },
        "currentPid" => Build::int(std::process::id() as i64),
        "currentExe" => Build::Str(meadow_rt::args::current_exe()),
        "isTerminal" => match arg {
            Val::Int(fd) => Build::bool(is_terminal(fd)),
            other => fail(format!(
                "Process.isTerminal: expected an Int, got {}",
                shown(other)
            )),
        },
        "argv" => Build::Vector(std::env::args().skip(1).map(Build::Str).collect()),
        "getEnv" => match std::env::var(text(arg, &what)) {
            Ok(v) => Build::Data(Role::Some, vec![Build::Str(v)]),
            Err(_) => Build::Data(Role::None, vec![]),
        },
        "setEnv" => {
            let t = tuple(arg, 2, &what);
            let (k, v) = (text(t[0], &what), text(t[1], &what));
            // Safety: set before any other thread of this program reads the
            // environment, as `meadow-glade` does.
            unsafe { std::env::set_var(k, v) };
            Build::unit()
        }
        "removeEnv" => {
            let k = text(arg, &what);
            // Safety: as `setEnv`.
            unsafe { std::env::remove_var(k) };
            Build::unit()
        }
        // Calling C, for `Std.Ffi`: see `meadow_rt::ffi`.
        op if op.starts_with("ffi") => {
            use meadow_rt::ffi;
            let int = |v: Val| match v {
                Val::Int(n) => n,
                other => fail(format!("{what}: expected an Int, got {}", shown(other))),
            };
            let float = |v: Val| match v {
                Val::Float(x) => x,
                other => fail(format!("{what}: expected a Float, got {}", shown(other))),
            };
            let answered = |r: Result<Build, String>| match r {
                Ok(b) => Build::ok(b),
                Err(e) => Build::error(e),
            };
            match op {
                "ffiOpen" => answered(ffi::open(&text(arg, &what)).map(Build::int)),
                "ffiSymbol" => {
                    let t = tuple(arg, 2, &what);
                    answered(ffi::symbol(int(t[0]), &text(t[1], &what)).map(Build::int))
                }
                "ffiCall" => {
                    let t = tuple(arg, 3, &what);
                    let function = int(t[0]);
                    let Some(ret) = ffi::Ret::from_code(int(t[1])) else {
                        return fail(format!("{what}: no such kind of answer"));
                    };
                    let args: Vec<ffi::Arg> = vector_arg(t[2], &what)
                        .into_iter()
                        .map(|a| {
                            let a = tuple(a, 4, &what);
                            match int(a[0]) {
                                0 => ffi::Arg::Int(int(a[1])),
                                1 => ffi::Arg::Float(float(a[2])),
                                2 => ffi::Arg::Str(text(a[3], &what)),
                                3 => ffi::Arg::Float32(float(a[2])),
                                _ => fail(format!("{what}: no such kind of argument")),
                            }
                        })
                        .collect();
                    answered(ffi::call(function, ret, &args).map(|(n, x, s)| {
                        Build::Tuple(vec![Build::int(n), Build::At(Val::Float(x)), Build::Str(s)])
                    }))
                }
                "ffiAlloc" => Build::int(ffi::alloc(int(arg))),
                "ffiFree" => {
                    ffi::free(int(arg));
                    Build::unit()
                }
                "ffiWriteInts" => {
                    let t = tuple(arg, 3, &what);
                    let values: Vec<i64> = vector_arg(t[2], &what).into_iter().map(int).collect();
                    // Safety: the program's word that the memory is its own.
                    answered(
                        unsafe { ffi::write_ints(int(t[0]), int(t[1]), &values) }
                            .map(|()| Build::unit()),
                    )
                }
                "ffiReadInts" => {
                    let t = tuple(arg, 4, &what);
                    let signed = matches!(t[2], Val::Bool(true));
                    // Safety: as above.
                    let read = unsafe { ffi::read_ints(int(t[0]), int(t[1]), signed, int(t[3])) };
                    answered(read.map(|ns| Build::Vector(ns.into_iter().map(Build::int).collect())))
                }
                "ffiWriteFloats" => {
                    let t = tuple(arg, 3, &what);
                    let values: Vec<f64> = vector_arg(t[2], &what).into_iter().map(float).collect();
                    // Safety: as above.
                    answered(
                        unsafe { ffi::write_floats(int(t[0]), int(t[1]), &values) }
                            .map(|()| Build::unit()),
                    )
                }
                "ffiReadFloats" => {
                    let t = tuple(arg, 3, &what);
                    // Safety: as above.
                    let read = unsafe { ffi::read_floats(int(t[0]), int(t[1]), int(t[2])) };
                    answered(read.map(|xs| {
                        Build::Vector(xs.into_iter().map(|x| Build::At(Val::Float(x))).collect())
                    }))
                }
                "ffiReadText" => {
                    let t = tuple(arg, 2, &what);
                    // Safety: as above.
                    Build::Str(unsafe { ffi::read_text(int(t[0]), int(t[1])) })
                }
                // What the last call C made into this library failed with,
                // once; nothing when none did.
                "ffiFailure" => Build::Str(crate::sched::take_failure().unwrap_or_default()),
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// SplitMix64, seeded once per thread from the clock, as `meadow-glade` does.
fn random(op: &str, arg: Val) -> Option<Build> {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    let next = || {
        STATE.with(|s| {
            let mut x = s.get();
            if x == 0 {
                x = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E37_79B9_7F4A_7C15)
                    | 1;
            }
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            s.set(x);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        })
    };
    Some(match op {
        "nextInt" | "nextSeed" => Build::int(next() as i64),
        "nextFloat" => Build::At(Val::Float((next() >> 11) as f64 / (1u64 << 53) as f64)),
        "intBetween" => {
            let t = tuple(arg, 2, &format!("Random.{op}"));
            match (t[0], t[1]) {
                (Val::Int(lo), Val::Int(hi)) => {
                    if hi <= lo {
                        Build::int(lo)
                    } else {
                        Build::int(lo + (next() % (hi - lo) as u64) as i64)
                    }
                }
                _ => fail(format!(
                    "Random.{op}: expected two Ints, got {}",
                    shown(arg)
                )),
            }
        }
        _ => return None,
    })
}

fn time(op: &str, arg: Val) -> Option<Build> {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    Some(match op {
        "now" => Build::int(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        ),
        "monotonic" => Build::int(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as i64),
        "sleep" => match arg {
            Val::Int(ms) if ms > 0 => {
                std::thread::sleep(Duration::from_millis(ms as u64));
                Build::unit()
            }
            Val::Int(_) => Build::unit(),
            other => fail(format!("Time.sleep: expected an Int, got {}", shown(other))),
        },
        _ => return None,
    })
}

/// Whether standard input (0), output (1) or error (2) is a terminal:
/// `Process.isTerminal`, for a program choosing whether to colour what it
/// writes: for output, a terminal that draws colour -- which a Windows console
/// does once asked to (`meadow_core::console::ansi`).
fn is_terminal(fd: i64) -> bool {
    use std::io::IsTerminal;
    match fd {
        0 => std::io::stdin().is_terminal(),
        1 => std::io::stdout().is_terminal() && meadow_rt::console::ansi(),
        2 => std::io::stderr().is_terminal() && meadow_rt::console::ansi(),
        _ => false,
    }
}
