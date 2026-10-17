//! A program hosted by another: the effects its host answers.
//!
//! A Rust program that embeds Meadow -- a GUI running a script, say -- gives
//! the script its own effect to reach it by. The script declares it as it
//! would any effect,
//!
//! ```text
//! effect Neo {
//!   present : Widget -> (),
//!   next    : () -> Event,
//! }
//! ```
//!
//! and installs no handler, so each operation arrives where `Fs` and
//! `Console` do: at the machine, as an effect nothing in the program answers.
//! [`Vm::host`] is who the machine asks before it gives up on one.
//!
//! # What crosses
//!
//! Nothing with an address in it, in either direction, for the reason
//! [`crate::native`] gives: a copying collector moves what an address points
//! at. The argument reaches the host as an [`Owned`], a copy that is plain
//! Rust data; the answer comes back as a [`Build`], which the machine
//! materialises with room made once. A host never sees a [`Value`].
//!
//! # What a host's operation can do
//!
//! Answer once, with a value: it is called where the operation was performed
//! and the program goes on with what it answers. It has no continuation to
//! keep or call twice. A host that has to wait -- for the user's next click
//! -- waits inside the call, on a thread of its own; the machine is single
//! threaded and simply does not go on until it is answered.
//!
//! A hosted machine is run with [`Vm::run`], with no scheduler behind it, so
//! a program that spawns a thread or starts a transaction is refused with the
//! error those give any machine that has none.

use crate::heap::Kind;
use crate::native::Build;
use crate::value::Value;
use crate::vm::Vm;

/// A value copied out of the machine: plain data, with no address in it.
#[derive(Debug, Clone, PartialEq)]
pub enum Owned {
    Unit,
    Bool(bool),
    Int(i64),
    /// A sized word, `UInt8` to `Int64`, as the number it is.
    Word(i128),
    Float(f64),
    Char(char),
    Str(String),
    /// A constructor and its fields. The name is the one a reader sees --
    /// `Just`, not `Maybe.Just` -- since a host matches on what the script's
    /// author wrote.
    Data(String, Vec<Owned>),
    Tuple(Vec<Owned>),
    /// A `Vector` or a `List`: the sequence either denotes.
    List(Vec<Owned>),
    Array(Vec<Owned>),
    /// A record's fields, sorted by label as the machine keeps them.
    Record(Vec<(String, Owned)>),
    /// Something that cannot leave the machine -- a closure, a `Ref`, a
    /// channel -- named for the host to say what it was given.
    Opaque(&'static str),
}

impl Owned {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Owned::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Owned::Int(n) => Some(*n),
            Owned::Word(n) => i64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Owned::Float(x) => Some(*x),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Owned::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The items of a `Vector`, a `List` or an `Array`.
    pub fn items(&self) -> Option<&[Owned]> {
        match self {
            Owned::List(xs) | Owned::Array(xs) | Owned::Tuple(xs) => Some(xs),
            _ => None,
        }
    }

    /// A record's field, or a constructor's when it was declared with names
    /// -- which arrive in declaration order, so by position there.
    pub fn field(&self, label: &str) -> Option<&Owned> {
        match self {
            Owned::Record(fields) => fields.iter().find(|(l, _)| l == label).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// What a host answers an operation with: `Ok(None)` for one that is not
/// its own, which the machine then reports as unhandled; `Err` for one it
/// refuses, which stops the program with that message.
pub type Answer = Result<Option<Build>, String>;

/// Who answers the operations no handler in the program does, after the
/// runtime's own: the effect's name as the program spells it, the
/// operation's, and its argument.
pub type Host = Box<dyn FnMut(&str, &str, Owned) -> Answer + Send>;

/// How deep [`Vm::owned`] follows a value before it stops: the bound
/// printing has, for the same reason. Sequences are flattened and do not
/// count against it.
const MAX_DEPTH: usize = 1000;

impl Vm<'_> {
    /// `v`, copied out of the machine.
    pub fn owned(&self, v: Value) -> Owned {
        self.owned_at(v, 0)
    }

    fn owned_at(&self, v: Value, depth: usize) -> Owned {
        if depth > MAX_DEPTH {
            return Owned::Opaque("a value nested too deep to copy");
        }
        let all = |vs: Vec<Value>| {
            vs.into_iter()
                .map(|x| self.owned_at(x, depth + 1))
                .collect()
        };
        let a = match v {
            Value::Unit => return Owned::Unit,
            Value::Bool(b) => return Owned::Bool(b),
            Value::Int(n) => return Owned::Int(n),
            Value::Word(w, b) => return Owned::Word(w.value(b)),
            Value::Float(x) => return Owned::Float(x),
            Value::Float32(x) => return Owned::Float(f64::from(x)),
            Value::Char(c) => return Owned::Char(c),
            Value::Str(s) => return Owned::Str(s.to_string()),
            Value::Obj(a) => a,
        };
        match self.heap.kind(a) {
            Kind::Str => {
                Owned::Str(String::from_utf8_lossy(&self.heap.packed_bytes(a)).into_owned())
            }
            Kind::Array | Kind::Bytes => Owned::Array(all(self.heap.array_values(a))),
            Kind::Record => Owned::Record(
                (0..self.heap.len(a) / 2)
                    .map(|j| {
                        let label = match self.heap.field(a, 2 * j) {
                            Value::Str(l) => l.to_string(),
                            other => self.show(other),
                        };
                        (
                            label,
                            self.owned_at(self.heap.field(a, 2 * j + 1), depth + 1),
                        )
                    })
                    .collect(),
            ),
            Kind::Data => {
                if let Some(xs) = self.vector_elems(v).or_else(|| self.list_items(v)) {
                    return Owned::List(all(xs));
                }
                let name = self.program.ctor(self.heap.meta(a));
                let fields = all(self.heap.fields(a));
                match name {
                    Some(n) if &*n == meadow_rt::roles::TUPLE => Owned::Tuple(fields),
                    Some(n) => Owned::Data(
                        n.rsplit_once('.').map_or(&*n, |(_, c)| c).to_owned(),
                        fields,
                    ),
                    None => Owned::Opaque("a constructor the program does not name"),
                }
            }
            Kind::BigInt => match self.bigint_at(v) {
                Some(n) => i128::try_from(&n).map_or(Owned::Str(n.to_string()), Owned::Word),
                None => Owned::Opaque("a big integer"),
            },
            Kind::Closure => Owned::Opaque("a function"),
            Kind::Ref => Owned::Opaque("a Ref"),
            Kind::MutArray => Owned::Opaque("a mutable array"),
            Kind::Channel => Owned::Opaque("a channel"),
            Kind::Task => Owned::Opaque("a thread"),
            Kind::TVar => Owned::Opaque("a TVar"),
            Kind::Compact => self.owned_at(self.heap.field(a, 0), depth + 1),
            _ => Owned::Opaque("a continuation"),
        }
    }

    /// Ask the host about `effect.op`, if there is one: `Ok(None)` where
    /// there is none or it is not the host's operation.
    pub(crate) fn ask_host(
        &mut self,
        effect: &str,
        op: &str,
        arg: Value,
    ) -> Result<Option<Value>, crate::Error> {
        // Taken for the call, since the host is called with the machine
        // borrowed to copy the argument out and build the answer.
        let Some(mut host) = self.host.take() else {
            return Ok(None);
        };
        let arg = self.owned(arg);
        let answer = host(meadow_rt::spelling(effect), op, arg);
        self.host = Some(host);
        match answer {
            Ok(Some(b)) => match self.missing_ctor(&b) {
                Some(name) => crate::vm::err(format!(
                    "{}.{op}: the host answered with {name}, and this program has no one constructor called that",
                    meadow_rt::spelling(effect)
                )),
                None => Ok(Some(self.build(b))),
            },
            Ok(None) => Ok(None),
            Err(why) => crate::vm::err(why),
        }
    }

    /// A constructor `b` names that the program does not have: built anyway
    /// it would be a value no `match` in the program has an arm for.
    fn missing_ctor(&self, b: &Build) -> Option<String> {
        match b {
            Build::Ctor(name, xs) => {
                if self.host_ctor(name).is_none() {
                    return Some(name.clone());
                }
                xs.iter().find_map(|x| self.missing_ctor(x))
            }
            Build::Data(_, xs) | Build::Tuple(xs) | Build::Vector(xs) => {
                xs.iter().find_map(|x| self.missing_ctor(x))
            }
            Build::At(_) | Build::Str(_) | Build::Bytes(_) => None,
        }
    }

    /// The tag of the constructor a host calls `name`: as much of its
    /// name as says which it is. A constructor's whole name has its package
    /// and type before it, `app.Event.Clicked`; a host may say that,
    /// `Event.Clicked`, or `Clicked` alone where the program has exactly one
    /// constructor so called.
    pub(crate) fn host_ctor(&self, name: &str) -> Option<u32> {
        let ctors = &self.program.ctors;
        if let Some(i) = ctors.iter().position(|c| &**c == name) {
            return Some(i as u32);
        }
        let mut ending = ctors.iter().enumerate().filter(|(_, c)| {
            c.strip_suffix(name)
                .is_some_and(|before| before.ends_with('.'))
        });
        match (ending.next(), ending.next()) {
            (Some((i, _)), None) => Some(i as u32),
            _ => None,
        }
    }
}
