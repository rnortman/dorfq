//! Checks that the portable `dorfq` binary is statically linked. Bazel
//! supplies its path through `DORFQ_MUSL_BIN`; this test does not build under
//! `cargo test`. `musl_test` runs it; `cli_musl_test` runs the CLI suite
//! against the same binary.

const PT_INTERP: u32 = 3;
const EM_X86_64: u16 = 62;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// A binary with no interpreter segment cannot load a dynamic libc.
#[test]
fn is_a_static_x86_64_elf_with_no_interpreter() {
    let elf = std::fs::read(env!("DORFQ_MUSL_BIN")).expect("read dorfq-musl");
    assert!(elf.len() >= 0x40, "shorter than an ELF header");
    assert_eq!(&elf[..4], b"\x7fELF", "not an ELF file");
    assert_eq!(elf[4], 2, "not a 64-bit ELF");
    assert_eq!(elf[5], 1, "not a little-endian ELF");
    assert_eq!(u16_at(&elf, 0x12), EM_X86_64, "not an x86-64 ELF");

    let phoff = usize::try_from(u64_at(&elf, 0x20)).expect("e_phoff fits usize");
    let phentsize = usize::from(u16_at(&elf, 0x36));
    let phnum = usize::from(u16_at(&elf, 0x38));
    assert!(phnum > 0, "no program headers");
    assert!(
        phoff + phnum * phentsize <= elf.len(),
        "program headers run past the end of the file"
    );
    for i in 0..phnum {
        let p_type = u32_at(&elf, phoff + i * phentsize);
        assert_ne!(p_type, PT_INTERP, "program header {i} is PT_INTERP");
    }
}
