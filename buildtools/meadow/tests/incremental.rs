//! Incremental compilation: a package is compiled again exactly when
//! something it was compiled from changed, and what is read back is what
//! compiling would have made.

use meadow::package::ProfileConfig;
use meadow::pipeline::{self, BuildOutput};
use meadow::profile::{Profile, Resolved};
use std::path::{Path, PathBuf};

fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-incr-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn append(path: &Path, text: &str) {
    let old = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, old + text).unwrap();
}

/// A workspace of `App`, which uses `Text`, which uses `Util`, and `Other`,
/// which uses nothing.
fn workspace(who: &str) -> PathBuf {
    let root = scratch(who).join("ws");
    write(
        &root.join("Meadow.toml"),
        "[workspace]\nmembers = [\"App\", \"libs/*\"]\n\n[workspace.dependencies]\n\
         Util = { path = \"libs/Util\" }\nText = { path = \"libs/Text\" }\n",
    );
    for (name, deps, src) in [
        ("libs/Util", "", "@pub fun double x = x * 2\n"),
        (
            "libs/Text",
            "Util = { workspace = true }\n",
            "use Util (double)\n\n@pub fun label s = \"${s} x${double 1}\"\n",
        ),
        ("libs/Other", "", "@pub fun triple x = x * 3\n"),
        (
            "App",
            "Util = { workspace = true }\nText = { workspace = true }\n",
            "use Util (double)\nuse Text (label)\n\ndef result = (double 21, label \"b\")\n\nfun main () = println \"${result:?}\"\n",
        ),
    ] {
        let short = name.rsplit('/').next().unwrap();
        write(
            &root.join(name).join("Meadow.toml"),
            &format!(
                "[package]\nname = \"{}\"\n\n[dependencies]\n{deps}",
                meadow::package::as_package_name(short)
            ),
        );
        let file = if name == "App" { "Main.mw" } else { "Lib.mw" };
        write(&root.join(name).join("src").join(file), src);
    }
    root
}

fn members(root: &Path) -> Vec<PathBuf> {
    ["App", "libs/Other"].iter().map(|m| root.join(m)).collect()
}

fn options(root: &Path) -> meadow::Options {
    Resolved::resolve(Profile::Debug, root, ProfileConfig::default())
        .options
        .entry("result")
}

/// Build `App` and `Other` together, answering what was compiled.
fn build(root: &Path, opts: meadow::Options) -> (Vec<String>, pipeline::ManyOutput) {
    let paths = members(root);
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let out = pipeline::build_each(&refs, opts);
    assert!(
        out.diagnostics.is_empty(),
        "{:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let names = out.compiled.iter().map(|n| n.to_string()).collect();
    (names, out)
}

/// Everything a build says about a program: its symbols and their types,
/// every node's type, the program the back end is given, and what running it
/// gives.
fn everything(out: &BuildOutput) -> String {
    let linked = out.linked.as_ref().unwrap();
    let value = match linked.program.entry {
        Some(_) => meadow_eval::run(&linked.program).unwrap().to_string(),
        None => String::new(),
    };
    format!(
        "{}{}{}{value}",
        linked.dump(),
        linked.annotations(),
        linked.program.pretty()
    )
}

#[test]
fn nothing_changed_compiles_nothing_and_builds_the_same() {
    let root = workspace("same");
    let opts = options(&root);
    let (first, fresh) = build(&root, opts);
    assert_eq!(first, ["Util", "Text", "App", "Other"]);
    let (second, reused) = build(&root, opts);
    assert!(second.is_empty(), "{second:?}");
    for (a, b) in fresh.each.iter().zip(&reused.each) {
        assert_eq!(everything(a), everything(b));
    }
    assert!(
        everything(&reused.each[0]).ends_with(r#"(42, "b x2")"#),
        "and it runs"
    );
}

/// Reusing a package trusts that its dependency, compiled again from the same
/// inputs, mints the same ids and says the same things: a saved `App` refers
/// to `Util`'s variables by number. Every hash map in a process is seeded
/// differently, so two compiles in one process disagree wherever an order
/// comes from one.
#[test]
fn compiling_the_same_package_twice_gives_the_same_package() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/MiniML");
    let main = example.join("src/Main.mw");
    let describe = || {
        // An addition keeps the cache out of it: both are compiled.
        let out = pipeline::build_with(
            &example,
            meadow::Options::debug().entry("result"),
            Some(pipeline::Addition {
                file: &main,
                text: "",
            }),
        );
        assert!(out.diagnostics.is_empty());
        let linked = out.linked.unwrap();
        linked
            .packages
            .iter()
            .map(|p| {
                let mut ctor_fields: Vec<_> = p.ctor_fields.iter().collect();
                ctor_fields.sort_by_key(|(k, _)| k.to_string());
                let mut variants: Vec<_> = p.variants.iter().collect();
                variants.sort_by_key(|(k, _)| k.to_string());
                let exports: Vec<_> = p
                    .exports
                    .iter()
                    .map(|e| (e.name, e.var, format!("{:?}", e.scheme), e.module.clone()))
                    .collect();
                format!(
                    "{} {:?} {:?} {:?} {:?} {:?} {:?} {exports:?} {:?} {:?} {ctor_fields:?} {variants:?}",
                    p.name,
                    p.vars,
                    p.entry,
                    p.flat_ctors,
                    p.prelude_exports,
                    p.tests,
                    p.types.rendered(),
                    p.defs,
                    p.data_decls,
                )
            })
            .collect::<Vec<_>>()
    };
    let (first, second) = (describe(), describe());
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(&second) {
        assert!(
            a == b,
            "{} differs between two compiles",
            &a[..a.find(' ').unwrap()]
        );
    }
}

#[test]
fn a_change_recompiles_the_package_and_what_depends_on_it() {
    let root = workspace("downstream");
    let opts = options(&root);
    build(&root, opts);

    append(&root.join("App/src/Main.mw"), "\n-- a comment\n");
    assert_eq!(build(&root, opts).0, ["App"]);

    append(&root.join("libs/Text/src/Lib.mw"), "\n@pub def more = 1\n");
    assert_eq!(build(&root, opts).0, ["Text", "App"]);

    append(&root.join("libs/Util/src/Lib.mw"), "\n-- Util\n");
    assert_eq!(build(&root, opts).0, ["Util", "Text", "App"]);

    append(&root.join("libs/Other/src/Lib.mw"), "\n-- Other\n");
    assert_eq!(build(&root, opts).0, ["Other"]);
    assert!(build(&root, opts).0.is_empty());
}

/// `Other` is compiled after `App`'s libraries, so its variables start above
/// theirs. A library growing must not move them, or every edit anywhere would
/// be a change to everything built after it.
#[test]
fn a_package_growing_does_not_recompile_the_packages_beside_it() {
    let root = workspace("grow");
    let opts = options(&root);
    build(&root, opts);
    let many: String = (0..200)
        .map(|i| format!("@pub fun f{i} x = x + {i}\n"))
        .collect();
    append(&root.join("libs/Util/src/Lib.mw"), &many);
    let (compiled, out) = build(&root, opts);
    assert_eq!(compiled, ["Util", "Text", "App"]);
    assert!(everything(&out.each[0]).ends_with(r#"(42, "b x2")"#));
}

#[test]
fn different_options_keep_their_own_packages() {
    let root = workspace("options");
    let plain = options(&root);
    let flagged = meadow::Options {
        cfg: plain.cfg.with_flags("fast"),
        ..plain
    };
    assert_eq!(build(&root, plain).0.len(), 4);
    assert_eq!(build(&root, flagged).0.len(), 4, "a flag is a change");
    assert!(build(&root, plain).0.is_empty(), "and each is kept");
    assert!(build(&root, flagged).0.is_empty());
}

#[test]
fn a_package_with_errors_is_compiled_every_time() {
    let root = workspace("errors");
    let opts = options(&root);
    build(&root, opts);
    append(
        &root.join("libs/Text/src/Lib.mw"),
        "\ndef broken = 1 + \"one\"\n",
    );
    let paths = members(&root);
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    for _ in 0..2 {
        let out = pipeline::build_each(&refs, opts);
        assert!(!out.diagnostics.is_empty(), "the error is reported again");
        let compiled: Vec<String> = out.compiled.iter().map(|n| n.to_string()).collect();
        assert_eq!(compiled, ["Text", "App"]);
    }
}

#[test]
fn a_damaged_cache_is_compiled_over() {
    let root = workspace("damaged");
    let opts = options(&root);
    build(&root, opts);
    let dir = root.join("target/debug/incremental");
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with("Util-") {
            // The magic this compiler writes, and then rubbish: the file is
            // damaged, not somebody else's.
            std::fs::write(&path, b"MWPKG\0\0\x02 not a package").unwrap();
            files += 1;
        }
    }
    assert_eq!(files, 1);
    let (compiled, out) = build(&root, opts);
    assert_eq!(compiled, ["Util"], "its dependents' inputs did not change");
    assert!(everything(&out.each[0]).ends_with(r#"(42, "b x2")"#));
}

#[test]
fn renaming_a_package_is_a_change() {
    let root = workspace("rename");
    let opts = options(&root);
    build(&root, opts);
    write(
        &root.join("libs/Other/Meadow.toml"),
        "[package]\nname = \"Another\"\n",
    );
    assert_eq!(build(&root, opts).0, ["Another"]);
}

#[test]
fn the_cache_can_be_turned_off() {
    let root = workspace("off");
    let run = |incremental: &str| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
            .current_dir(&root)
            .args(["run", "-p", "App"])
            .env("MEADOW_INCREMENTAL", incremental)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let dir = root.join("target/debug/incremental");
    assert!(run("0").contains(r#"(42, "b x2")"#));
    assert!(!dir.exists(), "nothing is written");
    assert!(run("1").contains(r#"(42, "b x2")"#));
    // `Util`, `Text` and `App`: `Std` is the toolchain's, not the project's.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
    assert!(run("1").contains(r#"(42, "b x2")"#), "and read back");
}
