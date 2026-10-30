//! Compile-time bindings: a value one macro leaves for another to read.
//!
//! A procedural macro may `define` a name to stand for a `Datum` and a later
//! one may `lookup` it -- where "later" is decided by what each needs, not by
//! where it is written: a call that asks for a name nothing has defined yet is
//! set aside and run again once something has. See `docs/MACROS.md`.

use meadow::{Options, pipeline};
use std::path::Path;

/// A scratch package, `what` naming it apart from every other test's.
fn package(what: &str, manifest: &str, files: &[(&str, &str)]) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-ct-{}-{what}", std::process::id());
    std::fs::create_dir_all(dir.join("src")).expect("a scratch package");
    std::fs::write(dir.join("Meadow.toml"), manifest).expect("a manifest");
    for (name, text) in files {
        std::fs::write(dir.join("src").join(name), text).expect("a module");
    }
    dir
}

/// What the package at `dir` evaluates to, or the diagnostics that stopped it.
fn build(dir: &Path) -> Result<String, Vec<String>> {
    let out = pipeline::build(dir, Options::debug().entry("result"));
    if !out.diagnostics.is_empty() {
        return Err(out.diagnostics.iter().map(|d| d.msg.clone()).collect());
    }
    let linked = out.linked.expect("a linked program");
    Ok(meadow_eval::run(&linked.program)
        .expect("the program runs")
        .to_string())
}

/// The macros these tests call: `remember!(name 42)` leaves a number under a
/// name, `recall!(name)` writes back the number a name stands for, and
/// `sumOf!(name)` reads a `Point` with `lookupAs`.
const MAKER: &str = r#"
use Std.Macro (Datum, Reflect, lookup, define, lookupAs)
use Std.Macro.TokenTree.*
use Std.Macro.Loc.*
use Std.Collections.Vector as V

@macro
@pub fun remember ts =
  match (V.get ts 0, V.get ts 1) with
  | (Just (Word n _), Just (Num v _)) -> let u = define (n, Datum.Int v) in []
  | _ -> [Fail "remember!(name number)" Nowhere]

@macro
@pub fun recall ts =
  match V.get ts 0 with
  | Just (Word n _) ->
      (match lookup n with
       | Just (Datum.Int v) -> [Num v Nowhere]
       | Just other -> [Fail "`${n}` is not a number" Nowhere]
       | None -> [Fail "nothing is called `${n}`" Nowhere])
  | _ -> [Fail "recall!(name)" Nowhere]

@derive(Reflect)
@pub record Point = { x : Int, y : Int }

fun point (r : Result String Point) : Result String Point = r

@macro
@pub fun sumOf ts =
  match V.get ts 0 with
  | Just (Word n _) -> (match point (lookupAs n) with | Ok p -> [Num (p.x + p.y) Nowhere] | Err e -> [Fail e Nowhere])
  | _ -> [Fail "sumOf!(name)" Nowhere]
"#;

fn maker(what: &str) -> meadow_scratch::Scratch {
    package(
        &format!("{what}-maker"),
        "[package]\nname = \"Maker\"\nversion = \"0.1.0\"\n",
        &[("Lib.mw", MAKER)],
    )
}

/// A package that depends on `Maker`, and on whatever else `more` lists. The
/// `Maker` made for it is removed with it.
fn app(what: &str, files: &[(&str, &str)], more: &[(&str, &Path)]) -> meadow_scratch::Scratch {
    let maker = maker(what);
    let mut deps = format!("Maker = {{ path = {:?} }}\n", maker.display().to_string());
    for (name, dir) in more {
        deps.push_str(&format!(
            "{name} = {{ path = {:?} }}\n",
            dir.display().to_string()
        ));
    }
    package(
        &format!("{what}-app"),
        &format!("[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}"),
        files,
    )
    .holding(maker)
}

const USES: &str = "use Maker (remember!, recall!, sumOf!)\n";

#[test]
fn a_macro_reads_what_another_defined() {
    let dir = app(
        "reads",
        &[(
            "Lib.mw",
            &format!("{USES}remember!(answer 42)\n\ndef result = recall!(answer)\n"),
        )],
        &[],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn a_lookup_of_what_is_defined_below_it_waits_for_it() {
    // Written the other way round: the recall runs first, finds nothing yet,
    // and is run again once the remember has had its turn.
    let dir = app(
        "waits",
        &[(
            "Lib.mw",
            &format!("{USES}def result = recall!(answer)\n\nremember!(answer 42)\n"),
        )],
        &[],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn a_name_nothing_defines_is_answered_none_in_the_end() {
    let dir = app(
        "missing",
        &[("Lib.mw", &format!("{USES}def result = recall!(nowhere)\n"))],
        &[],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("nothing is called `nowhere`")),
        "{errs:?}"
    );
}

#[test]
fn a_binding_is_reached_from_another_module_through_a_use() {
    let dir = app(
        "modules",
        &[
            ("Facts.mw", &format!("{USES}@pub remember!(answer 42)\n")),
            (
                "Lib.mw",
                &format!(
                    "mod Facts\n\n{USES}use App.Facts (answer!)\n\ndef result = recall!(answer)\n"
                ),
            ),
        ],
        &[],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn a_binding_is_private_to_its_module_without_a_pub() {
    let dir = app(
        "private",
        &[
            (
                "Facts.mw",
                &format!("{USES}remember!(answer 42)\n\n@pub def unused = 0\n"),
            ),
            (
                "Lib.mw",
                &format!("mod Facts\n\n{USES}use App.Facts\n\ndef result = recall!(answer)\n"),
            ),
        ],
        &[],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("nothing is called `answer`")),
        "{errs:?}"
    );
}

#[test]
fn a_binding_crosses_packages_with_the_package_it_is_in() {
    // Defined in one package, read by a macro run in another: what makes a
    // language defined in a library usable by the passes of its dependents.
    let maker = maker("cross-facts");
    let facts = package(
        "cross-facts",
        &format!(
            "[package]\nname = \"Facts\"\nversion = \"0.1.0\"\n\n[dependencies]\nMaker = {{ path = {:?} }}\n",
            maker.display().to_string()
        ),
        &[("Lib.mw", &format!("{USES}@pub remember!(answer 42)\n"))],
    );
    let dir = app(
        "cross",
        &[(
            "Lib.mw",
            &format!("{USES}use Facts (answer!)\n\ndef result = recall!(answer)\n"),
        )],
        &[("Facts", &facts)],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn a_compile_time_def_is_a_binding_written_by_hand() {
    let dir = app(
        "handwritten",
        &[(
            "Lib.mw",
            &format!("{USES}@compileTime def answer = 42\n\ndef result = recall!(answer)\n"),
        )],
        &[],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn a_compile_time_def_is_data_and_is_not_run() {
    let dir = app(
        "notdata",
        &[(
            "Lib.mw",
            "fun f x = x\n\n@compileTime def answer = f 42\n\ndef result = 0\n",
        )],
        &[],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("a compile-time value is data, and a call is not")),
        "{errs:?}"
    );
}

#[test]
fn a_name_defined_twice_in_a_module_is_reported() {
    let dir = app(
        "twice",
        &[(
            "Lib.mw",
            &format!("{USES}remember!(answer 1)\n\nremember!(answer 2)\n\ndef result = 0\n"),
        )],
        &[],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(
        errs.iter()
            .any(|e| e.contains("`answer` is defined twice at compile time")),
        "{errs:?}"
    );
}

#[test]
fn reflect_reads_a_binding_back_as_the_type_it_was() {
    // A record's constructor applied to its fields, written by hand, read by a
    // macro as the `Point` the derive knows how to rebuild.
    let dir = app(
        "reflect",
        &[(
            "Lib.mw",
            &format!(
                "{USES}@compileTime def origin = Point {{ x = 40, y = 2 }}\n\ndef result = sumOf!(origin)\n"
            ),
        )],
        &[],
    );
    assert_eq!(build(&dir).expect("it builds"), "42");
}

#[test]
fn reflect_says_which_field_was_wrong() {
    let dir = app(
        "reflectbad",
        &[(
            "Lib.mw",
            &format!(
                "{USES}@compileTime def origin = {{ x = 40 }}\n\ndef result = sumOf!(origin)\n"
            ),
        )],
        &[],
    );
    let errs = build(&dir).expect_err("it does not build");
    assert!(errs.iter().any(|e| e.contains("no field `y`")), "{errs:?}");
}
