//! **How a value is laid out in memory**: the object layout, as a
//! description that everything which reads or writes a block takes from one
//! place -- the runtime's heap, the interpreter, and the code the LLVM back
//! end writes (`docs/RUNTIME-MERGE.md`, "One object model").
//!
//! A layout says:
//! - what a word that is not a block is: nothing, or a small value with no
//!   block (a constructor with no fields, a closure with no captures), and
//!   how one is told from an address;
//! - what a block's header holds -- its count, if it has one, its length, its
//!   kind, its `meta` (a constructor's tag, a closure's methods, a string's
//!   length), the bits a collector keeps -- and where, as a [`Field`] of a
//!   word;
//! - how its fields' descriptors are packed, and where its fields begin;
//! - which number each kind is.
//!
//! What a collector keeps in a header is part of the description too:
//! - **the mark**, the number of the collection that last marked a block:
//!   Silo's count, which nothing else reads under tracing, or a field of its
//!   own;
//! - **a region's blocks**, whose mark is at or above
//!   [`Layout::region_floor`], which no collection's number reaches;
//! - **a block that moved**, which says so in [`Layout::forward`] and keeps
//!   where it went in word [`Layout::forward_to`];
//! - **a free block**, whose kind's word is zero, and which is linked to the
//!   next through word [`Layout::link_word`].
//!
//! There are two layouts:
//! - [`SILO`] is Silo's: counted by reference, with machine pointers.
//! - [`GLADE`] is Glade's, as near as machine pointers allow. It has no
//!   count, so a header of two words holds a block of up to eight fields, and
//!   it is only ever traced.
//!
//! A runtime is built for one of them (`meadow-glade`'s `glade-layout`
//! feature). More of a layout may be made a choice after that.

/// A field of a header word: which word, where in it, and how wide. A field
/// of no bits is one the layout does not have: it reads as zero, and writing
/// it changes nothing.
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

    /// Whether it is the top of its word, so that a shift alone reads it.
    #[inline(always)]
    pub const fn tops(self) -> bool {
        self.shift + self.bits == 64
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
    /// Whether blocks are counted by reference. A layout with no count is
    /// only ever traced.
    pub counted: bool,
    /// The extra references a block has, beyond the one: zero for a block
    /// with one owner. No bits, where blocks are not counted.
    pub count: Field,
    pub len: Field,
    pub kind: Field,
    /// The one descriptor every field of a uniform block has.
    pub elem_desc: Field,
    pub uniform: Field,
    pub meta: Field,
    /// What the cycle collector keeps, in the kind's word: a colour, whether
    /// the block is a kind a cycle can contain (a `Ref`, a mutable array),
    /// whether it is waiting in the candidate buffer, whether it died while it
    /// waited, whether its fields were moved out before it died, whether a
    /// block a cycle could run through was ever stored in it, and whether it
    /// waits for a walk with no budget. See `meadow-glade`'s `heap`.
    pub colour: Field,
    pub mutable: Field,
    pub buffered: Field,
    pub dead: Field,
    pub moved: Field,
    pub cyclable: Field,
    pub deferred: Field,
    /// The number of the collection that last marked the block, under
    /// tracing.
    pub mark: Field,
    /// A mark at or above this is a block inside a compact region, which a
    /// region's block is given -- under counting, [`Layout::sticky`] on top
    /// of the references it has.
    pub region_floor: u64,
    pub sticky: u64,
    /// A block that moved has [`Layout::forwarded`] in this field, and where
    /// it went in word [`Layout::forward_to`].
    pub forward: Field,
    pub forwarded: u64,
    pub forward_to: usize,
    /// The word a free block links the next through. Its kind's word is
    /// zero, which no block in use has.
    pub link_word: usize,
    /// Fields' descriptors: `desc_bits` each. The first `inline_descs` are
    /// in header word `inline_word` from bit `inline_shift`; the rest
    /// follow the header, `descs_per_word` to a word. A uniform block has
    /// none.
    pub desc_bits: u32,
    pub descs_per_word: usize,
    pub inline_descs: usize,
    pub inline_word: usize,
    pub inline_shift: u32,
    pub kinds: Kinds,
}

/// Every kind's number, which both layouts share.
const KINDS: Kinds = Kinds {
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
};

const fn bit(word: usize, shift: u32) -> Field {
    Field {
        word,
        shift,
        bits: 1,
    }
}

/// Silo's layout (`glade/src/heap.rs`):
///
/// ```text
/// word 0   extra references: u32 (0: the only one) | length: u32
/// word 1   kind: u8 | element descriptor: u4 | uniform: u1 | colour: u2 | … | meta: u32
/// words    field descriptors, four bits each, sixteen to a word
///          (absent for a uniform block, whose one descriptor is in word 1)
/// fields
/// ```
///
/// `0` is nothing, and an odd word a value with no block. The count is the
/// mark under tracing; a block that moved has a count of `u32::MAX - 1`, and
/// where it went in word 1.
pub const SILO: Layout = Layout {
    name: "silo",
    null: 0,
    imm_shift: 1,
    imm_tag: 1,
    imm_mask: 1,
    header_words: 2,
    counted: true,
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
    uniform: bit(1, 12),
    colour: Field {
        word: 1,
        shift: 13,
        bits: 2,
    },
    buffered: bit(1, 15),
    dead: bit(1, 16),
    moved: bit(1, 17),
    mutable: bit(1, 18),
    cyclable: bit(1, 19),
    deferred: bit(1, 20),
    meta: Field {
        word: 1,
        shift: 32,
        bits: 32,
    },
    mark: Field {
        word: 0,
        shift: 0,
        bits: 32,
    },
    region_floor: 1 << 29,
    sticky: 1 << 30,
    forward: Field {
        word: 0,
        shift: 0,
        bits: 32,
    },
    forwarded: (u32::MAX - 1) as u64,
    forward_to: 1,
    link_word: 0,
    desc_bits: 4,
    descs_per_word: 16,
    inline_descs: 0,
    inline_word: 0,
    inline_shift: 0,
    kinds: KINDS,
};

/// Glade's layout, with machine pointers (`glade/src/object.rs`):
///
/// ```text
/// word 0   kind: u8 | element descriptor: u4 | uniform: u1 | cycle bits: u8 | mark: u11 | length: u32
/// word 1   meta: u32 | descriptors of fields 0..8, four bits each
/// words    descriptors of fields 8.., sixteen to a word, only while needed
/// fields
/// ```
///
/// There is no count, and so no counting: a block of up to eight fields has
/// two words of header, which Silo's gives three. A region's block has the
/// mark at its top, `2047`. A block that moved has kind `0xFF`, as Glade's
/// do, and where it went in word 1. The cycle collector's bits are where
/// Silo's are, in the kind's word, though nothing counts to need them.
pub const GLADE: Layout = Layout {
    name: "glade",
    null: 0,
    imm_shift: 1,
    imm_tag: 1,
    imm_mask: 1,
    header_words: 2,
    counted: false,
    count: Field {
        word: 0,
        shift: 0,
        bits: 0,
    },
    len: Field {
        word: 0,
        shift: 32,
        bits: 32,
    },
    kind: Field {
        word: 0,
        shift: 0,
        bits: 8,
    },
    elem_desc: Field {
        word: 0,
        shift: 8,
        bits: 4,
    },
    uniform: bit(0, 12),
    colour: Field {
        word: 0,
        shift: 13,
        bits: 2,
    },
    buffered: bit(0, 15),
    dead: bit(0, 16),
    moved: bit(0, 17),
    mutable: bit(0, 18),
    cyclable: bit(0, 19),
    deferred: bit(0, 20),
    mark: Field {
        word: 0,
        shift: 21,
        bits: 11,
    },
    meta: Field {
        word: 1,
        shift: 0,
        bits: 32,
    },
    region_floor: (1 << 11) - 1,
    sticky: (1 << 11) - 1,
    forward: Field {
        word: 0,
        shift: 0,
        bits: 8,
    },
    forwarded: 0xFF,
    forward_to: 1,
    link_word: 1,
    desc_bits: 4,
    descs_per_word: 16,
    inline_descs: 8,
    inline_word: 1,
    inline_shift: 32,
    kinds: KINDS,
};

/// A layout by its name: `silo` or `glade`.
pub fn named(name: &str) -> Option<Layout> {
    match name {
        "silo" => Some(SILO),
        "glade" => Some(GLADE),
        _ => None,
    }
}

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

    /// How many words after the header the descriptors of `len` fields
    /// take.
    #[inline(always)]
    pub const fn desc_words(&self, len: usize) -> usize {
        len.saturating_sub(self.inline_descs)
            .div_ceil(self.descs_per_word)
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
        if i < self.inline_descs {
            (
                self.inline_word,
                self.inline_shift + i as u32 * self.desc_bits,
            )
        } else {
            let j = i - self.inline_descs;
            (
                self.header_words + j / self.descs_per_word,
                (j % self.descs_per_word) as u32 * self.desc_bits,
            )
        }
    }

    /// The mask of one descriptor.
    #[inline(always)]
    pub const fn desc_mask(&self) -> u64 {
        (1 << self.desc_bits) - 1
    }

    /// The word the cycle collector's bits are in: the kind's.
    #[inline(always)]
    pub const fn flags_word(&self) -> usize {
        self.kind.word
    }

    /// The header of a new block of `kind`, `len` fields and `meta`, with
    /// nothing counted, marked or described: each word, as it is written
    /// (`header_words` of them).
    #[inline(always)]
    pub const fn header(&self, kind: u64, len: u64, meta: u64) -> [u64; 2] {
        let mut h = [0u64; 2];
        h[self.len.word] |= self.len.put(len);
        h[self.kind.word] |= self.kind.put(kind);
        h[self.meta.word] |= self.meta.put(meta);
        h
    }

    /// The descriptors `descs` of a non-uniform block, packed: `put(word,
    /// bits)` for each word that holds any, once, with the bits of that word
    /// -- a header word's to be or-ed into it, a word after the header's to
    /// be written whole. Every descriptor word after the header is put.
    #[inline]
    pub fn pack_descs(&self, descs: &[i64], mut put: impl FnMut(usize, u64)) {
        let inline = descs.len().min(self.inline_descs);
        if inline > 0 {
            let mut bits = 0u64;
            for (i, d) in descs[..inline].iter().enumerate() {
                bits |= ((*d as u64) & self.desc_mask()) << self.desc_at(i).1;
            }
            put(self.inline_word, bits);
        }
        let per = self.descs_per_word;
        for w in 0..self.desc_words(descs.len()) {
            let mut bits = 0u64;
            let from = self.inline_descs + per * w;
            for (j, d) in descs.iter().enumerate().skip(from).take(per) {
                bits |= ((*d as u64) & self.desc_mask()) << self.desc_at(j).1;
            }
            put(self.header_words + w, bits);
        }
    }

    /// The most collections a layout's marks can number before they are
    /// numbered again, which stays below its region floor.
    #[inline(always)]
    pub const fn epochs(&self) -> u64 {
        let most = 1 << 27;
        if self.region_floor - 1 < most {
            self.region_floor - 1
        } else {
            most
        }
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
        assert_eq!(l.header(l.kinds.data, 3, 7), [w0, w1]);
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
        assert_eq!(l.epochs(), 1 << 27);
        // The mark is the count, and a block that moved has a count no
        // other has.
        assert_eq!(l.mark, l.count);
        assert!(l.forwarded >= l.region_floor);
        assert_eq!(l.flags_word(), 1);
        assert!(l.meta.tops() && l.len.tops());
    }

    #[test]
    fn glade_s_header_is_two_words_up_to_eight_fields() {
        let l = GLADE;
        assert!(!l.counted && l.count.mask() == 0);
        // Kind, length and meta.
        assert_eq!(l.header(l.kinds.data, 3, 7), [1 | (3 << 32), 7]);
        assert_eq!(l.first_field(0, false), 2);
        assert_eq!(l.first_field(8, false), 2);
        assert_eq!(l.first_field(9, false), 3);
        assert_eq!(l.first_field(24, false), 3);
        assert_eq!(l.first_field(25, false), 4);
        assert_eq!(l.first_field(100, true), 2);
        // The first eight in word 1's high half; the rest after the header.
        assert_eq!(l.desc_at(0), (1, 32));
        assert_eq!(l.desc_at(7), (1, 60));
        assert_eq!(l.desc_at(8), (2, 0));
        assert_eq!(l.desc_at(23), (2, 60));
        assert_eq!(l.desc_at(24), (3, 0));
        let mut words = Vec::new();
        l.pack_descs(&[1, 2, 0, 0, 0, 0, 0, 0, 3], |w, b| words.push((w, b)));
        assert_eq!(words, vec![(1, (1 << 32) | (2 << 36)), (2, 3)]);
        // Every bit of word 0 has one field, and none two.
        let fields = [
            l.kind,
            l.elem_desc,
            l.uniform,
            l.colour,
            l.buffered,
            l.dead,
            l.moved,
            l.mutable,
            l.cyclable,
            l.deferred,
            l.mark,
            l.len,
        ];
        let mut seen = 0u64;
        for f in fields {
            assert_eq!(f.word, 0);
            assert_eq!(seen & f.bits_in_place(), 0, "{f:?} overlaps");
            seen |= f.bits_in_place();
        }
        assert_eq!(seen, u64::MAX);
        // A region's mark is one no collection reaches, and a block that
        // moved is a kind no block is.
        assert_eq!(l.region_floor, l.mark.mask());
        assert!(l.epochs() < l.region_floor);
        assert!(l.forwarded > l.kinds.region && l.forward == l.kind);
        assert_eq!(l.flags_word(), 0);
        assert_ne!(l.link_word, l.kind.word);
    }

    #[test]
    fn silo_s_descriptors_pack_as_its_heap_packs_them() {
        let l = SILO;
        let descs: Vec<i64> = (0..20).map(|i| i % 16).collect();
        let mut words = Vec::new();
        l.pack_descs(&descs, |w, b| words.push((w, b)));
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].0, 2);
        assert_eq!(words[0].1, 0xFEDC_BA98_7654_3210);
        assert_eq!(words[1], (3, 0x3210));
    }
}
