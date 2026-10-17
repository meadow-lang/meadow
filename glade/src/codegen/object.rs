//! Object files: native code and the image it runs, for a system linker.
//!
//! Two sections and two symbols, and no relocations: the code needs none (see
//! [`crate::codegen`]), and nothing in the data points anywhere. `meadow_code`
//! is the first byte of the code; `meadow_data` is laid out as
//!
//! ```text
//!   u64 image length   u64 block count   u64 offset of the block table
//!   u64 method entry count   u64 offset of the method entry table
//!   the image (see meadow_bytecode::image)
//!   the block table: (u32 entry pc, u32 offset into the code), per block
//!   the method entry table: the same, per method entry
//! ```
//!
//! which [`crate::aot::meadow_silo_main`] reads back. A program links one of
//! these, the runtime as a static library, and a `main` that hands the two
//! symbols to it.

use super::{Arch, Compiled};

/// An object file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// macOS.
    MachO,
    /// Linux, and the rest.
    Elf,
    /// Windows.
    Coff,
}

impl Format {
    /// What this process's platform links.
    pub fn host() -> Format {
        if cfg!(target_os = "macos") {
            Format::MachO
        } else if cfg!(windows) {
            Format::Coff
        } else {
            Format::Elf
        }
    }

    /// What an object file in this format is called.
    pub fn object_extension(self) -> &'static str {
        match self {
            Format::Coff => "obj",
            Format::MachO | Format::Elf => "o",
        }
    }

    /// What a shared library named `name` is called in this format.
    pub fn library_name(self, name: &str) -> String {
        match self {
            Format::Coff => format!("{name}.dll"),
            Format::MachO => format!("lib{name}.dylib"),
            Format::Elf => format!("lib{name}.so"),
        }
    }

    /// What an executable in this format is called after its name.
    pub fn exe_suffix(self) -> &'static str {
        match self {
            Format::Coff => ".exe",
            Format::MachO | Format::Elf => "",
        }
    }

    /// The system libraries the runtime library leans on, as this format's
    /// linker takes them: what `rustc --print native-static-libs` says, less
    /// what the C compiler links anyway. The Windows ones assume a `main`
    /// compiled against the DLL C runtime (`cl /MD`), as Rust's own `std` is.
    ///
    /// A native executable is for the machine this is running on, so which
    /// ELF system it is -- Linux or Android -- is which one this was built for.
    pub fn system_libs(self) -> &'static [&'static str] {
        self.system_libs_on(cfg!(target_os = "android"))
    }

    /// The same, saying whether an ELF target is Android: its C library is
    /// Bionic, which has in it what glibc splits into `pthread`, `rt` and
    /// `util`, and it has no `gcc_s` at all -- asking for one fails the link.
    /// What it has instead is `log` and `unwind`. Again what `rustc --print
    /// native-static-libs` says, for `aarch64-linux-android`.
    pub fn system_libs_on(self, android: bool) -> &'static [&'static str] {
        match self {
            Format::Elf if android => &["-ldl", "-llog", "-lunwind", "-ldl", "-lm", "-lc"],
            Format::MachO => &["-liconv", "-lSystem", "-lc", "-lm"],
            Format::Elf => &[
                "-lgcc_s",
                "-lutil",
                "-lrt",
                "-lpthread",
                "-lm",
                "-ldl",
                "-lc",
            ],
            Format::Coff => &[
                "legacy_stdio_definitions.lib",
                "kernel32.lib",
                "ntdll.lib",
                "userenv.lib",
                "ws2_32.lib",
                "dbghelp.lib",
            ],
        }
    }
}

/// The names, without the leading underscore Mach-O adds.
pub const CODE: &str = "meadow_code";
pub const DATA: &str = "meadow_data";

/// The data section: the image and the block table, as the module docs lay it
/// out.
pub fn data(compiled: &Compiled, image: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let table = align(40 + image.len(), 4);
    let stubs = table + 8 * compiled.blocks.len();
    out.extend_from_slice(&(image.len() as u64).to_le_bytes());
    out.extend_from_slice(&(compiled.blocks.len() as u64).to_le_bytes());
    out.extend_from_slice(&(table as u64).to_le_bytes());
    out.extend_from_slice(&(compiled.stubs.len() as u64).to_le_bytes());
    out.extend_from_slice(&(stubs as u64).to_le_bytes());
    out.extend_from_slice(image);
    out.resize(table, 0);
    for (pc, offset) in compiled.blocks.iter().chain(&compiled.stubs) {
        out.extend_from_slice(&pc.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out
}

/// An object file holding `compiled` and `image`.
pub fn write(compiled: &Compiled, image: &[u8], format: Format) -> Vec<u8> {
    let data = data(compiled, image);
    match format {
        Format::MachO => macho(compiled.arch, &compiled.code, &data),
        Format::Elf => elf(compiled.arch, &compiled.code, &data),
        Format::Coff => coff(compiled.arch, &compiled.code, &data),
    }
}

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// A fixed-width, zero-padded name.
    fn name(&mut self, s: &str, width: usize) {
        let mut b = s.as_bytes().to_vec();
        b.resize(width, 0);
        self.0.extend_from_slice(&b);
    }
    fn pad(&mut self, to: usize) {
        self.0.resize(align(self.0.len(), to), 0);
    }
}

/// A Mach-O `MH_OBJECT`: one segment with `__TEXT,__text` and
/// `__TEXT,__const`, a symbol table, and the platform it is for.
fn macho(arch: Arch, code: &[u8], data: &[u8]) -> Vec<u8> {
    const HEADER: usize = 32;
    const SEGMENT: usize = 72 + 2 * 80;
    const BUILD: usize = 24;
    const SYMTAB: usize = 24;
    const DYSYMTAB: usize = 80;
    let cmds = SEGMENT + BUILD + SYMTAB + DYSYMTAB;
    let text_off = align(HEADER + cmds, 16);
    let const_addr = align(code.len(), 16);
    let const_off = text_off + const_addr;
    let sym_off = align(const_off + data.len(), 8);
    let strings = format!("\0_{CODE}\0_{DATA}\0");
    let str_off = sym_off + 2 * 16;
    let (cpu, sub, text_align) = match arch {
        Arch::Aarch64 => (0x0100_000C, 0, 2),
        Arch::X86_64 => (0x0100_0007, 3, 0),
    };

    let mut o = Out(Vec::new());
    o.u32(0xFEED_FACF);
    o.u32(cpu);
    o.u32(sub);
    o.u32(1); // MH_OBJECT
    o.u32(4);
    o.u32(cmds as u32);
    o.u32(0);
    o.u32(0);

    // LC_SEGMENT_64
    o.u32(0x19);
    o.u32(SEGMENT as u32);
    o.name("", 16);
    o.u64(0);
    o.u64((const_addr + data.len()) as u64);
    o.u64(text_off as u64);
    o.u64((const_addr + data.len()) as u64);
    o.u32(7);
    o.u32(7);
    o.u32(2);
    o.u32(0);
    for (sect, addr, size, off, align, flags) in [
        (
            "__text",
            0,
            code.len(),
            text_off,
            text_align,
            0x8000_0400u32,
        ),
        ("__const", const_addr, data.len(), const_off, 3, 0),
    ] {
        o.name(sect, 16);
        o.name("__TEXT", 16);
        o.u64(addr as u64);
        o.u64(size as u64);
        o.u32(off as u32);
        o.u32(align);
        o.u32(0);
        o.u32(0);
        o.u32(flags);
        o.u32(0);
        o.u32(0);
        o.u32(0);
    }

    // LC_BUILD_VERSION: macOS 11.
    o.u32(0x32);
    o.u32(BUILD as u32);
    o.u32(1);
    o.u32(0x000B_0000);
    o.u32(0);
    o.u32(0);

    // LC_SYMTAB
    o.u32(0x2);
    o.u32(SYMTAB as u32);
    o.u32(sym_off as u32);
    o.u32(2);
    o.u32(str_off as u32);
    o.u32(strings.len() as u32);

    // LC_DYSYMTAB: both symbols are defined and external.
    o.u32(0xB);
    o.u32(DYSYMTAB as u32);
    for v in [0, 0, 0, 2, 2, 0] {
        o.u32(v);
    }
    for _ in 0..12 {
        o.u32(0);
    }

    o.0.resize(text_off, 0);
    o.0.extend_from_slice(code);
    o.0.resize(const_off, 0);
    o.0.extend_from_slice(data);
    o.pad(8);
    debug_assert_eq!(o.0.len(), sym_off);
    for (strx, sect, value) in [
        (1u32, 1u8, 0u64),
        (2 + CODE.len() as u32 + 1, 2, const_addr as u64),
    ] {
        o.u32(strx);
        o.u8(0x0F); // N_SECT | N_EXT
        o.u8(sect);
        o.u16(0);
        o.u64(value);
    }
    o.0.extend_from_slice(strings.as_bytes());
    o.0
}

/// An ELF64 relocatable: `.text`, `.rodata`, a symbol table, and a note that
/// the stack need not be executable.
fn elf(arch: Arch, code: &[u8], data: &[u8]) -> Vec<u8> {
    let machine: u16 = match arch {
        Arch::Aarch64 => 183,
        Arch::X86_64 => 62,
    };
    let shstr = "\0.text\0.rodata\0.symtab\0.strtab\0.shstrtab\0.note.GNU-stack\0";
    let name = |s: &str| shstr.find(&format!("\0{s}\0")).expect("a section name") as u32 + 1;
    let strtab = format!("\0{CODE}\0{DATA}\0");

    let text_off = 64usize;
    let rodata_off = align(text_off + code.len(), 8);
    let symtab_off = align(rodata_off + data.len(), 8);
    let symtab_len = 3 * 24;
    let strtab_off = symtab_off + symtab_len;
    let shstr_off = strtab_off + strtab.len();
    let sh_off = align(shstr_off + shstr.len(), 8);

    let mut o = Out(Vec::new());
    o.0.extend_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0]);
    o.u64(0);
    o.u16(1); // ET_REL
    o.u16(machine);
    o.u32(1);
    o.u64(0);
    o.u64(0);
    o.u64(sh_off as u64);
    o.u32(0);
    o.u16(64);
    o.u16(0);
    o.u16(0);
    o.u16(64);
    o.u16(7);
    o.u16(5);

    o.0.extend_from_slice(code);
    o.0.resize(rodata_off, 0);
    o.0.extend_from_slice(data);
    o.0.resize(symtab_off, 0);
    // The null symbol, then the two globals.
    for _ in 0..24 {
        o.u8(0);
    }
    for (strx, info, shndx, size) in [
        (1u32, 0x12u8, 1u16, code.len()),
        (2 + CODE.len() as u32, 0x11, 2, data.len()),
    ] {
        o.u32(strx);
        o.u8(info);
        o.u8(0);
        o.u16(shndx);
        o.u64(0);
        o.u64(size as u64);
    }
    o.0.extend_from_slice(strtab.as_bytes());
    o.0.extend_from_slice(shstr.as_bytes());
    o.pad(8);
    debug_assert_eq!(o.0.len(), sh_off);

    let text_align = match arch {
        Arch::Aarch64 => 4,
        Arch::X86_64 => 16,
    };
    // name, type, flags, addr, offset, size, link, info, align, entsize
    let sections: [(u32, u32, u64, usize, usize, u32, u32, u64, u64); 7] = [
        (0, 0, 0, 0, 0, 0, 0, 0, 0),
        (
            name(".text"),
            1,
            6,
            text_off,
            code.len(),
            0,
            0,
            text_align,
            0,
        ),
        (name(".rodata"), 1, 2, rodata_off, data.len(), 0, 0, 8, 0),
        (name(".symtab"), 2, 0, symtab_off, symtab_len, 4, 1, 8, 24),
        (name(".strtab"), 3, 0, strtab_off, strtab.len(), 0, 0, 1, 0),
        (name(".shstrtab"), 3, 0, shstr_off, shstr.len(), 0, 0, 1, 0),
        (name(".note.GNU-stack"), 1, 0, sh_off, 0, 0, 0, 1, 0),
    ];
    for (name, ty, flags, off, size, link, info, align, entsize) in sections {
        o.u32(name);
        o.u32(ty);
        o.u64(flags);
        o.u64(0);
        o.u64(off as u64);
        o.u64(size as u64);
        o.u32(link);
        o.u32(info);
        o.u64(align);
        o.u64(entsize);
    }
    o.0
}

/// A COFF object, as the Microsoft linker takes: `.text` and `.rdata`, and a
/// symbol table whose two names are too long for the eight bytes a symbol has,
/// so live in the string table after it.
fn coff(arch: Arch, code: &[u8], data: &[u8]) -> Vec<u8> {
    const HEADER: usize = 20;
    const SECTION: usize = 40;
    const SYMBOL: usize = 18;
    let (machine, text_align): (u16, u32) = match arch {
        Arch::Aarch64 => (0xAA64, 0x0030_0000),
        Arch::X86_64 => (0x8664, 0x0050_0000),
    };
    let text_off = HEADER + 2 * SECTION;
    let rdata_off = text_off + code.len();
    let symtab_off = rdata_off + data.len();
    // The string table starts with its own length, which the offsets count.
    let strings = format!("{CODE}\0{DATA}\0");

    let mut o = Out(Vec::new());
    o.u16(machine);
    o.u16(2); // sections
    o.u32(0); // timestamp: none, so the same program makes the same object
    o.u32(symtab_off as u32);
    o.u32(2); // symbols
    o.u16(0); // no optional header in an object
    o.u16(0);

    // CNT_CODE | MEM_EXECUTE | MEM_READ, and CNT_INITIALIZED_DATA | ALIGN_8BYTES
    // | MEM_READ.
    for (name, size, off, flags) in [
        (".text", code.len(), text_off, 0x6000_0020 | text_align),
        (".rdata", data.len(), rdata_off, 0x4040_0040u32),
    ] {
        o.name(name, 8);
        o.u32(0);
        o.u32(0);
        o.u32(size as u32);
        o.u32(off as u32);
        o.u32(0);
        o.u32(0);
        o.u16(0);
        o.u16(0);
        o.u32(flags);
    }

    o.0.extend_from_slice(code);
    o.0.extend_from_slice(data);
    debug_assert_eq!(o.0.len(), symtab_off);
    // Name by string table offset, value, section, type (a function, or not),
    // IMAGE_SYM_CLASS_EXTERNAL, no auxiliary records.
    for (strx, section, ty) in [(4u32, 1u16, 0x20u16), (4 + CODE.len() as u32 + 1, 2, 0)] {
        o.u32(0);
        o.u32(strx);
        o.u32(0);
        o.u16(section);
        o.u16(ty);
        o.u8(2);
        o.u8(0);
    }
    debug_assert_eq!(o.0.len(), symtab_off + 2 * SYMBOL);
    o.u32(4 + strings.len() as u32);
    o.0.extend_from_slice(strings.as_bytes());
    o.0
}

/// The symbol only a runtime library built from these sources defines -- see
/// `build.rs`.
pub fn runtime_symbol() -> String {
    format!("meadow_glade_{}", env!("MEADOW_GLADE_FINGERPRINT"))
}

/// The C `main` a program links: it hands the object's two symbols to the
/// runtime, and refers to [`runtime_symbol`], so that it links only with a
/// runtime its code was generated for.
pub fn main_c() -> String {
    let runtime = runtime_symbol();
    format!(
        "extern const unsigned char {CODE}[];\n\
         extern const unsigned char {DATA}[];\n\
         extern const unsigned char {runtime};\n\
         const unsigned char *const meadow_runtime = &{runtime};\n\
         extern int meadow_silo_main(const unsigned char *code, const unsigned char *data,\n\
         \x20                          int argc, char **argv);\n\
         int main(int argc, char **argv) {{\n\
         \x20   return meadow_silo_main({CODE}, {DATA}, argc, argv);\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::Format;

    /// Android is ELF, but not glibc: asking its linker for `gcc_s` fails the
    /// link, which is how the first native build on a phone went.
    #[test]
    fn android_links_its_own_system_libraries() {
        let android = Format::Elf.system_libs_on(true);
        for glibc_only in ["-lgcc_s", "-lutil", "-lrt", "-lpthread"] {
            assert!(
                !android.contains(&glibc_only),
                "{glibc_only} in {android:?}"
            );
        }
        assert!(android.contains(&"-llog") && android.contains(&"-lunwind"));
        assert!(
            Format::Elf.system_libs_on(false).contains(&"-lgcc_s"),
            "Linux keeps its own"
        );
    }
}
