//! `meadow cut FILE`: a program of Cut -- the IR a front end hands the back
//! end, `docs/CUT.md` -- lowered to AxCut and run, on Glade's bytecode or as
//! an executable of Silo's. What any front end's programs run through,
//! Meadow's own or not.

use std::path::{Path, PathBuf};

use crate::aot;
use crate::profile::Profile;

/// The program in the file at `path`, lowered to run: read, and refused with
/// why where it does not read or is not lowered yet.
pub fn lowered(path: &Path) -> Result<meadow_seq::Program, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let program = meadow_cut::parse(&text).map_err(|e| format!("{}:{e}", path.display()))?;
    let lowered =
        meadow_cut::lower::executable(&program).map_err(|e| format!("{}: {e}", path.display()))?;
    // `MEADOW_DUMP_AXCUT=1`: every definition's AxCut on stderr, as a build
    // of Meadow source prints it, to set the two beside each other.
    if std::env::var_os("MEADOW_DUMP_AXCUT").is_some() {
        for d in &lowered.defs {
            eprintln!("-- {} (L{})\n{}", d.name, d.label.0, d.block);
        }
    }
    Ok(lowered)
}

/// What `meadow cut --emit` prints of the program at `path`: `axcut`, each
/// definition it is lowered to, under a line naming it; or `llvm`, the LLVM
/// IR a Silo build of it for this machine compiles -- each unit, after the
/// first, under a line naming it. A view of the back end's, for a front
/// end's tools to show: its form is not a contract (`docs/CUT.md`).
pub fn emitted(path: &Path, what: &str) -> Result<String, String> {
    use std::fmt::Write;
    let program = lowered(path)?;
    let mut out = String::new();
    match what {
        "axcut" => {
            for d in &program.defs {
                let _ = writeln!(out, "-- {} (L{})\n{}", d.name, d.label.0, d.block);
            }
        }
        "llvm" => {
            let conv = aot::Target::host()?.call_conv();
            let units = meadow_llvm::compile_split(&program, meadow_llvm::UNIT, conv)
                .map_err(|e| format!("{}: {}", path.display(), e.msg))?;
            for (i, u) in units.iter().enumerate() {
                if i > 0 {
                    let _ = writeln!(out, "\n; ---- unit {} ----\n", u.name);
                }
                out.push_str(&u.text);
            }
        }
        other => {
            return Err(format!(
                "`{other}` is not what `meadow cut --emit` prints: `axcut` or `llvm`"
            ));
        }
    }
    Ok(out)
}

/// The program's bytecode image, kept beside the file, under `target`, for
/// the next run of the same text by the same `meadow`.
///
/// A program that is run again and again -- the one a front end builds of a
/// package's macros is run once for every round of a build that calls them --
/// is read and lowered once: reading megabytes of Cut and lowering it is most
/// of what a short run costs. The image is named for what it was made of, the
/// text and the `meadow` that lowered it, so one made of anything else is not
/// found, and is removed when its successor is written.
fn image(path: &Path) -> Result<meadow_bytecode::Program, String> {
    let fresh = || -> Result<meadow_bytecode::Program, String> {
        let program = lowered(path)?;
        meadow_codegen::compile(&program).map_err(|e| e.msg)
    };
    // What `MEADOW_DUMP_AXCUT` prints is printed by lowering.
    if std::env::var_os("MEADOW_DUMP_AXCUT").is_some() {
        return fresh();
    }
    let Some((dir, stem, key)) = image_key(path) else {
        return fresh();
    };
    let kept = dir.join(format!("{stem}.{key:016x}.image"));
    if let Ok(bytes) = std::fs::read(&kept)
        && let Ok(image) = meadow_bytecode::image::decode(&bytes)
    {
        return Ok(image);
    }
    let image = fresh()?;
    // Kept if it can be: a directory that cannot be written to costs the
    // next run the lowering, and this one nothing.
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let prefix = format!("{stem}.");
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".image") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        // Written whole or not at all: two runs of one program may start at once.
        let partial = dir.join(format!("{stem}.{key:016x}.{}.partial", std::process::id()));
        if std::fs::write(&partial, meadow_bytecode::image::encode(&image)).is_ok() {
            let _ = std::fs::rename(&partial, &kept);
        }
        let _ = std::fs::remove_file(&partial);
    }
    Ok(image)
}

/// Where the image of the program at `path` is kept, the file's name without
/// its extension, and what names the image: the program's text and this
/// `meadow`, by its size and when it was written.
fn image_key(path: &Path) -> Option<(PathBuf, String, u64)> {
    use std::hash::{Hash, Hasher};
    let text = std::fs::read(path).ok()?;
    let exe = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    exe.len().hash(&mut h);
    exe.modified().ok()?.hash(&mut h);
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = path.file_stem()?.to_str()?.to_string();
    Some((root.join("target").join("cut"), stem, h.finish()))
}

/// Lower it for Glade and keep the image, running nothing: what a later
/// run of the same program reads rather than lowering it again.
pub fn kept_for_glade(path: &Path) -> Result<(), String> {
    image(path).map(|_| ())
}

/// Run it on Glade: compiled to bytecode, which is interpreted, and what
/// runs often compiled to machine code as it goes -- as `meadow run` does.
/// Where there is no JIT for the machine, it is interpreted throughout.
pub fn run_on_glade(path: &Path) -> Result<(), String> {
    let image = image(path)?;
    let entry = image.entry.ok_or("the program has no entry")?;
    let jit = meadow_glade::jit::Native::jit(
        &image,
        meadow_glade::jit::Native::threshold_from_env(),
        meadow_compiler::OptLevel::O2,
    )
    .ok();
    meadow_glade::sched::run_native(
        &image,
        jit.as_ref(),
        entry,
        u64::MAX,
        meadow_glade::sched::workers(),
    )
    .result
    .map(|_| ())
    .map_err(|e| e.msg)
}

/// Build it for Glade ahead of time -- its bytecode compiled to machine
/// code, linked with the `meadow_glade` runtime -- beside the file, under
/// `target`, and answer where the executable is.
pub fn build_for_glade(path: &Path, opt: meadow_compiler::OptLevel) -> Result<PathBuf, String> {
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("a file with a name")?;
    let program = lowered(path)?;
    let image = meadow_codegen::compile(&program).map_err(|e| e.msg)?;
    aot::build(
        root,
        Profile::Release,
        opt,
        name,
        &image,
        aot::Target::host()?,
        &Default::default(),
    )
}

/// Build it for Silo -- LLVM's code, linked with the `meadow_silo` runtime
/// -- beside the file, under `target`, and answer where the executable is.
pub fn build_for_silo(path: &Path, opt: meadow_compiler::OptLevel) -> Result<PathBuf, String> {
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("a file with a name")?;
    let target = aot::Target {
        runtime: aot::Runtime::Silo,
        ..aot::Target::host()?
    };
    aot::build_native_of(root, Profile::Release, opt, name, target, None, || {
        lowered(path)
    })
}
