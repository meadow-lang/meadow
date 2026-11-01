//! What a program of Silo's that cannot spawn holds at the very top of its
//! stack is a root of its traces like anything below it.
//!
//! Such a program runs on the OS thread's stack, and a trace scans that from
//! where the program stopped up to where it began. If the place it began is
//! taken too low, the outermost frames are not scanned, and the first thing
//! they hold is `main`'s own continuation: the sweep frees it while the
//! program runs, and the program comes to it only when `main` answers. A
//! copy of it left behind lower down, by a call long since returned, is
//! scanned, and hides all this until something writes over it. So the
//! program below writes over it on purpose, with a recursion too deep to
//! leave one, before the one trace it needs starts.

use std::process::Command;

/// A list of 8000 cells and two recursions 400 deep. Only `main`'s frames
/// hold the `Ref` and `main`'s continuation while the first recursion runs,
/// and what it leaves on the stack is what the trace finds below them. The
/// list crosses the 4096 blocks a heap's first trace waits for, with a `Ref`
/// in use, so that trace starts while the stack is a few frames deep, and is
/// swept long before `main` answers.
const PROGRAM: &str = r#"cut 0
entry t:Main/main
answer str

data t:Main/L { Nil; Cons(i64, ptr) }

def t:Main/digits (n: i64, acc: ptr; k: ptr) =
  prim mod(n, 10; μ̃ d: i64.
    prim arrayGet(["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"], d; μ̃ s: str.
      prim lt(n, 10;
        μ̃ u: unit. prim div(n, 10; μ̃ q: i64. t:Main/digits(q, acc; μ̃ a: ptr. prim arrayPush(a, s; k))),
        μ̃ u: unit. prim arrayPush(acc, s; k))))

def t:Main/upto (i: i64, n: i64; k: ptr) =
  prim gt(i, n;
    μ̃ u: unit. prim add(i, 1; μ̃ j: i64. t:Main/upto(j, n; μ̃ r: ptr. <t:Main/L.Cons(i, r) | k>)),
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/sum (xs: ptr, acc: i64; k: ptr) =
  <xs | case { t:Main/L.Nil => <acc | k>;
               t:Main/L.Cons(x: i64, r: ptr) => prim add(acc, x; μ̃ a: i64. t:Main/sum(r, a; k)) }>

def t:Main/deep (n: i64, a: i64, b: i64, c: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. prim add(a, 1; μ̃ d: i64. prim add(b, 3; μ̃ e: i64. t:Main/deep(m, d, e, a; μ̃ r: i64.
      prim add(r, a; μ̃ s: i64. prim add(s, b; μ̃ t1: i64. prim add(t1, c; μ̃ v: i64. prim add(v, n; k)))))))),
    μ̃ u: unit. <1 | k>)

def t:Main/main (; k: ptr) =
  prim newRef(t:Main/L.Nil; μ̃ r: ptr.
  t:Main/deep(400, 1, 2, 3; μ̃ z: i64.
  t:Main/upto(1, 8000; μ̃ xs: ptr.
  prim setRef(r, xs; μ̃ v: unit.
  t:Main/deep(400, 1, 2, 3; μ̃ y: i64.
  prim getRef(r; μ̃ ys: ptr.
  t:Main/sum(ys, 0; μ̃ s: i64.
  t:Main/digits(s, []; μ̃ a: ptr. prim concatStrings(a; k)))))))))
"#;

/// Built, then run here rather than by `meadow cut`, which answers a signal
/// as a plain failure: this one, without the roots it needs, dies of one.
#[test]
fn a_trace_keeps_what_the_outermost_frames_of_main_hold() {
    let dir = meadow_scratch::scratch!("meadow-silo-roots-{}", std::process::id());
    std::fs::write(dir.join("top.cut"), PROGRAM).unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(&*dir)
        .args(["cut", "top.cut", "--runtime", "silo", "--no-run"])
        .output()
        .expect("the meadow binary runs");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let exe = dir
        .join("target/release/native/silo")
        .join(format!("top{}", std::env::consts::EXE_SUFFIX));
    let ran = Command::new(&exe)
        .envs([
            ("MEADOW_SILO_LEAKS", "1"),
            ("MEADOW_SILO_CYCLES", "1"),
            ("MEADOW_SILO_POISON", "ptr"),
        ])
        .output()
        .expect("the program runs");
    let out = String::from_utf8_lossy(&ran.stdout);
    let err = String::from_utf8_lossy(&ran.stderr);
    assert!(ran.status.success(), "{:?}\n{out}\n{err}", ran.status);
    // 1..=8000.
    assert_eq!(out, "32004000");
    let traces: u64 = err
        .lines()
        .find_map(|l| {
            l.strip_prefix("aot: ")?
                .split_once(" traces, ")?
                .0
                .parse()
                .ok()
        })
        .unwrap_or(0);
    assert!(traces >= 1, "no trace ran: {err}");
    // There is no cycle here: what a trace frees, it freed in use.
    assert!(err.contains(", 0 blocks freed by tracing"), "{err}");
    assert!(err.contains("aot: 0 blocks live at exit"), "leaked: {err}");
    assert!(
        err.contains("; 0 written to after, 0 freed twice"),
        "the poisoned heap says otherwise: {err}"
    );
}
