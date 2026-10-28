//! **The runtime interface**: what every engine that runs a program -- the
//! Glade VM and its native code, Silo, the CEK evaluator -- shares with the
//! compilers that feed it, and with one another, and nothing of any front
//! end's.
//!
//! Descriptors ([`desc`]), the numeric tower ([`num`]), text and hashing
//! rules ([`text`], [`hash`]), compact regions ([`compact`]), what may cross
//! between threads or sit in a `TVar` ([`thread`], [`stm`]), the console's
//! colours ([`console`]) and the process's arguments ([`args`]), the
//! primitives ([`Prim`]) and their literals ([`Lit`]). The front ends reach
//! all of it through `meadow-core`, which re-exports it; the engines reach it
//! here, so that a language other than Meadow can compile for them.

pub mod args;
pub mod compact;
pub mod console;
pub mod desc;
pub mod ffi;
pub mod hash;
pub mod layout;
pub mod log;
pub mod look;
pub mod net;
pub mod num;
pub mod roles;
pub mod stm;
pub mod terminal;
pub mod text;
pub mod thread;
pub mod world;

use meadow_intern::InternedString;

/// A type declared outside the standard library is known past the resolver by
/// its fully qualified path, written as the source writes a path --
/// `anstyle.Color`, `app.Syntax.Tree.Expr` -- so that two packages, or two
/// modules of one, may each declare a `Color`, and a program may use both.
/// Messages, hovers and printed types show the name without its path, as the
/// source spells it. A constructor keeps its type: see [`ctor_spelling`].
pub fn spelling(name: &str) -> &str {
    match name.rfind('.') {
        Some(at) => &name[at + 1..],
        None => name,
    }
}

/// A constructor's canonical name, `Type.Ctor` after its type's path, as a
/// person writes it: `anstyle.Color.Red` is `Color.Red`, and `Maybe.Just`,
/// whose type the compiler knows, stays as it is.
pub fn ctor_spelling(name: &str) -> &str {
    match name.rfind('.').and_then(|at| name[..at].rfind('.')) {
        Some(at) => &name[at + 1..],
        None => name,
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Lit {
    /// Fixed-width integer (`Int`, i.e. i64).
    Int(i64),
    /// An integer literal whose inferred type is `BigInt` (context coerced it) —
    /// widened to an arbitrary-precision value at runtime.
    BigInt(i64),
    Float(f64),
    /// An integer literal whose inferred type is a sized one -- `UInt8`,
    /// `Int16`, … -- already wrapped to it.
    Word(num::Width, u64),
    /// A float literal whose inferred type is `Float32`.
    Float32(f32),
    /// An integer literal whose type is still a variable -- inside a function
    /// generic over its integer type, `fun succ n = n + 1` -- and that variable.
    /// [`specialize`] makes it the type a copy of the function is for; where
    /// none can be known it is an `Int` at run time, and the primitives let it
    /// take the type of what it meets ([`num`]). The core checker lets it stand
    /// for any type.
    AnyInt(i64, u32),
    /// The same for a float literal: a `Float` where no type is known.
    AnyFloat(f64, u32),
    Str(InternedString),
    /// An interned name, compared by its word rather than its text: the key
    /// an effect operation is dispatched on. No program writes one -- only
    /// lowering to the bytecode machine makes them -- and its type is
    /// [`desc::SYMBOL_TYPE`], not `String`.
    Sym(InternedString),
    Char(char),
    Bool(bool),
    Unit,
}

/// Render a float the way Meadow prints it — always with a fractional part, so it
/// reads as a float and not an int (`1` becomes `1.0`). Shared by the core
/// pretty-printer and the evaluator's `Value` display.
pub fn fmt_float(x: f64) -> String {
    if x.is_finite() && x == x.trunc() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Prim {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Neg,
    /// A value as text the way output shows it: a `String` as itself,
    /// anything else as `show` renders it. What `print` and `println` write.
    Display,
    /// A structural hash, consistent with `==` -- see [`hash`].
    Hash,
    // --- floating point: `Float` and `Float32` ---
    AddF,
    SubF,
    MulF,
    DivF,
    LtF,
    GtF,
    LeF,
    GeF,
    /// `toFloat : n -> Float` for any integer type, and `toFloat64 : f -> Float`
    /// for either float type -- one conversion at run time.
    ToFloat,
    /// `toFloat32 : f -> Float32` for either float type.
    ToFloat32,
    /// `floor : f -> Int` (round toward negative infinity)
    Floor,
    // --- integer conversions ---
    /// `toBigInt : n -> BigInt` for any integer type.
    ToBig,
    /// `toInt : n -> Int` for any integer type, keeping the low 64 bits.
    ToInt,
    /// `toInt8 : n -> Int8` and the rest of the sized types, keeping the low
    /// bits likewise.
    ToWord(num::Width),
    /// `bitWidth : n -> Int` -- 64 for `Int`, 8 for `UInt8`; an error for `BigInt`.
    BitWidth,
    // --- the builtin `Array` (a persistent, `Arc`-shared contiguous buffer) ---
    /// `Array a -> Int`
    ArrayLen,
    /// `Array a -> Int -> a` — runtime error if out of bounds.
    ArrayGet,
    /// `a -> Array a -> Int -> a` — total: the default is returned out of bounds.
    ArrayGetOr,
    /// `Array a -> Int -> a -> Array a` — persistent update.
    ArraySet,
    /// `Array a -> a -> Array a` — append one element.
    ArrayPush,
    /// `Array a -> Array a` — drop the last element (runtime error if empty).
    ArrayPop,
    /// `Array a -> Int -> Int -> Array a` — the `[from, to)` slice, clamped.
    ArraySlice,
    /// `Array a -> Array a -> Array a`
    ArrayConcat,
    // --- bitwise ops on every integer type, at that type's width ---
    /// `n -> Int -> n` — left shift (`x << k`).
    Shl,
    /// `n -> Int -> n` — right shift (`x >> k`): sign-extending for a signed
    /// type, zero-filling for an unsigned one.
    Shr,
    /// `n -> Int -> n` — zero-filling right shift (`x >>> k`) whatever the sign.
    Ushr,
    BitAnd,
    BitOr,
    BitXor,
    /// `n -> n` — bitwise complement.
    BitNot,
    /// `n -> Int` — number of set bits (an error for a negative `BigInt`).
    PopCount,
    // --- bytes ---
    /// `String -> #[UInt8]` — the UTF-8 bytes of a string.
    StringToBytes,
    /// `#[UInt8] -> String` — decode UTF-8, replacing invalid sequences with
    /// U+FFFD. An element that is not a byte is an error at run time.
    BytesToString,
    /// `#[UInt8] -> String` -- lowercase hex, two chars per byte, no separator.
    /// An element that is not a byte is an error at run time.
    BytesToHex,
    /// `show : forall a. a -> String` — the runtime's own rendering of a value,
    /// the same one the REPL prints. Structural, so it needs no per-type work.
    Show,
    /// `Char -> Int` — the Unicode scalar value.
    CharCode,
    /// `Int -> Char` — errors at run time on a value that is not a scalar.
    CharFromCode,
    /// `String -> Array Char` — decodes UTF-8.
    StringToChars,
    /// `Array Char -> String`.
    CharsToString,
    /// `concatStrings : Array String -> String` -- every string of the array,
    /// one after another, in one allocation. What an interpolated string
    /// literal is compiled to.
    ConcatStrings,
    /// `stringByteLength : String -> Int` -- the bytes a string takes, without
    /// copying them out.
    StringByteLength,
    /// `stringByteAt : String -> Int -> UInt8` -- one byte; an error at run
    /// time past either end.
    StringByteAt,
    /// `stringSlice : String -> Int -> Int -> String` -- bytes `from` up to
    /// `to`, both clamped to the string. A cut through a character leaves
    /// U+FFFD where its bytes were.
    StringSlice,
    /// `stringIndexOf : String -> String -> Int -> Int` -- where `needle` next
    /// occurs in `hay` at or after byte `from`, or -1.
    StringIndexOf,
    /// `stringCompare : String -> String -> Int` -- -1, 0 or 1 as `a` sorts
    /// before, with or after `b`, byte by byte: for UTF-8, the order of the
    /// code points.
    StringCompare,
    /// `newRef : a -> Ref a ! { Mut | e }` — allocate a mutable cell.
    NewRef,
    /// `getRef : Ref a -> a ! { Mut | e }`
    GetRef,
    /// `setRef : Ref a -> a -> () ! { Mut | e }`
    SetRef,
    /// `runSt : (forall s. () -> a ! { St s | e }) -> a ! e` -- run a
    /// computation whose mutable state cannot outlive it. Its type is the
    /// checker's business (see `meadow_infer`); lowering turns `runSt f` into
    /// `f ()`, so no engine ever meets this.
    RunSt,
    /// `stNewArray : Int -> a -> StArray s a ! { St s | e }` -- `n` copies of
    /// a value, in a mutable array.
    StNewArray,
    /// `stGetArray : StArray s a -> Int -> a ! { St s | e }`
    StGetArray,
    /// `stSetArray : StArray s a -> Int -> a -> () ! { St s | e }` -- in place.
    StSetArray,
    /// `stArrayLen : StArray s a -> Int` -- fixed at allocation, so pure.
    StArrayLen,
    /// `stFreeze : StArray s a -> Array a ! { St s | e }` -- a copy.
    StFreeze,
    /// `stThaw : Array a -> StArray s a ! { St s | e }` -- a copy.
    StThaw,
    // --- compact regions ---
    /// `compact : a -> Compact a` -- copy a value, and all it reaches, into a
    /// new region the bytecode VM's collector neither copies nor scans. An
    /// error if the value reaches a `Ref`, a mutable array or a function.
    Compact,
    /// `getCompact : Compact a -> a` -- the value inside, without copying.
    GetCompact,
    /// `compactAdd : Compact b -> a -> Compact a` -- copy a value into the same
    /// region, sharing whatever of it is there already.
    CompactAdd,
    /// `compactSize : Compact a -> Int` -- bytes the region holds. Engine
    /// dependent: the VM counts its slots, the others estimate.
    CompactSize,
    // --- green threads ---
    /// `threadSpawn : (() -> a ! { Thread, Console, … }) -> Task a ! { Thread | e }`
    /// -- start a green thread with a heap of its own, running a copy of the
    /// function. What it may do is closed: the runtime's own effects, and
    /// nothing a handler in the spawning thread would have to answer.
    ThreadSpawn,
    /// `threadAwait : Task a -> a ! { Thread | e }` -- wait for a thread to
    /// finish, and take a copy of its result; its failure, if it failed.
    ThreadAwait,
    /// `threadYield : () -> () ! { Thread | e }` -- let other threads run.
    ThreadYield,
    /// `channelNew : () -> Channel a ! { Thread | e }`
    ChannelNew,
    /// `channelSend : Channel a -> a -> () ! { Thread | e }` -- a copy of the
    /// value goes in; the sender never waits.
    ChannelSend,
    /// `channelReceive : Channel a -> a ! { Thread | e }` -- the oldest value
    /// sent, waiting for one if there is none.
    ChannelReceive,
    // --- software transactional memory (see `stm`) ---
    /// `stmNew : a -> TVar a ! { Stm | e }`, and `stmNewIO`, the same outside a
    /// transaction with `Thread` instead.
    StmNew,
    /// `stmRead : TVar a -> Maybe a ! { Stm | e }` -- `None` is a conflict.
    StmRead,
    /// `stmWrite : TVar a -> a -> () ! { Stm | e }`, into the log.
    StmWrite,
    /// `stmBegin : () -> () ! { Thread | e }` -- a new, empty log.
    StmBegin,
    /// `stmCommit : () -> Bool ! { Thread | e }` -- publish the log, or say it
    /// conflicted.
    StmCommit,
    /// `stmWait : () -> () ! { Thread | e }` -- wait until something the log
    /// read is written.
    StmWait,
    /// `stmNest : () -> () ! { Stm | e }` -- a nested log, for `orElse`.
    StmNest,
    /// `stmMerge : () -> () ! { Stm | e }` -- keep the nested log's writes.
    StmMerge,
    /// `stmRollback : () -> () ! { Stm | e }` -- drop them, keeping its reads.
    StmRollback,
    // --- top-level definitions, cached (see `globals`; no source name) ---
    /// `Int -> Bool` -- has this machine cached definition `i`?
    GlobalReady,
    /// `Int -> a` -- the cached value of definition `i`.
    GlobalGet,
    /// `Int -> a -> ()` -- cache the value of definition `i`.
    GlobalSet,
    /// `String -> Maybe #[UInt8]` -- parse a hex string (either case, no
    /// separators, even length) into bytes. `None` on any malformed input.
    BytesFromHex,
    // --- resumptions (no source name; made by lowering handlers) ---
    /// `a -> Once` -- a fresh one-shot flag, what a resumption checks. Its
    /// argument is ignored. Not a `Ref`, so that a resumption sent to another
    /// thread or compacted is refused as the continuation it is.
    Once,
    /// `Once -> Bool` -- `true` the first time, and `false` ever after.
    TakeOnce,
    // --- the frame stack (no source name; made by lowering handlers) ---
    //
    // A function's continuation is a frame on a per-thread stack rather than
    // a heap object, and an effect handler has to capture the frames between
    // a `perform` and itself. The stack is chunked, and these three cut and
    // rejoin it at chunk boundaries, as the bytecode runtime's frame stack did.
    /// `Ref -> ()` -- entering a `handle`: start a chunk, so that whatever the
    /// body pushes can be detached from the handler's own frames in O(1).
    Enter,
    /// `Ref -> Stack` -- at a general clause: detach every chunk above the one
    /// holding the handler's current continuation, as a `Stack` value.
    Detach,
    /// `Stack -> ()` -- resuming: put the detached chunks back on top. Once.
    Reattach,
    // --- tail recursion modulo cons (no source name; made by `trmc`) ---
    /// `d -> Int -> a -> ()` -- fill field `i` of a constructor that was built
    /// with a placeholder there, in place. Only ever given a cell [`trmc`] made
    /// a moment before and nothing else has seen, so no program can tell that
    /// a constructor was written after it was made.
    SetField,
    // --- typed arithmetic (no source name) ---
    //
    // The operators above at a type core knows: both operands are `Int`, or
    // both `Float`, so no engine has to look at what it was given to decide
    // what to do. Nothing in core chooses them yet; the back ends pick typed
    // operations from representations instead. `Int` wraps and dividing it by
    // zero is an error, as `Add`
    // and `Div` on two `Int`s are; `Float` is IEEE.
    IntAdd,
    IntSub,
    IntMul,
    IntDiv,
    IntMod,
    IntEq,
    IntNe,
    IntLt,
    IntLe,
    IntGt,
    IntGe,
    FloatAdd,
    FloatSub,
    FloatMul,
    FloatDiv,
    FloatEq,
    FloatNe,
    FloatLt,
    FloatLe,
    FloatGt,
    FloatGe,
}

impl Prim {
    /// A number for this primitive, stable for as long as the list of
    /// primitives is: what an image written to a file names one by. See
    /// [`Prim::from_code`].
    pub const fn code(self) -> u16 {
        match self {
            Prim::Add => 0,
            Prim::Sub => 1,
            Prim::Mul => 2,
            Prim::Div => 3,
            Prim::Mod => 4,
            Prim::Pow => 5,
            Prim::Eq => 6,
            Prim::Ne => 7,
            Prim::Lt => 8,
            Prim::Gt => 9,
            Prim::Le => 10,
            Prim::Ge => 11,
            Prim::Neg => 12,
            Prim::Display => 13,
            Prim::Hash => 14,
            Prim::AddF => 15,
            Prim::SubF => 16,
            Prim::MulF => 17,
            Prim::DivF => 18,
            Prim::LtF => 19,
            Prim::GtF => 20,
            Prim::LeF => 21,
            Prim::GeF => 22,
            Prim::ToFloat => 23,
            Prim::ToFloat32 => 24,
            Prim::Floor => 25,
            Prim::ToBig => 26,
            Prim::ToInt => 27,
            Prim::ToWord(w) => 28 + w as u16,
            Prim::BitWidth => 35,
            Prim::ArrayLen => 36,
            Prim::ArrayGet => 37,
            Prim::ArrayGetOr => 38,
            Prim::ArraySet => 39,
            Prim::ArrayPush => 40,
            Prim::ArrayPop => 41,
            Prim::ArraySlice => 42,
            Prim::ArrayConcat => 43,
            Prim::Shl => 44,
            Prim::Shr => 45,
            Prim::Ushr => 46,
            Prim::BitAnd => 47,
            Prim::BitOr => 48,
            Prim::BitXor => 49,
            Prim::BitNot => 50,
            Prim::PopCount => 51,
            Prim::StringToBytes => 52,
            Prim::BytesToString => 53,
            Prim::BytesToHex => 54,
            Prim::Show => 55,
            Prim::CharCode => 56,
            Prim::CharFromCode => 57,
            Prim::StringToChars => 58,
            Prim::CharsToString => 59,
            Prim::NewRef => 60,
            Prim::GetRef => 61,
            Prim::SetRef => 62,
            Prim::RunSt => 63,
            Prim::StNewArray => 64,
            Prim::StGetArray => 65,
            Prim::StSetArray => 66,
            Prim::StArrayLen => 67,
            Prim::StFreeze => 68,
            Prim::StThaw => 69,
            Prim::Compact => 70,
            Prim::GetCompact => 71,
            Prim::CompactAdd => 72,
            Prim::CompactSize => 73,
            Prim::ThreadSpawn => 74,
            Prim::ThreadAwait => 75,
            Prim::ThreadYield => 76,
            Prim::ChannelNew => 77,
            Prim::ChannelSend => 78,
            Prim::ChannelReceive => 79,
            Prim::StmNew => 80,
            Prim::StmRead => 81,
            Prim::StmWrite => 82,
            Prim::StmBegin => 83,
            Prim::StmCommit => 84,
            Prim::StmWait => 85,
            Prim::StmNest => 86,
            Prim::StmMerge => 87,
            Prim::StmRollback => 88,
            Prim::GlobalReady => 89,
            Prim::GlobalGet => 90,
            Prim::GlobalSet => 91,
            Prim::BytesFromHex => 92,
            Prim::Once => 93,
            Prim::TakeOnce => 94,
            Prim::IntAdd => 95,
            Prim::IntSub => 96,
            Prim::IntMul => 97,
            Prim::IntDiv => 98,
            Prim::IntMod => 99,
            Prim::IntEq => 100,
            Prim::IntNe => 101,
            Prim::IntLt => 102,
            Prim::IntLe => 103,
            Prim::IntGt => 104,
            Prim::IntGe => 105,
            Prim::FloatAdd => 106,
            Prim::FloatSub => 107,
            Prim::FloatMul => 108,
            Prim::FloatDiv => 109,
            Prim::FloatEq => 110,
            Prim::FloatNe => 111,
            Prim::FloatLt => 112,
            Prim::FloatLe => 113,
            Prim::FloatGt => 114,
            Prim::FloatGe => 115,
            Prim::ConcatStrings => 116,
            Prim::StringByteLength => 117,
            Prim::StringByteAt => 118,
            Prim::StringSlice => 119,
            Prim::StringIndexOf => 120,
            Prim::StringCompare => 121,
            Prim::Enter => 122,
            Prim::Detach => 123,
            Prim::Reattach => 124,
            Prim::SetField => 125,
        }
    }

    /// The primitive [`Prim::code`] numbered `c`, if any.
    pub fn from_code(c: u16) -> Option<Prim> {
        Some(match c {
            0 => Prim::Add,
            1 => Prim::Sub,
            2 => Prim::Mul,
            3 => Prim::Div,
            4 => Prim::Mod,
            5 => Prim::Pow,
            6 => Prim::Eq,
            7 => Prim::Ne,
            8 => Prim::Lt,
            9 => Prim::Gt,
            10 => Prim::Le,
            11 => Prim::Ge,
            12 => Prim::Neg,
            13 => Prim::Display,
            14 => Prim::Hash,
            15 => Prim::AddF,
            16 => Prim::SubF,
            17 => Prim::MulF,
            18 => Prim::DivF,
            19 => Prim::LtF,
            20 => Prim::GtF,
            21 => Prim::LeF,
            22 => Prim::GeF,
            23 => Prim::ToFloat,
            24 => Prim::ToFloat32,
            25 => Prim::Floor,
            26 => Prim::ToBig,
            27 => Prim::ToInt,
            28..=34 => Prim::ToWord(num::Width::ALL[(c - 28) as usize]),
            35 => Prim::BitWidth,
            36 => Prim::ArrayLen,
            37 => Prim::ArrayGet,
            38 => Prim::ArrayGetOr,
            39 => Prim::ArraySet,
            40 => Prim::ArrayPush,
            41 => Prim::ArrayPop,
            42 => Prim::ArraySlice,
            43 => Prim::ArrayConcat,
            44 => Prim::Shl,
            45 => Prim::Shr,
            46 => Prim::Ushr,
            47 => Prim::BitAnd,
            48 => Prim::BitOr,
            49 => Prim::BitXor,
            50 => Prim::BitNot,
            51 => Prim::PopCount,
            52 => Prim::StringToBytes,
            53 => Prim::BytesToString,
            54 => Prim::BytesToHex,
            55 => Prim::Show,
            56 => Prim::CharCode,
            57 => Prim::CharFromCode,
            58 => Prim::StringToChars,
            59 => Prim::CharsToString,
            60 => Prim::NewRef,
            61 => Prim::GetRef,
            62 => Prim::SetRef,
            63 => Prim::RunSt,
            64 => Prim::StNewArray,
            65 => Prim::StGetArray,
            66 => Prim::StSetArray,
            67 => Prim::StArrayLen,
            68 => Prim::StFreeze,
            69 => Prim::StThaw,
            70 => Prim::Compact,
            71 => Prim::GetCompact,
            72 => Prim::CompactAdd,
            73 => Prim::CompactSize,
            74 => Prim::ThreadSpawn,
            75 => Prim::ThreadAwait,
            76 => Prim::ThreadYield,
            77 => Prim::ChannelNew,
            78 => Prim::ChannelSend,
            79 => Prim::ChannelReceive,
            80 => Prim::StmNew,
            81 => Prim::StmRead,
            82 => Prim::StmWrite,
            83 => Prim::StmBegin,
            84 => Prim::StmCommit,
            85 => Prim::StmWait,
            86 => Prim::StmNest,
            87 => Prim::StmMerge,
            88 => Prim::StmRollback,
            89 => Prim::GlobalReady,
            90 => Prim::GlobalGet,
            91 => Prim::GlobalSet,
            92 => Prim::BytesFromHex,
            93 => Prim::Once,
            94 => Prim::TakeOnce,
            95 => Prim::IntAdd,
            96 => Prim::IntSub,
            97 => Prim::IntMul,
            98 => Prim::IntDiv,
            99 => Prim::IntMod,
            100 => Prim::IntEq,
            101 => Prim::IntNe,
            102 => Prim::IntLt,
            103 => Prim::IntLe,
            104 => Prim::IntGt,
            105 => Prim::IntGe,
            106 => Prim::FloatAdd,
            107 => Prim::FloatSub,
            108 => Prim::FloatMul,
            109 => Prim::FloatDiv,
            110 => Prim::FloatEq,
            111 => Prim::FloatNe,
            112 => Prim::FloatLt,
            113 => Prim::FloatLe,
            114 => Prim::FloatGt,
            115 => Prim::FloatGe,
            116 => Prim::ConcatStrings,
            117 => Prim::StringByteLength,
            118 => Prim::StringByteAt,
            119 => Prim::StringSlice,
            120 => Prim::StringIndexOf,
            121 => Prim::StringCompare,
            122 => Prim::Enter,
            123 => Prim::Detach,
            124 => Prim::Reattach,
            125 => Prim::SetField,
            _ => return None,
        })
    }

    /// The untyped primitive a typed one is an instance of -- what an engine
    /// that does not care to be fast can run instead.
    pub const fn untyped(self) -> Prim {
        use Prim::*;
        match self {
            IntAdd => Add,
            IntSub => Sub,
            IntMul => Mul,
            IntDiv => Div,
            IntMod => Mod,
            IntEq | FloatEq => Eq,
            IntNe | FloatNe => Ne,
            IntLt => Lt,
            IntLe => Le,
            IntGt => Gt,
            IntGe => Ge,
            FloatAdd => AddF,
            FloatSub => SubF,
            FloatMul => MulF,
            FloatDiv => DivF,
            FloatLt => LtF,
            FloatLe => LeF,
            FloatGt => GtF,
            FloatGe => GeF,
            other => other,
        }
    }
}

impl Prim {
    /// Does `p` compare two values and answer a `Bool`?
    ///
    /// Two things rest on this list, and both are about a comparison being the
    /// only kind of primitive that can be fused into a branch. It has to answer
    /// a boolean, or the branch would have nothing to test; and — the reason
    /// the runtime cares — it must not **allocate**, because the machine puts a
    /// fused comparison's result in a register the collector does not scan.
    ///
    /// So this is a claim about the runtime's primitives (`glade/src/prims.rs`),
    /// not only about types. A
    /// primitive that allocates does not belong here however boolean it looks.
    pub const fn compares(self) -> bool {
        use Prim::*;
        matches!(
            self,
            Eq | Ne
                | IntEq
                | IntNe
                | IntLt
                | IntLe
                | IntGt
                | IntGe
                | FloatEq
                | FloatNe
                | FloatLt
                | FloatLe
                | FloatGt
                | FloatGe
                | Lt
                | Gt
                | Le
                | Ge
                | LtF
                | GtF
                | LeF
                | GeF
        )
    }

    /// Is this the primitive of one of the language's operators -- what
    /// `Std.Ops` defines `+`, `==`, `<<` and the rest with?
    pub fn is_operator(self) -> bool {
        matches!(
            self,
            Prim::Add
                | Prim::Sub
                | Prim::Mul
                | Prim::Div
                | Prim::Mod
                | Prim::Pow
                | Prim::Eq
                | Prim::Ne
                | Prim::Lt
                | Prim::Gt
                | Prim::Le
                | Prim::Ge
                | Prim::AddF
                | Prim::SubF
                | Prim::MulF
                | Prim::DivF
                | Prim::LtF
                | Prim::GtF
                | Prim::LeF
                | Prim::GeF
                | Prim::Shl
                | Prim::Shr
                | Prim::Ushr
        )
    }

    pub fn from_name(name: &str) -> Option<Prim> {
        Some(match name {
            "_primAdd" => Prim::Add,
            "_primSub" => Prim::Sub,
            "_primMul" => Prim::Mul,
            "_primDiv" => Prim::Div,
            "_primMod" => Prim::Mod,
            "_primPow" => Prim::Pow,
            "_primEq" => Prim::Eq,
            "_primNe" => Prim::Ne,
            "_primLt" => Prim::Lt,
            "_primGt" => Prim::Gt,
            "_primLe" => Prim::Le,
            "_primGe" => Prim::Ge,
            "neg" => Prim::Neg,
            "display" | "_primDisplay" => Prim::Display,
            "hash" => Prim::Hash,
            "_primAddF" => Prim::AddF,
            "_primSubF" => Prim::SubF,
            "_primMulF" => Prim::MulF,
            "_primDivF" => Prim::DivF,
            "_primLtF" => Prim::LtF,
            "_primGtF" => Prim::GtF,
            "_primLeF" => Prim::LeF,
            "_primGeF" => Prim::GeF,
            "toFloat" | "toFloat64" => Prim::ToFloat,
            "toFloat32" => Prim::ToFloat32,
            "floor" => Prim::Floor,
            "toBigInt" => Prim::ToBig,
            "toInt" | "toInt64" => Prim::ToInt,
            "bitWidth" => Prim::BitWidth,
            other => match other.strip_prefix("to").and_then(num::Width::from_type) {
                Some(w) => Prim::ToWord(w),
                None => return Self::from_name_rest(name),
            },
        })
    }

    fn from_name_rest(name: &str) -> Option<Prim> {
        Some(match name {
            "arrayLen" => Prim::ArrayLen,
            "arrayGet" => Prim::ArrayGet,
            "arrayGetOr" => Prim::ArrayGetOr,
            "arraySet" => Prim::ArraySet,
            "arrayPush" => Prim::ArrayPush,
            "arrayPop" => Prim::ArrayPop,
            "arraySlice" => Prim::ArraySlice,
            "arrayConcat" => Prim::ArrayConcat,
            "shl" | "_primShl" => Prim::Shl,
            "shr" | "_primShr" => Prim::Shr,
            "ushr" | "_primUshr" => Prim::Ushr,
            "bitAnd" => Prim::BitAnd,
            "bitOr" => Prim::BitOr,
            "bitXor" => Prim::BitXor,
            "bitNot" => Prim::BitNot,
            "popCount" => Prim::PopCount,
            "stringToBytes" => Prim::StringToBytes,
            "bytesToString" => Prim::BytesToString,
            "bytesToHex" => Prim::BytesToHex,
            "bytesFromHex" => Prim::BytesFromHex,
            "show" => Prim::Show,
            "charCode" => Prim::CharCode,
            "charFromCode" => Prim::CharFromCode,
            "stringToChars" => Prim::StringToChars,
            "charsToString" => Prim::CharsToString,
            "concatStrings" => Prim::ConcatStrings,
            "stringByteLength" => Prim::StringByteLength,
            "stringByteAt" => Prim::StringByteAt,
            "stringSlice" => Prim::StringSlice,
            "stringIndexOf" => Prim::StringIndexOf,
            "stringCompare" => Prim::StringCompare,
            "newRef" => Prim::NewRef,
            "getRef" => Prim::GetRef,
            "setRef" => Prim::SetRef,
            // A cell tied to a `runSt` is an ordinary cell at run time; only
            // its type differs.
            "stNewRef" => Prim::NewRef,
            "stGetRef" => Prim::GetRef,
            "stSetRef" => Prim::SetRef,
            "runSt" => Prim::RunSt,
            "stNewArray" => Prim::StNewArray,
            "stGetArray" => Prim::StGetArray,
            "stSetArray" => Prim::StSetArray,
            "stArrayLen" => Prim::StArrayLen,
            "stFreeze" => Prim::StFreeze,
            "stThaw" => Prim::StThaw,
            "compact" => Prim::Compact,
            "getCompact" => Prim::GetCompact,
            "compactAdd" => Prim::CompactAdd,
            "compactSize" => Prim::CompactSize,
            "threadSpawn" => Prim::ThreadSpawn,
            "threadAwait" => Prim::ThreadAwait,
            "threadYield" => Prim::ThreadYield,
            "channelNew" => Prim::ChannelNew,
            "channelSend" => Prim::ChannelSend,
            "channelReceive" => Prim::ChannelReceive,
            "stmNew" | "stmNewIO" => Prim::StmNew,
            "stmRead" => Prim::StmRead,
            "stmWrite" => Prim::StmWrite,
            "stmBegin" => Prim::StmBegin,
            "stmCommit" => Prim::StmCommit,
            "stmWait" => Prim::StmWait,
            "stmNest" => Prim::StmNest,
            "stmMerge" => Prim::StmMerge,
            "stmRollback" => Prim::StmRollback,
            _ => return None,
        })
    }

    pub fn arity(self) -> usize {
        match self {
            Prim::Neg
            | Prim::Display
            | Prim::Hash
            | Prim::ToFloat
            | Prim::ToFloat32
            | Prim::Floor
            | Prim::ToBig
            | Prim::ToInt
            | Prim::ToWord(_)
            | Prim::BitWidth
            | Prim::ArrayLen
            | Prim::ArrayPop
            | Prim::BitNot
            | Prim::PopCount
            | Prim::StringToBytes
            | Prim::BytesToString
            | Prim::BytesToHex
            | Prim::BytesFromHex
            | Prim::Show
            | Prim::CharCode
            | Prim::CharFromCode
            | Prim::StringToChars
            | Prim::CharsToString
            | Prim::ConcatStrings
            | Prim::StringByteLength
            | Prim::NewRef
            | Prim::GetRef
            | Prim::RunSt
            | Prim::StArrayLen
            | Prim::StFreeze
            | Prim::StThaw
            | Prim::Compact
            | Prim::GetCompact
            | Prim::CompactSize
            | Prim::ThreadSpawn
            | Prim::ThreadAwait
            | Prim::ThreadYield
            | Prim::ChannelNew
            | Prim::ChannelReceive
            | Prim::GlobalReady
            | Prim::GlobalGet
            | Prim::StmNew
            | Prim::StmRead
            | Prim::StmBegin
            | Prim::StmCommit
            | Prim::StmWait
            | Prim::StmNest
            | Prim::StmMerge
            | Prim::StmRollback
            | Prim::Once
            | Prim::TakeOnce
            | Prim::Enter
            | Prim::Detach
            | Prim::Reattach => 1,
            Prim::ArraySet
            | Prim::ArraySlice
            | Prim::ArrayGetOr
            | Prim::StSetArray
            | Prim::StringSlice
            | Prim::StringIndexOf
            | Prim::SetField => 3,
            _ => 2,
        }
    }
}

/// Where a term was written: which source, and where in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Loc {
    /// A `meadow_source::Source`'s id -- see `meadow_source::SourceId`.
    pub source: u32,
    pub span: meadow_span::Span,
}

/// How hard the compiler works to make the program fast.
///
/// **`O0` is not "no optimisation".** The compiler is never gratuitously bad:
/// it does not build a closure for a subexpression that cannot transfer control,
/// a saturated call to a known function is a jump rather than three allocations,
/// and a primitive names its operand registers instead of gathering them. Those
/// cost nothing — no code size, no compile time worth measuring, no fidelity —
/// so turning them off would only make a debug build ten times slower for no
/// benefit to anyone. Nothing on this ladder controls them.
///
/// What the ladder controls is the work that *trades* something.
///
/// | | adds | costs |
/// |---|---|---|
/// | `O0` | nothing | — |
/// | `O1` | native code keeps the machine's books only where they are observed, with short encodings | — |
/// | `O2` | `match` compiled to a decision tree; generic code specialized; native loops kept inside one function, hot registers in machine registers | code size, compile time |
///
/// The native rows were the bytecode runtime's code generator's, which its JIT
/// and its ahead-of-time executables shared. `O1` is the default, and it is
/// where a pass lands that is worth doing on every keystroke. `O0` exists so
/// that a suspected miscompilation can be bisected against a compiler doing
/// the least it is allowed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum OptLevel {
    /// Nothing optional at all.
    ///
    /// The bytecode is the same as at [`OptLevel::O1`]: everything below `O2`
    /// there is unconditional, because it is not a trade. A known call becoming
    /// a jump, a literal folding into the instruction that uses it — those make
    /// debug builds smaller and faster and cost nothing to read, so there is
    /// nothing to turn off. Native code differs: at `O0` it is the literal
    /// translation of each instruction.
    O0,
    /// The default: everything cheap enough to want while editing.
    #[default]
    O1,
    /// Everything, including passes that trade code size for speed.
    O2,
}
impl OptLevel {
    /// Parse `0`, `1`, `2` — or `O0`, `o1`, as a `-O` flag is usually written.
    pub fn parse(s: &str) -> Option<OptLevel> {
        match s.trim().trim_start_matches(['O', 'o']) {
            "0" => Some(OptLevel::O0),
            "1" => Some(OptLevel::O1),
            "2" => Some(OptLevel::O2),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            OptLevel::O0 => "O0",
            OptLevel::O1 => "O1",
            OptLevel::O2 => "O2",
        }
    }

    /// Compile `match` to a decision tree rather than a chain of failure
    /// continuations.
    ///
    /// The chain retests what an earlier arm already tested, so a wide `match`
    /// does more work than it needs to; a tree tests each scrutinee once. It is
    /// gated because it is the trade the chain was avoiding — a tree duplicates
    /// the arms it shares, so the code grows.
    pub const fn case_trees(self) -> bool {
        matches!(self, OptLevel::O2)
    }

    /// Copy generic code per representation it is used at, rather than pass
    /// descriptors at run time -- see `meadow_core::specialize::release`. The
    /// program grows; generic code runs as fast as the code it was written
    /// for. Gated because a debug build is the one being rebuilt constantly.
    pub const fn specializes(self) -> bool {
        matches!(self, OptLevel::O2)
    }

    /// Copy a small definition to the places that call it -- see
    /// `meadow_core::inline`. Gated because a debug build is what the debugger
    /// steps through, and a call that was inlined is not a call to step into,
    /// a frame to see, or a line a breakpoint can stop at.
    pub const fn inlines(self) -> bool {
        matches!(self, OptLevel::O2)
    }
}
