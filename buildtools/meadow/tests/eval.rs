//! End-to-end evaluation snapshots. Each program defines `main`; the snapshot is
//! the `Display` of the resulting `Value` (or the runtime error).

mod common;
use common::{eval_expr, eval_expr_std, eval_main};

#[test]
fn arithmetic() {
    insta::assert_snapshot!(eval_expr("1 + 2 * 3 - 4"));
}

#[test]
fn comparison_and_bool() {
    insta::assert_snapshot!(eval_expr("if 2 < 3 then True else False"));
}

#[test]
fn and_or_precedence_and_result() {
    // `and` binds tighter than `or`, both looser than `<`.
    insta::assert_snapshot!(eval_expr("1 < 2 and 2 < 3 or 9 < 0"));
}

#[test]
fn and_short_circuits_before_dividing_by_zero() {
    // `False and _` must not evaluate the right operand.
    insta::assert_snapshot!(eval_expr("2 < 1 and 1 / 0 == 0"));
}

#[test]
fn or_short_circuits_before_dividing_by_zero() {
    insta::assert_snapshot!(eval_expr("1 < 2 or 1 / 0 == 0"));
}

#[test]
fn operator_section_two_holes() {
    // `(_ + _)` is `\a b -> a + b`
    insta::assert_snapshot!(eval_expr("(_ + _) 3 4"));
}

#[test]
fn operator_section_one_hole_each_side() {
    insta::assert_snapshot!(eval_main(
        "def addTen = (_ + 10)\ndef half = (_ / 2)\ndef result = (addTen 5, half 30, (100 - _) 40)\n"
    ));
}

#[test]
fn point_free_fun_with_no_params() {
    insta::assert_snapshot!(eval_main(
        "fun twice f x = f (f x)\nfun quad = twice (_ * 2)\ndef result = quad 3\n"
    ));
}

#[test]
fn string_literal() {
    insta::assert_snapshot!(eval_expr("\"hello\""));
}

#[test]
fn tuple_and_list() {
    insta::assert_snapshot!(eval_expr("(1, [2, 3], \"x\")"));
}

#[test]
fn lambda_application() {
    insta::assert_snapshot!(eval_expr("(\\x -> \\y -> x + y) 10 20"));
}

#[test]
fn let_binding() {
    insta::assert_snapshot!(eval_expr("let x = 21 in x + x"));
}

#[test]
fn recursion_fib() {
    insta::assert_snapshot!(eval_main(
        "fun fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
         def result = fib 15\n"
    ));
}

#[test]
fn list_map_builtin() {
    insta::assert_snapshot!(eval_main(
        "fun map f xs = match xs with | Nil -> Nil | Cons x r -> Cons (f x) (map f r)\n\
         def result = map (\\x -> x * x) [1, 2, 3, 4]\n"
    ));
}

#[test]
fn match_on_data() {
    insta::assert_snapshot!(eval_main(
        "use Shape.*\ndata Shape = Circle Int | Rect Int Int\n\
         fun area s = match s with | Circle r -> r * r | Rect w h -> w * h\n\
         def result = area (Rect 3 4) + area (Circle 5)\n"
    ));
}

#[test]
fn recursive_data() {
    insta::assert_snapshot!(eval_main(
        "use Tree.*\ndata Tree a = Tip | Branch (Tree a) a (Tree a)\n\
         fun sum t = match t with | Tip -> 0 | Branch l x r -> x + sum l + sum r\n\
         def result = sum (Branch (Branch Tip 3 Tip) 5 (Branch Tip 7 Tip))\n"
    ));
}

#[test]
fn nominal_record_field() {
    insta::assert_snapshot!(eval_main(
        "record Person = { name : String, age : Int }\n\
         def p = Person { name = \"Ann\", age = 30 }\n\
         def result = p.age\n"
    ));
}

#[test]
fn structural_record_field() {
    insta::assert_snapshot!(eval_expr("{ x = 1, y = 2 }.y"));
}

#[test]
fn division_by_zero() {
    insta::assert_snapshot!(eval_expr("1 / 0"));
}

#[test]
fn non_exhaustive_match() {
    insta::assert_snapshot!(eval_main(
        "fun head xs = match xs with | Cons x r -> x\n\
         def result = head Nil\n"
    ));
}

#[test]
fn bigint_beyond_i64() {
    // A number nothing pins down is an `Int`, which wraps; asked for, a
    // `BigInt` is exact.
    insta::assert_snapshot!(eval_expr("toBigInt 2 ^ 100"));
    insta::assert_snapshot!(eval_expr("2 ^ 100"), @"0");
}

#[test]
fn bigint_factorial() {
    // `25!` overflows i64. `fact` is generic over its number type, and `25` in
    // `main` is made a `BigInt`, so every literal in `fact` becomes one too.
    insta::assert_snapshot!(eval_main(
        "fun fact n = if n == 0 then 1 else n * fact (n - 1)\n\
         def result = fact (toBigInt 25)\n"
    ));
}

#[test]
fn radix_int_literals() {
    insta::assert_snapshot!(eval_expr("0xff + 0b1010 + 0o17"));
}

#[test]
fn float_arithmetic_and_precedence() {
    // `*` binds tighter than `+` on floats as on integers; unary minus
    // folds into the literal.
    insta::assert_snapshot!(eval_expr_std("2.0 * 3.0 + -1.5 / 3.0"));
}

#[test]
fn float_comparison_ops() {
    insta::assert_snapshot!(eval_expr_std("(1.5 < 2.0, 2.0 >= 2.0, 3.0 <= 1.0)"));
}

#[test]
fn float_int_conversions() {
    insta::assert_snapshot!(eval_expr_std("(toFloat 7 / 2.0, floor 3.9, floor (-2.1))"));
}
