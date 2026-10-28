//! `meadow init` — what it writes, and what it refuses to.
//!
//! The interesting assertion is not that two files appear. It is that what
//! appears is a package the *rest* of the tools accept: the manifest parses
//! back through `package::Manifest`, the source builds, and the name is one
//! that could appear in a `use` path. A template that produced something the
//! compiler then rejected would be worse than no template.

use meadow::{Options, init, package::Manifest, pipeline};
use std::path::{Path, PathBuf};

/// A directory of our own, named after the test so concurrent ones do not
/// collide. Removed first, so a previous failed run cannot make this one pass.
fn scratch(who: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-init-{}-{who}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_at(path: &Path, name: Option<&str>) -> Result<init::Created, String> {
    init::run(&init::Options {
        workspace: false,
        path: path.to_path_buf(),
        name: name.map(|s| s.to_string()),
    })
}

/// The whole point: what is written is a package that builds and runs.
#[test]
fn what_init_writes_is_a_package_that_builds() {
    let dir = scratch("builds");
    let made = init_at(&dir.join("demo"), None).expect("init");
    assert_eq!(made.name, "Demo", "the name comes from the directory");

    // It parses back as a manifest, with the name written down rather than
    // inferred — which is the thing a bare directory of sources would not have.
    let m = Manifest::load(&made.root)
        .expect("readable")
        .expect("a manifest");
    assert_eq!(m.name, "Demo");
    assert_eq!(m.version, "0.1.0");
    assert!(m.deps.is_empty());

    // And the source compiles clean against the real pipeline.
    let out = pipeline::build(&made.root, Options::debug().entry("result"));
    assert!(
        out.diagnostics.is_empty(),
        "the generated package should build clean: {:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    assert!(out.linked.is_some(), "and it should have an entry point");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_directory_is_created_if_it_is_missing() {
    let dir = scratch("mkdir");
    let target = dir.join("deep").join("nested");
    let made = init_at(&target, None).expect("init");
    assert_eq!(made.name, "Nested");
    assert!(target.join("Meadow.toml").is_file());
    assert!(target.join("src").join("Main.mw").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_name_can_be_given_when_the_directory_is_not_one() {
    let dir = scratch("named");
    let made = init_at(&dir.join("my-app"), Some("MyApp")).expect("init");
    assert_eq!(made.name, "MyApp");
    assert_eq!(Manifest::load(&made.root).unwrap().unwrap().name, "MyApp");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest is what says "this is already a package", so overwriting one to
/// put a template there is the one thing this must not do by accident.
#[test]
fn an_existing_package_is_refused_rather_than_overwritten() {
    let dir = scratch("refuse");
    let target = dir.join("twice");
    init_at(&target, None).expect("the first one");

    // Something worth losing, in the file that would be overwritten.
    let manifest = target.join("Meadow.toml");
    let before =
        std::fs::read_to_string(&manifest).unwrap() + "\n[dependencies]\nutil = \"../u\"\n";
    std::fs::write(&manifest, &before).unwrap();

    let err = init_at(&target, None).expect_err("the second one");
    assert!(err.contains("already"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        before,
        "the manifest must be left exactly as it was"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run in a directory that already has sources, `init` supplies the manifest
/// and leaves the code alone.
#[test]
fn existing_sources_are_not_replaced() {
    let dir = scratch("existing");
    let target = dir.join("has-code");
    std::fs::create_dir_all(target.join("src")).unwrap();
    let main = target.join("src").join("Main.mw");
    std::fs::write(&main, "fun main () = println 7\n").unwrap();

    init_at(&target, Some("HasCode")).expect("init");
    assert_eq!(
        std::fs::read_to_string(&main).unwrap(),
        "fun main () = println 7\n",
        "the existing `main` should survive"
    );
    assert!(
        target.join("Meadow.toml").is_file(),
        "but the manifest arrives"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory called `my-pkg` is what someone will type, and `my-pkg` is not
/// a package name -- so it becomes one rather than being refused for a
/// spelling nobody chose.
#[test]
fn a_directory_that_is_not_a_package_name_becomes_one() {
    let dir = scratch("frompath");
    let made = init_at(&dir.join("my-pkg"), None).expect("a name is made from it");
    assert_eq!(made.name, "MyPkg");
    assert!(dir.join("my-pkg").join("Meadow.toml").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that *was* asked for, on the other hand, is taken at its word -- and
/// refused when the language could not refer to it.
#[test]
fn a_name_the_language_could_not_refer_to_is_refused() {
    let dir = scratch("badname");
    let err = init_at(&dir.join("z"), Some("My-Pkg")).expect_err("hyphen");
    assert!(err.contains("My-Pkg"), "{err}");
    assert!(
        err.contains("run the words together"),
        "the message should say what to do: {err}"
    );
    // And one that does not start with a capital is told which name to use.
    let err = init_at(&dir.join("z"), Some("my-pkg")).expect_err("lower case");
    assert!(err.contains("`MyPkg`"), "{err}");
    assert!(
        !dir.join("z").join("Meadow.toml").exists(),
        "nothing should be written when the name is refused"
    );

    for bad in ["2fast", "a b", "My_App", "myApp"] {
        assert!(
            init_at(&dir.join("z"), Some(bad)).is_err(),
            "{bad} should be refused"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A generated package can be depended on *by the name `init` wrote* — which
/// is what the name rule exists to guarantee.
#[test]
fn a_generated_package_can_be_used_as_a_dependency() {
    let dir = scratch("dep");
    let util = dir.join("util");
    let app = dir.join("app");
    init_at(&util, None).expect("util");
    init_at(&app, None).expect("app");

    std::fs::write(
        util.join("src").join("Main.mw"),
        "@pub fun double x = x * 2\n",
    )
    .unwrap();
    std::fs::write(
        app.join("Meadow.toml"),
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nutil = { path = \"../util\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src").join("Main.mw"),
        "use util (double)\n\ndef result = double 21\n",
    )
    .unwrap();

    let out = pipeline::build(&app, Options::debug().entry("result"));
    assert!(
        out.diagnostics.is_empty(),
        "{:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = out.linked.expect("linked");
    assert_eq!(meadow_eval::run(&linked.program).unwrap().to_string(), "42");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Builds write into `target`, so a new package ignores it from the start.
#[test]
fn a_new_package_ignores_its_target_directory() {
    let dir = scratch("gitignore");
    let made = init_at(&dir.join("demo"), None).expect("init");
    let ignore = made.root.join(".gitignore");
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "/target/\n");
    assert!(made.files.contains(&ignore), "and says it wrote it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.gitignore` already there is someone's: the line is added, once, and
/// nothing else changes.
#[test]
fn an_existing_gitignore_gains_the_line_and_keeps_the_rest() {
    let dir = scratch("gitignore-existing");
    let root = dir.join("demo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".gitignore"), "*.log").unwrap();
    init_at(&root, None).expect("init");
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "*.log\n/target/\n"
    );

    let other = dir.join("ignores-already");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(".gitignore"), "target\n").unwrap();
    init_at(&other, Some("IgnoresAlready")).expect("init");
    assert_eq!(
        std::fs::read_to_string(other.join(".gitignore")).unwrap(),
        "target\n",
        "a package that ignores `target` already is left alone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `meadow run` leaves the image it ran in the package's own `target`
/// directory, and that image is the program: run again, it does the same.
#[test]
fn running_a_package_writes_its_image_under_target() {
    let dir = scratch("target");
    let made = init_at(&dir.join("demo"), None).expect("init");
    std::fs::write(
        made.root.join("src").join("Main.mw"),
        "fun main () = println (6 * 7)\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
        .arg("run")
        .arg(&made.root)
        .output()
        .expect("the meadow binary runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "42
"
    );
    let image = meadow::artifacts::native_dir(&made.root, meadow::Profile::Debug)
        .join("silo")
        .join(format!("{}.image", made.name));
    assert!(image.exists(), "the image is written");
    let again = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
        .arg("exec")
        .arg(&image)
        .output()
        .expect("the meadow binary runs");
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        "42
"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
