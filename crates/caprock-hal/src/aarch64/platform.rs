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
//! parent bus shifts addresses are skipped, see `Node::cpu_addressable`); only the first
//! PCIe host / GIC is used (a second GIC or ECAM host is ignored, never merged); PSCI
//! function IDs from the tree are ignored (they are architectural, see
//! `caprock_dtb::PsciInfo`); the ACPI path ([`Platform::from_acpi`]) covers GIC, timer and
//! console only — ECAM/MCFG, RAM (which on ACPI machines comes from the UEFI memory map,
//! not from a static table) and virtio-mmio have no source there and stay on the fallback;
//! the x86 HAL keeps its own proven ACPI path (adopting `caprock-acpi` there is a named
//! follow-up, not part of this layer).

use caprock_acpi;
use caprock_dtb::{Dtb, FbInfo, GicVersion, PsciConduit};
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

/// Bits of [`Platform::from_dtb_mask`]: which sections were taken from the firmware
/// description (device tree or ACPI tables, see [`Platform::from_dtb`] / [`Platform::from_acpi`]).
pub mod found {
    pub const UART: u32 = 1 << 0;
    pub const GIC: u32 = 1 << 1;
    pub const TIMER: u32 = 1 << 2;
    pub const PCIE: u32 = 1 << 3;
    pub const VIRTIO_MMIO: u32 = 1 << 4;
    pub const RAM: u32 = 1 << 5;
    pub const FRAMEBUFFER: u32 = 1 << 6;
    pub const RESERVED: u32 = 1 << 7;
    pub const PSCI: u32 = 1 << 8;
}

/// Everything the platform layer needs to know about the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// Sections that came from the firmware description (see [`found`]); 0 = pure fallback.
    pub from_dtb_mask: u32,

    /// Console UART MMIO base and programming model.
    pub uart_base: usize,
    pub uart_kind: UartKind,

    /// PSCI call conduit (`hvc` on QEMU `virt`, `smc` on EL3-firmware hardware).
    pub psci_conduit: PsciConduit,

    pub gic_version: GicVersion,
    /// GIC distributor base.
    pub gicd_base: usize,
    /// GICv2 CPU interface base (unused with GICv3).
    pub gicc_base: usize,
    /// GICv3 redistributor regions `(base, size)`; first `gic_redist_count` valid.
    pub gic_redist: [(u64, u64); caprock_dtb::MAX_REDIST_REGIONS],
    pub gic_redist_count: usize,
    pub gic_redist_stride: u64,
    /// ITS frame `(base, size)` if the description has one. Size is 0 when the source names
    /// no size (the ACPI MADT entry has no length field for the ITS).
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

/// The ACPI tables [`Platform::from_acpi`] reads. Every table is optional (`None` or an
/// empty/unparseable slice = absent from the handover, which is not an error): each
/// section falls back independently, so a GTDT-less firmware (observed: this tree's
/// AAVMF build emits MADT/MCFG/DBG2 but no GTDT) still yields GIC, console and ECAM
/// while the timer stays on its default.
#[derive(Clone, Copy)]
pub struct AcpiTables<'a> {
    pub madt: Option<&'a [u8]>,
    pub gtdt: Option<&'a [u8]>,
    pub spcr: Option<&'a [u8]>,
    pub mcfg: Option<&'a [u8]>,
    pub dbg2: Option<&'a [u8]>,
}

/// Map an ACPI console kind onto the UART models the console driver speaks. SBSA generic
/// UARTs are real consoles, but not models this layer drives; `Unknown` keeps the driver
/// silent (fail-closed) instead of speaking PL011 to the wrong register file.
fn uart_kind_from_acpi(kind: caprock_acpi::ConsoleKind) -> UartKind {
    match kind {
        caprock_acpi::ConsoleKind::Pl011 => UartKind::Pl011,
        caprock_acpi::ConsoleKind::Ns16550 => UartKind::Ns16550,
        _ => UartKind::Unknown,
    }
}

impl Platform {
    /// The QEMU `virt` constants the HAL used before this layer existed.
    pub const QEMU_VIRT: Platform = Platform {
        from_dtb_mask: 0,
        uart_base: 0x0900_0000,
        uart_kind: UartKind::Pl011,
        psci_conduit: PsciConduit::Hvc,
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

        // PSCI conduit: `hvc` on QEMU `virt` (the fallback), `smc` on EL3-firmware hardware.
        // Only the conduit comes from the tree; the function IDs are architectural.
        if let Some(psci) = d.psci() {
            p.psci_conduit = psci.conduit;
            p.from_dtb_mask |= found::PSCI;
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

    /// Build the description from ACPI tables, falling back to [`Self::QEMU_VIRT`] per field —
    /// the ACPI counterpart to [`Self::from_dtb`]. Prefer [`Self::from_acpi_handover`] (the
    /// bootloader-handover form) at boot; this split form exists for tests.
    ///
    /// Boot handover: every input is a byte slice the bootloader hands over, so this stays
    /// pure (bytes in, struct out) exactly like `from_dtb`. On a UEFI boot the RSDP address
    /// arrives via the UEFI configuration table (`EFI_ACPI_20_TABLE_GUID`, see
    /// `caprock_acpi::uefi_select_rsdp`) and the tables themselves are copied before
    /// `ExitBootServices`; on a Multiboot2 boot the RSDP copy arrives as an ACPI tag. The
    /// caller resolves XSDT addresses to slices (see `caprock_acpi::handover_find` for the
    /// list form); this function never touches live firmware memory itself. The x86 HAL does
    /// not call this — it keeps its proven live-memory path (including the legacy BIOS-area
    /// RSDP scan), and adopting `caprock-acpi` there is a named follow-up.
    ///
    /// Coverage: GIC bases + version (MADT), timer INTID (GTDT), ECAM window + bus range
    /// (MCFG, first segment), and the console (SPCR, else first DBG2 serial device). RAM
    /// (which on ACPI machines comes from the UEFI memory map — a different handover, not
    /// a static table), virtio-mmio, framebuffer, reserved ranges and PSCI have no
    /// static-ACPI source here and stay on the fallback. SRAT/SLIT, IORT, PPTT and TPM2 are
    /// parsed and validated by `caprock-acpi` but have no `Platform` field to fill (see the
    /// per-table matrix in the crate docs for who consumes them).
    pub fn from_acpi(t: AcpiTables<'_>) -> Platform {
        let mut p = Platform::QEMU_VIRT;
        // GIC: distributor + version from the first GICD entry; the CPU interface from the
        // first GICC entry on v2 (system registers on v3); redistributors only on v3, where
        // they exist by architecture (a v2 firmware may still emit a type-13 entry, which
        // is parsed faithfully and ignored here — see `caprock_acpi::first_gicr`).
        if let Some(madt_bytes) = t.madt {
            if let Some(madt) = caprock_acpi::Madt::parse(madt_bytes) {
                if let Some(g) = caprock_acpi::gicd(&madt) {
                    p.gicd_base = g.base as usize;
                    if g.version >= 3 {
                        // System-register CPU interface: there is no MMIO CPU-interface base.
                        p.gic_version = GicVersion::V3;
                        p.gicc_base = 0;
                        if let Some(r) = caprock_acpi::first_gicr(&madt) {
                            p.gic_redist[0] = (r.base, r.length as u64);
                            p.gic_redist_count = 1;
                            // A MADT discovery range has no stride field; 0 is the contiguous default.
                            p.gic_redist_stride = 0;
                        }
                    } else {
                        p.gic_version = GicVersion::V2;
                        // A V2 MADT without a GICC entry keeps the QEMU fallback per the standing
                        // per-field rule above (a parsed-but-GICC-less V2 MADT is outside the handled
                        // set; misdriving an unknown CPU interface would be worse than the default).
                        if let Some(c) = caprock_acpi::first_gicc_base(&madt) {
                            p.gicc_base = c as usize;
                        }
                    }
                    if let Some(its) = caprock_acpi::first_its(&madt) {
                        p.gic_its = Some((its, 0));
                    }
                    p.from_dtb_mask |= found::GIC;
                }
            }
        }
        // Timer: GTDT names no frequency (there is no `clock-frequency` equivalent); the
        // live `CNTFRQ_EL0` read stays the only source, so no hint is recorded here.
        if let Some(gtdt_bytes) = t.gtdt {
            if let Some(timer) = caprock_acpi::gtdt(gtdt_bytes) {
                p.timer_intid = timer.ns_el1_gsiv;
                p.from_dtb_mask |= found::TIMER;
            }
        }
        // Console: SPCR first, first DBG2 serial device as the fallback. Only a memory-space
        // address describes an MMIO console this layer can use; an I/O-space one (x86 legacy)
        // is left on the fallback rather than misread.
        let mut uart: Option<(u64, UartKind)> = None;
        if let Some(spcr_bytes) = t.spcr {
            if let Some(u) = caprock_acpi::spcr(spcr_bytes) {
                if u.addr_space == 0 {
                    uart = Some((u.base, uart_kind_from_acpi(u.kind)));
                }
            }
        }
        if uart.is_none() {
            if let Some(dbg2) = t.dbg2 {
                let mut first = None;
                caprock_acpi::dbg2_serial_devices(dbg2, |d| {
                    if first.is_none() && d.addr_space == 0 {
                        first = Some((d.base, uart_kind_from_acpi(d.kind)));
                    }
                });
                uart = first;
            }
        }
        if let Some((base, kind)) = uart {
            p.uart_base = base as usize;
            p.uart_kind = kind;
            p.from_dtb_mask |= found::UART;
        }
        // PCI segments (MCFG, x86 and ARM ECAM alike): the first segment names the ECAM
        // window; the 32-bit MMIO window has no static-ACPI source (it lives in `_CRS`,
        // i.e. AML territory) and stays on the fallback.
        if let Some(mcfg) = t.mcfg {
            if let Some(parsed) = caprock_acpi::Mcfg::parse(mcfg) {
                if let Some((base, size)) = caprock_acpi::mcfg_first_ecam(&parsed) {
                    p.ecam = (base, size);
                    p.from_dtb_mask |= found::PCIE;
                }
            }
        }
        p
    }

    /// [`Self::from_acpi`] over a bootloader handover list (`signature` → table bytes, see
    /// `caprock_acpi::handover_find`). MADT + GTDT are required, everything else is optional:
    /// a handover without SPCR/DBG2 simply has no ACPI console, without MCFG no ACPI ECAM —
    /// none of which is an error.
    pub fn from_acpi_handover(tables: &[(&[u8; 4], &[u8])]) -> Platform {
        Self::from_acpi(AcpiTables {
            madt: caprock_acpi::handover_find(tables, b"APIC"),
            gtdt: caprock_acpi::handover_find(tables, b"GTDT"),
            spcr: caprock_acpi::handover_find(tables, b"SPCR"),
            mcfg: caprock_acpi::handover_find(tables, b"MCFG"),
            dbg2: caprock_acpi::handover_find(tables, b"DBG2"),
        })
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
/// PSCI call conduit announced by the tree (`hvc` on fallback).
pub fn psci_conduit() -> PsciConduit {
    current().psci_conduit
}
/// `true` when PSCI calls go through `smc` (EL3 firmware) rather than `hvc`.
pub fn psci_uses_smc() -> bool {
    current().psci_conduit == PsciConduit::Smc
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
            assert_eq!(p.psci_conduit, PsciConduit::Hvc);
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
    fn psci_conduit_follows_the_tree() {
        assert_eq!(Platform::from_dtb(QEMU).psci_conduit, PsciConduit::Hvc);
        assert_eq!(Platform::from_dtb(SYNTH).psci_conduit, PsciConduit::Smc);
        assert_eq!(Platform::from_dtb(QCOM).psci_conduit, PsciConduit::Smc);
        let p = Platform::from_dtb(SYNTH);
        assert!(p.from_dtb_mask & found::PSCI != 0);
        assert_eq!(Platform::from_dtb(&[]).psci_conduit, PsciConduit::Hvc);
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

    // --- ACPI builders (minimal twins of the `caprock-acpi` test builders; this file is
    // compiled standalone by the host-test harness, so it cannot reuse that crate's tests).
    fn acpi_table(sig: &[u8; 4], rev: u8, body: &[u8]) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&sig[..]);
        v.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
        v.push(rev);
        v.push(0);
        v.extend_from_slice(b"OEMID0");
        v.extend_from_slice(b"OEMTABLE");
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(b"CRID");
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(body);
        let sum = v.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        v[9] = (0u8).wrapping_sub(sum);
        v
    }

    fn acpi_madt(dist_base: u64, version: u8, gicc_base: u64, extra: &[u8]) -> std::vec::Vec<u8> {
        let mut body = std::vec![0u8; 8];
        body.extend_from_slice(&[12u8, 24, 0, 0]);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&dist_base.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(version);
        body.extend_from_slice(&[0u8; 3]);
        // One GICC entry (type 11, length 80) with the CPU-interface base at offset 32.
        body.extend_from_slice(&[11u8, 80, 0, 0]);
        body.extend_from_slice(&[0u8; 28]);
        body.extend_from_slice(&gicc_base.to_le_bytes());
        body.extend_from_slice(&[0u8; 40]);
        body.extend_from_slice(extra);
        acpi_table(b"APIC", 6, &body)
    }

    fn acpi_gtdt(ns_el1: u32) -> std::vec::Vec<u8> {
        let mut body = std::vec::Vec::new();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        for gsiv in [29u32, ns_el1, 27, 26] {
            body.extend_from_slice(&gsiv.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        acpi_table(b"GTDT", 3, &body)
    }

    fn acpi_spcr(interface: u8, base: u64) -> std::vec::Vec<u8> {
        let mut body = std::vec::Vec::new();
        body.push(interface);
        body.extend_from_slice(&[0u8; 3]);
        body.push(0); // GAS memory space
        body.push(4);
        body.push(0);
        body.push(3);
        body.extend_from_slice(&base.to_le_bytes());
        body.push(1);
        body.push(0);
        body.extend_from_slice(&33u32.to_le_bytes());
        body.push(7);
        body.extend_from_slice(&[0u8; 5]);
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 3]);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(0);
        body.extend_from_slice(&[0u8; 4]);
        acpi_table(b"SPCR", 2, &body)
    }

    fn acpi_tables<'a>(
        madt: &'a [u8],
        gtdt: &'a [u8],
        spcr: Option<&'a [u8]>,
        mcfg: Option<&'a [u8]>,
        dbg2: Option<&'a [u8]>,
    ) -> AcpiTables<'a> {
        AcpiTables { madt: Some(madt), gtdt: Some(gtdt), spcr, mcfg, dbg2 }
    }

    fn acpi_mcfg(base: u64, end_bus: u8) -> std::vec::Vec<u8> {
        let mut body = std::vec::Vec::new();
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(&base.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.push(0);
        body.push(end_bus);
        body.extend_from_slice(&[0u8; 4]);
        acpi_table(b"MCFG", 1, &body)
    }

    fn acpi_dbg2_pl011(base: u64) -> std::vec::Vec<u8> {
        let mut body = std::vec::Vec::new();
        body.extend_from_slice(&44u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        let mut d = std::vec::Vec::new();
        d.push(0);
        d.extend_from_slice(&0u16.to_le_bytes());
        d.push(1);
        d.extend_from_slice(&[0u8; 8]);
        d.extend_from_slice(&0x8000u16.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes()); // PL011
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&22u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.push(0);
        d.push(4);
        d.push(0);
        d.push(3);
        d.extend_from_slice(&base.to_le_bytes());
        let n = d.len() as u16;
        d[1..3].copy_from_slice(&n.to_le_bytes());
        body.extend_from_slice(&d);
        acpi_table(b"DBG2", 0, &body)
    }

    #[test]
    fn acpi_qemu_like_tables_reproduce_the_old_constants() {
        let m = acpi_madt(0x0800_0000, 2, 0x0801_0000, &[]);
        let g = acpi_gtdt(30);
        let s = acpi_spcr(3, 0x0900_0000); // PL011
        let c = acpi_mcfg(0x40_1000_0000, 255); // QEMU virt ECAM
        let p = Platform::from_acpi(acpi_tables(&m, &g, Some(&s), Some(&c), None));
        let q = Platform::QEMU_VIRT;
        assert_eq!(p.uart_base, q.uart_base);
        assert_eq!(p.uart_kind, UartKind::Pl011);
        assert_eq!(p.gic_version, GicVersion::V2);
        assert_eq!(p.gicd_base, q.gicd_base);
        assert_eq!(p.gicc_base, q.gicc_base);
        assert_eq!(p.timer_intid, 30);
        assert_eq!(p.timer_freq_hint, None, "ACPI names no timer frequency");
        assert_eq!(p.ecam, q.ecam);
        assert!(p.from_dtb_mask & (found::UART | found::GIC | found::TIMER | found::PCIE) != 0);
        // The handover list form reduces identically.
        let tables: [(&[u8; 4], &[u8]); 4] = [(b"APIC", &m), (b"GTDT", &g), (b"SPCR", &s), (b"MCFG", &c)];
        assert_eq!(Platform::from_acpi_handover(&tables), p);
    }

    #[test]
    fn acpi_dbg2_console_fallback() {
        // No SPCR: the DBG2 serial device becomes the console.
        let m = acpi_madt(0x0800_0000, 2, 0x0801_0000, &[]);
        let g = acpi_gtdt(30);
        let d = acpi_dbg2_pl011(0x0900_0000);
        let p = Platform::from_acpi(acpi_tables(&m, &g, None, None, Some(&d)));
        assert_eq!(p.uart_kind, UartKind::Pl011);
        assert_eq!(p.uart_base, 0x0900_0000);
        assert!(p.from_dtb_mask & found::UART != 0);
        // SPCR wins over DBG2 when both are present.
        let s = acpi_spcr(0, 0x1000_0000); // 16550
        let q = Platform::from_acpi(acpi_tables(&m, &g, Some(&s), None, Some(&d)));
        assert_eq!(q.uart_kind, UartKind::Ns16550);
        assert_eq!(q.uart_base, 0x1000_0000);
    }

    #[test]
    fn acpi_gicv3_names_redistributor_and_its_and_smc_uart_stays_silent() {
        let mut extra = std::vec::Vec::new();
        extra.extend_from_slice(&[13u8, 16, 0, 0]); // GICR
        extra.extend_from_slice(&0x1708_0000u64.to_le_bytes());
        extra.extend_from_slice(&0x30_0000u32.to_le_bytes());
        extra.extend_from_slice(&[14u8, 20, 0, 0]); // ITS
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&0x1704_0000u64.to_le_bytes());
        extra.extend_from_slice(&0u32.to_le_bytes());
        let m = acpi_madt(0x1700_0000, 3, 0, &extra);
        let g = acpi_gtdt(30);
        // SBSA UART: a real console, but not one this layer drives — fail-closed.
        let s = acpi_spcr(0x0D, 0x2_0000_0000);
        let p = Platform::from_acpi(acpi_tables(&m, &g, Some(&s), None, None));
        assert_eq!(p.gic_version, GicVersion::V3);
        assert_eq!(p.gicd_base, 0x1700_0000);
        assert_eq!(p.gicc_base, 0, "GICv3 has no MMIO CPU interface");
        assert_eq!(p.redistributor_regions(), &[(0x1708_0000, 0x30_0000)]);
        assert_eq!(p.gic_its, Some((0x1704_0000, 0)), "the MADT names no ITS size");
        assert_eq!(p.timer_intid, 30);
        assert_eq!(p.uart_kind, UartKind::Unknown);
        assert_eq!(p.uart_base, 0x2_0000_0000);
    }

    #[test]
    fn acpi_garbage_falls_back_entirely() {
        let empty = AcpiTables { madt: None, gtdt: None, spcr: None, mcfg: None, dbg2: None };
        assert_eq!(Platform::from_acpi(empty), Platform::QEMU_VIRT);
        let garbage = AcpiTables {
            madt: Some(&[0u8; 100]),
            gtdt: Some(&[0u8; 100]),
            spcr: None,
            mcfg: None,
            dbg2: None,
        };
        assert_eq!(Platform::from_acpi(garbage), Platform::QEMU_VIRT);
        assert_eq!(Platform::from_acpi_handover(&[]), Platform::QEMU_VIRT);
        // Bad SPCR poisons only the UART section: GIC + timer still apply.
        let m = acpi_madt(0x0800_0000, 2, 0x0801_0000, &[]);
        let g = acpi_gtdt(30);
        let p = Platform::from_acpi(acpi_tables(&m, &g, Some(&[0u8; 64]), None, None));
        assert_eq!(p.gicd_base, 0x0800_0000);
        assert_eq!(p.timer_intid, 30);
        assert_eq!(p.from_dtb_mask & found::UART, 0);
        assert_eq!(p.uart_base, Platform::QEMU_VIRT.uart_base);
    }

    #[test]
    fn real_aavmf_tables_reduce_to_virt_values_without_gtdt() {
        // Genuine AAVMF bytes (see the `caprock-acpi` fixtures). This firmware emits no
        // GTDT, so the timer stays on its default while GIC, console and ECAM come from
        // ACPI — per-field fallback, exactly the GTDT-less case the shape allows.
        static MADT: &[u8] = include_bytes!("../../../caprock-acpi/tests/fixtures/aavmf-virt-madt.bin");
        static DBG2: &[u8] = include_bytes!("../../../caprock-acpi/tests/fixtures/aavmf-virt-dbg2.bin");
        static MCFG: &[u8] = include_bytes!("../../../caprock-acpi/tests/fixtures/aavmf-virt-mcfg.bin");
        let p = Platform::from_acpi(AcpiTables { madt: Some(MADT), gtdt: None, spcr: None, mcfg: Some(MCFG), dbg2: Some(DBG2) });
        let q = Platform::QEMU_VIRT;
        assert_eq!(p.gic_version, GicVersion::V2);
        assert_eq!(p.gicd_base, 0x0800_0000);
        assert_eq!(p.gicc_base, 0x0801_0000);
        assert!(p.redistributor_regions().is_empty(), "v2 has no redistributors (firmware emits a type-13 anyway)");
        assert_eq!(p.uart_kind, UartKind::Pl011);
        assert_eq!(p.uart_base, 0x0900_0000);
        assert_eq!(p.ecam, q.ecam);
        assert_eq!(p.timer_intid, q.timer_intid, "no GTDT: timer stays on default");
        assert_eq!(p.from_dtb_mask & found::TIMER, 0);
        assert!(p.from_dtb_mask & (found::GIC | found::UART | found::PCIE) != 0);
        // The handover list form agrees (no GTDT entry at all in the list).
        let tables: [(&[u8; 4], &[u8]); 3] = [(b"APIC", MADT), (b"DBG2", DBG2), (b"MCFG", MCFG)];
        assert_eq!(Platform::from_acpi_handover(&tables), p);
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
