//! `use` declarations: module resolution, the `as` alias, and reporting a path
//! that names no module.

mod common;
use common::{errors_std_with, eval_main_std, eval_unit, unit_errors};
use meadow::Options;

fn errors(src: &str) -> String {
    errors_std_with(src, Options::debug().entry("result"))
}

// --- resolution errors -------------------------------------------------------

#[test]
fn unknown_module_is_an_error() {
    // Previously this was silently accepted, and only a *use site* of the
    // qualifier complained.
    assert_eq!(
        errors("use Nowhere\ndef result = 1\n"),
        "no module `Nowhere`"
    );
}

#[test]
fn unknown_submodule_of_a_real_package_is_an_error() {
    assert_eq!(
        errors("use Std.Nope\ndef result = 1\n"),
        "no module `Std.Nope`"
    );
}

#[test]
fn a_misplaced_module_suggests_its_real_path() {
    assert_eq!(
        errors("use List\ndef result = 1\n"),
        "no module `List` — did you mean `Std.Collections.List`?"
    );
    assert_eq!(
        errors("use Std.Vector\ndef result = 1\n"),
        "no module `Std.Vector` — did you mean `Std.Collections.Vector`?"
    );
}

#[test]
fn a_real_module_is_accepted() {
    assert_eq!(errors("use Std.Collections.List\ndef result = 1\n"), "");
}

#[test]
fn a_module_that_only_declares_submodules_is_accepted() {
    // `Std.Collections` is nothing but `mod` lines, so it exports no values —
    // which must not be mistaken for "no such module".
    assert_eq!(errors("use Std.Collections\ndef result = 1\n"), "");
}

// --- what a bare `use` does -------------------------------------------------

#[test]
fn a_bare_use_imports_every_name_unqualified() {
    assert_eq!(
        eval_main_std("use Std.Collections.List\ndef result = length [1; 2; 3]\n"),
        "3"
    );
}

#[test]
fn a_bare_use_introduces_no_qualifier() {
    // Qualifying is what `as` is for.
    assert_eq!(
        errors("use Std.Collections.List\ndef result = List.length [1; 2]\n"),
        "module `List` is not in scope here (add `use <path> as List`)"
    );
}

#[test]
fn a_bare_use_shadows_the_prelude() {
    // The prelude's `length` is `Vector`'s; `List`'s wins where it is imported.
    assert_eq!(
        eval_main_std("use Std.Collections.List\ndef result = length [1; 2; 3]\n"),
        "3"
    );
}

// --- qualifiers and aliases --------------------------------------------------

#[test]
fn as_gives_a_qualifier() {
    assert_eq!(
        eval_main_std("use Std.Collections.List as L\ndef result = L.length [1; 2; 3]\n"),
        "3"
    );
}

#[test]
fn as_does_not_import_unqualified() {
    // `[1; 2]` is a `List`, so reaching the prelude's `Vector.length` here is a
    // type error — which is the evidence that `as` imported nothing.
    assert!(
        errors("use Std.Collections.List as L\ndef result = length [1; 2]\n")
            .contains("type mismatch"),
        "an aliased `use` should not bring names into scope unqualified"
    );
}

#[test]
fn two_aliases_for_one_module_coexist() {
    assert_eq!(
        eval_main_std(
            "use Std.Collections.List as List\n\
             use Std.Collections.List as L\n\
             def result = (List.length [1; 2; 3], L.length [1; 2])\n"
        ),
        "(3, 2)"
    );
}

#[test]
fn one_alias_for_two_modules_is_an_error() {
    // The second used to take the first's place without a word, and a name
    // only the first exports was then "not exported by module `C`".
    assert_eq!(
        errors(
            "use Std.Collections.List as C\n\
             use Std.String as C\n\
             def result = 1\n"
        ),
        "`C` already names `Std.Collections.List` here, so it cannot name `Std.String` too"
    );
}

#[test]
fn the_same_use_twice_is_let_be() {
    assert_eq!(
        eval_main_std(
            "use Std.Collections.List as L\n\
             use Std.Collections.List as L\n\
             def result = L.length [1; 2]\n"
        ),
        "2"
    );
}

// --- selected names ----------------------------------------------------------

#[test]
fn selected_names_arrive_unqualified() {
    assert_eq!(
        eval_main_std("use Std.String (concat)\ndef result = concat \"a\" \"b\"\n"),
        "\"ab\""
    );
}

#[test]
fn selecting_names_introduces_no_qualifier() {
    assert_eq!(
        errors("use Std.String (concat)\ndef result = String.concat \"a\" \"b\"\n"),
        "module `String` is not in scope here (add `use <path> as String`)"
    );
}

#[test]
fn alias_combines_with_selected_names() {
    assert_eq!(
        eval_main_std(
            "use Std.Collections.List as L (length)\n\
             def result = (length [1; 2], L.reverse [1; 2])\n"
        ),
        "(2, [2; 1])"
    );
}

#[test]
fn distinct_modules_keep_distinct_aliases() {
    assert_eq!(
        eval_main_std(
            "use Std.Collections.List as L\n\
             use Std.Collections.Tree as T\n\
             def result = (L.length [1; 2], T.toList (T.fromList [2; 1]))\n"
        ),
        "(2, [1; 2])"
    );
}

// --- qualified types -----------------------------------------------------------

#[test]
fn a_type_is_written_through_the_module_that_qualifies_it() {
    // `M.Ty` in a type, as `M.f` in an expression: the module's type, which
    // `use M as Q` puts in scope no other way.
    assert_eq!(
        eval_main_std(
            "use Std.Collections.List as L\n\
             use Std.Maybe as Mb\n\
             fun size (xs : L.List Int) : Int = L.length xs\n\
             fun pick (m : Mb.Maybe Int) : Int = match m with | Just n -> n | None -> 0\n\
             def result = size [1; 2; 3] + pick (Just 4)\n"
        ),
        "7"
    );
}

#[test]
fn a_qualified_type_the_module_lacks_or_a_qualifier_nothing_names_is_said() {
    assert_eq!(
        errors(
            "use Std.Collections.List as L\n\
             fun f (x : L.Nope) : Int = 1\n\
             def result = 1\n"
        ),
        "unknown type `L.Nope`"
    );
    assert_eq!(
        errors("fun f (x : W.List Int) : Int = 1\ndef result = 1\n"),
        "module `W` is not in scope here (add `use <path> as W`)"
    );
}

// --- qualified constructors ----------------------------------------------------

#[test]
fn a_constructor_is_written_through_the_module_that_qualifies_it() {
    // `M.Ctor` in an expression and a pattern, as `M.f` and `M.Ty` are: the
    // module's constructor, which `use M as Q` puts in scope no other way.
    assert_eq!(
        eval_main_std(
            "use Std.Maybe as Mb
             fun pick (m : Mb.Maybe Int) : Int = match m with | Mb.Just n -> n | Mb.None -> 0
             def result = pick (Mb.Just 4) + pick Mb.None
"
        ),
        "4"
    );
    // And through its type, `M.Ty.Ctor`.
    assert_eq!(
        eval_main_std(
            "use Std.Maybe as Mb
             def result = match Mb.Maybe.Just 2 with | Mb.Maybe.Just n -> n | Mb.Maybe.None -> 0
"
        ),
        "2"
    );
}

#[test]
fn a_module_constructor_two_of_its_types_have_is_written_through_its_type() {
    let ast = "@pub mod Ast {
  @pub data Term = Var Int | App Term Term
  @pub data Ty = Var Int
}
";
    // Through the type, it is the one meant.
    assert_eq!(
        eval_unit(&[(
            "",
            &format!(
                "{ast}fun size (t : Ast.Term) : Int = match t with | Ast.Term.Var _ -> 1 | Ast.App f x -> size f + size x
                 def result = size (Ast.App (Ast.Term.Var 1) (Ast.Term.Var 2))
"
            )
        )]),
        "2"
    );
    // Without it, the module cannot say which.
    let said = unit_errors(&[(
        "",
        &format!(
            "{ast}def result = Ast.Var 1
"
        ),
    )]);
    assert!(
        said.contains(
            "`Ast.Var` could be the constructor of any of `Term`, `Ty`: write `Ast.<Type>.Var`"
        ),
        "{said}"
    );
}
