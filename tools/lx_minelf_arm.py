#!/usr/bin/env python3
"""MinELF generator for the LXPD-ARM first-start vehicle (strand 8).

Builds a minimal, VALID ET_EXEC aarch64 ELF with exactly one PT_LOAD segment
at a caller-chosen VA (default 0x41000000 — the canonical user base from
programs/user.ld, inside the kernel's ARM user window
[USER_RAM_MIN, GIB1_END) = [0x40200000, 0x80000000), enforced by
`vspace_map_page_at` in crates/caprock-hal/src/aarch64/mmu.rs).
Entry = segment start.

Segment content: `14 00 00 00` (`b .` — branches to itself; proves execution
without syscalls or devices), rest of the page A64 NOP (`D5 03 20 1F`, in case
execution ever steps past the loop). Everything page-aligned (VA, offset,
align 0x1000), p_memsz == p_filesz (no .bss), e_machine = EM_AARCH64 (0xB7) —
the caprock-loader parser (crates/caprock-loader/src/elf.rs) requires
ET_EXEC + EXPECTED_MACHINE + page-aligned p_vaddr.

This mirrors tools/lx_minelf.py (x86-64 Fahrt 5, VA 0x20000000) for ARM, where
the x86 base would fall BELOW the user window and the kernel would rightly
refuse it (Code 10, MANGEL_MAPPING_ABGEWIESEN). A low --vaddr build is
supported on purpose: the vehicle's negative boot proves that refusal.

Usage:
  python3 tools/lx_minelf_arm.py --out build/diag/lx-minelf-arm.img
  python3 tools/lx_minelf_arm.py --out build/diag/lx-minelf-arm-low.img --vaddr 0x100000
Stdout: size + SHA-256 (becomes the manifest hash of entry 7).
"""
import argparse
import hashlib
import struct
import sys

DEFAULT_VA = 0x41000000
PAGE = 0x1000
EM_AARCH64 = 0xB7
ET_EXEC = 2


def build(va: int) -> bytes:
    ehsize = 64
    phentsize = 56
    phnum = 1
    phoff = ehsize
    table_end = phoff + phnum * phentsize  # 120
    p_offset = PAGE  # 0x1000, page-aligned
    filesz = PAGE  # one page of code
    total = p_offset + filesz  # 0x2000

    v = bytearray(total)
    # e_ident
    v[0:4] = b"\x7fELF"
    v[4] = 2  # ELFCLASS64
    v[5] = 1  # ELFDATA2LSB
    v[6] = 1  # EI_VERSION
    # ELF64 header (little-endian)
    struct.pack_into("<H", v, 16, ET_EXEC)
    struct.pack_into("<H", v, 18, EM_AARCH64)
    struct.pack_into("<I", v, 20, 1)  # e_version
    struct.pack_into("<Q", v, 24, va)  # e_entry = segment start
    struct.pack_into("<Q", v, 32, phoff)  # e_phoff
    struct.pack_into("<Q", v, 40, 0)  # e_shoff
    struct.pack_into("<I", v, 48, 0)  # e_flags
    struct.pack_into("<H", v, 52, ehsize)
    struct.pack_into("<H", v, 54, phentsize)
    struct.pack_into("<H", v, 56, phnum)
    struct.pack_into("<H", v, 58, 0)  # e_shentsize
    struct.pack_into("<H", v, 60, 0)  # e_shnum
    struct.pack_into("<H", v, 62, 0)  # e_shstrndx
    # Program header: one PT_LOAD, R+X
    base = phoff
    struct.pack_into("<I", v, base + 0, 1)  # p_type = PT_LOAD
    struct.pack_into("<I", v, base + 4, 5)  # p_flags = PF_R|PF_X
    struct.pack_into("<Q", v, base + 8, p_offset)
    struct.pack_into("<Q", v, base + 16, va)  # p_vaddr
    struct.pack_into("<Q", v, base + 24, va)  # p_paddr
    struct.pack_into("<Q", v, base + 32, filesz)
    struct.pack_into("<Q", v, base + 40, filesz)  # p_memsz == filesz
    struct.pack_into("<Q", v, base + 48, PAGE)  # p_align
    # Segment content: `b .` + A64 NOP fill
    struct.pack_into("<I", v, p_offset, 0x14000000)
    for i in range(p_offset + 4, p_offset + filesz, 4):
        struct.pack_into("<I", v, i, 0xD503201F)
    assert table_end <= p_offset  # header table overlaps no segment
    return bytes(v)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="MinELF generator (LXPD-ARM)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--vaddr", default=f"0x{DEFAULT_VA:x}",
                    help="link base / entry (default 0x41000000)")
    a = ap.parse_args(argv)
    va = int(a.vaddr, 0)
    if va % PAGE != 0:
        print(f"minelf-arm: refused: vaddr 0x{va:x} not page-aligned", file=sys.stderr)
        return 2
    img = build(va)
    with open(a.out, "wb") as f:
        f.write(img)
    print(f"minelf-arm: {a.out} ({len(img)} B, VA 0x{va:08x}, entry 0x{va:08x}, 1x PT_LOAD R+X)")
    print(f"minelf-arm: sha256={hashlib.sha256(img).hexdigest()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
