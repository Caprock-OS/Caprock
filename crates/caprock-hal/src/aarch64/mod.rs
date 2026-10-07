//! **aarch64-Implementierung** der HAL (QEMU `virt`, GICv2, PSCI).
//!
//! Die Module hier hießen bis ext-30 direkt `caprock_hal::*`; sie sind unverändert und
//! werden von [`crate`] per `cfg(target_arch)` ausgewählt. Arch-neutrale Namen (`intc`,
//! `power`) sind Aliase auf die ARM-Bezeichnungen (`gic`, `psci`) — so bleibt der
//! ARM-spezifische Gerätecode (SMMU/PCIe) lesbar, während der Kernel-Kern nur die neutrale
//! API sieht.

pub mod cache;
pub mod console;
pub mod cpu;
pub mod exception;
pub mod fp;
pub mod gic;
pub mod gicv3;
pub mod intc;
pub mod iommu;
pub mod mmu;
pub mod pcie;
pub mod psci;
pub mod smmu;
pub mod syscall;
pub mod timer;

/// Arch-neutraler Name der Power-/SMP-Schnittstelle (hier: PSCI).
pub use psci as power;
