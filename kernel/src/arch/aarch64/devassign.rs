//! ARM device offer (MSI strand): the first virtio device on QEMU `virt`.
//!
//! Mirrors the x86 bringup `anbieten` closure (`arch::x86_64::bringup`): resolve
//! the transport, pick the BAR that holds the virtio common config, refuse a
//! device whose MSI-X table lies inside that BAR (E11 — the vector is no
//! driver authority), and hand the resolved facts to
//! [`crate::system::offer_driver_device`]. Only the resolved facts cross into
//! the allocator — never a bus walk (a walk over all buses sees every device
//! of the machine, and exactly that authority stays in the kernel).
//!
//! Wired from ARM boot by a one-line hook (patch P0); until it lands this
//! module is uncalled. It must still build on both targets: the body is
//! aarch64-only by directory, the types it touches are arch-neutral.

use caprock_hal::{self as hal, println};

/// Offer the first virtio device (virtio-blk) as assignable to a driver PD.
///
/// Default entry: the selector admits virtio-blk (the only block device in
/// QEMU `virt` test topologies). The selector — not the scan order — decides
/// *which* device is admitted: the boot disk comes from the DTB/ACPI +
/// bootloader handover (Mitteilung 23), and that caller passes its predicate
/// via [`offer_arm_devices_where`]. Scan order is enumeration, never policy.
pub fn offer_arm_devices() {
    offer_arm_devices_where(|v, d, _, _| {
        v == hal::pcie::VIRTIO_VENDOR && hal::pcie::VIRTIO_BLK_DEVICES.contains(&d)
    })
}

/// Offer the first enumerated virtio device admitted by `sel`.
///
/// `sel(vendor, device, class, rid)` is the caller-provided boot-disk
/// selector seam: whatever the handover names as the boot disk (RID, class,
/// location) is expressed here, and enumeration order never is. A rejected
/// candidate is reported and skipped — silence would read like absence.
/// Today there is at most one candidate per test topology, so first-match
/// iteration suffices; a second block device wants an explicit loop over
/// `dump_devices` in this same file, not a wider net here.
pub fn offer_arm_devices_where(sel: impl Fn(u16, u16, u32, u32) -> bool) {
    // Maps the ECAM window globally as a side effect (same call the selftest
    // uses); without it no config-space read below answers.
    let _ = crate::system::pcie_find_virtio();
    let blk = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_BLK_DEVICES);
    let Some(d) = blk else {
        println!("devarm  : SKIP (no virtio-blk device on the bus)");
        return;
    };
    if !sel(d.vendor, d.device, d.class, d.rid()) {
        println!(
            "devarm  : SKIP (RID {:#06x} {:04x}:{:04x} rejected by the caller selector -- boot disk comes from the handover, not the scan)",
            d.rid(),
            d.vendor,
            d.device
        );
        return;
    };
    let Some(t) = hal::virtio::probe_transport(&d) else {
        println!(
            "devarm  : RID {:#06x} {:04x}:{:04x} has no virtio transport -- not offered",
            d.rid(),
            d.vendor,
            d.device
        );
        return;
    };
    let common = t.common_addr();
    let mut bar = 0u64;
    let mut bar_len = 0u64;
    let mut bar_index = usize::MAX;
    for i in 0..6 {
        let b = d.bars[i];
        if b == 0 {
            continue;
        }
        let len = d.bar_size[i];
        if len != 0 && common >= b && common < b + len {
            bar = b;
            bar_len = (len + 0xfff) & !0xfff;
            bar_index = i;
            break;
        }
    }
    if bar == 0 {
        println!(
            "devarm  : RID {:#06x} offers no BAR holding the common config -- not offered",
            d.rid()
        );
        return;
    }
    // E11: a table inside the offered BAR would let the driver write address
    // and data words itself — and thereby choose where its interrupt lands.
    // Carving a page-granular hole into a region the driver otherwise owns
    // whole is worse than refusing the device, so refuse it, fail-closed,
    // with its own reason.
    let msix = hal::pcie::msix_find(&d);
    let (msix_cap, msix_table, msix_eintraege) = match msix {
        Some(m) => {
            if hal::pcie::msix_in_bar(&m, bar_index) {
                println!(
                    "devarm  : RID {:#06x} NOT offered -- MSI-X table in BAR {bar_index} the driver would get (E11)",
                    d.rid()
                );
                return;
            }
            let basis = d.bars[m.bar as usize];
            if basis == 0 {
                // BAR not assigned -> no interrupt, but the device stays
                // usable. `m.cap` is kept anyway: it is the only thing that
                // tells "no MSI-X at all" (lawful polling) apart from "offers
                // MSI-X and got no vector" (a failed grant).
                (m.cap, 0, 0)
            } else {
                (m.cap, basis + m.offset as u64, m.entries)
            }
        }
        None => (0, 0, 0),
    };
    let ok = crate::system::offer_driver_device(crate::system::DriverDevice {
        rid: d.rid(),
        cfg_page: hal::pcie::cfg_page(&d),
        bar,
        bar_len,
        msix_cap,
        msix_table,
        msix_eintraege,
        vendor: d.vendor,
        device: d.device,
        class: d.class,
    });
    println!(
        "devarm  : offerable: RID {:#06x} {:04x}:{:04x} class {:#08x}, cfg page {:#x}, BAR {:#x}+{:#x}{}",
        d.rid(),
        d.vendor,
        d.device,
        d.class,
        hal::pcie::cfg_page(&d),
        bar,
        bar_len,
        if ok { "" } else { " -- REFUSED, offer list full" }
    );
    println!(
        "devarm  : msix cap={:#06x} table={:#x} entries={} doorbell={:#x} (GICv2m; rows written at grant, E-B2)",
        msix_cap,
        msix_table,
        msix_eintraege,
        hal::pcie::msi_doorbell_addr()
    );
    println!(
        "devarm  : {} device(s) offered (ARM parity: first virtio device via offer_driver_device)",
        crate::system::offered_device_count()
    );
}
