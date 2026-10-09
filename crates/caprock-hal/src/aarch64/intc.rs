//! Runtime selection between the GICv2 driver ([`super::gic`], unchanged) and the GICv3 driver
//! ([`super::gicv3`]). This is the arch-neutral `intc` seen by the kernel.
//!
//! Selection happens once in [`init_dist`] on the primary core, before secondaries start:
//! * [`configure_v3`] (e.g. from a DTB `arm,gic-v3` node) forces GICv3 with the given bases;
//! * otherwise [`super::gicv3::probe_v3`] decides from the distributor registers
//!   (`GICD_PIDR2.ArchRev`, backed by GICv3-only `GICD_TYPER` fields because QEMU leaves
//!   the GIC PIDRs RAZ).
//!
//! Without any configuration the behaviour on a GICv2 machine is exactly the old one.

use super::{gic, gicv3};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const MODE_UNSET: u8 = 0;
const MODE_V2: u8 = 2;
const MODE_V3: u8 = 3;

static MODE: AtomicU8 = AtomicU8::new(MODE_UNSET);
static CFG_GICD: AtomicUsize = AtomicUsize::new(0);
static CFG_GICR: AtomicUsize = AtomicUsize::new(0);
static CFG_GICR_LEN: AtomicUsize = AtomicUsize::new(0);

/// SGI INTID of the cross-core reschedule IPI (identical in both drivers).
pub const IPI_RESCHED_INTID: u32 = gic::IPI_RESCHED_INTID;

/// Force GICv3 with explicit base addresses (call before [`init_dist`]).
pub fn configure_v3(b: gicv3::Bases) {
    CFG_GICD.store(b.gicd, Ordering::Relaxed);
    CFG_GICR.store(b.gicr, Ordering::Relaxed);
    CFG_GICR_LEN.store(b.gicr_len, Ordering::Relaxed);
    MODE.store(MODE_V3, Ordering::Release);
}

/// GIC architecture major version in use (2 or 3; 0 before [`init_dist`]).
pub fn version() -> u8 {
    MODE.load(Ordering::Acquire)
}

fn v3() -> bool {
    MODE.load(Ordering::Acquire) == MODE_V3
}

/// Distributor init on the primary core; decides v2/v3 if not configured.
pub fn init_dist() {
    if MODE.load(Ordering::Acquire) == MODE_UNSET {
        if gicv3::probe_v3(gicv3::QEMU_VIRT.gicd) {
            configure_v3(gicv3::QEMU_VIRT);
        } else {
            MODE.store(MODE_V2, Ordering::Release);
        }
    }
    if v3() {
        gicv3::init_dist(gicv3::Bases {
            gicd: CFG_GICD.load(Ordering::Relaxed),
            gicr: CFG_GICR.load(Ordering::Relaxed),
            gicr_len: CFG_GICR_LEN.load(Ordering::Relaxed),
        });
    } else {
        gic::init_dist();
    }
}

pub fn init_cpu() {
    if v3() { gicv3::init_cpu() } else { gic::init_cpu() }
}
pub fn enable_intid(intid: u32) {
    if v3() { gicv3::enable_intid(intid) } else { gic::enable_intid(intid) }
}
pub fn mask_intid(intid: u32) {
    if v3() { gicv3::mask_intid(intid) } else { gic::mask_intid(intid) }
}
pub fn route_spi(intid: u32, target_core: usize) {
    if v3() { gicv3::route_spi(intid, target_core) } else { gic::route_spi(intid, target_core) }
}
pub fn send_sgi(target_core: usize, intid: u32) {
    if v3() { gicv3::send_sgi(target_core, intid) } else { gic::send_sgi(target_core, intid) }
}
pub fn handle_irq() -> Option<u32> {
    if v3() { gicv3::handle_irq() } else { gic::handle_irq() }
}
