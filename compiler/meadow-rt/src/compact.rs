//! What the engines share about compact regions.
//!
//! A region is the runtime's: blocks its collector neither copies nor scans.
//! The CEK and sequent machines reference-count, so for them a `Compact` is a
//! wrapper and nothing moves -- but they must refuse the same values, fail
//! with the same words, and say the same `compactSize`, or `compact` would
//! mean different things on different engines. This is where those live.
//!
//! The rule is the VM's, since it is the VM whose collector depends on it: a
//! region is closed and immutable, so nothing that can be written after it is
//! built may go in -- a `Ref`, a mutable array -- and neither may a function,
//! whose captures differ between engines (the CEK closes over a whole
//! environment, the others over exactly what the body uses).

/// The error for a value that reaches something a region cannot hold.
pub fn uncompactable(what: &str) -> String {
    format!("compact: cannot compact {what}; a compact region holds only immutable data")
}

/// Bytes one word takes, for the engines that can only estimate what
/// `compactSize` would say: they count words the way the runtime lays a
/// region's blocks out, and multiply.
pub const SLOT_BYTES: usize = 8;

/// Words a block of `len` fields takes in a region: its header in the
/// runtime's layout -- with a descriptor for each field, unless all its
/// fields share one (`uniform`: a `BigInt`) -- the fields, and the word that
/// says which region it is in.
pub fn object_slots(uniform: bool, len: usize) -> usize {
    crate::layout::SILO.first_field(len, uniform) + len + 1
}

/// [`object_slots`] for an array of `len` elements, which has room for more
/// than it holds: see [`array_room`].
pub fn array_slots(len: usize) -> usize {
    crate::layout::SILO.first_field(len, true) + array_room(len) + 1
}

/// Field descriptors a header's second word holds in Glade's bytecode VM,
/// before it needs more words: its code generator lays objects out so.
pub const INLINE_DESCS: usize = 8;

/// Field descriptors in each further header word, in Glade's bytecode VM.
pub const DESCS_PER_WORD: usize = 16;

/// The header of an object of `len` fields in Glade's bytecode VM: two
/// words, and -- unless all its fields share one descriptor -- a word of
/// descriptors for every 16 fields past the eighth.
pub fn header_slots(uniform: bool, len: usize) -> usize {
    if uniform || len <= INLINE_DESCS {
        2
    } else {
        2 + (len - INLINE_DESCS).div_ceil(DESCS_PER_WORD)
    }
}

/// The element words an array of `n` is given: `n` for a small one, and for
/// a bigger one the next of 12, 16, 24, 32, 48, ... -- as the runtime's
/// `heap::array_room` gives it.
pub fn array_room(n: usize) -> usize {
    if n <= 8 {
        return n;
    }
    let p = n.next_power_of_two();
    let q = p / 4 * 3;
    if q >= n { q } else { p }
}
