//! Firmware-description handover to the IRT, kernel side: PASS-THROUGH only.
//!
//! Strand 5 (DTB-or-ACPI to the IRT). The kernel validates BOUNDS and moves
//! bytes; it never interprets tables. Foreign firmware data stays untrusted:
//! the DTB blob is copied into kernel-owned frames and mapped read-only into
//! the root task, and the IRT parses the copy with its own foreign-data-safe
//! discipline. Table contents (nodes, CPUs, checksums) are the IRT's report,
//! never the kernel's.
//!
//! Boot path covered here: aarch64 direct boot (`-kernel`). The DTB address
//! arrives in `x0`; ACPI (RSDP) and bootloader driver modules do not exist on
//! this path and are recorded as absent. The UEFI path (strand 4,
//! `caprock-handover`) will fill those fields; the descriptor already carries
//! them.
//!
//! aarch64-only. The x86 build never sees this file.

use caprock_hal::println;
use core::sync::atomic::{AtomicU64, Ordering};

// --- Interface constants ---------------------------------------------------
//
// DIVERGENCE (strand 4 convergence): these values are a LOCAL stub copy of
// the `caprock-handover` contract (uefi-stub worktree, read-only reference).
// The real handover passes addresses dynamically in boot args; until strand 4
// lands, kernel and IRT agree on these FIXED virtual addresses inside the
// root task's address space. Every use site is marked; convergence deletes
// this file's constants in favour of the shared crate.

/// Magic of the handover descriptor page ("CAPHO002", little-endian ASCII).
pub const HO_MAGIC: u64 = 0x3230_304F_4850_4143;
/// Descriptor layout version. v2 carries the driver-module list (supplements
/// 2026-10-09: disk driver from the bootloader + supervision inputs).
pub const HO_VERSION: u32 = 2;
/// `size_of` the descriptor. The IRT refuses anything else.
pub const HO_DESC_LEN: usize = 152;
/// Virtual address of the descriptor page inside the root task.
pub const HO_DESC_VA: u64 = 0x43E0_0000;
/// Virtual address of the DTB copy inside the root task.
pub const HO_DTB_VA: u64 = 0x43C0_0000;
/// Largest DTB the pass-through accepts (1 MiB). The embedded `virt.dtb`
/// claims `totalsize == 0x100000` at 8616 real bytes, and no in-tree parser
/// reads `totalsize` at all -- so the cap bounds the copy, never the parse.
pub const HO_MAX_DTB: u64 = 0x100_000;
/// Largest single bootloader driver module accepted (32 MiB).
pub const HO_MAX_DRV_LEN: u64 = 0x200_0000;
/// Driver-module slots in the descriptor.
pub const HO_MAX_DRV: usize = 4;

/// DTB source codes (`dtb_source`).
pub const SRC_ABSENT: u32 = 0;
pub const SRC_FIRMWARE: u32 = 1; // `x0` on this boot path
pub const SRC_EMBEDDED: u32 = 2; // build-time fallback, reported as such

/// Address window the kernel will dereference for a firmware DTB probe. The
/// aarch64 kernel identity-maps GiB 1..9; anything outside is refused BEFORE
/// any read, so a wild `x0` can never fault the boot.
const HO_PROBE_LO: u64 = 0x4020_0000; // RAM_BASE + 2 MiB (above the kernel L3 reservation)
const HO_PROBE_HI: u64 = 0x2_4000_0000; // 9 GiB ceiling of the static kernel map

extern "C" {
    static __text_start: u8;
}

/// The handover descriptor as the root task sees it at [`HO_DESC_VA`].
/// `repr(C)`, fixed 152 bytes. The IRT copy (`handover_stub.rs`) mirrors it
/// field for field; the two are checked against each other by magic, version
/// and length, never by trust.
#[repr(C)]
pub struct HandoverDesc {
    pub magic: u64,
    pub version: u32,
    pub desc_len: u32,
    pub dtb_va: u64,
    pub dtb_len: u64,
    pub dtb_source: u32,
    pub _pad0: u32,
    pub rsdp: u64,
    pub drv_count: u32,
    pub _pad1: u32,
    pub drv: [DrvEntry; HO_MAX_DRV],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrvEntry {
    /// Physical span of the bootloader-provided module (0 = slot unused).
    pub base: u64,
    pub len: u64,
    /// Device identity for boot-disk selection AND driver restart.
    /// `kind`: 0 = none, 1 = virtio-blk, 2 = virtio-net, 3 = nvme.
    /// `dev`: transport id (virtio device id / PCI RID-like).
    pub kind: u32,
    pub dev: u32,
}

const _: () = assert!(core::mem::size_of::<HandoverDesc>() == HO_DESC_LEN);

// --- Pure bounds validation (host-testable, no kernel state) ----------------

/// FDT header probe: magic + totalsize inside the copy cap. Takes the first
/// 8 bytes only; the caller must have validated the ADDRESS already.
/// `None` = malformed.
///
/// `totalsize` is a sanity signal and a copy-length hint, NOT a parse bound:
/// the parse walks strictly inside the available bytes (same discipline as
/// `caprock-dtb`, which never reads `totalsize`). A blob whose claim
/// disagrees with its length still parses safely -- and the disagreement is
/// visible in the staged length, not hidden.
pub fn dtb_totalsize_if_sane(head: &[u8; 8]) -> Option<u64> {
    let magic = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if magic != 0xd00d_feed {
        return None;
    }
    let total = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as u64;
    if total < 64 || total > HO_MAX_DTB {
        return None;
    }
    Some(total)
}

/// A claimed span `[base, base+len)` is acceptable for pass-through: non-empty
/// and within `max_len`, inside the probe window without wrap, and NOT
/// overlapping the kernel image. Counts nothing; the caller counts.
pub fn span_bounds_ok(base: u64, len: u64, max_len: u64, kstart: u64, kend: u64) -> bool {
    if len == 0 || len > max_len {
        return false;
    }
    if base < HO_PROBE_LO {
        return false;
    }
    let Some(end) = base.checked_add(len) else {
        return false;
    };
    if end > HO_PROBE_HI {
        return false;
    }
    // No overlap with the kernel image: foreign bytes must never alias it.
    if base < kend && kstart < end {
        return false;
    }
    true
}

/// A claimed bootloader driver-module span is acceptable: shape sane,
/// inside the probe window, clear of the kernel image. Pure; the UEFI path
/// (strand 4) feeds real spans here, the direct-boot path stages none --
/// the check is still built and host-tested, so the present/plausible IRT
/// verdict for driver modules rests on reviewed code, not on a dead path.
///
/// In-tree the UEFI path is the only future caller; until it lands this is
/// exercised by the host tests (`/tmp` harness, strand evidence).
#[allow(dead_code)]
pub fn drv_span_bounds_ok(base: u64, len: u64, kstart: u64, kend: u64) -> bool {
    span_bounds_ok(base, len, HO_MAX_DRV_LEN, kstart, kend)
}

// --- Boot state ------------------------------------------------------------

static HO_X0: AtomicU64 = AtomicU64::new(0); // validated firmware DTB base (0 = none)
static HO_X0_LEN: AtomicU64 = AtomicU64::new(0);
static HO_MALFORMED: AtomicU64 = AtomicU64::new(0); // counted firmware malformations
static HO_X0_ABSENT: AtomicU64 = AtomicU64::new(0); // x0 == 0 sightings
static HO_SRC: AtomicU64 = AtomicU64::new(SRC_ABSENT as u64);
static HO_STAGED_LEN: AtomicU64 = AtomicU64::new(0);
static HO_COPY_PHYS: AtomicU64 = AtomicU64::new(0); // owned DTB-copy frames
static HO_COPY_PAGES: AtomicU64 = AtomicU64::new(0);
static HO_DESC_PHYS: AtomicU64 = AtomicU64::new(0); // descriptor page (static below)
static HO_MAPPED: AtomicU64 = AtomicU64::new(0); // 1 = mapped into the root task
static HO_MAP_FAIL: AtomicU64 = AtomicU64::new(0); // mapping failures (fail-open, reported)

/// The descriptor page: static, not allocated. The descriptor must exist even
/// when the allocator is already empty at staging time — otherwise the IRT's
/// header read would fault on an unmapped page exactly when the boot is
/// sickest. (The mapping itself can still fail on page-table OOM; that is
/// counted in HO_MAP_FAIL and reported. Absolute fault-freedom for userspace
/// reads of maybe-unmapped memory does not exist without fault recovery.)
#[repr(align(4096))]
struct DescPage([u8; 4096]);
static mut HO_DESC_PAGE: DescPage = DescPage([0; 4096]);

fn kernel_span() -> (u64, u64) {
    let start = unsafe { &__text_start as *const u8 as u64 };
    (start, caprock_hal::mmu::kernel_end())
}

/// Record the firmware DTB address from `x0`. Bounds only: probe-window
/// check, 8-byte header read, header sanity, full-span check against the
/// kernel image. Every refusal counts into [`HO_MALFORMED`]; nothing is
/// parsed. Must run after the MMU identity map is up.
pub fn note_boot_dtb(dtb_addr: u64) {
    if dtb_addr == 0 {
        HO_X0_ABSENT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(probe_end) = dtb_addr.checked_add(8) else {
        HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if dtb_addr < HO_PROBE_LO || probe_end > HO_PROBE_HI {
        HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: address pre-validated inside the static identity map; 8-byte read only.
    let head: [u8; 8] = unsafe { *(dtb_addr as *const [u8; 8]) };
    let Some(total) = dtb_totalsize_if_sane(&head) else {
        HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (ks, ke) = kernel_span();
    if !span_bounds_ok(dtb_addr, total, HO_MAX_DTB, ks, ke) {
        HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    HO_X0.store(dtb_addr, Ordering::Relaxed);
    HO_X0_LEN.store(total, Ordering::Relaxed);
}

/// Copy the DTB into kernel-owned frames and stage the descriptor page.
///
/// Source: validated firmware span, else the embedded build-time DTB
/// (reported as fallback, never as firmware). The descriptor page is ALWAYS
/// staged -- even when there is no DTB to hand over, it is staged marked
/// absent. Reason: the IRT reads it through a fixed address with no
/// fault-safe probe; an unmapped header page would fault the IRT instead of
/// letting it report absent. Returns false only when even the descriptor
/// page cannot be staged (then there is nothing to map).
pub fn stage_for_root() -> bool {
    let staged: Option<(u64, u64, u32)> = {
        let x0 = HO_X0.load(Ordering::Relaxed);
        if x0 != 0 {
            Some((x0, HO_X0_LEN.load(Ordering::Relaxed), SRC_FIRMWARE))
        } else {
            // Fallback: the DTB embedded in the kernel image. It overlaps the
            // kernel image by construction, so it is COPIED, never mapped.
            // Staged length is what is actually there (never more than the
            // claim, never less than a header).
            let b = crate::DTB_BYTES;
            let mut head = [0u8; 8];
            if b.len() < 8 {
                None
            } else {
                head.copy_from_slice(&b[..8]);
                match dtb_totalsize_if_sane(&head) {
                    Some(total) => {
                        let len = total.min(b.len() as u64);
                        if len < 64 {
                            HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
                            None
                        } else {
                            Some((b.as_ptr() as u64, len, SRC_EMBEDDED))
                        }
                    }
                    _ => {
                        HO_MALFORMED.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                }
            }
        }
    };
    // The DTB copy (optional): owned frames, filled from the source above.
    let (copy_phys, copy_pages, copy_len, code) = match staged {
        Some((src, len, code)) => {
            let pages = (len + 4095) & !4095;
            match crate::system::alloc_anywhere(pages, 4096) {
                Some(region) => {
                    let dst = region.base();
                    // SAFETY: `dst` is freshly allocated identity-mapped RAM,
                    // `pages` bytes; `src` is the validated firmware span or
                    // the embedded slice (len-checked).
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src as *const u8,
                            dst as *mut u8,
                            len as usize,
                        );
                        if pages > len {
                            core::ptr::write_bytes(
                                (dst + len) as *mut u8,
                                0,
                                (pages - len) as usize,
                            );
                        }
                    }
                    (dst, pages / 4096, len, code)
                }
                None => (0, 0, 0, SRC_ABSENT),
            }
        }
        None => (0, 0, 0, SRC_ABSENT),
    };
    // SAFETY: static zeroed page, sole writer before any mapping; descriptor written
    // once here. Physical == virtual by the kernel identity map (same assumption as
    // the embedded-DTB slice pointer above).
    let dphys = unsafe { &raw mut HO_DESC_PAGE.0 as *mut u8 as u64 };
    unsafe {
        let d = &mut *(dphys as *mut HandoverDesc);
        d.magic = HO_MAGIC;
        d.version = HO_VERSION;
        d.desc_len = HO_DESC_LEN as u32;
        d.dtb_va = if copy_phys == 0 { 0 } else { HO_DTB_VA };
        d.dtb_len = copy_len;
        d.dtb_source = code;
        d._pad0 = 0;
        d.rsdp = 0; // no ACPI on the direct-boot path (UEFI stub fills this)
        d.drv_count = 0; // no bootloader modules on the direct-boot path
        d._pad1 = 0;
        d.drv = [DrvEntry { base: 0, len: 0, kind: 0, dev: 0 }; HO_MAX_DRV];
    }
    HO_COPY_PHYS.store(copy_phys, Ordering::Relaxed);
    HO_COPY_PAGES.store(copy_pages, Ordering::Relaxed);
    HO_DESC_PHYS.store(dphys, Ordering::Relaxed);
    HO_SRC.store(code as u64, Ordering::Relaxed);
    HO_STAGED_LEN.store(copy_len, Ordering::Relaxed);
    true
}

/// Map the staged descriptor (+ DTB copy, if any) into the root task
/// (read-only). The descriptor is always mapped when staged, so the IRT can
/// always read the header and report absent instead of faulting. Fail-OPEN
/// with a counted failure: a handover outage must never brick the boot.
pub fn hand_to_root(tid: caprock_sched::ThreadId) -> bool {
    let dphys = HO_DESC_PHYS.load(Ordering::Relaxed);
    if dphys == 0 {
        return false;
    }
    if !crate::system::map_handover_into_thread(tid, HO_DESC_VA, dphys, 4096) {
        HO_MAP_FAIL.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let dst = HO_COPY_PHYS.load(Ordering::Relaxed);
    let pages = HO_COPY_PAGES.load(Ordering::Relaxed);
    let len = HO_STAGED_LEN.load(Ordering::Relaxed);
    if dst == 0 || pages == 0 || len == 0 {
        // Descriptor-only handover (absent DTB): still a handover.
        HO_MAPPED.store(1, Ordering::Relaxed);
        return true;
    }
    let rounded = (len + 4095) & !4095;
    if !crate::system::map_handover_into_thread(tid, HO_DTB_VA, dst, rounded) {
        HO_MAP_FAIL.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    HO_MAPPED.store(1, Ordering::Relaxed);
    true
}

/// Boot-time pass-through facts. Interpretation (nodes, versions, checksums)
/// is the IRT's report, never this line.
pub fn boot_report() {
    let src = HO_SRC.load(Ordering::Relaxed);
    let srctxt = match src as u32 {
        SRC_FIRMWARE => "firmware-x0",
        SRC_EMBEDDED => "embedded-fallback",
        _ => "none",
    };
    println!(
        "handover: dtb src={srctxt} len={} x0absent={} malformed={} staged={} mapped={} mapfail={} rsdp=absent drv=0 (pass-through only: bounds checked, never parsed)",
        HO_STAGED_LEN.load(Ordering::Relaxed),
        HO_X0_ABSENT.load(Ordering::Relaxed),
        HO_MALFORMED.load(Ordering::Relaxed),
        (HO_COPY_PHYS.load(Ordering::Relaxed) != 0) as u8,
        HO_MAPPED.load(Ordering::Relaxed),
        HO_MAP_FAIL.load(Ordering::Relaxed),
    );
}

// --- IRT report decoding (badge word defined in `handover_stub.rs`) ----------
//
// Selftest-gated: the only caller is the suite report path
// (`threads::report` + `all_done`). The pass-through above serves every
// build; the attestation verdict serves the test build.

/// Marker bit of the IRT handover report word.
#[cfg(feature = "selftest")]
pub const HO_BIT: u64 = 1 << 37;
#[cfg(feature = "selftest")]
const DTB_S: u32 = 38;
#[cfg(feature = "selftest")]
const ACPI_S: u32 = 40;

#[cfg(feature = "selftest")]
fn field(badge: u64, shift: u32, width: u32) -> u64 {
    (badge >> shift) & ((1u64 << width) - 1)
}

/// Decode the IRT's handover report badge and print it. Returns the verdict.
/// Report-only by itself; the caller (`threads::report`) wires it into the
/// boot verdict so a missing/false IRT report fails loudly, never silently.
#[cfg(feature = "selftest")]
pub fn irt_report() -> bool {
    let Some(ntfn) = crate::loader::root_notification() else {
        println!("irt-ho  : FAILURES (no root notification: the IRT never had a channel)");
        return false;
    };
    let badge = crate::system::notification_pending(ntfn);
    if badge & HO_BIT == 0 {
        println!(
            "irt-ho  : FAILURES (no IRT handover report in badge {badge:#x}: the IRT never attested its firmware tables)"
        );
        return false;
    }
    let dtb = field(badge, DTB_S, 2);
    let acpi = field(badge, ACPI_S, 2);
    let neg_len = badge & (1 << 42) != 0;
    let neg_magic = badge & (1 << 43) != 0;
    let neg_cksum = badge & (1 << 44) != 0;
    let cpus = field(badge, 45, 4);
    let nodes = field(badge, 49, 7);
    let drvn = field(badge, 56, 3);
    let drv_ok = badge & (1 << 59) != 0;
    let bootdisk = field(badge, 60, 3);
    let sup_complete = badge & (1 << 63) != 0;
    let dtbtxt = match dtb {
        1 => "ok",
        2 => "rejected",
        _ => "absent",
    };
    let acpitxt = match acpi {
        1 => "ok",
        2 => "invalid",
        _ => "absent",
    };
    let bootdtxt = match bootdisk {
        1 => "selected-dtb",
        2 => "selected-acpi",
        3 => "ambiguous-refused",
        _ => "undecided",
    };
    println!("irt-ho  : IRT firmware-table report badge {badge:#018x}");
    println!(
        "irt-ho  : dtb={dtbtxt} nodes={nodes} cpus={cpus} staged-len={} staged-src={} rsdp={acpitxt} drv={drvn} drv-plausible={drv_ok} bootdisk={bootdtxt} (rule: DTB/ACPI + boot-module info select the boot disk, NEVER enumeration order)",
        HO_STAGED_LEN.load(Ordering::Relaxed),
        match HO_SRC.load(Ordering::Relaxed) as u32 {
            SRC_FIRMWARE => "firmware-x0",
            SRC_EMBEDDED => "embedded-fallback",
            _ => "none",
        },
    );
    println!(
        "irt-ho  : negprobes corrupted-len-rejected={neg_len} bad-magic-rejected={neg_magic} acpi-bad-checksum-rejected={neg_cksum} sup-inputs-complete={sup_complete}"
    );
    println!(
        "irt-ho  : supervision: have=LOAD,SPAWN,PDCTL-lifecycle,KILL,EXIT; missing=service-directory(lookup/register),dependency-order (named gaps, not built)"
    );
    let ok = dtb == 1 && neg_len && neg_magic && neg_cksum && sup_complete && (drvn == 0 || drv_ok);
    println!(
        "irt-ho  : {} (IRT attested its tables: a report, not silent use)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// Gating predicate for `all_done`: the IRT's attestation arrived and holds.
#[cfg(feature = "selftest")]
pub fn irt_ho_done() -> bool {
    let Some(ntfn) = crate::loader::root_notification() else {
        return false;
    };
    let badge = crate::system::notification_pending(ntfn);
    if badge & HO_BIT == 0 {
        return false;
    }
    field(badge, DTB_S, 2) == 1
        && badge & (1 << 42) != 0
        && badge & (1 << 43) != 0
        && badge & (1 << 44) != 0
        && badge & (1 << 63) != 0
        && (field(badge, 56, 3) == 0 || badge & (1 << 59) != 0)
}
