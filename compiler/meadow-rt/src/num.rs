//! Numbers, as every engine's primitives see them.
//!
//! Meadow has one family of integer types -- `Int` (64 bits, also called
//! `Int64`), the sized `Int8` … `Int32`, `UInt8` … `UInt64`, and `BigInt` -- and
//! one of floats: `Float` (also `Float64`) and `Float32`. The operators are
//! shared across each family, so the arithmetic lives here once and each engine
//! only converts its own values to a [`Num`] and back. That is what keeps the
//! CEK machine, the sequent machine and the VM agreeing, the same way
//! [`crate::hash`] does for hashing.
//!
//! # What the types promise, and one thing the runtime has to forgive
//!
//! Inference makes both operands of an operator the same type, so a primitive
//! ordinarily meets two `Word`s of one width, two `Int`s, two `Big`s. The
//! exception is a literal inside a function generic over its integer type:
//! `fun succ n = n + 1` is `forall n. n -> n`, and the `1` in it has no one width
//! to be compiled at, so it is an `Int`. When a `UInt8` arrives, the primitive
//! sees `Word(U8, _)` and `Int(1)`. So an `Int` beside a `Word` or a `Big` takes
//! the other's type ([`int_pair`]), and likewise a `Float` beside a `Float32`.
//! Nothing else can produce a mixed pair.
//!
//! Every fixed width wraps on overflow, as `Int` always has, and converting to a
//! fixed width keeps the low bits of the two's complement -- from a `BigInt`
//! too, since a literal nothing pins down is one. `BigInt` itself never
//! overflows.

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};
use std::cmp::Ordering;

/// A sized integer type other than `Int` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Width {
    I8,
    I16,
    I32,
    U8,
    U16,
    U32,
    U64,
}

impl Width {
    pub const ALL: [Width; 7] = [
        Width::I8,
        Width::I16,
        Width::I32,
        Width::U8,
        Width::U16,
        Width::U32,
        Width::U64,
    ];

    /// The width a type name denotes: `Int8` is [`Width::I8`]. `Int` and `Int64`
    /// are not widths -- they are the plain `Int`.
    pub fn from_type(name: &str) -> Option<Width> {
        Width::ALL.into_iter().find(|w| w.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Width::I8 => "Int8",
            Width::I16 => "Int16",
            Width::I32 => "Int32",
            Width::U8 => "UInt8",
            Width::U16 => "UInt16",
            Width::U32 => "UInt32",
            Width::U64 => "UInt64",
        }
    }

    pub const fn bits(self) -> u32 {
        match self {
            Width::I8 | Width::U8 => 8,
            Width::I16 | Width::U16 => 16,
            Width::I32 | Width::U32 => 32,
            Width::U64 => 64,
        }
    }

    pub const fn signed(self) -> bool {
        matches!(self, Width::I8 | Width::I16 | Width::I32)
    }

    const fn mask(self) -> u64 {
        if self.bits() == 64 {
            u64::MAX
        } else {
            (1u64 << self.bits()) - 1
        }
    }

    /// `x` wrapped to this width: its low bits, which is how a value of the
    /// type is held.
    pub fn wrap(self, x: i128) -> u64 {
        (x as u64) & self.mask()
    }

    /// The number held bits `b` stand for -- sign-extended for a signed width.
    pub fn value(self, b: u64) -> i128 {
        let b = b & self.mask();
        if self.signed() && (b >> (self.bits() - 1)) & 1 == 1 {
            b as i128 - (1i128 << self.bits())
        } else {
            b as i128
        }
    }

    /// Whether `x` is a value of this type as it stands, without wrapping.
    pub fn fits(self, x: i128) -> bool {
        self.value(self.wrap(x)) == x
    }
}

/// A number, whichever engine it came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Num {
    Int(i64),
    /// A sized integer: its width and its bits, masked to that width.
    Word(Width, u64),
    Big(BigInt),
    Float(f64),
    Float32(f32),
}

impl Num {
    /// The integer this is, exactly, if it is an integer that fits in `i128`.
    fn exact(&self) -> Option<i128> {
        match self {
            Num::Int(x) => Some(*x as i128),
            Num::Word(w, b) => Some(w.value(*b)),
            Num::Big(x) => x.to_i128(),
            _ => None,
        }
    }

    fn is_integer(&self) -> bool {
        matches!(self, Num::Int(_) | Num::Word(..) | Num::Big(_))
    }
}

/// How a number prints: an integer in decimal, a float always with a fraction.
pub fn show(n: &Num) -> String {
    match n {
        Num::Int(x) => x.to_string(),
        Num::Word(w, b) => w.value(*b).to_string(),
        Num::Big(x) => x.to_string(),
        Num::Float(x) => crate::fmt_float(*x),
        Num::Float32(x) => fmt_float32(*x),
    }
}

/// [`crate::fmt_float`] for a `Float32`: its own shortest form, not the
/// longer one its `f64` widening would print.
pub fn fmt_float32(x: f32) -> String {
    if x.is_finite() && x == x.trunc() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

// --- integers ----------------------------------------------------------------

enum IntPair {
    Int(i64, i64),
    Word(Width, u64, u64),
    Big(BigInt, BigInt),
}

/// Two integers as one type -- see the module docs for the `Int` that gives way.
fn int_pair(what: &str, a: Num, b: Num) -> Result<IntPair, String> {
    Ok(match (a, b) {
        (Num::Int(x), Num::Int(y)) => IntPair::Int(x, y),
        (Num::Word(w, x), Num::Word(v, y)) if w == v => IntPair::Word(w, x, y),
        (Num::Word(w, x), Num::Int(y)) => IntPair::Word(w, x, w.wrap(y as i128)),
        (Num::Int(x), Num::Word(w, y)) => IntPair::Word(w, w.wrap(x as i128), y),
        (Num::Big(x), Num::Big(y)) => IntPair::Big(x, y),
        (Num::Big(x), Num::Int(y)) => IntPair::Big(x, BigInt::from(y)),
        (Num::Int(x), Num::Big(y)) => IntPair::Big(BigInt::from(x), y),
        (a, b) => {
            return Err(format!(
                "`{what}` expects two integers of one type, got {} and {}",
                show(&a),
                show(&b)
            ));
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

impl Arith {
    const fn symbol(self) -> &'static str {
        match self {
            Arith::Add => "+",
            Arith::Sub => "-",
            Arith::Mul => "*",
            Arith::Div => "/",
            Arith::Mod => "%",
            Arith::Pow => "^",
        }
    }
}

/// `+ - * / % ^` on integers. Fixed widths wrap; division truncates toward
/// zero and `%` takes the dividend's sign; dividing by zero is an error.
pub fn int_arith(op: Arith, a: Num, b: Num) -> Result<Num, String> {
    let exponent = |e: Option<u32>, shown: String| {
        e.ok_or_else(|| format!("`^` exponent must fit in u32, got {shown}"))
    };
    match int_pair(op.symbol(), a, b)? {
        IntPair::Int(x, y) => Ok(Num::Int(match op {
            Arith::Add => x.wrapping_add(y),
            Arith::Sub => x.wrapping_sub(y),
            Arith::Mul => x.wrapping_mul(y),
            Arith::Div if y == 0 => return Err("division by zero".into()),
            Arith::Div => x.wrapping_div(y),
            Arith::Mod if y == 0 => return Err("modulo by zero".into()),
            Arith::Mod => x.wrapping_rem(y),
            Arith::Pow => x.wrapping_pow(exponent(u32::try_from(y).ok(), y.to_string())?),
        })),
        IntPair::Word(w, x, y) => {
            let (x, y) = (w.value(x), w.value(y));
            // Every operand fits in 64 bits, so the low bits of each `i128`
            // result are the wrapped answer.
            Ok(Num::Word(
                w,
                w.wrap(match op {
                    Arith::Add => x.wrapping_add(y),
                    Arith::Sub => x.wrapping_sub(y),
                    Arith::Mul => x.wrapping_mul(y),
                    Arith::Div if y == 0 => return Err("division by zero".into()),
                    Arith::Div => x / y,
                    Arith::Mod if y == 0 => return Err("modulo by zero".into()),
                    Arith::Mod => x % y,
                    Arith::Pow => x.wrapping_pow(exponent(u32::try_from(y).ok(), y.to_string())?),
                }),
            ))
        }
        IntPair::Big(x, y) => Ok(Num::Big(match op {
            Arith::Add => x + y,
            Arith::Sub => x - y,
            Arith::Mul => x * y,
            Arith::Div if y.is_zero() => return Err("division by zero".into()),
            Arith::Div => x / y,
            Arith::Mod if y.is_zero() => return Err("modulo by zero".into()),
            Arith::Mod => x % y,
            Arith::Pow => {
                let e = exponent(y.to_u32(), y.to_string())?;
                x.pow(e)
            }
        })),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Gt,
    Le,
    Ge,
}

impl Cmp {
    const fn symbol(self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Gt => ">",
            Cmp::Le => "<=",
            Cmp::Ge => ">=",
        }
    }

    fn holds(self, o: Ordering) -> bool {
        match self {
            Cmp::Lt => o == Ordering::Less,
            Cmp::Gt => o == Ordering::Greater,
            Cmp::Le => o != Ordering::Greater,
            Cmp::Ge => o != Ordering::Less,
        }
    }
}

/// `< > <= >=` on integers, by value -- unsigned widths compare as unsigned.
pub fn int_cmp(op: Cmp, a: Num, b: Num) -> Result<bool, String> {
    let o = match int_pair(op.symbol(), a, b)? {
        IntPair::Int(x, y) => x.cmp(&y),
        IntPair::Word(w, x, y) => w.value(x).cmp(&w.value(y)),
        IntPair::Big(x, y) => x.cmp(&y),
    };
    Ok(op.holds(o))
}

pub fn int_neg(a: Num) -> Result<Num, String> {
    Ok(match a {
        Num::Int(x) => Num::Int(x.wrapping_neg()),
        Num::Word(w, b) => Num::Word(w, w.wrap(-w.value(b))),
        Num::Big(x) => Num::Big(-x),
        other => return Err(format!("`neg` expects an integer, got {}", show(&other))),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bits {
    Shl,
    Shr,
    Ushr,
    And,
    Or,
    Xor,
}

impl Bits {
    const fn name(self) -> &'static str {
        match self {
            Bits::Shl => "shl",
            Bits::Shr => "shr",
            Bits::Ushr => "ushr",
            Bits::And => "bitAnd",
            Bits::Or => "bitOr",
            Bits::Xor => "bitXor",
        }
    }
}

/// The bitwise operators. A shift amount is taken modulo the width, as `Int`'s
/// always was; `shr` is arithmetic on a signed type and logical on an unsigned
/// one, and `ushr` is logical everywhere. On a `BigInt`, `and`/`or`/`xor` act on
/// the infinite two's complement and `ushr` has no meaning.
pub fn int_bits(op: Bits, a: Num, b: Num) -> Result<Num, String> {
    match int_pair(op.name(), a, b)? {
        IntPair::Int(x, y) => Ok(Num::Int(match op {
            Bits::Shl => x.wrapping_shl(y as u32),
            Bits::Shr => x.wrapping_shr(y as u32),
            Bits::Ushr => (x as u64).wrapping_shr(y as u32) as i64,
            Bits::And => x & y,
            Bits::Or => x | y,
            Bits::Xor => x ^ y,
        })),
        IntPair::Word(w, x, y) => {
            let s = (w.value(y) as u64 % w.bits() as u64) as u32;
            let x = x & w.mask();
            Ok(Num::Word(
                w,
                match op {
                    Bits::Shl => w.wrap((x << s) as i128),
                    Bits::Shr if w.signed() => w.wrap(w.value(x) >> s),
                    Bits::Shr | Bits::Ushr => x >> s,
                    Bits::And => x & (y & w.mask()),
                    Bits::Or => x | (y & w.mask()),
                    Bits::Xor => x ^ (y & w.mask()),
                },
            ))
        }
        IntPair::Big(x, y) => {
            let shift = || {
                y.to_usize()
                    .ok_or_else(|| format!("`{}`: shift amount {y} out of range", op.name()))
            };
            Ok(Num::Big(match op {
                Bits::Shl => x << shift()?,
                Bits::Shr => x >> shift()?,
                Bits::Ushr => return Err("`ushr` has no meaning for a BigInt; use `shr`".into()),
                Bits::And => x & y,
                Bits::Or => x | y,
                Bits::Xor => x ^ y,
            }))
        }
    }
}

pub fn int_not(a: Num) -> Result<Num, String> {
    Ok(match a {
        Num::Int(x) => Num::Int(!x),
        Num::Word(w, b) => Num::Word(w, !b & w.mask()),
        Num::Big(x) => Num::Big(!x),
        other => return Err(format!("`bitNot` expects an integer, got {}", show(&other))),
    })
}

/// How many bits are set. A negative `BigInt` has infinitely many.
pub fn pop_count(a: Num) -> Result<i64, String> {
    Ok(match a {
        Num::Int(x) => x.count_ones() as i64,
        Num::Word(w, b) => (b & w.mask()).count_ones() as i64,
        Num::Big(x) if x.is_negative() => {
            return Err(format!("`popCount` of a negative BigInt: {x}"));
        }
        Num::Big(x) => x.magnitude().count_ones() as i64,
        other => {
            return Err(format!(
                "`popCount` expects an integer, got {}",
                show(&other)
            ));
        }
    })
}

/// How many bits the type holds: 64 for `Int`, the width for a sized type. A
/// `BigInt` has no fixed width, so asking is an error.
pub fn bit_width(a: &Num) -> Result<i64, String> {
    Ok(match a {
        Num::Int(_) => 64,
        Num::Word(w, _) => w.bits() as i64,
        Num::Big(_) => return Err("`bitWidth` of a BigInt, which has no fixed width".into()),
        other => {
            return Err(format!(
                "`bitWidth` expects an integer, got {}",
                show(other)
            ));
        }
    })
}

/// The integer type a conversion produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntTarget {
    Int,
    Big,
    Word(Width),
}

impl IntTarget {
    pub fn name(self) -> &'static str {
        match self {
            IntTarget::Int => "Int",
            IntTarget::Big => "BigInt",
            IntTarget::Word(w) => w.name(),
        }
    }
}

/// `toInt`, `toBigInt`, `toUInt8` and the rest. To a fixed width the low bits
/// are kept and reinterpreted, as Rust's `as` does -- whatever the source.
pub fn to_int(target: IntTarget, a: Num) -> Result<Num, String> {
    let fn_name = match target {
        IntTarget::Big => "toBigInt".to_string(),
        other => format!("to{}", other.name()),
    };
    if let Num::Big(x) = &a {
        return Ok(match target {
            IntTarget::Big => a.clone(),
            IntTarget::Int => Num::Int(low_bits(x) as i64),
            IntTarget::Word(w) => Num::Word(w, w.wrap(low_bits(x) as i128)),
        });
    }
    let v = a
        .exact()
        .filter(|_| a.is_integer())
        .ok_or_else(|| format!("`{fn_name}` expects an integer, got {}", show(&a)))?;
    Ok(match target {
        IntTarget::Int => Num::Int(v as i64),
        IntTarget::Big => Num::Big(BigInt::from(v)),
        IntTarget::Word(w) => Num::Word(w, w.wrap(v)),
    })
}

/// The low 64 bits of `x`'s two's complement.
fn low_bits(x: &BigInt) -> u64 {
    let bytes = x.to_signed_bytes_le();
    let fill = if x.is_negative() { 0xFF } else { 0 };
    let mut word = [fill; 8];
    for (slot, b) in word.iter_mut().zip(bytes) {
        *slot = b;
    }
    u64::from_le_bytes(word)
}

// --- floats ------------------------------------------------------------------

enum FloatPair {
    F64(f64, f64),
    F32(f32, f32),
}

fn float_pair(what: &str, a: Num, b: Num) -> Result<FloatPair, String> {
    Ok(match (a, b) {
        (Num::Float(x), Num::Float(y)) => FloatPair::F64(x, y),
        (Num::Float32(x), Num::Float32(y)) => FloatPair::F32(x, y),
        (Num::Float32(x), Num::Float(y)) => FloatPair::F32(x, y as f32),
        (Num::Float(x), Num::Float32(y)) => FloatPair::F32(x as f32, y),
        (a, b) => {
            return Err(format!(
                "`{what}` expects two floats of one type, got {} and {}",
                show(&a),
                show(&b)
            ));
        }
    })
}

/// `+ - * /` on floats: the float primitives.
pub fn float_arith(op: Arith, a: Num, b: Num) -> Result<Num, String> {
    let what = op.symbol().to_string();
    Ok(match float_pair(&what, a, b)? {
        FloatPair::F64(x, y) => Num::Float(match op {
            Arith::Add => x + y,
            Arith::Sub => x - y,
            Arith::Mul => x * y,
            _ => x / y,
        }),
        FloatPair::F32(x, y) => Num::Float32(match op {
            Arith::Add => x + y,
            Arith::Sub => x - y,
            Arith::Mul => x * y,
            _ => x / y,
        }),
    })
}

/// `< > <= >=` on floats -- IEEE comparisons, so anything against NaN is
/// false.
pub fn float_cmp(op: Cmp, a: Num, b: Num) -> Result<bool, String> {
    let what = op.symbol().to_string();
    Ok(match float_pair(&what, a, b)? {
        FloatPair::F64(x, y) => match op {
            Cmp::Lt => x < y,
            Cmp::Gt => x > y,
            Cmp::Le => x <= y,
            Cmp::Ge => x >= y,
        },
        FloatPair::F32(x, y) => match op {
            Cmp::Lt => x < y,
            Cmp::Gt => x > y,
            Cmp::Le => x <= y,
            Cmp::Ge => x >= y,
        },
    })
}

/// `toFloat`: an integer, or either float, as a `Float`.
pub fn to_float(a: Num) -> Result<f64, String> {
    Ok(match a {
        Num::Int(x) => x as f64,
        Num::Word(w, b) => w.value(b) as f64,
        Num::Big(x) => x.to_f64().unwrap_or(f64::NAN),
        Num::Float(x) => x,
        Num::Float32(x) => x as f64,
    })
}

/// `toFloat32`: either float, narrowed or kept.
pub fn to_float32(a: Num) -> Result<f32, String> {
    Ok(match a {
        Num::Float(x) => x as f32,
        Num::Float32(x) => x,
        other => return Err(format!("`toFloat32` expects a float, got {}", show(&other))),
    })
}

/// `floor`: round toward negative infinity, into an `Int`.
pub fn floor(a: Num) -> Result<i64, String> {
    let x = match a {
        Num::Float(x) => x,
        Num::Float32(x) => x as f64,
        other => return Err(format!("`floor` expects a float, got {}", show(&other))),
    };
    let f = x.floor();
    if !f.is_finite() {
        return Err(format!("`floor` of a non-finite Float: {x}"));
    }
    Ok(f as i64)
}

// --- equality and hashing ------------------------------------------------------

/// `==` on two numbers: integers by value, floats by value in the narrower of
/// their two types. Only ever asked about two numbers of one type -- or a
/// generic literal beside one, which is why a `Float` meeting a `Float32` is
/// compared as a `Float32`.
pub fn num_eq(a: &Num, b: &Num) -> bool {
    match (a, b) {
        (Num::Float(x), Num::Float(y)) => x == y,
        (Num::Float32(x), Num::Float32(y)) => x == y,
        (Num::Float32(x), Num::Float(y)) | (Num::Float(y), Num::Float32(x)) => *x == *y as f32,
        (Num::Big(x), Num::Big(y)) => x == y,
        (x, y) if x.is_integer() && y.is_integer() => match (x.exact(), y.exact()) {
            (Some(p), Some(q)) => p == q,
            // One is a `BigInt` beyond `i128`, which no fixed width can equal.
            _ => false,
        },
        _ => false,
    }
}

/// Feed a number to a hash so that equal integers hash alike whatever their
/// type, which is what keeps a generic literal and the value it stands beside
/// in the same slot of a table.
pub fn hash_into(h: &mut crate::hash::Hasher, n: &Num) {
    match n {
        Num::Float(x) => h.float(*x),
        Num::Float32(x) => h.float(*x as f64),
        Num::Big(x) => match x.to_i64() {
            Some(v) => h.int(v),
            None => h.bigint(&x.to_signed_bytes_le()),
        },
        other => match other.exact().and_then(|v| i64::try_from(v).ok()) {
            Some(v) => h.int(v),
            None => h.bigint(&BigInt::from(other.exact().unwrap_or(0)).to_signed_bytes_le()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(width: Width, x: i128) -> Num {
        Num::Word(width, width.wrap(x))
    }

    #[test]
    fn fixed_widths_wrap() {
        assert_eq!(
            int_arith(Arith::Add, w(Width::U8, 250), w(Width::U8, 10)),
            Ok(w(Width::U8, 4))
        );
        assert_eq!(
            int_arith(Arith::Add, w(Width::I8, 127), w(Width::I8, 1)),
            Ok(w(Width::I8, -128))
        );
        assert_eq!(
            int_arith(Arith::Sub, w(Width::U32, 0), w(Width::U32, 1)),
            Ok(w(Width::U32, u32::MAX as i128))
        );
        assert_eq!(
            int_arith(Arith::Div, w(Width::I8, -128), w(Width::I8, -1)),
            Ok(w(Width::I8, -128))
        );
        assert_eq!(
            int_arith(
                Arith::Mul,
                w(Width::U64, u64::MAX as i128),
                w(Width::U64, 2)
            ),
            Ok(w(Width::U64, (u64::MAX - 1) as i128))
        );
    }

    #[test]
    fn unsigned_compares_as_unsigned() {
        assert_eq!(
            int_cmp(Cmp::Gt, w(Width::U64, u64::MAX as i128), w(Width::U64, 1)),
            Ok(true)
        );
        assert_eq!(
            int_cmp(Cmp::Lt, w(Width::I8, -1), w(Width::I8, 1)),
            Ok(true)
        );
        assert_eq!(
            show(&w(Width::U64, u64::MAX as i128)),
            "18446744073709551615"
        );
    }

    #[test]
    fn a_generic_literal_takes_its_neighbours_type() {
        assert_eq!(
            int_arith(Arith::Add, w(Width::U8, 255), Num::Int(1)),
            Ok(w(Width::U8, 0))
        );
        assert_eq!(
            int_arith(
                Arith::Add,
                Num::Big(BigInt::from(1u64 << 62)),
                Num::Int(1 << 62)
            ),
            Ok(Num::Big(BigInt::from(1u64 << 63)))
        );
        assert!(num_eq(&w(Width::I16, -3), &Num::Int(-3)));
        assert!(!num_eq(&w(Width::U8, 255), &Num::Int(-1)));
        assert!(num_eq(&Num::Float32(0.1), &Num::Float(0.1)));
    }

    #[test]
    fn shifts_and_bits_respect_the_width() {
        assert_eq!(
            int_bits(Bits::Shl, w(Width::U8, 0b1000_0001), w(Width::U8, 1)),
            Ok(w(Width::U8, 0b10))
        );
        assert_eq!(
            int_bits(Bits::Shr, w(Width::I8, -128), w(Width::I8, 7)),
            Ok(w(Width::I8, -1))
        );
        assert_eq!(
            int_bits(Bits::Ushr, w(Width::I8, -128), w(Width::I8, 7)),
            Ok(w(Width::I8, 1))
        );
        assert_eq!(int_not(w(Width::U8, 0)), Ok(w(Width::U8, 255)));
        assert_eq!(pop_count(w(Width::I8, -1)), Ok(8));
    }

    #[test]
    fn conversions_keep_the_low_bits() {
        assert_eq!(
            to_int(IntTarget::Word(Width::U8), Num::Int(300)),
            Ok(w(Width::U8, 44))
        );
        assert_eq!(
            to_int(IntTarget::Word(Width::I8), w(Width::U8, 255)),
            Ok(w(Width::I8, -1))
        );
        assert_eq!(
            to_int(IntTarget::Int, w(Width::U32, u32::MAX as i128)),
            Ok(Num::Int(u32::MAX as i64))
        );
        assert_eq!(
            to_int(IntTarget::Word(Width::U8), Num::Big(BigInt::from(300))),
            Ok(w(Width::U8, 44))
        );
        assert_eq!(
            to_int(IntTarget::Word(Width::U8), Num::Big(BigInt::from(-1))),
            Ok(w(Width::U8, 255))
        );
        // Past 64 bits, only the low ones count.
        let big = (BigInt::from(1) << 100usize) + BigInt::from(7);
        assert_eq!(to_int(IntTarget::Int, Num::Big(big)), Ok(Num::Int(7)));
        assert_eq!(
            to_int(
                IntTarget::Int,
                Num::Big(-(BigInt::from(1) << 70usize) - BigInt::from(1))
            ),
            Ok(Num::Int(-1))
        );
    }

    #[test]
    fn equal_integers_hash_alike_whatever_their_type() {
        let of = |n: &Num| {
            let mut h = crate::hash::Hasher::new();
            hash_into(&mut h, n);
            h.finish()
        };
        assert_eq!(of(&w(Width::U8, 7)), of(&Num::Int(7)));
        assert_eq!(of(&Num::Big(BigInt::from(7))), of(&Num::Int(7)));
        assert_ne!(of(&w(Width::U8, 7)), of(&Num::Int(8)));
    }

    #[test]
    fn float32_prints_its_own_shortest_form() {
        assert_eq!(show(&Num::Float32(0.1)), "0.1");
        assert_eq!(show(&Num::Float32(2.0)), "2.0");
    }
}
