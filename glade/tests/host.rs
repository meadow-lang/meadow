//! A program hosted by another: its unhandled effects answered by the host.
//!
//! See `meadow_glade::host`. Each case is Meadow source with an effect of the
//! host's and no handler for it, run on a machine whose host is a closure
//! here.

use meadow_compiler::{Options, compile_str_with, core};
use meadow_glade::{Build, Owned, Vm};
use std::sync::{Arc, Mutex};

const FUEL: u64 = 20_000_000;

/// Compile `src`, whose entry point is the value `result`, to bytecode.
#[track_caller]
fn image(src: &str) -> meadow_bytecode::Program {
    let (pkg, diags) = compile_str_with("host", src, Options::default().entry("result"));
    let hard: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(hard.is_empty(), "compile errors:\n{}", hard.join("\n"));
    let prog = core::Program {
        defs: pkg.defs.clone(),
        entry: pkg.value_entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
    };
    let lowered = meadow_seq::lower_program(&prog, meadow_core::OptLevel::O1);
    assert!(
        lowered.unsupported.is_empty(),
        "lowering gave up on {:?}",
        lowered.unsupported
    );
    meadow_codegen::compile(&lowered.program)
        .unwrap_or_else(|e| panic!("codegen failed: {}", e.msg))
}

/// What the script of a small interface looks like: it shows something,
/// waits to be told what happened, and goes round again.
const APP: &str = "
    data Event = Clicked String | Tick Int | Resized (Int, Int) | Quit
    data Widget = Label String | Button String String | Column Widget Widget | Slider { at : Float, wide : Bool }
    use Event.*
    use Widget.*
    effect Neo {
      present : Widget -> (),
      next : () -> Event,
      setting : String -> { name : String, value : Int },
    }
    fun view (n : Int) = Column (Label \"count\") (Column (Button \"add\" \"Add one\") (Slider { at = 0.5, wide = n > 2 }))
    fun loop (n : Int) =
      let _ = present (view n) in
      match next () with
      | Clicked id -> if id == \"add\" then loop (n + 1) else loop n
      | Tick k -> loop (n + k)
      | Resized (w, h) -> loop (n + w * h)
      | Quit -> (n, (setting \"limit\").value)
    def result = loop 0";

#[test]
fn a_host_answers_the_effect_a_program_leaves_unhandled() {
    let image = image(APP);
    let mut vm = Vm::new(&image);
    let seen: Arc<Mutex<Vec<(String, String, Owned)>>> = Arc::default();
    let mut events = vec![
        Build::Ctor("Quit".into(), vec![]),
        Build::Ctor(
            "Event.Resized".into(),
            vec![Build::Tuple(vec![Build::int(2), Build::int(5)])],
        ),
        Build::Ctor("Clicked".into(), vec![Build::str("other")]),
        Build::Ctor("Tick".into(), vec![Build::int(3)]),
        Build::Ctor("Clicked".into(), vec![Build::str("add")]),
    ];
    let log = seen.clone();
    vm.host = Some(Box::new(move |effect, op, arg| {
        log.lock()
            .unwrap()
            .push((effect.to_owned(), op.to_owned(), arg.clone()));
        Ok(match op {
            "present" => Some(Build::unit()),
            "next" => events.pop(),
            // A record cannot be built by a host; a constructor declared with
            // the same fields can, and `.value` reads it the same.
            "setting" => Some(Build::int(0)),
            _ => None,
        })
    }));
    // `setting` answers with something that is not a record, to show that
    // what a host says is taken at its word and fails where it is used.
    let failed = vm.run(image.entry.expect("an entry"), FUEL).unwrap_err();
    assert!(!failed.msg.is_empty());
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|(e, op, _)| e == "Neo" && op == "present")
            .count(),
        5,
        "shown before each wait"
    );
    // What was shown first, and after three more had been counted, as the host sees it.
    let column = |a: Owned, b: Owned| Owned::Data("Column".into(), vec![a, b]);
    let label = Owned::Data("Label".into(), vec![Owned::Str("count".into())]);
    let button = Owned::Data(
        "Button".into(),
        vec![Owned::Str("add".into()), Owned::Str("Add one".into())],
    );
    let slider =
        |wide: bool| Owned::Data("Slider".into(), vec![Owned::Float(0.5), Owned::Bool(wide)]);
    assert_eq!(
        seen[0],
        (
            "Neo".into(),
            "present".into(),
            column(label.clone(), column(button.clone(), slider(false)))
        )
    );
    assert_eq!(
        seen[4].2,
        column(label, column(button, slider(true))),
        "one, and then three: more than two"
    );
    assert_eq!(seen[1], ("Neo".into(), "next".into(), Owned::Unit));
    assert_eq!(
        seen.last().unwrap(),
        &("Neo".into(), "setting".into(), Owned::Str("limit".into()))
    );
}

#[test]
fn what_a_program_answers_with_is_copied_out() {
    let src = "
        data Shape = Circle Float | Named { label : String, sides : Int }
        use Shape.*
        effect Host { limit : () -> Int, scale : Float -> Float, greet : String -> String, pair : () -> (Int, String) }
        def result =
          let (n, s) = pair () in
          { count = limit () + n, big = scale 2.0, said = greet \"hello\", shapes = (Circle 1.5, Named { label = s, sides = 3 }), ok = True, unit = (), letter = 'x' }";
    let image = image(src);
    let mut vm = Vm::new(&image);
    vm.host = Some(Box::new(|effect, op, arg| {
        assert_eq!(effect, "Host");
        Ok(Some(match (op, arg) {
            ("limit", Owned::Unit) => Build::int(40),
            ("scale", Owned::Float(x)) => Build::float(x * 10.0),
            ("greet", Owned::Str(s)) => Build::str(format!("{s}, world")),
            ("pair", _) => Build::Tuple(vec![Build::int(2), Build::str("triangle")]),
            _ => return Ok(None),
        }))
    }));
    let answer = vm
        .run(image.entry.expect("an entry"), FUEL)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    let answer = vm.owned(answer);
    assert_eq!(
        (
            answer.field("count").and_then(Owned::as_int),
            answer.field("big").and_then(Owned::as_float),
            answer.field("said").and_then(Owned::as_str)
        ),
        (Some(42), Some(20.0), Some("hello, world"))
    );
    assert_eq!(
        (
            answer.field("ok").and_then(Owned::as_bool),
            answer.field("unit"),
            answer.field("letter"),
            answer.field("missing")
        ),
        (
            Some(true),
            Some(&Owned::Unit),
            Some(&Owned::Char('x')),
            None
        )
    );
    assert_eq!(
        answer.field("shapes"),
        Some(&Owned::Tuple(vec![
            Owned::Data("Circle".into(), vec![Owned::Float(1.5)]),
            Owned::Data(
                "Named".into(),
                vec![Owned::Str("triangle".into()), Owned::Int(3)]
            )
        ]))
    );
    let Owned::Record(fields) = &answer else {
        panic!("a record: {answer:?}")
    };
    assert_eq!(
        fields.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        ["big", "count", "letter", "ok", "said", "shapes", "unit"],
        "sorted by label, as the machine keeps them"
    );
}

#[test]
fn an_operation_nobody_answers_is_still_unhandled_and_a_refusal_stops_the_program() {
    let src = "effect Host { known : () -> Int, unknown : () -> Int, refused : () -> Int }
               def result = known () + ";
    let run = |last: &str, host: bool| {
        let image = image(&format!("{src}{last} ()"));
        let mut vm = Vm::new(&image);
        if host {
            vm.host = Some(Box::new(|_, op, _| match op {
                "known" => Ok(Some(Build::int(1))),
                "refused" => Err("the host will not do that".into()),
                _ => Ok(None),
            }));
        }
        vm.run(image.entry.expect("an entry"), FUEL)
            .map(|v| vm.show(v))
            .map_err(|e| e.msg)
    };
    assert_eq!(run("known", true), Ok("2".into()));
    assert_eq!(
        run("unknown", true),
        Err("unhandled effect Host.unknown".into())
    );
    assert_eq!(
        run("refused", true),
        Err("the host will not do that".into())
    );
    // With no host it is as it always was.
    assert_eq!(
        run("known", false),
        Err("unhandled effect Host.known".into())
    );
}

#[test]
fn a_host_that_names_a_constructor_the_program_lacks_is_told_so() {
    let src = "data A = Leaf Int | Other
               data B = Leaf String
               effect Host { a : () -> A }
               def result = match a () with | A.Leaf n -> n | A.Other -> 0";
    let run = |answer: fn() -> Build| {
        let image = image(src);
        let mut vm = Vm::new(&image);
        vm.host = Some(Box::new(move |_, _, _| Ok(Some(answer()))));
        vm.run(image.entry.expect("an entry"), FUEL)
            .map(|v| vm.show(v))
            .map_err(|e| e.msg)
    };
    assert_eq!(
        run(|| Build::Ctor("A.Leaf".into(), vec![Build::int(7)])),
        Ok("7".into())
    );
    assert_eq!(
        run(|| Build::Ctor("Other".into(), vec![])),
        Ok("0".into()),
        "the only one called that"
    );
    // Two types have a `Leaf`: the bare name does not say which.
    let ambiguous = run(|| Build::Ctor("Leaf".into(), vec![Build::int(7)])).unwrap_err();
    assert_eq!(
        ambiguous,
        "Host.a: the host answered with Leaf, and this program has no one constructor called that"
    );
    let missing = run(|| Build::Tuple(vec![Build::Ctor("Nowhere".into(), vec![])])).unwrap_err();
    assert!(missing.contains("Nowhere"), "{missing}");
}

#[test]
fn a_hosted_program_survives_collections_between_the_hosts_answers() {
    // Enough garbage between operations that the nursery is collected many
    // times with the host's answers still in use.
    let src = "data L = Nil | Cons String L
               use L.*
               effect Host { word : Int -> String }
               fun build (n : Int) (acc : L) = if n == 0 then acc else build (n - 1) (Cons (word n) acc)
               fun count (l : L) (n : Int) = match l with | Nil -> n | Cons s rest -> count rest (if s == \"\" then n else n + 1)
               def result = count (build 200000 Nil) 0";
    let image = image(src);
    let mut vm = Vm::new(&image);
    vm.host = Some(Box::new(|_, _, arg| {
        Ok(Some(Build::str(format!(
            "word number {}",
            arg.as_int().unwrap_or(0)
        ))))
    }));
    let answer = vm
        .run(image.entry.expect("an entry"), 2_000_000_000)
        .unwrap_or_else(|e| panic!("{}", e.msg));
    assert_eq!(vm.owned(answer), Owned::Int(200_000));
}
