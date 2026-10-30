//! What a thread leaves behind as it ends -- its releases, its sockets --
//! and `Std.Thread`'s time limits, through the front door: `meadow run` with
//! every backend, in debug and release, Silo's executables and Glade's
//! included. `docs/CANCELLATION.md` is the design; the engines' own tests are
//! in `glade/tests/differential.rs` and `compiler/meadow-llvm/tests/native.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A package of its own, `main` its program, in a directory this test owns.
fn package(who: &str, main: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-cleanup-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        format!(
            "[package]\nname = \"{}\"\n",
            meadow::package::as_package_name(who)
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/Main.mw"), main).unwrap();
    dir
}

fn meadow(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `meadow run` of the package at `dir`, with each backend, in debug and in
/// release: whether it succeeded, and what it printed, every time.
fn every_backend(dir: &Path) -> Vec<(Vec<&'static str>, bool, String, String)> {
    let mut runs = Vec::new();
    for profile in [&[][..], &["--release"]] {
        for engine in [
            &["--backend", "vm"][..],
            &["--backend", "jit"],
            &["--backend", "aot"],
            &["--cek"],
            &["--runtime", "silo"],
        ] {
            let mut args = vec!["run"];
            args.extend_from_slice(profile);
            args.extend_from_slice(engine);
            args.push(".");
            let (ok, out, err) = meadow(dir, &args);
            runs.push((args, ok, out, err));
        }
    }
    runs
}

const RELEASES: &str = r#"use Std.Thread as Thread
use Std.Time (sleep)

fun note log (n : Int) = setRef log (getRef log * 10 + n)

fun main () =
  let log = newRef 0 in
  let s =
    Thread.bracket (\() -> 1) (\n -> note log n) (\a ->
      Thread.bracket (\() -> 2) (\n -> note log n) (\b -> a + b))
  in
  let told = Thread.newChannel () in
  let never = Thread.newChannel () in
  let f = Thread.spawn (\() -> Thread.finally (\() -> Thread.send told "failed") (\() -> 1 / 0)) in
  let first = Thread.receive told in
  let w =
    Thread.spawn (\() -> Thread.finally (\() -> Thread.send told "cancelled") (\() -> Thread.receive never))
  in
  let _ = Thread.cancel w in
  let second = Thread.receive told in
  let fast = Thread.timeout 60000 (\() -> 6 * 7) in
  let slow = Thread.timeout 20 (\() -> let _ = sleep 3600000 in 0) in
  println "${s} ${getRef log} ${first} ${second} ${fast:?} ${slow:?}"
"#;

#[test]
fn releases_and_time_limits_work_on_every_backend() {
    let dir = package("releases", RELEASES);
    for (args, ok, out, err) in every_backend(&dir) {
        assert!(ok, "{args:?}: {err}");
        assert_eq!(out, "3 21 failed cancelled Just(42) None\n", "{args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

const FAILING_MAIN: &str = r#"use Std.Thread as Thread

fun main () =
  Thread.finally (\() -> println "released first") (\() ->
    Thread.finally (\() -> println "released second") (\() -> let x = 1 / 0 in println "${x}"))
"#;

#[test]
fn a_failing_main_runs_its_releases_before_the_program_stops() {
    let dir = package("failing-main", FAILING_MAIN);
    for (args, ok, out, err) in every_backend(&dir) {
        assert!(!ok, "{args:?} should have failed");
        assert_eq!(out, "released second\nreleased first\n", "{args:?}");
        assert!(err.contains("division by zero"), "{args:?}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

const SOCKETS: &str = r#"use Std.Thread as Thread
use Std.Net as Net
use Std.Time (sleep)
use Std.Result.Result.*

-- Whether connecting to `port` is refused within `n` tries.
fun refusedSoon port (n : Int) =
  if n == 0 then False
  else
    match Net.connect "127.0.0.1" port with
    | Err _ -> True
    | Ok s -> let _ = Net.close s in let _ = sleep 10 in refusedSoon port (n - 1)

fun readSome s = match Net.readText s 64 with | Ok t -> t | Err why -> why

fun portOf l = match Net.port l with | Ok p -> p | Err _ -> 0

fun main () =
  let never = Thread.newChannel () in
  -- A listener a cancelled thread opened goes with it.
  let ports = Thread.newChannel () in
  let holder =
    Thread.spawn (\() ->
      match Net.listen "127.0.0.1" 0 with
      | Err why -> Thread.send ports 0
      | Ok l -> let _ = Thread.send ports (portOf l) in Thread.receive never)
  in
  let p = Thread.receive ports in
  let _ = Thread.cancel holder in
  let _ = println (if refusedSoon p 500 then "listener closed" else "listener open") in
  match Net.listen "127.0.0.1" 0 with
  | Err why -> println why
  | Ok l ->
      let q = portOf l in
      -- So does a stream: this end reads to the end of it.
      let client = Thread.spawn (\() -> let c = Net.connect "127.0.0.1" q in Thread.receive never) in
      (match Net.accept l with
       | Err why -> println why
       | Ok (s, peer) ->
           let _ = Thread.cancel client in
           let _ = println "cancelled: [${readSome s}]" in
           -- A stream handed to another thread is that thread's: the one
           -- that opened it ends, and it stays open until the other does.
           let go = Thread.newChannel () in
           let opener =
             Thread.spawn (\() ->
               match Net.connect "127.0.0.1" q with
               | Err why -> ()
               | Ok c ->
                   let _ = Thread.spawn (\() -> let _ = Thread.receive go in Net.writeText c "hi") in
                   ())
           in
           let _ = Thread.await opener in
           (match Net.accept l with
            | Err why -> println why
            | Ok (s2, peer2) ->
                let _ = Thread.send go () in
                let got = readSome s2 in
                println "handed on: [${got}] then [${readSome s2}]"))
"#;

#[test]
fn a_thread_that_ends_closes_the_sockets_it_holds() {
    let dir = package("sockets", SOCKETS);
    for (args, ok, out, err) in every_backend(&dir) {
        assert!(ok, "{args:?}: {err}");
        assert_eq!(
            out, "listener closed\ncancelled: []\nhanded on: [hi] then []\n",
            "{args:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
