//! **Values crossing between threads.** Each green thread has a heap of its
//! own (`crate::ctx`), so what one hands another -- a spawned function and
//! what it captures, a message, a thread's answer, a `TVar`'s value -- is
//! copied: out of the sender's heap into a parcel, which belongs to no heap,
//! and out of the parcel into the receiver's. Sharing within the value is
//! kept, so a structure that is a DAG arrives as one.
//!
//! Immutable data copies without anyone being able to tell. What cannot be
//! copied so is refused, as `meadow-glade` refuses it: a `Ref`, a mutable array,
//! a continuation. A thread, a channel or a `TVar` is a handle to something
//! the scheduler keeps, and crosses as the same handle.

use crate::heap::{self, Word};
use meadow_rt::desc;
use std::collections::HashMap;

/// A value lifted out of a heap.
pub struct Parcel {
    /// The value's word -- or, for a block, its index in `blocks` -- and its
    /// descriptor.
    root: (Word, i64),
    blocks: Vec<Lifted>,
    /// The regions this parcel carries a reference into, one per `Compact` it
    /// holds. A region belongs to no thread, so a compact crosses as the
    /// pointer it is and nothing inside it is copied -- which is what
    /// `Std.Compact` promises. See [`crate::region`].
    regions: Vec<*const crate::region::Region>,
    /// The threads this parcel carries a `Task` of, one per `Task`: while
    /// it is in flight it is a way to the thread's answer, like a `Task` in
    /// a heap -- see `crate::sched::held`.
    tasks: Vec<u32>,
    /// And the `TVar`s it carries a handle to, by their cells' addresses.
    vars: Vec<usize>,
}

// Safety: what a parcel holds is a copy belonging to no heap, and a region it
// names is shared between threads by design -- closed, immutable, and counted
// atomically. See [`crate::region`].
unsafe impl Send for Parcel {}
unsafe impl Sync for Parcel {}

impl Clone for Parcel {
    fn clone(&self) -> Parcel {
        for r in &self.regions {
            // Safety: this parcel holds a reference to each.
            unsafe { crate::region::retain(*r) };
        }
        for t in &self.tasks {
            crate::sched::held(*t);
        }
        for v in &self.vars {
            crate::sched::held_var(*v);
        }
        Parcel {
            root: self.root,
            blocks: self.blocks.clone(),
            regions: self.regions.clone(),
            tasks: self.tasks.clone(),
            vars: self.vars.clone(),
        }
    }
}

impl Drop for Parcel {
    fn drop(&mut self) {
        for r in &self.regions {
            // Safety: as above, and given up here.
            unsafe { crate::region::release(*r) };
        }
        for t in &self.tasks {
            crate::sched::let_go(*t);
        }
        for v in &self.vars {
            crate::sched::let_go_var(*v);
        }
    }
}

#[derive(Clone)]
struct Lifted {
    /// The whole block, header and all, with each reference field holding
    /// the index of the block it refers to.
    words: Vec<Word>,
    /// Which words those are.
    refs: Vec<usize>,
    /// How many references to it the parcel holds.
    incoming: u32,
}

impl Parcel {
    /// `v`, described by `d`, lifted out of the running thread's heap -- which
    /// keeps it -- or why it cannot be.
    pub fn of(v: Word, d: i64) -> Result<Parcel, String> {
        let mut blocks: Vec<Lifted> = Vec::new();
        let mut index: HashMap<Word, usize> = HashMap::new();
        let mut regions = Vec::new();
        let mut tasks = Vec::new();
        let mut vars = Vec::new();
        if d != desc::REF || !heap::is_block(v) {
            return Ok(Parcel {
                root: (v, d),
                blocks,
                regions,
                tasks,
                vars,
            });
        }
        let mut todo = vec![v];
        index.insert(v, 0);
        blocks.push(lift(v)?);
        while let Some(b) = todo.pop() {
            let at = index[&b];
            let first = heap::first_field(b);
            let raw = heap::kind(b) == heap::STRING || heap::kind(b) == heap::BIGINT;
            if raw {
                continue;
            }
            if heap::kind(b) == heap::TASK {
                crate::sched::held(heap::meta(b));
                tasks.push(heap::meta(b));
                continue;
            }
            if heap::kind(b) == heap::TVAR {
                crate::sched::held_var(heap::field(b, 0) as usize);
                vars.push(heap::field(b, 0) as usize);
                continue;
            }
            // A compact crosses as itself: its region belongs to no thread,
            // so the block is carried whole -- the pointer into the region
            // and the region beside it -- and nothing in it is copied. The
            // parcel keeps a reference to the region while it is in flight.
            if heap::kind(b) == heap::COMPACT {
                let r = heap::field(b, 1) as *const crate::region::Region;
                // Safety: the block being lifted holds a reference to it, and
                // the heap it is in keeps that block while this runs.
                unsafe { crate::region::retain(r) };
                regions.push(r);
                continue;
            }
            for i in 0..heap::len(b) {
                let x = heap::field(b, i);
                if heap::field_desc(b, i) != desc::REF || !heap::is_block(x) {
                    continue;
                }
                let j = match index.get(&x) {
                    Some(j) => *j,
                    None => {
                        let j = blocks.len();
                        blocks.push(lift(x)?);
                        index.insert(x, j);
                        todo.push(x);
                        j
                    }
                };
                blocks[j].incoming += 1;
                blocks[at].words[first + i] = j as Word;
                blocks[at].refs.push(first + i);
            }
        }
        blocks[0].incoming += 1;
        Ok(Parcel {
            root: (0, d),
            blocks,
            regions,
            tasks,
            vars,
        })
    }

    /// The value, put into the running thread's heap: owned by the caller.
    pub fn open(&self) -> Word {
        if self.blocks.is_empty() {
            return self.root.0;
        }
        let made: Vec<Word> = self
            .blocks
            .iter()
            .map(|b| {
                let v = heap::acquire(b.words.len());
                for (i, w) in b.words.iter().enumerate() {
                    heap::set_word(v, i, *w);
                }
                // The count is the references besides one.
                let len = b.words[0] & 0xFFFF_FFFF_0000_0000;
                heap::set_word(v, 0, len | u64::from(b.incoming - 1));
                // What the sending thread's cycle collector thought of the
                // block it copied is about that heap's candidate list, not
                // this one's: a fresh block here has never been a candidate.
                heap::set_word(v, 1, b.words[1] & !heap::MARKS);
                // A compact arrives naming the region it always named, and
                // that block holds two references into it: its own, given up
                // when the block dies, and the one its field holds, given up
                // where that field is erased.
                // A `Task` arrived is one more way to its thread's answer.
                if heap::kind(v) == heap::TASK {
                    crate::sched::held(heap::meta(v));
                }
                if heap::kind(v) == heap::TVAR {
                    crate::sched::held_var(heap::field(v, 0) as usize);
                }
                if heap::kind(v) == heap::COMPACT {
                    let r = heap::field(v, 1) as *const crate::region::Region;
                    // Safety: the parcel holds a reference to it, so it is
                    // alive while these are taken.
                    unsafe {
                        crate::region::retain(r);
                        if heap::field_desc(v, 0) == desc::REF && heap::in_region(heap::field(v, 0))
                        {
                            crate::region::retain(r);
                        }
                    }
                }
                v
            })
            .collect();
        for (b, v) in self.blocks.iter().zip(&made) {
            for &i in &b.refs {
                heap::set_word(*v, i, made[b.words[i] as usize]);
            }
        }
        made[0]
    }

    /// The descriptor of the value.
    pub fn desc(&self) -> i64 {
        self.root.1
    }
}

fn lift(v: Word) -> Result<Lifted, String> {
    let why = match heap::kind(v) {
        heap::CELL => Some("a Ref"),
        heap::MUT_ARRAY => Some("a mutable array"),
        heap::ONCE | heap::STACK => Some("a continuation"),
        _ => None,
    };
    if let Some(what) = why {
        return Err(meadow_rt::thread::unsendable(what));
    }
    let n = heap::size(v);
    let mut words: Vec<Word> = (0..n).map(|i| heap::word(v, i)).collect();
    words[0] &= 0xFFFF_FFFF_0000_0000;
    Ok(Lifted {
        words,
        refs: Vec::new(),
        incoming: 0,
    })
}
