//! aarch64 Boot-Trampolin (Primär- und Sekundärkern-Entry).

mod boot;
/// ARM device offer (MSI strand): first virtio device via `offer_driver_device`.
pub mod devassign;
