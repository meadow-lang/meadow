//! Green threads, as the type checker sees them.
//!
//! What they do at run time is in `glade/tests/differential.rs`, where both
//! schedulers are held to the same answers.

mod common;
use common::{errors, errors_std_with, eval_main_std, run_main_std, schemes_std};

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

// --- cancellation ------------------------------------------------------------
//
// What the schedulers do with a cancel is in `glade/tests/differential.rs`;
// these are the parts that need the standard library's effects: an
// operation the runtime answers is where a cancelled thread stops, as a thread
// operation is.

const ENGINES: [meadow::Engine; 3] = [meadow::Engine::Vm, meadow::Engine::Jit, meadow::Engine::Cek];

#[test]
fn the_cancel_api_has_the_thread_effect_in_its_types() {
    let out = schemes_std("use Std.Thread as T\ndef c = T.cancel\ndef d = T.cancelled\n");
    assert!(
        out.contains("c : forall a e. Task a -> () ! { Thread | e }"),
        "{out}"
    );
    assert!(
        out.contains("d : forall e. () -> Bool ! { Thread | e }"),
        "{out}"
    );
}

#[test]
fn a_cancelled_thread_stops_at_an_operation_the_runtime_answers() {
    // It asks the time, over and over, under a handler of its own, and makes
    // no thread operation: the clock is where it stops.
    let src = "use Std.Thread as T
               use Std.Time (monotonic)
               effect Ask { ask : () -> Int }
               fun loop (n : Int) = if monotonic () < 0 then n else loop (n + ask ())
               def result =
                 let t = T.spawn (\\() -> handle loop 0 with { ask u k -> k 1 }) in
                 let _ = T.yieldNow () in
                 let _ = T.cancel t in
                 T.await t
";
    for engine in ENGINES {
        assert_eq!(run_main_std(src, engine), "cancelled", "{engine}");
    }
}

#[test]
fn a_thread_cancelled_while_it_waits_on_a_socket_is_woken() {
    // Nobody ever connects: unless the cancel wakes the thread, `await` waits
    // for ever. `main` gives it a while to get into its accept first; the
    // accept itself goes on until the run ends.
    let src = "use Std.Thread as T
               use Std.Net as Net
               use Std.Time (sleep)
               def result =
                 match Net.listen \"127.0.0.1\" 0 with
                 | Err why -> why
                 | Ok l ->
                     let t = T.spawn (\\() -> match Net.accept l with | Ok _ -> \"accepted\" | Err why -> why) in
                     let _ = T.yieldNow () in
                     let _ = sleep 50 in
                     let _ = T.cancel t in
                     T.await t
";
    for engine in ENGINES {
        assert_eq!(run_main_std(src, engine), "cancelled", "{engine}");
    }
}

// --- sleeping ----------------------------------------------------------------

#[test]
fn a_sleeping_thread_is_woken_by_a_cancel() {
    // An hour's `Time.sleep`, on the scheduler's timer: the cancel wakes it.
    let src = "use Std.Thread as T
               use Std.Time (sleep)
               def result =
                 let t = T.spawn (\\() -> let _ = sleep 3600000 in \"slept\") in
                 let _ = sleep 20 in
                 let _ = T.cancel t in
                 T.await t
";
    for engine in ENGINES {
        assert_eq!(run_main_std(src, engine), "cancelled", "{engine}");
    }
}

#[test]
fn many_sleepers_share_the_workers() {
    // A thousand threads asleep at once, each a tenth of a second: together
    // they take about that long, however few the workers.
    let src = "use Std.Thread as T
               use Std.Time (sleep)
               use Std.Collections.Vector as V
               def result =
                 let ts = V.map (\\i -> T.spawn (\\() -> let _ = sleep 100 in i)) (V.range (toInt 0) 1000) in
                 V.foldl (\\acc t -> acc + T.await t) 0 ts
";
    let began = std::time::Instant::now();
    for engine in ENGINES {
        assert_eq!(run_main_std(src, engine), "499500", "{engine}");
    }
    let took = began.elapsed();
    assert!(took < std::time::Duration::from_secs(120), "took {took:?}");
}

// --- releases and time limits ------------------------------------------------

#[test]
fn the_cleanup_api_holds_releases_to_what_the_runtime_answers() {
    let out = schemes_std(
        "use Std.Thread as T\ndef b = T.bracket\ndef f = T.finally\ndef d = T.defer\n\
         def r = T.race\ndef t = T.timeout\n",
    );
    for want in [
        "b : forall a e b. (() -> a ! { Thread | e }) -> (a -> () ! Eff) -> (a -> b ! { Thread | e }) -> b ! { Thread | e }",
        "f : forall a e. (() -> () ! Eff) -> (() -> a ! { Thread | e }) -> a ! { Thread | e }",
        "d : forall e. (() -> () ! Eff) -> () ! { Thread | e }",
        "r : forall a b e. (() -> a ! Eff) -> (() -> b ! Eff) -> Either a b ! { Thread | e }",
        "t : forall a e. Int -> (() -> a ! Eff) -> Maybe a ! { Thread | e }",
    ] {
        assert!(out.contains(want), "{want}\n{out}");
    }
    // A release performing an effect only the program handles would have
    // nobody to answer it on the way out of a failure.
    let src = "use Std.Thread as T\n\
               effect Note { note : String -> () }\n\
               def result = handle T.finally (\\() -> note \"done\") (\\() -> toInt 1) with { note s k -> k () }\n";
    assert_eq!(
        errors_std_with(src, meadow::Options::debug().entry("result")),
        "type mismatch: the effect `Note` is not allowed here"
    );
}

#[test]
fn releases_run_as_brackets_end_and_as_threads_do() {
    let src = "use Std.Thread as T
               fun note log (n : Int) = setRef log (getRef log * 10 + n)
               def result =
                 let log = newRef (toInt 0) in
                 let s = T.bracket (\\() -> 1) (\\n -> note log n) (\\a ->
                   T.bracket (\\() -> 2) (\\n -> note log n) (\\b -> a + b)) in
                 let told = T.newChannel () in
                 let never = T.newChannel () in
                 let failing = T.spawn (\\() -> T.finally (\\() -> T.send told \"failed\") (\\() -> toInt 1 / toInt 0)) in
                 let f = T.receive told in
                 let waiting = T.spawn (\\() -> T.finally (\\() -> T.send told \"cancelled\") (\\() -> T.receive never)) in
                 let _ = T.cancel waiting in
                 (s, getRef log, f, T.receive told)
";
    for engine in ENGINES {
        assert_eq!(
            run_main_std(src, engine),
            "(3, 21, \"failed\", \"cancelled\")",
            "{engine}"
        );
    }
}

#[test]
fn a_time_limit_answers_in_time_or_stops_what_is_slow() {
    // A slow action is cancelled where it sleeps, its release runs before
    // `timeout` returns, and nothing of it is left: the run ends.
    let src = "use Std.Thread as T
               use Std.Time (sleep)
               def result =
                 let told = T.newChannel () in
                 let fast = T.timeout 60000 (\\() -> toInt 6 * 7) in
                 let slow = T.timeout 20 (\\() -> T.finally (\\() -> T.send told \"stopped\") (\\() -> let _ = sleep 3600000 in toInt 0)) in
                 (fast, slow, T.receive told)
";
    for engine in ENGINES {
        assert_eq!(
            run_main_std(src, engine),
            "(Just(42), None, \"stopped\")",
            "{engine}"
        );
    }
    // A race fails as its first thread to end did.
    let src = "use Std.Thread as T
               def result =
                 let never = T.newChannel () in
                 T.race (\\() -> toInt 1 / toInt 0) (\\() -> T.receive never)
";
    for engine in ENGINES {
        assert_eq!(run_main_std(src, engine), "division by zero", "{engine}");
    }
}
