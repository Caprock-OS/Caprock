//! Debug-Konsole über die PL011-UART (QEMU `virt`).
//!
//! Reine *Kernel-Debug*-Einrichtung (vgl. seL4-Debug-`printf`). Produktives
//! Logging ist ein Userland-Dienst (ADR 0006).
//!
//! Zwei Pfade:
//! * [`emit_raw`] — lock- und atomic-frei; für **vor** der MMU-Aktivierung und
//!   für den Panic-Handler (siehe ADR 0002: Atomics brauchen die MMU).
//! * [`_print`] (über `println!`) — durch einen Spinlock SMP-serialisiert; erst
//!   nach MMU-Aktivierung benutzen.

use super::platform::{self, UartKind};
use crate::konsole::{self, Schreibordnung};
use core::fmt::{self, Write};

/// MMIO base of the PL011 on QEMU `virt`. Kept as the documented default; the base actually
/// used comes from [`super::platform`] (device tree, with exactly this value as the fallback).
pub const PL011_BASE: usize = 0x0900_0000;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

/// Zustandsloser UART-Sender (alle Register werden direkt per MMIO bedient).
struct Pl011;

impl Pl011 {
    fn put_byte(b: u8) {
        // A console node of another UART model cannot be driven by this PL011 code; stay silent
        // instead of spinning on a foreign register file.
        if platform::uart_kind() != UartKind::Pl011 {
            return;
        }
        let base = platform::uart_base();
        // SAFETY: `base` is the PL011 MMIO address from the platform description (QEMU `virt`
        // constant unless the device tree says otherwise); volatile accesses to device
        // registers alias no Rust memory. Device register access is an allowed `unsafe` domain.
        unsafe {
            let fr = (base + UART_FR) as *const u32;
            while core::ptr::read_volatile(fr) & UART_FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            core::ptr::write_volatile(base as *mut u32, b as u32);
        }
    }

    /// Nimmt der Port jetzt ein Byte an? (`!FR.TXFF`.)
    ///
    /// **Getrennt von [`Self::put_byte`], weil das Warten OHNE Maske laufen muss** (C9b).
    fn bereit() -> bool {
        if platform::uart_kind() != UartKind::Pl011 {
            return true; // nothing to wait for: `put_byte` drops the byte
        }
        let base = platform::uart_base();
        // SAFETY: PL011 MMIO address from the platform description, read only (see `put_byte`).
        unsafe {
            let fr = (base + UART_FR) as *const u32;
            core::ptr::read_volatile(fr) & UART_FR_TXFF == 0
        }
    }

    /// Die CR/LF-Regel steht arch-neutral in [`crate::konsole`] — hier nur der Treiber.
    fn write_str_raw(s: &str) {
        konsole::roh(TREIBER, s);
    }
}

impl Write for Pl011 {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::write_str_raw(s);
        Ok(())
    }
}

/// Die Schreibordnung dieser Konsole (C9b): Besitzrecht ueber die Nachricht, Portsperre je
/// Block. Begruendung und Messung in [`crate::konsole`].
///
/// **Mitgenommen, nicht mitgemessen.** Die Zahl, die den Umbau ausgeloest hat, stammt aus der
/// x86-Konsole; die Sperrhaltedauer-Marke gattert auf aarch64 nicht (todo C9d), und niemand hat
/// die PL011 gemessen. Die FORM des Fehlers war hier aber woertlich dieselbe — eine Haltung ueber
/// das ganze `write_fmt` bei pollend bedientem UART —, und eine Architektur, die man beim
/// Beheben auslaesst, laesst man verrotten (dieselbe Einordnung wie der aarch64-Bau in der
/// Abnahme-Reihe).
/// Der Treiber als Wertepaar: warten und schreiben sind getrennt (s. `konsole::Treiber`).
static TREIBER: konsole::Treiber = konsole::Treiber { bereit: Pl011::bereit, senden: Pl011::put_byte };

static ORDNUNG: Schreibordnung = Schreibordnung::neu();

/// Lock-freie Direktausgabe. Nur für Pre-MMU-Bring-up und den Panic-Handler.
pub fn emit_raw(s: &str) {
    Pl011::write_str_raw(s);
}

/// Lock-freie formatierte Ausgabe. Für den Panic-Handler (der den Lock evtl.
/// gerade selbst hält) und für Pre-MMU-Diagnose.
pub fn emit_fmt(args: fmt::Arguments) {
    let _ = Pl011.write_fmt(args);
}

/// Backing für [`crate::print!`] / [`crate::println!`] (SMP-serialisiert).
#[doc(hidden)]
#[track_caller]
pub fn _print(args: fmt::Arguments) {
    ORDNUNG.drucken(
        super::cpu::core_id(),
        super::cpu::irqs_freigegeben(),
        TREIBER,
        args,
    );
}

/// Was die Schreibordnung gesehen hat — fuer die `konsole`-Berichtszeile.
pub fn schreibstand() -> konsole::Stand {
    ORDNUNG.stand()
}
