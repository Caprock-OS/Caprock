#![no_std]
//! `caprock-hal` — **Hardware-Abstraktion**, architekturselektiv.
//!
//! Kapselt alles Architekturabhängige (CPU-Register, MMU, Exceptions, Interrupt-Controller,
//! Timer, Power/SMP). Sämtliches `unsafe` liegt hier in den von der Projektregel erlaubten
//! Domänen und ist einzeln begründet.
//!
//! ## Struktur (ext-31)
//!
//! Bis ext-30 war diese Crate **aarch64-only**: die Module lagen direkt in `src/` und
//! benutzten aarch64-Assembler. Für den x86_64-Port sind sie nach `src/aarch64/` gewandert
//! und werden per `cfg(target_arch)` ausgewählt; daneben steht `src/x86_64/` mit derselben
//! **öffentlichen API**. Der Kernel-Kern (Caps, Scheduler, IPC, PDs) enthält deshalb
//! **keinerlei** `cfg(target_arch)` — er sieht nur `hal::…`.
//!
//! ```text
//!   caprock-hal
//!    ├── aarch64/   console cpu exception fp intc(GICv2) mmu power(PSCI) timer syscall
//!    │              + pcie smmu virtio   (ARM-/QEMU-`virt`-spezifisch, ext-22..24)
//!    └── x86_64/    console cpu exception fp intc(LAPIC)  mmu power       timer syscall
//! ```
//!
//! ### Namen
//!
//! Die Modulnamen sind **arch-neutral**: `intc` (Interrupt-Controller: GICv2 bzw. LAPIC),
//! `power` (PSCI bzw. ACPI-/QEMU-Abschaltung + SMP-Start). Auf aarch64 bleiben `gic`/`psci`
//! zusätzlich als Alias sichtbar, damit ARM-spezifischer Gerätecode unverändert bleibt.
//!
//! ### Was der x86_64-Port (noch) nicht hat
//!
//! `pcie`/`smmu`/`virtio` sind ARM-/QEMU-`virt`-spezifisch (SMMUv3, ECAM-Fenster des
//! `virt`-Boards). Ihre x86-Entsprechungen (PCI-ECAM über ACPI-MCFG, VT-d/AMD-Vi) sind ein
//! eigener Schritt; bis dahin sind sie **aarch64-only** und der Kernel gated die zugehörigen
//! Subsysteme.

pub(crate) mod hook;
/// Die **Schreibordnung der Debug-Konsole** (C9b) — arch-neutral, weil die Regel es ist: die
/// Portsperre blockweise, das Besitzrecht ueber die ganze Nachricht, und die CR/LF-Regel an
/// **einer** Stelle. Zwei Architekturen, die dieselbe Ordnung getrennt hinschreiben, sind der
/// Riss, durch den eine von beiden zurueckfaellt.
pub(crate) mod konsole;
pub use konsole::Stand as Konsolenstand;
/// Wieviele ausgegebene Bytes die Konsole am Stueck unter der Portsperre haelt — **eine Zahl,
/// eine Quelle**: der Bericht liest sie hier und rechnet sie nicht nach.
pub const KONSOLENBLOCK: usize = konsole::BLOCK;

// --- Architekturauswahl -------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/mod.rs"]
mod imp;

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

// Gemeinsame API-Fläche beider Architekturen.
pub mod cache_decode;
pub mod fault;
// **Die IOMMU-Gesundheit als EINE Aussage** — arch-neutral, weil sie es inhaltlich ist. Bis dahin
// berichtete jede Architektur in ihren eigenen Worten (`smmu` gegen `iommu`/`vtdcaps`), und genau
// das hatte `todo.md` vorab als Warnsignal benannt: zwei Formulierungen sind zwei Entwuerfe.
// Reiner Typ ueber eingespeisten Werten, deshalb einzeln host-testbar (Muster von `dmar.rs`).
pub mod iommu_health;
// Der zweite Summand der C4-Stackrechnung. Arch-neutral, weil er es INHALTLICH ist (zwei
// Atomics); beide `exception`-Module reichen ihn weiter. Siehe Modul-Doku — eine zweite Kopie
// je Architektur war genau der Fehler, der den aarch64-Bau gerissen hat.
pub mod irqtiefe;
// **NUMA topology + the placement ladder** (Z8, N0-N2). Pure over injected firmware bytes for a
// sharper reason than usual: the development machine has ONE node, so the interesting cases cannot
// arise on it -- a test against the real table would be an oracle that holds because its antecedent
// is false.
pub mod bootparams;
pub mod fbtext;
pub mod numa;
// **SMT topology + the one-thread-per-physical-core admission policy** (Z6 stages 0+1). Arch-
// neutral because the *decision* is: the decode differs per architecture (`CPUID.0Bh` against
// `MPIDR_EL1.MT`), the classification and the policy do not. Pure over injected register values,
// so it is host-testable — the `dmar.rs`/`iommu_health.rs` shape.
pub mod smt;
// virtio-pci ist ein PCI-Standard und liegt deshalb arch-neutral (A-5.2); es braucht nur `cpu`
// und `pcie`, die es auf beiden Zweigen gibt.
pub mod virtio;
pub use imp::{cache, console, cpu, exception, fp, intc, mmu, power, syscall, timer};

// ARM-/QEMU-`virt`-spezifische Geräte (noch ohne x86-Entsprechung, s. Modul-Doku).
#[cfg(target_arch = "aarch64")]
pub use imp::{gic, gicv3, iommu, pcie, psci, smmu};

// x86-spezifisch: Segmentierung existiert auf ARM nicht (dort gibt es keine GDT/TSS).
#[cfg(target_arch = "x86_64")]
pub use imp::{acpi, dmar, gdt, iommu, pcie, vtd};

// The cap the MADT reader enforces (`acpi::cpus`). Exported because a caller that wants to hold
// one entry per enumerated CPU must size its array by **the same constant the producer stops at**
// — otherwise the two numbers drift and the difference is a silent truncation, which reads exactly
// like "everything was covered".
#[cfg(target_arch = "x86_64")]
pub use imp::MAX_CPUS;

/// Formatierte Ausgabe auf der Debug-Konsole (gesperrt, SMP-sicher).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

/// Wie [`print!`], mit Zeilenumbruch.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
