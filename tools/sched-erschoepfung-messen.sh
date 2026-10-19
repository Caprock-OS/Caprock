#!/usr/bin/env bash
# **Loest die GELESENEN Scheduler-Befunde B1/B2 aus -- am echten Quelltext.** (B-7.3, Strang Scheduler)
#
# ================================================================================================
# WARUM ES DIESE DATEI GIBT
# ================================================================================================
# `tools/verus-modelltreue-sched.sh` hat am 2026-08-03 fuenf Befunde gemeldet. Alle fuenf sind aus
# dem Quelltext HERGELEITET -- kein einziger ist je AUSGELOEST worden. Genau diese Differenz hat
# dieses Projekt mehrfach bezahlt: eine Aussage sieht wahr aus, weil der Fall, der sie widerlegen
# koennte, nie laeuft (leere Event-Queue ohne `CD.R`; `virtio-rng` als "Beleg" fuer den DMA-Pfad).
# Ein Befund, den niemand ausloest, ist eine Lesart, kein Messwert.
#
# Dieses Werkzeug fuehrt den ECHTEN `crates/caprock-sched/src/lib.rs` aus -- unveraendert, bis auf
# die eine `#![no_std]`-Zeile, die einem Host-Binary im Weg steht. Es gibt KEINE handgeschriebene
# Zweitfassung des Schedulers; das waere die B-7.2-Falle ("ein Beweis ueber eine Kopie beweist
# etwas ueber die Kopie"). Gegen die HAL steht ein Stellvertreter mit genau EINER Funktion
# (`init_thread_frame`); `caprock-slab` und `caprock-sync` werden im ECHTEN Quelltext gelinkt.
#
# ================================================================================================
# WAS GEMESSEN WIRD
# ================================================================================================
#   P0  **Sprechprobe des Messgeraets.** Die Messgroesse "steht in einer Ready-Liste" wird durch
#       LAUFEN der intrusiven Liste bestimmt (nicht durch Lesen des `queued`-Flags), und es wird
#       gezeigt, dass sie BEIDE Antworten geben kann. Ein Messgeraet, das nur "ja" sagen kann,
#       misst nichts.
#   P1  **Positivkontrolle.** Dieselbe Folge (`pause` -> `unblock`) an einem Thread mit RESTBUDGET.
#       Er muss danach in der Liste stehen, `depleted_count` muss 0 bleiben, `audit()` 0.
#       Ohne diesen Schritt ist jede Zahl aus M1..M6 wertlos.
#   M1  Die Kette aus dem Befund: Konto erschoepfen -> PAUSE -> RESUME. Steht der erschoepfte
#       Thread in der Ready-Liste? Wird er `current`? Was sagt `audit()`?
#   M2  Dieselbe Wirkung ueber einen Pfad, der KEINE PdControl-Cap braucht: Budget-Donation
#       (`switch_to`) + `unblock` des Aufrufers -- die Form, die `unblock_with_error`
#       (kernel/src/system.rs:6588) auf dem Reply-/Leichen-Pfad hat.
#   M3  Die Zahlenreihen: `depleted_count`, `next_refill`, `depletions` ueber N Runden --
#       einmal MIT pause/unblock je Runde (M3a), einmal OHNE (M3b, Kontrolle).
#   M4  `refill_depleted` reiht einen PAUSIERTEN Thread wieder ein -- eine ZWEITE, von `unblock`
#       unabhaengige Ursache. Gemessen ueber `audit()` und ueber die Wirkung (er wird `current`).
#   M5  Nach dem Refill: `depleted_count` gegen die WIRKLICHE Zahl erschoepfter Konten.
#   M6  **Die Verwechslungskontrolle.** Dieselbe Zahlenreihe entsteht auch ohne jedes `unblock`,
#       wenn kein anderer Thread lauffaehig ist (`dequeue_highest() == None` laesst `current`
#       stehen). Deshalb belegt M1 zusaetzlich, dass eine Alternative bereitstand.
#
# ------------------------------------------------------------------------------------------------
# D-REIHE (2026-08-03, nach D8): der DONEE-ZWEIG in `refill_depleted`
# ------------------------------------------------------------------------------------------------
# Der `_`-Zweig hat seit D8 zwei Waechter; der Donee-Zweig hat KEINEN -- er setzt `blocked = false`
# bedingungslos und reiht bedingungslos ein. Gemessen wird, ob das erreichbar schadet.
#   P2  **Positivkontrolle des Donee-Zweigs.** Der Donee war WIRKLICH auf das Konto-Budget
#       geblockt und wird beim Refill zu Recht bereit -- und laeuft danach auf einem Konto MIT
#       Budget. Ohne diesen Schritt ist keine Zahl aus D1..D7 etwas wert. Steht in `pk`.
#   D1  (F1) Der Donee ist aus einem ANDEREN Grund blockiert: die PAUSE wird gesetzt, waehrend
#       das Konto noch Budget hat (`konto_depleted = 0`) -- damit ist die Blockade nachweislich
#       die PAUSE. Der Refill hebt sie mit auf, weil beides dasselbe Bit ist.
#       D1b: dieselbe PAUSE nach der Erschoepfung ist ein No-Op und meldet trotzdem Erfolg.
#   D2  (F2) Der Donee ist SELBST erschoepft, wenn der Zweig ihn einreiht -> Audit-Code 9.
#   D3  (F3) Kann `sc_donee` veralten? (a) Tod des Donee, (b) Migration -- mit Sprechprobe.
#   D4  Das KONTO stirbt, waehrend der Donee auf sein Budget geblockt ist.
#   D5  VERSCHACHTELTE Spende (fs -> Blockdienst -> Treiber): der zweite CALL ueberschreibt
#       `sc_donee`, der innere REPLY loescht es -- der aeussere Server behaelt seinen `sc_donor`.
#   D6  `unblock` prueft das EIGENE Konto des Threads, nicht das belastete.
#   D7  `set_budget` auf das Konto raeumt `depleted` weg -- und damit den einzigen Wecker.
#   Jede dieser Messungen traegt eine Verhungerungsprobe (`ticks_donee_lief`,
#   `D1.nach_resume.ticks_mit_budget`): ein Donee, der NIE WIEDER laeuft, ist schlimmer als einer,
#   der zu frueh laeuft.
#
# **NICHT gemessen:** Nebenlaeufigkeit (der Waechter ist sequentiell, die Kern-Locks bleiben
# ausserhalb), der echte Kontextwechsel (HAL-Stellvertreter), und die Erreichbarkeit aus dem
# Syscall-Pfad des Kernels (dazu braeuchte es einen hwfuzz-Fall; hier wird nur die Aufruffolge
# nachgestellt, die `caprock-ipc` / `kernel/src/system.rs` an dieser Stelle absetzen).
#
# ================================================================================================
# GEGENPROBE
# ================================================================================================
# Dieselbe Folge laeuft an mehreren Fassungen. Die Mutationen liegen ausschliesslich auf KOPIEN;
# `crates/` wird nie beschrieben.
#   echt  der unveraenderte Quelltext
#   V0    der Stand VOR D8 (beide Waechter und Audit-Code 9 entfernt) -- die Sprechprobe der
#         Gegenprobenmechanik: die alten Befunde muessen hier wieder auftauchen
#   H-a   der Waechter des `_`-Zweigs WOERTLICH in den Donee-Zweig uebertragen -- faellt in der
#         Positivkontrolle durch (der Donee ist dort IMMER blockiert, also weckt ihn niemand)
#   H-b   der Vorschlag: der GRUND der Blockade wird mitgeschrieben (`budget_blocked`), und der
#         Refill weckt, wer WEGEN DIESES KONTOS blockiert ist -- statt des einen Slots `sc_donee`
#
# Aufruf:  tools/sched-erschoepfung-messen.sh [--nur-echt]
# Rueckgabe: 0 = Messung gelaufen (Positivkontrolle bestanden) · 2 = Werkzeugfehler.
if [ -z "${BASH_VERSION:-}" ]; then echo "FEHLER: braucht bash, nicht sh/dash." >&2; exit 2; fi
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

CODE_STD="$ROOT/crates/caprock-sched/src/lib.rs"
CYCLES_STD="$ROOT/crates/caprock-sched/src/cycles.rs"
SLAB_STD="$ROOT/crates/caprock-slab/src/lib.rs"
SYNC_STD="$ROOT/crates/caprock-sync/src/lib.rs"
RUSTC="${RUSTC:-rustc}"

WURZEL="$(mktemp -d)"
trap 'rm -rf "$WURZEL"' EXIT
trap 'rm -rf "$WURZEL"; exit 130' INT TERM

# ================================================================================================
# 1. Der Anbau: HAL-Stellvertreter + Messung. Bildet den Scheduler NICHT nach -- er misst ihn.
# ================================================================================================
anbau_schreiben() {
    cat > "$1" <<'RSEOF'

// ================================================================================================
// Ab hier: der Anbau des Messwerkzeugs. Nicht Teil von `caprock-sched`.
// ================================================================================================

/// Stellvertreter fuer `caprock-hal`. Der Scheduler braucht davon genau eine Funktion. Was ein
/// echter Frame ist, sagt dieser Lauf NICHT -- hier zaehlt nur, dass jeder Thread einen
/// unterscheidbaren `sp` bekommt.
mod caprock_hal {
    pub mod exception {
        pub fn init_thread_frame(
            stack_top: usize,
            _entry: usize,
            _arg: usize,
            _user: bool,
            _user_sp: usize,
        ) -> usize {
            stack_top
        }
    }
}

mod messung {
    use crate::{
        attach_directory, core_storage_align, core_storage_bytes, directory_align,
        directory_bytes, Scheduler, ThreadId, NIL, NOT_QUEUED, NPRIO,
    };

    const CAP: usize = 32;
    /// Dummy-Frame. Der Scheduler legt ihn nur ab; sein Wert spielt keine Rolle.
    const F: usize = 0xF000;
    const PRIO_IDLE: u8 = 0;
    const PRIO_T: u8 = 1;
    const BUDGET: u32 = 3;
    const PERIOD: u32 = 100;

    // --- D10: die beiden Tabellengroessen der Leistungsmessung -------------------------------
    //
    // `Slab::len()` ist die TABELLENGROESSE, nicht die Belegung. Im Kernel ist sie
    // `je_kern * MIGRATION_HEADROOM` mit `je_kern = max(ceil(TARGET_THREADS/cores), 256)`
    // (kernel/src/system.rs:801/822) -- also **1000 bei 20 Kernen bis 20000 auf einer
    // Einkernmaschine**. `L_GROSS` nimmt die Zahl aus dem Todo-Eintrag (10000); sie liegt
    // mitten in diesem Band. `L_KLEIN` ist die Groesse, mit der die uebrige Messung faehrt.
    const L_KLEIN: usize = 32;
    const L_GROSS: usize = 10_000;

    // --- D10: die Instrumentierung ------------------------------------------------------------
    //
    // **Warum Iterationen und nicht Zeit.** Zeit auf einer Mehrkernmaschine unter Last misst
    // den Wirt mit; dieselbe Schleife kostet je nach Nachbarprozess das Doppelte. Eine
    // Iterationszahl ist eine Eigenschaft des Programms und ueber Laeufe hinweg identisch --
    // und genau darum kann sie ueberhaupt in eine Gegenprobentabelle.
    //
    // Die `fetch_add`-Zeilen stehen NICHT im Quelltext von `caprock-sched`. Sie werden vom
    // Werkzeug in eine KOPIE eingesetzt (s. `INSTR_*` unten) -- deshalb tragen die
    // Verhaltensmessungen (P/M/D) und die Kostenmessungen (L) getrennte Binaries: die
    // Verhaltensaussage soll am unveraenderten Quelltext haengen.
    pub static SCAN_AUSSEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static SCAN_INNEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static SCAN_SETBUDGET: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    pub static SCAN_ZOMBIE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn z_null() {
        use std::sync::atomic::Ordering as O;
        SCAN_AUSSEN.store(0, O::Relaxed);
        SCAN_INNEN.store(0, O::Relaxed);
        SCAN_SETBUDGET.store(0, O::Relaxed);
        SCAN_ZOMBIE.store(0, O::Relaxed);
    }

    fn z(a: &std::sync::atomic::AtomicU64) -> i64 {
        a.load(std::sync::atomic::Ordering::Relaxed) as i64
    }

    // --- Aufbau -------------------------------------------------------------------------------

    fn roh(bytes: usize, align: usize) -> *mut u8 {
        let l = std::alloc::Layout::from_size_align(bytes, align).expect("Layout");
        // SAFETY: frisch alloziert, exklusiv, wird nie freigegeben (Slab-Vertrag: kein Drop).
        let p = unsafe { std::alloc::alloc(l) };
        assert!(!p.is_null(), "kein Speicher");
        p
    }

    /// Ein frischer Kern mit frischem Directory **beliebiger Tabellengroesse**. Jedes Szenario
    /// steht fuer sich. Die Groesse ist ein Parameter, weil die D10-Aussage genau von ihr
    /// handelt: eine Messung, die nur bei 32 Slots laeuft, kann ueber 10000 nichts sagen.
    fn aufbau_mit(cap: usize) -> (Scheduler, ThreadId) {
        // SAFETY: frischer, exklusiver Speicher; das Directory wird je Szenario neu angehaengt
        // (der alte Block bleibt liegen -- ein Messbinary lebt Millisekunden).
        unsafe { attach_directory(roh(directory_bytes(cap), directory_align()), cap) };
        let mut s = Scheduler::new();
        // SAFETY: wie oben.
        unsafe { s.attach_storage(0, roh(core_storage_bytes(cap), core_storage_align()), cap) };
        let idle = s.init_core(0, PRIO_IDLE).expect("init_core");
        (s, idle)
    }

    /// Ein frischer Kern mit frischem Directory. Jedes Szenario steht fuer sich.
    fn aufbau() -> (Scheduler, ThreadId) {
        aufbau_mit(CAP)
    }

    fn faden(s: &mut Scheduler, prio: u8, nr: usize) -> ThreadId {
        s.spawn(0, 0x1000 + nr, 0, 0x10_0000 + nr * 0x1000, 0x1000, prio)
            .expect("spawn")
    }

    // --- Messgroessen -------------------------------------------------------------------------
    //
    // Alles hier LIEST nur. Die Mitgliedschaft in einer Ready-Liste wird durch **Laufen** der
    // intrusiven Liste bestimmt, nicht durch Lesen des `queued`-Flags: das Flag ist eine
    // Behauptung des Schedulers ueber sich selbst, die Liste ist die Sache.

    fn lok(s: &Scheduler, t: ThreadId) -> usize {
        s.resolve(t).expect("Thread nicht aufloesbar")
    }

    /// In welcher Prioritaetsliste steht `local` **wirklich**? (Liste gelaufen.)
    fn in_liste(s: &Scheduler, local: usize) -> Option<usize> {
        for p in 0..NPRIO {
            let mut i = s.queues[p].head;
            let mut n = 0usize;
            while i != NIL {
                if i as usize == local {
                    return Some(p);
                }
                i = s.tcbs[i as usize].qnext;
                n += 1;
                // Schranke aus der TABELLE, nicht aus `CAP`: seit D10 laeuft dieselbe Messung
                // auch mit 10000 Slots, und eine feste 32 waere dort eine falsche Zyklusmeldung.
                assert!(n <= s.tcbs.len() + 1, "Listenzyklus in Prioritaet {p}");
            }
        }
        None
    }

    /// Was BEHAUPTET das Flag? Weicht es von der Liste ab, ist das selbst ein Befund.
    fn flag_liste(s: &Scheduler, local: usize) -> Option<usize> {
        let q = s.tcbs[local].queued;
        if q == NOT_QUEUED {
            None
        } else {
            Some(q as usize)
        }
    }

    fn ja(b: bool) -> i64 {
        if b {
            1
        } else {
            0
        }
    }

    /// Wie viele Konten sind WIRKLICH erschoepft (Tabelle gezaehlt)? Die zweite, unabhaengig
    /// hergeleitete Zahl neben `depleted_count` -- eine allein koennte zufaellig stimmen.
    fn echte_erschoepfte(s: &Scheduler) -> i64 {
        (0..s.tcbs.len())
            .filter(|&i| s.tcbs[i].used && s.tcbs[i].depleted)
            .count() as i64
    }

    /// Wie viele Threads sind WIRKLICH budget-blockiert (Tabelle gezaehlt)? Die zweite,
    /// unabhaengig hergeleitete Zahl neben einem etwaigen Zaehler im Scheduler. Genau diese
    /// Nachzaehlung fehlte bei `depleted_count` -- er log (D8/M5), und niemand konnte es sehen.
    fn echte_budget_blocked(s: &Scheduler) -> i64 {
        (0..s.tcbs.len())
            .filter(|&i| s.tcbs[i].used && s.tcbs[i].budget_blocked)
            .count() as i64
    }

    /// Was BEHAUPTET der Scheduler ueber dieselbe Zahl? `-1` heisst „es gibt den Zaehler
    /// (noch) nicht" -- das ist der Stand VOR der D10-Behebung und ist ausdruecklich ein
    /// gueltiges Messergebnis, kein Fehler.
    fn zaehler_budget_blocked(s: &Scheduler) -> i64 {
        s.budget_blocked_count as i64
    }

    /// Vollstaendiger Blick auf einen Thread, ohne Adjektive.
    fn zeile(s: &Scheduler, name: &str, local: usize) {
        let t = &s.tcbs[local];
        println!(
            "     {name:<10} blocked={} depleted={} remaining={:<3} budget={:<3} next_refill={:<5} \
in_liste={} flag={} current={}",
            ja(t.blocked),
            ja(t.depleted),
            t.remaining,
            t.budget,
            t.next_refill,
            match in_liste(s, local) {
                Some(p) => p as i64,
                None => -1,
            },
            match flag_liste(s, local) {
                Some(p) => p as i64,
                None => -1,
            },
            ja(s.current == Some(local)),
        );
    }

    // --- Protokoll ----------------------------------------------------------------------------

    pub struct Bericht {
        pub zeilen: Vec<(String, i64)>,
    }

    impl Bericht {
        fn k(&mut self, name: &str, wert: i64) {
            self.zeilen.push((name.to_string(), wert));
        }
    }

    // ==============================================================================================
    // P0 -- Sprechprobe: kann die Messgroesse ueberhaupt BEIDE Antworten geben?
    // ==============================================================================================
    fn p0(b: &mut Bericht) {
        println!("  P0  Sprechprobe des Messgeraets (ohne Budget, ohne Erschoepfung)");
        let (mut s, _idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);

        b.k("P0.nach_spawn.in_liste", ja(in_liste(&s, lt).is_some()));
        zeile(&s, "nach spawn", lt);
        s.pause(t);
        b.k("P0.nach_pause.in_liste", ja(in_liste(&s, lt).is_some()));
        zeile(&s, "nach pause", lt);
        s.unblock(t);
        b.k("P0.nach_unblock.in_liste", ja(in_liste(&s, lt).is_some()));
        zeile(&s, "nach unblock", lt);
        b.k("P0.audit", s.audit() as i64);
        b.k(
            "P0.liste_gleich_flag",
            ja(in_liste(&s, lt) == flag_liste(&s, lt)),
        );
    }

    // ==============================================================================================
    // P1 -- Positivkontrolle: dieselbe Folge an einem Thread mit RESTBUDGET.
    //
    // Der entscheidende Aufruf (`unblock` an einem blockierten, nicht eingereihten, nicht
    // laufenden Thread) steht hier in genau derselben Lage wie in M1. Der EINZIGE Unterschied
    // ist, ob das Konto leer war.
    // ==============================================================================================
    fn p1(b: &mut Bericht) {
        println!("  P1  Positivkontrolle -- pause/unblock mit RESTBUDGET");
        let (mut s, idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        let li = lok(&s, idle);
        assert!(s.set_budget(t, 5, PERIOD), "set_budget");

        s.on_tick(0, F, false); // YIELD: idle einreihen, hoechste Prioritaet waehlen -> t
        b.k("P1.t_ist_current", ja(s.current == Some(lt)));
        s.on_tick(0, F, true); // 5 -> 4
        s.on_tick(0, F, true); // 4 -> 3

        s.pause(t); // blocked = true, laeuft aber noch
        s.on_tick(0, F, true); // deplaniert ihn sauber (requeue = false), 3 -> 2
        b.k("P1.vor_unblock.blocked", ja(s.tcbs[lt].blocked));
        b.k("P1.vor_unblock.depleted", ja(s.tcbs[lt].depleted));
        b.k("P1.vor_unblock.remaining", s.tcbs[lt].remaining as i64);
        b.k("P1.vor_unblock.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("P1.vor_unblock.current", ja(s.current == Some(lt)));
        zeile(&s, "vor unblock", lt);

        s.unblock(t);
        b.k("P1.nach_unblock.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("P1.nach_unblock.depleted", ja(s.tcbs[lt].depleted));
        b.k("P1.nach_unblock.remaining", s.tcbs[lt].remaining as i64);
        b.k("P1.depleted_count", s.depleted_count as i64);
        b.k("P1.depletions", s.budget_stats().0 as i64);
        b.k("P1.audit", s.audit() as i64);
        zeile(&s, "nach unblock", lt);

        // Wirkung: wird er eingeplant, und laeuft er mit ECHTEM Budget?
        s.on_tick(0, F, false);
        b.k("P1.wird_current", ja(s.current == Some(lt)));
        b.k("P1.idle_war_bereit", ja(in_liste(&s, li).is_some()));
        s.on_tick(0, F, true);
        b.k("P1.remaining_sinkt", s.tcbs[lt].remaining as i64);
    }

    // ==============================================================================================
    // M1 -- die Kette aus dem Befund: erschoepfen -> PAUSE -> RESUME.
    // ==============================================================================================
    fn m1(b: &mut Bericht) {
        println!("  M1  erschoepfen -> pause -> unblock");
        let (mut s, idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        let li = lok(&s, idle);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");

        s.on_tick(0, F, false);
        assert_eq!(s.current, Some(lt), "t muss laufen");
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        // Nach der Erschoepfung: deplaniert, off-queue, NICHT blockiert.
        b.k("M1.nach_erschoepfung.depleted", ja(s.tcbs[lt].depleted));
        b.k("M1.nach_erschoepfung.remaining", s.tcbs[lt].remaining as i64);
        b.k(
            "M1.nach_erschoepfung.in_liste",
            ja(in_liste(&s, lt).is_some()),
        );
        b.k("M1.nach_erschoepfung.current", ja(s.current == Some(lt)));
        b.k("M1.nach_erschoepfung.depleted_count", s.depleted_count as i64);
        b.k("M1.nach_erschoepfung.audit", s.audit() as i64);
        let refill_vor = s.tcbs[lt].next_refill as i64;
        b.k("M1.next_refill_vor", refill_vor);
        zeile(&s, "erschoepft", lt);

        s.pause(t);
        b.k("M1.nach_pause.blocked", ja(s.tcbs[lt].blocked));
        b.k("M1.nach_pause.in_liste", ja(in_liste(&s, lt).is_some()));

        s.unblock(t);
        // ==> DIE MESSUNG
        b.k("M1.nach_unblock.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("M1.nach_unblock.depleted", ja(s.tcbs[lt].depleted));
        b.k("M1.nach_unblock.remaining", s.tcbs[lt].remaining as i64);
        b.k("M1.nach_unblock.audit", s.audit() as i64);
        b.k(
            "M1.liste_gleich_flag",
            ja(in_liste(&s, lt) == flag_liste(&s, lt)),
        );
        zeile(&s, "nach unblock", lt);

        // Stand eine Alternative bereit? Ohne diese Zahl koennte "wird current" auch heissen
        // "es gab nichts anderes" (s. M6). Gemessen wird sie **im Augenblick der Wahl**, also
        // nach dem Umplanungsschritt -- davor ist der Idle-Thread `current` und steht deshalb
        // in keiner Liste. (Genau diese Verwechslung hat die erste Fassung gemacht.)
        s.on_tick(0, F, false);
        b.k("M1.alternative_bei_wahl", ja(in_liste(&s, li).is_some()));
        b.k("M1.wird_current", ja(s.current == Some(lt)));
        b.k("M1.remaining_beim_laufen", s.tcbs[lt].remaining as i64);
        b.k("M1.depleted_beim_laufen", ja(s.tcbs[lt].depleted));

        // Ein voller Tick auf leerem Konto.
        let dep_vor = s.budget_stats().0 as i64;
        s.on_tick(0, F, true);
        b.k("M1.depletions_delta", s.budget_stats().0 as i64 - dep_vor);
        b.k("M1.depleted_count_danach", s.depleted_count as i64);
        b.k("M1.echte_erschoepfte_danach", echte_erschoepfte(&s));
        b.k("M1.next_refill_nach", s.tcbs[lt].next_refill as i64);
        b.k("M1.next_refill_verschoben", s.tcbs[lt].next_refill as i64 - refill_vor);
    }

    // ==============================================================================================
    // M2 -- dieselbe Wirkung ohne PdControl: Budget-Donation + `unblock` des Aufrufers.
    //
    // `switch_to` ist der IPC-CALL-Fastpath: der Aufrufer blockiert und leiht dem Server sein
    // Konto. Erschoepft sich das Konto, waehrend der Server laeuft, ist der AUFRUFER blockiert
    // UND erschoepft. Weckt ihn danach irgendwer (`unblock_with_error` auf dem Leichen-/
    // Abbruchpfad, kernel/src/system.rs:6588), greift dieselbe Stelle.
    // ==============================================================================================
    fn m2(b: &mut Bericht) {
        println!("  M2  Budget-Donation -> unblock des Aufrufers (kein PdControl noetig)");
        let (mut s, idle) = aufbau();
        let c = faden(&mut s, PRIO_T, 1); // Aufrufer
        let srv = faden(&mut s, PRIO_T, 2); // Server
        let lc = lok(&s, c);
        let ls = lok(&s, srv);
        let li = lok(&s, idle);
        assert!(s.set_budget(c, BUDGET, PERIOD), "set_budget");

        s.on_tick(0, F, false);
        assert_eq!(s.current, Some(lc), "der Aufrufer muss laufen");
        s.switch_to(0, F, srv); // CALL-Fastpath: c blockiert, srv laeuft gegen cs Konto
        b.k("M2.nach_call.caller_blocked", ja(s.tcbs[lc].blocked));
        b.k("M2.nach_call.server_current", ja(s.current == Some(ls)));

        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("M2.nach_erschoepfung.caller_depleted", ja(s.tcbs[lc].depleted));
        b.k("M2.nach_erschoepfung.caller_blocked", ja(s.tcbs[lc].blocked));
        b.k("M2.nach_erschoepfung.server_blocked", ja(s.tcbs[ls].blocked));
        b.k(
            "M2.nach_erschoepfung.caller_in_liste",
            ja(in_liste(&s, lc).is_some()),
        );
        zeile(&s, "caller", lc);
        zeile(&s, "server", ls);

        s.unblock(c); // das tut der Kernel, wenn der Ruf abbricht / der Partner stirbt
        b.k("M2.nach_unblock.in_liste", ja(in_liste(&s, lc).is_some()));
        b.k("M2.nach_unblock.depleted", ja(s.tcbs[lc].depleted));
        b.k("M2.nach_unblock.remaining", s.tcbs[lc].remaining as i64);
        b.k("M2.nach_unblock.audit", s.audit() as i64);
        zeile(&s, "caller", lc);

        s.on_tick(0, F, false);
        b.k("M2.alternative_bei_wahl", ja(in_liste(&s, li).is_some()));
        b.k("M2.wird_current", ja(s.current == Some(lc)));
    }

    // ==============================================================================================
    // M3 -- die Zahlenreihen. (a) mit pause/unblock je Runde, (b) ohne -- die Kontrolle.
    // ==============================================================================================
    fn m3(b: &mut Bericht) {
        const RUNDEN: usize = 6;

        println!("  M3a Reihe MIT pause/unblock je Runde");
        let (mut s, idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        let li = lok(&s, idle);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        let mut dc = Vec::new();
        let mut nr = Vec::new();
        let mut dp = Vec::new();
        let mut ec = Vec::new();
        let mut leer_ticks = 0i64;
        dc.push(s.depleted_count as i64);
        nr.push(s.tcbs[lt].next_refill as i64);
        dp.push(s.budget_stats().0 as i64);
        ec.push(echte_erschoepfte(&s));
        for r in 0..RUNDEN {
            s.pause(t);
            s.unblock(t);
            s.on_tick(0, F, false); // t wird gewaehlt
            let lief_leer = s.current == Some(lt)
                && s.tcbs[lt].remaining == 0
                && s.tcbs[lt].depleted
                && in_liste(&s, li).is_some(); // ... und eine Alternative stand bereit
            s.on_tick(0, F, true); // ein voller Tick auf leerem Konto
            if lief_leer {
                leer_ticks += 1;
            }
            dc.push(s.depleted_count as i64);
            nr.push(s.tcbs[lt].next_refill as i64);
            dp.push(s.budget_stats().0 as i64);
            ec.push(echte_erschoepfte(&s));
            let _ = r;
        }
        println!("     depleted_count : {dc:?}");
        println!("     next_refill    : {nr:?}");
        println!("     depletions     : {dp:?}");
        println!("     echt erschoepft: {ec:?}");
        println!("     Ticks auf leerem Konto (mit bereitstehender Alternative): {leer_ticks}");
        b.k("M3a.runden", RUNDEN as i64);
        b.k("M3a.leer_ticks", leer_ticks);
        b.k("M3a.depleted_count_ende", *dc.last().unwrap());
        b.k("M3a.echte_erschoepfte_ende", *ec.last().unwrap());
        b.k("M3a.next_refill_drift", nr.last().unwrap() - nr[0]);
        b.k("M3a.depletions_ende", *dp.last().unwrap());

        println!("  M3b Kontrolle: dieselben Ticks OHNE pause/unblock");
        let (mut s2, _i2) = aufbau();
        let t2 = faden(&mut s2, PRIO_T, 1);
        let l2 = lok(&s2, t2);
        assert!(s2.set_budget(t2, BUDGET, PERIOD), "set_budget");
        s2.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s2.on_tick(0, F, true);
        }
        let mut dc2 = vec![s2.depleted_count as i64];
        let mut nr2 = vec![s2.tcbs[l2].next_refill as i64];
        for _ in 0..(RUNDEN * 2) {
            s2.on_tick(0, F, true);
            dc2.push(s2.depleted_count as i64);
            nr2.push(s2.tcbs[l2].next_refill as i64);
        }
        println!("     depleted_count : {dc2:?}");
        println!("     next_refill    : {nr2:?}");
        b.k("M3b.depleted_count_ende", *dc2.last().unwrap());
        b.k("M3b.next_refill_drift", nr2.last().unwrap() - nr2[0]);
    }

    // ==============================================================================================
    // M4 -- `refill_depleted` reiht einen PAUSIERTEN Thread wieder ein. Zweite, von `unblock`
    //       unabhaengige Ursache: PAUSE allein reicht.
    // ==============================================================================================
    fn m4(b: &mut Bericht) {
        println!("  M4  erschoepfen -> pause -> Refill abwarten (KEIN unblock)");
        let (mut s, idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        let li = lok(&s, idle);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        s.pause(t);
        b.k("M4.vor_refill.blocked", ja(s.tcbs[lt].blocked));
        b.k("M4.vor_refill.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("M4.vor_refill.audit", s.audit() as i64);

        let (_, refills_vor) = s.budget_stats();
        let mut ticks = 0i64;
        while s.budget_stats().1 == refills_vor && ticks < (PERIOD as i64 + 10) {
            s.on_tick(0, F, true);
            ticks += 1;
        }
        b.k("M4.ticks_bis_refill", ticks);
        b.k("M4.refill_passiert", ja(s.budget_stats().1 > refills_vor));
        // Der Refill laeuft am ANFANG von `on_tick`; danach waehlt derselbe Aufruf den
        // naechsten Thread. Gemessen wird also der Zustand unmittelbar nach diesem Aufruf.
        b.k("M4.nach_refill.blocked", ja(s.tcbs[lt].blocked));
        b.k("M4.nach_refill.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("M4.nach_refill.audit", s.audit() as i64);
        b.k("M4.idle_bereit", ja(in_liste(&s, li).is_some()));
        // ==> DIE WIRKUNG: laeuft ein PAUSIERTER Thread wieder?
        b.k("M4.pausierter_ist_current", ja(s.current == Some(lt)));
        b.k("M4.beim_laufen_blocked", ja(s.tcbs[lt].blocked));
        zeile(&s, "nach refill", lt);
        // Und wie lange? Der naechste Umplanungsschritt deplaniert ihn (requeue = !blocked).
        s.on_tick(0, F, true);
        b.k("M4.eine_runde_spaeter_current", ja(s.current == Some(lt)));

        // --- M4b: derselbe Fehler, aber mit einem hoeher priorisierten Laeufer daneben.
        // Dann bleibt der pausierte Thread in der Liste STEHEN, statt sofort gewaehlt zu
        // werden -- und `audit()` sieht ihn.
        println!("  M4b dasselbe, aber ein hoeher priorisierter Thread laeuft daneben");
        let (mut s, _idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        s.pause(t);
        let hog = faden(&mut s, PRIO_T + 1, 2); // hoehere Prioritaet, unbeschraenkt
        let _ = hog;
        let (_, refills_vor) = s.budget_stats();
        let mut ticks = 0i64;
        while s.budget_stats().1 == refills_vor && ticks < (PERIOD as i64 + 10) {
            s.on_tick(0, F, true);
            ticks += 1;
        }
        b.k("M4b.refill_passiert", ja(s.budget_stats().1 > refills_vor));
        b.k("M4b.nach_refill.blocked", ja(s.tcbs[lt].blocked));
        b.k("M4b.nach_refill.in_liste", ja(in_liste(&s, lt).is_some()));
        b.k("M4b.nach_refill.audit", s.audit() as i64);
        zeile(&s, "nach refill", lt);
    }

    // ==============================================================================================
    // M5 -- nach dem Refill: `depleted_count` gegen die wirkliche Zahl erschoepfter Konten.
    // ==============================================================================================
    fn m5(b: &mut Bericht) {
        println!("  M5  Drift des Zaehlers ueber einen vollstaendigen Refill hinweg");
        let (mut s, _idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        // Drei pause/unblock-Runden, jede kostet einen Tick auf leerem Konto.
        for _ in 0..3 {
            s.pause(t);
            s.unblock(t);
            s.on_tick(0, F, false);
            s.on_tick(0, F, true);
        }
        b.k("M5.vor_refill.depleted_count", s.depleted_count as i64);
        b.k("M5.vor_refill.echte_erschoepfte", echte_erschoepfte(&s));
        let (_, refills_vor) = s.budget_stats();
        let mut ticks = 0i64;
        while s.budget_stats().1 == refills_vor && ticks < 4 * PERIOD as i64 {
            s.on_tick(0, F, true);
            ticks += 1;
        }
        b.k("M5.refill_passiert", ja(s.budget_stats().1 > refills_vor));
        b.k("M5.nach_refill.depleted_count", s.depleted_count as i64);
        b.k("M5.nach_refill.echte_erschoepfte", echte_erschoepfte(&s));
        b.k(
            "M5.zaehler_luegt_um",
            s.depleted_count as i64 - echte_erschoepfte(&s),
        );
        b.k("M5.scan_laeuft_weiter_je_tick", ja(s.depleted_count > 0));
        zeile(&s, "nach refill", lt);
    }

    // ==============================================================================================
    // M6 -- die VERWECHSLUNGSKONTROLLE. Dieselbe Reihe entsteht ohne jedes `unblock`, sobald
    //       nichts anderes lauffaehig ist: `dequeue_highest() == None` laesst `current` stehen.
    //       Deshalb traegt "wird current" die Aussage nur ZUSAMMEN mit "eine Alternative war da".
    // ==============================================================================================
    fn m6(b: &mut Bericht) {
        println!("  M6  Verwechslungskontrolle: kein anderer Thread lauffaehig, kein unblock");
        let (mut s, idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false); // t wird current, idle steht in der Liste
        s.pause(idle); // ... und wird herausgenommen: keine Alternative mehr
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        let mut dc = vec![s.depleted_count as i64];
        let mut nr = vec![s.tcbs[lt].next_refill as i64];
        for _ in 0..5 {
            s.on_tick(0, F, true);
            dc.push(s.depleted_count as i64);
            nr.push(s.tcbs[lt].next_refill as i64);
        }
        println!("     depleted_count : {dc:?}");
        println!("     next_refill    : {nr:?}");
        b.k("M6.depleted_count_ende", *dc.last().unwrap());
        b.k("M6.next_refill_drift", nr.last().unwrap() - nr[0]);
        b.k("M6.bleibt_current", ja(s.current == Some(lt)));
        b.k("M6.unblock_kam_vor", 0);
    }

    // ==============================================================================================
    // M7 -- die Frage an JEDE Behebung: verhungert er? Wer den Thread nicht einreiht, muss
    //       zeigen, dass ihn spaeter jemand einreiht. Eine halbe Behebung ist schlimmer als
    //       der Befund.
    // ==============================================================================================
    fn m7(b: &mut Bericht) {
        println!("  M7  Verhungert er? erschoepfen -> pause -> unblock -> 3 Perioden zusehen");
        let (mut s, _idle) = aufbau();
        let t = faden(&mut s, PRIO_T, 1);
        let lt = lok(&s, t);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        s.pause(t);
        s.unblock(t); // das RESUME kam an -- was macht diese Fassung daraus?
        b.k("M7.nach_unblock.blocked", ja(s.tcbs[lt].blocked));

        let (_, refills_vor) = s.budget_stats();
        let mut mit_budget = 0i64; // Ticks, die er mit ECHTEM Restbudget verbraucht hat
        let mut auf_leer = 0i64; // Ticks, die er auf leerem Konto verbraucht hat
        for _ in 0..(3 * PERIOD as i64) {
            let laeuft = s.current == Some(lt);
            let leer = s.tcbs[lt].remaining == 0 && s.tcbs[lt].depleted;
            s.on_tick(0, F, true);
            if laeuft && leer {
                auf_leer += 1;
            } else if laeuft {
                mit_budget += 1;
            }
        }
        b.k("M7.refills", (s.budget_stats().1 - refills_vor) as i64);
        b.k("M7.ticks_mit_budget", mit_budget);
        b.k("M7.ticks_auf_leerem_konto", auf_leer);
        b.k("M7.ende.blocked", ja(s.tcbs[lt].blocked));
        b.k(
            "M7.ende.einplanbar",
            ja(in_liste(&s, lt).is_some() || s.current == Some(lt)),
        );
        zeile(&s, "am Ende", lt);
    }

    // ==============================================================================================
    // D-REIHE -- der DONEE-ZWEIG in `refill_depleted` (2026-08-03, der von D8 offen gelassene Punkt)
    //
    //     match self.tcbs[slot].sc_donee {
    //         Some(d) if d != slot => { self.tcbs[d].blocked = false; self.enqueue_ready(d); }
    //         _ => { if !blocked && current != Some(slot) { enqueue_ready(slot) } }
    //     }
    //
    // Der `_`-Zweig hat seit D8 zwei Waechter, der Donee-Zweig KEINEN: er setzt `blocked = false`
    // bedingungslos und reiht bedingungslos ein. Gemessen wird, ob das erreichbar schadet.
    // ==============================================================================================

    /// Ticken, bis ein Refill passiert (oder die Schranke reisst). Rueckgabe: (Ticks, passiert).
    fn bis_refill(s: &mut Scheduler, grenze: i64) -> (i64, bool) {
        let (_, vor) = s.budget_stats();
        let mut t = 0i64;
        while s.budget_stats().1 == vor && t < grenze {
            s.on_tick(0, F, true);
            t += 1;
        }
        (t, s.budget_stats().1 > vor)
    }

    /// Aufrufer (= Konto) ruft Server (= Donee) und erschoepft das Konto, waehrend der Server
    /// laeuft. Danach ist der Donee auf das Konto-Budget geblockt -- der Zustand, den der
    /// Donee-Zweig aufloest. Rueckgabe: (Scheduler, idle, konto, donee) als lokale Slots.
    fn donee_aufbau(budget: u32) -> (Scheduler, usize, usize, usize) {
        donee_aufbau_mit(CAP, budget)
    }

    /// Wie [`donee_aufbau`], aber mit waehlbarer Tabellengroesse (D10).
    fn donee_aufbau_mit(cap: usize, budget: u32) -> (Scheduler, usize, usize, usize) {
        let (mut s, idle) = aufbau_mit(cap);
        let a = faden(&mut s, PRIO_T, 1);
        let srv = faden(&mut s, PRIO_T, 2);
        let (la, ls, li) = (lok(&s, a), lok(&s, srv), lok(&s, idle));
        assert!(s.set_budget(a, budget, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        assert_eq!(s.current, Some(la), "der Aufrufer muss laufen");
        s.switch_to(0, F, srv); // IPC-CALL-Fastpath: a blockiert, srv laeuft gegen as Konto
        assert_eq!(s.current, Some(ls), "der Server muss laufen");
        (s, li, la, ls)
    }

    /// Der Handle zu einem lokalen Slot (fuer die Aufrufe, die eine `ThreadId` wollen).
    fn tid(s: &Scheduler, local: usize) -> ThreadId {
        s.id(local)
    }

    // ==============================================================================================
    // P2 -- POSITIVKONTROLLE DES DONEE-ZWEIGS. Ohne sie ist jede Zahl aus D1..D7 wertlos.
    //       Gesunder Fall: der Donee war WIRKLICH auf das Konto-Budget geblockt und wird beim
    //       Refill zu Recht wieder bereit -- und laeuft danach auf einem Konto MIT Budget.
    // ==============================================================================================
    fn p2(b: &mut Bericht) {
        println!("  P2  Positivkontrolle DONEE-ZWEIG -- der gesunde Fall");
        let (mut s, li, la, ls) = donee_aufbau(BUDGET);
        b.k("P2.donee_link", ja(s.tcbs[la].sc_donee == Some(ls)));
        b.k("P2.donee_hat_kein_eigenes_budget", ja(s.tcbs[ls].budget == 0));
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        // Der Zustand, den der Donee-Zweig aufloesen soll.
        b.k("P2.erschoepft.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("P2.erschoepft.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("P2.erschoepft.donee_in_liste", ja(in_liste(&s, ls).is_some()));
        b.k("P2.erschoepft.audit", s.audit() as i64);
        zeile(&s, "donee", ls);
        let (t, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("P2.ticks_bis_refill", t);
        b.k("P2.refill_passiert", ja(passiert));
        b.k("P2.nach_refill.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("P2.nach_refill.donee_current", ja(s.current == Some(ls)));
        b.k("P2.nach_refill.alternative_bereit", ja(in_liste(&s, li).is_some()));
        b.k("P2.nach_refill.konto_remaining", s.tcbs[la].remaining as i64);
        b.k("P2.nach_refill.audit", s.audit() as i64);
        // ==> Die WIRKUNG: laeuft er, und wird dabei ein Konto MIT Budget belastet?
        let r = s.tcbs[la].remaining as i64;
        s.on_tick(0, F, true);
        b.k(
            "P2.tick_auf_gefuelltem_konto",
            ja(r > 0 && s.tcbs[la].remaining as i64 == r - 1),
        );
        zeile(&s, "donee", ls);
    }

    // ==============================================================================================
    // D1 -- (F1) Ist der Donee zum Zeitpunkt des Refills aus einem ANDEREN Grund blockiert?
    //       Die PAUSE wird gesetzt, WAEHREND das Konto noch Budget hat -- damit ist bewiesen,
    //       dass die Blockade die PAUSE ist und nicht das Budget. Danach erschoepft das Konto,
    //       und der Refill hebt beides auf, weil beides dasselbe Bit ist.
    // ==============================================================================================
    fn d1(b: &mut Bericht) {
        println!("  D1  (F1) PAUSE am Donee -- gesetzt VOR der Erschoepfung");
        let (mut s, li, la, ls) = donee_aufbau(2);
        s.on_tick(0, F, true); // 2 -> 1: der Donee laeuft weiter, Konto noch nicht leer
        b.k("D1.vor_pause.donee_current", ja(s.current == Some(ls)));
        b.k("D1.vor_pause.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D1.vor_pause.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D1.vor_pause.konto_remaining", s.tcbs[la].remaining as i64);
        let ok = s.pause(tid(&s, ls));
        // ==> Die Blockade stammt NACHWEISLICH von der PAUSE: das Konto ist noch nicht leer.
        b.k("D1.pause_meldet_erfolg", ja(ok));
        b.k("D1.nach_pause.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D1.nach_pause.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D1.nach_pause.depleted_count", s.depleted_count as i64);
        s.on_tick(0, F, true); // 1 -> 0: jetzt erschoepft das Konto
        b.k("D1.nach_erschoepfung.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D1.nach_erschoepfung.donee_current", ja(s.current == Some(ls)));
        let (t, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("D1.ticks_bis_refill", t);
        b.k("D1.refill_passiert", ja(passiert));
        // ==> DIE MESSUNG: haelt die PAUSE ueber den Refill hinweg?
        b.k("D1.nach_refill.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D1.pausierter_ist_current", ja(s.current == Some(ls)));
        b.k("D1.alternative_bereit", ja(in_liste(&s, li).is_some()));
        b.k("D1.audit", s.audit() as i64);
        let r = s.tcbs[la].remaining as i64;
        s.on_tick(0, F, true);
        b.k(
            "D1.pausierter_verbraucht_budget",
            ja(r > 0 && s.tcbs[la].remaining as i64 == r - 1),
        );
        zeile(&s, "donee", ls);
        // Punkt 6 (die Frage an JEDE Behebung): wer die PAUSE haelt, muss zeigen, dass der
        // Donee nach dem RESUME wieder laeuft. Ein Donee, der nie wieder laeuft, ist schlimmer
        // als einer, der zu frueh laeuft.
        s.unblock(tid(&s, ls)); // RESUME
        let mut lief = 0i64;
        let mut mit_budget = 0i64;
        for _ in 0..(2 * PERIOD as i64) {
            let laeuft = s.current == Some(ls);
            let voll = s.tcbs[la].remaining > 0 && !s.tcbs[la].depleted;
            s.on_tick(0, F, true);
            if laeuft {
                lief += 1;
                if voll {
                    mit_budget += 1;
                }
            }
        }
        b.k("D1.nach_resume.ticks_gelaufen", lief);
        b.k("D1.nach_resume.ticks_mit_budget", mit_budget);
        b.k("D1.nach_resume.blocked", ja(s.tcbs[ls].blocked));

        // --- D1b: dieselbe PAUSE NACH der Erschoepfung. Sie ist ein No-Op (`blocked` steht
        // schon) und meldet trotzdem Erfolg -- die beiden Gruende teilen sich EIN Bit.
        println!("  D1b PAUSE am Donee -- gesetzt NACH der Erschoepfung (No-Op, meldet Erfolg)");
        let (mut s, _li, _la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        let vor = ja(s.tcbs[ls].blocked);
        let ok = s.pause(tid(&s, ls));
        b.k("D1b.vor_pause.donee_blocked", vor);
        b.k("D1b.pause_meldet_erfolg", ja(ok));
        let (_, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("D1b.refill_passiert", ja(passiert));
        b.k("D1b.pausierter_ist_current", ja(s.current == Some(ls)));
    }

    // ==============================================================================================
    // D2 -- (F2) Kann der Donee SELBST erschoepft sein, waehrend der Donee-Zweig ihn einreiht?
    //       Der Server hat ein EIGENES Konto und hat es leergefahren, bevor der Aufruf kommt.
    //       `switch_to` prueft `depleted` nicht -- der Donee-Zweig danach auch nicht.
    //       Ein hoeher priorisierter Laeufer haelt ihn in der Liste, sonst kann `audit()` ihn
    //       nicht sehen (er waere sofort `current`).
    // ==============================================================================================
    fn d2(b: &mut Bericht) {
        println!("  D2  (F2) der Donee ist SELBST erschoepft, wenn der Refill ihn einreiht");
        let (mut s, _idle) = aufbau();
        let srv = faden(&mut s, PRIO_T, 1); // zuerst gespawnt -> laeuft zuerst
        let a = faden(&mut s, PRIO_T, 2);
        let (la, ls) = (lok(&s, a), lok(&s, srv));
        assert!(s.set_budget(srv, 2, 4 * PERIOD), "set_budget srv");
        // `a` hat hier noch KEIN Budget -- sonst faehrt der Rundlauf beide Konten zugleich leer
        // und die Messung haette zwei Ursachen.
        let mut n = 0;
        while !s.tcbs[ls].depleted && n < 20 {
            s.on_tick(0, F, true);
            n += 1;
        }
        assert!(s.tcbs[ls].depleted, "srv muss sein eigenes Konto leerfahren");
        // Ohne Budgetverbrauch (YIELD) zum Aufrufer umschalten, dann erst sein Konto setzen.
        while s.current != Some(la) && n < 40 {
            s.on_tick(0, F, false);
            n += 1;
        }
        assert!(s.set_budget(a, BUDGET, PERIOD), "set_budget a");
        b.k("D2.srv_eigen_depleted", ja(s.tcbs[ls].depleted));
        b.k("D2.srv_blocked", ja(s.tcbs[ls].blocked));
        b.k("D2.a_ist_current", ja(s.current == Some(la)));
        s.switch_to(0, F, tid(&s, ls)); // CALL an den erschoepften Server
        b.k("D2.nach_call.srv_current", ja(s.current == Some(ls)));
        b.k("D2.nach_call.srv_depleted", ja(s.tcbs[ls].depleted));
        b.k("D2.nach_call.audit", s.audit() as i64);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D2.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D2.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D2.echte_erschoepfte", echte_erschoepfte(&s));
        let _hog = faden(&mut s, PRIO_T + 1, 3); // haelt ihn nach dem Refill in der Liste
        let (_, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("D2.refill_passiert", ja(passiert));
        b.k("D2.nach_refill.donee_depleted", ja(s.tcbs[ls].depleted));
        b.k("D2.nach_refill.donee_in_liste", ja(in_liste(&s, ls).is_some()));
        // ==> DIE MESSUNG: Audit-Code 9 ist genau dafuer da (D8, 2026-08-03).
        b.k("D2.nach_refill.audit", s.audit() as i64);
        zeile(&s, "donee", ls);
    }

    // ==============================================================================================
    // D3 -- (F3) Kann ein VERALTETER `sc_donee`-Eintrag hier hereinlaufen?
    //       (a) der Donee stirbt, (b) der Donee migriert. Beides mit Sprechprobe.
    // ==============================================================================================
    fn d3(b: &mut Bericht) {
        println!("  D3  (F3) veralteter sc_donee: gestorbener / migrierter Donee");
        let (mut s, _li, la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D3a.vor_kill.sc_donee_zeigt_auf_donee", ja(s.tcbs[la].sc_donee == Some(ls)));
        let gid_vor = s.tcbs[ls].gid as i64;
        assert!(s.kill(tid(&s, ls), 0), "kill Donee");
        b.k("D3a.nach_kill.sc_donee_geloescht", ja(s.tcbs[la].sc_donee.is_none()));
        b.k("D3a.nach_kill.slot_frei", ja(!s.tcbs[ls].used));
        let (_, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("D3a.refill_passiert", ja(passiert));
        b.k("D3a.toter_slot_in_liste", ja(in_liste(&s, ls).is_some()));
        b.k("D3a.nach_refill.audit", s.audit() as i64);
        b.k("D3a.gid_vor", gid_vor);

        // (b) Migration -- kommt ein Donee ueberhaupt aus dem Kern heraus?
        let (mut s, _li, la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D3b.donee_nicht_current", ja(s.current != Some(ls)));
        b.k("D3b.konto_nicht_current", ja(s.current != Some(la)));
        b.k("D3b.donee_detach_verweigert", ja(s.detach_for_migration(tid(&s, ls)).is_none()));
        b.k("D3b.konto_detach_verweigert", ja(s.detach_for_migration(tid(&s, la)).is_none()));
        // Sprechprobe: das Messgeraet kann auch JA sagen -- ein Thread OHNE Spende geht heraus.
        let frei = faden(&mut s, PRIO_T, 3);
        b.k("D3b.sprechprobe_fremder_detach_geht", ja(s.detach_for_migration(frei).is_some()));
    }

    // ==============================================================================================
    // D4 -- Das KONTO stirbt, waehrend der Donee auf sein Budget geblockt ist. `record_zombie`
    //       loest die Spende, laesst den Donee aber BLOCKIERT zurueck -- und mit dem Konto
    //       verschwindet der einzige Wecker.
    // ==============================================================================================
    fn d4(b: &mut Bericht) {
        println!("  D4  das KONTO stirbt, waehrend der Donee auf sein Budget geblockt ist");
        let (mut s, _li, la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D4.vor_kill.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D4.vor_kill.depleted_count", s.depleted_count as i64);
        assert!(s.kill(tid(&s, la), 0), "kill Konto");
        b.k("D4.nach_kill.donee_sc_donor_geloescht", ja(s.tcbs[ls].sc_donor.is_none()));
        b.k("D4.nach_kill.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D4.nach_kill.depleted_count", s.depleted_count as i64);
        b.k("D4.nach_kill.audit", s.audit() as i64);
        let mut lief = 0i64;
        for _ in 0..(3 * PERIOD as i64) {
            if s.current == Some(ls) {
                lief += 1;
            }
            s.on_tick(0, F, true);
        }
        // ==> DIE MESSUNG: laeuft er je wieder? (Ein Donee, der nie wieder laeuft, ist
        // schlimmer als einer, der zu frueh laeuft.)
        b.k("D4.ticks_donee_lief", lief);
        b.k("D4.ende.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D4.ende.donee_in_liste", ja(in_liste(&s, ls).is_some()));
        b.k("D4.ende.audit", s.audit() as i64);
        zeile(&s, "donee", ls);
    }

    // ==============================================================================================
    // D5 -- VERSCHACHTELTE Spende (fs -> Blockdienst -> Treiber ist genau diese Form).
    //       `sc_donee` ist EIN Slot, die Spende aber ein STAPEL: der zweite CALL ueberschreibt
    //       den Eintrag, und der REPLY des inneren Servers loescht ihn ganz. Der aeussere
    //       Server behaelt seinen `sc_donor` -- und wird spaeter gegen dasselbe Konto belastet.
    //       Kein Privileg noetig: zwei CALLs und ein REPLY.
    // ==============================================================================================
    fn d5(b: &mut Bericht) {
        println!("  D5  verschachtelte Spende: der innere REPLY loescht sc_donee");
        let (mut s, _idle) = aufbau();
        let a = faden(&mut s, PRIO_T, 1); // Wurzelkonto (der Mandant)
        let mid = faden(&mut s, PRIO_T, 2); // mittlerer Server
        let inn = faden(&mut s, PRIO_T, 3); // innerer Server
        let (la, lm, lin) = (lok(&s, a), lok(&s, mid), lok(&s, inn));
        assert!(s.set_budget(a, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        assert_eq!(s.current, Some(la), "a muss laufen");
        s.switch_to(0, F, mid); // CALL a -> mid
        b.k("D5.nach_call1.sc_donee_ist_mid", ja(s.tcbs[la].sc_donee == Some(lm)));
        s.switch_to(0, F, inn); // CALL mid -> inn, dasselbe Wurzelkonto
        b.k("D5.nach_call2.sc_donee_ist_inn", ja(s.tcbs[la].sc_donee == Some(lin)));
        b.k("D5.nach_call2.mid_sc_donor_bleibt", ja(s.tcbs[lm].sc_donor == Some(la)));
        b.k("D5.nach_call2.mid_blocked", ja(s.tcbs[lm].blocked));
        // REPLY des inneren Servers -- genau die Folge aus `caprock_ipc::Endpoint::reply`:
        // erst `end_donation`, dann `unblock(caller)`.
        s.end_donation(0);
        s.unblock(tid(&s, lm));
        b.k("D5.nach_reply.sc_donee_ist_none", ja(s.tcbs[la].sc_donee.is_none()));
        b.k("D5.nach_reply.mid_sc_donor_bleibt", ja(s.tcbs[lm].sc_donor == Some(la)));
        b.k("D5.nach_reply.mid_in_liste", ja(in_liste(&s, lm).is_some()));
        s.block_current(0, F); // der innere Server geht zurueck ins RECV
        b.k("D5.mid_ist_current", ja(s.current == Some(lm)));
        // `mid` laeuft weiter gegen das Konto von `a` -- bis es erschoepft.
        let mut n = 0;
        while !s.tcbs[la].depleted && n < 10 {
            s.on_tick(0, F, true);
            n += 1;
        }
        b.k("D5.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D5.mid_blocked", ja(s.tcbs[lm].blocked));
        b.k(
            "D5.sc_donee_beim_erschoepfen",
            match s.tcbs[la].sc_donee {
                Some(d) => d as i64,
                None => -1,
            },
        );
        b.k("D5.audit", s.audit() as i64);
        let mut lief = 0i64;
        for _ in 0..(3 * PERIOD as i64) {
            if s.current == Some(lm) {
                lief += 1;
            }
            s.on_tick(0, F, true);
        }
        // ==> DIE MESSUNG: der Refill kommt, aber niemand weckt `mid`.
        b.k("D5.refills", s.budget_stats().1 as i64);
        b.k("D5.ticks_mid_lief", lief);
        b.k("D5.ende.mid_blocked", ja(s.tcbs[lm].blocked));
        b.k("D5.ende.mid_in_liste", ja(in_liste(&s, lm).is_some()));
        b.k("D5.ende.a_blocked", ja(s.tcbs[la].blocked));
        b.k("D5.ende.a_depleted", ja(s.tcbs[la].depleted));
        b.k("D5.ende.audit", s.audit() as i64);
        zeile(&s, "mid", lm);
        zeile(&s, "a (Konto)", la);
    }

    // ==============================================================================================
    // D6 -- `unblock` prueft das EIGENE Konto des Threads, nicht das belastete. Bei einem Donee
    //       sind das zwei verschiedene. Damit steht D8 ueber diesen Zweig noch offen.
    // ==============================================================================================
    fn d6(b: &mut Bericht) {
        println!("  D6  `unblock` am Donee: geprueft wird das eigene, belastet das fremde Konto");
        let (mut s, li, la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D6.vor_unblock.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D6.vor_unblock.donee_eigen_depleted", ja(s.tcbs[ls].depleted));
        b.k("D6.vor_unblock.konto_depleted", ja(s.tcbs[la].depleted));
        s.unblock(tid(&s, ls)); // RESUME am Server (PdControl) bzw. `thaw_thread`
        b.k("D6.nach_unblock.donee_in_liste", ja(in_liste(&s, ls).is_some()));
        b.k("D6.nach_unblock.audit", s.audit() as i64);
        let dep_vor = s.budget_stats().0 as i64;
        let dc_vor = s.depleted_count as i64;
        s.on_tick(0, F, false);
        b.k("D6.wird_current", ja(s.current == Some(ls)));
        b.k("D6.alternative_bei_wahl", ja(in_liste(&s, li).is_some()));
        b.k("D6.konto_remaining_beim_laufen", s.tcbs[la].remaining as i64);
        s.on_tick(0, F, true); // ein voller Tick auf leerem Konto
        b.k("D6.depletions_delta", s.budget_stats().0 as i64 - dep_vor);
        b.k("D6.depleted_count_delta", s.depleted_count as i64 - dc_vor);
        b.k("D6.echte_erschoepfte", echte_erschoepfte(&s));
        b.k("D6.zaehler_luegt_um", s.depleted_count as i64 - echte_erschoepfte(&s));
    }

    // ==============================================================================================
    // D7 -- `set_budget` auf das Konto raeumt `depleted` weg -- und damit den einzigen Anlass,
    //       aus dem der Donee je wieder geweckt wuerde. (`kernel/src/threads/mod.rs:5578` prueft
    //       genau diese Strandung -- aber nur fuer das Konto selbst, nicht fuer seinen Donee.)
    // ==============================================================================================
    fn d7(b: &mut Bericht) {
        println!("  D7  `set_budget` auf das Konto strandet den Donee");
        let (mut s, _li, la, ls) = donee_aufbau(BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        b.k("D7.vor.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D7.vor.depleted_count", s.depleted_count as i64);
        assert!(s.set_budget(tid(&s, la), BUDGET, PERIOD), "set_budget erneut");
        b.k("D7.nach.konto_depleted", ja(s.tcbs[la].depleted));
        b.k("D7.nach.depleted_count", s.depleted_count as i64);
        b.k("D7.nach.donee_blocked", ja(s.tcbs[ls].blocked));
        let mut lief = 0i64;
        for _ in 0..(3 * PERIOD as i64) {
            if s.current == Some(ls) {
                lief += 1;
            }
            s.on_tick(0, F, true);
        }
        b.k("D7.ticks_donee_lief", lief);
        b.k("D7.ende.donee_blocked", ja(s.tcbs[ls].blocked));
        b.k("D7.ende.audit", s.audit() as i64);
    }

    // ==============================================================================================
    // L-REIHE (D10, 2026-08-03) -- die KOSTEN des Refill-Scans, gemessen in ITERATIONEN.
    //
    // Der Befund war GELESEN: die Donee-Schleife aus H-b (`for d in 0..self.tcbs.len()`) liegt
    // INNERHALB der Schleife ueber die Thread-Tabelle. Gemessen wird hier, was das kostet -- und
    // zwar getrennt nach den drei Schleifen, die es gibt:
    //
    //   SCAN_AUSSEN     `refill_depleted`, die aeussere Schleife  -- **bestand schon vorher**,
    //                   O(n) je Tick, solange irgendein Konto erschoepft ist
    //   SCAN_INNEN      `refill_depleted`, die Donee-Schleife      -- NEU seit H-b, O(n) je
    //                   AUFGEFUELLTEM Konto  ==> der D10-Befund
    //   SCAN_SETBUDGET  `set_budget`, `was_depleted`-Zweig         -- NEU seit H-b
    //   SCAN_ZOMBIE     `record_zombie`                            -- NEU seit H-b
    //
    // **Die Sprechprobe steht in L1:** dieselbe Messgroesse bei 32 und bei 10000 Slots. Bleibt
    // sie gleich, misst sie nicht die Tabellengroesse, und dann sagt keine Zahl darunter etwas.
    // ==============================================================================================

    /// L0 -- die ZUSICHERUNG aus `on_tick`: „Normalfall: kein Konto erschoepft -> der Tick
    ///       kostet nichts, UNABHAENGIG VON DER TABELLENGROESSE."
    fn l0(b: &mut Bericht, cap: usize, tag: &str) {
        let (mut s, _idle) = aufbau_mit(cap);
        let _t = faden(&mut s, PRIO_T, 1); // Thread OHNE Budget -> nichts erschoepft je
        s.on_tick(0, F, false);
        z_null();
        for _ in 0..200 {
            s.on_tick(0, F, true);
        }
        b.k(&format!("L0.{tag}.tabelle"), s.tcbs.len() as i64);
        b.k(&format!("L0.{tag}.ruhe_aussen"), z(&SCAN_AUSSEN));
        b.k(&format!("L0.{tag}.ruhe_innen"), z(&SCAN_INNEN));
        b.k(&format!("L0.{tag}.depleted_count"), s.depleted_count as i64);
    }

    /// L1 -- EIN Konto, EIN Donee (genau der Aufbau der Positivkontrolle P2). Getrennt gemessen:
    ///       was kostet ein WARTETAKT (Konto erschoepft, Refill noch nicht faellig) und was
    ///       kostet der REFILL-TAKT selbst.
    fn l1(b: &mut Bericht, cap: usize, tag: &str) {
        let (mut s, _li, la, ls) = donee_aufbau_mit(cap, BUDGET);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        assert!(s.tcbs[la].depleted, "das Konto muss erschoepft sein");
        assert!(s.tcbs[ls].blocked, "der Donee muss geblockt sein");
        let (_, vor) = s.budget_stats();
        let (mut warte_a, mut warte_i, mut wartetakte) = (0i64, 0i64, 0i64);
        let (mut refill_a, mut refill_i) = (-1i64, -1i64);
        for _ in 0..(PERIOD as i64 + 10) {
            z_null();
            s.on_tick(0, F, true);
            let (a, i) = (z(&SCAN_AUSSEN), z(&SCAN_INNEN));
            if s.budget_stats().1 > vor {
                refill_a = a;
                refill_i = i;
                break;
            }
            warte_a += a;
            warte_i += i;
            wartetakte += 1;
        }
        b.k(&format!("L1.{tag}.tabelle"), s.tcbs.len() as i64);
        b.k(&format!("L1.{tag}.wartetakte"), wartetakte);
        b.k(
            &format!("L1.{tag}.aussen_je_wartetakt"),
            if wartetakte > 0 { warte_a / wartetakte } else { -1 },
        );
        b.k(
            &format!("L1.{tag}.innen_je_wartetakt"),
            if wartetakte > 0 { warte_i / wartetakte } else { -1 },
        );
        b.k(&format!("L1.{tag}.refilltakt_aussen"), refill_a);
        // ==> DIE MESSUNG: die Donee-Schleife im Refill-Takt.
        b.k(&format!("L1.{tag}.refilltakt_innen"), refill_i);
        b.k(
            &format!("L1.{tag}.depleted_count_nach"),
            s.depleted_count as i64,
        );
        b.k(
            &format!("L1.{tag}.donee_geweckt"),
            ja(!s.tcbs[ls].blocked),
        );
    }

    /// L3 -- der O(n^2)-VERSUCH. Der Todo-Eintrag laesst offen, ob „viele Konten refillen im
    ///       SELBEN Tick" ueberhaupt erreichbar ist; dagegen spricht, dass je Kern und Tick
    ///       hoechstens EIN Konto erschoepft. Das stimmt -- und reicht nicht:
    ///       `next_refill = now + period`, und die PERIODE waehlt der Mandant. Erschoepft
    ///       Konto i im Tick `m+i` und traegt es die Periode `Z - m - i`, faellt sein Refill
    ///       auf denselben Tick `Z` wie alle anderen. Kein Privileg, nur `set_budget`.
    ///
    ///       Drei Varianten, weil sie sich unterscheiden MUESSEN, wenn der Zaehler wirkt:
    ///         a) kein Donee            -- niemand ist budget-blockiert
    ///         b) EIN Donee             -- einer ist es
    ///         c) je Konto EIN Donee    -- der echte schlechteste Fall
    fn l3(b: &mut Bericht, tag: &str, k: usize, donees: usize) {
        let cap = L_GROSS;
        let (mut s, _idle) = aufbau_mit(cap);
        let mut ida = Vec::new();
        let mut ids = Vec::new();
        let mut la = Vec::new();
        for i in 0..k {
            let a = faden(&mut s, PRIO_T, 1 + 2 * i);
            let srv = faden(&mut s, PRIO_T, 2 + 2 * i);
            la.push(lok(&s, a));
            ida.push(a);
            ids.push(srv);
            // Alle parken: dann bestimmt allein `unblock`, wer als naechstes laeuft -- die
            // Reihenfolge ist damit gesetzt und nicht dem Rundlauf ueberlassen.
            s.pause(a);
            s.pause(srv);
        }
        let ziel = s.ticks() + 3 * k as u64 + 50;
        for i in 0..k {
            assert!(s.unblock(ida[i]), "unblock A{i}");
            s.on_tick(0, F, false); // YIELD: A_i wird current, kostet kein Budget
            assert_eq!(s.current, Some(la[i]), "A{i} muss laufen");
            if i < donees {
                s.switch_to(0, F, ids[i]); // S_i laeuft gegen das Konto von A_i
            }
            let m = s.ticks();
            assert!(ziel > m + 1, "Zieltick zu knapp gewaehlt");
            assert!(
                s.set_budget(ida[i], 1, (ziel - m - 1) as u32),
                "set_budget A{i}"
            );
            s.on_tick(0, F, true); // now = m+1 -> A_i erschoepft, next_refill = ziel
            assert!(s.tcbs[la[i]].depleted, "A{i} muss erschoepft sein");
        }
        b.k(&format!("L3{tag}.konten"), k as i64);
        b.k(&format!("L3{tag}.depleted_count"), s.depleted_count as i64);
        b.k(
            &format!("L3{tag}.budget_blocked_echt"),
            echte_budget_blocked(&s),
        );
        // Bis zum Zieltick laufen und JEDEN Tick einzeln messen.
        let (mut max_a, mut max_i, mut max_r) = (0i64, 0i64, 0i64);
        let mut takte = 0i64;
        while s.ticks() < ziel {
            let (_, r0) = s.budget_stats();
            z_null();
            s.on_tick(0, F, true);
            takte += 1;
            let (a, i) = (z(&SCAN_AUSSEN), z(&SCAN_INNEN));
            let dr = (s.budget_stats().1 - r0) as i64;
            if dr > max_r {
                max_r = dr;
            }
            if a > max_a {
                max_a = a;
            }
            if i > max_i {
                max_i = i;
            }
        }
        b.k(&format!("L3{tag}.takte"), takte);
        // ==> DIE MESSUNG: refillen wirklich alle im SELBEN Tick, und was kostet dieser Tick?
        b.k(&format!("L3{tag}.refills_max_tick"), max_r);
        b.k(&format!("L3{tag}.aussen_max_tick"), max_a);
        b.k(&format!("L3{tag}.innen_max_tick"), max_i);
        b.k(
            &format!("L3{tag}.innen_max_je_konto"),
            if k > 0 { max_i / (k as i64) } else { -1 },
        );
    }

    /// L3d -- **dieselbe Wirkung, ohne dass irgendwer eine Periode waehlt.**
    ///        `attach_migrated` rechnet `next_refill` auf die eigene Tick-Uhr um:
    ///        `tcb.next_refill = self.now + tcb.period`. Kommen mehrere erschoepfte Threads
    ///        **im selben Tick** auf einem Kern an, ist das fuer alle dieselbe Zahl -- und die
    ///        Periode muss dafuer nicht gewaehlt, sondern nur GETEILT werden (dieselbe
    ///        SchedContext-Cap). Ausgeloest wird es vom Lastausgleich, nicht vom Mandanten.
    fn l3d(b: &mut Bericht, k: usize) {
        let cap = L_GROSS;
        const P: u32 = 500;
        // Zwei Kerne an EINEM Directory -- genau die Lage, in der der Kernel migriert.
        // SAFETY: frischer, exklusiver Speicher (s. `aufbau_mit`).
        unsafe { attach_directory(roh(directory_bytes(cap), directory_align()), cap) };
        let mut a = Scheduler::new();
        // SAFETY: wie oben.
        unsafe { a.attach_storage(0, roh(core_storage_bytes(cap), core_storage_align()), cap) };
        a.init_core(0, PRIO_IDLE).expect("init_core 0");
        let mut zk = Scheduler::new();
        // SAFETY: wie oben.
        unsafe { zk.attach_storage(1, roh(core_storage_bytes(cap), core_storage_align()), cap) };
        zk.init_core(1, PRIO_IDLE).expect("init_core 1");

        let mut ids = Vec::new();
        for i in 0..k {
            let t = a
                .spawn(0, 0x1000 + i, 0, 0x10_0000 + i * 0x1000, 0x1000, PRIO_T)
                .expect("spawn");
            // **Dieselbe** Periode fuer alle -- nicht gewaehlt, sondern geteilt.
            assert!(a.set_budget(t, 1, P), "set_budget {i}");
            ids.push(t);
        }
        a.on_tick(0, F, false); // der erste wird current
        for _ in 0..k {
            a.on_tick(0, F, true); // je Tick erschoepft genau EINES -- unterschiedliche next_refill
        }
        b.k("L3d.erschoepft_auf_quelle", a.depleted_count as i64);
        // Und jetzt wandern sie -- alle im selben Tick des Zielkerns.
        let mut gewandert = 0i64;
        for t in ids.iter() {
            if let Some(m) = a.detach_for_migration(*t) {
                assert!(zk.attach_migrated(m).is_ok(), "attach_migrated");
                gewandert += 1;
            }
        }
        b.k("L3d.gewandert", gewandert);
        b.k("L3d.erschoepft_auf_ziel", zk.depleted_count as i64);
        let (mut max_i, mut max_r) = (0i64, 0i64);
        for _ in 0..(P as i64 + 5) {
            let (_, r0) = zk.budget_stats();
            z_null();
            zk.on_tick(1, F, true);
            let dr = (zk.budget_stats().1 - r0) as i64;
            if dr > max_r {
                max_r = dr;
                max_i = z(&SCAN_INNEN);
            }
        }
        // ==> DIE MESSUNG: wie viele Refills fallen auf EINEN Tick, ohne jede Periodenwahl?
        b.k("L3d.refills_max_tick", max_r);
        b.k("L3d.innen_max_tick", max_i);
    }

    /// L4 -- `record_zombie` (je Thread-Tod) und L5 -- `set_budget` im `was_depleted`-Zweig.
    ///       Beide waren vor H-b O(1). Nicht im Tick-Pfad, deshalb getrennt ausgewiesen.
    fn l45(b: &mut Bericht, cap: usize, tag: &str) {
        let (mut s, _idle) = aufbau_mit(cap);
        let t = faden(&mut s, PRIO_T, 1);
        z_null();
        assert!(s.kill(t, 0), "kill");
        b.k(&format!("L4.{tag}.zombie_scan"), z(&SCAN_ZOMBIE));

        let (mut s, _idle) = aufbau_mit(cap);
        let t = faden(&mut s, PRIO_T, 1);
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget");
        s.on_tick(0, F, false);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        assert!(s.tcbs[lok(&s, t)].depleted, "muss erschoepft sein");
        z_null();
        assert!(s.set_budget(t, BUDGET, PERIOD), "set_budget erneut");
        b.k(&format!("L5.{tag}.setbudget_scan"), z(&SCAN_SETBUDGET));
    }

    /// L9 -- **die Nachzaehlung.** Ein Zaehler nach dem Muster von `depleted_count` ist genau
    ///       die Konstruktion, die an diesem Projekt am selben Tag schon einmal gelogen hat
    ///       (D8/M5: mehrfach erhoeht, einmal gesenkt, nie wieder 0 -- und der Scan lief ab da
    ///       in jedem Tick). Deshalb wird er hier gegen eine **unabhaengig hergeleitete** Zahl
    ///       gehalten: die Tabelle wird gezaehlt. Zwei Stellen pruefen dasselbe:
    ///       `audit()` (im Kernel-Quelltext, Code 10) und diese Zeile (im Messwerkzeug).
    fn l9(b: &mut Bericht) {
        println!("  L9  Nachzaehlung: Zaehler gegen die gezaehlte Tabelle");
        let (mut s, _li, la, ls) = donee_aufbau_mit(L_KLEIN, BUDGET);
        b.k("L9.start.echt", echte_budget_blocked(&s));
        b.k("L9.start.zaehler", zaehler_budget_blocked(&s));
        b.k("L9.start.audit", s.audit() as i64);
        for _ in 0..BUDGET {
            s.on_tick(0, F, true);
        }
        // Jetzt ist genau EINER budget-blockiert: der Donee.
        b.k("L9.blockiert.echt", echte_budget_blocked(&s));
        b.k("L9.blockiert.zaehler", zaehler_budget_blocked(&s));
        b.k("L9.blockiert.ist_der_donee", ja(s.tcbs[ls].blocked));
        b.k("L9.blockiert.audit", s.audit() as i64);
        let (_, passiert) = bis_refill(&mut s, PERIOD as i64 + 10);
        b.k("L9.refill_passiert", ja(passiert));
        // ... und nach dem Refill KEINER mehr. Kehrt der Zaehler nicht auf 0 zurueck, ist der
        // Waechter dauerhaft wahr und die Behebung wirkungslos -- die M5-Form.
        b.k("L9.nach_refill.echt", echte_budget_blocked(&s));
        b.k("L9.nach_refill.zaehler", zaehler_budget_blocked(&s));
        b.k(
            "L9.nach_refill.zaehler_luegt_um",
            zaehler_budget_blocked(&s) - echte_budget_blocked(&s),
        );
        // ==> DIE MESSUNG: schlaegt `audit()` an, wenn der Zaehler luegt?
        b.k("L9.nach_refill.audit", s.audit() as i64);
        let _ = la;

        // Und dieselbe Frage nach den drei uebrigen Aufloesungswegen (PAUSE, Kontotod,
        // set_budget) -- jeder von ihnen senkt den Zaehler, und jeder einzeln ist eine Stelle,
        // an der er stehenbleiben koennte.
        for (nr, was) in ["pause", "kill", "setbudget"].iter().enumerate() {
            let (mut s, _li, la, ls) = donee_aufbau_mit(L_KLEIN, BUDGET);
            for _ in 0..BUDGET {
                s.on_tick(0, F, true);
            }
            match nr {
                0 => {
                    s.pause(tid(&s, ls));
                }
                1 => {
                    assert!(s.kill(tid(&s, la), 0), "kill Konto");
                }
                _ => {
                    assert!(s.set_budget(tid(&s, la), BUDGET, PERIOD), "set_budget");
                }
            }
            b.k(&format!("L9.{was}.echt"), echte_budget_blocked(&s));
            b.k(&format!("L9.{was}.zaehler"), zaehler_budget_blocked(&s));
            b.k(
                &format!("L9.{was}.zaehler_luegt_um"),
                zaehler_budget_blocked(&s) - echte_budget_blocked(&s),
            );
            b.k(&format!("L9.{was}.audit"), s.audit() as i64);
        }
    }

    // ==============================================================================================

    pub fn leistung(fassung: &str) -> i32 {
        println!("== D10-Kostenmessung (instrumentierte Fassung: {fassung}) ==");
        let mut b = Bericht { zeilen: Vec::new() };
        println!("  L0  Ruhe: kein Konto erschoepft -- die Zusicherung aus `on_tick`");
        l0(&mut b, L_KLEIN, "klein");
        l0(&mut b, L_GROSS, "gross");
        println!("  L1  ein Konto, ein Donee -- Wartetakt gegen Refill-Takt (SPRECHPROBE)");
        l1(&mut b, L_KLEIN, "klein");
        l1(&mut b, L_GROSS, "gross");
        println!("  L3  der O(n^2)-Versuch: viele Konten refillen im SELBEN Tick");
        l3(&mut b, "a", 100, 0);
        l3(&mut b, "b", 100, 1);
        l3(&mut b, "c", 100, 100);
        l3d(&mut b, 100);
        println!("  L4/L5  record_zombie und set_budget (nicht im Tick-Pfad)");
        l45(&mut b, L_KLEIN, "klein");
        l45(&mut b, L_GROSS, "gross");
        l9(&mut b);

        let hol = |n: &str| -> i64 {
            b.zeilen
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(-999)
        };
        // **Die Sprechprobe des Messgeraets.** Ohne diesen Kontrast misst die L-Reihe nichts:
        // eine Zahl, die bei 32 und bei 10000 Slots gleich ist, sagt ueber die Tabellengroesse
        // gar nichts -- und genau darum geht D10.
        let gross_teurer = hol("L1.gross.refilltakt_aussen") > hol("L1.klein.refilltakt_aussen");
        let lk = hol("L0.klein.ruhe_aussen") == 0
            && hol("L0.gross.ruhe_aussen") == 0
            && hol("L0.gross.ruhe_innen") == 0
            && hol("L1.klein.tabelle") == L_KLEIN as i64
            && hol("L1.gross.tabelle") == L_GROSS as i64
            && hol("L1.klein.donee_geweckt") == 1
            && hol("L1.gross.donee_geweckt") == 1
            && gross_teurer
            && hol("L3c.refills_max_tick") > 1 // der O(n^2)-Aufbau ist wirklich entstanden
            // ... und der Zaehler luegt an keiner der vier Aufloesungsstellen. Ohne diese
            // Zeilen waere er genau der von D8/M5: eine Zahl, die niemand nachhaelt.
            && hol("L9.nach_refill.zaehler_luegt_um") == 0
            && hol("L9.nach_refill.audit") == 0
            && hol("L9.pause.zaehler_luegt_um") == 0
            && hol("L9.pause.audit") == 0
            && hol("L9.kill.zaehler_luegt_um") == 0
            && hol("L9.kill.audit") == 0
            && hol("L9.setbudget.zaehler_luegt_um") == 0
            && hol("L9.setbudget.audit") == 0
            && hol("L9.blockiert.zaehler") == hol("L9.blockiert.echt");

        println!();
        println!("  -- Werte ({}) --", fassung);
        for (k, v) in &b.zeilen {
            println!("  {k}={v}");
        }
        println!();
        println!(
            "  SPRECHPROBE (klein != gross, Ruhe == 0, Zaehler luegt nicht): {}",
            if lk { "BESTANDEN" } else { "DURCHGEFALLEN" }
        );
        println!("-- {} Messwerte, Fassung {} --", b.zeilen.len(), fassung);
        if lk {
            0
        } else {
            1
        }
    }

    pub fn run(fassung: &str) -> i32 {
        println!("== Messung am echten caprock-sched (Fassung: {fassung}) ==");
        let mut b = Bericht { zeilen: Vec::new() };
        p0(&mut b);
        p1(&mut b);
        m1(&mut b);
        m2(&mut b);
        m3(&mut b);
        m4(&mut b);
        m5(&mut b);
        m6(&mut b);
        m7(&mut b);
        p2(&mut b);
        d1(&mut b);
        d2(&mut b);
        d3(&mut b);
        d4(&mut b);
        d5(&mut b);
        d6(&mut b);
        d7(&mut b);

        // Die Positivkontrolle entscheidet, ob irgendeine andere Zahl etwas wert ist.
        let hol = |n: &str| -> i64 {
            b.zeilen
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(-999)
        };
        let pk = hol("P0.nach_spawn.in_liste") == 1
            && hol("P0.nach_pause.in_liste") == 0      // das Messgeraet kann NEIN sagen
            && hol("P0.nach_unblock.in_liste") == 1
            && hol("P0.liste_gleich_flag") == 1
            && hol("P0.audit") == 0
            && hol("P1.t_ist_current") == 1
            && hol("P1.vor_unblock.blocked") == 1
            && hol("P1.vor_unblock.depleted") == 0
            && hol("P1.vor_unblock.in_liste") == 0
            && hol("P1.vor_unblock.current") == 0
            && hol("P1.nach_unblock.in_liste") == 1
            && hol("P1.depleted_count") == 0
            && hol("P1.depletions") == 0
            && hol("P1.audit") == 0
            && hol("P1.wird_current") == 1
            // --- Positivkontrolle des DONEE-Zweigs (2026-08-03). Ohne sie ist keine Zahl aus
            //     D1..D7 etwas wert -- und eine Behebung, die den gesunden Donee nicht mehr
            //     weckt, faellt HIER durch statt still zu verhungern.
            && hol("P2.donee_link") == 1
            && hol("P2.erschoepft.konto_depleted") == 1
            && hol("P2.erschoepft.donee_blocked") == 1
            && hol("P2.erschoepft.donee_in_liste") == 0 // das Messgeraet kann NEIN sagen
            && hol("P2.erschoepft.audit") == 0
            && hol("P2.refill_passiert") == 1
            && hol("P2.nach_refill.donee_blocked") == 0
            && hol("P2.nach_refill.donee_current") == 1
            && hol("P2.nach_refill.alternative_bereit") == 1
            && hol("P2.nach_refill.audit") == 0
            && hol("P2.tick_auf_gefuelltem_konto") == 1;

        println!();
        println!("  -- Werte ({}) --", fassung);
        for (k, v) in &b.zeilen {
            println!("  {k}={v}");
        }
        println!();
        println!(
            "  POSITIVKONTROLLE: {}",
            if pk { "BESTANDEN" } else { "DURCHGEFALLEN" }
        );
        println!("-- {} Messwerte, Fassung {} --", b.zeilen.len(), fassung);
        if pk {
            0
        } else {
            1
        }
    }
}

fn main() {
    let f = std::env::args().nth(1).unwrap_or_else(|| "echt".to_string());
    let modus = std::env::args().nth(2).unwrap_or_default();
    std::process::exit(if modus == "leistung" {
        messung::leistung(&f)
    } else {
        messung::run(&f)
    });
}
RSEOF
}

# ================================================================================================
# 2. Bauen.
# ================================================================================================
no_std_raus() {   # no_std_raus <quelle> <ziel> <muster>
    python3 - "$1" "$2" "$3" <<'PY'
import io, sys
q, z, m = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    s = io.open(q, encoding='utf-8').read()
except OSError as e:
    sys.exit("FEHLER: %s nicht lesbar (%s)." % (q, e))
if m not in s:
    sys.exit("FEHLER: %r in %s NICHT GEFUNDEN.\n"
             "        Das Werkzeug nimmt an, dies sei eine no_std-Kernel-Crate; ist sie es\n"
             "        nicht, stimmt seine Annahme ueber den Gegenstand nicht mehr." % (m, q))
s = s.replace(m, "// %s -- fuer den Host-Lauf des Messwerkzeugs entfernt." % m.strip(), 1)
io.open(z, 'w', encoding='utf-8').write(s)
PY
}

einbetten() {   # einbetten <quelle> <modulname> <muster>  -> stdout
    python3 - "$1" "$2" "$3" <<'PY'
import io, sys
q, name, m = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    s = io.open(q, encoding='utf-8').read()
except OSError as e:
    sys.exit("FEHLER: %s nicht lesbar (%s)." % (q, e))
if m not in s:
    sys.exit("FEHLER: %r in %s NICHT GEFUNDEN." % (m, q))
s = s.replace(m, "// %s -- als Modul eingebettet." % m.strip(), 1)
print("// Aus %s uebernommen -- ECHTER Quelltext, unveraendert bis auf die no_std-Zeile." % q)
print("mod %s {" % name)
for z in s.splitlines():
    print(("    " + z) if z.strip() else z)
print("}")
PY
}

harness_bauen() {   # harness_bauen <lib.rs> <arbeitsverzeichnis>
    local code="$1" W="$2" ausgabe f

    for f in "$code" "$CYCLES_STD" "$SLAB_STD" "$SYNC_STD"; do
        [ -f "$f" ] || { echo "  FEHLER: $f fehlt." >&2; return 2; }
    done

    if ! no_std_raus "$code" "$W/harness.rs" '#![no_std]'; then return 2; fi
    cp "$CYCLES_STD" "$W/cycles.rs" || return 2

    if ! ausgabe="$(einbetten "$SLAB_STD" caprock_slab '#![no_std]' 2>&1)"; then
        printf '%s\n' "$ausgabe" >&2; return 2
    fi
    printf '%s\n' "$ausgabe" >> "$W/harness.rs"
    if ! ausgabe="$(einbetten "$SYNC_STD" caprock_sync '#![cfg_attr(not(loom), no_std)]' 2>&1)"; then
        printf '%s\n' "$ausgabe" >&2; return 2
    fi
    printf '%s\n' "$ausgabe" >> "$W/harness.rs"

    anbau_schreiben "$W/anbau.rs"
    cat "$W/anbau.rs" >> "$W/harness.rs"

    rm -f "$W/mess.bin"
    if ! ausgabe="$($RUSTC --edition 2021 -A warnings "$W/harness.rs" -o "$W/mess.bin" 2>&1)"; then
        echo "  FEHLER: das Messwerkzeug liess sich nicht uebersetzen." >&2
        printf '%s\n' "$ausgabe" | sed 's/^/    /' >&2
        echo "    Ein Uebersetzungsfehler ist hier ein BEFUND: der echte Quelltext passt nicht" >&2
        echo "    mehr zu dem, was das Werkzeug von ihm annimmt." >&2
        return 2
    fi
    [ -x "$W/mess.bin" ] || { echo "  FEHLER: kein lauffaehiges Binary." >&2; return 2; }
    return 0
}

# Rueckgabe: 0 = Positiv-/Sprechprobe bestanden, 1 = durchgefallen, 2 = Werkzeugfehler,
#            3 = der Lauf ist ABGEBROCHEN (nur bei ausdruecklich dafuer erklaerten Fassungen).
#
# Zu 3: eine Fassung, die den Zaehler nach UNTEN luegen laesst, laeuft in die naechste
# Subtraktion und bricht dort ab. Das ist eine Erkennung und kein Werkzeugfehler -- aber nur
# fuer die Fassungen, bei denen es erwartet wird; ueberall sonst bleibt „kein Messwert" ein
# Fehler ("ein leerer Lauf ist kein Testergebnis").
fahren() {   # fahren <arbeitsverzeichnis> <fassung> [--leise] [modus] [--darf-abbrechen]
    local W="$1" fassung="$2" leise="${3:-}" modus="${4:-}" abbruch_ok="${5:-}" ausgabe rc n
    ausgabe="$("$W/mess.bin" "$fassung" $modus 2>&1)"; rc=$?
    printf '%s\n' "$ausgabe" > "$W/ausgabe.txt"
    n="$(printf '%s\n' "$ausgabe" | sed -n 's/^-- \([0-9]*\) Messwerte.*/\1/p')"
    if [ -z "$n" ] || [ "$n" -lt 1 ]; then
        if [ -n "$abbruch_ok" ]; then
            printf '%s\n' "$ausgabe" | grep -m1 "panicked at" | sed 's/^/    Abbruch: /'
            return 3
        fi
        echo "  FEHLER: kein Messwert entstanden -- ein leerer Lauf ist kein Ergebnis." >&2
        printf '%s\n' "$ausgabe" | sed 's/^/    /' >&2
        return 2
    fi
    [ -n "$leise" ] || printf '%s\n' "$ausgabe" | sed 's/^/  /'
    return "$rc"
}

mutieren() {   # mutieren <zieldatei> <python-programm>
    python3 - "$1" "$2" <<'PY'
import io, sys
p, prog = sys.argv[1], sys.argv[2]
s = io.open(p, encoding='utf-8').read()
vorher = s
ns = {'s': s}
exec(prog, ns)
if ns['s'] == vorher:
    sys.exit("FEHLER: die Mutation hat NICHTS geaendert -- der Anker passt nicht mehr.\n"
             "        Eine Gegenprobe an unveraendertem Quelltext ist keine Gegenprobe.")
io.open(p, 'w', encoding='utf-8').write(ns['s'])
PY
}

wert() {   # wert <ausgabedatei> <schluessel>
    sed -n "s/^  $2=\(-\?[0-9]*\)$/\1/p" "$1" | head -1
}

# ================================================================================================
# 3. Ablauf.
# ================================================================================================
NUR_ECHT=0
[ "${1:-}" = "--nur-echt" ] && NUR_ECHT=1

echo "== sched-erschoepfung-messen: die GELESENEN Befunde B1/B2 ausloesen =="
echo
echo "  Herkunft (der Gegenstand, nicht eine Nachbildung):"
for f in "$CODE_STD" "$CYCLES_STD" "$SLAB_STD" "$SYNC_STD"; do
    printf '    %-44s %5s Zeilen  %s\n' "${f#$ROOT/}" "$(wc -l < "$f")" "$(sha256sum "$f" | cut -c1-12)"
done
echo

W_ECHT="$WURZEL/echt"; mkdir -p "$W_ECHT"
harness_bauen "$CODE_STD" "$W_ECHT" || exit 2
fahren "$W_ECHT" echt; RC_ECHT=$?
[ "$RC_ECHT" = 2 ] && exit 2
echo

if [ "$RC_ECHT" != 0 ]; then
    echo "ABBRUCH: die Positivkontrolle ist durchgefallen. Jede weitere Zahl waere wertlos." >&2
    exit 1
fi
[ "$NUR_ECHT" = 1 ] && exit 0

# --- Gegenproben ---------------------------------------------------------------------------
#
# **Die Anker sind der GEGENSTAND, nicht seine Geschichte.** Bis zum 2026-08-03 standen hier die
# Mutationen der D8-Messung; nach deren Behebung passte kein einziger Anker mehr, und die
# Gegenproben brachen mit "der Anker passt nicht mehr" ab -- die Messung am echten Quelltext lief
# weiter gruen. Eine Gegenprobe, die nach der Behebung ihres eigenen Befundes stumm abbricht, ist
# genau die Form aus der Fallenliste ("ein Waechter, der nach seiner eigenen Behebung
# weiterschreit"). Die Mutationen unten beziehen sich deshalb auf den HEUTIGEN Quelltext.

# V0 -- der Stand VOR D8. Sprechprobe der Gegenprobenmechanik: die alten Befunde muessen hier
#       wieder auftauchen, sonst misst der Vergleich nichts.
V0_UNBLOCK='s = s.replace("""            if !self.tcbs[s].depleted {
                self.enqueue_ready(s);
            }""", """            self.enqueue_ready(s);""", 1)'
V0_REFILL='s = s.replace("""                    _ => {
                        if !self.tcbs[slot].blocked && self.current != Some(slot) {
                            self.enqueue_ready(slot);
                        }
                    }""", """                    _ => self.enqueue_ready(slot),""", 1)'
V0_AUDIT='s = s.replace("""                if t.depleted {
                    return 9;
                }
""", "", 1)'

# H-a -- der Waechter des `_`-Zweigs WOERTLICH in den Donee-Weckelauf uebertragen. Sieht
#        symmetrisch aus und ist es nicht: der Donee ist an dieser Stelle IMMER blockiert
#        (`on_tick` hat ihn gerade blockiert), der Lauf weckt also niemanden mehr.
#
#        **Neu angesetzt am 2026-08-03 (D10).** Bis dahin zeigte der Anker auf den `sc_donee`-Zweig
#        VOR H-b; seit dessen Behebung passte er nicht mehr, und das Werkzeug brach an dieser
#        Stelle mit rc=2 ab -- nach der Behebung von D9, also seit es H-b im Quelltext gibt.
#        Genau die Form aus der eigenen Kopfzeile: die Anker sind der GEGENSTAND, nicht seine
#        Geschichte.
H_A='s = s.replace("""                            && self.tcbs[d].used
                            && self.tcbs[d].budget_blocked
                            && self.tcbs[d].sc_donor == Some(slot)""", """                            && self.tcbs[d].used
                            // H-a: der woertlich uebertragene Waechter aus dem `_`-Zweig.
                            && !self.tcbs[d].blocked
                            && self.current != Some(d)
                            && self.tcbs[d].sc_donor == Some(slot)""", 1)'

# **H-b ist keine Mutation mehr, sondern der Quelltext.** Bis zum 2026-08-03 stand hier der
# Vorschlag aus D9 (`budget_blocked`, der Refill weckt den ganzen Spenden-STAPEL). Er ist seit
# Commit 068db2a in `crates/caprock-sched/src/lib.rs` -- und damit ist die Fassung `echt` die
# Fassung H-b. Ihn hier stehen zu lassen hiesse, das Feld ein zweites Mal einzufuegen; das
# Werkzeug wuerde mit einem Uebersetzungsfehler abbrechen. Was H-b war, steht in `TODO.md` (D9)
# und im Commit; was H-b TUT, misst jede Zeile der D-Reihe an der Fassung `echt`.

# --- D10 (2026-08-03) -------------------------------------------------------------------------
# ohne-Zaehler -- der Stand VOR D10: der Waechter faellt weg, der Weckelauf laeuft wieder
#                 bedingungslos. Er dient ZWEI Aussagen zugleich: die Kosten muessen sichtbar
#                 hoeher sein (L-Reihe), und das VERHALTEN muss in JEDER Zeile gleich bleiben
#                 (Verhaltenstabelle). Eine Behebung, die nur Kosten senken soll und dabei eine
#                 Zahl verschiebt, ist keine.
OHNE_ZAEHLER='s = s.replace("""                if self.budget_blocked_count > 0 {
                    for d in 0..self.tcbs.len() {
                        if d != slot""", """                if true {
                    for d in 0..self.tcbs.len() {
                        if d != slot""", 1)
s = s.replace("""                if self.budget_blocked_count > 0 {
                    for d in 0..self.tcbs.len() {
                        if d != s""", """                if true {
                    for d in 0..self.tcbs.len() {
                        if d != s""", 1)'

# Luegner-hoch -- **die Form, die dieses Projekt am 2026-08-03 schon einmal hatte** (D8/M5):
#                 der Zaehler wird erhoeht und nie gesenkt, kehrt nie auf 0 zurueck, der teure
#                 Lauf ist ab da dauerhaft scharf. Die Nachzaehlung in `audit()` muss anschlagen
#                 (Code 10) -- sonst ist der neue Zaehler genau der alte.
LUEGNER_HOCH='s = s.replace("""        if an {
            self.budget_blocked_count += 1;
        } else {
            self.budget_blocked_count -= 1;
        }""", """        if an {
            self.budget_blocked_count += 1;
        }""", 1)'

# Luegner-runter -- die andere Richtung: der Zaehler bleibt 0, der Waechter ueberspringt den
#                 Lauf, und der Donee wird NIE geweckt (die D5-Form). Auch das muss die
#                 Nachzaehlung sehen -- ein Waechter, der nur eine Richtung prueft, prueft halb.
LUEGNER_RUNTER='s = s.replace("""        if an {
            self.budget_blocked_count += 1;
        } else {""", """        if an {
        } else {""", 1)'

echo "== Gegenproben (Mutationen ausschliesslich auf KOPIEN) =="
GEGENPROBEN="V0 H-a ohne-Zaehler Luegner-hoch Luegner-runter"
for fassung in $GEGENPROBEN; do
    Wg="$WURZEL/$fassung"; mkdir -p "$Wg"
    cp "$CODE_STD" "$Wg/lib.rs" || exit 2
    case "$fassung" in
        V0)  mutieren "$Wg/lib.rs" "$V0_UNBLOCK" || exit 2
             mutieren "$Wg/lib.rs" "$V0_REFILL"  || exit 2
             mutieren "$Wg/lib.rs" "$V0_AUDIT"   || exit 2 ;;
        H-a) mutieren "$Wg/lib.rs" "$H_A" || exit 2 ;;
        ohne-Zaehler)   mutieren "$Wg/lib.rs" "$OHNE_ZAEHLER" || exit 2 ;;
        Luegner-hoch)   mutieren "$Wg/lib.rs" "$LUEGNER_HOCH" || exit 2 ;;
        Luegner-runter) mutieren "$Wg/lib.rs" "$LUEGNER_RUNTER" || exit 2 ;;
    esac
    harness_bauen "$Wg/lib.rs" "$Wg" || exit 2
    abbruch_ok=""; [ "$fassung" = "Luegner-runter" ] && abbruch_ok="--darf-abbrechen"
    fahren "$Wg" "$fassung" --leise "" "$abbruch_ok"; rc=$?
    if [ "$rc" = 2 ]; then exit 2; fi
    case "$rc" in
        0) pk="BESTANDEN" ;;
        3) pk="ABGEBROCHEN (die Subtraktion selbst erkennt es)" ;;
        *) pk="DURCHGEFALLEN" ;;
    esac
    echo "  $fassung gebaut und gefahren -- Positivkontrolle: $pk"
done
echo

# --- Vergleich der entscheidenden Groessen ---------------------------------------------------
SCHLUESSEL="
M1.nach_unblock.in_liste
M1.wird_current
M1.depletions_delta
M4.nach_refill.in_liste
M4.pausierter_ist_current
M4b.nach_refill.audit
M5.zaehler_luegt_um
M7.ticks_mit_budget
M7.ende.blocked
P2.nach_refill.donee_blocked
P2.nach_refill.donee_current
P2.tick_auf_gefuelltem_konto
P2.erschoepft.audit
P2.nach_refill.audit
D1.nach_pause.donee_blocked
D1.nach_refill.donee_blocked
D1.pausierter_ist_current
D1.alternative_bereit
D1.pausierter_verbraucht_budget
D1.nach_resume.ticks_gelaufen
D1.nach_resume.ticks_mit_budget
D1.nach_resume.blocked
D1b.pausierter_ist_current
D2.nach_refill.donee_depleted
D2.nach_refill.donee_in_liste
D2.nach_refill.audit
D3a.nach_kill.sc_donee_geloescht
D3a.toter_slot_in_liste
D3b.donee_detach_verweigert
D3b.sprechprobe_fremder_detach_geht
D4.nach_kill.donee_blocked
D4.ticks_donee_lief
D4.ende.donee_blocked
D4.ende.audit
D5.sc_donee_beim_erschoepfen
D5.refills
D5.ticks_mid_lief
D5.ende.mid_blocked
D5.ende.audit
D6.nach_unblock.donee_in_liste
D6.wird_current
D6.depletions_delta
D6.zaehler_luegt_um
D7.ticks_donee_lief
D7.ende.donee_blocked
"

# Die Spalten entstehen aus $GEGENPROBEN -- wer eine Fassung dazunimmt, bekommt sie hier
# automatisch, statt eine Kopfzeile und eine `wert`-Zeile getrennt nachzupflegen (genau so
# entstehen Beschriftungen, die neben der Sache herlaufen).
printf '  %-36s %14s' "Groesse (Verhalten)" "echt"
for f in $GEGENPROBEN; do printf ' %14s' "$f"; done
printf '\n'
for k in $SCHLUESSEL; do
    ve="$(wert "$W_ECHT/ausgabe.txt" "$k")"
    printf '  %-36s %14s' "$k" "${ve:--}"
    for f in $GEGENPROBEN; do
        v="$(wert "$WURZEL/$f/ausgabe.txt" "$k")"
        printf ' %14s' "${v:--}"
    done
    printf '\n'
done
echo

# --- D10: das VERHALTEN darf sich nicht bewegt haben ------------------------------------------
#
# Die Tabelle oben zeigt 45 ausgewaehlte Groessen. Das reicht fuer eine Aussage ueber die
# BEHEBUNG nicht: sie soll KOSTEN senken und sonst nichts, und „sonst nichts" ist eine Aussage
# ueber ALLE Messwerte, nicht ueber die, die jemand in eine Liste geschrieben hat. Deshalb wird
# hier stumpf alles verglichen -- eine ausgewaehlte Liste haette genau die Zeile nicht enthalten,
# die sich bewegt.
werte_alle() { sed -n 's/^  \([A-Za-z][A-Za-z0-9_.]*\)=\(-\?[0-9]*\)$/\1=\2/p' "$1"; }
D10_OK=1
n_echt="$(werte_alle "$W_ECHT/ausgabe.txt" | wc -l)"
if [ "$n_echt" -lt 1 ]; then
    echo "  D10: FEHLER -- keine Verhaltensmesswerte zum Vergleichen." >&2; D10_OK=0
elif diff -q <(werte_alle "$W_ECHT/ausgabe.txt") \
              <(werte_alle "$WURZEL/ohne-Zaehler/ausgabe.txt") >/dev/null; then
    echo "  D10: alle $n_echt Verhaltensmesswerte von 'ohne-Zaehler' sind mit 'echt' IDENTISCH."
    echo "       Die Behebung senkt Kosten und verschiebt keine einzige Zahl."
else
    echo "  D10: ACHTUNG -- 'echt' und 'ohne-Zaehler' unterscheiden sich im VERHALTEN:" >&2
    diff <(werte_alle "$W_ECHT/ausgabe.txt") \
         <(werte_alle "$WURZEL/ohne-Zaehler/ausgabe.txt") | sed 's/^/       /' >&2
    D10_OK=0
fi
echo
echo "  Lesart:"
echo "   * V0 ist der Stand VOR D8 -- die Sprechprobe der Gegenprobenmechanik. Tauchen dort die"
echo "     alten Befunde (M1/M4/M5) nicht wieder auf, misst der ganze Vergleich nichts."
echo "   * H-a ist der woertlich uebertragene Waechter. Er faellt in der POSITIVKONTROLLE durch:"
echo "     der Donee ist an dieser Stelle IMMER blockiert, also weckt ihn niemand mehr."
echo "   * H-b steht nicht mehr als Spalte: H-b IST seit 2026-08-03 der Quelltext, also die"
echo "     Spalte 'echt'. D1/D4/D5/D6/D7 tragen dort die behobenen Werte."
echo "   * ohne-Zaehler ist der Stand VOR D10 (der Waechter im Refill faellt weg). Sein"
echo "     VERHALTEN muss in JEDER Zeile mit 'echt' uebereinstimmen -- eine Behebung, die nur"
echo "     Kosten senken soll und dabei eine Zahl verschiebt, ist keine."
echo "   * Luegner-hoch / Luegner-runter lassen den neuen Zaehler luegen (Senken bzw. Erhoehen"
echo "     faellt weg). Sie muessen in der Positivkontrolle bzw. an Audit-Code 10 auffallen."
echo "   * D3a/D3b sind das Ergebnis 'nicht ausloesbar': ein veralteter sc_donee entsteht nicht,"
echo "     weil Tod ihn loescht und Migration verweigert wird (Sprechprobe: ohne Spende geht sie)."
echo

# ================================================================================================
# 4. D10 -- die KOSTEN. Eigene, INSTRUMENTIERTE Binaries.
# ================================================================================================
#
# Warum getrennte Binaries: die Verhaltensaussage oben soll am **unveraenderten** Quelltext
# haengen. Die Zaehlzeilen sind zwar wirkungsfrei (ein `fetch_add` auf einem lokalen Static),
# aber „wirkungsfrei" ist eine Behauptung, und die Verhaltensmessung soll sie nicht brauchen.
#
# Gemessen werden ITERATIONEN, nicht Zeit: eine Iterationszahl ist eine Eigenschaft des
# Programms und ueber Laeufe hinweg identisch -- Zeit misst auf einer Mehrkernmaschine den
# Wirt mit.
INSTR_AUSSEN='s = s.replace("""    fn refill_depleted(&mut self) {
        for slot in 0..self.tcbs.len() {""", """    fn refill_depleted(&mut self) {
        for slot in 0..self.tcbs.len() {
            crate::messung::SCAN_AUSSEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);""", 1)'
INSTR_INNEN='s = s.replace("""                    for d in 0..self.tcbs.len() {
                        if d != slot""", """                    for d in 0..self.tcbs.len() {
                        crate::messung::SCAN_INNEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if d != slot""", 1)'
INSTR_SETBUDGET='s = s.replace("""                    for d in 0..self.tcbs.len() {
                        if d != s""", """                    for d in 0..self.tcbs.len() {
                        crate::messung::SCAN_SETBUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if d != s""", 1)'
INSTR_ZOMBIE='s = s.replace("""        for d in 0..self.tcbs.len() {
            if d != local""", """        for d in 0..self.tcbs.len() {
            crate::messung::SCAN_ZOMBIE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if d != local""", 1)'

leistung_fassung() {   # leistung_fassung <name> [zusatzmutation ...]
    local name="$1"; shift
    local Wl="$WURZEL/L-$name" m
    mkdir -p "$Wl"
    cp "$CODE_STD" "$Wl/lib.rs" || return 2
    for m in "$@"; do mutieren "$Wl/lib.rs" "$m" || return 2; done
    for m in "$INSTR_AUSSEN" "$INSTR_INNEN" "$INSTR_SETBUDGET" "$INSTR_ZOMBIE"; do
        mutieren "$Wl/lib.rs" "$m" || return 2
    done
    harness_bauen "$Wl/lib.rs" "$Wl" || return 2
    local abbruch_ok=""
    [ "$name" = "Luegner-runter" ] && abbruch_ok="--darf-abbrechen"
    fahren "$Wl" "L-$name" --leise leistung "$abbruch_ok"
    return $?
}

echo "== D10: die Kosten des Refill-Scans (Iterationen, instrumentierte Kopien) =="
L_FASSUNGEN="echt ohne-Zaehler Luegner-hoch Luegner-runter"
# `lf` und nicht `f`: `harness_bauen` benutzt `f` als Laufvariable OHNE `local` -- der Name
# waere nach dem ersten Bauen ueberschrieben, und die Zeile darunter meldete eine Fassung,
# die es nicht gibt. (Eine Beschriftung, die neben der Sache herlaeuft.)
for lf in $L_FASSUNGEN; do
    case "$lf" in
        echt)           leistung_fassung "$lf" ;;
        ohne-Zaehler)   leistung_fassung "$lf" "$OHNE_ZAEHLER" ;;
        Luegner-hoch)   leistung_fassung "$lf" "$LUEGNER_HOCH" ;;
        Luegner-runter) leistung_fassung "$lf" "$LUEGNER_RUNTER" ;;
    esac
    rc=$?
    [ "$rc" = 2 ] && exit 2
    case "$rc" in
        0) sp="BESTANDEN" ;;
        3) sp="ABGEBROCHEN (die Subtraktion selbst erkennt es)" ;;
        *) sp="DURCHGEFALLEN" ;;
    esac
    echo "  L-$lf gebaut und gefahren -- Sprechprobe: $sp"
done
echo
L_SCHLUESSEL="
L0.klein.ruhe_aussen
L0.gross.ruhe_aussen
L0.gross.ruhe_innen
L1.klein.aussen_je_wartetakt
L1.gross.aussen_je_wartetakt
L1.klein.refilltakt_aussen
L1.gross.refilltakt_aussen
L1.klein.refilltakt_innen
L1.gross.refilltakt_innen
L1.gross.donee_geweckt
L3a.refills_max_tick
L3a.aussen_max_tick
L3a.innen_max_tick
L3b.refills_max_tick
L3b.innen_max_tick
L3c.refills_max_tick
L3c.innen_max_tick
L3c.innen_max_je_konto
L3d.gewandert
L3d.refills_max_tick
L3d.innen_max_tick
L4.klein.zombie_scan
L4.gross.zombie_scan
L5.klein.setbudget_scan
L5.gross.setbudget_scan
L9.blockiert.echt
L9.blockiert.zaehler
L9.nach_refill.echt
L9.nach_refill.zaehler
L9.nach_refill.zaehler_luegt_um
L9.nach_refill.audit
L9.pause.zaehler_luegt_um
L9.pause.audit
L9.kill.zaehler_luegt_um
L9.kill.audit
L9.setbudget.zaehler_luegt_um
L9.setbudget.audit
"
printf '  %-38s' "Groesse (Kosten, Iterationen)"
for f in $L_FASSUNGEN; do printf ' %14s' "$f"; done
printf '\n'
for k in $L_SCHLUESSEL; do
    printf '  %-38s' "$k"
    for f in $L_FASSUNGEN; do
        v="$(wert "$WURZEL/L-$f/ausgabe.txt" "$k")"
        printf ' %14s' "${v:--}"
    done
    printf '\n'
done
echo
echo "  Lesart der L-Reihe:"
echo "   * L0 ist die ZUSICHERUNG aus \`on_tick\`: kein Konto erschoepft -> 0 Iterationen, und"
echo "     zwar bei 32 wie bei 10000 Slots. Steht dort etwas anderes als 0, faellt der Satz."
echo "   * L1 ist die SPRECHPROBE: dieselbe Groesse bei kleiner und bei grosser Tabelle. Sind"
echo "     die Zahlen gleich, misst die ganze Reihe nicht die Tabellengroesse."
echo "   * L3 beantwortet die offene Frage aus dem Todo-Eintrag: refillen mehrere Konten im"
echo "     SELBEN Tick? \`refills_max_tick\` ist die Antwort, \`innen_max_tick\` ihr Preis."
echo "     a = kein Donee, b = EIN Donee, c = je Konto einer (der schlechteste Fall)."
echo "   * L4/L5 sind die zwei kleineren Stellen. Sie liegen NICHT im Tick-Pfad."
echo "   * L9 haelt den Zaehler gegen die gezaehlte Tabelle. \`zaehler_luegt_um\` != 0 ist der"
echo "     Befund, den \`depleted_count\` am 2026-08-03 hatte und den niemand sehen konnte."
[ "$D10_OK" = 1 ] || exit 1
exit 0
