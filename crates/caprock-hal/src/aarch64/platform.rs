//! **Platform description** for aarch64: the board-specific addresses the HAL used to hard-code
//! for QEMU `virt`, read from the device tree at boot.
//!
//! * [`Platform::QEMU_VIRT`] holds the previous constants. It is the value before [`install`]
//!   runs and the per-field fallback whenever the tree lacks (or has an unusable) description,
//!   so behaviour on QEMU `virt` is unchanged by construction.
//! * [`Platform::from_dtb`] is pure (bytes in, struct out) and host-testable with real blobs.
//! * [`install`] publishes the result once; the accessors ([`current`], and the thin wrappers the
//!   console, GIC, timer and PCIe code call) read it afterwards.
//!
//! This file deliberately avoids anything arch-specific so it can be compiled and tested on the
//! host (`tools/host-tests.sh platform`). Only the one-shot publication needs `unsafe`.
//!
//! **Not yet handled** (documented, not hidden): `ranges` translation is not applied (nodes whose
//! parent bus shifts addresses are skipped, see `Node::cpu_addressable`), and only the first
//! PCIe host / GIC is used.

use caprock_dtb::{Dtb, FbInfo, GicVersion};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

/// Maximum number of RAM regions recorded.
pub const MAX_RAM: usize = 8;
/// Maximum number of virtio-mmio transports recorded (QEMU `virt` provides 32).
pub const MAX_VIRTIO_MMIO: usize = 32;
/// Maximum number of reserved ranges recorded.
pub const MAX_RESERVED: usize = 64;

/// Which UART programming model the console node announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartKind {
    /// ARM PrimeCell PL011 (`arm,pl011`): the only model the HAL console drives today.
    Pl011,
    /// 8250/16550 family (`ns16550a`, `snps,dw-apb-uart`, ...).
    Ns16550,
    /// Qualcomm GENI serial engine (`qcom,geni-uart`, `qcom,geni-debug-uart`).
    Geni,
    /// A compatible this layer does not recognise.
    Unknown,
}

impl UartKind {
    fn classify(n: &caprock_dtb::Node) -> UartKind {
        if n.is_compatible("arm,pl011") {
            UartKind::Pl011
        } else if n.is_compatible("ns16550a")
            || n.is_compatible("ns16550")
            || n.is_compatible("snps,dw-apb-uart")
        {
            UartKind::Ns16550
        } else if n.is_compatible("qcom,geni-uart") || n.is_compatible("qcom,geni-debug-uart") {
            UartKind::Geni
        } else {
            UartKind::Unknown
        }
    }
}

/// Bits of [`Platform::from_dtb_mask`]: which sections were taken from the tree.
pub mod found {
    pub const UART: u32 = 1 << 0;
    pub const GIC: u32 = 1 << 1;
    pub const TIMER: u32 = 1 << 2;
    pub const PCIE: u32 = 1 << 3;
    pub const VIRTIO_MMIO: u32 = 1 << 4;
    pub const RAM: u32 = 1 << 5;
    pub const FRAMEBUFFER: u32 = 1 << 6;
    pub const RESERVED: u32 = 1 << 7;
}

/// Everything the platform layer needs to know about the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// Sections that came from the device tree (see [`found`]); 0 = pure fallback.
    pub from_dtb_mask: u32,

    /// Console UART MMIO base and programming model.
    pub uart_base: usize,
    pub uart_kind: UartKind,

    pub gic_version: GicVersion,
    /// GIC distributor base.
    pub gicd_base: usize,
    /// GICv2 CPU interface base (unused with GICv3).
    pub gicc_base: usize,
    /// GICv3 redistributor regions `(base, size)`; first `gic_redist_count` valid.
    pub gic_redist: [(u64, u64); caprock_dtb::MAX_REDIST_REGIONS],
    pub gic_redist_count: usize,
    pub gic_redist_stride: u64,
    pub gic_its: Option<(u64, u64)>,

    /// INTID of the EL1 non-secure physical timer.
    pub timer_intid: u32,
    /// `clock-frequency` from the tree, used only when `CNTFRQ_EL0` reads 0.
    pub timer_freq_hint: Option<u32>,

    /// PCIe ECAM window `(base, size)`.
    pub ecam: (u64, u64),
    /// 32-bit PCI memory window as CPU `(base, end)` (end exclusive).
    pub mmio32: (u64, u64),

    pub virtio_mmio: [(u64, u64); MAX_VIRTIO_MMIO],
    pub virtio_mmio_count: usize,

    pub ram: [(u64, u64); MAX_RAM],
    pub ram_count: usize,

    pub reserved: [(u64, u64); MAX_RESERVED],
    pub reserved_count: usize,

    pub framebuffer: Option<FbInfo>,
}

impl Platform {
    /// The QEMU `virt` constants the HAL used before this layer existed.
    pub const QEMU_VIRT: Platform = Platform {
        from_dtb_mask: 0,
        uart_base: 0x0900_0000,
        uart_kind: UartKind::Pl011,
        gic_version: GicVersion::V2,
        gicd_base: 0x0800_0000,
        gicc_base: 0x0801_0000,
        gic_redist: [(0, 0); caprock_dtb::MAX_REDIST_REGIONS],
        gic_redist_count: 0,
        gic_redist_stride: 0,
        gic_its: None,
        timer_intid: 30,
        timer_freq_hint: None,
        ecam: (0x40_1000_0000, 0x1000_0000),
        mmio32: (0x1000_0000, 0x3eff_0000),
        virtio_mmio: [(0, 0); MAX_VIRTIO_MMIO],
        virtio_mmio_count: 0,
        ram: [(0, 0); MAX_RAM],
        ram_count: 0,
        reserved: [(0, 0); MAX_RESERVED],
        reserved_count: 0,
        framebuffer: None,
    };

    /// Build the description from a device tree, falling back to [`Self::QEMU_VIRT`] per field.
    pub fn from_dtb(bytes: &[u8]) -> Platform {
        let mut p = Platform::QEMU_VIRT;
        let d = match Dtb::parse(bytes) {
            Some(d) => d,
            None => return p,
        };

        // Console: /chosen/stdout-path, else the first PL011 node.
        let uart = d
            .stdout_node()
            .filter(|n| n.cpu_addressable())
            .or_else(|| d.find_compatible("arm,pl011"));
        if let Some(n) = uart {
            if let Some((base, _)) = n.reg(0) {
                // The base is recorded whatever the model; the console driver checks
                // `uart_kind` and stays silent for a model it cannot drive, rather than
                // speaking PL011 to a GENI or 16550 register file.
                p.uart_kind = UartKind::classify(&n);
                p.uart_base = base as usize;
                p.from_dtb_mask |= found::UART;
            }
        }

        if let Some(g) = d.gic() {
            p.gic_version = g.version;
            p.gicd_base = g.dist.0 as usize;
            if let Some((c, _)) = g.cpu_if {
                p.gicc_base = c as usize;
            }
            p.gic_redist = g.redist;
            p.gic_redist_count = g.redist_count;
            p.gic_redist_stride = g.redist_stride;
            p.gic_its = g.its;
            p.from_dtb_mask |= found::GIC;
        }

        if let Some(t) = d.armv8_timer() {
            if let Some(id) = t.ns_phys_intid() {
                p.timer_intid = id;
                p.timer_freq_hint = t.frequency;
                p.from_dtb_mask |= found::TIMER;
            }
        }

        if let Some(h) = d.pci_host() {
            p.ecam = h.ecam;
            if let Some((b, s)) = h.mmio32 {
                p.mmio32 = (b, b.saturating_add(s));
            }
            p.from_dtb_mask |= found::PCIE;
        }

        let mut n = 0;
        d.for_each_compatible("virtio,mmio", |node| {
            if n < MAX_VIRTIO_MMIO && node.cpu_addressable() {
                if let Some(r) = node.reg(0) {
                    p.virtio_mmio[n] = r;
                    n += 1;
                }
            }
        });
        if n > 0 {
            p.virtio_mmio_count = n;
            p.from_dtb_mask |= found::VIRTIO_MMIO;
        }

        let mut n = 0;
        d.for_each_memory(|b, s| {
            if s != 0 && n < MAX_RAM {
                p.ram[n] = (b, s);
                n += 1;
            }
        });
        if n > 0 {
            p.ram_count = n;
            p.from_dtb_mask |= found::RAM;
        }

        let mut n = 0;
        d.for_each_reserved(|r| {
            if n < MAX_RESERVED {
                p.reserved[n] = (r.base, r.size);
                n += 1;
            }
        });
        if n > 0 {
            p.reserved_count = n;
            p.from_dtb_mask |= found::RESERVED;
        }

        let mut fb = None;
        d.for_each_framebuffer(|f| {
            if fb.is_none() {
                fb = Some(f);
            }
        });
        if fb.is_some() {
            p.framebuffer = fb;
            p.from_dtb_mask |= found::FRAMEBUFFER;
        }
        p
    }

    /// RAM regions found in the tree (empty on pure fallback).
    pub fn ram_regions(&self) -> &[(u64, u64)] {
        &self.ram[..self.ram_count]
    }
    /// Reserved ranges found in the tree.
    pub fn reserved_ranges(&self) -> &[(u64, u64)] {
        &self.reserved[..self.reserved_count]
    }
    /// virtio-mmio transports found in the tree.
    pub fn virtio_mmio_regions(&self) -> &[(u64, u64)] {
        &self.virtio_mmio[..self.virtio_mmio_count]
    }
    /// GICv3 redistributor regions (empty on GICv2).
    pub fn redistributor_regions(&self) -> &[(u64, u64)] {
        &self.gic_redist[..self.gic_redist_count]
    }
}

// --- one-shot publication ---------------------------------------------------------------------

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

struct Slot(UnsafeCell<Platform>);
// SAFETY: the cell is written exactly once, by `install`, before `STATE` becomes `READY`
// (release); readers only touch it after observing `READY` (acquire).
unsafe impl Sync for Slot {}

static STATE: AtomicU8 = AtomicU8::new(EMPTY);
static SLOT: Slot = Slot(UnsafeCell::new(Platform::QEMU_VIRT));
static DEFAULT: Platform = Platform::QEMU_VIRT;

/// Publish `p` as the platform. Intended to be called once by the boot core, after the MMU is up
/// (it uses only plain loads and stores, no exclusive access) and before secondary cores start.
/// Returns `false` (and changes nothing) if a platform was already installed.
pub fn install(p: Platform) -> bool {
    // Plain load/store rather than compare-exchange: single boot core, and it also works before
    // the MMU is enabled (exclusives need Normal memory).
    if STATE.load(Ordering::Acquire) != EMPTY {
        return false;
    }
    STATE.store(WRITING, Ordering::Relaxed);
    // SAFETY: state was EMPTY and is now WRITING, so no reader dereferences the cell and no
    // other writer is past the check (single boot core, documented above).
    unsafe { *SLOT.0.get() = p };
    STATE.store(READY, Ordering::Release);
    true
}

/// The installed platform, or [`Platform::QEMU_VIRT`] before [`install`] has run.
pub fn current() -> &'static Platform {
    if STATE.load(Ordering::Acquire) == READY {
        // SAFETY: READY is only stored after the one write completed; the cell is never written
        // again, so a shared reference for `'static` is sound.
        unsafe { &*SLOT.0.get() }
    } else {
        &DEFAULT
    }
}

// --- accessors used by the existing aarch64 code ----------------------------------------------

/// Console UART programming model.
pub fn uart_kind() -> UartKind {
    current().uart_kind
}
/// Console UART MMIO base.
pub fn uart_base() -> usize {
    current().uart_base
}
/// GIC distributor base.
pub fn gicd_base() -> usize {
    current().gicd_base
}
/// GICv2 CPU interface base.
pub fn gicc_base() -> usize {
    current().gicc_base
}
/// GICv3 redistributor regions for a GICv3 module (empty on GICv2 / fallback).
pub fn gic_redistributors() -> &'static [(u64, u64)] {
    current().redistributor_regions()
}
/// GIC architecture version announced by the tree (GICv2 on fallback).
pub fn gic_version() -> GicVersion {
    current().gic_version
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    static QEMU: &[u8] = include_bytes!("../../../caprock-dtb/tests/fixtures/qemu-virt-smmu-8cpu.dtb");
    static EMBEDDED: &[u8] = include_bytes!("../../../../kernel/src/virt.dtb");
    static QCOM: &[u8] = include_bytes!("../../../caprock-dtb/tests/fixtures/x1p42100-asus-vivobook-s15.dtb");
    static SYNTH: &[u8] = include_bytes!("../../../caprock-dtb/tests/fixtures/synthetic-fb.dtb");

    /// The central claim: on QEMU `virt` the tree reproduces every constant the HAL hard-coded.
    #[test]
    fn qemu_tree_reproduces_the_old_constants() {
        for blob in [QEMU, EMBEDDED] {
            let p = Platform::from_dtb(blob);
            let q = Platform::QEMU_VIRT;
            assert_eq!(p.uart_base, q.uart_base);
            assert_eq!(p.uart_kind, UartKind::Pl011);
            assert_eq!(p.gic_version, q.gic_version);
            assert_eq!(p.gicd_base, q.gicd_base);
            assert_eq!(p.gicc_base, q.gicc_base);
            assert_eq!(p.timer_intid, q.timer_intid);
            assert_eq!(p.timer_freq_hint, None);
            assert_eq!(p.ecam, q.ecam);
            assert_eq!(p.mmio32, q.mmio32);
            assert_eq!(p.ram_regions(), &[(0x4000_0000, 0x1_0000_0000)]);
            assert_eq!(p.virtio_mmio_count, 32);
            assert_eq!(p.virtio_mmio_regions()[0], (0x0a00_0000, 0x200));
            assert_eq!(p.framebuffer, None);
        }
    }

    #[test]
    fn garbage_and_empty_fall_back_entirely() {
        assert_eq!(Platform::from_dtb(&[]), Platform::QEMU_VIRT);
        assert_eq!(Platform::from_dtb(&[0u8; 100]), Platform::QEMU_VIRT);
    }

    #[test]
    fn qualcomm_tree_describes_a_gicv3_machine() {
        let p = Platform::from_dtb(QCOM);
        assert_eq!(p.gic_version, GicVersion::V3);
        assert_eq!(p.gicd_base, 0x1700_0000);
        assert_eq!(p.redistributor_regions(), &[(0x1708_0000, 0x30_0000)]);
        assert_eq!(p.gic_redist_stride, 0x4_0000);
        assert_eq!(p.gic_its, Some((0x1704_0000, 0x4_0000)));
        assert_eq!(p.timer_intid, 30);
        assert!(p.from_dtb_mask & found::GIC != 0);
        assert!(p.from_dtb_mask & found::RESERVED != 0);
        assert_eq!(p.ram_count, 0, "firmware fills /memory at boot; the tree has size 0");
        assert!(p.reserved_count > 30);
        // No PCIe ECAM generic host, no stdout, no virtio: those stay on the fallback.
        assert_eq!(p.from_dtb_mask & found::PCIE, 0);
        assert_eq!(p.from_dtb_mask & found::VIRTIO_MMIO, 0);
    }

    #[test]
    fn synthetic_tree_picks_up_framebuffer_and_foreign_uart() {
        let p = Platform::from_dtb(SYNTH);
        assert_eq!(p.uart_kind, UartKind::Ns16550);
        assert_eq!(p.uart_base, 0x1000_0000);
        let fb = p.framebuffer.unwrap();
        assert_eq!((fb.width, fb.height, fb.stride), (1920, 1080, 7680));
        assert_eq!(p.ram_count, 2);
        assert_eq!(p.reserved_count, 2);
        assert_eq!(p.gic_version, GicVersion::V2);
        assert_eq!(p.gicd_base, 0x2c00_1000);
        assert_eq!(p.gicc_base, 0x2c00_2000);
    }

    #[test]
    fn install_is_one_shot_and_current_defaults() {
        // `current()` before install is the QEMU default; this test owns the process-wide slot.
        assert_eq!(*current(), Platform::QEMU_VIRT);
        let p = Platform::from_dtb(SYNTH);
        assert!(install(p));
        assert!(!install(Platform::QEMU_VIRT), "second install must be refused");
        assert_eq!(current().gicd_base, 0x2c00_1000);
        assert_eq!(gicc_base(), 0x2c00_2000);
        assert_eq!(uart_base(), 0x1000_0000);
    }
}
