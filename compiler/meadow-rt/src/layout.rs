//! **How a value is laid out in memory**: the object layout, as a
//! description that everything which reads or writes a block takes from one
//! place -- the runtime's heap, the interpreter, and the code the LLVM back
//! end writes (`docs/RUNTIME-MERGE.md`, "One object model").
//!
//! A layout says:
//! - what a word that is not a block is: nothing, or a small value with no
//!   block (a constructor with no fields, a closure with no captures), and
//!   how one is told from an address;
//! - what a block's header holds -- its count, its length, its kind, its
//!   `meta` (a constructor's tag, a closure's methods, a string's length),
//!   the bits a collector keeps -- and where, as a [`Field`] of a word;
//! - how its fields' descriptors are packed, and where its fields begin;
//! - how a block in a compact region is told from the rest;
//! - which number each kind is.
//!
//! [`SILO`] is the only layout there is yet: Silo's, counted by reference,
//! with machine pointers and nothing that moves. A second, Glade's -- slot
//! numbers whose range says the space, a header with no count, and a
//! collector that moves blocks -- is the next instance; more of a layout may
//! be made a choice after that.

/// A field of a header word: which word, where in it, and how wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub word: usize,
    pub shift: u32,
    pub bits: u32,
}

impl Field {
    /// Its value in `w`, the header word it is in.
    #[inline(always)]
    pub const fn get(self, w: u64) -> u64 {
        (w >> self.shift) & self.mask()
    }

    /// `w` with this field set to `v`.
    #[inline(always)]
    pub const fn set(self, w: u64, v: u64) -> u64 {
        (w & !(self.mask() << self.shift)) | ((v & self.mask()) << self.shift)
    }

    /// `v` where this field is in its word, the rest of it zero.
    #[inline(always)]
    pub const fn put(self, v: u64) -> u64 {
        (v & self.mask()) << self.shift
    }

    /// Its bits, at the bottom of a word.
    #[inline(always)]
    pub const fn mask(self) -> u64 {
        if self.bits >= 64 {
            u64::MAX
        } else {
            (1 << self.bits) - 1
        }
    }

    /// Its bits, where they are in their word.
    #[inline(always)]
    pub const fn bits_in_place(self) -> u64 {
        self.mask() << self.shift
    }
}

/// The number of each kind of block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kinds {
    pub data: u64,
    pub closure: u64,
    pub string: u64,
    pub array: u64,
    pub bigint: u64,
    pub cell: u64,
    pub record: u64,
    pub mut_array: u64,
    /// A resumption's one-shot flag.
    pub once: u64,
    pub compact: u64,
    pub channel: u64,
    pub task: u64,
    pub tvar: u64,
    /// A suspended stack segment.
    pub stack: u64,
    /// What a compact's values live in.
    pub region: u64,
}

/// An object layout: see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: &'static str,
    /// The word that is no value at all: no block, and nothing in it.
    pub null: u64,
    /// A small value with no block is `(payload << imm_shift) | imm_tag`,
    /// and told from an address by `word & imm_mask == imm_tag`.
    pub imm_shift: u32,
    pub imm_tag: u64,
    pub imm_mask: u64,
    /// How many words a header has before its fields' descriptors.
    pub header_words: usize,
    /// The extra references a block has, beyond the one: zero for a block
    /// with one owner.
    pub count: Field,
    pub len: Field,
    pub kind: Field,
    /// The one descriptor every field of a uniform block has.
    pub elem_desc: Field,
    pub uniform: Field,
    pub meta: Field,
    /// What the cycle collector keeps: a colour, and whether a block is a
    /// kind a cycle can contain (a `Ref`, a mutable array).
    pub colour: Field,
    pub mutable: Field,
    /// Fields' descriptors: `desc_bits` each, `descs_per_word` to a word,
    /// after the header -- absent for a uniform block.
    pub desc_bits: u32,
    pub descs_per_word: usize,
    /// A count at or above this is a block inside a compact region, which
    /// is given [`Layout::sticky`] on top of the references it has.
    pub region_floor: u64,
    pub sticky: u64,
    pub kinds: Kinds,
}

/// Silo's layout (`silo/src/heap.rs`):
///
/// ```text
/// word 0   extra references: u32 (0: the only one) | length: u32
/// word 1   kind: u8 | element descriptor: u4 | uniform: u1 | colour: u2 | … | meta: u32
/// words    field descriptors, four bits each, sixteen to a word
///          (absent for a uniform block, whose one descriptor is in word 1)
/// fields
/// ```
///
/// `0` is nothing, and an odd word a value with no block.
pub const SILO: Layout = Layout {
    name: "silo",
    null: 0,
    imm_shift: 1,
    imm_tag: 1,
    imm_mask: 1,
    header_words: 2,
    count: Field {
        word: 0,
        shift: 0,
        bits: 32,
    },
    len: Field {
        word: 0,
        shift: 32,
        bits: 32,
    },
    kind: Field {
        word: 1,
        shift: 0,
        bits: 8,
    },
    elem_desc: Field {
        word: 1,
        shift: 8,
        bits: 4,
    },
    uniform: Field {
        word: 1,
        shift: 12,
        bits: 1,
    },
    colour: Field {
        word: 1,
        shift: 13,
        bits: 2,
    },
    mutable: Field {
        word: 1,
        shift: 18,
        bits: 1,
    },
    meta: Field {
        word: 1,
        shift: 32,
        bits: 32,
    },
    desc_bits: 4,
    descs_per_word: 16,
    region_floor: 1 << 29,
    sticky: 1 << 30,
    kinds: Kinds {
        data: 1,
        closure: 2,
        string: 3,
        array: 4,
        bigint: 5,
        cell: 6,
        record: 7,
        mut_array: 8,
        once: 9,
        compact: 10,
        channel: 11,
        task: 12,
        tvar: 13,
        stack: 14,
        region: 15,
    },
};

impl Layout {
    /// Whether `w` is the address of a block.
    #[inline(always)]
    pub const fn is_block(&self, w: u64) -> bool {
        w != self.null && w & self.imm_mask != self.imm_tag
    }

    /// The small value with no block that carries `payload`.
    #[inline(always)]
    pub const fn immediate(&self, payload: u64) -> u64 {
        (payload << self.imm_shift) | self.imm_tag
    }

    /// What a small value with no block carries.
    #[inline(always)]
    pub const fn payload(&self, w: u64) -> u64 {
        w >> self.imm_shift
    }

    /// How many words the descriptors of `len` fields take.
    #[inline(always)]
    pub const fn desc_words(&self, len: usize) -> usize {
        len.div_ceil(self.descs_per_word)
    }

    /// Where the fields of a block of `len` begin, in words: after the
    /// header, and after the descriptors unless it is uniform.
    #[inline(always)]
    pub const fn first_field(&self, len: usize, uniform: bool) -> usize {
        if uniform {
            self.header_words
        } else {
            self.header_words + self.desc_words(len)
        }
    }

    /// Where field `i`'s descriptor is: its word, and its shift in it.
    #[inline(always)]
    pub const fn desc_at(&self, i: usize) -> (usize, u32) {
        (
            self.header_words + i / self.descs_per_word,
            (i % self.descs_per_word) as u32 * self.desc_bits,
        )
    }

    /// The mask of one descriptor.
    #[inline(always)]
    pub const fn desc_mask(&self) -> u64 {
        (1 << self.desc_bits) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silo_s_header_is_where_its_heap_says() {
        let l = SILO;
        // A block of three fields, kind 1, meta 7: word 0 and word 1.
        let w0 = l.len.put(3);
        let w1 = l.kind.put(l.kinds.data) | l.meta.put(7);
        assert_eq!(w0, 3 << 32);
        assert_eq!(w1, 1 | (7 << 32));
        assert_eq!(l.len.get(w0), 3);
        assert_eq!(l.meta.get(w1), 7);
        assert_eq!(l.first_field(3, false), 3);
        assert_eq!(l.first_field(17, false), 4);
        assert_eq!(l.first_field(17, true), 2);
        assert_eq!(l.desc_at(17), (3, 4));
        assert_eq!(l.immediate(5), 11);
        assert!(!l.is_block(11) && !l.is_block(0) && l.is_block(16));
        // What the emitted counting helper tests in one go: mutable, black.
        assert_eq!(l.mutable.bits_in_place() | l.colour.bits_in_place(), 286720);
        assert_eq!(l.region_floor, 536870912);
    }
}
