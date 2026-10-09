//! Architektur-spezifischer Boot-Glue.
//!
//! Bleibt im Kernel-Binary (referenziert Linker-Symbole und `kernel_main`).
//! Alles übrige CPU-Spezifische liegt in `caprock-hal`.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

// x86_64-Port (Branch arch/x86_64): Boot-Trampolin + first-light-Serial (Stufe 0). `pub`, damit der
// Panic-Handler die Serial-Ausgabe erreicht. Die volle HAL folgt in den Stufen 1-5.
#[cfg(target_arch = "x86_64")]
pub mod x86_64;
