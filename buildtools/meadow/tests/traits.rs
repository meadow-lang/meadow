//! Traits: what they mean on every machine, what inference works out for a
//! function nobody annotated, and what the checker says when one is misused.

mod common;
use meadow::{Engine, OptLevel, Options, pipeline, runtime};
use std::path::Path;

/// The CEK machine's answer, required of the VM and its JIT at every level
/// and of a release build, where dictionaries meet the specializer.
fn agreed(src: &str) -> String {
    let (program, diags) =
        pipeline::compile_str_with_std("test", src, Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "compile errors in\n{src}\n{}",
        diags
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let program = meadow_compiler::core::prune::prune(&program);
    let cek = runtime::run(&program, Engine::Cek, OptLevel::O1)
        .unwrap_or_else(|e| panic!("the CEK machine failed on\n{src}\n{e}"));
    for engine in [Engine::Vm, Engine::Jit] {
        for opt in [OptLevel::O0, OptLevel::O1, OptLevel::O2] {
            let got = runtime::run(&program, engine, opt)
                .unwrap_or_else(|e| panic!("{engine:?} at {} failed: {e}", opt.name()));
            assert_eq!(got, cek, "{engine:?} at {} on\n{src}", opt.name());
        }
    }
    let (release, diags) =
        pipeline::compile_str_with_std("test", src, Options::release().entry("result"));
    assert!(
        diags.is_empty(),
        "release: {:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let release = meadow_compiler::core::prune::prune(&release);
    let got = runtime::run(&release, Engine::Jit, OptLevel::O2).unwrap();
    assert_eq!(got, cek, "a release build on\n{src}");
    cek
}

fn is(src: &str, expected: &str) {
    assert_eq!(agreed(src), expected, "{src}");
}

fn errors(src: &str) -> String {
    common::errors_std_with(src, Options::debug().entry("result"))
}

const DESCRIBE: &str = "trait Describe a {\n\
    \x20 fun describe : a -> String\n\
    \x20 fun twice : a -> String\n\
    \x20   | twice x = describe x ++ describe x\n\
    }\n\
    impl Describe Int { fun describe n = \"int \" ++ show n }\n\
    impl Describe Bool {\n\
    \x20 fun describe b = if b then \"yes\" else \"no\"\n\
    \x20 fun twice b = \"bool!\"\n\
    }\n\
    impl Describe [a;] where Describe a {\n\
    \x20 fun describe xs = match xs with\n\
    \x20   | [;] -> \".\"\n\
    \x20   | x :: rest -> describe x ++ \",\" ++ describe rest\n\
    }\n\
    impl Describe (a, b) where Describe a, Describe b {\n\
    \x20 fun describe p = match p with | (x, y) -> describe x ++ \"+\" ++ describe y\n\
    }\n";

#[test]
fn a_method_is_the_impl_of_the_type_it_meets() {
    is(
        &format!(
            "{DESCRIBE}def result = (describe 3, describe True, describe [1; 2], describe (1, [True;]))\n"
        ),
        r#"("int 3", "yes", "int 1,int 2,.", "int 1+yes,.")"#,
    );
    // A default is what an `impl` that leaves the method out gets, and one
    // that defines it does not.
    is(
        &format!("{DESCRIBE}def result = (twice 4, twice False, twice [7;])\n"),
        r#"("int 4int 4", "bool!", "int 7,.int 7,.")"#,
    );
}

/// A dictionary known at a call from a function generic in some other type --
/// one it only passes along, or a `runSt`'s state, which no caller ever
/// knows -- is still the one the method is found in: a release build copies
/// the callee for it, generic in what is not known.
#[test]
fn a_known_dictionary_is_used_under_a_type_nobody_knows() {
    is(
        &format!(
            "{DESCRIBE}\
             use Std.St as St\n\
             fun tagged tag x = describe x\n\
             fun passing (tag : b) (n : Int) : String = tagged tag n\n\
             fun counted (r : StRef s Int) (n : Int) : String ! {{ St s | e }} =\n\
             \x20 let _ = St.modifyRef r (\\c -> c + 1) in tagged r (n, [True;])\n\
             def result = (passing \"s\" 3, passing False 4, runSt (\\() -> let r = St.newRef 0 in (counted r 5, St.getRef r)))\n"
        ),
        r#"("int 3", "int 4", ("int 5+yes,.", 1))"#,
    );
}

// --- a method and its default are one item ------------------------------------

/// A default belongs to the method it is a default *for*, so it is written in
/// clauses under the signature -- the same shape a top-level definition takes.
/// Naming the method a second time to define it is the Haskell habit Meadow
/// does not have.
#[test]
fn a_default_is_written_under_the_signature_it_belongs_to() {
    is(
        "trait Greet a {\n\
        \x20 fun name : a -> String\n\
        \x20 fun greet : a -> String\n\
        \x20   | greet x = \"hello, \" ++ name x\n\
        }\n\
        impl Greet Int { fun name n = show n }\n\
        impl Greet Bool {\n\
        \x20 fun name b = if b then \"yes\" else \"no\"\n\
        \x20 fun greet b = \"HI \" ++ name b\n\
        }\n\
        def result = (greet 1, greet True)\n",
        r#"("hello, 1", "HI yes")"#,
    );
    // Several clauses, as any other definition may have.
    is(
        "trait Tag a {\n\
        \x20 fun show1 : a -> String\n\
        \x20 fun tag : a -> Bool -> String\n\
        \x20   | tag x True  = \"!\" ++ show1 x\n\
        \x20   | tag x False = show1 x\n\
        }\n\
        impl Tag Int { fun show1 n = show n }\n\
        def result = (tag 1 True, tag 2 False)\n",
        r#"("!1", "2")"#,
    );
}

/// The method with no default at all -- most of what a trait holds -- is a
/// signature on its own, and stays one.
#[test]
fn a_method_without_a_default_is_still_a_signature_of_its_own() {
    is(
        "trait Size a { fun size : a -> Int }\n\
         impl Size [b;] where Size b { fun size xs = match xs with | [;] -> 0 | _ :: r -> 1 + size r }\n\
         impl Size Int { fun size n = n }\n\
         def result = size [1; 2; 3]\n",
        "3",
    );
}

#[test]
fn a_method_named_twice_in_a_trait_is_reported() {
    let out = errors(
        "trait Greet a {\n\
        \x20 fun greet : a -> String\n\
        \x20 fun greet x = \"hi\"\n\
        }\n\
        impl Greet Int {}\n\
        def result = greet 1\n",
    );
    assert!(
        out.contains(
            "`greet` is named twice in this trait -- a method and its default are one item, so \
             write the default as `| greet … = …` under the signature rather than declaring the \
             method again"
        ),
        "{out}"
    );
    // An operator method is named the way it is declared, in parentheses.
    let out = errors(
        "trait Near a {\n\
        \x20 fun (~=) : a -> a -> Bool\n\
        \x20 fun (~=) x y = False\n\
        }\n\
        def result = 1\n",
    );
    assert!(out.contains("`(~=)` is named twice in this trait"), "{out}");
    // A clause that names another method is caught against the signature.
    let out = errors(
        "trait Pair a {\n\
        \x20 fun left : a -> Int\n\
        \x20 fun right : a -> Int\n\
        \x20   | left x = 0\n\
        }\n\
        def result = 1\n",
    );
    assert!(
        out.contains("this equation defines `left`, but the signature above it is for `right`"),
        "{out}"
    );
}

#[test]
fn a_function_asks_for_what_its_body_needs() {
    // Nobody wrote a signature: the `where` is inferred, and printed.
    let schemes = common::schemes_std(&format!(
        "{DESCRIBE}fun both x y = describe x ++ \" & \" ++ describe y\n\
         fun loud : Describe a => a -> String\n\
           | loud x = twice x ++ \"!\"\n"
    ));
    assert!(
        schemes.contains("both : forall a b. (Describe a, Describe b) => a -> b -> String"),
        "{schemes}"
    );
    assert!(
        schemes.contains("loud : forall a. Describe a => a -> String"),
        "{schemes}"
    );
    is(
        &format!(
            "{DESCRIBE}fun both x y = describe x ++ \" & \" ++ describe y\n\
             fun loud : Describe a => a -> String\n\
               | loud x = twice x ++ \"!\"\n\
             fun nested : Describe a => a -> String\n\
               | nested x = describe [(x, x); (x, x)]\n\
             def result = (both True [False;], loud [True;], nested 1)\n"
        ),
        r#"("yes & no,.", "yes,.yes,.!", "int 1+int 1,int 1+int 1,.")"#,
    );
}

/// Functions that call one another share what they ask for, and pass it on.
#[test]
fn mutual_recursion_passes_its_dictionaries_along() {
    is(
        &format!(
            "{DESCRIBE}fun evens n x = if n == 0 then describe x else odds (n - 1) x\n\
             fun odds n x = if n == 0 then \"odd \" ++ describe x else evens (n - 1) x\n\
             fun count n x = if n == 0 then describe x else count (n - 1) x\n\
             def result = (evens 4 True, odds 3 [1;], evens 3 2, count 5 (True, 1))\n"
        ),
        r#"("yes", "int 1,.", "odd int 2", "yes+int 1")"#,
    );
}

#[test]
fn a_trait_can_require_another() {
    let src = "trait Same a { fun same : a -> a -> Bool }\n\
        trait Ranked a <: Same a { fun before : a -> a -> Bool }\n\
        impl Same Int { fun same x y = x == y }\n\
        impl Ranked Int { fun before x y = x < y }\n\
        impl Same [a;] where Same a {\n\
        \x20 fun same xs ys = match (xs, ys) with\n\
        \x20   | ([;], [;]) -> True\n\
        \x20   | (x :: a, y :: b) -> same x y and same a b\n\
        \x20   | _ -> False\n\
        }\n\
        impl Ranked [a;] where Ranked a {\n\
        \x20 fun before xs ys = match (xs, ys) with\n\
        \x20   | (_, [;]) -> False\n\
        \x20   | ([;], _) -> True\n\
        \x20   | (x :: a, y :: b) -> before x y or (same x y and before a b)\n\
        }\n\
        fun atMost : Ranked a => a -> a -> Bool\n\
          | atMost x y = before x y or same x y\n\
        def result = (atMost 1 1, atMost 2 1, atMost [1; 2] [1; 3], atMost [2;] [1; 9])\n";
    is(src, "(True, False, True, False)");
    // An `impl` of the one needs an `impl` of the other.
    let out = errors(
        "trait Same a { fun same : a -> a -> Bool }\n\
         trait Ranked a <: Same a { fun before : a -> a -> Bool }\n\
         impl Ranked Int { fun before x y = x < y }\n\
         def result = before 1 2\n",
    );
    assert!(out.contains("`Int` does not implement `Same`"), "{out}");
}

#[test]
fn an_associated_type_is_the_impls_to_choose() {
    let src = "trait Container f {\n\
        \x20 type Elem f\n\
        \x20 fun empty : () -> f\n\
        \x20 fun insert : Elem f -> f -> f\n\
        \x20 fun toList : f -> [Elem f;]\n\
        }\n\
        record Bag a = { items : [a;] }\n\
        record Bits = { word : Int }\n\
        impl Container (Bag a) {\n\
        \x20 type Elem (Bag a) = a\n\
        \x20 fun empty u = Bag { items = [;] }\n\
        \x20 fun insert x b = Bag { items = x :: b.items }\n\
        \x20 fun toList b = b.items\n\
        }\n\
        impl Container Bits {\n\
        \x20 type Elem Bits = Bool\n\
        \x20 fun empty u = Bits { word = 1 }\n\
        \x20 fun insert x b = Bits { word = b.word * 2 + (if x then 1 else 0) }\n\
        \x20 fun toList b = if b.word <= 1 then [;] else (b.word % 2 == 1) :: toList (Bits { word = b.word / 2 })\n\
        }\n\
        fun fromList : Container f => [Elem f;] -> f\n\
          | fromList xs = match xs with\n\
        \x20 | [;] -> empty ()\n\
        \x20 | x :: rest -> insert x (fromList rest)\n\
        fun bag : [a;] -> Bag a\n\
          | bag xs = fromList xs\n\
        fun bits : [Bool;] -> Bits\n\
          | bits xs = fromList xs\n\
        def result = (toList (bag [\"a\"; \"b\"]), (bits [True; False; True]).word, toList (bits [False; True]))\n";
    is(src, r#"(["a"; "b"], 13, [False; True])"#);
    // What it is comes with the `impl`: a `Bits` holds `Bool`s and nothing else.
    let out = errors(&format!(
        "{}def wrong = insert 3 (bits [True;])\n",
        src.replace("def result", "def unused")
    ));
    assert!(out.contains("type mismatch"), "{out}");
}

#[test]
fn a_method_may_be_general_in_its_effects() {
    is(
        "trait Each f {\n\
         \x20 type Item f\n\
         \x20 fun each : (Item f -> () ! e) -> f -> () ! e\n\
         }\n\
         impl Each [a;] {\n\
         \x20 type Item [a;] = a\n\
         \x20 fun each f xs = match xs with\n\
         \x20   | [;] -> ()\n\
         \x20   | x :: rest -> let _ = f x in each f rest\n\
         }\n\
         use Std.St as St\n\
         fun total xs = runSt (\\() ->\n\
         \x20 let sum = St.newRef 0 in\n\
         \x20 let _ = each (\\x -> St.modifyRef sum (\\s -> s + x)) xs in\n\
         \x20 St.getRef sum)\n\
         def result = total [1; 2; 3; 4]\n",
        "10",
    );
}

#[test]
fn what_is_wrong_is_said() {
    let with = |rest: &str| errors(&format!("{DESCRIBE}{rest}"));
    let out = with("def result = describe \"x\"\n");
    assert!(
        out.contains("`String` does not implement `Describe`"),
        "{out}"
    );
    let out = with("fun f : a -> String\n  | f x = describe x\ndef result = f 1\n");
    assert!(
        out.contains("this needs `Describe a`, which the signature does not ask for"),
        "{out}"
    );
    let out = with("impl Describe Int { fun describe n = \"again\" }\ndef result = 1\n");
    assert!(
        out.contains("`Describe` is already implemented for `Int`"),
        "{out}"
    );
    let out = with("impl Describe String { }\ndef result = 1\n");
    assert!(
        out.contains("this `impl Describe` is missing `describe`"),
        "{out}"
    );
    let out =
        with("impl Describe String { fun describe s = s  fun other s = s }\ndef result = 1\n");
    assert!(
        out.contains("`other` is not a method of `Describe`"),
        "{out}"
    );
    let out = with("impl Describe String { fun describe s = 5 }\ndef result = 1\n");
    assert!(out.contains("type mismatch"), "{out}");
    let out = with("impl Missing Int { }\ndef result = 1\n");
    assert!(out.contains("unknown trait `Missing`"), "{out}");
    let out = with("impl Maybe Int { }\ndef result = 1\n");
    assert!(out.contains("`Maybe` is a type, not a trait"), "{out}");
    let out = with("impl Describe (Maybe Int) { fun describe m = \"m\" }\ndef result = 1\n");
    assert!(
        out.contains("a type constructor applied to distinct variables"),
        "{out}"
    );
    let out = errors("trait Conv a { fun conv : a -> b -> b }\ndef result = 1\n");
    assert!(out.contains("has a type variable of its own"), "{out}");
    // Nothing says which `impl`: the value is made and thrown away.
    let out = errors(
        "trait Make a { fun make : () -> a }\nimpl Make Int { fun make u = 1 }\n\
         def result = let _ = make () in 0\n",
    );
    assert!(
        out.contains("cannot tell which `impl Make` is meant"),
        "{out}"
    );
}

// --- across modules and packages ---------------------------------------------

#[test]
fn a_trait_is_used_across_modules() {
    let out = common::eval_unit(&[
        (
            "Shape",
            "@pub(pkg) trait Area a { fun area : a -> Int }\n\
             @pub(pkg) record Square = { side : Int }\n\
             impl Area Square { fun area s = s.side * s.side }\n",
        ),
        (
            "Round",
            "use Shape (Area, area)\n\
             @pub(pkg) record Circle = { r : Int }\n\
             impl Area Circle { fun area c = 3 * c.r * c.r }\n\
             @pub(pkg) fun double x = 2 * area x\n",
        ),
        (
            "",
            "use Shape (Square, area)\nuse Round (Circle, double)\n\
             def result = (area (Square { side = 3 }), double (Circle { r = 2 }), double (Square { side = 1 }))\n",
        ),
    ]);
    assert_eq!(out, "(9, 24, 2)");
}

fn scratch(who: &str) -> meadow_scratch::Scratch {
    let dir = meadow_scratch::scratch!("meadow-traits-{}-{who}", std::process::id());
    dir.canonical()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A dependent implements a package's trait for its own type, uses the
/// package's `impl`s and its bounded functions, and gets its defaults.
#[test]
fn a_trait_crosses_the_package() {
    let root = scratch("pkg");
    write(
        &root.join("Pretty/Meadow.toml"),
        "[package]\nname = \"Pretty\"\nversion = \"0.1.0\"\n",
    );
    write(
        &root.join("Pretty/src/Lib.mw"),
        "@pub trait Pretty a {\n\
         \x20 fun pretty : a -> String\n\
         \x20 fun framed : a -> String\n\
         \x20   | framed x = \"[\" ++ pretty x ++ \"]\"\n\
         }\n\
         impl Pretty Int { fun pretty n = show n }\n\
         impl Pretty [a;] where Pretty a {\n\
         \x20 fun pretty xs = match xs with | [;] -> \"\" | x :: r -> pretty x ++ \";\" ++ pretty r\n\
         }\n\
         @pub fun shout x = framed x ++ \"!\"\n",
    );
    write(
        &root.join("App/Meadow.toml"),
        "[package]\nname = \"App\"\nversion = \"0.1.0\"\n\n[dependencies]\nPretty = { path = \"../Pretty\" }\n",
    );
    write(
        &root.join("App/src/Main.mw"),
        "use Pretty (Pretty, pretty, framed, shout)\n\
         record Point = { x : Int, y : Int }\n\
         impl Pretty Point { fun pretty p = pretty p.x ++ \"/\" ++ pretty p.y }\n\
         def result = (pretty [1; 2], framed (Point { x = 1, y = 2 }), shout [Point { x = 3, y = 4 };])\n",
    );
    for opts in [
        Options::debug().entry("result"),
        Options::release().entry("result"),
    ] {
        let out = pipeline::build(&root.join("App"), opts);
        assert!(
            out.diagnostics.is_empty(),
            "{:?}",
            out.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
        let program = out.linked.unwrap().program;
        let want = r#"("1;2;", "[1/2]", "[3/4;]!")"#;
        assert_eq!(meadow_eval::run(&program).unwrap().to_string(), want);
        assert_eq!(
            runtime::run(&program, Engine::Jit, OptLevel::O2).unwrap(),
            want
        );
    }
}

// --- several parameters --------------------------------------------------------

#[test]
fn a_trait_can_be_of_several_types() {
    let src = "trait Convert a b { fun convert : a -> b }\n\
        impl Convert Int String { fun convert n = \"#\" ++ show n }\n\
        impl Convert Int Bool { fun convert n = n != 0 }\n\
        impl Convert Bool Int { fun convert b = if b then 1 else 0 }\n\
        impl Convert [a;] [b;] where Convert a b {\n\
        \x20 fun convert xs = match xs with\n\
        \x20   | [;] -> [;]\n\
        \x20   | x :: rest -> convert x :: convert rest\n\
        }\n\
        trait Scale v s {\n\
        \x20 type Out v s\n\
        \x20 fun scale : s -> v -> Out v s\n\
        }\n\
        record V2 = { x : Int, y : Int }\n\
        impl Scale V2 Int {\n\
        \x20 type Out V2 Int = V2\n\
        \x20 fun scale k v = V2 { x = k * v.x, y = k * v.y }\n\
        }\n\
        impl Scale V2 Bool {\n\
        \x20 type Out V2 Bool = Int\n\
        \x20 fun scale b v = if b then v.x + v.y else 0\n\
        }\n\
        trait Same a { fun same : a -> a -> Bool }\n\
        impl Same Int { fun same x y = x == y }\n\
        trait RoundTrip a b <: Convert a b, Convert b a, Same a {\n\
        \x20 fun stable : a -> b -> Bool\n\
        }\n\
        impl RoundTrip Int Bool { fun stable n witness = same (there (back n witness)) n }\n\
        fun back : Convert a b => a -> b -> b\n\
          | back x witness = convert x\n\
        fun there : Convert b a => b -> a\n\
          | there y = convert y\n\
        fun strings : [Int;] -> [String;]\n\
          | strings xs = convert xs\n\
        fun both : (Convert a b, Convert a c) => a -> (b, c)\n\
          | both x = (convert x, convert x)\n\
        fun viaRound : RoundTrip a b => a -> b -> Bool\n\
          | viaRound x w = stable x w and same x x\n\
        fun pairOf : Int -> (String, Bool)\n\
          | pairOf n = both n\n\
        fun flag : Bool -> Int\n\
          | flag b = convert b\n\
        def result = (strings [1; 2], pairOf 0, flag True, scale 3 (V2 { x = 1, y = 2 }),\n\
        \x20 scale True (V2 { x = 1, y = 2 }), viaRound 1 True, viaRound 5 True)\n";
    is(
        src,
        r##"(["#1"; "#2"], ("#0", False), 1, V2(3, 6), 3, True, False)"##,
    );
    let out = errors(
        "trait Convert a b { fun convert : a -> b }\nimpl Convert Int { }\ndef result = 1\n",
    );
    assert!(
        out.contains("`Convert` is a trait of 2 types, given 1"),
        "{out}"
    );
    let out = errors(
        "trait Convert a b { fun convert : a -> b }\n\
         impl Convert Int String { fun convert n = show n }\n\
         fun f : Int -> Bool\n  | f n = convert n\ndef result = f 1\n",
    );
    assert!(
        out.contains("`Int Bool` does not implement `Convert`"),
        "{out}"
    );
}

// --- dictionaries, specialized away ----------------------------------------------

/// What a program reaches once its dictionaries are known takes none: the
/// copies are well typed, and the functions of dictionaries they were made
/// from are nowhere `main` can get to.
#[test]
fn known_dictionaries_are_specialized_away() {
    use meadow_compiler::core;
    let src = format!(
        "{DESCRIBE}fun loud : Describe a => a -> String\n\
           | loud x = twice x ++ \"!\"\n\
         fun all xs = match xs with | [;] -> \"\" | x :: rest -> loud x ++ all rest\n\
         def result = (all [1; 2], all [[True;]; [False;]], loud (1, [2;]))\n"
    );
    let (program, diags) =
        pipeline::compile_str_with_std("test", &src, Options::debug().entry("result"));
    assert!(
        diags.is_empty(),
        "{:?}",
        diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
    let program = core::prune::prune(&program);
    for opt in [OptLevel::O1, OptLevel::O2] {
        let copied = core::dictionaries::program(&program, opt);
        let problems = core::lint::check(&copied, &copied.variants, &Default::default());
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        let reached = core::prune::prune(&copied);
        let takes_one = |d: &core::Def| match &d.poly.ty {
            meadow_compiler::infer::Type::Fun(params, _, _) => matches!(
                &params[0],
                meadow_compiler::infer::Type::Con(n, _) if n.ends_with("Describe")
            ),
            _ => false,
        };
        let left: Vec<String> = reached
            .defs
            .iter()
            .filter(|d| takes_one(d))
            .map(|d| d.name.to_string())
            .collect();
        assert!(
            left.is_empty(),
            "at {}: still taking a dictionary: {left:?}",
            opt.name()
        );
        assert!(
            reached.defs.len() < copied.defs.len(),
            "the generic originals are unreachable"
        );
    }
}

// --- inherited associated types -------------------------------------------------

const STREAM: &str = r#"trait Stream s {
  type Token s
  fun take1 : s -> Int -> Maybe (Token s, Int)
}

impl Stream String {
  type Token String = Char
  fun take1 s i = if i < stringByteLength s then Just (charFromCode (toInt (stringByteAt s i)), i + 1) else None
}

impl Stream [t] {
  type Token [t] = t
  fun take1 v i = match get v i with | Just x -> Just (x, i + 1) | None -> None
}

trait Visual s <: Stream s {
  fun showToken : s -> Token s -> String
    | showToken _ t = "<" ++ show t ++ ">"
}

impl Visual String {}

impl Visual [t] where Display t {
  fun showToken _ t = "[${t}]"
}
"#;

#[test]
fn a_trait_requiring_one_with_associated_types_can_use_them() {
    // `Visual`'s method mentions `Stream`'s `Token s`, and a function given
    // `Visual s` reaches `Stream`'s methods with the same `Token s`.
    let src = format!(
        "{STREAM}
fun first : Visual s => s -> String
  | first s = match take1 s 0 with | Just (t, _) -> showToken s t | None -> \"empty\"

fun inferred s = match take1 s 1 with | Just (t, _) -> showToken s t | None -> \"empty\"

def result = (first \"xyz\", first [5, 6], inferred \"ab\", inferred [7, 8])
"
    );
    assert_eq!(
        common::eval_main_std(&src),
        r#"("<'x'>", "[5]", "<'b'>", "[8]")"#
    );
    assert_eq!(
        common::cek_main_std(&src),
        r#"("<'x'>", "[5]", "<'b'>", "[8]")"#
    );
}

#[test]
fn a_point_free_function_takes_the_dictionaries_its_type_needs() {
    // `fun f = e` at the top level is a function of its dictionaries, as a
    // Haskell binding with a context is -- not a value made once.
    let src = format!(
        "{STREAM}
fun second : Stream s => s -> Maybe (Token s, Int)
  | second = \\s -> take1 s 1

def result = (second \"ab\", second [1, 2])
"
    );
    assert_eq!(
        common::eval_main_std(&src),
        "(Just(('b', 2)), Just((2, 2)))"
    );
}

#[test]
fn a_point_free_function_still_may_not_perform_effects() {
    let errs = common::errors_std_with(
        "fun loud = let _ = println \"x\" in 1\ndef result = loud\n",
        meadow::Options::debug().entry("result"),
    );
    assert!(errs.contains("cannot perform effects"), "{errs}");
}

// --- associated effects --------------------------------------------------------
//
// What a method performs is each implementation's to say, as what an
// associated type is: `effect Reading r` in the trait, a row in each `impl`,
// and `! Reading r` where a method's type says what it performs.

const ROWS: &str = "\
data Done = Done #[Int]
data Writing s = Writing (StArray s Int)

trait Rows r {
  effect Reading r
  fun slot : r -> Int -> Int ! Reading r
}

impl Rows Done {
  effect Reading Done = {}
  fun slot r i = match r with | Done a -> arrayGet a i
}

impl Rows (Writing s) {
  effect Reading (Writing s) = { St s }
  fun slot r i = match r with | Writing a -> stGetArray a i
}

fun second rows = slot rows 1

fun sum3 rows = slot rows 0 + slot rows 1 + slot rows 2
";

#[test]
fn a_method_performs_what_its_impl_says_it_does() {
    // Pure at one type, a state's at another, and one generic function over
    // both: it performs whichever its rows' reading does.
    let src = format!(
        "{ROWS}
fun pureOne (d : Done) : Int = second d

def result =
  let d = Done #[10, 20, 30] in
  let w = runSt (\\() -> let a = stThaw #[1, 2, 3] in (second (Writing a), sum3 (Writing a))) in
  (pureOne d, sum3 d, w)
"
    );
    is(&src, "(20, 60, (2, 6))");
}

#[test]
fn a_pure_associated_effect_asks_nothing_of_where_it_is_used() {
    // Beside something that does perform: the call is not held to be all
    // the place does, nor the place to be pure.
    let src = format!(
        "{ROWS}
fun shown (d : Done) : String ! {{ Console | e }} =
  let _ = println (slot d 2) in
  \"${{sum3 d}}\"

def result = second (Done #[1, 2, 3])
"
    );
    is(&src, "2");
}

#[test]
fn a_generic_function_performs_its_associated_effect_and_what_else_it_does() {
    // Reading rows it knows nothing of, beside a cell it writes: its effect
    // is the state's and then whatever reading is, not reading alone.
    let src = format!(
        "{ROWS}
fun counted rows (seen : StRef t Int) =
  let u = stSetRef seen (stGetRef seen + 1) in
  slot rows 1

def result =
  runSt (\\() ->
    let seen = stNewRef 0 in
    let a = counted (Done #[1, 2, 3]) seen in
    let b = runSt (\\() -> counted (Writing (stThaw #[4, 5, 6])) (stNewRef 0)) in
    (a, b, stGetRef seen))
"
    );
    is(&src, "(2, 5, 1)");
}

#[test]
fn what_an_associated_effect_is_cannot_be_left_out_or_made_up() {
    let missing = errors(
        "trait Rows r {\n  effect Reading r\n  fun slot : r -> Int ! Reading r\n}\n\
         impl Rows Int { fun slot r = r }\ndef result = slot 1\n",
    );
    assert!(
        missing.contains("does not say what `Reading` is"),
        "{missing}"
    );
    let unknown = errors(
        "trait Rows r {\n  fun slot : r -> Int\n}\n\
         impl Rows Int {\n  effect Reading Int = {}\n  fun slot r = r\n}\ndef result = slot 1\n",
    );
    assert!(
        unknown.contains("`Reading` is not an associated effect of `Rows`"),
        "{unknown}"
    );
    let of_other =
        errors("trait Rows r {\n  effect Reading s\n  fun slot : r -> Int\n}\ndef result = 1\n");
    assert!(
        of_other.contains("an associated effect is of the trait's parameters"),
        "{of_other}"
    );
}

#[test]
fn an_associated_effect_is_the_rest_of_its_row() {
    // With effects named before it; not with a variable after, which would
    // be a second rest.
    let both = errors(
        "trait Rows r {\n  effect Reading r\n  fun slot : r -> Int ! { Reading r | e }\n}\n\
         def result = 1\n",
    );
    assert!(
        both.contains("an associated effect, which is the rest of a row"),
        "{both}"
    );
    let src = "\
trait Logs r {
  effect Also r
  fun note : r -> String -> () ! { Console, Also r }
}

impl Logs Int {
  effect Also Int = {}
  fun note n text = println \"${n}: ${text}\"
}

def result = 7
";
    is(src, "7");
}

// --- a parameter that is an effect --------------------------------------------
//
// The same thing said the other way: what a method performs is one of the
// trait's parameters, and an `impl` is given a row for it.

const ROWS_OVER: &str = "\
data Done = Done #[Int]
data Writing s = Writing (StArray s Int)

trait Rows r e {
  fun slot : r -> Int -> Int ! e
}

impl Rows Done e {
  fun slot r i = match r with | Done a -> arrayGet a i
}

impl Rows (Writing s) { St s | e } {
  fun slot r i = match r with | Writing a -> stGetArray a i
}

fun second rows = slot rows 1

fun sum3 rows = slot rows 0 + slot rows 1 + slot rows 2
";

#[test]
fn a_trait_can_be_of_an_effect_which_an_impl_gives_a_row_for() {
    // The `impl` is found by the type alone, and says what the effect is:
    // anything for `Done`, the state's and anything else for `Writing s`.
    let src = format!(
        "{ROWS_OVER}
fun pureOne (d : Done) : Int = second d

def result =
  let d = Done #[10, 20, 30] in
  let w = runSt (\\() -> let a = stThaw #[1, 2, 3] in (second (Writing a), sum3 (Writing a))) in
  (pureOne d, sum3 d, w)
"
    );
    is(&src, "(20, 60, (2, 6))");
}

#[test]
fn a_row_given_to_a_trait_is_told_from_the_body_after_it() {
    // Both are in braces. In a `where` too, and with nothing in it.
    let src = "\
trait Runs a e {
  fun go : a -> Int ! e
}

impl Runs Int {} {
  fun go n = n + 1
}

impl Runs Bool { Console | e } {
  fun go b = let _ = println \"going\" in if b then 1 else 0
}

impl Runs (Maybe a) e where Runs a e {
  fun go m = match m with | Just x -> go x | None -> 0
}

def result = (go 41, go (Just 6))
";
    is(src, "(42, 7)");
}

#[test]
fn a_trait_at_a_type_made_in_a_run_st_is_answered_inside_it() {
    // The `impl` found is over variables of its own, which have to be made
    // while the state is still this `runSt`'s.
    let src = "\
data Cell s = Cell (StRef s Int)

trait Peek c {
  fun label : c -> String
}

impl Peek (Cell s) {
  fun label c = \"a cell\"
}

fun named x = label x

def result = runSt (\\() -> let c = Cell (stNewRef 1) in named c)
";
    is(src, "\"a cell\"");
}
