//! A word and its descriptor, read as the value it is -- what the primitives
//! work on, since a word alone does not say.

use crate::heap::{self, Word};
use meadow_rt::desc;
use meadow_rt::num::{Num, Width};
use num_bigint::{BigInt, Sign};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Val {
    Int(i64),
    Word(Width, u64),
    Float(f64),
    Float32(f32),
    Bool(bool),
    Char(char),
    Unit,
    /// An interned name, as the program's symbol table numbers it.
    Sym(usize),
    /// A reference: a block, an object with no block, or nothing.
    Ref(Word),
}

/// `w`, described by `d`.
pub fn val(w: Word, d: i64) -> Val {
    match d {
        desc::REF => Val::Ref(w),
        desc::INT => Val::Int(w as i64),
        desc::FLOAT => Val::Float(f64::from_bits(w)),
        desc::FLOAT32 => Val::Float32(f32::from_bits(w as u32)),
        desc::BOOL => Val::Bool(w != 0),
        desc::CHAR => Val::Char(char::from_u32(w as u32).unwrap_or('\u{fffd}')),
        desc::UNIT => Val::Unit,
        desc::STR => Val::Sym(w as usize),
        d if (desc::WORD..desc::WORD + 7).contains(&d) => {
            Val::Word(Width::ALL[(d - desc::WORD) as usize], w)
        }
        _ => crate::fail(&format!(
            "a value described as {} reached the native runtime",
            desc::name(d)
        )),
    }
}

impl Val {
    /// The word and its descriptor.
    pub fn bits(self) -> (Word, i64) {
        match self {
            Val::Int(n) => (n as Word, desc::INT),
            Val::Word(w, b) => (b, desc::word(w)),
            Val::Float(x) => (x.to_bits(), desc::FLOAT),
            Val::Float32(x) => (u64::from(x.to_bits()), desc::FLOAT32),
            Val::Bool(b) => (Word::from(b), desc::BOOL),
            Val::Char(c) => (c as Word, desc::CHAR),
            Val::Unit => (0, desc::UNIT),
            Val::Sym(s) => (s as Word, desc::STR),
            Val::Ref(w) => (w, desc::REF),
        }
    }

    pub fn word(self) -> Word {
        self.bits().0
    }

    /// A block of `kind`, if this is one.
    pub fn block(self, kind: u64) -> Option<Word> {
        match self {
            Val::Ref(w) if heap::is_block(w) && heap::kind(w) == kind => Some(w),
            _ => None,
        }
    }
}

// --- numbers --------------------------------------------------------------

/// A `BigInt`'s `meta`: its sign.
pub const ZERO: u32 = 0;
pub const PLUS: u32 = 1;
pub const MINUS: u32 = 2;

pub fn num(v: Val) -> Option<Num> {
    Some(match v {
        Val::Int(x) => Num::Int(x),
        Val::Word(w, b) => Num::Word(w, b),
        Val::Float(x) => Num::Float(x),
        Val::Float32(x) => Num::Float32(x),
        Val::Ref(w) => Num::Big(bigint(w)?),
        _ => return None,
    })
}

pub fn bigint(w: Word) -> Option<BigInt> {
    if !heap::is_block(w) || heap::kind(w) != heap::BIGINT {
        return None;
    }
    let sign = match heap::meta(w) {
        ZERO => Sign::NoSign,
        PLUS => Sign::Plus,
        _ => Sign::Minus,
    };
    let bytes: Vec<u8> = (0..heap::len(w))
        .flat_map(|i| heap::field(w, i).to_le_bytes())
        .collect();
    Some(BigInt::from_bytes_le(sign, &bytes))
}

/// A `BigInt`: 64-bit limbs, least significant first, no zero limb on top --
/// the one representation each number has, so equality compares words.
pub fn new_bigint(n: &BigInt) -> Word {
    let meta = match n.sign() {
        Sign::NoSign => ZERO,
        Sign::Plus => PLUS,
        Sign::Minus => MINUS,
    };
    let limbs = n.magnitude().to_u64_digits();
    let v = heap::build_uniform(heap::BIGINT, meta, limbs.len(), desc::INT);
    for (i, l) in limbs.iter().enumerate() {
        heap::set_word(v, heap::ELEMS + i, *l);
    }
    v
}

pub fn from_num(n: Num) -> Val {
    match n {
        Num::Int(x) => Val::Int(x),
        Num::Word(w, b) => Val::Word(w, b),
        Num::Float(x) => Val::Float(x),
        Num::Float32(x) => Val::Float32(x),
        Num::Big(b) => Val::Ref(new_bigint(&b)),
    }
}

// --- strings --------------------------------------------------------------

/// The bytes of a string, where they are, or of an interned name. Borrowed
/// from the string: valid while it is, which is the primitive's call.
pub fn text(v: Val) -> Option<std::borrow::Cow<'static, [u8]>> {
    use std::borrow::Cow;
    match v {
        Val::Ref(w) if heap::is_block(w) && heap::kind(w) == heap::STRING => {
            Some(Cow::Borrowed(heap::str_bytes(w)))
        }
        Val::Sym(s) => Some(Cow::Owned(crate::show::sym_name(s).into_bytes())),
        _ => None,
    }
}

// --- constructors a primitive builds by name ------------------------------

/// The tag of the constructor called `name` in this program.
pub fn tag(name: &str) -> u32 {
    crate::show::names()
        .ctors
        .iter()
        .position(|c| c.as_deref() == Some(name))
        .map_or(u32::MAX, |t| t as u32)
}

/// Data of the constructor `name`, owning `fields`.
pub fn data(name: &str, fields: &[Val]) -> Word {
    let (words, descs): (Vec<Word>, Vec<i64>) = fields.iter().map(|f| f.bits()).unzip();
    heap::build(heap::DATA, tag(name), &words, &descs)
}
