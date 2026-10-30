//! Programs compiled by `meadow-llvm`, linked with Silo, run --
//! and checked against the AxCut abstract machine, which is the meaning they
//! must keep, and for leaks: a program with no mutable state must end with
//! every block it acquired given back.
//!
//! Needs clang, and builds the runtime library (`aot/`) the first time.

use meadow_compiler::{Options, compile_str_with, core};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn program(src: &str) -> core::Program {
    let (pkg, diags) = compile_str_with("native", src, Options::default().entry("result"));
    let hard: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(hard.is_empty(), "compile errors:\n{}", hard.join("\n"));
    let entry = pkg.value_entry;
    assert!(entry.is_some(), "the case has no `main`");
    core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    }
}

/// The runtime library, built once.
fn runtime() -> &'static Path {
    static LIB: OnceLock<PathBuf> = OnceLock::new();
    LIB.get_or_init(|| {
        let aot = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../silo");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "--quiet", "--manifest-path"])
            .arg(aot.join("Cargo.toml"))
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building Silo failed");
        let lib = if cfg!(windows) {
            "meadow_silo.lib"
        } else {
            "libmeadow_silo.a"
        };
        aot.join("target").join("release").join(lib)
    })
}

fn system_libs() -> &'static [&'static str] {
    if cfg!(windows) {
        &[
            "-lkernel32",
            "-lntdll",
            "-luserenv",
            "-lws2_32",
            "-ldbghelp",
            "-lbcrypt",
            "-ladvapi32",
        ]
    } else if cfg!(target_os = "macos") {
        &["-liconv", "-lSystem"]
    } else {
        &["-lpthread", "-ldl", "-lm"]
    }
}

/// Compile `src` natively, run it, and answer its output and whether it
/// succeeded; and check it against the AxCut machine and for leaks.
#[track_caller]
fn run(name: &str, src: &str) -> String {
    run_checked(name, src, true)
}

#[track_caller]
fn run_checked(name: &str, src: &str, check: bool) -> String {
    run_full(name, src, check, meadow_core::OptLevel::O2).0
}

/// [`run`], and how many blocks the program acquired: what reuse saves.
#[track_caller]
fn run_counting(name: &str, src: &str) -> (String, u64) {
    let (got, stderr) = run_full(name, src, true, meadow_core::OptLevel::O2);
    let acquired = stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("aot: ")?
                .strip_suffix(" blocks acquired")?
                .parse()
                .ok()
        })
        .expect("the run says how many blocks it acquired");
    (got, acquired)
}

/// [`run`], lowered at `O1` as a debug build is: nothing specialized, so a
/// polymorphic definition runs on the descriptors it is passed. Not checked
/// against the AxCut machine, which has no threads.
#[track_caller]
fn run_o1(name: &str, src: &str) -> String {
    run_full(name, src, false, meadow_core::OptLevel::O1).0
}

/// [`run`], compiled as many modules -- a function or so each -- and linked,
/// as a build of a large program is: a call and the function it enters are
/// then in different modules.
#[track_caller]
fn run_split(name: &str, src: &str, check: bool) -> String {
    run_units(name, src, check, meadow_core::OptLevel::O2, 1).0
}

#[track_caller]
fn run_full(name: &str, src: &str, check: bool, opt: meadow_core::OptLevel) -> (String, String) {
    run_units(name, src, check, opt, usize::MAX)
}

#[track_caller]
fn run_units(
    name: &str,
    src: &str,
    check: bool,
    opt: meadow_core::OptLevel,
    unit: usize,
) -> (String, String) {
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, opt);
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    let want = check.then(|| {
        meadow_seq::machine::Machine::run(&lowered.program, 50_000_000)
            .unwrap_or_else(|e| panic!("the AxCut machine failed: {}", e.msg))
            .to_string()
    });
    let units = meadow_llvm::compile_split(&lowered.program, unit, meadow_llvm::CallConv::host())
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let dir = std::env::temp_dir().join("meadow-llvm-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<std::path::PathBuf> = units
        .iter()
        .enumerate()
        .map(|(i, ll)| {
            let path = dir.join(format!("prog{i}.ll"));
            std::fs::write(&path, &ll.text).unwrap();
            path
        })
        .collect();
    let ll_path = paths[0].clone();
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let out = Command::new("clang")
        .args(["-O2", "-Wno-override-module", "-o"])
        .arg(&exe)
        .args(&paths)
        .arg(runtime())
        .args(system_libs())
        .output()
        .expect("clang runs");
    assert!(
        out.status.success(),
        "clang failed on {}:\n{}",
        ll_path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let ran = Command::new(&exe)
        .env("MEADOW_SILO_LEAKS", "1")
        // What went through the runtime's generic entry, on stderr: see
        // `a_method_under_run_st_is_found_before_the_program_runs`.
        .env("MEADOW_SILO_PRIMS", "1")
        .output()
        .expect("the program runs");
    let stdout = String::from_utf8_lossy(&ran.stdout).trim_end().to_string();
    let stderr = String::from_utf8_lossy(&ran.stderr).to_string();
    assert!(
        ran.status.success(),
        "{name} failed: {stderr}\n(see {})",
        ll_path.display()
    );
    let got = if stdout.is_empty() {
        "()".to_string()
    } else {
        stdout
    };
    if let Some(want) = want {
        assert_eq!(got, want, "native vs the AxCut machine, {name}");
    }
    // Every program, a continuation abandoned or not: what its frames held
    // is given up with it (`silo/src/shadow.rs`).
    assert!(
        stderr.contains("aot: 0 blocks live at exit"),
        "{name} leaked: {stderr}"
    );
    (got, stderr)
}

#[test]
fn a_method_under_run_st_is_found_before_the_program_runs() {
    // `check` is generic in its tag, and the loop calls it with the `runSt`'s
    // state -- a type no caller knows -- at a dictionary everyone knows. The
    // method is found where the program is compiled: nothing reads the
    // dictionary at run time, which is what a top-level value's cache would
    // be asked for -- and the runtime counts what it is asked
    // (`MEADOW_SILO_PRIMS`).
    let src = "trait Same a { fun same : a -> a -> Bool }\n\
         impl Same Int { fun same x y = x == y }\n\
         fun check tag x y = same x y\n\
         fun count (r : StRef s Int) (i : Int) : Int ! { St s | e } =\n\
         \x20 if i == 0 then stGetRef r\n\
         \x20 else (let _ = (if check r i 3 then stSetRef r (stGetRef r + 1) else ()) in count r (i - 1))\n\
         def result = runSt (\\() -> count (stNewRef 0) 1000)\n";
    let (got, stderr) = run_full("same-under-st", src, true, meadow_core::OptLevel::O2);
    assert_eq!(got, "1");
    assert!(
        !stderr.contains("GlobalGet"),
        "the dictionary is read at run time:
{stderr}"
    );
}

#[test]
fn aarch64_code_is_never_in_the_ghc_convention() {
    // LLVM gives a GHC-convention function no prologue on aarch64 and does
    // not make its tail calls, so a call there returned to itself for ever.
    // Every function, call and declaration is `tailcc` instead -- and
    // x86-64 keeps `ghccc`.
    let prog = program(
        "fun count (lo : n) (hi : n) : Int =\n\
         \x20 let rec go i k = if i >= hi then k else go (i + 1) (k + 1)\n\
         \x20 in go lo 0\n\
         def result = count 0 10\n",
    );
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O2);
    let text = |arch| {
        meadow_llvm::compile_split(&lowered.program, 1, meadow_llvm::CallConv::for_arch(arch))
            .unwrap_or_else(|e| panic!("{}", e.msg))
            .into_iter()
            .map(|u| u.text)
            .collect::<String>()
    };
    let a64 = text("aarch64");
    assert!(!a64.contains("ghccc"), "ghccc in aarch64 code");
    assert!(
        a64.contains("tail call tailcc"),
        "its tail calls are tailcc"
    );
    assert!(
        a64.contains("declare hidden tailcc"),
        "and so are its declarations"
    );
    let x64 = text("x86_64");
    assert!(!x64.contains("tailcc") && x64.contains("tail call ghccc"));
}

#[test]
fn arithmetic_and_comparison() {
    assert_eq!(
        run(
            "arith",
            "def result = (1 + 2 * 3, 10 - 4, 7 / 2, 7 % 3, _primAddF 2.5 1.0, 3 < 4, 'a' == 'b')"
        ),
        "(7, 6, 3, 1, 3.5, True, False)"
    );
}

#[test]
fn recursion_on_the_native_stack() {
    assert_eq!(
        run(
            "fib",
            "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)
             def result = fib 25"
        ),
        "75025"
    );
}

#[test]
fn data_and_matching() {
    assert_eq!(
        run(
            "data",
            "use L.*
             data L = Nil | Cons Int L
             fun build (n : Int) : L = if n == 0 then Nil else Cons n (build (n - 1))
             fun total (xs : L) : Int = match xs with | Nil -> 0 | Cons x r -> x + total r
             fun len (xs : L) : Int = match xs with | Nil -> 0 | Cons _ r -> 1 + len r
             def result = let xs = build 1000 in (total xs, len xs)"
        ),
        "(500500, 1000)"
    );
}

#[test]
fn view_patterns() {
    // A view that fails goes on to the arms after it; one in a parameter is
    // applied as the function is called.
    assert_eq!(
        run(
            "views",
            "use L.*
             data L = Nil | Cons Int L
             data M = No | Yes Int
             use M.*
             fun half (n : Int) : M = if n % 2 == 0 then Yes (n / 2) else No
             fun steps (n : Int) (acc : Int) : Int = match n with
               | 1 -> acc
               | (half -> Yes h) -> steps h (acc + 1)
               | _ -> steps (3 * n + 1) (acc + 1)
             fun sum (xs : L) : Int = match xs with | Nil -> 0 | Cons x r -> x + sum r
             fun total ((\\n -> Cons n (Cons (n + 1) Nil)) -> xs) = sum xs
             def result = (steps 27 0, total 20)"
        ),
        "(111, 41)"
    );
}

#[test]
fn closures_and_higher_order() {
    assert_eq!(
        run(
            "closures",
            "fun twice f x = f (f x)
             fun compose f g x = f (g x)
             def result =
               let k = 10 in
               let add = \\x -> x + k in
               (twice add 1, compose (\\x -> x * 2) add 5, twice (twice (\\x -> x + 1)) 0)"
        ),
        "(21, 30, 4)"
    );
}

#[test]
fn deep_non_tail_recursion() {
    assert_eq!(
        run(
            "deep",
            "use L.*
             data L = Nil | Cons Int L
             fun build (n : Int) (acc : L) : L = if n == 0 then acc else build (n - 1) (Cons n acc)
             fun total (xs : L) : Int = match xs with | Nil -> 0 | Cons x r -> x + total r
             def result = total (build 1000000 Nil)"
        ),
        "500000500000"
    );
}

/// Not a test: how long `fib 32` takes natively, for comparing with the other
/// backends. `cargo test --release -p meadow-llvm --test native -- --ignored`.
#[test]
#[ignore]
fn time_fib() {
    let started = std::time::Instant::now();
    run_checked(
        "timefib",
        "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)
         def result = fib 32",
        false,
    );
    eprintln!("fib 32, compiled and run: {:?}", started.elapsed());
}

// --- effects: stack segments ---------------------------------------------

#[test]
fn a_handler_that_never_resumes_aborts_the_body() {
    assert_eq!(
        run(
            "abort",
            "effect Abort { bail : () -> Int }
             def result = handle 1 + bail () with { bail u k -> 99 }"
        ),
        "99"
    );
}

#[test]
fn a_handler_that_resumes_continues_the_body_in_place() {
    assert_eq!(
        run(
            "resume",
            "effect Ask { ask : () -> Int }
             def result = handle ask () + 1 with { ask u k -> k 5 + 100 }"
        ),
        "106"
    );
}

#[test]
fn handlers_are_deep() {
    assert_eq!(
        run(
            "deep_handler",
            "effect Ask { ask : () -> Int }
             fun twice u = ask () + ask ()
             def result = (handle ask () + ask () with { ask u k -> k 5 },
                         handle twice () with { ask u k -> k 3 })"
        ),
        "(10, 6)"
    );
}

#[test]
fn state_by_hand_is_a_handler_returning_a_function() {
    assert_eq!(
        run(
            "state",
            "effect St { get : () -> Int, put : Int -> () }
             def result =
               let f =
                 handle
                   let a = get () in
                   let u = put (a + 1) in
                   let b = get () in
                   a + b
                 with {
                   get u k -> \\s -> (k s) s,
                   put v k -> \\s -> (k ()) v,
                   return x -> \\s -> x
                 }
               in f 10"
        ),
        "21"
    );
}

#[test]
fn nested_handlers_and_performing_outwards() {
    assert_eq!(
        run(
            "nested",
            "effect Ask { ask : () -> Int }
             def result = handle (handle ask () with { ask u k -> k 1 }) + ask () with { ask u k -> k 100 }"
        ),
        "101"
    );
    assert_eq!(
        run(
            "outwards",
            "effect Yield { yield : Int -> () }
             data L = Nil | Cons Int L
             use L.*
             fun range (lo : Int) (hi : Int) = if lo >= hi then () else let _ = yield lo in range (lo + 1) hi
             fun map f producer =
               handle producer () with { yield x k -> let _ = yield (f x) in k (), return r -> () }
             fun toList producer =
               handle producer () with { yield x k -> Cons x (k ()), return r -> Nil }
             def result = toList (\\() -> map (\\x -> x * 2) (\\() -> range 0 4))"
        ),
        "Cons(0, Cons(2, Cons(4, Cons(6, Nil))))"
    );
}

/// What a program does happens in the order it is written: the program every
/// engine runs and must answer alike (`glade/tests/differential.rs`), whose
/// numbers are the order each construct's parts were evaluated in. See
/// "Evaluation order" in `docs/TUTORIAL.md`.
#[test]
fn everything_is_evaluated_in_the_order_it_is_written() {
    let src = include_str!("../../../glade/tests/evaluation_order.mw");
    let want = "(123, 123, 123, 12, 12, 123, 123, 12, 12, 123, 12, 12, 12, \
                1234, 12, 1234, 123, 12, 12, 12, 122, 1234567, 12)";
    assert_eq!(run("evaluation_order", src), want);
    assert_eq!(run_o1("evaluation_order_o1", src), want);
}

// --- threads -----------------------------------------------------------------
//
#[test]
fn aarch64_passes_nothing_on_the_stack() {
    // `tailcc` on aarch64 passes eight words in registers and would pass the
    // rest on the stack, which LLVM gets wrong there: a caller passing one
    // word on the stack keeps a local beside it, in memory the callee owns
    // and its tail call overwrites -- `Lingua.Query`'s `revision`, read before
    // a `set`, came back as an address -- and at `-O0` the caller loses
    // track of the stack pointer altogether. So no function takes more than
    // eight words, the context among them, and the rest go through the
    // spill area; x86-64, with ten registers, is as it was.
    //
    // Seven parameters, the word every function is passed after them, and
    // the context a program that spawns hands on: nine.
    let src = "fun seven (a : Int) (b : Int) (c : Int) (d : Int) (e : Int) (f : Int) (g : Int) : Int =\n\
         \x20 if a == 0 then b + c + d + e + f + g else seven (a - 1) b c d e f g\n\
         def result = let t = threadSpawn (\\() -> 1) in threadAwait t + seven 3 1 1 1 1 1 1\n";
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O1);
    let most = |arch, unit| {
        meadow_llvm::compile_split(
            &lowered.program,
            unit,
            meadow_llvm::CallConv::for_arch(arch),
        )
        .unwrap_or_else(|e| panic!("{}", e.msg))
        .into_iter()
        .map(|u| u.text)
        .collect::<String>()
        .lines()
        .filter(|l| l.contains("tailcc i64 ") || l.contains("ghccc i64 "))
        .map(|l| {
            l.rsplit_once('(')
                .map_or(0, |(_, args)| args.matches("i64").count())
        })
        .max()
    };
    // One module, and one a function: the second declares what it calls.
    for unit in [usize::MAX, 1] {
        assert_eq!(most("aarch64", unit), Some(8), "words to a call on aarch64");
        assert_eq!(most("x86_64", unit), Some(9), "words to a call on x86-64");
    }
    for opt in [meadow_core::OptLevel::O0, meadow_core::OptLevel::O1] {
        let name = format!("nine_words_{opt:?}");
        assert_eq!(run_full(&name, src, false, opt).0, "7");
    }
}

// The AxCut machine has no scheduler, so these are checked by their answers.

#[test]
fn threads_answer_through_await() {
    assert_eq!(
        run_checked(
            "spawn",
            "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)
             def result =
               let a = threadSpawn (\\() -> fib 15) in
               let b = threadSpawn (\\() -> (fib 16, 1)) in
               (threadAwait a, threadAwait b, threadAwait a)",
            false
        ),
        "(610, (987, 1), 610)"
    );
}

#[test]
fn a_receiver_waits_for_a_sender() {
    assert_eq!(
        run_checked(
            "channel",
            "def result =
               let ch = channelNew () in
               let reader = threadSpawn (\\() -> channelReceive ch + channelReceive ch) in
               let _ = threadYield () in
               let _ = channelSend ch (toInt 20) in
               let _ = channelSend ch 22 in
               threadAwait reader",
            false
        ),
        "42"
    );
}

#[test]
fn a_thread_waits_inside_a_handler() {
    assert_eq!(
        run_checked(
            "wait_in_handler",
            "effect Ask { ask : () -> Int }
             def result =
               let ch = channelNew () in
               let t = threadSpawn (\\() ->
                 handle (let x = channelReceive ch in x + ask ()) with { ask u k -> k 1 + 100 }) in
               let _ = threadYield () in
               let _ = channelSend ch (toInt 41) in
               threadAwait t",
            false
        ),
        "142"
    );
}

/// A thread that waits under two handlers and then performs the outer one's
/// operation: each handler's frame parks the segment it was driving while the
/// thread waits, and the outer one resumes before the inner one has taken
/// its own back. It counted that one as beneath it, and the operation that
/// followed cut the list of parked segments past its end.
#[test]
fn a_thread_waits_inside_two_handlers() {
    // The thread says it has started, and `main` works a while before it
    // sends, so that the thread is waiting by then.
    assert_eq!(
        run_checked(
            "wait_in_two_handlers",
            "effect Ask { ask : () -> Int }
             effect Tell { tell : () -> Int }
             fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)
             def result =
               let ch = channelNew () in
               let started = channelNew () in
               let t = threadSpawn (\\() ->
                 handle
                   (let x =
                      handle
                        (let _ = channelSend started (toInt 0) in channelReceive ch + tell ())
                      with { tell u k -> k 1 + 1000 } in
                    x + ask () + ask ())
                 with { ask u k -> k 1 + 100 }) in
               let _ = channelReceive started in
               let _ = fib 27 in
               let _ = channelSend ch (toInt 40) in
               threadAwait t",
            false
        ),
        "1243"
    );
}

// --- cancellation ------------------------------------------------------------
//
// A cancelled thread fails at its next thread operation, and a waiting one is
// woken to fail: see `meadow_core::thread`. Each program is run on one worker,
// where a spawned thread waits for `main` to stop before it starts, and on
// every core, where it does not; the answers are arranged to be the same.

// `Std.Stm`'s handlers, as `glade/tests/differential.rs` has them: these
// programs are compiled without the standard library.
const STM: &str = "use Maybe.*\ndata Maybe a = None | Just a
    effect Stm { retrySignal : () -> (), conflictSignal : () -> () }
    use Attempt.*\ndata Attempt a = Done a | Retried | Conflicted
    fun unreachable u = unreachable u
    fun readTVar tv = match stmRead tv with | Just x -> x | None -> let _ = conflictSignal () in unreachable ()
    fun retry u = let _ = retrySignal () in unreachable ()
    fun atomically (f : () -> a ! { Stm }) =
      let _ = stmBegin () in
      match handle Done (f ()) with { retrySignal u k -> Retried, conflictSignal u k -> Conflicted } with
      | Done x -> if stmCommit () then x else atomically f
      | Retried -> let _ = stmWait () in atomically f
      | Conflicted -> atomically f\n";

/// Compile `src`, run it under the leak check once per worker count, and
/// answer each run: whether it succeeded, what it printed, and its stderr.
#[track_caller]
fn run_cancelling(name: &str, src: &str, env: &[(&str, &str)]) -> Vec<(bool, String, String)> {
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O2);
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    let units =
        meadow_llvm::compile_split(&lowered.program, usize::MAX, meadow_llvm::CallConv::host())
            .unwrap_or_else(|e| panic!("{}", e.msg));
    let dir = std::env::temp_dir().join("meadow-llvm-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = units
        .iter()
        .enumerate()
        .map(|(i, ll)| {
            let path = dir.join(format!("prog{i}.ll"));
            std::fs::write(&path, &ll.text).unwrap();
            path
        })
        .collect();
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let out = Command::new("clang")
        .args(["-O2", "-Wno-override-module", "-o"])
        .arg(&exe)
        .args(&paths)
        .arg(runtime())
        .args(system_libs())
        .output()
        .expect("clang runs");
    assert!(
        out.status.success(),
        "clang failed on {}:\n{}",
        paths[0].display(),
        String::from_utf8_lossy(&out.stderr)
    );
    ["1", "8"]
        .iter()
        .map(|workers| {
            let mut cmd = Command::new(&exe);
            cmd.env("MEADOW_SILO_LEAKS", "1")
                .env("MEADOW_THREADS", workers);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let ran = cmd.output().expect("the program runs");
            (
                ran.status.success(),
                String::from_utf8_lossy(&ran.stdout).trim_end().to_string(),
                String::from_utf8_lossy(&ran.stderr).to_string(),
            )
        })
        .collect()
}

/// [`run_cancelling`], for a program that must fail with `want` every time.
#[track_caller]
fn fails_cancelling(name: &str, src: &str, want: &str) {
    for (ok, _, stderr) in run_cancelling(name, src, &[]) {
        assert!(!ok, "{name} should have failed");
        assert_eq!(stderr.trim_end(), want, "{name}");
    }
}

/// [`run_cancelling`], for a program that must answer `want` every time, and
/// leave nothing behind: no block live, and on one worker -- where the threads
/// it cancelled run, and end, before `main` runs again after it yields -- no
/// segment either. On eight, a thread may still be on its way out when `main`
/// ends the program, its segments with it.
#[track_caller]
fn answers_cancelling(name: &str, src: &str, want: &str, env: &[(&str, &str)]) -> Vec<String> {
    run_cancelling(name, src, env)
        .into_iter()
        .enumerate()
        .map(|(i, (ok, got, stderr))| {
            assert!(ok, "{name} failed: {stderr}");
            assert_eq!(got, want, "{name}");
            assert_eq!(live_at_exit(&stderr), 0, "{name}: {stderr}");
            if i == 0 {
                assert_eq!(
                    said(&stderr, " segments live at exit"),
                    Some(0),
                    "{name}: {stderr}"
                );
            }
            stderr
        })
        .collect()
}

#[test]
fn a_cancelled_thread_fails_and_so_does_its_await() {
    let cancelled = meadow_core::thread::CANCELLED;
    // On a channel: waiting, or not yet there.
    fails_cancelling(
        "cancel_receive",
        "def result =
           let ch = channelNew () in
           let t = threadSpawn (\\() -> channelReceive ch + toInt 1) in
           let _ = threadCancel t in
           let _ = channelSend ch (toInt 1) in
           threadAwait t",
        cancelled,
    );
    // On a thread that never finishes: only the cancel ends the wait.
    fails_cancelling(
        "cancel_await",
        "def result =
           let gate = channelNew () in
           let blocker = threadSpawn (\\() -> channelReceive gate) in
           let waiter = threadSpawn (\\() -> threadAwait blocker + toInt 1) in
           let _ = threadYield () in
           let _ = threadCancel waiter in
           threadAwait waiter",
        cancelled,
    );
    // In a transaction's `retry`, on a `TVar` nobody writes.
    fails_cancelling(
        "cancel_retry",
        &format!(
            "{STM}def result =
               let box = stmNewIO (toInt 0) in
               let t = threadSpawn (\\() -> atomically (\\() -> if readTVar box == 0 then retry () else readTVar box)) in
               let _ = threadYield () in
               let _ = threadCancel t in
               threadAwait t"
        ),
        cancelled,
    );
    // Under two handlers, neither of which can catch it.
    fails_cancelling(
        "cancel_in_handlers",
        "effect Ask { ask : () -> Int }
         effect Exn { throw : Int -> () }
         def result =
           let ch = channelNew () in
           let t = threadSpawn (\\() ->
             handle
               (handle (let x = channelReceive ch in x + ask ())
                with { ask u k -> k 1 + 100, return r -> r * 1000 })
             with { throw e k -> 0 - 1 }) in
           let _ = threadYield () in
           let _ = threadCancel t in
           threadAwait t",
        cancelled,
    );
    // Asking does not use the cancel up: the next operation still fails.
    fails_cancelling(
        "cancel_after_asking",
        "fun spin (n : Int) = if threadCancelled () then let _ = threadYield () in n else spin (n + 1)
         def result =
           let t = threadSpawn (\\() -> spin 0) in
           let _ = threadCancel t in
           threadAwait t",
        cancelled,
    );
}

/// Waiting threads of every kind cancelled at once -- on a channel, on
/// another thread, in `retry`, inside a handler -- each holding a list of its
/// own. Each is woken and fails, its heap let go whole; `main` takes back the
/// message none of them may have taken, and goes on.
const CANCEL_MANY: &str = "use L.*\ndata L = Nil | Cons Int L
     use Ts.*\ndata Ts = End | More (Task Int) Ts
     effect Ask { ask : () -> Int }
     fun build (n : Int) = if n == 0 then Nil else Cons n (build (n - 1))
     fun len xs = match xs with | Nil -> toInt 0 | Cons _ r -> 1 + len r
     fun cancelAll ts = match ts with | End -> () | More t rest -> let _ = threadCancel t in cancelAll rest
     fun yields (n : Int) = if n == 0 then () else let _ = threadYield () in yields (n - 1)
     fun readers gate (n : Int) acc = if n == 0 then acc else
       readers gate (n - 1) (More (threadSpawn (\\() -> let xs = build 50 in channelReceive gate + len xs)) acc)
     fun waiters blocker (n : Int) acc = if n == 0 then acc else
       waiters blocker (n - 1) (More (threadSpawn (\\() ->
         handle (let xs = build 50 in threadAwait blocker + len xs + ask ()) with { ask u k -> k 1 })) acc)
     fun retriers box (n : Int) acc = if n == 0 then acc else
       retriers box (n - 1) (More (threadSpawn (\\() ->
         let xs = build 50 in atomically (\\() -> if readTVar box == 0 then retry () else readTVar box) + len xs)) acc)
     def result =
       let gate = channelNew () in
       let hold = channelNew () in
       let box = stmNewIO (toInt 0) in
       let blocker = threadSpawn (\\() -> channelReceive hold) in
       let rs = readers gate 100 End in
       let ws = waiters blocker 100 End in
       let ts = retriers box 100 End in
       let _ = yields 3 in
       let _ = cancelAll rs in
       let _ = cancelAll ws in
       let _ = cancelAll ts in
       let _ = channelSend gate (toInt 42) in
       let mine = channelReceive gate in
       let _ = channelSend hold (toInt 1) in
       let _ = atomically (\\() -> stmWrite box 5) in
       let _ = yields 3 in
       (mine, threadAwait blocker, atomically (\\() -> readTVar box))";

#[test]
fn many_threads_cancelled_at_once_leave_nothing_behind() {
    let src = format!("{STM}{CANCEL_MANY}");
    answers_cancelling("cancel_many", &src, "(42, 1, 5)", &[]);
    for stderr in answers_cancelling(
        "cancel_many_poisoned",
        &src,
        "(42, 1, 5)",
        &[("MEADOW_SILO_POISON", "ptr")],
    ) {
        poison_clean(&stderr);
    }
}

#[test]
fn a_cancel_a_thread_deals_with_itself_changes_nothing_else() {
    // Finished before it was cancelled; and asking, and returning what it
    // had -- which spares it being ended at a safe point.
    let src = "fun spin (n : Int) = if threadCancelled () then n else spin (n + 1)
         def result =
           let done = threadSpawn (\\() -> toInt 7) in
           let first = threadAwait done in
           let _ = threadCancel done in
           let _ = threadCancel done in
           let asks = threadSpawn (\\() -> spin 0) in
           let _ = threadCancel asks in
           (first, threadAwait done, threadAwait asks >= 0, threadCancelled ())";
    answers_cancelling("cancel_unnoticed", src, "(7, 7, True, False)", &[]);
}

#[test]
fn a_thread_in_a_pure_loop_is_ended_at_a_safe_point() {
    let cancelled = meadow_core::thread::CANCELLED;
    // Alone: nothing else is ready, so only the cancel has it look.
    fails_cancelling(
        "cancel_pure_loop",
        "fun spin (n : Int) : Int = spin (n + 1)
         def result =
           let t = threadSpawn (\\() -> spin 0) in
           let _ = threadYield () in
           let _ = threadCancel t in
           threadAwait t",
        cancelled,
    );
    // Inside a handler, whose segment goes with it.
    fails_cancelling(
        "cancel_pure_loop_in_handler",
        "effect Ask { ask : () -> Int }
         fun spin (n : Int) : Int = spin (n + 1)
         def result =
           let t = threadSpawn (\\() -> handle spin 0 with { ask u k -> k 1 }) in
           let _ = threadYield () in
           let _ = threadCancel t in
           threadAwait t",
        cancelled,
    );
}

#[test]
fn threads_ended_at_safe_points_leave_nothing_behind() {
    // Twenty threads in loops that hold lists, cancelled; `main` works on a
    // while, and its answer is all there is to see.
    let src = "use L.*\ndata L = Nil | Cons Int L
         use Ts.*\ndata Ts = End | More (Task Int) Ts
         fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)
         fun build (n : Int) = if n == 0 then Nil else Cons n (build (n - 1))
         fun len xs = match xs with | Nil -> toInt 0 | Cons _ r -> 1 + len r
         fun churn (n : Int) : Int = churn (len (build 20) + n % 7)
         fun loopers (n : Int) acc = if n == 0 then acc else loopers (n - 1) (More (threadSpawn (\\() -> churn n)) acc)
         fun cancelAll ts = match ts with | End -> () | More t rest -> let _ = threadCancel t in cancelAll rest
         fun wait (k : Int) (acc : Int) = if k == 0 then acc else let _ = threadYield () in wait (k - 1) (acc + fib 22)
         def result =
           let ts = loopers 20 End in
           let _ = threadYield () in
           let _ = cancelAll ts in
           wait 40 0";
    answers_cancelling("cancel_loopers", src, "708440", &[]);
}

// --- sleeping on the scheduler ---------------------------------------------------
//
// `Time.sleep` parks its thread on the scheduler's timer, holding up no
// worker: see `docs/CANCELLATION.md`.

const TIME: &str = "effect Time { sleep : Int -> () }\n";

#[test]
fn ten_thousand_sleepers_hold_up_no_worker() {
    // A fifth of a second each: one after another on one worker, that would
    // be over half an hour.
    let src = format!(
        "{TIME}use Ts.*\ndata Ts = End | More (Task Int) Ts
         fun sleepers (n : Int) acc = if n == 0 then acc else
           sleepers (n - 1) (More (threadSpawn (\\() -> let _ = sleep 200 in n)) acc)
         fun sumAll ts acc = match ts with | End -> acc | More t rest -> sumAll rest (acc + threadAwait t)
         def result = sumAll (sleepers 10000 End) 0"
    );
    let began = std::time::Instant::now();
    answers_cancelling("sleepers", &src, "50005000", &[]);
    for stderr in answers_cancelling(
        "sleepers_poisoned",
        &src,
        "50005000",
        &[("MEADOW_SILO_POISON", "ptr")],
    ) {
        poison_clean(&stderr);
    }
    let took = began.elapsed();
    assert!(took < std::time::Duration::from_secs(120), "took {took:?}");
}

#[test]
fn a_sleeping_thread_is_woken_by_a_cancel() {
    let began = std::time::Instant::now();
    // An hour's sleep, which only the cancel ends in time; and one cancelled
    // before it goes to sleep.
    for (name, pause) in [
        ("cancel_sleeper", "let _ = sleep 20 in"),
        ("cancel_sleeper_early", ""),
    ] {
        fails_cancelling(
            name,
            &format!(
                "{TIME}def result =
                   let t = threadSpawn (\\() -> let _ = sleep 3600000 in toInt 1) in
                   {pause}
                   let _ = threadCancel t in
                   threadAwait t"
            ),
            meadow_core::thread::CANCELLED,
        );
    }
    // A sleeper woken so is no longer one that will wake: what is left
    // waiting is a deadlock, said at once.
    for (ok, _, stderr) in run_cancelling(
        "cancel_sleeper_deadlock",
        &format!(
            "{TIME}def result =
               let ch = channelNew () in
               let t = threadSpawn (\\() -> let _ = sleep 3600000 in toInt 1) in
               let _ = sleep 20 in
               let _ = threadCancel t in
               channelReceive ch"
        ),
        &[],
    ) {
        assert!(!ok);
        assert!(stderr.contains("deadlock"), "{stderr}");
    }
    // While a sleeper is asleep, nobody else running is not a deadlock.
    answers_cancelling(
        "sleeper_not_deadlock",
        &format!(
            "{TIME}def result =
               let ch = channelNew () in
               let t = threadSpawn (\\() -> let _ = sleep 30 in channelSend ch (toInt 5)) in
               let _ = threadAwait t in
               channelReceive ch + toInt 1"
        ),
        "6",
        &[],
    );
    let took = began.elapsed();
    assert!(took < std::time::Duration::from_secs(120), "took {took:?}");
}

// --- cycles ------------------------------------------------------------------
//
// Counting alone leaves a cycle behind, so the runtime collects them by trial
// deletion (`silo/src/cycles.rs`). The leak check in `run` is what these are
// really testing: a program that ties knots and drops them must end with
// nothing live.

#[test]
fn a_cycle_through_a_ref_is_collected() {
    assert_eq!(
        run_checked(
            "cycle_ref",
            "data L = Nil | Node Int (Ref L)
             use L.*
             fun knot (n : Int) =
               let r = newRef Nil in
               let _ = setRef r (Node n r) in
               ()
             fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
             def result = many 2000",
            false
        ),
        "0"
    );
}

#[test]
fn what_a_garbage_cycle_holds_that_cannot_cycle_goes_with_it() {
    // Each knot holds a string, and an array of numbers beside it: neither is
    // walked by the collector -- a string points at nothing, and an array of
    // numbers can hold no reference -- so the string is let go of when the
    // cycle is freed, and the leak check at exit finds nothing left.
    assert_eq!(
        run_checked(
            "cycle_leaves",
            "data L = Nil | Node String (Array Int) (Ref L)
             use L.*
             fun knot (n : Int) =
               let r = newRef Nil in
               let _ = setRef r (Node (show n) #[n, n + 1] r) in
               ()
             fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
             def result = many 2000",
            false
        ),
        "0"
    );
}

#[test]
fn a_cycle_through_a_mutable_array_is_collected() {
    // The array starts out holding nothing a cycle could run through, and is
    // then given a node that holds the array: from then on the collector
    // walks it, and the knot is found.
    assert_eq!(
        run_checked(
            "cycle_array",
            "data Opt a = No | Some a
             use Opt.*
             data N s = N (StArray s (Opt (N s))) Int
             fun knot (n : Int) : Int =
               runSt (\\() ->
                 let a = stNewArray 2 No in
                 let _ = stSetArray a 0 (Some (N a n)) in
                 n)
             fun many (n : Int) (acc : Int) : Int = if n == 0 then acc else many (n - 1) (acc + knot n)
             def result = many 2000 0",
            false
        ),
        "2001000"
    );
}

#[test]
fn a_longer_cycle_is_collected() {
    // Two cells and two nodes to a knot, so the collector has a subgraph to
    // walk rather than a self-reference.
    assert_eq!(
        run_checked(
            "cycle_long",
            "data L = Nil | Node Int (Ref L)
             use L.*
             fun knot (n : Int) =
               let a = newRef Nil in
               let b = newRef Nil in
               let _ = setRef a (Node n b) in
               let _ = setRef b (Node n a) in
               ()
             fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
             def result = many 2000",
            false
        ),
        "0"
    );
}

#[test]
fn a_cycle_still_in_use_is_kept() {
    // The knot is held across thousands of allocations, which is long enough
    // for the collector to have run over it several times. If it collected
    // what is still reachable, this reads freed memory or answers wrongly.
    assert_eq!(
        run_checked(
            "cycle_live",
            "data L = Nil | Node Int (Ref L)
             use L.*
             fun churn (n : Int) (acc : Int) : Int ! Mut =
               if n == 0 then acc
               else
                 let r = newRef Nil in
                 let _ = setRef r (Node n r) in
                 churn (n - 1) (acc + 1)
             fun step (r : Ref L) : Int ! Mut =
               match getRef r with
               | Nil -> 0
               | Node v back -> match getRef back with | Nil -> v | Node w _ -> v + w
             def result =
               let keep = newRef Nil in
               let _ = setRef keep (Node 7 keep) in
               let n = churn 5000 0 in
               n + step keep",
            false
        ),
        "5014"
    );
}

#[test]
fn a_cycle_of_more_candidates_than_a_run_takes_is_collected_whole() {
    // A ring of 300 cells, each a candidate when the knot is let go: a run
    // takes 64 of them, and its walk reaches the rest, waiting beyond its
    // batch. Those were left white, pointing at the blocks the run freed --
    // memory a later run walked once it had been handed out again, which
    // crashed a compiler written in Meadow one run in six. The runtime checks
    // under the leak check that no run leaves one so.
    assert_eq!(
        run_checked(
            "cycle_ring",
            "data L = Nil | Node Int (Ref L)
             use L.*
             fun link (first : Ref L) (prev : Ref L) (n : Int) : () ! Mut =
               if n == 0 then setRef prev (Node 0 first)
               else
                 let r = newRef Nil in
                 let _ = setRef prev (Node n r) in
                 link first r (n - 1)
             fun ring (n : Int) : () ! Mut =
               let first = newRef Nil in
               link first first n
             fun many (k : Int) (acc : Int) : Int ! Mut =
               if k == 0 then acc else let _ = ring 300 in many (k - 1) (acc + 1)
             def result = many 60 0",
            false
        ),
        "60"
    );
}

// --- reuse ---------------------------------------------------------------------

const LIST: &str = "use L.*
     data L = Nil | Cons Int L
     fun build (n : Int) (acc : L) : L = if n == 0 then acc else build (n - 1) (Cons n acc)
     fun inc (xs : L) : L = match xs with | Nil -> Nil | Cons x rest -> Cons (x + 1) (inc rest)
     fun total (xs : L) : Int = match xs with | Nil -> 0 | Cons x rest -> x + total rest
";

/// The parameters of `name`'s direct entry point, as borrowed or owned: in
/// the block itself, and then in each copy of it.
fn borrowed_params_all(src: &str, name: &str) -> Vec<Vec<bool>> {
    let pruned = core::prune::prune(&program(src));
    let lowered = meadow_seq::lower_program(&pruned, meadow_core::OptLevel::O2);
    let entry = lowered.program.entry.expect("an entry point");
    let blocks = meadow_llvm::linear::program(&lowered.program, &[entry]).unwrap();
    let defs = &lowered.program.defs;
    let Some(i) = (0..defs.len())
        .filter(|i| &*defs[*i].name.to_string() == name)
        .max_by_key(|i| defs[*i].block.params.len())
    else {
        return Vec::new();
    };
    let mut found: Vec<(bool, Vec<bool>)> = blocks
        .iter()
        .filter(|(d, _, _)| *d == i)
        .map(|(_, l, b)| (*l != defs[i].label, b.borrowed.clone()))
        .collect();
    found.sort();
    found.into_iter().map(|(_, b)| b).collect()
}

/// The parameters of `name`'s direct entry point, as borrowed or owned.
fn borrowed_params(src: &str, name: &str) -> Vec<bool> {
    borrowed_params_all(src, name)
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[test]
fn a_list_only_read_is_borrowed_and_one_rebuilt_is_owned() {
    // `total` reads its list and hands its tail on: the caller's reference
    // is all it needs, and walking the list counts nothing. `inc` rebuilds
    // the list in the cells it takes apart, which it can only do owning them.
    let src = format!("{LIST} def result = let xs = build 100 Nil in total xs + total (inc xs)");
    assert_eq!(
        borrowed_params(&src, "total").first(),
        Some(&true),
        "total borrows its list"
    );
    assert_eq!(
        borrowed_params(&src, "inc").first(),
        Some(&false),
        "inc owns its list"
    );
    // And what it computes is what it did, with nothing left over.
    assert_eq!(run("borrow-total", &src), "10200");
}

#[test]
fn a_list_handed_over_for_good_goes_to_a_copy_that_owns_it() {
    // `total (inc xs)` gives `total` a list nobody will use again. Held by
    // the caller, it would be walked twice -- summed, then freed; a copy of
    // `total` that owns its list frees each cell as it sums it, and calls
    // itself on the tail it now owns.
    let src = format!("{LIST} def result = let xs = build 100 Nil in total xs + total (inc xs)");
    let all = borrowed_params_all(&src, "total");
    assert_eq!(all.len(), 2, "total and one copy: {all:?}");
    assert_eq!(all[0].first(), Some(&true), "total borrows its list");
    assert_eq!(all[1].first(), Some(&false), "the copy owns it");
    assert_eq!(run("borrow-copy", &src), "10200");
}

#[test]
fn a_list_nobody_else_holds_is_rebuilt_in_place() {
    // Each `inc` takes a cell apart and builds one of the same size: with the
    // list its only reference, it builds in the cell it took apart. Two
    // passes over a thousand cells acquire none beyond the thousand built.
    let (got, acquired) = run_counting(
        "reuse-map",
        &format!("{LIST} def result = total (inc (inc (build 1000 Nil)))"),
    );
    assert_eq!(got, "502500");
    assert!(
        acquired < 1100,
        "{acquired} blocks acquired: the passes did not reuse"
    );
}

#[test]
fn a_list_still_held_elsewhere_is_copied() {
    // Shared, the cells are not `inc`'s to reuse: it copies, and the list it
    // was given is still whole for the second `total`.
    let (got, acquired) = run_counting(
        "reuse-shared",
        &format!("{LIST} def result = let xs = build 1000 Nil in total (inc xs) + total xs"),
    );
    assert_eq!(got, "1002000");
    assert!(
        acquired >= 2000,
        "{acquired}: a shared list cannot have been reused"
    );
}

#[test]
fn a_record_nobody_else_holds_is_updated_in_place() {
    // The type says where `n` and `total` are, and the record is the loop's
    // alone: each update writes the field where it is, acquiring nothing.
    let (got, acquired) = run_counting(
        "record-update",
        "fun go (st : { n : Int, total : Int, name : String }) : { n : Int, total : Int, name : String } =\n\
         \x20 if st.n == 0 then st else go { st | n = st.n - 1, total = st.total + st.n }\n\
         def result = let r = go { name = \"sum\", n = 1000, total = 0 } in (r.name, r.total)",
    );
    assert_eq!(got, "(\"sum\", 500500)");
    assert!(
        acquired < 100,
        "{acquired} blocks acquired: the updates copied"
    );
}

#[test]
fn a_record_still_held_elsewhere_is_copied_by_an_update() {
    let got = run(
        "record-update-shared",
        "def result = let a = { x = 1, ys = [1; 2], z = \"z\" } in\n\
         \x20 let b = { a | ys = [3;] } in\n\
         \x20 let c = { b | x = 5, z = \"c\" } in\n\
         \x20 (a.x, a.ys, a.z, b.x, b.ys, b.z, c.x, c.ys, c.z)",
    );
    assert_eq!(got, "(1, [1; 2], \"z\", 1, [3], \"z\", 5, [3], \"c\")");
}

#[test]
fn a_path_that_builds_nothing_gives_the_block_back() {
    // `keep` builds a cell on one path and none on the other: the cells it
    // drops go back to the runtime, and nothing leaks.
    let (got, _) = run_counting(
        "reuse-filter",
        &format!(
            "{LIST} fun keep (xs : L) : L = match xs with | Nil -> Nil | Cons x rest -> if x % 2 == 0 then Cons x (keep rest) else keep rest
             def result = total (keep (build 1000 Nil))"
        ),
    );
    assert_eq!(got, "250500");
}

#[test]
fn a_block_is_rebuilt_after_the_calls_it_waits_on() {
    // `bump` takes a node apart, recurses into both sides -- two calls that
    // are not tail calls -- and only then builds the node again. The block
    // it took apart waits in the frames of those calls, and the node is built
    // in it when they return.
    let (got, acquired) = run_counting(
        "reuse-across-calls",
        "use T.*
         data T = Tip | Node T Int T
         fun build (d : Int) : T = if d == 0 then Tip else Node (build (d - 1)) d (build (d - 1))
         fun bump (t : T) : T = match t with | Tip -> Tip | Node l x r -> let l2 = bump l in let r2 = bump r in Node l2 (x + 1) r2
         fun sum (t : T) : Int = match t with | Tip -> 0 | Node l x r -> sum l + x + sum r
         def result = sum (bump (bump (build 10)))",
    );
    assert_eq!(got, "4082");
    assert!(
        acquired < 1100,
        "{acquired} blocks acquired for a tree of 1023: the rebuilds did not reuse"
    );
}

#[test]
fn nested_patterns_rebuild_in_what_they_took_apart() {
    // Koka's red-black insertion (its `rbtree` benchmark), with the nested
    // patterns it is written with. The matches are decision trees, so each
    // `switch` consumes what it takes apart and the nodes a rotation builds
    // are the nodes it matched; a chain of failure objects would have held
    // every scrutinee while the arm ran, and nothing could have been reused.
    // `balanceLeft` is inlined into `ins`, its one caller, so a rotation
    // builds in the node `ins` took apart as well as the two it matched; and
    // where `ins` keeps `t` for the path that returns it, the other paths drop
    // it into a token. Two thousand insertions copy a path of a dozen nodes
    // each without reuse -- over forty thousand blocks -- and with it build
    // nothing but the two thousand leaves.
    let (got, acquired) = run_counting(
        "reuse-rbtree",
        "use Color.*
         use Tree.*
         data Color = Red | Black
         data Tree = Leaf | Node Color Tree Int Tree
         fun isRed (t : Tree) : Bool = match t with | Node Red _ _ _ -> True | _ -> False
         fun balanceLeft (l : Tree) (k : Int) (r : Tree) : Tree = match l with | Node _ (Node Red a x b) y c -> Node Red (Node Black a x b) y (Node Black c k r) | Node _ a x (Node Red b y c) -> Node Red (Node Black a x b) y (Node Black c k r) | Node _ a x b -> Node Black (Node Red a x b) k r | Leaf -> Leaf
         fun balanceRight (l : Tree) (k : Int) (r : Tree) : Tree = match r with | Node _ (Node Red a x b) y c -> Node Red (Node Black l k a) x (Node Black b y c) | Node _ a x (Node Red b y c) -> Node Red (Node Black l k a) x (Node Black b y c) | Node _ a x b -> Node Black l k (Node Red a x b) | Leaf -> Leaf
         fun ins (t : Tree) (k : Int) : Tree = match t with | Node Red l x r -> (if k < x then Node Red (ins l k) x r else if k > x then Node Red l x (ins r k) else t) | Node Black l x r -> (if k < x then (if isRed l then balanceLeft (ins l k) x r else Node Black (ins l k) x r) else if k > x then (if isRed r then balanceRight l x (ins r k) else Node Black l x (ins r k)) else t) | Leaf -> Node Red Leaf k Leaf
         fun insert (t : Tree) (k : Int) : Tree = match ins t k with | Node _ l x r -> Node Black l x r | Leaf -> Leaf
         fun make (n : Int) (t : Tree) : Tree = if n == 0 then t else make (n - 1) (insert t n)
         fun size (t : Tree) : Int = match t with | Leaf -> 0 | Node _ l _ r -> size l + 1 + size r
         def result = size (make 2000 Leaf)",
    );
    assert_eq!(got, "2000");
    assert!(
        acquired < 2_100,
        "{acquired} blocks acquired for 2000 insertions: the paths were copied"
    );
}

#[test]
fn an_arm_can_test_a_value_and_keep_it_whole() {
    // The second arm takes `xs` apart two cells deep and then uses all of
    // it; the first tests a literal in the same place. As a decision tree
    // the switch on `xs` keeps it in the arm that wants it and gives it up
    // in the others.
    let (got, _) = run_counting(
        "keep-arm",
        &format!(
            "{LIST} fun pick (xs : L) : Int = match xs with | Cons 0 rest -> total rest | Cons x (Cons y _) -> x * 100 + y + total xs | ys -> total ys
             def result = pick (Cons 0 (build 3 Nil)) + pick (build 4 Nil) + pick (Cons 7 Nil) + pick Nil"
        ),
    );
    assert_eq!(got, "125");
}

#[test]
fn a_literal_in_a_tuple_from_an_inlined_copy_is_tested_at_its_own_representation() {
    // `mapOpt` is generic, so a release build copies it at `#Ref`: the
    // `match` sees an `Opt #Ref`, which says nothing of the tuple inside, and
    // the `True` field went without a representation. The test on it then
    // reached the runtime as a value described as anything. The literal says
    // what it is, as a constructor's field already did.
    let got = run(
        "literal-in-inlined-tuple",
        "use Opt.*
         data Opt a = None | Some a
         fun mapOpt (f : a -> b) (o : Opt a) : Opt b = match o with | Some x -> Some (f x) | None -> None
         fun shape (n : Int) : (Int, Bool) = (n, n > 1)
         fun kind (n : Int) : Int = match mapOpt shape (Some n) with | Some (k, True) -> k * 10 | Some (k, False) -> k | None -> 0
         def result = kind 3 + kind 1",
    );
    assert_eq!(got, "31");
}

#[test]
fn a_nested_pattern_on_an_inlined_copy_counts_what_it_takes_apart() {
    // `first` is generic, so a release build copies it at `#Ref` and inlines
    // the copy: the `match` sees a `Opt #Ref`, which says nothing of what the
    // `Some` holds. The `Computed` there is still `xs`'s too, so the arm shares
    // the `Memo` inside it -- which it could only do knowing the `Memo` is a
    // reference, from `Computed`'s declaration. It was left unknown, the
    // share counted nothing, and the `Memo` was taken apart as the only
    // reference to it: its `deps` went with it while `xs` still held them,
    // and the second `match` read freed memory.
    let got = run(
        "nested-inlined-copy",
        &format!(
            "{LIST} use Opt.*
             use E.*
             use M.*
             use Ps.*
             data Opt a = None | Some a
             data M = Memo Int L
             data E = Computed M | Running Int
             data Ps a = PNil | PCons a (Ps a)
             fun first (xs : Ps a) : Opt a = match xs with | PCons x _ -> Some x | PNil -> None
             fun read (xs : Ps E) : Int = match first xs with | Some (Computed (Memo n deps)) -> n + total deps | _ -> 0
             def result = let xs = PCons (Computed (Memo 1 (build 3 Nil))) PNil in read xs * 100 + read xs"
        ),
    );
    assert_eq!(got, "707");
}

// --- literal operands ---------------------------------------------------------
//
// A literal operand rides inside its `extern` (`PrimK`, `BranchPrimK`) rather
// than as a name, so the ownership pass never schedules its erase. A literal
// that is a heap block -- a string, a big integer -- is made fresh for the one
// primitive, which only borrows it, and must be given back after.

#[test]
fn a_string_literal_pattern_gives_its_block_back() {
    assert_eq!(
        run(
            "string_literal_pattern",
            "fun code s = match s with | \"red\" -> 1 | \"green\" -> 2 | \"blue\" -> 3 | _ -> 0\n\
             def result = code \"green\" + code \"blue\" * 10 + code \"mauve\" * 100\n"
        ),
        "32"
    );
}

#[test]
fn a_big_integer_literal_operand_gives_its_block_back() {
    assert_eq!(
        run(
            "bigint_literal_operand",
            "fun next (x : BigInt) : BigInt = x + 2\n\
             def result = next (next 40)\n"
        ),
        "44"
    );
}

#[test]
fn a_big_integer_literal_in_a_comparison_gives_its_block_back() {
    assert_eq!(
        run(
            "bigint_literal_branch",
            "fun small (x : BigInt) : Int = if x < 100 then 1 else 0\n\
             def result = small 5 + small 500 * 10\n"
        ),
        "1"
    );
}

#[test]
fn literal_operands_in_a_loop_do_not_accumulate() {
    // Many iterations, each comparing against a string literal and adding a
    // big literal: a per-iteration leak would leave thousands of blocks.
    assert_eq!(
        run(
            "literal_operands_loop",
            "fun count (n : Int) (acc : BigInt) : BigInt =\n\
             \x20 if n == 0 then acc\n\
             \x20 else count (n - 1) (match \"tick\" with | \"tick\" -> acc + 1 | _ -> acc)\n\
             def result = count 2000 0\n"
        ),
        "2000"
    );
}

// --- a spawn taken as a value -------------------------------------------------
//
// A thread's answer crosses to the thread that awaits it by its descriptor:
// a block is copied out of the thread's heap, which goes when the thread does.
// The descriptor of `a` in a `Task a` names no value, so a closure around a
// spawn -- `spawn` passed to another function -- once captured nothing for it,
// and the answer crossed as a bare word into a heap already freed.

const SPAWNING: &str = "data L = N | C Int L
     use L.*
     fun mk (n : Int) : L = if n == 0 then N else C n (mk (n - 1))
     fun len (l : L) : Int = match l with | N -> 0 | C _ r -> 1 + len r
     fun busy (n : Int) : Int = if n == 0 then 0 else busy (n - 1)
     fun spawn f = threadSpawn f
     fun app f x = f x
";

#[test]
fn a_spawn_passed_as_a_value_hands_its_answer_across() {
    for opt in ["o1", "o2"] {
        let src = format!(
            "{SPAWNING}\
             def result =
               let t = app spawn (\\() -> mk 2000) in
               let b1 = threadSpawn (\\() -> busy 300000) in
               let b2 = threadSpawn (\\() -> busy 300000) in
               let r = threadAwait t in
               (len r, threadAwait b1 + threadAwait b2)"
        );
        let name = format!("spawn_as_value_{opt}");
        let got = if opt == "o1" {
            run_o1(&name, &src)
        } else {
            run_checked(&name, &src, false)
        };
        assert_eq!(got, "(2000, 0)", "{opt}");
    }
}

#[test]
fn spawns_mapped_over_a_list_hand_their_answers_across() {
    // The shape of `V.map Thread.spawn fs`: the spawn is a closure's body.
    let src = format!(
        "{SPAWNING}\
         data Fs = FNil | FCons (() -> L) Fs
         data Ts = TNil | TCons (Task L) Ts
         use Fs.*
         use Ts.*
         fun mapSpawn g fs = match fs with | FNil -> TNil | FCons f rest -> TCons (g f) (mapSpawn g rest)
         fun total ts = match ts with | TNil -> 0 | TCons t rest -> len (threadAwait t) + total rest
         def result = total (mapSpawn spawn (FCons (\\() -> mk 300) (FCons (\\() -> mk 400) (FCons (\\() -> mk 500) FNil))))"
    );
    assert_eq!(run_o1("spawns_mapped", &src), "1200");
}

#[test]
fn a_generic_spawner_passes_its_descriptor_on() {
    // `later` never learns what `a` is; its caller's descriptor has to reach
    // the spawn inside the lambda it builds.
    let src = format!(
        "{SPAWNING}\
         fun later f = \\() -> spawn f
         def result = let go = later (\\() -> mk 1500) in len (threadAwait (go ()))"
    );
    assert_eq!(run_o1("generic_spawner", &src), "1500");
}

// --- segments inside a discarded continuation ----------------------------------
//
// An operation that passes an inner handler on its way to an outer one
// suspends the inner handler's segment too. If the outer clause never resumes,
// the continuation is discarded without unwinding, and the inner segment --
// once held only by a frame on the outer one's stack -- was never given back:
// a stack of its own, per turn, for good.

/// [`run`], and how many handler segments were still live at exit.
#[track_caller]
fn run_segments(name: &str, src: &str) -> (String, usize) {
    let (got, stderr) = run_full(name, src, true, meadow_core::OptLevel::O2);
    let live = stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("aot: ")?
                .strip_suffix(" segments live at exit")?
                .parse()
                .ok()
        })
        .expect("the run says how many segments are live");
    (got, live)
}

const RAISING: &str = "effect Raise { raise : Int -> Int }
     effect Ask { ask : Int -> Int }
     fun asking body = handle body () with { ask n k -> let r = k n in r + 1, return x -> x }
     fun catching body = handle body () with { raise e k -> 0 - e, return x -> x }
     fun loop (i : Int) (acc : Int) = if i == 0 then acc else loop (i - 1) (acc + once i)
";

#[test]
fn an_abandoned_continuation_gives_back_the_segments_inside_it() {
    // `raise` passes `asking` on its way to `catching`, which drops `k`.
    let src = format!(
        "{RAISING}\
         fun once (i : Int) = catching (\\() -> asking (\\() -> let a = ask i in if a >= 0 then raise 1 else a))
         def result = loop 2000 0"
    );
    let (got, live) = run_segments("abandoned_nested", &src);
    assert_eq!(got, "-2000");
    assert_eq!(live, 0, "segments left behind");
}

#[test]
fn several_handlers_deep_are_all_given_back() {
    // Three handlers between the operation and the one that drops it.
    let src = format!(
        "{RAISING}\
         fun once (i : Int) = catching (\\() -> asking (\\() -> asking (\\() -> asking (\\() ->
           let a = ask i in if a >= 0 then raise 2 else a))))
         def result = loop 500 0"
    );
    let (got, live) = run_segments("abandoned_deep", &src);
    assert_eq!(got, "-1000");
    assert_eq!(live, 0, "segments left behind");
}

#[test]
fn a_resumed_continuation_takes_its_nested_segments_back() {
    // The outer clause resumes: the segments parked inside the continuation go
    // back to the frames that drive them, and the inner handler still answers.
    let src = format!(
        "{RAISING}\
         fun resuming body = handle body () with {{ raise e k -> k (e * 10), return x -> x }}
         fun once (i : Int) = resuming (\\() -> asking (\\() -> let a = ask i in raise a + 1))
         def result = loop 300 0"
    );
    // ask i -> i; raise i -> 10 i; + 1; asking's clause adds 1: 10 i + 2.
    let (got, live) = run_segments("resumed_nested", &src);
    assert_eq!(got, (1..=300).map(|i| 10 * i + 2).sum::<i64>().to_string());
    assert_eq!(live, 0, "segments left behind");
}

#[test]
fn a_continuation_resumed_later_still_finds_its_segments() {
    // The outer clause keeps its continuation while another operation passes
    // a handler and is dropped -- parking, and discarding, segments of its own
    // -- and only then resumes the first.
    let src = format!(
        "{RAISING}\
         fun later body = handle body () with {{
           raise e k ->
             let other = catching (\\() -> asking (\\() -> let a = ask e in if a >= 0 then raise 5 else a)) in
             k (other + e),
           return x -> x }}
         fun once (i : Int) = later (\\() -> asking (\\() -> raise (ask i)))
         def result = loop 200 0"
    );
    // ask i -> i; raise i: other = -5, so k (i - 5); the outer asking's clause
    // adds 1: i - 4.
    let (got, live) = run_segments("resumed_after_other", &src);
    assert_eq!(got, (1..=200).map(|i| i - 4).sum::<i64>().to_string());
    assert_eq!(live, 0, "segments left behind");
}

// --- values read out of a compact that is gone ------------------------------------
//
// A block inside a compact region holds a reference to the region, and taking
// one apart gives that reference up. When the compact itself has gone, the
// block's may be the region's last -- and giving it up before sharing the
// fields just loaded freed the region under the shares: heap corruption.

/// Run `src` lowered at both levels, since what one inlines away the other
/// keeps: a debug build is where a value read out of a compact is released
/// through the generic path.
#[track_caller]
fn both(name: &str, src: &str, want: &str) {
    assert_eq!(run(name, src), want, "O2");
    assert_eq!(run_o1(&format!("{name}_o1"), src), want, "O1");
}

const COMPACTED: &str = "data L = N | C Int L
     use L.*
     fun sum (l : L) : Int = match l with | N -> 0 | C x r -> x + sum r
     fun inc (l : L) : L = match l with | N -> N | C x r -> C (x + 1) (inc r)
     fun build (n : Int) : L = if n == 0 then N else C n (build (n - 1))
";

#[test]
fn data_read_out_of_a_dropped_compact_can_be_taken_apart() {
    let src = format!("{COMPACTED}def result = sum (getCompact (compact (build 100)))");
    both("compact_taken_apart", &src, "5050");
}

#[test]
fn data_read_out_of_a_dropped_compact_can_be_rebuilt() {
    // `inc` would build each cell in place of the one it takes apart, were
    // that one its own; a region's never is, so each is released and copied.
    let src = format!("{COMPACTED}def result = sum (inc (getCompact (compact (build 100))))");
    both("compact_rebuilt", &src, "5150");
}

#[test]
fn data_read_out_of_a_dropped_compact_compares_equal() {
    let src = format!(
        "{COMPACTED}def result =
           let a = getCompact (compact (build 50)) == build 50 in
           let b = build 50 == getCompact (compact (build 50)) in
           let c = getCompact (compact (build 50)) == build 49 in
           (a, b, c)"
    );
    both("compact_equal", &src, "(True, True, False)");
}

#[test]
fn a_tuple_read_out_of_a_dropped_compact_can_be_taken_apart() {
    let src = format!(
        "{COMPACTED}def result =
           let (a, b) = getCompact (compact (build 3, build 4)) in
           sum a * 100 + sum b"
    );
    both("compact_tuple", &src, "610");
}

#[test]
fn a_wrapper_read_out_of_a_dropped_compact_can_be_unwrapped() {
    // The shape `Std.Collections.Vector`'s `==` has, which crashed: the one
    // field of a block whose reference is the region's last.
    let src = format!(
        "{COMPACTED}data W = W L
         use W.*
         fun unwrap (w : W) : L = match w with | W l -> l
         def result = sum (unwrap (getCompact (compact (W (build 10)))))"
    );
    both("compact_unwrapped", &src, "55");
}

#[test]
fn reading_a_compact_in_a_loop_leaves_nothing_behind() {
    // Each turn makes a region, reads it, drops the compact, and takes the value
    // apart: the region must go every time, and only once.
    let src = format!(
        "{COMPACTED}fun loop (i : Int) (acc : Int) : Int =
           if i == 0 then acc else loop (i - 1) (acc + sum (getCompact (compact (build 20))))
         def result = loop 500 0"
    );
    both("compact_loop", &src, "105000");
    assert_eq!(
        run_regions("compact_loop_regions", &src, true),
        ("105000".into(), 0)
    );
}

/// [`run`], and how many compact regions were still live at exit.
#[track_caller]
fn run_regions(name: &str, src: &str, check: bool) -> (String, usize) {
    let (got, stderr) = run_full(name, src, check, meadow_core::OptLevel::O2);
    let live = stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("aot: ")?
                .strip_suffix(" regions live at exit")?
                .parse()
                .ok()
        })
        .expect("the run says how many regions are live");
    (got, live)
}

/// A thread counts the references it takes into a region for itself, and the
/// region's own count hears of them when the thread has touched other
/// regions or ends (`silo/src/region.rs`). Threads that read one compact side
/// by side, and a `main` that reads several, leave none behind.
#[test]
fn regions_read_by_several_threads_are_all_freed() {
    let src = format!(
        "{COMPACTED}data Ts = Done | More (Task Int) Ts
         use Ts.*
         fun reads (c : Compact L) (n : Int) (acc : Int) : Int =
           if n == 0 then acc else reads c (n - 1) (acc + sum (getCompact c))
         fun spawned (c : Compact L) (n : Int) acc =
           if n == 0 then acc else spawned c (n - 1) (More (threadSpawn (\\() -> reads c 200 0)) acc)
         fun awaited ts (acc : Int) =
           match ts with | Done -> acc | More t rest -> awaited rest (acc + threadAwait t)
         fun several (i : Int) (acc : Int) : Int =
           if i == 0 then acc else several (i - 1) (acc + sum (getCompact (compact (build i))))
         def result =
           let c = compact (build 20) in
           awaited (spawned c 8 Done) 0 + several 12 0"
    );
    assert_eq!(
        run_regions("regions_by_threads", &src, false),
        ("336364".into(), 0)
    );
}

/// What a thread answers is kept for `await` -- for as long as there is a
/// `Task` of the thread to await it with, and no longer. Kept for good, a
/// compact a thread answered with held its region to the end of the program.
#[test]
fn what_a_thread_answered_goes_when_nothing_can_await_it() {
    let src = format!(
        "{COMPACTED}data Ts = Done | More (Task (Compact L)) Ts
         use Ts.*
         fun spawned (n : Int) acc =
           if n == 0 then acc else spawned (n - 1) (More (threadSpawn (\\() -> compact (build n))) acc)
         fun awaited ts (acc : Int) =
           match ts with
           | Done -> acc
           | More t rest ->
               -- Awaited twice: the answer is there for as long as the `Task` is.
               awaited rest (acc + sum (getCompact (threadAwait t)) + sum (getCompact (threadAwait t)))
         def result = awaited (spawned 12 Done) 0"
    );
    assert_eq!(
        run_regions("answers_let_go", &src, false),
        ("728".into(), 0)
    );
}

// --- what an abandoned continuation's frames held -----------------------------
//
// A frame waiting on a call holds what it will need when the call returns. If
// the call never returns -- an operation inside it was answered by a clause
// that dropped the continuation -- those values were never given up. They are
// now recorded around the call and given up when the continuation is
// discarded (`silo/src/shadow.rs`), so these programs end with nothing live.

const HOLDING: &str = "effect Raise { raise : Int -> Int }
     data L = N | C Int L
     use L.*
     fun build (n : Int) : L = if n == 0 then N else C n (build (n - 1))
     fun len (l : L) : Int = match l with | N -> 0 | C _ r -> 1 + len r
     fun catching body = handle body () with { raise e k -> 0 - e }
     fun loop (i : Int) (acc : Int) = if i == 0 then acc else loop (i - 1) (acc + once i)
";

/// Run at both levels, checked for leaks: O1 keeps every call a call.
#[track_caller]
fn no_leak(name: &str, src: &str, want: &str) {
    assert_eq!(run_checked(name, src, true), want, "O2");
    assert_eq!(run_o1(&format!("{name}_o1"), src), want, "O1");
}

#[test]
fn frames_that_held_lists_give_them_up_when_abandoned() {
    // Ten frames deep, each holding a list across the call below it, when
    // `raise` is answered by a clause that drops the continuation.
    let src = format!(
        "{HOLDING}\
         fun deep (n : Int) (held : L) =\n\
         \x20 if n == 0 then raise 1 else let r = deep (n - 1) (C n held) in r + len held\n\
         fun once (i : Int) = catching (\\() -> deep 10 (build 5))\n\
         def result = loop 300 0"
    );
    no_leak("abandoned_frames", &src, "-300");
}

#[test]
fn frames_holding_values_of_a_type_variable_give_them_up() {
    // What each frame holds is described at run time: a descriptor in a
    // register, recorded as it is.
    let src = format!(
        "{HOLDING}\
         fun keep (n : Int) (x : a) f =\n\
         \x20 if n == 0 then raise 2 else let r = keep (n - 1) x f in r + f x\n\
         fun once (i : Int) = catching (\\() -> keep 6 (build 4) len + keep 6 (C i N, build 3) (\\p -> 1))\n\
         def result = loop 200 0"
    );
    no_leak("abandoned_poly_frames", &src, "-400");
}

#[test]
fn frames_under_a_nested_handler_give_theirs_up_too() {
    // The operation passes an inner handler on its way out: the frames on the
    // inner segment and the outer are both discarded, and the continuation
    // `handle` was to answer, held by the runtime, goes with them.
    let src = format!(
        "{HOLDING}\
         effect Ask {{ ask : Int -> Int }}\n\
         fun asking body = handle body () with {{ ask n k -> let r = k n in r + 1 }}\n\
         fun deep (n : Int) (held : L) =\n\
         \x20 if n == 0 then raise (ask 3) else let r = deep (n - 1) (C n held) in r + len held\n\
         fun once (i : Int) =\n\
         \x20 catching (\\() -> let before = build 3 in len before + asking (\\() -> deep 5 (build 2)))\n\
         def result = loop 200 0"
    );
    no_leak("abandoned_nested_frames", &src, "-600");
}

#[test]
fn a_program_with_threads_gives_abandoned_frames_up() {
    // With threads the chain's head is each thread's own, reached through the
    // runtime rather than a global.
    let src = format!(
        "{HOLDING}\
         fun deep (n : Int) (held : L) =\n\
         \x20 if n == 0 then raise 1 else let r = deep (n - 1) (C n held) in r + len held\n\
         fun once (i : Int) = catching (\\() -> deep 8 (build 4))\n\
         def result =\n\
         \x20 let t = threadSpawn (\\() -> loop 100 0) in\n\
         \x20 let here = loop 100 0 in\n\
         \x20 here + threadAwait t"
    );
    assert_eq!(run_checked("abandoned_threaded", &src, false), "-200");
    assert_eq!(run_o1("abandoned_threaded_o1", &src), "-200");
}

// --- calls wider than the registers --------------------------------------------
//
// Arguments past the tenth go through a spill area: once a fixed 256 words,
// written past its end by a wider call, and in a program without threads one
// area per module, so a call into another module read the wrong one.

fn wide_call(n: usize) -> String {
    let params: String = (0..n).map(|i| format!("(x{i} : Int) ")).collect();
    let sum: Vec<String> = (0..n).map(|i| format!("x{i} * {}", i % 3 + 1)).collect();
    let args: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!(
        "fun f {params}: Int = {}
def result = f {}",
        sum.join(" + "),
        args.join(" ")
    )
}

fn wide_want(n: usize) -> String {
    (0..n).map(|i| i * (i % 3 + 1)).sum::<usize>().to_string()
}

#[test]
fn a_call_of_three_hundred_arguments() {
    assert_eq!(run("wide_call_300", &wide_call(300)), wide_want(300));
    assert_eq!(run_o1("wide_call_300_o1", &wide_call(300)), wide_want(300));
}

#[test]
fn a_wide_call_into_another_module() {
    // Twelve arguments, two past the registers, and three hundred: each
    // function its own module.
    assert_eq!(
        run_split("split_call_12", &wide_call(12), true),
        wide_want(12)
    );
    assert_eq!(
        run_split("split_call_300", &wide_call(300), true),
        wide_want(300)
    );
}

#[test]
fn a_wide_call_in_a_program_with_threads() {
    // With threads each has a spill area of its own, which the runtime makes
    // as big as the module says it needs.
    let src = wide_call(300).replace("def result = f", "def once = f")
        + "\ndef result = let t = threadSpawn (\\() -> once) in once + threadAwait t";
    let want = (2 * wide_want(300).parse::<usize>().unwrap()).to_string();
    assert_eq!(run_checked("wide_call_threads", &src, false), want);
}

/// The same program lowered and compiled twice is the same IR, byte for byte:
/// what a build compares to tell whether its executable is already this one.
#[test]
fn the_same_program_compiles_to_the_same_ir() {
    let src = "effect Ask { ask : () -> Int }\n\
               use Shape.*\n\
               data Shape = Circle Int | Rect Int Int\n\
               record Box = { w : Int, h : Int }\n\
               fun area s = match s with | Circle r -> r * r | Rect w h -> w * h\n\
               fun sum xs = match xs with | Nil -> 0 | Cons x r -> x + sum r\n\
               fun twice f x = f (f x)\n\
               def result =\n\
               \x20 let b = Box { w = 2, h = 3 } in\n\
               \x20 let t = (area (Rect b.w b.h), sum (Cons 1 (Cons 2 Nil)), twice (\\x -> x + 1) 5) in\n\
               \x20 handle (ask () + ask ()) with { ask u k -> k 21 }\n";
    let p = program(src);
    let ir = |p: &core::Program| {
        for opt in [meadow_compiler::OptLevel::O1, meadow_compiler::OptLevel::O2] {
            let lowered = meadow_seq::lower_program(p, opt);
            let _ = meadow_llvm::compile_split(
                &lowered.program,
                meadow_llvm::UNIT,
                meadow_llvm::CallConv::Ghc,
            );
        }
        let lowered = meadow_seq::lower_program(p, meadow_compiler::OptLevel::O2);
        meadow_llvm::compile_split(
            &lowered.program,
            meadow_llvm::UNIT,
            meadow_llvm::CallConv::Ghc,
        )
        .expect("it compiles")
    };
    let (a, b) = (ir(&p), ir(&p));
    assert_eq!(a.len(), b.len(), "as many modules");
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.name, y.name, "by the same names");
        let (x, y) = (&x.text, &y.text);
        if x != y {
            let at = x
                .lines()
                .zip(y.lines())
                .position(|(l, r)| l != r)
                .unwrap_or(0);
            panic!(
                "the IR differs at line {at}:\n{}\n{}",
                x.lines().nth(at).unwrap_or(""),
                y.lines().nth(at).unwrap_or("")
            );
        }
    }
}

// --- tracing --------------------------------------------------------------------
//
// Unless a build asks for trial deletion, a program that ties knots has its
// cycles found by concurrent tracing (`silo/src/trace.rs`): the roots taken
// in a pause, marking on a thread of its own, a barrier at every overwrite,
// and a sweep a slice at a time. These run programs under both collectors and
// ask the tally whether a trace ran while the program did -- not only at
// exit, where the leak check collects everything there is anyway -- and some
// run with every freed block poisoned, so that one the trace frees while it
// is still in use is found.

use meadow_llvm::Cycles;

/// Compile `src` with `how` for its cycles, run it with `env`, and answer
/// what it printed and said on stderr: the leak check's lines, and the
/// collectors' tallies. Not checked against the AxCut machine, which has no
/// threads, and not checked for leaks: the callers say what they expect.
#[track_caller]
fn run_mode(name: &str, src: &str, how: Cycles, env: &[(&str, &str)]) -> (String, String) {
    let prog = program(src);
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O2);
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    let units = meadow_llvm::with_cycles(how, || {
        meadow_llvm::compile_split(&lowered.program, usize::MAX, meadow_llvm::CallConv::host())
    })
    .unwrap_or_else(|e| panic!("{}", e.msg));
    let name = format!("{name}_{}", how.name());
    let dir = std::env::temp_dir().join("meadow-llvm-tests").join(&name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = units
        .iter()
        .enumerate()
        .map(|(i, ll)| {
            let path = dir.join(format!("prog{i}.ll"));
            std::fs::write(&path, &ll.text).unwrap();
            path
        })
        .collect();
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let out = Command::new("clang")
        .args(["-O2", "-Wno-override-module", "-o"])
        .arg(&exe)
        .args(&paths)
        .arg(runtime())
        .args(system_libs())
        .output()
        .expect("clang runs");
    assert!(
        out.status.success(),
        "clang failed on {}:\n{}",
        paths[0].display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut cmd = Command::new(&exe);
    cmd.env("MEADOW_SILO_LEAKS", "1")
        .env("MEADOW_SILO_CYCLES", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let ran = cmd.output().expect("the program runs");
    let stdout = String::from_utf8_lossy(&ran.stdout).trim_end().to_string();
    let stderr = String::from_utf8_lossy(&ran.stderr).to_string();
    assert!(
        ran.status.success(),
        "{name} failed: {stderr}\n(see {})",
        paths[0].display()
    );
    (stdout, stderr)
}

/// The number before `suffix` on the `aot: ` line that ends with it.
fn said(stderr: &str, suffix: &str) -> Option<u64> {
    stderr.lines().find_map(|l| {
        l.strip_prefix("aot: ")?
            .strip_suffix(suffix)?
            .trim()
            .parse()
            .ok()
    })
}

fn live_at_exit(stderr: &str) -> u64 {
    said(stderr, " blocks live at exit").expect("the leak check says what is live")
}

/// How many traces ran while the program did, and how many blocks they freed.
fn traced(stderr: &str) -> (u64, u64) {
    stderr
        .lines()
        .find_map(|l| {
            let rest = l.strip_prefix("aot: ")?;
            let (n, rest) = rest.split_once(" traces, ")?;
            let freed = rest.strip_suffix(" blocks freed by tracing")?;
            Some((n.parse().ok()?, freed.parse().ok()?))
        })
        .unwrap_or((0, 0))
}

/// Under `MEADOW_SILO_POISON`: no freed block was written to after it was
/// freed, and none was freed twice.
#[track_caller]
fn poison_clean(stderr: &str) {
    assert!(
        stderr.contains("; 0 written to after, 0 freed twice"),
        "the poisoned heap says otherwise: {stderr}"
    );
}

const KNOT: &str = "data L = Nil | Node Int (Ref L)
     use L.*
     fun knot (n : Int) =
       let r = newRef Nil in
       let _ = setRef r (Node n r) in
       ()
     fun knots (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in knots (n - 1)
";

#[test]
fn every_kind_of_cycle_is_collected_by_either_collector() {
    let cases = [
        (
            "both_ref",
            format!("{KNOT}def result = knots 20000"),
            "0",
        ),
        (
            "both_long",
            "data L = Nil | Node Int (Ref L)
             use L.*
             fun knot (n : Int) =
               let a = newRef Nil in
               let b = newRef Nil in
               let _ = setRef a (Node n b) in
               let _ = setRef b (Node n a) in
               ()
             fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
             def result = many 20000"
                .to_string(),
            "0",
        ),
        (
            "both_array",
            "data Opt a = No | Some a
             use Opt.*
             data N s = N (StArray s (Opt (N s))) Int
             fun knot (n : Int) : Int =
               runSt (\\() ->
                 let a = stNewArray 2 No in
                 let _ = stSetArray a 0 (Some (N a n)) in
                 n)
             fun many (n : Int) (acc : Int) : Int = if n == 0 then acc else many (n - 1) (acc + knot n)
             def result = many 20000 0"
                .to_string(),
            "200010000",
        ),
        (
            "both_leaves",
            "data L = Nil | Node String (Array Int) (Ref L)
             use L.*
             fun knot (n : Int) =
               let r = newRef Nil in
               let _ = setRef r (Node (show n) #[n, n + 1] r) in
               ()
             fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
             def result = many 20000"
                .to_string(),
            "0",
        ),
    ];
    for (name, src, want) in &cases {
        for how in [Cycles::Tracing, Cycles::Trial] {
            let (got, stderr) = run_mode(name, src, how, &[]);
            assert_eq!(got, *want, "{name} under {how:?}");
            assert_eq!(live_at_exit(&stderr), 0, "{name} under {how:?}: {stderr}");
        }
    }
}

#[test]
fn a_trace_runs_while_the_program_does() {
    // Forty thousand knots of two blocks each: the heap doubles past the
    // first trace's mark many times over, so traces run as the program goes
    // and free what it let go of -- not only the collection at exit.
    let (got, stderr) = run_mode(
        "trace_runs",
        &format!("{KNOT}def result = knots 40000"),
        Cycles::Tracing,
        &[],
    );
    assert_eq!(got, "0");
    let (traces, freed) = traced(&stderr);
    assert!(traces >= 1, "no trace ran while the program did: {stderr}");
    assert!(freed >= 4096, "the traces freed only {freed}: {stderr}");
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

/// The cycle trial deletion is documented to miss: its `Ref` is let go of
/// while the cycle is still held -- here by a list, long enough for runs of
/// the collector to look at the `Ref` and find it alive -- and the cycle
/// becomes garbage later, when the list lets go of the node, which is not a
/// `Ref` and so is never a candidate.
const HELD_KNOTS: &str = "data L = Nil | Node Int (Ref L)
     data K = KNil | KCons L K
     use L.*
     use K.*
     fun knot (n : Int) : L ! Mut =
       let r = newRef Nil in
       let node = Node n r in
       let _ = setRef r node in
       node
     fun build (n : Int) (acc : K) : K ! Mut = if n == 0 then acc else build (n - 1) (KCons (knot n) acc)
     fun count (ks : K) (acc : Int) : Int = match ks with
       | KNil -> acc
       | KCons _ rest -> count rest (acc + 1)
     def result = count (build 6000 KNil) 0
";

#[test]
fn a_cycle_trial_deletion_misses_is_traced() {
    // The list is counted, not taken apart: taking a node apart would let go
    // of its `Ref` again, which trial deletion would see.
    let (got, stderr) = run_mode("held_knots", HELD_KNOTS, Cycles::Tracing, &[]);
    assert_eq!(got, "6000");
    assert_eq!(live_at_exit(&stderr), 0, "tracing left it behind: {stderr}");
    // And the hole is still where the design says it is: what trial deletion
    // leaves is exactly what tracing exists to find.
    let (got, stderr) = run_mode("held_knots", HELD_KNOTS, Cycles::Trial, &[]);
    assert_eq!(got, "6000");
    assert!(
        live_at_exit(&stderr) > 0,
        "trial deletion found these after all; the design says it cannot: {stderr}"
    );
}

#[test]
fn a_live_ring_rewritten_while_tracing_keeps_every_node() {
    // A ring of a hundred `Ref`s held from one place, whose first ten nodes
    // are replaced by new ones at every turn, while garbage knots keep the
    // traces coming: each replaced node is logged by the barrier, the new
    // ones are black, and the rest of the ring is reached from the roots.
    // Every freed block is poisoned, so a node freed while the ring still
    // holds it is read as junk -- an address nothing is mapped at.
    let src = format!(
        "{KNOT}\
         fun link (first : Ref L) (prev : Ref L) (i : Int) (n : Int) : () ! Mut =
           if i == n then setRef prev (Node (n - 1) first)
           else
             let r = newRef Nil in
             let _ = setRef prev (Node (i - 1) r) in
             link first r (i + 1) n
         fun ring (n : Int) : Ref L ! Mut =
           let first = newRef Nil in
           let _ = link first first 1 n in
           first
         fun bump (r : Ref L) (k : Int) : () ! Mut =
           if k == 0 then ()
           else match getRef r with
             | Nil -> ()
             | Node v next -> let _ = setRef r (Node (v + 1) next) in bump next (k - 1)
         fun turn (r : Ref L) (i : Int) : Int ! Mut =
           if i == 0 then 0 else let _ = bump r 10 in let _ = knots 40 in turn r (i - 1)
         fun total (r : Ref L) (k : Int) (acc : Int) : Int ! Mut =
           if k == 0 then acc
           else match getRef r with | Nil -> acc | Node v next -> total next (k - 1) (acc + v)
         def result = let r = ring 100 in let _ = turn r 3000 in total r 100 0"
    );
    let (got, stderr) = run_mode(
        "live_ring",
        &src,
        Cycles::Tracing,
        &[("MEADOW_SILO_POISON", "ptr")],
    );
    assert_eq!(got, "34950");
    assert!(traced(&stderr).0 >= 2, "too few traces ran: {stderr}");
    poison_clean(&stderr);
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

#[test]
fn a_reference_moved_between_refs_while_marking_is_kept() {
    // The case a barrier is for: the only reference to a chain moves from one
    // `Ref` to another and back, over and over, while traces mark -- so at
    // some point one of them has scanned the `Ref` it is moving to before
    // the move, and not yet the one it is moving from. Without the logged
    // reference, the chain would go unmarked and be freed in use.
    let src = format!(
        "{KNOT}\
         fun chain (n : Int) (acc : L) : L ! Mut = if n == 0 then acc else chain (n - 1) (Node n (newRef acc))
         fun swap (a : Ref L) (b : Ref L) (i : Int) : () ! Mut =
           if i == 0 then ()
           else
             let x = getRef a in
             let _ = setRef b x in
             let _ = setRef a Nil in
             let _ = knots 20 in
             swap b a (i - 1)
         fun sum (l : L) (acc : Int) : Int ! Mut =
           match l with | Nil -> acc | Node v next -> sum (getRef next) (acc + v)
         def result =
           let a = newRef (chain 200 Nil) in
           let b = newRef Nil in
           let _ = swap a b 5001 in
           sum (getRef b) 0 + sum (getRef a) 0"
    );
    let (got, stderr) = run_mode(
        "moved_ref",
        &src,
        Cycles::Tracing,
        &[("MEADOW_SILO_POISON", "ptr")],
    );
    assert_eq!(got, "20100");
    assert!(traced(&stderr).0 >= 2, "too few traces ran: {stderr}");
    poison_clean(&stderr);
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

#[test]
fn reuse_in_a_program_that_ties_knots_survives_tracing() {
    // `inc` rebuilds the list in the cells it takes apart: each rebuilt cell
    // goes through the reuse barrier while a trace marks, and is a new block
    // from then on.
    let src = format!(
        "{LIST}\
         data R = RNil | RNode Int (Ref R)
         use R.*
         fun knot (n : Int) =
           let r = newRef RNil in
           let _ = setRef r (RNode n r) in
           ()
         fun knots (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in knots (n - 1)
         fun rounds (xs : L) (i : Int) : L ! Mut =
           if i == 0 then xs else let _ = knots 30 in rounds (inc xs) (i - 1)
         def result = total (rounds (build 2000 Nil) 300)"
    );
    let (got, stderr) = run_mode(
        "reuse_traced",
        &src,
        Cycles::Tracing,
        &[("MEADOW_SILO_POISON", "ptr")],
    );
    assert_eq!(got, "2601000");
    assert!(traced(&stderr).0 >= 1, "no trace ran: {stderr}");
    poison_clean(&stderr);
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

#[test]
fn knots_tied_under_a_handler_are_traced() {
    // The body runs on a segment of its own, and each operation suspends it
    // to the handler's: a trace that starts in either finds the roots on
    // both, and on every segment that is waiting.
    let src = format!(
        "{KNOT}\
         effect Ask {{ ask : () -> Int }}
         fun body (n : Int) = if n == 0 then 0 else let _ = knots 4 in body (n - 1) + ask ()
         def result = handle body 3000 with {{ ask u k -> k 1 }}"
    );
    let (got, stderr) = run_mode("handler_knots", &src, Cycles::Tracing, &[]);
    assert_eq!(got, "3000");
    assert!(traced(&stderr).0 >= 1, "no trace ran: {stderr}");
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
    assert_eq!(said(&stderr, " segments live at exit"), Some(0), "{stderr}");
}

#[test]
fn threads_each_trace_their_own_heap() {
    // Every thread has a heap of its own, and so a trace of its own; the
    // marker takes them in turn. The knots are a mutable array's, made under
    // `runSt`, which a thread may do.
    let src = "data Opt a = No | Some a
         use Opt.*
         data N s = N (StArray s (Opt (N s))) Int
         fun knot (n : Int) : Int =
           runSt (\\() ->
             let a = stNewArray 2 No in
             let _ = stSetArray a 0 (Some (N a n)) in
             n)
         fun knots (n : Int) (acc : Int) : Int = if n == 0 then acc else knots (n - 1) (acc + knot 1)
         fun work (n : Int) : Int = knots n 0
         def result =
           let a = threadSpawn (\\() -> work 20000) in
           let b = threadSpawn (\\() -> work 20000) in
           let c = threadSpawn (\\() -> work 20000) in
           let d = work 20000 in
           threadAwait a + threadAwait b + threadAwait c + d";
    let (got, stderr) = run_mode("thread_knots", src, Cycles::Tracing, &[]);
    assert_eq!(got, "80000");
    assert!(
        traced(&stderr).0 >= 4,
        "a trace per thread at least: {stderr}"
    );
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
    assert!(
        !stderr.contains("held by threads when they ended"),
        "{stderr}"
    );
}

#[test]
fn a_cycle_through_a_large_array_is_traced() {
    // An array of six hundred elements is a block too large for a size
    // class: allocated alone, and swept from the trace's list of them.
    let src = "data Opt a = No | Some a
         use Opt.*
         data N s = N (StArray s (Opt (N s))) Int
         fun knot (n : Int) : Int =
           runSt (\\() ->
             let a = stNewArray 600 No in
             let _ = stSetArray a 7 (Some (N a n)) in
             n)
         fun many (n : Int) (acc : Int) : Int = if n == 0 then acc else many (n - 1) (acc + knot n)
         def result = many 3000 0";
    let (got, stderr) = run_mode(
        "large_knots",
        src,
        Cycles::Tracing,
        &[("MEADOW_SILO_POISON", "ptr")],
    );
    assert_eq!(got, "4501500");
    poison_clean(&stderr);
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

#[test]
fn a_compact_only_garbage_cycles_hold_goes_with_them() {
    // Each knot holds a compact: freeing the cycle lets go of the compact,
    // and with it its region.
    let src = format!(
        "{COMPACTED}\
         data K = KNil | KNode L (Ref K)
         use K.*
         fun knot (n : Int) =
           let r = newRef KNil in
           let _ = setRef r (KNode (getCompact (compact (build 20))) r) in
           ()
         fun many (n : Int) : Int ! Mut = if n == 0 then 0 else let _ = knot n in many (n - 1)
         def result = many 5000"
    );
    let (got, stderr) = run_mode("compact_knots", &src, Cycles::Tracing, &[]);
    assert_eq!(got, "0");
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
    assert_eq!(said(&stderr, " regions live at exit"), Some(0), "{stderr}");
}

#[test]
fn one_large_cycle_is_freed_by_a_trace() {
    // A single ring of two hundred thousand cells: trial deletion frees a
    // cycle that size in one long pause, if at all; a trace frees it in the
    // sweep, a slice at a time, once the program has let go of it and gone on
    // to allocate: enough, after it, for the heap to double past it however
    // the traces fall.
    let src = format!(
        "{KNOT}\
         fun link (first : Ref L) (prev : Ref L) (n : Int) : () ! Mut =
           if n == 0 then setRef prev (Node 0 first)
           else
             let r = newRef Nil in
             let _ = setRef prev (Node n r) in
             link first r (n - 1)
         fun ring (n : Int) : () ! Mut =
           let first = newRef Nil in
           link first first n
         def result = let _ = ring 200000 in knots 1000000"
    );
    let (got, stderr) = run_mode("big_ring", &src, Cycles::Tracing, &[]);
    assert_eq!(got, "0");
    let (traces, freed) = traced(&stderr);
    assert!(traces >= 2, "{stderr}");
    assert!(
        freed >= 400_000,
        "the ring was not freed while running: {stderr}"
    );
    assert_eq!(live_at_exit(&stderr), 0, "{stderr}");
}

#[test]
fn the_collector_is_emitted_only_for_a_program_that_can_tie_a_knot() {
    let ir = |src: &str, how: Cycles| -> String {
        let prog = program(src);
        let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O2);
        meadow_llvm::with_cycles(how, || {
            meadow_llvm::compile_split(&lowered.program, usize::MAX, meadow_llvm::CallConv::host())
        })
        .unwrap_or_else(|e| panic!("{}", e.msg))
        .into_iter()
        .map(|u| u.text)
        .collect::<Vec<_>>()
        .join("\n")
    };
    let knots = format!("{KNOT}def result = knots 10");
    let none = format!("{LIST}def result = total (inc (build 10 Nil))");
    // Tracing: the flag says so, the barrier is there, the candidate test is
    // not.
    let traced = ir(&knots, Cycles::Tracing);
    assert!(traced.contains("@meadow_cycles = constant i8 2"));
    assert!(traced.contains("call void @meadow_satb("));
    assert!(!traced.contains("call void @meadow_candidate("));
    // Trial deletion: the other way round.
    let trial = ir(&knots, Cycles::Trial);
    assert!(trial.contains("@meadow_cycles = constant i8 1"));
    assert!(trial.contains("call void @meadow_candidate("));
    assert!(!trial.contains("call void @meadow_satb"));
    // A program that cannot tie a knot has neither, whatever was asked.
    for how in [Cycles::Tracing, Cycles::Trial] {
        let plain = ir(&none, how);
        assert!(plain.contains("@meadow_cycles = constant i8 0"), "{how:?}");
        assert!(!plain.contains("call void @meadow_satb"), "{how:?}");
        assert!(!plain.contains("call void @meadow_candidate("), "{how:?}");
    }
}
