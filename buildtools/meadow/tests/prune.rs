//! Pruning a program to what its entry reaches (`meadow_core::prune`) must not
//! change what it does -- on any machine. The REPL runs every line that way.
//!
//! Each program below runs whole and pruned, on the CEK machine, the bytecode
//! VM and the JIT, and has to answer the same both ways -- and pruning has to
//! have actually dropped something, or the test proves nothing.

use meadow::Options;
use meadow::pipeline;
use meadow_compiler::core::prune::prune;

fn same_pruned(src: &str) {
    let (program, diags) =
        pipeline::compile_str_with_std("prune", src, Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "compile errors:\n{}\n{src}",
        diags
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let pruned = prune(&program);
    assert!(
        pruned.defs.len() < program.defs.len() / 2,
        "pruning kept {} of {} definitions\n{src}",
        pruned.defs.len(),
        program.defs.len()
    );
    for engine in [meadow::Engine::Cek, meadow::Engine::Vm, meadow::Engine::Jit] {
        let opt = Options::debug().entry("result").opt;
        let whole = meadow::runtime::run(&program, engine, opt);
        let small = meadow::runtime::run(&pruned, engine, opt);
        assert_eq!(whole, small, "whole vs pruned on {engine}\n{src}");
    }
}

#[test]
fn a_constant() {
    same_pruned("def result = \"hello\"");
}

#[test]
fn prelude_collections_and_strings() {
    same_pruned(
        "use Std.String as S
         def result = (map (\\x -> x * 2) [1, 2, 3], S.split \",\" \"a,b,c\", Just 3, foldl (\\a b -> a + b) 0 (range 0 100))",
    );
}

#[test]
fn json_round_trip() {
    same_pruned(
        "use Std.Json as J
         def result = match J.parse \"{\\\"a\\\": [1, 2.5, true, null]}\" with
           | Ok j -> J.render j
           | Err e -> e",
    );
}

#[test]
fn a_user_type_and_a_record() {
    same_pruned(
        "data Shape = Circle Float | Square Float
         fun area s = match s with | Shape.Circle r -> 3.0 * r * r | Shape.Square a -> a * a
         def result = (area (Shape.Circle 2.0), { name = \"sq\", area = area (Shape.Square 3.0) })",
    );
}

#[test]
fn effects_and_handlers() {
    same_pruned(
        "use Std.Time as T
         use Std.Random as R
         def result = (T.withClock 500 (\\() -> T.formatTimestamp (T.now ())), R.withSeed 42 (\\() -> R.between 1 100))",
    );
}

#[test]
fn threads_channels_and_stm() {
    same_pruned(
        "use Std.Thread as Thread
         use Std.Stm as Stm
         fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)
         def result =
           let t = Thread.spawn (\\() -> fib 15) in
           let tv = Stm.newTVarIO (toInt 1) in
           let _ = Stm.atomically (\\() -> Stm.modifyTVar tv (\\x -> x + 41)) in
           (Thread.await t, Stm.readTVarIO tv)",
    );
}

#[test]
fn a_native_answer_the_program_never_takes_apart() {
    // The runtime builds this `Result` by name; the program only shows it, so
    // nothing in the pruned program's own code gives `Result.Err` a tag.
    same_pruned(
        "use Std.Fs as Fs
         def result = Fs.readToString \"this-file-is-not-there.txt\"",
    );
}
