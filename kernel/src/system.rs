//! Kernzustand hinter **per-Kern-Scheduler-Locks** (`SCHEDS[core]`, je Kern eine Instanz mit
//! eigener TCB-Partition) und den **getrennten** Ressourcen-Locks (`CAPS` = Capability-Space + PDs;
//! `MEM` = physischer Allokator; `EPS[]`/`NTFNS[]` = Endpoints/Notifications; `VSPACES`; `DMA_CTX`)
//! — plus die Trap-Hooks.
//!
//! **Echte per-Kern-parallele Einplanung:** Der heiße Timer-/IPI-Reschedule-Pfad sperrt nur
//! `SCHEDS[core]` des eigenen Kerns — Kerne planen gleichzeitig ein, ohne sich gegenseitig zu
//! blockieren. Kern-übergreifendes Aufwecken ([`wake_remote`]) sperrt die **Ziel**instanz und
//! schickt einen Reschedule-IPI.
//!
//! **Lock-Ordnung (Rang-Hierarchie, totale Ordnung):** `CAPS` (R0) → `EPS`/`NTFNS`/`VSPACES`/
//! `DMA_CTX` (R1) → `SCHEDS[*]` (R2) → `Heap.inner` (R3) → `MEM` (R4, innerster). Nur aufsteigend
//! schachteln; `MEM` hält nie einen weiteren Lock. Der reine Reschedule-Pfad nimmt nur
//! `SCHEDS[core]` (+ atomares `FP_OWNER`) und kann an keinem Deadlock-Zyklus teilnehmen; kein Pfad
//! nimmt zwei verschiedene `SCHEDS[*]` gleichzeitig. Vollständige Herleitung + alle belegten
//! Schachtelungen: `docs/invariants.md` §1.

use caprock_cap::{CapError, CapInfo, CapPtr, DmaCoherence, DmaDir, ObjectKind};
use caprock_hal::{self as hal, exception::TrapFrame, fp::FpState, println};
use caprock_cap::checkpoint::{Channel, Edge, EdgeRole, Scope};
use caprock_ipc::{Endpoint, Notification, Quiescence, Rebind, Role as IpcRole};
use caprock_mem::{MemoryCap, PhysAllocator, PhysRegion, Rights};
use caprock_microkit::{Caps, Domain};
use caprock_region::heap::RegionSource;
use caprock_region::{state, Purpose, Region, RegionTag};
use caprock_loader::elf::{ElfImage, PF_W, PF_X};
use caprock_loader::manifest as man;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::addr::{DmaRegion, Iova, Pa};
use caprock_sched::{SchedOps, Scheduler, ThreadId, MAX_CORES};
use caprock_slab::{AtomicTable, Slab};
use caprock_sync::{RwSpinLock, SpinLock};

/// FORK/EXEC-Planlogik des Ladepfads (rein, host-testbar) — die Entscheidungen VOR der ersten
/// Allokation. Der Vollzug (`dispatch_fork`/`dispatch_exec` weiter unten) ruft sie auf, statt
/// sie nachzubauen. Als Pfad-Modul hier angehaengt statt in `main.rs`: die Datei ist neu, und
/// `main.rs` gehoert niemandem allein (AGENTS.md) — ein `#[path]`-Modul braucht dort keine Zeile.
#[path = "forkexec.rs"]
mod forkexec;

/// **Tatsächliche** Kernzahl (beim Boot gesetzt, s. [`configure`]). Alle per-Kern-Schleifen
/// laufen hierüber; die Compile-Zeit-Konstante [`MAX_CORES`] dimensioniert nur die Arrays
/// von *Locks*/Atomics, nicht die Datentabellen.
static NUM_CORES: AtomicUsize = AtomicUsize::new(1);

/// Stack-Größe eines Kernel-Threads (für Ressourcen-Buchhaltung in Tests).
pub fn stack_bytes() -> u64 {
    STACK_SIZE
}

/// Noch freie **Thread-Slots** im globalen Thread-Directory (Kapazitäts-Telemetrie).
pub fn threads_available() -> usize {
    caprock_sched::threads_available()
}

/// Gesamtzahl der Thread-Slots (Boot-Kapazität).
pub fn thread_capacity() -> usize {
    caprock_sched::thread_capacity()
}

/// Anzahl aktiver Kerne (Laufzeitwert).
pub fn num_cores() -> usize {
    NUM_CORES.load(Ordering::Relaxed)
}

/// Der Kern, auf dem der Aufrufer gerade läuft. Für die Ladepfade, die den Heimatkern seit C8
/// ausdrücklich nennen müssen (s. `loader::ladepolitik_auf`) und ihn beim Boot noch selbst sind.
pub fn eigener_kern() -> usize {
    hal::cpu::core_id()
}

const STACK_SIZE: u64 = 64 * 1024;
/// Standardpriorität für Idle und gewöhnliche Demo-Threads (Round-Robin).
pub const IDLE_PRIO: u8 = 1;

// EL0-User-Threads: Kernel-Stacks aus einem EL1-only Pool (Linker), User-Stacks
// aus dem EL0-zugänglichen RAM. Der Kernel-Stack MUSS EL1-only sein (sonst könnte
// der EL0-Thread seinen eigenen Kernel-Stack lesen/schreiben), daher der feste Pool
// im Kernel-Image (nicht aus dem EL0-zugänglichen MEM-Allokator).
/// **Der EL1-Stack eines EL0-Threads — 4 KiB auf x86, 16 KiB auf aarch64, und der Unterschied
/// ist eine Messung und keine Vorsicht.**
///
/// Bis zum 2026-08-11 waren es ueberall 16 KiB, und der tiefste Pfad benutzte davon **73 %**:
/// `SYS_LOAD` verifizierte eine Ed25519-Signatur **im Kernel**, auf dem Stack des aufrufenden
/// EL0-Threads. Seit C8 laeuft das auf einem eigenen Verifiziererthread; der Hoechststand hier
/// faellt damit auf **1312 B** (Lade-Suite) bzw. **824 B** (Hauptsuite).
///
/// **Warum 4 KiB auf x86 verantwortbar sind — und die Begruendung haengt an drei Dingen, die es
/// vorher nicht gab:**
/// * die **Guard-Page** unter jedem Stack: ein Ueberlauf ist ein `#PF` und kein stiller Treffer,
/// * die **IST-Staecke** fuer `#DF`, `NMI` und `#MC`: ein NMI (nicht maskierbar, er fragt `IF`
///   nicht) laeuft **nicht** auf diesem Stack, sondern auf seinem eigenen — genau der Einwand,
///   der die alte Reserve auf echter Hardware fraglich machte,
/// * die **Wasserstandsmarke**, die den Wert bei jedem Lauf nachmisst statt ihn zu glauben.
///
/// Gemessen bei 4 KiB: 1312 von 4096 B (32,0 %), Reserve 2784 B, beide Suiten `ALL PASS`.
///
/// **Auf aarch64 bleibt es bei 16 KiB, und das ist kein Zoegern, sondern das Fehlen der drei
/// Voraussetzungen:** dort gibt es (noch) keine Guard-Page, kein IST-Gegenstueck und keine
/// Berichtszeile mit dem Wasserstand. Eine Zahl, die auf einer Architektur gemessen und auf der
/// anderen uebernommen wird, ist auf der anderen geraten — und dieses Projekt hat genau dafuer
/// schon bezahlt (`MASK_BITS`, das auf x86 zufaellig richtig war und auf aarch64 falsch).
#[cfg(target_arch = "x86_64")]
pub(crate) const USER_KSTACK_SIZE: usize = 0x1000;
#[cfg(target_arch = "aarch64")]
pub(crate) const USER_KSTACK_SIZE: usize = 0x4000;
// Kein stiller Fallback fuer eine dritte Architektur: wer sie anfaehrt, bekommt einen
// Baufehler statt einer geratenen Stackgroesse (dieselbe Haltung wie D11 zur Laufzeit).
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("USER_KSTACK_SIZE ist nur fuer x86_64/aarch64 belegt -- Dichte-Zahlen ableiten, nicht raten");

/// Besitzer-Abbildung der **dynamisch aus `MEM` allozierten** EL0-Kernel-Stacks: `base_of[slot]`
/// = physische Basis des Kstacks des Threads `slot` (0 = keiner). Kstacks kommen NICHT mehr aus
/// einer festen Image-Region — die lag im 2-MiB-Kernel-Image und deckelte die gleichzeitige
/// EL0-Thread-Zahl hart. Jetzt aus dem allgemeinen RAM: Limit ist nur RAM + Thread-Zahl
/// (Thread-Kapazität), passend als Basis eines vollen OS. Beim Thread-Ende an `MEM` zurück (kein Leck).
/// EL1-only-Zugriff: der Kstack liegt im User-RAM-Fenster; in JEDER isolierten VSpace ist dieses
/// EL1-only (kernel_block-Spiegel) -> untrusted EL0 kommt nicht heran. Nur die globale SAS-Map
/// (TrustedSAS) sieht es EL0-RW — konsistent mit dem SAS-Vertrauensmodell.
struct KstackPool {
    /// Physische Basis des Kstacks je globalem Thread-Slot (`0` = keiner). Zur Boot-Zeit
    /// dimensioniert (Thread-Kapazität), s. [`configure`].
    base_of: Slab<u64>,
    /// **Die GENERATION des Threads, dem dieser Kstack gehoert** (D15, 2026-08-13).
    ///
    /// `base_of` ist ueber den **Slot** indiziert, und ein Slot ist keine Identitaet: er wird nach
    /// dem Thread-Tod wieder vergeben (`release_gid`, LIFO — der zuletzt freigegebene ist der
    /// naechste ausgegebene). Eine Aufraeumung, die erst NACH der Freigabe der `gid` laeuft und
    /// nur den Slot in der Hand haelt, gibt dann den Kernel-Stack seines **Nachfolgers** an den
    /// Allokator zurueck. Mit der Generation daneben ist der Unterschied entscheidbar.
    gen_of: Slab<u32>,
    live: usize,
    /// **C7b: die EL0-USER-Stackregion je Thread-Slot** — physische Basis und Länge (`0` =
    /// keine). Das ist die *andere* Seite desselben Threads: `base_of` hält seinen 16-KiB-EL1-
    /// Stack, `ubase_of` die Region, auf der sein Ring-3-Code rechnet.
    ///
    /// **Warum eine Buchführung und kein Erkennen am Inhalt.** Im Zombie-Ring des Schedulers
    /// liegen Kernel-Thread-Stacks und EL0-User-Regionen nebeneinander. `kstackmark` unter-
    /// scheidet sie am Füllmuster — die EL0-Region trägt aber keines (sie ist **genullt**, s.
    /// `userstackmark`), und „Fuss ist null" gilt genauso für einen restlos aufgebrauchten
    /// Kernel-Stack. Eine Unterscheidung am Inhalt wäre also genau die Sorte Erkenner, der den
    /// schlimmsten Fall nicht sieht. Hier steht stattdessen eine **positive** Zuordnung: der
    /// Spawn-Pfad sagt, welche Region die EL0-Region dieses Slots ist, und `reap_core` prüft die
    /// Übereinstimmung mit dem Zombie.
    ///
    /// Sie liegt aus zwei Gründen in **diesem** Lock und nicht in einem eigenen: sie wird in
    /// genau denselben kritischen Abschnitten geschrieben wie `base_of` (s.
    /// [`record_user_kstack`]), und ein weiteres Blattlock wäre eine weitere Kante in der
    /// Sperrordnung, die niemand prüft.
    ubase_of: Slab<u64>,
    ulen_of: Slab<u64>,
}

// --- D15-Melder: die GELEGENHEIT, nicht der Treffer ------------------------------------------
//
// D15 ist ein Bild, kein Ereignis: ein EL0-Thread mit PC=0 (`EC=0x20 FAR=0`), danach der Kernel
// selbst nach 0 (`EC=0x21 ELR=0`). Rate 0,225 % ueber 4000 aarch64-Laeufe — ein Melder, der nur
// beim Unglueck spricht, ist in 443 von 444 Laeufen stumm. Diese drei Zaehler sprechen dagegen in
// JEDEM Lauf, weil sie den ZUSTAND pruefen und nicht den Ausgang.

/// Wie oft traf eine spaete Kstack-Aufraeumung einen Slot, der **schon wieder belegt** war.
/// Das ist die Gelegenheit: ab hier bezeichnet die `gid` einen fremden, lebenden Thread.
static KSTACK_SPAET_SLOT: AtomicU64 = AtomicU64::new(0);
/// Wie oft haette diese Aufraeumung den Kstack eines **fremden** Threads freigegeben
/// (`base_of` steht, aber die eingetragene Generation ist eine andere). Das ist der Treffer.
static KSTACK_FREMD: AtomicU64 = AtomicU64::new(0);
/// Wie oft gab der Kernel den Kernel-Stack frei, auf dem er **in diesem Moment selbst steht**.
/// Deterministisch, kein Rennen — der Schaden entsteht erst, wenn ein anderer Kern die Region in
/// diesem Fenster holt und nullt (jede Vergabe geht durch `zero_phys`).
static KSTACK_UNTER_FUESSEN: AtomicU64 = AtomicU64::new(0);
/// **Der Nenner.** Wie oft lief die spaete Aufraeumung ueberhaupt, und wie oft mit einem Stack in
/// der Hand. Ohne ihn ist eine `0` oben nicht von „der Pfad lief nie" zu unterscheiden — ein leerer
/// Lauf ist kein Testergebnis.
static KSTACK_RECLAIM_GESAMT: AtomicU64 = AtomicU64::new(0);
static KSTACK_RECLAIM_MIT_STACK: AtomicU64 = AtomicU64::new(0);

// **Was hier NICHT mehr steht, und was die Messung dazu ergeben hat.** Am 2026-08-13 lief hier
// zusaetzlich ein Kanarienvogel: nach dem `free_region` bekam die Wache der Region ein Magiewort,
// und am spaetestmoeglichen Punkt (unmittelbar vor `mov sp, x0`) wurde nachgesehen, ob es noch
// steht. Ergebnis ueber **4295 Fenster** (108 Laeufe): **0 fremde Vergaben, 0 tote Kanarienvoegel**
// — das Fenster ist strukturell offen und wird unter dieser Last **nicht** genommen.
//
// Der Melder ist wieder ausgebaut, weil er in eine freigegebene Region SCHREIBT: er ist ein
// Messwerkzeug und kein Kernelbestandteil. Die Zahl bleibt, der Schreibzugriff nicht. Was bleibt,
// ist [`KSTACK_UNTER_FUESSEN`] — es zaehlt die Gelegenheit ohne einen einzigen Schreibzugriff.

/// Zählerstände der D15-Melder:
/// `(spaet_slot, fremd, unter_fuessen, reclaim_gesamt, reclaim_mit_stack)`.
pub fn kstack_spaet_stats() -> (u64, u64, u64, u64, u64) {
    (
        KSTACK_SPAET_SLOT.load(Ordering::Relaxed),
        KSTACK_FREMD.load(Ordering::Relaxed),
        KSTACK_UNTER_FUESSEN.load(Ordering::Relaxed),
        KSTACK_RECLAIM_GESAMT.load(Ordering::Relaxed),
        KSTACK_RECLAIM_MIT_STACK.load(Ordering::Relaxed),
    )
}

/// Eine Adresse auf dem **gerade benutzten** Kernel-Stack.
///
/// `#[inline(never)]` und `black_box`, damit der Anker wirklich einen Rahmen bekommt und nicht
/// wegoptimiert wird. Gebraucht wird keine exakte SP-Ablesung, sondern die Antwort auf „liege ich
/// in dieser Region" — dafuer genuegt irgendeine Adresse im aktuellen Rahmen, und die ist
/// arch-neutral zu bekommen (kein `asm!`, also auch auf x86 dieselbe Zeile).
#[inline(never)]
fn stapeladresse() -> usize {
    let anker = 0u8;
    core::hint::black_box(&anker) as *const u8 as usize
}
/// Pool-Lock (nur `base_of`/`live`); NIE verschachtelt mit `MEM` gehalten (claim/reclaim nehmen die
/// Locks sequenziell) -> keine Sperrordnungsverletzung; `MEM` ist ohnehin innerster Rang.
static KSTACKS: SpinLock<KstackPool> = SpinLock::new(KstackPool {
    base_of: Slab::empty(),
    gen_of: Slab::empty(),
    live: 0,
    ubase_of: Slab::empty(),
    ulen_of: Slab::empty(),
});

/// C7c: die Stack-Arena — ein zusammenhaengender Boot-RAM-Bereich, dessen 2-MiB-Bloecke
/// EINMAL aufgeteilt werden, statt je gestreutem Stack einmal.
///
/// `None` heisst „nicht bestueckt" (zu wenig RAM, Guard-Vorrat erschoepft) — dann gilt der
/// heutige Streupfad weiter, ohne dass ein Aufrufer es merkt. Sperrordnung: Blattlock wie
/// `KSTACKS`, NIE verschachtelt mit `MEM` oder `SCHEDS` gehalten (Vergabe/Freigabe nehmen
/// die Locks sequenziell).
static KSTACK_ARENA: SpinLock<Option<crate::stack_arena::StackArena<'static>>> =
    SpinLock::new(None);
/// C7c/Dichte: der Arena-PLAN — `(gewollte Faeden, Plaetze, Grund, bestueckt)`.
///
/// `gewollt` ist die Thread-Kapazitaet aus [`configure`], `Plaetze` das abgeleitete Minimum
/// (s. `stack_arena::arena_plaetze`), `Grund` nennt die bindende Schranke, `bestueckt` ob die
/// Arena danach wirklich steht. Der Plan steht auch dann, wenn die Bestueckung scheiterte: was
/// gewollt war und woran es lag, waere sonst nur aus der Spanne zu raten.
static ARENA_PLAN: SpinLock<(usize, usize, u32, bool)> =
    SpinLock::new((0, 0, crate::stack_arena::GRUND_THREADS, false));

/// Der Arena-Plan fuer den Bericht: `(gewollte Faeden, Plaetze, Grund, bestueckt)`.
/// Die Klartexte zu `Grund` liefert `stack_arena::grund_name`. `(0, .., false)` heisst
/// „nicht bestueckt" — derselbe Stil wie `kstack_arena_stats`.
pub fn kstack_arena_plan() -> (usize, usize, u32, bool) {
    *ARENA_PLAN.lock()
}

/// C7c/Dichte: Arena-Plaetze ableiten statt Fixzahl (einmalig in `configure`, vor dem ersten
/// Thread). Gibt die belegten Bytes zurueck (Spanne + Bitmap, fuer den Boot-Report),
/// `0` = nicht bestueckt.
fn kstack_arena_bestuecken(faeden: usize) -> u64 {
    use crate::stack_arena::{
        arena_plaetze, slots_je_block, volle_pd_kosten, StackArena, ARENA_DECKEL_PLAETZE,
        BLOCK_2M, GRUND_BITMAP, GRUND_GEOMETRIE, GRUND_SPANNE,
    };
    use crate::stack_arena::woerter;
    let stapel = USER_KSTACK_SIZE;
    let schritt = caprock_mem::PAGE as usize + stapel;
    // Guard-Vorrat: freie Splits mal Dichte je Block — gelesen, nicht geraten. Die Streu-Stacks
    // des Selbsttests haben zu diesem Zeitpunkt bereits Bloecke aufgeteilt; ein Slot ohne
    // legbare Wache wuerde beim Bestuecken gesperrt und waere belegtes, nie nutzbares RAM.
    // Nur wo Wachen echt sind — wo der Aufruf nur zaehlt, begrenzt kein Split.
    let guard_plaetze = if hal::mmu::guard_unterstuetzt() {
        let (_, _, _, belegt, vorrat) = hal::mmu::guard_stats();
        Some(
            vorrat
                .saturating_sub(belegt)
                .saturating_mul(slots_je_block(schritt)),
        )
    } else {
        None
    };
    let frei = MEM.lock().total_free();
    let (plaetze, grund) = arena_plaetze(
        faeden,
        frei,
        volle_pd_kosten(schritt),
        guard_plaetze,
        ARENA_DECKEL_PLAETZE,
    );
    *ARENA_PLAN.lock() = (faeden, plaetze, grund, false);
    if plaetze == 0 {
        return 0;
    }
    // Dieselbe Zone wie die Streu-Stacks heute (`IdentityMapped`, keine neue Annahme
    // ueber die Identitaetskarte), aber EIN zusammenhaengender Bereich, 2-MiB-ausgerichtet:
    // so fallen die Wachen in wenige Kartenbloecke statt in hunderte gestreute.
    let Some(cap) = mem_alloc((plaetze as u64) * (schritt as u64), BLOCK_2M as u64) else {
        *ARENA_PLAN.lock() = (faeden, plaetze, GRUND_SPANNE, false);
        return 0;
    };
    let basis = cap.base() as usize;
    // Die Bitmap kommt aus Boot-RAM statt BSS (kein BSS-Sprengen): ein Wort je 64 Plaetze,
    // reines Kernel-Wissen, also aus der oberen Zone wie die Tabellen in `configure`.
    let w = woerter(plaetze);
    let Some(bcap) = mem_alloc_anywhere((w as u64) * 8, 8) else {
        MEM.lock().free_region(PhysRegion::new(cap.base(), cap.len()));
        *ARENA_PLAN.lock() = (faeden, plaetze, GRUND_BITMAP, false);
        return 0;
    };
    // Die Caps bewusst fallen lassen (Muster `table` in `configure`): Arena + Bitmap leben
    // bis Reboot.
    // SAFETY: einmaliger Aufruf beim Boot, bevor ein anderer Kern oder Thread laeuft; frisch
    // allozierte, exklusive, u64-ausgerichtete Region von genau `w` Worten; danach wird sie
    // ausschliesslich durch die Arena unter `KSTACK_ARENA` angefasst, nie wieder direkt.
    let bits: &'static mut [u64] =
        unsafe { core::slice::from_raw_parts_mut(bcap.base() as *mut u64, w) };
    let mut arena = match StackArena::neu(basis, stapel, plaetze, bits) {
        Ok(a) => a,
        Err(_) => {
            MEM.lock().free_region(PhysRegion::new(cap.base(), cap.len()));
            MEM.lock().free_region(PhysRegion::new(bcap.base(), bcap.len()));
            *ARENA_PLAN.lock() = (faeden, plaetze, GRUND_GEOMETRIE, false);
            return 0;
        }
    };
    // KEIN Vorab-Legen der Wachen (2026-09-10, Root-NoResources): eine Schleife ueber alle
    // Plaetze rief `guard_unmap` je Slot -- bei ~4000 Plaetzen waren damit fast alle 4096
    // Guard-Pages des HAL-Vorrats beim Bestuecken schon vergeben, und jeder weitere
    // Streu-Stack (hello!) fiel mit MANGEL_GUARD_TABELLE. Wachen werden stattdessen LAZY
    // bei der ersten Vergabe gelegt (s. `kstack_arena_vergeben`): was nie lebt, kostet
    // keine Wache; belegte Bloecke + Pages folgen den lebenden Stacks, nicht der Planzahl.
    *KSTACK_ARENA.lock() = Some(arena);
    *ARENA_PLAN.lock() = (faeden, plaetze, grund, true);
    cap.len() + bcap.len()
}

/// C7c: Arena-Vergabe (nur ungefaerbter Pfad; `None` = Streupfad weiter).
///
/// Die Wache wird LAZY gelegt (s. `kstack_arena_bestuecken`): erst pruefen, ob sie schon
/// steht (`ist_wache` -- wiederverwendete Slots behalten ihre), sonst legen; schlaegt das
/// Legen fehl (Blockvorrat erschoepft), wird genau dieser Slot gesperrt und der naechste
/// freie versucht (Retry-Schleife, begrenzt durch die Platzahl -- kein endloses Kreisen).
/// aarch64 kennt keine echten Wachen (`guard_unterstuetzt() == false`): dort zaehlt die
/// Vergabe wie im Streupfad (`unbewachte_stacks`), und jeder Slot gilt sofort.
fn kstack_arena_vergeben() -> Option<usize> {
    let mut arena_guard = KSTACK_ARENA.lock();
    let arena = arena_guard.as_mut()?;
    loop {
        let base = arena.vergeben().ok()?;
        let Some(slot) = arena.slot_von_stapelbasis(base) else {
            continue;
        };
        if !hal::mmu::guard_unterstuetzt() {
            // Kein Fail-closed ohne Wachen (wie im Streupfad) -- aber auch kein Schweigen:
            // `guard_unmap` zaehlt dort nur (`unbewachte_stacks`), und ohne diesen Aufruf
            // waere jeder Arena-Slot ein unbewachter Stack, den keine Zahl nennt.
            if let Some(wache) = arena.wache_von_slot(slot) {
                let _ = hal::mmu::guard_unmap(wache as u64);
            }
            return Some(base);
        }
        let Some(wache) = arena.wache_von_slot(slot) else {
            arena.sperren(slot);
            continue;
        };
        if hal::mmu::ist_wache(wache as u64) {
            return Some(base);
        }
        if hal::mmu::guard_unmap(wache as u64) {
            return Some(base);
        }
        arena.sperren(slot);
    }
}

/// C7c: Arena-Freigabe — `true` heisst „war ein Arena-Slot, ist gebucht".
fn kstack_arena_freigeben(stapelbasis: usize) -> bool {
    match KSTACK_ARENA.lock().as_mut() {
        Some(a) => a.freigeben(stapelbasis).is_ok(),
        None => false,
    }
}

/// C7c: Arena-Telemetrie fuer den Bericht
/// `(plaetze, vergeben, frei, hoechststand, abg_erschoepft, abg_ungueltig, promille)`.
/// `(0, ..)` heisst „nicht bestueckt" — derselbe Stil wie `ZONE_MISSED` (0 = kam nicht vor).
pub fn kstack_arena_stats() -> (usize, usize, usize, usize, u64, u64, usize) {
    match KSTACK_ARENA.lock().as_ref() {
        Some(a) => (
            a.plaetze(),
            a.vergeben_anzahl(),
            a.frei(),
            a.hoechststand(),
            a.abgewiesen_erschoepft(),
            a.abgewiesen_ungueltig(),
            a.auslastung_promille(),
        ),
        None => (0, 0, 0, 0, 0, 0, 0),
    }
}

/// Einen EL0-Kernel-Stack (`USER_KSTACK_SIZE` -- 4 KiB auf x86, 16 KiB auf aarch64 --
/// plus Wache) aus `MEM` allozieren. Gibt die **physische Basis**
/// zurück (identity-gemappt = EL1-SP-Region), oder `None` bei RAM-Erschöpfung.
/// **Was fuer EINEN EL0-Kernel-Stack wirklich angefordert wird**: der Stack plus seine Wache.
///
/// Die Konstante steht hier und nicht bei den Pruefern: eine zweite Quelle fuer dieselbe Zahl war
/// genau die Falle, wegen der `mangel(MANGEL_SEITENTABELLE, 4096)` als Literal entstehen konnte.
/// Wer die Wache abschafft oder vergroessert, aendert diese Zeile -- und jede Pruefung, die den
/// gemeldeten Betrag gegenliest, folgt automatisch.
pub const USER_KSTACK_ALLOC: u64 = USER_KSTACK_SIZE as u64 + caprock_mem::PAGE;

fn claim_user_kstack() -> Option<usize> {
    claim_user_kstack_masked(None, caprock_hal::numa::Node::Unaffiliated)
}
/// Wie [`claim_user_kstack`], aber optional aus einem Farbsatz (todo A1) und mit
/// Knotenwunsch (Z8/N3: `LadePolitik.node`, gesetzt aus `manifest.numa_node`).
///
/// Der Kernel-Stack einer PD wird zwar vom Kernel benutzt, aber **im Namen dieses Subjekts** —
/// seine Cache-Zeilen tragen also dessen Zugriffsmuster. Ihn ungefärbt zu lassen hieße, die
/// Trennung an genau der Stelle aufzugeben, an der der Kernel für das Subjekt arbeitet.
/// Der Stack (4 KiB = 1 Seite auf x86, 16 KiB = 4 Seiten auf aarch64) passt damit in jeden
/// Streifen (kleinster Streifen: 16 Seiten).
///
/// **Die Regel, nach der Knoten und Farbe nachgeben (Z8/N3):** ein Knotenwunsch ist eine
/// Platzierung, keine Zusicherung — was ihn nicht erfuellt, faellt BENANNT zurueck
/// (`kstack : RUECKFALL`), nie auf einen Fehler. Der Boot hinge sonst an der Topologie der
/// Maschine. Die Reihenfolge ist die der Leiter: erst gibt der Knoten nach (gefaerbt ohne
/// Knoten), dann die Farbe (ungefaerbt/unaffiliated). Ohne Knotenwunsch laeuft keine dieser
/// Stufen: die rein gefaerbte Anforderung bleibt fail-closed (A1), und der rein ungefragte
/// Pfad ist bitgleich der alte.
fn claim_user_kstack_masked(
    mask: Option<caprock_mem::ColorMask>,
    node: caprock_hal::numa::Node,
) -> Option<usize> {
    let (sz, al) = (USER_KSTACK_SIZE as u64, USER_KSTACK_SIZE as u64);
    // **Der Topf benennt sich hier, nicht bei seinen fuenf Aufrufern.** Ein Aufrufer, der den
    // Mangel selbst hinschreibt, muss die Groesse ein zweites Mal nennen -- und eine zweite
    // Quelle fuer dieselbe Zahl ist die Falle, wegen der `mangel(MANGEL_SEITENTABELLE, 4096)`
    // ueberhaupt entstehen konnte.
    // **Eine Seite mehr, und die unterste wird die WACHE.** Gemessen am 2026-08-10: der tiefste
    // Kernelpfad (`SYS_LOAD` -> Ed25519 + SHA-2 im Kernel) benutzt 73 % dieses Stacks. Ohne Wache
    // schriebe ein Ueberlauf still in den Nachbarn -- und der Pfad ist aus Ring 3 erreichbar,
    // also ist das eine Privilegieneskalation und keine Diagnosefrage.
    //
    // **Eine Seite Aufschlag, nicht eine Stackgroesse.** Die erste Fassung forderte `sz + al` mit
    // Ausrichtung `al` an, um den Stack ausgerichtet zu lassen -- 16 KiB Verschnitt je 16 KiB
    // Stack, also 50 %. Der Stack braucht diese Ausrichtung nicht (die CPU verlangt fuer `rsp0`
    // 16 Byte); die Wache dagegen muss seitenausgerichtet sein. Also: Seitenausrichtung, eine
    // Seite Aufschlag, Wache ganz unten.
    let _ = al;
    // **Die Wache steht UNTER keiner Farbbedingung** — und ohne diesen Halbsatz ist die gefaerbte
    // Anforderung auf aarch64 strukturell unerfuellbar (C9e, 2026-08-13).
    //
    // Die Rechnung: 16 Farben, 4 Partitionen -> ein Streifen ist **4** Farben breit, und ein
    // EL0-Kernel-Stack ist dort 16 KiB = **4** Seiten. Er passte also GENAU, solange er allein
    // angefordert wurde. Mit der Wache sind es **5** aufeinanderfolgende Seiten, und fuenf
    // aufeinanderfolgende Seiten tragen fuenf VERSCHIEDENE Farben — in einen 4-Farben-Streifen
    // passen sie nie, an keiner Adresse, bei keinem Fuellstand. `alloc_colored` gab damit immer
    // `None`, `spawn_isolated_colored` ebenso, und der A1-Farbtest meldete lauter Nullen.
    //
    // Warum es die Wache nicht kostet: sie traegt keine Daten. Auf x86 ist sie unabgebildet, auf
    // aarch64 gibt es sie gar nicht (`guard_unterstuetzt() == false`, `guard_unmap` zaehlt nur).
    // Eine Seite, die nie gelesen und nie geschrieben wird, belegt kein Cache-Set — die
    // Farbzusicherung sagt nichts ueber sie aus, und sie in den Streifen zu zwingen war keine
    // schaerfere Zusicherung, sondern eine Seite ueber der Streifenbreite.
    //
    // **Ausrichtung `PAGE`, nicht `al`.** Ausgerichtet werden musste hier ohnehin nie der Block,
    // sondern die Wache (seitenausgerichtet) — das stand schon oben. Mit `al` waere `roh`
    // 16-KiB-ausgerichtet und damit der STACK bei `roh + PAGE` gerade NICHT streifenausgerichtet;
    // die Farbbedingung des gefaerbten Teils waere wieder unerfuellbar. Der Stack braucht die
    // 16-KiB-Ausrichtung nicht (die CPU verlangt fuer `rsp0` 16 Byte).
    // C7c: Arena zuerst — aber NUR ohne jede Politik. Die Arena kennt weder Farben noch
    // Knoten (Kopplung A-1.4/B-4); ein knotenlokaler Stack aus dem Arena-Topf waere eine
    // Platzierungszusage, die niemand einloest. Gefaerbte Anforderungen (`mask`) laufen weiter
    // ueber den Streupfad: die Arena kennt keine Farben. Arena voll oder nicht bestueckt:
    // Streupfad unten (benennt den Mangel selbst).
    if mask.is_none() && matches!(node, caprock_hal::numa::Node::Unaffiliated) {
        if let Some(base) = kstack_arena_vergeben() {
            // SAFETY: exklusiv vergebener Slot, identity-gemappt — wie Streupfad unten.
            unsafe { crate::kstackmark::fuellen(crate::kstackmark::KL_EL0, base, sz as usize) };
            KSTACKS.lock().live += 1;
            return Some(base);
        }
    }
    let roh = benannt_alloc(MANGEL_KERNEL_STACK, USER_KSTACK_ALLOC, |n| match (mask, node) {
        // Ueber die Politik aus `mem_alloc`/`alloc_colored` (unten zuerst), nicht daran vorbei:
        // eine Vorgabe, an der eine einzige Stelle vorbeigreift, ist keine Vorgabe (E-Rest 3b).
        // Mit Knotenwunsch gilt zusaetzlich die Leiter (s. Funktionsdoku): der Knoten gibt
        // zuerst nach, die Farbe zuletzt — beides benannt, nie ein Fehler aus Topologie.
        (Some(m), caprock_hal::numa::Node::At(wunsch)) => kstack_gefaerbt_auf_knoten(n, m, wunsch),
        (Some(m), _) => alloc_colored_vorspann(1, n, caprock_mem::PAGE, m),
        (None, caprock_hal::numa::Node::At(wunsch)) => kstack_auf_knoten(n, al, wunsch),
        (None, _) => mem_alloc(n, al),
    })?
    .base();
    let wache = roh; // die unterste Seite des Blocks
    if !hal::mmu::guard_unterstuetzt() {
        let _ = hal::mmu::guard_unmap(wache); // zaehlt nur -- s. aarch64-HAL
    }
    let base = roh + caprock_mem::PAGE;
    // **Fail-closed.** Der Vorrat aufgeteilter Bloecke ist fest (s. `mmu::guard_unmap`); reicht
    // er nicht, wird die Anforderung BENANNT abgewiesen. Ein Stack ohne Wache waere die stille
    // Fassung genau des Fehlers, gegen den die Wache gebaut ist.
    // **„Vorrat leer" und „diese Architektur kann es nicht" sind zwei Antworten.** Nur die erste
    // ist ein Grund abzuweisen; die zweite darf nicht fail-closed sein, sonst koennte aarch64
    // keinen einzigen EL0-Thread mehr anlegen. Verschwiegen wird sie trotzdem nicht: die
    // aarch64-HAL zaehlt jeden unbewachten Stack, und die Zahl steht im Bericht.
    if hal::mmu::guard_unterstuetzt() && !hal::mmu::guard_unmap(wache) {
        MEM.lock().free_region(PhysRegion::new(roh, sz + caprock_mem::PAGE));
        mangel(MANGEL_GUARD_TABELLE, caprock_mem::PAGE);
        return None;
    }
    // C4: Wasserstandsmarke. **Hier und nicht spaeter** — `init_thread_frame` legt gleich den
    // Startframe an den Stack-Top; wer danach fuellt, ueberschreibt ihn, und der Thread spraenge
    // nach `MUSTER`.
    // SAFETY: frisch allozierte, exklusiv gehaltene, auf `USER_KSTACK_SIZE`
    // ausgerichtete Region; identity-gemappt.
    unsafe { crate::kstackmark::fuellen(crate::kstackmark::KL_EL0, base as usize, sz as usize) };
    KSTACKS.lock().live += 1;
    Some(base as usize)
}
/// Gefaerbt UND in einem Physfenster, mit Wachseiten-Vorspann (Z8/N3).
///
/// Die Fensterfassung von [`alloc_colored_vorspann`] fuer die knotenlokale Kstack-Vergabe:
/// die erste Seite (`vorspann`) steht wie dort unter keiner Farbbedingung (sie traegt keine
/// Daten und belegt kein Cache-Set), der Rest kommt aus `[lo, hi)`. Wie
/// [`alloc_colored_in_window`] eine Bedingung, keine Vorliebe: `None` statt ausserhalb.
fn alloc_colored_vorspann_in_fenster(
    vorspann: u64,
    size: u64,
    align: u64,
    mask: caprock_mem::ColorMask,
    lo: u64,
    hi: u64,
) -> Option<MemoryCap> {
    #[cfg(feature = "selftest")]
    if sperre_greift(size) {
        return None;
    }
    let cap = MEM.lock().alloc_colored_vorspann_in(
        vorspann,
        size,
        align,
        crate::colors::count(),
        mask,
        lo,
        hi,
    )?;
    zero_phys(cap.base(), cap.len());
    Some(cap)
}

/// Gefaerbter Kstack mit Knotenwunsch (Z8/N3) — die Leiter als Schleife.
///
/// Erst fensterweise auf dem Knoten (exakt), dann gefaerbt ohne Knoten (der Knoten gibt
/// nach), dann ungefaerbt (die Farbe gibt zuletzt nach). Jede Stufe ist benannt: ein stiller
/// Farbverzicht waere der `MASK_BITS`-Fehler noch einmal (gruen, obwohl nichts getrennt war).
/// Echte Erschoepfung (auch ungefaerbt nichts frei) meldet der Aufrufer (`benannt_alloc`).
fn kstack_gefaerbt_auf_knoten(
    n: u64,
    m: caprock_mem::ColorMask,
    wunsch: u8,
) -> Option<MemoryCap> {
    let top = crate::numa::topology();
    if top.trustworthy() {
        let mut i = 0;
        while let Some((lo, hi)) = top.window_of(wunsch, i) {
            if let Some(c) = alloc_colored_vorspann_in_fenster(1, n, caprock_mem::PAGE, m, lo, hi) {
                return Some(c);
            }
            i += 1;
        }
    }
    #[cfg(feature = "debug-log")]
    println!(
        "kstack  : RUECKFALL knotenlokal-gefaerbt unerfuellbar (Knoten {wunsch}) -- gefaerbt ohne Knoten"
    );
    if let Some(c) = alloc_colored_vorspann(1, n, caprock_mem::PAGE, m) {
        return Some(c);
    }
    #[cfg(feature = "debug-log")]
    println!("kstack  : RUECKFALL gefaerbt erschoepft -- ungefärbt/unaffiliated (Farbe gibt zuletzt nach)");
    mem_alloc(n, USER_KSTACK_SIZE as u64)
}

/// Ungefaerbter Kstack mit Knotenwunsch (Z8/N3).
///
/// `alloc_on_node` traegt die Leiter selbst (exakt, dann irgendwo — gezaehlt nach Wirkung);
/// was hier noch fehlschlaegt, ist echte Erschoepfung auf dem Wunschpfad, und dann gilt die
/// Regel: benannt zurueck auf ungefärbt/unaffiliated, nie ein Fehler aus Topologie.
fn kstack_auf_knoten(n: u64, al: u64, wunsch: u8) -> Option<MemoryCap> {
    let knoten = caprock_hal::numa::Node::At(wunsch);
    match crate::numa::alloc_on_node(n, al, knoten) {
        Some(c) => Some(c),
        None => {
            #[cfg(feature = "debug-log")]
            println!(
                "kstack  : RUECKFALL Knoten {wunsch} ohne Speicher -- ungefärbt/unaffiliated"
            );
            mem_alloc(n, al)
        }
    }
}
/// Einen (noch keinem Thread zugeordneten) Kstack wieder an `MEM` freigeben (Fehlerpfad vor `record`).
fn release_user_kstack(base: usize) {
    // C7c: Arena-Slot — keine `guard_remap`, keine MEM-Rueckgabe je Stack; nur Bit loeschen.
    if kstack_arena_freigeben(base) {
        let mut p = KSTACKS.lock();
        p.live = p.live.saturating_sub(1);
        return;
    }
    // **Die Wache zuerst zurueck in die Karte** -- sonst gaebe der Allokator eine Seite heraus,
    // die niemand mehr erreichen kann, und der naechste Zugriff darauf waere ein #PF an einer
    // Adresse, die laut Freiliste in Ordnung ist.
    let roh = base as u64 - caprock_mem::PAGE;
    hal::mmu::guard_remap(roh);
    MEM.lock().free_region(PhysRegion::new(roh, USER_KSTACK_SIZE as u64 + caprock_mem::PAGE));
    let mut p = KSTACKS.lock();
    p.live = p.live.saturating_sub(1);
}
/// Den Kstack `base` dem Thread-Slot zuordnen (nach erfolgreichem spawn) -> Reclaim beim Thread-Ende.
///
/// **MUSS unter `SCHEDS[core]` aufgerufen werden, im selben kritischen Abschnitt wie
/// `sched.spawn_user()`** — zusammen mit [`set_vspace_of`], und *nach* `fp_reset_slot` (das setzt
/// `VSPACE_OF[slot] = 0`).
///
/// Grund: `spawn_user` macht den Thread lauffaehig. Steht die Buchfuehrung erst *hinter* dem
/// kritischen Abschnitt, laeuft er zwischen Freigabe und Eintrag bereits — ein Timer-Tick genuegt,
/// SMP braucht es dafuer nicht. Faultet er in diesem Fenster (der Isolationstest faultet
/// ABSICHTLICH) und wird eingesammelt, sieht `reclaim_user_kstack` `base_of[slot] == 0` und gibt
/// den Kernel-Stack **nie** frei; `set_vspace_of` schreibt danach in einen Slot, der bereits neu
/// vergeben sein kann. Sichtbar wurde das als `color : FAILURES` mit `rueckgelesen=0` in 4 von 400
/// Laeufen — das Leck selbst sucht bis heute kein Test.
///
/// Sperrordnung: `KSTACKS` ist ein Blatt (wie `FP_STATES`) und wird nirgends *ausserhalb* von
/// `SCHEDS` gehalten, waehrend `SCHEDS` genommen wird — jeder Reclaim-Pfad gibt `SCHEDS` vorher
/// frei (s. die Kommentare „KSTACKS allein"). Die Kante `SCHEDS -> KSTACKS` ist damit zyklusfrei.
fn record_user_kstack(tid: ThreadId, base: usize) {
    let mut p = KSTACKS.lock();
    p.base_of[tid.slot()] = base as u64;
    // D15: **die Identitaet, nicht nur der Index.** Wer den Stack zurueckgibt, muss belegen
    // koennen, dass er derselbe Thread ist, der ihn genommen hat.
    p.gen_of[tid.slot()] = tid.gen();
}

/// **C7b: die EL0-USER-Stackregion dieses Threads buchen.** Steht neben [`record_user_kstack`]
/// und wird im selben kritischen Abschnitt gerufen — aus demselben Grund: was hinter der Freigabe
/// von `SCHEDS` gebucht wird, kann der Thread bereits überholt haben.
///
/// `(0, 0)` heisst „dieser Thread hat keine EL0-Region, die ihm gehört". Das ist kein Ausfall,
/// sondern eine benannte Lage: bei einem **gefärbt geladenen** Programm liegt der Stack in
/// mehreren Stücken in der `seglist` der PD und gehört nicht dem Thread (er überlebt dessen Tod
/// bis zum PD-Abbau). Eine Buchführung, die dort eine Region behauptete, zeigte auf Speicher, den
/// ein anderer freigibt.
fn record_user_region(thread_slot: usize, base: u64, len: u64) {
    if base == 0 || len == 0 {
        return;
    }
    {
        let mut p = KSTACKS.lock();
        // Ohne `selftest` sind die Tabellen gar nicht angehaengt (s. `configure`) -- dann ist die
        // Laenge 0, und die Buchung entfaellt, statt daneben zu greifen.
        if thread_slot >= p.ubase_of.len() {
            return;
        }
        p.ubase_of[thread_slot] = base;
        p.ulen_of[thread_slot] = len;
    }
    crate::userstackmark::registriert();
}
/// Beim Thread-Ende: den ggf. zugeordneten Kstack an `MEM` zurückgeben. No-Op für EL1-Threads.
fn reclaim_user_kstack(tid: ThreadId, _anlass: &'static str) {
    let thread_slot = tid.slot();
    KSTACK_RECLAIM_GESAMT.fetch_add(1, Ordering::Relaxed);
    // **D15-Melder 1 (Gelegenheit).** Ist der Slot in diesem Moment schon wieder belegt, bezeichnet
    // die `gid` einen FREMDEN, lebenden Thread — jede Aufraeumung, die nur den Slot in der Hand
    // hat, trifft ab hier ihn. Gezaehlt wird das unabhaengig davon, ob es diesmal schadet.
    if caprock_sched::slot_in_use(thread_slot) {
        KSTACK_SPAET_SLOT.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "debug-log")]
        println!(
            "kstackid: SPAET via {_anlass} -- Slot {thread_slot} ist bereits wieder belegt \
             (aufgeraeumt wird fuer Generation {})",
            tid.gen()
        );
    }
    let base = {
        let mut p = KSTACKS.lock();
        let b = p.base_of[thread_slot];
        // **D15-Melder 2 (Treffer).** Steht ein Stack im Slot, dessen eingetragene Generation eine
        // andere ist als die des Sterbenden, dann gehoert er dem NACHFOLGER. Ihn freizugeben hiesse,
        // einem lebenden Thread den Kernel-Stack unter dem Frame wegzuziehen — und weil jede Vergabe
        // durch `zero_phys` geht, waere sein gesicherter TrapFrame danach genullt: `elr=0`,
        // `spsr=0` (= EL0t). Genau das Bild von D15.
        let fremd = b != 0 && p.gen_of[thread_slot] != tid.gen();
        if fremd {
            KSTACK_FREMD.fetch_add(1, Ordering::Relaxed);
            // **Laut, nicht nur gezaehlt.** Der Zaehler steht am Ende des Laufs; ein Lauf, der
            // vorher stirbt, verliert ihn -- und genau die sterben interessieren. Die Zeile nennt
            // Opfer und Region, damit ein spaeterer Sprung nach 0 zuzuordnen ist.
            #[cfg(feature = "debug-log")]
            println!(
                "kstackid: FREMD via {_anlass} -- Slot {thread_slot} traegt Kstack {b:#x} der \
                 Generation {}, aufgeraeumt wird aber fuer Generation {} \
                 (Leck statt UAF: nicht freigegeben)",
                p.gen_of[thread_slot],
                tid.gen()
            );
        }
        // **LECK STATT UAF** -- dieselbe Entscheidung wie beim Teardown-Token (ext-37). Der eigene
        // Stack dieses Threads ist in diesem Fall ohnehin schon verloren: der Nachfolger hat den
        // Eintrag beim `record_user_kstack` ueberschrieben, und eine Tabelle ueber dem SLOT hat
        // dafuer keinen zweiten Platz. Ihn *ersatzweise* freizugeben heisst, den Stack eines
        // LEBENDEN Threads herzugeben -- der Schaden ist dann nicht ein verlorenes Fragment,
        // sondern ein genullter TrapFrame und ein Sprung nach 0.
        if b != 0 && !fremd {
            p.base_of[thread_slot] = 0;
            p.gen_of[thread_slot] = 0;
            p.live = p.live.saturating_sub(1);
        }
        if fremd {
            0
        } else {
            b
        }
    };
    if base != 0 {
        KSTACK_RECLAIM_MIT_STACK.fetch_add(1, Ordering::Relaxed);
        // **D15-Melder 3 (deterministisch, kein Rennen).** Der haeufigste Aufrufweg ist
        // `exit_current`/`el0_fault` — also der sterbende Thread SELBST, und auf aarch64 laeuft der
        // Trap-Handler auf SP_EL1, also auf genau diesem Stack (die Vektortabelle hat keinen
        // eigenen Exception-Stack). Die Region geht hier an den Allokator zurueck, waehrend der
        // Kernel noch darauf rechnet; ein anderer Kern darf sie ab diesem Befehl holen und nullen.
        let hier = stapeladresse() as u64;
        let roh_u = base - caprock_mem::PAGE;
        if hier >= roh_u && hier < roh_u + USER_KSTACK_ALLOC {
            KSTACK_UNTER_FUESSEN.fetch_add(1, Ordering::Relaxed);
        }
        // C4: **hier** wird der Wasserstand abgelesen, nicht im Bericht — beim Tod des Threads
        // steht sein tiefster Pfad noch im Speicher, nach der Freigabe gehoert die Region dem
        // naechsten Anforderer. Der haeufigste Aufrufweg ist `exit_current`, also der sterbende
        // Thread SELBST auf eben diesem Stack: das Lesen ist gefahrlos (IRQs maskiert), und die
        // Tiefe des Messrahmens gehoert ehrlicherweise mit zur gemessenen Tiefe.
        // SAFETY: die Region gehoert bis zum `free_region` unten noch diesem Thread.
        unsafe {
            crate::kstackmark::messen(
                crate::kstackmark::KL_EL0,
                base as usize,
                USER_KSTACK_SIZE,
                crate::kstackmark::Anlass::Tod(thread_slot),
            )
        };
        // **Die Wache zurueck in die Karte, und den GANZEN Block freigeben** -- der Stack ist
        // die obere Haelfte einer doppelt so grossen Anforderung (s. `claim_user_kstack_masked`).
        //
        // Hier stand zuerst nur `free_region(base, USER_KSTACK_SIZE)`. Das war die Fassung von
        // vor der Guard-Page, und sie hat den Umbau ueberlebt: die untere Haelfte blieb belegt,
        // die Wache blieb aus der Karte -- und der naechste Ring-3-Thread faultete auf seinem
        // eigenen Stack. Sechs `el0-trap`-Zeilen, `ring3`/`iso`/`park` rot. **Wer eine
        // Anforderung vergroessert, muss JEDEN Freigabepfad mitnehmen, nicht den erstbesten.**
        let roh = base - caprock_mem::PAGE;
        // C7c: Arena-Slot oder Streu-Block — die Buchfuehrung oben (`base_of`/`live`) ist
        // in beiden Faellen schon bereinigt; hier geht es nur um Karte + Allokator.
        // Ein Arena-Slot braucht weder `guard_remap` (Wache liegt im Arena-Block) noch
        // `free_region` (die Arena lebt bis Reboot); ein Streu-Block braucht beides.
        if !kstack_arena_freigeben(base as usize) {
            hal::mmu::guard_remap(roh);
            MEM.lock()
                .free_region(PhysRegion::new(roh, USER_KSTACK_SIZE as u64 + caprock_mem::PAGE));
        }
    }
}
/// **C4: ueber alle NOCH LEBENDEN EL0-Kstacks fegen** und ihren Wasserstand einrechnen. Gibt
/// `(gefegt, tiefster_slot)` zurueck.
///
/// Warum das gebraucht wird: [`reclaim_user_kstack`] misst nur die Stacks **sterbender** Threads.
/// Die langlebigen (IPC-Server, Treiber-PDs, Root-Task) sterben in einem gruenen Lauf nie — und
/// gerade sie fahren die Pfade, um die es geht. Ein Wasserstand nur aus den Toten waere eine
/// Stichprobe mit Auswahlfehler.
///
/// Gefegt wird am **Schluss** des Laufs (im Bericht), nicht laufend: die Schleife nimmt `KSTACKS`
/// und liest fremde Stacks, waehrend deren Threads darauf rechnen koennen. Das ist fuer die
/// **Messung** harmlos (nur Lesen, und ein gleichzeitig tiefer werdender Stack kann den Messwert
/// nur zu **klein** machen, nie zu gross), waere aber Rauschen in jeder baseline-empfindlichen
/// Zeile davor.
pub fn kstack_marke_fegen() -> (usize, usize) {
    let mut gefegt = 0usize;
    let mut tiefster = usize::MAX;
    let mut tiefe = 0usize;
    // Gemessen wird UNTER der Sperre, und das ist die Entscheidung gegen einen Zwischenpuffer:
    // ein Feld fester Groesse haette die Liste stillschweigend abgeschnitten, sobald mehr Threads
    // leben als es Plaetze hat — bei 1024 lebenden Threads und 64 Plaetzen waere `gefegt=64` von
    // „mehr gibt es nicht" nicht zu unterscheiden. `KSTACKS` ist ein Blattlock (es wird nirgends
    // ein weiteres genommen, waehrend es gehalten wird), die Schleife kann also nur warten lassen,
    // nicht verklemmen — und sie laeuft am Schluss des Laufs, wo Wandzeit nichts mehr kippt.
    let p = KSTACKS.lock();
    for slot in 0..p.base_of.len() {
        let b = p.base_of[slot];
        if b == 0 {
            continue;
        }
        // SAFETY: der Kstack liegt identity-gemappt im RAM; gelesen wird nur.
        let (benutzt, _) = unsafe {
            crate::kstackmark::messen(
                crate::kstackmark::KL_EL0,
                b as usize,
                USER_KSTACK_SIZE,
                crate::kstackmark::Anlass::Fegen(slot),
            )
        };
        gefegt += 1;
        if benutzt > tiefe {
            tiefe = benutzt;
            tiefster = slot;
        }
    }
    (gefegt, tiefster)
}

/// **C7b: über alle noch LEBENDEN EL0-User-Regionen fegen** und ihren Wasserstand einrechnen.
/// Gibt `(gefegt, tiefster_slot)`.
///
/// Gegenstück zu [`kstack_marke_fegen`] und aus demselben Grund nötig: der Sterbepfad misst nur
/// die Regionen sterbender Threads. Die langlebigen (Root-Task, Treiber-PDs, IPC-Server) sterben
/// in einem grünen Lauf nie — und gerade sie fahren die tiefsten Userland-Pfade.
pub fn userstack_marke_fegen() -> (usize, usize) {
    let mut gefegt = 0usize;
    let mut tiefster = usize::MAX;
    let mut tiefe = 0usize;
    // Wie beim Kstack-Fegen: gemessen wird UNTER dem Blattlock (kein Zwischenpuffer, der die
    // Liste stillschweigend abschneiden könnte), am Schluss des Laufs, wo Wandzeit nichts kippt.
    let p = KSTACKS.lock();
    for slot in 0..p.ubase_of.len() {
        let (b, l) = (p.ubase_of[slot], p.ulen_of[slot]);
        if b == 0 || l == 0 {
            continue;
        }
        // SAFETY: die Region ist identity-gemappt und gehört diesem (lebenden) Thread; gelesen
        // wird nur, und ein gleichzeitig tiefer werdender Stack macht den Messwert nur kleiner.
        let (benutzt, _) = unsafe {
            crate::userstackmark::messen(
                b as usize,
                l as usize,
                crate::userstackmark::Anlass::Lebend(slot),
            )
        };
        gefegt += 1;
        if benutzt > tiefe {
            tiefe = benutzt;
            tiefster = slot;
        }
    }
    (gefegt, tiefster)
}

/// **C7b/C7: die LEBENDIGKEIT der gezählten Prozesse** — `(lebende EL0-Regionen, davon benutzt)`.
///
/// **Warum diese Zeile existiert.** Die isolierte Kapazitätskurve hat schon einmal Leichen
/// gezählt: `kurven_arbeiter` lag in `.text`, jeder Thread faultete an seiner Einsprungadresse,
/// und „3040 isolierte Prozesse" hiess in Wahrheit „3040 mal eine PD angelegt, deren Thread sofort
/// starb". Aufgefallen ist es an einer Nebenbeobachtung (228 `el0-trap`-Zeilen), nicht an einer
/// Prüfung.
///
/// Hier ist es eine Prüfung, und sie hängt nicht an einer Fehlermeldung: der Arbeiter der Kurve
/// legt vor dem Parken ein von Null verschiedenes Wort auf **seinen** Stack. Eine Region mit Tiefe
/// `0` gehört damit zu einem Thread, der nie eine Instruktion ausgeführt hat. Gezählt wird also
/// die WIRKUNG (er hat gerechnet), nicht ein Zustand (er ist eingetragen).
pub fn userstack_lebendig_zaehlen() -> (usize, usize) {
    let mut lebend = 0usize;
    let mut benutzt = 0usize;
    let p = KSTACKS.lock();
    for slot in 0..p.ubase_of.len() {
        let (b, l) = (p.ubase_of[slot], p.ulen_of[slot]);
        if b == 0 || l == 0 {
            continue;
        }
        lebend += 1;
        // SAFETY: identity-gemappte Region eines lebenden Threads; gelesen wird nur.
        if unsafe { crate::userstackmark::unberuehrt(b as usize, l as usize) } < l as usize {
            benutzt += 1;
        }
    }
    (lebend, benutzt)
}

/// **C7b: die Tiefensonde nachmessen** — die Sprechprobe des GEMESSENEN PFADES.
///
/// Misst die EL0-Region **dieses** Threads, während er lebt, und legt das Ergebnis in
/// `userstackmark::sonde_melden` ab. Gibt die gemessene Tiefe zurück (`0` = keine Region gebucht).
///
/// Warum das nicht erst im Bericht passiert: das Urteil der `ustack`-Zeile liest die Sonde als
/// Konjunkt, und `all_done()` wird **gepollt** — was im Bericht entsteht, kann den Bericht nicht
/// auslösen. Deshalb ruft der Hochlauf diese Funktion, sobald die Sonde gemeldet hat, dass sie
/// fertig berührt hat.
pub fn userstack_sonde_pruefen(tid: ThreadId) -> usize {
    let (b, l) = {
        let p = KSTACKS.lock();
        if tid.slot() >= p.ubase_of.len() {
            return 0;
        }
        (p.ubase_of[tid.slot()], p.ulen_of[tid.slot()])
    };
    if b == 0 || l == 0 {
        return 0;
    }
    // SAFETY: wie beim Fegen — identity-gemappte Region eines lebenden Threads, nur gelesen.
    let (benutzt, _) = unsafe {
        crate::userstackmark::messen(
            b as usize,
            l as usize,
            crate::userstackmark::Anlass::Lebend(tid.slot()),
        )
    };
    crate::userstackmark::sonde_melden(benutzt);
    benutzt
}

/// Virtuelle „freie Kstack-Kapazität" (jetzt RAM-begrenzt): `MAX_THREADS - live`. Für den
/// Reclaim-Test (spawnt, solange > 0) + Diagnose.
pub fn user_kstack_free_count() -> usize {
    caprock_sched::thread_capacity().saturating_sub(KSTACKS.lock().live)
}
/// Sticky: wurde jemals ein Syscall von EL0 (User-Thread) gesehen?
static EL0_SYSCALL_SEEN: AtomicBool = AtomicBool::new(false);
/// Zähler: wie oft hat der Kernel einen EL0-Fault abgefangen und den fehlerhaften
/// User-Thread isoliert (statt selbst anzuhalten)?
static EL0_FAULTS: AtomicUsize = AtomicUsize::new(0);
/// Zähler: wie oft faultete ein Thread, der in einer **isolierten VSpace** lief?
/// (Beleg, dass die Hardware-Adressraumtrennung Fremd-/unmapped-Zugriffe verhindert.)
static ISO_FAULTS: AtomicUsize = AtomicUsize::new(0);

/// **Z26/A3: wie oft ist ein Fault an eine Persönlichkeits-PD gegangen** (statt den Thread zu
/// beenden)? Die **Sprechprobe** der Fault-Weiche: ein Prüfer, der meldet „kein Gast ist am
/// nativen Fault-Pfad gestorben", muss belegen können, dass überhaupt umgeleitet wurde.
static HANDLER_FAULTS: AtomicUsize = AtomicUsize::new(0);

/// Zählerstand von [`HANDLER_FAULTS`].
pub fn handler_fault_count() -> usize {
    HANDLER_FAULTS.load(Ordering::Relaxed)
}

// --- FP-Zustand: EAGER auf x86_64, lazy auf aarch64 ---
//
// Pro Kern besitzt höchstens ein Thread die FP/SIMD-Register (der „FP-Owner").
// FP wird beim Kontextwechsel NICHT gesichert; erst ein FP-Trap (EC 0x07) eines
// Nicht-Owners aus EL0 löst Save (alter Owner) + Restore (neuer) aus. EL1 ist
// soft-float und berührt FP nie. Zähler `FP_SWITCHES` belegt, dass tatsächlich
// gewechselt wurde.
/// **Wer auf `core` zuletzt Ring-3-Kontext hatte -- SPERRFREI abfragbar** (fuer den `#DF`-Bericht).
///
/// Ein Double Fault darf **nichts sperren**: er kann genau den Kontext unterbrochen haben, der
/// `SCHEDS`/`KSTACKS` haelt. Der Handler bliebe stehen, und heraus kaeme **kein Output** -- also
/// exakt das Bild, gegen das die laute Meldung gebaut ist. Deshalb identifiziert der Bericht den
/// betroffenen **Stack** (aus `TSS.rsp0`) und nicht den Thread.
///
/// Diese Zeile schliesst die Luecke: `FP_OWNER` ist ein `AtomicU64` und traegt die volle
/// `ThreadId` des Kontexts, dessen FP-Zustand der Kern haelt -- ein sperrfreies Lesen. `None`
/// heisst „kein Ring-3-Kontext auf diesem Kern", nicht „unbekannt": beide Faelle gehoeren im
/// Bericht auseinander, sonst liest sich ein leerer Kern wie ein verlorener Thread.
pub fn ring3_kontext_von_kern(core: usize) -> Option<u64> {
    if core >= MAX_CORES {
        return None;
    }
    match FP_OWNER[core].load(Ordering::Relaxed) {
        FP_OWNER_NONE => None,
        t => Some(t),
    }
}

const FP_OWNER_NONE: u64 = u64::MAX;
/// Raw-`ThreadId` des FP-Owners je Kern (nur der jeweilige Kern schreibt seinen
/// Eintrag -> Atomics genügen, keine Sperre nötig).
#[allow(clippy::declare_interior_mutable_const)]
static FP_OWNER: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(FP_OWNER_NONE) }; MAX_CORES];
/// Unerwartete `#NM` **je Kern** (x86, unter eager immer 0). Nicht global: die wahrscheinlichste
/// kuenftige Regression ist ein einzelner AP, dessen `enable_sse` aus der Reihenfolge rutscht --
/// und der ginge in einer Summe unter. S. `fp_unerwartet`.
#[allow(clippy::declare_interior_mutable_const)]
static NM_TRAPS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
/// Per-Thread-Slot FP-Kontextpuffer. Zugriff ausschließlich unter dem SCHED-Lock
/// (fp_trap hält SCHED; spawn ebenfalls) -> für gleiche Slots serialisiert,
/// verschiedene Slots sind disjunkt. Lock-Ordnung: …->SCHED->FP_STATES (innerste).
static FP_STATES: SpinLock<Slab<FpState>> = SpinLock::new(Slab::empty());
/// Zähler abgeschlossener FP-Owner-Wechsel (Save+Restore), für den Test.
///
/// **Berichtigt 2026-08-14: hier stand „Lazy-FP-Owner-Wechsel".** Auf **x86_64 ist das Schema
/// EAGER** (s. `sync_fp_trap`: „Der Auslöser des Wechsels ist der WECHSEL, nicht der erste
/// Zugriff", `CR0.TS` bleibt aus, ein `#NM` ist per Definition ein Kernelfehler). Lazy ist der
/// **aarch64**-Pfad. Ein Papiertest hat aus genau diesem Kommentar auf den Mechanismus
/// geschlossen und ihn falsch berichtet — **einen Namen gelesen statt die Sache**, und der Name
/// war seit der Eager-Umstellung veraltet.
static FP_SWITCHES: AtomicUsize = AtomicUsize::new(0);
/// **Die Sprechprobe der FP-Sonde: wie oft wurde der Zustand DIESES Threads restauriert.**
///
/// `fp_switch_count()` belegt, dass *irgendwo* gewechselt wurde — nicht, dass die Sonde daran
/// beteiligt war. Zwei Sonden, die auf zwei Kernen liegen und einander nie verdrängen, ergäben
/// 77 Wechsel und **null** Aussage über ihr Muster: sie prüften dann Register, die zwischen ihren
/// Abgaben niemand angefasst hat. Dieselbe Form wie `rx_used` gegen „Daten sind angekommen".
///
/// Beobachtet werden zwei Threads, eingetragen von der Sonde selbst. `FP_OWNER_NONE` = unbesetzt
/// (roh `0` taugt nicht als Sentinel — Slot 0/Generation 0 ist eine gültige `ThreadId`).
#[cfg(feature = "selftest")]
static FP_WATCH_TID: [AtomicU64; 2] = [const { AtomicU64::new(FP_OWNER_NONE) }; 2];
#[cfg(feature = "selftest")]
static FP_WATCH_RESTORES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Summe der per `reap` an den Allokator zurückgegebenen Stack-Bytes (monoton).
static REAPED_BYTES: AtomicU64 = AtomicU64::new(0);

// --- Per-Prozess-VSpaces (Weg C, Hybrid) ---
//
// Vertrauenswürdige PDs laufen in der globalen SAS-Map (TTBR0 = global_root, ASID 0);
// **isolierte** PDs in einer eigenen VSpace (eigene Wurzel + ASID), die nur den
// Kernel (EL1-only) + ihre eigene User-Region (EL0) mappt. Der Kontextwechsel setzt
// TTBR0 passend zum einlaufenden Thread.
/// Gepackter `TTBR0`-Wert (asid<<48 | root) je globalem Thread-Slot; `0` = globale
/// SAS-Map. Beim Spawn einer isolierten PD gesetzt.
#[allow(clippy::declare_interior_mutable_const)]
static VSPACE_OF: AtomicTable<AtomicU64> = AtomicTable::empty();

/// Gepacktes TTBR0 eines Thread-Slots lesen (0 = globale SAS-Map).
fn vspace_of(slot: usize) -> u64 {
    VSPACE_OF.get(slot).map(|v| v.load(Ordering::Relaxed)).unwrap_or(0)
}
/// Gepacktes TTBR0 eines Thread-Slots setzen.
fn set_vspace_of(slot: usize, packed: u64) {
    if let Some(v) = VSPACE_OF.get(slot) {
        v.store(packed, Ordering::Relaxed);
    }
}
/// Aktuell aktive VSpace (gepacktes TTBR0) je Kern; `0` = noch unbekannt. Vermeidet
/// einen TTBR0-Write, wenn die VSpace gleich bleibt (der häufige All-Trusted-Fall).
#[allow(clippy::declare_interior_mutable_const)]
static CURRENT_VSPACE: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// **Per-Kern** Scheduler-Instanzen, jede hinter eigenem Lock. Der heiße
/// Timer-/IPI-Reschedule-Pfad sperrt nur `SCHEDS[core]` des eigenen Kerns -> echte
/// parallele Einplanung ohne globalen Lock. Kern-übergreifend (z. B. `wake_remote`)
/// sperrt der Kernel die **Ziel**instanz und schickt einen Reschedule-IPI.
#[allow(clippy::declare_interior_mutable_const)]
static SCHEDS: [SpinLock<Scheduler>; MAX_CORES] =
    [const { SpinLock::new(Scheduler::new()) }; MAX_CORES];

// --- Feinkörnige Ressourcen-Locks (ersetzen den einen RES-Lock) ---
//
// Sperrordnung (außen->innen): CAPS < {EPS[i], NTFNS[i], MEM} < SCHEDS[*] < FP_STATES.
// Der Reschedule-Pfad nimmt nur SCHEDS[core] (+ atomares FP_OWNER), wartet also nie
// auf einen anderen Lock. IPC auf verschiedenen Endpoints läuft parallel (je eigener
// EPS[i]-Lock). Die heiße Cap-Auflösung ist rein lesend und nimmt CAPS nur GETEILT
// (Read) -> Lookups auf verschiedenen Kernen laufen parallel; nur die seltenen
// Mutationen (install/copy/mint/move/delete/revoke/grant/PD-Ops) sperren CAPS exklusiv
// (Write). `read()` und `write()` liegen an derselben Ordnungsposition wie zuvor `lock()`.
/// Cap-Auflösung + -Verwaltung (CapSpace + PD-Tabelle) hinter einem Reader-Writer-Lock.
static CAPS: RwSpinLock<Caps> = RwSpinLock::new(Caps::new());
/// Physischer Allokator (Thread-Stacks etc.) — getrennt, blockiert IPC nicht.
static MEM: SpinLock<PhysAllocator> = SpinLock::new(PhysAllocator::new());
/// **Eine beim Boot einmalig befüllte Tabelle** (A-3.4 Teil 4).
///
/// Für die IPC-Tabellen taugt das sonst übliche `SpinLock<Slab<T>>` (so halten es `FP_STATES`
/// und `CAP_AUDIT`) **nicht**: ein Lock um die Tabelle würde jede IPC serialisieren und damit
/// genau die Eigenschaft aufheben, für die es die per-Endpoint-Locks gibt (ADR 0004:
/// „IPC auf verschiedenen Endpoints läuft parallel"). Auch ein `RwSpinLock` löst es nicht —
/// sein Read-Guard maskiert IRQs für seine ganze Lebensdauer, hier also über die gesamte IPC.
///
/// Die Tabelle braucht aber gar keinen Lock: sie wird **einmal** beim Boot befüllt und danach
/// nie wieder verändert. Verändert werden nur die *Elemente*, und die tragen ihren eigenen
/// Lock. Das ist dieselbe Bauform, die die HAL für ihre Seitentabellen benutzt
/// (`TableStore(UnsafeCell<PageTable>)`), nur typisiert.
///
/// **Der Vertrag, auf dem das steht** (die einzige `unsafe`-Stelle hier):
/// `attach` läuft genau einmal, auf dem Boot-Kern, **bevor** weitere Kerne starten und bevor
/// die erste IPC möglich ist. Danach gibt es nur noch `&Slab<T>` — geteilt, unveränderlich.
/// Wird der Aufruf vergessen, hat die Tabelle Länge 0: jeder Zugriff läuft in die
/// Schrankenprüfung und wird als `ERR_BADCAP` abgewiesen. Ein vergessener Aufruf fällt damit
/// als sauberer Fehler auf, nicht als Fehlzugriff.
struct BootSlab<T: 'static>(core::cell::UnsafeCell<Slab<T>>);

// SAFETY: Nach dem einmaligen `attach` (single-threaded, vor dem Start weiterer Kerne) gibt
// dieser Typ ausschließlich `&Slab<T>` heraus. Er verhält sich damit wie ein `&[T]`, den sich
// alle Kerne teilen -> `Sync` verlangt nur `T: Sync`, exakt wie bei `[T]`.
unsafe impl<T: Sync> Sync for BootSlab<T> {}

impl<T> BootSlab<T> {
    const fn new() -> Self {
        Self(core::cell::UnsafeCell::new(Slab::empty()))
    }

    /// Der Tabelle ihren Speicher geben.
    ///
    /// # Safety
    /// * Vertrag von [`Slab::attach`] (exklusiver, ausgerichteter, dauerhafter Speicher).
    /// * **Genau einmal**, auf dem Boot-Kern, bevor ein anderer Kern oder eine IPC laufen
    ///   kann — sonst existiert währenddessen ein `&mut` neben geteilten `&`.
    unsafe fn attach(&self, ptr: *mut T, len: usize, init: impl FnMut(usize) -> T) {
        // SAFETY: an den Aufrufer durchgereicht (s. Funktionsdoku): zu diesem Zeitpunkt gibt es
        // keinen zweiten Verweis auf die Tabelle, der `&mut` ist also exklusiv.
        unsafe { (*self.0.get()).attach(ptr, len, init) };
    }

    /// Die Tabelle geteilt lesen — nach dem Boot der einzige Zugang.
    fn get(&self) -> &Slab<T> {
        // SAFETY: nach dem Boot wird die Tabelle nicht mehr verändert (s. Typ-Doku); es kann
        // also kein `&mut` daneben existieren. Vor dem `attach` ist sie leer, aber gültig.
        unsafe { &*self.0.get() }
    }
}

/// **Per-Endpoint** Locks: IPC auf verschiedenen Endpoints ist nebenläufig.
///
/// Seit A-3.4 Teil 4 beim Boot dimensioniert (vorher `[SpinLock<Endpoint>; 32]` im `.bss`).
static EPS: BootSlab<SpinLock<Endpoint>> = BootSlab::new();
/// **Per-Notification** Locks. Wie [`EPS`] beim Boot dimensioniert.
static NTFNS: BootSlab<SpinLock<Notification>> = BootSlab::new();

/// Zugriff auf die Endpoint-Tabelle als Slice (Schranke = **reale** Kapazität).
fn eps() -> &'static [SpinLock<Endpoint>] {
    EPS.get().as_slice()
}

/// Zugriff auf die Notification-Tabelle als Slice.
fn ntfns() -> &'static [SpinLock<Notification>] {
    NTFNS.get().as_slice()
}

/// **Sammelflaeche fuer verwaiste Aufrufer** beim Thread-Tod (`purge_ipc_queues`, A-3.4 Teil 4).
///
/// Ein Eintrag je Endpoint — mehr Waisen kann ein einzelner sterbender Thread nicht erzeugen.
/// Frueher eine lokale Variable; bei 10 000 Endpoints waeren das 240 KiB Kernelstack im
/// Todespfad gewesen (s. `purge_ipc_queues`).
static IPC_ORPHANS: SpinLock<Slab<Option<ThreadId>>> = SpinLock::new(Slab::empty());

// --- Trap-Hooks ---

/// Eine Umplanung **eingeklammert abrechnen** (B-5.1): den abgehenden Thread belasten, den
/// ankommenden neu stempeln — beides unter derselben Sperre wie die Umplanung selbst.
///
/// **Ein** Zählerstand, nicht zwei. Zwei Lesungen wären die naheliegende Fassung und hinterliessen
/// ein Loch: die Zyklen der Umplanung selbst lägen zwischen den Stempeln und gehörten niemandem.
/// Die Summe aller Konten wäre dann systematisch kleiner als die verstrichene Zeit — also genau
/// der Fehler, gegen den B-5.1 antritt, nur kleiner. Mit einem Stempel ist die Zeitachse
/// **lückenlos** aufgeteilt, und das ist nachprüfbar: die Summe muss zur Uhr passen.
///
/// Der Preis ist benannt: die Kosten der Umplanung trägt der **ankommende** Thread. Das ist eine
/// Zuordnungsentscheidung, keine Messungenauigkeit — und die einzige, die eine lückenlose Achse
/// zulässt.
#[inline]
fn charged<R>(core: usize, sched: &mut Scheduler, f: impl FnOnce(&mut Scheduler) -> R) -> R {
    let now = hal::timer::cycles();
    sched.charge_current(core, now);
    let r = f(sched);
    sched.stamp_current(core, now);
    r
}

/// **Wie viele Fristen ein einzelner Tick abarbeitet.**
///
/// Eine benannte Kapazitaet, kein Zufall: der Puffer liegt auf dem **IRQ-Stack**, und ein Feld je
/// Thread waere dort 10 000 Eintraege. Laufen mehr Fristen im selben Tick ab, kommen die uebrigen
/// im naechsten -- `naechste_frist` bleibt dann auf dem kleinsten noch offenen Wert stehen, also
/// **sofort wieder faellig**. Verloren geht keine; verzoegert werden sie um Ticks.
///
/// *Wer eine Kapazitaet einfuehrt, muss den Ueberlauf benennen* -- hier ist er Verzoegerung, nicht
/// Verlust, und das ist der Grund, warum er ohne eigenen Fehlercode auskommt.
///
/// **Und dieser Satz war bis 2026-08-28 eine Behauptung.** Der Deckel stand in `fristen_faellig`
/// hinter der Wirkung (`… && n < out.len()` als drittes Konjunkt des Einreihens): der Grund wurde
/// entfernt, der Thread aber weder eingereiht noch berichtet -- der siebzehnte Wartende eines
/// Ticks war **verloren**, nicht verzoegert. Der Deckel greift jetzt VOR dem Entfernen. Eine
/// benannte Kapazitaet ist erst dann eine, wenn ihr Ueberlauf gefahren oder wenigstens gelesen
/// worden ist.
const FRISTEN_JE_TICK: usize = 16;

fn reschedule(frame: *mut TrapFrame) -> *mut TrapFrame {
    // **Der zweite Summand der Stackrechnung (C4), gemessen an der TIEFSTEN Stelle des
    // IRQ-Pfads** -- hier und nicht im Einsprung der HAL: dort kam die erste Fassung auf 24 Byte,
    // also auf den Verbrauch BIS dorthin. Die Tiefe entsteht darunter, im Reschedule.
    // `&hier` liegt im Rahmen dieser Funktion; die Differenz zum abgelegten Frame ist Frame +
    // Handlerkette bis hierher.
    let hier: u64 = 0;
    hal::exception::irq_tiefe_melden(frame as u64, core::ptr::addr_of!(hier) as u64);
    let core = hal::cpu::core_id();
    // Deferred-IRQ-Zustellung (ext-22, P5): pending Geräte-IRQs als Notification signalisieren
    // — VOR dem SCHEDS-Lock (signal nimmt NTFNS<SCHEDS; kein verschachtelter SCHEDS). Fast-
    // Check -> Null-Overhead auf dem heißen Timer-Pfad, wenn nichts pending ist.
    drain_pending_irqs();
    // Heißer Pfad: nur der Scheduler-Lock DIESES Kerns (parallel zu anderen Kernen).
    // Echter Zeitscheiben-Tick -> MCS-Budget des laufenden Threads belasten.
    // **Wer aus einer Frist geweckt wird, muss beim OBJEKT abgemeldet werden** -- gesammelt hier,
    // ausgefuehrt unter der SCHEDS-Sperre NICHT (Sperrordnung `EPS`/`NTFNS` < `SCHEDS`).
    let mut frist_abmelden: Option<([ThreadId; FRISTEN_JE_TICK], usize)> = None;
    let next = {
        let mut sched = SCHEDS[core].lock();
        let next = charged(core, &mut sched, |s| s.on_tick(core, frame as usize, true));
        // --- Stufe A / A2: faellige Fristen ---------------------------------------------------
        //
        // **Hier und nicht im Scheduler**, weil das Ergebnis `ERR_TIMEOUT` eine ABI-Groesse ist
        // und `caprock-sched` die ABI nicht kennt (und nicht kennen soll). Der Scheduler weckt,
        // der Kernel sagt WARUM.
        //
        // Im Normalfall kostet das **einen Vergleich**: `fristen_faellig` kehrt sofort um, solange
        // die frueheste Frist in der Zukunft liegt. Das ist die D10-Bedingung -- ein Zaehler
        // („gibt es Fristen?") machte jeden Tick zum Tabellendurchlauf, sobald ein einziger
        // Treiber wartet, und genau dafuer gibt es A2.
        let mut geweckt = [ThreadId::from_raw(0); FRISTEN_JE_TICK];
        let n = sched.fristen_faellig(&mut geweckt);
        for t in geweckt.iter().take(n) {
            if let Some(f) = sched.frame_of(*t) {
                hal::exception::frame_set_reg(
                    f,
                    caprock_abi::reg::SYSNO_RESULT,
                    caprock_abi::result::ERR_TIMEOUT,
                );
            }
        }
        if n > 0 {
            frist_abmelden = Some((geweckt, n));
        }
        sync_thread_state(core, &sched);
        next
    }; // SCHEDS freigegeben — der Lastausgleich sperrt selbst (zwei Kerne).
    // **Die zweite Haelfte der Frist, und sie hat gefehlt.**
    //
    // Ein Thread, den die Frist aus einem IPC-Warten holt, steht danach **immer noch** im
    // Wartefeld seines Objekts. Gemessen: der zweite `WAIT` desselben Threads bekam `ERR_EP_FULL`
    // (Kapazitaet 1, D11) -- die Notification hielt ihn fuer den Wartenden, obwohl er laengst
    // weitergelaufen war. Ein spaeteres Signal haette einen Thread geweckt, der nicht mehr wartet.
    //
    // Dieselbe Familie wie D11, nur andersherum: dort stand ein Wartender in KEINER Struktur,
    // hier steht ein Nicht-mehr-Wartender in EINER. Beide Male meldet jeder Pruefer Ordnung.
    //
    // **Ausserhalb der SCHEDS-Sperre**, weil `purge_thread` die Objektsperren nimmt und die
    // Ordnung `EPS`/`NTFNS` < `SCHEDS` lautet (`docs/invariants.md` §1).
    if let Some((tids, n)) = frist_abmelden {
        for t in tids.iter().take(n) {
            for ep in eps().iter() {
                let mut e = ep.lock();
                if e.is_used() {
                    e.purge_thread(*t);
                }
            }
            for nt in ntfns().iter() {
                let mut x = nt.lock();
                if x.is_used() {
                    x.purge_thread(*t);
                }
            }
        }
    }
    // Periodischer Lastausgleich (ext-30). Sicher an dieser Stelle: `balance_once` verschiebt
    // nie den **laufenden** Thread, der gerade gewählte `next`-Frame bleibt also gültig.
    //
    // KEIN Tick-Pfad fuer NOHZ (B-5.2, Stand 2026-09-10): ein armierter Tick-Pfad (Disarmed
    // ohne garantiertes Rearm auf laufendem Thread) liess den Timer sterben -- der Gast stand
    // danach still, ohne Watchdog, ohne Zeile (E2E-Befund). `nohz_stand`/`nohz_idle` bleiben
    // als gepruefte, aber unverdrahtete Bausteine (s. dort): erst IPI-Kick-Sicherheit, dann
    // Verdrahtung. Bis dahin tickt jeder Kern wie bisher.
    if BALANCING.load(Ordering::Relaxed) {
        let n = BALANCE_TICK[core].fetch_add(1, Ordering::Relaxed) + 1;
        if n % BALANCE_INTERVAL_TICKS == 0 {
            balance_once();
        }
    }
    next as *mut TrapFrame
}

/// Callback für den Dispatch: eine Capability löschen (Finalisierung inkl. Speicher-
/// rückgabe und Abbruch finalisierter Reply-Calls). Wird **ohne** gehaltene Dispatch-Locks
/// gerufen (der Dispatch gibt CAPS/EPS vorher frei); `cap_delete` sperrt selbst CAPS+MEM
/// in der richtigen Ordnung. Ein Fehlschlag (Cap bereits weg) ist unkritisch.
fn dispatch_delete_cap(cap: caprock_cap::CapPtr) -> Result<(), u64> {
    // **K1a: die Kopplung, die `SYS_SPAWN` erzeugt** (2026-08-17).
    //
    // Ein Stack, der aus einer Cap des Aufrufers kommt, koppelt CapSpace und Scheduler. Von den
    // zwei ehrlichen Ausgaengen faehrt dieser Kernel den zweiten: **die Loeschung wird
    // abgewiesen, solange der Thread lebt.** Nicht weil es netter ist, sondern weil der Verweis
    // damit ZAEHLBAR ist -- „Loeschung toetet den Thread mit" waere eine Gruppenoperation ueber
    // `CAPS` und `SCHEDS[core]`, also die V4-Klasse mit zwei Sperren und einer Ordnung.
    //
    // Die Pruefung steht **vor** `cap_delete`: die Finalisierung gibt die Region an den Allokator
    // zurueck, und ein Stapel, unter dem das geschieht, waere ein Use-after-free mit dem
    // Rueckgabezeiger als Nutzlast.
    if stack_cap_in_use(cap) {
        return Err(caprock_abi::result::ERR_INUSE);
    }
    cap_delete(cap)
        .map(|_| ())
        .map_err(|_| caprock_abi::result::ERR_HASCHILDREN)
}

/// **Die acht Debug-Syscalls, an einer Stelle** (Z6b + Debugger-v2).
///
/// Der Rueckruf des Dispatch. Er loest die Cap **selbst** auf und haelt `CAPS` ueber den ganzen
/// Vorgang -- s. die Doku an [`debug_stop`] fuer den Grund. 21-25 und 33 sind voll verdrahtet;
/// 34-35 sind autorisiert geprueft und weisen den fehlenden CPU-Pfad (`hal::debug`) benannt ab --
/// s. [`debug_single_step`] und [`debug_hwbreak`]. Nie ein Stub-Erfolg.
fn dispatch_debug(
    nr: u64,
    pd: usize,
    slot: usize,
    a1: u64,
    a2: u64,
    a3: u64,
) -> Result<u64, u64> {
    match nr {
        caprock_abi::sys::DEBUG_ATTACH => debug_attach(pd, slot, a1),
        caprock_abi::sys::DEBUG_STOP => debug_stop(pd, slot, a1),
        caprock_abi::sys::DEBUG_CONTINUE => debug_continue(pd, slot, a1),
        caprock_abi::sys::DEBUG_READ_MEM => debug_read_mem(pd, slot, a1, a2, a3),
        caprock_abi::sys::DEBUG_WRITE_REGS => debug_write_reg(pd, slot, a1, a2, a3),
        caprock_abi::sys::DEBUG_WRITE_MEM => debug_write_mem(pd, slot, a1, a2, a3),
        caprock_abi::sys::DEBUG_SINGLE_STEP => debug_single_step(pd, slot, a1),
        caprock_abi::sys::DEBUG_HWBREAK => debug_hwbreak(pd, slot, a1, a2, a3),
        _ => Err(caprock_abi::result::ERR_BADSYS),
    }
}

fn syscall(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = hal::cpu::core_id();
    if hal::exception::frame_from_el0(frame as usize) {
        EL0_SYSCALL_SEEN.store(true, Ordering::Relaxed);
    }
    // Der Dispatch besorgt das Locking selbst (feinkörnig: CAPS für die kurze
    // Cap-Auflösung, dann per-Objekt-Lock; SchedOps je Op genau eine SCHEDS-Instanz).
    // Sperrordnung CAPS < EPS[i]/NTFNS[i] < SCHEDS -> deadlockfrei. IRQs im Trap
    // maskiert -> kein Preempt beim Lock-Halten.
    //
    // FORK/EXEC (31/32) laufen VOR dem Microkit-Dispatch (`forkexec_syscall`): die Crate ist
    // nur gelesen, ihre 31/32-Arme bleiben fail-closed `ERR_BADSYS` („Antrag ok, Pfad fehlt").
    // Der Hook haelt nichts — die Rueckrufe nehmen CAPS/MEM/SCHEDS selbst.
    let nr = hal::exception::frame_reg(frame as usize, caprock_abi::reg::SYSNO_RESULT);
    if nr == caprock_abi::sys::FORK_SNAPSHOT || nr == caprock_abi::sys::EXEC_REPLACE {
        let next = forkexec_syscall(frame as usize, core, nr);
        {
            let sched = SCHEDS[core].lock();
            sync_thread_state(core, &sched);
        }
        return next as *mut TrapFrame;
    }
    let mut ops = KernelSched;
    let next = caprock_microkit::dispatch(
        frame as usize,
        core,
        &mut ops,
        &CAPS,
        eps(),
        ntfns(),
        // ext-26: SYS_LOAD-Callback (cap-gegatet im Dispatch); A-5.1: mit Aufrufer-PD.
        // **Seit C8 eine Uebergabe an den Verifiziererthread**, kein synchrones Laden mehr --
        // die Krypto gehoert nicht auf den 16-KiB-Stack des Aufrufers.
        sys_load_uebergeben,
        dispatch_delete_cap,          // beim Grant verdrängte Cap freigeben (kein Slot-Leck)
        dispatch_spawn,               // K1a: zweiter Thread in derselben PD, Stack aus einer Cap
        dispatch_debug,               // Z6b: die fuenf Debug-Syscalls
        dispatch_bind_irq,            // B3: SYS_BIND_IRQ, mit Cap-Riegel
        // LXPD-Laufzeit (`SYS_LOAD_IMAGE = 36`): wie `sys_load_uebergeben`, aber das Bild kommt
        // aus Aufrufer-RAM statt aus dem Boot-Archiv — Geometrie hat der Dispatch bereits gegen
        // die Aufrufer-Memory-Cap geprüft, der Verifizierer kopiert EINMAL nach Staging.
        sys_load_image_uebergeben,
    );
    // Der Syscall kann den laufenden Thread gewechselt haben (block/exit) -> FP-Trap
    // + VSpace passend zum neuen aktuellen Thread setzen.
    {
        let sched = SCHEDS[core].lock();
        sync_thread_state(core, &sched);
    }
    next as *mut TrapFrame
}

/// Facade über alle per-Kern-Scheduler-Instanzen für den IPC-/Dispatch-Pfad. Jede
/// Operation sperrt **genau eine** `SCHEDS`-Instanz (die des angegebenen Kerns bzw.
/// des Ziel-Threads) und gibt sie sofort wieder frei — nie zwei gleichzeitig. Beim
/// kern-übergreifenden Wecken (`unblock` auf einen Thread eines anderen Kerns)
/// schickt sie zusätzlich einen Reschedule-IPI an den Zielkern.
struct KernelSched;

/// Wie oft ein kernübergreifender Zugriff wiederholt wird, wenn der Thread genau zwischen
/// „Besitzer nachschlagen" und „Kern sperren" **migriert** ist. Migration ist selten und
/// endlich (der Balancer verschiebt einen Thread je Tick), zwei Wiederholungen sind schon
/// großzügig; die Schranke verhindert, dass ein pathologischer Fall hier festhängt.
const MIGRATION_RETRIES: usize = 4;

/// **Migrationssicherer Zugriff auf den besitzenden Kern eines Threads.**
///
/// Seit ext-30 ist der Kern eines Threads nicht mehr aus seiner `ThreadId` ableitbar,
/// sondern steht im Thread-Directory. Zwischen dem (lock-freien) Nachschlagen und dem
/// Sperren kann der Thread migrieren — dann gehört er dem gesperrten Kern nicht mehr und
/// `f` schlägt fehl. Hier wird genau dieser Fall erkannt (Besitzer hat gewechselt) und mit
/// dem **neuen** Besitzer wiederholt. Ein Fehlschlag ohne Besitzerwechsel ist ein echter
/// Fehlschlag (Thread tot / Operation unzulässig) und wird durchgereicht.
///
/// Gibt `(Ergebnis, Kern)` zurück — der Kern wird für den Reschedule-IPI gebraucht, der
/// **nach** dem Freigeben des Locks geschickt wird.
fn with_owner<R>(
    tid: ThreadId,
    mut f: impl FnMut(&mut Scheduler, usize) -> Option<R>,
) -> Option<(R, usize)> {
    for _ in 0..MIGRATION_RETRIES {
        let c = caprock_sched::owner_core(tid)?;
        if c >= num_cores() {
            return None;
        }
        let attempt = {
            let mut sched = SCHEDS[c].lock();
            f(&mut sched, c)
        }; // Lock hier freigegeben
        if let Some(r) = attempt {
            return Some((r, c));
        }
        // Fehlgeschlagen: nur wiederholen, wenn der Thread inzwischen woanders lebt.
        match caprock_sched::owner_core(tid) {
            Some(c2) if c2 != c => continue,
            _ => return None,
        }
    }
    None
}

/// **Das Ergebnis aus dem Sidecar in den Frame des Gastes — und ERST DANN der Wecker** (Z26/A3).
///
/// Läuft unter der Sperre des besitzenden Kerns (`with_owner`), also in demselben kritischen
/// Abschnitt wie das Entfernen des Grundes. **Die Reihenfolge ist die Zusicherung:** wer zuerst
/// weckt, gibt den Gast frei, bevor sein Frame steht — auf einem anderen Kern liefe er dann mit
/// halbem Syscall los. Genau die Wirkung, die Z26/Nachtrag 3 vorhersagt, und genau der Grund, aus
/// dem `HANDLER` überhaupt ein eigener Grund ist.
///
/// Ein Thread **ohne** Bindung (der Kernel-Prüfpfad in `handlermess.rs`) überspringt das Kopieren
/// und wird nur geweckt: „kein Fenster" ist hier eine Antwort und kein Fehlschlag.
fn handler_reply_mit_frame(s: &mut Scheduler, tid: ThreadId) -> Option<()> {
    if let (Some(b), Some(frame)) = (s.handler_of(tid), s.frame_of(tid)) {
        if b.sidecar != 0 {
            // **Ein fremder Kopf wird BENANNT abgewiesen, nicht ausgelegt** (A-4.3-Regel). Der
            // Gast läuft weiter -- er muss, sonst hinge er an einem Formatstreit --, aber mit
            // einem Code, der sagt, was los war. `KeinFrame` bekommt ausdrücklich KEINEN Code:
            // dort ist der Frame unverändert, und der Gast sieht, was der IPC-Transport
            // hinterlassen hat. Genau daran ist die Gegenprobe ablesbar.
            if let crate::sidecarkopie::Uebernahme::FremderKopf(_) =
                crate::sidecarkopie::uebernehmen(frame, b.sidecar, b.slot)
            {
                hal::exception::frame_set_reg(
                    frame,
                    caprock_abi::reg::SYSNO_RESULT,
                    caprock_abi::result::ERR_HANDLER_ABI,
                );
            }
        }
    }
    s.handler_reply(tid).then_some(())
}

/// Besitzender Kern eines Threads (lock-frei; kann beim Sperren bereits veraltet sein).
pub fn owner_core_of(tid: ThreadId) -> Option<usize> {
    caprock_sched::owner_core(tid)
}

/// Einen Reschedule-IPI an `core` schicken, falls es nicht der eigene ist.
fn kick(core: usize) {
    if core != hal::cpu::core_id() {
        hal::intc::send_sgi(core, hal::intc::IPI_RESCHED_INTID);
    }
}

impl SchedOps for KernelSched {
    fn current_id(&mut self, core: usize) -> ThreadId {
        SCHEDS[core].lock().current_id(core)
    }
    fn frist_setzen(&mut self, core: usize, ticks: u64, grund: u16) {
        SCHEDS[core].lock().frist_setzen(core, ticks, grund);
    }
    fn clock_hz(&mut self) -> u64 {
        hal::timer::cycles_per_sec()
    }
    fn clock_now(&mut self) -> u64 {
        hal::timer::cycles()
    }
    fn set_tls(&mut self, tid: ThreadId, va: usize) -> bool {
        // **Ueber `with_owner`**, nicht ueber den Kern des Aufrufers: der Zielthread kann auf einer
        // anderen Scheduler-Instanz liegen. Heute ruft nur `SETTLS` das, und dort ist das Ziel der
        // Aufrufer selbst -- aber eine Fassade, die das voraussetzt, ist beim ersten fremden
        // Aufrufer still falsch.
        //
        // Die **Adressschranke** ist im Dispatch gefallen (`USER_VA_TOP`); hier steht sie nicht
        // noch einmal, und der Grund dafuer steht bei `sync_tls`.
        with_owner(tid, |s, _| s.set_tls(tid, va).then_some(())).is_some()
    }
    fn frame_of(&mut self, tid: ThreadId) -> Option<usize> {
        with_owner(tid, |s, _| s.frame_of(tid)).map(|(f, _)| f)
    }
    fn block_current(&mut self, core: usize, frame: usize) -> usize {
        // Genau der Pfad, den die Tick-Rechnung **nicht** sieht: wer kurz vor dem Tick blockiert,
        // hat gerechnet und zahlte bisher nichts (B-5.1).
        charged(core, &mut SCHEDS[core].lock(), |s| s.block_current(core, frame))
    }
    fn switch_to(&mut self, core: usize, frame: usize, target: ThreadId) -> usize {
        charged(core, &mut SCHEDS[core].lock(), |s| s.switch_to(core, frame, target))
    }
    fn unblock(&mut self, tid: ThreadId) {
        if let Some((_, c)) = with_owner(tid, |s, _| s.unblock(tid).then_some(())) {
            kick(c);
        }
    }
    fn wartet_auf_ipc(&mut self, tid: ThreadId) -> bool {
        // A2-Rest, zweite Partei: reine Lesefrage an den besitzenden Scheduler (Muster
        // `frame_of` darueber) — kein Lock, kein Kick, kein Eingriff.
        with_owner(tid, |s, _| Some(s.wartet_auf_ipc(tid))).map(|(w, _)| w).unwrap_or(false)
    }
    fn park_current(&mut self, core: usize, frame: usize) -> Option<usize> {
        SCHEDS[core].lock().park_current(core, frame)
    }
    fn unpark(&mut self, tid: ThreadId) {
        // Dieselbe Migrationsschleife wie `unblock`: zwischen „Besitzer nachschlagen" und
        // „Kern sperren" kann der Thread den Kern wechseln. `unpark` gibt `false` genau dann,
        // wenn er auf DIESEM Kern nicht aufloesbar war -- `with_owner` wiederholt dann.
        if let Some((_, c)) = with_owner(tid, |s, _| s.unpark(tid).then_some(())) {
            kick(c);
        }
    }
    fn pause(&mut self, tid: ThreadId) {
        // Läuft das Ziel gerade auf einem anderen Kern, per Reschedule-IPI deplanen.
        if let Some((_, c)) = with_owner(tid, |s, _| s.pause(tid).then_some(())) {
            kick(c);
        }
    }
    // --- Z26/A3: umgeleitete Syscalls -------------------------------------------------------
    fn current_handler(&mut self, core: usize) -> Option<caprock_sched::redirect::Bindung> {
        // **Der heisse Pfad**: einmal je Syscall, eine Sperrung, ein `Option`-Lesen am laufenden
        // TCB. Kein `current_id` + `handler_of` (das wären zwei Sperrungen) und kein Cap-Lookup.
        SCHEDS[core].lock().current_handler(core)
    }
    fn handler_of(&mut self, tid: ThreadId) -> Option<caprock_sched::redirect::Bindung> {
        // `with_owner` wiederholt nur bei `None` -- deshalb wird das INNERE `Option` in ein
        // `Some(..)` gewickelt: „kein Handler gebunden" ist eine Antwort und kein Fehlschlag.
        // Ohne diese Hülle liefe die Migrationsschleife bei jedem ungebundenen Thread vier Runden
        // leer und meldete am Ende dasselbe -- teuer und irreführend zugleich.
        with_owner(tid, |s, _| Some(s.handler_of(tid))).and_then(|(b, _)| b)
    }
    fn set_handler(&mut self, tid: ThreadId, b: Option<caprock_sched::redirect::Bindung>) -> bool {
        with_owner(tid, |s, _| s.set_handler(tid, b).then_some(())).is_some()
    }
    fn block_for_handler(&mut self, core: usize, frame: usize) -> usize {
        // Abgerechnet wie jede andere Blockade (B-5.1): wer hier blockiert, hat bis eben
        // gerechnet, und die Zyklen gehören ihm.
        charged(core, &mut SCHEDS[core].lock(), |s| {
            s.block_for_handler(core, frame)
        })
    }
    fn mark_handler_wait(&mut self, tid: ThreadId) {
        // **Kein `kick`.** Ein Grund hinzuzufügen macht niemanden lauffähig — ein IPI wäre hier
        // reine Last. Geweckt wird bei `handler_reply`.
        let _ = with_owner(tid, |s, _| s.mark_handler_wait(tid).then_some(()));
    }
    fn sidecar_ablegen(
        &mut self,
        frame: usize,
        sidecar: u64,
        slot: u16,
        anlass: u64,
        code: u64,
    ) -> bool {
        crate::sidecarkopie::ablegen(frame, sidecar, slot, anlass, code)
    }
    fn handler_reply(&mut self, tid: ThreadId) {
        // Dieselbe Migrationsschleife wie `unblock`/`unpark`, und aus demselben Grund.
        if let Some((_, c)) = with_owner(tid, |s, _| handler_reply_mit_frame(s, tid)) {
            kick(c);
        }
    }
    fn resume(&mut self, tid: ThreadId) {
        // Z24: `RESUME` hebt **die Pause** auf, nicht „die Blockade". Vorher lief das ueber
        // `unblock`, das seit dem Umbau den IPC-Grund entfernt -- ein Wecker ohne Namen weckt
        // sonst eine fremde Entscheidung mit weg.
        if let Some((_, c)) = with_owner(tid, |s, _| s.resume(tid).then_some(())) {
            kick(c);
        }
    }
    fn stop(&mut self, tid: ThreadId) -> bool {
        // STOP = vollständiger Teardown: cross-core kill + (falls isoliert) VSpace/ASID
        // freigeben, damit ein gestopptes Backend/UserLand keine Ressourcen leakt.
        let asid = (vspace_of(tid.slot()) >> 48) as u16;
        let ok = kill_remote(tid);
        if ok && asid != 0 {
            vspace_teardown(asid);
            set_vspace_of(tid.slot(), 0);
        }
        ok
    }
    fn on_tick(&mut self, core: usize, frame: usize) -> usize {
        // YIELD ist freiwillig -> verbraucht kein MCS-**Budget** (tick = false). Gerechnet hat der
        // Thread trotzdem, und B-5.1 misst den Verbrauch, nicht das Budget: die beiden Zahlen
        // dürfen auseinanderlaufen, sonst misst man wieder nur die Durchsetzung.
        charged(core, &mut SCHEDS[core].lock(), |s| s.on_tick(core, frame, false))
    }
    fn exit_current(&mut self, core: usize, frame: usize) -> usize {
        // Tid des sich beendenden Threads vor dem Wechsel merken; IRQs sind im Trap
        // maskiert -> der aktuelle Thread ist über die kurzen Sperren stabil.
        let tid = SCHEDS[core].lock().current_id(core);
        // Auch der sterbende Thread wird belastet. Sein Konto liest niemand mehr — aber die
        // kernweite Summe bliebe sonst hinter der Uhr zurück, und genau diese Summe ist die
        // einzige Probe darauf, ob die Achse lückenlos ist.
        let next = charged(core, &mut SCHEDS[core].lock(), |s| s.exit_current(core, frame));
        purge_ipc_queues(tid); // eager: aus allen IPC-Queues entfernen (keine SCHEDS gehalten)
        reclaim_user_kstack(tid, "exit_current"); // EL0-Kernel-Stack-Pool-Slot zurückgeben (KSTACKS allein)
        next
    }
    fn kill(&mut self, tid: ThreadId, core: usize) -> bool {
        let ok = SCHEDS[core].lock().kill(tid, core);
        if ok {
            purge_ipc_queues(tid); // eager: tote IPC-Queue-Einträge vermeiden
            reclaim_user_kstack(tid, "SchedOps::kill"); // getöteter EL0-Thread: Pool-Slot zurück
        }
        ok
    }
    fn end_donation(&mut self, core: usize) {
        SCHEDS[core].lock().end_donation(core);
    }
    fn map_frame(&mut self, caller: ThreadId, base: u64, len: u64, perm_code: u8) -> bool {
        // Nur isolierte PDs (ASID != 0). Granularität nach Frame-Größe (2-MiB-Block
        // oder 4-KiB-Seiten), Recht nach `perm_code` (0=Ro, 1=Rw, 2=Rx).
        let asid = (vspace_of(caller.slot()) >> 48) as u16;
        if asid == 0 || len == 0 || len % 4096 != 0 {
            return false;
        }
        let perm = match perm_code {
            2 => hal::mmu::UserPerm::Rx,
            1 => hal::mmu::UserPerm::Rw,
            _ => hal::mmu::UserPerm::Ro,
        };
        vspace_map(
            asid,
            &|pa| crate::addr::Va::for_syscall_map(SyscallMapWitness(()), pa),
            crate::addr::Pa::new(base),
            len,
            perm,
        )
    }
    fn unmap_frame(&mut self, caller: ThreadId, base: u64, len: u64) -> bool {
        let asid = (vspace_of(caller.slot()) >> 48) as u16;
        if asid == 0 || len == 0 || len % 4096 != 0 {
            return false;
        }
        vspace_unmap(
            asid,
            &|pa| crate::addr::Va::for_syscall_map(SyscallMapWitness(()), pa),
            crate::addr::Pa::new(base),
            len,
        )
    }
    /// A-5.1: ein Geräte-Fenster in die VSpace des Aufrufers, mit den Attributen des **Objekts**
    /// (Device vs. DMA-RAM), nicht denen der Rechte. Gibt die Gerätesicht (IOVA) zurück, `0` für
    /// MMIO.
    fn map_window(
        &mut self,
        caller: ThreadId,
        base: u64,
        len: u64,
        ro: bool,
        dma: Option<bool>,
    ) -> Option<u64> {
        if len == 0 || len % 4096 != 0 {
            return None;
        }
        let kind = match dma {
            None => MappingKind::Device { ro },
            Some(coherent) => MappingKind::Dma { coherent },
        };
        if !map_region_into_thread(caller, base, len, kind) {
            return None;
        }
        // Die IOVA ist **nicht** die PA und wird auch nicht daraus gerechnet: sie stammt aus dem
        // Fenster des Übersetzungskontexts, das der Kernel bei der Zuteilung gewählt hat. Wer sie
        // hier aus `base` ableitete, hätte die beiden Achsen wieder zusammengelegt — genau der
        // Fehler, gegen den `Pa`/`Iova` getrennte Typen sind.
        Some(match dma {
            Some(_) => assigned_iova(base).unwrap_or(0),
            None => 0,
        })
    }
    fn unmap_window(&mut self, caller: ThreadId, base: u64, len: u64) -> bool {
        let asid = (vspace_of(caller.slot()) >> 48) as u16;
        if asid == 0 || len == 0 || len % 4096 != 0 {
            return false;
        }
        // **Der Grund haengt an der ART des Fensters, nicht am Aufrufer.** MMIO und DMA in
        // einen Eintrag zu falten war der Fehler der ersten Fassung: bei MMIO ist die Identitaet
        // die Zusicherung (der Treiber rechnet mit BAR-Adressen aus der Enumeration), bei DMA ist
        // sie nur die CPU-seitige Haelfte -- das Geraet sieht eine IOVA, und die ist eine andere
        // Achse. `unmap_window` weiss hier nicht mehr, welche Art es war; die Abbildung ist in
        // beiden Faellen dieselbe, der Grund also der schwaechere der beiden.
        vspace_unmap(
            asid,
            &|pa| crate::addr::Va::for_mmio_window(MmioWindowWitness(()), pa),
            crate::addr::Pa::new(base),
            len,
        )
    }
}

/// `CPACR_EL1.FPEN` für den **gerade aktuellen** Thread auf `core` setzen: FP an
/// EL0 trappen, falls dieser Thread NICHT der FP-Owner des Kerns ist (so trappt
/// sein erster FP-Zugriff und löst den Lazy-Owner-Wechsel aus); sonst FP freigeben.
/// Am Ende jedes Hooks aufzurufen, der den laufenden Thread gewechselt haben kann.
/// **Allen Zustand spiegeln, der JE THREAD gilt und in einem Register des Kerns steht.**
///
/// Es gab bis heute **vier** Stellen, an denen `sync_fp_trap` und `sync_vspace` als Paar
/// nebeneinanderstanden — Syscall, Reschedule, Fault, Tick. Vier parallele Listen, gepflegt von
/// Hand, und `sync_tls` landete bei seiner Einfuehrung prompt an **einer** davon: die Kinder der
/// TLS-Sonde setzten ihren Zeiger, lasen ihn korrekt zurueck und verloren ihn beim ersten `YIELD`.
///
/// Das ist dieselbe Form wie *wer einen neuen Zustand einfuehrt, muss jede Stelle mitnehmen, die
/// ueber Zustaende URTEILT* (D0-Regression, Audit-Code 7) — hier: jede Stelle, die ihn
/// **spiegelt**. Die Behebung ist deshalb nicht die vierte Zeile, sondern **eine** Funktion: wer
/// ein fuenftes Stueck Thread-Zustand einfuehrt, kann es nicht mehr an drei von vier Stellen
/// vergessen.
fn sync_thread_state(core: usize, sched: &Scheduler) {
    sync_fp_trap(core, sched);
    sync_vspace(core, sched);
    sync_tls(core, sched);
}

fn sync_fp_trap(core: usize, sched: &Scheduler) {
    let cur_id = sched.current_id(core);
    let cur = cur_id.to_raw();
    let owner = FP_OWNER[core].load(Ordering::Relaxed);

    // ------------------------------------------------------------------------------------------
    // x86_64: EAGER. Der Auslöser des Wechsels ist der WECHSEL, nicht der erste Zugriff.
    // ------------------------------------------------------------------------------------------
    //
    // **Warum, und es ist keine Leistungsfrage:** Lazy-FP über eine PD-Grenze ist auf x86
    // CVE-2018-3665 (LazyFP). Mit gesetztem `CR0.TS` wird der `FXRSTOR` aufgeschoben, und
    // spekulative Ausführung kann die Register des **vorigen** Besitzers lesen, bevor das `#NM`
    // zugestellt ist. Für ein Cap-System, dessen Verkaufsargument Isolation ist, wäre das ein
    // Widerspruch im Kern des Anspruchs — und seit SSE für Userland scharf ist, kann in diesen
    // Registern Schlüsselmaterial liegen.
    //
    // **Die Trennung bleibt trotzdem billig:** wechselt der laufende Thread nicht (der häufige
    // Fall — jeder Syscall ohne Umplanung), passiert hier gar nichts. Gespart wird also alles
    // ausser dem, was ein Wechsel wirklich kostet.
    //
    // Die Trap-Konfiguration wird hier NICHT mehr angefasst: `CR0.TS` bleibt dauerhaft aus (einmal
    // je Kern in `enable_sse`). Ein `#NM` ist ab jetzt per Definition ein Kernelfehler und wird
    // als solcher gemeldet, s. `fp_unerwartet`.
    #[cfg(target_arch = "x86_64")]
    {
        if owner != cur {
            let mut states = FP_STATES.lock();
            if owner != FP_OWNER_NONE {
                let prev_slot = ThreadId::from_raw(owner).slot();
                hal::fp::save(&mut states[prev_slot]);
                FP_SWITCHES.fetch_add(1, Ordering::Relaxed);
            }
            hal::fp::restore(&states[cur_id.slot()]);
            drop(states);
            fp_watch_note(cur);
            FP_OWNER[core].store(cur, Ordering::Relaxed);
        }
    }
    // ------------------------------------------------------------------------------------------
    // aarch64: LAZY. **Bewusste Divergenz** — die Begründung steht im HAL-Vertrag
    // (`crates/caprock-hal/src/aarch64/fp.rs`), damit der nächste Leser sie nicht für ein
    // Versehen hält. Kurz: `CPACR_EL1.FPEN` trappt **nur EL0** und ist präzise; es gibt keine
    // LazyFP-Entsprechung. Die Trap-Reichweiten sind verschieden, und die Exponierung ist es auch.
    #[cfg(not(target_arch = "x86_64"))]
    hal::fp::set_el0_trap(cur != owner);
}

/// `TTBR0` (Adressraum) für den **gerade aktuellen** Thread auf `core` setzen:
/// dessen isolierte VSpace, oder die globale SAS-Map (trusted). Schreibt TTBR0 nur,
/// wenn sich die VSpace ändert (vermeidet `isb` im häufigen All-Trusted-Fall). Am
/// Ende jedes Hooks aufzurufen, der den laufenden Thread gewechselt haben kann.
fn sync_vspace(core: usize, sched: &Scheduler) {
    let slot = sched.current_id(core).slot();
    let v = vspace_of(slot);
    let want = if v == 0 { hal::mmu::global_root() } else { v };
    if CURRENT_VSPACE[core].load(Ordering::Relaxed) != want {
        let root = want & ((1u64 << 48) - 1);
        let asid = (want >> 48) as u16;
        hal::mmu::set_user_vspace(root, asid);
        // Spekulations-Barriere an der Adressraum-Grenze: nach dem Wechsel darf keine noch
        // offene Spekulation aus dem VORHERIGEN Adressraum weiterlaufen (`SB`, sonst
        // `dsb sy; isb`). Nur im Wechselfall — der All-Trusted-Pfad bleibt unbelastet.
        hal::cpu::speculation_barrier();
        CURRENT_VSPACE[core].store(want, Ordering::Relaxed);
    }
}

/// **Den Thread-Pointer des laufenden Threads an die CPU spiegeln** (TLS, T1).
///
/// Dieselbe Naht wie [`sync_vspace`] und aus demselben Grund: es ist Zustand, der **je Thread**
/// gilt und in einem **Register des Kerns** steht. Wer ihn nicht bei jedem Wechsel spiegelt, hat
/// TLS gesetzt und nicht gehalten — und das sind zwei verschiedene Aussagen, die die `tls`-Zeile
/// als `tp-gesetzt` und `ueberlebt-wechsel` getrennt fuehrt.
///
/// **Nur bei Aenderung**, mit Kernpuffer: ein `wrmsr` je Wechsel waere sonst der Preis dafuer,
/// dass die meisten Threads gar kein TLS haben.
///
/// # Warum hier NICHT noch einmal geprueft wird
///
/// Ein `WRMSR` auf `IA32_FS_BASE` mit nicht-kanonischem Wert faultet in **Ring 0** — die
/// Schranke ist also lebenswichtig, und es gibt **zwei** Stellen, an denen der Wert den Kern
/// erreicht: `SYS_SETTLS` und diese hier.
///
/// Gedeckt ist diese hier dadurch, dass der Wert **beim Speichern** gegen
/// `caprock_abi::USER_VA_TOP` geprueft wurde: in `Tcb::tls` steht nie etwas, das den Dispatch
/// nicht passiert hat, und `Tcb::EMPTY` traegt `0`. Der Satz steht hier ausgeschrieben, weil er
/// sonst beim naechsten Umbau verlorengeht — wer je eine zweite Schreibstelle fuer `tls`
/// einfuehrt, muss die Pruefung mitnehmen oder sie hierher holen.
///
/// **Auf aarch64 gibt es das Problem gar nicht**: `TPIDR_EL0` nimmt jeden Wert. Dieselbe Form wie
/// E-B1 — eine Architektur stuetzt sich auf eine Pruefung, die die andere strukturell nicht
/// braucht, und ohne den Grund an der Stelle sieht sie wie eine ueberfluessige Zeile aus.
fn sync_tls(core: usize, sched: &Scheduler) {
    let want = sched.tls_of(sched.current_id(core)) as u64;
    if CURRENT_TLS[core].load(Ordering::Relaxed) != want {
        hal::cpu::set_thread_pointer(want);
        CURRENT_TLS[core].store(want, Ordering::Relaxed);
    }
}

/// Zuletzt an den Kern geschriebener Thread-Pointer — der Puffer fuer [`sync_tls`].
#[allow(clippy::declare_interior_mutable_const)]
static CURRENT_TLS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// FP-Trap-Hook (Lazy-FP): Ein EL0-Thread hat FP/SIMD benutzt, ohne Owner zu sein.
/// Alten Owner sichern, FP-Kontext dieses Threads laden, ihn zum Owner machen und
/// FP freigeben. Rückgabe: derselbe Frame — der `eret` wiederholt die getrappte
/// Instruktion, jetzt mit aktivem FP. Hält SCHED (current_id) und FP_STATES.
/// **Unter EAGER ist ein `#NM` ein KERNELFEHLER — und wird als solcher laut.**
///
/// Er kann nur aus drei Gründen kommen, und alle drei sind Fehler *dieses* Kernels:
/// `CR0.EM` ist gesetzt (SSE nie freigegeben), `CR0.TS` ist gesetzt (jemand hat es geschrieben),
/// oder `enable_sse` ist auf **diesem** Kern nie gelaufen. Der letzte Fall ist die
/// wahrscheinlichste künftige Regression — deshalb zählt der Melder **je Kern**: ein AP, dessen
/// `enable_sse` in einem Refactor aus der Reihenfolge rutscht, ginge in einer globalen Summe
/// unter, und zwar genau in dem Lauf, in dem alle anderen Kerne ihn übertönen.
///
/// Gemeldet werden **RIP und Kern-ID** — ohne beides ist „irgendwo trappte etwas" keine Diagnose.
/// Dazu der gelesene `CR0`, damit die drei Gründe unterscheidbar sind, statt geraten zu werden.
///
/// **Es wird nicht angehalten.** Ein Panic hier reisst den Knoten nachweislich *nicht* mit
/// (`docs/invariants.md` §14, gemessen), er würde also nur die Diagnose verschlechtern. Statt
/// dessen ist der Zähler Teil der Prüfzeile: **jedes** Vorkommen macht den Lauf rot.
#[cfg(target_arch = "x86_64")]
fn fp_unerwartet(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = hal::cpu::core_id();
    NM_TRAPS[core].fetch_add(1, Ordering::Relaxed);
    // SAFETY: gültiger, vom Stub angelegter Trap-Frame.
    let rip = unsafe { (*frame).rip };
    let cr0 = hal::fp::cr0_lesen();
    println!(
        "fp      : INVARIANTE VERLETZT -- #NM unter eager auf Kern {core} (rip={rip:#018x} \
         cr0={cr0:#x} TS={} EM={}). Unter eager darf dieser Trap nicht vorkommen: entweder \
         lief `enable_sse` auf diesem Kern nicht, oder jemand hat CR0.TS geschrieben.",
        (cr0 >> 3) & 1,
        (cr0 >> 2) & 1
    );
    // `TS` löschen, damit der Lauf weitergeht und die Zeile den Lauf rot macht, statt in einer
    // Trap-Schleife zu verschwinden -- ein rekursives `#NM` sähe aus wie ein Hänger.
    hal::fp::clear_task_switched();
    frame
}

/// Wie viele unerwartete `#NM` je Kern (x86, unter eager immer 0) — Erstzeile des
/// Vektor-Inventars.
pub fn nm_traps(core: usize) -> u64 {
    NM_TRAPS.get(core).map_or(0, |c| c.load(Ordering::Relaxed))
}

/// Summe über alle Kerne. **Nur für die Kurzfassung** — das Urteil hängt an den Einzelwerten,
/// s. [`fp_unerwartet`].
pub fn nm_traps_gesamt() -> u64 {
    NM_TRAPS.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

#[allow(dead_code)] // auf aarch64 ungenutzt (dort ist Lazy-FP der reguläre Weg)
fn fp_trap(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = hal::cpu::core_id();
    let sched = SCHEDS[core].lock();
    let cur_id = sched.current_id(core);
    let cur = cur_id.to_raw();
    let cur_slot = cur_id.slot();
    let owner = FP_OWNER[core].load(Ordering::Relaxed);

    let mut states = FP_STATES.lock();
    if owner != FP_OWNER_NONE && owner != cur {
        // Live-Register gehören dem alten Owner -> in seinen Slot sichern.
        let prev_slot = ThreadId::from_raw(owner).slot();
        hal::fp::save(&mut states[prev_slot]);
        FP_SWITCHES.fetch_add(1, Ordering::Relaxed);
    }
    // FP-Kontext dieses Threads laden (bei Erststart genullt).
    hal::fp::restore(&states[cur_slot]);
    drop(states);

    fp_watch_note(cur);
    FP_OWNER[core].store(cur, Ordering::Relaxed);
    hal::fp::set_el0_trap(false); // FP für diesen (jetzt Owner-)Thread freigeben
    frame
}

/// Per-Thread-Kernelzustand für einen frisch belegten Slot zurücksetzen: genullter
/// FP-Kontext, veraltete FP-Owner-Referenzen löschen, und die VSpace auf **global**
/// (SAS) zurücksetzen (eine vorher isolierte PD auf diesem Slot darf nicht
/// nachwirken). Unter gehaltenem SCHED aufzurufen, BEVOR der Thread laufen kann.
fn fp_reset_slot(slot: usize) {
    FP_STATES.lock()[slot] = FpState::new();
    for owner in FP_OWNER.iter() {
        let o = owner.load(Ordering::Relaxed);
        if o != FP_OWNER_NONE && (o & 0xffff_ffff) as usize == slot {
            owner.store(FP_OWNER_NONE, Ordering::Relaxed);
        }
    }
    set_vspace_of(slot, 0); // Default: globale SAS-Map
}

/// Anzahl abgeschlossener Lazy-FP-Owner-Wechsel (Save eines alten Owners).
pub fn fp_switch_count() -> usize {
    FP_SWITCHES.load(Ordering::Relaxed)
}

/// Einen Thread für die Restore-Zählung eintragen (`idx` 0 oder 1). Von der Sonde selbst gerufen,
/// sobald sie ihre eigene `ThreadId` kennt.
#[cfg(feature = "selftest")]
pub fn fp_watch(idx: usize, tid: ThreadId) {
    if let Some(s) = FP_WATCH_TID.get(idx) {
        s.store(tid.to_raw(), Ordering::Relaxed);
    }
}

/// Wie oft der FP-Zustand des beobachteten Threads `idx` **aus seinem Slot geladen** wurde.
///
/// Der erste Restore lädt einen frisch genullten Slot — also *bevor* die Sonde ihr Muster
/// geschrieben hat. Für „das Muster hat eine Verdrängung überstanden" braucht es deshalb **zwei**,
/// nicht einen.
#[cfg(feature = "selftest")]
pub fn fp_watch_restores(idx: usize) -> u64 {
    FP_WATCH_RESTORES
        .get(idx)
        .map_or(0, |c| c.load(Ordering::Relaxed))
}

/// Im Wechselpfad: gehört `cur` zu einem beobachteten Thread, diesen Restore zählen.
#[cfg(feature = "selftest")]
#[inline]
fn fp_watch_note(cur: u64) {
    for (t, c) in FP_WATCH_TID.iter().zip(FP_WATCH_RESTORES.iter()) {
        if t.load(Ordering::Relaxed) == cur {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }
}
#[cfg(not(feature = "selftest"))]
#[inline]
fn fp_watch_note(_cur: u64) {}

/// EL0-Fault-Hook: ein User-Thread hat einen synchronen Fault ausgelöst (Zugriff
/// auf EL1-only Kernel-Speicher, privilegierte Instruktion o. Ä.). Statt den
/// Kernel anzuhalten, wird der fehlerhafte Thread **beendet** und auf den nächsten
/// lauffähigen Thread gewechselt — der Kernel läuft weiter. Das ist der konkrete
/// Nachweis der EL0/EL1-Privileg-Trennung: User-Code kann den Kernel nicht
/// kompromittieren. Nur der SCHED-Lock (wie der Reschedule-Pfad).
fn el0_fault(frame: *mut TrapFrame, esr: u64, far: u64) -> *mut TrapFrame {
    let core = hal::cpu::core_id();
    let ec = (esr >> 26) & 0x3f;
    EL0_FAULTS.fetch_add(1, Ordering::Relaxed);
    // --- Z26/A3: die Weiche für FAULTS -------------------------------------------------------
    //
    // Sie ist **nicht** die Spiegelung der Syscall-Weiche, und der Unterschied ist der Punkt: bei
    // einem Syscall ist „der Kernel macht es" eine **Beförderung** (der Gast bekäme die native ABI
    // zurück), bei einem Fault ist es eine **Herabstufung** (der Kernel beendet den Thread).
    // Fail-closed heisst hier also nicht dasselbe -- deshalb zwei Funktionen und nicht ein
    // Parameter mit zwei Bedeutungen.
    //
    // **Ganz oben, mit allen Locks frei.** Die Sperrordnung ist `CAPS < EPS < SCHEDS`; der
    // Diagnoseblock darunter hält `SCHEDS`, und von dort aus zuzustellen drehte die Ordnung um.
    //
    // Ohne Fault-Bindung fällt der Weg unverändert in den Pfad darunter: beenden. Das ist richtig
    // und keine Lücke -- der native Fault-Pfad gewährt nichts.
    {
        let mut ops = KernelSched;
        if let Some(next) = caprock_microkit::fault_dispatch(
            frame as usize,
            core,
            &mut ops,
            &CAPS,
            eps(),
            ec,
        ) {
            HANDLER_FAULTS.fetch_add(1, Ordering::Relaxed);
            let sched = SCHEDS[core].lock();
            sync_thread_state(core, &sched);
            return next as *mut TrapFrame;
        }
    }
    let (next, tid) = {
        let mut sched = SCHEDS[core].lock();
        let tid = sched.current_id(core);
        if vspace_of(tid.slot()) != 0 {
            // Der fehlerhafte Thread lief in einer isolierten VSpace -> die
            // Adressraumtrennung hat einen Fremd-/unmapped-Zugriff verhindert.
            ISO_FAULTS.fetch_add(1, Ordering::Release);
        }
        // **Wer** faultete, nicht nur welche Zahl. Drei Zeilen „User-Thread 0x8 faultete" sahen
        // am 2026-08-10 gleich aus, waehrend genau eine davon die gesuchte war -- eine
        // Fehlermeldung ohne Subjekt ist Buchhaltung, keine Diagnose. `unbekannt` heisst
        // ausdruecklich *unbekannt* (Kernel-Thread, Testfaden), nicht *keins*.
        match crate::loader::program_of_thread(tid) {
            Some(pid) => println!(
                "el0-trap: User-Thread {:#x} (Programm {pid}) faultete (EC={ec:#04x} FAR={far:#018x}) -> beendet, Kernel laeuft weiter",
                tid.to_raw()
            ),
            None => println!(
                "el0-trap: User-Thread {:#x} (Programm unbekannt -- kein Ladepfad hat ihn erzeugt) faultete (EC={ec:#04x} FAR={far:#018x}) -> beendet, Kernel laeuft weiter",
                tid.to_raw()
            ),
        }
        let next = charged(core, &mut sched, |s| s.exit_current(core, frame as usize));
        // Auf den nächsten Thread gewechselt -> FP-Trap + VSpace passend setzen.
        sync_thread_state(core, &sched);
        (next, tid)
    }; // SCHEDS freigegeben
    purge_ipc_queues(tid); // eager: faultenden Thread aus allen IPC-Queues entfernen
    reclaim_user_kstack(tid, "el0_fault"); // EL0-Kernel-Stack-Pool-Slot zurückgeben (KSTACKS allein)
    // War es eine isolierte PD, ihre VSpace abbauen. Sicher: `sync_vspace` oben hat
    // TTBR0 bereits auf den nächsten Thread umgeschaltet (nicht mehr die tote VSpace).
    let packed = vspace_of(tid.slot());
    if packed != 0 {
        vspace_teardown((packed >> 48) as u16);
        set_vspace_of(tid.slot(), 0);
    }
    // **Alles ab `reclaim_user_kstack` lief auf einer bereits freigegebenen Region** — auf aarch64
    // benutzt der Trap-Pfad SP_EL1, und das ist der Kernel-Stack genau dieses Threads; erst der
    // Vektor-Epilog (`mov sp, x0`) schaltet um. Gemessen als `KSTACK_UNTER_FUESSEN` (32,8 je Lauf);
    // gemessen ist aber auch, dass das Fenster unter dieser Last nicht genommen wird (0 von 4295).
    next as *mut TrapFrame
}

/// Wie oft hat der Kernel einen EL0-Fault abgefangen und den Thread isoliert?
pub fn el0_fault_count() -> usize {
    EL0_FAULTS.load(Ordering::Relaxed)
}

/// Hat ein Thread in einer **isolierten VSpace** durch einen Fremd-/unmapped-Zugriff
/// gefaultet? (Hardware-erzwungene Adressraumtrennung, Weg C.)
pub fn iso_faulted() -> bool {
    ISO_FAULTS.load(Ordering::Acquire) > 0
}
/// Anzahl der Faults in isolierten VSpaces (Fremdzugriff bzw. unmapped Frame).
pub fn iso_fault_count() -> usize {
    ISO_FAULTS.load(Ordering::Acquire)
}

/// Reschedule-, Syscall-, EL0-Fault- + Lazy-FP-Hook registrieren (einmalig, vor
/// IRQs/Threads).
pub fn set_hooks() {
    hal::exception::set_reschedule_hook(reschedule);
    hal::exception::set_syscall_hook(syscall);
    hal::exception::set_fault_hook(el0_fault);
    hal::exception::set_irq_hook(irq_hook); // Geräte-IRQ-Hook (ext-22, P5)
    #[cfg(target_arch = "x86_64")]
    hal::exception::set_fp_hook(fp_unerwartet); // eager: ein `#NM` ist ein Kernelfehler
    #[cfg(not(target_arch = "x86_64"))]
    hal::exception::set_fp_hook(fp_trap); // aarch64: Lazy-FP ist dort der regulaere Weg
}

/// Freies RAM `[free_base, ram_end)` beim Allokator registrieren.
pub fn init_mem(free_base: u64, ram_end: u64) {
    init_mem_regions(&[(free_base, ram_end - free_base)], ram_end);
}

/// Freien Speicher als **Liste von Bereichen** registrieren.
///
/// Der Kernel bekam seinen Speicher bisher als genau ein `[free_base, ram_end)`. Das ist die
/// Sicht eines Kernels, der die Maschine allein besitzt — und genau die gilt nicht mehr, sobald
/// er neben einem anderen Betriebssystem auf uebergebenen Kernen laeuft: dort kommt der Speicher
/// als Sammlung dessen, was der Wirt hergibt (auf Linux hoechstens 4 MiB am Stueck, weil die
/// Buddy-Ordnung dort endet).
///
/// Der Allokator selbst fuehrt ohnehin eine Fragmentliste; die Einschraenkung lag allein in
/// dieser Funktion. `ram_top` bleibt separat, weil daran die Wahl des IOVA-Fensters haengt —
/// es muss oberhalb des **hoechsten** physischen Speichers liegen, nicht oberhalb des letzten
/// uebergebenen Bereichs.
pub fn init_mem_regions(regions: &[(u64, u64)], ram_top: u64) {
    RAM_TOP.store(ram_top, Ordering::Release);
    let mut mem = MEM.lock();
    for &(base, len) in regions {
        if len != 0 && !mem.add_region(base, len) {
            // Laut scheitern statt still weniger Speicher zu haben: ein verworfener Bereich
            // faellt sonst erst auf, wenn eine spaetere Allokation ohne erkennbaren Grund
            // fehlschlaegt.
            // Die serielle Ausgabe haengt am aarch64-HAL (s. `note_undeclared_device`).
            #[cfg(target_arch = "aarch64")]
            crate::println!(
                "mem     : WARNUNG Bereich 0x{:x}+0x{:x} nicht aufgenommen (Fragmentliste voll)",
                base,
                len
            );
            MEM_REGIONS_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Oberste physische RAM-Adresse (aus dem Boot). Grundlage für die Wahl des IOVA-Fensters:
/// oberhalb davon kann eine IOVA **nie** eine gültige PA sein.
static RAM_TOP: AtomicU64 = AtomicU64::new(0);

/// Wie viele beim Hochlauf angebotene Speicherbereiche **nicht** aufgenommen wurden.
///
/// Muss 0 sein. Ein verworfener Bereich faellt sonst erst auf, wenn eine spaetere Allokation
/// ohne erkennbaren Grund fehlschlaegt — gezaehlt statt stillschweigend hingenommen.
static MEM_REGIONS_DROPPED: AtomicU32 = AtomicU32::new(0);

/// Anzahl beim Hochlauf verworfener Speicherbereiche (muss 0 sein).
pub fn mem_regions_dropped() -> u32 {
    MEM_REGIONS_DROPPED.load(Ordering::Relaxed)
}

/// **Thread-Slots je Kern**, die der Kernel im Mittel vorsieht. Die Gesamtkapazität ist
/// `cores * THREADS_PER_CORE`; jeder Kern kann durch Migration bis zum
/// [`MIGRATION_HEADROOM`]-fachen davon *hosten*.
const THREADS_PER_CORE: usize = 256;

/// **Ziel-Thread-Kapazitaet des Systems** (A-3.4).
///
/// Bis hierher ergab sich die Kapazitaet als `cores * THREADS_PER_CORE` -- auf einer
/// 4-Kern-Maschine also 1024, und auf einer 2-Kern-Maschine die Haelfte. Das ist eine Eigenschaft
/// des **Testaufbaus**, keine Zusage des Systems: dieselbe Software haette je nach Maschine eine
/// andere Obergrenze, ohne dass es irgendwo steht.
///
/// Jetzt ist die Kapazitaet eine **Zusage**, und die Kernzahl teilt sie nur auf. Die Tabellen
/// dahinter sind seit ext-30 ohnehin boot-dimensioniert (`attach_*`), nicht `.bss` -- die Zahl
/// kostet also RAM, keine Struktur. Was sie kostet, meldet der Boot-Report (`bytes`), damit die
/// Entscheidung an einer gemessenen Groesse haengt und nicht an einem Gefuehl.
///
/// **Untergrenze bleibt `THREADS_PER_CORE` je Kern**: eine Maschine mit vielen Kernen soll nicht
/// weniger Threads je Kern haben als vorher.
pub const TARGET_THREADS: usize = 10_000;
/// Faktor, um den die **Hosting**-Kapazität eines Kerns über seinem Anteil liegt. Ohne
/// Reserve könnte kein Kern einen migrierten Thread aufnehmen, sobald alle Kerne ihren
/// Anteil ausgeschöpft haben.
const MIGRATION_HEADROOM: usize = 2;

/// **Kernel-Tabellen zur Boot-Zeit dimensionieren + anlegen** (ext-30).
///
/// Ersetzt die früheren `.bss`-Arrays mit Compile-Zeit-Konstanten (`[Tcb; PER_CORE]`,
/// `[FpState; MAX_THREADS]`, …). `cores` ist die **gemessene** Kernzahl (aus dem Device
/// Tree); daraus folgen Thread-Kapazität und Tabellengrößen. Danach sind alle
/// per-Kern-Scheduler an ihre Kern-ID gebunden — vom Bootkern **einmalig vor** dem
/// Erzeugen irgendwelcher Threads und **vor** dem Start der Sekundärkerne aufzurufen
/// (der Bootkern plant Threads auf noch nicht gestartete Kerne ein).
///
/// Gibt `(cores, threads_total, per_core_hosting, bytes)` zurück (Boot-Report).
pub fn configure(cores: usize) -> (usize, usize, usize, u64) {
    let cores = cores.clamp(1, MAX_CORES);
    NUM_CORES.store(cores, Ordering::Relaxed);
    // Die Ziel-Kapazitaet wird auf die Kerne aufgeteilt, mindestens aber THREADS_PER_CORE je Kern
    // (aufgerundet, damit die Summe das Ziel nie unterschreitet).
    let je_kern = ((TARGET_THREADS + cores - 1) / cores).max(THREADS_PER_CORE);
    let total = cores * je_kern;
    let per_core = je_kern * MIGRATION_HEADROOM;
    let mut bytes = 0u64;

    // Eine Tabelle belegen. Der Speicher gehört ab hier **dauerhaft** dem Kernel: die
    // MemoryCap wird bewusst fallen gelassen (kein `Drop` -> die Region kehrt nie in den
    // Allokator zurück). Kerneltabellen leben bis zum Reboot.
    let mut table = |size: usize, align: usize| -> *mut u8 {
        let cap = mem_alloc_anywhere(size as u64, align as u64).expect("Kerneltabelle: RAM erschoepft");
        bytes += cap.len();
        cap.base() as *mut u8
    };

    // Thread-Directory (gid -> Kern/Slot) + gid-Freiliste.
    let dir = table(
        caprock_sched::directory_bytes(total),
        caprock_sched::directory_align().max(4096),
    );
    // SAFETY: frisch allozierter, exklusiver, korrekt ausgerichteter Speicher der
    // geforderten Größe, der bis zum Reboot lebt; einmaliger Aufruf beim Boot, bevor ein
    // anderer Kern läuft.
    unsafe { caprock_sched::attach_directory(dir, total) };

    // Per-Kern-Tabellen (TCBs + Zombie-Ring + Freiliste).
    let core_bytes = caprock_sched::core_storage_bytes(per_core);
    for c in 0..cores {
        let mem = table(core_bytes, caprock_sched::core_storage_align().max(4096));
        // SAFETY: wie oben; jede Instanz bekommt ihren **eigenen** Block (exklusiv).
        unsafe { SCHEDS[c].lock().attach_storage(c, mem, per_core) };
        // B-5.1: die Zyklenquelle **zusichern oder nicht**. `invariant_tsc()` fragt die Hardware
        // (x86: `CPUID.80000007H:EDX[8]`; auf aarch64 ist der architektonische Zähler per
        // Konstruktion invariant). Sagt sie nein, bleibt es bei `Untrusted` — dann wird nichts
        // abgerechnet, und das steht als `rejected_source` im Bericht statt als stille Null.
        // Genau darum wird hier **gefragt** und nicht angenommen: unter TCG lautet die Antwort
        // je nach CPU-Modell verschieden, und eine erfundene Rechnung wäre schlimmer als keine.
        SCHEDS[c].lock().set_cycle_source(if hal::timer::invariant_tsc() {
            caprock_sched::Source::Invariant
        } else {
            caprock_sched::Source::Untrusted
        });
    }

    // Per-Thread-Tabellen des Kernels (über den globalen, migrationsstabilen Slot indiziert).
    let fp = table(
        total * core::mem::size_of::<FpState>(),
        core::mem::align_of::<FpState>().max(4096),
    );
    // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig).
    unsafe {
        FP_STATES
            .lock()
            .attach(fp as *mut FpState, total, |_| FpState::new())
    };
    let vs = table(
        total * core::mem::size_of::<AtomicU64>(),
        core::mem::align_of::<AtomicU64>().max(4096),
    );
    // SAFETY: wie oben; `VSPACE_OF` wird erst nach diesem Anhängen gelesen.
    unsafe {
        VSPACE_OF.attach(vs as *mut AtomicU64, total, |_| AtomicU64::new(0));
    }
    let ks = table(
        total * core::mem::size_of::<u64>(),
        core::mem::align_of::<u64>().max(4096),
    );
    // SAFETY: wie oben.
    unsafe {
        KSTACKS
            .lock()
            .base_of
            .attach(ks as *mut u64, total, |_| 0u64)
    };
    // D15: die Generation neben der Basis. Eigene Tabelle statt Bits im `u64`: eine Generation in
    // den unteren 12 Bits der (seitenausgerichteten) Basis waere nach 4096 Wiederverwendungen
    // desselben Slots mehrdeutig — und der Churn-Test allein faehrt 2000 Zyklen ueber denselben
    // LIFO-Kopf. Eine Ratsche mit Ueberlauf ist keine.
    let ksg = table(
        total * core::mem::size_of::<u32>(),
        core::mem::align_of::<u32>().max(4096),
    );
    // SAFETY: wie oben.
    unsafe {
        KSTACKS
            .lock()
            .gen_of
            .attach(ksg as *mut u32, total, |_| 0u32)
    };

    // **C7b: die EL0-USER-Regionen je Thread-Slot** (Basis + Laenge). Zwei weitere Tabellen
    // derselben Dimension; sie kosten `2 * 8 * total` Byte und stehen im Boot-Report (`bytes`),
    // damit auch diese Entscheidung an einer gemessenen Groesse haengt.
    //
    // **Nur unter `selftest`** -- die Marke ist Messmaschinerie und gehoert nicht in den schlanken
    // Kernel (dieselbe Haltung wie `kstackmark::fuellen`, das dort ein No-Op ist). Ohne die
    // Tabellen bleiben die Slabs LEER, und jeder Zugriff darauf prueft das; ein `record` in einen
    // leeren Slab waere ein Fehlgriff, kein Messausfall.
    #[cfg(feature = "selftest")]
    {
        let ub = table(
            total * core::mem::size_of::<u64>(),
            core::mem::align_of::<u64>().max(4096),
        );
        let ul = table(
            total * core::mem::size_of::<u64>(),
            core::mem::align_of::<u64>().max(4096),
        );
        // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig).
        unsafe {
            let mut p = KSTACKS.lock();
            p.ubase_of.attach(ub as *mut u64, total, |_| 0u64);
            p.ulen_of.attach(ul as *mut u64, total, |_| 0u64);
        };
    }

    // **Rueckwaerts-Index Thread-Slot -> PD** (Z22 P2 / C4). Die vierte per-Thread-Tabelle, und
    // sie wird HIER angehaengt und nicht in `configure_caps`: dort ist die Thread-Kapazitaet
    // noch nicht bekannt (`configure_caps` laeuft frueher). Vor dem ersten `bind_thread` ist sie
    // trotzdem da -- Threads gibt es erst nach diesem Aufruf.
    //
    // Ohne sie loest `pd_of` linear ueber alle `NPDS` PDs auf, und das bei JEDEM Syscall. Was
    // sie kostet, steht im Boot-Report (`bytes`), damit die Entscheidung an einer gemessenen
    // Groesse haengt.
    // **K1b: die Stack-Cap je Thread-Slot.** Sie traegt den `ERR_INUSE`-Schutz UND die
    // Geschwister-Ueberlappung, haengt also unbedingt und nicht nur unter `selftest`. Dimensioniert
    // mit demselben `total` wie die Tabellen darueber -- die vorige feste Schranke von 1024 hat
    // auf aarch64 jeden `SYS_SPAWN` abgewiesen, weil die Slots dort weit darueber liegen.
    let sc = table(
        total * core::mem::size_of::<Option<StackEintrag>>(),
        core::mem::align_of::<Option<StackEintrag>>().max(4096),
    );
    // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig).
    unsafe {
        STACK_CAP_OF
            .lock()
            .attach(sc as *mut Option<StackEintrag>, total, |_| None)
    };

    let owner = table(
        total * core::mem::size_of::<caprock_microkit::ThreadOwner>(),
        core::mem::align_of::<caprock_microkit::ThreadOwner>().max(4096),
    );
    // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig) -- und vor der ersten
    // PD-Bindung, weil es vor dem ersten Thread liegt.
    unsafe {
        CAPS.write()
            .pds
            .attach_owner(owner as *mut caprock_microkit::ThreadOwner, total)
    };

    // C7c: Stack-Arena bestuecken — steht hier am Ende von `configure`, nicht bei den
    // KSTACKS-Tabellen oben, weil `table` dort `bytes` leiht: einmalig, vor dem ersten
    // Thread, `0` = nicht bestueckt (Streupfad weiter). Die Plaetze leitet die Arena aus
    // Thread-Zahl, Boot-RAM, Guard-Vorrat und Deckel ab (s. `kstack_arena_plan`); Spanne +
    // Bitmap stehen im Boot-Report.
    bytes += kstack_arena_bestuecken(total);

    (cores, total, per_core, bytes)
}

/// **Slot-Kapazität des globalen Capability-Space** (A-3.4).
///
/// Summe aller PD-Budgets plus Kernel-Reserve. Die Rechnung stand hier als
/// `CAP_SLOTS_FOR_ALL_PDS + 256` und damit an einer zweiten Stelle neben der Definition der
/// Budgets; sie ist nach `caprock_microkit::CAP_SLOTS_TOTAL` gezogen. Was die Reserve deckt und
/// warum sie neben der Summe steht, ist dort dokumentiert — **nachgezählt** wird sie von
/// `cap_nonbudget_slots`.
const CAP_SLOTS: usize = caprock_microkit::CAP_SLOTS_TOTAL;

/// **Objekt-Kapazität** — genauso viele wie Slots.
///
/// Bis A-3.4 stand das Verhältnis auf 2:1 (256 Slots / 128 Objekte), aus der Beobachtung heraus,
/// dass viele Caps Ableitungen auf dasselbe Objekt sind. Als Zusage taugt das nicht: `install`
/// erzeugt Slot **und** Objekt, ein System aus lauter Wurzel-Caps braucht also so viele Objekte wie
/// Slots. Mit 2:1 wäre „jede PD bekommt ihr Budget" an der Objekttabelle gescheitert statt an der
/// Slot-Tabelle — dieselbe Lücke, eine Ebene tiefer.
const CAP_OBJECTS: usize = CAP_SLOTS;

/// **Capability-Tabellen zur Boot-Zeit anlegen** (A-3.4, Teil 2) — Slot- und Objekttabelle des
/// globalen `CapSpace`, dazu der Finalisierungspuffer und die Zählfläche des CDT-Audits.
///
/// **Muss vor der ersten Cap-Installation laufen**, also vor `selftest::run()` und vor allem, was
/// Speicher als Cap ausgibt. Vorher hat der Space Kapazität 0 und jede Installation scheitert mit
/// `NoSlot` — ein vergessener Aufruf fällt damit als sauberer Fehler auf, nicht als Fehlzugriff.
///
/// Der Finalisierungspuffer kommt hier aus **derselben Quelle** wie die Tabellen — genau das hatte
/// A-3.3 vorbereitet, als es die Arrays aus `Finalized` in geliehene Slices verwandelte: die
/// Objekttabelle wachsen zu lassen, ohne den Kernelstack jedes Threads mitwachsen zu lassen.
///
/// Gibt die belegten Bytes zurück (Boot-Report).
pub fn configure_caps() -> u64 {
    let mut bytes = 0u64;
    // Wie in `configure`: der Speicher gehört ab hier dauerhaft dem Kernel (die MemoryCap wird
    // bewusst fallen gelassen — Kerneltabellen leben bis zum Reboot).
    let mut table = |size: usize, align: usize| -> *mut u8 {
        let cap = mem_alloc_anywhere(size as u64, align as u64).expect("Cap-Tabelle: RAM erschoepft");
        bytes += cap.len();
        cap.base() as *mut u8
    };

    let slots_mem = table(
        CAP_SLOTS * core::mem::size_of::<caprock_cap::CapSlot>(),
        core::mem::align_of::<caprock_cap::CapSlot>().max(4096),
    );
    let objs_mem = table(
        CAP_OBJECTS * core::mem::size_of::<caprock_cap::Object>(),
        core::mem::align_of::<caprock_cap::Object>().max(4096),
    );
    let mut slots: Slab<caprock_cap::CapSlot> = Slab::empty();
    let mut objects: Slab<caprock_cap::Object> = Slab::empty();
    // SAFETY: frisch allozierter, exklusiver, korrekt ausgerichteter Speicher der geforderten
    // Größe, der bis zum Reboot lebt; einmaliger Aufruf beim Boot vor jeder Cap-Operation.
    unsafe {
        slots.attach(
            slots_mem as *mut caprock_cap::CapSlot,
            CAP_SLOTS,
            |_| caprock_cap::CapSlot::EMPTY,
        );
        objects.attach(
            objs_mem as *mut caprock_cap::Object,
            CAP_OBJECTS,
            |_| caprock_cap::Object::EMPTY,
        );
    }
    CAPS.write().cspace.attach(slots, objects);
    // Die Zusage "jede PD bekommt ihr Budget" haengt daran, dass die Tabelle wirklich so gross
    // ist wie gerechnet. `attach` nimmt entgegen, was der Aufrufer gibt -- eine zu kleine Tabelle
    // faellt sonst erst auf, wenn eine PD innerhalb ihres Budgets `NoSlot` bekommt, also
    // fruehestens unter Last und ohne Hinweis auf die Ursache.
    {
        let g = CAPS.read();
        let (have_slots, have_objs) = g.cspace.capacity();
        assert!(
            have_slots >= caprock_microkit::CAP_SLOTS_TOTAL && have_objs >= CAP_OBJECTS,
            "Cap-Tabellen kleiner als gerechnet: {have_slots}/{have_objs} Slots/Objekte"
        );
    }

    // Finalisierungspuffer + Enforcer-Kratzflächen: je ein Eintrag pro Objekt, denn mehr Objekte
    // als die Tabelle fasst kann eine einzelne `delete`/`revoke`-Operation nicht finalisieren.
    let n = CAP_OBJECTS;
    let items = table(n * core::mem::size_of::<(u32, u64)>(), 4096);
    let dma = table(n * core::mem::size_of::<(u64, u64)>(), 4096);
    let ok = table(n, 4096);
    let ctx = table(n * core::mem::size_of::<usize>(), 4096);
    let dbg = table(n * core::mem::size_of::<u16>(), 4096);
    {
        let mut b = FINALIZE.lock();
        // SAFETY: wie oben; jede Tabelle bekommt ihren **eigenen** Block (exklusiv).
        unsafe {
            b.items.attach(items as *mut (u32, u64), n, |_| (0, 0));
            b.dma.attach(dma as *mut (u64, u64), n, |_| (0, 0));
            b.ok.attach(ok as *mut bool, n, |_| false);
            b.ctx_of.attach(ctx as *mut usize, n, |_| usize::MAX);
            b.debug.attach(dbg as *mut u16, n, |_| 0u16);
        }
    }

    let refs = table(n * core::mem::size_of::<u32>(), 4096);
    // SAFETY: wie oben.
    unsafe { CAP_AUDIT.lock().attach(refs as *mut u32, n, |_| 0u32) };

    // Zaehlflaeche der Summenpruefung: ein Markierbit je SLOT (nicht je Objekt), s.
    // `cap_nonbudget_slots`.
    let seen = table(CAP_SLOTS, 4096);
    // SAFETY: wie oben.
    unsafe { CAP_SEEN.lock().attach(seen as *mut bool, CAP_SLOTS, |_| false) };

    // A-3.4 Teil 3: die PD-Tabelle. Sie war der Grund, warum 10000 Threads keine 10000 Tenants
    // waren -- `[Pd; 256]` im `.bss`. Ab hier gilt dasselbe wie fuer die Cap-Tabellen: die Zahl
    // kostet RAM, keine Struktur, und was sie kostet, steht im Boot-Report.
    let pds_mem = table(
        caprock_microkit::NPDS * core::mem::size_of::<caprock_microkit::Pd>(),
        core::mem::align_of::<caprock_microkit::Pd>().max(4096),
    );
    // SAFETY: frisch allozierter, exklusiver, ausgerichteter Speicher der geforderten Groesse,
    // der bis zum Reboot lebt; einmaliger Aufruf beim Boot VOR der ersten PD.
    unsafe {
        CAPS.write().pds.attach(
            pds_mem as *mut caprock_microkit::Pd,
            caprock_microkit::NPDS,
        )
    };
    // TODO0 K1c: der Pool der PD-Cspace-Laeufe. Richtgroesse `NPDS * STANDARD_PLAETZE` —
    // genau das, was die alten Inline-Arrays kosteten; groessere PDs nehmen sich ihren
    // Mehrbedarf aus demselben Topf. VOR der ersten PD, sonst ist jede Erzeugung eine
    // benannte `PoolErschoepft`-Absage (fail-closed, kein Absturz).
    let cspace_mem = table(
        caprock_microkit::NPDS
            * caprock_microkit::STANDARD_PLAETZE as usize
            * core::mem::size_of::<Option<CapPtr>>(),
        core::mem::align_of::<Option<CapPtr>>().max(4096),
    );
    // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig).
    unsafe {
        CAPS.write().pds.attach_cspace_pool(
            cspace_mem as *mut Option<CapPtr>,
            caprock_microkit::NPDS * caprock_microkit::STANDARD_PLAETZE as usize,
        )
    };

    // Dichte: die VSpace-Tabelle (eine VSpace je PD im Grenzfall) + die Teardown-Buchhaltung
    // geladener Programme (ein Eintrag je PD im Grenzfall). Beide waren statische Reserven
    // (4096 VSpaces, 16 Images) und damit die Stelle, an der „10 000 Tenants" endete — Threads
    // (Teil 1), Caps (Teil 2) und PD-Slots (Teil 3) waren gedreht, die Adressraeume nicht.
    // VOR der ersten PD/VSpace/dem ersten Laden (der Selbsttest gleich darunter legt bereits
    // welche an): vorher haben die Tabellen Laenge 0, und jede Vergabe scheitert benannt aus
    // dem leeren Vorrat statt daneben zu greifen.
    let vspaces_mem = table(
        caprock_microkit::NPDS * core::mem::size_of::<VSpaceEnt>(),
        core::mem::align_of::<VSpaceEnt>().max(4096),
    );
    // SAFETY: wie oben (exklusiv, ausgerichtet, dauerhaft, einmalig).
    unsafe {
        VSPACES.lock().attach(
            vspaces_mem as *mut VSpaceEnt,
            caprock_microkit::NPDS,
            |_| VSpaceEnt::FREI,
        )
    };
    let loaded_mem = table(
        caprock_microkit::NPDS * core::mem::size_of::<LoadedImage>(),
        core::mem::align_of::<LoadedImage>().max(4096),
    );
    // SAFETY: wie oben.
    unsafe {
        LOADED_IMAGES.lock().attach(
            loaded_mem as *mut LoadedImage,
            caprock_microkit::NPDS,
            |_| LoadedImage::EMPTY,
        )
    };

    bytes
}

/// **Endpoint-Kapazität** (A-3.4 Teil 4): einer je PD, dazu eine Reserve für die Kanäle, die
/// der Kernel selbst aufmacht (Bringup-Kanäle, HardwareLand-Backends, Testkanäle).
///
/// Die Reserve steht **neben** der Summe, nicht in ihr — aus demselben Grund wie bei `CAP_SLOTS`:
/// eine PD, die ihren Endpoint nimmt, soll dem Kernel keinen wegnehmen können.
const NEPS: usize = caprock_microkit::ENDPOINTS_FOR_ALL_PDS + 64;
/// **Notification-Kapazität** — wie [`NEPS`], eine je PD plus Reserve.
const NNTFNS: usize = caprock_microkit::NOTIFICATIONS_FOR_ALL_PDS + 64;

/// **IPC-Tabellen zur Boot-Zeit anlegen** (A-3.4 Teil 4) — Endpoints, Notifications und die
/// Sammelfläche für verwaiste Aufrufer.
///
/// **Muss vor der ersten IPC laufen** und vor allem, was einen Endpoint reserviert
/// (`create_endpoint`/`create_notification`, also vor `selftest::run()` und vor den
/// Bringup-Kanälen). Vorher haben die Tabellen Länge 0: `create_endpoint` findet keinen freien
/// Slot (`None`), der Dispatch weist jede Endpoint-Cap mit `ERR_BADCAP` ab. Ein vergessener
/// Aufruf fällt damit als sauberer Fehler auf, nicht als Fehlzugriff.
///
/// Gibt die belegten Bytes zurück (Boot-Report).
pub fn configure_ipc() -> u64 {
    let mut bytes = 0u64;
    let mut table = |size: usize, align: usize| -> *mut u8 {
        let cap = mem_alloc_anywhere(size as u64, align as u64).expect("IPC-Tabelle: RAM erschoepft");
        bytes += cap.len();
        cap.base() as *mut u8
    };

    let eps_mem = table(
        NEPS * core::mem::size_of::<SpinLock<Endpoint>>(),
        core::mem::align_of::<SpinLock<Endpoint>>().max(4096),
    );
    let ntfns_mem = table(
        NNTFNS * core::mem::size_of::<SpinLock<Notification>>(),
        core::mem::align_of::<SpinLock<Notification>>().max(4096),
    );
    let orphans_mem = table(
        NEPS * core::mem::size_of::<Option<ThreadId>>(),
        core::mem::align_of::<Option<ThreadId>>().max(4096),
    );

    // SAFETY: frisch allozierter, exklusiver, korrekt ausgerichteter Speicher der geforderten
    // Größe, der bis zum Reboot lebt. Einmaliger Aufruf beim Boot auf dem Boot-Kern, bevor ein
    // weiterer Kern startet und bevor die erste IPC möglich ist — der Vertrag von
    // `BootSlab::attach`.
    unsafe {
        EPS.attach(eps_mem as *mut SpinLock<Endpoint>, NEPS, |_| {
            SpinLock::new(Endpoint::EMPTY)
        });
        NTFNS.attach(ntfns_mem as *mut SpinLock<Notification>, NNTFNS, |_| {
            SpinLock::new(Notification::EMPTY)
        });
        IPC_ORPHANS
            .lock()
            .attach(orphans_mem as *mut Option<ThreadId>, NEPS, |_| None);
    }

    // Gemeldet wird die **angehaengte** Kapazitaet, nicht `NEPS`/`NNTFNS`: eine Zahl, die neben
    // der Tabelle her existiert, kann von ihr abweichen — und genau daran haengt die Pruefung
    // der Testsuite.
    let (n_eps, n_ntfns) = ipc_capacity();
    println!(
        "ipc     : {n_eps} Endpoints / {n_ntfns} Notifications, Tabellen {} KiB aus dem RAM (eine PD, ein Endpoint: {} PDs)",
        bytes >> 10,
        caprock_microkit::ENDPOINTS_FOR_ALL_PDS,
    );
    bytes
}

/// Kapazität der IPC-Tabellen (Selbsttest/Telemetrie): `(Endpoints, Notifications)` — die
/// **tatsächlich** zugewiesene, nicht die Konstante.
pub fn ipc_capacity() -> (usize, usize) {
    (eps().len(), ntfns().len())
}

/// Boot-Kontext des aufrufenden Kerns als Idle-Thread registrieren (vor IRQs).
pub fn init_core() {
    let core = hal::cpu::core_id();
    SCHEDS[core].lock().init_core(core, IDLE_PRIO);
}

// --- Allokator (MEM) ---

/// Physische Region `[base, base+len)` **nullen**.
///
/// Datenremanenz-Schutz: der Allokator vergibt Regionen wieder, die zuvor einem anderen
/// Subjekt gehörten (Thread-Stack, isolierte PD-Region, DMA-Puffer, geladene Segmente).
/// Ohne Nullung könnte der neue Eigentümer die Restdaten des alten lesen — eine
/// Vertraulichkeitslücke ÜBER Vertrauensgrenzen hinweg (und Boot-RAM enthält ohnehin
/// unbekannten Firmware-Inhalt). Deshalb wird **bei der Vergabe** genullt, nicht bei der
/// Rückgabe: das deckt auch fabrikfrisches RAM ab und ist gegen jeden Freigabepfad robust
/// (auch solche, die eine Region ohne `free` verlieren).
fn zero_phys(base: u64, len: u64) {
    // SAFETY: Der Bereich stammt direkt aus dem Allokator, gehört also exklusiv uns
    // (noch an kein Subjekt vergeben), liegt vollständig im identity-gemappten Normal-RAM
    // und wird nicht gleichzeitig benutzt. Rohspeicher-Initialisierung ist eine erlaubte
    // `unsafe`-Domäne (ADR 0021).
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, len as usize) };
}

/// **Zentrale RAM-Allokation des Kernels: liefert stets genullten Speicher.**
/// Jede Region, die an ein Subjekt gehen kann, MUSS hierüber kommen (nicht direkt über
/// `MEM.lock().alloc`) — s. [`zero_phys`]. Genullt wird **außerhalb** des MEM-Locks: die
/// Region ist bereits exklusiv unsere, und das Nullen großer Blöcke soll den Allokator
/// nicht blockieren.
fn mem_alloc(size: u64, align: u64) -> Option<MemoryCap> {
    zoned_alloc(size, align, Zone::IdentityMapped)
}

/// **Reiner Kernel-Speicher** (E-Rest 3d): dieser Puffer wird NIE in den Adressraum einer PD
/// abgebildet und nie einem Geraet gezeigt — der Kernel erreicht ihn ausschliesslich ueber seine
/// eigene Identitaetskarte. Er soll deshalb **oberhalb 4 GiB** liegen, wenn es dort Speicher gibt.
///
/// Das ist keine Optimierung, sondern die Auflösung einer Ressourcenkonkurrenz: GiB 0 ist die
/// knappe Zone (nur dort kann eine PD-eigene Abbildung entstehen, s. [`Zone::IdentityMapped`]), und
/// jeder Kernel-Stack, jede Seitentabelle und jede Kerneltabelle, die dort liegt, nimmt einem
/// Mandanten den Platz weg.
fn mem_alloc_anywhere(size: u64, align: u64) -> Option<MemoryCap> {
    zoned_alloc(size, align, Zone::Anywhere)
}

/// **Wofuer der Speicher gebraucht wird — und daraus folgt, wo er liegen darf.**
///
/// Bis E-Rest 3d gab es diese Unterscheidung nicht: `mem_alloc` gab „unten zuerst" vor, **weil**
/// unbekannt war, wer alles darauf baut. Das war eine Vorsichtsmassnahme, keine Zusicherung — und
/// eine Vorsichtsmassnahme, die den gesamten Speicher oberhalb 4 GiB praktisch zur Reserve machte.
///
/// # Die Aufzaehlung, um die es in E-Rest 3d ging
///
/// **[`Zone::Anywhere`] — bevorzugt oberhalb 4 GiB, gemessen tragfaehig:**
/// Thread-/Cap-/IPC-Tabellen ([`Slab`]-Rueckwaende), Kernel-Thread-Stacks (`STACK_SIZE`),
/// die Segment- und Stack-Frames **geladener Programme** (`load_into_pd` bildet ueber
/// `vspace_map_page_at` ab, das VA und PA **getrennt** nimmt — die Physadresse ist frei),
/// alle L3-Seitentabellen (Ladepfad und [`map_region_into_thread`]), AP-Stacks und die
/// Sekundaerstacks aus `main.rs`.
/// Gemessen bei `-m 3G` und `-m 6G`: **alle 28** dieser Allokationen liegen oberhalb 4 GiB,
/// Haupt- und Lade-Suite `== ALL PASS ==`.
///
/// **[`Zone::IdentityMapped`] — muss tief liegen, und das ist strukturell:**
/// alles, was ueber [`vspace_map`]/`map_frame`/[`map_into_thread`] **identisch** abgebildet wird
/// (VA == PA). `hal::mmu::vspace_map_page_at` weist `va >= GIB1_END` ab, `pd_block_index`
/// ebenso — eine Region darueber ist fuer eine PD schlicht nicht adressierbar.
/// **Gegenprobe gefahren:** wird `alloc` auf [`Zone::Anywhere`] gestellt, faellt die
/// **Lade-Suite** bei `-m 3G` aus (`drv`/`blkdev`/`dmaiso`: die Treiber-PD wird nie bereit) —
/// die Hauptsuite bleibt dabei gruen und haette den Fehler durchgelassen.
///
/// **Harte Bedingung statt Vorliebe** ([`mem_alloc_below`] mit [`gib0_zone`]): die private
/// Region einer isolierten PD, die Code-/Stack-Frames von [`spawn_isolated_native`] und
/// [`alloc_dma_region`]. Diese bekommen `None` statt einer unbrauchbaren Adresse.
///
/// **Noch nicht klassifiziert** (bewusst konservativ auf [`Zone::IdentityMapped`], s. `todo.md`):
/// die EL0-Kernel-Stacks ([`claim_user_kstack`]), der EL0-User-Stack von [`spawn_user`], die
/// IOMMU-Tabellen (`alloc_zeroed`) und die Sentinel-Page des DMA-Tests. Fuer keine davon ist
/// gezeigt, dass sie oben liegen **darf**; „konservativ" heisst hier ungeprueft, nicht sicher.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// **Wird IDENTISCH abgebildet** (VA == PA) — ueber `vspace_map_block`/`vspace_map_page`,
    /// `map_frame` oder [`map_into_thread`]. Muss deshalb tief liegen: die per-PD-Tabelle deckt
    /// nur GiB 1 ab, und `vspace_map_page_at` weist jede VA `>= GIB1_END` ab. Die harte Grenze
    /// steht bei den Aufrufern ([`gib0_zone`]); hier gilt sie als **Vorliebe**, damit auch die
    /// Stellen tief bleiben, deren Bedingung noch nicht einzeln nachgewiesen ist.
    ///
    /// **Das Kriterium ist die Identitaet, nicht „eine PD sieht es".** Seit E-Rest 3d wird die
    /// private Region einer isolierten PD ueber ein **Fenster** abgebildet
    /// (`vspace_map_user_window`) — eine PD sieht sie, und sie darf trotzdem ueberall liegen.
    IdentityMapped,
    /// **Keine Identitaetsbindung** — bevorzugt hoch, s. [`mem_alloc_anywhere`]. Reiner
    /// Kernel-Speicher gehoert hierher, seit E-Rest 3d aber auch die fensterabgebildeten
    /// Regionen isolierter PDs.
    Anywhere,
}

/// Die gemeinsame Mechanik hinter [`mem_alloc`] und [`mem_alloc_anywhere`].
///
/// Beide Zonen sind **Vorlieben mit Ausweich**, keine Bedingungen: wer eine echte Bedingung hat,
/// nimmt [`mem_alloc_below`] und bekommt `None` statt einer unbrauchbaren Adresse. Hier dagegen
/// ist „woanders" besser als „gar nicht" — ein Kernel, der wegen einer Vorliebe keinen Stack mehr
/// bekommt, waere schlechter dran als einer, der ihn tief nimmt.
fn zoned_alloc(size: u64, align: u64, zone: Zone) -> Option<MemoryCap> {
    // Trichter fuer die Sperre des Mangel-Sweeps -- s. dort.
    #[cfg(feature = "selftest")]
    if sperre_greift(size) {
        return None;
    }
    let grenze = hal::mmu::LOW_MAPPED_END;
    // **Architekturen ohne die Zweiteilung.** Auf aarch64 ist `LOW_MAPPED_END` bewusst
    // `u64::MAX` -- es gibt dort keinen Bereich oberhalb einer Kartengrenze. Ohne diese Zeile
    // waere `Zone::Anywhere` das LEERE Intervall `[MAX, MAX)`: die Suche schluege immer fehl, jede
    // Allokation liefe ueber den Ausweich, und der Zaehler meldete lauter Fehlschlaege fuer eine
    // Vorliebe, die auf dieser Architektur gar nicht existiert.
    let (lo, hi) = if grenze == 0 || grenze == u64::MAX {
        (0, u64::MAX)
    } else {
        match zone {
            Zone::IdentityMapped => (0, grenze),
            Zone::Anywhere => (grenze, u64::MAX),
        }
    };
    // **EIN Lock, nicht zwei.** `match MEM.lock() { .. }` haelt den Guard bis zum Ende des
    // `match` -- ein zweites `MEM.lock()` im `None`-Zweig ist ein Selbst-Deadlock auf einem
    // Spinlock, und zwar genau im Ausweichpfad, der selten laeuft. Selbst gebaut und gemessen:
    // die Lade-Suite blieb stehen, sobald der untere Bereich einmal nicht reichte.
    let cap = {
        let mut mem = MEM.lock();
        match mem.alloc_in(size, align, lo, hi) {
            Some(c) => Some(c),
            // Ausweich in die andere Zone. Er wird GEZAEHLT, und zwar nach WIRKUNG: eine
            // Anforderung, die nirgends passt, ist kein Ausweich. Die erste Fassung zaehlte
            // bedingungslos und meldete `1x` auf einer 512-MiB-Maschine, auf der es oberhalb
            // 4 GiB ueberhaupt keinen Speicher gibt -- gezaehlt hatte sie eine absichtlich
            // uebergrosse Anforderung aus dem Farbtest. Dieselbe Verwechslung wie `rx_used`
            // gegen „Daten sind angekommen".
            None => match mem.alloc(size, align) {
                Some(c) => {
                    let ausgewichen = match zone {
                        Zone::IdentityMapped => c.base() >= grenze,
                        Zone::Anywhere => c.base() < grenze,
                    };
                    if ausgewichen {
                        ZONE_MISSED[zone as usize].fetch_add(1, Ordering::Relaxed);
                    }
                    Some(c)
                }
                None => None,
            },
        }
    }?;
    zero_phys(cap.base(), cap.len());
    Some(cap)
}

/// Wie oft eine Zonenvorliebe nicht erfuellt werden konnte und in die andere Zone ausgewichen
/// wurde — je Zone getrennt (E-Rest 3b/3d). `0` heisst **nicht** „geht nicht", sondern „kam nicht
/// vor"; die Zeile im Bericht sagt das ausdruecklich.
///
/// Zwei Zahlen, weil es zwei verschiedene Lagen sind: `IdentityMapped` ausgewichen heisst „GiB 0 ist
/// voll, und die Region liegt jetzt dort, wo eine PD sie NICHT sehen kann" — das ist ein Befund.
/// `Anywhere` ausgewichen heisst nur „oben war nichts frei" und ist harmlos.
static ZONE_MISSED: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// Zaehlerstaende fuer den Bericht: `(IdentityMapped ausgewichen, Anywhere ausgewichen)`.
pub fn zone_misses() -> (u32, u32) {
    (
        ZONE_MISSED[Zone::IdentityMapped as usize].load(Ordering::Relaxed),
        ZONE_MISSED[Zone::Anywhere as usize].load(Ordering::Relaxed),
    )
}

/// **Die Zone, aus der alles kommen muss, was eine PD-eigene Abbildung braucht** (E-Rest 3b).
///
/// `vspace_map_region`/`vspace_map_block` bilden nur in GiB 1 des PD-eigenen Adressraums ab;
/// GiB 1..3 hängen an geteilten statischen Tabellen (`ISO_PD_HIGH`, s. CLAUDE.md). Eine private
/// Region **muss** deshalb in `[USER_RAM_MIN, GIB1_END)` liegen. Das ist keine Vorliebe, die man
/// nachträglich prüfen kann, sondern eine Bedingung, die in die Anforderung gehört.
fn gib0_zone() -> (u64, u64) {
    (hal::mmu::USER_RAM_MIN, hal::mmu::GIB1_END)
}

/// [`mem_alloc`] mit Zonenwunsch: die Region liegt vollständig unterhalb von `limit`.
///
/// Der Unterschied zum alten Weg („anfordern, Grenze prüfen, bei Verfehlung aufgeben") ist,
/// dass hier **gesucht** statt geraten wird. Wer nur einmal fragt und die Antwort verwirft,
/// verlässt sich stillschweigend auf die Belegungsordnung des Allokators — und die ist keine
/// Zusicherung, sondern eine Nebenwirkung von Best-Fit über einen zufälligen Speicherplan.
fn mem_alloc_below(size: u64, align: u64, limit: u64) -> Option<MemoryCap> {
    // Trichter fuer die Sperre des Mangel-Sweeps -- s. dort.
    #[cfg(feature = "selftest")]
    if sperre_greift(size) {
        return None;
    }
    let cap = MEM.lock().alloc_below(size, align, limit)?;
    zero_phys(cap.base(), cap.len());
    Some(cap)
}

/// [`mem_alloc`] oder [`alloc_colored`], je nachdem ob ein Farbsatz vorgegeben ist. Beide
/// nullen; der Unterschied ist ausschliesslich die Farbbedingung. Mit Knotenwunsch laeuft
/// die Anforderung durch [`mem_alloc_masked_auf`].
fn mem_alloc_masked(
    size: u64,
    align: u64,
    mask: Option<caprock_mem::ColorMask>,
    node: caprock_hal::numa::Node,
) -> Option<MemoryCap> {
    mem_alloc_masked_auf(size, align, mask, node)
}

/// **Wie [`mem_alloc_masked`], aber mit KNOTENWUNSCH** (Z8/N2, 2026-08-20).
///
/// Der Weg, auf dem die Farbachse in die Platzierungsleiter kommt: mit Maske **und** Knoten laeuft
/// die Anforderung durch `numa::alloc_on_node_colored` und damit durch die drei Sprossen
/// `Exact -> OffNode -> OffNodeUncolored`. Ohne einen von beiden bleibt es beim alten Verhalten --
/// bitgleich, denn `Node::Unaffiliated` nimmt die Leiter gar nicht erst.
///
/// **Der Grund, warum das ueberhaupt hier steht und nicht als freie Funktion daneben:** eine
/// Leiter ohne Aufrufer ist die Falle, die Z26/A3 schon bezahlt hat -- zwei Mutationen belegten,
/// dass die FUNKTIONEN richtig sind, waehrend sie niemand rief. `mem_alloc_masked` ist die Stelle,
/// an der im Kernel „gefaerbt oder nicht" entschieden wird; wer den Knoten dazunimmt, nimmt ihn
/// hier dazu.
fn mem_alloc_masked_auf(
    size: u64,
    align: u64,
    mask: Option<caprock_mem::ColorMask>,
    node: caprock_hal::numa::Node,
) -> Option<MemoryCap> {
    match (mask, node) {
        (Some(m), caprock_hal::numa::Node::At(_)) => {
            crate::numa::alloc_on_node_colored(size, align, node, m)
        }
        (Some(m), _) => alloc_colored(size, align, m),
        (None, caprock_hal::numa::Node::At(_)) => crate::numa::alloc_on_node(size, align, node),
        (None, _) => mem_alloc(size, align),
    }
}

pub fn alloc(size: u64, align: u64) -> Option<MemoryCap> {
    mem_alloc(size, align)
}
/// Wie [`alloc`], aber fuer Speicher, der **nie** in den Adressraum einer PD abgebildet und nie
/// einem Geraet gezeigt wird (E-Rest 3d) — er liegt bevorzugt oberhalb 4 GiB und laesst GiB 0
/// denen, die es brauchen. Wer sich nicht sicher ist, nimmt [`alloc`]: die Vorgabe ist die
/// vorsichtige.
/// **Allocate strictly inside `[lo, hi)`** — the window form the NUMA placement ladder needs (N2).
///
/// A node is a *set of address ranges*, and the allocator already takes a window
/// (`PhysAllocator::alloc_in`). So "allocate on node n" needs no second allocator and no second
/// free list: it is the existing best-fit search restricted to that node's ranges, which keeps
/// colour, zone and node in **one** decision instead of three layers arguing with each other.
///
/// Unlike [`alloc_anywhere`] this is a **condition, not a preference**: it returns `None` rather
/// than something outside the window. The caller decides whether to descend the ladder.
pub fn alloc_in_window(size: u64, align: u64, lo: u64, hi: u64) -> Option<MemoryCap> {
    let cap = { MEM.lock().alloc_in(size, align, lo, hi) }?;
    zero_phys(cap.base(), cap.len());
    Some(cap)
}

pub fn alloc_anywhere(size: u64, align: u64) -> Option<MemoryCap> {
    mem_alloc_anywhere(size, align)
}
/// Wie [`alloc`], aber jede Seite trägt eine Farbe aus `mask` (todo A1). Genullt wie jede
/// Region, die an ein Subjekt gehen kann.
/// **Gefaerbt UND in einem Physfenster** (Z8/N2, die dritte Sprosse der Platzierungsleiter).
///
/// Die Farbachse fehlte der Leiter bis zum 2026-08-20: `alloc_on_node` kannte nur *Knoten* und
/// *irgendwo*, `off_node_uncolored` war damit strukturell `0` — und „Farbe gibt zuletzt nach" eine
/// Entscheidung auf Papier. **Ein Zaehler, der nie ueber 0 gehen kann, ist von einem, der nie
/// ausloest, nicht zu unterscheiden.**
pub fn alloc_colored_in_window(
    size: u64,
    align: u64,
    mask: caprock_mem::ColorMask,
    lo: u64,
    hi: u64,
) -> Option<MemoryCap> {
    MEM.lock()
        .alloc_colored_in(size, align, crate::colors::count(), mask, lo, hi)
}

pub fn alloc_colored(size: u64, align: u64, mask: caprock_mem::ColorMask) -> Option<MemoryCap> {
    zoned_alloc_colored(0, size, align, mask, Zone::IdentityMapped)
}

/// Wie [`alloc_colored`], aber die ersten `vorspann` Seiten stehen unter **keiner**
/// Farbbedingung — fuer eine Wachseite, die keine Daten traegt und deshalb kein Cache-Set belegt.
///
/// Der Grund und die Rechnung stehen bei `caprock_mem::Allocator::alloc_colored_vorspann_in`.
/// Kurz: eine Wachseite in den Streifen zu zwingen, hat auf aarch64 jede gefaerbte
/// Stack-Anforderung strukturell unerfuellbar gemacht (5 Seiten Lauf, 4 Farben Streifen).
fn alloc_colored_vorspann(
    vorspann: u64,
    size: u64,
    align: u64,
    mask: caprock_mem::ColorMask,
) -> Option<MemoryCap> {
    zoned_alloc_colored(vorspann, size, align, mask, Zone::IdentityMapped)
}

/// Wie [`alloc_colored`], aber ohne Identitaetsbindung (E-Rest 3d) — bevorzugt oberhalb 4 GiB.
/// Fuer die fensterabgebildete Region einer isolierten PD: die Farbbedingung ist eine Aussage
/// ueber die **Physadresse** und von der virtuellen Lage vollstaendig unberuehrt.
fn alloc_colored_anywhere(
    size: u64,
    align: u64,
    mask: caprock_mem::ColorMask,
) -> Option<MemoryCap> {
    zoned_alloc_colored(0, size, align, mask, Zone::Anywhere)
}

/// Das Gegenstueck zu [`mem_alloc_masked`], nur ohne Identitaetsbindung -- fuer die Segmente und
/// den Stack einer geladenen PD (A1/Z11c) sowie deren Seitentabellen im Ladepfad.
/// **Ohne Maske und ohne Knoten ist es bitgleich der bisherige Pfad**: der ungefaerbte Ladeweg
/// soll sich durch diese Aenderung nicht verschieben.
///
/// Mit Knotenwunsch (Z8/N3) gilt die Leiter aus `numa` (exakt, dann ausserhalb — gezaehlt nach
/// Wirkung) plus dieselbe Rueckfallregel wie am Kstack: was die Leiter nicht erfuellt, faellt
/// BENANNT zurueck (`lader : RUECKFALL`), nie auf einen Fehler aus Topologie. Die Reihenfolge
/// ist auch hier: erst gibt der Knoten nach, dann die Farbe. Ohne Knotenwunsch bleibt die rein
/// gefaerbte Anforderung fail-closed (A1) — die Farblogik ist unangetastet.
/// Wie [`mem_alloc_masked`], aber mit KNOTENWUNSCH (Z8/N3).
fn mem_alloc_masked_anywhere_auf(
    size: u64,
    align: u64,
    mask: Option<caprock_mem::ColorMask>,
    node: caprock_hal::numa::Node,
) -> Option<MemoryCap> {
    match (mask, node) {
        (Some(m), caprock_hal::numa::Node::At(_)) => {
            if let Some(c) = crate::numa::alloc_on_node_colored(size, align, node, m) {
                return Some(c);
            }
            #[cfg(feature = "debug-log")]
            println!(
                "lader   : RUECKFALL knotenlokal-gefaerbt erschoepft -- gefaerbt ohne Knoten"
            );
            if let Some(c) = alloc_colored_anywhere(size, align, m) {
                return Some(c);
            }
            #[cfg(feature = "debug-log")]
            println!(
                "lader   : RUECKFALL gefaerbt erschoepft -- ungefärbt/unaffiliated (Farbe gibt zuletzt nach)"
            );
            mem_alloc_anywhere(size, align)
        }
        (Some(m), _) => alloc_colored_anywhere(size, align, m),
        (None, caprock_hal::numa::Node::At(_)) => crate::numa::alloc_on_node(size, align, node),
        (None, _) => mem_alloc_anywhere(size, align),
    }
}

/// Die gemeinsame Mechanik — dieselbe Zonenwahl und derselbe EINE Lock wie in [`zoned_alloc`].
fn zoned_alloc_colored(
    vorspann: u64,
    size: u64,
    align: u64,
    mask: caprock_mem::ColorMask,
    zone: Zone,
) -> Option<MemoryCap> {
    // Trichter fuer die Sperre des Mangel-Sweeps -- s. dort.
    #[cfg(feature = "selftest")]
    if sperre_greift(size) {
        return None;
    }
    let colors = crate::colors::count();
    let grenze = hal::mmu::LOW_MAPPED_END;
    let (lo, hi) = if grenze == 0 || grenze == u64::MAX {
        (0, u64::MAX)
    } else {
        match zone {
            Zone::IdentityMapped => (0, grenze),
            Zone::Anywhere => (grenze, u64::MAX),
        }
    };
    let cap = {
        let mut mem = MEM.lock();
        match mem.alloc_colored_vorspann_in(vorspann, size, align, colors, mask, lo, hi) {
            Some(c) => Some(c),
            // Gezaehlt wird nur, was in der anderen Zone auch WIRKLICH genommen wurde.
            None => match mem
                .alloc_colored_vorspann_in(vorspann, size, align, colors, mask, 0, u64::MAX)
            {
                Some(c) => {
                    let ausgewichen = match zone {
                        Zone::IdentityMapped => c.base() >= grenze,
                        Zone::Anywhere => c.base() < grenze,
                    };
                    if ausgewichen {
                        ZONE_MISSED[zone as usize].fetch_add(1, Ordering::Relaxed);
                    }
                    Some(c)
                }
                None => None,
            },
        }
    }?;
    zero_phys(cap.base(), cap.len());
    Some(cap)
}
pub fn free(cap: MemoryCap) {
    MEM.lock().free(cap);
}
pub fn total_free() -> u64 {
    MEM.lock().total_free()
}
/// Ist `[base, base+len)` vollständig frei? Präzise Leckprüfung für eine **bestimmte** Region —
/// im Gegensatz zu [`total_free`], das global ist und von der Nebenläufigkeit anderer Kerne
/// mitbewegt wird.
pub fn region_fully_free(base: u64, len: u64) -> bool {
    MEM.lock().fully_free(base, len)
}
pub fn fragments() -> usize {
    MEM.lock().fragments()
}

// --- Capability-Space + PDs (CAPS) ---

/// Anzahl belegter Cap-Slots (Fuzzer-/Leak-Oracle).
#[cfg_attr(not(feature = "kernel-fuzz"), allow(dead_code))] // nur vom Fuzzer benutzt (ADR 0013)
pub fn cap_used_slots() -> usize {
    CAPS.read().cspace.used_slots()
}
/// Anzahl belegter Cap-Objekte (Fuzzer-/Leak-Oracle: kein Objekt ohne lebende Cap).
#[cfg_attr(not(feature = "kernel-fuzz"), allow(dead_code))] // nur vom Fuzzer benutzt (ADR 0013)
pub fn cap_used_objects() -> usize {
    CAPS.read().cspace.used_objects()
}
/// **CDT-/Refcount-Property-Oracle** (Fuzzer): `0` bei Konsistenz, sonst Anomalie-Code
/// (s. `CapSpace::audit_cdt`). Sichert: keine verlorenen Objekte, keine negativen/
/// falschen Refcounts, keine toten CDT-Knoten, keine Ableitung auf fremde Objekte,
/// Baumform.
pub fn cap_audit_cdt() -> u32 {
    // Sperrordnung: CAP_AUDIT vor CAPS (s. dort). Code 8 = die Zählfläche war kürzer als die
    // Objekttabelle; dann konnte das Audit nicht laufen und meldet das, statt „konsistent" zu
    // sagen.
    let mut scratch = CAP_AUDIT.lock();
    let g = CAPS.read();
    // **Code 9: ein CDT-Lauf hat seine Schranke gerissen** (B-5.5).
    //
    // Die Schranke ist `slots.len()` -- ein azyklischer Lauf besucht keinen Slot zweimal. Wird sie
    // erreicht, ist der Baum nicht mehr azyklisch, und `revoke` hat **abgebrochen**: Abkoemmlinge
    // bleiben am Leben, die weg sein sollten. Das ist kein Latenzbefund, sondern ein
    // Autoritaetsbefund.
    //
    // Die Pruefung steht hier und nicht bloss der Zaehler in der Crate -- genau diese Trennung
    // ("die Pruefbarkeit war vorhanden, die Pruefung nicht") hat diese Datei bei A-3.3 schon
    // einmal gekostet.
    if g.cspace.cdt_walk_overruns() != 0 {
        return 9;
    }
    g.cspace.audit_cdt(scratch.as_mut_slice())
}

/// **Hoechststaende der CDT-Laeufe** (B-5.5): `(Abstiegsschritte, Revoke-Loeschungen, Schranke)`.
///
/// Als **Operationszahl**, nicht als Zeit: eine kritische Sektion, deren Laenge man nur in
/// Zyklen ausdruecken kann, sagt auf Blech, unter KVM und unter TCG jeweils etwas anderes. Erst
/// der **Abstand zur Schranke** macht "begrenzte kritische Sektion" zu einer Aussage.
pub fn cap_cdt_peaks() -> (usize, usize, usize) {
    let g = CAPS.read();
    let (walk, limit) = g.cspace.peak_cdt_walk();
    let (ops, _) = g.cspace.peak_revoke_ops();
    (walk, ops, limit)
}

/// **Zyklenabrechnung dieses Kerns** (B-5.1): `(Proben, Zyklen, Ticks, rückwärts, unplausibel,
/// Kernwechsel, Quelle-untrusted)`.
///
/// Die Ablehnungszähler gehören zwingend dazu. Eine 0 bei `Zyklen` hat zwei völlig verschiedene
/// Bedeutungen — „hat nicht gerechnet" und „konnte nicht messen" — und nur die Zähler trennen sie.
/// Ohne sie wäre ein Kernel, in dem die Klammerung **gar nicht** gerufen wird, von einem völlig
/// unbeschäftigten Kernel nicht zu unterscheiden.
pub fn cycle_accounting(core: usize) -> (u64, u64, u64, u32, u32, u32, u32) {
    let g = SCHEDS[core].lock();
    let c = g.cycle_stats();
    (
        c.samples,
        c.consumed,
        g.ticks(),
        c.rejected_backward,
        c.rejected_implausible,
        c.rejected_core,
        c.rejected_source,
    )
}

/// **Summenprüfung des Cap-Budgets** (A-3.4, Abschluss): wie viele belegte Slots gehen auf **kein**
/// PD-Budget, und passen sie in die Kernel-Reserve?
///
/// Liefert `(nicht_budgetiert, reserve, ok)`. `ok == false` heisst: der Kernel selbst hat mehr
/// Slots belegt als die Reserve deckt — dann kann eine PD *innerhalb* ihres Budgets abgewiesen
/// werden, obwohl `budget_allows` nichts zu beanstanden hat. Genau diese Richtung prüfte bisher
/// niemand; die Dimensionierung trug die Zusage allein.
///
/// `nicht_budgetiert == usize::MAX` markiert „die Prüfung konnte nicht laufen" (zu kleine
/// Zählfläche); `ok` ist dann `false`, nicht stillschweigend `true`.
pub fn cap_nonbudget_slots() -> (usize, usize, bool) {
    // Sperrordnung: CAP_SEEN vor CAPS (s. dort).
    let mut scratch = CAP_SEEN.lock();
    let g = CAPS.read();
    match g.nonbudget_slots(scratch.as_mut_slice()) {
        Some(n) => (n, caprock_microkit::CAP_SLOTS_KERNEL_RESERVE, n <= caprock_microkit::CAP_SLOTS_KERNEL_RESERVE),
        None => (usize::MAX, caprock_microkit::CAP_SLOTS_KERNEL_RESERVE, false),
    }
}

pub fn cap_install(cap: MemoryCap) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_memory(cap)
}
pub fn cap_copy(src: CapPtr, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.copy(src, rights)
}
pub fn cap_mint(src: CapPtr, rights: Rights, badge: u64) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.mint(src, rights, badge)
}
pub fn cap_move(src: CapPtr) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.move_cap(src)
}
pub fn cap_inspect(ptr: CapPtr) -> Option<CapInfo> {
    CAPS.read().cspace.inspect(ptr)
}
/// **Der Rückmeldepuffer der Cap-Finalisierung** (A-3.3) — einmal statisch, nicht je Aufruf auf
/// dem Kernelstack.
///
/// Bis hierher legte jeder `cap_delete`/`cap_revoke` ein `Finalized` als **lokale Variable** an:
/// zwei `[_; 128]`-Arrays, rund 4 KiB, plus die 2,2 KiB, die `dma_finalize` daneben nochmals
/// aufmachte. Damit hing die Objekttabelle an der Stackgröße — `NOBJECTS` zu vergrößern (A-3.4,
/// todo C3) hätte den Stack **jedes** Threads mitvergrößert. Genau das nennt todo A3 als
/// Nebenbedingung, und deshalb steht A-3.3 vor A-3.4.
///
/// Die Kapazität kommt aus der Cap-Crate ([`caprock_cap::MAX_FINALIZED`]) statt aus einer
/// zweiten Konstante hier; wächst die Tabelle, wächst der Puffer mit, ohne dass jemand daran
/// denken muss.
struct FinalizeBuf {
    /// `(ep, caller)`-Paare abzubrechender Calls.
    items: Slab<(u32, u64)>,
    /// Finalisierte DMA-Regionen `(phys, len)`.
    dma: Slab<(u64, u64)>,
    /// Ergebnis je Region aus `dma_enforcer().finalize` — abgebaut ja/nein.
    ok: Slab<bool>,
    /// Kratzfläche der Enforcer: je Region der Index des betroffenen Übersetzungskontexts.
    /// Lag bis A-3.3 in **jeder** `finalize`-Implementierung nochmals auf dem Stack.
    ctx_of: Slab<usize>,
    /// **PDs, deren Debug-Halt freizugeben ist** (Z6b). Dieselbe Schranke, derselbe Grund.
    debug: Slab<u16>,
}

/// **Sperrordnung: `FINALIZE` ist die äußerste.** Sie wird vor `CAPS` genommen und erst nach
/// `abort_finalized_replies` (EPS < SCHEDS) und `dma_finalize` wieder freigegeben — die Meldungen
/// liegen ja darin.
///
/// Das führt **keine neue Verklemmungsklasse** ein: `FINALIZE` nimmt ausschliesslich
/// `cap_delete`/`cap_revoke`, und beide nehmen unmittelbar danach ohnehin `CAPS.write()`. Ein
/// Pfad, der sich hier verklemmen könnte, verklemmte sich vorher schon an `CAPS`. Was sich
/// tatsächlich ändert: die beiden Operationen serialisieren jetzt auch gegeneinander — was sie
/// über `CAPS.write()` bereits taten.
static FINALIZE: SpinLock<FinalizeBuf> = SpinLock::new(FinalizeBuf {
    items: Slab::empty(),
    dma: Slab::empty(),
    ok: Slab::empty(),
    ctx_of: Slab::empty(),
    debug: Slab::empty(),
});

/// **Zählfläche des CDT-Audits** (A-3.4) — ein `u32` je Objekt-Eintrag.
///
/// Lag bis hierher als `[0u32; NOBJECTS]` auf dem Stack von `audit_cdt`. Mit einer
/// boot-dimensionierten Objekttabelle geht das nicht mehr: die Größe steht erst zur Laufzeit fest,
/// und ein mitwachsendes Stack-Array wäre genau die Kopplung, die A-3.3 für den
/// Finalisierungspuffer aufgelöst hat.
///
/// **Sperrordnung wie bei [`FINALIZE`]:** wird *vor* `CAPS` genommen. Das führt keine neue
/// Verklemmungsklasse ein — `CAP_AUDIT` nimmt ausschliesslich [`cap_audit_cdt`], und die nimmt
/// unmittelbar danach `CAPS.read()`.
static CAP_AUDIT: SpinLock<Slab<u32>> = SpinLock::new(Slab::empty());

/// **Zählfläche der Summenprüfung** (A-3.4, Abschluss) — ein Markierbit je Cap-**Slot**.
///
/// Eigene Fläche statt Mitbenutzung von [`CAP_AUDIT`]: die dortige ist je *Objekt* dimensioniert
/// (heute gleich gross, aber aus einem anderen Grund) und wird von `audit_cdt` genullt. Zwei
/// Prüfungen, die sich einen Puffer teilen, laufen irgendwann ineinander — und dann meldet die
/// eine ein Ergebnis, das die andere überschrieben hat.
///
/// **Sperrordnung wie bei [`CAP_AUDIT`]:** vor `CAPS`, genommen ausschliesslich von
/// [`cap_nonbudget_slots`].
static CAP_SEEN: SpinLock<Slab<bool>> = SpinLock::new(Slab::empty());

/// Wie oft eine Finalisierungsmeldung mangels Kapazität verworfen wurde. **Muss 0 bleiben.**
static FINALIZE_OVERFLOW: AtomicU32 = AtomicU32::new(0);

/// Zähler der verworfenen Finalisierungsmeldungen (Audit-Code 70 in [`ipc_audit`]).
pub fn finalize_overflow_count() -> u32 {
    FINALIZE_OVERFLOW.load(Ordering::Relaxed)
}

/// Den Überlauf auswerten — **die Prüfung, die es bis A-3.3 nicht gab.**
///
/// `Finalized::overflowed()` existierte mit dem Kommentar „damit «kann nicht vorkommen» prüfbar
/// ist statt behauptet", und gerufen hat sie **niemand**. Solange die Arrays im Typ lagen, war das
/// verschmerzbar; seit der Puffer vom Aufrufer kommt, ist die Kapazität eine Entscheidung und
/// keine Typeigenschaft mehr. Ein Überlauf heisst konkret: ein in `CALL` blockierter Aufrufer wird
/// **nie** entblockt (Liveness-Bug) oder eine DMA-Region bleibt gemappt und belegt.
///
/// Deshalb laut und sofort, nicht nur als Zähler: ein Fehler dieser Klasse ist im Nachhinein an
/// nichts mehr zu erkennen — man sieht nur einen Thread, der steht.
fn note_finalize_overflow(rf: &caprock_cap::Finalized<'_>) {
    if rf.overflowed() {
        FINALIZE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        println!(
            "cap     : FAILURES Finalisierungspuffer uebergelaufen (Kapazitaet {}) -- Meldungen verworfen: blockierte CALL-Aufrufer bleiben stehen",
            CAPS.read().cspace.finalize_capacity()
        );
    }
}

pub fn cap_delete(ptr: CapPtr) -> Result<(), CapError> {
    // Finalisierung gibt ggf. Speicher zurück -> CAPS vor MEM (Sperrordnung). Beim
    // Finalisieren einer Reply-Cap wird der zugehörige Call abgebrochen (NACH dem
    // Freigeben von CAPS/MEM, da das Entblocken EPS<SCHEDS sperrt -> CAPS < EPS).
    let mut buf = FINALIZE.lock();
    let b = &mut *buf;
    let mut rf = caprock_cap::Finalized::mit_debug(
        b.items.as_mut_slice(),
        b.dma.as_mut_slice(),
        b.debug.as_mut_slice(),
    );
    let r = {
        let mut caps = CAPS.write();
        let mut mem = MEM.lock();
        caps.cspace.delete(&mut mem, ptr, &mut rf)
    };
    abort_finalized_replies(&rf);
    release_finalized_debug(&rf);
    dma_finalize(&rf, b.ok.as_mut_slice(), b.ctx_of.as_mut_slice());
    note_finalize_overflow(&rf);
    r
}
pub fn cap_revoke(ptr: CapPtr) -> Result<(), CapError> {
    let mut buf = FINALIZE.lock();
    let b = &mut *buf;
    let mut rf = caprock_cap::Finalized::mit_debug(
        b.items.as_mut_slice(),
        b.dma.as_mut_slice(),
        b.debug.as_mut_slice(),
    );
    let r = {
        let mut caps = CAPS.write();
        let mut mem = MEM.lock();
        caps.cspace.revoke(&mut mem, ptr, &mut rf)
    };
    abort_finalized_replies(&rf);
    release_finalized_debug(&rf);
    dma_finalize(&rf, b.ok.as_mut_slice(), b.ctx_of.as_mut_slice());
    note_finalize_overflow(&rf);
    r
}

/// **Z6b: den Debug-Halt jeder PD freigeben, deren letzte `DebugControl` gerade verschwunden ist.**
///
/// ## Warum das hier steht und nicht in `cap_revoke`
///
/// Nur der Debugger entfernt [`BlockReasons::DEBUG`](caprock_sched::BlockReasons::DEBUG). Faellt
/// seine Autoritaet weg — durch `revoke`, oder weil seine PD stirbt —, traegt ein angehaltener
/// Thread einen Grund, den **niemand** mehr entfernen darf: er laeuft nie wieder. *Ein Revoke, das
/// sein Ziel unbrauchbar macht, ist schlimmer als kein Revoke*, und es ist dieselbe Klasse wie die
/// vier D9-Befunde — ein Wecker, dessen Weckender wegging.
///
/// Die naheliegende Fassung waere, den Grund **in** `cap_revoke` zu entfernen. Sie traegt nicht:
/// `CAPS` laege dabei fuer eine datenabhaengige, unbegrenzte Dauer, und die Kante
/// `CAPS -> SCHEDS[core]` waere neu und muesste gegen jeden Pfad geprueft werden, der andersherum
/// laeuft. Der Platz hier hat beides nicht — `CAPS` ist freigegeben, die Ordnung ist
/// `CAPS < EPS < SCHEDS` wie bei [`abort_finalized_replies`], neben dem diese Funktion **absichtlich
/// steht**: sie ist derselbe Fall, nur mit einem anderen Grundbit.
///
/// ## Und warum NICHT zweimal hingeschrieben
///
/// `cap_delete` und `cap_revoke` sind strukturgleich und rufen beide hierher. Eine sterbende
/// Debugger-PD nimmt den **`cap_delete`**-Weg (je Cap einzeln, s. `pd_teardown`), nicht den
/// Revoke-Weg. Stuende die Regel an beiden Stellen, altert die `cap_delete`-Kopie: der ausdrueckliche
/// Revoke ist der Fall, an den beim Testen jeder zuerst denkt (K1a).
///
/// Gezaehlt wird **nach Wirkung**: `DEBUG_RELEASED` waechst um die Zahl tatsaechlich freigegebener
/// Threads, nicht um die Zahl der Meldungen. „Es gab eine Meldung" und „ein Thread lief wieder los"
/// sind zwei Aussagen — dieselbe Unterscheidung wie `rx_used` gegen „Daten sind angekommen".
fn release_finalized_debug(rf: &caprock_cap::Finalized<'_>) {
    for pd in rf.iter_debug() {
        // **Erst fragen, dann freigeben.** Der Sammler meldet JEDES geloeschte Steuerrecht; ob
        // damit das LETZTE ging, weiss nur ein Blick auf den ganzen Cspace — und den gibt es erst
        // hier, nachdem `CAPS` freigegeben und wieder lesend genommen werden kann. Ohne diese
        // Frage gaebe der Wegfall EINES von zwei Steuerrechten das Ziel frei, waehrend das andere
        // es angehalten haelt: der zweite Debugger laese danach den Frame eines LAUFENDEN Threads
        // und wuesste es nicht.
        if CAPS.read().cspace.any_debug_control_over(pd) {
            continue;
        }
        let pd_of = |tid: ThreadId| pd_of_thread(tid).map(|p| p as u32);
        let mut n = 0usize;
        // Jeder Kern einzeln, je unter seiner eigenen Sperre. Dass dabei kein Thread zwischen zwei
        // besuchten Kernen entwischt, ist **erzwungen** und nicht beobachtet: `detach_for_migration`
        // weist einen Thread mit gesetztem `DEBUG` ab.
        for c in 0..num_cores() {
            if !caprock_sched::core_online(c) {
                continue;
            }
            n += SCHEDS[c].lock().debug_release_pd(&pd_of, pd as u32);
        }
        if n > 0 {
            DEBUG_RELEASED.fetch_add(n as u64, Ordering::Relaxed);
        }
        DEBUG_RELEASE_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Für jede beim Löschen/Revoke finalisierte Reply-Cap den ausstehenden Call abbrechen:
/// den noch wartenden Aufrufer mit `ERR_SERVER_GONE` entblocken (Revocation eines
/// Calls). Läuft OHNE gehaltenen CAPS/MEM-Lock (Ordnung CAPS < EPS < SCHEDS).
fn abort_finalized_replies(rf: &caprock_cap::Finalized<'_>) {
    for (ep, caller_raw) in rf.iter() {
        endpoint_abort_call(ep as usize, ThreadId::from_raw(caller_raw));
    }
}

/// Einen konkreten ausstehenden Call an Endpoint `ep` abbrechen (Reply-Cap-Revocation):
/// ist `caller` noch der wartende Aufrufer, wird er mit `ERR_SERVER_GONE` entblockt.
pub fn endpoint_abort_call(ep: usize, caller: ThreadId) -> bool {
    if ep >= eps().len() {
        return false;
    }
    let orphan = {
        let mut e = eps()[ep].lock();
        e.abort_call(caller)
    };
    if let Some(c) = orphan {
        unblock_with_error(c, caprock_abi::result::ERR_SERVER_GONE);
        true
    } else {
        false
    }
}

/// Eine **Reply-Capability** für den an Endpoint `ep` blockierten `caller` prägen
/// (first-class `ObjectKind::Reply`): die einmalige, revozierbare Autorität, diesen Call
/// abzubrechen. Wird die Cap gelöscht/revoked, wird der Aufrufer mit `ERR_SERVER_GONE`
/// entblockt (s. `cap_delete`/`cap_revoke`).
pub fn reply_cap_for(ep: usize, caller: ThreadId) -> Result<CapPtr, CapError> {
    CAPS.write()
        .cspace
        .install_reply(ep as u32, caller.to_raw(), Rights::WRITE)
}

// --- Test-Sonde: CAPS-Read-Concurrency (Verifikation des Reader-Writer-Locks) ---
//
// Beweist, dass der CAPS-Read-Lock mehrere Leser GLEICHZEITIG zulässt (mit dem alten
// exklusiven SpinLock strukturell unmöglich -> Höchststand stets 1). Zwei Sonden auf
// zwei Kernen synchronisieren sich über eine **zweiphasige Barriere innerhalb des
// gehaltenen Read-Locks**: keiner verlässt den Abschnitt, bevor beide angekommen sind
// (Phase 2), damit auch der Spätere den gemeinsamen Höchststand noch beobachtet.
static CAPLK_IN: AtomicU32 = AtomicU32::new(0); // aktuell gleichzeitig im Read-Abschnitt
static CAPLK_DEP: AtomicU32 = AtomicU32::new(0); // Abmarsch-Barriere (Phase 2)
static CAPLK_MAX: AtomicU32 = AtomicU32::new(0); // beobachteter Höchststand gleichzeitiger Leser

/// Beobachteter Höchststand gleichzeitiger CAPS-Leser (`>=2` beweist Read-Parallelität;
/// mit einem exklusiven Lock wäre er strukturell `1`).
pub fn caps_max_concurrent_readers() -> u32 {
    CAPLK_MAX.load(Ordering::Acquire)
}

/// Test-Sonde: nimmt den CAPS-**Read**-Lock und wartet (begrenzt durch `spin_limit`) an
/// einer Barriere, bis `want` Leser gleichzeitig im Read-Abschnitt sind. Gibt zurück, ob
/// das beobachtet wurde. Mit dem Reader-Writer-Lock kommen beide Sonden gleichzeitig
/// hinein (-> `true`); wäre der Lock exklusiv, blockierte die zweite Sonde -> die Barriere
/// läuft in `spin_limit` und beide melden `false`. Nimmt keinen weiteren Lock -> hält nur
/// den CAPS-Read-Lock (kein Sperrordnungsproblem). Im isolierten Test-Fenster aufzurufen.
pub fn caps_read_concurrency_probe(want: u32, spin_limit: u32) -> bool {
    let _g = CAPS.read();
    CAPLK_IN.fetch_add(1, Ordering::AcqRel);
    // Phase 1: ankommen + warten, bis beide Leser gleichzeitig drin sind.
    let mut seen = false;
    let mut s = 0u32;
    loop {
        let n = CAPLK_IN.load(Ordering::Acquire);
        CAPLK_MAX.fetch_max(n, Ordering::AcqRel);
        if n >= want {
            seen = true;
            break;
        }
        if s >= spin_limit {
            break;
        }
        s += 1;
        core::hint::spin_loop();
    }
    // Phase 2: Abmarsch-Barriere — erst gehen, wenn beide angekommen sind.
    CAPLK_DEP.fetch_add(1, Ordering::AcqRel);
    let mut s2 = 0u32;
    while CAPLK_DEP.load(Ordering::Acquire) < want && s2 < spin_limit {
        s2 += 1;
        core::hint::spin_loop();
    }
    CAPLK_IN.fetch_sub(1, Ordering::AcqRel);
    seen
}

// --- Endpoints / Notifications (per-Objekt-Locks) ---

/// Einen freien Endpoint-Slot reservieren (scannt die per-Endpoint-Locks).
/// **Z23 S1: die Tore einer PD schliessen oder oeffnen.** Rueckgabe: hat sich etwas geaendert?
///
/// Erste Phase der Zwei-Phasen-Stilllegung: was die PD **anfaengt** (`CALL`/`RECV`), wird
/// abgewiesen; was schon laeuft, darf **auslaufen** (`REPLY` bleibt erlaubt). Erst danach ist
/// Einfrieren ueberhaupt sinnvoll -- ein Freeze ohne geschlossene Tore friert eine PD ein, die im
/// naechsten Moment eine neue Transaktion angefangen haette.
pub fn pd_quiesce(pd: usize, an: bool) -> bool {
    CAPS.write().pds.set_quiescing(pd, an)
}

/// Sind die Tore dieser PD zu? (Bericht/Pruefpfad.)
pub fn pd_is_quiescing(pd: usize) -> bool {
    CAPS.read().pds.is_quiescing(pd)
}

/// Wie viele PDs gerade stillgelegt sind -- fuer den Bericht.
pub fn pd_quiescing_count() -> usize {
    CAPS.read().pds.quiescing_count()
}

pub fn create_endpoint() -> Option<usize> {
    for (i, ep) in eps().iter().enumerate() {
        let mut e = ep.lock();
        if !e.is_used() {
            e.mark_used();
            return Some(i);
        }
    }
    None
}
pub fn install_endpoint_cap(ep: u32, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_endpoint(ep, rights)
}
pub fn create_notification() -> Option<usize> {
    for (i, n) in ntfns().iter().enumerate() {
        let mut nt = n.lock();
        if !nt.is_used() {
            nt.mark_used();
            return Some(i);
        }
    }
    None
}
pub fn install_notification_cap(ntfn: u32, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_notification(ntfn, rights)
}

/// Wie [`install_notification_cap`], aber mit einem **Badge** (A-2.1).
///
/// Das Badge einer Notification ist eine Eigenschaft der **Cap**, nicht der Nachricht:
/// `SYS_SIGNAL` verodert das Badge des benutzten Caps in `pending`, das `x2`-Wort des Aufrufs
/// spielt keine Rolle. Wer also unterscheiden will, *wer* signalisiert hat, muss beim Vergeben
/// unterschiedlich badgen — hier beim Endowment, und beim Weiterreichen in `SYS_LOAD`.
///
/// Zurück kommt die **geminzte** Cap; die ungebadgete bleibt als CDT-Eltern beim Kernel (sie zu
/// löschen ginge ohnehin nicht, solange ein Kind existiert, und sie ist die Wurzel, über die eine
/// spätere Rücknahme läuft).
pub fn install_notification_cap_badged(
    ntfn: u32,
    rights: Rights,
    badge: u64,
) -> Result<CapPtr, CapError> {
    let mut g = CAPS.write();
    let root = g.cspace.install_notification(ntfn, rights)?;
    g.cspace.mint(root, rights, badge)
}

// --- Threads / Protection Domains ---

/// Einen Thread mit Priorität `prio` auf dem aktuellen Kern erzeugen (Stack aus
/// dem Allokator). Lock-Ordnung RES vor SCHEDS[core].
pub fn spawn(entry: usize, arg: usize, prio: u8) -> Option<ThreadId> {
    spawn_on_core(hal::cpu::core_id(), entry, arg, prio)
}

/// Den Kern mit der **geringsten Last** wählen (Anzahl belegter TCB-Slots). Sperrt
/// jede Scheduler-Instanz einzeln/kurz (nie zwei gleichzeitig).
pub fn least_loaded_core() -> usize {
    // **Lock-frei** (ext-30): die Last jedes Kerns steht in einem Atomic, das der jeweilige
    // Scheduler pflegt. Vorher sperrte diese Funktion JEDEN Kern-Scheduler nacheinander —
    // bei 256 Kernen 256 Lock-Zyklen je Spawn, und dabei stand die Einplanung aller Kerne.
    // **Nur Kerne, die sich gemeldet haben** (2026-08-17, Z6 Stufe 1). `num_cores()` ist die
    // dimensionierte Kernzahl, nicht die laufende: ein unterdruecktes Geschwister hat einen
    // Scheduler-Slot, aber keinen Idle-Thread. Sein `CORE_LOAD` steht bei 0 und gewann damit
    // JEDE Platzierung -- gemessen als Kernel-Panic „kein laufender Thread (gefragter Kern 1)"
    // unter `-smp cores=2,threads=2`.
    let mut best = hal::cpu::core_id();
    let mut best_load = usize::MAX;
    for c in 0..num_cores() {
        if !caprock_sched::core_online(c) {
            continue;
        }
        let load = caprock_sched::core_load(c);
        if load < best_load {
            best_load = load;
            best = c;
        }
    }
    // Der Rueckfall ist der EIGENE Kern, nicht Kern 0: der Aufrufer laeuft nachweislich, Kern 0
    // muss es nicht. Erreichbar ist der Zweig nur, bevor der erste Kern `init_core` durchlaufen
    // hat -- dann ist „hier" die einzige belegbare Antwort.
    best
}

/// **N3: den am wenigsten belasteten Kern DES KNOTENS `node`.**
///
/// Falls dieser Knoten keinen laufenden Kern hat, faellt es auf [`least_loaded_core`] zurueck —
/// und **genau dieser Fall wird gezaehlt**, nicht der Versuch. „Auf dem Knoten war kein Kern" und
/// „der Thread landete auf einem fremden Knoten" sind zwei Aussagen; nur die zweite ist eine
/// Tatsache ueber die Platzierung (E-Rest 3b, wortwoertlich).
///
/// Der Knoten ist eine **eigene benannte Groesse** und wird nicht in den Lastzaehler geschmuggelt.
/// Diese Woche hat genau die Verwechslung schon einmal gekostet: `CORE_LOAD` bedeutete nebenbei
/// „lebt dieser Kern", und ein nie gestarteter Kern gewann damit jede Platzierung.
pub fn least_loaded_core_on(node: caprock_hal::numa::Node) -> usize {
    let caprock_hal::numa::Node::At(n) = node else {
        return least_loaded_core();
    };
    let mut best: Option<(usize, usize)> = None;
    for c in 0..num_cores() {
        if !caprock_sched::core_online(c) {
            continue;
        }
        if crate::numa::node_of_core(c) != caprock_hal::numa::Node::At(n) {
            continue;
        }
        let load = caprock_sched::core_load(c);
        if best.map_or(true, |(_, bl)| load < bl) {
            best = Some((c, load));
        }
    }
    match best {
        Some((c, _)) => {
            crate::numa::note_core_placement(true);
            c
        }
        None => {
            crate::numa::note_core_placement(false);
            least_loaded_core()
        }
    }
}

// --- Thread-Migration (ext-30) -----------------------------------------------------------

/// Zähler erfolgreicher Migrationen (Telemetrie/Tests).
static MIGRATIONS: AtomicUsize = AtomicUsize::new(0);
/// Ist der **automatische** periodische Lastausgleich aktiv? Default **aus**: die
/// Migrationsmechanik ist vollständig und getestet, aber die Demo-/Testthreads dieses
/// Images setzen teils feste Kern-Affinität voraus (Cross-Core-IPC-Test, Budget-Donation,
/// Platzierungs-Telemetrie). Der Schalter trennt „Mechanismus fertig" von „Policy
/// standardmäßig an" — s. `todo.md` B4.
static BALANCING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Tick-Zähler je Kern für das Ausgleichs-Intervall.
static BALANCE_TICK: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];
/// Nur jeder n-te Tick prüft die Balance (Migration kostet Cache-Lokalität).
const BALANCE_INTERVAL_TICKS: u64 = 16;

/// Automatischen Lastausgleich ein-/ausschalten.
pub fn set_balancing(on: bool) {
    BALANCING.store(on, Ordering::Relaxed);
}

/// Anzahl bisher durchgeführter Thread-Migrationen.
pub fn migration_count() -> usize {
    MIGRATIONS.load(Ordering::Relaxed)
}

/// **Einen Thread des AKTUELLEN Kerns auf `dst` verschieben.**
///
/// Die Migration wird immer vom **abgebenden** Kern ausgeführt (Push, nicht Pull). Das ist
/// keine Bequemlichkeit, sondern notwendig: der Lazy-FP-Kontext eines Threads liegt
/// physisch in den FP-Registern **seines** Kerns. Nur dieser Kern kann ihn sichern — ein
/// fremder Kern könnte fremde FP-Register gar nicht lesen. Deshalb: FP hier lokal
/// ausspülen, dann übergeben.
///
/// Sperrordnung: beide Scheduler-Locks werden in **aufsteigender Kern-Reihenfolge**
/// genommen (zwei gleichzeitig migrierende Kerne können sich so nicht verklemmen).
///
/// Gibt `false` zurück, wenn der Thread nicht migrierbar ist (läuft gerade, aktive
/// Budget-Donation, fremder Kern) oder der Zielkern keine Kapazität hat.
pub fn migrate_to(tid: ThreadId, dst: usize) -> bool {
    let src = hal::cpu::core_id();
    if dst == src || dst >= num_cores() {
        return false;
    }
    if caprock_sched::owner_core(tid) != Some(src) {
        return false; // nur der besitzende Kern schiebt (s. o.)
    }
    // Lazy-FP: gehören die FP-Register dieses Kerns dem Migranten, MÜSSEN sie jetzt in
    // seinen (gid-indizierten, migrationsstabilen) Kontextpuffer gesichert werden —
    // andernfalls liefe er auf dem Zielkern mit fremdem FP-Zustand weiter.
    if FP_OWNER[src].load(Ordering::Relaxed) == tid.to_raw() {
        hal::fp::save(&mut FP_STATES.lock()[tid.slot()]);
        FP_OWNER[src].store(FP_OWNER_NONE, Ordering::Relaxed);
        // Der laufende Thread ist ab jetzt nicht mehr FP-Owner -> wieder trappen lassen.
        hal::fp::set_el0_trap(true);
    }
    let (lo, hi) = if src < dst { (src, dst) } else { (dst, src) };
    let ok = {
        let mut a = SCHEDS[lo].lock();
        let mut b = SCHEDS[hi].lock();
        let (source, target) = if src == lo {
            (&mut *a, &mut *b)
        } else {
            (&mut *b, &mut *a)
        };
        match source.detach_for_migration(tid) {
            None => false,
            Some(m) => match target.attach_migrated(m) {
                Ok(()) => true,
                // Zielkern voll -> zurück zum Quellkern (kein Thread geht verloren).
                Err(m) => {
                    let _ = source.attach_migrated(m);
                    false
                }
            },
        }
    }; // beide Locks freigegeben
    if ok {
        MIGRATIONS.fetch_add(1, Ordering::Relaxed);
        kick(dst); // Zielkern soll den Zugang zeitnah einplanen
    }
    ok
}

/// **Periodischer Lastausgleich** (ext-30): der aufrufende Kern gibt einen bereiten Thread
/// an einen deutlich weniger belasteten Kern ab.
///
/// Aus dem Tick-Pfad aufzurufen, aber bewusst **nicht bei jedem Tick** (der Aufrufer
/// entscheidet über das Intervall): Migration kostet Cache-Lokalität, deshalb erst ab einer
/// spürbaren Differenz (`IMBALANCE_THRESHOLD`) und höchstens ein Thread je Aufruf — das
/// dämpft Oszillation (zwei Kerne, die sich Threads gegenseitig zuschieben).
const IMBALANCE_THRESHOLD: usize = 2;

pub fn balance_once() -> bool {
    let src = hal::cpu::core_id();
    let my_load = caprock_sched::core_load(src);
    let dst = least_loaded_core();
    if dst == src || my_load < caprock_sched::core_load(dst) + IMBALANCE_THRESHOLD {
        return false;
    }
    let Some(cand) = SCHEDS[src].lock().migration_candidate() else {
        return false;
    };
    migrate_to(cand, dst)
}

/// **Lastbewusst** einen Thread erzeugen: auf dem aktuell am wenigsten ausgelasteten
/// Kern. Best-effort (die Last kann sich zwischen Auswahl und spawn ändern). Seit ext-30
/// ist die Platzierung nicht mehr endgültig — [`balance_once`] verschiebt Threads zur
/// Laufzeit nach.
pub fn spawn_balanced(entry: usize, arg: usize, prio: u8) -> Option<ThreadId> {
    spawn_on_core(least_loaded_core(), entry, arg, prio)
}

/// Wie [`spawn`], aber auf einem **bestimmten** Kern `core` (z. B. vom Bootkern aus,
/// um Arbeit über die Kerne zu verteilen). Der Zielkern muss vorab via
/// [`bind_cores`] gebunden sein. Lock-Ordnung RES vor SCHEDS[core].
pub fn spawn_on_core_parked(core: usize, entry: usize, arg: usize, prio: u8) -> Option<Parked> {
    spawn_on_core_parked_mit_stack(core, entry, arg, prio).map(|(p, _, _)| p)
}

/// Wie [`spawn_on_core_parked`], aber **nennt den Stack**, den der Thread bekommen hat.
///
/// Gebraucht von genau einer Stelle (dem Verifiziererthread, C8) und aus einem benannten Grund:
/// seine Stackgrösse ist zu **messen**, nicht zu wählen. Ein Kernel-Thread, der nie stirbt, wird
/// vom Reap-Pfad nie gemessen; ohne seine Basis gäbe es keinen Wasserstand, und „64 KiB reichen"
/// wäre wieder eine Behauptung statt einer Zahl.
///
/// Der Weg über den Scheduler wäre die Alternative gewesen (`Tcb::stack_base` lesen). Er ist
/// bewusst nicht gewählt: der Scheduler hätte einen weiteren Auskunftspfad bekommen, und das Wissen
/// entsteht ohnehin **hier**, wo der Stack alloziert wird.
pub fn spawn_on_core_parked_mit_stack(
    core: usize,
    entry: usize,
    arg: usize,
    prio: u8,
) -> Option<(Parked, usize, usize)> {
    mangel_zuruecksetzen();
    let (base, len) = {
        let stack = benannt_alloc(MANGEL_KERNEL_THREAD_STACK, STACK_SIZE, |n| {
            mem_alloc_anywhere(n, 16)
        })?;
        (stack.base() as usize, stack.len() as usize)
    }; // MEM vor SCHEDS freigegeben (Ordnung MEM < SCHEDS)
    // C4: Wasserstandsmarke fuer die 64-KiB-Klasse, s. `claim_user_kstack_masked`.
    // SAFETY: frisch allozierte, exklusiv gehaltene Region; identity-gemappt.
    unsafe { crate::kstackmark::fuellen(crate::kstackmark::KL_KERN, base, len) };
    let tid = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_parked(core, entry, arg, base, len, prio);
        if let Some(t) = r {
            fp_reset_slot(t.slot()); // frischer FP-Kontext, bevor der Thread laufen kann
        }
        r
    };
    // **Der Stack muss zurueck, wenn kein Thread daraus wird.** Bis heute stand hier ein `?`, und
    // der 64-KiB-Rahmen war weg: gerade an der Kapazitaetsgrenze, wo dieser Zweig laeuft, verliert
    // der Kernel damit bei JEDEM Versuch einen Stack. Gefunden beim Benennen des Mangels, nicht
    // gesucht.
    let Some(tid) = benannt_slot(MANGEL_THREAD_SLOT, tid) else {
        MEM.lock().free_region(PhysRegion::new(base as u64, len as u64));
        return None;
    };
    Some((Parked(tid), base, len))
}

/// **Einen Thread erzeugen, der seine PD schon hat, bevor er das erste Mal laufen darf** (D0).
///
/// # Warum es diese Funktion gibt
///
/// Bis zum 2026-08-07 lautete das Muster ueberall:
///
/// ```text
/// let tid = spawn(..);      // <-- ab HIER lauffaehig
/// bind_pd(pd, tid);         // <-- die Autoritaet kommt erst hier
/// ```
///
/// Dazwischen liegt eine Luecke: eine Speicherbelegung, ein `CAPS.write()`, ein Timer-Tick. Der
/// Thread kann in dieser Luecke anlaufen, seinen ersten Syscall machen und `ERR_NOPD` bekommen --
/// mit einem leeren Cspace ist **jede** Cap unsichtbar, nicht nur eine.
///
/// Das ist keine Theorie: es ist **D0**. Gemessen am 2026-08-07 ueber 50 000 Laeufe, 9 Treffer
/// (0,018 %, einer je 5556). Der IPC-Server der x86-Suite fiel dabei mit `ERR_NOPD` aus seiner
/// `RECV`-Schleife und kam nie zurueck; der Client wartete 61 s auf eine Antwort von einem
/// Empfaenger, den es nicht mehr gab. Die Suite lief dabei ansonsten vollstaendig durch --
/// deshalb hat sie den Fehler 2300 Laeufe lang nicht gezeigt.
///
/// # Warum nicht einfach die Reihenfolge umdrehen
///
/// Weil es keine Reihenfolge GIBT, die traegt: `bind_pd` braucht die `tid`, die `spawn` erst
/// liefert. Die Luecke laesst sich verkleinern, nicht schliessen. Deshalb parkt der Scheduler den
/// Thread jetzt (`spawn_parked`) und laesst ihn erst auf `admit` los -- die PD steht dann schon.
pub fn spawn_in_pd(pd: usize, entry: usize, arg: usize, prio: u8) -> Option<ThreadId> {
    let core = hal::cpu::core_id();
    mangel_zuruecksetzen();
    let (base, len) = {
        let stack = benannt_alloc(MANGEL_KERNEL_THREAD_STACK, STACK_SIZE, |n| {
            mem_alloc_anywhere(n, 16)
        })?;
        (stack.base() as usize, stack.len() as usize)
    }; // MEM vor SCHEDS freigegeben (Ordnung MEM < SCHEDS)
    // C4: Wasserstandsmarke fuer die 64-KiB-Klasse, s. `claim_user_kstack_masked`.
    // SAFETY: frisch allozierte, exklusiv gehaltene Region; identity-gemappt.
    unsafe { crate::kstackmark::fuellen(crate::kstackmark::KL_KERN, base, len) };
    let roh = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_parked(core, entry, arg, base, len, prio);
        if let Some(t) = r {
            fp_reset_slot(t.slot()); // frischer FP-Kontext, bevor der Thread laufen kann
        }
        r
    }; // SCHEDS freigegeben, BEVOR CAPS genommen wird (Ordnung SCHEDS < CAPS gilt hier nicht)
    // Wie in `spawn_on_core_parked`: kein Thread -> der Stack geht zurueck.
    let Some(tid) = benannt_slot(MANGEL_THREAD_SLOT, roh) else {
        MEM.lock().free_region(PhysRegion::new(base as u64, len as u64));
        return None;
    };
    bind_pd(pd, tid);
    // Erst jetzt darf er laufen. Schlaegt das Zulassen fehl, ist die `tid` nicht mehr auflösbar --
    // dann ist der Thread ohnehin weg, und ein stiller `true` waere eine Luege.
    if !SCHEDS[core].lock().admit(tid) {
        return None;
    }
    Some(tid)
}

/// Einen **EL0-User-Thread** erzeugen: Kernel-Stack aus dem EL1-only Pool (per
/// Free-List, beim Thread-Ende zurückgegeben), User-Stack aus dem EL0-zugänglichen
/// RAM. Der Thread läuft auf EL0 und kann nur per Syscall mit dem Kernel interagieren.
/// **Erreicht ein Gerät diese Region?** (K1a)
///
/// Eine `ObjectKind::Memory`-Cap ist per Variante keine DMA-Region — aber sie kann eine
/// **physisch überlappen**, und dann schreibt das Gerät trotzdem hinein. Geprüft wird deshalb die
/// Lage, nicht der Typ.
///
/// Läuft über `for_each_dma` (denselben Weg, den der DMA-Audit nimmt) — *ein Prüfer, der die
/// Grösse nachrechnet statt sie zu lesen, prüft eine zweite Wirklichkeit.*
fn dma_region_overlaps(base: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let end = base.saturating_add(len);
    let mut treffer = false;
    CAPS.read().cspace.for_each_dma(&mut |b, l| {
        if base < b.saturating_add(l) && b < end {
            treffer = true;
        }
    });
    treffer
}

/// **Überlappt die Region eine bestehende EL0-Abbildung dieser PD?** (K1a)
///
/// **Bewusst konservativ, und das steht hier statt in einem Commit-Text:** geprüft werden die
/// beim Kernel gebuchten User-Regionen der Threads dieser PD (`KSTACKS.ubase_of`/`ulen_of`) —
/// also genau die Stapel, die ein zweiter `SPAWN` treffen könnte. **Nicht** geprüft wird die
/// vollständige VSpace der PD (Segmente eines geladenen Programms, `SYS_MAP`-Abbildungen); dafür
/// bräuchte es einen Seitentabellenlauf.
///
/// Die Richtung des Irrtums ist die verzeihliche: eine Überlappung mit einem **fremden Segment**
/// bliebe unerkannt. Das ist innerhalb *einer* PD dieselbe Vertrauenszone (der Aufrufer könnte
/// dort ohnehin hineinschreiben) — es wird zum Loch, sobald `SPAWN` je eine PD-Grenze
/// überschreitet, und dann gehört hier ein VSpace-Lauf hin.
fn pd_mapping_overlaps(pd: usize, base: u64, len: u64) -> bool {
    if len == 0 {
        return true; // eine leere Region „passt überall" -- fail-closed statt fail-open
    }
    let end = base.saturating_add(len);
    let p = KSTACKS.lock();
    let g = CAPS.read();
    (0..p.ubase_of.len()).any(|slot| {
        let (b, l) = (p.ubase_of[slot], p.ulen_of[slot]);
        if b == 0 || l == 0 {
            return false;
        }
        // Nur Threads DIESER PD: die Region eines fremden Adressraums kollidiert nicht.
        let gehoert_pd = g
            .pds
            .pd_of_thread(caprock_sched::ThreadId::from_raw(slot as u64))
            == Some(pd);
        gehoert_pd && base < b.saturating_add(l) && b < end
    })
}

/// **Die Stack-Cap eines Threads** (K1a) — der Verweis, der `CDELETE` blockiert.
///
/// Der benannte Preis dafür, dass der Stack aus einer Cap des Aufrufers kommt: TCB und CapSpace
/// sind gekoppelt. Ein Verweis, der die Löschung **abweist**, ist zählbar; die Alternative
/// („Löschung tötet den Thread mit") wäre eine Gruppenoperation über `CAPS` **und**
/// `SCHEDS[core]`, also die V4-Klasse mit zwei Sperren und einer Ordnung.
/// Plaetze der Stack-Cap-Tabelle.
///
/// **Eine eigene Zahl, und sie ist fail-closed benannt:** die Thread-Tabellen des Kernels sind
/// `Slab`s und damit erst zur Laufzeit gross; eine statische Tabelle braucht eine Schranke. 1024
/// deckt den `scale`-Test (1024 Threads). Ein Slot darueber bekommt **keinen** Eintrag -- und
/// weil `stack_cap_in_use` dann nichts findet, waere seine Cap loeschbar. Deshalb weist
/// `dispatch_spawn` einen solchen Thread ab, statt ihn ungeschuetzt laufen zu lassen.
/// Ein Eintrag: `(rohe ThreadId, Stack-Cap, Basis, Laenge der TEILREGION)`.
///
/// **Die Region steht seit K1b (2026-08-26) mit dabei, und sie ist nicht Zierrat.** Mit mehreren
/// Stapeln in EINER Cap ist die Ueberlappung zwischen Geschwistern der gefaehrliche Fall, und die
/// Cap allein sagt darueber nichts -- sie ist fuer alle vier dieselbe.
type StackEintrag = (u64, caprock_cap::CapPtr, u64, u64);

/// **Eine per-Thread-Tabelle wie die anderen** (berichtigt 2026-08-26).
///
/// Hier stand `const STACK_CAP_SLOTS: usize = 1024;` mit der Begruendung „deckt den `scale`-Test
/// (1024 Threads)". Sie war **fail-closed** -- ein Slot darueber bekam keinen Eintrag, und
/// `dispatch_spawn` wies den Thread ab, statt ihn ungeschuetzt laufen zu lassen. Genau das ist
/// eingetreten: auf aarch64 liegen die Thread-Slots weit ueber 1024 (Kapazitaet rund 10 000 bei
/// acht Kernen), und **jeder** `SYS_SPAWN` der `arena`-Sonde bekam `ERR_NOSPACE` -- nachdem der
/// Thread bereits zugelassen und sofort wieder getoetet worden war. Auf x86 lief es, weil die
/// Slots dort zufaellig klein blieben: *eine Eigenschaft, die aus einer Groessenrelation folgt
/// statt aus der Struktur, verschwindet beim naechsten Messwert.*
///
/// Deshalb keine zweite Zahl mehr: dimensioniert mit **demselben `total`** wie `KSTACKS.base_of`,
/// `VSPACE_OF` und der Rueckwaerts-Index, Groesse im Boot-Report (`bytes`). Sie haengt
/// **unbedingt** und nicht nur unter `selftest`: sie traegt den `ERR_INUSE`-Schutz und die
/// Geschwister-Ueberlappung, also Sicherheitsaussagen und keine Messmaschinerie.
static STACK_CAP_OF: SpinLock<Slab<Option<StackEintrag>>> = SpinLock::new(Slab::empty());

fn record_stack_cap(
    tid: ThreadId,
    cap: caprock_cap::CapPtr,
    base: u64,
    len: u64,
) -> bool {
    let slot = tid.slot();
    let mut t = STACK_CAP_OF.lock();
    if slot >= t.len() {
        return false; // fail-closed: lieber kein Thread als ein ungeschuetzter
    }
    t[slot] = Some((tid.to_raw(), cap, base, len));
    true
}

/// **Ueberlappt die Teilregion den Stapel eines lebenden Geschwisterthreads?** (K1b)
///
/// Diese Frage gab es bis heute nicht, und sie fehlte an genau der Stelle, an der sie zaehlt:
/// [`pd_mapping_overlaps`] liest `KSTACKS.ubase_of`, und `spawn_with_stack_parked` traegt dort
/// **absichtlich nichts** ein (die Region gehoert der Cap, nicht dem Kernel). Damit war die
/// `Overlaps`-Absage fuer genau die Threads, die `SYS_SPAWN` erzeugt, **strukturell
/// unerreichbar** -- ein Pruefer, der den Fall, gegen den er gebaut ist, nicht sehen kann.
/// Solange eine Cap genau einen Stapel trug, fiel es nicht auf; mit der Teilregion ist es der
/// Hauptfall.
///
/// **Das Fenster, das offen bleibt, und es steht hier statt in einem Commit-Text:** der Eintrag
/// entsteht in [`dispatch_spawn`] nach `admit`. Zwei Threads derselben PD, die auf verschiedenen
/// Kernen gleichzeitig `SYS_SPAWN` rufen, koennen beide vor dem jeweils anderen Eintrag pruefen.
/// Es ist dasselbe Fenster, das der `ERR_INUSE`-Schutz seit K1a hat, und es zu schliessen hiesse,
/// die Region **vor** dem Spawn zu reservieren und bei jedem Fehlschlag wieder freizugeben --
/// eine eigene Buchhaltung mit eigenen Abbruchpfaden. Innerhalb EINER PD ist das dieselbe
/// Vertrauenszone (wer den Nachbarstapel ueberlappen darf, duerfte hineinschreiben); es wird zu
/// einem Loch, sobald `SPAWN` je eine PD-Grenze ueberschreitet.
fn stack_sibling_overlaps(pd: usize, base: u64, len: u64) -> bool {
    if len == 0 {
        return true; // eine leere Region „passt ueberall" -- fail-closed statt fail-open
    }
    let t = STACK_CAP_OF.lock();
    let g = CAPS.read();
    (0..t.len()).any(|i| match &t[i] {
        Some((raw, _, b, l)) => {
            let tid = caprock_sched::ThreadId::from_raw(*raw);
            *l != 0
                && thread_alive(tid)
                && g.pds.pd_of_thread(tid) == Some(pd)
                && base < b.saturating_add(*l)
                && *b < base.saturating_add(len)
        }
        None => false,
    })
}

/// Ist diese Cap der Stack eines **lebenden** Threads?
///
/// Die `ThreadId` wird mitgeprüft, nicht nur der Slot: ein wiederverwendeter Slot mit neuer
/// Generation ist ein **anderer** Thread, und seine Cap dürfte nicht am alten Eintrag hängen
/// bleiben. Das ist dieselbe Falle wie bei den wiederverwendeten Slots in D15.
fn stack_cap_in_use(cap: caprock_cap::CapPtr) -> bool {
    let t = STACK_CAP_OF.lock();
    (0..t.len()).any(|i| match &t[i] {
        Some((raw, c, _, _)) => {
            *c == cap && thread_alive(caprock_sched::ThreadId::from_raw(*raw))
        }
        None => false,
    })
}

/// **Der Rückruf hinter `SYS_SPAWN`** (K1a, 2026-08-17) — die einzige Stelle, an der über einen
/// vorgeschlagenen Stack geurteilt wird.
///
/// Er läuft **ohne gehaltenes `CAPS`** (der Dispatch gibt es vorher frei) und nimmt es selbst,
/// damit die Ordnung `MEM < SCHEDS` erhalten bleibt.
///
/// Die sechs Absagen kommen aus [`caprock_cap::spawncheck::check_stack`] und werden hier **nur
/// übersetzt**, nicht neu entschieden. Jede hat ihren eigenen ABI-Code: eine Sammelabsage machte
/// „Stack zu klein" und „ein Gerät kann den Stack schreiben" ununterscheidbar, und das zweite ist
/// ein Angriff.
fn dispatch_spawn(
    pd: usize,
    cap: caprock_cap::CapPtr,
    entry: usize,
    arg: usize,
    prio: u8,
    sub: u64,
) -> Result<u64, u64> {
    use caprock_cap::spawncheck::{check_stack, sub_region, StackProposal, StackRefusal};
    // Alles, was aus der Cap und der PD-Tabelle kommt, unter EINEM Lesezugriff -- sonst könnte
    // sich die Zahl zwischen zwei Blicken ändern, und die Schranke wäre eine über einem
    // veralteten Stand.
    let (is_memory, writable, base, len, threads_now) = {
        let g = CAPS.read();
        let threads_now = g.pds.thread_count(pd);
        match g.cspace.lookup(cap) {
            Some((caprock_cap::ObjectKind::Memory(r), rights, _)) => (
                true,
                rights.contains(caprock_mem::Rights::RW),
                r.base,
                r.len,
                threads_now,
            ),
            _ => (false, false, 0, 0, threads_now),
        }
    };
    // **K1b: erst das Fenster, dann die Fragen.** Beide Kernelfragen unten (erreicht ein Geraet
    // die Region? ueberlappt sie einen Nachbarstapel?) gelten der TEILREGION -- wer sie an der
    // ganzen Cap stellte, bekaeme fuer vier Geschwister in einer Arena viermal dieselbe Antwort.
    // `SubRegion` ist ausserhalb von `sub_region` nicht herstellbar, deshalb kann das Narrowing
    // hier nicht uebersprungen werden.
    let (off_pages, len_pages) = caprock_abi::spawn_sub_unpack(sub);
    let region = match sub_region(base, len, off_pages, len_pages) {
        Ok(r) => r,
        // Der einzige Ausgang von `sub_region`, und er hat seinen eigenen ABI-Code: „du hast eine
        // Region genannt, die du nicht haeltst" ist etwas anderes als „diese Region taugt nicht
        // als Stapel".
        Err(_) => return Err(caprock_abi::result::ERR_SUBREGION),
    };
    let p = StackProposal {
        is_memory,
        writable,
        // Eine `ObjectKind::Memory`-Cap IST der normale Raum: Geräte- und DMA-Regionen sind
        // eigene Varianten (`Mmio`, `Dma`) und wären oben schon durchgefallen. Der Wert steht
        // trotzdem im Vorschlag, damit die Prüfung vollständig **lesbar** ist und nicht davon
        // abhängt, dass jemand die Variantenliste im Kopf hat.
        normal_space: is_memory,
        region,
        // **Die Frage, die nur der Kernel beantworten kann.** Ein Stack, den ein Gerät schreiben
        // kann, ist die `by ops`-Platzierungsregel als Angriff -- die Rücksprungadresse ist Daten.
        device_reachable: dma_region_overlaps(region.base(), region.len()),
        // Ebenso: nur der Kernel kennt die Abbildungen der Ziel-VSpace. **Zwei Quellen, ODER
        // verknuepft** (K1b): `pd_mapping_overlaps` sieht die vom Kernel gebuchten User-Regionen,
        // `stack_sibling_overlaps` die Stapel, die aus Caps kommen und dort absichtlich NICHT
        // gebucht sind. Ohne die zweite waere die Absage fuer genau die Threads unerreichbar, die
        // dieser Syscall erzeugt.
        overlaps_existing: pd_mapping_overlaps(pd, region.base(), region.len())
            || stack_sibling_overlaps(pd, region.base(), region.len()),
        threads_now,
    };
    let stack_top = check_stack(&p).map_err(|r| match r {
        StackRefusal::NotMemory => caprock_abi::result::ERR_BADCAP,
        StackRefusal::NotWritable | StackRefusal::WrongSpace => caprock_abi::result::ERR_RIGHTS,
        StackRefusal::DeviceReachable => caprock_abi::result::ERR_DMA_REACHABLE,
        StackRefusal::BadGeometry => caprock_abi::result::ERR_BADSTACK,
        StackRefusal::Overlaps => caprock_abi::result::ERR_NOSPACE,
        StackRefusal::ThreadLimit => caprock_abi::result::ERR_THREAD_LIMIT,
        StackRefusal::OutsideCap => caprock_abi::result::ERR_SUBREGION,
    })?;
    // **D0 wörtlich: parken -- binden -- zulassen.** Ein Thread, der lauffähig ist, bevor er seine
    // PD hat, macht seinen ersten Syscall mit LEEREM Cspace. Das hat zehn Tage gekostet, und die
    // Rate war 0,018 %.
    let parked = spawn_with_stack_parked(entry, arg, region.base(), stack_top, prio)
        .ok_or(caprock_abi::result::ERR_NOSPACE)?;
    bind_pd_parked(&parked, pd);
    // Die Stack-Cap am TCB vermerken, BEVOR der Thread laufen darf: danach ist sie gegen
    // `CDELETE` gesperrt (`ERR_INUSE`). Andersherum gäbe es ein Fenster, in dem der Thread läuft
    // und seine Cap löschbar wäre -- und ihre Finalisierung gibt den Speicher an den Allokator
    // zurück, unter den Füßen eines laufenden Stapels.
    let tid = admit(parked).ok_or(caprock_abi::result::ERR_NOSPACE)?;
    if !record_stack_cap(tid, cap, region.base(), region.len()) {
        // **Fail-closed.** Ohne Eintrag ist die Stack-Cap loeschbar, waehrend der Thread auf ihr
        // laeuft -- die Finalisierung gaebe den Speicher an den Allokator zurueck, unter den
        // Fuessen eines laufenden Stapels. Lieber kein Thread als ein ungeschuetzter.
        kill_local(tid);
        return Err(caprock_abi::result::ERR_NOSPACE);
    }
    Ok(tid.to_raw())
}

// ================================================================================================
// FORK/EXEC-Dispatch (Prozessmodell, Phase 1) — die Kernel-Rueckrufe zu 31/32
// ================================================================================================
//
// `caprock-microkit` (fremd, nur gelesen) dekodiert und prueft vor (`proc::dekodiere_fork/exec`,
// fail-closed), ohne einen einzigen Seiteneffekt — was hier steht, ist der Vollzug. Verdrahtet
// ist er VOR dem Microkit-Dispatch (`forkexec_syscall` in `fn syscall`): die 31/32-Arme dort
// bleiben fail-closed `ERR_BADSYS` („Antrag ok, Pfad fehlt") als zweite Wahrheit daneben, und
// genau deshalb liegt der Pfad hier und nicht dort — ein Rucckruf, den der Dispatch naehme,
// braeuchte seine Signatur (fremde Datei).
//
// Sperrordnung ueberall: CAPS/MEM/SCHEDS werden nie verschachtelt gehalten, sondern je Schritt
// kurz genommen (Muster `dispatch_spawn`). Der Aufrufer (`forkexec_syscall`) haelt nichts.

/// Teardown-Epoche je PD fuer das EXEC-Token (monoton je PD, Start 0).
///
/// 10 000 Eintraege = 40 KiB BSS; die Laenge folgt `NPDS` aus microkit (nicht einer zweiten
/// Zahl, die daneben altern wuerde). Wer mit dem Token von gestern kommt, bekommt keinen halb
/// geraeumten Zustand, sondern `ERR_STALE_TOKEN` — ein Ueberrest (alter Thread, alte Cap,
/// altes Mapping) ist ein Baufehler, keine Lage.
static EXEC_EPOCHE: SpinLock<[u32; caprock_microkit::NPDS]> =
    SpinLock::new([0; caprock_microkit::NPDS]);
// Sperr-Rang (Audit 2026-09-10, §1-Nachtrag): Blattlock -- alle Takes standalone
// (lesen in `dispatch_exec`, heben nach Erfolg), nie verschachtelt gehalten.

/// FORK/EXEC-Bilanz (nach Wirkung, nicht nach Aufruf): `(fork_gelungen, fork_abgewiesen,
/// exec_gelungen, exec_abgewiesen)`. Gezaehlt wird in `forkexec_syscall`, einmal je Ausgang.
static FORK_GELUNGEN: AtomicU64 = AtomicU64::new(0);
static FORK_ABGEWIESEN: AtomicU64 = AtomicU64::new(0);
static EXEC_GELUNGEN: AtomicU64 = AtomicU64::new(0);
static EXEC_ABGEWIESEN: AtomicU64 = AtomicU64::new(0);

/// Die FORK/EXEC-Bilanz fuer den Bericht: `(fork_gelungen, fork_abgewiesen, exec_gelungen,
/// exec_abgewiesen)`. Vier Zahlen, weil „lief" und „lief nicht" zwei Aussagen sind — und weil
/// ein Zaehler, der nie ueber 0 gehen kann, von einem, der nie ausloest, nicht zu unterscheiden
/// ist (N2-Lehre).
pub fn forkexec_bilanz() -> (u64, u64, u64, u64) {
    (
        FORK_GELUNGEN.load(Ordering::Relaxed),
        FORK_ABGEWIESEN.load(Ordering::Relaxed),
        EXEC_GELUNGEN.load(Ordering::Relaxed),
        EXEC_ABGEWIESEN.load(Ordering::Relaxed),
    )
}

/// FORK-Kratzflaeche (BSS, ~5,7 KiB): die sechs Segmentlisten von `dispatch_fork`
/// (`quelle`, `vas`, `plan`, `kind_segs`, `kind_vas`, `kind_perms`, je `MAX_IMG_SEGS`
/// Eintraege).
///
/// Weder Heap noch Stack — beides ist hier kein Fix, sondern der Fehler: der Kernel hat
/// KEINEN Heap (`main.rs`: `NoGlobalHeap`, jeder `alloc`-Versuch scheitert per Design —
/// sechs `Vec`s mit `try_reserve` schlugen also IMMER fehl und der Root-Task startete
/// nie), und ~6 KiB Arrays auf dem Stack sprengten per Inlining die 4-KiB-Kstacks heisser
/// Pfade (#DF-Befund 2026-09-10). Die Kapazitaet folgt `MAX_IMG_SEGS` (keine zweite Zahl
/// daneben); gelesen wird immer nur bis zum frisch geschriebenen Zaehler (`nq`/`nseg`/
/// `nkind`), nie weiter — alte Inhalte sind damit unerreichbar, kein Rueckstand.
///
/// Sperr-Rang: Blattlock (`docs/invariants.md` §1) — IMMER als aeusserster Lock zuerst
/// genommen (`dispatch_fork` nimmt ihn als erste Anweisung, ohne dass eine andere Sperre
/// gehalten wird; CAPS/MEM/SCHEDS/... liegen alle darunter), nie verschachtelt genommen,
/// waehrend eine von ihnen gehalten wird. Eigenes Lock statt eines gemeinsamen mit
/// `LOAD_SCRATCH`: FORK laeuft auf dem Syscall-Kern, LOAD auf dem Verifizierer bzw. via
/// EXEC nebenlaeufig — ein gemeinsames Lock serialisierte beide Pfade ohne Grund. Beide
/// Kratzlocks werden nie gleichzeitig gehalten (`dispatch_exec` nimmt keines der beiden,
/// `dispatch_fork` ruft nie den Ladepfad).
struct ForkScratch {
    quelle: [(u64, u64, u64, u8); MAX_IMG_SEGS],
    vas: [(u64, u64); MAX_IMG_SEGS],
    plan: [caprock_loader::snapshot::SnapSeg; MAX_IMG_SEGS],
    kind_segs: [(u64, u64); MAX_IMG_SEGS],
    kind_vas: [u64; MAX_IMG_SEGS],
    kind_perms: [u8; MAX_IMG_SEGS],
}
static FORK_SCRATCH: SpinLock<ForkScratch> = SpinLock::new(ForkScratch {
    quelle: [(0, 0, 0, 0); MAX_IMG_SEGS],
    vas: [(0, 0); MAX_IMG_SEGS],
    plan: [caprock_loader::snapshot::SnapSeg { va: 0, len: 0 }; MAX_IMG_SEGS],
    kind_segs: [(0, 0); MAX_IMG_SEGS],
    kind_vas: [0; MAX_IMG_SEGS],
    kind_perms: [0; MAX_IMG_SEGS],
});

/// LOAD-Kratzflaeche (BSS, ~1,6 KiB): die drei Stuecklisten von `load_into_pd_mit_va`
/// (`seglist`, `vvas`, `pperm`, je `MAX_IMG_SEGS` Eintraege).
///
/// Derselbe Grund wie bei `FORK_SCRATCH`: kein Heap (der Ladepfad wiese sonst ueber EXEC
/// auf 4-KiB-Aufrufer-Stacks jede Ladung ab), kein Stack (1,6 KiB gehoeren nicht auf einen
/// 4-KiB-Kstack). Gelesen wird nur bis `nrec`. Sperr-Rang: Blattlock
/// (`docs/invariants.md` §1) — wie `FORK_SCRATCH` als aeusserster Lock zuerst genommen
/// (erste Anweisung von `load_into_pd_mit_va`), nie verschachtelt mit CAPS/MEM/SCHEDS,
/// nie gleichzeitig mit `FORK_SCRATCH` gehalten.
struct LoadScratch {
    seglist: [(u64, u64); MAX_IMG_SEGS],
    vvas: [u64; MAX_IMG_SEGS],
    pperm: [u8; MAX_IMG_SEGS],
}
static LOAD_SCRATCH: SpinLock<LoadScratch> = SpinLock::new(LoadScratch {
    seglist: [(0, 0); MAX_IMG_SEGS],
    vvas: [0; MAX_IMG_SEGS],
    pperm: [0; MAX_IMG_SEGS],
});

/// `FORK_SNAPSHOT`: volle Kopie der registrierten Frames der Quell-PD in eine frische Kind-PD.
///
/// Kein COW (s. `caprock-loader::snapshot`-Doku: Dirty-Tracking + nachladender Fault-Pfad
/// fehlen — geteilt ohne beides waere kein Snapshot, sondern ein Geschwister, das die Seite
/// des anderen beschreibt). Das Kind startet am SELBEN Eintritt mit demselben Argument
/// (Neustart vom Eintritt, kein Fortsetzen — beides steht in der Teardown-Buchhaltung, nicht
/// in einer zweiten Tabelle) und wird SOFORT zugelassen: ein geparktes Kind, das niemand
/// starten kann (kein PDCTL/START-Syscall existiert), waere ein Leck, kein Prozess.
///
/// Was kopiert wird, ist die Teardown-Buchhaltung (`loaded_snapshot`): was dort nicht steht,
/// gehoert der PD nicht und wird nicht kopiert. Der Stack bekommt das Kind FRISCH (genullt,
/// an derselben VA): sein Inhalt ist fluechtiger Aufrufzustand, kein Adressraum — ihn zu
/// kopieren hiesse, die Ruecksprungadressen des Vaters im Kind fortzusetzen.
///
/// Jeder Fehlerausgang raeumt ab, was er angelegt hat (PD-Slot, VSpace, Frames, Kstack,
/// Streifen) — kein `ERR_NOSPACE`-statt-Aufraeumen: ein halb angelegtes Kind waere ein Leck
/// mit gueltiger PD-Nummer.
///
/// **Weder Heap noch Stack-Speicher ueber ~256 Byte in dieser Funktion** (2026-09-10):
/// die sechs Segmentlisten (`quelle`, `vas`, `plan`, `kind_*`, je 64 Eintraege, zusammen
/// ~6 KiB) standen als Arrays auf dem Stack — und der Compiler zog sie per Inlining in
/// `syscall()` hoch, wo sie auf JEDEM Syscall (nicht nur FORK) 4-KiB-Kstacks sprengten
/// (RIP im Syscall-Prolog, RSP 264 B unter dem Stackboden). Auf dem Heap standen sie
/// danach (`Vec`): dort schlugen sie IMMER fehl — der Kernel hat KEINEN Heap (`main.rs`:
/// `NoGlobalHeap`, `alloc` scheitert per Design; `try_reserve` → Err → benannte Absage),
/// der Root-Task startete nie. Sie liegen deshalb in `FORK_SCRATCH` (BSS, Blattlock, als
/// aeusserster Lock zuerst genommen — s. dort), und die Funktion traegt weiter
/// `#[inline(never)]`: selbst ein kuenftiges Inline duerfte die Pfade nie wieder in einen
/// heissen Rahmen legen.
#[inline(never)]
fn dispatch_fork(
    pd: usize,
    kind_slot: usize,
    max_len: u64,
    prio: u8,
) -> Result<u64, u64> {
    use caprock_abi::result;
    // Kratzflaeche ZUERST (Blattlock, s. `FORK_SCRATCH`): keine andere Sperre gehalten;
    // alles Folgende (CAPS/MEM/SCHEDS/...) liegt darunter. Gueltig bis Funktionsende —
    // die Slices unten laufen auf den statischen Feldern.
    let mut kratz = FORK_SCRATCH.lock();
    // EINMAL aufspalten: Borrows durch den `SpinLock`-Guard sind nicht feldgenau
    // (`&kratz.quelle` neben `&mut kratz.kind_segs` lehnte der Borrowchecker ab, E0502) —
    // ueber `&mut ForkScratch` sind sie es. Die Namen unten bleiben die statischen
    // Felder, keine Kopien.
    let kratz: &mut ForkScratch = &mut *kratz;
    // Aufrufer-Domaene + Budget erben; der Kind-Slot muss im Aufrufer-Cspace frei sein —
    // alles lesend, bevor irgendetwas angelegt ist.
    let g = CAPS.read();
    let Some(domain) = g.pds.domain_of(pd) else {
        return Err(result::ERR_NOPD);
    };
    let budget = g.pds.budget_of(pd);
    let caps = g.pds.caps_of(pd);
    let Some(kind_frei) = caps.get(kind_slot).map(|c| c.is_none()) else {
        // Jenseits des Laufs: ein Slot, den es nicht gibt, ist kein freier Slot.
        return Err(result::ERR_BADCAP);
    };
    if !kind_frei {
        return Err(result::ERR_NOSPACE);
    }
    drop(g);
    // Quellmenge: registrierte Frames der eigenen ASID. `asid == 0` (globale SAS-Map) heisst:
    // dieser Aufrufer hat keinen eigenen Adressraum, den es zu kopieren gaebe.
    let core = hal::cpu::core_id();
    let afl = SCHEDS[core].lock().current_id(core);
    let asid = (vspace_of(afl.slot()) >> 48) as u16;
    if asid == 0 {
        return Err(result::ERR_NOPD);
    }
    // Quellmenge: registrierte Frames der eigenen ASID. Kratzfeld statt Heap/Stack
    // (s. Funktionsdoku) — Kapazitaet statisch, kein Reserve-Fehler.
    let nq = loaded_snapshot(asid, &mut kratz.quelle);
    if nq == 0 {
        return Err(result::ERR_NOPD);
    }
    let (src_node, src_maske) = vspace_herkunft(asid);
    let Some((eintritt, startarg)) = loaded_eintritt(asid) else {
        // Registrierte Frames ohne Eintritt: die Buchhaltung widerspricht sich — benannt,
        // nicht geraten (ein Kind ohne Eintritt liefe nach MUSTER).
        #[cfg(feature = "debug-log")]
        println!("fork    : ABGEWIESEN -- asid {asid} hat Frames, aber keinen Eintritt");
        return Err(result::ERR_BADSYS);
    };
    if eintritt == 0 {
        // Gebucht, aber nie belegt (`EMPTY`-Eintritt): dieselbe Lage wie oben, nur eine Stufe
        // frueher — ein Kind am Eintritt 0 faultete an seiner ersten Instruktion.
        #[cfg(feature = "debug-log")]
        println!("fork    : ABGEWIESEN -- asid {asid} ohne gebuchten Eintritt");
        return Err(result::ERR_BADSYS);
    }
    // Die Stack-Fenster fallen aus der Kopie: sie bekommen das Kind frisch (s. Doku oben).
    // Gefiltert wird ueber die VA — ein weiterer Grund, warum die Liste beim Mappen
    // gesammelt wird statt geraten.
    let mut nseg = 0usize;
    for &(phys, va, len, _) in &kratz.quelle[..nq] {
        let _ = phys;
        if va >= LOADED_STACK_VA && va < LOADED_STACK_VA + LOADED_STACK_BYTES {
            continue;
        }
        if nseg >= MAX_IMG_SEGS {
            return Err(result::ERR_SNAPSHOT_LIMIT);
        }
        kratz.vas[nseg] = (va, len);
        nseg += 1;
    }
    // Rein pruefen, bevor irgendetwas alloziert ist (kanonisch in `forkexec`).
    // Kratzfeld statt Heap/Stack (s. Funktionsdoku): `SnapSeg` ist `Copy`.
    let (_nstueck, summe) =
        match forkexec::fork_plan_pruefen(&kratz.vas[..nseg], max_len, &mut kratz.plan) {
        Ok(v) => v,
        Err(f) => return Err(forkexec::fork_fehler_code(f)),
    };
    // Kostenabschaetzung VOR der Allokation: Daten + Tabellen (je ≤512 Seiten eine + eine).
    // Reicht der freie Rest nicht einmal dafuer, faellt der Antrag hier — benannt, nicht erst
    // nach dem dritten kopierten Stueck.
    let (daten, tabellen) = caprock_loader::snapshot::snapshot_kosten(summe);
    if MEM.lock().total_free() < (daten + tabellen) * 4096 {
        mangel(MANGEL_SEGMENT_SPEICHER, summe);
        return Err(result::ERR_NOSPACE);
    }
    // Das Kind: eigene PD (Budget geerbt), eigene VSpace, eigener Streifen wenn moeglich.
    // Der Streifen zuerst (Isolation erhalten, wenn es geht), dann das Erbe der Mutter
    // (benannt: das Kind teilt dann deren Farben), dann ungefärbt (die Farbe gibt zuletzt
    // nach — benannt, nie ein Fehler aus Topologie).
    let budget16 = budget.min(u16::MAX as usize) as u16;
    let Some(kind_pd) = create_pd_mit_budget(domain, budget16) else {
        mangel(MANGEL_PD_SLOT, 0);
        return Err(result::ERR_NOSPACE);
    };
    let (streifen, kind_maske): (Option<(u32, caprock_mem::ColorMask)>, Option<caprock_mem::ColorMask>) =
        match src_maske {
            Some(_) => match crate::colors::claim_stripe() {
                Some(st) => {
                    let m = st.1;
                    (Some(st), Some(m))
                }
                None => {
                    #[cfg(feature = "debug-log")]
                    println!(
                        "fork    : RUECKFALL kein eigener Streifen frei -- Kind erbt Farbsatz der Mutter"
                    );
                    (None, src_maske)
                }
            },
            None => (None, None),
        };
    let knoten = src_node;
    // Ab hier raeumt jeder Fehlerausgang ab (`kind_aufraeumen`): PD-Slot, VSpace, Frames,
    // Kstack und Streifen — in genau dieser Reihenfolge (erst Inhalt, dann Behaelter).
    let kind_aufraeumen = |kind_pd: usize,
                           casid: u16,
                           segs: &[(u64, u64)],
                           stack: Option<(u64, u64)>,
                           kbase: usize,
                           streifen: Option<(u32, caprock_mem::ColorMask)>| {
        {
            let mut mem = MEM.lock();
            for &(b, l) in segs {
                if l > 0 {
                    mem.free_region(PhysRegion::new(b, l));
                }
            }
            if let Some((b, l)) = stack {
                mem.free_region(PhysRegion::new(b, l));
            }
        }
        if casid != 0 {
            vspace_teardown(casid);
        }
        if kbase != 0 {
            release_user_kstack(kbase);
        }
        if let Some((i, _)) = streifen {
            crate::colors::release_stripe(i);
        }
        CAPS.write().pds.free(kind_pd);
    };
    let kbase_opt = claim_user_kstack_masked(kind_maske, knoten);
    let Some(kbase) = kbase_opt else {
        kind_aufraeumen(kind_pd, 0, &[], None, 0, streifen);
        return Err(result::ERR_NOSPACE);
    };
    let vspace_opt = create_vspace_masked(kind_maske, knoten);
    let Some((casid, l1)) = vspace_opt else {
        kind_aufraeumen(kind_pd, 0, &[], None, kbase, streifen);
        return Err(result::ERR_NOSPACE);
    };
    if let Some((i, _)) = streifen {
        vspace_bind_stripe(casid, i);
    }
    let Some(l2) = vspace_l2(casid) else {
        mangel(MANGEL_L2_TABELLE, 0);
        kind_aufraeumen(kind_pd, casid, &[], None, kbase, None);
        return Err(result::ERR_NOSPACE);
    };
    // Stuecke kopieren: phys→phys (beide identity-gemappt), an DIESELBEN VAs mappen.
    // Drei Kratzfelder (s. Funktionsdoku) — Kapazitaet statisch, kein Reserve-Fehler.
    let mut nkind = 0usize;
    for &(sphys, va, len, pcode) in &kratz.quelle[..nq] {
        if va >= LOADED_STACK_VA && va < LOADED_STACK_VA + LOADED_STACK_BYTES {
            continue;
        }
        let Some(p) = perm_von(pcode) else {
            #[cfg(feature = "debug-log")]
            println!("fork    : ABGEWIESEN -- unbekannter Perm-Code {pcode} an VA {va:#x}");
            kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], None, kbase, None);
            return Err(result::ERR_BADSYS);
        };
        let Some(region) = mem_alloc_masked_anywhere_auf(len, 4096, kind_maske, knoten) else {
            mangel(MANGEL_SEGMENT_SPEICHER, len);
            kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], None, kbase, None);
            return Err(result::ERR_NOSPACE);
        };
        let dpa = region.base();
        // SAFETY: beide Frames frisch alloziert bzw. der Quell-Frame der eigenen, lebenden
        // VSpace (der Aufrufer laeuft darauf — `loaded_snapshot` haelt kein Lock, aber die
        // Frames gehoeren bis zum Teardown dieser ASID niemand anderem); beide
        // identity-gemappt, exakt `len` Bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(sphys as *const u8, dpa as *mut u8, len as usize);
        }
        kratz.kind_segs[nkind] = (dpa, len);
        kratz.kind_vas[nkind] = va;
        kratz.kind_perms[nkind] = pcode;
        nkind += 1;
        let mut off = 0u64;
        while off < len {
            let mut a3 = || {
                let r = pt_rahmen(mem_alloc_masked_anywhere_auf(4096, 4096, kind_maske, knoten));
                if r.is_none() {
                    mangel(MANGEL_SEITENTABELLE, 4096);
                }
                r
            };
            if !hal::mmu::vspace_map_page_at(l2, va + off, dpa + off, p, &mut a3) {
                if lade_mangel().0 == MANGEL_KEINER {
                    mangel(MANGEL_MAPPING_ABGEWIESEN, 0);
                }
                kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], None, kbase, None);
                return Err(result::ERR_NOSPACE);
            }
            off += 4096;
        }
    }
    // Frischer Stack an derselben VA (genullt aus dem Allokator — nichts geht ungenullt an
    // ein Subjekt). Gefaerbt wandert er in `kind_segs` (Teardown wie die Segmente), ungefaerbt
    // als Reap-Region an den Thread (wie im Ladepfad — derselbe benannte Unterschied).
    let mut kind_stack: Option<(u64, u64)> = None;
    {
        let n = LOADED_STACK_BYTES;
        let Some(region) = mem_alloc_masked_anywhere_auf(n, 4096, kind_maske, knoten) else {
            mangel(MANGEL_STACK_SPEICHER, n);
            kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], None, kbase, None);
            return Err(result::ERR_NOSPACE);
        };
        let spa = region.base();
        if kind_maske.is_none() {
            kind_stack = Some((spa, n));
        } else {
            kratz.kind_segs[nkind] = (spa, n);
            kratz.kind_vas[nkind] = LOADED_STACK_VA;
            kratz.kind_perms[nkind] = PERM_RW;
            nkind += 1;
        }
        let mut off = 0u64;
        while off < n {
            let mut a3 = || {
                let r = pt_rahmen(mem_alloc_masked_anywhere_auf(4096, 4096, kind_maske, knoten));
                if r.is_none() {
                    mangel(MANGEL_SEITENTABELLE, 4096);
                }
                r
            };
            if !hal::mmu::vspace_map_page_at(
                l2,
                LOADED_STACK_VA + off,
                spa + off,
                hal::mmu::UserPerm::Rw,
                &mut a3,
            ) {
                if lade_mangel().0 == MANGEL_KEINER {
                    mangel(MANGEL_MAPPING_ABGEWIESEN, 0);
                }
                kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], kind_stack, kbase, None);
                return Err(result::ERR_NOSPACE);
            }
            off += 4096;
        }
    }
    hal::mmu::flush_asid(casid);
    // Thread: geparkt erzeugen, buchen (noch unter SCHEDS — s. `record_user_kstack`), binden,
    // Kind-Cap beim Aufrufer eintragen, DANN zulassen (D0: parken — binden — zulassen).
    let daif = hal::cpu::local_irq_save();
    let ctid = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_user_at_parked(
            core,
            eintritt,
            startarg,
            kbase,
            USER_KSTACK_SIZE,
            (LOADED_STACK_VA + LOADED_STACK_BYTES) as usize,
            kind_stack.map(|(b, _)| b as usize).unwrap_or(0),
            kind_stack.map(|(_, l)| l as usize).unwrap_or(0),
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot());
            record_user_kstack(t, kbase);
            // Gefaerbt liegt der Stack in `kind_segs` (PD-Eigentum, kein Reap) — dann ist die
            // Buchung hier `(0, 0)` und damit ein No-Op (s. `record_user_region`).
            let (sb, sl) = kind_stack.unwrap_or((0, 0));
            record_user_region(t.slot(), sb, sl);
            set_vspace_of(t.slot(), ((casid as u64) << 48) | l1);
        }
        r
    };
    let Some(ctid) = ctid else {
        hal::cpu::local_irq_restore(daif);
        mangel(MANGEL_THREAD_SLOT, 0);
        kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], kind_stack, kbase, None);
        return Err(result::ERR_NOSPACE);
    };
    if !loaded_register(
        casid,
        eintritt,
        startarg,
        &kratz.kind_segs[..nkind],
        &kratz.kind_vas[..nkind],
        &kratz.kind_perms[..nkind],
    ) {
        hal::cpu::local_irq_restore(daif);
        // `kill_remote` rechnet den Kstack selbst ab (`reclaim_user_kstack`) und der Reaper die
        // Reap-Region — beides danach NICHT mehr anfassen (doppelte Freigabe bzw. fremder Stack).
        // `0`/`None` heisst fuer `kind_aufraeumen „bereits abgerechnet"; was `kill` ablehnt,
        // bleibt benannt stehen (Leck statt UAF).
        if !kill_remote(ctid) {
            #[cfg(feature = "debug-log")]
            println!(
                "fork    : RUECKZUG Kind-Thread nicht beendbar -- PD {kind_pd} bleibt stehen (Leck statt UAF)"
            );
        }
        kind_aufraeumen(kind_pd, casid, &kratz.kind_segs[..nkind], None, 0, None);
        return Err(result::ERR_NOSPACE);
    }
    // KEIN `record_program_thread`: das Kind hat keine `program_id` aus dem Manifest, und die
    // Tabelle beantwortet „steht fuer jeden Manifest-Eintrag ein Programm" (`vollzaehligkeit`).
    // Ein Kind-Eintrag unter der PD-Nummer koennte einen fehlenden Manifest-Eintrag derselben
    // Nummer als geladen ausweisen — ein stiller Vollzaehligkeits-Fund, schlimmer als keiner.
    bind_pd(kind_pd, ctid);
    // Die Kind-`PdControl`-Cap in den Ziel-Slot des Aufrufers (autorisiert: nur TrustedSas
    // haelt `PdControl` — `install_pd_cap` prueft die Policy, und was sie abweist, wird
    // geloescht statt geleckt).
    let kind_cap = match install_pd_control_cap(kind_pd, Rights::WRITE) {
        Ok(c) => c,
        Err(_) => {
            hal::cpu::local_irq_restore(daif);
            // Kstack/Reap liegen beim `kill` (s. oben) — hier `0`/`None`; registrierte Frames
            // raeumt der Teardown (`&[]` heisst: nichts Ungetracktes mehr frei).
            if !kill_remote(ctid) {
                println!(
                    "fork    : RUECKZUG Kind-Thread nicht beendbar -- PD {kind_pd} bleibt stehen (Leck statt UAF)"
                );
            }
            kind_aufraeumen(kind_pd, casid, &[], None, 0, None);
            return Err(result::ERR_NOSPACE);
        }
    };
    if !install_pd_cap(pd, kind_slot, kind_cap) {
        let _ = cap_delete(kind_cap);
        hal::cpu::local_irq_restore(daif);
        if !kill_remote(ctid) {
            #[cfg(feature = "debug-log")]
            println!(
                "fork    : RUECKZUG Kind-Thread nicht beendbar -- PD {kind_pd} bleibt stehen (Leck statt UAF)"
            );
        }
        kind_aufraeumen(kind_pd, casid, &[], None, 0, None);
        return Err(result::ERR_BADCAP);
    }
    if !SCHEDS[core].lock().admit(ctid) {
        hal::cpu::local_irq_restore(daif);
        // Wie der Ladepfad (`load_into_pd_mit_va`): keine Zulassung ohne Aufloesung — best
        // effort beenden, registrierte Frames ueber den Teardown, Kstack/Reap ueber Kill/Reaper.
        #[cfg(feature = "debug-log")]
        println!("fork    : RUECKZUG Kind-Thread nicht zulassbar -- PD {kind_pd} wird abgebaut");
        kill_remote(ctid);
        kind_aufraeumen(kind_pd, casid, &[], None, 0, None);
        return Err(result::ERR_NOSPACE);
    }
    hal::cpu::local_irq_restore(daif);
    Ok(kind_pd as u64)
}

/// `EXEC_REPLACE`: neues Image in die BESTEHENDE PD laden (Teardown-Token-Form).
///
/// Geordneter Rueckzug statt `ERR_NOSPACE`-statt-Aufraeumen: fremde Threads abziehen, Slots
/// loeschen (ausser dem der Loader-Cap), DANN laden — und erst nach erfolgreichem Laden den
/// Aufrufer auf die neue VSpace binden, die alte Abbauen und die Epoche heben. Schlaegt das
/// Laden fehl, lebt die alte Fassung weiter (Slots ausser Loader sind dann allerdings schon
/// geraeumt — benannt, kein stiller Ueberrest) und das Token bleibt gueltig (die Epoche hebt
/// sich erst bei Erfolg): ein Fehlschlag ist wiederholbar, kein Einbahnstrassen-Verlust.
///
/// Was die Reihenfolge gegenueber der `exec`-Doku (`erst 1-3, dann laden`) verschiebt: dort
/// steht der Rueckbau vor dem Laden, hier laedt erst das Neue, dann geht das Alte. Der Grund
/// ist atomar: mit zwei VSpaces (die PD lebt weiter, nur ihr Inhalt geht) ist „erst laden,
/// dann umbinden und abbauen" der einzige Weg, auf dem ein Fehlschlag nichts halb
/// Geraeumtes hinterlaesst. Die alte VSpace wird erst nach dem Umbinden abgebaut — nie zeigt
/// ein lebender Thread auf freigegebene Tabellen.
#[inline(never)] // s. `dispatch_fork`: heisse Rahmen bleiben schlank, auch per Inline.
fn dispatch_exec(
    pd: usize,
    loader_cap: caprock_cap::CapPtr,
    loader_slot: usize,
    prog_index: u32,
    token: u64,
) -> Result<(), u64> {
    use caprock_abi::result;
    // 1. Loader-Autoritaet: Loader-Art + WRITE (dieselbe Pruefung wie SYS_LOAD).
    {
        let g = CAPS.read();
        match g.cspace.lookup(loader_cap) {
            Some((caprock_cap::ObjectKind::Loader { .. }, r, _))
                if r.contains(caprock_mem::Rights::WRITE) => {}
            _ => return Err(result::ERR_BADCAP),
        }
    }
    // 2. Token gegen (program_id, Epoche): 0 ist kein „egal" (kanonisch in `forkexec`).
    if token == 0 {
        return Err(result::ERR_STALE_TOKEN);
    }
    let core = hal::cpu::core_id();
    let me = SCHEDS[core].lock().current_id(core);
    // Nur die EIGENE PD wird ersetzt — eine fremde waere ein Kill mit Extraschritten.
    match CAPS.read().pds.pd_of_thread(me) {
        Some(eigen) if eigen == pd => {}
        _ => return Err(result::ERR_BADCAP),
    }
    let prog_id = crate::loader::program_of_thread(me).unwrap_or(u32::MAX);
    let epoche = EXEC_EPOCHE.lock()[pd % caprock_microkit::NPDS];
    // 3. Programm aufloesen (Archiv-Index -> Program; dieselbe Quelle wie `reload_driver`).
    let arch = crate::loader::read_archive().ok_or(result::ERR_BADCAP)?;
    let prog = arch.program(prog_index as usize).ok_or(result::ERR_BADCAP)?;
    let img = match caprock_loader::elf::ElfImage::parse(prog.elf) {
        Ok(i) => i,
        Err(_) => {
            #[cfg(feature = "debug-log")]
            println!("exec    : ABGEWIESEN -- Archiv-Index {prog_index} ist kein gueltiges ELF");
            return Err(result::ERR_BADCAP);
        }
    };
    let nseg = img.segments().count();
    forkexec::exec_antrag_pruefen(prog_id, epoche, token, img.entry(), nseg)?;
    // Herkunft der laufenden Fassung: Farbe und Knoten erbt die neue (der Aufrufer hat keine
    // Manifestposition mehr — EXEC ist Laufzeit, kein Boot).
    let alt_asid = (vspace_of(me.slot()) >> 48) as u16;
    let (src_node, src_maske) = vspace_herkunft(alt_asid);
    // 4a. Fremde Threads abziehen (KILL-Ordnung) — alle ausser dem Aufrufer, begrenzt durch
    // die Threadzahl plus einen: was danach noch laeuft und fremd ist, ist benannt (der Kill
    // hat es abgelehnt), nicht uebersehen.
    let fremd_anzahl = CAPS.read().pds.thread_count(pd).saturating_add(1) as usize;
    for _ in 0..fremd_anzahl {
        let gef = core::cell::Cell::new(None);
        {
            let g = CAPS.read();
            // `any_thread` nimmt `&dyn Fn` (kein `FnMut`) — deshalb die Zelle statt `&mut`.
            g.pds.any_thread(pd, &|t| {
                if t != me && gef.get().is_none() {
                    gef.set(Some(t));
                }
                false
            });
        }
        let Some(t) = gef.get() else { break };
        if !kill_remote(t) {
            #[cfg(feature = "debug-log")]
            println!("exec    : RUECKZUG unvollstaendig -- Thread {} blieb (PD {pd})", t.to_raw());
            break;
        }
    }
    // 4b. Alle Slots loeschen ausser dem der Loader-Cap. Was `cap_delete` ablehnt (Kinder),
    // wird trotzdem aus der PD geraeumt (Autoritaet entzogen) und benannt — ein globales
    // Leck ist besser als eine EXEC-PD mit fremder Autoritaet.
    {
        let caps = CAPS.read().pds.caps_of(pd);
        for (slot, c) in caps.iter().enumerate() {
            if slot == loader_slot {
                continue;
            }
            if let Some(cap) = *c {
                if cap_delete(cap).is_err() {
                    #[cfg(feature = "debug-log")]
                    println!("exec    : RUECKZUG Slot {slot} global nicht freigegeben -- aus PD {pd} geraeumt");
                }
                clear_pd_cap(pd, slot);
            }
        }
    }
    // 4c. Neu laden in DIESELBE PD (neue VSpace, neuer Thread — die PD lebt weiter, nur ihr
    // Inhalt geht). Farbe und Knoten der alten Fassung reisen mit; die Prio ist die Vorgabe
    // (EXEC umgeht das Manifest, und eine erfundene Prio waere eine Zusicherung, die niemand
    // verlangt hat — dieselbe Form wie `budget_us` im `zahlenpolitik_gate`).
    let pol = LadePolitik {
        farbig: src_maske.is_some(),
        node: src_node,
        prio: LadePolitik::VORGABE.prio,
        core: Some(core),
        budget_us: 0,
        angebotene_slots: 0,
        cap_budget: 0,
    };
    let arg = crate::loader::boot_arg(prog_index as usize, arch.count());
    let (neu, _) = match load_into_pd_mit_va(&img, pd, &[], arg, pol, None) {
        Some(v) => v,
        None => {
            // Die alte Fassung lebt weiter (ihre VSpace wurde nicht angeruehrt); das Token
            // bleibt gueltig (Epoche ungehubt) — wiederholbar statt verloren. Was fehlt
            // (geraemte Slots, abgezogene Threads), steht oben benannt.
            #[cfg(feature = "debug-log")]
            println!("exec    : ABGEBROCHEN -- alte Fassung laeuft weiter (PD {pd}), Token gueltig");
            return Err(result::ERR_NOSPACE);
        }
    };
    // 4d. Aufrufer auf die neue VSpace binden, DANN die alte abbauen (nie umgekehrt — sonst
    // zeigte ein lebender Thread auf freigegebene Tabellen).
    let neu_asid = (vspace_of(neu.slot()) >> 48) as u16;
    match vspace_l1(neu_asid) {
        Some(neu_l1) => set_vspace_of(me.slot(), ((neu_asid as u64) << 48) | neu_l1),
        None => {
            #[cfg(feature = "debug-log")]
            println!("exec    : WIDERSPRUCH -- neue ASID {neu_asid} ohne L1 (PD {pd})");
            return Err(result::ERR_BADSYS);
        }
    }
    if alt_asid != 0 && alt_asid != neu_asid {
        vspace_teardown(alt_asid);
    }
    // 5. Epoche erst nach erfolgreichem Laden heben — sonst verlöre ein Fehlschlag das Token
    // der noch laufenden alten Fassung.
    EXEC_EPOCHE.lock()[pd % caprock_microkit::NPDS] = epoche.wrapping_add(1).max(1);
    Ok(())
}

/// FORK/EXEC (31/32) VOR dem Microkit-Dispatch (s. Modul-Doku oben).
///
/// `caprock-microkit` ist nur gelesen: seine 31/32-Arme bleiben fail-closed `ERR_BADSYS`
/// („Antrag ok, Pfad fehlt") fuer den Fall, dass dieser Hook je umgangen wird. Die
/// Vorpruefung (`proc::dekodiere_fork/exec`: reserviert == 0, Token != 0, Laenge gedeckelt)
/// laeuft hier — ohne einen einzigen Seiteneffekt — und danach der Rueckruf.
#[inline(never)] // s. `dispatch_fork`: dieser Hook steht in `syscall()`, sein Rahmen muss
// schlank bleiben -- die dicken Listen wohnen dahinter, nie darin.
fn forkexec_syscall(frame: usize, core: usize, nr: u64) -> usize {
    use caprock_abi::{reg, result, sys};
    use caprock_microkit::proc::{dekodiere_exec, dekodiere_fork, ForkExecEntscheid};
    let deny = |code: u64| -> usize {
        hal::exception::frame_set_reg(frame, reg::SYSNO_RESULT, code);
        frame
    };
    if nr == sys::FORK_SNAPSHOT {
        let m0 = hal::exception::frame_reg(frame, reg::MSG0);
        let m1 = hal::exception::frame_reg(frame, reg::MSG0 + 1);
        let m2 = hal::exception::frame_reg(frame, reg::MSG0 + 2);
        let m3 = hal::exception::frame_reg(frame, reg::MSG0 + 3);
        match dekodiere_fork(m0, m1, m2, m3) {
            ForkExecEntscheid::Abgewiesen(code) => {
                FORK_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(code)
            }
            ForkExecEntscheid::Fork(a) => {
                let me = SCHEDS[core].lock().current_id(core);
                let pd = { CAPS.read().pds.pd_of_thread(me) };
                // CAPS freigegeben — `dispatch_fork` nimmt CAPS/MEM/SCHEDS selbst.
                let Some(pd) = pd else {
                    FORK_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                    return deny(result::ERR_NOPD);
                };
                match dispatch_fork(pd, a.kind_slot, a.max_len, a.prio) {
                    Ok(kind_pd) => {
                        FORK_GELUNGEN.fetch_add(1, Ordering::Relaxed);
                        hal::exception::frame_set_reg(frame, reg::MSG0, kind_pd);
                        hal::exception::frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                        frame
                    }
                    Err(code) => {
                        FORK_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                        deny(code)
                    }
                }
            }
            ForkExecEntscheid::Exec(_) => {
                FORK_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(result::ERR_BADCAP)
            }
            ForkExecEntscheid::Unbekannt => {
                FORK_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(result::ERR_BADSYS)
            }
        }
    } else {
        let loader_slot = hal::exception::frame_reg(frame, reg::EP_BADGE);
        let m0 = hal::exception::frame_reg(frame, reg::MSG0);
        let m1 = hal::exception::frame_reg(frame, reg::MSG0 + 1);
        let m2 = hal::exception::frame_reg(frame, reg::MSG0 + 2);
        let m3 = hal::exception::frame_reg(frame, reg::MSG0 + 3);
        match dekodiere_exec(loader_slot, m0, m1, m2, m3) {
            ForkExecEntscheid::Abgewiesen(code) => {
                EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(code)
            }
            ForkExecEntscheid::Exec(a) => {
                let me = SCHEDS[core].lock().current_id(core);
                let (pd, cap) = {
                    let g = CAPS.read();
                    let Some(pd) = g.pds.pd_of_thread(me) else {
                        EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                        return deny(result::ERR_NOPD);
                    };
                    let cap = g
                        .pds
                        .caps_of(pd)
                        .get(a.loader_slot)
                        .copied()
                        .flatten();
                    match cap {
                        Some(c) => (pd, c),
                        None => {
                            EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                            return deny(result::ERR_BADCAP);
                        }
                    }
                };
                // CAPS freigegeben — `dispatch_exec` nimmt CAPS/MEM/SCHEDS selbst.
                match dispatch_exec(pd, cap, a.loader_slot, a.prog_index, a.token) {
                    Ok(()) => {
                        EXEC_GELUNGEN.fetch_add(1, Ordering::Relaxed);
                        deny(result::OK)
                    }
                    Err(code) => {
                        EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                        deny(code)
                    }
                }
            }
            ForkExecEntscheid::Fork(_) => {
                EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(result::ERR_BADCAP)
            }
            ForkExecEntscheid::Unbekannt => {
                EXEC_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
                deny(result::ERR_BADSYS)
            }
        }
    }
}

/// **`SYS_SPAWN`: a thread whose stack the CALLER brought** (K1a, 2026-08-17).
///
/// The difference to [`spawn_user_parked`] is one line long and it is the whole point: that
/// function calls `mem_alloc`, this one does not. **The kernel manages authority, not supply** —
/// the region comes out of a Cap the caller already holds, so the caller carries the cost and the
/// memory policy stays in userspace where it is revisable.
///
/// `stack_base`/`stack_len` have already passed [`caprock_cap::spawncheck::check_stack`]: memory
/// Cap, `rw`, `normal` space, **not device-reachable**, aligned, big enough, non-overlapping.
/// This function does not re-derive any of that — *a checker that recomputes the quantity it
/// checks is checking a second reality.*
///
/// **What is deliberately NOT done here: `record_user_region`.** That call marks a region as
/// kernel-owned so the reap path frees it on thread death. This stack is **not** the kernel's; it
/// belongs to the Cap, and freeing it would be a double free the moment the caller deletes the
/// Cap. The Cap's own finalisation returns the memory — which is exactly why deleting it while
/// the thread lives is refused (`ERR_INUSE`).
pub fn spawn_with_stack_parked(
    entry: usize,
    arg: usize,
    stack_base: u64,
    stack_top: u64,
    prio: u8,
) -> Option<Parked> {
    let core = hal::cpu::core_id();
    mangel_zuruecksetzen();
    // Der EL1-Kernel-Stack bleibt Kernelsache: er traegt Kernelzustand, nie Userdaten, und ein
    // Aufrufer duerfte ihn nicht stellen (er koennte hineinschreiben, waehrend der Kernel darauf
    // laeuft). Nur der EL0-Stack kommt aus der Cap.
    let kbase = claim_user_kstack()?;
    let tid = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_user_at_parked(
            core,
            entry,
            arg,
            kbase,
            USER_KSTACK_SIZE,
            stack_top as usize, // EL0-SP: Stapel wachsen nach unten, also das ENDE
            // Reap-Region **leer**: die Region gehoert der Cap, nicht dem Kernel. Ein `0`-Laenge
            // heisst „beim Thread-Tod nichts freigeben" -- s. Doku oben.
            stack_base as usize,
            0,
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot());
            record_user_kstack(t, kbase);
        }
        r
    };
    match tid {
        Some(t) => Some(Parked(t)),
        None => {
            release_user_kstack(kbase);
            None
        }
    }
}

pub fn spawn_user_parked(entry: usize, arg: usize, prio: u8) -> Option<Parked> {
    let core = hal::cpu::core_id();
    mangel_zuruecksetzen();
    let kbase = claim_user_kstack()?; // benennt sich selbst (s. `claim_user_kstack_masked`)
    let (user_base, user_len) = match benannt_alloc(MANGEL_STACK_SPEICHER, STACK_SIZE, |n| {
        mem_alloc(n, 16)
    }) {
        Some(s) => (s.base() as usize, s.len() as usize),
        None => {
            release_user_kstack(kbase);
            return None;
        }
    }; // MEM vor SCHEDS freigegeben (Ordnung MEM < SCHEDS)
    let tid = {
        let mut sched = SCHEDS[core].lock();
        // **Beide Werte hingeschrieben, auch wenn sie hier gleich sind.** Ein SAS-Thread teilt
        // sich die Identitaetskarte des Kernels, sein Stack ist also unter derselben Zahl
        // virtuell wie physisch. Genau deshalb steht sie zweimal da: `spawn_user` (ein Wert
        // fuer beides) ist geloescht, weil die Gleichheit hier ein Zufall der Umgebung ist und
        // keine Eigenschaft des Aufrufs.
        let r = sched.spawn_user_at_parked(
            core,
            entry,
            arg,
            kbase,
            USER_KSTACK_SIZE,
            user_base + user_len, // EL0-SP (virtuell; im SAS identisch zur PA)
            user_base,            // Reap-Region (physisch)
            user_len,
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot()); // frischer FP-Kontext, bevor der Thread laufen kann
            // Buchfuehrung NOCH UNTER SCHEDS -- s. Kommentar an `record_user_kstack`.
            record_user_kstack(t, kbase); // Kstack dem Thread zuordnen
            // C7b: SAS-User-Stack (`STACK_SIZE`) -- dieselbe Klasse, andere Groesse.
            record_user_region(t.slot(), user_base as u64, user_len as u64);
        }
        r
    }; // SCHEDS freigegeben
    match tid {
        Some(t) => Some(Parked(t)),
        None => {
            release_user_kstack(kbase);
            MEM.lock().free_region(PhysRegion::new(user_base as u64, user_len as u64));
            None
        }
    }
}

/// Isolierte-VSpace-Slots aus Boot-RAM (Dichte): eine VSpace je PD im Grenzfall.
///
/// Bis hierher eine statische Reserve (`[VSpaceEnt; 4096]` im BSS, rund 128 KiB): 10 000
/// Threads waren darstellbar, aber nur solange sie sich Adressraeume teilen — als 10 000
/// isolierte Tenants nicht, und genau das ist der Punkt des Systems. Die Tabelle wird jetzt
/// beim Boot alloziert (s. `configure_caps`), die Zahl kostet RAM statt Struktur, und was sie
/// kostet, steht im Boot-Report. Dimensioniert auf `NPDS`: im Grenzfall bekommt jede PD ihre
/// eigene VSpace. Die **tatsaechlich vergebene** Anzahl deckelt `create_vspace` weiter auf
/// `min(angehaengt, max_asid())`: die HW-ASID-Breite ist die harte Grenze — eine ASID darueber
/// wuerde aliasen. Ein vergessener Anhang faellt als benannte Absage aus dem leeren Vorrat
/// auf, nicht als Fehlzugriff: die Tabelle hat dann Laenge 0, und jede Vergabe meldet den
/// ASID-Topf statt daneben zu greifen.

/// Metadaten einer isolierten VSpace (für map/unmap + Teardown). Indiziert per
/// `asid - 1`.
#[derive(Clone, Copy)]
struct VSpaceEnt {
    used: bool,
    l1: u64,
    l2: u64,
    /// Belegter Farbstreifen dieser PD (B-4.2), `None` bei ungefärbten VSpaces.
    ///
    /// **Warum hier und nicht am Thread:** ein Streifen gehört dem *Adressraum*, nicht dem
    /// Ausführungsfaden — Region, Kernel-Stack und Seitentabellen der PD stammen daraus. Am
    /// Thread aufgehängt würde er bei mehreren Threads je PD mehrfach freigegeben oder gar nicht;
    /// an der VSpace hat er genau einen Anfang und genau ein Ende (`vspace_teardown`).
    stripe: Option<u32>,
    /// Der GEWUENSCHTE Knoten dieser VSpace (Z8/N3) — die Platzierung, keine Messung.
    ///
    /// Gesetzt aus `LadePolitik.node` beim Anlegen; was davon WIRKUNG wurde, zaehlt die Leiter
    /// (`numa::stats`), nicht dieses Feld. Ein Wunsch ist von einem Treffer zu unterscheiden —
    /// sonst liest sich „Knoten 0 gewuenscht, weil das Manifest schwieg" wie „Knoten 0 getroffen".
    /// Spawn-Wege ohne Manifest tragen `Unaffiliated`.
    node: caprock_hal::numa::Node,
    /// Der Farbsatz, aus dem Tabellen und Stacks dieser VSpace stammen (Z8/N3, FORK-Erbe).
    ///
    /// `None` heisst ungefärbt. Zusammen mit `node` erbt der FORK-Pfad daraus die Platzierung
    /// der Eltern-PD, ohne sie zu raten: was die Mutter trug, steht hier, nicht in einer zweiten
    /// Tabelle, die daneben altern wuerde.
    cmask: Option<caprock_mem::ColorMask>,
}

impl VSpaceEnt {
    /// Freier Slot — keine Herkunft, keine Farbe, kein Knoten.
    const FREI: VSpaceEnt = VSpaceEnt {
        used: false,
        l1: 0,
        l2: 0,
        stripe: None,
        node: caprock_hal::numa::Node::Unaffiliated,
        cmask: None,
    };
}
static VSPACES: SpinLock<Slab<VSpaceEnt>> = SpinLock::new(Slab::empty());

/// Zahl der angehaengten VSpace-Slots (`0` = `configure_caps` vergessen oder zu spaet).
fn vspace_slots() -> usize {
    VSPACES.lock().len()
}

/// VSpace-Kapazitaet fuer den Bericht: `(angehaengt, nutzbar)`.
///
/// `nutzbar` ist durch die HW-ASID-Breite gedeckelt (x86: keine, aarch64: 255 ohne FEAT_ASID16)
/// — angehaengt heisst nicht verwendbar, und die beiden Zahlen gehoeren auseinander, sonst
/// liest sich ein voller Anhang wie volle Verfuegbarkeit.
pub fn vspace_capacity() -> (usize, usize) {
    let n = vspace_slots();
    (n, n.min(hal::mmu::max_asid() as usize))
}

/// Eine **leere** isolierte VSpace anlegen (Kernel EL1-only, kein User-Frame). Gibt
/// `(asid, l1_phys)` oder `None` (kein ASID/Speicher). Allokiert L1+L2 aus `MEM`
/// (zuerst, freigegeben), trägt dann die Metadaten ein (`MEM` und `VSPACES` nie
/// gleichzeitig gehalten -> keine Sperrordnungs-Inversion zu Teardown/map).
fn create_vspace() -> Option<(u16, u64)> {
    create_vspace_masked(None, caprock_hal::numa::Node::Unaffiliated)
}

/// Wie [`create_vspace`], aber die Seitentabellen kommen optional aus einem Farbsatz (todo A1)
/// und mit Knotenwunsch (Z8/N3).
///
/// Tabellenzeilen werden vom **Seitenlaufwerk der MMU** geladen und liegen im selben LLC wie
/// alles andere; ein Walk im Namen einer PD hinterlaesst also Spuren. Sie mitzufaerben kostet
/// nichts (zwei bzw. drei 4-KiB-Seiten) und schliesst einen Kanal, den man sonst uebersieht.
///
/// Der Knoten laeuft ueber [`mem_alloc_masked_auf`] (die Leiter: exakt, dann ausserhalb —
/// benannt, nie ein Fehler aus Topologie). Die Herkunft (`node`, `cmask`) steht danach in der
/// VSpace selbst (`vspace_herkunft`) — der FORK-Pfad erbt sie, ohne zu raten.
fn create_vspace_masked(
    mask: Option<caprock_mem::ColorMask>,
    node: caprock_hal::numa::Node,
) -> Option<(u16, u64)> {
    // ASID/VSpace-Slot aus der **Free-List** (VSPACES) belegen — wiederverwendbar
    // (kein monoton wachsender Zähler -> keine ASID-Leaks). Reservierung unter EINEM
    // Lock (Platzhalter), damit zwei Kerne nicht denselben Slot greifen.
    let asid = {
        // Nur die ersten `usable` Slots vergeben: ASID = Slot+1 darf die HW-ASID-Breite
        // (`max_asid()`, 255 oder 65535) NIE überschreiten, sonst aliast eine zu große ASID auf
        // eine andere VSpace (Isolationsbruch). Angehaengt sein darf mehr, als die HW-Grenze
        // hergibt (Boot-RAM-Tabelle), genutzt wird aber nur bis `usable`.
        let usable = vspace_slots().min(hal::mmu::max_asid() as usize);
        let mut t = VSPACES.lock();
        // **Zwei Toepfe in EINER Funktion, und sie muessen unterscheidbar bleiben.** Hier ist der
        // ASID-Vorrat leer -- eine Zahl fester Groesse. Zwei Zeilen weiter unten waere es
        // Seitentabellen-SPEICHER, ein dynamischer Topf. Wer beides `MANGEL_VSPACE_ASID` nennt,
        // schickt die naechste Diagnose an die falsche Stelle.
        let Some(i) = t.iter().take(usable).position(|v| !v.used) else {
            drop(t);
            mangel(MANGEL_VSPACE_ASID, 0);
            return None;
        };
        t[i] = VSpaceEnt {
            used: true,
            l1: 0,
            l2: 0,
            stripe: None,
            node,
            cmask: mask,
        }; // reserviert
        (i + 1) as u16
    };
    // Ab hier zaehlt jeder Rahmen in den Seitentabellen-Topf (C7) -- und der Fehlerpfad bucht
    // ihn wieder aus, sonst waere der Fuellstand nach dem ersten Fehlschlag dauerhaft zu hoch.
    let a = pt_rahmen(mem_alloc_masked(4096, 4096, mask, node));
    let b = pt_rahmen(mem_alloc_masked(4096, 4096, mask, node));
    let (l1, l2) = match (a, b) {
        (Some(l1), Some(l2)) => (l1, l2),
        (a, b) => {
            // **Der Allokator hat gesprochen, nicht der Aufrufer.** Genau hier stand am
            // 2026-08-10 die Verwechslung, die die `wasmhost`-Diagnose zweimal in die falsche
            // Richtung geschickt hat: der leere Topf ist der SEITENTABELLEN-Speicher, nicht der
            // ASID-Vorrat -- die beiden liegen in derselben Funktion und haben nichts miteinander
            // zu tun.
            mangel(MANGEL_SEITENTABELLE, 4096);
            let mut mem = MEM.lock();
            let mut zurueck = 0u64;
            if let Some(c) = a {
                mem.free_region(PhysRegion::new(c, 4096));
                zurueck += 1;
            }
            if let Some(c) = b {
                mem.free_region(PhysRegion::new(c, 4096));
                zurueck += 1;
            }
            drop(mem);
            pt_zurueck(zurueck);
            VSPACES.lock()[asid as usize - 1].used = false; // Slot zurückgeben
            return None;
        }
    };
    // Läuft in der globalen Map. Der `alloc`-Rückkanal deckt Architekturen mit einer
    // zusätzlichen Tabellenebene ab (x86_64: PML4 -> PDPT -> PD); schlägt er fehl, werden die
    // beiden bereits belegten Frames wieder freigegeben.
    let mut extra: Option<u64> = None;
    let ok = hal::mmu::vspace_create_base(l1, l2, &mut || {
        // **Auch diese Ebene traegt die Maske.** Sie ging bisher ueber das ungefaerbte
        // `mem_alloc` — auf x86_64 (PML4 -> PDPT -> PD) lag damit eine der drei Tabellen einer
        // gefaerbten PD ausserhalb ihres Farbsatzes, und der Farbtest sah es nicht, weil er nur
        // `l1`/`l2` zurueckliest. Ein Seitenlauf der MMU im Namen der PD hinterlaesst dort
        // dieselben Spuren wie in den beiden anderen. Schlaegt die gefaerbte Zuteilung fehl,
        // scheitert das Anlegen der VSpace — kein stiller Rueckfall auf fremde Farben. Mit
        // Knotenwunsch laeuft dieselbe Leiter wie bei L1/L2 (exakt, dann ausserhalb).
        let f = pt_rahmen(mem_alloc_masked(4096, 4096, mask, node))?;
        extra = Some(f);
        Some(f)
    });
    if !ok {
        // Auch hier hat der Allokator gesprochen: `vspace_create_base` gibt `false`
        // ausschliesslich dann, wenn der Rueckkanal nichts geliefert hat.
        mangel(MANGEL_SEITENTABELLE, 4096);
        let mut mem = MEM.lock();
        mem.free_region(PhysRegion::new(l1, 4096));
        mem.free_region(PhysRegion::new(l2, 4096));
        let mut zurueck = 2u64;
        if let Some(e) = extra {
            mem.free_region(PhysRegion::new(e, 4096));
            zurueck += 1;
        }
        drop(mem);
        pt_zurueck(zurueck);
        VSPACES.lock()[asid as usize - 1].used = false;
        return None;
    }
    VSPACES.lock()[asid as usize - 1] = VSpaceEnt {
        used: true,
        l1,
        l2,
        // Der Streifen wird nach dem Anlegen gesetzt (`vspace_bind_stripe`): hier ist noch nicht
        // entschieden, ob diese VSpace eine gefärbte PD trägt. Knoten und Farbsatz dagegen stehen
        // schon hier — sie sind die WUNSCH-Herkunft dieses Anlegens (s. `vspace_herkunft`).
        stripe: None,
        node,
        cmask: mask,
    };
    Some((asid, l1))
}

/// Die Herkunft einer VSpace: `(gewuenschter Knoten, Farbsatz)` (Z8/N3, FORK-Erbe).
///
/// `(Unaffiliated, None)` heisst „keine Politik" — das gilt fuer Spawn-Wege ohne Manifest,
/// fuer freie/unbekannte ASIDs und fuer den globalen SAS-Fall (`asid == 0`). Der FORK-Pfad
/// liest hier die Platzierung der Eltern-PD, statt sie zu raten; was er damit tut (eigener
/// Streifen zuerst, dann Erbe, dann Rueckfall), steht in `dispatch_fork`.
fn vspace_herkunft(asid: u16) -> (caprock_hal::numa::Node, Option<caprock_mem::ColorMask>) {
    if asid == 0 || asid as usize > vspace_slots() {
        return (caprock_hal::numa::Node::Unaffiliated, None);
    }
    let v = VSPACES.lock()[asid as usize - 1];
    if v.used {
        (v.node, v.cmask)
    } else {
        (caprock_hal::numa::Node::Unaffiliated, None)
    }
}

// ----------------------------------------------------------------------------------------------
// C7: DER SEITENTABELLEN-TOPF -- gemessen AN DER QUELLE
// ----------------------------------------------------------------------------------------------
//
// **Warum es diesen Zaehler gibt.** Die Kapazitaetskurve (C7) sagt, DASS es bis 9984 Prozesse
// ging; sie sagt nicht, welcher Vorrat als naechster reisst. Fuer den einen Topf, an dem
// `wasmhost` bei SECHS Programmen wirklich gescheitert ist -- Speicher fuer Seitentabellen --
// gibt es bis heute keine Kurve: eine PD im GLOBALEN Adressraum zieht davon gar nichts (sie
// bekommt keine eigene VSpace), und in der isolierten Kurve erschlaegt die private 2-MiB-Region
// ihn um den Faktor 500.
//
// **Warum an der Quelle und nicht als Differenz des freien RAM.** Eine Differenz misst alles
// andere mit: Kernel-Stacks, private Regionen, Segmente, Slabs. Sie beantwortet die Frage nach
// dem SEITENTABELLEN-Topf so wenig, wie `rx_used` die Frage beantwortet, ob Daten angekommen
// sind. Gezaehlt wird deshalb, was der Allokator FUER eine Seitentabelle herausgibt -- an genau
// den Stellen, die ihn dafuer fragen, und nirgends sonst.
//
// **Was der Zaehler NICHT deckt** (und das gehoert hierher, nicht in eine Fussnote): die
// IOMMU-Tabellen (`VtdEnforcer::alloc_zeroed`, `SmmuV3Enforcer::alloc_zeroed`) sind ein EIGENER
// Topf mit eigener Lebensdauer und stehen bewusst nicht darin. Ein Zaehler, der zwei Toepfe
// addiert, kann keinen von beiden beantworten.
/// Rahmen, die der Allokator fuer eine **CPU-Seitentabelle** herausgegeben hat.
static PT_RAHMEN_RAUS: AtomicU64 = AtomicU64::new(0);
/// ... und die wieder eingesammelt wurden (`vspace_teardown` und die Fehlerpfade des Anlegens).
static PT_RAHMEN_ZURUECK: AtomicU64 = AtomicU64::new(0);
/// Hoechststand der Differenz. Der Endstand allein sagt ueber Erschoepfung nichts -- dieselbe
/// Ueberlegung wie bei den Cap-Hoechststaenden (A-3.4).
static PT_RAHMEN_PEAK: AtomicU64 = AtomicU64::new(0);

/// **Die EINZIGE Stelle, an der der Seitentabellen-Topf waechst.**
///
/// Nimmt den Rueckgabewert des Allokators und gibt die Basis weiter -- der Zaehler haengt damit
/// am Erfolg der Anforderung selbst und nicht an einer Zahl daneben.
fn pt_rahmen(r: Option<MemoryCap>) -> Option<u64> {
    let c = r?;
    let raus = PT_RAHMEN_RAUS.fetch_add(1, Ordering::Relaxed) + 1;
    let zurueck = PT_RAHMEN_ZURUECK.load(Ordering::Relaxed);
    PT_RAHMEN_PEAK.fetch_max(raus.saturating_sub(zurueck), Ordering::Relaxed);
    Some(c.base())
}

/// `n` Seitentabellen-Rahmen sind an den Allokator zurueck.
fn pt_zurueck(n: u64) {
    PT_RAHMEN_ZURUECK.fetch_add(n, Ordering::Relaxed);
}

/// `(herausgegeben, zurueck, gehalten, Hoechststand)` -- in **Rahmen** zu je 4 KiB.
///
/// `gehalten` ist die Differenz und damit der Fuellstand; sie steht neben den beiden Rohwerten,
/// weil eine Differenz allein nicht sagen kann, ob sie klein ist, weil wenig geholt oder viel
/// zurueckgegeben wurde.
pub fn seitentabellen_topf() -> (u64, u64, u64, u64) {
    let raus = PT_RAHMEN_RAUS.load(Ordering::Relaxed);
    let zurueck = PT_RAHMEN_ZURUECK.load(Ordering::Relaxed);
    (
        raus,
        zurueck,
        raus.saturating_sub(zurueck),
        PT_RAHMEN_PEAK.load(Ordering::Relaxed),
    )
}

/// Belegte VSpace-/ASID-Slots -- die **zweite, unabhaengige** Buchfuehrung, gegen die sich der
/// Seitentabellen-Zaehler halten laesst.
///
/// Jede belegte VSpace haelt mindestens ihre L1 und ihre L2 (beide aus [`create_vspace_masked`],
/// beide gezaehlt). Faellt die Zaehlung an der Quelle weg, unterschreitet der Topf diese Schranke
/// -- ohne den Quervergleich waere ein Zaehler, der nur sich selbst befragt, von einem
/// abgeklemmten nicht zu unterscheiden.
pub fn used_vspaces() -> usize {
    VSPACES.lock().iter().filter(|v| v.used).count()
}

/// Rahmen je VSpace, die **jede** Architektur unbedingt anlegt: L1 und L2. x86 legt zusaetzlich
/// eine PDPT an (`vspace_create_base`), aarch64 nicht -- deshalb steht hier die Untergrenze und
/// nicht die x86-Zahl. Ein Pruefer, der die architekturabhaengige Zahl nachrechnet, prueft eine
/// zweite Wirklichkeit (`iova_window_clear_of_msi`).
pub const PT_RAHMEN_JE_VSPACE: u64 = 2;

/// **A-3.4-Telemetrie:** `(Slot-Hoechststand, Slot-Kapazitaet, Objekt-Hoechststand,
/// Objekt-Kapazitaet)` des globalen Cap-Space.
///
/// Der Endstand (`used_slots`) sagt ueber Erschoepfung nichts: ein Lauf, der zwischendurch an die
/// Grenze stiess und danach aufraeumte, sieht hinterher harmlos aus. Die Fairness-Zusage des
/// Cap-Budgets haengt aber am GLEICHZEITIGEN Verbrauch.
/// Kapazität `(Slots, Objekte)` des globalen Capability-Space (Boot-Report/Tests).
pub fn cap_capacity() -> (usize, usize) {
    CAPS.read().cspace.capacity()
}

/// Tatsaechliche PD-Kapazitaet (A-3.4 Teil 3) -- die angehaengte, nicht die Konstante.
pub fn pd_capacity() -> usize {
    CAPS.read().pds.capacity()
}

/// **Wie oft [`purge_ipc_queues`] lief** und **wie viele IPC-Objekte es dabei durchlaufen hat**
/// (C4). Das PAAR ist die Aussage: die Iterationszahl allein waere von „nie gestorben" nicht zu
/// unterscheiden.
pub static PURGE_IPC_CALLS: AtomicU64 = AtomicU64::new(0);
pub static PURGE_IPC_ITER: AtomicU64 = AtomicU64::new(0);

/// **Fuellstand der festen Vorraete** -- (belegte PDs, PD-Kapazitaet, belegte Endpoints,
/// Endpoint-Kapazitaet, belegte Notifications, Notification-Kapazitaet).
///
/// Ein fester Vorrat ohne Fuellstandsanzeige spricht erst, wenn er leer ist -- und dann als
/// `NoResources` an einer Stelle, die mit der Ursache nichts zu tun hat (so ist `wasmhost` bei
/// SECHS Programmen gescheitert). Dieselbe Bewegung wie beim Vektor-Inventar: die Groesse
/// hinschreiben, solange sie noch harmlos ist.
pub fn vorrat_fuellstand() -> (usize, usize, usize, usize, usize, usize) {
    let (pds_used, pds_cap) = {
        let g = CAPS.read();
        (g.pds.used_count(), g.pds.capacity())
    };
    let eps_used = eps().iter().filter(|e| e.lock().is_used()).count();
    let ntfns_used = ntfns().iter().filter(|n| n.lock().is_used()).count();
    (
        pds_used,
        pds_cap,
        eps_used,
        eps().len(),
        ntfns_used,
        ntfns().len(),
    )
}

pub fn cap_peaks() -> (usize, usize, usize, usize) {
    let g = CAPS.read();
    let (ps, cs) = g.cspace.peak_slots();
    let (po, co) = g.cspace.peak_objects();
    (ps, cs, po, co)
}

/// Anzahl freier VSpace-/ASID-Slots (für die Leak-Prüfung des Churn-Tests).
pub fn free_vspaces() -> usize {
    VSPACES.lock().iter().filter(|v| !v.used).count()
}

/// **VMM-Property-Oracle** (read-only, Fuzzer): prüft jede belegte isolierte VSpace auf
/// die **W^X-Invariante** (keine EL0-Seite schreibbar+ausführbar) und strukturelle
/// Konsistenz (L3-Zeiger valide), sowie dass `l1`/`l2` belegter Einträge gesetzt sind.
/// Gibt `0` bei Konsistenz zurück, sonst: 1=W^X-Verletzung, 2=struktureller Defekt
/// (L3-Zeiger), 3=belegte VSpace ohne L1/L2 (inkonsistenter Slot). (ASID-Eindeutigkeit
/// ist durch den VSPACES-Index = ASID-1 baulich garantiert.) Sperrt `VSPACES` kurz und
/// liest die Tabellen über die Identity-Map.
pub fn vspace_audit() -> u32 {
    // Unter dem `VSPACES`-Lock walken: `vspace_wx_ok`/`vspace_device_wx_ok` lesen nur die Tabellen
    // (nehmen KEINE Locks) -> kein Deadlock, und race-frei (kein Teardown während des Walks). Früher
    // wurden die L1/L2-Adressen erst in einem BSS-grossen Puffer auf dem Stack kopiert; das skaliert
    // nicht auf zehntausend VSpaces (Stack-Overflow) und war unnötig, da der Walk lock-frei ist.
    let t = VSPACES.lock();
    for v in t.iter() {
        if !v.used {
            continue;
        }
        if v.l1 == 0 || v.l2 == 0 {
            return 3;
        }
        let code = hal::mmu::vspace_wx_ok(v.l2);
        if code != 0 {
            return code;
        }
        // W^X auch für GiB-0-Device-Mappings (ext-22): keine EL0-Device-Seite ausführbar.
        if !hal::mmu::vspace_device_wx_ok(v.l1) {
            return 1;
        }
    }
    0
}

/// Anzahl belegter TCB-Slots auf `core` (für die Leak-Prüfung).
pub fn used_tcbs(core: usize) -> usize {
    SCHEDS[core].lock().load()
}

/// L2-Tabelle einer (gültigen) isolierten VSpace nachschlagen.
fn vspace_l2(asid: u16) -> Option<u64> {
    if asid == 0 || asid as usize > vspace_slots() {
        return None;
    }
    let v = VSPACES.lock()[asid as usize - 1];
    if v.used {
        Some(v.l2)
    } else {
        None
    }
}

/// Die L1-Wurzel der VSpace `asid` (für Device-MMIO-Mapping in GiB 0).
fn vspace_l1(asid: u16) -> Option<u64> {
    if asid == 0 || asid as usize > vspace_slots() {
        return None;
    }
    let v = VSPACES.lock()[asid as usize - 1];
    if v.used {
        Some(v.l1)
    } else {
        None
    }
}


/// 4-KiB-Granularität: Region `[base, base+len)` (identity) mit `perm` in die VSpace
/// `asid` mappen. 2-MiB-ausgerichtete 2-MiB-Regionen mit RW/RX nutzen den
/// Block-Fastpath; sonst seitenweise (L3 wird bei Bedarf aus `MEM` angelegt).
fn vspace_map(
    asid: u16,
    als_va: &dyn Fn(crate::addr::Pa) -> crate::addr::Va,
    pa: crate::addr::Pa,
    len: u64,
    perm: hal::mmu::UserPerm,
) -> bool {
    vspace_map_masked(asid, als_va, pa, len, perm, None)
}

/// Wie [`vspace_map`], aber die bei Bedarf angelegten L3-Tabellen kommen aus `mask` (todo A1).
fn vspace_map_masked(
    asid: u16,
    als_va: &dyn Fn(crate::addr::Pa) -> crate::addr::Va,
    pa: crate::addr::Pa,
    len: u64,
    perm: hal::mmu::UserPerm,
    mask: Option<caprock_mem::ColorMask>,
) -> bool {
    // **Hier und nur hier** wird aus einer PA eine VA — mit Grund. Der Rest der Funktion rechnet
    // danach mit `base`, weil die HAL an dieser Stelle EINEN Wert nimmt; das ist der Sinn der
    // identischen Abbildung. Der Unterschied zu vorher ist, dass die Gleichsetzung eine
    // benannte Handlung ist und keine Zeile, die man beim Lesen ueberspringt.
    // **Der Aufrufer reicht den KONSTRUKTOR, nicht einen Grund.** Ein Grund waere ein Wert, den
    // man verwechseln kann (`Va::identity(Mmio, dma_pa)` haette der Waechter durchgewinkt); ein
    // Konstruktor traegt seine Stelle im Namen. Die Bindung Stelle<->Grund ist damit typgeprueft
    // statt disziplingeprueft.
    let base = als_va(pa).raw();
    let Some(l2) = vspace_l2(asid) else {
        return false;
    };
    let ok = if len == hal::mmu::ISO_REGION_SIZE
        && base % hal::mmu::ISO_REGION_SIZE == 0
        && perm != hal::mmu::UserPerm::Ro
    {
        match perm {
            hal::mmu::UserPerm::Rx => hal::mmu::vspace_map_code_block(l2, base),
            _ => hal::mmu::vspace_map_block(l2, base),
        }
    } else {
        let mut all = true;
        let mut p = base;
        while p < base + len {
            let mapped =
                hal::mmu::vspace_map_page(l2, p, perm, &mut || {
                    // Abbildungszeitpunkt, kein Ladezeitpunkt: hier fliesst keine
                    // `LadePolitik` (MAP-Syscall, Spawn-Wege ohne Manifest) — also
                    // ungefragt unaffiliated; die Knotenpolitik lebt im Ladepfad.
                    pt_rahmen(mem_alloc_masked(
                        4096,
                        4096,
                        mask,
                        caprock_hal::numa::Node::Unaffiliated,
                    ))
                });
            if !mapped {
                all = false;
                break;
            }
            p += 4096;
        }
        all
    };
    if ok {
        hal::mmu::flush_asid(asid);
    }
    ok
}

// ================================================================================================
// Zeugen fuer die identischen Abbildungen (E-Rest 3g, 2026-08-05)
// ================================================================================================
//
// **Die Bindung Stelle<->Grund haelt jetzt rustc, nicht mehr eine Tabelle im Pruefskript.**
//
// `Va::for_mmio_window` war eine oeffentliche Methode: der DMA-Pfad *koennte* sie rufen, und das
// haette nur ein Skript gemerkt, das Funktionsnamen aus dem Quelltext liest. Der Einwand dagegen
// war richtig -- und mein Gegenargument („`pub(in path)` verlangt einen Vorfahren, `Va` liegt in
// `crate::addr`") ging am Punkt vorbei: der ZEUGE braucht keinen Vorfahren.
//
// Jeder dieser Typen hat ein **privates** Feld und ist damit nur in DIESEM Modul herstellbar.
// `crate::addr` kann ihn nennen (er ist `pub`), aber nicht erzeugen. Wer die falsche Umwandlung
// nehmen will, muss den Zeugen dafuer haben -- und den gibt es an seiner Stelle nicht.
//
// Kosten: eine Zeile je Engstelle. Ich hatte das als „Entwurfsarbeit" eingestuft; dieselbe
// Fehleinstufung, die `tail -1` neben Entwurfsarbeit geparkt hat.

/// Zeuge fuer `SYS_MAP`/`SYS_UNMAP` (`map_frame`/`unmap_frame`).
pub struct SyscallMapWitness(());
/// Zeuge fuer das kernel-seitige Einblenden (`map_into_thread`/`unmap_into_thread`).
pub struct KernelSetupWitness(());
/// Zeuge fuer MMIO-Registerfenster (`map_region_into_thread`/`unmap_window`).
pub struct MmioWindowWitness(());
/// Zeuge fuer DMA-Fenster (`map_region_into_thread`/`unmap_dma_from_thread`).
pub struct DmaWindowWitness(());
/// Zeuge fuer globale Kernel-Geraetefenster (ECAM, BAR beim Hochlauf).
pub struct KernelGlobalWindowWitness(());

/// **Ein BAR-Fenster global einblenden**, damit der Kernel enumerieren kann (kein Subjekt
/// beteiligt).
///
/// Diese Funktion gibt es, weil `bringup` den Zeugen nicht herstellen kann -- und das ist der
/// Beleg dafuer, dass die Bindung wirkt: der erste Bauversuch scheiterte genau hier mit
/// „argument #1 of type `KernelGlobalWindowWitness` is missing". Statt den Zeugen oeffentlich
/// konstruierbar zu machen (was ihn wertlos machte), wandert der Aufruf hierher. Nebenertrag:
/// die Schichtung stimmt danach besser -- `bringup` sagt, WAS eingeblendet werden soll, `system`
/// entscheidet, unter welcher Achse.
#[cfg(target_arch = "x86_64")]
pub fn map_device_window_global(base: u64, len: u64) -> bool {
    let _ = crate::addr::Va::for_kernel_global_window(
        KernelGlobalWindowWitness(()),
        crate::addr::Pa::new(base),
    );
    hal::mmu::map_device_window_global(base, len)
}

/// **Die Platzbelegung des User-Fensters.** Zwei benannte Plaetze statt zweier Zahlen im
/// Aufruf: `spawn_isolated_native` braucht beide, und „Slot 0 ist der Code" ist genau die Sorte
/// Wissen, die sonst in zwei Dateien halb steht.
const SLOT_CODE: usize = 0;
/// Daten bzw. Stack. Bei `spawn_isolated`/`_colored` ist es der einzige belegte Platz.
const SLOT_DATA: usize = 1;

/// **Die private Region einer isolierten PD in ihr User-Fenster abbilden** (E-Rest 3d).
///
/// Gibt die **virtuelle** Adresse zurück, unter der die PD sie sieht. Die Tabellen des Fensters
/// kommen aus demselben Farbstreifen wie alles andere dieser PD, wenn einer vorgegeben ist —
/// sonst wäre A1 an genau der Stelle durchlöchert, an der eine neue Tabelle dazukommt.
fn vspace_map_user_region(
    asid: u16,
    slot: usize,
    phys: u64,
    len: u64,
    perm: hal::mmu::UserPerm,
    mask: Option<caprock_mem::ColorMask>,
) -> Option<u64> {
    let l1 = vspace_l1(asid)?;
    // **Der Allokator markiert sich SELBST** -- dieselbe Form wie im Ladepfad (`a3`). Ein
    // Aufrufer, der hinterher nachrechnet, WARUM das Abbilden scheiterte, baut eine zweite
    // Wirklichkeit neben die erste (`iova_window_clear_of_msi`).
    let mut alloc = || {
        // Wie oben: Abbildungszeitpunkt ohne Ladepolitik — unaffiliated.
        let r = pt_rahmen(mem_alloc_masked(
            4096,
            4096,
            mask,
            caprock_hal::numa::Node::Unaffiliated,
        ));
        if r.is_none() {
            mangel(MANGEL_SEITENTABELLE, 4096);
        }
        r
    };
    let va = hal::mmu::vspace_map_user_window(l1, slot, phys, len, perm, &mut alloc);
    let Some(va) = va else {
        // Nur wenn der Allokator NICHTS gemeldet hat, lag es am Abbilden selbst -- und dann ist
        // die ehrliche Antwort „KEINE Ressource", nicht die naechstbeste Zahl.
        if lade_mangel().0 == MANGEL_KEINER {
            mangel(MANGEL_MAPPING_ABGEWIESEN, 0);
        }
        return None;
    };
    hal::mmu::flush_asid(asid);
    Some(va)
}

/// 4-KiB-Granularität: Region `[base, base+len)` aus der VSpace `asid` entfernen.
fn vspace_unmap(
    asid: u16,
    als_va: &dyn Fn(crate::addr::Pa) -> crate::addr::Va,
    pa: crate::addr::Pa,
    len: u64,
) -> bool {
    // Muss dieselbe Achse benutzen wie das Mappen, sonst raeumt es an der falschen Stelle ab --
    // deshalb dieselbe benannte Umwandlung und nicht etwa ein roher Wert.
    let base = als_va(pa).raw();
    let Some(l2) = vspace_l2(asid) else {
        return false;
    };
    let ok = if len == hal::mmu::ISO_REGION_SIZE && base % hal::mmu::ISO_REGION_SIZE == 0 {
        hal::mmu::vspace_unmap_block(l2, base)
    } else {
        let mut all = true;
        let mut p = base;
        while p < base + len {
            if !hal::mmu::vspace_unmap_page(l2, p) {
                all = false;
            }
            p += 4096;
        }
        all
    };
    if ok {
        hal::mmu::flush_asid(asid);
    }
    ok
}

/// Kernel-Setup: Region `[base, base+len)` mit `perm_code` (0=Ro,1=Rw,2=Rx) in die
/// VSpace des Threads `tid` mappen (für Demos, die vorab feingranulare Seiten — z. B.
/// RW/RO/Guard — anlegen wollen). No-Op für nicht-isolierte Threads.
pub fn map_into_thread(tid: ThreadId, base: u64, len: u64, perm_code: u8) -> bool {
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    if asid == 0 {
        return false;
    }
    let perm = match perm_code {
        2 => hal::mmu::UserPerm::Rx,
        1 => hal::mmu::UserPerm::Rw,
        _ => hal::mmu::UserPerm::Ro,
    };
    vspace_map(
        asid,
        &|pa| crate::addr::Va::for_kernel_setup(KernelSetupWitness(()), pa),
        crate::addr::Pa::new(base),
        len,
        perm,
    )
}

/// Kernel-Setup-Gegenstück zu [`map_into_thread`]: `[base, base+len)` aus der VSpace
/// des Threads `tid` wieder entfernen (Seiten auf EL1-only, TLB-Flush). Für den
/// Fuzzer/Tests, um den per-Seite-Unmap-Pfad explizit zu fahren. No-Op (false) für
/// nicht-isolierte Threads.
#[cfg_attr(not(feature = "kernel-fuzz"), allow(dead_code))] // nur vom Fuzzer benutzt (ADR 0013)
pub fn unmap_into_thread(tid: ThreadId, base: u64, len: u64) -> bool {
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    if asid == 0 {
        return false;
    }
    vspace_unmap(
        asid,
        &|pa| crate::addr::Va::for_kernel_setup(KernelSetupWitness(()), pa),
        crate::addr::Pa::new(base),
        len,
    )
}

/// Eine isolierte VSpace abbauen: alle per-PD-L3-Tabellen **und** L1+L2 an `MEM`
/// zurückgeben, Eintrag freigeben, TLB der ASID flushen. Beim Thread-Ende einer
/// isolierten PD aufzurufen.
fn vspace_teardown(asid: u16) {
    if asid == 0 || asid as usize > vspace_slots() {
        return;
    }
    loaded_free(asid); // ext-26 L4: geladene Programm-Segment-Frames freigeben (sonst Leck)
    // Eintrag NUR LESEN (Slot noch NICHT freigeben): der Kontextwechsel flusht nicht, sondern
    // verlässt sich auf ASID-Tagging. Würde der Slot vor `flush_asid` freigegeben, könnte ein
    // anderer Kern die ASID via `create_vspace` sofort neu vergeben + benutzen, WÄHREND die alten
    // TLB-Einträge dieser ASID noch existieren -> das neue Programm sähe fremde Mappings. Auf realer
    // HW mit 16-Bit-ASIDs reproduzierbar (geladene PDs liefen falsch); 8-Bit maskierte es. Reihenfolge:
    // 1) TLB der ASID leeren, 2) Frames freigeben, 3) Slot freigeben (ab dann reusable).
    let ent = { VSPACES.lock()[asid as usize - 1] };
    if ent.used {
        hal::mmu::flush_asid(asid); // 1. TLB für diese ASID leeren (Tabellen noch intakt)
        let mut mem = MEM.lock();
        // **Die Freigabeseite des Seitentabellen-Topfs** (C7). Gezaehlt wird, was hier
        // tatsaechlich zurueckgeht -- die drei Einsammler liefern je Aufruf eine unbekannte
        // Anzahl, also zaehlt der Rueckruf selbst und nicht eine Schaetzung daneben.
        let mut zurueck = 0u64;
        // 2. feingranulare L3-Tabellen (aus Seiten-Mappings) einsammeln.
        hal::mmu::vspace_collect_l3s(ent.l2, &mut |p| {
            mem.free_region(PhysRegion::new(p, 4096));
            zurueck += 1;
        });
        // GiB-0-Device-Tabellen (aus MMIO-Mappings, ext-22) einsammeln, falls vorhanden.
        hal::mmu::vspace_collect_device_tables(ent.l1, &mut |p| {
            mem.free_region(PhysRegion::new(p, 4096));
            zurueck += 1;
        });
        // Die Tabellen des privaten User-Fensters (E-Rest 3d). Sie haengen an einem eigenen
        // obersten Eintrag und werden von keiner der beiden Zeilen darueber beruehrt -- ohne
        // diese waere jede isolierte PD ein Leck von zwei bis drei Rahmen.
        hal::mmu::vspace_collect_user_window(ent.l1, &mut |p| {
            mem.free_region(PhysRegion::new(p, 4096));
            zurueck += 1;
        });
        mem.free_region(PhysRegion::new(ent.l1, 4096));
        mem.free_region(PhysRegion::new(ent.l2, 4096));
        drop(mem);
        // `+ 2` sind L1 und L2 aus den beiden Zeilen darueber. Die PDPT der x86-Fassung zaehlt
        // NICHT hier, sondern im Rueckruf von `vspace_collect_device_tables` -- die gibt sie
        // selbst frei. Wer sie hier noch einmal addierte, buchte einen Rahmen zweimal aus, und
        // der Fuellstand liefe langsam ins Negative.
        pt_zurueck(zurueck + 2);
    }
    // 3. Slot erst JETZT freigeben (nach Flush) -> keine Wiederverwendung mit veralteten Einträgen.
    VSPACES.lock()[asid as usize - 1] = VSpaceEnt::FREI;
    // Herkunft und Farbe fallen mit dem Slot: wer ihn neu vergibt, erbt keinen Knoten und
    // keinen Farbsatz (ein geerbter Knoten waere eine Platzierungszusage, die niemand gab).
    // 4. Farbstreifen zuletzt (B-4.2). **Nach** dem Slot, nicht davor: gäbe man den Streifen frei,
    // solange diese VSpace noch steht, könnte eine neue PD ihn belegen und läge kurzzeitig auf
    // denselben Farben wie eine noch existierende — genau die Überschneidung, die B-4.2 verhindern
    // soll, nur in einem schmalen Fenster statt dauerhaft.
    if let Some(i) = ent.stripe {
        crate::colors::release_stripe(i);
    }
}

/// Den belegten Farbstreifen an die Lebensdauer einer VSpace binden (B-4.2).
///
/// Getrennt von `create_vspace_masked`, weil dort nur die *Maske* bekannt ist, nicht die
/// Streifennummer — und die Freigabe braucht die Nummer. Wird der Aufruf vergessen, ist der
/// Streifen für immer belegt: nach vier vergessenen PDs entsteht keine gefärbte mehr. Deshalb
/// steht er unmittelbar neben dem `claim_stripe()` seines einzigen Aufrufers.
fn vspace_bind_stripe(asid: u16, stripe: u32) {
    if asid == 0 || asid as usize > vspace_slots() {
        return;
    }
    VSPACES.lock()[asid as usize - 1].stripe = Some(stripe);
}

/// Einen **isolierten EL0-User-Thread** erzeugen (Weg C): eigene VSpace, die nur den
/// Kernel (EL1-only) + eine **private 2-MiB-Stack-Region** (EL0-RW) mappt. Sein Code
/// ist die geteilte `.user_text` (EL0-RX in jeder VSpace). Greift er auf **fremdes**
/// User-RAM zu, ist das in seiner VSpace EL1-only -> Fault -> Kernel beendet ihn.
/// Zur Laufzeit kann er weitere Frames per `MAP`-Syscall (cap-gated) hinzunehmen.
/// Gibt `(ThreadId, region_base)` — die **Physadresse**; der Thread sieht die Region unter
/// [`hal::mmu::ISO_USER_VA`]. Bis E-Rest 3d war beides dasselbe. Die Kernelseite braucht die PA
/// (Farbprüfung, Rücklesen), der Thread die VA — sie auseinanderzuhalten ist der ganze Umbau.
/// VSpace-Tabellen werden beim Thread-Ende abgebaut.
pub fn spawn_isolated_parked(entry: usize, arg: usize, prio: u8) -> Option<(Parked, u64)> {
    let core = hal::cpu::core_id();
    mangel_zuruecksetzen();
    let kbase = claim_user_kstack()?; // EL0-Kernel-Stack aus MEM (16 KiB, ausgerichtet)

    // **Private 2-MiB-Stack-Region -- seit E-Rest 3d ohne Zonenbindung.** Sie wird nicht mehr
    // identisch abgebildet, sondern in das private User-Fenster dieser PD
    // (`hal::mmu::ISO_USER_VA`, ausserhalb der Identitaetskarte). Damit ist die PHYSADRESSE frei,
    // und der GiB-0-Deckel von gemessenen 504 gleichzeitigen isolierten PDs faellt.
    // **`PRIV_REGION_SIZE` und nicht `ISO_REGION_SIZE`** (C7b): die eine Konstante trug beide
    // Bedeutungen, und sie waren nur solange dieselbe Zahl, wie die Region genau ein
    // 2-MiB-Blockdeskriptor war. Hier ist die Groesse eines STACKS gemeint, nicht die eines
    // Blocks.
    let region_sz = hal::mmu::PRIV_REGION_SIZE;
    let (rbase, rlen) = match benannt_alloc(MANGEL_PRIVATREGION, region_sz, |n| {
        mem_alloc_anywhere(n, n)
    }) {
        Some(r) => (r.base(), r.len()),
        None => {
            release_user_kstack(kbase);
            return None;
        }
    };
    // `create_vspace_masked` benennt selbst, WELCHER der beiden Toepfe leer war (ASID-Platz oder
    // Seitentabellen-Speicher) -- deshalb hier keine zweite, groebere Meldung darueber.
    let Some((asid, l1)) = create_vspace() else {
        MEM.lock().free_region(PhysRegion::new(rbase, rlen));
        release_user_kstack(kbase);
        return None;
    };
    // Stack-Region in das private User-Fenster mappen (EL0-RW, nicht-identisch).
    let Some(rva) = vspace_map_user_region(asid, SLOT_DATA, rbase, rlen, hal::mmu::UserPerm::Rw, None)
    else {
        vspace_teardown(asid);
        MEM.lock().free_region(PhysRegion::new(rbase, rlen));
        release_user_kstack(kbase);
        return None;
    };
    let packed = ((asid as u64) << 48) | l1;

    // **`spawn_user_at` statt `spawn_user`, und das ist der Kern des Umbaus.** `spawn_user`
    // nimmt EINEN Wert fuer zwei Dinge: den EL0-Stackzeiger und die Reap-Region, die beim
    // Thread-Tod an den Allokator zurueckgeht. Solange VA == PA galt, war das dieselbe Zahl;
    // jetzt sind es zwei, und die Verwechslung ist teuer -- gemessen als `#PF cr2=0x80_0000_0000`
    // im KERNEL, weil der Reap-Pfad eine virtuelle Adresse als Physadresse freigab.
    let tid = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_user_at_parked(
            core,
            entry,
            arg,
            kbase,
            USER_KSTACK_SIZE,
            (rva + rlen) as usize, // EL0-SP: virtuell, waechst nach unten
            rbase as usize,        // Reap: physisch
            rlen as usize,
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot()); // FP + VSPACE_OF[slot]=0 (global) zurücksetzen
            // Buchfuehrung NOCH UNTER SCHEDS -- s. Kommentar an `record_user_kstack`.
            record_user_kstack(t, kbase); // Kstack dem Thread zuordnen (reclaim!)
            // C7b: DIE private Region -- der EL0-Stack dieses Threads und der groesste
            // Einzelposten je Mandant.
            record_user_region(t.slot(), rbase, rlen);
            set_vspace_of(t.slot(), packed); // ab jetzt isoliert (nach fp_reset_slot!)
        }
        r
    };
    match benannt_slot(MANGEL_THREAD_SLOT, tid) {
        Some(t) => Some((Parked(t), rbase)),
        None => {
            vspace_teardown(asid);
            MEM.lock().free_region(PhysRegion::new(rbase, rlen));
            release_user_kstack(kbase);
            None
        }
    }
}

/// **Farbige Variante von [`spawn_isolated`]** (todo A1): die private Region der PD wird aus
/// einem Farbsatz alloziert, der zu keinem anderen Satz überlappt — zwei so erzeugte PDs können
/// sich im Last-Level-Cache nicht gegenseitig verdrängen und damit auch nicht über Laufzeit
/// beobachten.
///
/// **Warum eine eigene Funktion und nicht ein Schalter in [`spawn_isolated`]:** dort ist die
/// Region 2 MiB groß und wird als *ein* Blockdeskriptor gemappt. 2 MiB sind 512 Seiten, also 512
/// aufeinanderfolgende Farben — bei den auf dem Testaufbau gemessenen 256 Farben überstreicht ein
/// einziger Block jede Farbe zweimal. Der Blockdeskriptor und die Färbung schließen einander
/// aus; das ist keine Nachlässigkeit, sondern Arithmetik. Diese Funktion nimmt deshalb die
/// kleinere Region ([`colors::region_bytes`]) und den **seitenweisen** Mapping-Pfad, der ohnehin
/// existiert. Der Preis steht damit im Code und nicht in einer Fußnote: statt eines PTE nun
/// `region_bytes / 4 KiB` Stück, plus eine L3-Tabelle.
///
/// **Was hier NICHT gefärbt ist:** der Kernel-Stack der PD (16 KiB aus dem Pool) und die
/// Seitentabellen. Beide gehören dem Kernel, nicht dem Subjekt; die Zusicherung lautet
/// ausdrücklich „die *private Region* zweier PDs teilt keine Cache-Farbe", nicht „die PDs teilen
/// keinerlei Cache-Zeile".
///
/// `None`, wenn die Maske leer ist, kein passend gefärbter Speicher frei ist, oder die
/// VSpace/Thread-Erzeugung scheitert. Gibt `(ThreadId, region_base)`.
/// Gibt zusaetzlich die **Kernelseite** `(kstack_base, l1, l2)` zurueck — und zwar so, wie sie beim
/// Anlegen war, nicht wie sie beim Nachsehen noch ist.
///
/// Das ist kein Komfort, sondern der Kern einer Fehlerbehebung. Der Selbsttest las diese drei Werte
/// bis 2026-08-01 ueber `testsupport::kstack_of`/`vspace_tables_of` **zurueck** — aus Tabellen, die
/// an der Lebendigkeit des Threads haengen. Der Einsprungpunkt der Sonde faultet ABSICHTLICH; wird
/// sie eingesammelt, bevor der Test liest, liefern alle drei 0. Gemessen: 5 von 500 Laeufen mit
/// `rueckgelesen=0 (kstack=0 l1=0 l2=0)` bei sonst fehlerfreier Zuteilung, und im Protokoll steht
/// die Ursache woertlich davor — `el0-trap: User-Thread 0x8 faultete ... -> beendet`.
///
/// Frueher zu lesen half nicht (gemessen: 4/400 -> 3/500 -> 5/500, alles Rauschen): das Fenster
/// beginnt, sobald `spawn` zurueckkehrt. Deshalb werden die Werte **hier drin** genommen, direkt
/// nach `create_vspace_masked` und **bevor es einen Thread gibt**, der sterben koennte.
pub fn spawn_isolated_colored(
    entry: usize,
    arg: usize,
    prio: u8,
    mask: caprock_mem::ColorMask,
) -> Option<(ThreadId, u64, (u64, u64, u64))> {
    // Ohne Streifennummer: der Aufrufer hat die Maske selbst gewaehlt (Selbsttest) und fuehrt
    // keine Belegung. Fuer PDs ist `spawn_isolated_colored_auto` der richtige Weg.
    //
    // **Zulassen nicht vergessen** (D0): `spawn_isolated_colored_inner` liefert seit dem
    // 2026-08-07 einen GEPARKTEN Thread. Diese Fassung bindet keine PD nach, also darf sie ihn
    // sofort loslassen -- aber „sofort" muss dastehen, sonst laeuft die Farbsonde nie an.
    mangel_zuruecksetzen();
    let (p, r, ks) = spawn_isolated_colored_inner(entry, arg, prio, mask, None)?;
    admit(p).map(|t| (t, r, ks))
}

/// **Gefaerbte PD mit gefuehrter Streifenvergabe** (B-4.2) — der Weg, den B-4.1 zum Normalfall
/// macht.
///
/// Belegt einen freien Farbstreifen, bindet ihn an die Lebensdauer der VSpace und gibt ihn bei
/// jedem Fehlschlag wieder frei. **`None` heisst: kein Streifen frei** — und dann entsteht die PD
/// gar nicht erst. Es gibt bewusst keine ungefaerbte Rueckfallebene: eine Trennung, die unter
/// Last leise verschwindet, waere schlimmer als gar keine, weil niemand mehr weiss, welche PD
/// getrennt ist und welche nicht.
pub fn spawn_isolated_colored_auto_parked(entry: usize, arg: usize, prio: u8) -> Option<(Parked, u64)> {
    mangel_zuruecksetzen();
    let (i, mask) = benannt_slot(MANGEL_FARBSTREIFEN, crate::colors::claim_stripe())?;
    match spawn_isolated_colored_inner(entry, arg, prio, mask, Some(i)) {
        // Die Kernelseite interessiert nur den Selbsttest (s. `spawn_isolated_colored`).
        Some((t, rbase, _kernelseite)) => Some((t, rbase)),
        None => {
            // Scheitert das Anlegen NACH dem Belegen, muss der Streifen zurueck -- sonst ist er
            // fuer immer weg, und nach vier Fehlschlaegen gibt es keine gefaerbte PD mehr.
            crate::colors::release_stripe(i);
            None
        }
    }
}

fn spawn_isolated_colored_inner(
    entry: usize,
    arg: usize,
    prio: u8,
    mask: caprock_mem::ColorMask,
    stripe: Option<u32>,
) -> Option<(Parked, u64, (u64, u64, u64))> {
    let core = hal::cpu::core_id();
    let _colors = crate::colors::count();
    let region_sz = crate::colors::region_bytes();
    // Region, Kernel-Stack UND Seitentabellen dieser PD kommen aus demselben Streifen.
    // Spawn-Wege tragen keinen Manifest-Knoten (keine Ladepolitik) — unaffiliated; die
    // Knotenpolitik lebt ausschliesslich im Ladepfad (`load_into_pd_mit_va`).
    let kbase =
        claim_user_kstack_masked(Some(mask), caprock_hal::numa::Node::Unaffiliated)?;

    // **Seit E-Rest 3d ohne Zonenbindung:** die Region wird nicht mehr identisch abgebildet,
    // sondern in das private User-Fenster dieser PD. Die Farbbedingung bleibt -- sie ist eine
    // Aussage ueber die PHYSADRESSE und von der virtuellen Lage unberuehrt.
    let cap = match benannt_alloc(MANGEL_PRIVATREGION, region_sz, |n| {
        alloc_colored_anywhere(n, crate::colors::PAGE, mask)
    }) {
        Some(c) => c,
        None => {
            release_user_kstack(kbase);
            return None;
        }
    };
    let (rbase, rlen) = (cap.base(), cap.len());
    zero_phys(rbase, rlen); // wie `mem_alloc`: nichts geht ungenullt an ein Subjekt
    let Some((asid, l1)) =
        create_vspace_masked(Some(mask), caprock_hal::numa::Node::Unaffiliated)
    else {
        MEM.lock().free_region(PhysRegion::new(rbase, rlen));
        release_user_kstack(kbase);
        return None;
    };
    // **Die Kernelseite JETZT festhalten** -- hier gibt es noch keinen Thread, der faulten und
    // eingesammelt werden koennte, also ist der Wert stabil. Zurueckgelesen waere er es nicht:
    // `kstack_of`/`vspace_tables_of` haengen an der Lebendigkeit des Threads. Siehe die Doku an
    // `spawn_isolated_colored`. `l2` steht in `VSPACES`, gesetzt von `create_vspace_masked`;
    // der Zugriff nimmt keinen SCHEDS-Lock -- Sperrordnung VSPACES < SCHEDS bleibt gewahrt.
    let kernelseite = {
        let e = VSPACES.lock()[asid as usize - 1];
        (kbase as u64, l1, e.l2)
    };
    // Ab hier gibt `vspace_teardown(asid)` den Streifen selbst zurueck -- deshalb sofort binden
    // und nicht erst am Ende: jeder Fehlerausgang unten ruft teardown.
    if let Some(i) = stripe {
        vspace_bind_stripe(asid, i);
    }
    // Seitenweise statt Block — die gefaerbte Region ist kleiner als ein 2-MiB-Block. Die L3
    // entsteht im User-Fenster und kommt aus demselben Farbstreifen; `vspace_teardown` sammelt
    // sie ueber `vspace_collect_user_window` wieder ein.
    let Some(rva) = vspace_map_user_region(asid, SLOT_DATA, rbase, rlen, hal::mmu::UserPerm::Rw, Some(mask))
    else {
        vspace_teardown(asid);
        MEM.lock().free_region(PhysRegion::new(rbase, rlen));
        release_user_kstack(kbase);
        return None;
    };
    let packed = ((asid as u64) << 48) | l1;

    // EL0-SP virtuell, Reap-Region physisch -- s. `spawn_isolated`.
    let tid = {
        let mut sched = SCHEDS[core].lock();
        let r = sched.spawn_user_at_parked(
            core,
            entry,
            arg,
            kbase,
            USER_KSTACK_SIZE,
            (rva + rlen) as usize,
            rbase as usize,
            rlen as usize,
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot());
            // Buchfuehrung NOCH UNTER SCHEDS -- s. Kommentar an `record_user_kstack`.
            record_user_kstack(t, kbase);
            // C7b: die gefaerbte private Region (`colors::region_bytes()`).
            record_user_region(t.slot(), rbase, rlen);
            set_vspace_of(t.slot(), packed); // nach fp_reset_slot!
        }
        r
    };
    match benannt_slot(MANGEL_THREAD_SLOT, tid) {
        Some(t) => Some((Parked(t), rbase, kernelseite)),
        None => {
            vspace_teardown(asid);
            MEM.lock().free_region(PhysRegion::new(rbase, rlen));
            release_user_kstack(kbase);
            None
        }
    }
}

/// Eine isolierte PD **vollständig abbauen** (für den Churn-/Leak-Test bzw. den
/// EXIT einer isolierten PD): den (nicht laufenden) Thread `tid` beenden + seinen
/// Stack einsammeln (an `MEM`), die VSpace abbauen (L1/L2/L3 an `MEM`, ASID-Slot
/// zurück), den Kernel-Stack-Pool-Slot zurückgeben und `VSPACE_OF` leeren. Gibt
/// damit **alle** PD-Ressourcen frei. `tid` muss auf dem aktuellen Kern liegen und
/// darf nicht der laufende Thread sein.
pub fn destroy_isolated(tid: ThreadId) {
    let core = hal::cpu::core_id();
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    SCHEDS[core].lock().kill(tid, core); // ready -> Zombie (TCB-Slot frei, Stack vorgemerkt)
    purge_ipc_queues(tid); // eager: tote IPC-Queue-Einträge vermeiden
    // **Erst alles erledigen, was `tid.slot()` braucht, DANN reapen** (D15, 2026-08-13).
    //
    // `reap()` ruft `release_gid` -- ab diesem Befehl darf die `gid` an einen anderen Thread gehen,
    // und die Freiliste ist LIFO: der eben freigegebene Slot ist der **naechste ausgegebene**. Die
    // beiden Zeilen darunter halten aber nur den Slot in der Hand, nicht die Identitaet; sie
    // traefen dann den Nachfolger. `vspace_teardown` dazwischen ist obendrein der teuerste Schritt
    // des ganzen Abbaus -- das Fenster war also nicht schmal, sondern das breiteste im Pfad.
    if asid != 0 {
        vspace_teardown(asid); // L1/L2/L3 + geladene Segmente an MEM, ASID-Slot frei, TLB-Flush
        set_vspace_of(tid.slot(), 0);
    }
    reclaim_user_kstack(tid, "destroy_isolated");
    reap(); // Stack-Zombie an MEM zurueck (korrekte Lock-Ordnung: SCHEDS frei, dann MEM)
}

/// Einen **geladenen Prozess vollständig abbauen** (ext-26, L4): die im Cspace seiner PD
/// installierten Caps löschen (delegierte CDT-Kopien → Refcount runter), den Thread + die VSpace +
/// die geladenen Segment-Frames + den Kernel-Stack abbauen ([`destroy_isolated`]) und den PD-Slot
/// freigeben. Danach ist die Ressourcen-Baseline wiederhergestellt (kein Leck). `tid` muss auf dem
/// aktuellen Kern liegen und darf nicht der laufende Thread sein.
pub fn destroy_loaded(tid: ThreadId, pd: usize) {
    let caps = CAPS.read().pds.caps_of(pd); // Snapshot; Read-Lock danach frei
    for cap in caps.iter().flatten() {
        let _ = cap_delete(*cap); // jeden installierten Cap löschen (CAPS.write intern)
    }
    destroy_isolated(tid);
    CAPS.write().pds.free(pd); // PD-Slot freigeben
}

/// Eine PD **ohne Thread** abbauen: alle in ihrem Cspace installierten Caps löschen
/// (Refcount runter, ggf. Finalisierung) und den PD-Slot freigeben. Für PDs, die nur als
/// Cap-Container existieren (Setup-Fehlerpfade, Selbsttests); PDs **mit** Thread bauen
/// [`destroy_loaded`] bzw. der PDCTL-STOP-Pfad ab.
pub fn destroy_pd(pd: usize) {
    // Der Abbau einer PD ist **der** realistische Weg in „quiesziert nie": das Gerät hängt, und
    // niemand ist mehr da, der auf einen Fehlercode reagieren könnte. Hier darf der
    // Pending-Zustand entstehen, ohne als Anomalie gezählt zu werden — überall sonst will man
    // ihn sehen (`dma_pending_stats().1`).
    let _kill = KillScope::enter();
    let caps = CAPS.read().pds.caps_of(pd); // Snapshot; Read-Lock danach frei
    for cap in caps.iter().flatten() {
        let _ = cap_delete(*cap);
    }
    CAPS.write().pds.free(pd);
}


/// Eine isolierte PD mit **privat geladenem nativem Code** erzeugen: der Code
/// `[code, code+code_len)` wird in einen frischen Frame kopiert, der **EL0-RX** in
/// die eigene VSpace gemappt wird (W^X; nicht die geteilte `.user_text`). Dazu ein
/// privater EL0-RW-Stack. Der Thread startet am Anfang des Code-Frames. Beweist, dass
/// eine isolierte PD beliebigen, nicht-geteilten Code in ihrer eigenen VSpace
/// ausführt. Gibt die `ThreadId`.
pub fn spawn_isolated_native_parked(code: *const u8, code_len: usize, prio: u8) -> Option<Parked> {
    let core = hal::cpu::core_id();
    mangel_zuruecksetzen();
    let kbase = claim_user_kstack()?; // EL0-Kernel-Stack aus MEM (16 KiB, ausgerichtet)
    // C7b: Code- **und** Stack-Frame nehmen die Groesse der privaten Region. Fuer den Code ist
    // das eine Obergrenze fuer `code_len` (unten geprueft), fuer den Stack die gemessene Groesse.
    let sz = hal::mmu::PRIV_REGION_SIZE;

    // Code-Frame + Stack-Frame. **Seit dem VA==PA-Durchgang ohne Zonenbindung:** beide gehen
    // in das private User-Fenster (Plaetze `SLOT_CODE`/`SLOT_DATA`), nicht mehr identisch in
    // GiB 0. Das war der letzte Spawn-Pfad, der die Identitaet noch vorausgesetzt hat.
    let ca = benannt_alloc(MANGEL_PRIVATREGION, sz, |n| mem_alloc_anywhere(n, n));
    let sa = benannt_alloc(MANGEL_PRIVATREGION, sz, |n| mem_alloc_anywhere(n, n));
    let (cf, sf) = match (ca, sa) {
        (Some(cf), Some(sf)) => (cf, sf),
        (ca, sa) => {
            let mut mem = MEM.lock();
            if let Some(c) = ca {
                mem.free_region(PhysRegion::new(c.base(), c.len()));
            }
            if let Some(c) = sa {
                mem.free_region(PhysRegion::new(c.base(), c.len()));
            }
            drop(mem);
            release_user_kstack(kbase);
            return None;
        }
    };
    let (cbase, clen) = (cf.base(), cf.len());
    let (sbase, slen) = (sf.base(), sf.len());
    if code_len > clen as usize {
        let mut mem = MEM.lock();
        mem.free_region(PhysRegion::new(cbase, clen));
        mem.free_region(PhysRegion::new(sbase, slen));
        drop(mem);
        release_user_kstack(kbase);
        return None;
    }

    // Code in den Frame kopieren (globale Map: cbase ist EL0+EL1-RW -> beschreibbar),
    // dann I-Cache kohärent machen, bevor er ausgeführt wird.
    // SAFETY: `cbase` ist ein frisch allozierter, identity-gemappter RW-Frame; wir
    // kopieren genau `code_len` (<= clen) Bytes aus dem gültigen Quellpuffer.
    unsafe {
        core::ptr::copy_nonoverlapping(code, cbase as *mut u8, code_len);
    }
    hal::cpu::sync_code_range(cbase as usize, code_len);

    let Some((asid, l1)) = create_vspace() else {
        let mut mem = MEM.lock();
        mem.free_region(PhysRegion::new(cbase, clen));
        mem.free_region(PhysRegion::new(sbase, slen));
        drop(mem);
        release_user_kstack(kbase);
        return None;
    };
    // Code EL0-RX (W^X), Stack EL0-RW -- beide im Fenster, beide nicht-identisch.
    let cva = vspace_map_user_region(asid, SLOT_CODE, cbase, clen, hal::mmu::UserPerm::Rx, None);
    let sva = vspace_map_user_region(asid, SLOT_DATA, sbase, slen, hal::mmu::UserPerm::Rw, None);
    // `vspace_map_user_region` benennt selbst (Seitentabellen-Speicher bzw. abgewiesenes Mapping).
    let (Some(cva), Some(sva)) = (cva, sva) else {
        vspace_teardown(asid);
        let mut mem = MEM.lock();
        mem.free_region(PhysRegion::new(cbase, clen));
        mem.free_region(PhysRegion::new(sbase, slen));
        drop(mem);
        release_user_kstack(kbase);
        return None;
    };
    let packed = ((asid as u64) << 48) | l1;

    let tid = {
        let mut sched = SCHEDS[core].lock();
        // **Entry ist eine VA, die Reap-Region eine PA.** Vorher stand an beiden Stellen
        // `cbase`/`sbase` -- dieselbe Zahl, zwei Bedeutungen. Genau diese Vermengung hat beim
        // Heben des GiB-0-Deckels einen `#PF` im Kernel erzeugt.
        let r = sched.spawn_user_at_parked(
            core,
            cva as usize,          // Entry: virtuell, Anfang des Code-Blocks
            0,
            kbase,
            USER_KSTACK_SIZE,
            (sva + slen) as usize, // EL0-SP: virtuell
            sbase as usize,        // Reap: physisch
            slen as usize,
            prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot());
            // Buchfuehrung NOCH UNTER SCHEDS -- s. Kommentar an `record_user_kstack`.
            record_user_kstack(t, kbase); // Kstack dem Thread zuordnen (reclaim!)
            // C7b: der Stack-Frame der nativ geladenen PD (der Code-Frame ist keiner).
            record_user_region(t.slot(), sbase, slen);
            set_vspace_of(t.slot(), packed); // nach fp_reset_slot!
        }
        r
    };
    match benannt_slot(MANGEL_THREAD_SLOT, tid) {
        Some(t) => {
            // Der private Code-Frame ist KEINE Reap-Region (das ist der Stack) und haengt als
            // 2-MiB-Block (kein L3) am L2 -> vspace_teardown faende ihn nicht. Unter der ASID
            // registrieren, damit ein spaeteres destroy_isolated ihn via loaded_free freigibt
            // (sonst leckt der Code-Frame beim Abbau). Der Stack wird separat via Reap freigegeben.
            // VA, Perm, Eintritt und Argument stehen hier im selben Sichtfeld — keine zweite
            // Quelle, kein Raten (FORK-Erbe wie im Ladepfad).
            let _ = loaded_register(asid, cva as usize, 0, &[(cbase, clen)], &[cva], &[PERM_RX]);
            Some(Parked(t))
        }
        None => {
            vspace_teardown(asid);
            let mut mem = MEM.lock();
            mem.free_region(PhysRegion::new(cbase, clen));
            mem.free_region(PhysRegion::new(sbase, slen));
            drop(mem);
            release_user_kstack(kbase);
            None
        }
    }
}

/// **Stack-VA-Fenster** geladener Programme (fest, getrennt von der Code-Link-VA) + Größe. Der
/// Stack wird **nicht-identity** an diese VA gemappt.
///
/// Die Adresse ist architekturabhängig, weil das mappbare VA-Fenster einer isolierten VSpace es
/// ist. Auf aarch64 liegt es in GiB 1 (Code bei `0x4100_0000`, Stack darüber). Auf x86 hat eine
/// isolierte VSpace ihr eigenes Page Directory **nur für GiB 0**; GiB 1..3 teilen sich alle
/// isolierten Adressräume statisch, und `vspace_map_page_at` weist deshalb jede VA ≥ 1 GiB ab.
/// Der aarch64-Wert wäre dort schlicht nicht mappbar — der Ladepfad schlüge beim Stack fehl, und
/// zwar erst nach dem Kopieren aller Segmente.
#[cfg(target_arch = "aarch64")]
const LOADED_STACK_VA: u64 = 0x43F0_0000;
/// x86: knapp unter der 1-GiB-Grenze, weit über der Code-Link-VA `0x2000_0000` (`user-x86.ld`).
#[cfg(not(target_arch = "aarch64"))]
const LOADED_STACK_VA: u64 = 0x3FF0_0000;
const LOADED_STACK_BYTES: u64 = 0x4000; // 16 KiB

/// `src` (filesz Bytes) nach `dst_phys` kopieren + `[src.len(), total)` nullen (`.bss` + Padding).
/// Die **einzige** unsafe-Stelle des Ladepfads (ADR 0011 §2): Kopieren bereits **validierter**
/// Segmente in den Zielspeicher.
/// Ein **Stueck** eines Segments kopieren: `n` Bytes ab `off` aus `src`, Rest mit Null.
///
/// Die stueckweise Fassung von [`copy_segment`] (A1/Z11c). `off` kann hinter `src.len()` liegen --
/// dann ist das Stueck reines BSS und wird vollstaendig genullt. Ohne diesen Fall waere ein
/// Segment mit `memsz > filesz`, dessen Nullteil ueber eine Stueckgrenze faellt, falsch gefuellt.
fn copy_segment_at(dst_phys: u64, src: &[u8], off: usize, n: usize) {
    let aus_datei = src.len().saturating_sub(off).min(n);
    // SAFETY: `dst_phys` ist ein frisch allozierter, identity-gemappter RW-Frame der Groesse `n`;
    // es werden genau `n` Bytes geschrieben, und `aus_datei <= n` sowie `off + aus_datei <=
    // src.len()` gelten nach Konstruktion.
    unsafe {
        let dst = dst_phys as *mut u8;
        if aus_datei > 0 {
            core::ptr::copy_nonoverlapping(src.as_ptr().add(off), dst, aus_datei);
        }
        core::ptr::write_bytes(dst.add(aus_datei), 0, n - aus_datei);
    }
}

#[allow(dead_code)]
fn copy_segment(dst_phys: u64, src: &[u8], total: usize) {
    // SAFETY: `dst_phys` ist ein frisch allozierter, identity-gemappter RW-Frame der Größe `total`
    // (auf Seiten aufgerundet, >= `src.len()`); es werden genau `total` Bytes im Frame geschrieben.
    unsafe {
        let dst = dst_phys as *mut u8;
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        core::ptr::write_bytes(dst.add(src.len()), 0, total - src.len());
    }
}

// --- Register geladener Programm-Segmente (ext-26, L4) ---
//
// Geladene PT_LOAD-Segmente + der Stack-Frame sind kernel-ausgeschnittene RAM-Frames, die NICHT
// cap-getrackt sind (der MemoryCap-Deskriptor wird in `load_into_pd` verworfen, da das Eigentum
// an die geladene PD/VSpace uebergeht). Damit sie beim Teardown der VSpace NICHT lecken, werden
// sie hier je `asid` registriert und von [`vspace_teardown`] mitfreigegeben.
// **64 statt 8** (A1/Z11c, 2026-08-07). Eine gefaerbte geladene PD kann ihre Segmente nicht als
// EINE zusammenhaengende Region nehmen: Farbe ist eine Funktion der Physadresse, und innerhalb
// eines Streifens sind hoechstens `colors::region_bytes()` aufeinanderfolgende Bytes gleichfarbig
// (x86 512 KiB, aarch64 16 KiB). Sie wird deshalb stueckweise alloziert, und jedes Stueck muss
// einzeln zurueckgegeben werden koennen.
const MAX_IMG_SEGS: usize = 64; // Frames je Programm (Segmentstuecke + Stackstuecke)
// W^X bleibt pro Stueck pruefbar: der Perm-Code reist mit der Buchhaltung, nicht mit dem Pfad.
const PERM_RX: u8 = 0;
const PERM_RW: u8 = 1;
const PERM_RO: u8 = 2;

/// Einen Abbildungs-Perm in seinen Buchhaltungs-Code falten (FORK-Erbe).
fn perm_code(p: hal::mmu::UserPerm) -> u8 {
    match p {
        hal::mmu::UserPerm::Rx => PERM_RX,
        hal::mmu::UserPerm::Rw => PERM_RW,
        hal::mmu::UserPerm::Ro => PERM_RO,
    }
}

/// Den Code zurueck in einen Perm — `None` kennt der Ladepfad nicht (fail-closed, benannt).
fn perm_von(c: u8) -> Option<hal::mmu::UserPerm> {
    match c {
        PERM_RX => Some(hal::mmu::UserPerm::Rx),
        PERM_RW => Some(hal::mmu::UserPerm::Rw),
        PERM_RO => Some(hal::mmu::UserPerm::Ro),
        _ => None,
    }
}
#[derive(Clone, Copy)]
struct LoadedImage {
    asid: u16, // 0 = freier Slot
    nseg: usize,
    segs: [(u64, u64); MAX_IMG_SEGS], // (base, len)
    /// Die VA je Stueck (FORK-Quellmenge) — `len` steht in `segs[i].1`.
    ///
    /// Beim Mappen gesammelt, nicht geraten: eine Rueckabbildung phys→VA existiert nicht
    /// (kein inverser Index; `vspace_resolve` geht nur VA→PA). Wer den FORK-Loop ohne diese
    /// Liste schriebe, kopierte an geratene VAs — das ist der Korruptionspfad, nicht die
    /// Abkuerzung. Kosten: 512 B je Eintrag zusaetzlich (s. `configure_caps`).
    vaddr: [u64; MAX_IMG_SEGS],
    /// Der Perm-Code je Stueck (W^X bleibt im Kind pruefbar).
    perm: [u8; MAX_IMG_SEGS],
    /// Eintritt und Startargument des geladenen Images (FORK-Erbe: das Kind startet am
    /// selben Eintritt mit demselben Argument — Neustart vom Eintritt, kein Fortsetzen).
    eintritt: usize,
    startarg: usize,
}
impl LoadedImage {
    const EMPTY: LoadedImage = LoadedImage {
        asid: 0,
        nseg: 0,
        segs: [(0, 0); MAX_IMG_SEGS],
        vaddr: [0; MAX_IMG_SEGS],
        perm: [0; MAX_IMG_SEGS],
        eintritt: 0,
        startarg: 0,
    };
}
/// Teardown-Buchhaltung geladener Programme aus Boot-RAM (Dichte): ein Eintrag je PD.
///
/// Bis hierher eine statische Reserve von 16 Eintraegen (`[LoadedImage; 16]` im BSS): das 17.
/// gleichzeitig geladene Programm bekam keinen Teardown-Eintrag, und der Ladevorgang schlug
/// fehl — bei 10 000 PDs also an einer Zahl, die nie mitgewachsen ist. Dimensioniert auf
/// `NPDS` (s. `configure_caps`), Kosten je Eintrag rund eineinhalb KiB aus Boot-RAM
/// (Frames + VA-Liste + Perms + Eintritt, s. `LoadedImage`) mit Abrechnung im Boot-Report.
/// Ein vergessener Anhang faellt wie bei `VSPACES` auf: die Tabelle hat dann
/// Laenge 0, und jede Registrierung meldet das Scheitern statt zu lecken.
static LOADED_IMAGES: SpinLock<Slab<LoadedImage>> = SpinLock::new(Slab::empty());

/// Kapazitaet der Teardown-Buchhaltung geladener Programme (angehaengt, nicht Konstante).
pub fn loaded_capacity() -> usize {
    LOADED_IMAGES.lock().len()
}

/// Die RAM-Frames `segs` eines geladenen Programms unter `asid` registrieren (für den Teardown),
/// dazu die VA-Liste, die Perms, Eintritt und Startargument (FORK-Erbe).
///
/// `vas`/`perms` laufen parallel zu `segs` (ungleiche Laengen = Absage, kein Abschneiden).
/// **Gibt `false` zurueck, wenn nicht ALLES registriert werden konnte** -- kein Slot frei, oder
/// mehr Stuecke als [`MAX_IMG_SEGS`].
///
/// Vorher stand hier ein `take(MAX_IMG_SEGS)` ohne Rueckmeldung: was darueber lag, wurde
/// stillschweigend weggelassen und beim Teardown nie freigegeben. Das ist wortwoertlich die Form
/// von D11 (`if cap { .. }` ohne `else`) -- eine Kapazitaet, deren Ueberlauf niemand erfaehrt.
/// Solange die Segmentzahl vorher gegen dieselbe Schranke geprueft wurde, war es unerreichbar;
/// mit der stueckweisen Allokation der gefaerbten PDs ist es das nicht mehr.
#[must_use = "ein nicht registriertes Stueck wird beim Teardown nie freigegeben -- ein Leck"]
fn loaded_register(
    asid: u16,
    eintritt: usize,
    startarg: usize,
    segs: &[(u64, u64)],
    vas: &[u64],
    perms: &[u8],
) -> bool {
    if segs.len() > MAX_IMG_SEGS || segs.len() != vas.len() || segs.len() != perms.len() {
        return false;
    }
    let mut t = LOADED_IMAGES.lock();
    let Some(slot) = t.iter().position(|i| i.asid == 0) else {
        return false;
    };
    let mut img = LoadedImage::EMPTY;
    img.asid = asid;
    img.eintritt = eintritt;
    img.startarg = startarg;
    for (i, &s) in segs.iter().enumerate() {
        img.segs[img.nseg] = s;
        img.vaddr[img.nseg] = vas[i];
        img.perm[img.nseg] = perms[i];
        img.nseg += 1;
    }
    t[slot] = img;
    true
}

/// Die zuletzt **gefaerbt** geladene PD: `(asid, Farbsatz)`. Nur fuer den Nachweis.
#[cfg(feature = "selftest")]
static PDCOLOR_GEFAERBT: SpinLock<Option<(u16, caprock_mem::ColorMask)>> = SpinLock::new(None);
/// Die zuletzt **ungefaerbt** geladene PD -- die Gegenprobe.
#[cfg(feature = "selftest")]
static PDCOLOR_UNGEFAERBT: SpinLock<u16> = SpinLock::new(0);

/// Die Prioritaet, die der SCHEDULER diesem Thread gibt.
#[cfg(feature = "selftest")]
pub fn priority_of(tid: ThreadId) -> Option<u8> {
    caprock_sched::owner_core(tid).and_then(|c| SCHEDS[c].lock().priority_of(tid))
}

/// Der letzte Ladevorgang mit einer Politik, die **von der Vorgabe abweicht**:
/// `(tid_roh, verlangt, Kern_verlangt)`. Nur fuer den Z11c-Nachweis.
///
/// **Nicht „der zuletzt geladene".** Der erste Entwurf tat das, und die Zeile las sich gut und
/// belegte nichts: das zuletzt geladene Programm der Lade-Suite verlangt `prio=1`, und 1 ist die
/// Vorgabe. Ein Ladepfad, der die Manifest-Prioritaet komplett ignoriert, haette dieselbe Zeile
/// gedruckt. Gefragt ist der Fall, in dem sich beides UNTERSCHEIDET.
#[cfg(feature = "selftest")]
#[allow(clippy::type_complexity)]
static LADEPOLITIK_ABWEICHEND: SpinLock<Option<(u8, Option<u8>, Option<usize>, Option<usize>)>> =
    SpinLock::new(None);

/// `(verlangte Prio, bekommene Prio, verlangter Kern, bekommener Kern)` des letzten
/// Ladevorgangs mit abweichender Politik. `None`, wenn jedes Programm die Vorgabe verlangt hat.
#[cfg(feature = "selftest")]
pub fn ladepolitik_abweichend() -> Option<(u8, Option<u8>, Option<usize>, Option<usize>)> {
    *LADEPOLITIK_ABWEICHEND.lock()
}

/// `(asid_gefaerbt, maske, asid_ungefaerbt)` fuer [`crate::colors::run_pd_color`].
#[cfg(feature = "selftest")]
pub fn pdcolor_kandidaten() -> Option<(u16, caprock_mem::ColorMask, u16)> {
    let g = (*PDCOLOR_GEFAERBT.lock())?;
    Some((g.0, g.1, *PDCOLOR_UNGEFAERBT.lock()))
}

/// **Die registrierten Frames einer `asid` lesen** (A1-Nachweis, 2026-08-07).
///
/// Der Farbnachweis einer gefaerbt geladenen PD kann nicht aus dem Ladepfad kommen -- der wuerde
/// bestaetigen, was er selbst getan hat. Er kommt aus der Buchhaltung, die auch der **Teardown**
/// benutzt: was hier nicht steht, wird nie freigegeben, und was hier steht, ist genau das, was der
/// PD gehoert. Dieselbe Ueberlegung wie bei `tools/checkfat.py` -- ein Schreiber, der sein eigenes
/// Ergebnis bestaetigt, bestaetigt nichts.
///
/// Gibt die Zahl der geschriebenen Eintraege; `0` heisst "keine solche `asid`".
#[cfg(feature = "selftest")]
pub fn loaded_frames_of(asid: u16, out: &mut [(u64, u64)]) -> usize {
    let t = LOADED_IMAGES.lock();
    let Some(slot) = t.iter().position(|i| i.asid == asid && i.asid != 0) else {
        return 0;
    };
    let n = t[slot].nseg.min(out.len());
    out[..n].copy_from_slice(&t[slot].segs[..n]);
    n
}

/// Die registrierten `(Phys, VA, Laenge, Perm-Code)` einer `asid` lesen — die Quellmenge des
/// FORK (NICHT hinter `selftest`: der FORK-Pfad braucht sie im Produktivbau).
///
/// Spiegel von [`loaded_frames_of`], aber vollstaendig: was dort nicht steht (die VA), gehoert
/// der PD nicht zur Kopie — der FORK kopiert an DIESELBEN VAs, und was er darueber hinaus
/// gemappt hat, wird NICHT kopiert (Luecke sehen, nicht raten). Gibt die Zahl der
/// geschriebenen Eintraege; `0` heisst „keine solche `asid`".
fn loaded_snapshot(asid: u16, out: &mut [(u64, u64, u64, u8)]) -> usize {
    let t = LOADED_IMAGES.lock();
    let Some(slot) = t.iter().position(|i| i.asid == asid && i.asid != 0) else {
        return 0;
    };
    let n = t[slot].nseg.min(out.len());
    for i in 0..n {
        out[i] = (t[slot].segs[i].0, t[slot].vaddr[i], t[slot].segs[i].1, t[slot].perm[i]);
    }
    n
}

/// Eintritt und Startargument des unter `asid` registrierten Images (FORK-Erbe).
/// `None` = keine solche `asid` (keine Aussage, kein Ersatzwert).
fn loaded_eintritt(asid: u16) -> Option<(usize, usize)> {
    let t = LOADED_IMAGES.lock();
    t.iter()
        .find(|i| i.asid == asid && i.asid != 0)
        .map(|i| (i.eintritt, i.startarg))
}

/// Die registrierten RAM-Frames der `asid` an den Allokator zurückgeben + den Slot freigeben.
/// Snapshot ziehen, `LOADED_IMAGES` freigeben, DANN `MEM` (Rangordnung: nie beide gleichzeitig).
fn loaded_free(asid: u16) {
    let mut snap = [(0u64, 0u64); MAX_IMG_SEGS];
    let mut n = 0;
    {
        let mut t = LOADED_IMAGES.lock();
        if let Some(slot) = t.iter().position(|i| i.asid == asid && i.asid != 0) {
            n = t[slot].nseg;
            snap[..n].copy_from_slice(&t[slot].segs[..n]);
            t[slot] = LoadedImage::EMPTY;
        }
    }
    if n > 0 {
        let mut mem = MEM.lock();
        for &(base, len) in &snap[..n] {
            mem.free_region(PhysRegion::new(base, len));
        }
    }
}

/// Ein extern geladenes, **validiertes** ELF-Image in eine **vor-erstellte** isolierte PD laden +
/// starten (ext-26, L1c/L3, generischer Binary-Loader). Kopiert die `PT_LOAD`-Segmente an beliebige
/// RAM-Frames und mappt sie **W^X** an ihre Link-VAs ([`hal::mmu::vspace_map_page_at`]), legt einen
/// Stack an, endowt die `endow`-Caps (über `install_cap_checked` — Domänen-Policy + Audits bleiben
/// gültig), spawnt einen EL0-Thread am Entry und **bindet** ihn an `pd`. Die **PD-Erzeugung**
/// (Domäne, ggf. HardwareLand-Partner/Kanal, Autoritäts-Caps) liegt beim Aufrufer — so kann der
/// Loader UserLand (`load_elf`) wie HardwareLand-Backends (vor-erstellt) bedienen. Der Aufbau ab
/// dem Spawn läuft **IRQ-maskiert** (der Thread startet nicht vor Bindung + Endowment). Gibt die
/// `ThreadId`. Bei Fehler wird `pd` NICHT abgebaut (gehört dem Aufrufer).
pub fn load_into_pd(
    img: &ElfImage,
    pd: usize,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
) -> Option<ThreadId> {
    load_into_pd_mit(img, pd, endow, boot_arg, LadePolitik::VORGABE)
}

/// **Die Politik, unter der ein Programm geladen wird** (Z11c, 2026-08-07).
///
/// Ein Verbund statt vier Argumenten, und das ist keine Kosmetik: `load_into_pd(img, pd, endow,
/// boot_arg, true, 1, None, 0)` waere eine Zeile, in der `true` und `1` beliebig vertauschbar
/// aussehen. Dieses Projekt hat genau diese Falle schon bezahlt -- `Scheduler::spawn_user` nahm
/// EINEN Wert fuer EL0-SP und Reap-Region, und solange beide zufaellig gleich waren, war es
/// harmlos.
///
/// Die Werte kommen aus dem **Manifest** (`policy_flags`, `core_affinity`, `priority`,
/// `budget_us`). Was der Kernel nicht einhalten kann, weist der Lader vorher ab -- hier kommt
/// also nur an, was gilt.
#[derive(Clone, Copy)]
pub struct LadePolitik {
    /// Exklusiver Farbstreifen (`POLICY_EXCLUSIVE_STRIPE`).
    pub farbig: bool,
    /// Gewuenschter NUMA-Knoten (Z8/N3, todo 3521-3529) — aus `manifest.numa_node`.
    ///
    /// `Unaffiliated` heisst „kein ausdruecklicher Wunsch" (das gilt auch fuer `numa_node = 0`:
    /// das Format kann „nichts gesagt" nicht von „Knoten 0" unterscheiden, und aus einem
    /// schweigenden Dokument einen Knotenwunsch zu erfinden waere die A1-Lehre andersherum).
    /// Steht er, reist er bis zu den gefaerbten Allokationen des Ladepfads
    /// (`mem_alloc_masked_auf`). Schweigt er, gewinnt der Ladepfad ihn aus dem geloesten Kern
    /// zurueck (`node_of_core` in `load_into_pd_mit_va`): der Lader loest `numa_node` in einen
    /// Kern DIESES Knotens auf, und der Kern traegt den Knoten noch. Vorher trug die Politik
    /// `farbig` und `core`, aber keinen Knoten, und niemand rief die Leiter je mit einem echten
    /// Knoten (Arithmetik ohne Anrufer).
    pub node: caprock_hal::numa::Node,
    /// Prioritaet des Threads.
    pub prio: u8,
    /// Fester Kern, oder `None` fuer den aufrufenden.
    pub core: Option<usize>,
    /// MCS-Budget in Mikrosekunden; `0` = kein Budget (Round-Robin).
    pub budget_us: u32,
    /// **Bitmaske der Ziel-Slots, in denen vom Aufrufer ANGEBOTENE Caps liegen** (`SYS_LOAD`)
    /// -- `0`, wenn nichts angeboten wurde. Seit der Mehrfachdelegation eine Menge und keine
    /// einzelne Zahl: mit acht moeglichen Angeboten waere ein Einzelwert die erste Delegation,
    /// und die uebrigen zaehlten faelschlich als Zusagen des Manifests.
    ///
    /// ## Warum das ueberhaupt unterschieden werden muss
    ///
    /// Im `endow`-Slice liegen zwei verschiedene Dinge, und bis 2026-08-25 behandelte der
    /// Ladepfad sie gleich:
    ///
    /// * eine **Zusage des signierten Manifests** -- was das Autoritaetsdokument dem Programm
    ///   verspricht. Haelt sie nicht, ist das Programm nicht das, als das es zugelassen wurde.
    /// * ein **Angebot des ladenden Aufrufers** -- eine Delegation, die eine Zieldomaene
    ///   ablehnen DARF (`cap_allowed`: eine HardwareLand-PD haelt nur Caps ihres eigenen Kanals).
    ///
    /// Ohne die Unterscheidung gibt es nur zwei Fassungen, und beide sind falsch: alles
    /// durchwinken laesst eine halb ausgestattete PD anlaufen, alles abweisen macht das Laden
    /// jedes Treibers unmoeglich (`init` bietet jedem Kind seine Notification an und kann die
    /// Domaene des Kindes gar nicht kennen).
    pub angebotene_slots: u16,
    /// **Das angeforderte Cap-Budget der neuen PD** (2026-08-26); `0` = Vorgabe
    /// ([`caprock_microkit::CAP_BUDGET_PER_PD`]).
    ///
    /// Es steht hier und nicht im Manifest, und das ist eine Entscheidung: das Manifest
    /// beschreibt das **Startverhalten** (ein kleiner Plattentreiber, ein Boot-Taskmanager) und
    /// wird signiert; alles Weitere entsteht zur Laufzeit. Eine Treiberumgebung, die dreissig
    /// Slots braucht, wird nicht gebootet, sondern geladen -- also traegt `SYS_LOAD` die Zahl
    /// (`MSG3`) und nicht das Autoritaetsdokument.
    pub cap_budget: u16,
}

impl LadePolitik {
    /// Was ohne Manifest-Angabe gilt -- **bitgleich das Verhalten vor Z11c**.
    pub const VORGABE: LadePolitik = LadePolitik {
        farbig: false,
        node: caprock_hal::numa::Node::Unaffiliated,
        prio: IDLE_PRIO,
        core: None,
        budget_us: 0,
        angebotene_slots: 0,
        cap_budget: 0,
    };
}

/// **Wie [`load_into_pd`], aber mit exklusivem Farbstreifen** (A1 / Z11c, 2026-08-07).
///
/// # Warum das bis heute nicht ging -- und warum die Begruendung nur halb stimmte
///
/// `policy_gate` wies `POLICY_EXCLUSIVE_STRIPE` mit dem Argument ab, Segmente und Stack eines
/// geladenen Programms kaemen „zusammenhaengend aus `mem_alloc`", ein Farbstreifen trage aber nur
/// `region_bytes()`. Der zweite Teil stimmt; der erste beschrieb die damalige **Allokation**, nicht
/// eine Notwendigkeit: gemappt wird hier laengst **seitenweise** (`vspace_map_page_at` in einer
/// 4-KiB-Schleife), physische Zusammenhaengung wird also gar nicht gebraucht.
///
/// Damit ist die Behebung nicht „stueckweises Mapping bauen", sondern nur: **stueckweise
/// allozieren und jedes Stueck einzeln buchen.**
///
/// # Was gefaerbt wird -- und was nicht
///
/// Gefaerbt aus **einem** Streifen: PT_LOAD-Segmente, User-Stack, die Seitentabellen der PD
/// (`create_vspace_masked`, und die L3-Tabellen ueber den `a3`-Allokator) und der
/// EL0-Kernel-Stack (`claim_user_kstack_masked`). Das ist derselbe Umfang wie bei
/// [`spawn_isolated_colored`]; die Zusicherung lautet „die private Region zweier PDs teilt keine
/// Cache-Farbe", nicht „die PDs teilen keinerlei Cache-Zeile".
///
/// # Fail-closed
///
/// Ist kein Streifen frei, entsteht die PD **gar nicht erst** -- es gibt bewusst keine ungefaerbte
/// Rueckfallebene. Eine Trennung, die unter Last leise verschwindet, waere schlimmer als gar keine:
/// danach weiss niemand mehr, welche PD getrennt ist und welche nicht. Dieselbe Festlegung wie bei
/// `spawn_isolated_colored_auto`.
/// **WELCHE Ressource beim Laden fehlte** (2026-08-10).
///
/// `LoaderError::NoResources` ist ein **Sammelbegriff** — und ein Sammelbegriff im Fehlerwert ist
/// die Prüfer-Krankheit eine Ebene tiefer: *ein Lader, der „NoResources" sagt, ist ein Prüfer, der
/// „FAIL" sagt.* Dieselbe Bewegung wie beim `#NM`-Handler (drei Gründe unterscheidbar statt
/// geraten) und beim Manifest-Format (Versionsunterschied statt Formfehler).
///
/// **Warum ein globaler Wert und keine Rückgabe:** die Umstellung auf `Result<_, Mangel>` ginge
/// durch acht Fehlerausgänge und alle Aufrufer. Der Wert wird beim EINTRITT in den Ladepfad
/// gelöscht und unmittelbar danach gelesen; der Ladepfad läuft **sequenziell** (`SYS_LOAD` bedient
/// einen Aufruf, `start_root_task` läuft einmal beim Hochlauf). **Was das kaputt machen würde:**
/// zwei gleichzeitige Ladevorgänge — dann gewönne der letzte Schreiber. Die Annahme steht hier,
/// damit sie beim ersten parallelen Lader auffällt und nicht danach.
///
/// **Seit 2026-08-10 melden auch die `spawn_*`-Pfade hierher** (C7). Damit gilt die Annahme
/// „sequenziell" ausdrücklich auch für sie: zwei Kerne, die gleichzeitig einen Thread anlegen und
/// beide scheitern, überschreiben einander. Der Wert ist eine **Diagnose**, kein Rückgabewert —
/// gelesen wird er unmittelbar nach dem Fehlschlag im selben Faden (`kapazitaet_kurve`,
/// `SYS_LOAD`). Wer ihn als Ergebnis benutzen will, braucht eine Rückgabe, keinen Zähler; die
/// Grenze steht hier, damit sie beim ersten Aufrufer auffällt, der sie überschreitet.
///
/// **Warum `MANGEL_KEINER` in einem Fehlerfall trotzdem eine Aussage ist:** dann lag es an keinem
/// Vorrat, sondern an einer Prüfung (krumme Adresse, zu viele Segmentstücke, abgelehnte Politik).
/// Genau dafür gibt es [`MANGEL_MAPPING_ABGEWIESEN`] — „keine Ressource" muss man sagen können,
/// ohne zu schweigen.
pub const MANGEL_KEINER: u32 = 0;
pub const MANGEL_FARBSTREIFEN: u32 = 1;
pub const MANGEL_KERNEL_STACK: u32 = 2;
pub const MANGEL_VSPACE_ASID: u32 = 3;
pub const MANGEL_L2_TABELLE: u32 = 4;
pub const MANGEL_SEGMENT_SPEICHER: u32 = 5;
pub const MANGEL_SEITENTABELLE: u32 = 6;
pub const MANGEL_STACK_SPEICHER: u32 = 7;
pub const MANGEL_THREAD_SLOT: u32 = 8;
pub const MANGEL_PD_SLOT: u32 = 9;
/// **Keine Ressource, sondern eine krumme Adresse** (2026-08-10). `vspace_map_page_at` weist eine
/// nicht seitenausgerichtete VA/PA ab, **ohne den Allokator je zu fragen**.
///
/// Dieser Code existiert, weil die erste Fassung genau das verschwieg: sie schrieb den Fehlschlag
/// als „Speicher fuer eine Seitentabelle, 4096 Byte" fest — und die `4096` war ein **Literal im
/// Quelltext**, kein Messwert. Eine Diagnose, die eine Ursache NENNT, die sie nicht gemessen hat,
/// ist dieselbe Krankheit, gegen die `NoResources` eine Ebene hoeher gerade erst benannt wurde.
/// Sie hat den `wasmhost`-Fall ein zweites Mal in die falsche Richtung geschickt (472 MiB frei,
/// und „der Allokator" als Erklaerung).
pub const MANGEL_MAPPING_ABGEWIESEN: u32 = 10;
/// **Stack eines KERNEL-Threads** (`STACK_SIZE`, 64 KiB) — nicht zu verwechseln mit
/// [`MANGEL_KERNEL_STACK`], dem 16-KiB-EL1-Stack eines EL0-Threads. Es sind zwei Toepfe mit
/// verschiedenen Groessen, und welcher gilt, haengt an der ART des Threads. In der
/// Kapazitaetskurve ist genau dieser der Grund, an dem die SAS-Reihe bei `-m 512M` endet.
pub const MANGEL_KERNEL_THREAD_STACK: u32 = 11;
/// **Die private Region einer isolierten PD** (2 MiB bei `spawn_isolated`, ein Farbstreifen-Stueck
/// bei der gefaerbten Fassung). Der Topf, an dem die isolierte Kurve reisst -- 224 Prozesse bei
/// 512 MiB, 3040 bei 6 GiB, und der Zusammenhang ist linear in der RAM-Groesse.
pub const MANGEL_PRIVATREGION: u32 = 12;
/// **Kein RAM, sondern eine aufgeteilte Seitentabelle fuer die Guard-Page** (2026-08-10).
///
/// Kernel-Stacks liegen oberhalb 16 MiB, wo die Identitaetskarte in 2-MiB-Bloecken steht; eine
/// 4-KiB-Wache verlangt, den Block aufzuteilen, und der Vorrat solcher Tabellen ist in der HAL
/// **fest** (sie hat keinen Allokator -- Kerngrenze). Reicht er nicht, wird die Stack-Anforderung
/// abgewiesen statt unbewacht bedient: ein Stack ohne Wache waere die stille Fassung genau des
/// Fehlers, gegen den die Wache gebaut ist.
///
/// Die gemeldete Menge ist eine SEITE und nicht der Stack -- der Speicher war da, die Wache
/// nicht. Wer hier `USER_KSTACK_SIZE` meldete, schickte die Diagnose in Richtung RAM.
pub const MANGEL_GUARD_TABELLE: u32 = 13;
/// **Eine Zusage des Manifests liess sich nicht installieren** (2026-08-25).
///
/// Kein Mangel an einer Ressource im ueblichen Sinn, und trotzdem hier: die Meldestelle ist der
/// einzige Weg, den GRUND eines abgewiesenen Ladevorgangs bis in den Bericht zu tragen. Der Wert
/// ist der **Slot**, nicht die Anzahl -- „irgendeine Cap ging nicht" ist als Diagnose wertlos.
pub const MANGEL_ENDOWMENT: u32 = 14;
/// **Kein Mangel, sondern eine Marke fuer die Sprechprobe.** Keine Stelle des Kernels schreibt
/// diesen Code; er wird ausschliesslich VOR einer absichtlich fehlschlagenden Anforderung gesetzt.
/// Steht er hinterher noch da, hat der gepruefte Pfad **geschwiegen** — und das ist von „es lag
/// an keiner Ressource" nicht zu unterscheiden, solange man nicht vorher vergiftet hat.
pub const MANGEL_VERGIFTET: u32 = 255;
/// **Wie viele Stellen im Kern einen Mangel MELDEN — gezaehlt, nicht geschaetzt.**
///
/// Bis zum 2026-08-10 stand an der `mangel`-Pruefzeile „rund zwanzig". Eine Zahl ohne Zaehlung ist
/// derselbe Nullbefund wie „ich habe das noch nie gesehen": sie klingt nach einer Groesse und ist
/// eine Erinnerung. Gezaehlt sind es **31** — 17 handgeschriebene [`mangel`]-Aufrufe, 8
/// [`benannt_alloc`] und 6 [`benannt_slot`]. Der Setzer der Marke ([`mangel_vergiften`]) zaehlt
/// **nicht** mit: er meldet keinen Mangel, er stellt eine Frage.
///
/// Die Unterscheidung, die daran haengt: die 14 Stellen ueber [`benannt_alloc`]/[`benannt_slot`]
/// koennen **strukturell nicht schweigen** (der Helfer meldet bei jedem `None`), die 17
/// handgeschriebenen koennen es sehr wohl — dort ist ein `return None` ohne `mangel(..)` daneben
/// eine Zeile Unaufmerksamkeit.
///
/// Bewacht von `tools/mangel-stellen.sh`: das Skript zaehlt im Quelltext nach und bricht ab, wenn
/// diese Zahl nicht mehr stimmt. Eine Zahl im Kommentar verrottet, eine Zahl mit Waechter nicht.
///
/// **Die Ratsche hat sofort gegriffen, und zwar auf einem Zusammenfluss zweier Zweige:** die
/// Guard-Page am Kernel-Stack brachte mit `MANGEL_GUARD_TABELLE` eine 32. Stelle mit, waehrend
/// der Sweep parallel gegen 31 gezaehlt hatte. Beide Aenderungen waren fuer sich richtig, der
/// Nenner war es nach dem Merge nicht mehr -- und ein falscher Nenner macht aus einer Abdeckung
/// eine Behauptung. Aufgefallen ist es nicht beim Gegenlesen, sondern in `tools/abnahme.sh`.
pub const MELDESTELLEN: usize = {
    // **ABGELEITET, nicht gefuehrt** (2026-08-11). `kernel/build.rs` ruft `tools/mangel-zaehlen.py`
    // und legt das Ergebnis als `CAPROCK_MELDESTELLEN` ins Abbild. Vorher stand hier eine Zahl,
    // die ein Mensch parallel zur Wahrheit fuehrte, gehalten von einer Ratsche -- die hat den
    // Merge-Fehler gefangen (31 gegen 32) und war doch zwei Gedaechtnisse fuer eine Tatsache.
    // Jetzt wird gelesen. Die Ratsche wacht seither ueber die ABLEITUNG.
    let b = env!("CAPROCK_MELDESTELLEN").as_bytes();
    let mut n = 0usize;
    let mut i = 0;
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as usize;
        i += 1;
    }
    n
};

/// **Wie oft ueberhaupt ein Mangel gemeldet wurde** — die Groesse, an der „der Pfad hat
/// geschwiegen" haengt, ohne dass jemand eine Marke pflegen muss.
///
/// ## Die Regel dahinter: WER MISST, SETZT DIE MARKE
///
/// Am 2026-08-11 stand hier eine vergiftete Marke, die der gemessene Pfad in seiner **ersten
/// Anweisung** loeschte (`mangel_zuruecksetzen()`). Damit war der Ausgang, den sie benennen
/// sollte, strukturell unerreichbar — und die Berichtszeile versprach ihn woertlich. Dieselbe
/// Form hatte am selben Tag die `park`-Zeile: sie las eine Groesse, die der gemessene Pfad
/// selbst schrieb.
///
/// **Die gemeinsame Wurzel: der gemessene Pfad durfte die Messgroesse anfassen.** Ein
/// Zaehler, der nur WAECHST, kann von niemandem zurueckgesetzt werden — der Messende liest ihn
/// vorher und nachher, und die Differenz ist die Aussage. Keine Refaktorierung des Pfades kann
/// die Messung mehr entwerten, ohne dass die Differenz sich aendert.
static MANGEL_GEN: AtomicU64 = AtomicU64::new(0);

/// Der Stand des Meldezaehlers. Zweimal lesen, Differenz bilden: `0` heisst **geschwiegen**.
pub fn mangel_generation() -> u64 {
    MANGEL_GEN.load(Ordering::Relaxed)
}

/// `code << 32 | angeforderte_bytes`
static LADE_MANGEL: AtomicU64 = AtomicU64::new(0);

// ================================================================================================
// DIE SPERRE — der Allokator sagt auf Ansage NEIN, damit jede Meldestelle einmal sprechen muss
// ================================================================================================
//
// **Warum eine Sperre und nicht zwanzig Mutationen.** Die Frage je Meldestelle lautet „kann diese
// Stelle SCHWEIGEN?". Eine Mutation je Stelle beantwortet sie, kostet aber einen Bau je Stelle.
// Die Sperre beantwortet sie fuer einen ganzen Pfad in einem Lauf: sie laesst `n` Anforderungen
// durch und weist ab der `n+1`-ten alles ab. Ueber wachsendes `n` wandert der Fehlschlag den Pfad
// entlang und trifft jede Anforderung genau einmal.
//
// **Der Allokator sagt nein, nicht die Meldestelle.** Die Sperre sitzt in den drei Trichtern
// ([`zoned_alloc`], [`zoned_alloc_colored`], [`mem_alloc_below`]) und gibt `None` zurueck — den
// Weg danach geht der echte Code. Sie faelscht keinen Mangel-Code; steht hinterher einer da, hat
// ihn der gepruefte Pfad geschrieben. Eine Sperre, die den Code selbst setzte, pruefte sich selbst.
//
// **Sie merkt sich die abgewiesene MENGE** — und das ist der Konjunkt gegen die Falle vom
// 2026-08-10 (`mangel(MANGEL_SEITENTABELLE, 4096)` als Literal neben einer Stelle, an der der
// Allokator nie gefragt worden war). Verglichen wird die gemeldete Zahl gegen diese — nicht gegen
// eine Konstante im Pruefer, die mit dem Literal gemeinsam falsch sein koennte.
//
// **Grenze, die hier steht, damit sie auffaellt:** die Sperre gilt nur auf dem Kern, der sie scharf
// gestellt hat. Wird der Pruefer dort verdraengt und der fremde Faden alloziert, verbraucht er
// einen Durchlass — dann gelingt der provozierte Aufruf, und der Sweep zaehlt den Durchgang als
// `fremd`, statt ihn zu bewerten. Ein verbrauchter Durchlass wird sichtbar und nicht still als
// Erfolg gebucht.

/// Restliche Durchlaesse; `u64::MAX` heisst **aus**.
#[cfg(feature = "selftest")]
static SPERRE_REST: AtomicU64 = AtomicU64::new(u64::MAX);
/// Menge der ERSTEN abgewiesenen Anforderung; `u64::MAX` heisst „hat nie gefeuert".
#[cfg(feature = "selftest")]
static SPERRE_MENGE: AtomicU64 = AtomicU64::new(u64::MAX);
/// Der Kern, auf dem die Sperre gilt.
#[cfg(feature = "selftest")]
static SPERRE_KERN: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Sperre scharf stellen: `durchlass` Anforderungen gehen noch durch, danach jede `None`.
#[cfg(feature = "selftest")]
pub fn sperre_scharf(durchlass: u32) {
    SPERRE_MENGE.store(u64::MAX, Ordering::Relaxed);
    SPERRE_KERN.store(hal::cpu::core_id(), Ordering::Relaxed);
    SPERRE_REST.store(u64::from(durchlass), Ordering::Release);
}

/// Sperre wegnehmen. Gibt `(hat gefeuert, Menge der ersten abgewiesenen Anforderung)`.
#[cfg(feature = "selftest")]
pub fn sperre_aus() -> (bool, u64) {
    SPERRE_REST.store(u64::MAX, Ordering::Release);
    let m = SPERRE_MENGE.load(Ordering::Relaxed);
    (m != u64::MAX, m)
}

#[cfg(feature = "selftest")]
fn sperre_greift(size: u64) -> bool {
    let rest = SPERRE_REST.load(Ordering::Acquire);
    if rest == u64::MAX {
        return false;
    }
    // Nur der armierte Kern ist betroffen; fremde Kerne allozieren unbehelligt weiter.
    if SPERRE_KERN.load(Ordering::Relaxed) != hal::cpu::core_id() {
        return false;
    }
    if rest > 0 {
        SPERRE_REST.store(rest - 1, Ordering::Release);
        return false;
    }
    let _ = SPERRE_MENGE.compare_exchange(u64::MAX, size, Ordering::Relaxed, Ordering::Relaxed);
    true
}

fn mangel(code: u32, bytes: u64) {
    LADE_MANGEL.store((u64::from(code) << 32) | (bytes & 0xffff_ffff), Ordering::Relaxed);
    // **Der Zaehler zuerst gedacht, dann geschrieben:** er waechst nur und wird von niemandem
    // zurueckgesetzt -- deshalb kann kein gemessener Pfad die Messung entwerten. `Release`, damit
    // ein Leser, der die Differenz sieht, auch den Code darunter sieht.
    MANGEL_GEN.fetch_add(1, Ordering::Release);
}

/// **Beim EINTRITT in einen Pfad loeschen, der einen Mangel melden koennte.**
///
/// Ohne das stuende beim naechsten Fehlschlag der Grund des VORIGEN da, und ein veralteter Grund
/// ist schlimmer als keiner: er macht den Punkt unbehebbar, weil die Diagnose an einer Stelle
/// sucht, an der nichts ist.
fn mangel_zuruecksetzen() {
    // **Die Marke der Sprechprobe ueberlebt das Zuruecksetzen** (2026-08-10, zweiter Durchgang).
    //
    // Ohne diese Zeile war der Ausgang, den [`MANGEL_VERGIFTET`] benennen soll, **strukturell
    // unerreichbar**: alle sieben Ruecksetzstellen liegen in der ERSTEN Anweisung ihres
    // `spawn_*`-Pfades, also zwischen dem Vergiften und der ersten Anforderung. Ein Pfad, der
    // schweigt, meldete damit `MANGEL_KEINER` -- exakt den Zustand, den das Vergiften
    // unterscheidbar machen sollte. Die Berichtszeile versprach „steht er noch da, hat der Pfad
    // GESCHWIEGEN" fuer einen Fall, den es nicht geben konnte.
    //
    // **Gemessen, nicht ueberlegt:** mit einer stumm gemachten Meldestelle meldet der Sweep mit
    // dieser Zeile `geschwiegen=5`, ohne sie `keiner=5` -- und `keiner` ist von „lag an keiner
    // Ressource" nicht zu unterscheiden.
    //
    // Die Marke setzt ausschliesslich der Pruefpfad, und er raeumt sie mit [`mangel_entgiften`]
    // wieder weg. Ein liegengebliebenes Gift faellt sofort auf: es steht als solches im Bericht.
    #[cfg(feature = "selftest")]
    if (LADE_MANGEL.load(Ordering::Relaxed) >> 32) as u32 == MANGEL_VERGIFTET {
        return;
    }
    LADE_MANGEL.store(0, Ordering::Relaxed);
}

/// Die Marke wieder wegnehmen — das Gegenstueck zu [`mangel_vergiften`].
///
/// Der Pruefpfad raeumt hinter sich auf, damit ein spaeterer echter Fehlschlag nicht die Marke
/// einer laengst gelaufenen Sprechprobe meldet. Ein veralteter Grund ist schlimmer als keiner.
#[cfg(feature = "selftest")]
pub fn mangel_entgiften() {
    if (LADE_MANGEL.load(Ordering::Relaxed) >> 32) as u32 == MANGEL_VERGIFTET {
        LADE_MANGEL.store(0, Ordering::Relaxed);
    }
}

/// **Die Marke fuer die Sprechprobe setzen** ([`MANGEL_VERGIFTET`]).
///
/// Nur fuer den Pruefpfad: er setzt sie VOR einer absichtlich fehlschlagenden Anforderung. Steht
/// sie danach noch da, hat der gepruefte Pfad geschwiegen -- und ohne das Vergiften waere
/// Schweigen von „lag an keiner Ressource" nicht zu unterscheiden.
#[cfg(feature = "selftest")]
pub fn mangel_vergiften() {
    mangel(MANGEL_VERGIFTET, 0);
}

/// **Eine Anforderung, die sich bei Misserfolg SELBST benennt** — und zwar mit der Menge, die
/// tatsaechlich in den Allokator gegangen ist.
///
/// `size` kommt genau EINMAL vor: als Argument dieser Funktion, das sie an `f` weiterreicht und im
/// Fehlerfall meldet. Damit kann die Meldung keine andere Zahl tragen als die angeforderte.
///
/// **Warum das eine eigene Funktion ist.** Am 2026-08-10 stand `mangel(MANGEL_SEITENTABELLE, 4096)`
/// als **Literal** neben einer Stelle, an der der Allokator gar nicht gefragt worden war. Die Zeile
/// log, und sie hat die `wasmhost`-Diagnose zweimal in die falsche Richtung geschickt (472 MiB
/// frei, und „der Allokator" als Erklaerung). Nur der Allokator darf behaupten, dass es am
/// Allokator lag.
fn benannt_alloc<T>(code: u32, size: u64, f: impl FnOnce(u64) -> Option<T>) -> Option<T> {
    let r = f(size);
    if r.is_none() {
        mangel(code, size);
    }
    r
}

/// Wie [`benannt_alloc`], aber fuer Toepfe, die keine BYTES vergeben, sondern **Plaetze**
/// (Thread-Slot, PD-Slot, ASID, Farbstreifen). `0` steht hier fuer „keine Byte-Groesse", nicht
/// fuer „nichts angefordert" — der Code sagt, welcher Platz fehlte.
fn benannt_slot<T>(code: u32, r: Option<T>) -> Option<T> {
    if r.is_none() {
        mangel(code, 0);
    }
    r
}

/// `(Code, angeforderte Bytes, freier Rest)`. Der **freie Rest** steht daneben, weil „4096 Byte
/// angefordert" ohne ihn nicht sagt, ob der Speicher knapp oder der Pool falsch war.
pub fn lade_mangel() -> (u32, u64, u64) {
    let v = LADE_MANGEL.load(Ordering::Relaxed);
    ((v >> 32) as u32, v & 0xffff_ffff, MEM.lock().total_free())
}

/// Klartext zum Code — damit die Berichtszeile keine Zahl bleibt.
pub fn mangel_name(code: u32) -> &'static str {
    match code {
        MANGEL_KEINER => "keiner (der Fehlschlag lag NICHT an einer Ressource)",
        MANGEL_FARBSTREIFEN => "Farbstreifen (A1: die Streifenvergabe ist erschoepft)",
        MANGEL_KERNEL_STACK => "EL0-Kernel-Stack",
        MANGEL_VSPACE_ASID => "VSpace/ASID",
        MANGEL_L2_TABELLE => "L2-Seitentabelle",
        MANGEL_SEGMENT_SPEICHER => "Speicher fuer ein PT_LOAD-Segment",
        MANGEL_SEITENTABELLE => "Speicher fuer eine Seitentabelle",
        MANGEL_STACK_SPEICHER => "Speicher fuer den User-Stack",
        MANGEL_THREAD_SLOT => "Thread-Slot (TCB)",
        MANGEL_PD_SLOT => "PD-Slot",
        MANGEL_MAPPING_ABGEWIESEN => {
            "KEINE Ressource -- das Abbilden wurde abgewiesen (krumme VA/PA oder ausserhalb des \
             Fensters). Der Allokator wurde dabei NICHT gefragt"
        }
        MANGEL_KERNEL_THREAD_STACK => "Speicher fuer den Stack eines KERNEL-Threads (64 KiB)",
        MANGEL_PRIVATREGION => "Speicher fuer die private Region einer isolierten PD",
        MANGEL_GUARD_TABELLE => {
            "KEIN RAM -- eine aufgeteilte Seitentabelle fuer die Guard-Page. Der Vorrat in der HAL \
             ist fest (sie hat keinen Allokator); ein Stack ohne Wache wird ABGEWIESEN statt \
             unbewacht bedient"
        }
        MANGEL_VERGIFTET => {
            "VERGIFTET -- der gepruefte Pfad hat GESCHWIEGEN. Kein Kernelpfad schreibt diesen \
             Code; er stand vor der Anforderung da und steht immer noch da"
        }
        _ => "unbekannt",
    }
}

/// **Wie viele ANGEBOTE des Aufrufers eine Zieldomaene abgelehnt hat** (2026-08-25).
///
/// Erwartet > 0: `init` bietet jedem Kind seine Notification an, und jede HardwareLand-PD lehnt
/// sie ab (`cap_allowed`: nur Caps des eigenen Kanals). Das ist kein Fehler -- aber es war bis
/// heute **unsichtbar**, und genau deshalb behauptet die Doku-Tabelle in `virtio-blk` bis heute,
/// in Slot 0 laege eine Notification. Eine Zahl im Bericht widerspricht ihr.
pub static ENDOW_ANGEBOTE_ABGELEHNT: AtomicUsize = AtomicUsize::new(0);

/// **Wie viele ZUSAGEN des Manifests sich nicht installieren liessen.**
///
/// Muss `0` bleiben. Jede andere Zahl heisst: ein Programm haette mit weniger Autoritaet
/// angefangen, als das signierte Dokument ihm zuspricht -- und haette das niemandem gesagt.
pub static ENDOW_ZUSAGEN_GEBROCHEN: AtomicUsize = AtomicUsize::new(0);

/// **Wie viele Endowment-Caps ueberhaupt geprueft wurden.**
///
/// Die Sprechprobe der `endow`-Zeile, und sie ist noetig: die Hauptsuite bootet ohne Boot-Archiv
/// und laedt deshalb **kein** Programm. Dort stuenden beide Zahlen darunter auf 0 -- und „nichts
/// gebrochen" waere von „nichts gemessen" nicht zu unterscheiden. Null ist ein Befund, kein
/// Messwert.
pub static ENDOW_GEPRUEFT: AtomicUsize = AtomicUsize::new(0);

/// Die drei Zahlen der `endow`-Zeile: `(geprueft, zusagen_gebrochen, angebote_abgelehnt)`.
pub fn endowment_bilanz() -> (usize, usize, usize) {
    (
        ENDOW_GEPRUEFT.load(Ordering::Acquire),
        ENDOW_ZUSAGEN_GEBROCHEN.load(Ordering::Acquire),
        ENDOW_ANGEBOTE_ABGELEHNT.load(Ordering::Acquire),
    )
}

/// **Passt dieses Endowment vollstaendig -- BEVOR irgendetwas alloziert ist?** (2026-08-25)
///
/// `Ok(n)` = es passt, `n` Angebote wurden dabei abgelehnt. `Err(slot)` = eine **Zusage** haelt
/// nicht, und zwar diese.
///
/// ## Warum die Pruefung hier steht und nicht in der Installationsschleife
///
/// Weil an dieser Stelle noch nichts zurueckzunehmen ist. Die Schleife laeuft, nachdem Segmente,
/// Stack, Seitentabellen und ein geparkter Thread existieren; ein Abbruch dort braucht einen
/// Teardown-Pfad, den es fuer den Thread gar nicht gibt (der `admit`-Fehlschlag daneben laesst
/// bis heute einen geparkten Thread liegen). Dieselbe Reihenfolge wie beim Farbstreifen weiter
/// unten: *schlaegt er fehl, ist noch nichts alloziert, was zurueckmuesste.*
///
/// ## Die Regel steht NICHT hier
///
/// Entschieden wird in [`Caps::endowment_fits`](caprock_microkit::Caps::endowment_fits), also
/// neben `budget_allows` und `cap_allowed`. Ein Nachbau an dieser Stelle waere die zweite
/// Wirklichkeit aus der Fallenliste -- und er waere auch inhaltlich falsch geworden, weil das
/// Budget den Zuwachs der vorigen Caps mitzaehlen muss.
fn endowment_pruefen(pd: usize, endow: &[(usize, CapPtr)], angebote: u16) -> Result<usize, usize> {
    CAPS.read().endowment_fits(pd, endow, angebote)
}

pub fn load_into_pd_mit(
    img: &ElfImage,
    pd: usize,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
    pol: LadePolitik,
) -> Option<ThreadId> {
    load_into_pd_mit_va(img, pd, endow, boot_arg, pol, None).map(|(t, _)| t)
}

/// Wie [`load_into_pd_mit`], sammelt zusaetzlich die VA-Liste: `(VA, Laenge)` je abgebildetem
/// Stueck, in `va_out` (bis zu dessen Laenge), und gibt `(Thread, GESAMTE Stueckzahl)` zurueck
/// (geschrieben wurde das Minimum aus Stueckzahl und Pufferlaenge — die Differenz benennt einen
/// zu kleinen Puffer, statt still zu schneiden).
///
/// Die Liste wird BEIM MAPPEN gesammelt — aus dem einzigen Ort, der beide Seiten kennt (die
/// Schleifen unten halten Physadresse UND Ziel-VA in der Hand). Eine Rueckabbildung phys→VA
/// existiert nicht (kein inverser Index; `vspace_resolve` geht nur VA→PA), und wer sie aus den
/// Tabellen raete, kopierte an geratene VAs. Dieselbe Sammlung traegt `loaded_register` in
/// die Teardown-Buchhaltung (`vaddr`/`perm` je Stueck): EINE Wahrheit statt zwei — was der
/// FORK kopiert, ist genau das, was der Teardown freigibt.
pub fn load_into_pd_mit_va(
    img: &ElfImage,
    pd: usize,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
    pol: LadePolitik,
    va_out: Option<&mut [(u64, u64)]>,
) -> Option<(ThreadId, usize)> {
    // Kratzflaeche ZUERST (Blattlock, s. `LOAD_SCRATCH`): keine andere Sperre gehalten;
    // alles Folgende (CAPS/MEM/SCHEDS/...) liegt darunter. Gueltig bis Funktionsende —
    // die Slices unten laufen auf den statischen Feldern.
    let mut kratz = LOAD_SCRATCH.lock();
    // EINMAL aufspalten (s. `dispatch_fork`): Guard-Borrows sind nicht feldgenau (E0502).
    let kratz: &mut LoadScratch = &mut *kratz;
    // **Das Endowment wird VERBUCHT, bevor irgendetwas alloziert ist** (2026-08-25).
    //
    // Bis heute lief die Installationsschleife weiter unten so: `if !install_pd_cap(..) {
    // cap_delete(..) }` -- ohne Zaehler, ohne Meldung, ohne Abbruch. Der Kommentar zwei Zeilen
    // dahinter behauptet „Endowment vollstaendig"; die Schleife konnte das nie einloesen. Ein
    // Programm, dem eine Zusage des Manifests fehlt, lief damit an, und niemand erfuhr es --
    // dieselbe Form wie der fehlende Endowment-Slot beim Hot-Reload, nur ohne die `NotReady`,
    // die dort wenigstens etwas sagte.
    ENDOW_GEPRUEFT.fetch_add(endow.len(), Ordering::Relaxed);
    let abgelehnte_angebote = match endowment_pruefen(pd, endow, pol.angebotene_slots) {
        Ok(n) => n,
        Err(slot) => {
            mangel(MANGEL_ENDOWMENT, slot as u64);
            ENDOW_ZUSAGEN_GEBROCHEN.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    ENDOW_ANGEBOTE_ABGELEHNT.fetch_add(abgelehnte_angebote, Ordering::Relaxed);
    // **Der Kern steht VOR jeder Allokation fest**, denn er bestimmt, welcher Scheduler den Thread
    // bekommt -- und `spawn_user_at_parked` prueft das per `debug_assert_eq!`. Eine Affinitaet, die
    // erst nach dem Anlegen wirkt, waere eine Migration und keine Zuteilung.
    let core = pol.core.unwrap_or_else(hal::cpu::core_id);
    // **Der wirksame Knoten** (Z8/N3, todo 3521-3529) — die Bruecke, fuer die `loader.rs` NICHT
    // angefasst wird (nur gelesen): `ladepolitik_auf` loest `manifest.numa_node` in einen Kern
    // DIESES Knotens auf (`least_loaded_core_on`) und legt ihn in `pol.core`. Der Kern traegt
    // den Knoten noch (`node_of_core`), also wird der Wunsch hier zurueckgewonnen, statt ihn
    // im Lader in ein neues Feld zu schreiben. Ein AUSDRUECKLICHER `pol.node` sticht immer —
    // sobald der Lader ihn setzt (Patch steht im Bericht), faellt diese Ableitung weg, ohne
    // dass sich eine Allokationsstelle aendert. Ohne Topologie ist das Ergebnis
    // `Unaffiliated` und damit bitgleich der alte Pfad; `None` (kein geloester Kern, reiner
    // `VORGABE`-Lauf) bleibt es ebenfalls.
    let knoten = match pol.node {
        caprock_hal::numa::Node::At(_) => pol.node,
        caprock_hal::numa::Node::Unaffiliated => match pol.core {
            Some(c) => crate::numa::node_of_core(c),
            None => caprock_hal::numa::Node::Unaffiliated,
        },
    };
    // Der Streifen zuerst: schlaegt er fehl, ist noch nichts alloziert, was zurueckmuesste.
    // Beim EINTRITT loeschen -- sonst stuende beim naechsten Fehlschlag der Grund des vorigen da,
    // und ein veralteter Grund ist schlimmer als keiner (er macht den Punkt unbehebbar).
    mangel_zuruecksetzen();
    let stripe = if pol.farbig {
        match crate::colors::claim_stripe() {
            Some(st) => Some(st),
            None => {
                mangel(MANGEL_FARBSTREIFEN, 0);
                return None;
            }
        }
    } else {
        None
    };
    let mask = stripe.map(|(_, m)| m);
    // Ab hier gibt jeder Fehlerausgang den Streifen zurueck -- bis `vspace_bind_stripe` ihn an die
    // Lebensdauer der VSpace haengt (dann tut es `vspace_teardown`).
    let streifen_zurueck = |st: Option<(u32, caprock_mem::ColorMask)>| {
        if let Some((i, _)) = st {
            crate::colors::release_stripe(i);
        }
    };
    // **Beide benennen sich selbst** (2026-08-10, zweiter Durchgang): `claim_user_kstack_masked`
    // meldet den EL0-Kernel-Stack MIT der angeforderten Menge, `create_vspace_masked`
    // unterscheidet den ASID-Platz vom Seitentabellen-Speicher. Eine zweite, groebere Meldung
    // hier wuerde die feinere ueberschreiben -- und ein groeberer Grund ist keine Sicherheit,
    // sondern eine falsche Faehrte. Beide laufen mit dem wirksamen Knoten (s. oben): der
    // Manifest-Knoten reist bis zu den gefaerbten Allokationen, mit benanntem Rueckfall statt
    // Fehler.
    let Some(kbase) = claim_user_kstack_masked(mask, knoten) else {
        streifen_zurueck(stripe);
        return None;
    };
    let Some((asid, l1)) = create_vspace_masked(mask, knoten) else {
        release_user_kstack(kbase);
        streifen_zurueck(stripe);
        return None;
    };
    // **Sofort binden, nicht am Ende.** Ab hier ruft jeder Fehlerausgang `vspace_teardown`, und der
    // gibt den Streifen selbst zurueck. Wer hier spaeter bindet, gibt ihn auf jedem Fehlerpfad
    // doppelt oder gar nicht frei -- dieselbe Ueberlegung wie in `spawn_isolated_colored_inner`.
    if let Some((i, _)) = stripe {
        vspace_bind_stripe(asid, i);
    }
    let Some(l2) = vspace_l2(asid) else {
        mangel(MANGEL_L2_TABELLE, 0);
        vspace_teardown(asid);
        release_user_kstack(kbase);
        return None;
    };
    // `cleanup` gibt die bereits allozierten Segment-/Stack-**Daten**-Frames frei (vor dem
    // erfolgreichen `loaded_register` trackt sie NICHTS -> sonst Leck), baut die VSpace ab (gibt die
    // L1/L2/L3-Tabellen-Frames zurück) und gibt den EL0-Kernel-Stack zurück. Der MEM-Lock wird VOR
    // `vspace_teardown` freigegeben (das sperrt MEM selbst -> sonst Selbst-Deadlock).
    let cleanup = |asid: u16,
                   kbase: usize,
                   segs: &[(u64, u64)],
                   stack: Option<(u64, u64)>|
     -> Option<(ThreadId, usize)> {
        {
            let mut mem = MEM.lock();
            for &(b, l) in segs {
                if l > 0 {
                    mem.free_region(PhysRegion::new(b, l));
                }
            }
            if let Some((b, l)) = stack {
                mem.free_region(PhysRegion::new(b, l));
            }
        }
        vspace_teardown(asid);
        release_user_kstack(kbase);
        None
    };

    // 1. PT_LOAD-Segmente: kopieren + W^X an Link-VA mappen (nicht-identity, vaddr -> beliebige pa).
    // Die Segment-Frames merken (für den Teardown via loaded_register; NICHT den Stack — der wird
    // beim Thread-Ende via Reap freigegeben).
    // Mehr Segmente als registrierbar (MAX_IMG_SEGS) würden beim Teardown lecken (loaded_register
    // merkt nur die ersten MAX_IMG_SEGS) -> **fail-closed**, bevor irgendetwas alloziert ist.
    // **Die Stueckgroesse.** Ungefaerbt bleibt es bei EINEM Stueck je Segment (wie bisher, und der
    // Fastpath bleibt damit unveraendert). Gefaerbt ist die groesste am Stueck gleichfarbige Menge
    // `colors::region_bytes()` -- x86 512 KiB, aarch64 16 KiB. Groesser anzufordern hiesse, eine
    // Farbe zu verlangen, die es am Stueck nicht gibt; `alloc_colored` weist das ab.
    let stueck = |gesamt: u64| -> u64 {
        match mask {
            None => gesamt,
            Some(_) => crate::colors::region_bytes().min(gesamt).max(4096),
        }
    };
    // **Fail-closed VOR jeder Allokation.** Die Zahl der Stuecke, nicht die der Segmente, ist die
    // Groesse, die `seglist` fuellen muss -- bis heute waren sie gleich, gefaerbt sind sie es
    // nicht. Der Stack zaehlt mit: er liegt in derselben Liste, wenn das Mapping scheitert.
    let stuecke_noetig: usize = img
        .segments()
        .map(|sg| {
            let total = ((sg.memsz as u64) + 4095) & !4095;
            let st = stueck(total.max(4096));
            (total.max(4096)).div_ceil(st) as usize
        })
        .sum::<usize>()
        + (LOADED_STACK_BYTES.div_ceil(stueck(LOADED_STACK_BYTES)) as usize);
    if stuecke_noetig > MAX_IMG_SEGS {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : ABGEWIESEN -- das Image braucht {stuecke_noetig} Frame-Stuecke, die \
             Teardown-Buchhaltung fasst {MAX_IMG_SEGS}. Lieber gar nicht laden als ein Leck."
        );
        mangel(MANGEL_SEGMENT_SPEICHER, 0);
        return cleanup(asid, kbase, &[], None);
    }
    // Parallel zu `seglist`: die VA und der Perm-Code je Stueck — beim Mappen gesammelt
    // (s. `load_into_pd_mit_va`-Doku), an `loaded_register` und `va_out` gegeben.
    // Kratzfelder statt Heap/Stack (s. `LOAD_SCRATCH`): der Ladepfad laeuft ueber EXEC
    // auch auf 4-KiB-Aufrufer-Stacks, 1,6 KiB Listen gehoeren dort nicht hin, und einen
    // Heap gibt es nicht (`NoGlobalHeap`). Kapazitaet statisch, kein Reserve-Fehler —
    // die Absage bei Ueberlauf steht oben (`stuecke_noetig`) und unten (`loaded_register`).
    let mut nrec = 0usize;
    for seg in img.segments() {
        let total = (((seg.memsz as u64) + 4095) & !4095).max(4096) as usize;
        let perm = if seg.flags & PF_X != 0 {
            hal::mmu::UserPerm::Rx
        } else if seg.flags & PF_W != 0 {
            hal::mmu::UserPerm::Rw
        } else {
            hal::mmu::UserPerm::Ro
        };
        let sz = stueck(total as u64) as usize;
        let mut done = 0usize;
        while done < total {
            let n = sz.min(total - done);
            let Some(region) = mem_alloc_masked_anywhere_auf(n as u64, 4096, mask, knoten)
            else {
                mangel(MANGEL_SEGMENT_SPEICHER, n as u64);
                return cleanup(asid, kbase, &kratz.seglist[..nrec], None);
            };
            let pa = region.base(); // MemoryCap-Drop = nur Deskriptor (kein Free); RAM bleibt belegt
            // VOR dem Mappen registrieren -> ein späterer Map-Fehler gibt diesen Frame mit frei.
            // Daneben SOFORT die VA und den Perm: spaeter ist die Zuordnung (welches Stueck lag
            // an welcher VA) nicht mehr herstellbar — kein inverser Index, kein Raten.
            kratz.seglist[nrec] = (pa, n as u64);
            kratz.vvas[nrec] = seg.vaddr + done as u64;
            kratz.pperm[nrec] = perm_code(perm);
            nrec += 1;
            copy_segment_at(pa, img.segment_bytes(&seg), done, n);
            let mut off = 0u64;
            while (off as usize) < n {
                // **Der Allokator markiert sich SELBST**, statt dass der Aufrufer nachrechnet,
                // warum das Abbilden scheiterte. Zwei Nachrechnungen derselben Groesse waeren die
                // `iova_window_clear_of_msi`-Falle: Zuteiler und Pruefer brauchen EINE Quelle.
                let mut a3 = || {
                    let r = pt_rahmen(mem_alloc_masked_anywhere_auf(4096, 4096, mask, knoten));
                    if r.is_none() {
                        mangel(MANGEL_SEITENTABELLE, 4096);
                    }
                    r
                };
                if !hal::mmu::vspace_map_page_at(
                    l2,
                    seg.vaddr + done as u64 + off,
                    pa + off,
                    perm,
                    &mut a3,
                ) {
                    // Nur wenn `a3` NICHTS gemeldet hat, lag es am Abbilden selbst.
                    if lade_mangel().0 == MANGEL_KEINER {
                        mangel(MANGEL_MAPPING_ABGEWIESEN, 0);
                    }
                    return cleanup(asid, kbase, &kratz.seglist[..nrec], None);
                }
                off += 4096;
            }
            if seg.flags & PF_X != 0 {
                hal::cpu::sync_code_range(pa as usize, n); // I-Cache kohärent vor der Ausführung
            }
            done += n;
        }
    }

    // 2. Stack (nicht-identity an festes VA-Fenster, EL0-RW).
    //
    // **Zwei Wege, und der Unterschied ist die Freigabe** (A1/Z11c).
    //
    // Ungefaerbt: EINE Region, und sie wird dem Thread als **Reap-Region** mitgegeben -- stirbt er
    // (etwa durch einen absichtlichen Fault, s. die Intruder-Tests), gibt der Scheduler sie sofort
    // zurueck. Das bleibt bitgleich wie bisher.
    //
    // Gefaerbt: mehrere Stuecke. Eine Reap-Region ist EINE zusammenhaengende Region -- mehr kann
    // der Scheduler nicht halten, und ihm eine Liste zu geben waere ein Umbau des Thread-Todes fuer
    // einen Randfall. Stattdessen wandern die Stuecke in `seglist`, werden also erst beim
    // VSpace-Teardown frei. Das ist ein **benannter Unterschied**, kein Versehen: der Stack einer
    // gefaerbt geladenen PD ueberlebt den Tod ihres Threads bis zum Abbau der PD -- genauso wie
    // ihre Segmente es heute schon tun.
    let stack_sz = stueck(LOADED_STACK_BYTES) as usize;
    let mut stack_reap: Option<(u64, u64)> = None;
    let mut sdone = 0usize;
    while sdone < LOADED_STACK_BYTES as usize {
        let n = stack_sz.min(LOADED_STACK_BYTES as usize - sdone);
        let Some(region) = mem_alloc_masked_anywhere_auf(n as u64, 4096, mask, knoten) else {
            mangel(MANGEL_STACK_SPEICHER, n as u64);
            return cleanup(asid, kbase, &kratz.seglist[..nrec], stack_reap);
        };
        let pa = region.base();
        if mask.is_none() {
            // Ein Stueck, und es gehoert dem Thread (Reap).
            stack_reap = Some((pa, n as u64));
        } else {
            // Wie die Segmente oben: Phys, VA und Perm SOFORT nebeneinander — der Stack einer
            // gefaerbt geladenen PD gehoert der PD (Teardown), nicht dem Thread (Reap).
            kratz.seglist[nrec] = (pa, n as u64);
            kratz.vvas[nrec] = LOADED_STACK_VA + sdone as u64;
            kratz.pperm[nrec] = PERM_RW;
            nrec += 1;
        }
        let mut off = 0u64;
        while (off as usize) < n {
            let mut a3 = || {
                let r = pt_rahmen(mem_alloc_masked_anywhere_auf(4096, 4096, mask, knoten));
                if r.is_none() {
                    mangel(MANGEL_SEITENTABELLE, 4096);
                }
                r
            };
            if !hal::mmu::vspace_map_page_at(
                l2,
                LOADED_STACK_VA + sdone as u64 + off,
                pa + off,
                hal::mmu::UserPerm::Rw,
                &mut a3,
            ) {
                // Das gerade allozierte Stueck ist noch nicht als Reap-Region vermerkt (ungefaerbt)
                // bzw. steht schon in `seglist` (gefaerbt) -> beide Wege geben es mit frei.
                if lade_mangel().0 == MANGEL_KEINER {
                    mangel(MANGEL_MAPPING_ABGEWIESEN, 0);
                }
                return cleanup(asid, kbase, &kratz.seglist[..nrec], stack_reap);
            }
            off += 4096;
        }
        sdone += n;
    }
    // Ungefaerbt ist das die eine Region von oben; gefaerbt gibt es keine Reap-Region.
    let (stack_pa, stack_reap_len) = match stack_reap {
        Some((b, l)) => (b, l),
        None => (0, 0),
    };
    hal::mmu::flush_asid(asid);

    // 3. PD + Thread + Endowment IRQ-maskiert: der Thread darf nicht vor dem Setup starten.
    // DAIF SICHERN + maskieren (nicht unbedingt freigeben): load_elf laeuft sowohl mit IRQs an
    // (In-Kernel-Test) ALS AUCH im Syscall-Trap (SYS_LOAD, IRQs bereits maskiert) -- der Vorzustand
    // muss erhalten bleiben, sonst gaebe man IRQs mitten im Trap frei.
    let daif = hal::cpu::local_irq_save();
    let tid = {
        let mut sched = SCHEDS[core].lock();
        // **Geparkt** (D0, 2026-08-07). Vorher stand hier `spawn_user_at`, und der Thread war ab
        // dieser Zeile lauffaehig -- `bind_pd` und das Endowment kamen erst 20 Zeilen spaeter. Die
        // `local_irq_save` darueber hat das bisher gedeckt, aber nur unter zwei Bedingungen, die
        // nirgends festgeschrieben sind: die Ready-Queue ist streng kernlokal, und der
        // Lastausgleich ist aus (`MIGRATIONS`, Vorgabe aus). Wer den Schalter umlegt, oeffnet
        // damit ein Fenster, in dem eine geladene Treiber-PD **ohne jede Cap** anlaeuft.
        //
        // Eine lokale IRQ-Sperre, die eine geteilte Groesse schuetzt, hat dieses Projekt schon
        // einmal bezahlt (`loadstop`, 2026-08-02). Hier haelt sie zufaellig -- das ist kein Grund,
        // sie halten zu lassen.
        let r = sched.spawn_user_at_parked(
            core,
            img.entry() as usize,
            boot_arg,
            kbase,
            USER_KSTACK_SIZE,
            (LOADED_STACK_VA + LOADED_STACK_BYTES) as usize, // EL0-SP (virtuell)
            stack_pa as usize,                               // Reap-Region (physisch; 0 = keine)
            stack_reap_len as usize,
            pol.prio,
        );
        if let Some(t) = r {
            fp_reset_slot(t.slot());
            // Buchfuehrung NOCH UNTER SCHEDS -- s. Kommentar an `record_user_kstack`. Hier waere
            // sie auch danach sicher (`local_irq_save` oben), aber die Form bleibt ueberall gleich:
            // eine Ausnahme, die nur unter einer Zusatzbedingung stimmt, ist eine kuenftige Falle.
            record_user_kstack(t, kbase);
            // C7b: der User-Stack eines GELADENEN Programms (`LOADED_STACK_BYTES`, 16 KiB).
            // **Gefaerbt geladen ist er hier `(0, 0)`** -- dann liegt er in Stuecken in der
            // `seglist` der PD und gehoert nicht dem Thread. Eine Buchfuehrung, die dort eine
            // Region behauptete, zeigte auf Speicher, den der PD-Abbau freigibt.
            record_user_region(t.slot(), stack_pa, stack_reap_len);
            set_vspace_of(t.slot(), ((asid as u64) << 48) | l1); // isoliert (nach fp_reset_slot!)
        }
        r
    };
    let Some(tid) = tid else {
        hal::cpu::local_irq_restore(daif);
        // Spawn fehlgeschlagen -> der Stack ist noch nicht als Reap-Region eines Threads vermerkt;
        // Segmente + Stack mitfreigeben (loaded_register lief noch nicht). Gefaerbt stehen die
        // Stackstuecke bereits in `seglist`, `stack_reap` ist dann `None`.
        mangel(MANGEL_THREAD_SLOT, 0);
        return cleanup(asid, kbase, &kratz.seglist[..nrec], stack_reap);
    };
    // **Der Rueckgabewert wird geprueft** (s. `loaded_register`): passt die Liste nicht mehr,
    // waeren die Stuecke beim Teardown verloren. Oben ist das bereits fail-closed abgefangen --
    // hier steht die zweite Schranke, weil die erste eine RECHNUNG ist und diese eine TATSACHE.
    // **Den bekommenen Wert JETZT erfassen, nicht im Bericht.** Hier lebt der Thread mit
    // Sicherheit -- er ist noch nicht einmal zugelassen. Zurueckgelesen waere er es nicht:
    // `hello` laeuft kurz und beendet sich, und `priority_of` gab im Bericht `None`. Genau
    // dieselbe Falle wie beim Farbtest, wo `kstack_of` 0 lieferte, sobald die Sonde eingesammelt
    // war -- ein Wert, der an der Lebendigkeit eines Threads haengt, taugt nicht als Messgroesse.
    #[cfg(feature = "selftest")]
    {
        if pol.prio != LadePolitik::VORGABE.prio || pol.core.is_some() {
            let bekommen = SCHEDS[core].lock().priority_of(tid);
            *LADEPOLITIK_ABWEICHEND.lock() =
                Some((pol.prio, bekommen, pol.core, caprock_sched::owner_core(tid)));
        }
    }
    // Fuer den A1-Nachweis festhalten, WELCHE asid gefaerbt geladen wurde -- die Messung liest
    // spaeter die Teardown-Buchhaltung dieser asid, nicht den Ladepfad.
    #[cfg(feature = "selftest")]
    {
        match stripe {
            Some((_, m)) => *PDCOLOR_GEFAERBT.lock() = Some((asid, m)),
            None => *PDCOLOR_UNGEFAERBT.lock() = asid,
        }
    }
    if !loaded_register(
        asid,
        img.entry() as usize,
        boot_arg,
        &kratz.seglist[..nrec],
        &kratz.vvas[..nrec],
        &kratz.pperm[..nrec],
    ) {
        hal::cpu::local_irq_restore(daif);
        return cleanup(asid, kbase, &kratz.seglist[..nrec], stack_reap);
    }
    bind_pd(pd, tid);
    for &(slot, cap) in endow {
        // Policy-geprüft (Domänen-Policy bleibt gültig). Lehnt die Policy die Cap ab (false), wurde
        // sie NICHT installiert -> die vom Dispatch erzeugte Kopie loeschen, sonst leckt sie (frisches
        // CDT-Blatt, nicht letzte Referenz -> delete_leaf senkt nur den Refcount). Keine Locks gehalten;
        // unter `daif` bleiben die IRQ-safe Locks maskiert.
        //
        // **Ein Fehlschlag hier ist seit 2026-08-25 ein WIDERSPRUCH, keine geduldete Lage.**
        // `endowment_pruefen` oben hat jeden dieser Caps schon gegen dieselben zwei Praedikate
        // gehalten; faellt hier trotzdem einer durch, hat sich der Cspace dazwischen geaendert.
        // Gezaehlt statt verschwiegen -- und die Zahl gattert (`endow`-Zeile).
        if !install_pd_cap(pd, slot, cap) {
            let _ = cap_delete(cap);
            if slot >= 16 || pol.angebotene_slots & (1u16 << slot) == 0 {
                ENDOW_ZUSAGEN_GEBROCHEN.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    // **Jetzt erst darf er laufen** -- PD gebunden, Endowment vollstaendig. Das ist die Stelle,
    // die der Kommentar an Schritt 3 seit jeher behauptet hat („der Thread darf nicht vor dem
    // Setup starten"); bis zum 2026-08-07 stand sie nur nicht im Code, sondern in der IRQ-Maske.
    if !SCHEDS[core].lock().admit(tid) {
        hal::cpu::local_irq_restore(daif);
        return None;
    }
    hal::cpu::local_irq_restore(daif);
    // Die VA-Liste an den Aufrufer — was nicht hineinpasst, wird gezaehlt statt geschnitten:
    // die Rueckgabe nennt die GESAMTE Stueckzahl, geschrieben wurde das Minimum. Ein Aufrufer,
    // der weniger bekam als es gibt, sieht es an der Differenz (kein stiller Schnitt).
    if let Some(out) = va_out {
        let n = nrec.min(out.len());
        for i in 0..n {
            out[i] = (kratz.vvas[i], kratz.seglist[i].1);
        }
    }
    Some((tid, nrec))
}

/// Wie [`load_into_pd`], aber **erzeugt** eine frische PD in `domain` (UserLand). Der bequeme Pfad
/// für UserLand-Programme (`SYS_LOAD`, In-Kernel-Tests). HardwareLand-Backends werden vom Aufrufer
/// vor-erstellt (Partner-Bindung + Kanal) und über [`load_into_pd`] geladen. Gibt `(ThreadId, pd)`.
pub fn load_elf(
    img: &ElfImage,
    domain: Domain,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
) -> Option<(ThreadId, usize)> {
    load_elf_mit(img, domain, endow, boot_arg, LadePolitik::VORGABE)
}

/// Wie [`load_elf`], aber unter einer benannten [`LadePolitik`] (Z11c).
pub fn load_elf_mit(
    img: &ElfImage,
    domain: Domain,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
    pol: LadePolitik,
) -> Option<(ThreadId, usize)> {
    // Loeschen schon HIER: `create_pd_in_domain` liegt VOR `load_into_pd_mit`, und ohne das
    // Loeschen stuende bei einem Fehlschlag der Grund des VORIGEN Ladevorgangs da -- ein
    // veralteter Grund ist schlimmer als keiner.
    mangel_zuruecksetzen();
    // **Das Budget wird HIER gebucht, vor allem anderen.** Es hat zwei unterscheidbare Absagen
    // (ueber `CAP_BUDGET_MAX` / Vorrat erschoepft); beide fuehren hier zu `MANGEL_PD_SLOT`, und
    // welche es war, steht in `pd_budget_bilanz()`.
    let Some(pd) = create_pd_mit_budget(domain, pol.cap_budget) else {
        mangel(MANGEL_PD_SLOT, 0);
        return None;
    };
    match load_into_pd_mit(img, pd, endow, boot_arg, pol) {
        Some(tid) => Some((tid, pd)),
        None => {
            // load_into_pd baut `pd` bei Fehler bewusst NICHT ab (gehört dem Aufrufer) -> hier
            // freigeben, sonst leckt der PD-Slot. Zu diesem Zeitpunkt ist die PD weder gebunden
            // noch mit Caps bestückt (Endowment/bind_pd laufen erst nach allen Fehlerpfaden).
            CAPS.write().pds.free(pd);
            None
        }
    }
}

/// Den **akkumulierten Badge** einer Notification lesen, **ohne** ihn zu konsumieren (Test-/
/// Loader-Telemetrie: hat ein geladenes Programm signalisiert?). `0`, falls leer/ungültig.
pub fn notification_pending(ntfn: usize) -> u64 {
    if ntfn < ntfns().len() {
        ntfns()[ntfn].lock().pending_badge()
    } else {
        0
    }
}

/// Einen blockierten/geparkten Thread `tid` auf **seinem** (ggf. fremden) Kern
/// aufwecken: die Zielinstanz sperren, ihn bereit machen und — falls es ein
/// anderer Kern ist — einen Reschedule-IPI schicken, damit der Zielkern ihn
/// zeitnah einplant. Das ist der Cross-Core-Aufweck-Primitive.
pub fn wake_remote(tid: ThreadId) {
    if let Some((_, c)) = with_owner(tid, |s, _| s.unblock(tid).then_some(())) {
        kick(c);
    }
}

/// Wurde jemals ein Syscall von EL0 (echter User-Thread) ausgeführt?
pub fn el0_syscall_seen() -> bool {
    EL0_SYSCALL_SEEN.load(Ordering::Relaxed)
}

/// Eine Tcb-Capability für einen Thread prägen (cap-kontrolliertes `KILL`).
pub fn install_tcb_cap(tid: ThreadId, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_tcb(tid.to_raw(), rights)
}

/// Eine **Management-Capability** (`PdControl`, ext-22) für die Ziel-PD `pd` prägen: die
/// Autorität, deren Lifecycle via `SYS_PDCTL` zu steuern. Nur eine TrustedSas-PD darf sie
/// nutzen (im Dispatch geprüft); installiert wird sie cap-policy-geprüft (nur in TrustedSas).
pub fn install_pd_control_cap(pd: usize, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_pd_control(pd as u32, rights)
}

/// Eine **MMIO-Capability** (ext-22, HardwareLand) für die Geräte-Registerregion
/// `[phys, phys+len)` prägen — **nur kernelseitig** (es gibt bewusst keinen User-Syscall,
/// der beliebige MMIO-Caps erzeugt). Installiert wird sie cap-policy-geprüft nur in
/// HardwareLand-PDs (`install_cap_checked`).
pub fn install_mmio_cap(phys: u64, len: u64, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_mmio(phys, len, rights)
}

/// Eine **Loader-Capability** (ext-26) prägen: die Autorität, über `SYS_LOAD` ein Programm aus
/// `source` (0 = Boot-Archiv) zu laden. Nur kernelseitig geprägt; cap-policy-geprüft nur in
/// TrustedSas-PDs installierbar (`install_cap_checked`).
pub fn install_loader_cap(source: u32, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_loader(source, rights)
}

/// **Mapping-Art** (Konsolidierung K6) für [`map_region_into_thread`]: bestimmt Tabellen-Level,
/// Adressfenster und Speicher-Attribute der in eine isolierte VSpace eingeblendeten Region.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MappingKind {
    /// Geräte-MMIO: Device-nGnRnE, PXN|UXN, nG, in GiB 0 (`vspace_map_device`, L1-Wurzel).
    /// `ro` = schreibgeschützt.
    Device { ro: bool },
    /// DMA-RAM: in GiB 1 (`vspace_map_dma`, L2-Wurzel). `coherent` = Normal-WB (Cache-Maintenance
    /// nötig), sonst Normal-NC (ext-23-Default).
    Dma { coherent: bool },
}

/// Eine autorisierte Region `[phys, phys+len)` **art-spezifisch** in die isolierte VSpace des
/// Threads `tid` einblenden — der **eine** Eintrittspunkt (Konsolidierung K6: ersetzt
/// `map_mmio_into_thread`/`map_dma_into_thread`/`_ex`). [`MappingKind`] wählt Tabellen-Level +
/// Attribute. Generisch (keine geräte-spezifische Annahme); nur für isolierte PDs (ASID != 0 —
/// also Hardware/UserLand; HW-Caps sind ohnehin nur in HardwareLand installierbar). Gibt `false`
/// bei nicht-isolierter VSpace oder fehlgeschlagenem Mapping.
pub fn map_region_into_thread(tid: ThreadId, phys: u64, len: u64, kind: MappingKind) -> bool {
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    // Reine Seitentabellen (E-Rest 3d): der Kernel schreibt sie ueber seine Identitaetskarte,
    // die PD sieht sie nie -- sie ist der Baum, nicht das Blatt.
    let mut alloc = || {
        let r = pt_rahmen(mem_alloc_anywhere(4096, 4096));
        if r.is_none() {
            mangel(MANGEL_SEITENTABELLE, 4096);
        }
        r
    };
    // **Zwei Arten, zwei Gruende -- und das ist keine Formsache.** Bei MMIO IST die Identitaet
    // die Zusicherung: ein Treiber rechnet mit BAR-Adressen aus der PCI-Enumeration, und die sind
    // physisch. Bei DMA ist sie nur die **CPU-seitige** Haelfte; was das Geraet sieht, ist eine
    // `Iova` aus dem Fenster des Uebersetzungskontexts. Beides in einen Eintrag zu falten waere
    // dieselbe Vermengung eine Achse weiter.
    let ok = match kind {
        MappingKind::Device { ro } => match vspace_l1(asid) {
            Some(l1) => {
                let va = crate::addr::Va::for_mmio_window(MmioWindowWitness(()), crate::addr::Pa::new(phys));
                hal::mmu::vspace_map_device(l1, va.raw(), len, ro, &mut alloc)
            }
            None => return false, // nicht isoliert / ungültige ASID
        },
        MappingKind::Dma { coherent } => match vspace_l2(asid) {
            Some(l2) => {
                let va = crate::addr::Va::for_dma_window(DmaWindowWitness(()), crate::addr::Pa::new(phys));
                hal::mmu::vspace_map_dma(l2, va.raw(), len, coherent, &mut alloc)
            }
            None => return false,
        },
    };
    if ok {
        hal::mmu::flush_asid(asid);
    }
    ok
}

// --- IRQ-Caps + Deferred-IRQ-Zustellung (ext-22, P5) ---
//
// Ein Geräte-IRQ wird einem HardwareLand-Backend als Notification-Badge zugestellt. Der
// IRQ-Pfad ist **deadlock-frei** gehalten: `irq_hook` läuft im IRQ-Kontext und ist
// LOCK-FREI (nur Atomics + GIC-Maskierung); die eigentliche Zustellung (`drain_pending_irqs`)
// läuft im Reschedule-Pfad (IRQs im Trap maskiert -> kein Reentrancy) und nimmt NTFNS<SCHEDS.
/// Bindungsplätze der IRQ-Tabelle — **hergeleitet, nicht gewählt**.
///
/// Eine Bindung je Gerätevektor (Stufe B gibt jedem Gerät [`VEKTOREN_JE_ZUTEILUNG`] Vektoren),
/// plus **eine** für den in-Kernel-Binder: den aarch64-RTC-Test, der zu keiner Zuteilung gehört.
/// Eine feste Zahl daneben wäre ein zweites Gedächtnis, und die Relation `NIRQ_BIND >=
/// MAX_DRIVER_ASSIGN` galt bis heute nur zufällig — genau die Form, die `STACK_CAP_SLOTS = 1024`
/// gekostet hat: eine Zahl, die neben ihren Nachbarn steht statt gegen sie geprüft zu werden.
///
/// **Die Folge ist die Aussage von `ERR_IRQ_FULL`** (E12): weil die Tabelle nie enger ist als die
/// Menge der Zuteilungen, kann kein Treiber-PD einem anderen den Platz wegnehmen. Der Code meint
/// damit etwas Lokales — *dieses Gerät hat keinen freien Vektor* — und nicht „das System ist voll".
const NIRQ_BIND: usize = MAX_DRIVER_ASSIGN * VEKTOREN_JE_ZUTEILUNG + 1;
#[allow(clippy::declare_interior_mutable_const)]
static IRQ_INTID: [AtomicU32; NIRQ_BIND] = [const { AtomicU32::new(u32::MAX) }; NIRQ_BIND];
#[allow(clippy::declare_interior_mutable_const)]
static IRQ_NTFN: [AtomicU32; NIRQ_BIND] = [const { AtomicU32::new(0) }; NIRQ_BIND];
#[allow(clippy::declare_interior_mutable_const)]
static IRQ_BADGE: [AtomicU64; NIRQ_BIND] = [const { AtomicU64::new(0) }; NIRQ_BIND];
#[allow(clippy::declare_interior_mutable_const)]
static IRQ_PENDING: [AtomicBool; NIRQ_BIND] = [const { AtomicBool::new(false) }; NIRQ_BIND];
/// **Der Vektor-Index je Bindungsplatz** — die Spalte, die aus „Interrupt `i` kam an" ein „Vektor
/// `j` des Geräts kam an" macht.
///
/// Gesetzt beim Binden aus der Zuteilung ([`dispatch_bind_irq`] löst den CPU-Vektor über
/// [`irq_vidx_fuer_vektor`] auf); `u32::MAX` heisst „kein Gerätevektor" (RTC-Test, Zustellprobe —
/// beide binden roh über [`bind_irq`], ohne Zuteilung dahinter). Gelesen wird sie im Bericht, der
/// sonst zwei Vektoren desselben Geräts nicht auseinanderhielte — dieselbe Unterscheidung wie
/// `angeboten` gegen `vergeben`, nur auf der Zustellachse.
#[allow(clippy::declare_interior_mutable_const)]
static IRQ_VIDX: [AtomicU32; NIRQ_BIND] = [const { AtomicU32::new(u32::MAX) }; NIRQ_BIND];
static IRQ_ANY_PENDING: AtomicBool = AtomicBool::new(false);
static IRQ_DELIVERED: AtomicU64 = AtomicU64::new(0); // Telemetrie: zugestellte Geräte-IRQs
/// **Wie oft `irq_hook` ueberhaupt gerufen wurde** — die ANKUNFT, unabhaengig von Bindung und Drain.
static IRQ_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
/// Der zuletzt gesehene Vektor. „Es kam etwas" und „es kam MEINER" sind zwei Aussagen.
static IRQ_HOOK_LAST: AtomicU32 = AtomicU32::new(u32::MAX);

/// **Wie oft `irq_hook` eine Flanke zusammengefasst statt neu gemeldet hat** — die zweite Flanke
/// desselben Vektors, während die erste noch undrained ist.
///
/// Das ist der **Re-Trigger-Schutz je Slot** (MSI ist flankengetriggert, es gibt keine Maske, die
/// eine zweite Flanke aufhielte): die erste Flanke setzt das Pending-Bit ([`MeldeUrteil::Neu`]),
/// jede weitere sieht es schon stehen und wird zusammengefasst statt doppelt zugestellt
/// ([`MeldeUrteil::Zusammengefasst`] — die Entscheidung aus `hal::irte::ReTriggerSchutz`, hier als
/// atomares Bit, weil der Hook **lock-frei** ist und kein `&mut` halten kann). Zusammenfassen ist
/// kein Verlieren: der Drain arbeitet beim Lesen alle ausstehenden ab.
static IRQ_ZUSAMMENGEFASST: AtomicU64 = AtomicU64::new(0);

/// **Geräte-IRQ-Hook** (aus `exception.rs`, IRQ-Kontext, **LOCK-FREI**): ist `intid`
/// registriert, vermerken (pending) + am Distributor maskieren (kein Re-Trigger), `true`.
/// Sonst `false`. Nimmt KEINEN Lock — die Zustellung erfolgt deferred im Reschedule-Pfad.
fn irq_hook(intid: u32) -> bool {
    // **Der rohe Zaehler: ANKUNFT, nicht Zustellung.**
    //
    // `IRQ_DELIVERED` waechst in `drain_pending_irqs`, also im Reschedule-Pfad -- es misst den
    // **Drain**. Drei Lagen sind damit ununterscheidbar, und genau daran ist die B4-Suche
    // haengengeblieben: der Interrupt kam nie an / er kam an und der Vektor war unbekannt / er kam
    // an, wurde vermerkt und nie gedrained. Dieser Zaehler laeuft **vor** jeder Bedingung und
    // zaehlt jeden Vektor, den der Dispatch hier hereingibt.
    //
    // Dieselbe Unterscheidung wie `rx_used` gegen „Daten sind angekommen" -- diesmal im eigenen
    // Messwerkzeug.
    IRQ_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    IRQ_HOOK_LAST.store(intid, Ordering::Relaxed);
    for i in 0..NIRQ_BIND {
        if IRQ_INTID[i].load(Ordering::Acquire) == intid {
            hal::intc::mask_intid(intid); // level-getriggerten Geräte-IRQ bis zum Drain sperren
            // **Schon ausstehend heisst: zusammengefasst, nicht verloren.** `swap(true)` ist kein
            // „erneutes Melden" — der Drain liest das Bit einmal und stellt einmal zu, ganz gleich
            // wie viele Flanken es gesetzt haben.
            if IRQ_PENDING[i].swap(true, Ordering::AcqRel) {
                IRQ_ZUSAMMENGEFASST.fetch_add(1, Ordering::Relaxed);
            }
            IRQ_ANY_PENDING.store(true, Ordering::Release);
            return true;
        }
    }
    false
}

/// Pending Geräte-IRQs zustellen: je Pending-Slot die gebundene Notification per
/// `signal_from_kernel` signalisieren (Sperrordnung NTFNS<SCHEDS, IRQs maskiert, KEIN
/// SCHEDS-Lock gehalten). Aus dem Reschedule-Pfad VOR dem SCHEDS-Lock. Fast-Check ->
/// Null-Overhead, wenn nichts pending ist.
fn drain_pending_irqs() {
    if !IRQ_ANY_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut ops = KernelSched;
    for i in 0..NIRQ_BIND {
        if IRQ_PENDING[i].swap(false, Ordering::AcqRel) {
            let ntfn = IRQ_NTFN[i].load(Ordering::Acquire) as usize;
            let badge = IRQ_BADGE[i].load(Ordering::Acquire);
            if ntfn < ntfns().len() {
                ntfns()[ntfn].lock().signal_from_kernel(&mut ops, badge);
                IRQ_DELIVERED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Eine **IRQ-Capability** prägen (ext-22, P5) — nur kernelseitig; installiert wird sie
/// cap-policy-geprüft nur in HardwareLand-PDs.
pub fn signal_from_kernel_ntfn(ntfn: usize, badge: u64) {
    // **Sperrordnung `NTFNS < SCHEDS`**, wie im Deferred-IRQ-Pfad: `signal_from_kernel` nimmt sich
    // die Scheduler-Instanz selbst, es darf hier keine gehalten werden. Fuer Sonden (A2).
    if ntfn < ntfns().len() {
        let mut ops = KernelSched;
        ntfns()[ntfn].lock().signal_from_kernel(&mut ops, badge);
    }
}

pub fn install_irq_cap(intid: u32, rights: Rights) -> Result<CapPtr, CapError> {
    CAPS.write().cspace.install_irq(intid, rights)
}

/// **Der cap-geprüfte Einstieg für `SYS_BIND_IRQ`** (B3) — gerufen aus dem Dispatch, nachdem
/// dieser `Irq`-Cap und Notification-Cap im Cspace des Aufrufers aufgelöst hat.
///
/// `intid` kommt hier aus dem **Cap-Objekt** und nicht aus einem Register des Aufrufers; das ist
/// der Riegel, und er ist strukturell statt geprüft. [`bind_irq`] daneben bleibt der rohe
/// in-Kernel-Weg (RTC-Test) und trägt deshalb weiter Zahlen.
///
/// **Erneutes Binden derselben Cap ersetzt den Eintrag**, statt einen zweiten zu verbrauchen —
/// sonst erschöpfte ein Treiber, der sich neu lädt, sein eigenes Gerät. Die Ersetzung ist auch der
/// Grund, warum hier nicht einfach `bind_irq` gerufen wird: das nimmt bedingungslos einen freien
/// Platz.
fn dispatch_bind_irq(intid: u32, ntfn: usize, badge: u64, core: usize) -> Result<(), u64> {
    // Erst der eigene Eintrag. `compare_exchange` gegen `intid` selbst: findet er ihn, gehört der
    // Platz schon diesem Interrupt, und das Neusetzen von Notification und Badge IST die Bindung.
    for i in 0..NIRQ_BIND {
        if IRQ_INTID[i].load(Ordering::Acquire) == intid {
            IRQ_NTFN[i].store(ntfn as u32, Ordering::Release);
            IRQ_BADGE[i].store(badge, Ordering::Release);
            return Ok(());
        }
    }
    // **Der Vektor-Index kommt aus der Zuteilung, nicht aus der Cap.** Die `Irq`-Cap trägt nur den
    // CPU-Vektor (`intid`); welcher Index *dieses Geräts* das ist, weiss allein die Tabelle, die
    // den Block vergeben hat. Gehört der Vektor zu keiner Zuteilung (Zustellprobe), läuft der
    // Rohpfad — die Spalte bleibt auf „kein Gerätevektor" statt einer erfundenen 0, die hiesse
    // „erster Vektor eines Geräts".
    let ok = match irq_vidx_fuer_vektor(intid) {
        Some(vidx) => bind_irq_vidx(intid, vidx, ntfn, badge, core),
        None => bind_irq(intid, ntfn, badge, core),
    };
    if ok {
        Ok(())
    } else {
        Err(caprock_abi::result::ERR_IRQ_FULL)
    }
}

/// Einen Geräte-IRQ `intid` an die Notification `ntfn` (mit `badge`) **binden**, an `core`
/// routen und freigeben (ext-22, P5).
///
/// **Der rohe Weg, ohne Cap-Prüfung** — der einzige Aufrufer ist der in-Kernel-RTC-Test. Aus EL0
/// führt [`dispatch_bind_irq`] hierher, und *der* prüft. Bis B3 behauptete der Kommentar an dieser
/// Stelle, die Funktion sei „über die IRQ-Cap autorisiert"; ihre Signatur konnte das nie tragen.
///
/// Gibt `false`, wenn kein Bindungs-Slot frei ist.
pub fn bind_irq(intid: u32, ntfn: usize, badge: u64, core: usize) -> bool {
    // Roh heisst auch: ohne Zuteilung, also ohne Vektor-Index (`u32::MAX`).
    bind_irq_vidx(intid, u32::MAX, ntfn, badge, core)
}

/// Wie [`bind_irq`], aber mit Vektor-Index `vidx` für die [`IRQ_VIDX`]-Spalte.
///
/// `u32::MAX` heisst „kein Gerätevektor" und ist der einzige Wert, den der Rohpfad je setzt —
/// eine erfundene 0 würde den RTC-Test als „ersten Vektor eines Geräts" ausweisen.
fn bind_irq_vidx(intid: u32, vidx: u32, ntfn: usize, badge: u64, core: usize) -> bool {
    for i in 0..NIRQ_BIND {
        if IRQ_INTID[i]
            .compare_exchange(u32::MAX, intid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            IRQ_NTFN[i].store(ntfn as u32, Ordering::Release);
            IRQ_BADGE[i].store(badge, Ordering::Release);
            IRQ_VIDX[i].store(vidx, Ordering::Release);
            // SPI an den Ziel-Kern routen (GICD_ITARGETSR — fehlte bisher; lasttragend,
            // sensitivitaetsgeprueft: ohne dies erreicht der RTC-IRQ keinen Kern).
            hal::intc::route_spi(intid, core);
            hal::intc::enable_intid(intid);
            return true;
        }
    }
    false
}

/// **Zu welchem Vektor-Index gehört dieser CPU-Vektor?** — die Auflösung für [`dispatch_bind_irq`].
///
/// Durchsucht die Zuteilungen nach dem Block, der `vektor` enthält; der Index ist der Abstand zur
/// Blockbasis. `None` = keine Zuteilung führt diesen Vektor (Zustellprobe, RTC-Test) — dann gibt
/// es auch keinen Index, statt eines geratenen.
fn irq_vidx_fuer_vektor(vektor: u32) -> Option<u32> {
    if vektor > 0xFF {
        return None;
    }
    let g = DRIVER_ASSIGN.lock();
    for a in g.iter().filter(|a| a.used && a.msi_da) {
        let basis = a.msi_basis as u32;
        let n = a.msi_anzahl as u32;
        if vektor >= basis && vektor < basis + n {
            return Some(vektor - basis);
        }
    }
    None
}

/// Anzahl bisher zugestellter Geräte-IRQs (Telemetrie für den `irq`-Test).
pub fn irqs_delivered() -> u64 {
    IRQ_DELIVERED.load(Ordering::Acquire)
}

/// **Angekommene Interrupts und der zuletzt gesehene Vektor** — `(Aufrufe, letzter)`.
///
/// Nicht dasselbe wie [`irqs_delivered`], und der Unterschied ist die ganze Diagnose: hier steht,
/// was den Prozessor erreicht hat, dort, was bis zur Notification gekommen ist. Der Timer und der
/// Resched-IPI kommen hier **nicht** vor -- der x86-Dispatch faengt sie vorher ab.
pub fn irq_hook_stats() -> (u64, u32) {
    (
        IRQ_HOOK_CALLS.load(Ordering::Acquire),
        IRQ_HOOK_LAST.load(Ordering::Acquire),
    )
}

/// **Wie oft der Re-Trigger-Schutz zusammengefasst hat** — zweite Flanke bei noch ausstehendem
/// Slot (s. [`IRQ_ZUSAMMENGEFASST`]). Zusammenfassen ist kein Verlieren: der Drain stellt einmal
/// zu, ganz gleich wie viele Flanken das Bit gesetzt haben.
pub fn irq_zusammengefasst() -> u64 {
    IRQ_ZUSAMMENGEFASST.load(Ordering::Acquire)
}

// --- DMA-Capabilities + DmaEnforcer-Abstraktion (ext-23) ---
//
// DMA ist die einzige HW-Cap-Kategorie, bei der ein bus-masterndes Gerät DIREKT Physikspeicher
// liest/schreibt (vorbei an der CPU-MMU). Der **Mechanismus** (DmaCap) ist vom **Enforcement-
// Treiber** entkoppelt: der Kernel erzwingt Ownership/Bounds/Lifetime/Audit hardware-unabhängig;
// ein `DmaEnforcer` setzt die Isolation ZUSÄTZLICH hardwareseitig durch. SMMUv3 ist in ext-23
// die einzige Implementierung (`SmmuV3Enforcer`, D2/D3); ein künftiger `NullIommuEnforcer` o.a.
// implementiert dasselbe Trait, OHNE öffentlichen Code (DmaCap, install_dma_cap, Mapping,
// Treiber, HardwareLand) zu berühren. Die Revoke-Reihenfolge läuft über die Abstraktion:
// `enforcer.detach` -> VSpace-Unmap -> `free_region` (DMA-use-after-free-sicher).

/// Eine generische **DMA-Bindung**: das Gerät mit `stream_id` darf die RAM-`region` als
/// DMA-Puffer nutzen (besessen von `backend_pd`). Enthält bewusst KEINE enforcer-/SMMU-
/// spezifischen Details — die Schnittstelle ist IOMMU-neutral.
#[derive(Clone, Copy)]
pub struct DmaBinding {
    pub stream_id: u32,
    /// **Physische** Basis der Region — was der Aufrufer besitzt. Die **IOVA** vergibt der
    /// Enforcer aus dem Fenster des Übersetzungskontexts und meldet sie über den Rückgabewert
    /// von [`DmaEnforcer::attach`] zurück; der Aufrufer kann sie nicht wählen.
    pub pa: Pa,
    pub len: u64,
    #[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
    pub backend_pd: usize,
    /// ext-24: Gerät liest nur (read-only -> schreibgeschützt). Default `false` (RW).
    pub ro: bool,
    /// ext-24: Coherent -> Normal-Cacheable. Default `false` (Non-Cacheable, ext-23-Verhalten).
    pub cacheable: bool,
}

impl DmaBinding {
    /// Rückwärtskompatible Bindung (ext-23-Semantik: bidirektional, non-cacheable).
    pub fn new(stream_id: u32, pa: Pa, len: u64) -> Self {
        Self {
            stream_id,
            pa,
            len,
            backend_pd: 0,
            ro: false,
            cacheable: false,
        }
    }
}

/// Treiber-Abstraktion für die **hardwareseitige DMA-Durchsetzung** (IOMMU-neutral). Die
/// einzige Stelle, die konkrete IOMMU-Register/Tabellen kennt, ist die jeweilige Impl
/// (`SmmuV3Enforcer`). Der öffentliche DMA-Pfad spricht ausschließlich dieses Trait an.
pub trait DmaEnforcer: Sync {
    /// Bring-up des Enforcers (einmalig beim Boot). `true` bei Erfolg / vorhandener HW.
    fn init(&self) -> bool;
    /// Durchsetzung für eine Bindung aktivieren: das Gerät darf danach NUR in `binding.region`
    /// DMAen, alles andere wird hardwareseitig abgewiesen. `false` bei Fehler.
    /// Gibt die **zugeteilte IOVA** zurück (`None` bei Fehlschlag). Der Aufrufer erfährt die
    /// Gerätesicht erst hier — sie stammt aus dem Fenster des Übersetzungskontexts.
    fn attach(&self, binding: &DmaBinding) -> Option<Iova>;
    /// Durchsetzung entziehen (VOR VSpace-Unmap + `free_region`): danach kann das Gerät nicht
    /// mehr in die Region DMAen (DMA-use-after-free-sicher).
    fn detach(&self, binding: &DmaBinding);
    /// **Gebündelte Finalisierung** (ext-37): alle genannten Regionen `(pa, len)` stilllegen,
    /// aus den Übersetzungstabellen entfernen und **einmal** synchronisieren. `ok[i]` sagt, ob
    /// Region `i` danach freigegeben werden darf.
    ///
    /// Gebündelt, weil ein `revoke` über einen CDT-Teilbaum N DMA-Caps auf einmal finalisiert:
    /// einzeln durchgereicht wären das N Quiesce/Flush/`CMD_SYNC`-Zyklen — dieselbe Form, die
    /// beim Multi-SID-Detach schon einmal die falsche war. Erst alle entwaffnen, alle spülen,
    /// alle unmappen, **ein** Sync, dann freigeben.
    ///
    /// Default: es gab nie eine Übersetzung (kein Enforcer / `attach` liefert `None`), also darf
    /// alles freigegeben werden. Das ist kein Freibrief, sondern die Wahrheit über x86 heute.
    ///
    /// `ctx_of` ist **geliehene Kratzfläche** (A-3.3): je Region der Index des betroffenen
    /// Übersetzungskontexts, gebraucht zwischen Phase 1 und Phase 4. Sie lag bis hierher als
    /// `[usize; MAX_FINALIZE]` auf dem Kernelstack — dieselbe Kopplung an `NOBJECTS`, die A-3.3
    /// beseitigt, nur eine Ebene tiefer. Wer sie nicht braucht (dieser Default), ignoriert sie.
    fn finalize(&self, regions: &[(u64, u64)], ok: &mut [bool], _ctx_of: &mut [usize]) {
        for b in ok.iter_mut().take(regions.len()) {
            *b = true;
        }
    }
    /// Durchsetzungs-Oracle: `0` = konsistent, sonst enforcer-spezifischer Anomalie-Code.
    fn audit(&self) -> u32;
    /// Ist die hardwareseitige Durchsetzung aktiv (HW vorhanden + initialisiert)?
    #[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
    fn is_active(&self) -> bool;
    /// **IOMMU-Stream-Gruppe** (ext-24): `member` soll fortan denselben Übersetzungskontext
    /// nutzen wie `leader` (z.B. Multi-Function-Gerät / Bridge ohne RID-Translation). Default:
    /// nicht unterstützt (`false`). `true` bei Erfolg.
    fn share_context(&self, _leader: u32, _member: u32) -> bool {
        false
    }
}

// --- SMMUv3-Enforcer (aarch64 / QEMU `virt`) -------------------------------------------------
//
// ext-31: Der SMMUv3 ist ARM-spezifisch. Das `DmaEnforcer`-Trait war von Anfang an
// IOMMU-neutral entworfen ("ein künftiger `NullIommuEnforcer` implementiert dasselbe Trait,
// OHNE öffentlichen Code zu berühren") — genau das wird hier eingelöst: der SMMU-Treiber ist
// aarch64-only, x86_64 bekommt (bis VT-d/AMD-Vi portiert sind) den Null-Enforcer. Der
// öffentliche DMA-Pfad (DmaCap, install_dma_cap, Mapping, Bounds, Lifetime, Audits) bleibt
// architekturunabhängig.
#[cfg(target_arch = "aarch64")]
mod smmu_enforcer_arm {
    use super::*;
    /// **SMMUv3-Enforcer** — die einzige `DmaEnforcer`-Implementierung in ext-23. Hält die
    /// Physadressen der Command-/Event-Queue + linearen Stream-Tabelle sowie den Command-Queue-
    /// PROD-Index. `init` (D2) bringt die SMMU hoch (Default-Abort); `attach`/`detach`
    /// (D3) programmieren je StreamID eine STE -> CD -> Stage-1-Tabelle. Das gesamte SMMU-Wissen
    /// liegt hier + in `hal::smmu`; der übrige Kernel kennt nur das `DmaEnforcer`-Trait.
    pub struct SmmuV3Enforcer {
        active: AtomicBool,
        strtab_phys: AtomicU64,
        cmdq_phys: AtomicU64,
        eventq_phys: AtomicU64,
        cmdq_prod: AtomicU32,
        sync_ok: AtomicBool, // CMD_SYNC-Round-Trip beim Bring-up gelang (D2-Spike)
    }

    impl SmmuV3Enforcer {
        /// STE einer StreamID **direkt** überschreiben — ausschließlich für die
        /// Sensitivitätskontrolle des Kronjuwel-Tests (Bypass-STE, s. `build_ste_bypass`).
        /// Nicht Teil der Durchsetzungs-API: hier wird die Durchsetzung absichtlich aufgehoben.
        pub fn override_ste(&self, sid: u32, ste: &[u64; 8]) -> bool {
            if !self.active.load(Ordering::Acquire) {
                return false;
            }
            let strtab = self.strtab_phys.load(Ordering::Acquire);
            let cmdq = self.cmdq_phys.load(Ordering::Acquire);
            let prod = self.cmdq_prod.load(Ordering::Acquire);
            let (next, ok) = hal::smmu::write_ste_and_sync(strtab, cmdq, prod, sid, ste);
            self.cmdq_prod.store(next, Ordering::Release);
            ok
        }
        /// Die reguläre Stage-1-STE des Kontexts der StreamID wiederherstellen.
        pub fn restore_ste(&self, sid: u32) -> bool {
            let cd = {
                let t = DMA_CTX.lock();
                match t.iter().find(|c| c.used && c.sids.contains(&sid)) {
                    Some(c) => c.cd,
                    None => return false,
                }
            };
            self.override_ste(sid, &hal::smmu::build_ste_stage1(cd))
        }
        pub const fn new() -> Self {
            Self {
                active: AtomicBool::new(false),
                strtab_phys: AtomicU64::new(0),
                cmdq_phys: AtomicU64::new(0),
                eventq_phys: AtomicU64::new(0),
                cmdq_prod: AtomicU32::new(0),
                sync_ok: AtomicBool::new(false),
            }
        }

        /// Einen kontiguierlichen, page-ausgerichteten, **genullten** RAM-Block der Größe `len`
        /// ausschneiden (für Queue-/Tabellen-Speicher der SMMU). `None` bei Erschöpfung.
        fn alloc_zeroed(len: u64) -> Option<u64> {
            let len = (len + 4095) & !4095;
            let base = mem_alloc(len, len.next_power_of_two().max(4096)).map(|c| c.base())?;
            // SAFETY: frisch allozierter, identity-gemappter RAM-Block; exklusiv hier beschrieben.
            unsafe { core::ptr::write_bytes(base as *mut u8, 0, len as usize) };
            hal::cpu::dsb_sy();
            Some(base)
        }

        /// CMD_SYNC-Round-Trip beim Bring-up gelungen? (D2-Spike-Telemetrie.)
        pub fn sync_ok(&self) -> bool {
            self.sync_ok.load(Ordering::Acquire)
        }
    }

    impl DmaEnforcer for SmmuV3Enforcer {
        fn init(&self) -> bool {
            if self.active.load(Ordering::Acquire) {
                return true; // idempotent
            }
            if !hal::smmu::present() {
                return false;
            }
            // Command-/Event-Queue + lineare Stream-Tabelle (genullt -> Default-Abort) ausschneiden.
            let Some(strtab) = Self::alloc_zeroed(hal::smmu::strtab_bytes()) else {
                return false;
            };
            let Some(cmdq) = Self::alloc_zeroed(hal::smmu::cmdq_bytes()) else {
                return false;
            };
            let Some(eventq) = Self::alloc_zeroed(hal::smmu::eventq_bytes()) else {
                return false;
            };
            self.strtab_phys.store(strtab, Ordering::Release);
            self.cmdq_phys.store(cmdq, Ordering::Release);
            self.eventq_phys.store(eventq, Ordering::Release);
            if !hal::smmu::bringup(strtab, cmdq, eventq) {
                return false;
            }
            // Spike: CMD_SYNC-Round-Trip beweist die Command-Queue-Mechanik.
            let (prod, ok) = hal::smmu::cmd_sync(cmdq, 0);
            self.cmdq_prod.store(prod, Ordering::Release);
            self.sync_ok.store(ok, Ordering::Release);
            self.active.store(true, Ordering::Release);
            ok
        }
        fn attach(&self, binding: &DmaBinding) -> Option<Iova> {
            if !self.active.load(Ordering::Acquire) {
                return None;
            }
            let strtab = self.strtab_phys.load(Ordering::Acquire);
            let cmdq = self.cmdq_phys.load(Ordering::Acquire);
            let mut t = DMA_CTX.lock();
            // Kontext finden, der `stream_id` bereits führt; sonst neu anlegen (additiv: mehrere
            // Regionen je Kontext, mehrere StreamIDs je Gruppe).
            let mut ci = t
                .iter()
                .position(|c| c.used && c.sids.contains(&binding.stream_id));
            let is_new_ctx = ci.is_none();
            if ci.is_none() {
                let mut alloc = || SmmuV3Enforcer::alloc_zeroed(4096);
                let Some(l1) = hal::smmu::stage1_create(&mut alloc) else {
                    return None;
                };
                let Some(cd) = SmmuV3Enforcer::alloc_zeroed(hal::smmu::CD_BYTES) else {
                    // Stage-1 (l1) ist bereits alloziert -> freigeben, sonst Frame-Leck.
                    let mut mem = MEM.lock();
                    hal::smmu::free_stage1(l1, &mut |x| {
                        mem.free_region(PhysRegion::new(x, 4096));
                    });
                    return None;
                };
                hal::smmu::write_cd(cd, l1);
                let Some(slot) = t.iter().position(|c| !c.used) else {
                    // l1 + cd sind bereits alloziert (kein DmaCtx-Slot frei) -> beide freigeben.
                    let mut mem = MEM.lock();
                    hal::smmu::free_stage1(l1, &mut |x| {
                        mem.free_region(PhysRegion::new(x, 4096));
                    });
                    mem.free_region(PhysRegion::new(cd, 4096));
                    return None;
                };
                t[slot] = DmaCtx::EMPTY;
                t[slot].used = true;
                t[slot].l1 = l1;
                t[slot].cd = cd;
                t[slot].sids[0] = binding.stream_id;
                ctx_assign_window(&mut t[slot], slot); // IOVA-Fenster dieses Kontexts
                ci = Some(slot);
            }
            let slot = ci.unwrap();
            let (l1, cd) = (t[slot].l1, t[slot].cd);
            // **Hier entsteht die Gerätesicht**: eine IOVA aus dem Fenster des Kontexts, ohne
            // arithmetische Beziehung zur PA (kein `PA + Offset`) — sonst funktionierte ein
            // Passthrough-Enforcer, der die IOVA als PA liest, für einen Teil der Regionen
            // zufällig weiter, und das laute Scheitern fiele aus.
            let iova = match ctx_alloc_iova(
                &mut t[slot],
                slot,
                binding.len,
                device_addr_bits(binding.stream_id),
            ) {
                Ok(v) => v,
                Err(e) => {
                    note_iova_reject(e);
                    if is_new_ctx {
                        let mut mem = MEM.lock();
                        hal::smmu::free_stage1(l1, &mut |x| {
                            mem.free_region(PhysRegion::new(x, 4096));
                        });
                        mem.free_region(PhysRegion::new(cd, 4096));
                        drop(mem);
                        t[slot] = DmaCtx::EMPTY;
                    }
                    return None;
                }
            };
            let region = DmaRegion { iova, pa: binding.pa, len: binding.len };
            // Region richtungs-/kohärenz-spezifisch additiv in die Kontext-Stage-1-Tabelle einhängen.
            let mut alloc = || SmmuV3Enforcer::alloc_zeroed(4096);
            if !hal::smmu::stage1_map_region(
                l1,
                region.iova.raw(), // Index in der Tabelle = Gerätesicht
                region.pa.raw(),   // Blatt-Eintrag = CPU-Sicht
                region.len,
                binding.ro,
                binding.cacheable,
                &mut alloc,
            ) {
                // Ein frisch angelegter Kontext ist jetzt halb aufgebaut (l1 + cd, keine Region, keine
                // STE) -> abbauen, sonst lecken Stage-1 + CD + der DmaCtx-Slot. Bestehender Kontext:
                // unveraendert lassen (er traegt weiter seine anderen Regionen).
                if is_new_ctx {
                    let mut mem = MEM.lock();
                    hal::smmu::free_stage1(l1, &mut |x| {
                        mem.free_region(PhysRegion::new(x, 4096));
                    });
                    mem.free_region(PhysRegion::new(cd, 4096));
                    drop(mem);
                    t[slot] = DmaCtx::EMPTY;
                }
                return None;
            }
            let Some(ri) = t[slot].regs.iter().position(|r| r.is_empty()) else {
                // Region wurde in die Stage-1-Tabelle gemappt, aber es ist kein regs-Slot frei (nur bei
                // einem BESTEHENDEN Kontext moeglich -> dessen regs sind voll; ein neuer Kontext hat
                // leere regs). Das Mapping zuruecknehmen, sonst bleibt es untracked in der Tabelle.
                hal::smmu::stage1_unmap_region(l1, region.iova.raw(), region.len);
                return None;
            };
            t[slot].regs[ri] = region;
            // Neuer Kontext: STE installieren. Sonst: nur TLBI+SYNC (STE zeigt schon auf den CD).
            // Bus-Master erst **nach** der Übersetzung scharf schalten (Reihenfolge: erst darf
            // es irgendwo hin, dann darf es überhaupt). Ohne das wäre ein Gerät nach einem
            // vollständigen Detach/Re-Attach-Zyklus tot — beim letzten Detach bleibt Bus-Master
            // bewusst aus, und niemand sonst setzt es wieder.
            ctx_arm_bus_master(&mut t[slot], binding.stream_id);
            let prod = self.cmdq_prod.load(Ordering::Acquire);
            let (next, ok) = if is_new_ctx {
                let ste = hal::smmu::build_ste_stage1(cd);
                hal::smmu::write_ste_and_sync(strtab, cmdq, prod, binding.stream_id, &ste)
            } else {
                hal::smmu::tlbi_sync(cmdq, prod)
            };
            self.cmdq_prod.store(next, Ordering::Release);
            if ok {
                Some(region.iova)
            } else {
                None
            }
        }
        fn detach(&self, binding: &DmaBinding) {
            if !self.active.load(Ordering::Acquire) {
                return;
            }
            // --- Gerät stilllegen, BEVOR die Übersetzung verschwindet (ext-35) ---
            //
            // Die Reihenfolge war bisher andersherum: erst unmappen, dann (nie) stilllegen. Das
            // hatte zwei Folgen. Erstens erzeugt ein noch aktives Gerät nach dem Unmap
            // Translation Faults statt Korruption — ungefährlich, aber auf mancher HW ein
            // Fault-Sturm, der echte Fehler verdeckt. Zweitens, und das ist der eigentliche
            // Punkt: das Entfernen der Übersetzung sagt **nichts** über bereits übersetzte,
            // unterwegs befindliche (posted) Writes. Genau die trifft `quiesce_by_rid`:
            // Bus-Master löschen (keine NEUEN Requests) + Config-Read vom Gerät (PCIe: eine
            // Completion überholt posted Writes nicht -> die alten sind danach zugestellt).
            //
            // **Arbeitsteilung, kein Ersatz:** Dieser Schritt deckt das *gutartige* Gerät mit
            // In-flight-Writes ab; gegen ein *kompromittiertes*, das `BME` ignoriert, wirkt
            // allein das Entfernen von Stage-1/STE weiter unten. Keiner der beiden Schritte
            // macht den anderen entbehrlich.
            //
            // **Serialisierung:** Der Zähler in `quiesce_by_rid` macht Verschachtelung sicher
            // (entwaffnen bei 0→1, wiederherstellen bei 1→0) — unabhängig davon, dass dieser Pfad
            // heute ohnehin unter `DMA_CTX` serialisiert ist. Ohne den Zähler könnte ein
            // nebenläufiger Teardown desselben Geräts das Bus-Master wieder scharf schalten,
            // bevor der andere seine Region entfernt hat; dessen Flush-Garantie wäre dann wertlos.
            let strtab = self.strtab_phys.load(Ordering::Acquire);
            let cmdq = self.cmdq_phys.load(Ordering::Acquire);
            let mut t = DMA_CTX.lock();
            let Some(slot) = t
                .iter()
                .position(|c| c.used && c.sids.contains(&binding.stream_id))
            else {
                return;
            };
            let _ = ctx_quiesce(&mut t[slot]);
            let l1 = t[slot].l1;
            // Region aus der Stage-1-Tabelle entfernen (danach kann das Gerät NICHT mehr dorthin
            // DMAen) + TLBI+SYNC. DMA-use-after-free-sicher (vor VSpace-Unmap + free_region).
            // Der Aufrufer kennt nur die **PA** — die IOVA steht in der aufgezeichneten Region.
            // (Sie aus der PA zu berechnen ginge nicht und soll auch nicht gehen: zwischen den
            // beiden Achsen besteht bewusst keine arithmetische Beziehung.)
            if let Some(ri) = t[slot]
                .regs
                .iter()
                .position(|r| !r.is_empty() && r.pa == binding.pa && r.len == binding.len)
            {
                hal::smmu::stage1_unmap_region(l1, t[slot].regs[ri].iova.raw(), t[slot].regs[ri].len);
                t[slot].regs[ri] = DmaRegion::EMPTY;
            }
            let prod = self.cmdq_prod.load(Ordering::Acquire);
            let (next, _) = hal::smmu::tlbi_sync(cmdq, prod);
            self.cmdq_prod.store(next, Ordering::Release);
            // Bus-Master wiederherstellen, **solange der Kontext noch Regionen trägt** — dann ist
            // das Gerät legitim weiter in Betrieb und die eben entfernte Region ist bereits nicht
            // mehr erreichbar. Hat es keine Region mehr, bleibt BME aus (fail-safe: ein Gerät ohne
            // jede Zuteilung soll auch keine Requests absetzen dürfen).
            // Bus-Master wiederherstellen, **solange der Kontext noch Regionen trägt** — dann ist
            // das Gerät legitim weiter in Betrieb und die eben entfernte Region ist ab dem
            // TLBI+SYNC ohnehin unerreichbar. Hat es keine Region mehr, bleibt Bus-Master **aus**
            // (fail-safe). Die Kopplung an die ATS-Entscheidung steht bei `release_quiesce`.
            // Bedingung **beim Restore** ausgewertet, nicht beim Entwaffnen gecacht: die
            // Pending-Intent-Mechanik kann Quiesce-Tiefe und Regionenzahl auseinanderziehen (ein
            // `attach` bei Tiefe > 0 erhöht die Regionen, ohne die Tiefe zu ändern). Effektiv gilt
            // damit  BME_neu = gespeichertes_BME  UND  Regionen(ctx) > 0,  beides zum Zeitpunkt
            // 1→0 unter demselben Lock.
            let still_in_use = !t[slot].regs.iter().all(|r| r.is_empty());
            ctx_release(&mut t[slot], still_in_use);
            // Letzte Region weg -> Kontext abbauen: alle STEs der Gruppe invalidieren, Stage-1 + CD frei.
            if t[slot].regs.iter().all(|r| r.is_empty()) {
                let mut p = self.cmdq_prod.load(Ordering::Acquire);
                for k in 0..MAX_CTX_SIDS {
                    let sid = t[slot].sids[k];
                    if sid != u32::MAX {
                        let (n, _) = hal::smmu::clear_ste_and_sync(strtab, cmdq, p, sid);
                        p = n;
                    }
                }
                self.cmdq_prod.store(p, Ordering::Release);
                let (cl1, ccd) = (t[slot].l1, t[slot].cd);
                {
                    let mut mem = MEM.lock();
                    hal::smmu::free_stage1(cl1, &mut |x| {
                        mem.free_region(PhysRegion::new(x, 4096));
                    });
                    mem.free_region(PhysRegion::new(ccd, 4096));
                }
                t[slot] = DmaCtx::EMPTY;
            }
        }
        fn finalize(&self, regions: &[(u64, u64)], ok: &mut [bool], ctx_of: &mut [usize]) {
            if !self.active.load(Ordering::Acquire) {
                for b in ok.iter_mut().take(regions.len()) {
                    *b = true; // keine Durchsetzung aktiv -> es gab nie eine Übersetzung
                }
                return;
            }
            let strtab = self.strtab_phys.load(Ordering::Acquire);
            let cmdq = self.cmdq_phys.load(Ordering::Acquire);
            let mut t = DMA_CTX.lock();

            // Phase 1 — **alle** betroffenen Kontexte stilllegen (je Kontext alle StreamIDs).
            // Ein Kontext kann mehrere der Regionen tragen; die Stilllegung ist verschachtelbar,
            // also wird sie hier einmal je *Region* genommen und in Phase 4 ebenso oft gelöst.
            // `ctx_of` kommt vom Aufrufer (A-3.3). `quiesced` gab es hier als zweites Array
            // derselben Groesse -- gelesen wurde es nie ausser in der Zeile darunter; eine lokale
            // Variable tut dasselbe.
            let nfin = regions.len().min(ok.len()).min(ctx_of.len());
            for e in ctx_of[..nfin].iter_mut() {
                *e = usize::MAX;
            }
            for (i, &(pa, len)) in regions.iter().enumerate().take(nfin) {
                let Some(slot) = t.iter().position(|c| {
                    c.used
                        && c.regs
                            .iter()
                            .any(|r| !r.is_empty() && r.pa == Pa::new(pa) && r.len == len)
                }) else {
                    // Keine lebende Übersetzung (z. B. bereits explizit detacht) -> nichts zu
                    // entwaffnen, Freigabe zulässig.
                    ok[i] = true;
                    continue;
                };
                ctx_of[i] = slot;
                ok[i] = ctx_quiesce(&mut t[slot]);
            }

            // Phase 2 — alle Regionen unmappen (IOVA-Achse). Der Unmap ist **immer** zwingend,
            // auch wenn die Stilllegung nicht bestätigt ist: eine stehende Übersetzung auf eine
            // Region, die gleich freigegeben würde, ist genau der geräteseitige Use-after-free.
            // Zurückgehalten wird nur die *Freigabe*.
            for (i, &(pa, len)) in regions.iter().enumerate().take(nfin) {
                let slot = ctx_of[i];
                if slot == usize::MAX {
                    continue;
                }
                let l1 = t[slot].l1;
                if let Some(ri) = t[slot]
                    .regs
                    .iter()
                    .position(|r| !r.is_empty() && r.pa == Pa::new(pa) && r.len == len)
                {
                    hal::smmu::stage1_unmap_region(l1, t[slot].regs[ri].iova.raw(), t[slot].regs[ri].len);
                    t[slot].regs[ri] = DmaRegion::EMPTY;
                }
            }

            // Phase 3 — **ein** TLBI+SYNC für den ganzen Stapel.
            let prod = self.cmdq_prod.load(Ordering::Acquire);
            let (next, _) = hal::smmu::tlbi_sync(cmdq, prod);
            self.cmdq_prod.store(next, Ordering::Release);

            // Phase 4 — Stilllegung lösen und leere Kontexte abbauen.
            for i in 0..nfin {
                let slot = ctx_of[i];
                if slot == usize::MAX || !t[slot].used {
                    continue;
                }
                let still_in_use = !t[slot].regs.iter().all(|r| r.is_empty());
                ctx_release(&mut t[slot], still_in_use);
                if !still_in_use {
                    let mut p = self.cmdq_prod.load(Ordering::Acquire);
                    for k in 0..MAX_CTX_SIDS {
                        let sid = t[slot].sids[k];
                        if sid != u32::MAX {
                            let (n, _) = hal::smmu::clear_ste_and_sync(strtab, cmdq, p, sid);
                            p = n;
                        }
                    }
                    self.cmdq_prod.store(p, Ordering::Release);
                    let (cl1, ccd) = (t[slot].l1, t[slot].cd);
                    {
                        let mut mem = MEM.lock();
                        hal::smmu::free_stage1(cl1, &mut |x| {
                            mem.free_region(PhysRegion::new(x, 4096));
                        });
                        mem.free_region(PhysRegion::new(ccd, 4096));
                    }
                    t[slot] = DmaCtx::EMPTY;
                }
            }
        }
        fn audit(&self) -> u32 {
            if !self.active.load(Ordering::Acquire) {
                return 0; // nicht initialisiert -> keine Durchsetzungs-Aussage (D0/D1)
            }
            // Aktiv: keine globalen Fehler + keine unerwarteten Translation-Faults.
            if hal::smmu::gerror() != 0 {
                return 1;
            }
            if !hal::iommu::faults_empty() {
                return 2;
            }
            0
        }
        fn is_active(&self) -> bool {
            self.active.load(Ordering::Acquire)
        }
        fn share_context(&self, leader: u32, member: u32) -> bool {
            if !self.active.load(Ordering::Acquire) {
                return false;
            }
            let strtab = self.strtab_phys.load(Ordering::Acquire);
            let cmdq = self.cmdq_phys.load(Ordering::Acquire);
            let mut t = DMA_CTX.lock();
            let Some(slot) = t.iter().position(|c| c.used && c.sids.contains(&leader)) else {
                return false;
            };
            if t[slot].sids.contains(&member) {
                return true; // schon in der Gruppe
            }
            let Some(si) = t[slot].sids.iter().position(|&s| s == u32::MAX) else {
                return false; // Gruppe voll
            };
            let cd = t[slot].cd;
            let ste = hal::smmu::build_ste_stage1(cd);
            let prod = self.cmdq_prod.load(Ordering::Acquire);
            let (next, ok) = hal::smmu::write_ste_and_sync(strtab, cmdq, prod, member, &ste);
            self.cmdq_prod.store(next, Ordering::Release);
            if ok {
                t[slot].sids[si] = member;
            }
            ok
        }
    }

    
    // DMA-Kontext-Telemetrie (dma_ctx_region_count/sid_count/stage1) liegt in `mod testsupport`
    // (Konsolidierung K5: Test-/Telemetrie-API von der verifizierten Kernschnittstelle getrennt).

}
#[cfg(target_arch = "aarch64")]
pub use smmu_enforcer_arm::SmmuV3Enforcer;

/// **VT-d-Enforcer** (x86_64) — Gegenstück zum SMMUv3-Treiber auf ARM.
///
/// `init` bringt die Remapping-Einheit mit **Default-Block** hoch: Root-Tabelle mit lauter
/// „not present"-Einträgen, dann `SRTP` + `TE`. Ab da ist die Übersetzung aktiv und **jede**
/// nicht ausdrücklich zugeteilte DMA-Anforderung wird von der Hardware abgewiesen — genau die
/// Aussage, die der `smmu`-Test auf ARM prüft.
///
/// `attach` meldet (noch) `false`: die per-Gerät-Zuteilung (Kontext-Einträge +
/// Second-Level-Tabellen je Domäne) fehlt. Auf x86 lässt sich also derzeit **kein** DMA-Puffer
/// an ein Gerät binden — der Zustand ist aber **sicher** (geblockt statt ungeschützt), und das
/// System sagt es, statt eine Isolation vorzutäuschen. Die softwareseitigen Garantien
/// (Ownership, Bounds, Revoke-Reihenfolge, `dma_audit`) sind davon unberührt.
#[cfg(not(target_arch = "aarch64"))]
pub struct VtdEnforcer;

#[cfg(not(target_arch = "aarch64"))]
impl VtdEnforcer {
    pub const fn new() -> Self {
        Self
    }
    /// Ein genulltes, ausgerichtetes Frame für Root-/Kontext-/SLPT-Tabellen.
    fn alloc_zeroed(len: u64) -> Option<u64> {
        let len = (len + 4095) & !4095;
        let base = mem_alloc(len, len.next_power_of_two().max(4096)).map(|c| c.base())?;
        // SAFETY: frisch allozierter, identity-gemappter RAM-Block; exklusiv hier beschrieben.
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, len as usize) };
        Some(base)
    }
}

/// Domain-IDs werden **ab 1** vergeben.
///
/// Bei `CAP.CM == 1` — dem Zustand dieses Aufbaus — reserviert die Spezifikation DID 0 für das
/// Caching nicht-präsenter Einträge. Das ist damit kein theoretischer Fall, sondern eine
/// Bedingung, unter der die Vergabe steht.
#[cfg(not(target_arch = "aarch64"))]
static NEXT_DID: AtomicU32 = AtomicU32::new(1);

/// Die **Requester-IDs der Isolationsgruppe**, zu der `stream_id` gehört.
///
/// Die Gruppe ist die Isolationsgranularität, nicht das Gerät — und ihre RID-Menge enthält auch
/// die RID einer Bridge, hinter der die eigentlichen Endpunkte sitzen. Für die muss ein
/// Kontexteintrag existieren, obwohl dort kein zuteilbares Gerät ist: sonst kämen genau die
/// Transaktionen ungeleitet an, die die Bridge-RID tragen.
///
/// Umgekehrt beim Abbau — alle Einträge räumen — und beim Stilllegen: **alle** RIDs der Gruppe
/// entwaffnen, nicht nur die auslösende. Das ist die ARM-Falle aus `13810e9`, wörtlich; sie wird
/// hier dadurch vermieden, dass die Gruppen-RIDs in `DmaCtx::sids` landen und damit durch
/// dieselbe `ctx_quiesce`-Schleife laufen.
#[cfg(not(target_arch = "aarch64"))]
fn group_rids(stream_id: u32, out: &mut [u32]) -> usize {
    let Some(tbl) = hal::acpi::dmar_table() else {
        out[0] = stream_id;
        return 1;
    };
    let info = hal::dmar::parse(tbl);
    let mut topo = [hal::dmar::DevNode::EMPTY; hal::dmar::MAX_DEVS];
    let n = hal::pcie::read_topology(&mut topo);
    let g = hal::dmar::build_groups(&info, &topo[..n]);
    let Some(i) = (0..n).find(|&i| topo[i].rid() == stream_id) else {
        out[0] = stream_id;
        return 1;
    };
    if g.excluded[i].is_some() {
        return 0; // nicht zuteilbar (RMRR / keine Einheit / Gruppe über Einheiten)
    }
    let gi = g.group_of[i];
    let mut k = 0;
    for r in 0..g.n_aliases[gi] {
        if k < out.len() {
            out[k] = g.aliases[gi][r];
            k += 1;
        }
    }
    k
}

#[cfg(not(target_arch = "aarch64"))]
impl DmaEnforcer for VtdEnforcer {
    fn init(&self) -> bool {
        if !hal::vtd::discover() {
            return false; // Plattform ohne IOMMU
        }
        // Root-Tabelle: ein genulltes Frame = alle 256 Einträge „not present" = Default-Block.
        let Some(root) = mem_alloc(4096, 4096) else {
            return false;
        };
        let mut alloc = || VtdEnforcer::alloc_zeroed(4096);
        hal::vtd::init(root.base(), &mut alloc)
    }

    fn attach(&self, binding: &DmaBinding) -> Option<Iova> {
        let caps = hal::vtd::caps_common()?;
        if !caps.usable() || !hal::vtd::enabled() {
            return None;
        }
        let root = hal::vtd::root_table();
        if root == 0 {
            return None;
        }
        // Die RID-Menge der Gruppe — leer heißt: nicht zuteilbar, und das ist ein Fehlschlag,
        // keine Zuteilung mit Lücke.
        let mut rids = [0u32; MAX_CTX_SIDS];
        let n_rids = group_rids(binding.stream_id, &mut rids);
        if n_rids == 0 {
            return None;
        }
        let levels = caps.agaw_levels;
        let mut t = DMA_CTX.lock();
        let mut ci = t
            .iter()
            .position(|c| c.used && c.sids.contains(&binding.stream_id));
        let is_new_ctx = ci.is_none();
        if ci.is_none() {
            let slpt = Self::alloc_zeroed(4096)?;
            let Some(slot) = t.iter().position(|c| !c.used) else {
                let mut mem = MEM.lock();
                mem.free_region(PhysRegion::new(slpt, 4096));
                return None;
            };
            t[slot] = DmaCtx::EMPTY;
            t[slot].used = true;
            t[slot].l1 = slpt; //                      x86: Wurzel der Second-Level-Pagetable
            t[slot].cd = NEXT_DID.fetch_add(1, Ordering::Relaxed) as u64; // x86: Domain-ID
            // **Alle** RIDs der Gruppe eintragen, nicht nur die auslösende.
            for (k, &r) in rids[..n_rids].iter().enumerate() {
                t[slot].sids[k] = r;
            }
            ctx_assign_window(&mut t[slot], slot);
            ci = Some(slot);
        }
        let slot = ci.unwrap();
        let (slpt, did) = (t[slot].l1, t[slot].cd as u16);
        let iova = match ctx_alloc_iova(
            &mut t[slot],
            slot,
            binding.len,
            device_addr_bits(binding.stream_id),
        ) {
            Ok(v) => v,
            Err(e) => {
                note_iova_reject(e);
                if is_new_ctx {
                    let mut mem = MEM.lock();
                    mem.free_region(PhysRegion::new(slpt, 4096));
                    drop(mem);
                    t[slot] = DmaCtx::EMPTY;
                }
                return None;
            }
        };
        // Richtung fällt heraus statt hinzuzukommen: Präsenz *ist* R|W. `ro` heißt „das Gerät
        // liest nur" -> kein W-Bit, und der Puffer ist gegen das Gerät schreibgeschützt.
        let mut alloc = || VtdEnforcer::alloc_zeroed(4096);
        // SAFETY: `slpt` ist eine frisch genullte, identity-gemappte Tabellenwurzel; `alloc`
        // liefert ebensolche Frames.
        let mapped = unsafe {
            hal::vtd::slpt_map(
                slpt,
                levels,
                iova.raw(),
                binding.pa.raw(),
                binding.len,
                true,
                !binding.ro,
                &mut alloc,
            )
        };
        if !mapped {
            if is_new_ctx {
                let mut mem = MEM.lock();
                mem.free_region(PhysRegion::new(slpt, 4096));
                drop(mem);
                t[slot] = DmaCtx::EMPTY;
            }
            return None;
        }
        let region = DmaRegion { iova, pa: binding.pa, len: binding.len };
        let Some(ri) = t[slot].regs.iter().position(|r| r.is_empty()) else {
            // SAFETY: eben gemappt, dieselbe Tabelle.
            unsafe { hal::vtd::slpt_unmap(slpt, levels, iova.raw(), binding.len) };
            return None;
        };
        t[slot].regs[ri] = region;
        if is_new_ctx {
            for k in 0..MAX_CTX_SIDS {
                let r = t[slot].sids[k];
                if r == u32::MAX {
                    continue;
                }
                // SAFETY: `root` stammt aus dem Bring-up und ist identity-gemappt.
                if !unsafe { hal::vtd::context_set(root, r, slpt, did, levels, &mut alloc) } {
                    return None;
                }
            }
        }
        ctx_arm_bus_master(&mut t[slot], binding.stream_id);
        // Unbedingt invalidieren — Kontext-Cache vor IOTLB. Mit `CAP.CM == 1` ist das auch nach
        // dem **Anlegen** Pflicht; die konservative Variante hält auch dort, wo sie es nicht ist.
        if !hal::vtd::sync_tables() {
            return None;
        }
        Some(iova)
    }

    fn detach(&self, binding: &DmaBinding) {
        // **Ein Teardown, der nicht laufen kann, darf nicht still zurueckkehren.**
        //
        // Bis B-3.3 war `caps_common()` praktisch immer `Some`. Seit die Faehigkeiten das Minimum
        // ueber ALLE Einheiten sind und eine stumme Einheit erkannt wird, kann es `None` werden --
        // und dann bliebe hier eine Uebersetzung stehen, waehrend die Region freigegeben wird.
        // Genau die Lage, gegen die das Teardown-Token existiert, nur ohne jede Meldung.
        //
        // Also zaehlen und ueber `dma_audit` Code 8 sichtbar machen. Ein stilles `return` waere
        // die schlimmere Variante desselben Fehlers: kein Schutz UND kein Hinweis darauf.
        let Some(caps) = hal::vtd::caps_common() else {
            DETACH_WITHOUT_CAPS.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let root = hal::vtd::root_table();
        let mut t = DMA_CTX.lock();
        let Some(slot) = t
            .iter()
            .position(|c| c.used && c.sids.contains(&binding.stream_id))
        else {
            return;
        };
        let _ = ctx_quiesce(&mut t[slot]); // über ALLE RIDs der Gruppe
        let slpt = t[slot].l1;
        if let Some(ri) = t[slot]
            .regs
            .iter()
            .position(|r| !r.is_empty() && r.pa == binding.pa && r.len == binding.len)
        {
            // SAFETY: gültige Tabellenwurzel, Region stammt aus dieser Tabelle.
            unsafe {
                hal::vtd::slpt_unmap(slpt, caps.agaw_levels, t[slot].regs[ri].iova.raw(), t[slot].regs[ri].len)
            };
            t[slot].regs[ri] = DmaRegion::EMPTY;
        }
        hal::vtd::sync_tables();
        let still_in_use = !t[slot].regs.iter().all(|r| r.is_empty());
        ctx_release(&mut t[slot], still_in_use);
        if !still_in_use {
            vtd_teardown_ctx(&mut t[slot], root, caps.agaw_levels);
        }
    }

    fn finalize(&self, regions: &[(u64, u64)], ok: &mut [bool], ctx_of: &mut [usize]) {
        let Some(caps) = hal::vtd::caps_common() else {
            for b in ok.iter_mut().take(regions.len()) {
                *b = true; // keine Einheit -> es gab nie eine Übersetzung
            }
            return;
        };
        let root = hal::vtd::root_table();
        let levels = caps.agaw_levels;
        let mut t = DMA_CTX.lock();
        // Kratzflaeche vom Aufrufer (A-3.3) statt `[usize; MAX_FINALIZE]` auf dem Kernelstack.
        let nfin = regions.len().min(ok.len()).min(ctx_of.len());
        for e in ctx_of[..nfin].iter_mut() {
            *e = usize::MAX;
        }
        // Phase 1: alle betroffenen Kontexte stilllegen (je Kontext alle RIDs der Gruppe).
        for (i, &(pa, len)) in regions.iter().enumerate().take(nfin) {
            let Some(slot) = t.iter().position(|c| {
                c.used
                    && c.regs
                        .iter()
                        .any(|r| !r.is_empty() && r.pa == Pa::new(pa) && r.len == len)
            }) else {
                ok[i] = true;
                continue;
            };
            ctx_of[i] = slot;
            ok[i] = ctx_quiesce(&mut t[slot]);
        }
        // Phase 2: alle Regionen unmappen — immer, auch ohne bestätigte Stilllegung.
        for (i, &(pa, len)) in regions.iter().enumerate().take(nfin) {
            let slot = ctx_of[i];
            if slot == usize::MAX {
                continue;
            }
            let slpt = t[slot].l1;
            if let Some(ri) = t[slot]
                .regs
                .iter()
                .position(|r| !r.is_empty() && r.pa == Pa::new(pa) && r.len == len)
            {
                // SAFETY: gültige Tabellenwurzel.
                unsafe {
                    hal::vtd::slpt_unmap(slpt, levels, t[slot].regs[ri].iova.raw(), t[slot].regs[ri].len)
                };
                t[slot].regs[ri] = DmaRegion::EMPTY;
            }
        }
        // Phase 3: **eine** Invalidierung für den ganzen Stapel.
        hal::vtd::sync_tables();
        // Phase 4: Stilllegung lösen, leere Kontexte abbauen.
        for i in 0..nfin {
            let slot = ctx_of[i];
            if slot == usize::MAX || !t[slot].used {
                continue;
            }
            let still_in_use = !t[slot].regs.iter().all(|r| r.is_empty());
            ctx_release(&mut t[slot], still_in_use);
            if !still_in_use {
                vtd_teardown_ctx(&mut t[slot], root, levels);
            }
        }
    }

    fn audit(&self) -> u32 {
        // Ist die Einheit hochgefahren, MUSS die Übersetzung aktiv sein — sonst liefe DMA
        // ungeschützt, obwohl der Kernel meint, sie sei an.
        if hal::vtd::present() && !hal::vtd::enabled() {
            return 1;
        }
        if hal::vtd::present() && !hal::iommu::faults_empty() {
            return 2;
        }
        0
    }
    fn is_active(&self) -> bool {
        hal::vtd::enabled()
    }
}

/// Einen leeren Übersetzungskontext abbauen: **alle** Kontexteinträge der Gruppe räumen,
/// invalidieren, dann die Tabellen freigeben.
#[cfg(not(target_arch = "aarch64"))]
fn vtd_teardown_ctx(c: &mut DmaCtx, root: u64, levels: u32) {
    for k in 0..MAX_CTX_SIDS {
        let r = c.sids[k];
        if r != u32::MAX && root != 0 {
            // SAFETY: gültige Root-Tabelle.
            unsafe { hal::vtd::context_clear(root, r) };
        }
    }
    hal::vtd::sync_tables();
    let slpt = c.l1;
    {
        let mut mem = MEM.lock();
        // SAFETY: `slpt` ist die Wurzel dieses Kontexts, ab hier von niemandem mehr referenziert.
        unsafe {
            hal::vtd::slpt_free(slpt, levels, &mut |x| {
                mem.free_region(PhysRegion::new(x, 4096));
            })
        };
    }
    *c = DmaCtx::EMPTY;
}

/// Granularität der Guard-Bänder **und** der IOVA-Ausrichtung.
///
/// Bewusst die größte Block-Granularität der Stage-1 und nicht 4 KiB: läge eine Region nur
/// seitengenau von ihrer Nachbarin getrennt, könnte ein Block-Mapping über die Lücke
/// hinwegreichen — die Eindämmung stünde auf dem Papier und nicht in der Tabelle.
///
/// **Der Wert hängt an der Granule-Wahl.** 2 MiB ist der Block bei 4-KiB-Granule, und der
/// Kernel fährt Stage 1 ausschließlich mit 4 KiB (`CD.TG0 = 0`, s. `write_cd`). Bei 64-KiB-
/// Granule wäre der Block 512 MiB, und ein 2-MiB-Band ließe sich von einem Block-Mapping
/// überbrücken — dann wäre dieser Wert falsch, nicht bloß knapp. Wer `TG0` ändert, muss ihn
/// mitändern.
const IOVA_GUARD: u64 = 2 * 1024 * 1024;

/// Eingangsbreite der Stage-1 (`CD.T0SZ = 25` → 39 Bit). Jede IOVA muss darunter bleiben.
const S1_INPUT_LIMIT: u64 = 1u64 << 39;

/// Hat die SMMU-Event-Queue in diesem Lauf **nachweislich gesprochen**?
///
/// Ein Oracle, das über Abwesenheit entscheidet („keine Faults"), braucht als Vorbedingung den
/// Nachweis, dass es überhaupt sprechen kann. Zweimal in Folge hat genau diese Lücke hier
/// zugeschlagen: `dma_audit` Code 4 verglich vor der Korrektur gegen die falsche Achse und wäre
/// nach Schritt b still grün geblieben, und die Event-Queue wäre ohne `CD.R` strukturell leer
/// gewesen. Beide Male bedeutete Grün nichts.
///
/// Der Nachweis lässt sich beim Hochlauf **nicht** führen: ein Übersetzungsfehler entsteht nur
/// durch eine echte Bus-Master-Anforderung, und die SMMUv3 bietet keinen Weg, eine zu erzeugen
/// (`ATOS` liefert das Ergebnis ins `PAR`, nicht in die Event-Queue, und QEMU implementiert es
/// nicht). Die erreichbare Form ist deshalb der Nachweis **im selben Lauf**: der Negativtest
/// erzeugt einen echten `F_TRANSLATION`, und erst danach zählt „Queue leer" als Aussage.
static EVTQ_LIVENESS: AtomicBool = AtomicBool::new(false);

/// Erste Fensterbasis oberhalb des RAM, 2-MiB-ausgerichtet.
///
/// **Warum oberhalb des RAM:** dann kann eine IOVA nie zugleich eine gültige PA sein. Eine
/// vertauschte Achse ist damit nicht nur im Audit auffällig (`dma_audit` Code 5), sondern in
/// **jedem** Pfad sofort ein Fault — die Zahl ist in keiner der beiden Rollen plausibel.
///
/// Die Obergrenze ist die Eingangsbreite der Stage-1 (`CD.T0SZ = 25` → 39 Bit). `None`, wenn
/// oberhalb des RAM kein Fenster mehr in diese Breite passt — dann bekommt der Kontext ein
/// schwaches Fenster (s. `ctx_assign_window`).
fn strong_window_base() -> Option<u64> {
    let top = RAM_TOP.load(Ordering::Acquire);
    let mut base = (top + IOVA_GUARD - 1) & !(IOVA_GUARD - 1);
    // **B-3.4: den Interrupt-Nachrichtenbereich ueberspringen.**
    //
    // Auf x86 behandelt VT-d eine DMA-Schreibung nach `0xFEE0_0000..0xFEF0_0000` als
    // Interrupt-Nachricht und befragt die Uebersetzung gar nicht. Eine IOVA dort ist damit
    // unbenutzbar — und zwar auf die unangenehmste Art: die Seitentabellen saehen richtig aus,
    // das Geraet erzeugte trotzdem Interrupts statt Speicherzugriffe. Kein Fault, kein Eintrag in
    // der Fehlerwarteschlange, nur Daten, die nirgends ankommen.
    //
    // Gewaehlt ist die **strukturelle** Loesung: das Fenster faengt oberhalb an, statt bei jeder
    // Vergabe zu pruefen. Eine Bedingung, die nicht gelten KANN, ist besser als eine, die an jeder
    // Vergabestelle richtig geprueft werden muss — die naechste Vergabestelle vergisst sie.
    // Der Preis sind ein paar GiB ungenutzter IOVA-Raum von 39 Bit. Das ist kein Preis.
    if let Some((msi, len)) = hal::iommu::interrupt_message_window() {
        let ende = msi + len;
        if base < ende {
            base = (ende + IOVA_GUARD - 1) & !(IOVA_GUARD - 1);
        }
    }
    // Reserve, damit auch ein paar Regionen hineinpassen.
    if base + 16 * IOVA_GUARD < S1_INPUT_LIMIT {
        Some(base)
    } else {
        None
    }
}

// `ndma_ctx()` ist entfallen: der einzige Aufrufer war `(0..ndma_ctx()).all(iova_window_clear_of_msi)`
// in `dmatests`, und der Lauf ueber alle Slots gehoert jetzt nach `dma_msi_windows` — dort, wo die
// Slotzahl auch die Kontexttabelle indiziert. Ein Accessor ohne Leser ist eine Behauptung ueber
// eine Unterscheidung, die nirgends wirkt.

/// Wie steht das IOVA-Fenster eines Slots zum Interrupt-Nachrichtenbereich? (B-3.4)
///
/// **Warum das kein `bool` mehr ist.** Ein `bool` kann nur „frei" und „nicht frei" sagen — und
/// genau daran ist die Vorgaengerfassung gescheitert. Sie gab im schwachen Zweig `true` zurueck,
/// mit der Begruendung „kein starkes Fenster -> es gibt nichts zu ueberlappen". Das stimmt nur,
/// wenn es GAR KEIN Fenster gibt. Tatsaechlich ist das schwache Fenster `[0, S1_INPUT_LIMIT)`
/// und **enthaelt** den Sperrbereich; der Pruefer gab also Schweigen als Erfolg aus. Dieselbe
/// Form wie die leere Event-Queue ohne `CD.R` und wie `virtio-rng` als angeblicher Beleg fuer
/// die Leserichtung: eine Aussage sieht wahr aus, weil der Fall, der sie widerlegen koennte,
/// im Pruefer gar nicht vorkommt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsiClearance {
    /// **Geprueft und frei**: das Fenster liegt vollstaendig ausserhalb des Sperrbereichs.
    Clear,
    /// **Geprueft und verletzt**: das Fenster schneidet den Sperrbereich. Eine IOVA daraus liest
    /// VT-d als Interrupt-Nachricht und uebersetzt sie gar nicht — kein Fault, keine Fehlerzeile,
    /// nur Daten, die nirgends ankommen.
    Overlaps,
    /// **Nicht entscheidbar**: dem Pruefer fehlt die Grundlage. Wird als SKIP berichtet und
    /// zaehlt **nicht** als bestanden — „nicht messbar" ist kein bestandener Test.
    Undecidable,
}

impl Default for MsiClearance {
    /// Fail-closed: ein nie gesetztes Ergebnis darf nicht als „bestanden" durchgehen. `Clear` als
    /// Vorgabe waere genau der Fehler, der hier gerade behoben wird, nur eine Ebene hoeher.
    fn default() -> Self {
        MsiClearance::Undecidable
    }
}

impl MsiClearance {
    /// Das schlechtere von zwei Urteilen. **Reihenfolge mit Absicht:** kaputt schlaegt
    /// unentscheidbar schlaegt frei — dieselbe Ordnung wie in `pprobe`, wo ein SKIP nie einen
    /// Bilanzfehler verdecken darf.
    pub fn worse(self, other: MsiClearance) -> MsiClearance {
        use MsiClearance::*;
        match (self, other) {
            (Overlaps, _) | (_, Overlaps) => Overlaps,
            (Undecidable, _) | (_, Undecidable) => Undecidable,
            _ => Clear,
        }
    }
}

/// **B-3.4-Nachweis:** wie steht das Fenster des Slots `slot` zum Interrupt-Nachrichtenbereich?
///
/// Als eigene Funktion, damit die Aussage pruefbar ist und nicht bloss aus der Konstruktion folgt.
/// „Folgt aus der Konstruktion" ist genau die Sorte Begruendung, die nach der naechsten Aenderung
/// nicht mehr stimmt und die niemand nachrechnet.
///
/// Geprueft wird das Fenster, das [`slot_window`] dem Slot gibt — **dieselbe** Funktion, die auch
/// `ctx_assign_window` benutzt. Vorher rechnete der Pruefer die Fensterlage selbst nach und
/// bildete dabei nur den *starken* Zweig ab; die beiden konnten also auseinanderlaufen, und sie
/// sind es. Ein Pruefer, der die gepruefte Groesse nachbaut statt sie zu lesen, prueft seine
/// eigene Kopie.
pub fn iova_window_clear_of_msi(slot: usize) -> MsiClearance {
    let Some((msi, len)) = hal::iommu::interrupt_message_window() else {
        // Das ist eine **Zusage** der HAL und keine Unkenntnis: „diese Architektur hat keinen
        // Bereich, der die Uebersetzung umgeht" (s. `aarch64::iommu`, ITS-Doorbell). Damit ist
        // die Frage beantwortet, nicht uebersprungen.
        return MsiClearance::Clear;
    };
    // **Sprechfaehigkeit vor Urteil.** Ohne bekannte RAM-Oberkante ist jedes hier gerechnete
    // Fenster eine Fiktion: `strong_window_base()` liefert dann eine Basis, die mit dem spaeter
    // wirklich vergebenen Fenster nichts zu tun hat, und ein „frei" darueber waere eine Aussage
    // ueber eine Zahl, die es noch nicht gibt.
    if RAM_TOP.load(Ordering::Acquire) == 0 {
        return MsiClearance::Undecidable;
    }
    let (base, limit, _strong) = slot_window(slot);
    if limit <= base {
        // Entartetes Fenster (Groesse 0): der Slot kann keine IOVA vergeben, die Aussage „das
        // Fenster meidet den Bereich" hat keinen Inhalt. Kein Erfolg, sondern kein Befund.
        return MsiClearance::Undecidable;
    }
    if base >= msi + len || limit <= msi {
        MsiClearance::Clear
    } else {
        MsiClearance::Overlaps
    }
}

/// Die B-3.4-Lage ueber **alle** Kontext-Slots, plus die Zahlen, die den Befund erklaeren.
#[derive(Clone, Copy, Default)]
pub struct MsiWindows {
    /// Urteil ueber die Fenster-**Politik**: was [`slot_window`] jedem der `NDMA_CTX` Slots gibt,
    /// unabhaengig davon, ob dort gerade ein Kontext sitzt. Das ist die tragende Aussage — ein
    /// Pruefer, der nur belegte Slots ansaehe, haette in einem Lauf ohne DMA-Kontext gar nichts
    /// geprueft und trotzdem gruen gemeldet.
    pub policy: MsiClearance,
    /// Belegte Kontexte zum Messzeitpunkt.
    pub live: u32,
    /// Davon auf einem **schwachen** Fenster (`DmaCtx::strong_window == false`).
    pub weak: u32,
    /// Belegte Kontexte, deren eingetragenes Fenster von dem abweicht, das die Politik fuer
    /// ihren Slot vorsieht (Basis, Grenze **oder** `strong_window`). Muss 0 sein.
    pub drift: u32,
    /// Belegte Kontexte, deren **eingetragenes** Fenster den Sperrbereich schneidet.
    pub live_overlaps: u32,
}

/// Die B-3.4-Lage erheben — Politik **und** eingetragener Zustand.
///
/// Zwei Quellen mit Absicht: `policy` rechnet nach, was der Zuteiler vergeben wuerde, `drift`
/// vergleicht das mit dem, was die belegten Kontexte wirklich tragen. Eine Zahl allein waere
/// hier wieder nur eine Hand — genau der Fehler, den `boot_arg` mit der Archivgroesse gemacht hat.
pub fn dma_msi_windows() -> MsiWindows {
    let mut r = MsiWindows {
        policy: MsiClearance::Clear,
        ..Default::default()
    };
    // Politik zuerst, und zwar OHNE den Kontext-Lock: sie braucht keinen Kontext.
    for slot in 0..NDMA_CTX {
        r.policy = r.policy.worse(iova_window_clear_of_msi(slot));
    }
    let window = hal::iommu::interrupt_message_window();
    let t = DMA_CTX.lock();
    for (slot, c) in t.iter().enumerate() {
        if !c.used {
            continue;
        }
        r.live += 1;
        if !c.strong_window {
            r.weak += 1;
        }
        let (base, limit, strong) = slot_window(slot);
        if c.iova_base != base || c.iova_limit != limit || c.strong_window != strong {
            r.drift += 1;
        }
        if let Some((msi, len)) = window {
            if c.iova_base < msi + len && c.iova_limit > msi {
                r.live_overlaps += 1;
            }
        }
    }
    r
}

/// Fenstergröße je Kontext-Slot: der gesamte Raum zwischen RAM-Oberkante und Eingangsgrenze,
/// gleichmäßig auf die `NDMA_CTX` Slots verteilt.
///
/// Vorher war das eine feste 1-GiB-Konstante, und der Bump lief zusätzlich über einen globalen
/// Zähler, der jedem *neu angelegten* Kontext ein frisches Fenster gab. Beides zusammen ergab
/// zwei unnötig knappe Obergrenzen über die Lebenszeit des Kernels: ~256 Attach-Vorgänge je
/// Kontext (jede Region kostet mit Ausrichtung und Schutzband mindestens 4 MiB) und ~500
/// Kontext-**Erzeugungen** insgesamt. Die zweite war die schärfere und stand nirgends.
///
/// Jetzt hängt das Fenster am **Slot**, nicht an einer Erzeugung: `NDMA_CTX` Slots, `NDMA_CTX`
/// Fenster, fertig. Der Bump je Slot überlebt den Abbau eines Kontexts (s. `SLOT_IOVA_NEXT`),
/// damit eine IOVA auch über Kontextgrenzen hinweg nie wiederverwendet wird.
fn window_size(first: u64) -> u64 {
    ((S1_INPUT_LIMIT - first) / NDMA_CTX as u64) & !(IOVA_GUARD - 1)
}

/// Nächste freie IOVA **je Slot**, über Kontext-Lebenszeiten hinweg.
///
/// Der Bump wird beim Abbau eines Kontexts bewusst **nicht** zurückgesetzt. Täte er das, bekäme
/// der nächste Kontext desselben Slots dieselben IOVAs — und ein Gerät, dessen ATC-Eintrag oder
/// in-flight Request die alte Zuordnung trägt, träfe auf eine gültige Abbildung fremder
/// Regionen. Nicht-Wiederverwendung ist der Preis dafür, dass die Reihenfolge beim Teardown eine
/// bewiesene und keine erzwungene Eigenschaft ist (s. Teardown-Token in `todo.md`).
static SLOT_IOVA_NEXT: [AtomicU64; NDMA_CTX] = [const { AtomicU64::new(0) }; NDMA_CTX];

/// Das Fenster des Slots `slot`: `(base, limit, strong)`.
///
/// Die Fenster liegen hintereinander, damit zwei Kontexte nie dieselbe IOVA vergeben — sonst
/// wäre die Bounds-Prüfung eines Treibers gegen den *eigenen* Kontext wertlos, sobald ein zweiter
/// dieselben Zahlen benutzt.
///
/// **Eine Quelle fuer Zuteiler und Pruefer.** Vorher rechnete `iova_window_clear_of_msi` die Lage
/// selbst nach und bildete dabei nur den starken Zweig ab — der Pruefer prueft dann seine eigene
/// Kopie, nicht die vergebene Groesse. Wer hier etwas aendert, aendert beides zugleich; das ist
/// der Punkt.
fn slot_window(slot: usize) -> (u64, u64, bool) {
    match strong_window_base() {
        Some(first) => {
            let size = window_size(first);
            let base = first + slot as u64 * size;
            (base, base + size, true)
        }
        None => {
            // Kein Platz oberhalb des RAM: schwaches Fenster (die Trennung gilt weiterhin, aber
            // eine IOVA *könnte* zufällig wie eine PA aussehen). Auf dieser Plattform gibt es das
            // nicht; der Zweig existiert für 32-Bit-Geräte / kleine Eingangsbreiten.
            //
            // **Auf x86 ist dieses Fenster nicht bloss schwach, sondern verletzt B-3.4:** es
            // enthaelt `0xFEE0_0000..0xFEF0_0000`. Genau deshalb urteilt
            // `iova_window_clear_of_msi` hier `Overlaps` und nicht mehr `true`.
            (0, S1_INPUT_LIMIT, false)
        }
    }
}

/// Dem Kontext im Slot `slot` sein IOVA-Fenster zuweisen — die Lage kommt aus [`slot_window`],
/// damit der Pruefer dieselbe Quelle liest.
fn ctx_assign_window(c: &mut DmaCtx, slot: usize) {
    let (base, limit, strong) = slot_window(slot);
    c.iova_base = base;
    c.iova_limit = limit;
    c.strong_window = strong;
    // Bump des Slots fortsetzen, nicht neu beginnen.
    let prev = SLOT_IOVA_NEXT[slot].load(Ordering::Relaxed);
    c.iova_next = if prev >= c.iova_base && prev < c.iova_limit {
        prev
    } else {
        c.iova_base + IOVA_GUARD // führendes Guard-Band
    };
}

/// Warum eine IOVA-Vergabe scheiterte — die Unterscheidung ist der Punkt: „Fenster voll" ist ein
/// Betriebszustand mit Obergrenze, „Gerät zu schmal" ein Konfigurationsfehler der Plattform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IovaError {
    /// Der Bump des Slots hat das Fensterende erreicht (keine Wiederverwendung).
    WindowExhausted,
    /// Die IOVA läge oberhalb der per `CD.T0SZ` konfigurierten Eingangsbreite.
    InputWidth,
    /// Das Gerät kann die Adresse nicht absetzen (z. B. 32-Bit-DMA, Fenster oberhalb 4 GiB).
    DeviceAddrWidth,
}

/// Eine IOVA für `len` Bytes aus dem Fenster des Kontexts vergeben — `IOVA_GUARD`-ausgerichtet,
/// mit einem Guard-Band dahinter, nie wiederverwendet.
///
/// Drei Bedingungen, alle drei mit eigener Fehlerursache, weil sie verschiedene Dinge bedeuten:
/// das Fensterende (Betriebsgrenze), die Eingangsbreite der Stage-1 (Kernel-Konfiguration) und
/// die Adressbreite des Geräts (Plattform). Vor allem die letzte darf nicht stillschweigend
/// durchgehen: ein Gerät mit 32-Bit-DMA bekäme aus einem Fenster oberhalb des RAM eine Adresse,
/// die es gar nicht absetzen kann — der Bus schneidet sie ab, und die abgeschnittene Adresse
/// trifft irgendetwas anderes.
fn ctx_alloc_iova(c: &mut DmaCtx, slot: usize, len: u64, dev_bits: u32) -> Result<Iova, IovaError> {
    let start = (c.iova_next + IOVA_GUARD - 1) & !(IOVA_GUARD - 1);
    let span = (len + IOVA_GUARD - 1) & !(IOVA_GUARD - 1);
    let next = start
        .checked_add(span)
        .and_then(|e| e.checked_add(IOVA_GUARD)) // Guard dahinter
        .ok_or(IovaError::WindowExhausted)?;
    if next > c.iova_limit {
        return Err(IovaError::WindowExhausted);
    }
    if start + span > S1_INPUT_LIMIT {
        return Err(IovaError::InputWidth);
    }
    if dev_bits < 64 && start + span > (1u64 << dev_bits) {
        return Err(IovaError::DeviceAddrWidth);
    }
    c.iova_next = next;
    SLOT_IOVA_NEXT[slot].store(next, Ordering::Relaxed);
    Ok(Iova::new(start))
}

/// Wie viele Adressbits ein Gerät absetzen kann (`64` = keine Einschränkung bekannt).
const MAX_NARROW_DEVS: usize = 4;
static NARROW_DEVS: [AtomicU64; MAX_NARROW_DEVS] = [const { AtomicU64::new(0) }; MAX_NARROW_DEVS];

/// Einem Gerät eine **schmalere** DMA-Adressbreite zuschreiben (z. B. 32 Bit).
///
/// Ohne diese Angabe nimmt der Kernel 64 Bit an — die einzige Annahme, die nicht still
/// fehlschlägt: ist sie falsch, scheitert `dma_attach` laut, statt eine Adresse zu vergeben, die
/// das Gerät abschneidet. `false`, wenn die Tabelle voll ist.
pub fn dma_declare_device_addr_bits(stream_id: u32, bits: u32) -> bool {
    let v = ((stream_id as u64) << 32) | (bits as u64);
    for e in NARROW_DEVS.iter() {
        let cur = e.load(Ordering::Acquire);
        if cur >> 32 == stream_id as u64 || cur == 0 {
            e.store(v, Ordering::Release);
            return true;
        }
    }
    false
}

/// StreamIDs, für die **keine** Adressbreite deklariert wurde (geführt, nicht bloß angenommen).
static UNDECLARED_DEVS: [AtomicU64; MAX_NARROW_DEVS] =
    [const { AtomicU64::new(u64::MAX) }; MAX_NARROW_DEVS];
static UNDECLARED_COUNT: AtomicU32 = AtomicU32::new(0);

/// Adressbreite eines Geräts. Undeklariert heißt **nicht** stillschweigend „64", sondern
/// „unbekannt, angenommen 64, und das wird geführt".
///
/// Der Unterschied ist der Punkt: eine falsche 64-Bit-Annahme scheitert nicht laut, sondern der
/// Bus schneidet die Adresse ab — und die abgeschnittene Adresse trifft weder ein Schutzband noch
/// eine Prüfung. In einem Kernel, der Bus-Master fail-safe hält und `Unaligned` eine eigene
/// Fehlervariante gibt, darf das nicht die einzige unsichtbare Annahme sein. Jedes undeklarierte
/// Gerät wird deshalb einmal protokolliert und gezählt; `dma_undeclared_devices()` macht den
/// Zustand prüfbar, statt ihn dem Zufall zu überlassen.
///
/// Ein harter Fehlschlag wäre die noch strengere Wahl, ist aber heute nicht zumutbar: die
/// Adressbreite steht in keinem Konfigurationsregister, sie ist Treiberwissen. Die Mittelstellung
/// ist ehrlich — sie behauptet nicht, etwas zu wissen.
fn device_addr_bits(stream_id: u32) -> u32 {
    for e in NARROW_DEVS.iter() {
        let cur = e.load(Ordering::Acquire);
        if cur != 0 && cur >> 32 == stream_id as u64 {
            return cur as u32;
        }
    }
    note_undeclared_device(stream_id);
    64
}

fn note_undeclared_device(stream_id: u32) {
    for e in UNDECLARED_DEVS.iter() {
        let cur = e.load(Ordering::Acquire);
        if cur == stream_id as u64 {
            return; // schon geführt
        }
        if cur == u64::MAX {
            e.store(stream_id as u64, Ordering::Release);
            UNDECLARED_COUNT.fetch_add(1, Ordering::Relaxed);
            // Die serielle Ausgabe haengt am aarch64-HAL; auf x86 bleibt es beim Zaehler.
            #[cfg(target_arch = "aarch64")]
            crate::println!(
                "dma: StreamID 0x{:x} ohne deklarierte Adressbreite -- angenommen 64 Bit \
                 (dma_declare_device_addr_bits)",
                stream_id
            );
            return;
        }
    }
    UNDECLARED_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Zähler der laut abgewiesenen Zuteilungen, je Ursache (Telemetrie/Test).
static IOVA_REJECTS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];

fn note_iova_reject(e: IovaError) {
    let i = match e {
        IovaError::WindowExhausted => 0,
        IovaError::InputWidth => 1,
        IovaError::DeviceAddrWidth => 2,
    };
    IOVA_REJECTS[i].fetch_add(1, Ordering::Relaxed);
}

/// Position einer StreamID in der SID-Liste eines Kontexts.
fn sid_index(c: &DmaCtx, rid: u32) -> Option<usize> {
    c.sids.iter().position(|&s| s == rid)
}

/// **Alle** Geräte des Kontexts stilllegen (nur beim Übergang 0→1 je Gerät tatsächlich).
///
/// Über **alle** StreamIDs, nicht nur die des Teardowns: die Region ist im **Kontext** gemappt,
/// nicht an einer RID. Bei einer Stream-Gruppe (mehrere Geräte teilen eine Stage-1-Tabelle,
/// s. `share_context`) können in-flight Writes von **jedem** Gerät der Gruppe kommen — würde nur
/// das eine entwaffnet, wäre der Zähler korrekt und die Flush-Garantie trotzdem unvollständig:
/// die Region würde entfernt und freigegeben, während ein zweites Gerät desselben Kontexts noch
/// Writes unterwegs hat.
///
/// Reihenfolge: erst **alle** entwaffnen, dann **alle** spülen. So ist kein Gerät der Gruppe mehr
/// scharf, während ein anderes noch spült.
/// Gibt zurück, ob die Stilllegung **bestätigt** ist: für jede StreamID muss das Bus-Master-Bit
/// nach dem Löschen auch wirklich gelöscht sein, und das Konfigurations-Read (der Flush) muss
/// eine plausible Antwort liefern. Ein Gerät, das verschwunden ist (`0xFFFF`) oder sein
/// Command-Register nicht übernimmt, hat die Spülgarantie **nicht** erbracht — dann darf seine
/// Region nie zurück in den Allokator (ext-37, Pending-Zustand).
fn ctx_quiesce(c: &mut DmaCtx) -> bool {
    for k in 0..MAX_CTX_SIDS {
        if c.sids[k] == u32::MAX {
            continue;
        }
        if c.quiesce_depth[k] == 0 {
            c.saved_cmd[k] = hal::pcie::clear_bus_master(c.sids[k]);
        }
        c.quiesce_depth[k] += 1;
    }
    let mut confirmed = true;
    for k in 0..MAX_CTX_SIDS {
        if c.sids[k] == u32::MAX {
            continue;
        }
        let vendor = hal::pcie::flush_posted_writes(c.sids[k]);
        let cmd = hal::pcie::read_command(c.sids[k]);
        if vendor == 0xFFFF || cmd & hal::pcie::CMD_BUS_MASTER_BIT != 0 {
            confirmed = false;
        }
    }
    confirmed
}

/// Stilllegung aufheben (nur beim Übergang 1→0 tatsächlich).
///
/// `allow_bme`: darf das Gerät danach wieder Bus-Master sein? Zurückgeschrieben wird stets der
/// **gesicherte** Wert (ggf. ohne Bus-Master-Bit) — ein Gerät, das absichtlich aus war, darf ein
/// Teardown nicht einschalten.
///
/// **Kopplung an die ATS-Entscheidung** (`docs/invariants.md` §2b): Wiedereinschalten ist nur
/// solide, *weil* nach `CMD_TLBI`+`CMD_SYNC` keine gecachte Übersetzung mehr existiert. Mit
/// **ATS** existiert sie im ATC des Geräts — dann müsste hier eine ATC-Invalidierung davorstehen.
fn ctx_release(c: &mut DmaCtx, allow_bme: bool) {
    // Umgekehrte Reihenfolge zum Entwaffnen (symmetrischer Abbau).
    for k in (0..MAX_CTX_SIDS).rev() {
        if c.sids[k] == u32::MAX || c.quiesce_depth[k] == 0 {
            continue;
        }
        c.quiesce_depth[k] -= 1;
        if c.quiesce_depth[k] > 0 {
            continue; // ein anderer Teardown hält dieses Gerät noch still
        }
        let cmd = if allow_bme {
            c.saved_cmd[k]
        } else {
            c.saved_cmd[k] & !hal::pcie::CMD_BUS_MASTER_BIT
        };
        hal::pcie::write_command(c.sids[k], cmd);
    }
}

/// Bus-Master nach dem Installieren einer Übersetzung erteilen.
///
/// **Kein unbedingtes Schreiben.** Läuft parallel ein Teardown desselben Geräts (Zähler > 0),
/// würde ein direktes Scharfschalten dessen Flush-Garantie entwerten: das Gerät dürfte neue
/// Requests in eine Region absetzen, die der andere Pfad gerade entfernt und gleich freigibt —
/// genau das Fenster, das die Stilllegung schließt, nur über `attach` hereingekommen. Stattdessen
/// wird die **Absicht** im gesicherten Command-Wert vermerkt; das Restore beim Übergang 1→0
/// nimmt sie mit.
fn ctx_arm_bus_master(c: &mut DmaCtx, rid: u32) {
    let Some(k) = sid_index(c, rid) else { return };
    if c.quiesce_depth[k] > 0 {
        c.saved_cmd[k] |= hal::pcie::CMD_BUS_MASTER_BIT;
    } else {
        hal::pcie::arm_bus_master(rid);
    }
}

// --- SMMU-Übersetzungskontexte (ext-24): je Kontext eine STE-Gruppe -> ein CD -> eine
// Stage-1-Tabelle, die MEHRERE Regionen abbildet. Ersetzt die 1:1-Bindungstabelle aus ext-23.
const NDMA_CTX: usize = 4; //      gleichzeitige Kontexte
const MAX_CTX_SIDS: usize = 4; //  StreamIDs je Kontext (Stream-Gruppe)
const MAX_CTX_REGS: usize = 8; //  Regionen je Kontext (Multi-Region / Scatter-Gather)

#[derive(Clone, Copy)]
struct DmaCtx {
    used: bool,
    l1: u64,                          // Stage-1-Wurzel
    cd: u64,                          // Context Descriptor
    sids: [u32; MAX_CTX_SIDS],        // StreamIDs (u32::MAX = leer)
    /// Verschachtelungstiefe der Stilllegung **je StreamID** (parallel zu `sids`).
    ///
    /// Warum hier und nicht in einer Seitentabelle der HAL: die RID gehört zu genau einem
    /// Kontext (`attach` sucht ihn über `sids.contains`), also ist der Zähler durch **denselben**
    /// Lock geschützt wie die RID selbst — er kann nicht aus dem Tritt geraten, wenn das
    /// Kontext-Locking später verfeinert wird, und er kann nicht überlaufen, weil seine
    /// Kapazität dieselbe Quelle hat wie die Kontextobergrenze (`NDMA_CTX * MAX_CTX_SIDS`).
    quiesce_depth: [u32; MAX_CTX_SIDS],
    /// Command-Register **vor** der ersten Stilllegung (Save/Restore, nie „auf 1 setzen").
    saved_cmd: [u16; MAX_CTX_SIDS],
    /// Basis des **IOVA-Fensters** dieses Kontexts (0 = noch nicht vergeben).
    ///
    /// Bewusst ein **Kontext-Attribut**, keine globale Konstante: die starke Eigenschaft
    /// („keine IOVA kann je eine gültige PA sein") verlangt eine Basis oberhalb des RAM, und die
    /// kann ein Gerät, das nur unterhalb 4 GiB adressiert, nicht immer haben. Welche Kontexte die
    /// starke Eigenschaft tragen, steht in `strong_window`.
    iova_base: u64,
    /// Nächste freie IOVA (Bump). **Wird nie zurückgesetzt**: Wiederverwendung ist bei einem
    /// 39-Bit-Fenster unnötig und würde Fragen nach veralteten Übersetzungen aufwerfen, die es
    /// so gar nicht erst gibt.
    iova_next: u64,
    /// Obergrenze des Fensters (exklusiv). Der Bump gibt nie zurück, also ist das zugleich die
    /// Lebenszeit-Obergrenze der Zuteilungen dieses Slots.
    iova_limit: u64,
    /// Liegt das Fenster oberhalb des RAM (dann ist eine vertauschte Achse **immer** ein Fault)?
    strong_window: bool,
    regs: [DmaRegion; MAX_CTX_REGS], // (base,len) je Region ((0,0) = leer)
}

impl DmaCtx {
    const EMPTY: DmaCtx = DmaCtx {
        used: false,
        l1: 0,
        cd: 0,
        sids: [u32::MAX; MAX_CTX_SIDS],
        quiesce_depth: [0; MAX_CTX_SIDS],
        saved_cmd: [0; MAX_CTX_SIDS],
        iova_base: 0,
        iova_next: 0,
        iova_limit: 0,
        strong_window: false,
        regs: [DmaRegion::EMPTY; MAX_CTX_REGS],
    };
}

static DMA_CTX: SpinLock<[DmaCtx; NDMA_CTX]> = SpinLock::new([DmaCtx::EMPTY; NDMA_CTX]);

/// Der globale DMA-Enforcer: auf aarch64 der SMMUv3-Treiber, sonst der Null-Enforcer. Ein
/// Wechsel tauscht nur diese Definition + den Accessor aus, ohne den öffentlichen DMA-Pfad zu
/// ändern (alle Aufrufer gehen über [`dma_enforcer`]).
#[cfg(target_arch = "aarch64")]
static DMA_ENFORCER: SmmuV3Enforcer = SmmuV3Enforcer::new();
#[cfg(not(target_arch = "aarch64"))]
static DMA_ENFORCER: VtdEnforcer = VtdEnforcer::new();

/// Zugriff auf den aktiven DMA-Enforcer (als Trait-Objekt — der öffentliche Pfad ist
/// enforcer-polymorph und SMMU-agnostisch).
pub fn dma_enforcer() -> &'static dyn DmaEnforcer {
    &DMA_ENFORCER
}

/// Den DMA-Enforcer initialisieren (Bring-up; idempotent). Bei SMMUv3: Queues/Stream-Tabelle
/// anlegen, Default-Abort, CR0 aktivieren. Gibt `true` bei Erfolg / aktiver Durchsetzung.
pub fn dma_enforcer_init() -> bool {
    dma_enforcer().init()
}

// SMMU-Diagnose-Accessors (smmu_present/idr0/sid_bits/enabled/sync_ok/eventq_empty/gerror) liegen
// in `mod testsupport` (Konsolidierung K5: SMMU-spezifische Test-/Bericht-Telemetrie getrennt).

/// Eine kontiguierliche **DMA-RAM-Region** (4-KiB-granular, in der mappbaren GiB-1-Region)
/// ausschneiden. `None` bei Erschöpfung oder wenn die Allokation nicht in GiB 1 liegt (dort
/// arbeitet [`hal::mmu::vspace_map_dma`]).
///
/// **Konsolidierung K3:** carvt über die **kanonische** [`KernelRegionSource`] (eine einzige
/// MEM-Carve-Stelle für besitzte Regionen), als `Purpose::Dma` getaggt. Anders als ein Heap-
/// `Region` (das seinen `MemoryCap` besitzt und über die `RegionSource` freigegeben wird) ist das
/// **Besitzmodell** einer DMA-Region bewusst (phys,len)-basiert: die Lebensdauer hängt an der
/// **DmaCap** — die Freigabe erfolgt beim Löschen der Cap (`delete_leaf` -> `free_region`) bzw.
/// über [`free_dma_region`] (roher Pfad), genau einmal. Daher wird der `Region`-Wrapper hier zu
/// einem reinen `MemoryCap`-Deskriptor aufgelöst (`into_cap`, **kein** Drop-Free) und nur die
/// `PhysRegion` weitergereicht. Siehe `docs/invariants.md` §2/§3.
pub fn alloc_dma_region(len: u64) -> Option<PhysRegion> {
    // E-Rest 3b: der Zonenwunsch steht in der Anforderung. Vorher fragte diese Stelle nach
    // "irgendeiner" Region, prüfte die Grenze danach und gab bei Verfehlung auf -- bei `-m 3G`
    // wählte Best-Fit dann den kleineren oberen Bereich und `dmawin`/`dmatok` fielen durch,
    // obwohl unten reichlich Platz war.
    let (floor, ceil) = gib0_zone();
    let region = KernelRegionSource.request_below(len as usize, Purpose::Dma, ceil)?;
    let pr = PhysRegion::new(region.phys(), region.len() as u64);
    if pr.base < floor || pr.base + pr.len > ceil {
        KernelRegionSource.release(region); // nicht in GiB 1 -> zurückgeben (sonst nicht mappbar)
        return None;
    }
    // Eigentum geht an die DmaCap über (Freigabe nach (phys,len), s.o.). Den linearen
    // `MemoryCap` als reinen Deskriptor auflösen — KEIN Drop-Free.
    let _ = region.into_cap();
    Some(pr)
}

/// Eine **DMA-Capability** (ext-23, HardwareLand) über die kernel-ausgeschnittene Region
/// `[phys, phys+len)` prägen — **nur kernelseitig** (kein User-Syscall erzeugt DMA-Caps).
/// Installiert wird sie cap-policy-geprüft nur in HardwareLand-PDs (`install_cap_checked`).
pub fn install_dma_cap(phys: u64, len: u64, rights: Rights) -> Result<CapPtr, CapError> {
    dma_granule_ok(phys, len)?;
    CAPS.write().cspace.install_dma(phys, len, rights)
}

/// **Granularitätsbedingung eines DMA-Puffers** (ext-35).
///
/// Cache-Wartung arbeitet auf ganzen Zeilen: `dc civac` (nach einem Geräte-Write, bzw. bei
/// bidirektionalen Puffern) **verwirft** eine komplette Zeile. Liegt in einer angebrochenen
/// Randzeile fremder Speicher, verliert der seine noch nicht zurückgeschriebenen Daten — ein
/// Datenverlust außerhalb des Puffers, den weder Bounds-Prüfung noch IOMMU sehen (beide
/// betrachten den Puffer, nicht seine Nachbarschaft).
///
/// Die Bedingung wird **einheitlich** geprüft, nicht nur für die Richtungen, bei denen
/// invalidiert wird. Richtungsabhängig wäre ehrlicher gegenüber dem tatsächlich Gefährlichen,
/// aber eine Cap ist langlebig und ihre Richtung ein Feld: eine später erlaubte
/// `Bidirectional`-Nutzung dürfte sonst auf einer Ausrichtung sitzen, die nie dafür geprüft
/// wurde. Einheitlich strikt ist die Bedingung, die man in einem Jahr noch begründen kann.
///
/// Die Granularität liefert die Architektur ([`hal::mmu::dma_granule`]): `CTR_EL0.CWG` auf ARM,
/// `1` auf x86 (hardware-kohärent, keine Wartung) — dort ist die Prüfung damit trivial erfüllt.
fn dma_granule_ok(phys: u64, len: u64) -> Result<(), CapError> {
    let g = hal::mmu::dma_granule();
    if g <= 1 {
        return Ok(()); // kohärente Architektur: keine Wartung, keine Bedingung
    }
    if phys % g == 0 && len % g == 0 {
        Ok(())
    } else {
        Err(CapError::Unaligned)
    }
}

/// Wie [`install_dma_cap`], aber mit expliziter **DMA-Richtung** + **Cache-Kohärenz** (ext-24).
/// Die Cap kodiert damit die volle Autorität; der Enforcer mappt richtungsminimal (Read-Puffer
/// schreibgeschützt) und kohärenz-spezifisch (cacheable vs. non-cacheable).
pub fn install_dma_cap_ex(
    phys: u64,
    len: u64,
    dir: DmaDir,
    coherence: DmaCoherence,
    rights: Rights,
) -> Result<CapPtr, CapError> {
    dma_granule_ok(phys, len)?;
    CAPS.write().cspace.install_dma_ex(phys, len, dir, coherence, rights)
}

/// Die DMA-Attribute (Richtung, Kohärenz) einer DMA-Cap nachschlagen (für den Enforcer/Treiber:
/// richtungs-/kohärenz-spezifisches Mapping). `None`, wenn die Cap keine DMA-Cap ist.
pub fn dma_cap_attrs(cap: CapPtr) -> Option<(u64, u64, DmaDir, DmaCoherence)> {
    match CAPS.read().cspace.lookup(cap)?.0 {
        ObjectKind::Dma {
            phys,
            len,
            dir,
            coherence,
        } => Some((phys, len, dir, coherence)),
        _ => None,
    }
}

/// **DMA-Transfer vorbereiten** (ext-24, Cache-Maintenance, hardware-/geräteunabhängig): vor dem
/// Start eines Transfers in Richtung `dir` die nötige Cache-Wartung auf der (identity-gemappten)
/// Region ausführen. Bei `DeviceRead`/`Bidirectional`: Clean (CPU-Daten sichtbar machen). Bei
/// `DeviceWrite`: nichts (das Gerät schreibt; die CPU invalidiert in `dma_complete`). No-Op-sicher
/// für Non-Coherent-Puffer.
#[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
pub fn dma_prepare(handle: DmaHandle, dir: DmaDir) {
    match dir {
        DmaDir::DeviceRead | DmaDir::Bidirectional => {
            // Cache-Wartung läuft über die **CPU**-Sicht: die IOVA existiert nur für das Gerät.
            hal::mmu::dma_cache_clean(handle.pa.raw(), handle.len)
        }
        DmaDir::DeviceWrite => {}
    }
}

/// **DMA-Transfer abschließen** (ext-24): nach einem Geräte-Write (`DeviceWrite`/`Bidirectional`)
/// die CPU-Cache-Zeilen invalidieren, damit die CPU die vom Gerät geschriebenen Daten frisch liest.
#[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
pub fn dma_complete(handle: DmaHandle, dir: DmaDir) {
    match dir {
        DmaDir::DeviceWrite | DmaDir::Bidirectional => {
            hal::mmu::dma_cache_invalidate(handle.pa.raw(), handle.len)
        }
        DmaDir::DeviceRead => {}
    }
}

/// Eine zuvor gemappte DMA-Region wieder aus der VSpace des Threads entmappen (zurück auf
/// EL1-only) + ASID flushen. Teil der Revoke-Reihenfolge.
#[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
pub fn unmap_dma_from_thread(tid: ThreadId, phys: u64, len: u64) -> bool {
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    let Some(l2) = vspace_l2(asid) else {
        return false;
    };
    // Gegenstueck zum DMA-Zweig von `map_region_into_thread` -- **derselbe Grund, damit es
    // dieselbe Achse ist**. Der Waechter hat diese Stelle gefunden: sie stand ausserhalb jeder
    // Engstelle und bildete identisch ab, ohne dass irgendwo stand warum. Raeumte sie auf einer
    // anderen Achse ab als das Mappen, bliebe eine Abbildung stehen -- und ein DMA-Fenster, das
    // beim Abbau stehenbleibt, ist genau das, was `dma_audit` Code 8 meldet.
    let base = crate::addr::Va::for_dma_window(DmaWindowWitness(()), crate::addr::Pa::new(phys)).raw();
    let mut p = base;
    let mut ok = true;
    while p < base + len {
        ok &= hal::mmu::vspace_unmap_page(l2, p);
        p += 4096;
    }
    hal::mmu::flush_asid(asid);
    ok
}

/// Die hardwareseitige DMA-Durchsetzung für `(stream_id, [base,len))` aktivieren (ext-23-API,
/// rückwärtskompatibel: bidirektional + non-cacheable). `true` bei Erfolg.
pub fn dma_enable(stream_id: u32, base: u64, len: u64) -> Option<Iova> {
    // Roher Pfad (ohne Cap): der Aufrufer nennt eine **physische** Adresse und bekommt die
    // **Gerätesicht** zurück. Er kann sie nicht selbst ausrechnen — sie stammt aus dem
    // IOVA-Fenster des Kontexts (ext-36 Schritt b) und hat mit `base` keinen Zusammenhang.
    dma_enforcer().attach(&DmaBinding::new(stream_id, Pa::new(base), len))
}

/// Die Durchsetzung für `(stream_id, [base,len))` wieder entziehen.
pub fn dma_disable(stream_id: u32, base: u64, len: u64) {
    dma_enforcer().detach(&DmaBinding::new(stream_id, Pa::new(base), len));
}

/// **Nachweis, dass eine DMA-Region abgebaut ist** (ext-37).
///
/// Der Token ist der einzige Weg zu [`free_dma_region`]. Er entsteht ausschließlich in
/// [`dma_finalize`], und zwar erst, nachdem für seine Region gilt: Gerät stillgelegt (bestätigt),
/// Übersetzung entfernt, `CMD_SYNC` durch. Vorher war diese Reihenfolge eine **bewiesene**
/// Vorbedingung des Gesamtsystems — `CapSpace::delete_leaf` rief `free_region` direkt, und dass
/// davor jemand aufgeräumt hatte, stand nirgends im Typ. Jetzt ist sie **erzwungen**.
///
/// Er trägt eine [`DmaRegion`], nicht eine `PhysRegion`: nach ext-36 gehören beide Achsen in den
/// Nachweis. Der Unmap ist auf der IOVA-Achse, der Free auf der PA-Achse — mit einer reinen
/// `PhysRegion` könnte der Token einen erfolgten Free bezeugen, ohne dass der Unmap darin
/// überhaupt vorkommt.
#[must_use = "ein Teardown-Token, der nicht eingelöst wird, ist eine geleakte Region"]
pub struct DmaTeardownToken {
    region: DmaRegion,
}

impl DmaTeardownToken {
    /// Die bezeugte Region — beide Achsen, für Diagnose und Audits.
    pub fn region(&self) -> DmaRegion {
        self.region
    }
}

/// Eine DMA-Region freigeben. **Nur** gegen einen [`DmaTeardownToken`].
fn free_dma_region(token: DmaTeardownToken) {
    free_raw_region(token.region.pa.raw(), token.region.len);
}

/// Regionen, deren Gerät die Stilllegung **nicht bestätigt** hat.
///
/// Ihre Übersetzung ist entfernt (das ist immer zwingend), aber ihre Physadresse geht **nie**
/// zurück in den Allokator: ohne bestätigte Spülung ist nicht auszuschließen, dass noch ein
/// übersetzter, unterwegs befindlicher Write ankommt. Ein Leck ist gegenüber einem
/// Use-after-free der richtige Failure-Mode — aber als **Entscheidung**, nicht als Versehen:
/// die Menge ist beschränkt, gezählt und auditiert (`dma_audit` Code 7).
const MAX_PENDING_DMA: usize = 16;
static PENDING_DMA: SpinLock<[DmaRegion; MAX_PENDING_DMA]> =
    SpinLock::new([DmaRegion::EMPTY; MAX_PENDING_DMA]);
/// Wie viele Regionen insgesamt in den Pending-Zustand gerieten (auch die, für die kein Platz
/// mehr war — die sind dann erst recht geleakt, aber nicht unbemerkt).
static PENDING_TOTAL: AtomicU32 = AtomicU32::new(0);
/// Davon außerhalb eines `KILL` — dort ist Pending erwartbar (eine sterbende PD mit hängendem
/// Gerät), überall sonst ist es eine Anomalie, die man sehen will.
static PENDING_UNEXPECTED: AtomicU32 = AtomicU32::new(0);

/// Läuft gerade ein `KILL`? Dort darf Pending entstehen, ohne als Anomalie zu zählen.
static IN_KILL: AtomicU32 = AtomicU32::new(0);

/// Markiert den laufenden Abbau einer PD/eines Threads (`KILL`).
pub struct KillScope;
impl KillScope {
    pub fn enter() -> Self {
        IN_KILL.fetch_add(1, Ordering::AcqRel);
        KillScope
    }
}
impl Drop for KillScope {
    fn drop(&mut self) {
        IN_KILL.fetch_sub(1, Ordering::AcqRel);
    }
}

fn park_pending(region: DmaRegion) {
    PENDING_TOTAL.fetch_add(1, Ordering::Relaxed);
    if IN_KILL.load(Ordering::Acquire) == 0 {
        PENDING_UNEXPECTED.fetch_add(1, Ordering::Relaxed);
    }
    let mut p = PENDING_DMA.lock();
    if let Some(e) = p.iter_mut().find(|r| r.is_empty()) {
        *e = region;
    }
    #[cfg(all(target_arch = "aarch64", feature = "debug-log"))]
    crate::println!(
        "dma: Region pa=0x{:x} len=0x{:x} bleibt dauerhaft pending -- Stilllegung nicht bestaetigt",
        region.pa.raw(),
        region.len
    );
}

/// **Finalisierung der beim Löschen/Revoke gemeldeten DMA-Regionen** (ext-37).
///
/// Läuft **ohne** gehaltenen CAPS/MEM-Lock — dieselbe Stelle und derselbe Grund wie bei
/// [`abort_finalized_replies`]: die Nachbereitung braucht Sperren, die unter CAPS nicht genommen
/// werden dürfen (hier `DMA_CTX` und, im Kontextabbau, `MEM`).
///
/// **Synchron, nicht aufgeschoben.** Der Preis ist eine geräteabhängige Laufzeit im `delete`;
/// der Gewinn ist, dass es den Pending-Zustand für den Normalfall **gar nicht gibt**. Ein
/// Zwischenzustand, den es meistens nicht gibt, ist schwerer richtig zu halten als einer, den es
/// nie gibt — und die Beschränktheits-Invariante der Pending-Menge muss dann nur noch den
/// pathologischen Fall tragen, für den sie gedacht war.
/// `ok` ist der Ergebnispuffer des Enforcers und kommt vom Aufrufer (A-3.3, s. [`FinalizeBuf`]).
/// Vorher lagen hier **zwei** weitere Arrays auf dem Kernelstack: eine Kopie der Regionen (nur um
/// aus dem Kollektor einen zusammenhängenden Slice zu machen — den liefert er jetzt selbst) und
/// das Ergebnisfeld.
fn dma_finalize(fin: &caprock_cap::Finalized<'_>, ok: &mut [bool], ctx_of: &mut [usize]) {
    let regs = fin.dma_regions();
    let n = regs.len().min(ok.len());
    if n == 0 {
        return;
    }
    dma_enforcer().finalize(&regs[..n], &mut ok[..n], ctx_of);
    for i in 0..n {
        let region = DmaRegion {
            iova: Iova::new(0), // die IOVA ist mit dem Unmap erloschen und kommt nie zurück
            pa: Pa::new(regs[i].0),
            len: regs[i].1,
        };
        if ok[i] {
            free_dma_region(DmaTeardownToken { region });
        } else {
            park_pending(region);
        }
    }
}

/// **Opakes DMA-Handle** (ext-24): die *gerätesichtbare* Adresse (IOVA) + Länge einer
/// angehängten Region. Backends programmieren das Gerät mit `iova` (heute identisch zur PA;
/// die Abstraktion erlaubt später Remap/Bounce/SG-Kompaktierung ohne API-Bruch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaHandle {
    /// Gerätesicht — steht im Deskriptor, wird gegen den Kontextinhalt geprüft.
    pub iova: Iova,
    pub len: u64,
    /// CPU-Sicht — **kernelprivat**: Cache-Wartung und Allokator-Buchhaltung laufen darüber.
    /// Ein Treiber hat hier nichts zu suchen; gäbe man sie heraus, wäre die Trennung der beiden
    /// Achsen wieder eine Konvention statt einer Grenze.
    pa: Pa,
}

/// Eine **DMA-Cap** (ext-24) an den Übersetzungskontext der `stream_id` **anhängen**: liest
/// Richtung + Kohärenz aus der Cap (cap-rein), mappt die Region richtungsminimal/kohärenz-
/// spezifisch in die (ggf. neue) Kontext-Stage-1-Tabelle und gibt ein [`DmaHandle`] zurück.
/// Mehrfaches `dma_attach` mit derselben `stream_id` (verschiedene Caps) hängt mehrere
/// Regionen an **denselben** Kontext (Multi-Region). `None` bei Fehler / falscher Cap.
pub fn dma_attach(stream_id: u32, dcap: CapPtr) -> Option<DmaHandle> {
    let (phys, len, dir, coherence) = dma_cap_attrs(dcap)?;
    let ro = dir == DmaDir::DeviceRead;
    let cacheable = coherence == DmaCoherence::Coherent;
    let assigned = dma_enforcer().attach(&DmaBinding {
        stream_id,
        pa: Pa::new(phys),
        len,
        backend_pd: 0,
        ro,
        cacheable,
    });
    if let Some(iova) = assigned {
        // Die IOVA kommt aus dem Fenster des Übersetzungskontexts (Schritt b) — der Aufrufer
        // konnte sie nicht wählen und erfährt sie erst hier.
        Some(DmaHandle {
            iova,
            len,
            pa: Pa::new(phys),
        })
    } else {
        None
    }
}

/// Eine zuvor per [`dma_attach`] angehängte Region wieder lösen (Stage-1-Eintrag entfernen +
/// TLBI; bei letzter Region des Kontexts: Kontext abbauen). `handle.iova` == PA.
pub fn dma_detach(stream_id: u32, handle: DmaHandle) {
    dma_enforcer().detach(&DmaBinding::new(
        stream_id,
        handle.pa,
        handle.len,
    ));
}

/// **IOMMU-Stream-Gruppe** (ext-24): `member` nutzt fortan denselben Übersetzungskontext wie
/// `leader` (Multi-Function/SR-IOV/Bridge). `true` bei Erfolg.
pub fn dma_group_add(leader: u32, member: u32) -> bool {
    dma_enforcer().share_context(leader, member)
}

// ================================================================================================
// A-5.1: ein Gerät an eine Treiber-PD zuteilen
// ================================================================================================

/// Ein Gerät, das der Hochlauf gefunden hat und das an eine Treiber-PD gehen **darf**.
///
/// Bewusst nur die **aufgelösten Tatsachen** und kein `PciDevice`: was der Kern hier weitergibt,
/// ist „diese Konfigurationsraum-Seite, dieses Registerfenster, diese Requester-ID". Was er
/// **nicht** weitergibt, ist der Weg, sie zu finden — ein Lauf über alle Busse sieht jedes Gerät
/// der Maschine, und genau das ist die Autorität, die im Kern bleibt.
///
/// Der Kern weiß hier auch **nicht**, was für ein Gerät das ist. Kein virtio, kein Blockgerät,
/// keine Vendor-ID in der Entscheidung. Das ist die halbe Richtungsumkehr aus A-5.1: der Kern
/// teilt Autorität zu, der Treiber weiß, was er damit anfängt.
#[derive(Clone, Copy)]
pub struct DriverDevice {
    /// Requester-ID (x86) bzw. StreamID (aarch64) — die Identität, unter der das Gerät DMA anfordert.
    pub rid: u32,
    /// Die Konfigurationsraum-**Seite** genau dieser Funktion (4 KiB).
    pub cfg_page: u64,
    /// Registerfenster (BAR) — Basis und Länge.
    pub bar: u64,
    pub bar_len: u64,
    /// PCI-Hersteller-ID — **nur zur Auswahl**, nicht zum Bedienen (A-5.3).
    pub vendor: u16,
    /// PCI-Geräte-ID — dito.
    pub device: u16,
    /// **MSI-X (Stufe B):** Offset der Capability im Konfigurationsraum; `0` = das Geraet hat
    /// keine, und dann gibt es fuer diese Zuteilung keinen Interrupt.
    pub msix_cap: u16,
    /// Identity-Adresse der MSI-X-Tabelle (`0` = keine).
    ///
    /// **Sie liegt garantiert NICHT in der BAR, die der Treiber bekommt** -- ein Geraet, bei dem
    /// sie das taete, wird gar nicht erst angeboten (E11). Sonst koennte der Treiber Adresse und
    /// Datenwort selbst schreiben und damit waehlen, wo sein Interrupt landet.
    pub msix_table: u64,
    /// Zeilen der Tabelle (`Table Size` + 1).
    pub msix_eintraege: u16,
    /// `class<<16 | subclass<<8 | prog_if` — dito.
    pub class: u32,
}

/// Höchstzahl gleichzeitig **angebotener** Geräte.
pub const MAX_OFFERED_DEVICES: usize = 8;

/// Die zuteilbaren Geräte. `None` in einem Platz = frei; ein vergebenes Gerät wird
/// **herausgenommen**, damit es kein zweites Mal vergeben werden kann.
///
/// **Vorher stand hier genau eines** — mit der Begründung, dass jede Auswahl unter mehreren eine im
/// Kernel versteckte Politik wäre, solange das Manifest nicht sagen kann, *welches*. Die Begründung
/// war richtig, und A-5.3 nimmt ihr die Grundlage: der Manifest-Eintrag trägt jetzt einen
/// [`man::DeviceSelector`], also steht die Politik im Autoritätsdokument und nicht in der
/// Fundreihenfolge des Enumerators.
///
/// Die Reihenfolge in dieser Liste ist damit **keine Zusage**. Wer sich auf sie verlässt, hat einen
/// Fehler, der bei der nächsten Maschine auffällt.
static DRIVER_DEVICES: SpinLock<[Option<DriverDevice>; MAX_OFFERED_DEVICES]> =
    SpinLock::new([None; MAX_OFFERED_DEVICES]);

/// **Die VORGABE, wenn der Ladeaufruf keine Groesse nennt** (C2, 2026-08-26). Reicht fuer Ringe +
/// Puffer eines einfachen Geraets.
///
/// Hier stand „die Groesse gehoert perspektivisch ins Manifest (Z11c)". Das ist **verworfen**: das
/// Manifest beschreibt den Bootzustand und wird signiert; eine Treiberumgebung, die einen groesseren
/// Pool braucht, wird zur Laufzeit geladen. Die Groesse ist deshalb **Argument der Zuteilung**
/// (`SYS_LOAD` `x5`, s. `caprock_abi::load_extras`) und die Konstante nur noch die Vorgabe.
const DRIVER_DMA_BYTES: u64 = 16 * 1024;

/// Obergrenze je Zuteilung -- **dieselbe Zahl wie in der ABI**, nicht eine zweite daneben.
///
/// Ein Kernel, der oberhalb einer anderen Grenze abweist als die, die die Schnittstelle
/// veroeffentlicht, macht die Absage fuer den Aufrufer unvorhersagbar. Der `const assert` haelt
/// beide zusammen; wer eine aendert, bricht den Bau, bis er die andere mitnimmt.
const DRIVER_DMA_MAX_BYTES: u64 = caprock_abi::DRIVER_DMA_MAX_PAGES * 4096;
const _: () = assert!(
    DRIVER_DMA_BYTES <= DRIVER_DMA_MAX_BYTES,
    "die Vorgabe liegt ueber der Obergrenze"
);

/// Höchstzahl gleichzeitiger Gerätezuteilungen (heute: eine, s. [`DRIVER_DEVICE`]).
const MAX_DRIVER_ASSIGN: usize = 4;

/// **Vektoren je Gerätezuteilung** (Stufe B, Mehrvektor-Entscheidung: 4).
///
/// Ein MSI-X-Gerät mit `n` Vektoren braucht `n` `Irq`-Caps und `n` Notifications — je Vektor ein
/// Paar, weil `SYS_BIND_IRQ` genau ein Paar bindet. Vier deckt die Geräte dieser Stufe (virtio:
/// Konfiguration + Warteschlangen); mehr ist kein neuer Mechanismus, sondern eine grössere Zahl
/// hier und in den Tabellen, die daraus abgeleitet sind ([`MSI_VEKTOR_POOL`], [`NIRQ_BIND`]).
/// Die Schranke ist *diese*, nicht `caprock_microkit::VEKTOREN_JE_GERAET_MAX` (64): jene begrenzt
/// die Bindungsseite je Gerät, diese die Vergabe — gewährt wird das Minimum beider.
pub const VEKTOREN_JE_ZUTEILUNG: usize = 4;

// --- Stufe B: CPU-Vektoren fuer Geraete-MSI --------------------------------------------------
//
// **Ein Block je Zuteilung, und die Zahl ist hergeleitet, nicht gewaehlt:** Stufe B gibt jedem
// Geraet [`VEKTOREN_JE_ZUTEILUNG`] Vektoren (MSI-X mit genau dieser Breite), also braucht es so
// viele wie es Zuteilungen mal Vektoren gibt.
//
// `0x50` ist frei: 0..31 sind CPU-Ausnahmen, 32 der Timer, 33 der Resched-IPI, 0x70 der
// IRTE-Selbsttest, 0x80 der Syscall. `0x60` ginge nicht mehr: der Block ist 16 Vektoren breit
// (`0x60 + 16 = 0x70` laege auf der Zustellprobe, und `0x60 + 16 + 1` auf dem Selbsttest). Der
// `const assert` haelt die Wahl gegen die belegten Zahlen, damit eine Verschiebung den Bau bricht
// statt still zu kollidieren.
//
// **x86 only, by content and not merely by use:** every number in that list is an entry of the
// x86 IDT. aarch64 delivers through GICv3/ITS and allocates no CPU vector at all (plan §5).
#[cfg(target_arch = "x86_64")]
const MSI_VEKTOR_BASIS: u8 = 0x50;
/// **Der Pool: ein Bit je vergebbaren Vektor** — `MAX_DRIVER_ASSIGN` Zuteilungen mal
/// [`VEKTOREN_JE_ZUTEILUNG`] Vektoren.
#[cfg(target_arch = "x86_64")]
const MSI_VEKTOR_POOL: usize = MAX_DRIVER_ASSIGN * VEKTOREN_JE_ZUTEILUNG;
/// **Der Vektor der ZUSTELLPROBE** — direkt hinter dem Block der Zuteilungen.
///
/// Er gehoert in denselben `const assert` wie die uebrigen: eine Probe, die sich mit einer echten
/// Zuteilung ueberschneidet, misst deren Interrupt und meldet Erfolg.
#[cfg(target_arch = "x86_64")]
const MSI_TEST_VEKTOR: u8 = MSI_VEKTOR_BASIS + MSI_VEKTOR_POOL as u8;
#[cfg(target_arch = "x86_64")]
const _: () = assert!(
    MSI_VEKTOR_BASIS as usize > 33
        && (MSI_VEKTOR_BASIS as usize) + MSI_VEKTOR_POOL + 1 <= 0x70,
    "MSI-Vektorblock (samt Zustellprobe) kollidiert mit Timer/IPI/IRTE-Selbsttest/Syscall"
);

/// Badge der Zustellprobe — eigenes Etikett, damit „es kam etwas an" von „es kam MEINES an"
/// unterscheidbar bleibt.
#[cfg(target_arch = "x86_64")]
const MSI_PROBE_BADGE: u64 = 0x4d53_4950; // "MSIP"

/// **Was die Zustellprobe ergeben hat** (Stufe B, der Schnitt zwischen Erzeugung und Zustellung).
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Default, Debug)]
pub struct MsiZustellBefund {
    /// Konnte die Probe ueberhaupt aufgebaut werden? (Remapping scharf, IRTE frei, Bindung frei)
    pub sprechfaehig: bool,
    /// Die Adresse, die geschrieben wurde — die **Erzeugerseite**, so wie ein Geraet sie schreibt.
    pub addr: u32,
    pub data: u32,
    /// Ist die **remappable** Nachricht angekommen? — **unter QEMU nicht aussagekraeftig**, s.
    /// [`msi_zustellprobe`].
    pub zugestellt: bool,
    /// Kam sie mit **diesem** Badge an? Ohne das waere ein fremder Interrupt ein Beleg.
    pub badge_stimmt: bool,
    /// Ist die **Kompatibilitaets**-Nachricht angekommen? Das ist die Haelfte, die traegt:
    /// APIC → IDT → `irq_hook` → Drain → Notification, ohne Interrupt-Remapping.
    pub kompat_zugestellt: bool,
    /// **Ist der remappable Interrupt ueberhaupt am Prozessor ANGEKOMMEN?** (`irq_hook` gerufen)
    pub remap_angekommen: bool,
    /// dito fuer die Kompatibilitaetsform.
    pub kompat_angekommen: bool,
    /// Der zuletzt in `irq_hook` gesehene Vektor (`u32::MAX` = noch keiner).
    pub letzter_vektor: u32,
    /// Hat die IOMMU einen Fault aufgezeichnet? Eine abgewiesene Geraete-MSI **faultet**; bleibt
    /// die Liste leer und kommt trotzdem nichts an, hat das Geraet nicht gesendet.
    pub faults_leer: bool,
}

/// **Die Halbierung: eine MSI vom KERN aus erzeugen** — ein gewoehnlicher Store, kein Geraet.
///
/// Der ganze Zustellpfad (IRTE → Interrupt-Remapping → APIC → IDT → `irq_hook` → Drain →
/// Notification) war bis heute **nie gefahren**: der IRTE-Selbsttest auf `0x70` liest ausschliesslich
/// den Tabelleninhalt zurueck und stellt nichts zu. Damit war jede gruene Zeile auf der
/// Zustellseite eine Aussage ueber Zustand, nicht ueber Wirkung — dieselbe Unterscheidung wie
/// `rx_used` gegen „Daten sind angekommen", nur eine Ebene tiefer.
///
/// **Was der Ausgang entscheidet:**
///
/// * **kommt an** → der Pfad traegt, und ein ausbleibender Geraeteinterrupt liegt vollstaendig auf
///   der **Erzeugerseite** (Geraet/virtio). `EIME`, `IRTA`, Vektorwahl, IDT sind damit erledigt.
/// * **kommt nicht an** → die Geraeteseite ist irrelevant, es ist der **Remapping-Pfad**.
///
/// **Der Haken, und er ist gemessen worden, nicht erschlossen:** die *remappable* Haelfte dieser
/// Probe ist **unter QEMU nicht aussagekraeftig**. Interrupt-Remapping haengt dort als
/// Speicherregion im **Geraete**-Adressraum; ein CPU-Store nach `0xFEE0_0000` geht daran vorbei
/// und wird vom lokalen APIC direkt dekodiert — also in **Kompatibilitaetsform**. `0xfee00048`
/// heisst dann „Ziel 0", und das Datenwort `0` heisst **Vektor 0**: nichts kann ankommen, ganz
/// gleich wie heil der Remapping-Pfad ist. Ein `zugestellt=false` belegt hier also **nichts** —
/// und ein `true` waere ein Beleg fuer den falschen Pfad gewesen.
///
/// Deshalb steht daneben die **Kompatibilitaetsprobe**, und die traegt: sie misst
/// APIC → IDT → `irq_hook` → Drain → Notification. Kommt sie an, ist die zweite Haelfte des
/// Zustellpfads bewiesen und die offene Frage schrumpft auf die Uebersetzung selbst. Kommt sie
/// nicht an, liegt der Fehler vor der IOMMU und die IRTE-Frage ist verfrueht.
///
/// **`rid = 0`** bleibt richtig fuer den Fall, dass die remappable Haelfte je aussagekraeftig wird:
/// `SVT/SID` prueft die Quelle, und der Prozessor ist kein PCI-Geraet.
#[cfg(target_arch = "x86_64")]
pub fn msi_zustellprobe() -> MsiZustellBefund {
    let mut b = MsiZustellBefund::default();
    let Some(nid) = create_notification() else { return b };
    if !bind_irq(MSI_TEST_VEKTOR as u32, nid, MSI_PROBE_BADGE, hal::cpu::core_id()) {
        return b;
    }
    let Ok(ticket) = hal::vtd::irte_vergib(
        0, // SID 0 -- s. Funktionsdoku
        MSI_TEST_VEKTOR,
        hal::intc::lapic_id(),
        hal::vtd::Vektorform::MsiX,
        1,
    ) else {
        return b;
    };
    let Some((addr, data, _)) = ticket.eintrag(0) else {
        let _ = hal::vtd::irte_zieh_ein(&ticket);
        return b;
    };
    b.sprechfaehig = true;
    b.addr = addr;
    b.data = data;
    let vorher = irqs_delivered();
    let (hook_vorher, _) = irq_hook_stats();
    // **Der Store.** Genau das, was das Geraet tut, wenn es seine MSI-X-Zeile abschickt.
    // SAFETY: `addr` liegt im Nachrichtenfenster (`0xFEE0_0000..`), identity-gemappt und
    // uncacheable; ein 32-Bit-Store dorthin IST die Nachricht.
    unsafe { core::ptr::write_volatile(addr as u64 as *mut u32, data) };
    // Warten wie die uebrigen Sonden: auf die GROESSE, mit Frist -- eine Zaehlschleife maesse die
    // Geschwindigkeit des Wartenden.
    let t0 = hal::timer::ticks(0);
    let mut wache = 0u64;
    while hal::timer::ticks(0).wrapping_sub(t0) < 10 && wache < 200_000_000 {
        if irqs_delivered() > vorher {
            break;
        }
        core::hint::spin_loop();
        wache += 1;
    }
    b.zugestellt = irqs_delivered() > vorher;
    b.remap_angekommen = irq_hook_stats().0 > hook_vorher;
    if b.zugestellt && nid < ntfns().len() {
        b.badge_stimmt = ntfns()[nid].lock().pending_badge() & MSI_PROBE_BADGE != 0;
    }

    // --- Die Haelfte, die traegt: ein SELF-IPI, ohne Remapping ------------------------------
    //
    // **Nicht ein Store nach `0xFEE0_0000`** -- das ist die LAPIC-MMIO-Seite des eigenen Kerns,
    // also ein Registerzugriff und keine Nachricht; unter x2APIC ist die Seite ueberdies
    // abgeschaltet. Der erste Anlauf dieser Probe hat genau das gemessen und `false` gemeldet,
    // ohne dass es etwas ueber den Zustellpfad ausgesagt haette.
    //
    // Der Self-IPI misst APIC → IDT → Dispatch → `irq_hook` → Drain → Notification und laesst
    // Remapping und Geraeteseite ausdruecklich weg. Kommt er an, ist diese Haelfte bewiesen und
    // die offene Frage schrumpft auf Erzeugung und Uebersetzung.
    let vorher2 = irqs_delivered();
    let (hook_vorher2, _) = irq_hook_stats();
    let kompat_addr: u32 = 0; // kein Store mehr -- die Zahl bleibt im Bericht als 0 stehen
    let _ = kompat_addr;
    hal::intc::self_ipi(MSI_TEST_VEKTOR);
    let t1 = hal::timer::ticks(0);
    let mut w2 = 0u64;
    while hal::timer::ticks(0).wrapping_sub(t1) < 10 && w2 < 200_000_000 {
        if irqs_delivered() > vorher2 {
            break;
        }
        core::hint::spin_loop();
        w2 += 1;
    }
    b.kompat_zugestellt = irqs_delivered() > vorher2;
    let (hook_nachher, letzter) = irq_hook_stats();
    b.kompat_angekommen = hook_nachher > hook_vorher2;
    b.letzter_vektor = letzter;
    b.faults_leer = hal::vtd::faults_empty();

    let _ = hal::vtd::irte_zieh_ein(&ticket);
    b
}

/// Belegte MSI-Vektoren, indiziert relativ zu [`MSI_VEKTOR_BASIS`] — ein Bit je
/// vergebbaren Vektor ([`MSI_VEKTOR_POOL`] Stück).
#[cfg(target_arch = "x86_64")]
#[allow(clippy::declare_interior_mutable_const)]
static MSI_VEKTOR_BELEGT: [AtomicBool; MSI_VEKTOR_POOL] =
    [const { AtomicBool::new(false) }; MSI_VEKTOR_POOL];

/// Einen **zusammenhängenden** CPU-Vektorblock der Länge `n` für ein Gerät reservieren.
///
/// Zusammenhängend, weil die IRTE-Vergabe (`irte_vergib` mit `anzahl = n`, Form `MsiX`) einen
/// Block mit `basis_vektor + i` je Eintrag einträgt: gestreute Vektoren liessen sich dort nicht
/// abbilden. `None` = kein zusammenhängender Block dieser Länge frei — gerätelokal, die anderen
/// Geräte stört es nicht (E12: „dieses Gerät hat keinen freien Vektor").
#[cfg(target_arch = "x86_64")]
fn msi_block_reservieren(n: usize) -> Option<u8> {
    if n == 0 || n > MSI_VEKTOR_POOL {
        return None;
    }
    let mut start = 0usize;
    while start + n <= MSI_VEKTOR_POOL {
        // Erst schauen (alle frei?), dann nehmen — zwei Durchgänge statt einem, damit ein
        // halb belegter Block nicht halb reserviert stehenbleibt.
        let mut frei = true;
        for i in 0..n {
            if MSI_VEKTOR_BELEGT[start + i].load(Ordering::Acquire) {
                frei = false;
                break;
            }
        }
        if frei {
            let mut genommen = 0usize;
            for i in 0..n {
                if MSI_VEKTOR_BELEGT[start + i]
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    genommen += 1;
                } else {
                    break;
                }
            }
            if genommen == n {
                return Some(MSI_VEKTOR_BASIS + start as u8);
            }
            // Ein anderer hat dazwischen gegriffen: Zurückgeben, was wir genommen haben, und
            // hinter der Lücke weitersuchen statt von vorn.
            for i in 0..genommen {
                MSI_VEKTOR_BELEGT[start + i].store(false, Ordering::Release);
            }
            start += 1;
        } else {
            start += 1;
        }
    }
    None
}

/// Einen Vektorblock zurückgeben (Rücknahme einer halben Zuteilung, Teardown).
#[cfg(target_arch = "x86_64")]
fn msi_block_freigeben(basis: u8, n: usize) {
    let start = basis.wrapping_sub(MSI_VEKTOR_BASIS) as usize;
    for i in 0..n {
        if start + i < MSI_VEKTOR_POOL {
            MSI_VEKTOR_BELEGT[start + i].store(false, Ordering::Release);
        }
    }
}

/// **What a granted multi-vector device interrupt consists of.**
///
/// Four pieces and not three: `ticket` is the `#[must_use]` receipt `irte_vergib` hands out for
/// the whole block (`anzahl` entries), and it is the **only** thing `irte_zieh_ein` accepts.
/// Keeping just the handle would leave present IRTEs that nobody can withdraw any more —
/// precisely what that `#[must_use]` exists to prevent.
#[derive(Clone, Copy)]
struct MsiMehrGrant {
    /// Erster CPU-Vektor des Blocks; Vektor `i` liegt auf `basis + i`.
    basis: u8,
    /// Vergebene Vektoren (`1..=VEKTOREN_JE_ZUTEILUNG`).
    anzahl: usize,
    /// Erster IRTE-Index; Eintrag `i` liegt auf `handle + i` (Blockvergabe, zusammenhängend).
    handle: u16,
    /// x86 only: on aarch64 there is no ticket, because there is no allocation (see
    /// [`msi_grant`]). A field that only exists where it means something beats an `Option` that
    /// is `None` on a whole architecture.
    #[cfg(target_arch = "x86_64")]
    ticket: hal::vtd::MsiZiel,
}

/// **Take a multi-vector interrupt grant back — device first, translation second, vectors last.**
///
/// The order is the assurance, not the tidiness. Reversed — withdraw the IRTEs, then mask, then
/// free — there is a window in which the device is still armed and its entries are already gone:
/// an MSI arriving there hits a **not present** IRTE. That is fail-closed as delivery, but it is
/// a VT-d fault with no owner, counted against nothing, and it happens on a perfectly ordinary
/// abort path.
///
/// This is the same order the DMA teardown already runs — **source before translation** — and it
/// is one place on purpose: every abort path in [`assign_driver_device`] goes through here, so the
/// paths before and after `msix_enable` cannot drift apart.
#[cfg(target_arch = "x86_64")]
fn msi_revoke(dev: &DriverDevice, g: &MsiMehrGrant) {
    // 1. Still the device. Function Mask rather than `Enable = 0`, see `msix_quiesce_by_rid`.
    let _ = hal::pcie::msix_quiesce_by_rid(dev.rid, dev.msix_cap);
    if dev.msix_table != 0 {
        // SAFETY: `msix_table` is this device's identity-mapped table base (determined from BAR
        // index + offset when the device was offered), each row `i < anzahl` below the reported
        // row count (the grant capped `anzahl` at the offered entries), and the device belongs
        // to nobody else until it is released.
        for i in 0..g.anzahl {
            unsafe { hal::pcie::msix_mask_entry(dev.msix_table, i as u16) };
        }
    }
    // 2. Only now the translation: nothing can be in flight towards it any more. One ticket for
    // the whole block — `irte_zieh_ein` withdraws every row (`P = 0`, invalidate, free).
    let _ = hal::vtd::irte_zieh_ein(&g.ticket);
    // 3. And only then the vectors, which is what a later grant would hand out again.
    msi_block_freigeben(g.basis, g.anzahl);
}

/// aarch64 has no MSI grant to take back — see [`msi_grant`].
#[cfg(not(target_arch = "x86_64"))]
fn msi_revoke(_dev: &DriverDevice, _g: &MsiMehrGrant) {}

/// **Allocate an IRTE block, write the device's MSI-X rows, arm them** (Stufe B, B1).
///
/// At the same place as the DMA attach and for the same reason: the assignment is the point at
/// which a device is tied to a PD, and both authorities — *where may it write* and *where may it
/// interrupt* — belong in the same decision. `SVT/SID` is set in the same step: an IRTE without a
/// source check accepts an MSI from **any** device, and then interrupt delivery is exactly the
/// channel A-5.4 closed on the DMA axis. Setting it afterwards would mean a window in which the
/// entry exists and does not check.
///
/// **Die gerätelokale Voll-Absage:** `wunsch` nennt, wie viele Vektoren dieses Gerät bekommen
/// soll; was das Gerät nicht trägt oder was nicht frei ist, gibt es **gar keinen** statt ein paar
/// (`None`, der Treiber pollt). Ein halber Satz wäre die halbe Zuteilung, die schlimmer ist als
/// keine: der Treiber bände Vektoren, deren Zeilen nie geschrieben wurden, und die Bindung trüge
/// eine Autorität ohne Übersetzung. Die Absage stört kein anderes Gerät (E12) — die Prüfung
/// ([`geraet_vektoren`]) läuft VOR der ersten Reservierung, und jeder Fehlschlag danach zieht
/// alles bereits Genommene über [`msi_revoke`] wieder ein.
///
/// **Kein Wunsch ist zu gross für diese Funktion, nur für dieses Gerät:** mehr als
/// [`VEKTOREN_JE_ZUTEILUNG`] oder mehr als die angebotenen Zeilen wird abgewiesen, nicht gekürzt —
/// gekürzt hiesse, der Aufrufer bekäme weniger, als er gebunden hat. Was das Gerät anbietet
/// (`msix_eintraege`), deckelt den Wunsch von oben: ein Gerät mit zwei Zeilen bekommt zwei
/// Vektoren, keines mit vier Zeilen Wunschdenken.
///
/// **No interrupt is not a failure.** A device without MSI-X (or a unit without live remapping)
/// gets an assignment without vectors — the driver then polls as before. Refusing the device
/// instead would make Stufe B a *precondition* of A-5.1 rather than its extension. What that
/// costs is a distinction the report has to carry: see `driver_msi`.
#[cfg(target_arch = "x86_64")]
fn msi_grant(dev: &DriverDevice, wunsch: usize) -> Option<MsiMehrGrant> {
    if dev.msix_table == 0 || dev.msix_eintraege == 0 {
        return None;
    }
    // **VORHER, rein, ohne Hardware:** was dieses Gerät trägt, entscheidet die gerätelokale
    // Funktion — `None` statt einer Zahl, die weder in die Tabelle noch in den Schutz passt.
    // (Die Bindungsseite prüft den Satz mit `caprock_microkit::paare_pruefen` VOR der ersten
    // Bindung; das ist der Zwilling dieser Zeile auf der anderen Seite des Syscalls.)
    let anzahl = wunsch.min(dev.msix_eintraege as usize);
    if anzahl == 0 || anzahl > VEKTOREN_JE_ZUTEILUNG {
        return None;
    }
    match hal::vtd::geraet_vektoren(anzahl, dev.msix_eintraege as usize) {
        hal::vtd::GeraeteVerdikt::Gewaehren(n) if n == anzahl => {}
        // `GeraetVoll` und jede gekürzte Gewährung: Voll-Absage statt Teilsatz (s. oben).
        _ => return None,
    }
    let basis = msi_block_reservieren(anzahl)?;
    let ticket = match hal::vtd::irte_vergib(
        dev.rid,
        basis,
        hal::intc::lapic_id(),
        hal::vtd::Vektorform::MsiX,
        anzahl,
    ) {
        Ok(t) => t,
        Err(_) => {
            // Nothing is present and nothing was written to the device: the reserved block is
            // the only thing to give back, and `msi_revoke` has no ticket to take.
            msi_block_freigeben(basis, anzahl);
            return None;
        }
    };
    let handle = ticket.handle();
    let g = MsiMehrGrant { basis, anzahl, handle, ticket };
    // **Je Zeile ihre eigene Adresse** — Zeile `i` trägt Handle `handle + i` (s. `MsixZeile`):
    // erst Adresse und Datenwort, dann die Maske lösen, sonst ginge ein Interrupt in der
    // Lücke an Vektor 0.
    for z in g.ticket.msix_zeilen() {
        // SAFETY: see `msi_revoke` (eigene Tabelle, Zeile < angebotene Zeilen, exklusives Gerät).
        unsafe { hal::pcie::msix_write_entry(dev.msix_table, z.zeile, z.addr, z.data) };
    }
    let ctrl = hal::pcie::msix_enable_by_rid(dev.rid, dev.msix_cap);
    // A spoken-for check, not a convenience: a configuration space that does not answer returns
    // `0xffff`, and a write nobody reads back looks the same in both cases.
    if hal::pcie::all_ones16(ctrl) || ctrl & hal::pcie::MSIX_CTRL_ENABLE == 0 {
        msi_revoke(dev, &g);
        return None;
    }
    Some(g)
}

/// **aarch64 grants no vector — and says so rather than pretending.**
///
/// GICv3/ITS is a different mechanism and is an explicit non-goal of Stufe B (plan §5); the
/// existing `irq` line already covers the aarch64 delivery path with a real device (RTC). So the
/// honest answer here is „no vector", which is exactly the state this architecture is in today:
/// the driver polls.
///
/// **Not `unimplemented!()`** — this path is walked on every device assignment, and it is not a
/// gap in the code, it is the scope of the stage. The seam sits here and not at the four call
/// sites inside `assign_driver_device`, because those four called `hal::vtd` directly and broke
/// the aarch64 build (E0433 × 4, E0425 × 4) — third instance of *arch-neutral kernel code calls a
/// HAL function that only x86 has*.
#[cfg(not(target_arch = "x86_64"))]
fn msi_grant(_dev: &DriverDevice, _wunsch: usize) -> Option<MsiMehrGrant> {
    None
}

/// Was einer Treiber-PD tatsächlich zugeteilt wurde — gebraucht, um die **Gerätesicht** ihrer
/// DMA-Region wiederzufinden, wenn sie diese mappt.
#[derive(Clone, Copy)]
struct DriverAssign {
    used: bool,
    /// **WEM** diese Zuteilung gehört (A-5.4) — die `program_id` aus dem Manifest-Eintrag.
    ///
    /// Bis A-5.3 gab es genau einen Treiber, und vier Stellen im Kernel nahmen deshalb „die
    /// **erste** benutzte Zuteilung" (`find(|a| a.used)`). Das ist bei zwei Treibern keine
    /// Abkürzung mehr, sondern eine **stille Fehlwahl**: der Client des Blockdienstes bekäme die
    /// Übertragungsfläche irgendeines Treibers, und ein Hot-Reload träfe irgendeinen.
    ///
    /// Der Schlüssel ist die `program_id` und nicht die PD-Nummer: die PD entsteht erst *nach*
    /// der Zuteilung (`endow_from_manifest` läuft vor `load_image`), und die `program_id` steht
    /// im **Autoritätsdokument** — dieselbe Zahl, mit der das Manifest den Eintrag benennt.
    program_id: u32,
    /// **Welches** Gerät hier hängt (A-5.3). Ohne diese Zahl wäre „der Treiber hat ein Gerät" die
    /// einzige Aussage, die der Bericht treffen könnte — und genau die ist bei zwei Geräten
    /// wertlos, weil sie auch dann wahr ist, wenn die Zuteilung vertauscht wurde.
    rid: u32,
    vendor: u16,
    device: u16,
    dma_phys: u64,
    dma_len: u64,
    /// **Was der Ladeaufruf angefordert hat**, in Seiten (`0` = Vorgabe) -- C2.
    ///
    /// Steht neben `dma_len` und nicht statt ihm: der Bericht prueft `dma_len == gewuenscht*4096`,
    /// und dafuer braucht er beide Zahlen. Nur die gewaehrte zu fuehren hiesse, „gekuerzt" von
    /// „so angefordert" nicht unterscheiden zu koennen -- genau die Aussage, um die es hier geht.
    dma_gewuenscht: u32,
    // --- Stufe B: Interrupt (Mehrvektor) ---------------------------------------------------
    //
    // **Die Bindung haengt HIER und nicht an einer globalen Tabelle** (E12). Es gibt bereits eine
    // natuerliche Obergrenze -- [`VEKTOREN_JE_ZUTEILUNG`] Vektoren je Geraet --, also braucht es
    // kein Konto: wer ein Geraet hat, hat genau dessen Bindungen, und kein Treiber-PD kann einem
    // anderen die Ressource wegnehmen, weil die Zuteilung schon die Vergabestelle ist.
    // `ERR_IRQ_FULL` meint damit etwas **Lokales**: dieses Geraet hat keinen freien Vektor.
    //
    // Dieselbe Zahl trug vorher `NIRQ_BIND = 4` global -- formgleich zu `CAP_BUDGET_PER_PD` vor
    // der Kontoumstellung, nur dass hier die Obergrenze schon existierte.
    /// Erster CPU-Vektor des Blocks (`0` = keiner vergeben — zweideutig allein, s. `msi_da`).
    msi_basis: u8,
    /// Vergebene Vektoren (`0` = keine; Vektor `i` liegt auf `msi_basis + i`).
    msi_anzahl: u8,
    /// Der erste IRTE-Index, den die MSI-X-Zeilen tragen. Nur fuer den Einzug beim Teardown —
    /// die Zeilen liegen als Block (`handle + i`), ein Ticket braucht es danach nicht mehr.
    msi_handle: u16,
    /// Ist er vergeben? (`msi_basis == 0` waere zweideutig -- 0 ist ein gueltiger Vektor.)
    msi_da: bool,
    /// Identity-Adresse der MSI-X-Tabelle (`0` = keine) — mitgefuehrt, damit der Bericht die Zeile
    /// **zurueklesen** kann, statt zu glauben, dass sie noch steht.
    msix_table: u64,
    /// Offset der MSI-X-Capability im Konfigurationsraum (`0` = keine).
    msix_cap: u16,
    /// Die Notifications, auf denen der Treiber seine Interrupts erwartet (B4, je Vektor eine).
    /// `u32::MAX` = keine — nur `[..msi_anzahl]` ist belegt.
    ///
    /// Die **Ids**, nicht die Caps: der Bericht will wissen, ob DIESES Objekt signalisiert wurde,
    /// und eine Cap sagt darueber nichts (der Treiber haelt eine Kopie, der Kernel das Original).
    /// Die Nachfolgefassung (Hot-Reload) bekommt frische Caps auf DIESELBEN Ids — ein frisches
    /// Objekt hiesse, dass der Interrupt weiter an das Objekt der gestorbenen Fassung geht.
    msi_ntfn: [u32; VEKTOREN_JE_ZUTEILUNG],
    /// **Bietet das GERAET ueberhaupt MSI-X an?** — der Wunsch neben der Gewaehrung, wie
    /// `dma_gewuenscht` neben `dma_len`.
    ///
    /// Ohne diese Zahl sind zwei Lagen ununterscheidbar, und die harmlose verdeckt die ernste:
    /// „das Geraet hat kein MSI-X, der Treiber pollt zulaessig" und „das Geraet hat MSI-X und die
    /// Vergabe ist gescheitert". Eine Pruefzeile, die nur `msi_da` liest, ist in beiden Faellen
    /// gleich -- also gruen fuer einen stillschweigend ausgefallenen Interruptpfad. Dieselbe Form
    /// wie „so gewollt" gegen „stillschweigend gekuerzt" bei C2.
    msi_angeboten: bool,
    iova: u64,
    /// Die Fenster, damit eine **Nachfolgefassung** dieselbe Zuteilung bekommen kann, ohne dass
    /// das Gerät dafür losgelassen und neu gesucht würde (A-5.1, Hot-Reload).
    cfg_page: u64,
    bar: u64,
    bar_len: u64,
    /// Wurzel-Cap der geteilten Uebertragungsflaeche (A-6.3). Jeder Halter bekommt eine **Kopie**;
    /// das Original bleibt beim Kernel, damit ein sterbender Treiber die Flaeche nicht mitnimmt.
    shared_root: Option<CapPtr>,
    shared_phys: u64,
}

impl DriverAssign {
    const EMPTY: Self = Self {
        used: false,
        program_id: 0,
        rid: 0,
        vendor: 0,
        device: 0,
        dma_phys: 0,
        dma_len: 0,
        dma_gewuenscht: 0,
        msi_basis: 0,
        msi_anzahl: 0,
        msi_handle: 0,
        msi_da: false,
        msix_table: 0,
        msix_cap: 0,
        msi_ntfn: [u32::MAX; VEKTOREN_JE_ZUTEILUNG],
        msi_angeboten: false,
        iova: 0,
        cfg_page: 0,
        bar: 0,
        bar_len: 0,
        shared_root: None,
        shared_phys: 0,
    };
}

static DRIVER_ASSIGN: SpinLock<[DriverAssign; MAX_DRIVER_ASSIGN]> =
    SpinLock::new([DriverAssign::EMPTY; MAX_DRIVER_ASSIGN]);

/// Ein gefundenes Gerät als zuteilbar anmelden (Hochlauf). `false` = Liste voll.
pub fn offer_driver_device(d: DriverDevice) -> bool {
    let mut g = DRIVER_DEVICES.lock();
    for s in g.iter_mut() {
        if s.is_none() {
            *s = Some(d);
            return true;
        }
    }
    false
}

/// Wie viele Geräte noch zuteilbar sind (Bericht).
pub fn offered_device_count() -> usize {
    DRIVER_DEVICES.lock().iter().filter(|s| s.is_some()).count()
}

/// Die angebotenen Geräte für den Bericht auslesen (RID, Hersteller, Gerät, Klasse).
pub fn offered_devices(out: &mut [(u32, u16, u16, u32)]) -> usize {
    let g = DRIVER_DEVICES.lock();
    let mut n = 0;
    for d in g.iter().flatten() {
        if n == out.len() {
            break;
        }
        out[n] = (d.rid, d.vendor, d.device, d.class);
        n += 1;
    }
    n
}

/// Die **Gerätesicht** (IOVA) einer zugeteilten DMA-Region nachschlagen.
///
/// Sie wird **nicht** aus der PA gerechnet. Die IOVA stammt aus dem Fenster des
/// Übersetzungskontexts, das der Enforcer beim Anhängen gewählt hat; oberhalb von `RAM_TOP` kann
/// sie nie zufällig eine gültige PA sein, und genau darauf beruht die Trennung der beiden Achsen.
fn assigned_iova(dma_phys: u64) -> Option<u64> {
    DRIVER_ASSIGN
        .lock()
        .iter()
        .find(|a| a.used && a.dma_phys == dma_phys)
        .map(|a| a.iova)
}

/// Groesse der **geteilten Uebertragungsflaeche** (A-6.3).
///
/// Sie ist **nicht** die DMA-Region des Treibers, und das ist der Punkt: die ist non-coherent
/// gemappt (Normal-NC), damit das Geraet hineinschreiben kann, ohne dass jemand Cache-Wartung
/// fahren muss. Eine zweite, **gecachte** Abbildung derselben Seiten in einer Client-PD waere auf
/// x86 ein Attribut-Alias -- laut SDM undefiniert, in der Praxis „geht meistens". Also eine
/// eigene Region aus normalem RAM, in beiden PDs mit denselben Attributen, und der Treiber
/// kopiert. Ein echter Treiber tut das ohnehin, sobald der Client-Puffer nicht DMA-faehig ist.
const SHARED_BYTES: u64 = 8 * 1024;

/// Die Caps, die eine Treiber-PD beim Start bekommt (A-5.1).
pub struct DriverGrant {
    /// Slot 3: die eigene Konfigurationsraum-Seite (der Treiber löst sein Gerät selbst auf).
    pub cfg: CapPtr,
    /// Slot 4: das Registerfenster.
    pub bar: CapPtr,
    /// Slot 5: die DMA-Region, bereits an die RID des Geräts angehängt.
    pub dma: CapPtr,
    /// Slot 6: die geteilte Übertragungsfläche (A-6.3) — dieselbe Region, die ein Client des
    /// Blockdienstes bekommt. Der Treiber kopiert gelesene Sektoren dorthin.
    pub shared: CapPtr,
    /// Slot 8: die **Notification**, auf der der Treiber seinen Interrupt erwartet (B4).
    ///
    /// **Ein eigenes Objekt und nicht die Kanal-Notification** (Slot 1), und der Grund ist ein
    /// Messfehler, kein Geschmack: eine Notification **rastet ein**. Der Treiber signalisiert auf
    /// seinem Kanal „ich bin bereit"; wartete er spaeter auf demselben Objekt, kaeme `WAIT` sofort
    /// mit einem alten pending-Bit zurueck — und das saehe von einem zugestellten Interrupt in
    /// nichts zu unterscheiden aus. `poll-runden == 0` waere gruen, ohne dass je ein Interrupt
    /// gekommen ist. Dieselbe Familie wie „eine Ablage je ROLLE ist eine Ablage zu wenig".
    ///
    /// `None` genau dann, wenn auch [`Self::irq`] `None` ist — ohne Vektor gibt es nichts zu
    /// erwarten.
    pub irq_ntfn: Option<CapPtr>,
    /// Die **Id** desselben Objekts. Die Cap allein reicht nicht: die HardwareLand-Cap-Politik
    /// entscheidet ueber die **Id**, und sie muss vor dem Endowment in der PD stehen
    /// ([`pd_set_irq_ntfn`]).
    pub irq_ntfn_id: u32,
    /// Slot 7: die **`Irq`-Cap** dieses Geräts (B2). `None` = das Gerät hat keinen Vektor
    /// bekommen — entweder hat es kein MSI-X, oder die Vergabe ist gescheitert.
    ///
    /// **`Option` und nicht ein Platzhalter**, weil die Abwesenheit eine Aussage ist: ein Treiber,
    /// dessen Slot 7 leer bleibt, pollt zulässig. Welcher der beiden Fälle vorliegt, sagt
    /// `driver_msi` über `angeboten` — hier wäre die Unterscheidung nicht unterzubringen, ohne aus
    /// einer Cap eine Statusmeldung zu machen.
    ///
    /// Das ist das **erste** Paar; die übrigen stehen in [`Self::irq_mehr`]. Getrennt, weil der
    /// Lader (fremder Besitz) heute nur Slot 7/8 endowt: was er kennt, behält seinen Namen, was
    /// neu ist, steht daneben statt darin.
    pub irq: Option<CapPtr>,
    /// Vergebene Vektoren (`0` = keine — der Treiber pollt zulässig).
    pub irq_anzahl: usize,
    /// Die **`Irq`-Caps je Vektor** (B2×N): `[i]` gehört zu Vektor `i` des Blocks.
    ///
    /// **Nur `[..irq_anzahl]` ist belegt**; der Rest ist `None`. `[0]` ist dieselbe Cap wie
    /// [`Self::irq`] — ein zweites Original daneben wäre zwei Wahrheiten über einen Vektor.
    /// Der Verbraucher (Lader, fremder Besitz) endowt heute `[0]` nach Slot 7 und trägt die
    /// übrigen nach, sobald es Slots dafür gibt — oder löscht sie ausdrücklich (`cap_delete`),
    /// statt sie still fallenzulassen.
    pub irq_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG],
    /// Die **Notification-Caps je Vektor** (B2×N) — `[i]` gehört zu `[i]` in [`Self::irq_mehr`].
    /// `[0]` ist dieselbe Cap wie [`Self::irq_ntfn`], aus demselben Grund wie dort.
    pub irq_ntfn_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG],
    /// Die **Ids** derselben Objekte — `[i]` gehört zum Paar `[i]`. Die Nachfolgefassung
    /// (Hot-Reload) bekommt frische Caps auf DIESELBEN Ids (s. `DriverAssign::msi_ntfn`).
    pub irq_ntfn_ids: [u32; VEKTOREN_JE_ZUTEILUNG],
}

/// Ein Gerät herausnehmen, das auf `sel` passt (A-5.3). Es ist danach **vergeben**.
///
/// **Fail-closed:** passt keines, gibt es `None` — es wird ausdrücklich **nicht** auf „dann eben
/// irgendeins" zurückgefallen. Der Rückfall wäre die bequeme Zeile und genau die versteckte
/// Politik, gegen die der Selektor antritt: ein Manifest, das ein Blockgerät verlangt, bekäme
/// stillschweigend eine Netzkarte, und der Treiber fände sein Gerät „irgendwie nicht".
fn take_matching_device(sel: man::DeviceSelector) -> Option<DriverDevice> {
    let mut g = DRIVER_DEVICES.lock();
    for s in g.iter_mut() {
        if let Some(d) = *s {
            if sel.matches(d.vendor, d.device, d.class) {
                *s = None;
                return Some(d);
            }
        }
    }
    None
}

/// Ein zurückgegebenes Gerät wieder anbieten (Rücknahme bei halbem Fehlschlag).
fn put_back_device(d: DriverDevice) {
    let mut g = DRIVER_DEVICES.lock();
    for s in g.iter_mut() {
        if s.is_none() {
            *s = Some(d);
            return;
        }
    }
}

/// **Ein zuteilbares Gerät an eine Treiber-PD vergeben** (A-5.1, seit A-5.3 mit Selektor).
///
/// Erzeugt vier Caps und hängt die DMA-Region an den Übersetzungskontext der Geräte-RID. Danach
/// ist das Gerät **vergeben** — ein zweiter Aufruf mit demselben Selektor liefert `None`, statt
/// dasselbe Gerät ein zweites Mal auszugeben. Zwei Treiber auf einem Gerät wären kein Grenzfall,
/// sondern zwei Instanzen, die sich gegenseitig die Virtqueue umschreiben.
///
/// `sel` kommt aus dem Manifest-Eintrag. [`man::DeviceSelector::ANY`] ist das Verhalten vor A-5.3.
///
/// Schlägt ein Schritt fehl, werden die vorher erzeugten Caps wieder abgeräumt und das Gerät
/// bleibt vergebbar: eine halbe Zuteilung ist schlimmer als keine, weil der Treiber dann an einer
/// Stelle scheitert, die niemand mit der Zuteilung in Verbindung bringt.
/// `dma_pages == 0` heisst **Vorgabe** ([`DRIVER_DMA_BYTES`]) -- bitgleich zu jedem Aufruf, den es
/// vor C2 gab. Ueber [`DRIVER_DMA_MAX_BYTES`] hinaus wird **abgewiesen und nicht gekuerzt**; die
/// Absage faellt schon im Dispatch mit eigenem Code (`ERR_DMA_TOO_LARGE`), damit sie den Aufrufer
/// erreicht, bevor irgendetwas alloziert ist. Hier steht sie **trotzdem noch einmal**: dieser
/// Einstieg ist auch aus dem Kernel erreichbar, und eine Schranke, die nur am Syscall-Rand haelt,
/// ist keine Schranke der Funktion.
pub fn assign_driver_device(
    sel: man::DeviceSelector,
    program_id: u32,
    dma_pages: u32,
) -> Option<DriverGrant> {
    let dma_bytes = if dma_pages == 0 {
        DRIVER_DMA_BYTES
    } else {
        (dma_pages as u64) * 4096
    };
    if dma_bytes > DRIVER_DMA_MAX_BYTES {
        // **Nicht kuerzen.** Eine stillschweigend halbierte DMA-Region ist ein Geraet, das ueber
        // ihr Ende hinausschreibt -- der Fehler waere still und traefe fremden Speicher.
        return None;
    }
    let dev = take_matching_device(sel)?;
    let give_back = || put_back_device(dev);

    let Some(region) = alloc_dma_region(dma_bytes) else {
        give_back();
        return None;
    };
    // Non-coherent (Normal-NC): der Treiber liest den used-Ring, den das Gerät geschrieben hat.
    // Cacheable wäre auf x86 richtig und auf aarch64 falsch, solange der Treiber keine
    // Cache-Wartung fährt -- und eine Zuteilung, die nur auf einer Architektur trägt, ist eine
    // Falle mit Verfallsdatum.
    let dma = match install_dma_cap_ex(
        region.base,
        region.len,
        DmaDir::Bidirectional,
        DmaCoherence::NonCoherent,
        Rights::RW,
    ) {
        Ok(c) => c,
        Err(_) => {
            MEM.lock().free_region(region);
            give_back();
            return None;
        }
    };
    // **Anhängen, bevor der Treiber existiert.** Danach ist die Region für genau diese RID
    // übersetzbar und für jede andere nicht -- das ist die Aussage, die eine Treiber-PD von
    // "der Kernel macht DMA für mich" unterscheidet.
    let Some(handle) = dma_attach(dev.rid, dma) else {
        let _ = cap_delete(dma);
        give_back();
        return None;
    };
    // --- Stufe B: IRTE-Block + MSI-X-Zeilen (B1) ------------------------------------------------
    //
    // At the same place as the DMA attach and for the same reason; the whole argument, and the
    // arch seam it sits behind, is at `msi_grant`. Der Wunsch ist die Entscheidung (4 Vektoren je
    // Gerät); was das Gerät nicht trägt oder was nicht frei ist, gibt es gar keinen statt ein paar
    // (Voll-Absage, s. `msi_grant`).
    let msi = msi_grant(&dev, VEKTOREN_JE_ZUTEILUNG);
    let msi_zurueck = || {
        if let Some(g) = msi.as_ref() {
            msi_revoke(&dev, g);
        }
    };
    let cfg = match install_mmio_cap(dev.cfg_page, 4096, Rights::RW) {
        Ok(c) => c,
        Err(_) => {
            msi_zurueck();
            dma_detach(dev.rid, handle);
            let _ = cap_delete(dma);
            give_back();
            return None;
        }
    };
    let bar = match install_mmio_cap(dev.bar, dev.bar_len, Rights::RW) {
        Ok(c) => c,
        Err(_) => {
            let _ = cap_delete(cfg);
            msi_zurueck();
            dma_detach(dev.rid, handle);
            let _ = cap_delete(dma);
            give_back();
            return None;
        }
    };
    // Die geteilte Uebertragungsflaeche (A-6.3): normales RAM, keine Geraetesicht. Das
    // **Original** bleibt beim Kernel, jeder Halter bekommt eine Kopie -- so nimmt ein sterbender
    // Treiber die Flaeche nicht mit, und die Nachfolgefassung findet dieselbe vor.
    let Some(shared_region) = alloc(SHARED_BYTES, 4096) else {
        let _ = cap_delete(bar);
        let _ = cap_delete(cfg);
        msi_zurueck();
        dma_detach(dev.rid, handle);
        let _ = cap_delete(dma);
        give_back();
        return None;
    };
    let shared_phys = shared_region.base();
    let Ok(shared_root) = cap_install(shared_region) else {
        let _ = cap_delete(bar);
        let _ = cap_delete(cfg);
        msi_zurueck();
        dma_detach(dev.rid, handle);
        let _ = cap_delete(dma);
        give_back();
        return None;
    };
    let Ok(shared) = cap_copy(shared_root, Rights::RW) else {
        let _ = cap_delete(bar);
        let _ = cap_delete(cfg);
        msi_zurueck();
        dma_detach(dev.rid, handle);
        let _ = cap_delete(dma);
        give_back();
        return None;
    };
    // --- B2×N: je Vektor ein Paar ---------------------------------------------------------
    //
    // **Zuletzt geprägt, und das ist Absicht:** es ist der einzige Schritt, dessen Misserfolg
    // nichts anderes rückgängig machen muss als sich selbst, also steht er dort, wo genau ein
    // Abweispfad hinter ihm liegt. Andersherum trüge jeder der sechs Pfade darüber eine weitere
    // Aufräumzeile — und *ein neuer Abweispfad erbt die Aufräumpflicht des alten NICHT*.
    //
    // Die Cap trägt den **CPU-Vektor** als `intid`. Auf x86 ruft der Dispatch `irq_hook` mit dem
    // rohen Vektor, es gibt also keine zweite Nummer, die hier umgerechnet werden müsste — und
    // damit auch keine, die auseinanderlaufen könnte. Der Vektor-Index (`IRQ_VIDX`) wird erst beim
    // Binden aus der Zuteilung aufgelöst (s. `dispatch_bind_irq`).
    //
    // Kein Vektor, keine Cap: `None` heisst „dieses Gerät unterbricht nicht", und der Treiber
    // pollt zulässig. Das Gerät deswegen abzuweisen machte Stufe B zur Vorbedingung von A-5.1.
    //
    // **Beide oder keins — je Paar, und alle Paare oder keins.** Eine `Irq`-Cap ohne Notification
    // ist eine Autoritaet ohne Ziel, eine Notification ohne Cap ein Objekt ohne Grund — und der
    // Treiber verlangt beim Binden beide. Ein halb ausgestattetes Geraet scheiterte erst im
    // Treiber, an einer Stelle, die niemand mit der Zuteilung in Verbindung braechte. Gilt ein
    // Paar nicht, fallen alle bereits geprägten mit (Schleife unten) plus der ganze Rest der
    // Zuteilung — dieselbe Voll-Absage wie im Grant, eine Ebene höher.
    let mut irq_ntfn_ids = [u32::MAX; VEKTOREN_JE_ZUTEILUNG];
    let mut irq_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG] = [None; VEKTOREN_JE_ZUTEILUNG];
    let mut irq_ntfn_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG] =
        [None; VEKTOREN_JE_ZUTEILUNG];
    let irq_anzahl = msi.map_or(0, |g| g.anzahl);
    if let Some(g) = msi {
        for i in 0..g.anzahl {
            let vektor = g.basis + i as u8;
            let paar = create_notification().and_then(|nid| {
                let ic = install_irq_cap(vektor as u32, Rights::READ).ok()?;
                match install_notification_cap(nid as u32, Rights::RWX) {
                    Ok(nc) => Some((nid as u32, ic, nc)),
                    Err(_) => {
                        let _ = cap_delete(ic);
                        None
                    }
                }
            });
            match paar {
                Some((nid, ic, nc)) => {
                    irq_ntfn_ids[i] = nid;
                    irq_mehr[i] = Some(ic);
                    irq_ntfn_mehr[i] = Some(nc);
                }
                None => {
                    for j in 0..i {
                        if let Some(c) = irq_mehr[j] {
                            let _ = cap_delete(c);
                        }
                        if let Some(c) = irq_ntfn_mehr[j] {
                            let _ = cap_delete(c);
                        }
                    }
                    let _ = cap_delete(shared);
                    let _ = cap_delete(bar);
                    let _ = cap_delete(cfg);
                    msi_zurueck();
                    dma_detach(dev.rid, handle);
                    let _ = cap_delete(dma);
                    give_back();
                    return None;
                }
            }
        }
    }
    // Paar 0 behält seine alten Namen: der Lader (fremder Besitz) endowt heute Slot 7/8 und kennt
    // nur sie — was er kennt, behält seinen Namen, was neu ist, steht in den Feldern daneben.
    let irq = irq_mehr[0];
    let irq_ntfn = irq_ntfn_mehr[0];
    let irq_ntfn_id = irq_ntfn_ids[0];
    {
        let mut t = DRIVER_ASSIGN.lock();
        let Some(slot) = t.iter().position(|a| !a.used) else {
            drop(t);
            for j in 0..irq_anzahl {
                if let Some(c) = irq_mehr[j] {
                    let _ = cap_delete(c);
                }
                if let Some(c) = irq_ntfn_mehr[j] {
                    let _ = cap_delete(c);
                }
            }
            let _ = cap_delete(shared);
            let _ = cap_delete(bar);
            let _ = cap_delete(cfg);
            msi_zurueck();
            dma_detach(dev.rid, handle);
            let _ = cap_delete(dma);
            give_back();
            return None;
        };
        t[slot] = DriverAssign {
            used: true,
            program_id,
            rid: dev.rid,
            vendor: dev.vendor,
            device: dev.device,
            dma_phys: region.base,
            dma_len: region.len,
            dma_gewuenscht: dma_pages,
            msi_basis: msi.map_or(0, |g| g.basis),
            msi_anzahl: msi.map_or(0, |g| g.anzahl as u8),
            msi_handle: msi.map_or(0, |g| g.handle),
            msi_da: msi.is_some(),
            msix_table: dev.msix_table,
            msix_cap: dev.msix_cap,
            msi_ntfn: irq_ntfn_ids,
            // Read from the OFFER, not from the grant: `msix_cap != 0` means the device carries an
            // MSI-X capability, whatever became of it afterwards.
            msi_angeboten: dev.msix_cap != 0,
            iova: handle.iova.raw(),
            cfg_page: dev.cfg_page,
            bar: dev.bar,
            bar_len: dev.bar_len,
            shared_root: Some(shared_root),
            shared_phys,
        };
    }
    Some(DriverGrant {
        cfg,
        bar,
        dma,
        shared,
        irq,
        irq_ntfn,
        irq_ntfn_id,
        irq_anzahl,
        irq_mehr,
        irq_ntfn_mehr,
        irq_ntfn_ids,
    })
}

/// Eine **Kopie** der geteilten Uebertragungsflaeche fuer einen Client des Blockdienstes (A-6.3).
///
/// **Es gibt hier keinen ersten Treiber mehr** (A-5.4). Gesucht wird die Zuteilung, die ueberhaupt
/// eine Uebertragungsflaeche hat -- und wenn es davon mehr als eine gibt, wird **keine** gewaehlt:
/// „irgendeine" waere eine Politik, die niemand aufgeschrieben hat, und der Client bekaeme die
/// Flaeche eines Dienstes, den er nicht gemeint hat. Sobald zwei Dienste eine anbieten, muss das
/// Manifest sie benennen (Z11b/Z11c); bis dahin ist Verweigern die einzige ehrliche Antwort.
pub fn driver_shared_cap(service_id: u32) -> Option<CapPtr> {
    let g = DRIVER_ASSIGN.lock();
    let root = if service_id != 0 {
        // Der Client hat seinen Dienst **benannt** (A-5.4) -- dann gibt es nichts zu entscheiden.
        g.iter()
            .find(|a| a.used && a.program_id == service_id)?
            .shared_root?
    } else {
        // Nicht benannt: nur zulaessig, solange es genau eine Flaeche gibt. „Irgendeine" waere
        // eine Politik, die niemand aufgeschrieben hat, und der Client saehe die Daten eines
        // Dienstes, den er nicht gemeint hat.
        let mut it = g.iter().filter(|a| a.used && a.shared_root.is_some());
        let a = it.next()?;
        if it.next().is_some() {
            return None;
        }
        a.shared_root?
    };
    drop(g);
    cap_copy(root, Rights::RW).ok()
}

/// **Was tatsächlich zugeteilt wurde** (A-5.3, Bericht): `(RID, Hersteller, Geräte-ID)` je Platz.
///
/// Der Bericht braucht die **Instanz**, nicht die Anzahl. „Eine Zuteilung ist erfolgt" ist auch
/// dann wahr, wenn der Treiber das falsche Gerät bekommen hat — genau der Fehler, den ein
/// Selektor verhindern soll, wäre damit unsichtbar.
pub fn driver_assignments(out: &mut [(u32, u32, u16, u16)]) -> usize {
    let g = DRIVER_ASSIGN.lock();
    let mut n = 0;
    for a in g.iter().filter(|a| a.used) {
        if n == out.len() {
            break;
        }
        out[n] = (a.program_id, a.rid, a.vendor, a.device);
        n += 1;
    }
    n
}

/// **Der Interrupt-Zustand EINER Zuteilung** (Stufe B) — was der Bericht braucht, um zu urteilen.
///
/// Ein Typ und kein Tupel, seit es sechs Felder sind: `(u32, u32, u8, u16, bool, bool)` hat zwei
/// Zahlenpaare, die sich vertauschen lassen, ohne dass der Übersetzer etwas merkt — dieselbe
/// Begründung, mit der `Vektorwunsch` in `irte.rs` entstanden ist.
#[derive(Clone, Copy, Default, Debug)]
pub struct DriverMsi {
    pub program_id: u32,
    /// Die Requester-ID des Geräts — **die Größe, gegen die `SVT/SID` geprüft wird.**
    pub rid: u32,
    /// Erster CPU-Vektor des Blocks — **Alias für `vektoren[0]`**, damit die bisherigen Leser
    /// (Prüfzeile: `irte_pruefen(handle, vektor, rid)`, Slot-7-Vergleich) unverändert weiterlaufen.
    /// Die volle Blockprüfung (`irte_block_pruefen` je Zeile) ist Folgelarbeit dort, wo sie
    /// gelesen wird (Bring-up, fremder Besitz).
    pub vektor: u8,
    /// Erster IRTE-Index — **Alias für den Blockbeginn** (die Zeilen liegen als `handle + i`).
    pub handle: u16,
    /// Basis der DMA-Region — dort legt der Treiber seine B4-Zahlen ab.
    pub dma_phys: u64,
    /// Identity-Adresse der MSI-X-Tabelle des Geraets — fuer die Ruecklesung der Zeile.
    pub msix_table: u64,
    /// Offset der MSI-X-Capability — fuer die Ruecklesung des Kontrollregisters.
    pub msix_cap: u16,
    /// Die Konfigurationsraum-Seite — fuer die Ruecklesung von `queue_msix_vector`.
    pub cfg_page: u64,
    /// Ist ein Vektor vergeben?
    pub da: bool,
    /// **Bietet das Gerät überhaupt MSI-X an?** Der Wunsch neben der Gewährung.
    pub angeboten: bool,
    /// Vergebene Vektoren (`0` = keine) — die Zahl, über die der Block gelesen wird.
    pub anzahl: u8,
    /// Alle Vektoren des Blocks — nur `[..anzahl]` ist belegt.
    pub vektoren: [u8; VEKTOREN_JE_ZUTEILUNG],
    /// Die Notification-Ids je Vektor — nur `[..anzahl]` ist belegt (`u32::MAX` = keine).
    pub ntfn_ids: [u32; VEKTOREN_JE_ZUTEILUNG],
}

/// **Stufe B: der Interrupt-Zustand der Zuteilungen.**
///
/// **Fuenf Zahlen, und die fuenfte ist die, an der die Pruefzeile haengt.** `vergeben` allein
/// trennt nicht, was getrennt werden muss: ein Geraet ohne MSI-X pollt zulaessig, ein Geraet MIT
/// MSI-X und ohne Vektor ist eine **gescheiterte Vergabe** -- und beide melden `vergeben=false`.
/// Erst `angeboten` macht daraus zwei Aussagen, und die tragende Bedingung der `irqmsi`-Zeile ist
/// deshalb bedingt formuliert (s. `docs/plan-cap-irq.md` §3):
///
/// * `angeboten` ⟹ `vergeben` — sonst ist die Vergabe still ausgefallen,
/// * `vergeben` ⟹ `poll-runden == 0` — sonst hat der Treiber trotz Vektor gepollt.
///
/// Ohne das erste Konjunkt haette die Zeile genau zwei Enden: fuer den vektorlosen Fall
/// abgeschaltet (und damit blind gegen eine stillschweigend ausgefallene Vergabe), oder rot aus
/// einem zulaessigen Grund -- und dann entfernt sie jemand.
pub fn driver_msi(out: &mut [DriverMsi]) -> usize {
    let g = DRIVER_ASSIGN.lock();
    let mut n = 0;
    for a in g.iter().filter(|a| a.used) {
        if n == out.len() {
            break;
        }
        // `vektoren` aus der Blockbasis hergeleitet, nicht geraten: der Grant vergibt
        // zusammenhängend (`basis + i`), und genau das steht hier — ein Array daneben wäre ein
        // zweites Gedächtnis für dieselbe Wahrheit.
        let mut vektoren = [0u8; VEKTOREN_JE_ZUTEILUNG];
        for (i, v) in vektoren.iter_mut().enumerate().take(a.msi_anzahl as usize) {
            *v = a.msi_basis + i as u8;
        }
        out[n] = DriverMsi {
            program_id: a.program_id,
            rid: a.rid,
            vektor: a.msi_basis,
            handle: a.msi_handle,
            dma_phys: a.dma_phys,
            msix_table: a.msix_table,
            msix_cap: a.msix_cap,
            cfg_page: a.cfg_page,
            da: a.msi_da,
            angeboten: a.msi_angeboten,
            anzahl: a.msi_anzahl,
            vektoren,
            ntfn_ids: a.msi_ntfn,
        };
        n += 1;
    }
    n
}

/// **C2: die DMA-Pools der Zuteilungen** — `(program_id, gewuenschte Seiten, gewaehrte Bytes, IOVA)`.
///
/// Vier Zahlen und nicht eine, weil die Aussage aus ihrem Verhaeltnis besteht: `gewaehrt ==
/// gewuenscht * 4096` (oder die Vorgabe bei `0`) trennt „so angefordert" von „stillschweigend
/// gekuerzt", und die IOVA daneben ist die Groesse, an der die Fenster zweier Treiber als disjunkt
/// nachweisbar sind. Eine Zeile, die nur „Pool vorhanden" meldete, waere auch bei einer halbierten
/// Region wahr — und eine halbierte DMA-Region ist ein Geraet, das ueber ihr Ende hinausschreibt.
pub fn driver_dma_pools(out: &mut [(u32, u32, u64, u64)]) -> usize {
    let g = DRIVER_ASSIGN.lock();
    let mut n = 0;
    for a in g.iter().filter(|a| a.used) {
        if n == out.len() {
            break;
        }
        out[n] = (a.program_id, a.dma_gewuenscht, a.dma_len, a.iova);
        n += 1;
    }
    n
}

/// Die Vorgabegroesse eines DMA-Pools -- fuer den Bericht, damit er sie nicht nachrechnet.
/// *Zuteiler und Pruefer brauchen EINE Quelle.*
pub fn driver_dma_default_bytes() -> u64 {
    DRIVER_DMA_BYTES
}

/// Lage der geteilten Uebertragungsflaeche — fuer den Bericht des Hochlaufs.
pub fn driver_shared_region(service_id: u32) -> Option<(u64, u64)> {
    let g = DRIVER_ASSIGN.lock();
    let a = *g
        .iter()
        .find(|a| a.used && a.shared_phys != 0 && a.program_id == service_id)?;
    Some((a.shared_phys, SHARED_BYTES))
}

/// **Dieselbe Zuteilung an eine Nachfolgefassung ausgeben** (A-5.1, Hot-Reload).
///
/// Es entstehen neue **Caps** auf dieselben Fenster und dieselbe DMA-Region — das Gerät wird
/// **nicht** losgelassen und nicht neu angehängt. Das ist der Unterschied zwischen einem
/// Austausch und einem Neustart: die Übersetzung im IOMMU-Kontext bleibt stehen, die IOVA bleibt
/// dieselbe, und in der Region steht noch, was die alte Fassung hinterlassen hat.
///
/// **Zur Überlappung:** zwischen dem Ausgeben und dem Abbau der alten Fassung halten kurzzeitig
/// **beide** Caps auf dasselbe Gerät. Das ist zulässig und nicht dasselbe wie „zwei Treiber":
/// die Stilllegung des Endpoints (A-4.2) sorgt dafür, dass in diesem Fenster **keiner** von
/// beiden eine Anfrage bekommt. Autorität zu halten und sie zu benutzen sind verschiedene Dinge —
/// und nur das zweite wäre hier ein Fehler.
pub fn reassign_driver_device(program_id: u32) -> Option<DriverGrant> {
    // **Genau diese Komponente**, nicht „die erste benutzte" (A-5.4). Ein Hot-Reload, der bei zwei
    // Treibern die falsche Zuteilung ausgibt, gaebe der Nachfolgefassung ein FREMDES Geraet -- und
    // zwar lautlos, denn alle Caps waeren gueltig.
    let a = *DRIVER_ASSIGN.lock().iter().find(|a| a.used && a.program_id == program_id)?;
    let dma = install_dma_cap_ex(
        a.dma_phys,
        a.dma_len,
        DmaDir::Bidirectional,
        DmaCoherence::NonCoherent,
        Rights::RW,
    )
    .ok()?;
    let cfg = match install_mmio_cap(a.cfg_page, 4096, Rights::RW) {
        Ok(c) => c,
        Err(_) => {
            let _ = cap_delete(dma);
            return None;
        }
    };
    let bar = match install_mmio_cap(a.bar, a.bar_len, Rights::RW) {
        Ok(c) => c,
        Err(_) => {
            let _ = cap_delete(cfg);
            let _ = cap_delete(dma);
            return None;
        }
    };
    let Some(shared) = a.shared_root.and_then(|r| cap_copy(r, Rights::RW).ok()) else {
        let _ = cap_delete(bar);
        let _ = cap_delete(cfg);
        let _ = cap_delete(dma);
        return None;
    };
    // **B2×N beim Hot-Reload: die Nachfolgefassung bekommt EIGENE Caps auf dieselben Vektoren.**
    //
    // Nicht die alten weitergereicht — die gehören dem Cspace der sterbenden PD und gehen mit ihr.
    // *Wer eine Fassung ersetzt, muss ihr ALLES geben, was die alte hatte*: fehlte Slot 7, bräche
    // die neue Fassung an derselben Stelle ab wie damals ohne Slot 6, und der Austausch meldete
    // `NotReady` — was nach einem Zeitproblem aussieht und ein fehlendes Cap ist.
    //
    // Die IRTEs und die MSI-X-Zeilen bleiben unangetastet: das Gerät wechselt nicht, nur sein
    // Bediener. Genau deshalb ist das hier eine Cap-Prägung und keine zweite Vergabe.
    let mut irq_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG] = [None; VEKTOREN_JE_ZUTEILUNG];
    let mut irq_ntfn_mehr: [Option<CapPtr>; VEKTOREN_JE_ZUTEILUNG] =
        [None; VEKTOREN_JE_ZUTEILUNG];
    let irq_anzahl = if a.msi_da { a.msi_anzahl as usize } else { 0 };
    // **Dieselben Notifications, neue Caps darauf** -- nicht neue Objekte. Die Bindung im Kernel
    // zeigt auf die alte Id; ein frisches Objekt hiesse, dass der Interrupt weiter an das Objekt
    // der gestorbenen Fassung zugestellt wird und die neue ewig wartet. Genau die Falle, die
    // A-5.1 mit der DMA-Region schon einmal hatte. Gilt ein Paar nicht, fallen alle bereits
    // geprägten mit — beide-oder-keins je Paar, alle Paare oder keins.
    let mut reassign_ok = true;
    for i in 0..irq_anzahl {
        let vektor = a.msi_basis + i as u8;
        let paar = install_irq_cap(vektor as u32, Rights::READ).ok().and_then(|ic| {
            match install_notification_cap(a.msi_ntfn[i], Rights::RWX) {
                Ok(nc) => Some((ic, nc)),
                Err(_) => {
                    let _ = cap_delete(ic);
                    None
                }
            }
        });
        match paar {
            Some((ic, nc)) => {
                irq_mehr[i] = Some(ic);
                irq_ntfn_mehr[i] = Some(nc);
            }
            None => {
                reassign_ok = false;
                break;
            }
        }
    }
    if !reassign_ok {
        for j in 0..irq_anzahl {
            if let Some(c) = irq_mehr[j] {
                let _ = cap_delete(c);
            }
            if let Some(c) = irq_ntfn_mehr[j] {
                let _ = cap_delete(c);
            }
        }
        let _ = cap_delete(shared);
        let _ = cap_delete(bar);
        let _ = cap_delete(cfg);
        let _ = cap_delete(dma);
        return None;
    }
    let (irq, irq_ntfn) = if a.msi_da {
        (irq_mehr[0], irq_ntfn_mehr[0])
    } else {
        (None, None)
    };
    Some(DriverGrant {
        cfg,
        bar,
        dma,
        shared,
        irq,
        irq_ntfn,
        irq_ntfn_id: a.msi_ntfn[0],
        irq_anzahl,
        irq_mehr,
        irq_ntfn_mehr,
        irq_ntfn_ids: a.msi_ntfn,
    })
}

/// Eine **weitere** HardwareLand-Backend-PD an einem **bestehenden** Kanal anlegen (A-5.1).
///
/// Der Kanal ist das, was den Austausch überlebt: Clients halten Caps auf **diesen** Endpoint.
/// Bekäme die neue Fassung einen eigenen, müsste jeder Client umgehängt werden — genau die
/// gerissene IPC-Beziehung, gegen die A-4.1 gebaut ist.
pub fn create_hardware_backend_on(
    partner: usize,
    backend_id: u16,
    ep: usize,
    ntfn: usize,
) -> Option<usize> {
    CAPS.write()
        .pds
        .create_hardware_backend(partner, backend_id, ep as u32, ntfn as u32)
}

/// Wurde das Gerät vergeben, und mit welcher Gerätesicht? Für den Bericht des Hochlaufs.
pub fn driver_assignment(program_id: u32) -> Option<(u64, u64)> {
    DRIVER_ASSIGN
        .lock()
        .iter()
        .find(|a| a.used && a.program_id == program_id)
        .map(|a| (a.dma_phys, a.iova))
}

/// **Die IOVA-Fenster aller Zuteilungen** (A-5.4): `(program_id, RID, IOVA, Laenge)` je Platz.
///
/// Grundlage der Isolationsaussage zwischen zwei Treibern. Zwei Geraete sind nur dann getrennt,
/// wenn ihre Gerätesichten sich nicht ueberschneiden -- und das ist eine Aussage ueber die
/// **IOVA**-Achse, nicht ueber die Physadressen.
pub fn driver_windows(out: &mut [(u32, u32, u64, u64)]) -> usize {
    let g = DRIVER_ASSIGN.lock();
    let mut n = 0;
    for a in g.iter().filter(|a| a.used) {
        if n == out.len() {
            break;
        }
        out[n] = (a.program_id, a.rid, a.iova, a.dma_len);
        n += 1;
    }
    n
}

/// Prüfen, ob `[addr, addr+len)` (IOVA) **vollständig in einer angehängten Region** des Kontexts
/// der `stream_id` liegt (Level-1-Software-Disziplin über mehrere Regionen / Scatter-Gather).
fn dma_addr_in_context(stream_id: u32, addr: Iova, len: u64) -> bool {
    let t = DMA_CTX.lock();
    t.iter()
        .find(|c| c.used && c.sids.contains(&stream_id))
        // **IOVA-Achse**: geprüft wird, was im Deskriptor stehen darf — also die Gerätesicht.
        .map(|c| {
            c.regs
                .iter()
                .any(|r| region_contains(r.iova.raw(), r.len, addr.raw(), len))
        })
        .unwrap_or(false)
}

/// Ein **Scatter-Gather-Segment** (ext-24): ein Teilbereich `[offset, offset+len)` innerhalb der
/// per `handle` referenzierten DMA-Region. Geräte-Adresse = `handle.iova + offset`. Das
/// gerätespezifische Deskriptorformat (virtio-desc, NVMe-PRP/SGL, NIC-Ring) baut das Backend
/// **aus** validierten Segmenten — die SG-Logik selbst bleibt geräteunabhängig.
#[derive(Clone, Copy)]
pub struct DmaSgEntry {
    pub handle: DmaHandle,
    pub offset: u64,
    pub len: u64,
}

/// **Scatter-Gather-Liste validieren** (ext-24, Level-1): jedes Segment muss (a) innerhalb seines
/// Handles liegen (`offset+len <= handle.len`) und (b) dessen IOVA-Bereich in einer angehängten
/// Region des `stream_id`-Kontexts liegen. Der vertrauenswürdige Treiber MUSS dies vor dem
/// Programmieren einer SG-Anfrage aufrufen; die SMMU ist der Hardware-Backstop. `false`, wenn ein
/// Segment ausserhalb liegt.
pub fn dma_sg_validate(stream_id: u32, entries: &[DmaSgEntry]) -> bool {
    entries.iter().all(|e| {
        e.len > 0
            && e.offset.saturating_add(e.len) <= e.handle.len
            && dma_addr_in_context(stream_id, e.handle.iova.offset(e.offset), e.len)
    })
}

// DmaPool (ext-24, ein Bump-Sub-Allokator über einen DmaHandle) wurde in der Konsolidierung K2
// entfernt: ein DMA-Sub-Puffer ist nur ein Teilbereich `[handle.iova + offset, +len)` einer
// angehängten Region; seine Disjunktheit/Bounds trägt bereits der kanonische Scatter-Gather-/
// Containment-Pfad ([`DmaSgEntry`] + [`dma_sg_validate`] -> [`region_contains`]). Ein Backend, das
// viele kleine Puffer schneidet, führt einen trivialen Offset-Cursor selbst — kein eigener
// öffentlicher Allokatortyp nötig (er duplizierte die Bump-Logik der Region-Runtime/SG).

/// **Kanonisches Region-Containment** (Konsolidierung K4): liegt `[addr, addr+len)` VOLLSTÄNDIG in
/// `[base, base+rlen)`? Die **eine** Grundlage aller DMA-Bounds-Prüfungen — Level-1-Software-
/// Disziplin ([`dma_addr_in_region`]), Multi-Region-Kontext ([`dma_addr_in_context`]) und
/// Scatter-Gather ([`dma_sg_validate`]) bauen alle darauf auf (keine duplizierte Formel mehr).
fn region_contains(base: u64, rlen: u64, addr: u64, len: u64) -> bool {
    len > 0 && addr >= base && addr.saturating_add(len) <= base.saturating_add(rlen)
}

/// **Level-1-Software-Disziplin** (ext-23): prüfen, dass ein Geräte-DMA-Zugriff `[addr, addr+len)`
/// VOLLSTÄNDIG in der DmaCap-Region `[base, base+rlen)` liegt. Der vertrauenswürdige Treiber MUSS
/// dies vor dem Programmieren JEDER Geräte-DMA-Adresse (Deskriptor/Register) aufrufen (backend-
/// direkt, kernel-auditiert). Hardware-unabhängig wirksam. Die SMMU (Level 2) ist der zusätzliche
/// **Hardware-Backstop**, falls ein kompromittiertes Backend diese Prüfung umgeht. Dünner Wrapper
/// um das kanonische [`region_contains`].
pub fn dma_addr_in_region(base: Iova, rlen: u64, addr: Iova, len: u64) -> bool {
    // **IOVA-Achse.** Geprüft wird eine Adresse, die in einen Deskriptor geschrieben werden soll
    // — also die Gerätesicht. Diese Signatur war bis ext-36 vollständig `u64` und hätte den
    // Achsenwechsel klaglos überlebt: sie hätte ab Schritt b gegen die falsche Achse geprüft,
    // ohne dass irgendetwas bricht. Genau deshalb steht hier jetzt der Typ.
    region_contains(base.raw(), rlen, addr.raw(), len)
}

/// Ergebnis der virtio-rng-DMA-Demo (ext-23, D4): echter Bus-Master-DMA in die DmaCap-Region +
/// **zweistufiger** Kronjuwel-Test (Level-1-Software-Bounds blockt Out-of-Window demonstrierbar;
/// Level-2-SMMU = Hardware-Backstop, unter QEMU für emulierte Geräte nicht beobachtbar).
#[derive(Clone, Copy, Default)]
pub struct VirtioDmaResult {
    pub found: bool,           // Gerät + virtio-Caps gefunden
    pub used_adv: bool,        // used-Ring fortgeschritten (Gerät hat geantwortet)
    pub written: u32,          // vom Gerät gemeldete Byte-Zahl
    pub rand0: u32,            // erste 4 zufällige Bytes (vom Gerät via DMA geschrieben)
    pub rand1: u32,            // nächste 4
    pub evtq_empty_good: bool, // SMMU-Event-Queue nach dem In-Window-DMA leer
    pub cj_sw_blocked: bool,   // Level 1: Software-Bounds wies den Out-of-Window-Deskriptor ab
    pub cj_sentinel_ok: bool,  // mit Level 1 aktiv: Out-of-Window-Ziel unverändert
    pub cj_unguarded_wrote: bool, // Sensitivität: OHNE die Prüfung schrieb das Gerät -> Prüfung lasttragend
    pub cj_smmu_enforced: bool, // Level 2: faultete die SMMU den ungeschützten Zugriff?
    pub cj_evt_translation: bool, // ... und zwar als F_TRANSLATION (nicht irgendein Fault)
    pub cj_evt_sid_ok: bool,      // ... von der erwarteten StreamID
    pub cj_evt_input_ok: bool,    // ... auf genau der absichtlich eingetragenen PA
    pub cj_axes_differ: bool,     // Positivkontrolle der Trennung: IOVA != PA in diesem Lauf
    /// Sensitivitätskontrolle: mit **Bypass-STE** (Durchsetzung aufgehoben) schreibt dasselbe
    /// Gerät dieselbe Adresse tatsächlich. Ohne diese Kontrolle wäre das Ausbleiben des
    /// Schreibzugriffs auch mit einem stillgelegten Gerät vereinbar.
    pub cj_bypass_wrote: bool,
    /// Die Event-Queue hat in **diesem Lauf** nachweislich gesprochen (ein echter
    /// `F_TRANSLATION`). Erst damit ist „Queue leer" oben überhaupt eine Aussage.
    pub evtq_liveness: bool,
    /// Keine Konfigurationsfehler der Einheit (`C_BAD_STE`/`C_BAD_CD`/…) über den ganzen Lauf.
    pub cfg_errors_zero: bool,
    pub audit_ok: bool,        // dma_audit==0 nach dem Aufräumen
}

#[cfg(target_arch = "aarch64")] // virtio-rng-PCI + SMMU: ARM-/QEMU-`virt`-spezifisch (ext-31)
// A-2.2: benutzt `testsupport` (liegt hinter `selftest`), und der einzige Aufrufer ist
// `threads::mod.rs` — das Modul gibt es ohne das Feature nicht. Ohne dieses Gate uebersetzt der
// `--no-default-features`-Bau auf aarch64 gar nicht.
#[cfg(feature = "selftest")]
/// **virtio-rng-DMA End-to-End** (ext-23, D4): das Gerät DMAt Zufallsbytes in die DmaCap-Region
/// (echter Bus-Master-DMA). Danach der **zweistufige Kronjuwel-Test**:
/// - **Level 1 (Software, demonstrierbar):** der vertrauenswürdige Treiber validiert jede
///   Deskriptor-Adresse via [`dma_addr_in_region`]; eine Out-of-Window-Adresse wird abgewiesen ->
///   das Gerät wird gar nicht erst programmiert -> das Ziel bleibt unverändert.
/// - **Sensitivität + Level 2 (Hardware):** wird die Prüfung umgangen und die Adresse doch
///   ausgegeben, muss etwas Beobachtbares passieren — entweder schreibt das Gerät (dann trug
///   allein die Software), oder die SMMU weist ab. Seit `iommu_platform=on` (der Treiber
///   verlangt `VIRTIO_F_ACCESS_PLATFORM`) übersetzt QEMU das Gerät tatsächlich durch die SMMU,
///   und der zweite Fall tritt ein: geprüft wird dann nicht *dass* ein Event kam, sondern dass es
///   ein `F_TRANSLATION` der erwarteten StreamID auf **genau der absichtlich eingetragenen PA**
///   ist. Weil seit ext-36 Schritt b IOVA != PA gilt, ist eine PA im Deskriptor für das Gerät
///   nicht auflösbar — der Negativtest prüft damit die Achsentrennung selbst.
pub fn virtio_rng_dma_demo() -> VirtioDmaResult {
    let mut r = VirtioDmaResult::default();
    let Some(dev) = virtio_device() else {
        return r;
    };
    r.found = true;
    let rid = dev.rid();
    // DMA-Region (Virtqueue + Datenpuffer) ausschneiden + an die Geräte-StreamID binden (Level 2).
    let Some(region) = alloc_dma_region(0x4000) else {
        return r;
    };
    let base = region.base;
    let len = region.len;
    // Was das Gerät sieht: die IOVA, die der Enforcer beim Anhängen aus dem Fenster des
    // Kontexts vergeben hat. Sie ist von `base` **verschieden** (Fensterbasis oberhalb des RAM)
    // — ab hier trägt die Demo die Trennung der beiden Achsen als Abnahme.
    let Some(dev_base) = dma_enable(rid, base, len) else {
        free_raw_region(base, len);
        return r;
    };
    hal::iommu::drain_faults(); // sauberer Ausgangsstand
    let dlen = hal::virtio::DATA_LEN_BYTES as u64;

    // 1. In-Window: Treiber validiert die Zieladresse (Level 1, ok) -> Gerät DMAt Zufallsbytes.
    let data = dev_base.offset(hal::virtio::DATA_OFFSET);
    if dma_addr_in_region(dev_base, len, data, dlen) {
        if let Some(rng) = hal::virtio::probe(&dev) {
            // SAFETY: kernel-/Trusted-seitiger virtio-Treiber; BAR ist global EL1-Device-gemappt,
            // die Virtqueue + der Datenpuffer liegen in der (identity-gemappten) DMA-Region.
            // Der virtio-Treiber braucht **beide** Achsen: die Virtqueue beschreibt er per CPU
            // (PA), ihre Adresse und die des Puffers programmiert er dem Gerät (IOVA).
            let (adv, wlen) = unsafe { rng.request(base, dev_base.raw(), data.raw()) };
            r.used_adv = adv;
            r.written = wlen;
            // Nachlesen tut die **CPU** -> PA-Achse.
            let (w0, w1) = testsupport::peek_dma_words(base + hal::virtio::DATA_OFFSET);
            r.rand0 = w0;
            r.rand1 = w1;
        }
    }
    r.evtq_empty_good = hal::iommu::faults_empty();

    // 2. Kronjuwel (Sentinel-Page AUSSERHALB der DmaCap-Region). MEM-Lock VOR dem `if let`
    // freigeben (sonst Deadlock über das `if let`-Temporary -> free_raw_region re-lockt MEM).
    let sentinel_page = mem_alloc(4096, 4096).map(|c| c.base());
    if let Some(sent) = sentinel_page {
        const SENTINEL: u64 = 0xA5A5_A5A5_5A5A_5A5A;
        // SAFETY: frische, identity-gemappte RAM-Page; exklusiv hier beschrieben/gelesen.
        unsafe { core::ptr::write_volatile(sent as *mut u64, SENTINEL) };
        hal::cpu::dsb_sy();

        // 2a. Level 1: der Treiber validiert die Out-of-Window-Adresse -> ABGEWIESEN, kein Notify.
        // Die Sentinel-Page liegt AUSSERHALB der Region. Geprüft wird, ob der Treiber diese
        // Adresse als Deskriptorziel abweist — also auf der Gerätesicht. Dass sie hier aus einer
        // PA gebildet wird, ist der Punkt des Tests: eine Adresse, die dem Gerät nie zugeteilt
        // wurde. (Ab Schritt b ist sie zusätzlich schon deshalb ungültig, weil sie gar nicht im
        // IOVA-Fenster liegt — dann prüft dieselbe Zeile zwei Dinge auf einmal.)
        r.cj_sw_blocked = !dma_addr_in_region(dev_base, len, Iova::new(sent), dlen);
        r.cj_axes_differ = dev_base.raw() != base; // Schritt b wirkt: IOVA != PA
        // SAFETY: nur Lesezugriff. Da nicht programmiert, muss das Ziel unverändert sein.
        let after_guard = unsafe { core::ptr::read_volatile(sent as *const u64) };
        r.cj_sentinel_ok = after_guard == SENTINEL;

        // 2b. Sensitivität: Prüfung umgehen + Out-of-Window doch ausgeben. Beweist, dass die
        // Software-Prüfung lasttragend ist; testet zugleich den SMMU-Backstop (QEMU: nicht
        // beobachtbar -> Gerät schreibt; reale HW: SMMU faultet).
        // Teil 1 des Negativtests: Event-Queue **vorher** leeren — sonst bestünde er an einem
        // Altbestand aus einem früheren Schritt.
        hal::iommu::drain_faults();
        if let Some(rng) = hal::virtio::probe(&dev) {
            // Die Virtqueue bleibt korrekt (CPU: PA, Gerät: IOVA) — **nur** die Zieladresse im
            // Deskriptor ist absichtlich eine **PA** statt einer IOVA. Genau das ist die
            // Verwechslung, die vor ext-36 Schritt b keinen Unterschied machte: seit die beiden
            // Achsen auseinanderliegen, ist `sent` im Fenster des Geräts nicht abgebildet.
            let _ = unsafe { rng.request(base, dev_base.raw(), sent) };
        }
        // SAFETY: s.o.
        let after_unguarded = unsafe { core::ptr::read_volatile(sent as *const u64) };
        r.cj_unguarded_wrote = after_unguarded != SENTINEL; // Prüfung war lasttragend
        // Teil 2: den Eintrag **prüfen**, nicht zählen. Ein Zähler bestünde auch an einem
        // beliebigen anderen Fault; belegt ist die Eigenschaft erst, wenn es ein
        // Übersetzungsfehler der erwarteten StreamID auf **genau der PA** ist, die oben
        // absichtlich als Gerätesicht eingetragen wurde.
        if let Some(ev) = hal::iommu::peek_fault() {
            r.cj_smmu_enforced = true;
            r.cj_evt_translation = ev.kind == hal::fault::FaultKind::Translation;
            r.cj_evt_sid_ok = ev.requester == rid;
            r.cj_evt_input_ok = ev.input_addr == sent;
            // **Queue-Liveness**: ein echter Übersetzungsfehler ist der Beleg, dass die Queue in
            // diesem Lauf überhaupt sprechen kann. Ohne ihn ruht jedes „keine Faults" auf
            // `CD.R` — einem Bit, das genau hier einmal gefehlt hat und das jemand später aus
            // Performancegründen wieder abschalten kann.
            if r.cj_evt_translation {
                EVTQ_LIVENESS.store(true, Ordering::Release);
                r.evtq_liveness = true;
            }
        }
        // Teil 3, Sensitivitätskontrolle: **ohne** Durchsetzung muss dasselbe Gerät dieselbe
        // Adresse wirklich schreiben. Die alte Kontrolle (`cj_unguarded_wrote`) ist eingeklappt,
        // seit die SMMU tatsächlich übersetzt: sie prüfte dann dieselbe Beobachtung wie die
        // Hauptaussage und konnte nicht mehr fehlschlagen, während diese besteht. Diese hier
        // kann es — sie ist rot, wenn das Gerät gar nicht mehr DMAt.
        //
        // Der Aufbau ist bewusst genau der eines „Passthrough-Enforcers für den Bringup":
        // Bypass-STE, Gerät läuft unübersetzt. Er steht sichtbar benannt im Test, damit er nicht
        // versehentlich in der Durchsetzung landet.
        hal::iommu::drain_faults();
        unsafe { core::ptr::write_volatile(sent as *mut u64, SENTINEL) };
        hal::cpu::dsb_sy();
        if DMA_ENFORCER.override_ste(rid, &hal::smmu::build_ste_bypass()) {
            if let Some(rng) = hal::virtio::probe(&dev) {
                // Unter Bypass gibt es **keine** Gerätesicht mehr: die Adressen des Geräts sind
                // physisch. Deshalb hier `base` in beiden Rollen — und genau das ist die
                // Konfiguration, in der die Achsentrennung wirkungslos ist.
                let _ = unsafe { rng.request(base, base, sent) };
            }
            // SAFETY: s.o.
            let after_bypass = unsafe { core::ptr::read_volatile(sent as *const u64) };
            r.cj_bypass_wrote = after_bypass != SENTINEL;
            DMA_ENFORCER.restore_ste(rid);
        }
        hal::iommu::drain_faults();
        free_raw_region(sent, 4096);
    }

    // 3. Aufräumen: Durchsetzung entziehen (STE invalidieren) + DMA-Region freigeben.
    dma_disable(rid, base, len);
    free_raw_region(base, len);
    r.audit_ok = dma_audit() == 0;
    r.cfg_errors_zero = hal::iommu::config_errors() == 0;
    r
}

/// Eine **nie angehängte** DMA-Region an den Allokator zurückgeben — für Fehler-/Cleanup-Pfade,
/// in denen eine `alloc_dma_region` gar nicht erst in eine Cap mündet.
///
/// Kein Token nötig und keiner möglich: die Region war nie in einer Übersetzungstabelle, also
/// gibt es nichts zu bezeugen. Deshalb der eigene Name — `free_dma_region` ist der
/// nachweispflichtige Pfad (ext-37).
#[cfg_attr(not(feature = "kernel-fuzz"), allow(dead_code))] // nur vom Fuzzer benutzt (ADR 0013)
pub fn free_unattached_dma_region(base: u64, len: u64) {
    free_raw_region(base, len);
}

// --- Prozess-Heap-Regionsquelle (ext-25) ---
//
// Der kernel-/Trusted-SAS-seitige `RegionSource`: bedient grow/shrink des prozess-lokalen
// Heaps direkt aus dem physischen Allokator (`MEM`). Ein EL0-Prozess würde dasselbe per Syscall
// marshallen — die `RegionSource`-Schnittstelle bleibt identisch (IOMMU-/Heap-neutral). Die
// Region trägt eine `MemoryCap` (lineares Eigentum); `release` gibt sie über `into_cap` zurück.
static REGION_ID: AtomicU32 = AtomicU32::new(1);

pub struct KernelRegionSource;

impl KernelRegionSource {
    /// Wie [`RegionSource::request`], aber mit Zonenwunsch (E-Rest 3b).
    ///
    /// Bewusst **nicht** in der `RegionSource`-Schnittstelle: die ist die Sicht des Heaps, und
    /// dem ist die Physadresse gleichgültig. Nur der Kernel weiss, dass eine DMA-Region in GiB 0
    /// liegen muss; die Bedingung gehört dorthin, wo sie begründbar ist.
    fn request_below(&self, min_len: usize, purpose: Purpose, limit: u64) -> Option<Region> {
        let len = (min_len as u64 + 4095) & !4095;
        let cap = mem_alloc_below(len, 4096, limit)?;
        let id = REGION_ID.fetch_add(1, Ordering::Relaxed);
        Some(Region::from_cap(cap, RegionTag::new(id, purpose)))
    }
}

impl RegionSource for KernelRegionSource {
    fn request(&self, min_len: usize, purpose: Purpose) -> Option<Region> {
        self.request_below(min_len, purpose, u64::MAX)
    }
    fn release(&self, region: Region) {
        MEM.lock().free(region.into_cap());
    }
}

// --- Hot-Reload-Zustand als Region (Konsolidierung O-B) ---
//
// Der zustandsbehaftete Hot-Reload-Test (Zähler-Service v1 -> v2) hielt seinen Zustand bisher in
// einer **roh** per `peek_u64`/`poke_u64` angesprochenen RAM-Adresse (`CS_STATE_BASE`). Er lebt nun
// in einer `Region` (`Purpose::HotReloadState`) und wird über die **sichere** RegionView-API
// gelesen/geschrieben — dasselbe Substrat wie der Prozess-Heap (ext-25), kein rohes `unsafe` im
// Testpfad. Beide Komponenten-Versionen (v1, v2) teilen DIESELBE Region (zero-copy; der Zustand
// überlebt den Tausch — genau die Hot-Reload-Invariante). Siehe ADR 0010.
static CS_STATE_REGION: SpinLock<Option<Region>> = SpinLock::new(None);

/// `program_id` des Zähler-Service (Hot-Reload-Testkomponente). Der Zustand gehört einem
/// **Programm**, nicht einer Instanz — genau deshalb überlebt er den Instanztausch.
pub const CS_PROGRAM_ID: u32 = 0xA43_0001;
/// Layout-Version der Zähler-Nutzlast: ein `u64` an Nutzlast-Offset 0.
///
/// Bewusst **getrennt** von der `iface_version` aus A-4.4: die eine beschreibt, was über den
/// Endpoint geht, die andere, was im Speicher liegt. v1 (+1) und v2 (+10) unterscheiden sich im
/// Verhalten, nicht im Zustandslayout — sie tragen dieselbe `STATE_VERSION`, und genau deshalb
/// darf v2 den Zähler von v1 fortsetzen.
pub const CS_STATE_VERSION: u32 = 1;
/// Nutzlastlänge des Zähler-Zustands (ein `u64`).
const CS_PAYLOAD_LEN: u32 = 8;

/// Die Hot-Reload-Zustandsregion anlegen: Kopf schreiben (A-4.3), Nutzlast nullen. Gibt die
/// Phys-Basis zurück (nur Telemetrie; der Zugriff läuft über [`hotreload_state_get`]/
/// [`hotreload_state_set`], die Übernahme über [`hotreload_state_attach`]).
pub fn hotreload_state_alloc() -> Option<u64> {
    let mut region = KernelRegionSource.request(4096, Purpose::HotReloadState)?;
    let phys = region.phys();
    // `init` nullt die Nutzlast selbst — `carve` liefert nicht garantiert genullten Speicher, und
    // ein Zustand aus Resten einer fremden Allokation ist von einem echten nicht zu unterscheiden.
    state::init(
        &mut region.view(),
        CS_PROGRAM_ID,
        CS_STATE_VERSION,
        CS_PAYLOAD_LEN,
    )
    .ok()?;
    *CS_STATE_REGION.lock() = Some(region);
    Some(phys)
}

/// Den Zustand **übernehmen** (A-4.3): der Kopf muss zu `program_id`/`state_version` passen, sonst
/// gibt es kein `Ok` — und damit keinen stillschweigend fehlinterpretierten Zustand. Erhöht bei
/// Erfolg den Übernahmezähler, macht die Übernahme also im Speicher sichtbar.
///
/// `NoState` heisst **Kaltstart**, nicht Fehler: der Aufrufer legt dann an, statt abzubrechen.
pub fn hotreload_state_attach(
    program_id: u32,
    state_version: u32,
) -> Result<state::Handover, state::StateError> {
    let mut guard = CS_STATE_REGION.lock();
    let Some(r) = guard.as_mut() else {
        return Err(state::StateError::NoState);
    };
    state::attach(&mut r.view(), program_id, state_version)
}

/// Die Übernahmezahl lesen, ohne zu übernehmen (Telemetrie/Selbsttest). `None`, wenn dort kein
/// gültiger Kopf steht.
pub fn hotreload_state_generation() -> Option<u32> {
    let mut guard = CS_STATE_REGION.lock();
    let r = guard.as_mut()?;
    state::generation(&r.view())
}

/// Das Zähler-Wort aus der **Nutzlast** der Hot-Reload-Region lesen (sichere RegionView-API).
/// Liegt hinter dem Kopf — ein Schreibfehler hier kann die Versionsangabe nicht überschreiben.
pub fn hotreload_state_get() -> u64 {
    CS_STATE_REGION
        .lock()
        .as_mut()
        .and_then(|r| state::payload(&mut r.view()).and_then(|p| p.get::<u64>(0)))
        .unwrap_or(0)
}

/// Das Zähler-Wort in die **Nutzlast** der Hot-Reload-Region schreiben (sichere RegionView-API).
pub fn hotreload_state_set(val: u64) {
    if let Some(r) = CS_STATE_REGION.lock().as_mut() {
        if let Some(mut p) = state::payload(&mut r.view()) {
            p.set::<u64>(0, val);
        }
    }
}

/// Eine roh-allozierte RAM-Region (ohne Cap) an den Allokator zurückgeben (interner Test-/
/// Setup-Helfer für temporäre DMA-/Sentinel-Regionen). 4-KiB-granular.
pub(crate) fn free_raw_region(base: u64, len: u64) {
    let len = (len + 4095) & !4095;
    MEM.lock().free_region(PhysRegion::new(base, len));
}

/// Eine DMA-Bindung **sicher abbauen** (Revoke-Reihenfolge, ext-23, DMA-use-after-free-sicher):
/// (1) `enforcer.detach` (hardwareseitige Durchsetzung entziehen — SMMU-Invalidierung),
/// dann (2) aus der Backend-VSpace unmappen. Erst danach darf der Aufrufer die DmaCap löschen
/// (`delete_leaf` -> `free_region`). Nach Schritt 1 kann kein Gerät mehr in die Region DMAen.
#[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
pub fn revoke_dma(binding: &DmaBinding, tid: ThreadId) {
    dma_enforcer().detach(binding);
    // VSpace-Unmap ist **CPU**-Sicht -> PA (die IOVA gehört der IOMMU, nicht der MMU).
    unmap_dma_from_thread(tid, binding.pa.raw(), binding.len);
}

/// **DMA-Policy-Oracle** (ext-23): `0` = konsistent, sonst Anomalie-Code:
/// - `1` = DmaCap-Bounds verletzt (nicht ausgerichtet/leer/außerhalb GiB-1-Fenster bzw.
///   überlappt das Kernel-Image — `floor` = Kernel-Image-Ende).
/// - `2` = zwei DmaCap-Regionen überlappen einander.
/// - `3` = der Enforcer meldet eine Durchsetzungs-Anomalie (`dma_enforcer().audit()`).
/// Wird in [`ipc_audit`] als Code `40 + dma_audit()` aggregiert.
#[inline(never)] // Audit 2026-09-10 (#DF-Klasse): dicke Pruef-Funktion aus heissen Rahmen
// heraushalten -- der Aufrufer (`ipc_audit`) laeuft auch auf heissem Pfad.
pub fn dma_audit() -> u32 {
    let floor = hal::mmu::kernel_end().max(hal::mmu::USER_RAM_MIN);
    let ceil = hal::mmu::GIB1_END;
    let code = CAPS.read().dma_bounds_audit(floor, ceil);
    if code != 0 {
        return code;
    }
    if dma_enforcer().audit() != 0 {
        return 3;
    }
    // Revoke-Ordnung-Invariante (docs/invariants.md §2, DMA-use-after-free-sicher): eine noch in
    // einem SMMU-Kontext gemappte Region darf NIE freigegeben sein. Wäre sie freigegeben (free
    // VOR detach), läge sie in der Free-Liste und überlappte sie -> Code 4. Funktioniert für
    // den cap-basierten (dma_attach) UND den rohen (dma_enable) Pfad. Snapshot von DMA_CTX ziehen
    // + Lock freigeben, DANN MEM prüfen (Rangordnung R1 vor R4, nie gleichzeitig gehalten).
    if !dma_ctx_regions_live() {
        return 4;
    }
    // Achsen-Wächter (ext-36): Der Vergleich oben läuft gegen die Freiliste des **physischen**
    // Allokators und ist nur dann aussagekräftig, wenn er physische Adressen sieht. Würde jemand
    // dort die IOVA einsetzen, überlappte ab Schritt b nie etwas — das Oracle bliebe bei jedem
    // Lauf grün, ohne noch irgendetwas zu prüfen. Deshalb hier explizit: jede erfasste Adresse
    // muss im RAM-Fenster liegen, das der Allokator überhaupt vergeben kann.
    if !dma_ctx_regions_are_physical() {
        return 5;
    }
    // Code 6: **Konfigurationsfehler der IOMMU**. `C_BAD_STE`/`C_BAD_CD` und Verwandte heißen,
    // dass die Einheit die Tabellen ablehnt und den Stream **gar nicht** übersetzt. Jede spätere
    // Aussage der Form „keine Faults beobachtet" wäre dann bedeutungslos — nicht weil nichts
    // passierte, sondern weil nichts passieren *konnte*. Genau dieser Zustand bestand hier
    // unbemerkt, solange das emulierte Gerät die SMMU ohnehin umging.
    #[cfg(target_arch = "aarch64")]
    if hal::iommu::config_errors() != 0 {
        return 6;
    }
    // Code 7: der **Pending-Zustand** ist eine eigene Invariante, kein Feld. Eine Region darin
    // ist weder frei noch übersetzt — genau die Konjunktion, die verletzt wäre, wenn die
    // Fädelung durch `Finalized` irgendwo abreißt: läge sie in der Freiliste, wäre sie trotz
    // unbestätigter Stilllegung neu vergebbar; stünde sie noch in einer Übersetzungstabelle,
    // wäre der zwingende Unmap ausgefallen.
    if !dma_pending_is_isolated() {
        return 7;
    }
    // Code 8: ein Teardown konnte die Faehigkeiten der Einheit nicht lesen und ist deshalb
    // **ausgefallen** (s. `VtdEnforcer::detach`). Eine Uebersetzung, die stehen bleibt, waehrend
    // ihre Region freigegeben wird, ist ein DMA-use-after-free -- und zwar einer, den ohne diesen
    // Zaehler niemand bemerkt haette.
    if DETACH_WITHOUT_CAPS.load(Ordering::Relaxed) != 0 {
        return 8;
    }
    0
}

/// Zaehler fuer Teardowns, die mangels lesbarer IOMMU-Faehigkeiten ausfielen (`dma_audit` Code 8).
static DETACH_WITHOUT_CAPS: AtomicU32 = AtomicU32::new(0);

/// Wächter für [`dma_audit`] Code `7`.
fn dma_pending_is_isolated() -> bool {
    let mut snap = [DmaRegion::EMPTY; MAX_PENDING_DMA];
    let mut n = 0;
    {
        let p = PENDING_DMA.lock();
        for r in p.iter() {
            if !r.is_empty() {
                snap[n] = *r;
                n += 1;
            }
        }
    }
    if n == 0 {
        return true;
    }
    // (a) nicht in irgendeiner Übersetzungstabelle geführt
    {
        let t = DMA_CTX.lock();
        for r in snap[..n].iter() {
            if t.iter().filter(|c| c.used).any(|c| {
                c.regs
                    .iter()
                    .any(|q| !q.is_empty() && q.pa == r.pa && q.len == r.len)
            }) {
                return false;
            }
        }
    }
    // (b) nicht in der Freiliste des Allokators
    let mem = MEM.lock();
    !snap[..n]
        .iter()
        .any(|r| mem.overlaps_free(r.pa.raw(), r.len))
}

/// Wächter für [`dma_audit`] Code `5`: liegen die in den Kontexten geführten **PA**-Werte im
/// RAM-Fenster des Allokators? Schlägt an, sobald die Achse vertauscht wurde (eine IOVA aus einem
/// Fenster außerhalb des RAM erfüllt die Bedingung nicht) — der Fall, in dem Code 4 stillschweigend
/// nichts mehr prüft.
fn dma_ctx_regions_are_physical() -> bool {
    let floor = hal::mmu::kernel_end().max(hal::mmu::USER_RAM_MIN);
    let ceil = hal::mmu::GIB1_END;
    let t = DMA_CTX.lock();
    t.iter().filter(|c| c.used).all(|c| {
        c.regs.iter().filter(|r| !r.is_empty()).all(|r| {
            let b = r.pa.raw();
            b >= floor && b + r.len <= ceil
        })
    })
}

/// Hilfsprüfung für [`dma_audit`] Code 4: keine aktuell in einem `DMA_CTX` gemappte Region
/// überlappt freies RAM (sonst wurde sie freigegeben, während die SMMU-Stage-1 noch darauf zeigte).
/// Snapshot der Kontext-Regionen (DMA_CTX kurz sperren, kopieren, freigeben), danach gegen
/// `MEM.overlaps_free` — die beiden Locks werden NIE gleichzeitig gehalten (R1 vor R4).
fn dma_ctx_regions_live() -> bool {
    let mut snap = [(0u64, 0u64); NDMA_CTX * MAX_CTX_REGS];
    let mut n = 0;
    {
        let t = DMA_CTX.lock();
        for c in t.iter() {
            if c.used {
                for r in c.regs.iter() {
                    if r.len != 0 {
                        // **PA-Achse**: verglichen wird gegen die Freiliste des physischen
                        // Allokators. Mit der IOVA wäre der Vergleich ab Schritt b sinnlos —
                        // sie liegt in einem Fenster, das der Allokator nie vergibt, also
                        // überlappte NIE etwas und das Oracle wäre still blind geworden.
                        snap[n] = (r.pa.raw(), r.len);
                        n += 1;
                    }
                }
            }
        }
    } // DMA_CTX freigegeben
    let mem = MEM.lock();
    !snap[..n].iter().any(|&(b, l)| mem.overlaps_free(b, l))
}

/// **Test-/Telemetrie-API** (Konsolidierung K5) — bewusst von der verifizierten Kernschnittstelle
/// getrennt. Diese Funktionen werden NUR vom Selbsttest/Bericht (`threads.rs`) gelesen; sie tragen
/// keine Sicherheitsinvariante und sind nicht Teil der zu verifizierenden DMA-Kern-API. `use
/// super::*` bringt die (für Kindmodule sichtbaren) privaten Statics/Imports von `system` in Scope.
#[cfg(feature = "selftest")]
pub(crate) mod testsupport {
    /// Kernel-Stack-Basis des Thread-Slots (0 = keiner). Fuer den Farbtest (todo A1): die
    /// Zusicherung „auch der Kernel-Stack der PD liegt in ihrem Farbsatz" muss nachpruefbar
    /// sein und nicht nur im Kommentar stehen.
    pub fn kstack_of(thread_slot: usize) -> u64 {
        super::KSTACKS.lock().base_of[thread_slot]
    }

    // **Die D15-Gegenprobe stand hier und ist wieder ausgebaut** (2026-08-13). Sie nullte den
    // Kernel-Stack eines LEBENDEN, noch geparkten EL0-Threads -- woertlich das, was der naechste
    // Anforderer der Region taete (`mem_alloc` -> `zero_phys`) -- und liess ihn dann zu.
    //
    // Ergebnis, zeichengleich mit dem D15-Protokoll:
    //   `el0-trap: User-Thread 0x7d100000020 faultete (EC=0x20 FAR=0x0000000000000000)`
    // Der Grund steht in `init_thread_frame`: dort ist `elr=entry` und `spsr=0` (EL0t). Genullt
    // ergibt das `elr=0, spsr=0` -> `eret` nach EL0 mit PC 0.
    //
    // Die zweite Haelfte des Bildes (`EC=0x21 ELR=0` im Kernel) kam aus der Gegenprobe im
    // Freigabepfad selbst; beide sind in `docs/befunde/d15/` beschrieben. Der Code bleibt draussen:
    // er reisst den Lauf absichtlich und hat in einem gruenen Kernel nichts zu suchen.

    /// Die beiden obersten Seitentabellen der VSpace `asid` (`(l1, l2)`), oder `(0, 0)`.
    pub fn vspace_tables_of(asid: u16) -> (u64, u64) {
        if asid == 0 || asid as usize > super::vspace_slots() {
            return (0, 0);
        }
        let e = super::VSPACES.lock()[asid as usize - 1];
        if e.used {
            (e.l1, e.l2)
        } else {
            (0, 0)
        }
    }

    /// ASID des Thread-Slots (0 = nicht isoliert).
    pub fn asid_of(thread_slot: usize) -> u16 {
        (super::vspace_of(thread_slot) >> 48) as u16
    }

    use super::*;

    /// Anzahl Regionen im Kontext der StreamID. `0`, wenn kein Kontext.
    pub fn dma_ctx_region_count(stream_id: u32) -> usize {
        let t = DMA_CTX.lock();
        t.iter()
            .find(|c| c.used && c.sids.contains(&stream_id))
            .map(|c| c.regs.iter().filter(|r| !r.is_empty()).count())
            .unwrap_or(0)
    }

    /// Anzahl StreamIDs im Kontext der StreamID (Stream-Gruppen-Telemetrie).
    pub fn dma_ctx_sid_count(stream_id: u32) -> usize {
        let t = DMA_CTX.lock();
        t.iter()
            .find(|c| c.used && c.sids.contains(&stream_id))
            .map(|c| c.sids.iter().filter(|&&s| s != u32::MAX).count())
            .unwrap_or(0)
    }

    /// Den Bump des Kontexts kurz vor das Fensterende schieben, damit der Erschöpfungsfall
    /// prüfbar wird, ohne zehntausende Zuteilungen zu fahren. Gibt `false`, wenn es den Kontext
    /// nicht gibt.
    pub fn dma_ctx_exhaust_window(stream_id: u32) -> bool {
        let mut t = DMA_CTX.lock();
        match t.iter_mut().find(|c| c.used && c.sids.contains(&stream_id)) {
            Some(c) => {
                c.iova_next = c.iova_limit - IOVA_GUARD;
                true
            }
            None => false,
        }
    }

    /// Pending-DMA-Regionen: `(insgesamt, davon ausserhalb eines KILL)`.
    pub fn dma_pending_stats() -> (u32, u32) {
        (
            PENDING_TOTAL.load(Ordering::Relaxed),
            PENDING_UNEXPECTED.load(Ordering::Relaxed),
        )
    }

    /// Wie viele Geräte DMA betreiben, ohne dass ihre Adressbreite deklariert wurde.
    pub fn dma_undeclared_devices() -> u32 {
        UNDECLARED_COUNT.load(Ordering::Relaxed)
    }

    /// Zähler der laut abgewiesenen IOVA-Zuteilungen: (Fenster voll, Eingangsbreite, Gerät).
    pub fn dma_iova_rejects() -> (u32, u32, u32) {
        (
            IOVA_REJECTS[0].load(Ordering::Relaxed),
            IOVA_REJECTS[1].load(Ordering::Relaxed),
            IOVA_REJECTS[2].load(Ordering::Relaxed),
        )
    }

    /// Hat die Event-Queue in diesem Lauf nachweislich gesprochen?
    pub fn evtq_liveness_proven() -> bool {
        EVTQ_LIVENESS.load(Ordering::Acquire)
    }

    /// Die Stage-1-Wurzel des Kontexts der StreamID (struktureller Leaf-Test). `0` = keiner.
    pub fn dma_ctx_stage1(stream_id: u32) -> u64 {
        let t = DMA_CTX.lock();
        t.iter()
            .find(|c| c.used && c.sids.contains(&stream_id))
            .map(|c| c.l1)
            .unwrap_or(0)
    }

    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    // --- SMMU-Diagnose-Accessors (ext-23, D2; SMMU-spezifisch, nur für den `smmu`-Test/Bericht) ---
    // ext-31: Die `smmu_*`-Accessors sind aarch64-only (auf x86 wären es VT-d/AMD-Vi-Register
    // — noch nicht portiert); die übrigen Helfer hier sind architekturunabhängig.
    pub fn smmu_present() -> bool {
        hal::smmu::present()
    }
    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    pub fn smmu_idr0() -> u32 {
        hal::smmu::idr0()
    }
    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    pub fn smmu_sid_bits() -> u32 {
        hal::smmu::sid_bits()
    }
    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    pub fn smmu_enabled() -> bool {
        hal::smmu::enabled()
    }
    #[cfg(target_arch = "aarch64")] // SMMU-spezifischer Enforcer-Accessor (ext-31)
    pub fn smmu_sync_ok() -> bool {
        DMA_ENFORCER.sync_ok()
    }
    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    pub fn smmu_eventq_empty() -> bool {
        hal::iommu::faults_empty()
    }
    #[cfg(target_arch = "aarch64")] // SMMU/virtio: ARM-spezifisch (ext-31)
    pub fn smmu_gerror() -> u32 {
        hal::smmu::gerror()
    }

    /// Wie [`super::dma_audit`], aber mit explizit gewähltem `floor` (Bounds-Sensitivitätstest:
    /// ein zu hoher `floor` muss eine legitime Region als Out-of-Window melden).
    pub fn dma_audit_with_floor(floor: u64) -> u32 {
        CAPS.read().dma_bounds_audit(floor, hal::mmu::GIB1_END)
    }

    /// Die ersten beiden 32-bit-Worte einer (RAM-)DMA-Region über die **globale Identity-Map**
    /// lesen (der Kernel sieht alles RAM EL1-RW). Für den Kohärenz-Check: sieht der Kernel dieselben
    /// Bytes, die das Backend über seine EL0-Non-Cacheable-Abbildung geschrieben hat?
    pub fn peek_dma_words(phys: u64) -> (u32, u32) {
        // SAFETY: `phys` ist eine kernel-ausgeschnittene RAM-DMA-Region, in der globalen SAS-Map
        // identity-gemappt und gültig; nur lesender Zugriff auf die ersten 8 Bytes.
        unsafe {
            let p = phys as *const u32;
            (
                core::ptr::read_volatile(p),
                core::ptr::read_volatile(p.add(1)),
            )
        }
    }
}

#[cfg(target_arch = "aarch64")] // PCIe-ECAM des QEMU-`virt`-Boards (ext-31)
mod pcie_arm {
    use super::*;
    // --- PCIe-Enumeration (ext-23, D1; kernel-/Trusted-Setup) ---

    /// Das DMA-Beweisgerät (`virtio-rng-pci`) per ECAM finden + einrichten: ECAM **global** als
    /// EL1-Device mappen (jenseits der statischen GiB 0..8), Bus 0 nach Vendor `0x1af4` scannen,
    /// BARs dimensionieren+zuweisen, Memory-Space + **Bus-Master** aktivieren. Gibt das Gerät
    /// (inkl. RID = SMMU-StreamID) zurück. Reines kernel-/Trusted-Setup — kein User-Pfad.
    pub fn pcie_find_virtio() -> Option<hal::pcie::PciDevice> {
        // Kein Subjekt beteiligt: der Kernel bildet sich selbst das ECAM-Fenster ein, um zu
        // enumerieren. Der Grund steht trotzdem hin -- er unterscheidet diese Stelle von einer,
        // an der eine PD etwas zu sehen bekaeme.
        let _ = crate::addr::Va::for_kernel_global_window(
            KernelGlobalWindowWitness(()),
            crate::addr::Pa::new((hal::pcie::ecam_gib() as u64) << 30),
        );
        hal::mmu::map_device_block_global(hal::pcie::ecam_gib());
        // Gezielt die virtio-RNG (nicht eine evtl. vorhandene Default-NIC, ebenfalls Vendor 0x1af4).
        let d = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_RNG_DEVICES);
        *VIRTIO_PCI.lock() = d; // für D4 (virtio-Treiber) cachen
        d
    }

    /// Das in [`pcie_find_virtio`] gefundene + eingerichtete virtio-RNG-Gerät (gecacht).
    static VIRTIO_PCI: SpinLock<Option<hal::pcie::PciDevice>> = SpinLock::new(None);
    pub fn virtio_device() -> Option<hal::pcie::PciDevice> {
        *VIRTIO_PCI.lock()
    }

    // dma_audit_with_floor + peek_dma_words liegen in `mod testsupport`
    // (Konsolidierung K5: Bounds-Sensitivitätstest + Kohärenz-Peek von der Kernschnittstelle getrennt).


}
#[cfg(target_arch = "aarch64")]
pub use pcie_arm::*;

// --- MCS Scheduling Contexts (Budget-basiertes Scheduling) ---
//
// CPU-Zeit wird **kapabilitätskontrolliert** vergeben: ein Scheduling-Context-Objekt
// (Budget/Periode) existiert nur als Cap im CapSpace. Erst die Vorlage einer gültigen
// SchedContext-Cap mit WRITE-Recht autorisiert, einem Thread dieses Budget zuzuweisen
// (`bind_sched_context`). Das Budget wird aus dem **Cap-Objekt** gelesen, nicht aus
// einem freien Argument — die Cap *ist* die Autorität (vgl. seL4 `SchedContext_Bind`).

/// Eine **Scheduling-Context-Capability** prägen: `budget` Ticks je `period` Ticks.
pub fn install_sched_context_cap(
    budget: u32,
    period: u32,
    rights: Rights,
) -> Result<CapPtr, CapError> {
    CAPS.write()
        .cspace
        .install_sched_context(budget, period, rights)
}

/// Einen Scheduling Context an einen Thread **binden**: die Cap `sc` auflösen, prüfen
/// dass sie ein `SchedContext` mit WRITE-Recht ist, und das darin gespeicherte Budget
/// auf dem Scheduler von `core` für `tid` setzen. Ohne gültige Cap keine Budget-
/// Autorität (gibt `false` zurück). Lock-Ordnung: CAPS vor SCHEDS[core].
pub fn bind_sched_context(sc: CapPtr, core: usize, tid: ThreadId) -> bool {
    let (budget, period) = {
        let caps = CAPS.read();
        match caps.cspace.lookup(sc) {
            Some((ObjectKind::SchedContext { budget, period }, rights, _))
                if rights.contains(Rights::WRITE) =>
            {
                (budget, period)
            }
            _ => return false, // keine (gültige) SchedContext-Cap -> keine Autorität
        }
    }; // CAPS vor SCHEDS freigegeben
    SCHEDS[core].lock().set_budget(tid, budget, period)
}

/// MCS-Telemetrie eines Kerns: `(Budget-Erschöpfungen, Refills)`. Für Tests.
pub fn budget_stats(core: usize) -> (u64, u64) {
    SCHEDS[core].lock().budget_stats()
}

/// Einen **nicht laufenden** Thread des **aktuellen** Kerns direkt beenden — dieselbe
/// Mechanik wie der cap-kontrollierte `KILL`-Syscall (`KernelSched::kill`), nur ohne
/// Cap-Vorlage (für kernelinterne Test-/Wartungspfade). Gibt `true` bei Erfolg. `kill`
/// ist kern-lokal: `tid` muss auf dem aufrufenden Kern liegen und darf nicht laufen.
pub fn kill_local(tid: ThreadId) -> bool {
    let core = hal::cpu::core_id();
    if caprock_sched::owner_core(tid) != Some(core) {
        return false;
    }
    let ok = SCHEDS[core].lock().kill(tid, core);
    if ok {
        purge_ipc_queues(tid); // eager: tote IPC-Queue-Einträge vermeiden
        reclaim_user_kstack(tid, "kill_local"); // falls EL0-Thread: Pool-Slot zurück
    }
    ok
}

/// **C7b: die EL0-Region eines sterbenden Threads messen und austragen.**
///
/// Wird aus [`reap_core`] gerufen, **vor** der Rückgabe an `MEM`. Misst nur, wenn die Buchführung
/// für `slot` auf genau diese Region zeigt — der Zombie-Ring führt Kernel-Thread-Stacks und
/// EL0-Regionen in derselben Liste, und ihre Unterscheidung darf nicht am Inhalt hängen.
///
/// Der Eintrag wird in **jedem** Fall geleert, sobald er auf diesen Slot passt: die `gid` geht
/// gleich zurück in die Freiliste, und ein stehengebliebener Eintrag zeigte danach auf die Region
/// eines fremden Threads — genau die Form, vor der der C4-Eintrag bei den Wartelisten warnt.
fn userstack_beim_tod_messen(slot: usize, base: u64, len: u64) {
    let treffer = {
        let mut p = KSTACKS.lock();
        if slot >= p.ubase_of.len() {
            false
        } else if p.ubase_of[slot] == base && p.ulen_of[slot] == len && base != 0 && len != 0 {
            p.ubase_of[slot] = 0;
            p.ulen_of[slot] = 0;
            true
        } else {
            // Kein Treffer: entweder ein Kernel-Thread-Stack (dann steht dort ohnehin nichts),
            // oder eine gefaerbt geladene PD, deren Stackstuecke der PD gehoeren. Der Eintrag
            // dieses Slots wird trotzdem geraeumt, wenn es einen gibt -- die `gid` wird gleich
            // neu vergeben.
            if slot < p.ubase_of.len() {
                p.ubase_of[slot] = 0;
                p.ulen_of[slot] = 0;
            }
            false
        }
    };
    if treffer {
        // SAFETY: der Zombie ist eingesammelt; die Region gehoert bis zum `free_region` des
        // Aufrufers niemandem sonst.
        unsafe {
            crate::userstackmark::messen(
                base as usize,
                len as usize,
                crate::userstackmark::Anlass::Tod(slot),
            )
        };
    }
}

/// Beendete Threads **dieses Kerns** einsammeln: TCB-Slots freigeben und Stacks an
/// den Allokator zurückgeben (aus dem Idle-Thread). Erst die Zombies unter
/// `SCHEDS[core]` einsammeln, dann den Lock **freigeben** und unter `MEM` freigeben
/// — nie SCHEDS und MEM gleichzeitig halten (sonst Ordnungsinversion zu `spawn`).
pub fn reap() -> usize {
    reap_core(hal::cpu::core_id())
}

/// Beendete Threads des Kerns `core` einsammeln — auch **kern-übergreifend** aufrufbar
/// (z. B. der Fuzzer-Controller auf core 0 reapt Zombies belasteter Kerne, deren Idle
/// nie läuft -> sonst lecken die Stacks). Sperrordnung: erst `SCHEDS[core]` (Zombies in
/// einen Puffer), freigeben, dann `MEM` — nie beide gleichzeitig.
pub fn reap_core(core: usize) -> usize {
    // (base, len, gid) je Zombie: Stack-Region an MEM, `gid` an die Thread-Freiliste —
    // beides erst NACH dem Freigeben von SCHEDS (Sperrordnung).
    let mut zombies: [(usize, usize, u32); 8] = [(0, 0, 0); 8];
    let mut n = 0;
    {
        let mut sched = SCHEDS[core].lock();
        while n < zombies.len() {
            match sched.reap() {
                Some(z) => {
                    zombies[n] = z;
                    n += 1;
                }
                None => break,
            }
        }
    } // SCHEDS freigegeben
    if n > 0 {
        // C4: **vor** der MEM-Sperre, nicht darin — eine Messschleife unter dem innersten Lock
        // waere derselbe Fehler wie ein `println!` dort. In dieser Liste liegen Kernel-Thread-
        // Stacks und EL0-**User**-Stacks nebeneinander, beide `STACK_SIZE` gross; unterscheidbar
        // sind sie nur am Muster, das nur die Kernel-Stacks tragen. Eine Region ohne Muster kostet
        // dabei genau EINEN Lesezugriff.
        for &(base, len, gid) in &zombies[..n] {
            // SAFETY: der Zombie ist eingesammelt, die Region gehoert bis zum `free_region` unten
            // niemandem sonst.
            unsafe { crate::kstackmark::messen_wenn_gefuellt(crate::kstackmark::KL_KERN, base, len) };
            // **C7b: und wenn es eine EL0-USER-Region ist, ihr Wasserstand -- HIER und nicht
            // spaeter.** Nach dem `free_region` unten gehoert sie dem naechsten Anforderer; beim
            // Tod des Threads steht sein tiefster Pfad noch darin.
            //
            // Erkannt wird sie NICHT am Inhalt (eine genullte Region ist von einem restlos
            // aufgebrauchten Kernel-Stack nicht zu unterscheiden), sondern an der Buchfuehrung:
            // `gid` IST der globale Thread-Slot, und der Eintrag muss auf genau diese Region
            // zeigen. Stimmt er nicht ueberein, wird NICHT gemessen -- lieber eine Messung
            // weniger als eine ueber fremdem Speicher.
            userstack_beim_tod_messen(gid as usize, base as u64, len as u64);
        }
        {
            let mut mem = MEM.lock();
            let mut bytes = 0u64;
            for &(base, len, _) in &zombies[..n] {
                if len > 0 {
                    mem.free_region(PhysRegion::new(base as u64, len as u64));
                    bytes += len as u64;
                }
            }
            REAPED_BYTES.fetch_add(bytes, Ordering::Relaxed);
        } // MEM freigegeben
        // Thread-Slots (`gid`) zurückgeben — erst jetzt kann eine `gid` neu vergeben werden.
        for &(_, _, gid) in &zombies[..n] {
            caprock_sched::release_gid(gid);
        }
    }
    n
}

/// Einen **nicht laufenden** Thread auf **irgendeinem** Kern beenden (für den Fuzzer-
/// Controller, der Aktoren auf anderen Kernen zu ungünstigen Zeiten killt). Schlägt
/// fehl (`false`), wenn der Thread gerade auf seinem Kern *läuft* (dann erneut
/// versuchen, sobald er blockiert/verdrängt ist). Entfernt ihn eager aus allen
/// IPC-Queues und weckt den Zielkern (IPI), damit er bald reapt.
pub fn kill_remote(tid: ThreadId) -> bool {
    let ok = with_owner(tid, |s, c| s.kill(tid, c).then_some(())).is_some();
    if ok {
        purge_ipc_queues(tid);
        reclaim_user_kstack(tid, "kill_remote");
        // Der Zielkern soll bald reapen; er kann inzwischen ein anderer sein — der IPI geht
        // an den Kern, auf dem der Kill tatsächlich stattfand.
        if let Some(c) = caprock_sched::owner_core(tid) {
            kick(c);
        }
    }
    ok
}

/// Summe der per [`reap`] an den Allokator zurückgegebenen Stack-Bytes (monoton) —
/// belegt, dass beendete Threads ihren Stack zurückgeben (robust gegen anderweitige
/// Allokationen, anders als ein absoluter `total_free`-Vergleich).
pub fn reaped_bytes() -> u64 {
    REAPED_BYTES.load(Ordering::Relaxed)
}

pub fn create_pd() -> Option<usize> {
    CAPS.write().pds.create()
}
/// Eine PD in einer bestimmten **Sicherheitsdomäne** anlegen (Domäne danach unveränderlich).
pub fn create_pd_in_domain(domain: Domain) -> Option<usize> {
    CAPS.write().pds.create_in_domain(domain)
}
/// **Eine PD mit eigenem Cap-Budget anlegen** (2026-08-26). `budget == 0` = Vorgabe.
///
/// Siehe [`caprock_microkit::PdTable::create_mit_budget`] fuer die zwei getrennten Absagen. Der
/// Weg, auf dem eine Zahl von aussen hierher kommt, ist `SYS_LOAD` (`MSG3`).
pub fn create_pd_mit_budget(domain: Domain, budget: u16) -> Option<usize> {
    CAPS.write().pds.create_mit_budget(domain, budget)
}
/// `(freier Vorrat, am Vorrat gescheiterte Erzeugungen)` — s. `capbudget` im Bericht.
pub fn pd_budget_bilanz() -> (usize, u64) {
    CAPS.read().pds.budget_bilanz()
}
/// Das Cap-Budget dieser PD (0 = die PD gibt es nicht).
pub fn pd_budget_of(pd: usize) -> usize {
    CAPS.read().pds.budget_of(pd)
}
/// Die (unveränderliche) Domäne einer PD.
pub fn pd_domain(pd: usize) -> Option<Domain> {
    CAPS.read().pds.domain_of(pd)
}
/// **Einen Dienstkanal ohne Geraet praegen** (2026-08-25) -- Endpoint + Notification, sonst nichts.
///
/// Das Gegenstueck zu [`create_hardware_backend`] fuer eine PD, die einen Dienst anbietet und
/// **kein Geraet** hat. Es entsteht keine PD und keine Partner-Bindung: die PD kommt aus dem
/// gewoehnlichen Ladepfad, und `cap_allowed` schraenkt Nicht-HardwareLand nicht auf einen Kanal
/// ein -- der Riegel dort ist ausdruecklich `domain == Domain::HardwareLand`.
///
/// **Die Ruecknahme ist dieselbe wie nebenan**: schlaegt die Notification fehl, wird der frisch
/// reservierte Endpoint-Slot zurueckgegeben. Ein halb gepraegter Kanal waere ein Objekt, das
/// niemand mehr findet und niemand mehr freigibt.
pub fn create_service_channel() -> Option<(usize, usize)> {
    let ep = create_endpoint()?;
    let Some(ntfn) = create_notification() else {
        *eps()[ep].lock() = Endpoint::EMPTY;
        return None;
    };
    Some((ep, ntfn))
}

/// Ein **HardwareLand-Backend** anlegen (ext-22, P3): erzeugt einen dedizierten Endpoint +
/// eine Notification und eine HardwareLand-PD mit **unveränderlicher** Partner-Bindung an
/// `trusted_pd` (TrustedSas) und genau diesem Kanal. Gibt `(backend_pd, ep, ntfn)` zurück;
/// der Aufrufer prägt + installiert die Kanal-Caps (Backend: recv/signal — policy-geprüft auf
/// genau diesen Kanal; Trusted-Partner: send/wait). 1:N (mehrere Backends je Trusted) erlaubt.
pub fn create_hardware_backend(
    trusted_pd: usize,
    backend_id: u16,
) -> Option<(usize, usize, usize)> {
    let ep = create_endpoint()?;
    let Some(ntfn) = create_notification() else {
        // `ep` ist frisch reserviert + noch unreferenziert (keine Cap, kein Waiter) -> Slot
        // zuruecknehmen, sonst leckt der Endpoint-Slot.
        *eps()[ep].lock() = Endpoint::EMPTY;
        return None;
    };
    let backend_pd = match CAPS.write().pds.create_hardware_backend(
        trusted_pd,
        backend_id,
        ep as u32,
        ntfn as u32,
    ) {
        Some(pd) => pd,
        None => {
            // ep + ntfn sind noch unreferenziert -> beide Slots zuruecknehmen.
            *eps()[ep].lock() = Endpoint::EMPTY;
            *ntfns()[ntfn].lock() = Notification::EMPTY;
            return None;
        }
    };
    Some((backend_pd, ep, ntfn))
}
/// Der **Partner** einer HardwareLand-Backend-PD (die TrustedSas-PD, an die sie gebunden ist).
pub fn pd_partner(pd: usize) -> Option<usize> {
    CAPS.read().pds.partner_of(pd)
}
// ==============================================================================================
// ZULASSUNG -- die zweite Haelfte jedes `spawn` (D0, 2026-08-07)
// ==============================================================================================
//
// Bis zum 2026-08-07 machte `spawn` den Thread in einem Zug lauffaehig. Wer ihm danach noch etwas
// geben musste -- eine PD, Caps, eine VSpace -- kam grundsaetzlich zu spaet; die Frage war nur, ob
// der Scheduler in der Luecke zuschlug. Gemessen: 9-mal in 50 000 Laeufen (D0).
//
// Seither gilt: **`spawn_*_parked` erzeugt, `admit_*` laesst zu.** Die uebliche Fassung `spawn_*`
// ist beides hintereinander und bleibt fuer jeden Thread richtig, dem keine Autoritaet
// nachgereicht wird. Wer eine PD bindet, nimmt die geparkte Fassung -- sonst zaehlt
// `LATE_PD_BIND` mit und die Berichtszeile `pdbind` faellt durch.
//
// **Warum nicht ein `must_use`-Token statt zweier Namen.** Es waere schaerfer: ein `Parked(tid)`,
// das nur `admit` konsumieren kann, machte das Vergessen zum Typfehler. Es waere aber auch ein
// Umbau aller 93 Spawn-Stellen, und der Zaehler leistet dasselbe ueber eine MESSUNG statt ueber
// eine Portierung -- er sieht auch die Stellen, die es morgen erst gibt. Als Verschaerfung in
// `todo.md` D0 vermerkt.

/// **Ein erzeugter, aber noch nicht zugelassener Thread** (D0, verschaerft 2026-08-07).
///
/// # Warum ein Typ und nicht eine `ThreadId` mit Disziplin
///
/// Die erste Fassung der D0-Behebung trennte `spawn_parked` von `admit` und zaehlte mit einem
/// Waechter (`pdbind`), ob eine PD zu spaet gebunden wurde. Das schliesst die Luecke **nicht**:
///
/// * `spaet == 0` zaehlt *spaete Bindungen*, nicht *ausbleibende Zulassungen*. Eine 62.
///   Aufrufstelle, die `spawn_parked` ruft und `admit` vergisst, ist an dem Zaehler nicht zu sehen
///   -- der Thread laeuft nie, und der Waechter schweigt.
/// * Die vier Stellen, an denen Autoritaet NACH der Zulassung vergeben wurde (`page_probe`,
///   `killer`, `producer`, `consumer`), fand ein Gegenlesen. Auffindbarkeit ist nicht
///   Unmoeglichkeit, und der Befund selbst hatte sie nicht genannt.
///
/// Dieser Typ schliesst beide Klassen mit derselben Konstruktion: **es gibt keinen oeffentlichen
/// Weg an die `ThreadId`.** Wer sie braucht, muss [`admit`] rufen, und das verbraucht den Zeugen.
/// Alles, was VOR der Zulassung geschehen muss -- PD binden, Caps setzen, Seiten mappen -- laeuft
/// ueber eine Funktion, die `&Parked` nimmt. Danach existiert kein Griff mehr, ueber den Autoritaet
/// nachgereicht werden koennte.
///
/// **Kein `Drop`-Impl**, und das ist Absicht: mit einem `Drop` liesse sich das Feld in [`admit`]
/// nicht mehr herausbewegen, und der Typ verlore genau die Eigenschaft, um die es geht. Der Preis
/// ist, dass ein *fallengelassener* `Parked` kein Uebersetzungsfehler ist -- `#[must_use]` macht
/// ihn zur Warnung, und der Thread laeuft dann nie, was laut ist. Was der Typ deckt, ist der
/// gefaehrlichere Fall: eine `ThreadId`, die vor der Zulassung in die Welt entkommt.
#[must_use = "ein Parked, der nicht durch `admit`/`admit_in_pd` geht, laeuft NIE -- genau die \
              Luecke, die der pdbind-Zaehler nicht sehen kann"]
pub struct Parked(ThreadId);

impl Parked {
    /// **Modulintern.** Es gibt bewusst kein `pub fn tid()`: der einzige Weg an die `ThreadId` ist
    /// [`admit`], und der verbraucht den Zeugen.
    fn tid(&self) -> ThreadId {
        self.0
    }
}

/// Einen geparkten Thread zulassen. Gibt seine `ThreadId` -- oder `None`, wenn sie nicht (mehr)
/// auflösbar ist (dann ist der Thread ohnehin weg).
pub fn admit(p: Parked) -> Option<ThreadId> {
    let tid = p.0;
    caprock_sched::owner_core(tid)
        .and_then(|c| SCHEDS[c].lock().admit(tid).then_some(tid))
}

/// **Erst binden, dann zulassen** -- die Reihenfolge, um die es bei D0 geht.
pub fn admit_in_pd(pd: usize, p: Parked) -> Option<ThreadId> {
    bind_pd(pd, p.tid());
    admit(p)
}

/// Eine PD an einen **geparkten** Thread binden, ohne ihn zuzulassen.
///
/// Fuer die Stellen, an denen zwischen Bindung und Zulassung noch etwas geschehen muss -- Caps in
/// die PD setzen, Seiten mappen. Wer das nach der Zulassung taete, haette dasselbe Rennen eine
/// Ebene tiefer: fuer den Thread sind ein leerer Cspace und ein Cspace ohne die eine gebrauchte
/// Cap dasselbe.
pub fn bind_pd_parked(p: &Parked, pd: usize) {
    bind_pd(pd, p.tid());
}

/// Eine **Region** in den Adressraum eines geparkten Threads mappen (s. `map_region_into_thread`).
///
/// Diese Variante hat der Typ gefunden, nicht das Gegenlesen: drei Backends (RTC, IRQ, DMA)
/// mappten ihre Geraeteseite **nach** der Zulassung. Der Kommentar an einer der Stellen sagt sogar,
/// was dann passiert -- „ohne dieses Mapping faultet das Backend beim RTC-Read". Mein Scan suchte
/// nach `map_into_thread` und `install_pd_cap`; `map_region_into_thread` kam darin nicht vor.
/// Ein Audit, der nur die gefundene Form sucht, findet nur sie wieder.
#[cfg(feature = "selftest")]
pub fn map_region_into_parked(p: &Parked, base: u64, len: u64, kind: MappingKind) -> bool {
    map_region_into_thread(p.tid(), base, len, kind)
}

/// Eine Seite in den Adressraum eines **geparkten** Threads mappen (s. [`map_into_thread`]).
#[cfg(feature = "selftest")]
pub fn map_into_parked(p: &Parked, base: u64, len: u64, perm: u8) -> bool {
    map_into_thread(p.tid(), base, len, perm)
}

/// Die `ThreadId` eines geparkten Threads **nur zur Buchfuehrung** (Telemetrie, Ablagen).
///
/// Ausdruecklich nicht `pub`: ein oeffentlicher Ausgang waere die Hintertuer, gegen die der ganze
/// Typ gebaut ist. Innerhalb von `system` ist er noetig, weil der Ladepfad die `tid` fuer
/// `record_user_kstack`/`set_vspace_of` braucht, bevor der Thread laufen darf.
fn parked_tid(p: &Parked) -> ThreadId {
    p.tid()
}

pub fn spawn_on_core(core: usize, entry: usize, arg: usize, prio: u8) -> Option<ThreadId> {
    admit(spawn_on_core_parked(core, entry, arg, prio)?)
}

/// Geparkte Fassung von [`spawn`] -- auf dem aufrufenden Kern, noch nicht lauffaehig.
pub fn spawn_parked(entry: usize, arg: usize, prio: u8) -> Option<Parked> {
    spawn_on_core_parked(hal::cpu::core_id(), entry, arg, prio)
}

/// Geparkte Fassung von [`spawn_balanced`].
pub fn spawn_balanced_parked(entry: usize, arg: usize, prio: u8) -> Option<Parked> {
    spawn_on_core_parked(least_loaded_core(), entry, arg, prio)
}

pub fn spawn_user(entry: usize, arg: usize, prio: u8) -> Option<ThreadId> {
    admit(spawn_user_parked(entry, arg, prio)?)
}

pub fn spawn_isolated(entry: usize, arg: usize, prio: u8) -> Option<(ThreadId, u64)> {
    let (p, r) = spawn_isolated_parked(entry, arg, prio)?;
    admit(p).map(|t| (t, r))
}

pub fn spawn_isolated_colored_auto(entry: usize, arg: usize, prio: u8) -> Option<(ThreadId, u64)> {
    let (p, r) = spawn_isolated_colored_auto_parked(entry, arg, prio)?;
    admit(p).map(|t| (t, r))
}

pub fn spawn_isolated_native(code: *const u8, code_len: usize, prio: u8) -> Option<ThreadId> {
    admit(spawn_isolated_native_parked(code, code_len, prio)?)
}

/// **Die Gruende, aus denen eine PD ABSICHTLICH spaet gebunden wird.**
///
/// Es gibt sie, und sie sind nicht alle ein Fehler: Z4 Stufe 2 braucht ein Subjekt mit einem
/// UMFANG, und der Umfang (eine Speicher- und eine SchedContext-Cap) entsteht erst am Ende von
/// `spawn_demo` -- nach den Farbtests, weil ein Test, der Speicher belegt, baseline-empfindliche
/// Tests kippt. Der Worker laeuft da laengst.
///
/// **Warum ein Aufzaehlungstyp und keine Ausnahme im Zaehler.** Ein `if tid == WORKER_TID0` waere
/// unsichtbar und wuechse mit jedem naechsten Sonderfall. Ein Grund mit Namen steht im Diff, im
/// Bericht und in [`ERLAUBTE_SPAETBINDUNGEN`] -- dieselbe Form wie `IdentityReason`/`IDENTITY_DEBTS`.
/// Wer eine Variante hinzufuegt, tut das sichtbar; wer `bind_pd` benutzt, wird gezaehlt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpaetbindungsGrund {
    /// Z4 Stufe 2: das Checkpoint-Subjekt bekommt seinen Umfang erst am Ende des Aufbaus.
    /// Unkritisch, weil der Worker keine Cap BENUTZT -- er zaehlt. Die PD ist Gegenstand des
    /// Checkpoints, nicht Werkzeug des Threads.
    CheckpointSubjektNachtraeglich,
}

impl SpaetbindungsGrund {
    pub const fn name(self) -> &'static str {
        match self {
            SpaetbindungsGrund::CheckpointSubjektNachtraeglich => "CheckpointSubjektNachtraeglich",
        }
    }
}

/// Die erklaerten Spaetbindungen -- Ratsche. Waechst die Liste, faellt es im Diff auf.
pub const ERLAUBTE_SPAETBINDUNGEN: [&str; 1] = ["CheckpointSubjektNachtraeglich"];

/// Bitmaske der Gruende, die in diesem Lauf tatsaechlich gefeuert haben (Bit = Variantenindex).
pub static SPAETBINDUNG_GESEHEN: AtomicUsize = AtomicUsize::new(0);

/// **Alle** Bindungen, rechtzeitige wie spaete, erklaerte wie unerklaerte -- die Sprechprobe.
///
/// Ein eigener Zaehler, und der erste Entwurf hatte hier den falschen. Er fragte
/// `SPAETBINDUNG_GESEHEN != 0`, also: „hat die erklaerte AUSNAHME gefeuert?" Das ist zweimal
/// verkehrt. Auf aarch64 gibt es diese Ausnahme gar nicht (sie sitzt im x86-Checkpoint-Aufbau) --
/// die Zeile waere dort durchgefallen, obwohl alles stimmt. Und auf x86 haette sie angefangen
/// durchzufallen, sobald jemand die Ausnahme beseitigt, also **als Antwort auf eine Verbesserung**.
///
/// Eine Sprechprobe gehoert an den GEPRUEFTEN PFAD, nicht an einen Sonderfall darin: gefragt ist
/// „wurde in diesem Lauf ueberhaupt eine PD gebunden?", denn nur dann kann `LATE_PD_BIND` etwas
/// gesehen haben. Derselbe Fehler wie eine Veraltungsmeldung, die an die Beobachtung statt an das
/// Register gekoppelt ist (s. D8).
pub static PD_BIND_GESAMT: AtomicUsize = AtomicUsize::new(0);

/// Eine **erklaerte** Spaetbindung. Zaehlt nicht in [`LATE_PD_BIND`], erscheint aber im Bericht.
pub fn bind_pd_late(pd: usize, tid: ThreadId, grund: SpaetbindungsGrund) {
    let bit = match grund {
        SpaetbindungsGrund::CheckpointSubjektNachtraeglich => 1usize,
    };
    SPAETBINDUNG_GESEHEN.fetch_or(bit, Ordering::Relaxed);
    PD_BIND_GESAMT.fetch_add(1, Ordering::Relaxed);
    CAPS.write().pds.bind_thread(pd, tid);
}

/// Zaehlt die Bindungen, die **zu spaet** kamen: der Thread war schon zugelassen (D0).
///
/// Das ist kein Diagnosekomfort, sondern die Abnahmebedingung. Es gibt im Kernel ueber 50 Stellen
/// mit dem Muster `spawn(); bind_pd()`, und die Frage „welche davon sind das Rennen?" ist per Grep
/// nicht zu beantworten -- ein `spawn` auf einem anderen Kern, unter IRQ-Maske oder mit einer
/// Prioritaet, die nie verdraengt, sieht im Text genauso aus. Ein Zaehler beantwortet sie.
///
/// **Er ist NICHT dasselbe wie „D0 ist aufgetreten".** Er zaehlt die *Gelegenheit*, nicht den
/// Treffer -- genau deshalb taugt er: bei 0,018 % Trefferquote braeuchte man 5556 Laeufe fuer eine
/// Beobachtung, aber die Gelegenheit tritt in JEDEM Lauf auf. Ein Melder, der nur beim Unglueck
/// spricht, ist bei dieser Rate stumm.
pub static LATE_PD_BIND: AtomicUsize = AtomicUsize::new(0);

/// Wie oft dabei die Gelegenheit an einem Thread bestand, den dieser Kern gar nicht kennt
/// (`is_admitted` gibt `None`). Getrennt gezaehlt, weil „nicht auflösbar" kein „rechtzeitig" ist.
pub static LATE_PD_BIND_UNKLAR: AtomicUsize = AtomicUsize::new(0);

/// Einen **PD-Slot** freigeben, ohne den vollen Teardown (der Aufrufer haelt keine Caps darin).
///
/// Fuer Messpfade, die eine PD anlegen und den Slot bei einem Fehlschlag sofort zurueckgeben --
/// ihn liegen zu lassen waere ein Leck im Messwerkzeug und wuerde die naechste Runde der
/// Messung verfaelschen.
pub fn free_pd_slot(pd: usize) -> bool {
    CAPS.write().pds.free(pd)
}

/// Die PD eines Threads (Z22 P2) — **derselbe Weg wie im Syscall-Dispatch**, nicht nachgerechnet.
pub fn pd_of_thread(tid: ThreadId) -> Option<usize> {
    CAPS.read().pds.pd_of_thread(tid)
}

/// **Wie viele Cap-Slots diese PD belegt** (K1b) — die Groesse, an der „vier Stapel aus EINER Cap"
/// von „vier Caps" zu unterscheiden ist.
///
/// Ohne sie waere die Aussage der Teilregion unbelegbar: vier laufende Threads beweisen nur, dass
/// vier Threads laufen. Der Punkt ist, was sie **gekostet** haben.
pub fn pd_cap_count(pd: usize) -> usize {
    CAPS.read().pds.cap_count(pd)
}

/// Wie viele Threads an dieser PD haengen (Z22 P2). Die Groesse, an der „mehrere Threads je PD"
/// von „einer, wie immer" zu unterscheiden ist.
pub fn pd_thread_count(pd: usize) -> u32 {
    CAPS.read().pds.thread_count(pd)
}

/// Der Hoechststand `Threads je PD` ueber alle PDs (Auskunft fuer den Bericht).
pub fn pd_threads_max() -> u32 {
    CAPS.read().pds.max_threads_per_pd()
}

/// **Die O(n)-Bilanz der PD-Tabelle** (C4): `(pd_of-Aufrufe, pd_of-Scan-Iterationen,
/// create-Scan-Iterationen, Bindungen ohne Rueckwaerts-Tabelle)`.
///
/// Gezaehlt statt gestoppt -- eine Iterationszahl ist eine Eigenschaft des Programms, eine
/// Zeitmessung nicht (D10). Die Aussage ist das PAAR aus Aufrufen und Iterationen: eine Null
/// bei den Iterationen allein waere von „nie gefragt" nicht zu unterscheiden.
pub fn pd_scan_bilanz() -> (u64, u64, u64, u64) {
    (
        caprock_microkit::PD_OF_CALLS.load(Ordering::Relaxed),
        caprock_microkit::PD_OF_SCAN_ITER.load(Ordering::Relaxed),
        caprock_microkit::PD_CREATE_SCAN_ITER.load(Ordering::Relaxed),
        caprock_microkit::PD_OWNER_UNANGEHAENGT.load(Ordering::Relaxed),
    )
}

/// Diagnose zum Rueckfallpfad: `(Aufrufe im Scan, erster betroffener Thread-Slot + 1,
/// Laenge der Rueckwaerts-Tabelle)`. Ein Rueckfall, den niemand erklaeren kann, ist ein offener
/// Posten und kein Messwert -- deshalb steht hier, WELCHER Slot es war und ob die Tabelle
/// ueberhaupt haengt.
pub fn pd_scan_diagnose() -> (u64, u64, usize) {
    (
        caprock_microkit::PD_OF_SCAN_CALLS.load(Ordering::Relaxed),
        caprock_microkit::PD_OF_SCAN_ERSTER_SLOT.load(Ordering::Relaxed),
        CAPS.read().pds.owner_len(),
    )
}

pub fn bind_pd(pd: usize, tid: ThreadId) {
    // **Vor** der Bindung fragen: danach saehe man nur noch das Ergebnis, nicht die Reihenfolge.
    //
    // **Auf dem Kern des THREADS, nicht dem des Aufrufers.** `is_admitted` loest die `tid` gegen
    // `self.core` auf und gaebe sonst fuer jeden `spawn_on_core(1, ..)`-Thread `None` -- der
    // Melder waere genau dort blind, wo das Rennen am ehesten trifft. Das Ergebnis wird in eine
    // Bindung gelegt, damit die SCHEDS-Sperre vor `CAPS.write()` sicher wieder faellt.
    let bereits = caprock_sched::owner_core(tid).and_then(|c| SCHEDS[c].lock().is_admitted(tid));
    match bereits {
        Some(true) => {
            LATE_PD_BIND.fetch_add(1, Ordering::Relaxed);
        }
        Some(false) => {}
        None => {
            LATE_PD_BIND_UNKLAR.fetch_add(1, Ordering::Relaxed);
        }
    }
    PD_BIND_GESAMT.fetch_add(1, Ordering::Relaxed);
    CAPS.write().pds.bind_thread(pd, tid);
}
/// Cap **policy-geprüft** in eine PD eintragen (Hardware-Caps nur HardwareLand, `PdControl`
/// nur TrustedSas). Gibt `false` zurück, wenn die Domänen-Policy es verbietet (kein Eintrag).
pub fn install_pd_cap(pd: usize, slot: usize, cap: CapPtr) -> bool {
    CAPS.write().install_cap_checked(pd, slot, cap)
}
pub fn clear_pd_cap(pd: usize, slot: usize) {
    CAPS.write().pds.clear_cap(pd, slot);
}

/// **Die Objektarten, die im Cspace einer PD stehen** — die Eingabe für
/// [`caprock_cap::checkpoint::classify_all`] (Z4b/Z4 Stufe 2).
///
/// Gibt die Zahl der betrachteten Slots zurück; `out[i]` ist `None`, wo der Slot leer ist. Der
/// **Platz** bleibt erhalten und wird nicht weggelassen: der Grund einer Verweigerung nennt den
/// Slot, und ein verdichteter Vektor verschöbe ihn.
///
/// Warum die Rechte hier nicht mitkommen: die Klassifikation entscheidet über die **Art** der
/// Autorität, nicht über ihren Umfang. Eine MMIO-Cap mit Leserecht ist auf der Zielmaschine
/// genauso wenig dieselbe wie eine mit Schreibrecht.
pub fn pd_object_kinds(pd: usize, out: &mut [Option<ObjectKind>]) -> usize {
    let g = CAPS.read();
    let caps = g.pds.caps_of(pd);
    let n = caps.len().min(out.len());
    for (i, slot) in caps.iter().take(n).enumerate() {
        out[i] = slot.and_then(|c| g.cspace.lookup(c)).map(|(k, _, _)| k);
    }
    n
}

/// Einen blockierten Endpoint-Empfänger zurückziehen (Hot-Reload).
pub fn endpoint_retire_receiver(ep: usize, tid: ThreadId) -> bool {
    if ep < eps().len() {
        eps()[ep].lock().retire_receiver(tid)
    } else {
        false
    }
}

/// **Reply-Liveness beim Quiescen** (Hot-Reload/Revocation OHNE Thread-Tod): wird der
/// Server `tid` als Reply-Owner von Endpoint `ep` zurückgezogen (z. B. seine Recv-Cap
/// entzogen / durch v2 ersetzt), während ein `caller` noch auf die Antwort wartet, wird
/// dieser Caller mit `ERR_SERVER_GONE` entblockt — sonst hinge er, weil der (lebende,
/// aber capless/ersetzte) Server nie mehr antwortet. Gibt `true`, falls ein Caller
/// entblockt wurde. Sperrordnung EPS vor SCHEDS (in `unblock_with_error`).
pub fn endpoint_quiesce_owner(ep: usize, tid: ThreadId) -> bool {
    if ep >= eps().len() {
        return false;
    }
    let orphan = {
        let mut e = eps()[ep].lock();
        e.owner_died(tid)
    }; // EPS freigegeben, bevor SCHEDS gesperrt wird
    if let Some(caller) = orphan {
        unblock_with_error(caller, caprock_abi::result::ERR_SERVER_GONE);
        true
    } else {
        false
    }
}

/// **Reply-Cap-Server-Migration beim Hot-Reload:** überträgt eine ausstehende
/// Antwortpflicht des Servers `tid` (das Reload-Opfer) auf die nächste RECV-Instanz
/// desselben Endpoints. Der wartende Aufrufer wird NICHT abgebrochen, sondern wieder
/// als Sender eingereiht; die neue Server-Instanz (v2) übernimmt dieselbe Nachricht
/// und schließt den Call ab. Gibt `true`, falls migriert wurde. Sperrt NUR EPS[ep] —
/// es wird niemand entblockt (kein SCHEDS-Lock, keine Sperrordnungsfrage).
pub fn endpoint_migrate_owner(ep: usize, tid: ThreadId) -> bool {
    if ep >= eps().len() {
        return false;
    }
    eps()[ep].lock().migrate_owner(tid)
}

// -- A-4.2: der ruhende Punkt ----------------------------------------------------------
//
// Ein Austausch der Server-Instanz braucht einen Zeitpunkt, an dem feststeht, was offen ist.
// Ohne Stilllegung gibt es den nicht: zwischen der Frage „ist etwas offen?" und der Antwort
// kann auf einem anderen Kern ein CALL eintreffen, und die Antwort ist falsch, bevor sie
// gelesen wird. `endpoint_begin_quiesce` schliesst neue Transaktionen aus; erst danach ist
// `endpoint_quiescence_of`/`endpoint_is_idle` eine Aussage über mehr als den Moment.
//
// Der Begriff ist bewusst weiter gefasst als der Hot-Reload-Fall: `thread_quiescence`
// beantwortet dieselbe Frage systemweit und trägt damit später Z4a (Thread einfrieren).

/// **Endpoint stilllegen** (A-4.2): ab jetzt scheitern `CALL`/`RECV` mit `ERR_QUIESCING`,
/// `REPLY` bleibt erlaubt — die Menge der offenen Transaktionen kann nur noch schrumpfen.
/// Gibt `true`, falls der Endpoint dadurch **neu** stillgelegt wurde; `false`, wenn bereits
/// ein anderer Austausch läuft (dann darf der Aufrufer ihn nicht für seinen halten) oder der
/// Endpoint nicht belegt ist. Sperrt nur EPS[ep].
pub fn endpoint_begin_quiesce(ep: usize) -> bool {
    if ep >= eps().len() {
        return false;
    }
    eps()[ep].lock().begin_quiesce()
}

/// **Stilllegung beenden** (Austausch fertig oder abgebrochen). Gibt `true`, falls der
/// Endpoint stillgelegt war. Auf **jedem** Ausgang des Austauschs aufzurufen, auch dem
/// fehlgeschlagenen: ein Endpoint, der stillgelegt zurückbleibt, weist von da an jeden
/// `CALL` ab — ein Totalausfall, der aussieht wie ein laufender Austausch.
pub fn endpoint_end_quiesce(ep: usize) -> bool {
    if ep >= eps().len() {
        return false;
    }
    eps()[ep].lock().end_quiesce()
}

/// Ist dieser Endpoint gerade stillgelegt?
pub fn endpoint_is_quiescing(ep: usize) -> bool {
    ep < eps().len() && eps()[ep].lock().is_quiescing()
}

/// **Die Torentscheidung eines Endpoints, wie `call`/`recv` sie faellen** — `None` = zulassen.
///
/// Bewusst dieselbe Funktion und keine Nachbildung: eine Pruefzeile, die die Bedingung
/// nachrechnet, prueft eine zweite Wirklichkeit (`iova_window_clear_of_msi` hat das einmal
/// gekostet). A-4.2 hat `gate_new_transaction` genau dafuer als reine Funktion herausgezogen.
pub fn endpoint_gate(ep: usize) -> Option<u64> {
    if ep >= eps().len() {
        return Some(caprock_abi::result::ERR_BADCAP);
    }
    eps()[ep].lock().gate_new_transaction()
}

/// Wie viele Empfaenger warten an diesem Endpoint? (Z23/S3: der Schnitt zieht sie zurueck, das
/// Auftauen reiht sie wieder ein — beides ist an dieser Zahl ablesbar.)
pub fn endpoint_receivers(ep: usize) -> usize {
    if ep >= eps().len() {
        return 0;
    }
    eps()[ep].lock().receiver_count()
}

/// Eine globale Cap in den Cspace einer PD legen (Pruefpfade und Aufbau).
pub fn pd_install_cap(pd: usize, slot: usize, cap: CapPtr) {
    CAPS.write().pds.install_cap(pd, slot, cap);
}

/// Einen Cap-Slot einer PD leeren (Pruefpfade: die **Positivkontrolle** einer Absage — dieselbe PD
/// ohne die Cap muss durchgehen, sonst belegt die Absage nur, dass irgendetwas nicht ging).
pub fn pd_clear_cap(pd: usize, slot: usize) {
    CAPS.write().pds.clear_cap(pd, slot);
}

/// **Ruht der Endpoint als Ganzes?** Keine wartenden Sender/Empfänger, kein offenes
/// Reply-Token — die Bedingung, unter der ein Austausch niemanden trifft.
pub fn endpoint_is_idle(ep: usize) -> bool {
    ep >= eps().len() || eps()[ep].lock().is_idle()
}

/// **Ruhepunkt-Befund für einen Thread an einem Endpoint** — aufgeschlüsselt nach den vier
/// Rollen (siehe [`Quiescence`]).
pub fn endpoint_quiescence_of(ep: usize, tid: ThreadId) -> Quiescence {
    if ep >= eps().len() {
        return Quiescence::default();
    }
    eps()[ep].lock().quiescence_of(tid)
}

/// **Systemweiter Ruhepunkt-Befund für einen Thread** (A-4.2, und derselbe Begriff für Z4a):
/// steht `tid` in **irgendeiner** IPC-Struktur — Endpoint-Queues, Reply-Token, Reply-Pflicht
/// oder als Notification-Waiter? Die Rollen werden über alle Objekte verschmolzen, die
/// Aufschlüsselung bleibt erhalten.
///
/// **Grenze, und sie ist wichtig:** diese Auskunft ist eine Momentaufnahme, solange nicht
/// jeder betroffene Endpoint stillgelegt ist. Die Objekte werden nacheinander gesperrt (ein
/// Lock über alle wäre eine Sperrordnungs-Verletzung und bei 10064 Objekten ohnehin
/// untragbar), also kann sich ein früh gelesenes Objekt ändern, während ein spätes gelesen
/// wird. Für „ruht der Thread wirklich?" ist das nur mit vorheriger Stilllegung eine Antwort;
/// ohne sie ist es Telemetrie. Wer das verwechselt, friert einen laufenden Thread ein.
pub fn thread_quiescence(tid: ThreadId) -> Quiescence {
    let mut q = Quiescence::default();
    for i in 0..eps().len() {
        q = q.merge(eps()[i].lock().quiescence_of(tid));
    }
    for i in 0..ntfns().len() {
        q = q.merge(ntfns()[i].lock().quiescence_of(tid));
    }
    q
}

// ------------------------------------------------------------------------------------------------
// Z4d stage 1: the observed edges of a cut
// ------------------------------------------------------------------------------------------------

/// How many edges a survey can report.
///
/// **The overflow is named** ([`cut_edges`] returns `Err(needed)`) and the list is never shortened.
/// A truncated edge list is not a smaller finding — it is *a participant left behind*, which is
/// literally the thing the survey exists to detect. D11 verbatim, at the place where the silent
/// version would be most expensive.
pub const CUT_EDGES_MAX: usize = 96;

/// Ein `Quiescence`-Befund traegt bis zu vier Rollen -- aufgeloest in einzelne Kanten, damit eine
/// Absage die ROLLE nennen kann und nicht nur den Thread.
fn quiescence_to_edges(
    channel: Channel,
    tid: ThreadId,
    q: Quiescence,
    out: &mut [Edge],
    n: &mut usize,
    need: &mut usize,
) {
    for (steht, role) in [
        (q.as_sender, EdgeRole::Sender),
        (q.as_receiver, EdgeRole::Receiver),
        (q.as_caller, EdgeRole::Caller),
        (q.as_reply_owner, EdgeRole::ReplyOwner),
    ] {
        if !steht {
            continue;
        }
        *need += 1;
        if *n < out.len() {
            out[*n] = Edge {
                channel,
                thread: tid.to_raw(),
                role,
            };
            *n += 1;
        }
    }
}

fn edge_role_of(r: IpcRole) -> EdgeRole {
    match r {
        IpcRole::Sender => EdgeRole::Sender,
        IpcRole::Receiver => EdgeRole::Receiver,
        IpcRole::Caller => EdgeRole::Caller,
        IpcRole::ReplyOwner => EdgeRole::ReplyOwner,
    }
}

/// **Every IPC role that touches this cut** (Z4d stage 1) — the measurement that
/// [`caprock_cap::checkpoint::classify_cut`] judges.
///
/// ## It takes the SCOPE, not three lists
///
/// Survey and verdict read **one** source. The first version took the threads, the endpoints and
/// the notifications as separate arguments, and a caller could survey one set and judge another:
/// list a peer in the scope but not in the survey, and the rule passes on a measurement that never
/// looked at him — a clean verdict over an incomplete finding. That is the `iova_window_clear_of_msi`
/// shape ("Zuteiler und Pruefer brauchen EINE Quelle"), and the topology it hid behind held only by
/// accident of the probe's own setup. With the `Scope` itself as the argument the mismatch is not
/// expressible.
///
/// ## Two directions, two questions — and only one of them had an answer
///
/// A relationship straddles the cut in two ways, and each is found differently:
///
/// | direction | question | asked via |
/// |---|---|---|
/// | the participant migrates, his channel stays behind | *where does **this thread** hold a role?* | `quiescence_of` over every channel |
/// | the channel migrates, this participant stays behind | *who holds a role **here**?* | `occupants` on the channels in scope |
///
/// The second question is the one that had no answer until 2026-08-25: `quiescence_of` needs a
/// thread you can already name, and the dangerous participant is by construction one the checkpoint
/// never listed. Asking only the first is how `Scope::endpoints` came to be believed instead of
/// checked.
///
/// ## The lock discipline is part of the contract
///
/// One channel, one `lock()`, released before the next — never a lock across the loop, and no CDT
/// walk inside it. `SpinLock` masks interrupts, and a survey that held them across 10064 objects
/// would be the `pd_haelt_dma` finding of 2026-08-21 again: the longest masked stretch there rose
/// to 3 701 562 cycles and turned the **debugger's** stop-latency line red — a line with no
/// connection to the thing that broke it.
///
/// ## What this is NOT
///
/// A snapshot, unless the channels are quiesced — the same limit as [`thread_quiescence`] and the
/// same reason. It does not weaken the gate: the checkpoint path freezes its subject first, so the
/// subject's own edges cannot move under the survey. A **foreign** thread may still enter a
/// relationship while we look; then the cut is refused on the next attempt rather than this one.
/// Refusing late is safe, admitting wrongly is not.
pub fn cut_edges(
    subject: ThreadId,
    scope: &Scope<'_>,
    out: &mut [Edge],
) -> Result<usize, usize> {
    let mut n = 0usize;
    let mut need = 0usize;
    // Auf zwei volle Warteschlangen plus das Reply-Paar bemessen -- mehr Rollen kann EIN Endpoint
    // strukturell nicht tragen.
    let mut lokal = [(ThreadId::from_raw(0), IpcRole::Sender); caprock_ipc::QUEUE_CAP * 2 + 2];

    // -- Pass 1: die Kanaele IM Umfang, vollstaendig aufgezaehlt --------------------------------
    for (&id, ist_ep) in scope
        .endpoints
        .iter()
        .map(|id| (id, true))
        .chain(scope.notifications.iter().map(|id| (id, false)))
    {
        let i = id as usize;
        let k = if ist_ep {
            if i >= eps().len() {
                continue;
            }
            eps()[i].lock().occupants(&mut lokal)
        } else {
            if i >= ntfns().len() {
                continue;
            }
            ntfns()[i].lock().occupants(&mut lokal)
        };
        // Mehr Rollen, als der Zwischenpuffer fasst: strukturell unmoeglich -- und deshalb eine
        // benannte Absage und kein Weiterlaufen. Faellt die Annahme, soll sie AUFFALLEN.
        let k = match k {
            Ok(k) => k,
            Err(gebraucht) => return Err(need + gebraucht),
        };
        let channel = if ist_ep {
            Channel::Endpoint(id)
        } else {
            Channel::Notification(id)
        };
        for &(t, r) in &lokal[..k] {
            need += 1;
            if n < out.len() {
                out[n] = Edge {
                    channel,
                    thread: t.to_raw(),
                    role: edge_role_of(r),
                };
                n += 1;
            }
        }
    }

    // -- Pass 2: die Kanaele AUSSERHALB, aber nur die Rollen der wandernden Threads --------------
    //
    // Hier wird gefragt statt aufgezaehlt: ein fremder Kanal interessiert nur, wenn einer der
    // Wandernden dort steht. Das haelt den Durchgang ueber alle Objekte bei EINER Sperrung je
    // Objekt und einer Handvoll Abfragen darin.
    //
    // **Das Subjekt kommt zusaetzlich zu `scope.threads`** -- es wandert per Definition mit,
    // woertlich dieselbe Regel wie in `classify_cut`. Doppelt genannt schadet nicht: eine Kante
    // zweimal zu erheben aendert am Urteil nichts, eine gar nicht zu erheben sehr wohl.
    let wandernde = |f: &mut dyn FnMut(ThreadId)| {
        f(subject);
        for &raw in scope.threads {
            let t = ThreadId::from_raw(raw);
            if t != subject {
                f(t);
            }
        }
    };
    for i in 0..eps().len() {
        let id = i as u32;
        if scope.endpoints.contains(&id) {
            continue; // in Pass 1 vollstaendig erledigt
        }
        let ep = eps()[i].lock();
        wandernde(&mut |t| {
            let q = ep.quiescence_of(t);
            if !q.is_quiescent() {
                quiescence_to_edges(Channel::Endpoint(id), t, q, out, &mut n, &mut need);
            }
        });
    }
    for i in 0..ntfns().len() {
        let id = i as u32;
        if scope.notifications.contains(&id) {
            continue;
        }
        let nt = ntfns()[i].lock();
        wandernde(&mut |t| {
            let q = nt.quiescence_of(t);
            if !q.is_quiescent() {
                quiescence_to_edges(Channel::Notification(id), t, q, out, &mut n, &mut need);
            }
        });
    }

    if need > out.len() {
        return Err(need);
    }
    Ok(n)
}

// ================================================================================================
// Z6b: der Debugger
// ================================================================================================
//
// **Die Zusage, die dieser Abschnitt traegt:** Debug-Autoritaet ist eine Capability ueber genau
// EINE PD — delegierbar, widerrufbar, pruefbar. Eine PD, ueber die nie eine gepraegt wurde, kann
// nicht debuggt werden, und das ist eine Eigenschaft des Systems, keine Zusage.
//
// Was `ptrace` daran nicht kann: es ist PID-gebundene, allgegenwaertige Autoritaet, gegen eine UID
// geprueft; `root` haengt sich an alles, und die Gegenmassnahmen (`yama`) sind nachtraeglich
// aufgeschraubte Politik. Hier IST die Autoritaet ein Objekt im CDT — Widerruf, Delegation und
// Audit kommen aus Maschinerie, die es schon gibt und die schon gemessen ist.

/// Wie viele Threads angehalten wurden (nach Wirkung).
pub static DEBUG_STOPS: AtomicU64 = AtomicU64::new(0);
/// Wie viele mit `ERR_DEBUG_BUSY` abgewiesen wurden -- der benannte Ueberlauf einer Kapazitaet
/// von eins.
pub static DEBUG_BUSY: AtomicU64 = AtomicU64::new(0);
/// Wie viele Threads durch `DEBUG_CONTINUE` weiterliefen.
pub static DEBUG_CONTINUES: AtomicU64 = AtomicU64::new(0);
/// Wie viele Threads die **Cap-Finalisierung** freigegeben hat (Revoke ODER Teardown).
pub static DEBUG_RELEASED: AtomicU64 = AtomicU64::new(0);
/// Wie oft die Finalisierung ueberhaupt eine Meldung hatte. **Getrennt von `DEBUG_RELEASED`**,
/// weil es zwei Aussagen sind: „die Autoritaet ging weg" und „ein Thread lief wieder los". Dieselbe
/// Unterscheidung wie `rx_used` gegen „Daten sind angekommen".
pub static DEBUG_RELEASE_EVENTS: AtomicU64 = AtomicU64::new(0);
/// Abweisungen mit `ERR_NOT_DEBUGGABLE` -- die Zahl, an der die Zusage aus §0 haengt.
pub static DEBUG_NOT_DEBUGGABLE: AtomicU64 = AtomicU64::new(0);
/// Erfolgreiche Ableitungen (`DEBUG_ATTACH`).
pub static DEBUG_ATTACHES: AtomicU64 = AtomicU64::new(0);
/// Abgewiesene Registerschreibvorgaenge, aufgeschluesselt: Ring-Wort · ueber der Stufe · ungueltiger
/// Wert. **Drei Zahlen, weil es drei verschiedene Lagen sind** -- „abgewiesen" allein ist als
/// Diagnose wertlos.
pub static DEBUG_WR_RING: AtomicU64 = AtomicU64::new(0);
pub static DEBUG_WR_LEVEL: AtomicU64 = AtomicU64::new(0);
pub static DEBUG_WR_VALUE: AtomicU64 = AtomicU64::new(0);
/// Angenommene Registerschreibvorgaenge.
pub static DEBUG_WR_OK: AtomicU64 = AtomicU64::new(0);

/// **Eine `Debuggable`-Cap fuer `pd` praegen** -- gerufen aus `loader::debuggable_praegen` und
/// sonst nirgends.
///
/// Die Cap landet im Cspace der **Root-PD** (PD 0), nicht in der der Ziel-PD: eine PD, die ihre
/// eigene Debug-Wurzel haelt, kann sich selbst debuggen und die Autoritaet weiterreichen — die
/// Zusage waere dann eine ueber die Gutwilligkeit des Ziels.
pub fn mint_debuggable(pd: usize) -> Option<usize> {
    let mut g = CAPS.write();
    let cap = g.cspace.install_debuggable(pd as u16, Rights::RW).ok()?;
    let slot = g.pds.free_cap_slot(0)?;
    if g.install_cap_checked(0, slot, cap) {
        Some(slot)
    } else {
        None
    }
}

/// **Haelt IRGENDJEMAND im ganzen System Debug-Autoritaet ueber `pd`?**
///
/// Die Frage, auf der die Zusage ruht — und sie wird **beantwortet, nicht behauptet**: ein Durchlauf
/// der Objekttabelle, nicht das Lesen eines Flags, das jemand gesetzt haben koennte.
pub fn any_debug_authority_over(pd: usize) -> bool {
    CAPS.read().cspace.any_debug_authority_over(pd as u16)
}

/// Welche PD bezeichnet diese Cap, und mit welchem Recht? `None` = keine Debug-Cap.
///
/// **Die Rechte unterscheiden, nicht die Objektart** — s. `ObjectKind::Debuggable`. Ein Kind mit
/// `READ` liest und haelt nie etwas an; eines mit `WRITE` steuert. Die Wurzel traegt `RW` und ist
/// damit Steuerrecht, weil sie jederzeit eines praegen kann.
fn debug_cap_kind(g: &Caps, cap: CapPtr) -> Option<(u16, bool, bool)> {
    let info = g.cspace.inspect(cap)?;
    let ObjectKind::Debuggable { pd } = info.kind else {
        return None;
    };
    // **Die Wurzel gewaehrt selbst NICHTS** -- weder Lesen noch Anhalten. Sie ist das Recht,
    // abzuleiten, und der Knoten, an dem `revoke` ansetzt. Ohne diese Zeile waere „gewaehrt selbst
    // nichts" eine Prosa-Aussage ohne Gatter, und ein Revoke liesse ein lebendes Steuerrecht
    // stehen -- gemessen am 2026-08-20 als `revoke-bricht-nicht=false`.
    if info.is_root {
        return Some((pd, false, false));
    }
    Some((
        pd,
        info.rights.contains(Rights::READ),
        info.rights.contains(Rights::WRITE),
    ))
}

/// **`SYS_DEBUG_ATTACH`** -- aus einer `Debuggable` ein Lese- und/oder Steuerrecht ableiten.
///
/// ## Abgeleitet wird ueber `mint`, und das ist die ganze Zusage
///
/// `mint` haengt das neue Cap als **CDT-Kind** unter das vorgelegte. Damit nimmt ein `revoke` an
/// der Wurzel jedes abgeleitete Recht mit — auch weiterverschenkte —, und „wer haelt Debug-
/// Autoritaet ueber diese PD?" ist ein Baumlauf statt einer Behauptung. Beides faellt aus der
/// Ableitung heraus, keines ist gebaut.
///
/// **Der erste Entwurf hat hier ein neues Wurzel-Cap angelegt.** Es sah aus wie eine Ableitung und
/// war keine: `revoke` an der Wurzel liess die „Kinder" stehen. Gefunden hat es nicht das
/// Gegenlesen, sondern die Messung — `revoke-bricht-nicht=false` in der `dbg`-Zeile.
pub fn debug_attach(caller_pd: usize, slot: usize, rights: u64) -> Result<u64, u64> {
    use caprock_abi::{debug as dbgr, result};
    if rights & !dbgr::RIGHT_BOTH != 0 || rights == 0 {
        // Ein unbekanntes Rechtebit heisst „ich verlange etwas, wofuer es keinen Mechanismus gibt".
        // Abweisen, nicht maskieren — dieselbe Regel wie bei den reservierten Manifest-Bytes.
        return Err(result::ERR_RIGHTS);
    }
    let mut g = CAPS.write();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some(info) = g.cspace.inspect(cap) else {
        return Err(result::ERR_BADCAP);
    };
    let ObjectKind::Debuggable { pd } = info.kind else {
        return Err(result::ERR_BADCAP);
    };
    let mut vergeben = 0u64;
    if rights & dbgr::RIGHT_READ != 0 {
        let c = g
            .cspace
            .mint(cap, Rights::READ, pd as u64)
            .map_err(|_| result::ERR_NOSPACE)?;
        let sl = g.pds.free_cap_slot(caller_pd).ok_or(result::ERR_NOSPACE)?;
        if !g.install_cap_checked(caller_pd, sl, c) {
            return Err(result::ERR_NOSPACE);
        }
        vergeben |= (sl as u64) << 32;
    }
    if rights & dbgr::RIGHT_CONTROL != 0 {
        let c = g
            .cspace
            .mint(cap, Rights::RW, pd as u64)
            .map_err(|_| result::ERR_NOSPACE)?;
        let sl = g.pds.free_cap_slot(caller_pd).ok_or(result::ERR_NOSPACE)?;
        if !g.install_cap_checked(caller_pd, sl, c) {
            return Err(result::ERR_NOSPACE);
        }
        vergeben |= sl as u64;
    }
    DEBUG_ATTACHES.fetch_add(1, Ordering::Relaxed);
    Ok(vergeben)
}

/// **`SYS_DEBUG_STOP`.**
///
/// ## Warum `CAPS.read()` ueber den ganzen Vorgang gehalten wird
///
/// Eine Cap-Pruefung beim Eintritt und ein Setzen des Grundes danach waeren **zwei** kritische
/// Sektionen, und dazwischen passt ein `revoke`: der Thread bekaeme `DEBUG` gesetzt, nachdem die
/// Freigabe schon gelaufen ist, und traege danach einen Grund, den niemand mehr entfernen darf.
/// Das ist genau der Fehler, den niemand sieht, weil er im Normalbetrieb nie auftritt.
///
/// Die Schachtelung ist erlaubt und nicht neu: `CAPS` ist **R0**, `SCHEDS[core]` ist **R2**
/// (`docs/invariants.md` §1), und Schachteln darf nur aufsteigend. `bind_sched_context` gibt
/// `CAPS` vorher frei — aus Kontentionsgruenden, nicht aus Ordnungsgruenden.
///
/// **Der urspruengliche Plan sah hier eine Objekt-Generation vor.** Sie wird nicht gebraucht: die
/// gemeinsame Lesesperre leistet dasselbe, ohne eine zweite Zahl, die jemand pflegen muss.
pub fn debug_stop(caller_pd: usize, slot: usize, tid_raw: u64) -> Result<u64, u64> {
    use caprock_abi::result;
    let tid = ThreadId::from_raw(tid_raw);
    let g = CAPS.read(); // R0 -- gehalten, s. Doku
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        return Err(result::ERR_RIGHTS);
    }
    // **Die Cap benennt EINE PD.** Ein Thread einer anderen ist kein Grenzfall, sondern der
    // Versuch, die Grenze zu ueberschreiten -- und `ERR_BADCAP` ist dafuer die richtige Antwort:
    // fuer DIESEN Thread haelt der Aufrufer keine Cap.
    if g.pds.pd_of_thread_pub(tid) != Some(pd as usize) {
        return Err(result::ERR_BADCAP);
    }
    let ok = with_owner(tid, |s, _| s.debug_stop(tid).then_some(())).is_some();
    drop(g);
    if ok {
        DEBUG_STOPS.fetch_add(1, Ordering::Relaxed);
        // Laeuft er auf einem anderen Kern, haelt ihn erst der naechste Kerneleintritt an --
        // der IPI beschleunigt genau das. **Mitten in einer Instruktion haelt hier nichts an**,
        // und das gehoert in die Zusage: bei 100 Hz sind das <= 10 ms.
        if let Some(c) = owner_core_of(tid) {
            kick(c);
        }
        Ok(0)
    } else {
        DEBUG_BUSY.fetch_add(1, Ordering::Relaxed);
        Err(result::ERR_DEBUG_BUSY)
    }
}

/// **`SYS_DEBUG_CONTINUE`** -- entfernt `DEBUG` und **nur** das.
pub fn debug_continue(caller_pd: usize, slot: usize, tid_raw: u64) -> Result<u64, u64> {
    use caprock_abi::result;
    let tid = ThreadId::from_raw(tid_raw);
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        return Err(result::ERR_RIGHTS);
    }
    if g.pds.pd_of_thread_pub(tid) != Some(pd as usize) {
        return Err(result::ERR_BADCAP);
    }
    let (ok, core) = match with_owner(tid, |s, _| s.debug_continue(tid).then_some(())) {
        Some((_, c)) => (true, Some(c)),
        None => (false, None),
    };
    drop(g);
    if ok {
        DEBUG_CONTINUES.fetch_add(1, Ordering::Relaxed);
        if let Some(c) = core {
            kick(c);
        }
        Ok(0)
    } else {
        // Nicht angehalten (oder tot). **Kein Fehler mit eigenem Code**, sondern `OK` mit der
        // Zahl 0: „fortsetzen, was nicht steht" ist idempotent, und ein Debugger, der nach einem
        // Revoke aufraeumt, soll daran nicht scheitern.
        Ok(1)
    }
}

/// **Eine Cap ueber ihren Slot in einer PD widerrufen** (Z6b, Pruefpfad).
///
/// Steht hier statt im Pruefer, weil der Pruefer sonst die Aufloesung `Slot -> Cap` nachrechnete —
/// *ein Pruefer, der die gepruefte Groesse NACHRECHNET statt sie zu lesen, prueft eine zweite
/// Wirklichkeit*.
pub fn revoke_slot(pd: usize, slot: usize) -> bool {
    let Some(cap) = CAPS.read().pds.cap_ptr_at(pd, slot) else {
        return false;
    };
    cap_revoke(cap).is_ok()
}

/// **Ein Steuerrecht in eine FREMDE PD ableiten** (Z6b, Pruefpfad).
///
/// Damit der Absturz einer Debugger-PD messbar wird und nicht nur gelesen: die Sonde legt eine
/// Wegwerf-PD an, gibt ihr das Steuerrecht, laesst sie das Ziel anhalten und **zerstoert sie**.
/// `destroy_pd` loescht jeden Cap ueber `cap_delete` — also ueber denselben Sammler wie
/// `cap_revoke`, aber ueber den ANDEREN Pfad. Genau das ist die Stelle, an der eine Regel, die
/// zweimal hingeschrieben wird, zum zweiten Mal altert.
pub fn debug_attach_to(caller_pd: usize, slot: usize, ziel_pd: usize) -> Option<usize> {
    let mut g = CAPS.write();
    let cap = g.pds.cap_ptr_at(caller_pd, slot)?;
    let info = g.cspace.inspect(cap)?;
    let ObjectKind::Debuggable { pd } = info.kind else {
        return None;
    };
    let c = g.cspace.mint(cap, Rights::RW, pd as u64).ok()?;
    let sl = g.pds.free_cap_slot(ziel_pd)?;
    g.install_cap_checked(ziel_pd, sl, c).then_some(sl)
}

/// **Ein LESERECHT in eine fremde PD ableiten** (Z6b, Pruefpfad der Speicher-Sonde).
///
/// Getrennt von [`debug_attach_to`], weil es ein anderes Recht ist: `READ` liest und haelt nie
/// etwas an. Die Sonde braucht genau diese Trennung — sie belegt, dass ein Leser **ohne**
/// Steuerrecht auskommt, und das ist die halbe Begruendung der Rechteaufteilung.
pub fn debug_attach_read_to(caller_pd: usize, slot: usize, ziel_pd: usize) -> Option<usize> {
    let mut g = CAPS.write();
    let cap = g.pds.cap_ptr_at(caller_pd, slot)?;
    let info = g.cspace.inspect(cap)?;
    let ObjectKind::Debuggable { pd } = info.kind else {
        return None;
    };
    let c = g.cspace.mint(cap, Rights::READ, pd as u64).ok()?;
    let sl = g.pds.free_cap_slot(ziel_pd)?;
    g.install_cap_checked(ziel_pd, sl, c).then_some(sl)
}

/// **Laeuft dieser Thread gerade auf irgendeinem Kern?** (Z6b, Latenzmessung.)
///
/// Steht hier und nicht im Pruefer, weil `SCHEDS` privat ist -- und weil ein Pruefer, der die
/// Bedingung nachrechnet statt sie zu lesen, eine zweite Wirklichkeit prueft. `freeze_thread`
/// stellt dieselbe Frage und muss dieselbe Antwort bekommen.
///
/// `core_online` und nicht nur `num_cores()`: ein unterdruecktes SMT-Geschwister hat Tabellen,
/// aber kein `current` (Z6 Stufe 1, gemessen unter `-smp cores=2,threads=2`).
pub fn thread_is_current(tid: ThreadId) -> bool {
    for c in 0..num_cores() {
        if !caprock_sched::core_online(c) {
            continue;
        }
        if SCHEDS[c].lock().current_id(c) == tid {
            return true;
        }
    }
    false
}

/// **Die Fenster-VA der privaten Region einer isolierten PD** (Z6b, Speicher-Sonde).
///
/// `spawn_isolated_parked` gibt die **Phys**basis zurueck; die Sonde braucht die **User**-VA, unter
/// der dieselbe Region im Adressraum des Ziels erscheint. Die Rechnung steht hier und nicht im
/// Pruefer: *ein Pruefer, der die gepruefte Groesse nachrechnet, prueft eine zweite Wirklichkeit* —
/// und `SLOT_DATA` ist privat, was genau der richtige Grund ist, die Zahl hier zu bilden.
///
/// **Warum das ein eigener Messpunkt ist:** diese Region wird als **2-MiB-Block** gemappt
/// (`vspace_map_user_region` mit `rlen == TWO_MIB`), nicht ueber eine L3-Tabelle. In
/// `vspace_resolve` ist das der andere Zweig — `PS` auf x86, `BLOCK_DESC` auf aarch64 — und dort
/// steht die Rechtepruefung an einer anderen Stelle als beim Blatt. Wer nur den Seitenzweig misst,
/// hat die Haelfte der Aufloesung ungeprueft.
pub fn iso_user_data_va() -> u64 {
    hal::mmu::ISO_USER_VA + (SLOT_DATA as u64) * 2 * 1024 * 1024
}

/// **Lebt ueber `pd` noch ein Steuerrecht?** (Z6b) — s. `CapSpace::any_debug_control_over` fuer
/// die Abgrenzung gegen [`any_debug_authority_over`].
pub fn any_debug_control_over(pd: usize) -> bool {
    CAPS.read().cspace.any_debug_control_over(pd as u16)
}

/// **Eine Cap ueber ihren Slot loeschen** (Z6b, Pruefpfad) — der `cap_delete`-Weg, den eine
/// sterbende PD nimmt, im Unterschied zu [`revoke_slot`].
pub fn delete_slot(pd: usize, slot: usize) -> bool {
    let Some(cap) = CAPS.read().pds.cap_ptr_at(pd, slot) else {
        return false;
    };
    cap_delete(cap).is_ok()
}

/// **`SYS_DEBUG_READ_MEM`** -- Speicher der Ziel-PD lesen und in den Puffer des Aufrufers legen.
///
/// ## Warum das ein Syscall ist und keine Bibliotheksfunktion
///
/// Weil die **Seitentabellen des Ziels** gelaufen werden muessen, nicht die des Aufrufers. Genau
/// diese Verwechslung waere von aussen nicht zu sehen: der Debugger bekaeme Bytes, sie waeren
/// plausibel, und sie waeren seine eigenen. Deshalb ist es auch die Gegenprobe der `dbg`-Zeile --
/// die Karte des Debuggers statt der des Ziels laufen zu lassen muss die Zeile rot machen.
///
/// ## Erlaubt, WAEHREND das Ziel laeuft -- und das ist eine Entscheidung
///
/// Ein Bytebereich hat keine innere Konsistenzbedingung; zerrissen ist der ehrliche, erwartete
/// Zustand, und ein durchgehend beobachtendes Werkzeug braucht genau das. Der **Registerframe** ist
/// der umgekehrte Fall (er ist nur als Ganzes wahr) und haengt deshalb an der Sidecar-Generation.
/// Zwei Objekte, zwei Regeln -- keine Wahlmoeglichkeit.
///
/// ## Die Laenge ist gedeckelt, und der Deckel ist benannt
///
/// [`caprock_abi::debug::READ_MAX`]. Ein vom Aufrufer gewaehlter, unbegrenzter Lauf unter einer
/// Sperre ist ein Latenzloch, das niemand sieht, bis es ein Haenger ist. Der Aufrufer schleift --
/// und **seine** Schleife ist unterbrechbar.
pub fn debug_read_mem(
    caller_pd: usize,
    slot: usize,
    va: u64,
    len: u64,
    dst: u64,
) -> Result<u64, u64> {
    use caprock_abi::{debug as dbgr, result};
    if len == 0 || len > dbgr::READ_MAX {
        return Err(result::ERR_RIGHTS);
    }
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, read, _)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !read {
        // Eine blosse `Debuggable` liest nicht. Sie ist das Recht, ein Leserecht ABZULEITEN --
        // die Unterscheidung ist der Grund, warum es drei Cap-Arten gibt und nicht eine.
        return Err(result::ERR_RIGHTS);
    }
    drop(g);

    // Die Tabellen beider Seiten. **Getrennt geholt und getrennt benannt** -- ein Parameter, der
    // beide Bedeutungen traegt, ist die Form, die dieses Projekt bei `spawn_user` bezahlt hat.
    let (ziel_l1, ziel_l2) = pd_tables(pd as usize).ok_or(result::ERR_NOPD)?;
    let (mein_l1, mein_l2) = pd_tables(caller_pd).ok_or(result::ERR_NOPD)?;

    let mut n = 0u64;
    while n < len {
        let Some(src_pa) = hal::mmu::vspace_resolve(ziel_l1, ziel_l2, va + n) else {
            break; // Luecke im Ziel -- ein Debugger soll eine Luecke SEHEN, nicht raten
        };
        let Some(dst_pa) = hal::mmu::vspace_resolve(mein_l1, mein_l2, dst + n) else {
            return Err(result::ERR_BADSTACK); // der Aufrufer hat einen Puffer benannt, den er nicht hat
        };
        // SAFETY: beide Physadressen stammen aus einer Tabellenaufloesung dieses Kernels und
        // liegen damit in gemapptem RAM; der Kernel erreicht sie ueber die Identitaetskarte.
        // Byteweise, weil die beiden Seiten verschiedene Ausrichtungen haben koennen.
        unsafe {
            core::ptr::write_volatile(dst_pa as *mut u8, core::ptr::read_volatile(src_pa as *const u8));
        }
        n += 1;
    }
    DEBUG_READ_BYTES.fetch_add(n, Ordering::Relaxed);
    Ok(n)
}

/// Wie viele Bytes ueber `DEBUG_READ_MEM` gelesen wurden (Sprechprobe der `dbg`-Zeile).
pub static DEBUG_READ_BYTES: AtomicU64 = AtomicU64::new(0);

/// Die beiden obersten Seitentabellen der PD `pd`.
///
/// Ueber einen ihrer Threads, weil die ASID am Thread-Slot haengt und nicht an der PD -- eine
/// Herleitung, die stimmt, solange alle Threads einer PD dieselbe VSpace teilen, und das ist die
/// Definition einer PD. Steht als **eine** Funktion da, damit die Herleitung nicht an drei Stellen
/// nachgerechnet wird.
fn pd_tables(pd: usize) -> Option<(u64, u64)> {
    let tid = CAPS.read().pds.any_thread_of(pd)?;
    let asid = (vspace_of(tid.slot()) >> 48) as u16;
    if asid == 0 {
        return None;
    }
    let v = VSPACES.lock()[asid as usize - 1];
    if v.used {
        Some((v.l1, v.l2))
    } else {
        None
    }
}

/// **`SYS_DEBUG_WRITE_REGS`** -- ein Frame-Wort des Ziels schreiben, gegen die Maske.
///
/// Die Maske ist die ganze Sicherheitsaussage, und sie steht an **einer** Stelle
/// (`caprock_sched::redirect::writeback_erlaubt`). Zwei Kopien einer Regel sind der Riss, den
/// dieses Projekt bei der Farbarithmetik schon einmal bezahlt hat.
pub fn debug_write_reg(
    caller_pd: usize,
    slot: usize,
    tid_raw: u64,
    idx: u64,
    value: u64,
) -> Result<u64, u64> {
    use caprock_abi::result;
    use caprock_sched::redirect::{writeback_erlaubt, SchreibStufe, SchreibUrteil};
    let tid = ThreadId::from_raw(tid_raw);
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        return Err(result::ERR_RIGHTS);
    }
    if g.pds.pd_of_thread_pub(tid) != Some(pd as usize) {
        return Err(result::ERR_BADCAP);
    }
    // **Nur an einem ANGEHALTENEN Thread.** Ein laufender Thread schreibt seinen eigenen Frame; wer
    // ihm dabei hineinschreibt, erzeugt einen Zustand, den es nie gab. Dass er steht, ist hier eine
    // Bedingung und keine Hoffnung.
    if !with_owner(tid, |s, _| s.is_debug_stopped(tid).then_some(())).is_some() {
        return Err(result::ERR_DEBUG_BUSY);
    }
    let urteil = writeback_erlaubt(
        hal::exception::FRAME_ARCH,
        idx as usize,
        value,
        SchreibStufe::Debugger,
    );
    match urteil {
        SchreibUrteil::Ok => {}
        SchreibUrteil::RingWort(_) => {
            DEBUG_WR_RING.fetch_add(1, Ordering::Relaxed);
            return Err(result::ERR_RIGHTS);
        }
        SchreibUrteil::UeberDerStufe(_) => {
            DEBUG_WR_LEVEL.fetch_add(1, Ordering::Relaxed);
            return Err(result::ERR_RIGHTS);
        }
        _ => {
            DEBUG_WR_VALUE.fetch_add(1, Ordering::Relaxed);
            return Err(result::ERR_RIGHTS);
        }
    }
    let done = with_owner(tid, |s, _| {
        let frame = s.frame_of(tid)?;
        hal::exception::frame_wort_setzen(frame, idx as usize, value).then_some(())
    })
    .is_some();
    drop(g);
    if done {
        DEBUG_WR_OK.fetch_add(1, Ordering::Relaxed);
        Ok(0)
    } else {
        Err(result::ERR_BADCAP)
    }
}

/// **`SYS_DEBUG_WRITE_MEM` (33)** -- Speicher der Ziel-PD schreiben, aus dem Puffer des Aufrufers.
///
/// Das Spiegelbild von [`debug_read_mem`]: dieselbe benannte Kapazitaet
/// ([`caprock_abi::debug::WRITE_MAX`] -- wertgleich mit `READ_MAX`, aber EINE Schranke je
/// Richtung, nicht eine Zahl mit zwei Bedeutungen), dieselbe Latenzform (der Aufrufer schleift,
/// seine Schleife ist preemptibel), dieselbe Luecken-Regel (eine Luecke im ZIEL beendet den Lauf
/// mit der bisherigen Zahl -- ein Debugger soll eine Luecke SEHEN, nicht raten; ein fehlender
/// Aufrufer-Puffer ist [`result::ERR_BADSTACK`]). Nur die Kopierrichtung ist gedreht; Reihenfolge
/// und Vorrang der beiden Aufloesungen sind bitgleich zum Lesen.
///
/// ## Das Recht ist `DebugControl`, nicht `DebugRead`
///
/// Lesen beobachtet, Schreiben veraendert -- ein Leserecht ohne Steuerrecht kommt hier nicht
/// hinein ([`result::ERR_RIGHTS`]). Dieselbe Trennung wie zwischen Sidecar-Lesen (gar kein
/// Syscall) und `DEBUG_WRITE_REGS` (Steuerrecht plus Maske).
pub fn debug_write_mem(
    caller_pd: usize,
    slot: usize,
    va: u64,
    len: u64,
    src: u64,
) -> Result<u64, u64> {
    use caprock_abi::{debug as dbgr, result};
    if len == 0 || len > dbgr::WRITE_MAX {
        return Err(result::ERR_RIGHTS);
    }
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        // Ein Leserecht schreibt nicht -- sonst waere `RIGHT_READ` die ganze Autoritaet und die
        // Aufteilung in zwei Rechte eine Behauptung ohne Gatter.
        return Err(result::ERR_RIGHTS);
    }
    drop(g);

    // Die Tabellen beider Seiten -- getrennt geholt und getrennt benannt, wie in
    // `debug_read_mem` (ein Parameter mit zwei Bedeutungen war dort die `spawn_user`-Falle).
    let (ziel_l1, ziel_l2) = pd_tables(pd as usize).ok_or(result::ERR_NOPD)?;
    let (mein_l1, mein_l2) = pd_tables(caller_pd).ok_or(result::ERR_NOPD)?;

    let mut n = 0u64;
    while n < len {
        let Some(dst_pa) = hal::mmu::vspace_resolve(ziel_l1, ziel_l2, va + n) else {
            break; // Luecke im Ziel -- wie beim Lesen: bisherige Zahl gilt
        };
        let Some(src_pa) = hal::mmu::vspace_resolve(mein_l1, mein_l2, src + n) else {
            return Err(result::ERR_BADSTACK); // der Aufrufer hat einen Puffer benannt, den er nicht hat
        };
        // SAFETY: beide Physadressen stammen aus einer Tabellenaufloesung dieses Kernels und
        // liegen damit in gemapptem RAM; der Kernel erreicht sie ueber die Identitaetskarte.
        // Byteweise, weil die beiden Seiten verschiedene Ausrichtungen haben koennen.
        unsafe {
            core::ptr::write_volatile(dst_pa as *mut u8, core::ptr::read_volatile(src_pa as *const u8));
        }
        n += 1;
    }
    Ok(n)
}

/// **`SYS_DEBUG_SINGLE_STEP` (34)** -- einen angehaltenen Thread genau einen Befehl tun lassen.
///
/// ## Stand: autorisiert geprueft, CPU-Pfad fehlt -- benannte Absage statt Stub-Erfolg
///
/// Cap-Aufloesung, Steuerrecht (`DebugControl`), PD-Zugehoerigkeit und Halt-Zustand werden VOLL
/// geprueft -- dieselben Codes wie [`debug_write_reg`]: `ERR_BADCAP` (fremde/leere Cap, falsche
/// PD), `ERR_RIGHTS` (kein Steuerrecht), `ERR_DEBUG_BUSY` (das Ziel ist nicht gehalten; ein
/// Schrittbefehl an einen laufenden Thread waere ein Zustand, den es nie gab). Danach faellt der
/// Aufruf auf [`result::ERR_BADSYS`] -- „Antrag ok, Pfad fehlt" (dieselbe Form wie die 31/32-Arme
/// im Microkit-Dispatch): es gibt kein `hal::debug` -- kein TF-Scharfstellen auf x86, kein
/// `MDSCR_EL1.SS`/`PSTATE.SS` auf aarch64, keinen `#DB`-Pfad, der den Schritt meldet und die
/// Scharfstellung zuruecknimmt.
///
/// Ein TF-Bit ohne Handler zu setzen waere geraten, nicht verdrahtet: der naechste Schritt
/// lieferte `#DB` an einen Kernel ohne `#DB`-Pfad. Deshalb wird hier NICHTS an der CPU gestellt --
/// fail-closed, nie `OK`.
///
/// ## Patch-Text fuer `hal::debug` (bauen, nicht raten)
/// ```text
/// // crates/caprock-hal/src/debug.rs (neu, je Arch ein Modul hinter einer Fassade):
/// pub fn single_step_scharf(frame: *mut TrapFrame);
/// //   x86: RFLAGS.TF (Bit 8) im GESPEICHERTEN Frame setzen (nicht live -- der Thread steht).
/// //   aarch64: PSTATE.SS im SPSR des Frames plus MDSCR_EL1.SS (EL1-Zugriff einrichten).
/// // Dazu ein #DB-/Debug-Exception-Pfad, der (a) den Schritt an den haltenden Debugger meldet,
/// // (b) TF/SS zuruecknimmt (sonst ist es ein Modus, kein Schritt: JEDER Befehl trapt),
/// // (c) ohne haltenden Debugger fail-closed bleibt.
/// // Reihenfolge im Rueckruf: erst scharfstellen, DANN freigeben (die Freigabe ist heute
/// // `debug_continue`-foermig) -- nie umgekehrt: freigegeben-aber-nicht-scharf liefe frei.
/// ```
pub fn debug_single_step(caller_pd: usize, slot: usize, tid_raw: u64) -> Result<u64, u64> {
    use caprock_abi::result;
    let tid = ThreadId::from_raw(tid_raw);
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        return Err(result::ERR_RIGHTS);
    }
    if g.pds.pd_of_thread_pub(tid) != Some(pd as usize) {
        return Err(result::ERR_BADCAP);
    }
    // **Nur an einem ANGEHALTENEN Thread** -- derselbe Grund wie in `debug_write_reg` und
    // derselbe Code wie im ABI-Vertrag (`ERR_DEBUG_BUSY`, wenn nicht gehalten).
    if !with_owner(tid, |s, _| s.is_debug_stopped(tid).then_some(())).is_some() {
        return Err(result::ERR_DEBUG_BUSY);
    }
    drop(g);
    // Kein `hal::debug` -- s. Doku oben. Benannt abgewiesen, nie `OK`.
    Err(result::ERR_BADSYS)
}

/// **`SYS_DEBUG_HWBREAK` (35)** -- Hardware-Breakpoint setzen/loeschen.
///
/// ## Stand: autorisiert geprueft, CPU-Pfad fehlt -- benannte Absage statt Stub-Erfolg
///
/// Geprueft wird VOLL: Cap (`ERR_BADCAP`), Steuerrecht (`ERR_RIGHTS`), PD-Zugehoerigkeit des
/// Zielthreads (`ERR_BADCAP`), Halt-Zustand (`ERR_DEBUG_BUSY` -- wie [`debug_write_reg`], in
/// derselben Reihenfolge: wer einem laufenden Thread in die Register redet, erzeugt einen
/// Zustand, den es nie gab), Art (`MSG3`: `1` = Hardware; `0` = Software ist die falsche Bitte an
/// den Kernel -- Software-Breakpoints (`INT3`/`BRK`) schreibt der Debugger selbst via
/// `DEBUG_WRITE_MEM`, dafuer braucht es keinen Syscall; alles andere ist keine Art -- beides
/// `ERR_RIGHTS`, dieselbe Klasse wie die Laengen-Deckel in `debug_read_mem`/`debug_write_mem`).
///
/// Danach faellt `1` (Hardware) auf [`result::ERR_BADSYS`] -- „Antrag ok, Pfad fehlt": vier
/// Register je Kern sind eine benannte Kapazitaet, und sie braucht das **pro-Thread-Sichern der
/// Debugregister im Kontextwechsel** (`hal::debug`), das es nicht gibt. Ohne das Sichern wuerde
/// ein Breakpoint des Threads A auf Kern 0 den Thread B treffen, der als naechstes dort laeuft --
/// still geteilt statt benannt abgewiesen, genau die Form, die der ABI-Vertrag verbietet.
///
/// ## Was die Luecke ehrlich umfasst -- kein Kontextwechsel-Umbau ohne Not
///
/// - `hal::debug` fehlt GANZ (x86: kein DR0-DR3/DR7-Sichern, kein DR6-Lesen, kein `#DB`-Pfad;
///   aarch64: kein DBGBVR/DBGBCR-Sichern, kein Debug-Exception-Pfad).
/// - Der Kontextwechsel fasst Debugregister heute NICHT an -- absichtlich unangetastet: ein
///   Sichern ohne Vergabe schuetzt nichts und kostet jedem Wechsel Zyklen.
/// - Kapazitaet (4 je Kern), Vergabe-Tabelle, Erschoepfungs-Absage und Loesch-Pfad entstehen
///   MIT `hal::debug`, nicht vorher. Die Loesch-Kodierung (z. B. `addr == 0`) wird mit der
///   RSP-PD geklaert, nicht hier festgelegt -- eine erfundene Konvention waere ein zweiter
///   Vertrag neben der ABI.
///
/// ## Patch-Text fuer `hal::debug` + Kontextwechsel (bauen, nicht raten)
/// ```text
/// // crates/caprock-hal/src/debug.rs:
/// pub const HWBREAKS_JE_KERN: usize = 4; // benannte Kapazitaet aus dem ABI-Vertrag
/// pub enum HwbreakAbweisung { Erschoepft, UngueltigeAdresse }
/// pub fn hwbreak_setzen(fach: usize /* 0..4 */, addr: u64) -> Result<(), HwbreakAbweisung>;
/// pub fn hwbreak_loeschen(fach: usize);
/// // Kontextwechsel: DR0-DR3 + DR7 (x86) bzw. DBGBVR/DBGBCR + DBGBCR-Ermoeglichung (aarch64)
/// // je THREAD sichern/wiederherstellen -- Ablage im TCB (4 Adressen + Kontrolle), NICHT global
/// // je Kern. Vergabe-Tabelle im Kernel; Erschoepfung -> ERR_NOSPACE (benannt, nicht still
/// // geteilt -- ABI-Vertrag); Loeschen gibt das Fach frei.
/// ```
pub fn debug_hwbreak(
    caller_pd: usize,
    slot: usize,
    tid_raw: u64,
    addr: u64,
    art: u64,
) -> Result<u64, u64> {
    use caprock_abi::result;
    let tid = ThreadId::from_raw(tid_raw);
    let g = CAPS.read();
    let Some(cap) = g.pds.cap_ptr_at(caller_pd, slot) else {
        return Err(result::ERR_BADCAP);
    };
    let Some((pd, _, control)) = debug_cap_kind(&g, cap) else {
        return Err(result::ERR_BADCAP);
    };
    if !control {
        return Err(result::ERR_RIGHTS);
    }
    if g.pds.pd_of_thread_pub(tid) != Some(pd as usize) {
        return Err(result::ERR_BADCAP);
    }
    // **Nur an einem ANGEHALTENEN Thread** -- wie `debug_write_reg`, vor der Art-Pruefung.
    if !with_owner(tid, |s, _| s.is_debug_stopped(tid).then_some(())).is_some() {
        return Err(result::ERR_DEBUG_BUSY);
    }
    // Die Art: nur `1` (Hardware) ist eine Bitte an den Kernel. `0` (Software) gehoert dem
    // Debugger selbst (INT3/BRK via `DEBUG_WRITE_MEM`); alles andere ist keine Art.
    if art != 1 {
        return Err(result::ERR_RIGHTS);
    }
    let _ = addr; // Entgegengenommen, aber an keine CPU gestellt -- s. Doku oben.
    drop(g);
    // Kein `hal::debug`, kein pro-Thread-Sichern -- s. Doku oben. Benannt abgewiesen, nie `OK`.
    Err(result::ERR_BADSYS)
}

// --- Z4a: der Haltepunkt ------------------------------------------------------------------------

/// Wie ein Einfrierversuch ausging (Z4a). Bewusst **unterscheidbar**: „geht nicht" ist als
/// Diagnose wertlos, und die drei Fälle verlangen drei verschiedene Reaktionen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freeze {
    /// Der Thread steht an einer **benennbaren** Grenze: deplant, auf keinem Kern, und in keiner
    /// IPC-Rolle. Sein Trap-Frame ist vollständig — das ist die Voraussetzung für alles Weitere.
    Frozen,
    /// Er ist deplant, läuft aber **noch** auf einem Kern (der Reschedule-IPI ist unterwegs).
    /// Der Aufrufer wiederholt; der Zustand ist vorübergehend.
    StillRunning,
    /// Wie [`Freeze::Busy`], aber **mit dem Partner** (Z23/S2) — der Thread haengt an einer
    /// konkreten Gegenseite, und die wird genannt.
    ///
    /// „Nicht einfrierbar" ist als Diagnose wertlos; „nicht einfrierbar, weil er auf Thread Y
    /// wartet" ist eine Handlungsanweisung. Ohne den Namen ist eine dauerhafte Absage von einem
    /// Haenger nicht zu unterscheiden — genau die Ununterscheidbarkeit, die
    /// `docs/fehlerdomaene.md` schon einmal gekostet hat.
    ///
    /// **Diese Auskunft geht an den Halter der Freeze-Autoritaet, NIE an die eingefrorene PD.**
    /// Eine Fehlermeldung mit einem fremden Threadnamen darin ist ein Kanal, mit dem sich die
    /// IPC-Topologie anderer PDs ausforschen laesst. Der Kernel gibt sie deshalb nur ueber
    /// [`freeze_thread`] zurueck (Kernelpfad, Pruefpfade) und **nicht** ueber einen Syscall.
    BusyOn(Quiescence, ThreadId),
    /// Er hat **offene IPC-Beziehungen** — er hängt in `CALL`, wartet in `RECV`, ist Ziel eines
    /// Reply-Tokens oder schuldet selbst eine Antwort. Das ist **kein** vorübergehender Zustand,
    /// sondern eine Absage: Z4d Stufe 1 lässt einen Thread nur ohne offene Transaktionen wandern.
    /// Ihn hier trotzdem einzufrieren hiesse, einen Partner auf der Gegenseite hängen zu lassen.
    Busy(Quiescence),
    /// Kein solcher Thread.
    NoThread,
    /// **Ein Debugger haelt ihn** (Z6b).
    ///
    /// Eigene Variante und nicht `Frozen`, obwohl der Thread nachweislich steht — und nicht
    /// `Busy`, obwohl es eine Absage ist. Drei Gruende, jeder fuer sich hinreichend:
    ///
    /// 1. **`Frozen` waere eine LUEGE mit Folgen.** Der Aufrufer schliesst daraus „ich habe ihn
    ///    angehalten" und ruft spaeter [`thaw_thread`], das `PAUSE` entfernt — `DEBUG` bleibt, der
    ///    Thread laeuft nicht los, und das Paar friere/taue wird still asymmetrisch.
    /// 2. **Der Frame gehoert gerade jemand anderem.** Z4a existiert, damit ein Checkpoint einen
    ///    vollstaendigen Trap-Frame vorfindet; ein Debugger mit `DebugControl` darf hineinschreiben.
    ///    Beides gleichzeitig ist kein Grenzfall, sondern zwei Schreiber.
    /// 3. **`Busy` heisst „offene IPC-Beziehung"** und legt die falsche Behebung nahe („warte, bis
    ///    die Transaktion durch ist"). Hier hilft nur: den Debugger fragen. Genau die
    ///    Unterscheidung, fuer die `HandlerBinding` sich 2026-08-13 von `PendingReply` getrennt hat.
    Debugged,
}

/// **Wer haelt diesen Thread fest?** (Z23/S2) — systemweit ueber alle Endpoints.
///
/// Dieselbe Grenze wie bei [`thread_quiescence`], und sie gilt hier genauso: die Objekte werden
/// nacheinander gesperrt, die Auskunft ist also eine Momentaufnahme, solange nicht stillgelegt
/// wurde. Fuer eine **Diagnose** reicht das; als Bedingung taugt sie nur nach der Stilllegung.
///
/// **Der erste Treffer gewinnt**, und das ist eine Entscheidung: ein Thread kann an mehreren
/// Endpoints Rollen haben, aber die Absage nennt EINEN Grund. Mehrere zu sammeln hiesse, eine
/// Liste zurueckzugeben, die der Aufrufer sortieren muss -- und die erste offene Beziehung reicht,
/// um den Freeze zu verhindern.
fn thread_partner(tid: ThreadId) -> Option<ThreadId> {
    for i in 0..eps().len() {
        if let Some(p) = eps()[i].lock().partner_of(tid) {
            return Some(p);
        }
    }
    None
}

/// **Einen Thread an einer benennbaren Grenze anhalten** (Z4a).
///
/// ## Was „benennbar" hier heisst
///
/// Genau zweierlei, und beides ist prüfbar:
///
/// 1. **Nicht im Kernel.** Ein Thread, der gerade auf einem Kern läuft, kann mitten in einem
///    Syscall stehen — sein Zustand liegt dann halb im Trap-Frame und halb in Kernel-Variablen.
///    Ein Thread, der auf **keinem** Kern läuft, ist per Konstruktion an einer Trap-Grenze
///    stehengeblieben: der Kernel betritt und verlässt sich in einem Zug, und was dazwischen
///    liegt, ist nie deplant. Deshalb wird die Bedingung über `current_id` je Kern geprüft und
///    nicht geglaubt.
/// 2. **Keine offene IPC-Beziehung** ([`thread_quiescence`]). Mitten in einer Transaktion hängt
///    ein Partner, und der bliebe hier zurück.
///
/// ## Warum das hier NICHT wartet
///
/// Z4a verlangt ein `SYS_FREEZE`, das *wartet*. Das Warten gehört aber **nicht** hierher: diese
/// Funktion nimmt die Scheduler-Sperre jedes Kerns, und in einer Schleife darauf zu warten, dass
/// ein anderer Kern voranschreitet, während man seine Sperre hält, ist die Bauanleitung für einen
/// Deadlock. Der Aufruf ist deshalb **idempotent und ergebnislos wiederholbar**; wer warten will,
/// wiederholt ihn. Ein blockierender Syscall darüber ist eine ABI-Frage und eine eigene Stufe.
///
/// **`Busy` ist kein Zwischenzustand.** Wer darauf wartet, wartet ewig — die Absage ist die
/// Antwort, nicht ein Zeitproblem.
pub fn freeze_thread(tid: ThreadId) -> Freeze {
    if with_owner(tid, |s, _| s.frame_of(tid)).is_none() {
        return Freeze::NoThread;
    }
    // Zuerst deplanen. `pause` schickt bei Bedarf einen Reschedule-IPI -- der wirkt nicht sofort,
    // und genau deshalb gibt es `StillRunning`.
    // **Z6b, und die Reihenfolge ist die Aussage: VOR dem `pause`.**
    //
    // Ein Debugger, der den Thread haelt, ist keine Lage, die ein zusaetzliches `PAUSE` verbessert
    // -- es waere ein zweiter Grund an einem Thread, den der Aufrufer gleich wieder aufgibt, und
    // der bliebe stehen. Erst pausieren und dann absagen hiesse, den Zustand zu veraendern, ueber
    // den man gerade urteilt.
    if with_owner(tid, |s, _| s.is_debug_stopped(tid).then_some(())).is_some() {
        return Freeze::Debugged;
    }
    KernelSched.pause(tid);
    let q = thread_quiescence(tid);
    if !q.is_quiescent() {
        // **Den Partner nennen, wenn es einen GIBT** (Z23/S2). Ein wartender Empfaenger hat
        // keinen -- und einen zu erfinden waere schlimmer als keinen zu nennen, weil dann der
        // Falsche zur Rechenschaft gezogen wird.
        return match thread_partner(tid) {
            Some(p) => Freeze::BusyOn(q, p),
            None => Freeze::Busy(q),
        };
    }
    // **Gefragt, nicht geglaubt:** laeuft er noch irgendwo?
    //
    // `num_cores()` und **nicht** `MAX_CORES`: die Schranke ist die Zahl der KONFIGURIERTEN
    // Kerne. Die erste Fassung lief bis `MAX_CORES` (8) und paniced auf einer Maschine mit vier
    // -- `current_id` auf einer Scheduler-Instanz ohne Tabellen ist kein Grenzfall, sondern ein
    // Programmfehler, und sie sagt das auch so ("TCB-Kapazitaet 0"). Gemessen, nicht ueberlegt.
    for c in 0..num_cores() {
        // **Und `core_online` und nicht nur `num_cores()`** (2026-08-17, Z6 Stufe 1). Der Kommentar
        // darueber beschreibt dieselbe Klasse ein Jahr frueher: damals war die Unterscheidung
        // *konfiguriert* gegen *vorhanden*, jetzt ist sie *konfiguriert* gegen *laufend*. Ein
        // unterdruecktes Geschwister hat Tabellen (`configure` haengt sie an alle konfigurierten
        // Kerne), aber kein `current` -- und `current_id` sagt dazu voellig zu Recht "Programm-
        // fehler". Gemessen unter `-smp cores=2,threads=2`, nicht ueberlegt.
        if !caprock_sched::core_online(c) {
            continue;
        }
        if SCHEDS[c].lock().current_id(c) == tid {
            return Freeze::StillRunning;
        }
    }
    Freeze::Frozen
}

/// Einen eingefrorenen Thread wieder laufen lassen.
///
/// **`resume`, nicht `unblock`** (Z24) — und das ist genau die Naht, für die der Umbau gebaut
/// wurde. `freeze_thread` friert über `pause` ein, hebt also `PAUSE` auf; `unblock` hob früher
/// „die Blockade" auf, gleich welche. Damit riss es einem Thread, der zugleich in IPC wartete oder
/// geparkt war, einen **fremden** Grund weg — und umgekehrt weckte ein `unpark` des Nachbarn eine
/// Einfrier-Entscheidung mit auf. Seit der Grund-Menge ist das nicht mehr formulierbar: `thaw`
/// entfernt `PAUSE`, und wer noch aus einem anderen Grund liegt, bleibt liegen.
pub fn thaw_thread(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| s.resume(tid).then_some(())).is_some()
}

// --- Z23/S3: der Gruppenschnitt -- eine ganze PD, oder keiner ihrer Threads --------------------

/// Wie viele Threads ein Gruppenschnitt fassen kann.
///
/// **Der Ueberlauf ist BENANNT** ([`PdFreeze::TooManyThreads`]) und nicht gekuerzt — D11 woertlich:
/// wer eine Kapazitaet einfuehrt, muss den Ueberlauf benennen, sonst ist die Schranke kein Schutz,
/// sondern ein Loch. Eine stillschweigend abgeschnittene Gruppe waere hier besonders teuer, weil
/// das Ergebnis genau der halbe Schnitt ist, gegen den der ganze Strang gebaut ist.
pub const FREEZE_MAX_THREADS: usize = 32;

/// „kein Endpoint" in [`FrozenPd::recv_ep`]. Ein eigener Wert und kein `Option`, weil das Feld ein
/// festes Feld in einem `Copy`-freien Zeugen ist und `u32::MAX` nie ein gueltiger Index ist.
const KEIN_EP: u32 = u32::MAX;

/// **Der Zeuge eines vollzogenen Gruppenschnitts** (Z23/S3), nach dem Vorbild von
/// [`Parked`](crate::system::Parked).
///
/// ## Was er zusichert
///
/// Wer ihn haelt, haelt eine **Menge**: jeder Thread der PD steht, keiner laeuft, und die
/// Rueckabwicklung ist vollstaendig moeglich. Ein Teilerfolg, der liegen bleibt, waere schlimmer
/// als ein Fehlschlag — die PD waere halb tot, und **jeder Pruefer meldete Ordnung** (D11-Form).
///
/// ## Warum die Bauform genau so ist
///
/// * `#[must_use]` — ein weggeworfener Zeuge ist eine eingefrorene PD ohne Auftauer.
/// * **kein `Drop`** — sonst liesse sich der Inhalt in [`thaw_pd`]/[`abort_freeze`] nicht
///   herausbewegen; dieselbe Ueberlegung wie bei `Parked` und aus demselben Grund.
/// * **kein oeffentlicher Weg an die `ThreadId`s.** Die Namen der Teilnehmer sind dieselbe Sorte
///   Auskunft wie der Partner in [`Freeze::BusyOn`]: sie gehoeren dem Halter der Autoritaet und
///   nicht der Welt. Wer einzelne Threads antasten koennte, koennte die Menge halbieren — und
///   genau das soll der Typ unmoeglich machen.
#[must_use = "ein Gruppenschnitt ohne Auftauer ist eine PD, die nie wieder laeuft"]
pub struct FrozenPd {
    pd: u16,
    n: u8,
    tids: [ThreadId; FREEZE_MAX_THREADS],
    /// Je Teilnehmer: der Endpoint, aus dessen Empfaengerschlange er zurueckgezogen wurde, oder
    /// [`KEIN_EP`]. Ohne diesen Eintrag waere das Auftauen keine Umkehrung — der Thread bliebe
    /// IPC-blockiert an einem Kanal, an dem ihn niemand mehr findet (S6).
    recv_ep: [u32; FREEZE_MAX_THREADS],
    /// Je Teilnehmer: hat **dieser** Schnitt den Endpoint nach innen zugesperrt? Nur dann wird er
    /// beim Auftauen wieder geoeffnet. Ein fremdes `begin_quiesce` (A-4.1, Hot-Reload) darf der
    /// Freeze nicht mit aufheben — das waere der Wecker, der eine fremde Entscheidung mitnimmt.
    ep_gesperrt: [bool; FREEZE_MAX_THREADS],
    /// **Der Tick, zu dem der Schnitt vollzogen war** (Z23/die ZEIT). Zusammen mit dem Tick des
    /// Auftauens ergibt er die Dauer, die diese PD stand — die einzige Groesse, mit der sich eine
    /// vor dem Freeze berechnete Frist hinterher berichtigen laesst.
    tick0: u64,
    /// Der Kern, dessen Uhr `tick0` gelesen hat. Ticks sind je Kern gezaehlt; die Differenz ueber
    /// zwei verschiedene Uhren zu bilden waere eine Zahl ohne Bedeutung.
    tick_kern: usize,
}

impl FrozenPd {
    /// Welche PD steht?
    pub fn pd(&self) -> usize {
        self.pd as usize
    }
    /// Wie viele Threads umfasst der Schnitt? (Sprechprobe: `0` gibt es nicht — [`freeze_pd`]
    /// weist eine leere PD mit [`PdFreeze::Empty`] ab, weil ein leerer Schnitt kein Erfolg ist.)
    pub fn len(&self) -> usize {
        self.n as usize
    }
    /// Ist der Schnitt leer? (Kann nicht vorkommen; steht hier, weil `len` ohne `is_empty` ein
    /// Clippy-Befund ist und eine `#[allow]`-Zeile schlechter waere als die Wahrheit.)
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    /// Wie viele Teilnehmer wurden aus einer Empfaengerschlange gezogen? (Berichtszeile.)
    pub fn zurueckgezogen(&self) -> usize {
        (0..self.len()).filter(|&i| self.recv_ep[i] != KEIN_EP).count()
    }
    /// Wie viele Kanaele hat dieser Schnitt nach innen zugesperrt? (Berichtszeile — und die
    /// Groesse, an der S1b gemessen wird.)
    pub fn kanaele_zu(&self) -> usize {
        (0..self.len()).filter(|&i| self.ep_gesperrt[i]).count()
    }
}

/// Wie ein Gruppenschnitt ausging (Z23/S3). Wie bei [`Freeze`] bewusst **unterscheidbar**: „geht
/// nicht" ist als Diagnose wertlos, und die Faelle verlangen verschiedene Reaktionen.
pub enum PdFreeze {
    /// Die ganze PD steht. Der Zeuge ist der einzige Weg zurueck.
    Frozen(FrozenPd),
    /// Diese PD gibt es nicht.
    NoPd,
    /// Sie hat keine Threads. **Kein Erfolg** — ein leerer Schnitt ist kein Schnitt, und ihn als
    /// `Frozen` zu melden hiesse, einer PD Stillstand zu bescheinigen, die nie lief.
    Empty,
    /// Mehr Threads als der Zeuge fasst. Die Zahl ist die **gebrauchte**, nicht die vorhandene
    /// Kapazitaet — wer die Schranke anheben will, soll den Wert ablesen koennen.
    TooManyThreads(usize),
    /// Ein Debugger haelt einen Teilnehmer (Z6b). Zwei Schreiber auf einem Frame sind kein
    /// Grenzfall; die Absage nennt den Thread, damit klar ist, **wen** man fragen muss.
    Debugged(ThreadId),
    /// Ihre Tore waren schon zu. Entweder laeuft bereits ein Schnitt oder ein Hot-Reload — in
    /// beiden Faellen darf dieser Aufrufer den Zustand nicht fuer seinen halten (dieselbe Regel
    /// wie bei [`Endpoint::begin_quiesce`]).
    AlreadyQuiescing,
    /// Die PD haelt eine DMA-Cap (S5, erste Fassung: **fail-closed**).
    ///
    /// Eine eingefrorene Treiber-PD, deren Geraet gerade in ihre Region schreibt, ist **nicht**
    /// eingefroren: der Deskriptorring laeuft weiter. Bis der Domaenen-Schwenk gebaut ist (S5c,
    /// jede DMA waehrend des Freeze wird ein IOMMU-Fault), ist eine ehrliche Absage besser als
    /// eine Zusage, die nicht haelt.
    HasDma,
    /// Ein Teilnehmer wartet als Empfaenger an **mehr als einem** Kanal. Strukturell unmoeglich
    /// fuer einen in `RECV` blockierten Thread — und genau deshalb eine Absage und keine Schleife:
    /// wenn die Annahme faellt, soll sie **auffallen**, nicht stillschweigend halb behandelt
    /// werden.
    MultiReceiver(ThreadId),
    /// An einem Kanal, an dem ein Teilnehmer als Empfaenger wartet, stehen **Sender an**. Das
    /// widerspricht der tragenden Invariante (Sender und Empfaenger treffen sich sofort). Absage
    /// statt Weiterbau auf einer Annahme, die gerade nachweislich nicht gilt.
    SenderQueued(ThreadId, usize),
    /// **Die Frist ist abgelaufen** — und die Absage nennt, worauf gewartet wurde.
    ///
    /// „Wir warten, bis es ruhig ist" terminiert nicht beweisbar, und eine Stilllegung ohne Frist
    /// ist von einem Deadlock nicht zu unterscheiden. `partner` ist der Thread ausserhalb der PD,
    /// an dem der Schnitt haengt — `None`, wenn der Teilnehmer als **Sender** ansteht und noch gar
    /// keine Gegenseite hat (einen zu erfinden waere schlimmer als keinen zu nennen).
    Deadline {
        tid: ThreadId,
        partner: Option<ThreadId>,
        q: Quiescence,
    },
}

impl PdFreeze {
    /// Ist der Schnitt vollzogen?
    pub fn is_frozen(&self) -> bool {
        matches!(self, PdFreeze::Frozen(_))
    }
    /// Ein kurzer, stabiler Code fuer die Berichtszeile. `0` = Erfolg.
    pub fn code(&self) -> u8 {
        match self {
            PdFreeze::Frozen(_) => 0,
            PdFreeze::NoPd => 1,
            PdFreeze::Empty => 2,
            PdFreeze::TooManyThreads(_) => 3,
            PdFreeze::Debugged(_) => 4,
            PdFreeze::AlreadyQuiescing => 5,
            PdFreeze::HasDma => 6,
            PdFreeze::MultiReceiver(_) => 7,
            PdFreeze::SenderQueued(_, _) => 8,
            PdFreeze::Deadline { .. } => 9,
        }
    }
}

/// Haelt diese PD eine DMA-Cap? (S5, fail-closed.)
fn pd_haelt_dma(pd: usize) -> bool {
    // **Erst die Slots kopieren, dann `CAPS` freigeben** -- und dann fragen. `caps_of` gibt ein
    // Array by value; die Sperre wird also gar nicht fuer die Schleife gebraucht.
    //
    // **`kind_of` und nicht `inspect`:** `inspect` rechnet `child_count`, also einen CDT-Gang mit
    // einer Schranke von `slots.len()` -- je Slot, unter gehaltener Sperre, und `SpinLock` maskiert
    // IRQs. Gemessen: die erste Fassung hat die laengste maskierte Strecke auf aarch64 so weit
    // hochgezogen, dass die STOPP-LATENZ-Zusage des Debuggers fiel. Eine Zeile ohne jeden Bezug
    // zum Freeze, rot gemacht von einer bequemen Abfrage.
    let slots = CAPS.read().pds.caps_of(pd);
    slots.iter().flatten().any(|p| {
        matches!(
            CAPS.read().cspace.kind_of(*p),
            Some(ObjectKind::Dma { .. })
        )
    })
}

/// **Die Interrupt-Notification einer PD eintragen** (B4) — muss VOR dem Endowment stehen, sonst
/// weist die HardwareLand-Cap-Politik die Cap in Slot 8 ab.
pub fn pd_set_irq_ntfn(pd: usize, id: u32) -> bool {
    CAPS.write().pds.set_irq_ntfn(pd, id)
}

/// **Haelt PD `pd` in Slot `slot` eine `Irq`-Cap, und auf welchen Vektor?** (B2, fuer den Pruefer)
///
/// `kind_of` und nicht `inspect` — aus dem Grund, den [`pd_haelt_dma`] daneben ausschreibt: eine
/// Auskunftsfunktion, die nebenbei einen CDT-Gang macht, ist an einer Engstelle kein Komfort,
/// sondern ein Latenzloch, und `SpinLock` maskiert IRQs.
pub fn pd_irq_cap_intid(pd: usize, slot: usize) -> Option<u32> {
    // `caps_of` gibt ein Array **by value** -- die Sperre wird also gar nicht ueber die Abfrage
    // gehalten (s. `pd_haelt_dma` daneben, wo genau das eine Latenzzusage gerissen hat).
    let cap = (*CAPS.read().pds.caps_of(pd).get(slot)?)?;
    match CAPS.read().cspace.kind_of(cap) {
        Some(ObjectKind::Irq { intid }) => Some(intid),
        _ => None,
    }
}

/// **Alle Threads einer PD einsammeln** — der Rohstoff des Schnitts.
///
/// Die Reihenfolge der Sperren ist die Aussage: erst je Kern unter **dessen** Sperre die lebenden
/// `ThreadId`s einsammeln, dann alle Sperren freigeben, **dann** unter `CAPS` nach PD filtern.
/// Andersherum — `pd_of_thread` innerhalb der gehaltenen Scheduler-Sperre — waere `CAPS` (R0)
/// **innerhalb** von `SCHEDS` (R2), also die Umkehrung der Ordnung aus `docs/invariants.md` §1.
fn pd_threads(pd: usize, out: &mut [ThreadId]) -> Result<usize, usize> {
    let mut alle = [ThreadId::from_raw(0); FREEZE_MAX_THREADS * 4];
    let mut m = 0usize;
    for c in 0..num_cores() {
        if !caprock_sched::core_online(c) {
            continue;
        }
        let rest = &mut alle[m..];
        match SCHEDS[c].lock().live_thread_ids(rest) {
            Ok(k) => m += k,
            // Mehr lebende Threads auf einem Kern, als der Sammelpuffer noch fasst. Auch das ist
            // ein **benannter** Ueberlauf und keine gekuerzte Liste.
            Err(gebraucht) => return Err(m + gebraucht),
        }
    }
    let mut n = 0usize;
    for t in &alle[..m] {
        if pd_of_thread(*t) != Some(pd) {
            continue;
        }
        if n >= out.len() {
            // Weiterzaehlen, damit die Absage die **gebrauchte** Groesse nennt.
            n += 1;
            continue;
        }
        out[n] = *t;
        n += 1;
    }
    if n > out.len() {
        return Err(n);
    }
    Ok(n)
}

/// An welchen Endpoints wartet dieser Thread als **Empfaenger**? Gibt `(erster, anzahl)`.
fn recv_endpoints(tid: ThreadId) -> (u32, usize) {
    let mut erster = KEIN_EP;
    let mut n = 0usize;
    for i in 0..eps().len() {
        if eps()[i].lock().quiescence_of(tid).as_receiver {
            if erster == KEIN_EP {
                erster = i as u32;
            }
            n += 1;
        }
    }
    (erster, n)
}

/// **Eine ganze PD einfrieren** (Z23/S3) — der Gruppenschnitt.
///
/// ## Warum das mehr ist als „alle Threads der Reihe nach"
///
/// Treiben zwei Threads derselben PD miteinander IPC, gibt [`freeze_thread`] fuer **beide** `Busy`,
/// und es gibt keine Reihenfolge, die das aufloest. Der Schnitt loest es, weil er die Frage anders
/// stellt: **eine Beziehung, deren beide Enden im Schnitt liegen, ist keine offene Beziehung des
/// Schnitts.** Genau das macht einen Prozess-Freeze moeglich, wo ein Thread-Freeze strukturell
/// scheitert — und es ist der einzige Fall, den dieser Strang zeigt und Z4a nicht schon zeigte.
///
/// ## Die Reihenfolge, und jeder Schritt hat einen Grund
///
/// 1. **Tore nach aussen zu** (S1, am Subjekt): die PD faengt nichts Neues an. Ab hier kann die
///    Menge ihrer offenen Transaktionen nur noch **schrumpfen** — ohne diesen Schritt waere jede
///    Ruhe-Auskunft in dem Moment veraltet, in dem sie zurueckkommt.
/// 2. **Tore nach innen zu** (S1b, am Objekt, aber **nur** wo die PD allein bedient): an jedem
///    Kanal, an dem ein Teilnehmer als einziger Empfaenger wartet, wird `begin_quiesce` gesetzt.
///    Fremde Aufrufer bekommen damit `ERR_QUIESCING` — „kommt gleich wieder" — statt unbegrenzt zu
///    blockieren. **Wo auch ein Fremder bedient, wird nicht zugesperrt**: dort fröre der Riegel
///    Dritte mit ein, und der Fremde bedient die Aufrufer ohnehin weiter.
/// 3. **Urteilen** — und zwar erst jetzt, nach beiden Toren, weil sich der Befund sonst unter der
///    Hand aendern koennte.
/// 4. **Vollziehen**: Empfaenger zurueckziehen, jeden Teilnehmer in den Schnitt aufnehmen.
/// 5. **Nachsehen, nicht glauben**: markiert ist nicht gestoppt. Ein Teilnehmer auf einem fremden
///    Kern laeuft, bis der Reschedule greift — also **erst markieren, dann warten, bis sie die
///    Kerne verlassen haben**. Andersherum terminiert es nicht, weil in der Zwischenzeit einer
///    zurueckkommt.
///
/// ## Die Frist
///
/// `frist_ticks` begrenzt Schritt 5 **und** das Zuwarten auf eine Gegenseite ausserhalb der PD.
/// Laeuft sie ab, wird **alles zurueckgenommen** und die Absage nennt den Thread und seinen
/// Partner. Ohne Frist waere eine Stilllegung von einem Deadlock nicht zu unterscheiden.
///
/// **Alles oder nichts:** jeder Ausgang ausser `Frozen` hinterlaesst den Zustand, den er vorfand —
/// Tore offen, Empfaenger eingereiht, kein Grundbit gesetzt.
pub fn freeze_pd(pd: usize, frist_ticks: u64) -> PdFreeze {
    if !CAPS.read().pds.is_used(pd) {
        return PdFreeze::NoPd;
    }
    // **S5, erste Fassung: fail-closed.** Vor jedem anderen Schritt, damit eine Treiber-PD gar
    // nicht erst halb behandelt wird.
    if pd_haelt_dma(pd) {
        return PdFreeze::HasDma;
    }
    // Schritt 1 -- und der Rueckgabewert ist die Eigentumsfrage: `false` heisst „war schon zu",
    // also laeuft hier bereits ein Schnitt oder ein Hot-Reload. Wer den Zustand nicht gesetzt hat,
    // darf ihn nicht aufheben.
    if !CAPS.write().pds.set_quiescing(pd, true) {
        return PdFreeze::AlreadyQuiescing;
    }
    let ergebnis = freeze_pd_innen(pd, frist_ticks);
    if !ergebnis.is_frozen() {
        // **Vollstaendig zurueck.** Die Tore gehoerten diesem Aufruf; sie gehen mit ihm.
        CAPS.write().pds.set_quiescing(pd, false);
    }
    ergebnis
}

/// Der Rumpf von [`freeze_pd`], hinter der Torentscheidung. Getrennt, damit es **einen** Ort gibt,
/// an dem die Tore wieder aufgehen — eine Rueckgabe mitten im Rumpf, die das vergisst, waere genau
/// der halbe Zustand, gegen den der Zeuge gebaut ist.
fn freeze_pd_innen(pd: usize, frist_ticks: u64) -> PdFreeze {
    let kern = hal::cpu::core_id();
    let t0 = hal::timer::ticks(kern);
    loop {
        let mut tids = [ThreadId::from_raw(0); FREEZE_MAX_THREADS];
        let n = match pd_threads(pd, &mut tids) {
            Ok(0) => return PdFreeze::Empty,
            Ok(k) => k,
            Err(gebraucht) => return PdFreeze::TooManyThreads(gebraucht),
        };

        // -- Schritt 2: die Kanaele, an denen diese PD ALLEIN bedient, nach innen zusperren -----
        let mut recv_ep = [KEIN_EP; FREEZE_MAX_THREADS];
        let mut ep_gesperrt = [false; FREEZE_MAX_THREADS];
        let mut abbruch: Option<PdFreeze> = None;
        for i in 0..n {
            let (ep, anzahl) = recv_endpoints(tids[i]);
            if anzahl > 1 {
                abbruch = Some(PdFreeze::MultiReceiver(tids[i]));
                break;
            }
            recv_ep[i] = ep;
            if ep == KEIN_EP {
                continue;
            }
            let mut e = eps()[ep as usize].lock();
            if e.sender_count() > 0 {
                // Sprechprobe der tragenden Invariante -- gefragt, nicht geglaubt.
                abbruch = Some(PdFreeze::SenderQueued(tids[i], e.sender_count()));
                drop(e);
                break;
            }
            if e.receiver_count() == 1 {
                ep_gesperrt[i] = e.begin_quiesce();
            }
        }
        if let Some(a) = abbruch {
            tore_auf(&recv_ep, &ep_gesperrt, n);
            return a;
        }

        // -- Schritt 3: urteilen, jetzt wo sich nichts mehr unter der Hand aendert --------------
        let mut hindernis: Option<(ThreadId, Option<ThreadId>, Quiescence)> = None;
        let mut debugged: Option<ThreadId> = None;
        for i in 0..n {
            let t = tids[i];
            if with_owner(t, |s, _| s.is_debug_stopped(t).then_some(())).is_some() {
                debugged = Some(t);
                break;
            }
            let q = thread_quiescence(t);
            if q.is_quiescent() {
                continue;
            }
            // **Ein wartender Empfaenger haelt niemanden fest.** Er wartet auf Arbeit; er wird
            // zurueckgezogen und beim Auftauen wieder eingereiht.
            if q.as_receiver && !q.as_sender && !q.as_caller && !q.as_reply_owner {
                continue;
            }
            match thread_partner(t) {
                // **Die Kernaussage des Schnitts:** liegt die Gegenseite in derselben PD, verlaesst
                // die Beziehung den Schnitt nicht. Beide werden gemeinsam eingefroren, das
                // Reply-Token bleibt unangetastet, und nach dem Auftauen laeuft die Transaktion
                // weiter, als waere nichts gewesen.
                Some(p) if pd_of_thread(p) == Some(pd) => continue,
                p => {
                    hindernis = Some((t, p, q));
                    break;
                }
            }
        }
        if let Some(t) = debugged {
            tore_auf(&recv_ep, &ep_gesperrt, n);
            return PdFreeze::Debugged(t);
        }
        if let Some((t, p, q)) = hindernis {
            if hal::timer::ticks(kern).wrapping_sub(t0) >= frist_ticks {
                tore_auf(&recv_ep, &ep_gesperrt, n);
                return PdFreeze::Deadline {
                    tid: t,
                    partner: p,
                    q,
                };
            }
            // Noch Zeit: alles zurueck, was dieser Durchgang gesetzt hat, und neu ansetzen. Die
            // Gegenseite darf ihre Transaktion abschliessen -- `REPLY` ist durch das Tor
            // ausdruecklich nicht gesperrt.
            tore_auf(&recv_ep, &ep_gesperrt, n);
            warte_einen_tick(kern);
            continue;
        }

        // -- Schritt 4: vollziehen ---------------------------------------------------------------
        for i in 0..n {
            if recv_ep[i] != KEIN_EP {
                eps()[recv_ep[i] as usize].lock().retire_receiver(tids[i]);
            }
            with_owner(tids[i], |s, _| s.freeze_group(tids[i]).then_some(()));
        }

        // -- Schritt 5: nachsehen, nicht glauben -------------------------------------------------
        //
        // Markiert heisst deplant, nicht gestoppt. Ab hier kann kein Teilnehmer mehr zurueck in
        // eine Ready-Queue (die Grund-Menge ist nicht leer) und keiner mehr den Kern wechseln
        // (`detach_for_migration` weist `FREEZE` ab) -- die Schleife terminiert also, sobald die
        // laufenden ihren naechsten Trap nehmen.
        loop {
            let mut laeuft = false;
            for c in 0..num_cores() {
                if !caprock_sched::core_online(c) {
                    continue;
                }
                let cur = SCHEDS[c].lock().current_id(c);
                if (0..n).any(|i| tids[i] == cur) {
                    laeuft = true;
                    break;
                }
            }
            if !laeuft {
                break;
            }
            if hal::timer::ticks(kern).wrapping_sub(t0) >= frist_ticks {
                // Frist abgelaufen, waehrend noch jemand lief: **vollstaendig** zurueck.
                let zeuge = FrozenPd {
                    pd: pd as u16,
                    n: n as u8,
                    tids,
                    recv_ep,
                    ep_gesperrt,
                    tick0: hal::timer::ticks(kern),
                    tick_kern: kern,
                };
                abort_freeze_innen(&zeuge);
                return PdFreeze::Deadline {
                    tid: tids[0],
                    partner: None,
                    q: Quiescence::default(),
                };
            }
            warte_einen_tick(kern);
        }

        FREEZE_PD_OK.fetch_add(1, Ordering::Relaxed);
        FREEZE_PD_THREADS.fetch_add(n as u64, Ordering::Relaxed);
        return PdFreeze::Frozen(FrozenPd {
            pd: pd as u16,
            n: n as u8,
            tids,
            recv_ep,
            ep_gesperrt,
            tick0: hal::timer::ticks(kern),
            tick_kern: kern,
        });
    }
}

/// Die von **diesem** Durchgang gesperrten Kanaele wieder oeffnen. Nur die eigenen — ein fremdes
/// `begin_quiesce` bleibt stehen.
fn tore_auf(recv_ep: &[u32; FREEZE_MAX_THREADS], gesperrt: &[bool; FREEZE_MAX_THREADS], n: usize) {
    for i in 0..n {
        if gesperrt[i] && recv_ep[i] != KEIN_EP {
            eps()[recv_ep[i] as usize].lock().end_quiesce();
        }
    }
}

/// Einen Tick verstreichen lassen, ohne eine Sperre zu halten.
///
/// **In Ticks und nicht in Runden**: eine Zaehlschleife misst die Geschwindigkeit des Wartenden,
/// nicht den Fortschritt der anderen. Die Wache daneben ist kein Ersatz fuer die Frist, sondern
/// der Schutz gegen eine stehende Uhr — ohne sie waere ein ausgefallener Timer ein Haenger statt
/// eines Befundes.
fn warte_einen_tick(kern: usize) {
    let t = hal::timer::ticks(kern);
    let mut wache: u64 = 0;
    while hal::timer::ticks(kern) == t && wache < 200_000_000 {
        core::hint::spin_loop();
        wache += 1;
    }
}

/// **Einen Gruppenschnitt auftauen** (Z23/S6) — die Umkehrung, und sie ist nicht bloss „Grund weg".
///
/// Drei Dinge in dieser Reihenfolge, und die Reihenfolge ist die Aussage:
/// 1. **Empfaenger wieder einreihen**, bevor irgendein Kanal wieder aufgeht. Andersherum koennte
///    ein Aufrufer den Kanal treffen, waehrend der Server noch nicht drinsteht — und in der
///    Senderschlange landen, statt bedient zu werden.
/// 2. **Kanaele oeffnen** — und nur die, die dieser Schnitt zugesperrt hat.
/// 3. **Den Grund entfernen.** `FREEZE` und nur `FREEZE`: wer zusaetzlich in IPC wartet, pausiert
///    wurde oder auf leerem Konto sitzt, bleibt liegen. Sein Wecker ist ein anderer.
///
/// Rueckgabe: wie viele Threads den Grund tatsaechlich verloren haben — **nach Wirkung gezaehlt**,
/// nicht nach Versuch.
pub fn thaw_pd(z: FrozenPd) -> Thawed {
    let t = Thawed {
        threads: auftauen(&z),
        ticks: hal::timer::ticks(z.tick_kern).wrapping_sub(z.tick0),
    };
    CAPS.write().pds.set_quiescing(z.pd as usize, false);
    THAW_PD_OK.fetch_add(1, Ordering::Relaxed);
    FROZEN_TICKS_TOTAL.fetch_add(t.ticks, Ordering::Relaxed);
    t
}

/// **Einen Gruppenschnitt abbrechen** (Z23/S3) — derselbe Weg zurueck wie [`thaw_pd`].
///
/// Eigene Funktion und nicht ein Alias, obwohl der Rumpf derselbe ist: die beiden bedeuten
/// Verschiedenes (*„fertig, weiterlaufen"* gegen *„es hat nicht geklappt"*), und der Zaehler
/// trennt sie. Dass der **Weg** derselbe ist, ist das Ergebnis der Grund-Menge aus Z24 — mit einem
/// eigenen Grundbit ist der Abbruch ein `remove` je Teilnehmer, und es gibt keinen
/// Zwischenzustand, der rueckwaerts zu durchlaufen waere. Genau deshalb stand im Register
/// „Z24 vor Z23".
pub fn abort_freeze(z: FrozenPd) -> usize {
    let n = auftauen(&z);
    FROZEN_TICKS_TOTAL.fetch_add(
        hal::timer::ticks(z.tick_kern).wrapping_sub(z.tick0),
        Ordering::Relaxed,
    );
    CAPS.write().pds.set_quiescing(z.pd as usize, false);
    FREEZE_PD_ABORT.fetch_add(1, Ordering::Relaxed);
    n
}

/// Der Abbruch **innerhalb** von [`freeze_pd_innen`], wo die Tore noch dem laufenden Aufruf
/// gehoeren und von `freeze_pd` selbst geoeffnet werden.
fn abort_freeze_innen(z: &FrozenPd) -> usize {
    FREEZE_PD_ABORT.fetch_add(1, Ordering::Relaxed);
    auftauen(z)
}

fn auftauen(z: &FrozenPd) -> usize {
    let n = z.len();
    for i in 0..n {
        if z.recv_ep[i] != KEIN_EP {
            eps()[z.recv_ep[i] as usize].lock().bind_receiver(z.tids[i]);
        }
    }
    for i in 0..n {
        if z.ep_gesperrt[i] && z.recv_ep[i] != KEIN_EP {
            eps()[z.recv_ep[i] as usize].lock().end_quiesce();
        }
    }
    let mut geweckt = 0usize;
    for i in 0..n {
        let t = z.tids[i];
        if let Some((_, c)) = with_owner(t, |s, _| s.thaw_group(t).then_some(())) {
            geweckt += 1;
            kick(c);
        }
    }
    geweckt
}

/// Wie viele Threads stehen **systemweit** im Gruppenschnitt? (Sprechprobe der `pdfreeze`-Zeile:
/// ein Pruefer, der ueber die Wirkung eines Schnitts urteilt, muss belegen koennen, dass ueberhaupt
/// geschnitten wurde.)
pub fn group_frozen_total() -> usize {
    (0..num_cores())
        .filter(|&c| caprock_sched::core_online(c))
        .map(|c| SCHEDS[c].lock().group_frozen_count())
        .sum()
}

/// Steht dieser Thread im Gruppenschnitt?
pub fn thread_is_group_frozen(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| s.is_group_frozen(tid).then_some(())).is_some()
}

/// **Was ein Auftauen ergeben hat** (Z23) — Wirkung **und** Dauer.
///
/// ## Die Entscheidung zur ZEIT, und sie faellt hier statt spaeter implizit
///
/// **Die monotone Uhr pausiert NICHT mit.** Ein aufgetauter Thread sieht sie springen, und jede
/// Frist, die er vor dem Freeze berechnet hat, ist danach falsch. Die naheliegende Gegenmassnahme —
/// eine Uhr je PD — ist verworfen, und zwar aus drei Gruenden:
///
/// 1. Sie waere ein **zweites Gedaechtnis fuer eine Tatsache**. Der Kernel rechnet MCS-Budgets,
///    Perioden und Zyklenstempel auf der globalen Uhr; eine zweite daneben laufen zu lassen heisst,
///    sie irgendwann auseinanderlaufen zu lassen.
/// 2. Ein per IPC empfangener Zeitstempel einer **nicht** eingefrorenen PD laege dann in der
///    Zukunft der eigenen Uhr. Aus „die Uhr springt" wuerde „fremde Zeitstempel sind unbrauchbar" —
///    ein schlechterer Tausch.
/// 3. EL0 liest die Uhr **direkt** (`rdtsc` / `CNTVCT_EL0`). Eine Kernel-Uhr, die pausiert, waere
///    von der Hardware-Uhr, die der Thread tatsaechlich liest, ohnehin nicht gedeckt — der Kernel
///    kann diese Zusage gar nicht einloesen.
///
/// **Also: die Uhr laeuft, und die Dauer wird BERICHTET.** Sie geht an den Halter der
/// Freeze-Autoritaet — dieselbe Regel wie beim Partnernamen in [`Freeze::BusyOn`], und aus
/// demselben Grund: wer eingefroren hat, weiss ohnehin, dass und wie lange.
///
/// **Was damit NICHT geloest ist, und es steht in `todo.md`:** die eingefrorene PD selbst erfaehrt
/// die Dauer nicht — dafuer braeuchte es einen Selbstauskunfts-Syscall. Ein Thread, der vor dem
/// Freeze eine Frist aus `rdtsc` gerechnet hat, kann sie danach also **nicht** berichtigen. Das ist
/// eine benannte Luecke und kein Nebeneffekt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Thawed {
    /// Wie viele Threads den Grund tatsaechlich verloren haben — **nach Wirkung** gezaehlt.
    pub threads: usize,
    /// Wie lange die PD stand, in Ticks der Uhr, die auch den Schnitt gestempelt hat.
    pub ticks: u64,
}

/// Summe aller Standzeiten (Bericht). `0` bei einem Lauf ohne Schnitt ist von „nie gemessen" nicht
/// zu unterscheiden — deshalb steht [`FREEZE_PD_OK`] daneben.
pub static FROZEN_TICKS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Vollzogene Gruppenschnitte.
pub static FREEZE_PD_OK: AtomicU64 = AtomicU64::new(0);
/// Summe der dabei eingefrorenen Threads — **nach Wirkung**, nicht nach Versuch.
pub static FREEZE_PD_THREADS: AtomicU64 = AtomicU64::new(0);
/// Abgebrochene Schnitte (Frist abgelaufen, waehrend schon markiert war).
pub static FREEZE_PD_ABORT: AtomicU64 = AtomicU64::new(0);
/// Aufgetaute Schnitte.
pub static THAW_PD_OK: AtomicU64 = AtomicU64::new(0);

/// **Einen geparkten Thread vom Kernel aus wecken** (Z22, P4) — dieselbe Operation wie
/// [`sys::UNPARK`](caprock_abi::sys::UNPARK), nur ohne die PD-Prüfung, die dort die Autorität
/// des Aufrufers begrenzt. Für Prüfpfade und für den IRQ-Weg.
pub fn unpark_thread(tid: ThreadId) -> bool {
    if let Some((_, c)) = with_owner(tid, |s, _| s.unpark(tid).then_some(())) {
        kick(c);
        return true;
    }
    false
}

/// **Dem Aufrufer eines `SYS_LOAD` antworten** (C8) — Ergebnis in den Frame, dann `LOAD` weg.
///
/// Die Reihenfolge ist die Aussage: **erst schreiben, dann wecken.** Umgekehrt liefe der Aufrufer
/// mit einem Ergebnisregister los, das noch niemand gesetzt hat. Beides passiert unter *einer*
/// Sperrung seiner Scheduler-Instanz — der Frame gehört einem blockierten Thread, und blockiert
/// bleibt er, bis `load_reply` in derselben Sperrung den Grund entfernt.
///
/// Dieselbe Bauform wie [`unblock_with_error`], nur mit dem Grund, der hierher gehört: `unblock`
/// entfernte `IPC`, und ein Aufrufer, der nie in IPC war, hätte davon nichts gemerkt — er wäre
/// blockiert liegen geblieben.
///
/// Rückgabe: ob der Aufrufer noch auflösbar war. `false` heisst „tot" und ist kein Fehler, aber
/// auch nicht nichts (der Verifizierer zählt es).
pub fn lade_antwort(caller: ThreadId, pd: Option<usize>) -> bool {
    let done = with_owner(caller, |sched, _| {
        let frame = sched.frame_of(caller)?; // fremder Kern/tot -> ggf. wiederholen
        match pd {
            Some(p) => {
                hal::exception::frame_set_reg(
                    frame,
                    caprock_abi::reg::SYSNO_RESULT,
                    caprock_abi::result::OK,
                );
                hal::exception::frame_set_reg(frame, caprock_abi::reg::EP_BADGE, p as u64);
            }
            // Wie bisher: ein fehlgeschlagener Ladevorgang ist `ERR_BADCAP` (Archiv/ELF/
            // Ressourcen). Der GRUND steht im Protokoll, den `loader::load_by_index` druckt.
            None => hal::exception::frame_set_reg(
                frame,
                caprock_abi::reg::SYSNO_RESULT,
                caprock_abi::result::ERR_BADCAP,
            ),
        }
        sched.load_reply(caller);
        Some(())
    });
    match done {
        Some((_, c)) => {
            kick(c);
            true
        }
        None => false,
    }
}

/// **Einen `SYS_LOAD` an den Verifizierer übergeben** (C8) — der Callback des Dispatch.
///
/// Blockiert den laufenden Thread mit [`BlockReasons::LOAD`](caprock_sched::BlockReasons::LOAD)
/// und reicht den Auftrag hinüber; die Reihenfolge (blockieren, *dann* veröffentlichen) liegt in
/// [`crate::verifizierer::uebergeben`] und ist dort begründet.
fn sys_load_uebergeben(
    index: u32,
    caller_pd: usize,
    endow: &[(usize, CapPtr)],
    core: usize,
    frame: usize,
    cap_budget: u16,
    dma_pages: u32,
) -> crate::verifizierer::Uebergabe {
    let caller = SCHEDS[core].lock().current_id(core);
    crate::verifizierer::uebergeben(index, caller_pd, endow, caller, core, cap_budget, dma_pages, || {
        // Genau der Pfad, den die Tick-Rechnung sonst nicht sieht (B-5.1): wer blockiert, hat
        // gerechnet. `charged` stempelt die verbrauchten Zyklen, bevor gewechselt wird.
        charged(core, &mut SCHEDS[core].lock(), |s| {
            s.block_for_load(core, frame)
        })
    })
}

/// **Einen `SYS_LOAD_IMAGE` an den Verifizierer übergeben** (LXPD-Laufzeit) — der
/// `load_image`-Callback des Dispatch.
///
/// Spiegel von [`sys_load_uebergeben`]: derselbe Blockierpfad (`block_for_load`, mit
/// `charged`-Abrechnung nach B-5.1), derselbe Verifizierer, dieselben drei Ausgänge. Der
/// einzige Unterschied ist die Bildquelle: kein Archiv-Index, sondern `(bild_phys, bild_len,
/// pid)` aus Aufrufer-RAM — die Geometrie (plain-RAM, READ-Recht, Länge gedeckt) hat der
/// Dispatch bereits gegen die Aufrufer-Memory-Cap geprüft.
fn sys_load_image_uebergeben(
    bild_phys: u64,
    bild_len: u64,
    pid: u32,
    caller_pd: usize,
    endow: &[(usize, CapPtr)],
    core: usize,
    frame: usize,
    cap_budget: u16,
    dma_pages: u32,
) -> crate::verifizierer::Uebergabe {
    let caller = SCHEDS[core].lock().current_id(core);
    crate::verifizierer::uebergeben_bild(
        bild_phys,
        bild_len,
        pid,
        caller_pd,
        endow,
        caller,
        core,
        cap_budget,
        dma_pages,
        || {
            // Derselbe Pfad wie nebenan: wer blockiert, hat gerechnet (B-5.1).
            charged(core, &mut SCHEDS[core].lock(), |s| {
                s.block_for_load(core, frame)
            })
        },
    )
}

/// **Staging-Puffer für `SYS_LOAD_IMAGE`** (LXPD-Laufzeit): Kernel-RAM für die EINMALIGE
/// Kopie des Aufrufer-Bilds, aus der danach Hash, Parse und Laden lesen.
///
/// Reiner Kernel-Speicher über [`mem_alloc_anywhere`] (Zone `Anywhere` — bevorzugt oberhalb
/// 4 GiB): der Puffer wird NIE in eine PD abgebildet und nie einem Gerät gezeigt, also darf
/// er überall liegen. Kein BSS-Monster: bis zu 4 MiB (`LXPD_MAX_BILD`) stehen nicht im Image,
/// sondern kommen aus `MEM`. Gibt `(Basis, Länge)` — die Länge ist die der Cap (aufgerundet
/// möglich, s. [`staging_free`]); `None` heisst RAM erschöpft, und der Ladevorgang schlägt
/// dann benannt fehl, statt ohne Kopie zu laden.
pub(crate) fn staging_alloc(len: u64) -> Option<(u64, u64)> {
    let cap = mem_alloc_anywhere(len, 8)?;
    Some((cap.base(), cap.len()))
}

/// Einen Staging-Puffer aus [`staging_alloc`] an `MEM` zurückgeben.
///
/// Paarig: JEDER `SYS_LOAD_IMAGE`-Ladevorgang gibt hier zurück — erfolgreich wie abgebrochen —,
/// sonst leckt pro Laufzeit-Treiber bis zu 4 MiB. `len` muss die von [`staging_alloc`]
/// gelieferte Cap-Länge sein, nicht die Bildlänge (der Allokator rundet auf).
pub(crate) fn staging_free(base: u64, len: u64) {
    MEM.lock().free_region(PhysRegion::new(base, len));
}

/// Wartet dieser Thread auf den Verifizierer? (C8, nur Prüfung — die `verif`-Zeile braucht die
/// Aussage in **beide** Richtungen: die Bedienten warten, der Überläufer nicht.)
pub fn is_load_blocked(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| s.is_load_blocked(tid).then_some(())).is_some()
}

/// Ist dieser Thread blockiert, **gleich aus welchem Grund**? Die Größe, an der sich zeigt, ob
/// `unpark` eine fremde Blockade aufgehoben hat — `is_parked` kann das nicht sagen.
/// **Den Scheduler nach einem Thread fragen** (2026-08-10) — die einzige Auskunft über eine PD,
/// die **keinen** Cap-Pfad benutzt.
///
/// Gibt `(existiert, zugelassen, Grund-Bits)`. Eine PD, deren Signal nicht ankommt, lässt zwei
/// grundverschiedene Lagen zu: *läuft nie an* (Lader/Scheduler) gegen *läuft, und das Signal
/// versandet* (Cap-Pfad). Jede Meldung über eine Cap kann diese Frage nicht beantworten — sie
/// benutzt genau den Pfad, der in Frage steht.
pub fn thread_lage(tid: ThreadId) -> (bool, bool, u16) {
    match with_owner(tid, |s, _| {
        Some((
            s.admitted_of(tid).unwrap_or(false),
            s.reasons_of(tid).map(|r| r.bits()).unwrap_or(0),
        ))
    }) {
        Some(((adm, bits), _)) => (true, adm, bits),
        None => (false, false, 0),
    }
}

pub fn is_blocked(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| Some(s.is_blocked(tid)))
        .map(|(b, _)| b)
        .unwrap_or(false)
}

/// Schläft dieser Thread **wegen `PARK`**? (Prüfpfad: eine IPC-Blockade sieht von aussen
/// genauso aus, und genau die Verwechslung ist der D9-Fehler.)
pub fn is_parked(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| Some(s.is_parked(tid)))
        .map(|(b, _)| b)
        .unwrap_or(false)
}

/// **Die Prüfzeile `handler`** (Z26/A3). Liegt in einer eigenen Datei und wird von hier aus
/// eingehängt statt aus `main.rs`: die Messung gehört zum Primitiv, und ein `mod` im Wurzelmodul
/// wäre eine Änderung an einer Datei, die dieser Strang sonst nicht anfasst.
#[cfg(feature = "selftest")]
#[path = "handlermess.rs"]
pub mod handlermess;

// -- Z26/A3: Prüfpfad für die Handler-Bindung ------------------------------------------

/// Wartet dieser Thread auf seine **Persönlichkeits-PD** (`BlockReasons::HANDLER`)?
///
/// Getrennt von [`is_blocked`] und [`is_parked`] aus demselben Grund, aus dem es die Grund-Menge
/// gibt: von aussen sehen alle drei Blockaden gleich aus, und ein Prüfer, der die falsche Größe
/// liest, kann den Fehler, gegen den er gebaut ist, strukturell nicht sehen (der `park`-Befund
/// vom 2026-08-09).
pub fn is_handler_blocked(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| Some(s.is_handler_blocked(tid)))
        .map(|(b, _)| b)
        .unwrap_or(false)
}

/// Die rohe Grund-Menge eines Threads (Z24) — für den Bericht, nicht für Entscheidungen.
pub fn reasons_bits(tid: ThreadId) -> Option<u16> {
    with_owner(tid, |s, _| Some(s.reasons_of(tid))).and_then(|(r, _)| r.map(|x| x.bits()))
}

/// Dem Thread den Handler-Grund anhängen (Prüfpfad; im Betrieb macht das der Dispatch).
pub fn mark_handler_wait(tid: ThreadId) -> bool {
    with_owner(tid, |s, _| s.mark_handler_wait(tid).then_some(())).is_some()
}

/// **Der einzige Wecker des Handler-Grundes**, auch für den Prüfpfad.
///
/// Geht durch **denselben** [`handler_reply_mit_frame`] wie der Betriebspfad — und das ist keine
/// Bequemlichkeit: ein Prüfpfad, der das Zurückschreiben auslässt, prüfte einen Ablauf, den es im
/// Betrieb nicht gibt, und die Reihenfolge „erst Frame, dann Wecker" stünde an zwei Stellen.
pub fn handler_reply(tid: ThreadId) -> bool {
    if let Some((_, c)) = with_owner(tid, |s, _| handler_reply_mit_frame(s, tid)) {
        kick(c);
        return true;
    }
    false
}

/// Eine **Pause** aufheben (Prüfpfad) — `resume` entfernt `PAUSE` und nur das.
pub fn resume_thread(tid: ThreadId) -> bool {
    if let Some((_, c)) = with_owner(tid, |s, _| s.resume(tid).then_some(())) {
        kick(c);
        return true;
    }
    false
}

/// **Die Gruende, aus denen ein Thread blockiert ist** (Pruefpfad, A2n).
///
/// `None` heisst „nicht aufloesbar", `Some(leer)` heisst „laeuft" -- die Unterscheidung, die ein
/// `bool` verliert. Die A2n-Sonde braucht genau sie: nach einer gefeuerten Frist muss `IPC` weg
/// und ein danebenstehender Grund **stehen** sein, und „beide weg" von „nie gesetzt" zu trennen
/// geht nur ueber die Menge.
pub fn reasons_of(tid: ThreadId) -> Option<caprock_sched::BlockReasons> {
    with_owner(tid, |s, _| s.reasons_of(tid)).map(|(r, _)| r)
}

/// **Steht an diesem Thread noch eine Frist?** (Pruefpfad, A2n.)
///
/// `0` heisst „keine". Ohne diese Groesse waere „der Thread lief nicht" auch dann wahr, wenn die
/// Frist nie gefeuert haette -- ein Pruefer, der nicht scheitern kann (D18).
pub fn frist_von(tid: ThreadId) -> u64 {
    with_owner(tid, |s, _| Some(s.frist_von(tid))).map_or(0, |(f, _)| f)
}

/// Einen Thread pausieren (Prüfpfad).
pub fn pause_thread(tid: ThreadId) -> bool {
    if let Some((_, c)) = with_owner(tid, |s, _| s.pause(tid).then_some(())) {
        kick(c);
        return true;
    }
    false
}

/// Die Handler-Bindung eines Threads setzen/aufheben (Prüfpfad; im Betrieb `SYS_SETHANDLER`).
pub fn set_handler(tid: ThreadId, b: Option<caprock_sched::redirect::Bindung>) -> bool {
    with_owner(tid, |s, _| s.set_handler(tid, b).then_some(())).is_some()
}

/// Wie viele Threads dieses Kerns sind gebunden? (Sprechprobe.)
pub fn handler_bound_count(core: usize) -> usize {
    SCHEDS[core].lock().handler_bound_count()
}

/// **Die Handler-Kante einer PD im Graphen setzen/lösen** (Prüfpfad für das Zyklusverbot).
pub fn handler_kante_setzen(gast: usize, handler: u16) -> bool {
    CAPS.write().pds.handler_kante_setzen(gast, handler)
}

/// Gegenstück zu [`handler_kante_setzen`].
pub fn handler_kante_loesen(gast: usize) -> bool {
    CAPS.write().pds.handler_kante_loesen(gast)
}

/// Das Urteil über eine gewünschte Bindung — **gegen die echte PD-Tabelle**, nicht gegen ein
/// Modell davon. Genau der Aufruf, den `SYS_SETHANDLER` macht.
pub fn pruefe_bindung(
    gast_pd: u16,
    handler_pd: u16,
    hat_syscall: bool,
    hat_fault: bool,
) -> caprock_sched::redirect::BindUrteil {
    let g = CAPS.read();
    let n = g.pds.pd_capacity();
    let frei = g.pds.sidecar_frei(handler_pd as usize);
    caprock_sched::redirect::pruefe_bindung(
        gast_pd,
        handler_pd,
        hat_syscall,
        hat_fault,
        |p| g.pds.handler_pd_of(p as usize),
        n,
        frei,
    )
}

/// **Ein Sidecar-Fenster anlegen und eine `SyscallHandler`-Cap darauf prägen** (Z26/A3).
///
/// Bis zum 2026-08-13 gab es **keinen** Pfad, der eine Handler-Cap prägt — `SYS_SETHANDLER` konnte
/// damit nie erfolgreich sein, und die Prüfzeile `handler` mass das Primitiv über den
/// Kernel-Prüfpfad statt über einen echten Gast.
///
/// **Das Fenster muss ALLE Slots decken**, die die Belegungsmaske vergeben kann
/// (`caprock_microkit::SIDECAR_SLOTS` × `SLOT_BYTES` = 32 KiB). Die Prüfung steht hier und noch
/// einmal an der Bindung, und das ist Absicht: die Maskenbreite und die Fensterlänge waren bis
/// heute **zwei unabhängige Zahlen**, und `redirect::fenster_deckt` — die Funktion, die genau
/// diesen Off-by-one abfängt und dafür einen Host-Test und eine Mutation hat — hatte im ganzen
/// Baum **keinen Aufrufer**.
///
/// Die Handler-Cap selbst hält **keinen** Allokator-Eintrag: das Fenster wird über eine getrennte
/// `Memory`-Cap vergeben, und genau das ist die Begründung der Sidecar-Form — die Autorität über
/// den Registerzustand fremder Threads steht in der **Speicherbuchhaltung** und nicht nur im
/// Cap-Audit.
///
/// Diese Funktion liefert die **nötige Fenstergrösse**; das Anlegen macht der Aufrufer.
pub fn sidecar_fenster_bytes() -> u64 {
    caprock_microkit::SIDECAR_SLOTS as u64 * caprock_sched::redirect::SLOT_BYTES as u64
}

/// Eine `SyscallHandler`-Cap auf ein bestehendes Fenster prägen.
pub fn install_syscall_handler_cap(
    ep: u32,
    pd: u16,
    sidecar: u64,
    len: u64,
    rights: Rights,
) -> Result<CapPtr, CapError> {
    if sidecar == 0 || !caprock_sched::redirect::fenster_deckt(caprock_microkit::SIDECAR_SLOTS, len)
    {
        return Err(CapError::ZuKlein);
    }
    CAPS.write()
        .cspace
        .install_syscall_handler(ep, pd, sidecar, len, rights)
}

/// Eine `FaultHandler`-Cap auf ein bestehendes Fenster prägen.
pub fn install_fault_handler_cap(
    ep: u32,
    pd: u16,
    sidecar: u64,
    len: u64,
    rights: Rights,
) -> Result<CapPtr, CapError> {
    if sidecar == 0 || !caprock_sched::redirect::fenster_deckt(caprock_microkit::SIDECAR_SLOTS, len)
    {
        return Err(CapError::ZuKlein);
    }
    CAPS.write()
        .cspace
        .install_fault_handler(ep, pd, sidecar, len, rights)
}

/// Freie Sidecar-Slots bei einer Handler-PD.
pub fn sidecar_frei(handler_pd: usize) -> u16 {
    CAPS.read().pds.sidecar_frei(handler_pd)
}

/// Einen Sidecar-Slot belegen / freigeben (Prüfpfad).
pub fn sidecar_belegen(handler_pd: usize) -> Option<u16> {
    CAPS.write().pds.sidecar_belegen(handler_pd)
}

/// Gegenstück zu [`sidecar_belegen`].
pub fn sidecar_freigeben(handler_pd: usize, slot: u16) -> bool {
    CAPS.write().pds.sidecar_freigeben(handler_pd, slot)
}

// -- A-4.1: atomares Umbinden ----------------------------------------------------------

/// **Die Server-Instanz eines Endpoints austauschen — Prüfung und Tausch unter EINEM Lock**
/// (A-4.1). Siehe [`Endpoint::rebind_server`] für die Begründung; hier zählt nur, dass es
/// **ein** `lock()` ist. Aus `endpoint_retire_receiver` + einem späteren `RECV` der neuen
/// Instanz zusammengesetzt, läge zwischen beiden ein Zustand ohne Empfänger — und ein `CALL`
/// darin wartet auf einen Server, dessen Existenz vom Gelingen des restlichen Austauschs
/// abhängt.
pub fn endpoint_rebind_server(ep: usize, old: ThreadId, new: ThreadId) -> Rebind {
    if ep >= eps().len() {
        return Rebind::NoEndpoint;
    }
    eps()[ep].lock().rebind_server(old, new)
}

/// **Eine Empfänger-Instanz binden, ohne dass sie selbst `RECV` ruft** (A-4.1). Vorbedingung:
/// `tid` ist blockiert geparkt — siehe [`Endpoint::bind_receiver`]. Der überlappende Weg
/// (`new` ruft sein `RECV` **vor** der Stilllegung) braucht diese Vorbedingung nicht.
pub fn endpoint_bind_receiver(ep: usize, tid: ThreadId) -> bool {
    ep < eps().len() && eps()[ep].lock().bind_receiver(tid)
}

/// **A-4.1-Selbsttest: die Torlogik des atomaren Umbindens, alle Ausgänge.**
///
/// Aus demselben Grund am lokalen Objekt wie [`run_quiesce`]: der Test darf keinen benutzten
/// Endpoint stilllegen. Die Empfängerqueue lässt sich hier — anders als Reply-Token und
/// Sender — über [`Endpoint::bind_receiver`] füllen, also sind **alle** Ausgänge erreichbar,
/// auch der überlappende Erfolgsfall. Nicht erreichbar bleiben `senders_waiting` und
/// `reply_open`: beide entstehen nur in `call`/`recv` und werden am echten offenen Call im
/// `rmig`-Szenario abgenommen.
#[cfg(feature = "selftest")]
pub fn run_rebind() -> bool {
    use caprock_ipc::RebindBlocked;
    let v1 = ThreadId::from_raw(0xA41_0001);
    let v2 = ThreadId::from_raw(0xA41_0002);
    let fremd = ThreadId::from_raw(0xA41_0003);
    let mut e = Endpoint::EMPTY;

    // Unbelegt: nichts umzubinden -- und der Grund wird benannt, nicht als "geht nicht"
    // verschwiegen.
    let leer_no_ep = e.rebind_server(v1, v2) == Rebind::NoEndpoint;
    let leer_nicht_bindbar = !e.bind_receiver(v1);

    e.mark_used();
    // **Die Kernabweisung:** ohne Stilllegung wird NICHT umgebunden, auch nicht an einem
    // ruhenden Endpoint. Der Befund waere sonst nur eine Momentaufnahme -- ein CALL auf einem
    // anderen Kern macht ihn falsch, bevor der Tausch geschieht.
    let ohne_ruhe_abgewiesen = e.rebind_server(v1, v2) == Rebind::NotQuiescing;

    e.begin_quiesce();
    // Stillgelegt, aber v1 ist gar nicht gebunden -> es gibt nichts abzuloesen.
    let ungebunden_gemeldet = e.rebind_server(v1, v2) == Rebind::NotReceiver;
    // Ein Austausch mit sich selbst meldet Erfolg, ohne einer zu sein -> eigener Ausgang.
    let selbsttausch_gemeldet = e.rebind_server(v1, v1) == Rebind::SameThread;

    // v1 binden; ein zweites Binden desselben Threads waere ein Duplikat in der Queue.
    let v1_gebunden = e.bind_receiver(v1);
    let doppelbindung_abgewiesen = !e.bind_receiver(v1);
    let v1_ist_empfaenger = e.quiescence_of(v1).as_receiver;

    // Ein FREMDER Empfaenger blockiert den Austausch: wer hier wartet, gehoert nicht zu den
    // beiden Instanzen und wuerde vom Tausch stillschweigend uebergangen.
    e.bind_receiver(fremd);
    let fremder_blockiert = e.rebind_server(v1, v2)
        == Rebind::Blocked(RebindBlocked {
            other_receiver: true,
            ..RebindBlocked::default()
        });
    e.retire_receiver(fremd);

    // **Der Erfolgsfall ohne Ueberlappung:** v2 wird hier eingereiht. Zulaessig, aber die
    // schwaechere Zusicherung -- der Aufrufer steht dafuer ein, dass v2 geparkt ist.
    let einfach = e.rebind_server(v1, v2) == Rebind::Done { overlapped: false };
    let v1_geloest = e.quiescence_of(v1).is_quiescent();
    let v2_gebunden = e.quiescence_of(v2).as_receiver;

    // **Der Erfolgsfall MIT Ueberlappung -- die starke Zusicherung.** Beide Instanzen sind
    // gebunden, das Loesen der alten hinterlaesst keine Luecke: zu keinem Zeitpunkt, auch
    // nicht innerhalb der Operation, hat der Endpoint null Empfaenger.
    e.bind_receiver(v1); // "v1" spielt hier die neue Instanz, v2 die alte
    let ueberlappend = e.rebind_server(v2, v1) == Rebind::Done { overlapped: true };
    let nur_noch_v1 = e.quiescence_of(v1).as_receiver && e.quiescence_of(v2).is_quiescent();
    // Und der Endpoint ist danach NICHT leer -- genau das ist die Aussage von A-4.1.
    let empfaenger_durchgehend = !e.is_idle();

    let ok = leer_no_ep
        && leer_nicht_bindbar
        && ohne_ruhe_abgewiesen
        && ungebunden_gemeldet
        && selbsttausch_gemeldet
        && v1_gebunden
        && doppelbindung_abgewiesen
        && v1_ist_empfaenger
        && fremder_blockiert
        && einfach
        && v1_geloest
        && v2_gebunden
        && ueberlappend
        && nur_noch_v1
        && empfaenger_durchgehend;
    println!(
        "rebind  : unbelegt -> NoEndpoint {leer_no_ep}/nicht bindbar {leer_nicht_bindbar}; OHNE \
         Stilllegung abgewiesen {ohne_ruhe_abgewiesen}; ungebunden gemeldet \
         {ungebunden_gemeldet}; Selbsttausch gemeldet {selbsttausch_gemeldet}; binden greift \
         {v1_gebunden}/Doppelbindung abgewiesen {doppelbindung_abgewiesen}/ist Empfaenger \
         {v1_ist_empfaenger}; FREMDER Empfaenger blockiert {fremder_blockiert}; Tausch ohne \
         Ueberlappung {einfach} (alt geloest {v1_geloest}, neu gebunden {v2_gebunden}); Tausch MIT \
         Ueberlappung {ueberlappend} (nur noch die neue {nur_noch_v1}, durchgehend ein Empfaenger \
         {empfaenger_durchgehend})"
    );
    println!(
        "rebind  : {} (A-4.1: Pruefung und Tausch unter EINEM Lock -- zwischen 'alter Server weg' \
         und 'neuer empfangsbereit' liegt kein Zustand ohne Empfaenger)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **D11-Selbsttest: der Überlauf einer Endpoint-Warteschlange ist BENANNT.**
///
/// Bis zum 2026-08-04 war `TidQueue::enqueue` ein `if cap { … }` **ohne `else`**: ab dem 33.
/// Eintrag verschwand der Faden lautlos. Der schlimmste Ausgang davon war nicht der Verlust,
/// sondern die Meldung darüber — `bind_receiver` gab `true` zurück, während der Eintrag
/// weggeworfen wurde. Wer darauf baute, hielt eine Server-Instanz für gebunden, die der Endpoint
/// nie gesehen hatte.
///
/// **Was dieser Test kann und was nicht.** Er läuft — wie [`run_quiesce`] und [`run_rebind`] — auf
/// einem *lokalen* Objekt und braucht deshalb keinen Scheduler. Damit ist genau der Weg prüfbar,
/// der ohne `SchedOps` auskommt: `bind_receiver`. Die anderen drei Wege (`call`, `recv`,
/// `migrate_owner`) brauchen blockierende Operationen und werden gegen **denselben** Quelltext in
/// `tools/verus-modelltreue-ipc.sh` gefahren — dort mit Stellvertretern für Frames und Scheduler,
/// samt fünf Mutationen, die D11 einzeln wieder herstellen. Diese Zeile ersetzt das nicht, sie
/// belegt, dass die Schranke **in diesem Kernel-Abbild** so gebaut ist.
///
/// Die Positivkontrolle steckt in der Anlage: der 32. Eintrag muss gelingen. Ein Test, in dem nur
/// der 33. scheitert, wäre auch von „bind_receiver geht nie" nicht zu unterscheiden.
#[cfg(feature = "selftest")]
pub fn run_epfull() -> bool {
    let mut e = Endpoint::EMPTY;
    e.mark_used();

    // 1..QUEUE_CAP fuellen. Jeder einzelne muss gelingen -- sonst misst der Rest nichts.
    let mut alle_gebunden = true;
    for i in 0..caprock_ipc::QUEUE_CAP {
        let t = ThreadId::from_raw(0xD11_0000 + i as u64);
        alle_gebunden &= e.bind_receiver(t);
    }
    // Und sie stehen wirklich alle drin -- nicht nur "hat true gesagt". Genau diese Lücke war
    // der Befund: die Meldung stimmte, der Eintrag fehlte.
    let mut alle_auffindbar = true;
    for i in 0..caprock_ipc::QUEUE_CAP {
        let t = ThreadId::from_raw(0xD11_0000 + i as u64);
        alle_auffindbar &= e.quiescence_of(t).as_receiver;
    }

    // Der eine ueber der Schranke: MISSERFOLG, und er steht nirgends.
    let ueberzaehlig = ThreadId::from_raw(0xD11_FFFF);
    let abgewiesen = !e.bind_receiver(ueberzaehlig);
    let nicht_eingetragen = e.quiescence_of(ueberzaehlig).is_quiescent();
    // Er hat auch keinen der 32 verdraengt -- eine Abweisung, die den Ringpuffer weiterdreht,
    // waere schlimmer als der Fehler.
    let erster_noch_da = e
        .quiescence_of(ThreadId::from_raw(0xD11_0000))
        .as_receiver;
    let audit_sauber = e.audit(&mut |_t: ThreadId| true) == (false, false);

    // Kein Leck: wird ein Platz frei, ist er wieder vergebbar. Ohne diese Haelfte waere
    // "abgewiesen" von "kaputt" nicht zu unterscheiden.
    e.retire_receiver(ThreadId::from_raw(0xD11_0000));
    let nach_freigabe = e.bind_receiver(ueberzaehlig);

    let ok = alle_gebunden
        && alle_auffindbar
        && abgewiesen
        && nicht_eingetragen
        && erster_noch_da
        && audit_sauber
        && nach_freigabe;
    println!(
        "epfull  : {} gebunden (alle auffindbar {alle_auffindbar}); der {}. abgewiesen \
         {abgewiesen}/nicht eingetragen {nicht_eingetragen}/keinen verdraengt {erster_noch_da}/\
         audit sauber {audit_sauber}; nach Freigabe wieder vergebbar {nach_freigabe}",
        caprock_ipc::QUEUE_CAP,
        caprock_ipc::QUEUE_CAP + 1
    );
    println!(
        "epfull  : {} (D11: der Ueberlauf einer Endpoint-Warteschlange wird BENANNT statt still \
         verworfen. Vorher meldete bind_receiver Erfolg, waehrend der Eintrag verschwand -- und \
         call/recv blockierten den Faden, ohne ihn irgendwo einzutragen: er hing dauerhaft, und \
         is_quiescent/audit/purge_thread meldeten Ordnung)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **A-4.3-Selbsttest: die Zustandsübergabe, alle Ausgänge.**
///
/// Läuft auf einer **eigenen** Scratch-Region, nicht auf der echten Zustandsregion des
/// Zähler-Service: der Test darf den Zustand, dessen Überleben er belegen soll, nicht selbst
/// anfassen. Er gibt die Region am Ende zurück.
///
/// Geprüft wird dieselbe Torlogik ([`state::attach`]), die der Reload-Pfad ausführt. Über den
/// regulären Pfad sind die Abweisungszweige heute **nicht** erreichbar — pro Boot gibt es genau
/// eine Fassung des Zustandslayouts, jede Übernahme liest also dieselbe Version. Erreichbar
/// werden sie erst, wenn ein Austausch zur Laufzeit ein anderes Layout mitbringt. Damit sie bis
/// dahin nicht ungeprüft bleiben (ungeprüft heisst: vermutlich kaputt, wenn sie zum ersten Mal
/// gebraucht werden), füttert der Selbsttest sie direkt — dieselbe Begründung wie bei
/// `iface_record_or_check` (A-4.4).
pub fn run_state() -> bool {
    const PROG: u32 = 0xA43_00FF;
    const VER: u32 = 7;
    const LEN: u32 = 16;

    let Some(mut region) = KernelRegionSource.request(4096, Purpose::HotReloadState) else {
        println!("state   : FAILURES (keine Scratch-Region)");
        return false;
    };

    // Vor `init` liegt dort kein Kopf: eine frische Region ist ein Kaltstart, kein Zustand der
    // Version 0. Der Allokator liefert nicht garantiert genullten Speicher -- deshalb erst nullen
    // und dann pruefen, sonst prueft dieser Fall den Zufall.
    region.view().fill(0);
    let vorher_kein_zustand =
        state::attach(&mut region.view(), PROG, VER) == Err(state::StateError::NoState);

    let angelegt = state::init(&mut region.view(), PROG, VER, LEN).is_ok();
    // Direkt nach dem Anlegen: null Uebernahmen. Ohne diesen Zaehler saehe ein still neu
    // angelegter Zustand genauso aus wie ein geerbter, der zufaellig dieselben Werte traegt.
    let gen0 = state::generation(&region.view()) == Some(0);

    // Die alte Fassung hinterlaesst etwas.
    let geschrieben = state::payload(&mut region.view())
        .map(|mut p| p.set::<u64>(0, 0xA43_BEEF))
        .unwrap_or(false);

    // **Der Erfolgsfall:** passende program_id UND state_version -> Uebernahme, Zaehler auf 1.
    let uebernommen = state::attach(&mut region.view(), PROG, VER)
        == Ok(state::Handover {
            generation: 1,
            payload_len: LEN,
        });
    // Und der Zustand ist wirklich da -- das ist die Aussage von A-4.3.
    let zustand_ueberlebt = state::payload(&mut region.view())
        .and_then(|p| p.get::<u64>(0))
        == Some(0xA43_BEEF);
    // Eine zweite Uebernahme zaehlt weiter, statt zurueckzusetzen: sonst waere die dritte
    // Fassung von der ersten nicht zu unterscheiden.
    let zaehlt_weiter = state::attach(&mut region.view(), PROG, VER)
        .map(|h| h.generation)
        == Ok(2);

    // **Die Kernabweisung:** anderes Layout -> KEIN Ok. Ohne sie laese die neue Fassung die Bytes
    // der alten in ihrem eigenen Sinn -- kein Datenverlust, sondern ein fehlinterpretierter
    // Zustand, und der faellt niemandem auf.
    let falsche_version = state::attach(&mut region.view(), PROG, VER + 1)
        == Err(state::StateError::VersionMismatch { found: VER });
    // Fremdes Programm: hier liegt der Zustand von jemand anderem, nicht "vermutlich passend".
    let fremdes_programm = state::attach(&mut region.view(), PROG + 1, VER)
        == Err(state::StateError::WrongProgram { found: PROG });
    // Und die Abweisungen haben den Zaehler NICHT erhoeht -- eine gescheiterte Uebernahme ist
    // keine.
    let abweisung_zaehlt_nicht = state::generation(&region.view()) == Some(2);

    // Der Kopf ist eine Behauptung ueber die Region, keine Tatsache: behauptet er mehr Nutzlast,
    // als die Region traegt, ist das ein eigener Befund und kein stillschweigend gekuerzter
    // Zugriff.
    let zu_gross = LEN as usize + 4096;
    region.view().set::<u32>(16, zu_gross as u32); // OFF_PAYLOAD_LEN
    let kopf_luegt = matches!(
        state::attach(&mut region.view(), PROG, VER),
        Err(state::StateError::Corrupt { .. })
    );

    // Eine Region, die kleiner ist als Kopf + gewuenschte Nutzlast, wird gar nicht erst angelegt.
    let zu_klein = state::init(&mut region.view(), PROG, VER, u32::MAX) == Err(state::StateError::TooSmall);

    KernelRegionSource.release(region);

    // Dass auch die ECHTE Zustandsregion einen gueltigen Kopf traegt, belegt dieser Test NICHT --
    // und zwar bewusst nicht mehr: die Region entsteht in der arch-neutralen Zaehler-Demo
    // (`threads::…`, Aufruf von `hotreload_state_alloc`), die auf x86 gar nicht laeuft. Hier
    // abgefragt war die Zusicherung nicht scharf, sondern nur an der falschen Stelle: sie fiel
    // durch, weil es die Region nicht gibt, nicht weil ihr Kopf fehlt. Belegt wird sie im
    // aarch64-Lauf durch `ckpt`: die Uebernahme-Generation 1 kann nur aus `state::attach` kommen,
    // und das gibt es ohne gueltigen Kopf nicht.
    let echte_region_hat_kopf = hotreload_state_generation();

    let ok = vorher_kein_zustand
        && angelegt
        && gen0
        && geschrieben
        && uebernommen
        && zustand_ueberlebt
        && zaehlt_weiter
        && falsche_version
        && fremdes_programm
        && abweisung_zaehlt_nicht
        && kopf_luegt
        && zu_klein;
    println!(
        "state   : frisch -> NoState {vorher_kein_zustand}; angelegt {angelegt} (Generation 0 \
         {gen0}, Nutzlast schreibbar {geschrieben}); UEBERNOMMEN {uebernommen} (Zustand ueberlebt \
         {zustand_ueberlebt}, zweite Uebernahme zaehlt weiter {zaehlt_weiter}); falsche \
         state_version abgewiesen {falsche_version}; fremde program_id abgewiesen \
         {fremdes_programm}; Abweisung zaehlt nicht {abweisung_zaehlt_nicht}; luegender Kopf \
         erkannt {kopf_luegt}; zu kleine Region abgewiesen {zu_klein}; echte Zustandsregion \
         {echte_region_hat_kopf:?} (auf x86 keine -- die Demo laeuft dort nicht; Beleg im \
         aarch64-Lauf via ckpt)"
    );
    println!(
        "state   : {} (A-4.3: der Zustand liegt in einer Region mit VERSIONIERTEM Kopf -- passt \
         das Layout nicht, wird abgewiesen statt fehlinterpretiert)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **A-4.2-Selbsttest: die Torlogik des ruhenden Punktes, alle Ausgänge.**
///
/// Fährt gegen [`Endpoint::gate_new_transaction`] statt gegen `call`/`recv`, aus dem dort
/// genannten Grund: über den regulären Pfad ist der Abweisungszweig nur während eines
/// laufenden Austauschs erreichbar und bliebe sonst bis zum ersten echten Hot-Reload
/// ungeprüft. Geprüft wird **dieselbe** Funktion, die `call`/`recv` ausführen.
///
/// Auf einem **lokalen** Endpoint-Objekt, nicht auf einem der echten: der Test darf keinen
/// Endpoint stilllegen, den gerade jemand benutzt. Der Rollenbefund
/// ([`Endpoint::quiescence_of`]) lässt sich hier nicht füllen — Queues und Reply-Token sind
/// privat und werden nur von `call`/`recv` gesetzt. Er wird deshalb am **echten** offenen
/// Call im Hot-Reload-Szenario (`rmig`) abgenommen, nicht an einer Attrappe.
#[cfg(feature = "selftest")]
pub fn run_quiesce() -> bool {
    use caprock_abi::result;
    let fremd = ThreadId::from_raw(0xA42_0001);
    let mut e = Endpoint::EMPTY;

    // Unbelegter Endpoint: nicht stilllegbar, und das Tor weist mit ERR_BADCAP ab -- NICHT mit
    // ERR_QUIESCING. Die beiden Gründe duerfen nicht verschwimmen: "gibt es nicht" ist fuer
    // einen Client eine andere Lage als "kommt gleich wieder".
    let leer_nicht_stilllegbar = !e.begin_quiesce();
    let leer_badcap = e.gate_new_transaction() == Some(result::ERR_BADCAP);
    // Ein unbelegter (recycelter) Endpoint darf keine Rollen melden, sonst schleppte er den
    // Befund seines Vorbesitzers weiter.
    let leer_ohne_rollen = e.quiescence_of(fremd).is_quiescent();

    e.mark_used();
    let frisch_offen = e.gate_new_transaction().is_none();
    let frisch_ruhig = e.is_idle();

    // Stilllegen greift; ein ZWEITER Aufruf meldet false. Das ist kein Schoenheitsfehler:
    // zwei gleichzeitige Austausche am selben Endpoint wuerden sich gegenseitig die Freigabe
    // ziehen -- der erste `end_quiesce` oeffnete das Tor mitten im zweiten Austausch.
    let erste_stilllegung = e.begin_quiesce();
    let zweite_abgewiesen = !e.begin_quiesce();
    let tor_zu = e.gate_new_transaction() == Some(result::ERR_QUIESCING);
    let meldet_still = e.is_quiescing();
    // Stillgelegt heisst NICHT beschaeftigt: ein Endpoint ohne offene Transaktion ruht auch
    // waehrend der Stilllegung -- genau das ist die Bedingung, unter der ausgetauscht werden
    // darf.
    let still_und_ruhig = e.is_idle();

    let freigabe = e.end_quiesce();
    let tor_wieder_offen = e.gate_new_transaction().is_none();
    let doppelte_freigabe_gemeldet = !e.end_quiesce();

    let ok = leer_nicht_stilllegbar
        && leer_badcap
        && leer_ohne_rollen
        && frisch_offen
        && frisch_ruhig
        && erste_stilllegung
        && zweite_abgewiesen
        && tor_zu
        && meldet_still
        && still_und_ruhig
        && freigabe
        && tor_wieder_offen
        && doppelte_freigabe_gemeldet;
    println!(
        "quiesce : unbelegt nicht stilllegbar {leer_nicht_stilllegbar}; unbelegt -> BADCAP (nicht \
         QUIESCING) {leer_badcap}; unbelegt ohne Rollen {leer_ohne_rollen}; frisch offen \
         {frisch_offen}/ruhig {frisch_ruhig}; stilllegen greift {erste_stilllegung}; ZWEITER \
         Austausch abgewiesen {zweite_abgewiesen}; Tor zu {tor_zu}; meldet still {meldet_still}; \
         still+ohne offene Transaktion = ruhig {still_und_ruhig}; freigeben greift {freigabe}; Tor \
         wieder offen {tor_wieder_offen}; doppelte Freigabe gemeldet {doppelte_freigabe_gemeldet}"
    );
    println!(
        "quiesce : {} (A-4.2: der ruhende Punkt -- waehrend eines Austauschs wird KEINE neue \
         Transaktion eroeffnet, laufende duerfen abschliessen)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **C9c — Raten-Grenze des Endpoint-Sweeps in [`purge_ipc_queues`].**
///
/// Eine Rate haelt `IPC_ORPHANS` als aeusseren Lock ueber hoechstens so viele
/// Endpoint-Sperrungen plus das Entblocken ihrer eigenen Waisen; danach wird
/// freigegeben (IRQs wieder an) und erst dann laeuft die naechste Rate. Bei
/// `NEPS + NNTFNS = 20 128` sind das rund 40 Raten statt eines einzigen
/// 7,3-ms-Blocks (gemessen 20,4 Mio. Zyklen je Thread-Tod). Die Zahl ist eine
/// Latenz-Grenze, keine Kapazitaet: kleiner waere mehr Sperrwechsel je Tod,
/// groesser waere naeher am alten Block. Der Notification-Sweep braucht keine
/// eigene Grenze — er laeuft ohne aeusseren Lock, jede Iteration ist dort
/// bereits ein eigener kurzer Abschnitt.
const PURGE_EP_SCHRITT: usize = 512;

/// **Eager-Cleanup beim Thread-Tod:** den (sterbenden) Thread `tid` aus ALLEN
/// Endpoint-Queues (senders/receivers/caller) und Notification-Waitern entfernen.
/// Verhindert tote TCBs in den festen Queues (Corpse-Fill -> verdrängte echte Sender)
/// und ein REPLY/SIGNAL in einen recycelten Frame. Aus den Todespfaden (kill/exit/
/// fault) aufzurufen — OHNE gehaltenen SCHEDS-Lock (Sperrordnung EPS/NTFNS < SCHEDS).
pub fn purge_ipc_queues(tid: ThreadId) {
    // Verwaiste Aufrufer (deren Reply-Owner gerade stirbt) einsammeln und NACH dem
    // Freigeben der EPS-Locks mit ERR_SERVER_GONE entblocken (kein verschachtelter
    // EPS->SCHEDS-Lock). Ein Thread kann (mehrfaches recv ohne reply) Reply-Owner von bis zu
    // `eps().len()` Endpoints zugleich sein -> die Flaeche MUSS so gross sein, sonst wuerden
    // Waisen jenseits der Kapazitaet still verworfen und ihre Aufrufer haengen dauerhaft
    // (Liveness-Bug).
    //
    // **A-3.4 Teil 4 — warum das hier keine lokale Variable mehr ist:** bis Teil 3 stand hier
    // `[Option<ThreadId>; NENDPOINTS]`, also 32 Eintraege = 768 Byte auf dem Kernelstack. Mit
    // 10 000 Endpoints waeren daraus 240 KiB geworden -- auf einem Stack, der ein Vielfaches
    // kleiner ist, und das im **Todespfad** eines Threads. Die Zusage der Zeile darueber
    // (`so gross wie die Endpoint-Zahl`) haette den Stack ueberrannt, statt Waisen zu verlieren:
    // ein Stack-Overflow statt eines Liveness-Bugs. Dieselbe Falle, die A-3.3 fuer den
    // Finalisierungspuffer aufgeloest hat -- die Flaeche kommt jetzt aus dem Boot-RAM.
    //
    // **C9c — warum der Sweep in Raten laeuft statt in einem Block:** die Flaeche ist
    // geteilt statt lokal. Hielte EIN aeusserer Lock ueber Sammeln UND Entblocken des
    // ganzen Sweeps, serialisierten Thread-Tode kernuebergreifend und der Kern liefe
    // O(Endpoints + Notifications) = 20 128 Einzelsperrungen je Thread-Tod mit
    // maskierten Interrupts (gemessen 20,4 Mio. Zyklen / 7,3 ms). Darum gilt je Rate
    // die Regel: **keine lebende Waise uebersteht das Freigeben.** Jede Rate sammelt
    // ihre Waisen, entblockt sie noch unter derselben Haltung und gibt erst danach
    // frei — was beim Freigeben in der Flaeche steht, ist restlos `None`. Ein zweiter
    // sterbender Thread auf einem anderen Kern findet die Flaeche dadurch immer leer
    // vor, egal wie sich die Raten verzahnen: keine Waise geht verloren, keine wird
    // doppelt entblockt.
    //
    // Sperrordnung je Rate: IPC_ORPHANS ist damit **aeusserer** Lock ->
    // IPC_ORPHANS < EPS[i] < SCHEDS. Kein anderer Pfad nimmt ihn, insbesondere keiner
    // mit gehaltenem EPS oder SCHEDS -> kein Zyklus.
    // **C4: dieser Pfad ist O(Endpoints + Notifications) JE THREAD-TOD** -- und er sperrt jedes
    // Objekt einzeln. Gezaehlt, nicht gestoppt: eine Iterationszahl ist eine Eigenschaft des
    // Programms, eine Zeitmessung nicht (D10). Bei `NEPS + NNTFNS = 20 128` und 10 000
    // sterbenden Threads sind das 201 Millionen Sperroperationen -- das ist die Groesse, die
    // ein Teardown von zehntausend PDs kostet, und sie stand bisher nirgends.
    //
    // **Messlatte (wo die 20,4M-Zahl herkommt und wo der Effekt sichtbar wird):**
    // die ITERATIONSZAHL zaehlen `PURGE_IPC_CALLS`/`PURGE_IPC_ITER` (Bericht
    // `purge_ipc_queues:` in `kernel/src/arch/x86_64/bringup.rs`); die LATENZ misst
    // die Sperrhaltedauer-Marke (`crates/caprock-sync`, Bericht `sperre`), deren
    // Schuldposten `kernel/src/system.rs` mit Deckel 24 Mio. Zyklen (= 856 Promille
    // eines Ticks) in `kernel/src/sperrmark.rs` steht. Diese Zeilen bleiben
    // unveraendert — der Umbau aendert die Haltung, nicht die Zaehlung.
    //
    // **Bewusst aufgegeben:** die alte Gesamt-Reihenfolge „erst ALLES bereinigen,
    // dann alle Waisen wecken". Die Waisen einer Rate werden geweckt, bevor die
    // naechste Rate bereinigt ist — genau diese Ordnung war der 7ms-Block. Ein
    // geweckter Aufrufer sieht nur `ERR_SERVER_GONE`; die Bereinigung selbst bleibt
    // vollstaendig (jeder Endpoint, jede Notification, jede Waise genau einmal).
    PURGE_IPC_CALLS.fetch_add(1, Ordering::Relaxed);
    PURGE_IPC_ITER.fetch_add((eps().len() + ntfns().len()) as u64, Ordering::Relaxed);
    let alle_eps = eps();
    let mut start = 0usize;
    while start < alle_eps.len() {
        let ende = (start + PURGE_EP_SCHRITT).min(alle_eps.len());
        // Sammeln UND Entblocken DIESER Rate unter einer Haltung (s. Regel oben).
        let mut orphans = IPC_ORPHANS.lock();
        let mut no = 0usize;
        for ep in &alle_eps[start..ende] {
            let mut e = ep.lock();
            if e.is_used() {
                e.purge_thread(tid);
                if let Some(caller) = e.owner_died(tid) {
                    if no < orphans.len() {
                        orphans[no] = Some(caller);
                        no += 1;
                    }
                }
            }
        } // EPS-Locks dieser Rate sind hier alle wieder frei
        for i in 0..no {
            if let Some(caller) = orphans[i].take() {
                unblock_with_error(caller, caprock_abi::result::ERR_SERVER_GONE);
            }
        }
        // Flaeche dieser Rate restlos geraeumt -> Freigabe mit leeren Haenden;
        // zwischen den Raten sind die IRQs wieder an (kein 7ms-Block mehr).
        drop(orphans);
        start = ende;
    }
    // Der Notification-Sweep erzeugt keine Waisen und braucht den aeusseren Lock
    // gar nicht: jede Iteration sperrt genau ein Objekt und gibt es sofort wieder
    // frei — von sich aus schon in Schritten, nicht in einem Block.
    for n in ntfns().iter() {
        let mut nt = n.lock();
        if nt.is_used() {
            nt.purge_thread(tid);
        }
    }
}

/// **Scheduler-Audit über alle Kerne** (ext-30): `0` = alle Kern-Scheduler strukturell
/// konsistent, sonst `100*Kern + Code` des ersten Fehlers (s. `Scheduler::audit`). Prüft
/// insbesondere, dass jeder Directory-Eintrag auf genau den Kern + Slot zeigt, auf dem der
/// TCB tatsächlich liegt (Code 8) — die zentrale Migrations-Invariante.
pub fn sched_audit_all() -> u32 {
    for c in 0..num_cores() {
        let code = SCHEDS[c].lock().audit();
        if code != 0 {
            return (c as u32) * 100 + code;
        }
    }
    0
}

/// **NOHZ-Schnappschuss eines Kerns** (B-5.2/Z5): `(rivalen, weckruf, hat_schranke)` aus
/// EINEM Lock statt drei Anlaeufen (s. `Scheduler::nohz_stand` -- eine Frist zwischen zwei
/// Anlaeufen fiele durch jedes Gatter). Nur kalter Pfad (Idle-Eintritt + Tick-Nachfrage).
pub fn nohz_stand(core: usize) -> (usize, Option<u64>, bool) {
    SCHEDS[core].lock().nohz_stand()
}

/// **NOHZ-Idle-Eintritt** (B-5.2/Z5): schlafen statt ticken, wenn nichts zu verdraengen ist.
///
/// Der einzige Ort ausser dem Tick, an dem der Timer je entwaffnet wird: der laufende Thread
/// ist der Idle-Thread, die Ready-Queues sind leer, und der naechste Weckruf ist benannt
/// (Frist oder Budget-Refill) oder es gibt keinen. Die wartende Demo-Schleife lebt VOM Tick
/// (Worker-Preemption) und darf hier nie landen.
///
/// ## Der Vertrag (drei Zeilen, alle drei noetig)
///
/// 1. Schnappschuss aus **einem** [`Scheduler::nohz_stand`](caprock_sched::Scheduler::nohz_stand)
///    unter **maskierten** IRQs (`local_irq_save`). Das schliesst das klassische NOHZ-Rennen:
///    Weckruf NACH dem Schnappschuss, aber VOR dem Armieren bewaffnet -- ohne Maskierung
///    schliefe der Kern ueber ihn hinweg bis zum naechsten fremden IRQ.
/// 2. Entscheidung in `caprock_sched::nohz_plan` (host-geprueft): `Periodic` aendert nichts,
///    `OneShot` armiert einmalig (32-Bit-gedeckelt, `d <= 1` faellt auf Tick zurueck),
///    `Disarmed` maskiert die LVT.
/// 3. Nach dem Aufwachen steht der Timer **unbedingt** wieder periodisch da. Im Zweifel ein
///    Tick zu viel statt Stille -- die sichere Richtung.
///
/// ## Fail-closed
///
/// Wer hier nicht durchkommt, tickt wie bisher. Mit Fristen/Zweit-Threads/Budgets liefert
/// `nohz_plan` `Periodic`, und auch dann aendert sich gegenueber heute genau nichts (ein
/// idempotentes Rearmieren plus `wfi`). Arch-neutral: beide Idle-Schleifen rufen hierher.
pub fn nohz_idle(core: usize) {
    let irq = hal::cpu::local_irq_save();
    let (rivalen, weckruf, hat_schranke) = nohz_stand(core);
    match caprock_sched::nohz_plan(rivalen, weckruf, hat_schranke) {
        caprock_sched::Nohz::Periodic => {
            hal::timer::rearm_periodic();
            hal::cpu::wfi();
        }
        caprock_sched::Nohz::OneShot { ticks } => {
            let d = ticks.min(hal::timer::oneshot_max_ticks());
            if d <= 1 {
                hal::timer::rearm_periodic();
            } else {
                hal::timer::arm_oneshot(d);
            }
            hal::cpu::wfi();
            hal::timer::rearm_periodic();
        }
        caprock_sched::Nohz::Disarmed => {
            hal::timer::disarm();
            hal::cpu::wfi();
            hal::timer::rearm_periodic();
        }
    }
    hal::cpu::local_irq_restore(irq);
}

/// Einen blockierten Thread mit einem **Fehlercode** in `x0` (statt `OK`) entblocken —
/// für die Reply-Liveness: ein `CALL`-Aufrufer, dessen Server (Reply-Owner) verschwand,
/// wird so entblockt und sieht den Fehler, statt dauerhaft zu hängen. Sperrt kurz die
/// Zielinstanz; bei fremdem Kern Reschedule-IPI.
fn unblock_with_error(caller: ThreadId, code: u64) {
    let done = with_owner(caller, |sched, _| {
        let frame = sched.frame_of(caller)?; // fremder Kern/tot -> ggf. wiederholen
        hal::exception::frame_set_reg(frame, caprock_abi::reg::SYSNO_RESULT, code);
        sched.unblock(caller);
        Some(())
    });
    if let Some((_, c)) = done {
        kick(c);
    }
}

/// Lebt `tid`? (Für IPC-Audits.) Sperrt die Zielinstanz kurz.
pub fn thread_alive(tid: ThreadId) -> bool {
    // Lock-frei über das Thread-Directory (ext-30): kein Sperren eines fremden Kerns nötig,
    // und immun dagegen, dass der Thread gerade migriert.
    caprock_sched::is_live(tid)
}

/// **IPC-Konsistenz-Oracle** (Fuzzer): jedes belegte Endpoint/Notification + jeden
/// Kern-Scheduler auf strukturelle Invarianten prüfen. Gibt `0` bei Konsistenz, sonst
/// einen Anomalie-Code: 1=toter TCB in Endpoint-Queue/caller, 2=Duplikat in Endpoint-
/// Queue, 3=toter Waiter in Notification, 10+n=Scheduler-Audit-Code n (s. `Scheduler::
/// audit`). Sperrt je Objekt einzeln; die Liveness-Prüfung verschachtelt EPS/NTFNS ->
/// SCHEDS (zulässige Ordnung), nie zwei Objekte gleichzeitig.
pub fn ipc_audit() -> u32 {
    let live = &mut |t: ThreadId| -> bool { caprock_sched::is_live(t) };
    for ep in eps().iter() {
        let e = ep.lock();
        if e.is_used() {
            let (dead, dup) = e.audit(live);
            if dead {
                return 1;
            }
            if dup {
                return 2;
            }
        }
    }
    for n in ntfns().iter() {
        let nt = n.lock();
        if nt.is_used() && nt.audit(live) {
            return 3;
        }
    }
    for c in 0..num_cores() {
        let code = SCHEDS[c].lock().audit();
        if code != 0 {
            return 10 + code;
        }
    }
    // CDT-/Refcount-Property (Cap-Churn-Events des IPC-Fuzzers laufen während IPC).
    let cdt = cap_audit_cdt();
    if cdt != 0 {
        return 20 + cdt;
    }
    // Domänen-Policy-Property (ext-22): Cap-Typen je Domäne + Domäne↔VSpace-Isolation.
    let dom = domain_audit();
    if dom != 0 {
        return 30 + dom;
    }
    // DMA-Policy-Property (ext-23): DmaCap-Bounds/Disjunktheit + Enforcer-Durchsetzung.
    let dma = dma_audit();
    if dma != 0 {
        return 40 + dma;
    }
    // Loader-Property (ext-26): kein geladenes Programm-Segment überlappt freies RAM.
    let ld = loader_audit();
    if ld != 0 {
        return 60 + ld;
    }
    0
}

/// **Loader-Property-Oracle** (ext-26, L5): `0` = konsistent, sonst Anomalie-Code:
/// - `1` = ein aktuell in einer geladenen VSpace gemapptes Segment überlappt **freies** RAM (es
///   wurde freigegeben, während es noch gemappt ist → Use-after-free). Spiegelt `dma_audit` Code 4.
/// Gestaffelt je Image (`LOADED_IMAGES` kurz sperren, dann gegen `MEM.overlaps_free`
/// pruefen — Rangordnung: nie beide Locks gleichzeitig). Hoechstens `MAX_IMG_SEGS` Eintraege
/// (1 KiB) liegen je auf dem Stapel: die vorige Fassung hielt alle Images als Ganzes, und das
/// skaliert nicht auf zehntausend Images (zehn MiB Stapel).
#[inline(never)] // s. `dma_audit`: Audit-Rahmen aus heissen Pfaden heraushalten.
pub fn loader_audit() -> u32 {
    let n = LOADED_IMAGES.lock().len();
    let mut buf = [(0u64, 0u64); MAX_IMG_SEGS];
    for i in 0..n {
        let m = {
            let t = LOADED_IMAGES.lock();
            let img = t[i];
            if img.asid == 0 {
                continue;
            }
            let m = img.nseg.min(MAX_IMG_SEGS);
            buf[..m].copy_from_slice(&img.segs[..m]);
            m
        };
        if m == 0 {
            continue;
        }
        let mem = MEM.lock();
        if buf[..m].iter().any(|&(b, l)| l != 0 && mem.overlaps_free(b, l)) {
            return 1;
        }
    }
    0
}

/// **Domänen-Policy-Oracle** (ext-22): `0` = konsistent, sonst Anomalie-Code (1 = HW-Cap in
/// Nicht-HardwareLand, 2 = `PdControl` in Nicht-TrustedSas, 3 = Domäne↔VSpace inkonsistent).
/// Die VSpace-Zugehörigkeit (global vs. isoliert) liegt in `VSPACE_OF` (nicht in `Caps`), daher
/// wird sie hier per Closure eingespeist. Sperrt nur `CAPS.read()` (VSPACE_OF ist atomar).
pub fn domain_audit() -> u32 {
    // Regel 3 (untrusted Domäne MUSS isoliert sein) gilt nur für **lebende** gebundene
    // Threads: ein gestoppter/getöteter Thread (dessen VSPACE_OF auf 0 zurückgesetzt wurde,
    // dessen PD-Bindung aber noch auf den toten tid zeigt) ist KEINE Verletzung.
    let is_live_global =
        |tid: ThreadId| thread_alive(tid) && vspace_of(tid.slot()) == 0;
    CAPS.read().domain_audit(&is_live_global)
}
