//! PSCI (Power State Coordination Interface) — secondary-core start.
//!
//! The call conduit (`hvc` vs `smc`) comes from the platform description (the DTB `/psci`
//! node, QEMU `virt` fallback `hvc`); the function IDs are architectural (PSCI) and
//! identical for both conduits. Inline assembly for a firmware call is an allowed
//! `unsafe` domain.

use core::arch::asm;

/// `CPU_ON` (SMC64-Funktions-ID gemäß QEMU-DTB).
const PSCI_CPU_ON: u64 = 0xC400_0003;
/// `SYSTEM_OFF` (SMC32-Funktions-ID) — fährt die (virtuelle) Maschine herunter.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// PSCI-Statuscodes (Auswahl).
pub const SUCCESS: i64 = 0;

/// Die (virtuelle) Maschine **sauber herunterfahren** (QEMU beendet sich). Wird nach
/// erfolgreichem Testdurchlauf aufgerufen, damit das Test-Skript die vollständige
/// Ausgabe erhält und nicht bis zum Timeout warten muss (kein Abschneiden des Reports).
pub fn system_off() -> ! {
    // SAFETY: PSCI firmware call via HVC/SMC (see `super::platform::psci_uses_smc`); does
    // not return (machine off).
    unsafe {
        if super::platform::psci_uses_smc() {
            asm!("smc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack, noreturn));
        } else {
            asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(nostack, noreturn));
        }
    }
}

/// Einen ausgeschalteten Kern starten.
///
/// * `target_mpidr` — Ziel-CPU als MPIDR-Affinitätswert (QEMU `virt`: Aff0 = Index).
/// * `entry_point`  — physische Einstiegsadresse (MMU des Zielkerns ist noch aus).
/// * `context_id`   — beliebiger Wert, den der Kern in `x0` vorfindet (wir
///   übergeben die Stack-Spitze).
///
/// Rückgabe: PSCI-Statuscode (`SUCCESS` = 0, sonst negativ).
pub fn cpu_on(target_mpidr: u64, entry_point: u64, context_id: u64) -> i64 {
    let ret: i64;
    // SAFETY: PSCI-Firmware-Call via HVC/SMC. x0..x3 sind Argumente/Rückgabe;
    // x4..x17 dürfen laut SMCCC zerstört werden und werden als Clobber markiert.
    unsafe {
        if super::platform::psci_uses_smc() {
            asm!(
                "smc #0",
                inlateout("x0") PSCI_CPU_ON => ret,
                in("x1") target_mpidr,
                in("x2") entry_point,
                in("x3") context_id,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        } else {
            asm!(
                "hvc #0",
                inlateout("x0") PSCI_CPU_ON => ret,
                in("x1") target_mpidr,
                in("x2") entry_point,
                in("x3") context_id,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        }
    }
    ret
}
