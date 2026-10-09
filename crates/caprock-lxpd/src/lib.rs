//! LXPD-Transportprüfung — die Abnahmeseite der lxport-Pipeline.
//!
//! `lx-bind` erzeugt **zwei** Transportformen, und beide müssen hier bestehen, bevor der
//! Kernel sie anfasst:
//!
//! * **(a) LXPD-v1-Container** (`LXPD`-Magic + Zähler + Trampolin-Stubs + `LXEND`), das
//!   deterministische Pseudo-Image aus `bind()`;
//! * **(b) reines ET_EXEC-ELF** (echtes ELF mit Trampolin- + Manifest-Segment) aus dem
//!   parallelen `bind_elf()`-Pfad — erkannt an der ELF-Magic, gemessen an den Loader-Regeln.
//!
//! Das JSON-Manifest kommt **separat** als Datei (der v1-Container enthält keines) und wird
//! in [`manifest`] geprüft: Feld-Fakten per Hand auf Byte-Ebene (kein Full-Parser, kein
//! `alloc`), die Signatur als echtes FNV-1a-64 über Key-Bytes + kanonische Bytes —
//! **niemals** ein angenommener Pass ohne Rechnung. Treiber-**Herkunft** (Boot-Modul gegen
//! Platte), Bild-Bindung per SHA-256 und Mengen-Überlappung prüft [`driver`] — mit denselben
//! Mitteln (Byte-Ebene, kein `alloc`, alles in [`LxpdError`]).
//!
//! Konventionen wie `caprock-loader`: `#![forbid(unsafe_code)]`, nur `&[u8]`-Sichten, jeder
//! Fehler landet in [`LxpdError`] — nie in einem Out-of-Bounds-Zugriff oder Panic.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod driver;
pub mod manifest;

pub use driver::{DriverEntry, Source};
pub use manifest::ManifestFacts;

use caprock_loader::elf::ElfImage;
use caprock_loader::LoaderError;

// --- Transport-Konstanten --------------------------------------------------

/// Magic am Anfang eines LXPD-v1-Containers (4 Bytes `"LXPD"`).
pub const LXPD_MAGIC: &[u8; 4] = b"LXPD";
/// Magic am Anfang jedes 32-Byte-Trampolin-Stubs (`"LXTR"`).
pub const LXTR_MAGIC: &[u8; 4] = b"LXTR";
/// End-Marker am Schluss des Containers (`"LXEND"`).
pub const LXEND_MARKER: &[u8; 5] = b"LXEND";
/// ELF-Magic (`0x7F 'E' 'L' 'F'`) — entscheidet, ob Pfad (b) greift.
pub const ELF_MAGIC: &[u8; 4] = &[0x7F, b'E', b'L', b'F'];
/// Breite eines Trampolin-Stubs in Bytes (`LXTR` + id + 2×FNV32 + NOP-Padding).
pub const STUB_LEN: usize = 32;
/// Kopfbreite des Containers: Magic(4) + tramp_count(4) + rewired_count(4) + section_size(8).
pub const HEADER_LEN: usize = 20;
/// Harte Schranke der Stubzahl: die Startmenge eines Treibers ist überschaubar, und eine
/// Schranke hier ist billiger als eine Schleife über eine fremde `u32`.
pub const MAX_TRAMPS: usize = 256;
/// NOP-Padding der Stub-Hinterhälfte (`[16..32]`).
const NOP_PAD: [u8; 16] = [0x90; 16];

// --- Fehlertexte als `&str`-Konstanten (ein Ort, keine verstreuten Literale) ---

/// Container endet vor der angekündigten Struktur.
pub const MSG_TOO_SMALL: &str = "lxpd: image ends before the announced structure";
/// Keine `LXPD`-Magic am Image-Anfang.
pub const MSG_BAD_MAGIC: &str = "lxpd: missing LXPD magic";
/// Zähler widersprechen einander oder dem Puffer.
pub const MSG_BAD_COUNTS: &str = "lxpd: trampoline counts do not match the image";
/// Ein Stub ist korrupt (Magic, ID-Folge oder Padding).
pub const MSG_BAD_STUB: &str = "lxpd: corrupt trampoline stub";
/// Der `LXEND`-Marker fehlt oder steht falsch.
pub const MSG_BAD_END: &str = "lxpd: missing LXEND trailer";
/// Das JSON-Manifest ist strukturell unbrauchbar (kein Objekt, Felder fehlen/falsch,
/// Coverage != 100, Stubnamen passen nicht zum Container).
pub const MSG_BAD_MANIFEST: &str = "lxpd: unusable manifest";
/// Signatur fehlt oder stimmt nicht (niemals angenommen ohne Rechnung).
pub const MSG_BAD_SIGNATURE: &str = "lxpd: missing or wrong manifest signature";
/// BAR-/DMA-/IRQ-Zusagen fehlen oder sind null.
pub const MSG_BAD_GRANTS: &str = "lxpd: empty device grants";
/// Keine ELF-Magic — Pfad (b) greift nicht.
pub const MSG_NOT_ELF: &str = "lxpd: not an ELF image";
/// Image basis below the loadable window (see [`check_image_basis`]).
pub const MSG_BAD_BASIS: &str = "lxpd: image basis below the loadable window";

/// Parse-Fehler. Jeder Pfad, der eine fehlerhafte Eingabe erkennt, endet hier — **nie** in
/// einem Out-of-Bounds-Zugriff oder Panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LxpdError {
    /// Datenpuffer kürzer als die angekündigte Struktur.
    TooSmall,
    /// Keine `LXPD`-Magic am Anfang.
    BadMagic,
    /// Zähler unplausibel (über [`MAX_TRAMPS`], `section_size != count*32`, Überlänge)
    /// oder Stub-Index ausserhalb der Zahl.
    BadCounts,
    /// Korrupter Stub — trägt die beanstandete Stub-ID (bzw. den Index, falls die Magic
    /// schon fehlt und keine ID lesbar ist).
    BadStub(u32),
    /// `LXEND`-Trailer fehlt oder steht falsch.
    BadEnd,
    /// JSON-Manifest strukturell unbrauchbar.
    BadManifest,
    /// Signatur fehlt oder stimmt nicht.
    BadSignature,
    /// Geräte-Zusagen leer (kein BAR mit `size>0`, `dma_window.size==0`, `irq==0`).
    BadGrants,
    /// Keine ELF-Magic — kein ET_EXEC-Pfad.
    NotElf,
    /// A `PT_LOAD` segment or the entry point lies below the loadable
    /// window — carries the offending address.
    BadBasis { vaddr: u64 },
    /// Ein als ELF erkanntes Image scheitert an den Loader-Regeln (Magic/Klasse/Typ/
    /// Maschine/PHDR/`memsz<filesz`/Alignment) — die benannte Diagnose steht in der
    /// eingepackten Variante.
    Loader(LoaderError),
}

impl From<LoaderError> for LxpdError {
    fn from(e: LoaderError) -> Self {
        LxpdError::Loader(e)
    }
}

impl LxpdError {
    /// Fester Fehlertext (für die beiden parametrisierten Varianten s. [`core::fmt::Display`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            LxpdError::TooSmall => MSG_TOO_SMALL,
            LxpdError::BadMagic => MSG_BAD_MAGIC,
            LxpdError::BadCounts => MSG_BAD_COUNTS,
            LxpdError::BadStub(_) => MSG_BAD_STUB,
            LxpdError::BadEnd => MSG_BAD_END,
            LxpdError::BadManifest => MSG_BAD_MANIFEST,
            LxpdError::BadSignature => MSG_BAD_SIGNATURE,
            LxpdError::BadGrants => MSG_BAD_GRANTS,
            LxpdError::NotElf => MSG_NOT_ELF,
            LxpdError::BadBasis { .. } => MSG_BAD_BASIS,
            LxpdError::Loader(_) => "lxpd: loader rules reject the ELF image",
        }
    }
}

impl core::fmt::Display for LxpdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LxpdError::BadStub(id) => write!(f, "{}: stub {}", MSG_BAD_STUB, id),
            LxpdError::BadBasis { vaddr } => write!(f, "{MSG_BAD_BASIS}: {vaddr:#x}"),
            LxpdError::Loader(e) => write!(f, "lxpd: loader rules reject the ELF image: {:?}", e),
            _ => f.write_str(self.as_str()),
        }
    }
}

// --- Container-Kopf --------------------------------------------------------

/// Geparster Container-Kopf — Zähler ohne Speicher (kein `alloc`, kein Kopieren).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LxpdHeader {
    /// Zahl der 32-Byte-Trampolin-Stubs.
    pub tramp_count: u32,
    /// Zahl der umverdrahteten Einträge (Zähler ohne eigene Bytes im Layout).
    pub rewired_count: u32,
    /// Angekündigte Stubbereichsgrösse — muss `tramp_count * 32` sein.
    pub section_size: u64,
}

fn rd_u32(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn rd_u64(d: &[u8], off: usize) -> Option<u64> {
    let b = d.get(off..off + 8)?;
    Some(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Den Kopf zählen/validieren, ohne etwas zu speichern: Magic, Längen, Zählerkonsistenz.
/// Die Stub-Inhalte prüft [`LxpdImage::parse`].
pub fn parse_header(data: &[u8]) -> Result<LxpdHeader, LxpdError> {
    if data.len() < LXPD_MAGIC.len() {
        return Err(LxpdError::TooSmall);
    }
    if data.get(..LXPD_MAGIC.len()) != Some(&LXPD_MAGIC[..]) {
        return Err(LxpdError::BadMagic);
    }
    if data.len() < HEADER_LEN {
        return Err(LxpdError::TooSmall);
    }
    let tramp_count = rd_u32(data, 4).ok_or(LxpdError::TooSmall)?;
    let rewired_count = rd_u32(data, 8).ok_or(LxpdError::TooSmall)?;
    let section_size = rd_u64(data, 12).ok_or(LxpdError::TooSmall)?;
    if tramp_count as usize > MAX_TRAMPS {
        return Err(LxpdError::BadCounts);
    }
    let want_section =
        (tramp_count as u64).checked_mul(STUB_LEN as u64).ok_or(LxpdError::BadCounts)?;
    if section_size != want_section {
        return Err(LxpdError::BadCounts);
    }
    Ok(LxpdHeader { tramp_count, rewired_count, section_size })
}

// --- Trampolin-Sicht -------------------------------------------------------

/// Ein validierter Trampolin-Stub: ID plus die beiden FNV-1a-32-Hashes (Familie, Quelle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrampInfo {
    /// Stub-ID — muss der Position im Stubbereich entsprechen (`0..count`).
    pub id: u32,
    /// FNV-1a-32 der Trampolin-Familie.
    pub family_hash: u32,
    /// FNV-1a-32 des Quellsymbols.
    pub source_hash: u32,
}

/// Ein validierter LXPD-v1-Container: Kopf + Stubs + Trailer, alles bounds-geprüft.
/// Hält nur die Sicht auf die Eingabe — kopiert nichts.
#[derive(Clone, Copy, Debug)]
pub struct LxpdImage<'a> {
    data: &'a [u8],
    header: LxpdHeader,
}

impl<'a> LxpdImage<'a> {
    /// Vollständig validieren: Kopf, exakte Gesamtlänge (`20 + n*32 + 5`, kein Byte mehr
    /// oder weniger), je Stub Magic + ID-Folge + NOP-Padding, `LXEND`-Trailer.
    pub fn parse(data: &'a [u8]) -> Result<Self, LxpdError> {
        let header = parse_header(data)?;
        let n = header.tramp_count as usize;
        let stubs_len = n.checked_mul(STUB_LEN).ok_or(LxpdError::BadCounts)?;
        let total = HEADER_LEN
            .checked_add(stubs_len)
            .and_then(|t| t.checked_add(LXEND_MARKER.len()))
            .ok_or(LxpdError::BadCounts)?;
        if data.len() < total {
            return Err(LxpdError::TooSmall);
        }
        if data.len() > total {
            // Anhängsel hinter dem Trailer sind kein „Straffungsspielraum", sondern Bytes,
            // über die niemand etwas zusagt — fail-closed.
            return Err(LxpdError::BadCounts);
        }
        for i in 0..n {
            let base = HEADER_LEN + i * STUB_LEN; // kein Overflow: `i <= n <= 256`
            let stub = data.get(base..base + STUB_LEN).ok_or(LxpdError::TooSmall)?;
            if stub.get(..LXTR_MAGIC.len()) != Some(&LXTR_MAGIC[..]) {
                return Err(LxpdError::BadStub(i as u32));
            }
            let id = rd_u32(stub, 4).ok_or(LxpdError::TooSmall)?;
            if id as usize != i {
                return Err(LxpdError::BadStub(id));
            }
            if stub.get(16..STUB_LEN) != Some(&NOP_PAD[..]) {
                return Err(LxpdError::BadStub(id));
            }
        }
        let tail = HEADER_LEN + stubs_len;
        if data.get(tail..tail + LXEND_MARKER.len()) != Some(&LXEND_MARKER[..]) {
            return Err(LxpdError::BadEnd);
        }
        Ok(LxpdImage { data, header })
    }

    /// Geparster Kopf.
    pub fn header(&self) -> LxpdHeader {
        self.header
    }

    /// Zahl der Trampolin-Stubs.
    pub fn tramp_count(&self) -> usize {
        self.header.tramp_count as usize
    }

    /// Zahl der umverdrahteten Einträge (Zähler ohne eigene Bytes).
    pub fn rewired_count(&self) -> u32 {
        self.header.rewired_count
    }

    /// Stub `i` in O(1), bounds-geprüft (`i >= count` → [`LxpdError::BadCounts`]).
    pub fn trampoline(&self, i: usize) -> Result<TrampInfo, LxpdError> {
        if i >= self.tramp_count() {
            return Err(LxpdError::BadCounts);
        }
        let base = HEADER_LEN + i * STUB_LEN;
        let stub = self.data.get(base..base + STUB_LEN).ok_or(LxpdError::TooSmall)?;
        Ok(TrampInfo {
            id: rd_u32(stub, 4).ok_or(LxpdError::TooSmall)?,
            family_hash: rd_u32(stub, 8).ok_or(LxpdError::TooSmall)?,
            source_hash: rd_u32(stub, 12).ok_or(LxpdError::TooSmall)?,
        })
    }

    /// Manifest-Fakten + Stubnamen-Abgleich + echte Signaturprüfung. Der Container selbst
    /// enthält kein Manifest — die JSON-Bytes kommen separat als Datei herein.
    pub fn verify_manifest(
        &self,
        manifest_json: &[u8],
        key: &[u8],
    ) -> Result<ManifestFacts, LxpdError> {
        let facts = manifest::manifest_facts(manifest_json)?;
        if facts.trampoline_names != self.tramp_count() {
            return Err(LxpdError::BadManifest);
        }
        manifest::verify_signature(manifest_json, key)?;
        Ok(facts)
    }
}

/// Trägt `data` die LXPD-Magic (Pfad (a))?
pub fn is_lxpd(data: &[u8]) -> bool {
    data.get(..LXPD_MAGIC.len()) == Some(&LXPD_MAGIC[..])
}

/// Trägt `data` die ELF-Magic (Pfad (b))?
pub fn is_elf(data: &[u8]) -> bool {
    data.get(..ELF_MAGIC.len()) == Some(&ELF_MAGIC[..])
}

/// Pfad (b): ein reines ET_EXEC-ELF direkt an den Loader-Regeln messen
/// (Magic/64/LE/EXEC/Maschine/PHDR/`PT_LOAD`-Align/`memsz>=filesz`). Ohne ELF-Magic gibt
/// es kein „kaputtes ELF", sondern [`LxpdError::NotElf`] — ein Container gehört nicht hierher.
pub fn elf_ready(bytes: &[u8]) -> Result<(), LxpdError> {
    if !is_elf(bytes) {
        return Err(LxpdError::NotElf);
    }
    ElfImage::parse(bytes).map(|_| ()).map_err(LxpdError::Loader)
}

// --- Image basis (the edge before loading) -------------------------------------------------
//
// The kernel maps a `PT_LOAD` segment only above its user window: below it,
// `vspace_map_page_at` refuses, and the load path reports Code 10
// (`MANGEL_MAPPING_ABGEWIESEN`). Whoever notices a too-low-linked image only in
// the guest gets the same refusal without a name — hence this check here, at the
// edge, with the offending address in the error ([`LxpdError::BadBasis`]).
//
// Both floors are **read** from the HALs, not invented:
// * x86_64: `USER_RAM_MIN` = 16 MiB (`crates/caprock-hal/src/x86_64/mmu.rs`, FINE_BLOCKS
//   guard over the low 16 MiB). Real programs link at 0x20000000.
// * aarch64: `USER_RAM_MIN` = RAM_BASE + 2 MiB (`crates/caprock-hal/src/aarch64/mmu.rs`).
//   Real programs link at 0x41000000 (`programs/user.ld`).
//
// This is deliberately a mirror, not a second enforcement: the rule is enforced in the
// kernel (HAL guard, same on both architectures); this function lets tooling and tests
// fail early and by name. Existing paths (`elf_ready`) stay untouched — a new verdict
// changes no old one.

/// Lowest loadable image basis on x86_64 (16 MiB, see module docs).
pub const MIN_IMAGE_BASIS_X86: u64 = 0x100_0000;
/// Lowest loadable image basis on aarch64 (RAM_BASE + 2 MiB, see module docs).
pub const MIN_IMAGE_BASIS_ARM: u64 = 0x4020_0000;

/// Measure an ET_EXEC image against the basis rule: ELF shape ([`elf_ready`]) PLUS
/// entry point and every `PT_LOAD` segment at/above `min_basis`. Below that there
/// is no "almost loadable", only [`LxpdError::BadBasis`] with the address.
pub fn check_image_basis(bytes: &[u8], min_basis: u64) -> Result<(), LxpdError> {
    if !is_elf(bytes) {
        return Err(LxpdError::NotElf);
    }
    let img = ElfImage::parse(bytes).map_err(LxpdError::Loader)?;
    if img.entry() < min_basis {
        return Err(LxpdError::BadBasis { vaddr: img.entry() });
    }
    for seg in img.segments() {
        if seg.vaddr < min_basis {
            return Err(LxpdError::BadBasis { vaddr: seg.vaddr });
        }
    }
    Ok(())
}

// --- Host-Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use caprock_loader::elf::EXPECTED_MACHINE;

    fn fnv32(s: &str) -> u32 {
        let mut h: u32 = 0x811c_9dc5;
        for b in s.bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }

    /// Ein Stub exakt nach `lx-rewrite::stub_for`: `LXTR` + id + FNV32(family) +
    /// FNV32(source) + `0x90`-Padding.
    fn stub(id: u32, family: &str, source: &str) -> [u8; 32] {
        let mut s = [0x90u8; 32];
        s[0..4].copy_from_slice(b"LXTR");
        s[4..8].copy_from_slice(&id.to_le_bytes());
        s[8..12].copy_from_slice(&fnv32(family).to_le_bytes());
        s[12..16].copy_from_slice(&fnv32(source).to_le_bytes());
        s
    }

    fn container(n: u32, stubs: &[[u8; 32]]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"LXPD");
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // rewired_count
        v.extend_from_slice(&(stubs.len() as u64 * 32).to_le_bytes());
        for s in stubs {
            v.extend_from_slice(&s[..]);
        }
        v.extend_from_slice(b"LXEND");
        v
    }

    // --- Manifest-Bau für die End-to-End-Tests (Spiegel von `lx-bind` ohne serde) ---
    //
    // Die kanonischen Bytes (kompakt, Schlüssel sortiert, ohne `signature`) werden per Hand
    // gebaut, die Signatur ist echtes FNV-1a-64-Hex über Key-Bytes + Kanonik. Das
    // übergebene JSON ist danach bewusst hässlich (unsortiert, pretty) — der Prüfer muss
    // die Kanonik selbst wiederherstellen.

    fn fnv64(data: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce4842225c5;
        for b in data {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    fn hex_val(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => 0,
        }
    }

    /// Spiegel von `lx-bind::key_bytes`: gerader Hex-String → dekodiert, sonst roh.
    fn key_bytes_mirror(key: &str) -> Vec<u8> {
        let s = key.trim();
        let is_hex = s.len() % 2 == 0 && s.bytes().all(|c| c.is_ascii_hexdigit());
        if is_hex {
            let b = s.as_bytes();
            let mut out = Vec::with_capacity(b.len() / 2);
            let mut i = 0;
            while i < b.len() {
                out.push((hex_val(b[i]) << 4) | hex_val(b[i + 1]));
                i += 2;
            }
            out
        } else {
            key.as_bytes().to_vec()
        }
    }

    fn sign(canonical: &[u8], key: &str) -> String {
        let mut input = key_bytes_mirror(key);
        input.extend_from_slice(canonical);
        format!("{:016x}", fnv64(&input))
    }

    const CANON: &[u8] = b"{\"api_version\":\"X1\",\"arch\":\"x86-64\",\"class_b_objects\":[],\"coverage_pct\":100.0,\"dma_window\":{\"base\":0,\"size\":65536,\"bits\":64},\"driver\":\"e1000e\",\"gpl_affected\":false,\"grants_bar\":[{\"index\":0,\"base\":4096,\"size\":8192,\"flags\":\"RW\"}],\"irq_vector\":7,\"schema_version\":1,\"trampolines\":[\"dma_map_single->caprock_dma_map\",\"spin_lock->caprock_spin_lock\"]}";

    fn manifest_json(sig: &str) -> Vec<u8> {
        // Absichtlich unsortiert + pretty: der Prüfer muss selbst kanonisieren.
        format!(
            "{{\n  \"signature\": \"{sig}\",\n  \"driver\": \"e1000e\",\n  \"trampolines\": [\n    \"dma_map_single->caprock_dma_map\",\n    \"spin_lock->caprock_spin_lock\"\n  ],\n  \"schema_version\": 1,\n  \"api_version\": \"X1\",\n  \"arch\": \"x86-64\",\n  \"grants_bar\": [ {{ \"index\": 0, \"base\": 4096, \"size\": 8192, \"flags\": \"RW\" }} ],\n  \"dma_window\": {{ \"base\": 0, \"size\": 65536, \"bits\": 64 }},\n  \"irq_vector\": 7,\n  \"class_b_objects\": [],\n  \"gpl_affected\": false,\n  \"coverage_pct\": 100.0\n}}"
        )
        .into_bytes()
    }

    fn good_manifest() -> Vec<u8> {
        manifest_json(&sign(CANON, "deadbeef"))
    }

    fn good_image() -> Vec<u8> {
        container(2, &[stub(0, "dma", "dma_map_single"), stub(1, "spin", "spin_lock")])
    }

    #[test]
    fn container_roundtrip_ok() {
        let raw = good_image();
        let img = LxpdImage::parse(&raw).unwrap();
        assert_eq!(img.tramp_count(), 2);
        assert_eq!(img.rewired_count(), 0);
        assert_eq!(img.header().section_size, 64);
        let t0 = img.trampoline(0).unwrap();
        assert_eq!(t0.id, 0);
        assert_eq!(t0.family_hash, fnv32("dma"));
        assert_eq!(t0.source_hash, fnv32("dma_map_single"));
        let t1 = img.trampoline(1).unwrap();
        assert_eq!((t1.id, t1.family_hash, t1.source_hash), (1, fnv32("spin"), fnv32("spin_lock")));
        assert_eq!(img.trampoline(2).unwrap_err(), LxpdError::BadCounts);
        assert!(is_lxpd(&raw));
        assert!(!is_elf(&raw));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut raw = good_image();
        raw[0] = b'X';
        assert_eq!(LxpdImage::parse(&raw).unwrap_err(), LxpdError::BadMagic);
        assert_eq!(parse_header(&[]).unwrap_err(), LxpdError::TooSmall);
        assert_eq!(parse_header(b"LXP").unwrap_err(), LxpdError::TooSmall);
        assert!(!is_lxpd(&raw));
    }

    #[test]
    fn bad_counts_rejected() {
        let raw = good_image();
        // section_size lügt (63 statt 64).
        let mut a = raw.clone();
        a[12..20].copy_from_slice(&63u64.to_le_bytes());
        assert_eq!(LxpdImage::parse(&a).unwrap_err(), LxpdError::BadCounts);
        // Zähler lügt (3 angekundigt + Sektion passend dazu, 2 vorhanden) → zu kurz.
        let mut b = raw.clone();
        b[4..8].copy_from_slice(&3u32.to_le_bytes());
        b[12..20].copy_from_slice(&96u64.to_le_bytes());
        assert_eq!(LxpdImage::parse(&b).unwrap_err(), LxpdError::TooSmall);
        // Anhängsel hinter dem Trailer → zu lang.
        let mut c = raw.clone();
        c.push(0x00);
        assert_eq!(LxpdImage::parse(&c).unwrap_err(), LxpdError::BadCounts);
        // Absurde Zahl über der Schranke.
        let mut d = raw.clone();
        d[4..8].copy_from_slice(&10_000u32.to_le_bytes());
        assert_eq!(LxpdImage::parse(&d).unwrap_err(), LxpdError::BadCounts);
        // Abgeschnittener Kopf.
        assert_eq!(LxpdImage::parse(&raw[..10]).unwrap_err(), LxpdError::TooSmall);
    }

    #[test]
    fn bad_stub_magic_rejected() {
        let mut raw = good_image();
        raw[20 + 32] = b'X'; // zweiter Stub, Magic kaputt
        assert_eq!(LxpdImage::parse(&raw).unwrap_err(), LxpdError::BadStub(1));
    }

    #[test]
    fn bad_stub_id_sequence_rejected() {
        // IDs vertauscht: (1, 0) statt (0, 1).
        let raw = container(2, &[stub(1, "dma", "dma_map_single"), stub(0, "spin", "spin_lock")]);
        assert_eq!(LxpdImage::parse(&raw).unwrap_err(), LxpdError::BadStub(1));
        // Padding angenagt.
        let mut raw2 = good_image();
        raw2[20 + 16] = 0x00;
        assert_eq!(LxpdImage::parse(&raw2).unwrap_err(), LxpdError::BadStub(0));
    }

    #[test]
    fn bad_end_rejected() {
        let mut raw = good_image();
        let n = raw.len();
        raw[n - 5..n].copy_from_slice(b"LXENX");
        assert_eq!(LxpdImage::parse(&raw).unwrap_err(), LxpdError::BadEnd);
    }

    #[test]
    fn manifest_end_to_end_ok() {
        let raw = good_image();
        let img = LxpdImage::parse(&raw).unwrap();
        let facts = img.verify_manifest(&good_manifest(), b"deadbeef").unwrap();
        assert_eq!(facts.schema_version, 1);
        assert_eq!(facts.trampoline_names, 2);
        assert_eq!(facts.coverage_pct, 100.0);
        assert_eq!(facts.bars, 1);
        assert_eq!(facts.dma_size, 65536);
        assert_eq!(facts.irq, 7);
        assert!(facts.signature_present);
    }

    #[test]
    fn manifest_count_mismatch_rejected() {
        let raw = good_image();
        let img = LxpdImage::parse(&raw).unwrap();
        // Nur EIN Stubname bei ZWEI Stubs — Kanonik dazu per Hand, echt signiert.
        let canon_one: &[u8] = b"{\"coverage_pct\":100.0,\"dma_window\":{\"size\":1},\"driver\":\"e1000e\",\"grants_bar\":[{\"size\":8}],\"irq_vector\":7,\"schema_version\":1,\"trampolines\":[\"a->b\"]}";
        let small: Vec<u8> = format!(
            "{{\"schema_version\":1,\"driver\":\"e1000e\",\"coverage_pct\":100.0,\"grants_bar\":[{{\"size\":8}}],\"dma_window\":{{\"size\":1}},\"irq_vector\":7,\"trampolines\":[\"a->b\"],\"signature\":\"{}\"}}",
            sign(canon_one, "deadbeef")
        )
        .into_bytes();
        // Für sich genommen gültig + signiert — aber die Zahl passt nicht zum Container.
        assert_eq!(
            img.verify_manifest(&small, b"deadbeef").unwrap_err(),
            LxpdError::BadManifest
        );
    }

    // --- Minimal-EXEC nach den Loader-Regeln bauen ---

    fn build_exec(etype: u16, machine: u16, vaddr: u64, filesz: usize, memsz: u64) -> Vec<u8> {
        let phoff = 64usize;
        let payload_off = (phoff + 56) as u64;
        let mut v = vec![0u8; (payload_off as usize) + filesz];
        v[0..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        v[4] = 2; // 64-bit
        v[5] = 1; // LE
        v[6] = 1; // Version
        v[16..18].copy_from_slice(&etype.to_le_bytes());
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v[24..32].copy_from_slice(&vaddr.to_le_bytes()); // entry = segment start (MinELF shape)
        v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[54..56].copy_from_slice(&56u16.to_le_bytes());
        v[56..58].copy_from_slice(&1u16.to_le_bytes());
        let b = phoff;
        v[b..b + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        v[b + 4..b + 8].copy_from_slice(&5u32.to_le_bytes()); // R+X
        v[b + 8..b + 16].copy_from_slice(&payload_off.to_le_bytes());
        v[b + 16..b + 24].copy_from_slice(&vaddr.to_le_bytes());
        v[b + 24..b + 32].copy_from_slice(&vaddr.to_le_bytes());
        v[b + 32..b + 40].copy_from_slice(&(filesz as u64).to_le_bytes());
        v[b + 40..b + 48].copy_from_slice(&memsz.to_le_bytes());
        v[b + 48..b + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        v
    }

    #[test]
    fn elf_ready_accepts_minimal_exec() {
        let raw = build_exec(2, EXPECTED_MACHINE, 0x1000, 8, 8);
        assert!(is_elf(&raw));
        assert_eq!(elf_ready(&raw), Ok(()));
    }

    #[test]
    fn elf_ready_rejects_rel() {
        let raw = build_exec(1, EXPECTED_MACHINE, 0x1000, 8, 8); // ET_REL
        assert_eq!(elf_ready(&raw).unwrap_err(), LxpdError::Loader(LoaderError::BadElf));
    }

    #[test]
    fn elf_ready_rejects_unaligned_load() {
        let raw = build_exec(2, EXPECTED_MACHINE, 0x1700, 8, 8);
        assert_eq!(
            elf_ready(&raw).unwrap_err(),
            LxpdError::Loader(LoaderError::UnalignedSegment { vaddr: 0x1700 })
        );
    }

    #[test]
    fn elf_ready_rejects_memsz_less_than_filesz() {
        let raw = build_exec(2, EXPECTED_MACHINE, 0x1000, 8, 4);
        assert_eq!(elf_ready(&raw).unwrap_err(), LxpdError::Loader(LoaderError::BadElf));
    }

    #[test]
    fn elf_ready_rejects_non_elf() {
        let raw = good_image();
        assert_eq!(elf_ready(&raw).unwrap_err(), LxpdError::NotElf);
        assert_eq!(elf_ready(b"").unwrap_err(), LxpdError::NotElf);
    }

    // --- Image basis: the edge before loading ------------------------------------

    #[test]
    fn basis_x86_rejects_low_link() {
        // The lxport test image (0x100000/0x200000, low 16 MiB) is rightly refused by
        // the kernel (Code 10) — here the same refusal falls, only earlier and addressed.
        for low in [0x1000u64, 0x100_000, 0x200_000, 0xFF_F000] {
            let raw = build_exec(2, EXPECTED_MACHINE, low, 8, 8);
            assert_eq!(
                check_image_basis(&raw, MIN_IMAGE_BASIS_X86).unwrap_err(),
                LxpdError::BadBasis { vaddr: low },
                "basis {low:#x} must fall"
            );
        }
    }

    #[test]
    fn basis_x86_accepts_at_and_above_floor() {
        // The other direction, without which the test above would prove nothing: exactly
        // the threshold and above must pass — otherwise "is refused" would also hold for
        // a checker that accepts nothing at all.
        for ok in [MIN_IMAGE_BASIS_X86, 0x2000_0000] {
            let raw = build_exec(2, EXPECTED_MACHINE, ok, 8, 8);
            assert_eq!(check_image_basis(&raw, MIN_IMAGE_BASIS_X86), Ok(()));
        }
    }

    #[test]
    fn basis_arm_floor_differs_from_x86() {
        // On ARM the user window sits higher (RAM_BASE + 2 MiB): what would load on
        // x86 (0x20000000 — where real programs link there) falls here.
        let x86_ok = build_exec(2, EXPECTED_MACHINE, 0x2000_0000, 8, 8);
        assert_eq!(check_image_basis(&x86_ok, MIN_IMAGE_BASIS_X86), Ok(()));
        assert_eq!(
            check_image_basis(&x86_ok, MIN_IMAGE_BASIS_ARM).unwrap_err(),
            LxpdError::BadBasis { vaddr: 0x2000_0000 }
        );
        // The canonical ARM basis (programs/user.ld) and the exact floor pass.
        for ok in [MIN_IMAGE_BASIS_ARM, 0x4100_0000] {
            let raw = build_exec(2, EXPECTED_MACHINE, ok, 8, 8);
            assert_eq!(check_image_basis(&raw, MIN_IMAGE_BASIS_ARM), Ok(()));
        }
    }

    #[test]
    fn basis_names_the_offending_address() {
        // The refusal names the address — "linked too low" without an address is
        // worthless as a diagnosis (same principle as UnalignedSegment).
        let raw = build_exec(2, EXPECTED_MACHINE, 0x100_000, 8, 8);
        let err = check_image_basis(&raw, MIN_IMAGE_BASIS_X86).unwrap_err();
        assert_eq!(err.as_str(), MSG_BAD_BASIS);
        let text = alloc_format(err);
        assert!(text.contains("0x100000"), "Display must name the address: {}", text.as_str());
        // ... and it is distinguishable from the other refusals.
        assert_ne!(err, LxpdError::BadManifest);
        assert_ne!(err, LxpdError::NotElf);
        assert_ne!(err, LxpdError::Loader(LoaderError::BadElf));
    }

    #[test]
    fn basis_checks_entry_and_every_segment() {
        // Entry ok, single segment too low: a checker looking only at the entry
        // would wave through an unloadable image.
        let mut entry_high = build_exec(2, EXPECTED_MACHINE, 0x200_000, 8, 8);
        entry_high[24..32].copy_from_slice(&MIN_IMAGE_BASIS_X86.to_le_bytes());
        assert_eq!(
            check_image_basis(&entry_high, MIN_IMAGE_BASIS_X86).unwrap_err(),
            LxpdError::BadBasis { vaddr: 0x200_000 }
        );
    }

    #[test]
    fn basis_passes_loader_errors_through() {
        // A broken ELF stays a loader error, not a basis error — a new verdict
        // changes no old one.
        let rel = build_exec(1, EXPECTED_MACHINE, MIN_IMAGE_BASIS_X86, 8, 8);
        assert_eq!(
            check_image_basis(&rel, MIN_IMAGE_BASIS_X86).unwrap_err(),
            LxpdError::Loader(LoaderError::BadElf)
        );
        assert_eq!(
            check_image_basis(&good_image(), MIN_IMAGE_BASIS_X86).unwrap_err(),
            LxpdError::NotElf
        );
    }

    /// `Display` into a fixed buffer without `alloc` (the crate is `no_std` outside tests).
    fn alloc_format(e: LxpdError) -> ArrayString {
        use core::fmt::Write as _;
        let mut s = ArrayString::new();
        let _ = write!(s, "{e}");
        s
    }

    struct ArrayString {
        buf: [u8; 96],
        len: usize,
    }

    impl ArrayString {
        fn new() -> Self {
            ArrayString { buf: [0u8; 96], len: 0 }
        }
        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<kein-utf8>")
        }
        fn contains(&self, needle: &str) -> bool {
            self.buf[..self.len].windows(needle.len()).any(|w| w == needle.as_bytes())
        }
    }

    impl core::fmt::Write for ArrayString {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                *self.buf.get_mut(self.len).ok_or(core::fmt::Error)? = b;
                self.len += 1;
            }
            Ok(())
        }
    }
}
