//! The runtime -- its library, which an executable links against, and its VM,
//! which runs an image -- built for the target this `meadow` is, and put
//! where `src/aot.rs` embeds them, so that a release build of `meadow` runs
//! and links programs with nothing installed beside the binary.
//!
//! Only for a release build of `meadow`, or when `MEADOW_EMBED_RUNTIME=1`
//! asks: each is a release build of the runtime (`silo/`) with link-time
//! optimization, which is minutes, not seconds, and a debug `meadow` in a
//! checkout finds the ones `cargo build --release` leaves there instead.
//! `MEADOW_EMBED_RUNTIME=0` leaves them out of a release build too.
//!
//! Each is built by a `cargo` of its own, in a target directory of its own,
//! from the same sources as this binary's code generator -- so the library's
//! fingerprint (see `silo/build.rs`) is the one the program it links refers
//! to. A failure to build one is a warning, not an error: `meadow` still
//! works, and says where else the runtime can come from when it wants it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    println!("cargo:rerun-if-env-changed=MEADOW_EMBED_RUNTIME");
    println!("cargo:rerun-if-changed=build.rs");

    // The channel this build is for, and what `meadow --version` says: see
    // `src/channel.rs`. A release workflow says `stable` or `nightly`, and a
    // nightly says which day and commit it is; anything else is a build from
    // a checkout.
    for name in ["MEADOW_CHANNEL", "MEADOW_BUILD_DATE", "MEADOW_COMMIT"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let said = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let number = env!("CARGO_PKG_VERSION");
    let (channel, version) = match said("MEADOW_CHANNEL").as_deref() {
        Some("stable") => ("stable", number.to_string()),
        Some("nightly") => {
            let stamp: Vec<String> = [said("MEADOW_BUILD_DATE"), said("MEADOW_COMMIT")]
                .into_iter()
                .flatten()
                .collect();
            let version = if stamp.is_empty() {
                format!("{number}-nightly")
            } else {
                format!("{number}-nightly ({})", stamp.join(" "))
            };
            ("nightly", version)
        }
        _ => ("dev", format!("{number}-dev")),
    };
    println!("cargo:rustc-env=MEADOW_CHANNEL={channel}");
    println!("cargo:rustc-env=MEADOW_VERSION={version}");

    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = here.join("../..");
    let compiler = root.join("compiler");
    // The library, and every input it -- and so its fingerprint -- depends on.
    let library = vec![
        root.join("silo/src"),
        root.join("silo/build.rs"),
        root.join("silo/Cargo.toml"),
        root.join("compiler/fingerprint.rs"),
        compiler.join("meadow-llvm/src"),
        compiler.join("meadow-rt"),
        compiler.join("meadow-intern"),
        compiler.join("meadow-span"),
    ];
    // The VM: the library's sources, its own, and its JIT's.
    let mut vm = library.clone();
    vm.extend([
        root.join("silo/vm/src"),
        root.join("silo/vm/Cargo.toml"),
        compiler.join("meadow-axcut"),
        compiler.join("meadow-exec"),
        compiler.join("meadow-lir"),
    ]);
    for (what, file, inputs) in [
        (Part::Library, "runtime-silo", library),
        (Part::Vm, "vm", vm),
    ] {
        for input in &inputs {
            println!("cargo:rerun-if-changed={}", input.display());
        }
        let bytes = match wanted() {
            true => build(&out, what).unwrap_or_else(|e| {
                println!(
                    "cargo:warning=no runtime {} built into meadow: {e}",
                    what.name()
                );
                Vec::new()
            }),
            false => Vec::new(),
        };
        if what == Part::Vm {
            // What the VM is extracted under: a directory of its own for each.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in &bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            println!("cargo:rustc-env=MEADOW_EMBEDDED_VM={h:016x}");
        }
        // Unchanged contents leave the file alone, so nothing recompiles for it.
        let embedded = out.join(file);
        if std::fs::read(&embedded).ok().as_deref() != Some(&bytes[..]) {
            std::fs::write(&embedded, &bytes).expect("OUT_DIR is writable");
        }
    }

    precompiled_std(&out);
}

/// The standard library, compiled ahead of time -- see `src/stdlib.rs`.
///
/// Two things. **A fingerprint** of everything that decides what compiling
/// `Std` produces: the compiler's crates, `Std`'s sources, and the code here
/// that drives the compile. It is of the *sources*, not of this binary, so a
/// `Std` compiled by one build of `meadow` is recognised by another built from
/// the same sources -- which is what lets a release compile `Std` once and
/// build it into the `meadow` it ships.
///
/// And **the precompiled library itself**, when `MEADOW_PRECOMPILED_STD` names
/// one (`meadow __precompile-std` writes it): embedded, so that a `meadow`
/// built this way never compiles `Std` at all. Without it the embedded bytes
/// are empty, and `Std` is compiled once per toolchain on first use.
fn precompiled_std(out: &Path) {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = here.join("../..");
    // The library's sources and manifest -- not all of `lib/Std`, where a
    // build of it by another compiler (MeadowBoot's) keeps its `target`: that
    // changing is no change to `Std`, and watching it rebuilt `meadow` after
    // every such build.
    let mut inputs: Vec<PathBuf> = vec![
        root.join("lib/Std/src"),
        root.join("lib/Std/Meadow.toml"),
        here.join("src/stdlib.rs"),
    ];
    if let Ok(crates) = std::fs::read_dir(root.join("compiler")) {
        for c in crates.flatten() {
            let p = c.path();
            if p.join("Cargo.toml").is_file() {
                inputs.push(p.join("src"));
                inputs.push(p.join("Cargo.toml"));
            }
        }
    }
    inputs.sort();
    let mut files = Vec::new();
    for i in &inputs {
        println!("cargo:rerun-if-changed={}", i.display());
        collect(i, &mut files);
    }
    files.sort();
    // FNV-1a, over each file's path relative to the root and its text with
    // line endings made one kind, so that a checkout with CRLFs and one
    // without agree.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(env!("CARGO_PKG_VERSION").as_bytes());
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f);
        feed(rel.to_string_lossy().replace('\\', "/").as_bytes());
        if let Ok(text) = std::fs::read(f) {
            let text: Vec<u8> = text.into_iter().filter(|b| *b != b'\r').collect();
            feed(&text);
        }
    }
    println!("cargo:rustc-env=MEADOW_STD_FINGERPRINT={h:016x}");

    println!("cargo:rerun-if-env-changed=MEADOW_PRECOMPILED_STD");
    let bytes = match std::env::var_os("MEADOW_PRECOMPILED_STD") {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", Path::new(&path).display());
            std::fs::read(&path).unwrap_or_else(|e| {
                println!(
                    "cargo:warning=MEADOW_PRECOMPILED_STD names {}, which could not be read: {e}",
                    Path::new(&path).display()
                );
                Vec::new()
            })
        }
        None => Vec::new(),
    };
    let file = out.join("std");
    if std::fs::read(&file).ok().as_deref() != Some(&bytes[..]) {
        std::fs::write(&file, &bytes).expect("OUT_DIR is writable");
    }
}

/// Every file under `path`, or `path` itself if it is one.
fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
    } else if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            collect(&e.path(), out);
        }
    }
}

fn wanted() -> bool {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if arch != "x86_64" && arch != "aarch64" {
        return false;
    }
    match std::env::var("MEADOW_EMBED_RUNTIME").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => std::env::var("PROFILE").as_deref() == Ok("release"),
    }
}

/// What of the runtime is embedded.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    /// The static library an executable links against.
    Library,
    /// The VM, which runs an image.
    Vm,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Part::Library => "library",
            Part::Vm => "VM",
        }
    }
}

/// Build `what` of the runtime, and answer its bytes.
fn build(out: &Path, what: Part) -> Result<Vec<u8>, String> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let krate = here.join("../../silo");
    let target = std::env::var("TARGET").map_err(|e| e.to_string())?;
    let dir = target_dir(out);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--release", "--target", &target]);
    match what {
        Part::Library => cmd.args(["-p", "meadow-silo", "--lib"]),
        Part::Vm => cmd.args(["-p", "meadow-silo-vm", "--bin", "meadow-silo-vm"]),
    };
    cmd.arg("--manifest-path")
        .arg(krate.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&dir);
    // What the outer build was told about itself is not for this one.
    for var in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_MAKEFLAGS",
        "MAKEFLAGS",
        "MFLAGS",
    ] {
        cmd.env_remove(var);
    }
    let built = cmd
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !built.status.success() {
        // What it said is in this script's output, which cargo keeps to
        // itself: the end of it goes in the warning, where it is seen.
        let said = String::from_utf8_lossy(&built.stderr);
        let lines: Vec<&str> = said.lines().collect();
        let tail = lines[lines.len().saturating_sub(15)..].join("\ncargo:warning=  ");
        return Err(format!(
            "`cargo build` of the runtime {} failed ({}):\ncargo:warning=  {tail}",
            what.name(),
            built.status
        ));
    }
    let file = match what {
        Part::Library if target.contains("windows-msvc") => "meadow_silo.lib",
        Part::Library => "libmeadow_silo.a",
        Part::Vm if target.contains("windows") => "meadow-silo-vm.exe",
        Part::Vm => "meadow-silo-vm",
    };
    let path = dir.join(&target).join("release").join(file);
    std::fs::read(&path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

/// A target directory shared by every build of `meadow` in this workspace --
/// debug and release, `build` and `check` -- beside theirs: `OUT_DIR` is
/// `<target>/<profile>/build/<package>/out`. Failing that, `OUT_DIR` itself.
fn target_dir(out: &Path) -> PathBuf {
    let build = out.ancestors().nth(2);
    match (build, out.ancestors().nth(4)) {
        (Some(b), Some(target)) if b.file_name().is_some_and(|n| n == "build") => {
            target.join("meadow-runtime")
        }
        _ => out.join("runtime"),
    }
}
