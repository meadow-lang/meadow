//! `meadow cut FILE`: a program of Cut -- the IR a front end hands the back
//! end, `docs/CUT.md` -- lowered to AxCut and run, on the merged runtime's VM
//! or as an executable through LLVM. What any front end's programs run
//! through, Meadow's own or not.

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

/// The program's image, for the merged runtime's VM, kept beside the file,
/// under `target`, for the next run of the same text by the same `meadow`:
/// where it is.
///
/// A program that is run again and again -- the one a front end builds of a
/// package's macros is run once for every round of a build that calls them --
/// is read and lowered once: reading megabytes of Cut and lowering it is most
/// of what a short run costs. The image is named for what it was made of, the
/// text, the `meadow` that lowered it and how it manages memory and its stack,
/// so one made of anything else is not found, and is removed when its
/// successor is written.
fn image(path: &Path) -> Result<PathBuf, String> {
    let fresh = |to: &Path| -> Result<(), String> {
        let program = lowered(path)?;
        aot::write_image_with(to, program, Vec::new(), aot::config())
    };
    let Some((dir, stem, key)) = image_key(path) else {
        let to = std::env::temp_dir().join(format!("meadow-cut-{}.image", std::process::id()));
        fresh(&to)?;
        return Ok(to);
    };
    let kept = dir.join(format!("{stem}.{key:016x}.image"));
    // What `MEADOW_DUMP_AXCUT` prints is printed by lowering. One that is
    // not an image, of this version, is made again.
    let readable = || {
        use std::io::Read;
        let mut head = [0u8; 64];
        let n = std::fs::File::open(&kept)
            .and_then(|mut f| f.read(&mut head))
            .unwrap_or(0);
        meadow_seq::image::is_image(&head[..n])
    };
    if std::env::var_os("MEADOW_DUMP_AXCUT").is_none() && readable() {
        return Ok(kept);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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
    // Written whole or not at all: two runs of one program may start at
    // once, and `write_image_with` renames it into place.
    fresh(&kept)?;
    Ok(kept)
}

/// Where the image of the program at `path` is kept, the file's name without
/// its extension, and what names the image: the program's text, this
/// `meadow`, by its size and when it was written, and how the image is to
/// run.
fn image_key(path: &Path) -> Option<(PathBuf, String, u64)> {
    use std::hash::{Hash, Hasher};
    let text = std::fs::read(path).ok()?;
    let exe = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    exe.len().hash(&mut h);
    exe.modified().ok()?.hash(&mut h);
    [aot::gc() as u8, aot::stack() as u8, aot::layout() as u8].hash(&mut h);
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = path.file_stem()?.to_str()?.to_string();
    Some((root.join("target").join("cut"), stem, h.finish()))
}

/// Lower it for the VM and keep the image, running nothing: what a later run
/// of the same program reads rather than lowering it again.
pub fn kept(path: &Path) -> Result<(), String> {
    image(path).map(|_| ())
}

/// Run it on the merged runtime's VM, given `args`: interpreted, and what
/// runs often compiled to machine code as it goes -- as `meadow run` does in
/// a debug build. Answers how it ended.
pub fn run_on_vm(path: &Path, args: &[String]) -> Result<i32, String> {
    let image = image(path)?;
    let mut vm = std::process::Command::new(aot::interpreter()?);
    if std::env::var_os("MEADOW_JIT").is_none() {
        vm.env("MEADOW_JIT", "1");
    }
    vm.arg(&image)
        .args(args)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .map_err(|e| format!("could not run the interpreter: {e}"))
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
