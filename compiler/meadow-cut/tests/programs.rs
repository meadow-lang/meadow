//! Cut programs as a front end writes them: read, printed and read back the
//! same, and run by the reference interpreter -- and, those the lowering to
//! AxCut takes, by the AxCut machine, to the same answer.

use meadow_cut::interp::{Options, Outcome, run};
use meadow_cut::{parse, print};

/// `text` read, printed and read again -- the same program both times --
/// and run.
#[track_caller]
fn runs(text: &str) -> Outcome {
    let p = parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let printed = print::program(&p);
    let again = parse(&printed).unwrap_or_else(|e| panic!("{e}\n{printed}"));
    assert_eq!(p, again, "printed and read back:\n{printed}");
    let out = run(&p, &Options::default()).unwrap_or_else(|(e, out)| panic!("{e}\n{out:?}"));
    lowered_agrees(&p, &out);
    inlined_agrees(&p, &out);
    out
}

/// `p` with its small definitions inlined (`meadow_cut::inline`) does what
/// `p` does, run by the interpreter and lowered.
#[track_caller]
fn inlined_agrees(p: &meadow_cut::Program, out: &Outcome) {
    let inlined = meadow_cut::inline::program(p);
    let shown = print::program(&inlined);
    let again = parse(&shown).unwrap_or_else(|e| panic!("inlined, read back: {e}\n{shown}"));
    match run(&again, &Options::default()) {
        Ok(o) => {
            assert_eq!(o.output, out.output, "inlined:\n{shown}");
            assert_eq!(o.errors, out.errors, "inlined:\n{shown}");
            assert_eq!(o.status, out.status, "inlined:\n{shown}");
        }
        Err((e, _)) => panic!("inlined, it fails: {e}\n{shown}"),
    }
    lowered_agrees(&again, out);
    trmc_agrees(&again, out);
}

/// `p` with its calls under a constructor made tail calls that fill it in
/// (`meadow_cut::trmc`), as the back end runs it after inlining, reads back
/// as printed and does what `p` does on the AxCut machine. The reference
/// interpreter runs Cut as a front end writes it, which has no `setField`.
#[track_caller]
fn trmc_agrees(p: &meadow_cut::Program, out: &Outcome) {
    let rewritten = meadow_cut::trmc::program(p);
    let shown = print::program(&rewritten);
    let again = parse(&shown).unwrap_or_else(|e| panic!("rewritten, read back: {e}\n{shown}"));
    assert_eq!(
        rewritten, again,
        "rewritten, printed and read back:\n{shown}"
    );
    lowered_agrees(&again, out);
}

/// A small definition is inlined where it is called, and what that leaves
/// known -- a pair taken apart where it is made, an object called where it
/// is made, a branch on a literal -- is taken: nothing is called, built or
/// tested but the sum.
#[test]
fn a_small_definition_is_inlined_and_what_it_leaves_known_is_taken() {
    let text = format!(
        "cut 0\nentry t:Main/main\nanswer str\n{CONSOLE}\n\
         data t:Main/Pair {{ P(i64, i64) }}\n\n\
         def t:Main/pair (a: i64, b: i64; k: ptr) =\n  <t:Main/Pair.P(a, b) | k>\n\n\
         def t:Main/first (p: ptr; k: ptr) =\n  <p | case {{ t:Main/Pair.P(x: i64, y: i64) => <x | k> }}>\n\n\
         def t:Main/twice (f: ptr, x: i64; k: ptr) =\n  \
         <f | apply(x; μ̃ y: i64. <f | apply(y; k)>)>\n\n\
         def t:Main/main (; k: ptr) =\n  \
         t:Main/pair(20, 1; μ̃ p: ptr.\n    \
         t:Main/first(p; μ̃ a: i64.\n      \
         prim if(true; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit.\n        \
         t:Main/twice(cocase {{ apply(n: i64; j: ptr) => prim add(n, a; j) }}, 2; μ̃ r: i64.\n          \
         prim eq(r, 42; μ̃ w: unit. <\"wrong\" | k>, μ̃ w: unit. <\"42\" | k>)))))\n"
    );
    let out = runs(&text);
    assert_eq!(out.output, "42");
    let p = parse(&text).unwrap();
    let inlined = meadow_cut::inline::program(&p);
    let main = inlined
        .defs
        .iter()
        .find(|d| d.symbol.last() == "main")
        .unwrap();
    let shown = print::program(&meadow_cut::Program {
        defs: vec![main.clone()],
        ..inlined.clone()
    });
    for gone in [
        "t:Main/pair(",
        "t:Main/first(",
        "t:Main/twice(",
        "| case",
        "prim if",
    ] {
        assert!(!shown.contains(gone), "`{gone}` is left:\n{shown}");
    }
}

/// Where `p` is one the lowering to AxCut takes and its answer is a string,
/// the AxCut machine answers what the interpreter printed.
#[track_caller]
fn lowered_agrees(p: &meadow_cut::Program, out: &Outcome) {
    use meadow_axcut::machine::{Machine, Value};
    let Ok(lowered) = meadow_cut::lower::lower(p) else {
        return;
    };
    if p.answer != meadow_cut::Answer::Str {
        return;
    }
    match Machine::run(&lowered, 100_000_000) {
        Ok(Value::Str(s)) => assert_eq!(&*s, out.output, "what the AxCut machine answers"),
        Ok(v) => panic!("the AxCut machine answered {v}, not a string"),
        // The machine performs what is printed and nothing else of the world:
        // a program that asks more of it is one for a runtime.
        Err(e) if e.msg.starts_with("unhandled effect") => {}
        Err(e) => panic!("the AxCut machine: {e:?}\n{}", lowered.pretty()),
    }
}

/// What `text` fails with when run.
#[track_caller]
fn fails(text: &str) -> String {
    let p = parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    match run(&p, &Options::default()) {
        Ok(out) => panic!("ran to the end: {out:?}"),
        Err((e, _)) => e,
    }
}

const CONSOLE: &str = "
native {
  idyll:Prelude/Console.putStr = Console.writeOutput
  idyll:Prelude/Console.putErr = Console.writeError
  idyll:Prelude/Process.exit = Process.exit
}

effect idyll:Prelude/Console { putStr(str) -> unit; putErr(str) -> unit }
effect idyll:Prelude/Process { exit(i64) -> unit }
";

#[test]
fn a_definition_is_called_and_a_test_branches() {
    let out = runs(&format!(
        "cut 0
entry idyll:Main/\"#entry\"
answer none
{CONSOLE}
def idyll:Main/double (n: i64; k: ptr) =
  prim mul(n, 2; k)

def idyll:Main/\"#entry\" (; k: ptr) =
  idyll:Main/double(21; μ̃ n: i64.
    prim lt(n, 50;
      μ̃ u: unit. perform idyll:Prelude/Console.putStr(\"big\\n\"; k),
      μ̃ u: unit. perform idyll:Prelude/Console.putStr(\"small\\n\"; k)))
"
    ));
    assert_eq!(out.output, "small\n");
}

const NAT: &str = "
data idyll:Main/Nat { Z; S(ptr) }

def idyll:Main/plus (x: ptr, m: ptr; k: ptr) =
  <x | case {
    idyll:Main/Nat.Z => <m | k>;
    idyll:Main/Nat.S(n: ptr) =>
      idyll:Main/plus(n, m; μ̃ v: ptr. <idyll:Main/Nat.S(v) | k>)
  }>

def idyll:Main/count (x: ptr; k: ptr) =
  <x | case {
    idyll:Main/Nat.Z => <0 | k>;
    idyll:Main/Nat.S(n: ptr) => idyll:Main/count(n; μ̃ c: i64. prim add(c, 1; k))
  }>
";

#[test]
fn data_is_built_and_taken_apart_and_a_str_is_the_answer() {
    let out = runs(&format!(
        "cut 0
entry idyll:Main/main
answer str
{NAT}
def idyll:Main/main (; k: ptr) =
  idyll:Main/plus(idyll:Main/Nat.S(idyll:Main/Nat.Z), idyll:Main/Nat.S(idyll:Main/Nat.S(idyll:Main/Nat.Z)); μ̃ r: ptr.
    idyll:Main/count(r; μ̃ c: i64.
      prim eq(c, 3; μ̃ u: unit. <\"wrong\" | k>, μ̃ u: unit. <\"three\" | k>)))
"
    ));
    assert_eq!(out.output, "three");
}

#[test]
fn a_deep_handler_resumes_and_the_clause_gets_the_answer() {
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

effect idyll:Main/Ask { ask(unit) -> i64 }

def idyll:Main/main (; k: ptr) =
  handle {
    idyll:Main/Ask.ask(u: unit; r: ptr, k2: ptr) => <r | apply(10; k2)>;
    return(x: i64; k3: ptr) => <x | k3>
  } in μ b.
    perform idyll:Main/Ask.ask(unit; μ̃ a: i64.
      perform idyll:Main/Ask.ask(unit; μ̃ c: i64. prim add(a, c; b)))
  ; μ̃ total: i64. prim eq(total, 20; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"yes\" | k>)
",
    );
    assert_eq!(out.output, "yes");
}

#[test]
fn a_many_operation_is_resumed_twice_and_every_path_is_counted() {
    // `choose` resumed with true and with false; each path answers 1 or 2, and
    // the clause adds what the two resumptions came back with.
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

effect idyll:Main/Choose { @many choose(unit) -> bool }

def idyll:Main/main (; k: ptr) =
  handle {
    idyll:Main/Choose.choose(u: unit; r: ptr, k2: ptr) =>
      <r | apply(true; μ̃ a: i64.
        <r | apply(false; μ̃ b2: i64. prim add(a, b2; k2))>)>;
    return(x: i64; k3: ptr) => <x | k3>
  } in μ b.
    perform idyll:Main/Choose.choose(unit; μ̃ c: bool.
      prim if(c; μ̃ u: unit. <2 | b>, μ̃ u: unit. <1 | b>))
  ; μ̃ n: i64. prim eq(n, 3; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"three\" | k>)
",
    );
    assert_eq!(out.output, "three");
}

#[test]
fn a_val_is_computed_once_and_a_mu_runs_where_an_argument_is_wanted() {
    let out = runs(&format!(
        "cut 0
entry idyll:Main/main
answer none
{CONSOLE}
val idyll:Main/forty : i64 =
  perform idyll:Prelude/Console.putStr(\"once\\n\"; μ̃ u: unit. prim add(40, 2; halt))

def idyll:Main/id <'a = d> (d: desc, x: 'a; k: ptr) =
  <x | k>

def idyll:Main/main (; k: ptr) =
  idyll:Main/id(desc(i64), μ j. <idyll:Main/forty | j>; μ̃ v: i64.
    idyll:Main/id(desc(i64), idyll:Main/forty; μ̃ w: i64.
      prim add(v, w; μ̃ s: i64.
        prim eq(s, 84;
          μ̃ u: unit. perform idyll:Prelude/Console.putStr(\"no\\n\"; k),
          μ̃ u: unit. perform idyll:Prelude/Console.putStr(\"84\\n\"; k)))))
"
    ));
    assert_eq!(out.output, "once\n84\n", "the val's effect happens once");
}

#[test]
fn resume_is_apply_by_another_name_beside_an_object_of_other_methods() {
    // A resumption called by `resume`, in a program whose objects have a
    // method besides `apply`: the lowering once gave `resume` a slot of its
    // own, and the slots a resumption does not use no captures, which the
    // pass adding descriptors to environments ran off the end of.
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

effect idyll:Main/Ask { ask(unit) -> i64 }

def idyll:Main/main (; k: ptr) =
  let two: i64 = 2 in
  <cocase { twice(x: i64; k0: ptr) => prim mul(x, two; k0) } | twice(5; μ̃ ten: i64.
    handle {
      idyll:Main/Ask.ask(u: unit; r: ptr, k2: ptr) => <r | resume(ten; k2)>;
      return(x: i64; k3: ptr) => <x | k3>
    } in μ b.
      perform idyll:Main/Ask.ask(unit; μ̃ a: i64.
        perform idyll:Main/Ask.ask(unit; μ̃ c: i64. prim add(a, c; b)))
    ; μ̃ total: i64. prim eq(total, 20; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"yes\" | k>))>
",
    );
    assert_eq!(out.output, "yes");
}

#[test]
fn an_object_is_called_through_its_method() {
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

def idyll:Main/main (; k: ptr) =
  let one: i64 = 1 in
  <cocase { apply(x: i64; k1: ptr) => prim add(x, one; k1) } | apply(41; μ̃ v: i64.
    prim eq(v, 42; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"42\" | k>))>
",
    );
    assert_eq!(out.output, "42");
}

#[test]
fn a_program_gives_up_on_standard_error() {
    let out = runs(&format!(
        "cut 0
entry idyll:Main/main
answer none
{CONSOLE}
def idyll:Main/main (; k: ptr) =
  perform idyll:Prelude/Console.putStr(\"before\\n\"; μ̃ u: unit.
    perform idyll:Prelude/Console.putErr(\"gave up\\n\"; μ̃ u2: unit.
      perform idyll:Prelude/Process.exit(3; μ̃ u3: unit. perform idyll:Prelude/Console.putStr(\"after\\n\"; k))))
"
    ));
    assert_eq!(out.output, "before\n");
    assert_eq!(out.errors, "gave up\n");
    assert_eq!(out.status, 3);
}

#[test]
fn a_loop_of_a_million_turns_runs_in_constant_host_stack() {
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

def idyll:Main/down (n: i64, acc: i64; k: ptr) =
  prim eq(n, 0;
    μ̃ u: unit. prim sub(n, 1; μ̃ m: i64. prim add(acc, 2; μ̃ a: i64. idyll:Main/down(m, a; k))),
    μ̃ u: unit. <acc | k>)

def idyll:Main/main (; k: ptr) =
  idyll:Main/down(1000000, 0; μ̃ r: i64.
    prim eq(r, 2000000; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"done\" | k>))
",
    );
    assert_eq!(out.output, "done");
}

#[test]
fn integer_arithmetic_wraps_and_division_by_zero_is_an_error() {
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

def idyll:Main/main (; k: ptr) =
  prim add(9223372036854775807, 1; μ̃ x: i64.
    prim div(-7, 2; μ̃ q: i64.
      prim mod(-7, 2; μ̃ r: i64.
        prim eq(x, -9223372036854775808; μ̃ u: unit. <\"no wrap\" | k>, μ̃ u: unit.
          prim eq(q, -3; μ̃ u: unit. <\"no truncation\" | k>, μ̃ u: unit.
            prim eq(r, -1; μ̃ u: unit. <\"wrong sign\" | k>, μ̃ u: unit. <\"ok\" | k>))))))
",
    );
    assert_eq!(out.output, "ok");
    let e = fails(
        "cut 0
entry idyll:Main/main
answer none

def idyll:Main/main (; k: ptr) =
  prim div(1, 0; k)
",
    );
    assert_eq!(e, "division by zero");
}

#[test]
fn an_unhandled_operation_not_in_the_native_table_is_an_error() {
    let e = fails(
        "cut 0
entry idyll:Main/main
answer none

effect idyll:Main/Ask { ask(unit) -> i64 }

def idyll:Main/main (; k: ptr) =
  perform idyll:Main/Ask.ask(unit; μ̃ n: i64. <unit | k>)
",
    );
    assert!(e.contains("no handler answers it"), "{e}");
}

#[test]
fn the_file_system_is_written_read_and_cleared_through_natives() {
    let dir = std::env::temp_dir().join(format!("meadow-cut-fs-{}", std::process::id()));
    let dir = dir.to_string_lossy().replace('\\', "/");
    let out = runs(&format!(
        "cut 0
entry idyll:Main/main
answer str

data idyll:Prelude/Result {{ Ok(ptr); Err(str) }}
roles {{ ok = idyll:Prelude/Result.Ok, err = idyll:Prelude/Result.Err }}

native {{
  idyll:Prelude/Fs.createDirAll = Fs.createDirAll
  idyll:Prelude/Fs.writeString = Fs.writeString
  idyll:Prelude/Fs.appendString = Fs.appendString
  idyll:Prelude/Fs.readToString = Fs.readToString
  idyll:Prelude/Fs.exists = Fs.exists
  idyll:Prelude/Fs.removeDirAll = Fs.removeDirAll
}}

effect idyll:Prelude/Fs {{ createDirAll(str) -> ptr; writeString(str, str) -> ptr; appendString(str, str) -> ptr;
  readToString(str) -> ptr; exists(str) -> bool; removeDirAll(str) -> ptr }}

def idyll:Main/main (; k: ptr) =
  perform idyll:Prelude/Fs.createDirAll(\"{dir}\"; μ̃ a: ptr.
    perform idyll:Prelude/Fs.writeString(\"{dir}/f.txt\", \"ab\"; μ̃ b: ptr.
      perform idyll:Prelude/Fs.appendString(\"{dir}/f.txt\", \"cd\"; μ̃ c: ptr.
        perform idyll:Prelude/Fs.readToString(\"{dir}/f.txt\"; μ̃ r: ptr.
          <r | case {{
            idyll:Prelude/Result.Ok(text: str) =>
              perform idyll:Prelude/Fs.removeDirAll(\"{dir}\"; μ̃ d: ptr.
                perform idyll:Prelude/Fs.exists(\"{dir}\"; μ̃ there: bool.
                  prim if(there; μ̃ u: unit. <text | k>, μ̃ u: unit. <\"still there\" | k>)));
            idyll:Prelude/Result.Err(why: str) => <why | k>
          }}>))))
"
    ));
    assert_eq!(out.output, "abcd");
}

#[test]
fn quoted_segments_versions_and_comments_read() {
    let p = parse(
        "cut 0 -- the version
data idyll:Prelude/List <'a> { Nil; \"::\"('a, ptr) }
roles { nil = idyll:Prelude/List.Nil, cons = idyll:Prelude/List.\"::\" }
def idyll:Json@1.2.0-alpha.1/parse (s: str; k: ptr) = <s | k>
def idyll:Prelude/Nat.\"x'\" (; k: ptr) = <'c' | k>
",
    )
    .unwrap();
    assert_eq!(p.datas[0].ctors[1].0, "::");
    assert_eq!(p.roles[1].1.to_string(), "idyll:Prelude/List.\"::\"");
    assert_eq!(p.defs[0].symbol.package, "Json@1.2.0-alpha.1");
    assert_eq!(p.defs[1].symbol.path, ["Nat", "x'"]);
}

#[test]
fn a_reader_refuses_what_it_cannot_read() {
    assert!(parse("cut 1\n").unwrap_err().contains("version 1"));
    let e = parse("cut 0\ndef idyll:A/f (; k: ptr) = <idyll:A/K(1) | k>\n").unwrap_err();
    assert!(e.contains("no data declares it"), "{e}");
    let e = parse("cut 0\ndef idyll:A/f (; k: ptr) = <1 | \n").unwrap_err();
    assert!(e.starts_with("3:1:"), "says where: {e}");
}

#[test]
fn what_is_not_lowered_to_axcut_yet_is_refused_and_says_what() {
    let lowered = |text: &str| meadow_cut::lower::lower(&parse(text).expect("a program"));
    // A `μ` where an argument is wanted: nothing says how its value is
    // represented.
    let mu = lowered(
        "cut 0
entry idyll:Main/main
answer none

def idyll:Main/main (; k: ptr) =
  prim add(μ j. <1 | j>, 1; k)
",
    );
    assert!(mu.as_ref().is_err_and(|e| e.contains("`μ`")), "{mu:?}");
    // A representation variable nothing describes.
    let undescribed = lowered(
        "cut 0
entry idyll:Main/main
answer none

def idyll:Main/id (x: 'a; k: ptr) =
  <x | k>

def idyll:Main/main (; k: ptr) =
  idyll:Main/id(1; k)
",
    );
    assert!(
        undescribed
            .as_ref()
            .is_err_and(|e| e.contains("representation variable")),
        "{undescribed:?}"
    );
}

#[test]
fn a_mu_is_an_argument_of_a_definition() {
    // What it gives its continuation is the argument, and what is in scope
    // where it is written is still in scope after it.
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

def idyll:Main/sum (a: i64, b: i64; k: ptr) =
  prim add(a, b; k)

def idyll:Main/main (; k: ptr) =
  let twenty: i64 = 20 in
  idyll:Main/sum(μ j. prim add(twenty, 1; j), μ j. <twenty | j>; μ̃ n: i64.
    prim add(n, 1; μ̃ m: i64.
      prim eq(m, 42; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"42\" | k>)))
",
    );
    assert_eq!(out.output, "42");
}

#[test]
fn a_definition_generic_in_a_representation_takes_its_descriptor() {
    // The same definitions at an `i64` and at a `str`: a value passed
    // through, one taken out of data whose field is the variable, and one a
    // closure keeps -- each with its descriptor wherever it is, which the
    // AxCut machine checks.
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

data idyll:Main/Box <'a> { Box('a) }

def idyll:Main/id <'a = d> (d: desc, x: 'a; k: ptr) =
  <x | k>

def idyll:Main/unbox <'a = d> (b: ptr, d: desc; k: ptr) =
  <b | case { idyll:Main/Box.Box(v: 'a) => idyll:Main/id(d, v; k) }>

def idyll:Main/always <'a = d> (d: desc, x: 'a; k: ptr) =
  <cocase { apply(u: unit; k1: ptr) => <x | k1> } | k>

def idyll:Main/main (; k: ptr) =
  idyll:Main/id(desc(i64), 20; μ̃ n: i64.
    idyll:Main/unbox(idyll:Main/Box.Box(n), desc(i64); μ̃ m: i64.
      idyll:Main/always(desc(i64), 2; μ̃ f: ptr.
        <f | apply(unit; μ̃ two: i64.
          prim add(n, m; μ̃ s: i64.
            prim add(s, two; μ̃ t: i64.
              idyll:Main/always(desc(str), \"forty-two\"; μ̃ g: ptr.
                <g | apply(unit; μ̃ text: str.
                  prim eq(t, 42; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <text | k>))>))))>)))
",
    );
    assert_eq!(out.output, "forty-two");
}

#[test]
fn a_top_level_value_is_computed_once_and_read_on_the_axcut_machine() {
    // The start block computes each value in the order written, the second
    // of the first, before the entry runs.
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

val idyll:Main/twenty : i64 =
  prim add(19, 1; halt)

val idyll:Main/pair : ptr =
  <#tuple(idyll:Main/twenty, \"x\") | halt>

def idyll:Main/main (; k: ptr) =
  <idyll:Main/pair | case {
    #tuple(n: i64, s: str) =>
      prim add(n, idyll:Main/twenty; μ̃ m: i64.
        prim eq(m, 40; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <s | k>))
  }>
",
    );
    assert_eq!(out.output, "x");
}

#[test]
fn an_operation_the_runtime_performs_is_lowered_and_an_executable_prints_its_answer() {
    use meadow_axcut::{Extern, Statement};
    let p = parse(&format!(
        "cut 0
entry idyll:Main/main
answer str
{CONSOLE}
def idyll:Main/main (; k: ptr) =
  perform idyll:Prelude/Console.putStr(\"said\\n\"; μ̃ u: unit. <\"answered\" | k>)
"
    ))
    .expect("a program");
    // Every native operation of the program, by the runtime's names.
    fn natives(s: &Statement, out: &mut Vec<String>) {
        match s {
            Statement::Extern { op, blocks, .. } => {
                if let Extern::Native(effect, name) = op {
                    out.push(format!("{effect}.{name}"));
                }
                blocks.iter().for_each(|b| natives(&b.body, out));
            }
            Statement::Substitute(_, b) => natives(&b.body, out),
            Statement::Let { rest, .. } => natives(rest, out),
            Statement::New { methods, rest, .. } => {
                methods.iter().for_each(|b| natives(&b.body, out));
                natives(rest, out);
            }
            Statement::Switch { arms, default, .. } => {
                arms.iter().for_each(|(_, b)| natives(&b.body, out));
                natives(&default.body, out);
            }
            _ => {}
        }
    }
    let performed = |program: &meadow_axcut::Program| {
        let mut out = Vec::new();
        program
            .defs
            .iter()
            .for_each(|d| natives(&d.block.body, &mut out));
        out
    };
    let answering = meadow_cut::lower::lower(&p).expect("lowered");
    assert_eq!(performed(&answering), ["Console.writeOutput"]);
    // An executable writes its answer too, after what it said.
    let printing = meadow_cut::lower::executable(&p).expect("lowered");
    assert_eq!(
        performed(&printing),
        ["Console.writeOutput", "Console.writeOutput"]
    );
}

#[test]
fn a_records_field_is_read_and_set_by_its_label() {
    let out = runs(
        "cut 0
entry idyll:Main/main
answer str

def idyll:Main/main (; k: ptr) =
  <record { x = 1, name = \"a\" } | μ̃ r: ptr.
    prim extend(r, \"x\", 41; μ̃ r2: ptr.
      prim extend(r2, \"y\", 1; μ̃ r3: ptr.
        prim select(r3, \"x\"; μ̃ x: i64.
          prim select(r3, \"y\"; μ̃ y: i64.
            prim add(x, y; μ̃ s: i64.
              prim eq(s, 42; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit.
                prim select(r, \"name\"; k)))))))>
",
    );
    assert_eq!(out.output, "a");
    assert!(
        fails(
            "cut 0
entry idyll:Main/main
answer none

def idyll:Main/main (; k: ptr) =
  <record { x = 1 } | μ̃ r: ptr. prim select(r, \"y\"; k)>
"
        )
        .contains("no field `y`")
    );
}

#[test]
fn handlers_lowered_to_axcut_answer_as_the_interpreter_does() {
    // A clause that resumes and then goes on with what the resumed code
    // answered; one that does not resume; a handler inside another, an
    // operation of the outer's performed in the inner's body and in its
    // clause; and the `return` clause.
    let program = |main: &str| {
        format!(
            "cut 0
entry idyll:Main/main
answer str

effect idyll:Main/Ask {{ ask(unit) -> i64 }}
effect idyll:Main/Stop {{ stop(i64) -> unit }}

def idyll:Main/said (n: i64, want: i64; k: ptr) =
  prim eq(n, want; μ̃ u: unit. <\"no\" | k>, μ̃ u: unit. <\"yes\" | k>)

def idyll:Main/main (; k: ptr) =
{main}
"
        )
    };
    // 10 + 10, then one more for each resume: 22.
    let resumed = runs(&program(
        "  handle {
    idyll:Main/Ask.ask(u: unit; r: ptr, h: ptr) =>
      <r | apply(10; μ̃ v: i64. prim add(v, 1; h))>;
    return(x: i64; h: ptr) => <x | h>
  } in μ b.
    perform idyll:Main/Ask.ask(unit; μ̃ a: i64.
      perform idyll:Main/Ask.ask(unit; μ̃ c: i64. prim add(a, c; b)))
  ; μ̃ n: i64. idyll:Main/said(n, 22; k)",
    ));
    assert_eq!(resumed.output, "yes");
    // Not resumed: the clause's answer is the handler's, and the `return`
    // clause is not run.
    let stopped = runs(&program(
        "  handle {
    idyll:Main/Stop.stop(n: i64; r: ptr, h: ptr) => <n | h>;
    return(x: i64; h: ptr) => prim add(x, 1000; h)
  } in μ b.
    perform idyll:Main/Stop.stop(7; μ̃ u: unit. <1 | b>)
  ; μ̃ n: i64. idyll:Main/said(n, 7; k)",
    ));
    assert_eq!(stopped.output, "yes");
    // Nested: the inner handler answers `ask` with 1, and its `return`
    // clause asks the outer, which answers 100: 1 + 100.
    let nested = runs(&program(
        "  handle {
    idyll:Main/Ask.ask(u: unit; r: ptr, h: ptr) => <r | apply(100; h)>;
    return(x: i64; h: ptr) => <x | h>
  } in μ b.
    handle {
      idyll:Main/Ask.ask(u: unit; r: ptr, h: ptr) => <r | apply(1; h)>;
      return(x: i64; h: ptr) =>
        perform idyll:Main/Ask.ask(unit; μ̃ o: i64. prim add(x, o; h))
    } in μ inner.
      perform idyll:Main/Ask.ask(unit; inner)
    ; b
  ; μ̃ n: i64. idyll:Main/said(n, 101; k)",
    ));
    assert_eq!(nested.output, "yes");
}
