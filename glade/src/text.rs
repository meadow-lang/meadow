//! **Strings on the heap.**
//!
//! A `String` is an object of [`Kind::Str`]: its UTF-8 bytes, packed eight to
//! a word. It used to be an interned key, which made every string a program
//! built live as long as the process -- a compiler making names and output by
//! the million would only ever grow -- and made taking one apart copy the whole
//! of it into an array first. Now a string is garbage like anything else, and
//! reading its length, a byte or a slice costs what it reads.
//!
//! Interned keys remain for what is not a `String`: a record's labels and the
//! name an effect operation is dispatched on ([`Value::Str`], `desc::STR`).
//!
//! A string is UTF-8 whatever made it. Everything that builds one from bytes
//! that might not be -- a slice through a character, `bytesToString` --
//! replaces what is not with U+FFFD first, as `meadow_core::text` says.

use crate::heap::{Heap, Kind};
use crate::value::{Addr, Value};
use crate::vm::{Error, Vm, err};

impl Vm<'_> {
    /// A string holding `bytes`, which must be UTF-8. Makes room first, so it
    /// may collect: hold no address across it.
    pub(crate) fn new_text(&mut self, bytes: &[u8]) -> Value {
        debug_assert!(
            std::str::from_utf8(bytes).is_ok(),
            "a string that is not UTF-8"
        );
        self.ensure(Heap::packed_slots(bytes.len()));
        Value::Obj(self.heap.alloc_str(bytes))
    }

    /// The address of the string `v` is, if it is one.
    pub(crate) fn text_at(&self, v: Value) -> Option<Addr> {
        v.addr().filter(|a| self.heap.kind(*a) == Kind::Str)
    }

    /// The string `v` is, as primitive or operation `what` needs one.
    pub(crate) fn text_addr(&self, v: Value, what: &str) -> Result<Addr, Error> {
        match self.text_at(v) {
            Some(a) => Ok(a),
            None => err(format!("{what}: expected a String, got {}", self.show(v))),
        }
    }

    /// The bytes of `v`, a string -- or an interned name, whose text is the
    /// same thing to anyone reading it.
    pub(crate) fn text_bytes(&self, v: Value, what: &str) -> Result<Vec<u8>, Error> {
        match v {
            Value::Str(s) => Ok(s.as_bytes().to_vec()),
            v => Ok(self.heap.packed_bytes(self.text_addr(v, what)?)),
        }
    }

    /// The text of `v`, a string.
    pub(crate) fn text(&self, v: Value, what: &str) -> Result<String, Error> {
        let bytes = self.text_bytes(v, what)?;
        Ok(String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

}

impl Heap {
    /// -1, 0 or 1 as the string at `a` sorts before, with or after the one at
    /// `b`, byte by byte -- a word at a time, read big-endian so that the word
    /// comparison is the bytes'. A word's spare bytes are zero, which only ties
    /// with real zero bytes, and then the shorter string comes first.
    pub fn packed_compare(&self, a: Addr, b: Addr) -> i64 {
        let (la, lb) = (self.packed_len(a), self.packed_len(b));
        let words = la.min(lb).div_ceil(8);
        for (x, y) in self.words_in(a, 0, words).zip(self.words_in(b, 0, words)) {
            if x != y {
                return if x.swap_bytes() < y.swap_bytes() {
                    -1
                } else {
                    1
                };
            }
        }
        match la.cmp(&lb) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    /// Where `needle` first occurs in the string at `a` at or after byte
    /// `from`, or -1 -- reading the string in place, so that a scanner calling
    /// it again and again does not copy the whole of it each time.
    pub fn packed_find(&self, a: Addr, needle: &[u8], from: i64) -> i64 {
        let len = self.packed_len(a);
        let start = from.max(0) as usize;
        if start > len {
            return -1;
        }
        if needle.is_empty() {
            return start as i64;
        }
        if needle.len() > len {
            return -1;
        }
        let first = needle[0];
        let last = len - needle.len();
        let mut i = start;
        while i <= last {
            if self.packed_byte(a, i) == first
                && (1..needle.len()).all(|k| self.packed_byte(a, i + k) == needle[k])
            {
                return i as i64;
            }
            i += 1;
        }
        -1
    }
}
