//! Pure guard-page address arithmetic for the aarch64 identity map.
//!
//! This module owns the address math behind [`crate::mmu`] guard pages and
//! nothing else: no tables, no registers, no `super::` imports. That is what
//! makes it host-testable — it compiles on the host with `rustc --test`
//! (the `dmar.rs`/`irte.rs` pattern from `tools/host-tests.sh`), while
//! `caprock-hal` as a whole never builds there.
//!
//! The constants below are the single source for both this module and
//! [`crate::mmu`]: the MMU module imports them instead of redefining them, so
//! the two cannot drift apart silently.
//!
//! Layout reminder (identity map, 4 KiB granule, 39-bit VA): GiB 1
//! (`[RAM_BASE, RAM_BASE + 1 GiB)`) is covered by one L2 table whose entries
//! `[1..]` are 2 MiB block descriptors (`Perm::UserRw`: EL0+EL1 RW, `nG`).
//! Guarding a 4 KiB page means splitting its 2 MiB block into an L3 table
//! whose entries mirror the block page by page, then invalidating the one
//! guard entry. [`split_page`] is that mirroring, as a pure function over
//! descriptors, so its rights-preservation is checkable with literals.

/// 4 KiB granule: the guard granularity.
pub const PAGE: u64 = 4096;
/// 2 MiB: one L2 block descriptor.
pub const TWO_MIB: u64 = 2 * 1024 * 1024;
/// 1 GiB: one L1 slot.
pub const ONE_GIB: u64 = 1 << 30;
/// Start of RAM (and of the identity map) on QEMU `virt`.
pub const RAM_BASE: u64 = 0x4000_0000;
/// Output-address bits of a table/page descriptor (bits 47:12).
pub const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

/// First address usable as free RAM: the first 2 MiB hold the shared
/// kernel L3 (kernel image + `.user_text`) and contain no EL0-accessible
/// free RAM. Guards below this address are rejected, never guessed.
pub const USER_RAM_MIN: u64 = RAM_BASE + TWO_MIB;
/// End of GiB 1. Guard splits only cover this GiB (see below).
pub const GIB1_END: u64 = RAM_BASE + ONE_GIB;

// Descriptor kinds (ARMv8-A stage 1). Must match `mmu.rs`; the test
// `split_preserves_rights_changes_granularity` below pins the combination
// independently via hand-computed literals.
const BLOCK_DESC: u64 = 0b01;
const PAGE_DESC: u64 = 0b11;
#[allow(dead_code)] // used by `mmu.rs`; dead only in standalone host-test builds
pub(crate) const BLOCK_KIND: u64 = BLOCK_DESC;
// Permission/attribute bits carried from a block into its split pages
// (test-only: the production path reads rights from the live entry).
#[cfg(test)]
const AF: u64 = 1 << 10;
#[cfg(test)]
const SH_INNER: u64 = 0b11 << 8;
#[cfg(test)]
const AP_RW_EL0: u64 = 0b01 << 6;
#[cfg(test)]
const ATTR_NORMAL: u64 = 1 << 2;
#[cfg(test)]
const NG: u64 = 1 << 11;
#[cfg(test)]
const PXN: u64 = 1 << 53;
#[cfg(test)]
const UXN: u64 = 1 << 54;

/// 2 MiB block number of `pa` within GiB 1, or `None` when `pa` is not
/// 4 KiB-aligned inside `[USER_RAM_MIN, GIB1_END)`.
///
/// Block 0 (the shared kernel L3, `[RAM_BASE, RAM_BASE + 2 MiB)`) is
/// excluded by the lower bound: splitting it would re-map the kernel image
/// itself, so a guard there is a caller bug and refused, not approximated.
pub fn block_of(pa: u64) -> Option<u64> {
    if pa % PAGE != 0 || pa < USER_RAM_MIN || pa >= GIB1_END {
        return None;
    }
    Some((pa - RAM_BASE) / TWO_MIB)
}

/// Index of `pa` within its 2 MiB block (0..512). Only meaningful when
/// [`block_of`] accepts `pa`; kept separate so the range check lives in
/// exactly one place.
pub fn page_index(pa: u64) -> usize {
    ((pa % TWO_MIB) / PAGE) as usize
}

/// Mirror a 2 MiB block descriptor into the 4 KiB page descriptor for
/// `page_phys` inside that block.
///
/// The rights come from the existing entry, not from a constant: a split
/// must not change the view, only its granularity (the x86 split learned
/// this by breaking Ring-3 stacks when it hardcoded rights). The output
/// address comes from the page index. `page_phys` must be the identity
/// address of a page inside the block; misalignment corrupts the entry, so
/// it is refused with a zero (invalid) descriptor rather than guessed.
pub fn split_page(block_desc: u64, page_phys: u64) -> u64 {
    if page_phys % PAGE != 0 {
        return 0;
    }
    (block_desc & !ADDR_MASK & !0b11) | (page_phys & ADDR_MASK) | PAGE_DESC
}

/// True when `desc` maps nothing (a standing guard, or never-split state
/// outside the guard pool).
pub fn is_invalid(desc: u64) -> bool {
    desc & 0b11 == 0b00
}

/// Full UserRw block descriptor as `mmu.rs` builds it for GiB-1 free RAM,
/// for test vectors only: `addr | AF | SH_INNER | ATTR_NORMAL | AP_RW_EL0 |
/// PXN | UXN | NG | BLOCK_DESC`.
#[cfg(test)]
pub fn user_block_desc(addr: u64) -> u64 {
    addr | AF | SH_INNER | ATTR_NORMAL | AP_RW_EL0 | PXN | UXN | NG | BLOCK_DESC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_numbers_follow_identity() {
        // First guardable page: start of block 1.
        assert_eq!(block_of(USER_RAM_MIN), Some(1));
        // Last page of block 1.
        assert_eq!(block_of(USER_RAM_MIN + TWO_MIB - PAGE), Some(1));
        // First page of block 2.
        assert_eq!(block_of(USER_RAM_MIN + TWO_MIB), Some(2));
        // Last guardable page overall.
        assert_eq!(block_of(GIB1_END - PAGE), Some(511));
    }

    #[test]
    fn kernel_l3_block_is_refused() {
        // Block 0 holds the shared kernel L3: no guard there, ever.
        assert_eq!(block_of(RAM_BASE), None);
        assert_eq!(block_of(RAM_BASE + PAGE), None);
        assert_eq!(block_of(USER_RAM_MIN - PAGE), None);
    }

    #[test]
    fn out_of_range_is_refused() {
        assert_eq!(block_of(GIB1_END), None);
        assert_eq!(block_of(GIB1_END + TWO_MIB), None);
        assert_eq!(block_of(0x8000_0000 + TWO_MIB), None); // GiB 2 (L1 block, no L2 split)
        assert_eq!(block_of(0x1_0000_0000), None); // above the 4 GiB identity map
        assert_eq!(block_of(0), None);
    }

    #[test]
    fn misalignment_is_refused() {
        assert_eq!(block_of(USER_RAM_MIN + 1), None);
        assert_eq!(block_of(USER_RAM_MIN + TWO_MIB + 512), None);
    }

    #[test]
    fn page_index_covers_block() {
        assert_eq!(page_index(USER_RAM_MIN), 0);
        assert_eq!(page_index(USER_RAM_MIN + PAGE), 1);
        assert_eq!(page_index(USER_RAM_MIN + TWO_MIB - PAGE), 511);
        assert_eq!(page_index(USER_RAM_MIN + TWO_MIB), 0); // next block restarts
    }

    #[test]
    fn split_preserves_rights_changes_granularity() {
        // Hand-computed from the ARM ARM bit positions (independent of mmu.rs):
        // block 0x4020_0000 UserRw = addr | AF | SH_INNER | ATTR_NORMAL |
        // AP_RW_EL0 | PXN | UXN | NG | BLOCK_DESC
        //   = 0x4020_0000 | 0x400 | 0x300 | 0x4 | 0x40 | 0x800 | BLOCK
        //     | 0x0020_0000_0000_0000 | 0x0040_0000_0000_0000
        //   = 0x0060_0000_4020_0F45.
        let block = user_block_desc(0x4020_0000);
        assert_eq!(block, 0x0060_0000_4020_0F45);
        // Page 0x4020_1000 of the same block: same rights, PAGE_DESC.
        let page = split_page(block, 0x4020_1000);
        assert_eq!(page, 0x0060_0000_4020_1F47);
        // The output address is the page, not the block.
        assert_eq!(page & ADDR_MASK, 0x4020_1000);
        // EL0 RW, never executable, ASID-tagged: unchanged.
        assert_eq!(page & (PXN | UXN | NG), PXN | UXN | NG);
        assert_eq!(page & (0b11 << 6), AP_RW_EL0);
    }

    #[test]
    fn split_misaligned_page_gives_invalid() {
        let block = user_block_desc(0x4020_0000);
        assert!(is_invalid(split_page(block, 0x4020_1001)));
    }

    #[test]
    fn invalid_is_only_kind_zero() {
        assert!(is_invalid(0));
        assert!(!is_invalid(user_block_desc(0x4020_0000))); // 0b01
        assert!(!is_invalid(split_page(user_block_desc(0x4020_0000), 0x4020_0000))); // 0b11
        assert!(!is_invalid(0b11)); // table descriptor is not "invalid"
    }

    #[test]
    fn every_page_of_block_roundtrips() {
        // Splitting must be total: each of the 512 pages mirrors with its
        // own address and identical rights.
        let base = RAM_BASE + 3 * TWO_MIB;
        let block = user_block_desc(base);
        for i in 0..512u64 {
            let pa = base + i * PAGE;
            assert_eq!(block_of(pa), Some(3));
            assert_eq!(page_index(pa) as u64, i);
            let p = split_page(block, pa);
            assert_eq!(p & ADDR_MASK, pa);
            assert_eq!(p & !ADDR_MASK, (block & !ADDR_MASK & !0b11) | PAGE_DESC);
        }
    }
}
