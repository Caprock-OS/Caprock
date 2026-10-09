#![no_std]
#![forbid(unsafe_code)]
//! Static ACPI table parser: RSDP/RSDT/XSDT/MADT/GTDT/SPCR/FADT headers and pointers.
//!
//! This crate answers exactly one question per table — *where is the device and which
//! interrupt does it use* — over plain `&[u8]` slices. Every read is bounds-checked;
//! malformed input yields `None`, never a panic. There is deliberately **no AML
//! interpreter** (`_CRS`, `_PRT`, power resources and everything under them are out of
//! scope); what AML would describe, the static tables below already state for the
//! devices Caprock needs.
//!
//! Role split with the x86 HAL (`caprock_hal::x86_64::acpi`): that module keeps its proven
//! path of reading the firmware tables through identity-mapped physical memory, including
//! the RSDP scan. This crate never touches memory — it parses slices the caller hands it,
//! so every path is host-testable with real-table-shaped blobs. Adoption of this crate on
//! x86 is a named follow-up; this crate must not change x86 behaviour, and it cannot (the
//! x86 HAL does not depend on it).
//!
//! ARM uses this crate immediately: [`arm_platform`] reduces a MADT, a GTDT and an
//! optional SPCR to the GIC bases and the timer interrupt the platform layer needs.
//!
//! What is NOT handled (documented, not hidden):
//! * Only the first GICD / first GICR discovery range / first ITS is reported. Multi-GIC
//!   machines need an explicit selection policy; silently merging them would be worse.
//! * GTDT platform timers (SBSA watchdogs) are counted, not described.
//! * FADT is header-validated only; no flags, no reset register, no power management.
//! * No RSDP scan (that needs physical memory access) and no SDT entry following (that
//!   needs the caller's address space): [`xsdt_entry`] hands out addresses, the caller
//!   resolves them.
//!
//! Per-table matrix — *parsed* means checksum-verified decode, *used* means a consumer
//! reads it, the last column is the one-line reason for everything else:
//!
//! | table | parsed | used for | parsed but unused, why |
//! |---|---|---|
//! | RSDP/XSDT | header, revision, addresses | handover table lookup | address following (caller's address space, not a parser's) |
//! | MADT | GICD/GICR/ITS/GICC entries | GIC bases, CPU count | MSI frames (no ITS driver), second and later GICs (no selection policy) |
//! | GTDT | timer GSIVs | NS EL1 tick INTID | platform timers (counted only; no SBSA-watchdog driver), CNTCTL frame (firmware-owned) |
//! | SPCR | console GAS, baud, GSI | UART base + kind | baud code (the firmware-configured rate is kept), GSI (no ACPI IRQ routing on ARM here) |
//! | DBG2 | serial devices | UART fallback when no SPCR | non-serial ports (no driver here), second and later ports (first wins, same as DTB stdout) |
//! | MCFG | segment entries | ECAM window + bus range (x86 and ARM) | per-segment detail beyond the first (single-host assumption, as on the DTB path) |
//! | SRAT/SLIT | affinities, distances | validated + reported; classification stays in `hal::numa` (one place, both arches) | — |
//! | IORT | nodes, SMMUv3 base/model/GSIVs | exposed as data for the DMA/IOMMU side | enforcement and mapping (lives in SMMU+MMU code, not in a parser) |
//! | PPTT | processor nodes | validated + reported; topology stays in `hal::cpu`/`smt` | cache nodes (no consumer reads them) |
//! | TPM2 | class, control area, start method | reported only | measured boot (a trust strand's business; no platform-layer consumer) |
//! | FADT | header | presence check | flags, reset register, power management (AML territory — see below) |
//!
//! NO AML interpreter, restated as a boundary: anything that needs `_CRS`, `_PRT` or a
//! method stays out. Power management would be the first asker, and only a power strand
//! may move this line — not a parser extension.

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some((rd_u32(b, off + 4)? as u64) << 32 | rd_u32(b, off)? as u64)
}

/// ACPI checksum: all bytes of the table must add to zero (mod 256).
pub fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

// --- SDT header ---------------------------------------------------------------------------------

/// A validated System Description Table header plus its full bytes.
#[derive(Clone, Copy)]
pub struct Header<'a> {
    bytes: &'a [u8],
}

impl<'a> Header<'a> {
    /// Validate `bytes` as one SDT: at least 36 bytes, plausible length (36..=1 MiB) that
    /// fits the slice, and a correct checksum over exactly the table length.
    pub fn parse(bytes: &'a [u8]) -> Option<Header<'a>> {
        let len = rd_u32(bytes, 4)? as usize;
        if !(36..=0x10_0000).contains(&len) {
            return None;
        }
        let body = bytes.get(..len)?;
        if !checksum_ok(body) {
            return None;
        }
        Some(Header { bytes: body })
    }

    /// Validate and additionally require the 4-byte `signature`.
    pub fn parse_sig(bytes: &'a [u8], signature: &[u8; 4]) -> Option<Header<'a>> {
        let h = Header::parse(bytes)?;
        if h.signature() == signature {
            Some(h)
        } else {
            None
        }
    }

    pub fn signature(&self) -> &[u8; 4] {
        // Validated to be at least 36 bytes by `parse`.
        self.bytes[..4].try_into().ok().unwrap_or(b"????")
    }

    pub fn length(&self) -> usize {
        rd_u32(self.bytes, 4).unwrap_or(0) as usize
    }

    pub fn revision(&self) -> u8 {
        self.bytes.get(8).copied().unwrap_or(0)
    }

    /// The full validated table bytes (header + body).
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Body after the 36-byte header.
    pub fn body(&self) -> &'a [u8] {
        self.bytes.get(36..).unwrap_or(&[])
    }
}

// --- RSDP ---------------------------------------------------------------------------------------

/// A validated Root System Description Pointer.
#[derive(Clone, Copy)]
pub struct Rsdp<'a> {
    bytes: &'a [u8],
}

impl<'a> Rsdp<'a> {
    /// Validate: `RSD PTR ` signature, v1 checksum over 20 bytes; for revision >= 2 a full
    /// 36 bytes with a second checksum over all of them.
    pub fn parse(bytes: &'a [u8]) -> Option<Rsdp<'a>> {
        if bytes.get(..8)? != b"RSD PTR " {
            return None;
        }
        if !checksum_ok(bytes.get(..20)?) {
            return None;
        }
        let revision = *bytes.get(15)?;
        if revision >= 2 {
            let len = rd_u32(bytes, 20)? as usize;
            if len != 36 {
                return None;
            }
            if !checksum_ok(bytes.get(..36)?) {
                return None;
            }
            Some(Rsdp { bytes: bytes.get(..36)? })
        } else {
            Some(Rsdp { bytes: bytes.get(..20)? })
        }
    }

    pub fn revision(&self) -> u8 {
        self.bytes.get(15).copied().unwrap_or(0)
    }

    /// RSDT address (always present, 32-bit).
    pub fn rsdt_address(&self) -> Option<u32> {
        rd_u32(self.bytes, 16)
    }

    /// XSDT address (ACPI 2.0+ only).
    pub fn xsdt_address(&self) -> Option<u64> {
        if self.revision() >= 2 {
            rd_u64(self.bytes, 24)
        } else {
            None
        }
    }
}

// --- RSDT / XSDT entry lists --------------------------------------------------------------------

/// Number of entries in an RSDT (`entry_size = 4`) or XSDT (`entry_size = 8`). The slice
/// must hold exactly one validated table; pass `Header::bytes`.
pub fn entry_count(table: &[u8], entry_size: usize) -> Option<usize> {
    let len = rd_u32(table, 4)? as usize;
    if len < 36 || len > table.len() || (entry_size != 4 && entry_size != 8) {
        return None;
    }
    Some((len - 36) / entry_size)
}

/// The `i`-th entry (a physical address) of an RSDT (`entry_size = 4`) or XSDT (8).
pub fn entry(table: &[u8], entry_size: usize, i: usize) -> Option<u64> {
    let n = entry_count(table, entry_size)?;
    if i >= n {
        return None;
    }
    let off = 36 + i * entry_size;
    if entry_size == 8 {
        rd_u64(table, off)
    } else {
        rd_u32(table, off).map(|a| a as u64)
    }
}

/// XSDT entry count (64-bit entries).
pub fn xsdt_count(xsdt: &[u8]) -> Option<usize> {
    entry_count(xsdt, 8)
}

/// The `i`-th XSDT entry (physical address of a table).
pub fn xsdt_entry(xsdt: &[u8], i: usize) -> Option<u64> {
    entry(xsdt, 8, i)
}

/// RSDT entry count (32-bit entries).
pub fn rsdt_count(rsdt: &[u8]) -> Option<usize> {
    entry_count(rsdt, 4)
}

/// The `i`-th RSDT entry (physical address of a table).
pub fn rsdt_entry(rsdt: &[u8], i: usize) -> Option<u32> {
    entry(rsdt, 4, i).map(|a| a as u32)
}

// --- MADT ---------------------------------------------------------------------------------------

/// Entry types of interest in the APIC table (ACPI §5.2.12).
pub const MADT_GICC: u8 = 11;
pub const MADT_GICD: u8 = 12;
pub const MADT_GICR: u8 = 13;
pub const MADT_GIC_ITS: u8 = 14;
pub const MADT_GIC_MSI_FRAME: u8 = 15;

/// A validated MADT (`APIC` signature): header + Local APIC address + flags + entries.
#[derive(Clone, Copy)]
pub struct Madt<'a> {
    header: Header<'a>,
}

impl<'a> Madt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Madt<'a>> {
        let header = Header::parse_sig(bytes, b"APIC")?;
        if header.length() < 44 {
            return None; // header + LocalApicAddress + Flags
        }
        Some(Madt { header })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.header.bytes()
    }

    /// Raw `(type, entry bytes)` pairs in table order; stops at the first malformed entry.
    pub fn entries(&self) -> MadtEntries<'a> {
        MadtEntries { bytes: self.header.bytes(), off: 44 }
    }

    /// The first entry of `entry_type`, if any.
    pub fn first_entry(&self, entry_type: u8) -> Option<&'a [u8]> {
        self.entries().find(|(t, _)| *t == entry_type).map(|(_, b)| b)
    }
}

/// Iterator over MADT entries.
pub struct MadtEntries<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Iterator for MadtEntries<'a> {
    type Item = (u8, &'a [u8]);
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let b = self.bytes.get(self.off..)?;
        let entry_type = *b.first()?;
        let len = *b.get(1)? as usize;
        if len < 2 || self.off + len > self.bytes.len() {
            return None;
        }
        let entry = self.bytes.get(self.off..self.off + len)?;
        self.off += len;
        Some((entry_type, entry))
    }
}

/// A GIC Distributor entry (type 12, length 24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gicd {
    /// Physical base of the distributor.
    pub base: u64,
    /// GIC architecture version (1..=4); 0 means the table does not say.
    pub version: u8,
}

pub fn gicd(madt: &Madt) -> Option<Gicd> {
    let e = madt.first_entry(MADT_GICD)?;
    if e.len() < 24 {
        return None;
    }
    Some(Gicd { base: rd_u64(e, 8)?, version: e.get(20).copied().unwrap_or(0) })
}

/// A GIC Redistributor discovery range (type 13, length 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gicr {
    pub base: u64,
    pub length: u32,
}

/// The first GICR discovery range, if the MADT has one. Reported faithfully whatever the
/// entry length is (within framing): observed in the wild as 24 bytes on a GICv2 AAVMF
/// machine, where no redistributors exist — consumers gate on the GIC version instead of
/// second-guessing the firmware here.
pub fn first_gicr(madt: &Madt) -> Option<Gicr> {
    let e = madt.first_entry(MADT_GICR)?;
    if e.len() < 16 {
        return None;
    }
    Some(Gicr { base: rd_u64(e, 4)?, length: rd_u32(e, 12)? })
}

/// Physical base of the first ITS (type 14), if the MADT has one.
pub fn first_its(madt: &Madt) -> Option<u64> {
    let e = madt.first_entry(MADT_GIC_ITS)?;
    if e.len() < 20 {
        return None;
    }
    rd_u64(e, 8)
}

/// Physical base of the CPU interface of the first GICC entry (type 11), if present and
/// non-zero. GICv3 leaves it zero (system-register interface); the caller decides what
/// "no CPU interface" means for its version.
pub fn first_gicc_base(madt: &Madt) -> Option<u64> {
    let e = madt.first_entry(MADT_GICC)?;
    if e.len() < 80 {
        return None;
    }
    let base = rd_u64(e, 32)?;
    if base == 0 {
        return None;
    }
    Some(base)
}

/// Number of GICC entries (type 11): the CPU count the x86 path reads from the MADT.
pub fn gicc_count(madt: &Madt) -> usize {
    madt.entries().filter(|(t, _)| *t == MADT_GICC).count()
}

// --- GTDT ---------------------------------------------------------------------------------------

/// The architected timer interrupts every ARM kernel needs (ACPI §5.2.24, GTDT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GtdtInfo {
    /// Physical address of the counter control frame (0 if the table says none).
    pub cntctl_base: u64,
    /// Secure EL1 physical timer GSIV.
    pub secure_el1_gsiv: u32,
    /// Non-secure EL1 physical timer GSIV — the tick source (DT `arm,armv8-timer`
    /// non-secure interrupt and this field name the same timer).
    pub ns_el1_gsiv: u32,
    /// EL1 virtual timer GSIV.
    pub virt_gsiv: u32,
    /// EL2 physical timer GSIV.
    pub el2_gsiv: u32,
}

/// Parse a GTDT: `GTDT` signature, at least the four timer GSIVs (length >= 80).
/// Platform timers (offset 88) are *counted* by [`platform_timer_count`], not described.
pub fn gtdt(bytes: &[u8]) -> Option<GtdtInfo> {
    let h = Header::parse_sig(bytes, b"GTDT")?;
    if h.length() < 80 {
        return None;
    }
    let b = h.bytes();
    Some(GtdtInfo {
        cntctl_base: rd_u64(b, 36)?,
        secure_el1_gsiv: rd_u32(b, 48)?,
        ns_el1_gsiv: rd_u32(b, 56)?,
        virt_gsiv: rd_u32(b, 64)?,
        el2_gsiv: rd_u32(b, 72)?,
    })
}

/// Number of GTDT platform timers (SBSA watchdogs etc.), if the table is long enough to
/// state it (length >= 96). `None` also covers "table too short", which is not an error.
pub fn platform_timer_count(bytes: &[u8]) -> Option<u32> {
    let h = Header::parse_sig(bytes, b"GTDT")?;
    if h.length() < 96 {
        return None;
    }
    rd_u32(h.bytes(), 92)
}

// --- SPCR ---------------------------------------------------------------------------------------

/// SPCR interface types (a selection; the full list is in ACPI §5.2.6, Table 5.15).
pub const SPCR_IF_16550: u8 = 0x00;
pub const SPCR_IF_16450: u8 = 0x01;
pub const SPCR_IF_PL011: u8 = 0x03;
pub const SPCR_IF_SBSA: u8 = 0x0D;
pub const SPCR_IF_SBSA2: u8 = 0x0E;

/// Which UART programming model an SPCR describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleKind {
    Ns16550,
    Pl011,
    Sbsa,
    Unknown(u8),
}

impl ConsoleKind {
    fn classify(interface: u8) -> ConsoleKind {
        match interface {
            SPCR_IF_16550 | SPCR_IF_16450 => ConsoleKind::Ns16550,
            SPCR_IF_PL011 => ConsoleKind::Pl011,
            SPCR_IF_SBSA | SPCR_IF_SBSA2 => ConsoleKind::Sbsa,
            other => ConsoleKind::Unknown(other),
        }
    }
}

/// The console description of an SPCR (ACPI §5.2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpcrInfo {
    pub kind: ConsoleKind,
    /// Raw interface type byte.
    pub interface: u8,
    /// Base address from the Generic Address Structure (meaningful for memory space).
    pub base: u64,
    /// GAS address space (0 = memory, 1 = I/O, ...).
    pub addr_space: u8,
    /// SPCR baud-rate code (3 = 9600, 4 = 19200, 6 = 57600, 7 = 115200, 0 = as configured).
    pub baud_code: u8,
    /// Global System Interrupt (valid when `interrupt_type` has bit 0 set).
    pub gsi: u32,
}

/// Parse an SPCR: `SPCR` signature, full 80-byte table.
pub fn spcr(bytes: &[u8]) -> Option<SpcrInfo> {
    let h = Header::parse_sig(bytes, b"SPCR")?;
    if h.length() < 80 {
        return None;
    }
    let b = h.bytes();
    let interface = *b.get(36)?;
    Some(SpcrInfo {
        kind: ConsoleKind::classify(interface),
        interface,
        addr_space: *b.get(40)?,
        base: rd_u64(b, 44)?,
        baud_code: *b.get(58)?,
        gsi: rd_u32(b, 54)?,
    })
}

// --- FADT ---------------------------------------------------------------------------------------

/// Validate a FADT (`FACP` signature). Header only: no flags, no reset register, no power
/// management — ARM does not need them and an AML-free crate must not pretend to.
pub fn fadt(bytes: &[u8]) -> Option<Header<'_>> {
    let h = Header::parse_sig(bytes, b"FACP")?;
    if h.revision() == 0 {
        return None;
    }
    Some(h)
}

// --- ARM platform reduction ---------------------------------------------------------------------

/// Everything the ARM platform layer takes from ACPI: GIC bases plus the timer interrupt.
/// `madt` and `gtdt` are required; `spcr` is optional (no SPCR means "no ACPI console").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArmPlatform {
    pub dist_base: u64,
    pub dist_version: u8,
    /// CPU interface base from the first GICC entry (GICv2); `None` on GICv3, whose CPU
    /// interface is system registers, not MMIO.
    pub gicc_base: Option<u64>,
    pub redist_base: Option<u64>,
    pub redist_length: Option<u32>,
    pub its_base: Option<u64>,
    /// Non-secure EL1 physical timer GSIV (= GIC INTID).
    pub timer_gsiv: u32,
    pub uart: Option<SpcrInfo>,
}

pub fn arm_platform(madt_bytes: &[u8], gtdt_bytes: &[u8], spcr_bytes: Option<&[u8]>) -> Option<ArmPlatform> {
    let madt = Madt::parse(madt_bytes)?;
    let g = gicd(&madt)?;
    let timer = gtdt(gtdt_bytes)?;
    let r = first_gicr(&madt);
    let uart = match spcr_bytes {
        Some(b) => Some(spcr(b)?),
        None => None,
    };
    Some(ArmPlatform {
        dist_base: g.base,
        dist_version: g.version,
        gicc_base: first_gicc_base(&madt),
        redist_base: r.map(|x| x.base),
        redist_length: r.map(|x| x.length),
        its_base: first_its(&madt),
        timer_gsiv: timer.ns_el1_gsiv,
        uart,
    })
}

// --- bootloader handover ----------------------------------------------------------------------
//
// A bootloader-loaded driver (GRUB module / UEFI image) cannot query live firmware for
// its controller routing — the tables must be present AT BOOT alongside it. The handover
// form is a plain list of `(signature, table bytes)`: on UEFI the loader copies the
// tables before `ExitBootServices` (the RSDP address comes from the configuration
// table), on Multiboot2 the RSDP copy arrives as an ACPI tag. No addresses are followed
// here and no memory is touched, so this stays as host-testable as everything else in
// this crate.
//

/// First handover table with signature `want` (`b"APIC"`, `b"GTDT"`, ...), if any.
pub fn handover_find<'a>(tables: &[(&'a [u8; 4], &'a [u8])], want: &[u8; 4]) -> Option<&'a [u8]> {
    tables.iter().find(|(sig, _)| *sig == want).map(|(_, b)| *b)
}

/// [`arm_platform`] over a bootloader handover list. MADT + GTDT are required, SPCR is
/// optional (a handover without one simply has no ACPI console — not an error).
pub fn arm_platform_from_handover(tables: &[(&[u8; 4], &[u8])]) -> Option<ArmPlatform> {
    let madt = handover_find(tables, b"APIC")?;
    let gtdt = handover_find(tables, b"GTDT")?;
    arm_platform(madt, gtdt, handover_find(tables, b"SPCR"))
}

// --- SRAT / SLIT (NUMA) --------------------------------------------------------------------------
//
// Affinities and distances, validated and reported. The *classification* (what an
// unaffiliated range means, what happens when storage runs out) stays in one place for
// both architectures — `caprock_hal::numa`, the same reason `caprock-dtb` hands out
// callbacks instead of structures. What this crate contributes is the validated decode:
// every entry below was covered by the table checksum, and malformed entries stop the
// walk instead of being skipped over.

/// SRAT entry types with typed decoders here (the rest are yielded raw by the iterator).
pub const SRAT_LAPIC: u8 = 0;
pub const SRAT_MEMORY: u8 = 1;
pub const SRAT_X2APIC: u8 = 2;
pub const SRAT_GICC: u8 = 3;

/// A validated SRAT (`SRAT` signature): header + 4 reserved bytes + affinity structures.
#[derive(Clone, Copy)]
pub struct Srat<'a> {
    bytes: &'a [u8],
}

impl<'a> Srat<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Srat<'a>> {
        let h = Header::parse_sig(bytes, b"SRAT")?;
        if h.length() < 40 {
            return None;
        }
        Some(Srat { bytes: h.bytes() })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Raw `(type, entry bytes)` pairs in table order; stops at the first malformed entry.
    /// Types 4+ (ITS affinity, generic initiators/ports) have no typed decoder — they are
    /// still checksum-covered and visible here, just not interpreted (no consumer).
    pub fn entries(&self) -> SratEntries<'a> {
        SratEntries { bytes: self.bytes, off: 40 }
    }
}

/// Iterator over SRAT entries.
pub struct SratEntries<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Iterator for SratEntries<'a> {
    type Item = (u8, &'a [u8]);
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let b = self.bytes.get(self.off..)?;
        let entry_type = *b.first()?;
        let len = *b.get(1)? as usize;
        if len < 2 || self.off + len > self.bytes.len() {
            return None;
        }
        let entry = self.bytes.get(self.off..self.off + len)?;
        self.off += len;
        Some((entry_type, entry))
    }
}

fn srat_enabled_flags(flags: u32) -> bool {
    flags & 1 != 0
}

/// Memory affinities: `f(base, length, proximity_domain, enabled)` per type-1 entry.
pub fn srat_memory(srat: &Srat, mut f: impl FnMut(u64, u64, u32, bool)) {
    for (t, e) in srat.entries() {
        if t != SRAT_MEMORY || e.len() < 40 {
            continue;
        }
        if let (Some(node), Some(base), Some(len), Some(flags)) =
            (rd_u32(e, 2), rd_u64(e, 8), rd_u64(e, 16), rd_u32(e, 28))
        {
            f(base, len, node, srat_enabled_flags(flags));
        }
    }
}

/// GICC affinities (the ARM CPU affinity): `f(acpi_uid, proximity_domain, enabled)`.
pub fn srat_gicc(srat: &Srat, mut f: impl FnMut(u32, u32, bool)) {
    for (t, e) in srat.entries() {
        if t != SRAT_GICC || e.len() < 18 {
            continue;
        }
        if let (Some(node), Some(uid), Some(flags)) = (rd_u32(e, 2), rd_u32(e, 6), rd_u32(e, 10)) {
            f(uid, node, srat_enabled_flags(flags));
        }
    }
}

/// x2APIC affinities (the x86 CPU affinity): `f(x2apic_id, proximity_domain, enabled)`.
pub fn srat_x2apic(srat: &Srat, mut f: impl FnMut(u32, u32, bool)) {
    for (t, e) in srat.entries() {
        if t != SRAT_X2APIC || e.len() < 24 {
            continue;
        }
        if let (Some(node), Some(id), Some(flags)) = (rd_u32(e, 4), rd_u32(e, 8), rd_u32(e, 12)) {
            f(id, node, srat_enabled_flags(flags));
        }
    }
}

/// Legacy local-APIC affinities: `f(apic_id, proximity_domain, enabled)`.
pub fn srat_lapic(srat: &Srat, mut f: impl FnMut(u8, u32, bool)) {
    for (t, e) in srat.entries() {
        if t != SRAT_LAPIC || e.len() < 16 {
            continue;
        }
        let lo = e.get(2).copied().unwrap_or(0) as u32;
        let hi = e.get(9..12).map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0])).unwrap_or(0);
        if let Some(flags) = rd_u32(e, 4) {
            f(e.get(3).copied().unwrap_or(0), lo | (hi << 8), srat_enabled_flags(flags));
        }
    }
}

/// Number of SRAT entries of `entry_type` (a cheap shape probe for tests and reports).
pub fn srat_count(srat: &Srat, entry_type: u8) -> usize {
    srat.entries().filter(|(t, _)| *t == entry_type).count()
}

/// Node count of a validated SLIT, if the distance matrix fits the table. Zero nodes and
/// more than 512 are rejected (a degenerate table and an absurd one, respectively).
pub fn slit_count(table: &[u8]) -> Option<u64> {
    let h = Header::parse_sig(table, b"SLIT")?;
    if h.length() < 44 {
        return None;
    }
    let n = rd_u64(h.bytes(), 36)?;
    if n == 0 || n > 512 {
        return None;
    }
    let need = (n as usize).checked_mul(n as usize)?.checked_add(44)?;
    if h.length() < need {
        return None;
    }
    Some(n)
}

/// Distance from node `i` to node `j` (10 = local by convention, larger = farther).
pub fn slit_distance(table: &[u8], i: u64, j: u64) -> Option<u8> {
    let n = slit_count(table)?;
    if i >= n || j >= n {
        return None;
    }
    let h = Header::parse(table)?;
    h.bytes().get(44 + (i as usize) * (n as usize) + (j as usize)).copied()
}

// --- DBG2 (serial debug ports) ------------------------------------------------------------------
//
// The SPCR names the console; the DBG2 names the *debug* ports, which on ARM servers is
// usually the same UART. Rule: SPCR first, first DBG2 serial device as the fallback, and
// non-serial DBG2 devices (USB, 1394, network) are reported by the iterator but never
// chosen — there is no driver for them here.

/// DBG2 port type "serial" (the only one this crate selects).
pub const DBG2_PORT_SERIAL: u16 = 0x8000;

/// One serial DBG2 device: programming model (as an SPCR-style code), address space, base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dbg2Serial {
    pub kind: ConsoleKind,
    /// Raw port subtype (serial codes match the SPCR interface codes).
    pub subtype: u16,
    /// GAS address space (0 = memory).
    pub addr_space: u8,
    pub base: u64,
}

/// Call `f` for every serial DBG2 device (first GAS register of each), in table order.
pub fn dbg2_serial_devices(table: &[u8], mut f: impl FnMut(Dbg2Serial)) {
    let h = match Header::parse_sig(table, b"DBG2") {
        Some(h) => h,
        None => return,
    };
    if h.length() < 44 {
        return;
    }
    let b = h.bytes();
    let (mut off, mut left) = match (rd_u32(b, 36), rd_u32(b, 40)) {
        (Some(o), Some(n)) => (o as usize, n as usize),
        _ => return,
    };
    // Sanity cap: a table of 16+ debug ports is absurd; without a cap a corrupt count
    // walks the whole table (still safe, but pointless).
    if left > 16 {
        return;
    }
    while left > 0 {
        left -= 1;
        let dlen = match rd_u16_at(b, off + 1) {
            Some(l) if l as usize >= 22 => l as usize,
            _ => return,
        };
        let end = match off.checked_add(dlen) {
            Some(e) if e <= b.len() => e,
            _ => return,
        };
        let (port_type, subtype, gas_off) =
            match (rd_u16_at(b, off + 12), rd_u16_at(b, off + 14), rd_u16_at(b, off + 18)) {
                (Some(t), Some(s), Some(g)) => (t, s, g as usize),
                _ => return,
            };
        if port_type == DBG2_PORT_SERIAL {
            let g = match off.checked_add(gas_off) {
                Some(g) if g + 12 <= end => g,
                _ => {
                    off = end;
                    continue;
                }
            };
            if let (Some(space), Some(base)) = (b.get(g).copied(), rd_u64(b, g + 4)) {
                let sub8 = u8::try_from(subtype).unwrap_or(0xFF);
                f(Dbg2Serial { kind: ConsoleKind::classify(sub8), subtype, addr_space: space, base });
            }
        }
        off = end;
    }
}

fn rd_u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// The first serial DBG2 device, if the table names one.
pub fn dbg2_first_serial(table: &[u8]) -> Option<Dbg2Serial> {
    let mut out = None;
    dbg2_serial_devices(table, |d| {
        if out.is_none() {
            out = Some(d);
        }
    });
    out
}

// --- MCFG (PCI segment groups) ------------------------------------------------------------------
//
// The ECAM window per segment: `base` plus `(end_bus - start_bus + 1)` MiB. Only the first
// segment feeds the platform description (single-host assumption, the same documented
// limit as the DTB path); the rest are listed by the iterator for the report.

/// A validated MCFG (`MCFG` signature): header + 8 reserved bytes + 16-byte entries.
#[derive(Clone, Copy)]
pub struct Mcfg<'a> {
    bytes: &'a [u8],
}

impl<'a> Mcfg<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Mcfg<'a>> {
        let h = Header::parse_sig(bytes, b"MCFG")?;
        if h.length() < 44 || (h.length() - 44) % 16 != 0 {
            return None;
        }
        Some(Mcfg { bytes: h.bytes() })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn segment_count(&self) -> usize {
        (self.bytes.len() - 44) / 16
    }
}

/// Call `f(base, segment, start_bus, end_bus)` for every MCFG entry, in table order.
pub fn mcfg_segments(mcfg: &Mcfg, mut f: impl FnMut(u64, u16, u8, u8)) {
    for i in 0..mcfg.segment_count() {
        let off = 44 + i * 16;
        if let (Some(base), Some(seg), Some(start), Some(end)) = (
            rd_u64(mcfg.bytes, off),
            rd_u16_at(mcfg.bytes, off + 8),
            mcfg.bytes.get(off + 10).copied(),
            mcfg.bytes.get(off + 11).copied(),
        ) {
            f(base, seg, start, end);
        }
    }
}

/// ECAM `(base, size)` of the first segment, if it names a non-empty bus range.
pub fn mcfg_first_ecam(mcfg: &Mcfg) -> Option<(u64, u64)> {
    let mut out = None;
    mcfg_segments(mcfg, |base, _, start, end| {
        if out.is_none() && end >= start {
            out = Some((base, ((end - start) as u64 + 1) << 20));
        }
    });
    out
}

// --- IORT (IO remapping: SMMU facts as data) ----------------------------------------------------
//
// The IORT tells the DMA/IOMMU side *which* SMMU translates *which* requester IDs. This
// crate hands those facts out as plain data — mapping and enforcement live in the SMMU
// and MMU code, never here. The one typed node is SMMUv3 (the ARM server IOMMU); every
// other node is checksum-covered and visible through the iterator, with its type and ID,
// but not interpreted.

/// IORT node types.
pub const IORT_ITS_GROUP: u8 = 0;
pub const IORT_NAMED_COMPONENT: u8 = 1;
pub const IORT_ROOT_COMPLEX: u8 = 2;
pub const IORT_SMMU_V1V2: u8 = 3;
pub const IORT_SMMU_V3: u8 = 4;
pub const IORT_PMCG: u8 = 5;

/// A validated IORT (`IORT` signature).
#[derive(Clone, Copy)]
pub struct Iort<'a> {
    bytes: &'a [u8],
}

/// One IORT node: type, ID (the namespace the mappings refer to), and raw bytes.
#[derive(Clone, Copy)]
pub struct IortNode<'a> {
    pub node_type: u8,
    pub id: u32,
    pub bytes: &'a [u8],
}

impl<'a> Iort<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Iort<'a>> {
        let h = Header::parse_sig(bytes, b"IORT")?;
        if h.length() < 48 {
            return None;
        }
        let b = h.bytes();
        let (count, at) = (rd_u32(b, 36)? as usize, rd_u32(b, 40)? as usize);
        if count > 64 || at.checked_add(16)? > b.len() {
            return None;
        }
        Some(Iort { bytes: b })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn node_count(&self) -> usize {
        rd_u32(self.bytes, 36).unwrap_or(0) as usize
    }

    /// Raw nodes in table order; stops at the first malformed one. Each node is at least
    /// the 16-byte node header; the type-specific payload follows.
    pub fn nodes(&self) -> IortNodes<'a> {
        IortNodes { bytes: self.bytes, off: rd_u32(self.bytes, 40).unwrap_or(0) as usize, left: self.node_count() }
    }
}

/// Iterator over IORT nodes.
pub struct IortNodes<'a> {
    bytes: &'a [u8],
    off: usize,
    left: usize,
}

impl<'a> Iterator for IortNodes<'a> {
    type Item = IortNode<'a>;
    fn next(&mut self) -> Option<IortNode<'a>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let ty = *self.bytes.get(self.off)?;
        let len = rd_u16_at(self.bytes, self.off + 1)? as usize;
        let id = rd_u32(self.bytes, self.off + 4)?;
        if len < 16 || self.off + len > self.bytes.len() {
            return None;
        }
        let node = IortNode { node_type: ty, id, bytes: self.bytes.get(self.off..self.off + len)? };
        self.off += len;
        Some(node)
    }
}

/// An SMMUv3 node (type 4): the facts the DMA/IOMMU side needs — MMIO base, model, and
/// the wired interrupt GSIVs. ID-mapping arrays are *counted* by the caller via the raw
/// node, not decoded here (translation policy is not a parser's business).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmmuV3 {
    pub base: u64,
    pub model: u32,
    pub event_gsiv: u32,
    pub pri_gsiv: u32,
    pub gerr_gsiv: u32,
    pub sync_gsiv: u32,
}

/// The first SMMUv3 node, if the IORT has one.
pub fn iort_smmu_v3(iort: &Iort) -> Option<SmmuV3> {
    for n in iort.nodes() {
        if n.node_type != IORT_SMMU_V3 || n.bytes.len() < 68 {
            continue;
        }
        let b = n.bytes;
        if let (Some(base), Some(model), Some(ev), Some(pri), Some(gerr), Some(sync)) = (
            rd_u64(b, 16),
            rd_u32(b, 40),
            rd_u32(b, 44),
            rd_u32(b, 48),
            rd_u32(b, 52),
            rd_u32(b, 56),
        ) {
            return Some(SmmuV3 { base, model, event_gsiv: ev, pri_gsiv: pri, gerr_gsiv: gerr, sync_gsiv: sync });
        }
    }
    None
}

// --- TPM2 (report only) -------------------------------------------------------------------------
//
// Parsed and reported so the inventory is complete; there is no consumer in the platform
// layer. Measured boot and attestation belong to a trust strand — when one needs the TPM,
// the control-area address and start method are here waiting, still checksum-verified.

/// TPM2 facts: platform class, control-area address, start method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tpm2 {
    pub platform_class: u16,
    pub control_area: u64,
    pub start_method: u32,
}

/// Parse a TPM2 (`TPM2` signature, at least through the start method).
pub fn tpm2(table: &[u8]) -> Option<Tpm2> {
    let h = Header::parse_sig(table, b"TPM2")?;
    if h.length() < 52 {
        return None;
    }
    let b = h.bytes();
    Some(Tpm2 {
        platform_class: rd_u16_at(b, 36)?,
        control_area: rd_u64(b, 40)?,
        start_method: rd_u32(b, 48)?,
    })
}

// --- PPTT (ARM CPU topology) --------------------------------------------------------------------
//
// Processor nodes (`uid`, `parent`) describe the socket/cluster/core/thread tree the DTB
// `cpu-map` describes on device-tree machines. Topology *decisions* stay in
// `hal::cpu`/`hal::smt` (one place, both firmwares); this crate contributes the validated
// walk. Cache nodes (type 1) and ID nodes (type 2) are checksum-covered and visible in
// the iterator but have no consumer — no cache driver reads them.

/// PPTT structure types.
pub const PPTT_PROCESSOR: u8 = 0;
pub const PPTT_CACHE: u8 = 1;
pub const PPTT_ID: u8 = 2;

/// A validated PPTT (`PPTT` signature). An empty one (header only) is well-formed.
#[derive(Clone, Copy)]
pub struct Pptt<'a> {
    bytes: &'a [u8],
}

impl<'a> Pptt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Pptt<'a>> {
        let h = Header::parse_sig(bytes, b"PPTT")?;
        Some(Pptt { bytes: h.bytes() })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Raw `(type, structure bytes)` pairs in table order; stops at the first malformed one.
    pub fn entries(&self) -> PpttEntries<'a> {
        PpttEntries { bytes: self.bytes, off: 36 }
    }
}

/// Iterator over PPTT structures.
pub struct PpttEntries<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Iterator for PpttEntries<'a> {
    type Item = (u8, &'a [u8]);
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let ty = *self.bytes.get(self.off)?;
        let len = *self.bytes.get(self.off + 1)? as usize;
        if len < 4 || self.off + len > self.bytes.len() {
            return None;
        }
        let entry = self.bytes.get(self.off..self.off + len)?;
        self.off += len;
        Some((ty, entry))
    }
}

/// Processor nodes: `f(acpi_uid, parent_offset, flags)` per type-0 structure. (`parent`
/// is a byte offset into this table, 0 for the root — resolve it with [`Pptt::entries`].)
pub fn pptt_processors(pptt: &Pptt, mut f: impl FnMut(u32, u32, u32)) {
    for (t, e) in pptt.entries() {
        if t != PPTT_PROCESSOR || e.len() < 20 {
            continue;
        }
        if let (Some(flags), Some(parent), Some(uid)) = (rd_u32(e, 4), rd_u32(e, 8), rd_u32(e, 12)) {
            f(uid, parent, flags);
        }
    }
}

/// Number of PPTT structures of `entry_type`.
pub fn pptt_count(pptt: &Pptt, entry_type: u8) -> usize {
    pptt.entries().filter(|(t, _)| *t == entry_type).count()
}

// --- UEFI configuration-table discovery ----------------------------------------------------------
//
// On UEFI the RSDP address is not scanned for — it is handed over in the configuration
// table under one of two GUIDs (v1 for the RSDT address, v2 for the XSDT address). These
// helpers classify and select purely: the caller walks
// `EFI_SYSTEM_TABLE.ConfigurationTable`, matches the GUID, and hands the address to the
// slice parsers above. The legacy BIOS-area scan (EBDA/0xE0000) stays an x86-only
// fallback in the x86 HAL, which owns live-memory access; this crate never scans.

/// An EFI configuration-table GUID (little-endian fields, as UEFI stores them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub a: u32,
    pub b: u16,
    pub c: u16,
    pub d: [u8; 8],
}

/// `EFI_ACPI_TABLE_GUID` — the RSDP (revision 1.x, RSDT address).
pub const EFI_ACPI_TABLE_GUID: Guid = Guid {
    a: 0x8868_e871,
    b: 0xe4f1,
    c: 0x11d3,
    d: [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
};

/// `EFI_ACPI_20_TABLE_GUID` — the RSDP (revision 2.0+, XSDT address).
pub const EFI_ACPI_20_TABLE_GUID: Guid = Guid {
    a: 0x8868_e871,
    b: 0xe4f1,
    c: 0x11d3,
    d: [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x82],
};

/// Which ACPI revision a configuration-table GUID announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UefiAcpiRev {
    V1,
    V2,
}

/// Classify a configuration-table GUID, if it is one of the two ACPI ones.
pub fn uefi_acpi_kind(guid: &Guid) -> Option<UefiAcpiRev> {
    if *guid == EFI_ACPI_20_TABLE_GUID {
        Some(UefiAcpiRev::V2)
    } else if *guid == EFI_ACPI_TABLE_GUID {
        Some(UefiAcpiRev::V1)
    } else {
        None
    }
}

/// Select the RSDP address from UEFI configuration-table `(guid, address)` pairs,
/// preferring the v2 entry (XSDT, 64-bit addresses). `None` when neither GUID is present.
pub fn uefi_select_rsdp(entries: &[(Guid, u64)]) -> Option<(UefiAcpiRev, u64)> {
    let mut v1 = None;
    for (g, addr) in entries {
        match uefi_acpi_kind(g) {
            Some(UefiAcpiRev::V2) => return Some((UefiAcpiRev::V2, *addr)),
            Some(UefiAcpiRev::V1) if v1.is_none() => v1 = Some((UefiAcpiRev::V1, *addr)),
            _ => {}
        }
    }
    v1
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// One SDT with a correct header + checksum over `body`.
    fn table(sig: &[u8; 4], rev: u8, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&sig[..]);
        v.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
        v.push(rev);
        v.push(0); // checksum, fixed up below
        v.extend_from_slice(b"OEMID0");
        v.extend_from_slice(b"OEMTABLE");
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(b"CRID");
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(body);
        let sum = v.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        v[9] = (0u8).wrapping_sub(sum);
        v
    }

    fn gicd_entry(base: u64, version: u8) -> Vec<u8> {
        let mut e = std::vec![12u8, 24, 0, 0];
        e.extend_from_slice(&0u32.to_le_bytes()); // GIC ID
        e.extend_from_slice(&base.to_le_bytes());
        e.extend_from_slice(&0u32.to_le_bytes()); // system vector base
        e.push(version);
        e.extend_from_slice(&[0u8; 3]);
        e
    }

    fn gicr_entry(base: u64, len: u32) -> Vec<u8> {
        let mut e = std::vec![13u8, 16, 0, 0];
        e.extend_from_slice(&base.to_le_bytes());
        e.extend_from_slice(&len.to_le_bytes());
        e
    }

    fn its_entry(base: u64) -> Vec<u8> {
        let mut e = std::vec![14u8, 20, 0, 0];
        e.extend_from_slice(&0u32.to_le_bytes()); // ITS ID
        e.extend_from_slice(&base.to_le_bytes());
        e.extend_from_slice(&0u32.to_le_bytes());
        e
    }

    fn gicc_entry() -> Vec<u8> {
        let mut e = std::vec![11u8, 80, 0, 0];
        e.extend_from_slice(&[0u8; 76]);
        e
    }

    /// The first GICC's CPU interface base (GICv2 QEMU-like MADTs carry it).
    fn gicc_entry_with_base(base: u64) -> Vec<u8> {
        let mut e = std::vec![11u8, 80, 0, 0];
        e.extend_from_slice(&[0u8; 28]); // up to offset 32
        e.extend_from_slice(&base.to_le_bytes());
        e.extend_from_slice(&[0u8; 40]); // rest
        e
    }

    /// MADT shaped like QEMU `virt` with GICv2: GICD + two GICCs, no redistributor, no ITS.
    fn qemu_like_madt() -> Vec<u8> {
        let mut body = std::vec![0u8; 8]; // LocalApicAddress + Flags
        body.extend_from_slice(&gicd_entry(0x0800_0000, 2));
        body.extend_from_slice(&gicc_entry_with_base(0x0801_0000));
        body.extend_from_slice(&gicc_entry());
        table(b"APIC", 6, &body)
    }

    /// MADT shaped like a GICv3 server part: GICD + GICR range + ITS + one GICC.
    fn gicv3_madt() -> Vec<u8> {
        let mut body = std::vec![0u8; 8];
        body.extend_from_slice(&gicd_entry(0x1700_0000, 3));
        body.extend_from_slice(&gicr_entry(0x1708_0000, 0x30_0000));
        body.extend_from_slice(&its_entry(0x1704_0000));
        body.extend_from_slice(&gicc_entry());
        table(b"APIC", 6, &body)
    }

    fn gtdt_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0u64.to_le_bytes()); // CNTControlBase: none
        body.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for gsiv in [29u32, 30, 27, 26] {
            body.extend_from_slice(&gsiv.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // flags
        }
        table(b"GTDT", 3, &body)
    }

    fn spcr_blob(interface: u8, base: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(interface);
        body.extend_from_slice(&[0u8; 3]); // reserved
        body.push(0); // GAS: memory space
        body.push(4); // bit width
        body.push(0); // bit offset
        body.push(3); // access size (dword)
        body.extend_from_slice(&base.to_le_bytes());
        body.push(1); // interrupt type: GSI
        body.push(0); // IRQ (unused for GSI)
        body.extend_from_slice(&33u32.to_le_bytes()); // GSI
        body.push(7); // 115200
        body.extend_from_slice(&[0u8; 5]); // parity/stop/flow/term/reserved
        body.extend_from_slice(&0u16.to_le_bytes()); // PCI dev id
        body.extend_from_slice(&0u16.to_le_bytes()); // PCI vendor id
        body.extend_from_slice(&[0u8; 3]); // bus/device/function
        body.extend_from_slice(&0u32.to_le_bytes()); // PCI flags
        body.push(0); // segment
        body.extend_from_slice(&[0u8; 4]); // reserved
        table(b"SPCR", 2, &body)
    }

    fn rsdp_blob(rev: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RSD PTR ");
        v.push(0); // checksum, fixed below
        v.extend_from_slice(b"OEMID0");
        v.push(rev);
        v.extend_from_slice(&0x11_0000u32.to_le_bytes()); // RSDT
        if rev >= 2 {
            v.extend_from_slice(&36u32.to_le_bytes());
            v.extend_from_slice(&0x22_0000u64.to_le_bytes()); // XSDT
            v.push(0); // extended checksum, fixed below
            v.extend_from_slice(&[0u8; 3]);
        }
        let sum = v[..20].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        v[8] = (0u8).wrapping_sub(sum);
        if rev >= 2 {
            let sum = v.iter().fold(0u8, |a, &x| a.wrapping_add(x));
            v[32] = (0u8).wrapping_sub(sum);
        }
        v
    }

    fn xsdt_blob(addrs: &[u64]) -> Vec<u8> {
        let mut body = Vec::new();
        for a in addrs {
            body.extend_from_slice(&a.to_le_bytes());
        }
        table(b"XSDT", 1, &body)
    }

    #[test]
    fn qemu_like_madt_names_gicv2_bases() {
        let m = qemu_like_madt();
        let madt = Madt::parse(&m).expect("valid MADT");
        assert_eq!(gicd(&madt), Some(Gicd { base: 0x0800_0000, version: 2 }));
        assert_eq!(first_gicr(&madt), None);
        assert_eq!(first_its(&madt), None);
        assert_eq!(gicc_count(&madt), 2);
        assert_eq!(first_gicc_base(&madt), Some(0x0801_0000));
    }

    #[test]
    fn gicv3_madt_names_redistributor_and_its() {
        let m = gicv3_madt();
        let madt = Madt::parse(&m).expect("valid MADT");
        assert_eq!(gicd(&madt), Some(Gicd { base: 0x1700_0000, version: 3 }));
        assert_eq!(
            first_gicr(&madt),
            Some(Gicr { base: 0x1708_0000, length: 0x30_0000 })
        );
        assert_eq!(first_its(&madt), Some(0x1704_0000));
        assert_eq!(gicc_count(&madt), 1);
    }

    #[test]
    fn gtdt_names_the_ns_el1_timer() {
        let g = gtdt_blob();
        let info = gtdt(&g).expect("valid GTDT");
        assert_eq!(info.ns_el1_gsiv, 30);
        assert_eq!(info.secure_el1_gsiv, 29);
        assert_eq!(info.virt_gsiv, 27);
        assert_eq!(info.el2_gsiv, 26);
        assert_eq!(platform_timer_count(&g), None, "this GTDT is too short for platform timers");
    }

    #[test]
    fn spcr_pl011_and_16550_classify() {
        let p = spcr_blob(SPCR_IF_PL011, 0x0900_0000).clone();
        let info = spcr(&p).expect("valid SPCR");
        assert_eq!(info.kind, ConsoleKind::Pl011);
        assert_eq!((info.base, info.addr_space), (0x0900_0000, 0));
        assert_eq!(info.baud_code, 7);
        assert_eq!(info.gsi, 33);
        let n = spcr_blob(SPCR_IF_16550, 0x1000_0000).clone();
        assert_eq!(spcr(&n).unwrap().kind, ConsoleKind::Ns16550);
        let s = spcr_blob(SPCR_IF_SBSA, 0x2000_0000).clone();
        assert_eq!(spcr(&s).unwrap().kind, ConsoleKind::Sbsa);
        let u = spcr_blob(0x42, 0).clone();
        assert_eq!(spcr(&u).unwrap().kind, ConsoleKind::Unknown(0x42));
    }

    #[test]
    fn arm_platform_reduction_qemu_like() {
        let m = qemu_like_madt();
        let g = gtdt_blob();
        let s = spcr_blob(SPCR_IF_PL011, 0x0900_0000);
        let p = arm_platform(&m, &g, Some(&s)).expect("reduction works");
        assert_eq!(p.dist_base, 0x0800_0000);
        assert_eq!(p.dist_version, 2);
        assert_eq!(p.redist_base, None);
        assert_eq!(p.its_base, None);
        assert_eq!(p.timer_gsiv, 30);
        assert_eq!(p.uart.unwrap().base, 0x0900_0000);
        // Without an SPCR there is simply no ACPI console — not an error.
        let q = arm_platform(&m, &g, None).expect("no SPCR is fine");
        assert_eq!(q.uart, None);
        assert_eq!(q.timer_gsiv, 30);
    }

    #[test]
    fn rsdp_v1_and_v2_addresses() {
        let v1 = rsdp_blob(0);
        let r = Rsdp::parse(&v1).expect("v1 RSDP");
        assert_eq!(r.revision(), 0);
        assert_eq!(r.rsdt_address(), Some(0x11_0000));
        assert_eq!(r.xsdt_address(), None);
        let v2 = rsdp_blob(2);
        let r = Rsdp::parse(&v2).expect("v2 RSDP");
        assert_eq!(r.xsdt_address(), Some(0x22_0000));
    }

    #[test]
    fn xsdt_lists_table_addresses() {
        let x = xsdt_blob(&[0x1000, 0x2000, 0x3000]);
        let h = Header::parse_sig(&x, b"XSDT").expect("valid XSDT");
        assert_eq!(xsdt_count(h.bytes()), Some(3));
        assert_eq!(xsdt_entry(h.bytes(), 0), Some(0x1000));
        assert_eq!(xsdt_entry(h.bytes(), 2), Some(0x3000));
        assert_eq!(xsdt_entry(h.bytes(), 3), None);
        assert_eq!(entry_count(h.bytes(), 5), None, "only 4/8-byte entries exist");
    }

    #[test]
    fn fadt_is_header_only() {
        let f = table(b"FACP", 6, &[0u8; 100]);
        assert!(fadt(&f).is_some());
        assert!(fadt(&table(b"FACP", 0, &[0u8; 100])).is_none(), "revision 0 is bogus");
        assert!(fadt(&table(b"APIC", 6, &[0u8; 100])).is_none(), "wrong signature");
    }

    #[test]
    fn handover_list_reduces_without_following_pointers() {
        let m = qemu_like_madt();
        let g = gtdt_blob();
        let s = spcr_blob(SPCR_IF_PL011, 0x0900_0000);
        // Order is irrelevant and extra tables are ignored — it is a list, not a layout.
        let tables: [(&[u8; 4], &[u8]); 4] =
            [(b"GTDT", &g), (b"FACP", &table(b"FACP", 6, &[0u8; 100])), (b"SPCR", &s), (b"APIC", &m)];
        let p = arm_platform_from_handover(&tables).expect("handover reduces");
        assert_eq!(p.dist_base, 0x0800_0000);
        assert_eq!(p.gicc_base, Some(0x0801_0000));
        assert_eq!(p.timer_gsiv, 30);
        assert_eq!(handover_find(&tables, b"DMAR"), None);
        // Without a GTDT there is no timer source — `None`, not a guess.
        let no_timer: [(&[u8; 4], &[u8]); 1] = [(b"APIC", &m)];
        assert!(arm_platform_from_handover(&no_timer).is_none());
        assert!(arm_platform_from_handover(&[]).is_none());
        // A corrupted table in the handover is rejected, never half-read.
        let mut bad = m.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        let poisoned: [(&[u8; 4], &[u8]); 2] = [(b"APIC", &bad), (b"GTDT", &g)];
        assert!(arm_platform_from_handover(&poisoned).is_none());
    }

    #[test]
    fn negative_blobs_are_rejected() {
        let m = qemu_like_madt();
        assert!(Madt::parse(b"").is_none());
        assert!(Madt::parse(&[0u8; 64]).is_none(), "bad signature and checksum");
        // Right signature, broken checksum.
        let mut bad = m.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(Madt::parse(&bad).is_none());
        // Truncated length field: claims more than it holds.
        let mut lying = m.clone();
        let len = (lying.len() + 100) as u32;
        lying[4..8].copy_from_slice(&len.to_le_bytes());
        assert!(Madt::parse(&lying).is_none());
        // GTDT too short for the timer fields.
        assert!(gtdt(&table(b"GTDT", 3, &[0u8; 8])).is_none());
        assert!(gtdt(&spcr_blob(SPCR_IF_PL011, 0)).is_none(), "wrong signature");
        // SPCR too short.
        assert!(spcr(&table(b"SPCR", 2, &[0u8; 8])).is_none());
        // RSDP garbage.
        assert!(Rsdp::parse(&[0u8; 36]).is_none());
        assert!(Rsdp::parse(b"RSD PTR too short").is_none());
        // Reduction needs both tables.
        let g = gtdt_blob();
        assert!(arm_platform(&[0u8; 64], &g, None).is_none());
        assert!(arm_platform(&m, &[0u8; 64], None).is_none());
        assert!(arm_platform(&m, &g, Some(&[0u8; 64])).is_none(), "bad SPCR poisons the UART only");
    }

    #[test]
    fn truncation_never_panics() {
        let blobs = [qemu_like_madt(), gicv3_madt(), gtdt_blob(), spcr_blob(SPCR_IF_PL011, 0), rsdp_blob(2)];
        for blob in blobs {
            let mut cut = 0;
            while cut <= blob.len() {
                let prefix = &blob[..cut];
                if Header::parse(prefix).is_some() {
                    // Valid prefix: full-table parsers must still not panic.
                }
                let _ = Madt::parse(prefix);
                let _ = gtdt(prefix);
                let _ = spcr(prefix);
                let _ = Rsdp::parse(prefix);
                let _ = fadt(prefix);
                let _ = xsdt_count(prefix);
                let _ = arm_platform(prefix, prefix, Some(prefix));
                cut += if cut < 60 { 1 } else { 7 };
            }
        }
    }

    #[test]
    fn corrupted_bytes_never_panic() {
        let mut v = gicv3_madt();
        for i in (8..v.len()).step_by(5) {
            let old = v[i];
            v[i] = 0xa5;
            if let Some(m) = Madt::parse(&v) {
                let _ = gicd(&m);
                let _ = first_gicr(&m);
                let _ = first_its(&m);
                let _ = gicc_count(&m);
                let _ = arm_platform(&v, &gtdt_blob(), None);
            }
            v[i] = old;
        }
    }

    // --- new tables: builders -----------------------------------------------------------

    fn srat_blob() -> Vec<u8> {
        let mut body = std::vec![0u8; 4]; // reserved
        // Type 0: LAPIC, id 3, node 1, enabled.
        body.extend_from_slice(&[0u8, 16, 1, 3]);
        body.extend_from_slice(&1u32.to_le_bytes()); // flags
        body.extend_from_slice(&[0u8, 0, 0, 0]); // sapic eid, proximity hi
        body.extend_from_slice(&0u32.to_le_bytes()); // clock domain
        // Type 1: memory 0x8000_0000+2G on node 0, enabled.
        body.extend_from_slice(&[1u8, 40]);
        body.extend_from_slice(&0u32.to_le_bytes()); // proximity
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&0x8000_0000u64.to_le_bytes());
        body.extend_from_slice(&0x8000_0000u64.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // flags
        body.extend_from_slice(&0u64.to_le_bytes());
        // Type 2: x2APIC id 7 on node 1, disabled.
        body.extend_from_slice(&[2u8, 24, 0, 0]);
        body.extend_from_slice(&1u32.to_le_bytes()); // proximity
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // flags
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        // Type 3: GICC uid 42 on node 0, enabled.
        body.extend_from_slice(&[3u8, 18]);
        body.extend_from_slice(&0u32.to_le_bytes()); // proximity
        body.extend_from_slice(&42u32.to_le_bytes()); // uid
        body.extend_from_slice(&1u32.to_le_bytes()); // flags
        body.extend_from_slice(&0u32.to_le_bytes()); // clock domain
        // Type 4 (ITS affinity): present, uninterpreted, still checksum-covered.
        body.extend_from_slice(&[4u8, 16]);
        body.extend_from_slice(&[0u8; 14]);
        table(b"SRAT", 3, &body)
    }

    fn slit_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&2u64.to_le_bytes());
        body.extend_from_slice(&[10u8, 20, 20, 10]); // symmetric 2-node matrix
        table(b"SLIT", 1, &body)
    }

    fn dbg2_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&44u32.to_le_bytes()); // first device right after
        body.extend_from_slice(&2u32.to_le_bytes()); // two devices
        // Device 0: PL011 serial, base 0x9000000.
        let mut d0 = Vec::new();
        d0.push(0); // revision
        d0.extend_from_slice(&0u16.to_le_bytes()); // length, fixed below
        d0.push(1); // one GAS
        d0.extend_from_slice(&[0u8; 8]); // namespace/oem lengths+offsets
        d0.extend_from_slice(&0x8000u16.to_le_bytes()); // serial
        d0.extend_from_slice(&3u16.to_le_bytes()); // PL011
        d0.extend_from_slice(&0u16.to_le_bytes()); // reserved
        d0.extend_from_slice(&22u16.to_le_bytes()); // GAS offset
        d0.extend_from_slice(&1u16.to_le_bytes()); // address size
        d0.push(0);
        d0.push(4);
        d0.push(0);
        d0.push(3);
        d0.extend_from_slice(&0x0900_0000u64.to_le_bytes());
        let len_pos = 1;
        let d0len = d0.len() as u16;
        d0[len_pos..len_pos + 2].copy_from_slice(&d0len.to_le_bytes());
        // Device 1: network port (never selected).
        let mut d1 = Vec::new();
        d1.push(0);
        d1.extend_from_slice(&0u16.to_le_bytes());
        d1.push(1);
        d1.extend_from_slice(&[0u8; 8]);
        d1.extend_from_slice(&0x8003u16.to_le_bytes()); // network
        d1.extend_from_slice(&0u16.to_le_bytes());
        d1.extend_from_slice(&0u16.to_le_bytes());
        d1.extend_from_slice(&22u16.to_le_bytes());
        d1.extend_from_slice(&1u16.to_le_bytes());
        d1.extend_from_slice(&[0u8; 12]);
        let d1len = d1.len() as u16;
        d1[1..3].copy_from_slice(&d1len.to_le_bytes());
        body.extend_from_slice(&d0);
        body.extend_from_slice(&d1);
        table(b"DBG2", 0, &body)
    }

    fn mcfg_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 8]); // reserved
        body.extend_from_slice(&0x40_1000_0000u64.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // segment 0
        body.push(0);
        body.push(255);
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&0x50_0000_0000u64.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // segment 1
        body.push(0);
        body.push(15);
        body.extend_from_slice(&[0u8; 4]);
        table(b"MCFG", 1, &body)
    }

    fn iort_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // one node
        body.extend_from_slice(&48u32.to_le_bytes()); // at offset 48
        body.extend_from_slice(&0u32.to_le_bytes()); // reserved
        // SMMUv3 node, length 68.
        let mut n = std::vec![4u8];
        n.extend_from_slice(&68u16.to_le_bytes());
        n.push(3); // revision
        n.extend_from_slice(&0u32.to_le_bytes()); // id
        n.extend_from_slice(&0u32.to_le_bytes()); // mappings
        n.extend_from_slice(&0u32.to_le_bytes()); // mapping offset
        n.extend_from_slice(&0x1000_0000u64.to_le_bytes()); // base
        n.extend_from_slice(&0u32.to_le_bytes()); // flags
        n.extend_from_slice(&0u32.to_le_bytes()); // reserved
        n.extend_from_slice(&0u64.to_le_bytes()); // vatos
        n.extend_from_slice(&1u32.to_le_bytes()); // model
        n.extend_from_slice(&48u32.to_le_bytes()); // event
        n.extend_from_slice(&49u32.to_le_bytes()); // pri
        n.extend_from_slice(&50u32.to_le_bytes()); // gerr
        n.extend_from_slice(&51u32.to_le_bytes()); // sync
        n.extend_from_slice(&0u32.to_le_bytes()); // proximity domain
        n.extend_from_slice(&0u32.to_le_bytes()); // device ID mapping index
        body.extend_from_slice(&n);
        table(b"IORT", 3, &body)
    }

    fn tpm2_blob() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes()); // class
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0xfed4_0000u64.to_le_bytes());
        body.extend_from_slice(&6u32.to_le_bytes()); // start method
        table(b"TPM2", 4, &body)
    }

    fn pptt_blob() -> Vec<u8> {
        let mut body = Vec::new();
        // Socket node (uid 0, parent 0).
        body.extend_from_slice(&[0u8, 20, 0, 0]);
        body.extend_from_slice(&2u32.to_le_bytes()); // flags: physical package
        body.extend_from_slice(&0u32.to_le_bytes()); // parent
        body.extend_from_slice(&0u32.to_le_bytes()); // uid
        body.extend_from_slice(&0u32.to_le_bytes()); // resources
        // Core node (uid 5, parent = socket at offset 36).
        body.extend_from_slice(&[0u8, 20, 0, 0]);
        body.extend_from_slice(&8u32.to_le_bytes()); // flags: leaf
        body.extend_from_slice(&36u32.to_le_bytes()); // parent
        body.extend_from_slice(&5u32.to_le_bytes()); // uid
        body.extend_from_slice(&0u32.to_le_bytes());
        // Cache node (uninterpreted).
        body.extend_from_slice(&[1u8, 16, 0, 0]);
        body.extend_from_slice(&[0u8; 12]);
        table(b"PPTT", 2, &body)
    }

    #[test]
    fn srat_names_memory_gicc_x2apic_and_lapic() {
        let s = srat_blob();
        let srat = Srat::parse(&s).expect("valid SRAT");
        let mut mem = Vec::new();
        srat_memory(&srat, |b, l, n, e| mem.push((b, l, n, e)));
        assert_eq!(mem, [(0x8000_0000, 0x8000_0000, 0, true)]);
        let mut g = Vec::new();
        srat_gicc(&srat, |u, n, e| g.push((u, n, e)));
        assert_eq!(g, [(42, 0, true)]);
        let mut x = Vec::new();
        srat_x2apic(&srat, |i, n, e| x.push((i, n, e)));
        assert_eq!(x, [(7, 1, false)]);
        let mut l = Vec::new();
        srat_lapic(&srat, |i, n, e| l.push((i, n, e)));
        assert_eq!(l, [(3, 1, true)]);
        assert_eq!(srat_count(&srat, SRAT_MEMORY), 1);
        assert_eq!(srat_count(&srat, 4), 1, "ITS affinity is visible, not interpreted");
    }

    #[test]
    fn slit_matrix_reports_distances() {
        let s = slit_blob();
        assert_eq!(slit_count(&s), Some(2));
        assert_eq!(slit_distance(&s, 0, 0), Some(10));
        assert_eq!(slit_distance(&s, 0, 1), Some(20));
        assert_eq!(slit_distance(&s, 1, 0), Some(20));
        assert_eq!(slit_distance(&s, 0, 2), None);
        assert!(slit_count(&table(b"SLIT", 1, &[0u8; 8])).is_none(), "count 0 is degenerate");
    }

    #[test]
    fn dbg2_selects_serial_not_network() {
        let d = dbg2_blob();
        let first = dbg2_first_serial(&d).expect("serial DBG2 device");
        assert_eq!(first.kind, ConsoleKind::Pl011);
        assert_eq!((first.base, first.addr_space), (0x0900_0000, 0));
        let mut all = Vec::new();
        dbg2_serial_devices(&d, |x| all.push(x.subtype));
        assert_eq!(all, [3], "the network port is never selected");
        assert!(dbg2_first_serial(&table(b"DBG2", 0, &[0u8; 8])).is_none());
    }

    #[test]
    fn mcfg_lists_segments_first_feeds_ecam() {
        let m = mcfg_blob();
        let mcfg = Mcfg::parse(&m).expect("valid MCFG");
        assert_eq!(mcfg.segment_count(), 2);
        let mut segs = Vec::new();
        mcfg_segments(&mcfg, |b, s, a, e| segs.push((b, s, a, e)));
        assert_eq!(segs, [(0x40_1000_0000, 0, 0, 255), (0x50_0000_0000, 1, 0, 15)]);
        assert_eq!(mcfg_first_ecam(&mcfg), Some((0x40_1000_0000, 256 << 20)));
        assert!(Mcfg::parse(&table(b"MCFG", 1, &[0u8; 9])).is_none(), "ragged tail rejected");
    }

    #[test]
    fn iort_names_the_smmuv3() {
        let i = iort_blob();
        let iort = Iort::parse(&i).expect("valid IORT");
        assert_eq!(iort.node_count(), 1);
        let nodes: Vec<(u8, u32)> = iort.nodes().map(|n| (n.node_type, n.id)).collect();
        assert_eq!(nodes, [(IORT_SMMU_V3, 0)]);
        assert_eq!(
            iort_smmu_v3(&iort),
            Some(SmmuV3 { base: 0x1000_0000, model: 1, event_gsiv: 48, pri_gsiv: 49, gerr_gsiv: 50, sync_gsiv: 51 })
        );
        assert!(Iort::parse(&table(b"IORT", 3, &[0u8; 8])).is_none(), "no room for a node");
    }

    #[test]
    fn tpm2_reports_facts() {
        let t = tpm2_blob();
        assert_eq!(
            tpm2(&t),
            Some(Tpm2 { platform_class: 1, control_area: 0xfed4_0000, start_method: 6 })
        );
        assert!(tpm2(&table(b"TPM2", 4, &[0u8; 8])).is_none());
    }

    #[test]
    fn pptt_names_processor_nodes_caches_visible() {
        let p = pptt_blob();
        let pptt = Pptt::parse(&p).expect("valid PPTT");
        let mut procs = Vec::new();
        pptt_processors(&pptt, |u, par, f| procs.push((u, par, f)));
        assert_eq!(procs, [(0, 0, 2), (5, 36, 8)]);
        assert_eq!(pptt_count(&pptt, PPTT_PROCESSOR), 2);
        assert_eq!(pptt_count(&pptt, PPTT_CACHE), 1);
        assert!(Pptt::parse(&table(b"APIC", 2, &[])).is_none(), "wrong signature");
    }

    #[test]
    fn uefi_guids_classify_and_prefer_v2() {
        assert_eq!(uefi_acpi_kind(&EFI_ACPI_TABLE_GUID), Some(UefiAcpiRev::V1));
        assert_eq!(uefi_acpi_kind(&EFI_ACPI_20_TABLE_GUID), Some(UefiAcpiRev::V2));
        assert_eq!(uefi_acpi_kind(&Guid { a: 0, b: 0, c: 0, d: [0; 8] }), None);
        let other = Guid { a: 1, b: 2, c: 3, d: [4; 8] };
        // v2 wins regardless of order; v1 alone still works; neither is None.
        let lists: [[(Guid, u64); 2]; 2] = [
            [(EFI_ACPI_TABLE_GUID, 0x1000), (EFI_ACPI_20_TABLE_GUID, 0x2000)],
            [(EFI_ACPI_20_TABLE_GUID, 0x2000), (EFI_ACPI_TABLE_GUID, 0x1000)],
        ];
        for l in lists {
            assert_eq!(uefi_select_rsdp(&l), Some((UefiAcpiRev::V2, 0x2000)));
        }
        assert_eq!(uefi_select_rsdp(&[(EFI_ACPI_TABLE_GUID, 0x1000)]), Some((UefiAcpiRev::V1, 0x1000)));
        assert_eq!(uefi_select_rsdp(&[(other, 0x3000)]), None);
        assert_eq!(uefi_select_rsdp(&[]), None);
    }

    #[test]
    fn real_aavmf_tables_parse() {
        // Carved from a real AAVMF boot (`virt`, cortex-a72, 2026-10-09): genuine firmware
        // bytes. Same refresh procedure as the OVMF fixtures above.
        let m = include_bytes!("../tests/fixtures/aavmf-virt-madt.bin");
        let madt = Madt::parse(m).expect("real AAVMF MADT parses");
        let types: Vec<(u8, usize)> =
            madt.entries().map(|(t, e)| (t, e.len())).collect();
        assert_eq!(types, [(12, 24), (11, 80), (13, 24)]);
        assert_eq!(gicd(&madt), Some(Gicd { base: 0x0800_0000, version: 2 }));
        assert_eq!(gicc_count(&madt), 1);
        assert_eq!(first_gicc_base(&madt), Some(0x0801_0000));
        // This firmware emits a 24-byte type-13 entry on a GICv2 machine. The parser
        // reports it faithfully (framing is valid, checksum covers it); the platform
        // layer ignores redistributors below GICv3, where none exist by architecture.
        let d = include_bytes!("../tests/fixtures/aavmf-virt-dbg2.bin");
        let first = dbg2_first_serial(d).expect("real AAVMF DBG2 names a serial port");
        assert_eq!(first.kind, ConsoleKind::Pl011);
        assert_eq!((first.base, first.addr_space), (0x0900_0000, 0));
        let mc = include_bytes!("../tests/fixtures/aavmf-virt-mcfg.bin");
        let mcfg = Mcfg::parse(mc).expect("real AAVMF MCFG parses");
        assert_eq!(mcfg_first_ecam(&mcfg), Some((0x40_1000_0000, 256 << 20)));
    }

    #[test]
    fn real_ovmf_tables_parse() {
        // Carved from a real QEMU edk2 boot (OVMF_CODE.4m.fd, q35, 2026-10-09): genuine
        // firmware bytes, not shaped blobs. Procedure to refresh: boot OVMF, QMP
        // `dump-guest-memory`, carve for signatures with valid checksums.
        let m = include_bytes!("../tests/fixtures/ovmf-q35-madt.bin");
        let madt = Madt::parse(m).expect("real OVMF MADT parses");
        let types: Vec<(u8, usize)> =
            madt.entries().map(|(t, e)| (t, e.len())).collect();
        assert_eq!(types[0], (0, 8), "first entry is a local APIC");
        assert!(types.iter().any(|(t, _)| *t == 1), "an IOAPIC entry exists");
        assert_eq!(gicc_count(&madt), 0, "x86 has no GICC entries");
        assert_eq!(gicd(&madt), None, "x86 has no GICD entry");
        let mc = include_bytes!("../tests/fixtures/ovmf-q35-mcfg.bin");
        let mcfg = Mcfg::parse(mc).expect("real OVMF MCFG parses");
        assert_eq!(mcfg.segment_count(), 1);
        assert_eq!(
            mcfg_first_ecam(&mcfg),
            Some((0xe000_0000, 256 << 20)),
            "Q35 ECAM window"
        );
    }

    #[test]
    fn new_tables_reject_garbage_and_truncation() {
        for mk in [srat_blob(), slit_blob(), dbg2_blob(), mcfg_blob(), iort_blob(), tpm2_blob(), pptt_blob()] {
            // Every prefix must parse-or-reject, never panic.
            let mut cut = 0;
            while cut <= mk.len() {
                let p = &mk[..cut];
                let _ = Srat::parse(p);
                let _ = slit_count(p);
                let _ = dbg2_first_serial(p);
                let _ = Mcfg::parse(p);
                let _ = Iort::parse(p);
                let _ = tpm2(p);
                let _ = Pptt::parse(p);
                cut += if cut < 60 { 1 } else { 5 };
            }
            // Flipped bytes: checksum dies, parsers say no.
            let mut bad = mk.clone();
            if bad.len() > 40 {
                bad[37] ^= 0xff;
                assert!(Header::parse(&bad).is_none(), "checksum must fail");
            }
        }
        assert!(Srat::parse(&[0u8; 64]).is_none());
        assert!(Mcfg::parse(&slit_blob()).is_none());
        assert!(Iort::parse(&mcfg_blob()).is_none());
    }
}
