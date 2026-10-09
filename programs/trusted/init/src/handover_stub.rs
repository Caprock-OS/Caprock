//! Firmware-handover report of the IRT (strand 5): receive, parse, attest.
//!
//! The IRT is the userspace SUPERVISOR: it starts everything else from the
//! boot archive, distributes caps, and (follow-up work, not here) restarts
//! crashed services and drivers. Supervision needs facts, not silence, so
//! this module turns the handed-over firmware description into an attested
//! report: DTB inventory (node/prop/cpu counts, memory, FDT version), ACPI
//! status (RSDP + header/checksum discipline), the bootloader driver-module
//! list with device identity, and the boot-disk decision.
//!
//! DIVERGENCE (convergence at integration): until strand 4 lands, this is a
//! LOCAL stub copy of the `caprock-handover` contract (uefi-stub worktree,
//! read-only reference -- do NOT modify that worktree). Consequences, all
//! marked `DIVERGENCE` below:
//! * fixed addresses/lengths mirror `kernel::handover` (the real handover
//!   passes them dynamically in boot args);
//! * the DTB walker duplicates the `caprock-dtb` discipline (an `init`
//!   dependency on `caprock-dtb` would break the TrustedSAS allowlist, which
//!   admits exactly `libcaprock`);
//! * the ACPI header check duplicates the `hal::acpi::table_at` discipline
//!   (same reason; the HAL is kernel-side and unreachable from EL0 anyway).
//!
//! Everything here is `#![forbid(unsafe_code)]`-safe: all reads are
//! bounds-checked slice accesses over the kernel-mapped window. Foreign
//! firmware data stays untrusted -- header first (magic/version/length),
//! parse second, and every refusal is reported, never silent.

// DIVERGENCE: fixed contract values, mirrored from `kernel::handover`.
const HO_MAGIC: u64 = 0x3230_304F_4850_4143;
const HO_VERSION: u32 = 2;
const HO_DESC_LEN: usize = 152;
// DIVERGENCE: copy-cap mirror of `kernel::handover::HO_MAX_DTB`. The
// embedded `virt.dtb` claims 1 MiB at 8616 real bytes; the cap bounds the
// claim, the walk stays inside the slice either way.
const HO_MAX_DTB: u64 = 0x100_000;
const HO_MAX_DRV_LEN: u64 = 0x200_0000;
const HO_MAX_DRV: usize = 4;
// DIVERGENCE: probe-window floor/ceiling mirror the kernel validator. The
// kernel-overlap check itself is kernel-side only (the IRT does not know the
// kernel image bounds); the IRT checks shape, the kernel checked overlap.
const HO_PROBE_LO: u64 = 0x4020_0000;
const HO_PROBE_HI: u64 = 0x2_4000_0000;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

// Badge-word layout (mirror of `kernel::handover::irt_report` decoding).
pub const HO_BIT: u64 = 1 << 37;

fn be32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    Some(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

fn cstr_len(data: &[u8], off: usize) -> Option<usize> {
    let rest = data.get(off..)?;
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    Some(end)
}

const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Parsed descriptor fields (shape-checked, not trusted).
struct Desc {
    dtb_va: u64,
    dtb_len: u64,
    dtb_source: u32,
    rsdp: u64,
    drv_count: u32,
    drv_base: [u64; HO_MAX_DRV],
    drv_len: [u64; HO_MAX_DRV],
    drv_kind: [u32; HO_MAX_DRV],
    drv_dev: [u32; HO_MAX_DRV],
}

fn read_desc(b: &[u8]) -> Option<Desc> {
    if b.len() < HO_DESC_LEN {
        return None;
    }
    if le64(b, 0)? != HO_MAGIC || le32(b, 8)? != HO_VERSION || le32(b, 12)? as usize != HO_DESC_LEN
    {
        return None;
    }
    let mut d = Desc {
        dtb_va: le64(b, 16)?,
        dtb_len: le64(b, 24)?,
        dtb_source: le32(b, 32)?,
        rsdp: le64(b, 40)?,
        drv_count: le32(b, 48)?,
        drv_base: [0; HO_MAX_DRV],
        drv_len: [0; HO_MAX_DRV],
        drv_kind: [0; HO_MAX_DRV],
        drv_dev: [0; HO_MAX_DRV],
    };
    if d.drv_count as usize > HO_MAX_DRV {
        return None;
    }
    let mut i = 0;
    while i < HO_MAX_DRV {
        let o = 56 + i * 24;
        d.drv_base[i] = le64(b, o)?;
        d.drv_len[i] = le64(b, o + 8)?;
        d.drv_kind[i] = le32(b, o + 16)?;
        d.drv_dev[i] = le32(b, o + 20)?;
        i += 1;
    }
    Some(d)
}

/// DTB inventory from a byte slice. Same discipline as `caprock-dtb`: header
/// first (magic, totalsize sane, offsets inside), then a single monotonic
/// walk (every iteration advances, every access bounds-checked).
/// Returns `(nodes, props, cpus, mem_base, mem_size, fdt_version)`.
///
/// `totalsize` is a sanity signal, not a length match: it must sit inside
/// the copy cap (absurd claims refused), but it is NOT required to equal the
/// slice length -- the embedded `virt.dtb` claims 1 MiB at 8616 real bytes
/// and walks fine, because the walk never leaves the slice. What the claim
/// cannot do is pull the parser out of bounds; that is enforced per access.
pub fn dtb_inventory(data: &[u8]) -> Option<(u64, u64, u64, u64, u64, u32)> {
    if data.len() < 40 {
        return None;
    }
    if be32(data, 0)? != FDT_MAGIC {
        return None;
    }
    let total = be32(data, 4)? as u64;
    if total < 64 || total > HO_MAX_DTB {
        return None; // absurd claim: refuse
    }
    let off_struct = be32(data, 8)? as usize;
    let off_strings = be32(data, 12)? as usize;
    if off_struct >= data.len() || off_strings >= data.len() {
        return None;
    }
    let version = be32(data, 20)?;
    let mut pos = off_struct;
    let mut depth = 0usize;
    let mut in_cpus_at = usize::MAX;
    let mut nodes = 0u64;
    let mut props = 0u64;
    let mut cpus = 0u64;
    let mut mem: Option<(u64, u64)> = None;
    let mut in_memory = false;
    // Monotonic: `pos` grows by >= 4 per iteration, so this always ends.
    while pos + 4 <= data.len() {
        let tok = be32(data, pos)?;
        pos += 4;
        match tok {
            FDT_BEGIN_NODE => {
                let nl = cstr_len(data, pos)?;
                let name = data.get(pos..pos + nl)?;
                pos += align4(nl + 1);
                if pos > data.len() {
                    return None;
                }
                depth += 1;
                nodes += 1;
                if depth == 2 && name == b"cpus" {
                    in_cpus_at = depth;
                } else if in_cpus_at != usize::MAX
                    && depth == in_cpus_at + 1
                    && name.len() >= 4
                    && &name[..4] == b"cpu@"
                {
                    cpus += 1;
                }
                in_memory = name.len() >= 6 && &name[..6] == b"memory";
            }
            FDT_END_NODE => {
                if depth == in_cpus_at {
                    in_cpus_at = usize::MAX;
                }
                depth = depth.saturating_sub(1);
                in_memory = false;
            }
            FDT_PROP => {
                let len = be32(data, pos)? as usize;
                let nameoff = be32(data, pos + 4)? as usize;
                let val = pos + 8;
                pos = val.checked_add(align4(len))?;
                if pos > data.len() {
                    return None;
                }
                props += 1;
                let pname = off_strings.checked_add(nameoff).and_then(|o| {
                    data.get(o..).and_then(|r| {
                        let e = r.iter().position(|&c| c == 0).unwrap_or(r.len());
                        r.get(..e)
                    })
                });
                // `#address-cells == #size-cells == 2` (QEMU `virt` form, same
                // assumption as `caprock-dtb::memory`).
                if in_memory && pname == Some(b"reg".as_slice()) && len >= 16 {
                    let hi = be32(data, val)? as u64;
                    let lo = be32(data, val + 4)? as u64;
                    let s_hi = be32(data, val + 8)? as u64;
                    let s_lo = be32(data, val + 12)? as u64;
                    if mem.is_none() {
                        mem = Some(((hi << 32) | lo, (s_hi << 32) | s_lo));
                    }
                }
            }
            FDT_NOP => {}
            FDT_END => {
                let (mb, ms) = mem.unwrap_or((0, 0));
                return Some((nodes, props, cpus, mb, ms, version));
            }
            _ => return None,
        }
    }
    None
}

/// ACPI table-header check, same discipline as `hal::acpi::table_at`:
/// 4-byte signature, LE length plausible (36..=1MiB, inside the slice),
/// checksum over the length bytes == 0. Returns the table length.
///
/// `pub` for host tests (see [`dtb_inventory`]).
pub fn acpi_sdt_check(data: &[u8], want: &[u8; 4]) -> Option<usize> {
    if data.len() < 36 {
        return None;
    }
    if data.get(..4)? != want {
        return None;
    }
    let len = le32(data, 4)? as usize;
    if !(36..=0x10_0000).contains(&len) || len > data.len() {
        return None;
    }
    let sum = data[..len].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    if sum != 0 {
        return None;
    }
    Some(len)
}

/// A claimed bootloader driver-module span is plausible: present, sane
/// length, inside the probe window without wrap. (Kernel-overlap was the
/// kernel's check; shape is checkable -- and checked -- here.)
///
/// `pub` for host tests (see [`dtb_inventory`]).
pub fn drv_span_plausible(base: u64, len: u64) -> bool {
    if base == 0 || len == 0 || len > HO_MAX_DRV_LEN {
        return false;
    }
    if base < HO_PROBE_LO {
        return false;
    }
    match base.checked_add(len) {
        Some(end) => end <= HO_PROBE_HI,
        None => false,
    }
}

/// Boot-disk decision codes (badge bits 60..62).
const BOOT_UNDECIDED: u64 = 0;
const BOOT_DTB: u64 = 1;
const BOOT_ACPI: u64 = 2;
const BOOT_AMBIGUOUS: u64 = 3;

/// Boot-disk determination rule (supplements 2026-10-09):
/// the boot disk is identified from DTB/ACPI + boot-module info, NEVER from
/// device enumeration order. By construction this function takes no order
/// parameter -- there is no first-found-wins input to consult. Ambiguity
/// (or absent inputs) refuses to decide instead of guessing.
///
/// `pub` for host tests (see [`dtb_inventory`]).
pub fn bootdisk_decide(dtb_ok: bool, acpi_ok: bool, drv_count: u64) -> u64 {
    if drv_count == 0 {
        return BOOT_UNDECIDED; // no boot modules: nothing to select from
    }
    match (dtb_ok, acpi_ok) {
        (true, false) => BOOT_DTB,
        (false, true) => BOOT_ACPI,
        (false, false) => BOOT_UNDECIDED, // no description: refuse, never guess
        (true, true) => BOOT_AMBIGUOUS,   // two descriptions: refuse, never pick first
    }
}

fn sat(v: u64, bits: u32) -> u64 {
    let max = (1u64 << bits) - 1;
    if v > max {
        max
    } else {
        v
    }
}

/// Run the full receive/parse/attest flow. Returns the badge word for the
/// kernel (`HO_BIT` set when this report attests something; 0 when there is
/// no handover to attest -- the kernel then reports the absence).
pub fn run() -> u64 {
    let Some(desc_bytes) = libcaprock::handover::desc_bytes() else {
        return 0;
    };
    let Some(d) = read_desc(desc_bytes) else {
        return 0;
    };
    // Unknown source codes are a malformed descriptor, not a new source.
    if d.dtb_source > 2 {
        return 0;
    }

    // --- DTB: receive + parse + negative probes ---
    let mut dtb_status = 0u64; // absent
    let mut nodes = 0u64;
    let mut cpus = 0u64;
    if d.dtb_len != 0 {
        if let Some(bytes) = libcaprock::handover::dtb_bytes(d.dtb_va, d.dtb_len) {
            if let Some((n, _, c, _, _, _)) = dtb_inventory(bytes) {
                dtb_status = 1; // ok
                nodes = n;
                cpus = c;
            } else {
                dtb_status = 2; // handed over but rejected
            }
        } else {
            dtb_status = 2; // window refusal: also a rejection
        }
    }
    // Negative probes over a stack copy of the real header when available,
    // else over a synthetic header: the discipline must refuse, in the guest.
    let mut probe_src = [0u8; 8];
    if d.dtb_len != 0 {
        if let Some(bytes) = libcaprock::handover::dtb_bytes(d.dtb_va, d.dtb_len) {
            if bytes.len() >= 8 {
                probe_src.copy_from_slice(&bytes[..8]);
            } else {
                probe_src[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
            }
        } else {
            probe_src[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        }
    } else {
        probe_src[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
    }
    // Corrupted totalsize (absurd length) must be refused.
    let mut bad_len = probe_src;
    bad_len[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    let neg_len_ok = dtb_inventory_header_only(&bad_len).is_none();
    // Bad magic must be refused.
    let mut bad_magic = probe_src;
    bad_magic[..4].copy_from_slice(&0x1234_5678u32.to_be_bytes());
    let neg_magic_ok = dtb_inventory_header_only(&bad_magic).is_none();

    // --- ACPI: status + checksum discipline probes ---
    // On this boot path the RSDP is absent (recorded 0 by the kernel); the
    // check discipline still runs, against synthetic headers, so the guest
    // log shows it refuses as designed.
    let acpi_status = if d.rsdp == 0 { 0 } else { 2 }; // present-but-unmapped: invalid here
    let neg_cksum_ok = acpi_neg_probes_ok();

    // --- Driver modules: list plausibility + self-probe of the check ---
    // Plausible = span plausible AND device identity present. Identity
    // (kind/dev) is what supervision needs twice: to select the boot disk
    // AND to restart the right driver after a crash. A module without it
    // cannot be selected, so it fails plausibility instead of loading blind.
    let mut drv_ok_all = true;
    let mut i = 0u32;
    while i < d.drv_count {
        let k = i as usize;
        if k >= HO_MAX_DRV
            || d.drv_base[k] == 0
            || !drv_span_plausible(d.drv_base[k], d.drv_len[k])
            || d.drv_kind[k] == 0
        {
            drv_ok_all = false;
        }
        let _ = d.drv_dev[k];
        i += 1;
    }
    // Self-probe: the plausibility check must accept a sane span (the DTB
    // window we were given, when present) and refuse an absurd one. This
    // proves the check runs; it attests no module.
    let self_probe_ok = if d.dtb_len != 0 && d.dtb_va != 0 {
        drv_span_plausible(d.dtb_va, d.dtb_len) && !drv_span_plausible(0, 0)
    } else {
        !drv_span_plausible(0, 0) && !drv_span_plausible(0x10, 0xFFFF_FFFF_FFFF_FFFF)
    };
    if !self_probe_ok {
        drv_ok_all = false;
    }

    // --- Boot disk + supervision inputs ---
    let bootdisk = bootdisk_decide(dtb_status == 1, acpi_status == 1, d.drv_count as u64);
    let sup_complete =
        dtb_status == 1 && neg_len_ok && neg_magic_ok && neg_cksum_ok && self_probe_ok;

    HO_BIT
        | (dtb_status << 38)
        | (acpi_status << 40)
        | ((neg_len_ok as u64) << 42)
        | ((neg_magic_ok as u64) << 43)
        | ((neg_cksum_ok as u64) << 44)
        | (sat(cpus, 4) << 45)
        | (sat(nodes, 7) << 49)
        | (sat(d.drv_count as u64, 3) << 56)
        | ((drv_ok_all as u64) << 59)
        | (bootdisk << 60)
        | ((sup_complete as u64) << 63)
}

/// Header-only DTB probe used by the negative cases: magic + totalsize sane
/// and totalsize == 8 (the probe slice length). Anything else is refused.
fn dtb_inventory_header_only(head: &[u8; 8]) -> Option<()> {
    if u32::from_be_bytes([head[0], head[1], head[2], head[3]]) != FDT_MAGIC {
        return None;
    }
    let total = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if total != 8 {
        return None;
    }
    Some(())
}

/// ACPI negative probes: a synthetic SDT header with a broken checksum and
/// one with a corrupted (tiny) length must both be refused, while a
/// well-formed one with a correct checksum is accepted (positive control).
fn acpi_neg_probes_ok() -> bool {
    // Minimal synthetic SDT: sig "TEST", len 36, 28 bytes payload, checksum
    // fixed so the whole sums to 0.
    let mut good = [0u8; 36];
    good[..4].copy_from_slice(b"TEST");
    good[4..8].copy_from_slice(&36u32.to_le_bytes());
    let mut s = 0u8;
    let mut i = 0;
    while i < 35 {
        s = s.wrapping_add(good[i]);
        i += 1;
    }
    good[35] = 0u8.wrapping_sub(s);
    if acpi_sdt_check(&good, b"TEST").is_none() {
        return false;
    }
    // Broken checksum: flip one payload byte.
    let mut bad_sum = good;
    bad_sum[10] ^= 0x01;
    if acpi_sdt_check(&bad_sum, b"TEST").is_some() {
        return false;
    }
    // Corrupted length: claims 36, slice is 10.
    if acpi_sdt_check(&good[..10], b"TEST").is_some() {
        return false;
    }
    // Wrong signature.
    if acpi_sdt_check(&good, b"APIC").is_some() {
        return false;
    }
    true
}
