//! Programs compiled ahead of time: to native code, into an object file, linked
//! with the system's C compiler against this runtime, and run as a process.
//! Each must answer what the bytecode VM answers.
//!
//! `MEADOW_CC` names the C compiler, `MEADOW_RUNTIME` the runtime library, and
//! `MEADOW_SILO_RUNNER` what runs the result, for testing under an emulator.

use meadow_compiler::{Options, compile_str_with, core};
use meadow_glade::codegen::{self, Arch, object};
use std::path::{Path, PathBuf};
use std::process::Command;

fn program(src: &str) -> core::Program {
    let (pkg, diags) = compile_str_with("aot", src, Options::default().entry("result"));
    let hard: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(hard.is_empty(), "compile errors:\n{}", hard.join("\n"));
    let entry = pkg.value_entry;
    core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    }
}

fn image(src: &str, opt: meadow_core::OptLevel) -> meadow_bytecode::Program {
    let lowered = meadow_seq::lower_program(&program(src), opt);
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    meadow_codegen::compile(&lowered.program).expect("codegen")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-silo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The runtime as a static library for `arch`, as this build of it made it:
/// the host's from `cargo build`, another from `cargo build --target`.
fn runtime(arch: Arch) -> Option<PathBuf> {
    // Named outright, for the host: built for a target this is emulating.
    if let Some(lib) = std::env::var_os("MEADOW_RUNTIME") {
        return (Some(arch) == Arch::host()).then(|| PathBuf::from(lib));
    }
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
    let dir = if Some(arch) == Arch::host() {
        target.join(profile)
    } else {
        target.join(triple(arch)?).join(profile)
    };
    fresh(arch, &target);
    let lib = if cfg!(windows) {
        "meadow_glade.lib"
    } else {
        "libmeadow_glade.a"
    };
    Some(dir.join(lib)).filter(|lib| lib.exists())
}

/// Bring the static library for `arch` up to date, once per run: `cargo test`
/// builds the crate to link the tests against, not the library a program
/// links. For another architecture, only where one was built before -- that
/// needs its Rust target installed.
fn fresh(arch: Arch, target: &Path) {
    static DONE: std::sync::Mutex<Vec<Arch>> = std::sync::Mutex::new(Vec::new());
    let mut done = DONE.lock().unwrap_or_else(|p| p.into_inner());
    if done.contains(&arch) {
        return;
    }
    done.push(arch);
    let mut cargo = Command::new(env!("CARGO"));
    cargo.args(["build", "--lib", "--manifest-path"]);
    cargo.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"));
    if !cfg!(debug_assertions) {
        cargo.arg("--release");
    }
    if Some(arch) != Arch::host() {
        match triple(arch) {
            Some(t) if target.join(t).is_dir() => {
                cargo.args(["--target", t]);
            }
            _ => return,
        }
    }
    let out = cargo.output().expect("cargo runs");
    assert!(
        out.status.success(),
        "building the runtime library failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The target a macOS runtime for `arch` is built for, where this machine can
/// also run it -- under Rosetta, on Apple silicon.
fn triple(arch: Arch) -> Option<&'static str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    Some(match arch {
        Arch::Aarch64 => "aarch64-apple-darwin",
        Arch::X86_64 => "x86_64-apple-darwin",
    })
}

/// Compile `src` for `arch`, link it, run it, and answer what it printed and
/// whether it succeeded -- or `None` if there is no runtime for `arch` to link.
fn native(name: &str, src: &str, opt: meadow_core::OptLevel, arch: Arch) -> Option<(String, bool)> {
    let runtime = runtime(arch)?;
    let image = image(src, opt);
    let compiled = codegen::compile(&image, arch, opt);
    let bytes = meadow_bytecode::image::encode(&image);
    let dir = scratch(name);
    let format = object::Format::host();
    let obj = dir.join(format!("program.{}", format.object_extension()));
    std::fs::write(&obj, object::write(&compiled, &bytes, format)).unwrap();
    let main = dir.join("main.c");
    std::fs::write(&main, object::main_c()).unwrap();
    let exe = dir.join(format!("program{}", format.exe_suffix()));
    let mut cc;
    #[cfg(windows)]
    {
        cc = find_msvc_tools::find(
            match arch {
                Arch::Aarch64 => "aarch64-pc-windows-msvc",
                Arch::X86_64 => "x86_64-pc-windows-msvc",
            },
            "cl.exe",
        )
        .expect("Visual Studio's C compiler");
        cc.current_dir(&dir)
            .args(["/nologo", "/MD"])
            .arg(&main)
            .arg(&obj)
            .arg(runtime)
            .arg(format!("/Fe{}", exe.display()));
    }
    #[cfg(not(windows))]
    {
        cc = Command::new(std::env::var("MEADOW_CC").unwrap_or_else(|_| "cc".into()));
        cc.arg(&main).arg(&obj).arg(runtime).arg("-o").arg(&exe);
        if cfg!(target_os = "macos") {
            let arch = match arch {
                Arch::Aarch64 => "arm64",
                Arch::X86_64 => "x86_64",
            };
            cc.args(["-arch", arch]);
        }
    }
    cc.args(format.system_libs());
    let linked = cc.output().expect("a C compiler");
    assert!(
        linked.status.success(),
        "linking failed:\n{}{}",
        String::from_utf8_lossy(&linked.stdout),
        String::from_utf8_lossy(&linked.stderr)
    );
    // `MEADOW_SILO_RUNNER`, for an executable this machine runs through an
    // emulator: `qemu-aarch64 -L <sysroot>`, say.
    let run = match std::env::var("MEADOW_SILO_RUNNER") {
        Ok(runner) => {
            let mut words = runner.split_whitespace();
            let mut cmd = Command::new(words.next().expect("a runner"));
            cmd.args(words).arg(&exe);
            cmd
        }
        Err(_) => Command::new(&exe),
    }
    .output()
    .expect("the program runs");
    let _ = std::fs::remove_dir_all(&dir);
    let out = if run.status.success() {
        String::from_utf8_lossy(&run.stdout).trim_end().to_string()
    } else {
        String::from_utf8_lossy(&run.stderr).trim_end().to_string()
    };
    Some((out, run.status.success()))
}

/// The native executable answers what the VM does, at every level, for each
/// architecture there is a runtime to link -- the host's always, and x86-64 on
/// Apple silicon once `cargo build --target x86_64-apple-darwin` has made one.
fn agrees(name: &str, src: &str) -> String {
    let mut want = None;
    for arch in [Arch::Aarch64, Arch::X86_64] {
        for opt in [
            meadow_core::OptLevel::O0,
            meadow_core::OptLevel::O1,
            meadow_core::OptLevel::O2,
        ] {
            let vm = meadow_glade::run(&image(src, opt), u64::MAX).map_err(|e| e.msg);
            let at = format!("{name}-{arch:?}-{}", opt.name());
            let Some((got, ok)) = native(&at, src, opt, arch) else {
                assert_ne!(Some(arch), Arch::host(), "no runtime for the host to link");
                eprintln!("skipping {at}: no runtime built for it");
                continue;
            };
            let got = if ok { Ok(got) } else { Err(got) };
            // The executable prints nothing for `()`.
            let vm = vm.map(|v| if v == "()" { String::new() } else { v });
            assert_eq!(vm, got, "VM vs native, {arch:?} at {}\n{src}", opt.name());
            want = Some(got.unwrap_or_else(|e| e));
        }
    }
    want.expect("run at least once")
}

#[test]
fn typed_arithmetic_and_branches_run_natively() {
    let src = "fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)
               fun loop (n : Int) (acc : Float) = if n == 0 then acc else loop (n - 1) (_primAddF acc 1.5)
               fun g (a : Int) (b : Int) = (a / b, a % b, a * b - 7, a >= b, a != b)
               def result = (fib 20, loop 1000 0.0, g 17 5, g (0 - 9223372036854775807 - 1) (0 - 1))";
    assert_eq!(
        agrees("typed", src),
        "(6765, 1500.0, (3, 2, 78, True, True), (-9223372036854775808, 0, 9223372036854775801, False, True))"
    );
}

#[test]
fn everything_else_goes_through_the_interpreter() {
    let src = "use L.*\ndata L = Nil | Cons Int L
               fun build (n : Int) = if n == 0 then Nil else Cons n (build (n - 1))
               fun total xs = match xs with | Nil -> 0 | Cons x r -> x + total r
               fun twice f x = f (f x)
               def result = (total (build 1000), twice (\\x -> x * 3) 7, \"hi\", _primLtF 1.5 2.5)";
    assert_eq!(agrees("mixed", src), "(500500, 63, \"hi\", True)");
}

/// What a program does happens in the order it is written, compiled ahead of
/// time as much as in the VM: the program `differential.rs` runs on every
/// engine. See "Evaluation order" in `docs/TUTORIAL.md`.
#[test]
fn everything_is_evaluated_in_the_order_it_is_written() {
    assert_eq!(
        agrees("order", include_str!("evaluation_order.mw")),
        "(123, 123, 123, 12, 12, 123, 123, 12, 12, 123, 12, 12, 12, \
         1234, 12, 1234, 123, 12, 12, 12, 122, 1234567, 12)"
    );
}

/// Objects that have been promoted are read, matched on and invoked by native
/// code, not handed back to the interpreter -- and read *correctly*: the old
/// generation is separate blocks, reached through a table that moves as blocks
/// are added. A tree kept alive across many collections is made of old
/// objects, and so are the closures a long fold carries. The bug this pins was
/// a `wordfreq` crashing on an address read out of a freed copy of that table.
#[test]
fn promoted_objects_are_reached_by_native_code() {
    let src = "data T = Tip Int | Fork T T
               use T.*
               fun build (v : Int) (d : Int) : T =
                 if d <= 0 then Tip v else Fork (build v (d - 1)) (build (v + 1) (d - 1))
               fun check (t : T) : Int = match t with | Tip v -> v | Fork l r -> check l + check r
               fun churn (n : Int) (acc : Int) : Int =
                 if n == 0 then acc else churn (n - 1) (acc + check (build n 6))
               data Fs = Done | Then (Int -> Int) Fs
               use Fs.*
               fun apply (fs : Fs) (x : Int) : Int =
                 match fs with | Done -> x | Then f rest -> apply rest (f x)
               def result =
                 let lasting = build 1 14 in
                 let adders = Then (\\x -> x + 1) (Then (\\x -> x * 2) (Then (\\x -> x - 3) Done)) in
                 let c = churn 3000 0 in
                 (check lasting, c, apply adders 10)";
    assert_eq!(agrees("promoted", src), "(131072, 288672000, 19)");
}

/// What a compiler's own inner loops are made of, done by native code and
/// answering what the interpreter answers: a field read out of a constructor
/// with more fields than the second header word describes (twelve, then
/// thirty, young and promoted), a cell written with an integer and with a
/// reference, and the field `trmc` fills in after the call it turned into a
/// jump. Each used to be handed back: on a parser generator's benchmark that
/// was half a billion instructions.
#[test]
fn wide_fields_cells_and_filled_fields_run_natively() {
    let fields: Vec<String> = (0..30).map(|i| format!("(a + {i})")).collect();
    let binders: Vec<String> = (0..30).map(|i| format!("x{i}")).collect();
    let sum = binders.join(" + ");
    let src = format!(
        "data Wide = Wide Int Int Int Int Int Int Int Int Int Int Int Int\n\
         use Wide.*\n\
         fun wide (a : Int) : Wide = Wide a (a + 1) (a + 2) (a + 3) (a + 4) (a + 5) (a + 6) (a + 7) (a + 8) (a + 9) (a + 10) (a + 11)\n\
         fun total (w : Wide) : Int = match w with | Wide a b c d e f g h i j k l -> a + b + c + d + e + f + g + h + i + j + k + l\n\
         data Wider = Wider {ints}\n\
         use Wider.*\n\
         fun wider (a : Int) : Wider = Wider {fields}\n\
         fun totalWider (w : Wider) : Int = match w with | Wider {binders} -> {sum}\n\
         fun churn (n : Int) (acc : Int) : Int = if n == 0 then acc else churn (n - 1) (acc + total (wide n) + totalWider (wider n))\n\
         data L = Nil | Cons Int L\n\
         use L.*\n\
         fun upTo (i : Int) (n : Int) : L = if i == n then Nil else Cons i (upTo (i + 1) n)\n\
         fun len (xs : L) (acc : Int) : Int = match xs with | Nil -> acc | Cons _ r -> len r (acc + 1)\n\
         fun count (r : StRef s Int) (last : StRef s L) (n : Int) : Int ! {{ St s | e }} =\n\
           if n == 0 then stGetRef r + len (stGetRef last) 0\n\
           else (let _ = stSetRef r (stGetRef r + n) in let _ = stSetRef last (Cons n (stGetRef last)) in count r last (n - 1))\n\
         def result =\n\
           let w = wide 100 in\n\
           let v = wider 1000 in\n\
           let c = churn 20000 0 in\n\
           (total w, totalWider v, c, len (upTo 0 100000) 0, runSt (\\() -> count (stNewRef 0) (stNewRef Nil) 50000))\n",
        ints = vec!["Int"; 30].join(" "),
        fields = fields.join(" "),
        binders = binders.join(" "),
    );
    assert_eq!(
        agrees("wide", &src),
        "(1266, 30435, 8410440000, 100000, 1250075000)"
    );
}

/// `popCount`, `>>>` and `toFloat` on an `Int` are one instruction each, and
/// have to answer what the interpreter answers at the edges: a negative shifted
/// logically, a count of 64, a shift of 64 and of 100 (modulo 64, on every
/// machine), a large `Int` rounded to the nearest `Float`.
#[test]
fn unary_typed_instructions_agree_with_the_interpreter() {
    let src = "fun f (x : Int) (n : Int) = (popCount x, x >>> n, toFloat x)
               def result = (f 255 4, f (0 - 1) 60, f (0 - 1) 64, f 9007199254740993 100, f 0 0)";
    assert_eq!(
        agrees("unary", src),
        "((8, 15, 255.0), (64, 15, -1.0), (64, -1, -1.0), (2, 131072, 9007199254740992.0), (0, 0, 0.0))"
    );
}

#[test]
fn a_failure_is_the_same_failure() {
    let src = "fun f (a : Int) (b : Int) = a / b\ndef result = f 1 0";
    assert_eq!(agrees("fails", src), "division by zero");
}

#[test]
fn threads_run_natively_too() {
    let src = "fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)
               def result = let t = threadSpawn (\\() -> fib 18) in (fib 17, threadAwait t)";
    assert_eq!(agrees("threads", src), "(1597, 2584)");
}
