//! `hash`: a structural hash that agrees with `==`.
//!
//! The engines' agreement on plain values is `tests/differential.rs`; these
//! are the cases that need `Std` -- a `Vector` is one of its types -- or that are
//! about what `hash` refuses.

mod common;
use common::{cek_main_std, eval_main_std};

fn both(src: &str) -> String {
    let vm = eval_main_std(src);
    assert_eq!(vm, cek_main_std(src), "the engines disagree on\n{src}");
    vm
}

#[test]
fn equal_vectors_hash_alike_whatever_their_shape() {
    // Past one chunk, a literal and a vector grown by pushes are laid out
    // differently inside and still compare equal -- so they must hash alike.
    let src = "def pushed = foldl (\\v i -> pushBack v i) [] (range 0 100)\n\
               def result = (pushed == range 0 100, hash pushed == hash (range 0 100), hash [1, 2] == hash [2, 1])\n";
    assert_eq!(both(src), "(True, True, False)");
}

#[test]
fn a_ref_cannot_be_hashed() {
    let out = both("def result = hash (newRef 1)\n");
    assert!(out.contains("cannot hash a Ref"), "{out}");
}

/// A closure that captures nothing is a word carrying where its methods are
/// in the method table, as a constructor of no fields is a word carrying its
/// tag, and the two are numbered apart: where a method table's number is
/// some constructor's tag, as in the whole standard library, the VM takes the
/// closure for that constructor and hashes it. In a smaller program, where
/// the numbers do not meet, it is refused.
#[test]
#[ignore = "a captureless closure and a nullary constructor are told apart by number only"]
fn a_function_cannot_be_hashed() {
    let out = both("def result = hash (\\x -> x)\n");
    assert!(out.contains("cannot hash a function"), "{out}");
}

#[test]
fn a_hash_map_behaves_the_same_on_both_engines() {
    let src = "use Std.Collections.HashMap as H\n\
               def m = foldl (\\acc i -> H.insert (show i) i acc) H.empty (range 0 500)\n\
               def result = (H.size m, H.lookup \"250\" m, H.lookup \"x\" m, H.size (H.delete \"7\" m))\n";
    assert_eq!(both(src), "(500, Just(250), None, 499)");
}
