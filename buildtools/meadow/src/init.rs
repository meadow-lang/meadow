//! `meadow init` — write the smallest thing the rest of the tools will accept
//! as a package.
//!
//! Which is not much: a `Meadow.toml` naming the package, and a `src/Main.mw`
//! with an entry point in it. [`crate::package`] would in fact accept a bare
//! directory of `.mw` files and name the package after the directory, so what
//! this really buys is the *name* — written down, rather than inferred from
//! wherever the directory happens to sit — and a file that already runs.
//!
//! And a `.gitignore` for the `target` directory builds write into (see
//! [`crate::artifacts`]), since nothing in it belongs in version control.
//!
//! Made inside a workspace, the package joins it: it is added to `members`
//! unless a pattern there already covers it, and gets no `.gitignore`, since
//! it builds into the workspace's `target`. `--workspace` makes the workspace
//! itself -- see [`crate::workspace`].
//!
//! `--lib` makes a library instead: `src/Lib.mw`, with a function for others
//! to use and a test of it, and no entry point.

use crate::workspace::Workspace;
use std::path::{Path, PathBuf};

pub struct Options {
    /// Where to put it. `.` is the usual answer.
    pub path: PathBuf,
    /// Overrides the name taken from the directory.
    pub name: Option<String>,
    /// Make a workspace rather than a package -- see [`crate::workspace`].
    pub workspace: bool,
    /// Make a library: `src/Lib.mw` rather than `src/Main.mw`.
    pub lib: bool,
}

/// What was created, for the caller to report.
#[derive(Debug)]
pub struct Created {
    pub name: String,
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    /// The root of the workspace the new package was added to, if it was
    /// made inside one that did not already count it a member.
    pub joined: Option<PathBuf>,
    /// The root of the workspace the new package was made in, whether or
    /// not it had to be added to its members.
    pub within: Option<PathBuf>,
}

pub fn run(opts: &Options) -> Result<Created, String> {
    let root = &opts.path;

    // Refuse rather than merge. A manifest is the one file that says "this is
    // already a package", and overwriting someone's dependency list to put a
    // template there is not a thing to do by accident.
    for existing in ["Meadow.toml", "Meadow.pkg"] {
        let p = root.join(existing);
        if p.exists() {
            return Err(format!(
                "{} already exists — this is already a package",
                p.display()
            ));
        }
    }

    // The workspace this lands in, which the new package joins: making one
    // inside a workspace means that, as `cargo new` does.
    let parent = std::path::absolute(root)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let workspace = match &parent {
        Some(parent) if !opts.workspace => enclosing_workspace(parent)?,
        _ => None,
    };

    std::fs::create_dir_all(root)
        .map_err(|e| format!("could not create {}: {e}", root.display()))?;

    if opts.workspace {
        refuse_nesting(root)?;
        return init_workspace(root);
    }

    let name = match &opts.name {
        Some(n) => n.clone(),
        None => directory_name(root)?,
    };
    check_name(&name)?;
    if let Some(ws) = &workspace
        && let Some(m) = ws.member_named(&name)
    {
        return Err(format!(
            "the workspace at {} already has a member called `{name}`, at {}",
            crate::workspace::shown(&ws.root),
            crate::workspace::shown(&m.dir)
        ));
    }

    let src = root.join("src");
    std::fs::create_dir_all(&src)
        .map_err(|e| format!("could not create {}: {e}", src.display()))?;

    let mut files = Vec::new();
    write_new(&root.join("Meadow.toml"), &manifest(&name), &mut files)?;
    // Left alone if it is already there: someone running this in a directory
    // that has sources wants the manifest, not a new `main`.
    let (file, template) = if opts.lib {
        ("Lib.mw", LIB)
    } else {
        ("Main.mw", MAIN)
    };
    let first = src.join(file);
    if !first.exists() {
        write_new(&first, template, &mut files)?;
    }

    // A member builds into its workspace's `target`, which the workspace's
    // own `.gitignore` covers.
    let joined = match &workspace {
        None => {
            ignore_target(&root.join(".gitignore"), &mut files)?;
            None
        }
        Some(ws) => join(ws, root, &mut files)?,
    };

    Ok(Created {
        name,
        root: root.clone(),
        files,
        joined,
        within: workspace.map(|ws| PathBuf::from(crate::workspace::shown(&ws.root))),
    })
}

/// A virtual workspace at `root`: a manifest with an empty `members`, and
/// the `.gitignore` for the one `target` every member builds into.
fn init_workspace(root: &Path) -> Result<Created, String> {
    let mut files = Vec::new();
    write_new(&root.join("Meadow.toml"), WORKSPACE, &mut files)?;
    ignore_target(&root.join(".gitignore"), &mut files)?;
    Ok(Created {
        name: directory_name(root)?,
        root: root.to_path_buf(),
        files,
        joined: None,
        within: None,
    })
}

/// Workspaces do not nest: one inside another would leave its members
/// belonging to two.
fn refuse_nesting(root: &Path) -> Result<(), String> {
    let full = std::fs::canonicalize(root)
        .map_err(|e| format!("could not read {}: {e}", root.display()))?;
    match full
        .parent()
        .map(enclosing_workspace)
        .transpose()?
        .flatten()
    {
        Some(ws) => Err(format!(
            "{} is inside the workspace at {}, and workspaces do not nest",
            root.display(),
            crate::workspace::shown(&ws.root)
        )),
        None => Ok(()),
    }
}

/// The workspace at or above `dir`, if there is one. A package being made is
/// not a member yet, so this looks for the `[workspace]` itself rather than
/// asking [`Workspace::find`], which would refuse it for not being one.
pub(crate) fn enclosing_workspace(dir: &Path) -> Result<Option<Workspace>, String> {
    let mut dir = Some(dir);
    while let Some(d) = dir {
        if d.join("Meadow.toml").is_file() {
            let manifest = crate::package::Manifest::load(d).ok().flatten();
            if manifest.is_some_and(|m| m.workspace.is_some()) {
                return Workspace::load(d).map(Some);
            }
        }
        dir = d.parent();
    }
    Ok(None)
}

/// Make the package at `root` a member of `ws`, if nothing already makes it
/// one, answering the workspace root when it had to be added.
fn join(ws: &Workspace, root: &Path, files: &mut Vec<PathBuf>) -> Result<Option<PathBuf>, String> {
    let dir = crate::package::canonical(root);
    let reloaded = Workspace::load(&ws.root)?;
    if reloaded.member_at(&dir).is_some() {
        return Ok(None);
    }
    let rel = dir
        .strip_prefix(&ws.root)
        .map_err(|_| format!("{} is not under {}", dir.display(), ws.root.display()))?;
    let rel = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    let manifest = ws.root.join("Meadow.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("could not read {}: {e}", manifest.display()))?;
    std::fs::write(&manifest, crate::workspace::add_member(&text, &rel))
        .map_err(|e| format!("could not write {}: {e}", manifest.display()))?;
    files.push(PathBuf::from(crate::workspace::shown(&manifest)));
    Ok(Some(PathBuf::from(crate::workspace::shown(&ws.root))))
}

const WORKSPACE: &str = "\
[workspace]
members = []
";

fn write_new(path: &Path, contents: &str, files: &mut Vec<PathBuf>) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    files.push(path.to_path_buf());
    Ok(())
}

/// The line that keeps build output out of version control.
const IGNORE_TARGET: &str = "/target/";

/// Make `.gitignore` ignore the `target` directory: a new file saying only
/// that, or -- when there is one already, which is someone else's -- the line
/// added to the end of it, unless it already ignores `target`.
fn ignore_target(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return write_new(path, &format!("{IGNORE_TARGET}\n"), files);
        }
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let ignored = existing
        .lines()
        .map(str::trim)
        .any(|l| matches!(l, "target" | "target/" | "/target" | "/target/"));
    if ignored {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write_new(
        path,
        &format!("{existing}{separator}{IGNORE_TARGET}\n"),
        files,
    )
}

fn manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n")
}

const MAIN: &str = "\
fun main () : () ! Eff =
  println \"Hello, world!\"
";

const LIB: &str = "\
use Std.Test (assertEq)

@pub fun add a b = a + b

@test fun addsTwoNumbers () = assertEq (add 2 3) 5 \"two and three\"
";

/// The name a directory implies: what it is called, as a package name.
///
/// A directory called `my-app` is perfectly ordinary and `my-app` is not a
/// package name, so it becomes `MyApp` rather than an error about a name
/// nobody chose.
fn directory_name(root: &Path) -> Result<String, String> {
    // Canonicalised first, because `.` and `..` are ordinary requests and
    // neither has a file name of its own.
    let full = std::fs::canonicalize(root)
        .map_err(|e| format!("could not read {}: {e}", root.display()))?;
    full.file_name()
        .and_then(|s| s.to_str())
        .map(crate::package::as_package_name)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            format!(
                "cannot tell what to call a package at {} — pass --name",
                root.display()
            )
        })
}

/// Reject a name the language could not refer to.
///
/// A package name is not decoration: it is the first segment of a `use` path
/// (`use Std.Collections.List`), so it is written the way every other segment
/// is — PascalCase, one identifier, no separators. A directory called
/// `my-package` is perfectly ordinary and would produce a package nobody could
/// name, and the moment to say so is now rather than at the first `use`.
pub fn check_name(name: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("`{name}` is not a usable package name: {why}"));

    let Some(first) = name.chars().next() else {
        return bad("it is empty");
    };
    if !first.is_ascii_uppercase() {
        return bad(&format!(
            "a package is named where a module is, so it starts with a capital — `{}`",
            crate::package::as_package_name(name)
        ));
    }
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            let hint = if c == '-' || c == '_' {
                " — run the words together instead"
            } else {
                ""
            };
            return bad(&format!(
                "`{c}` cannot appear in one, and the name is the first segment \
                 of a `use` path{hint}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_name;

    #[test]
    fn a_name_is_written_the_way_a_module_is() {
        for good in ["App", "MyApp", "Std", "App2", "X"] {
            assert!(check_name(good).is_ok(), "{good} should be allowed");
        }
        for bad in [
            "app", "myApp", "my-app", "my_app", "2fast", "", "my app", "my.app", "My_App",
        ] {
            assert!(check_name(bad).is_err(), "{bad} should be refused");
        }
    }

    /// The two mistakes worth a hint: a name that reads like a value, and the
    /// hyphen every other ecosystem spells a multi-word package with.
    #[test]
    fn a_refused_name_says_what_to_write_instead() {
        let e = check_name("my-app").unwrap_err();
        assert!(e.contains("`MyApp`"), "{e}");
        let e = check_name("My-App").unwrap_err();
        assert!(e.contains("run the words together"), "{e}");
    }
}
