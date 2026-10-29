//! x86-64 (System V): instructions encoded by hand.
//!
//! Machine registers during a block function: `rbx` the machine, `r12` its
//! register file, `r13` steps not yet counted in the machine -- saved by the
//! callee, so they survive the calls into the interpreter -- `r14` the loop
//! iterations this call has made, and `rax`, `rcx`, `rdx`, `rsi`, `rdi`,
//! `xmm0` and `xmm1` for the instruction being done.
//!
//! `r8` to `r11` and `r15` hold bytecode registers `r0` to `r4`, in every
//! function alike -- the fixed registers -- so that one function goes on to
//! another with nothing moved, as on arm64: they are loaded from the register
//! file where the machine enters native code (the prologue, before the warm
//! entry), written to it before every call into the interpreter and every
//! return, and loaded again after every call. Nothing else touches them.

use super::{Emit, FloatOp, IntOp, Label, Operand, UnaryOp, layout, thin};
use meadow_bytecode::{Cond, Pc, Reg};
use meadow_rt::OptLevel;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RBX: u8 = 3;
const RSI: u8 = 6;
const R12: u8 = 12;
const R13: u8 = 13;

/// Where pinned registers live, in the order they are handed out.
const PIN_REGS: [u8; 5] = [8, 9, 10, 11, 15];

// Condition codes, as the low nibble of `jcc` and `setcc`.
const CC_BE: u8 = 0x6;
const CC_B: u8 = 0x2;
const CC_AE: u8 = 0x3;
const CC_E: u8 = 0x4;
const CC_NE: u8 = 0x5;
const CC_A: u8 = 0x7;
const CC_P: u8 = 0xA;
const CC_NP: u8 = 0xB;
const CC_L: u8 = 0xC;
const CC_GE: u8 = 0xD;
const CC_LE: u8 = 0xE;
const CC_G: u8 = 0xF;

pub struct Asm {
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    /// Where a 32-bit displacement to a label is, measured from the end of it.
    fixups: Vec<(usize, Label)>,
    ret_eax: Option<Label>,
    /// Writing a register leaves `live` to the caller (see `codegen::Book`).
    defer_live: bool,
    /// Constant operands encoded into the instruction, where they fit.
    short: bool,
    /// Pinned bytecode registers, and the machine register each lives in.
    pins: Vec<(Reg, u8)>,
    /// Go straight on to the next block's native code where there is some.
    chain: bool,
}

impl Asm {
    fn bytes(&mut self, b: &[u8]) {
        self.code.extend_from_slice(b);
    }

    fn rel32(&mut self, to: Label) {
        self.fixups.push((self.code.len(), to));
        self.bytes(&[0; 4]);
    }

    /// `REX.W` with the extension bits for `reg` and `base`.
    fn rex_w(&mut self, reg: u8, base: u8) {
        self.bytes(&[0x48 | (reg >> 3) << 2 | (base >> 3)]);
    }

    /// ModRM, and SIB where the base needs one, for `[base + disp]`.
    fn mem(&mut self, reg: u8, base: u8, disp: u32) {
        self.bytes(&[0x80 | (reg & 7) << 3 | (base & 7)]);
        if base & 7 == 4 {
            self.bytes(&[0x24]);
        }
        self.bytes(&disp.to_le_bytes());
    }

    /// `reg = [base + disp]`, 64 bits.
    fn load(&mut self, reg: u8, base: u8, disp: u32) {
        self.rex_w(reg, base);
        self.bytes(&[0x8B]);
        self.mem(reg, base, disp);
    }

    /// `[base + disp] = reg`, 64 bits.
    fn save(&mut self, reg: u8, base: u8, disp: u32) {
        self.rex_w(reg, base);
        self.bytes(&[0x89]);
        self.mem(reg, base, disp);
    }

    /// `dst = src`, 64 bits.
    fn mov_rr(&mut self, dst: u8, src: u8) {
        if dst != src {
            self.rex_w(src, dst);
            self.bytes(&[0x89, 0xC0 | (src & 7) << 3 | (dst & 7)]);
        }
    }

    /// `reg = imm`, 64 bits.
    fn imm(&mut self, reg: u8, imm: u64) {
        self.bytes(&[0x48 | (reg >> 3), 0xB8 + (reg & 7)]);
        self.bytes(&imm.to_le_bytes());
    }

    /// `[base + disp] = imm`, sign-extended from 32 bits.
    fn save_imm(&mut self, base: u8, disp: u32, imm: u32) {
        self.rex_w(0, base);
        self.bytes(&[0xC7]);
        self.mem(0, base, disp);
        self.bytes(&imm.to_le_bytes());
    }

    fn slot(r: u32) -> u32 {
        r * 8
    }

    /// The machine register bytecode register `r` is pinned to, if it is.
    fn pinned(&self, r: u32) -> Option<u8> {
        self.pins
            .iter()
            .find(|&&(p, _)| p as u32 == r)
            .map(|&(_, m)| m)
    }

    /// `reg = r[r]`
    fn get(&mut self, reg: u8, r: u32) {
        match self.pinned(r) {
            Some(m) => self.mov_rr(reg, m),
            None => self.load(reg, R12, Self::slot(r)),
        }
    }

    /// `r[r] = reg`
    fn put(&mut self, reg: u8, r: u32) {
        match self.pinned(r) {
            Some(m) => self.mov_rr(m, reg),
            None => self.save(reg, R12, Self::slot(r)),
        }
    }

    /// Every pinned register, to memory.
    fn spill(&mut self) {
        for (r, m) in self.pins.clone() {
            self.save(m, R12, Self::slot(r as u32));
        }
    }

    /// Every pinned register, from memory.
    fn reload(&mut self) {
        for (r, m) in self.pins.clone() {
            self.load(m, R12, Self::slot(r as u32));
        }
    }

    /// `rcx = c`
    fn operand(&mut self, c: Operand) {
        match c {
            Operand::Reg(r) => self.get(RCX, r as u32),
            Operand::Imm(n) => self.imm(RCX, n as u64),
        }
    }

    /// `c` as a sign-extended 32-bit immediate, where that is how it is
    /// encoded.
    fn short_imm(&self, c: Operand) -> Option<i32> {
        match c {
            Operand::Imm(n) if self.short => i32::try_from(n).ok(),
            _ => None,
        }
    }

    /// `cmp rax, c`
    fn cmp_rax(&mut self, c: Operand) {
        match self.short_imm(c) {
            Some(k) => {
                self.bytes(&[0x48, 0x3D]);
                self.bytes(&k.to_le_bytes());
            }
            None => {
                self.operand(c);
                self.bytes(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
            }
        }
    }

    fn raise(&mut self, n: u32) {
        self.load(RCX, RBX, layout::LIVE);
        self.bytes(&[0xBA]); // mov edx, n
        self.bytes(&n.to_le_bytes());
        self.bytes(&[0x48, 0x39, 0xD1]); // cmp rcx, rdx
        self.bytes(&[0x48, 0x0F, 0x42, 0xCA]); // cmovb rcx, rdx
        self.save(RCX, RBX, layout::LIVE);
    }

    /// `r[a] = rax`, raising `live`.
    fn store(&mut self, a: Reg) {
        self.put(RAX, a as u32);
        if !self.defer_live {
            self.raise(a as u32 + 1);
        }
    }

    fn flush_steps(&mut self) {
        // add [rbx + STEPS], r13
        self.rex_w(R13, RBX);
        self.bytes(&[0x01]);
        self.mem(R13, RBX, layout::STEPS);
        self.bytes(&[0x45, 0x31, 0xED]); // xor r13d, r13d
    }

    /// On to `out` if this call has made [`super::CHAINS`] jumps into other
    /// functions, counting this one.
    fn budget(&mut self, out: Label) {
        self.bytes(&[0x49, 0xFF, 0xC6]); // inc r14
        self.bytes(&[0x49, 0x81, 0xFE]); // cmp r14, CHAINS
        self.bytes(&super::CHAINS.to_le_bytes());
        self.jcc(CC_AE, out);
    }

    /// Jump to the warm entry of the function whose start is in `rax`.
    fn enter_warm(&mut self) {
        self.bytes(&[0x48, 0x83, 0xC0, Self::WARM as u8]); // add rax, WARM
        self.bytes(&[0xFF, 0xE0]); // jmp rax
    }

    /// `rcx = ` the method entry for the pc in `esi`, or on to `none` if it
    /// has none, or this call has made [`super::CHAINS`] jumps already. The
    /// table is there whenever native code runs at all. After it, the entry
    /// wants the arguments in `r[0..argc]`, in registers, and the object's
    /// first word's address in `rdx`; it loads the captures itself.
    fn method_entry(&mut self, none: Label) {
        self.load(RCX, RBX, layout::NATIVE_METHODS);
        self.bytes(&[0x48, 0x8B, 0x0C, 0xF1]); // mov rcx, [rcx + rsi*8]
        self.bytes(&[0x48, 0x85, 0xC9]); // test rcx, rcx
        self.jcc(CC_E, none);
        self.budget(none);
    }

    /// The `argc` arguments at `r[base]` down to `r[0..argc]`, lowest first:
    /// with `base` above zero every one moves below where it was, so none is
    /// overwritten before it is read. Through `rax`, which is all it touches
    /// besides the registers themselves.
    fn args_down(&mut self, base: Reg, argc: u32) {
        if base == 0 {
            return;
        }
        for j in 0..argc {
            let from = base as u32 + j;
            match (self.pinned(j), self.pinned(from)) {
                (Some(d), Some(s)) => self.mov_rr(d, s),
                (Some(d), None) => self.load(d, R12, Self::slot(from)),
                (None, _) => {
                    self.get(RAX, from);
                    self.save(RAX, R12, Self::slot(j));
                }
            }
        }
    }

    /// Return `status` with the register file as it is in memory.
    fn ret_as_is(&mut self, status: u32) {
        self.flush_steps();
        self.bytes(&[0xB8]); // mov eax, status
        self.bytes(&status.to_le_bytes());
        self.restore_and_return();
    }

    fn restore_and_return(&mut self) {
        self.bytes(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
        self.bytes(&[
            0x41, 0x5F, 0x41, 0x5E, 0x41, 0x5D, 0x41, 0x5C, 0x5B, 0x5D, 0xC3,
        ]);
    }

    fn jcc(&mut self, cc: u8, to: Label) {
        self.bytes(&[0x0F, 0x80 | cc]);
        self.rel32(to);
    }

    fn int_cc(c: Cond) -> u8 {
        match c {
            Cond::Eq => CC_E,
            Cond::Ne => CC_NE,
            Cond::Lt => CC_L,
            Cond::Le => CC_LE,
            Cond::Gt => CC_G,
            Cond::Ge => CC_GE,
        }
    }

    /// `al = cond(xmm0, xmm1)`, IEEE's way.
    fn float_bool(&mut self, cond: Cond) {
        const UCOMISD_0_1: [u8; 4] = [0x66, 0x0F, 0x2E, 0xC1];
        const UCOMISD_1_0: [u8; 4] = [0x66, 0x0F, 0x2E, 0xC8];
        let setcc = |a: &mut Asm, cc: u8, reg: u8| a.bytes(&[0x0F, 0x90 | cc, 0xC0 | reg]);
        match cond {
            // Unordered sets ZF, PF and CF together.
            Cond::Eq => {
                self.bytes(&UCOMISD_0_1);
                setcc(self, CC_E, RAX);
                setcc(self, CC_NP, RCX);
                self.bytes(&[0x20, 0xC8]); // and al, cl
            }
            Cond::Ne => {
                self.bytes(&UCOMISD_0_1);
                setcc(self, CC_NE, RAX);
                setcc(self, CC_P, RCX);
                self.bytes(&[0x08, 0xC8]); // or al, cl
            }
            Cond::Lt => {
                self.bytes(&UCOMISD_1_0);
                setcc(self, CC_A, RAX);
            }
            Cond::Le => {
                self.bytes(&UCOMISD_1_0);
                setcc(self, CC_AE, RAX);
            }
            Cond::Gt => {
                self.bytes(&UCOMISD_0_1);
                setcc(self, CC_A, RAX);
            }
            Cond::Ge => {
                self.bytes(&UCOMISD_0_1);
                setcc(self, CC_AE, RAX);
            }
        }
    }

    /// `rax = r[x]`, an object in either generation, and then `rdx` its first
    /// word's address, `rsi` that word, `ecx` its kind -- one in the nursery
    /// the short way, one in the old generation through the block tables.
    /// Only a compact region goes to `slow`.
    ///
    /// Everything that reads an object goes through here, as on arm64, and
    /// not only the calls. A long-lived closure is an old one -- a parser
    /// combinator defined at the top level, say -- and so is any data that
    /// has lived long enough to be promoted: a tag test or a field read of a
    /// tree that survived two collections used to go to the interpreter, and
    /// that was 35 million trips through `meadow_exec` in `binarytrees`.
    fn anywhere(&mut self, x: Reg, slow: Label) {
        let old = self.label();
        let found = self.label();
        self.get(RAX, x as u32);
        self.bytes(&[0x48, 0x3D]); // cmp rax, OLD_BASE
        self.bytes(&crate::old::OLD_BASE.to_le_bytes());
        self.jcc(CC_AE, old);
        self.load(2, RBX, layout::BASE); // rdx
        self.bytes(&[0x48, 0x8D, 0x14, 0xC2]); // lea rdx, [rdx + rax*8]
        self.jump(found);
        self.bind(old);
        // Past the generations, a region: bit 31 set.
        self.bytes(&[0x48, 0x89, 0xC6]); // mov rsi, rax
        self.bytes(&[0x48, 0xC1, 0xEE, 31]); // shr rsi, 31
        self.jcc(CC_NE, slow);
        self.thin_where();
        self.bind(found);
        self.bytes(&[0x48, 0x8B, 0x32]); // mov rsi, [rdx]
        self.bytes(&[0x40, 0x0F, 0xB6, 0xCE]); // movzx ecx, sil
    }

    /// `reg = ` a thin step's operand.
    fn thin_operand(&mut self, reg: u8, src: thin::Src) {
        match src {
            thin::Src::Reg(r) => self.get(reg, r as u32),
            thin::Src::Tmp(t) => self.load(reg, R12, Self::thin_slot(t)),
            thin::Src::Imm(w) => self.imm(reg, w),
        }
    }

    /// Temporary `t = reg`.
    fn thin_put(&mut self, t: u8, reg: u8) {
        self.save(reg, R12, Self::thin_slot(t));
    }

    /// Where temporary `t` of a run of steps lives: see [`Emit::steps`].
    fn thin_slot(t: u8) -> u32 {
        (crate::vm::SCRATCH as u32 + t as u32) * 8
    }

    /// `rax >>= n`, unsigned.
    fn thin_shr_rax(&mut self, n: u32) {
        debug_assert!(n < 64);
        if n != 0 {
            self.bytes(&[0x48, 0xC1, 0xE8, n as u8]); // shr rax, n
        }
    }

    /// `rdx = ` the slot address in `rax`, as a machine address, through the
    /// block tables: the generation's table, the block's first slot, and the
    /// slot's offset in it. `rax` is left alone; `rsi` is not.
    fn thin_where(&mut self) {
        use super::addr::*;
        self.bytes(&[0x48, 0x89, 0xC6]); // mov rsi, rax
        self.bytes(&[0x48, 0xC1, 0xEE, GEN_SHIFT as u8]); // shr rsi, 30
        self.bytes(&[0x48, 0x8B, 0x94, 0xF3]); // mov rdx, [rbx + rsi*8 + TABLES]
        self.bytes(&layout::TABLES.to_le_bytes());
        self.bytes(&[0x48, 0x89, 0xC6]); // mov rsi, rax
        self.bytes(&[0x48, 0xC1, 0xEE, BLOCK_SHIFT as u8]); // shr rsi, 13
        self.bytes(&[0x81, 0xE6]); // and esi, BLOCK_MASK
        self.bytes(&(BLOCK_MASK as u32).to_le_bytes());
        self.bytes(&[0x48, 0x8B, 0x14, 0xF2]); // mov rdx, [rdx + rsi*8]
        self.bytes(&[0x89, 0xC6]); // mov esi, eax
        self.bytes(&[0x81, 0xE6]); // and esi, SLOT_MASK
        self.bytes(&(SLOT_MASK as u32).to_le_bytes());
        self.bytes(&[0x48, 0x8D, 0x14, 0xF2]); // lea rdx, [rdx + rsi*8]
    }

    /// `rdi = rsi >> 32`: the object's length.
    fn length(&mut self) {
        self.bytes(&[0x48, 0x89, 0xF7, 0x48, 0xC1, 0xEF, 0x20]);
    }

    /// `xmm = r[r]`
    fn get_float(&mut self, xmm: u8, r: Reg) {
        match self.pinned(r as u32) {
            // movq xmm, m
            Some(m) => self.bytes(&[0x66, 0x48 | (m >> 3), 0x0F, 0x6E, 0xC0 | xmm << 3 | (m & 7)]),
            None => {
                self.bytes(&[0xF2, 0x41, 0x0F, 0x10]); // movsd xmm, [r12 + slot]
                self.mem(xmm, R12, Self::slot(r as u32));
            }
        }
    }

    /// `r[a] = xmm0`, raising `live`.
    fn store_float(&mut self, a: Reg) {
        match self.pinned(a as u32) {
            // movq m, xmm0
            Some(m) => self.bytes(&[0x66, 0x48 | (m >> 3), 0x0F, 0x7E, 0xC0 | (m & 7)]),
            None => {
                self.bytes(&[0xF2, 0x41, 0x0F, 0x11]); // movsd [r12 + slot], xmm0
                self.mem(0, R12, Self::slot(a as u32));
            }
        }
        if !self.defer_live {
            self.raise(a as u32 + 1);
        }
    }
}

impl Emit for Asm {
    const FIXED: usize = PIN_REGS.len();
    const WARM: usize = 33 + 8 * PIN_REGS.len();

    fn new() -> Asm {
        Asm {
            code: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
            ret_eax: None,
            defer_live: false,
            short: false,
            pins: Vec::new(),
            chain: false,
        }
    }

    fn offset(&self) -> usize {
        self.code.len()
    }

    fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    fn bind(&mut self, l: Label) {
        self.labels[l.0] = Some(self.code.len());
    }

    fn configure(&mut self, opt: OptLevel, _floats: &[Reg]) {
        self.defer_live = opt >= OptLevel::O1;
        self.short = opt >= OptLevel::O1;
        self.chain = opt >= OptLevel::O1;
        // The fixed registers, in general registers only: floats are not kept
        // apart here.
        self.pins = (0..PIN_REGS.len() as Reg).zip(PIN_REGS).collect();
    }

    fn prologue(&mut self, warm: Option<Label>) {
        let start = self.offset();
        // push rbp; mov rbp, rsp; push rbx; push r12; push r13; push r14;
        // push r15; sub rsp, 8 -- eight words on the stack with the return
        // address, so it stays aligned for the calls.
        self.bytes(&[
            0x55, 0x48, 0x89, 0xE5, 0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48,
            0x83, 0xEC, 0x08,
        ]);
        self.bytes(&[0x48, 0x89, 0xFB]); // mov rbx, rdi
        self.load(R12, RBX, layout::REGS);
        self.bytes(&[0x45, 0x31, 0xED]); // xor r13d, r13d
        self.bytes(&[0x45, 0x31, 0xF6]); // xor r14d, r14d
        self.reload();
        // A chained function arrives here, with the frame, the machine, its
        // register file, the fixed registers, the steps and the loop count of
        // the one it came from.
        assert_eq!(self.offset() - start, Self::WARM, "the warm entry moved");
        if let Some(warm) = warm {
            self.bind(warm);
        }
        self.ret_eax = None;
    }

    fn ret(&mut self, status: u32) {
        self.spill();
        self.ret_as_is(status);
    }

    fn chain(&mut self, pc: Pc, live: Option<u32>, to: Option<Label>) {
        if let Some(live) = live {
            self.save_imm(RBX, layout::LIVE, live);
        }
        // The fixed registers go on in their registers: written back only on
        // the way out to the machine.
        let out = self.label();
        self.budget(out);
        match to {
            Some(to) => self.jump(to),
            None => {
                self.load(RAX, RBX, layout::NATIVE_TABLE);
                self.bytes(&[0x48, 0x8B, 0x80]); // mov rax, [rax + pc*8]
                self.bytes(&(pc * 8).to_le_bytes());
                self.bytes(&[0x48, 0x85, 0xC0]); // test rax, rax
                self.jcc(CC_E, out);
                self.enter_warm();
            }
        }
        self.bind(out);
        self.spill();
        self.save_imm(RBX, layout::PC, pc);
        self.ret_as_is(crate::abi::JUMPED);
    }

    fn leave(&mut self, pc: Pc, live: Option<u32>) {
        self.save_imm(RBX, layout::PC, pc);
        if let Some(live) = live {
            self.save_imm(RBX, layout::LIVE, live);
        }
        self.ret(crate::abi::JUMPED);
    }

    fn end(&mut self) {
        if let Some(l) = self.ret_eax.take() {
            self.bind(l);
            self.restore_and_return();
        }
    }

    fn step(&mut self) {
        self.bytes(&[0x49, 0xFF, 0xC5]); // inc r13
    }

    fn unstep(&mut self) {
        self.bytes(&[0x49, 0xFF, 0xCD]); // dec r13
    }

    fn add_steps(&mut self, n: u32) {
        if n == 1 {
            self.step();
        } else {
            self.bytes(&[0x49, 0x81, 0xC5]); // add r13, n
            self.bytes(&n.to_le_bytes());
        }
    }

    /// A run of thin steps. This architecture has five registers to spare and
    /// a run may want five temporaries besides its operands, so a temporary
    /// lives in the scratch slots past the register file -- where `invoke`
    /// parks arguments, and which nothing reads between instructions -- and
    /// each step works in `rax`, `rcx` and `rdx`. A store and the load after
    /// it are forwarded, and either is a small fraction of the round trip
    /// through the interpreter this replaces.
    fn steps(&mut self, run: &[thin::Step], slow: Label) {
        use thin::Step;
        const RDX: u8 = 2;
        debug_assert!(thin::temporaries(run) <= thin::TEMPORARIES);
        for s in run {
            match *s {
                Step::Set(t, src) => {
                    self.thin_operand(RAX, src);
                    self.thin_put(t, RAX);
                }
                Step::Add(t, a, b) => {
                    self.thin_operand(RAX, a);
                    self.thin_operand(RCX, b);
                    self.bytes(&[0x48, 0x01, 0xC8]); // add rax, rcx
                    self.thin_put(t, RAX);
                }
                Step::And(t, a, mask) => {
                    self.thin_operand(RAX, a);
                    self.imm(RCX, mask);
                    self.bytes(&[0x48, 0x21, 0xC8]); // and rax, rcx
                    self.thin_put(t, RAX);
                }
                Step::Shr(t, a, n) => {
                    self.thin_operand(RAX, a);
                    self.thin_shr_rax(n);
                    self.thin_put(t, RAX);
                }
                Step::ShrBy(t, a, b) => {
                    self.thin_operand(RAX, a);
                    self.thin_operand(RCX, b);
                    self.bytes(&[0x48, 0xD3, 0xE8]); // shr rax, cl
                    self.thin_put(t, RAX);
                }
                Step::ShlBy(t, a, b) => {
                    self.thin_operand(RAX, a);
                    self.thin_operand(RCX, b);
                    self.bytes(&[0x48, 0xD3, 0xE0]); // shl rax, cl
                    self.thin_put(t, RAX);
                }
                Step::Xor(t, a, b) => {
                    self.thin_operand(RAX, a);
                    self.thin_operand(RCX, b);
                    self.bytes(&[0x48, 0x31, 0xC8]); // xor rax, rcx
                    self.thin_put(t, RAX);
                }
                Step::Load(t, at) => {
                    self.thin_operand(RAX, at);
                    self.thin_where();
                    self.bytes(&[0x48, 0x8B, 0x02]); // mov rax, [rdx]
                    self.thin_put(t, RAX);
                }
                Step::Store(at, v) => {
                    self.thin_operand(RAX, at);
                    self.thin_where();
                    self.thin_operand(RCX, v);
                    self.bytes(&[0x48, 0x89, 0x0A]); // mov [rdx], rcx
                }
                Step::Locate(t, at) => {
                    self.thin_operand(RAX, at);
                    self.thin_where();
                    self.thin_put(t, RDX);
                }
                Step::LoadAt(t, base, off) => {
                    self.thin_operand(RDX, base);
                    self.thin_operand(RCX, off);
                    self.bytes(&[0x48, 0x8B, 0x04, 0xCA]); // mov rax, [rdx + rcx*8]
                    self.thin_put(t, RAX);
                }
                Step::StoreAt(base, off, v) => {
                    self.thin_operand(RDX, base);
                    self.thin_operand(RCX, off);
                    self.thin_operand(RAX, v);
                    self.bytes(&[0x48, 0x89, 0x04, 0xCA]); // mov [rdx + rcx*8], rax
                }
                // Branch away on the *negation*: the guard holding is the
                // ordinary case and falls through. Unsigned throughout, which
                // is what makes one `u <` reject a negative index as well as
                // one past the end.
                Step::Guard(c, a, b) => {
                    self.thin_operand(RAX, a);
                    self.thin_operand(RCX, b);
                    self.bytes(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
                    let fails = match c {
                        Cond::Eq => CC_NE,
                        Cond::Ne => CC_E,
                        Cond::Lt => CC_AE,
                        Cond::Le => CC_A,
                        Cond::Gt => CC_BE,
                        Cond::Ge => CC_B,
                    };
                    self.jcc(fails, slow);
                }
                Step::Put(r, src) => {
                    self.thin_operand(RAX, src);
                    self.store(r);
                }
            }
        }
    }

    fn jump(&mut self, to: Label) {
        self.bytes(&[0xE9]);
        self.rel32(to);
    }

    fn mov(&mut self, a: Reg, b: Reg) {
        self.get(RAX, b as u32);
        self.store(a);
    }

    fn word(&mut self, a: Reg, w: u64) {
        match u32::try_from(w) {
            Ok(w) if self.short => {
                self.bytes(&[0xB8]); // mov eax, w
                self.bytes(&w.to_le_bytes());
            }
            _ => self.imm(RAX, w),
        }
        self.store(a);
    }

    fn int(&mut self, op: IntOp, a: Reg, b: Reg, c: Operand, zero: Label) {
        self.get(RAX, b as u32);
        match (op, self.short_imm(c)) {
            (IntOp::Add, Some(k)) => {
                self.bytes(&[0x48, 0x05]); // add rax, k
                self.bytes(&k.to_le_bytes());
            }
            (IntOp::Sub, Some(k)) => {
                self.bytes(&[0x48, 0x2D]); // sub rax, k
                self.bytes(&k.to_le_bytes());
            }
            (IntOp::Mul, Some(k)) => {
                self.bytes(&[0x48, 0x69, 0xC0]); // imul rax, rax, k
                self.bytes(&k.to_le_bytes());
            }
            _ => {
                self.operand(c);
                match op {
                    IntOp::Add => self.bytes(&[0x48, 0x01, 0xC8]), // add rax, rcx
                    IntOp::Sub => self.bytes(&[0x48, 0x29, 0xC8]), // sub rax, rcx
                    IntOp::Mul => self.bytes(&[0x48, 0x0F, 0xAF, 0xC1]), // imul rax, rcx
                    // The count has to be in `cl`, and `operand` put it in
                    // `rcx`. A 64-bit shift takes it modulo 64, as
                    // `wrapping_shl` does, so no range check is needed.
                    IntOp::Shl => self.bytes(&[0x48, 0xD3, 0xE0]), // shl rax, cl
                    IntOp::Shr => self.bytes(&[0x48, 0xD3, 0xF8]), // sar rax, cl
                    IntOp::Ushr => self.bytes(&[0x48, 0xD3, 0xE8]), // shr rax, cl
                    IntOp::And => self.bytes(&[0x48, 0x21, 0xC8]), // and rax, rcx
                    IntOp::Div | IntOp::Rem => {
                        self.bytes(&[0x48, 0x85, 0xC9]); // test rcx, rcx
                        self.jcc(CC_E, zero);
                        // `idiv` traps on MIN / -1, where `wrapping_div` gives
                        // MIN and `wrapping_rem` 0 -- which is `-x` and `0` for
                        // any `x`.
                        let normal = self.label();
                        let done = self.label();
                        self.bytes(&[0x48, 0x83, 0xF9, 0xFF]); // cmp rcx, -1
                        self.jcc(CC_NE, normal);
                        if op == IntOp::Div {
                            self.bytes(&[0x48, 0xF7, 0xD8]); // neg rax
                        } else {
                            self.bytes(&[0x31, 0xC0]); // xor eax, eax
                        }
                        self.jump(done);
                        self.bind(normal);
                        self.bytes(&[0x48, 0x99]); // cqo
                        self.bytes(&[0x48, 0xF7, 0xF9]); // idiv rcx
                        if op == IntOp::Rem {
                            self.bytes(&[0x48, 0x89, 0xD0]); // mov rax, rdx
                        }
                        self.bind(done);
                    }
                }
            }
        }
        self.store(a);
    }

    fn frame(&mut self, a: Reg, header: &[u64], fields: &[Reg], slow: Label) {
        let hdr = header.len() as u32;
        let n = fields.len() as u32;
        let size = hdr + n;
        // eax = fsp, ecx = fsp + size; over the chunk's end -- or no chunk yet,
        // when the end is zero -- is the interpreter's to sort out.
        self.bytes(&[0x8B, 0x83]); // mov eax, [rbx + FSP]
        self.bytes(&layout::FSP.to_le_bytes());
        self.bytes(&[0x8D, 0x88]); // lea ecx, [rax + size]
        self.bytes(&size.to_le_bytes());
        self.bytes(&[0x3B, 0x8B]); // cmp ecx, [rbx + FLIM]
        self.bytes(&layout::FLIM.to_le_bytes());
        self.jcc(CC_A, slow);
        // The frame's slot in memory: rdx = fbase + (fsp - fcur) * 8.
        self.bytes(&[0x8B, 0x93]); // mov edx, [rbx + FCUR]
        self.bytes(&layout::FCUR.to_le_bytes());
        self.bytes(&[0x48, 0x89, 0xC6]); // mov rsi, rax
        self.bytes(&[0x48, 0x29, 0xD6]); // sub rsi, rdx
        self.load(2, RBX, layout::FBASE); // rdx
        self.bytes(&[0x48, 0x8D, 0x14, 0xF2]); // lea rdx, [rdx + rsi*8]
        for (k, &w) in header.iter().enumerate() {
            self.imm(RSI, w);
            self.bytes(&[0x48, 0x89, 0xB2]); // mov [rdx + k * 8], rsi
            self.bytes(&(k as u32 * 8).to_le_bytes());
        }
        for (j, &r) in (0..).zip(fields) {
            self.get(RSI, r as u32);
            self.bytes(&[0x48, 0x89, 0xB2]); // mov [rdx + (hdr + j) * 8], rsi
            self.bytes(&((hdr + j) * 8).to_le_bytes());
        }
        self.bytes(&[0x89, 0x8B]); // mov [rbx + FSP], ecx
        self.bytes(&layout::FSP.to_le_bytes());
        self.store(a);
    }

    fn unary(&mut self, op: UnaryOp, a: Reg, b: Reg) {
        self.get(RAX, b as u32);
        match op {
            UnaryOp::ToFloat => {
                self.bytes(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]); // cvtsi2sd xmm0, rax
                self.store_float(a);
            }
            UnaryOp::PopCount => {
                self.bytes(&[0xF3, 0x48, 0x0F, 0xB8, 0xC0]); // popcnt rax, rax
                self.store(a);
            }
        }
    }

    fn float(&mut self, op: FloatOp, a: Reg, b: Reg, c: Reg) {
        self.get_float(0, b);
        let opcode = match op {
            FloatOp::Add => 0x58,
            FloatOp::Sub => 0x5C,
            FloatOp::Mul => 0x59,
            FloatOp::Div => 0x5E,
        };
        if self.pinned(c as u32).is_some() {
            self.get_float(1, c);
            self.bytes(&[0xF2, 0x0F, opcode, 0xC1]); // xmm0 op= xmm1
        } else {
            self.bytes(&[0xF2, 0x41, 0x0F, opcode]); // xmm0 op= [r12 + c]
            self.mem(0, R12, Self::slot(c as u32));
        }
        self.store_float(a);
    }

    fn cmp_int(&mut self, cond: Cond, a: Reg, b: Reg, c: Operand) {
        self.get(RAX, b as u32);
        self.cmp_rax(c);
        self.bytes(&[0x0F, 0x90 | Self::int_cc(cond), 0xC0]); // setcc al
        self.bytes(&[0x0F, 0xB6, 0xC0]); // movzx eax, al
        self.store(a);
    }

    fn cmp_float(&mut self, cond: Cond, a: Reg, b: Reg, c: Reg) {
        self.get_float(0, b);
        self.get_float(1, c);
        self.float_bool(cond);
        self.bytes(&[0x0F, 0xB6, 0xC0]); // movzx eax, al
        self.store(a);
    }

    fn branch_int(&mut self, cond: Cond, x: Reg, y: Operand, to: Label) {
        self.get(RAX, x as u32);
        self.cmp_rax(y);
        self.jcc(Self::int_cc(cond) ^ 1, to);
    }

    fn branch_float(&mut self, cond: Cond, x: Reg, y: Reg, to: Label) {
        self.get_float(0, x);
        self.get_float(1, y);
        self.float_bool(cond);
        self.bytes(&[0x84, 0xC0]); // test al, al
        self.jcc(CC_E, to);
    }

    fn branch_zero(&mut self, x: Reg, to: Label) {
        self.get(RAX, x as u32);
        self.bytes(&[0x48, 0x85, 0xC0]); // test rax, rax
        self.jcc(CC_E, to);
    }

    fn set_live(&mut self, n: u32) {
        self.save_imm(RBX, layout::LIVE, n);
    }

    fn raise_live_to(&mut self, n: u32) {
        self.raise(n);
    }

    fn back_edge(&mut self, to: Label, over: Label) {
        self.bytes(&[0x49, 0xFF, 0xC6]); // inc r14
        // cmp r14, BACK_EDGES
        self.bytes(&[0x49, 0x81, 0xFE]);
        self.bytes(&super::BACK_EDGES.to_le_bytes());
        self.jcc(CC_AE, over);
        self.jump(to);
    }

    fn tag_test(&mut self, x: Reg, tag: u32, miss: Label, slow: Label) {
        self.anywhere(x, slow);
        self.bytes(&[0x85, 0xC9]); // test ecx, ecx: Data
        self.jcc(CC_NE, miss);
        self.bytes(&[0x8B, 0x4A, 0x08]); // mov ecx, [rdx + 8]: meta
        self.bytes(&[0x81, 0xF9]); // cmp ecx, tag
        self.bytes(&tag.to_le_bytes());
        self.jcc(CC_NE, miss);
    }

    fn field(&mut self, a: Reg, b: Reg, i: u32, slow: Label) {
        self.anywhere(b, slow);
        self.bytes(&[0x83, 0xF9, crate::heap::Kind::Array as u8]); // cmp ecx, Array
        self.jcc(CC_A, slow);
        self.length();
        self.bytes(&[0x48, 0x81, 0xFF]); // cmp rdi, i
        self.bytes(&i.to_le_bytes());
        self.jcc(CC_BE, slow);
        // As on aarch64: a word of header more for every sixteen fields
        // past the eight the second word describes.
        let (two_words, wide, done) = (self.label(), self.label(), self.label());
        self.bytes(&[0xF7, 0xC6, 0x00, 0x10, 0x00, 0x00]); // test esi, UNIFORM
        self.jcc(CC_NE, two_words);
        self.bytes(&[0x48, 0x83, 0xFF, 0x08]); // cmp rdi, 8
        self.jcc(CC_A, wide);
        self.bind(two_words);
        self.bytes(&[0x48, 0x8B, 0x82]); // mov rax, [rdx + (2 + i) * 8]
        self.bytes(&((2 + i) * 8).to_le_bytes());
        self.store(a);
        self.jump(done);
        self.bind(wide);
        const {
            assert!(
                meadow_rt::compact::INLINE_DESCS == 8 && meadow_rt::compact::DESCS_PER_WORD == 16
            )
        };
        self.bytes(&[0x48, 0x89, 0xF9]); // mov rcx, rdi
        self.bytes(&[0x48, 0x83, 0xC1, 0x07]); // add rcx, 7
        self.bytes(&[0x48, 0xC1, 0xE9, 0x04]); // shr rcx, 4
        self.bytes(&[0x48, 0x8B, 0x84, 0xCA]); // mov rax, [rdx + rcx * 8 + (2 + i) * 8]
        self.bytes(&((2 + i) * 8).to_le_bytes());
        self.store(a);
        self.bind(done);
    }

    fn vector_loop(&mut self, _plan: &super::vector::Plan, _scalar: Label) {
        // Everything falls into the scalar loop.
    }

    fn stub(&mut self, captures: u32, params: u32, warm: Label) -> bool {
        let argc = params - captures;
        // The arguments up past the captures, highest first, so that none is
        // overwritten before it has moved.
        for j in (0..argc).rev() {
            self.mov_rr(PIN_REGS[(captures + j) as usize], PIN_REGS[j as usize]);
        }
        // The captures follow the header, which is longer than two words past
        // `compact::INLINE_DESCS` of them: see `object::write_header`.
        let hdr = meadow_rt::compact::header_slots(false, captures as usize) as u32;
        for i in 0..captures {
            self.load(PIN_REGS[i as usize], 2, (hdr + i) * 8); // [rdx + field i]
        }
        self.save_imm(RBX, layout::LIVE, params);
        self.jump(warm);
        true
    }

    fn invoke_known(
        &mut self,
        obj: Reg,
        base: Reg,
        argc: u32,
        g: super::Guess,
        generic: Label,
    ) -> bool {
        if argc > 247 || g.captures + argc > 255 {
            return false;
        }
        if g.known.frame {
            // A return through a frame in the current chunk whose return pc
            // is the one expected -- which also says how many captures it
            // holds, since a frame's pc is its method table's one method.
            self.get(RAX, obj as u32);
            self.bytes(&[0x8B, 0x8B]); // mov ecx, [rbx + FCUR]
            self.bytes(&layout::FCUR.to_le_bytes());
            self.bytes(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
            self.jcc(CC_B, generic);
            self.bytes(&[0x8B, 0x93]); // mov edx, [rbx + FLIM]
            self.bytes(&layout::FLIM.to_le_bytes());
            self.bytes(&[0x48, 0x39, 0xD0]); // cmp rax, rdx
            self.jcc(CC_AE, generic);
            self.bytes(&[0x48, 0x89, 0xC2]); // mov rdx, rax
            self.bytes(&[0x48, 0x29, 0xCA]); // sub rdx, rcx
            self.load(RSI, RBX, layout::FBASE);
            self.bytes(&[0x48, 0x8D, 0x14, 0xD6]); // lea rdx, [rsi + rdx*8]
            self.bytes(&[0x81, 0x7A, 0x08]); // cmp dword [rdx + 8], target
            self.bytes(&g.target.to_le_bytes());
            self.jcc(CC_NE, generic);
            // Returning pops it: the stack's top goes back to its slot. What
            // it holds stays where it is until something else is pushed.
            self.bytes(&[0x89, 0x83]); // mov [rbx + FSP], eax
            self.bytes(&layout::FSP.to_le_bytes());
        } else {
            // A closure made with the method table expected, which says how
            // many captures it holds.
            self.anywhere(obj, generic);
            self.bytes(&[0x83, 0xF9, crate::heap::Kind::Closure as u8]); // cmp ecx, Closure
            self.jcc(CC_NE, generic);
            self.bytes(&[0x81, 0x7A, 0x08]); // cmp dword [rdx + 8], table
            self.bytes(&g.known.meta.to_le_bytes());
            self.jcc(CC_NE, generic);
        }
        // The register file as the target wants it, with every count known:
        // the arguments out of the way, the captures in, the arguments after.
        // Through `put`, so a pinned register is written where it lives, and
        // `chain` writes them all back on the way out.
        let hdr = meadow_rt::compact::header_slots(false, g.captures as usize) as u32;
        let scratch = crate::vm::SCRATCH as u32;
        for j in 0..argc {
            self.get(RAX, base as u32 + j);
            self.save(RAX, R12, (scratch + j) * 8);
        }
        for c in 0..g.captures {
            self.load(RAX, 2, (hdr + c) * 8); // rax = [rdx + field c]
            self.put(RAX, c);
        }
        for j in 0..argc {
            self.load(RAX, R12, (scratch + j) * 8);
            self.put(RAX, g.captures + j);
        }
        self.chain(g.target, Some(g.captures + argc), g.to);
        true
    }

    fn invoke(&mut self, obj: Reg, method: u8, base: Reg, argc: u32, slow: Label) {
        if argc > 247 {
            self.jump(slow);
            return;
        }
        let object = self.label();
        let rebuild = self.label();
        let generic = self.label();
        let generic_frame = self.label();
        // A return: the object is in the current chunk of the frame stack.
        // Everything in a stack chunk is a frame, so its kind needs no test;
        // its slot is one load from the chunk's base; its return pc is its
        // `meta`, and its one method is #0, so there is no table to look up.
        // A frame in a chunk below has chunks to release on the way down,
        // which the interpreter does: it fails the kind test below.
        self.get(RAX, obj as u32);
        self.bytes(&[0x8B, 0x8B]); // mov ecx, [rbx + FCUR]
        self.bytes(&layout::FCUR.to_le_bytes());
        self.bytes(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
        self.jcc(CC_B, object);
        self.bytes(&[0x8B, 0x93]); // mov edx, [rbx + FLIM]
        self.bytes(&layout::FLIM.to_le_bytes());
        self.bytes(&[0x48, 0x39, 0xD0]); // cmp rax, rdx
        self.jcc(CC_AE, object);
        self.bytes(&[0x48, 0x89, 0xC2]); // mov rdx, rax
        self.bytes(&[0x48, 0x29, 0xCA]); // sub rdx, rcx
        self.load(RSI, RBX, layout::FBASE);
        self.bytes(&[0x48, 0x8D, 0x14, 0xD6]); // lea rdx, [rsi + rdx*8]
        self.bytes(&[0x8B, 0x72, 0x08]); // mov esi, [rdx + 8]
        if self.chain {
            // Returning pops the frame: the stack's top goes back to its
            // slot. Then on to the method entry, as for a call below, which
            // loads the captures from where the frame still is.
            self.method_entry(generic_frame);
            self.bytes(&[0x89, 0x83]); // mov [rbx + FSP], eax
            self.bytes(&layout::FSP.to_le_bytes());
            self.args_down(base, argc);
            self.bytes(&[0xFF, 0xE1]); // jmp rcx
        }
        // The general path wants the header and the length, which a return
        // to a method entry did not need.
        self.bind(generic_frame);
        self.bytes(&[0x48, 0x8B, 0x3A]); // mov rdi, [rdx]
        self.bytes(&[0x48, 0xC1, 0xEF, 0x20]); // shr rdi, 32: the length
        self.bytes(&[0x48, 0x83, 0xFF, 0x18]); // cmp rdi, 24
        self.jcc(CC_A, slow);
        self.bytes(&[0x89, 0x83]); // mov [rbx + FSP], eax
        self.bytes(&layout::FSP.to_le_bytes());
        self.jump(rebuild);
        // A call: a closure, wherever it is.
        self.bind(object);
        self.anywhere(obj, slow);
        self.bytes(&[0x83, 0xF9, crate::heap::Kind::Closure as u8]); // cmp ecx, Closure
        self.jcc(CC_NE, slow);
        self.length();
        self.bytes(&[0x48, 0x83, 0xFF, 0x18]); // cmp rdi, 24
        self.jcc(CC_A, slow);
        // The method's pc: table `meta`, entry `method`, if it has one.
        self.bytes(&[0x8B, 0x4A, 0x08]); // mov ecx, [rdx + 8]
        self.load(RSI, RBX, layout::METHOD_STARTS);
        self.bytes(&[0x8B, 0x04, 0x8E]); // mov eax, [rsi + rcx*4]
        self.bytes(&[0x8B, 0x4C, 0x8E, 0x04]); // mov ecx, [rsi + rcx*4 + 4]
        self.bytes(&[0x29, 0xC1]); // sub ecx, eax
        self.bytes(&[0x81, 0xF9]); // cmp ecx, method
        self.bytes(&(method as u32).to_le_bytes());
        self.jcc(CC_BE, slow);
        self.bytes(&[0x05]); // add eax, method
        self.bytes(&(method as u32).to_le_bytes());
        self.load(RSI, RBX, layout::METHOD_PCS);
        self.bytes(&[0x8B, 0x34, 0x86]); // mov esi, [rsi + rax*4]
        if self.chain {
            self.method_entry(generic);
            self.args_down(base, argc);
            self.bytes(&[0xFF, 0xE1]); // jmp rcx
        }
        self.bind(generic);
        // The general path: the register file rebuilt in memory, as the
        // interpreter does it, from `rdx` the object, `rdi` its length and
        // `esi` the method's pc.
        self.bind(rebuild);
        self.spill();
        // The captures follow a header of two words, and one more for every
        // sixteen fields past the eighth, whose descriptors no longer fit the
        // second (see `object::write_header`; a closure and a frame are never
        // uniform). Up to 24, that is one word more past eight.
        let two = self.label();
        self.bytes(&[0x48, 0x83, 0xFF, 0x08]); // cmp rdi, 8
        self.jcc(CC_BE, two);
        self.bytes(&[0x48, 0x83, 0xC2, 0x08]); // add rdx, 8
        self.bind(two);
        // The arguments out of the way, the captures in, the arguments after.
        let scratch = crate::vm::SCRATCH as u32;
        for j in 0..argc {
            self.load(RAX, R12, Self::slot(base as u32 + j));
            self.save(RAX, R12, (scratch + j) * 8);
        }
        let top = self.label();
        let done = self.label();
        self.bytes(&[0x31, 0xC9]); // xor ecx, ecx
        self.bind(top);
        self.bytes(&[0x48, 0x39, 0xF9]); // cmp rcx, rdi
        self.jcc(CC_AE, done);
        self.bytes(&[0x48, 0x8B, 0x44, 0xCA, 0x10]); // mov rax, [rdx + rcx*8 + 16]
        self.bytes(&[0x49, 0x89, 0x04, 0xCC]); // mov [r12 + rcx*8], rax
        self.bytes(&[0x48, 0xFF, 0xC1]); // inc rcx
        self.jump(top);
        self.bind(done);
        for j in 0..argc {
            self.load(RAX, R12, (scratch + j) * 8);
            self.bytes(&[0x49, 0x89, 0x84, 0xFC]); // mov [r12 + rdi*8 + j*8], rax
            self.bytes(&(j * 8).to_le_bytes());
        }
        self.bytes(&[0x48, 0x8D, 0x87]); // lea rax, [rdi + argc]
        self.bytes(&argc.to_le_bytes());
        self.save(RAX, RBX, layout::LIVE);
        if self.chain {
            let out = self.label();
            self.budget(out);
            self.load(RAX, RBX, layout::NATIVE_TABLE);
            self.bytes(&[0x48, 0x8B, 0x04, 0xF0]); // mov rax, [rax + rsi*8]
            self.bytes(&[0x48, 0x85, 0xC0]); // test rax, rax
            self.jcc(CC_E, out);
            // The register file was rebuilt in memory: the next function
            // expects the fixed registers in their registers.
            self.reload();
            self.enter_warm();
            self.bind(out);
        }
        self.save(RSI, RBX, layout::PC);
        self.ret_as_is(crate::abi::JUMPED);
    }

    fn alloc(&mut self, a: Reg, header: &[u64], fields: &[Reg], slow: Label) {
        let hdr = header.len() as u32;
        let n = fields.len() as u32;
        let size = hdr + n;
        self.load(RAX, RBX, layout::TOP);
        self.bytes(&[0x48, 0x8D, 0x88]); // lea rcx, [rax + size]
        self.bytes(&size.to_le_bytes());
        self.bytes(&[0x48, 0x3B, 0x8B]); // cmp rcx, [rbx + CAP]
        self.bytes(&layout::CAP.to_le_bytes());
        self.jcc(CC_A, slow);
        // A heap that has put enough into regions wants a collection first.
        self.load(2, RBX, layout::REGION_GROWTH); // rdx
        self.bytes(&[0x48, 0x3B, 0x93]); // cmp rdx, [rbx + CAP]
        self.bytes(&layout::CAP.to_le_bytes());
        self.jcc(CC_A, slow);
        self.load(2, RBX, layout::BASE);
        self.bytes(&[0x48, 0x8D, 0x14, 0xC2]); // lea rdx, [rdx + rax*8]
        for (k, &w) in header.iter().enumerate() {
            self.imm(RSI, w);
            self.bytes(&[0x48, 0x89, 0xB2]); // mov [rdx + k * 8], rsi
            self.bytes(&(k as u32 * 8).to_le_bytes());
        }
        for (j, &r) in (0..).zip(fields) {
            self.get(RSI, r as u32);
            self.bytes(&[0x48, 0x89, 0xB2]); // mov [rdx + (hdr + j) * 8], rsi
            self.bytes(&((hdr + j) * 8).to_le_bytes());
        }
        self.save(RCX, RBX, layout::TOP);
        self.bytes(&[0x48, 0x81, 0x83]); // add qword [rbx + ALLOCATED], size
        self.bytes(&layout::ALLOCATED.to_le_bytes());
        self.bytes(&size.to_le_bytes());
        self.store(a);
    }

    fn exec(&mut self, pc: Pc) {
        self.flush_steps();
        self.spill();
        self.bytes(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
        self.bytes(&[0xB8 + RSI]); // mov esi, pc
        self.bytes(&pc.to_le_bytes());
        self.bytes(&[0xFF]); // call [rbx + EXEC]
        self.mem(2, RBX, layout::EXEC);
        self.bytes(&[0x85, 0xC0]); // test eax, eax
        let ret = match self.ret_eax {
            Some(l) => l,
            None => {
                let l = self.label();
                self.ret_eax = Some(l);
                l
            }
        };
        self.jcc(CC_NE, ret);
        self.reload();
    }

    fn finish(mut self) -> Vec<u8> {
        for (at, l) in std::mem::take(&mut self.fixups) {
            let to = self.labels[l.0].expect("every label is bound");
            let delta = to as i64 - (at as i64 + 4);
            let delta = i32::try_from(delta).expect("a branch too far");
            self.code[at..at + 4].copy_from_slice(&delta.to_le_bytes());
        }
        self.code
    }
}
