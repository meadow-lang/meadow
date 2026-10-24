//! What an *unhandled* effect operation means, on this machine.
//!
//! `Std.Fs`, `Std.Process`, `Std.Random` and `Std.Time` each pair an effect
//! declaration with a handler that fakes it. Reaching one with no handler
//! installed is a request for the real thing. What each operation does is
//! `meadow_rt::world`'s, the same on every runtime; this is the machine's side
//! of it -- how its values are read, and how an answer is made on its heap
//! ([`World`] for [`Vm`]).
//!
//! `Std.Test.fail` is the other unhandled operation with a meaning, and it lives
//! in the VM proper because it is a failure rather than a value.
//!
//! # Building results
//!
//! A native answer is a tree -- `Ok(Just(#[1, 2, 3]))`, a record of file
//! metadata, a list of directory names -- and building it one object at a time
//! would mean holding an address across an allocation, which a copying
//! collector makes wrong. So an operation describes what it wants as a
//! [`Build`], an ordinary Rust value with no addresses in it. [`Vm::build`]
//! then measures the whole tree, makes room once, and materialises it bottom-up
//! with no collection possible in between.

use crate::heap::Kind;
use crate::value::Value;
use crate::vm::{Error, Vm, err};
use meadow_rt::roles::{Role, TUPLE};
use meadow_rt::world::{Imm, Job, World};

/// A result to build, described before any of it exists: see
/// `meadow_rt::world::Build`.
///
/// It contains no [`Value::Obj`]: nothing in it can be invalidated by the
/// collection that making room for it may trigger. Immediates
/// ([`Build::At`]) are fine, since they are not addresses.
pub type Build = meadow_rt::world::Build<Value>;

impl Imm for Value {
    fn unit() -> Self {
        Value::Unit
    }
    fn int(n: i64) -> Self {
        Value::Int(n)
    }
    fn float(x: f64) -> Self {
        Value::Float(x)
    }
    fn boolean(b: bool) -> Self {
        Value::Bool(b)
    }
}

/// How `Vector.fromArray` lays out `n` elements: nothing, one chunk, or a
/// radix-balanced tree of chunks of [`VECTOR_WIDTH`] under a `Full` with empty
/// side buffers. Returns the shift and, level by level from the leaves up, how
/// many nodes each level has.
///
/// This is `vBuildTree` in `Std.Collections.Vector`, and has to stay it: a
/// vector built here is taken apart by that module's code.
pub(crate) fn vector_shape(n: usize) -> (i64, Vec<usize>) {
    let mut levels = vec![n.div_ceil(VECTOR_WIDTH)];
    let mut shift = 5;
    while *levels.last().expect("never empty") > VECTOR_WIDTH {
        levels.push(levels.last().expect("never empty").div_ceil(VECTOR_WIDTH));
        shift += 5;
    }
    (shift, levels)
}

/// `vWidth` in `Std.Collections.Vector`.
pub(crate) const VECTOR_WIDTH: usize = 32;

/// How many heap slots the whole tree needs.
fn slots(b: &Build) -> usize {
    let size = crate::heap::Heap::size_of;
    match b {
        Build::At(_) => 0,
        Build::Str(s) => crate::heap::Heap::packed_slots(s.len()),
        Build::Data(_, xs) | Build::Tuple(xs) | Build::Ctor(_, xs) => {
            size(Kind::Data, xs.len()) + xs.iter().map(slots).sum::<usize>()
        }
        Build::Bytes(b) => crate::heap::Heap::packed_slots(b.len()) + size(Kind::Array, 0),
        Build::Vector(xs) => {
            let inner = xs.iter().map(slots).sum::<usize>();
            if xs.len() <= VECTOR_WIDTH {
                // `Single` and its array, or a bare `Empty`.
                return size(Kind::Data, 1) + size(Kind::Array, xs.len()) + inner;
            }
            let (_, levels) = vector_shape(xs.len());
            // `Full` and its four empty buffers; every leaf a `Leaf` and an
            // array of elements; every branch a `Branch`, a `None` and an
            // array of children.
            let leaves = levels[0];
            let branches: usize = levels[1..].iter().sum::<usize>() + 1;
            let children: usize = levels.iter().sum::<usize>();
            size(Kind::Data, 7)
                + 4 * size(Kind::Array, 0)
                + leaves * (size(Kind::Data, 1) + size(Kind::Array, 0))
                + xs.len()
                + branches * (size(Kind::Data, 2) + size(Kind::Data, 0) + size(Kind::Array, 0))
                + children
                + inner
        }
    }
}

impl Vm<'_> {
    /// Materialise a [`Build`]. Makes room once, then allocates with no
    /// collection possible in between — which is what lets it hold addresses.
    pub(crate) fn build(&mut self, b: Build) -> Value {
        self.ensure(slots(&b));
        self.build_here(b)
    }

    fn build_here(&mut self, b: Build) -> Value {
        match b {
            Build::At(v) => v,
            Build::Str(s) => Value::Obj(self.heap.alloc_str(s.as_bytes())),
            Build::Data(role, xs) => {
                let fields: Vec<Value> = xs.into_iter().map(|x| self.build_here(x)).collect();
                let tag = self.role_tag(role);
                Value::Obj(self.heap.alloc(Kind::Data, tag, &fields))
            }
            Build::Tuple(xs) => {
                let fields: Vec<Value> = xs.into_iter().map(|x| self.build_here(x)).collect();
                let tag = self.ctor_tag(TUPLE);
                Value::Obj(self.heap.alloc(Kind::Data, tag, &fields))
            }
            Build::Ctor(name, xs) => {
                let fields: Vec<Value> = xs.into_iter().map(|x| self.build_here(x)).collect();
                let tag = self.host_ctor(&name).unwrap_or(u32::MAX);
                Value::Obj(self.heap.alloc(Kind::Data, tag, &fields))
            }
            Build::Bytes(b) => Value::Obj(self.heap.alloc_bytes(&b)),
            Build::Vector(xs) => {
                let items: Vec<Value> = xs.into_iter().map(|x| self.build_here(x)).collect();
                self.vector_here(items)
            }
        }
    }

    /// Lay `items` out as `Vector.fromArray` would, or as a plain array where
    /// the program declares no vector (`docs/CUT.md`). Room must already have
    /// been made -- see [`slots`], which counts a vector's, the more.
    fn vector_here(&mut self, items: Vec<Value>) -> Value {
        if self.role(Role::VectorSingle).is_none() {
            return Value::Obj(self.heap.alloc(Kind::Array, 0, &items));
        }
        let data = |vm: &mut Vm, role: Role, fields: &[Value]| {
            let tag = vm.role_tag(role);
            Value::Obj(vm.heap.alloc(Kind::Data, tag, fields))
        };
        let array = |vm: &mut Vm, xs: &[Value]| Value::Obj(vm.heap.alloc(Kind::Array, 0, xs));
        if items.is_empty() {
            return data(self, Role::VectorEmpty, &[]);
        }
        if items.len() <= VECTOR_WIDTH {
            let a = array(self, &items);
            return data(self, Role::VectorSingle, &[a]);
        }
        let n = items.len();
        let (shift, _) = vector_shape(n);
        let mut nodes: Vec<Value> = items
            .chunks(VECTOR_WIDTH)
            .map(|chunk| {
                let a = array(self, chunk);
                data(self, Role::VNodeLeaf, &[a])
            })
            .collect();
        loop {
            let group = |vm: &mut Vm, kids: &[Value]| {
                let none = data(vm, Role::None, &[]);
                let a = array(vm, kids);
                data(vm, Role::VNodeBranch, &[none, a])
            };
            if nodes.len() <= VECTOR_WIDTH {
                let root = group(self, &nodes);
                let empties: Vec<Value> = (0..4).map(|_| array(self, &[])).collect();
                return data(
                    self,
                    Role::VectorFull,
                    &[
                        Value::Int(n as i64),
                        Value::Int(shift),
                        empties[0],
                        empties[1],
                        root,
                        empties[2],
                        empties[3],
                    ],
                );
            }
            nodes = nodes
                .chunks(VECTOR_WIDTH)
                .map(|kids| group(self, kids))
                .collect();
        }
    }

    /// Discharge `effect.op` against the real world (`meadow_rt::world`), or
    /// ask the host for it, or say it cannot.
    pub(crate) fn native(
        &mut self,
        effect: &str,
        op: &str,
        arg: Value,
    ) -> Result<Option<Value>, Error> {
        match meadow_rt::world::perform(self, effect, op, arg) {
            Ok(Some(b)) => Ok(Some(self.build(b))),
            // Not the runtime's own: the host's, if this machine has one.
            Ok(None) => self.ask_host(effect, op, arg),
            Err(msg) => err(msg),
        }
    }
}

/// What this machine says of itself to `meadow_rt::world`: its readers keep
/// the words they always had.
impl World for Vm<'_> {
    type Value = Value;

    fn int_of(&self, v: Value) -> Option<i64> {
        match v {
            Value::Int(n) => Some(n),
            _ => None,
        }
    }

    fn float_of(&self, v: Value) -> Option<f64> {
        match v {
            Value::Float(x) => Some(x),
            _ => None,
        }
    }

    fn bool_of(&self, v: Value) -> Option<bool> {
        match v {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    fn text(&self, v: Value, what: &str) -> Result<String, String> {
        Vm::text(self, v, what).map_err(|e| e.msg)
    }

    fn bytes(&self, v: Value, what: &str) -> Result<Vec<u8>, String> {
        Vm::bytes(self, v, what).map_err(|e| e.msg)
    }

    fn tuple(&self, v: Value, n: usize, what: &str) -> Result<Vec<Value>, String> {
        let ok = v
            .addr()
            .filter(|a| self.heap.kind(*a) == Kind::Data && self.heap.len(*a) == n)
            .filter(|a| {
                self.program
                    .ctor(self.heap.meta(*a))
                    .is_some_and(|c| &*c == TUPLE)
            });
        match ok {
            Some(a) => Ok(self.heap.fields(a)),
            None => Err(format!(
                "{what}: expected a tuple of {n}, got {}",
                self.show(v)
            )),
        }
    }

    fn seq(&self, v: Value, what: &str) -> Result<Vec<Value>, String> {
        match self.vector_elems(v).or_else(|| self.array_elems(v)) {
            Some(xs) => Ok(xs),
            None => Err(format!("{what}: expected a Vector, got {}", self.show(v))),
        }
    }

    fn maybe(&self, v: Value, what: &str) -> Result<Option<Value>, String> {
        let Some(a) = v.addr().filter(|a| self.heap.kind(*a) == Kind::Data) else {
            return Err(format!("{what}: expected a Maybe, got {}", self.show(v)));
        };
        match self
            .program
            .ctor(self.heap.meta(a))
            .and_then(|n| self.role_of(&n))
        {
            Some(Role::None) if self.heap.len(a) == 0 => Ok(None),
            Some(Role::Some) if self.heap.len(a) == 1 => Ok(Some(self.heap.field(a, 0))),
            _ => Err(format!("{what}: expected a Maybe, got {}", self.show(v))),
        }
    }

    fn shown(&self, v: Value) -> String {
        self.show(v)
    }

    fn printed(&self, v: Value, what: &str) -> Result<String, String> {
        Vm::text(self, v, what).map_err(|e| e.msg)
    }

    /// Wherever [`Vm::io`] sends it: a debugger shows a program's printing in
    /// its console.
    fn write_out(&mut self, s: &str) {
        Vm::write_out(self, s);
    }

    fn read_line(&mut self) -> Option<Option<String>> {
        self.io.input.as_mut().map(|input| input())
    }

    fn argv(&self) -> Vec<String> {
        meadow_rt::args::get()
    }

    /// See `Vm::reached`.
    fn covered(&mut self, reset: bool) -> i64 {
        Vm::covered(self, reset) as i64
    }

    fn exit(&mut self, code: i32) -> ! {
        std::process::exit(code)
    }

    /// A machine a scheduler runs leaves the job as a request, so that the
    /// thread waits rather than its worker (see [`crate::sched`]), and the
    /// answer replaces the placeholder; one run on its own does it there and
    /// then.
    fn blocking(&mut self, job: Job<Value>) -> Build {
        if self.scheduled {
            self.blocking = Some(job);
            Build::unit()
        } else {
            job()
        }
    }
}
