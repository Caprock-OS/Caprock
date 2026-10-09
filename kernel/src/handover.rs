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
//!
//! Boot memory source (Mitteilung 24, "Speicherschutz beim Boot"): the
//! allocator must never hold framebuffer, ACPI-reclaim, MMIO, or otherwise
//! reserved ranges. On UEFI boot the handover `regions[]` (USABLE only) minus
//! framebuffer/module/kernel spans are THE source; on direct boot the DTB
//! memory range minus kernel/archive/reserved-memory spans is. Both go
//! through [`subtract_regions`] -- pure range arithmetic, host-tested.
//!
//! Enforced property: MEM holds exactly the kept regions, and everything the
//! IRT or a driver maps or owns (loader copies, DMA grants, handover frames)
//! is allocated from MEM. Reserved ranges are therefore unreachable to EL0
//! by construction -- there is no second source to audit.

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

// --- Boot memory source: pure range subtraction -------------------------------
//
// Mitteilung 24: the allocator source is usable-minus-reserved, never the raw
// firmware window. UEFI call shape (strand 4, `uefi_boot.rs`, not in this
// tree yet) over the SAME core:
//   usable = handover regions[] filtered to USABLE (base, len),
//   excl   = [framebuffer if plausible] + [kernel] + [archive] + modules[],
//   kept   = subtract_regions(usable, excl, &mut kept) -> init_mem_regions.
// Direct boot (wired below): usable = DTB memory range, excl = low/kernel +
// archive window + DTB reserved-memory regs. No AML, no policy beyond bounds:
// every span is arithmetic, every refusal is counted.

/// Kept-region capacity of the boot source computation (stack scratch).
pub const MEMSRC_MAX: usize = 16;
/// Reserved-memory entries read per DTB (direct boot).
pub const HO_MAX_RSVD: usize = 8;

/// What [`subtract_regions`] kept, cut, and refused. Every field is printed
/// on the `memsrc` line; a red field fails loudly there, never silently.
pub struct MemsrcCounts {
    /// Kept regions written to `out` (<= `out.len()`).
    pub kept: usize,
    /// Their total bytes.
    pub kept_bytes: u64,
    /// Exclusion applications that actually removed bytes.
    pub cut: u32,
    /// Empty exclusions skipped (unused slots, not a finding).
    pub excl_empty: u32,
    /// Wrapping exclusions: counted AND applied saturating to u64::MAX
    /// (fail-closed direction for an insane input; construction sites
    /// pre-validate, so this is belt, not path).
    pub excl_bad: u32,
    /// Corrupt usable inputs skipped (zero length or wrapping end).
    pub in_bad: u32,
    /// Pieces beyond `out` capacity (a too-small caller buffer, not a
    /// firmware finding -- size `out` as MEMSRC_MAX and this stays 0).
    pub dropped: u32,
}

/// Usable-minus-reserved as pure arithmetic. `usable` and `excl` are
/// (base, len) spans; `out` receives the kept pieces in input order.
/// Overlaps (also exclusion-vs-exclusion) resolve naturally; adjacency
/// without overlap cuts nothing. Deterministic; no allocation, no parsing.
pub fn subtract_regions(
    usable: &[(u64, u64)],
    excl: &[(u64, u64)],
    out: &mut [(u64, u64)],
) -> MemsrcCounts {
    let mut c = MemsrcCounts {
        kept: 0,
        kept_bytes: 0,
        cut: 0,
        excl_empty: 0,
        excl_bad: 0,
        in_bad: 0,
        dropped: 0,
    };
    // Classify exclusions once (counts are per exclusion, not per piece).
    let mut bad_excl = 0u32;
    let mut empty_excl = 0u32;
    for &(b, l) in excl {
        if l == 0 {
            empty_excl += 1;
        } else if b.checked_add(l).is_none() {
            bad_excl += 1;
        }
    }
    c.excl_empty = empty_excl;
    c.excl_bad = bad_excl;
    // Scratch for one usable span's pieces on the stack (allocation-free).
    // One input span plus at most one extra piece per applied exclusion;
    // MEMSRC_MAX+1 slots cover every firmware-realistic input, and anything
    // beyond is counted as dropped, never lost silently.
    for &(ub, ul) in usable {
        if ul == 0 {
            c.in_bad += 1;
            continue;
        }
        let Some(uend) = ub.checked_add(ul) else {
            c.in_bad += 1;
            continue;
        };
        // Piece list as (base, end) pairs on the stack.
        let mut pieces = [(0u64, 0u64); MEMSRC_MAX + 1];
        let mut npieces = 1usize;
        pieces[0] = (ub, uend);
        for &(eb, el) in excl {
            if el == 0 {
                continue;
            }
            // Wrapping exclusion: saturate to the top (fail-closed direction,
            // counted above). Construction sites pre-validate; this arm is
            // pinned by host tests, not by firmware.
            let eend = eb.checked_add(el).unwrap_or(u64::MAX);
            let mut w = 0usize;
            while w < npieces {
                let (pb, pe) = pieces[w];
                if eb < pe && pb < eend {
                    // Overlap: cut out [max(pb,eb), min(pe,eend)).
                    c.cut += 1;
                    let cb = pb.max(eb);
                    let ce = pe.min(eend);
                    if pb < cb && ce < pe {
                        // Split: keep [pb,cb), insert [ce,pe) behind.
                        pieces[w] = (pb, cb);
                        if npieces < pieces.len() {
                            // Shift right to make room for the split tail.
                            let mut s = npieces;
                            while s > w + 1 {
                                pieces[s] = pieces[s - 1];
                                s -= 1;
                            }
                            pieces[w + 1] = (ce, pe);
                            npieces += 1;
                        } else {
                            // Scratch full: keep the head, count the tail as
                            // dropped (reported; sized so real inputs never
                            // hit this).
                            pieces[w] = (pb, cb);
                            c.dropped += 1;
                        }
                    } else if pb < cb {
                        pieces[w] = (pb, cb);
                    } else if ce < pe {
                        pieces[w] = (ce, pe);
                    } else {
                        // Fully covered: remove by shifting left.
                        let mut s = w;
                        while s + 1 < npieces {
                            pieces[s] = pieces[s + 1];
                            s += 1;
                        }
                        npieces -= 1;
                        continue; // re-examine the shifted piece
                    }
                }
                w += 1;
            }
        }
        for i in 0..npieces {
            let (pb, pe) = pieces[i];
            if pe <= pb {
                continue;
            }
            if c.kept < out.len() {
                out[c.kept] = (pb, pe - pb);
                c.kept += 1;
                c.kept_bytes += pe - pb;
            } else {
                c.dropped += 1;
            }
        }
    }
    c
}

// --- DTB reserved-memory (direct boot) ----------------------------------------
//
// Minimal walker over a DTB slice: decodes the `reg` of `/reserved-memory`
// children with that node's `#address-cells`/`#size-cells` (FDT defaults
// (2,1) at the root, inherited). DIVERGENCE: duplicates the `caprock-dtb`
// token discipline (strand 3 owns `caprock-dtb`; convergence is a
// `reserved_regs()` API there -- see the patch-text in the strand report).
// Refusals, all counted, never silent: insane cells, `ranges` translation
// (beyond bounds -- refused, not misplaced), per-reg overflow, over-capacity.
// `status` is deliberately NOT honoured: a disabled reservation stays
// excluded (fail-closed direction for protection; no AML either way).

fn be32at(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn cstrlen(data: &[u8], off: usize) -> Option<usize> {
    let rest = data.get(off..)?;
    Some(rest.iter().position(|&c| c == 0).unwrap_or(rest.len()))
}

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Read `/reserved-memory` regs. Returns `(found, corrupt)`; `out` holds up
/// to `HO_MAX_RSVD` spans (valid extras beyond capacity count as corrupt:
/// silently dropping a reservation is worse than reporting it).
pub fn dtb_reserved_regs(
    dtb: &[u8],
    out: &mut [(u64, u64); HO_MAX_RSVD],
) -> (u32, u32) {
    let mut found = 0u32;
    let mut corrupt = 0u32;
    if dtb.len() < 40 || be32at(dtb, 0) != Some(0xd00d_feed) {
        return (0, 0); // no header, no reservations -- absent, not corrupt
    }
    let off_struct = match be32at(dtb, 8) {
        Some(v) => v as usize,
        None => return (0, 0),
    };
    let off_strings = match be32at(dtb, 12) {
        Some(v) => v as usize,
        None => return (0, 0),
    };
    if off_struct >= dtb.len() || off_strings >= dtb.len() {
        return (0, 0);
    }
    let mut pos = off_struct;
    let mut depth = 0usize;
    // Cell sizes: FDT defaults (2,1), root overrides, /reserved-memory
    // inherits root and may override again. Children decode with the node's.
    let mut root_addr = 2u32;
    let mut root_size = 1u32;
    let mut in_rsvd_at = usize::MAX; // depth of /reserved-memory
    let mut rsvd_addr = 2u32;
    let mut rsvd_size = 1u32;
    let mut rsvd_ranges_bad = false;
    // Monotonic walk: pos advances >= 4 per step, always terminates.
    while pos + 4 <= dtb.len() {
        let tok = match be32at(dtb, pos) {
            Some(t) => t,
            None => return (found, corrupt),
        };
        pos += 4;
        match tok {
            FDT_BEGIN_NODE => {
                let nl = match cstrlen(dtb, pos) {
                    Some(n) => n,
                    None => return (found, corrupt),
                };
                let name = dtb.get(pos..pos + nl).unwrap_or(&[]);
                pos += (nl + 1 + 3) & !3;
                if pos > dtb.len() {
                    return (found, corrupt);
                }
                depth += 1;
                if depth == 2 && name == b"reserved-memory" {
                    in_rsvd_at = depth;
                    // Inherit root cells; the node's own props may override.
                    rsvd_addr = root_addr;
                    rsvd_size = root_size;
                    rsvd_ranges_bad = false;
                }
            }
            FDT_END_NODE => {
                if depth == in_rsvd_at {
                    in_rsvd_at = usize::MAX;
                }
                depth = depth.saturating_sub(1);
            }
            FDT_PROP => {
                let len = match be32at(dtb, pos) {
                    Some(v) => v as usize,
                    None => return (found, corrupt),
                };
                let nameoff = match be32at(dtb, pos + 4) {
                    Some(v) => v as usize,
                    None => return (found, corrupt),
                };
                let val = pos + 8;
                pos = match val.checked_add((len + 3) & !3) {
                    Some(p) => p,
                    None => return (found, corrupt),
                };
                if pos > dtb.len() {
                    return (found, corrupt);
                }
                let pname = off_strings
                    .checked_add(nameoff)
                    .and_then(|o| dtb.get(o..))
                    .and_then(|r| {
                        let e = r.iter().position(|&c| c == 0).unwrap_or(r.len());
                        r.get(..e)
                    })
                    .unwrap_or(&[]);
                if pname == b"#address-cells" && len >= 4 {
                    let v = be32at(dtb, val).unwrap_or(2);
                    if depth == 1 {
                        root_addr = v;
                    } else if in_rsvd_at != usize::MAX && depth == in_rsvd_at {
                        rsvd_addr = v;
                    }
                } else if pname == b"#size-cells" && len >= 4 {
                    let v = be32at(dtb, val).unwrap_or(1);
                    if depth == 1 {
                        root_size = v;
                    } else if in_rsvd_at != usize::MAX && depth == in_rsvd_at {
                        rsvd_size = v;
                    }
                } else if pname == b"ranges"
                    && in_rsvd_at != usize::MAX
                    && depth == in_rsvd_at
                    && len > 0
                {
                    // Translation tables are beyond bounds: refuse the node
                    // rather than misplace its regs. Counted: an unusable
                    // reservation must be loud, not absent-looking.
                    rsvd_ranges_bad = true;
                    corrupt += 1;
                } else if pname == b"reg"
                    && in_rsvd_at != usize::MAX
                    && depth == in_rsvd_at + 1
                    && !rsvd_ranges_bad
                {
                    let ac = rsvd_addr as usize;
                    let sc = rsvd_size as usize;
                    if !(1..=2).contains(&ac) || !(1..=2).contains(&sc) {
                        corrupt += 1;
                    } else if len < 4 * (ac + sc) {
                        corrupt += 1;
                    } else {
                        let mut base = 0u64;
                        let mut ok = true;
                        let mut i = 0;
                        while i < ac {
                            let w = match be32at(dtb, val + 4 * i) {
                                Some(w) => w as u64,
                                None => {
                                    ok = false;
                                    break;
                                }
                            };
                            base = (base << 32) | w;
                            i += 1;
                        }
                        let mut size = 0u64;
                        i = 0;
                        while i < sc {
                            let w = match be32at(dtb, val + 4 * ac + 4 * i) {
                                Some(w) => w as u64,
                                None => {
                                    ok = false;
                                    break;
                                }
                            };
                            size = (size << 32) | w;
                            i += 1;
                        }
                        match (ok, size, base.checked_add(size)) {
                            (true, 1.., Some(_)) => {
                                if (found as usize) < out.len() {
                                    out[found as usize] = (base, size);
                                    found += 1;
                                } else {
                                    corrupt += 1; // over capacity, reported
                                }
                            }
                            _ => corrupt += 1,
                        }
                    }
                }
            }
            FDT_NOP => {}
            FDT_END => return (found, corrupt),
            _ => return (found, corrupt),
        }
    }
    (found, corrupt)
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

// --- Direct-boot DTB slice + memsrc verdict -----------------------------------

/// The DTB to read reserved-memory from at allocator setup: the validated
/// firmware bytes when `x0` carried them, else the embedded build-time copy.
/// Same source the staging path copies later; header re-checked by the walker.
pub fn direct_dtb() -> &'static [u8] {
    let x0 = HO_X0.load(Ordering::Relaxed);
    if x0 != 0 {
        let len = HO_X0_LEN.load(Ordering::Relaxed) as usize;
        // SAFETY: probe-window + header + full span validated in
        // `note_boot_dtb`; every consumer is bounds-checked regardless.
        unsafe { core::slice::from_raw_parts(x0 as *const u8, len) }
    } else {
        crate::DTB_BYTES
    }
}

static HO_MEMSRC_OK: AtomicU64 = AtomicU64::new(0);

/// Record the boot memory-source verdict for the suite gate.
pub fn memsrc_record(ok: bool) {
    HO_MEMSRC_OK.store(ok as u64, Ordering::Relaxed);
}

/// Gating predicate for `all_done`: the allocator holds exactly usable-minus-
/// reserved (kept non-empty, nothing refused, nothing dropped).
#[cfg(feature = "selftest")]
pub fn memsrc_ok() -> bool {
    HO_MEMSRC_OK.load(Ordering::Relaxed) != 0
}
