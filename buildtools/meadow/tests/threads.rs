//! Green threads, as the type checker sees them.
//!
//! What they do at run time is in `glade/tests/differential.rs`, where both
//! schedulers are held to the same answers.

mod common;
use common::{errors, errors_std_with, eval_main_std, schemes_std};

#[test]
fn the_thread_api_has_the_thread_effect_in_its_types() {
    let out =
        schemes_std("use Std.Thread as T\ndef s = T.spawn\ndef a = T.await\ndef r = T.receive\n");
    assert!(
        out.contains("s : forall a e. (() -> a ! Eff) -> Task a ! { Thread | e }"),
        "{out}"
    );
    assert!(
        out.contains("a : forall a e. Task a -> a ! { Thread | e }"),
        "{out}"
    );
    assert!(
        out.contains("r : forall a e. Channel a -> a ! { Thread | e }"),
        "{out}"
    );
}

#[test]
fn a_thread_may_not_perform_an_effect_only_its_spawner_handles() {
    // A spawned thread starts with no handlers; `Log` would have nobody to
    // answer it.
    let src = "effect Log { log : String -> () }\n\
               def result = threadAwait (threadSpawn (\\() -> log \"hello\"))\n";
    assert_eq!(
        errors(src),
        "type mismatch: the effect `Log` is not allowed here"
    );
}

#[test]
fn a_thread_may_perform_what_the_runtime_answers() {
    let src = "use Std.Thread as T\n\
               def result = T.await (T.spawn (\\() -> let _ = println \"hi\" in toInt 1))\n";
    assert_eq!(
        errors_std_with(src, meadow::Options::debug().entry("result")),
        ""
    );
}

#[test]
fn a_thread_may_handle_its_own_effects() {
    let src = "effect Log { log : String -> () }\n\
               def result = threadAwait (threadSpawn (\\() -> handle (let _ = log \"x\" in toInt 1) with { log m k -> toInt 3 }))\n";
    assert_eq!(errors(src), "");
}

#[test]
fn the_thread_effect_can_be_written_in_an_annotation() {
    let src = "fun spawnIt (f : () -> Int ! { Thread }) = f ()\ndef result = 0\n";
    assert_eq!(errors(src), "");
    let src = "fun cell (f : () -> Int ! { Mut }) = f ()\ndef result = 0\n";
    assert_eq!(errors(src), "");
}

#[test]
fn a_program_with_threads_runs_through_the_front_door() {
    let src = "use Std.Thread as T\n\
               fun fib (n : Int) = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
               def result = T.parMap (\\n -> fib n) [10, 15, 20]\n";
    assert_eq!(eval_main_std(src), "[55, 610, 6765]");
}
