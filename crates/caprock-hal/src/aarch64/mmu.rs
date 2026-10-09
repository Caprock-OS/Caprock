//! MMU: eine einzige, statische Identity-Map mit aktivierten Caches und **W^X**
//! (ADR 0002).
//!
//! Es gibt **keine** per-Prozess-Adressräume und **keine** Adressübersetzung:
//! virtuell == physisch. Die MMU dient (a) Caches/Speculation (Performance) und
//! (b) groben Schutzattributen. Isolation kommt aus Rust + Capabilities, nicht
//! aus per-Prozess-Page-Tables.
//!
//! Layout (4-KiB-Granule, 39-bit VA, Top-Level = L1):
//! ```text
//!  L1[0]      0..1 GiB   Device-nGnRnE, XN         (MMIO: UART, GIC, …)
//!  L1[1]      1..2 GiB   -> L2  (enthält das Kernelimage; feinere Rechte)
//!  L1[2..=8]  2..9 GiB   Normal WB cacheable, RW + XN   (freies RAM)
//!
//!  L2[0]      +0..2 MiB  -> L3  (Kernel-Sektionen, 4-KiB-Seiten)
//!  L2[1..]    +2 MiB..   Normal WB cacheable, RW + XN   (freies RAM in GiB 1)
//!
//!  L3[i]      4-KiB-Seite: .text = R-X, .rodata = R--, sonst RW + XN
//! ```
//!
//! Sämtliches `unsafe` hier ist MMU-/Registerinitialisierung — erlaubte Domäne.

use super::cpu;
use super::guard::{ADDR_MASK, ONE_GIB, PAGE, RAM_BASE, TWO_MIB};
/// Re-exported from the host-testable [`super::guard`] module (single source
/// for the guard arithmetic); the range comments live there.
pub use super::guard::{GIB1_END, USER_RAM_MIN};
use core::arch::asm;
use core::cell::UnsafeCell;

// Vom Linker bereitgestellte Sektionsgrenzen (4-KiB-ausgerichtet).
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __user_text_start: u8;
    static __user_text_end: u8;
    static __kernel_end: u8;
}

/// 4-KiB-ausgerichtete Übersetzungstabelle (512 × 64-bit Deskriptoren).
#[repr(C, align(4096))]
struct PageTable([u64; 512]);

impl PageTable {
    const EMPTY: PageTable = PageTable([0; 512]);
}

/// Sync-Wrapper, damit Tabellen `static` sein können. Schreibzugriff erfolgt nur
/// einmalig durch den Primärkern (vor SMP-Start); danach effektiv read-only.
struct TableStore(UnsafeCell<PageTable>);
// SAFETY: siehe oben — einmaliger, alleiniger Schreibzugriff vor SMP.
unsafe impl Sync for TableStore {}

static L1_TABLE: TableStore = TableStore(UnsafeCell::new(PageTable([0; 512])));
static L2_TABLE: TableStore = TableStore(UnsafeCell::new(PageTable([0; 512])));
static L3_TABLE: TableStore = TableStore(UnsafeCell::new(PageTable([0; 512])));

// --- Deskriptor-Bits (ARMv8-A, Stage-1) ---
const TABLE_DESC: u64 = 0b11; // Tabellen-Deskriptor (zeigt auf nächste Ebene)
const BLOCK_DESC: u64 = 0b01; // Block-Deskriptor (L1: 1 GiB, L2: 2 MiB)
const PAGE_DESC: u64 = 0b11; //  Seiten-Deskriptor (L3: 4 KiB)
const AF: u64 = 1 << 10; //      Access Flag
const SH_INNER: u64 = 0b11 << 8; // Inner Shareable (für Normal-Memory)
const AP_RW: u64 = 0b00 << 6; //  RW, nur EL1
const AP_RO: u64 = 0b10 << 6; //  RO, nur EL1
const AP_RW_EL0: u64 = 0b01 << 6; // RW, EL0 + EL1 (User-Daten/Stacks)
const AP_RO_EL0: u64 = 0b11 << 6; // RO, EL0 + EL1 (User-Code)
const ATTR_DEVICE: u64 = 0 << 2; //    MAIR-Index 0 (Device-nGnRnE)
const ATTR_NORMAL: u64 = 1 << 2; //    MAIR-Index 1 (Normal WB cacheable)
const ATTR_NORMAL_NC: u64 = 2 << 2; // MAIR-Index 2 (Normal Non-Cacheable, ext-23 DMA-Puffer)
const NG: u64 = 1 << 11; //       non-global: Eintrag ist ASID-spezifisch
const PXN: u64 = 1 << 53; //      Privileged Execute Never
const UXN: u64 = 1 << 54; //      Unprivileged Execute Never

/// Zugriffsrecht einer gemappten User-Seite (4 KiB) in einer isolierten VSpace.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UserPerm {
    /// EL0 Read+Write, nicht ausführbar (Daten/Stack).
    Rw,
    /// EL0 Read+Execute, read-only (Code, W^X).
    Rx,
    /// EL0 Read-only (Konstanten/rodata).
    Ro,
}

/// Zugriffsrecht eines Normal-Memory-Leafs.
#[derive(Clone, Copy)]
enum Perm {
    /// Kernel Read + Execute (Code, nur EL1).
    Rx,
    /// Kernel Read only (Konstanten, nur EL1).
    Ro,
    /// Kernel Read + Write, niemals ausführbar (Kernel-Daten/Stacks, nur EL1).
    Rw,
    /// User Read + Write, niemals ausführbar (User-Daten/Stacks, EL0+EL1).
    UserRw,
    /// User Read + Execute (User-Code; EL0 ausführbar, EL1 nicht (PXN)).
    UserRx,
}

/// Device-Block (nicht-cacheable, XN, RW). Für die MMIO-1-GiB-Region.
const DEVICE_BLOCK: u64 = BLOCK_DESC | AF | AP_RW | ATTR_DEVICE | PXN | UXN;

/// Normal-Memory-Leaf (Block oder Page) mit gegebenem Recht erzeugen.
///
/// User-**Daten** (`UserRw`) sind `nG` (ASID-spezifisch): die globale SAS-Map nutzt
/// ASID 0, isolierte VSpaces eigene ASIDs — so leckt freies User-RAM der SAS-Map
/// nicht per global-getaggtem TLB-Eintrag in eine isolierte VSpace. User-**Code**
/// (`UserRx`, `.user_text`) bleibt global (nG=0): er wird read-only von allen
/// VSpaces geteilt. Kernel/Device sind ebenfalls global (in jeder VSpace gemappt).
fn normal_leaf(addr: u64, perm: Perm, page: bool) -> u64 {
    let kind = if page { PAGE_DESC } else { BLOCK_DESC };
    let perm_bits = match perm {
        Perm::Rx => AP_RO, //                   ausführbar bei EL1 (PXN=UXN=0)
        Perm::Ro => AP_RO | PXN | UXN,
        Perm::Rw => AP_RW | PXN | UXN,
        Perm::UserRw => AP_RW_EL0 | PXN | UXN | NG, // EL0+EL1 RW, kein Code, ASID-spezifisch
        Perm::UserRx => AP_RO_EL0 | PXN, //      EL0 ausführbar (UXN=0), EL1 nicht (PXN), global
    };
    addr | AF | SH_INNER | ATTR_NORMAL | perm_bits | kind
}

/// Tabellen-Deskriptor auf die nächste Ebene.
fn table_desc(next_table_phys: u64) -> u64 {
    next_table_phys | TABLE_DESC
}

fn sym(addr_of: &u8) -> u64 {
    addr_of as *const u8 as u64
}

/// Recht für die 4-KiB-Seite, die bei `addr` beginnt (Kernel-Sektionen).
fn perm_for_page(addr: u64) -> Perm {
    // SAFETY: nur Adressberechnung über Linker-Symbole, kein Speicherzugriff.
    let (ts, te, rs, re, us, ue, ke) = unsafe {
        (
            sym(&__text_start),
            sym(&__text_end),
            sym(&__rodata_start),
            sym(&__rodata_end),
            sym(&__user_text_start),
            sym(&__user_text_end),
            sym(&__kernel_end),
        )
    };
    if addr >= ts && addr < te {
        Perm::Rx
    } else if addr >= rs && addr < re {
        Perm::Ro
    } else if addr >= us && addr < ue {
        Perm::UserRx // EL0-ausführbarer User-Code (global, von allen VSpaces geteilt)
    } else if addr >= ke {
        // Freies RAM in den ersten 2 MiB: **EL1-only**. Diese L3 wird von jeder
        // isolierten VSpace geteilt (Kernelimage + .user_text); sie darf daher kein
        // EL0-zugängliches freies RAM enthalten. User-RAM wird erst ab 2 MiB
        // alloziert (siehe init_mem-Aufrunden).
        Perm::Rw
    } else {
        // Kernel-Daten/BSS/Stacks (EL1-only).
        Perm::Rw
    }
}

fn table_mut(store: &TableStore) -> &mut [u64; 512] {
    // SAFETY: einmaliger, alleiniger Schreibzugriff durch den Primärkern vor SMP.
    unsafe { &mut (*store.0.get()).0 }
}

fn table_phys(store: &TableStore) -> u64 {
    store.0.get() as u64
}

/// Identity-Map mit W^X in die statischen Tabellen schreiben (nur Primärkern).
fn build_tables() {
    // L3: erste 2 MiB des RAM (Kernelimage) seitenweise mit Sektionsrechten.
    let l3 = table_mut(&L3_TABLE);
    for (i, entry) in l3.iter_mut().enumerate() {
        let addr = RAM_BASE + i as u64 * PAGE;
        *entry = normal_leaf(addr, perm_for_page(addr), true);
    }

    // L2: [0] -> L3 ; [1..] = restliches GiB 1 als 2-MiB-Blöcke, EL0-zugänglich
    // (freies RAM für User-Stacks/-Daten).
    let l2 = table_mut(&L2_TABLE);
    l2[0] = table_desc(table_phys(&L3_TABLE));
    for (i, entry) in l2.iter_mut().enumerate().skip(1) {
        let addr = RAM_BASE + i as u64 * TWO_MIB;
        *entry = normal_leaf(addr, Perm::UserRw, false);
    }

    // L1: [0] Device ; [1] -> L2 ; [2..=8] freies RAM (EL0-zugänglich).
    let l1 = table_mut(&L1_TABLE);
    l1[0] = DEVICE_BLOCK; // 0..1 GiB (MMIO)
    l1[1] = table_desc(table_phys(&L2_TABLE)); // 1..2 GiB (Kernel-GiB)
    for i in 2..=8u64 {
        l1[i as usize] = normal_leaf(i * ONE_GIB, Perm::UserRw, false);
    }

    // Sicherheitsnetz: das Kernelimage muss in die ersten 2 MiB passen, sonst
    // läge Code in einem nur-RW-Block (nicht ausführbar). Bisher ~1 MiB.
    // SAFETY: reine Adressberechnung.
    let kernel_end = unsafe { sym(&__kernel_end) };
    if kernel_end > RAM_BASE + TWO_MIB {
        super::console::emit_raw("[mmu] WARNUNG: Kernelimage > 2 MiB, W^X-Mapping unvollständig!\n");
    }
}

// MAIR: attr0 = Device-nGnRnE (0x00), attr1 = Normal WB R/W-allocate (0xFF),
// attr2 = Normal Non-Cacheable (0x44, inner+outer NC) für ext-23-DMA-Puffer.
const MAIR_VALUE: u64 = 0x00 | (0xFF << 8) | (0x44 << 16);

// TCR_EL1: T0SZ=25 (39-bit VA), 4-KiB-Granule, WB+inner-shareable Walks,
// TTBR1 deaktiviert, IPS=40-bit.
const TCR_VALUE: u64 = 25            // T0SZ
    | (0b01 << 8)                    // IRGN0 = WB
    | (0b01 << 10)                   // ORGN0 = WB
    | (0b11 << 12)                   // SH0 = inner shareable
    | (0b00 << 14)                   // TG0 = 4 KiB
    | (1 << 23)                      // EPD1 = TTBR1 aus
    | (0b010 << 32); //                IPS = 40-bit

const SCTLR_M: u64 = 1 << 0; //   MMU an
const SCTLR_C: u64 = 1 << 2; //   Data-Cache an
const SCTLR_I: u64 = 1 << 12; //  Instruction-Cache an

/// Größte nutzbare ASID (= max. Anzahl gleichzeitiger isolierter VSpaces). Default 255 (8-Bit,
/// TCR.AS=0); `enable()` hebt auf 65535, wenn die HW 16-Bit-ASIDs meldet (FEAT_ASID16,
/// `ID_AA64MMFR0_EL1.ASIDBits == 0b0010`) und setzt dann TCR.AS. Der VSpace-Allokator deckelt
/// seine Vergabe hierauf, damit ASIDs NIE über die HW-Breite hinaus vergeben werden (sonst würde
/// eine zu große ASID auf eine andere aliasen — Isolationsbruch).
static MAX_ASID: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(255);

/// Größte HW-nutzbare ASID (255 oder 65535). Erst nach [`init_primary`] gültig.
pub fn max_asid() -> u16 {
    MAX_ASID.load(core::sync::atomic::Ordering::Relaxed)
}

/// MMU + Caches am aktuellen Kern aktivieren (Tabellen müssen bereits stehen).
fn enable() {
    let ttbr0 = table_phys(&L1_TABLE);
    // 16-Bit-ASIDs (FEAT_ASID16) runtime erkennen: ID_AA64MMFR0_EL1.ASIDBits (Bits [7:4]) == 0b0010.
    // Dann TCR.AS (Bit 36) setzen -> 65535 statt 255 ASIDs. Läuft auf jedem Kern (gleiche HW ->
    // gleicher Wert); der primäre Kern hebt MAX_ASID. Ist FEAT_ASID16 nicht da, ist AS RES0 (bleibt
    // 8-Bit) und MAX_ASID bleibt 255 -> sicher.
    // SAFETY: nur ein Systemregister-Read.
    let asid16 = unsafe {
        let mmfr0: u64;
        asm!("mrs {}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack, preserves_flags));
        ((mmfr0 >> 4) & 0xf) == 0b0010
    };
    let tcr = if asid16 {
        MAX_ASID.store(65535, core::sync::atomic::Ordering::Relaxed);
        TCR_VALUE | (1u64 << 36) // AS = 1 -> 16-Bit-ASIDs
    } else {
        TCR_VALUE
    };
    // SAFETY: vollständige MMU-Aktivierungssequenz (Registerinit + Barrieren).
    // Der ausgeführte Code liegt in Normal-RAM (.text, R-X), daher läuft die
    // Instruktionsausführung unterbrechungsfrei weiter (VA == PA).
    unsafe {
        asm!(
            "msr MAIR_EL1, {mair}",
            "msr TCR_EL1, {tcr}",
            "msr TTBR0_EL1, {ttbr0}",
            "dsb sy",
            "isb",
            "tlbi vmalle1",
            "ic  iallu",
            "dsb sy",
            "isb",
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) tcr,
            ttbr0 = in(reg) ttbr0,
            options(nostack, preserves_flags),
        );

        let mut sctlr: u64;
        asm!("mrs {}, SCTLR_EL1", out(reg) sctlr, options(nomem, nostack, preserves_flags));
        sctlr |= SCTLR_M | SCTLR_C | SCTLR_I;
        asm!("msr SCTLR_EL1, {}", in(reg) sctlr, options(nostack, preserves_flags));
        asm!("isb", options(nomem, nostack, preserves_flags));
    }
    cpu::dsb_sy();
}

/// Primärkern: Tabellen bauen + MMU aktivieren.
pub fn init_primary() {
    record_cache_granule();
    build_tables();
    enable();
}

/// Sekundärkern: dieselbe (bereits gebaute) Tabelle aktivieren.
pub fn init_secondary() {
    record_cache_granule();
    enable();
}

/// (M, C, I)-Bits aus `SCTLR_EL1` zur Verifikation.
pub fn sctlr_flags() -> (bool, bool, bool) {
    let sctlr: u64;
    // SAFETY: read-only Systemregister.
    unsafe {
        asm!("mrs {}, SCTLR_EL1", out(reg) sctlr, options(nomem, nostack, preserves_flags));
    }
    (
        sctlr & SCTLR_M != 0,
        sctlr & SCTLR_C != 0,
        sctlr & SCTLR_I != 0,
    )
}

/// Physische Endadresse des Kernelimages (= erste freie RAM-Adresse), 4-KiB-aligned.
pub fn kernel_end() -> u64 {
    // SAFETY: reine Adressberechnung über ein Linker-Symbol.
    unsafe { sym(&__kernel_end) }
}

/// **Obergrenze der bevorzugten Allokationszone** (E-Rest 3b).
///
/// Auf x86 ist das die Grenze des fest abgebildeten Bereichs (4 GiB): darüber wächst die Karte
/// nur gezielt, und mehrere Pfade brauchen Speicher, den sie identisch abbilden können. Der
/// aarch64-Zweig hat diese Zweiteilung **nicht** — es gibt keinen Bereich oberhalb einer
/// Kartengrenze, in den der Allokator ausweichen könnte.
///
/// Deshalb steht hier `u64::MAX` und nicht etwa [`GIB1_END`]: eine Vorgabe, die es auf dieser
/// Architektur gar nicht gibt, als Zahl zu erfinden hiesse, dem Allokator eine Einschränkung
/// unterzuschieben, die keine Ursache hat. Die *echte* GiB-0-Bedingung für PD-private Regionen
/// steht weiterhin bei ihren Aufrufern (`system::gib0_zone`), auf beiden Architekturen gleich.
pub const LOW_MAPPED_END: u64 = u64::MAX;

/// **Die Blockgranularität der Identitätskarte** (2 MiB) — ein L2-Blockdeskriptor.
///
/// Begründung wortgleich zur x86-Seite: die Konstante trug bis C7b zwei Bedeutungen (Blockgrösse
/// **und** private Regionsgrösse), die nur solange dieselbe Zahl waren, wie die Region genau ein
/// Block war. Die private Region steht seither als [`PRIV_REGION_SIZE`] daneben.
pub const ISO_REGION_SIZE: u64 = TWO_MIB;

/// **Die private Region einer isolierten PD** — der EL0-Stack EINES Threads (C7b).
///
/// Die Zahl ist gemessen (`kernel/src/userstackmark.rs`, Prüfzeile `ustack`) und folgt der
/// Summenbedingung „tiefster gemessener Pfad + geforderte Reserve ≤ Regionsgrösse"; die volle
/// Begründung steht bei der x86-Fassung dieser Konstante.
///
/// **Was auf dieser Architektur NICHT gemessen ist, und das gehört dazu:** die Berichtszeile
/// `ustack` steht heute nur im x86-Hochlaufweg (dieselbe Einordnung wie `kstack` und `sperre`).
/// Die Zähler laufen hier mit, aber es gibt niemanden, der sie druckt oder gattert. Die Zahl
/// stammt also aus einer Messung auf x86 — und sie ist auf aarch64 **konservativer**, nicht
/// knapper: der User-Stack eines geladenen Programms ist dort dieselben 16 KiB, und die gefärbte
/// Region ist mit 16 KiB (16 Farben / 4 Partitionen) sogar kleiner als diese 64 KiB.
pub const PRIV_REGION_SIZE: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Per-Prozess-VSpaces (Weg C, Hybrid): die SAS-Map bleibt für vertrauenswürdige
// PDs; isolierte PDs erhalten eine eigene VSpace, die nur den Kernel (EL1-only,
// geteilt) + ihre eigene User-Region (EL0) mappt. Adressierung bleibt Identity.
// ---------------------------------------------------------------------------

/// Wurzel der globalen SAS-Map (Kernel + alles RAM EL0-zugänglich); ASID 0.
pub fn global_root() -> u64 {
    table_phys(&L1_TABLE)
}

/// `TTBR0_EL1` auf eine VSpace-Wurzel `root` mit `asid` setzen. Kein TLB-Flush:
/// Kernel/Device/Code sind global (nG=0, in jeder VSpace gültig), User-Daten sind
/// `nG` und ASID-getaggt — verschiedene ASIDs kollidieren nicht. Pro Kontextwechsel
/// vom Kernel aufzurufen.
pub fn set_user_vspace(root: u64, asid: u16) {
    let ttbr0 = ((asid as u64) << 48) | root;
    // SAFETY: Schreiben von TTBR0_EL1 (Adressraum-Wurzel) + isb. Kernel/Code sind in
    // jeder VSpace identisch (global) gemappt, daher läuft der Kernel unterbrechungs-
    // frei weiter. MMU-/Registerdomäne.
    unsafe {
        asm!("msr TTBR0_EL1, {}", "isb", in(reg) ttbr0, options(nostack, preserves_flags));
    }
}

/// EL1-only Normal-Block (Kernel sieht das RAM, EL0 nicht), global. Für die
/// „Rest-RAM"-Einträge einer isolierten VSpace (1-GiB- bzw. 2-MiB-Blöcke).
fn kernel_block(addr: u64) -> u64 {
    // nG (ASID-spezifisch): diese EL1-only-Blöcke gehören zur jeweiligen isolierten VSpace. Wären
    // sie global, überschattete ein in VSpace A gecachter EL1-only-Block dieselbe VA in VSpace B —
    // inkl. einer frisch als EL0 gesplitteten Seite -> Permission-Fault Level 2 (Block) auf realer
    // HW (QEMU-TCG maskiert das). Ein per-ASID-Flush kann globale Einträge zudem nicht evictieren.
    addr | AF | SH_INNER | ATTR_NORMAL | AP_RW | PXN | UXN | NG | BLOCK_DESC
}

/// User-RW 2-MiB-Block, `nG` (ASID-spezifisch): die eigene Region einer isolierten PD.
fn user_block(addr: u64) -> u64 {
    addr | AF | SH_INNER | ATTR_NORMAL | AP_RW_EL0 | PXN | UXN | NG | BLOCK_DESC
}

/// User-RX 2-MiB-Block, `nG`: privat geladener **Code** (EL0 read+execute, nicht
/// schreibbar -> W^X; an EL1 nicht ausführbar (PXN)).
fn user_code_block(addr: u64) -> u64 {
    addr | AF | SH_INNER | ATTR_NORMAL | AP_RO_EL0 | PXN | NG | BLOCK_DESC
}

/// Ist `desc` ein **gültiger** Deskriptor, der eine **EL0-zugängliche, schreibbare UND
/// ausführbare** Seite/Block beschreibt — also eine **W^X-Verletzung**?
/// EL0-Zugriff = Bit 6 gesetzt (AP_*_EL0); schreibbar = Bit 7 klar (AP[2]=0);
/// EL0-ausführbar = `UXN` (Bit 54) klar.
fn is_wx_violation(desc: u64) -> bool {
    if desc & 0b1 == 0 {
        return false; // ungültiger Eintrag
    }
    let el0 = (desc >> 6) & 1 == 1;
    let writable = (desc >> 7) & 1 == 0;
    let el0_exec = desc & UXN == 0;
    el0 && writable && el0_exec
}

/// **VMM-Property-Walker** (read-only, Fuzzer-Oracle): durchläuft die GiB-1-L2-Tabelle
/// `l2_phys` einer VSpace **und** alle daran hängenden L3-Tabellen und prüft die
/// **W^X-Invariante** (keine EL0-Seite ist gleichzeitig schreibbar und ausführbar)
/// sowie die Struktur (Tabellen-Deskriptoren zeigen 4-KiB-ausgerichtet in den
/// RAM-Bereich). Gibt `0` bei Konsistenz zurück, sonst `1` (W^X-Verletzung) bzw. `2`
/// (struktureller Defekt: L3-Zeiger außerhalb des RAM / unausgerichtet).
pub fn vspace_wx_ok(l2_phys: u64) -> u32 {
    let ram_end = RAM_BASE + (4u64 << 30); // 4 GiB RAM (großzügige Obergrenze)
    // SAFETY: `l2_phys` ist eine gültige, identity-gemappte L2-Tabelle (512 Einträge).
    let l2 = unsafe { core::slice::from_raw_parts(l2_phys as *const u64, 512) };
    for &e in l2.iter() {
        match e & 0b11 {
            0b01 => {
                // 2-MiB-Block direkt.
                if is_wx_violation(e) {
                    return 1;
                }
            }
            0b11 => {
                // Tabellen-Deskriptor -> L3. Zeiger validieren, dann Seiten prüfen.
                let l3_phys = e & ADDR_MASK;
                if l3_phys < RAM_BASE || l3_phys >= ram_end || l3_phys % PAGE != 0 {
                    return 2;
                }
                // SAFETY: validierter, identity-gemappter L3-Frame (512 Einträge).
                let l3 = unsafe { core::slice::from_raw_parts(l3_phys as *const u64, 512) };
                for &p in l3.iter() {
                    if is_wx_violation(p) {
                        return 1;
                    }
                }
            }
            _ => {} // 0b00 = ungültig (z. B. Guard-/ungemappte Seite) -> ok
        }
    }
    0
}

/// **Eine User-VA eines fremden Adressraums nach PA aufloesen** (Z6b) — s. die x86-Fassung fuer
/// die Begruendung, warum das im Kernel liegt und nicht im Debugger.
///
/// Auf aarch64 traegt `l2_phys` GiB 1 (dort liegen die identisch abgebildeten User-Frames), und
/// das User-Fenster haengt an `L1[9]` ([`ISO_USER_VA`]).
///
/// **Diese Funktion existiert von Tag eins auf BEIDEN Architekturen**, und das ist keine
/// Gruendlichkeit, sondern eine bezahlte Lehre: zweimal hat eine nur auf x86 vorhandene
/// HAL-Funktion in arch-neutralem Kernelcode den aarch64-Bau gerissen (`guard_unmap`,
/// `irq_tiefe`) — beim zweiten Mal, **nachdem** die Abnahme gegen genau diese Klasse gehaertet
/// worden war.
pub fn vspace_resolve(l1_phys: u64, l2_phys: u64, va: u64) -> Option<u64> {
    /// **Ist dieser Deskriptor fuer EL0 lesbar?** `AP[1]` (Bit 6) unterscheidet EL1-only von
    /// EL0+EL1. Ohne diese Frage loeste ein Debugger auch die **geteilten Kernel-Eintraege** der
    /// isolierten VSpace auf und laese Kernelspeicher — s. die x86-Fassung, wo dieselbe Luecke
    /// am 2026-08-20 gemessen wurde (`luecke-abgewiesen=false`).
    #[inline]
    fn el0_lesbar(desc: u64) -> bool {
        desc & (1 << 6) != 0
    }
    let gib1 = ONE_GIB;
    // SAFETY: gueltige, identity-gemappte Tabellen dieses Adressraums.
    unsafe {
        let (tbl, off) = if va >= gib1 && va < 2 * gib1 {
            (l2_phys, va - gib1)
        } else if va >= ISO_USER_VA && va < ISO_USER_VA + 512 * TWO_MIB {
            let l1 = core::slice::from_raw_parts(l1_phys as *const u64, 512);
            let e = l1[ISO_USER_L1];
            if e & 0b11 != TABLE_DESC {
                return None;
            }
            (e & ADDR_MASK, va - ISO_USER_VA)
        } else {
            // Abgewiesen, nicht geraten.
            return None;
        };
        let l2 = core::slice::from_raw_parts(tbl as *const u64, 512);
        let e = l2[(off >> 21) as usize];
        match e & 0b11 {
            // **Ein Block traegt seine Rechte selbst.** Ein Tabellen-Deskriptor auf L2 traegt
            // keine AP-Bits (die stehen im Blatt), deshalb wird dort NICHT geprueft, sondern eine
            // Ebene tiefer -- die Pruefung an der falschen Ebene waere schlimmer als keine, weil
            // sie wie eine Pruefung aussieht.
            BLOCK_DESC if el0_lesbar(e) => {
                Some((e & ADDR_MASK & !(TWO_MIB - 1)) | (va & (TWO_MIB - 1)))
            }
            TABLE_DESC => {
                let l3 = core::slice::from_raw_parts((e & ADDR_MASK) as *const u64, 512);
                let q = l3[((off >> 12) & 0x1ff) as usize];
                if q & 0b11 != PAGE_DESC || !el0_lesbar(q) {
                    return None;
                }
                Some((q & ADDR_MASK) | (va & (PAGE - 1)))
            }
            _ => None,
        }
    }
}

/// Die **Basis** einer isolierten VSpace in zwei frische 4-KiB-Frames bauen
/// (`l1_phys` = Wurzel, `l2_phys` = L2 für GiB 1): Device (EL1-only), das Kernelimage
/// über die **geteilte** Kernel-L3 (inkl. `.user_text` EL0-RX) und alles RAM
/// **EL1-only** (Kernel sieht alles, EL0 nichts). User-Frames werden erst über
/// [`vspace_map_block`] hinzugefügt. Der Aufrufer läuft in der globalen Map
/// (Identity), daher sind `l1_phys`/`l2_phys` direkt beschreibbar.
pub fn vspace_create_base(
    l1_phys: u64,
    l2_phys: u64,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    // SAFETY: frisch allozierte, 4-KiB-ausgerichtete RAM-Frames, in der globalen
    // Identity-Map gültig + beschreibbar. Genau zwei Tabellen werden initialisiert.
    let l1 = unsafe { core::slice::from_raw_parts_mut(l1_phys as *mut u64, 512) };
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };

    // L2 (GiB 1): [0] -> gemeinsame Kernel-L3; alles andere EL1-only (kein EL0).
    for (i, e) in l2.iter_mut().enumerate() {
        *e = if i == 0 {
            table_desc(table_phys(&L3_TABLE))
        } else {
            kernel_block(RAM_BASE + i as u64 * TWO_MIB)
        };
    }
    // L1: [0] Device, [1] -> L2 (GiB 1), [2..=8] restliches RAM EL1-only, Rest leer.
    for e in l1.iter_mut() {
        *e = 0;
    }
    l1[0] = DEVICE_BLOCK;
    l1[1] = table_desc(l2_phys);
    for (i, slot) in l1.iter_mut().enumerate().take(9).skip(2) {
        *slot = kernel_block(i as u64 * ONE_GIB);
    }
    cpu::dsb_sy();
    // ARM kommt mit den beiden gelieferten Frames aus (drei Ebenen bei 39-Bit-VA); der
    // `alloc`-Rückkanal existiert für x86, das eine Ebene mehr hat (PML4 -> PDPT -> PD).
    true
}

/// Index eines 2-MiB-Blocks in der GiB-1-L2 für die physische Adresse `phys`.
/// `None`, wenn `phys` nicht in `[USER_RAM_MIN, GIB1_END)` 2-MiB-ausgerichtet liegt.
fn l2_block_index(phys: u64) -> Option<usize> {
    if phys < USER_RAM_MIN || phys >= GIB1_END || phys % TWO_MIB != 0 {
        return None;
    }
    Some(((phys - RAM_BASE) / TWO_MIB) as usize)
}

/// Einen 2-MiB-Frame `phys` als **EL0-RW** (nG) in die VSpace mit GiB-1-Tabelle
/// `l2_phys` mappen (identity). Gibt `false` bei ungültiger/unausgerichteter Adresse.
/// Der Aufrufer muss anschließend die ASID flushen ([`flush_asid`]).
pub fn vspace_map_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = l2_block_index(phys) else {
        return false;
    };
    // SAFETY: `l2_phys` ist eine gültige, in der globalen Map beschreibbare L2-Tabelle.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    l2[idx] = user_block(phys);
    cpu::dsb_sy();
    true
}

/// Wie [`vspace_map_block`], aber als **EL0-RX** (privat geladener Code, W^X).
pub fn vspace_map_code_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = l2_block_index(phys) else {
        return false;
    };
    // SAFETY: gültige, in der globalen Map beschreibbare L2-Tabelle.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    l2[idx] = user_code_block(phys);
    cpu::dsb_sy();
    true
}

/// Einen zuvor gemappten 2-MiB-Frame `phys` wieder auf **EL1-only** zurücksetzen
/// (EL0 kann nicht mehr darauf zugreifen). Der Aufrufer muss die ASID flushen.
pub fn vspace_unmap_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = l2_block_index(phys) else {
        return false;
    };
    // SAFETY: wie `vspace_map_block`.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    l2[idx] = kernel_block(phys);
    cpu::dsb_sy();
    true
}

// --- 4-KiB-Seiten-Mapping (L3) ---
//
// Feingranular gegenüber den 2-MiB-Blöcken: ermöglicht gemischte RX/RO/RW-Rechte,
// Guard Pages und kleine Frames. Wird ein 2-MiB-Block erstmals seitenweise belegt,
// legt der Kernel eine L3-Tabelle an (alle 512 Seiten zunächst **EL1-only**, den
// Block spiegelnd) und hängt sie in die L2 ein; einzelne Seiten werden dann auf EL0
// gesetzt. L3-Frames werden beim Teardown über [`vspace_collect_l3s`] eingesammelt.

/// EL1-only 4-KiB-Seite (Spiegel beim Aufsplitten eines Blocks: Kernel sieht das RAM).
fn kernel_page(phys: u64) -> u64 {
    // nG (s. kernel_block): sonst überschattet ein global gecachter EL1-only-Spiegel dieselbe VA
    // in einer anderen VSpace, inkl. der EL0-Seite -> Permission-Fault Level 2/3 auf realer HW.
    phys | AF | SH_INNER | ATTR_NORMAL | AP_RW | PXN | UXN | NG | PAGE_DESC
}

/// EL0-User-Seite (4 KiB) mit gegebenem Recht, `nG` (ASID-spezifisch).
fn user_page(phys: u64, perm: UserPerm) -> u64 {
    let bits = match perm {
        UserPerm::Rw => AP_RW_EL0 | PXN | UXN,
        UserPerm::Rx => AP_RO_EL0 | PXN, //        EL0 ausführbar (UXN=0), W^X
        UserPerm::Ro => AP_RO_EL0 | PXN | UXN,
    };
    phys | AF | SH_INNER | ATTR_NORMAL | bits | NG | PAGE_DESC
}

/// Eine einzelne 4-KiB-Seite `phys` (identity) mit `perm` in die VSpace mit
/// GiB-1-L2 `l2_phys` mappen. Existiert für den 2-MiB-Block noch keine L3, wird über
/// `alloc_l3` eine neue 4-KiB-Tabelle angefordert (alle Seiten EL1-only, den Block
/// spiegelnd) und eingehängt. `false` bei ungültiger/unausgerichteter Adresse oder
/// fehlgeschlagener L3-Allokation. Der Aufrufer flusht anschließend die ASID.
pub fn vspace_map_page(
    l2_phys: u64,
    phys: u64,
    perm: UserPerm,
    alloc_l3: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if phys < USER_RAM_MIN || phys >= GIB1_END || phys % PAGE != 0 {
        return false;
    }
    let i2 = ((phys - RAM_BASE) / TWO_MIB) as usize;
    // SAFETY: gültige, in der globalen Map beschreibbare L2-Tabelle.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    let l3_phys = if l2[i2] & 0b11 == TABLE_DESC {
        l2[i2] & ADDR_MASK
    } else {
        let new = match alloc_l3() {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: frischer, identity-gemappter 4-KiB-Frame für die neue L3.
        let l3 = unsafe { core::slice::from_raw_parts_mut(new as *mut u64, 512) };
        let blk = RAM_BASE + i2 as u64 * TWO_MIB;
        for (j, e) in l3.iter_mut().enumerate() {
            *e = kernel_page(blk + j as u64 * PAGE); // zunächst alles EL1-only
        }
        l2[i2] = table_desc(new);
        new
    };
    // SAFETY: gültige L3-Tabelle (gerade angelegt oder bestehend).
    let l3 = unsafe { core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512) };
    l3[((phys >> 12) & 0x1ff) as usize] = user_page(phys, perm);
    cpu::dsb_sy();
    flush_va_global(phys); // identity (VA=phys): globalen Kernel-Block dieser VA evictieren
    true
}

/// Eine einzelne 4-KiB-Seite `vaddr` → `phys` (**nicht-identity**) mit `perm` in die VSpace mit
/// GiB-1-L2 `l2_phys` mappen (ext-26, Binary-Loader): `vaddr` indiziert die Tabellen (muss in
/// GiB 1 liegen), `phys` ist die **Ausgabe-Adresse** (beliebiger allokierter RAM-Frame). Damit
/// kann ein an einer festen VA gelinktes Programm an beliebige Physadressen geladen werden. Sonst
/// wie [`vspace_map_page`] (L3 bei Bedarf aus `alloc_l3`, EL1-only-Spiegel). ASID anschließend
/// flushen.
pub fn vspace_map_page_at(
    l2_phys: u64,
    vaddr: u64,
    phys: u64,
    perm: UserPerm,
    alloc_l3: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if vaddr < USER_RAM_MIN || vaddr >= GIB1_END || vaddr % PAGE != 0 || phys % PAGE != 0 {
        return false;
    }
    let i2 = ((vaddr - RAM_BASE) / TWO_MIB) as usize;
    // SAFETY: gültige, in der globalen Map beschreibbare L2-Tabelle.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    let l3_phys = if l2[i2] & 0b11 == TABLE_DESC {
        l2[i2] & ADDR_MASK
    } else {
        let new = match alloc_l3() {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: frischer, identity-gemappter 4-KiB-Frame für die neue L3.
        let l3 = unsafe { core::slice::from_raw_parts_mut(new as *mut u64, 512) };
        let blk = RAM_BASE + i2 as u64 * TWO_MIB; // VA-Block; Rest bleibt EL1-only
        for (j, e) in l3.iter_mut().enumerate() {
            *e = kernel_page(blk + j as u64 * PAGE);
        }
        l2[i2] = table_desc(new);
        new
    };
    // SAFETY: gültige L3-Tabelle (gerade angelegt oder bestehend).
    let l3 = unsafe { core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512) };
    // Indiziert per VADDR, Ausgabe-Adresse = PHYS (nicht-identity).
    l3[((vaddr >> 12) & 0x1ff) as usize] = user_page(phys, perm);
    cpu::dsb_sy();
    flush_va_global(vaddr); // globalen Kernel-Block der Link-VA evictieren (s. flush_va_global)
    true
}

/// Eine einzelne 4-KiB-Seite wieder auf **EL1-only** zurücksetzen (EL0-Zugriff
/// faultet). `false`, wenn der Block nicht seitenweise gemappt ist. ASID flushen.
pub fn vspace_unmap_page(l2_phys: u64, phys: u64) -> bool {
    if phys < USER_RAM_MIN || phys >= GIB1_END || phys % PAGE != 0 {
        return false;
    }
    let i2 = ((phys - RAM_BASE) / TWO_MIB) as usize;
    // SAFETY: gültige L2-Tabelle.
    let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
    if l2[i2] & 0b11 != TABLE_DESC {
        return false; // Block (nicht seitenweise) -> keine Einzelseite zu entmappen
    }
    let l3_phys = l2[i2] & ADDR_MASK;
    // SAFETY: gültige L3-Tabelle.
    let l3 = unsafe { core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512) };
    l3[((phys >> 12) & 0x1ff) as usize] = kernel_page(phys);
    cpu::dsb_sy();
    true
}

// --- DMA-Puffer-Mapping (ext-23, HardwareLand) ---
//
// Ein DMA-Puffer ist **echtes RAM** (kernel-ausgeschnitten), das ein bus-masterndes Gerät
// liest/schreibt. Es wird als **Normal-Non-Cacheable** EL0-RW-Seite (PXN|UXN, nG) in die
// isolierte Backend-VSpace gemappt — NC, weil CPU (Backend) und Gerät dieselbe Region ohne
// CPU-Cache-Pflege kohärent sehen sollen (QEMU `virt` ist ohnehin `dma-coherent`; auf realer
// HW ist NC die korrekte Wahl bzw. erspart Cache-Maintenance). Die Region liegt in **GiB 1**
// und nutzt die **bestehende** L3-Maschinerie: die Tabellen werden von [`vspace_collect_l3s`]
// eingesammelt und von [`vspace_wx_ok`] mit-auditiert (die DMA-Seiten sind PXN|UXN -> W^X gilt
// baulich). Das Entmappen beim Revoke nutzt [`vspace_unmap_page`] (zurück auf EL1-only).

/// EL0-RW 4-KiB-DMA-Seite (`nG`, PXN|UXN). `cacheable` = Coherent (Normal-WB), sonst Normal-NC.
fn dma_page(phys: u64, cacheable: bool) -> u64 {
    let attr = if cacheable { ATTR_NORMAL } else { ATTR_NORMAL_NC };
    phys | AF | SH_INNER | attr | AP_RW_EL0 | PXN | UXN | NG | PAGE_DESC
}

/// Eine DMA-Region `[phys, phys+len)` (4-KiB-granular, in **GiB 1**) als **EL0-RW** in die
/// isolierte VSpace mit GiB-1-L2 `l2_phys` mappen (identity, VA=PA). `cacheable` = Coherent
/// (Normal-WB, dann Cache-Maintenance per [`dma_cache_clean`]/[`dma_cache_invalidate`] nötig),
/// sonst Normal-NC (ext-23-Default). Legt L3-Tabellen bei Bedarf über `alloc_l3` an; diese
/// werden beim Teardown von [`vspace_collect_l3s`] freigegeben. Der Aufrufer flusht die ASID.
pub fn vspace_map_dma(
    l2_phys: u64,
    phys: u64,
    len: u64,
    cacheable: bool,
    alloc_l3: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if len == 0
        || phys % PAGE != 0
        || len % PAGE != 0
        || phys < USER_RAM_MIN
        || phys + len > GIB1_END
    {
        return false;
    }
    let mut p = phys;
    while p < phys + len {
        let i2 = ((p - RAM_BASE) / TWO_MIB) as usize;
        // SAFETY: gültige, in der globalen Map beschreibbare L2-Tabelle.
        let l2 = unsafe { core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512) };
        let l3_phys = if l2[i2] & 0b11 == TABLE_DESC {
            l2[i2] & ADDR_MASK
        } else {
            let new = match alloc_l3() {
                Some(q) => q,
                None => return false,
            };
            // SAFETY: frischer, identity-gemappter 4-KiB-Frame für die neue L3.
            let l3 = unsafe { core::slice::from_raw_parts_mut(new as *mut u64, 512) };
            let blk = RAM_BASE + i2 as u64 * TWO_MIB;
            for (j, e) in l3.iter_mut().enumerate() {
                *e = kernel_page(blk + j as u64 * PAGE); // zunächst alles EL1-only
            }
            l2[i2] = table_desc(new);
            new
        };
        // SAFETY: gültige L3-Tabelle.
        let l3 = unsafe { core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512) };
        l3[((p >> 12) & 0x1ff) as usize] = dma_page(p, cacheable);
        p += PAGE;
    }
    cpu::dsb_sy();
    true
}

/// **Cache-Maintenance für DMA** (ext-24, Coherent-Puffer). Operiert auf der identity-gemappten
/// VA (= PA). `dma_cache_clean`: Clean-to-PoC (CPU-Schreibvorgänge sichtbar machen, **vor**
/// einem Geräte-Read). Bei Non-Cacheable-Puffern sind diese No-Ops nötig, aber harmlos. QEMU
/// modelliert keine Caches; die Instruktionen sind dennoch gültig (Korrektheit auf realer HW).
/// **Cache Writeback Granule** aus `CTR_EL0.CWG` (Bits [27:24], log2 der Wortzahl).
///
/// Das ist die Granularität, mit der Cache-Wartung tatsächlich arbeitet — und damit die
/// Ausrichtung, die ein DMA-Puffer haben MUSS: `dc civac` invalidiert eine ganze Zeile. Liegt
/// fremder Speicher in derselben angebrochenen Zeile, verliert er beim Invalidate seine noch
/// nicht zurückgeschriebenen Daten. Meldet die HW `CWG = 0` (keine Angabe), gilt die
/// architektonische Obergrenze von 2 KiB als sichere Annahme.
/// Größtes bisher auf **irgendeinem** Kern gesehenes CWG (`0` = noch keiner gemeldet).
static CWG_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// CWG des **aufrufenden** Kerns aus `CTR_EL0` (Bits [27:24], log2 der Wortzahl).
fn local_cwg() -> u64 {
    let ctr: u64;
    // SAFETY: `CTR_EL0` ist read-only und ohne Seiteneffekte.
    unsafe { asm!("mrs {}, CTR_EL0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    match (ctr >> 24) & 0xf {
        // `0` heißt **„nicht angegeben"**, nicht „4 Byte". Würde man hier `4 << 0` rechnen, käme
        // 4 heraus und die Prüfung in `install_dma_cap` wäre praktisch vakuum — sie bestünde auf
        // Hardware, die CWG nicht meldet, ohne etwas zu prüfen. Stattdessen die architektonische
        // Obergrenze annehmen. (Linux fährt hier historisch 128, neuer bis 256; 2048 ist die
        // konservativere Wahl und kostet hier nichts, weil DMA-Regionen ohnehin seitengranular
        // vergeben werden.)
        0 => 2048,
        cwg => 4u64 << cwg,
    }
}

/// Den CWG des aufrufenden Kerns in das globale Maximum einrechnen.
///
/// `CTR_EL0` ist **pro Kern**. Auf heterogenen Systemen (big.LITTLE) können sich die Werte
/// unterscheiden — wer nur den gerade laufenden Kern liest, bekommt eine Antwort, die auf einem
/// anderen Cluster zu klein sein kann. Deshalb meldet jeder Kern seinen Wert beim Hochlauf, und
/// [`dma_granule`] liefert das Maximum.
///
pub fn record_cache_granule() {
    let g = local_cwg();
    // Nach dem Versiegeln darf kein Kern mehr einen GRÖSSEREN Wert melden: seither geprägte
    // DMA-Caps wurden gegen das kleinere Maximum geprüft und wären rückwirkend zu schwach
    // geprüft. Ein leises Anheben würde genau das verdecken — deshalb ein Abbruch mit Kontext.
    // Erreichbar wäre das nur durch CPU-Hotplug oder einen verzögerten Sekundärkern; heute
    // starten alle Kerne vor `seal_cache_granule()` (s. `kernel_main`).
    if CWG_SEALED.load(core::sync::atomic::Ordering::Acquire)
        && g > CWG_MAX.load(core::sync::atomic::Ordering::Relaxed)
    {
        panic!(
            "CTR_EL0.CWG={} eines spaeten Kerns groesser als das versiegelte Maximum {} — \
             seither gepraegte DMA-Caps waeren zu schwach geprueft",
            g,
            CWG_MAX.load(core::sync::atomic::Ordering::Relaxed)
        );
    }
    CWG_MAX.fetch_max(g, core::sync::atomic::Ordering::Relaxed);
}

/// Architektonische Obergrenze, wenn (noch) nicht alle Kerne gemeldet haben.
const CWG_ARCH_MAX: u64 = 2048;
/// Haben alle Kerne ihren Wert gemeldet?
static CWG_SEALED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Die Granule-Ermittlung **abschließen** — nach dem SMP-Hochlauf zu rufen, wenn jeder Kern
/// seinen `CTR_EL0.CWG` gemeldet hat.
///
/// Bis dahin liefert [`dma_granule`] die architektonische Obergrenze statt des bisher gesehenen
/// Maximums. Damit ist die frühere Fußnote („Caps vor dem SMP-Hochlauf sehen nur die bis dahin
/// gemeldeten Werte") **erzwungen statt dokumentiert**: eine früh geprägte Cap wird gegen die
/// strengstmögliche Granularität geprüft und kann nie zu schwach geprüft worden sein. Auf
/// homogenen Zielen ändert das nichts; auf einem heterogenen wird aus einer stillen
/// Fehlprägung eine abgelehnte.
pub fn seal_cache_granule() {
    CWG_SEALED.store(true, core::sync::atomic::Ordering::Release);
}

pub fn dma_granule() -> u64 {
    if !CWG_SEALED.load(core::sync::atomic::Ordering::Acquire) {
        // Noch nicht alle Kerne gemeldet -> konservativ, nie zu klein.
        return CWG_ARCH_MAX;
    }
    match CWG_MAX.load(core::sync::atomic::Ordering::Relaxed) {
        0 => CWG_ARCH_MAX,
        g => g,
    }
}

/// Cache-Line-Größe für die Wartungsschleifen (`CTR_EL0.DminLine`, Bits [19:16]).
fn cache_line() -> u64 {
    let ctr: u64;
    // SAFETY: read-only Systemregister.
    unsafe { asm!("mrs {}, CTR_EL0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    match (ctr >> 16) & 0xf {
        0 => 64,
        d => 4u64 << d,
    }
}
pub fn dma_cache_clean(va: u64, len: u64) {
    let line = cache_line();
    let mut p = va & !(line - 1);
    let end = va + len;
    // SAFETY: reine Cache-Wartung (DC CVAC) auf einer gültigen, identity-gemappten Region.
    unsafe {
        while p < end {
            asm!("dc cvac, {a}", a = in(reg) p, options(nostack, preserves_flags));
            p += line;
        }
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}
/// Clean **und** Invalidate (DC CIVAC) — **nach** einem Geräte-Write, bevor die CPU liest, bzw.
/// vor bidirektionalen Transfers. Verwirft stale CPU-Cache-Zeilen + schreibt Dirty-Zeilen zurück.
pub fn dma_cache_invalidate(va: u64, len: u64) {
    let line = cache_line();
    let mut p = va & !(line - 1);
    let end = va + len;
    // SAFETY: reine Cache-Wartung (DC CIVAC) auf einer gültigen, identity-gemappten Region.
    unsafe {
        while p < end {
            asm!("dc civac, {a}", a = in(reg) p, options(nostack, preserves_flags));
            p += line;
        }
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Alle **per-PD-L3-Tabellen** dieser VSpace einsammeln (für den Teardown): ruft
/// `free_l3(l3_phys)` für jeden L2-Eintrag, der auf eine L3 zeigt — außer Index 0
/// (die geteilte Kernel-L3). Danach gibt der Aufrufer L2 + L1 frei.
pub fn vspace_collect_l3s(l2_phys: u64, free_l3: &mut dyn FnMut(u64)) {
    // SAFETY: gültige L2-Tabelle (read-only Scan).
    let l2 = unsafe { core::slice::from_raw_parts(l2_phys as *const u64, 512) };
    for &e in l2.iter().skip(1) {
        if e & 0b11 == TABLE_DESC {
            free_l3(e & ADDR_MASK);
        }
    }
}

// --- Generisches Device-MMIO-Mapping (ext-22, HardwareLand) ---
//
// Ein **generischer** Mechanismus, eine beliebige MMIO-Region EL0-zugänglich in eine
// isolierte VSpace zu mappen — ohne geräte-spezifische Annahmen (PL031, UART, VirtIO,
// NVMe, … nutzen dieselbe Primitive). Die GiB-0-Device-Region liegt im Basis-VSpace als
// **EL1-only** 1-GiB-Block (`L1[0]`); zum Freigeben einzelner EL0-Device-Seiten wird der
// Block bei Bedarf in eine Device-L2 und die betroffene 2-MiB-Region in eine Device-L3
// aufgespalten — der Rest bleibt **EL1-only Device** (Kernel-MMIO wie UART/GIC bleibt
// erreichbar). Alle Device-Mappings sind `ATTR_DEVICE` (nGnRnE) + **PXN|UXN** (nie
// ausführbar -> W^X gilt baulich); EL0-Seiten sind `nG` (ASID-spezifisch).

/// EL1-only Device-2-MiB-Block (Spiegel beim Aufsplitten der GiB-0-Device-Region).
fn device_block(phys: u64) -> u64 {
    phys | AF | AP_RW | ATTR_DEVICE | PXN | UXN | BLOCK_DESC
}
/// EL1-only Device-4-KiB-Seite (Spiegel beim Aufsplitten eines Device-Blocks).
fn device_page_kernel(phys: u64) -> u64 {
    phys | AF | AP_RW | ATTR_DEVICE | PXN | UXN | PAGE_DESC
}
/// **EL0**-Device-4-KiB-Seite (`ro` = read-only, sonst RW), `nG`, PXN|UXN.
fn device_page_user(phys: u64, ro: bool) -> u64 {
    let ap = if ro { AP_RO_EL0 } else { AP_RW_EL0 };
    phys | AF | ap | ATTR_DEVICE | PXN | UXN | NG | PAGE_DESC
}

/// Eine MMIO-Region `[phys, phys+len)` (4-KiB-granular) als **EL0-Device** in die isolierte
/// VSpace mit L1-Wurzel `l1_phys` mappen. Generisch (keine geräte-spezifischen Annahmen).
/// Spaltet `L1[0]` bei Bedarf in eine Device-L2 und die betroffenen 2-MiB-Blöcke in
/// Device-L3 auf (Rest bleibt EL1-only Device). `alloc` liefert frische 4-KiB-Tabellen-
/// Frames. Gibt `false` bei ungültiger/unausgerichteter Region (nur GiB 0) oder
/// fehlgeschlagener Allokation. Der Aufrufer flusht anschließend die ASID.
pub fn vspace_map_device(
    l1_phys: u64,
    phys: u64,
    len: u64,
    ro: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if len == 0 || phys % PAGE != 0 || len % PAGE != 0 || phys >= ONE_GIB || phys + len > ONE_GIB {
        return false; // nur die GiB-0-Device-Region, seitengranular
    }
    // SAFETY: gültige, in der globalen Map beschreibbare L1-Tabelle.
    let l1 = unsafe { core::slice::from_raw_parts_mut(l1_phys as *mut u64, 512) };
    // 1. L1[0] (Device-1-GiB-Block) bei Bedarf in eine Device-L2 aufspalten.
    let dev_l2_phys = if l1[0] & 0b11 == TABLE_DESC {
        l1[0] & ADDR_MASK
    } else {
        let new = match alloc() {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: frischer identity-gemappter 4-KiB-Frame für die Device-L2.
        let l2 = unsafe { core::slice::from_raw_parts_mut(new as *mut u64, 512) };
        for (i, e) in l2.iter_mut().enumerate() {
            *e = device_block(i as u64 * TWO_MIB); // EL1-only Device, spiegelt die Region
        }
        l1[0] = table_desc(new);
        new
    };
    // SAFETY: gültige Device-L2 (gerade angelegt oder bestehend).
    let dev_l2 = unsafe { core::slice::from_raw_parts_mut(dev_l2_phys as *mut u64, 512) };
    // 2./3. Pro 4-KiB-Seite: betroffenen 2-MiB-Block bei Bedarf in eine Device-L3
    // aufspalten und die Seite EL0-Device setzen.
    let mut p = phys;
    while p < phys + len {
        let i2 = (p / TWO_MIB) as usize;
        let l3_phys = if dev_l2[i2] & 0b11 == TABLE_DESC {
            dev_l2[i2] & ADDR_MASK
        } else {
            let new = match alloc() {
                Some(q) => q,
                None => return false,
            };
            // SAFETY: frischer identity-gemappter 4-KiB-Frame für die Device-L3.
            let l3 = unsafe { core::slice::from_raw_parts_mut(new as *mut u64, 512) };
            let blk = i2 as u64 * TWO_MIB;
            for (j, e) in l3.iter_mut().enumerate() {
                *e = device_page_kernel(blk + j as u64 * PAGE); // EL1-only Device, spiegelt
            }
            dev_l2[i2] = table_desc(new);
            new
        };
        // SAFETY: gültige Device-L3.
        let l3 = unsafe { core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512) };
        l3[((p >> 12) & 0x1ff) as usize] = device_page_user(p, ro);
        p += PAGE;
    }
    cpu::dsb_sy();
    true
}

/// Beim Teardown die GiB-0-Device-Tabellen einer VSpace einsammeln: ist `L1[0]` eine
/// Tabelle (durch [`vspace_map_device`] aufgespalten), `free(...)` für jede Device-L3
/// **und** die Device-L2. (Die EL1-only Device-Blöcke/Seiten zeigen auf MMIO, nicht auf
/// RAM — es wird nur der Tabellen-Speicher freigegeben.)
// ================================================================================================
// Das private User-VA-Fenster einer isolierten PD (E-Rest 3d, zweite Hälfte)
// ================================================================================================
//
// Begründung wortgleich zur x86-Seite (s. dort): die private Region einer isolierten PD wurde
// **identisch** abgebildet und war damit an GiB 1 gebunden — gemessen **504** Regionen, der
// bindende Deckel für die Zahl gleichzeitiger Mandanten.
//
// Das Fenster muss dort liegen, wo der Kernel **nie identisch** zugreift, sonst verdeckt eine
// User-VA seine eigene Sicht auf physisches RAM. Auf aarch64 belegt `vspace_create_base` die
// L1-Einträge `[0]` (Device), `[1]` (GiB 1, die private RAM-Sicht) und `[2..=8]` (restliches RAM,
// EL1-only). **`L1[9]` und höher sind leer** — dort liegt das Fenster.
//
// Der Preis ist ein 4-KiB-Rahmen je PD (die L2 für dieses GiB), plus eine L3, wenn die Region
// kleiner als 2 MiB ist (gefärbter Pfad).

/// Gegenstueck zur x86-Funktion: dieser Zweig kennt die Zweiteilung „unterhalb/oberhalb der
/// Kartengrenze" nicht (s. [`LOW_MAPPED_END`]), also gibt es dort auch nichts zu zaehlen. `0`
/// heisst hier „die Frage stellt sich nicht", und die Pruefzeile meldet folgerichtig `SKIP`.
pub fn high_ram_gib() -> usize {
    0
}

/// Basis des privaten User-VA-Fensters einer isolierten PD: `L1[9]`, also 9 GiB.
pub const ISO_USER_VA: u64 = 9 * ONE_GIB;
/// Der L1-Index dazu.
const ISO_USER_L1: usize = 9;

/// Eine Tabelle beschaffen oder anlegen; gibt die Physadresse der nächsten Ebene zurück.
///
/// # Safety
/// `slot` muss auf einen gültigen, beschreibbaren Tabelleneintrag zeigen.
unsafe fn table_or_new(slot: &mut u64, alloc: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
    if *slot & 0b11 == TABLE_DESC {
        return Some(*slot & 0x0000_ffff_ffff_f000);
    }
    let new = alloc()?;
    // SAFETY: frisch alloziertes, identity-gemapptes Frame.
    unsafe {
        let t = core::slice::from_raw_parts_mut(new as *mut u64, 512);
        for e in t.iter_mut() {
            *e = 0;
        }
    }
    *slot = table_desc(new);
    Some(new)
}

/// **Die private Region `[phys, phys+len)` in das User-Fenster dieser PD abbilden.**
///
/// Gibt die **virtuelle** Adresse zurück; die Physadresse ist damit frei wählbar. Ein
/// 2-MiB-ausgerichteter 2-MiB-Block bleibt **ein** Blockdeskriptor — der Fastpath hing nie an der
/// Identität, sondern an der Ausrichtung der VA.
pub fn vspace_map_user_window(
    l1_phys: u64,
    slot: usize,
    phys: u64,
    len: u64,
    perm: UserPerm,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Option<u64> {
    if len == 0 || len % PAGE != 0 || phys % PAGE != 0 || len > TWO_MIB || slot >= 512 {
        return None;
    }
    // SAFETY: gültige, beschreibbare L1 dieses Adressraums; die Tabellen darunter sind frisch
    // alloziert oder von diesem Adressraum angelegt.
    unsafe {
        let l1 = core::slice::from_raw_parts_mut(l1_phys as *mut u64, 512);
        let l2_phys = table_or_new(&mut l1[ISO_USER_L1], alloc)?;
        let l2 = core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512);
        if len == TWO_MIB && phys % TWO_MIB == 0 {
            l2[slot] = match perm {
                UserPerm::Rx => user_code_block(phys),
                _ => user_block(phys),
            };
        } else {
            let l3_phys = table_or_new(&mut l2[slot], alloc)?;
            let l3 = core::slice::from_raw_parts_mut(l3_phys as *mut u64, 512);
            for i in 0..(len / PAGE) as usize {
                l3[i] = user_page(phys + (i as u64) * PAGE, perm);
            }
        }
    }
    cpu::dsb_sy();
    Some(ISO_USER_VA + (slot as u64) * TWO_MIB)
}

/// Die Tabellen des User-Fensters beim Abbau zurückgeben (Gegenstück zu
/// [`vspace_map_user_window`]).
pub fn vspace_collect_user_window(l1_phys: u64, free: &mut dyn FnMut(u64)) {
    // SAFETY: gültige, lesbare L1 des abzubauenden Adressraums.
    unsafe {
        let l1 = core::slice::from_raw_parts_mut(l1_phys as *mut u64, 512);
        if l1[ISO_USER_L1] & 0b11 != TABLE_DESC {
            return;
        }
        let l2_phys = l1[ISO_USER_L1] & 0x0000_ffff_ffff_f000;
        let l2 = core::slice::from_raw_parts_mut(l2_phys as *mut u64, 512);
        // **Alle** Plätze durchgehen, nicht nur den ersten (`spawn_isolated_native` belegt
        // zwei). Ein Blockdeskriptor ist dabei keine Tabelle -- ihn freizugeben gäbe dem
        // Allokator die Nutzregion zurück, die der Aufrufer verwaltet.
        for &e in l2.iter() {
            if e & 0b11 == TABLE_DESC {
                free(e & 0x0000_ffff_ffff_f000);
            }
        }
        free(l2_phys);
        l1[ISO_USER_L1] = 0;
    }
}

pub fn vspace_collect_device_tables(l1_phys: u64, free: &mut dyn FnMut(u64)) {
    // SAFETY: gültige L1-Tabelle (read-only Scan).
    let l1 = unsafe { core::slice::from_raw_parts(l1_phys as *const u64, 512) };
    if l1[0] & 0b11 != TABLE_DESC {
        return; // GiB 0 ist noch ein Block -> keine Device-Tabellen
    }
    let dev_l2_phys = l1[0] & ADDR_MASK;
    // SAFETY: gültige Device-L2.
    let dev_l2 = unsafe { core::slice::from_raw_parts(dev_l2_phys as *const u64, 512) };
    for &e in dev_l2.iter() {
        if e & 0b11 == TABLE_DESC {
            free(e & ADDR_MASK);
        }
    }
    free(dev_l2_phys);
}

/// **W^X-/Struktur-Audit der GiB-0-Device-Mappings** einer VSpace (read-only). Prüft jede
/// EL0-Device-Seite: sie MUSS nicht-ausführbar sein (PXN+UXN gesetzt) — eine EL0-aus-
/// führbare Device-Seite wäre eine W^X-Verletzung. Gibt `true` bei Konsistenz. Device-
/// Seiten sind per Konstruktion PXN|UXN; der Check fängt eine fehlerhafte Erzeugung ab.
pub fn vspace_device_wx_ok(l1_phys: u64) -> bool {
    // SAFETY: gültige L1-Tabelle (read-only).
    let l1 = unsafe { core::slice::from_raw_parts(l1_phys as *const u64, 512) };
    if l1[0] & 0b11 != TABLE_DESC {
        return true; // kein Device-EL0-Mapping
    }
    let dev_l2 = unsafe { core::slice::from_raw_parts((l1[0] & ADDR_MASK) as *const u64, 512) };
    for &e2 in dev_l2.iter() {
        if e2 & 0b11 != TABLE_DESC {
            continue;
        }
        let l3 = unsafe { core::slice::from_raw_parts((e2 & ADDR_MASK) as *const u64, 512) };
        for &e3 in l3.iter() {
            // Eine EL0-Device-Seite darf nie schreibbar+ausführbar sein (W^X). Device-
            // Seiten sind per Konstruktion PXN|UXN; `is_wx_violation` fängt eine
            // fehlerhafte (EL0+writable+executable) Erzeugung ab.
            if is_wx_violation(e3) {
                return false;
            }
        }
    }
    true
}

/// Eine 1-GiB-**Device**-Region (EL1-only, nGnRnE, PXN|UXN) bei GiB-Index `gib` in die
/// **globale** Kernel-Map einhängen — für kernel-seitigen Gerätezugriff jenseits der statisch
/// gemappten GiB 0..8 (ext-23: PCIe-ECAM @256 GiB). Nur vom Primärkern aufzurufen; danach
/// TLB-Broadcast (inner-shareable), damit alle Kerne den neuen Eintrag sehen. Idempotent.
/// `false` bei out-of-range `gib` (39-bit VA -> 512 GiB) oder bereits belegtem Nicht-Block-
/// Eintrag (würde eine bestehende Tabelle überschreiben).
pub fn map_device_block_global(gib: usize) -> bool {
    if gib >= 512 {
        return false;
    }
    let l1 = table_mut(&L1_TABLE);
    // Einen bestehenden Tabellen-Deskriptor NICHT überschreiben (kein Leak/Korruption).
    if l1[gib] & 0b11 == TABLE_DESC {
        return false;
    }
    l1[gib] = device_block(gib as u64 * ONE_GIB);
    // SAFETY: globale L1-Tabelle aktualisiert; vollständiger inner-shareable TLB-Flush +
    // Barrieren, damit der neue (zuvor ungültige) Eintrag auf allen Kernen sichtbar wird.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
    true
}

/// TLB-Einträge einer ASID invalidieren (nach map/unmap bzw. VSpace-Teardown).
/// Global getaggte Kernel-/Code-Einträge bleiben gültig.
pub fn flush_asid(asid: u16) {
    let arg = (asid as u64) << 48;
    // SAFETY: TLB-Invalidate nach ASID; reine MMU-Wartung + Barrieren.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi aside1is, {a}", // inner-shareable: alle Kerne (echtes SMP; Threads migrieren)
            "dsb ish",
            "isb",
            a = in(reg) arg,
            options(nostack, preserves_flags),
        );
    }
}

/// TLB-Invalidate **einer VA über ALLE ASIDs** (`tlbi vaae1is`, inner-shareable) — inklusive
/// **globaler** Einträge und über alle Kerne. Nötig, weil eine identity-gemappte User-Seite (VA=PA)
/// dieselbe VA trifft wie ein global gemappter Kernel-Block: ein per-ASID-Flush ([`flush_asid`])
/// evictet globale Einträge nicht, und ein Nutzer-Thread kann auf einem anderen Kern laufen als dem,
/// der gemappt hat. `va` sollte 4-KiB-ausgerichtet sein; das Argument ist VA[55:12].
pub fn flush_va_global(va: u64) {
    // SAFETY: TLB-Invalidate nach VA (all-ASID, inner-shareable); reine MMU-Wartung + Barrieren.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {v}",
            "dsb ish",
            "isb",
            v = in(reg) (va >> 12),
            options(nostack, preserves_flags),
        );
    }
}

// ==============================================================================================
// Guard pages for EL1 (kernel) stacks
// ==============================================================================================
//
// Since 2026-08-10 the EL0 kernel stack of every user thread carries an
// unmapped page below it (`guard_unmap`): a stack overflow faults instead of
// silently writing into the neighbor — reachable from EL0, hence a privilege
// escalation, not a diagnostics question. On aarch64 this used to be a
// counter (`guard_unterstuetzt() == false`); this section is the real mapping,
// built on the x86 pattern (`hal/src/x86_64/mmu.rs`, guard section).
//
// A guard must be missing from the **identity map**, because that is how the
// kernel reaches its own stack. Kernel stacks come from `mem_alloc`, whose
// free list starts at 2 MiB — above the 4 KiB-granular kernel L3 — where the
// map runs in **2 MiB blocks**. Carving a 4 KiB hole therefore means
// splitting the block into a page table: 512 entries mirroring the block,
// the L2 entry repointed at the table, TLB flushed. Afterwards the same
// block is 4 KiB-granular and every further guard in it costs one entry.
//
// The pool is **fixed**: the HAL has no allocator (kernel boundary), so
// `GUARD_L3` holds `GUARD_SLOTS` split blocks. When it runs out,
// `guard_unmap` returns `false` and the caller refuses the stack request by
// name (`MANGEL_GUARD_TABELLE`). Fail-closed: a silently unguarded stack
// would be exactly the failure this function guards against.
//
// Scope: GiB 1 only. Splitting a 1 GiB L1 block (GiB 2..8) would need a
// second split level (a static L2 pool on top of the L3 pool); stacks above
// GiB 1 are refused by name instead of guarded approximately. At suite scale
// every stack lands in GiB 1 (free RAM starts at 2 MiB and the suite needs
// tens of MiB of stacks), so the bound never fires there — but it is a
// bound, documented here rather than discovered in a log.
//
// TLB discipline: the split repoints a live L2 entry, so every cached
// translation below that block is stale — a full inner-shareable EL1
// invalidate (`flush_all_is`), once per split. Setting/clearing a single
// guard entry invalidates that VA across all ASIDs (`flush_va_global`):
// the same VA hits a globally-tagged kernel block, which a per-ASID flush
// would not evict.
//
// SMP discipline: slot claiming is a compare-exchange to an in-progress
// marker (`SLOT_READING`); the block number is published last, so lookups
// (`Acquire`) only ever see completed splits. Stack allocation runs on all
// cores, so two cores may split different blocks concurrently; the loser
// retries and finds the winner's slot. Fill-then-publish: the L3 is
// completed before the L2 entry points at it.
//
// Color interplay (C9e): a guard page carries no data and therefore occupies
// no cache set. The colored stack path already requests it with a one-page
// lead (`Vorspann`) exempt from the color condition
// (`alloc_colored_vorspann`); forcing the guard into the stripe made every
// colored stack request on 16-color ARM structurally unsatisfiable (5
// consecutive pages against a 4-color stripe). Nothing is weakened by the
// exemption — the color guarantee says nothing about a page that is never
// read or written.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// How many 2 MiB blocks can be split for guards. Same derivation as the
/// x86 `GUARD_BLOCKS`: a 16 KiB stack plus guard occupies 20 KiB, so one
/// block holds ~100 guarded stacks and 16 blocks hold ~1600 — far above a
/// suite run, below 10 000 threads (the structural answer to that is a
/// stack arena, not a bigger constant).
const GUARD_SLOTS: usize = 16;

/// Split L3 tables, one per slot. Written once at split time
/// (fill-then-publish), then only single entries are flipped.
static mut GUARD_L3: [PageTable; GUARD_SLOTS] = [const { PageTable::EMPTY }; GUARD_SLOTS];

/// Which 2 MiB block slot `k` carries (`usize::MAX` = free). Claimed by
/// compare-exchange; see [`guard_slot`].
static GUARD_BLOCK_OF: [AtomicUsize; GUARD_SLOTS] =
    [const { AtomicUsize::new(usize::MAX) }; GUARD_SLOTS];

/// The original L2 block descriptor per slot, stored at split time. The
/// source the mirror pages — and any later [`guard_remap`] — are derived
/// from, never a hardcoded constant.
static GUARD_BLOCK_DESC: [AtomicU64; GUARD_SLOTS] =
    [const { AtomicU64::new(0) }; GUARD_SLOTS];

/// Guards currently standing (report/balance).
static GUARDS_LIVE: AtomicUsize = AtomicUsize::new(0);
/// Guards set in total (speaking probe: 0 means "never used").
static GUARDS_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// How often a guard could **not** be set (pool exhausted / out of range).
static GUARDS_DENIED: AtomicUsize = AtomicUsize::new(0);

/// Unguarded stacks allocated while guards were unsupported. Permanently 0
/// since real guards landed; kept because the kernel's fallback path still
/// reports through it on architectures without guards (x86 reports 0 too).
static UNBEWACHT: AtomicUsize = AtomicUsize::new(0);

/// Full inner-shareable EL1 TLB invalidate. For the split path, where a
/// live L2 entry is repointed and every cached translation below it is
/// stale. Guard set/clear touch one VA and use [`flush_va_global`].
fn flush_all_is() {
    // SAFETY: TLB maintenance only, with barriers; no memory effect.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

/// Marker for a slot whose split is in progress (claimed, not yet
/// published). Never equals a block number, so the reuse check cannot hit a
/// half-filled table.
const SLOT_READING: usize = usize::MAX - 1;

/// Make the 2 MiB block of `pa` 4 KiB-granular; return its `GUARD_L3` slot.
///
/// Reuses the slot when the block is already split — the second stack in
/// the same block costs no second table. `None` when `pa` is outside the
/// guard range, when the L2 entry is not a plain block, or when the pool is
/// exhausted.
///
/// Concurrency: the claim is a compare-exchange to `SLOT_READING`, and the
/// block number is published (`Release`) only after the table is filled and
/// the L2 entry repoints at it (fill-then-publish). Lookups use `Acquire`
/// loads, so they only ever see completed splits. A core that loses a claim
/// retries: the winner publishes within microseconds (straight-line code,
/// no locks, no allocation), so the loop always terminates.
fn guard_slot(pa: u64) -> Option<usize> {
    let blk = super::guard::block_of(pa)? as usize;
    loop {
        // Published split of this block?
        if let Some(k) = GUARD_BLOCK_OF
            .iter()
            .position(|b| b.load(Ordering::Acquire) == blk)
        {
            return Some(k);
        }
        // Claim a free slot for the split.
        let Some(k) = GUARD_BLOCK_OF
            .iter()
            .position(|b| b.load(Ordering::Relaxed) == usize::MAX)
        else {
            return None; // pool exhausted (or all mid-split; caller refuses)
        };
        if GUARD_BLOCK_OF[k]
            .compare_exchange(usize::MAX, SLOT_READING, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue; // lost the race — retry (reuse check first)
        }
        // Won the slot: mirror the block into the L3 **before** publishing.
        // SAFETY: slot `k` is ours alone (just claimed); the L2 table is the
        // kernel's own identity-mapped static; `blk` is range-checked above.
        let l2 = table_mut(&L2_TABLE);
        let old = l2[blk];
        if old & 0b11 != super::guard::BLOCK_KIND {
            // Not a plain block (unmapped, or a table — unreachable through
            // the reuse check): release the claim and refuse.
            GUARD_BLOCK_OF[k].store(usize::MAX, Ordering::Release);
            return None;
        }
        let base = RAM_BASE + blk as u64 * TWO_MIB;
        // SAFETY: claimed, 4 KiB-aligned static table; index < 512.
        unsafe {
            let tables = &mut *core::ptr::addr_of_mut!(GUARD_L3);
            for (j, e) in tables[k].0.iter_mut().enumerate() {
                *e = super::guard::split_page(old, base + j as u64 * PAGE);
            }
            l2[blk] = table_desc(core::ptr::addr_of!(tables[k]) as u64);
        }
        GUARD_BLOCK_DESC[k].store(old, Ordering::Relaxed);
        cpu::dsb_sy();
        flush_all_is();
        GUARD_BLOCK_OF[k].store(blk, Ordering::Release);
        return Some(k);
    }
}

/// **Whether this architecture does guard pages.** The caller uses it to
/// tell "pool empty" (refuse) from "unsupported here" (count and continue).
pub fn guard_unterstuetzt() -> bool {
    true
}

/// **Set a guard**: remove the 4 KiB page at `pa` from the identity map.
///
/// `false` means "could not" — the caller must then **refuse** the request.
/// A silently unguarded stack would be exactly the state this function
/// prevents.
pub fn guard_unmap(pa: u64) -> bool {
    if pa % PAGE != 0 {
        return false;
    }
    let Some(k) = guard_slot(pa) else {
        GUARDS_DENIED.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let idx = super::guard::page_index(pa);
    // SAFETY: table and index just derived; one 64-bit store, then TLB flush.
    unsafe {
        let tables = &mut *core::ptr::addr_of_mut!(GUARD_L3);
        tables[k].0[idx] = 0; // invalid -> every access faults
    }
    flush_va_global(pa);
    GUARDS_LIVE.fetch_add(1, Ordering::Relaxed);
    GUARDS_TOTAL.fetch_add(1, Ordering::Relaxed);
    true
}

/// Lift a guard again (when freeing the stack — otherwise the allocator
/// would hand out a page nobody can reach).
pub fn guard_remap(pa: u64) -> bool {
    if pa % PAGE != 0 {
        return false;
    }
    let Some(blk) = super::guard::block_of(pa) else {
        return false;
    };
    let Some(k) = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Acquire) == blk as usize)
    else {
        return false;
    };
    // The rights come from the stored block descriptor — the same source
    // the split used. A neighbor entry would do (a guard never fills a
    // whole table), but the stored source cannot be another guard.
    let entry = super::guard::split_page(GUARD_BLOCK_DESC[k].load(Ordering::Relaxed), pa);
    let idx = super::guard::page_index(pa);
    // SAFETY: as in `guard_unmap`.
    unsafe {
        let tables = &mut *core::ptr::addr_of_mut!(GUARD_L3);
        tables[k].0[idx] = entry;
    }
    flush_va_global(pa);
    GUARDS_LIVE.fetch_sub(1, Ordering::Relaxed);
    true
}

/// How many stacks currently run without a guard (0: all guarded).
pub fn unbewachte_stacks() -> usize {
    UNBEWACHT.load(Ordering::Relaxed)
}

/// `(standing guards, total set, refused, occupied blocks, block pool)`.
pub fn guard_stats() -> (usize, usize, usize, usize, usize) {
    let belegt = GUARD_BLOCK_OF
        .iter()
        .filter(|b| {
            let v = b.load(Ordering::Relaxed);
            v != usize::MAX && v != SLOT_READING
        })
        .count();
    (
        GUARDS_LIVE.load(Ordering::Relaxed),
        GUARDS_TOTAL.load(Ordering::Relaxed),
        GUARDS_DENIED.load(Ordering::Relaxed),
        belegt,
        GUARD_SLOTS,
    )
}

/// **Is `pa` currently a standing guard?** — read, not computed.
pub fn ist_wache(pa: u64) -> bool {
    let Some(blk) = super::guard::block_of(pa) else {
        return false;
    };
    let Some(k) = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Acquire) == blk as usize)
    else {
        return false;
    };
    let idx = super::guard::page_index(pa);
    // SAFETY: static table, occupied slot, derived index — read-only.
    unsafe { super::guard::is_invalid((*core::ptr::addr_of!(GUARD_L3))[k].0[idx]) }
}

/// **The address of ONE currently standing guard** — or `None` when none
/// stands. For probes that must touch a page provably unmapped (the x86
/// `#DF` probe reads its guard instead of computing one from `TSS.rsp0`).
pub fn erste_lebende_wache() -> Option<u64> {
    for (k, b) in GUARD_BLOCK_OF.iter().enumerate() {
        let blk = b.load(Ordering::Acquire);
        if blk == usize::MAX || blk == SLOT_READING {
            continue;
        }
        // SAFETY: static table, occupied slot (just read); read-only.
        let tables = unsafe { &*core::ptr::addr_of!(GUARD_L3) };
        for (i, e) in tables[k].0.iter().enumerate() {
            if super::guard::is_invalid(*e) {
                return Some(blk as u64 * TWO_MIB + RAM_BASE + i as u64 * PAGE);
            }
        }
    }
    None
}
