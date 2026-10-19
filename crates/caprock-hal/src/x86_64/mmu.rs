//! 4-Level-Paging mit W^X-Identity-Map (x86_64) — API-gleich zum aarch64-MMU-Modul.
//!
//! ## Gleiches Modell, andere Deskriptoren
//!
//! Wie auf ARM ist die Abbildung eine **Identity-Map** (virtuell == physisch, ADR 0002) mit
//! strikter W^X-Trennung; nur die Deskriptorformate unterscheiden sich:
//!
//! | Eigenschaft | aarch64 | x86_64 |
//! |---|---|---|
//! | Ebenen | L1/L2/L3 (4 KiB / 2 MiB / 1 GiB) | PML4/PDPT/PD/PT (dieselben Größen) |
//! | „nicht ausführbar" | `UXN`/`PXN` getrennt für EL0/EL1 | **ein** `NX`-Bit (braucht `EFER.NXE`) |
//! | User-Zugriff | `AP[1]` | `US`-Bit |
//! | Schreibschutz für Ring 0 | über `AP` | `CR0.WP` **muss** gesetzt sein, sonst ignoriert Ring 0 `RW` |
//! | Adressraum-Tag | ASID | PCID |
//!
//! Die ersten 16 MiB werden mit **4-KiB-Seiten** abgebildet, damit das Kernel-Image
//! seitengenau in `.text` (R-X), `.rodata` (R--/NX) und Daten (RW/NX) zerfällt; darüber
//! genügen 2-MiB-Blöcke. Das oberste GiB unter 4 GiB ist **uncacheable** (LAPIC/IOAPIC/PCI).
//!
//! ## Oberhalb von 4 GiB (E-Rest 3)
//!
//! Bis 2026-08-03 endete die Karte bei 4 GiB. Das genügt, solange die Firmware alles unter
//! 4 GiB legt — und genau das tut sie **nicht** mehr, sobald QEMU Speicher oberhalb 4 GiB
//! anlegt: SeaBIOS schiebt die 64-Bit-BARs dann in ein Loch bei 448 GiB, und der erste
//! Registerzugriff des Treibers starb mit `#PF, cr2 = 0x70_0000_0014`. Der Zweig „RAM oberhalb
//! 4 GiB" war damit nicht bloß ungeprüft, sondern **nicht ausführbar** — und an `RAM_TOP`
//! hängt die Wahl des IOVA-Fensters.
//!
//! Oberhalb von `LOW_GIB` ist die Karte deshalb **nicht flächig**, sondern wächst nur dort, wo
//! der Hochlauf belegen kann, dass etwas da ist. Zwei Quellen, zwei Mechanismen:
//!
//! | Was | Woher | Wie abgebildet |
//! |---|---|---|
//! | Geräte-Registerfenster | die BAR-Ermittlung der PCI-Enumeration | 2-MiB-Blöcke aus einem kleinen statischen Vorrat ([`HIGH_DEV_SLOTS`]), **nur** wo ein BAR liegt |
//! | RAM | der Speicherplan des Bootloaders | 1-GiB-Blätter, und nur für GiB, die der Plan **vollständig** deckt |
//!
//! Die Alternative wäre gewesen, den ganzen `PML4[0]` (512 GiB) mit 1-GiB-Seiten flächig
//! abzubilden — 512 Einträge, kein einziger zusätzlicher Tabellenrahmen. Dagegen spricht genau
//! ein Satz: dann ist auch alles präsent, wo **nichts** ist. Ein verirrter Kernel-Zeiger nach
//! 200 GiB träfe eine gültige, beschreibbare Seite, der Schreibvorgang verschwände im Nichts
//! und niemand erführe davon. Ein #PF ist die bessere Antwort auf eine Adresse, die es nicht
//! gibt. Der Preis der sparsamen Variante sind 16 KiB statische Tabellen und ein Aufruf an der
//! Stelle, an der die BARs ohnehin schon gelesen werden.
//!
//! **Für die Isolation ändert sich dadurch nichts zum Schlechteren, sondern etwas zum
//! Besseren.** `vspace_create_base` spiegelt die hohen PDPT-Einträge in den isolierten
//! Adressraum — aber **ohne `US`**. Der Kernel läuft beim Syscall im Adressraum der PD und
//! muss dort hohes RAM und hohe Register erreichen; ein Ring-3-Thread der PD darf es nicht.
//! Ein Gerätefenster für eine einzelne PD entsteht wie unter 4 GiB über eine **private Kopie**
//! des Seitenverzeichnisses ([`private_pd`]); der statische Vorrat wird dabei nie beschrieben.
//! Dass er nie beschrieben und nie freigegeben wird, hängt jetzt an **einer** strukturellen
//! Frage ([`is_static_table`]) statt an einem Vergleich mit einer einzelnen Tabelle.
//!
//! ## Noch nicht portiert
//!
//! Die `vspace_*`-Funktionen (per-Prozess-Adressräume isolierter PDs, ext-11ff) melden hier
//! ehrlich „nicht unterstützt" statt etwas Halbes zu tun: der Kernel fängt das ab und
//! erzeugt auf x86 keine isolierten PDs. Sie brauchen zusätzlich PCID-Verwaltung und ein
//! per-VSpace-Tabellenlayout — ein eigener Portierungsschritt (s. `TODO.md`).

use super::cpu;
use core::sync::atomic::{AtomicUsize, Ordering};

// --- Deskriptor-Bits ------------------------------------------------------------------------

const P: u64 = 1 << 0; //  present
const RW: u64 = 1 << 1; // schreibbar
const US: u64 = 1 << 2; // im User-Modus (Ring 3) zugreifbar
const PWT: u64 = 1 << 3; // write-through
const PCD: u64 = 1 << 4; // cache disable (MMIO)
const A: u64 = 1 << 5; // accessed  -- von der HARDWARE gesetzt, nie von uns
const D: u64 = 1 << 6; // dirty     -- dito
const PS: u64 = 1 << 7; // page size: 2-MiB-Block statt Tabellenverweis
const NX: u64 = 1 << 63; // not executable (braucht EFER.NXE)

const PAGE: u64 = 4096;
const TWO_MIB: u64 = 2 * 1024 * 1024;
const ONE_GIB: u64 = 1 << 30;

/// **Ende des fest abgebildeten Bereichs** (4 GiB). Darunter steht die Karte seit
/// [`init_primary`]; darüber wächst sie nur dort, wo der Hochlauf etwas belegen kann.
pub const LOW_MAPPED_END: u64 = (LOW_GIB as u64) * ONE_GIB;

/// Untergrenze des für User-RAM nutzbaren Bereichs.
///
/// Unterhalb liegen auf dem PC das BIOS-/VGA-Fenster, das Kernel-Image (ab 1 MiB) und die
/// Boot-Seitentabellen. 16 MiB ist die Grenze, bis zu der wir mit 4-KiB-Seiten abbilden.
pub const USER_RAM_MIN: u64 = 16 * 1024 * 1024;
/// Ende des ersten GiB (Grenze für isolierte Regionen — wie auf aarch64).
pub const GIB1_END: u64 = ONE_GIB;
/// **Die Blockgranularität der Identitätskarte** (2 MiB) — ein L2/PD-Blockdeskriptor.
///
/// **Diese Konstante trug bis C7b ZWEI Bedeutungen**, und beide standen unter demselben Namen:
/// „so gross ist ein Block, den `vspace_map_block` in einem Deskriptor abbildet" und „so gross ist
/// die private Region einer isolierten PD". Solange die private Region genau ein Block war, war
/// das dieselbe Zahl — und genau deshalb ist es dieselbe Klasse Falle wie `Scheduler::spawn_user`
/// (ein Wert für EL0-SP **und** Reap-Region): harmlos, solange die beiden zufällig gleich sind.
///
/// Seit C7b ist die private Region **gemessen** und kleiner. Wer hier eine kleinere Zahl einsetzt,
/// bekäme `vspace_map_block` mit einer Länge, die kein Block ist. Die private Region steht deshalb
/// als [`PRIV_REGION_SIZE`] daneben.
pub const ISO_REGION_SIZE: u64 = TWO_MIB;

/// **Die private Region einer isolierten PD** — der EL0-Stack EINES Threads (C7b).
///
/// **Die Zahl ist gemessen, nicht gewählt.** Bis zum 2026-08-13 stand hier 2 MiB, und das war
/// **Granularität, nicht Bedarf**: [`vspace_map_user_window`] bildet eine 2-MiB-ausgerichtete
/// 2-MiB-Region in EINEM Blockdeskriptor ab und spart damit den 4-KiB-Rahmen einer Seitentabelle.
/// Eine PD kostete dadurch rund 2076 KiB, davon 2048 KiB für einen Stack, dessen tatsächliche
/// Tiefe niemand gemessen hatte.
///
/// Die Wasserstandsmarke (`kernel/src/userstackmark.rs`, Prüfzeile `ustack`) misst sie seither.
/// Die Grösse folgt der **Summenbedingung** und nicht dem beobachteten Höchststand: tiefster
/// gemessener Pfad + geforderte Reserve ≤ Regionsgrösse. Der Tausch ist eindeutig — eine Tabelle
/// (+4 KiB) gegen 1984 KiB gespartem Stack.
///
/// **Die Messung, aus der die 64 KiB folgen** (2026-08-13, x86, `-m 512M`):
///
/// | Suite | tiefster gemessener EL0-Pfad | Region, in der er auftrat |
/// |---|---|---|
/// | `test-qemu-x86.sh` (ohne Archiv) | 6168 B — und das ist die **Tiefensonde selbst** | 2 MiB |
/// | `test-qemu-x86-load.sh` (mit Archiv) | **7400 B** (ein echtes geladenes Programm) | 16 KiB |
///
/// Summenbedingung: `7400 B + 8192 B Reserve = 15 592 B ≤ 65 536 B` — Abstand Faktor 4,2. Gegen
/// den 2-MiB-Stand ist das **Faktor 32 weniger Speicher** je Mandant.
///
/// **Warum nicht kleiner, obwohl auch 16 KiB die Bedingung erfüllten** (`7400 + 2048 ≤ 16384`):
/// weil der gemessene Wasserstand über die **Nullung** erhoben ist und damit eine **Untergrenze**
/// bleibt — ein mit Null beschriebenes Stackwort ist unsichtbar. Der Abstand zwischen Messwert und
/// Grösse ist die Antwort darauf und keine Bequemlichkeit. Dazu kommt, dass 64 KiB die Grösse ist,
/// die die **gefärbte** Region auf x86 ohnehin hat (`colors::region_bytes()`): zwei Wege zur
/// selben PD-Sorte mit **einer** Zahl statt mit zweien.
///
/// **Was das Senken NEBENBEI bringt, und es ist kein Nebensatz:** die private Region liegt am
/// Anfang eines 2-MiB-VA-Fensterplatzes ([`vspace_map_user_window`]), und abgebildet wird nur,
/// was sie belegt. Mit 2 MiB war der Platz **voll** — ein Stacküberlauf lief in die eigene Region
/// weiter. Mit 64 KiB liegen darunter 1984 KiB **ungemappte** VA: ein Überlauf faultet, statt
/// still zu schreiben. Das ist dieselbe Eigenschaft, die der EL1-Stack seit dem 2026-08-10 als
/// Guard-Page hat — hier fällt sie ohne eine einzige zusätzliche Seite an.
pub const PRIV_REGION_SIZE: u64 = 64 * 1024;

/// Rechte einer Kernel-Abbildung (wie aarch64 `Perm`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Perm {
    /// Lesen + Ausführen (Kernel-Code).
    Rx,
    /// Nur lesen.
    Ro,
    /// Lesen + Schreiben, nicht ausführbar.
    Rw,
    /// EL0/Ring-3 lesbar+schreibbar, nicht ausführbar.
    UserRw,
    /// EL0/Ring-3 lesbar+ausführbar (User-Code).
    UserRx,
}

/// Rechte einer User-Abbildung (wie aarch64 `UserPerm`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserPerm {
    Ro,
    Rw,
    Rx,
}

const fn perm_bits(p: Perm) -> u64 {
    match p {
        Perm::Rx => P,                    // ausführbar, nur lesbar, Ring 0
        Perm::Ro => P | NX,               //
        Perm::Rw => P | RW | NX,          //
        Perm::UserRw => P | RW | US | NX, //
        Perm::UserRx => P | US,           // ausführbar für Ring 3, nicht schreibbar
    }
}

// --- Tabellen -------------------------------------------------------------------------------

/// Eine 4-KiB-Seitentabelle (512 Einträge).
#[repr(C, align(4096))]
struct Table([u64; 512]);

impl Table {
    const EMPTY: Table = Table([0; 512]);
}

/// Anzahl der mit **4-KiB-Seiten** abgebildeten 2-MiB-Blöcke (= die ersten 16 MiB).
const FINE_BLOCKS: usize = 8;
/// Wie viele GiB mit **eigenen Seitenverzeichnissen** (2-MiB-Granularität) fest abgebildet
/// werden — RAM und MMIO-Fenster **unter 4 GiB**. Auf einer kleinen Maschine ist das alles,
/// was es gibt; die Karte oberhalb davon entsteht erst im Hochlauf (s. Modul-Doku).
const LOW_GIB: usize = 4;

/// **Obergrenze der Identity-Map**: ein PML4-Eintrag spannt 512 GiB, und diese Karte benutzt
/// genau einen (`PML4[0]`, s. `vspace_create_base`). Eine physische Adresse darüber ist hier
/// nicht abbildbar — eine **benannte** Grenze: [`map_device_window_global`] und
/// [`vspace_map_device`] weisen sie ab, statt irgendwo eine Tabelle zu erfinden.
const MAPPED_GIB: usize = 512;

/// Wie viele GiB **oberhalb** von [`LOW_GIB`] ein Geräte-Registerfenster tragen können.
///
/// Vier, weil eine Plattform ihre 64-Bit-BARs praktisch immer in **ein** zusammenhängendes Loch
/// legt (QEMU/q35: alles bei 448 GiB). Reicht es nicht, gibt [`map_device_window_global`]
/// `false` zurück und der Aufrufer sagt es — eine still verkürzte Karte wäre ein #PF, der
/// später und woanders auftritt.
const HIGH_DEV_SLOTS: usize = 4;

static mut PML4: Table = Table::EMPTY;
static mut PDPT: Table = Table::EMPTY;
static mut PD: [Table; LOW_GIB] = [const { Table::EMPTY }; LOW_GIB];
static mut PT: [Table; FINE_BLOCKS] = [const { Table::EMPTY }; FINE_BLOCKS];

/// Seitenverzeichnisse für GiB 1..3 in der **isolierten** Sicht: identisch zur globalen Map,
/// aber **ohne** `US` — der Kernel sieht dort alles, ein Ring-3-Thread einer isolierten PD
/// nichts. Sie sind statisch und werden von **allen** isolierten Adressräumen geteilt (sie
/// enthalten keine PD-spezifischen Einträge), sparen also je Adressraum drei Frames.
static mut ISO_PD_HIGH: [Table; LOW_GIB - 1] = [const { Table::EMPTY }; LOW_GIB - 1];

/// Seitenverzeichnisse für Geräte-GiB **oberhalb** von [`LOW_GIB`] (globale Karte).
///
/// Ebenfalls statisch und geteilt — und deshalb gilt für sie **wörtlich** dasselbe wie für
/// [`ISO_PD_HIGH`]: wer hier einen PD-spezifischen Eintrag hineinschriebe, gäbe ihn jedem
/// isolierten Adressraum. Verhindert wird das nicht durch Sorgfalt an jeder Schreibstelle,
/// sondern durch [`is_static_table`] an der einen Stelle, die entscheidet.
static mut HIGH_DEV_PD: [Table; HIGH_DEV_SLOTS] = [const { Table::EMPTY }; HIGH_DEV_SLOTS];

/// Welches GiB der Platz `k` in [`HIGH_DEV_PD`] trägt (`usize::MAX` = frei).
static HIGH_DEV_GIB: [AtomicUsize; HIGH_DEV_SLOTS] =
    [const { AtomicUsize::new(usize::MAX) }; HIGH_DEV_SLOTS];

/// Wie viele 1-GiB-Blätter oberhalb von [`LOW_GIB`] als RAM übernommen wurden (Bericht).
static HIGH_RAM_GIB: AtomicUsize = AtomicUsize::new(0);

/// Wie oft für ein GiB **oberhalb** von [`LOW_GIB`] eine **private** Kopie eines
/// Seitenverzeichnisses angelegt wurde ([`private_pd`]).
///
/// Der Zähler ist die Sprechprobe zu [`high_shared_audit`]: „keine unzulässigen Einträge in der
/// geteilten Tabelle" ist eine Aussage über gar nichts, solange nie eine PD ein Fenster dort
/// bekommen hat. Erst zusammen wird daraus ein Testergebnis.
static HIGH_DEV_PRIVATE: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    /// Ring-3-Code: eigene Seiten, damit sie `US`+ausführbar sein können, ohne dass der
    /// Kernel-`.text` es wird.
    static __user_text_start: u8;
    static __user_text_end: u8;
    static __user_data_start: u8;
    static __user_data_end: u8;
    static __kernel_end: u8;
}

fn sym(s: &u8) -> u64 {
    s as *const u8 as u64
}

/// Erste freie physische Adresse hinter dem Kernel-Image (inkl. Seitentabellen).
pub fn kernel_end() -> u64 {
    // SAFETY: Linker-Symbol, nur die Adresse wird gelesen.
    let end = unsafe { sym(&__kernel_end) };
    (end + PAGE - 1) & !(PAGE - 1)
}

/// Rechte einer 4-KiB-Seite im Bereich der ersten 16 MiB nach dem Image-Layout bestimmen.
fn fine_perm(pa: u64) -> u64 {
    // SAFETY: Linker-Symbole, nur Adressen.
    let (ts, te, rs, re) = unsafe {
        (
            sym(&__text_start),
            sym(&__text_end),
            sym(&__rodata_start),
            sym(&__rodata_end),
        )
    };
    // SAFETY: wie oben — nur Adressen von Linker-Symbolen.
    let (uts, ute, uds, ude) = unsafe {
        (
            sym(&__user_text_start),
            sym(&__user_text_end),
            sym(&__user_data_start),
            sym(&__user_data_end),
        )
    };
    if pa >= ts && pa < te {
        perm_bits(Perm::Rx) // Kernel-Code: ausführbar, NICHT schreibbar, Ring 0
    } else if pa >= rs && pa < re {
        perm_bits(Perm::Ro) // Konstanten: nur lesbar, NX
    } else if pa >= uts && pa < ute {
        perm_bits(Perm::UserRx) // Ring-3-Code: US + ausführbar, nicht schreibbar (W^X)
    } else if pa >= uds && pa < ude {
        perm_bits(Perm::UserRw) // Ring-3-Daten: US + RW + NX
    } else {
        perm_bits(Perm::Rw) // alles andere: RW, NX, Ring 0
    }
}

/// Ist dieser 2-MiB-Block Geräte-Speicher (MMIO)? Das oberste GiB unter 4 GiB trägt auf dem
/// PC LAPIC (0xFEE0_0000), IOAPIC und die PCI-Fenster.
fn is_device(pa: u64) -> bool {
    pa >= 0xC000_0000
}

/// Seitentabellen aufbauen und aktivieren (Primärkern).
pub fn init_primary() {
    // NX-Bit freischalten (sonst ist Bit 63 reserviert -> #GP beim Laden von CR3).
    const MSR_EFER: u32 = 0xC000_0080;
    const EFER_NXE: u64 = 1 << 11;
    // SAFETY: EFER existiert auf jeder x86_64-CPU; wir setzen nur NXE.
    unsafe {
        let efer = cpu::rdmsr(MSR_EFER);
        cpu::wrmsr(MSR_EFER, efer | EFER_NXE);
    }

    // SAFETY: die Tabellen sind statische, 4-KiB-ausgerichtete Arrays, die ausschließlich hier
    // (beim Boot, einkernig, vor dem Laden von CR3) beschrieben werden.
    unsafe {
        let pml4 = &mut *core::ptr::addr_of_mut!(PML4);
        let pdpt = &mut *core::ptr::addr_of_mut!(PDPT);
        let pd = &mut *core::ptr::addr_of_mut!(PD);
        let pt = &mut *core::ptr::addr_of_mut!(PT);

        // WICHTIG: Die **Zwischenebenen** tragen `US`. Auf x86 ist die effektive Berechtigung
        // die UND-Verknüpfung über alle vier Ebenen — ohne `US` im PML4E/PDPTE/PDE verweigert die
        // CPU jeden Ring-3-Zugriff, egal was im Blatt-Eintrag steht. Die eigentliche Entscheidung
        // fällt damit ausschließlich am Blatt (dort ist `US` nur für User-Seiten gesetzt), genau
        // wie `AP[1]` auf aarch64.
        pml4.0[0] = (pdpt as *const Table as u64) | P | RW | US;
        for (g, table) in pd.iter_mut().enumerate() {
            pdpt.0[g] = (table as *const Table as u64) | P | RW | US;
        }
        // Erste 16 MiB: 4-KiB-Granularität (seitengenaues W^X über das Kernel-Image).
        for (b, table) in pt.iter_mut().enumerate() {
            for (i, e) in table.0.iter_mut().enumerate() {
                let pa = (b as u64) * TWO_MIB + (i as u64) * PAGE;
                *e = pa | fine_perm(pa);
            }
            pd[0].0[b] = (table as *const Table as u64) | P | RW | US;
        }
        // Rest: 2-MiB-Blöcke, RW + NX; MMIO-Bereiche uncacheable.
        for g in 0..LOW_GIB {
            let first = if g == 0 { FINE_BLOCKS } else { 0 };
            for i in first..512 {
                let pa = (g as u64) * ONE_GIB + (i as u64) * TWO_MIB;
                let mut bits = P | RW | NX | PS;
                if is_device(pa) {
                    bits |= PCD | PWT;
                } else if pa >= USER_RAM_MIN {
                    // SAS-Modell (ADR 0002, wie `Perm::UserRw` auf aarch64): das allgemeine
                    // User-RAM ist für Ring 3 les-/schreibbar — Stacks und Daten der
                    // Ring-3-Threads liegen darin. Der Kernel selbst (unter 16 MiB) bleibt
                    // supervisor-only; ein Ring-3-Zugriff dorthin faultet.
                    bits |= US;
                }
                pd[g].0[i] = pa | bits;
            }
        }
        // Isolierte Sicht auf GiB 1..3: dieselben Blöcke, aber ohne `US`.
        let iso = &mut *core::ptr::addr_of_mut!(ISO_PD_HIGH);
        for (k, table) in iso.iter_mut().enumerate() {
            let g = (k + 1) as u64;
            for (i, e) in table.0.iter_mut().enumerate() {
                let pa = g * ONE_GIB + (i as u64) * TWO_MIB;
                let mut bits = P | RW | NX | PS; // kein US -> nur der Kernel sieht es
                if is_device(pa) {
                    bits |= PCD | PWT;
                }
                *e = pa | bits;
            }
        }
        activate(pml4 as *const Table as u64);
    }
}

// --- Die Karte oberhalb von 4 GiB (E-Rest 3) --------------------------------------------------
//
// Beide Funktionen laufen **im Hochlauf, einkernig**: die Sekundärkerne werden erst nach der
// PCI-Enumeration gestartet (s. `bringup::run`). Deshalb genügt ein lokales `flush_all()` und es
// braucht keinen TLB-Shootdown. Zusätzlich gilt: die Einträge waren vorher *nicht präsent*, und
// nicht präsente Einträge legt x86 nicht in den Paging-Structure-Caches ab — ein hinzugefügter
// Eintrag muss also gar nichts verdrängen.

/// Kann diese CPU **1-GiB-Seiten**? (`CPUID.8000_0001h:EDX[26]`, `PDPE1GB`)
///
/// Die Frage wird gestellt und nicht angenommen: unter TCG meldet nicht jedes CPU-Modell das
/// Bit, und ein 1-GiB-Blatt auf einer CPU ohne die Fähigkeit ist ein reservierter Bit-Fehler,
/// also ein #PF beim ersten Zugriff — genau der Fehler, den dieser Abschnitt beseitigen soll.
pub fn gib_pages() -> bool {
    let (max_ext, _, _, _) = cpu::cpuid(0x8000_0000);
    if max_ext < 0x8000_0001 {
        return false;
    }
    let (_, _, _, edx) = cpu::cpuid(0x8000_0001);
    edx & (1 << 26) != 0
}

/// Was oberhalb von [`LOW_GIB`] tatsächlich abgebildet wurde: `(Geräte-GiB, RAM-GiB,
/// 1-GiB-Seiten verfügbar)`. Für den Bootbericht — eine Karte, über die niemand etwas sagen
/// kann, ist von einer falschen nicht zu unterscheiden.
pub fn high_map_report() -> (usize, usize, bool) {
    let dev = HIGH_DEV_GIB
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != usize::MAX)
        .count();
    (dev, HIGH_RAM_GIB.load(Ordering::Acquire), gib_pages())
}

/// Das Seitenverzeichnis für das GiB `g` oberhalb von [`LOW_GIB`] — vorhandenes oder neues.
/// `None`, wenn der Vorrat erschöpft ist.
fn high_dev_pd(g: usize) -> Option<u64> {
    for (k, slot) in HIGH_DEV_GIB.iter().enumerate() {
        if slot.load(Ordering::Acquire) == g {
            // SAFETY: nur die Adresse eines statischen Objekts.
            return Some(unsafe { &(*core::ptr::addr_of!(HIGH_DEV_PD))[k] as *const Table as u64 });
        }
    }
    for (k, slot) in HIGH_DEV_GIB.iter().enumerate() {
        if slot.load(Ordering::Acquire) != usize::MAX {
            continue;
        }
        // SAFETY: statische, 4-KiB-ausgerichtete Tabelle; einkerniger Hochlauf (s. o.).
        let phys = unsafe {
            let t = &mut (*core::ptr::addr_of_mut!(HIGH_DEV_PD))[k];
            for e in t.0.iter_mut() {
                *e = 0;
            }
            t as *const Table as u64
        };
        // SAFETY: die globale PDPT dieses Kernels; `g < MAPPED_GIB == 512` ist geprüft.
        unsafe { (*core::ptr::addr_of_mut!(PDPT)).0[g] = table_desc(phys) };
        slot.store(g, Ordering::Release);
        return Some(phys);
    }
    None
}

/// **Ein Geräte-Registerfenster in die globale Karte aufnehmen** — der Pfad von der
/// BAR-Ermittlung zur Seitentabelle.
///
/// Abgebildet werden die 2-MiB-Blöcke, die `[pa, pa+len)` überdecken: uncacheable,
/// nicht ausführbar, **supervisor-only**. Unter [`LOW_GIB`] ist das ein No-op — dort steht der
/// Block seit [`init_primary`].
///
/// `false` heißt „nicht abbildbar" (oberhalb von [`MAPPED_GIB`] oder Vorrat erschöpft) und ist
/// als Rückgabewert wichtiger als es aussieht: der Aufrufer würde sonst gleich darauf Register
/// lesen und bekäme einen #PF mitten im Hochlauf — also die Fehlerform, wegen der es diese
/// Funktion überhaupt gibt.
pub fn map_device_window_global(pa: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = pa.checked_add(len) else {
        return false;
    };
    if end > (MAPPED_GIB as u64) * ONE_GIB {
        return false;
    }
    let mut blk = pa & !(TWO_MIB - 1);
    while blk < end {
        let g = (blk / ONE_GIB) as usize;
        if g >= LOW_GIB {
            let Some(pd_phys) = high_dev_pd(g) else {
                return false;
            };
            let idx = ((blk % ONE_GIB) / TWO_MIB) as usize;
            // SAFETY: `pd_phys` ist eine statische, identity-gemappte Tabelle dieses Moduls;
            // `idx < 512`.
            unsafe { table_mut(pd_phys)[idx] = blk | P | RW | NX | PS | PCD | PWT };
        }
        blk += TWO_MIB;
    }
    flush_all();
    true
}

/// **RAM oberhalb von 4 GiB in die globale Karte aufnehmen** — 1-GiB-Blätter, cacheable, `US`
/// (SAS-Modell wie unter 4 GiB oberhalb von [`USER_RAM_MIN`]).
///
/// Übernommen wird nur, was ein 1-GiB-Blatt **vollständig** trägt. Ein Blatt, das zur Hälfte
/// über Adressen läge, die der Speicherplan nicht als RAM meldet, wäre eine Zusage über
/// Speicher, den niemand zugesagt hat — und auf echter Hardware liegt dort gerne MMIO.
///
/// Zurück kommt der übernommene Bereich, und der Aufrufer gibt dem Allokator **genau den**.
/// Damit ist „was der Allokator ausgeben darf" dieselbe Zahl wie „was die Karte als RAM
/// ausweist", und nicht zwei Zahlen, die zueinander passen müssen.
///
/// `None`, wenn nichts übernommen werden konnte — insbesondere ohne [`gib_pages`].
pub fn adopt_high_ram(base: u64, len: u64) -> Option<(u64, u64)> {
    if !gib_pages() {
        return None;
    }
    let end = base.checked_add(len)?;
    let first = base.div_ceil(ONE_GIB).max(LOW_GIB as u64);
    let last = (end / ONE_GIB).min(MAPPED_GIB as u64);
    if last <= first {
        return None;
    }
    let mut taken = first;
    for g in first..last {
        // SAFETY: die globale PDPT; `g < 512`.
        let occupied = unsafe { (*core::ptr::addr_of!(PDPT)).0[g as usize] & P != 0 };
        if occupied {
            break; // schon vergeben (Gerätefenster) -- nicht überschreiben, sondern hier enden
        }
        // SAFETY: wie oben; ein 1-GiB-Blatt ist erlaubt, weil `gib_pages()` geprüft ist.
        unsafe {
            (*core::ptr::addr_of_mut!(PDPT)).0[g as usize] =
                (g * ONE_GIB) | P | RW | NX | PS | US;
        }
        taken = g + 1;
    }
    if taken == first {
        return None;
    }
    HIGH_RAM_GIB.fetch_add((taken - first) as usize, Ordering::Release);
    flush_all();
    Some((first * ONE_GIB, (taken - first) * ONE_GIB))
}

/// **Audit der geteilten Geräte-Seitenverzeichnisse oberhalb 4 GiB** —
/// `(benutzte Tabellen, private Kopien, unzulässige Einträge)`.
///
/// In [`HIGH_DEV_PD`] darf **ausschließlich** stehen, was [`map_device_window_global`]
/// hineinschreibt: ein 2-MiB-Geräteblock (`P|RW|NX|PS|PCD|PWT`) oder nichts. Jeder andere
/// Eintrag — insbesondere einer mit `US` oder ein Tabellenverweis — heißt, dass ein
/// PD-spezifisches Fenster in eine **geteilte** Tabelle geschrieben wurde. Die Folge steht in
/// CLAUDE.md: aus einer Zuteilung an einen Treiber würde ein Zugriff für **jede** isolierte PD,
/// und zwar lautlos, weil die Cap-Prüfung dabei korrekt durchläuft.
///
/// Das ist wörtlich die Widerlegung der bequemen Abkürzung: wer sich in [`private_pd`] die
/// Kopie spart, fällt hier durch. Die **private Kopien** stehen mit im Ergebnis, weil die
/// Aussage sonst über nichts urteilt: keine Kopie heißt, dass nie eine PD ein Fenster dort
/// bekommen hat, und dann ist „0 unzulässige Einträge" kein Testergebnis.
///
/// **`A`/`D` bleiben ausserhalb des Vergleichs**, und das war der erste Befund dieser Funktion an
/// sich selbst: sie meldete `unzulaessige Eintraege=1` auf einem tadellosen Aufbau. Der Eintrag
/// war `0x8000_0070_0000_00fb` — `US` **nicht** gesetzt, dafuer Accessed und Dirty, gesetzt von
/// der HARDWARE, weil der Kernel die Register gelesen hatte. Ein Pruefer, der Bits vergleicht,
/// die ihm nicht gehoeren, misst die Benutzung statt der Zuteilung.
pub fn high_shared_audit() -> (usize, usize, usize) {
    const ERLAUBT: u64 = P | RW | NX | PS | PCD | PWT;
    let mut tabellen = 0usize;
    let mut bad = 0usize;
    for (k, slot) in HIGH_DEV_GIB.iter().enumerate() {
        if slot.load(Ordering::Acquire) == usize::MAX {
            continue;
        }
        tabellen += 1;
        // SAFETY: statische Tabelle dieses Moduls, nur gelesen.
        let t = unsafe { &(*core::ptr::addr_of!(HIGH_DEV_PD))[k] };
        for &e in t.0.iter() {
            if e == 0 {
                continue;
            }
            if e & !0x000f_ffff_ffff_f000 & !(A | D) != ERLAUBT {
                bad += 1;
            }
        }
    }
    (tabellen, HIGH_DEV_PRIVATE.load(Ordering::Acquire), bad)
}

/// **Eine Adresse durch die globale Karte auflösen** — der Beleg, dass eine Abbildung steht.
///
/// Gibt die physische Adresse, auf die `va` in der globalen Identity-Map zeigt, oder `None`,
/// wenn dort nichts abgebildet ist. Alle vier Ebenen werden gelaufen, 1-GiB- und 2-MiB-Blätter
/// eingeschlossen.
///
/// Warum das eine eigene Funktion ist und nicht „folgt aus der Konstruktion": nach einem
/// `map_device_window_global` ist die Aussage „das Fenster ist jetzt da" sonst nur eine
/// Behauptung des Aufbaus. Mit dieser Funktion ist sie prüfbar — **und** widerlegbar: dieselbe
/// Frage an ein GiB, das niemand abgebildet hat, muss `None` liefern. Eine flächige Karte
/// (der bequeme Entwurf) würde daran scheitern.
pub fn resolve_global(va: u64) -> Option<u64> {
    const MASK: u64 = 0x000f_ffff_ffff_f000;
    // SAFETY: nur Lesezugriffe auf die eigenen, identity-gemappten Seitentabellen.
    unsafe {
        let e4 = (*core::ptr::addr_of!(PML4)).0[((va >> 39) & 0x1ff) as usize];
        if e4 & P == 0 {
            return None;
        }
        let e3 = table_mut(e4 & MASK)[((va >> 30) & 0x1ff) as usize];
        if e3 & P == 0 {
            return None;
        }
        if e3 & PS != 0 {
            return Some((e3 & MASK & !(ONE_GIB - 1)) | (va & (ONE_GIB - 1)));
        }
        let e2 = table_mut(e3 & MASK)[((va >> 21) & 0x1ff) as usize];
        if e2 & P == 0 {
            return None;
        }
        if e2 & PS != 0 {
            return Some((e2 & MASK & !(TWO_MIB - 1)) | (va & (TWO_MIB - 1)));
        }
        let e1 = table_mut(e2 & MASK)[((va >> 12) & 0x1ff) as usize];
        if e1 & P == 0 {
            return None;
        }
        Some((e1 & MASK) | (va & (PAGE - 1)))
    }
}

/// Gehört dieser Rahmen zu den **statischen** Tabellen dieses Moduls?
///
/// Der Test ist absichtlich strukturell und nicht „ist es genau die eine geteilte Tabelle":
/// eine statische Tabelle darf **nie** PD-spezifisch beschrieben werden (sie ist geteilt) und
/// **nie** freigegeben werden (sie ist Kernel-Speicher, kein Allokat — eine Freigabe gäbe dem
/// Allokator das Kernel-Image als freies RAM zurück). Beide Zusagen hängen an derselben Frage,
/// also steht sie an **einer** Stelle. Ein Vergleich gegen eine einzelne Tabelle wäre nach der
/// nächsten hinzugefügten Tabelle wieder unvollständig — und zwar lautlos.
fn is_static_table(phys: u64) -> bool {
    let in_range = |start: u64, n: usize| phys >= start && phys < start + (n as u64) * PAGE;
    // Nur die **Adressen** statischer Objekte -- `addr_of!` liest nichts.
    in_range(core::ptr::addr_of!(PML4) as u64, 1)
        || in_range(core::ptr::addr_of!(PDPT) as u64, 1)
        || in_range(core::ptr::addr_of!(PD) as u64, LOW_GIB)
        || in_range(core::ptr::addr_of!(PT) as u64, FINE_BLOCKS)
        || in_range(core::ptr::addr_of!(ISO_PD_HIGH) as u64, LOW_GIB - 1)
        || in_range(core::ptr::addr_of!(HIGH_DEV_PD) as u64, HIGH_DEV_SLOTS)
        // **Die Wachen-Tabellen gehoeren dazu, und die Modul-Doku sagt warum:** wer eine neue
        // statische Tabelle einfuehrt und sie hier vergisst, gibt dem Allokator Kernel-Speicher
        // als freies RAM zurueck -- lautlos. Genau der Satz, der hier schon stand, bevor es
        // `GUARD_PT` gab.
        || in_range(core::ptr::addr_of!(GUARD_PT) as u64, GUARD_BLOCKS)
}

/// **Rechte einer einzelnen 4-KiB-Seite ändern** (nur im fein abgebildeten Bereich < 16 MiB).
///
/// Gebraucht für die AP-Trampolin-Seite: sie muss beim Boot **beschreibbar** sein (der BSP
/// kopiert den Block hinein) und danach **ausführbar, nicht schreibbar** (der Sekundärkern
/// führt dort weiter, nachdem er `CR0.PG` gesetzt hat — läge sie dann NX, gäbe es genau in
/// diesem Moment einen #PF ohne IDT und damit einen Triple Fault). Beides gleichzeitig wäre
/// eine W^X-Verletzung; der Wechsel ist die saubere Auflösung.
///
/// Gibt `false` für Adressen außerhalb des 4-KiB-granularen Bereichs oder unausgerichtete.
pub fn protect_page(pa: u64, perm: Perm) -> bool {
    if pa % PAGE != 0 || pa >= (FINE_BLOCKS as u64) * TWO_MIB {
        return false;
    }
    let block = (pa / TWO_MIB) as usize;
    let idx = ((pa % TWO_MIB) / PAGE) as usize;
    // SAFETY: `PT` ist die statische Seitentabelle dieses Kernels; Block-/Index-Bereich ist eben
    // geprüft. Der Eintrag wird atomar (ein 64-bit-Store) ersetzt und die Adresse danach aus dem
    // TLB geworfen.
    unsafe {
        let pt = &mut *core::ptr::addr_of_mut!(PT);
        pt[block].0[idx] = pa | perm_bits(perm);
    }
    flush_va_global(pa);
    true
}

// ==============================================================================================
// GUARD-PAGES AM KERNEL-STACK -- und warum dafuer ein 2-MiB-Block AUFGETEILT werden muss
// ==============================================================================================
//
// Am 2026-08-10 gemessen: der tiefste Kernelpfad (`SYS_LOAD` -> Ed25519 + SHA-2 **im Kernel**)
// benutzt **73 %** des 16-KiB-EL1-Stacks eines EL0-Threads. Reserve: 4376 Byte. Ohne Wache
// schreibt ein Ueberlauf **still** in den Nachbarn -- und weil der Pfad aus Ring 3 erreichbar
// ist, ist das keine Diagnosefrage, sondern eine **Privilegieneskalation**.
//
// Die Wache ist eine **nicht abgebildete** Seite unterhalb des Stacks. Sie muss in der
// **Identitaetskarte** fehlen, denn genau ueber die greift der Kernel auf seinen Stack zu.
//
// **Die Vorarbeit, die es dafuer braucht:** Kernel-Stacks kommen aus `mem_alloc`, dessen Freiliste
// bei 16 MiB beginnt -- also **oberhalb** des mit 4-KiB-Seiten abgebildeten Bereichs
// ([`FINE_BLOCKS`], die ersten 16 MiB). Dort steht die Karte in **2-MiB-Bloecken** (`PS`), und
// `protect_page` weist Adressen darueber ausdruecklich ab. Eine 4-KiB-Luecke verlangt also, den
// Block in eine Seitentabelle **aufzuteilen**: 512 Eintraege mit denselben Rechten schreiben, den
// PD-Eintrag auf die Tabelle zeigen lassen, TLB werfen. Danach ist derselbe Block 4-KiB-granular,
// und jede weitere Wache darin kostet nur noch einen Eintrag.
//
// **Der Vorrat ist FEST, und das ist eine benannte Grenze, keine Ueberraschung.** Die HAL hat
// keinen Allokator (Kerngrenze), die Tabellen sind statisch. [`GUARD_PT`] traegt
// [`GUARD_BLOCKS`] aufgeteilte Bloecke; reicht er nicht, gibt [`guard_unmap`] `false` zurueck und
// der Aufrufer weist die Stack-Anforderung **benannt** ab. Fail-closed: ein Stack ohne Wache
// waere die stille Fassung des Fehlers, gegen den diese Funktion gebaut ist.
//
// Was das NICHT deckt, steht als offener Punkt im Register: bei 10 000 Threads reichen die
// Bloecke nicht, weil die Stacks ueber den ganzen Speicher streuen. Die strukturelle Antwort
// waere eine **Stack-Arena** -- ein zusammenhaengender Bereich, dessen Bloecke einmal aufgeteilt
// werden. Das ist ein Umbau der Stack-Zuteilung und nicht dieser Patch.

/// Wie viele 2-MiB-Bloecke fuer Wachen aufgeteilt werden koennen.
///
/// **16 ist keine runde Zahl, sondern eine hergeleitete:** ein 16-KiB-Stack mit Wache belegt
/// 20 KiB, in einen 2-MiB-Block passen also gut 100 davon. 16 Bloecke tragen damit rund
/// **1600 bewachte Stacks** -- deutlich mehr als die 16 Threads eines Suitenlaufs und mehr als
/// die 224..1477 einer isolierten Kapazitaetskurve, aber **weniger als 10 000**. Genau deshalb
/// steht der Fuellstand im Bericht: ein fester Vorrat ohne Anzeige spricht erst, wenn er leer
/// ist, und dann an einer Stelle, die mit der Ursache nichts zu tun hat.
const GUARD_BLOCKS: usize = 16;

static mut GUARD_PT: [Table; GUARD_BLOCKS] = [const { Table::EMPTY }; GUARD_BLOCKS];

/// Welchen 2-MiB-Block der Platz `k` traegt (`usize::MAX` = frei).
static GUARD_BLOCK_OF: [AtomicUsize; GUARD_BLOCKS] =
    [const { AtomicUsize::new(usize::MAX) }; GUARD_BLOCKS];

/// Wie viele Wachen zurzeit stehen (Bericht/Bilanz).
static GUARDS_LIVE: AtomicUsize = AtomicUsize::new(0);
/// Wie viele Wachen insgesamt gesetzt wurden (Sprechprobe: 0 heisst „nie benutzt").
static GUARDS_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Wie oft eine Wache **nicht** gesetzt werden konnte (Vorrat erschoepft).
static GUARDS_DENIED: AtomicUsize = AtomicUsize::new(0);

/// **Die Adresse EINER zurzeit stehenden Wache** -- oder `None`, wenn keine steht.
///
/// Gebraucht von der `#DF`-Sonde: sie muss auf eine Seite schreiben, die **nachweislich** nicht
/// abgebildet ist. Am 2026-08-11 hat sie stattdessen `TSS.rsp0` benutzt und daraus den Stackfuss
/// gerechnet -- und traf einen Stack, dessen Thread laengst tot und dessen Wache damit wieder
/// eingehaengt war. Beide `push` gingen durch, der Kernel lief in ein `#UD`, und die Sonde haette
/// bei einer schwaecheren Auswertung „kein #DF" als „die Kette traegt nicht" gemeldet.
///
/// Der Umweg ueber eine gerechnete Adresse ist damit dieselbe Falle wie ein Pruefer, der die
/// gepruefte Groesse nachrechnet statt sie zu lesen: hier wird die Wache **gelesen**.
pub fn erste_lebende_wache() -> Option<u64> {
    for (k, b) in GUARD_BLOCK_OF.iter().enumerate() {
        let blk = b.load(Ordering::Relaxed);
        if blk == usize::MAX {
            continue;
        }
        // SAFETY: statische Tabelle, Platz `k` ist belegt (eben gelesen); nur Lesen.
        let pt = unsafe { &*core::ptr::addr_of!(GUARD_PT) };
        for (i, e) in pt[k].0.iter().enumerate() {
            if *e & P == 0 {
                return Some((blk as u64) * TWO_MIB + (i as u64) * PAGE);
            }
        }
    }
    None
}

/// **Ist `pa` zurzeit eine stehende Wache?** -- gelesen, nicht gerechnet.
///
/// Dafuer gebaut, dass der `#DF`-Bericht `CR2` deuten kann: liegt die Adresse auf einer Wache,
/// war der Erstfault ein Stackueberlauf und `CR2` ist gueltig. Der erste Anlauf verglich `CR2`
/// gegen eine aus `TSS.rsp0` **gerechnete** Wachenadresse -- und meldete „passt nicht", obwohl
/// der Ueberlauf gerade ueber eine echte Wache gelaufen war: `rsp0` zeigte auf den Stack eines
/// laengst toten Threads. Zweimal am selben Tag dieselbe Lehre: **lesen statt nachrechnen.**
pub fn ist_wache(pa: u64) -> bool {
    let blk = (pa / TWO_MIB) as usize;
    let Some(k) = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Relaxed) == blk)
    else {
        return false;
    };
    let idx = ((pa % TWO_MIB) / PAGE) as usize;
    // SAFETY: statische Tabelle, Block belegt, Index aus dem Offset -- nur Lesen.
    let pt = unsafe { &*core::ptr::addr_of!(GUARD_PT) };
    pt[k].0[idx] & P == 0
}

/// `(stehende Wachen, insgesamt gesetzte, abgewiesene, belegte Bloecke, Blockvorrat)`.
pub fn guard_stats() -> (usize, usize, usize, usize, usize) {
    let belegt = GUARD_BLOCK_OF
        .iter()
        .filter(|b| b.load(Ordering::Relaxed) != usize::MAX)
        .count();
    (
        GUARDS_LIVE.load(Ordering::Relaxed),
        GUARDS_TOTAL.load(Ordering::Relaxed),
        GUARDS_DENIED.load(Ordering::Relaxed),
        belegt,
        GUARD_BLOCKS,
    )
}

/// Den 2-MiB-Block von `pa` 4-KiB-granular machen; gibt den Platz in [`GUARD_PT`] zurueck.
///
/// Ist der Block schon aufgeteilt, wird sein Platz wiederverwendet -- der zweite Stack in
/// demselben Block kostet keine zweite Tabelle.
fn guard_block(pa: u64) -> Option<usize> {
    let blk = (pa / TWO_MIB) as usize;
    // Unterhalb von `FINE_BLOCKS` ist die Karte ohnehin fein -- dort gehoert kein Stack hin, und
    // eine Aufteilung waere ein Doppelbild derselben Seiten.
    if blk < FINE_BLOCKS || pa >= LOW_MAPPED_END {
        return None;
    }
    if let Some(k) = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Relaxed) == blk)
    {
        return Some(k);
    }
    let k = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Relaxed) == usize::MAX)?;
    let basis = (blk as u64) * TWO_MIB;
    // SAFETY: `GUARD_PT` ist eine statische Tabelle dieses Kernels, der Platz `k` ist eben als
    // frei ermittelt und wird nur hier belegt.
    //
    // **Die Rechte werden aus dem vorhandenen Eintrag GELESEN, nicht hingeschrieben.** Die erste
    // Fassung setzte fest `P | RW | NX` -- und nahm damit jedem aufgeteilten Block sein `US`.
    // Oberhalb `USER_RAM_MIN` traegt die Identitaetskarte `US` (SAS-Modell, ADR 0002: Stacks und
    // Daten der Ring-3-Threads liegen darin), und ohne das Bit faultet ein Ring-3-Thread auf
    // seinem EIGENEN Stack. Gemessen: sechs `el0-trap`-Zeilen, `ring3`/`iso`/`park` rot.
    // Eine Aufteilung darf die Sicht nicht aendern, nur ihre Koernung -- also ist der alte
    // Eintrag die Quelle und nicht eine zweite Meinung darueber, was dort stehen sollte.
    unsafe {
        let pd = &mut *core::ptr::addr_of_mut!(PD);
        let gib = blk / 512;
        let idx = blk % 512;
        let alt = pd[gib].0[idx];
        if alt & P == 0 || alt & PS == 0 {
            return None; // nicht abgebildet oder schon aufgeteilt -- beides ist hier ein Fehler
        }
        // `PS` faellt weg (auf Blattebene bedeutet dasselbe Bit PAT), die Adresse kommt aus dem
        // Index; alles andere bleibt, wie es war.
        let rechte = alt & (P | RW | US | PWT | PCD | NX);
        let pt = &mut *core::ptr::addr_of_mut!(GUARD_PT);
        for (i, e) in pt[k].0.iter_mut().enumerate() {
            *e = (basis + (i as u64) * PAGE) | rechte;
        }
        // **Erst die Tabelle fuellen, dann den Verweis setzen** -- in der anderen Reihenfolge
        // saehe ein anderer Kern fuer einen Augenblick eine halb gefuellte Tabelle.
        // **`US` gehoert an die ZWISCHENEBENE.** Auf x86 ist die effektive Berechtigung die
        // UND-Verknuepfung ueber alle vier Ebenen: bisher WAR dieser Eintrag das Blatt und trug
        // sein `US` selbst; jetzt ist er ein Verweis, und ohne `US` verweigert die CPU jeden
        // Ring-3-Zugriff auf den ganzen 2-MiB-Block -- unabhaengig davon, was im Blatt steht.
        // Gemessen, weil ich es zuerst weggelassen habe: fuenf `el0-trap`-Zeilen, Ring-3-Threads
        // faulteten auf ihrem eigenen Stack, `ring3`/`iso`/`park` rot. Die Warnung stand
        // woertlich zwanzig Zeilen ueber `init_primary` -- und ich habe sie beim Schreiben der
        // Aufteilung nicht mitgelesen. Entschieden wird weiterhin am Blatt (dort steht `US` nur
        // fuer User-Seiten), also aendert das die Sicht nicht, sondern erhaelt sie.
        pd[gib].0[idx] = (core::ptr::addr_of!(pt[k]) as u64) | P | RW | US; // kein PS mehr
    }
    GUARD_BLOCK_OF[k].store(blk, Ordering::Relaxed);
    flush_all();
    Some(k)
}

/// **Eine Wache setzen**: die 4-KiB-Seite bei `pa` aus der Identitaetskarte nehmen.
///
/// `false` heisst „konnte nicht" -- der Aufrufer muss die Anforderung dann **abweisen**. Ein
/// stillschweigend unbewachter Stack waere genau der Zustand, den diese Funktion verhindern soll.
pub fn guard_unmap(pa: u64) -> bool {
    if pa % PAGE != 0 {
        return false;
    }
    let Some(k) = guard_block(pa) else {
        GUARDS_DENIED.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let idx = ((pa % TWO_MIB) / PAGE) as usize;
    // SAFETY: Tabelle und Index sind eben ermittelt; ein 64-bit-Store, danach TLB-Wurf.
    unsafe {
        let pt = &mut *core::ptr::addr_of_mut!(GUARD_PT);
        pt[k].0[idx] = 0; // nicht present -> jeder Zugriff faultet
    }
    flush_va_global(pa);
    GUARDS_LIVE.fetch_add(1, Ordering::Relaxed);
    GUARDS_TOTAL.fetch_add(1, Ordering::Relaxed);
    true
}

/// Eine Wache wieder aufheben (beim Freigeben des Stacks -- sonst gaebe der Allokator eine
/// Seite zurueck, die niemand mehr erreichen kann).
pub fn guard_remap(pa: u64) -> bool {
    if pa % PAGE != 0 {
        return false;
    }
    let blk = (pa / TWO_MIB) as usize;
    let Some(k) = GUARD_BLOCK_OF
        .iter()
        .position(|b| b.load(Ordering::Relaxed) == blk)
    else {
        return false;
    };
    let idx = ((pa % TWO_MIB) / PAGE) as usize;
    // SAFETY: wie oben. **Die Rechte kommen aus einem NACHBAREINTRAG derselben Tabelle**, nicht
    // aus einer Konstante -- aus demselben Grund wie beim Aufteilen: was dort steht, ist die
    // Quelle. Ein Nachbar existiert immer, denn eine Wache belegt nie die ganze Tabelle.
    unsafe {
        let pt = &mut *core::ptr::addr_of_mut!(GUARD_PT);
        let nachbar = if idx + 1 < 512 { idx + 1 } else { idx - 1 };
        let rechte = pt[k].0[nachbar] & (P | RW | US | PWT | PCD | NX);
        pt[k].0[idx] = pa | rechte;
    }
    flush_va_global(pa);
    GUARDS_LIVE.fetch_sub(1, Ordering::Relaxed);
    true
}

/// CR3 auf `root` setzen und `CR0.WP` erzwingen.
///
/// **`CR0.WP` ist sicherheitskritisch:** ohne dieses Bit ignoriert Ring 0 das `RW`-Bit der
/// Seitentabellen — die W^X-Trennung gälte dann nur für Ring 3, und ein Kernel-Bug könnte
/// `.text` überschreiben.
fn activate(root: u64) {
    // SAFETY: `root` zeigt auf die eben aufgebaute, gültige PML4. Das Laden von CR3/CR0 ist
    // eine erlaubte Low-Level-Domäne und serialisiert selbst.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        const CR0_WP: u64 = 1 << 16;
        core::arch::asm!("mov cr0, {}", in(reg) cr0 | CR0_WP, options(nomem, nostack, preserves_flags));
    }
}

/// Sekundärkern: dieselbe (globale) Tabelle aktivieren.
pub fn init_secondary() {
    // SAFETY: die Tabelle wurde vom Primärkern fertig aufgebaut (Reihenfolge über den
    // SMP-Bring-up sichergestellt).
    unsafe { activate(core::ptr::addr_of!(PML4) as u64) };
}

/// Status für den Boot-Report (aarch64: `SCTLR_EL1`-Flags M/C/I).
///
/// x86: Paging ist im Long Mode immer an; Caches sind an, wenn `CR0.CD` **nicht** gesetzt
/// ist. Zusätzlich melden wir `CR0.WP` als drittes Flag (die Entsprechung zu „I": erst damit
/// wirkt W^X auch gegen Ring 0).
pub fn sctlr_flags() -> (bool, bool, bool) {
    let cr0: u64;
    // SAFETY: reines Lesen eines Steuerregisters.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags)) };
    const CR0_CD: u64 = 1 << 30;
    const CR0_WP: u64 = 1 << 16;
    (true, cr0 & CR0_CD == 0, cr0 & CR0_WP != 0)
}

/// Wurzel der globalen Identity-Map (aarch64: `global_root`) — der CR3-Wert.
pub fn global_root() -> u64 {
    core::ptr::addr_of!(PML4) as u64
}

/// Adressraum wechseln (aarch64: `TTBR0` + ASID; hier CR3 + PCID).
///
/// Solange es keine isolierten Adressräume gibt (s. Modul-Doku), wird nur die globale Wurzel
/// gesetzt; ein Wechsel auf `root == global_root()` ist ein No-Op.
pub fn set_user_vspace(root: u64, _asid: u16) {
    let cur: u64;
    // SAFETY: Lesen/Schreiben von CR3 (Adressraumwechsel) — erlaubte Domäne; `root` ist
    // entweder die globale Wurzel oder eine vom Kernel aufgebaute Tabelle.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cur, options(nomem, nostack, preserves_flags));
        if cur & !0xfff != root & !0xfff {
            core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
        }
    }
}

/// Größte nutzbare Adressraum-Kennung.
///
/// Auf ARM ist die ASID ein **Hardware-Tag** im TLB: ein Adressraumwechsel muss nicht flushen,
/// und eine zu große ASID würde auf eine andere aliasen (Isolationsbruch). Auf x86 gäbe es
/// dafür PCIDs; hier ist die Kennung vorerst eine reine **Software**-Nummer des Kernels, und
/// jeder `CR3`-Wechsel flusht den TLB ohnehin vollständig. Damit kann sie nicht aliasen — die
/// Obergrenze ist reine Buchhaltung. (PCID nachzurüsten ist eine reine Optimierung, s. TODO.md.)
pub fn max_asid() -> u16 {
    u16::MAX
}

/// TLB-Einträge eines Adressraums verwerfen (aarch64: `tlbi aside1is`).
pub fn flush_asid(_asid: u16) {
    flush_all();
}

/// Eine einzelne Adresse global aus dem TLB werfen.
pub fn flush_va_global(va: u64) {
    // SAFETY: `invlpg` invalidiert nur einen TLB-Eintrag; keine Speicherwirkung.
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) };
}

/// Kompletten TLB verwerfen (CR3 neu laden).
fn flush_all() {
    let cr3: u64;
    // SAFETY: CR3-Round-Trip verwirft die nicht-globalen TLB-Einträge.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }
}

// --- DMA-Cache-Wartung ------------------------------------------------------------------------

/// **Cache Writeback Granule** — die Ausrichtung, die ein DMA-Puffer braucht, damit
/// Cache-Wartung keine fremden Daten in einer angebrochenen Zeile trifft.
///
/// Auf x86 ist DMA **hardware-kohärent** (Snooping): es gibt keine Wartung, also auch keine
/// Granularitätsbedingung. `1` heißt „jede Ausrichtung ist zulässig" — die Prüfung in
/// `install_dma_cap` ist damit hier trivial erfüllt, statt dass sie ARM-spezifisch danebensteht.
pub fn dma_granule() -> u64 {
    1
}

/// Gegenstück zur ARM-Fassung (dort wird `CTR_EL0` je Kern eingerechnet). Auf x86 ist DMA
/// hardware-kohärent — es gibt keine Wartungsgranularität und damit nichts zu ermitteln.
pub fn record_cache_granule() {}

/// Gegenstück zur ARM-Fassung — hier ohne Wirkung (Granule ist konstant 1).
pub fn seal_cache_granule() {}

/// Auf x86 sind DMA-Zugriffe **hardware-kohärent** zum CPU-Cache (Snooping) — anders als auf
/// aarch64 ist hier keine Cache-Wartung nötig. Die Barriere stellt nur die Reihenfolge sicher.
pub fn dma_cache_clean(_va: u64, _len: u64) {
    cpu::dsb_sy();
}

/// Siehe [`dma_cache_clean`].
pub fn dma_cache_invalidate(_va: u64, _len: u64) {
    cpu::dsb_sy();
}

// --- Per-Prozess-Adressräume (ext-33) ---------------------------------------------------------
//
// Ein **isolierter** Adressraum entsteht aus zwei vom Kernel gelieferten Frames plus einem, den
// die HAL selbst anfordert. Der Aufbau spiegelt exakt das ARM-Modell: **der Kernel sieht alles,
// der User nichts** — und in diese Grundfläche werden die Frames der PD als User-Seiten
// „hineingestanzt".
//
// ```text
//   PML4 (l1) ──[0]──> PDPT (alloc) ──[0]──> PD (l2)   GiB 0: Kernel-Image (geteilte PTs)
//                                    │                        + RAM supervisor-only
//                                    │                        + User-Blöcke der PD (US)
//                                    └─[1..3]─> ISO_PD_HIGH   GiB 1..3 supervisor-only (statisch)
//  ```
//
// Die ersten 16 MiB zeigen auf **dieselben** PTs wie die globale Map: dort liegen Kernel-Code
// (supervisor) und `.user_text`/`.user_data` (US) — genau wie auf ARM die geteilte Kernel-L3.

/// Tabelleneintrag, der auf eine tiefere Ebene zeigt. Zwischenebenen sind **permissiv**
/// (`US|RW`); die Entscheidung fällt am Blatt (s. `init_primary`).
const fn table_desc(phys: u64) -> u64 {
    phys | P | RW | US
}

/// 2-MiB-Block, den **nur der Kernel** sieht (ARM: `kernel_block`).
const fn kernel_block(pa: u64) -> u64 {
    pa | P | RW | NX | PS
}

/// 2-MiB-Block als **User-RW** (ARM: `user_block`).
const fn user_block(pa: u64) -> u64 {
    pa | P | RW | US | NX | PS
}

/// 2-MiB-Block als **User-RX** (privat geladener Code, W^X).
const fn user_code_block(pa: u64) -> u64 {
    pa | P | US | PS
}

/// Eine Tabelle als Slice über ihre physische (identity-gemappte) Adresse.
///
/// # Safety
/// `phys` muss auf eine gültige, 4-KiB-ausgerichtete Seitentabelle zeigen, die in der aktuell
/// aktiven Map beschreibbar ist (der Aufrufer läuft in der globalen Identity-Map).
unsafe fn table_mut(phys: u64) -> &'static mut [u64] {
    unsafe { core::slice::from_raw_parts_mut(phys as *mut u64, 512) }
}

/// Index des 2-MiB-Blocks für `phys` innerhalb von GiB 0 (dort liegen die User-Regionen).
/// `None`, wenn `phys` außerhalb von `[USER_RAM_MIN, GIB1_END)` oder unausgerichtet ist.
fn pd_block_index(phys: u64) -> Option<usize> {
    if phys < USER_RAM_MIN || phys >= GIB1_END || phys % TWO_MIB != 0 {
        return None;
    }
    Some((phys / TWO_MIB) as usize)
}

/// **Grundgerüst eines isolierten Adressraums** anlegen (ARM: `vspace_create_base`).
///
/// `l1_phys` wird die PML4, `l2_phys` das Seitenverzeichnis für GiB 0; die PDPT dazwischen
/// kommt aus `alloc` (x86 hat eine Ebene mehr als ARM). Gibt `false`, wenn `alloc` nichts
/// liefert — dann wurde **nichts** verändert.
pub fn vspace_create_base(
    l1_phys: u64,
    l2_phys: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    let Some(pdpt_phys) = alloc() else {
        return false;
    };
    // SAFETY: frisch allozierte, 4-KiB-ausgerichtete RAM-Frames; der Aufrufer läuft in der
    // globalen Identity-Map, in der sie beschreibbar sind. Genau drei Tabellen werden gefüllt.
    unsafe {
        let pd = table_mut(l2_phys);
        let global_pt = &*core::ptr::addr_of!(PT);
        for (i, e) in pd.iter_mut().enumerate() {
            *e = if i < FINE_BLOCKS {
                // Kernel-Image + `.user_text`/`.user_data`: dieselben PTs wie global (geteilt).
                table_desc(&global_pt[i] as *const Table as u64)
            } else {
                kernel_block((i as u64) * TWO_MIB) // RAM: nur der Kernel
            };
        }
        let pdpt = table_mut(pdpt_phys);
        let iso_high = &*core::ptr::addr_of!(ISO_PD_HIGH);
        let global = &*core::ptr::addr_of!(PDPT);
        for (g, e) in pdpt.iter_mut().enumerate() {
            *e = if g == 0 {
                table_desc(l2_phys)
            } else if g < LOW_GIB {
                table_desc(&iso_high[g - 1] as *const Table as u64)
            } else {
                // **Oberhalb von 4 GiB spiegelt der isolierte Adressraum die globale Karte —
                // ohne `US`.** Beide Hälften des Satzes tragen:
                //
                // *Spiegeln*, weil der Kernel beim Syscall in **diesem** Adressraum läuft. Läge
                // hier `0`, faultete er, sobald er hohes RAM oder ein hohes Registerfenster
                // anfasst — an einer Stelle, die mit dieser PD nichts zu tun hat.
                //
                // *Ohne `US`*, weil das die Zuteilung ist. Ein Ring-3-Thread dieser PD sieht
                // oberhalb von 4 GiB nichts, solange ihm nicht ausdrücklich eine Seite gegeben
                // wurde — und die entsteht in einer **privaten** Kopie (`private_pd`), nie in
                // der geteilten Tabelle. `US` an der Zwischenebene stellt `private_pd` mit
                // `table_desc` wieder her; die Entscheidung fällt also am Blatt, wie überall
                // sonst in dieser Datei.
                global.0[g] & !US
            };
        }
        let pml4 = table_mut(l1_phys);
        for e in pml4.iter_mut() {
            *e = 0;
        }
        pml4[0] = table_desc(pdpt_phys);
    }
    cpu::dsb_sy();
    true
}

/// Einen 2-MiB-Frame als **User-RW** in den Adressraum mit Seitenverzeichnis `l2_phys` mappen.
pub fn vspace_map_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = pd_block_index(phys) else {
        return false;
    };
    // SAFETY: gültiges, in der globalen Map beschreibbares Seitenverzeichnis.
    unsafe { table_mut(l2_phys)[idx] = user_block(phys) };
    cpu::dsb_sy();
    true
}

/// Wie [`vspace_map_block`], aber als **User-RX** (privat geladener Code, W^X).
pub fn vspace_map_code_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = pd_block_index(phys) else {
        return false;
    };
    // SAFETY: wie `vspace_map_block`.
    unsafe { table_mut(l2_phys)[idx] = user_code_block(phys) };
    cpu::dsb_sy();
    true
}

/// Einen Block wieder auf **supervisor-only** zurücksetzen (der User kommt nicht mehr heran).
pub fn vspace_unmap_block(l2_phys: u64, phys: u64) -> bool {
    let Some(idx) = pd_block_index(phys) else {
        return false;
    };
    // SAFETY: wie `vspace_map_block`.
    unsafe { table_mut(l2_phys)[idx] = kernel_block(phys) };
    cpu::dsb_sy();
    true
}

/// Blatt-Bits für eine 4-KiB-User-Seite.
const fn user_page(pa: u64, perm: UserPerm) -> u64 {
    match perm {
        UserPerm::Ro => pa | P | US | NX,
        UserPerm::Rw => pa | P | RW | US | NX,
        UserPerm::Rx => pa | P | US, // ausführbar, nicht schreibbar (W^X)
    }
}

/// Die Seitentabelle für den 2-MiB-Block `idx` beschaffen: existiert dort noch ein Block,
/// wird er in eine Tabelle **aufgeteilt** (alle Seiten zunächst supervisor-only, damit sich
/// die Sicht des Kernels nicht ändert). `None`, wenn kein Frame verfügbar ist.
unsafe fn pt_for(pd: &mut [u64], idx: usize, alloc: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
    let e = pd[idx];
    if e & PS == 0 && e & P != 0 {
        return Some(e & 0x000f_ffff_ffff_f000); // schon eine Tabelle
    }
    let pt_phys = alloc()?;
    // SAFETY: frisch alloziertes, identity-gemapptes Frame.
    let pt = unsafe { table_mut(pt_phys) };
    let base = (idx as u64) * TWO_MIB;
    for (i, slot) in pt.iter_mut().enumerate() {
        *slot = (base + (i as u64) * PAGE) | P | RW | NX; // supervisor, wie vorher der Block
    }
    pd[idx] = table_desc(pt_phys);
    Some(pt_phys)
}

/// Einen 4-KiB-Frame identity als User-Seite mappen (feingranular, ARM: `vspace_map_page`).
pub fn vspace_map_page(
    l2_phys: u64,
    phys: u64,
    perm: UserPerm,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    vspace_map_page_at(l2_phys, phys, phys, perm, alloc)
}

/// Wie [`vspace_map_page`], aber an einer **wählbaren** virtuellen Adresse.
pub fn vspace_map_page_at(
    l2_phys: u64,
    va: u64,
    phys: u64,
    perm: UserPerm,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if va % PAGE != 0 || phys % PAGE != 0 || va >= GIB1_END {
        return false;
    }
    let idx = (va / TWO_MIB) as usize;
    if idx < FINE_BLOCKS {
        return false; // Kernel-Image-Bereich wird nicht überschrieben
    }
    // SAFETY: gültiges, beschreibbares Seitenverzeichnis; `pt_for` liefert eine gültige Tabelle.
    unsafe {
        let pd = table_mut(l2_phys);
        let Some(pt_phys) = pt_for(pd, idx, alloc) else {
            return false;
        };
        let pt = table_mut(pt_phys);
        pt[((va % TWO_MIB) / PAGE) as usize] = user_page(phys, perm);
    }
    cpu::dsb_sy();
    true
}

/// Eine zuvor gemappte User-Seite wieder auf supervisor-only zurücksetzen.
pub fn vspace_unmap_page(l2_phys: u64, phys: u64) -> bool {
    if phys % PAGE != 0 || phys >= GIB1_END {
        return false;
    }
    let idx = (phys / TWO_MIB) as usize;
    // SAFETY: gültiges, beschreibbares Seitenverzeichnis.
    unsafe {
        let pd = table_mut(l2_phys);
        let e = pd[idx];
        if e & P == 0 || e & PS != 0 {
            return false; // kein feingranularer Block -> nichts zu entfernen
        }
        let pt = table_mut(e & 0x000f_ffff_ffff_f000);
        let slot = ((phys % TWO_MIB) / PAGE) as usize;
        pt[slot] = phys | P | RW | NX; // wieder supervisor-only
    }
    cpu::dsb_sy();
    true
}

/// DMA-Puffer in einen isolierten Adressraum mappen (User-RW; `coherent` steuert die
/// Cache-Attribute — auf x86 ist DMA hardware-kohärent, nicht-kohärente Puffer werden
/// dennoch uncacheable gemappt, damit die Semantik dieselbe bleibt wie auf ARM).
pub fn vspace_map_dma(
    l2_phys: u64,
    phys: u64,
    len: u64,
    coherent: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if len == 0 || len % PAGE != 0 {
        return false;
    }
    let mut off = 0;
    while off < len {
        if !vspace_map_page(l2_phys, phys + off, UserPerm::Rw, alloc) {
            return false;
        }
        if !coherent {
            // Blatt nachträglich auf uncacheable stellen.
            let va = phys + off;
            let idx = (va / TWO_MIB) as usize;
            // SAFETY: die Seite wurde eben gemappt, die Tabellen sind gültig.
            unsafe {
                let pd = table_mut(l2_phys);
                let pt = table_mut(pd[idx] & 0x000f_ffff_ffff_f000);
                let slot = ((va % TWO_MIB) / PAGE) as usize;
                pt[slot] |= PCD | PWT;
            }
        }
        off += PAGE;
    }
    cpu::dsb_sy();
    true
}

/// **W^X-Audit** eines isolierten Adressraums: `0` = keine Verletzung, sonst die Anzahl der
/// Einträge, die zugleich für den User schreibbar **und** ausführbar sind.
pub fn vspace_wx_ok(l2_phys: u64) -> u32 {
    let mut bad = 0;
    // SAFETY: gültiges, in der globalen Map lesbares Seitenverzeichnis.
    unsafe {
        let pd = table_mut(l2_phys);
        for (i, &e) in pd.iter().enumerate() {
            if e & P == 0 {
                continue;
            }
            if e & PS != 0 {
                if e & US != 0 && e & RW != 0 && e & NX == 0 {
                    bad += 1;
                }
            } else if i >= FINE_BLOCKS {
                // Nur selbst angelegte Tabellen prüfen (die ersten sind die geteilten Kernel-PTs).
                let pt = table_mut(e & 0x000f_ffff_ffff_f000);
                for &p in pt.iter() {
                    if p & P != 0 && p & US != 0 && p & RW != 0 && p & NX == 0 {
                        bad += 1;
                    }
                }
            }
        }
    }
    bad
}

/// **Eine User-VA eines fremden Adressraums nach PA aufloesen** (Z6b).
///
/// Der Debugger liest den Speicher seines Ziels; dafuer muss der Kernel dessen Seitentabellen
/// laufen — **nicht seine eigenen**. Genau diese Verwechslung ist der Grund, warum
/// `DEBUG_READ_MEM` ein Syscall ist und keine Bibliotheksfunktion: waere sie im Debugger, laese
/// sie dessen Karte und lieferte dessen Speicher, und das saehe von aussen wie ein Erfolg aus.
///
/// Beide Fenster, in denen User-Speicher liegen kann:
///
/// * **GiB 0** ueber das Seitenverzeichnis `l2` (2-MiB-Bloecke oder feine Tabellen),
/// * das **User-Fenster** bei [`ISO_USER_VA`] ueber `PML4[1] -> PDPT[0] -> PD[slot]`.
///
/// Alles andere gibt `None` — **abgewiesen, nicht geraten**. Eine Adresse ausserhalb dieser beiden
/// Bereiche ist im Adressraum einer PD nicht gemappt; „vermutlich identisch" waere hier die
/// flaechige Identitaetskarte durch die Hintertuer.
///
/// `None` heisst „nicht gemappt" und ist eine **Antwort**, kein Fehler: ein Debugger, der eine
/// Luecke liest, soll eine Luecke sehen.
///
/// ## Aufgeloest wird NUR, was der Gast selbst erreichen kann (`US` auf JEDER Ebene)
///
/// Das ist keine Vorsicht, sondern eine am 2026-08-20 **gemessene Behebung**. Die erste Fassung
/// lief die Tabellen ohne Rechtepruefung — und in einer isolierten VSpace stehen ausser den
/// User-Seiten auch die **geteilten Kernel-Eintraege** (RAM EL1-only, `vspace_create_base`). Ein
/// Debugger mit blossem `DebugRead` haette damit **Kernelspeicher** lesen koennen: eine
/// Rechteausweitung, die von aussen wie ein funktionierender Debugger aussieht.
///
/// Gefunden hat es nicht das Gegenlesen, sondern die Sonde: `luecke-abgewiesen=false` in der
/// `dbgmem`-Zeile — eine Adresse 16 MiB hinter dem Ziel loeste auf, obwohl das Ziel dort nichts
/// hat. „Nicht gemappt" und „nicht FUER DEN GAST gemappt" sind zwei Aussagen, und die zweite ist
/// die, die die Zusage traegt.
///
/// `US` muss auf **jeder** Ebene stehen: x86 verundet die Rechte den Pfad entlang, und eine
/// Pruefung nur am Blatt liesse eine Seite durch, die der Gast wegen einer EL1-only-Tabelle
/// darueber gar nicht erreicht.
pub fn vspace_resolve(l1_phys: u64, l2_phys: u64, va: u64) -> Option<u64> {
    // SAFETY: gueltige, in der globalen Map lesbare Tabellen dieses Adressraums.
    unsafe {
        if va < ONE_GIB {
            let pd = table_mut(l2_phys);
            let e = pd[(va >> 21) as usize];
            if e & P == 0 || e & US == 0 {
                return None;
            }
            if e & PS != 0 {
                return Some((e & 0x000f_ffff_ffe0_0000) | (va & (TWO_MIB - 1)));
            }
            let pt = table_mut(e & 0x000f_ffff_ffff_f000);
            let q = pt[((va >> 12) & 0x1ff) as usize];
            if q & P == 0 || q & US == 0 {
                return None;
            }
            return Some((q & 0x000f_ffff_ffff_f000) | (va & (PAGE - 1)));
        }
        if va >= ISO_USER_VA && va < ISO_USER_VA + 512 * TWO_MIB {
            let off = va - ISO_USER_VA;
            let pml4 = table_mut(l1_phys);
            if pml4[1] & P == 0 || pml4[1] & US == 0 {
                return None;
            }
            let pdpt = table_mut(pml4[1] & 0x000f_ffff_ffff_f000);
            if pdpt[0] & P == 0 || pdpt[0] & US == 0 {
                return None;
            }
            let pd = table_mut(pdpt[0] & 0x000f_ffff_ffff_f000);
            let e = pd[(off >> 21) as usize];
            if e & P == 0 || e & US == 0 {
                return None;
            }
            if e & PS != 0 {
                return Some((e & 0x000f_ffff_ffe0_0000) | (va & (TWO_MIB - 1)));
            }
            let pt = table_mut(e & 0x000f_ffff_ffff_f000);
            let q = pt[((off >> 12) & 0x1ff) as usize];
            if q & P == 0 || q & US == 0 {
                return None;
            }
            return Some((q & 0x000f_ffff_ffff_f000) | (va & (PAGE - 1)));
        }
        None
    }
}

/// Die **selbst angelegten** Seitentabellen eines Adressraums einsammeln (Teardown).
/// Die geteilten Kernel-PTs der ersten 16 MiB bleiben unangetastet.
pub fn vspace_collect_l3s(l2_phys: u64, free_l3: &mut dyn FnMut(u64)) {
    // SAFETY: gültiges, lesbares Seitenverzeichnis.
    unsafe {
        let pd = table_mut(l2_phys);
        for (i, e) in pd.iter_mut().enumerate().skip(FINE_BLOCKS) {
            if *e & P != 0 && *e & PS == 0 {
                free_l3(*e & 0x000f_ffff_ffff_f000);
                *e = 0;
            }
        }
    }
}

/// Geräte-MMIO in einen isolierten Adressraum mappen — auf x86 liegt MMIO oberhalb von GiB 0
/// in **geteilten**, supervisor-only Tabellen (`ISO_PD_HIGH`). Eine PD-eigene Geräteabbildung
/// bräuchte dort eine private Tabelle; das kommt mit dem HardwareLand-Port (s. TODO.md).
pub fn vspace_map_device(
    l1_phys: u64,
    phys: u64,
    len: u64,
    ro: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    if len == 0 || len % PAGE != 0 || phys % PAGE != 0 {
        return false;
    }
    // Oberhalb des einen benutzten PML4-Eintrags (512 GiB) gibt es keine Blattstruktur, an die
    // sich etwas haengen liesse. Das ist eine Grenze der Plattformabbildung, kein Rechtefehler
    // -- deshalb hier ablehnen und nicht irgendwo eine Tabelle erfinden.
    if phys.saturating_add(len) > (MAPPED_GIB as u64) * ONE_GIB {
        return false;
    }
    let mut off = 0;
    while off < len {
        if !map_device_page(l1_phys, phys + off, ro, alloc) {
            return false;
        }
        off += PAGE;
    }
    cpu::dsb_sy();
    true
}

/// Das Seitenverzeichnis fuer GiB `g` dieses Adressraums **privat** machen, falls es noch das
/// geteilte ist.
///
/// **Der wichtigste Schritt der ganzen Funktion.** `vspace_create_base` haengt GiB 1..3 jeder
/// isolierten PD an dieselben statischen Tabellen ([`ISO_PD_HIGH`]) -- das spart drei Frames je
/// Adressraum und ist richtig, solange dort nichts PD-Spezifisches steht. Ein Geraetefenster ist
/// aber genau das. Wer es in die geteilte Tabelle schriebe, gaebe es **jeder** isolierten PD:
/// aus einer Zuteilung an einen Treiber wuerde ein Zugriff fuer alle, und zwar lautlos -- die
/// Cap-Pruefung liefe korrekt durch, die Isolation waere trotzdem weg.
///
/// Also: beim ersten Geraetefenster in diesem GiB eine **Kopie** anlegen und einhaengen. Danach
/// gehoert sie diesem Adressraum, und `vspace_collect_device_tables` gibt sie beim Abbau zurueck.
///
/// # Safety
/// `pdpt` muss die PDPT dieses Adressraums sein.
unsafe fn private_pd(
    pdpt: &mut [u64],
    g: usize,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Option<u64> {
    let e = pdpt[g];
    let cur = e & 0x000f_ffff_ffff_f000;
    let leaf = e & P != 0 && e & PS != 0; // 1-GiB-Blatt: KEINE Tabelle
    if e & P != 0 && !leaf && !is_shared_high_pd(g, cur) {
        return Some(cur); // schon privat
    }
    let new = alloc()?;
    // SAFETY: frisch alloziertes, identity-gemapptes Frame; Quelle ist die bisherige Tabelle.
    unsafe {
        let dst = table_mut(new);
        if leaf {
            // **Ein 1-GiB-Blatt ist keine Tabelle.** `cur` zeigt hier auf RAM, nicht auf
            // Einträge; es als Tabelle zu lesen wäre der teuerste Fehler dieser Datei — 512
            // Speicherworte würden als Seitentabelleneinträge gedeutet. Also **attributgetreu**
            // in 2-MiB-Blöcke auflösen: dieselben Flags (inkl. `PS`, das auf dieser Ebene 2 MiB
            // heißt), dieselbe Sicht wie vorher.
            let flags = e & !0x000f_ffff_ffff_f000;
            let base = (g as u64) * ONE_GIB;
            for (i, s) in dst.iter_mut().enumerate() {
                *s = (base + (i as u64) * TWO_MIB) | flags;
            }
        } else if e & P != 0 {
            let src = table_mut(cur);
            dst.copy_from_slice(src);
        } else {
            for s in dst.iter_mut() {
                *s = 0;
            }
        }
    }
    pdpt[g] = table_desc(new);
    if g >= LOW_GIB {
        HIGH_DEV_PRIVATE.fetch_add(1, Ordering::Release);
    }
    Some(new)
}

/// Zeigt `pd_phys` auf ein **geteiltes** Seitenverzeichnis (also eines, das keinem einzelnen
/// Adressraum gehört)?
///
/// Vorher stand hier ein Vergleich gegen **eine** Tabelle ([`ISO_PD_HIGH`]`[g-1]`). Das war
/// richtig, solange es nur diese gab; mit dem Vorrat für hohe Geräte-GiB ([`HIGH_DEV_PD`]) wäre
/// es lautlos unvollständig geworden — und die Folge stünde in CLAUDE.md: ein Gerätefenster in
/// einer geteilten Tabelle gehört **jeder** isolierten PD, bei korrekt durchlaufender
/// Cap-Prüfung. Die Frage ist deshalb jetzt strukturell: gehört der Rahmen dem Kernel-Image?
fn is_shared_high_pd(g: usize, pd_phys: u64) -> bool {
    if g == 0 {
        return false; // GiB 0 hat ohnehin eine eigene Tabelle je Adressraum
    }
    is_static_table(pd_phys)
}

/// Eine einzelne Geraeteseite user-zugreifbar in diesen Adressraum haengen (uncacheable, NX).
fn map_device_page(
    l1_phys: u64,
    pa: u64,
    ro: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> bool {
    let g = (pa / ONE_GIB) as usize;
    // SAFETY: gueltige, identity-gemappte PML4 dieses Adressraums; alle weiteren Tabellen werden
    // aus ihr aufgeloest bzw. frisch alloziert.
    unsafe {
        let pml4 = table_mut(l1_phys);
        if pml4[0] & P == 0 {
            return false;
        }
        let pdpt = table_mut(pml4[0] & 0x000f_ffff_ffff_f000);
        let Some(pd_phys) = private_pd(pdpt, g, alloc) else {
            return false;
        };
        let pd = table_mut(pd_phys);
        let idx = ((pa % ONE_GIB) / TWO_MIB) as usize;
        let e = pd[idx];
        let pt_phys = if e & P != 0 && e & PS == 0 {
            e & 0x000f_ffff_ffff_f000
        } else {
            let Some(p) = alloc() else {
                return false;
            };
            // Den 2-MiB-Block **attributgetreu** in Seiten aufloesen: dieselben Flags, nur ohne
            // `PS`. Feste Bits einzusetzen waere hier ein Fehler mit Reichweite -- der Kernel
            // laeuft beim Syscall in DIESEM Adressraum, und ein RAM-Block, den die Aufteilung
            // nebenbei uncacheable machte, wuerde ihn ausbremsen, ohne dass es jemand mit dieser
            // Zeile in Verbindung braechte.
            let base = (g as u64) * ONE_GIB + (idx as u64) * TWO_MIB;
            let flags = if e & P != 0 { e & !0x000f_ffff_ffff_f000 & !PS } else { 0 };
            let pt = table_mut(p);
            for (i, slot) in pt.iter_mut().enumerate() {
                *slot = if flags == 0 { 0 } else { (base + (i as u64) * PAGE) | flags };
            }
            pd[idx] = table_desc(p);
            p
        };
        let pt = table_mut(pt_phys);
        let slot = ((pa % TWO_MIB) / PAGE) as usize;
        // Geraeteregister: uncacheable + write-through, nie ausfuehrbar. `US` ist die eigentliche
        // Zuteilung -- ohne dieses Bit sieht der Treiber in Ring 3 nichts.
        pt[slot] = pa | P | US | NX | PCD | PWT | if ro { 0 } else { RW };
    }
    true
}

/// Die PDPT eines Adressraums einsammeln (x86 hat eine Ebene mehr als ARM; der Kernel ruft
/// dies im Teardown mit der PML4 auf, s. `vspace_collect_l3s` für die unteren Ebenen).
pub fn vspace_collect_device_tables(l1_phys: u64, free: &mut dyn FnMut(u64)) {
    // SAFETY: gültige, lesbare PML4 des abzubauenden Adressraums.
    unsafe {
        let pml4 = table_mut(l1_phys);
        if pml4[0] & P == 0 {
            return;
        }
        let pdpt_phys = pml4[0] & 0x000f_ffff_ffff_f000;
        let pdpt = table_mut(pdpt_phys);
        // GiB 1..3: **privat gewordene** Seitenverzeichnisse samt ihrer Seitentabellen zurückgeben.
        // Die geteilten (`ISO_PD_HIGH`) gehören keinem Adressraum und dürfen nicht freigegeben
        // werden -- das wäre kein Leck, sondern das Gegenteil: der Allokator bekäme statischen
        // Kernel-Speicher als freies RAM zurück. Deshalb wird jede Tabelle vor dem Freigeben
        // **daraufhin geprüft**, ob sie überhaupt aus dem Allokator stammt.
        //
        // GiB 0 bleibt außen vor: dessen Tabellen räumt `vspace_collect_l3s` ab. Beides zu tun
        // wäre eine doppelte Freigabe.
        for g in 1..MAPPED_GIB {
            let e = pdpt[g];
            // Ein 1-GiB-Blatt (`PS`) ist keine Tabelle -- es zu „befreien" gäbe dem Allokator
            // RAM zurück, das ihm nie gehörte, und die Schleife darunter läse Speicher als
            // Einträge. Nicht präsent: nichts zu tun.
            if e & P == 0 || e & PS != 0 {
                continue;
            }
            let pd_phys = e & 0x000f_ffff_ffff_f000;
            if is_shared_high_pd(g, pd_phys) {
                continue;
            }
            let pd = table_mut(pd_phys);
            for &pe in pd.iter() {
                if pe & P != 0 && pe & PS == 0 {
                    free(pe & 0x000f_ffff_ffff_f000);
                }
            }
            free(pd_phys);
            pdpt[g] = 0;
        }
        free(pdpt_phys);
        pml4[0] = 0;
    }
}

// ================================================================================================
// Das private User-VA-Fenster einer isolierten PD (E-Rest 3d, zweite Hälfte)
// ================================================================================================
//
// **Warum es das gibt.** Bis hierher wurde die private Region einer isolierten PD **identisch**
// abgebildet (VA == PA) — `vspace_map_block` leitet den Index aus der *Physadresse* ab. Damit war
// sie an GiB 0 gebunden, denn nur dafür hat eine isolierte VSpace ein eigenes Seitenverzeichnis.
// Gemessen: **504** solche Regionen passen hinein (`gib0_deckel_ist_eine_zahl`), und das war der
// bindende Deckel für die Zahl gleichzeitiger Mandanten — nicht `MAX_VSPACES` (4096).
//
// **Warum das Fenster oberhalb der Identitätskarte liegen MUSS und nicht einfach woanders in
// GiB 0.** Der Kernel läuft beim Syscall im Adressraum *dieser* PD und greift dort über die
// Identitätskarte auf beliebiges physisches RAM zu. Eine User-VA in GiB 0, die auf eine andere PA
// zeigt, **verdeckt** genau diese Sicht: der Kernel läse an der Stelle den Speicher der PD statt
// den eigenen. Das Fenster gehört deshalb dorthin, wo der Kernel nie identisch zugreift.
//
// Auf x86-64 belegt die Identitätskarte ausschließlich `PML4[0]` (0..512 GiB, s. `MAPPED_GIB`).
// `PML4[1]` ist frei — dort liegt das Fenster, und jede isolierte PD bekommt darin ihre eigenen
// Tabellen. Der Preis sind zwei bis drei 4-KiB-Rahmen je PD; sie sind reiner Kernel-Speicher und
// dürfen selbst oberhalb 4 GiB liegen.

/// Wieviele GiB oberhalb von [`LOW_MAPPED_END`] als RAM in die Karte aufgenommen wurden.
/// `0` heisst: diese Maschine hat dort keinen Speicher — eine Aussage ueber die Maschine, nicht
/// ueber den Kernel (E-Rest 3d, `isohigh`).
pub fn high_ram_gib() -> usize {
    HIGH_RAM_GIB.load(Ordering::Acquire)
}

/// Basis des privaten User-VA-Fensters einer isolierten PD: `PML4[1]`, also 512 GiB.
pub const ISO_USER_VA: u64 = 512 * ONE_GIB;

/// Eine Tabelle beschaffen oder anlegen: gibt die Physadresse des nächsten Levels zurück.
///
/// # Safety
/// `slot` muss auf einen gültigen Tabelleneintrag zeigen, der beschreibbar ist.
unsafe fn table_or_new(slot: &mut u64, alloc: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
    if *slot & P != 0 {
        return Some(*slot & 0x000f_ffff_ffff_f000);
    }
    let new = alloc()?;
    // SAFETY: frisch alloziertes, identity-gemapptes Frame.
    unsafe {
        for e in table_mut(new).iter_mut() {
            *e = 0;
        }
    }
    *slot = table_desc(new);
    Some(new)
}

/// **Die private Region `[phys, phys+len)` in das User-Fenster dieser PD abbilden.**
///
/// Gibt die **virtuelle** Adresse zurück, unter der die PD sie sieht — die Physadresse ist damit
/// frei wählbar, und genau das hebt den GiB-0-Deckel. Ein 2-MiB-ausgerichteter 2-MiB-Block nimmt
/// weiterhin **einen Blockdeskriptor** (der Fastpath geht nicht verloren: er hing nie an der
/// Identität, sondern nur an der Ausrichtung der VA); alles andere wird seitenweise abgebildet.
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
    // SAFETY: gültige, beschreibbare PML4 dieses Adressraums; alle Tabellen darunter sind frisch
    // alloziert oder von diesem Adressraum angelegt.
    unsafe {
        let pml4 = table_mut(l1_phys);
        let pdpt_phys = table_or_new(&mut pml4[1], alloc)?;
        let pdpt = table_mut(pdpt_phys);
        let pd_phys = table_or_new(&mut pdpt[0], alloc)?;
        let pd = table_mut(pd_phys);
        if len == TWO_MIB && phys % TWO_MIB == 0 {
            pd[slot] = match perm {
                UserPerm::Rx => user_code_block(phys),
                _ => user_block(phys),
            };
        } else {
            let pt_phys = table_or_new(&mut pd[slot], alloc)?;
            let pt = table_mut(pt_phys);
            for i in 0..(len / PAGE) as usize {
                pt[i] = user_page(phys + (i as u64) * PAGE, perm);
            }
        }
    }
    cpu::dsb_sy();
    Some(ISO_USER_VA + (slot as u64) * TWO_MIB)
}

/// Die Tabellen des User-Fensters beim Abbau zurückgeben (Gegenstück zu
/// [`vspace_map_user_window`]). Gibt **nur** Rahmen frei, die aus dem Allokator stammen — hier
/// sind das alle, denn das Fenster hat keine geteilten Tabellen.
pub fn vspace_collect_user_window(l1_phys: u64, free: &mut dyn FnMut(u64)) {
    // SAFETY: gültige, lesbare PML4 des abzubauenden Adressraums.
    unsafe {
        let pml4 = table_mut(l1_phys);
        if pml4[1] & P == 0 {
            return;
        }
        let pdpt_phys = pml4[1] & 0x000f_ffff_ffff_f000;
        let pdpt = table_mut(pdpt_phys);
        if pdpt[0] & P != 0 {
            let pd_phys = pdpt[0] & 0x000f_ffff_ffff_f000;
            let pd = table_mut(pd_phys);
            // **Alle** Plätze durchgehen, nicht nur den ersten: `spawn_isolated_native` belegt
            // zwei (Code und Stack). Ein Blockdeskriptor (`PS`) ist dabei keine Tabelle -- ihn
            // freizugeben gäbe dem Allokator die Nutzregion zurück, die der Aufrufer verwaltet.
            for &e in pd.iter() {
                if e & P != 0 && e & PS == 0 {
                    free(e & 0x000f_ffff_ffff_f000);
                }
            }
            free(pd_phys);
        }
        free(pdpt_phys);
        pml4[1] = 0;
    }
}

/// **W^X-Audit der Geräte-Tabellen**: `false`, wenn eine für den User schreibbare Seite zugleich
/// ausführbar ist.
///
/// Bis A-5.1 gab es keine PD-eigenen Gerätetabellen, und die Antwort war fest `true`. Das war
/// richtig, solange nichts zu prüfen war — und wäre ab jetzt genau die Sorte Prüfer, die über
/// Abwesenheit entscheidet, ohne sprechfähig zu sein.
pub fn vspace_device_wx_ok(l1_phys: u64) -> bool {
    // SAFETY: gültige, lesbare PML4 des Adressraums.
    unsafe {
        let pml4 = table_mut(l1_phys);
        if pml4[0] & P == 0 {
            return true;
        }
        let pdpt = table_mut(pml4[0] & 0x000f_ffff_ffff_f000);
        for g in 1..MAPPED_GIB {
            let e = pdpt[g];
            if e & P == 0 {
                continue;
            }
            if e & PS != 0 {
                // 1-GiB-Blatt: selbst prüfbar, aber keine Tabelle. Die Verletzung wäre
                // „für den User schreibbar UND ausführbar" -- dieselbe Frage, eine Ebene höher.
                if e & US != 0 && e & RW != 0 && e & NX == 0 {
                    return false;
                }
                continue;
            }
            let pd_phys = e & 0x000f_ffff_ffff_f000;
            if is_shared_high_pd(g, pd_phys) {
                continue;
            }
            for &pe in table_mut(pd_phys).iter() {
                if pe & P == 0 {
                    continue;
                }
                if pe & PS != 0 {
                    if pe & US != 0 && pe & RW != 0 && pe & NX == 0 {
                        return false;
                    }
                    continue;
                }
                for &leaf in table_mut(pe & 0x000f_ffff_ffff_f000).iter() {
                    if leaf & P != 0 && leaf & US != 0 && leaf & RW != 0 && leaf & NX == 0 {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Ein ganzes GiB als Geräte-Block global abbilden (ARM-API-Gegenstück).
///
/// Unter 4 GiB ein No-op: dort steht der MMIO-Bereich seit [`init_primary`] uncacheable
/// identity-abgebildet. Darüber geht es durch denselben Pfad wie ein einzelnes Registerfenster
/// ([`map_device_window_global`]) — und meldet ehrlich `false`, wenn der Vorrat nicht reicht.
pub fn map_device_block_global(gib: usize) -> bool {
    if gib < LOW_GIB {
        return true;
    }
    map_device_window_global((gib as u64) * ONE_GIB, ONE_GIB)
}

/// **Kann diese Architektur Guard-Pages?** Auf x86 ja (s. `guard_unmap`). Das Gegenstueck in der
/// aarch64-HAL gibt `false` und zaehlt stattdessen die unbewachten Stacks -- der Aufrufer
/// unterscheidet damit „Vorrat leer" (abweisen) von „hier gibt es das nicht" (zaehlen).
pub fn guard_unterstuetzt() -> bool {
    true
}

/// Auf x86 gibt es Wachen; die Zahl ohne Wache ist deshalb immer 0.
pub fn unbewachte_stacks() -> usize {
    0
}
