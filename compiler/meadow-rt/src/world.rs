//! **What an unhandled effect operation means**: the real world's answer, once
//! for every runtime.
//!
//! `Std.Fs`, `Std.Process`, `Std.Console`, `Std.Random`, `Std.Time`,
//! `Std.Log`, `Std.Net` and `Std.Ffi` each pair an effect with handlers that
//! fake it; an operation no handler takes is a request for the real thing,
//! and this is where it is made. Glade and Silo each kept a copy of all of
//! this, which drifted apart and had to be fixed twice (a sequence answered
//! as a vector where the program declares none). Now each runtime says only
//! how to read its values and how to build the answer -- see [`World`] -- and
//! [`perform`] is the rest (`docs/RUNTIME-MERGE.md`, phase 1).
//!
//! # Answers are descriptions
//!
//! An answer is a tree -- `Ok (Just #[1, 2, 3])`, a file's metadata, a list
//! of names -- and a runtime whose collector moves objects cannot build one
//! an object at a time while holding the addresses of the parts already made.
//! So an operation answers a [`Build`]: an ordinary Rust value, with no
//! address of the heap in it, which the runtime measures and makes in one go.
//!
//! # Where the runtimes differ
//!
//! What is the runtime's own is asked of it through [`World`]: how a value is
//! read and how one is shown, in its own words; what a program's arguments
//! are; where its output goes (a debugger may take it); how a call that waits
//! is waited for (off the worker, by its scheduler); and how a program ends.
//! Everything else is here, and is the same on every runtime.

use crate::roles::Role;

/// A value a runtime can make without its heap: a word and what it is.
pub trait Imm: Copy {
    fn unit() -> Self;
    fn int(n: i64) -> Self;
    fn float(x: f64) -> Self;
    fn boolean(b: bool) -> Self;
}

/// An answer to build, described before any of it exists. Nothing in it is
/// an address, so nothing in it can be invalidated by the collection that
/// making room for it may set off. `At` holds an immediate.
#[derive(Debug, Clone)]
pub enum Build<V> {
    At(V),
    Str(String),
    /// The constructor playing a role, with its fields.
    Data(Role, Vec<Build<V>>),
    Tuple(Vec<Build<V>>),
    /// An `Array` of `UInt8`, a byte to an element.
    Bytes(Vec<u8>),
    /// A `Std.Collections.Vector`, laid out as `Vector.fromArray` lays one out
    /// -- or a plain array, where the program declares no vector
    /// (`docs/CUT.md`).
    Vector(Vec<Build<V>>),
    /// A constructor of the program's own, by name, with its fields: what an
    /// embedding host answers with. `Event.Clicked`, or `Clicked` where only
    /// one constructor is called that.
    Ctor(String, Vec<Build<V>>),
}

impl<V: Imm> Build<V> {
    pub fn unit() -> Self {
        Build::At(V::unit())
    }

    pub fn int(n: i64) -> Self {
        Build::At(V::int(n))
    }

    pub fn float(x: f64) -> Self {
        Build::At(V::float(x))
    }

    pub fn bool(b: bool) -> Self {
        Build::At(V::boolean(b))
    }

    pub fn str(s: impl Into<String>) -> Self {
        Build::Str(s.into())
    }

    pub fn ok(v: Self) -> Self {
        Build::Data(Role::Ok, vec![v])
    }

    pub fn error(msg: String) -> Self {
        Build::Data(Role::Err, vec![Build::Str(msg)])
    }

    pub fn some(v: Self) -> Self {
        Build::Data(Role::Some, vec![v])
    }

    pub fn none() -> Self {
        Build::Data(Role::None, Vec::new())
    }

    /// `Ok` of what was made, or `Err` of why not.
    pub fn answered(r: Result<Self, String>) -> Self {
        match r {
            Ok(b) => Build::ok(b),
            Err(e) => Build::error(e),
        }
    }
}

/// Work that waits on the world -- an accept, a read, a handshake -- made of
/// owned values, so that it can run on a thread of its own.
pub type Job<V> = Box<dyn FnOnce() -> Build<V> + Send>;

/// What a runtime says of itself for [`perform`].
///
/// A reader answers `Err` with the whole message, in the runtime's own words:
/// `what` names the operation asking (`Fs.readToString`).
pub trait World {
    type Value: Imm + Send + 'static;

    fn int_of(&self, v: Self::Value) -> Option<i64>;
    fn float_of(&self, v: Self::Value) -> Option<f64>;
    fn bool_of(&self, v: Self::Value) -> Option<bool>;
    fn text(&self, v: Self::Value, what: &str) -> Result<String, String>;
    fn bytes(&self, v: Self::Value, what: &str) -> Result<Vec<u8>, String>;
    /// The fields of a tuple of exactly `n`.
    fn tuple(&self, v: Self::Value, n: usize, what: &str) -> Result<Vec<Self::Value>, String>;
    /// The elements of a sequence the program gives: a vector, or a plain
    /// array, which is a language's that has no vector.
    fn seq(&self, v: Self::Value, what: &str) -> Result<Vec<Self::Value>, String>;
    /// `Just x` / `None`.
    fn maybe(&self, v: Self::Value, what: &str) -> Result<Option<Self::Value>, String>;
    /// `v` as an error message shows it.
    fn shown(&self, v: Self::Value) -> String;
    /// The text `Console.writeOutput` and `writeError` write of `v`.
    fn printed(&self, v: Self::Value, what: &str) -> Result<String, String>;

    /// Write to standard output, or wherever the runtime sends it.
    fn write_out(&mut self, s: &str) {
        use std::io::Write;
        let _ = std::io::stdout().lock().write_all(s.as_bytes());
    }
    /// A line from wherever the runtime's input comes from, if not standard
    /// input: `Some(None)` at its end, `None` to read standard input.
    fn read_line(&mut self) -> Option<Option<String>> {
        None
    }
    /// What the program was given: `Process.argv`.
    fn argv(&self) -> Vec<String>;
    /// How much of the program has been reached, for a fuzzer to steer by;
    /// `reset` starts the count again. Nothing, where nothing is noted.
    fn covered(&mut self, reset: bool) -> i64 {
        let _ = reset;
        0
    }
    /// What the last call C made into this library failed with, once.
    fn ffi_failure(&mut self) -> String {
        String::new()
    }
    /// End the program with `code`, standard output written out.
    fn exit(&mut self, code: i32) -> !;
    /// Run `job`, which waits on the world, so that what waits is the thread
    /// and not its worker; answer what it answers -- or, where the runtime
    /// waits for it later, a placeholder it will replace.
    fn blocking(&mut self, job: Job<Self::Value>) -> Build<Self::Value>;
}

/// The world's answer to `effect.op` given `arg`: `Ok(None)` where the
/// operation is not one the runtime performs (a host's, or nobody's), and
/// `Err` with the message where the program got it wrong.
///
/// `effect` is by its spelling or its canonical name: `Log` or `Std.Log.Log`.
/// Only the standard library's are taken by their spelling, so that a host's
/// own effect called `Console` is still the host's. `Test.fail` is not here:
/// it is a failure, which each runtime has its own way of.
pub fn perform<W: World>(
    w: &mut W,
    effect: &str,
    op: &str,
    arg: W::Value,
) -> Result<Option<Build<W::Value>>, String> {
    let name = if effect.starts_with("Std.") {
        crate::spelling(effect)
    } else {
        effect
    };
    match name {
        "Console" => console(w, op, arg),
        "Fs" => fs(w, op, arg),
        "Process" => process(w, op, arg),
        "Random" => random(w, op, arg),
        "Time" => time(w, op, arg),
        "Log" => log(w, op, arg),
        "Net" => net(w, op, arg),
        _ => Ok(None),
    }
}

fn int<W: World>(w: &W, v: W::Value, what: &str) -> Result<i64, String> {
    w.int_of(v)
        .ok_or_else(|| format!("{what}: expected an Int, got {}", w.shown(v)))
}

fn float<W: World>(w: &W, v: W::Value, what: &str) -> Result<f64, String> {
    w.float_of(v)
        .ok_or_else(|| format!("{what}: expected a Float, got {}", w.shown(v)))
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

/// Standard output and input, and the terminal.
///
/// `readLine` strips the line terminator, `\r\n` included, so a program
/// reading a file piped in on Windows sees the same lines as one reading a
/// terminal. End of input is `None`, not an error: a loop over standard input
/// ends by matching it.
fn console<W: World>(
    w: &mut W,
    op: &str,
    arg: W::Value,
) -> Result<Option<Build<W::Value>>, String> {
    use std::io::{BufRead, Write};
    let what = format!("Console.{op}");
    Ok(Some(match op {
        "writeOutput" => {
            let s = w.printed(arg, &what)?;
            w.write_out(&s);
            Build::unit()
        }
        // Standard error, after what was written to standard output so far:
        // a message about what a program printed comes after it.
        "writeError" => {
            let s = w.printed(arg, &what)?;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().lock().write_all(s.as_bytes());
            Build::unit()
        }
        "readLine" => {
            if let Some(line) = w.read_line() {
                return Ok(Some(match line {
                    Some(l) => Build::some(Build::Str(l)),
                    None => Build::none(),
                }));
            }
            // What was written before the program waits on its input is what
            // whoever gives the input may be waiting for: a prompt, or a reply
            // the next request depends on.
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                // Zero bytes is end of input, not an empty line: an empty line
                // still carries its terminator.
                Ok(0) => Build::none(),
                Ok(_) => {
                    let line = line.strip_suffix('\n').unwrap_or(&line);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    Build::some(Build::Str(line.to_string()))
                }
                Err(e) => return Err(format!("Console.readLine: {e}")),
            }
        }
        "readExact" => {
            let n = int(w, arg, &what)?;
            let _ = std::io::stdout().flush();
            match read_exact_bounded(&mut std::io::stdin().lock(), n) {
                Ok(Some(text)) => Build::some(Build::Str(text)),
                Ok(None) => Build::none(),
                Err(e) => return Err(format!("Console.readExact: {e}")),
            }
        }
        // The terminal, for `Std.Terminal`: see `crate::terminal`.
        "rawMode" => match w.bool_of(arg) {
            Some(on) => Build::bool(crate::terminal::raw(on)),
            None => {
                return Err(format!(
                    "Console.rawMode: expected a Bool, got {}",
                    w.shown(arg)
                ));
            }
        },
        "readKey" => {
            let (code, text, mods) = crate::terminal::read_key();
            Build::Tuple(vec![Build::int(code), Build::Str(text), Build::int(mods)])
        }
        "terminalSize" => {
            let (columns, rows) = crate::terminal::size();
            Build::Tuple(vec![Build::int(columns), Build::int(rows)])
        }
        _ => return Ok(None),
    }))
}

fn fs<W: World>(w: &mut W, op: &str, arg: W::Value) -> Result<Option<Build<W::Value>>, String> {
    use std::fs;
    use std::path::Path;
    let what = format!("Fs.{op}");
    let ioerr = |e: std::io::Error| Build::error(e.to_string());
    let unit = |r: std::io::Result<()>| match r {
        Ok(()) => Build::ok(Build::unit()),
        Err(e) => Build::error(e.to_string()),
    };
    let one = |w: &W| w.text(arg, &what);
    let two = |w: &W| -> Result<(String, String), String> {
        let t = w.tuple(arg, 2, &what)?;
        Ok((w.text(t[0], &what)?, w.text(t[1], &what)?))
    };
    Ok(Some(match op {
        "readToString" => match fs::read_to_string(one(w)?) {
            Ok(c) => Build::ok(Build::Str(c)),
            Err(e) => ioerr(e),
        },
        "readBytes" => match fs::read(one(w)?) {
            Ok(b) => Build::ok(Build::Bytes(b)),
            Err(e) => ioerr(e),
        },
        "writeString" => {
            let (p, c) = two(w)?;
            unit(fs::write(p, c.as_bytes()))
        }
        "writeBytes" => {
            let t = w.tuple(arg, 2, &what)?;
            let path = w.text(t[0], &what)?;
            let bytes = w.bytes(t[1], &what)?;
            unit(fs::write(path, bytes))
        }
        "appendString" => {
            use std::io::Write;
            let (p, c) = two(w)?;
            let r = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .and_then(|mut f| f.write_all(c.as_bytes()));
            unit(r)
        }
        "removeFile" => unit(fs::remove_file(one(w)?)),
        "createDir" => unit(fs::create_dir(one(w)?)),
        "createDirAll" => unit(fs::create_dir_all(one(w)?)),
        "removeDir" => unit(fs::remove_dir(one(w)?)),
        "removeDirAll" => unit(fs::remove_dir_all(one(w)?)),
        "rename" => {
            let (a, b) = two(w)?;
            unit(fs::rename(a, b))
        }
        "copy" => {
            let (a, b) = two(w)?;
            match fs::copy(a, b) {
                Ok(n) => Build::ok(Build::int(n as i64)),
                Err(e) => ioerr(e),
            }
        }
        "readDir" => match fs::read_dir(one(w)?) {
            Ok(entries) => {
                let mut names = Vec::new();
                for e in entries {
                    match e {
                        Ok(en) => {
                            names.push(Build::Str(en.file_name().to_string_lossy().into_owned()))
                        }
                        Err(e) => return Ok(Some(ioerr(e))),
                    }
                }
                Build::ok(Build::Vector(names))
            }
            Err(e) => ioerr(e),
        },
        "metadata" => match fs::metadata(one(w)?) {
            // `Std.Fs.FileMeta`, a record the language knows by name: its
            // fields in the order it declares them.
            Ok(md) => Build::ok(Build::Data(
                Role::FileMeta,
                vec![
                    Build::bool(md.is_file()),
                    Build::bool(md.is_dir()),
                    Build::int(md.len() as i64),
                    Build::bool(md.permissions().readonly()),
                    Build::int(crate::args::modified_millis(&md)),
                ],
            )),
            Err(e) => ioerr(e),
        },
        "exists" => Build::bool(Path::new(&one(w)?).exists()),
        "isFile" => Build::bool(Path::new(&one(w)?).is_file()),
        "isDir" => Build::bool(Path::new(&one(w)?).is_dir()),
        _ => return Ok(None),
    }))
}

/// A `Command` is `(program, args, cwd, envVars)`; an `Output` is
/// `(status, stdout, stderr)`.
fn process<W: World>(
    w: &mut W,
    op: &str,
    arg: W::Value,
) -> Result<Option<Build<W::Value>>, String> {
    use std::process::Command as Proc;
    let what = format!("Process.{op}");
    // The whole command read out before anything runs.
    let command = |w: &W| -> Result<Proc, String> {
        let t = w.tuple(arg, 4, &what)?;
        let mut cmd = Proc::new(w.text(t[0], &what)?);
        for a in w.seq(t[1], &what)? {
            cmd.arg(w.text(a, &what)?);
        }
        if let Some(dir) = w.maybe(t[2], &what)? {
            cmd.current_dir(w.text(dir, &what)?);
        }
        for e in w.seq(t[3], &what)? {
            let kv = w.tuple(e, 2, &what)?;
            cmd.env(w.text(kv[0], &what)?, w.text(kv[1], &what)?);
        }
        Ok(cmd)
    };
    Ok(Some(match op {
        "spawn" => match command(w)?.output() {
            Ok(out) => Build::ok(Build::Tuple(vec![
                Build::int(out.status.code().unwrap_or(-1) as i64),
                Build::Str(String::from_utf8_lossy(&out.stdout).into_owned()),
                Build::Str(String::from_utf8_lossy(&out.stderr).into_owned()),
            ])),
            Err(e) => Build::error(e.to_string()),
        },
        "status" => match command(w)?.status() {
            Ok(st) => Build::ok(Build::int(st.code().unwrap_or(-1) as i64)),
            Err(e) => Build::error(e.to_string()),
        },
        "exit" => match w.int_of(arg) {
            Some(code) => w.exit(code as i32),
            None => {
                return Err(format!(
                    "Process.exit: expected an Int, got {}",
                    w.shown(arg)
                ));
            }
        },
        "currentPid" => Build::int(std::process::id() as i64),
        "currentExe" => Build::Str(crate::args::current_exe()),
        "isTerminal" => match w.int_of(arg) {
            Some(fd) => Build::bool(is_terminal(fd)),
            None => {
                return Err(format!(
                    "Process.isTerminal: expected an Int, got {}",
                    w.shown(arg)
                ));
            }
        },
        "argv" => Build::Vector(w.argv().into_iter().map(Build::Str).collect()),
        "getEnv" => match std::env::var(w.text(arg, &what)?) {
            Ok(v) => Build::some(Build::Str(v)),
            Err(_) => Build::none(),
        },
        "setEnv" => {
            let t = w.tuple(arg, 2, &what)?;
            let (k, v) = (w.text(t[0], &what)?, w.text(t[1], &what)?);
            // Safety: a program sets its environment before any of its other
            // threads read it, as it always has.
            unsafe { std::env::set_var(k, v) };
            Build::unit()
        }
        "removeEnv" => {
            let k = w.text(arg, &what)?;
            // Safety: as `setEnv`.
            unsafe { std::env::remove_var(k) };
            Build::unit()
        }
        // How much of the program has been reached: what a fuzzer steers by.
        "covered" => Build::int(w.covered(w.bool_of(arg) == Some(true))),
        op if op.starts_with("ffi") => return ffi(w, op, arg, &what),
        _ => return Ok(None),
    }))
}

/// Calling C, for `Std.Ffi`: see `crate::ffi`. Everything is read out of the
/// program's values before the call, and built after it.
fn ffi<W: World>(
    w: &mut W,
    op: &str,
    arg: W::Value,
    what: &str,
) -> Result<Option<Build<W::Value>>, String> {
    use crate::ffi;
    let answered = Build::answered;
    Ok(Some(match op {
        "ffiOpen" => answered(ffi::open(&w.text(arg, what)?).map(Build::int)),
        "ffiSymbol" => {
            let t = w.tuple(arg, 2, what)?;
            let lib = int(w, t[0], what)?;
            answered(ffi::symbol(lib, &w.text(t[1], what)?).map(Build::int))
        }
        "ffiCall" => {
            let t = w.tuple(arg, 3, what)?;
            let function = int(w, t[0], what)?;
            let Some(ret) = ffi::Ret::from_code(int(w, t[1], what)?) else {
                return Err(format!("{what}: no such kind of answer"));
            };
            let mut args = Vec::new();
            for a in w.seq(t[2], what)? {
                let a = w.tuple(a, 4, what)?;
                args.push(match int(w, a[0], what)? {
                    0 => ffi::Arg::Int(int(w, a[1], what)?),
                    1 => ffi::Arg::Float(float(w, a[2], what)?),
                    2 => ffi::Arg::Str(w.text(a[3], what)?),
                    3 => ffi::Arg::Float32(float(w, a[2], what)?),
                    _ => return Err(format!("{what}: no such kind of argument")),
                });
            }
            answered(
                ffi::call(function, ret, &args).map(|(n, x, s)| {
                    Build::Tuple(vec![Build::int(n), Build::float(x), Build::Str(s)])
                }),
            )
        }
        "ffiAlloc" => Build::int(ffi::alloc(int(w, arg, what)?)),
        "ffiFree" => {
            ffi::free(int(w, arg, what)?);
            Build::unit()
        }
        "ffiWriteInts" => {
            let t = w.tuple(arg, 3, what)?;
            let (at, width) = (int(w, t[0], what)?, int(w, t[1], what)?);
            let mut values = Vec::new();
            for v in w.seq(t[2], what)? {
                values.push(int(w, v, what)?);
            }
            // Safety: the program's word that the memory is its own.
            answered(unsafe { ffi::write_ints(at, width, &values) }.map(|()| Build::unit()))
        }
        "ffiReadInts" => {
            let t = w.tuple(arg, 4, what)?;
            let signed = w.bool_of(t[2]) == Some(true);
            let (at, width, count) = (
                int(w, t[0], what)?,
                int(w, t[1], what)?,
                int(w, t[3], what)?,
            );
            // Safety: as above.
            let read = unsafe { ffi::read_ints(at, width, signed, count) };
            answered(read.map(|ns| Build::Vector(ns.into_iter().map(Build::int).collect())))
        }
        "ffiWriteFloats" => {
            let t = w.tuple(arg, 3, what)?;
            let (at, width) = (int(w, t[0], what)?, int(w, t[1], what)?);
            let mut values = Vec::new();
            for v in w.seq(t[2], what)? {
                values.push(float(w, v, what)?);
            }
            // Safety: as above.
            answered(unsafe { ffi::write_floats(at, width, &values) }.map(|()| Build::unit()))
        }
        "ffiReadFloats" => {
            let t = w.tuple(arg, 3, what)?;
            let (at, width, count) = (
                int(w, t[0], what)?,
                int(w, t[1], what)?,
                int(w, t[2], what)?,
            );
            // Safety: as above.
            let read = unsafe { ffi::read_floats(at, width, count) };
            answered(read.map(|xs| Build::Vector(xs.into_iter().map(Build::float).collect())))
        }
        "ffiReadText" => {
            let t = w.tuple(arg, 2, what)?;
            let (at, len) = (int(w, t[0], what)?, int(w, t[1], what)?);
            // Safety: as above.
            Build::Str(unsafe { ffi::read_text(at, len) })
        }
        // What the last call C made into this library failed with, once;
        // nothing when none did.
        "ffiFailure" => Build::Str(w.ffi_failure()),
        _ => return Ok(None),
    }))
}

/// SplitMix64, seeded once per OS thread from the clock. Not cryptographic,
/// and no promise of repeating: a program that wants repeatable numbers
/// handles the effect with `Std.Random`'s own pure generator, which is why
/// that generator is in the library.
fn random<W: World>(w: &mut W, op: &str, arg: W::Value) -> Result<Option<Build<W::Value>>, String> {
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
    Ok(Some(match op {
        "nextInt" | "nextSeed" => Build::int(next() as i64),
        // [0, 1) from the top 53 bits, which is what an f64 holds exactly.
        "nextFloat" => Build::float((next() >> 11) as f64 / (1u64 << 53) as f64),
        "intBetween" => {
            let what = format!("Random.{op}");
            let t = w.tuple(arg, 2, &what)?;
            match (w.int_of(t[0]), w.int_of(t[1])) {
                (Some(lo), Some(hi)) => {
                    if hi <= lo {
                        Build::int(lo)
                    } else {
                        Build::int(lo + (next() % (hi - lo) as u64) as i64)
                    }
                }
                _ => {
                    return Err(format!(
                        "Random.{op}: expected two Ints, got {}",
                        w.shown(arg)
                    ));
                }
            }
        }
        _ => return Ok(None),
    }))
}

fn time<W: World>(w: &mut W, op: &str, arg: W::Value) -> Result<Option<Build<W::Value>>, String> {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    // One origin for the process, so `monotonic` is a small number that fits
    // an `Int`, and one for every OS thread, since a green thread can start a
    // measurement on one worker and finish it on another.
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    Ok(Some(match op {
        // Wall clock, milliseconds since the Unix epoch.
        "now" => Build::int(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        ),
        // Monotonic, for measuring a duration: unaffected by the clock being
        // set.
        "monotonic" => Build::int(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as i64),
        "sleep" => match w.int_of(arg) {
            Some(ms) if ms > 0 => {
                std::thread::sleep(Duration::from_millis(ms as u64));
                Build::unit()
            }
            Some(_) => Build::unit(),
            None => return Err(format!("Time.sleep: expected an Int, got {}", w.shown(arg))),
        },
        _ => return Ok(None),
    }))
}

/// `Std.Log`'s line, where no handler took it: see `crate::log`.
fn log<W: World>(w: &mut W, op: &str, arg: W::Value) -> Result<Option<Build<W::Value>>, String> {
    match op {
        "logAt" => {
            let what = "Log.logAt";
            let t = w.tuple(arg, 2, what)?;
            let level = int(w, t[0], what)?;
            crate::log::line(level, &w.text(t[1], what)?);
            Ok(Some(Build::unit()))
        }
        _ => Ok(None),
    }
}

/// `Std.Net`'s sockets and `Std.Tls`'s sessions: see `crate::net`. A call that
/// may wait is made a [`Job`] of owned values, which the runtime waits for
/// through [`World::blocking`].
fn net<W: World>(w: &mut W, op: &str, arg: W::Value) -> Result<Option<Build<W::Value>>, String> {
    use crate::net;
    let what = format!("Net.{op}");
    let answered = Build::answered;
    let job: Job<W::Value> = match op {
        "tcpListen" => {
            let t = w.tuple(arg, 2, &what)?;
            let (host, port) = (w.text(t[0], &what)?, int(w, t[1], &what)?);
            return Ok(Some(answered(net::listen(&host, port).map(Build::int))));
        }
        "tcpClose" => {
            net::close(int(w, arg, &what)?);
            return Ok(Some(Build::unit()));
        }
        "tcpTimeout" => {
            let t = w.tuple(arg, 2, &what)?;
            let set = net::set_timeout(int(w, t[0], &what)?, int(w, t[1], &what)?);
            return Ok(Some(answered(set.map(|()| Build::unit()))));
        }
        "tcpLocalPort" => {
            return Ok(Some(answered(
                net::local_port(int(w, arg, &what)?).map(Build::int),
            )));
        }
        "tcpConnect" => {
            let t = w.tuple(arg, 2, &what)?;
            let (host, port) = (w.text(t[0], &what)?, int(w, t[1], &what)?);
            Box::new(move || answered(net::connect(&host, port).map(Build::int)))
        }
        "tcpAccept" => {
            let id = int(w, arg, &what)?;
            Box::new(move || {
                answered(
                    net::accept(id)
                        .map(|(s, peer)| Build::Tuple(vec![Build::int(s), Build::Str(peer)])),
                )
            })
        }
        "tcpRead" => {
            let t = w.tuple(arg, 2, &what)?;
            let (id, max) = (int(w, t[0], &what)?, int(w, t[1], &what)?);
            Box::new(move || answered(net::read(id, max).map(Build::Bytes)))
        }
        "tcpWrite" => {
            let t = w.tuple(arg, 2, &what)?;
            let id = int(w, t[0], &what)?;
            let bytes = w.bytes(t[1], &what)?;
            Box::new(move || answered(net::write(id, &bytes).map(Build::int)))
        }
        "resolve" => {
            let host = w.text(arg, &what)?;
            Box::new(move || {
                answered(
                    net::resolve(&host)
                        .map(|ips| Build::Vector(ips.into_iter().map(Build::Str).collect())),
                )
            })
        }
        "tlsConnect" | "tlsAccept" => {
            let t = w.tuple(arg, 3, &what)?;
            let id = int(w, t[0], &what)?;
            let (a, b) = (w.text(t[1], &what)?, w.text(t[2], &what)?);
            let connecting = op == "tlsConnect";
            Box::new(move || {
                let made = if connecting {
                    net::tls_connect(id, &a, &b)
                } else {
                    net::tls_accept(id, &a, &b)
                };
                answered(made.map(Build::int))
            })
        }
        _ => return Ok(None),
    };
    Ok(Some(w.blocking(job)))
}

/// Whether standard input (0), output (1) or error (2) is a terminal:
/// `Process.isTerminal`, for a program choosing whether to colour what it
/// writes -- for output, a terminal that draws colour, which a Windows
/// console does once asked to (`crate::console::ansi`).
fn is_terminal(fd: i64) -> bool {
    use std::io::IsTerminal;
    match fd {
        0 => std::io::stdin().is_terminal(),
        1 => std::io::stdout().is_terminal() && crate::console::ansi(),
        2 => std::io::stderr().is_terminal() && crate::console::ansi(),
        _ => false,
    }
}
