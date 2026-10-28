//! What an emitted module defines, for the runtime's own tests, which link
//! with no program: the constants at what a program that sets none of them
//! has, the tables empty, and the ways back into a program's code never
//! taken. A linker that resolves every symbol before it drops what nothing
//! reaches -- Windows' -- refuses the tests without them.

use crate::heap::Word;

#[unsafe(no_mangle)]
static mut meadow_threaded: u8 = 0;
#[unsafe(no_mangle)]
static mut meadow_cycles: u8 = 0;
#[unsafe(no_mangle)]
static mut meadow_gc: u8 = 0;
#[unsafe(no_mangle)]
static mut meadow_spill_words: i64 = 256;
#[unsafe(no_mangle)]
static mut meadow_result_desc: i64 = meadow_rt::desc::ANY;
#[unsafe(no_mangle)]
static meadow_ctor_names: [usize; 0] = [];
#[unsafe(no_mangle)]
static meadow_ctor_fields: [usize; 0] = [];
#[unsafe(no_mangle)]
static meadow_ctor_count: i64 = 0;
#[unsafe(no_mangle)]
static meadow_role_names: [usize; 0] = [];
#[unsafe(no_mangle)]
static meadow_sym_names: [usize; 0] = [];
#[unsafe(no_mangle)]
static meadow_sym_ranks: [i64; 0] = [];
#[unsafe(no_mangle)]
static meadow_sym_count: i64 = 0;

#[unsafe(no_mangle)]
extern "C" fn meadow_invoke1(_obj: Word, _arg: Word) -> Word {
    unreachable!("a test runs no program")
}

#[unsafe(no_mangle)]
extern "C" fn meadow_apply(_obj: Word, _arg: Word, _ev: Word) -> Word {
    unreachable!("a test runs no program")
}
