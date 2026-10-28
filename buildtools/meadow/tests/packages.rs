//! Multi-package builds: `Meadow.toml` manifests, a local path dependency, and
//! `@pub` gating between packages.

use meadow::{
    package::{DepSource, Manifest},
    pipeline,
};
use meadow_eval as eval;
use std::path::{Path, PathBuf};

mod common;

#[test]
fn manifest_parses_cargo_style() {
    let m = Manifest::load(&common::fixture("App"))
        .unwrap()
        .expect("app has a manifest");
    assert_eq!(m.name, "App");
    assert_eq!(m.version, "0.1.0");
    assert_eq!(m.deps.len(), 1);
    assert_eq!(m.deps[0].name, "Util");
    assert_eq!(m.deps[0].source, DepSource::Path(PathBuf::from("../Util")));
}

#[test]
fn builds_app_against_a_path_dependency() {
    let out = pipeline::build(
        &common::fixture("App"),
        meadow::Options::debug().entry("result"),
    );
    assert!(
        out.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = out.linked.expect("linked program");
    // [1..6] -> double -> *3 -> sum  ==  (2+4+6+8+10)*3 == 90
    assert_eq!(eval::run(&linked.program).unwrap().to_string(), "90");
}

#[test]
fn private_names_do_not_cross_package_boundaries() {
    // `util` exports `double` / `scale` (both `@pub`) but not `secret`.
    let out = pipeline::build(
        &common::fixture("Util"),
        meadow::Options::debug().entry("result"),
    );
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    let linked = out.linked.unwrap();
    let mut names: Vec<_> = linked
        .symbols
        .iter()
        .filter(|s| &*s.package == "Util")
        .map(|s| s.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["double", "scale"]);
}

#[test]
fn without_pub_everything_is_still_exported() {
    // A unit that never mentions visibility exports every top-level binding.
    let (cp, _) = pipeline::compile_str("m", "fun a x = x\nfun b y = y\ndef c = 1\n");
    let mut names: Vec<_> = cp.exports.iter().map(|e| e.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

// --- module ordering within a package ----------------------------------------

#[test]
fn modules_are_compiled_in_dependency_order() {
    // `layers` has `Alpha.mw` (needs `Zeta`), `Zeta.mw` and `Main.mw`. Modules are
    // discovered in filename order, so `Alpha` comes first and everything it uses
    // comes later — inference and evaluation both have to sort that out.
    let out = pipeline::build(
        &common::fixture("Layers"),
        meadow::Options::debug().entry("result"),
    );
    assert!(
        out.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let linked = out.linked.expect("linked program");
    // describe 20 == (20 / 2) + 1 == 11; twentyOne == 42 / 2 == 21
    assert_eq!(eval::run(&linked.program).unwrap().to_string(), "32");
}

/// `[profile.<name>]` sections, and the layering that puts them between the
/// profile's built-in meaning and a command-line flag.
#[test]
fn a_manifest_configures_the_build_profiles() {
    use meadow::package::ProfileConfig;
    use meadow::{OptLevel, Profile, Resolved, Strictness};

    let dir = std::env::temp_dir().join("meadow-profile-manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 1\n").unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\n\
         name = \"Tuned\"\n\
         \n\
         [profile.debug]\n\
         opt-level = 2      # fast builds are not what this package wants\n\
         \n\
         [profile.release]\n\
         strictness = \"lenient\"\n",
    )
    .unwrap();

    let m = Manifest::load(&dir).unwrap().expect("a manifest");
    assert_eq!(m.profile("debug").opt, Some(OptLevel::O2));
    assert_eq!(m.profile("debug").strictness, None);
    assert_eq!(m.profile("release").strictness, Some(Strictness::Lenient));
    // A profile the manifest says nothing about keeps its built-in meaning.
    assert_eq!(m.profile("bench"), ProfileConfig::default());

    // Debug is `-O1` by default; this package's manifest makes it `-O2`, and
    // says nothing about strictness, so that stays lenient.
    let debug = Resolved::resolve(Profile::Debug, &dir, ProfileConfig::default());
    assert_eq!(debug.opt(), OptLevel::O2);
    assert_eq!(debug.strictness(), Strictness::Lenient);

    // Release is strict by default; the manifest turns that off.
    let release = Resolved::resolve(Profile::Release, &dir, ProfileConfig::default());
    assert_eq!(release.opt(), OptLevel::O2);
    assert_eq!(release.strictness(), Strictness::Lenient);

    // A flag beats both.
    let flagged = Resolved::resolve(
        Profile::Debug,
        &dir,
        ProfileConfig {
            opt: Some(OptLevel::O0),
            strictness: Some(Strictness::Strict),
            ..ProfileConfig::default()
        },
    );
    assert_eq!(flagged.opt(), OptLevel::O0);
    assert_eq!(flagged.strictness(), Strictness::Strict);

    // A package with no manifest at all is just the profile.
    let bare = Resolved::resolve(
        Profile::Release,
        Path::new("no/such/place"),
        ProfileConfig::default(),
    );
    assert_eq!(bare.options, Profile::Release.options());

    std::fs::remove_dir_all(&dir).ok();
}

/// Debug runs on the JIT and release as an executable, unless `Meadow.toml` or
/// a flag says otherwise -- and a release backend nobody named gives way to the
/// JIT when there is no executable to be had.
#[test]
fn a_manifest_chooses_the_backend() {
    use meadow::package::ProfileConfig;
    use meadow::{Backend, Profile, Resolved};

    let bare = Path::new("no/such/place");
    let debug = Resolved::resolve(Profile::Debug, bare, ProfileConfig::default());
    assert_eq!(debug.backend, Backend::Jit);
    assert_eq!(debug.fallback(), None);
    let release = Resolved::resolve(Profile::Release, bare, ProfileConfig::default());
    assert_eq!(release.backend, Backend::Aot);
    assert_eq!(release.fallback().map(|r| r.backend), Some(Backend::Jit));

    let dir = std::env::temp_dir().join("meadow-backend-manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 1\n").unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\n\
         name = \"Chosen\"\n\
         \n\
         [profile.debug]\n\
         backend = \"vm\"\n\
         \n\
         [profile.release]\n\
         backend = \"aot\"\n",
    )
    .unwrap();

    let m = Manifest::load(&dir).unwrap().expect("a manifest");
    assert_eq!(m.profile("debug").backend, Some(Backend::Vm));
    assert_eq!(m.profile("release").backend, Some(Backend::Aot));

    let debug = Resolved::resolve(Profile::Debug, &dir, ProfileConfig::default());
    assert_eq!(debug.backend, Backend::Vm);
    // Named in the manifest, so it is what the package gets or an error.
    let release = Resolved::resolve(Profile::Release, &dir, ProfileConfig::default());
    assert_eq!(release.backend, Backend::Aot);
    assert_eq!(release.fallback(), None);

    // A flag beats the manifest.
    let flagged = Resolved::resolve(
        Profile::Release,
        &dir,
        ProfileConfig {
            backend: Some(Backend::Jit),
            ..ProfileConfig::default()
        },
    );
    assert_eq!(flagged.backend, Backend::Jit);

    std::fs::remove_dir_all(&dir).ok();
}

/// A profile chooses the runtime's flags; the runtime is one, by either of
/// its names, and how it runs a program is the backend's.
#[test]
fn a_manifest_chooses_the_runtime_and_its_flags() {
    use meadow::aot::Runtime;
    use meadow::package::ProfileConfig;
    use meadow::{Profile, Resolved};

    let dir = std::env::temp_dir().join("meadow-runtime-manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 1\n").unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\n\
         name = \"Stored\"\n\
         \n\
         [profile.debug]\n\
         aot = true\n\
         threads = 2\n\
         \n\
         [profile.release]\n\
         runtime = \"silo\"\n\
         leaks = true\n\
         target = \"aarch64\"\n",
    )
    .unwrap();

    let debug = Resolved::resolve(Profile::Debug, &dir, ProfileConfig::default());
    assert_eq!(debug.runtime, Runtime::Silo);
    assert!(debug.native(), "`aot = true` is compiled ahead of time");
    assert_eq!(debug.threads, Some(2));
    assert_eq!(debug.options.cfg.backend, "aot");

    let release = Resolved::resolve(Profile::Release, &dir, ProfileConfig::default());
    assert_eq!(release.runtime, Runtime::Silo);
    assert!(release.native());
    assert!(release.leaks);
    assert_eq!(
        release.target.map(|t| t.to_string()),
        Some("aarch64".into())
    );
    assert_eq!(release.options.cfg.backend, "aot", "what `@cfg` sees");
    assert_eq!(
        release.fallback().map(|r| r.backend),
        Some(meadow::Backend::Jit),
        "the profile's own backend gives way to the JIT"
    );

    // The command line beats the manifest.
    let flagged = Resolved::resolve(
        Profile::Release,
        &dir,
        ProfileConfig {
            runtime: Runtime::named("glade"),
            ..ProfileConfig::default()
        },
    );
    assert_eq!(flagged.runtime, Runtime::Silo);
    assert_eq!(flagged.options.cfg.backend, "aot");

    std::fs::remove_dir_all(&dir).ok();
}

/// Pruning is on in every profile; a manifest can turn it off for one, and a
/// flag beats the manifest.
/// `profile = true` in a `[profile.<name>]` asks for the run to be sampled,
/// and keeps the debug info a profile's frames are named by -- which an
/// optimizing build would otherwise drop.
#[test]
fn a_profile_section_can_ask_to_be_profiled() {
    use meadow::package::ProfileConfig;
    use meadow::profile::{Profile, Resolved};

    let dir = std::env::temp_dir().join("meadow-profiled-manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 1\n").unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\n\
         name = \"Watched\"\n\
         \n\
         [profile.debug]\n\
         profile = true\n",
    )
    .unwrap();

    let debug = Resolved::resolve(Profile::Debug, &dir, ProfileConfig::default());
    assert!(debug.sample, "the manifest asked to be profiled");
    assert!(
        debug.options.debug_info,
        "which is what names a profile's frames"
    );

    // And a profile that did not ask is left alone.
    let release = Resolved::resolve(Profile::Release, &dir, ProfileConfig::default());
    assert!(!release.sample);
}

#[test]
fn prune_is_on_unless_a_manifest_or_a_flag_says_otherwise() {
    use meadow::package::ProfileConfig;
    use meadow::profile::{Profile, Resolved};

    let dir = std::env::temp_dir().join("meadow-prune-manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/Main.mw"), "def result = 1\n").unwrap();
    std::fs::write(
        dir.join("Meadow.toml"),
        "[package]\n\
         name = \"Pruned\"\n\
         \n\
         [profile.release]\n\
         prune = false\n",
    )
    .unwrap();

    let debug = Resolved::resolve(Profile::Debug, &dir, ProfileConfig::default());
    assert!(debug.prune, "on when nothing says otherwise");
    let release = Resolved::resolve(Profile::Release, &dir, ProfileConfig::default());
    assert!(!release.prune, "off where the manifest says so");
    let flagged = Resolved::resolve(
        Profile::Debug,
        &dir,
        ProfileConfig {
            prune: Some(false),
            ..ProfileConfig::default()
        },
    );
    assert!(!flagged.prune, "off where a flag says so");

    std::fs::remove_dir_all(&dir).ok();
}

// --- constructors a package re-exports -----------------------------------------

/// A library whose root re-exports a sibling module's type and constructors,
/// and an app using it as `main` says.
fn reexporting_pair(what: &str, main: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-reexport-{}-{what}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let lib = dir.join("shapes");
    let app = dir.join("app");
    for (path, text) in [
        (
            lib.join("Meadow.toml"),
            "[package]\nname = \"Shapes\"\nversion = \"0.1.0\"\n",
        ),
        (
            lib.join("src/Lib.mw"),
            "mod Kinds\n@pub use Shapes.Kinds (Shape, area)\n@pub use Shapes.Kinds.Shape.*\n",
        ),
        (
            lib.join("src/Kinds.mw"),
            "@pub data Shape = Square Int | Rect Int Int\n\n\
             @pub fun area s = match s with\n  | Shape.Square n -> n * n\n  | Shape.Rect w h -> w * h\n",
        ),
        (
            app.join("Meadow.toml"),
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nShapes = { path = \"../shapes\" }\n",
        ),
        (app.join("src/Main.mw"), main),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    app
}

fn run_app(app: &Path) -> String {
    let out = pipeline::build(app, meadow::Options::debug().entry("result"));
    if !out.diagnostics.is_empty() {
        return out
            .diagnostics
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n");
    }
    eval::run(&out.linked.expect("linked").program)
        .unwrap()
        .to_string()
}

#[test]
fn a_package_root_can_re_export_constructors_flat() {
    // Named in the `use`, as `use Shapes::Square` would be in Rust.
    let named = reexporting_pair(
        "named",
        "use Shapes (area, Square)\ndef result = area (Square 4)\n",
    );
    assert_eq!(run_app(&named), "16");
    // A bare `use` brings every one.
    let all = reexporting_pair("all", "use Shapes\ndef result = area (Rect 2 3)\n");
    assert_eq!(run_app(&all), "6");
    // Without either, they stay under their type.
    let qualified = reexporting_pair(
        "qualified",
        "use Shapes (area, Shape)\ndef result = area (Shape.Square 3)\n",
    );
    assert_eq!(run_app(&qualified), "9");
    let unnamed = reexporting_pair(
        "unnamed",
        "use Shapes (area)\ndef result = area (Square 4)\n",
    );
    assert!(
        run_app(&unnamed).contains("unknown constructor `Square`"),
        "{}",
        run_app(&unnamed)
    );
}

/// Two libraries, `shapes` and `figures`, each with its own `Shape`, and an
/// app using both.
fn two_shapes(what: &str, main: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meadow-two-shapes-{}-{what}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = dir.join("app");
    let mut files = vec![
        (
            app.join("Meadow.toml"),
            "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nShapes = { path = \"../shapes\" }\nFigures = { path = \"../figures\" }\n"
                .to_string(),
        ),
        (app.join("src/Main.mw"), main.to_string()),
    ];
    for (lib, ctor, area) in [
        ("shapes", "Square", "n * n"),
        ("figures", "Circle", "3 * n * n"),
    ] {
        let name = meadow::package::as_package_name(lib);
        let root = dir.join(lib);
        files.push((
            root.join("Meadow.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        ));
        files.push((
            root.join("src/Lib.mw"),
            format!(
                "@pub data Shape = {ctor} Int\n\n\
                 @pub fun {lib}Area (s : Shape) = match s with\n  | Shape.{ctor} n -> {area}\n\n\
                 @pub def {lib}Unit = Shape.{ctor} 1\n"
            ),
        ));
    }
    for (path, text) in files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    app
}

#[test]
fn two_packages_may_each_declare_a_type_of_one_name() {
    // Each package's `Shape` is its own: both work side by side.
    let both = two_shapes(
        "both",
        "use Shapes (shapesArea, shapesUnit)\nuse Figures (figuresArea, figuresUnit)\n\
         def result = (shapesArea shapesUnit, figuresArea figuresUnit)\n",
    );
    assert_eq!(run_app(&both), "(1, 3)");
    // One package's value is no value of the other's type: this used to
    // type-check, and fail with a non-exhaustive match when run.
    let crossed = two_shapes(
        "crossed",
        "use Shapes (shapesArea)\nuse Figures (figuresUnit)\n\
         def result = shapesArea figuresUnit\n",
    );
    let got = run_app(&crossed);
    // A package is named with its version, since two copies of one package at
    // different versions are two packages.
    assert!(
        got.contains(
            "type mismatch: `Shape` (from `Shapes@0.1.0`) vs `Shape` (from `Figures@0.1.0`)"
        ) || got.contains(
            "type mismatch: `Shape` (from `Figures@0.1.0`) vs `Shape` (from `Shapes@0.1.0`)"
        ),
        "{got}"
    );
    // Named bare, `Shape` is neither's: a dependency's type is in scope only
    // where a `use` brings it.
    let unnamed = two_shapes("unnamed", "fun f (s : Shape) = s\ndef result = 1\n");
    assert!(
        run_app(&unnamed).contains("unknown type `Shape`"),
        "{}",
        run_app(&unnamed)
    );
    let chosen = two_shapes(
        "chosen",
        "use Shapes (Shape, shapesArea)\nfun f (s : Shape) = shapesArea s\n\
         def result = f (Shape.Square 5)\n",
    );
    assert_eq!(run_app(&chosen), "25");
}

#[test]
fn a_type_of_the_package_itself_shadows_one_of_the_same_name_elsewhere() {
    // `Shape` here is the app's, whatever its dependencies declare -- and so
    // is `Parser`, which the standard library's prelude declares too.
    let local = two_shapes(
        "local",
        "data Shape = Dot\ndata Parser = Parser Int\n\
         fun f (s : Shape) = match s with | Shape.Dot -> 7\n\
         def result = (f Shape.Dot, match Parser 2 with | Parser n -> n)\n",
    );
    assert_eq!(run_app(&local), "(7, 2)");
}
