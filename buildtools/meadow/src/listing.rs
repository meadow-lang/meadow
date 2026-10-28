//! **What a build writes**, as `--emit` names it.

/// What a build writes for a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// The image the VM runs, `<name>.image`: what a debug build makes, and
    /// what `meadow exec` takes.
    Image,
    /// A native executable, compiled ahead of time and linked.
    Exe,
    /// The package's sources with every macro call replaced by what it
    /// produced, under `expanded/`: what `cargo expand` shows of a crate.
    Expanded,
}

impl Emit {
    pub const ALL: [Emit; 3] = [Emit::Image, Emit::Exe, Emit::Expanded];

    pub fn parse(s: &str) -> Option<Emit> {
        Self::ALL.into_iter().find(|e| e.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Emit::Image => "image",
            Emit::Exe => "exe",
            Emit::Expanded => "expanded",
        }
    }

    /// Why `s` is not something a build writes, where it once was: the old
    /// runtime's bytecode and machine code, which went with it.
    pub fn retired(s: &str) -> Option<&'static str> {
        match s {
            "bytecode" => Some(
                "`--emit bytecode` wrote the old runtime's bytecode, which is gone; \
                 `meadow dis` prints the code the VM runs",
            ),
            "asm" => Some(
                "`--emit asm` wrote the old runtime's machine code, which is gone; \
                 `MEADOW_KEEP_IR=1` keeps the LLVM IR an executable is compiled from",
            ),
            _ => None,
        }
    }
}
