//! MSI-X table programming for ARM PCIe (QEMU `virt`, GICv2m).
//!
//! aarch64 counterpart of the x86 MSI-X section in `x86_64/pcie.rs`. The PCI
//! capability walk, the row format and the enable/quiesce discipline are the
//! PCI standard and therefore identical; what differs is the *address* a row
//! carries and the *fence* after writing it:
//!
//! * x86 rows carry an IRTE handle (`hal::vtd::msi_addr`); ARM rows carry the
//!   GICv2m doorbell address ([`msi_doorbell_addr`]) with the SPI number as
//!   data. GICv2m has no remapping stage, so the kernel-owned routing decision
//!   is *which* SPI number the row carries, written by the kernel at grant
//!   time — the same authority rule as x86 E-B2 (the handle/vector is a
//!   number, not an authority, and the driver must not choose it).
//! * x86 fences row writes with `mfence`; ARM needs `dsb sy` before the
//!   device may observe the row.
//!
//! The file is intentionally self-contained (no `super::` imports) so it stays
//! host-testable as a single file (`rustc --test`), the same shape as
//! `x86_64/irte.rs` and `x86_64/dmar.rs`. Hardware access enters through
//! injected readers ([`msix_find_with`]) or explicit addresses (row access);
//! the thin `PciDevice`/RID binding lives in `super::pcie`, mirroring the x86
//! function names so the grant path reads identically on both arches.

/// Capability ID of the MSI-X structure (PCI 3.0, `PCI_CAP_ID_MSIX`).
pub const CAP_ID_MSIX: u8 = 0x11;
/// Offset of the capability list in configuration space.
pub const CFG_CAP_PTR: u16 = 0x34;
/// `Status` register; bit 4 says whether there is a capability list at all.
pub const CFG_STATUS: u16 = 0x06;
/// That bit.
pub const STATUS_CAP_LIST: u16 = 1 << 4;
/// Step bound for the capability walk: a device-owned linked list may be
/// cyclic (broken or hostile hardware), and the kernel must not hang in it.
/// Same bound as the x86 walk and `caprock_virtio::probe_ecam`.
pub const WALK_BUDGET: u32 = 48;

/// **Where the MSI-X table of a device lives.**
///
/// `bar` is the BAR **index** (0..5), not the address: only together with the
/// device's BAR assignment does it yield a location, and the split is on
/// purpose — the grant question ("does the table lie inside the BAR the
/// driver is offered?") is a question about the INDEX, and phrased as an
/// address comparison it would misfire on overlapping windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixInfo {
    /// Offset of the capability in configuration space (for enable/quiesce).
    pub cap: u16,
    /// BAR index of the table.
    pub bar: u8,
    /// Offset of the table inside that BAR.
    pub offset: u32,
    /// Number of table rows (`Table Size` + 1).
    pub entries: u16,
}

/// BAR index out of a Table-offset register value (`cap + 4`).
#[inline]
pub fn table_bar(tbl: u32) -> u8 {
    (tbl & 0b111) as u8
}

/// Byte offset out of a Table-offset register value.
#[inline]
pub fn table_offset(tbl: u32) -> u32 {
    tbl & !0b111
}

/// Row count out of a Message-Control value (`Table Size` + 1).
#[inline]
pub fn entry_count(ctrl: u16) -> u16 {
    (ctrl & 0x7ff) + 1
}

/// `0xffff` means "no device answers here" (same guard as the x86 walk).
#[inline]
pub fn all_ones16(v: u16) -> bool {
    v == 0xffff
}

/// **Find the MSI-X capability of a device.** `None` = it carries none.
///
/// The readers address configuration space by 8/16/32-bit offset *within this
/// function's space*; the `PciDevice`-bound wrapper in `super::pcie` supplies
/// them. The walk mirrors the x86 one, including the two guards that make
/// "no MSI-X" the honest answer instead of a burnt step budget: an absent
/// device answers `0xffff` (which HAS `STATUS_CAP_LIST` set), and a dead
/// space mid-walk answers capability ID `0xff`.
pub fn msix_find_with(
    r8: &dyn Fn(u16) -> u8,
    r16: &dyn Fn(u16) -> u16,
    r32: &dyn Fn(u16) -> u32,
) -> Option<MsixInfo> {
    let status = r16(CFG_STATUS);
    if all_ones16(status) || status & STATUS_CAP_LIST == 0 {
        return None;
    }
    let mut cap = (r8(CFG_CAP_PTR) & 0xfc) as u16;
    let mut guard = 0u32;
    while cap != 0 && guard < WALK_BUDGET {
        guard += 1;
        let id = r8(cap);
        if id == 0xff {
            return None; // config space died mid-walk — same case as above, one level in
        }
        if id == CAP_ID_MSIX {
            let ctrl = r16(cap + 2);
            let tbl = r32(cap + 4);
            return Some(MsixInfo {
                cap,
                bar: table_bar(tbl),
                offset: table_offset(tbl),
                entries: entry_count(ctrl),
            });
        }
        cap = (r8(cap + 1) & 0xfc) as u16;
    }
    None
}

/// **Does the MSI-X table lie inside the BAR with index `bar_index`?**
///
/// The question on which the grant decides whether the device is offered at
/// all: a table inside the driver's BAR would let the driver write address
/// and data words itself — and thereby choose where its interrupt lands.
#[inline]
pub fn msix_in_bar(info: &MsixInfo, bar_index: usize) -> bool {
    info.bar as usize == bar_index
}

/// **Write one MSI-X table row** and unmask it.
///
/// # Safety
/// `table` must be the identity-mapped base of this device's MSI-X table and
/// `row < info.entries` must hold. The caller holds the device exclusively.
///
/// Deliberate order: **address and data first, mask release last.** The other
/// way round opens a window in which the row is unmasked and still points at
/// `0` — an interrupt in that window would target vector 0.
pub unsafe fn msix_write_entry(table: u64, row: u16, addr_lo: u32, data: u32) {
    let e = table + (row as u64) * 16;
    // SAFETY: see function docs; the table is a device window, hence `volatile`.
    unsafe {
        core::ptr::write_volatile(e as *mut u32, addr_lo);
        core::ptr::write_volatile((e + 4) as *mut u32, 0); // address high: 0 (below 4 GiB)
        core::ptr::write_volatile((e + 8) as *mut u32, data);
        fence_rows();
        core::ptr::write_volatile((e + 12) as *mut u32, 0); // Vector Control: release mask
    }
}

/// **Read one row back**: `(addr_lo, addr_hi, data, vector_control)`.
///
/// For the checker, and the emphasis is on *back*: the row lives in the
/// device's BAR, and whoever wants to know whether it still holds must read
/// it. Recomputing what the kernel wrote would only prove that it wrote it —
/// not that it still holds. A device reset clears rows, and in QEMU
/// `virtio_pci_reset` does exactly that.
///
/// # Safety
/// like [`msix_write_entry`].
pub unsafe fn msix_read_entry(table: u64, row: u16) -> (u32, u32, u32, u32) {
    let e = table + (row as u64) * 16;
    // SAFETY: see function docs.
    unsafe {
        (
            core::ptr::read_volatile(e as *const u32),
            core::ptr::read_volatile((e + 4) as *const u32),
            core::ptr::read_volatile((e + 8) as *const u32),
            core::ptr::read_volatile((e + 12) as *const u32),
        )
    }
}

/// Mask one row again (teardown, counter-proof).
///
/// # Safety
/// like [`msix_write_entry`].
pub unsafe fn msix_mask_entry(table: u64, row: u16) {
    let e = table + (row as u64) * 16;
    // SAFETY: see function docs.
    unsafe { core::ptr::write_volatile((e + 12) as *mut u32, 1) };
}

/// Make row writes visible to the device before it may observe them: `dsb
/// sy` on ARM (the x86 walk uses `mfence` for the same place). On the host
/// test build this is a compiler fence — ordering against silicon is not what
/// a host test measures; the row *values* are.
#[inline]
fn fence_rows() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// `MSI-X Control` (capability + 2) — the two bits this kernel ever writes.
pub const MSIX_CTRL_ENABLE: u16 = 1 << 15;
/// Function Mask: while set, **no** vector of this function fires, whatever
/// the per-row mask says. The switch that stills a device without MSI-X
/// fallback (clearing Enable would fall back to INTx, which nothing routes).
pub const MSIX_CTRL_FUNC_MASK: u16 = 1 << 14;

/// Pure transition for arming MSI-X (`Enable` on, Function Mask off).
#[inline]
pub fn msix_enable_ctrl(ctrl: u16) -> u16 {
    (ctrl | MSIX_CTRL_ENABLE) & !MSIX_CTRL_FUNC_MASK
}

/// Pure transition for stilling MSI-X (Function Mask on, Enable untouched).
#[inline]
pub fn msix_quiesce_ctrl(ctrl: u16) -> u16 {
    ctrl | MSIX_CTRL_FUNC_MASK
}

// --- ARM delivery: GICv2m doorbell ---------------------------------------------------------
//
// QEMU `virt` with GICv2 carries a GICv2m MSI frame; a device triggers SPI
// `n` by writing `n` to the SETSPI register. The programmed row address is
// therefore the doorbell, the data word the SPI number — no IRTE, no
// translation, and hence no per-source check in hardware. The latch is the
// kernel-written row (E-B2 rule), not the fabric.

/// GICv2m MSI frame base on QEMU `virt`.
pub const GICV2M_FRAME_BASE: u64 = 0x0802_0000;
/// SETSPI register offset inside the frame (non-secure write port).
pub const GICV2M_SETSPI_OFF: u64 = 0x040;

/// Address an MSI-X row must carry on this board: the doorbell.
#[inline]
pub fn msi_doorbell_addr() -> u64 {
    GICV2M_FRAME_BASE + GICV2M_SETSPI_OFF
}

/// Data word for SPI `spi`: the number itself (GICv2m SETSPI semantics).
#[inline]
pub fn msi_data_for_spi(spi: u32) -> u32 {
    spi
}

/// Lowest shareable SPI (INTID 32); below lie SGIs/PPIs, which are not MSI targets.
pub const SPI_MIN: u32 = 32;
/// Highest MSI-usable SPI on this GIC (1020 interrupts, INTIDs 32..1019).
pub const SPI_MAX: u32 = 1019;

/// Is `spi` an MSI-usable SPI?
#[inline]
pub fn spi_in_range(spi: u32) -> bool {
    spi >= SPI_MIN && spi <= SPI_MAX
}

/// **First SPI of the MSI block.** RTC (INTID 34) and the virt UART stay
/// clear of it; the block mirrors the x86 sizing (`MAX_DRIVER_ASSIGN`
/// grants times `VEKTOREN_JE_ZUTEILUNG` vectors = 16) so the grant path
/// reasons identically on both arches.
pub const MSI_SPI_BASIS: u32 = 64;
/// Pool width in SPIs (one block per assignment, four vectors each).
pub const MSI_SPI_POOL: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_space(bytes: &[u8]) -> (impl Fn(u16) -> u8 + '_, impl Fn(u16) -> u16 + '_, impl Fn(u16) -> u32 + '_) {
        let r8 = move |off: u16| *bytes.get(off as usize).unwrap_or(&0xff);
        let r16 = move |off: u16| {
            let o = (off & !1) as usize;
            u16::from_le_bytes([*bytes.get(o).unwrap_or(&0xff), *bytes.get(o + 1).unwrap_or(&0xff)])
        };
        let r32 = move |off: u16| {
            let o = (off & !3) as usize;
            u32::from_le_bytes([
                *bytes.get(o).unwrap_or(&0xff),
                *bytes.get(o + 1).unwrap_or(&0xff),
                *bytes.get(o + 2).unwrap_or(&0xff),
                *bytes.get(o + 3).unwrap_or(&0xff),
            ])
        };
        (r8, r16, r32)
    }

    #[test]
    fn no_cap_list_means_no_msix() {
        // Status without bit 4: the walk must not even start.
        let mut cfg = [0xffu8; 256];
        cfg[CFG_STATUS as usize] = 0x00;
        cfg[CFG_STATUS as usize + 1] = 0x00;
        let (r8, r16, r32) = fake_space(&cfg);
        assert_eq!(msix_find_with(&r8, &r16, &r32), None);
    }

    #[test]
    fn absent_device_answers_none_not_budget() {
        // All-ones space HAS the cap-list bit set; without the guard the walk
        // would chase a `0xfc` pointer into itself for the whole budget.
        let cfg = [0xffu8; 256];
        let (r8, r16, r32) = fake_space(&cfg);
        assert_eq!(msix_find_with(&r8, &r16, &r32), None);
    }

    #[test]
    fn finds_msix_and_decodes_bar_offset_entries() {
        // Cap list at 0x40: MSI (0x05) -> MSI-X (0x11) at 0x50. Control says
        // Table Size 3 (4 rows); table in BAR 2 at offset 0x1000.
        let mut cfg = [0u8; 256];
        cfg[CFG_STATUS as usize] = 0x10;
        cfg[CFG_CAP_PTR as usize] = 0x40;
        cfg[0x40] = 0x05;
        cfg[0x41] = 0x50;
        cfg[0x50] = CAP_ID_MSIX;
        cfg[0x51] = 0x00;
        cfg[0x52] = 0x03;
        cfg[0x53] = 0x00;
        let tbl: u32 = 0x1000 | 2;
        cfg[0x54..0x58].copy_from_slice(&tbl.to_le_bytes());
        let (r8, r16, r32) = fake_space(&cfg);
        assert_eq!(
            msix_find_with(&r8, &r16, &r32),
            Some(MsixInfo { cap: 0x50, bar: 2, offset: 0x1000, entries: 4 })
        );
    }

    #[test]
    fn cyclic_list_terminates_with_none() {
        // 0x40 points at itself: the budget must end the walk, not the device.
        let mut cfg = [0u8; 256];
        cfg[CFG_STATUS as usize] = 0x10;
        cfg[CFG_CAP_PTR as usize] = 0x40;
        cfg[0x40] = 0x09; // some vendor cap, next = self
        cfg[0x41] = 0x40;
        let (r8, r16, r32) = fake_space(&cfg);
        assert_eq!(msix_find_with(&r8, &r16, &r32), None);
    }

    #[test]
    fn table_in_offered_bar_is_detected() {
        let info = MsixInfo { cap: 0x50, bar: 2, offset: 0, entries: 4 };
        assert!(msix_in_bar(&info, 2));
        assert!(!msix_in_bar(&info, 1));
    }

    #[test]
    fn ctrl_transitions_keep_the_other_bit() {
        assert_eq!(msix_enable_ctrl(0), MSIX_CTRL_ENABLE);
        assert_eq!(msix_enable_ctrl(MSIX_CTRL_FUNC_MASK), MSIX_CTRL_ENABLE);
        assert_eq!(msix_quiesce_ctrl(MSIX_CTRL_ENABLE), MSIX_CTRL_ENABLE | MSIX_CTRL_FUNC_MASK);
        assert_eq!(msix_quiesce_ctrl(0), MSIX_CTRL_FUNC_MASK);
    }

    #[test]
    fn doorbell_and_spi_encoding() {
        assert_eq!(msi_doorbell_addr(), 0x0802_0040);
        assert_eq!(msi_data_for_spi(64), 64);
        assert!(spi_in_range(32));
        assert!(spi_in_range(1019));
        assert!(!spi_in_range(31));
        assert!(!spi_in_range(1020));
        // The pool stays inside the usable range and clear of the RTC (34).
        assert!(MSI_SPI_BASIS >= SPI_MIN);
        assert!(MSI_SPI_BASIS + MSI_SPI_POOL as u32 - 1 <= SPI_MAX);
        assert!(MSI_SPI_BASIS > 34);
    }

    #[test]
    fn decode_helpers() {
        assert_eq!(table_bar(0x1002), 2);
        assert_eq!(table_offset(0x1002), 0x1000);
        assert_eq!(entry_count(0x0003), 4);
        assert_eq!(entry_count(0x07ff), 2048);
        assert!(all_ones16(0xffff));
        assert!(!all_ones16(0x0010));
    }
}
