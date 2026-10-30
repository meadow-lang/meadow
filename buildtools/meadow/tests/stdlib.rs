//! The embedded `Std` package must compile clean, and be usable (prelude names in
//! scope, `::` sugar, the `Std.*` containers) from a program built against it.

use meadow::{pipeline, stdlib};
use meadow_eval as eval;

#[test]
fn stdlib_compiles_without_diagnostics() {
    let (_pkgs, diags) = stdlib::std_packages(meadow::Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "Std did not compile clean:\n{}",
        diags
            .iter()
            .map(|d| format!("  {}: {}", d.filename, d.msg))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn run(src: &str) -> String {
    let (program, diags) =
        pipeline::compile_str_with_std("test", src, meadow::Options::debug().entry("result"));
    if !diags.is_empty() {
        return format!(
            "compile errors:\n{}",
            diags
                .iter()
                .map(|d| d.msg.clone())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    match eval::run(&program) {
        Ok(v) => v.to_string(),
        Err(e) => format!("runtime error: {e}"),
    }
}

#[test]
fn prelude_list_functions_are_in_scope() {
    assert_eq!(
        run("def result = sum (map (\\x -> x * x) (range 1 5))\n"),
        "30" // 1 + 4 + 9 + 16
    );
}

#[test]
fn cons_sugar_builds_a_list() {
    assert_eq!(run("def result = 1 :: 2 :: 3 :: Nil\n"), "[1; 2; 3]");
}

#[test]
fn cons_sugar_in_patterns() {
    assert_eq!(
        run(
            "fun swapFirstTwo xs =\n  match xs with\n  | a :: b :: rest -> b :: a :: rest\n  | other -> other\ndef result = swapFirstTwo [1; 2; 3; 4]\n"
        ),
        "[2; 1; 3; 4]"
    );
}

#[test]
fn point_free_sum_with_operator_section() {
    // the motivating example
    assert_eq!(
        run("fun total = foldl (_ + _) 0\ndef result = total (range 1 11)\n"),
        "55"
    );
}

#[test]
fn foldl_and_filter() {
    assert_eq!(
        run("def result = foldl (\\a x -> a + x) 0 (filter (\\n -> n % 2 == 0) (range 0 10))\n"),
        "20" // 0 + 2 + 4 + 6 + 8
    );
}

#[test]
fn option_helpers() {
    assert_eq!(
        run(
            "use Std.Maybe as Maybe\ndef result = Maybe.unwrapOr 0 (Maybe.map (\\x -> x + 1) (Just 41))\n"
        ),
        "42"
    );
}

#[test]
fn std_map_roundtrip() {
    assert_eq!(
        run(
            "use Std.Collections.Map as Map\ndef result =\n  let m = Map.insert 2 \"b\" (Map.insert 1 \"a\" Map.empty) in\n  Map.lookup 2 m\n"
        ),
        "Just(\"b\")"
    );
}

#[test]
fn std_set_dedups() {
    assert_eq!(
        run(
            "use Std.Collections.Set as Set\ndef result = Set.size (Set.fromList [1; 2; 2; 3; 3; 3])\n"
        ),
        "3"
    );
}

/// `metadata` answers a `FileMeta`, which the runtime builds: every machine's
/// has to be the record the compiled code reads, field by field and whole.
#[test]
fn metadata_is_a_file_meta_on_every_machine() {
    let dir = meadow_scratch::scratch!("meadow_meta_test_{}", std::process::id());
    let p = dir.join("five.txt");
    std::fs::write(&p, "12345").unwrap();
    let p = p.to_string_lossy().replace('\\', "/");
    let src = format!(
        "def result =\n\
         \x20 match metadata \"{p}\" with\n\
         \x20 | Ok (m : FileMeta) -> (m.len, m.isFile, m.isDir, m.modified > 0, m == m)\n\
         \x20 | Err e -> (0, False, False, False, False)\n"
    );
    let (program, diags) =
        pipeline::compile_str_with_std("test", &src, meadow::Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let want = "(5, True, False, True, True)";
    assert_eq!(
        eval::run(&program)
            .map(|v| v.to_string())
            .unwrap_or_default(),
        want
    );
    let program = meadow_compiler::core::prune::prune(&program);
    let on_vm = meadow::runtime::run(&program, meadow::Engine::Vm, meadow::Options::debug().opt);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(on_vm, Ok(want.to_string()));
}

#[test]
fn fs_write_read_list_remove() {
    let dir = meadow_scratch::scratch!("meadow_fs_test_{}", std::process::id());
    let p = dir.join("hello.txt").to_string_lossy().replace('\\', "/");
    let d = dir.to_string_lossy().replace('\\', "/");

    let src = format!(
        "def result =\n\
        \x20 let w = writeString (\"{p}\", \"greetings\") in\n\
        \x20 let back = readToString \"{p}\" in\n\
        \x20 let there = exists \"{p}\" in\n\
        \x20 let names = tryReadDir \"{d}\" in\n\
        \x20 let gone = removeFile \"{p}\" in\n\
        \x20 (back, there, names, exists \"{p}\")\n"
    );
    let got = run(&src);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(got, r#"(Ok("greetings"), True, ["hello.txt"], False)"#);
}

#[test]
fn fs_effect_can_be_handled() {
    // a handler intercepts `Fs` so the runtime never touches the disk
    let src = "def result =\n\
      \x20 handle readToString \"/nope\" with {\n\
      \x20   readToString path k -> k (Ok \"mocked\"),\n\
      \x20   return x -> x\n\
      \x20 }\n";
    assert_eq!(run(src), r#"Ok("mocked")"#);
}

#[test]
fn std_tree_sorts() {
    assert_eq!(
        run(
            "use Std.Collections.Tree as Tree\ndef result = Tree.toList (Tree.fromList [5; 3; 8; 1; 4; 7; 9; 2; 6])\n"
        ),
        "[1; 2; 3; 4; 5; 6; 7; 8; 9]"
    );
}

// --- the standard library is compiled once per process ------------------------

#[test]
fn std_packages_is_cached() {
    // `compile_std` takes seconds and is deterministic. It used to run afresh
    // for every REPL line, every language-server keystroke and, worst of all,
    // once per test — which is what made the suite take minutes.
    //
    // Asserted by counting rather than by timing. The tests in this binary run
    // in parallel and most of them warm the cache first, so a "cold" call here
    // is really a second clone; comparing two clones against each other says
    // nothing about the cache, and on a loaded machine it fails outright. The
    // count *is* the property: at most one compile per profile per process.
    let (first, _) = stdlib::std_packages(meadow::Options::debug().entry("result"));
    let (second, _) = stdlib::std_packages(meadow::Options::debug().entry("result"));

    assert_eq!(first.len(), second.len(), "the same packages come back");
    assert_eq!(
        first[0].exports.len(),
        second[0].exports.len(),
        "and with the same exports"
    );
    assert_eq!(
        stdlib::compiles(meadow::Options::debug().entry("result")),
        1,
        "the embedded Std should be compiled exactly once per process"
    );
}

/// `@cfg` flags, the backend and testing change what a package compiles to,
/// but not `Std`, which sees only the platform: one compile serves them all.
#[test]
fn cfg_flags_do_not_recompile_std() {
    let mut flagged = meadow::Options::debug().entry("result");
    flagged.cfg = flagged.cfg.with_flags("fast, feature=gpu");
    flagged.cfg.backend = "cek";
    flagged.cfg.test = true;
    let (plain, _) = stdlib::std_packages(meadow::Options::debug().entry("result"));
    let (other, _) = stdlib::std_packages(flagged);
    assert_eq!(plain.len(), other.len());
    assert_eq!(
        stdlib::compiles(meadow::Options::debug().entry("result")),
        1
    );
}

#[test]
fn the_two_profiles_are_cached_separately() {
    // `--release` turns on the exhaustiveness check, so the two can disagree
    // about diagnostics and must not share a cache entry.
    let (debug, _) = stdlib::std_packages(meadow::Options::debug().entry("result"));
    let (release, _) = stdlib::std_packages(meadow::Options::release().entry("result"));
    assert_eq!(debug.len(), release.len());
    // Two entries, each filled once: sharing one would leave the other at zero.
    assert_eq!(
        stdlib::compiles(meadow::Options::debug().entry("result")),
        1
    );
    assert_eq!(
        stdlib::compiles(meadow::Options::release().entry("result")),
        1
    );
}

/// `Lib.mw` declares every top-level `Std` module, by the name it is compiled
/// under. Nothing else checks a `mod` declaration against a real module, which
/// is how it came to list `Test` twice, miss three modules and spell `Prelude`
/// in lower case.
#[test]
fn lib_declares_exactly_the_top_level_modules() {
    let (_, lib) = meadow::stdlib::MODULES
        .iter()
        .find(|(name, _)| *name == "Lib")
        .expect("Std has a Lib");
    let declared: Vec<&str> = lib
        .lines()
        .filter_map(|l| l.strip_prefix("@pub mod "))
        .map(str::trim)
        .collect();
    let compiled: Vec<&str> = meadow::stdlib::MODULES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| *name != "Lib" && !name.contains('.'))
        .collect();
    assert_eq!(declared, compiled);
}
