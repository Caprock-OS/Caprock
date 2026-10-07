//! ARM Generic Timer (EL1 physical timer) als periodische Tick-Quelle.
//!
//! Der nicht-sichere EL1-Physical-Timer signalisiert über PPI INTID 30. Wir
//! programmieren ein festes Intervall und laden es bei jedem Tick neu. Pro Kern
//! wird ein Tick-Zähler geführt (Verifikation des Multicore-Timings, P1e).
//!
//! `unsafe` nur für Timer-Systemregisterzugriffe — erlaubte Domäne.

use super::{cpu, gic};
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// PPI of the non-secure EL1 physical timer on QEMU `virt` (the default; the INTID in use comes
/// from [`intid`], i.e. the device tree with this value as the fallback).
pub const TIMER_INTID: u32 = 30;

/// INTID of the EL1 non-secure physical timer, from the platform description.
pub fn intid() -> u32 {
    super::platform::current().timer_intid
}

/// Compile-Zeit-Obergrenze der Kernzahl (nur diese Telemetrie-Tabelle; die tatsächliche
/// Kernzahl ermittelt der Kernel beim Boot).
const MAX_CORES: usize = 256;
static TICKS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
/// Tick-Intervall in Timer-Zählern (für alle Kerne identisch).
static INTERVAL: AtomicU64 = AtomicU64::new(0);

// CNTP_CTL_EL0-Bits.
const CTL_ENABLE: u64 = 1 << 0;
// (IMASK = 1<<1 bleibt 0 -> Interrupt nicht maskiert.)

/// Timer-Frequenz (`CNTFRQ_EL0`) in Hz.
pub fn freq() -> u64 {
    let f: u64;
    // SAFETY: read-only Systemregister.
    unsafe { asm!("mrs {}, CNTFRQ_EL0", out(reg) f, options(nomem, nostack, preserves_flags)) }
    // Some firmware leaves CNTFRQ_EL0 at 0; the device tree's `clock-frequency` is the fallback.
    if f == 0 {
        return super::platform::current().timer_freq_hint.unwrap_or(0) as u64;
    }
    f
}

fn set_tval(ticks: u64) {
    // SAFETY: Timer-Registerzugriff (erlaubte Low-Level-Domäne).
    unsafe { asm!("msr CNTP_TVAL_EL0, {}", in(reg) ticks, options(nomem, nostack, preserves_flags)) }
}

fn set_ctl(val: u64) {
    // SAFETY: Timer-Registerzugriff.
    unsafe { asm!("msr CNTP_CTL_EL0, {}", in(reg) val, options(nomem, nostack, preserves_flags)) }
}

/// Periodischen Tick mit `hz` Hertz am aktuellen Kern starten.
///
/// Setzt das gemeinsame Intervall (idempotent), gibt die Timer-PPI am GIC frei
/// und armiert den Timer. IRQs müssen separat freigegeben werden
/// ([`cpu::local_irq_enable`]).
pub fn init(hz: u64) {
    let interval = freq() / hz;
    INTERVAL.store(interval, Ordering::Relaxed);
    gic::enable_intid(intid());
    set_tval(interval);
    set_ctl(CTL_ENABLE);
}

// --- NOHZ-Umprogrammierung (B-5.2/Z5) --------------------------------------------------------
//
// Der Generic Timer ist von Natur aus einmalig: `CNTP_TVAL_EL0` zaehlt herunter, feuert
// genau einmal, und nur `on_irq` laedt ihn neu -- die Periodik ist Software, kein
// Modus. Ein One-Shot ist also kein zweiter Mechanismus, sondern das Weglassen des
// Nachladens: `arm_oneshot` schreibt einen anderen Wert, `disarm` schaltet ganz ab.
// (Gegenstueck x86: dort ist einmalig ein eigener LVT-Modus ohne `LVT_PERIODIC`.)
//
// Alle drei Funktionen sind reine Registerprogrammierungen ohne Fehlerausgang --
// Fail-closed liegt beim Aufrufer (Idle-/Tick-Pfad): Wer nicht umprogrammiert, tickt
// wie bisher.

/// Zaehler pro Tick bei der periodischen Rate (`TVAL`-Einheiten je Tick).
fn counts_pro_tick() -> u64 {
    INTERVAL.load(Ordering::Relaxed).max(1)
}

/// Groesstes als One-Shot armierbares Delta in Ticks (`TVAL` ist 32-bittig).
pub fn oneshot_max_ticks() -> u64 {
    (u32::MAX as u64 / counts_pro_tick()).max(1)
}

/// Timer ganz entwaffnen (`CNTP_CTL_EL0.ENABLE = 0`).
///
/// Geweckt wird der Kern danach durch alles, was kein Tick ist -- IPI, Geraete-IRQ.
/// Nur zu rufen, wenn die NOHZ-Entscheidung `Disarmed` lautet; alles andere waere ein
/// Kern, der schlaeft, waehrend Arbeit anliegt.
pub fn disarm() {
    set_ctl(0);
}

/// Genau einmal in `delta_ticks` Ticks feuern, danach Stille.
///
/// `0` wird auf `1` aufgerundet (sofort statt nie -- ein One-Shot, der nie feuert, ist
/// ein verlorener Weckruf); groessere Werte werden auf [`oneshot_max_ticks`] gedeckelt
/// (ein Ueberlauf wuerde frueh feuern -- die gefaehrliche Richtung, weil der Weckruf
/// danach verbraucht waere). Der Aufrufer gibt nur Werte `>= 2` herein (s. `nohz_plan`
/// in `caprock-sched`); die Aufrundung ist das Netz, nicht der Weg.
///
/// Schichtetiefe, festgehalten statt vorausgesetzt: Feuert der One-Shot, laeuft
/// [`on_irq`] und laedt `INTERVAL` nach -- der Timer steht danach von selbst wieder
/// periodisch da, auch wenn der Kernel das Rearmieren vergisst. Das ist die sichere
/// Richtung (ein Tick zu viel statt Stille), aber NICHT die volle NOHZ-Semantik: Wer
/// nach dem Weckruf weiter schweigen will, braucht die Tick-seitige Umprogrammierung
/// in `system::reschedule` (s. Patch-Text im Arbeitsergebnis).
pub fn arm_oneshot(delta_ticks: u64) {
    let counts = delta_ticks
        .max(1)
        .saturating_mul(counts_pro_tick())
        .min(u32::MAX as u64)
        .max(1);
    set_tval(counts);
    set_ctl(CTL_ENABLE);
}

/// Zurueck zur periodischen Rate aus dem letzten [`init`]. Nach jedem Aufwachen aus einem
/// One-Shot/Disarmed zu rufen -- unbedingt, nicht bedingt: Im Zweifel tickt der Kern
/// einmal zu viel statt einmal zu wenig (die sichere Richtung).
///
/// Achtung: Wer aus einem One-Shot aufwacht, steht NICHT automatisch wieder periodisch
/// da -- `on_irq` laedt das alte `INTERVAL` nach, das hier aber gerade ueberschrieben
/// wurde. Deshalb stellt diese Funktion das Intervall aus `INTERVAL` wieder her, statt
/// sich auf das Nachladen zu verlassen.
pub fn rearm_periodic() {
    let interval = INTERVAL.load(Ordering::Relaxed);
    if interval == 0 {
        return; // nie armiert -- nichts wiederherzustellen, nichts zu starten
    }
    set_tval(interval);
    set_ctl(CTL_ENABLE);
}

/// Vom IRQ-Dispatch bei einem Timer-Interrupt aufgerufen: neu armieren + zählen.
/// (Die eigentliche Reschedule-Entscheidung trifft der Hook in `exception`.)
pub fn on_irq() {
    set_tval(INTERVAL.load(Ordering::Relaxed));
    let core = cpu::core_id();
    TICKS[core].fetch_add(1, Ordering::Relaxed);
}

/// Tick-Zähler eines Kerns (`0` jenseits der Tabelle — wie `x86_64::timer::ticks`).
pub fn ticks(core: usize) -> u64 {
    TICKS.get(core).map(|t| t.load(Ordering::Relaxed)).unwrap_or(0)
}


// --- Zyklenzähler (Stufe 1) -------------------------------------------------------------------
//
// Gegenstück zu `x86_64::timer::cycles`. Auf ARM ist die Sache einfacher: `CNTPCT_EL0` ist
// architektonisch definiert, läuft mit `CNTFRQ_EL0` und ist per Konstruktion invariant — es gibt
// keinen P-State-abhängigen Zähler, dessen Rate sich unter der Messung ändert.

/// Ein **serialisierender** Zeitstempel des architektonischen Zählers.
///
/// `isb` davor: ohne die Barriere darf der Kern das Lesen gegenüber den umgebenden Befehlen
/// verschieben, und bei kurzen kritischen Sektionen — dem Zweck dieses Primitivs — misst man
/// dann verschobene Grenzen statt der Sektion.
pub fn cycles() -> u64 {
    let v: u64;
    // SAFETY: read-only Systemregister + Instruktionsbarriere.
    unsafe {
        asm!("isb", "mrs {v}, cntpct_el0", v = out(reg) v, options(nostack));
    }
    v
}

/// Zyklen pro Sekunde — hier schlicht `CNTFRQ_EL0`.
pub fn cycles_per_sec() -> u64 {
    freq()
}

/// Der architektonische Zähler ist immer invariant (kein P-State-Einfluss).
pub fn invariant_tsc() -> bool {
    true
}
