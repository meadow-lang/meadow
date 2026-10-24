//! **Which slots hold a value**, at each pc of a procedure: what a debugger
//! may show of a frame.
//!
//! A slot keeps whatever it last held. After its name's last use that may
//! be a block given back -- erased, consumed by a switch, passed on owned to
//! a call -- or a field borrowed from one; and a frame of words reused by a
//! tail call keeps the last procedure's in the slots the next has not
//! written yet. Read as values, those can be anything at all: where the
//! freed memory is unmapped, reading one fails. So a debugger shows only a
//! slot that is *live*: one that some path from the pc reads before
//! anything writes it. The program being linear, what it is about to read
//! is still alive.
//!
//! Control never joins within a procedure, and every successor of an
//! instruction is laid out after it (see `code`), so one pass from the last
//! pc to the first is the whole analysis. It is only ever run for a frame a
//! debugger looks at.

use super::code::{Code, Ins, WideI};
use super::{Callee, D, Slot, U};

/// A set of slots.
#[derive(Clone, Default)]
pub(super) struct Slots(Vec<u64>);

impl Slots {
    fn put(&mut self, s: Slot) {
        let (w, b) = (s as usize / 64, s % 64);
        if self.0.len() <= w {
            self.0.resize(w + 1, 0);
        }
        self.0[w] |= 1 << b;
    }

    fn take(&mut self, s: Slot) {
        if let Some(w) = self.0.get_mut(s as usize / 64) {
            *w &= !(1 << (s % 64));
        }
    }

    pub(super) fn has(&self, s: Slot) -> bool {
        self.0
            .get(s as usize / 64)
            .is_some_and(|w| w & (1 << (s % 64)) != 0)
    }

    fn join(&mut self, other: &Slots) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        for (w, o) in self.0.iter_mut().zip(&other.0) {
            *w |= o;
        }
    }
}

/// What a procedure's slots hold, by pc: those live as each instruction
/// begins, and those live once it has done what it does -- for a frame
/// waiting on the call it makes, which has already handed on its
/// arguments.
pub(super) struct Live {
    entry: usize,
    before: Vec<Slots>,
    after: Vec<Slots>,
}

impl Live {
    /// Live as the instruction at `pc` begins: a frame stopped there.
    pub(super) fn at(&self, pc: usize) -> Slots {
        self.get(&self.before, pc)
    }

    /// Live once the instruction at `pc` is done, but for what it writes: a
    /// frame waiting there on a call.
    pub(super) fn waiting(&self, pc: usize) -> Slots {
        self.get(&self.after, pc)
    }

    fn get(&self, of: &[Slots], pc: usize) -> Slots {
        pc.checked_sub(self.entry)
            .and_then(|i| of.get(i))
            .cloned()
            .unwrap_or_default()
    }
}

fn read_d(d: D, into: &mut Slots) {
    if let D::S(s) = d {
        into.put(s);
    }
}

fn read_u(u: &U, into: &mut Slots) {
    into.put(u.s);
    read_d(u.d, into);
}

/// The instructions of `code` from `entry` to `end`, one procedure's.
pub(super) fn of(code: &Code, entry: usize, end: usize) -> Live {
    let n = end.saturating_sub(entry);
    let mut before = vec![Slots::default(); n];
    let mut after = vec![Slots::default(); n];
    let live_at = |before: &[Slots], pc: usize| -> Slots {
        pc.checked_sub(entry)
            .and_then(|i| before.get(i))
            .cloned()
            .unwrap_or_default()
    };
    for pc in (entry..end).rev() {
        let mut reads = Slots::default();
        let mut writes: Vec<Slot> = Vec::new();
        // Live after it, the edges it may take joined.
        let mut out = Slots::default();
        let next = |out: &mut Slots, before: &[Slots]| out.join(&live_at(before, pc + 1));
        match code.ins[pc] {
            Ins::Share { s, d } | Ins::Erase { s, d } => {
                reads.put(Slot::from(s));
                read_d(d.get(), &mut reads);
                next(&mut out, &before);
            }
            Ins::Rename(i) => {
                for (to, from) in &code.renames[i as usize] {
                    reads.put(*from);
                    writes.push(*to);
                }
                next(&mut out, &before);
            }
            Ins::Let(i) => {
                let l = &code.lets[i as usize];
                l.fields.iter().for_each(|u| read_u(u, &mut reads));
                if let Some(r) = l.reuse {
                    reads.put(r);
                }
                writes.push(l.dst);
                next(&mut out, &before);
            }
            Ins::Clean(t) => {
                reads.put(t);
                next(&mut out, &before);
            }
            Ins::DropReuse(i) => {
                let dr = &code.drops[i as usize];
                reads.put(dr.src);
                dr.fields.iter().for_each(|d| read_d(*d, &mut reads));
                writes.push(dr.token);
                next(&mut out, &before);
            }
            Ins::Switch(i) => {
                let sw = &code.switches[i as usize];
                reads.put(sw.scrutinee);
                for a in &sw.arms {
                    // What an arm binds is written on its way in.
                    let mut arm = live_at(&before, a.pc as usize);
                    for f in &a.fields {
                        arm.take(f.s);
                    }
                    if let Some(r) = a.reuse {
                        arm.take(r);
                    }
                    out.join(&arm);
                }
                out.join(&live_at(&before, sw.default as usize));
            }
            Ins::New(i) => {
                let nw = &code.news[i as usize];
                nw.captures.iter().for_each(|u| read_u(u, &mut reads));
                writes.push(nw.dst);
                next(&mut out, &before);
            }
            Ins::Jump(i) => {
                code.jumps[i as usize]
                    .args
                    .iter()
                    .for_each(|s| reads.put(*s));
            }
            Ins::Invoke(i) => {
                let iv = &code.invokes[i as usize];
                reads.put(iv.target);
                iv.args.iter().for_each(|s| reads.put(*s));
            }
            Ins::Call(i) => {
                let c = &code.calls[i as usize];
                c.args.iter().for_each(|s| reads.put(*s));
                c.held.iter().for_each(|(u, _)| read_u(u, &mut reads));
                if let Callee::Method { target, .. } = c.callee {
                    reads.put(target);
                }
                writes.push(c.answer);
                next(&mut out, &before);
            }
            Ins::Prim(i) => {
                let p = &code.prims[i as usize];
                p.args.iter().for_each(|u| read_u(u, &mut reads));
                writes.extend(p.result);
                next(&mut out, &before);
            }
            Ins::Bin { a, b, dst, .. } => {
                reads.put(Slot::from(a));
                reads.put(Slot::from(b));
                writes.push(Slot::from(dst));
                next(&mut out, &before);
            }
            Ins::BinK { a, dst, .. } | Ins::BinI { a, dst, .. } => {
                reads.put(Slot::from(a));
                writes.push(Slot::from(dst));
                next(&mut out, &before);
            }
            Ins::IfBin { a, b, no, .. } => {
                reads.put(Slot::from(a));
                reads.put(Slot::from(b));
                next(&mut out, &before);
                out.join(&live_at(&before, no as usize));
            }
            Ins::IfBinI { a, no, .. } => {
                reads.put(Slot::from(a));
                next(&mut out, &before);
                out.join(&live_at(&before, no as usize));
            }
            Ins::Branch(t) => {
                let t = &code.tests[t as usize];
                t.args.iter().for_each(|u| read_u(u, &mut reads));
                next(&mut out, &before);
                out.join(&live_at(&before, t.no as usize));
            }
            Ins::Wide(w) => {
                match &code.wides[w as usize] {
                    WideI::Share { s, d } | WideI::Erase { s, d } => {
                        reads.put(*s);
                        read_d(*d, &mut reads);
                    }
                    WideI::Bin { a, b, dst, .. } => {
                        reads.put(*a);
                        reads.put(*b);
                        writes.push(*dst);
                    }
                    WideI::BinK { a, dst, .. } => {
                        reads.put(*a);
                        writes.push(*dst);
                    }
                    WideI::IfBin { a, b, no, .. } => {
                        reads.put(*a);
                        reads.put(*b);
                        out.join(&live_at(&before, *no as usize));
                    }
                    WideI::IfBinK { a, no, .. } => {
                        reads.put(*a);
                        out.join(&live_at(&before, *no as usize));
                    }
                }
                next(&mut out, &before);
            }
            Ins::Segment(i) => {
                let sg = &code.segments[i as usize];
                read_u(&sg.arg, &mut reads);
                sg.captures.iter().for_each(|u| read_u(u, &mut reads));
            }
            Ins::Error(_) => {}
        }
        for w in &writes {
            out.take(*w);
        }
        let mut live = out.clone();
        live.join(&reads);
        before[pc - entry] = live;
        after[pc - entry] = out;
    }
    Live {
        entry,
        before,
        after,
    }
}
