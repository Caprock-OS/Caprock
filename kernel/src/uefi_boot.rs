//! UEFI handover consumer (aarch64). Entered from `_start_uefi` (see `arch/aarch64/boot.rs`)
//! with the MMU off, on the boot stack, with `.bss` zeroed. Validates the handover, copies it out
//! of firmware-owned memory, checks the framebuffer, then continues into the regular
//! `kernel_main` — everything after this point is shared with the `-kernel` path.
//!
//! Two things travel in the handover for the bootloader-loaded disk driver (Simon 2026-10-09):
//! the driver image span itself and the device description (DTB and/or ACPI RSDP). The kernel
//! checks only bounds here and passes the bytes through; parsing happens in the driver PD / IRT.
//!
//! Output goes to serial (the test channel, unchanged) AND to a small framebuffer text console
//! (for QEMU readability). The framebuffer never replaces serial: the result signature the
//! suites compare is serial-only.

use caprock_handover as ho;
use caprock_hal::console::{emit_fmt, emit_raw};

/// The handover, copied into kernel memory before anything can allocate over the stub's copy.
static mut HANDOVER: ho::UefiHandover = ho::UefiHandover::zeroed();
static mut HANDOVER_VALID: bool = false;

/// The validated handover, if this boot came through the UEFI stub.
#[allow(dead_code)]
pub fn handover() -> Option<&'static ho::UefiHandover> {
    // SAFETY: written once, before any other core runs and before interrupts exist.
    unsafe {
        if *core::ptr::addr_of!(HANDOVER_VALID) {
            Some(&*core::ptr::addr_of!(HANDOVER))
        } else {
            None
        }
    }
}

fn park(why: &str) -> ! {
    emit_fmt(format_args!("uefi   : FAILURES ({why}) -- refusing to boot\n"));
    loop {
        caprock_hal::cpu::wfi();
    }
}

/// Bounds only, no parsing: the driver span must sit inside the reserved loader window (which
/// the allocator never touches) and must not overlap the archive. Anything else is a broken
/// stub promise; the caller zeroes the span and boots on without a driver.
fn driver_span_ok(db: u64, dl: u64, ab: u64, al: u64) -> bool {
    let win_end = crate::loader::MOD_BASE + crate::loader::MOD_WINDOW;
    let Some(de) = db.checked_add(dl) else { return false };
    if db < crate::loader::MOD_BASE || de > win_end {
        return false;
    }
    if al != 0 {
        let Some(ae) = ab.checked_add(al) else { return false };
        if db < ae && ab < de {
            return false;
        }
    }
    true
}

/// Paint a vertical bar pattern into the whole framebuffer and read back what was written.
/// Pixels are written as the byte sequence the pixel format demands, so a correct format is
/// visible on screen (red, green, blue bars) and the read-back proves the memory is real RAM,
/// not a hole.
fn fb_selftest(fb: &ho::Framebuffer) -> bool {
    let base = fb.base as *mut u8;
    let bars: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]]; // R,G,B
    let mut ok = true;
    for y in 0..fb.height as usize {
        let row = unsafe { base.add(y * fb.pitch as usize) };
        for x in 0..fb.width as usize {
            let [r, g, b] = bars[x * 4 / fb.width as usize];
            let px = match fb.format {
                ho::PIXEL_RGBX => [r, g, b, 0],
                ho::PIXEL_BGRX => [b, g, r, 0],
                _ => {
                    let v = ((r as u32) & 0xff) * (fb.red_mask & fb.red_mask.wrapping_neg()).max(1)
                        | (g as u32) * (fb.green_mask & fb.green_mask.wrapping_neg()).max(1)
                        | (b as u32) * (fb.blue_mask & fb.blue_mask.wrapping_neg()).max(1);
                    v.to_le_bytes()
                }
            };
            unsafe {
                let p = row.add(x * 4) as *mut u32;
                p.write_volatile(u32::from_le_bytes(px));
            }
        }
        // Read back one pixel per row (all rows would double the boot time under TCG).
        let probe = (y % fb.width as usize) * 4;
        let _ = probe;
        let x = (y * 7) % fb.width as usize;
        let [r, g, b] = bars[x * 4 / fb.width as usize];
        let want = match fb.format {
            ho::PIXEL_RGBX => u32::from_le_bytes([r, g, b, 0]),
            ho::PIXEL_BGRX => u32::from_le_bytes([b, g, r, 0]),
            _ => unsafe { (row.add(x * 4) as *const u32).read_volatile() },
        };
        let got = unsafe { (row.add(x * 4) as *const u32).read_volatile() };
        if got != want {
            ok = false;
        }
    }
    ok
}

// -------------------------------------------------------------------------------------------------
// Framebuffer text console (alongside serial, never replacing it).
//
// 8x8 glyphs, 32 bpp, white on black. Covers ASCII 0x20..=0x5F (space, punctuation, digits,
// uppercase); lowercase maps to uppercase, anything else to `?`. The charset is deliberately
// small: every line this console prints is composed from it, so no message can silently degrade.
// -------------------------------------------------------------------------------------------------

/// Glyph rows, top first, bit 7 = leftmost pixel. Index = byte - 0x20.
#[rustfmt::skip]
static FONT: [[u8; 8]; 96 - 32] = [
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // space
    [0x10,0x10,0x10,0x10,0x10,0x00,0x10,0x00], // !
    [0x44,0x44,0x44,0x00,0x00,0x00,0x00,0x00], // "
    [0x24,0x24,0x7E,0x24,0x24,0x7E,0x24,0x24], // #
    [0x10,0x3C,0x54,0x38,0x2C,0x3C,0x10,0x00], // $
    [0x62,0x64,0x08,0x10,0x20,0x46,0x86,0x00], // %
    [0x3C,0x42,0x42,0x3C,0x4A,0x44,0x3A,0x00], // &
    [0x10,0x10,0x10,0x00,0x00,0x00,0x00,0x00], // '
    [0x08,0x10,0x20,0x20,0x20,0x20,0x10,0x08], // (
    [0x20,0x10,0x08,0x08,0x08,0x08,0x10,0x20], // )
    [0x00,0x54,0x38,0x7C,0x38,0x54,0x00,0x00], // *
    [0x00,0x10,0x10,0x7C,0x10,0x10,0x00,0x00], // +
    [0x00,0x00,0x00,0x00,0x10,0x10,0x20,0x00], // ,
    [0x00,0x00,0x00,0x7C,0x00,0x00,0x00,0x00], // -
    [0x00,0x00,0x00,0x00,0x00,0x00,0x10,0x00], // .
    [0x04,0x04,0x08,0x10,0x20,0x40,0x40,0x80], // /
    [0x38,0x44,0x4C,0x54,0x54,0x64,0x38,0x00], // 0 (slashed: O is identical otherwise)
    [0x10,0x30,0x10,0x10,0x10,0x10,0x7C,0x00], // 1
    [0x38,0x44,0x04,0x08,0x10,0x20,0x7C,0x00], // 2
    [0x38,0x44,0x04,0x30,0x04,0x44,0x38,0x00], // 3
    [0x08,0x18,0x28,0x48,0x7C,0x08,0x08,0x00], // 4
    [0x7C,0x40,0x78,0x04,0x04,0x44,0x38,0x00], // 5
    [0x30,0x20,0x40,0x78,0x44,0x44,0x38,0x00], // 6
    [0x7C,0x04,0x08,0x10,0x20,0x20,0x20,0x00], // 7
    [0x38,0x44,0x44,0x38,0x44,0x44,0x38,0x00], // 8
    [0x38,0x44,0x44,0x3C,0x04,0x08,0x30,0x00], // 9
    [0x00,0x00,0x10,0x00,0x00,0x10,0x00,0x00], // :
    [0x00,0x00,0x10,0x00,0x00,0x10,0x10,0x20], // ;
    [0x08,0x10,0x20,0x40,0x20,0x10,0x08,0x00], // <
    [0x00,0x00,0x7C,0x00,0x7C,0x00,0x00,0x00], // =
    [0x20,0x10,0x08,0x04,0x08,0x10,0x20,0x00], // >
    [0x38,0x44,0x04,0x08,0x10,0x00,0x10,0x00], // ?
    [0x38,0x44,0x5C,0x54,0x5C,0x40,0x3C,0x00], // @
    [0x10,0x28,0x44,0x44,0x7C,0x44,0x44,0x00], // A
    [0x78,0x44,0x44,0x78,0x44,0x44,0x78,0x00], // B
    [0x1C,0x20,0x40,0x40,0x40,0x20,0x1C,0x00], // C
    [0x78,0x44,0x44,0x44,0x44,0x44,0x78,0x00], // D
    [0x7C,0x40,0x40,0x78,0x40,0x40,0x7C,0x00], // E
    [0x7C,0x40,0x40,0x78,0x40,0x40,0x40,0x00], // F
    [0x1C,0x20,0x40,0x4C,0x44,0x24,0x1C,0x00], // G
    [0x44,0x44,0x44,0x7C,0x44,0x44,0x44,0x00], // H
    [0x7C,0x10,0x10,0x10,0x10,0x10,0x7C,0x00], // I
    [0x3C,0x08,0x08,0x08,0x08,0x48,0x30,0x00], // J
    [0x44,0x48,0x50,0x60,0x50,0x48,0x44,0x00], // K
    [0x40,0x40,0x40,0x40,0x40,0x40,0x7C,0x00], // L
    [0x44,0x6C,0x54,0x54,0x44,0x44,0x44,0x00], // M
    [0x44,0x68,0x68,0x54,0x4C,0x4C,0x44,0x00], // N
    [0x38,0x44,0x44,0x44,0x44,0x44,0x38,0x00], // O
    [0x78,0x44,0x44,0x78,0x40,0x40,0x40,0x00], // P
    [0x38,0x44,0x44,0x44,0x54,0x48,0x34,0x00], // Q
    [0x78,0x44,0x44,0x78,0x50,0x48,0x44,0x00], // R
    [0x3C,0x40,0x40,0x38,0x04,0x04,0x78,0x00], // S
    [0x7C,0x10,0x10,0x10,0x10,0x10,0x10,0x00], // T
    [0x44,0x44,0x44,0x44,0x44,0x44,0x38,0x00], // U
    [0x44,0x44,0x44,0x44,0x44,0x28,0x10,0x00], // V
    [0x44,0x44,0x44,0x44,0x54,0x54,0x28,0x00], // W
    [0x44,0x44,0x28,0x10,0x28,0x44,0x44,0x00], // X
    [0x44,0x44,0x28,0x10,0x10,0x10,0x10,0x00], // Y
    [0x7C,0x04,0x08,0x10,0x20,0x40,0x7C,0x00], // Z
    [0x30,0x20,0x20,0x20,0x20,0x20,0x30,0x00], // [
    [0x40,0x40,0x20,0x10,0x08,0x04,0x04,0x02], // backslash
    [0x18,0x08,0x08,0x08,0x08,0x08,0x18,0x00], // ]
    [0x10,0x28,0x44,0x00,0x00,0x00,0x00,0x00], // ^
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xFE], // _
];

struct FbCon {
    base: u64,
    pitch: u32,
    cols: u32,
    rows: u32,
    fg: u32,
    col: u32,
    row: u32,
}

impl FbCon {
    /// Attach to a plausible framebuffer and clear it. `None` when the geometry cannot carry
    /// text (including a bitmask mode without usable masks).
    fn new(fb: &ho::Framebuffer) -> Option<FbCon> {
        if !fb.plausible() {
            return None;
        }
        let fg = match fb.format {
            ho::PIXEL_RGBX | ho::PIXEL_BGRX => 0x00FF_FFFF,
            _ => fb.red_mask | fb.green_mask | fb.blue_mask,
        };
        if fg == 0 {
            return None;
        }
        let (cols, rows) = (fb.width / 8, fb.height / 8);
        if cols == 0 || rows == 0 {
            return None;
        }
        let mut c = FbCon { base: fb.base, pitch: fb.pitch, cols, rows, fg, col: 0, row: 0 };
        c.clear();
        Some(c)
    }

    fn clear(&mut self) {
        let n = (self.pitch as usize) * (self.rows as usize) * 8 / 4;
        for i in 0..n {
            unsafe { (self.base as *mut u32).add(i).write_volatile(0) };
        }
    }

    fn scroll(&mut self) {
        let cell_row_u32 = self.pitch as usize * 8 / 4;
        for r in 1..self.rows as usize {
            for i in 0..cell_row_u32 {
                let v = unsafe { (self.base as *const u32).add(r * cell_row_u32 + i).read_volatile() };
                unsafe { (self.base as *mut u32).add((r - 1) * cell_row_u32 + i).write_volatile(v) };
            }
        }
        let last = (self.rows as usize - 1) * cell_row_u32;
        for i in 0..cell_row_u32 {
            unsafe { (self.base as *mut u32).add(last + i).write_volatile(0) };
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.scroll();
            self.row = self.rows - 1;
        }
    }

    fn putc(&mut self, b: u8) {
        if b == b'\n' {
            self.newline();
            return;
        }
        if self.col >= self.cols {
            self.newline();
        }
        let idx = match b {
            0x20..=0x5F => b - 0x20,
            0x61..=0x7A => b - 0x40, // lowercase -> uppercase
            _ => 0x3F - 0x20,        // '?'
        };
        let (x0, y0) = (self.col as usize * 8, self.row as usize * 8);
        for gy in 0..8 {
            let bits = FONT[idx as usize][gy];
            for gx in 0..8 {
                let px = if bits & (0x80 >> gx) != 0 { self.fg } else { 0 };
                unsafe {
                    ((self.base + (y0 + gy) as u64 * self.pitch as u64) as *mut u32)
                        .add(x0 + gx)
                        .write_volatile(px);
                }
            }
        }
        self.col += 1;
    }

    fn puts(&mut self, s: &str) {
        for b in s.bytes() {
            self.putc(b);
        }
    }
}

/// `0X` + 16 uppercase hex digits into the caller's buffer (fb charset has no lowercase).
fn hex_into<'b>(v: u64, out: &'b mut [u8; 18]) -> &'b str {
    const D: &[u8; 16] = b"0123456789ABCDEF";
    out[0] = b'0';
    out[1] = b'X';
    for i in 0..16 {
        out[2 + i] = D[((v >> (60 - 4 * i)) & 0xF) as usize];
    }
    core::str::from_utf8(&out[..]).unwrap_or("?")
}

/// Decimal into the caller's buffer.
fn dec_into<'b>(mut v: u64, out: &'b mut [u8; 20]) -> &'b str {
    let mut i = out.len();
    if v == 0 {
        out[19] = b'0';
        return core::str::from_utf8(&out[19..]).unwrap_or("?");
    }
    while v > 0 && i > 0 {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    core::str::from_utf8(&out[i..]).unwrap_or("?")
}

#[no_mangle]
pub extern "C" fn kernel_main_uefi(h: *const ho::UefiHandover) -> ! {
    emit_raw("\nuefi   : kernel entered via UEFI stub\n");
    if h.is_null() || (h as usize) & 7 != 0 {
        park("handover pointer null or misaligned");
    }
    // SAFETY: the pointer comes from the stub in x0; the header is validated before use of the
    // rest. Plain memcpy out of firmware memory; MMU is off so there are no cache surprises.
    // The driver span is bounds-checked before HANDOVER_VALID is published, so no consumer ever
    // sees a span outside the reserved loader window.
    let driver_absent: bool;
    unsafe {
        let src = &*h;
        if !src.header_ok() {
            park("handover header (magic/version/size) mismatch");
        }
        core::ptr::copy_nonoverlapping(h, core::ptr::addr_of_mut!(HANDOVER), 1);
        let hp = core::ptr::addr_of_mut!(HANDOVER);
        driver_absent = (*hp).driver_len == 0;
        if !driver_absent
            && !driver_span_ok((*hp).driver_base, (*hp).driver_len, (*hp).archive_base, (*hp).archive_len)
        {
            (*hp).driver_base = 0;
            (*hp).driver_len = 0;
        }
        *core::ptr::addr_of_mut!(HANDOVER_VALID) = true;
    }
    let h = handover().unwrap();
    let el = caprock_hal::cpu::current_el();
    emit_fmt(format_args!(
        "uefi   : running at EL{} (stub was at EL{}){}\n",
        el,
        h.stub_el,
        if h.stub_el == 2 && el == 1 { " -- dropped EL2->EL1" } else { "" }
    ));
    emit_fmt(format_args!(
        "uefi   : kernel image {:#x}+{:#x}, archive {:#x}+{:#x}, dtb {:#x}+{:#x}, rsdp {:#x}\n",
        h.kernel_base, h.kernel_size, h.archive_base, h.archive_len, h.dtb, h.dtb_size, h.rsdp
    ));
    // Device description for the bootloader-loaded disk driver: DTB or ACPI, whichever the
    // firmware provided. The kernel passes both through; this line only records which source
    // exists so a missing one is visible here, not as a mysterious driver failure later.
    emit_fmt(format_args!(
        "uefi   : devdesc dtb {} rsdp {}\n",
        if h.dtb != 0 && h.dtb_size != 0 { "present" } else { "absent" },
        if h.rsdp != 0 { "present" } else { "absent" }
    ));
    let mut usable = 0u64;
    for r in &h.regions[..h.n_regions as usize] {
        if r.kind == ho::REGION_USABLE {
            usable += r.len;
        }
    }
    emit_fmt(format_args!(
        "uefi   : memory map {} regions, {} MiB usable after ExitBootServices\n",
        h.n_regions,
        usable >> 20
    ));

    let fb = &h.fb;
    emit_fmt(format_args!(
        "uefi   : fb base={:#x} size={:#x} {}x{} pitch={} bpp={} format={} masks={:#x}/{:#x}/{:#x}\n",
        fb.base, fb.size, fb.width, fb.height, fb.pitch, fb.bpp, fb.format as i32,
        fb.red_mask, fb.green_mask, fb.blue_mask
    ));

    // Framebuffer console from here on: mirror the lines above (serial stays the test channel).
    let mut con = FbCon::new(fb);
    if let Some(c) = con.as_mut() {
        let mut b1 = [0u8; 18];
        let mut b2 = [0u8; 18];
        c.puts("CAPROCK UEFI BOOT\nHANDOVER OK\n");
        c.puts("KERNEL ");
        c.puts(hex_into(h.kernel_base, &mut b1));
        c.puts(" + ");
        c.puts(hex_into(h.kernel_size, &mut b2));
        c.puts("\nARCHIVE ");
        c.puts(hex_into(h.archive_base, &mut b1));
        c.puts(" + ");
        c.puts(hex_into(h.archive_len, &mut b2));
        c.puts("\nDTB ");
        c.puts(hex_into(h.dtb, &mut b1));
        c.puts(" ACPI ");
        c.puts(hex_into(h.rsdp, &mut b2));
        let mut n = [0u8; 20];
        let mut m = [0u8; 20];
        c.puts("\nMAP ");
        c.puts(dec_into(h.n_regions as u64, &mut n));
        c.puts(" REGIONS ");
        c.puts(dec_into(usable >> 20, &mut m));
        c.puts(" MIB USABLE\n");
    }

    if h.driver_len != 0 {
        emit_fmt(format_args!(
            "uefi   : driver image {:#x}+{:#x} inside loader window, past the archive\n",
            h.driver_base, h.driver_len
        ));
        if let Some(c) = con.as_mut() {
            let mut b1 = [0u8; 18];
            let mut b2 = [0u8; 18];
            c.puts("DRIVER ");
            c.puts(hex_into(h.driver_base, &mut b1));
            c.puts(" + ");
            c.puts(hex_into(h.driver_len, &mut b2));
            c.puts("\n");
        }
    } else if driver_absent {
        emit_raw("uefi   : driver none (no \\driver.bin on the ESP)\n");
        if let Some(c) = con.as_mut() {
            c.puts("DRIVER NONE\n");
        }
    } else {
        emit_raw("uefi   : driver FAILURES (span outside loader window or overlapping archive) -- ignored\n");
        if let Some(c) = con.as_mut() {
            c.puts("DRIVER REJECTED (BOUNDS)\n");
        }
    }

    if fb.plausible() {
        let ok = fb_selftest(fb);
        emit_fmt(format_args!("uefi   : fb {}\n", if ok { "ALL PASS" } else { "FAILURES (read-back mismatch)" }));
        // The bar pattern wiped the early lines; leave a readable summary on screen.
        if let Some(c) = con.as_mut() {
            c.puts("FB TEST ");
            c.puts(if ok { "ALL PASS\n" } else { "FAILURES\n" });
            c.puts("BOOT CONTINUES ON SERIAL\n");
        }
    } else {
        emit_raw("uefi   : fb FAILURES (absent or implausible geometry)\n");
        if let Some(c) = con.as_mut() {
            c.puts("FB ABSENT\n");
        }
    }

    if h.archive_len != 0 {
        crate::loader::set_archive_span(h.archive_base, h.archive_len);
    }
    // Everything from here on is the shared kernel. The embedded DTB is still what it uses for RAM
    // layout; `h.dtb` is passed through for the day it consumes the firmware's one.
    crate::kernel_main(h.dtb)
}
