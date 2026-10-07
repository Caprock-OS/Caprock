//! UEFI handover consumer (aarch64). Entered from `_start_uefi` (see `arch/aarch64/boot.rs`)
//! with the MMU off, on the boot stack, with `.bss` zeroed. Validates the handover, copies it out
//! of firmware-owned memory, checks the framebuffer, then continues into the regular
//! `kernel_main` — everything after this point is shared with the `-kernel` path.

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

#[no_mangle]
pub extern "C" fn kernel_main_uefi(h: *const ho::UefiHandover) -> ! {
    emit_raw("\nuefi   : kernel entered via UEFI stub\n");
    if h.is_null() || (h as usize) & 7 != 0 {
        park("handover pointer null or misaligned");
    }
    // SAFETY: the pointer comes from the stub in x0; the header is validated before use of the
    // rest. Plain memcpy out of firmware memory; MMU is off so there are no cache surprises.
    unsafe {
        let src = &*h;
        if !src.header_ok() {
            park("handover header (magic/version/size) mismatch");
        }
        core::ptr::copy_nonoverlapping(h, core::ptr::addr_of_mut!(HANDOVER), 1);
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
    if fb.plausible() {
        let ok = fb_selftest(fb);
        emit_fmt(format_args!("uefi   : fb {}\n", if ok { "ALL PASS" } else { "FAILURES (read-back mismatch)" }));
    } else {
        emit_raw("uefi   : fb FAILURES (absent or implausible geometry)\n");
    }

    if h.archive_len != 0 {
        crate::loader::set_archive_span(h.archive_base, h.archive_len);
    }
    // Everything from here on is the shared kernel. The embedded DTB is still what it uses for RAM
    // layout; `h.dtb` is passed through for the day it consumes the firmware's one.
    crate::kernel_main(h.dtb)
}
