//! **Roles: the values a runtime builds or recognizes by itself.**
//!
//! A native operation answers with an optional value or a result, splits a
//! line it read into a list, or makes a vector; `show` prints a list or a
//! vector as the sequence it is, and a tuple as a tuple. None of that is the
//! program's doing, so a runtime has to know which of the program's
//! constructors to build and to look for -- and which it is is the front end's
//! business, not the runtime's: Meadow's optional value is `Maybe.Just`,
//! another language's may be `Option.Some`. So a program declares, for each
//! [`Role`], the constructor that plays it (`meadow_axcut::Program::roles`),
//! and a runtime asks for the role, never for a name.
//!
//! What a role promises is its shape, the same in every language that
//! declares it -- [`Role::shape`] says it. A program that declares none of a
//! role's constructors is one no runtime builds that value for: an operation
//! that would have to says so instead.
//!
//! A few names are the IR's own rather than any language's: a tuple's
//! constructor ([`TUPLE`]) and the evidence an effect is performed with
//! ([`evidence`]). Those every front end uses as they are.

/// A part a constructor plays for the runtimes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Role {
    /// The empty optional value: no fields.
    None,
    /// An optional value that has one: one field, the value.
    Some,
    /// A result that succeeded: one field, the value.
    Ok,
    /// A result that failed: one field, the error.
    Err,
    /// The empty list: no fields.
    Nil,
    /// A list's first element and the rest: two fields.
    Cons,
    /// The empty vector: no fields.
    VectorEmpty,
    /// A vector of one array: one field, the array.
    VectorSingle,
    /// A vector as a tree: seven fields -- see `Std.Collections.Vector`.
    VectorFull,
    /// A vector tree's leaf: one field, an array of elements.
    VNodeLeaf,
    /// A vector tree's branch: two fields, an optional size table and an array
    /// of children.
    VNodeBranch,
    /// `False` built as data, which a branch treats as false: no fields.
    False,
    /// A file's metadata, as `Fs.metadata` answers it: a record of `isFile`,
    /// `isDir`, `len`, `readonly` and `modified`, in that order.
    FileMeta,
}

impl Role {
    /// Every role, in the order a runtime's tables keep them: a role's place
    /// here is its [`Role::index`], which an emitted program's table and the
    /// runtime reading it agree on.
    pub const ALL: [Role; 13] = [
        Role::None,
        Role::Some,
        Role::Ok,
        Role::Err,
        Role::Nil,
        Role::Cons,
        Role::VectorEmpty,
        Role::VectorSingle,
        Role::VectorFull,
        Role::VNodeLeaf,
        Role::VNodeBranch,
        Role::False,
        Role::FileMeta,
    ];

    /// This role's place in [`Role::ALL`].
    pub fn index(self) -> usize {
        Role::ALL
            .iter()
            .position(|r| *r == self)
            .expect("every role is in ALL")
    }

    /// What it is called, as the textual IR writes it.
    pub const fn name(self) -> &'static str {
        match self {
            Role::None => "none",
            Role::Some => "some",
            Role::Ok => "ok",
            Role::Err => "err",
            Role::Nil => "nil",
            Role::Cons => "cons",
            Role::VectorEmpty => "vector-empty",
            Role::VectorSingle => "vector-single",
            Role::VectorFull => "vector-full",
            Role::VNodeLeaf => "vnode-leaf",
            Role::VNodeBranch => "vnode-branch",
            Role::False => "false",
            Role::FileMeta => "file-meta",
        }
    }

    /// The role [`Role::name`] calls `name`.
    pub fn named(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.name() == name)
    }

    /// How many fields the constructor playing it has.
    pub const fn arity(self) -> usize {
        match self {
            Role::None | Role::Nil | Role::VectorEmpty | Role::False => 0,
            Role::Some | Role::Ok | Role::Err | Role::VectorSingle | Role::VNodeLeaf => 1,
            Role::Cons | Role::VNodeBranch => 2,
            Role::FileMeta => 5,
            Role::VectorFull => 7,
        }
    }
}

/// The constructor of a tuple, in every language: the IR's own.
pub const TUPLE: &str = "#tuple";

/// The evidence an effect is performed with: the handlers in scope, newest
/// first, as data the lowering builds and the runtimes start every thread
/// with. Fixed by the IR, so that a front end's lowering of effects and every
/// runtime agree on it.
pub mod evidence {
    /// An entry: `#ev(key, clause, target, rest)`.
    pub const ENTRY: &str = "#ev";
    /// An entry for a tail-resumptive clause -- `op x k -> k e` -- whose
    /// object takes the argument and the performing code's own continuation.
    pub const TAIL: &str = "#evt";
    /// No handlers: an object, so that evidence is always a reference.
    pub const NONE: &str = "#evnone";
}

/// The records a runtime builds by their constructor's name: `Fs.metadata`'s
/// `FileMeta`, a name the language knows (`meadow_rename`'s
/// `LANGUAGE_NAMES`). With the roles' constructors, what a program loaded
/// into a running one must not change: the runtime finds each by its name,
/// and would not know which build's it meant.
pub const RECORDS: &[&str] = &["FileMeta.FileMeta"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_is_found_by_its_name_and_its_place() {
        for (i, r) in Role::ALL.into_iter().enumerate() {
            assert_eq!(r.index(), i);
            assert_eq!(Role::named(r.name()), Some(r));
        }
        assert_eq!(Role::named("maybe"), None);
    }
}
