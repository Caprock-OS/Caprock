//! `intruder-u` — adversarialer UserLand-Testdienst (ext-27 T1, ADR 0012).
//!
//! **Speicher-Isolations-Angriff (hardware-erzwungen).** Ein extern geladener EL0-Prozess der
//! Domaene UserLand versucht, **Kernel-RAM** zu lesen. Da seine isolierte VSpace **nur** die
//! eigenen Segmente + den eigenen Stack abbildet, ist die Ziel-Adresse **nicht** gemappt → der
//! Zugriff loest einen **Translation-Fault** aus → der Kernel **terminiert** diesen Thread und
//! **laeuft weiter** (Beweis: ein geladener Prozess kann Kernel-/Fremdspeicher nicht lesen).
//!
//! Protokoll: Der Dienst signalisiert zuerst ein **PRE**-Badge (Slot 0) — Beleg, dass er lief
//! (eigene VSpace/Code/Stack funktionieren) — und fuehrt **dann** den fatalen Zugriff aus. Der
//! Kernel-Test beobachtet: PRE-Badge erhalten **+** `el0_fault_count` erhoeht **+** Kernel lebt.

#![no_std]
#![no_main]

use libcaprock::{exit, signal};

/// PRE-Badge "INTR" (muss zum Kernel-Test `INTRU_PRE` passen).
pub const PRE_BADGE: u64 = 0x494E_5452;

const REPORT: u64 = 0; // Slot 0: Report-Notification (WRITE-only, Badge PRE_BADGE)

/// Beginn des physischen RAM — dort liegt das **Kernel-Image**. In der isolierten EL0-VSpace dieses
/// Dienstes **nicht** gemappt (die VSpace bildet nur die eigenen Segmente ab 0x6000_0000 + Stack
/// ab) → jeder Lesezugriff faultet.
const KERNEL_RAM: usize = 0x4000_0000;

#[no_mangle]
pub extern "C" fn _start(_arg: usize) -> ! {
    // Nicht-fataler Teil bestanden (der Dienst LAEUFT = eigene VSpace/Code/Stack ok) -> PRE melden,
    // BEVOR der fatale Zugriff den Thread terminiert.
    signal(REPORT, PRE_BADGE);

    // FATAL: Kernel-RAM aus EL0 lesen. Die Adresse ist NICHT in der eigenen isolierten VSpace
    // gemappt -> Translation-Fault -> der Kernel beendet diesen Thread und laeuft weiter.
    // SAFETY: BEWUSST illegaler Zugriff -- genau das ist der Testzweck (Isolationsnachweis). Der
    // Zugriff kehrt nie zurueck (der Kernel terminiert den Thread im Fault-Handler). Liesse der
    // Kernel ihn faelschlich zu, kaeme der Dienst zu `exit()` ohne weiteres Signal -- der Kernel-
    // Test beobachtet dann `el0_fault_count` UNVERAENDERT -> FAIL.
    let _v = unsafe { core::ptr::read_volatile(KERNEL_RAM as *const u64) };

    exit(); // unerreichbar
}
