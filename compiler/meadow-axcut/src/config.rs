//! **How a program of AxCut is run**: the choices every engine that runs one
//! honours -- Silo's interpreter, which runs it as it is, and the LLVM back
//! end, which lowers it. Each is a choice of the program's, not of an
//! engine: what either runtime offered is an option on both
//! (`docs/RUNTIME-MERGE.md`).

/// How the program's memory is managed.
///
/// - **`Count`**: the AxCut paper's reference counting, with reuse and
///   borrowing, as `docs/RUNTIME.md` has it. Silo's own, and the default.
/// - **`Trace`**: the same blocks, found rather than counted. Nothing is
///   shared, erased or reused; the runtime marks what the program's records
///   and its context reach and sweeps the rest, at safe points where a
///   function begins. Nothing moves.
/// - **`Generational`**: traced, with a young generation as Glade's has. A
///   block is made in a nursery, and those still reached when it fills are
///   copied out to where `Trace` keeps its blocks; most die young and cost
///   nothing to collect. Blocks move, so both engines hold every reference
///   where the collector can rewrite it, and read it back after each place
///   one may run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Memory {
    #[default]
    Count,
    Trace,
    Generational,
}

impl Memory {
    /// By the name a command line gives it: `rc`, `tracing`, or
    /// `generational`.
    pub fn named(name: &str) -> Option<Memory> {
        match name {
            "rc" | "count" | "counting" => Some(Memory::Count),
            "tracing" | "trace" => Some(Memory::Trace),
            "generational" | "copying" => Some(Memory::Generational),
            _ => None,
        }
    }

    /// Whether blocks are found by tracing rather than counted.
    pub fn traced(self) -> bool {
        self != Memory::Count
    }
}

/// How deep the program's calls may go.
///
/// - **`Fixed`**: as deep as the stack each thread and each effect handler's
///   segment reserves -- a gigabyte for `main`, a quarter of one otherwise --
///   past which the program stops. Silo's own, and what costs nothing.
/// - **`Growable`**: as deep as memory allows, as Glade's are. A function
///   that begins with too little of its stack left runs on a new piece of
///   stack, and comes back to the old one when it returns: a compare where
///   each function begins, and a switch of stacks only when one is used up.
/// - **`Heap`**: no stack at all for the program's own calls, as Glade's
///   frames are on its heap. Every continuation is an object -- the frames a
///   function makes for its non-tail calls included, which otherwise live on
///   the native stack -- and every call is a tail call: as deep as memory
///   allows, a frame a debugger or a reload can find on the heap, and an
///   allocation for each non-tail call. See [`Config::without_frames`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Stack {
    #[default]
    Fixed,
    Growable,
    Heap,
}

impl Stack {
    /// By the name a command line gives it: `fixed` or `growable`.
    pub fn named(name: &str) -> Option<Stack> {
        match name {
            "fixed" => Some(Stack::Fixed),
            "growable" | "grow" => Some(Stack::Growable),
            "heap" => Some(Stack::Heap),
            _ => None,
        }
    }
}

/// How a block is laid out in memory: which of `meadow_rt::layout`'s
/// descriptions every engine, and the runtime the program runs on, read and
/// write blocks by.
///
/// - **`Silo`**: Silo's, with a count in every header. The default, and the
///   only one counting can use.
/// - **`Glade`**: Glade's, with no count: a block of up to eight fields has
///   two words of header rather than three. Only traced, by `Trace` or
///   `Generational`.
///
/// A runtime library and an interpreter are each built for one of them.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Layout {
    #[default]
    Silo,
    Glade,
}

impl Layout {
    /// By the name a command line gives it: `silo` or `glade`.
    pub fn named(name: &str) -> Option<Layout> {
        match name {
            "silo" => Some(Layout::Silo),
            "glade" => Some(Layout::Glade),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Layout::Silo => "silo",
            Layout::Glade => "glade",
        }
    }

    /// What it is, field by field.
    pub fn description(self) -> meadow_rt::layout::Layout {
        match self {
            Layout::Silo => meadow_rt::layout::SILO,
            Layout::Glade => meadow_rt::layout::GLADE,
        }
    }
}

impl Config {
    /// Whether the choices go together: what is wrong with them, if they do
    /// not. A layout with no count cannot be counted.
    pub fn check(&self) -> Result<(), String> {
        if !self.layout.description().counted && !self.memory.traced() {
            return Err(format!(
                "the `{}` layout has no count, so its memory is traced: \
                 build with `--gc tracing` or `--gc generational`",
                self.layout.name()
            ));
        }
        Ok(())
    }

    /// Whether the program's frames are to be objects: then `program` is to
    /// be made linear with no name a frame, so that every continuation is a
    /// closure the memory pass owns the captures of, and both engines build
    /// it on the heap. See [`Stack::Heap`].
    pub fn without_frames(&self) -> bool {
        self.stack == Stack::Heap
    }
}

/// Every choice, together.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub struct Config {
    pub memory: Memory,
    pub stack: Stack,
    pub layout: Layout,
}
