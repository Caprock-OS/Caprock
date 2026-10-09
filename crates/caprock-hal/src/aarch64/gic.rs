//! GICv2-Interrupt-Controller (QEMU `virt`: `arm,cortex-a15-gic`).
//!
//! Bewusst **nur GICv2, kein GICv3/ITS**: MSI läuft hier über die ITS-Doorbell als
//! gewöhnliche übersetzte Adresse (s. `iommu::interrupt_message_window` = `None`),
//! ein ITS-Treiber ist Non-Goal. The GICv3 driver lives next to this file (`gicv3.rs`);
//! [`super::intc`] selects between the two at boot — kein `cfg` in dieser Datei.
//!
//! GICD (Distributor) @ 0x0800_0000, GICC (CPU-Interface) @ 0x0801_0000.
//! Diese Region ist als Device-Memory gemappt (MMU, ADR 0002), daher sind die
//! volatilen MMIO-Zugriffe wohldefiniert. MMIO-Registerzugriff ist eine erlaubte
//! `unsafe`-Domäne.

use core::ptr::{read_volatile, write_volatile};

const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100; // write-1-to-set, je Bit ein INTID (freigeben)
const GICD_ICENABLER: usize = 0x180; // write-1-to-clear, je Bit ein INTID (maskieren)
const GICD_ITARGETSR: usize = 0x800; // je INTID ein Byte: Ziel-CPU-Maske (nur SPIs >= 32)
const GICD_SGIR: usize = 0xf00; // Software Generated Interrupt (IPI auslösen)

/// SGI-INTID für den Cross-Core-Reschedule-IPI (SGIs belegen INTID 0..15).
pub const IPI_RESCHED_INTID: u32 = 0;

const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004; // Priority Mask
const GICC_IAR: usize = 0x00c; // Interrupt Acknowledge
const GICC_EOIR: usize = 0x010; // End Of Interrupt

const INTID_MASK: u32 = 0x3ff;
const SPURIOUS: u32 = 1023;

fn gicd_write(off: usize, val: u32) {
    // SAFETY: feste MMIO-Adresse des GIC-Distributors (Device-Memory).
    unsafe { write_volatile((GICD_BASE + off) as *mut u32, val) }
}
fn gicc_write(off: usize, val: u32) {
    // SAFETY: feste MMIO-Adresse des GIC-CPU-Interface (Device-Memory).
    unsafe { write_volatile((GICC_BASE + off) as *mut u32, val) }
}
fn gicc_read(off: usize) -> u32 {
    // SAFETY: feste MMIO-Adresse des GIC-CPU-Interface (Device-Memory).
    unsafe { read_volatile((GICC_BASE + off) as *const u32) }
}

/// Distributor global aktivieren. Einmalig (Primärkern).
pub fn init_dist() {
    gicd_write(GICD_CTLR, 1);
}

/// CPU-Interface des aktuellen Kerns aktivieren (alle Prioritäten zulassen).
pub fn init_cpu() {
    gicc_write(GICC_PMR, 0xff);
    gicc_write(GICC_CTLR, 1);
}

/// Eine (private) Interrupt-ID am Distributor freigeben.
///
/// Für PPIs (INTID 16..31) ist `GICD_ISENABLER0` pro Kern gebankt; jeder Kern
/// ruft dies für seine eigene private Quelle (z. B. den Timer) auf.
pub fn enable_intid(intid: u32) {
    let reg = (intid / 32) as usize;
    let bit = intid % 32;
    gicd_write(GICD_ISENABLER + 4 * reg, 1 << bit);
}

/// Eine Interrupt-ID am Distributor **maskieren** (sperren). Verhindert ein Re-Triggern
/// eines level-getriggerten Geräte-IRQ (z. B. RTC), bis er behandelt/wieder freigegeben ist.
pub fn mask_intid(intid: u32) {
    let reg = (intid / 32) as usize;
    let bit = intid % 32;
    gicd_write(GICD_ICENABLER + 4 * reg, 1 << bit);
}

/// Einen **SPI** (shared peripheral interrupt, INTID >= 32) an genau einen Ziel-Kern
/// routen (`GICD_ITARGETSR`: ein Byte je INTID, Bit `target_core` = Ziel-CPU). Für PPIs/
/// SGIs (< 32) ist das Register gebankt/read-only -> No-Op. **Ohne dieses Routing erreicht
/// ein SPI keinen Kern** — der Standard-Reset-Wert kann 0 (kein Ziel) sein.
pub fn route_spi(intid: u32, target_core: usize) {
    if intid < 32 {
        return; // PPI/SGI: gebankt, kein ITARGETSR-Routing
    }
    let off = GICD_ITARGETSR + intid as usize; // Byte-Offset = INTID
    // SAFETY: feste MMIO-Adresse des GIC-Distributors (Device-Memory), Byte-Zugriff.
    unsafe {
        core::ptr::write_volatile((GICD_BASE + off) as *mut u8, 1u8 << (target_core & 0x7));
    }
}

/// Einen Software-generierten Interrupt (SGI/IPI) `intid` (0..15) an genau einen
/// Zielkern `target_core` (0..7) senden. GICv2 `GICD_SGIR`: TargetListFilter=0b00
/// (Liste benutzen), CPUTargetList = `1<<target_core`, SGIINTID = `intid`.
pub fn send_sgi(target_core: usize, intid: u32) {
    let val = ((1u32 << (16 + (target_core & 0x7))) | (intid & 0xf)) as u32;
    gicd_write(GICD_SGIR, val);
}

/// IRQ aus dem Exception-Dispatch behandeln: acknowledgen, zuordnen, EOI.
/// Gibt die behandelte INTID zurück (`None` bei spurious), damit der Aufrufer
/// z. B. den Timer-Tick erkennt.
pub fn handle_irq() -> Option<u32> {
    let iar = gicc_read(GICC_IAR);
    let intid = iar & INTID_MASK;
    if intid == SPURIOUS {
        return None;
    }
    if intid == crate::timer::TIMER_INTID {
        crate::timer::on_irq();
    }
    // EOI mit vollständigem IAR-Wert (inkl. CPUID-Feld bei SGIs).
    gicc_write(GICC_EOIR, iar);
    Some(intid)
}
