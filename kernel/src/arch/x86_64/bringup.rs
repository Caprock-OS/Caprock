//! **Kernel-Kern-Bring-up auf x86_64** (ext-31, Stufe 4).
//!
//! Bis Stufe 3 war der x86-Zweig eine Kette von Hardware-Demos: Boot, Paging, IDT, LAPIC,
//! Ring-3-Round-Trip — jeweils direkt gegen die Hardware, ohne den eigentlichen Microkernel.
//! Hier läuft nun der **echte Kern**: derselbe `system.rs`, derselbe Capability-Space,
//! derselbe per-Kern-Scheduler, dasselbe cap-gesicherte IPC wie auf aarch64. Möglich wurde
//! das, weil `caprock-hal` jetzt architekturselektiv ist — der Kern selbst enthält kein
//! einziges `cfg(target_arch)`.
//!
//! Was hier läuft:
//!
//! | Prüfung | Aussage |
//! |---|---|
//! | `memtest`/`zerotest`/`captest`/`budget` | die arch-neutralen Selbsttests (Allokator, Datenremanenz, CDT/Refcounts, Cap-Budget) — **unverändert** dieselben wie auf ARM |
//! | `sched` | echte **Präemption**: der LAPIC-Timer verdrängt Threads über den Trap-Frame-Tausch |
//! | `ipc` | cap-gesicherter `CALL`/`RECV`/`REPLY` zwischen zwei Protection Domains |
//! | `audit` | Scheduler- + CDT-Audit nach dem Lauf sauber |
//!
//! Noch **nicht** auf x86 (s. `todo.md`): SMP (INIT-SIPI-SIPI), isolierte Adressräume
//! (PCID + per-VSpace-Tabellen), Ring-3-PDs im Kernel-Kern, Boot-Archiv/Loader, IOMMU.

use crate::system;
use caprock_abi::{result, sys};
use caprock_hal::{self as hal, print, println, syscall::invoke};
use caprock_mem::Rights;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Rückfall-RAM-Obergrenze, falls der Bootloader keinen Speicherplan mitgibt.
const RAM_END_FALLBACK: u64 = 128 * 1024 * 1024;

/// Zeitscheibe (Hz) — wie auf aarch64.
// **Eine Zahl, eine Stelle** (2026-08-27): die Tick-Rate steht in `main.rs` und wird hier nur noch
// benutzt. Vorher stand dieselbe 100 zweimal im Baum -- todo D16.
use crate::TICK_HZ;

// --- Telemetrie der Demo-Threads ------------------------------------------------------------

#[cfg(feature = "selftest")]
const NWORKERS: usize = 3;
/// So viele Runden muss jeder Worker schaffen, damit „Präemption läuft" belegt ist.
#[cfg(feature = "selftest")]
const WORK_TARGET: u64 = 3;
#[cfg(feature = "selftest")]
static WORKER_ROUNDS: [AtomicU64; NWORKERS] = [const { AtomicU64::new(0) }; NWORKERS];
#[cfg(feature = "selftest")]
static IPC_RESULT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static IPC_DONE: AtomicBool = AtomicBool::new(false);

/// Z4a: die TIDs, die der Haltepunkt-Test braucht (`0` = keine).
#[cfg(feature = "selftest")]
static WORKER_TID0: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static IPC_SERVER_TID: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static FREEZE_OK: AtomicBool = AtomicBool::new(false);

/// Worker: zählt Runden. Da er **nie** freiwillig abgibt, beweist steigender Fortschritt
/// aller Worker, dass der Timer-Interrupt sie gegeneinander verdrängt.
#[cfg(feature = "selftest")]
extern "C" fn worker(arg: usize) -> ! {
    loop {
        WORKER_ROUNDS[arg].fetch_add(1, Ordering::Relaxed);
        for _ in 0..200_000 {
            core::hint::spin_loop();
        }
    }
}

/// IPC-Server: verdoppelt die erste Nachricht und antwortet. Erreichbar **nur** über die
/// Endpoint-Cap in Slot 0 seiner PD.
#[cfg(feature = "selftest")]
/// **Warum der Server seine Schleife verlassen hat** — `u64::MAX` heisst „er ist noch drin".
///
/// D0-Diagnose (2026-08-07). Aus 50000 Laeufen kamen 9 Abweichungen, alle byte-identisch:
/// `bringup : offen waren: ipc`, `CALL(21) -> 0 (erwartet 42)` und `IPC-Rolle-abgewiesen=false`.
/// Die letzte Zeile prueft `freeze_thread(IPC_SERVER_TID)` und erwartet `Busy`, weil der Server in
/// `RECV` parkt — im Fehlerfall ist er **in keiner IPC-Rolle**. Zusammen mit dem haengenden
/// Client heisst das: der Server ist nicht mehr in der Schleife.
///
/// Wohin er gegangen ist, verschwieg der Rumpf: `if m.result != result::OK { break; }` warf den
/// Grund weg. Genau die Form, die dieses Projekt sonst jagt — ein Ausgang ohne Namen. Hier steht
/// er jetzt, und die Berichtszeile zeigt ihn.
#[cfg(feature = "selftest")]
static IPC_SERVER_EXIT: AtomicU64 = AtomicU64::new(u64::MAX);
/// Wieviele Anfragen der Server beantwortet hat, bevor er ausstieg.
#[cfg(feature = "selftest")]
static IPC_SERVER_BEDIENT: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "selftest")]
extern "C" fn ipc_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            // **Den Grund festhalten, bevor die Schleife endet.** Ohne diese Zeile ist der
            // Unterschied zwischen „ERR_BADCAP (Startrennen)", „ERR_QUIESCING" und
            // „ERR_EP_FULL" von aussen nicht zu sehen — und der Server dreht sich stumm weiter,
            // waehrend der Client fuer immer auf einen Empfaenger wartet.
            IPC_SERVER_EXIT.store(m.result, Ordering::Release);
            break;
        }
        IPC_SERVER_BEDIENT.fetch_add(1, Ordering::Relaxed);
        invoke(sys::REPLY, 0, [2 * m.msg[0], 0, 0, 0], 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

// ================================================================================================
// Z22 P2: MEHRERE THREADS JE PD — und die Messung, die Z23 S1 dadurch erst bekommt
// ================================================================================================
//
// Bis heute trug eine PD **einen** Thread: `Pd::thread` war ein Feld, `pd_of` ein linearer Scan
// darüber. Eine zweite Bindung überschrieb die erste, und der erste Thread verlor damit lautlos
// seinen ganzen Cspace — `pd_of` fand ihn nicht mehr, jeder seiner Syscalls endete in
// `ERR_NOPD`. Dieselbe Form wie „eine Ablage je ROLLE" (`CLIENT_NTFN`): eine Zelle für etwas,
// das es mehrfach gibt.
//
// Gemessen wird hier die **Wirkung**, nicht das Feld — an drei verschiedenen Größen:
//
//  1. **Derselbe Cspace.** Server und Client sind zwei Threads DERSELBEN PD und reden über
//     **denselben lokalen Cap-Slot** miteinander. Gelänge die Bindung nur einem von beiden,
//     bekäme der andere `ERR_NOPD` statt `OK`.
//  2. **Getrennte Grund-Mengen (Z24).** Der Server parkt sich; sein Rundenzähler muss stehen,
//     **während der des Clients weiterläuft**. Ohne die zweite Hälfte wäre „steht" von „ist
//     tot" nicht zu unterscheiden — und ohne die erste hieße eine gemeinsame Grund-Menge, dass
//     ein `PARK` des einen den anderen mitnimmt.
//  3. **Z23 S1: `REPLY` bleibt erlaubt.** Diese Zusicherung war *gebaut, aber nicht gemessen*,
//     und der Grund stand in der `qgate`-Zeile: es braucht eine **offene Transaktion**, also
//     zwei Threads in derselben PD. Genau die gibt es jetzt: der Client hängt in seinem `CALL`,
//     der Server hält den Reply-Token — und **in diesem Zustand** werden die Tore geschlossen.
//     `RECV` und `CALL` müssen dann `ERR_QUIESCING` geben, `REPLY` muss durchgehen, und der
//     Client muss seine Antwort bekommen.
//
// **Warum die Tore erst NACH dem Rendezvous zugehen** (und nicht wie bei `qgate` vorher): hier
// ist die offene Transaktion der Messgegenstand. Vorher geschlossen gäbe es sie gar nicht, und
// der Test würde wieder nur belegen, dass ein Tor schließt.
#[cfg(feature = "selftest")]
static PT_PD: AtomicU64 = AtomicU64::new(u64::MAX);
/// ThreadIds der beiden Threads DERSELBEN PD (0 = noch nicht veröffentlicht).
#[cfg(feature = "selftest")]
static PT_S_TID: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static PT_C_TID: AtomicU64 = AtomicU64::new(0);
/// Ergebniscodes — **roh**. „Abgewiesen" und „mit DIESEM Grund abgewiesen" sind zwei Aussagen.
#[cfg(feature = "selftest")]
static PT_RECV1_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(feature = "selftest")]
static PT_RECV2_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(feature = "selftest")]
static PT_CALL_GATE_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(feature = "selftest")]
static PT_REPLY_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
/// Was der Client aus seinem `CALL` zurückbekommt (erwartet 42) und ob er überhaupt fertig ist.
#[cfg(feature = "selftest")]
static PT_CLIENT_ANTWORT: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(feature = "selftest")]
static PT_CLIENT_FERTIG: AtomicBool = AtomicBool::new(false);
/// Rundenzähler beider Threads — die Größe, an der „läuft" ablesbar ist (kein Zustandsbit tut das).
#[cfg(feature = "selftest")]
static PT_S_RUNDEN: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static PT_C_RUNDEN: AtomicU64 = AtomicU64::new(0);
/// Handschlag Kernel -> Server: 1 = „die Tore sind zu, mach weiter"; der Server quittiert mit 2.
#[cfg(feature = "selftest")]
static PT_TOR_ZU: AtomicU64 = AtomicU64::new(0);
/// Handschlag Kernel -> Server: 1 = „parke dich"; der Server quittiert mit 2, bevor er parkt.
#[cfg(feature = "selftest")]
static PT_PARK_BITTE: AtomicU64 = AtomicU64::new(0);
/// Ergebnis der EINMALIGEN Messung (Bit 63 = gemessen, Bit 0 = Urteil). Das Urteil entsteht
/// HIER und nicht im Bericht — was der Bericht setzt, kann den Bericht nicht auslösen.
#[cfg(feature = "selftest")]
static PT_MESS: AtomicU64 = AtomicU64::new(0);

/// Der **Server**-Thread der Zwei-Thread-PD.
#[cfg(feature = "selftest")]
extern "C" fn pd_thread_server(_arg: usize) -> ! {
    // 1. Auf den Client warten. Gelingt die PD-Bindung nur einem der beiden, endet das hier
    //    sofort mit ERR_NOPD statt OK — der Code steht in der Prüfzeile.
    let m = invoke(sys::RECV, 0, [0; 4], 0);
    PT_RECV1_CODE.store(m.result, Ordering::Release);
    if m.result != result::OK {
        loop {
            core::hint::spin_loop();
        }
    }
    // Ab hier ist die Transaktion OFFEN: der Client hängt in seinem CALL, wir halten den
    // Reply-Token. Jetzt darf der Kernel die Tore schliessen.
    while PT_TOR_ZU.load(Ordering::Acquire) != 1 {
        core::hint::spin_loop();
    }
    PT_TOR_ZU.store(2, Ordering::Release);
    // 2. Was die PD ANFAENGT, muss abgewiesen werden — beide Wege einzeln.
    PT_RECV2_CODE.store(invoke(sys::RECV, 0, [0; 4], 0).result, Ordering::Release);
    PT_CALL_GATE_CODE.store(invoke(sys::CALL, 0, [7, 0, 0, 0], 0).result, Ordering::Release);
    // 3. Was schon LAEUFT, muss auslaufen duerfen. Das ist die Zusicherung, die Z23 S1 gebaut
    //    und nie gemessen hat.
    PT_REPLY_CODE.store(
        invoke(sys::REPLY, 0, [2 * m.msg[0], 0, 0, 0], 0).result,
        Ordering::Release,
    );
    // 4. Schlussschleife: der Rundenzaehler ist die Groesse, an der „laeuft" ablesbar ist.
    loop {
        PT_S_RUNDEN.fetch_add(1, Ordering::Relaxed);
        if PT_PARK_BITTE.load(Ordering::Acquire) == 1 {
            PT_PARK_BITTE.store(2, Ordering::Release);
            invoke(sys::PARK, 0, [0; 4], 0);
        }
        core::hint::spin_loop();
    }
}

/// Der **Client**-Thread derselben PD — gleicher Cspace, gleicher lokaler Cap-Slot.
#[cfg(feature = "selftest")]
extern "C" fn pd_thread_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [21, 0, 0, 0], 0);
    PT_CLIENT_ANTWORT.store(if r.result == result::OK { r.msg[0] } else { u64::MAX }, Ordering::Release);
    PT_CLIENT_FERTIG.store(true, Ordering::Release);
    loop {
        PT_C_RUNDEN.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

/// IPC-Client: ruft den Server über seine Send-Cap und hält das Ergebnis fest.
#[cfg(feature = "selftest")]
extern "C" fn ipc_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [21, 0, 0, 0], 0);
    IPC_RESULT.store(r.msg[0], Ordering::Release);
    IPC_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

// --- Ring-3-Threads (ext-32) ---------------------------------------------------------------
//
// Der Kernel-Code liegt in supervisor-only Seiten; Ring-3-Code braucht eigene, `US`-markierte
// Seiten. Deshalb steht er — wie die EL0-Demos auf aarch64 — in `.user_text`, samt seiner
// Syscalls: eine Ring-3-Funktion darf keine Kernel-Funktion aufrufen (die Seite ist für sie
// nicht ausführbar), also ist der `int 0x80` hier direkt eingebettet.
/// Der Zähler wird **aus Ring 3** hochgezählt und muss deshalb in einer `US`-schreibbaren Seite
/// liegen — die Kernel-Statics (`.bss`/`.data`) sind supervisor-only.
#[link_section = ".user_data"]
#[cfg(feature = "selftest")]
static USER_SYSCALLS: AtomicU64 = AtomicU64::new(0);

/// Z19/A4: auf wie vielen Kernen wurde SSE freigeschaltet? Muss `num_cores()` sein.
///
/// **Nicht hinter `selftest`** -- die Aufrufstelle steht im Hochlauf, der immer laeuft. Ein `cfg`
/// nur am Zaehler liess `--no-default-features` nicht mehr uebersetzen, und genau das prueft F1.
/// Acht Byte; ihn zu gaten waere Sparsamkeit an der falschen Stelle.
static SSE_CORES: AtomicU64 = AtomicU64::new(0);

/// Ring-3-Arbeiter: ruft den Kernel per Syscall (`SYS_YIELD`) und zählt die Runden.
#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn ring3_worker(_arg: usize) -> ! {
    loop {
        // SAFETY: `int 0x80` ist der für Ring 3 freigegebene Syscall-Vektor (IDT-Gate DPL 3);
        // der Kernel liest/schreibt nur die ABI-Register dieses Frames.
        unsafe {
            core::arch::asm!("int 0x80", in("rax") 0u64, in("rdi") 0u64,
                             lateout("rax") _, lateout("rdi") _, clobber_abi("sysv64"));
        }
        // Atomarer Zugriff auf eine `US`-schreibbare Seite (s. `USER_SYSCALLS`) — das ist eine
        // Instruktion, kein Aufruf in den (für Ring 3 nicht ausführbaren) Kernel-Code.
        USER_SYSCALLS.fetch_add(1, Ordering::Relaxed);
    }
}

// ================================================================================================
// PARK/UNPARK — SCHLAFEN OHNE KERNELOBJEKT (Z22 P4, 2026-08-09)
// ================================================================================================
//
// Ein Linux-Treiber schlaeft an vielen Stellen (`msleep`, `wait_event`, Completions, der
// bestrittene Zweig jedes Mutex). Jede dieser Warteschlangen auf eine **Notification** abzubilden
// hiesse: ein Kernelobjekt und einen Cap-Slot je Warteschlange — und `Notification` fasst ohnehin
// nur EINEN Wartenden (dieselbe Kapazitaetsform wie D11). Mit `PARK`/`UNPARK` liegt die
// Warteschlange als gewoehnliche Liste im Speicher der PD, und der Kernel kennt nur „schlafe" und
// „wecke Thread T".
//
// **Was diese Sonde belegt — und warum jeder einzelne Punkt gebraucht wird:**
//
//  1. `UNPARK(self)` gefolgt von `PARK` kehrt **sofort** zurueck. Ohne Weckmarke schliefe der
//     Faden hier fuer immer; das ist das verlorene Wecken, gegen das ein Futex seinen
//     Vergleichswert braucht.
//  2. **Positivkontrolle:** das ZWEITE `PARK` (ohne Marke) muss wirklich blockieren. Ohne diesen
//     Punkt waere Punkt 1 auch von einem `PARK` erfuellt, das gar nichts tut — und ein `PARK`,
//     das nie blockiert, besteht Punkt 1 mit Bestnote.
//  3. Nach einem `unpark` **von aussen** laeuft er weiter. Sonst waere ein `PARK`, das den Thread
//     kaputtmacht, von einem richtigen nicht zu unterscheiden (dieselbe Dreiteilung wie bei
//     `freeze`).
//  4. `UNPARK` auf eine **fremde** ThreadId wird abgewiesen. Fail-closed, nicht still.
//  5. Und die Aussage, die den D9-Fehler verhindert: `unpark` auf einen Thread, der **in IPC**
//     wartet, weckt ihn NICHT. Ein Bit, das zwei Blockadegruende traegt, macht den Wecker
//     unbestimmbar — dort hingen vier von fuenf D9-Befunden.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static PARK_PROBE_TID: AtomicU64 = AtomicU64::new(0);
/// Bit 0 = `PARK` mit Marke kehrte zurueck · Bit 1 = nach dem `unpark` von aussen weitergelaufen ·
/// Bit 2 = fremde ThreadId abgewiesen · Bit 3 = das zweite `PARK` wurde ueberhaupt erreicht.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static PARK_PROBE_BITS: AtomicU64 = AtomicU64::new(0);

/// Ein `int 0x80` aus Ring 3 mit einem Argument; gibt den Ergebniscode zurueck.
///
/// # Safety
/// Nur aus Ring-3-Kontext zu rufen. `int 0x80` ist der dafuer freigegebene Vektor (IDT-Gate
/// DPL 3); der Kernel liest/schreibt nur die ABI-Register dieses Frames.
#[cfg(feature = "selftest")]
#[inline(always)]
unsafe fn ring3_syscall1(nr: u64, a1: u64) -> u64 {
    let out: u64;
    // SAFETY: siehe Funktionsdoku.
    unsafe {
        core::arch::asm!("int 0x80", inlateout("rax") nr => out, in("rdi") a1,
                         clobber_abi("sysv64"));
    }
    out
}

#[cfg(feature = "selftest")]
#[link_section = ".user_text"]
extern "C" fn ring3_park_probe(_arg: usize) -> ! {
    // Auf die eigene ThreadId **warten**, statt sie vorauszusetzen. Sie wird erst nach der
    // Zulassung bekannt (`Parked` gibt sie nicht heraus, D0), und ein Faden, der ohne sie
    // losrechnet, waere genau der Fehler, gegen den dieser Typ gebaut ist: er wuerde `UNPARK(0)`
    // rufen und damit einen fremden Thread meinen.
    let mut me = PARK_PROBE_TID.load(Ordering::Acquire);
    while me == 0 {
        core::hint::spin_loop();
        me = PARK_PROBE_TID.load(Ordering::Acquire);
    }

    // 1. Marke an sich selbst, dann schlafen -> muss durchlaufen.
    // SAFETY: Ring-3-Kontext, freigegebener Syscall-Vektor.
    let r_self = unsafe { ring3_syscall1(caprock_abi::sys::UNPARK, me) };
    // SAFETY: dito.
    unsafe { ring3_syscall1(caprock_abi::sys::PARK, 0) };
    if r_self == caprock_abi::result::OK {
        PARK_PROBE_BITS.fetch_or(1, Ordering::Release);
    }

    // 2. Jetzt OHNE Marke. Bit 3 sagt „ich stehe gleich im zweiten PARK" -- ohne das waere ein
    //    Thread, der schon in Punkt 1 haengengeblieben ist, von einem blockierten nicht zu
    //    unterscheiden.
    PARK_PROBE_BITS.fetch_or(8, Ordering::Release);
    // SAFETY: dito.
    unsafe { ring3_syscall1(caprock_abi::sys::PARK, 0) };
    PARK_PROBE_BITS.fetch_or(2, Ordering::Release);

    // 3. Eine ThreadId, die es nicht gibt -- muss abgewiesen werden, nicht still nichts tun.
    // SAFETY: dito.
    let r_fremd = unsafe { ring3_syscall1(caprock_abi::sys::UNPARK, 0xDEAD_BEEF) };
    if r_fremd == caprock_abi::result::ERR_BADCAP {
        PARK_PROBE_BITS.fetch_or(4, Ordering::Release);
    }

    // 4. Danach: eine Runde je Aufwachen zaehlen. **Der Zaehler ist die Messgroesse fuer
    //    „verlorenes Pausieren"** -- ob ein Thread laeuft, ist an keinem Bit ablesbar, an einem
    //    Fortschritt schon. Dieselbe Unterscheidung wie bei der FP-Sonde: Ergebnis gegen Wirkung.
    loop {
        PARK_PROBE_RUNDEN.fetch_add(1, Ordering::Release);
        // SAFETY: dito.
        unsafe { ring3_syscall1(caprock_abi::sys::PARK, 0) };
    }
}

/// Runden der Park-Sonde in ihrer Schlussschleife (Z24). Liegt in `.user_data`, damit Ring 3 sie
/// ohne Cap hochzaehlen kann.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static PARK_PROBE_RUNDEN: AtomicU64 = AtomicU64::new(0);

// ================================================================================================
// Z23 S1 — DIE TORE EINER PD (Zwei-Phasen-Stilllegung)
// ================================================================================================
//
// **Was diese Sonde belegt, und was sie ausdruecklich NICHT belegt.**
//
// Belegt wird die erste Phase: eine PD mit geschlossenen Toren kann nichts **anfangen**
// (`RECV` -> `ERR_QUIESCING`), und nach dem Oeffnen kann sie es wieder. Das zweite ist die
// Sprechprobe, und sie ist der schwierigere Teil: „weist ab" ist von „ist kaputt" nur zu
// unterscheiden, wenn derselbe Aufruf danach **anders** ausgeht.
//
// **Gemessen wird der Unterschied SOFORT-ABWEISUNG gegen BLOCKIEREN**, nicht ein zweiter
// Ergebniscode. Nach dem Oeffnen gibt es keinen Sender, also blockiert das `RECV` -- und genau
// das ist der Beleg, dass es durch das Tor gekommen ist. Ablesbar ist es an der **Grund-Menge**
// aus Z24 (`IPC` gesetzt): der Umbau von heute frueh liefert Z23 sein Messinstrument.
//
// **NICHT belegt** ist S1b -- was ein FREMDER Aufrufer erlebt, der in diese PD hineinruft. Die
// Endpoints existieren weiter, und ein Dritter blockiert dort bis zum Thaw. Das ist eine offene
// Entwurfsentscheidung mit drei Optionen (todo Z23/S1b), und eine stillschweigende Wahl waere die
// schlechteste.
#[cfg(feature = "selftest")]
static Q_PROBE_TID: AtomicU64 = AtomicU64::new(0);
/// Der PD-Index der Sonde -- der Kernel muss ihre Tore wieder oeffnen koennen.
#[cfg(feature = "selftest")]
static Q_PROBE_PD: AtomicU64 = AtomicU64::new(u64::MAX);
/// Ergebnis der EINMALIGEN Messung: Bit 0..5 die Aussagen, Bit 63 „gemessen". Das Urteil entsteht
/// HIER und nicht im Bericht -- was der Bericht setzt, kann den Bericht nicht ausloesen.
#[cfg(feature = "selftest")]
static Q_MESS: AtomicU64 = AtomicU64::new(0);
/// Bit 0 = die Sonde lief · Bit 1 = das erste `RECV` gab `ERR_QUIESCING` · Bit 2 = sie steht
/// gleich im zweiten `RECV` (nach dem Oeffnen). Liegt in `.user_data` (US-schreibbar).
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static Q_PROBE_BITS: AtomicU64 = AtomicU64::new(0);
/// Der Ergebniscode des ERSTEN `RECV` -- roh, nicht als bool. „Abgewiesen" und „mit DIESEM Grund
/// abgewiesen" sind zwei Aussagen, und nur die zweite belegt, dass das Tor gefeuert hat und nicht
/// irgendetwas anderes.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static Q_PROBE_CODE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Ring-3-Sonde fuer Z23 S1. Liegt in `.user_text` — ohne das faultet sie an ihrer eigenen
/// Einsprungadresse, und das sieht wie ein kaputter Mechanismus aus (s. Fallenliste).
#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn ring3_quiesce_probe(_arg: usize) -> ! {
    let mut me = Q_PROBE_TID.load(Ordering::Acquire);
    while me == 0 {
        core::hint::spin_loop();
        me = Q_PROBE_TID.load(Ordering::Acquire);
    }
    Q_PROBE_BITS.fetch_or(1, Ordering::Release);
    // 1. Tore zu -> `RECV` muss SOFORT abweisen. Der Code wird abgelegt, nicht bloss ein Ja/Nein.
    // SAFETY: Ring-3-Kontext, freigegebener Syscall-Vektor.
    let r = unsafe { ring3_syscall1(caprock_abi::sys::RECV, 0) };
    Q_PROBE_CODE.store(r, Ordering::Release);
    Q_PROBE_BITS.fetch_or(2, Ordering::Release);
    // 2. Warten, bis der Kernel die Tore geoeffnet hat.
    // SAFETY: dito.
    unsafe { ring3_syscall1(caprock_abi::sys::PARK, 0) };
    // 3. Jetzt noch einmal dasselbe `RECV`. Es gibt keinen Sender -> es muss BLOCKIEREN.
    //    Bit 2 wird VORHER gesetzt: ein Thread, der schon in Schritt 1 haengengeblieben waere,
    //    ist von einem blockierten sonst nicht zu unterscheiden.
    Q_PROBE_BITS.fetch_or(4, Ordering::Release);
    // SAFETY: dito.
    unsafe { ring3_syscall1(caprock_abi::sys::RECV, 0) };
    // Hierher kommt sie nur, wenn das zweite `RECV` NICHT blockiert hat -- Bit 3 ist damit das
    // Gegenteil der erwarteten Aussage und faerbt die Zeile.
    Q_PROBE_BITS.fetch_or(8, Ordering::Release);
    loop {
        // SAFETY: dito.
        unsafe { ring3_syscall1(caprock_abi::sys::PARK, 0) };
    }
}

// ================================================================================================
// LAZY-FP AUF X86 — die Pruefzeile, die es bisher nur auf aarch64 gab (Z18, 2026-08-09)
// ================================================================================================
//
// **Warum das VOR jeder SSE-Entscheidung kommt.** Der Kernel hat Lazy-FP fuer Ring 3
// (`fxsave64`/`fxrstor64` mit `CR0.TS`-Trap, `crates/caprock-hal/src/x86_64/fp.rs`), aber gemessen
// wurde der Pfad nur auf aarch64 (`fp : EL0-FP-Threads-OK=0b11/0b11`). SSE im User-Ziel
// einzuschalten hiesse, einen Pfad scharfzustellen, der auf DIESER Architektur nie ausgefuehrt
// wurde — dieselbe Fehlerform wie beim Farbtest, nur gespiegelt.
//
// **Und die SSE-Entscheidung ist keine Leistungsfrage.** Gemessen am 2026-08-09: dieselbe Funktion
// `f64 -> f64` uebergibt ihre Argumente auf einem normalen x86_64-Ziel in `xmm0`/`xmm1`, auf
// `x86_64-caprock-user` dagegen in `rdi`/`rsi`. Das sind zwei AUFRUFKONVENTIONEN. Ein upstream
// gebautes musl nimmt die erste an; der Linker sieht nur gleiche Symbolnamen und kann die
// Verwechslung nicht bemerken. Das ist stille Korruption, kein langsamer Code — und damit ein
// Blocker fuer Z16 auf x86, unabhaengig von jeder Zyklenzahl.
//
// **Was diese Sonde belegt.** Zwei Ring-3-Threads laden je ein eigenes Muster in `xmm0..xmm3`,
// geben per `YIELD` ab (dabei stiehlt der andere die FP-Register) und pruefen nach jeder Rueckkehr,
// dass ihr Muster unveraendert ist. Nur korrektes Lazy-Save/Restore laesst das ueberstehen; ohne
// `fxsave` traegt der zweite Thread das Muster des ersten davon, und die Pruefung faellt durch.
//
// Eigener `global_asm!`-Block: der Kernel ist mit `-sse` uebersetzt, `xmm`-Registerklassen sind in
// `asm!` damit nicht benutzbar. Ein getrennter Block hat dieses Problem nicht — dieselbe Loesung
// wie der `.arch armv8-a`-Block auf aarch64.
#[cfg(feature = "selftest")]
const FP_ITERS: u64 = 64;
/// Bit `i` = FP-Thread `i` hat sein Muster ueber alle Abgaben behalten. Liegt in `.user_data`
/// (US-schreibbar), damit Ring 3 es ohne Cap setzen kann — wie `USER_SYSCALLS`.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static FP_OK: AtomicU64 = AtomicU64::new(0);

/// **Was die Sonde STATT ihres Musters vorfand** — je Sonde ein Wort (Z25).
///
/// `Muster = 0b0` kollabiert drei grundverschiedene Fehlerbilder in ein Bit, und genau das hat die
/// Diagnose eine Runde gekostet. Der gefundene Wert trennt sie:
///
/// * **Nullen** -> es wird ein frisch resetteter Slot restauriert (Reset-nach-Save-Reihenfolge,
///   oder Restore aus einem nie beschriebenen Slot).
/// * **das Muster des PARTNERS** -> Identitaetsvertauschung im Save/Restore-Pfad: der FP-Zustand
///   folgt der CPU statt dem Thread.
/// * **Muell** -> Layout-, Groessen- oder Ausrichtungsfehler.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static FP_FOUND: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Restliche Versuche der Sofortpruefung (von 1000). `1000` = beim ersten Versuch gelungen.
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static FP_TRIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// **Wie weit die Sonde in ihrer Schleife gekommen ist** (Z25). Weder Erfolgs- noch
/// Korruptionsbit gesetzt heisst: sie steckt darin — und dann sagt das *Ergebnis* nichts, der
/// *Fortschritt* aber alles. `FP_ITERS` = durchgelaufen, 0 = nie begonnen, dazwischen = sie kommt
/// voran, aber langsam (oder wurde beendet).
#[cfg(feature = "selftest")]
#[link_section = ".user_data"]
static FP_PROGRESS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// **Das FP-Urteil an EINER Stelle.** `all_done` und der Bericht rufen dieselbe Funktion.
///
/// Zwei Wirklichkeiten aus derselben Hand waren der `pdbind`-Fehler: eine Liste fuer den Bericht,
/// eine getrennte `&&`-Kette fuer das Urteil -- und drei Zeilen gatterten nichts. Hier gibt es
/// nur diese Funktion; der Bericht druckt die Bestandteile, entscheidet aber nicht.
///
/// **Was NICHT drinsteht, und warum:** „alle {FP_ITERS} Abgaben ueberstanden". Das konnte die
/// Sonde nie liefern (gemessen 3/64 bei gruener Sofortpruefung und null Korruptionen) -- sie kommt
/// je Rundlauf-Runde eine Iteration voran, und eine Runde ist durch den Tick begrenzt. Ein
/// Kriterium, das die geprueft Sache nicht erreichen kann, hat keine Trennschaerfe: gruen ist
/// unmoeglich, also sagt rot nichts.
#[cfg(feature = "selftest")]
fn fp_urteil() -> bool {
    let fpmask = FP_OK.load(Ordering::Relaxed);
    let gelaufen = fpmask & 0b100 != 0; // Sprechprobe: die Sonde lief und konnte schreiben
    let sofort_ok = (fpmask >> 3) & 0b11 == 0b11; // Muster schreiben/lesen OHNE jeden Wechsel
    // **Zwei, nicht eins**: der erste Restore laedt einen frisch genullten Slot, also *bevor* die
    // Sonde ihr Muster geschrieben hat. Erst der zweite belegt, dass ein GESCHRIEBENES Muster eine
    // Verdraengung ueberstanden hat. Dasselbe Argument fuer den Fortschritt.
    let verdraengt =
        system::fp_watch_restores(0) >= 2 && system::fp_watch_restores(1) >= 2;
    let fortschritt = FP_PROGRESS[0].load(Ordering::Relaxed) >= 2
        && FP_PROGRESS[1].load(Ordering::Relaxed) >= 2;
    let keine_korruption =
        FP_FOUND[0].load(Ordering::Relaxed) == 0 && FP_FOUND[1].load(Ordering::Relaxed) == 0;
    let nm: u64 = (0..system::num_cores()).map(system::nm_traps).sum();
    gelaufen
        && sofort_ok
        && verdraengt
        && fortschritt
        && keine_korruption
        && hal::fp::ts_vorgefunden() == 0
        && nm == 0
}

#[cfg(feature = "selftest")]
core::arch::global_asm!(
    r#"
.section .user_text,"ax"
.globl user_fp_probe_x86
user_fp_probe_x86:
    mov     r12, rdi              // r12 = Muster (Entry-Argument; der Spawn setzt nur EINS)
    // Die Erfolgsmaske aus dem Muster ableiten: niedrigstes Bit 0 -> Wert 1, 1 -> Wert 2. Damit
    // hat jede der beiden Sonden ihr eigenes Bit, ohne ein zweites Argument zu brauchen.
    mov     r15, r12
    and     r15, 1
    inc     r15
    // Sprechprobe: Bit 2 sagt „diese Sonde lief und konnte schreiben" -- BEVOR FP angefasst wird.
    // Ohne sie waere „0b00" nicht von „lief nicht" zu unterscheiden.
    lock or qword ptr [rip + {ok}], 4
    movq    xmm0, r12
    movq    xmm1, r12
    movq    xmm2, r12
    movq    xmm3, r12
    // **Sofortpruefung, OHNE jeden Wechsel** (Z25): Muster schreiben, sofort lesen. Sie halbiert
    // den Suchraum -- gruen entlastet die Register selbst, `enable_sse` und das Target; rot hiesse,
    // die Sonde misst etwas anderes, als sie glaubt.
    // **1000 Versuche, nicht einer** (Z25): "die Instruktion wirkt nie" und "ein Wechsel
    // dazwischen loescht" sehen bei EINEM Versuch gleich aus. Gelingt auch nur EINER, ist das
    // Schreiben in Ordnung und es ist ein Rennen; gelingt keiner, wirkt `movq xmm, r64` hier
    // ueberhaupt nicht -- und dann misst die Sonde etwas anderes, als sie glaubt.
    mov     rbx, 1000
5:
    movq    xmm0, r12
    movq    r14, xmm0
    cmp     r14, r12
    je      6f
    dec     rbx
    jne     5b
    // Kein einziger Versuch gelang -> den gefundenen Wert ablegen und weiter (die Schleife
    // unten faengt es ohnehin, aber der Wert soll HIER schon stehen).
    mov     rax, r12
    and     rax, 1
    shl     rax, 3
    lea     rcx, [rip + {found}]
    mov     [rcx + rax], r14
    jmp     4f
6:
    // **`shl rcx, cl` waere hier falsch** und war es einen Lauf lang: `cl` ist das niedrige Byte
    // von `rcx` SELBST. Der Schiebebetrag muss ueber `cl` kommen, also zuerst nach rcx.
    mov     rcx, r12
    and     rcx, 1
    mov     rax, 8
    shl     rax, cl                    // Bit 3 fuer Sonde 0, Bit 4 fuer Sonde 1
    lock or qword ptr [rip + {ok}], rax
    // Und wie viele Versuche es brauchte -- 1000 heisst "sofort", weniger heisst "es gab ein
    // Rennen und wie oft".
    mov     rax, r12
    and     rax, 1
    shl     rax, 3
    lea     rcx, [rip + {tries}]
    mov     [rcx + rax], rbx
4:
    movq    xmm0, r12
    movq    xmm1, r12
    movq    xmm2, r12
    movq    xmm3, r12
    mov     r13, {iters}
2:
    movq    r14, xmm0             // Muster nach jeder Abgabe pruefen
    cmp     r14, r12
    jne     3f
    movq    r14, xmm1
    cmp     r14, r12
    jne     3f
    movq    r14, xmm2
    cmp     r14, r12
    jne     3f
    movq    r14, xmm3
    cmp     r14, r12
    jne     3f
    // Fortschritt ablegen, BEVOR abgegeben wird -- sonst steht bei einem Thread, der beim Yield
    // haengt, eine Runde zu wenig da.
    mov     rax, r12
    and     rax, 1
    shl     rax, 3
    lea     rcx, [rip + {prog}]
    mov     rdx, {iters}
    sub     rdx, r13
    mov     [rcx + rax], rdx
    xor     eax, eax              // sys::YIELD -> FP-Besitz abgeben
    xor     edi, edi
    int     0x80
    dec     r13
    jne     2b
    lock or qword ptr [rip + {ok}], r15   // Erfolg vermerken (US-schreibbare Seite)
1:
    mov     rax, 5                // sys::PARK
    int     0x80
    jmp     1b
3:
    // Markierung "hier war ich" in Bit 63 -- sonst ist ein gefundener Wert 0 nicht vom
    // Anfangswert zu unterscheiden, und genau das hat eine Messrunde gekostet.
    bts     r14, 63
    // **Den GEFUNDENEN Wert ablegen, bevor geparkt wird.** Ohne ihn ist „nicht meins" die einzige
    // Aussage, und drei verschiedene Ursachen sehen gleich aus.
    mov     rax, r12
    and     rax, 1
    shl     rax, 3
    lea     rbx, [rip + {found}]
    mov     [rbx + rax], r14
    mov     rax, 5                // Korruption erkannt: OHNE Vermerk parken
    int     0x80
    jmp     3b
"#,
    iters = const FP_ITERS,
    ok = sym FP_OK,
    found = sym FP_FOUND,
    tries = sym FP_TRIES,
    prog = sym FP_PROGRESS,
);

#[cfg(feature = "selftest")]
extern "C" {
    /// Einsprung der Ring-3-FP-Sonde (s. `global_asm!` oben).
    static user_fp_probe_x86: u8;
}

/// Ring-3-Eindringling: liest **Kernel**-Speicher. Muss faulten — der Kernel beendet ihn und
/// läuft weiter (das x86-Gegenstück zum `el0iso`-Test auf aarch64).
#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn ring3_intruder(_arg: usize) -> ! {
    // SAFETY(-Absicht): Genau dieser Zugriff SOLL fehlschlagen. Das Kernel-Image liegt bei
    // 1 MiB in supervisor-only Seiten; ein Ring-3-Lesezugriff dorthin muss #PF auslösen.
    unsafe {
        let v = core::ptr::read_volatile(0x10_0000 as *const u64);
        core::ptr::write_volatile(0x20_0000 as *mut u64, v); // nie erreicht
    }
    loop {}
}

// --- Isolierte Adressräume (ext-33) ---------------------------------------------------------
//
// Eine **isolierte** PD bekommt einen eigenen Adressraum, in dem NUR ihre eigene Region
// user-zugänglich ist. Der Test ist derselbe wie auf aarch64 (`vspace`): zwei Ring-3-Threads
// lesen **dieselbe** Adresse in fremdem RAM — der SAS-Thread darf (gemeinsamer Adressraum),
// der isolierte muss faulten.
/// Prüfadresse in fremdem User-RAM (wird vom Kernel beschrieben, s. `spawn_demo`).
#[link_section = ".user_data"]
#[cfg(feature = "selftest")]
static ISO_PROBE_ADDR: AtomicU64 = AtomicU64::new(0);
/// Der SAS-Thread konnte lesen (und meldet den Wert).
#[link_section = ".user_data"]
#[cfg(feature = "selftest")]
static SAS_READ_OK: AtomicU64 = AtomicU64::new(0);
/// Erwarteter Wert an der Prüfadresse.
#[cfg(feature = "selftest")]
const PROBE_MAGIC: u64 = 0x5E14_1A4E_0BED_C0DE;

/// SAS-Ring-3-Thread: liest die Prüfadresse — im gemeinsamen Adressraum ist das erlaubt.
#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn sas_probe(_arg: usize) -> ! {
    let a = ISO_PROBE_ADDR.load(Ordering::Relaxed);
    // SAFETY: `a` zeigt auf eine vom Kernel angelegte, im SAS-Modell user-lesbare RAM-Zelle.
    let v = unsafe { core::ptr::read_volatile(a as *const u64) };
    SAS_READ_OK.store(v, Ordering::Relaxed);
    loop {
        // SAFETY: freigegebener Syscall-Vektor (s. `ring3_worker`).
        unsafe {
            core::arch::asm!("int 0x80", in("rax") 0u64, in("rdi") 0u64,
                             lateout("rax") _, lateout("rdi") _, clobber_abi("sysv64"));
        }
    }
}

// --- C7b: DIE TIEFENSONDE -------------------------------------------------------------------
//
// **Was sie prueft, und warum die Eichung es nicht kann.** Die Eichung von `userstackmark` prueft
// das MESSGERAET an einem Feld im BSS. Sie kann strukturell nicht pruefen, ob die Buchfuehrung
// (Thread-Slot -> Basis/Laenge der EL0-Region) auf die RICHTIGE Region zeigt: ein Eintrag, der
// auf irgendeine andere, genullte Region verweist, meldet brav „viel Luft" und besteht jede
// Eichung. Genau diese Verwechslung hat die alte Kapazitaetszeile entwertet (`kurven_arbeiter`
// in `.text`: 3040 PDs gezaehlt, deren Thread sofort starb).
//
// Die Sonde beruehrt deshalb ihren ECHTEN EL0-Stack bis zu einer BEKANNTEN krummen Tiefe, und
// der Kernel muss sie mit genau dieser Tiefe wiederfinden -- nach oben UND nach unten begrenzt.
// Die untere Schranke faengt „die Messung bleibt bei 0"; die obere faengt „die Messung meldet
// einfach die ganze Region".
//
// **Warum das Schreiben unterhalb von `RSP` hier gefahrlos ist** (und nicht etwa nur meistens):
// ein Interrupt oder eine Exception aus Ring 3 wechselt auf x86-64 IMMER den Stack -- die CPU
// laedt `RSP0` aus der TSS. Unterhalb des User-`RSP` legt niemand etwas ab; es gibt in diesem
// System auch keine Signal-Zustellung, die es taete. Dieselbe Aussage traegt den zweiten
// Summanden der EL0-Rechnung (s. `userstackmark::summe`).
/// Bit 0 = die Sonde hat fertig beruehrt. In `.user_data`, weil Ring 3 schreibt.
#[link_section = ".user_data"]
#[cfg(feature = "selftest")]
static TIEFENSONDE_BITS: AtomicU64 = AtomicU64::new(0);

/// Der Wert, mit dem die Sonde ihren Stack beruehrt. **Nicht null** ist die einzige Anforderung
/// (die Marke zaehlt Nullbytes); auffaellig ist er, damit er in einem Speicherauszug erkennbar ist.
#[cfg(feature = "selftest")]
const TIEFENSONDE_WERT: u64 = 0x7EF7_0000_5041_0DE7;

#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn ring3_tiefensonde(_arg: usize) -> ! {
    // SAFETY: reiner Ring-3-Code. Beschrieben wird ausschliesslich der eigene EL0-Stack, und zwar
    // `SONDE_TIEFE` Bytes UNTERHALB des aktuellen `RSP` -- ein Bereich, den auf x86-64 kein
    // Interrupt und keine Exception benutzt (Stackwechsel ueber `RSP0`). Die private Region der
    // PD ist um Groessenordnungen groesser als die Sondentiefe; darunter liegt ungemappte VA,
    // ein Fehler um eine Zehnerpotenz faultet also, statt fremden Speicher zu treffen.
    #[cfg(not(feature = "ustack-gegenprobe"))]
    unsafe {
        core::arch::asm!(
            "mov rax, rsp",
            "2:",
            "sub rax, 8",
            "mov qword ptr [rax], rdx",
            "sub rcx, 1",
            "jnz 2b",
            in("rcx") (crate::userstackmark::SONDE_TIEFE / 8) as u64,
            in("rdx") TIEFENSONDE_WERT,
            lateout("rax") _,
            lateout("rcx") _,
        );
    }
    // **Die Gegenprobe isoliert GENAU EIN Konjunkt** (`ustack-gegenprobe`): die Sonde meldet sich
    // fertig, ohne ihren Stack berührt zu haben. Damit bleibt `gemessen=true` und nur
    // `getroffen` fällt — jede andere Aussage der Zeile ist unberührt. Eine Mutation, die zwei
    // Dinge zugleich kaputtmacht, misst die Reihenfolge der Prüfungen und nicht die Eigenschaft
    // (D9, erste Fassung).
    TIEFENSONDE_BITS.fetch_or(1, Ordering::Release);
    loop {
        // Weiterlaufen statt sterben: gemessen wird sie am LEBENDEN Thread, damit die Messung
        // nicht davon abhaengt, wann der Idle-Thread das naechste Mal reapt. Der `int 0x80`
        // (YIELD) haelt sie praemptierbar und traegt nichts Tiefes bei -- er wechselt den Stack.
        // SAFETY: freigegebener Syscall-Vektor (s. `ring3_worker`).
        unsafe {
            core::arch::asm!("int 0x80", in("rax") 0u64, in("rdi") 0u64,
                             lateout("rax") _, lateout("rdi") _, clobber_abi("sysv64"));
        }
    }
}

/// ThreadId der Tiefensonde (`0` = keine).
#[cfg(feature = "selftest")]
static TIEFENSONDE_TID: AtomicU64 = AtomicU64::new(0);

/// **Die Sonde nachmessen, sobald sie fertig beruehrt hat** — aus der Hauptschleife, also VOR dem
/// Bericht. Eine Messung, die erst im Bericht entsteht, kann den Bericht nicht ausloesen.
///
/// Gepollt statt gewartet: „ist sie schon fertig" ist in jedem Durchlauf beantwortbar, und wenn
/// sie es nie wird, bleibt das Konjunkt falsch und der Lauf faellt sichtbar durch (`offen:
/// ustack`) — statt einen Erfolg zu erfinden.
#[cfg(feature = "selftest")]
fn tiefensonde_messen() {
    if crate::userstackmark::sonde_stand().0 {
        return; // schon gemessen
    }
    if TIEFENSONDE_BITS.load(Ordering::Acquire) & 1 == 0 {
        return; // sie beruehrt noch
    }
    let raw = TIEFENSONDE_TID.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    system::userstack_sonde_pruefen(caprock_sched::ThreadId::from_raw(raw));
}

/// Isolierter Ring-3-Thread: liest **dieselbe** Adresse. In seinem eigenen Adressraum ist sie
/// nicht user-gemappt -> #PF -> der Kernel beendet ihn.
#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn iso_probe(_arg: usize) -> ! {
    let a = ISO_PROBE_ADDR.load(Ordering::Relaxed);
    // SAFETY(-Absicht): Genau dieser Zugriff SOLL fehlschlagen — er liegt außerhalb der Region
    // dieser PD und ist in ihrem Adressraum nicht user-zugänglich.
    unsafe {
        let v = core::ptr::read_volatile(a as *const u64);
        core::ptr::write_volatile(a as *mut u64, v); // nie erreicht
    }
    loop {}
}

/// Stackgröße je Sekundärkern (aus dem RAM belegt, wie auf aarch64 seit ext-30).
const AP_STACK_BYTES: u64 = 64 * 1024;

/// Einstieg eines **Sekundärkerns** (aus dem AP-Trampolin, bereits im Long Mode mit den
/// Seitentabellen des BSP und eigenem Stack).
///
/// Dieselbe Reihenfolge wie beim Bootkern, nur ohne die globalen Schritte (Seitentabellen,
/// PIC-Stilllegung, Kerneltabellen — die stehen schon).
extern "C" fn ap_entry() -> ! {
    hal::exception::init(); // IDT ist global, das IDTR-Register aber pro Kern
    // **Eigene TSS, eigene IST-Stacks — oder gar nicht laufen.** Ohne TSS hat dieser Kern kein
    // `RSP0`; der erste Trap eines Ring-3-Threads auf ihm landete auf dem **User-Stack**, und das
    // ist ein direkter Privilegienbruch. Ein Kern ohne Wache darf deshalb nicht in den Scheduler:
    // `init_core()` würde ihn zum Idle-Thread machen und ihm damit Arbeit zuweisbar.
    if !hal::gdt::init_ap() {
        println!(
            "smp     : Kern {} bekommt KEINE TSS (Kapazitaet {}) -- er wird angehalten statt \
             ohne RSP0/IST zu laufen.",
            hal::cpu::core_id(),
            hal::gdt::kapazitaet()
        );
        hal::cpu::halt();
    }
    // **VOR `ap_report_online`**: der BSP druckt seinen Bericht erst, nachdem `cpu_on` den
    // Online-Zaehler hat steigen sehen. Was hier vorher gemessen ist, kann er lesen; alles danach
    // waere ein Rennen zwischen Messung und Bericht.
    super::ist::stacks_fuellen();
    super::ist::messen();
    hal::intc::init_cpu(); // eigener LAPIC
    hal::timer::init(TICK_HZ); // eigener Timer
    system::init_core(); // dieser Kontext wird der Idle-Thread dieses Kerns
    // **NACH `init_core()`**: davor hat dieser Kern keine Scheduler-Instanz, und ein `#NM` in
    // diesem Fenster findet niemanden. Aber VOR der Scheduler-Freigabe: `fxsave`/`fxrstor`
    // sichern XMM nur bei gesetztem `OSFXSR` zuverlaessig (SDM).
    if hal::fp::enable_sse() {
        SSE_CORES.fetch_add(1, Ordering::Relaxed);
    }
    hal::power::ap_report_online();
    hal::cpu::local_irq_enable();
    loop {
        system::reap(); // jeder Kern sammelt seine eigenen Zombies ein
        // NOHZ unverdrahtet (s. `system.rs` am Lastausgleich): plain `wfi`, wie bisher.
        hal::cpu::wfi();
    }
}

/// Alle Demo-Threads + PDs aufsetzen (vor dem Freigeben der Interrupts).
#[cfg(feature = "selftest")]
fn spawn_demo() -> bool {
    // Drei Worker auf dem Bootkern -> sie können nur durch Präemption alle vorankommen.
    for i in 0..NWORKERS {
        let Some(t) = system::spawn(worker as *const () as usize, i, system::IDLE_PRIO) else {
            return false;
        };
        if i == 0 {
            WORKER_TID0.store(t.to_raw(), Ordering::Release);
        }
    }

    // Ring-3-Threads: einer, der ordentlich per Syscall arbeitet, und einer, der Kernel-Speicher
    // liest und dafür beendet werden muss.
    if system::spawn_user(ring3_worker as *const () as usize, 0, system::IDLE_PRIO).is_none() {
        return false;
    }
    if system::spawn_user(ring3_intruder as *const () as usize, 0, system::IDLE_PRIO).is_none() {
        return false;
    }

    // Park-Sonde. Sie braucht ihre **eigene** ThreadId, und `Parked` gibt sie absichtlich nicht
    // heraus -- das ist der ganze Zweck des Typs (D0). Der Weg ist deshalb nicht „vorher lesen",
    // sondern: die Sonde **wartet**, bis der Kernel sie veroeffentlicht hat. Das ist rennfrei ohne
    // eine neue Kernel-Schnittstelle, die den Zeugen aufweichen wuerde.
    match system::spawn_user(ring3_park_probe as *const () as usize, 0, system::IDLE_PRIO) {
        Some(t) => PARK_PROBE_TID.store(t.to_raw(), Ordering::Release),
        None => return false,
    }

    // C7b: die Tiefensonde. **Isoliert** und nicht SAS -- gemessen werden soll genau die Sorte
    // Region, um die es geht (die private Region einer isolierten PD), und nicht der 64-KiB-
    // Stack eines SAS-Threads. Ihre ThreadId wird vor der Zulassung veroeffentlicht, damit die
    // Hauptschleife sie nachmessen kann, sobald die Sonde meldet.
    match system::spawn_isolated_parked(ring3_tiefensonde as *const () as usize, 0, system::IDLE_PRIO)
    {
        Some((p, _region)) => match system::admit(p) {
            Some(t) => TIEFENSONDE_TID.store(t.to_raw(), Ordering::Release),
            None => return false,
        },
        None => return false,
    }

    // --- Z23 S1: eine PD mit GESCHLOSSENEN Toren --------------------------------------------
    //
    // Die Tore werden zugemacht, **bevor** der Thread zugelassen wird. Andersherum haette die
    // Sonde ein Zeitfenster, in dem ihr `RECV` noch durchginge -- und ein Test, der von der
    // Reihenfolge zweier Ereignisse abhaengt, misst die Reihenfolge und nicht die Eigenschaft.
    // Dieselbe Lehre wie D0, nur eine Ebene hoeher.
    #[cfg(feature = "selftest")]
    {
        if let (Some(qep), Some(qpd)) = (system::create_endpoint(), system::create_pd()) {
            if let Ok(cap) = system::install_endpoint_cap(qep as u32, Rights::RW) {
                if system::install_pd_cap(qpd, 0, cap) {
                    Q_PROBE_PD.store(qpd as u64, Ordering::Release);
                    system::pd_quiesce(qpd, true);
                    if let Some(t) = system::spawn_parked(
                        ring3_quiesce_probe as *const () as usize,
                        0,
                        system::IDLE_PRIO,
                    ) {
                        if let Some(tid) = system::admit_in_pd(qpd, t) {
                            Q_PROBE_TID.store(tid.to_raw(), Ordering::Release);
                        }
                    }
                }
            }
        }
    }

    // --- Z22 P2: EINE PD, ZWEI THREADS ------------------------------------------------------
    //
    // Beide gehen ueber `admit_in_pd(pd, ..)` an DIESELBE PD -- das ist der ganze Umbau von
    // aussen: bis heute haette die zweite Bindung die erste ueberschrieben. Der Server wird
    // ZUERST zugelassen, damit er im `RECV` steht, bevor der Client ruft; ein CALL ohne
    // Empfaenger blockiert zwar korrekt, aber dann misst die Zeile die Reihenfolge zweier
    // Ereignisse statt der Eigenschaft (dieselbe Lehre wie bei `qgate`).
    #[cfg(feature = "selftest")]
    {
        if let (Some(pep), Some(ppd)) = (system::create_endpoint(), system::create_pd()) {
            if let Ok(cap) = system::install_endpoint_cap(pep as u32, Rights::RW) {
                if system::install_pd_cap(ppd, 0, cap) {
                    PT_PD.store(ppd as u64, Ordering::Release);
                    if let Some(sp) = system::spawn_parked(
                        pd_thread_server as *const () as usize,
                        0,
                        system::IDLE_PRIO,
                    ) {
                        if let Some(stid) = system::admit_in_pd(ppd, sp) {
                            PT_S_TID.store(stid.to_raw(), Ordering::Release);
                            if let Some(cp) = system::spawn_parked(
                                pd_thread_client as *const () as usize,
                                0,
                                system::IDLE_PRIO,
                            ) {
                                if let Some(ctid) = system::admit_in_pd(ppd, cp) {
                                    PT_C_TID.store(ctid.to_raw(), Ordering::Release);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // --- Z26/A3: EIN ECHTER UMGELEITETER SYSCALL --------------------------------------------
    //
    // Gast, Handler-PD und Binder werden hier aufgesetzt; gemessen wird unten in der Schleife.
    // Der Aufbau steht **hinter** den PD-Tests, damit er ihre Baseline nicht verschiebt, und
    // **vor** dem ersten `all_done` -- der Gast braucht Zeit fuer seinen Umlauf.
    #[cfg(feature = "selftest")]
    {
        system::handlermess::redirect_aufbau();
        // Die Sonde der `handler`-Zeile (der Kernel-Pruefpfad). Sie misst eine ANDERE Aussage als
        // `redirect` -- dass kein fremder Wecker den Handler-Grund aufhebt -- und braucht deshalb
        // einen eigenen Thread.
        system::handlermess::sonde_starten();
    }

    // Isolierter Adressraum vs. SAS: beide lesen dieselbe fremde Adresse.
    let Some(probe) = system::alloc(4096, 4096) else {
        return false;
    };
    // SAFETY: frisch allozierte, identity-gemappte RAM-Seite des Kernels.
    unsafe { core::ptr::write_volatile(probe.base() as *mut u64, PROBE_MAGIC) };
    ISO_PROBE_ADDR.store(probe.base(), Ordering::Relaxed);
    if system::spawn_user(sas_probe as *const () as usize, 0, system::IDLE_PRIO).is_none() {
        return false;
    }
    if system::spawn_isolated(iso_probe as *const () as usize, 0, system::IDLE_PRIO).is_none() {
        println!("iso     : spawn_isolated fehlgeschlagen");
        return false;
    }

    #[cfg(feature = "selftest")]
    {
        // Cache-Partitionierung (todo A1): zwei PDs mit disjunkten Farbsaetzen. Der Test baut sie
        // sofort wieder ab -- geprueft wird die Zuteilung, nicht ihr Programm.
        let c = crate::colors::run_color(iso_probe as *const () as usize, system::IDLE_PRIO);
        // Gemeldet wird in `colors::report_color` — EINE Druckstelle fuer beide Hochlaufwege.
        crate::colors::report_color(&c);
        // B-4.2: die Streifenvergabe fuehrt Belegung -- erschoepft heisst FEHLSCHLAG, nicht
        // stille Wiederholung. Laeuft NACH `run_color` (das baut seine PDs sofort wieder ab) und
        // vor allem, was selbst Streifen belegt, also im Ruhezustand.
        STRIPE_ALLOC_OK.store(crate::colors::run_stripe_alloc(), Ordering::Release);
        // B-4.5: die WIRKUNG der Faerbung, nicht nur die Zuteilung. Traegt die Positivkontrolle
        // nicht (Maschine kann Verdraengung nicht zeigen, z. B. TCG), ist das ein SKIP und darf
        // die Abschlussbedingung nicht blockieren -- aber die Bilanz muss immer stimmen.
        let pp = crate::colors::run_prime_probe();
        crate::colors::report_prime_probe(&pp);
        PPROBE_OK.store(
            (!pp.ran || pp.balanced) && (!pp.sensitive || pp.ok),
            Ordering::Release,
        );
        // A-4.4: Versionssperre des Laders, beide Ausgaenge.
        IFACE_GATE_OK.store(crate::loader::run_iface_gate(), Ordering::Release);
        // A-4.2: der ruhende Punkt -- die Torlogik, alle Ausgaenge. Laeuft auf einem lokalen
        // Endpoint-Objekt, legt also keinen benutzten Endpoint still.
        QUIESCE_OK.store(system::run_quiesce(), Ordering::Release);
        // A-4.1: atomares Umbinden -- alle Ausgaenge, ebenfalls auf einem lokalen Objekt.
        REBIND_OK.store(system::run_rebind(), Ordering::Release);
        // D11: der Ueberlauf einer Endpoint-Warteschlange ist benannt, nicht still. Ebenfalls
        // auf einem lokalen Objekt -- ein benutzter Endpoint wuerde hier 32 Leichen behalten.
        EPFULL_OK.store(system::run_epfull(), Ordering::Release);
        // A-4.3: die Zustandsuebergabe, alle Ausgaenge. Auf einer EIGENEN Scratch-Region -- der
        // Test darf den Zustand, dessen Ueberleben `ckpt` belegt, nicht selbst anfassen.
        STATE_OK.store(system::run_state(), Ordering::Release);
    }

    // Cap-gesichertes IPC: ein Endpoint, zwei PDs. Der Server hält die RECV-, der Client die
    // SEND-Cap — ohne Cap ist der Endpoint für beide unsichtbar (das ist die Autoritätsregel).
    let Some(ep) = system::create_endpoint() else {
        return false;
    };
    let Ok(root) = system::install_endpoint_cap(ep as u32, Rights::RWX) else {
        return false;
    };
    let (Ok(recv), Ok(send)) = (
        system::cap_mint(root, Rights::READ, 0),
        system::cap_mint(root, Rights::WRITE, 0),
    ) else {
        return false;
    };
    let (Some(srv_pd), Some(cli_pd)) = (system::create_pd(), system::create_pd()) else {
        return false;
    };
    if !system::install_pd_cap(srv_pd, 0, recv) || !system::install_pd_cap(cli_pd, 0, send) {
        return false;
    }
    // Z18: zwei Ring-3-FP-Sonden mit VERSCHIEDENEN Mustern. Verschieden ist der Punkt -- mit
    // demselben Muster wuerde ein fehlendes `fxsave` nicht auffallen, weil der Dieb genau das
    // zurueckliesse, was das Opfer erwartet.
    {
        let entry = core::ptr::addr_of!(user_fp_probe_x86) as usize;
        // Die beiden Muster unterscheiden sich im NIEDRIGSTEN Bit -- daraus leitet die Sonde ihr
        // Erfolgsbit ab (0 -> Bit 0, 1 -> Bit 1).
        for (i, muster) in [0x1234_5678_9ABC_DEF0u64, 0xA5A5_5A5A_3C3C_C3C3u64]
            .into_iter()
            .enumerate()
        {
            if let Some(t) = system::spawn_user_parked(entry, muster as usize, system::IDLE_PRIO) {
                // **Die Sprechprobe dieser Sonde ist ihre eigene Restore-Zahl, nicht die globale.**
                // `fp_switch_count()` zaehlt Wechsel irgendwo im Knoten; lagen beide Sonden auf
                // verschiedenen Kernen und haben einander nie verdraengt, sagt eine hohe Zahl
                // ueber ihr Muster **nichts** -- sie pruefen dann Register, die zwischen ihren
                // Abgaben niemand angefasst hat. Dieselbe Form wie `rx_used` gegen „Daten sind
                // angekommen".
                //
                // Eingetragen wird NACH der Zulassung, weil es die `ThreadId` erst dort gibt
                // (`Parked` gibt sie absichtlich nicht heraus). Der Zaehler ist damit eine
                // **Untergrenze** -- er verliert hoechstens die ersten Restores, und in der
                // Richtung, die den Test strenger macht statt nachsichtiger.
                if let Some(tid) = system::admit(t) {
                    system::fp_watch(i, tid);
                }
            }
        }
    }

    // **D0 sass genau hier** (gemessen 2026-08-07, 9 Treffer in 50 000 Laeufen). Vorher stand da
    //
    //     let (srv, cli) = (spawn(ipc_server, ..), spawn(ipc_client, ..));   // ab hier lauffaehig
    //     bind_pd(srv_pd, srv); bind_pd(cli_pd, cli);                        // Autoritaet erst hier
    //
    // Zwischen den beiden Zeilen liegt der ganze Aufbau des Clients: eine Stackbelegung unter der
    // MEM-Sperre, ein `sched.spawn`, ein `CAPS.write()`. Faellt der Server in dieses Fenster, macht
    // er sein erstes `RECV` mit LEEREM Cspace, bekommt `ERR_NOPD` und verlaesst seine Schleife --
    // fuer immer. Der Client wartet danach 61 s auf einen Empfaenger, den es nicht mehr gibt, und
    // der Rest des Knotens laeuft munter weiter (`ticks=6104` gegen 52). Deshalb sah es nie nach
    // einem Deadlock aus, und deshalb hat die Suite es 2300 Laeufe lang nicht gezeigt.
    //
    // `spawn_in_pd` parkt den Thread, bindet die PD und laesst ihn erst dann zu. Die Reihenfolge
    // ist damit keine Sorgfaltsfrage mehr: es gibt keinen Aufruf, der sie umdrehen koennte.
    // `_cli` -- die Client-`tid` wurde bisher nur fuer `bind_pd` gebraucht; `spawn_in_pd` erledigt
    // das. Sie wird trotzdem gebunden, damit der Fehlschlag beider Spawns EIN Muster bleibt.
    let (Some(srv), Some(_cli)) = (
        system::spawn_in_pd(srv_pd, ipc_server as *const () as usize, 0, system::IDLE_PRIO),
        system::spawn_in_pd(cli_pd, ipc_client as *const () as usize, 0, system::IDLE_PRIO),
    ) else {
        return false;
    };
    // Z4a: der IPC-Server steht spaeter in `RECV` -- er sieht von aussen ruhend aus und ist es
    // nicht. Genau daran wird die Absage geprueft.
    IPC_SERVER_TID.store(srv.to_raw(), Ordering::Release);

    // --- Z4 Stufe 2: das Subjekt bekommt einen UMFANG ------------------------------------------
    //
    // Ein Thread ohne PD hat einen leeren Cspace, und `classify_all` sagt dazu `Ok(())` -- was
    // „nichts spricht dagegen" heisst und nicht „es ist etwas dabei" (so steht es im Test von
    // `checkpoint.rs`). Ein Checkpoint ueber einen leeren Umfang belegte von Z4b genau nichts.
    //
    // Deshalb bekommt Worker 0 eine PD mit zwei Caps, und beide sind mit Absicht gewaehlt:
    //
    //   * eine **Speicher**-Cap -> wandert als `Region { len }`, ohne Physadresse. Die
    //     Zielmaschine legt sie hin, wo sie will, und faerbt neu.
    //   * eine **SchedContext**-Cap -> wandert MIT VORBEDINGUNG (`SameTickSemantics`). Ihre
    //     Zahlen sind Ticks, und ein Tick bedeutet auf einer Maschine ohne invarianten Zaehler
    //     etwas anderes (Z4f/B-5.1). Genau diese Vorbedingung steht spaeter im Checkpoint.
    //
    // **Am Ende von `spawn_demo`, nicht am Anfang:** die Farb- und Streifentests darueber messen
    // gegen eine Speicher-Baseline. Ein Test, der Speicher belegt, kippt baseline-empfindliche
    // Tests -- das hat dieses Projekt auf aarch64 schon bezahlt.
    if let (Some(mem), Some(pd)) = (system::alloc(4096, 4096), system::create_pd()) {
        if let (Ok(mcap), Ok(sc)) = (
            system::cap_install(mem),
            system::install_sched_context_cap(4, 10, Rights::RW),
        ) {
            if system::install_pd_cap(pd, 0, mcap) && system::install_pd_cap(pd, 1, sc) {
                // **Erklaerte Spaetbindung** (D0): Worker 0 laeuft hier laengst. Das ist kein
                // Rennen -- er BENUTZT keine Cap, die PD ist Gegenstand des Checkpoints. Der
                // Grund steht als Variante da, damit er im Bericht auftaucht statt im Zaehler
                // zu fehlen.
                system::bind_pd_late(
                    pd,
                    caprock_sched::ThreadId::from_raw(WORKER_TID0.load(Ordering::Acquire)),
                    system::SpaetbindungsGrund::CheckpointSubjektNachtraeglich,
                );
                CKPT_PD.store(pd as u32 + 1, Ordering::Release);
            }
        }
    }
    true
}

/// Badge, mit dem sich der Root-Task meldet (`programs/trusted/init`, `ROOT_BADGE`).
#[cfg(feature = "selftest")]
const ROOT_BADGE: u64 = 1 << 32;
/// Badge, mit dem sich `hello` meldet (`programs/userland/hello`, `HELLO_BADGE`).
#[cfg(feature = "selftest")]
const HELLO_BADGE: u64 = 0x4845_4C4F;
// Z15/W1 -- muessen zu `programs/userland/wasmhost` passen (Bits 40..43; 32..34 sind vergeben).
const WASM_INST: u64 = 1 << 40;
const WASM_RESULT: u64 = 1 << 41;
const WASM_REJECT: u64 = 1 << 42;
const WASM_TRAP: u64 = 1 << 43;
/// **Sprechprobe der WASM-PD** — dasselbe Bit, mit dem der Lader ihre Manifest-Notification
/// muenzt (`CLIENT_NTFN_BADGE`). Sie setzt es mit einem `signal` auf Slot 1, **ohne** jede
/// Cap-Operation und vor jedem WASM-Schritt.
const WASM_LEBT: u64 = crate::loader::CLIENT_NTFN_BADGE;
/// Sprechprobe des CAP-Pfads: eine `ccopy`-Kopie mit eigenem Badge, ebenfalls vor der Engine.
const WASM_CCOPY: u64 = 1 << 45;
// Tagged IPC capabilities, end to end (`programs/userland/ipcperm-cli`, bits 46..51). The client
// derives tagged endpoint capabilities, calls the server (`ipcperm-srv`) through them, and the
// server answers with the permission mask and id the KERNEL put in the badge word. Each bit is
// one fact; they must match the constants in the client.
/// Program id of the client in the load suite's manifest (the server is id 7).
#[cfg(feature = "selftest")]
const TEST_IPCPERM_PROGRAM_ID: u32 = 8;
#[cfg(feature = "selftest")]
const IPCPERM_UNTAGGED: u64 = 1 << 46;
#[cfg(feature = "selftest")]
const IPCPERM_TAGGED: u64 = 1 << 47;
#[cfg(feature = "selftest")]
const IPCPERM_ATTENUATED: u64 = 1 << 48;
#[cfg(feature = "selftest")]
const IPCPERM_DUP_GATED: u64 = 1 << 49;
#[cfg(feature = "selftest")]
const IPCPERM_WIDE_ID: u64 = 1 << 50;
#[cfg(feature = "selftest")]
const IPCPERM_NOT_ENDPOINT: u64 = 1 << 51;
/// A-3.1: `SYS_CDELETE` hat die Loader-Cap geloescht **und** die Autoritaet war danach weg.
#[cfg(feature = "selftest")]
const CDELETE_GONE_BADGE: u64 = 1 << 33;
/// A-3.1: ein Cap mit abgeleiteten Kopien wird abgewiesen und bleibt benutzbar.
#[cfg(feature = "selftest")]
const CDELETE_CHILDREN_BADGE: u64 = 1 << 34;
/// C2: ein DMA-Pool ueber `DRIVER_DMA_MAX_PAGES` wurde aus Ring 3 mit EIGENEM Code abgewiesen
/// (`programs/trusted/init`, `POOL_REFUSED_BADGE`).
#[cfg(feature = "selftest")]
const POOL_REFUSED_BADGE: u64 = 1 << 35;
/// B3: `BIND_IRQ` **ohne** `Irq`-Cap wurde aus Ring 3 mit `ERR_BADCAP` abgewiesen
/// (`programs/trusted/init`, `IRQ_UNAUTHORIZED_BADGE`).
///
/// Die Zahl steht hier ein zweites Mal, weil Kernel und `init` getrennt gebaut werden und kein
/// gemeinsames Modul haben — dieselbe Lage wie bei den vier Badges darueber. Wer sie verschiebt,
/// verschiebt sie an **beiden** Stellen; laeuft es auseinander, meldet die Zeile
/// `bind-ohne-cap-abgewiesen=false` bei korrekt gefahrener Absage.
#[cfg(feature = "selftest")]
const IRQ_UNAUTHORIZED_BADGE: u64 = 1 << 36;

/// A-4.4: hat die Versionssperre die geaenderte Schnittstellenversion abgewiesen?
#[cfg(feature = "selftest")]
static IFACE_GATE_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// B-4.2: hat die Streifenvergabe den Erschoepfungsfall sauber abgewiesen?
#[cfg(feature = "selftest")]
static STRIPE_ALLOC_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// B-4.5: Prime+Probe. `true` heisst auch "nicht messbar" (SKIP) -- ein Fehlschlag ist nur ein
/// Lauf, in dem die Positivkontrolle TRAEGT und die Faerbung trotzdem nichts bewirkt.
static PPROBE_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.2: virtio-pci-Transport auf x86.
static VIRTIO_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.2, Teil 1: lief der Transport, solange die IOMMU noch nicht sperrte?
static VIRTIO_XPORT_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.2: virtio-blk (Sektor gelesen -- und nach dem VT-d-Aufbau nicht mehr).
static VBLK_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.2, virtio-blk Teil 1: kam der Sektor an, solange die IOMMU noch nicht sperrte?
static VBLK_READ_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.2: virtio-net (ARP-Anfrage raus, ARP-Antwort rein).
/// **Der Ausgang der `vnet`-Sonde -- dreiwertig** (2026-08-25, s. `crate::befund`).
///
/// Vorher ein `AtomicBool`, und der SKIP-Zweig setzte ihn auf `true`. Ein `AtomicU8` statt eines
/// `Atomic<Befund>`, weil `Befund` kein `AtomicX`-Typ ist; die drei Werte stehen als Konstanten
/// daneben und `vnet_befund()` uebersetzt zurueck -- die Umwandlung an EINER Stelle.
static VNET_BEFUND: crate::befund::AtomicBefund = crate::befund::AtomicBefund::neu();

/// Der Ausgang der `vnet`-Sonde.
#[cfg(feature = "selftest")]
fn vnet_befund() -> crate::befund::Befund {
    VNET_BEFUND.lesen()
}
/// A-5.1: hat eine geladene Treiber-PD ihr Geraet bedient?
/// **Welchen Dienst der KERNEL-Testclient anspricht** (A-5.4) — die `program_id` des Blockdienstes
/// aus dem Manifest der Lade-Suite.
///
/// Ein echter Client nennt seinen Dienst im **Manifest** (`service_id`, A-5.4); der Kernel-Test
/// hat keinen Manifest-Eintrag und muss die Wahl deshalb hier treffen. Dass er sie **treffen
/// muss**, ist der Punkt: seit es zwei Treiber-Dienste gibt, liefert `driver_service()` bei
/// Mehrdeutigkeit bewusst `None` statt „den zuletzt geladenen" — und genau daran ist dieser Test
/// beim ersten Lauf mit zwei Treibern aufgelaufen, was richtig war.
///
/// Der Kernel-**Lader** kennt diese Zahl nicht; sie steht in einem Testpfad, nicht in
/// `loader::reload_driver`.
const TEST_BLK_SERVICE_ID: u32 = 3;
/// **Der geraetelose Dienst der Lade-Suite** (2026-08-25): `hello` traegt seit heute
/// `POLICY_PROVIDES_SERVICE`. Bewusst ein Programm mit KLEINEREM Index als sein Client -- `init`
/// laedt die Startmenge in Index-Ordnung, und ein Client vor seinem Dienst kann ihn nicht finden.
const DIENST_PROG_ID: u32 = 2;
/// Sein Client: `wasmhost` nennt `service_id = 2`.
const DIENST_CLIENT_PROG_ID: u32 = 6;
/// Die Treiber-PD, deren Zuteilung der Test misst — dieselbe Komponente.
const TEST_BLK_PROGRAM_ID: u32 = 3;
/// `program_id` der **Dateisystem**-PD im Manifest der Lade-Suite.
///
/// Sie muss genannt werden, seit es **zwei** Client-PDs gibt. Vorher fragte diese Folge nach
/// „der Client-Notification"; mit `wasmhost` als zweitem Client zeigte dieselbe Zelle auf
/// dessen Objekt, und die Folge wartete auf ein Badge, das dort nie ankommt (Bisect `a159b6b`).
const TEST_FS_PROGRAM_ID: u32 = 4;
/// `program_id` der **WASM**-PD im Manifest der Lade-Suite — der zweite Client.
const TEST_WASM_PROGRAM_ID: u32 = 6;


/// Ergebnis der EINMALIGEN Park-Messung: Bit 0..5 die sechs Aussagen, Bit 8..15 die
/// Sondenbits, Bit 63 „gemessen". **Das Urteil entsteht hier und nicht im Bericht** — es steht
/// in `all_done()`, und was der Bericht setzt, kann den Bericht nicht auslösen.
#[cfg(feature = "selftest")]
static PARK_MESS: AtomicU64 = AtomicU64::new(0);

/// **Z22 P4: schlafen und geweckt werden, ohne dass ein Kernelobjekt entsteht.**
///
/// Fuenf Aussagen, jede mit ihrem eigenen Zweck — s. den Block bei [`ring3_park_probe`].
/// Laeuft **einmal**, aus der Hauptschleife; das Ergebnis liegt danach in [`PARK_MESS`].
#[cfg(feature = "selftest")]
fn park_messen() {
    if PARK_MESS.load(Ordering::Acquire) != 0 {
        return; // schon gemessen
    }
    let ok = park_messen_inner();
    let b = PARK_PROBE_BITS.load(Ordering::Acquire) & 0xff;
    PARK_MESS.store((1 << 63) | (b << 8) | u64::from(ok), Ordering::Release);
}

#[cfg(feature = "selftest")]
fn park_messen_inner() -> bool {
    let warten = || {
        let t0 = hal::timer::ticks(0);
        let mut wache = 0u64;
        while hal::timer::ticks(0) < t0 + 3 && wache < 200_000_000 {
            core::hint::spin_loop();
            wache += 1;
        }
    };
    let raw = PARK_PROBE_TID.load(Ordering::Acquire);
    let Some(tid) = (raw != 0).then(|| caprock_sched::ThreadId::from_raw(raw)) else {
        println!("park    : FAILURES (keine Sonde -- ohne sie ist nichts gemessen)");
        return false;
    };
    // Ihr Zeitfenster: sie muss bis in das zweite `PARK` gekommen sein.
    for _ in 0..64 {
        if PARK_PROBE_BITS.load(Ordering::Acquire) & 8 != 0 {
            break;
        }
        warten();
    }
    let b = PARK_PROBE_BITS.load(Ordering::Acquire);
    let marke_wirkt = b & 1 != 0; // 1. PARK mit Marke kam zurueck
    let erreichte_zweites = b & 8 != 0;

    // 2. **Positivkontrolle.** Blockiert das zweite `PARK` wirklich? Ohne diese Zeile bestuende
    //    Punkt 1 auch ein `PARK`, das gar nichts tut.
    let blockiert = erreichte_zweites && (b & 2 == 0) && system::is_parked(tid);

    // 5. **Die D9-Aussage, und sie kommt VOR dem Wecken:** ein Thread, der in IPC wartet, darf
    //    von `unpark` nicht hochkommen. Gemessen am IPC-Server, der in `RECV` steht -- er sieht
    //    von aussen genauso „blockiert" aus wie ein geparkter, und genau das ist die Falle.
    let sraw = IPC_SERVER_TID.load(Ordering::Acquire);
    let ipc_bleibt = if sraw != 0 {
        let stid = caprock_sched::ThreadId::from_raw(sraw);
        // **Gemessen wird `blocked`, nicht `parked`.** Die erste Fassung las `is_parked` vorher
        // und nachher -- an einem IPC-Wartenden ist dieses Bit aber in BEIDEN Faellen falsch, ob
        // `unpark` ihn nun weckt oder nicht. Der Pruefer haette den Fehler, gegen den er gebaut
        // ist, strukturell nicht sehen koennen; die Gegenprobe hat das gezeigt.
        let vorher_geparkt = system::is_parked(stid);
        let vorher_blockiert = system::is_blocked(stid);
        system::unpark_thread(stid);
        !vorher_geparkt && vorher_blockiert && system::is_blocked(stid)
    } else {
        false // nicht anwendbar heisst hier NICHT bestanden -- ohne Server ist nichts gemessen
    };

    // 3. Wecken -> laeuft weiter.
    let geweckt_ok = system::unpark_thread(tid);
    let mut lief_weiter = false;
    for _ in 0..64 {
        if PARK_PROBE_BITS.load(Ordering::Acquire) & 2 != 0 {
            lief_weiter = true;
            break;
        }
        warten();
    }
    // 4. Fremde ThreadId abgewiesen (setzt die Sonde nach dem Weiterlaufen).
    let mut fremd_abgewiesen = false;
    for _ in 0..64 {
        if PARK_PROBE_BITS.load(Ordering::Acquire) & 4 != 0 {
            fremd_abgewiesen = true;
            break;
        }
        warten();
    }

    // 6. **VERLORENES PAUSIEREN** (Z24) -- die Aussage, die es bis zum 2026-08-10 nicht gab.
    //
    // Das ist NICHT dasselbe wie ein spurious wake, und keine `while`-Schleife auf der Warteseite
    // deckt es: hier geht eine **Autoritaetsentscheidung** verloren. Ein Debugger (oder der
    // Gruppenschnitt aus Z23) friert einen Thread ein, ein Geschwister ruft `UNPARK` -- und der
    // eingefrorene laeuft. Vorher war das formulierbar, weil `unpark` ueber `unblock` ging und
    // `blocked` EIN Bit war; seit der Grund-Menge entfernt jeder Wecker nur SEINEN Grund.
    //
    // Gemessen wird die **Wirkung**, nicht ein Bit: ein Rundenzaehler, den die Sonde in ihrer
    // Schlussschleife hochzaehlt. Ob ein Thread laeuft, ist an keinem Zustandsbit ablesbar, an
    // einem Fortschritt schon -- dieselbe Unterscheidung wie bei der FP-Sonde.
    let mut in_schlussschleife = false;
    for _ in 0..64 {
        if system::is_parked(tid) && PARK_PROBE_RUNDEN.load(Ordering::Acquire) > 0 {
            in_schlussschleife = true;
            break;
        }
        warten();
    }
    // Einfrieren geht ueber genau den Pfad, um den es geht: `freeze_thread` pausiert.
    let eingefroren = matches!(system::freeze_thread(tid), system::Freeze::Frozen);
    let runden_vor = PARK_PROBE_RUNDEN.load(Ordering::Acquire);
    // Jetzt das Geschwister-`UNPARK`. Es nimmt den PARK-Grund -- und **nur** den.
    let _ = system::unpark_thread(tid);
    let mut lief_trotz_pause = false;
    for _ in 0..64 {
        warten();
        if PARK_PROBE_RUNDEN.load(Ordering::Acquire) != runden_vor {
            lief_trotz_pause = true;
            break;
        }
    }
    let pause_haelt = in_schlussschleife && eingefroren && !lief_trotz_pause;
    // **Sprechprobe**: der Thread muss danach WIRKLICH wieder laufen. Ohne sie bestuende
    // `pause_haelt` auch ein Kernel, in dem die Sonde schlicht tot ist -- „bewegt sich nicht" ist
    // von „darf sich nicht bewegen" sonst nicht zu unterscheiden.
    let aufgetaut = system::thaw_thread(tid);
    let mut laeuft_nach_thaw = false;
    for _ in 0..256 {
        if PARK_PROBE_RUNDEN.load(Ordering::Acquire) != runden_vor {
            laeuft_nach_thaw = true;
            break;
        }
        warten();
    }

    let ok = marke_wirkt
        && blockiert
        && geweckt_ok
        && lief_weiter
        && fremd_abgewiesen
        && ipc_bleibt
        && pause_haelt
        && aufgetaut
        && laeuft_nach_thaw;
    println!(
        "park    : marke-wirkt={marke_wirkt} zweites-blockiert={blockiert} \
         geweckt={geweckt_ok} lief-weiter={lief_weiter} fremd-abgewiesen={fremd_abgewiesen} \
         ipc-bleibt-liegen={ipc_bleibt} bits={b:#06b}"
    );
    println!(
        "park    : Z24 verlorenes-Pausieren: in-schlussschleife={in_schlussschleife} \
         eingefroren={eingefroren} lief-trotz-pause={lief_trotz_pause} (muss false sein) \
         aufgetaut={aufgetaut} laeuft-nach-thaw={laeuft_nach_thaw} (Sprechprobe: sonst waere \
         „bewegt sich nicht\" von „darf sich nicht bewegen\" nicht zu unterscheiden) : {}",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **Z23 S1: die Tore einer PD — gemessen an der WIRKUNG, nicht am Bit.**
///
/// Sechs Aussagen, und die dritte ist die Sprechprobe:
///
/// 1. Die Sonde lief (sonst ist nichts gemessen).
/// 2. Ihr erstes `RECV` wurde **sofort** abgewiesen — und zwar mit `ERR_QUIESCING`. Der ROHE Code
///    steht in der Zeile: „abgewiesen" und „mit DIESEM Grund abgewiesen" sind zwei Aussagen, und
///    nur die zweite belegt, dass das Tor gefeuert hat und nicht irgendetwas anderes.
/// 3. Das Öffnen **ändert etwas** (`pd_quiesce(.., false)` gibt `true`) — ein Tor, das schon offen
///    war, belegt nichts.
/// 4. Nach dem Öffnen **blockiert** dasselbe `RECV`, statt abzuweisen. Das ist der eigentliche
///    Beleg: derselbe Aufruf, anderer Ausgang. Ablesbar an der **Grund-Menge** aus Z24.
/// 5. Die Sonde ist dabei nicht durchgelaufen (Bit 3 bleibt aus).
/// 6. `pd_is_quiescing` sagt danach `false` — der Zustand ist wirklich weg und nicht nur die
///    Wirkung ausgeblieben.
#[cfg(feature = "selftest")]
fn quiesce_messen() -> bool {
    if Q_MESS.load(Ordering::Acquire) & (1 << 63) != 0 {
        return Q_MESS.load(Ordering::Acquire) & 1 != 0;
    }
    let ok = quiesce_messen_inner();
    Q_MESS.store((1 << 63) | u64::from(ok), Ordering::Release);
    ok
}

#[cfg(feature = "selftest")]
fn quiesce_messen_inner() -> bool {
    let warten = || {
        let t0 = hal::timer::ticks(0);
        let mut wache = 0u64;
        while hal::timer::ticks(0) == t0 && wache < 5_000_000 {
            core::hint::spin_loop();
            wache += 1;
        }
    };
    let raw = Q_PROBE_TID.load(Ordering::Acquire);
    let pd = Q_PROBE_PD.load(Ordering::Acquire);
    let (Some(tid), true) = (
        (raw != 0).then(|| caprock_sched::ThreadId::from_raw(raw)),
        pd != u64::MAX,
    ) else {
        println!("qgate   : FAILURES (keine Sonde/PD -- ohne sie ist nichts gemessen)");
        return false;
    };
    let pd = pd as usize;
    // 1./2. Auf das Ergebnis des ersten `RECV` warten.
    let mut lief = false;
    for _ in 0..64 {
        if Q_PROBE_BITS.load(Ordering::Acquire) & 0b11 == 0b11 {
            lief = true;
            break;
        }
        warten();
    }
    let code = Q_PROBE_CODE.load(Ordering::Acquire);
    let abgewiesen = code == caprock_abi::result::ERR_QUIESCING;
    // 3. Tore oeffnen -- und das MUSS etwas aendern.
    let geoeffnet = system::pd_quiesce(pd, false);
    let zu_danach = system::pd_is_quiescing(pd);
    let _ = system::unpark_thread(tid);
    // 4./5. Jetzt muss dasselbe `RECV` BLOCKIEREN statt abzuweisen.
    let mut steht_im_zweiten = false;
    for _ in 0..64 {
        if Q_PROBE_BITS.load(Ordering::Acquire) & 4 != 0 {
            steht_im_zweiten = true;
            break;
        }
        warten();
    }
    let mut blockiert = false;
    for _ in 0..128 {
        if system::is_blocked(tid) {
            blockiert = true;
            break;
        }
        warten();
    }
    let durchgelaufen = Q_PROBE_BITS.load(Ordering::Acquire) & 8 != 0;
    let ok = lief && abgewiesen && geoeffnet && !zu_danach && steht_im_zweiten && blockiert
        && !durchgelaufen;
    println!(
        "qgate   : lief={lief} erstes-RECV-Code={code} (erwartet {} = ERR_QUIESCING) \
         oeffnen-aenderte-etwas={geoeffnet} danach-zu={zu_danach} steht-im-zweiten-RECV={steht_im_zweiten} \
         blockiert-jetzt={blockiert} (statt abgewiesen -- DAS ist der Beleg) durchgelaufen={durchgelaufen} : {}",
        caprock_abi::result::ERR_QUIESCING,
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    println!(
        "qgate   : {} (Z23 S1: die Tore haengen am SUBJEKT, nicht am Objekt -- an einem Endpoint \
         haengen auch fremde PDs, und ein Riegel dort fröre Dritte mit ein. `REPLY` bleibt \
         erlaubt, sonst koennte ein Server seine offene Antwort nicht loswerden und der Freeze \
         erzeugte genau den Deadlock, den er aufloesen soll. **OFFEN (S1b): was ein FREMDER \
         Aufrufer erlebt, der hineinruft** -- die Endpoints existieren weiter, drei Optionen \
         stehen in todo Z23/S1b, und eine stillschweigende Wahl waere die schlechteste). \
         **`REPLY bleibt erlaubt` wird seit Z22 P2 GEMESSEN** -- nicht hier, sondern in \
         `pdthrd`: dafuer braucht es eine offene Transaktion, also zwei Threads in derselben PD, \
         und die gibt es seit dem 2026-08-10)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **Wie viele PDs die Kurve anlegen soll** — Bauzeit-Parameter, Vorgabe 0 (aus).
///
/// **Parametrisierbar und nicht verdrahtet**, weil die Frage „wo bricht es wirklich" von der
/// Maschine abhängt (RAM, Kerne) und nicht von einer Zahl im Quelltext. `0` heisst aus: der
/// normale Suitenlauf ist dann bit-identisch zu einem ohne diesen Code — eine Messung, die
/// tausende PDs anlegt, hat in einem Lauf nichts zu suchen, der Baselines vergleicht.
///
/// Gesetzt über die Umgebung (`CAPROCK_SCALE_TARGET=…`); `kernel/build.rs` meldet die Variable
/// als Bau-Eingabe, sonst misst man beim nächsten Drehen wieder den Vorgängerstand — dieselbe
/// Falle wie bei den Linkerskripten.
#[cfg(feature = "selftest")]
const SCALE_TARGET: usize = match option_env!("CAPROCK_SCALE_TARGET") {
    Some(s) => konst_parse(s.as_bytes(), 0),
    None => 0,
};

/// **ISOLIERTE PDs statt SAS-PDs** (`CAPROCK_SCALE_ISOLIERT=1`) — und das ist eine ganz andere
/// Frage, nicht eine strengere Variante derselben.
///
/// Eine PD im **globalen** Adressraum zieht Seitentabellen-Speicher **gar nicht**: sie teilt sich
/// die Karte des Kernels. Genau deshalb kann eine Kurve über SAS-PDs den Topf, an dem `wasmhost`
/// bei SECHS Programmen gescheitert ist, strukturell nicht erreichen — sie fragt ihn nie.
/// Eine isolierte PD dagegen braucht eine eigene VSpace, eine ASID und einen Satz Tabellen.
///
/// Beide Kurven zu haben ist der Punkt: die eine misst Tabellen und Stacks, die andere den
/// dynamischen Vorrat. Nur eine zu fahren und „10 000 Prozesse" zu sagen wäre dieselbe
/// Verwechslung wie `rx_used` gegen „Daten sind angekommen".
#[cfg(feature = "selftest")]
const SCALE_ISOLIERT: bool = match option_env!("CAPROCK_SCALE_ISOLIERT") {
    Some(s) => konst_parse(s.as_bytes(), 0) != 0,
    None => false,
};

/// `usize` aus einem Byte-String in `const`-Kontext (kein `parse` in `const fn`).
#[cfg(feature = "selftest")]
const fn konst_parse(b: &[u8], acc: usize) -> usize {
    match b {
        [] => acc,
        [d, rest @ ..] if *d >= b'0' && *d <= b'9' => {
            konst_parse(rest, acc * 10 + (*d - b'0') as usize)
        }
        // Ein unlesbares Zeichen macht den Parameter **0** (= aus) statt eine Zahl zu raten.
        _ => 0,
    }
}

/// **Die Kapazitätskurve: wie weit trägt der Vorrat wirklich?** (C4 / A3)
///
/// Die Zahlen, die dieses Projekt über seine Kapazität führt, sind alle **statisch** —
/// PD-Tabelle, Thread-Slots, Cap-Slots. Die Schranke, an der es zuletzt tatsächlich gescheitert
/// ist, war ein **dynamischer** Vorrat: `SYS_LOAD` gab `NoResources`, und die Ressource war
/// Speicher für eine **Seitentabelle** — bei sechs Programmen. In keiner Kapazitätstabelle kommt
/// dieser Topf vor.
///
/// Deshalb misst diese Funktion nicht „geht N", sondern **den Füllstand über wachsendes N** und
/// benennt die Ressource, an der es endet. Ein einzelner grüner Lauf bei einer Zahl belegt die
/// Tabellengrössen und verfehlt die reale Schranke.
///
/// Jede Runde legt eine PD **mit einem Thread** an — das ist die Einheit, um die es geht (eine
/// PD ohne Thread ist kein Prozess), und sie zieht genau die Töpfe, die zur Debatte stehen:
/// PD-Slot, Thread-Slot, Kernel-Stack-RAM.
#[cfg(feature = "selftest")]
fn kapazitaet_kurve() {
    if SCALE_TARGET == 0 {
        println!(
            "kurve   : SKIP -- CAPROCK_SCALE_TARGET=0 (aus). Das ist eine BENANNTE Absage, kein \
             Schweigen: die Messung belegt tausende PDs und gehoert deshalb nicht in einen Lauf, \
             der Baselines vergleicht. Fahren mit tools/kapazitaet-messen.sh"
        );
        return;
    }
    let frei0 = system::total_free();
    let mut n = 0usize;
    let mut grund = "Ziel erreicht (keine Schranke gefunden -- die Kurve endet am Parameter, \
                     nicht am Vorrat)";
    // Schrittweite der Kurvenpunkte: zehn Stuetzstellen, mindestens jede.
    let schritt = (SCALE_TARGET / 10).max(1);
    while n < SCALE_TARGET {
        let Some(pd) = system::create_pd() else {
            grund = "PD-Tabelle erschoepft (create_pd -> None)";
            break;
        };
        // **`spawn_balanced_parked` und nicht `spawn_parked`** -- und der Unterschied IST ein
        // Messergebnis. Mit dem kernlokalen `spawn_parked` endete die Kurve bei **n=4987**, und
        // zwar bei `-m 512M` UND bei `-m 6G` (dort mit 5771 MiB frei): das ist keine
        // RAM-Schranke, sondern die **Hosting-Kapazitaet EINES Kerns**
        // (`je_kern * MIGRATION_HEADROOM` = 2500 * 2 = 5000, abzueglich der schon lebenden
        // Threads). Wer alle Prozesse aus einem Thread heraus erzeugt, trifft die Kernschranke,
        // nicht die Systemschranke -- und haelt sie fuer die Kapazitaet des Systems.
        let geparkt = if SCALE_ISOLIERT {
            // **EL0-Einsprung aus `.user_text`**, nicht der Kernel-Arbeiter -- s. dort. Mit dem
            // falschen Einsprung faultet jeder einzelne Thread sofort, und die Kurve misst das
            // Anlegen statt das Bestehen.
            system::spawn_isolated_parked(
                kurven_arbeiter_el0 as *const () as usize,
                n,
                system::IDLE_PRIO,
            )
            .map(|(p, _region)| p)
        } else {
            system::spawn_balanced_parked(kurven_arbeiter as *const () as usize, n, system::IDLE_PRIO)
        };
        let Some(p) = geparkt else {
            let _ = system::free_pd_slot(pd);
            grund = if SCALE_ISOLIERT {
                "VSpace/ASID, Seitentabellen-Speicher, Thread-Slot oder Kernel-Stack-RAM \
                 erschoepft (spawn_isolated_parked -> None) -- WELCHER, sagt der Mangel unten"
            } else {
                "Thread-Slot oder Kernel-Stack-RAM erschoepft (spawn_balanced_parked -> None)"
            };
            break;
        };
        if system::admit_in_pd(pd, p).is_none() {
            grund = "Zulassung fehlgeschlagen (admit -> None)";
            break;
        }
        n += 1;
        if n % schritt == 0 || n == SCALE_TARGET {
            let (pds_u, pds_c, _, _, _, _) = system::vorrat_fuellstand();
            let (ps, _, po, _) = system::cap_peaks();
            let (cs, co) = system::cap_capacity();
            // **Der Seitentabellen-Topf ueber wachsendes N** (C7). Er ist der Kandidat fuer „was
            // reisst als naechstes", und bis heute hatte er keine Kurve: die SAS-Reihe fragt ihn
            // strukturell nie (eine PD im globalen Adressraum bekommt keine eigene VSpace), die
            // isolierte Reihe erschlaegt ihn mit der privaten 2-MiB-Region. Hier steht er als
            // eigene Zahl daneben -- gezaehlt an der Quelle, nicht als RAM-Differenz.
            let (_pt_raus, _pt_zur, pt_gehalten, pt_peak) = system::seitentabellen_topf();
            println!(
                "kurve   : n={n} · PDs {pds_u}/{pds_c} · Threads {}/{} · Cap-Slots {ps}/{cs} · \
                 Cap-Objekte {po}/{co} · freie VSpaces {} · Seitentabellen {pt_gehalten} Rahmen \
                 = {} KiB (Hoechststand {pt_peak}, {} Byte je Prozess) · freies RAM {} MiB \
                 (Start {} MiB, verbraucht {} KiB je PD+Thread)",
                system::thread_capacity() - system::threads_available(),
                system::thread_capacity(),
                system::free_vspaces(),
                (pt_gehalten * 4096) >> 10,
                (pt_gehalten * 4096) / (n as u64),
                system::total_free() >> 20,
                frei0 >> 20,
                (frei0.saturating_sub(system::total_free())) / (n as u64) >> 10
            );
        }
    }
    // Die benannte Ressource -- **seit 2026-08-10 auch von den `spawn_*`-Pfaden**, nicht mehr nur
    // von `SYS_LOAD`. Vorher stand hier in JEDEM Abbruch `keiner`, auch bei 2 MiB freiem RAM.
    let (mcode, mbytes, mfrei) = system::lade_mangel();
    let (pt_raus, pt_zur, pt_gehalten, pt_peak) = system::seitentabellen_topf();
    println!(
        "kurve   : ENDE bei n={n} von {SCALE_TARGET} -- Grund: {grund}. Zuletzt gemeldeter \
         Allokationsmangel: {} (angefordert {mbytes} Byte, frei waren {mfrei}). Freies RAM \
         {} MiB von {} MiB am Start. Je Prozess kostete es {} KiB (Kernel-Thread: {} KiB Stack \
         + Seitentabellen/PD-Metadaten; ein EL0-Thread haette {} KiB EL1-Stack).",
        system::mangel_name(mcode),
        system::total_free() >> 20,
        frei0 >> 20,
        if n > 0 {
            (frei0.saturating_sub(system::total_free())) / (n as u64) >> 10
        } else {
            0
        },
        system::stack_bytes() >> 10,
        system::USER_KSTACK_SIZE >> 10
    );
    println!(
        "kurve   : Seitentabellen-Topf am Ende -- {pt_gehalten} Rahmen = {} KiB gehalten \
         (Hoechststand {pt_peak}, {pt_raus} geholt / {pt_zur} zurueck), {} Byte je Prozess bei \
         n={n}, {} belegte VSpaces. Die SAS-Reihe fragt diesen Topf STRUKTURELL nie (eine PD im \
         globalen Adressraum bekommt keine eigene VSpace) -- steht hier eine Null bei n in den \
         Tausenden, ist das ein Befund ueber den Aufbau und kein Messwert",
        (pt_gehalten * 4096) >> 10,
        if n > 0 { (pt_gehalten * 4096) / (n as u64) } else { 0 },
        system::used_vspaces()
    );
    // **C7b: die LEBENDIGKEIT -- und sie ist der Grund, warum diese Zeile ueberhaupt existiert.**
    //
    // Diese Reihe hat am 2026-08-10 Leichen gezaehlt: `kurven_arbeiter` lag in `.text`, jeder
    // Thread faultete an seiner Einsprungadresse, und „3040 isolierte Prozesse" hiess „3040 mal
    // eine PD angelegt, deren Thread sofort starb". Der Einsprung ist seither richtig -- aber die
    // KURVE hatte danach immer noch keine Zahl, die das BELEGT. Sie stand nur nicht mehr im
    // Verdacht.
    //
    // Jetzt steht sie hier, und sie misst die WIRKUNG: jeder Arbeiter legt vor dem Parken ein von
    // Null verschiedenes Wort auf seinen (genullt ausgegebenen) EL0-Stack. `benutzt` zaehlt die
    // Regionen, in denen das angekommen ist.
    //
    // **Und der Sprechprobe-Vorbehalt gehoert dazu**: bei der SAS-Reihe ist `lebend` strukturell
    // 0 -- ein Kernel-Thread hat keine EL0-Region. Die Zeile sagt das selbst, statt eine Null zu
    // drucken, die wie ein Befund aussieht.
    let (leb, ben) = system::userstack_lebendig_zaehlen();
    println!(
        "kurve   : LEBENDIGKEIT -- {leb} lebende EL0-Regionen registriert, davon {ben} mit einer \
         Spur ihres Threads (er legt vor dem Parken ein von Null verschiedenes Wort auf den \
         genullt ausgegebenen Stack). Bei n={n} muessen beide Zahlen zu n passen; {} \
         **Gemessen wird die WIRKUNG, nicht ein Tabelleneintrag** -- genau daran ist die alte \
         Fassung dieser Reihe gescheitert (Arbeiter in `.text`, jeder Thread tot, Kurve gruen)",
        if SCALE_ISOLIERT {
            "eine Luecke zwischen leb und ben heisst: Threads, die nie gerechnet haben."
        } else {
            "In DIESER Reihe (SAS) ist 0/0 der erwartete Wert: ein Kernel-Thread hat keine \
             EL0-Region, es ist also nichts zu belegen und nichts behauptet."
        }
    );
}

/// Arbeiter der Kapazitätskurve **für die SAS-Reihe**: ein KERNEL-Thread (Ring 0), der sofort
/// parkt. Es geht um die **Verwaltung** vieler gleichzeitig existierender Prozesse, nicht um
/// Rechenlast (dieselbe Wahl wie `scale_worker` auf aarch64).
#[cfg(feature = "selftest")]
extern "C" fn kurven_arbeiter(_arg: usize) -> ! {
    invoke(sys::PARK, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Arbeiter der Kapazitätskurve **für die ISOLIERTE Reihe** — und der Unterschied ist keine
/// Formsache, sondern war ein Messfehler.
///
/// Eine isolierte PD bekommt einen **EL0**-Thread. `kurven_arbeiter` liegt in `.text`, also in
/// supervisor-only Seiten: der Thread faultet an seiner eigenen Einsprungadresse, bevor er eine
/// einzige Instruktion ausführt. Gemessen am 2026-08-10: **228 `el0-trap`-Zeilen in einem Lauf,
/// der 224 isolierte PDs anlegt** — jede einzelne. Am Ende der Kurve standen **0 belegte
/// VSpaces**, der Seitentabellen-Topf war auf 15 Rahmen zurückgefallen (Höchststand 215 bei
/// n=224), und die Zahl „3040 isolierte Prozesse" der C7-Tabelle war in Wahrheit „3040 mal eine
/// PD angelegt, deren Thread sofort starb".
///
/// Das ist die Falle aus `CLAUDE.md` wörtlich („Ring-3-Code gehört in `.user_text`, und die
/// Fehlermeldung dafür sieht aus wie ein Kernelfehler") — nur hat sie hier keine Prüfzeile rot
/// gefärbt, sondern eine **Kapazitätszahl** erzeugt, die etwas anderes misst als ihr Name sagt.
///
/// Der `int 0x80` steht direkt hier: eine Ring-3-Funktion darf keine Kernel-Funktion aufrufen
/// (`invoke` liegt in `.text`), sonst wandert der Fault nur eine Ebene tiefer.
/// Die Spur, die ein Kurvenarbeiter auf seinem EL0-Stack hinterlässt. Nur „nicht null" ist
/// gefordert (die Marke zählt Nullbytes); auffällig, damit sie in einem Auszug erkennbar ist.
#[cfg(feature = "selftest")]
const KURVE_LEBENSSPUR: u64 = 0x1EBE_4D16_0000_0001;

#[link_section = ".user_text"]
#[cfg(feature = "selftest")]
extern "C" fn kurven_arbeiter_el0(_arg: usize) -> ! {
    // **Eine SPUR auf dem eigenen Stack, und sie ist der Lebendigkeitsbeleg der Kurve** (C7b).
    //
    // Die private Region wird genullt ausgegeben; ein von Null verschiedenes Wort darin kann nur
    // von diesem Thread stammen. `system::userstack_lebendig_zaehlen()` zählt am Ende der Kurve
    // genau das — und beantwortet damit die Frage, an der diese Reihe schon einmal gescheitert
    // ist („3040 Prozesse", deren Threads alle sofort starben), mit einer WIRKUNG statt mit einem
    // Eintrag in einer Tabelle.
    //
    // SAFETY: geschrieben wird 8 Byte unterhalb des eigenen `RSP`, also in den eigenen EL0-Stack.
    // Auf x86-64 legt dort niemand etwas ab: ein Trap aus Ring 3 wechselt über `RSP0` den Stack.
    unsafe {
        core::arch::asm!("mov qword ptr [rsp - 8], rax", in("rax") KURVE_LEBENSSPUR);
    }
    // SAFETY: `int 0x80` ist der für Ring 3 freigegebene Syscall-Vektor (s. `ring3_worker`).
    // `PARK` braucht KEINE Cap und keine PD — der Dispatch kehrt vor der Cap-Auflösung zurück.
    unsafe {
        core::arch::asm!("int 0x80", in("rax") sys::PARK, in("rdi") 0u64,
                         lateout("rax") _, lateout("rdi") _, clobber_abi("sysv64"));
    }
    loop {
        core::hint::spin_loop();
    }
}

/// **C7: der SEITENTABELLEN-TOPF — die drei Konjunkte, an EINER Stelle.**
///
/// Der Topf wird an der Quelle gezählt (`system::pt_rahmen`), nicht als Differenz des freien RAM
/// — eine Differenz misst Kernel-Stacks, private Regionen und Segmente mit und beantwortet die
/// Frage nach *diesem* Topf so wenig wie `rx_used` die Frage nach angekommenen Daten.
///
/// * `raus > 0` — **Sprechprobe der Allokationsseite.** Ohne sie bestünde die Zeile auch ein
///   System, in dem gar nicht gezählt wird; eine Null wäre dann von „keine Tabellen nötig" nicht
///   zu unterscheiden.
/// * `zurueck > 0` — **Sprechprobe der Freigabeseite.** Der Abbau kommt in jedem Lauf vor
///   (Farbtest und Churn-Test bauen isolierte PDs ab); wird die Buchung dort entfernt, fällt das
///   hier auf und nicht erst, wenn der Füllstand unerklärlich wächst.
/// * `raus >= zurueck` — die **Bilanz, und der schärfste der vier.** Es kann nichts zurückkommen,
///   was nie herausgegeben wurde. Die Freigabeseite zählt, was die Einsammler des HAL
///   **tatsächlich** liefern; fehlt die Buchung an *irgendeiner* der acht Allokationsstellen, ist
///   die Rückgabe größer als die Ausgabe, sobald die betroffene VSpace abgebaut wird. Gemessen in
///   der Hauptsuite: 17 raus / 17 zurück — der Topf schließt auf null, und das ist zugleich die
///   erste Leckprüfung, die dieser Topf je hatte.
/// * `gehalten >= PT_RAHMEN_JE_VSPACE * used_vspaces` — der **Quervergleich gegen eine zweite,
///   unabhängige Buchführung** (die `VSPACES`-Tabelle). Jede belegte VSpace hält mindestens ihre
///   L1 und ihre L2.
///   **Ehrlich dazu:** in der HAUPTSUITE ist dieser Konjunkt gehaltlos — dort sind am Ende null
///   VSpaces belegt, die Aussage lautet `0 >= 0`. Er beißt in der LADE-Suite, wo geladene PDs
///   leben. Ein Konjunkt, der in einer Suite nichts prüft, muss das sagen; sonst liest sich seine
///   grüne Farbe wie ein Beleg.
///
/// **Wieviel Luft der Quervergleich hat:** x86 hält *drei* Basisrahmen je VSpace (PML4, PDPT, PD)
/// plus die L3-Tabellen, verlangt werden zwei — der Abstand ist mindestens ein Rahmen je VSpace.
/// Der Grund für die Untergrenze statt der genauen Zahl ist ein Rennen: `create_vspace_masked`
/// belegt den ASID-Slot, **bevor** es die Rahmen holt. Eine Gleichheit wäre in genau diesem
/// Fenster falsch, ohne dass etwas kaputt ist.
#[cfg(feature = "selftest")]
fn ptab_urteil() -> (bool, bool, bool, bool) {
    let (raus, zurueck, gehalten, _peak) = system::seitentabellen_topf();
    let noetig = system::PT_RAHMEN_JE_VSPACE * system::used_vspaces() as u64;
    (raus > 0, zurueck > 0, raus >= zurueck, gehalten >= noetig)
}

/// **Das Urteil der `vorrat`-Zeile — an EINER Stelle**, damit `all_done` und der Bericht nicht
/// zwei Wirklichkeiten aus derselben Hand werden (der `pdbind`-Fehler: 21 Glieder in der Kette
/// gegen 24 Einträge in der Liste).
///
/// Zwei Konjunkte, beide falsifizierbar:
/// * `scan == 0` — die Auflösung Thread → PD ist O(1). Fällt, sobald die Rückwärts-Tabelle
///   fehlt (Gegenprobe: `attach_owner` weglassen).
/// * `aufrufe > 0` — die **Sprechprobe**: ohne sie bestünde die Zeile auch ein System, das gar
///   keine Syscalls macht, und eine Null bei den Iterationen wäre bedeutungslos.
#[cfg(feature = "selftest")]
fn vorrat_urteil() -> bool {
    let (pd_calls, pd_scan, _, owner_fehlt) = system::pd_scan_bilanz();
    // **Die Wachen sind Teil des Urteils, mit Sprechprobe und Bilanz.**
    //
    // `gesetzt > 0` ist die Sprechprobe: null gesetzte Wachen hiesse „nie benutzt", und dann
    // sagte die Zeile ueber die Wache nichts aus -- genau der leere Lauf, der in diesem Projekt
    // kein Testergebnis ist. `abgewiesen == 0` ist die Bilanz: eine abgewiesene Wache bedeutet,
    // dass ein Stack NICHT bewacht werden konnte, und die Anforderung wurde deshalb abgelehnt.
    // In einem Lauf dieser Groesse darf das nicht vorkommen; kommt es vor, ist der feste Vorrat
    // zu klein und das gehoert gesagt, nicht ueberlesen.
    let (_, gd_total, gd_denied, _, _) = hal::mmu::guard_stats();
    pd_scan == 0 && pd_calls > 0 && owner_fehlt == 0 && gd_total > 0 && gd_denied == 0
}

/// Ergebnis der EINMALIGEN Seitentabellen-Messung: Bit 63 = gemessen, Bit 0 = Urteil.
///
/// **Einmalig und nicht in `all_done`**, weil `used_vspaces()` die `VSPACES`-Tabelle sperrt und
/// über 4096 Einträge läuft — in einer Schleife, die Milliarden Umdrehungen macht, wäre das ein
/// Prüfer, der die Sache aushungert, die er beobachtet (dieselbe Überlegung wie bei der
/// Notbremse vor `reap()`).
#[cfg(feature = "selftest")]
static PTAB_MESS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "selftest")]
fn ptab_messen() -> bool {
    if PTAB_MESS.load(Ordering::Acquire) & (1 << 63) != 0 {
        return PTAB_MESS.load(Ordering::Acquire) & 1 != 0;
    }
    let (a, b, c, d) = ptab_urteil();
    let ok = a && b && c && d;
    PTAB_MESS.store((1 << 63) | u64::from(ok), Ordering::Release);
    ok
}

/// **C7: ist der Mangel-Melder auf den `spawn_*`-Pfaden überhaupt SPRECHFÄHIG?**
///
/// Bis zum 2026-08-10 meldete `lade_mangel()` in **jedem** Abbruch der Kapazitätskurve
/// `keiner (der Fehlschlag lag NICHT an einer Ressource)` — auch dort, wo das freie RAM
/// nachweislich auf 2 MiB stand. Der Grund war kein Messfehler, sondern eine Lücke: der
/// Benennungsmechanismus hing allein am `SYS_LOAD`-Pfad, die `spawn_*`-Pfade gaben ein nacktes
/// `None`. Eine Kurve, die zählt, ohne die Ressource zu nennen, ist eine Zählung und kein Beleg.
///
/// **Warum eine provozierte Anforderung und nicht eine Beobachtung.** In einem gesunden Lauf
/// scheitert kein `spawn_*` — ein Prüfer, der auf einen Fehlschlag wartet, wäre in jedem grünen
/// Lauf stumm und damit von einem abgeklemmten nicht zu unterscheiden. Deshalb wird der Allokator
/// hier **wirklich gefragt** und sagt **wirklich nein**: eine leere Farbmaske hat keine Seite, die
/// passt. Der Weg dorthin ist der reguläre (`spawn_isolated_colored` → der EL0-Kernel-Stack ist
/// die erste Anforderung des Pfades), nicht ein Testeinsprung daneben.
///
/// **Warum vorher vergiftet wird.** Ohne die Marke wäre „der Pfad hat geschwiegen" von „der Pfad
/// hat `MANGEL_KEINER` geschrieben" nicht zu unterscheiden, und beides sähe wie `0` aus. Mit der
/// Marke ist Schweigen ein eigener, benannter Ausgang.
///
/// Drei Konjunkte:
/// * `abgewiesen` — **Positivkontrolle der Provokation.** Gelingt der Spawn, ist nichts gemessen;
///   dann sagt auch ein grüner Code nichts. (Der Thread wird in diesem Fall abgebaut, sonst
///   verschöbe die Probe die Baseline der folgenden Zeilen.)
/// * `benannt` — der gemeldete Code ist der des angefragten Topfes, nicht `MANGEL_KEINER` und
///   nicht die Marke.
/// * `menge` — die gemeldete Byte-Zahl ist die **angeforderte**. Das ist der Konjunkt gegen die
///   Falle vom Vormittag: `mangel(MANGEL_SEITENTABELLE, 4096)` stand als Literal da, wo der
///   Allokator nie gefragt worden war. Verglichen wird gegen `USER_KSTACK_ALLOC` — dieselbe
///   Konstante, die in den Allokator geht, nicht eine zweite Rechnung daneben.
///
/// **Was diese Zeile NICHT belegt:** dass jede der rund zwanzig neuen Meldestellen die richtige
/// Ressource nennt. Sie belegt, dass der Mechanismus auf einem `spawn_*`-Pfad greift und die
/// Menge aus dem Aufruf trägt. Die übrigen Stellen sind gegengelesen, nicht gemessen.
#[cfg(feature = "selftest")]
static MANGEL_MESS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "selftest")]
fn mangel_messen() -> bool {
    if MANGEL_MESS.load(Ordering::Acquire) & (1 << 63) != 0 {
        return MANGEL_MESS.load(Ordering::Acquire) & 1 != 0;
    }
    let ok = mangel_messen_inner();
    MANGEL_MESS.store((1 << 63) | u64::from(ok), Ordering::Release);
    ok
}

/// Ergebnis der Mangel-Sprechprobe für den Bericht: `(abgewiesen, code, bytes, farben)`.
#[cfg(feature = "selftest")]
static MANGEL_PROBE: caprock_sync::SpinLock<(bool, u32, u64, u32)> =
    caprock_sync::SpinLock::new((false, 0, 0, 0));

#[cfg(feature = "selftest")]
fn mangel_messen_inner() -> bool {
    let farben = crate::colors::count();
    // **Eine benannte Absage, kein Schweigen.** Bei einer Farbe ist die leere Maske wirkungslos
    // (`alloc_colored` nimmt dann den ungefärbten Weg) — die Provokation griffe nicht, und ein
    // grünes Urteil wäre eine Behauptung über einen Pfad, der nie gelaufen ist.
    if farben <= 1 {
        *MANGEL_PROBE.lock() = (false, system::MANGEL_VERGIFTET, 0, farben);
        return false;
    }
    system::mangel_vergiften();
    let r = system::spawn_isolated_colored(
        kurven_arbeiter as *const () as usize,
        0,
        system::IDLE_PRIO,
        caprock_mem::ColorMask::EMPTY,
    );
    let (code, bytes, _frei) = system::lade_mangel();
    let abgewiesen = r.is_none();
    // Wider Erwarten entstanden? Dann sofort abbauen — eine Probe, die Ressourcen liegen lässt,
    // kippt die baseline-empfindlichen Zeilen hinter ihr.
    if let Some((tid, _, _)) = r {
        system::destroy_isolated(tid);
    }
    *MANGEL_PROBE.lock() = (abgewiesen, code, bytes, farben);
    abgewiesen
        && code == system::MANGEL_KERNEL_STACK
        && bytes == system::USER_KSTACK_ALLOC
}

// ================================================================================================
// C7: DER MANGEL-SWEEP -- aus „gegengelesen" wird „kann nicht schweigen"
// ================================================================================================
//
// **Was die Zeile darueber offen liess, und warum das eine Luecke ist.** `mangel` belegt EINE der
// 31 Meldestellen (`system::MELDESTELLEN`). Die uebrigen 30 standen als „gegengelesen, nicht
// gemessen" im Quelltext -- ehrlich aufgeschrieben und trotzdem ein Nullbefund: gegengelesen
// wurde auch die Zeile, die am selben Vormittag `mangel(MANGEL_SEITENTABELLE, 4096)` als Literal
// neben eine Stelle schrieb, an der der Allokator nie gefragt worden war.
//
// **Die Frage je Stelle ist nicht „nennt sie den richtigen Topf", sondern „kann sie SCHWEIGEN".**
// Darauf antwortet die vergiftete Marke (`MANGEL_VERGIFTET = 255`): kein Kernelpfad schreibt
// diesen Code. Steht er nach einem provozierten Fehlschlag noch da, hat der Pfad nichts gesagt.
//
// **Der Befund, der beim Bauen dieser Zeile herausfiel: die Marke wurde bis heute gelöscht,
// bevor sie etwas belegen konnte.** Jeder `spawn_*`-Pfad ruft `mangel_zuruecksetzen()` in seiner
// ERSTEN Anweisung -- also zwischen dem Vergiften und der ersten Anforderung. Der Ausgang „der
// Pfad hat geschwiegen" war damit strukturell unerreichbar, waehrend die Berichtszeile ihn
// woertlich versprach. Seit dem 2026-08-10 ueberlebt die Marke das Zuruecksetzen.
//
// **Wie provoziert wird.** `system::sperre_scharf(k)` laesst `k` Anforderungen durch und weist ab
// der `k+1`-ten jede ab -- im Allokator, nicht in der Meldestelle. Ueber wachsendes `k` wandert
// der Fehlschlag den Pfad entlang und trifft jede Anforderung genau einmal. Der Allokator sagt
// nein, den Weg danach geht der echte Code; die Sperre faelscht keinen Mangel-Code.
//
// **Die Menge kommt aus dem Aufruf, nicht aus einem Literal daneben.** Die Sperre merkt sich die
// Byte-Zahl der Anforderung, die sie abgewiesen hat. Verglichen wird die GEMELDETE Zahl gegen
// diese -- nicht gegen eine Konstante im Pruefer, die mit dem Literal gemeinsam falsch sein
// koennte. Das ist der Konjunkt, den die Falle vom Vormittag gebraucht haette.
//
// **Vier Ausgaenge, und sie sind getrennt:**
// * `punkte`  -- bewertete Abweisungen (die Sperre hat gefeuert, der Aufruf ist gescheitert).
// * `stumm`   -- die Marke steht noch da: der Pfad hat GESCHWIEGEN. Muss 0 sein.
// * `keiner`  -- gemeldet wurde `MANGEL_KEINER`, obwohl der Allokator abgewiesen hat. Muss 0 sein.
// * `fremd`   -- die Sperre feuerte, der Aufruf gelang trotzdem: ein fremder Faden auf demselben
//                Kern hat einen Durchlass verbraucht. Wird gezaehlt statt als Erfolg gebucht.

/// Bit 63 = gemessen, Bit 0 = Urteil.
#[cfg(feature = "selftest")]
static SWEEP_MESS: AtomicU64 = AtomicU64::new(0);

/// `(punkte, stumm, keiner, menge_falsch, fremd, codemaske, pfade)`
#[cfg(feature = "selftest")]
static SWEEP: caprock_sync::SpinLock<(u32, u32, u32, u32, u32, u32, u32)> =
    caprock_sync::SpinLock::new((0, 0, 0, 0, 0, 0, 0));

/// **Was der Sweep hinterlaesst** -- `(dVSpaces, dPD-Slots, dThread-Slots, dPT-Rahmen, dRAM)`,
/// jeweils *nachher minus vorher*.
///
/// **Warum das ein eigener Konjunkt ist und nicht ein Kommentar.** Der Sweep legt PDs, Threads,
/// Adressraeume und Seitentabellen an und weist mittendrin Anforderungen ab -- also genau in den
/// Fehlerpfaden, die am seltensten gelaufen sind. Ein halb abgebauter Ladevorgang faelscht jede
/// Zeile nach ihm, und zwar lautlos: die Suite meldet dann einen Speicherstand, den niemand mehr
/// dem Sweep zuordnet. „Er raeumt auf" ist eine Behauptung, solange es niemand nachzaehlt.
///
/// Vier der fuenf Groessen sind **exakt**: VSpaces, PD-Slots, Thread-Slots und
/// Seitentabellen-Rahmen bewegt in diesem Fenster nur der Sweep (er laeuft, nachdem jedes andere
/// Urteil steht). Beim RAM steht bewusst nur „nicht weniger": ein sterbender Thread gibt
/// waehrenddessen seinen Stack zurueck, das ist ein Zuwachs und kein Befund.
#[cfg(feature = "selftest")]
static SWEEP_BILANZ: caprock_sync::SpinLock<(i64, i64, i64, i64, i64)> =
    caprock_sync::SpinLock::new((0, 0, 0, 0, 0));

/// Abweisungen je Pfad und ob der Pfad **zu Ende** gefahren wurde (Sperre feuerte nicht mehr).
///
/// **Warum das getrennt dasteht:** eine Gesamtzahl belegt nicht, dass jede Anforderung JEDES
/// Pfades getroffen wurde -- 23 Punkte koennten auch alle auf einem Pfad liegen. Erst „Pfad p
/// wurde bis zu seinem Ende gefahren" erlaubt den Schluss, dass jede Meldestelle DIESES Pfades
/// einmal sprechen musste. Wo bei `SWEEP_KMAX` gedeckelt wurde, steht das ausdruecklich dabei --
/// dort gilt der Schluss nur bis zum Deckel.
#[cfg(feature = "selftest")]
static SWEEP_PFAD: caprock_sync::SpinLock<([u32; SWEEP_PFADE], u32, [u32; SWEEP_PFADE])> =
    caprock_sync::SpinLock::new(([0; SWEEP_PFADE], 0, [0; SWEEP_PFADE]));

/// Wie viele Pfade der Sweep faehrt.
#[cfg(feature = "selftest")]
const SWEEP_PFADE: usize = 8;
/// Pfad 6: [`system::map_region_into_thread`] -- **kein Ladepfad**, s. `SWEEP_KLASSEN`.
#[cfg(feature = "selftest")]
const PFAD_FENSTER: usize = 6;
/// Pfad 7: der echte Ladepfad ([`system::load_into_pd_mit`]) -- braucht ein Boot-Archiv.
#[cfg(feature = "selftest")]
const PFAD_LADEN: usize = 7;

/// Hoechste Zahl der Durchlaesse je Pfad.
///
/// Pfad 2 (`spawn_isolated_native`) ist bewusst **kurz** gedeckelt: er kopiert die uebergebenen
/// Bytes in die PD und startet sie. Ein gelungener letzter Durchgang liefe also fremden Code --
/// ein zweites, mit der Messung unverwandtes Risiko in einer Pruefzeile. Bei `k <= 3` scheitert
/// der Pfad **strukturell** (er braucht mehr Anforderungen, bevor der Thread ueberhaupt entsteht).
///
/// **Pfad 7 (Laden) ist aus demselben Grund gedeckelt, und zusaetzlich noch anders begrenzt** --
/// s. die Halteregel in [`sweep_messen_inner`]: ein gelungener Ladevorgang startet ein fremdes
/// Programm ohne jede Cap. Der Deckel allein waere geraten (die Zahl der Anforderungen haengt am
/// Image), deshalb haelt der Sweep dort an einer **beobachteten** Groesse an.
#[cfg(feature = "selftest")]
const SWEEP_KMAX: [u32; SWEEP_PFADE] = [12, 12, 4, 6, 4, 6, 3, 24];

// ------------------------------------------------------------------------------------------------
// C7: DIE KLASSIFIKATION DER MELDESTELLEN -- und warum sie im TYP steht und nicht im Fliesstext
// ------------------------------------------------------------------------------------------------
//
// **Der Nenner ist abgeleitet, die Summanden waren es nicht.** `system::MELDESTELLEN` kommt seit
// dem 2026-08-11 aus `tools/mangel-zaehlen.py` -- gelesen, nicht gefuehrt. Die Aufteilung der
// Abdeckung („11 provoziert + 9 Platz + 10 Ladepfad + 1") stand daneben als **Prosa**, und Prosa
// hat kein Gatter: als die Guard-Page mit `MANGEL_GUARD_TABELLE` eine 32. Stelle mitbrachte, ging
// der Nenner mit (er wird gezaehlt), die Summanden nicht -- 11+9+10+1 = 31 gegen einen Nenner von
// 32, und niemand sah es. Genau die Klasse, gegen die die Ableitung des Nenners gebaut war, eine
// Ebene weiter.
//
// Deshalb stehen die Summanden hier als Konstanten mit einer **Zusicherung zur Bauzeit**. Wer eine
// Meldestelle hinzufuegt, bricht den Bau, bis er sie eingeordnet hat. Eine Klassifikation, die man
// vergessen kann, ist keine.
//
// Die fuenf Klassen, mit Zeilennummern aus `tools/mangel-stellen.sh --liste` (2026-08-11):
//
//  1. **PROVOZIERT, ohne Boot-Archiv** (12): 125 Kernel-Stack · 2379/2436 Kernel-Thread-Stack ·
//     2473 User-Stack · 2590/2625 Seitentabelle in `create_vspace` · 2989 Seitentabelle im
//     Fenster · 3169/3322/3463/3464 Privatregion · **4608 `map_region_into_thread`**.
//     Die letzte ist neu und ein Befund fuer sich: sie stand als „Ladepfad" gebucht und ist
//     keiner -- sie ist der Weg, auf dem eine Treiber-PD ihr Geraetefenster bekommt, und der
//     braucht kein Archiv. Ein Grund, der nie nachgeprueft wird, ueberlebt jede Umgebung.
//  2. **PROVOZIERT, nur MIT Boot-Archiv** (4): 4288 Segmentspeicher · 4304 Seitentabelle eines
//     Segments · 4350 User-Stack · 4366 Seitentabelle des Stacks. Das ist der Ladepfad, auf dem
//     `NoResources` sechs Wochen stumm war; die Lade-Suite hat das Archiv, die Hauptsuite nicht.
//     (Die Heap-OOM-Stelle der va_liste (Sept 2026) ist mit den statischen Scratch-Bereichen
//     entfallen -- kein Heap im Kernel, also keine OOM-Meldestelle. Nenner 43.)
//  3. **PLATZ-TOEPFE** (10): 2399/2453/3221/3386/3547/4438 Thread-Slot · 2572 ASID ·
//     3293/4171 Farbstreifen · 4516 PD-Slot. Die Sperre sitzt im SPEICHER-Allokator; diese Toepfe
//     vergeben keine Bytes. Sie zu leeren heisst tausende Threads/PDs anzulegen -- das ist die
//     Kapazitaetskurve, und die kippt jede baseline-empfindliche Zeile vor ihr.
//  4. **DER ALLOKATOR WURDE NICHT GEFRAGT** (4): 2998/4317/4380 `MANGEL_MAPPING_ABGEWIESEN`
//     und `MANGEL_ENDOWMENT`. Sie melden genau den Fall „es lag NICHT am Speicher" (krumme
//     VA/PA, ausserhalb des Fensters; bzw. eine Zusage des Manifests, die die Domaenenpolitik
//     oder das Cap-Budget nicht traegt) -- eine Sperre im Allokator kann sie strukturell nicht
//     ausloesen. Erreichbar sind sie ueber ein Image mit krummer Segment-VA (genau so ist der
//     `wasm`-Ladefehler vom 2026-08-10 entstanden) bzw. ueber ein Manifest, das mehr Caps
//     zusagt, als `CAP_BUDGET_PER_PD` traegt. Keine tote Gegend.
//  5. **KEIN ALLOKATOR-TOPF, sondern ein fester Vorrat** (1): 139 `MANGEL_GUARD_TABELLE`. Der
//     Vorrat aufgeteilter Seitentabellen liegt in der HAL, die keinen Allokator hat.
//  6. **STRUKTURELL NICHT AUSLOESBAR** (2), und das ist ein Befund ueber die Stellen selbst:
//     * 4207 `MANGEL_L2_TABELLE` -- `vspace_l2(asid)` gibt fuer eine soeben von
//       `create_vspace_masked` gelieferte ASID **immer** `Some`. Der Zweig ist auf diesem Pfad
//       tot; er waere nur ueber einen nebenlaeufigen Teardown derselben ASID erreichbar.
//     * 4269 `MANGEL_SEGMENT_SPEICHER` mit Menge 0 -- die Fail-closed-Schranke gegen mehr als
//       `MAX_IMG_SEGS` (64) Frame-Stuecke. Sie braucht ein IMAGE mit mehr als 64 Stuecken, keinen
//       leeren Topf; kein Programm des Testarchivs kommt in die Naehe.
//  7. **FORK-PFAD** (10, Stand 2026-09-09): die zehn Meldestellen in `dispatch_fork`
//     (`system.rs`: Kostenschaetzung, PD-Slot, L2-Tabelle, Segment-/Seitentabellen-/Stack-
//     Speicher, Mapping-Abweisung, Thread-Slot). Sie brauchen kein Boot-Archiv (FORK kopiert
//     eine laufende PD), aber der Sweep uebt sie NOCH NICHT -- er kennt nur den Lade-Pfad.
//     Die Zahl steht hier, damit der Nenner stimmt; die Provozierung ist eigene Arbeit
//     (FORK-Sonde mit leeren Toepfen, Muster `sweep`). B reviewt die Einordnung.
#[cfg(feature = "selftest")]
const SWEEP_K1_PROVOZIERT_OHNE_ARCHIV: usize = 12;
#[cfg(feature = "selftest")]
const SWEEP_K2_PROVOZIERT_MIT_ARCHIV: usize = 4;
#[cfg(feature = "selftest")]
const SWEEP_K3_PLATZ: usize = 10;
#[cfg(feature = "selftest")]
const SWEEP_K4_NICHT_GEFRAGT: usize = 4;
#[cfg(feature = "selftest")]
const SWEEP_K5_HAL_VORRAT: usize = 1;
#[cfg(feature = "selftest")]
const SWEEP_K6_UNAUSLOESBAR: usize = 2;
#[cfg(feature = "selftest")]
const SWEEP_K7_FORK_PFAD: usize = 10;

/// **Die Summanden MUESSEN aufgehen** -- zur Bauzeit, nicht im Bericht.
#[cfg(feature = "selftest")]
const _: () = assert!(
    SWEEP_K1_PROVOZIERT_OHNE_ARCHIV
        + SWEEP_K2_PROVOZIERT_MIT_ARCHIV
        + SWEEP_K3_PLATZ
        + SWEEP_K4_NICHT_GEFRAGT
        + SWEEP_K5_HAL_VORRAT
        + SWEEP_K6_UNAUSLOESBAR
        + SWEEP_K7_FORK_PFAD
        == system::MELDESTELLEN,
    "C7: die Summanden der Sweep-Abdeckung gehen nicht mehr gegen system::MELDESTELLEN auf. \
     Es ist eine Meldestelle dazugekommen (oder weggefallen), und sie ist nicht eingeordnet. \
     Ordne sie in eine der sieben Klassen bei SWEEP_KMAX ein -- eine Abdeckung mit einem \
     falschen Nenner ist eine Behauptung."
);

#[cfg(feature = "selftest")]
fn sweep_messen() -> bool {
    if SWEEP_MESS.load(Ordering::Acquire) & (1 << 63) != 0 {
        return SWEEP_MESS.load(Ordering::Acquire) & 1 != 0;
    }
    let ok = sweep_messen_inner();
    SWEEP_MESS.store((1 << 63) | u64::from(ok), Ordering::Release);
    ok
}

/// Einen Pfad einmal fahren, mit der Sperre auf `k`.
///
/// Gibt `(gelungen, gefeuert, abgewiesene Menge, gemeldeter Code, gemeldete Bytes)`.
/// Alles, was der Aufbau selbst alloziert (die PD fuer `spawn_in_pd`), entsteht **vor** dem
/// Scharfstellen -- sonst verbrauchte der Aufbau die Durchlaesse, und die Abweisung landete an
/// einer Stelle, die gar nicht gemeint war.
/// **Das Fenster, das Pfad 6 in eine fremde VSpace legt** -- HPET, also ein Registerbereich, der
/// mit Sicherheit **kein RAM** ist.
///
/// Ein RAM-Rahmen waere hier die bequeme Wahl und die falsche: er gehoerte jemandem. Ein
/// Geraetefenster gehoert niemandem, und `map_region_into_thread` ist genau fuer diesen Fall
/// gebaut -- ein Treiber bekommt die Registerseite seines Geraets. `ro: true`, und die PD wird
/// unmittelbar danach abgebaut; gelesen wird die Seite nie (der Thread parkt sich in seiner
/// ersten Instruktion).
#[cfg(feature = "selftest")]
const SWEEP_FENSTER_PA: u64 = 0xFED0_0000;

/// **Das Bild, mit dem Pfad 7 den echten Ladepfad faehrt** -- das KLEINSTE Programm des Archivs.
///
/// Klein, weil die Zahl der Anforderungen an der Zahl der Segmentstuecke und der Seiten haengt:
/// je kleiner das Bild, desto kuerzer der Weg bis zum Stack-Topf, und desto weniger Durchgaenge
/// braucht der Sweep. Die Wahl ist **abgeleitet** (kleinste Summe der `memsz`), nicht ein Index
/// im Quelltext -- ein Index waere eine zweite Wahrheit neben dem Archiv, und beim naechsten
/// Modul waere er still falsch.
///
/// **Ohne Archiv gibt es hier nichts**, und das ist der ehrliche Ausgang: die Hauptsuite hat
/// bauartbedingt kein Boot-Archiv. `None` heisst „dieser Pfad ist heute nicht fahrbar" und wird
/// im Bericht als solcher genannt, nicht als bestandene Pruefung.
#[cfg(feature = "selftest")]
fn sweep_ladebild() -> Option<caprock_loader::elf::ElfImage<'static>> {
    let archiv = crate::loader::read_archive()?;
    let mut beste: Option<(u64, caprock_loader::elf::ElfImage<'static>)> = None;
    for i in 0..archiv.count() {
        let Some(prog) = archiv.program(i) else { continue };
        let Ok(img) = caprock_loader::elf::ElfImage::parse(prog.elf) else { continue };
        let gross: u64 = img.segments().map(|s| ((s.memsz as u64) + 4095) & !4095).sum();
        if beste.as_ref().is_none_or(|(g, _)| gross < *g) {
            beste = Some((gross, img));
        }
    }
    beste.map(|(_, img)| img)
}

#[cfg(feature = "selftest")]
fn sweep_versuch(pfad: usize, k: u32) -> (bool, bool, u64, u32, u64) {
    // **Der ganze Aufbau entsteht VOR dem Scharfstellen** -- sonst verbrauchte er die
    // Durchlaesse, und die Abweisung landete an einer Stelle, die gar nicht gemeint war.
    // „Nicht fahrbar" wird als „nicht gefeuert" gemeldet, damit der Sweep den Pfad
    // ueberspringt, statt ein Urteil zu erfinden.
    let nicht_fahrbar = (false, false, u64::MAX, system::MANGEL_VERGIFTET, 0);
    let pd = if pfad == 5 || pfad == PFAD_LADEN { system::create_pd() } else { None };
    if (pfad == 5 || pfad == PFAD_LADEN) && pd.is_none() {
        return nicht_fahrbar;
    }
    let el0 = kurven_arbeiter_el0 as *const () as usize;
    let kern = kurven_arbeiter as *const () as usize;
    // Pfad 6 braucht eine lebende isolierte VSpace, in die das Fenster gelegt wird.
    let wirt = if pfad == PFAD_FENSTER {
        match system::spawn_isolated(el0, 0, system::IDLE_PRIO) {
            Some((t, _)) => Some(t),
            None => return nicht_fahrbar,
        }
    } else {
        None
    };
    let bild = if pfad == PFAD_LADEN { sweep_ladebild() } else { None };
    if pfad == PFAD_LADEN && bild.is_none() {
        if let Some(p) = pd {
            let _ = system::free_pd_slot(p);
        }
        return nicht_fahrbar;
    }
    system::mangel_vergiften();
    system::sperre_scharf(k);
    let mut iso = None;
    let mut thr = None;
    let mut geladen = None;
    let mut fenster = false;
    match pfad {
        0 => iso = system::spawn_isolated(el0, 0, system::IDLE_PRIO).map(|(t, _)| t),
        1 => iso = system::spawn_isolated_colored_auto(el0, 0, system::IDLE_PRIO).map(|(t, _)| t),
        2 => {
            iso = system::spawn_isolated_native(
                kurven_arbeiter_el0 as *const () as *const u8,
                64,
                system::IDLE_PRIO,
            )
        }
        3 => thr = system::spawn_user(el0, 0, system::IDLE_PRIO),
        4 => thr = system::spawn_on_core(hal::cpu::core_id(), kern, 0, system::IDLE_PRIO),
        5 => thr = system::spawn_in_pd(pd.unwrap_or(0), kern, 0, system::IDLE_PRIO),
        PFAD_FENSTER => {
            fenster = system::map_region_into_thread(
                wirt.expect("Pfad 6 hat seinen Wirt oben angelegt"),
                SWEEP_FENSTER_PA,
                caprock_mem::PAGE,
                system::MappingKind::Device { ro: true },
            )
        }
        // **Der ECHTE Ladepfad**, nicht ein Nachbau: dieselbe Funktion, die `SYS_LOAD` und
        // `load_by_index` rufen. Ohne Endowment und mit der Vorgabepolitik -- die Politik ist
        // hier nicht die gepruefte Sache, und ein abweichender Wert schriebe nebenbei die
        // `ladepol`-Ablage voll.
        _ => {
            geladen = system::load_into_pd_mit(
                bild.as_ref().expect("Pfad 7 hat sein Bild oben geholt"),
                pd.unwrap_or(0),
                &[],
                0,
                system::LadePolitik::VORGABE,
            )
        }
    }
    // **Erst entschaerfen, dann abbauen.** Ein Teardown unter scharfer Sperre koennte selbst
    // abgewiesen werden und Spuren hinterlassen, die niemand mehr der Sperre zuordnet.
    let (gefeuert, menge) = system::sperre_aus();
    let (code, bytes, _frei) = system::lade_mangel();
    let gelungen = iso.is_some() || thr.is_some() || geladen.is_some() || fenster;
    if let Some(t) = iso {
        system::destroy_isolated(t);
    }
    if let Some(t) = thr {
        let _ = system::kill_local(t);
    }
    if let Some(t) = wirt {
        // Baut die VSpace ab und gibt die dabei privat gewordenen Geraetetabellen zurueck --
        // auch wenn das Fenster tatsaechlich entstanden ist.
        system::destroy_isolated(t);
    }
    if let Some(t) = geladen {
        // **Dieser Zweig soll nach der Halteregel gar nicht vorkommen** (s. `sweep_messen_inner`).
        // Er steht hier trotzdem, weil „soll nicht" keine Zusicherung ist: ein halb geladenes
        // Programm, das liegen bleibt, faelschte jede Zeile danach. `destroy_loaded` gibt den
        // PD-Slot mit frei -- deshalb hier heraus, statt unten noch einmal freizugeben.
        system::destroy_loaded(t, pd.unwrap_or(0));
        return (true, gefeuert, menge, code, bytes);
    }
    if let Some(p) = pd {
        let _ = system::free_pd_slot(p);
    }
    (gelungen, gefeuert, menge, code, bytes)
}

#[cfg(feature = "selftest")]
fn sweep_messen_inner() -> bool {
    let (mut punkte, mut stumm, mut keiner, mut menge_falsch, mut fremd) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut codes = 0u32;
    let mut pfade = 0u32;
    let mut je_pfad = [0u32; SWEEP_PFADE];
    let mut je_codes = [0u32; SWEEP_PFADE];
    let mut zuende = 0u32;
    // **Vorher zaehlen, damit „nichts bleibt liegen" eine Messung ist und keine Zusage.**
    // Vor dem Fegen der Zombies: `destroy_isolated` reapt selbst, der Vergleich soll denselben
    // Zustand gegen denselben halten.
    system::reap();
    let bilanz_vor = sweep_bestand();
    for pfad in 0..SWEEP_KMAX.len() {
        let mut gefahren = false;
        // **Die Halteregel des Ladepfades** (Pfad 7), und sie haengt an einer BEOBACHTETEN
        // Groesse statt an einer geratenen Zahl.
        //
        // Der Ladepfad endet mit dem User-Stack: erst sein Rahmen (`MANGEL_STACK_SPEICHER`),
        // dann dessen Seitentabelle. Danach alloziert er nichts mehr -- der naechste Durchgang
        // gelaenge also, und ein gelungener Ladevorgang STARTET ein fremdes Programm, ohne jede
        // Cap, mitten in einer Pruefzeile. Ein fester Deckel waere die naheliegende Abhilfe und
        // eine geratene Zahl: wieviele Anforderungen ein Bild braucht, haengt an seinen
        // Segmenten. Also: sobald der Stack-Topf gesprochen hat, noch **genau einen** Durchgang
        // (das ist die Seitentabelle des Stacks), dann Schluss.
        let mut stack_gesehen = false;
        for k in 0..SWEEP_KMAX[pfad] {
            // **Der Messende liest die Generation, nicht der gemessene Pfad.**
            let gen_vor = system::mangel_generation();
            let (gelungen, gefeuert, menge, code, bytes) = sweep_versuch(pfad, k);
            let gen_nach = system::mangel_generation();
            if !gefeuert {
                // Der Pfad braucht hoechstens `k` Anforderungen -- er ist zu Ende gefahren.
                zuende |= 1 << pfad;
                break;
            }
            gefahren = true;
            je_pfad[pfad] += 1;
            if gelungen {
                // Ein fremder Faden auf demselben Kern hat einen Durchlass verbraucht.
                fremd += 1;
                continue;
            }
            punkte += 1;
            // **Geschwiegen wird an der GENERATION gemessen, nicht an der Marke.**
            //
            // Die vergiftete Marke war bis zum 2026-08-11 der Melder dafuer -- und sie hing daran,
            // dass der gemessene Pfad sie nicht loescht. Genau das tat er (erste Anweisung), und
            // der Ausgang war strukturell unerreichbar. Behoben wurde damals die Loeschung; die
            // Regel dahinter ist groesser: **wer misst, setzt die Marke -- der gemessene Pfad darf
            // sie weder setzen noch loeschen.**
            //
            // `MANGEL_GEN` waechst nur und wird von niemandem zurueckgesetzt. Der Messende liest
            // vorher und nachher; `nachher == vorher` heisst geschwiegen, und keine Aenderung am
            // Pfad kann das entwerten. Die Marke bleibt als zweiter, unabhaengiger Melder stehen
            // -- zwei Wege zur selben Aussage sind hier billig und decken einander ab.
            if gen_nach == gen_vor || code == system::MANGEL_VERGIFTET {
                stumm += 1;
            } else if code == system::MANGEL_KEINER {
                keiner += 1;
            } else {
                if code < 32 {
                    codes |= 1 << code;
                    je_codes[pfad] |= 1 << code;
                }
                // `MANGEL_MAPPING_ABGEWIESEN` traegt keine Menge (der Allokator wurde nicht
                // gefragt) -- dort waere ein Mengenvergleich eine Frage an die falsche Groesse.
                if code != system::MANGEL_MAPPING_ABGEWIESEN && bytes != menge {
                    menge_falsch += 1;
                }
            }
            // Halteregel, s. oben. Erst zaehlen, dann anhalten -- der Durchgang, der den
            // Stack-Topf gemeldet hat, ist ein vollwertiger Punkt.
            if pfad == PFAD_LADEN {
                if stack_gesehen {
                    break;
                }
                if code == system::MANGEL_STACK_SPEICHER {
                    stack_gesehen = true;
                }
            }
        }
        if gefahren {
            pfade += 1;
        }
    }
    system::mangel_entgiften();
    system::reap();
    let bilanz_nach = sweep_bestand();
    let bilanz = (
        bilanz_nach.0 - bilanz_vor.0,
        bilanz_nach.1 - bilanz_vor.1,
        bilanz_nach.2 - bilanz_vor.2,
        bilanz_nach.3 - bilanz_vor.3,
        bilanz_nach.4 - bilanz_vor.4,
    );
    *SWEEP_BILANZ.lock() = bilanz;
    *SWEEP.lock() = (punkte, stumm, keiner, menge_falsch, fremd, codes, pfade);
    *SWEEP_PFAD.lock() = (je_pfad, zuende, je_codes);
    // **Die Sprechprobe steht vorn:** ohne wirklich gefahrene Abweisungen sagt ein gruenes Urteil
    // nichts -- genau die Form, gegen die dieses Projekt `config_errors()` gebaut hat. Erwartet
    // werden mindestens die drei Toepfe, die die `spawn_*`-Pfade selbst besitzen.
    let toepfe = (1 << system::MANGEL_KERNEL_STACK)
        | (1 << system::MANGEL_PRIVATREGION)
        | (1 << system::MANGEL_SEITENTABELLE);
    // **Der Ladepfad wird JE PFAD belegt, nicht ueber die Gesamtmaske.** `MANGEL_STACK_SPEICHER`
    // setzt auch `spawn_user` (Pfad 3) -- eine Gesamtmaske koennte den Ladepfad also gar nicht
    // von ihm unterscheiden, und die Abdeckungsangabe waere eine Behauptung. Gefragt ist, welche
    // Toepfe **auf diesem Pfad** gesprochen haben.
    let lade_toepfe = (1 << system::MANGEL_SEGMENT_SPEICHER)
        | (1 << system::MANGEL_STACK_SPEICHER)
        | (1 << system::MANGEL_SEITENTABELLE);
    // **Und ob er ueberhaupt fahrbar WAR, wird gemessen und nicht angenommen.** Die Hauptsuite
    // hat bauartbedingt kein Boot-Archiv; dort ist „nicht gefahren" die richtige Antwort und
    // kein Fehlschlag. In der Lade-Suite dagegen ist ein ungefahrener Ladepfad genau die stille
    // Luecke, gegen die diese Zeile gebaut ist.
    let mit_archiv = crate::loader::read_archive().is_some();
    punkte >= 12
        && pfade >= 6
        && codes & toepfe == toepfe
        // Pfad 6 laeuft ohne Archiv -- er ist gar kein Ladepfad, s. `SWEEP_KLASSEN`.
        && je_codes[PFAD_FENSTER] & (1 << system::MANGEL_SEITENTABELLE) != 0
        && (!mit_archiv || je_codes[PFAD_LADEN] & lade_toepfe == lade_toepfe)
        && stumm == 0
        && keiner == 0
        && menge_falsch == 0
        // **Nichts bleibt liegen** -- vier exakte Groessen, eine gerichtete.
        && bilanz.0 == 0
        && bilanz.1 == 0
        && bilanz.2 == 0
        && bilanz.3 == 0
        && bilanz.4 >= 0
}

/// Der Bestand, gegen den die Sweep-Bilanz gehalten wird:
/// `(belegte VSpaces, belegte PD-Slots, lebende Thread-Slots, gehaltene PT-Rahmen, freies RAM)`.
#[cfg(feature = "selftest")]
fn sweep_bestand() -> (i64, i64, i64, i64, i64) {
    let (pds_used, _, _, _, _, _) = system::vorrat_fuellstand();
    let (_, _, pt_gehalten, _) = system::seitentabellen_topf();
    let thr_live = system::thread_capacity().saturating_sub(system::threads_available());
    (
        system::used_vspaces() as i64,
        pds_used as i64,
        thr_live as i64,
        pt_gehalten as i64,
        system::total_free() as i64,
    )
}

/// **Z22 P2 + Z23 S1: zwei Threads in EINER PD — gemessen an der Wirkung.**
///
/// Zehn Aussagen in drei Gruppen; welche Gruppe was belegt, steht am Modulkopf bei den Statics.
/// Das Urteil entsteht hier (einmalig) und nicht im Bericht.
#[cfg(feature = "selftest")]
fn pd_threads_messen() -> bool {
    if PT_MESS.load(Ordering::Acquire) & (1 << 63) != 0 {
        return PT_MESS.load(Ordering::Acquire) & 1 != 0;
    }
    let ok = pd_threads_messen_inner();
    PT_MESS.store((1 << 63) | u64::from(ok), Ordering::Release);
    ok
}

#[cfg(feature = "selftest")]
fn pd_threads_messen_inner() -> bool {
    let warten = || {
        let t0 = hal::timer::ticks(0);
        let mut wache = 0u64;
        while hal::timer::ticks(0) == t0 && wache < 5_000_000 {
            core::hint::spin_loop();
            wache += 1;
        }
    };
    let (sraw, craw, pd) = (
        PT_S_TID.load(Ordering::Acquire),
        PT_C_TID.load(Ordering::Acquire),
        PT_PD.load(Ordering::Acquire),
    );
    if sraw == 0 || craw == 0 || pd == u64::MAX {
        println!(
            "pdthrd  : FAILURES (Aufbau unvollstaendig: server={sraw:#x} client={craw:#x} \
             pd={pd} -- ohne beide Threads ist NICHTS gemessen, und ein SKIP hier waere ein \
             Schluss von Schweigen auf Abwesenheit)"
        );
        return false;
    }
    let (stid, ctid, pd) = (
        caprock_sched::ThreadId::from_raw(sraw),
        caprock_sched::ThreadId::from_raw(craw),
        pd as usize,
    );

    // --- Gruppe 1: BEIDE Threads gehoeren derselben PD ---------------------------------------
    //
    // Vom Kernel aus nachgefragt, ueber genau den Weg, den auch der Syscall-Dispatch nimmt.
    // `anzahl` ist die Groesse, an der „mehrere" von „einer, wie immer" zu unterscheiden ist.
    let pd_s = system::pd_of_thread(stid);
    let pd_c = system::pd_of_thread(ctid);
    let anzahl = system::pd_thread_count(pd);
    let beide_gebunden = pd_s == Some(pd) && pd_c == Some(pd) && anzahl == 2;

    // --- Gruppe 2: Z23 S1 -- die offene Transaktion -------------------------------------------
    //
    // Erst warten, bis der Server sein `RECV` beantwortet bekommen hat: DANN haelt er den
    // Reply-Token, und der Client haengt in seinem CALL. Genau dieser Zustand ist der
    // Messgegenstand.
    let mut rendezvous = false;
    for _ in 0..256 {
        if PT_RECV1_CODE.load(Ordering::Acquire) != u64::MAX {
            rendezvous = true;
            break;
        }
        warten();
    }
    let recv1 = PT_RECV1_CODE.load(Ordering::Acquire);
    // Sprechprobe fuer „offene Transaktion": der Client darf noch NICHT fertig sein.
    let client_haengt = !PT_CLIENT_FERTIG.load(Ordering::Acquire);
    // Jetzt die Tore schliessen -- und es MUSS etwas aendern (ein Tor, das schon zu war,
    // belegt nichts).
    let tor_aenderte = system::pd_quiesce(pd, true);
    PT_TOR_ZU.store(1, Ordering::Release);
    let mut quittiert = false;
    for _ in 0..256 {
        if PT_TOR_ZU.load(Ordering::Acquire) == 2 {
            quittiert = true;
            break;
        }
        warten();
    }
    let mut alle_drei = false;
    for _ in 0..512 {
        if PT_REPLY_CODE.load(Ordering::Acquire) != u64::MAX {
            alle_drei = true;
            break;
        }
        warten();
    }
    let (recv2, callg, replyc) = (
        PT_RECV2_CODE.load(Ordering::Acquire),
        PT_CALL_GATE_CODE.load(Ordering::Acquire),
        PT_REPLY_CODE.load(Ordering::Acquire),
    );
    let mut client_fertig = false;
    for _ in 0..512 {
        if PT_CLIENT_FERTIG.load(Ordering::Acquire) {
            client_fertig = true;
            break;
        }
        warten();
    }
    let antwort = PT_CLIENT_ANTWORT.load(Ordering::Acquire);
    // Tore wieder auf -- sonst stuende die PD fuer den Rest des Laufs still.
    let _ = system::pd_quiesce(pd, false);

    // --- Gruppe 3: getrennte Grund-Mengen (Z24) ------------------------------------------------
    //
    // Der Server parkt sich; sein Zaehler muss stehen, waehrend der des Clients weiterlaeuft.
    // Beide Haelften zusammen sind die Aussage: ohne die zweite waere „steht" von „ist tot"
    // nicht zu unterscheiden.
    PT_PARK_BITTE.store(1, Ordering::Release);
    let mut geparkt = false;
    for _ in 0..256 {
        if PT_PARK_BITTE.load(Ordering::Acquire) == 2 && system::is_parked(stid) {
            geparkt = true;
            break;
        }
        warten();
    }
    let (s0, c0) = (
        PT_S_RUNDEN.load(Ordering::Acquire),
        PT_C_RUNDEN.load(Ordering::Acquire),
    );
    for _ in 0..8 {
        warten();
    }
    let (s1, c1) = (
        PT_S_RUNDEN.load(Ordering::Acquire),
        PT_C_RUNDEN.load(Ordering::Acquire),
    );
    let server_steht = s1 == s0;
    let client_laeuft = c1 > c0;
    // Sprechprobe: nach dem Wecken muss er WIRKLICH weiterlaufen.
    let _ = system::unpark_thread(stid);
    let mut server_laeuft_wieder = false;
    for _ in 0..256 {
        if PT_S_RUNDEN.load(Ordering::Acquire) > s1 {
            server_laeuft_wieder = true;
            break;
        }
        warten();
    }

    let q = caprock_abi::result::ERR_QUIESCING;
    let ok = beide_gebunden
        && rendezvous
        && recv1 == result::OK
        && client_haengt
        && tor_aenderte
        && quittiert
        && alle_drei
        && recv2 == q
        && callg == q
        && replyc == result::OK
        && client_fertig
        && antwort == 42
        && geparkt
        && server_steht
        && client_laeuft
        && server_laeuft_wieder;
    println!(
        "pdthrd  : EINE PD, ZWEI Threads: pd(server)={pd_s:?} pd(client)={pd_c:?} (PD {pd}) \
         gebundene-Threads={anzahl} (erwartet 2) · erstes-RECV-Code={recv1} (erwartet \
         {} = OK -- beide Threads sehen DENSELBEN Cap-Slot; nur einer gebunden hiesse {} = ERR_NOPD)",
        result::OK,
        result::ERR_NOPD
    );
    println!(
        "pdthrd  : Z23 S1 an der OFFENEN Transaktion: rendezvous={rendezvous} \
         client-haengt-noch={client_haengt} tor-aenderte-etwas={tor_aenderte} \
         server-quittiert={quittiert} · zweites-RECV={recv2} CALL={callg} (beide erwartet {q} = \
         ERR_QUIESCING) · REPLY={replyc} (erwartet {} = OK -- DAS ist die bis heute ungemessene \
         Zusicherung) · Client bekam {antwort} (erwartet 42), fertig={client_fertig}",
        result::OK
    );
    println!(
        "pdthrd  : Z24 getrennte Grund-Mengen: geparkt={geparkt} server-Runden {s0}->{s1} \
         (muss STEHEN) waehrend client-Runden {c0}->{c1} (muss LAUFEN -- ohne diese Haelfte \
         waere 'steht' von 'ist tot' nicht zu unterscheiden) · nach UNPARK laeuft der Server \
         wieder={server_laeuft_wieder} : {}",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// **Z4a: der Haltepunkt — und der Beleg, dass er einer ist.**
///
/// Die naheliegende Pruefung waere „`freeze_thread` liefert `Frozen`". Die ist wertlos: sie sagt
/// nur, dass eine Funktion einen Wert zurueckgibt. Geprueft wird deshalb die **Wirkung**, und zwar
/// in beide Richtungen:
///
/// 1. Ein Worker zaehlt sichtbar hoch (`WORKER_ROUNDS`). **Vor** dem Einfrieren muss er sich
///    bewegen — sonst belegt „er steht" nichts, sondern beschreibt einen Thread, der nie lief.
///    Das ist dieselbe Positivkontrolle wie beim Kreuz-DMA-Nachweis (A-5.4).
/// 2. **Nach** dem Einfrieren muss der Zaehler ueber ein Beobachtungsfenster **unveraendert**
///    bleiben.
/// 3. Nach dem Auftauen muss er sich **wieder** bewegen. Ohne diesen dritten Schritt waere ein
///    `freeze`, das den Thread einfach kaputtmacht, von einem korrekten nicht zu unterscheiden.
///
/// Und die vierte Aussage, die keine Bewegung braucht: ein Thread mit **offener IPC-Beziehung**
/// wird abgewiesen (`Busy`), nicht eingefroren. Dafuer wird der IPC-Server genommen, der in `RECV`
/// steht — er ruht scheinbar, und genau das ist die Falle: „blockiert" und „ruhend" sehen von
/// aussen gleich aus und sind es nicht.
#[cfg(feature = "selftest")]
fn freeze_bericht() {
    use system::Freeze;
    // **Das Fenster muss laenger sein als ein Tick.** Die erste Fassung wartete 400 000
    // Leerdurchlaeufe -- unter KVM knapp eine Millisekunde, also kuerzer als die 10 ms bis zur
    // naechsten Verdraengung. Die Positivkontrolle meldete daraufhin `laeuft-vorher=false` bei
    // einem Worker, der voellig in Ordnung war: gemessen wurde nicht sein Stillstand, sondern die
    // Laenge des Fensters. Gezaehlt wird deshalb in **Ticks**, nicht in Schleifendurchlaeufen.
    let warten = || {
        let t0 = hal::timer::ticks(0);
        let mut wache = 0u64;
        while hal::timer::ticks(0) < t0 + 3 && wache < 200_000_000 {
            core::hint::spin_loop();
            wache += 1;
        }
    };
    let raw = WORKER_TID0.load(Ordering::Acquire);
    let Some(tid) = (raw != 0).then(|| caprock_sched::ThreadId::from_raw(raw)) else {
        println!("freeze  : SKIP (kein Worker-Thread -- ohne einen laufenden Thread gibt es nichts anzuhalten)");
        return;
    };

    // 1. Positivkontrolle: bewegt er sich ueberhaupt?
    let a0 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    warten();
    let a1 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    let laeuft = a1 > a0;

    // 2. Einfrieren. `StillRunning` ist vorübergehend -> wiederholen; `Busy` waere eine Absage.
    let mut erg = Freeze::StillRunning;
    for _ in 0..64 {
        erg = system::freeze_thread(tid);
        if erg != Freeze::StillRunning {
            break;
        }
        warten();
    }
    let eingefroren = erg == Freeze::Frozen;

    // 3. Steht er?
    let b0 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    warten();
    warten();
    let b1 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    let steht = b0 == b1;

    // 4. Und laeuft er nach dem Auftauen wieder?
    let aufgetaut = system::thaw_thread(tid);
    let c0 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    warten();
    let c1 = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    let laeuft_wieder = c1 > c0;

    // 5. Ein Thread in einer IPC-Rolle wird ABGEWIESEN, nicht eingefroren.
    let sraw = IPC_SERVER_TID.load(Ordering::Acquire);
    let ipc_abgewiesen = if sraw != 0 {
        matches!(
            system::freeze_thread(caprock_sched::ThreadId::from_raw(sraw)),
            Freeze::Busy(_)
        )
    } else {
        true // nicht anwendbar
    };

    let ok = laeuft && eingefroren && steht && aufgetaut && laeuft_wieder && ipc_abgewiesen;
    println!(
        "freeze  : laeuft-vorher={laeuft} ({a0}->{a1}) eingefroren={eingefroren} \
         steht={steht} ({b0}->{b1}) aufgetaut={aufgetaut} laeuft-wieder={laeuft_wieder} \
         ({c0}->{c1}) IPC-Rolle-abgewiesen={ipc_abgewiesen}"
    );
    println!(
        "freeze  : {} (Z4a: ein Thread haelt an einer BENENNBAREN Grenze -- deplant, auf keinem \
         Kern, in keiner IPC-Rolle. Geprueft wird die WIRKUNG, nicht der Rueckgabewert: der \
         Zaehler muss sich vorher bewegen, danach stehen und nach dem Auftauen wieder laufen. \
         Ohne den ersten Schritt belegt 'er steht' nichts; ohne den dritten waere ein freeze, das \
         den Thread kaputtmacht, davon nicht zu unterscheiden. Ein Thread mit offener \
         IPC-Beziehung wird ABGEWIESEN -- 'blockiert' und 'ruhend' sehen von aussen gleich aus)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    // **Bewusst kein Konjunkt in `all_done()`.** Diese Pruefung laeuft IM Bericht -- was der
    // Bericht setzt, kann den Bericht nicht ausloesen (die Falle aus A-6.1, zweimal an einem Tag).
    // Gelesen wird die Zeile von der Suite; dass sie gelesen WIRD, ist die Lehre aus D5.
    FREEZE_OK.store(ok, Ordering::Release);
}

/// **Z4 Stufe 2: der Bericht ueber die Bootgrenze.**
///
/// Die Aussage, um die es geht, ist **eine**: der Zaehler in Lauf 2 startet bei dem Wert aus Lauf
/// 1, nicht bei 0. Alles andere in dieser Zeile ist das, was noetig ist, damit diese Aussage
/// ueberhaupt etwas belegt:
///
/// * **Die Nonce.** Ein Puffer mit den richtigen Bytes ist von einem beschriebenen Puffer nicht zu
///   unterscheiden. Der Fortschritt allein koennte in zwei Laeufen zufaellig gleich sein; die
///   Nonce ist der Zyklenzaehler beim Einfrieren und damit ein Wert, den Lauf 1 **erst erzeugt**
///   hat.
/// * **Der Kernel-Hash.** Er steht IM Checkpoint und wird beim Lesen geprueft. Ein Checkpoint
///   eines anderen Kernel-Images wird abgewiesen, nicht geladen (Z4f in klein).
/// * **Die Verweigerung.** Die Treiber-PD wird in jedem Lauf klassifiziert und faellt durch --
///   mit ihrem Kanal IM Umfang, damit als Grund nur die Geraete-Autoritaet uebrigbleibt. Ohne
///   diesen Fall belegte „es wurde gespeichert" nur, dass gespeichert wird.
/// * **„Er laeuft weiter."** Ein Wiederherstellen, das den Thread kaputtmacht, waere von einem
///   korrekten sonst nicht zu unterscheiden -- dieselbe dritte Stufe wie in `freeze_bericht`.
///
/// **Bewusst kein Konjunkt in `all_done()`** -- was der Bericht setzt, kann den Bericht nicht
/// ausloesen (A-6.1, zweimal an einem Tag). In der Abschlussbedingung steht nur der
/// **Fertig-Merker** `CKPT_DONE`, damit der Bericht nicht vor der Platten-E/A kommt; gelesen wird
/// die Zeile von der Suite.
#[cfg(feature = "selftest")]
fn ckpt_bericht() {
    let zustand = CKPT_STATE.load(Ordering::Acquire);
    let (rslot, rgrund) = (
        CKPT_REFUSED[0].load(Ordering::Acquire),
        CKPT_REFUSED[1].load(Ordering::Acquire),
    );
    if !CKPT_DONE.load(Ordering::Acquire) || zustand == CKPT_SKIP {
        println!(
            "bootckpt: SKIP (kein Blockdienst -- ohne Platte gibt es keine Bootgrenze, ueber die \
             etwas wandern koennte. Die Hauptsuite bootet ohne Archiv und damit ohne Treiber-PD)"
        );
        return;
    }
    let p = CKPT_PROGRESS.load(Ordering::Acquire);
    let nonce = CKPT_NONCE.load(Ordering::Acquire);
    let epoche = CKPT_EPOCH.load(Ordering::Acquire);
    let sp = CKPT_S_PROGRESS.load(Ordering::Acquire);
    let snonce = CKPT_S_NONCE.load(Ordering::Acquire);
    let sepoche = CKPT_S_EPOCH.load(Ordering::Acquire);
    let jetzt = WORKER_ROUNDS[0].load(Ordering::Relaxed);
    let vorher = CKPT_BEFORE.load(Ordering::Acquire);
    let nach = CKPT_AFTER.load(Ordering::Acquire);
    let caps = CKPT_CAPS.load(Ordering::Acquire);
    let pre = CKPT_PRECOND.load(Ordering::Acquire);
    // Die Verweigerung ist eine eigene Zeile: sie gilt in JEDEM Lauf und haengt nicht daran, ob
    // gespeichert oder wiederhergestellt wurde.
    println!(
        "bootckpt: Verweigerung: die Treiber-PD ist NICHT speicherbar -- Slot {rslot}, Grund \
         {rgrund} (1=Geraetefenster 2=Interrupt 3=DMA-Region 4=offene Antwort 5=Partner nicht im \
         Umfang 6=fremder Thread 7=fremde PD 8=Loader-Quelle; 20/21=Schnittkante, s. \
         ckpt_cut_reason; 0=nichts sprach dagegen, also \
         NICHTS gemessen). Ihr Kanal liegt IM Umfang -- was uebrigbleibt, ist Geraete-Autoritaet, \
         und die laesst sich durch keinen groesseren Umfang beheben"
    );
    // **Geraete-Autoritaet**, nicht irgendein Grund: 5 (Partner nicht im Umfang) waere durch einen
    // groesseren Umfang behebbar und belegte die Regel deshalb nicht.
    let verweigert_richtig = rgrund == 1 || rgrund == 2 || rgrund == 3;
    // **Jeder Lauf, der durchkommt, SCHREIBT** — auch der, der eben wiederhergestellt hat. Erst
    // dadurch waechst die Kette; ein Lauf, der nur liest, koennte nicht belegen, dass sein
    // Nachfolger etwas GEERBTES findet und nicht seinen eigenen Wert noch einmal.
    let io = |i: usize| CKPT_IO[i].load(Ordering::Acquire);
    let speichern_ok = || {
        CKPT_FROZEN.load(Ordering::Acquire)
            && sp > 0
            && snonce != 0
            && sepoche >= 1
            && CKPT_BYTES.load(Ordering::Acquire) > 0
            && io(1) == 0
            && io(2) == 0
            && caps > 0
            && CKPT_DELTA_OK.load(Ordering::Acquire)
            && verweigert_richtig
            // Der Thread laeuft nach dem Speichern weiter -- speichern ist kein Beenden.
            && jetzt > sp
    };
    let ok = match zustand {
        CKPT_SAVED => {
            println!(
                "bootckpt: gespeichert Sektor={CKPT_SECTOR} Fortschritt={sp} Nonce={snonce:#018x} \
                 Epoche={sepoche} Caps={caps} Vorbedingung={pre} Bytes={} Lesen={} Schreiben={} \
                 Flush={} eingefroren={} Zuwachs-erreicht={} Zaehler-jetzt={jetzt} \
                 Schnittkanten={}",
                CKPT_BYTES.load(Ordering::Acquire),
                io(0), io(1), io(2),
                CKPT_FROZEN.load(Ordering::Acquire) as u8,
                CKPT_DELTA_OK.load(Ordering::Acquire) as u8,
                CKPT_CUT_EDGES.load(Ordering::Acquire)
            );
            io(0) == 0 && sepoche == 1 && speichern_ok()
        }
        CKPT_RESTORED => {
            println!(
                "bootckpt: wiederhergestellt Sektor={CKPT_SECTOR} Fortschritt={p} \
                 Nonce={nonce:#018x} Epoche={epoche} Caps={caps} Vorbedingung={pre} \
                 (Kernel-Hash passt) eingefroren={} Zaehler-vorher={vorher} Zaehler-danach={nach}",
                CKPT_FROZEN.load(Ordering::Acquire) as u8
            );
            println!(
                "bootckpt: gespeichert Sektor={CKPT_SECTOR} Fortschritt={sp} Nonce={snonce:#018x} \
                 Epoche={sepoche} Caps={caps} Vorbedingung={pre} Bytes={} Lesen={} Schreiben={} \
                 Flush={} eingefroren={} Zuwachs-erreicht={} Zaehler-jetzt={jetzt} \
                 Schnittkanten={}",
                CKPT_BYTES.load(Ordering::Acquire),
                io(0), io(1), io(2),
                CKPT_FROZEN.load(Ordering::Acquire) as u8,
                CKPT_DELTA_OK.load(Ordering::Acquire) as u8,
                CKPT_CUT_EDGES.load(Ordering::Acquire)
            );
            io(0) == 0
                && p > 0
                && nonce != 0
                && epoche >= 1
                // **Der wiederhergestellte Wert steht IM THREAD**, nicht bloss im Bericht.
                && nach == p
                // Die Kette waechst: dieser Lauf schreibt ein Glied weiter und einen groesseren
                // Fortschritt. Ohne beides waere „wiederhergestellt" nicht von „neu angelegt" zu
                // unterscheiden.
                && sepoche == epoche + 1
                && sp > p
                && speichern_ok()
        }
        CKPT_REJECTED => {
            println!(
                "bootckpt: ABGEWIESEN Sektor={CKPT_SECTOR} Lesecode={} (1=kein Checkpoint \
                 2=Formatversion 3=Laenge 4=zu viele Caps 5=unbekannter Cap-Typ 6=Pruefsumme \
                 7=FREMDES KERNEL-IMAGE) -- der Sektor bleibt unveraendert, es wurde weder \
                 geladen noch ueberschrieben",
                CKPT_DECODE.load(Ordering::Acquire)
            );
            // Eine Abweisung ist ein **erfolgreicher** Ausgang, wenn sie den richtigen Grund hat.
            // Das Urteil hier ist trotzdem `false`: welcher Grund richtig ist, entscheidet der
            // Aufbau, und den kennt die Suite -- sie liest die Codezeile. Ein Kernel, der seine
            // eigene Abweisung als PASS meldete, koennte einen falsch abgewiesenen Lauf nicht von
            // einem richtig abgewiesenen unterscheiden.
            false
        }
        _ => {
            println!(
                "bootckpt: FEHLER (Lesecode={}, eingefroren={}) -- die Folge kam nicht durch",
                CKPT_DECODE.load(Ordering::Acquire),
                CKPT_FROZEN.load(Ordering::Acquire) as u8
            );
            false
        }
    };
    println!(
        "bootckpt: {} (Z4 Stufe 2: ein Thread wird eingefroren, sein Fortschritt ueber eine \
         BOOTGRENZE gespeichert und im naechsten Lauf wiederhergestellt. Derselbe Kernel macht \
         beides und entscheidet selbst welches -- er liest den Sektor. Der Transport ist der \
         vorhandene Blockdienst; der Kern ist Client. Gebunden ist der Checkpoint an das \
         Kernel-Image: ein fremder Hash wird ABGEWIESEN statt geladen)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
}

/// **A-5.3: hat der Selektor entschieden — oder die Fundreihenfolge?**
///
/// Die naheliegende Pruefung waere „der Treiber hat ein Geraet bekommen". Die ist wertlos: sie ist
/// auch dann wahr, wenn er das falsche bekam, und genau das ist der Fehler, den ein Selektor
/// verhindern soll. Geprueft wird deshalb dreierlei, und der erste Punkt ist der wichtigste:
///
/// 1. **Es gab ueberhaupt etwas zu entscheiden.** Bei nur einem angebotenen Geraet trifft jeder
///    Selektor und jeder Nicht-Selektor dieselbe Wahl — eine gruene Zeile hiesse dann nichts.
///    Weniger als zwei Angebote sind ein **SKIP mit Begruendung**, kein PASS.
/// 2. **Die Zuteilung passt auf den Selektor des Manifest-Eintrags** — Hersteller und Geraete-ID
///    aus dem Autoritaetsdokument gegen die tatsaechlich vergebene Instanz.
/// 3. **Das nicht gewaehlte Geraet ist noch frei.** Erst das macht aus „es passte" ein „es wurde
///    ausgewaehlt": es belegt, dass eine Alternative dastand und liegen blieb.
/// **C2: die DMA-Pools sind ein Argument des Ladens** — der Bericht.
///
/// Vier Aussagen, und keine davon ist „ein Pool ist da":
///
/// * **`eingeloest`** — jede Zuteilung hat genau bekommen, was der Ladeaufruf angefordert hat
///   (oder die Vorgabe bei `0`). Das trennt „so gewollt" von „stillschweigend gekuerzt", und die
///   Kuerzung ist der Fehler, um den es geht: eine halbierte DMA-Region ist ein Geraet, das ueber
///   ihr Ende hinausschreibt.
/// * **`verschieden`** — mindestens zwei Zuteilungen haben verschiedene Groessen. Ohne das waere
///   „eingeloest" auch dann wahr, wenn alle die Vorgabe bekommen und der Parameter nie gelesen
///   wird.
/// * **`disjunkt`** — die IOVA-Fenster ueberschneiden sich paarweise nicht. A-5.4 ist bei EINER
///   Groesse gemessen worden; bei geaenderten Groessen gilt es nicht weiter, sondern wird neu
///   gefahren.
/// * **`zu-gross-abgewiesen`** — der Ring-3-Negativfall aus `init`. Ohne ihn waere die Obergrenze
///   eine Zahl, von der niemand weiss, ob sie beisst.
#[cfg(feature = "selftest")]
fn dmapool_bericht() {
    let mut p = [(0u32, 0u32, 0u64, 0u64); system::MAX_OFFERED_DEVICES];
    let n = system::driver_dma_pools(&mut p);
    if n < 2 {
        println!(
            "dmapool : SKIP ({n} Zuteilung(en) -- unter zweien gibt es keine zwei Groessen zu \
             vergleichen und keine zwei Fenster, die disjunkt sein koennten)"
        );
        DMAPOOL_BEFUND.uebersprungen();
        return;
    }
    let vorgabe = system::driver_dma_default_bytes();
    let mut eingeloest = true;
    let mut verschieden = false;
    let mut disjunkt = true;
    for i in 0..n {
        let (pid, gewuenscht, gewaehrt, iova) = p[i];
        let soll = if gewuenscht == 0 {
            vorgabe
        } else {
            gewuenscht as u64 * 4096
        };
        if gewaehrt != soll {
            eingeloest = false;
        }
        for j in 0..n {
            if i == j {
                continue;
            }
            if p[i].2 != p[j].2 {
                verschieden = true;
            }
            // Fenster: [iova, iova+len). Ueberschneidung ist der Fehler, den A-5.4 auf der
            // Adressachse ausschliesst -- bei geaenderten Groessen neu zu pruefen und nicht als
            // weitergeltend anzunehmen.
            let (a0, a1) = (iova, iova.saturating_add(gewaehrt));
            let (b0, b1) = (p[j].3, p[j].3.saturating_add(p[j].2));
            if a0 < b1 && b0 < a1 {
                disjunkt = false;
            }
        }
        println!(
            "dmapool : Eintrag {pid} wuenschte {gewuenscht} Seiten, bekam {gewaehrt} B bei IOVA {iova:#x}"
        );
    }
    let abgewiesen = root_badge() & POOL_REFUSED_BADGE != 0;
    // **Eine Zahl mit ERWARTUNG, und die Erwartung ist eine SCHULD** (2026-08-26).
    //
    // Erst stand hier `audit == 0` -- rot aus einem Grund, der mit C2 nichts zu tun hat. Dann
    // stand die Zahl nur im Text -- und eine Zahl, deren erwarteter Wert 2 ist, wird nicht
    // geprueft: wuerde sie aus einem ECHTEN Grund 3, feuerte nichts. Beide Fassungen waren falsch.
    //
    // Jetzt ist die Gleichheit das Konjunkt. `2` ist dabei **kein Freibrief**: `for_each_dma`
    // laeuft objekt- und nicht cap-granular, ausdruecklich damit Kopien nicht doppelt zaehlen --
    // `reassign_driver_device` praegt beim Hot-Reload aber ein ZWEITES Dma-OBJEKT ueber dieselbe
    // Region statt den Cap zu kopieren. Der Wert sagt also die Wahrheit, und die Wahrheit ist eine
    // offene Schuld (todo A3d). Er geht auf 0, sobald der Reload kopiert statt praegt; bis dahin
    // faengt diese Zeile jede DRIFT.
    let audit = system::dma_audit();
    let audit_wie_erwartet = audit == DMA_AUDIT_SCHULD;
    let ok = eingeloest && verschieden && disjunkt && abgewiesen && audit_wie_erwartet;
    println!(
        "dmapool : {} (eingeloest={eingeloest} verschieden={verschieden} disjunkt={disjunkt} \
         zu-gross-abgewiesen={abgewiesen} zuteilungen={n} vorgabe={vorgabe} B \
         dma_audit={audit} erwartet={DMA_AUDIT_SCHULD} wie-erwartet={audit_wie_erwartet} \
         -- die Erwartung ist eine SCHULD, kein Freibrief: reassign_driver_device praegt beim \
         Hot-Reload ein ZWEITES Dma-Objekt ueber dieselbe Region, statt den Cap zu kopieren. \
         Sie geht auf 0, sobald das behoben ist -- s. todo A3d)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    DMAPOOL_BEFUND.gemessen(ok);
}

/// **Die `irqmsi`-Zeile** (Stufe B, B1+B2+B3) — was von der Interrupt-Vergabe belegbar ist,
/// **bevor** der Treiber wartet statt zu pollen.
///
/// Die Konjunkte, und warum jedes einzelne dasteht:
///
/// * **`angeboten`/`da` gedruckt** — der Wunsch neben der Gewährung, wie `dma_gewuenscht` neben
///   `dma_len`. Ohne beide Zahlen sind „das Gerät hat kein MSI-X" (Pollen ist zulässig) und „die
///   Vergabe ist gescheitert" dieselbe Zeile, und die harmlose Lesart verdeckt die ernste.
/// * **`angeboten-dann-da`** (`angeboten ⟹ da`) — genau diese Trennung als Urteil. Ein Gerät mit
///   MSI-X ohne Vektor ist eine **gescheiterte Vergabe**.
///
///   **Der Name trägt kein `=`**, und das ist keine Kosmetik: die Berichtszeilen dieses Baums sind
///   `name=wert`-Paare, und jeder Leser, der auf `=` trennt, liest bei `angeboten=>da=false` den
///   Wert `>da`. Genau daran ist die erste Fassung der Gegenprobe gescheitert — sie meldete
///   `FAIL … ist '>da'` für ein Konjunkt, das korrekt gefallen war.
/// * **`praesent`, `vektor-passt`, `svt`, `sid`** — aus der **Tabelle zurückgelesen**
///   (`irte_pruefen`), nicht nachgerechnet. `svt` ist die Sicherheitsaussage: ohne `SVT=01` nähme
///   der Eintrag eine MSI von JEDEM Gerät an, und die Interruptzustellung wäre genau der Kanal,
///   den A-5.4 auf der DMA-Achse geschlossen hat. `sid` sagt, dass er die **richtige** Quelle
///   prüft — ohne dieses zweite Konjunkt wäre `svt` mit jeder beliebigen BDF wahr.
/// * **`cap-in-slot7`** — die Treiber-PD hält die `Irq`-Cap wirklich (B2). Sie zu prägen und
///   nirgends zu installieren wäre der `SYS_SPAWN`-Zustand: gebaut, ohne Halter.
/// * **`cap-nennt-vektor`** — und es ist **sein** Vektor. Eine `Irq`-Cap auf einen fremden Vektor
///   wäre gültig, prüfbar und falsch.
/// * **`bind-ohne-cap-abgewiesen`** — der Ring-3-Negativfall aus `init` (B3). Ohne ihn wäre der
///   Cap-Riegel eine Behauptung; mit ihm ist er gefahren. Er prüft auf `ERR_BADCAP` und nicht auf
///   „ungleich OK": ein nicht existierender Syscall gäbe `ERR_BADSYS`, und der Negativfall wäre
///   grün, **weil nichts gebaut ist**.
///
/// **Was hier NOCH NICHT steht**, und zwar benannt: `poll-runden == 0`, `zugestellt > 0`,
/// `badge == erwartet`, `kein-retrigger`, `zweite-PD-unberuehrt`. Alle fünf brauchen B4 — der
/// Treiber pollt noch. Sie fehlen hier als **Lücke mit Namen** statt als stillschweigend kürzere
/// Liste; s. `docs/plan-cap-irq.md` §3.
#[cfg(feature = "selftest")]
fn irqmsi_bericht() {
    let mut m = [system::DriverMsi::default(); system::MAX_OFFERED_DEVICES];
    let n = system::driver_msi(&mut m);
    if n == 0 {
        println!("irqmsi  : SKIP (keine Geraetezuteilung -- es gibt nichts zu vergeben)");
        IRQMSI_BEFUND.uebersprungen();
        return;
    }
    let mut angeboten_impl_da = true;
    let mut praesent = true;
    let mut vektor_passt = true;
    let mut svt = true;
    let mut sid = true;
    let mut cap_in_slot7 = true;
    let mut cap_nennt_vektor = true;
    let mut mit_vektor = 0usize;
    for e in m.iter().take(n) {
        if e.angeboten && !e.da {
            angeboten_impl_da = false;
        }
        if !e.da {
            println!(
                "irqmsi  : Eintrag {} (RID {:#06x}) OHNE Vektor -- angeboten={}                  (angeboten=false heisst: das Geraet hat kein MSI-X, Pollen ist zulaessig;                  angeboten=true heisst: die Vergabe ist GESCHEITERT)",
                e.program_id, e.rid, e.angeboten
            );
            continue;
        }
        mit_vektor += 1;
        let b = hal::vtd::irte_pruefen(e.handle, e.vektor, e.rid);
        if !b.praesent {
            praesent = false;
        }
        if !b.vektor_passt {
            vektor_passt = false;
        }
        if !b.svt_gesetzt {
            svt = false;
        }
        if !b.sid_passt {
            sid = false;
        }
        // B2: haelt die PD, die den Treiber faehrt, die Cap wirklich -- und nennt sie SEINEN
        // Vektor? Der Dienst wird ueber die `program_id` gesucht und nicht als „der erste": bei
        // zwei Treibern waere „der erste" eine stille Fehlwahl (A-5.4).
        match crate::loader::driver_service_of(e.program_id) {
            Some(sv) => match system::pd_irq_cap_intid(sv.pd, 7) {
                Some(intid) => {
                    if intid != e.vektor as u32 {
                        cap_nennt_vektor = false;
                    }
                }
                None => cap_in_slot7 = false,
            },
            // Kein laufender Dienst zu dieser Zuteilung -> ueber den Slot ist nichts auszusagen.
            // **Nicht als Erfolg buchen**: eine PD, die es nicht gibt, haelt auch keine Cap.
            None => cap_in_slot7 = false,
        }
        println!(
            "irqmsi  : Eintrag {} (RID {:#06x}) Vektor {:#04x} Handle {} --              tabelle={} praesent={} vektor-passt={} svt={} sid={}",
            e.program_id,
            e.rid,
            e.vektor,
            e.handle,
            b.tabelle_da,
            b.praesent,
            b.vektor_passt,
            b.svt_gesetzt,
            b.sid_passt
        );
    }
    // --- B4: hat ein Treiber wirklich GEWARTET statt zu pollen? --------------------------------
    //
    // Gelesen wird, was der Treiber in seine Region gelegt hat -- geurteilt wird hier. Ein
    // Treiber, der sein eigenes Ergebnis bestaetigt, bestaetigt nichts; er liefert Zahlen.
    //
    // **Es reicht EIN Treiber, und das ist der Umfang der Stufe, keine Nachlaessigkeit.** Von den
    // beiden Zuteilungen faehrt nur `virtio-blk` diesen Weg; `virtio-net` ist ein anderes Programm
    // und pollt weiter. Die Zeile sagt das als `wartende=N/M`, statt es zu verschweigen -- eine
    // Aussage ueber „alle", die nur fuer einen gilt, waere die teurere Luege.
    let mut wartende = 0usize;
    let mut melder = 0usize;
    let mut zeile_weg = false;
    let mut b4_aktiv = false;
    let mut badge_falsch = false;
    let mut gepollt_trotz_vektor = false;
    for e in m.iter().take(n).filter(|e| e.da) {
        // **Erst die Marke.** Ohne sie las diese Schleife die vier Offsets aus JEDER Region --
        // auch aus der von `virtio-net`, wo dort Virtqueue-Bytes liegen. Die Zahlen sahen aus wie
        // Messwerte (`weckrufe=9223372037261623427`) und waren Ringdaten; eine davon machte
        // `wartende` wahr. Ein Wert wird dort gelesen, wo ihn jemand hingeschrieben hat, und nicht
        // dort, wo er stehen koennte.
        // SAFETY: identity-gemappte DMA-Region dieser Zuteilung, ein Wort.
        // **Steht die MSI-X-Zeile ueberhaupt noch im Geraet?** Gelesen, nicht nachgerechnet.
        // Ein Geraetereset setzt sie zurueck (QEMUs `virtio_pci_reset` ruft `msix_reset`), und der
        // Treiber setzt sein Geraet bei JEDER Anfrage zurueck. Ohne dieses Konjunkt saehe „der
        // Interrupt kam nicht" genauso aus wie „die Zeile ist weg".
        let (za, _zh, zd, zc) = unsafe { hal::pcie::msix_read_entry(e.msix_table, 0) };
        let zeile_steht = za != 0 && zc & 1 == 0;
        // **Die Funktionsmaske (Bit 14) und der used-Ring** -- die beiden Groessen, die „das
        // Geraet sendet nicht" von „es sendet und wird verschluckt" trennen.
        //
        // `msix_enable_by_rid` loescht Bit 14 und liest **Bit 15** zurueck; ueber Bit 14 sagte die
        // Ruecklesung bisher nichts, und eine gesetzte Funktionsmaske unterdrueckt JEDEN Vektor,
        // unabhaengig von `vctrl`.
        //
        // `used.idx` ist die andere Haelfte: steht er ueber 0, hat das Geraet die Arbeit **getan**
        // und nur nicht gemeldet -- dann ist es kein Interruptproblem, sondern ein Meldeproblem.
        // Steht er auf 0, gab es nichts zu melden, und die Interruptfrage stellt sich nicht.
        let ctrl = hal::pcie::msix_ctrl_lesen(e.rid, e.msix_cap);
        // SAFETY: identity-gemappte DMA-Region; `used.idx` liegt bei Queue-Offset 0x200 + 2.
        let used_idx = unsafe {
            core::ptr::read_volatile((e.dma_phys + 0x200 + 2) as *const u16)
        };
        // **Haelt die Queue ihren Vektor?** Der Treiber liest ihn beim Schreiben zurueck, aber
        // sein Ergebnis erreicht den Bericht nur nach einer FERTIGEN Anfrage -- blockiert er, ist
        // die Zahl nie zu sehen. Der Kernel loest den Transport dafuer selbst auf; er darf das,
        // die Konfigurationsseite gehoert ihm.
        // SAFETY: `cfg_page` ist die identity-gemappte Konfigurationsseite genau dieser Funktion.
        let qvec = unsafe {
            hal::virtio::probe_ecam(e.cfg_page, || {})
                .map(|t| t.queue_msix_lesen(0))
                .unwrap_or(0xfffe)
        };
        println!(
            "irqmsi  : Eintrag {} Geraet: msix-ctrl={ctrl:#06x} (Bit15=Enable Bit14=FunctionMask) used.idx={used_idx} queue0-msix-vektor={qvec:#06x} (0xffff = KEINE -- dann sendet das Geraet fuer diese Queue nichts)",
            e.program_id
        );
        if !zeile_steht {
            zeile_weg = true;
        }
        println!(
            "irqmsi  : Eintrag {} MSI-X-Zeile 0: addr={za:#x} data={zd:#x} vctrl={zc:#x} (Bit 0 = maskiert) steht={zeile_steht}",
            e.program_id
        );
        let marke = unsafe { core::ptr::read_volatile((e.dma_phys + B4_OFF_MAGIC) as *const u64) };
        if marke != B4_MAGIC {
            println!(
                "irqmsi  : Eintrag {} B4: kein Melder (Marke {marke:#x}) -- dieses Programm faehrt den Warteweg nicht; virtio-net ist so eines",
                e.program_id
            );
            continue;
        }
        melder += 1;
        // SAFETY: wie oben.
        let aktiv = unsafe { core::ptr::read_volatile((e.dma_phys + B4_OFF_AKTIV) as *const u64) };
        if aktiv != 0 {
            b4_aktiv = true;
        }
        // SAFETY: `dma_phys` ist die vom Kernel ausgeschnittene, identity-gemappte DMA-Region
        // dieser Zuteilung; die vier Offsets liegen darin und ausserhalb von Queue, Kopf und
        // Datenpuffer (s. `virtio-blk`, OFF_POLLED..OFF_MSIX_OK).
        let (gepollt, weckrufe, badge, msix_ok) = unsafe {
            (
                core::ptr::read_volatile((e.dma_phys + B4_OFF_POLLED) as *const u64),
                core::ptr::read_volatile((e.dma_phys + B4_OFF_WAKEUPS) as *const u64),
                core::ptr::read_volatile((e.dma_phys + B4_OFF_BADGE) as *const u64),
                core::ptr::read_volatile((e.dma_phys + B4_OFF_MSIX_OK) as *const u64),
            )
        };
        println!(
            "irqmsi  : Eintrag {} B4: gepollt={gepollt} weckrufe={weckrufe} badge={badge:#x} msix-ok={msix_ok}",
            e.program_id
        );
        if weckrufe > 0 {
            wartende += 1;
            // Badge und Poll-Weg zaehlen nur, wo ueberhaupt geweckt wurde -- sonst waere `0` ein
            // Fehler, obwohl nichts schiefgegangen ist.
            if badge != B4_ERWARTETES_BADGE {
                badge_falsch = true;
            }
            if gepollt != 0 {
                gepollt_trotz_vektor = true;
            }
        }
    }
    // --- Die Halbierung: eine MSI vom KERN aus ------------------------------------------------
    //
    // Sie steht VOR dem Urteil und nicht daneben, weil sie die Frage entscheidet, die alle anderen
    // Konjunkte offenlassen: bis heute war der Zustellpfad **nie gefahren** -- der IRTE-Selbsttest
    // liest nur den Tabelleninhalt zurueck. Jede gruene Zeile darueber war damit eine Aussage
    // ueber Zustand, keine ueber Wirkung.
    let zp = system::msi_zustellprobe();
    println!(
        "irqmsi  : Zustellprobe (Store vom KERN, kein Geraet): sprechfaehig={} addr={:#x} data={:#x} | remappable: angekommen={} zugestellt={} badge={} | KOMPAT: angekommen={} zugestellt={} | letzter-vektor={:#x} iommu-faults-leer={} -- ANGEKOMMEN zaehlt irq_hook (der Prozessor hat ihn gesehen), ZUGESTELLT den Drain (er kam bis zur Notification). Zwei Zahlen, weil drei Lagen sonst ununterscheidbar sind: nie angekommen / angekommen und Vektor unbekannt / angekommen und nie gedrained. Die remappable Haelfte ist unter QEMU NICHT aussagekraeftig (die IR-Region liegt im GERAETE-Adressraum; ein CPU-Store geht daran vorbei)",
        zp.sprechfaehig, zp.addr, zp.data, zp.remap_angekommen, zp.zugestellt, zp.badge_stimmt, zp.kompat_angekommen, zp.kompat_zugestellt, zp.letzter_vektor, zp.faults_leer
    );
    let zugestellt = system::irqs_delivered();
    let bind_abgewiesen = root_badge() & IRQ_UNAUTHORIZED_BADGE != 0;
    // **Sprechprobe:** ohne einen einzigen vergebenen Vektor sagen `praesent`/`svt`/`sid` nichts --
    // sie sind dann wahr, weil die Schleife nicht lief. Ein leerer Lauf ist kein Testergebnis.
    let sprechfaehig = mit_vektor > 0;
    let ok = sprechfaehig
        && angeboten_impl_da
        && praesent
        && vektor_passt
        && svt
        && sid
        && cap_in_slot7
        && cap_nennt_vektor
        && bind_abgewiesen
        // **Die Zeile steht im GERAET** -- zurueckgelesen, nicht nachgerechnet. Sie gattert auch
        // ohne B4: ohne sie ist eine geschriebene Zeile von einer weggeräumten nicht zu trennen.
        && !zeile_weg
        // **Die B4-Haelfte ist BEDINGT** (`b4-aktiv`), und das ist dieselbe Disziplin wie
        // `angeboten-dann-da`: solange der Treiber den Warteweg nicht faehrt, sagen `wartende`,
        // `nicht-gepollt`, `badge-stimmt` und `zugestellt` NICHTS -- sie waeren wahr, weil nichts
        // lief. Eine unbedingte Fassung haette genau zwei Enden: rot aus einem benannten Grund
        // (und jemand entfernt die Zeile), oder abgeschaltet (und sie misst nie wieder).
        //
        // `melder > 0` bleibt unbedingt: dass der Treiber ueberhaupt MELDET, ist die Sprechprobe.
        && melder > 0
        && (!b4_aktiv
            || (wartende > 0 && !gepollt_trotz_vektor && !badge_falsch && zugestellt > 0));
    println!(
        "irqmsi  : {} (zuteilungen={n} mit-vektor={mit_vektor} angeboten-dann-da={angeboten_impl_da} praesent={praesent} vektor-passt={vektor_passt} svt={svt} sid={sid} cap-in-slot7={cap_in_slot7} cap-nennt-vektor={cap_nennt_vektor} bind-ohne-cap-abgewiesen={bind_abgewiesen} melder={melder} zeile-steht={} b4-aktiv={b4_aktiv} wartende={wartende}/{melder} nicht-gepollt={} badge-stimmt={} zugestellt={zugestellt} -- B1..B3 gemessen. B4 haengt an b4-aktiv: der Treiber BINDET (ueber die ABI, aus einer echten Treiber-PD), WARTET aber noch nicht -- IRTE praesent mit SVT/SID, MSI-X-Zeile im Geraet zurueckgelesen und unmaskiert, queue_msix_vector angenommen, und IRQ_DELIVERED bleibt 0. Offen. Ebenfalls offen und benannt: kein-retrigger, zweite-PD-unberuehrt)",
        if ok { "ALL PASS" } else { "FAILURES" },
        !zeile_weg,
        !gepollt_trotz_vektor,
        !badge_falsch
    );
    IRQMSI_BEFUND.gemessen(ok);
}

/// Offsets der B4-Zahlen in der DMA-Region einer Treiber-PD — **Spiegel** von
/// `programs/hardware/virtio-blk` (`OFF_POLLED` … `OFF_MSIX_OK`).
///
/// Sie stehen hier ein zweites Mal, weil Kernel und Treiber getrennt gebaut werden und kein
/// gemeinsames Modul haben — dieselbe Lage wie bei den Badges. Laufen sie auseinander, liest diese
/// Zeile Nullen, und die Konjunkte fallen. Ein **stiller Erfolg** ist dabei nicht moeglich, und
/// das ist die Bedingung, unter der eine doppelt gefuehrte Zahl tragbar ist: `0` ist bei
/// `weckrufe` und `msix-ok` die schlechte Richtung.
#[cfg(feature = "selftest")]
const B4_OFF_POLLED: u64 = 0x608;
#[cfg(feature = "selftest")]
const B4_OFF_WAKEUPS: u64 = 0x610;
#[cfg(feature = "selftest")]
const B4_OFF_BADGE: u64 = 0x618;
#[cfg(feature = "selftest")]
const B4_OFF_MSIX_OK: u64 = 0x620;
/// Marke des Melders — ohne sie sind die vier Zahlen darueber **fremde Bytes**.
#[cfg(feature = "selftest")]
const B4_OFF_MAGIC: u64 = 0x628;
#[cfg(feature = "selftest")]
const B4_MAGIC: u64 = 0x4234_4D45_4C44_4552;
/// Faehrt der Treiber den Warteweg? — Spiegel von `virtio-blk`s `B4_WARTEN`.
#[cfg(feature = "selftest")]
const B4_OFF_AKTIV: u64 = 0x630;
/// Das Etikett, das `virtio-blk` beim Binden waehlt (`IRQ_BADGE` dort).
#[cfg(feature = "selftest")]
const B4_ERWARTETES_BADGE: u64 = 0x1;

/// Urteil der `irqmsi`-Zeile (Stufe B).
#[cfg(feature = "selftest")]
static IRQMSI_BEFUND: crate::befund::AtomicBefund = crate::befund::AtomicBefund::neu();

/// Urteil der `dmapool`-Zeile (C2).
#[cfg(feature = "selftest")]
static DMAPOOL_BEFUND: crate::befund::AtomicBefund = crate::befund::AtomicBefund::neu();

/// **Der erwartete `dma_audit`-Wert am Ende eines Laufs mit Hot-Reload -- eine RATSCHE nach unten.**
///
/// `2` heisst „zwei DMA-Objekte auf einer Region", und das ist keine Eigenheit des Pruefers:
/// `for_each_dma` zaehlt **Objekte** und nicht Caps, gerade damit Kopien nicht doppelt zaehlen.
/// `reassign_driver_device` praegt beim Austausch ein zweites Objekt, statt den vorhandenen Cap zu
/// kopieren -- die Zahl sagt also die Wahrheit ueber eine offene Schuld (todo A3d).
///
/// Sie steht hier als Konstante und nicht als Literal in der Bedingung, damit die Behebung EINE
/// Zeile ist: `2` -> `0`. Und sie darf **nur fallen**; wer sie erhoeht, deckt eine neue Ueberlappung
/// zu.
#[cfg(feature = "selftest")]
const DMA_AUDIT_SCHULD: u32 = 2;

fn devsel_bericht() {
    let angeboten_vorher = ANGEBOTEN_VORHER.load(Ordering::Acquire);
    let mut zu = [(0u32, 0u32, 0u16, 0u16); system::MAX_OFFERED_DEVICES];
    let n_zu = system::driver_assignments(&mut zu);
    let frei = system::offered_device_count();

    if angeboten_vorher < 2 {
        println!(
            "devsel  : SKIP ({angeboten_vorher} Geraet(e) angeboten -- bei weniger als zweien gibt \
             es nichts zu entscheiden, und eine gruene Zeile hiesse hier nichts. Der Selektor ist \
             erst pruefbar, wenn eine Alternative dasteht, die liegen bleiben KANN)"
        );
        return;
    }
    let Some(man) = crate::loader::read_manifest() else {
        println!("devsel  : SKIP (kein Manifest -- ohne Autoritaetsdokument wird nichts zugeteilt)");
        return;
    };

    // **Jede** Zuteilung gegen den Selektor **ihres** Eintrags. Ein Sammelurteil ueber "die
    // Zuteilung" waere bei zwei Treibern wieder die Aussage, die auch bei vertauschten Geraeten
    // wahr ist -- und genau das soll der Selektor ausschliessen.
    let mut alle_passen = n_zu > 0;
    let mut ids_genannt = n_zu > 0;
    for &(pid, rid, ven, dev) in zu.iter().take(n_zu) {
        let Some(e) = (0..man.count()).filter_map(|i| man.entry(i)).find(|e| e.program_id == pid)
        else {
            alle_passen = false;
            continue;
        };
        // Der Klassenvergleich entfaellt: `driver_assignments` fuehrt die Klasse nicht mit, und
        // ein Platzhalter wuerde einen klassenbasierten Selektor stillschweigend durchwinken.
        if !e.device.matches(ven, dev, u32::MAX) {
            alle_passen = false;
        }
        if e.device.vendor == caprock_loader::manifest::ANY16
            && e.device.device == caprock_loader::manifest::ANY16
        {
            ids_genannt = false;
        }
        println!("devsel  : Eintrag {pid} verlangt {:04x}:{:04x}, bekam RID {rid:#06x} {ven:04x}:{dev:04x}", e.device.vendor, e.device.device);
    }
    // Die Bilanz: was vergeben wurde, fehlt im Angebot -- und zwar genau das.
    let bilanz = frei + n_zu == angeboten_vorher;
    let ok = alle_passen && ids_genannt && bilanz;
    println!(
        "devsel  : {} (A-5.3/A-5.4: {angeboten_vorher} angeboten, {n_zu} vergeben, {frei} frei. \
         JEDE Zuteilung wurde gegen den Selektor IHRES Manifest-Eintrags geprueft -- ein \
         Sammelurteil ueber 'die Zuteilung' waere bei zwei Treibern auch dann wahr, wenn die \
         Geraete vertauscht waeren)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
}

static DRIVER_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-5.3: wie viele Geräte angeboten waren, **bevor** die erste Zuteilung stattfand. Hinterher
/// nicht mehr feststellbar — ein vergebenes Gerät ist aus der Liste heraus.
static ANGEBOTEN_VORHER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
/// A-6.1: traegt das Dienstprotokoll (Auskunft, Lesen, Schreiben, Flush, Bereichsfehler)?
static BLKDEV_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-6.2: wurde die Partitionstabelle richtig gelesen?
static PART_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A-6.3: hat die Dateisystem-PD ihre Datei gelesen?
static FS_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Was `tools/mkgpt.py --file` ins Dateisystem legt: "CAPROCKS-DATEIINHALT" (20 Byte).
const FS_DATEI_GROESSE: u64 = 20;
/// Die ersten acht Bytes davon, little-endian gelesen ("CAPROCKS").
const FS_ERSTE_ACHT: u64 = 0x534B_434F_5250_4143;
/// A-6.4: auf diese Groesse schreibt die Probe die Datei — ueber einen zweiten Cluster hinaus.
/// Eine Schreibprobe, die in den vorhandenen Cluster passt, prueft die Kettenverlaengerung nicht.
const FS_NEU_GROESSE: u64 = 700;

// --- A-5.1: der Client des Treiber-DIENSTES ---------------------------------------------------
//
// Der Kernel ist hier **Client**, nicht Treiber. Das ist genau die Richtungsumkehr: bis A-5.1
// fuhr er den virtio-Handshake selbst; jetzt fragt er einen Dienst und bekommt eine Antwort.
// Zwischen den beiden Anfragen wird der Dienst **ausgetauscht** -- und der Client merkt davon
// nichts ausser einem weitergezaehlten Bedienungszaehler.
// Das Dienstprotokoll (A-6.1) -- muss zu `programs/hardware/virtio-blk` passen.
const OP_INFO: u64 = 0;
const OP_READ: u64 = 1;
const OP_WRITE: u64 = 3;
const OP_FLUSH: u64 = 4;
const OP_SCAN: u64 = 5;
/// Sektor, auf den die Probe schreibt — in der **zweiten** (rohen) Partition und **nicht** die
/// Magie selbst. Ein Test, der seine eigene Referenz ueberschreibt, prueft ab dem zweiten
/// Lauf etwas anderes als beim ersten.
const PROBE_SECTOR: u64 = 20002;
/// Erwartete Partitionstabelle des Testabbilds (`tools/mkgpt.py`).
const ERWARTET_PARTITIONEN: u64 = 2;
const ERWARTET_ERSTE_LBA: u64 = 34;
const ERWARTET_ERSTE_SEKTOREN: u64 = 19967;
/// Ergebnisse der Protokollfolge.
static DRV_SEQ: [core::sync::atomic::AtomicU64; 12] = [
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(u64::MAX),
];
/// Ergebnis der ersten Anfrage: `(Status, erste acht Byte, Kapazitaet, Bedienungszaehler)`.
static DRV_R1: [core::sync::atomic::AtomicU64; 4] = [
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];
/// Ergebnis der zweiten Anfrage (nach dem Austausch).
static DRV_R2: [core::sync::atomic::AtomicU64; 4] = [
    core::sync::atomic::AtomicU64::new(u64::MAX),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];
/// Das erste Wort im Datenpuffer des Treibers **am Ende seiner Dienstfolge** — erfasst dort, wo
/// die Aussage gilt, nicht im Bericht (s. `report_and_off`). `u64::MAX` = nie erfasst.
static DRV_DMA_WORT: AtomicU64 = AtomicU64::new(u64::MAX);
static DRV_CALL1_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DRV_CALL2_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DRV_SWAPPED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Wie weit die Dienst-/Austausch-Abfolge ist (0 = noch nichts, 1 = Client laeuft, 2 = getauscht).
static DRV_STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Ergebnis des Austauschs, als Text fuer den Bericht.
static DRV_RELOAD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);

/// **Die Abfolge von A-5.1**, Schritt fuer Schritt aus der Hauptschleife des Hochlaufs getrieben.
///
/// Sie laeuft hier und nicht in einem eigenen Faden, weil ihre Schritte **aufeinander warten**
/// muessen: erst wenn der Treiber bereit ist, darf gefragt werden; erst wenn die erste Antwort da
/// ist, darf getauscht werden. Ein Faden, der das mit Schlaeuchen loest, waere schwerer zu lesen
/// als eine Zustandsmaschine, die bei jedem Durchlauf genau eine Bedingung prueft.
#[cfg(feature = "selftest")]
fn drv_service_step(archive: bool) {
    match DRV_STEP.load(Ordering::Acquire) {
        0 => {
            // **Ohne Boot-Archiv gibt es nie einen Treiber-Dienst.** Das muss hier entschieden
            // werden und nicht im Bericht: `BLKDEV_OK` steht in `all_done()`, und was der Bericht
            // setzt, kann den Bericht nicht ausloesen. Beim ersten Anlauf lief die Hauptsuite
            // deshalb in den Watchdog -- sie hat kein Archiv, also wartete die Zustandsmaschine
            // auf einen Dienst, der per Aufbau nie kommt.
            //
            // `driver_service().is_none()` allein taugt nicht als Kriterium: unmittelbar nach dem
            // Start ist es auch dann None, wenn gleich einer kommt. "Es gibt kein Archiv" ist die
            // Aussage, die von Anfang an feststeht.
            if !archive {
                BLKDEV_OK.store(true, Ordering::Release); // nicht anwendbar, nicht bestanden
                PART_OK.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
                return;
            }
            // Warten, bis der Treiber sein Geraet aufgeloest hat und in `recv` steht. Vorher zu
            // fragen hiesse, auf eine Zusage zu bauen, die er noch nicht gegeben hat.
            let Some(svc) = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID) else { return };
            let badge = system::notification_pending(svc.ntfn);
            if badge & DRV_READY_BADGE == 0 {
                return;
            }
            // **Erst die Dateisystem-PD, dann diese Folge.** Beide sind Clients desselben
            // Dienstes; liefen sie gleichzeitig, mischten sich ihre Anfragen, und der Austausch
            // fiele mitten in ein fremdes Gespraech. Der Bedienungszaehler waere dann auch keine
            // Aussage mehr ueber DIESE Folge.
            // **Genannt, nicht „die" Client-PD.** Bis zum 2026-08-10 stand hier
            // `client_notification()` ohne Argument -- eine Zelle, zwei Subjekte: diese Zeile
            // meint die DATEISYSTEM-PD, die Zeile in Schritt 2 meint `wasmhost`. Solange es nur
            // einen Client gab, war das dieselbe Zahl. Mit dem zweiten zeigte sie auf wasmhost,
            // dieses Badge kam nie, und `drv`/`blkdev`/`part` fielen aus, ohne dass am Treiber
            // etwas kaputt war.
            if let Some(n) = crate::loader::client_notification_of(TEST_FS_PROGRAM_ID) {
                if system::notification_pending(n) & crate::loader::CLIENT_NTFN_BADGE == 0 {
                    return;
                }
            }
            // Client-PD mit einer SEND-Cap auf den Kanal. Mehr braucht ein Client nicht -- er
            // kann rufen und sonst nichts, insbesondere den Dienst nicht empfangen.
            let Ok(ep_cap) = system::install_endpoint_cap(svc.ep as u32, Rights::WRITE) else {
                return;
            };
            let Some(cli_pd) = system::create_pd() else { return };
            if !system::install_pd_cap(cli_pd, 0, ep_cap) {
                return;
            }
            let Some(cli) = system::spawn_parked(drv_client as *const () as usize, 0, system::IDLE_PRIO)
            else {
                return;
            };
            let _ = system::admit_in_pd(cli_pd, cli);
            DRV_STEP.store(1, Ordering::Release);
        }
        1 => {
            // Erst tauschen, wenn die erste Anfrage beantwortet ist -- sonst belegte der zweite
            // Aufruf nicht, dass eine ANDERE Fassung antwortet.
            if !DRV_CALL1_DONE.load(Ordering::Acquire) {
                return;
            }
            let out = crate::loader::reload_driver(TEST_BLK_SERVICE_ID);
            DRV_RELOAD.store(out as u32, Ordering::Release);
            DRV_SWAPPED.store(true, Ordering::Release);
            DRV_STEP.store(2, Ordering::Release);
        }
        2 => {
            // **Das Urteil wird HIER gebildet, nicht im Bericht.** Es steht in `all_done()`, und
            // ein Urteil, das erst im Bericht entsteht, koennte den Bericht nicht ausloesen --
            // der Lauf liefe in den Watchdog und druckte das Ergebnis trotzdem. Das sah beim
            // ersten Anlauf aus wie "gruen, aber gehangen" und war eine Zirkularitaet.
            if !DRV_CALL2_DONE.load(Ordering::Acquire) {
                return;
            }
            let q = |i: usize| DRV_SEQ[i].load(Ordering::Acquire);
            BLKDEV_OK.store(
                q(0) == 0
                    && q(1) == 32768
                    && q(2) == 512
                    && q(3) == 0
                    && q(4) == 0
                    && q(5) == 0
                    && q(6) == BLK_MAGIC
                    && q(7) == 3,
                Ordering::Release,
            );
            // **A1 auf dem regulaeren Weg, EINMAL gemessen.** Hier, weil `init` seine PDs
            // laengst geladen hat -- frueher waere die Kandidatenliste leer, und leer haette
            // wie "nichts zu beanstanden" ausgesehen.
            PDCOLOR_OK.store(crate::loader::run_pdcolor(), Ordering::Release);
            {
                // **Abwesenheit wird an der ENDOWMENT-TABELLE entschieden, nicht am Schweigen.**
                // Bis zum 2026-08-10 stand hier „kein Bit gesetzt -> nicht anwendbar". Das ist der
                // Schluss von Schweigen auf Abwesenheit, den dieses Projekt sonst verbietet: eine
                // WASM-PD, die laeuft und nichts meldet, war von einer, die es gar nicht gibt,
                // nicht zu unterscheiden -- und genau so stand die Zeile auf SKIP, waehrend die
                // PD im Archiv lag.
                let alle = WASM_INST | WASM_RESULT | WASM_REJECT | WASM_TRAP;
                WASM_OK.store(
                    match crate::loader::client_notification_of(TEST_WASM_PROGRAM_ID) {
                        None => true, // wirklich keine WASM-PD endowt -> nicht anwendbar
                        Some(n) => system::notification_pending(n) & alle == alle,
                    },
                    Ordering::Release,
                );
            }
            LADEPOL_OK.store(crate::loader::run_ladepolitik(), Ordering::Release);
            PART_OK.store(
                q(8) == 0
                    && q(9) == ERWARTET_PARTITIONEN
                    && q(10) == ERWARTET_ERSTE_LBA
                    && q(11) == ERWARTET_ERSTE_SEKTOREN,
                Ordering::Release,
            );
            // **Den Datenpuffer des Treibers JETZT erfassen, nicht im Bericht.** Genau hier gilt
            // die Aussage „der Blockdienst hat die Magie geliefert"; jeder spaetere Client (seit
            // Z4 Stufe 2 gibt es einen) legt dort seinen eigenen Inhalt ab.
            if let Some((dma_phys, _)) = system::driver_assignment(TEST_BLK_PROGRAM_ID) {
                // SAFETY: identity-gemappte, vom Kernel ausgeschnittene DMA-Region; nur lesend.
                let w = unsafe {
                    core::ptr::read_volatile((dma_phys + hal::virtio::blk::OFF_DATA) as *const u64)
                };
                DRV_DMA_WORT.store(w, Ordering::Release);
            }
            // **Der Checkpoint laeuft VOR dem Kreuz-DMA-Nachweis**, und das ist keine
            // Geschmacksfrage: A-5.4 liest das erste Wort der fremden DMA-Region vor und nach dem
            // Fremdversuch. Eine Platten-E/A dazwischen ginge durch genau diese Region -- der
            // „unabhaengige Zeuge" saehe dann eine Aenderung, die der Angreifer nicht gemacht hat.
            DRV_STEP.store(5, Ordering::Release);
        }
        // --- Z4 Stufe 2: den Sektor lesen und entscheiden, was dieser Lauf ist -----------------
        5 => {
            let Some(svc) = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID) else {
                CKPT_DONE.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
                return;
            };
            // Der Client braucht nur eine SEND-Cap auf den Kanal -- er kann rufen und sonst
            // nichts. Derselbe Aufbau wie `drv_client`; der Kernel ist auch hier Client.
            let Ok(ep_cap) = system::install_endpoint_cap(svc.ep as u32, Rights::WRITE) else {
                return;
            };
            let Some(cli_pd) = system::create_pd() else { return };
            if !system::install_pd_cap(cli_pd, 0, ep_cap) {
                return;
            }
            let Some(cli) = system::spawn_parked(ckpt_client as *const () as usize, 0, system::IDLE_PRIO)
            else {
                return;
            };
            let _ = system::admit_in_pd(cli_pd, cli);
            CKPT_REQ.store(1, Ordering::Release);
            DRV_STEP.store(6, Ordering::Release);
        }
        6 => {
            if CKPT_ACK.load(Ordering::Acquire) < 1 {
                return;
            }
            let Some((shared, _)) = system::driver_shared_region(TEST_BLK_SERVICE_ID) else {
                CKPT_STATE.store(CKPT_ERROR, Ordering::Release);
                CKPT_DONE.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
                return;
            };
            let mut sek = [0u8; 512];
            // SAFETY: identity-gemappte, vom Kernel ausgeschnittene Uebertragungsflaeche
            // (`SHARED_BYTES` >= 8 KiB); es werden 512 Byte daraus KOPIERT, nicht ausgewertet.
            // Die Auswertung passiert danach in einer abhaengigkeitsfreien Crate ohne `unsafe`.
            unsafe { core::ptr::copy_nonoverlapping(shared as *const u8, sek.as_mut_ptr(), 512) };

            // **Die Negativkontrolle laeuft in JEDEM Lauf mit, nicht nur im Fehlerfall.** Die
            // TREIBER-PD haelt echte Geraete-Autoritaet (MMIO-Fenster, DMA-Region) -- sie ist der
            // Fall, an dem Z4b etwas zu sagen hat. Ihr Kanal (Endpoint + Notification) liegt dabei
            // ausdruecklich IM Umfang: damit bleibt als Grund nur die Geraete-Autoritaet uebrig,
            // und die Absage ist keine, die ein groesserer Umfang beheben koennte.
            if let Some(svc) = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID) {
                use caprock_cap::checkpoint::{classify_all, Scope};
                let mut kinds = [None; 16];
                let n = system::pd_object_kinds(svc.pd, &mut kinds);
                let (eps, ns) = ([svc.ep as u32], [svc.ntfn as u32]);
                let scope = Scope { endpoints: &eps, notifications: &ns, ..Scope::EMPTY };
                match classify_all(&kinds[..n], &scope) {
                    Err((slot, r)) => {
                        CKPT_REFUSED[0].store(slot as u32, Ordering::Release);
                        CKPT_REFUSED[1].store(ckpt_reason(r), Ordering::Release);
                    }
                    // `Ok` heisst hier NICHT „bestanden": eine Treiber-PD, deren Cspace nichts
                    // Geraetegebundenes enthaelt, hat gar keine Geraete-Autoritaet -- dann hat der
                    // Test nichts gemessen. Der Grund-Code 0 sagt genau das.
                    Ok(()) => {
                        CKPT_REFUSED[0].store(n as u32, Ordering::Release);
                        CKPT_REFUSED[1].store(0, Ordering::Release);
                    }
                }
            }

            let kh = crate::loader::kernel_code_hash();
            match caprock_cap::checkpoint::Image::decode(&sek, &kh) {
                Ok(img) => {
                    CKPT_DECODE.store(0, Ordering::Release);
                    CKPT_PROGRESS.store(img.progress, Ordering::Release);
                    CKPT_NONCE.store(img.nonce, Ordering::Release);
                    CKPT_EPOCH.store(img.epoch, Ordering::Release);
                    CKPT_CAPS.store(img.cap_count as u32, Ordering::Release);
                    CKPT_PRECOND.store(img.precondition_bits, Ordering::Release);
                    CKPT_STATE.store(CKPT_RESTORED, Ordering::Release);
                }
                Err(e) => {
                    CKPT_DECODE.store(ckpt_code(&e), Ordering::Release);
                    if e == caprock_cap::checkpoint::ImageError::NoImage {
                        CKPT_STATE.store(CKPT_SAVED, Ordering::Release); // Vorhaben, noch kein Befund
                    } else {
                        // **Abgewiesen -- und der Sektor bleibt, wie er ist.** Ihn hier zu
                        // ueberschreiben waere bequem und faelschte den naechsten Lauf: aus einem
                        // fremden Checkpoint wuerde stillschweigend ein eigener.
                        CKPT_STATE.store(CKPT_REJECTED, Ordering::Release);
                        CKPT_REQ.store(3, Ordering::Release);
                        CKPT_DONE.store(true, Ordering::Release);
                        DRV_STEP.store(3, Ordering::Release);
                        return;
                    }
                }
            }
            if CKPT_STATE.load(Ordering::Acquire) == CKPT_RESTORED {
                CKPT_FREEZE_DEADLINE.store(hal::timer::ticks(0) + 300, Ordering::Release);
                DRV_STEP.store(7, Ordering::Release);
            } else {
                // **Kaltstart: nichts einzufrieren.** Der erste Anlauf fror den Thread auch hier
                // ein -- und liess ihn dann im Wartezustand STEHEN. Der Zuwachs, auf den gewartet
                // wurde, konnte damit nie eintreten (`Zuwachs-erreicht=0`, 20 s Notschranke).
                // Einfrieren gehoert zum ANFASSEN des Zustands, nicht zum Warten darauf.
                CKPT_WAIT_BASE.store(WORKER_ROUNDS[0].load(Ordering::Relaxed), Ordering::Release);
                CKPT_WAIT_UNTIL.store(hal::timer::ticks(0) + 600, Ordering::Release);
                DRV_STEP.store(10, Ordering::Release);
            }
        }
        // --- Z4a am echten Gegenstand: einfrieren, Zustand anfassen, auftauen -------------------
        7 => {
            use system::Freeze;
            let raw = WORKER_TID0.load(Ordering::Acquire);
            let Some(tid) = (raw != 0).then(|| caprock_sched::ThreadId::from_raw(raw)) else {
                CKPT_STATE.store(CKPT_ERROR, Ordering::Release);
                CKPT_REQ.store(3, Ordering::Release);
                CKPT_DONE.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
                return;
            };
            match system::freeze_thread(tid) {
                Freeze::Frozen => {}
                // **`StillRunning` ist voruebergehend, `Busy` ist eine Absage** (Z4a). Nur der
                // erste Fall wird wiederholt -- und auch der nicht ewig: die Schranke zaehlt in
                // TICKS, nicht in Umdrehungen. Eine Umdrehungszahl misst die Taktrate.
                Freeze::StillRunning
                    if hal::timer::ticks(0) < CKPT_FREEZE_DEADLINE.load(Ordering::Acquire) =>
                {
                    return;
                }
                _ => {
                    CKPT_STATE.store(CKPT_ERROR, Ordering::Release);
                    CKPT_REQ.store(3, Ordering::Release);
                    CKPT_DONE.store(true, Ordering::Release);
                    DRV_STEP.store(3, Ordering::Release);
                    return;
                }
            }
            CKPT_FROZEN.store(true, Ordering::Release);

            // **Wiederherstellen.** Der Zustand, um den es geht, IST dieser Zaehler -- er wird
            // gesetzt, waehrend der Thread steht. Ihn im Laufen zu setzen hiesse, gegen den
            // Thread zu schreiben, den man gerade wiederherstellt.
            //
            // Der Wert VOR dem Setzen wird mitgemeldet: er ist die Positivkontrolle der Suite.
            // Waeren gefundener und eigener Wert gleich, waere „gesetzt" von „nicht gesetzt"
            // nicht zu unterscheiden -- und genau das ist im Mutationslauf passiert.
            CKPT_BEFORE.store(WORKER_ROUNDS[0].load(Ordering::Relaxed), Ordering::Release);
            WORKER_ROUNDS[0].store(CKPT_PROGRESS.load(Ordering::Acquire), Ordering::Relaxed);
            CKPT_AFTER.store(WORKER_ROUNDS[0].load(Ordering::Relaxed), Ordering::Release);
            // **Sofort auftauen.** „Jetzt arbeiten lassen, dann erst speichern" heisst, dass der
            // Thread LAUFEN muss -- ein eingefrorener Thread erreicht keinen Zuwachs, und die
            // Wartestufe darunter wartete dann auf etwas, das per Aufbau nicht eintreten kann.
            let _ = system::thaw_thread(tid);
            CKPT_WAIT_BASE.store(WORKER_ROUNDS[0].load(Ordering::Relaxed), Ordering::Release);
            CKPT_WAIT_UNTIL.store(hal::timer::ticks(0) + 600, Ordering::Release);
            DRV_STEP.store(10, Ordering::Release);
        }
        // --- Warten, bis der Zuwachs da ist ----------------------------------------------------
        //
        // Gezaehlt wird in **Runden**, nicht in Ticks: der Zuwachs ist die Groesse, um die es geht,
        // und eine Zeitspanne haette ihn nur mittelbar erzeugt. Die Tick-Schranke daneben ist eine
        // Notbremse -- sie verhindert einen Haenger, ist aber kein Erfolgskriterium; ob der
        // Zuwachs wirklich zustande kam, steht in `CKPT_DELTA_OK` und im Bericht.
        10 => {
            let base = CKPT_WAIT_BASE.load(Ordering::Acquire);
            if WORKER_ROUNDS[0].load(Ordering::Relaxed) >= base + CKPT_MIN_DELTA {
                CKPT_DELTA_OK.store(true, Ordering::Release);
            } else if hal::timer::ticks(0) < CKPT_WAIT_UNTIL.load(Ordering::Acquire) {
                return;
            }
            CKPT_FREEZE_DEADLINE.store(hal::timer::ticks(0) + 300, Ordering::Release);
            DRV_STEP.store(11, Ordering::Release);
        }
        // --- Speichern: einfrieren, lesen, klassifizieren, schreiben ---------------------------
        11 => {
            use system::Freeze;
            let raw = WORKER_TID0.load(Ordering::Acquire);
            let Some(tid) = (raw != 0).then(|| caprock_sched::ThreadId::from_raw(raw)) else {
                CKPT_STATE.store(CKPT_ERROR, Ordering::Release);
                CKPT_REQ.store(3, Ordering::Release);
                CKPT_DONE.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
                return;
            };
            let abbruch = |tid, tauen: bool| {
                if tauen {
                    let _ = system::thaw_thread(tid);
                }
                CKPT_STATE.store(CKPT_ERROR, Ordering::Release);
                CKPT_REQ.store(3, Ordering::Release);
                CKPT_DONE.store(true, Ordering::Release);
                DRV_STEP.store(3, Ordering::Release);
            };
            match system::freeze_thread(tid) {
                Freeze::Frozen => {}
                Freeze::StillRunning
                    if hal::timer::ticks(0) < CKPT_FREEZE_DEADLINE.load(Ordering::Acquire) =>
                {
                    return;
                }
                _ => {
                    abbruch(tid, false);
                    return;
                }
            }
            CKPT_FROZEN.store(true, Ordering::Release);
            // Der Fortschritt wird am STEHENDEN Thread gelesen: ein Wert, der waehrend des Lesens
            // weiterlaeuft, gehoert zu keinem Zeitpunkt.
            let p = WORKER_ROUNDS[0].load(Ordering::Relaxed);
            // Die Nonce muss dieser Lauf ERZEUGT haben. Der Zyklenzaehler beim Einfrieren ist der
            // billigste Wert, den kein Rest in einem Puffer haben kann.
            let nonce = hal::timer::cycles() | 1;
            let epoche = CKPT_EPOCH.load(Ordering::Acquire) + 1;
            let kh = crate::loader::kernel_code_hash();
            let pd = CKPT_PD.load(Ordering::Acquire);
            let mut kinds = [None; 16];
            let n = if pd == 0 {
                0
            } else {
                system::pd_object_kinds(pd as usize - 1, &mut kinds)
            };
            // **Der Umfang nennt sein Subjekt** (Z4d Stufe 1). Bis 2026-08-25 stand hier ein
            // reines `Scope::EMPTY` und das wandernde Subjekt war ein **stillschweigendes**
            // Mitglied ("nichts wandert mit ausser dem Thread selbst") -- ein implizites Mitglied
            // laesst die Schnittregel gar nicht erst formulieren, denn sie ist eine Aequivalenz
            // ueber "wandert mit".
            let subject = tid.to_raw();
            let scope = caprock_cap::checkpoint::Scope {
                threads: core::slice::from_ref(&subject),
                ..caprock_cap::checkpoint::Scope::EMPTY
            };
            // **Die Kanten werden ERHOBEN, nicht behauptet.** Ein leerer Schnitt und ein
            // ungemessener Schnitt sehen im Urteil gleich aus; die Zahl steht deshalb in der
            // Berichtszeile, und der `ckptcut`-Pruefer weist an einer bekannten Beziehung nach,
            // dass dieser Sammler ueberhaupt etwas findet.
            let mut kanten = [caprock_cap::checkpoint::Edge {
                channel: caprock_cap::checkpoint::Channel::Endpoint(0),
                thread: 0,
                role: caprock_cap::checkpoint::EdgeRole::Sender,
            }; system::CUT_EDGES_MAX];
            let kn = match system::cut_edges(tid, &scope, &mut kanten) {
                Ok(k) => k,
                // Mehr offene Beziehungen, als der Befund fasst. **Kein gekuerzter Schnitt** --
                // eine gekuerzte Kantenliste IST ein zurueckgelassener Partner.
                Err(gebraucht) => {
                    CKPT_CUT_EDGES.store(gebraucht as u32, Ordering::Release);
                    abbruch(tid, true);
                    return;
                }
            };
            CKPT_CUT_EDGES.store(kn as u32, Ordering::Release);
            match caprock_cap::checkpoint::Image::build(
                kh,
                p,
                nonce,
                epoche,
                &kinds[..n],
                &scope,
                subject,
                &kanten[..kn],
            ) {
                Ok(img) => {
                    let Some((shared, _)) = system::driver_shared_region(TEST_BLK_SERVICE_ID)
                    else {
                        abbruch(tid, true);
                        return;
                    };
                    let mut sek = [0u8; 512];
                    let len = img.encode(&mut sek).unwrap_or(0);
                    CKPT_BYTES.store(len as u32, Ordering::Release);
                    CKPT_CAPS.store(img.cap_count as u32, Ordering::Release);
                    CKPT_PRECOND.store(img.precondition_bits, Ordering::Release);
                    CKPT_S_PROGRESS.store(p, Ordering::Release);
                    CKPT_S_NONCE.store(nonce, Ordering::Release);
                    CKPT_S_EPOCH.store(epoche, Ordering::Release);
                    // SAFETY: wie beim Lesen -- identity-gemappte Uebertragungsflaeche, 512 Byte.
                    unsafe {
                        core::ptr::copy_nonoverlapping(sek.as_ptr(), shared as *mut u8, 512)
                    };
                    CKPT_REQ.store(2, Ordering::Release);
                    DRV_STEP.store(12, Ordering::Release);
                }
                Err(refusal) => {
                    // Das Subjekt selbst haelt etwas, das nicht mitwandern darf -> kein
                    // Checkpoint. Der Slot kommt mit; „irgendeine Cap" ist als Diagnose wertlos.
                    //
                    // **Cap-Grund und Schnitt-Grund bleiben getrennt** (Z4d Stufe 1): der erste
                    // wird durch Warten nie besser, der zweite kann sich in einem Tick von selbst
                    // aufloesen. Zusammengelegt hiesse das, jemanden auf etwas warten zu lassen,
                    // das sich nicht bewegt.
                    use caprock_cap::checkpoint::BuildRefusal as B;
                    let (wo, code) = match refusal {
                        B::Cap(slot, r) => (slot as u32, ckpt_reason(r)),
                        B::Cut(i, r) => (i as u32, ckpt_cut_reason(r)),
                    };
                    CKPT_REFUSED[0].store(wo, Ordering::Release);
                    CKPT_REFUSED[1].store(code, Ordering::Release);
                    CKPT_STATE.store(CKPT_REJECTED, Ordering::Release);
                    let _ = system::thaw_thread(tid);
                    CKPT_REQ.store(3, Ordering::Release);
                    CKPT_DONE.store(true, Ordering::Release);
                    DRV_STEP.store(3, Ordering::Release);
                }
            }
        }
        12 => {
            if CKPT_ACK.load(Ordering::Acquire) < 2 {
                return;
            }
            let raw = WORKER_TID0.load(Ordering::Acquire);
            if raw != 0 {
                let tid = caprock_sched::ThreadId::from_raw(raw);
                // **Auftauen gehoert zum Speichern.** Ein Checkpoint, der sein Subjekt stehen
                // laesst, ist ein Abbruch mit Nebenwirkung -- und `freeze_bericht` weiter unten
                // braucht einen laufenden Thread als Positivkontrolle.
                let _ = system::thaw_thread(tid);
            }
            CKPT_REQ.store(3, Ordering::Release);
            CKPT_DONE.store(true, Ordering::Release);
            DRV_STEP.store(3, Ordering::Release);
        }
        // --- A-5.4: der Kreuz-DMA-Nachweis ----------------------------------------------------
        //
        // Erst hier, ganz am Ende: der Fremdversuch loest absichtlich VT-d-Faults aus, und die
        // duerfen keiner frueheren Messung in die Zahlen laufen.
        3 => {
            let Some(net) = crate::loader::driver_service_of(TEST_NET_SERVICE_ID) else {
                // Kein zweiter Treiber -> nicht anwendbar. **Nicht** bestanden: die Zeile sagt
                // das gleich selbst, und `all_done()` haengt an DMAISO_OK.
                DMAISO_OK.store(true, Ordering::Release);
                DRV_STEP.store(9, Ordering::Release);
                return;
            };
            if system::notification_pending(net.ntfn) & crate::loader::DRIVER_NTFN_BADGE == 0 {
                return; // die Netz-PD hat ihr Geraet noch nicht aufgeloest
            }
            let Some((victim_phys, _)) = system::driver_assignment(TEST_BLK_PROGRAM_ID) else {
                DMAISO_OK.store(true, Ordering::Release);
                DRV_STEP.store(9, Ordering::Release);
                return;
            };
            // Die **Gerätesicht** der fremden Region -- nicht ihre Physadresse. Ein Deskriptor
            // traegt IOVAs; die PA dort einzutragen waere ein anderer Fehler und ein anderer Test.
            let mut fenster = [(0u32, 0u32, 0u64, 0u64); system::MAX_OFFERED_DEVICES];
            let n = system::driver_windows(&mut fenster);
            let Some(&(_, _, fremd_iova, _)) =
                fenster.iter().take(n).find(|w| w.0 == TEST_BLK_PROGRAM_ID)
            else {
                DMAISO_OK.store(true, Ordering::Release);
                DRV_STEP.store(9, Ordering::Release);
                return;
            };
            // Der **unabhaengige Zeuge**: das erste Wort der fremden Region vor dem Versuch. Dass
            // der Treiber "nichts bekommen" meldet, ist eine Aussage des Angreifers ueber sich
            // selbst; dass beim Opfer nichts ankam, sieht nur, wer beim Opfer nachsieht.
            // SAFETY: identity-gemappte, vom Kernel ausgeschnittene DMA-Region; nur lesend.
            let vorher = unsafe { core::ptr::read_volatile(victim_phys as *const u64) };
            NET_VICTIM[0].store(vorher, Ordering::Release);
            NET_FOREIGN_IOVA.store(fremd_iova, Ordering::Release);
            let _ = hal::iommu::drain_faults(); // Zaehler auf null, s. o.

            let Ok(ep_cap) = system::install_endpoint_cap(net.ep as u32, Rights::WRITE) else {
                return;
            };
            let Some(cli_pd) = system::create_pd() else { return };
            if !system::install_pd_cap(cli_pd, 0, ep_cap) {
                return;
            }
            let Some(cli) = system::spawn_parked(net_client as *const () as usize, 0, system::IDLE_PRIO)
            else {
                return;
            };
            let _ = system::admit_in_pd(cli_pd, cli);
            DRV_STEP.store(4, Ordering::Release);
        }
        4 => {
            if !NET_DONE.load(Ordering::Acquire) {
                return;
            }
            let victim_phys = system::driver_assignment(TEST_BLK_PROGRAM_ID).map(|(p, _)| p);
            // SAFETY: wie oben.
            let nachher =
                victim_phys.map_or(0, |p| unsafe { core::ptr::read_volatile(p as *const u64) });
            NET_VICTIM[1].store(nachher, Ordering::Release);
            NET_FAULTS.store(hal::iommu::drain_faults(), Ordering::Release);
            // **Das Urteil faellt HIER, nicht im Bericht.** `DMAISO_OK` steht in `all_done()` --
            // was der Bericht setzt, kann den Bericht nicht ausloesen. Genau das ist in diesem
            // Projekt schon zweimal an einem Tag passiert (A-6.1): der Lauf lief in den Watchdog
            // und druckte das Ergebnis trotzdem, und im Log sah das aus wie "gruen, aber gehangen".
            let f = |a: &[core::sync::atomic::AtomicU64; 4], i: usize| a[i].load(Ordering::Acquire);
            // 1. **Positivkontrolle**: derselbe Treiber, dasselbe Geraet, dieselbe Kette -- nur
            //    die eine Adresse ist die eigene. Ohne sie belegt der Rest nichts.
            let self_ok = f(&NET_SELF, 0) == 1 && f(&NET_SELF, 3) == 1;
            // 2. Der Fremdversuch liefert **keine Daten**.
            //
            //    Bewusst NICHT ueber `rx_used`. Das Bit kommt aus dem used-Ring und sagt, dass das
            //    Geraet den Deskriptor ABGEARBEITET hat -- gemessen ist es auch dann 1, wenn die
            //    Schreibung an der IOMMU scheiterte (QEMU legt den Puffer trotzdem zurueck). "Das
            //    Geraet hat gehandelt" und "die Daten sind angekommen" sind zwei verschiedene
            //    Aussagen, und nur die zweite gehoert hierher.
            let fremd_daten = f(&NET_FOREIGN, 3) == 1;
            // 3. Das **Opfer** ist unberuehrt -- am Ort, an den der Rahmen gegangen waere
            //    (`rx_buf_dev + 0`, also die Basis der fremden Region). Das prueft der Kernel
            //    selbst nach; die Meldung des Angreifers ist eine Aussage ueber ihn.
            let opfer_unberuehrt = NET_VICTIM[0].load(Ordering::Acquire) == nachher;
            // 4. Und ein **aktiver** Beleg, dass geblockt wurde: mindestens ein VT-d-Fault. Ohne
            //    ihn waere "keine Antwort" auch mit einem stummen Gegenueber vereinbar -- dann
            //    haette der Test nichts ueber die Isolierung gesagt, sondern ueber das Netz.
            let geblockt = NET_FAULTS.load(Ordering::Acquire) > 0;
            if self_ok {
                DMAISO_STATE.store(2, Ordering::Release);
                DMAISO_OK.store(!fremd_daten && opfer_unberuehrt && geblockt, Ordering::Release);
            } else {
                // Nicht messbar -- und deshalb kein Urteil. `all_done()` darf daran nicht
                // haengenbleiben, sonst wuerde aus "konnte nicht messen" ein Watchdog.
                DMAISO_STATE.store(1, Ordering::Release);
                DMAISO_OK.store(true, Ordering::Release);
            }
            DRV_STEP.store(9, Ordering::Release);
        }
        _ => {}
    }
}


// --- A-5.4: der Kreuz-DMA-Nachweis -------------------------------------------------------------
//
// Die Aussage, um die es geht: **das Geraet des einen Treibers kann nicht in die DMA-Region des
// anderen schreiben.** Bis A-5.3 war sie unpruefbar, weil immer nur eine Treiber-PD lief -- der
// Fall, der sie widerlegen koennte, kam gar nicht vor. Dieselbe Form wie `virtio-rng` vor A-5.2.
//
// Gemessen wird mit ZWEI Anfragen an dieselbe Netz-PD, und die erste ist die wichtigere:
//
//   1. `OP_SELF`    -- eine echte ARP-Transaktion in der EIGENEN Region. **Positivkontrolle.**
//                      Ohne sie hiesse "der Fremdzugriff kam nicht an" nur, dass nichts lief.
//   2. `OP_FOREIGN` -- derselbe Ablauf, aber der Empfangspuffer liegt unter der Gerätesicht des
//                      BLOCK-Treibers. Genau eine Adresse wandert; Ringe und Sendepuffer bleiben
//                      in der eigenen Region, damit das Geraet weiter laufen KANN.
//
// Warum die Netz-PD die fremde IOVA ueberhaupt genannt bekommt: eine Adresse zu **kennen** hilft
// nicht, wenn der Uebersetzungskontext des Geraets sie nicht aufloest -- und genau das ist die
// Messung. Ein Angreifer, der die Adresse raten muesste, bewiese nur, dass Raten schwer ist.
const OP_NET_SELF: u64 = 1;
const OP_NET_FOREIGN: u64 = 2;
/// `program_id` der Netz-PD im Manifest der Lade-Suite (s. `TEST_BLK_SERVICE_ID`).
const TEST_NET_SERVICE_ID: u32 = 5;
/// Ergebnisse: `[features_ok, tx_used, rx_used, arp_reply]` je Anfrage.
static NET_SELF: [core::sync::atomic::AtomicU64; 4] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; 4];
static NET_FOREIGN: [core::sync::atomic::AtomicU64; 4] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; 4];
/// Die fremde Gerätesicht, die dem Netztreiber genannt wird — vom Kernel gesetzt, bevor der
/// Client laeuft.
static NET_FOREIGN_IOVA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// VT-d-Faults, die waehrend des Fremdversuchs anfielen.
static NET_FAULTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Das erste Wort der fremden Region vor und nach dem Versuch — der **unabhaengige** Zeuge.
static NET_VICTIM: [core::sync::atomic::AtomicU64; 2] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 2];
static NET_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DMAISO_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// **Z6 stage 1 verdict** — set once at bring-up, after the AP loop.
///
/// Default `false`, because the line has to be *reached* to be green: a run that dies before the
/// AP loop must not gate through on a flag that was never written. The same reason `IOHEALTH_OK`
/// below starts `false`.
static SMT_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// **Z8/N1 verdict** — set once after the AP loop, for the same reason as [`SMT_OK`].
static NUMA_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Wie das Ergebnis zu lesen ist: 0 = nicht gelaufen, 1 = **nicht messbar** (die Positivkontrolle
/// trug nicht), 2 = geurteilt.
///
/// Der Unterschied ist nicht kosmetisch. Traegt die Positivkontrolle nicht -- weil kein Gegenueber
/// antwortet, weil die Netzkarte fehlt --, dann belegt "der Fremdzugriff kam nicht an" **nichts**:
/// es kam ueberhaupt nichts an. Das als FAILURES zu melden zeigte auf die Isolierung statt auf den
/// Aufbau; als ALL PASS zu melden waere schlimmer. Also ein eigener Ausgang.
static DMAISO_STATE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Client-Thread der Netz-PD: Positivkontrolle, dann der Fremdversuch.
#[cfg(feature = "selftest")]
extern "C" fn net_client(_arg: usize) -> ! {
    let store = |slot: &[core::sync::atomic::AtomicU64; 4], r: caprock_hal::syscall::Ret| {
        for (a, v) in slot.iter().zip(r.msg) {
            a.store(if r.result == result::OK { v } else { u64::MAX }, Ordering::Release);
        }
    };
    store(&NET_SELF, invoke(sys::CALL, 0, [OP_NET_SELF, 0, 0, 0], 0));
    let fremd = NET_FOREIGN_IOVA.load(Ordering::Acquire);
    store(&NET_FOREIGN, invoke(sys::CALL, 0, [OP_NET_FOREIGN, fremd, 0, 0], 0));
    NET_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::YIELD, 0, [0; 4], 0);
    }
}

/// Badge, mit dem der Treiber "ich bin bereit" meldet (`B_READY` in seinem Quelltext).
const DRV_READY_BADGE: u64 = crate::loader::DRIVER_NTFN_BADGE;

/// Client-Thread: fragt den Treiber-Dienst, wartet auf den Austausch, fragt noch einmal.
#[cfg(feature = "selftest")]
extern "C" fn drv_client(_arg: usize) -> ! {
    let store = |slot: &[core::sync::atomic::AtomicU64; 4], m: [u64; 4]| {
        for (a, v) in slot.iter().zip(m) {
            a.store(v, Ordering::Release);
        }
    };
    let r1 = invoke(sys::CALL, 0, [OP_READ, MAGIC_LBA, 1, 0], 0);
    store(&DRV_R1, [r1.result, r1.msg[1], r1.msg[2], r1.msg[3]]);
    // `msg[0]` traegt den Dienststatus, `result` den der IPC. Beide gehoeren in die Bewertung:
    // eine geglueckte IPC mit Fehlerstatus ist etwas anderes als eine gescheiterte IPC.
    DRV_R1[0].store(if r1.result == result::OK { r1.msg[0] } else { u64::MAX }, Ordering::Release);
    DRV_CALL1_DONE.store(true, Ordering::Release);
    while !DRV_SWAPPED.load(Ordering::Acquire) {
        invoke(sys::YIELD, 0, [0; 4], 0);
    }
    let r2 = invoke(sys::CALL, 0, [OP_READ, MAGIC_LBA, 1, 0], 0);
    store(&DRV_R2, [r2.result, r2.msg[1], r2.msg[2], r2.msg[3]]);
    DRV_R2[0].store(if r2.result == result::OK { r2.msg[0] } else { u64::MAX }, Ordering::Release);

    // --- A-6.1: die volle Protokollfolge, an der NEUEN Fassung ------------------------------
    //
    // Sie laeuft nach dem Austausch, und das ist kein Zufall: was hier durchgeht, geht durch
    // einen Dienst, der eben ausgetauscht wurde. Ein Protokoll, das nur die erste Fassung
    // bedient, waere kein Dienst, sondern ein Startvorgang.
    let st = |r: caprock_hal::syscall::Ret| if r.result == result::OK { r.msg[0] } else { u64::MAX };
    // 1. Auskunft: Kapazitaet, Hoechstzahl je Anfrage, Sektorgroesse.
    let info = invoke(sys::CALL, 0, [OP_INFO, 0, 0, 0], 0);
    DRV_SEQ[0].store(st(info), Ordering::Release);
    DRV_SEQ[1].store(info.msg[1], Ordering::Release); // Kapazitaet
    DRV_SEQ[2].store(info.msg[3], Ordering::Release); // Sektorgroesse
    // 2. Den ersten Sektor der ersten Partition lesen -- dort steht die Magie.
    let rd0 = invoke(sys::CALL, 0, [OP_READ, MAGIC_LBA, 1, 0], 0);
    DRV_SEQ[3].store(st(rd0), Ordering::Release);
    // 3. Denselben Puffer auf einen ANDEREN Sektor schreiben. Das ist der Schreibpfad mit
    //    ECHTEN Daten -- kein erfundenes Muster, sondern was gerade von der Platte kam.
    let wr = invoke(sys::CALL, 0, [OP_WRITE, PROBE_SECTOR, 1, 0], 0);
    DRV_SEQ[4].store(st(wr), Ordering::Release);
    // 4. Dauerhaft machen. Ohne Flush ist "geschrieben" eine Aussage ueber einen Puffer.
    let fl = invoke(sys::CALL, 0, [OP_FLUSH, 0, 0, 0], 0);
    DRV_SEQ[5].store(st(fl), Ordering::Release);
    // 5. **Die Partitionstabelle lesen** (A-6.2). Sie laeuft VOR dem Rueckleseschritt, damit der
    //    Puffer des Treibers am Ende die Magie enthaelt und nicht die Eintragsliste -- der
    //    Kernel prueft ihn (`drv`), und ein Test, der seine eigene Vorbedingung zerstoert,
    //    meldet einen Fehler, den es nicht gibt.
    let sc = invoke(sys::CALL, 0, [OP_SCAN, 0, 0, 0], 0);
    DRV_SEQ[8].store(st(sc), Ordering::Release);
    DRV_SEQ[9].store(sc.msg[1], Ordering::Release);  // belegte Eintraege
    DRV_SEQ[10].store(sc.msg[2], Ordering::Release); // erste LBA
    DRV_SEQ[11].store(sc.msg[3], Ordering::Release); // Sektorzahl
    // 6. Zurueck lesen. Erst DAS belegt, dass geschrieben wurde -- ein quittiertes Schreiben
    //    ist eine Quittung, keine Daten.
    let rb = invoke(sys::CALL, 0, [OP_READ, PROBE_SECTOR, 1, 0], 0);
    DRV_SEQ[6].store(if st(rb) == 0 { rb.msg[1] } else { u64::MAX }, Ordering::Release);
    // 7. Und der Fall, der abgewiesen gehoert: ein Sektor jenseits der Platte. Ein Dienst, der
    //    nur den gueltigen Fall kann, hat keine Bereichspruefung -- er hatte bloss Glueck.
    let oob = invoke(sys::CALL, 0, [OP_READ, info.msg[1], 1, 0], 0);
    DRV_SEQ[7].store(st(oob), Ordering::Release);

    DRV_CALL2_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

// --- Z4 Stufe 2: ein Thread ueber die BOOTGRENZE ------------------------------------------------
//
// Z4a haelt einen Thread an, Z4b sagt, was mitwandern darf. Was fehlte, war die Grenze, ueber die
// etwas wandert. Hier ist es die **Maschinengrenze in der Zeit**: derselbe Rechner, zwei Laeufe,
// dazwischen ein Neustart. Alles, was der Kernel im RAM haelt, ist weg; was bleibt, ist die Platte.
//
// **Derselbe Kernel macht beides und entscheidet selbst welches.** Kein Flag, keine zweite
// Konfiguration: er liest den Sektor und sieht, was dort steht. Ein Schalter waere bequemer und
// waere zugleich der Fehler -- ein Kernel, dem man sagen muss, ob er wiederherstellen soll, kann
// nicht merken, dass der Checkpoint gar nicht zu ihm gehoert.
//
// Vier Ausgaenge, und der vierte ist der wichtigste:
//
//   1. **Kein Checkpoint** (leerer Sektor) -> Lauf 1: einfrieren, speichern.
//   2. **Passt alles**                     -> Lauf 2: wiederherstellen.
//   3. **Struktur kaputt**                 -> abweisen, mit Code.
//   4. **Magie ja, Kernel-Hash NEIN**      -> ABWEISEN. Das ist Z4f in klein: ein Zustand, der in
//      eine Umgebung mit anderen Zusicherungen wandert, ist schlimmer als einer, der gar nicht
//      wandert -- denn niemand merkt es.
//
// **Der Transport ist der, den es schon gibt.** Der Blockdienst (A-6.1) nimmt `OP_WRITE` und
// kopiert die geteilte Uebertragungsflaeche auf die Platte; der Kernel legt seine Bytes hinein und
// ruft wie jeder andere Client. Ein eigener Speicherpfad im Kern waere ein zweiter Treiber und
// genau das, was A-5.1 abgeschafft hat.

/// **Der Sektor, auf dem der Checkpoint liegt.**
///
/// AUSSERHALB beider Partitionen und ausserhalb der GPT-Sicherungskopie: `tools/mkgpt.py` legt
/// Partition 1 auf 34..20000, Partition 2 auf 20001..32700, die Sicherungs-Eintraege auf
/// 32735..32766 und den Sicherungskopf auf 32767. Frei bleibt 32701..32734.
///
/// Das ist keine Kosmetik: die A-6-Pruefungen lesen GPT und FAT16 aus demselben Abbild, und
/// `tools/checkfat.py` liest es unabhaengig nach. Ein Checkpoint in einer Partition machte aus
/// einer bestandenen Dateisystempruefung eine, die zufaellig noch durchgeht.
const CKPT_SECTOR: u64 = 32710;

/// Was der Lauf mit dem Checkpoint gemacht hat.
const CKPT_SKIP: u32 = 0;
const CKPT_SAVED: u32 = 1;
const CKPT_RESTORED: u32 = 2;
const CKPT_REJECTED: u32 = 3;
const CKPT_ERROR: u32 = 4;

/// Die PD des Checkpoint-Subjekts (+1; 0 = keine). Ihr Cspace ist der **Umfang** (Z4b).
#[cfg(feature = "selftest")]
static CKPT_PD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "selftest")]
static CKPT_STATE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(CKPT_SKIP);
/// Ist die Folge durch? **Ein Fertig-Merker, kein Urteil** — er steht in `all_done()`, damit der
/// Bericht nicht vor der Platten-E/A kommt. Das Urteil steht dort ausdruecklich NICHT (s.
/// `ckpt_bericht`).
#[cfg(feature = "selftest")]
static CKPT_DONE: AtomicBool = AtomicBool::new(false);
/// **Der GEFUNDENE Zustand** (nur im Wiederherstellungsfall): Fortschritt, Nonce, Epoche.
#[cfg(feature = "selftest")]
static CKPT_PROGRESS: AtomicU64 = AtomicU64::new(0);
/// Ein Wert, den der schreibende Lauf **erst erzeugt** hat (Zyklenzaehler beim Einfrieren).
///
/// Ohne ihn waere „Lauf 2 fand den Wert aus Lauf 1" nicht von „im Puffer standen zufaellig die
/// richtigen Bytes" zu unterscheiden.
#[cfg(feature = "selftest")]
static CKPT_NONCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static CKPT_EPOCH: AtomicU64 = AtomicU64::new(0);
/// **Der GESCHRIEBENE Zustand.** Jeder Lauf, der durchkommt, schreibt einen -- auch der, der
/// gerade wiederhergestellt hat.
///
/// **Warum ueberhaupt:** der Fortschritt allein belegt nicht, dass er geerbt wurde. Gemessen
/// (2026-08-02, KVM): der Kernel erreicht an derselben Stelle des Hochlaufs 133, 148 und 151
/// Runden -- eine Streuung von rund 18 -- und ein Mutationslauf, in dem gar nichts
/// wiederhergestellt wurde, traf den gespeicherten Wert **exakt**. Ein reproduzierbarer Wert kann
/// nicht belegen, dass er von woanders kam.
///
/// Deshalb waechst der Zustand ueber die Kette: jeder Lauf laesst den wiederhergestellten Thread
/// noch **mindestens** [`CKPT_MIN_DELTA`] Runden arbeiten und speichert erst dann. Ab dem zweiten
/// Glied liegt der gespeicherte Wert damit strukturell ausserhalb dessen, was ein Lauf ohne
/// Wiederherstellung erreicht -- und die Frage „geerbt oder selbst gezaehlt?" ist entscheidbar.
#[cfg(feature = "selftest")]
static CKPT_S_PROGRESS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static CKPT_S_NONCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static CKPT_S_EPOCH: AtomicU64 = AtomicU64::new(0);
/// Um so viele Runden muss der Thread nach dem Wiederherstellen weitergekommen sein, bevor
/// gespeichert wird. Rund das Fuenffache der gemessenen Lauf-zu-Lauf-Streuung.
#[cfg(feature = "selftest")]
const CKPT_MIN_DELTA: u64 = 100;
/// Hat der Thread den Zuwachs wirklich erreicht — oder lief nur die Notschranke ab?
#[cfg(feature = "selftest")]
static CKPT_DELTA_OK: AtomicBool = AtomicBool::new(false);
/// Der Zaehlerstand VOR dem Wiederherstellen — die Positivkontrolle der Suite: war ueberhaupt
/// etwas zu messen, oder waren gefundener und eigener Wert zufaellig gleich?
#[cfg(feature = "selftest")]
static CKPT_BEFORE: AtomicU64 = AtomicU64::new(0);
/// Zaehlerstand beim Beginn des Wartens + Tick-Notschranke dafuer.
#[cfg(feature = "selftest")]
static CKPT_WAIT_BASE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "selftest")]
static CKPT_WAIT_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Diagnosecode des Lesens (s. `ckpt_code`).
#[cfg(feature = "selftest")]
static CKPT_DECODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Klassifizierte Caps und ihre Vorbedingungen (Z4b im Erzeugnis).
#[cfg(feature = "selftest")]
static CKPT_CAPS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
#[cfg(feature = "selftest")]
static CKPT_PRECOND: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Laenge des geschriebenen Checkpoints in Byte.
#[cfg(feature = "selftest")]
static CKPT_BYTES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// **Die Negativkontrolle, die in JEDEM Lauf mitlaeuft:** `(Slot, Grund)` der Cap, an der das
/// Speichern der TREIBER-PD scheitert. `u32::MAX` = nicht gelaufen.
#[cfg(feature = "selftest")]
static CKPT_REFUSED: [core::sync::atomic::AtomicU32; 2] =
    [const { core::sync::atomic::AtomicU32::new(u32::MAX) }; 2];
/// **Wie viele IPC-Kanten der Schnitt gesehen hat** (Z4d Stufe 1) — `u32::MAX` = nicht erhoben.
///
/// Die Zahl steht in der Berichtszeile, weil ein sauberer Schnitt und ein **ungemessener** Schnitt
/// dasselbe Urteil ergeben. Sie ist hier bauartbedingt 0 (das Subjekt ist ein reiner Rechenthread
/// ohne jede IPC-Rolle) — und genau deshalb hat `bootckpt` von dieser Regel keine Trennschaerfe:
/// dass der Sammler ueberhaupt etwas findet, weist die `ckptcut`-Zeile an einer bekannten
/// Beziehung nach, nicht diese hier.
#[cfg(feature = "selftest")]
static CKPT_CUT_EDGES: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(u32::MAX);
/// Wurde das Subjekt wirklich eingefroren, bevor sein Zustand angefasst wurde?
#[cfg(feature = "selftest")]
static CKPT_FROZEN: AtomicBool = AtomicBool::new(false);
/// Der Zaehlerstand unmittelbar NACH dem Anfassen — die Vergleichsbasis fuer „er laeuft weiter".
#[cfg(feature = "selftest")]
static CKPT_AFTER: AtomicU64 = AtomicU64::new(0);
/// Statuszeilen der Platten-E/A: `[READ, WRITE, FLUSH]`.
#[cfg(feature = "selftest")]
static CKPT_IO: [AtomicU64; 3] = [const { AtomicU64::new(u64::MAX) }; 3];
/// Auftrag an den Checkpoint-Client (0 = nichts, 1 = lesen, 2 = schreiben+flush, 3 = enden).
#[cfg(feature = "selftest")]
static CKPT_REQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Was der Client davon erledigt hat.
#[cfg(feature = "selftest")]
static CKPT_ACK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Tick-Schranke fuer das Einfrieren (`StillRunning` ist voruebergehend — aber nicht ewig).
#[cfg(feature = "selftest")]
static CKPT_FREEZE_DEADLINE: AtomicU64 = AtomicU64::new(0);

/// **Client-Thread des Checkpoints.** Er tut genau das, was jeder andere Client tut: rufen.
///
/// Er laeuft als eigener Faden und nicht in der Zustandsmaschine, weil `CALL` **blockiert** — die
/// Hauptschleife des Hochlaufs darf das nicht. Die Auftraege kommen ueber `CKPT_REQ`, damit der
/// Kernel zwischen Lesen und Schreiben entscheiden kann, was ueberhaupt geschrieben wird.
#[cfg(feature = "selftest")]
extern "C" fn ckpt_client(_arg: usize) -> ! {
    let st = |r: caprock_hal::syscall::Ret| if r.result == result::OK { r.msg[0] } else { u64::MAX };
    loop {
        let req = CKPT_REQ.load(Ordering::Acquire);
        let ack = CKPT_ACK.load(Ordering::Acquire);
        if req == 3 {
            break;
        }
        if req == 1 && ack < 1 {
            CKPT_IO[0].store(st(invoke(sys::CALL, 0, [OP_READ, CKPT_SECTOR, 1, 0], 0)), Ordering::Release);
            CKPT_ACK.store(1, Ordering::Release);
        } else if req == 2 && ack < 2 {
            CKPT_IO[1].store(st(invoke(sys::CALL, 0, [OP_WRITE, CKPT_SECTOR, 1, 0], 0)), Ordering::Release);
            // **Ohne Flush ist „geschrieben" eine Aussage ueber einen Puffer.** Ueber eine
            // Bootgrenze ist das der ganze Unterschied: was nur im Geraetecache steht, ist beim
            // naechsten Start nicht da.
            CKPT_IO[2].store(st(invoke(sys::CALL, 0, [OP_FLUSH, 0, 0, 0], 0)), Ordering::Release);
            CKPT_ACK.store(2, Ordering::Release);
        } else {
            invoke(sys::YIELD, 0, [0; 4], 0);
        }
    }
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Diagnosecode fuer den Ausgang des Lesens — eine Zahl je Ursache, kein Sammel-Fehlschlag.
#[cfg(feature = "selftest")]
fn ckpt_code(e: &caprock_cap::checkpoint::ImageError) -> u32 {
    use caprock_cap::checkpoint::ImageError as E;
    match e {
        E::NoImage => 1,
        E::Version { .. } => 2,
        E::Truncated => 3,
        E::CapOverflow => 4,
        E::UnknownCapTag { .. } => 5,
        E::Checksum => 6,
        E::ForeignKernel => 7,
        E::BufferTooSmall => 8,
    }
}

/// Grund-Code fuer eine verweigerte Cap (`LocalReason`) — dieselbe Form, aus demselben Grund.
#[cfg(feature = "selftest")]
fn ckpt_reason(r: caprock_cap::checkpoint::LocalReason) -> u32 {
    use caprock_cap::checkpoint::LocalReason as L;
    match r {
        L::DeviceWindow => 1,
        L::InterruptLine => 2,
        L::DmaRegion => 3,
        L::PendingReply => 4,
        L::PeerNotInScope => 5,
        L::ThreadNotInScope => 6,
        L::PdNotInScope => 7,
        L::LoaderSource => 8,
        // Z26/A3, 2026-08-13: eigener Code fuer die Handler-Bindung. Bis dahin trug sie den Code
        // von `PendingReply` -- der Refusal-Grund stimmte, die Cap-Art nicht, und ein falscher
        // Name fuehrt zur falschen Behebung. Dass die Umstellung hier eine Zeile KOSTET, ist die
        // Wirkung des erschoepfenden `match` und nicht sein Preis.
        L::HandlerBinding => 9,
        // Z6b: eigener Code aus demselben Grund wie 9. Debug-Autoritaet wandert nicht mit -- nicht,
        // weil die PD ausserhalb des Umfangs laege (das waere Code 7 und legte die falsche Behebung
        // nahe, „nimm sie in den Umfang auf"), sondern weil die Praegung eine benannte Handlung auf
        // DIESER Maschine war.
        L::DebugAuthority => 10,
        // Z23/S4, 2026-08-21: eigener Code, und die Begruendung ist woertlich die von 9 und 10.
        // „Eine Zahl DIESER Maschine" ist nicht `DeviceWindow`: ein Geraetefenster ist drueben
        // **nicht herstellbar**, eine Kernaffinitaet **entsteht drueben neu**. Derselbe Code liesse
        // jemanden nach einem Geraet suchen, wo eine Zuteilung fehlt.
        L::MachineLocalNumber => 11,
    }
}

/// Codes for the **cut** refusals (Z4d stage 1) — a range of their own, starting at 20.
///
/// Deliberately disjoint from [`ckpt_reason`]'s numbers rather than continuing them: a cap refusal
/// and a cut refusal are answered differently (the first never improves by waiting, the second may
/// dissolve on its own within a tick), and a reader who has to remember where one range ends and
/// the next begins will eventually treat them alike.
fn ckpt_cut_reason(r: caprock_cap::checkpoint::CutRefusal) -> u32 {
    use caprock_cap::checkpoint::CutRefusal as C;
    match r {
        C::ChannelNotInScope => 20,
        C::PeerNotInScope => 21,
    }
}

/// Die Magie, die die Testsuiten in Sektor 0 des Plattenabbilds legen ("CAPROCKS", LE).
///
/// Warum ueberhaupt eine: ein Puffer voller Nullen ist von einem nie beschriebenen Puffer nicht
/// zu unterscheiden, und ein frisches Abbild besteht genau daraus. Ein Test, der nur "das Geraet
/// hat geantwortet" prueft, waere auch dann gruen, wenn der Datenpfad gar nichts uebertraegt.
const BLK_MAGIC: u64 = 0x534B_434F_5250_4143;

/// LBA, auf der die Magie liegt.
///
/// Die Testabbilder sind **echte GPTs** mit ZWEI Partitionen (A-6.2/A-6.3): die erste traegt ein
/// FAT16, die zweite ist roh. Die Magie liegt in der **zweiten** -- in die erste zu schreiben
/// hiesse, den Bootsektor des Dateisystems zu ueberschreiben. Zwei Tests, die sich dieselbe
/// Flaeche teilen, sind ein Riss, durch den beide fallen koennen.
///
/// **Beide** Suiten benutzen dasselbe Abbild-Werkzeug (`tools/mkgpt.py`). Zwei Suiten, die
/// dieselbe Platte verschieden aufsetzen, waeren derselbe Riss wie zwei, die dasselbe Geraet
/// verschieden aufsetzen -- das hat dieses Projekt am 2026-08-01 schon einmal bezahlt.
const MAGIC_LBA: u64 = 20001;

/// Adressen der Testumgebung fuer den ARP-Austausch (QEMU `-netdev user`): der eingebaute
/// Gateway liegt auf 10.0.2.2, der Gast bekommt 10.0.2.15.
///
/// Sie stehen **hier** und nicht im Treiber: `caprock-virtio` bekommt sie hereingereicht. Ein
/// Treiber, der die Adressen seiner Testumgebung kennt, ist keiner mehr -- und genau diese
/// Kenntnis ist das, was mit A-5.1 in die Treiber-PD bzw. deren Manifest wandert.
#[cfg(feature = "selftest")]
const NET_SRC_IP: [u8; 4] = [10, 0, 2, 15];
#[cfg(feature = "selftest")]
const NET_GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Eine vollstaendige virtio-rng-Anfrage auf einer frisch belegten Virtqueue-Region.
///
/// Gibt `(caps_gefunden, used_ring_fortgeschritten, gemeldete_bytes)`. Die Region wird hier
/// belegt und wieder freigegeben -- der Aufrufer soll sich um nichts kuemmern muessen ausser um
/// die Frage, die er stellt.
#[cfg(feature = "selftest")]
fn virtio_versuch(dev: Option<&hal::pcie::PciDevice>, max_poll: u64) -> (bool, bool, u32) {
    let Some(d) = dev else { return (false, false, 0) };
    let Some(cap) = system::alloc(4096, 4096) else { return (false, false, 0) };
    let base = cap.base();
    let r = match hal::virtio::probe(d) {
        // SAFETY: das BAR-Fenster unter 4 GiB ist identity-gemappt (`map_device_block_global`),
        // und die Virtqueue-Region stammt aus dem Allokator, gehoert also exklusiv uns.
        Some(rng) => {
            let (adv, w) = unsafe {
                rng.request_polled(base, base, base + hal::virtio::DATA_OFFSET, max_poll)
            };
            (true, adv, w)
        }
        None => (false, false, 0),
    };
    system::free(cap);
    r
}

/// Eine vollstaendige virtio-blk-Leseanfrage auf einer frisch belegten Region (A-5.2).
///
/// `None` heisst "kein Blockgeraet am Bus" -- ausdruecklich etwas anderes als ein Fehlschlag,
/// s. die Meldung an der Aufrufstelle.
#[cfg(feature = "selftest")]
fn virtio_blk_versuch(
    dev: Option<&hal::pcie::PciDevice>,
    max_poll: u64,
) -> Option<hal::virtio::blk::BlkResult> {
    let d = dev?;
    // **Diese Region sieht ein GERAET** (Bus-Master-DMA, vor dem VT-d-Aufbau als rohe
    // Physadresse). `system::alloc` steht hier als die vorsichtige Wahl -- **nicht**, weil das
    // Geraet oben nicht hinkaeme: gemessen (E-Rest 3d, `-m 3G`, Region oberhalb 4 GiB) liest
    // virtio-blk den Sektor korrekt, `Geraet-DMA=1`. Die Vermutung „das Geraet erreicht nur
    // GiB 0" war also falsch, und sie steht hier, damit sie nicht ein zweites Mal aufkommt.
    // Konservativ bleibt es trotzdem: die 32-Bit-Faehigkeit eines Geraets ist eine Eigenschaft
    // des Geraets, und die Angebotsliste enthaelt sie nicht (s. `dmawin`:
    // `Geraete-ohne-deklarierte-Adressbreite=1`).
    let cap = system::alloc(hal::virtio::blk::REGION_BYTES, 4096)?;
    let base = cap.base();
    let r = hal::virtio::probe_blk(d).map(|blk| {
        // SAFETY: das BAR-Fenster unter 4 GiB ist identity-gemappt (`map_device_block_global`),
        // und die Region stammt aus dem Allokator, gehoert also exklusiv uns.
        //
        // **Nicht Sektor 0.** Seit A-6.2 sind die Testabbilder echte GPTs: auf LBA 0 liegt der
        // schuetzende MBR, auf 1 der Kopf, auf 2..33 die Eintragsliste. Die Magie liegt auf dem
        // ersten Sektor der ersten Partition. Wer hier weiter 0 liest, liest den MBR -- und der
        // Test faellt durch, ohne dass am Geraetepfad irgendetwas fehlt.
        unsafe { blk.read_sector(base, base, MAGIC_LBA, max_poll) }
    });
    system::free(cap);
    r
}

/// Ein vollstaendiger ARP-Austausch ueber virtio-net auf einer frisch belegten Region (A-5.2).
#[cfg(feature = "selftest")]
fn virtio_net_versuch(
    dev: Option<&hal::pcie::PciDevice>,
    max_poll: u64,
) -> Option<hal::virtio::net::NetResult> {
    let d = dev?;
    // Geraete-sichtbar wie in `virtio_blk_versuch` -- s. dort.
    let cap = system::alloc(hal::virtio::net::REGION_BYTES, 4096)?;
    let base = cap.base();
    let r = hal::virtio::probe_net(d).map(|net| {
        // SAFETY: wie in `virtio_blk_versuch`.
        unsafe { net.arp_probe(base, base, NET_SRC_IP, NET_GATEWAY_IP, max_poll) }
    });
    system::free(cap);
    r
}

/// A-4.2: haelt der ruhende Punkt -- weist ein stillgelegter Endpoint neue Transaktionen ab?
#[cfg(feature = "selftest")]
static QUIESCE_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A-4.1: bindet der Austausch atomar um -- ohne Zustand ohne Empfaenger dazwischen?
#[cfg(feature = "selftest")]
static REBIND_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "selftest")]
static EPFULL_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A-4.3: weist die Zustandsuebergabe ein fremdes Layout ab, statt es fehlzuinterpretieren?
#[cfg(feature = "selftest")]
static STATE_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Der akkumulierte Badge der Root-Notification (`0`, wenn keine endowt wurde).
#[cfg(feature = "selftest")]
fn root_badge() -> u64 {
    crate::loader::root_notification()
        .map(system::notification_pending)
        .unwrap_or(0)
}

/// Hat der Root-Task gelaufen **und** von sich aus die uebrige Startmenge geladen?
///
/// Beides muss belegt sein, und zwar getrennt: dass ein geladenes Programm laeuft, sagt noch
/// nichts darueber, ob seine Loader-Cap traegt. Erst das zweite Badge zeigt, dass ein
/// **Userland**-Programm ein weiteres Programm gestartet hat -- das ist die Aussage, wegen der
/// Stufe 1 des Plans existiert.
#[cfg(feature = "selftest")]
fn root_chain_done() -> bool {
    let b = root_badge();
    b & ROOT_BADGE != 0 && b & HELLO_BADGE == HELLO_BADGE
}

/// A-3.1: beide Ausgänge von `SYS_CDELETE` aus Ring 3 belegt — der erfolgreiche **und** der
/// abgelehnte. Ein Löschpfad, von dem nur der Erfolgsfall geprüft ist, sagt nichts darüber, ob er
/// im Zweifel zu viel löscht.
#[cfg(feature = "selftest")]
fn cdelete_done() -> bool {
    let b = root_badge();
    b & CDELETE_GONE_BADGE != 0 && b & CDELETE_CHILDREN_BADGE != 0
}

/// Sind alle Demo-Aussagen belegt? Dazu gehört, dass **jeder** Kern tickt — ein Kern, der
/// zwar bootet, aber keinen Timer-Interrupt bekommt, würde sonst unbemerkt bleiben.
///
/// **`archive` = liegt überhaupt ein Boot-Archiv vor** (B-1.6). Ohne Archiv gibt es keinen
/// Root-Task, also können [`root_chain_done`] und [`cdelete_done`] **prinzipiell** nicht wahr
/// werden — `test-qemu-x86.sh` bootet genau so, absichtlich (das Archiv prüft die Lade-Suite).
/// Standen sie trotzdem in der Bedingung, wurde `all_done()` dort **nie** wahr: der Bericht fiel
/// jedes Mal aus der Notbremse unten, also nach 50 Mio. Spins statt nach dem letzten Beleg. Damit
/// war jede knappe Aussage ein Rennen gegen einen Zähler — beobachtet am `iso`-Test, der bei
/// gleichem Bau mal `2x` faultete und mal `0x`, je nachdem, ob die Probe bis zum Ablauf drankam.
///
/// Eine Aussage, die diese Konfiguration nicht belegen **kann**, darf deshalb nicht dauerhaft
/// *verlangt* werden — sie ist nicht anwendbar. Gemeldet wird sie trotzdem, und zwar mit Grund
/// (`root : FAILURES (NoArchive)`); die Suite nimmt genau das seit B-1.5 ausdrücklich ab. Was
/// hier NICHT passiert: die Anforderung abschwächen, wenn ein Archiv da ist. Dann gilt sie voll.
#[cfg(feature = "selftest")]
fn all_done(
    archive: bool,
    warum: Option<&mut [(&'static str, crate::befund::Befund); DONE_FLAGS]>,
) -> bool {
    let workers = (0..NWORKERS).all(|i| WORKER_ROUNDS[i].load(Ordering::Relaxed) >= WORK_TARGET);
    // Z6 Stufe 1b: nur ONLINE-Kerne muessen ticken (Analogon threads/mod.rs ARM-Seite).
    // Ein unterdruecktes SMT-Geschwister hat Tabellen, aber keinen Timer -- `ticks == 0`
    // ist dort die Politik, kein Defekt. Ohne SMT ist jedes Bit gesetzt und die Bedingung
    // buchstabengleich zur alten.
    let cores = (0..system::num_cores())
        .filter(|&c| caprock_sched::core_online(c))
        .all(|c| hal::timer::ticks(c) > 0);
    let ring3 = USER_SYSCALLS.load(Ordering::Relaxed) > 0 && system::el0_fault_count() > 0;
    let iso = SAS_READ_OK.load(Ordering::Relaxed) == PROBE_MAGIC && system::iso_fault_count() > 0;
    let root = !archive || (root_chain_done() && cdelete_done());
    // B-4.2 gehoert in die Abschlussbedingung, nicht bloss in den Bericht: sonst waere ein
    // Fehlschlag genau die Sorte Zeile, die niemand liest.
    let stripes = STRIPE_ALLOC_OK.load(Ordering::Acquire);
    // B-4.5 in der Abschlussbedingung, aus demselben Grund wie B-4.2 daneben.
    let pprobe = PPROBE_OK.load(Ordering::Acquire);
    let virtio = VIRTIO_OK.load(Ordering::Acquire);
    // A-5.2: `blk` und `net` gehoeren aus demselben Grund hierher wie `virtio` daneben. Sie
    // duerfen dabei SKIPpen (kein Geraet am Bus -> `true`); was sie nicht duerfen, ist
    // durchfallen, ohne dass der Lauf davon abhaengt.
    let vblk = VBLK_OK.load(Ordering::Acquire);
    let vnet = vnet_befund();
    // A-5.1: die Dienst-/Austausch-Abfolge muss DURCH sein, bevor berichtet wird. `DRIVER_OK`
    // taugt dafuer nicht -- es wird IM Bericht gesetzt und koennte ihn deshalb nicht ausloesen.
    // Ohne diese Zeile kam der Bericht, bevor der Austausch ueberhaupt anlief, und meldete
    // "nicht bereit" ueber etwas, das noch gar nicht dran gewesen war.
    let drv_seq = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID).is_none()
        || DRV_CALL2_DONE.load(Ordering::Acquire);
    // Z4 Stufe 2, aus demselben Grund wie `drv_seq` daneben: ein **Fertig-Merker**, kein Urteil.
    // Die Platten-E/A des Checkpoints muss durch sein, bevor berichtet wird -- sonst meldete der
    // Bericht „nicht gelaufen" ueber etwas, das noch gar nicht dran war. Das URTEIL steht
    // ausdruecklich nicht hier (s. `ckpt_bericht`).
    let ckpt_seq = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID).is_none()
        || CKPT_DONE.load(Ordering::Acquire);
    let blkdev = BLKDEV_OK.load(Ordering::Acquire);
    let part = PART_OK.load(Ordering::Acquire);
    // ------------------------------------------------------------------------------------------
    // Z25: EAGER-FP + das VEKTOR-INVENTAR
    // ------------------------------------------------------------------------------------------
    //
    // Der FP-Wechsel haengt nicht mehr am ersten Zugriff, sondern am Wechsel. Damit ist ein `#NM`
    // per Definition ein Kernelfehler -- und diese Zeile ist der Ort, an dem das nachpruefbar
    // wird. Drei Aussagen, und die dritte ist die eigentliche Invariante:
    //
    //  1. Die Sonde traegt ihr Muster ueber Abgaben (dass ueberhaupt gewechselt wird).
    //  2. `CR0.TS` wurde NIE gesetzt vorgefunden (`ts_vorgefunden == 0`) -- die eager-Zusicherung
    //     in beobachtbarer Form. Ein `debug_assert!` waere im Release-Bau weg, und dort laeuft
    //     die Suite.
    //  3. **Kein `#NM`, und zwar JE KERN.** Global summiert waere ein einzelner AP, dessen
    //     `enable_sse` in einem Refactor aus der Reihenfolge rutscht, im Rauschen der uebrigen
    //     unsichtbar -- und das ist die wahrscheinlichste kuenftige Regression.
    //
    // **Das Urteil hing bis zum 2026-08-09 an „alle 64 Abgaben ueberstanden" -- und genau das
    // konnte die Sonde nie liefern.** Gemessen: Fortschritt 4/64 und 7/64 bei gruener
    // Sofortpruefung und **null** gemeldeten Korruptionen. Die Sonde war nicht kaputt, sie war
    // **langsamer als der Bericht**: sie kommt je Runde des Rundlaufs genau eine Iteration voran,
    // und eine Runde ist durch den Tick begrenzt, nicht durch das YIELD. 64 war eine Zahl ohne
    // Bezug zur Rundenlaenge.
    //
    // Ein Kriterium, das die geprüfte Sache nie erreichen kann, ist kein strenges Kriterium,
    // sondern **gar keins** -- es hat keine Trennschaerfe: gruen ist unmoeglich, also sagt rot
    // nichts. Das Urteil liest deshalb jetzt die drei Groessen, die sich wirklich aendern:
    // **Fortschritt**, **Korruptionsmeldung** und die **eigene Restore-Zahl** der Sonde.
    // Die 64 laufen weiter und stehen im Bericht -- als Auskunft, nicht als Bedingung.
    let fpmask = FP_OK.load(Ordering::Relaxed);
    let fp_gelaufen = fpmask & 0b100 != 0;
    let muster_ok = fpmask & 0b11 == 0b11;
    let ts_gesehen = hal::fp::ts_vorgefunden();
    let mut nm_je_kern = [0u64; 8];
    let kerne = system::num_cores().min(8);
    for (c, n) in nm_je_kern.iter_mut().enumerate().take(kerne) {
        *n = system::nm_traps(c);
    }
    let nm_gesamt: u64 = nm_je_kern.iter().take(kerne).sum();
    // Die drei Groessen, die das Urteil jetzt traegt.
    let (f0, f1) = (
        FP_FOUND[0].load(Ordering::Relaxed),
        FP_FOUND[1].load(Ordering::Relaxed),
    );
    let (p0, p1) = (
        FP_PROGRESS[0].load(Ordering::Relaxed),
        FP_PROGRESS[1].load(Ordering::Relaxed),
    );
    let (r0, r1) = (system::fp_watch_restores(0), system::fp_watch_restores(1));
    // **Das Urteil kommt aus `fp_urteil()`, nicht von hier.** Der Bericht druckt die Bestandteile;
    // entschieden wird an EINER Stelle, die auch `all_done` liest.
    let fp_ok = fp_urteil();
    println!(
        "fp      : gelaufen={fp_gelaufen} · Muster ueber ALLE {FP_ITERS} Abgaben erhalten = {:#b} (vollstaendig={muster_ok}) -- **Auskunft, nicht Bedingung**, s. u. · CR0.TS-gesetzt-vorgefunden={ts_gesehen} (eager: muss 0 sein)          (zwei Sonden mit VERSCHIEDENEN Mustern in xmm0..xmm3; verschieden ist der Punkt -- mit          demselben Muster fiele ein fehlendes fxsave nicht auf, weil der Dieb genau das          zuruecklaesst, was das Opfer erwartet)",
        fpmask & 0b11
    );
    // **Die drei Hypothesen trennen** -- ohne den gefundenen Wert sehen sie gleich aus.
    println!(
        "fp      : sofort-ok (OHNE Wechsel) = {:#b} (erwartet 0b11 in Bit 3/4) · gefunden statt \
         des Musters: Sonde0={f0:#018x} Sonde1={f1:#018x}. Nullen = frisch resetteter Slot \
         restauriert · Muster des PARTNERS = Identitaetsvertauschung, der FP-Zustand folgt der \
         CPU statt dem Thread · Muell = Layout/Ausrichtung. 0 in BEIDEN bei gruener \
         Sofortpruefung hiesse: nie eine Korruption erreicht",
        (fpmask >> 3) & 0b11
    );
    println!(
        "fp      : Sofortpruefung -- Restversuche von 1000: Sonde0={} Sonde1={} (1000 = beim \
         ERSTEN Versuch gelungen; kleiner = es gab ein Rennen; 0 zusammen mit sofort-ok=0 heisst: \
         `movq xmm, r64` wirkt hier ueberhaupt nicht, und dann ist die Korruption eine Attrappe)",
        FP_TRIES[0].load(Ordering::Relaxed),
        FP_TRIES[1].load(Ordering::Relaxed)
    );
    println!(
        "fp      : Schleifenfortschritt = Sonde0={p0}/{FP_ITERS} Sonde1={p1}/{FP_ITERS} (gefordert \
         >=2 je Sonde: 1 = einmal geprueft und abgegeben, 2 = nach einer Abgabe NOCH EINMAL \
         geprueft -- und das ist die Aussage). {FP_ITERS} = durchgelaufen, 0 = nie begonnen. \
         **Die Sonde kommt je Rundlauf-Runde genau eine Iteration voran, und eine Runde ist \
         durch den Tick begrenzt, nicht durch das YIELD** -- deshalb ist der volle Durchlauf \
         Auskunft und nicht Bedingung"
    );
    println!(
        "fp      : Verdraengungen DIESER Sonden = Sonde0={r0} Sonde1={r1} (gefordert >=2; der \
         erste Restore laedt einen frisch genullten Slot, also VOR dem Muster) · FP-Wechsel \
         gesamt = {} (Save eines vorigen Besitzers). **Die Gesamtzahl ist NICHT die Sprechprobe**: \
         laegen beide Sonden auf verschiedenen Kernen und verdraengten einander nie, waere sie \
         hoch und die Musterpruefung trotzdem gegenstandslos -- dieselbe Form wie `rx_used` gegen \
         „Daten sind angekommen\"",
        system::fp_switch_count()
    );
    print!("fp      : #NM je Kern =");
    for (c, n) in nm_je_kern.iter().enumerate().take(kerne) {
        print!(" k{c}={n}");
    }
    println!(
        " (Summe {nm_gesamt}, muss 0 sein). Unter eager ist ein #NM per Definition ein          KERNELFEHLER -- gezaehlt je Kern, weil ein einzelner AP ohne `enable_sse` in einer          Summe untergeht"
    );
    println!(
        "fp      : {} (EAGER-FP fuer Ring 3: der Wechsel liegt im WECHSEL, nicht im ersten          Zugriff. Der Grund ist CVE-2018-3665 (LazyFP) -- mit gesetztem CR0.TS kann spekulative          Ausfuehrung die FP-Register des VORIGEN Besitzers lesen, bevor das #NM zugestellt ist.          Ueber eine PD-Grenze ist das ein Isolationsbruch, und seit SSE fuer Userland scharf ist,          kann dort Schluesselmaterial liegen. aarch64 bleibt lazy -- CPACR_EL1.FPEN trappt          praezise und nur EL0; die Begruendung steht im HAL-Vertrag beider Seiten)",
        if fp_ok { "ALL PASS" } else { "FAILURES" }
    );

    // **Die Client-Notification-Bilanz** (2026-08-10). Hier und nicht in `manifest_audit`: das
    // laeuft VOR dem Laden, dort waere die Zahl immer 0 gewesen -- eine Groesse, die zum
    // Messzeitpunkt gar nicht anders sein kann, gattert nichts.
    //
    // Was sie belegt: bis zum 2026-08-10 gab es EINE Zelle fuer die Rolle „Client". Die
    // Behebung von A-6.3 lautete „drei Rollen, drei Badges, drei Ablagen" -- und mit der ZWEITEN
    // Client-PD kam derselbe Fehler eine Ebene hoeher zurueck: `wasmhost` ueberschrieb die Ablage
    // der Dateisystem-PD, der `drv`-Ablauf wartete auf ein Badge an einem fremden Objekt, und
    // `drv`/`blkdev`/`part` fielen aus, ohne dass am Treiber etwas kaputt war.
    let (_clients, _ntfn_verloren) = crate::loader::client_notification_stats();
    {
        // **Die IDs, nicht nur die Zahl.** Eine Id-Kollision (Root und ein Client auf demselben
        // Objekt) erklaerte alles auf einmal: warum das Root-Badge einmal das CLIENT-Bit trug und
        // warum Badges „nicht ankommen". Ohne die Ids ist das nicht entscheidbar.
        let mut _ids = [(0u32, 0usize); 8];
        let _n = crate::loader::client_notification_ids(&mut _ids);
        #[cfg(feature = "debug-log")]
        print!("clientn : Objekt-Ids: root=");
        #[cfg(feature = "debug-log")]
        match crate::loader::root_notification() {
            Some(r) => print!("#{r}"),
            None => print!("keine"),
        }
        #[cfg(feature = "debug-log")]
        print!(" driver=");
        #[cfg(feature = "debug-log")]
        match crate::loader::driver_notification() {
            Some(d) => print!("#{d}"),
            None => print!("keine"),
        }
        #[cfg(feature = "debug-log")]
        for &(pid, id) in _ids.iter().take(_n) {
            print!(" · Programm {pid}=#{id}");
        }
        #[cfg(feature = "debug-log")]
        println!(" (zwei gleiche Zahlen = EIN Objekt fuer zwei Rollen -- dann sind alle Badges an derselben Stelle)");
        // **Das Thread-Register als Ganzes.** Ohne es ist „Programm 6 hat keinen Thread" nicht von
        // „die Registrierung laeuft ueberhaupt nicht" zu unterscheiden -- eine leere Tabelle sieht
        // aus wie ein leerer Befund. Sprechprobe des Registers gegen sich selbst.
        let (_n_reg, _verloren_reg) = crate::loader::program_thread_stats();
        #[cfg(feature = "debug-log")]
        print!("clientn : Thread-Register: {_n_reg} Eintrag/Eintraege, {_verloren_reg} verloren ·");
        for pid in 1..=8u32 {
            if let Some(t) = crate::loader::thread_of_program(pid) {
                let (_ex, _adm, _g) = system::thread_lage(t);
                #[cfg(feature = "debug-log")]
                print!(" P{pid}=tid{:#x}(da={_ex} zul={_adm} gr={_g:#04b})", t.to_raw());
            }
        }
        #[cfg(feature = "debug-log")]
        println!(" (leer BEI geladenen Programmen hiesse: die Registrierung selbst laeuft nicht)");
    }
    #[cfg(feature = "debug-log")]
    println!(
        "clientn : {_clients} Client-PD(s) mit EIGENER Ablage, {_ntfn_verloren} verloren (muss 0 \
         sein). Verschluesselt ist die Ablage mit der program_id, nicht mit der ROLLE -- „Client\" \
         ist eine Rolle, und die zweite Client-PD ueberschrieb bis 2026-08-10 die Ablage der \
         ersten. Die Schranke ist die Hoechstzahl der Manifest-Eintraege, also HERGELEITET: ein \
         Ueberlauf ist strukturell unerreichbar, und der Zaehler ist die Ratsche dagegen, dass \
         jemand die Schranke senkt"
    );

    // **VOLLZAEHLIGKEIT** (2026-08-10) -- die Zeile, die es sechs Wochen lang nicht gab.
    //
    // Ein Programm, das nicht laedt, hatte nichts, woran es haette auffallen koennen: es fehlte
    // einfach, und alle uebrigen Pruefzeilen blieben gruen, weil sie ueber ANDERE Programme
    // urteilen. Der wasm-Fall hat das bezahlt -- gefunden wurde er ueber die Umwege zweier
    // kaputter Pruefer, nicht ueber eine Pruefung.
    //
    // **Gemeldet werden die NAMEN der Fehlenden**, nicht eine Differenz: „einer fehlt" ist keine
    // Diagnose, „program_id 6 fehlt" ist eine.
    {
        let mut fehlend = [0u32; 16];
        let (erw, gel, n) = crate::loader::vollzaehligkeit(&mut fehlend);
        if erw == 0 {
            println!(
                "vollzahl: SKIP -- kein Manifest, also keine Sollmenge (diese Suite faehrt ohne \
                 Boot-Archiv; das ist bauartbedingt und kein Befund)"
            );
        } else {
            print!("vollzahl: {gel} von {erw} Programmen des Manifests sind geladen");
            if n > 0 {
                print!(" · FEHLEND:");
                for &pid in fehlend.iter().take(n) {
                    print!(" program_id {pid}");
                }
            }
            println!(
                " : {} (der GRUND eines Ladefehlschlags steht in der `loader :`-Zeile darueber)",
                if n == 0 { "ALL PASS" } else { "FAILURES" }
            );
        }
    }

    // **Das Vektor-Inventar.** Gedruckt wird JEDER Vektor, der ueberhaupt genommen wurde -- nicht
    // nur die auffaelligen. Ein Melder, der nur beim Unglueck spricht, ist in einem gesunden Lauf
    // stumm, und dann weiss niemand, ob er sprechfaehig ist. Ein Inventar ist in jedem Lauf
    // ablesbar, und Vektor 7 ist seine erste Zeile.
    print!("vektor  : Inventar (CPU-Ausnahmen 0..31, genommen):");
    let mut gesehen = 0;
    for v in 0..32u64 {
        let n = hal::exception::vector_hits(v as usize);
        if n > 0 {
            print!(" {v}({})={n}", hal::exception::vector_label(v));
            gesehen += 1;
        }
    }
    if gesehen == 0 {
        print!(" keine");
    }
    println!(
        " -- Vektor 7 (#NM) ist die Zeile, um die es geht; die uebrigen stehen dabei, damit          sichtbar ist, dass hier ueberhaupt gezaehlt wird"
    );

    let iface = IFACE_GATE_OK.load(Ordering::Acquire);
    // **A1 auf dem regulaeren Weg** (2026-08-07). Gelesen, nicht gemessen: die Messung DRUCKT,
    // und `all_done()` wird gepollt -- eine druckende Messung gehoert hier so wenig hin wie ein
    // Urteil, das erst im Bericht entsteht. Beim ersten Anlauf stand der Aufruf hier und die
    // Suite lief in den Watchdog. Gemessen wird einmal, in Schritt 2 unten.
    // **Ohne Archiv nicht anwendbar** -- dieselbe Form wie `root` darueber. Es wird dann kein
    // Programm geladen, also gibt es weder eine gefaerbte PD noch eine abweichende Politik; ein
    // dauerhaft falsches Konjunkt waere kein Befund, sondern eine Anforderung, die diese
    // Konfiguration nicht belegen KANN.
    let pdcolor = !archive || PDCOLOR_OK.load(Ordering::Acquire);
    // Z15/W1: ohne Archiv gibt es keine WASM-PD -- nicht anwendbar, wie `root` darueber.
    let wasm = !archive || WASM_OK.load(Ordering::Acquire);
    let ladepol = !archive || LADEPOL_OK.load(Ordering::Acquire);
    // **In die ABSCHLUSSBEDINGUNG, nicht nur in den Bericht** (2026-08-07). Die Gegenprobe hat es
    // gezeigt: eine Mutation, die `spawn_in_pd` zuerst zulassen und dann binden liess, ergab
    // `pdbind : FAILURES` -- und die Suite meldete `== ALL PASS ==`. Eine Zusicherung, die nur im
    // Bericht steht, faellt beim Brechen niemandem auf; das steht seit B-4.2 im Projekt und galt
    // fuer diese Zeile trotzdem nicht.
    let pdbind = system::LATE_PD_BIND.load(Ordering::Relaxed) == 0
        && system::LATE_PD_BIND_UNKLAR.load(Ordering::Relaxed) == 0
        && system::PD_BIND_GESAMT.load(Ordering::Relaxed) > 0;
    // A-4.2 aus demselben Grund wie B-4.2 in der Abschlussbedingung: eine Zusicherung, die nur
    // im Bericht steht, faellt beim Brechen niemandem auf.
    let quiesce = QUIESCE_OK.load(Ordering::Acquire);
    // A-4.1 aus demselben Grund: eine Zusicherung, die nur im Bericht steht, faellt beim
    // Brechen niemandem auf.
    let rebind = REBIND_OK.load(Ordering::Acquire);
    // D11 aus demselben Grund: eine Zusicherung, die nur im Bericht steht, faellt beim Brechen
    // niemandem auf.
    let epfull = EPFULL_OK.load(Ordering::Acquire);
    // A-4.3 aus demselben Grund: eine Zusicherung, die nur im Bericht steht, faellt beim Brechen
    // niemandem auf.
    let state = STATE_OK.load(Ordering::Acquire);
    // **Die Notbremse muss sagen koennen, WORAUF sie gewartet hat.**
    //
    // Bis hierher druckte sie "nicht alle Aussagen belegt" und sonst nichts. Damit ist ein
    // Haenger von einem nicht erfuellten Kriterium nicht zu unterscheiden -- man sieht, DASS es
    // nicht fertig wurde, und muss raten, woran. Beim Bau von A-6.2 hat genau das eine Runde
    // gekostet: alle Einzelzeilen standen auf ALL PASS, der Lauf lief trotzdem in die Notbremse.
    //
    // Der Grund wird deshalb ZURUECKGEGEBEN, nicht bloss verrechnet. Kostenlos ist das nicht ganz
    // (ein `&mut` je Runde), aber die Schleife dreht ohnehin Millionen Mal ohne etwas zu tun.
    // **Die Liste IST das Urteil** (2026-08-07). Bis hierher baute diese Funktion eine Liste
    // fuer den Bericht UND gab eine getrennte `&&`-Kette zurueck. Zwei Wirklichkeiten aus
    // derselben Hand: die Kette hatte 21 Glieder, die Liste 24 -- `pdcolor`, `ladepol` und
    // `pdbind` standen im Bericht und gatterten NICHTS. Gefunden hat das eine Gegenprobe:
    // eine Mutation, die zuerst zulaesst und dann bindet, ergab `pdbind : FAILURES`, und die
    // Suite meldete `== ALL PASS ==`.
    let flags: [(&'static str, crate::befund::Befund); DONE_FLAGS] = [
            ("workers", crate::befund::Befund::from(workers)),
            ("ipc", crate::befund::Befund::from(IPC_DONE.load(Ordering::Acquire))),
            ("cores", crate::befund::Befund::from(cores)),
            ("ring3", crate::befund::Befund::from(ring3)),
            ("iso", crate::befund::Befund::from(iso)),
            ("root", crate::befund::Befund::from(root)),
            ("stripes", crate::befund::Befund::from(stripes)),
            ("pprobe", crate::befund::Befund::from(pprobe)),
            ("virtio", crate::befund::Befund::from(virtio)),
            ("vblk", crate::befund::Befund::from(vblk)),
            ("vnet", vnet),
            ("drv", crate::befund::Befund::from(drv_seq)),
            ("bootckpt", crate::befund::Befund::from(ckpt_seq)),
            ("blkdev", crate::befund::Befund::from(blkdev)),
            ("dmaiso", crate::befund::Befund::from(DMAISO_OK.load(Ordering::Acquire))),
            ("part", crate::befund::Befund::from(part)),
            ("iface", crate::befund::Befund::from(iface)),
            ("pdcolor", crate::befund::Befund::from(pdcolor)),
            ("ladepol", crate::befund::Befund::from(ladepol)),
            ("pdbind", crate::befund::Befund::from(pdbind)),
            // **`wasm` gattert bewusst NICHT** (2026-08-10) -- eine benannte Auslassung, kein
            // Uebersehen, und sie steht mit Datum in `BEKANNT_ROT` der Lade-Suite.
            //
            // Bis dahin gatterte die Zeile, aber nur, weil ihr Kriterium ein SKIP zuliess: „kein
            // Bit gesetzt -> nicht anwendbar" war immer wahr, also war das Gatter wirkungslos.
            // Mit dem erreichbaren Kriterium (Abwesenheit an der Endowment-Tabelle entschieden)
            // wird sie **nie** wahr, solange die Badges nicht ankommen -- und ein Gatter, das
            // jeden Lauf in den Watchdog schickt, macht die Suite fuer alles andere unbrauchbar.
            // Dieselbe Abwaegung wie bei `fp` bis zum 2026-08-09: erst die Ursache, dann das
            // Gatter.
            ("quiesce", crate::befund::Befund::from(quiesce)),
            ("rebind", crate::befund::Befund::from(rebind)),
            ("epfull", crate::befund::Befund::from(epfull)),
            ("state", crate::befund::Befund::from(state)),
            ("park", crate::befund::Befund::from(PARK_MESS.load(Ordering::Acquire) & 1 != 0)),
            // Z23 S1: gattert von Anfang an. Anders als bei `fp`/`wasm` ist das Kriterium hier
            // erreichbar -- es wurde gegen die WIRKUNG formuliert (blockiert statt abgewiesen),
            // nicht gegen eine Zahl, die vom Zeitpunkt abhaengt.
            ("qgate", crate::befund::Befund::from(Q_MESS.load(Ordering::Acquire) & 1 != 0)),
            // Z22 P2: gattert aus demselben Grund wie `qgate` -- das Kriterium ist gegen die
            // WIRKUNG formuliert (Rundenzaehler, Ergebniscodes), nicht gegen einen Zustand.
            ("pdthrd", crate::befund::Befund::from(PT_MESS.load(Ordering::Acquire) & 1 != 0)),
            // C4/A3: die Fuellstands- und O(n)-Zeile gattert ebenfalls -- sonst waere sie genau
            // die Sorte Zeile, die niemand liest (dieselbe Begruendung wie bei B-4.2).
            ("vorrat", crate::befund::Befund::from(vorrat_urteil())),
            // C7: beide gattern von Anfang an. Ihre Kriterien sind erreichbar und gegen die
            // WIRKUNG formuliert (gezaehlte Rahmen; eine provozierte, wirklich abgewiesene
            // Anforderung) -- nicht gegen eine Zahl, die vom Zeitpunkt abhaengt. Eine gruene
            // Zeile, die nichts gattert, waere der `pdbind`-Fehler von neuem.
            ("ptab", crate::befund::Befund::from(PTAB_MESS.load(Ordering::Acquire) & 1 != 0)),
            ("mangel", crate::befund::Befund::from(MANGEL_MESS.load(Ordering::Acquire) & 1 != 0)),
            // C7: der Sweep gattert aus demselben Grund. Sein Kriterium ist gegen die WIRKUNG
            // formuliert (gefahrene Abweisungen, geschwiegene Pfade) und traegt seine eigene
            // Sprechprobe (`punkte >= 12`, `pfade >= 5`) -- ein Sweep, der nichts provoziert hat,
            // faellt durch, statt gruen zu schweigen.
            ("sweep", crate::befund::Befund::from(SWEEP_MESS.load(Ordering::Acquire) & 1 != 0)),
            // C4: die **Stack-Wasserstandsmarke**. Sie gattert aus demselben Grund wie `vorrat`,
            // und ihr Kriterium ist gegen die WIRKUNG formuliert (benutzte Tiefe), nicht gegen
            // eine Zahl, die vom Zeitpunkt abhaengt. Erreichbar ist es gemessen und nicht
            // gehofft: der Hoechststand lag bei der Einfuehrung bei 6,6 % des Stacks, die
            // Schwelle liegt bei 25 %.
            //
            // **Faellt die Fuellung aus, faellt dieses Konjunkt** -- ohne Muster am Fuss meldet
            // die Messung die volle Stackgroesse als benutzt. Ein Wasserzeichen, das immer „viel
            // Luft" sagt, ist damit strukturell ausgeschlossen und nicht bloss unwahrscheinlich.
            ("kstack", crate::befund::Befund::from(crate::kstackmark::urteil())),
            // C7b: die **EL0-Wasserstandsmarke**. Sie gattert aus demselben Grund wie `kstack`,
            // und ihr Kriterium ist gegen die WIRKUNG formuliert (benutzte Tiefe gegen die
            // kleinste Region der Klasse). Erreichbar ist es gemessen und nicht gehofft.
            //
            // **Drei Wege, auf denen sie still gruen werden koennte, sind einzeln verstellt**:
            // faellt die Nullung aus, ist der Fuss nicht null und `erschoepft > 0`; misst niemand,
            // faellt `gemessen_tod`; und zeigt die Buchfuehrung auf eine FREMDE (genullte) Region,
            // faellt die Tiefensonde -- den letzten Fall kann die Eichung strukturell nicht sehen.
            ("ustack", crate::befund::Befund::from(crate::userstackmark::urteil())),
            // C9: die **Sperrhaltedauer-Marke**. Gattert von Anfang an, und ihr Kriterium ist
            // gegen die WIRKUNG formuliert (gemessene maskierte Dauer gegen einen Timer-Tick),
            // nicht gegen einen Zustand. Erreichbar ist es gemessen und nicht gehofft: der
            // bereinigte Hoechststand lag bei der Einfuehrung bei 269..372 Promille eines Ticks
            // (fuenf Laeufe), die Schwelle bei 1000 -- Abstand Faktor 2,7 bis 3,7.
            //
            // **Faellt die Messung aus, faellt dieses Konjunkt**: ohne Zaehler ist der
            // Hoechststand `0`, und `0` waere von „alles kurz" nicht zu unterscheiden. Deshalb
            // verlangt das Urteil ausdruecklich `MESSUNG_VORHANDEN`, eine vollstaendige Eichung
            // und eine Mindestzahl gemessener Haltungen.
            ("sperre", crate::befund::Befund::from(crate::sperrmark::urteil())),
            // **C9b: die Schreibordnung der Konsole.** Gattert von Anfang an, und ihr Kriterium
            // ist gegen die WIRKUNG formuliert: nicht „ist der Umbau da", sondern „ist ein Byte
            // an der Ordnung vorbeigegangen". Genau das ist die einzige Richtung, in der der
            // Umbau schaden koennte -- verschlucken kann er strukturell nichts, weil es keinen
            // Puffer gibt.
            //
            // **Faellt die Ausgabe aus, faellt dieses Konjunkt**: ohne Bloecke ist `rueckritt`
            // trivialerweise 0, und das waere von „alles sauber" nicht zu unterscheiden. Deshalb
            // steht die Sprechprobe `bloecke >= MIND_BLOECKE` mit im Urteil.
            ("konsole", crate::befund::Befund::from(crate::sperrmark::konsole_urteil())),
            // **Die Wache unter der Guard-Page** (2026-08-10). Gattert von Anfang an, und ihr
            // Kriterium ist gegen die WIRKUNG formuliert: der Vektor wird ausgeloest, und die
            // Frame-Adresse muss in der Region liegen, die fuer genau ihn gedacht ist. Ein
            // IST-Eintrag, der nie benutzt wurde, ist von einem falsch aufgesetzten nicht zu
            // unterscheiden -- eine Zeile, die nur die Konfiguration LIEST, waere deshalb genau
            // die Sorte gruene Zeile, die nichts gattert.
            ("ist", crate::befund::Befund::from(super::ist::urteil())),
            // **Seit dem 2026-08-09 gattert `fp` wirklich.** Vorher stand die Zeile bewusst
            // draussen, weil ihr Kriterium („alle 64 Abgaben") unerreichbar war und die Suite
            // dauerhaft rot gefaerbt haette. Mit einem erreichbaren Kriterium waere ein
            // Draussenbleiben das Gegenteil: eine gruene Zeile, die nichts gattert -- genau der
            // `pdbind`-Fehler drei Zeilen weiter oben.
            ("fp", crate::befund::Befund::from(fp_urteil())),
            // **Der Ueberlauf ist BENANNT, nicht bloss verhindert** (D11). Die Schranke ist die
            // Hoechstzahl der Manifest-Eintraege, also hergeleitet -- damit ist der Fall heute
            // unerreichbar. Das Konjunkt ist die Ratsche dagegen, dass jemand die Schranke senkt.
            ("clientntfn", crate::befund::Befund::from(crate::loader::client_notification_stats().1 == 0)),
            // **C8: der Verifiziererthread und seine benannte Absage.** Gattert von Anfang an, und
            // das Kriterium ist gegen die WIRKUNG formuliert: der Ueberlauf wird GEFAHREN (die
            // Schranke muss ihren Hoechststand wirklich erreicht haben), der Ueberlaeufer bekommt
            // `ERR_LOAD_BUSY`, ist NICHT blockiert und laeuft nachweislich weiter, waehrend die
            // Bedienten wegen `LOAD` warten. Ohne Messung ist das Urteil `false` -- eine nie
            // gefahrene Absage darf nicht wie eine bestandene aussehen.
            ("verif", crate::befund::Befund::from(crate::verifizierer::urteil())),
            // **Z26/A3, die Nutzlast.** Gattert von Anfang an, und das Kriterium ist gegen die
            // WIRKUNG formuliert: der Gast bekommt einen Wert zurueck, den nur jemand liefern
            // kann, der seinen Frame im Sidecar GELESEN hat (Antwort = Argument + 1), und ein
            // Koeder in der Antwortnachricht belegt, dass der Wert NICHT ueber den IPC-Transport
            // kam. Dazu die Fail-closed-Haelfte an derselben Zeile.
            //
            // **Ohne Messung ist das Urteil `false`** -- ein nie gefahrener Umlauf darf nicht wie
            // ein bestandener aussehen (dieselbe Regel wie bei `verif`).
            ("redirect", crate::befund::Befund::from(system::handlermess::redirect_urteil())),
            // Z26/A3, der Kernel-Pruefpfad: kein fremder Wecker hebt den Handler-Grund auf. Eine
            // ANDERE Aussage als `redirect` -- sie laesst sich nur ohne echten Gast messen, weil
            // sie von der Abwesenheit einer Wirkung handelt.
            ("handler", crate::befund::Befund::from(system::handlermess::urteil())),
            // **Z6 stage 1: gates from the first day.** Its criterion is formulated against the
            // EFFECT (no two online logical CPUs share a physical core), recomputed from the IDs
            // that actually came up — not against the policy's own bookkeeping, which would hold
            // by construction.
            //
            // Reachable, and measured rather than hoped: under `-smp 4` (QEMU default
            // `threads=1`) the topology reads `Single`, nothing is suppressed and the clause is
            // trivially true. **That is exactly why the suite also runs a `threads=2` pass** — a
            // conjunct whose antecedent never occurs is the RMRR-on-q35 trap, and it would sit
            // here looking green forever. See `tools/smt-messen.sh`.
            ("smt", crate::befund::Befund::from(SMT_OK.load(Ordering::Acquire))),
            // Z8/N1: gates from the first day. Its criterion is formulated against the EFFECT
            // (was the topology read, is it whole, does the placement bookkeeping rest on a
            // trustworthy picture) and it is reachable: a machine without an SRAT reads
            // `readable=false`, which is allowed, while a TRUNCATED table fails -- "no statement"
            // and "a wrong statement" are different outcomes.
            ("numa", crate::befund::Befund::from(NUMA_OK.load(Ordering::Acquire))),
            // **Drei Sonden, die bis 2026-08-25 gar nicht gatterten.** Ihre `urteil()` waren
            // allesamt tot (`never used`) -- die gedruckte Zeile las nur `grep` in der Suite, im
            // Kernel hing an ihr nichts. Das ging nicht anders, solange der Ausgang ein `bool`
            // war: ein Ressourcen-SKIP haette den Lauf in den Watchdog geschickt. Mit dem dritten
            // Wert haengen sie hier, und ein SKIP steht im Bericht statt ihn aufzuhalten.
            ("dbg", crate::dbgprobe::urteil()),
            ("pdfreeze", crate::pdfreeze::urteil()),
            ("ckptcut", crate::ckptcut::urteil()),
            ("arena", crate::spawnarena::urteil()),
            ("dmapool", DMAPOOL_BEFUND.lesen()),
            ("irqmsi", IRQMSI_BEFUND.lesen()),
            // **BERICHTIGT 2026-08-26 -- was diese letzten fuenf Eintraege WIRKLICH tun.**
            //
            // Hier stand an `arena` „haengt vom ersten Tag an -- eine Zeile, die nur `grep` in der
            // Suite liest, gattert nichts". Der zweite Halbsatz stimmt, der erste nicht: `dbg`,
            // `pdfreeze`, `ckptcut`, `arena` und `dmapool` werden alle in **`report_and_off`**
            // gemessen, also NACHDEM `all_done()` entschieden hat. Gemessen statt erschlossen: ein
            // Lauf mit `arena : FAILURES` endete mit `SELFTEST COMPLETE` und OHNE Watchdog.
            //
            // Was der Eintrag hier bewirkt, ist deshalb genau eines: die Zeile erscheint in der
            // `offen waren:`-Aufzaehlung des Berichts. **Gegattert wird von der Suite** (`check`,
            // Rueckgabecode 1) -- und das ist auch der Grund, warum diese Sonden ueberhaupt hier
            // unten stehen duerfen: sie belegen PDs, Threads und Speicher, und ein Test, der
            // Speicher belegt, kippt baseline-empfindliche Tests weiter oben.
            //
            // *Ein Urteil, das in `all_done()` steht, darf nicht erst im Bericht entstehen* -- die
            // Regel gilt weiter. Diese fuenf sind die benannte Ausnahme: ihr Gatter ist die Suite,
            // und dieser Absatz ist die Stelle, an der das steht statt in einem Commit-Text.
    ];
    if let Some(w) = warum {
        *w = flags;
    }
    // **Nur `Durchgefallen` gattert** (2026-08-25). Ein `NichtGefahren` haelt den Bericht nicht
    // auf -- es steht IN ihm. Ob ein SKIP an dieser Stelle hinnehmbar ist, entscheidet die Suite
    // und nicht der Kernel: die Hauptsuite darf `vnet` ueberspringen (keine Karte), die
    // Lade-Suite nicht (sie bringt eine). Genau diese Aufteilung faehrt die `endow`-Zeile schon.
    !flags.iter().any(|&(_, v)| v.gattert())
}

/// Wie viele Einzelaussagen [`all_done`] prueft.
///
/// 2026-08-17: 41 -> 42 durch `smt` (Z6 Stufe 1), 42 -> 43 durch `numa` (Z8 N1).
/// 2026-08-25: 43 -> 46 durch `dbg`, `pdfreeze`, `ckptcut` -- drei Sonden, deren Urteil bis dahin
/// **nirgends gelesen** wurde (ihre `urteil()` meldete rustc als `never used`).
/// 2026-08-26: 48 -> 49 durch `irqmsi` (Stufe B, B1+B2+B3).
///
/// **Der Eintrag gattert hier NICHT**, und das ist wichtig genug fuer eine eigene Zeile: `irqmsi`
/// wird -- wie `dmapool`, `arena`, `ckptcut`, `pdfreeze`, `dbg` -- in `report_and_off` gemessen,
/// also **nachdem** `all_done()` entschieden hat. Was gattert, ist die **Suite** (`check`,
/// Rueckgabecode 1). Der Eintrag bewirkt genau eines: die Zeile erscheint in der
/// `offen waren:`-Aufzaehlung. Gemessen, nicht erschlossen -- s. das Register in `CLAUDE.md`.
#[cfg(feature = "selftest")]
const DONE_FLAGS: usize = 49;

/// A1 auf dem regulaeren Weg -- Ergebnis der EINMALIGEN Messung (s. Schritt 2 der Ladefolge).
#[cfg(feature = "selftest")]
static PDCOLOR_OK: AtomicBool = AtomicBool::new(false);
/// Z15/W1: hat die WASM-PD alle vier Aussagen belegt?
#[cfg(feature = "selftest")]
static WASM_OK: AtomicBool = AtomicBool::new(false);
/// Z11c: wird die Manifest-Politik angewandt? Ergebnis der EINMALIGEN Messung.
#[cfg(feature = "selftest")]
static LADEPOL_OK: AtomicBool = AtomicBool::new(false);

/// Bericht + Abschaltung (das Testskript wertet die Marker aus).
///
/// **`watchdog` = wir kamen über die Notbremse hierher, nicht über [`all_done`]** (B-1.8). Der
/// Unterschied MUSS im Marker stehen: vorher druckte diese Funktion `SELFTEST COMPLETE`
/// bedingungslos, also auch nach einem Abbruch. Damit konnte ausgerechnet der Marker, auf dem die
/// ganze Wiederholungsmessung steht (B-1.2/B-1.3 zählen ihn), einen vollständigen Lauf nicht von
/// einem abgelaufenen unterscheiden — im selben Log standen `WATCHDOG` und `SELFTEST COMPLETE`
/// untereinander. Der aarch64-Zweig macht es seit jeher richtig (`SELFTEST FAILED (watchdog)`);
/// das hier ist die Spiegelung, nicht eine neue Erfindung.
#[cfg(feature = "selftest")]
fn report_and_off(watchdog: bool) -> ! {
    // **E-Rest 3b/3d, und die Zahlen gehoeren an den SCHLUSS, nicht an den Hochlauf.** Zwei
    // Zahlen, weil es zwei verschiedene Lagen sind: „PD-abbildbar musste nach oben ausweichen"
    // heisst, GiB 0 ist voll und die Region liegt jetzt dort, wo eine PD sie NICHT sehen kann --
    // das ist ein Befund. „Reiner Kernel-Speicher musste nach unten" heisst nur, dass oben nichts
    // frei war, und ist harmlos. `0` heisst in beiden Faellen „kam nicht vor", nicht „geht
    // nicht" -- ohne diese Zeile saehe ein ungefahrener Pfad genauso aus wie ein tragender.
    let (pd_miss, kern_miss) = system::zone_misses();
    println!(
        "mem     : Zonen (E-Rest 3d) -- PD-abbildbar musste {pd_miss}x nach oben ausweichen, \
         reiner Kernel-Speicher {kern_miss}x nach unten (0 heisst 'kam nicht vor', nicht \
         'geht nicht'; ein PD-Ausweich waere ein Befund, ein Kernel-Ausweich ist harmlos)"
    );
    // ------------------------------------------------------------------------------------------
    // C4 / A3: DIE FUELLSTAENDE DER FESTEN VORRAETE -- und die Bilanz der O(n)-Pfade
    // ------------------------------------------------------------------------------------------
    //
    // **Warum das eine eigene Zeile ist und nicht Telemetrie am Rand.** Der Kernel hat mehrere
    // Vorraete FESTER Groesse (PD-Tabelle, Cap-Slots, Cap-Objekte, Endpoints, Notifications,
    // Thread-Slots, Kernel-Stack-RAM). Keiner davon hatte eine Fuellstandsanzeige. Was daraus
    // wird, hat dieses Projekt am 2026-08-10 vorgefuehrt: `SYS_LOAD` scheiterte mit
    // `NoResources` bei SECHS Programmen, und der leere Topf war keiner aus der obigen Liste,
    // sondern der Speicher fuer eine Seitentabelle. Ein fester Vorrat ohne Anzeige spricht erst,
    // wenn er leer ist -- und dann an einer Stelle, die mit der Ursache nichts zu tun hat.
    //
    // **Die zweite Haelfte sind ITERATIONSZAHLEN, keine Zeiten** (D10-Lehre). `pd_of` loest die
    // aufrufende PD bei jedem Syscall auf, der eine CAP aufloest (CALL/RECV/REPLY/SIGNAL/WAIT/
    // MAP/...); YIELD und PARK/UNPARK kehren im Dispatch vorher zurueck und kommen deshalb NICHT
    // vor -- die Zahl unten ist entsprechend kleiner als die Gesamtzahl der Syscalls, und das
    // gehoert dazu, sonst laese sich die 15 als Widerspruch zum Wort „jeder".
    // In der C4-Liste stand diese Stelle nicht -- die dort genannten laufen je Cap-Allokation
    // bzw. je Thread-Tod, diese je Syscall. Nach dem Umbau muss `scan=0` sein, WAEHREND
    // `aufrufe` gross ist: erst beide Zahlen zusammen sind die Aussage, eine Null allein waere
    // von „nie gefragt" nicht zu unterscheiden.
    let (pd_calls, pd_scan, pd_create_scan, owner_fehlt) = system::pd_scan_bilanz();
    let (pds_used, pds_cap, eps_used, eps_cap, nt_used, nt_cap) = system::vorrat_fuellstand();
    let (cap_slots, cap_objs) = system::cap_capacity();
    let (peak_slots, _, peak_objs, _) = system::cap_peaks();
    let (thr_frei, thr_cap) = (system::threads_available(), system::thread_capacity());
    let thr_live = thr_cap.saturating_sub(thr_frei);
    let kstack_mib = (thr_live as u64 * system::USER_KSTACK_SIZE as u64) >> 20;
    let (purge_calls, purge_iter) = (
        system::PURGE_IPC_CALLS.load(Ordering::Relaxed),
        system::PURGE_IPC_ITER.load(Ordering::Relaxed),
    );
    // **Der Seitentabellen-Topf gehoert IN diese Zeile** (C7). Er war der einzige der genannten
    // Vorraete ohne Fuellstandsanzeige -- und ausgerechnet der, an dem `wasmhost` bei SECHS
    // Programmen gescheitert ist. Gezaehlt an der Quelle (was der Allokator FUER Seitentabellen
    // herausgibt), nicht als Differenz des freien RAM: eine Differenz misst Stacks, private
    // Regionen und Segmente mit.
    let (pt_raus, pt_zurueck, pt_gehalten, pt_peak) = system::seitentabellen_topf();
    // **Der Wachen-Vorrat ist FEST** (die HAL hat keinen Allokator -- Kerngrenze), also gehoert
    // sein Fuellstand hierher: ein fester Vorrat ohne Anzeige spricht erst, wenn er leer ist,
    // und dann an einer Stelle, die mit der Ursache nichts zu tun hat. `gd_total > 0` ist
    // zugleich die Sprechprobe -- null gesetzte Wachen hiesse „nie benutzt", und dann sagte die
    // Zeile nichts ueber die Wache aus.
    let (gd_live, gd_total, gd_denied, gd_blk, gd_blkcap) = hal::mmu::guard_stats();
    let pt_vspaces = system::used_vspaces();
    println!(
        "vorrat  : Fuellstaende -- PDs {pds_used}/{pds_cap} · Cap-Slots {peak_slots}/{cap_slots} \
         (Hoechststand) · Cap-Objekte {peak_objs}/{cap_objs} · Endpoints {eps_used}/{eps_cap} · \
         Notifications {nt_used}/{nt_cap} · Thread-Slots {thr_live}/{thr_cap} · freies RAM {} MiB \
         · Kernel-Stacks {kstack_mib} MiB ({} KiB je Thread) · Seitentabellen {pt_gehalten} \
         Rahmen = {} KiB (Hoechststand {pt_peak}; {pt_raus} raus / {pt_zurueck} zurueck; \
         {pt_vspaces} belegte VSpaces, also {} Rahmen je VSpace) · Guard-Pages {gd_live} stehen / \
         {gd_total} gesetzt / {gd_denied} abgewiesen, {gd_blk}/{gd_blkcap} aufgeteilte 2-MiB-Bloecke",
        system::total_free() >> 20,
        system::USER_KSTACK_SIZE >> 10,
        (pt_gehalten * 4096) >> 10,
        if pt_vspaces > 0 { pt_gehalten / pt_vspaces as u64 } else { 0 }
    );
    println!(
        "vorrat  : O(n)-Bilanz (Iterationen, nicht Zeit) -- pd_of: {pd_calls} cap-aufloesende \
         Syscalls (YIELD/PARK kehren vorher zurueck und zaehlen NICHT mit) / \
         {pd_scan} Scan-Iterationen (muss 0 sein: seit Z22 P2 O(1) ueber den Rueckwaerts-Index; \
         linear waeren es {} gewesen) · PdTable::create: {pd_create_scan} Iterationen · \
         purge_ipc_queues: {purge_calls} Aufrufe / {purge_iter} IPC-Objekte durchlaufen \
         (O(Endpoints+Notifications) JE Thread-Tod -- offen, C4) · Bindungen ohne \
         Rueckwaerts-Tabelle={owner_fehlt}",
        pd_calls.saturating_mul(pds_cap as u64)
    );
    {
        let (scan_calls, erster, owner_len) = system::pd_scan_diagnose();
        println!(
            "vorrat  : Rueckfall-Diagnose -- {scan_calls} Aufruf(e) liefen linear, erster \
             betroffener Thread-Slot={} (u64::MAX = keiner), Rueckwaerts-Tabelle haengt mit \
             {owner_len} Eintraegen (0 = gar nicht). Ein Rueckfall, den niemand erklaeren kann, \
             ist ein offener Posten und kein Messwert",
            if erster == 0 {
                u64::MAX
            } else {
                erster - 1
            }
        );
    }
    // **Zwei Konjunkte, und beide sind falsifizierbar.** `scan == 0` faellt, sobald die
    // Rueckwaerts-Tabelle fehlt (Gegenprobe: `attach_owner` weglassen -> die Zahl springt auf
    // Aufrufe x PD-Kapazitaet). `aufrufe > 0` ist die Sprechprobe am gepruefte Pfad selbst:
    // ohne sie bestuende die Zeile auch ein System, das gar keine Syscalls macht.
    let vorrat_ok = vorrat_urteil();
    println!(
        "vorrat  : {} (C4: der teuerste O(n)-Pfad des Systems lief je SYSCALL und stand nicht \
         in der C4-Liste. Die uebrigen drei stehen dort und sind weiterhin linear -- ihre \
         Iterationszahlen oben sind die Grundlinie, gegen die eine Behebung zu messen ist)",
        if vorrat_ok { "ALL PASS" } else { "FAILURES" }
    );
    // ------------------------------------------------------------------------------------------
    // C7: das URTEIL ueber den Seitentabellen-Topf -- getrennt von `vorrat`, damit eine
    // Gegenprobe genau EIN Konjunkt kippen kann und nicht die halbe Zeile.
    // ------------------------------------------------------------------------------------------
    {
        let (_a, _b, _c, _d) = ptab_urteil();
        let _noetig = system::PT_RAHMEN_JE_VSPACE * pt_vspaces as u64;
        #[cfg(feature = "debug-log")]
        println!(
            "ptab    : Seitentabellen-Topf -- allokationsseite-spricht={_a} ({pt_raus} Rahmen \
             geholt) · freigabeseite-spricht={_b} ({pt_zurueck} Rahmen zurueck) · \
             bilanz={_c} (raus >= zurueck: es kann nichts zurueckkommen, was nie herausgegeben \
             wurde -- faellt, sobald EINE Allokationsstelle nicht mehr bucht) · \
             quervergleich={_d} ({pt_gehalten} gehalten >= {_noetig} = {} je VSpace x \
             {pt_vspaces} belegte; bei NULL belegten VSpaces sagt dieser Konjunkt nichts, und \
             das gehoert dazu). Gemessen AN DER QUELLE (was der Allokator fuer eine \
             Seitentabelle herausgibt), nicht als Differenz des freien RAM -- eine Differenz \
             misst Stacks, private Regionen und Segmente mit",
            system::PT_RAHMEN_JE_VSPACE
        );
        println!(
            "ptab    : {} (C7: eine PD im GLOBALEN Adressraum zieht diesen Topf gar nicht, und \
             in der isolierten Kurve erschlaegt ihn die private 2-MiB-Region -- deshalb hatte \
             ausgerechnet der Topf, an dem `wasmhost` bei SECHS Programmen gescheitert ist, bis \
             heute keine Anzeige)",
            if ptab_messen() { "ALL PASS" } else { "FAILURES" }
        );
    }
    // ------------------------------------------------------------------------------------------
    // C7: die SPRECHPROBE des Mangel-Melders auf einem `spawn_*`-Pfad.
    // ------------------------------------------------------------------------------------------
    {
        let (_abgewiesen, _code, _bytes, _farben) = *MANGEL_PROBE.lock();
        #[cfg(feature = "debug-log")]
        println!(
            "mangel  : Sprechprobe auf spawn_isolated_colored (leere Farbmaske, {_farben} Farben) \
             -- abgewiesen={_abgewiesen} · Code {_code} = {} · gemeldet {_bytes} Byte (angefordert \
             {} Byte) · vergiftet war {} (kein Kernelpfad schreibt diesen Code -- steht er noch \
             da, hat der Pfad GESCHWIEGEN)",
            system::mangel_name(_code),
            system::USER_KSTACK_SIZE,
            system::MANGEL_VERGIFTET
        );
        println!(
            "mangel  : {} (C7: bis zum 2026-08-10 meldete `lade_mangel()` in JEDEM Kurvenabbruch \
             'keiner', auch bei 2 MiB freiem RAM -- der Melder hing allein am SYS_LOAD-Pfad. Die \
             Zeile belegt, dass er auf einem spawn_*-Pfad greift UND die Menge aus dem Aufruf \
             traegt, nicht aus einem Literal daneben)",
            if mangel_messen() { "ALL PASS" } else { "FAILURES" }
        );
        let (punkte, stumm, keiner, menge_falsch, fremd, codes, pfade) = *SWEEP.lock();
        let (je_pfad, zuende, je_codes) = *SWEEP_PFAD.lock();
        // **Der Ladepfad wird GEMESSEN, nicht angenommen** -- und beide Zahlen stehen daneben,
        // damit „nicht gefahren" von „gar nicht fahrbar" unterscheidbar bleibt.
        let mit_archiv = crate::loader::read_archive().is_some();
        let lade_gefahren = je_pfad[PFAD_LADEN];
        // Wieviele der 32 Meldestellen dieser LAUF wirklich provoziert hat. Die Klassen stehen
        // bei `SWEEP_KMAX` und gehen zur Bauzeit gegen `system::MELDESTELLEN` auf; was hier
        // gerechnet wird, ist allein die Frage, ob der Ladepfad in DIESEM Lauf dabei war.
        let provoziert = SWEEP_K1_PROVOZIERT_OHNE_ARCHIV
            + if lade_gefahren > 0 { SWEEP_K2_PROVOZIERT_MIT_ARCHIV } else { 0 };
        println!(
            "sweep   : {punkte} provozierte Abweisungen auf {pfade} Pfaden -- \
             geschwiegen={stumm} (die Marke {} stand danach noch da) · 'keiner' trotz \
             Abweisung={keiner} · Menge NICHT aus dem Aufruf={menge_falsch} · fremder \
             Verbraucher={fremd} · Toepfe die gesprochen haben: {} von 13 (Maske {codes:#x})",
            system::MANGEL_VERGIFTET,
            codes.count_ones()
        );
        println!(
            "sweep   : je Pfad -- isoliert={} ({}) · gefaerbt={} ({}) · nativ={} ({}) · \
             user={} ({}) · kernel={} ({}) · in-PD={} ({}) · geraetefenster={} ({}) · \
             LADEN={} ({}, Toepfe {:#x}). 'bis Ende' heisst: die Sperre feuerte \
             nicht mehr, der Pfad hat also JEDE seiner Anforderungen einmal abgewiesen bekommen -- \
             nur dann traegt der Schluss auf die einzelnen Meldestellen. 'gedeckelt' heisst, der \
             Schluss gilt bis zum Deckel; beim Ladepfad ist der Deckel die Halteregel (ein \
             Durchgang nach dem Stack-Topf), denn der naechste Durchgang wuerde ein fremdes \
             Programm STARTEN",
            je_pfad[0], if zuende & 1 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[1], if zuende & 2 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[2], if zuende & 4 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[3], if zuende & 8 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[4], if zuende & 16 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[5], if zuende & 32 != 0 { "bis Ende" } else { "gedeckelt" },
            je_pfad[PFAD_FENSTER],
            if zuende & (1 << PFAD_FENSTER) != 0 { "bis Ende" } else { "gedeckelt" },
            lade_gefahren,
            if mit_archiv { "Boot-Archiv da" } else { "KEIN Boot-Archiv -- nicht fahrbar" },
            je_codes[PFAD_LADEN],
        );
        {
            let (dv, dp, dt, dr, dm) = *SWEEP_BILANZ.lock();
            println!(
                "sweep   : Bilanz (nachher minus vorher) -- VSpaces={dv} · PD-Slots={dp} · \
                 Thread-Slots={dt} · Seitentabellen-Rahmen={dr} · freies RAM={dm} Byte. Die \
                 ersten vier muessen 0 sein und sind Konjunkte: in diesem Fenster bewegt sie nur \
                 der Sweep (er laeuft, nachdem jedes andere Urteil steht). Beim RAM steht 'nicht \
                 weniger', weil ein sterbender Thread waehrenddessen seinen Stack zurueckgibt -- \
                 ein Zuwachs ist kein Befund. Ohne diese Zeile waere 'der Sweep raeumt auf' eine \
                 Behauptung, und ein halb abgebauter Ladevorgang faelschte jede Zeile nach ihm"
            );
        }
        println!(
            "sweep   : {} (C7-Abdeckung: {provoziert} von {} Meldestellen in DIESEM Lauf \
             provoziert; der Nenner kommt aus tools/mangel-zaehlen.py, die Summanden aus einer \
             Zusicherung zur Bauzeit -- eine Klassifikation, die man vergessen kann, ist keine. \
             {}+{}+{}+{}+{}+{}+{} = {}. **{} PROVOZIERT OHNE ARCHIV**: 125 Kernel-Stack · \
             2379/2436 Kernel-Thread-Stack · 2473 User-Stack · 2590/2625 Seitentabelle in \
             create_vspace · 2989 Seitentabelle im Fenster · 3169/3322/3463/3464 Privatregion · \
             4608 map_region_into_thread -- die letzte stand bis heute als 'Ladepfad' gebucht und \
             ist keiner: sie ist der Weg zum GERAETEFENSTER einer Treiber-PD und braucht kein \
             Archiv. **{} PROVOZIERT NUR MIT ARCHIV** (in diesem Lauf: {}): 4288 Segmentspeicher · \
             4304 Seitentabelle eines Segments · 4350 User-Stack · 4366 Seitentabelle des Stacks \
             · Heap-OOM entfallen (kein Heap im Kernel, s. Klasse 2) \
             -- der echte load_into_pd_mit, dieselbe Funktion, die SYS_LOAD ruft. **{} PLATZ-\
             TOEPFE** (2399/2453/3221/3386/3547/4438 Thread-Slot, 2572 ASID, 3293/4171 \
             Farbstreifen, 4516 PD-Slot): die Sperre sitzt im SPEICHER-Allokator, diese Toepfe \
             vergeben keine Bytes. **{} 'DER ALLOKATOR WURDE NICHT GEFRAGT'** \
             (2998/4317/4380 MAPPING_ABGEWIESEN): sie melden genau den Fall 'es lag NICHT am \
             Speicher' -- erreichbar ueber ein Image mit krummer Segment-VA, nicht ueber einen \
             leeren Topf. **{} FESTER HAL-VORRAT** (139 Guard-Tabelle): die HAL hat keinen \
             Allokator. **{} STRUKTURELL NICHT AUSLOESBAR, und das ist ein Befund ueber die \
             Stellen**: 4207 (vspace_l2 gibt fuer eine soeben angelegte ASID immer Some -- toter \
             Zweig) und 4269 (fail-closed gegen mehr als 64 Frame-Stuecke -- das braucht ein \
             IMAGE, keinen leeren Topf)) **{} FORK-PFAD (noch nicht provoziert -- eigene Sonde \
             offen)**: die zehn Meldestellen in `dispatch_fork`",
            if sweep_messen() { "ALL PASS" } else { "FAILURES" },
            system::MELDESTELLEN,
            SWEEP_K1_PROVOZIERT_OHNE_ARCHIV,
            SWEEP_K2_PROVOZIERT_MIT_ARCHIV,
            SWEEP_K3_PLATZ,
            SWEEP_K4_NICHT_GEFRAGT,
            SWEEP_K5_HAL_VORRAT,
            SWEEP_K6_UNAUSLOESBAR,
            SWEEP_K7_FORK_PFAD,
            system::MELDESTELLEN,
            SWEEP_K1_PROVOZIERT_OHNE_ARCHIV,
            SWEEP_K2_PROVOZIERT_MIT_ARCHIV,
            if lade_gefahren > 0 { "gefahren" } else { "NICHT gefahren" },
            SWEEP_K3_PLATZ,
            SWEEP_K4_NICHT_GEFRAGT,
            SWEEP_K5_HAL_VORRAT,
            SWEEP_K6_UNAUSLOESBAR,
            SWEEP_K7_FORK_PFAD,
        );
    }

    // ------------------------------------------------------------------------------------------
    // C4: DIE STACK-WASSERSTANDSMARKE -- reichen 16 KiB, oder sind sie nur gross?
    // ------------------------------------------------------------------------------------------
    //
    // Die `vorrat`-Zeile darueber sagt, wie GROSS die Kernel-Stacks sind. Sie sagt nicht, ob sie
    // REICHEN -- und das ist der Unterschied zwischen einer Zahl und einem Beleg. Ohne diese
    // Zeile ist jede kleinere Groesse geraten: 10 000 EL0-Threads x 16 KiB sind 160 MiB, auf einer
    // 512-MiB-Maschine 31 % nur fuer schlafende Threads. Wer die Zahl senken will, muss wissen,
    // wie tief der tiefste Kernelpfad wirklich geht.
    //
    // **Zuerst fegen, dann urteilen.** `reclaim_user_kstack` misst nur die Stacks STERBENDER
    // Threads; die langlebigen (IPC-Server, Treiber-PDs, Root-Task) sterben in einem gruenen Lauf
    // nie -- und gerade sie fahren die Pfade, um die es geht. Ein Wasserstand nur aus den Toten
    // waere eine Stichprobe mit Auswahlfehler. Gefegt wird deshalb hier, am SCHLUSS, nach jedem
    // anderen Urteil: eine Schleife ueber alle lebenden Kstacks haelt kurz das KSTACKS-Blattlock
    // und wuerde jede baseline-empfindliche Zeile davor stoeren.
    //
    // **Das Fegen kann das Urteil nur VERSCHLECHTERN**, nie verbessern (es hebt den Hoechststand
    // und senkt die Reserve). Der Unterschied zwischen dem Gatter in `all_done` (das ohne den
    // Fegelauf entscheidet) und dieser Zeile faellt damit fail-closed aus: das Gatter laesst
    // durch, was der Bericht danach noch rot faerben kann -- und eine rote `kstack`-Zeile faengt
    // der allgemeine Rotzeilen-Scanner der Suite.
    {
        let (gefegt, tiefster) = system::kstack_marke_fegen();
        // **C8: der Verifiziererstack gehoert in DIESE Messung, nicht in eine eigene Zahl.**
        // Er ist ein Kernel-Thread, der nie stirbt -- der Reap-Pfad misst ihn also nie, und
        // `kstack_marke_fegen` fegt ueber die EL0-Klasse. Ohne diese Zeile waere seine Groesse
        // GEWAEHLT statt gemessen, und C8 (b) verlangt das Gegenteil. Sie steht VOR dem Lesen der
        // Marke, damit sie in denselben Hoechststand einfliesst.
        let (v_benutzt, v_frei, v_groesse) = crate::verifizierer::stack_messen();
        let e = crate::kstackmark::marke(crate::kstackmark::KL_EL0);
        let k = crate::kstackmark::marke(crate::kstackmark::KL_KERN);
        let anteil = |m: &crate::kstackmark::Marke| -> u64 {
            if m.groesse == 0 {
                0
            } else {
                (m.tiefe_max as u64 * 1000) / m.groesse as u64
            }
        };
        println!(
            "kstack  : Wasserstand EL0-Kstack -- Hoechststand {} von {} B ({}.{} %), Reserve \
             mindestens {} B · {} gefuellt / {} gemessen ({gefegt} davon am Schluss ueber LEBENDE \
             Stacks gefegt, tiefster lebender Thread-Slot {}) · ohne Muster am Fuss: {} (muss 0 \
             sein -- das heisst 'aufgebraucht ODER nie gefuellt', beide sollen dasselbe Urteil \
             ausloesen)",
            e.tiefe_max,
            e.groesse,
            anteil(&e) / 10,
            anteil(&e) % 10,
            if e.frei_min == usize::MAX { 0 } else { e.frei_min },
            e.gefuellt,
            e.gemessen,
            if tiefster == usize::MAX { u64::MAX } else { tiefster as u64 },
            e.erschoepft,
        );
        // **Die Herkunft trennt eine Zahl von einer Aussage.** Ein STERBENDER Thread wurde zuletzt
        // im Fault-/Exit-Pfad gemessen -- dort laeuft `println!` mit voller Formatierung und der
        // VSpace-Teardown auf eben diesem Stack; ein LEBENDER im Zustand seines letzten Syscalls.
        // Welcher der beiden den Hoechststand haelt, sagt, wo die naechste Vertiefung herkaeme.
        println!(
            "kstack  : Herkunft des Hoechststands -- sterbende Threads {} B ({} Messungen, \
             DIESE sieht das Gatter), lebende Threads {} B ({gefegt} Messungen, erst im Bericht) \
             · Rekordhalter Thread-Slot {} (Programm {}). Alle IDT-Tore sind INTERRUPT-Gates \
             (Typ 0x8E), IF ist waehrend des Handlers 0 -- ein TIMER-Tick kann also nicht auf \
             einem Syscall-Frame schachteln. **Das gilt aber nur fuer MASKIERBARE Interrupts: \
             NMI und #MC fragen IF nicht.** Unter QEMU ohne Watchdog kommt keiner, deshalb misst \
             diese Zahl eine reine AUFRUFKETTE; auf echter Hardware ist ein NMI Betriebsrealitaet, \
             und dann muss die Reserve zusaetzlich einen NMI-Frame samt Handler tragen -- oder NMI \
             bekommt seinen eigenen IST-Stack. Der Hoechststand hier ist also eine Aussage ueber \
             das MESSUMFELD, nicht ueber die Maschine, auf der die Reserve gelten muss. \
             **ACHTUNG, DIESE ZAHL HAT SEIT C8 EINE ANDERE BEDEUTUNG (2026-08-11).** Bis dahin \
             mass sie den Ladepfad: `SYS_LOAD` verifizierte eine Ed25519-Signatur und einen \
             SHA-2-Hash IM KERNEL, also auf dem 16-KiB-Stack des aufrufenden EL0-Threads, und der \
             Hoechststand lag bei 11992 B (73,1 %) in der Lade-Suite. Seit C8 laeuft genau dieser \
             Pfad auf dem eigenen 64-KiB-Stack des VERIFIZIERERTHREADS (Zeile darunter); was diese \
             Zeile misst, ist der RESTPFAD -- der tiefste verbliebene Syscall. Wer die Zahl von \
             heute mit einer von vor dem 2026-08-11 vergleicht, vergleicht ZWEI VERSCHIEDENE \
             GROESSEN. Die Guard-Page unter dem Kstack gibt es inzwischen (`USER_KSTACK_ALLOC`)",
            e.tiefe_tod,
            e.gemessen_tod,
            e.tiefe_lebend,
            if e.tiefster_slot == usize::MAX { u64::MAX } else { e.tiefster_slot as u64 },
            match crate::loader::program_of_slot(e.tiefster_slot) {
                Some(p) => p as i64,
                None => -1,
            },
        );
        println!(
            "kstack  : Wasserstand Kernel-Thread (64 KiB) -- Hoechststand {} von {} B ({}.{} %), \
             {} gefuellt / {} beim Reap wiedererkannt. **Auskunft, kein Konjunkt**: erkannt wird \
             am Muster, ein RESTLOS aufgebrauchter Kernel-Stack traegt keines mehr und faellt hier \
             heraus -- fuer die EL0-Klasse gilt die Einschraenkung nicht (dort wird der Kstack \
             direkt gemessen, nicht gesucht)",
            k.tiefe_max,
            k.groesse,
            anteil(&k) / 10,
            anteil(&k) % 10,
            k.gefuellt,
            k.gemessen,
        );
        // **C8 (b): die Stackgroesse des Verifizierers ist GEMESSEN, nicht gewaehlt.**
        //
        // Er ist der Traeger des tiefsten Kernelpfads dieses Systems -- Ed25519 + SHA-2. Solange
        // niemand seinen Wasserstand liest, waeren die 64 KiB eine Zahl mit derselben Berechtigung
        // wie die 16 KiB vorher: keine. `0 / 0` heisst hier „nicht messbar" (kein Verifizierer),
        // NICHT „viel Luft" -- dieselbe Unterscheidung, an der `Urteil::gemessen` haengt.
        println!(
            "kstack  : Wasserstand VERIFIZIERER (C8) -- benutzt {v_benutzt} von {v_groesse} B, \
             Reserve {v_frei} B. HIER liegt seit dem 2026-08-11 die Krypto: `SYS_LOAD` -> \
             verify_image -> Ed25519 + SHA-2. Vorher lief sie auf dem 16-KiB-Kstack des \
             AUFRUFERS und fuellte ihn zu 73,1 % -- also zahlte JEDER Thread 16 KiB fuer EINEN \
             Pfad. 0 von 0 heisst 'nicht messbar' (kein Verifiziererthread), nicht 'viel Luft'"
        );
        // **Die Summe steht als eigene Zeile, weil sie die eigentliche Aussage traegt.**
        let (s_pfad, s_irq, s_res, s_gr, s_n) = crate::kstackmark::summe();
        println!(
            "kstack  : SUMME (C4, strukturell statt statistisch) -- tiefster Pfad {s_pfad} B + \
             tiefster IRQ-Handler {s_irq} B ({s_n} Messungen) + geforderte Reserve {s_res} B = \
             {} B von {s_gr} B. Addiert wird, weil ein Interrupt genau am Scheitelpunkt der \
             tiefsten Kette eintreffen kann und dann auf DEMSELBEN Stack landet -- ob das \
             Messumfeld diese Koinzidenz je gewuerfelt hat, weiss niemand. #DF/NMI/#MC zaehlen \
             NICHT mit: sie laufen auf eigenen IST-Staecken, und genau das haben die gekauft. \
             VORBEHALT zum zweiten Summanden: gemessen wird an der tiefsten Stelle, die OHNE \
             Instrumentierung jedes Aufgerufenen erreichbar ist (`reschedule`) -- der Scheduler \
             darunter geht weiter, die Zahl ist also eine UNTERGRENZE. Im Einsprung der HAL waren \
             es 24 B, hier 144 B; der Abstand zur Stackgroesse traegt auch ein Vielfaches davon",
            s_pfad + s_irq + s_res
        );
        println!(
            "kstack  : {} (C4: geforderte Mindestreserve {} B = 1/{} des Stacks, Eichung \
             {:#06b}/{:#06b}, mindestens {} Messungen vor dem Gatter. Die Schwelle ist die \
             RESERVE und nicht der Verbrauch -- sie bleibt richtig, wenn jemand USER_KSTACK_SIZE \
             aendert, und sie benennt die Groesse, um die es geht)",
            if crate::kstackmark::urteil() { "ALL PASS" } else { "FAILURES" },
            crate::kstackmark::mindestreserve(e.groesse),
            crate::kstackmark::MIND_RESERVE_NENNER,
            crate::kstackmark::eichstand(),
            crate::kstackmark::EICH_ALLE,
            crate::kstackmark::MIND_MESSUNGEN,
        );
    }

    // **D15-Melder, arch-neutral gezaehlt und hier GEDRUCKT.** Ohne diese Zeile waere die
    // Frage „gilt das auch auf x86?" nur zu beantworten, indem jemand denselben Melder ein zweites
    // Mal baut -- und zwei Fassungen derselben Messung laufen auseinander. Gezaehlt wird der
    // ZUSTAND (Identitaet des Kstack-Eigentuemers, Freigabe des eigenen Stacks), nicht der Ausgang.
    {
        let (_spaet, _fremd, _fuesse, _rec_ges, _rec_stack) = crate::system::kstack_spaet_stats();
        let (_zomb_ges, _zomb_fuss) = caprock_sched::zombie_fuss_stats();
        #[cfg(feature = "debug-log")]
        println!(
            "kstackid: spaet-am-wiedervergebenen-Slot={_spaet} fremder-Kstack-freigegeben={_fremd} \
             (von {_rec_ges} Aufraeumungen, {_rec_stack} mit Stack)"
        );
        #[cfg(feature = "debug-log")]
        println!(
            "kstackid: EL0-Kstack unter den eigenen Fuessen freigegeben={_fuesse}; \
             Zombie-Region unter den eigenen Fuessen eingereiht={_zomb_fuss} von {_zomb_ges}"
        );
    }

    // ------------------------------------------------------------------------------------------
    // C7b: DIE EL0-WASSERSTANDSMARKE -- wie tief ist der USER-Stack wirklich?
    // ------------------------------------------------------------------------------------------
    //
    // Die Zeile darueber misst den EL1-Stack eines EL0-Threads. Diese misst die andere Seite
    // desselben Threads: seine **private Region**, den EL0-Stack -- und die ist mit 2 MiB der
    // groesste Einzelposten je Mandant (C7b: 2048 von rund 2076 KiB). Solange diese Zahl nicht
    // gemessen ist, ist jede kleinere Regionsgroesse geraten.
    //
    // **Zuerst fegen, dann urteilen** -- aus demselben Grund wie oben: der Sterbepfad misst nur
    // die Regionen sterbender Threads, die tiefsten Userland-Pfade fahren aber die langlebigen
    // (Root-Task, Treiber-PDs). Und wie oben kann das Fegen das Urteil nur VERSCHLECHTERN, nie
    // verbessern: es hebt den Hoechststand. Der Unterschied zwischen dem Gatter (das ohne den
    // Fegelauf entscheidet) und dieser Zeile faellt damit fail-closed aus.
    {
        let (_gefegt, _tiefster) = system::userstack_marke_fegen();
        let u = crate::userstackmark::marke();
        let (_s_gemessen, _s_ok, _s_tiefe) = crate::userstackmark::sonde_stand();
        // **Zwei Groessen, zwei Zahlen** -- und sie werden getrennt gedruckt, weil sie aus
        // verschiedenen Regionen stammen duerfen: der TIEFSTE Pfad in Bytes (er traegt die
        // Summenbedingung) und der hoechste FUELLGRAD (er sagt, wie knapp es irgendwo wurde). In
        // einer Klasse mit vier Regionsgroessen ist „45 %" ohne seine Region keine Aussage.
        let _tiefe_promille = if u.tiefste_groesse == 0 {
            0
        } else {
            u.tiefe_max * 1000 / u.tiefste_groesse
        };
        #[cfg(feature = "debug-log")]
        println!(
            "ustack  : Wasserstand EL0-USER-Stack -- Hoechststand {} von {} B ({}.{} % SEINER \
             Region; hoechster Fuellgrad ueberhaupt {}.{} %, moeglicherweise in einer anderen) · \
             {} registriert / {} gemessen ({} davon im \
             Sterbepfad -- DIESE sieht das Gatter, {_gefegt} am Schluss ueber LEBENDE Regionen \
             gefegt) · Rekordhalter Thread-Slot {} · Regionsgroessen {}..{} B · Fuss nicht genullt: \
             {} (muss 0 sein -- das heisst 'aufgebraucht ODER nie genullt', beide sollen dasselbe \
             Urteil ausloesen) · nie benutzt: {} (Threads, die kein von Null verschiedenes Byte \
             hinterlassen haben -- KEIN Messfehler, aber auch kein Beleg fuer Luft) · tiefster \
             lebender Slot {}",
            u.tiefe_max,
            u.tiefste_groesse,
            _tiefe_promille / 10,
            _tiefe_promille % 10,
            u.fuell_max_promille / 10,
            u.fuell_max_promille % 10,
            u.registriert,
            u.gemessen,
            u.gemessen_tod,
            if u.tiefster_slot == usize::MAX { u64::MAX } else { u.tiefster_slot as u64 },
            if u.groesse_min == usize::MAX { 0 } else { u.groesse_min },
            u.groesse_max,
            u.erschoepft,
            u.nie_benutzt,
            if _tiefster == usize::MAX { u64::MAX } else { _tiefster as u64 },
        );
        #[cfg(feature = "debug-log")]
        println!(
            "ustack  : Herkunft -- sterbende Threads {} B, lebende {} B. **Die Klasse fasst VIER \
             Groessen**: die private Region einer isolierten PD ({} B), die gefaerbte Region \
             ({} B = colors::region_bytes), der User-Stack eines GELADENEN Programms (16384 B) \
             und der SAS-Stack (65536 B). Deshalb traegt die Summenbedingung unten die KLEINSTE \
             gemessene Region und nicht 'die' Groesse -- eine Klasse aus mehreren Groessen ist so \
             tragfaehig wie ihr kleinstes Mitglied. **NICHT erfasst**: der Stack einer GEFAERBT \
             geladenen PD (er liegt in Stuecken in der seglist der PD und gehoert nicht dem \
             Thread) und die Userland-Tiefe zwischen zwei Messungen",
            u.tiefe_tod,
            u.tiefe_lebend,
            hal::mmu::PRIV_REGION_SIZE,
            crate::colors::region_bytes(),
        );
        #[cfg(feature = "debug-log")]
        println!(
            "ustack  : Tiefensonde (Sprechprobe des GEMESSENEN PFADES) -- gemessen={_s_gemessen} \
             getroffen={_s_ok} gemeldete Tiefe={_s_tiefe} B gegen beruehrte {} B (+{} B Schlupf fuer \
             ihren eigenen Rahmen). Sie prueft, was die Eichung strukturell NICHT kann: dass die \
             Buchfuehrung Thread-Slot -> Region auf die RICHTIGE Region zeigt. Ein Eintrag, der \
             auf irgendeine andere genullte Region zeigt, meldet 'viel Luft' und bestuende jede \
             Eichung -- genau die Verwechslung, die die alte Kapazitaetszeile entwertet hat",
            crate::userstackmark::SONDE_TIEFE,
            crate::userstackmark::SONDE_SCHLUPF,
        );
        let (_s_pfad, _s_zweit, _s_res, _s_gr) = crate::userstackmark::summe();
        #[cfg(feature = "debug-log")]
        println!(
            "ustack  : SUMME (C7b, strukturell statt statistisch) -- tiefster Pfad {_s_pfad} B + \
             zweiter Summand {_s_zweit} B + geforderte Reserve {_s_res} B = {} B von {_s_gr} B \
             (kleinste Region). **Der zweite Summand ist auf EL0 NULL, und das ist eine Aussage \
             ueber die Architektur, keine Bequemlichkeit**: ein Interrupt oder eine Exception aus \
             Ring 3 wechselt IMMER den Stack (x86-64 laedt RSP0 aus der TSS, aarch64 laeuft auf \
             SP_EL1) -- unterhalb des User-RSP legt niemand etwas ab. Das Gegenstueck des \
             EL1-Stacks (Handler auf DEMSELBEN Stack) existiert hier nicht. Der einzige \
             Mechanismus, der das aendern wuerde, sind Signal-Handler auf dem User-Stack; die gibt \
             es in diesem System nicht. **VORBEHALT zum ERSTEN Summanden**: gemessen wird ueber \
             die Nullung, ein mit NULL beschriebenes Stackwort ist also unsichtbar -- die Zahl ist \
             eine UNTERGRENZE, und deshalb liegt die Regionsgroesse ein Vielfaches darueber und \
             nicht knapp daneben",
            _s_pfad + _s_zweit + _s_res
        );
        println!(
            "ustack  : {} (C7b: geforderte Mindestreserve {} B = 1/{} der kleinsten Region, \
             Eichung {:#06b}/{:#06b}, mindestens {} Messungen vor dem Gatter)",
            if crate::userstackmark::urteil() { "ALL PASS" } else { "FAILURES" },
            crate::userstackmark::mindestreserve(if u.groesse_min == usize::MAX {
                0
            } else {
                u.groesse_min
            }),
            crate::userstackmark::MIND_RESERVE_NENNER,
            crate::userstackmark::eichstand(),
            crate::userstackmark::EICH_ALLE,
            crate::userstackmark::MIND_MESSUNGEN,
        );
    }

    // --- C9: DIE SPERRHALTEDAUER ----------------------------------------------------------------
    //
    // Steht direkt hinter `kstack`, weil beide dieselbe Bauform haben und dieselbe Klasse Fehler
    // fangen: eine Groesse, die keine Pruefzeile ansieht, bis sie jemand zu einer Zahl macht. Die
    // eine misst, wieviel Stack ein tiefer Pfad verbraucht; diese, wie lange er die Praemption
    // aufhaelt.
    crate::sperrmark::bericht();

    // **C9b: die Schreibordnung der Konsole** -- die Behebung des groessten der drei Befunde, und
    // die Zahl, an der ihr eigenes Risiko haengt. Steht direkt hinter `sperre`, weil sie dessen
    // dritten Schuldposten ersetzt: dort stand die Zahl, hier steht, was an ihre Stelle getreten
    // ist.
    crate::sperrmark::konsole_bericht();

    // **Der Unterbau unter der Guard-Page**: per-Kern-TSS + IST-Stacks fuer #DF/NMI/#MC.
    // Steht direkt hinter `kstack`, weil beide Zeilen dieselbe Gefahr behandeln -- die eine misst,
    // wieviel Luft ein Kernel-Stack noch hat, die andere sorgt dafuer, dass sein Ueberlauf
    // ueberhaupt jemand melden kann.
    super::ist::bericht();

    // --- C8: der VERIFIZIERERTHREAD und seine benannte Absage -----------------------------------
    //
    // Steht direkt hinter `kstack`, weil die beiden Zeilen dieselbe Groesse von zwei Seiten
    // beschreiben: die eine misst, was auf den Stacks noch passiert, die andere sagt, wohin der
    // tiefste Pfad gewandert ist.
    {
        let s = crate::verifizierer::stand();
        println!(
            "verif   : Verifiziererthread laeuft={} · angenommen={} bearbeitet={} \
             abgewiesen(ERR_LOAD_BUSY)={} ohne-Thread(ERR_SERVER_GONE)={} \
             Antwort-ins-Leere={} Hoechststand={}/{} wartend={} verloren={} (muss 0 sein)",
            s.laeuft,
            s.angenommen,
            s.bearbeitet,
            s.abgewiesen,
            s.ohne_thread,
            s.antwort_ins_leere,
            s.hoechststand,
            crate::verifizierer::AUFTRAEGE_MAX,
            s.wartend,
            s.verloren,
        );
        match crate::verifizierer::sondenbild() {
            None => println!(
                "verif   : FAILURES (die Absage wurde NICHT gefahren -- eine Schranke, die nie \
                 erreicht wurde, ist von einer fehlenden nicht zu unterscheiden; genau das war D11)"
            ),
            Some(b) => {
                println!(
                    "verif   : Absage gefahren -- {} Sonden gegen eine Schranke von {}: \
                     abgewiesen={} bedient={} · beim Beobachten wegen LOAD blockiert={} \
                     (Positivkontrolle: ohne einen einzigen Wartenden waere 'der Ueberlaeufer \
                     wartet nicht' trivial wahr) · Ueberlaeufer NICHT blockiert={} und danach \
                     WEITERGELAUFEN={} (am Rundenzaehler gemessen, nicht an einem Zustandsbit) · \
                     Fuellstand erreichte {}/{}",
                    b.gestartet,
                    crate::verifizierer::AUFTRAEGE_MAX,
                    b.abgewiesen,
                    b.bedient,
                    b.blockiert,
                    b.ueberlaeufer_frei,
                    b.ueberlaeufer_laeuft,
                    b.hoechststand,
                    crate::verifizierer::AUFTRAEGE_MAX,
                );
                println!(
                    "verif   : {} (C8: `SYS_LOAD` verifiziert nicht mehr auf dem 16-KiB-Stack des \
                     Aufrufers, sondern auf dem eigenen des Verifizierers. Der Aufrufer blockiert \
                     mit dem EIGENEN Grund LOAD in der Grund-Menge (Z24) -- kein `resume`, kein \
                     `unpark`, kein fremdes `reply` weckt ihn, nur die Fertigmeldung. Die \
                     Serialisierung ist ein DoS-Kanal und hat deshalb eine Schranke MIT NAMEN; der \
                     Ueberlaeufer bleibt lauffaehig statt blockiert liegenzubleiben)",
                    if b.ok() { "ALL PASS" } else { "FAILURES" },
                );
            }
        }
    }

    let ticks = hal::timer::ticks(0);
    let mut all_tick = true;
    for c in 0..system::num_cores() {
        // Z6 Stufe 1b: Abwesenheit mit Grund ist keine fehlende Messung (Analogon ARM-Seite).
        if !caprock_sched::core_online(c) {
            println!("sched   : core {c} suppressed (SMT-Stufe-1: nie gebootet, kein Tick erwartet)");
            continue;
        }
        let t = hal::timer::ticks(c);
        println!("sched   : core {c} ticks={t}");
        all_tick &= t > 0;
    }
    let rounds: [u64; NWORKERS] = core::array::from_fn(|i| WORKER_ROUNDS[i].load(Ordering::Relaxed));
    println!(
        "sched   : Worker-Runden {rounds:?} (jeder >= {WORK_TARGET} -> Timer verdraengt sie gegeneinander)"
    );
    let sched_ok = ticks > 0 && all_tick && rounds.iter().all(|&r| r >= WORK_TARGET);
    println!("sched   : {}", if sched_ok { "ALL PASS" } else { "FAILURES" });

    let ipc = IPC_RESULT.load(Ordering::Acquire);
    // **Der Ausgang des Servers gehoert neben das Ergebnis des Clients.** Ohne ihn sagt
    // `-> 0 (erwartet 42)` nur, DASS nichts ankam. D0-Diagnose, s. `IPC_SERVER_EXIT`.
    {
        let ex = IPC_SERVER_EXIT.load(Ordering::Acquire);
        let name = match ex {
            u64::MAX => "nein (noch in der Schleife)",
            1 => "ja, Code 1 = ERR_BADCAP (Startrennen: RECV vor der Cap)",
            2 => "ja, Code 2 = ERR_BADSYS",
            3 => "ja, Code 3 = ERR_RIGHTS",
            4 => "ja, Code 4 = ERR_NOPD",
            5 => "ja, Code 5 = ERR_SERVER_GONE",
            8 => "ja, Code 8 = ERR_QUIESCING",
            9 => "ja, Code 9 = ERR_EP_FULL",
            // Ein unbekannter Code darf nicht als bekannter durchgehen -- die Zahl steht dabei.
            _ => "ja, ANDERER Code",
        };
        println!(
            "ipc     : Server bediente {} Anfrage(n); Schleife verlassen: {name} (roh {ex})",
            IPC_SERVER_BEDIENT.load(Ordering::Relaxed)
        );
    }
    println!("ipc     : CALL(21) ueber Endpoint-Cap -> {ipc} (erwartet 42)");
    // **Die Gelegenheit zaehlen, nicht den Treffer** (D0). Der Fehler selbst trat in 0,018 % der
    // Laeufe auf -- eine Zeile, die nur dann spricht, ist in 5555 von 5556 Laeufen stumm und taugt
    // als Waechter nicht. Die REIHENFOLGE dagegen ist in jedem Lauf pruefbar: wird eine PD an einen
    // Thread gebunden, der schon laufen darf, war das Rennen offen -- ob es diesmal getroffen hat
    // oder nicht.
    //
    // `unklar` steht daneben, weil „nicht auflösbar" kein „rechtzeitig" ist: ein Thread, der beim
    // Binden schon gestorben ist, faellt hier hinein. Eine Null in beiden Spalten ist die Aussage;
    // eine Null in der ersten allein waere eine halbe.
    {
        let spaet = system::LATE_PD_BIND.load(Ordering::Relaxed);
        let unklar = system::LATE_PD_BIND_UNKLAR.load(Ordering::Relaxed);
        let gesamt = system::PD_BIND_GESAMT.load(Ordering::Relaxed);
        let _gesehen = system::SPAETBINDUNG_GESEHEN.load(Ordering::Relaxed);
        #[cfg(feature = "debug-log")]
        println!(
            "pdbind  : gebunden={gesamt} spaet-gebunden={spaet} unklar={unklar} \
             erklaert-gefeuert={_gesehen:#x} (D0: eine PD, die an einen bereits zugelassenen Thread \
             geht, kommt zu spaet -- der Thread kann seinen ersten Syscall schon gemacht und \
             ERR_NOPD bekommen haben. Erklaerte Gruende zaehlen NICHT als spaet, stehen aber hier)"
        );
        #[cfg(feature = "debug-log")]
        for g in system::ERLAUBTE_SPAETBINDUNGEN {
            println!("pdbind  :   erklaert zulaessig: {g}");
        }
        // **Sprechprobe am gepruefte Pfad, nicht an einer Ausnahme darin.** `gebunden == 0` heisst:
        // dieser Lauf ist an `bind_pd` gar nicht vorbeigekommen -- dann sagt eine Null bei `spaet`
        // nichts. Nicht `erklaert-gefeuert` abfragen: das ist ein Sonderfall, den es auf aarch64
        // nicht gibt und den eine kuenftige Verbesserung wegnehmen darf.
        println!(
            "pdbind  : {}",
            if gesamt == 0 {
                "FAILURES (NICHT SPRECHFAEHIG: in diesem Lauf wurde keine einzige PD gebunden)"
            } else if spaet == 0 && unklar == 0 {
                "ALL PASS"
            } else {
                "FAILURES"
            }
        );
    }
    println!("ipc     : {}", if ipc == 42 { "ALL PASS" } else { "FAILURES" });

    let syscalls = USER_SYSCALLS.load(Ordering::Relaxed);
    let faults = system::el0_fault_count();
    println!("ring3   : Ring-3-Thread machte {syscalls} Syscalls; abgefangene Ring-3-Faults: {faults}");
    let ring3_ok = syscalls > 0 && faults > 0 && system::el0_syscall_seen();
    println!(
        "ring3   : {} (Ring-3-Thread laeuft + syscallt; Zugriff auf Kernel-Speicher faultet, Thread beendet, Kernel laeuft weiter)",
        if ring3_ok { "ALL PASS" } else { "FAILURES" }
    );

    // A-3.4: wie nah kam der globale Cap-Space seiner Grenze? Der Hoechststand, nicht der
    // Endstand -- s. `system::cap_peaks`. Gemeldet wird auch, wie viele PDs mit VOLLEM Budget
    // ueberhaupt hineinpassen: `CAP_BUDGET_PER_PD` deckelt den Verbrauch EINER PD, die Summe
    // traegt seit A-3.4 die Dimensionierung (`CAP_SLOTS_TOTAL`). Nachgezaehlt wird sie von
    // `capsum` weiter unten.
    let (pslots, cslots, pobjs, cobjs) = system::cap_peaks();
    let pd_voll = cslots / caprock_microkit::CAP_BUDGET_PER_PD;
    println!(
        "capsz   : Cap-Slots Hoechststand {pslots}/{cslots}, Objekte {pobjs}/{cobjs}; bei vollem \
         Budget ({} Slots/PD) passen {pd_voll} PDs in die globale Tabelle",
        caprock_microkit::CAP_BUDGET_PER_PD
    );
    let capsz_ok = pslots < cslots && pobjs < cobjs;
    println!(
        "capsz   : {} (A-3.4: der globale Cap-Space wurde NICHT erschoepft -- gemessen am \
         Hoechststand, nicht am Endstand)",
        if capsz_ok { "ALL PASS" } else { "FAILURES" }
    );

    // A-3.4 Abschluss: die SUMME. Das Budget deckelt eine PD; dass daneben noch die Wurzel-Caps
    // des Kernels in die Tabelle passen, war eine Zahl (256) mit einem Kommentar daneben. Hier
    // wird sie nachgezaehlt -- nicht durch Abziehen (zwei PDs koennen denselben CapPtr halten,
    // dann zaehlt die Summe zu hoch und die Pruefung ginge faelschlich durch), sondern durch
    // Markieren jedes von einer PD gehaltenen Slots.
    let (nonbudget, reserve, sum_ok) = system::cap_nonbudget_slots();
    if nonbudget == usize::MAX {
        println!("capsum  : Summenpruefung KONNTE NICHT LAUFEN (Zaehlflaeche zu klein)");
    } else {
        println!(
            "capsum  : {nonbudget}/{reserve} Slots ausserhalb aller PD-Budgets (Kernel-Wurzelcaps); \
             Kapazitaet {cslots} = {} PD-Budgets + {reserve} Reserve",
            caprock_microkit::CAP_SLOTS_FOR_ALL_PDS
        );
    }
    println!(
        "capsum  : {} (A-3.4: der Kernel bleibt in seiner Reserve -- sonst bekommt eine PD \
         INNERHALB ihres Budgets keinen Slot mehr)",
        if sum_ok { "ALL PASS" } else { "FAILURES" }
    );

    let sas = SAS_READ_OK.load(Ordering::Relaxed);
    let isof = system::iso_fault_count();
    println!(
        "iso     : SAS-Thread las {sas:#x} (erwartet {PROBE_MAGIC:#x}); isolierter Thread faultete {isof}x an derselben Adresse"
    );
    let iso_ok = sas == PROBE_MAGIC && isof > 0;
    println!(
        "iso     : {} (eigener Adressraum je PD: dieselbe Adresse ist fuer SAS lesbar, fuer die isolierte PD nicht)",
        if iso_ok { "ALL PASS" } else { "FAILURES" }
    );

    let b = root_badge();
    // ---- Z15/W1: WASM in einer PD -------------------------------------------------------------
    //
    // Die vier Bits kommen aus `programs/userland/wasmhost`, ueber eigen gebadgte `ccopy`-Kopien
    // derselben Notification (dasselbe Muster wie `init` bei A-3.1). Sie stehen fuer VIER
    // verschiedene Aussagen, und der Unterschied ist der Punkt:
    //
    //   * `inst`   -- ein Modul wurde geladen und instanziiert
    //   * `wert`   -- die exportierte Funktion lieferte den GERECHNETEN Wert (42 steht im Modul,
    //                 nicht im Programm) -- ohne das waere `inst` eine Aussage ueber eine Engine,
    //                 die startet, und keine ueber Ausfuehrung
    //   * `mutation` -- ein Modul mit EINEM verdorbenen Opcode wurde abgewiesen; ein Interpreter,
    //                 der jedes Byte ausfuehrt, ist keine Sandbox
    //   * `trap`   -- ein Zugriff hinter das Ende des Linearspeichers ergab einen WASM-Trap.
    //                 **Dieses Bit belegt sich selbst:** haette der Trap die PD mitgerissen,
    //                 koennte sie ihn nicht mehr melden. Die Sandbox haelt also INNERHALB der PD,
    //                 und die PD-Isolation ist die zweite Linie, nicht die erste.
    // **Das Badge der CLIENT-Notification, nicht das des Root-Tasks.** wasmhost leitet seine
    // gebadgten Kopien von seiner EIGENEN, aus dem Manifest endowten Cap ab (Slot 1, RWX) --
    // die vom Root-Task delegierte Cap in Slot 0 ist eine reine Signal-Cap, und `ccopy` kann
    // Rechte nicht verstaerken.
    let w_ntfn = crate::loader::client_notification_of(TEST_WASM_PROGRAM_ID);
    // **`wb`, nicht `b`.** Bis zum 2026-08-10 hiess diese Variable `b` -- und ueberdeckte damit
    // `let b = root_badge()`, das dreissig Zeilen weiter unten von der `root`-Zeile gelesen wird.
    // Seit `a159b6b` druckte die Root-Zeile also das Badge der CLIENT-Notification. Daraus wurde
    // ein „Root-Task lief: false" (obwohl `pdcolor`/`ladepol` belegen, dass er lief und lud), ein
    // vermeintlicher Kippunkt im Bisect und eine Hypothese ueber Cap-Fehlbindungen -- alles aus
    // einer verdeckten Variablen. Ein Name, der zweimal vorkommt, ist teurer als ein langer.
    let wb = w_ntfn.map(system::notification_pending).unwrap_or(0);
    let (w_inst, w_wert, w_mut, w_trap) = (
        wb & WASM_INST != 0,
        wb & WASM_RESULT != 0,
        wb & WASM_REJECT != 0,
        wb & WASM_TRAP != 0,
    );
    let (w_lebt, w_ccopy) = (wb & WASM_LEBT != 0, wb & WASM_CCOPY != 0);
    // **Den SCHEDULER fragen, nicht eine Cap.** `SIGNAL` ist selbst eine Cap-Invokation -- die
    // frühere Formulierung „ohne jede Cap-Operation" war schlicht falsch, gemeint war „ohne
    // `ccopy`". Die Unterscheidung ist genau die, um die es geht, und sie erledigt sich nicht
    // durch eine Sonde, die über eine Cap meldet.
    //
    // Diese Auskunft benutzt **keinen** Cap-Pfad: existiert der Thread, ist er zugelassen, worin
    // blockiert er? Damit zerfällt „lebt nicht" in seine zwei Hälften -- *läuft nie an* gegen
    // *läuft, und das Signal versandet*.
    // **Drei Lagen, nicht zwei.** „Kein Registereintrag" und „Eintrag da, Thread nicht mehr
    // aufloesbar" sind verschiedene Aussagen -- die erste heisst „nie geladen", die zweite „lief
    // und ist gestorben". Sie in einen Wert zu werfen waere genau der Fehler, den dieser Bericht
    // heute dreimal gefunden hat.
    let w_reg = crate::loader::thread_of_program(TEST_WASM_PROGRAM_ID);
    let (w_thread, w_adm, w_gruende) = match w_reg {
        Some(t) => system::thread_lage(t),
        None => (false, false, 0),
    };
    println!(
        "wasm    : endowt={} lebt={w_lebt} ccopy-geht={w_ccopy} | instanziiert={w_inst} \
         Ergebnis-stimmt={w_wert} Mutation-abgewiesen={w_mut} Uebergriff-getrappt={w_trap}. \
         **`lebt` braucht keine Cap-Operation** (SIGNAL auf die eigene Manifest-Cap) und steht vor \
         allem anderen. **`SIGNAL` ist aber selbst eine Cap-Invokation** -- „lebt\" trennt also \
         `ccopy` ab, nicht den Cap-Pfad. Die cap-freie Auskunft steht in der Zeile darunter",
        w_ntfn.is_some()
    );
    println!(
        "wasm    : Scheduler-Auskunft (OHNE Cap-Pfad): Thread existiert={w_thread} \
         im-Register={} zugelassen={w_adm} Grund-Bits={w_gruende:#06b} (1=IPC 2=BUDGET 4=PAUSE 8=PARK, 0=lauffaehig). \
         **Das ist die Trennung**: existiert er nicht oder ist er nicht zugelassen, ist es ein \
         Lader-/Scheduler-Problem; laeuft er und sein Signal kommt trotzdem nicht an, ist es der \
         Cap-Pfad. Eine Meldung UEBER eine Cap kann diese Frage nicht beantworten -- sie benutzt \
         genau den Pfad, der in Frage steht. Register leer = **nie geladen**; Register besetzt und \
         Thread weg = **lief und ist gestorben** -- zwei Aussagen, ein Wert waere hier derselbe \
         Fehler, den dieser Bericht heute schon dreimal gefunden hat",
        w_reg.is_some()
    );
    if w_ntfn.is_none() {
        println!(
            "wasm    : SKIP -- es ist wirklich KEINE WASM-PD endowt (kein `wasmhost` in der \
             Startmenge). **Entschieden an der Endowment-Tabelle, nicht am Schweigen**: bis zum \
             2026-08-10 stand hier „kein Bit gesetzt -> nicht anwendbar\", und damit war eine PD, \
             die laeuft und nichts meldet, von einer, die es nicht gibt, nicht zu unterscheiden"
        );
    } else {
        println!(
            "wasm    : {} (Z15/W1: eine WASM-Laufzeit als gewoehnliche UserLand-PD. Die Engine ist \
             216 KiB Code -- so gross wie der ganze Mikrokern --, liegt aber HIER und nicht dort: \
             fuer jeden anderen Mandanten waechst die TCB um null. Der Uebergriffsfall ist der \
             eigentliche Beleg, und er belegt sich selbst: das Badge kommt nur an, wenn die PD \
             den Trap ueberlebt hat)",
            if w_inst && w_wert && w_mut && w_trap { "ALL PASS" } else { "FAILURES" }
        );
    }
    // **Der Name sagt jetzt, was gemessen wird.** „Root-Task lief" war falsch: gemessen wird die
    // ANKUNFT eines Badges, nicht Leben. Dass Root laeuft und laedt, belegen `pdcolor` und
    // `ladepol` unabhaengig -- und trotzdem hat diese Zeile unter ihrem alten Namen eine ganze
    // Fehlspur getragen. Vierte Instanz derselben Form an einem Tag: ein Pruefer, der etwas
    // anderes misst, als er behauptet.
    let rb = root_badge();
    println!(
        "root    : Notification-Badge {rb:#x} (root-Badge angekommen: {}; hello-Badge angekommen: {})",
        rb & ROOT_BADGE != 0,
        rb & HELLO_BADGE == HELLO_BADGE
    );
    println!(
        "root    : {} (A-2.1: ein extern gebautes, aus dem signierten Manifest ausgewaehltes Programm laeuft -- und laedt seinerseits ueber SEINE Loader-Cap ein weiteres)",
        if root_chain_done() { "ALL PASS" } else { "FAILURES" }
    );
    println!(
        "cdelete : Loader-Cap geloescht und Autoritaet danach weg: {}; Cap mit abgeleiteten Kopien abgewiesen und weiter benutzbar: {}",
        b & CDELETE_GONE_BADGE != 0,
        b & CDELETE_CHILDREN_BADGE != 0
    );
    println!(
        "cdelete : {} (A-3.1: SYS_CDELETE aus Ring 3 -- beide Ausgaenge belegt, nicht nur der erfolgreiche)",
        if cdelete_done() { "ALL PASS" } else { "FAILURES" }
    );

    // --- Tagged IPC capabilities, end to end ----------------------------------------------------
    //
    // Measured at the client's notification (per program, like `wasm`): the client reports six
    // facts, each only after the SERVER answered with what the kernel delivered. Not part of
    // `all_done()` -- same reasoning as `wasm`: a gate on badges that may be late would turn a
    // slow run into a watchdog. The suite gates on this line instead.
    match crate::loader::client_notification_of(TEST_IPCPERM_PROGRAM_ID) {
        None => println!(
            "ipcperm : SKIP -- no ipcperm-cli in the boot set (decided at the endowment table, \
             not by silence)"
        ),
        Some(n) => {
            let ib = system::notification_pending(n);
            let facts = [
                ("untagged-unchanged", ib & IPCPERM_UNTAGGED != 0),
                ("tagged-mask-and-id-delivered", ib & IPCPERM_TAGGED != 0),
                ("derivation-only-shrinks", ib & IPCPERM_ATTENUATED != 0),
                ("no-dup-no-copy", ib & IPCPERM_DUP_GATED != 0),
                ("wide-id-refused", ib & IPCPERM_WIDE_ID != 0),
                ("only-endpoints-taggable", ib & IPCPERM_NOT_ENDPOINT != 0),
            ];
            print!("ipcperm : badge {ib:#x}");
            for (name, ok) in facts {
                print!(" {name}={ok}");
            }
            println!();
            println!(
                "ipcperm : {} (tagged IPC capabilities: the server received the permission mask \
                 and id the kernel wrote into the badge word -- unchanged for untagged caps, \
                 shrinking only along a derivation, DUP-gated, 48-bit id, endpoints only)",
                if facts.iter().all(|&(_, ok)| ok) { "ALL PASS" } else { "FAILURES" }
            );
        }
    }

    // --- A-5.1: hat ein Treiber IM USERLAND sein Geraet bedient? -------------------------------
    //
    // Zwei Aussagen, absichtlich aus zwei Quellen:
    //
    //  1. **Der Treiber meldet**, dass seine Transaktion durchlief (Badge in seiner endowten
    //     Notification). Das kann nur er wissen -- er hat den used-Ring gepollt.
    //  2. **Der Kernel prueft die Bytes** in der Region, die er ihm ausgegeben hat. Das kann nur
    //     er, denn er kennt die Magie des Abbilds.
    //
    // Keine der beiden allein genuegt. Meldet nur der Treiber, glaubt man dem Code, der es
    // behauptet. Pruefen nur die Bytes, koennte sie auch jemand anders geschrieben haben.
    if let Some((dma_phys, iova)) = system::driver_assignment(TEST_BLK_PROGRAM_ID) {
        // **Die Notification DIESES Dienstes** (A-5.4). `driver_notification()` ist ein einzelnes
        // Register: die zuletzt geladene Treiber-PD ueberschreibt es. Mit zwei Treibern las der
        // Bericht deshalb die Ablage der NETZ-PD und sah dort das v2-Badge des Blockdienstes
        // nicht -- `v2 meldete bereit=0` bei einem Austausch, der sauber gelaufen war.
        // Dieselbe Falle wie 2026-08-01 zwischen Root-Task, Treiber und Client, nur eine Rolle
        // weiter: Rollen, die sich melden, brauchen getrennte Ablagen.
        let badge = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID)
            .map(|s| system::notification_pending(s.ntfn))
            .unwrap_or(0);
        let v1_bereit = badge & crate::loader::DRIVER_NTFN_BADGE != 0;
        let v2_bereit = badge & crate::loader::DRIVER_V2_BADGE != 0;
        // **Der Puffer eines Treibers gehoert dem LETZTEN Client, nicht der Aussage.**
        //
        // Hier stand bis Z4 Stufe 2 ein Lesezugriff im Bericht -- und damit maass die Zeile nicht,
        // was der Blockdienst geliefert hat, sondern was zuletzt durch seinen Datenpuffer ging.
        // Solange nur `drv_client` ihn benutzte, fiel das nicht auf; sein letzter Schritt war
        // ausdruecklich so gelegt, dass die Magie am Ende dort steht (s. Schritt 5 in
        // `drv_client`). Mit einem ZWEITEN Client (dem Checkpoint) stand dort dessen Sektor, und
        // die Zeile meldete `FAILURES` fuer einen Treiber, der alles richtig gemacht hatte.
        //
        // Der Wert wird deshalb **erfasst, wenn die Aussage gilt** (Ende der Dienstfolge,
        // `drv_service_step` Schritt 2) und im Bericht nur noch gelesen. Der Momentanwert wird
        // daneben gedruckt: er ist Diagnose, kein Kriterium.
        // SAFETY: `dma_phys` ist eine vom Kernel ausgeschnittene, identity-gemappte DMA-Region.
        let jetzt = unsafe {
            core::ptr::read_volatile((dma_phys + hal::virtio::blk::OFF_DATA) as *const u64)
        };
        let erfasst = DRV_DMA_WORT.load(Ordering::Acquire);
        let wort = if erfasst != u64::MAX { erfasst } else { jetzt };
        let g = |a: &[core::sync::atomic::AtomicU64; 4]| {
            [
                a[0].load(Ordering::Acquire),
                a[1].load(Ordering::Acquire),
                a[2].load(Ordering::Acquire),
                a[3].load(Ordering::Acquire),
            ]
        };
        let (r1, r2) = (g(&DRV_R1), g(&DRV_R2));
        let reload = DRV_RELOAD.load(Ordering::Acquire);
        println!(
            "drv     : Zuteilung: DMA-Region {dma_phys:#x}, Geraetesicht {iova:#x}; \
             Sektor {wort:#018x} (erwartet {BLK_MAGIC:#018x}; Puffer jetzt {jetzt:#018x} -- er \
             gehoert dem letzten Client und ist deshalb kein Kriterium)"
        );
        println!(
            "drv     : Anfrage 1 an v1: Status={} Bytes={:#018x} Sektoren={} bedient={}",
            r1[0], r1[1], r1[2], r1[3]
        );
        println!(
            "drv     : Austausch: Ergebnis={} (0=umgebunden OHNE Empfaengerluecke, 1=umgebunden, \
             2=kein Dienst, 3=nicht geladen, 4=nicht bereit, 5=abgewiesen); v1 meldete bereit={} \
             v2 meldete bereit={}",
            reload, v1_bereit as u8, v2_bereit as u8
        );
        println!(
            "drv     : Anfrage 2 an v2: Status={} Bytes={:#018x} Sektoren={} bedient={}",
            r2[0], r2[1], r2[2], r2[3]
        );
        let q = |i: usize| DRV_SEQ[i].load(Ordering::Acquire);
        println!(
            "blkdev  : INFO Status={} Kapazitaet={} Sektorgroesse={}; READ(0)={} \
             WRITE({PROBE_SECTOR})={} FLUSH={}; Rueckgelesen={:#018x} (erwartet {BLK_MAGIC:#018x}); \
             READ(jenseits der Platte)={} (erwartet 3 = Bereich)",
            q(0), q(1), q(2), q(3), q(4), q(5), q(6), q(7)
        );
        // A-6.3: was die Dateisystem-PD in die geteilte Flaeche gelegt hat.
        if let Some((shared_phys, _)) = system::driver_shared_region(TEST_BLK_SERVICE_ID) {
            // SAFETY: identity-gemappte, vom Kernel ausgeschnittene Region; nur lesend.
            let f = |i: u64| unsafe {
                core::ptr::read_volatile((shared_phys + 4096 + i * 8) as *const u64)
            };
            println!(
                "fs      : Status={} (0=Datei gelesen, 2=nicht gefunden, 3=Kette defekt, \
                 4=Kette zu kurz, {:#x}=PD lief nicht); Groesse={} erste acht Byte={:#018x} \
                 Cluster={}",
                f(0), u64::MAX, f(1), f(2), f(3)
            );
            println!(
                "fs      : Schreiben Status={} (0=gut, 10..24=Schritt der schieflief); \
                 Rueckgelesen Status={} Groesse={} (erwartet {FS_NEU_GROESSE}) geprueft={} Byte",
                f(4), f(5), f(6), f(7)
            );
            let ok = f(0) == 0
                && f(1) == FS_DATEI_GROESSE
                && f(2) == FS_ERSTE_ACHT
                // A-6.4: geschrieben, und beim Zurueckhlesen stimmte JEDES Byte.
                && f(4) == 0
                && f(5) == 0
                && f(6) == FS_NEU_GROESSE
                && f(7) == FS_NEU_GROESSE;
            println!(
                "fs      : {} (A-6.3: ein lesendes Dateisystem als EIGENE PD -- sie faehrt kein \
                 Geraet, sondern ruft den Blockdienst ueber dessen Kanal und liest die Bytes aus \
                 der geteilten Uebertragungsflaeche. GPT und FAT16 liegen in kernfreien Crates \
                 mit forbid(unsafe_code); der Kern kennt weder Partitionen noch Dateien. A-6.4: \
                 sie SCHREIBT auch -- ueber einen zweiten Cluster hinaus, in beide FAT-Kopien, \
                 mit Flush, und liest jedes Byte zurueck)",
                if ok { "ALL PASS" } else { "FAILURES" }
            );
            FS_OK.store(ok, Ordering::Release);
        }
        let blk_ok = BLKDEV_OK.load(Ordering::Acquire);
        println!(
            "blkdev  : {} (A-6.1: das Dienstprotokoll traegt -- Auskunft, Lesen, SCHREIBEN, \
             Flush, und ein Sektor jenseits der Platte wird ABGEWIESEN statt ans Geraet \
             durchgereicht. Der Rueckleseschritt ist der eigentliche Beleg: eine quittierte \
             Schreibanfrage ist eine Quittung, keine Daten)",
            if blk_ok { "ALL PASS" } else { "FAILURES" }
        );
        // Bei einem Fehlschlag traegt `msg[1]` den GRUND, nicht die Eintragszahl. Ein Feld, das
        // je nach Status etwas anderes bedeutet, muss auch je nach Status anders beschriftet
        // werden -- sonst steht im Log "belegte Eintraege=5", wo eine kaputte Kopf-Pruefsumme
        // gemeint ist.
        if q(8) == 0 {
            println!(
                "part    : GPT-Scan Status=0 belegte Eintraege={} (erwartet \
                 {ERWARTET_PARTITIONEN}); erste Partition LBA {} ueber {} Sektoren (erwartet \
                 {ERWARTET_ERSTE_LBA} / {ERWARTET_ERSTE_SEKTOREN})",
                q(9), q(10), q(11)
            );
        } else {
            println!(
                "part    : GPT-Scan Status={} ABGEWIESEN, Grund={} (1=zu kurz 2=Signatur \
                 3=Revision 4=Kopfgroesse 5=Kopf-CRC 6=Eintragsgroesse 7=Eintragszahl \
                 8=Eintrags-CRC)",
                q(8), q(9)
            );
        }
        println!(
            "part    : {} (A-6.2: die Partitionstabelle wird im BLOCKDIENST gelesen, nicht im \
             Kern -- `caprock-part` ist abhaengigkeitsfrei, ohne unsafe und host-getestet. \
             Geprueft werden beide Pruefsummen; die Eintragsliste passt nicht in eine Anfrage und \
             wird stueckweise gelesen, die Pruefsumme aber ueber das GANZE gebildet)",
            if PART_OK.load(Ordering::Acquire) { "ALL PASS" } else { "FAILURES" }
        );
        // Was hier zusammenkommen MUSS, und warum jedes Stueck einzeln zaehlt:
        //  * beide Anfragen liefen und lieferten die Magie  -> der Dienst bedient wirklich
        //  * `bedient` zaehlte 1 -> 2                       -> v2 hat die REGION geerbt, nicht
        //                                                      eine frische bekommen
        //  * v1 UND v2 meldeten sich bereit                 -> es waren zwei Fassungen
        //  * Austausch ohne Empfaengerluecke                -> A-4.1 am echten Dienst, nicht am
        //                                                      lokalen Objekt
        //  * IOVA != PA                                     -> die Gerätesicht ist eine eigene Achse
        let ok = r1[0] == 0
            && r2[0] == 0
            && r1[1] == BLK_MAGIC
            && r2[1] == BLK_MAGIC
            // **Relativ, nicht absolut.** Vor dieser Folge hat die Dateisystem-PD den Dienst
            // schon benutzt; ein fester Startwert waere eine Aussage ueber die Reihenfolge
            // aller Clients statt ueber den Austausch.
            && r1[3] >= 1
            && r2[3] == r1[3] + 1
            && v1_bereit
            && v2_bereit
            && reload == 0
            && wort == BLK_MAGIC
            && iova != 0
            && iova != dma_phys;
        println!(
            "drv     : {} (A-5.1: ein Treiber als DIENST ausserhalb des Kerns -- er loest sein \
             Geraet auf seiner EIGENEN Konfigurationsraum-Seite auf, wartet auf Anfragen und \
             liest Sektoren per Bus-Master-DMA. Der Kern hat enumeriert, zugeteilt und den \
             Empfaenger AUSGETAUSCHT -- ohne einen virtio-Schritt und ohne zu wissen, was der \
             Dienst treibt. Der Zaehler 1 -> 2 belegt, dass die neue Fassung dieselbe Region \
             geerbt hat)",
            if ok { "ALL PASS" } else { "FAILURES" }
        );
        DRIVER_OK.store(ok, Ordering::Release);
    } else {
        println!(
            "drv     : SKIP (keine Zuteilung erfolgt -- entweder war kein Geraet da, oder es \
             gibt keine Treiber-PD, die eines anfordert: ohne Boot-Archiv laeuft kein geladenes \
             Programm, und ohne Manifest-Eintrag mit mmio,dma verlangt keines Geraete-Autoritaet)"
        );
        DRIVER_OK.store(true, Ordering::Release); // nicht anwendbar, nicht bestanden
        println!("blkdev  : SKIP (kein Treiber-Dienst -- s. die Begruendung eine Zeile darueber)");
        println!("part    : SKIP (kein Treiber-Dienst -- s. die Begruendung zwei Zeilen darueber)");
        println!("fs      : SKIP (kein Treiber-Dienst -- s. die Begruendung drei Zeilen darueber)");
    }
    // --- A-5.4: die eigentliche Mandantenaussage -------------------------------------------
    //
    // Drei Zahlen, und keine davon reicht allein:
    //   * die **Positivkontrolle** (`OP_SELF`) muss getragen haben -- sonst hiesse "nichts kam an"
    //     nur, dass nichts lief;
    //   * der **Fremdversuch** darf keine Daten geliefert haben;
    //   * das **Opfer** darf sich nicht geaendert haben -- und das sieht nur, wer beim Opfer
    //     nachschaut. Dass der Angreifer "nichts bekommen" meldet, ist eine Aussage ueber ihn.
    //
    // Die VT-d-Faults sind Beiwerk, kein Kriterium: ein Geraet, dessen Uebersetzung fehlschlaegt,
    // MUSS keinen Fault erzeugen (der DMAR-Remapping-Modus darf still verwerfen). Sie werden
    // berichtet, weil ihre Zahl die Diagnose traegt.
    {
        let sf = |a: &[core::sync::atomic::AtomicU64; 4], i: usize| a[i].load(Ordering::Acquire);
        let (v0, v1) = (
            NET_VICTIM[0].load(Ordering::Acquire),
            NET_VICTIM[1].load(Ordering::Acquire),
        );
        let zustand = DMAISO_STATE.load(Ordering::Acquire);
        if zustand == 1 {
            println!(
                "dmaiso  : Positivkontrolle features={} tx={} rx={} arp={} -- sie traegt NICHT",
                sf(&NET_SELF, 0), sf(&NET_SELF, 1), sf(&NET_SELF, 2), sf(&NET_SELF, 3)
            );
            println!(
                "dmaiso  : SKIP -- nicht messbar: die Positivkontrolle traegt nicht (kein \
                 Gegenueber, das auf ARP antwortet, oder keine brauchbare Netzkarte). 'Der \
                 Fremdzugriff kam nicht an' belegt dann nichts -- es kam ueberhaupt nichts an. \
                 Das als FAILURES zu melden zeigte auf die Isolierung statt auf den Aufbau"
            );
        } else if zustand == 0 {
            println!(
                "dmaiso  : SKIP (kein zweiter Treiber -- ohne eine zweite Treiber-PD gibt es \
                 nichts, wovon getrennt werden koennte, und eine gruene Zeile hiesse nichts)"
            );
        } else {
            // Nur drucken -- das Urteil steht seit `drv_service_step` Schritt 4 fest.
            let ok = DMAISO_OK.load(Ordering::Acquire);
            println!(
                "dmaiso  : Positivkontrolle features={} tx={} rx={} arp={} · Fremdversuch \
                 features={} tx={} rx={} arp={} · Opfer {:#018x} -> {:#018x} · VT-d-Faults={}",
                sf(&NET_SELF, 0), sf(&NET_SELF, 1), sf(&NET_SELF, 2), sf(&NET_SELF, 3),
                sf(&NET_FOREIGN, 0), sf(&NET_FOREIGN, 1), sf(&NET_FOREIGN, 2), sf(&NET_FOREIGN, 3),
                v0, v1, NET_FAULTS.load(Ordering::Acquire)
            );
            println!(
                "dmaiso  : {} (A-5.4: das Geraet der EINEN Treiber-PD erreicht die DMA-Region der \
                 ANDEREN nicht. Die Positivkontrolle laeuft ueber denselben Treiber, dasselbe \
                 Geraet und dieselbe Deskriptorkette -- nur EINE Adresse wandert. Ohne sie waere \
                 'nichts kam an' kein Beleg, sondern die Beschreibung eines Geraets, das gar nicht \
                 laeuft. Und dass beim Opfer nichts ankam, prueft der Kernel selbst nach; die \
                 Meldung des Angreifers ist eine Aussage ueber ihn)",
                if ok { "ALL PASS" } else { "FAILURES" }
            );
        }
    }

    // --- E-Rest 3: bleibt die GETEILTE Geraete-Tabelle oberhalb 4 GiB sauber? ------------------
    //
    // Die Falle steht in CLAUDE.md und gilt fuer die neuen hohen Tabellen woertlich wie fuer
    // `ISO_PD_HIGH`: wer ein Geraetefenster in eine GETEILTE Tabelle schreibt, gibt es JEDER
    // isolierten PD -- lautlos, denn die Cap-Pruefung laeuft dabei korrekt durch. Eine Loesung,
    // die das Fenster allen PDs gibt, ist keine.
    //
    // Geprueft wird deshalb nicht die Absicht, sondern der Inhalt: in der geteilten Tabelle darf
    // ausschliesslich stehen, was der Hochlauf hineinschreibt (2-MiB-Geraeteblock, kein `US`).
    // Und weil "0 unzulaessige Eintraege" ueber nichts urteilt, solange nie eine PD ein Fenster
    // dort bekommen hat, steht die Zahl der PRIVATEN Kopien danebst: erst sie macht die Aussage
    // zu einem Testergebnis. Ohne Treiber-PD (nackte Suite) ist das ein SKIP mit Grund.
    {
        let (geteilt, privat, verstoesse) = hal::mmu::high_shared_audit();
        println!(
            "hiiso   : geteilte Geraete-Tabellen oberhalb 4 GiB={geteilt}, private Kopien fuer \
             isolierte PDs={privat}, unzulaessige Eintraege in den geteilten={verstoesse}"
        );
        println!(
            "hiiso   : {}",
            if verstoesse > 0 {
                "FAILURES"
            } else if privat == 0 {
                // Kein Urteil moeglich -- und das ist ein eigenes Ergebnis, kein Bestehen.
                "SKIP"
            } else {
                "ALL PASS"
            }
        );
    }

    // Z4 Stufe 2 **vor** `freeze_bericht`: jene Pruefung friert denselben Worker noch einmal ein
    // und taut ihn auf. Liefe sie zuerst, waere „der Zaehler lief nach dem Wiederherstellen
    // weiter" eine Aussage ueber ihr Auftauen statt ueber den Checkpoint.
    ckpt_bericht();

    freeze_bericht();

    // Z6b **nach** `freeze_bericht`: beide halten denselben Worker an, und der Debugger benutzt
    // dafuer einen anderen Grund (`DEBUG` statt `PAUSE`). Liefe die Debug-Sonde zuerst und liesse
    // -- durch einen Fehler -- den Grund stehen, meldete `freeze` einen `Debugged` und die
    // Diagnose zeigte auf die falsche Zeile.
    // Z6b: die Debugger-Sonde. **Arch-neutral und von BEIDEN Zweigen gefahren**, seit sie ihr
    // eigenes Ziel mitbringt statt am hiesigen Worker zu messen.
    crate::dbgprobe::messen(system::IDLE_PRIO);

    // Z6b: die Speicher-Sonde. **Arch-neutral und von BEIDEN Zweigen gefahren** -- sie prueft die
    // eine Stelle (`vspace_resolve`), an der x86 und aarch64 sich wirklich unterscheiden.
    crate::dbgmem::messen(system::IDLE_PRIO);
    crate::dbgmem::bericht();

    // Z23/S3: der Gruppenschnitt. **Arch-neutral und von BEIDEN Zweigen gefahren** -- die geprueften
    // Stellen (Scheduler-Grundmenge, Endpoint-Rollen, PD-Tabelle) sind es auch. Sie laeuft NACH der
    // Debugger-Sonde, weil sie drei PDs und fuenf Threads belegt und ein Test, der Speicher belegt,
    // baseline-empfindliche Tests kippt (Fallenliste).
    crate::pdfreeze::messen(system::IDLE_PRIO);

    // Z4d stage 1: the checkpoint cut. **Arch-neutral and run by BOTH paths**, and deliberately
    // right behind the group cut: it asks the same question one level up (a relationship whose
    // both ends lie inside the cut is not an open relationship of the cut) but about a
    // CHECKPOINT's scope instead of a PD's threads. Two PDs and four threads, so it sits late for
    // the same reason `pdfreeze` does -- a test that allocates memory tips baseline-sensitive
    // tests (Fallenliste).
    crate::ckptcut::messen(system::IDLE_PRIO);

    // K1b: mehrere Thread-Stapel aus EINER Memory-Cap. **Arch-neutral, von BEIDEN Wegen gefahren**,
    // und aus demselben Grund spaet wie `ckptcut`: eine PD, sechs Threads und zwei Regionen -- ein
    // Test, der Speicher belegt, kippt baseline-empfindliche Tests (Fallenliste).
    //
    // Er ist zugleich der ERSTE Lauf von `SYS_SPAWN` ueberhaupt: der Syscall steht seit dem
    // 2026-08-17 in der ABI und hatte bis heute keinen Aufrufer und kein Gatter.
    crate::spawnarena::messen();
    crate::tlsprobe::messen();
    crate::uhr::messen();

    // A-5.3 -- hier, nicht beim Start des Root-Tasks: die Treiber-PD entsteht erst, wenn `init`
    // laeuft und `SYS_LOAD` ruft.
    devsel_bericht();
    // C2: die Pools stehen direkt daneben -- dieselbe Quelle (`DRIVER_ASSIGN`), andere Frage.
    #[cfg(feature = "selftest")]
    dmapool_bericht();
    irqmsi_bericht();

    let sa = system::sched_audit_all();
    let cdt = system::cap_audit_cdt();
    let (walk, revops, limit) = system::cap_cdt_peaks();
    println!("audit   : sched_audit={sa} cdt_audit={cdt}");
    println!(
        "cdtlen  : B-5.5 Hoechststaende als OPERATIONSZAHL -- Abstieg {walk}/{limit} Schritte, \
         Revoke {revops}/{limit} Loeschungen (Schranke = Slotzahl: ein azyklischer Lauf besucht \
         keinen Slot zweimal). Zeit waere das falsche Mass -- sie haengt an Taktrate und \
         Emulation, nicht an der Struktur"
    );
    println!(
        "audit   : {}",
        if sa == 0 && cdt == 0 { "ALL PASS" } else { "FAILURES" }
    );

    // --- Das Endowment wird VERBUCHT (2026-08-25) -----------------------------------------------
    //
    // Die Zeile hat drei Ausgaenge und nicht zwei, und der dritte ist der Grund, warum sie etwas
    // sagt: diese Suite bootet **ohne Boot-Archiv**, laedt also kein Programm, und dann ist
    // „keine Zusage gebrochen" von „nichts gemessen" nicht zu unterscheiden. Entschieden wird
    // deshalb an `geprueft`, nicht an den beiden Zahlen darunter.
    //
    // `angebote` ist kein Fehler: `init` bietet jedem Kind seine Notification an und kann dessen
    // Domaene nicht kennen; jede HardwareLand-PD lehnt sie ab (`cap_allowed`). Bis heute geschah
    // das **still** -- und genau deshalb behauptet die Doku-Tabelle in `virtio-blk` bis heute,
    // in Slot 0 laege eine Notification.
    {
        let (geprueft, gebrochen, angebote) = system::endowment_bilanz();
        if geprueft == 0 {
            println!(
                "endow   : SKIP (kein Programm geladen -- diese Suite hat kein Boot-Archiv, der Ladepfad wurde also nicht gefahren. Die Zahlen stuenden auf 0, ohne dass etwas gemessen waere)"
            );
        } else {
            println!(
                "endow   : {} (geprueft={geprueft} Zusagen-gebrochen={gebrochen} Angebote-abgelehnt={angebote}) -- eine Zusage des signierten Manifests, die sich nicht installieren laesst, weist den Ladevorgang ab, BEVOR etwas alloziert ist; ein Angebot des Aufrufers darf eine Zieldomaene ablehnen und wird gezaehlt",
                if gebrochen == 0 { "ALL PASS" } else { "FAILURES" }
            );
        }
    }

    // --- Ein Dienst OHNE Geraet ist auffindbar (2026-08-25) -------------------------------------
    //
    // Die Aussage, um die es geht: **der Client bekommt den Endpoint DES DIENSTES, nicht einen
    // frischen.** Genau das ging bis heute nicht -- `set_driver_service` lief nur auf dem
    // HardwareLand-Zweig, ein Client mit `service_id` auf eine geraetelose PD bekam `None` und
    // danach einen unverbundenen Endpoint. Beide Seiten haetten einen Kanal gehabt und keinen
    // gemeinsamen.
    //
    // Gemessen wird eine **Gleichheit** und eine **Ungleichheit**, nicht ein Rueckgabewert:
    // die Endpoint-ID in Slot 2 des Clients muss die des Dienstes sein UND darf nicht die eines
    // anderen Dienstes sein. Ohne die zweite Haelfte belegte die erste nur, dass irgendein
    // Endpoint dort steht.
    {
        // **Gelesen wird die ERFASSUNG aus dem Endowment**, nicht der Cap-Slot der PD. Der erste
        // Anlauf tat das Zweite und meldete `client-trifft-dienst=false` -- richtig gemessen, nur
        // an der falschen Stelle: `wasmhost` ist zu diesem Zeitpunkt fertig und **tot**
        // (`Thread existiert=false, im-Register=true`), seine PD abgebaut, der Slot weg.
        let ep_in_slot2 =
            |program_id: u32| -> Option<u32> { crate::loader::client_ep_of(program_id).map(|e| e as u32) };
        let dienst = crate::loader::driver_service_of(DIENST_PROG_ID);
        let blk = crate::loader::driver_service_of(TEST_BLK_SERVICE_ID);
        let registriert = dienst.is_some();
        let client_ep = ep_in_slot2(DIENST_CLIENT_PROG_ID);
        let dienst_ep = dienst.map(|s| s.ep as u32);
        let client_trifft = client_ep.is_some() && client_ep == dienst_ep;
        // Die Gegenprobe: es ist NICHT der Kanal des Blockdienstes. Ohne sie waere „der Client hat
        // einen Endpoint" von „der Client hat den RICHTIGEN Endpoint" nicht zu unterscheiden.
        let nicht_fremd = match (client_ep, blk.map(|s| s.ep as u32)) {
            (Some(c), Some(b)) => c != b,
            // Kein Blockdienst -> die Unterscheidung ist hier nicht entscheidbar, und das ist
            // etwas anderes als bestanden.
            _ => false,
        };
        if crate::loader::thread_of_program(DIENST_PROG_ID).is_none() {
            println!(
                "dienst  : SKIP (Programm {DIENST_PROG_ID} nicht geladen -- diese Suite hat keine \
                 Startmenge mit einem geraetelosen Dienst)"
            );
        } else {
            let ok = registriert && client_trifft && nicht_fremd;
            println!(
                "dienst  : {} (registriert={registriert} client-trifft-dienst={client_trifft} \
                 nicht-fremder-kanal={nicht_fremd} dienst-ep={:?} client-ep={:?}) -- eine PD OHNE \
                 Geraet wird unter ihrer program_id registriert, und ein Client, der sie mit \
                 service_id benennt, bekommt IHREN Endpoint statt eines frischen",
                if ok { "ALL PASS" } else { "FAILURES" },
                dienst_ep,
                client_ep
            );
        }
    }

    // --- B-5.1: wird der Verbrauch gestempelt oder getickt? -------------------------------------
    //
    // Die Aussage, die hier fallen koennen muss: **es wird bei jeder Umplanung abgerechnet, nicht
    // nur beim Tick.** Genau das ist der Unterschied -- wer kurz vor dem Tick blockiert, hat
    // gerechnet und zahlte vorher nichts.
    //
    // Die Zahl, die das belegt, ist `Proben > Ticks`: die Tick-Rechnung kann hoechstens einmal je
    // Tick belasten. Jede Probe darueber hinaus ist ein Abrechnungsereignis ZWISCHEN zwei Ticks --
    // also genau die Zyklen, die vorher niemand zahlte.
    //
    // Und die Sprechprobe: ist die Zeitquelle nicht zugesichert, wird bewusst nichts gerechnet --
    // dann muss `rejected_source > 0` sein. Ein Kernel, in dem die Klammerung gar nicht gerufen
    // wird, sieht sonst genauso aus wie einer, der korrekt schweigt. Beide Zweige verlangen einen
    // BELEG, keiner laesst eine leere Statistik als Erfolg durchgehen.
    let (proben, zyklen, ticks, rw, unpl, kw, unsicher) = system::cycle_accounting(0);
    // **Welcher Zweig gilt, entscheidet die Maschine, nicht der Kernel.** Der erste Entwurf las
    // nur `unsicher == 0` und waehlte danach den Zweig -- damit haette ein vergessenes
    // `set_cycle_source` den Test bestanden: alles landete in `rejected_source`, der
    // Untrusted-Zweig war erfuellt, und der Bericht meldete gruen fuer einen Kernel, der gar
    // nicht abrechnet. Die Sollgroesse muss von aussen kommen: `invariant_tsc()` sagt, was die
    // Maschine kann. Sagt sie ja und der Scheduler rechnet trotzdem nicht, ist die Verdrahtung
    // kaputt -- und genau das soll die Zeile sehen.
    let hw = hal::timer::invariant_tsc();
    let anomalie = rw != 0 || unpl != 0 || kw != 0;
    let cyc_ok = !anomalie
        && if hw {
            // Die Maschine kann messen -> es MUSS gemessen werden, und zwar oefter als getickt.
            unsicher == 0 && proben > ticks && zyklen > 0
        } else {
            // Die Maschine kann es nicht -> es darf nichts gerechnet werden. Aber der Pfad muss
            // nachweislich laufen: der Ablehnungszaehler ist hier der Sprechbeleg.
            proben == 0 && zyklen == 0 && u64::from(unsicher) > ticks
        };
    println!(
        "cycacct : {} -- B-5.1: {proben} Zyklenproben gegen {ticks} Ticks ({zyklen} Zyklen \
         verbucht; verworfen: rueckwaerts={rw} unplausibel={unpl} Kernwechsel={kw} \
         Quelle-nicht-zugesichert={unsicher}; Maschine invariant={hw}). Proben > Ticks IST die \
         Aussage: die Tick-Rechnung kann hoechstens einmal je Tick belasten, jede weitere Probe \
         ist Rechenzeit, die vorher niemand zahlte. Welcher Zweig gilt, sagt die MASCHINE \
         (invariant_tsc) -- sonst bestuende ein Kernel, der gar nicht abrechnet, ueber den \
         Untrusted-Zweig",
        if cyc_ok { "ALL PASS" } else { "FAILURES" }
    );

    // **Die Kapazitaetskurve ganz zum SCHLUSS** -- nach jedem Urteil, vor dem Herunterfahren.
    //
    // Der Platz ist die halbe Entscheidung. Diese Messung belegt tausende PDs, Threads und
    // Seitentabellen; irgendwo weiter oben ausgefuehrt kippte sie jede baseline-empfindliche
    // Zeile des Laufs (`loadstop`, `capsz`, die Farbtests) -- genau die Falle, an der schon der
    // aarch64-Farbtest haengengeblieben ist: „ein Test, der Speicher belegt, kippt
    // baseline-empfindliche Tests". Hier kann sie strukturell nichts mehr beeinflussen, weil
    // alle Zeilen bereits gedruckt sind.
    kapazitaet_kurve();

    println!("x86_64 Stufe 4: Kernel-Kern laeuft (Selbsttests + Scheduler + cap-gesicherte IPC)");
    // Der Marker trennt die beiden Ausgänge -- s. Funktionsdoku. Ein Lauf, der aus der Notbremse
    // kommt, darf nicht denselben Satz drucken wie einer, der alles belegt hat.
    if watchdog {
        println!("== SELFTEST FAILED (watchdog) -> system_off ==");
    } else {
        println!("== SELFTEST COMPLETE -> system_off ==");
    }
    hal::power::system_off()
}

/// Einstieg des Kernel-Kerns auf x86_64 (vom Boot-Trampolin über `x86_rust_entry` gerufen).
pub fn run(multiboot_info: u64) -> ! {
    // --- Hardware in der Reihenfolge hochziehen, in der sie voneinander abhängt ---
    hal::console::init();
    #[cfg(feature = "debug-log")]
    println!("========================================");
    #[cfg(feature = "debug-log")]
    println!(" Caprock — capability microkernel");
    #[cfg(feature = "debug-log")]
    println!(" x86_64 (Multiboot -> Long Mode)");
    // **Die Konfiguration gehört an das Artefakt gebunden, nicht nur ins Bauprotokoll.**
    // Der Binärfingerabdruck der Suiten schliesst „veralteter Build" für einen *Suitenlauf* aus;
    // für die *Bauumgebung* stand dieselbe Tür offen, und am 2026-08-10 lieferte ein sauber
    // übersetzender `cargo build` ein Abbild mit `__text_start = 0x100000`, das nie gebootet
    // hätte (Cargo mischt `.cargo/config.toml` aus jedem Vorfahrenverzeichnis — s.
    // `kernel/build.rs`). Seither steht der Fingerabdruck der **effektiven** Flags im Abbild und
    // wird gedruckt: zwei Läufe mit verschiedener Bauumgebung sind damit unterscheidbar, ohne
    // dass jemand die Umgebung nachträglich rekonstruieren muss.
    #[cfg(feature = "debug-log")]
    println!(
        " bauflags {} ({} Flags)",
        env!("CAPROCK_FLAGS_FP"),
        env!("CAPROCK_FLAGS_N")
    );
    #[cfg(feature = "debug-log")]
    println!("========================================");
    hal::exception::init(); // IDT: Faults ab hier diagnostizierbar
    hal::gdt::init(); // GDT + per-Kern-TSS (Selektoren für Trap-/Ring-Wechsel, IST-Stacks)
    // **Sofort nach der TSS, vor allem anderen**: ab hier kann die CPU auf einen IST-Stack
    // umschalten, und ein ungefüllter Stack macht den Wasserstand im `#DF`-Bericht wertlos.
    super::ist::stacks_fuellen();
    super::ist::haken_installieren();
    hal::mmu::init_primary(); // 4-Level-Paging, W^X, CR0.WP
    let (m, c, w) = hal::mmu::sctlr_flags();
    println!("mmu     : identity-map, paging={} caches={} CR0.WP={}", m as u8, c as u8, w as u8);
    #[cfg(feature = "debug-log")]
    println!("arch    : x86_64 (CPL {} = Ring 0)", 1 - hal::cpu::current_el());

    hal::intc::init_dist(); // 8259-PIC stilllegen
    hal::intc::init_cpu(); // LAPIC aktivieren
    hal::timer::init(TICK_HZ);
    // C9: die Schwelle der Sperrhaltedauer-Marke ist EIN Tick. Sie wird hier hinterlegt, direkt
    // neben dem Aufruf, der den Timer wirklich programmiert -- eine `100` in `sperrmark.rs` waere
    // ein zweites Gedaechtnis fuer dieselbe Tatsache.
    #[cfg(feature = "selftest")]
    crate::sperrmark::tickrate_setzen(TICK_HZ);
    // Z19/A4: SSE auf dem BSP freischalten -- dieselbe Funktion wie im AP-Pfad.
    if hal::fp::enable_sse() {
        SSE_CORES.fetch_add(1, Ordering::Relaxed);
    }
    println!(
        "timer   : LAPIC-Timer {} Hz (Basis {} Hz, gegen PIT kalibriert), Vektor {}",
        TICK_HZ,
        hal::timer::freq(),
        hal::timer::TIMER_INTID
    );
    #[cfg(feature = "debug-log")]
    println!(
        "spec    : CSV2={} CSV3={} FEAT_SB={} · nospec-Indizes an",
        hal::cpu::csv2(),
        hal::cpu::csv3(),
        hal::cpu::sb_supported() as u8
    );
    crate::colors::report();

    // --- Speicher + arch-neutrale Selbsttests (identisch zu aarch64) ---
    let mbi = super::multiboot::MultibootInfo::new(multiboot_info);
    let ram_end = match mbi.and_then(|m| m.ram_end()) {
        Some(e) => {
            println!("mbi     : Speicherplan gelesen -> RAM bis {:#x} ({} MiB)", e, e >> 20);
            e
        }
        None => {
            println!("mbi     : kein Speicherplan -> Rueckfall {} MiB", RAM_END_FALLBACK >> 20);
            RAM_END_FALLBACK
        }
    };
    // --- Multiboot-Module (A-1.1) ---
    //
    // Die Startmenge kommt auf x86 als Multiboot-Module. Zwei Dinge muessen VOR der ersten
    // Allokation stehen, nicht danach:
    //
    // 1. Die Modulbereiche duerfen dem `PhysAllocator` nie als frei gemeldet werden. Sonst
    //    ueberschreibt die erste Allokation genau das Archiv, das der Kernel gleich lesen will --
    //    und zwar lautlos, weil ein ueberschriebenes Archiv einfach "kein gueltiges Archiv" ergibt.
    // 2. Der Loader muss wissen, WO das Archiv liegt. Auf ARM ist das eine Verabredung mit dem
    //    Testaufbau; hier sagt es der Bootloader erst zur Laufzeit.
    let mut mods = [super::multiboot::Module { start: 0, end: 0 }; super::multiboot::MAX_MODULES];
    let nmods = mbi.map(|m| m.modules(&mut mods)).unwrap_or(0);
    let claimed = mbi.map(|m| m.mods_claimed()).unwrap_or(0);
    if nmods > 0 {
        print!("mbi     : {nmods} Modul(e):");
        for m in &mods[..nmods] {
            print!(" [{:#x}..{:#x}) {} KiB", m.start, m.end, m.len() >> 10);
        }
        println!();
    }
    if claimed > nmods {
        // Eine stille Kuerzung sieht in jeder spaeteren Auswertung aus wie Vollstaendigkeit.
        println!(
            "mbi     : HINWEIS Bootloader meldet {claimed} Module, ausgewertet werden {nmods} (Grenze {} bzw. leere Eintraege verworfen)",
            super::multiboot::MAX_MODULES
        );
    }
    // Modul 0 ist das Boot-Archiv (Verabredung mit `test-qemu-x86.sh`: `-initrd boot-archive.bin`).
    if nmods > 0 {
        crate::loader::set_archive_span(mods[0].start, mods[0].len());
    }
    // Module 1.. sind LXPD-Treiber-Images (Mitteilung 12): Spannen melden, damit der
    // Boot-Hook sie per Hash findet. Alle Module sind oben aus der Freiliste
    // ausgeschnitten (s. subtract_holes) — lesbar, nie doppelt vergeben.
    // `nmods > 1` zuerst: bei null Modulen waere `mods[1..0]` ein Panic im Boot-Pfad.
    if nmods > 1 {
        for (i, m) in mods[1..nmods].iter().enumerate() {
            if m.len() == 0 {
                continue;
            }
            crate::loader::set_lxpd_module_span(i, m.start, m.len());
        }
    }
    let free_base = hal::mmu::kernel_end().max(hal::mmu::USER_RAM_MIN);
    /*
     * **Die Struktur des Bootloaders liegt unter der Freiliste -- gemessen, nicht angenommen.**
     *
     * Der klassische Fehler dieser Klasse (OSDev, GRUB): ein Lader meldet den Speicher unter
     * 1 MiB als frei, obwohl dort EBDA, BIOS-Datenbereich und seine eigenen Strukturen liegen --
     * die Empfehlung lautet, alles darunter als belegt zu behandeln. Hier ist das dreifach
     * gedeckt: `ram_regions` nimmt nur Typ 1 und verwirft `base < 1 MiB`, `free_base` liegt bei
     * mindestens 16 MiB, und die Modulbereiche werden ausgeschnitten.
     *
     * Aber: **die Multiboot-Info-Struktur selbst wird NICHT ausgeschnitten.** Dass sie trotzdem
     * sicher ist, haengt heute allein an `USER_RAM_MIN` -- unter QEMU liegt sie bei `0x9500`,
     * unter einem anderen Lader kann sie anderswo liegen. Genau die Sorte Eigenschaft, die aus
     * einer Konstante folgt, die niemand daraufhin prueft: senkt jemand `USER_RAM_MIN`, faellt
     * der Schutz lautlos weg.
     *
     * Deshalb steht die Bedingung hier als Zeile und nicht als Annahme.
     */
    let mbi_geschuetzt = multiboot_info < free_base;
    println!(
        "mbi     : Bootloader-Struktur bei {multiboot_info:#x}, Freiliste ab {free_base:#x} -- \
         ausserhalb: {}",
        mbi_geschuetzt as u8
    );
    /*
     * Den freien Speicher **absichtlich zerstueckelt** uebergeben, statt als einen Block.
     *
     * Fuer die Kern-Uebergabe an Linux (Variante B) kommt der Speicher als Sammlung dessen,
     * was der Wirt hergibt -- dort hoechstens 4 MiB am Stueck. Ob der Kernel damit umgehen
     * kann, ist keine Frage der Absicht, sondern eine Eigenschaft, die gelten muss; und ein
     * Pfad, der im Test nie zerstueckelten Speicher sieht, belegt sie nicht. Also sieht der
     * regulaere QEMU-Lauf ihn immer: dieselbe Menge Speicher, nur in acht Bereichen.
     *
     * Luecken entstehen dabei keine -- die Bereiche stossen aneinander. Der Allokator
     * verschmilzt sie beim Freigeben ohnehin wieder; geprueft wird der *Eingang*.
     */
    const SPLIT: u64 = 8;
    /// Wie viele `mmap`-Eintraege hoechstens ausgewertet werden. Klein und sichtbar -- die
    /// Struktur kommt vom Bootloader, und eine unbegrenzte Schleife ueber fremde Daten ist
    /// keine Auswertung, sondern ein Vertrauensvorschuss.
    const MAX_MMAP: usize = 16;
    let mut bi = super::bootinfo::HandoverInfo::empty();
    bi.ram_top = ram_end;
    /*
     * **E-Rest 3: der Speicherplan wird jetzt gelesen statt interpoliert.**
     *
     * Bisher stand hier `[free_base, ram_end)` als EIN Block. Das ist genau so lange richtig,
     * wie der Speicher zusammenhaengt -- und sobald QEMU RAM oberhalb 4 GiB anlegt, tut er das
     * nicht mehr: bei `-m 3G` liegen 2 GiB unter 4 GiB, 1 GiB ab 4 GiB, und dazwischen klafft
     * das PCI-Loch `0x8000_0000..0x1_0000_0000`. Der Kernel meldete es dem Allokator als freies
     * RAM -- gemessen `mem : freies RAM [0x1000000, 0x140000000)`, also 5070 MiB "frei" auf
     * einer 3-GiB-Maschine. Eine Allokation dort trifft Geraeteregister oder gar nichts, und
     * zwar erst dann, wenn genug Speicher verbraucht ist: also spaet, weit weg und ohne
     * erkennbaren Zusammenhang.
     *
     * `ram_top` bleibt der **hoechste** Wert (daran haengt die Wahl des IOVA-Fensters); die
     * Freiliste kommt aus den als verfuegbar gemeldeten Bereichen. Zwei Fragen, zwei Quellen.
     */
    let mut avail = [(0u64, 0u64); MAX_MMAP];
    let mut navail = mbi.map(|m| m.ram_regions(&mut avail)).unwrap_or(0);
    if navail == 0 {
        // Kein auswertbarer Plan -> die alte Annahme, aber ausgesprochen statt unterstellt.
        avail[0] = (free_base, ram_end.saturating_sub(free_base));
        navail = 1;
        println!("mem     : HINWEIS kein Bereichsplan -> Rueckfall auf einen Block");
    }
    let mut gross = 0u64; // Summe vor dem Ausschnitt
    // Einen Bereich in `SPLIT` Stuecke zerlegt in die Freiliste geben. Die Zerstueckelung ist
    // eine Aussage ueber den **Eingang** (s. o.); der Allokator verschmilzt angrenzende Stuecke
    // beim Einfuegen ohnehin sofort wieder.
    let mut einspeisen = |a: u64, b: u64, gross: &mut u64, bi: &mut super::bootinfo::HandoverInfo| {
        let chunk = ((b - a) / SPLIT) & !0xfff;
        for k in 0..SPLIT {
            let base = a + k * chunk;
            let len = if k == SPLIT - 1 { b - base } else { chunk };
            if len == 0 {
                continue;
            }
            *gross += len;
            // Die Modulbereiche werden hier **ausgeschnitten**, nicht spaeter markiert: was nie
            // als frei gemeldet wurde, kann auch nicht vergeben werden (A-1.1).
            super::multiboot::subtract_holes(base, len, &mods[..nmods], &mut |b, l| {
                bi.push_region(b, l);
            });
        }
    };
    let grenze = hal::mmu::LOW_MAPPED_END;
    // **Erst alles unter 4 GiB** -- der Speicher, den es auf jeder Maschine gibt. Die Reihenfolge
    // ist seit E-Rest 3b nur noch Ordnung im Bericht, keine Zusicherung mehr: welcher Bereich
    // eine Anforderung bedient, entscheidet der Zonenwunsch, nicht die Einspeisereihenfolge.
    for &(rb, rl) in &avail[..navail] {
        // Was unterhalb von `free_base` liegt, gehoert dem Kernel-Image und den Boot-Tabellen.
        let a = rb.max(free_base);
        let b = rb.saturating_add(rl).min(grenze);
        if b <= a {
            continue;
        }
        einspeisen(a, b, &mut gross, &mut bi);
    }
    /*
     * **Und jetzt der Teil oberhalb von 4 GiB -- seit E-Rest 3b ohne Bedingung.**
     *
     * Bis zum 2026-08-04 stand hier ein Behelf: der hohe Bereich kam nur in die Freiliste, wenn
     * er GROESSER war als der kleinste untere. Der Grund war eine unausgesprochene Eigenschaft,
     * auf die sich `alloc_dma_region` und die Region einer isolierten PD verliessen -- beide
     * MUESSEN in GiB 0 liegen (`vspace_map_block` bildet nur dort ab), fragten aber nach
     * "irgendeiner" Region und gaben bei Verfehlung auf, ohne ein zweites Mal zu fragen. Solange
     * der Speicher zusammenhing, belegte Best-Fit von unten und es ging gut; ein zweiter,
     * KLEINERER Bereich oben brach es (gemessen bei `-m 3G`: `dmawin`/`dmatok : FAILURES`,
     * `iso : spawn_isolated fehlgeschlagen`, waehrend 4G und 6G zufaellig gruen waren).
     *
     * Der Behelf war fail-closed und hat den nutzbaren Speicher fuer DMA-Regionen und isolierte
     * PDs bei 1 GiB gedeckelt -- fuer das Zielbild der haertere Deckel als die alte 4-GiB-Karte.
     *
     * Behoben ist es jetzt dort, wo die Ursache lag: `caprock_mem::alloc_below` kennt den
     * Zonenwunsch, und die drei Aufrufer nennen ihn (`system::gib0_zone`). Damit haengt nichts
     * mehr an der Belegungsordnung, und hoher Speicher geht vollstaendig in die Freiliste --
     * fuer alles, was keine PD-eigene Abbildung braucht (Kernel-Stacks, Heap, Slabs, Archiv).
     */
    let mut hoch_abgebildet = 0u64;
    let mut hoch_frei = 0u64;
    let mut hoch_unabgebildet = 0u64;
    for &(rb, rl) in &avail[..navail] {
        let a = rb.max(free_base).max(grenze);
        let b = rb.saturating_add(rl);
        if b <= a {
            continue;
        }
        // **Erst abbilden, dann verteilen.** In die Freiliste kann nur kommen, was
        // `adopt_high_ram` als RAM in die Karte gelegt hat -- nicht ein Byte mehr. Ohne diese
        // Kopplung waeren "was der Allokator ausgeben darf" und "was abgebildet ist" zwei
        // Zahlen, die zueinander passen muessen; so ist es eine.
        let Some((hb, hl)) = hal::mmu::adopt_high_ram(a, b - a) else {
            hoch_unabgebildet += b - a;
            continue;
        };
        hoch_abgebildet += hl;
        hoch_unabgebildet += (b - a) - hl;
        hoch_frei += hl;
        einspeisen(hb, hb + hl, &mut gross, &mut bi);
    }
    // Wieviel durch den Ausschnitt wegfiel -- als Zahl, nicht als Gefuehl.
    let net = bi.mem_regions().iter().map(|r| r.len).sum::<u64>();
    let carved = gross - net;
    // Dieselbe Pruefung, die der Uebergabeweg vor jeder Benutzung faehrt -- damit sie im
    // regulaeren Lauf auch tatsaechlich einmal ausgefuehrt wird.
    if !bi.valid() {
        println!("mem     : FAILURES (HandoverInfo unplausibel)");
        hal::power::system_off();
    }
    let mut regions = [(0u64, 0u64); super::bootinfo::MAX_REGIONS];
    for (i, r) in bi.mem_regions().iter().enumerate() {
        regions[i] = (r.base, r.len);
    }
    system::init_mem_regions(&regions[..bi.n_regions as usize], bi.ram_top);
    println!(
        "mem     : {} Bereiche / {} MiB ueber HandoverInfo (Quelle {}), verworfen={}",
        bi.n_regions,
        net >> 20,
        if bi.source == super::bootinfo::BootSource::Handover { "Kern-Uebergabe" } else { "Multiboot" },
        system::mem_regions_dropped()
    );
    print!(
        "mem     : freies RAM ab {free_base:#x} aus {navail} gemeldeten Bereich(en), \
         RAM-Oberkante {ram_end:#x}"
    );
    if hoch_abgebildet > 0 || hoch_unabgebildet > 0 {
        // Drei Zahlen, weil es drei verschiedene Lagen sind, und ein stillschweigend kleinerer
        // Speicher eine Fehldiagnose in Wartestellung ist: **abgebildet** (der Kernel kommt
        // heran), **davon vergeben** (der Allokator darf es ausgeben -- seit E-Rest 3b immer
        // alles Abgebildete; die beiden Zahlen sind gleich, und wenn sie es einmal nicht sind,
        // steht es hier), **nicht abgebildet** (kein vollstaendig gedecktes 1-GiB-Blatt oder
        // keine 1-GiB-Seiten auf dieser CPU).
        print!(
            " -- oberhalb 4 GiB: {} MiB abgebildet, davon {} MiB vergeben; {} MiB nicht abgebildet",
            hoch_abgebildet >> 20,
            hoch_frei >> 20,
            hoch_unabgebildet >> 20
        );
    }
    println!();
    if carved > 0 {
        println!("mem     : {carved} Byte fuer Multiboot-Module ausgeschnitten (vor der ersten Allokation)");
    }
    // Cap-Tabellen VOR dem ersten Cap (A-3.4): der Selbsttest weiter unten installiert bereits
    // welche, und ein nicht angehaengter Space haette Kapazitaet 0.
    let cap_bytes = system::configure_caps();
    let (cap_slots, cap_objs) = system::cap_capacity();
    println!(
        "cap     : {cap_slots} Slots / {cap_objs} Objekte / {} PDs, Tabellen {} KiB aus dem RAM (Summe aller PD-Budgets: {})",
        system::pd_capacity(),
        cap_bytes >> 10,
        caprock_microkit::CAP_SLOTS_FOR_ALL_PDS
    );
    // A-3.4 Teil 4: IPC-Tabellen VOR dem ersten Endpoint. Meldet sich selbst (`ipc :`).
    system::configure_ipc();
    #[cfg(feature = "selftest")]
    {
        // Das Ausschneiden traegt eine Sicherheitsaussage; der reale Lauf sieht davon nur EINEN
        // Fall. Die Grenzfaelle werden eingespeist (s. `multiboot::selftest`).
        let ok = super::multiboot::selftest();
        println!(
            "mbmod   : {} (Modulbereiche werden aus der Freiliste ausgeschnitten: Rand, Ueberlappung, unsortiert, Vollabdeckung)",
            if ok { "ALL PASS" } else { "FAILURES" }
        );
    }
    crate::loader::probe(); // was liegt im Archiv? (A-1.1: die Quelle ist jetzt auch auf x86 da)
    crate::loader::manifest_report(); // A-1.2..A-1.4: wer bekommt welche Autoritaet?
    #[cfg(feature = "selftest")]
    crate::selftest::run();

    // --- Kernel-Kern: Hooks, Tabellen, Scheduler ---
    system::set_hooks();
    // Kernzahl aus der ACPI-MADT (x86-Gegenstück zu den `/cpus`-Knoten des DTB).
    let cpus = hal::acpi::cpus();
    let ncpu = cpus.as_ref().map(|c| c.count()).unwrap_or(1);
    println!("acpi    : {ncpu} CPU(s) laut MADT");
    if let Some((ecam, b0, b1)) = hal::acpi::pci_ecam() {
        println!("acpi    : PCI-ECAM @ {ecam:#x}, Busse {b0}..{b1}");
    }
    // --- PCI-Enumeration (die Firmware hat BARs/Bridges bereits konfiguriert) ---
    let mut ndev = 0usize;
    hal::pcie::dump_devices(&mut |bus, dev, ven, did, class| {
        ndev += 1;
        println!("pci     : {bus:02x}:{dev:02x}.0 {ven:04x}:{did:04x} class={class:#08x}");
    });
    let rng = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_RNG_DEVICES);
    match rng {
        Some(d) => println!(
            "pci     : {ndev} Geraet(e); virtio-rng gefunden (RID {:#06x}, MMIO-BAR {:#x}, Bus-Master {})",
            d.rid(),
            d.bars.iter().copied().find(|&b| b != 0).unwrap_or(0),
            hal::pcie::bus_master_enabled(&d)
        ),
        None => println!("pci     : {ndev} Geraet(e); kein virtio-rng"),
    }
    println!("pci     : {}", if ndev > 0 { "ALL PASS" } else { "FAILURES" });

    // --- E-Rest 3: Registerfenster oberhalb von 4 GiB in die Karte holen ----------------------
    //
    // **Der Pfad von der BAR-Ermittlung zur Seitentabelle.** Sobald die Maschine Speicher
    // oberhalb 4 GiB hat, legt SeaBIOS die 64-Bit-BARs in ein Loch bei 448 GiB -- gemessen:
    // `cr2 = 0x70_0000_0014`, also `DEVICE_STATUS` des virtio-Transports, drei Zeilen nach
    // dieser hier. Die Karte endete bei 4 GiB, die Seite war schlicht nicht da.
    //
    // Abgebildet wird **gezielt**, nicht flaechig: nur die 2-MiB-Bloecke, in denen wirklich ein
    // BAR liegt. Eine flaechige 512-GiB-Karte waere billiger zu schreiben (512 PDPT-Eintraege,
    // kein zusaetzlicher Rahmen) -- und haette zur Folge, dass jede erfundene Adresse in 448 GiB
    // Nichts auf eine gueltige, beschreibbare Seite trifft.
    //
    // Die Zeile ist **sprechfaehig**: sie nennt beide Richtungen. Positivkontrolle -- jedes
    // abgebildete Fenster wird durch die Seitentabellen aufgeloest und muss auf sich selbst
    // zeigen. Negativkontrolle -- dieselbe Frage an ein GiB, das niemand abgebildet hat, muss
    // `None` liefern. Ohne die zweite waere ein `ALL PASS` auch dann zu haben, wenn die Karte
    // flaechig alles abbildet; ohne die erste hiesse "0 Fenster oberhalb 4 GiB" dasselbe wie
    // "alles in Ordnung", und das sind zwei verschiedene Aussagen.
    {
        let mut ueber4 = 0usize;
        let mut abgebildet = 0usize;
        let mut aufloesbar = 0usize;
        let mut erste = 0u64;
        hal::pcie::for_each_bar(&mut |base, len| {
            if base.saturating_add(len) <= hal::mmu::LOW_MAPPED_END {
                return; // unter 4 GiB: steht seit `mmu::init_primary`
            }
            ueber4 += 1;
            if erste == 0 {
                erste = base;
            }
            // Kernelsicht, kein Subjekt: das BAR-Fenster wird global eingeblendet, damit der
            // Kernel enumerieren kann. Der Weg geht ueber `system`, weil nur dort der Zeuge
            // herstellbar ist (E-Rest 3g) -- rustc haelt die Bindung, nicht ein Skript.
            if system::map_device_window_global(base, len) {
                abgebildet += 1;
                if hal::mmu::resolve_global(base) == Some(base) {
                    aufloesbar += 1;
                }
            }
        });
        // Das oberste GiB des einen benutzten PML4-Eintrags: dort bildet dieser Kernel
        // grundsaetzlich nichts ab. Faende der Aufloeser hier etwas, waere die Karte flaechig
        // geworden -- und die Positivkontrolle darueber wertlos.
        let leer = hal::mmu::resolve_global(511 * (1 << 30)).is_none();
        let (dev_gib, ram_gib, gib_seiten) = hal::mmu::high_map_report();
        println!(
            "himap   : BAR-Fenster oberhalb 4 GiB: {ueber4} gefunden, {abgebildet} abgebildet, \
             {aufloesbar} durch die Seitentabellen aufloesbar (erstes {erste:#x}); \
             hohe GiB: {dev_gib} Geraet + {ram_gib} RAM, 1-GiB-Seiten={gib_seiten}, \
             unabgebildetes GiB bleibt unabgebildet={leer}"
        );
        println!(
            "himap   : {}",
            if abgebildet == ueber4 && aufloesbar == ueber4 && leer { "ALL PASS" } else { "FAILURES" }
        );
    }

    // --- virtio-pci auf x86, Teil 1 von 2 (A-5.2) -------------------------------------------
    //
    // Bis 2026-08-01 lag der virtio-Treiber unter `hal/aarch64/`, obwohl nichts daran ARM-
    // spezifisch ist: virtio-pci ist ein PCI-Standard. Er liegt jetzt arch-neutral
    // (`hal::virtio`) und laeuft hier zum ersten Mal auf x86.
    //
    // **Diese Stelle ist bewusst gewaehlt: VOR dem VT-d-Aufbau.** Hier steht die Root-Tabelle
    // noch nicht auf Default-Block, das Geraet darf also DMAen. Belegt wird damit der TRANSPORT
    // in voller Laenge -- Capability-Liste, Handshake, Feature-Aushandlung einschliesslich
    // VIRTIO_F_ACCESS_PLATFORM, Virtqueue, und dass das Geraet wirklich Bytes liefert.
    //
    // Teil 2 steht nach dem IOMMU-Aufbau und prueft die Gegenrichtung. Zwei Teile, weil ein Test,
    // der nur den Erfolgsfall zeigt, nicht sagen kann, ob die Sperre danach wirkt -- und einer,
    // der nur die Sperre zeigt, nicht, ob ueberhaupt etwas funktioniert haette.
    #[cfg(feature = "selftest")]
    {
        let (ok, adv, w) = virtio_versuch(rng.as_ref(), 50_000_000);
        println!(
            "virtio  : Transport (vor VT-d): Caps={} Geraet-DMA={} ({} Byte)",
            ok as u8, adv as u8, w
        );
        VIRTIO_XPORT_OK.store(rng.is_none() || (ok && adv && w > 0), Ordering::Release);
    }

    // --- virtio-blk und virtio-net (A-5.2) --------------------------------------------------
    //
    // Der RNG belegt den Transport, aber nur eine Richtung: das Geraet SCHREIBT in unseren
    // Speicher. Ob es ihn auch LESEN kann, sagt er nicht -- er liest nie etwas von uns. Genau
    // daran haengen die beiden Geraete hier:
    //
    //   * `blk` schickt eine dreigliedrige Deskriptorkette. Glied 0 ist der Anfragekopf mit der
    //     Sektornummer, und ihn muss das GERAET LESEN. Kommt er nicht an, weiss es nicht einmal,
    //     was es liefern soll. Ein Sektor mit der richtigen Magie belegt beide Richtungen in
    //     einer Transaktion -- und dazu, dass das Geraet `next` verfolgt.
    //   * `net` hat ZWEI Queues mit verschiedenem `queue_notify_off`. Ein Treiber, der die
    //     Notify-Adresse der ersten fuer beide benutzt, weckt das Geraet auf der falschen Seite;
    //     bei einem Einqueue-Geraet kann dieser Fehler strukturell nicht auftreten.
    //
    // Beides steht wie der RNG-Transport VOR dem VT-d-Aufbau -- hier darf das Geraet DMAen.
    let blk_dev = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_BLK_DEVICES);
    // **Nicht mehr unter `selftest`** (A-5.3): dieses Geraet wird jetzt auch im Produktivkernel
    // ZUGETEILT, nicht nur getestet. Ein Fund, den nur die Pruefkonfiguration macht, waere eine
    // Zuteilung, die es im ausgelieferten Kernel gar nicht gibt.
    let net_dev = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_NET_DEVICES);
    #[cfg(feature = "selftest")]
    {
        match virtio_blk_versuch(blk_dev.as_ref(), 50_000_000) {
            Some(r) => {
                println!(
                    "vblk    : Lesen (vor VT-d): Caps={} Kapazitaet={} Sektor(en) Status={:#04x} \
                     geschrieben={} Byte Magie={:#018x} (erwartet {:#018x})",
                    r.features_ok as u8, r.capacity_sectors, r.status, r.written, r.first_word,
                    BLK_MAGIC
                );
                VBLK_READ_OK.store(r.ok(BLK_MAGIC) && r.capacity_sectors > 0, Ordering::Release);
            }
            // Kein Geraet ist KEIN Fehlschlag und auch kein Bestehen. Die Lade-Suite reicht
            // keine Platte herein; ein stilles PASS an dieser Stelle waere genau die Sorte
            // Zeile, die spaeter jemand als Beleg liest, obwohl nichts geprueft wurde.
            None => {
                println!("vblk    : SKIP (kein virtio-blk-Geraet am Bus)");
                VBLK_READ_OK.store(true, Ordering::Release); // nicht anwendbar, nicht bestanden
            }
        }
        match virtio_net_versuch(net_dev.as_ref(), 50_000_000) {
            Some(r) => {
                println!(
                    "vnet    : MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}; Caps={} \
                     Sendepuffer abgeholt={} Rahmen empfangen={} ({} Byte) ARP-Antwort von \
                     {}.{}.{}.{}={}",
                    r.mac[0], r.mac[1], r.mac[2], r.mac[3], r.mac[4], r.mac[5],
                    r.features_ok as u8, r.tx_used as u8, r.rx_used as u8, r.rx_len,
                    r.sender_ip[0], r.sender_ip[1], r.sender_ip[2], r.sender_ip[3],
                    r.arp_reply as u8
                );
                println!(
                    "vnet    : {} (A-5.2: virtio-net auf x86 -- zwei Queues mit getrennter \
                     Notify-Adresse; die ARP-ANTWORT auf die eigene Anfrage belegt beide \
                     Richtungen inhaltlich, ein bloss gefuellter Puffer koennte Restspeicher sein)",
                    if r.features_ok && r.tx_used && r.rx_used && r.arp_reply {
                        "ALL PASS"
                    } else {
                        "FAILURES"
                    }
                );
                VNET_BEFUND
                    .gemessen(r.features_ok && r.tx_used && r.rx_used && r.arp_reply);
            }
            None => {
                println!(
                    "vnet    : SKIP (keine virtio-net-Karte am Bus -- die Frage ist hier nicht \
                     entscheidbar, und das ist weder bestanden noch durchgefallen)"
                );
                // **Bis 2026-08-25 stand hier `store(true)`** -- der einzige Netzbeleg des
                // Systems meldete sich als BESTANDEN, wenn das Geraet fehlt. Seither sagt die
                // Sonde `NichtGefahren`, `all_done()` laesst sie durch (ein SKIP darf den Bericht
                // nicht aufhalten), und ob ein SKIP hier hinnehmbar ist, entscheidet die SUITE.
                VNET_BEFUND.uebersprungen();
            }
        }
    }

    // --- IOMMU (VT-d): Bring-up mit Default-Block ---
    // Über denselben Weg wie auf ARM: der Kernel kennt nur das `DmaEnforcer`-Trait.
    let up = system::dma_enforcer_init();
    let iommu_ok = if hal::vtd::present() {
        println!(
            "iommu   : VT-d Version {:#x} CAP {:#x} (Root-Tabelle mit lauter 'not present' = Default-Block)",
            hal::vtd::version(),
            hal::vtd::cap()
        );
        // Schritt 1 des VT-d-Aufbaus: Fähigkeiten EINMAL lesen, protokollieren, und jede
        // spätere Bit-Entscheidung daraus ableiten statt an der Verwendungsstelle. Die Lektion
        // stammt von der ARM-Seite (`STE.S1STALLD` war nur unter einer Bedingung zulässig, und
        // die Einheit übersetzte deshalb gar nicht, ohne dass es jemand sagte).
        if let Some(c) = hal::vtd::VtdCaps::read() {
            println!(
                "vtdcaps : SAGAW {:#x} -> gewaehlt {} Bit ({} Level), MGAW {} Bit, Domains {}, CM={} RWBF={} ECAP.C={} QI={} IR={} SC={} ScalableMode={}",
                c.sagaw, c.agaw_bits, c.agaw_levels, c.mgaw_bits, c.num_domains,
                c.caching_mode, c.rwbf, c.coherent_walk, c.queued_invalidation,
                c.interrupt_remapping, c.snoop_control, c.scalable_mode
            );
            println!(
                "vtdcaps : Fault-Recording {} Register @ +{:#x}, Overflow(FSTS.PFO)={}; Eingangsgrenze {:#x}; brauchbar={}",
                c.num_fault_regs, c.fault_reg_offset, hal::vtd::fault_overflow(),
                c.input_limit(), c.usable()
            );
            // Was der Enforcer NICHT hat, gehoert genauso ins Log wie das, was er hat.
            println!(
                "vtdcaps : {} (Schritt 1: Faehigkeiten gelesen -- als Minimum ueber ALLE Einheiten (B-3.3), \
                 nicht nur ueber Einheit 0. IR/CFI stehen seit B-3.2; attach TEILT ZU (A-5.1: die \
                 Treiber-PD bekommt eine uebersetzte DMA-Region); die IOVA-Fensterwahl meidet den \
                 Interrupt-Nachrichtenbereich (B-3.4). Offen: QI und IR laufen weiterhin nur auf \
                 Einheit 0 -- fuer die Uebersetzung folgenlos, fuer Interrupt Remapping nicht)",
                if c.usable() { "ALL PASS" } else { "FAILURES" }
            );
        }
        // Schritt 2: DMAR-Auswertung + Gruppenbildung. Der Selbsttest laeuft gegen eine
        // EINGESPEISTE Tabelle/Topologie -- auf dem realen Aufbau (flach, keine RMRR) wuerden
        // Ausschlusspfad und Gruppenfaelle nie ausgefuehrt.
        #[cfg(feature = "selftest")]
        {
            let st = super::dmar_selftest::run();
            println!(
                "vtdgrp  : Selbsttest: parse={} Catch-all-zuletzt={} Bridge-Scope-Subhierarchie={} Gruppen={} Alias-Mengen={} RMRR-ausgeschlossen={} Firmware-Muell-abgefangen={} Oracle={}",
                st.parse_ok, st.catch_all_last, st.bridge_scope_subtree, st.groups_ok,
                st.alias_ok, st.rmrr_excluded, st.malformed_caught, st.audit
            );
            super::dmar_selftest::report_real();
            println!("vtdgrp  : {}", if st.ok() { "ALL PASS" } else { "FAILURES" });
        }
        let inv = hal::vtd::invalidate_context_cache();
        println!(
            "iommu   : Uebersetzung aktiv={} (GSTS.TES), Kontext-Cache-Invalidierung quittiert={inv}, dma_audit={}",
            system::dma_enforcer().is_active(),
            system::dma_enforcer().audit()
        );
        // B-3.1: laeuft die Queued Invalidation, und traegt sie den Interrupt-Entry-Cache?
        // Der IEC ist der Punkt: fuer ihn gibt es KEINEN Registerpfad. Ohne ihn waere eine
        // geaenderte IRTE nie wirksam zu invalidieren -- und damit Interrupt Remapping (B-3.2)
        // nicht zulaessig, sondern nur scheinbar aktiv.
        let qi = hal::vtd::qi_active();
        let iec = hal::vtd::invalidate_iec_global();
        println!(
            "qi      : Queued Invalidation aktiv={qi}; Kontext-Cache ueber die Warteschlange \
             quittiert={inv}; Interrupt-Entry-Cache invalidiert={iec} (fuer den gibt es KEINEN \
             Registerpfad -- deshalb ist B-3.1 Vorbedingung von B-3.2, nicht Geschmackssache)"
        );
        println!(
            "qi      : {} (B-3.1: Invalidierung laeuft ueber die Warteschlange, und der \
             Interrupt-Entry-Cache ist erreichbar)",
            if qi && iec { "ALL PASS" } else { "FAILURES" }
        );
        // B-3.2: Interrupt Remapping aktiv UND Compatibility-Format zu. Beides zusammen, denn
        // IR mit erlaubtem CFI ist eine offene Tuer an der Seite: das Compatibility-Format ist
        // der alte, nicht-remappte Nachrichtenpfad und umgeht die Tabelle vollstaendig.
        let ir = hal::vtd::ir_active();
        let cfi = hal::vtd::cfi_blocked();
        println!(
            "ir      : Interrupt Remapping aktiv={ir} (GSTS.IRES), Compatibility-Format \
             abgeschaltet={cfi} (GSTS.CFIS==0); Tabelle mit lauter 'not present' = Default-Block, \
             ein Geraet ohne IRTE kann keinen Interrupt ausloesen"
        );
        println!(
            "ir      : {} (B-3.2: ohne IR koennte ein durchgereichtes Geraet beliebige \
             Interrupt-Nachrichten erzeugen -- MSI ist eine DMA-Schreibung, die die Uebersetzung \
             gar nicht ansieht)",
            if ir && cfi { "ALL PASS" } else { "FAILURES" }
        );
        // --- Die ARCH-NEUTRALE Gesundheitsaussage (2026-08-17) -------------------------------
        //
        // Bis hierher hat jede Architektur in ihren eigenen Worten berichtet: aarch64 als `smmu`
        // (IDR0, SIDSIZE, SMMUEN, CMD_SYNC-Round-Trip, Event-Queue, GERROR), x86 als
        // `iommu`/`vtdcaps`/`qi`/`ir`. Beide Fassungen sind vollstaendig -- aber es gab KEINEN
        // Satz, der auf beiden Seiten dasselbe bedeutet, und genau das hatte `todo.md` vorab als
        // Warnsignal benannt: zwei Formulierungen sind zwei Entwuerfe.
        //
        // `inv` wird HEREINGEREICHT statt hier neu erhoben: eine Gesundheitsabfrage darf keine
        // Invalidierung ausloesen. Ein Lesen, das seinen Gegenstand veraendert, ist keine
        // Beobachtung.
        let hl = hal::iommu::health(inv);
        println!(
            "iohealth: present={} translation={} units={}/{} round_trip={} faults_empty={} \
             config_errors={} hw_error={:#x}",
            hl.present, hl.translation_enabled, hl.units_speaking, hl.units,
            hl.invalidation_round_trip, hl.faults_empty, hl.config_errors, hl.hw_error
        );
        println!(
            "iohealth: speaking={} verdict={:?} -- `faults_empty` zaehlt NUR mit round_trip: eine \
             tote Einheit meldet ebenfalls eine leere Warteschlange (dieselbe Form wie die leere \
             Event-Queue ohne `CD.R`)",
            hl.speaking(), hl.verdict()
        );
        println!("iohealth: {}", if hl.ok() { "ALL PASS" } else { "FAILURES" });

        up && system::dma_enforcer().is_active() && inv && qi && iec && ir && cfi
            && system::dma_enforcer().audit() == 0
            && hl.ok()
    } else {
        println!("iommu   : keine ACPI-DMAR -> Plattform ohne IOMMU");
        // Auch der Abwesenheitsfall bekommt die Zeile -- sonst waere sie auf einer Plattform ohne
        // IOMMU schlicht nicht da, und „fehlt" ist von „bestanden" nicht zu unterscheiden.
        let hl = hal::iommu::health(false);
        println!("iohealth: {:?} (Plattform ohne IOMMU)", hl.verdict());
        println!("iohealth: SKIP");
        false
    };
    println!("iommu   : {}", if iommu_ok { "ALL PASS" } else { "SKIP/FAILURES" });
    let (nc, nthreads, per_core, tbl) = system::configure(ncpu);
    println!(
        "apic    : {} (LAPIC-ID {}); x2APIC adressiert 32-Bit-IDs, xAPIC nur 8 -> 255 Kerne",
        if hal::intc::x2apic_active() { "x2APIC (MSR-Pfad)" } else { "xAPIC (MMIO-Pfad)" },
        hal::intc::lapic_id()
    );
    println!(
        "sched   : {nc} Kern, {nthreads} Thread-Slots ({per_core} hostbar), Tabellen {} KiB aus dem RAM",
        tbl >> 10
    );
    // Der Bootkern MUSS die Kern-ID haben, für die `configure` Tabellen angelegt hat —
    // sonst hätte sein Scheduler keinen Speicher und der erste Trap fände keinen Thread.
    let boot_core = hal::cpu::core_id();
    if boot_core >= nc {
        println!("bringup : FAILURES (Bootkern hat LAPIC-ID {boot_core}, erwartet < {nc})");
        hal::power::system_off();
    }
    system::init_core();

    // --- virtio-pci auf x86, Teil 2 von 2 (A-5.2 / E) ---------------------------------------
    //
    // Dieselbe Anfrage, jetzt mit aufgebauter VT-d-Einheit und Root-Tabelle auf Default-Block.
    // Das Geraet hat KEINE Zuteilung (`attach` liefert auf x86 noch None -- B-3.3/B-3.4), sein
    // Bus-Master-Zugriff muss also ins Leere laufen. Kaeme er durch, waere das ein
    // Isolationsbruch, und dieser Test wuerde ihn sehen.
    //
    // Kurze Poll-Schranke: hier wird ein AUSBLEIBEN erwartet: auf eine Antwort zu warten, die per
    // Entwurf nie kommt, kostet sonst je Lauf hunderte Millisekunden.
    #[cfg(feature = "selftest")]
    {
        let vorher = hal::iommu::drain_faults();
        let (ok, adv, w) = virtio_versuch(rng.as_ref(), 2_000_000);
        let faults = hal::iommu::drain_faults();
        println!(
            "virtio  : Sperre (nach VT-d, Geraet nicht zugeteilt): Caps={} Geraet-DMA={} ({} Byte) \
             VT-d-Faults davor={} danach={}",
            ok as u8, adv as u8, w, vorher, faults
        );
        let blockiert = rng.is_none() || !adv;
        println!(
            "virtio  : {} (A-5.2: der virtio-pci-Transport laeuft auf x86 -- arch-neutraler Treiber, \
             Caps, Handshake, Feature-Aushandlung inkl. VIRTIO_F_ACCESS_PLATFORM, Virtqueue, echte \
             Geraete-DMA. Und nach dem VT-d-Aufbau kommt dasselbe Geraet OHNE Zuteilung nicht mehr \
             durch -- beide Richtungen belegt, nicht nur die bequeme)",
            if VIRTIO_XPORT_OK.load(Ordering::Acquire) && blockiert { "ALL PASS" } else { "FAILURES" }
        );
        VIRTIO_OK.store(VIRTIO_XPORT_OK.load(Ordering::Acquire) && blockiert, Ordering::Release);

        // Dieselbe Gegenprobe fuer das Blockgeraet -- und sie sagt etwas ANDERES als die daneben.
        //
        // Der RNG belegt, dass das Geraet nicht mehr in unseren Speicher SCHREIBEN kann. Dass es
        // ihn auch nicht mehr LESEN kann, folgt daraus nicht: das waere eine Annahme ueber die
        // Root-Tabelle ("not present sperrt beide Richtungen"), und Annahmen ueber
        // Hardwaresemantik haben in diesem Projekt schon zweimal danebengelegen (STE.S1STALLD,
        // GCMD als Read-Modify-Write). Hier ist sie pruefbar: bleibt das Statusbyte auf 0xff,
        // hat das Geraet nicht einmal den Anfragekopf zu Gesicht bekommen.
        if let Some(r) = virtio_blk_versuch(blk_dev.as_ref(), 2_000_000) {
            let faults2 = hal::iommu::drain_faults();
            println!(
                "vblk    : Sperre (nach VT-d, Geraet nicht zugeteilt): abgeschlossen={} \
                 Status={:#04x} (0xff = vom Geraet unberuehrt) VT-d-Faults={}",
                r.used_advanced as u8, r.status, faults2
            );
            let blk_blockiert = !r.used_advanced && r.status == 0xff;
            println!(
                "vblk    : {} (A-5.2: virtio-blk auf x86 -- dreigliedrige Deskriptorkette, das \
                 Geraet LIEST den Anfragekopf und liefert den angeforderten Sektor mit der \
                 erwarteten Magie. Nach dem VT-d-Aufbau erreicht es den Kopf nicht mehr -- damit \
                 ist auch die LESERICHTUNG gesperrt belegt, die der RNG-Test nicht zeigen kann)",
                if VBLK_READ_OK.load(Ordering::Acquire) && blk_blockiert {
                    "ALL PASS"
                } else {
                    "FAILURES"
                }
            );
            VBLK_OK.store(
                VBLK_READ_OK.load(Ordering::Acquire) && blk_blockiert,
                Ordering::Release,
            );
        } else {
            // Die SKIP-Zeile steht bereits oben (Teil 1) -- hier nur die Abschlussbedingung.
            VBLK_OK.store(true, Ordering::Release);
        }
    }

    // --- A-5.1: das Blockgeraet als zuteilbar anmelden -----------------------------------------
    //
    // Ab hier ist es **nicht mehr die Sache des Kerns**, was mit dem Geraet geschieht. Er hat es
    // enumeriert -- das ist seine Aufgabe, denn ein Lauf ueber alle Busse sieht jede Maschine --
    // und legt jetzt drei Tatsachen hin: diese Konfigurationsraum-Seite, dieses Registerfenster,
    // diese Requester-ID. Wer das bekommt, sagt das Manifest, nicht dieser Code.
    //
    // Der Kern weiss hier ausdruecklich **nicht**, dass es virtio ist. Er ruft `probe_transport`
    // nur, um herauszufinden, in WELCHEM BAR die Strukturen liegen, die er mitgeben muss -- eine
    // Frage der Zuteilung, keine des Bedienens. Die Zeile darunter waere ohne diesen Umweg
    // "irgendein BAR", und ein Treiber mit dem falschen Fenster faende sein Geraet nicht.
    // **A-5.3: es sind jetzt MEHRERE.** Bis A-5.2 wurde genau ein Geraet angeboten, mit der
    // ausdruecklichen Begruendung, dass jede Auswahl unter mehreren eine im Kernel versteckte
    // Politik waere, solange das Manifest nicht sagen kann, WELCHES. Der Selektor im Eintrag
    // (A-5.3) nimmt dieser Begruendung die Grundlage -- also wird jetzt alles angeboten, was
    // gefunden wurde, und die Auswahl steht im Autoritaetsdokument.
    //
    // Die Hersteller-/Geraete-/Klassen-IDs gehen **nur zur Auswahl** mit. Der Kern bedient das
    // Geraet weiterhin nicht und weiss nicht, was virtio ist; er reicht durch, was auf jeder
    // PCI-Maschine an derselben Stelle im Konfigurationsraum steht.
    let anbieten = |d: &hal::pcie::PciDevice| {
        let Some(t) = hal::virtio::probe_transport(d) else { return };
        let common = t.common_addr();
        let mut bar = 0u64;
        let mut bar_len = 0u64;
        let mut bar_index = usize::MAX;
        for i in 0..6 {
            let b = d.bars[i];
            if b == 0 {
                continue;
            }
            let len = hal::pcie::bar_size(d, i);
            if len != 0 && common >= b && common < b + len {
                bar = b;
                bar_len = (len + 0xfff) & !0xfff;
                bar_index = i; // E11: die MSI-X-Pruefung fragt nach dem INDEX, nicht der Adresse
                break;
            }
        }
        if bar == 0 {
            return;
        }
        // --- Stufe B / E11: die MSI-X-Tabelle, und die Bedingung, unter der das Geraet ueberhaupt
        // vergeben wird ---------------------------------------------------------------------
        //
        // **Liegt die Tabelle in der BAR, die der Treiber bekommt, wird das Geraet NICHT
        // angeboten.** Er koennte sonst Adresse und Datenwort selbst schreiben und damit waehlen,
        // wo sein Interrupt landet -- SVT/SID faengt die Zustellung, aber es gibt keinen Grund,
        // sich auf die zweite Linie zu verlassen, wenn die erste umsonst ist.
        //
        // Dass es heute nie zutrifft (virtio-pci legt die Tabelle in eine andere BAR als die
        // Common-Config), ist genau der Grund, warum die Bedingung hier steht: *eine Eigenschaft,
        // die aus einer Groessenrelation folgt statt aus der Struktur, verschwindet beim naechsten
        // Messwert.* Aussparen waere die Alternative und ist schlechter -- seitengranulare Loecher
        // in einer Region, die der Treiber sonst ganz besitzt.
        let msix = hal::pcie::msix_find(d);
        let (msix_cap, msix_table, msix_eintraege) = match msix {
            Some(m) => {
                if hal::pcie::msix_in_bar(&m, bar_index) {
                    println!(
                        "devassign: RID {:#06x} NICHT angeboten -- die MSI-X-Tabelle liegt in der \
                         BAR {bar_index}, die der Treiber bekaeme (E11: der Vektor ist keine \
                         Autoritaet des Treibers)",
                        d.rid()
                    );
                    return;
                }
                let basis = d.bars[m.bar as usize];
                if basis == 0 {
                    // BAR not assigned -> no interrupt, but the device is still usable.
                    // **`m.cap` is kept anyway**, and that is the point: it is the only thing that
                    // tells "this device has no MSI-X at all" (lawful polling) apart from "it
                    // offers MSI-X and did not get a vector" (a failed grant). Answering `(0,0,0)`
                    // here made the two indistinguishable downstream -- the same shape as C2's
                    // requested-beside-granted.
                    (m.cap, 0, 0)
                } else {
                    (m.cap, basis + m.offset as u64, m.eintraege)
                }
            }
            None => (0, 0, 0),
        };
        let ok = system::offer_driver_device(system::DriverDevice {
            rid: d.rid(),
            cfg_page: hal::pcie::cfg_page(d),
            bar,
            bar_len,
            msix_cap,
            msix_table,
            msix_eintraege,
            vendor: d.vendor,
            device: d.device,
            class: d.class,
        });
        println!(
            "devassign: zuteilbar: RID {:#06x} {:04x}:{:04x} Klasse {:#08x}, \
             Konfigurationsseite {:#x}, BAR {:#x}+{:#x}{}",
            d.rid(),
            d.vendor,
            d.device,
            d.class,
            hal::pcie::cfg_page(d),
            bar,
            bar_len,
            if ok { "" } else { " -- ABGEWIESEN, Angebotsliste voll" }
        );
    };
    if let Some(d) = blk_dev.as_ref() {
        anbieten(d);
    }
    if let Some(d) = net_dev.as_ref() {
        anbieten(d);
    }
    println!(
        "devassign: {} Geraet(e) angeboten (A-5.1/A-5.3: der Kern enumeriert und teilt zu; was fuer \
         Geraete das sind, weiss er nicht -- das Manifest sagt, WER WELCHES bekommt, und die \
         Reihenfolge dieser Liste ist ausdruecklich KEINE Zusage)",
        system::offered_device_count()
    );

    // --- Zyklenzaehler (Stufe 1) ---
    //
    // Das Primitiv, das sowohl die per-Thread-Abrechnung als auch die Messung kritischer
    // Sektionen braucht. Geprueft wird hier nur, was ohne weitere Infrastruktur pruefbar ist:
    // dass der Zaehler laeuft, monoton ist, plausibel kalibriert und -- getrennt davon -- ob er
    // invariant ist. Der letzte Punkt ist keine Kosmetik: ein nicht-invarianter TSC aendert
    // seine Rate mit dem P-State, und Zeitdifferenzen waeren dann keine Zeit, sondern eine
    // Funktion des Taktverhaltens. Das sieht im Messlauf plausibel aus.
    {
        let inv = hal::timer::invariant_tsc();
        let hz = hal::timer::cycles_per_sec();
        let c0 = hal::timer::cycles();
        let mut spin = 0u64;
        while hal::timer::cycles().wrapping_sub(c0) < hz / 1000 {
            spin += 1; // ~1 ms
        }
        let c1 = hal::timer::cycles();
        let d = c1.wrapping_sub(c0);
        // Aufloesung: die kleinste messbare Differenz zweier aufeinanderfolgender Stempel.
        let a = hal::timer::cycles();
        let b = hal::timer::cycles();
        let grain = b.wrapping_sub(a);
        println!(
            "cycles  : invariant-TSC={inv} {} MHz; 1-ms-Fenster = {d} Zyklen ({spin} Iterationen); Aufloesung {grain} Zyklen (Tick-Uhr: {} Zyklen)",
            hz / 1_000_000,
            hz / TICK_HZ
        );
        // Invarianz ist **Telemetrie, keine Bestehensbedingung** -- dieselbe Mittelstellung wie
        // bei den undeklarierten Geraete-Adressbreiten. TCG unterstuetzt `invtsc` nicht
        // ("TCG doesn't support requested feature: CPUID[80000007h].EDX.invtsc"), die Emulation
        // kann die Eigenschaft also gar nicht zusagen. Sie zur Bedingung zu machen hiesse
        // entweder, den Test auf dieser Plattform dauerhaft rot zu lassen, oder die Pruefung
        // wegzulassen -- und Letzteres waere die stillschweigende Annahme, die hier gerade
        // vermieden wird. Gefuehrt und ausgewiesen: auf einer Plattform ohne Zusage sind
        // Zyklenzahlen ein Anhaltspunkt, keine Abrechnungsgrundlage.
        if !inv {
            // Wichtig: das ist eine Aussage ueber DIESE Plattform, nicht ueber den Entwurf.
            // Unter KVM mit `-cpu host,+invtsc` meldet dieselbe Pruefung `true`, und die
            // Zielhardware (Zen/EPYC, seit Zen durchgehend) hat Invariant TSC ohnehin. Ohne
            // diesen Zusatz verfestigt sich sonst die Lesart "wir koennen nicht abrechnen",
            // obwohl die Einschraenkung am Emulator haengt.
            println!("cycles  : HINWEIS invariant-TSC nicht zugesagt (TCG kann es nicht) -> Zyklenwerte hier indikativ; unter KVM/-cpu host,+invtsc und auf Zen/EPYC ist die Zusage vorhanden");
        }
        let ok = hz > 1_000_000 && d >= hz / 2000 && d <= hz / 250 && grain > 0;
        println!(
            "cycles  : {} (serialisierender Zeitstempel: rdtscp+lfence, gegen den PIT kalibriert, auf EINEM Kern gemessen; Invarianz gefuehrt statt angenommen)",
            if ok { "ALL PASS" } else { "FAILURES" }
        );
    }

    // --- Arch-neutrale DMA-Tests (ext-38) ---
    //
    // Dieselben Funktionen, die der ARM-Lauf ruft -- nicht nachgebaute. Bis hierher lagen sie in
    // `threads/mod.rs`, und das Modul ist aarch64-only: auf x86 haben sie nicht geskippt, es gab
    // sie nicht. Ein Test, den es auf einer Architektur nicht gibt, kann dort auch nicht gruen
    // werden; das Abnahmekriterium war so nicht einloesbar.
    #[cfg(feature = "selftest")]
    {
        let live_rid = hal::pcie::find(hal::pcie::VIRTIO_VENDOR, &hal::pcie::VIRTIO_RNG_DEVICES)
            .map(|d| d.rid())
            .unwrap_or(0);
        let w = crate::dmatests::run_dmawin(live_rid);
        // **Drei unterscheidbare Faelle, nicht zwei.** Die Vorgaengerzeile druckte `=1` und
        // bedeutete damit wahlweise „geprueft und frei" oder „gar nicht geprueft" -- im schwachen
        // Zweig gab der Pruefer `true` zurueck, obwohl das Fenster `[0, 512 GiB)` den Sperrbereich
        // ENTHAELT. Wer aus einer solchen Zeile liest, kann den Unterschied nicht sehen; deshalb
        // steht das Urteil jetzt im Klartext und die Begruendung dahinter.
        println!(
            "dmawin  : B-3.4 Fenster gegen den Interrupt-Nachrichtenbereich \
             (0xFEE0_0000..0xFEF0_0000): {} -- dorthin geschriebene DMA waere fuer VT-d eine \
             Interrupt-Nachricht und wuerde gar nicht uebersetzt: kein Fault, keine Fehlerzeile, \
             nur Daten, die nirgends ankommen",
            crate::dmatests::msi_klartext(w.msi.policy)
        );
        println!(
            "dmawin  : B-3.4 belegte-DMA-Kontexte={} davon-schwaches-Fenster={} \
             Fenster-weicht-von-der-Politik-ab={} eingetragenes-Fenster-im-Sperrbereich={} \
             (das schwache Fenster ist [0, 512 GiB) und enthaelt den Sperrbereich -- die Zahl \
             erklaert ein VERLETZT, statt es zu ersetzen)",
            w.msi.live, w.msi.weak, w.msi.drift, w.msi.live_overlaps
        );
        println!("dmawin  : 32-Bit-Geraet-abgewiesen={} Fenster-voll-abgewiesen={} Kontext-danach-intakt={} balanciert={} Geraete-ohne-deklarierte-Adressbreite={}",
            w.narrow, w.exhausted, w.intact, w.balanced, w.undeclared);
        println!("dmawin  : {}", w.verdict().wort());
        let tk = crate::dmatests::run_dmatok(live_rid);
        println!("dmatok  : attach-installierte-Uebersetzung={} delete-ohne-detach-baut-ab={} Region-danach-frei={} unbestaetigte-Stilllegung-bleibt-pending={} Audit-Code-7-haelt={}",
            tk.attached, tk.tears, tk.freed, tk.pending, tk.audit7);
        println!("dmatok  : {}", if tk.ok { "ALL PASS" } else { "FAILURES" });
    }

    #[cfg(feature = "selftest")]
    {
        if !spawn_demo() {
            println!("bringup : FAILURES (Demo-Aufbau fehlgeschlagen)");
            hal::power::system_off();
        }
        println!("bringup : 3 Worker + 2 PDs (IPC-Server/Client) eingeplant");
    }

    // --- Root-Task (A-2.1) ---
    //
    // Steht BEWUSST ausserhalb von `selftest`: das hier ist die Aufgabe des Kernels, nicht seine
    // Pruefung. Ohne diesen Aufruf ist `--no-default-features` ein leerer Kernel (todo F2), und
    // genau das war der Grund, warum `selftest` bis hierher in `default` bleiben musste.
    //
    // Die Geraete-Zuteilung passiert **hier drin**: der Root-Task laedt die Treiber-PD, und
    // `endow_from_manifest` zieht ihr Geraet aus der Angebotsliste (A-5.3).
    // A-5.3: der Stand VOR jeder Zuteilung festhalten. Danach ist er nicht mehr feststellbar -- ein
    // vergebenes Geraet wird aus der Angebotsliste genommen, und ohne diese Zahl liesse sich
    // hinterher nicht mehr sagen, ob ueberhaupt eine Auswahl zu treffen war.
    //
    // **Berichtet wird spaeter.** `start_root_task_reported` LAEDT den Root-Task, es fuehrt ihn
    // nicht aus; die Treiber-PD entsteht erst, wenn `init` selbst laeuft und `SYS_LOAD` ruft. Ein
    // Bericht an dieser Stelle haette 0 Zuteilungen gesehen und sie fuer einen Fehlschlag gehalten
    // -- gemessen, nicht vermutet: genau das tat der erste Entwurf.
    ANGEBOTEN_VORHER.store(system::offered_device_count(), Ordering::Release);
    // **C8: der Verifizierer MUSS vor dem Root-Task stehen** -- er ist der erste, der `SYS_LOAD`
    // benutzt. Ohne ihn bekaeme sein erster Ladeversuch `ERR_SERVER_GONE`, und das saehe wie ein
    // Cap-Problem aus statt wie ein fehlender Thread.
    if !crate::verifizierer::starten() {
        println!(
            "verif   : FAILURES (Verifiziererthread liess sich nicht starten -- SYS_LOAD ist damit tot)"
        );
    }
    // **Z8/N1: die Topologie MUSS vor dem Lader gelesen sein** (gemessen 2026-08-20).
    //
    // Sie stand bis heute elf Zeilen weiter unten -- also NACH `start_root_task_reported`. Der
    // Knoten-Gatter des Laders (`zahlenpolitik_gate`) fragte damit eine leere Topologie und wies
    // jeden `numa_node`-Wunsch mit „die Maschine hat keine tragfaehige Topologie" ab, **auf einer
    // Maschine mit zwei Knoten**. Die Zeile war nicht falsch, sie war zu frueh gefragt.
    //
    // Gefunden hat es der neue Positivfall der Lade-Suite: `numa : readable=true nodes=2` in
    // derselben Ausgabe wie `loader : ... readable=false`. Zwei Aussagen ueber dieselbe Groesse in
    // einem Lauf -- die Sorte Widerspruch, die ein Negativtest allein nie zeigt, weil dort BEIDE
    // Zeilen „nein" sagen und man den Grund nicht sieht.
    crate::numa::init();

    let root_ok = crate::loader::start_root_task_reported();
    let _ = root_ok;

    // --- Sekundärkerne starten (INIT-SIPI-SIPI, s. `hal::power`) ---
    //
    // **Z6 stage 1 sits here, and nowhere else.** The property "a physical core belongs to at most
    // one trust domain" cannot be established later by the scheduler: once a sibling is online it
    // is a core the balancer may place anything on. The only place the question is still open is
    // the moment before `cpu_on`.
    // **Z8/N0+N1: die Topologie VOR dem AP-Hochlauf lesen** -- die AP-Stacks sollen gleich
    // knotenlokal belegt werden, und danach waere die Frage schon entschieden.
    let topo = hal::cpu::smt_topology();
    let mut occ = caprock_hal::smt::CoreOccupancy::new();
    let boot_id = hal::cpu::core_id() as u8;
    // Der Bootkern belegt seinen physischen Kern ZUERST -- sonst waere ausgerechnet sein
    // Geschwister das eine Paar, das die Politik durchliesse.
    let boot_claim = occ.claim(&topo, boot_id as u32);
    let mut online_ids = [0u32; caprock_hal::MAX_CPUS];
    let mut online = 1usize;
    online_ids[0] = boot_id as u32;
    let mut suppressed = 0usize;
    if let Some(list) = cpus.as_ref() {
        for i in 0..list.count() {
            let Some(id) = list.id(i) else { continue };
            if id == boot_id {
                continue;
            }
            // **Vor der Stackbelegung**, nicht danach: ein unterdruecktes Geschwister soll auch
            // keine 16 KiB kosten. (Und die Reihenfolge ist die von D0: erst entscheiden, dann
            // Zustand anlegen.)
            match occ.claim(&topo, id as u32) {
                caprock_hal::smt::Claim::Admit => {}
                other => {
                    suppressed += 1;
                    println!("smp     : CPU {id} nicht zugelassen ({other:?}) -- Z6 Stufe 1");
                    continue;
                }
            }
            // **Z8/N2: der erste echte Aufrufer der Platzierungsleiter.** Der Stack eines
            // Sekundaerkerns ist die kernlokalste Struktur, die es gibt -- er wird von genau
            // diesem Kern benutzt und von keinem anderen. Ohne einen Aufrufer waere die Leiter
            // die Falle „ein Negativtest, der eine Eigenschaft absichert, die NIEMAND benutzt".
            let Some(stack) =
                crate::numa::alloc_on_node(AP_STACK_BYTES, 4096, crate::numa::node_of_cpu(id as u32))
            else {
                break;
            };
            let top = stack.base() + stack.len();
            if hal::power::cpu_on(id as u64, ap_entry as *const () as u64, top)
                == hal::power::SUCCESS
            {
                if online < online_ids.len() {
                    online_ids[online] = id as u32;
                }
                online += 1;
            } else {
                println!("smp     : CPU {id} hat sich nicht gemeldet");
            }
        }
    }
    println!("smp     : {online} von {nc} Kern(en) online");

    // --- Z6 Stufe 0: die Topologie MELDEN, Stufe 1: das Urteil darueber ---
    //
    // Das Urteil rechnet aus `online_ids` nach und fragt `occ` NICHT: ein Pruefer, der die
    // Buchfuehrung der Politik befragt, bestaetigt die Politik mit sich selbst.
    let verdict = caprock_hal::smt::judge(&topo, &online_ids[..online.min(online_ids.len())]);
    println!(
        "smt     : topology={topo:?} width={:?} logical={} online={} suppressed={} boot_claim={boot_claim:?}",
        topo.width(),
        cpus.as_ref().map(|c| c.count()).unwrap_or(1),
        online,
        suppressed
    );
    println!(
        "smt     : readable={} no_shared_core={:?} contained={} -- policy=one-thread-per-physical-core \
         (Z6 stage 1). `no_shared_core=None` heisst UNENTSCHEIDBAR, nicht bestanden.",
        verdict.readable, verdict.no_shared_core, verdict.contained
    );
    // **Was diese Zeile NICHT belegt** (und das gehoert an die Zeile, nicht in eine Fussnote):
    // unter QEMU ist die Geschwister-Beziehung EMULIERT -- `-smp cores=n,threads=2` gibt dem Gast
    // die Sicht ueber CPUID, aber die vCPUs sind gewoehnliche Wirtsthreads ohne geteilte
    // Ausfuehrungseinheiten. Geprueft ist damit die POLITIK (wird ein Geschwister erkannt,
    // unterdrueckt, gezaehlt), nie der KANAL. Dieselbe Unterscheidung wie beim SMMU-Befund in
    // ADR 0008 (`docs/invariants.md` §6).
    println!("smt     : {}", if verdict.ok() { "ALL PASS" } else { "FAILURES" });
    SMT_OK.store(verdict.ok(), Ordering::Release);

    // --- Z8/N1: die Topologie melden. NACH dem Hochlauf, damit die Platzierungszahlen der
    // AP-Stacks schon drinstehen -- eine Zeile mit `speaking=false` sagt ueber Platzierung nichts.
    crate::numa::bericht();
    NUMA_OK.store(crate::numa::urteil(), Ordering::Release);

    // **Die IST-Messung des BSP** — hier und nicht früher, weil `num_cores()` erst jetzt steht
    // und die Zeile sonst gegen eine Kernzahl urteilte, die sich noch ändert. Die Sekundärkerne
    // haben ihre Messung bereits in `ap_entry` abgelegt (vor ihrer Online-Meldung).
    super::ist::messen();

    hal::mmu::seal_cache_granule(); // alle Kerne haben gemeldet (auf x86 wirkungslos)

    // Ab hier schedult der Timer-Interrupt präemptiv; dieser Kontext ist der Idle-Thread.
    hal::cpu::local_irq_enable();
    #[cfg(feature = "selftest")]
    {
        // Einmal, nicht je Runde: `read_archive()` parst den Multiboot-Modulbereich, und diese
        // Schleife dreht Millionen Mal. Die Anwesenheit eines Archivs ändert sich zur Laufzeit
        // ohnehin nicht — sie steht mit dem Bootvorgang fest.
        let archive = crate::loader::read_archive().is_some();
        let mut spins: u64 = 0;
        loop {
            // Die Notbremse steht VOR `reap()`, und sie zaehlt Ticks statt Umdrehungen.
            //
            // Beides aus einem Grund, den zwei Haenger am 2026-08-01 gezeigt haben (Laeufe 115
            // und 235 von 400): sie blieben nach `smp : 4 von 4 Kern(en) online` stehen, und
            // im Log stand KEINE WATCHDOG-Zeile. Unter KVM braucht diese Schleife fuer 50 Mio
            // Umdrehungen den Bruchteil einer Sekunde -- haette sie sich gedreht, waere die
            // Notbremse in 120 s laengst gefallen. Sie hat sich also nicht gedreht.
            //
            // `reap()` nimmt `SCHEDS[core].lock()` und `MEM.lock()`. Blockiert es dort, kam
            // der Zaehler frueher nie wieder an die Reihe: die Notbremse wurde von genau dem
            // ausgehungert, was sie ueberwachen soll. Steht sie davor, faengt sie jeden
            // Stillstand ausserhalb von `reap()` selbst -- insbesondere einen in `all_done()`.
            //
            // Ticks statt Umdrehungen, weil eine Umdrehungszahl von der Taktrate abhaengt:
            // 50 Mio sind unter KVM Millisekunden und unter TCG Minuten, dieselbe Zahl meint
            // also je nach Aufbau etwas anderes. Ticks kommen vom Timer-Interrupt und messen
            // Zeit. Der Zaehler bleibt als zweites Kriterium fuer den Fall, dass gar kein
            // Timer laeuft -- dann gaebe es keine Ticks, und eine reine Tick-Grenze wuerde nie
            // greifen.
            //
            // OFFEN (todo D0): blockiert `reap()` SELBST, hilft auch das nicht -- dieser Faden
            // kommt dann nirgends mehr an. Dafuer braeuchte es eine Notbremse im
            // Timer-Interrupt, also ausserhalb dieses Fadens.
            spins += 1;
            let sekunden = hal::timer::ticks(0) / 100; // 100 Hz
            // **Die #DF-Sonde** (nur mit `--features dfprobe`, eigener QEMU-Lauf). Hier und nicht
            // frueher: erst wenn Ring-3-Threads gelaufen sind, traegt `TSS.rsp0` einen echten
            // EL0-Kernel-Stack -- und nur dann kann der #DF-Bericht auch den WASSERSTAND DES
            // BETROFFENEN STACKS zeigen. Vor dem ersten Ring-3-Wechsel waere `rsp0` null und die
            // Sonde belegte die halbe Aussage.
            #[cfg(all(feature = "dfprobe", not(feature = "dfprobe-wache")))]
            if hal::timer::ticks(0) >= 200 {
                super::ist::df_sonde_ausloesen();
            }
            // Dieselbe Stelle, dieselbe Begruendung -- aber ueber die ECHTE Guard-Page. Sie
            // schliesst sich gegenseitig mit der obigen aus: zwei Sonden in einem Lauf hiesse,
            // dass die zweite nie drankommt (die erste kehrt nicht zurueck), und ein Zweig, der
            // strukturell nie laeuft, ist genau das, was hier belegt werden soll.
            #[cfg(feature = "dfprobe-wache")]
            if hal::timer::ticks(0) >= 200 {
                super::ist::df_wache_ausloesen();
            }
            if sekunden > 60 || spins > 5_000_000_000 {
                let mut _w = [("", crate::befund::Befund::Bestanden); DONE_FLAGS];
                let _ = all_done(archive, Some(&mut _w));
                println!(
                    "bringup : WATCHDOG — nicht alle Aussagen belegt (nach {sekunden}s, {spins} Umdrehungen)"
                );
                #[cfg(feature = "debug-log")]
                print!("bringup : offen waren:");
                #[cfg(feature = "debug-log")]
                for (name, b) in _w.iter() {
                    // **Nur das Offene nennen, und SKIP ist nicht offen.** Ein uebersprungener
                    // Punkt hat den Watchdog nicht verursacht -- ihn hier mitzudrucken schickte
                    // den naechsten Leser an die falsche Stelle.
                    if b.gattert() {
                        print!(" {name}");
                    }
                }
                #[cfg(feature = "debug-log")]
                println!();
                report_and_off(true);
            }
            system::reap();
            drv_service_step(archive);
            park_messen(); // laeuft genau einmal; das Urteil steht danach in `all_done()`
            tiefensonde_messen(); // C7b, ebenso einmalig (Sprechprobe des gemessenen Pfades)
            let _ = quiesce_messen(); // Z23 S1, ebenso einmalig
            let _ = pd_threads_messen(); // Z22 P2 + die Z23-S1-REPLY-Messung, ebenso einmalig
            // Z26/A3, beide einmalig: `handler` misst den Kernel-Pruefpfad (kein fremder Wecker
            // hebt den Handler-Grund auf), `redirect` den ECHTEN Umlauf ueber einen Gast.
            system::handlermess::messen_einmal();
            system::handlermess::redirect_messen();
            // C7. **Nicht in `all_done`**, obwohl beide dort gattern: `used_vspaces()` sperrt eine
            // Tabelle mit 4096 Eintraegen, und die Mangel-Sprechprobe fragt den Allokator. Beides
            // je Umdrehung waere ein Pruefer, der die Sache aushungert, die er beobachtet.
            let _ = ptab_messen();
            let _ = mangel_messen();
            // **Der Sweep laeuft GANZ AM SCHLUSS -- nach jedem anderen Urteil.**
            //
            // Er belegt Ressourcen und weist Anforderungen ab; beides kippt baseline-empfindliche
            // Zeilen (`cycles`, `freeze` mit seiner Positivkontrolle, `color`). Bis heute stand er
            // einfach in der Schleife und lief damit in der ERSTEN Umdrehung, also mitten in den
            // Ladevorgaengen. Das ging gut, solange er nur `spawn_*` fuhr; seit er den echten
            // Ladepfad fahrt (Pfad 7), waere es ein Pruefer, der die Sache aushungert, die er
            // beobachtet -- genau die Ueberlegung, mit der die Kapazitaetskurve ans Ende von
            // `report_and_off` gewandert ist.
            //
            // Der Platz ist deshalb an eine BEDINGUNG gebunden statt an eine Zeile: erst wenn
            // jedes andere Konjunkt von `all_done` steht. Der Sweep gattert selbst mit -- er ist
            // also genau das eine, das dann noch fehlt. Bleibt ein anderes Konjunkt offen, laeuft
            // der Sweep nie, und der Watchdog nennt das echte Konjunkt zuerst statt es hinter
            // einem ungefahrenen Sweep zu verstecken.
            //
            // Genau EIN `all_done` je Umdrehung wie bisher: die Zeugenliste ersetzt den alten
            // Aufruf, und der zweite entsteht nur, wenn ohnehin alles andere steht.
            let mut offen = [("", crate::befund::Befund::Bestanden); DONE_FLAGS];
            let _ = all_done(archive, Some(&mut offen));
            // C8. **An eine BEDINGUNG gebunden, nicht an eine Zeile** -- aus demselben Grund wie
            // der Sweep darunter: die Messung haelt den Verifizierer an und legt SONDEN PDs an.
            // Liefe sie in der ersten Umdrehung, stuende sie mitten in den Ladevorgaengen der
            // Suite, und der Ladepfad braeuchte genau den Thread, den sie pausiert.
            if offen
                .iter()
                .all(|&(name, b)| !b.gattert() || name == "verif" || name == "sweep")
            {
                crate::verifizierer::messen();
            }
            // `verif` steht in dieser Bedingung neben `sweep`, und das ist eine Aussage ueber
            // GEGENPROBEN: faellt die C8-Messung, soll der Watchdog **sie** nennen und nicht
            // zusaetzlich einen Sweep, der nur deshalb offen ist, weil er hinter ihr steht. Ein
            // Ausfall, der zwei Zeilen faerbt, laesst nicht mehr erkennen, welche die Ursache war.
            // Die Reihenfolge bleibt gewahrt: `messen()` steht oben in derselben Umdrehung.
            if offen
                .iter()
                .all(|&(name, b)| !b.gattert() || name == "sweep" || name == "verif")
            {
                let _ = sweep_messen();
                if all_done(archive, None) {
                    report_and_off(false);
                }
            }
            core::hint::spin_loop();
        }
    }
    // OHNE `selftest` bleibt genau das hier uebrig, und das ist die ehrliche Aussage von todo F2:
    // der Kernel hat derzeit **keinen Nicht-Test-Zweck**. Es gibt kein Boot-Archiv auf x86 und
    // keinen Root-Task, dem die Wurzel-Caps uebergeben wuerden. Das Gating liefert also keinen
    // schlankeren Kernel, sondern einen leeren -- deshalb steht `selftest` weiterhin in `default`.
    // Diese Konfiguration wird trotzdem GEBAUT (test-qemu-x86.sh), damit sie nicht verrottet.
    #[cfg(not(feature = "selftest"))]
    {
        println!("bringup : Kernel-Kern steht; ohne Feature `selftest` gibt es keine Aufgabe (todo F2)");
        loop {
            system::reap();
            core::hint::spin_loop();
        }
    }
}
