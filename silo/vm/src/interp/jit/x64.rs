//! **The x86-64 instructions tier 1 writes**, encoded: moves, the integer
//! arithmetic and compares, calls and jumps, and labels resolved when the
//! function is done. Only what `super` asks for is here.

/// A register's number, as the encoding has it.
pub(super) type Reg = u8;

pub(super) const RAX: Reg = 0;
pub(super) const RCX: Reg = 1;
pub(super) const RDX: Reg = 2;
pub(super) const RBX: Reg = 3;
pub(super) const RSP: Reg = 4;
#[cfg(not(windows))]
pub(super) const RSI: Reg = 6;
#[cfg(not(windows))]
pub(super) const RDI: Reg = 7;
pub(super) const R11: Reg = 11;
pub(super) const R12: Reg = 12;

/// The first two arguments of a call in the C convention, which differs
/// between Windows and everything else.
#[cfg(windows)]
pub(super) const ARG: [Reg; 2] = [RCX, RDX];
#[cfg(not(windows))]
pub(super) const ARG: [Reg; 2] = [RDI, RSI];

/// A condition, as `jcc` and `setcc` take it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Cc {
    E = 0x4,
    Ne = 0x5,
    L = 0xC,
    Ge = 0xD,
    Le = 0xE,
    G = 0xF,
    S = 0x8,
    B = 0x2,
    Ae = 0x3,
    A = 0x7,
}

/// A scalar double arithmetic operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Sse {
    Add = 0x58,
    Mul = 0x59,
    Sub = 0x5C,
    Div = 0x5E,
}

/// A place in the code, bound once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Label(usize);

/// An integer operation of the form `op dst, src`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Alu {
    Add = 0x03,
    Sub = 0x2B,
    And = 0x23,
    Or = 0x0B,
    Xor = 0x33,
    Cmp = 0x3B,
}

#[derive(Default)]
pub(super) struct Asm {
    pub(super) code: Vec<u8>,
    labels: Vec<Option<usize>>,
    /// A 32-bit field at an offset, to be made the distance to a label from
    /// the end of the field.
    rel: Vec<(usize, Label)>,
    /// A 32-bit field at an offset, to be made the distance from the first
    /// label to the second.
    diff: Vec<(usize, Label, Label)>,
}

impl Asm {
    pub(super) fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub(super) fn bind(&mut self, l: Label) {
        debug_assert!(self.labels[l.0].is_none(), "a label bound twice");
        self.labels[l.0] = Some(self.code.len());
    }

    /// The code, every label filled in.
    pub(super) fn finish(mut self) -> Vec<u8> {
        let at =
            |l: Label, labels: &[Option<usize>]| labels[l.0].expect("a label used and never bound");
        for &(pos, l) in &self.rel {
            let d = at(l, &self.labels) as i64 - (pos as i64 + 4);
            self.code[pos..pos + 4].copy_from_slice(&(d as i32).to_le_bytes());
        }
        for &(pos, from, to) in &self.diff {
            let d = at(to, &self.labels) as i64 - at(from, &self.labels) as i64;
            self.code[pos..pos + 4].copy_from_slice(&(d as i32).to_le_bytes());
        }
        self.code
    }

    fn byte(&mut self, b: u8) {
        self.code.push(b);
    }

    fn u32(&mut self, v: u32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    /// A REX prefix: 64-bit if `w`, and the high bits of the registers.
    fn rex(&mut self, w: bool, reg: Reg, base: Reg) {
        let r = 0x40 | (u8::from(w) << 3) | ((reg >> 3) << 2) | (base >> 3);
        if r != 0x40 {
            self.byte(r);
        }
    }

    fn modrm(&mut self, md: u8, reg: Reg, rm: Reg) {
        self.byte((md << 6) | ((reg & 7) << 3) | (rm & 7));
    }

    /// `[base + disp]` as the operand of `reg`, with a 32-bit displacement.
    fn mem(&mut self, reg: Reg, base: Reg, disp: i32) {
        self.modrm(0b10, reg, base);
        if base & 7 == RSP {
            self.byte(0x24);
        }
        self.u32(disp as u32);
    }

    pub(super) fn push(&mut self, r: Reg) {
        self.rex(false, 0, r);
        self.byte(0x50 + (r & 7));
    }

    pub(super) fn pop(&mut self, r: Reg) {
        self.rex(false, 0, r);
        self.byte(0x58 + (r & 7));
    }

    pub(super) fn ret(&mut self) {
        self.byte(0xC3);
    }

    /// `mov dst, src`.
    pub(super) fn mov(&mut self, dst: Reg, src: Reg) {
        self.rex(true, src, dst);
        self.byte(0x89);
        self.modrm(0b11, src, dst);
    }

    /// `mov dst, imm`, at whatever width the value needs.
    pub(super) fn mov_imm(&mut self, dst: Reg, imm: u64) {
        if let Ok(v) = u32::try_from(imm) {
            // A 32-bit move zeroes the top half.
            self.rex(false, 0, dst);
            self.byte(0xB8 + (dst & 7));
            self.u32(v);
        } else {
            self.rex(true, 0, dst);
            self.byte(0xB8 + (dst & 7));
            self.code.extend_from_slice(&imm.to_le_bytes());
        }
    }

    /// `mov dst, [base + disp]`.
    pub(super) fn load(&mut self, dst: Reg, base: Reg, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x8B);
        self.mem(dst, base, disp);
    }

    /// `mov [base + disp], src`.
    pub(super) fn store(&mut self, base: Reg, disp: i32, src: Reg) {
        self.rex(true, src, base);
        self.byte(0x89);
        self.mem(src, base, disp);
    }

    /// `mov qword [base + disp], imm`, an immediate of 32 bits sign-extended.
    pub(super) fn store_imm(&mut self, base: Reg, disp: i32, imm: i32) {
        self.rex(true, 0, base);
        self.byte(0xC7);
        self.mem(0, base, disp);
        self.u32(imm as u32);
    }

    /// `cmp qword [base + disp], imm`, an immediate of 8 bits sign-extended.
    pub(super) fn cmp_mem_imm8(&mut self, base: Reg, disp: i32, imm: i8) {
        self.rex(true, 0, base);
        self.byte(0x83);
        self.mem(7, base, disp);
        self.byte(imm as u8);
    }

    /// `op dst, [base + disp]`.
    pub(super) fn alu_mem(&mut self, op: Alu, dst: Reg, base: Reg, disp: i32) {
        self.rex(true, dst, base);
        self.byte(op as u8);
        self.mem(dst, base, disp);
    }

    /// `op dst, src`.
    pub(super) fn alu(&mut self, op: Alu, dst: Reg, src: Reg) {
        self.rex(true, dst, src);
        self.byte(op as u8);
        self.modrm(0b11, dst, src);
    }

    /// `op dst, imm`, an immediate of 32 bits sign-extended.
    pub(super) fn alu_imm(&mut self, op: Alu, dst: Reg, imm: i32) {
        // The `81 /n` form: the operation's number is its opcode's middle.
        let n = match op {
            Alu::Add => 0,
            Alu::Or => 1,
            Alu::And => 4,
            Alu::Sub => 5,
            Alu::Xor => 6,
            Alu::Cmp => 7,
        };
        self.rex(true, 0, dst);
        self.byte(0x81);
        self.modrm(0b11, n, dst);
        self.u32(imm as u32);
    }

    /// `imul dst, [base + disp]`.
    pub(super) fn imul_mem(&mut self, dst: Reg, base: Reg, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x0F);
        self.byte(0xAF);
        self.mem(dst, base, disp);
    }

    /// `imul dst, src`.
    pub(super) fn imul(&mut self, dst: Reg, src: Reg) {
        self.rex(true, dst, src);
        self.byte(0x0F);
        self.byte(0xAF);
        self.modrm(0b11, dst, src);
    }

    /// `shl dst, cl` or, if `arith`, `sar dst, cl`.
    pub(super) fn shift_cl(&mut self, dst: Reg, arith: bool) {
        self.rex(true, 0, dst);
        self.byte(0xD3);
        self.modrm(0b11, if arith { 7 } else { 4 }, dst);
    }

    /// `setcc al; movzx eax, al`: the condition as 0 or 1 in `rax`.
    pub(super) fn set_rax(&mut self, cc: Cc) {
        self.byte(0x0F);
        self.byte(0x90 + cc as u8);
        self.byte(0xC0);
        self.byte(0x0F);
        self.byte(0xB6);
        self.byte(0xC0);
    }

    /// `shr r, n`.
    pub(super) fn shr_imm(&mut self, r: Reg, n: u8) {
        self.rex(true, 0, r);
        self.byte(0xC1);
        self.modrm(0b11, 5, r);
        self.byte(n);
    }

    /// `test r, imm`, an immediate of 32 bits sign-extended.
    pub(super) fn test_imm(&mut self, r: Reg, imm: i32) {
        self.rex(true, 0, r);
        self.byte(0xF7);
        self.modrm(0b11, 0, r);
        self.u32(imm as u32);
    }

    /// `[base + index * 8 + disp]` as the operand of `reg`.
    fn indexed(&mut self, reg: Reg, base: Reg, index: Reg, disp: i32) {
        debug_assert!(index & 7 != RSP || index >= 8, "rsp is no index");
        self.modrm(0b10, reg, 0b100);
        self.byte((0b11 << 6) | ((index & 7) << 3) | (base & 7));
        self.u32(disp as u32);
    }

    fn rex_indexed(&mut self, reg: Reg, base: Reg, index: Reg) {
        self.byte(0x48 | ((reg >> 3) << 2) | ((index >> 3) << 1) | (base >> 3));
    }

    /// `mov dst, [base + index * 8 + disp]`.
    pub(super) fn load_indexed(&mut self, dst: Reg, base: Reg, index: Reg, disp: i32) {
        self.rex_indexed(dst, base, index);
        self.byte(0x8B);
        self.indexed(dst, base, index, disp);
    }

    /// `mov [base + index * 8 + disp], src`.
    pub(super) fn store_indexed(&mut self, base: Reg, index: Reg, disp: i32, src: Reg) {
        self.rex_indexed(src, base, index);
        self.byte(0x89);
        self.indexed(src, base, index, disp);
    }

    /// The 32-bit word at `[base]`, as the operand of `reg`: for a block's
    /// count, which is the integer its address points at. `base` is one the
    /// encoding takes without a displacement or an index.
    fn at32(&mut self, reg: Reg, base: Reg) {
        debug_assert!(base & 7 != RSP && base & 7 != 5, "a base that needs more");
        self.rex(false, reg, base);
    }

    /// `mov dst32, dword [base]`.
    pub(super) fn load32(&mut self, dst: Reg, base: Reg) {
        self.at32(dst, base);
        self.byte(0x8B);
        self.modrm(0b00, dst, base);
    }

    /// `cmp dword [base], imm`.
    pub(super) fn cmp32_imm(&mut self, base: Reg, imm: u32) {
        self.at32(0, base);
        self.byte(0x81);
        self.modrm(0b00, 7, base);
        self.u32(imm);
    }

    /// `add dword [base], 1`, or `sub dword [base], 1` if `down`.
    pub(super) fn step32(&mut self, base: Reg, down: bool) {
        self.at32(0, base);
        self.byte(0x83);
        self.modrm(0b00, if down { 5 } else { 0 }, base);
        self.byte(1);
    }

    /// `test a32, b32`.
    pub(super) fn test32(&mut self, a: Reg, b: Reg) {
        self.rex(false, b, a);
        self.byte(0x85);
        self.modrm(0b11, b, a);
    }

    /// `test a, b`.
    pub(super) fn test(&mut self, a: Reg, b: Reg) {
        self.rex(true, b, a);
        self.byte(0x85);
        self.modrm(0b11, b, a);
    }

    /// `not r`.
    pub(super) fn not(&mut self, r: Reg) {
        self.rex(true, 0, r);
        self.byte(0xF7);
        self.modrm(0b11, 2, r);
    }

    /// `call r`.
    pub(super) fn call(&mut self, r: Reg) {
        self.rex(false, 0, r);
        self.byte(0xFF);
        self.modrm(0b11, 2, r);
    }

    /// `jmp r`.
    pub(super) fn jmp_reg(&mut self, r: Reg) {
        self.rex(false, 0, r);
        self.byte(0xFF);
        self.modrm(0b11, 4, r);
    }

    /// `jmp l`.
    pub(super) fn jmp(&mut self, l: Label) {
        self.byte(0xE9);
        self.rel.push((self.code.len(), l));
        self.u32(0);
    }

    /// `jcc l`.
    pub(super) fn jcc(&mut self, cc: Cc, l: Label) {
        self.byte(0x0F);
        self.byte(0x80 + cc as u8);
        self.rel.push((self.code.len(), l));
        self.u32(0);
    }

    /// `lea dst, [rip + l]`: the address of label `l`.
    pub(super) fn lea_label(&mut self, dst: Reg, l: Label) {
        self.rex(true, dst, 0);
        self.byte(0x8D);
        self.modrm(0b00, dst, 0b101);
        self.rel.push((self.code.len(), l));
        self.u32(0);
    }

    /// `movsxd dst, dword [base + index * 4]`.
    pub(super) fn load_i32_indexed(&mut self, dst: Reg, base: Reg, index: Reg) {
        debug_assert!(
            base & 7 != 5,
            "a base the encoding would take for a displacement"
        );
        let r = 0x48 | ((dst >> 3) << 2) | ((index >> 3) << 1) | (base >> 3);
        self.byte(r);
        self.byte(0x63);
        self.modrm(0b00, dst, 0b100);
        self.byte((0b10 << 6) | ((index & 7) << 3) | (base & 7));
    }

    /// A 32-bit word in the code holding `to - from`: a jump table's entry.
    pub(super) fn offset(&mut self, from: Label, to: Label) {
        self.diff.push((self.code.len(), from, to));
        self.u32(0);
    }

    /// A scalar double operation, `F2 0F op`, between `xmm` and `[base +
    /// disp]`.
    fn sse_mem(&mut self, op: u8, xmm: u8, base: Reg, disp: i32) {
        self.byte(0xF2);
        self.rex(false, xmm, base);
        self.byte(0x0F);
        self.byte(op);
        self.mem(xmm, base, disp);
    }

    /// `movsd xmm, [base + disp]`.
    pub(super) fn load_f64(&mut self, xmm: u8, base: Reg, disp: i32) {
        self.sse_mem(0x10, xmm, base, disp);
    }

    /// `movsd [base + disp], xmm`.
    pub(super) fn store_f64(&mut self, base: Reg, disp: i32, xmm: u8) {
        self.sse_mem(0x11, xmm, base, disp);
    }

    /// `op xmm, [base + disp]`, for `op` one of [`Sse`]'s.
    pub(super) fn sse_op_mem(&mut self, op: Sse, xmm: u8, base: Reg, disp: i32) {
        self.sse_mem(op as u8, xmm, base, disp);
    }

    /// `op dst, src`, between two `xmm` registers.
    pub(super) fn sse_op(&mut self, op: Sse, dst: u8, src: u8) {
        self.byte(0xF2);
        self.rex(false, dst, src);
        self.byte(0x0F);
        self.byte(op as u8);
        self.modrm(0b11, dst, src);
    }

    /// `movq xmm, r`.
    pub(super) fn movq_to_xmm(&mut self, xmm: u8, r: Reg) {
        self.byte(0x66);
        self.rex(true, xmm, r);
        self.byte(0x0F);
        self.byte(0x6E);
        self.modrm(0b11, xmm, r);
    }

    /// Pad to a multiple of `n` bytes with `int3`.
    pub(super) fn align(&mut self, n: usize) {
        while self.code.len() % n != 0 {
            self.byte(0xCC);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_are_the_assembler_s() {
        let mut a = Asm::default();
        a.mov(RBX, RCX);
        a.load(RAX, R12, 24);
        a.store(RBX, 8, RAX);
        a.cmp_mem_imm8(R12, 16, 1);
        a.store_imm(R12, 32, 1);
        a.alu_mem(Alu::Add, RAX, R12, 40);
        a.imul_mem(RAX, R12, 8);
        a.mov_imm(RDX, 7);
        a.call(RAX);
        a.push(RBX);
        a.pop(R12);
        a.load_f64(0, R12, 8);
        a.sse_op_mem(Sse::Add, 0, R12, 24);
        a.store_f64(RBX, 16, 0);
        a.movq_to_xmm(1, RAX);
        a.sse_op(Sse::Mul, 0, 1);
        a.shr_imm(RDX, 32);
        a.test_imm(RAX, 1);
        a.load_indexed(RAX, RAX, RCX, 16);
        a.store_indexed(RAX, RCX, 16, RDX);
        a.load32(RDX, RAX);
        a.cmp32_imm(RAX, 1 << 29);
        a.step32(RAX, false);
        a.step32(RAX, true);
        a.test32(RDX, RDX);
        let code = a.finish();
        assert_eq!(
            code,
            [
                0x48, 0x89, 0xCB, // mov rbx, rcx
                0x49, 0x8B, 0x84, 0x24, 24, 0, 0, 0, // mov rax, [r12+24]
                0x48, 0x89, 0x83, 8, 0, 0, 0, // mov [rbx+8], rax
                0x49, 0x83, 0xBC, 0x24, 16, 0, 0, 0, 1, // cmp qword [r12+16], 1
                0x49, 0xC7, 0x84, 0x24, 32, 0, 0, 0, 1, 0, 0, 0, // mov qword [r12+32], 1
                0x49, 0x03, 0x84, 0x24, 40, 0, 0, 0, // add rax, [r12+40]
                0x49, 0x0F, 0xAF, 0x84, 0x24, 8, 0, 0, 0, // imul rax, [r12+8]
                0xBA, 7, 0, 0, 0, // mov edx, 7
                0xFF, 0xD0, // call rax
                0x53, // push rbx
                0x41, 0x5C, // pop r12
                0xF2, 0x41, 0x0F, 0x10, 0x84, 0x24, 8, 0, 0, 0, // movsd xmm0, [r12+8]
                0xF2, 0x41, 0x0F, 0x58, 0x84, 0x24, 24, 0, 0, 0, // addsd xmm0, [r12+24]
                0xF2, 0x0F, 0x11, 0x83, 16, 0, 0, 0, // movsd [rbx+16], xmm0
                0x66, 0x48, 0x0F, 0x6E, 0xC8, // movq xmm1, rax
                0xF2, 0x0F, 0x59, 0xC1, // mulsd xmm0, xmm1
                0x48, 0xC1, 0xEA, 32, // shr rdx, 32
                0x48, 0xF7, 0xC0, 1, 0, 0, 0, // test rax, 1
                0x48, 0x8B, 0x84, 0xC8, 16, 0, 0, 0, // mov rax, [rax+rcx*8+16]
                0x48, 0x89, 0x94, 0xC8, 16, 0, 0, 0, // mov [rax+rcx*8+16], rdx
                0x8B, 0x10, // mov edx, [rax]
                0x81, 0x38, 0, 0, 0, 0x20, // cmp dword [rax], 1 << 29
                0x83, 0x00, 1, // add dword [rax], 1
                0x83, 0x28, 1, // sub dword [rax], 1
                0x85, 0xD2, // test edx, edx
            ]
        );
    }

    #[test]
    fn labels_are_filled_in() {
        let mut a = Asm::default();
        let (top, end) = (a.label(), a.label());
        a.bind(top);
        a.jcc(Cc::E, end);
        a.jmp(top);
        a.bind(end);
        a.offset(top, end);
        let code = a.finish();
        // je +5 over the jmp; jmp back over both; the distance from top to
        // end, 11.
        assert_eq!(&code[..6], &[0x0F, 0x84, 5, 0, 0, 0]);
        assert_eq!(&code[6..11], &[0xE9, 0xF5, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&code[11..], &[11, 0, 0, 0]);
    }
}
