//! Tail recursion modulo context on both runtimes: a program of Cut whose
//! lists are built by calls under a constructor, run by `meadow cut` on
//! Glade and as an executable of Silo's, answering the same -- deep enough
//! that, as written, it does not fit in Silo's stack -- and leaving nothing
//! behind, with the heap poisoned and under the tracing collector.

use std::path::Path;
use std::process::Command;

fn scratch(who: &str) -> meadow_scratch::Scratch {
    meadow_scratch::scratch!("meadow-trmc-{}-{who}", std::process::id())
}

/// `meadow` run in `dir` with `args` and `env`: whether it succeeded, and
/// its standard output and error.
fn meadow(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Lists, the functions that build them under a constructor -- a map, a
/// filter, an append, a take, two that call each other -- and their text.
const LISTS: &str = "cut 0
entry t:Main/main
answer str

data t:Main/L { Nil; Cons(i64, ptr) }

def t:Main/digits (n: i64, acc: ptr; k: ptr) =
  prim mod(n, 10; μ̃ d: i64.
    prim arrayGet([\"0\", \"1\", \"2\", \"3\", \"4\", \"5\", \"6\", \"7\", \"8\", \"9\"], d; μ̃ s: str.
      prim lt(n, 10;
        μ̃ u: unit. prim div(n, 10; μ̃ q: i64. t:Main/digits(q, acc; μ̃ a: ptr. prim arrayPush(a, s; k))),
        μ̃ u: unit. prim arrayPush(acc, s; k))))

def t:Main/text (xs: ptr, acc: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <acc | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/digits(x, acc; μ̃ a: ptr. prim arrayPush(a, \" \"; μ̃ b: ptr. t:Main/text(r, b; k))) }>

def t:Main/upto (i: i64, n: i64; k: ptr) =
  prim gt(i, n;
    μ̃ u: unit. prim add(i, 1; μ̃ j: i64. t:Main/upto(j, n; μ̃ r: ptr. <t:Main/L.Cons(i, r) | k>)),
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/sum (xs: ptr, acc: i64; k: ptr) =
  <xs | case { t:Main/L.Nil => <acc | k>;
               t:Main/L.Cons(x: i64, r: ptr) => prim add(acc, x; μ̃ a: i64. t:Main/sum(r, a; k)) }>

def t:Main/len (xs: ptr, acc: i64; k: ptr) =
  <xs | case { t:Main/L.Nil => <acc | k>;
               t:Main/L.Cons(x: i64, r: ptr) => prim add(acc, 1; μ̃ a: i64. t:Main/len(r, a; k)) }>

def t:Main/map (f: ptr, xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 <f | apply(x; μ̃ y: i64. t:Main/map(f, r; μ̃ ys: ptr. <t:Main/L.Cons(y, ys) | k>))> }>

def t:Main/evens (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 prim mod(x, 2; μ̃ m: i64. prim eq(m, 0;
                   μ̃ u: unit. t:Main/evens(r; k),
                   μ̃ u: unit. t:Main/evens(r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>))) }>

def t:Main/append (xs: ptr, ys: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <ys | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/append(r, ys; μ̃ zs: ptr. <t:Main/L.Cons(x, zs) | k>) }>

def t:Main/take (n: i64, xs: ptr; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
                            t:Main/L.Cons(x: i64, r: ptr) =>
                              prim sub(n, 1; μ̃ m: i64. t:Main/take(m, r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>)) }>,
    μ̃ u: unit. <t:Main/L.Nil | k>)

def t:Main/odds (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/skip(r; μ̃ ys: ptr. <t:Main/L.Cons(x, ys) | k>) }>

def t:Main/skip (xs: ptr; k: ptr) =
  <xs | case { t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) => t:Main/odds(r; k) }>
";

/// A program over `1..=n`: tripled, the even ones kept, `1..=10` appended,
/// every other one kept; then its sum, its length and its first five.
fn lists(n: i64) -> String {
    format!(
        "{LISTS}
def t:Main/main (; k: ptr) =
  t:Main/upto(1, {n}; μ̃ xs: ptr.
  t:Main/map(cocase {{ apply(x: i64; j: ptr) => prim mul(x, 3; j) }}, xs; μ̃ ys: ptr.
  t:Main/evens(ys; μ̃ zs: ptr.
  t:Main/upto(1, 10; μ̃ small: ptr.
  t:Main/append(zs, small; μ̃ ws: ptr.
  t:Main/odds(ws; μ̃ os: ptr.
  t:Main/take(5, os; μ̃ first: ptr.
  t:Main/sum(os, 0; μ̃ s: i64.
  t:Main/len(os, 0; μ̃ m: i64.
  t:Main/digits(s, []; μ̃ a: ptr. prim arrayPush(a, \" \"; μ̃ b: ptr.
  t:Main/digits(m, b; μ̃ c: ptr. prim arrayPush(c, \" \"; μ̃ d: ptr.
  t:Main/text(first, d; μ̃ e: ptr. prim concatStrings(e; k)))))))))))))))
"
    )
}

/// What [`lists`] answers, worked out here.
fn answer(n: i64) -> String {
    let mut ws: Vec<i64> = (1..=n).map(|x| x * 3).filter(|x| x % 2 == 0).collect();
    ws.extend(1..=10);
    let os: Vec<i64> = ws.iter().copied().step_by(2).collect();
    let first: String = os.iter().take(5).map(|x| format!("{x} ")).collect();
    format!("{} {} {first}", os.iter().sum::<i64>(), os.len())
}

/// Ten million elements: as written, a frame for each of them, which Silo's
/// stack has not room for.
const DEEP: i64 = 10_000_000;

#[test]
fn deep_lists_are_built_in_constant_stack_on_both_runtimes() {
    let dir = scratch("lists");
    // Glade's frames are on the heap, so depth is only memory there: a
    // million elements, which a debug `meadow` runs in reasonable time.
    std::fs::write(dir.join("shallow.cut"), lists(1_000_000)).unwrap();
    let (ok, out, err) = meadow(&dir, &["cut", "shallow.cut"], &[]);
    assert!(ok, "Glade: {err}");
    assert_eq!(out, answer(1_000_000), "Glade");

    std::fs::write(dir.join("deep.cut"), lists(DEEP)).unwrap();
    let (ok, out, err) = meadow(
        &dir,
        &["cut", "deep.cut", "--runtime", "silo"],
        &[("MEADOW_SILO_LEAKS", "1"), ("MEADOW_SILO_POISON", "ptr")],
    );
    assert!(ok, "Silo: {err}");
    assert_eq!(out, answer(DEEP), "Silo");
    assert!(
        err.contains("aot: 0 blocks live at exit"),
        "Silo leaked: {err}"
    );
    assert!(
        err.contains("; 0 written to after, 0 freed twice"),
        "the poisoned heap says otherwise: {err}"
    );
}

/// Two cells at once, a tuple on the way to the hole, and a tree whose last
/// subtree is its hole: what each runtime's in-place write is given besides
/// a list's cell.
#[test]
fn nested_cells_tuples_and_trees_are_filled_alike_on_both_runtimes() {
    let dir = scratch("shapes");
    let text = format!(
        "{LISTS}
data t:Main/T {{ Leaf; Node(ptr, i64, ptr) }}
data t:Main/S {{ Stop; Step(ptr) }}

def t:Main/twice (xs: ptr; k: ptr) =
  <xs | case {{ t:Main/L.Nil => <t:Main/L.Nil | k>;
               t:Main/L.Cons(x: i64, r: ptr) =>
                 t:Main/twice(r; μ̃ ys: ptr. let a: ptr = t:Main/L.Cons(x, ys) in <t:Main/L.Cons(x, a) | k>) }}>

def t:Main/steps (n: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. t:Main/steps(m; μ̃ s: ptr. <t:Main/S.Step(#tuple(n, s)) | k>)),
    μ̃ u: unit. <t:Main/S.Stop | k>)

def t:Main/unstep (s: ptr; k: ptr) =
  <s | case {{ t:Main/S.Stop => <t:Main/L.Nil | k>;
              t:Main/S.Step(p: ptr) => <p | case {{ #tuple(n: i64, rest: ptr) =>
                t:Main/unstep(rest; μ̃ ns: ptr. <t:Main/L.Cons(n, ns) | k>) }}> }}>

def t:Main/build (d: i64, v: i64; k: ptr) =
  prim eq(d, 0;
    μ̃ u: unit. prim sub(d, 1; μ̃ e: i64. prim mul(v, 2; μ̃ w: i64.
      t:Main/build(e, w; μ̃ l: ptr. prim add(w, 1; μ̃ w1: i64.
        t:Main/build(e, w1; μ̃ r: ptr. <t:Main/T.Node(l, v, r) | k>))))),
    μ̃ u: unit. <t:Main/T.Leaf | k>)

def t:Main/flatten (t: ptr, rest: ptr; k: ptr) =
  <t | case {{ t:Main/T.Leaf => <rest | k>;
              t:Main/T.Node(l: ptr, v: i64, r: ptr) =>
                t:Main/flatten(r, rest; μ̃ rs: ptr. t:Main/flatten(l, t:Main/L.Cons(v, rs); k)) }}>

def t:Main/main (; k: ptr) =
  t:Main/upto(1, 3; μ̃ xs: ptr. t:Main/twice(xs; μ̃ a: ptr.
  t:Main/steps(200000; μ̃ s: ptr. t:Main/unstep(s; μ̃ b: ptr. t:Main/sum(b, 0; μ̃ n: i64.
  t:Main/build(4, 1; μ̃ tr: ptr. t:Main/flatten(tr, t:Main/L.Nil; μ̃ c: ptr.
  t:Main/odds(c; μ̃ d: ptr. t:Main/append(a, d; μ̃ e: ptr.
  t:Main/text(e, []; μ̃ f: ptr. t:Main/digits(n, f; μ̃ g: ptr. prim concatStrings(g; k))))))))))))
"
    );
    std::fs::write(dir.join("shapes.cut"), text).unwrap();
    // `1 1 2 2 3 3`; then every other node, in order, of a tree of depth
    // four numbered from 1 at its root, which are the nodes of its deepest
    // level; then the sum of 1..=200000.
    let want = "1 1 2 2 3 3 8 9 10 11 12 13 14 15 20000100000";
    for runtime in [&[][..], &["--runtime", "silo"]] {
        let mut args = vec!["cut", "shapes.cut"];
        args.extend_from_slice(runtime);
        let (ok, out, err) = meadow(
            &dir,
            &args,
            &[("MEADOW_SILO_LEAKS", "1"), ("MEADOW_SILO_POISON", "ptr")],
        );
        assert!(ok, "{runtime:?}: {err}");
        assert_eq!(out, want, "{runtime:?}");
        if !runtime.is_empty() {
            assert!(err.contains("aot: 0 blocks live at exit"), "leaked: {err}");
            assert!(
                err.contains("; 0 written to after, 0 freed twice"),
                "the poisoned heap says otherwise: {err}"
            );
        }
    }
}

/// Records of eight integers, four million of them, each rebuilt rotated by
/// a call under a constructor -- on a thread, whose stack is 256 MiB (`main`
/// has a GiB). As written each call keeps the eight fields it loaded, the
/// block they came from and its continuation across the next: some 350 MB
/// of frames, which do not fit. Rewritten, the frames are one.
#[test]
fn a_deep_context_on_a_thread_runs_in_constant_stack_and_as_written_does_not_fit() {
    let dir = scratch("wide");
    std::fs::write(
        dir.join("wide.cut"),
        "cut 0
entry t:Main/main
answer str

data t:Main/R { End; R(i64, i64, i64, i64, i64, i64, i64, i64, ptr) }

def t:Main/build (i: i64, n: i64; k: ptr) =
  prim gt(i, n;
    μ̃ u: unit. prim add(i, 1; μ̃ j: i64. prim mul(i, 2; μ̃ b: i64.
      t:Main/build(j, n; μ̃ r: ptr. <t:Main/R.R(i, b, i, i, i, i, i, i, r) | k>))),
    μ̃ u: unit. <t:Main/R.End | k>)

def t:Main/rotate (xs: ptr; k: ptr) =
  <xs | case { t:Main/R.End => <t:Main/R.End | k>;
               t:Main/R.R(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64, r: ptr) =>
                 t:Main/rotate(r; μ̃ w: ptr. <t:Main/R.R(b, c, d, e, f, g, h, a, w) | k>) }>

def t:Main/total (w: ptr, acc: i64; k: ptr) =
  <w | case { t:Main/R.End => <acc | k>;
              t:Main/R.R(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64, r: ptr) =>
                prim add(acc, a; μ̃ s: i64. prim sub(s, h; μ̃ t: i64. t:Main/total(r, t; k))) }>

def t:Main/main (; k: ptr) =
  prim threadSpawn(desc(i64), cocase { apply(u: unit; j: ptr) =>
      t:Main/build(1, 4000000; μ̃ xs: ptr. t:Main/rotate(xs; μ̃ ys: ptr. t:Main/total(ys, 0; j))) };
    μ̃ t: ptr. prim threadAwait(t; μ̃ s: i64.
      prim eq(s, 8000002000000; μ̃ u: unit. <\"a wrong sum\" | k>, μ̃ u: unit. <\"ran\" | k>)))
",
    )
    .unwrap();
    // Each record's second field less its first, rotated: every `i` once.
    // Silo alone: Glade's frames are on the heap, where depth is memory, and
    // the lists above are its check.
    let (ok, out, err) = meadow(
        &dir,
        &["cut", "wide.cut", "--runtime", "silo"],
        &[("MEADOW_SILO_LEAKS", "1")],
    );
    assert!(ok, "{err}");
    assert_eq!(out, "ran");
    assert!(err.contains("aot: 0 blocks live at exit"), "leaked: {err}");
    let (ok, out, err) = meadow(
        &dir,
        &["cut", "wide.cut", "--runtime", "silo"],
        &[("MEADOW_CUT_TRMC", "0")],
    );
    assert!(!ok, "ran to {out:?} as written");
    // At run time: it is read and lowered as it is with the rewrite.
    assert!(!err.contains("wide.cut:"), "{err}");
}

/// Lists built in place while the tracing collector marks: what a program
/// keeps in a `Ref` -- which is what makes Silo build it with the tracer --
/// is the list the last pass built, and each pass builds a new one, filling
/// each cell's hole as traces run. The hole holds a nullary constructor,
/// which no barrier logs; the cells are new, which marking has already
/// counted live.
#[test]
fn lists_built_in_place_while_the_tracer_marks_are_kept_and_freed() {
    let dir = scratch("traced");
    let text = format!(
        "{LISTS}
def t:Main/passes (i: i64, r: ptr; k: ptr) =
  prim eq(i, 0;
    μ̃ u: unit. prim getRef(r; μ̃ xs: ptr.
      t:Main/map(cocase {{ apply(x: i64; j: ptr) => prim add(x, 1; j) }}, xs; μ̃ ys: ptr.
      prim setRef(r, ys; μ̃ v: unit. prim sub(i, 1; μ̃ m: i64. t:Main/passes(m, r; k))))),
    μ̃ u: unit. prim getRef(r; k))

def t:Main/main (; k: ptr) =
  t:Main/upto(1, 200000; μ̃ xs: ptr. prim newRef(xs; μ̃ r: ptr.
  t:Main/passes(30, r; μ̃ ys: ptr. t:Main/sum(ys, 0; μ̃ s: i64.
  t:Main/digits(s, []; μ̃ a: ptr. prim concatStrings(a; k))))))
"
    );
    std::fs::write(dir.join("traced.cut"), text).unwrap();
    let (ok, out, err) = meadow(
        &dir,
        &["cut", "traced.cut", "--runtime", "silo"],
        &[
            ("MEADOW_SILO_LEAKS", "1"),
            ("MEADOW_SILO_CYCLES", "1"),
            ("MEADOW_SILO_POISON", "ptr"),
        ],
    );
    assert!(ok, "{err}");
    // Each of 1..=200000, thirty more.
    assert_eq!(out, (200_000i64 * 200_001 / 2 + 30 * 200_000).to_string());
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
    assert!(err.contains("aot: 0 blocks live at exit"), "leaked: {err}");
    assert!(
        err.contains("; 0 written to after, 0 freed twice"),
        "the poisoned heap says otherwise: {err}"
    );
}
