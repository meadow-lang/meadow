//! Choosing which tests `meadow test` runs.
//!
//! Through the real binary, because what is under test is the command line an
//! editor's Test lens builds: `meadow test <package> --exact <Module.test>`.
//!
//! Two things make choosing one test harder than it looks, and a runner that
//! got either wrong would have the lens run something other than what was
//! clicked. A filter is a *substring*, so `parse` also runs `parseInt`. And
//! modules are namespaces, so two of them may each declare a test called
//! `works` — by bare name, there is no telling them apart.

use meadow_compiler::TestSite;
use std::path::Path;
use std::process::Command;

/// A package with the awkward cases in it: a test name repeated across two
/// modules (one passing, one failing, so running the wrong one is visible),
/// and a name that is a prefix of another.
fn package(who: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-test-select-{}-{who}", std::process::id());
    std::fs::create_dir_all(dir.join("src").join("Deep")).unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\nname = \"Sel\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("Main.mw"),
        "use Std.Test (assertEq)\n\ndef result = 0\n\n@test fun root u = assertEq 1 1 \"root\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("A.mw"),
        "use Std.Test (assertEq)\n\n\
         @test fun works u = assertEq 1 1 \"a passes\"\n\n\
         @test fun parse u = assertEq 1 1 \"parse\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("B.mw"),
        "use Std.Test (assertEq)\n\n\
         @test fun works u = assertEq 1 2 \"b fails\"\n\n\
         @test fun parseInt u = assertEq 1 1 \"parseInt\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("Deep").join("Er.mw"),
        "use Std.Test (assertEq)\n\n@test fun nested u = assertEq 1 1 \"nested\"\n",
    )
    .unwrap();
    dir
}

/// The tests a run reported, in order, as `(name, passed)`.
fn ran(dir: &Path, args: &[&str]) -> Vec<(String, bool)> {
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .arg("test")
        .arg(dir)
        .args(args)
        .output()
        .expect("run meadow");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("test "))
        .filter_map(|l| {
            let (name, result) = l.split_once(" ... ")?;
            Some((name.to_string(), result == "ok"))
        })
        .collect()
}

fn names(r: &[(String, bool)]) -> Vec<&str> {
    let mut v: Vec<&str> = r.iter().map(|(n, _)| n.as_str()).collect();
    v.sort();
    v
}

/// Output says which module a test is in, so two `works` can be told apart.
#[test]
fn a_test_is_named_by_its_module() {
    let dir = package("named");
    let all = ran(&dir, &[]);
    assert_eq!(
        names(&all),
        vec![
            "A.parse",
            "A.works",
            "B.parseInt",
            "B.works",
            "Deep.Er.nested",
            "root"
        ]
    );
}

/// `--exact` on a qualified name runs that test and no other — the one that
/// shares its bare name included.
#[test]
fn exact_picks_one_of_two_tests_with_the_same_name() {
    let dir = package("same-name");
    assert_eq!(
        ran(&dir, &["A.works", "--exact"]),
        vec![("A.works".to_string(), true)]
    );
    assert_eq!(
        ran(&dir, &["B.works", "--exact"]),
        vec![("B.works".to_string(), false)]
    );
}

/// `--exact` is not a prefix match: `parse` is contained in `parseInt`.
#[test]
fn exact_does_not_match_a_longer_name() {
    let dir = package("prefix");
    assert_eq!(names(&ran(&dir, &["A.parse", "--exact"])), vec!["A.parse"]);
    // And a bare name is not a qualified one: nothing is called `works`.
    assert!(ran(&dir, &["works", "--exact"]).is_empty());
    // Root-module tests have no qualifier to give.
    assert_eq!(names(&ran(&dir, &["root", "--exact"])), vec!["root"]);
    assert_eq!(
        names(&ran(&dir, &["Deep.Er.nested", "--exact"])),
        vec!["Deep.Er.nested"]
    );
}

/// Without `--exact` the filter is still a substring, as it always was — of the
/// qualified name now, which contains the bare one.
#[test]
fn a_plain_filter_still_matches_by_substring() {
    let dir = package("substring");
    assert_eq!(names(&ran(&dir, &["works"])), vec!["A.works", "B.works"]);
    assert_eq!(names(&ran(&dir, &["parse"])), vec!["A.parse", "B.parseInt"]);
    assert_eq!(names(&ran(&dir, &["B."])), vec!["B.parseInt", "B.works"]);
}

#[test]
fn exact_needs_something_to_match() {
    let dir = package("needs-filter");
    let out = Command::new(env!("CARGO_BIN_EXE_meadow"))
        .args(["test"])
        .arg(&dir)
        .arg("--exact")
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// The spelling itself, which the runner and the editor both take from here.
#[test]
fn a_root_test_has_no_qualifier_and_a_nested_one_has_every_segment() {
    let site = |module: &[&str], name: &str| TestSite {
        module: module.iter().map(|s| (*s).into()).collect(),
        name: name.into(),
        var: meadow_compiler::hir::VarId(0),
    };
    assert_eq!(site(&[], "works").qualified(), "works");
    assert_eq!(site(&["A"], "works").qualified(), "A.works");
    assert_eq!(
        site(&["Deep", "Er"], "nested").qualified(),
        "Deep.Er.nested"
    );
}

/// `@cfg(test)` is the tested package's, not its dependencies': a dependency
/// whose own tests no longer compile does not stop the package using it from
/// being tested, as cargo builds a dependency without `cfg(test)`.
#[test]
fn a_dependency_is_built_without_its_tests() {
    let root = meadow_scratch::scratch!("meadow-test-deps-{}", std::process::id());
    let dep = root.join("dep");
    let app = root.join("app");
    std::fs::create_dir_all(dep.join("src")).unwrap();
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(
        dep.join("Meadow.toml"),
        "[package]\nname = \"Dep\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dep.join("src").join("Lib.mw"),
        "@pub fun one u = 1\n\n@cfg(test)\nfun broken u = 1 + \"not a number\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("Meadow.toml"),
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nDep = { path = \"../dep\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src").join("Main.mw"),
        "use Std.Test (assertEq)\nuse Dep (one)\n\ndef result = 0\n\n\
         @test fun usesDep u = assertEq (one ()) 1 \"one\"\n",
    )
    .unwrap();
    assert_eq!(ran(&app, &[]), vec![("usesDep".to_string(), true)]);
    // Tested on its own, the dependency's tests are compiled -- and fail to.
    assert!(ran(&dep, &[]).is_empty());
}
