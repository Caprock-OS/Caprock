#![no_std]
#![no_main]
// Die allocator_api-Features braucht der (arch-agnostische) Kernel-Kern; auf dem x86_64-Branch ist
// in Stufe 0 nur Boot+Serial aktiv (Kern folgt) -> nur fuer aarch64 anfordern.
#![cfg_attr(target_arch = "aarch64", feature(allocator_api))]
#![cfg_attr(target_arch = "aarch64", feature(btreemap_alloc))]
//! Caprock kernel — bootable image entry point.
//!
//! Phase 1 (HAL): Boot, Exception-Vektoren, Identity-MMU + Caches (W^X), GICv2,
//! Timer, SMP-Bring-up. Phase 2: capability-basiertes physisches Speichermodell
//! (`caprock-mem`, hier per Selbsttest exerziert). Die Hardware-Spezifik liegt
//! in `caprock-hal`; diese Crate verdrahtet Boot-Trampolin und Init-Reihenfolge.

// ext-25: prozess-lokale Heaps (caprock-region) nutzen den allocator_api — `Box`/`Vec` werden
// stets mit EXPLIZITEM Allokator (`*_in(&heap)`) erzeugt. Es gibt bewusst KEINEN globalen Heap;
// der Global-Allocator unten ist ein Wächter, der versehentliche `Box::new`/`Vec::new` abfängt.
#[cfg(target_arch = "aarch64")]
extern crate alloc;

/// Wächter-Global-Allocator: das SAS-Modell verlangt **prozess-lokale** Heap-Instanzen
/// (`Heap::new(source)` + `*_in(&heap)`). Ein impliziter globaler Heap existiert nicht.
struct NoGlobalHeap;
// SAFETY: niemals ein Block vergeben/freigegeben; jede Nutzung paniert (= Designfehler-Wächter).
unsafe impl core::alloc::GlobalAlloc for NoGlobalHeap {
    unsafe fn alloc(&self, _l: core::alloc::Layout) -> *mut u8 {
        panic!("kein globaler Heap im SAS — prozess-lokale Heap-Instanz nutzen (*_in)")
    }
    unsafe fn dealloc(&self, _p: *mut u8, _l: core::alloc::Layout) {}
}
#[global_allocator]
static GLOBAL: NoGlobalHeap = NoGlobalHeap;

/// Zwei Adressachsen (physisch / IOVA) als getrennte Typen — s. Moduldoku.
mod addr;
mod arch;
mod panic;
/// Seitenfarben / Cache-Partitionierung zwischen PDs (todo A1) — arch-neutral.
mod colors;
// Der Kernel-Kern (arch-agnostisch, nutzt aber die aarch64-HAL) ist auf dem x86_64-Branch in Stufe 0
// noch nicht aktiv — er wird Stufe fuer Stufe fuer x86_64 eingeschaltet (s. README-X86.md).
// **Z6b: die Speicher-Sonde des Debuggers.** Arch-neutral aus demselben Grund wie `dmatests`:
// sie prueft die EINE Stelle, an der sich die beiden Architekturen wirklich unterscheiden
// (`vspace_resolve`), und eine Sonde, die nur ein Zweig faehrt, laesst die andere ungeprueft --
// nicht „vermutlich gruen".
//
// **Eigenes `cfg` je Modul, nicht ein geteiltes.** Beim Einfuegen ist das Attribut von `dmatests`
// auf `dbgmem` gerutscht -- `dmatests` wurde damit bedingungslos gebaut und riss den Bau ohne
// Feature (`system::testsupport` gibt es dort nicht). Ein Attribut bindet an das NAECHSTE Item,
// und ein Kommentar dazwischen aendert daran nichts.
#[cfg(feature = "selftest")]
mod dbgmem;
// Z6b: die Debugger-Sonde selbst -- ebenfalls arch-neutral, s. Moduldoku.
#[cfg(feature = "selftest")]
mod dbgprobe;
#[cfg(feature = "selftest")]
mod dmatests;
// Z23/S3: die Sonde des Gruppenschnitts -- arch-neutral, s. Moduldoku. Das Attribut bindet an das
// NAECHSTE Item; ein Kommentar dazwischen aendert daran nichts, ein weiteres `mod` dazwischen sehr
// wohl (zweimal am 2026-08-19 passiert, beide Male vom F1-Bau gefangen).
#[cfg(feature = "selftest")]
mod pdfreeze;
// Z4d stage 1: the cut probe -- arch-neutral, see the module doc. Same attribute caveat as above:
// it binds to the NEXT item.
#[cfg(feature = "selftest")]
mod ckptcut;
// K1b: mehrere Thread-Stapel aus EINER Memory-Cap -- arch-neutral, s. Moduldoku. Dieselbe
// Attribut-Eigenheit wie oben: es bindet an das NAECHSTE Item.
#[cfg(feature = "selftest")]
mod spawnarena;
/// C7c: Stack-Arena — einmal aufgeteilte zusammenhaengende Bloecke statt gestreuter
/// Kernel-Stacks (Dichte-Ziel ~12x). Safe-only, abhaengigkeitsfrei, host-getestet.
mod stack_arena;
mod tlsprobe;
mod uhr;
/// Der dreiwertige Sondenausgang (2026-08-25) -- s. Moduldoku. Nicht hinter `selftest`: der Typ
/// ist die Sprache, in der Sonden reden, und ein Typ, den es nur im Testbau gibt, ist einer, den
/// der schlanke Bau nicht pruefen kann.
mod befund;
/// Z26, Vorbedingung 2: grosse, zusammenhaengende DMA mit Geraetesicht — und eine **benannte**
/// Absage statt `None`. Arch-neutral; die Klassifikation liegt host-getestet in `caprock-dma`.
mod grossdma;
/// C4: die **Stack-Wasserstandsmarke** — reichen 16 KiB, oder sind sie nur gross? Arch-neutral;
/// die Zaehler laufen auf beiden Architekturen mit, die Berichtszeile steht heute nur auf x86.
mod kstackmark;
/// Kernel-Glue des generischen Binary-Loaders. **Seit A-1 auf beiden Architekturen** — die
/// Archiv-Quelle ist nicht mehr ein fest verdrahtetes ARM-Fenster, sondern eine zur Laufzeit
/// gemeldete Spanne (auf x86 ein Multiboot-Modul).
mod loader;
/// Read-only Root-Schlüssel des **System-Manifests** (A-1.3) — autogeneriert von
/// `tools/gen_manifest_key.py`. Getrennt von [`trusted_keys`]: ein Zertifikat bezeugt die Herkunft
/// eines Binaries, ein Manifest die Zuteilung der ganzen Maschine.
mod manifest_keys;
/// Z8/N0-N4: NUMA — Topologie lesen, melden, und danach platzieren. Arch-neutral; die Dekoder
/// liegen abhaengigkeitsfrei in `caprock_hal::numa`, die Quellen sind ACPI (x86) bzw. der Device
/// Tree (aarch64).
mod numa;
#[cfg(feature = "selftest")]
mod selftest;
/// C9: die **Sperrhaltedauer-Marke** — wie lange war der Kern am Stueck nicht praemptierbar?
/// Die Messung selbst sitzt in `caprock-sync` (in jedem `lock()`), hier stehen die aus der
/// Tickrate hergeleitete Schwelle, die Eichung, die Gegenprobe und das Urteil. Arch-neutral.
#[cfg(feature = "selftest")]
mod sperrmark;
/// Z26/A3: das Kopieren Frame ↔ Sidecar — die **Nutzlast** des Umleitungsprimitivs. Arch-neutral;
/// den Frame zerlegt `caprock_hal::exception`, das Slot-Format steht abhängigkeitsfrei in
/// `caprock_sched::redirect`.
mod sidecarkopie;
mod system;
#[cfg(all(target_arch = "aarch64", feature = "selftest"))]
mod threads;
/// Read-only TrustedSAS-Root-Key-DB (ext-28, ADR 0014) — autogeneriert von `tools/gen_trusted_key.py`,
/// in den Kernel kompiliert, nur per Firmware-/Kernel-Update änderbar (nicht per Syscall).
mod trusted_keys;
/// C7b: die **EL0-Wasserstandsmarke** — wie tief ist der USER-Stack wirklich? Sie misst die
/// private Region einer isolierten PD, also den grössten Einzelposten je Mandant. Arch-neutral;
/// die Zähler laufen auf beiden Architekturen mit, die Berichtszeile steht heute nur auf x86
/// (dieselbe Einordnung wie [`kstackmark`]).
mod userstackmark;
/// C8: der **Verifiziererthread** — `SYS_LOAD` verifiziert nicht mehr auf dem 16-KiB-Stack des
/// Aufrufers, sondern auf seinem eigenen. Arch-neutral; gestartet aus beiden Hochlaufwegen.
mod verifizierer;

#[cfg(target_arch = "aarch64")]
use caprock_hal::{self as hal, println};

// --- aarch64-Kernel-Kern (auf dem x86_64-Branch noch inaktiv; Boot-Entry kommt aus arch). ---

/// Rückfall-Kernzahl, wenn der Device Tree keine `cpu@`-Knoten meldet.
#[cfg(target_arch = "aarch64")]
const FALLBACK_CORES: usize = 8;
/// Stack-Größe je Sekundärkern (zur Boot-Zeit aus dem RAM belegt).
#[cfg(target_arch = "aarch64")]
const SEC_STACK_SIZE: u64 = 0x10000;

/// Periodische Tick-Rate des Timers (Hz). 100 Hz = 10-ms-Zeitscheiben.
///
/// **Arch-neutral seit 2026-08-27, und das ist eine Zusammenfuehrung, keine Verallgemeinerung.**
/// Bis dahin stand dieselbe 100 zweimal im Baum -- hier unter `cfg(aarch64)` und noch einmal in
/// `arch/x86_64/bringup.rs`. Zwei Gedaechtnisse fuer eine Tatsache, und die `uhr`-Sonde brauchte
/// sie auf beiden Architekturen. Wer die Rate aendert, aendert sie jetzt an EINER Stelle (todo
/// D16).
pub const TICK_HZ: u64 = 100;

/// RAM-Layout der Zielplattform (Fallback; tatsächlich aus dem DTB gelesen).
#[cfg(target_arch = "aarch64")]
const RAM_BASE: u64 = 0x4000_0000;
#[cfg(target_arch = "aarch64")]
const RAM_END: u64 = RAM_BASE + 4 * 1024 * 1024 * 1024;

/// Von QEMU erzeugter Device Tree (eingebettet — siehe `caprock-dtb`).
#[cfg(target_arch = "aarch64")]
static DTB_BYTES: &[u8] = include_bytes!("virt.dtb");

#[cfg(target_arch = "aarch64")]
extern "C" {
    /// Sekundärkern-Einstieg (Assembler, `arch::aarch64::boot`).
    fn _start_secondary();
}

/// Basis der zur Boot-Zeit belegten Sekundär-Stacks (ext-30: nicht mehr im Linker
/// reserviert — bei bis zu 256 Kernen wären das 16 MiB tote Image-Größe, und die Kernzahl
/// steht erst zur Laufzeit fest).
#[cfg(target_arch = "aarch64")]
static SEC_STACKS_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// **Z6 stage 1 verdict** (aarch64) — written once after the PSCI bring-up loop, read by the
/// report gate in `threads/mod.rs`.
///
/// Default `false`: a run that dies before the loop must not gate through on a flag nobody wrote.
#[cfg(target_arch = "aarch64")]
pub static SMT_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// **Z8/N1 verdict** (aarch64) — same shape and same reason as [`SMT_OK`].
#[cfg(target_arch = "aarch64")]
pub static NUMA_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Stack-Spitze für Kern `core` (Slot `core` im belegten Block).
#[cfg(target_arch = "aarch64")]
fn secondary_stack_top(core: usize) -> u64 {
    SEC_STACKS_BASE.load(core::sync::atomic::Ordering::Relaxed) + (core as u64) * SEC_STACK_SIZE
}

/// Pro-Kern-Interrupt-Init (nach MMU). VBAR wird bereits vor der MMU gesetzt.
#[cfg(target_arch = "aarch64")]
fn init_core_irqs() {
    hal::intc::init_cpu(); // GIC-CPU-Interface (pro Kern)
    hal::timer::init(TICK_HZ); // Timer-PPI armieren (pro Kern)
                               // C9: die Schwelle der Sperrhaltedauer-Marke ist EIN Tick — hinterlegt neben dem Aufruf, der
                               // den Timer wirklich programmiert (eine `100` in `sperrmark.rs` waere ein zweites Gedaechtnis).
    #[cfg(feature = "selftest")]
    crate::sperrmark::tickrate_setzen(TICK_HZ);
}

/// Kernel-Eintritt des Primärkerns, gerufen vom Boot-Trampolin.
///
/// `dtb_addr` ist die physische Adresse des Device-Tree-Blobs (von QEMU in `x0`).
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn kernel_main(dtb_addr: u64) -> ! {
    // Vor der MMU sind Atomics/Spinlocks nicht wohldefiniert -> lock-freie Ausgabe.
    hal::console::emit_raw("\nboot: primary core up (pre-MMU)\n");

    // Exception-Vektoren früh setzen, damit Faults während des MMU-Bring-ups
    // diagnostiziert werden (Dump ist lock-frei).
    hal::exception::init();

    // MMU + Caches zuerst: danach sind Atomics/der Konsolen-Lock wohldefiniert.
    hal::mmu::init_primary();

    // F3 (Hygiene): Bring-up-Meldung, kein Testbericht -- nur im Pruefbau sichtbar, sonst
    // still. Es gibt kein eigenes `boot-verbose`-Feature (kernel/Cargo.toml ist fremder Besitz),
    // also traegt `selftest` das Gatter. Keine Logikaenderung: reiner Register-Read + Ausgabe.
    #[cfg(feature = "selftest")]
    {
        let (m, c, i) = hal::mmu::sctlr_flags();
        println!("mmu     : identity-map, M={} C={} I={} (caches an)", m as u8, c as u8, i as u8);
    }
    #[cfg(feature = "debug-log")]
    println!("========================================");
    #[cfg(feature = "debug-log")]
    println!(" Caprock — capability microkernel");
    #[cfg(feature = "debug-log")]
    println!(" phase 1: HAL bring-up");
    #[cfg(feature = "debug-log")]
    println!("========================================");
    #[cfg(feature = "debug-log")]
    println!("arch    : aarch64 (running at EL{})", hal::cpu::current_el());
    #[cfg(feature = "debug-log")]
    println!("boot-x0 : {dtb_addr:#018x} (DTB-Zeiger; bei QEMU-ELF 0 -> DTB eingebettet)");

    // Distributor global + Init des Primärkerns (core 0).
    hal::intc::init_dist();
    init_core_irqs();
    println!("core 0  : online (vectors, gicv{}, timer @ {} Hz)", hal::intc::version(), TICK_HZ);
    println!("timer   : CNTFRQ={} Hz, PPI {}", hal::timer::freq(), hal::timer::TIMER_INTID);

    // Spekulations-Eigenschaften der HW melden (ext-29). CSV2/CSV3 sagen, ob die HW von sich
    // aus gegen Spectre-v2 (Branch-Predictor über Kontexte) bzw. Meltdown immun ist; der Kernel
    // härtet unabhängig davon seine EL0-Indexpfade (`array_index_nospec`) und setzt beim
    // VSpace-Wechsel eine Spekulationsbarriere. NICHT abgedeckt bleiben Cache-/Timing-
    // Seitenkanäle zwischen PDs (keine Cache-Partitionierung) — s. docs/invariants.md.
    #[cfg(feature = "debug-log")]
    println!(
        "spec    : CSV2={} CSV3={} FEAT_SB={} · nospec-Indizes an, Barriere beim VSpace-Wechsel",
        hal::cpu::csv2(),
        hal::cpu::csv3(),
        hal::cpu::sb_supported() as u8
    );
    // Cache-Geometrie melden (todo A1/B-2.2). Steht bewusst NEBEN der `spec`-Zeile: beides sagt,
    // was die Hardware von sich aus hergibt, und beides ist eine Bring-up-Meldung, kein
    // Testbericht (todo F3). Bis hierher lief die aarch64-Fassung von `hal::cache` nie -- sie war
    // geschrieben und uebersetzt, aber nur der x86-Hochlauf rief sie.
    crate::colors::report();

    // RAM-Layout aus dem Device Tree lesen (statt fest verdrahtet).
    let (ram_base, ram_size) = caprock_dtb::Dtb::parse(DTB_BYTES)
        .and_then(|d| d.memory())
        .unwrap_or((RAM_BASE, RAM_END - RAM_BASE));
    let ram_end = ram_base + ram_size;
    println!(
        "dtb     : RAM base={ram_base:#x} size={} MiB (aus Device Tree)",
        ram_size >> 20
    );
    // Kernzahl ebenfalls aus dem Device Tree (ext-30) statt fest verdrahtet.
    let cores = caprock_dtb::Dtb::parse(DTB_BYTES)
        .and_then(|d| d.cpu_count())
        .filter(|&n| n > 0)
        .unwrap_or(FALLBACK_CORES)
        .min(caprock_sched::MAX_CORES);
    println!("dtb     : {cores} CPUs (aus Device Tree; Kernel-Obergrenze {})", caprock_sched::MAX_CORES);
    let dtb_ok = ram_base == RAM_BASE && ram_size == 4 * 1024 * 1024 * 1024 && cores > 0;
    println!("dtb     : {}", if dtb_ok { "ALL PASS" } else { "FAILURES" });

    // Phase 2/3: capability-basiertes Speichermodell + Capability-Space.
    // User-RAM erst ab 2 MiB: die ersten 2 MiB sind die geteilte Kernel-L3 (von
    // jeder isolierten VSpace genutzt) und dürfen kein EL0-zugängliches RAM enthalten.
    let free_base = hal::mmu::kernel_end().max(hal::mmu::USER_RAM_MIN);
    // ext-26: das oberste RAM-Fenster [MOD_BASE, ram_end) ist für das extern geladene Boot-Archiv
    // reserviert (QEMU `-device loader`); der Allokator bekommt es NICHT -> kein Konflikt.
    let alloc_end = ram_end.min(loader::MOD_BASE);
    system::init_mem(free_base, alloc_end);
    // Cap-Tabellen VOR dem ersten Cap: der Selbsttest gleich darunter installiert bereits welche.
    let cap_bytes = system::configure_caps();
    // F3 (Hygiene): Bring-up-Meldung, kein Testbericht -- nur im Pruefbau sichtbar, sonst
    // still (dasselbe Gatter wie `mmu` oben; keine Logikaenderung, nur die Ausgabe faellt weg).
    #[cfg(feature = "selftest")]
    println!("mem     : freies RAM [{free_base:#x}, {alloc_end:#x})  (Loader-Fenster [{:#x}, {ram_end:#x}) reserviert)", loader::MOD_BASE);
    let (cap_slots, cap_objs) = system::cap_capacity();
    println!(
        "cap     : {cap_slots} Slots / {cap_objs} Objekte, Tabellen {} KiB aus dem RAM (Summe aller PD-Budgets: {})",
        cap_bytes >> 10,
        caprock_microkit::CAP_SLOTS_FOR_ALL_PDS
    );
    // A-3.4 Teil 4: IPC-Tabellen VOR dem ersten Endpoint — der Selbsttest und die Bringup-Kanäle
    // reservieren gleich welche. Meldet sich selbst (`ipc :`).
    system::configure_ipc();
    loader::probe(); // Boot-Archiv lesen + Module melden (L0; Laden folgt ab L1)
    #[cfg(feature = "selftest")]
    selftest::run();

    // Phase 4–6: Scheduler + cap-gesicherte IPC + Protection Domains.
    system::set_hooks(); // Reschedule- + Syscall-Hook registrieren (vor IRQs)
    // Kerneltabellen zur Boot-Zeit dimensionieren + alle per-Kern-Scheduler binden (vor spawn).
    let (nc, nthreads, per_core, tbl_bytes) = system::configure(cores);
    println!(
        "sched   : {nc} Kerne, {nthreads} Thread-Slots ({per_core} hostbar je Kern), Tabellen {} KiB aus dem RAM",
        tbl_bytes >> 10
    );
    system::init_core(); // Boot-Kontext von core 0 wird Idle-Thread
    #[cfg(feature = "selftest")]
    {
        threads::spawn_demo(); // 2 PDs (Client/Server) + 3 Worker auf core 0
        println!("sched   : Round-Robin + cap-gesicherte IPC (2 PDs + 3 Worker + Idle)");
    }

    // --- Root-Task (A-2.1/A-2.2) ---
    //
    // Dieselbe Stelle wie auf x86 (`arch::x86_64::bringup`) und aus demselben Grund **ausserhalb**
    // von `selftest`: das ist die Aufgabe des Kernels, nicht seine Pruefung. Bis A-2.2 rief den
    // Loader auf ARM ausschliesslich `threads/mod.rs` — also nur der Testcode. Ohne diesen Aufruf
    // ist der `--no-default-features`-Kernel auf ARM tatsaechlich leer, und das Gating waere kein
    // schlankerer Kernel, sondern ein Kernel ohne Zweck.
    // D5: erst das Autoritaetsdokument melden, dann den Root-Task starten. Genau diese Zeile fehlte
    // auf ARM -- die x86-Seite druckt sie seit A-1.4 (`bringup`), hier lief der Manifest-Pfad
    // ungeprueft mit. Ohne sie waere „root : ALL PASS" die einzige Aussage ueber ein Dokument, an
    // dem die gesamte Anfangsverteilung von Autoritaet haengt.
    loader::manifest_report();
    // **C8: der Verifizierer MUSS vor dem Root-Task stehen** -- er ist der erste, der `SYS_LOAD`
    // benutzt. Ohne ihn bekaeme sein erster Ladeversuch `ERR_SERVER_GONE`, und das saehe wie ein
    // Cap-Problem aus statt wie ein fehlender Thread.
    if !verifizierer::starten() {
        println!("verif   : FAILURES (Verifiziererthread liess sich nicht starten -- SYS_LOAD ist damit tot)");
    }
    let _root_ok = loader::start_root_task_reported();

    // Sekundärkerne via PSCI starten.
    println!("smp     : starte Kerne 1..{} via PSCI CPU_ON (hvc) ...", cores - 1);
    // Sekundär-Stacks aus dem RAM (ein Block, ein Slot je Kern; Slot 0 bleibt ungenutzt —
    // der Bootkern hat seinen Stack aus dem Linker-Image).
    let stacks = system::alloc_anywhere(cores as u64 * SEC_STACK_SIZE, SEC_STACK_SIZE)
        .expect("Sekundaer-Stacks: RAM erschoepft");
    SEC_STACKS_BASE.store(stacks.base(), core::sync::atomic::Ordering::Relaxed);
    let entry = _start_secondary as *const () as u64;
    // **Z6 stage 1 — the same policy as on x86, and it must exist here even though it is a no-op
    // under QEMU `virt`.** `MPIDR_EL1.MT` is clear on `cortex-a72`, so the topology reads `Single`
    // and nothing is suppressed. A line that exists on one architecture only is how `color` stood
    // hard-wired to `true` on aarch64 for months: the absence was not merely unproven, it was
    // invisible.
    // Z8/N0+N1: Topologie aus dem Device Tree, VOR dem Hochlauf der Sekundaerkerne.
    crate::numa::init(DTB_BYTES);
    let topo = hal::cpu::smt_topology();
    let mut occ = caprock_hal::smt::CoreOccupancy::new();
    let boot_id = hal::cpu::core_id() as u32;
    let boot_claim = occ.claim(&topo, boot_id);
    let mut online_ids = [0u32; caprock_sched::MAX_CORES];
    let mut online = 1usize;
    online_ids[0] = boot_id;
    let mut suppressed = 0usize;
    for core in 1..cores {
        let target = core as u64; // MPIDR Aff0 = Kernindex (QEMU virt, ein Cluster)
        match occ.claim(&topo, core as u32) {
            caprock_hal::smt::Claim::Admit => {}
            other => {
                suppressed += 1;
                println!("smp     : Kern {core} nicht zugelassen ({other:?}) -- Z6 Stufe 1");
                continue;
            }
        }
        let r = hal::power::cpu_on(target, entry, secondary_stack_top(core));
        if r != hal::power::SUCCESS {
            println!("smp     : CPU_ON für Kern {core} fehlgeschlagen (status {r})");
        } else {
            if online < online_ids.len() {
                online_ids[online] = core as u32;
            }
            online += 1;
        }
    }
    // Das Urteil rechnet aus den ONLINE-IDs nach und fragt `occ` NICHT.
    let verdict = caprock_hal::smt::judge(&topo, &online_ids[..online.min(online_ids.len())]);
    println!(
        "smt     : topology={topo:?} width={:?} logical={cores} online={online} \
         suppressed={suppressed} boot_claim={boot_claim:?}",
        topo.width()
    );
    println!(
        "smt     : readable={} no_shared_core={:?} contained={} -- policy=one-thread-per-physical-core \
         (Z6 stage 1). `no_shared_core=None` heisst UNENTSCHEIDBAR, nicht bestanden.",
        verdict.readable, verdict.no_shared_core, verdict.contained
    );
    // Was diese Zeile NICHT belegt: unter QEMU ist die Geschwisterbeziehung EMULIERT. Geprueft ist
    // die POLITIK, nie der KANAL -- dieselbe Unterscheidung wie beim SMMU-Befund (ADR 0008).
    // Auf `virt`/`cortex-a72` gibt es ueberdies gar kein SMT, die Zeile belegt hier also nur, dass
    // die Lesung `Single` ergibt und die Politik niemanden faelschlich unterdrueckt.
    println!("smt     : {}", if verdict.ok() { "ALL PASS" } else { "FAILURES" });
    SMT_OK.store(verdict.ok(), core::sync::atomic::Ordering::Release);

    // Z8/N1: melden, nachdem die Kerne stehen.
    crate::numa::bericht();
    NUMA_OK.store(crate::numa::urteil(), core::sync::atomic::Ordering::Release);

    // Alle Kerne haben ihren `CTR_EL0.CWG` gemeldet -> die DMA-Granularität steht fest. Bis
    // hierher galt die architektonische Obergrenze (s. `mmu::seal_cache_granule`).
    hal::mmu::seal_cache_granule();

    hal::cpu::local_irq_enable();
    // Ab hier läuft core 0 als Idle-Thread; der Timer-Tick schedult preemptiv.
    #[cfg(feature = "selftest")]
    threads::demo_report_then_idle();
    // Ohne `selftest` gibt es keinen Bericht, auf den zu warten waere: core 0 wird Idle-Thread,
    // und was laeuft, laeuft im Root-Task. `idle()` reapt weiter (Stacks beendeter Threads) und
    // wartet per WFI — dieselbe Schleife wie auf jedem Sekundaerkern.
    #[cfg(not(feature = "selftest"))]
    {
        println!("bringup : Kernel-Kern steht; Aufgaben kommen ab hier aus dem Root-Task (A-2.2)");
        idle();
    }
}

/// Kernel-Eintritt jedes Sekundärkerns (gerufen aus `_start_secondary`).
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn kernel_secondary_main() -> ! {
    // Vektoren vor der MMU setzen (Fault-Diagnose), dann MMU (gemeinsame Tabelle)
    // aktivieren -> erst danach Atomics/Konsole gültig.
    hal::exception::init();
    hal::mmu::init_secondary();
    init_core_irqs();

    let core = hal::cpu::core_id();
    println!("core {core}  : online (EL{}, MMU on)", hal::cpu::current_el());

    // Boot-Kontext dieses Kerns als Idle-Thread registrieren (vor IRQ-Freigabe).
    system::init_core();
    hal::cpu::local_irq_enable();
    idle();
}

/// Idle-Schleife: auf Interrupts warten (Low-Power).
#[cfg(target_arch = "aarch64")]
fn idle() -> ! {
    loop {
        // Jeder Kern sammelt seine EIGENEN beendeten Threads ein (per-Kern-Reaping):
        // gibt deren Stacks an den Allokator zurück. So lecken auch Threads, die auf
        // einem Sekundärkern enden (z. B. lastbewusst platzierte), keinen Speicher.
        system::reap();
        // NOHZ unverdrahtet (s. `system.rs` am Lastausgleich): plain `wfi`, wie bisher.
        hal::cpu::wfi();
    }
}
