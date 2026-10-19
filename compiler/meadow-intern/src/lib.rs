//! String interning.
//!
//! Identifiers, string literals and type names are interned into a single
//! process-wide [`ThreadedRodeo`] so the rest of the compiler can pass around a
//! `Copy`, `Eq`, `Hash` handle ([`InternedString`]) instead of `String`s.
//! `InternedString` `Deref`s to `str`, so it is usable directly in `format!`,
//! comparisons, etc. The interner is never cleared — fine for a compiler process.

use lasso::{Spur, ThreadedRodeo};
use once_cell::sync::Lazy;
use std::{
    borrow::Borrow,
    fmt::{Debug, Display},
    ops::Deref,
};

static INTERNER: Lazy<ThreadedRodeo> = Lazy::new(|| ThreadedRodeo::default());

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InternedString {
    pub key: Spur,
}

/// The empty string. Not the handle's own default: that is the first handle,
/// whichever string the process happened to intern first -- in a build, the
/// package name a directory's name suggests -- and every definition made with
/// `module: Default::default()` was put in a module of that name.
impl Default for InternedString {
    fn default() -> Self {
        static EMPTY: Lazy<InternedString> = Lazy::new(|| InternedString {
            key: INTERNER.get_or_intern_static(""),
        });
        *EMPTY
    }
}

/// In the order of the text, not of the handles. A handle is a string's
/// place in the order strings were first seen, and with several threads
/// interning that is a different order each run -- so whatever was sorted by
/// it came out differently each run too: where a record keeps each field,
/// which of two definitions is written first, which string is `@s40`. The
/// program meant the same, and was not the same program: an object file
/// compiled from one build's text was no use to the next.
impl Ord for InternedString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.key == other.key {
            return std::cmp::Ordering::Equal;
        }
        INTERNER
            .resolve(&self.key)
            .cmp(INTERNER.resolve(&other.key))
    }
}

impl PartialOrd for InternedString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl InternedString {
    /// The handle as a number, for storing in a machine word. Stable for the
    /// life of the process, like the handle itself.
    pub fn to_raw(self) -> u32 {
        lasso::Key::into_usize(self.key) as u32
    }

    /// The handle [`InternedString::to_raw`] made `raw` from.
    pub fn from_raw(raw: u32) -> Option<InternedString> {
        <Spur as lasso::Key>::try_from_usize(raw as usize).map(|key| InternedString { key })
    }
}

impl From<Spur> for InternedString {
    fn from(key: Spur) -> Self {
        Self { key }
    }
}

impl From<&str> for InternedString {
    fn from(name: &str) -> Self {
        Self {
            key: INTERNER.get_or_intern(name),
        }
    }
}

impl From<String> for InternedString {
    fn from(name: String) -> Self {
        Self {
            key: INTERNER.get_or_intern(name),
        }
    }
}

/// Written as its text, since a handle means nothing to another process: a
/// compiled package saved by one build and read back by the next is interned
/// again as it is read.
impl serde::Serialize for InternedString {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(INTERNER.resolve(&self.key))
    }
}

impl<'de> serde::Deserialize<'de> for InternedString {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Text;
        impl serde::de::Visitor<'_> for Text {
            type Value = InternedString;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<InternedString, E> {
                Ok(InternedString::from(v))
            }
        }
        d.deserialize_str(Text)
    }
}

impl Debug for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InternedString({})", INTERNER.resolve(&self.key))
    }
}

impl Display for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", INTERNER.resolve(&self.key))
    }
}

impl Borrow<str> for InternedString {
    fn borrow(&self) -> &str {
        INTERNER.resolve(&self.key)
    }
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        INTERNER.resolve(&self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_empty_string_whatever_was_interned_first() {
        // A build interns a package name before anything asks for a default;
        // the default handle used to be that name.
        let _ = InternedString::from("Silorepro");
        assert_eq!(&*InternedString::default(), "");
        assert!(InternedString::default().is_empty());
        assert_eq!(InternedString::default(), InternedString::from(""));
    }
}
