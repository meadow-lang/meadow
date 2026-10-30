//! The primitives.
//!
//! Ported operation for operation from `meadow_eval::run_prim` — the wrapping
//! arithmetic, the two zero checks, the clamping in `arraySlice`, the exact
//! error strings where a program can see them. Anything that differs is a
//! difference between the two runtimes rather than a difference in the language,
//! and the differential tests would call it a bug.
//!
//! # Allocation discipline
//!
//! The collector moves objects, so a primitive that allocates cannot be holding
//! an address when it does. Every one of them follows the same three steps:
//!
//! 1. **measure** — read whatever sizes the result depends on;
//! 2. [`Vm::ensure`] — make room, which may collect and move everything;
//! 3. **re-read** the arguments out of registers, which the collector has
//!    updated, and only then allocate.
//!
//! Skipping step 3 is the one mistake this file can make, and it would show up
//! as a value that is subtly the wrong object rather than as a crash — so the
//! places it matters say so.

use crate::heap::{Heap, Kind};
use crate::value::{Addr, Value};
use crate::vm::{Error, Vm, err};
use meadow_rt::roles::Role;
use meadow_rt::{Prim, num};
use num_bigint::{BigInt, Sign};

fn arith(p: Prim) -> num::Arith {
    match p {
        Prim::Add | Prim::AddF => num::Arith::Add,
        Prim::Sub | Prim::SubF => num::Arith::Sub,
        Prim::Mul | Prim::MulF => num::Arith::Mul,
        Prim::Div | Prim::DivF => num::Arith::Div,
        Prim::Mod => num::Arith::Mod,
        _ => num::Arith::Pow,
    }
}

fn cmp(p: Prim) -> num::Cmp {
    match p {
        Prim::Lt | Prim::LtF => num::Cmp::Lt,
        Prim::Gt | Prim::GtF => num::Cmp::Gt,
        Prim::Le | Prim::LeF => num::Cmp::Le,
        _ => num::Cmp::Ge,
    }
}

fn bits(p: Prim) -> num::Bits {
    match p {
        Prim::Shl => num::Bits::Shl,
        Prim::Shr => num::Bits::Shr,
        Prim::Ushr => num::Bits::Ushr,
        Prim::BitAnd => num::Bits::And,
        Prim::BitOr => num::Bits::Or,
        _ => num::Bits::Xor,
    }
}

/// What a transaction says on a machine that has no scheduler behind it.
const NO_WORLD: &str = "transactions need the scheduler; this machine is running on its own";

impl Vm<'_> {
    /// An array -- or a mutable one -- of `words`, each a value of descriptor
    /// `d`. Room must have been made.
    fn array_of(&mut self, kind: Kind, words: &[u64], d: meadow_rt::desc::Desc) -> Value {
        Value::Obj(
            self.heap
                .alloc_described(kind, 0, words.len(), |i| words[i], |_| d),
        )
    }

    pub(crate) fn alloc_bigint(&mut self, n: BigInt) -> Value {
        let meta = match n.sign() {
            Sign::NoSign => big::ZERO,
            Sign::Plus => big::PLUS,
            Sign::Minus => big::MINUS,
        };
        let limbs = n.magnitude().to_u64_digits();
        self.alloc_limbs(meta, &limbs)
    }

    /// A `BigInt` of sign `meta` and magnitude `limbs`: 64-bit limbs, least
    /// significant first, with no zero limb at the top -- the one
    /// representation each number has, which is what lets equality compare
    /// words.
    ///
    /// Makes its own room. The limbs are Rust memory, so nothing can move them.
    pub(crate) fn alloc_limbs(&mut self, meta: u32, limbs: &[u64]) -> Value {
        debug_assert!(limbs.last() != Some(&0), "a BigInt with a zero top limb");
        let meta = if limbs.is_empty() { big::ZERO } else { meta };
        self.ensure(Heap::size_of(Kind::BigInt, limbs.len()));
        Value::Obj(self.heap.alloc_described(
            Kind::BigInt,
            meta,
            limbs.len(),
            |i| limbs[i],
            |_| meadow_rt::desc::INT,
        ))
    }

    /// An integer operand as sign and magnitude, for [`big`]: a `BigInt`'s own
    /// limbs, borrowed where they lie when they lie in the nursery, or an
    /// `Int`'s one limb. `None` for anything else.
    fn limbs_of(&self, v: Value) -> Option<(i8, std::borrow::Cow<'_, [u64]>)> {
        use std::borrow::Cow;
        match v {
            Value::Int(0) => Some((0, Cow::Borrowed(&[]))),
            Value::Int(x) => Some((x.signum() as i8, Cow::Owned(vec![x.unsigned_abs()]))),
            Value::Obj(a) if self.heap.kind(a) == Kind::BigInt => {
                let sign = match self.heap.meta(a) {
                    big::ZERO => 0,
                    big::PLUS => 1,
                    _ => -1,
                };
                let limbs = match self.heap.nursery_words(a) {
                    Some(slice) => Cow::Borrowed(slice),
                    None => Cow::Owned(self.heap.words(a).collect()),
                };
                Some((sign, limbs))
            }
            _ => None,
        }
    }

    /// `a + b` or `a - b` where one side is a `BigInt` and the other a `BigInt`
    /// or an `Int`, done on the limbs where they are. `None` when the operands
    /// are not that, for the general path to handle.
    ///
    /// The point is what it does not do. The general path reads each operand
    /// off the heap into a `num_bigint::BigInt`, adds, and writes the answer
    /// back -- and on a loop adding 70,000-bit numbers that conversion was 80%
    /// of the run and the addition 9%.
    fn big_add_sub(&mut self, subtract: bool, a: Value, b: Value) -> Option<Value> {
        if !matches!((a, b), (Value::Obj(_), _) | (_, Value::Obj(_))) {
            return None;
        }
        let (sign, limbs) = {
            let (sa, la) = self.limbs_of(a)?;
            let (sb, lb) = self.limbs_of(b)?;
            let sb = if subtract { -sb } else { sb };
            big::add(sa, &la, sb, &lb)
        };
        // Computed into Rust memory before any room is made, so the collector
        // moving the operands cannot matter: nothing below reads them.
        let meta = if sign < 0 { big::MINUS } else { big::PLUS };
        Some(self.alloc_limbs(meta, &limbs))
    }

    /// `a` against `b`, for an integer comparison with a `BigInt` on either
    /// side. Reads and never allocates, as a fused comparison must not.
    fn big_cmp(&self, a: Value, b: Value) -> Option<std::cmp::Ordering> {
        if !matches!((a, b), (Value::Obj(_), _) | (_, Value::Obj(_))) {
            return None;
        }
        let (sa, la) = self.limbs_of(a)?;
        let (sb, lb) = self.limbs_of(b)?;
        Some(big::cmp(sa, &la, sb, &lb))
    }

    fn int(&self, v: Value) -> Result<i64, Error> {
        match v {
            Value::Int(i) => Ok(i),
            other => err(format!("expected an Int, got {}", self.show(other))),
        }
    }

    fn index(&self, v: Value) -> Result<usize, Error> {
        match self.int(v)? {
            i if i >= 0 => Ok(i as usize),
            i => err(format!("expected a non-negative index, got {i}")),
        }
    }

    /// A number, for `meadow_core::num`. A `BigInt` is read off the heap, which
    /// allocates nothing there.
    pub(crate) fn num(&self, v: Value) -> Result<num::Num, Error> {
        match self.try_num(v) {
            Some(n) => Ok(n),
            None => err(format!("expected a number, got {}", self.show(v))),
        }
    }

    /// [`Vm::num`], without building an error for something that is not one.
    pub(crate) fn try_num(&self, v: Value) -> Option<num::Num> {
        Some(match v {
            Value::Int(x) => num::Num::Int(x),
            Value::Word(w, b) => num::Num::Word(w, b),
            Value::Float(x) => num::Num::Float(x),
            Value::Float32(x) => num::Num::Float32(x),
            other => num::Num::Big(self.bigint_at(other)?),
        })
    }

    /// A number as a value. Only a `BigInt` allocates, and `alloc_bigint` makes
    /// its own room -- nothing heap-shaped may be held across this call.
    fn from_num(&mut self, n: num::Num) -> Value {
        match n {
            num::Num::Int(x) => Value::Int(x),
            num::Num::Word(w, b) => Value::Word(w, b),
            num::Num::Big(x) => self.alloc_bigint(x),
            num::Num::Float(x) => Value::Float(x),
            num::Num::Float32(x) => Value::Float32(x),
        }
    }

    /// The array `v` is, kept either way -- see `Kind::Bytes`.
    fn array(&self, v: Value) -> Result<Addr, Error> {
        match v.addr().filter(|a| self.heap.kind(*a).is_array()) {
            Some(a) => Ok(a),
            None => err(format!("expected an Array, got {}", self.show(v))),
        }
    }

    /// The run's `TVar`s.
    /// The `TVar`s, borrowed: read straight out of the field wherever the
    /// transaction is needed mutably beside it. Never cloned -- the count of
    /// an `Arc` every thread holds is a word every thread would then write at
    /// every read of every transaction.
    fn world(&self) -> Result<&crate::stm::World, Error> {
        match &self.world {
            Some(w) => Ok(w),
            None => err(NO_WORLD),
        }
    }

    /// `v` put where every thread can read it: into `into` -- a region, whether
    /// it is new, and how big a fresh copy of what it holds was -- or into a
    /// new region. An immediate needs no region.
    fn share(
        &mut self,
        v: Value,
        into: Option<(std::sync::Arc<crate::region::Region>, bool, usize)>,
    ) -> Result<crate::stm::Shared, Error> {
        if !matches!(v, Value::Obj(_)) {
            return Ok(crate::stm::Shared {
                region: None,
                root: v,
                fresh: 0,
            });
        }
        let (region, fresh_region, fresh) =
            into.unwrap_or_else(|| (crate::region::Region::new(), true, 0));
        self.heap.adopt(&region);
        let root = match self.heap.compact_into(region.id, v) {
            Ok(root) => root,
            Err(why) => return err(meadow_rt::stm::unstorable(why.describe())),
        };
        let fresh = if fresh_region { region.used() } else { fresh };
        Ok(crate::stm::Shared {
            region: Some(region),
            root,
            fresh,
        })
    }

    /// The transaction this thread is in, for `op`.
    fn txn(&mut self, op: &str) -> Result<&mut crate::stm::Txn, Error> {
        match &mut self.txn {
            Some(t) if t.live => Ok(t),
            _ => err(meadow_rt::stm::outside(op)),
        }
    }

    /// `v` exported for another thread, or the error saying why it cannot be.
    fn sendable(&self, v: Value) -> Result<crate::heap::Parcel, Error> {
        self.heap
            .export(v)
            .or_else(|why| err(meadow_rt::thread::unsendable(why.describe())))
    }

    /// The number of a channel or thread handle.
    fn handle(&self, v: Value, kind: Kind, what: &str) -> Result<u32, Error> {
        match v.addr().filter(|a| self.heap.kind(*a) == kind) {
            Some(a) => Ok(self.heap.meta(a)),
            None => err(format!("expected {what}, got {}", self.show(v))),
        }
    }

    fn compact_handle(&self, v: Value) -> Result<Addr, Error> {
        match v.addr().filter(|a| self.heap.kind(*a) == Kind::Compact) {
            Some(a) => Ok(a),
            None => err(format!("expected a Compact, got {}", self.show(v))),
        }
    }

    fn mut_array(&self, v: Value) -> Result<Addr, Error> {
        match v.addr().filter(|a| self.heap.kind(*a) == Kind::MutArray) {
            Some(a) => Ok(a),
            None => err(format!("expected a mutable array, got {}", self.show(v))),
        }
    }

    /// A mutable array and its header, read once: the kind check, the length
    /// and the field offset all come out of the same two slot reads.
    fn mut_array_head(&self, v: Value) -> Result<(Addr, crate::object::Head), Error> {
        match v.addr() {
            Some(a) => {
                let h = self.heap.head(a);
                if h.kind == Kind::MutArray {
                    return Ok((a, h));
                }
                err(format!("expected a mutable array, got {}", self.show(v)))
            }
            None => err(format!("expected a mutable array, got {}", self.show(v))),
        }
    }

    pub(crate) fn bytes(&self, v: Value, what: &str) -> Result<Vec<u8>, Error> {
        let a = self.array(v)?;
        if self.heap.kind(a) == Kind::Bytes {
            return Ok(self.heap.packed_bytes(a));
        }
        let mut out = Vec::with_capacity(self.heap.len(a));
        for i in 0..self.heap.len(a) {
            match self.heap.field(a, i) {
                Value::Word(num::Width::U8, b) => out.push(b as u8),
                // A literal in code generic over its integer type -- see
                // `meadow_core::num`.
                Value::Int(n) if (0..=255).contains(&n) => out.push(n as u8),
                other => {
                    return err(format!("`{what}`: not a byte: {}", self.show(other)));
                }
            }
        }
        Ok(out)
    }

    /// Build data whose constructor a primitive names rather than the compiler.
    ///
    /// The tag has to be the one the program's `switch` arms were compiled with,
    /// or a `match` on the result would miss.
    fn data(&mut self, role: Role, fields: &[Value]) -> Value {
        let tag = self.role_tag(role);
        Value::Obj(self.heap.alloc(Kind::Data, tag, fields))
    }

    /// Run `p` on the values in `srcs`, into `dst`.
    ///
    /// Registers rather than values: a primitive that allocates has to re-read
    /// its arguments after making room, because the collector will have moved
    /// them. See the discipline at the top of this file.
    ///
    /// Register *numbers* rather than indices, and that is not incidental:
    /// `[u8; 3]` is passed in a register and `[usize; 3]` is passed on the
    /// stack. This is the innermost call in the machine — every arithmetic
    /// operation the program performs — and widening it cost about 4% across
    /// every benchmark.
    pub(crate) fn run_prim(&mut self, p: Prim, srcs: [u8; 3], dst: u8) -> Result<(), Error> {
        use Prim::*;
        // What each operand is, once: a primitive may read one many times.
        let descs = [self.operand(0), self.operand(1), self.operand(2)];
        let arg = |vm: &Vm, i: usize| {
            if descs[i] == meadow_rt::desc::ANY {
                vm.missing_descriptor(i, srcs[i]);
            }
            Value::from_bits(vm.reg(srcs[i]), descs[i])
        };

        // Typed primitives run as their untyped ones for now; the machine's
        // own instructions for them come with the typed bytecode.
        let p = p.untyped();
        let out = match p {
            // --- numbers ---------------------------------------------------
            //
            // Two `Int`s -- by far the common case -- are handled inline; every
            // other pairing goes through `meadow_core::num`, shared with the
            // other engines.
            Add | Sub | Mul | Div | Mod | Pow => match (arg(self, 0), arg(self, 1)) {
                (Value::Int(x), Value::Int(y)) => Value::Int(match p {
                    Add => x.wrapping_add(y),
                    Sub => x.wrapping_sub(y),
                    Mul => x.wrapping_mul(y),
                    Div if y == 0 => return err("division by zero"),
                    Div => x.wrapping_div(y),
                    Mod if y == 0 => return err("modulo by zero"),
                    Mod => x.wrapping_rem(y),
                    _ => {
                        let e = u32::try_from(y).map_err(|_| Error {
                            msg: format!("`^` exponent must fit in u32, got {y}"),
                        })?;
                        x.wrapping_pow(e)
                    }
                }),
                (a, b) => match p {
                    Add | Sub if let Some(v) = self.big_add_sub(p == Sub, a, b) => v,
                    _ => {
                        let r = num::int_arith(arith(p), self.num(a)?, self.num(b)?);
                        self.from_num(r.map_err(|msg| Error { msg })?)
                    }
                },
            },
            // A fused comparison may not allocate (`Prim::compares`), and none
            // of these does: a `BigInt` is read, not built.
            Lt | Gt | Le | Ge => match (arg(self, 0), arg(self, 1)) {
                (Value::Int(x), Value::Int(y)) => Value::Bool(match p {
                    Lt => x < y,
                    Gt => x > y,
                    Le => x <= y,
                    _ => x >= y,
                }),
                (a, b) => Value::Bool(match self.big_cmp(a, b) {
                    Some(o) => match p {
                        Lt => o.is_lt(),
                        Gt => o.is_gt(),
                        Le => o.is_le(),
                        _ => o.is_ge(),
                    },
                    None => num::int_cmp(cmp(p), self.num(a)?, self.num(b)?)
                        .map_err(|msg| Error { msg })?,
                }),
            },
            Neg => match arg(self, 0) {
                Value::Int(x) => Value::Int(x.wrapping_neg()),
                other => {
                    let r = num::int_neg(self.num(other)?);
                    self.from_num(r.map_err(|msg| Error { msg })?)
                }
            },
            AddF | SubF | MulF | DivF => match (arg(self, 0), arg(self, 1)) {
                (Value::Float(x), Value::Float(y)) => Value::Float(match p {
                    AddF => x + y,
                    SubF => x - y,
                    MulF => x * y,
                    _ => x / y,
                }),
                (a, b) => {
                    let r = num::float_arith(arith(p), self.num(a)?, self.num(b)?);
                    self.from_num(r.map_err(|msg| Error { msg })?)
                }
            },
            LtF | GtF | LeF | GeF => Value::Bool(
                num::float_cmp(cmp(p), self.num(arg(self, 0))?, self.num(arg(self, 1))?)
                    .map_err(|msg| Error { msg })?,
            ),
            ToFloat => {
                Value::Float(num::to_float(self.num(arg(self, 0))?).map_err(|msg| Error { msg })?)
            }
            ToFloat32 => Value::Float32(
                num::to_float32(self.num(arg(self, 0))?).map_err(|msg| Error { msg })?,
            ),
            Floor => Value::Int(num::floor(self.num(arg(self, 0))?).map_err(|msg| Error { msg })?),
            ToBig | ToInt | ToWord(_) => {
                let target = match p {
                    ToBig => num::IntTarget::Big,
                    ToInt => num::IntTarget::Int,
                    ToWord(w) => num::IntTarget::Word(w),
                    _ => unreachable!("matched above"),
                };
                let r = num::to_int(target, self.num(arg(self, 0))?);
                self.from_num(r.map_err(|msg| Error { msg })?)
            }

            // --- structural ----------------------------------------------
            Eq => Value::Bool(self.value_eq(arg(self, 0), arg(self, 1))),
            Ne => Value::Bool(!self.value_eq(arg(self, 0), arg(self, 1))),
            Show => {
                let text = self.show(arg(self, 0));
                self.new_text(text.as_bytes())
            }
            Hash => Value::Int(self.hash_value(arg(self, 0))?),

            // A `String` as its text; everything else the way `show` renders
            // it. See `meadow_eval::displayed` for why.
            Display => {
                let text = self.displayed(arg(self, 0));
                self.new_text(text.as_bytes())
            }

            // --- the builtin Array ----------------------------------------
            // An array of bytes is kept a byte to an element (`Kind::Bytes`),
            // anything else a word; each reads either, and what each builds
            // is kept however its elements say. Room is made *before* anything
            // is copied out: the elements may be addresses, and making room
            // moves what they point at. So each measures, makes room, and
            // only then reads its arguments.
            ArrayLen => Value::Int(self.heap.array_len(self.array(arg(self, 0))?) as i64),
            ArrayGet => {
                let a = self.array(arg(self, 0))?;
                let i = self.index(arg(self, 1))?;
                let n = self.heap.array_len(a);
                if i >= n {
                    return err(format!("arrayGet: index {i} out of bounds (len {n})"));
                }
                self.heap.array_value(a, i)
            }
            ArrayGetOr => {
                let a = self.array(arg(self, 1))?;
                let i = self.index(arg(self, 2))?;
                if i < self.heap.array_len(a) {
                    self.heap.array_value(a, i)
                } else {
                    arg(self, 0)
                }
            }
            ArraySet => {
                let n = self.heap.array_len(self.array(arg(self, 0))?);
                let i = self.index(arg(self, 1))?;
                if i >= n {
                    return err(format!("arraySet: index {i} out of bounds (len {n})"));
                }
                self.ensure(Heap::size_of(Kind::Array, n));
                let a = self.array(arg(self, 0))?;
                let v = arg(self, 2);
                let mut words = self.heap.array_words(a, 0, n);
                words[i] = v.bits();
                Value::Obj(self.heap.alloc_array(&words, v.desc()))
            }
            ArrayPush => {
                let n = self.heap.array_len(self.array(arg(self, 0))?);
                self.ensure(Heap::size_of(Kind::Array, n + 1));
                let a = self.array(arg(self, 0))?;
                let v = arg(self, 1);
                let mut words = self.heap.array_words(a, 0, n);
                words.push(v.bits());
                Value::Obj(self.heap.alloc_array(&words, v.desc()))
            }
            ArrayPop => {
                let n = self.heap.array_len(self.array(arg(self, 0))?);
                if n == 0 {
                    return err("arrayPop: empty array");
                }
                self.ensure(Heap::size_of(Kind::Array, n - 1));
                let a = self.array(arg(self, 0))?;
                let words = self.heap.array_words(a, 0, n - 1);
                Value::Obj(self.heap.alloc_array(&words, self.heap.array_desc(a)))
            }
            ArraySlice => {
                let n = self.heap.array_len(self.array(arg(self, 0))?) as i64;
                let from = self.int(arg(self, 1))?.clamp(0, n) as usize;
                let to = self.int(arg(self, 2))?.clamp(from as i64, n) as usize;
                self.ensure(Heap::size_of(Kind::Array, to - from));
                let a = self.array(arg(self, 0))?;
                let words = self.heap.array_words(a, from, to);
                Value::Obj(self.heap.alloc_array(&words, self.heap.array_desc(a)))
            }
            ArrayConcat => {
                let x = self.heap.array_len(self.array(arg(self, 0))?);
                let y = self.heap.array_len(self.array(arg(self, 1))?);
                if x == 0 {
                    arg(self, 1)
                } else if y == 0 {
                    arg(self, 0)
                } else {
                    self.ensure(Heap::size_of(Kind::Array, x + y));
                    let a = self.array(arg(self, 0))?;
                    let b = self.array(arg(self, 1))?;
                    let mut words = self.heap.array_words(a, 0, x);
                    words.extend(self.heap.array_words(b, 0, y));
                    Value::Obj(self.heap.alloc_array(&words, self.heap.array_desc(a)))
                }
            }

            // --- bitwise --------------------------------------------------
            Shl | Shr | Ushr | BitAnd | BitOr | BitXor => match (arg(self, 0), arg(self, 1)) {
                (Value::Int(x), Value::Int(y)) => Value::Int(match p {
                    Shl => x.wrapping_shl(y as u32),
                    Shr => x.wrapping_shr(y as u32),
                    Ushr => (x as u64).wrapping_shr(y as u32) as i64,
                    BitAnd => x & y,
                    BitOr => x | y,
                    _ => x ^ y,
                }),
                (a, b) => {
                    let r = num::int_bits(bits(p), self.num(a)?, self.num(b)?);
                    self.from_num(r.map_err(|msg| Error { msg })?)
                }
            },
            BitNot => {
                let r = num::int_not(self.num(arg(self, 0))?);
                self.from_num(r.map_err(|msg| Error { msg })?)
            }
            PopCount => {
                Value::Int(num::pop_count(self.num(arg(self, 0))?).map_err(|msg| Error { msg })?)
            }
            BitWidth => {
                Value::Int(num::bit_width(&self.num(arg(self, 0))?).map_err(|msg| Error { msg })?)
            }

            // --- text and bytes -------------------------------------------
            StringToBytes => {
                let bytes = self.text_bytes(arg(self, 0), "stringToBytes")?;
                self.ensure(Heap::packed_slots(bytes.len()) + Heap::size_of(Kind::Array, 0));
                Value::Obj(self.heap.alloc_bytes(&bytes))
            }
            BytesToString => {
                let buf = self.bytes(arg(self, 0), "bytesToString")?;
                let text = String::from_utf8_lossy(&buf).into_owned();
                self.new_text(text.as_bytes())
            }
            BytesToHex => {
                let buf = self.bytes(arg(self, 0), "bytesToHex")?;
                let mut s = String::with_capacity(buf.len() * 2);
                for b in buf {
                    s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
                    s.push(char::from_digit((b & 0xf) as u32, 16).expect("nibble"));
                }
                self.new_text(s.as_bytes())
            }
            BytesFromHex => {
                let bytes = self.text_bytes(arg(self, 0), "bytesFromHex")?;
                let mut out = Vec::with_capacity(bytes.len() / 2);
                let mut ok = bytes.len() % 2 == 0;
                if ok {
                    for pair in bytes.chunks_exact(2) {
                        match (
                            (pair[0] as char).to_digit(16),
                            (pair[1] as char).to_digit(16),
                        ) {
                            (Some(h), Some(l)) => {
                                out.push(Value::Word(num::Width::U8, ((h << 4) | l) as u64))
                            }
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
                if !ok {
                    self.ensure(Heap::size_of(Kind::Data, 0));
                    self.data(Role::None, &[])
                } else {
                    // The array first, then `Just` around it — with room for
                    // both reserved up front, so the array cannot move in
                    // between.
                    self.ensure(
                        Heap::size_of(Kind::Array, out.len()) + Heap::size_of(Kind::Data, 1),
                    );
                    let words: Vec<u64> = out.iter().map(|v| v.bits()).collect();
                    let arr = Value::Obj(self.heap.alloc_array(&words, Heap::BYTE));
                    self.data(Role::Some, &[arr])
                }
            }
            CharCode => match arg(self, 0) {
                Value::Char(c) => Value::Int(c as i64),
                other => {
                    return err(format!(
                        "charCode: expected a Char, got {}",
                        self.show(other)
                    ));
                }
            },
            CharFromCode => match arg(self, 0) {
                Value::Int(n) => match u32::try_from(n).ok().and_then(char::from_u32) {
                    Some(c) => Value::Char(c),
                    None => {
                        return err(format!("charFromCode: {n} is not a Unicode scalar value"));
                    }
                },
                other => {
                    return err(format!(
                        "charFromCode: expected an Int, got {}",
                        self.show(other)
                    ));
                }
            },
            StringToChars => {
                let text = self.text(arg(self, 0), "stringToChars")?;
                let fields: Vec<Value> = text.chars().map(Value::Char).collect();
                self.ensure(Heap::size_of(Kind::Array, fields.len()));
                Value::Obj(self.heap.alloc(Kind::Array, 0, &fields))
            }
            CharsToString => {
                let a = match arg(self, 0)
                    .addr()
                    .filter(|a| self.heap.kind(*a) == Kind::Array)
                {
                    Some(a) => a,
                    None => {
                        return err(format!(
                            "charsToString: expected an Array, got {}",
                            self.show(arg(self, 0))
                        ));
                    }
                };
                let mut s = String::with_capacity(self.heap.len(a));
                for i in 0..self.heap.len(a) {
                    match self.heap.field(a, i) {
                        Value::Char(c) => s.push(c),
                        other => {
                            return err(format!(
                                "charsToString: expected a Char, got {}",
                                self.show(other)
                            ));
                        }
                    }
                }
                self.new_text(s.as_bytes())
            }
            ConcatStrings => {
                let a = match arg(self, 0)
                    .addr()
                    .filter(|a| self.heap.kind(*a) == Kind::Array)
                {
                    Some(a) => a,
                    None => {
                        return err(format!(
                            "concatStrings: expected an Array, got {}",
                            self.show(arg(self, 0))
                        ));
                    }
                };
                let mut out = Vec::new();
                for i in 0..self.heap.len(a) {
                    let part = self.heap.field(a, i);
                    match (part, self.text_at(part)) {
                        (_, Some(p)) => out.extend(self.heap.packed_bytes(p)),
                        (Value::Str(sym), None) => out.extend_from_slice(sym.as_bytes()),
                        (other, None) => {
                            return err(format!(
                                "concatStrings: expected a String, got {}",
                                self.show(other)
                            ));
                        }
                    }
                }
                // Every part is whole UTF-8, so what they make is too.
                self.new_text(&out)
            }
            StringByteLength => {
                let a = self.text_addr(arg(self, 0), "stringByteLength")?;
                Value::Int(self.heap.packed_len(a) as i64)
            }
            StringByteAt => {
                let a = self.text_addr(arg(self, 0), "stringByteAt")?;
                let i = self.int(arg(self, 1))?;
                let len = self.heap.packed_len(a);
                match usize::try_from(i).ok().filter(|at| *at < len) {
                    Some(at) => Value::Word(num::Width::U8, self.heap.packed_byte(a, at) as u64),
                    None => {
                        return err(format!("stringByteAt: index {i} out of bounds (len {len})"));
                    }
                }
            }
            StringSlice => {
                let a = self.text_addr(arg(self, 0), "stringSlice")?;
                let (from, to) = (self.int(arg(self, 1))?, self.int(arg(self, 2))?);
                let (lo, hi) = meadow_rt::text::clamp(self.heap.packed_len(a), from, to);
                // Copied out before making room, which moves the string.
                let bytes = self.heap.packed_bytes_in(a, lo, hi);
                match std::str::from_utf8(&bytes) {
                    Ok(_) => self.new_text(&bytes),
                    Err(_) => {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        self.new_text(text.as_bytes())
                    }
                }
            }
            StringCompare => {
                let a = self.text_addr(arg(self, 0), "stringCompare")?;
                let b = self.text_addr(arg(self, 1), "stringCompare")?;
                Value::Int(self.heap.packed_compare(a, b))
            }
            StringIndexOf => {
                let hay = self.text_addr(arg(self, 0), "stringIndexOf")?;
                let needle = self.text_bytes(arg(self, 1), "stringIndexOf")?;
                let from = self.int(arg(self, 2))?;
                Value::Int(self.heap.packed_find(hay, &needle, from))
            }

            // --- the mutable cell -----------------------------------------
            //
            // The only place a value a program can still see is changed in
            // place. Everything else here builds something new.
            NewRef => {
                self.ensure(Heap::size_of(Kind::Ref, 1));
                let v = arg(self, 0);
                Value::Obj(self.heap.alloc(Kind::Ref, 0, &[v]))
            }
            GetRef => match arg(self, 0)
                .addr()
                .filter(|a| self.heap.kind(*a) == Kind::Ref)
            {
                Some(a) => self.heap.field(a, 0),
                None => {
                    return err(format!(
                        "getRef: expected a Ref, got {}",
                        self.show(arg(self, 0))
                    ));
                }
            },
            SetRef => match arg(self, 0)
                .addr()
                .filter(|a| self.heap.kind(*a) == Kind::Ref)
            {
                Some(a) => {
                    let v = arg(self, 1);
                    self.heap.set_field(a, 0, v);
                    Value::Unit
                }
                None => {
                    return err(format!(
                        "setRef: expected a Ref, got {}",
                        self.show(arg(self, 0))
                    ));
                }
            },

            // --- the mutable array ----------------------------------------
            //
            // Written in place, like a `Ref`. `runSt` never gets here: lowering
            // applies its body.
            RunSt => return err("runSt reached the VM; lowering applies its body"),
            StNewArray => {
                let n = match self.int(arg(self, 0))? {
                    n if n >= 0 => n as usize,
                    n => {
                        return err(format!(
                            "stNewArray: expected a length of zero or more, got {n}"
                        ));
                    }
                };
                self.ensure(Heap::size_of(Kind::MutArray, n));
                let fill = vec![arg(self, 1); n];
                Value::Obj(self.heap.alloc(Kind::MutArray, 0, &fill))
            }
            // These two read the header *once*. Reading it is two slot reads,
            // and a slot read of an old-generation object is a block lookup --
            // so asking for the kind, then the length, then the field, each
            // through an accessor that reads the header again, cost six block
            // lookups per element of a promoted array. A matrix multiply is
            // nothing but this.
            StGetArray => {
                let (a, h) = self.mut_array_head(arg(self, 0))?;
                let i = self.index(arg(self, 1))?;
                let n = h.len as usize;
                if i >= n {
                    return err(format!("stGetArray: index {i} out of bounds (len {n})"));
                }
                self.heap.field_of(a, &h, i)
            }
            StSetArray => {
                let (a, h) = self.mut_array_head(arg(self, 0))?;
                let i = self.index(arg(self, 1))?;
                let n = h.len as usize;
                if i >= n {
                    return err(format!("stSetArray: index {i} out of bounds (len {n})"));
                }
                let v = arg(self, 2);
                self.heap.set_field_of(a, &h, i, v);
                Value::Unit
            }
            StArrayLen => Value::Int(self.mut_array_head(arg(self, 0))?.1.len as i64),
            StFreeze => {
                let n = self.heap.len(self.mut_array(arg(self, 0))?);
                self.ensure(Heap::size_of(Kind::Array, n));
                // Re-read: making room may have moved it.
                let a = self.mut_array(arg(self, 0))?;
                let words: Vec<u64> = self.heap.words(a).collect();
                Value::Obj(self.heap.alloc_array(&words, self.heap.element_desc(a)))
            }
            StThaw => {
                let n = self.heap.array_len(self.array(arg(self, 0))?);
                self.ensure(Heap::size_of(Kind::MutArray, n));
                let a = self.array(arg(self, 0))?;
                let words = self.heap.array_words(a, 0, n);
                self.array_of(Kind::MutArray, &words, self.heap.array_desc(a))
            }

            // --- compact regions --------------------------------------------
            //
            // Copying into a region allocates nothing in the heap, so the
            // one `ensure` is for the handle, made before the copy and taken
            // after it -- nothing can move in between.
            Compact => {
                self.ensure(Heap::size_of(Kind::Compact, 1));
                let region = self.heap.new_region();
                match self.heap.compact_into(region, arg(self, 0)) {
                    Ok(root) => Value::Obj(self.heap.alloc(Kind::Compact, region, &[root])),
                    Err(why) => {
                        self.heap.free_region(region);
                        return err(meadow_rt::compact::uncompactable(why.describe()));
                    }
                }
            }
            GetCompact => self.heap.field(self.compact_handle(arg(self, 0))?, 0),
            CompactAdd => {
                self.ensure(Heap::size_of(Kind::Compact, 1));
                let region = self.heap.meta(self.compact_handle(arg(self, 0))?);
                match self.heap.compact_into(region, arg(self, 1)) {
                    Ok(root) => Value::Obj(self.heap.alloc(Kind::Compact, region, &[root])),
                    Err(why) => return err(meadow_rt::compact::uncompactable(why.describe())),
                }
            }
            CompactSize => {
                let region = self.heap.meta(self.compact_handle(arg(self, 0))?);
                Value::Int((self.heap.region_used(region) * crate::heap::SLOT_BYTES) as i64)
            }

            // --- resumptions --------------------------------------------------
            //
            // The flag is its own kind, not a `Ref`, so a resumption refused
            // passage to another thread or into a region is refused as the
            // continuation it is.
            Once => {
                self.ensure(Heap::size_of(Kind::Resume, 1));
                Value::Obj(self.heap.alloc(Kind::Resume, 0, &[Value::Bool(false)]))
            }
            // --- the frame stack ------------------------------------------
            //
            // See `Heap`'s frame stack. `Enter` at a `handle`, `Detach` where a
            // general clause is entered, `Reattach` where it resumes.
            Enter => {
                let target = arg(self, 0)
                    .addr()
                    .filter(|a| self.heap.kind(*a) == Kind::Ref)
                    .ok_or_else(|| Error {
                        msg: "entering a handler without its target".into(),
                    })?;
                let tag = self.heap.enter_frames();
                self.heap.set_meta(target, tag);
                Value::Unit
            }
            Detach => {
                let target = arg(self, 0)
                    .addr()
                    .filter(|a| self.heap.kind(*a) == Kind::Ref)
                    .ok_or_else(|| Error {
                        msg: "detaching from something that is not a handler".into(),
                    })?;
                let tag = self.heap.meta(target);
                let Some((top, fsp)) = self.heap.detach(tag) else {
                    return err(
                        "an effect was performed after its handler had finished: the \
                         function performing it escaped the `handle` that answers it",
                    );
                };
                self.ensure(Heap::size_of(Kind::Stack, 2));
                let obj = self.heap.alloc(
                    Kind::Stack,
                    0,
                    &[Value::Int(top as i64), Value::Int(fsp as i64)],
                );
                self.heap.name_segment(top, obj);
                Value::Obj(obj)
            }
            Reattach => {
                let seg = arg(self, 0)
                    .addr()
                    .filter(|a| self.heap.kind(*a) == Kind::Stack)
                    .ok_or_else(|| Error {
                        msg: "resumed something that is not a stack segment".into(),
                    })?;
                let (Value::Int(top), Value::Int(fsp)) =
                    (self.heap.field(seg, 0), self.heap.field(seg, 1))
                else {
                    return err("a stack segment without its bounds");
                };
                if !self.heap.reattach(top as Addr, fsp as Addr) {
                    return err("continuation resumed more than once");
                }
                Value::Unit
            }
            TakeOnce => match arg(self, 0)
                .addr()
                .filter(|a| self.heap.kind(*a) == Kind::Resume)
            {
                Some(a) if self.heap.field(a, 0) == Value::Bool(false) => {
                    self.heap.set_field(a, 0, Value::Bool(true));
                    Value::Bool(true)
                }
                Some(_) => Value::Bool(false),
                None => return err("takeOnce: expected a resumption's flag"),
            },

            IntAdd | IntSub | IntMul | IntDiv | IntMod | IntEq | IntNe | IntLt | IntLe | IntGt
            | IntGe | FloatAdd | FloatSub | FloatMul | FloatDiv | FloatEq | FloatNe | FloatLt
            | FloatLe | FloatGt | FloatGe => unreachable!("made untyped above"),

            // --- top-level values, evaluated once per thread -------------------
            GlobalReady => {
                let i = self.index(arg(self, 0))?;
                Value::Bool(matches!(self.globals.get(i), Some(Some(_))))
            }
            GlobalGet => match self.globals.get(self.index(arg(self, 0))?) {
                Some(Some(v)) => *v,
                _ => return err("a definition read before it was evaluated"),
            },
            GlobalSet => {
                let i = self.index(arg(self, 0))?;
                if self.globals.len() <= i {
                    self.globals.resize(i + 1, None);
                }
                self.globals[i] = Some(arg(self, 1));
                Value::Unit
            }

            // --- tail recursion modulo cons ------------------------------------
            //
            // The cell was made with a placeholder in field `i` a moment ago, and
            // nothing but this chain of calls has seen it. It may have been
            // promoted since, which is what `set_field`'s barriers are for.
            SetField => {
                let i = self.index(arg(self, 1))?;
                match arg(self, 0).addr() {
                    Some(a) if self.heap.kind(a) == Kind::Data && i < self.heap.len(a) => {
                        self.heap.set_field(a, i, arg(self, 2));
                        Value::Unit
                    }
                    _ => {
                        return err(format!(
                            "setField: expected a constructor with a field {i}, got {}",
                            self.show(arg(self, 0))
                        ));
                    }
                }
            }

            // --- software transactional memory --------------------------------
            StmNew => {
                self.world()?;
                let shared = self.share(arg(self, 0), None)?;
                let id = self.world()?.new_tvar(shared);
                self.ensure(Heap::size_of(Kind::TVar, 0));
                Value::Obj(self.heap.alloc(Kind::TVar, id, &[]))
            }
            StmRead => {
                let id = self.handle(arg(self, 0), Kind::TVar, "a TVar")?;
                let read = match (&self.world, &mut self.txn) {
                    (None, _) => return err(NO_WORLD),
                    (Some(world), Some(txn)) if txn.live => world.read(txn, id),
                    _ => return err(meadow_rt::stm::outside("readTVar")),
                };
                match read {
                    crate::stm::Read::Conflict => {
                        self.ensure(Heap::size_of(Kind::Data, 0));
                        let tag = self.role_tag(Role::None);
                        Value::Obj(self.heap.alloc(Kind::Data, tag, &[]))
                    }
                    crate::stm::Read::Value(shared) => {
                        // Room first: the region is adopted after, so the
                        // collection that making room may run cannot let it go.
                        self.ensure(Heap::size_of(Kind::Data, 1));
                        if let Some(r) = &shared.region {
                            self.heap.adopt(r);
                        }
                        let tag = self.role_tag(Role::Some);
                        Value::Obj(self.heap.alloc(Kind::Data, tag, &[shared.root]))
                    }
                }
            }
            StmWrite => {
                let id = self.handle(arg(self, 0), Kind::TVar, "a TVar")?;
                // An immediate goes in no region, so there is none to find --
                // and finding one locks the cell, and makes a region when the
                // value there now is an immediate too.
                let into = match (arg(self, 1), &self.world, &self.txn) {
                    (_, None, _) => return err(NO_WORLD),
                    (Value::Obj(_), Some(world), Some(txn)) if txn.live => {
                        Some(world.region_for_write(txn, id))
                    }
                    (Value::Obj(_), ..) => {
                        return err(meadow_rt::stm::outside("writeTVar"));
                    }
                    _ => None,
                };
                let shared = self.share(arg(self, 1), into)?;
                self.txn("writeTVar")?.write(id, shared);
                Value::Unit
            }
            StmBegin => {
                let last = self.txn.take();
                self.txn = Some(self.world()?.begin(last));
                Value::Unit
            }
            StmNest => {
                self.txn("orElse")?.writes.push(Vec::new());
                Value::Unit
            }
            StmMerge => {
                self.txn("orElse")?.merge();
                Value::Unit
            }
            StmRollback => {
                self.txn("orElse")?.rollback();
                Value::Unit
            }
            StmCommit => {
                let went = match (&self.world, &mut self.txn) {
                    (None, _) => return err(NO_WORLD),
                    (Some(world), Some(txn)) if txn.live => world.commit(txn),
                    _ => return err(meadow_rt::stm::outside("atomically")),
                };
                match went {
                    crate::stm::Commit::Conflict => Value::Bool(false),
                    // Nobody to wake: done, without leaving the machine.
                    crate::stm::Commit::Done => Value::Bool(true),
                    crate::stm::Commit::Wake(written) => {
                        self.request = Some(crate::vm::Request::StmWake { written, dst });
                        Value::Unit
                    }
                }
            }
            StmWait => {
                self.txn("retry")?;
                self.request = Some(crate::vm::Request::StmWait { dst });
                Value::Unit
            }

            // --- green threads ------------------------------------------------
            //
            // None of these acts here. Each leaves a request for the scheduler,
            // which owns the channels and the other threads, and puts a
            // placeholder in `dst` that the answer overwrites. Anything that
            // crosses to another thread is exported first, while this thread
            // still has it -- see `meadow_core::thread`.
            ThreadSpawn | ThreadAwait | ThreadYield | ChannelNew | ChannelSend | ChannelReceive
            | ThreadCancel | ThreadCancelled
                if !self.scheduled =>
            {
                return err("green threads need the scheduler; this machine is running on its own");
            }
            ThreadSpawn => {
                let body = self.sendable(arg(self, 0))?;
                let answer = self.operand(1);
                self.request = Some(crate::vm::Request::Spawn { body, answer, dst });
                Value::Unit
            }
            ThreadAwait => {
                let task = self.handle(arg(self, 0), Kind::Task, "a thread")?;
                self.request = Some(crate::vm::Request::Await { task, dst });
                Value::Unit
            }
            ThreadYield => {
                self.request = Some(crate::vm::Request::Yield);
                Value::Unit
            }
            ChannelNew => {
                self.request = Some(crate::vm::Request::NewChannel { dst });
                Value::Unit
            }
            ChannelSend => {
                let channel = self.handle(arg(self, 0), Kind::Channel, "a channel")?;
                let message = self.sendable(arg(self, 1))?;
                self.request = Some(crate::vm::Request::Send { channel, message });
                Value::Unit
            }
            ChannelReceive => {
                let channel = self.handle(arg(self, 0), Kind::Channel, "a channel")?;
                self.request = Some(crate::vm::Request::Receive { channel, dst });
                Value::Unit
            }
            ThreadCancel => {
                let task = self.handle(arg(self, 0), Kind::Task, "a thread")?;
                self.request = Some(crate::vm::Request::Cancel { task });
                Value::Unit
            }
            ThreadCancelled => {
                self.request = Some(crate::vm::Request::Cancelled { dst });
                Value::Unit
            }
        };

        self.set(dst, out);
        Ok(())
    }
}

/// Arithmetic on `BigInt` magnitudes as the heap holds them: 64-bit limbs,
/// least significant first, no zero limb at the top, and the sign kept apart.
pub(crate) mod big {
    use std::cmp::Ordering;

    /// A `BigInt`'s `meta`: its sign.
    pub const ZERO: u32 = 0;
    pub const PLUS: u32 = 1;
    pub const MINUS: u32 = 2;

    /// `sa·a + sb·b`, as a sign (-1, 0 or 1) and a normalized magnitude.
    pub fn add(sa: i8, a: &[u64], sb: i8, b: &[u64]) -> (i8, Vec<u64>) {
        if sb == 0 {
            return (sa, a.to_vec());
        }
        if sa == 0 {
            return (sb, b.to_vec());
        }
        if sa == sb {
            return (sa, add_mag(a, b));
        }
        match cmp_mag(a, b) {
            Ordering::Equal => (0, Vec::new()),
            Ordering::Greater => (sa, sub_mag(a, b)),
            Ordering::Less => (sb, sub_mag(b, a)),
        }
    }

    /// `sa·a` against `sb·b`.
    pub fn cmp(sa: i8, a: &[u64], sb: i8, b: &[u64]) -> Ordering {
        match sa.cmp(&sb) {
            Ordering::Equal if sa < 0 => cmp_mag(b, a),
            Ordering::Equal => cmp_mag(a, b),
            other => other,
        }
    }

    /// Magnitudes compared. Normalized, so the longer one is the larger.
    pub fn cmp_mag(a: &[u64], b: &[u64]) -> Ordering {
        a.len()
            .cmp(&b.len())
            .then_with(|| a.iter().rev().cmp(b.iter().rev()))
    }

    pub fn add_mag(a: &[u64], b: &[u64]) -> Vec<u64> {
        let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
        let mut out = Vec::with_capacity(long.len() + 1);
        let mut carry = false;
        for (i, &x) in long.iter().enumerate() {
            let y = short.get(i).copied().unwrap_or(0);
            let (s, c1) = x.overflowing_add(y);
            let (s, c2) = s.overflowing_add(carry as u64);
            out.push(s);
            carry = c1 | c2;
        }
        if carry {
            out.push(1);
        }
        out
    }

    /// `a - b`, for `a >= b`.
    pub fn sub_mag(a: &[u64], b: &[u64]) -> Vec<u64> {
        debug_assert!(cmp_mag(a, b) != Ordering::Less);
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = false;
        for (i, &x) in a.iter().enumerate() {
            let y = b.get(i).copied().unwrap_or(0);
            let (d, b1) = x.overflowing_sub(y);
            let (d, b2) = d.overflowing_sub(borrow as u64);
            out.push(d);
            borrow = b1 | b2;
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use num_bigint::BigInt;
        use num_traits::Zero;

        fn as_limbs(n: &BigInt) -> (i8, Vec<u64>) {
            let s = if n.is_zero() {
                0
            } else if n.sign() == num_bigint::Sign::Minus {
                -1
            } else {
                1
            };
            (s, n.magnitude().to_u64_digits())
        }

        fn from_limbs(s: i8, l: &[u64]) -> BigInt {
            let bytes: Vec<u8> = l.iter().flat_map(|w| w.to_le_bytes()).collect();
            let m = BigInt::from(num_bigint::BigUint::from_bytes_le(&bytes));
            if s < 0 { -m } else { m }
        }

        /// Every sign combination, carries across limbs, borrows that empty
        /// the top limbs, and cancellation to zero -- against num-bigint, which
        /// is what the general path uses and what these have to agree with.
        #[test]
        fn limb_arithmetic_agrees_with_num_bigint() {
            let two64 = BigInt::from(1u128 << 64);
            let samples: Vec<BigInt> = vec![
                BigInt::zero(),
                BigInt::from(1),
                BigInt::from(-1),
                BigInt::from(u64::MAX),
                -BigInt::from(u64::MAX),
                two64.clone(),
                -two64.clone(),
                &two64 * &two64 - 1,
                -(&two64 * &two64) + 1,
                BigInt::from(i64::MIN),
                BigInt::parse_bytes(b"123456789012345678901234567890123456789", 10).unwrap(),
                -BigInt::parse_bytes(b"123456789012345678901234567890123456788", 10).unwrap(),
            ];
            for x in &samples {
                for y in &samples {
                    let (sx, lx) = as_limbs(x);
                    let (sy, ly) = as_limbs(y);
                    let (s, l) = add(sx, &lx, sy, &ly);
                    assert_eq!(from_limbs(s, &l), x + y, "{x} + {y}");
                    assert!(l.last() != Some(&0), "{x} + {y} left a zero top limb");
                    assert_eq!(
                        s == 0,
                        l.is_empty(),
                        "{x} + {y}: sign and magnitude disagree"
                    );
                    let (s, l) = add(sx, &lx, -sy, &ly);
                    assert_eq!(from_limbs(s, &l), x - y, "{x} - {y}");
                    assert_eq!(cmp(sx, &lx, sy, &ly), x.cmp(y), "{x} <=> {y}");
                }
            }
        }
    }
}
