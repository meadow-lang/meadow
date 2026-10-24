//! **How a program of AxCut is run**: the choices every engine that runs one
//! honours -- Silo's interpreter, which runs it as it is, and the LLVM back
//! end, which lowers it. Each is a choice of the program's, not of an
//! engine: what either runtime offered is an option on both
//! (`docs/RUNTIME-MERGE.md`).

/// How the program's memory is managed.
///
/// - **`Count`**: the AxCut paper's reference counting, with reuse and
///   borrowing, as `docs/SILO.md` has it. Silo's own, and the default.
/// - **`Trace`**: the same blocks, found rather than counted. Nothing is
///   shared, erased or reused; the runtime marks what the program's records
///   and its context reach and sweeps the rest, at safe points where a
///   function begins. Nothing moves.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Memory {
    #[default]
    Count,
    Trace,
}

impl Memory {
    /// By the name a command line gives it: `rc` or `tracing`.
    pub fn named(name: &str) -> Option<Memory> {
        match name {
            "rc" | "count" | "counting" => Some(Memory::Count),
            "tracing" | "trace" => Some(Memory::Trace),
            _ => None,
        }
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

impl Config {
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
}
