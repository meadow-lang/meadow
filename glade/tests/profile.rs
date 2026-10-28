//! The profiler, against programs whose shape is known.
//!
//! The claim being tested is the one that is not obvious: the machine has no
//! call stack, and a sample can still see one, because the chain of
//! continuation objects *is* the call stack. If that walk is wrong the profile
//! is a plausible-looking lie, so these check the depth against programs whose
//! nesting is known by construction.

use meadow_compiler::{Options, compile_str_with, core};
use meadow_glade::profile::Profile;
use meadow_glade::vm::Vm;

const FUEL: u64 = 50_000_000;

/// Compile `src` to an image that carries debug info, which is what a profile
/// is walked by.
fn image(src: &str) -> meadow_bytecode::Program {
    let (pkg, diags) = compile_str_with("prof", src, Options::default().entry("result"));
    let hard: Vec<_> = diags.iter().map(|d| d.msg.clone()).collect();
    assert!(hard.is_empty(), "compile errors:\n{}", hard.join("\n"));
    let entry = pkg.value_entry;
    let program = core::Program {
        defs: pkg.defs.clone(),
        entry,
        ctor_fields: pkg.ctor_fields.clone(),
        variants: pkg.variants.clone(),
        origins: Default::default(),
        migrations: Vec::new(),
    };
    let lowered = meadow_seq::lower_program(&program, core::OptLevel::O1);
    assert!(lowered.unsupported.is_empty(), "{:?}", lowered.unsupported);
    meadow_codegen::compile_with_debug_info(&lowered.program).expect("codegen")
}

/// Run `src` under a profile taking a sample every `every` block entries.
fn profile_of(src: &str, every: u64, depth: usize) -> (Profile, String) {
    let image = image(src);
    let entry = image.entry.expect("an entry point");
    let mut vm = Vm::new(&image);
    vm.profile = Some(Box::new(Profile::new(every, depth)));
    let value = vm.run(entry, FUEL).expect("the program runs");
    let shown = vm.show(value);
    (*vm.profile.take().expect("the profile"), shown)
}

/// The deepest stack any sample saw.
fn deepest(p: &Profile) -> usize {
    p.stacks().map(|(s, _)| s.len()).max().unwrap_or(0)
}

/// Sampling does not change what a program answers, and does produce samples.
#[test]
fn a_profile_does_not_change_the_answer() {
    let src = "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
               def result = fib 18\n";
    let (p, shown) = profile_of(src, 50, 64);
    assert_eq!(shown, "2584");
    assert!(p.taken() > 0, "no samples taken");
}

/// The walk sees more than the frame it was taken in.
///
/// `fib 18` nests eighteen deep at its deepest, so a profile of it that only
/// ever sees one frame is a profile that is not walking anything.
#[test]
fn a_sample_sees_the_callers_below_it() {
    let src = "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
               def result = fib 18\n";
    let (p, _) = profile_of(src, 20, 64);
    assert!(
        deepest(&p) > 1,
        "every sample saw one frame; the chain was not walked"
    );
    assert!(
        p.shallow() < p.taken(),
        "{} of {} samples saw one frame",
        p.shallow(),
        p.taken()
    );
}

/// Deeper recursion, deeper stacks. The point of a flamegraph is that this
/// relationship holds; if it does not, the walk is finding something else.
#[test]
fn deeper_recursion_is_deeper_in_the_profile() {
    let shallow = "fun down (n : Int) : Int = if n <= 0 then 0 else 1 + down (n - 1)\n\
                   def result = down 4\n";
    let deep = "fun down (n : Int) : Int = if n <= 0 then 0 else 1 + down (n - 1)\n\
                def result = down 40\n";
    let (a, _) = profile_of(shallow, 1, 200);
    let (b, _) = profile_of(deep, 1, 200);
    assert!(
        deepest(&b) > deepest(&a),
        "40 deep saw {} frames, 4 deep saw {}",
        deepest(&b),
        deepest(&a)
    );
}

/// The depth asked for is a bound, and it is respected -- a chain can be as
/// long as a program is recursive, and a profile must not walk for ever.
#[test]
fn the_depth_asked_for_is_a_bound() {
    let src = "fun down (n : Int) : Int = if n <= 0 then 0 else 1 + down (n - 1)\n\
               def result = down 200\n";
    for depth in [2, 5, 17] {
        let (p, _) = profile_of(src, 1, depth);
        assert!(
            deepest(&p) <= depth,
            "asked for {depth}, saw {}",
            deepest(&p)
        );
    }
    // And the deep one still finds real frames rather than stopping at one.
    let (p, _) = profile_of(src, 1, 200);
    assert!(deepest(&p) > 17, "200 deep saw only {}", deepest(&p));
}

/// Every frame is somewhere in the program, not a word that happened to look
/// like one. A profile that points outside the code is worse than none.
#[test]
fn every_frame_is_an_instruction_of_the_program() {
    let src = "fun fib (n : Int) : Int = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\
               def result = fib 14\n";
    let image = image(src);
    let entry = image.entry.expect("an entry point");
    let mut vm = Vm::new(&image);
    vm.profile = Some(Box::new(Profile::new(10, 64)));
    vm.run(entry, FUEL).expect("runs");
    let p = vm.profile.take().expect("the profile");
    for (stack, _) in p.stacks() {
        for pc in stack {
            assert!(
                (*pc as usize) < image.code.len(),
                "pc {pc} is outside a program of {} instructions",
                image.code.len()
            );
        }
    }
}
