//! GICv3 interrupt controller driver (distributor, per-core redistributors, system-register
//! CPU interface `ICC_*`). Affinity routing is always on; there is no GICv2 legacy mode here.
//!
//! No ITS (LPIs/MSI translation) — non-goal, see `iommu::interrupt_message_window`.
//!
//! All base addresses are parameters ([`Bases`]), so they can later come from the DTB
//! (`arm,gic-v3`: reg\[0\] = GICD, reg\[1\] = GICR region). The distributor and the redistributor
//! frames are accessed as MMIO in the device-mapped GiB 0; the CPU interface is system registers.
//!
//! Layout used (GIC architecture spec, IHI 0069):
//! * GICD @ `gicd`: `CTLR`, `IGROUPR`, `ISENABLER`, `ICENABLER`, `IPRIORITYR`, `IROUTER` (0x6000).
//! * Per core, two 64 KiB frames in the redistributor region: RD_base (`GICR_WAKER`,
//!   `GICR_TYPER`) and SGI_base = RD_base + 0x10000 (`IGROUPR0`, `ISENABLER0`, ... for
//!   INTID 0..31: SGIs and PPIs, banked per core).
//!
//! Core indexing follows the kernel (`cpu::core_id()` = MPIDR Aff0). Each core records its full
//! 32-bit affinity (Aff3.Aff2.Aff1.Aff0) in [`init_cpu`]; SGI targets and SPI routes use that
//! table, so a core must have run `init_cpu` before it is targeted.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// GIC base addresses. `gicr_len` bounds the redistributor frame walk.
#[derive(Clone, Copy, Debug)]
pub struct Bases {
    pub gicd: usize,
    pub gicr: usize,
    pub gicr_len: usize,
}

/// QEMU `virt` (gic-version=3) layout: GICD 0x0800_0000, GICR 0x080A_0000 (up to 8 cores here).
pub const QEMU_VIRT: Bases = Bases { gicd: 0x0800_0000, gicr: 0x080A_0000, gicr_len: 0x0010_0000 };

/// SGI INTID for the cross-core reschedule IPI (same as the GICv2 driver).
pub const IPI_RESCHED_INTID: u32 = 0;

const MAX_CORES: usize = 256;
const UNSET: u64 = u64::MAX;

static GICD: AtomicUsize = AtomicUsize::new(0);
static GICR: AtomicUsize = AtomicUsize::new(0);
static GICR_LEN: AtomicUsize = AtomicUsize::new(0);
/// Per core: 32-bit affinity (Aff3<<24|Aff2<<16|Aff1<<8|Aff0), or `UNSET`.
static AFF: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(UNSET) }; MAX_CORES];
/// Per core: base of its redistributor RD frame (0 = not found yet).
static RD: [AtomicUsize; MAX_CORES] = [const { AtomicUsize::new(0) }; MAX_CORES];

// GICD registers.
const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_IROUTER: usize = 0x6000;
const GICD_PIDR2: usize = 0xFE8;
const CTLR_ENABLE_GRP0: u32 = 1 << 0;
const CTLR_ENABLE_GRP1: u32 = 1 << 1;
const CTLR_ARE: u32 = 1 << 4;
const CTLR_RWP: u32 = 1 << 31;

// GICR registers (RD frame / SGI frame).
const GICR_TYPER: usize = 0x08;
const GICR_WAKER: usize = 0x14;
const SGI_FRAME: usize = 0x1_0000;
const RD_STRIDE: usize = 0x2_0000;
const GICR_IGROUPR0: usize = SGI_FRAME + 0x080;
const GICR_ISENABLER0: usize = SGI_FRAME + 0x100;
const GICR_ICENABLER0: usize = SGI_FRAME + 0x180;
const GICR_IPRIORITYR: usize = SGI_FRAME + 0x400;
const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const TYPER_LAST: u64 = 1 << 4;

const SPURIOUS_MIN: u32 = 1020;
const DEFAULT_PRIO: u8 = 0x80;

/// `GICD_PIDR2.ArchRev` of the GIC at `gicd`: 2 = GICv2, 3 = GICv3, 4 = GICv4.
pub fn arch_rev(gicd: usize) -> u32 {
    // SAFETY: MMIO read of an ID register in the device-mapped GIC distributor page.
    let pidr2 = unsafe { read_volatile((gicd + GICD_PIDR2) as *const u32) };
    (pidr2 >> 4) & 0xf
}

fn gicd() -> usize {
    GICD.load(Ordering::Relaxed)
}

fn d_read(off: usize) -> u32 {
    // SAFETY: MMIO in the GICD frame configured via `Bases` (device memory).
    unsafe { read_volatile((gicd() + off) as *const u32) }
}
fn d_write(off: usize, v: u32) {
    // SAFETY: as `d_read`.
    unsafe { write_volatile((gicd() + off) as *mut u32, v) }
}
fn mmio_r32(addr: usize) -> u32 {
    // SAFETY: MMIO in a redistributor frame found by `find_rd` (device memory).
    unsafe { read_volatile(addr as *const u32) }
}
fn mmio_w32(addr: usize, v: u32) {
    // SAFETY: as `mmio_r32`.
    unsafe { write_volatile(addr as *mut u32, v) }
}

fn mpidr_aff32() -> u64 {
    let m: u64;
    // SAFETY: read-only register.
    unsafe { asm!("mrs {}, MPIDR_EL1", out(reg) m, options(nomem, nostack, preserves_flags)) };
    ((m >> 32) & 0xff) << 24 | (m & 0x00ff_ffff)
}

fn core_idx() -> usize {
    super::cpu::core_id() % MAX_CORES
}

fn wait_rwp() {
    while d_read(GICD_CTLR) & CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

/// Distributor global init (primary core, once). Records `bases`.
pub fn init_dist(bases: Bases) {
    GICD.store(bases.gicd, Ordering::Relaxed);
    GICR.store(bases.gicr, Ordering::Relaxed);
    GICR_LEN.store(bases.gicr_len, Ordering::Relaxed);
    // Disable, then enable with affinity routing. Bit meanings differ with GICD_CTLR.DS, so
    // enable both groups; unimplemented bits are RAZ/WI.
    d_write(GICD_CTLR, 0);
    wait_rwp();
    let itlines = ((d_read(GICD_TYPER) & 0x1f) + 1) * 32;
    let mut i = 32;
    while i < itlines {
        let n = i as usize;
        d_write(GICD_IGROUPR + 4 * (n / 32), 0xffff_ffff); // Group 1
        d_write(GICD_ICENABLER + 4 * (n / 32), 0xffff_ffff);
        for b in (0..32).step_by(4) {
            d_write(GICD_IPRIORITYR + n + b, u32::from_ne_bytes([DEFAULT_PRIO; 4]));
        }
        i += 32;
    }
    d_write(GICD_CTLR, CTLR_ARE | CTLR_ENABLE_GRP0 | CTLR_ENABLE_GRP1);
    wait_rwp();
}

/// Locate the redistributor whose `GICR_TYPER.Affinity_Value` equals `aff`.
fn find_rd(aff: u64) -> Option<usize> {
    let base = GICR.load(Ordering::Relaxed);
    let len = GICR_LEN.load(Ordering::Relaxed);
    let mut off = 0;
    while off + RD_STRIDE <= len {
        let rd = base + off;
        let lo = mmio_r32(rd + GICR_TYPER) as u64;
        let hi = mmio_r32(rd + GICR_TYPER + 4) as u64;
        let typer = lo | hi << 32;
        if typer >> 32 == aff {
            return Some(rd);
        }
        if typer & TYPER_LAST != 0 {
            return None;
        }
        off += RD_STRIDE;
    }
    None
}

fn my_rd() -> usize {
    RD[core_idx()].load(Ordering::Relaxed)
}

/// Per-core init: wake the redistributor, enable SGIs, bring up the `ICC_*` CPU interface.
pub fn init_cpu() {
    let me = core_idx();
    let aff = mpidr_aff32();
    AFF[me].store(aff, Ordering::Release);
    let rd = match find_rd(aff) {
        Some(rd) => rd,
        None => return, // no redistributor for this core: leave the CPU interface off
    };
    RD[me].store(rd, Ordering::Release);

    // Wake the redistributor.
    let w = mmio_r32(rd + GICR_WAKER) & !WAKER_PROCESSOR_SLEEP;
    mmio_w32(rd + GICR_WAKER, w);
    while mmio_r32(rd + GICR_WAKER) & WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    // SGIs/PPIs: Group 1, default priority, SGIs 0..15 enabled (PPIs are enabled by their users).
    mmio_w32(rd + GICR_IGROUPR0, 0xffff_ffff);
    for b in (0..32).step_by(4) {
        mmio_w32(rd + GICR_IPRIORITYR + b, u32::from_ne_bytes([DEFAULT_PRIO; 4]));
    }
    mmio_w32(rd + GICR_ISENABLER0, 0xffff);

    // CPU interface via system registers. EL1 view: SRE must be allowed by EL2/EL3.
    // SAFETY: GIC CPU-interface system registers; accessible at EL1 once ICC_SRE_EL1.SRE is set.
    unsafe {
        asm!(
            "mrs {t}, ICC_SRE_EL1",
            "orr {t}, {t}, #1",
            "msr ICC_SRE_EL1, {t}",
            "isb",
            "mov {t}, #0xff",
            "msr ICC_PMR_EL1, {t}",
            "msr ICC_BPR1_EL1, xzr",
            "mov {t}, #1",
            "msr ICC_IGRPEN1_EL1, {t}",
            "isb",
            t = out(reg) _,
            options(nomem, nostack)
        );
    }
}

fn banked(intid: u32) -> bool {
    intid < 32
}

/// Enable `intid`. INTID < 32 (SGI/PPI) acts on the calling core's redistributor.
pub fn enable_intid(intid: u32) {
    let reg = (intid / 32) as usize;
    let bit = 1u32 << (intid % 32);
    if banked(intid) {
        let rd = my_rd();
        if rd != 0 {
            mmio_w32(rd + GICR_ISENABLER0, bit);
        }
    } else {
        d_write(GICD_ISENABLER + 4 * reg, bit);
    }
}

/// Mask `intid` (see `enable_intid` for banking).
pub fn mask_intid(intid: u32) {
    let reg = (intid / 32) as usize;
    let bit = 1u32 << (intid % 32);
    if banked(intid) {
        let rd = my_rd();
        if rd != 0 {
            mmio_w32(rd + GICR_ICENABLER0, bit);
        }
    } else {
        d_write(GICD_ICENABLER + 4 * reg, bit);
    }
}

/// Route SPI `intid` to `target_core` (`GICD_IROUTER`, IRM=0). No-op for INTID < 32.
pub fn route_spi(intid: u32, target_core: usize) {
    if banked(intid) {
        return;
    }
    let a = AFF[target_core % MAX_CORES].load(Ordering::Acquire);
    // Not yet online: assume the primary's higher affinities with Aff0 = core index.
    let a = if a == UNSET { (AFF[0].load(Ordering::Acquire) & !0xff) | (target_core as u64 & 0xff) } else { a };
    // IROUTER: Aff3[39:32] Aff2[23:16] Aff1[15:8] Aff0[7:0].
    let r = ((a >> 24) & 0xff) << 32 | (a & 0x00ff_ffff);
    // SAFETY: 64-bit MMIO in the GICD IROUTER array (device memory).
    unsafe { write_volatile((gicd() + GICD_IROUTER + 8 * intid as usize) as *mut u64, r) };
}

/// Send SGI `intid` (0..15) to `target_core` via `ICC_SGI1R_EL1`.
pub fn send_sgi(target_core: usize, intid: u32) {
    let a = AFF[target_core % MAX_CORES].load(Ordering::Acquire);
    let a = if a == UNSET { (AFF[0].load(Ordering::Acquire) & !0xff) | (target_core as u64 & 0xff) } else { a };
    let (aff0, aff1, aff2, aff3) = (a & 0xff, (a >> 8) & 0xff, (a >> 16) & 0xff, (a >> 24) & 0xff);
    let v = (aff3 << 48)
        | (((intid & 0xf) as u64) << 24)
        | (aff2 << 32)
        | (aff1 << 16)
        | ((aff0 >> 4) << 44) // RangeSelector
        | (1u64 << (aff0 & 0xf)); // TargetList
    // SAFETY: ICC_SGI1R_EL1 write; the DSB orders prior stores (e.g. run-queue updates) before it.
    unsafe { asm!("dsb ishst", "msr ICC_SGI1R_EL1, {v}", "isb", v = in(reg) v, options(nomem, nostack)) };
}

/// Acknowledge, dispatch, EOI. Returns the handled INTID (`None` if spurious).
pub fn handle_irq() -> Option<u32> {
    let iar: u64;
    // SAFETY: ICC_IAR1_EL1 read acknowledges the highest-priority pending Group 1 interrupt.
    unsafe { asm!("mrs {}, ICC_IAR1_EL1", out(reg) iar, options(nomem, nostack)) };
    let intid = (iar & 0xff_ffff) as u32;
    if intid >= SPURIOUS_MIN {
        return None;
    }
    if intid == crate::timer::TIMER_INTID {
        crate::timer::on_irq();
    }
    // EOImode=0: one write both drops priority and deactivates.
    // SAFETY: ICC_EOIR1_EL1 write for the INTID just acknowledged.
    unsafe { asm!("msr ICC_EOIR1_EL1, {}", "isb", in(reg) iar, options(nomem, nostack)) };
    Some(intid)
}
