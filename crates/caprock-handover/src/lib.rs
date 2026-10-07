//! **UEFI -> kernel handover (aarch64)** — the one structure the UEFI boot stub passes to the
//! Caprock kernel in `x0`.
//!
//! The stub (`boot/uefi-aarch64`) and the kernel (`arch::aarch64::boot::_start_uefi`) both depend
//! on this crate, so the layout has exactly one definition. The structure is `repr(C)`, fixed
//! size, and carries a magic + version + size: a kernel that finds anything else in `x0` must
//! refuse it rather than interpret it.
//!
//! Nothing here is a pointer the kernel must follow into firmware memory except the optional
//! `dtb` / `rsdp` addresses, which are plain physical addresses. Boot services are gone by the
//! time this is read, so every field is a snapshot.
#![no_std]

/// "CAPUEFI1" as little-endian ASCII.
pub const MAGIC: u64 = 0x3149_4645_5550_4143;
/// Bumped whenever the layout changes.
pub const VERSION: u32 = 1;
/// Memory regions the stub can describe. Adjacent regions of the same kind are merged first.
pub const MAX_REGIONS: usize = 128;

/// Region kinds (`Region::kind`).
pub const REGION_USABLE: u32 = 1; // conventional + loader/boot-services memory, free after EBS
pub const REGION_RESERVED: u32 = 0; // everything else (runtime services, MMIO, ACPI, unusable)
pub const REGION_ACPI: u32 = 2; // ACPI reclaim / NVS
pub const REGION_IMAGE: u32 = 3; // kernel image + boot archive as loaded by the stub

/// `Framebuffer::format` values. They mirror `EFI_GRAPHICS_PIXEL_FORMAT` on purpose.
pub const PIXEL_RGBX: u32 = 0; // byte order R,G,B,x
pub const PIXEL_BGRX: u32 = 1; // byte order B,G,R,x
pub const PIXEL_BITMASK: u32 = 2; // see `Framebuffer::{red,green,blue}_mask`
pub const PIXEL_NONE: u32 = 0xFFFF_FFFF; // no linear framebuffer (BltOnly) or no GOP at all

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Region {
    pub base: u64,
    pub len: u64,
    pub kind: u32,
    pub _pad: u32,
}

/// UEFI GOP linear framebuffer. `base == 0 || format == PIXEL_NONE` means "absent".
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Framebuffer {
    pub base: u64,
    /// Bytes the framebuffer occupies (GOP `FrameBufferSize`).
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline. Not `width * 4` in general; derived from GOP `PixelsPerScanLine * 4`.
    pub pitch: u32,
    pub bpp: u32,
    pub format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct UefiHandover {
    pub magic: u64,
    pub version: u32,
    /// `size_of::<UefiHandover>()` as the stub saw it.
    pub size: u32,
    /// Exception level the stub was running at when it jumped (1 or 2). The kernel entry detects
    /// `CurrentEL` itself and does not trust this; it is reported for cross-checking.
    pub stub_el: u32,
    pub n_regions: u32,
    pub fb: Framebuffer,
    /// Physical address of the flattened device tree from the EFI configuration table, or 0.
    pub dtb: u64,
    pub dtb_size: u64,
    /// Physical address of the ACPI RSDP, or 0.
    pub rsdp: u64,
    pub kernel_base: u64,
    pub kernel_size: u64,
    pub archive_base: u64,
    /// 0 when no archive was found.
    pub archive_len: u64,
    pub regions: [Region; MAX_REGIONS],
}

impl UefiHandover {
    pub const fn zeroed() -> Self {
        UefiHandover {
            magic: 0,
            version: 0,
            size: 0,
            stub_el: 0,
            n_regions: 0,
            fb: Framebuffer {
                base: 0,
                size: 0,
                width: 0,
                height: 0,
                pitch: 0,
                bpp: 0,
                format: PIXEL_NONE,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
            },
            dtb: 0,
            dtb_size: 0,
            rsdp: 0,
            kernel_base: 0,
            kernel_size: 0,
            archive_base: 0,
            archive_len: 0,
            regions: [Region { base: 0, len: 0, kind: 0, _pad: 0 }; MAX_REGIONS],
        }
    }

    /// Is `self` something this kernel may interpret? Checks identity only, not contents.
    pub fn header_ok(&self) -> bool {
        self.magic == MAGIC
            && self.version == VERSION
            && self.size as usize == core::mem::size_of::<UefiHandover>()
            && (self.n_regions as usize) <= MAX_REGIONS
    }
}

impl Framebuffer {
    /// A framebuffer the kernel can safely write: linear, 32 bpp, pitch covers a line, no overflow.
    pub fn plausible(&self) -> bool {
        if self.base == 0 || self.format == PIXEL_NONE || self.format > PIXEL_BITMASK {
            return false;
        }
        if self.width == 0 || self.height == 0 || self.bpp != 32 {
            return false;
        }
        if (self.pitch as u64) < (self.width as u64) * 4 {
            return false;
        }
        let Some(need) = (self.pitch as u64).checked_mul(self.height as u64) else { return false };
        self.size >= need && self.base.checked_add(need).is_some()
    }
}
