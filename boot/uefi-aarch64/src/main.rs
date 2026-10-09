//! Caprock aarch64 UEFI boot stub.
//!
//! Reads `\caprock.elf` (the kernel, the same ELF `-kernel` boots) and optionally
//! `\archive.bin` (the boot archive) from the volume the stub was started from, collects the
//! framebuffer (GOP), DTB, RSDP and memory map, calls `ExitBootServices` and jumps to the
//! kernel's `_start_uefi` with a pointer to a `caprock_handover::UefiHandover` in `x0`.
//!
//! The kernel is loaded at the physical addresses in its program headers (no relocation: Caprock
//! is a single identity-mapped address space). If firmware already owns that memory the stub
//! refuses to boot instead of guessing.
#![no_main]
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use caprock_handover as ho;
use core::ptr;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::table::cfg::{ACPI2_GUID, ACPI_GUID};
use uefi::{cstr16, guid, println, CStr16};

/// EFI_DTB_TABLE_GUID.
const DTB_GUID: uefi::Guid = guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");
/// Boot archive window. MUST equal `caprock_kernel::loader::MOD_BASE` / `MOD_WINDOW`
/// (the kernel excludes this window from its allocator).
const ARCHIVE_BASE: u64 = 0x1_3F00_0000;
const ARCHIVE_WINDOW: u64 = 0x0100_0000;
/// The top 8 KiB of RAM are owned by firmware (BOOT_SERVICES_DATA, seen under edk2/QEMU), so the
/// window is claimed minus its last two pages; the kernel still excludes the whole window.
const ARCHIVE_USABLE: u64 = ARCHIVE_WINDOW - 0x2000;
const PAGE: u64 = 4096;

fn fail(msg: &str, e: impl core::fmt::Debug) -> Status {
    println!("stub: FAILED {msg}: {e:?}");
    Status::LOAD_ERROR
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

struct Loaded {
    base: u64,
    end: u64,
    entry: u64,
}

/// Load an ELF64 little-endian aarch64 image at its `p_paddr` addresses. Returns the extent and
/// the address of symbol `_start_uefi` (NOT `e_entry`, which is the `-kernel` entry `_start`).
fn load_kernel(elf: &[u8]) -> Result<Loaded, &'static str> {
    if elf.len() < 64 || &elf[0..4] != b"\x7fELF" || elf[4] != 2 || elf[5] != 1 {
        return Err("not an ELF64 LE file");
    }
    if rd16(elf, 18) != 183 {
        return Err("not an aarch64 ELF");
    }
    let phoff = rd64(elf, 32) as usize;
    let shoff = rd64(elf, 40) as usize;
    let phentsize = rd16(elf, 54) as usize;
    let phnum = rd16(elf, 56) as usize;
    let shentsize = rd16(elf, 58) as usize;
    let shnum = rd16(elf, 60) as usize;

    // Pass 1: extent.
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if p + 56 > elf.len() {
            return Err("truncated program headers");
        }
        if rd32(elf, p) != 1 {
            continue; // PT_LOAD only
        }
        let paddr = rd64(elf, p + 24);
        let memsz = rd64(elf, p + 40);
        lo = lo.min(paddr);
        hi = hi.max(paddr.checked_add(memsz).ok_or("segment overflow")?);
    }
    if lo >= hi {
        return Err("no PT_LOAD segment");
    }
    let lo_pg = lo & !(PAGE - 1);
    let hi_pg = (hi + PAGE - 1) & !(PAGE - 1);
    let pages = ((hi_pg - lo_pg) / PAGE) as usize;
    // Exact address or nothing: another address would be a different (unlinked) kernel.
    // LOADER_CODE, not LOADER_DATA: firmware maps data types execute-never (EFI memory attribute
    // protection), and the kernel must be executable while the stub still runs on the firmware MMU.
    boot::allocate_pages(AllocateType::Address(lo_pg), MemoryType::LOADER_CODE, pages)
        .map_err(|_| "kernel load address is not free in the UEFI memory map")?;
    unsafe { ptr::write_bytes(lo_pg as *mut u8, 0, (hi_pg - lo_pg) as usize) };

    // Pass 2: copy file bytes (bss is already zero).
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if rd32(elf, p) != 1 {
            continue;
        }
        let off = rd64(elf, p + 8) as usize;
        let paddr = rd64(elf, p + 24);
        let filesz = rd64(elf, p + 32) as usize;
        if off.checked_add(filesz).ok_or("overflow")? > elf.len() {
            return Err("segment beyond file");
        }
        unsafe { ptr::copy_nonoverlapping(elf.as_ptr().add(off), paddr as *mut u8, filesz) };
    }

    // Symbol lookup: `_start_uefi` in .symtab.
    let mut entry = 0u64;
    for i in 0..shnum {
        let s = shoff + i * shentsize;
        if s + 64 > elf.len() || rd32(elf, s + 4) != 2 {
            continue; // SHT_SYMTAB
        }
        let sym_off = rd64(elf, s + 24) as usize;
        let sym_size = rd64(elf, s + 32) as usize;
        let strtab_idx = rd32(elf, s + 40) as usize;
        let st = shoff + strtab_idx * shentsize;
        let str_off = rd64(elf, st + 24) as usize;
        let str_size = rd64(elf, st + 32) as usize;
        if sym_off + sym_size > elf.len() || str_off + str_size > elf.len() {
            return Err("bad symtab");
        }
        for k in 0..sym_size / 24 {
            let e = sym_off + k * 24;
            let name = rd32(elf, e) as usize;
            if name < str_size && elf[str_off + name..].starts_with(b"_start_uefi\0") {
                entry = rd64(elf, e + 8);
            }
        }
    }
    if entry < lo || entry >= hi {
        return Err("symbol _start_uefi missing (kernel built without UEFI entry, or stripped)");
    }
    Ok(Loaded { base: lo_pg, end: hi_pg, entry })
}

fn read_file(path: &CStr16) -> Option<Vec<u8>> {
    let fs = boot::get_image_file_system(boot::image_handle()).ok()?;
    let mut fs = uefi::fs::FileSystem::new(fs);
    fs.read(uefi::fs::Path::new(path)).ok()
}

fn config_table(guid: uefi::Guid) -> u64 {
    uefi::system::with_config_table(|t| {
        t.iter().find(|e| e.guid == guid).map(|e| e.address as u64).unwrap_or(0)
    })
}

fn current_el() -> u32 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) v) };
    ((v >> 2) & 3) as u32
}

/// Clean + invalidate data cache lines to the point of coherency for `[base, base+len)`, so the
/// kernel (which starts with MMU and caches off) sees exactly what this stub wrote.
unsafe fn flush(base: u64, len: u64) {
    let mut a = base & !63;
    let end = base + len;
    while a < end {
        core::arch::asm!("dc civac, {}", in(reg) a);
        a += 64;
    }
    core::arch::asm!("dsb sy");
}

fn kind_of(ty: MemoryType) -> u32 {
    match ty {
        MemoryType::CONVENTIONAL | MemoryType::BOOT_SERVICES_CODE | MemoryType::BOOT_SERVICES_DATA => {
            ho::REGION_USABLE
        }
        MemoryType::LOADER_CODE | MemoryType::LOADER_DATA => ho::REGION_IMAGE,
        MemoryType::ACPI_RECLAIM | MemoryType::ACPI_NON_VOLATILE => ho::REGION_ACPI,
        _ => ho::REGION_RESERVED,
    }
}

#[entry]
fn efi_main() -> Status {
    println!("stub: Caprock aarch64 UEFI stub, EL{}", current_el());

    // --- kernel + archive + disk-driver image ---------------------------------------------
    let Some(elf) = read_file(cstr16!("\\caprock.elf")) else {
        println!("stub: FAILED cannot read \\caprock.elf from the boot volume");
        return Status::NOT_FOUND;
    };
    let k = match load_kernel(&elf) {
        Ok(k) => k,
        Err(e) => return fail("load kernel", e),
    };
    drop(elf);
    println!("stub: kernel loaded [{:#x}, {:#x}) entry {:#x}", k.base, k.end, k.entry);

    // --- kernel + archive + disk-driver image ------------------------------------------------
    // The disk driver comes FROM the bootloader, not from disk (Simon 2026-10-09: what reads the
    // disk cannot live on it). `\driver.bin` travels on the ESP next to `\caprock.elf` and
    // `\archive.bin`, is placed page-aligned right after the archive inside the same reserved
    // window (so the kernel side needs no new reservation), and is recorded in the handover's
    // driver span. Absent files, an unclaimable window, or no room mean "that cargo is missing",
    // never a boot failure.
    let archive_file = read_file(cstr16!("\\archive.bin"));
    let driver_file = read_file(cstr16!("\\driver.bin"));
    let mut archive_len = 0u64;
    let mut driver_base = 0u64;
    let mut driver_len = 0u64;
    let need_window = archive_file.as_ref().is_some_and(|a| a.len() as u64 <= ARCHIVE_USABLE)
        || driver_file.is_some();
    if need_window {
        let pages = (ARCHIVE_USABLE / PAGE) as usize;
        match boot::allocate_pages(AllocateType::Address(ARCHIVE_BASE), MemoryType::LOADER_DATA, pages) {
            Ok(_) => unsafe {
                ptr::write_bytes(ARCHIVE_BASE as *mut u8, 0, ARCHIVE_USABLE as usize);
                if let Some(a) = &archive_file {
                    if a.len() as u64 <= ARCHIVE_USABLE {
                        ptr::copy_nonoverlapping(a.as_ptr(), ARCHIVE_BASE as *mut u8, a.len());
                        archive_len = a.len() as u64;
                    } else {
                        println!(
                            "stub: archive larger than the {ARCHIVE_USABLE:#x}-byte window; continuing without"
                        );
                    }
                } else {
                    println!("stub: no \\archive.bin; continuing without");
                }
                let archive_end = ARCHIVE_BASE + (archive_len + PAGE - 1) / PAGE * PAGE;
                if let Some(d) = &driver_file {
                    let room = ARCHIVE_BASE + ARCHIVE_USABLE - archive_end;
                    if d.len() as u64 <= room {
                        ptr::copy_nonoverlapping(d.as_ptr(), archive_end as *mut u8, d.len());
                        driver_base = archive_end;
                        driver_len = d.len() as u64;
                    } else {
                        println!(
                            "stub: driver larger than the remaining {room:#x} bytes; continuing without"
                        );
                    }
                } else {
                    println!("stub: no \\driver.bin; continuing without");
                }
            },
            Err(e) => println!("stub: cargo window {ARCHIVE_BASE:#x} not free ({e:?}); continuing without"),
        }
    } else if archive_file.is_some() {
        println!("stub: archive larger than the {ARCHIVE_USABLE:#x}-byte window; continuing without");
    } else {
        println!("stub: no \\archive.bin and no \\driver.bin; continuing without");
    }
    println!("stub: archive {archive_len} bytes at {ARCHIVE_BASE:#x}");
    if driver_len != 0 {
        println!("stub: driver {driver_len} bytes at {driver_base:#x}");
    }
    if archive_len == 0 {
        let mm = boot::memory_map(MemoryType::LOADER_DATA).unwrap();
        for d in mm.entries().filter(|d| d.phys_start >= 0x1_0000_0000 || d.ty == MemoryType::CONVENTIONAL) {
            println!("stub:   mmap {:#x} pages={} ty={:?}", d.phys_start, d.page_count, d.ty);
        }
    }

    // --- handover: everything except the memory map -----------------------------------------
    let mut h = Box::new(ho::UefiHandover::zeroed());
    h.magic = ho::MAGIC;
    h.version = ho::VERSION;
    h.size = core::mem::size_of::<ho::UefiHandover>() as u32;
    h.stub_el = current_el();
    h.kernel_base = k.base;
    h.kernel_size = k.end - k.base;
    h.archive_base = ARCHIVE_BASE;
    h.archive_len = archive_len;
    h.driver_base = driver_base;
    h.driver_len = driver_len;

    if let Ok(gh) = boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = boot::open_protocol_exclusive::<GraphicsOutput>(gh) {
            let mi = gop.current_mode_info();
            let (w, ht) = mi.resolution();
            let (fmt, masks) = match mi.pixel_format() {
                PixelFormat::Rgb => (ho::PIXEL_RGBX, (0, 0, 0)),
                PixelFormat::Bgr => (ho::PIXEL_BGRX, (0, 0, 0)),
                PixelFormat::Bitmask => {
                    let m = mi.pixel_bitmask().unwrap();
                    (ho::PIXEL_BITMASK, (m.red, m.green, m.blue))
                }
                PixelFormat::BltOnly => (ho::PIXEL_NONE, (0, 0, 0)),
            };
            let mut fb = gop.frame_buffer();
            h.fb = ho::Framebuffer {
                base: fb.as_mut_ptr() as u64,
                size: fb.size() as u64,
                width: w as u32,
                height: ht as u32,
                pitch: (mi.stride() * 4) as u32,
                bpp: 32,
                format: fmt,
                red_mask: masks.0,
                green_mask: masks.1,
                blue_mask: masks.2,
            };
        }
    }
    println!(
        "stub: fb base={:#x} {}x{} pitch={} format={}",
        h.fb.base, h.fb.width, h.fb.height, h.fb.pitch, h.fb.format as i32
    );

    h.dtb = config_table(DTB_GUID);
    if h.dtb != 0 {
        // FDT header: magic 0xd00dfeed, totalsize, both big-endian.
        let p = h.dtb as *const u8;
        let hdr = unsafe { core::slice::from_raw_parts(p, 8) };
        if hdr[0..4] == [0xd0, 0x0d, 0xfe, 0xed] {
            h.dtb_size = u32::from_be_bytes(hdr[4..8].try_into().unwrap()) as u64;
        } else {
            h.dtb = 0;
        }
    }
    h.rsdp = match config_table(ACPI2_GUID) {
        0 => config_table(ACPI_GUID),
        a => a,
    };
    println!("stub: dtb={:#x} ({} bytes) rsdp={:#x}", h.dtb, h.dtb_size, h.rsdp);

    // --- the point of no return ----------------------------------------------------------
    // No println!/allocation after this: boot services (and the console) are gone.
    let mmap = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };
    let mut n = 0usize;
    for d in mmap.entries() {
        let kind = kind_of(d.ty);
        let base = d.phys_start;
        let len = d.page_count * PAGE;
        if n > 0 {
            let last = &mut h.regions[n - 1];
            if last.kind == kind && last.base + last.len == base {
                last.len += len;
                continue;
            }
        }
        if n < ho::MAX_REGIONS {
            h.regions[n] = ho::Region { base, len, kind, _pad: 0 };
            n += 1;
        }
        // Overflow drops the tail: a missing region is never usable memory, only less of it.
    }
    h.n_regions = n as u32;

    let hp = Box::into_raw(h) as u64;
    unsafe {
        flush(k.base, k.end - k.base);
        flush(ARCHIVE_BASE, ARCHIVE_USABLE);
        flush(hp, core::mem::size_of::<ho::UefiHandover>() as u64);
        core::arch::asm!("ic iallu", "dsb sy", "isb");
        core::arch::asm!(
            "msr daifset, #0xf",
            "br {entry}",
            entry = in(reg) k.entry,
            in("x0") hp,
            options(noreturn)
        );
    }
}
