//! Workspaces: which packages are members, what a command selects, and that
//! members share a `target`, their profiles and what they depend on.

use meadow::package::{Manifest, ProfileConfig};
use meadow::profile::{Profile, Resolved};
use meadow::workspace::{Selection, Workspace};
use meadow::{Options, init, pipeline};
use std::path::{Path, PathBuf};

/// A directory of our own, named after the test so concurrent ones do not
/// collide.
fn scratch(who: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-ws-{}-{who}", std::process::id());
    dir.canonical()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `shop/`: a virtual workspace of `app`, which uses `text` and `util`, and
/// `text`, which uses `util` -- the libraries under `libs/*`, taking their
/// version and dependencies from the root, which also sets a debug flag.
fn shop(who: &str) -> meadow_scratch::Scratch {
    let root = scratch(who).child("shop");
    write(
        &root.join("Meadow.toml"),
        r#"[workspace]
members = [
    "app",   # the program
    "libs/*",
]

[workspace.package]
version = "0.3.0"

[workspace.dependencies]
Util = { path = "libs/util" }
Text = { path = "libs/text" }

[profile.debug]
cfg = "shop"
"#,
    );
    write(
        &root.join("libs/util/Meadow.toml"),
        "[package]\nname = \"Util\"\nversion.workspace = true\n",
    );
    write(
        &root.join("libs/util/src/Lib.mw"),
        "use Std.Test\n\n@pub fun double x = x * 2\n\n@cfg(shop)\n@pub def place = \"shop\"\n\n\
         @test\nfun doubles u = assertEq (double 4) 8 \"doubles\"\n",
    );
    write(
        &root.join("libs/text/Meadow.toml"),
        "[package]\nname = \"Text\"\nversion = { workspace = true }\n\n\
         [dependencies]\nUtil.workspace = true\n",
    );
    write(
        &root.join("libs/text/src/Lib.mw"),
        "use Std.Test\nuse Util (double)\n\n@pub fun label s = \"${s} x${double 1}\"\n\n\
         @test\nfun labels u = assertEq (label \"a\") \"a x2\" \"labels\"\n",
    );
    write(
        &root.join("app/Meadow.toml"),
        "[package]\nname = \"App\"\nversion.workspace = true\n\n[dependencies]\n\
         Util = { workspace = true }\nText = { workspace = true }\n",
    );
    write(
        &root.join("app/src/Main.mw"),
        "use Std.Test\nuse Util (double, place)\nuse Text (label)\n\n\
         def result = (double 21, label \"b\", place)\n\nfun main () = println \"${result:?}\"\n\n@test\nfun works u = assertEq (double 1) 2 \"works\"\n",
    );
    root
}

fn names(ws: &Workspace) -> Vec<&str> {
    ws.members.iter().map(|m| m.name.as_str()).collect()
}

fn run(out: &pipeline::BuildOutput) -> String {
    let linked = out.linked.as_ref().expect("linked");
    meadow_eval::run(&linked.program).unwrap().to_string()
}

fn meadow(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_meadow"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the meadow binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn a_root_manifest_says_what_the_workspace_is() {
    let root = shop("manifest");
    let m = Manifest::load(&root).unwrap().unwrap();
    assert!(!m.is_package, "no [package]: a virtual manifest");
    let ws = m.workspace.expect("a [workspace]");
    assert_eq!(ws.members, ["app", "libs/*"], "an array over several lines");
    assert_eq!(ws.version.as_deref(), Some("0.3.0"));
    assert_eq!(ws.deps.len(), 2);

    // Both spellings of inheriting.
    let text = Manifest::load(&root.join("libs/text")).unwrap().unwrap();
    assert!(text.problems.is_empty(), "{:?}", text.problems);
    assert_eq!(text.version, "0.3.0");
    assert_eq!(text.deps.len(), 1);
    let meadow::package::DepSource::Path(p) = &text.deps[0].source else {
        panic!("a path dependency, got {:?}", text.deps[0].source);
    };
    assert_eq!(std::fs::canonicalize(p).unwrap(), root.join("libs/util"));
    let app = Manifest::load(&root.join("app")).unwrap().unwrap();
    assert_eq!(app.version, "0.3.0");
    assert_eq!(app.deps.len(), 2);
}

#[test]
fn members_come_from_patterns_and_path_dependencies() {
    let root = shop("members");
    // A library the app depends on, under the root but in no pattern.
    write(
        &root.join("vendor/extra/Meadow.toml"),
        "[package]\nname = \"Extra\"\n",
    );
    write(&root.join("vendor/extra/src/Lib.mw"), "@pub def one = 1\n");
    write(
        &root.join("app/Meadow.toml"),
        "[package]\nname = \"App\"\n\n[dependencies]\nUtil = { workspace = true }\n\
         Text = { workspace = true }\nExtra = \"../vendor/extra\"\n",
    );
    // A directory a pattern matches that is not a package.
    std::fs::create_dir_all(root.join("libs/notes")).unwrap();

    let ws = Workspace::load(&root).unwrap();
    assert_eq!(names(&ws), ["App", "Text", "Util", "Extra"]);
    assert!(ws.default_members.is_empty());
}

#[test]
fn a_package_inside_a_workspace_must_be_a_member() {
    let root = shop("stray");
    write(
        &root.join("stray/Meadow.toml"),
        "[package]\nname = \"Stray\"\n",
    );
    write(&root.join("stray/src/Main.mw"), "def result = 1\n");

    let err = Workspace::find(&root.join("stray")).unwrap_err();
    assert!(err.contains("not a member"), "{err}");
    let out = pipeline::build(&root.join("stray"), Options::debug().entry("result"));
    assert!(out.linked.is_none());
    assert!(out.diagnostics[0].msg.contains("not a member"));

    // Unless it is excluded, when it is a package of its own.
    let manifest = std::fs::read_to_string(root.join("Meadow.toml")).unwrap();
    write(
        &root.join("Meadow.toml"),
        &manifest.replace(
            "[workspace.package]",
            "exclude = [\"stray\"]\n\n[workspace.package]",
        ),
    );
    assert!(Workspace::find(&root.join("stray")).unwrap().is_none());
    let out = pipeline::build(&root.join("stray"), Options::debug().entry("result"));
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert_eq!(out.package.unwrap().0, root.join("stray"));
}

#[test]
fn a_workspace_is_found_from_anywhere_inside_a_member() {
    let root = shop("find");
    for inside in [
        root.to_path_buf(),
        root.join("app"),
        root.join("app/src"),
        root.join("app/src/Main.mw"),
        root.join("libs/util"),
    ] {
        let ws = Workspace::find(&inside).unwrap().expect("found");
        assert_eq!(ws.root, *root, "from {}", inside.display());
    }
    assert!(Workspace::find(&root.parent().unwrap()).unwrap().is_none());
}

#[test]
fn what_a_command_selects() {
    let root = shop("select");
    let dirs = |sel: &Selection, at: &Path| -> Vec<PathBuf> { sel.select(at).unwrap().paths };

    // At a virtual root, everything; in a member, that member.
    assert_eq!(dirs(&Selection::default(), &root).len(), 3);
    assert_eq!(
        dirs(&Selection::default(), &root.join("app")),
        [root.join("app")]
    );

    // `-p` from anywhere, `--workspace` less `--exclude`.
    let p = Selection {
        packages: vec!["Text".into()],
        ..Selection::default()
    };
    assert_eq!(dirs(&p, &root.join("app/src")), [root.join("libs/text")]);
    let all_but_app = Selection {
        workspace: true,
        exclude: vec!["App".into()],
        ..Selection::default()
    };
    assert_eq!(
        dirs(&all_but_app, &root.join("app")),
        [root.join("libs/text"), root.join("libs/util")]
    );

    let unknown = Selection {
        packages: vec!["nope".into()],
        ..Selection::default()
    };
    let err = unknown.select(&root).unwrap_err();
    assert!(
        err.contains("no member `nope`") && err.contains("`App`"),
        "{err}"
    );

    // `run` needs one.
    let err = Selection::default()
        .select(&root)
        .unwrap()
        .one("run")
        .unwrap_err();
    assert!(err.contains("-p NAME"), "{err}");

    // `default-members` is what the root means.
    let manifest = std::fs::read_to_string(root.join("Meadow.toml")).unwrap();
    write(
        &root.join("Meadow.toml"),
        &manifest.replace(
            "[workspace.package]",
            "default-members = [\"app\"]\n\n[workspace.package]",
        ),
    );
    assert_eq!(dirs(&Selection::default(), &root), [root.join("app")]);
}

#[test]
fn members_build_together_into_one_target() {
    let root = shop("build");
    let paths = [root.join("app"), root.join("libs/text")];
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let opts = Resolved::resolve(Profile::Debug, &root, ProfileConfig::default())
        .options
        .entry("result");
    let out = pipeline::build_each(&refs, opts);
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert_eq!(out.each.len(), 2);

    // The root's `cfg = "shop"` reached `util`, which only `app` asked for.
    assert_eq!(run(&out.each[0]), r#"(42, "b x2", "shop")"#);
    for (built, name) in out.each.iter().zip(["App", "Text"]) {
        let (dir, pkg) = built.package.clone().unwrap();
        assert_eq!(dir, *root, "{name} builds into the workspace's target");
        assert_eq!(&*pkg, name);
    }
    // `text` is linked with what it depends on and nothing else.
    let text = out.each[1].linked.as_ref().unwrap();
    assert!(text.packages.iter().all(|p| &*p.name != "App"));
}

#[test]
fn every_member_builds_with_the_root_profiles() {
    let root = shop("profiles");
    write(
        &root.join("app/Meadow.toml"),
        "[package]\nname = \"App\"\n\n[dependencies]\nUtil = { workspace = true }\n\
         Text = { workspace = true }\n\n[profile.debug]\ncfg = \"mine\"\n",
    );
    let resolved = Resolved::resolve(Profile::Debug, &root.join("app"), ProfileConfig::default());
    assert!(resolved.options.cfg.has_flag("shop"));
    assert!(
        !resolved.options.cfg.has_flag("mine"),
        "a member's own is ignored"
    );

    let (ok, output) = meadow(&root, &["run", "-p", "App"]);
    assert!(ok, "{output}");
    assert!(output.contains(r#"(42, "b x2", "shop")"#), "{output}");
    assert!(output.contains("warning: the `[profile]` sections of `App` are ignored"));
    assert!(root.join("target/debug/bytecode/App.mbc").is_file());
    assert!(!root.join("app/target").exists());
}

#[test]
fn testing_runs_the_selected_packages_tests() {
    let root = shop("test");
    let (ok, output) = meadow(&root, &["test"]);
    assert!(ok, "{output}");
    assert!(output.contains("running 3 tests"), "{output}");
    for name in ["App.works", "Text.labels", "Util.doubles"] {
        assert!(output.contains(&format!("test {name} ... ok")), "{output}");
    }

    // A member's tests, not its dependencies'.
    let (ok, output) = meadow(&root.join("app"), &["test"]);
    assert!(ok, "{output}");
    assert!(output.contains("running 1 test\n"), "{output}");
    assert!(output.contains("test works ... ok"), "{output}");

    let (ok, output) = meadow(
        &root,
        &["test", "--workspace", "--exclude", "App", ".", "labels"],
    );
    assert!(ok, "{output}");
    assert!(output.contains("running 1 test\n"), "{output}");
    assert!(output.contains("test Text.labels ... ok"), "{output}");

    // And a failure in one member fails the run.
    let lib = root.join("libs/util/src/Lib.mw");
    let source = std::fs::read_to_string(&lib).unwrap();
    write(&lib, &source.replace("(double 4) 8", "(double 4) 9"));
    let (ok, output) = meadow(&root, &["test", "--workspace"]);
    assert!(!ok, "{output}");
    assert!(output.contains("test Util.doubles ... FAILED"), "{output}");
    assert!(output.contains("test App.works ... ok"), "{output}");
}

#[test]
fn a_virtual_root_is_not_a_package() {
    let root = shop("virtual");
    let out = pipeline::build(&root, Options::debug().entry("result"));
    assert!(out.linked.is_none());
    assert!(
        out.diagnostics[0].msg.contains("no package of its own"),
        "{}",
        out.diagnostics[0].msg
    );
}

#[test]
fn inheriting_what_the_workspace_lacks_is_an_error() {
    let root = shop("inherit");
    write(
        &root.join("app/Meadow.toml"),
        "[package]\nname = \"App\"\n\n[dependencies]\nhttp = { workspace = true }\n",
    );
    let out = pipeline::build(&root.join("app"), Options::debug().entry("result"));
    assert!(out.linked.is_none());
    let msg = &out.diagnostics[0].msg;
    assert!(
        msg.contains("`http`") && msg.contains("[workspace.dependencies]"),
        "{msg}"
    );

    let lone = scratch("inherit-lone").child("lone");
    write(
        &lone.join("Meadow.toml"),
        "[package]\nname = \"Lone\"\nversion.workspace = true\n",
    );
    let m = Manifest::load(&lone).unwrap().unwrap();
    assert!(m.problems[0].contains("not in one"), "{:?}", m.problems);
}

#[test]
fn init_makes_a_workspace_and_members_join_it() {
    let dir = scratch("init");
    let root = dir.join("shop");
    let made = init::run(&init::Options {
        path: root.clone(),
        name: None,
        workspace: true,
    })
    .unwrap();
    assert!(made.files.contains(&root.join(".gitignore")));

    let member = |path: PathBuf| {
        init::run(&init::Options {
            path,
            name: None,
            workspace: false,
        })
        .unwrap()
    };
    let app = member(root.join("app"));
    assert!(app.joined.is_some());
    assert!(
        !root.join("app/.gitignore").exists(),
        "the workspace ignores target"
    );
    member(root.join("libs").join("util"));
    assert_eq!(
        std::fs::read_to_string(root.join("Meadow.toml")).unwrap(),
        "[workspace]\nmembers = [\"app\", \"libs/util\"]\n"
    );

    // A pattern that already covers a new package leaves the manifest be.
    write(
        &root.join("Meadow.toml"),
        "[workspace]\nmembers = [\"app\", \"libs/*\"]\n",
    );
    let text = member(root.join("libs").join("text"));
    assert!(text.joined.is_none());
    let ws = Workspace::load(&root).unwrap();
    assert_eq!(names(&ws), ["App", "Text", "Util"]);

    // No workspace inside a workspace, and no second member of a name.
    let err = init::run(&init::Options {
        path: root.join("inner"),
        name: None,
        workspace: true,
    })
    .unwrap_err();
    assert!(err.contains("do not nest"), "{err}");
    let err = init::run(&init::Options {
        path: root.join("other"),
        name: Some("App".into()),
        workspace: false,
    })
    .unwrap_err();
    assert!(err.contains("already has a member called `App`"), "{err}");

    // And what `init` made builds.
    let (ok, output) = meadow(&root, &["build", "--workspace"]);
    assert!(ok, "{output}");
}

/// Two packages beside each other, both depending only on `Std`, used to be
/// numbered from the same variable: linked into one program -- here by a
/// package depending on both -- one's definitions replaced the other's.
#[test]
fn packages_beside_each_other_keep_their_own_definitions() {
    let dir = scratch("siblings");
    for (name, body) in [
        ("a", "@pub fun fa x = x + 1\n@pub def va = fa 10\n"),
        ("b", "@pub fun fb x = x * 100\n@pub def vb = fb 3\n"),
    ] {
        write(
            &dir.join(name).join("Meadow.toml"),
            &format!(
                "[package]\nname = \"{}\"\n",
                meadow::package::as_package_name(name)
            ),
        );
        write(&dir.join(name).join("src/Lib.mw"), body);
    }
    write(
        &dir.join("app/Meadow.toml"),
        "[package]\nname = \"App\"\n\n[dependencies]\na = \"../a\"\nb = \"../b\"\n",
    );
    write(
        &dir.join("app/src/Main.mw"),
        "use a (fa, va)\nuse b (fb, vb)\n\ndef result = (fa 1, va, fb 2, vb)\n",
    );
    let out = pipeline::build(&dir.join("app"), Options::debug().entry("result"));
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert_eq!(run(&out), "(2, 11, 200, 300)");
}
