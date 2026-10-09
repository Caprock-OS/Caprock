//! Demo-Threads + Hot-Reload (Phase 4–7).
//!
//! Aufbau: ein **Endpoint** (stabile Cap-Identität), ein **Client**-PD (Send-Cap)
//! und ein **Server**-PD. Der Server v1 verdoppelt; nach einem **Hot-Reload**
//! (Phase 7) übernimmt Server v2 *denselben* Endpoint und verdreifacht — ohne
//! Kernel-Neustart und für den Client transparent (gleiche Send-Cap, gleicher
//! Endpoint). Drei Worker-Threads belegen weiterhin die Preemption.
//!
//! Der Reload-Manager ist der Idle-Thread des Primärkerns: Quiesce (Empfänger
//! zurückziehen + Recv-Cap entziehen) → Swap (v2 starten + Recv-Cap delegieren).

use crate::loader;
use crate::system;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use caprock_abi::{pdctl, result, sys, GRANT_FLAG, GRANT_RECV_SLOT};
use caprock_cap::{DmaCoherence, DmaDir};
use caprock_hal::{self as hal, println, syscall::invoke};
use caprock_loader::LoaderError;
use caprock_mem::{peek_u64, poke_u64, Rights};
use caprock_microkit::Domain;
use caprock_sched::ThreadId;
use caprock_sync::SpinLock;

// In-Kernel-Fuzzer (ADR 0013): nur mit Feature `kernel-fuzz` einkompiliert (eigenes Kindmodul
// `threads/fuzz.rs`). Im Release-Build (ohne Feature) stellt ein Stub-Modul dieselbe API als No-Op
// bereit -- die Ketten-Gates geben ihren VORGAENGER durch, sodass die Idle-Manager-Kette ohne die
// Fuzzer transparent durchfliesst. Die Audits bleiben IMMER im Kernel (nicht hier).
#[cfg(feature = "kernel-fuzz")]
mod fuzz;
#[cfg(not(feature = "kernel-fuzz"))]
mod fuzz {
    pub(super) fn drive() {}
    pub(super) fn report() {}
    pub(super) fn all_passed() -> bool {
        true
    }
    pub(super) fn loaderfuzz_gate(pred_loadstop: bool) -> bool {
        pred_loadstop
    }
    pub(super) fn fuzzers_gate(pred_cross: bool) -> bool {
        pred_cross
    }
    pub(super) fn dbg_fuzz() -> bool {
        true
    }
    pub(super) fn dbg_ipcfuzz() -> bool {
        true
    }
    pub(super) fn dbg_hwfuzz() -> bool {
        true
    }
    pub(super) fn dbg_loaderfuzz() -> bool {
        true
    }
}

// Continuous-Soak-Treiber (Burn-in #2): nur mit Feature `soak`. Reiner Harness-/Testcode (Kernel-Kern
// unveraendert); nach dem Selbsttest faehrt `soak::run()` statt `system_off` eine Endlosschleife.
#[cfg(feature = "soak")]
mod soak;

/// Compile-Zeit-Obergrenze für die Demo-Telemetrie-Arrays (die **tatsächliche** Kernzahl
/// liefert `system::num_cores()`; die Demos laufen darüber).
const NUM_CORES: usize = caprock_sched::MAX_CORES;
const NWORKERS: usize = 3;
const THRESHOLD: u64 = 3;
/// Lazy-FP-Test: zwei **echte EL0-User-Threads** halten je ein eindeutiges Muster
/// in FP-Registern (d0..d3) und geben per `YIELD` gegenseitig ab. Der Kernel ist
/// soft-float und FP trappt nur an EL0 -> der Lazy-Owner-Wechsel (FP-Trap EC 0x07)
/// muss das Muster über jede Abgabe hinweg korrekt sichern/wiederherstellen, sonst
/// erkennt der Thread eine Korruption und meldet KEINEN Erfolg.
const FP_WORKERS: usize = 2;
const FP_CHECK_ITERS: u64 = 200; // Iterationen je Thread, jede mit YIELD (Owner-Wechsel)
const FP_PRIO: u8 = 5; // eigene, höchste Demo-Priorität -> striktes A<->B-Ping-Pong
const FP_PATTERN: [u64; FP_WORKERS] = [0xA5A5_5A5A_3C3C_C3C3, 0x1234_5678_9ABC_DEF0];
const FP_ALL_OK: u64 = (1 << FP_WORKERS) - 1;
/// Prioritätstest: drei Threads mit Prioritäten 4 > 3 > 2 (alle über der
/// Standardpriorität 1) führen Arbeit aus und parken sich danach. Der höher
/// priorisierte muss zuerst fertig werden.
const NPRIO_TEST: usize = 3;
const PRIO_WORK: u64 = 300_000;
/// Lokaler Cap-Index des Endpoints im jeweiligen PD-Cspace.
const EP_CAP: u64 = 0;

const B1_IN: [u64; 2] = [5, 6]; // Server v1: verdoppelt -> [10, 12]
const B2_IN: [u64; 2] = [5, 6]; // Server v2: verdreifacht -> [15, 18]

static WORKER_COUNTS: [AtomicU64; NWORKERS] = [const { AtomicU64::new(0) }; NWORKERS];
/// Erfolgs-Maske der EL0-FP-Threads (Bit i = Thread i meldete unbeschädigtes Muster).
static FP_OK_MASK: AtomicU64 = AtomicU64::new(0);
static FP_COLLECTOR_DONE: AtomicBool = AtomicBool::new(false);
static PRIO_SEQ: AtomicU64 = AtomicU64::new(0);
static PRIO_FINISH: [AtomicU64; NPRIO_TEST] = [const { AtomicU64::new(0) }; NPRIO_TEST];
static PRIO_DONE: [AtomicBool; NPRIO_TEST] = [const { AtomicBool::new(false) }; NPRIO_TEST];

// Thread-Lebenszyklus (Ausbaustufe 2).
static VICTIM_COUNT: AtomicU64 = AtomicU64::new(0);
static KILL_SNAP1: AtomicU64 = AtomicU64::new(0); // Victim-Zähler direkt nach KILL
static KILL_SNAP2: AtomicU64 = AtomicU64::new(0); // ... etwas später (muss gleich sein)
static DENIED_KILL: AtomicU64 = AtomicU64::new(0); // KILL ohne Cap (muss != OK sein)
static KILLER_DONE: AtomicBool = AtomicBool::new(false);
static REAPED: AtomicU64 = AtomicU64::new(0); // vom Manager eingesammelte Threads
/// Lokaler Cap-Slot des Killers ohne Cap (Negativtest).
const NO_CAP_SLOT: u64 = 5;

// Notifications (asynchrone Signale).
const NOTIF_BADGE_VAL: u64 = 0xA5;
const NOTIF_ROUNDS: u64 = 5;
static NOTIF_GOT_BADGE: AtomicU64 = AtomicU64::new(0);
static NOTIF_COUNT: AtomicU64 = AtomicU64::new(0);
static PRODUCER_DONE: AtomicBool = AtomicBool::new(false);
static CONSUMER_DONE: AtomicBool = AtomicBool::new(false);

// Capability-Transfer in IPC (Broker delegiert dem Client eine Service-Cap).
static XFER_RESULT: AtomicU64 = AtomicU64::new(0);
static XFER_DONE: AtomicBool = AtomicBool::new(false);

// --- Grant-Leak-Regression (ext-29) --------------------------------------------------------
//
// Ein REPLY-Grant legt eine ABLEITUNG der Server-Cap an und traegt sie im Empfangs-Slot des
// Aufrufers ein. Lag dort schon eine Cap, wurde sie frueher nur ueberschrieben: die alte blieb
// als Kind im CDT und als belegter Slot in der GETEILTEN globalen Tabelle zurueck, fuer
// niemanden mehr erreichbar. Ein Server, der wiederholt mit Grant antwortet, konnte so die
// Tabelle erschoepfen und ALLEN PDs jede weitere Cap-Installation verwehren (Cross-PD-DoS).
//
// Die Messgroesse ist bewusst der **Kindzaehler der Quell-Cap** und nicht die globale
// Slot-Zahl: er zaehlt genau die Ableitungen DIESER Cap und ist damit immun gegen die
// parallel laufenden uebrigen Demos (die staendig Caps anlegen/loeschen). Erwartung nach
// `XFER_GRANT_LOOPS` zusaetzlichen Grants: **genau 1** lebende Ableitung (die aktuelle).
const XFER_GRANT_LOOPS: usize = 64;
/// Quell-Cap des Grants (Slot 2 des Brokers) — fuer die Kindzaehler-Messung.
static XFER_SRC_CAP: SpinLock<Option<caprock_cap::CapPtr>> = SpinLock::new(None);
/// Beobachtete Ableitungen der Quell-Cap nach den Wiederhol-Grants (Soll: 1).
static XFER_SRC_CHILDREN: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Ergebnis des Cap-Aufrufs NACH den Wiederhol-Grants (die Cap muss weiter funktionieren).
static XFER_RESULT_AFTER: AtomicU64 = AtomicU64::new(0);
static XFER_GRANTLK_DONE: AtomicBool = AtomicBool::new(false);

// EL0-Userland: ein echter EL0-User-Thread ruft per Syscall einen EL1-Server.
const USER_MAGIC: u64 = 0xC0DE;
static USER_RECV: AtomicU64 = AtomicU64::new(0);

// Reclaim-Test: transiente EL0-Exiter erzeugen, mehr als der Kernel-Stack-Pool (8)
// gleichzeitig fasst — gelingt nur, wenn beim Thread-Ende der Pool-Slot zurückkommt.
// Churn-/Leak-Test (Härtung Ziel 2): tausende spawn/destroy-Zyklen isolierter PDs;
// danach müssen MEM/TCB/ASID/kstack-Stände exakt zur Baseline zurückkehren.
const CHURN_TARGET: u32 = 2000;
static CHURN_DONE: AtomicBool = AtomicBool::new(false);
static CHURN_OK: AtomicBool = AtomicBool::new(false);

// MCS Scheduling Contexts (Härtung Ziel 3): budget-basiertes Scheduling wie seL4-MCS.
// Auf einem eigenen Kern konkurrieren zwei gleichprioritäre Threads: einer mit knappem
// CPU-Budget (über eine SchedContext-Cap gebunden), einer unbeschränkt (Greedy). Der
// budgetierte Thread wird bei Budgetende deplaniert und nach der Periode aufgefüllt ->
// er macht garantierte, aber stark begrenzte Fortschritte gegenüber dem Greedy-Thread.
const MCS_CORE: usize = 5; // eigener Kern: keine Konkurrenz mit den Demos auf core 0
const MCS_BUDGET: u32 = 2; // 2 Ticks CPU ...
const MCS_PERIOD: u32 = 16; // ... je 16 Ticks (knappe Zuteilung -> klar messbar)
const MCS_PRIO: u8 = 3; // über den Sekundärkern-Workern (IDLE_PRIO=1)
const MCS_SETTLE_TICKS: u64 = 150; // so viele core-MCS_CORE-Ticks konkurrieren lassen
const MCS_MIN_CYCLES: u64 = 3; // mind. so viele Erschöpfungs-/Refill-Zyklen beobachten
static MCS_BUDGETED_COUNT: AtomicU64 = AtomicU64::new(0);
static MCS_GREEDY_COUNT: AtomicU64 = AtomicU64::new(0);
static MCS_START_TICK: AtomicU64 = AtomicU64::new(0);
static MCS_STARTED: AtomicBool = AtomicBool::new(false);
static MCS_BOUND: AtomicBool = AtomicBool::new(false); // Budget per Cap gebunden?
static MCS_STOP: AtomicBool = AtomicBool::new(false); // Signal: beide Threads parken
static MCS_DONE: AtomicBool = AtomicBool::new(false);
static MCS_OK: AtomicBool = AtomicBool::new(false);

// Audit-Regression A (IPC): ein in einer Endpoint-Queue blockierter Thread, der
// zwischenzeitlich gekillt wird, darf beim nächsten RECV/CALL KEINE Kernel-Panik
// auslösen (Fund: `.expect("sender frame")` in `recv`/`call`). Ablauf (alles auf
// core 0, da `kill` kern-lokal ist): Opfer CALLt einen ungedienten Endpoint ->
// blockiert in `senders`; es wird gekillt (toter Eintrag bleibt in der Queue); ein
// Server RECVt -> MUSS den toten Eintrag überspringen statt zu paniken; ein lebender
// Client CALLt -> wird korrekt bedient (Beweis: Endpoint überlebt + funktioniert).
const STALE_MAGIC: u64 = 0x57A1_E000; // Eingabe des lebenden Clients (Antwort = 2x)
static STALE_VICTIM_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static STALE_SERVER_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static STALE_CLIENT_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static STALE_VICTIM_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static STALE_SERVED: AtomicU64 = AtomicU64::new(0); // Antwortwert beim Client
static STALE_STEP: AtomicU32 = AtomicU32::new(0);
static STALE_DONE: AtomicBool = AtomicBool::new(false);
static STALE_OK: AtomicBool = AtomicBool::new(false);

// Audit-Regression C (Reply-Liveness): stirbt der Server (Reply-Owner) NACH RECV, VOR
// REPLY, muss der blockierte CALL-Aufrufer mit ERR_SERVER_GONE entblockt werden (statt
// dauerhaft zu hängen). Server RECVt einmal + parkt (antwortet nie); Manager killt ihn;
// Client-CALL muss mit ERR_SERVER_GONE zurückkehren. (alles core 0, kill ist kern-lokal)
// Runde 2 (Reload/Quiesce-Pfad OHNE Thread-Tod): derselbe Aufbau, aber der Server wird
// per endpoint_quiesce_owner (wie im Hot-Reload) zurückgezogen statt gekillt — der
// Client muss ebenfalls ERR_SERVER_GONE sehen, und der Server bleibt am Leben.
const RGONE_MAGIC: u64 = 0x4711;
static RGONE_SERVER_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RGONE_CLIENT_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RGONE_EP_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static RGONE_SERVER_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static RGONE_RESULT: AtomicU64 = AtomicU64::new(u64::MAX); // Client-CALL-Ergebnis (kill-Runde)
static RGONE_Q_RESULT: AtomicU64 = AtomicU64::new(u64::MAX); // Client-CALL-Ergebnis (quiesce-Runde)
static RGONE_Q_SRV_ALIVE: AtomicBool = AtomicBool::new(false); // Server lebt nach quiesce?
static RGONE_STEP: AtomicU32 = AtomicU32::new(0);
static RGONE_DONE: AtomicBool = AtomicBool::new(false);
static RGONE_OK: AtomicBool = AtomicBool::new(false);

// Budget-Donation (MCS): ein budgetierter Client (Budget DDON_CBUDGET/DDON_CPERIOD) ruft
// intra-core einen unbeschränkten Server, der pro Call Arbeit über >=1 Tick verrichtet.
// Mit Donation wird diese Arbeit gegen das KONTO DES CLIENTS belastet -> der Client
// erschöpft sich je Call (Erschöpfungs-Delta >= DDON_MIN_DEPL). Ohne Donation liefe der
// Server auf seinem eigenen (unbeschränkten) Budget -> Client-Konto kaum belastet (~0).
const DDON_CBUDGET: u32 = 1;
const DDON_CPERIOD: u32 = 8;
const DDON_CALLS: u32 = 14; // so viele Calls fährt der Client, dann parkt er
const DDON_MIN_DEPL: u64 = 6; // erwartete Erschöpfungen des Client-Kontos (mit Donation)
static DDON_SERVER_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DDON_CLIENT_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DDON_CLIENT_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static DDON_DEPL0: AtomicU64 = AtomicU64::new(0); // Erschöpfungs-Baseline (core 0)
static DDON_DELTA: AtomicU64 = AtomicU64::new(u64::MAX); // beobachtetes Delta (Diagnose)
static DDON_CLIENT_DONE: AtomicBool = AtomicBool::new(false); // Client hat alle Calls durch
static DDON_STEP: AtomicU32 = AtomicU32::new(0);
static DDON_DONE: AtomicBool = AtomicBool::new(false);
static DDON_OK: AtomicBool = AtomicBool::new(false);

// First-class Reply-Cap (ObjectKind::Reply) + Revocation: ein Client-CALL etabliert
// einen ausstehenden Call; der Manager prägt eine Reply-Cap dafür (reply_cap_for) und
// LÖSCHT sie -> die Finalisierung bricht den Call ab, der Client kehrt mit
// ERR_SERVER_GONE zurück (Revocation einer Reply-Cap über das Capability-System).
static RCAP_SERVER_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RCAP_CLIENT_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RCAP_EP_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static RCAP_CLIENT_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static RCAP_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);
static RCAP_STEP: AtomicU32 = AtomicU32::new(0);
static RCAP_DONE: AtomicBool = AtomicBool::new(false);
static RCAP_OK: AtomicBool = AtomicBool::new(false);

// Reply-Cap-Server-Migration (Hot-Reload mit Call in Bearbeitung): Server v1 empfängt
// einen Client-CALL (wird Reply-Owner) und parkt OHNE zu antworten. Der Manager
// migriert die ausstehende Antwortpflicht (endpoint_migrate_owner) auf eine frische
// v2-Instanz und zieht v1 capless zurück (wie beim Hot-Reload). v2 empfängt DIESELBE
// Nachricht und antwortet -> der Client-CALL kehrt mit OK + dem von v2 berechneten Wert
// zurück (statt ERR_SERVER_GONE). Damit überlebt eine Reply-Cap einen Server-Wechsel.
const RMIG_INPUT: u64 = 7;
const RMIG_V2_FACTOR: u64 = 5; // v2 verfünffacht -> beweist, dass v2 (nicht v1) bediente
static RMIG_SERVER_PD: AtomicUsize = AtomicUsize::new(usize::MAX); // v1-PD
static RMIG_V2_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RMIG_CLIENT_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RMIG_EP_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static RMIG_V1_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static RMIG_RECEIVED: AtomicBool = AtomicBool::new(false); // v1 hat den CALL empfangen
static RMIG_MIGRATED: AtomicBool = AtomicBool::new(false); // Migration meldete Erfolg
static RMIG_QUIESCED: AtomicBool = AtomicBool::new(false); // A-4.2: Endpoint neu stillgelegt
static RMIG_ROLE_OK: AtomicBool = AtomicBool::new(false); // A-4.2: Rollenbefund am offenen Call
static RMIG_RESUMED: AtomicBool = AtomicBool::new(false); // A-4.2: wieder freigegeben
static RMIG_RESULT: AtomicU64 = AtomicU64::new(u64::MAX); // Client-CALL-Ergebniscode
static RMIG_VALUE: AtomicU64 = AtomicU64::new(u64::MAX); // Client-CALL-Antwortwert
static RMIG_STEP: AtomicU32 = AtomicU32::new(0);
static RMIG_DONE: AtomicBool = AtomicBool::new(false);
static RMIG_OK: AtomicBool = AtomicBool::new(false);

// CAPS-Read-Concurrency (Verifikation des Reader-Writer-Locks für #3): zwei Sonden auf
// zwei Kernen halten GLEICHZEITIG den CAPS-Read-Lock (synchronisiert über eine Barriere
// im kritischen Abschnitt). Mit dem alten exklusiven SpinLock strukturell unmöglich.
// Beweist, dass die heißen Cap-Lookups jetzt nebenläufig (statt serialisiert) laufen.
const CAPLK_CORE_A: usize = 1;
const CAPLK_CORE_B: usize = 2;
const CAPLK_WANT: u32 = 2; // zwei gleichzeitige Leser
const CAPLK_SPIN_LIMIT: u32 = 50_000_000; // bricht im Normalfall früh ab (Barriere erfüllt)
static CAPLK_A_OK: AtomicBool = AtomicBool::new(false);
static CAPLK_B_OK: AtomicBool = AtomicBool::new(false);
static CAPLK_A_DONE: AtomicBool = AtomicBool::new(false);
static CAPLK_B_DONE: AtomicBool = AtomicBool::new(false);
static CAPLK_STEP: AtomicU32 = AtomicU32::new(0);
static CAPLK_DONE: AtomicBool = AtomicBool::new(false);
static CAPLK_OK: AtomicBool = AtomicBool::new(false);

// Sicherheitsdomänen (ext-22, P1): je eine PD pro Domäne. TrustedSas an einen globalen
// EL1-Thread gebunden, HardwareLand + UserLand an isolierte EL0-Threads (eigene VSpace).
// `domain_audit()` muss 0 melden (Cap-Typ-Policy + untrusted-Domänen sind isoliert) und die
// Domänen müssen round-trippen. Fixtures in spawn_demo (Teil der Ressourcen-Baseline).
static DOMAIN_TRUSTED_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DOMAIN_HW_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DOMAIN_USER_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DOMAIN_AUDIT_CODE: AtomicU32 = AtomicU32::new(u32::MAX);
static DOMAIN_HW_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static DOMAIN_USER_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static DOMAIN_DONE: AtomicBool = AtomicBool::new(false);
static DOMAIN_OK: AtomicBool = AtomicBool::new(false);

// UserLand-Management (ext-22, P2): ein TrustedSas-Controller steuert per SYS_PDCTL (cap-
// gated PdControl) den Lifecycle eines UserLand-Ziels. Ziel = isolierter EL0-Thread, der
// einen geteilten Frame inkrementiert; der Controller (TrustedSas EL1) beobachtet den
// Zaehler ueber peek_u64 und prueft PAUSE(eingefroren)/RESUME(waechst)/STOP + cap-Negativ.
const PDCTL_FRAME_SIZE: u64 = 0x20_0000; // 2 MiB (MAP-2MiB-Fastpath wie shm)
const PDCTL_CTRL_SLOT: u64 = 0; // PdControl-Cap im Controller-Cspace
const PDCTL_EMPTY_SLOT: u64 = 5; // leerer Slot (Negativtest: kein Cap -> ERR_BADCAP)
const PDCTL_YIELD_OBS: u32 = 24; // Beobachtungsfenster in YIELDs (kooperativ; knapp gehalten)
static PDCTL_FRAME: AtomicU64 = AtomicU64::new(u64::MAX); // phys. Basis des Zaehler-Frames
static PDCTL_TARGET_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static PDCTL_CTRL_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static PDCTL_TARGET_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static PDCTL_RAN: AtomicBool = AtomicBool::new(false); // Ziel lief (Zaehler wuchs)
static PDCTL_FROZE: AtomicBool = AtomicBool::new(false); // PAUSE -> eingefroren + OK
static PDCTL_RESUMED: AtomicBool = AtomicBool::new(false); // RESUME -> waechst wieder
static PDCTL_NOCAP: AtomicBool = AtomicBool::new(false); // leerer Slot -> ERR_BADCAP
static PDCTL_POLICY: AtomicBool = AtomicBool::new(false); // PdControl in UserLand-PD -> denied
static PDCTL_STOPPED: AtomicBool = AtomicBool::new(false); // STOP -> OK
static PDCTL_CTRL_DONE: AtomicBool = AtomicBool::new(false); // Controller-Sequenz fertig
static PDCTL_STEP: AtomicU32 = AtomicU32::new(0);
static PDCTL_DONE: AtomicBool = AtomicBool::new(false);
static PDCTL_OK: AtomicBool = AtomicBool::new(false);

// Paarweiser Treiber<->Backend-Kanal (ext-22, P3): eine TrustedSas-Zeitdienst-PD + ein
// HardwareLand-Backend (isolierter EL0-Stub), gebunden via create_hardware_backend
// (unveränderlich, 1:N). Der Trusted-Client CALLt das Backend über den gebundenen Kanal;
// das Backend antwortet (verdoppelt). Plus: 2. Backend am selben Partner (1:N), und der
// Versuch, eine FREMDE Endpoint-Cap ins Backend zu legen, wird abgelehnt.
const CHAN_INPUT: u64 = 9;
const CHAN_FACTOR: u64 = 2; // Backend verdoppelt -> Beleg, dass das Backend bediente
static CHAN_TS_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static CHAN_BE_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
#[allow(dead_code)] // bewusst behalten (HW-Register/API-Vollstaendigkeit bzw. nur unter cfg(kani)/Feature genutzt)
static CHAN_BE2_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static CHAN_RESULT: AtomicU64 = AtomicU64::new(u64::MAX); // Trusted-Client CALL-Antwortwert
static CHAN_1N_OK: AtomicBool = AtomicBool::new(false); // 2. Backend am selben Partner ok
static CHAN_FOREIGN_DENIED: AtomicBool = AtomicBool::new(false); // Fremd-Cap ins Backend denied
static CHAN_BE_RECV_OK: AtomicBool = AtomicBool::new(false); // Kanal-Recv-Cap ins Backend ok
static CHAN_CLIENT_DONE: AtomicBool = AtomicBool::new(false);
static CHAN_STEP: AtomicU32 = AtomicU32::new(0);
static CHAN_DONE: AtomicBool = AtomicBool::new(false);
static CHAN_OK: AtomicBool = AtomicBool::new(false);

// RTC-Referenz-Backend (ext-22, P4): erstes echtes HardwareLand-Backend über die
// GENERISCHE Device-Infrastruktur. Eine MMIO-Cap fuer das PL031-RTC wird in das
// HardwareLand-Backend installiert; der Kernel mappt die Registerseite EL0-RO in dessen
// isolierte VSpace (vspace_map_device). Der Trusted-Zeitdienst CALLt das Backend; das
// Backend liest RTC_DR (echtes Geraet) und liefert den Wert ueber den paarweisen Kanal.
const RTC_PHYS: u64 = 0x0901_0000; // PL031 RTC (QEMU virt), RTC_DR an Offset 0
const RTC_LEN: u64 = 0x1000;
static RTC_TS_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RTC_BE_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static RTC_VALUE: AtomicU64 = AtomicU64::new(u64::MAX); // gelesener RTC_DR-Wert
static RTC_RES_CODE: AtomicU64 = AtomicU64::new(u64::MAX); // Zeitdienst-CALL-Ergebniscode
static RTC_MMIO_OK: AtomicBool = AtomicBool::new(false); // MMIO-Cap ins HardwareLand-Backend ok
static RTC_POLICY: AtomicBool = AtomicBool::new(false); // MMIO-Cap in UserLand-PD -> denied
static RTC_CLIENT_DONE: AtomicBool = AtomicBool::new(false);
static RTC_STEP: AtomicU32 = AtomicU32::new(0);
static RTC_DONE: AtomicBool = AtomicBool::new(false);
static RTC_OK: AtomicBool = AtomicBool::new(false);

// RTC-IRQ (ext-22, P5): das HardwareLand-Backend armiert den PL031-Match-Interrupt (schreibt
// RTC_MR/RTC_IMSC ueber die RW-gemappte Device-Seite) und WAITet auf seine Kanal-Notification.
// Der Kernel routet SPI 34 (RTC) an Kern 0, faengt ihn ab (deferred), maskiert ihn und stellt
// ihn als Notification-Badge zu -> Backend wacht auf und meldet "IRQ erhalten" an den Partner.
const RTC_INTID: u32 = 34; // PL031 RTC = SPI 2 -> GIC-INTID 34 (QEMU virt)
const IRQT_BADGE: u64 = 0x10;
static IRQT_TS_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static IRQT_BE_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static IRQT_GOT_VALUE: AtomicU64 = AtomicU64::new(u64::MAX); // Partner-CALL-Antwort (1 = IRQ erhalten)
static IRQT_RES_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
static IRQT_MMIO_OK: AtomicBool = AtomicBool::new(false);
static IRQT_CAP_OK: AtomicBool = AtomicBool::new(false); // IRQ-Cap ins HardwareLand-Backend ok
static IRQT_POLICY: AtomicBool = AtomicBool::new(false); // IRQ-Cap in UserLand -> denied
static IRQT_CLIENT_DONE: AtomicBool = AtomicBool::new(false);
static IRQT_STEP: AtomicU32 = AtomicU32::new(0);
static IRQT_DONE: AtomicBool = AtomicBool::new(false);
static IRQT_OK: AtomicBool = AtomicBool::new(false);

// DMA-Capability (ext-23, D0): erstes DMA-Backend über die GENERISCHE DmaCap-Schicht hinter der
// DmaEnforcer-Abstraktion. Der Kernel schneidet eine kontiguierliche RAM-DMA-Region aus,
// installiert eine DmaCap ins HardwareLand-Backend und mappt sie EL0-RW Normal-Non-Cacheable in
// dessen isolierte VSpace. Das Backend (EL0) schreibt ein Muster über die NC-Abbildung und liest
// es zurueck (Round-Trip in EL0); der Kernel prueft via Identity-Map, dass er dieselben Bytes
// sieht (Kohaerenz). Negativ: DmaCap in UserLand -> abgelehnt. Audits (dma/domain/vspace) == 0.
const DMA_LEN: u64 = 0x4000; // 16 KiB DMA-Puffer
const DMA_PAT0: u32 = 0x600D_BEEF; // vom Backend an Offset 0 geschrieben
const DMA_PAT1: u32 = 0xD0DA_D0DA; // vom Backend an Offset 4 geschrieben
static DMA_TS_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DMA_BE_PD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DMA_PHYS: AtomicU64 = AtomicU64::new(0); // ausgeschnittene DMA-Region-Basis
static DMA_VALUE: AtomicU64 = AtomicU64::new(u64::MAX); // EL0-Round-Trip-Read (Reply msg0)
static DMA_RES_CODE: AtomicU64 = AtomicU64::new(u64::MAX);
static DMA_CAP_OK: AtomicBool = AtomicBool::new(false); // DmaCap ins HardwareLand-Backend ok
static DMA_POLICY: AtomicBool = AtomicBool::new(false); // DmaCap in UserLand -> denied
static DMA_SENS_OK: AtomicBool = AtomicBool::new(false); // Bounds-Sensitivität: dma_audit greift
static DMA_CLIENT_DONE: AtomicBool = AtomicBool::new(false);
static DMA_STEP: AtomicU32 = AtomicU32::new(0);
static DMA_DONE: AtomicBool = AtomicBool::new(false);
static DMA_OK: AtomicBool = AtomicBool::new(false);

// PCIe-Enumeration (ext-23, D1): das DMA-Beweisgerät virtio-rng-pci ueber ECAM finden,
// BAR zuweisen, Memory-Space + Bus-Master aktivieren, RID (= SMMU-StreamID) bestimmen. Reines
// kernel-/Trusted-Setup (kein User-Pfad). Die SMMU uebersetzt nur PCIe -> das Geraet MUSS
// virtio-rng-PCI sein. Negativ: Suche nach einem Bogus-Vendor liefert nichts.
static PCIE_VENDOR: AtomicU32 = AtomicU32::new(0);
static PCIE_DEVICE: AtomicU32 = AtomicU32::new(0);
static PCIE_RID: AtomicU32 = AtomicU32::new(u32::MAX);
static PCIE_BAR: AtomicU64 = AtomicU64::new(0);
static PCIE_BM: AtomicBool = AtomicBool::new(false); // Bus-Master aktiviert
static PCIE_NEG: AtomicBool = AtomicBool::new(false); // Bogus-Vendor -> nicht gefunden
static PCIE_DONE: AtomicBool = AtomicBool::new(false);
static PCIE_OK: AtomicBool = AtomicBool::new(false);

// SMMUv3-Bring-up (ext-23, D2): den DmaEnforcer (SmmuV3Enforcer) initialisieren — Command-/
// Event-Queue + lineare Stream-Tabelle (genullt = Default-Abort) anlegen, CR0 (CMDQEN|EVENTQEN,
// dann SMMUEN) aktivieren, CMD_SYNC-Round-Trip als Spike. Danach: Uebersetzung aktiv (CR0ACK),
// Event-Queue leer, keine globalen Fehler (GERROR).
static SMMU_IDR0: AtomicU32 = AtomicU32::new(0);
static SMMU_SID: AtomicU32 = AtomicU32::new(0);
static SMMU_GERR: AtomicU32 = AtomicU32::new(u32::MAX);
static SMMU_SYNC: AtomicBool = AtomicBool::new(false); // CMD_SYNC-Round-Trip gelang

// --- Die arch-neutrale IOMMU-Gesundheit (2026-08-17) -----------------------------------------
// Beim Bring-up erfasst, im Bericht gedruckt UND im Gatter gelesen -- aus DENSELBEN Zellen.
// Ein Urteil, das im Bericht neu entsteht, kann den Bericht nicht ausloesen (Fallenliste).
// Vorbelegung `false`/`u32::MAX`: nie gesetzt darf nicht wie bestanden aussehen.
static IOHEALTH_OK: AtomicBool = AtomicBool::new(false);
static IOHEALTH_SPEAKING: AtomicBool = AtomicBool::new(false);
static IOHEALTH_PRESENT: AtomicBool = AtomicBool::new(false);
static IOHEALTH_XLAT: AtomicBool = AtomicBool::new(false);
static IOHEALTH_FAULTS_EMPTY: AtomicBool = AtomicBool::new(false);
static IOHEALTH_UNITS: AtomicU32 = AtomicU32::new(0);
static IOHEALTH_SPEAK_N: AtomicU32 = AtomicU32::new(0);
static IOHEALTH_HWERR: AtomicU32 = AtomicU32::new(u32::MAX);
static IOHEALTH_CFGERR: AtomicU32 = AtomicU32::new(u32::MAX);
static SMMU_EN: AtomicBool = AtomicBool::new(false); // CR0ACK.SMMUEN
static SMMU_EVTQ: AtomicBool = AtomicBool::new(false); // Event-Queue leer
static SMMU_DONE: AtomicBool = AtomicBool::new(false);
static SMMU_OK: AtomicBool = AtomicBool::new(false);

// SMMU-Bindung (ext-23, D3): enable_dma/disable_dma — eine STE -> CD -> Stage-1-Tabelle fuer die
// StreamID der virtio-RNG installieren (bildet NUR die DMA-Region ab), dann wieder entziehen.
// Strukturell: CFGI_STE/TLBI/SYNC laufen durch, Event-Queue bleibt leer, GERROR=0; die
// Stage-1-/CD-Frames werden balanciert wieder freigegeben (total_free zurueck zur Baseline).
// Die funktionale Uebersetzung + Out-of-Window-Fault wird in D4 bewiesen.
static SMMUB_RID: AtomicU32 = AtomicU32::new(u32::MAX);
static SMMUB_ENABLE: AtomicBool = AtomicBool::new(false); // enable_dma erfolgreich
static SMMUB_EVTQ: AtomicBool = AtomicBool::new(false); // Event-Queue nach enable leer
static SMMUB_BALANCED: AtomicBool = AtomicBool::new(false); // total_free nach disable = Baseline
static SMMUB_DONE: AtomicBool = AtomicBool::new(false);
static SMMUB_OK: AtomicBool = AtomicBool::new(false);

// virtio-rng-DMA (ext-23, D4): echter Bus-Master-DMA in die DmaCap-Region. Das Gerät DMAt
// Zufallsbytes in die Region (zur StreamID gebundene SMMU-Stage-1, Level 2). ZWEISTUFIGER
// KRONJUWEL: Level 1 (Software) — der Treiber validiert jede Deskriptor-Adresse gegen die
// DmaCap; eine Out-of-Window-Adresse wird ABGEWIESEN (demonstrierbar in QEMU). Sensitivitaet:
// ohne die Pruefung schreibt das Geraet das Ziel (Pruefung lasttragend); auf realer HW faultet
// dann die SMMU (Level 2), unter QEMU fuer emulierte Geraete nicht beobachtbar.
static VRNG_USED: AtomicBool = AtomicBool::new(false); // used-Ring fortgeschritten
static VRNG_WRITTEN: AtomicU32 = AtomicU32::new(0); // gemeldete Byte-Zahl
static VRNG_R0: AtomicU32 = AtomicU32::new(0); // erste Zufallsbytes
static VRNG_R1: AtomicU32 = AtomicU32::new(0);
static VRNG_EVTQ: AtomicBool = AtomicBool::new(false); // SMMU-Event-Queue nach In-Window leer
static VRNG_CJ_SW: AtomicBool = AtomicBool::new(false); // L1: Out-of-Window software-abgewiesen
static VRNG_CJ_SENT: AtomicBool = AtomicBool::new(false); // L1 aktiv: Ziel unveraendert
static VRNG_CJ_UNGUARDED: AtomicBool = AtomicBool::new(false); // Sensitivitaet: Pruefung lasttragend
static VRNG_SMMU_ENF: AtomicBool = AtomicBool::new(false); // L2: SMMU-Fault
static VRNG_EVT_XLAT: AtomicBool = AtomicBool::new(false); // ... F_TRANSLATION
static VRNG_EVT_SID: AtomicBool = AtomicBool::new(false); //  ... erwartete StreamID
static VRNG_EVT_IN: AtomicBool = AtomicBool::new(false); //   ... erwartete Input-Adresse
static VRNG_AXES: AtomicBool = AtomicBool::new(false); //     ext-36b: IOVA != PA
static VRNG_BYPASS: AtomicBool = AtomicBool::new(false); //   Sensitivitaet: Bypass-STE -> Geraet schreibt
static VRNG_LIVE: AtomicBool = AtomicBool::new(false); //     Event-Queue hat nachweislich gesprochen
static VRNG_CFGERR0: AtomicBool = AtomicBool::new(false); //  keine C_BAD_STE/C_BAD_CD im Lauf
static VRNG_DONE: AtomicBool = AtomicBool::new(false);
static VRNG_OK: AtomicBool = AtomicBool::new(false);

// Generische DMA-Infrastruktur (ext-24): Richtung/Kohaerenz (DmaCap-Attribute -> richtungs-
// minimale SMMU-AP + kohaerenz-spezifische Attribute, strukturell geprueft), Multi-Region-
// Kontext (mehrere DmaCaps je StreamID in EINER Stage-1-Tabelle), Stream-Gruppen (mehrere
// StreamIDs je Kontext), Scatter-Gather-Validierung (Level 1), disjunkte Sub-Puffer (SG-Pfad).
static DMAGEN_MULTIREGION: AtomicBool = AtomicBool::new(false); // 3 Regionen in 1 Kontext
static DMAGEN_DIR: AtomicBool = AtomicBool::new(false); // DeviceRead-Leaf RO, DeviceWrite-Leaf RW
static DMAGEN_COH: AtomicBool = AtomicBool::new(false); // Coherent-Leaf cacheable, NonCoherent NC
static DMAGEN_GROUP: AtomicBool = AtomicBool::new(false); // 2 StreamIDs teilen den Kontext
static DMAGEN_SG: AtomicBool = AtomicBool::new(false); // SG-Liste valide + Out-of-Window abgewiesen
static DMAGEN_POOL: AtomicBool = AtomicBool::new(false); // disjunkte DMA-Sub-Puffer (SG-Pfad) + Erschoepfung
static DMAGEN_BALANCED: AtomicBool = AtomicBool::new(false); // total_free nach Teardown = Baseline
static DMAGEN_DONE: AtomicBool = AtomicBool::new(false);
// IOVA-Fenstergrenzen (ext-36b Nacharbeit): Erschoepfung + Geraete-Adressbreite als GEPRUEFTE
// Eigenschaften. Der Bump gibt nie zurueck, also ist die Obergrenze real -- sie darf nur kein
// Betriebszustand sein, und wenn sie erreicht wird, muss `attach` sauber scheitern statt eine
// Adresse zu vergeben, die das Geraet nicht absetzen kann.
static DMAWIN_DONE: AtomicBool = AtomicBool::new(false);
static DMAWIN_OK: AtomicBool = AtomicBool::new(false);
/// Cache-Partitionierung (todo A1) und Streifenvergabe (B-4.2) — bis 2026-08-01 liefen beide
/// **nur** auf x86, weil ihr einziger Aufrufer in `arch/x86_64/bringup.rs` stand. `hal::cache` hat
/// eine aarch64-Fassung, die damit nie an einer echten Zuteilung geprueft wurde; genau diese
/// Fehlerform (Code auf dem zweiten Zweig, uebersetzt aber nie ausgefuehrt) hat das Projekt schon
/// dreimal bezahlt. Der Test selbst liegt arch-neutral in `crate::colors` — wie `crate::dmatests`.
static COLOR_DONE: AtomicBool = AtomicBool::new(false);
static COLOR_OK: AtomicBool = AtomicBool::new(false);
static STRIPE_ALLOC_OK: AtomicBool = AtomicBool::new(false);
/// B-4.5: Prime+Probe. `true` heisst auch "nicht messbar" (SKIP).
static PPROBE_OK: AtomicBool = AtomicBool::new(false);
static DMAWIN_NARROW: AtomicBool = AtomicBool::new(false); //   32-Bit-Geraet -> abgewiesen
static DMAWIN_EXHAUST: AtomicBool = AtomicBool::new(false); //  Fenster voll -> abgewiesen
static DMAWIN_INTACT: AtomicBool = AtomicBool::new(false); //   Kontext danach unveraendert nutzbar
static DMAWIN_BALANCED: AtomicBool = AtomicBool::new(false);
static DMAWIN_UNDECL: AtomicU32 = AtomicU32::new(0); //        Geraete ohne deklarierte Adressbreite
// B-3.4 (E-Rest 1). Vorgabe 1 = FAILURES bzw. 2 = "nicht entscheidbar": ein Wert, den niemand
// gesetzt hat, darf nicht als bestanden bzw. als "frei" durchgehen.
static DMAWIN_VERDICT: AtomicU32 = AtomicU32::new(1);
static DMAWIN_MSI_POLICY: AtomicU32 = AtomicU32::new(2);
static DMAWIN_MSI_LIVE: AtomicU32 = AtomicU32::new(0);
static DMAWIN_MSI_WEAK: AtomicU32 = AtomicU32::new(0);
static DMAWIN_MSI_DRIFT: AtomicU32 = AtomicU32::new(0);
static DMAWIN_MSI_OVL: AtomicU32 = AtomicU32::new(0);
                                                     // Teardown-Token (ext-37): `cap_delete` allein baut die Uebersetzung ab und gibt erst danach
                                                     // frei; ohne bestaetigte Stilllegung wird NIE freigegeben (Pending, beschraenkt, auditiert).
static DMATOK_DONE: AtomicBool = AtomicBool::new(false);
static DMATOK_OK: AtomicBool = AtomicBool::new(false);
static DMATOK_DELETE_TEARS: AtomicBool = AtomicBool::new(false); // delete ohne detach baut ab
static DMATOK_FREED: AtomicBool = AtomicBool::new(false); //        ... und gibt die Region frei
static DMATOK_PENDING: AtomicBool = AtomicBool::new(false); //      unbestaetigt -> nicht frei
static DMATOK_AUDIT7: AtomicBool = AtomicBool::new(false); //       Code 7 haelt im Pending-Zustand
static DMAGEN_OK: AtomicBool = AtomicBool::new(false);

// Prozess-Heap (ext-25): ein Trusted-SAS-Thread (safe Rust) nutzt einen prozess-lokalen
// Heap (caprock_region::heap::Heap ueber KernelRegionSource) fuer ECHTE Box/Vec/BTreeMap.
// Der Allokator (Groessenklassen-Slabs + Bump-Arenen) fordert Regionen ueber den RegionSource
// an (grow) und gibt sie zurueck (shrink/Drop). Alle Adressen sind reale Physadressen (SAS);
// das einzige unsafe liegt in der Region-Runtime, der Testcode ist 100% safe.
static SASHEAP_VEC: AtomicBool = AtomicBool::new(false); // Vec waechst ueber Klassen + Region
static SASHEAP_BOX: AtomicBool = AtomicBool::new(false); // Box
static SASHEAP_MAP: AtomicBool = AtomicBool::new(false); // BTreeMap (Slab-Churn)
static SASHEAP_LARGE: AtomicBool = AtomicBool::new(false); // Large-Alloc -> dedizierte Region
static SASHEAP_GREW: AtomicBool = AtomicBool::new(false); // >=1 Region angefordert
static SASHEAP_BALANCED: AtomicBool = AtomicBool::new(false); // total_free nach Heap-Drop = Baseline
static SASHEAP_DONE: AtomicBool = AtomicBool::new(false);
static SASHEAP_OK: AtomicBool = AtomicBool::new(false);

// Binary-Loader (ext-26, L1): ein EXTERN gebautes Programm (`hello`) wird aus dem Boot-Archiv
// geladen + in einer frischen isolierten UserLand-PD gestartet. Beweis der Ausfuehrung: hello
// signalisiert die vom Loader endowte Notification mit `HELLO_BADGE`. Asynchron: laden, dann das
// Badge in spaeteren Manager-Iterationen pollen (hello laeuft dazwischen, vom Scheduler eingeplant).
const HELLO_BADGE: u64 = 0x4845_4C4F; // "HELO" (muss zu programs/hello uebereinstimmen)
static LOAD_STARTED: AtomicBool = AtomicBool::new(false);
static LOAD_DONE: AtomicBool = AtomicBool::new(false);
static LOAD_OK: AtomicBool = AtomicBool::new(false);
static LOAD_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static LOAD_POLLS: AtomicU32 = AtomicU32::new(0);

// Binary-Loader L2 (ext-26): Laden zur LAUFZEIT via SYS_LOAD, cap-gegatet. Ein TrustedSAS-Caller-
// Thread haelt eine Loader-Cap (Slot 0) + eine Notification-Cap (Slot 1, Badge HELLO_BADGE) und
// ruft invoke(SYS_LOAD, loader_slot, [hello_idx, delegate_slot, ..]) -> der Kernel laedt hello +
// delegiert die Notification-Cap in hellos Slot 0; hello signalisiert sie. Negativfall: SYS_LOAD
// ueber einen leeren Slot (keine Loader-Cap) -> ERR_BADCAP.
static SYSLOAD_STARTED: AtomicBool = AtomicBool::new(false);
static SYSLOAD_FIN: AtomicBool = AtomicBool::new(false);
static SYSLOAD_OK: AtomicBool = AtomicBool::new(false);
static SYSLOAD_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static SYSLOAD_POLLS: AtomicU32 = AtomicU32::new(0);
static SYSLOAD_DONE: AtomicBool = AtomicBool::new(false); // Caller-Thread hat seine Syscalls beendet
static SYSLOAD_RESULT: AtomicU64 = AtomicU64::new(u64::MAX); // SYS_LOAD-Ergebnis (OK erwartet)
static SYSLOAD_NEG_OK: AtomicBool = AtomicBool::new(false); // Negativfall: ERR_BADCAP erhalten

// Binary-Loader L3 (ext-26): HardwareLand-Programm laden (vor-erstellte Backend-PD mit Partner-
// Bindung + Kanal) + Signatur-/Trust-Gate. hwhello (= hello, Domaene HardwareLand) signalisiert
// ueber seine Kanal-Notification-Cap; TrustedSAS/EL1-Image wird vom verify_image-Gate abgelehnt.
static LOADHW_STARTED: AtomicBool = AtomicBool::new(false);
static LOADHW_FIN: AtomicBool = AtomicBool::new(false);
static LOADHW_OK: AtomicBool = AtomicBool::new(false);
static LOADHW_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static LOADHW_POLLS: AtomicU32 = AtomicU32::new(0);
static LOADTRUSTED_EL0: AtomicBool = AtomicBool::new(false); // TrustedSAS als EL0-isoliert geladen

// Binary-Loader L4 (ext-26): Teardown geladener Prozesse. Ein UserLand-Programm laden, dann
// VOLLSTAENDIG abbauen (Thread + VSpace-Tabellen + geladene Segment-Frames + Kernel-Stack + PD) und
// die Ressourcen-Baseline pruefen (MEM/VSpace/kstack zurueck = kein Leck). Beweist sauberen
// Lebenszyklus -- Voraussetzung fuer das Churnen geladener Prozesse (ext-27).
static LOADSTOP_DONE: AtomicBool = AtomicBool::new(false);
static LOADSTOP_OK: AtomicBool = AtomicBool::new(false);

// ext-27: adversariale EXTERNE Testdienste (geladen wie Drittsoftware, NICHT im Kernel-Image; ADR
// 0012). Jeder Dienst greift den Kernel ueber die Syscall-ABI an und signalisiert sein SUCCESS-Badge
// GENAU DANN, wenn ALLE Angriffe korrekt abgewiesen wurden ("der Dienst ist sein eigener Richter").
// AGGRU = UserLand-Aggressor: Cap-Confusion (leerer Slot/falscher Typ/falsche Rechte) + Autoritaets-
// Eskalation (PDCTL/LOAD/KILL ohne gating-Cap) -> BADCAP/RIGHTS/BADSYS.
static AGGRU_STARTED: AtomicBool = AtomicBool::new(false);
static AGGRU_DONE: AtomicBool = AtomicBool::new(false);
static AGGRU_OK: AtomicBool = AtomicBool::new(false);
static AGGRU_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static AGGRU_POLLS: AtomicU32 = AtomicU32::new(0);
/// Erfolgs-Badge "AGRU" — aggressor-u signalisiert es nur bei vollstaendig abgewiesener Batterie.
const AGGRU_SUCCESS: u64 = 0x4147_5255;

// ext-27 T1: INTRU = UserLand-Intruder (Speicher-Isolation). Liest aus EL0 Kernel-RAM -> Fault ->
// Thread terminiert, Kernel laeuft weiter. Der Dienst meldet PRE VOR dem fatalen Zugriff; der Test
// beobachtet PRE + el0_fault_count++ (relativ zur Baseline beim Start) + Audits sauber.
static INTRU_STARTED: AtomicBool = AtomicBool::new(false);
static INTRU_DONE: AtomicBool = AtomicBool::new(false);
static INTRU_OK: AtomicBool = AtomicBool::new(false);
static INTRU_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static INTRU_POLLS: AtomicU32 = AtomicU32::new(0);
static INTRU_FAULT_BASE: AtomicUsize = AtomicUsize::new(usize::MAX); // el0_fault_count beim Start
/// PRE-Badge "INTR" — intruder-u signalisiert es VOR dem fatalen Kernel-RAM-Zugriff.
const INTRU_PRE: u64 = 0x494E_5452;

// ext-27 T2: HardwareLand-Dienste. AGGRH = HardwareLand-Aggressor (Cap-Confusion + Eskalation aus
// einem Backend -> KEINE Management-Autoritaet, nichts ausserhalb des eigenen Kanals erreichbar).
static AGGRH_STARTED: AtomicBool = AtomicBool::new(false);
static AGGRH_DONE: AtomicBool = AtomicBool::new(false);
static AGGRH_OK: AtomicBool = AtomicBool::new(false);
static AGGRH_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static AGGRH_POLLS: AtomicU32 = AtomicU32::new(0);
/// Erfolgs-Badge "AGRH" — aggressor-h meldet es ueber den eigenen Kanal bei voller Abweisung.
const AGGRH_SUCCESS: u64 = 0x4147_5248;
// INTRH = HardwareLand-Intruder (Speicher-Isolation domaenen-unabhaengig: ein Backend faultet
// ebenso auf Kernel-RAM).
static INTRH_STARTED: AtomicBool = AtomicBool::new(false);
static INTRH_DONE: AtomicBool = AtomicBool::new(false);
static INTRH_OK: AtomicBool = AtomicBool::new(false);
static INTRH_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static INTRH_POLLS: AtomicU32 = AtomicU32::new(0);
static INTRH_FAULT_BASE: AtomicUsize = AtomicUsize::new(usize::MAX);
/// PRE-Badge "INRH" — intruder-h signalisiert es ueber den eigenen Kanal vor dem fatalen Zugriff.
const INTRH_PRE: u64 = 0x494E_5248;

// ext-27 T3: TrustedSAS-Dienste (geladen EL0-isoliert). AGGRT = TrustedSAS-Aggressor (Beweis
// "Trust != Privileg": hoechste Cap-Autoritaet, doch ohne PdControl/Loader-Cap -> PDCTL/LOAD/KILL
// = BADCAP).
static AGGRT_STARTED: AtomicBool = AtomicBool::new(false);
static AGGRT_DONE: AtomicBool = AtomicBool::new(false);
static AGGRT_OK: AtomicBool = AtomicBool::new(false);
static AGGRT_NTFN: AtomicUsize = AtomicUsize::new(usize::MAX);
static AGGRT_POLLS: AtomicU32 = AtomicU32::new(0);
/// Erfolgs-Badge "AGRT" — aggressor-t signalisiert es nur bei vollstaendig abgewiesener Batterie.
const AGGRT_SUCCESS: u64 = 0x4147_5254;
// INTRT = TrustedSAS-Intruder, ext-28 UMGEWIDMET: Beweis, dass UNZERTIFIZIERTES TrustedSAS gar nicht
// erst laedt. intruder-t traegt absichtlich `unsafe` (der illegale Kernel-RAM-Zugriff IST sein Test)
// und ist daher NICHT zertifizierbar -> ohne gueltiges Zertifikat weist das verify_image-Gate (ADR
// 0014) das Laden ab. Reine Lade-Abweisung (synchron) -> kein Notification-/Fault-Polling noetig.
static INTRT_STARTED: AtomicBool = AtomicBool::new(false);
static INTRT_DONE: AtomicBool = AtomicBool::new(false);
static INTRT_OK: AtomicBool = AtomicBool::new(false);

// ext-27 T4: Cross-Service-Matrix -- DREI Angreifer DREIER Domaenen NEBENLAEUFIG geladen: zwei
// Aggressoren (UserLand + TrustedSAS) muessen UNABHAENGIG ihr SUCCESS melden (gleichzeitige
// cross-domain Angreifer stoeren einander NICHT), ein Intruder (HardwareLand) faultet beim
// Fremdspeicher-Zugriff; ein kernel-geschuetztes Canary-Frame bleibt BIT-FUER-BIT unberuehrt;
// Audits 0 -> Cross-Service-Isolation unter Nebenlaeufigkeit ueber alle Domaenen.
static CROSS_STARTED: AtomicBool = AtomicBool::new(false);
static CROSS_DONE: AtomicBool = AtomicBool::new(false);
static CROSS_OK: AtomicBool = AtomicBool::new(false);
static CROSS_NU: AtomicUsize = AtomicUsize::new(usize::MAX); // aggressor-u Report-Notification
static CROSS_NT: AtomicUsize = AtomicUsize::new(usize::MAX); // aggressor-t Report-Notification
static CROSS_NH: AtomicUsize = AtomicUsize::new(usize::MAX); // intruder-h Kanal-Notification
static CROSS_CANARY: AtomicU64 = AtomicU64::new(0); // phys. Basis des geschuetzten Canary-Frames
static CROSS_FAULT_BASE: AtomicUsize = AtomicUsize::new(usize::MAX);
static CROSS_POLLS: AtomicU32 = AtomicU32::new(0);
// Report-Badges (kernel-gemintet; SIGNAL nutzt das Cap-Badge, nicht das Argument).
const CROSS_U: u64 = 0x4352_5355; // "CRSU"
const CROSS_T: u64 = 0x4352_5354; // "CRST"
const CROSS_H: u64 = 0x4352_5348; // "CRSH"
/// Canary-Muster (kernel-geschuetzter Speicher; KEIN geladener Dienst darf es erreichen).
const CROSS_CANARY_VAL: u64 = 0xC0FF_EE5E_A1ED_0001;

/// **Binary-Loader starten** (ext-26, L1): das extern gebaute `hello`-Programm aus dem Boot-Archiv
/// in eine frische isolierte UserLand-PD laden + starten. Endowt eine Notification-Cap (Slot 0,
/// Badge `HELLO_BADGE`) — hello signalisiert sie beim Start. Gibt die Notification-ID zurück (zum
/// Pollen) oder `usize::MAX` bei Fehler. Setup IRQ-maskiert (Cap-Aufbau vor der Ausführung).
fn run_load_start() -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let hello = archive.iter().find(|p| p.name() == "hello")?;
        let ntfn = system::create_notification()?;
        let root = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        // SIGNAL nutzt den CAP-Badge -> mit HELLO_BADGE minten (hellos x2 wird ignoriert).
        let wcap = system::cap_mint(root, Rights::WRITE, HELLO_BADGE).ok()?;
        loader::load_image(&hello, &[(0, wcap)], 0).ok()?;
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// **Binary-Loader L2 (SYS_LOAD) starten** (ext-26): einen TrustedSAS-Caller-Thread aufsetzen, der
/// eine **Loader-Cap** (Slot 0) + eine Notification-Cap (Slot 1, Badge `HELLO_BADGE`) haelt und
/// `hello` per `SYS_LOAD` zur Laufzeit laedt (cap-gegatet) + die Notification-Cap delegiert. Gibt
/// die Notification-ID zum Pollen zurueck (`usize::MAX` bei Setup-Fehler). IRQ-maskiert (der Caller
/// darf nicht vor dem PD-/Cap-Aufbau laufen).
fn run_sysload_start() -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let hello_idx = (0..archive.count())
            .find(|&i| archive.program(i).map_or(false, |p| p.name() == "hello"))?;
        let ntfn = system::create_notification()?;
        let nroot = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        let ncap = system::cap_mint(nroot, Rights::WRITE, HELLO_BADGE).ok()?; // Cap-Badge = HELLO_BADGE
        let lcap = system::install_loader_cap(0, Rights::RW).ok()?; // 0 = Boot-Archiv
        let pd = system::create_pd_in_domain(Domain::TrustedSas)?;
        if !system::install_pd_cap(pd, 0, lcap) || !system::install_pd_cap(pd, 1, ncap) {
            return None; // Loader-/Notification-Cap muss in TrustedSAS installierbar sein
        }
        let tid = system::spawn_parked(
            sysload_caller as *const () as usize,
            hello_idx,
            system::IDLE_PRIO,
        )?;
        let tid = system::admit_in_pd(pd, tid).expect("zulassen");
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// Caller-Thread fuer den L2-`SYS_LOAD`-Test (TrustedSAS, EL1): laedt `hello` per Syscall
/// (cap-gegatet ueber die Loader-Cap in Slot 0, delegiert die Notification-Cap aus Slot 1 in
/// hellos Slot 0). `arg` = hello-Archiv-Index. Danach ein Negativaufruf ueber einen leeren Slot
/// (keine Loader-Cap) -> ERR_BADCAP. Setzt die Telemetrie + parkt.
extern "C" fn sysload_caller(arg: usize) -> ! {
    // x1 = Loader-Cap-Slot 0 · x2 = hello-Index · x3 = **gepackte Delegationsliste** ·
    // x4 = Anzahl gueltiger Paare · x5 = Cap-Budget (0 = Vorgabe).
    //
    // **Berichtigt 2026-08-26.** Hier stand `[arg, 1, 0, 0]` -- die ABI von VOR der
    // Mehrfachdelegation (2026-08-25), in der x3 ein einzelner Slot war. Mit der neuen liest der
    // Dispatch x4 als ANZAHL, bekam `0` und delegierte **nichts**: `hello` lief, hatte aber keine
    // Notification und konnte sich nicht melden. `sysload` meldete `result=0` (der Ladevorgang
    // gelang ja) und `hello-Signal=NEIN`, `all_done()` wurde nie wahr, Watchdog -- und **neun
    // weitere Zeilen** meldeten FAILURES, weil sie nach dem Haenger gar nicht mehr liefen.
    // Aufgefallen ist es erst beim ersten aarch64-Lauf nach jener Aenderung; x86 hat diesen
    // in-Kernel-Aufrufer nicht.
    let paar = caprock_abi::delegate_pair(1, 0) as u64;
    let r = invoke(sys::LOAD, 0, [arg as u64, paar, 1, 0], 0);
    SYSLOAD_RESULT.store(r.result, Ordering::Relaxed);
    // Negativ: SYS_LOAD ueber Slot 5 (leer, keine Loader-Cap) -> ERR_BADCAP.
    let r2 = invoke(sys::LOAD, 5, [arg as u64, u64::MAX, 0, 0], 0);
    SYSLOAD_NEG_OK.store(r2.result == result::ERR_BADCAP, Ordering::Relaxed);
    SYSLOAD_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Binary-Loader L3 (HardwareLand) starten** (ext-26): eine HardwareLand-Backend-PD vor-erstellen
/// (TrustedSAS-Partner + Kanal `ep`/`ntfn`), die Kanal-Signal-Cap (Badge `HELLO_BADGE`) in Slot 0
/// installieren und `hwhello` (= hello, Domaene HardwareLand) **in diese PD** laden
/// (`load_program_into_pd`). hwhello signalisiert die Kanal-Notification. Gibt die Notification-ID
/// zum Pollen (`usize::MAX` bei Fehler). IRQ-maskiert.
fn run_loadhw_start() -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let hw = archive.iter().find(|p| p.name() == "hwhello")?;
        let partner = system::create_pd_in_domain(Domain::TrustedSas)?;
        let (hpd, _ep, ntfn) = system::create_hardware_backend(partner, 0x26)?;
        // Kanal-Signal-Cap (Badge HELLO_BADGE) in hpd Slot 0 -- install_cap_checked erlaubt fuer ein
        // HardwareLand-Backend NUR Caps des EIGENEN Kanals (Policy bleibt gueltig).
        let nroot = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        let scap = system::cap_mint(nroot, Rights::WRITE, HELLO_BADGE).ok()?;
        if !system::install_pd_cap(hpd, 0, scap) {
            return None;
        }
        loader::load_program_into_pd(&hw, hpd, &[], 0).ok()?;
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// **Binary-Loader L4 (Teardown) testen** (ext-26): ein UserLand-Programm laden, dann VOLLSTAENDIG
/// abbauen ([`system::destroy_loaded`]) und die Ressourcen-Baseline pruefen (freies MEM, freie
/// VSpaces, freie Kstack-Pool-Slots zurueck = kein Leck der geladenen Segmente/VSpace/Stack).
/// Synchron, IRQ-maskiert (saubere Baseline-Messung); das Programm wird vor dem Lauf abgebaut.
/// Die drei Zahlen, gegen die L4 seine Balance-Aussage haelt.
fn loadstop_snapshot() -> (u64, usize, usize) {
    (
        system::total_free(),
        system::free_vspaces(),
        system::user_kstack_free_count(),
    )
}

/// Wie oft die Messung wiederholt wird, wenn das System waehrend des Fensters nicht ruhig war.
const LOADSTOP_TRIES: usize = 16;

/// **Warten, bis sich die globalen Zaehler nicht mehr bewegen** (D5).
///
/// `Some(...)` = zwei aufeinanderfolgende Proben waren gleich, das System ist ruhig.
/// `None` = in `LOADSTOP_TRIES` Anlaeufen nie -- dann ist die Baseline **nicht messbar**, und das
/// ist etwas anderes als „nicht bestanden".
fn loadstop_quiet() -> Option<(u64, usize, usize)> {
    let mut prev = loadstop_snapshot();
    for _ in 0..LOADSTOP_TRIES {
        for _ in 0..200_000 {
            core::hint::spin_loop();
        }
        let now = loadstop_snapshot();
        if now == prev {
            return Some(now);
        }
        prev = now;
    }
    None
}

/// **Binary-Loader L4** (ext-26): laden + vollstaendig abbauen, und danach steht die Baseline wieder.
///
/// ## Warum hier eine Ruhephase steht (D5)
///
/// Die Messung vergleicht **globale** Zaehler (freier Speicher, freie VSpaces, Kernel-Stacks) vor
/// und nach der Operation -- abgesichert mit `local_irq_disable()`, das aber nur **diesen einen
/// Kern** stillstellt. Sieben andere Kerne und der Einsammler laufen weiter.
///
/// Solange in dem Fenster sonst nichts passierte, ging das gut. Mit dem Root-Task (D5) passiert
/// dort etwas: `init` beendet sich, und seine Freigabe faellt gelegentlich mitten in die Messung.
/// Gemessen wurde dann `free 4204597248 -> 4204613632` -- **16 KiB mehr** hinterher. Das ist kein
/// Leck, das ist ein fremder `free`; die Zeile meldete trotzdem FAILURES.
///
/// Der Punkt ist allgemeiner als der Root-Task: eine Gleichheitsaussage ueber eine geteilte
/// Groesse braucht ein Fenster, in dem niemand sonst daran schreibt. Die lokale IRQ-Sperre war nie
/// dieses Fenster -- sie sah nur so aus. Deshalb wird jetzt **erst Ruhe festgestellt** und dann
/// gemessen, und ein Lauf, der nie ruhig wird, faellt DURCH statt still als bestanden zu gelten
/// („nicht messbar" ist kein Ergebnis).
///
/// Ein echtes Leck faellt weiterhin durch: es steht in **jeder** Wiederholung in den Zahlen.
fn run_loadstop() -> bool {
    let mut last = None;
    for _ in 0..LOADSTOP_TRIES {
        let Some(_) = loadstop_quiet() else { continue };
        hal::cpu::local_irq_disable();
        let vorher = loadstop_snapshot();
        let loaded = (|| {
            let archive = loader::read_archive()?;
            let hello = archive.iter().find(|p| p.name() == "hello")?;
            let (tid, pd) = loader::load_image(&hello, &[], 0).ok()?;
            system::destroy_loaded(tid, pd);
            Some(())
        })()
        .is_some();
        let nachher = loadstop_snapshot();
        hal::cpu::local_irq_enable();
        if loaded && vorher == nachher {
            return true;
        }
        last = Some((loaded, vorher, nachher));
    }
    // Durchgefallen -- und zwar mit Zahlen. Ein blosses „FAILURES" liesse offen, ob etwas leckte
    // oder ob nie ein ruhiges Fenster zustande kam; das sind zwei verschiedene Befunde.
    match last {
        Some((loaded, (f0, v0, k0), (f1, v1, k1))) => println!(
            "loadstop: geladen={loaded} frei {f0}->{f1} vspaces {v0}->{v1} kstacks {k0}->{k1} \
             (nach {LOADSTOP_TRIES} Anlaeufen)"
        ),
        None => println!(
            "loadstop: in {LOADSTOP_TRIES} Anlaeufen kein ruhiges Fenster -- die Baseline war \
             NICHT MESSBAR (das ist kein bestandener Test)"
        ),
    }
    false
}

/// **Die drei Farbtests auf aarch64 fahren** — A1 (`color`), B-4.2 (`stripe`), B-4.5 (`pprobe`).
///
/// ## Warum das hier steht und nicht in `spawn_demo`
///
/// Bis 2026-08-02 liefen sie auf aarch64 **gar nicht**: `spawn_demo` setzte die drei Flaggen hart
/// auf `true`. Der Grund dafuer war echt — an jener Stelle belegen und geben die Tests Speicher
/// frei, BEVOR die baseline-empfindlichen Pruefungen ihre Ausgangswerte nehmen, und danach fiel
/// mal `captest`, mal `sched` durch. Nur war die Antwort falsch: ein hart gesetztes `true` ohne
/// jede Berichtszeile ist nicht „Test fehlt", sondern „Test besteht immer", und das ist die
/// schlechtere Haelfte.
///
/// Die richtige Antwort ist der **Platz**. Diese Funktion haengt am ENDE der Testkette, hinter
/// `cross` (dem letzten adversarialen Dienst), `strand` (der letzten Scheduler-Messung) und
/// `loadstop` (der Ressourcen-Bilanz). Danach nimmt keine Pruefung mehr eine Baseline; was der
/// Farbtest an der Freiliste anrichtet, kann niemanden mehr kippen.
///
/// **Der Gate-Ausdruck ist eine Zusicherung, keine Bequemlichkeit.** Wer eine baseline-empfindliche
/// Pruefung NACH `cross`/`strand`/`loadstop` einhaengt, muss sie hier ergaenzen — sonst laeuft der
/// Farbtest wieder davor.
///
/// ## Die Ruhephase ist hier eine DIAGNOSE, kein Tor — und das ist mit Absicht so
///
/// `run_loadstop` (D5) braucht die Ruhephase, weil es eine Gleichheitsaussage ueber **globale**
/// Zaehler trifft: dort entscheidet ein fremder `free` im Messfenster ueber PASS oder FAIL. Die
/// Farbtests treffen diese Aussage **nicht** — sie pruefen ihre Bilanz regionsgenau
/// (`region_fully_free` je geholter Region) statt ueber den Summenzaehler. Genau diese Umstellung
/// hat A1 schon einmal bezahlt, und sie macht die Ruhephase als *Tor* ueberfluessig.
///
/// Gemessen wird sie trotzdem, und gemeldet auch: findet sich nach der gesamten Testkette **kein**
/// ruhiges Fenster, dann werkelt dort noch etwas, obwohl alles fertig gemeldet hat. Das ist ein
/// Befund fuer sich — und ein Grund, den Zahlen darunter weniger zu glauben. Ein Tor daraus zu
/// machen waere die falsche Konsequenz: der Test wuerde dann ausgerechnet dort schweigen, wo etwas
/// nicht stimmt. Dieselbe Mechanik wie in [`run_loadstop`]: zwei gleiche Proben in Folge. Wird es nie
/// ruhig, laufen die Tests trotzdem — sie haengen nicht an der globalen Summe — aber der Bericht
/// sagt es.
fn run_color_suite() {
    let ruhig = loadstop_quiet().is_some();
    if !ruhig {
        println!(
            "color: HINWEIS  kein ruhiges Fenster in {LOADSTOP_TRIES} Anlaeufen; die drei \
             Farbtests laufen trotzdem (sie pruefen ihre Bilanz regionsgenau, nicht ueber den \
             globalen Summenzaehler)"
        );
    }

    // A1: zwei isolierte PDs mit disjunkten Farbsaetzen. Der Test baut sie sofort wieder ab.
    let c = crate::colors::run_color(iso_probe as *const () as usize, system::IDLE_PRIO);
    crate::colors::report_color(&c);
    // `usable == false` heisst NICHT durchgefallen, sondern nicht durchfuehrbar (unter zwei Farben
    // gibt es nichts zu trennen). `report_color` druckt dafuer SKIP; die Abschlussbedingung darf
    // daran nicht haengenbleiben.
    COLOR_OK.store(c.ok || !c.usable, Ordering::Release);

    // B-4.2: die Streifenvergabe fuehrt Belegung -- erschoepft heisst FEHLSCHLAG, nicht stille
    // Wiederholung. Laeuft NACH `run_color` (das gibt seine Streifen sofort wieder frei).
    STRIPE_ALLOC_OK.store(crate::colors::run_stripe_alloc(), Ordering::Release);

    // B-4.5: die WIRKUNG der Faerbung. Traegt die Positivkontrolle nicht (TCG hat keinen echten
    // Cache), ist das ein SKIP und darf die Abschlussbedingung nicht blockieren -- aber die
    // Bilanz muss immer stimmen. Gleiche Verrechnung wie auf x86 (`arch/x86_64/bringup.rs`).
    let pp = crate::colors::run_prime_probe();
    crate::colors::report_prime_probe(&pp);
    PPROBE_OK.store(
        (!pp.ran || pp.balanced) && (!pp.sensitive || pp.ok),
        Ordering::Release,
    );
}

/// **EL0-TrustedSAS-Laden pruefen** (ext-26 L3 + ext-28): ein als TrustedSAS (Domaene 0) deklariertes
/// Image wird — **nur mit gueltigem Zertifikat** (ADR 0014) — als **EL0-ISOLIERTE** PD geladen (nicht
/// EL1), hardware-isoliert, behaelt aber die Trust-Stufe. `true`, wenn `trusted-x` (das saubere,
/// zertifizierte svc-demo) laedt, die PD-Domaene TrustedSAS ist, `domain_audit` konsistent bleibt
/// **und** `trust_audit` sauber ist (Key-DB selbstkonsistent + Gate setzt zur Laufzeit aktiv durch).
/// Danach wird die PD wieder abgebaut (Balance).
fn check_loadtrusted_el0() -> bool {
    if loader::trust_audit() != 0 {
        return false; // Key-DB inkonsistent / Gate setzt nicht durch (ext-28)
    }
    let Some(archive) = loader::read_archive() else {
        return false;
    };
    let Some(tp) = archive.iter().find(|p| p.name() == "trusted-x") else {
        return false;
    };
    match loader::load_image(&tp, &[], 0) {
        Ok((tid, pd)) => {
            let trusted = system::pd_domain(pd) == Some(Domain::TrustedSas);
            let audit_ok = system::domain_audit() == 0;
            system::destroy_loaded(tid, pd); // aufraeumen
            trusted && audit_ok
        }
        Err(_) => false,
    }
}

/// **ext-27 — einen EL0-Aggressor (UserLand/TrustedSAS) laden.** Den per `name` benannten Dienst
/// laden (Domaene aus dem Archiv-Eintrag; `load_image` erzeugt eine isolierte EL0-PD) und mit GENAU
/// zwei Caps endowen: Slot 0 = Report-Notification (gemintet **WRITE-only**, Badge `badge`) — der
/// Dienst signalisiert darueber Erfolg UND probiert daran `WAIT` (braucht READ) -> `ERR_RIGHTS`;
/// Slot 1 = dieselbe Notification **READ-only** — der Dienst probiert daran `SIGNAL` (braucht WRITE)
/// -> `ERR_RIGHTS`. Alle uebrigen Angriffe laufen gegen einen leeren Slot bzw. den falschen
/// Objekttyp. Gibt die Notification-ID zum Pollen (`usize::MAX` bei Setup-Fehler). IRQ-maskiert
/// (der Dienst darf nicht vor abgeschlossenem PD-/Cap-Aufbau laufen).
fn run_el0_aggressor(name: &str, badge: u64) -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let svc = archive.iter().find(|p| p.name() == name)?;
        let ntfn = system::create_notification()?;
        let root = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        let wcap = system::cap_mint(root, Rights::WRITE, badge).ok()?; // Slot 0: WRITE-only + Badge
        let rcap = system::cap_mint(root, Rights::READ, 0).ok()?; // Slot 1: READ-only
        loader::load_image(&svc, &[(0, wcap), (1, rcap)], 0).ok()?;
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// **ext-27 — Sicherheits-Audits nach einem Angriffs-Onslaught.** Gemeinsam von allen ext-27-Tests
/// genutzt: nach jeder Attacke MUSS jedes Sicherheits-Oracle sauber sein (kein Korruptions-/Policy-/
/// Leak-Effekt durch die Angriffe eines extern geladenen Dienstes).
fn ext27_audits_ok() -> bool {
    system::domain_audit() == 0
        && system::vspace_audit() == 0
        && system::cap_audit_cdt() == 0
        && system::loader_audit() == 0
        && system::ipc_audit() == 0
}

/// **ext-27 — einen EL0-Intruder (UserLand/TrustedSAS) laden.** Den per `name` benannten Dienst
/// laden (Domaene aus dem Archiv-Eintrag) und mit Slot 0 = Report-Notification (gemintet WRITE-only,
/// Badge `badge`) endowen. Der Dienst signalisiert PRE, liest dann Kernel-RAM aus EL0 ->
/// Translation-Fault -> terminiert. Gibt die Notification-ID zum Pollen (`usize::MAX` bei Fehler).
fn run_el0_intruder(name: &str, badge: u64) -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let svc = archive.iter().find(|p| p.name() == name)?;
        let ntfn = system::create_notification()?;
        let root = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        let wcap = system::cap_mint(root, Rights::WRITE, badge).ok()?;
        loader::load_image(&svc, &[(0, wcap)], 0).ok()?;
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// **ext-28 — die Lade-Abweisung eines unzertifizierten TrustedSAS-Binaries pruefen** (ADR 0014).
/// Versucht, das per `name` benannte TrustedSAS-Programm zu laden, und gibt `true` **genau dann**,
/// wenn das [`loader::load_image`]-Gate ([`crate::loader`]) es mit [`LoaderError::Unverified`]
/// abweist (fehlendes/ungueltiges Zertifikat). Es entsteht dabei **kein** Thread und **keine** PD.
/// IRQ-maskiert (Symmetrie mit den uebrigen Lade-Helfern).
fn trusted_load_rejected(name: &str) -> bool {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let svc = archive.iter().find(|p| p.name() == name)?;
        Some(matches!(
            loader::load_image(&svc, &[], 0),
            Err(LoaderError::Unverified)
        ))
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(false)
}

/// **ext-27 T2 — ein HardwareLand-Testdienst in ein Backend laden.** Eine HardwareLand-Backend-PD
/// vor-erstellen (TrustedSAS-Partner + Kanal `ep`/`ntfn` via [`system::create_hardware_backend`]),
/// die **eigene Kanal-Notification** (gemintet WRITE-only, Badge `badge`) in Slot 0 installieren
/// (die HardwareLand-Cap-Policy erlaubt einem Backend NUR Caps des eigenen Kanals) und das per
/// `name` benannte Programm (Domaene HardwareLand) **in diese PD** laden. Gibt die Kanal-
/// Notification-ID zum Pollen (`usize::MAX` bei Setup-Fehler). IRQ-maskiert.
fn run_hw_service_start(name: &str, badge: u64, backend_id: u16) -> usize {
    hal::cpu::local_irq_disable();
    let res = (|| {
        let archive = loader::read_archive()?;
        let svc = archive.iter().find(|p| p.name() == name)?;
        let partner = system::create_pd_in_domain(Domain::TrustedSas)?;
        let (hpd, _ep, ntfn) = system::create_hardware_backend(partner, backend_id)?;
        let nroot = system::install_notification_cap(ntfn as u32, Rights::RW).ok()?;
        let scap = system::cap_mint(nroot, Rights::WRITE, badge).ok()?;
        if !system::install_pd_cap(hpd, 0, scap) {
            return None; // Kanal-Cap muss ins Backend installierbar sein
        }
        loader::load_program_into_pd(&svc, hpd, &[], 0).ok()?;
        Some(ntfn)
    })();
    hal::cpu::local_irq_enable();
    res.unwrap_or(usize::MAX)
}

/// **ext-27 T4 — Cross-Service-Matrix starten.** Ein kernel-geschuetztes Canary-Frame allozieren +
/// Muster schreiben (Proxy fuer geschuetzten Speicher), die el0_fault_count-Baseline schnappen, dann
/// DREI Angreifer DREIER Domaenen NEBENLAEUFIG laden: `aggressor-u` (UserLand) + `aggressor-t`
/// (TrustedSAS) als Self-Judging-Aggressoren + `intruder-h` (HardwareLand) als Speicher-Angreifer.
/// Sie laufen danach parallel auf den 8 Kernen. Beweisziel: gleichzeitige Angreifer VERSCHIEDENER
/// Domaenen stoeren einander NICHT (jeder Aggressor wird unabhaengig korrekt abgewiesen), der
/// Intruder faultet, und KEIN Dienst erreicht das geschuetzte Canary. `true` bei erfolgreichem
/// Setup. Die Canary-/Baseline-Schnappschuesse IRQ-maskiert.
fn run_cross_start() -> bool {
    hal::cpu::local_irq_disable();
    let cbase = match system::alloc(4096, 4096) {
        Some(c) => c.region().base,
        None => {
            hal::cpu::local_irq_enable();
            return false;
        }
    };
    poke_u64(cbase, CROSS_CANARY_VAL); // Kernel (EL1) schreibt das geschuetzte Muster
    CROSS_CANARY.store(cbase, Ordering::Relaxed);
    CROSS_FAULT_BASE.store(system::el0_fault_count(), Ordering::Relaxed);
    hal::cpu::local_irq_enable();
    // Drei Angreifer dreier Domaenen laden (jeder Starter intern IRQ-maskiert) -> laufen danach
    // NEBENLAEUFIG. Report-Badges kernel-gemintet (SIGNAL nutzt das Cap-Badge).
    let nu = run_el0_aggressor("aggressor-u", CROSS_U);
    let nt = run_el0_aggressor("aggressor-t", CROSS_T);
    let nh = run_hw_service_start("intruder-h", CROSS_H, 0x29);
    CROSS_NU.store(nu, Ordering::Relaxed);
    CROSS_NT.store(nt, Ordering::Relaxed);
    CROSS_NH.store(nh, Ordering::Relaxed);
    nu != usize::MAX && nt != usize::MAX && nh != usize::MAX
}

/// **Generische DMA-Infrastruktur testen** (ext-24): Richtung/Kohärenz (DmaCap-Attribute ->
/// richtungsminimales SMMU-AP + kohärenz-spezifische Attribute, strukturell zurückgelesen),
/// Multi-Region-Kontext, Stream-Gruppen, Scatter-Gather-Validierung, disjunkte Sub-Puffer. Synchron (IRQs
/// aus für die saubere total_free-Messung). Gibt `true` bei Erfolg + setzt die Telemetrie.
fn run_dmagen() -> bool {
    let (sid, sid2) = (0x40u32, 0x41u32);
    hal::cpu::local_irq_disable();
    // 3 RAM-Regionen + DmaCaps mit unterschiedlicher Richtung/Kohärenz.
    let (Some(r1), Some(r2), Some(r3)) = (
        system::alloc_dma_region(0x1000),
        system::alloc_dma_region(0x1000),
        system::alloc_dma_region(0x1000),
    ) else {
        hal::cpu::local_irq_enable();
        return false;
    };
    let (Ok(cap1), Ok(cap2), Ok(cap3)) = (
        system::install_dma_cap_ex(
            r1.base,
            r1.len,
            DmaDir::DeviceRead,
            DmaCoherence::NonCoherent,
            Rights::RW,
        ),
        system::install_dma_cap_ex(
            r2.base,
            r2.len,
            DmaDir::DeviceWrite,
            DmaCoherence::Coherent,
            Rights::RW,
        ),
        system::install_dma_cap_ex(
            r3.base,
            r3.len,
            DmaDir::Bidirectional,
            DmaCoherence::NonCoherent,
            Rights::RW,
        ),
    ) else {
        hal::cpu::local_irq_enable();
        return false;
    };
    let free0 = system::total_free();
    // attach: alle 3 Regionen in EINEN Kontext der StreamID (Multi-Region).
    let (Some(h1), Some(h2), Some(h3)) = (
        system::dma_attach(sid, cap1),
        system::dma_attach(sid, cap2),
        system::dma_attach(sid, cap3),
    ) else {
        hal::cpu::local_irq_enable();
        return false;
    };
    let multiregion = system::testsupport::dma_ctx_region_count(sid) == 3;
    // Richtungsminimales AP + Kohärenz strukturell prüfen (Stage-1-Leaves zurücklesen).
    let l1 = system::testsupport::dma_ctx_stage1(sid);
    // Die Stage-1-Tabelle wird mit der **IOVA** indiziert, nicht mit der PA (ext-36 Schritt b).
    // Bis dahin stand hier `r1.base`/`r2.base` — richtig nur, solange beide Achsen denselben Wert
    // trugen. Der Test lebte vollständig in `u64` und war deshalb für die Newtypes unsichtbar;
    // gefunden hat ihn erst das Auseinanderlaufen der Werte.
    let leaf1 = hal::smmu::stage1_read_leaf(l1, h1.iova.raw()); // DeviceRead  -> RO, NonCoherent -> NC
    let leaf2 = hal::smmu::stage1_read_leaf(l1, h2.iova.raw()); // DeviceWrite -> RW, Coherent    -> WB
    let dir = hal::smmu::leaf_is_ro(leaf1) && !hal::smmu::leaf_is_ro(leaf2);
    let coh = hal::smmu::leaf_is_cacheable(leaf2) && !hal::smmu::leaf_is_cacheable(leaf1);
    // Stream-Gruppe: sid2 teilt den Kontext (2 StreamIDs -> 1 CD/Stage-1).
    let group =
        system::dma_group_add(sid, sid2) && system::testsupport::dma_ctx_sid_count(sid) == 2;
    // Scatter-Gather-Validierung: valide Liste ok, Out-of-Window abgewiesen.
    let sg_good = [
        system::DmaSgEntry {
            handle: h1,
            offset: 0,
            len: 0x800,
        },
        system::DmaSgEntry {
            handle: h2,
            offset: 0x100,
            len: 0x100,
        },
    ];
    let sg_bad = [system::DmaSgEntry {
        handle: h1,
        offset: 0x800,
        len: 0x1000,
    }];
    let sg = system::dma_sg_validate(sid, &sg_good) && !system::dma_sg_validate(sid, &sg_bad);
    // Disjunkte DMA-Sub-Puffer (Konsolidierung K2: ohne DmaPool): zwei Teilbereiche der
    // angehängten Region h3 per Offset; Disjunktheit + Bounds trägt der kanonische SG-/
    // Containment-Pfad. Erschöpfung = ein Sub-Puffer größer als die Region wird abgewiesen.
    let sub_a = system::DmaSgEntry {
        handle: h3,
        offset: 0x0,
        len: 0x100,
    };
    let sub_b = system::DmaSgEntry {
        handle: h3,
        offset: 0x100,
        len: 0x100,
    };
    let pool_ok = sub_a.offset + sub_a.len <= sub_b.offset // disjunkt
        && system::dma_sg_validate(sid, &[sub_a, sub_b]) // beide in-window
        && !system::dma_sg_validate(
            sid,
            &[system::DmaSgEntry { handle: h3, offset: 0, len: 0x10000 }],
        ); // Erschöpfung: größer als die Region -> abgewiesen
    let audit = system::dma_audit() == 0 && system::vspace_audit() == 0;
    // Teardown: alle Regionen lösen -> Kontext abgebaut (Stage-1/CD frei). Danach Caps löschen.
    system::dma_detach(sid, h1);
    system::dma_detach(sid, h2);
    system::dma_detach(sid, h3);
    let balanced = system::total_free() == free0; // Stage-1-/CD-Frames balanciert
    let _ = system::cap_delete(cap1);
    let _ = system::cap_delete(cap2);
    let _ = system::cap_delete(cap3);
    hal::cpu::local_irq_enable();

    DMAGEN_MULTIREGION.store(multiregion, Ordering::Relaxed);
    DMAGEN_DIR.store(dir, Ordering::Relaxed);
    DMAGEN_COH.store(coh, Ordering::Relaxed);
    DMAGEN_GROUP.store(group, Ordering::Relaxed);
    DMAGEN_SG.store(sg, Ordering::Relaxed);
    DMAGEN_POOL.store(pool_ok, Ordering::Relaxed);
    DMAGEN_BALANCED.store(balanced, Ordering::Relaxed);
    multiregion
        && dir
        && coh
        && group
        && sg
        && pool_ok
        && audit
        && balanced
        && system::domain_audit() == 0
}

/// Die beiden DMA-Pruefungen liegen jetzt arch-neutral in `crate::dmatests` -- sie waren zuvor
/// in diesem aarch64-only Modul und existierten auf x86 gar nicht (ext-38).
fn run_dmawin() -> bool {
    let r = crate::dmatests::run_dmawin(PCIE_RID.load(Ordering::Acquire));
    DMAWIN_NARROW.store(r.narrow, Ordering::Relaxed);
    DMAWIN_EXHAUST.store(r.exhausted, Ordering::Relaxed);
    DMAWIN_INTACT.store(r.intact, Ordering::Relaxed);
    DMAWIN_BALANCED.store(r.balanced, Ordering::Relaxed);
    DMAWIN_UNDECL.store(r.undeclared, Ordering::Relaxed);
    // B-3.4 (E-Rest 1): das Urteil ueber das Interrupt-Nachrichtenfenster hat **drei** Zustaende
    // und wurde auf aarch64 bis hierher ueberhaupt nicht berichtet -- es steckte still in `r.ok`.
    // Auf dieser Architektur sagt die HAL `None` zu ("die ITS-Doorbell wird uebersetzt"), das
    // Urteil ist also `Clear` aus einer ZUSAGE. Genau das gehoert in den Bericht: sonst ist von
    // aussen nicht zu sehen, ob geprueft oder zugesagt wurde.
    DMAWIN_VERDICT.store(r.verdict().code(), Ordering::Relaxed);
    DMAWIN_MSI_POLICY.store(crate::dmatests::msi_code(r.msi.policy), Ordering::Relaxed);
    DMAWIN_MSI_LIVE.store(r.msi.live, Ordering::Relaxed);
    DMAWIN_MSI_WEAK.store(r.msi.weak, Ordering::Relaxed);
    DMAWIN_MSI_DRIFT.store(r.msi.drift, Ordering::Relaxed);
    DMAWIN_MSI_OVL.store(r.msi.live_overlaps, Ordering::Relaxed);
    r.ok
}

fn run_dmatok() -> bool {
    let r = crate::dmatests::run_dmatok(PCIE_RID.load(Ordering::Acquire));
    DMATOK_DELETE_TEARS.store(r.tears, Ordering::Relaxed);
    DMATOK_FREED.store(r.freed, Ordering::Relaxed);
    DMATOK_PENDING.store(r.pending, Ordering::Relaxed);
    DMATOK_AUDIT7.store(r.audit7, Ordering::Relaxed);
    r.ok
}

/// **Prozess-Heap testen** (ext-25): ein Trusted-SAS-Kontext (safe Rust) baut einen prozess-
/// lokalen `Heap` über die kernel-`RegionSource` und nutzt **echte** `Box`/`Vec`/`BTreeMap` auf
/// realen Physadressen. Wachsen (über Größenklassen + über eine Region hinaus = grow), Large-Alloc
/// (dedizierte Region), Drop (Slab-Free-Listen/Release) und Balance (alle Regionen zurück an `MEM`).
/// Der Testcode ist **100% safe** — das einzige `unsafe` liegt in der Region-Runtime. IRQs aus für
/// die saubere total_free-Messung.
fn run_sasheap() -> bool {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use caprock_region::heap::Heap;

    hal::cpu::local_irq_disable();
    let free0 = system::total_free();
    let (vec_ok, box_ok, map_ok, large_ok, grew) = {
        let heap = Heap::new(system::KernelRegionSource);
        // 1. Vec, das ueber Groessenklassen UND ueber eine Region hinaus waechst (Realloc = grow).
        let mut v: Vec<u64, _> = Vec::new_in(&heap);
        for i in 0..4096u64 {
            v.push(i.wrapping_mul(2654435761));
        }
        let vec_ok = v.len() == 4096 && v[0] == 0 && v[4095] == 4095u64.wrapping_mul(2654435761);
        // 2. Box (mittelgross, eine Slab-Klasse).
        let b = Box::new_in([0xA5u8; 300], &heap);
        let box_ok = b[0] == 0xA5 && b[299] == 0xA5;
        // 3. BTreeMap (viele kleine Knoten -> Slab-Alloc/Free-Churn).
        let mut m: BTreeMap<u64, u64, _> = BTreeMap::new_in(&heap);
        for i in 0..256u64 {
            m.insert(i, i ^ 0xDEAD);
        }
        let map_ok =
            m.len() == 256 && m.get(&123) == Some(&(123u64 ^ 0xDEAD)) && m.get(&999).is_none();
        // 4. Large-Allokation (> groesste Klasse) -> dedizierte Region (Bump/whole-region).
        let mut big: Vec<u8, _> = Vec::with_capacity_in(16384, &heap);
        big.resize(16384, 0x5A);
        let large_ok = big.len() == 16384 && big[16383] == 0x5A;
        // mind. eine Region wurde vom Heap angefordert (grow real beobachtet).
        let grew = heap.region_count() >= 1;
        // 5. Allokationen droppen -> Slab-Free-Listen befuellt / Large-Region freigegeben.
        drop(v);
        drop(b);
        drop(m);
        drop(big);
        (vec_ok, box_ok, map_ok, large_ok, grew)
        // `heap` faellt hier -> alle gehaltenen Regionen zurueck an MEM (Balance).
    };
    let balanced = system::total_free() == free0;
    hal::cpu::local_irq_enable();

    SASHEAP_VEC.store(vec_ok, Ordering::Relaxed);
    SASHEAP_BOX.store(box_ok, Ordering::Relaxed);
    SASHEAP_MAP.store(map_ok, Ordering::Relaxed);
    SASHEAP_LARGE.store(large_ok, Ordering::Relaxed);
    SASHEAP_GREW.store(grew, Ordering::Relaxed);
    SASHEAP_BALANCED.store(balanced, Ordering::Relaxed);
    vec_ok
        && box_ok
        && map_ok
        && large_ok
        && grew
        && balanced
        && system::domain_audit() == 0
        && system::vspace_audit() == 0
}

// Audit-Regression B (Scheduler/MCS): `set_budget`/`bind_sched_context` auf einen
// ERSCHÖPFTEN Thread darf ihn nicht stranden (Fund: `depleted` gelöscht ohne
// `enqueue_ready` -> Thread nie wieder einplanbar, belegter Slot -> DoS + Leak-
// Erkennung getäuscht). Ein budgetierter Worker auf STRAND_CORE erschöpft (budget=1,
// lange Periode -> kein natürlicher Refill im Testfenster); nach erneutem Bind muss er
// WIEDER laufen (Zähler wächst). Ohne Fix bleibt der Zähler eingefroren.
const STRAND_CORE: usize = 4;
static STRAND_COUNT: AtomicU64 = AtomicU64::new(0);
static STRAND_TID: AtomicU64 = AtomicU64::new(u64::MAX);
static STRAND_SNAP1: AtomicU64 = AtomicU64::new(0);
static STRAND_STEP: AtomicU32 = AtomicU32::new(0);
static STRAND_SETTLE: AtomicU32 = AtomicU32::new(0);
static STRAND_STOP: AtomicBool = AtomicBool::new(false);
static STRAND_DONE: AtomicBool = AtomicBool::new(false);
static STRAND_OK: AtomicBool = AtomicBool::new(false);

const RECLAIM_TARGET: u64 = 16;
const RECLAIM_PRIO: u8 = 6; // höchste Demo-Prio: Exiter läuft sofort + beendet sich
static RECLAIM_SPAWNED: AtomicU64 = AtomicU64::new(0);

// Lastausgleich: lastbewusst platzierte Worker landen auf den am wenigsten
// belasteten Kernen (nicht alle auf dem Bootkern) -> Verteilung über die Kerne.
const BALANCED_TARGET: u64 = 16;
static BALANCED_SPAWNED: AtomicU64 = AtomicU64::new(0);

// --- Skalierung: viele Threads gleichzeitig (ext-30) ---------------------------------------
//
// Zielbild des Projekts ist eine Maschine mit sehr vielen Kernen und **vielen tausend**
// Prozessen. Vor ext-30 war die Obergrenze eine Compile-Zeit-Konstante (8 Kerne x 64 TCBs =
// 512 Slots, als `.bss`-Arrays im Image); jetzt kommt die Kapazität beim Boot aus dem RAM und
// die heissen Pfade sind O(1) (Freilisten + intrusive Ready-Listen statt linearer Scans).
// Dieser Test belegt beides: SCALE_TARGET Threads GLEICHZEITIG am Leben, danach vollstaendig
// abgebaut (Thread-Slots + Stack-RAM zurueck auf die Baseline -> kein Leck).
const SCALE_TARGET: usize = 1024;
static SCALE_SPAWNED: AtomicU64 = AtomicU64::new(0);
static SCALE_ALIVE_OK: AtomicBool = AtomicBool::new(false);
static SCALE_SLOTS_OK: AtomicBool = AtomicBool::new(false);
static SCALE_TEARDOWN_OK: AtomicBool = AtomicBool::new(false);
static SCALE_AUDIT: AtomicU64 = AtomicU64::new(u64::MAX);
static SCALE_DONE: AtomicBool = AtomicBool::new(false);

/// Skalierungs-Arbeiter: sofort parken (blockiert, verbraucht keine CPU) — es geht um die
/// **Verwaltung** vieler gleichzeitig existierender Threads, nicht um Rechenlast.
extern "C" fn scale_worker(_arg: usize) -> ! {
    invoke(sys::PARK, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Treiber des Skalierungstests (core 0).
extern "C" fn scale_driver(_arg: usize) -> ! {
    // Erst starten, wenn der Migrationstest fertig ist: beide Tests messen Lastverhaeltnisse
    // bzw. Slot-Buchhaltung und wuerden sich sonst gegenseitig stoeren.
    while !MIG_DONE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let slots0 = system::threads_available();
    // Handles in einer Region ablegen (kein Kernel-Heap): SCALE_TARGET u64-Rohwerte.
    let Some(store) = system::alloc(SCALE_TARGET as u64 * 8, 4096) else {
        SCALE_DONE.store(true, Ordering::Release);
        invoke(sys::EXIT, 0, [0; 4], 0);
        loop {
            core::hint::spin_loop();
        }
    };
    let base = store.base();
    let mut n = 0usize;
    for i in 0..SCALE_TARGET {
        // Lastbewusst ueber alle Kerne verteilen (die Platzierung nutzt den lock-freien
        // Lastzaehler — frueher haette das je Spawn JEDEN Kern-Scheduler gesperrt).
        match system::spawn_balanced(scale_worker as *const () as usize, i, system::IDLE_PRIO) {
            Some(t) => {
                poke_u64(base + (i as u64) * 8, t.to_raw());
                n += 1;
            }
            None => break,
        }
    }
    SCALE_SPAWNED.store(n as u64, Ordering::Release);
    // Alle muessen GLEICHZEITIG leben und ueber ihre (migrationsstabile) ThreadId auffindbar sein.
    let mut alive = 0usize;
    for i in 0..n {
        if system::thread_alive(ThreadId::from_raw(peek_u64(base + (i as u64) * 8))) {
            alive += 1;
        }
    }
    SCALE_ALIVE_OK.store(alive == n && n == SCALE_TARGET, Ordering::Release);
    let slots_low = system::threads_available();
    // Slot-Buchhaltung mit Toleranz: die uebrigen Demos erzeugen/beenden nebenher staendig
    // kurzlebige Threads und geben dabei selbst Slots zurueck — eine exakte Gleichheit waere
    // eine Wettlaufbedingung, kein Beweis. Beweiskraeftig ist die Groessenordnung (1024
    // belegte Slots gegen eine Handvoll Stoerung) und, exakt, die Rueckgabe beim Teardown.
    const SCALE_SLACK: usize = 64;
    SCALE_SLOTS_OK.store(slots_low + n <= slots0 + SCALE_SLACK, Ordering::Release);
    SCALE_AUDIT.store(system::sched_audit_all() as u64, Ordering::Release);
    let reaped0 = system::reaped_bytes();
    // Vollstaendig abbauen: killen (auf beliebigem Kern) + auf allen Kernen reapen.
    for i in 0..n {
        let t = ThreadId::from_raw(peek_u64(base + (i as u64) * 8));
        for _ in 0..4 {
            if system::kill_remote(t) {
                break;
            }
        }
    }
    // Reap: die Zombies liegen auf allen Kernen; mehrfach durchlaufen (Puffer je Runde begrenzt).
    let mut rounds = 0;
    while rounds < 4096 {
        let mut got = 0;
        for c in 0..system::num_cores() {
            got += system::reap_core(c);
        }
        if got == 0 {
            break;
        }
        rounds += 1;
    }
    system::free(store);
    // Buchhaltung **relativ** statt absolut: die uebrigen Demos laufen nebenher und veraendern
    // `total_free`/`threads_available` staendig. Beweiskraeftig ist, dass GENAU DIESE n Threads
    // ihre Slots und ihr Stack-RAM zurueckgegeben haben — `reaped_bytes` ist dafuer monoton.
    let slots_back = system::threads_available() >= slots_low + n;
    let mem_back = system::reaped_bytes() - reaped0 >= n as u64 * system::stack_bytes();
    SCALE_TEARDOWN_OK.store(slots_back && mem_back, Ordering::Release);
    SCALE_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

// --- Thread-Migration (ext-30) ------------------------------------------------------------
//
// Beweist, dass ein Thread den Kern wechseln kann, OHNE seine Identität zu verlieren:
// dieselbe `ThreadId` (und damit jede Tcb-Cap darauf) bezeichnet ihn vorher wie nachher, er
// läuft danach nachweislich auf dem Zielkern weiter, und seine per-Thread-Kernelzustände
// (FP-Kontext, VSpace-Zuordnung, Kernel-Stack-Slot) folgen ihm — sie sind über den
// migrationsstabilen globalen Slot indiziert.
/// Kern, auf dem sich der Migrant zuletzt selbst beobachtet hat.
static MIG_CORE_SEEN: AtomicU64 = AtomicU64::new(u64::MAX);
/// Fortschritt des Migranten (er zählt, solange er läuft).
static MIG_PROGRESS: AtomicU64 = AtomicU64::new(0);
/// Signal an Migrant + Helfer: Arbeit einstellen und sich **parken** (blockieren).
static MIG_STOP: AtomicBool = AtomicBool::new(false);
/// Ergebnisbits des Migrationstests.
static MIG_BEFORE_OK: AtomicBool = AtomicBool::new(false);
static MIG_MOVED: AtomicBool = AtomicBool::new(false);
static MIG_AFTER_OK: AtomicBool = AtomicBool::new(false);
static MIG_CAP_OK: AtomicBool = AtomicBool::new(false);
static MIG_AUTO_OK: AtomicBool = AtomicBool::new(false);
static MIG_KILLED_OK: AtomicBool = AtomicBool::new(false);
static MIG_AUDIT: AtomicU64 = AtomicU64::new(u64::MAX);
static MIG_DONE: AtomicBool = AtomicBool::new(false);
/// Zielkern der Migration (fest: core 3 — ein Sekundärkern, der im Test sonst nur seinen
/// eigenen SMP-Worker trägt).
const MIG_DST_CORE: usize = 3;

/// Der Migrant: notiert bei jedem Durchlauf, auf welchem Kern er gerade läuft, und zählt
/// seinen Fortschritt. Beides zusammen belegt „läuft nach der Migration auf dem Zielkern
/// weiter" (Kern-ID wechselt UND der Zähler steigt weiter).
extern "C" fn migrant_worker(_arg: usize) -> ! {
    loop {
        MIG_CORE_SEEN.store(hal::cpu::core_id() as u64, Ordering::Release);
        MIG_PROGRESS.fetch_add(1, Ordering::Release);
        // Auf Anforderung dauerhaft parken. Das macht den anschliessenden KILL
        // **deterministisch**: `kill` verweigert den gerade LAUFENDEN Thread eines Kerns, und
        // ein dauerhaft rechnender Thread ist auf seinem Kern in ~jedem zweiten Tick genau das.
        // Ein blockierter Thread ist es nie.
        if MIG_STOP.load(Ordering::Acquire) {
            invoke(sys::PARK, 0, [0; 4], 0);
        }
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
    }
}

/// Treiber des Migrationstests. Läuft **auf core 0**, weil eine Migration immer vom
/// abgebenden Kern ausgeführt wird (Push-Modell: nur er kann den Lazy-FP-Kontext des
/// Migranten aus seinen eigenen FP-Registern sichern).
extern "C" fn migrate_driver(_arg: usize) -> ! {
    let ok = (|| {
        // 1) Migranten auf core 0 erzeugen + eine Tcb-Cap darauf prägen.
        let tid = system::spawn_on_core(
            0,
            migrant_worker as *const () as usize,
            0,
            system::IDLE_PRIO,
        )?;
        let cap = system::install_tcb_cap(tid, Rights::RW).ok()?;
        // 2) Warten, bis er auf core 0 gelaufen ist.
        let mut spins = 0u64;
        while MIG_PROGRESS.load(Ordering::Acquire) < 3 {
            spins += 1;
            if spins > 200_000_000 {
                return None;
            }
            core::hint::spin_loop();
        }
        MIG_BEFORE_OK.store(
            MIG_CORE_SEEN.load(Ordering::Acquire) == 0 && system::owner_core_of(tid) == Some(0),
            Ordering::Release,
        );
        // 3) Migrieren (explizit) und warten, bis er sich auf dem Zielkern meldet.
        let moved = system::migrate_to(tid, MIG_DST_CORE);
        MIG_MOVED.store(moved, Ordering::Release);
        let p0 = MIG_PROGRESS.load(Ordering::Acquire);
        spins = 0;
        while MIG_PROGRESS.load(Ordering::Acquire) < p0 + 3
            || MIG_CORE_SEEN.load(Ordering::Acquire) != MIG_DST_CORE as u64
        {
            spins += 1;
            if spins > 200_000_000 {
                break;
            }
            core::hint::spin_loop();
        }
        MIG_AFTER_OK.store(
            MIG_CORE_SEEN.load(Ordering::Acquire) == MIG_DST_CORE as u64
                && MIG_PROGRESS.load(Ordering::Acquire) >= p0 + 3
                && system::owner_core_of(tid) == Some(MIG_DST_CORE),
            Ordering::Release,
        );
        // 4) **Identität**: dieselbe Tcb-Cap bezeichnet weiterhin genau diesen (nun auf einem
        //    anderen Kern laufenden) Thread.
        let cap_ok = match system::cap_inspect(cap).map(|i| i.kind) {
            Some(caprock_cap::ObjectKind::Tcb(raw)) => {
                ThreadId::from_raw(raw) == tid && system::thread_alive(tid)
            }
            _ => false,
        };
        MIG_CAP_OK.store(cap_ok, Ordering::Release);
        // 5) **Automatische** Policy: `balance_once` gibt von diesem Kern einen bereiten Thread
        //    an einen leichteren ab. Dafür ein **definiertes** Ungleichgewicht herstellen
        //    (mehrere rechenbereite Threads auf core 0) — sonst hinge das Ergebnis an der
        //    zufälligen Umgebungslast der übrigen Demos.
        let mut helpers = [0u64; 8];
        for h in helpers.iter_mut() {
            match system::spawn_on_core(
                0,
                migrant_worker as *const () as usize,
                0,
                system::IDLE_PRIO,
            ) {
                Some(t) => *h = t.to_raw(),
                None => *h = u64::MAX,
            }
        }
        let before = system::migration_count();
        let mut auto_ok = false;
        for _ in 0..64 {
            if system::balance_once() {
                auto_ok = true;
                break;
            }
        }
        MIG_AUTO_OK.store(
            auto_ok && system::migration_count() > before,
            Ordering::Release,
        );
        // Ab hier soll niemand mehr rechnen: Migrant + Helfer parken sich selbst, sobald sie
        // das Signal sehen. Danach ist keiner von ihnen mehr der laufende Thread seines Kerns
        // und der KILL greift beim ersten Versuch.
        MIG_STOP.store(true, Ordering::Release);
        // Helfer abräumen (kurz warten, bis sie das Signal gesehen haben).
        for &h in helpers.iter() {
            if h == u64::MAX {
                continue;
            }
            let t = ThreadId::from_raw(h);
            for _ in 0..2_000 {
                if !system::thread_alive(t) || system::kill_remote(t) {
                    break;
                }
                for _ in 0..5_000 {
                    core::hint::spin_loop();
                }
            }
        }
        // 6) Migrierten Thread über seinen (fremden) Kern beenden -> Ressourcen zurück.
        //    Der Migrant rechnet dauerhaft, ist auf seinem Kern also oft der LAUFENDE Thread —
        //    `kill` verweigert genau den (er säße auf seinem eigenen Stack). Also wiederholen,
        //    bis er verdrängt ist; das ist der normale Ablauf, kein Fehlerfall.
        let mut killed = false;
        for _ in 0..2_000 {
            if system::kill_remote(tid) {
                killed = true;
                break;
            }
            for _ in 0..5_000 {
                core::hint::spin_loop();
            }
        }
        MIG_KILLED_OK.store(killed && !system::thread_alive(tid), Ordering::Release);
        let _ = system::cap_delete(cap);
        // 7) Beide beteiligten Kern-Scheduler müssen strukturell konsistent sein.
        MIG_AUDIT.store(system::sched_audit_all() as u64, Ordering::Release);
        Some(())
    })();
    if ok.is_none() {
        MIG_AUDIT.store(u64::MAX, Ordering::Release); // Setup fehlgeschlagen -> FAILURES
    }
    MIG_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

static BALANCED_PLACE: [AtomicU64; NUM_CORES] = [const { AtomicU64::new(0) }; NUM_CORES];

// Weg C (Hybrid): isolierte EL0-PD mit eigener VSpace vs. vertrauenswürdige SAS-PD.
// Beide versuchen, dieselbe fremde RAM-Adresse X zu lesen: die SAS-PD darf (X ist im
// SAS EL0-RW), die isolierte PD faultet (X ist in ihrer VSpace EL1-only -> Kernel
// beendet sie). Belegt: nur per-Prozess-VSpace gibt echte User<->User-Trennung.
const ISO_SECRET: u64 = 0x5E_C4E7_5E_C4E7;
const ISO_BADGE_RAN: u64 = 1 << 0; // isolierte PD lief + IPC ging (vor dem Fault)
const ISO_BADGE_READ: u64 = 1 << 1; // isolierte PD las X (DARF NIE kommen)
const ISO_BADGE_TRUSTED: u64 = 1 << 2; // SAS-PD las X erfolgreich
const ISO_BADGE_MAPPED: u64 = 1 << 3; // VMM-Probe: Frame gemappt + RW erfolgreich
const ISO_BADGE_SHARED: u64 = 1 << 4; // Reader las den Wert des Writers via Shared Frame
const ISO_BADGE_NATIVE: u64 = 1 << 5; // privat geladener Code lief in eigener VSpace
const ISO_BADGE_PAGES: u64 = 1 << 6; // 4-KiB-Seiten: RW-Schreiben + RO-Lesen erfolgreich
static ISO_MASK: AtomicU64 = AtomicU64::new(0); // gesammelte Badges
static ISO_SECRET_ADDR: AtomicU64 = AtomicU64::new(0); // Adresse X (fremdes RAM)

// Shared-Memory-IPC: zwei isolierte PDs teilen einen Frame F (in beide VSpaces
// gemappt). Der Writer schreibt SHM_SECRET, der Reader liest es (nach Notification).
// Der Wert ist in den shm_writer/shm_reader-Asm-Blöcken als Immediate kodiert; diese
// Konstante dokumentiert ihn (Quelle der Wahrheit für den Kommentar).
#[allow(dead_code)]
const SHM_SECRET: u64 = 0x5A5A_1234_5678_9ABC;

// Kern-übergreifende synchrone IPC: Client auf core 0 ruft (CALL) einen Server auf
// core 2; Antwort (REPLY) geht zurück über die Kerngrenze. Beide Richtungen wecken
// den Partner per Cross-Core-IPI.
const XIPC_N: usize = 3;
const XIPC_IN: [u64; XIPC_N] = [3, 5, 7];
const XIPC_FACTOR: u64 = 7; // Server antwortet mit FACTOR * Eingabe
const XIPC_SERVER_CORE: usize = 2;
static XIPC_RESULT: [AtomicU64; XIPC_N] = [const { AtomicU64::new(0) }; XIPC_N];
static XIPC_DONE: AtomicBool = AtomicBool::new(false);

// Per-Kern-paralleler Scheduler: je ein Worker auf den Sekundärkernen 1..NUM_CORES
// läuft PARALLEL (eigene Scheduler-Instanz je Kern, kein globaler Lock); ein
// Parker auf core 1 wird kern-übergreifend von core 0 per IPI geweckt.
const SMP_WORK_TARGET: u64 = 5;
static XCORE_PROGRESS: [AtomicU64; NUM_CORES] = [const { AtomicU64::new(0) }; NUM_CORES];
static XCORE_PARKED: AtomicBool = AtomicBool::new(false); // Parker hat sich blockiert
static XCORE_WOKEN: AtomicBool = AtomicBool::new(false); // Parker nach IPI-Wake fortgesetzt
static XCORE_PARKER_TID: AtomicU64 = AtomicU64::new(u64::MAX); // raw ThreadId des Parkers

// Stateful Hot-Reload: Zähler-Service, dessen Zustand (in einer Region, Purpose::HotReloadState,
// gehalten in system::CS_STATE_REGION) den Komponententausch v1(+1) -> v2(+10) überlebt. Der
// Zugriff läuft über die sichere RegionView-API (system::hotreload_state_get/set), kein rohes
// peek/poke mehr (Konsolidierung O-B).
static CS_R1: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3]; // v1: 1,2,3
static CS_R2: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2]; // v2: 13,23 (Zustand erhalten)
static CS_BATCH1_DONE: AtomicBool = AtomicBool::new(false);
static CS_RELOADED: AtomicBool = AtomicBool::new(false);
static CS_DONE: AtomicBool = AtomicBool::new(false);
// A-4.3: wievielte Uebernahme des Zustands die neue Fassung war. 0 heisst: v2 hat den Zustand
// nie uebernommen -- entweder weil es die Uebernahme nicht versucht hat oder weil sie abgewiesen
// wurde. Ohne diesen Zaehler waere "der Zustand hat den Tausch ueberlebt" von "v2 hat zufaellig
// dieselben Zahlen erzeugt" nicht zu unterscheiden.
static CS_GENERATION: AtomicU64 = AtomicU64::new(0);
static CS_RELOAD_INFO: SpinLock<Option<ReloadInfo>> = SpinLock::new(None);
static R1: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static R2: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static BATCH1_DONE: AtomicBool = AtomicBool::new(false);
static RELOADED: AtomicBool = AtomicBool::new(false);
static ALL_DONE: AtomicBool = AtomicBool::new(false);

/// Zustand, den der Reload-Manager braucht.
#[derive(Clone, Copy)]
struct ReloadInfo {
    ep: usize,
    v1: ThreadId,
    v1_pd: usize,
    v2_pd: usize,
}
static RELOAD_INFO: SpinLock<Option<ReloadInfo>> = SpinLock::new(None);

/// PDs + Endpoint + Caps anlegen und Demo-Threads (v1, Client, Worker) starten.
pub fn spawn_demo() {
    // Die drei Farbtests (A1, B-4.2, B-4.5) laufen hier **nicht mehr** -- und auch nicht mehr
    // gar nicht. Sie stehen jetzt als letztes Glied der Testkette in `demo_report_then_idle`
    // (s. `run_color_suite`).
    //
    // Was hier stand, war schlimmer als ein fehlender Test: `COLOR_OK`, `STRIPE_ALLOC_OK` und
    // `PPROBE_OK` wurden hart auf `true` gesetzt, ohne dass irgendetwas lief. Drei dauerhaft
    // wahre Konjunkte in `all_done()`, keine Berichtszeile, kein Check in `test-qemu.sh` --
    // die Abwesenheit war damit nicht bloss unbelegt, sie war **unsichtbar**. Genau die
    // Fehlerform, gegen die dieses Projekt seinen Leitsatz hat.
    //
    // Der Grund, aus dem sie hier ausgehaengt waren, galt: an dieser Stelle belegen und geben
    // sie Speicher frei, BEVOR die baseline-empfindlichen Tests ihre Ausgangswerte nehmen --
    // danach fiel mal `captest`, mal `sched` durch. Die Antwort darauf ist nicht, den Test
    // wegzulassen, sondern ihn dorthin zu stellen, wo keine Baseline mehr genommen wird.

    let ep = system::create_endpoint().expect("endpoint");
    let root = system::install_endpoint_cap(ep as u32, Rights::RWX).expect("ep cap");
    let send_cap = system::cap_mint(root, Rights::WRITE, 0).expect("send cap");
    let recv_cap_v1 = system::cap_mint(root, Rights::READ, 0).expect("recv cap v1");
    let recv_cap_v2 = system::cap_mint(root, Rights::READ, 0).expect("recv cap v2");

    let client_pd = system::create_pd().expect("client pd");
    let v1_pd = system::create_pd().expect("v1 pd");
    let v2_pd = system::create_pd().expect("v2 pd"); // Thread erst beim Reload

    system::install_pd_cap(client_pd, EP_CAP as usize, send_cap);
    system::install_pd_cap(v1_pd, EP_CAP as usize, recv_cap_v1);
    system::install_pd_cap(v2_pd, EP_CAP as usize, recv_cap_v2);

    let prio = system::IDLE_PRIO; // gewöhnliche Demo-Threads: Round-Robin mit Idle
    let v1 = system::spawn_parked(server_v1 as *const () as usize, 0, prio).expect("v1 thread");
    let v1 = system::admit_in_pd(v1_pd, v1).expect("zulassen");
    let client = system::spawn_parked(client as *const () as usize, 0, prio).expect("client thread");
    let client = system::admit_in_pd(client_pd, client).expect("zulassen");

    for id in 0..NWORKERS {
        system::spawn(worker as *const () as usize, id, prio);
    }
    // Lazy-FP: ein EL1-Kollektor + zwei EL0-FP-User-Threads (höchste Demo-Prio ->
    // striktes Ping-Pong über YIELD, das jede Iteration einen Owner-Wechsel erzwingt).
    let fp_ntfn = system::create_notification().expect("fp ntfn");
    let fp_nroot =
        system::install_notification_cap(fp_ntfn as u32, Rights::RWX).expect("fp ntfn cap");
    let fp_wait = system::cap_mint(fp_nroot, Rights::READ, 0).expect("fp wait cap");
    let fp_coll_pd = system::create_pd().expect("fp collector pd");
    system::install_pd_cap(fp_coll_pd, 0, fp_wait);
    let fp_coll = system::spawn_parked(fp_collector as *const () as usize, 0, prio).expect("fp collector");
    let fp_coll = system::admit_in_pd(fp_coll_pd, fp_coll).expect("zulassen");
    let fp_entry = core::ptr::addr_of!(user_fp_entry) as usize;
    for id in 0..FP_WORKERS {
        let sig = system::cap_mint(fp_nroot, Rights::WRITE, 1 << id).expect("fp signal cap");
        let pd = system::create_pd().expect("fp pd");
        system::install_pd_cap(pd, 0, sig);
        let t =
            system::spawn_user_parked(fp_entry, FP_PATTERN[id] as usize, FP_PRIO).expect("fp user thread");
        let t = system::admit_in_pd(pd, t).expect("zulassen");
    }
    // Prioritätstest: höhere Priorität (4) zuerst, dann 3, dann 2.
    for id in 0..NPRIO_TEST {
        system::spawn(prio_thread as *const () as usize, id, 4 - id as u8);
    }

    // Thread-Lebenszyklus: Victim (wird cap-kontrolliert getötet) + Killer-PD.
    let victim = system::spawn(victim as *const () as usize, 0, prio).expect("victim");
    let kill_cap = system::install_tcb_cap(victim, Rights::WRITE).expect("tcb cap");
    let killer_pd = system::create_pd().expect("killer pd");
    // **Cap VOR der Zulassung** (D0). Die PD zu binden reicht nicht -- ein leerer Cspace und ein
    // Cspace ohne die eine gebrauchte Cap sehen fuer den Thread gleich aus. Der Killer greift
    // sofort auf Slot 0 zu; laeuft er, bevor die Cap steht, bekommt er BADCAP und der Test misst
    // eine Verweigerung, die keine ist.
    let killer = system::spawn_parked(killer as *const () as usize, 0, prio).expect("killer");
    system::bind_pd_parked(&killer, killer_pd);
    system::install_pd_cap(killer_pd, 0, kill_cap); // Slot 0 = Tcb-Cap; Slot 5 leer
    let killer = system::admit(killer).expect("zulassen");

    // Notifications: ein Objekt, zwei abgeleitete Caps (Signal mit Badge, Wait).
    let ntfn = system::create_notification().expect("ntfn");
    let nroot = system::install_notification_cap(ntfn as u32, Rights::RWX).expect("ntfn cap");
    let signal_cap = system::cap_mint(nroot, Rights::WRITE, NOTIF_BADGE_VAL).expect("signal cap");
    let wait_cap = system::cap_mint(nroot, Rights::READ, 0).expect("wait cap");
    let producer_pd = system::create_pd().expect("producer pd");
    let consumer_pd = system::create_pd().expect("consumer pd");
    // Dito fuer Producer/Consumer: binden, Cap einsetzen, DANN zulassen.
    let producer = system::spawn_parked(producer as *const () as usize, 0, prio).expect("producer");
    system::bind_pd_parked(&producer, producer_pd);
    system::install_pd_cap(producer_pd, 0, signal_cap);
    let producer = system::admit(producer).expect("zulassen");
    let consumer = system::spawn_parked(consumer as *const () as usize, 0, prio).expect("consumer");
    system::bind_pd_parked(&consumer, consumer_pd);
    system::install_pd_cap(consumer_pd, 0, wait_cap);
    let consumer = system::admit(consumer).expect("zulassen");

    // Capability-Transfer: Service-Endpoint + Broker, der dem Client eine
    // svc-Send-Cap per REPLY delegiert.
    let svc_ep = system::create_endpoint().expect("svc ep");
    let svc_root = system::install_endpoint_cap(svc_ep as u32, Rights::RWX).expect("svc cap");
    let svc_recv = system::cap_mint(svc_root, Rights::READ, 0).expect("svc recv");
    let svc_send = system::cap_mint(svc_root, Rights::WRITE, 0).expect("svc send");
    let svc_pd = system::create_pd().expect("svc pd");
    system::install_pd_cap(svc_pd, 0, svc_recv);
    let svc = system::spawn_parked(svc_server as *const () as usize, 0, prio).expect("svc thread");
    let svc = system::admit_in_pd(svc_pd, svc).expect("zulassen");

    let brk_ep = system::create_endpoint().expect("brk ep");
    let brk_root = system::install_endpoint_cap(brk_ep as u32, Rights::RWX).expect("brk cap");
    let brk_recv = system::cap_mint(brk_root, Rights::READ, 0).expect("brk recv");
    let brk_send = system::cap_mint(brk_root, Rights::WRITE, 0).expect("brk send");
    let brk_pd = system::create_pd().expect("brk pd");
    system::install_pd_cap(brk_pd, 0, brk_recv);
    system::install_pd_cap(brk_pd, 2, svc_send); // Slot 2 = die zu delegierende Cap
    *XFER_SRC_CAP.lock() = Some(svc_send); // Quelle der Grant-Ableitungen (Leak-Regression)
    let brk = system::spawn_parked(broker_server as *const () as usize, 0, prio).expect("brk thread");
    let brk = system::admit_in_pd(brk_pd, brk).expect("zulassen");

    let xfer_pd = system::create_pd().expect("xfer pd");
    system::install_pd_cap(xfer_pd, 0, brk_send); // Slot 0 = Broker; Slot 1 wird per Grant gefüllt
    let xfer = system::spawn_parked(xfer_client as *const () as usize, 0, prio).expect("xfer thread");
    let xfer = system::admit_in_pd(xfer_pd, xfer).expect("zulassen");

    // Stateful Hot-Reload: Zähler-Service, dessen Zustand in einer Memory-Region
    // liegt und den Tausch v1(+1) -> v2(+10) überlebt.
    // Zustand in einer Region (Purpose::HotReloadState), Zugriff über die sichere RegionView-API.
    let _cs_state_phys = system::hotreload_state_alloc().expect("cs state");
    let cs_ep = system::create_endpoint().expect("cs ep");
    let cs_root = system::install_endpoint_cap(cs_ep as u32, Rights::RWX).expect("cs cap");
    let cs_send = system::cap_mint(cs_root, Rights::WRITE, 0).expect("cs send");
    let cs_recv1 = system::cap_mint(cs_root, Rights::READ, 0).expect("cs recv1");
    let cs_recv2 = system::cap_mint(cs_root, Rights::READ, 0).expect("cs recv2");
    let cs_v1_pd = system::create_pd().expect("cs v1 pd");
    let cs_v2_pd = system::create_pd().expect("cs v2 pd");
    let cs_client_pd = system::create_pd().expect("cs client pd");
    system::install_pd_cap(cs_v1_pd, EP_CAP as usize, cs_recv1);
    system::install_pd_cap(cs_v2_pd, EP_CAP as usize, cs_recv2);
    system::install_pd_cap(cs_client_pd, EP_CAP as usize, cs_send);
    let cs_v1 = system::spawn_parked(counter_v1 as *const () as usize, 0, prio).expect("cs v1");
    let cs_v1 = system::admit_in_pd(cs_v1_pd, cs_v1).expect("zulassen");
    let csc = system::spawn_parked(cs_client as *const () as usize, 0, prio).expect("cs client");
    let csc = system::admit_in_pd(cs_client_pd, csc).expect("zulassen");
    *CS_RELOAD_INFO.lock() = Some(ReloadInfo {
        ep: cs_ep,
        v1: cs_v1,
        v1_pd: cs_v1_pd,
        v2_pd: cs_v2_pd,
    });

    // EL0-Userland: ein EL1-Server + ein echter EL0-User-Thread, der per Syscall ruft.
    let uep = system::create_endpoint().expect("user ep");
    let uroot = system::install_endpoint_cap(uep as u32, Rights::RWX).expect("user ep cap");
    let usend = system::cap_mint(uroot, Rights::WRITE, 0).expect("user send");
    let urecv = system::cap_mint(uroot, Rights::READ, 0).expect("user recv");
    let usrv_pd = system::create_pd().expect("user server pd");
    system::install_pd_cap(usrv_pd, 0, urecv);
    let usrv = system::spawn_parked(user_server as *const () as usize, 0, prio).expect("user server");
    let usrv = system::admit_in_pd(usrv_pd, usrv).expect("zulassen");
    let user_pd = system::create_pd().expect("user pd");
    system::install_pd_cap(user_pd, 0, usend);
    let ut = system::spawn_user_parked(user_entry as *const () as usize, 0, prio).expect("user thread");
    let ut = system::admit_in_pd(user_pd, ut).expect("zulassen");

    // EL0-Isolation: ein bösartiger EL0-Thread liest EL1-only Kernel-Speicher. Der
    // Kernel muss ihn isolieren (Thread beenden) statt anzuhalten.
    let bad_pd = system::create_pd().expect("bad user pd");
    let bad = system::spawn_user_parked(bad_user as *const () as usize, 0, prio).expect("bad user thread");
    let bad = system::admit_in_pd(bad_pd, bad).expect("zulassen");

    // Strand-7 guard probe: dedizierte Kratz-Wache in GiB 1 + SAS-EL0-Beruehrung.
    // Spaet in der Spawn-Reihe vorbereitet: Zu diesem Zeitpunkt stehen bereits
    // viele Stack-Wachen (echter Vorratsdruck), und die Sonde beweist die Kette
    // Ende-zu-Ende (legen -> stehen (rueckgelesen) -> EL0-Fault mit FAR=Wache).
    match system::guard_sonde_vorbereiten() {
        Some(wache) => {
            let g_pd = system::create_pd().expect("guard probe pd");
            match system::spawn_user_parked(guard_probe as *const () as usize, wache as usize, prio) {
                Some(g) => match system::admit_in_pd(g_pd, g) {
                    Some(tid) => system::guard_sonde_arm(wache, tid.to_raw()),
                    None => println!("guard   : Sonde vorbereitet (Wache={wache:#x}), aber nicht zugelassen"),
                },
                None => println!("guard   : Sonde vorbereitet (Wache={wache:#x}), aber kein Thread entstanden"),
            }
        }
        None => println!("guard   : Sonde NICHT SCHARF (keine Wache legbar — das Urteil faellt auf FAILURES)"),
    }

    // Per-Kern-paralleler Scheduler: je einen Worker auf JEDEN Sekundärkern (1..N)
    // einplanen (die Kerne booten gleich per PSCI und picken ihn auf). Sie laufen
    // parallel zu core 0 — jeder Kern schedult über seine eigene Instanz.
    for c in 1..system::num_cores() {
        system::spawn_on_core(c, xcore_worker as *const () as usize, c, prio);
    }
    // Thread-Migration (ext-30): Treiber auf core 0 (nur der abgebende Kern migriert).
    system::spawn_on_core(0, migrate_driver as *const () as usize, 0, prio);
    // Skalierung (ext-30): viele Threads gleichzeitig verwalten + vollstaendig abbauen.
    system::spawn_on_core(0, scale_driver as *const () as usize, 0, system::IDLE_PRIO);

    // Cross-Core-Wake: ein Parker auf core 1 (höhere Prio als Idle), den core 0
    // später per IPI weckt.
    if let Some(parker) = system::spawn_on_core(1, xcore_parker as *const () as usize, 0, 2) {
        XCORE_PARKER_TID.store(parker.to_raw(), Ordering::Relaxed);
    }

    // Kern-übergreifende synchrone IPC: Server auf core 2, Client auf core 0, ein
    // Endpoint. CALL und REPLY queren die Kerngrenze (Cross-Core-Rendezvous + IPI).
    let xep = system::create_endpoint().expect("xipc ep");
    let xroot = system::install_endpoint_cap(xep as u32, Rights::RWX).expect("xipc ep cap");
    let xsend = system::cap_mint(xroot, Rights::WRITE, 0).expect("xipc send");
    let xrecv = system::cap_mint(xroot, Rights::READ, 0).expect("xipc recv");
    let xsrv_pd = system::create_pd().expect("xipc server pd");
    system::install_pd_cap(xsrv_pd, 0, xrecv);
    let xsrv = system::spawn_on_core_parked(XIPC_SERVER_CORE, xipc_server as *const () as usize, 0, prio)
        .expect("xipc server");
    let xsrv = system::admit_in_pd(xsrv_pd, xsrv).expect("zulassen");
    let xcli_pd = system::create_pd().expect("xipc client pd");
    system::install_pd_cap(xcli_pd, 0, xsend);
    let xcli =
        system::spawn_on_core_parked(0, xipc_client as *const () as usize, 0, prio).expect("xipc client");
    let xcli = system::admit_in_pd(xcli_pd, xcli).expect("zulassen");

    // --- Weg C (Hybrid): isolierte PD vs. SAS-PD lesen dieselbe fremde Adresse X ---
    // X ist eine fremde RAM-Adresse (eigene kleine Region) mit einem Geheimwert. Die
    // MemoryCap wird verworfen -> die Region bleibt belegt (der Allokator reklamiert
    // nur per free), niemand sonst bekommt sie. Die Proben lesen X roh (kein Cap nötig).
    let xaddr = system::alloc(4096, 4096)
        .expect("secret region")
        .region()
        .base;
    poke_u64(xaddr, ISO_SECRET); // Kernel (EL1) schreibt das Geheimnis
    ISO_SECRET_ADDR.store(xaddr, Ordering::Relaxed);

    // Notification + Kollektor; jede Probe bekommt eine SIGNAL-Cap mit eigenem Badge.
    let intf = system::create_notification().expect("iso ntfn");
    let introot = system::install_notification_cap(intf as u32, Rights::RWX).expect("iso ntfn cap");
    let iso_wait = system::cap_mint(introot, Rights::READ, 0).expect("iso wait");
    let icoll_pd = system::create_pd().expect("iso collector pd");
    system::install_pd_cap(icoll_pd, 0, iso_wait);
    let icoll = system::spawn_parked(iso_collector as *const () as usize, 0, prio).expect("iso collector");
    let icoll = system::admit_in_pd(icoll_pd, icoll).expect("zulassen");

    // Vertrauenswürdige SAS-Probe: liest X (erlaubt) und meldet Badge TRUSTED.
    let t_sig = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_TRUSTED).expect("trusted sig");
    let t_pd = system::create_pd().expect("trusted probe pd");
    system::install_pd_cap(t_pd, 0, t_sig);
    let tp = system::spawn_user_parked(trusted_probe as *const () as usize, xaddr as usize, prio)
        .expect("trusted probe");
    let tp = system::admit_in_pd(t_pd, tp).expect("zulassen");

    // Isolierte Probe (eigene VSpace): Slot 0 = Badge RAN, Slot 1 = Badge READ.
    let i_sig0 = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_RAN).expect("iso sig0");
    let i_sig1 = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_READ).expect("iso sig1");
    let i_pd = system::create_pd().expect("iso probe pd");
    system::install_pd_cap(i_pd, 0, i_sig0);
    system::install_pd_cap(i_pd, 1, i_sig1);
    if let Some((ip, _region)) =
        system::spawn_isolated_parked(iso_probe as *const () as usize, xaddr as usize, prio)
    {
        let ip = system::admit_in_pd(i_pd, ip).expect("zulassen");
    }

    // Allgemeiner VMM: eine isolierte PD mappt/entmappt einen **4-KiB**-Frame G per
    // Syscall (cap-gated, feingranular). Slot 0 = Memory-Cap fuer G, Slot 1 = SIGNAL.
    let gframe = system::alloc(4096, 4096).expect("g frame");
    let gbase = gframe.region().base;
    let groot = system::cap_install(gframe).expect("g cap");
    let gmem = system::cap_mint(groot, Rights::WRITE, 0).expect("g mem cap");
    let vmm_sig = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_MAPPED).expect("vmm sig");
    let vmm_pd = system::create_pd().expect("vmm pd");
    system::install_pd_cap(vmm_pd, 0, gmem); // Slot 0 = Memory-Cap (MAP/UNMAP)
    system::install_pd_cap(vmm_pd, 1, vmm_sig); // Slot 1 = SIGNAL (Badge MAPPED)
    if let Some((vp, _)) =
        system::spawn_isolated_parked(vmm_probe as *const () as usize, gbase as usize, prio)
    {
        let vp = system::admit_in_pd(vmm_pd, vp).expect("zulassen");
    }

    // Shared-Memory-IPC: zwei isolierte PDs teilen den Frame F (in BEIDE VSpaces
    // gemappt, identity -> gleiche Adresse). Writer schreibt SHM_SECRET + signalisiert;
    // Reader wartet, liest F und meldet Erfolg. Zero-Copy ueber die Isolationsgrenze,
    // nur ueber cap-gewaehrten Frame + IPC; die VSpaces teilen sonst nichts.
    let fframe =
        system::alloc(hal::mmu::ISO_REGION_SIZE, hal::mmu::ISO_REGION_SIZE).expect("shared F");
    let fbase = fframe.region().base; // Adresse des Frames (identity-VA in beiden PDs)
    let froot = system::cap_install(fframe).expect("F cap"); // EINE Cap fuer denselben Frame
    let shmn = system::create_notification().expect("shm ntfn");
    let shmnroot =
        system::install_notification_cap(shmn as u32, Rights::RWX).expect("shm ntfn cap");
    // Writer-PD: Slot 0 = F-Cap, Slot 1 = shm-SIGNAL (Badge != 0, sonst ginge ein
    // Signal vor dem WAIT des Readers verloren -> pending bliebe 0).
    let w_f = system::cap_mint(froot, Rights::WRITE, 0).expect("w F");
    let w_sig = system::cap_mint(shmnroot, Rights::WRITE, 1).expect("w sig");
    let w_pd = system::create_pd().expect("shm writer pd");
    system::install_pd_cap(w_pd, 0, w_f);
    system::install_pd_cap(w_pd, 1, w_sig);
    if let Some((wp, _)) =
        system::spawn_isolated_parked(shm_writer as *const () as usize, fbase as usize, prio)
    {
        let wp = system::admit_in_pd(w_pd, wp).expect("zulassen");
    }
    // Reader-PD: Slot 0 = F-Cap (derselbe Frame!), Slot 1 = shm-WAIT, Slot 2 = SIGNAL.
    let r_f = system::cap_mint(froot, Rights::WRITE, 0).expect("r F");
    let r_wait = system::cap_mint(shmnroot, Rights::READ, 0).expect("r wait");
    let r_done = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_SHARED).expect("r done");
    let r_pd = system::create_pd().expect("shm reader pd");
    system::install_pd_cap(r_pd, 0, r_f);
    system::install_pd_cap(r_pd, 1, r_wait);
    system::install_pd_cap(r_pd, 2, r_done);
    if let Some((rp, _)) =
        system::spawn_isolated_parked(shm_reader as *const () as usize, fbase as usize, prio)
    {
        let rp = system::admit_in_pd(r_pd, rp).expect("zulassen");
    }

    // Natives Code-Laden: eine isolierte PD fuehrt PRIVAT geladenen Code (Kopie des
    // native_template) aus einer eigenen EL0-RX-Region aus (nicht die geteilte
    // .user_text). Slot 0 = SIGNAL-Cap (Badge NATIVE).
    let nat_sig = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_NATIVE).expect("nat sig");
    let nat_pd = system::create_pd().expect("native pd");
    system::install_pd_cap(nat_pd, 0, nat_sig);
    if let Some(nt) =
        system::spawn_isolated_native_parked(native_template as *const () as *const u8, 64, prio)
    {
        let nt = system::admit_in_pd(nat_pd, nt).expect("zulassen");
    }

    // 4-KiB-Seiten: eine isolierte PD bekommt eine 12-KiB-Region feingranular gemappt
    // -- P (RW), P+4KiB (RO, vorbefuellt), P+8KiB (Guard, ungemappt). Beweist mehrere
    // einzelne Seiten mit gemischten Rechten + Guard-Page-Fault.
    let preg = system::alloc(3 * 4096, 4096)
        .expect("page region")
        .region()
        .base;
    poke_u64(preg + 4096, 0x5EAD_DA7A); // RO-Seite vorbefuellen (Inhalt fuer den Reader)
    let p_sig = system::cap_mint(introot, Rights::WRITE, ISO_BADGE_PAGES).expect("page sig");
    let p_pd = system::create_pd().expect("page pd");
    system::install_pd_cap(p_pd, 0, p_sig);
    if let Some((pp, _)) =
        system::spawn_isolated_parked(page_probe as *const () as usize, preg as usize, prio)
    {
        // **Binden, mappen, DANN zulassen** (D0). Ein `admit_in_pd` an dieser Stelle waere die
        // Behebung mit demselben Fehler drin: die Sonde liefe los, bevor ihre drei Seiten stehen,
        // und faultete auf P statt auf P+8KiB -- der Test misst dann etwas anderes, als er sagt.
        system::bind_pd_parked(&pp, p_pd);
        system::map_into_parked(&pp, preg, 4096, 1); // P: RW
        system::map_into_parked(&pp, preg + 4096, 4096, 0); // P+4KiB: RO
                                                           // P+8KiB bleibt ungemappt (Guard).
        let pp = system::admit(pp).expect("zulassen");
    }

    *RELOAD_INFO.lock() = Some(ReloadInfo {
        ep,
        v1,
        v1_pd,
        v2_pd,
    });

    // Audit-Regression A: Endpoint + 3 PDs (Opfer/Server/Client) für den Stale-Queue-
    // Test. Die Threads selbst werden später vom Idle-Manager gestaffelt erzeugt (siehe
    // demo_report_then_idle), damit das Opfer zuerst blockiert und dann gekillt wird.
    let sep = system::create_endpoint().expect("stale ep");
    let sroot = system::install_endpoint_cap(sep as u32, Rights::RWX).expect("stale ep cap");
    let s_send_v = system::cap_mint(sroot, Rights::WRITE, 0).expect("stale send victim");
    let s_recv = system::cap_mint(sroot, Rights::READ, 0).expect("stale recv");
    let s_send_c = system::cap_mint(sroot, Rights::WRITE, 0).expect("stale send client");
    let v_pd = system::create_pd().expect("stale victim pd");
    system::install_pd_cap(v_pd, EP_CAP as usize, s_send_v);
    let s_pd = system::create_pd().expect("stale server pd");
    system::install_pd_cap(s_pd, EP_CAP as usize, s_recv);
    let c_pd = system::create_pd().expect("stale client pd");
    system::install_pd_cap(c_pd, EP_CAP as usize, s_send_c);
    STALE_VICTIM_PD.store(v_pd, Ordering::Relaxed);
    STALE_SERVER_PD.store(s_pd, Ordering::Relaxed);
    STALE_CLIENT_PD.store(c_pd, Ordering::Relaxed);

    // Audit-Regression C: eigener Endpoint + Server/Client-PD für den Reply-Liveness-
    // Test (Server stirbt nach RECV vor REPLY -> Client muss ERR_SERVER_GONE sehen).
    let rgep = system::create_endpoint().expect("rgone ep");
    let rgroot = system::install_endpoint_cap(rgep as u32, Rights::RWX).expect("rgone ep cap");
    let rg_recv = system::cap_mint(rgroot, Rights::READ, 0).expect("rgone recv");
    let rg_send = system::cap_mint(rgroot, Rights::WRITE, 0).expect("rgone send");
    let rg_s_pd = system::create_pd().expect("rgone server pd");
    system::install_pd_cap(rg_s_pd, EP_CAP as usize, rg_recv);
    let rg_c_pd = system::create_pd().expect("rgone client pd");
    system::install_pd_cap(rg_c_pd, EP_CAP as usize, rg_send);
    RGONE_SERVER_PD.store(rg_s_pd, Ordering::Relaxed);
    RGONE_CLIENT_PD.store(rg_c_pd, Ordering::Relaxed);
    RGONE_EP_ID.store(rgep, Ordering::Relaxed);

    // Budget-Donation-Test: eigener Endpoint + Server/Client-PD (Server unbeschränkt,
    // Client bekommt spaeter ein knappes Budget gebunden).
    let ddep = system::create_endpoint().expect("ddon ep");
    let ddroot = system::install_endpoint_cap(ddep as u32, Rights::RWX).expect("ddon ep cap");
    let dd_recv = system::cap_mint(ddroot, Rights::READ, 0).expect("ddon recv");
    let dd_send = system::cap_mint(ddroot, Rights::WRITE, 0).expect("ddon send");
    let dd_s_pd = system::create_pd().expect("ddon server pd");
    system::install_pd_cap(dd_s_pd, EP_CAP as usize, dd_recv);
    let dd_c_pd = system::create_pd().expect("ddon client pd");
    system::install_pd_cap(dd_c_pd, EP_CAP as usize, dd_send);
    DDON_SERVER_PD.store(dd_s_pd, Ordering::Relaxed);
    DDON_CLIENT_PD.store(dd_c_pd, Ordering::Relaxed);

    // Reply-Cap-Revocation-Test: eigener Endpoint + Server/Client-PD.
    let rcep = system::create_endpoint().expect("rcap ep");
    let rcroot = system::install_endpoint_cap(rcep as u32, Rights::RWX).expect("rcap ep cap");
    let rc_recv = system::cap_mint(rcroot, Rights::READ, 0).expect("rcap recv");
    let rc_send = system::cap_mint(rcroot, Rights::WRITE, 0).expect("rcap send");
    let rc_s_pd = system::create_pd().expect("rcap server pd");
    system::install_pd_cap(rc_s_pd, EP_CAP as usize, rc_recv);
    let rc_c_pd = system::create_pd().expect("rcap client pd");
    system::install_pd_cap(rc_c_pd, EP_CAP as usize, rc_send);
    RCAP_SERVER_PD.store(rc_s_pd, Ordering::Relaxed);
    RCAP_CLIENT_PD.store(rc_c_pd, Ordering::Relaxed);
    RCAP_EP_ID.store(rcep, Ordering::Relaxed);

    // Reply-Cap-Server-Migration-Test: eigener Endpoint + v1-/v2-Server-PD + Client-PD.
    // v1 und v2 bekommen je eine eigene Recv-Cap auf denselben Endpoint (wie beim
    // Hot-Reload); der Client eine Send-Cap.
    let rmep = system::create_endpoint().expect("rmig ep");
    let rmroot = system::install_endpoint_cap(rmep as u32, Rights::RWX).expect("rmig ep cap");
    let rm_recv1 = system::cap_mint(rmroot, Rights::READ, 0).expect("rmig recv v1");
    let rm_recv2 = system::cap_mint(rmroot, Rights::READ, 0).expect("rmig recv v2");
    let rm_send = system::cap_mint(rmroot, Rights::WRITE, 0).expect("rmig send");
    let rm_s_pd = system::create_pd().expect("rmig v1 pd");
    system::install_pd_cap(rm_s_pd, EP_CAP as usize, rm_recv1);
    let rm_v2_pd = system::create_pd().expect("rmig v2 pd");
    system::install_pd_cap(rm_v2_pd, EP_CAP as usize, rm_recv2);
    let rm_c_pd = system::create_pd().expect("rmig client pd");
    system::install_pd_cap(rm_c_pd, EP_CAP as usize, rm_send);
    RMIG_SERVER_PD.store(rm_s_pd, Ordering::Relaxed);
    RMIG_V2_PD.store(rm_v2_pd, Ordering::Relaxed);
    RMIG_CLIENT_PD.store(rm_c_pd, Ordering::Relaxed);
    RMIG_EP_ID.store(rmep, Ordering::Relaxed);
    // Sicherheitsdomänen-Fixtures (ext-22, P1) werden NICHT hier angelegt, sondern LAZY im
    // Manager-Schritt (nach allen Fuzzer-/Reclaim-Tests), damit ihre 2 isolierten EL0-Threads
    // den kstack-Pool/ASIDs der früheren Tests nicht beanspruchen (sonst Pool-/Timing-Races).
}

/// Generischer Hot-Reload-Swap: v1 zurückziehen (Quiesce: Empfänger entfernen +
/// Recv-Cap entziehen), dann v2 (mit Argument) starten und an seine PD binden.
fn reload_swap(info: ReloadInfo, v2_entry: usize, v2_arg: usize) {
    // Reply-Liveness: hatte v1 einen Aufrufer mitten in der Bearbeitung (RECV ohne REPLY),
    // wird dieser beim Quiescen mit ERR_SERVER_GONE entblockt statt zu stranden (v1 wird
    // gleich capless/ersetzt und antwortet nie mehr).
    system::endpoint_quiesce_owner(info.ep, info.v1);
    system::endpoint_retire_receiver(info.ep, info.v1);
    system::clear_pd_cap(info.v1_pd, EP_CAP as usize);
    // Atomar gegen Preemption, damit v2 nicht vor dem Bind läuft.
    hal::cpu::local_irq_disable();
    if let Some(v2) = system::spawn_parked(v2_entry, v2_arg, system::IDLE_PRIO) {
        let v2 = system::admit_in_pd(info.v2_pd, v2).expect("zulassen");
    }
    hal::cpu::local_irq_enable();
}

/// Stateless Hot-Reload (Phase 7): Server v1 (verdoppelt) -> v2 (verdreifacht).
fn do_reload() {
    if let Some(info) = *RELOAD_INFO.lock() {
        reload_swap(info, server_v2 as *const () as usize, 0);
    }
}

/// Stateful Hot-Reload: Zähler-Service v1 (+1) -> v2 (+10); der Zustand (in der Region
/// `system::CS_STATE_REGION`, Purpose::HotReloadState) bleibt erhalten (zero-copy, dieselbe
/// Region). v2 greift über dieselben system::hotreload_state_*-Helfer zu (Arg ungenutzt).
fn do_cs_reload() {
    if let Some(info) = *CS_RELOAD_INFO.lock() {
        reload_swap(info, counter_v2 as *const () as usize, 0);
    }
}

// --- Demo-Threads ---

extern "C" fn worker(arg: usize) -> ! {
    let id = arg;
    loop {
        if id < NWORKERS {
            WORKER_COUNTS[id].fetch_add(1, Ordering::Relaxed);
        }
        for _ in 0..50_000 {
            core::hint::spin_loop();
        }
    }
}

// Die EL0-User-Programme kodieren die Syscall-Nummern als Immediates; hier
// absichern, dass sie zur ABI passen.
const _: () = assert!(
    sys::YIELD == 0
        && sys::EXIT == 6
        && sys::SIGNAL == 8
        && sys::WAIT == 9
        && sys::PARK == 5
        && sys::MAP == 10
        && sys::UNMAP == 11
        // K1a (2026-08-17): eine neue Nummer gehoert in denselben Anker wie die uebrigen --
        // sonst driftet sie beim naechsten Einschub, und EL0-Programme kodieren sie als Immediate.
        && sys::SPAWN == 20
        // B3 (2026-08-26): dieselbe Begruendung wie fuer SPAWN -- eine neue Nummer gehoert in den
        // Anker, sonst driftet sie beim naechsten Einschub.
        && sys::BIND_IRQ == 26
);

/// **EL0-Exiter** (Sektion `.user_text`, EL0-ausführbar): beendet sich sofort per
/// `EXIT`-Syscall. Für den Reclaim-Test transient erzeugt; beim Exit gibt der Kernel
/// seinen EL1-only Kernel-Stack-Pool-Slot zurück, sodass er wiederverwendbar ist.
#[link_section = ".user_text"]
extern "C" fn exiter_entry(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `svc` ist die einzige Kernel-Interaktion.
    unsafe {
        core::arch::asm!(
            "mov x0, #6", // sys::EXIT
            "svc #0",
            options(noreturn),
        );
    }
}

// **EL0-FP-User-Programm** (`user_fp_entry`, Sektion `.user_text`, EL0-ausführbar).
// Lädt das Muster (Argument in x0) in d0..d3 und prüft in jeder Iteration, dass es
// erhalten ist; danach gibt es per `YIELD` ab (der andere FP-Thread stiehlt dabei
// die FP-Register). Nur korrektes Lazy-Save/Restore lässt das Muster jede Abgabe
// überleben. Bei Erfolg `SIGNAL` (Slot 0) an den Kollektor, sonst stilles Parken
// (Test scheitert per Timeout).
//
// Eigener `global_asm!`-Block mit `.arch armv8-a`, damit die `fmov`-Instruktionen
// trotz `-neon`-Kernel assemblieren — eigene Übersetzungseinheit, daher kein
// soft-float/NEON-ABI-Problem (anders als `#[target_feature]`) und kein Leck der
// `.arch`-Direktive in den Modulstrom. Register: x9=Muster, x10=Zähler, x11=Puffer;
// der Kernel-Trap sichert x0..x30, daher überleben sie die `svc`.
core::arch::global_asm!(
    r#"
.arch armv8-a
.section .user_text,"ax"
.globl user_fp_entry
user_fp_entry:
    mov   x9, x0                 // x9 = Muster (Entry-Argument)
    fmov  d0, x9                 // Muster in d0..d3
    fmov  d1, x9
    fmov  d2, x9
    fmov  d3, x9
    mov   x10, #{iters}
20:
    fmov  x11, d0                // Muster prüfen (nach jeder Abgabe erhalten?)
    cmp   x11, x9
    b.ne  30f
    fmov  x11, d1
    cmp   x11, x9
    b.ne  30f
    fmov  x11, d2
    cmp   x11, x9
    b.ne  30f
    fmov  x11, d3
    cmp   x11, x9
    b.ne  30f
    mov   x0, #0                 // sys::YIELD -> FP-Owner abgeben
    svc   #0
    subs  x10, x10, #1
    b.ne  20b
    mov   x0, #8                 // sys::SIGNAL (Slot 0): Erfolg melden
    mov   x1, #0
    svc   #0
10:
    mov   x0, #5                 // sys::PARK (dauerhaft)
    svc   #0
    b     10b
30:
    mov   x0, #5                 // Korruption erkannt: ohne SIGNAL parken
    svc   #0
    b     30b
"#,
    iters = const FP_CHECK_ITERS,
);

extern "C" {
    /// Entry-Symbol des EL0-FP-User-Programms (siehe `global_asm!` oben).
    static user_fp_entry: u8;
}

/// FP-Kollektor (EL1): wartet auf die Erfolgs-Notifications der EL0-FP-Threads und
/// sammelt ihre Badges (Bit i je Thread). Sobald alle gemeldet haben, ist der
/// Lazy-FP-Test bestanden.
extern "C" fn fp_collector(_arg: usize) -> ! {
    loop {
        let r = invoke(sys::WAIT, 0, [0; 4], 0);
        if r.result == result::OK {
            let mask = FP_OK_MASK.fetch_or(r.badge, Ordering::Relaxed) | r.badge;
            if mask == FP_ALL_OK {
                FP_COLLECTOR_DONE.store(true, Ordering::Release);
            }
        }
    }
}

/// Per-Kern-Worker (EL1): läuft auf einem **Sekundärkern** und macht Fortschritt,
/// während core 0 seine eigene Demo abarbeitet — Beleg für parallele Einplanung
/// (jeder Kern schedult über seine eigene Scheduler-Instanz, kein globaler Lock).
/// `arg` = die Kern-ID (Index in `XCORE_PROGRESS`). Danach selbst parken.
extern "C" fn xcore_worker(arg: usize) -> ! {
    let core = arg;
    let mut n = 0;
    while n < SMP_WORK_TARGET {
        // Moderate Arbeit: unter single-threaded QEMU-TCG teilen sich alle Kerne
        // EINE Host-CPU, daher die Sekundärkern-Last bewusst klein halten.
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
        n += 1;
        if core < system::num_cores() {
            XCORE_PROGRESS[core].store(n, Ordering::Relaxed);
        }
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Isolierte EL0-Probe** (`.user_text`, läuft in eigener VSpace). x0 = Adresse X
/// (fremdes RAM). Meldet erst per `SIGNAL` (Slot 0, Badge RAN), dass sie auf EL0 in
/// ihrer VSpace läuft und IPC funktioniert; **dann** liest sie X — das ist in ihrer
/// VSpace EL1-only → Fault → der Kernel beendet sie. Der zweite `SIGNAL` (Slot 1,
/// Badge READ) wird daher NIE erreicht (käme er, wäre die Isolation gebrochen).
#[link_section = ".user_text"]
extern "C" fn iso_probe(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `svc`/`ldr` sind die einzigen Operationen.
    unsafe {
        core::arch::asm!(
            "mov x9, x0", // x9 = X (fremde Adresse)
            "mov x0, #8", // sys::SIGNAL
            "mov x1, #0", // Slot 0 (Badge RAN)
            "svc #0",
            "ldr x2, [x9]", // X lesen -> FAULT in isolierter VSpace (EL1-only)
            "mov x0, #8",   // (unerreichbar bei intakter Isolation) Slot 1 (Badge READ)
            "mov x1, #1",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Vertrauenswürdige EL0-Probe** (`.user_text`, läuft in der globalen SAS-Map).
/// x0 = Adresse X. Liest X (im SAS EL0-RW → erlaubt) und meldet Erfolg per `SIGNAL`
/// (Slot 0, Badge TRUSTED). Kontrast zur isolierten Probe: **dieselbe** Adresse.
#[link_section = ".user_text"]
extern "C" fn trusted_probe(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code (SAS); `ldr` auf X ist hier erlaubt.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",
            "ldr x2, [x9]", // X lesen (im SAS erlaubt)
            "mov x0, #8",   // sys::SIGNAL Slot 0 (Badge TRUSTED)
            "mov x1, #0",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **VMM-Probe** (`.user_text`, isolierte VSpace): demonstriert `map`/`unmap` per
/// Syscall. x0 = Frame-Adresse G (identity). Slot 0 = Memory-Cap für G, Slot 1 =
/// SIGNAL-Cap. Ablauf: `MAP` Slot 0 → G ist EL0-RW → schreibt+liest G (Roundtrip) →
/// `SIGNAL` Badge MAPPED → `UNMAP` Slot 0 → liest G erneut → **Fault** (unmapped) →
/// Kernel beendet sie + baut die VSpace ab. Beweist: map macht G zugänglich, unmap
/// entzieht es wieder — alles cap-gated in der eigenen VSpace.
#[link_section = ".user_text"]
extern "C" fn vmm_probe(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `svc`/`ldr`/`str` auf den selbst gemappten Frame.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",  // x9 = G (Frame-Adresse)
            "mov x0, #10", // sys::MAP
            "mov x1, #0",  // Slot 0 (Memory-Cap für G)
            "svc #0",
            "movz x10, #0xBEEF", // x10 = 0xDEADBEEF (Testmuster)
            "movk x10, #0xDEAD, lsl #16",
            "str x10, [x9]", // in den gemappten Frame schreiben
            "ldr x11, [x9]", // zurücklesen
            "cmp x11, x10",
            "b.ne 3f",    // Roundtrip fehlgeschlagen -> kein SIGNAL
            "mov x0, #8", // sys::SIGNAL Slot 1 (Badge MAPPED)
            "mov x1, #1",
            "svc #0",
            "mov x0, #11", // sys::UNMAP Slot 0
            "mov x1, #0",
            "svc #0",
            "ldr x11, [x9]", // nach UNMAP lesen -> FAULT -> beendet
            "3:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 3b",
            options(noreturn),
        );
    }
}

/// **Shared-Memory-Writer** (`.user_text`, isolierte VSpace). x0 = Adresse des
/// geteilten Frames F. Mappt F (Slot 0 = Memory-Cap), schreibt `SHM_SECRET` hinein
/// und signalisiert (Slot 1, shm-Notification), dass der Reader lesen darf. F ist in
/// **beide** isolierte VSpaces gemappt (identity, gleiche Adresse) -> Zero-Copy über
/// die Isolationsgrenze, ohne dass die VSpaces sonst etwas teilen.
#[link_section = ".user_text"]
extern "C" fn shm_writer(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `str` nur auf den selbst gemappten Shared-Frame.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",
            "mov x0, #10", // sys::MAP Slot 0 (Memory-Cap für F)
            "mov x1, #0",
            "svc #0",
            "movz x10, #0x9ABC", // x10 = SHM_SECRET
            "movk x10, #0x5678, lsl #16",
            "movk x10, #0x1234, lsl #32",
            "movk x10, #0x5A5A, lsl #48",
            "str x10, [x9]", // in den geteilten Frame schreiben
            "mov x0, #8",    // sys::SIGNAL Slot 1 (shm-Notification -> Reader wecken)
            "mov x1, #1",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Shared-Memory-Reader** (`.user_text`, **andere** isolierte VSpace). x0 = F.
/// Mappt F (Slot 0), wartet auf den Writer (Slot 1, `WAIT`), liest F und meldet bei
/// `SHM_SECRET` Erfolg (Slot 2, Badge SHARED an den Kollektor). Beweis: der Reader
/// hat den Wert des Writers über den geteilten Frame gelesen — die VSpaces sind
/// sonst vollständig getrennt, Kommunikation lief über IPC + cap-gewährten Frame.
#[link_section = ".user_text"]
extern "C" fn shm_reader(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `ldr` nur auf den selbst gemappten Shared-Frame.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",
            "mov x0, #10", // sys::MAP Slot 0 (Memory-Cap für F)
            "mov x1, #0",
            "svc #0",
            "mov x0, #9", // sys::WAIT Slot 1 (blockiert bis Writer signalisiert)
            "mov x1, #1",
            "svc #0",
            "ldr x11, [x9]", // geteilten Frame lesen
            "movz x10, #0x9ABC",
            "movk x10, #0x5678, lsl #16",
            "movk x10, #0x1234, lsl #32",
            "movk x10, #0x5A5A, lsl #48",
            "cmp x11, x10",
            "b.ne 3f",
            "mov x0, #8", // sys::SIGNAL Slot 2 (Badge SHARED an Kollektor)
            "mov x1, #2",
            "svc #0",
            "3:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 3b",
            options(noreturn),
        );
    }
}

/// Dummy-Entry für den Churn-Test: wird nie ausgeführt (die PD wird sofort nach dem
/// Spawn wieder zerstört). Nur eine Adresse in `.user_text`.
#[link_section = ".user_text"]
extern "C" fn churn_dummy(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code (unerreichbar).
    unsafe {
        core::arch::asm!("1:", "mov x0, #5", "svc #0", "b 1b", options(noreturn));
    }
}

/// **PD-Management-Ziel** (ext-22, `.user_text`, isolierte UserLand-VSpace). x0 = phys.
/// Basis des Zähler-Frames. Mappt den Frame (Slot 0) und inkrementiert ihn in einer
/// kooperativen Schleife (YIELD je Runde). Der TrustedSas-Controller beobachtet den
/// Zähler und pausiert/setzt fort/stoppt dieses Ziel über `SYS_PDCTL`.
#[link_section = ".user_text"]
extern "C" fn pdctl_target(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `ldr/str` nur auf den selbst gemappten Frame.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",  // x9 = Frame-Basis
            "mov x0, #10", // sys::MAP Slot 0 (Memory-Cap des Zähler-Frames)
            "mov x1, #0",
            "svc #0",
            "1:",
            "ldr x10, [x9]",
            "add x10, x10, #1",
            "str x10, [x9]",
            "mov x0, #0", // sys::YIELD (kooperativ -> Controller kommt dran)
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Kanal-Backend** (ext-22, P3, `.user_text`, isolierte HardwareLand-VSpace): ein minimaler
/// Server, der über seinen **einzigen** Kanal (Slot 0 = Recv-Cap des Partner-Endpoints) Anfragen
/// empfängt und verdoppelt zurückgibt — steht stellvertretend für ein Hardware-Backend, das
/// ausschließlich mit seinem Trusted-Partner spricht.
#[link_section = ".user_text"]
extern "C" fn chan_backend(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; nur RECV/REPLY/PARK-Syscalls über die Kanal-Cap (Slot 0).
    // Ein Austausch (RECV+REPLY), dann PARK (kein RECV-Loop -> kein Busy-Spin bei Fehlern).
    unsafe {
        core::arch::asm!(
            "mov x0, #2", // sys::RECV Slot 0
            "mov x1, #0",
            "svc #0",
            "lsl x2, x2, #1", // Antwort = Eingabe * 2 (CHAN_FACTOR)
            "mov x0, #3",     // sys::REPLY Slot 0
            "mov x1, #0",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Kanal-Trusted-Client** (ext-22, P3, EL1, TrustedSas-Zeitdienst): CALLt das gebundene
/// Backend über die Send-Cap (Slot 0) und hält die Antwort fest (Beleg: der Kanal trägt
/// Anfragen Trusted -> Backend und Antworten zurück).
extern "C" fn chan_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [CHAN_INPUT, 0, 0, 0], 0);
    CHAN_RESULT.store(r.msg[0], Ordering::Release);
    CHAN_CLIENT_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **RTC-Hardware-Backend** (ext-22, P4, `.user_text`, isolierte HardwareLand-VSpace). x0 =
/// EL0-gemappte RTC_DR-Adresse (vom Kernel via MMIO-Cap + vspace_map_device bereitgestellt).
/// Empfaengt eine Anfrage ueber den Kanal (Slot 0), liest das **echte** PL031-Register
/// RTC_DR und liefert den Wert zurueck. Der kleine `unsafe`-Hardwarezugriff (ein `ldr`)
/// ist genau der Teil, der in HardwareLand isoliert wird — die Logik bleibt im Trusted-SAS.
#[link_section = ".user_text"]
extern "C" fn rtc_backend(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; `ldr` nur auf die selbst (cap-autorisiert) gemappte
    // RTC-Registerseite (Device-Memory), RECV/REPLY/PARK ueber die Kanal-Cap (Slot 0).
    unsafe {
        core::arch::asm!(
            "mov x9, x0", // x9 = RTC_DR-Adresse (identity-gemappt)
            "mov x0, #2", // sys::RECV Slot 0 (Anfrage des Zeitdienstes)
            "mov x1, #0",
            "svc #0",
            "ldr w2, [x9]", // RTC_DR lesen (32-bit Sekundenzaehler) -> Antwort-msg0
            "mov x0, #3",   // sys::REPLY Slot 0
            "mov x1, #0",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Trusted-Zeitdienst** (ext-22, P4, EL1, TrustedSas): CALLt das RTC-Backend ueber den
/// gebundenen Kanal und haelt den vom Backend gelesenen RTC-Wert fest.
extern "C" fn rtc_timeservice(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [0, 0, 0, 0], 0);
    RTC_VALUE.store(r.msg[0], Ordering::Release);
    RTC_RES_CODE.store(r.result, Ordering::Release);
    RTC_CLIENT_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **RTC-IRQ-Backend** (ext-22, P5, `.user_text`, isolierte HardwareLand-VSpace). x0 = RW-
/// gemappte RTC-Basis. Empfaengt die Anfrage des Partners (Slot 0), **armiert** den PL031-
/// Match-Interrupt (RTC_MR = RTC_DR+1, RTC_IMSC=1 — kleiner `unsafe`-HW-Zugriff), WAITet auf
/// die Kanal-Notification (Slot 2). Der Kernel stellt den RTC-IRQ als Badge zu -> der WAIT
/// kehrt zurueck; das Backend meldet "IRQ erhalten" (msg0=1) an den Partner (REPLY Slot 0).
#[link_section = ".user_text"]
extern "C" fn rtc_irq_backend(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; Device-Schreib-/Lesezugriffe nur auf die selbst (cap-
    // autorisiert) RW-gemappte RTC-Registerseite; RECV/WAIT/REPLY/PARK ueber die Kanal-Caps.
    unsafe {
        core::arch::asm!(
            "mov x9, x0", // x9 = RTC-Basis (RW)
            "mov x0, #2", // sys::RECV Slot 0 (Anfrage des Partners)
            "mov x1, #0",
            "svc #0",
            "ldr w10, [x9]",     // w10 = RTC_DR
            "add w10, w10, #1",  // Match = DR + 1 (naechste Sekunde)
            "str w10, [x9, #4]", // RTC_MR (Offset 0x04) = Match
            "mov w11, #1",
            "str w11, [x9, #16]", // RTC_IMSC (Offset 0x10) = 1 (Match-Interrupt freigeben)
            "mov x0, #9",         // sys::WAIT Slot 2 (Kanal-Notification = IRQ-Zustellung)
            "mov x1, #2",
            "svc #0",
            "mov x2, #1", // IRQ erhalten -> Antwort-msg0 = 1
            "mov x0, #3", // sys::REPLY Slot 0
            "mov x1, #0",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **DMA-Backend** (ext-23, D0, `.user_text`, isolierte HardwareLand-VSpace). x0 = EL0-RW
/// Normal-Non-Cacheable gemappte DMA-Region-Basis (vom Kernel via DmaCap + vspace_map_dma
/// bereitgestellt). Empfaengt eine Anfrage ueber den Kanal (Slot 0), schreibt zwei bekannte
/// Muster in die DMA-Region (NC-Abbildung), liest Offset 0 zurueck (Round-Trip in EL0) und
/// liefert ihn als Antwort. Der Kernel prueft danach via Identity-Map, dass er dieselben Bytes
/// sieht. Der DMA-Puffer-Zugriff ist genau der kleine HardwareLand-Anteil; keine Treiberlogik.
#[link_section = ".user_text"]
extern "C" fn dma_backend(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; Lese-/Schreibzugriffe nur auf die selbst (cap-autorisiert)
    // EL0-RW gemappte DMA-Region (Normal-NC); RECV/REPLY/PARK ueber die Kanal-Cap (Slot 0).
    unsafe {
        core::arch::asm!(
            "mov x9, x0", // x9 = DMA-Region-Basis (identity, EL0-RW NC)
            "mov x0, #2", // sys::RECV Slot 0 (Anfrage des Partners)
            "mov x1, #0",
            "svc #0",
            "movz w10, #0xBEEF", // w10 = 0x600DBEEF (DMA_PAT0)
            "movk w10, #0x600D, lsl #16",
            "str w10, [x9]",     // DMA[0] = PAT0 (EL0-NC-Schreibzugriff)
            "movz w11, #0xD0DA", // w11 = 0xD0DAD0DA (DMA_PAT1)
            "movk w11, #0xD0DA, lsl #16",
            "str w11, [x9, #4]", // DMA[1] = PAT1
            "dsb sy",            // NC-Schreibvorgaenge sichtbar machen
            "ldr w2, [x9]",      // Round-Trip-Read von Offset 0 -> Antwort-msg0
            "mov x0, #3",        // sys::REPLY Slot 0 (msg0 = zurueckgelesenes PAT0)
            "mov x1, #0",
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// **Trusted-DMA-Dienst** (ext-23, D0, EL1, TrustedSas): CALLt das DMA-Backend ueber den
/// gebundenen Kanal und haelt den vom Backend zurueckgelesenen DMA-Wert fest.
extern "C" fn dma_timeservice(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [0, 0, 0, 0], 0);
    DMA_VALUE.store(r.msg[0], Ordering::Release);
    DMA_RES_CODE.store(r.result, Ordering::Release);
    DMA_CLIENT_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Trusted-Zeitdienst (IRQ-Variante)** (ext-22, P5, EL1, TrustedSas): CALLt das RTC-IRQ-
/// Backend; der CALL kehrt erst zurueck, wenn das Backend den RTC-Interrupt empfangen hat.
extern "C" fn irq_timeservice(_arg: usize) -> ! {
    let r = invoke(sys::CALL, 0, [0, 0, 0, 0], 0);
    IRQT_GOT_VALUE.store(r.msg[0], Ordering::Release);
    IRQT_RES_CODE.store(r.result, Ordering::Release);
    IRQT_CLIENT_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **4-KiB-Seiten-Probe** (`.user_text`, isolierte VSpace). x0 = Basis P einer
/// 12-KiB-Region, vom Kernel feingranular gemappt: P=RW, P+4 KiB=RO (vorbefüllt),
/// P+8 KiB=**Guard** (ungemappt). Die Probe schreibt P (RW), liest P+4 KiB (RO) und
/// meldet Badge PAGES (Beleg: mehrere einzelne Seiten mit gemischten Rechten); dann
/// schreibt sie die Guard-Seite -> **Fault** (ungemappte Seite) -> beendet. Zeigt
/// feingranulare Rechte + Guard Pages.
#[link_section = ".user_text"]
extern "C" fn page_probe(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; Zugriffe nur auf die eigenen gemappten Seiten.
    unsafe {
        core::arch::asm!(
            "mov x9, x0",
            "movz x10, #0x1234",
            "str x10, [x9]",        // P (RW) schreiben
            "ldr x11, [x9, #4096]", // P+4KiB (RO) lesen -> ok
            "mov x0, #8",           // sys::SIGNAL Slot 0 (Badge PAGES)
            "mov x1, #0",
            "svc #0",
            "str x10, [x9, #8192]", // P+8KiB (Guard, ungemappt) schreiben -> FAULT
            "3:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 3b",
            options(noreturn),
        );
    }
}

/// **Natives Programm-Template** (`.user_text`). Diese Bytes werden in einen privaten
/// Code-Frame **kopiert** und dort (EL0-RX, eigene VSpace) ausgeführt — NICHT die
/// geteilte `.user_text`. Es signalisiert (Slot 0, Badge NATIVE) und parkt. Reines
/// position-unabhängiges Inline-Asm (svc + Immediates + relativer Branch), daher an
/// einer beliebigen Adresse lauffähig.
#[link_section = ".user_text"]
extern "C" fn native_template(_arg: usize) -> ! {
    // SAFETY: reiner EL0-User-Code; nur `svc`.
    unsafe {
        core::arch::asm!(
            "mov x0, #8", // sys::SIGNAL
            "mov x1, #0", // Slot 0 (Badge NATIVE)
            "svc #0",
            "1:",
            "mov x0, #5", // sys::PARK
            "svc #0",
            "b 1b",
            options(noreturn),
        );
    }
}

/// Kollektor (EL1) für die Weg-C-Demo: sammelt die Badges der Proben.
extern "C" fn iso_collector(_arg: usize) -> ! {
    loop {
        let r = invoke(sys::WAIT, 0, [0; 4], 0);
        if r.result == result::OK {
            ISO_MASK.fetch_or(r.badge, Ordering::Relaxed);
        }
    }
}

/// Lastausgleich-Worker (EL1): wird lastbewusst auf einem Kern platziert und parkt
/// dort (belegt einen Slot dieses Kerns -> macht die Verteilung sichtbar).
extern "C" fn balanced_worker(_arg: usize) -> ! {
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **MCS-Budget-Worker** (EL1, auf [`MCS_CORE`]): zählt in festen Arbeits-Chunks hoch,
/// solange nicht gestoppt. Bekommt über eine SchedContext-Cap ein knappes CPU-Budget
/// gebunden -> wird bei Budgetende deplaniert, nach der Periode aufgefüllt. Sein
/// Zähler wächst daher viel langsamer als der des unbeschränkten Greedy-Workers.
/// Reiner CPU-Spin (kein YIELD) -> nur der Timer-Tick preemptet ihn und belastet das
/// Budget. Nach dem Stopp parkt er (gibt die CPU frei, hinterlässt keinen Zombie).
extern "C" fn mcs_budgeted_worker(_arg: usize) -> ! {
    while !MCS_STOP.load(Ordering::Relaxed) {
        for _ in 0..5_000 {
            core::hint::spin_loop();
        }
        MCS_BUDGETED_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **MCS-Greedy-Worker** (EL1, auf [`MCS_CORE`]): identischer Arbeits-Chunk, aber
/// **ohne** Budget (unbeschränkt). Läuft jede Tick-Zeitscheibe, in der der budgetierte
/// Thread deplaniert ist -> dominiert die CPU. Referenz für den Drosselungs-Vergleich.
extern "C" fn mcs_greedy_worker(_arg: usize) -> ! {
    while !MCS_STOP.load(Ordering::Relaxed) {
        for _ in 0..5_000 {
            core::hint::spin_loop();
        }
        MCS_GREEDY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Stale-Opfer** (Audit-Regression A, EL1, core 0): CALLt den ungedienten Endpoint
/// (lokaler Slot `EP_CAP`) -> blockiert in `senders`. Wird während der Blockade gekillt;
/// kehrt nie zurück (der `loop` ist nur formal, damit die Signatur `-> !` stimmt).
extern "C" fn stale_victim(_arg: usize) -> ! {
    invoke(sys::CALL, EP_CAP, [0; 4], 0); // blockiert (kein Empfänger); wird gekillt
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Stale-Server** (Audit-Regression A, EL1, core 0): RECV/REPLY-Schleife. Beim ersten
/// RECV muss er den toten Opfer-Eintrag in `senders` überspringen (Fix) statt zu paniken,
/// dann als Empfänger blockieren und schließlich den lebenden Client bedienen (2x Echo).
extern "C" fn stale_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, EP_CAP, [0; 4], 0);
        if m.result != result::OK {
            break;
        }
        invoke(sys::REPLY, EP_CAP, [m.msg[0].wrapping_mul(2), 0, 0, 0], 0);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Stale-Client** (Audit-Regression A, EL1, core 0): CALLt mit `STALE_MAGIC` und hält
/// die Antwort fest. Erfolgt die Antwort (2x `STALE_MAGIC`), hat der Server den toten
/// Eintrag korrekt übersprungen und der Endpoint ist nach dem Kill weiter funktionsfähig.
extern "C" fn stale_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, EP_CAP, [STALE_MAGIC, 0, 0, 0], 0);
    STALE_SERVED.store(r.msg[0], Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Reply-Gone-Server** (Audit-Regression C, EL1): empfängt EINEN Aufrufer (wird damit
/// Reply-Owner) und parkt dann **ohne zu antworten** — simuliert einen Server, der nach
/// RECV, vor REPLY verschwindet. Der Manager killt ihn anschließend.
extern "C" fn rgone_server(_arg: usize) -> ! {
    let _ = invoke(sys::RECV, EP_CAP, [0; 4], 0); // wird Reply-Owner, antwortet NIE
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Reply-Gone-Client** (Audit-Regression C, EL1): CALLt den Server und hält den
/// Ergebniscode fest. Wird der Server vor dem REPLY gekillt, MUSS dieser CALL mit
/// `ERR_SERVER_GONE` zurückkehren (statt dauerhaft zu blockieren).
extern "C" fn rgone_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, EP_CAP, [RGONE_MAGIC, 0, 0, 0], 0);
    RGONE_RESULT.store(r.result, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Reply-Gone-Client Runde 2** (Quiesce-/Reload-Pfad): wie [`rgone_client`], hält das
/// Ergebnis aber in `RGONE_Q_RESULT` fest. Der Server wird hier NICHT gekillt, sondern
/// via `endpoint_quiesce_owner` zurückgezogen (Hot-Reload) — der CALL muss trotzdem mit
/// `ERR_SERVER_GONE` zurückkehren.
extern "C" fn rgone_client2(_arg: usize) -> ! {
    let r = invoke(sys::CALL, EP_CAP, [RGONE_MAGIC, 0, 0, 0], 0);
    RGONE_Q_RESULT.store(r.result, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Donations-Server** (EL1, core 0, unbeschränktes Budget): pro Call verrichtet er
/// Arbeit, die **mindestens einen Timer-Tick überspannt** (Tick-Warten), und antwortet.
/// Bei aktiver Donation wird diese Arbeit gegen das Budget des Aufrufers belastet.
extern "C" fn ddon_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, EP_CAP, [0; 4], 0);
        if m.result != result::OK {
            invoke(sys::YIELD, 0, [0; 4], 0);
            continue;
        }
        // Arbeit über >=1 Tick (so wird das belastete Konto je Call mind. einmal
        // dekrementiert). Wird der Server bei Konto-Erschöpfung deplaniert, friert das
        // Warten ein und setzt nach dem Refill fort.
        let t0 = hal::timer::ticks(0);
        while hal::timer::ticks(0).wrapping_sub(t0) < 2 {
            core::hint::spin_loop();
        }
        invoke(sys::REPLY, EP_CAP, [m.msg[0], 0, 0, 0], 0);
    }
}

/// **Donations-Client** (EL1, core 0, knappes Budget): ruft den Server `DDON_CALLS`-mal
/// und parkt dann (gibt core 0 frei, damit der Idle-Manager das Erschöpfungs-Delta misst).
extern "C" fn ddon_client(_arg: usize) -> ! {
    for _ in 0..DDON_CALLS {
        let _ = invoke(sys::CALL, EP_CAP, [0; 4], 0);
    }
    DDON_CLIENT_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Reply-Cap-Client** (EL1): CALLt den (nie antwortenden) Server und hält das Ergebnis
/// fest. Wird die zugehörige Reply-Cap revoked, MUSS dieser CALL mit `ERR_SERVER_GONE`
/// zurückkehren.
extern "C" fn rcap_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, EP_CAP, [0; 4], 0);
    RCAP_RESULT.store(r.result, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Migrations-Server v1** (EL1): empfängt EINEN Aufrufer (wird Reply-Owner), meldet
/// das per Flag und parkt dann OHNE zu antworten — simuliert einen Server, der mitten in
/// der Bearbeitung eines Calls hot-reloaded wird. Die Antwortpflicht wird anschließend
/// vom Manager auf v2 migriert; v1 antwortet selbst nie.
extern "C" fn rmig_server_v1(_arg: usize) -> ! {
    let m = invoke(sys::RECV, EP_CAP, [0; 4], 0);
    if m.result == result::OK {
        RMIG_RECEIVED.store(true, Ordering::Release);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Migrations-Server v2** (EL1): die neue Server-Instanz nach dem Hot-Reload. Empfängt
/// die migrierte (erneut zugestellte) Nachricht und antwortet mit `RMIG_V2_FACTOR * x` —
/// so beweist der Antwortwert, dass v2 den Call abgeschlossen hat.
extern "C" fn rmig_server_v2(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, EP_CAP, [0; 4], 0);
        if m.result != result::OK {
            break; // Recv-Cap entzogen -> Komponente außer Dienst
        }
        invoke(sys::REPLY, EP_CAP, [RMIG_V2_FACTOR * m.msg[0], 0, 0, 0], 0);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Migrations-Client** (EL1): CALLt einmal und hält Ergebniscode + Antwortwert fest.
/// Wird der bedienende Server mitten im Call hot-reloaded und die Antwortpflicht auf v2
/// migriert, MUSS dieser CALL mit OK + dem von v2 berechneten Wert zurückkehren.
extern "C" fn rmig_client(_arg: usize) -> ! {
    let r = invoke(sys::CALL, EP_CAP, [RMIG_INPUT, 0, 0, 0], 0);
    RMIG_VALUE.store(r.msg[0], Ordering::Release);
    RMIG_RESULT.store(r.result, Ordering::Release); // zuletzt: Manager pollt hierauf
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **PD-Management-Controller** (ext-22, EL1, TrustedSas-PD, hält die `PdControl`-Cap für
/// das UserLand-Ziel in [`PDCTL_CTRL_SLOT`]). Steuert das Ziel über `SYS_PDCTL` und
/// beobachtet dessen Zähler-Frame (peek_u64): erst läuft es, PAUSE friert es ein, RESUME
/// lässt es weiterzählen, ein Slot ohne Cap wird abgelehnt (ERR_BADCAP), STOP beendet es.
extern "C" fn pdctl_controller(_arg: usize) -> ! {
    let f = PDCTL_FRAME.load(Ordering::Acquire); // u64 phys. Basis (peek_u64 nimmt u64)
    let observe = |n: u32| {
        for _ in 0..n {
            invoke(sys::YIELD, 0, [0; 4], 0);
        }
    };
    // 1. Ziel läuft? (Zähler wächst über das Beobachtungsfenster)
    let a = peek_u64(f);
    observe(PDCTL_YIELD_OBS);
    let b = peek_u64(f);
    PDCTL_RAN.store(b > a, Ordering::Release);
    // 2. PAUSE -> eingefroren
    let rp = invoke(sys::PDCTL, PDCTL_CTRL_SLOT, [pdctl::PAUSE, 0, 0, 0], 0);
    let c = peek_u64(f);
    observe(PDCTL_YIELD_OBS);
    let d = peek_u64(f);
    PDCTL_FROZE.store(rp.result == result::OK && d == c, Ordering::Release);
    // 3. RESUME -> wächst wieder
    let _ = invoke(sys::PDCTL, PDCTL_CTRL_SLOT, [pdctl::RESUME, 0, 0, 0], 0);
    let e = peek_u64(f);
    observe(PDCTL_YIELD_OBS);
    let g = peek_u64(f);
    PDCTL_RESUMED.store(g > e, Ordering::Release);
    // 4. Negativ: ein Slot OHNE Cap -> ERR_BADCAP (Steuerung ist cap-gated)
    let rn = invoke(sys::PDCTL, PDCTL_EMPTY_SLOT, [pdctl::PAUSE, 0, 0, 0], 0);
    PDCTL_NOCAP.store(rn.result == result::ERR_BADCAP, Ordering::Release);
    // 5. PAUSE (deplanen) dann STOP (beenden)
    let _ = invoke(sys::PDCTL, PDCTL_CTRL_SLOT, [pdctl::PAUSE, 0, 0, 0], 0);
    observe(PDCTL_YIELD_OBS); // sicherstellen, dass das Ziel deplaniert ist
    let rs = invoke(sys::PDCTL, PDCTL_CTRL_SLOT, [pdctl::STOP, 0, 0, 0], 0);
    PDCTL_STOPPED.store(rs.result == result::OK, Ordering::Release);
    PDCTL_CTRL_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **CAPS-Read-Concurrency-Sonde A** (EL1, [`CAPLK_CORE_A`]): hält den CAPS-Read-Lock und
/// wartet an der Barriere, bis auch Sonde B drin ist. Braucht keine PD (ruft `system::`
/// direkt + PARK). Beweist zusammen mit B die Read-Parallelität des Reader-Writer-Locks.
extern "C" fn caplk_probe_a(_arg: usize) -> ! {
    let ok = system::caps_read_concurrency_probe(CAPLK_WANT, CAPLK_SPIN_LIMIT);
    CAPLK_A_OK.store(ok, Ordering::Release);
    CAPLK_A_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **CAPS-Read-Concurrency-Sonde B** (EL1, [`CAPLK_CORE_B`]): Gegenstück zu A.
extern "C" fn caplk_probe_b(_arg: usize) -> ! {
    let ok = system::caps_read_concurrency_probe(CAPLK_WANT, CAPLK_SPIN_LIMIT);
    CAPLK_B_OK.store(ok, Ordering::Release);
    CAPLK_B_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// **Strand-Worker** (Audit-Regression B, EL1, auf [`STRAND_CORE`]): zählt hoch bis
/// gestoppt. Reiner CPU-Spin (kein YIELD), damit der Timer-Tick das gebundene Budget
/// belastet und der Worker erschöpft. Nach erneutem Bind muss der Zähler wieder wachsen.
extern "C" fn strand_worker(_arg: usize) -> ! {
    while !STRAND_STOP.load(Ordering::Relaxed) {
        STRAND_COUNT.fetch_add(1, Ordering::Relaxed);
        for _ in 0..2_000 {
            core::hint::spin_loop();
        }
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

// (Die generischen Fuzzer-Helfer frand/frand_rights/pick_live/free_slot liegen jetzt im Modul
// `fuzz`, ADR 0013 — sie werden ausschliesslich von den Fuzzern benutzt.)

/// Cross-Core-Park/Wake-Thread (EL1, auf core 1): blockiert sich per `PARK`; core 0
/// weckt ihn kern-übergreifend per `unpark_thread` (Reschedule-IPI). Setzt nach dem
/// Aufwachen `XCORE_WOKEN` — Beleg für den IPI-getriebenen Cross-Core-Unblock.
extern "C" fn xcore_parker(_arg: usize) -> ! {
    XCORE_PARKED.store(true, Ordering::Release);
    invoke(sys::PARK, 0, [0; 4], 0); // blockiert; von core 0 via IPI geweckt
    XCORE_WOKEN.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// Kern-übergreifender IPC-Server (EL1, auf core 2): empfängt per `RECV` (Slot 0)
/// und antwortet mit `XIPC_FACTOR * msg[0]`. Aufrufer liegt auf core 0 -> Nachricht
/// und Antwort queren die Kerngrenze (Cross-Core-Rendezvous + IPI-Wake).
extern "C" fn xipc_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            break;
        }
        invoke(sys::REPLY, 0, [XIPC_FACTOR * m.msg[0], 0, 0, 0], 0);
    }
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// Kern-übergreifender IPC-Client (EL1, auf core 0): ruft den Server auf core 2
/// per `CALL` (Slot 0) und prüft die Antworten.
extern "C" fn xipc_client(_arg: usize) -> ! {
    for (i, &v) in XIPC_IN.iter().enumerate() {
        let r = invoke(sys::CALL, 0, [v, 0, 0, 0], 0);
        XIPC_RESULT[i].store(r.msg[0], Ordering::Relaxed);
    }
    XIPC_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// Zähler-Service v1: liest den Zustand aus der Region, addiert 1, schreibt zurück.
extern "C" fn counter_v1(_arg: usize) -> ! {
    counter_serve(1)
}

/// Zähler-Service v2 (neu geladen): addiert 10 — setzt den Zustand von v1 fort.
///
/// **A-4.3:** v2 *übernimmt* den Zustand, statt ihn vorauszusetzen. Die Übernahme prüft den
/// versionierten Kopf der Region; passt das Layout nicht, bedient v2 **nicht** — die Alternative
/// wäre, die Bytes der alten Fassung im eigenen Sinn zu lesen, und das ist kein Datenverlust,
/// sondern ein fehlinterpretierter Zustand. Der schweigt.
extern "C" fn counter_v2(_arg: usize) -> ! {
    match system::hotreload_state_attach(system::CS_PROGRAM_ID, system::CS_STATE_VERSION) {
        Ok(h) => {
            CS_GENERATION.store(h.generation as u64, Ordering::Release);
            counter_serve(10)
        }
        Err(_) => {
            // Nicht bedienen. Der Client laeuft dann in die leere Antwort und `ckpt` faellt
            // durch -- lauter, als still mit falschem Zustand weiterzuzaehlen.
            loop {
                core::hint::spin_loop();
            }
        }
    }
}

/// Zähler-Service: liest/schreibt den Zustand über die **sichere** RegionView-API
/// (`system::hotreload_state_*`) auf der gemeinsamen Hot-Reload-Region — kein rohes peek/poke
/// (Konsolidierung O-B). v1 und v2 teilen dieselbe Region, daher überlebt der Zähler den Tausch.
fn counter_serve(delta: u64) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            break; // Cap entzogen -> zurückgezogen
        }
        let c = system::hotreload_state_get() + delta;
        system::hotreload_state_set(c);
        invoke(sys::REPLY, 0, [c, 0, 0, 0], 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Zähler-Client: 3 Aufrufe (von v1 bedient), Reload abwarten, 2 weitere (v2).
extern "C" fn cs_client(_arg: usize) -> ! {
    for r in CS_R1.iter() {
        r.store(invoke(sys::CALL, 0, [0; 4], 0).msg[0], Ordering::Relaxed);
    }
    CS_BATCH1_DONE.store(true, Ordering::Release);
    while !CS_RELOADED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    for r in CS_R2.iter() {
        r.store(invoke(sys::CALL, 0, [0; 4], 0).msg[0], Ordering::Relaxed);
    }
    CS_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// EL1-Server für den EL0-User-Thread: empfängt dessen `CALL` und merkt sich den
/// Wert (Beleg, dass der EL0-Syscall durchkam), antwortet leer.
extern "C" fn user_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            break;
        }
        USER_RECV.store(m.msg[0], Ordering::Relaxed);
        invoke(sys::REPLY, 0, [0; 4], 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// **EL0-User-Programm** (eigene Sektion `.user_text`, EL0-ausführbar). Es darf
/// NUR Syscalls ausführen (kein Kernelzugriff): ein `CALL` mit `USER_MAGIC` über
/// den Endpoint an lokalem Cap-Slot 0, danach Selbst-Park. Reines Inline-`svc`,
/// damit kein EL1-Kernelcode aufgerufen wird.
#[link_section = ".user_text"]
extern "C" fn user_entry(_arg: usize) -> ! {
    // SAFETY: EL0-User-Code; `svc` ist die einzige erlaubte Kernel-Interaktion.
    unsafe {
        // CALL(slot 0, msg0 = USER_MAGIC)
        core::arch::asm!(
            "svc #0",
            inout("x0") sys::CALL => _,
            inout("x1") 0u64 => _,
            inout("x2") USER_MAGIC => _,
            lateout("x3") _, lateout("x4") _, lateout("x5") _, lateout("x6") _,
            options(nostack),
        );
        // Danach dauerhaft parken (kein weiterer Kernelzugriff nötig).
        core::arch::asm!(
            "svc #0",
            in("x0") sys::PARK,
            options(noreturn, nostack),
        );
    }
}

/// **Bösartiges EL0-User-Programm** (Sektion `.user_text`, EL0-ausführbar). Es
/// versucht, EL1-only Kernel-Speicher (die Kernel-`.text` an `0x4008_0000`) zu
/// lesen. Da diese Seite nur EL1-Zugriff erlaubt, MUSS das aus EL0 einen Data
/// Abort auslösen. Der Kernel fängt ihn ab, beendet **nur diesen Thread** und
/// läuft weiter — der Nachweis, dass User-Code den Kernel nicht kompromittieren
/// kann. Reines Inline-Asm, kein EL1-Kernelcode.
#[link_section = ".user_text"]
extern "C" fn bad_user(_arg: usize) -> ! {
    // SAFETY: EL0-User-Code. Der Load auf eine EL1-only Adresse faultet
    // garantiert und kehrt nie zurück (Kernel beendet den Thread im Fault-Hook).
    unsafe {
        let kernel_addr: usize = 0x4008_0000; // Kernel-.text (EL1-only gemappt)
        let mut v: u64;
        core::arch::asm!(
            "ldr {v}, [{a}]",
            a = in(reg) kernel_addr,
            v = out(reg) v,
            options(nostack),
        );
        // Wird nie erreicht. Den geladenen Wert "verbrauchen", damit der Compiler
        // den Load nicht wegoptimiert.
        core::arch::asm!("svc #0", in("x0") sys::PARK, in("x2") v, options(nostack));
    }
    loop {
        // SAFETY: Fallback (unerreichbar) — parken statt EL1-Code zu berühren.
        unsafe {
            core::arch::asm!("svc #0", in("x0") sys::PARK, options(nostack));
        }
    }
}

/// **Strand-7 guard probe** (Sektion `.user_text`, EL0-ausführbar, SAS-Domain).
/// Berührt eine dedizierte Kratz-Wache (x0 = Wachseite, x0+4096 = eigene
/// Kontrollseite, abgebildet). Die Kontroll-Ladung MUSS gelingen — sie belegt,
/// dass EL0 den Block lesen kann (Gegenbeleg: der Wachen-Fault kommt von der
/// Wache, nicht von einer fehlenden Abbildung); die Wachen-Ladung MUSS faulten.
/// Erreicht der Thread das PARK, gab es keine Wache (die alte Zaehler-Semantik)
/// — das Urteil faellt dann auf FAILURES, nicht auf Stille.
#[link_section = ".user_text"]
extern "C" fn guard_probe(_arg: usize) -> ! {
    // SAFETY: EL0-User-Code. Die zweite Ladung SOLL faulten (der Kernel beendet
    // den Thread im Fault-Hook und laeuft weiter).
    unsafe {
        core::arch::asm!(
            "mov x9, x0",
            "ldr x2, [x9, #4096]", // Kontrolle: abgebildet -> muss gelingen
            "ldr x3, [x9]",        // Wache: nicht abgebildet -> muss faulten
            "mov x0, #5",          // (nur ohne Wache erreichbar) parken
            "svc #0",
            options(nostack),
        );
    }
    loop {
        // SAFETY: Fallback (nur ohne Wache erreichbar) — parken.
        unsafe {
            core::arch::asm!("svc #0", in("x0") sys::PARK, options(nostack));
        }
    }
}

/// Service-Server: verdreifacht (erreichbar nur über eine transferierte Cap).
extern "C" fn svc_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            break;
        }
        invoke(sys::REPLY, 0, [3 * m.msg[0], 0, 0, 0], 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Broker-Server: delegiert dem Aufrufer per REPLY-Grant seine svc-Send-Cap (Slot 2).
extern "C" fn broker_server(_arg: usize) -> ! {
    loop {
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result != result::OK {
            break;
        }
        // REPLY mit Grant der Cap an lokalem Slot 2.
        invoke(sys::REPLY, 0, [0; 4], GRANT_FLAG | 2);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Transfer-Client: holt sich beim Broker eine svc-Cap und nutzt sie dann.
extern "C" fn xfer_client(_arg: usize) -> ! {
    // 1) Broker rufen -> erhält per Grant eine svc-Send-Cap an GRANT_RECV_SLOT.
    let _ = invoke(sys::CALL, 0, [0; 4], 0);
    // 2) Service über die transferierte Cap rufen (vorher kein Zugriff).
    let r = invoke(sys::CALL, GRANT_RECV_SLOT as u64, [7, 0, 0, 0], 0);
    XFER_RESULT.store(r.msg[0], Ordering::Relaxed);
    XFER_DONE.store(true, Ordering::Release);

    // 3) Leak-Regression: denselben Empfangs-Slot wieder und wieder per Grant befuellen.
    //    Jede Wiederholung verdraengt die vorige Ableitung — der Kernel MUSS sie freigeben.
    for _ in 0..XFER_GRANT_LOOPS {
        let _ = invoke(sys::CALL, 0, [0; 4], 0);
    }
    // 4) Die (neueste) transferierte Cap muss weiterhin funktionieren ...
    let r2 = invoke(sys::CALL, GRANT_RECV_SLOT as u64, [7, 0, 0, 0], 0);
    XFER_RESULT_AFTER.store(r2.msg[0], Ordering::Relaxed);
    // ... und die Quell-Cap darf trotz {XFER_GRANT_LOOPS}+1 Grants nur EINE lebende
    //     Ableitung haben (dieser Thread laeuft auf EL1 -> direkter Kernel-Zugriff).
    let children = XFER_SRC_CAP
        .lock()
        .and_then(system::cap_inspect)
        .map(|i| i.child_count)
        .unwrap_or(usize::MAX);
    XFER_SRC_CHILDREN.store(children, Ordering::Relaxed);
    XFER_GRANTLK_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Producer: signalisiert die Notification asynchron, bis der Consumer fertig ist.
extern "C" fn producer(_arg: usize) -> ! {
    while !CONSUMER_DONE.load(Ordering::Acquire) {
        let _ = invoke(sys::SIGNAL, 0, [0; 4], 0); // Notification-Cap an Slot 0
        for _ in 0..250_000 {
            core::hint::spin_loop();
        }
    }
    PRODUCER_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Consumer: wartet `NOTIF_ROUNDS`-mal auf die Notification und prüft den Badge.
extern "C" fn consumer(_arg: usize) -> ! {
    let mut n: u64 = 0;
    while n < NOTIF_ROUNDS {
        let r = invoke(sys::WAIT, 0, [0; 4], 0);
        NOTIF_GOT_BADGE.store(r.badge, Ordering::Relaxed);
        n += 1;
        NOTIF_COUNT.store(n, Ordering::Relaxed);
    }
    CONSUMER_DONE.store(true, Ordering::Release);
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Victim: zählt fortlaufend hoch, bis es (cap-kontrolliert) getötet wird.
extern "C" fn victim(_arg: usize) -> ! {
    loop {
        VICTIM_COUNT.fetch_add(1, Ordering::Relaxed);
        for _ in 0..2_000 {
            core::hint::spin_loop();
        }
    }
}

/// Killer (PD mit Tcb-Cap): tötet das Victim, prüft das Einfrieren + den
/// Negativfall (KILL ohne Cap), und beendet sich danach selbst (EXIT).
extern "C" fn killer(_arg: usize) -> ! {
    for _ in 0..1_000_000 {
        core::hint::spin_loop(); // Victim etwas laufen lassen
    }
    // KILL über die Tcb-Cap an Slot 0.
    let _ = invoke(sys::KILL, 0, [0; 4], 0);
    KILL_SNAP1.store(VICTIM_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
    // Negativtest: KILL über leeren Slot -> verweigert.
    DENIED_KILL.store(
        invoke(sys::KILL, NO_CAP_SLOT, [0; 4], 0).result,
        Ordering::Relaxed,
    );
    // Warten und erneut messen: das Victim darf nicht weitergezählt haben.
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }
    KILL_SNAP2.store(VICTIM_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
    KILLER_DONE.store(true, Ordering::Release);
    // Selbst beenden -> Stack/TCB werden zurückgewonnen.
    invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Prioritätstest-Thread: feste Arbeit, dann Reihenfolge festhalten und parken.
extern "C" fn prio_thread(arg: usize) -> ! {
    let id = arg;
    let mut i: u64 = 0;
    while i < PRIO_WORK {
        core::hint::spin_loop();
        i += 1;
    }
    if id < NPRIO_TEST {
        let pos = PRIO_SEQ.fetch_add(1, Ordering::AcqRel);
        PRIO_FINISH[id].store(pos, Ordering::Relaxed);
        PRIO_DONE[id].store(true, Ordering::Release);
    }
    // Selbst-parken: gibt die CPU dauerhaft an niedriger priorisierte Threads frei.
    invoke(sys::PARK, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Server v1: verdoppelt. Bei entzogener Cap (Recv-Fehler) zieht er sich zurück.
extern "C" fn server_v1(_arg: usize) -> ! {
    serve(2)
}

/// Server v2: verdreifacht (das „neu geladene" Modul).
extern "C" fn server_v2(_arg: usize) -> ! {
    serve(3)
}

/// Gemeinsame Server-Schleife: empfangen, mit `factor` multiplizieren, antworten.
fn serve(factor: u64) -> ! {
    loop {
        let m = invoke(sys::RECV, EP_CAP, [0; 4], 0);
        if m.result != result::OK {
            break; // Cap entzogen -> Komponente zurückgezogen
        }
        invoke(sys::REPLY, EP_CAP, [factor * m.msg[0], 0, 0, 0], 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Client: Batch 1 (bedient von v1), dann auf den Hot-Reload warten, dann
/// Batch 2 (bedient von v2) — durchgehend über dieselbe Send-Cap.
extern "C" fn client(_arg: usize) -> ! {
    for (i, &v) in B1_IN.iter().enumerate() {
        R1[i].store(
            invoke(sys::CALL, EP_CAP, [v, 0, 0, 0], 0).msg[0],
            Ordering::Relaxed,
        );
    }
    BATCH1_DONE.store(true, Ordering::Release);

    while !RELOADED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    for (i, &v) in B2_IN.iter().enumerate() {
        R2[i].store(
            invoke(sys::CALL, EP_CAP, [v, 0, 0, 0], 0).msg[0],
            Ordering::Relaxed,
        );
    }
    ALL_DONE.store(true, Ordering::Release);
    loop {
        core::hint::spin_loop();
    }
}

// --- Reload-Manager + Bericht (Idle-Thread des Primärkerns) ---

// `reported = true` (defensive Re-Report-Sperre) ist nach `report()` tot, weil danach IMMER
// `system_off()` bzw. `soak::run()` (beide `-> !`) divergieren -> die Schleife kehrt nie zurueck,
// der Wert wird nie wieder gelesen. Die Markierung bleibt dennoch (Intent + Robustheit, falls die
// Divergenz-Annahme je entfaellt) -> das `unused_assignments`-Warning hier bewusst erlauben.
#[allow(unused_assignments)]
pub fn demo_report_then_idle() -> ! {
    let mut reloaded = false;
    let mut cs_reloaded = false;
    let mut reported = false;
    let mut dbg_ticks = 0u32;
    let mut dbg_printed = false;
    // **Ein zweiter Schnappschuss, kurz VOR der Notbremse** (D6, 2026-08-02).
    //
    // Die Zeile darunter lief bisher genau einmal, nach ~25 s. Im beobachteten Haenger ist das zu
    // frueh: dort sind am Ende **alle** Ergebniszeilen gruen, der Bericht kommt trotzdem aus der
    // Notbremse, und der 25-s-Schnappschuss zeigt lauter Tests, die danach noch fertig wurden.
    // Damit ist nicht zu sagen, welche Bedingung bei Ablauf offen war -- und ohne das ist jede
    // Ursachensuche ein Raten. Auf x86 nennt die Notbremse die offenen Punkte seit heute; das
    // hier ist das ARM-Gegenstueck mit den Mitteln, die es dort schon gibt.
    let mut dbg_printed_wd = false;
    // (Die Fuzzer-Spawn-Flags liegen jetzt als Statics im Modul `fuzz`, ADR 0013.)
    loop {
        // Diagnose: falls der Bericht ausbleibt, nach ~25 s die ausstehenden
        // Bedingungen einmalig ausgeben (zeigt, welcher Test haengt).
        dbg_ticks += 1;
        // Kurz vor der Deadline (6000 Ticks) noch einmal -- der Schnappschuss soll den Stand ZUM
        // ZEITPUNKT DER BREMSE zeigen, nicht den von vor 35 Sekunden.
        let kurz_vor_der_bremse = hal::timer::ticks(0) > 5990;
        if !reported
            && ((!dbg_printed && dbg_ticks > 250) || (!dbg_printed_wd && kurz_vor_der_bremse))
        {
            if kurz_vor_der_bremse {
                dbg_printed_wd = true;
            } else {
                dbg_printed = true;
            }
            let m = ISO_MASK.load(Ordering::Relaxed);
            println!("DBG pending: workers={} fp={} prio={} life={} notif={} xfer={} ckpt={} el0={} smp={} xipc={} reclaim={} balance={} vspace={} vmm={} shm={} native={} pages4k={} churn={} mcs={} stale={} strand={} rgone={} ddon={} rcap={} rmig={} fuzz={} ipcfuzz={} caplk={} domain={} pdctl={} chan={} rtc={} irq={} dma={} pcie={} smmu={} smmubind={} virtiorng={} dmagen={} sasheap={} load={} sysload={} loadhw={} loadstop={} loaderfuzz={} aggru={} intru={} aggrh={} intrh={} aggrt={} intrt={} cross={} hwfuzz={} guard={}",
                (0..NWORKERS).all(|i| WORKER_COUNTS[i].load(Ordering::Relaxed) >= THRESHOLD),
                FP_COLLECTOR_DONE.load(Ordering::Acquire) && system::fp_switch_count() > 0,
                (0..NPRIO_TEST).all(|i| PRIO_DONE[i].load(Ordering::Acquire)),
                KILLER_DONE.load(Ordering::Acquire) && REAPED.load(Ordering::Relaxed) >= 2,
                PRODUCER_DONE.load(Ordering::Acquire) && NOTIF_COUNT.load(Ordering::Relaxed) >= NOTIF_ROUNDS,
                XFER_DONE.load(Ordering::Acquire),
                CS_DONE.load(Ordering::Acquire),
                USER_RECV.load(Ordering::Relaxed) == USER_MAGIC && system::el0_syscall_seen(),
                (1..system::num_cores()).all(|c| XCORE_PROGRESS[c].load(Ordering::Relaxed) >= SMP_WORK_TARGET) && XCORE_WOKEN.load(Ordering::Acquire),
                XIPC_DONE.load(Ordering::Acquire),
                RECLAIM_SPAWNED.load(Ordering::Relaxed) >= RECLAIM_TARGET,
                BALANCED_SPAWNED.load(Ordering::Relaxed) >= BALANCED_TARGET,
                (m & ISO_BADGE_RAN != 0) && (m & ISO_BADGE_TRUSTED != 0) && system::iso_faulted(),
                (m & ISO_BADGE_MAPPED != 0) && system::iso_fault_count() >= 2,
                m & ISO_BADGE_SHARED != 0,
                m & ISO_BADGE_NATIVE != 0,
                m & ISO_BADGE_PAGES != 0,
                CHURN_DONE.load(Ordering::Acquire) && CHURN_OK.load(Ordering::Acquire),
                MCS_DONE.load(Ordering::Acquire) && MCS_OK.load(Ordering::Acquire),
                STALE_DONE.load(Ordering::Acquire) && STALE_OK.load(Ordering::Acquire),
                STRAND_DONE.load(Ordering::Acquire) && STRAND_OK.load(Ordering::Acquire),
                RGONE_DONE.load(Ordering::Acquire) && RGONE_OK.load(Ordering::Acquire),
                DDON_DONE.load(Ordering::Acquire) && DDON_OK.load(Ordering::Acquire),
                RCAP_DONE.load(Ordering::Acquire) && RCAP_OK.load(Ordering::Acquire),
                RMIG_DONE.load(Ordering::Acquire) && RMIG_OK.load(Ordering::Acquire),
                fuzz::dbg_fuzz(),
                fuzz::dbg_ipcfuzz(),
                CAPLK_DONE.load(Ordering::Acquire) && CAPLK_OK.load(Ordering::Acquire),
                DOMAIN_DONE.load(Ordering::Acquire) && DOMAIN_OK.load(Ordering::Acquire),
                PDCTL_DONE.load(Ordering::Acquire) && PDCTL_OK.load(Ordering::Acquire),
                CHAN_DONE.load(Ordering::Acquire) && CHAN_OK.load(Ordering::Acquire),
                RTC_DONE.load(Ordering::Acquire) && RTC_OK.load(Ordering::Acquire),
                IRQT_DONE.load(Ordering::Acquire) && IRQT_OK.load(Ordering::Acquire),
                DMA_DONE.load(Ordering::Acquire) && DMA_OK.load(Ordering::Acquire),
                PCIE_DONE.load(Ordering::Acquire) && PCIE_OK.load(Ordering::Acquire),
                SMMU_DONE.load(Ordering::Acquire) && SMMU_OK.load(Ordering::Acquire),
                SMMUB_DONE.load(Ordering::Acquire) && SMMUB_OK.load(Ordering::Acquire),
                VRNG_DONE.load(Ordering::Acquire) && VRNG_OK.load(Ordering::Acquire),
                DMAGEN_DONE.load(Ordering::Acquire) && DMAGEN_OK.load(Ordering::Acquire),
                SASHEAP_DONE.load(Ordering::Acquire) && SASHEAP_OK.load(Ordering::Acquire),
                LOAD_DONE.load(Ordering::Acquire) && LOAD_OK.load(Ordering::Acquire),
                SYSLOAD_FIN.load(Ordering::Acquire) && SYSLOAD_OK.load(Ordering::Acquire),
                LOADHW_FIN.load(Ordering::Acquire) && LOADHW_OK.load(Ordering::Acquire),
                LOADSTOP_DONE.load(Ordering::Acquire) && LOADSTOP_OK.load(Ordering::Acquire),
                fuzz::dbg_loaderfuzz(),
                AGGRU_DONE.load(Ordering::Acquire) && AGGRU_OK.load(Ordering::Acquire),
                INTRU_DONE.load(Ordering::Acquire) && INTRU_OK.load(Ordering::Acquire),
                AGGRH_DONE.load(Ordering::Acquire) && AGGRH_OK.load(Ordering::Acquire),
                INTRH_DONE.load(Ordering::Acquire) && INTRH_OK.load(Ordering::Acquire),
                AGGRT_DONE.load(Ordering::Acquire) && AGGRT_OK.load(Ordering::Acquire),
                INTRT_DONE.load(Ordering::Acquire) && INTRT_OK.load(Ordering::Acquire),
                CROSS_DONE.load(Ordering::Acquire) && CROSS_OK.load(Ordering::Acquire),
                fuzz::dbg_hwfuzz(),
                system::guard_sonde_urteil().3,
            );
        }
        // Beendete Threads einsammeln (sicher: läuft auf dem Idle-Stack).
        let n = system::reap();
        if n > 0 {
            REAPED.fetch_add(n as u64, Ordering::Relaxed);
        }
        if !reloaded && BATCH1_DONE.load(Ordering::Acquire) {
            do_reload();
            RELOADED.store(true, Ordering::Release);
            reloaded = true;
        }
        if !cs_reloaded && CS_BATCH1_DONE.load(Ordering::Acquire) {
            do_cs_reload();
            CS_RELOADED.store(true, Ordering::Release);
            cs_reloaded = true;
        }
        // Cross-Core-Wake: sobald der Parker (auf core 1) blockiert ist, ihn per
        // IPI wecken. Idempotent + Wiederholung pro Tick.
        //
        // **`unpark_thread`, nicht `wake_remote`** (Z24, 2026-08-10). Der Parker schläft wegen
        // `SYS_PARK`; `wake_remote` ist der Wecker für eine **IPC**-Blockade. Bis zur Grund-Menge
        // war das dieselbe Operation — ein Bit, drei Bedeutungen —, und dieser Test hat genau
        // davon gelebt: er weckte einen geparkten Thread mit einem Wecker, der ihn gar nicht
        // meinte. Gefunden hat es nicht das Gegenlesen, sondern die aarch64-Suite, die nach dem
        // Umbau in **4 von 4** Läufen `offen: smp` meldete, während x86 grün blieb (dort gibt es
        // diesen Pfad nicht).
        //
        // Nebenertrag: die Weckmarke macht das Rennen, das der alte Kommentar noch mit
        // „Wiederholung pro Tick" abfing, **strukturell** unmöglich — `unpark` legt sie auch ab,
        // wenn der Parker noch nicht schläft.
        if XCORE_PARKED.load(Ordering::Acquire) && !XCORE_WOKEN.load(Ordering::Acquire) {
            let raw = XCORE_PARKER_TID.load(Ordering::Relaxed);
            if raw != u64::MAX {
                system::unpark_thread(ThreadId::from_raw(raw));
            }
        }

        // Reclaim-Test: nach dem FP-Test transiente EL0-Exiter spawnen — insgesamt
        // mehr (RECLAIM_TARGET) als der Kernel-Stack-Pool gleichzeitig fasst. Nur ein
        // freier Slot wird belegt; jeder Exiter beendet sich (Prio 6 -> sofort) und
        // gibt seinen Slot zurück. Gelingen alle TARGET Spawns, funktioniert Reclaim.
        if FP_COLLECTOR_DONE.load(Ordering::Acquire)
            && RECLAIM_SPAWNED.load(Ordering::Relaxed) < RECLAIM_TARGET
        {
            // Pro Tick alle gerade freien Slots belegen (Batch); die Exiter (Prio 6)
            // beenden sich beim nächsten wfi-Tick und geben ihre Slots zurück.
            // Atomar gegen Preemption (Locks im Thread-Kontext), wie beim Reload.
            hal::cpu::local_irq_disable();
            while RECLAIM_SPAWNED.load(Ordering::Relaxed) < RECLAIM_TARGET
                && system::user_kstack_free_count() > 0
            {
                if system::spawn_user(exiter_entry as *const () as usize, 0, RECLAIM_PRIO).is_some()
                {
                    RECLAIM_SPAWNED.fetch_add(1, Ordering::Relaxed);
                } else {
                    break;
                }
            }
            hal::cpu::local_irq_enable();
        }

        // Lastausgleich: nach dem Reclaim-Test BALANCED_TARGET Worker lastbewusst
        // platzieren. Jeder landet auf dem gerade am wenigsten belasteten Kern (die
        // Last steigt mit jedem Spawn) -> Verteilung über die Kerne statt alle auf
        // dem Bootkern. Wir merken uns den gewählten Kern je Worker.
        if RECLAIM_SPAWNED.load(Ordering::Relaxed) >= RECLAIM_TARGET
            && BALANCED_SPAWNED.load(Ordering::Relaxed) < BALANCED_TARGET
        {
            hal::cpu::local_irq_disable();
            while BALANCED_SPAWNED.load(Ordering::Relaxed) < BALANCED_TARGET {
                match system::spawn_balanced(
                    balanced_worker as *const () as usize,
                    0,
                    system::IDLE_PRIO,
                ) {
                    Some(t) => {
                        if let Some(c) = system::owner_core_of(t) {
                            BALANCED_PLACE[c].fetch_add(1, Ordering::Relaxed);
                        }
                        BALANCED_SPAWNED.fetch_add(1, Ordering::Relaxed);
                    }
                    None => break,
                }
            }
            hal::cpu::local_irq_enable();
        }
        // Churn-/Leak-Test: tausende spawn/destroy-Zyklen isolierter PDs (auf core 0) und pruefen,
        // dass ALLE Ressourcenstaende exakt zur Baseline zurueckkehren. BUGFIX (nach Burn-in #1,
        // seltener Hang ~1/1000): `local_irq_disable()` stoppt NUR core 0 — die anderen Kerne
        // (reclaim/balance) hinterlassen kurzlebige Threads/Zombies auf IHREN Kernen, deren
        // spaeteres Reap die GLOBALEN Zaehler (total_free/free_vspaces/kstack) waehrend der Messung
        // perturbiert -> Schein-Leak -> CHURN_OK=false -> all_done() nie true -> Hang. Fix: gegate
        // auf reclaim+balance vollstaendig gespawnt UND VOR der Baseline ueber ALLE Kerne
        // quieszieren (reap_core ist kern-uebergreifend), bis die globalen Zaehler stabil sind.
        if !CHURN_DONE.load(Ordering::Acquire)
            && (ISO_MASK.load(Ordering::Relaxed) & ISO_BADGE_PAGES != 0)
            && system::iso_fault_count() >= 3
            && RECLAIM_SPAWNED.load(Ordering::Relaxed) >= RECLAIM_TARGET
            && BALANCED_SPAWNED.load(Ordering::Relaxed) >= BALANCED_TARGET
            // ext-30: auch der Skalierungstest muss durch sein. Er erzeugt/beendet 1024 Threads
            // ueber ALLE Kerne — deren Reaping wuerde die globalen Zaehler waehrend der
            // Churn-Messung perturbieren (exakt der oben beschriebene Schein-Leak).
            && SCALE_DONE.load(Ordering::Acquire)
        {
            hal::cpu::local_irq_disable();
            let snap = || {
                (
                    system::total_free(),
                    system::used_tcbs(0),
                    system::free_vspaces(),
                    system::user_kstack_free_count(),
                )
            };
            // Alle Kerne einsammeln (auch fremde Zombies) + settlen, bis sich die globalen Zaehler
            // ueber eine Runde nicht mehr aendern. Bounded -> bleibt es instabil, faengt der
            // Watchdog (kein Hang). Erst danach die Baseline schnappen.
            let drain_all = || {
                let mut t = 0usize;
                for c in 0..system::num_cores() {
                    t += system::reap_core(c);
                }
                t
            };
            while drain_all() > 0 {}
            let mut base = snap();
            for _ in 0..64u32 {
                for _ in 0..20_000 {
                    core::hint::spin_loop();
                }
                while drain_all() > 0 {}
                let now = snap();
                if now == base {
                    break; // quiesziert: keine Perturbation durch andere Kerne mehr
                }
                base = now;
            }
            let (mem0, tcb0, vs0, ks0) = base;
            let mut ok = true;
            for _ in 0..CHURN_TARGET {
                match system::spawn_isolated(
                    churn_dummy as *const () as usize,
                    0,
                    system::IDLE_PRIO,
                ) {
                    Some((tid, _)) => system::destroy_isolated(tid),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            while drain_all() > 0 {} // churn-eigene Zombies (alle Kerne) einsammeln
            let leak = system::total_free() != mem0
                || system::used_tcbs(0) != tcb0
                || system::free_vspaces() != vs0
                || system::user_kstack_free_count() != ks0;
            hal::cpu::local_irq_enable();
            CHURN_OK.store(ok && !leak, Ordering::Release);
            CHURN_DONE.store(true, Ordering::Release);
        }

        // MCS Scheduling Contexts: nach dem Churn-Test und sobald die Sekundärkern-
        // Worker fertig sind (MCS_CORE damit frei), zwei gleichprioritäre Threads auf
        // MCS_CORE konkurrieren lassen — einer budgetiert (Budget per SchedContext-Cap
        // gebunden), einer unbeschränkt. Greedy zuerst spawnen, dann den budgetierten
        // (er bekommt das Budget erst beim Bind). Autorität ausschließlich über die Cap.
        if !MCS_STARTED.load(Ordering::Acquire)
            && CHURN_DONE.load(Ordering::Acquire)
            && (1..system::num_cores())
                .all(|c| XCORE_PROGRESS[c].load(Ordering::Relaxed) >= SMP_WORK_TARGET)
        {
            hal::cpu::local_irq_disable();
            let greedy = system::spawn_on_core(
                MCS_CORE,
                mcs_greedy_worker as *const () as usize,
                0,
                MCS_PRIO,
            );
            let budg = system::spawn_on_core(
                MCS_CORE,
                mcs_budgeted_worker as *const () as usize,
                0,
                MCS_PRIO,
            );
            if let (Some(_g), Some(b)) = (greedy, budg) {
                // Budget ausschließlich über eine SchedContext-Cap vergeben: Cap prägen
                // (Budget/Periode), dann an den Thread binden. Ohne Cap kein Budget.
                if let Ok(sc) =
                    system::install_sched_context_cap(MCS_BUDGET, MCS_PERIOD, Rights::WRITE)
                {
                    MCS_BOUND.store(
                        system::bind_sched_context(sc, MCS_CORE, b),
                        Ordering::Release,
                    );
                }
                MCS_START_TICK.store(hal::timer::ticks(MCS_CORE), Ordering::Relaxed);
                MCS_STARTED.store(true, Ordering::Release);
            }
            hal::cpu::local_irq_enable();
        }

        // MCS-Auswertung: genug Ticks + mehrere Erschöpfungs-/Refill-Zyklen abwarten,
        // dann beide Threads stoppen und prüfen, dass der budgetierte Thread Fortschritt
        // machte (garantierte CPU), aber deutlich gedrosselt unter dem Greedy-Thread lag.
        if MCS_STARTED.load(Ordering::Acquire) && !MCS_DONE.load(Ordering::Acquire) {
            let (depl, refl) = system::budget_stats(MCS_CORE);
            let elapsed =
                hal::timer::ticks(MCS_CORE).wrapping_sub(MCS_START_TICK.load(Ordering::Relaxed));
            if elapsed >= MCS_SETTLE_TICKS && depl >= MCS_MIN_CYCLES && refl >= MCS_MIN_CYCLES {
                MCS_STOP.store(true, Ordering::Release); // beide Threads parken lassen
                let bg = MCS_BUDGETED_COUNT.load(Ordering::Relaxed);
                let gr = MCS_GREEDY_COUNT.load(Ordering::Relaxed);
                // Budgetierter Thread: garantierter (bg > 0), aber stark begrenzter
                // Fortschritt (mind. 3x weniger als der unbeschränkte Greedy-Thread).
                let throttled = bg > 0 && bg.saturating_mul(3) < gr;
                MCS_OK.store(
                    MCS_BOUND.load(Ordering::Acquire) && throttled,
                    Ordering::Release,
                );
                MCS_DONE.store(true, Ordering::Release);
            }
        }

        // Audit-Regression A (IPC-Stale-Queue): gestaffelt, gegate auf MCS fertig + SMP
        // fertig (core 0 frei von höher-prioren Workern -> der Idle-Manager läuft erst,
        // wenn der gerade erzeugte prio-3-Thread blockiert hat -> deterministische
        // Sequenz ohne Timing-Annahmen). Jeder Schritt: eine Aktion pro Manager-Runde.
        if !STALE_DONE.load(Ordering::Acquire)
            && MCS_DONE.load(Ordering::Acquire)
            && (1..system::num_cores())
                .all(|c| XCORE_PROGRESS[c].load(Ordering::Relaxed) >= SMP_WORK_TARGET)
        {
            match STALE_STEP.load(Ordering::Acquire) {
                0 => {
                    // Opfer erzeugen + an seine PD binden (atomar gegen Preempt, damit es
                    // nicht vor dem Bind läuft). Es CALLt sofort -> blockiert in `senders`.
                    hal::cpu::local_irq_disable();
                    if let Some(v) =
                        system::spawn_on_core_parked(0, stale_victim as *const () as usize, 0, 3)
                    {
                        let v = system::admit_in_pd(STALE_VICTIM_PD.load(Ordering::Relaxed), v).expect("zulassen");
                        STALE_VICTIM_TID.store(v.to_raw(), Ordering::Relaxed);
                        STALE_STEP.store(1, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                1 => {
                    // Der Idle-Manager läuft nur, wenn alle prio-3-Threads blockiert sind
                    // -> das Opfer ist jetzt sicher in `senders` blockiert. Töten + reapen
                    // -> ein TOTER Eintrag verbleibt in der Endpoint-`senders`-Queue.
                    let v = ThreadId::from_raw(STALE_VICTIM_TID.load(Ordering::Relaxed));
                    if system::kill_local(v) {
                        while system::reap() > 0 {}
                        STALE_STEP.store(2, Ordering::Release);
                    }
                }
                2 => {
                    // Server erzeugen: sein erstes RECV MUSS den toten Opfer-Eintrag
                    // überspringen (Fix) statt zu paniken, dann als Empfänger blockieren.
                    hal::cpu::local_irq_disable();
                    if let Some(s) =
                        system::spawn_on_core_parked(0, stale_server as *const () as usize, 0, 3)
                    {
                        let s = system::admit_in_pd(STALE_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                        STALE_STEP.store(3, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                3 => {
                    // Lebenden Client erzeugen: sein CALL wird vom wartenden Server bedient.
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, stale_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(STALE_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        STALE_STEP.store(4, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                _ => {
                    // Auswertung: Kernel hat überlebt (keine Panik beim RECV trotz totem
                    // Eintrag) UND der Endpoint ist weiter funktionsfähig (Client erhielt
                    // 2x STALE_MAGIC). Ohne Fix wäre der Kernel beim Server-RECV panikt.
                    let served = STALE_SERVED.load(Ordering::Acquire);
                    if served != 0 {
                        STALE_OK.store(served == STALE_MAGIC.wrapping_mul(2), Ordering::Release);
                        STALE_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Audit-Regression C (Reply-Liveness): Server RECVt einen Client-CALL und parkt
        // ohne zu antworten; der Manager killt ihn; der Client-CALL MUSS mit
        // ERR_SERVER_GONE zurückkehren (statt zu hängen). Gestaffelt, gegate auf stale
        // fertig (core 0 frei von höher-prioren Workern -> deterministische Sequenz).
        if !RGONE_DONE.load(Ordering::Acquire) && STALE_DONE.load(Ordering::Acquire) {
            match RGONE_STEP.load(Ordering::Acquire) {
                0 => {
                    // Server erzeugen + binden. Er RECVt sofort -> blockiert (kein Caller).
                    hal::cpu::local_irq_disable();
                    if let Some(s) =
                        system::spawn_on_core_parked(0, rgone_server as *const () as usize, 0, 3)
                    {
                        let s = system::admit_in_pd(RGONE_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                        RGONE_SERVER_TID.store(s.to_raw(), Ordering::Relaxed);
                        RGONE_STEP.store(1, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                1 => {
                    // Client erzeugen + binden. Sein CALL trifft den wartenden Server ->
                    // Rendezvous (Server wird Reply-Owner, parkt), Client blockiert auf
                    // die Antwort.
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, rgone_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(RGONE_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        RGONE_STEP.store(2, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                2 => {
                    // Der Idle-Manager läuft erst, wenn Server (geparkt) UND Client (auf
                    // Antwort blockiert) ruhen. Server killen -> purge_ipc_queues sieht
                    // den Reply-Owner-Tod und entblockt den Client mit ERR_SERVER_GONE.
                    let s = ThreadId::from_raw(RGONE_SERVER_TID.load(Ordering::Relaxed));
                    if system::kill_local(s) {
                        while system::reap() > 0 {}
                        RGONE_STEP.store(3, Ordering::Release);
                    }
                }
                3 => {
                    // Kill-Runde ausgewertet (Client muss ERR_SERVER_GONE haben). Dann
                    // Runde 2 (Quiesce/Reload-Pfad) starten: frischen Server erzeugen.
                    if RGONE_RESULT.load(Ordering::Acquire) != u64::MAX {
                        hal::cpu::local_irq_disable();
                        if let Some(s) =
                            system::spawn_on_core_parked(0, rgone_server as *const () as usize, 0, 3)
                        {
                            let s = system::admit_in_pd(RGONE_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                            RGONE_SERVER_TID.store(s.to_raw(), Ordering::Relaxed);
                            RGONE_STEP.store(4, Ordering::Release);
                        }
                        hal::cpu::local_irq_enable();
                    }
                }
                4 => {
                    // Runde-2-Client erzeugen -> CALL -> Rendezvous, Server wird Reply-
                    // Owner + parkt, Client blockiert auf die Antwort.
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, rgone_client2 as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(RGONE_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        RGONE_STEP.store(5, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                5 => {
                    // Server NICHT killen, sondern via endpoint_quiesce_owner zurückziehen
                    // (Hot-Reload-Pfad, Server bleibt am Leben). Der Client muss ebenfalls
                    // ERR_SERVER_GONE bekommen; danach prüfen, dass der Server noch lebt.
                    let s = ThreadId::from_raw(RGONE_SERVER_TID.load(Ordering::Relaxed));
                    let ep = RGONE_EP_ID.load(Ordering::Relaxed);
                    if system::endpoint_quiesce_owner(ep, s) {
                        RGONE_Q_SRV_ALIVE.store(system::thread_alive(s), Ordering::Release);
                        RGONE_STEP.store(6, Ordering::Release);
                    }
                }
                _ => {
                    // Auswertung beider Runden: Kill-Pfad UND Quiesce-Pfad liefern dem
                    // Client ERR_SERVER_GONE; im Quiesce-Pfad lebt der Server weiter.
                    let r1 = RGONE_RESULT.load(Ordering::Acquire);
                    let r2 = RGONE_Q_RESULT.load(Ordering::Acquire);
                    if r1 != u64::MAX && r2 != u64::MAX {
                        let ok = r1 == result::ERR_SERVER_GONE
                            && r2 == result::ERR_SERVER_GONE
                            && RGONE_Q_SRV_ALIVE.load(Ordering::Acquire);
                        RGONE_OK.store(ok, Ordering::Release);
                        RGONE_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Budget-Donation (MCS): intra-core Client(budgetiert)->Server(unbeschränkt). Mit
        // Donation wird die Server-Arbeit gegen das Client-Budget belastet -> Client-
        // Konto erschöpft sich je Call. Gegate auf rgone fertig (core 0 frei sequenzierbar).
        if !DDON_DONE.load(Ordering::Acquire) && RGONE_DONE.load(Ordering::Acquire) {
            match DDON_STEP.load(Ordering::Acquire) {
                0 => {
                    // Server erzeugen + binden (RECVt, blockiert). prio 3.
                    hal::cpu::local_irq_disable();
                    if let Some(s) =
                        system::spawn_on_core_parked(0, ddon_server as *const () as usize, 0, 3)
                    {
                        let s = system::admit_in_pd(DDON_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                        DDON_STEP.store(1, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                1 => {
                    // Client erzeugen, KNAPPES Budget binden (vor dem ersten Call -> Donation
                    // sofort aktiv), Erschöpfungs-Baseline schnappen. Alles atomar, bevor der
                    // Client läuft (kein wfi dazwischen).
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, ddon_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(DDON_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        DDON_CLIENT_TID.store(c.to_raw(), Ordering::Relaxed);
                        if let Ok(sc) = system::install_sched_context_cap(
                            DDON_CBUDGET,
                            DDON_CPERIOD,
                            Rights::WRITE,
                        ) {
                            system::bind_sched_context(sc, 0, c);
                        }
                        DDON_DEPL0.store(system::budget_stats(0).0, Ordering::Relaxed);
                        DDON_STEP.store(2, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                _ => {
                    // Warten, bis der Client alle Calls durch hat (dann parkt er -> Manager
                    // läuft). Erschöpfungs-Delta des core-0-Kontos messen: mit Donation
                    // wurde das Client-Budget durch die Server-Arbeit je Call belastet.
                    if DDON_CLIENT_DONE.load(Ordering::Acquire) {
                        let delta = system::budget_stats(0)
                            .0
                            .wrapping_sub(DDON_DEPL0.load(Ordering::Relaxed));
                        DDON_DELTA.store(delta, Ordering::Release);
                        DDON_OK.store(delta >= DDON_MIN_DEPL, Ordering::Release);
                        DDON_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // First-class Reply-Cap (ObjectKind::Reply) + Revocation: ausstehenden Call per
        // Reply-Cap-Löschung abbrechen -> Client ERR_SERVER_GONE. Gegate auf ddon fertig.
        if !RCAP_DONE.load(Ordering::Acquire) && DDON_DONE.load(Ordering::Acquire) {
            match RCAP_STEP.load(Ordering::Acquire) {
                0 => {
                    // Server erzeugen (RECVt + parkt, antwortet nie).
                    hal::cpu::local_irq_disable();
                    if let Some(s) =
                        system::spawn_on_core_parked(0, rgone_server as *const () as usize, 0, 3)
                    {
                        let s = system::admit_in_pd(RCAP_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                        RCAP_STEP.store(1, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                1 => {
                    // Client erzeugen -> CALL -> Rendezvous (Server parkt, Client blockiert).
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, rcap_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(RCAP_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        RCAP_CLIENT_TID.store(c.to_raw(), Ordering::Relaxed);
                        RCAP_STEP.store(2, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                2 => {
                    // Manager läuft -> Client blockiert auf die Antwort. Eine Reply-Cap
                    // für diesen Call prägen und SOFORT löschen -> Finalisierung bricht
                    // den Call ab (Client ERR_SERVER_GONE).
                    let ep = RCAP_EP_ID.load(Ordering::Relaxed);
                    let caller = ThreadId::from_raw(RCAP_CLIENT_TID.load(Ordering::Relaxed));
                    if let Ok(rc) = system::reply_cap_for(ep, caller) {
                        let _ = system::cap_delete(rc); // Revocation -> Call-Abbruch
                        RCAP_STEP.store(3, Ordering::Release);
                    }
                }
                _ => {
                    let r = RCAP_RESULT.load(Ordering::Acquire);
                    if r != u64::MAX {
                        RCAP_OK.store(r == result::ERR_SERVER_GONE, Ordering::Release);
                        RCAP_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Reply-Cap-Server-Migration: v1 empfängt den Call und parkt; der Manager migriert
        // die Antwortpflicht auf v2 (endpoint_migrate_owner) und zieht v1 capless zurück.
        // v2 schließt DENSELBEN Call ab -> Client bekommt OK + RMIG_V2_FACTOR*Eingabe.
        // Gegate auf rcap fertig (core 0 sequenziell frei).
        if !RMIG_DONE.load(Ordering::Acquire) && RCAP_DONE.load(Ordering::Acquire) {
            match RMIG_STEP.load(Ordering::Acquire) {
                0 => {
                    // v1-Server erzeugen + binden (RECVt, blockiert als Empfänger).
                    hal::cpu::local_irq_disable();
                    if let Some(s) =
                        system::spawn_on_core_parked(0, rmig_server_v1 as *const () as usize, 0, 3)
                    {
                        let s = system::admit_in_pd(RMIG_SERVER_PD.load(Ordering::Relaxed), s).expect("zulassen");
                        RMIG_V1_TID.store(s.to_raw(), Ordering::Relaxed);
                        RMIG_STEP.store(1, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                1 => {
                    // Client erzeugen -> CALL -> Rendezvous mit v1 (v1 wird Reply-Owner,
                    // setzt RMIG_RECEIVED, parkt ohne REPLY; Client blockiert auf Antwort).
                    hal::cpu::local_irq_disable();
                    if let Some(c) =
                        system::spawn_on_core_parked(0, rmig_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(RMIG_CLIENT_PD.load(Ordering::Relaxed), c).expect("zulassen");
                        RMIG_STEP.store(2, Ordering::Release);
                    }
                    hal::cpu::local_irq_enable();
                }
                2 => {
                    // Sobald v1 den Call empfangen hat: Antwortpflicht auf v2 migrieren,
                    // v1 capless zurückziehen (Hot-Reload) und v2 starten + binden.
                    if RMIG_RECEIVED.load(Ordering::Acquire) {
                        let ep = RMIG_EP_ID.load(Ordering::Relaxed);
                        let v1 = ThreadId::from_raw(RMIG_V1_TID.load(Ordering::Relaxed));
                        // A-4.2: den Ruhepunkt VOR dem Eingriff herstellen. Ohne die
                        // Stilllegung waere jede Auskunft ueber offene Transaktionen in dem
                        // Moment veraltet, in dem sie zurueckkommt -- ein CALL auf einem
                        // anderen Kern genuegt. Erst danach ist der Befund unten eine
                        // Aussage und keine Momentaufnahme.
                        RMIG_QUIESCED.store(system::endpoint_begin_quiesce(ep), Ordering::Release);
                        // Der Befund am ECHTEN offenen Call: v1 schuldet eine Antwort, also
                        // ruht der Endpoint NICHT -- und zwar aus genau diesem Grund. Das ist
                        // die Rollenaufschluesselung, die der Zustandstest (`run_quiesce`)
                        // nicht fuellen kann.
                        let q = system::endpoint_quiescence_of(ep, v1);
                        RMIG_ROLE_OK.store(
                            q.as_reply_owner
                                && !q.as_sender
                                && !q.as_caller
                                && !q.is_quiescent()
                                && !system::endpoint_is_idle(ep),
                            Ordering::Release,
                        );
                        let migrated = system::endpoint_migrate_owner(ep, v1);
                        RMIG_MIGRATED.store(migrated, Ordering::Release);
                        system::endpoint_retire_receiver(ep, v1);
                        system::clear_pd_cap(
                            RMIG_SERVER_PD.load(Ordering::Relaxed),
                            EP_CAP as usize,
                        );
                        // A-4.2: freigeben, BEVOR v2 existiert -- nicht danach. Sonst liefe
                        // v2s erstes RECV moeglicherweise in das noch geschlossene Tor und
                        // bekaeme ERR_QUIESCING; ob das passiert, haengt daran, wann der
                        // Scheduler ihn erstmals laufen laesst. Ein Rennen, das der Test
                        // gelegentlich bestuende, ist schlimmer als keiner. Der Ruhepunkt hat
                        // hier seinen Zweck (Migration + Rueckzug von v1) bereits erfuellt.
                        RMIG_RESUMED.store(system::endpoint_end_quiesce(ep), Ordering::Release);
                        hal::cpu::local_irq_disable();
                        if let Some(v2) =
                            system::spawn_on_core_parked(0, rmig_server_v2 as *const () as usize, 0, 3)
                        {
                            let v2 = system::admit_in_pd(RMIG_V2_PD.load(Ordering::Relaxed), v2).expect("zulassen");
                        }
                        hal::cpu::local_irq_enable();
                        RMIG_STEP.store(3, Ordering::Release);
                    }
                }
                _ => {
                    // v2 hat geantwortet -> Client kehrte zurück. Erfolg: OK + von v2
                    // berechneter Wert + Migration meldete Erfolg (kein ERR_SERVER_GONE).
                    let r = RMIG_RESULT.load(Ordering::Acquire);
                    if r != u64::MAX {
                        let ok = r == result::OK
                            && RMIG_VALUE.load(Ordering::Acquire) == RMIG_V2_FACTOR * RMIG_INPUT
                            && RMIG_MIGRATED.load(Ordering::Acquire)
                            // A-4.2: der Austausch lief ueber einen echten Ruhepunkt --
                            // stillgelegt, Rollenbefund korrekt, wieder freigegeben.
                            && RMIG_QUIESCED.load(Ordering::Acquire)
                            && RMIG_ROLE_OK.load(Ordering::Acquire)
                            && RMIG_RESUMED.load(Ordering::Acquire);
                        RMIG_OK.store(ok, Ordering::Release);
                        RMIG_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // CAPS-Read-Concurrency (#3, Reader-Writer-Lock): zwei Sonden auf zwei Kernen
        // halten gleichzeitig den CAPS-Read-Lock -> beweist parallele Cap-Lookups (mit
        // dem alten exklusiven Lock unmöglich). Gegate auf rmig + BEIDE Fuzzer fertig,
        // damit im Sondenfenster keine konkurrierenden CAPS-Mutationen laufen.
        if !CAPLK_DONE.load(Ordering::Acquire)
            && RMIG_DONE.load(Ordering::Acquire)
            && fuzz::fuzzers_gate(CROSS_DONE.load(Ordering::Acquire))
        {
            match CAPLK_STEP.load(Ordering::Acquire) {
                0 => {
                    // Beide Sonden auf verschiedenen Kernen erzeugen (keine PD nötig).
                    // Atomar einreihen, dann beide Zielkerne per IPI wecken -> sie laufen
                    // nebenläufig und treffen sich im CAPS-Read-Abschnitt an der Barriere.
                    hal::cpu::local_irq_disable();
                    let a = system::spawn_on_core(
                        CAPLK_CORE_A,
                        caplk_probe_a as *const () as usize,
                        0,
                        3,
                    );
                    let b = system::spawn_on_core(
                        CAPLK_CORE_B,
                        caplk_probe_b as *const () as usize,
                        0,
                        3,
                    );
                    hal::cpu::local_irq_enable();
                    if let (Some(ta), Some(tb)) = (a, b) {
                        system::wake_remote(ta);
                        system::wake_remote(tb);
                        CAPLK_STEP.store(1, Ordering::Release);
                    }
                }
                _ => {
                    if CAPLK_A_DONE.load(Ordering::Acquire) && CAPLK_B_DONE.load(Ordering::Acquire)
                    {
                        let ok = CAPLK_A_OK.load(Ordering::Acquire)
                            && CAPLK_B_OK.load(Ordering::Acquire)
                            && system::caps_max_concurrent_readers() >= CAPLK_WANT;
                        CAPLK_OK.store(ok, Ordering::Release);
                        CAPLK_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Sicherheitsdomänen (ext-22, P1): Fixtures LAZY anlegen (nach allen Fuzzer-/Reclaim-
        // Tests -> kstack-Pool/ASIDs frei, keine Baseline-/Timing-Races) und strukturell
        // prüfen. TrustedSas: nur die Domäne (Regel 3 prüft nur untrusted). Hardware/User:
        // isolierte EL0-Threads (`spawn_isolated` setzt VSPACE_OF synchron). Gegate auf caplk.
        // VOR den Fuzzern (gegate auf rmig fertig) — die ext-22-Tests laufen im frischen,
        // schnellen Frühregime; die Fuzzer warten ihrerseits auf CHAN_DONE (s. u.).
        if !DOMAIN_DONE.load(Ordering::Acquire) && RMIG_DONE.load(Ordering::Acquire) {
            hal::cpu::local_irq_disable();
            let tpd = system::create_pd_in_domain(Domain::TrustedSas);
            if let Some(tpd) = tpd {
                DOMAIN_TRUSTED_PD.store(tpd, Ordering::Relaxed);
                // HardwareLand-Fixture als echtes Backend an den TrustedSas-Partner gebunden
                // (Regel 4: jede HardwareLand-PD hat einen TrustedSas-Partner).
                if let Some((hpd, _ep, _ntfn)) = system::create_hardware_backend(tpd, 1) {
                    // SENSITIVITAET P1 (geprueft): spawn_isolated -> spawn (global) =>
                    // HardwareLand laeuft global => domain_audit()==3 => domain FAILURES.
                    if let Some((h, _)) =
                        system::spawn_isolated_parked(churn_dummy as *const () as usize, 0, 3)
                    {
                        let h = system::admit_in_pd(hpd, h).expect("zulassen");
                        DOMAIN_HW_TID.store(h.to_raw(), Ordering::Relaxed);
                    }
                    DOMAIN_HW_PD.store(hpd, Ordering::Relaxed);
                }
            }
            if let Some(upd) = system::create_pd_in_domain(Domain::UserLand) {
                if let Some((u, _)) =
                    system::spawn_isolated_parked(churn_dummy as *const () as usize, 0, 3)
                {
                    let u = system::admit_in_pd(upd, u).expect("zulassen");
                    DOMAIN_USER_TID.store(u.to_raw(), Ordering::Relaxed);
                }
                DOMAIN_USER_PD.store(upd, Ordering::Relaxed);
            }
            hal::cpu::local_irq_enable();

            let code = system::domain_audit();
            DOMAIN_AUDIT_CODE.store(code, Ordering::Release);
            let roundtrip = system::pd_domain(DOMAIN_TRUSTED_PD.load(Ordering::Relaxed))
                == Some(Domain::TrustedSas)
                && system::pd_domain(DOMAIN_HW_PD.load(Ordering::Relaxed))
                    == Some(Domain::HardwareLand)
                && system::pd_domain(DOMAIN_USER_PD.load(Ordering::Relaxed))
                    == Some(Domain::UserLand);
            DOMAIN_OK.store(code == 0 && roundtrip, Ordering::Release);
            DOMAIN_DONE.store(true, Ordering::Release);
            // Aufräumen: die zwei isolierten Domänen-Fixtures abbauen (ASIDs + kstack-Slots
            // freigeben), damit die folgenden Tests (pdctl/chan) den Pool wieder voll haben.
            let ht = DOMAIN_HW_TID.swap(u64::MAX, Ordering::Relaxed);
            if ht != u64::MAX {
                system::destroy_isolated(ThreadId::from_raw(ht));
            }
            let ut = DOMAIN_USER_TID.swap(u64::MAX, Ordering::Relaxed);
            if ut != u64::MAX {
                system::destroy_isolated(ThreadId::from_raw(ut));
            }
        }

        // UserLand-Management (ext-22, P2): cap-gated SYS_PDCTL. Gegate auf domain fertig.
        // Lazy-Setup + Controller/Ziel auf core 0 (kooperativ via YIELD).
        if !PDCTL_DONE.load(Ordering::Acquire) && DOMAIN_DONE.load(Ordering::Acquire) {
            match PDCTL_STEP.load(Ordering::Acquire) {
                0 => {
                    // Zähler-Frame + UserLand-Ziel-PD + TrustedSas-Controller-PD + Caps.
                    if let Some(frame) = system::alloc(PDCTL_FRAME_SIZE, PDCTL_FRAME_SIZE) {
                        let fbase = frame.region().base;
                        poke_u64(fbase, 0);
                        if let Ok(root) = system::cap_install(frame) {
                            if let (Ok(wcap), Some(tpd), Some(cpd)) = (
                                system::cap_mint(root, Rights::WRITE, 0),
                                system::create_pd_in_domain(Domain::UserLand),
                                system::create_pd_in_domain(Domain::TrustedSas),
                            ) {
                                system::install_pd_cap(tpd, 0, wcap); // Frame-Cap -> Ziel Slot 0
                                if let Ok(ctrl) = system::install_pd_control_cap(tpd, Rights::WRITE)
                                {
                                    system::install_pd_cap(cpd, PDCTL_CTRL_SLOT as usize, ctrl);
                                }
                                // Policy-Negativtest: eine PdControl-Cap in die UserLand-Ziel-PD
                                // zu installieren MUSS abgelehnt werden (Cap-Typ-Policy).
                                if let Ok(ctrl2) =
                                    system::install_pd_control_cap(tpd, Rights::WRITE)
                                {
                                    let denied = !system::install_pd_cap(tpd, 7, ctrl2);
                                    PDCTL_POLICY.store(denied, Ordering::Release);
                                }
                                PDCTL_FRAME.store(fbase, Ordering::Release);
                                PDCTL_TARGET_PD.store(tpd, Ordering::Relaxed);
                                PDCTL_CTRL_PD.store(cpd, Ordering::Relaxed);
                                PDCTL_STEP.store(1, Ordering::Release);
                            }
                        }
                    }
                }
                1 => {
                    // Ziel (isoliert) + Controller (EL1) ATOMAR erzeugen, damit das Ziel nicht
                    // vor dem Controller die CPU monopolisiert; beide core 0, prio 3, kooperativ.
                    let fbase = PDCTL_FRAME.load(Ordering::Acquire) as usize;
                    hal::cpu::local_irq_disable();
                    if let Some((t, _)) =
                        system::spawn_isolated_parked(pdctl_target as *const () as usize, fbase, 3)
                    {
                        let t = system::admit_in_pd(PDCTL_TARGET_PD.load(Ordering::Relaxed), t).expect("zulassen");
                        PDCTL_TARGET_TID.store(t.to_raw(), Ordering::Relaxed);
                    }
                    if let Some(c) =
                        system::spawn_on_core_parked(0, pdctl_controller as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(PDCTL_CTRL_PD.load(Ordering::Relaxed), c).expect("zulassen");
                    }
                    hal::cpu::local_irq_enable();
                    PDCTL_STEP.store(2, Ordering::Release);
                }
                _ => {
                    if PDCTL_CTRL_DONE.load(Ordering::Acquire) {
                        let ok = PDCTL_RAN.load(Ordering::Acquire)
                            && PDCTL_FROZE.load(Ordering::Acquire)
                            && PDCTL_RESUMED.load(Ordering::Acquire)
                            && PDCTL_NOCAP.load(Ordering::Acquire)
                            && PDCTL_POLICY.load(Ordering::Acquire)
                            && PDCTL_STOPPED.load(Ordering::Acquire);
                        PDCTL_OK.store(ok, Ordering::Release);
                        PDCTL_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Paarweiser Treiber<->Backend-Kanal (ext-22, P3): gegate auf pdctl fertig.
        if !CHAN_DONE.load(Ordering::Acquire) && PDCTL_DONE.load(Ordering::Acquire) {
            match CHAN_STEP.load(Ordering::Acquire) {
                0 => {
                    // TrustedSas-Zeitdienst + HardwareLand-Backend (unveränderliche Bindung).
                    if let Some(ts) = system::create_pd_in_domain(Domain::TrustedSas) {
                        CHAN_TS_PD.store(ts, Ordering::Relaxed);
                        if let Some((be, ep, _ntfn)) = system::create_hardware_backend(ts, 1) {
                            CHAN_BE_PD.store(be, Ordering::Relaxed);
                            if let Ok(root) = system::install_endpoint_cap(ep as u32, Rights::RWX) {
                                if let (Ok(recv), Ok(send)) = (
                                    system::cap_mint(root, Rights::READ, 0),
                                    system::cap_mint(root, Rights::WRITE, 0),
                                ) {
                                    // Recv-Cap ins Backend (policy-geprüft auf genau diesen Kanal).
                                    CHAN_BE_RECV_OK.store(
                                        system::install_pd_cap(be, 0, recv),
                                        Ordering::Release,
                                    );
                                    system::install_pd_cap(ts, 0, send); // Send-Cap an Trusted
                                }
                            }
                            // 1:N — ein 2. Backend am selben Trusted-Partner muss möglich sein.
                            CHAN_1N_OK.store(
                                system::create_hardware_backend(ts, 2).is_some(),
                                Ordering::Release,
                            );
                            // Negativ: eine FREMDE Endpoint-Cap (anderer Endpoint) ins Backend
                            // zu legen MUSS abgelehnt werden (Kanal-Bindung).
                            if let Some(other) = system::create_endpoint() {
                                if let Ok(oroot) =
                                    system::install_endpoint_cap(other as u32, Rights::RWX)
                                {
                                    if let Ok(ocap) = system::cap_mint(oroot, Rights::READ, 0) {
                                        let denied = !system::install_pd_cap(be, 6, ocap);
                                        CHAN_FOREIGN_DENIED.store(denied, Ordering::Release);
                                    }
                                }
                            }
                            CHAN_STEP.store(1, Ordering::Release);
                        }
                    }
                }
                1 => {
                    // Backend (isoliert EL0) erzeugen + binden -> RECVt, blockiert.
                    if let Some((b, _)) =
                        system::spawn_isolated_parked(chan_backend as *const () as usize, 0, 3)
                    {
                        let b = system::admit_in_pd(CHAN_BE_PD.load(Ordering::Relaxed), b).expect("zulassen");
                    }
                    CHAN_STEP.store(2, Ordering::Release);
                }
                2 => {
                    // Trusted-Client (EL1) erzeugen + binden -> CALLt das Backend.
                    if let Some(c) =
                        system::spawn_on_core_parked(0, chan_client as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(CHAN_TS_PD.load(Ordering::Relaxed), c).expect("zulassen");
                    }
                    CHAN_STEP.store(3, Ordering::Release);
                }
                _ => {
                    if CHAN_CLIENT_DONE.load(Ordering::Acquire) {
                        let ok = CHAN_RESULT.load(Ordering::Acquire) == CHAN_INPUT * CHAN_FACTOR
                            && CHAN_BE_RECV_OK.load(Ordering::Acquire)
                            && CHAN_1N_OK.load(Ordering::Acquire)
                            && CHAN_FOREIGN_DENIED.load(Ordering::Acquire)
                            && system::domain_audit() == 0;
                        CHAN_OK.store(ok, Ordering::Release);
                        CHAN_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // RTC-Hardware-Backend (ext-22, P4): erstes echtes Geraet ueber die generische
        // MMIO-Infrastruktur. Gegate auf chan fertig (sequenziell, vor den Fuzzern).
        if !RTC_DONE.load(Ordering::Acquire) && CHAN_DONE.load(Ordering::Acquire) {
            match RTC_STEP.load(Ordering::Acquire) {
                0 => {
                    // TrustedSas-Zeitdienst + HardwareLand-RTC-Backend + Kanal-Caps + MMIO-Cap.
                    if let Some(ts) = system::create_pd_in_domain(Domain::TrustedSas) {
                        RTC_TS_PD.store(ts, Ordering::Relaxed);
                        if let Some((be, ep, _ntfn)) = system::create_hardware_backend(ts, 3) {
                            RTC_BE_PD.store(be, Ordering::Relaxed);
                            if let Ok(root) = system::install_endpoint_cap(ep as u32, Rights::RWX) {
                                if let (Ok(recv), Ok(send)) = (
                                    system::cap_mint(root, Rights::READ, 0),
                                    system::cap_mint(root, Rights::WRITE, 0),
                                ) {
                                    system::install_pd_cap(be, 0, recv); // Kanal-Recv ins Backend
                                    system::install_pd_cap(ts, 0, send); // Kanal-Send an Trusted
                                }
                            }
                            // MMIO-Cap fuer das RTC: nur kernelseitig geprägt, ins HardwareLand-
                            // Backend (Slot 1) installierbar (HardwareLand-Policy).
                            if let Ok(mcap) =
                                system::install_mmio_cap(RTC_PHYS, RTC_LEN, Rights::READ)
                            {
                                RTC_MMIO_OK
                                    .store(system::install_pd_cap(be, 1, mcap), Ordering::Release);
                            }
                            // Policy-Negativtest: MMIO-Cap in eine UserLand-PD -> abgelehnt.
                            if let Some(upd) = system::create_pd_in_domain(Domain::UserLand) {
                                if let Ok(mcap2) =
                                    system::install_mmio_cap(RTC_PHYS, RTC_LEN, Rights::READ)
                                {
                                    let denied = !system::install_pd_cap(upd, 0, mcap2);
                                    RTC_POLICY.store(denied, Ordering::Release);
                                }
                            }
                            RTC_STEP.store(1, Ordering::Release);
                        }
                    }
                }
                1 => {
                    // Backend (isoliert EL0) erzeugen + binden, dann die RTC-Registerseite
                    // EL0-RO in seine VSpace mappen (generischer Device-Mechanismus).
                    if let Some((b, _)) = system::spawn_isolated_parked(
                        rtc_backend as *const () as usize,
                        RTC_PHYS as usize,
                        3,
                    ) {
                        // **Binden und mappen VOR der Zulassung** (D0-Verschaerfung). Diese Stelle hat
                        // der TYP gefunden, nicht das Gegenlesen: mein Scan suchte nach
                        // `map_into_thread`/`install_pd_cap`, und `map_region_into_thread` kam darin
                        // nicht vor. Der Kommentar daneben sagt selbst, was sonst passiert -- das
                        // Backend liest die Geraeteseite, bevor sie da ist, und faultet.
                        system::bind_pd_parked(&b, RTC_BE_PD.load(Ordering::Relaxed));
                        // RTC-Registerseite EL0-RO in die Backend-VSpace mappen (generisch).
                        // SENSITIVITAET P4 (geprueft): ohne dieses Mapping faultet das Backend
                        // beim RTC-Read (FAR=0x09010000) -> Isolation greift.
                        system::map_region_into_parked(
                            &b,
                            RTC_PHYS,
                            RTC_LEN,
                            system::MappingKind::Device { ro: true },
                        );
                        let _ = system::admit(b).expect("zulassen");
                    }
                    RTC_STEP.store(2, Ordering::Release);
                }
                2 => {
                    // Trusted-Zeitdienst (EL1) erzeugen + binden -> CALLt das RTC-Backend.
                    if let Some(c) =
                        system::spawn_on_core_parked(0, rtc_timeservice as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(RTC_TS_PD.load(Ordering::Relaxed), c).expect("zulassen");
                    }
                    RTC_STEP.store(3, Ordering::Release);
                }
                _ => {
                    if RTC_CLIENT_DONE.load(Ordering::Acquire) {
                        let ok = RTC_RES_CODE.load(Ordering::Acquire) == result::OK
                            && RTC_VALUE.load(Ordering::Acquire) > 0 // echtes RTC_DR gelesen
                            && RTC_MMIO_OK.load(Ordering::Acquire)
                            && RTC_POLICY.load(Ordering::Acquire)
                            && system::domain_audit() == 0
                            && system::vspace_audit() == 0;
                        RTC_OK.store(ok, Ordering::Release);
                        RTC_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // RTC-IRQ (ext-22, P5): IRQ-Cap + GIC-SPI-Routing + Deferred-IRQ-Zustellung. Gegate
        // auf rtc fertig (sequenziell, vor den Fuzzern).
        if !IRQT_DONE.load(Ordering::Acquire) && RTC_DONE.load(Ordering::Acquire) {
            match IRQT_STEP.load(Ordering::Acquire) {
                0 => {
                    if let Some(ts) = system::create_pd_in_domain(Domain::TrustedSas) {
                        IRQT_TS_PD.store(ts, Ordering::Relaxed);
                        if let Some((be, ep, ntfn)) = system::create_hardware_backend(ts, 5) {
                            IRQT_BE_PD.store(be, Ordering::Relaxed);
                            // Kanal-Caps (Endpoint).
                            if let Ok(eroot) = system::install_endpoint_cap(ep as u32, Rights::RWX)
                            {
                                if let (Ok(recv), Ok(send)) = (
                                    system::cap_mint(eroot, Rights::READ, 0),
                                    system::cap_mint(eroot, Rights::WRITE, 0),
                                ) {
                                    system::install_pd_cap(be, 0, recv);
                                    system::install_pd_cap(ts, 0, send);
                                }
                            }
                            // Kanal-Notification: WAIT-Cap (Slot 2) ins Backend = IRQ-Zustellung.
                            if let Ok(nroot) =
                                system::install_notification_cap(ntfn as u32, Rights::RWX)
                            {
                                if let Ok(waitc) = system::cap_mint(nroot, Rights::READ, 0) {
                                    system::install_pd_cap(be, 2, waitc);
                                }
                            }
                            // MMIO-Cap (RTC, RW) + IRQ-Cap (INTID 34) ins HardwareLand-Backend.
                            if let Ok(mcap) =
                                system::install_mmio_cap(RTC_PHYS, RTC_LEN, Rights::WRITE)
                            {
                                IRQT_MMIO_OK
                                    .store(system::install_pd_cap(be, 1, mcap), Ordering::Release);
                            }
                            if let Ok(icap) = system::install_irq_cap(RTC_INTID, Rights::READ) {
                                IRQT_CAP_OK
                                    .store(system::install_pd_cap(be, 3, icap), Ordering::Release);
                            }
                            // Policy-Negativtest: IRQ-Cap in eine UserLand-PD -> abgelehnt.
                            if let Some(upd) = system::create_pd_in_domain(Domain::UserLand) {
                                if let Ok(icap2) = system::install_irq_cap(RTC_INTID, Rights::READ)
                                {
                                    let denied = !system::install_pd_cap(upd, 0, icap2);
                                    IRQT_POLICY.store(denied, Ordering::Release);
                                }
                            }
                            // IRQ an die Kanal-Notification binden + SPI an Kern 0 routen.
                            system::bind_irq(RTC_INTID, ntfn, IRQT_BADGE, 0);
                            IRQT_STEP.store(1, Ordering::Release);
                        }
                    }
                }
                1 => {
                    // Backend (isoliert EL0) + RTC RW-Mapping; es armiert den IRQ und WAITet.
                    if let Some((b, _)) = system::spawn_isolated_parked(
                        rtc_irq_backend as *const () as usize,
                        RTC_PHYS as usize,
                        3,
                    ) {
                        // **Binden und mappen VOR der Zulassung** (D0-Verschaerfung). Diese Stelle hat
                        // der TYP gefunden, nicht das Gegenlesen: mein Scan suchte nach
                        // `map_into_thread`/`install_pd_cap`, und `map_region_into_thread` kam darin
                        // nicht vor. Der Kommentar daneben sagt selbst, was sonst passiert -- das
                        // Backend liest die Geraeteseite, bevor sie da ist, und faultet.
                        system::bind_pd_parked(&b, IRQT_BE_PD.load(Ordering::Relaxed));
                        system::map_region_into_parked(
                            &b,
                            RTC_PHYS,
                            RTC_LEN,
                            system::MappingKind::Device { ro: false },
                        ); // RW (armieren)
                        let _ = system::admit(b).expect("zulassen");
                    }
                    IRQT_STEP.store(2, Ordering::Release);
                }
                2 => {
                    // Trusted-Zeitdienst (EL1) -> CALLt das Backend (kehrt erst nach dem IRQ zurueck).
                    if let Some(c) =
                        system::spawn_on_core_parked(0, irq_timeservice as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(IRQT_TS_PD.load(Ordering::Relaxed), c).expect("zulassen");
                    }
                    IRQT_STEP.store(3, Ordering::Release);
                }
                _ => {
                    if IRQT_CLIENT_DONE.load(Ordering::Acquire) {
                        let ok = IRQT_RES_CODE.load(Ordering::Acquire) == result::OK
                            && IRQT_GOT_VALUE.load(Ordering::Acquire) == 1 // Backend meldete IRQ
                            && system::irqs_delivered() >= 1
                            && IRQT_MMIO_OK.load(Ordering::Acquire)
                            && IRQT_CAP_OK.load(Ordering::Acquire)
                            && IRQT_POLICY.load(Ordering::Acquire)
                            && system::domain_audit() == 0;
                        IRQT_OK.store(ok, Ordering::Release);
                        IRQT_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // DMA-Capability (ext-23, D0): erstes DMA-Backend über die generische DmaCap-Schicht
        // hinter der DmaEnforcer-Abstraktion. Gegate auf irq fertig (sequenziell, vor den Fuzzern).
        if !DMA_DONE.load(Ordering::Acquire) && IRQT_DONE.load(Ordering::Acquire) {
            match DMA_STEP.load(Ordering::Acquire) {
                0 => {
                    // TrustedSas-Dienst + HardwareLand-DMA-Backend + Kanal-Caps + DmaCap.
                    if let Some(ts) = system::create_pd_in_domain(Domain::TrustedSas) {
                        DMA_TS_PD.store(ts, Ordering::Relaxed);
                        if let Some((be, ep, _ntfn)) = system::create_hardware_backend(ts, 6) {
                            DMA_BE_PD.store(be, Ordering::Relaxed);
                            if let Ok(root) = system::install_endpoint_cap(ep as u32, Rights::RWX) {
                                if let (Ok(recv), Ok(send)) = (
                                    system::cap_mint(root, Rights::READ, 0),
                                    system::cap_mint(root, Rights::WRITE, 0),
                                ) {
                                    system::install_pd_cap(be, 0, recv); // Kanal-Recv ins Backend
                                    system::install_pd_cap(ts, 0, send); // Kanal-Send an Trusted
                                }
                            }
                            // DMA-Region ausschneiden + DmaCap ins HardwareLand-Backend (Slot 1).
                            if let Some(r) = system::alloc_dma_region(DMA_LEN) {
                                DMA_PHYS.store(r.base, Ordering::Release);
                                if let Ok(dcap) = system::install_dma_cap(r.base, r.len, Rights::RW)
                                {
                                    DMA_CAP_OK.store(
                                        system::install_pd_cap(be, 1, dcap),
                                        Ordering::Release,
                                    );
                                    // Policy-Negativtest: DIESELBE Cap (dasselbe DMA-Objekt, KEINE
                                    // zweite Region) in eine UserLand-PD -> abgelehnt. (Eine zweite
                                    // install_dma_cap-Region wuerde ein zweites Objekt mit gleicher
                                    // Region erzeugen -> dma_audit-Ueberlappung; das vermeiden wir.)
                                    if let Some(upd) = system::create_pd_in_domain(Domain::UserLand)
                                    {
                                        let denied = !system::install_pd_cap(upd, 0, dcap);
                                        DMA_POLICY.store(denied, Ordering::Release);
                                    }
                                }
                            }
                            DMA_STEP.store(1, Ordering::Release);
                        }
                    }
                }
                1 => {
                    // Backend (isoliert EL0) erzeugen + binden, dann die DMA-Region EL0-RW
                    // Normal-NC in seine VSpace mappen (generischer DMA-Mechanismus).
                    let phys = DMA_PHYS.load(Ordering::Acquire);
                    if let Some((b, _)) =
                        system::spawn_isolated_parked(dma_backend as *const () as usize, phys as usize, 3)
                    {
                        // **Binden und mappen VOR der Zulassung** (D0-Verschaerfung). Diese Stelle hat
                        // der TYP gefunden, nicht das Gegenlesen: mein Scan suchte nach
                        // `map_into_thread`/`install_pd_cap`, und `map_region_into_thread` kam darin
                        // nicht vor. Der Kommentar daneben sagt selbst, was sonst passiert -- das
                        // Backend liest die Geraeteseite, bevor sie da ist, und faultet.
                        system::bind_pd_parked(&b, DMA_BE_PD.load(Ordering::Relaxed));
                        // SENSITIVITAET D0 (geprueft): ohne dieses Mapping faultet das Backend
                        // beim DMA-Zugriff (FAR in der DMA-Region) -> Isolation greift.
                        system::map_region_into_parked(
                            &b,
                            phys,
                            DMA_LEN,
                            system::MappingKind::Dma { coherent: false },
                        );
                        let _ = system::admit(b).expect("zulassen");
                    }
                    DMA_STEP.store(2, Ordering::Release);
                }
                2 => {
                    // Trusted-Dienst (EL1) erzeugen + binden -> CALLt das DMA-Backend.
                    if let Some(c) =
                        system::spawn_on_core_parked(0, dma_timeservice as *const () as usize, 0, 3)
                    {
                        let c = system::admit_in_pd(DMA_TS_PD.load(Ordering::Relaxed), c).expect("zulassen");
                    }
                    DMA_STEP.store(3, Ordering::Release);
                }
                _ => {
                    if DMA_CLIENT_DONE.load(Ordering::Acquire) {
                        // Kohaerenz: der Kernel liest die DMA-Region via Identity-Map und prueft,
                        // dass er DIESELBEN Bytes sieht, die das Backend ueber seine NC-Abbildung
                        // geschrieben hat (write+read+coherency end-to-end).
                        let phys = DMA_PHYS.load(Ordering::Acquire);
                        let (w0, w1) = system::testsupport::peek_dma_words(phys);
                        // Bounds-Sensitivität (selbstreinigend): mit einem absichtlich zu hohen
                        // `floor` MUSS die (legitime) Region das dma_audit verletzen (Code != 0) —
                        // beweist, dass das Oracle Out-of-Window-Regionen faengt. Der echte
                        // dma_audit() (korrektes floor) bleibt 0.
                        let sens = system::testsupport::dma_audit_with_floor(phys + DMA_LEN) != 0;
                        DMA_SENS_OK.store(sens, Ordering::Release);
                        let ok = DMA_RES_CODE.load(Ordering::Acquire) == result::OK
                            && DMA_VALUE.load(Ordering::Acquire) as u32 == DMA_PAT0 // EL0-Round-Trip
                            && w0 == DMA_PAT0 // Kernel sieht PAT0 (Kohaerenz)
                            && w1 == DMA_PAT1 // Kernel sieht PAT1
                            && DMA_CAP_OK.load(Ordering::Acquire)
                            && DMA_POLICY.load(Ordering::Acquire)
                            && sens
                            && system::dma_audit() == 0
                            && system::domain_audit() == 0
                            && system::vspace_audit() == 0;
                        DMA_OK.store(ok, Ordering::Release);
                        DMA_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // PCIe-Enumeration (ext-23, D1): synchrones kernel-/Trusted-Setup, ein Schritt.
        // Gegate auf dma fertig (sequenziell, vor den Fuzzern).
        if !PCIE_DONE.load(Ordering::Acquire) && DMA_DONE.load(Ordering::Acquire) {
            if let Some(d) = system::pcie_find_virtio() {
                PCIE_VENDOR.store(d.vendor as u32, Ordering::Relaxed);
                PCIE_DEVICE.store(d.device as u32, Ordering::Relaxed);
                PCIE_RID.store(d.rid(), Ordering::Relaxed);
                let bar = d.bars.iter().copied().find(|&b| b != 0).unwrap_or(0);
                PCIE_BAR.store(bar, Ordering::Relaxed);
                PCIE_BM.store(hal::pcie::bus_master_enabled(&d), Ordering::Relaxed);
                // Negativ: Suche nach einem Bogus-Vendor liefert nichts (kein Seiteneffekt).
                PCIE_NEG.store(hal::pcie::find(0xdead, &[]).is_none(), Ordering::Relaxed);
                let ok = d.vendor == hal::pcie::VIRTIO_VENDOR
                    && d.device != 0
                    && bar != 0
                    && PCIE_BM.load(Ordering::Relaxed)
                    && PCIE_NEG.load(Ordering::Relaxed)
                    && system::dma_audit() == 0;
                PCIE_OK.store(ok, Ordering::Release);
            }
            // Auch bei "nicht gefunden" abschliessen (OK bleibt false -> sichtbarer FAIL,
            // statt die Suite haengen zu lassen).
            PCIE_DONE.store(true, Ordering::Release);
        }

        // SMMUv3-Bring-up (ext-23, D2): den DmaEnforcer initialisieren + Spike. Synchrones
        // kernel-/Trusted-Setup, ein Schritt. Gegate auf pcie fertig (sequenziell, vor Fuzzern).
        if !SMMU_DONE.load(Ordering::Acquire) && PCIE_DONE.load(Ordering::Acquire) {
            let ok_init = system::dma_enforcer_init();
            SMMU_IDR0.store(system::testsupport::smmu_idr0(), Ordering::Relaxed);
            SMMU_SID.store(system::testsupport::smmu_sid_bits(), Ordering::Relaxed);
            SMMU_SYNC.store(system::testsupport::smmu_sync_ok(), Ordering::Relaxed);
            SMMU_EN.store(system::testsupport::smmu_enabled(), Ordering::Relaxed);
            SMMU_EVTQ.store(system::testsupport::smmu_eventq_empty(), Ordering::Relaxed);
            SMMU_GERR.store(system::testsupport::smmu_gerror(), Ordering::Relaxed);
            // Die arch-neutrale Gesundheit HIER erfassen, nicht im Bericht: ein Wert wird dort
            // festgestellt, wo die Aussage gilt. Im Bericht darf die Einheit sich laengst anders
            // verhalten -- dieselbe Lehre wie beim Treiberpuffer, der dem LETZTEN Client gehoert.
            let hl = hal::iommu::health(SMMU_SYNC.load(Ordering::Relaxed));
            IOHEALTH_OK.store(hl.ok(), Ordering::Relaxed);
            IOHEALTH_SPEAKING.store(hl.speaking(), Ordering::Relaxed);
            IOHEALTH_UNITS.store(hl.units, Ordering::Relaxed);
            IOHEALTH_SPEAK_N.store(hl.units_speaking, Ordering::Relaxed);
            IOHEALTH_HWERR.store(hl.hw_error, Ordering::Relaxed);
            IOHEALTH_CFGERR.store(hl.config_errors, Ordering::Relaxed);
            IOHEALTH_XLAT.store(hl.translation_enabled, Ordering::Relaxed);
            IOHEALTH_FAULTS_EMPTY.store(hl.faults_empty, Ordering::Relaxed);
            IOHEALTH_PRESENT.store(hl.present, Ordering::Relaxed);
            let ok = system::testsupport::smmu_present()
                && ok_init
                && SMMU_EN.load(Ordering::Relaxed)
                && SMMU_SYNC.load(Ordering::Relaxed)
                && SMMU_EVTQ.load(Ordering::Relaxed)
                && SMMU_GERR.load(Ordering::Relaxed) == 0
                && system::dma_audit() == 0;
            SMMU_OK.store(ok, Ordering::Release);
            SMMU_DONE.store(true, Ordering::Release);
        }

        // SMMU-Bindung (ext-23, D3): enable_dma/disable_dma fuer die virtio-RNG-StreamID auf die
        // D0-DMA-Region. Synchrones Setup, ein Schritt. Gegate auf smmu fertig.
        if !SMMUB_DONE.load(Ordering::Acquire) && SMMU_DONE.load(Ordering::Acquire) {
            let rid = PCIE_RID.load(Ordering::Acquire);
            let base = DMA_PHYS.load(Ordering::Acquire);
            SMMUB_RID.store(rid, Ordering::Relaxed);
            // IRQs aus -> saubere total_free-Messung (enable alloziert Stage-1-/CD-Frames,
            // disable gibt sie wieder frei -> balanciert).
            hal::cpu::local_irq_disable();
            let free0 = system::total_free();
            let en = system::dma_enable(rid, base, DMA_LEN).is_some();
            SMMUB_ENABLE.store(en, Ordering::Release);
            SMMUB_EVTQ.store(
                system::testsupport::smmu_eventq_empty() && system::testsupport::smmu_gerror() == 0,
                Ordering::Release,
            );
            let mid_audit = system::dma_audit();
            system::dma_disable(rid, base, DMA_LEN);
            let free1 = system::total_free();
            hal::cpu::local_irq_enable();
            SMMUB_BALANCED.store(free1 == free0, Ordering::Release);
            let ok = en
                && SMMUB_EVTQ.load(Ordering::Acquire)
                && mid_audit == 0
                && free1 == free0
                && system::testsupport::smmu_eventq_empty()
                && system::dma_audit() == 0;
            SMMUB_OK.store(ok, Ordering::Release);
            SMMUB_DONE.store(true, Ordering::Release);
        }

        // virtio-rng-DMA End-to-End (ext-23, D4): echter Bus-Master-DMA hinter der SMMU +
        // Kronjuwel-Sensitivitaet. Synchrones kernel-/Trusted-Setup, ein Schritt. Gegate auf
        // smmubind fertig (sequenziell, vor den Fuzzern).
        if !VRNG_DONE.load(Ordering::Acquire) && SMMUB_DONE.load(Ordering::Acquire) {
            let res = system::virtio_rng_dma_demo();
            VRNG_USED.store(res.used_adv, Ordering::Relaxed);
            VRNG_WRITTEN.store(res.written, Ordering::Relaxed);
            VRNG_R0.store(res.rand0, Ordering::Relaxed);
            VRNG_R1.store(res.rand1, Ordering::Relaxed);
            VRNG_EVTQ.store(res.evtq_empty_good, Ordering::Relaxed);
            VRNG_CJ_SW.store(res.cj_sw_blocked, Ordering::Relaxed);
            VRNG_CJ_SENT.store(res.cj_sentinel_ok, Ordering::Relaxed);
            VRNG_CJ_UNGUARDED.store(res.cj_unguarded_wrote, Ordering::Relaxed);
            VRNG_SMMU_ENF.store(res.cj_smmu_enforced, Ordering::Relaxed);
            VRNG_EVT_XLAT.store(res.cj_evt_translation, Ordering::Relaxed);
            VRNG_EVT_SID.store(res.cj_evt_sid_ok, Ordering::Relaxed);
            VRNG_EVT_IN.store(res.cj_evt_input_ok, Ordering::Relaxed);
            VRNG_AXES.store(res.cj_axes_differ, Ordering::Relaxed);
            VRNG_BYPASS.store(res.cj_bypass_wrote, Ordering::Relaxed);
            VRNG_LIVE.store(res.evtq_liveness, Ordering::Relaxed);
            VRNG_CFGERR0.store(res.cfg_errors_zero, Ordering::Relaxed);
            // PASS = echter DMA + Level-1-Software-Erzwingung (demonstrierbar). cj_smmu_enforced
            // (Level 2) ist unter QEMU fuer emulierte Geraete nicht beobachtbar -> NICHT gefordert.
            let ok = res.found
                && res.used_adv
                && res.written > 0
                && (res.rand0 != 0 || res.rand1 != 0) // Gerät hat echte Bytes DMAt
                && res.evtq_empty_good //                In-Window: keine SMMU-Faults
                && res.cj_sw_blocked //                  L1: Out-of-Window software-abgewiesen
                && res.cj_sentinel_ok //                 L1 aktiv: Ziel unveraendert
                // Sensitivitaetskontrolle: mit Bypass-STE (Durchsetzung aufgehoben) schreibt
                // dasselbe Geraet dieselbe Adresse wirklich. Erst damit ist das Ausbleiben des
                // Schreibzugriffs oben eine Aussage ueber die SMMU und nicht ueber ein Geraet,
                // das aus irgendeinem Grund gar nicht mehr DMAt. Die frueher hier stehende
                // "schreibt ODER faultet"-Fassung war eingeklappt: sie prueft dieselbe
                // Beobachtung wie die Hauptaussage und kann nicht fehlschlagen, waehrend diese
                // besteht.
                && res.cj_bypass_wrote
                // Queue-Liveness: "Event-Queue leer" oben zaehlt nur, wenn die Queue in diesem
                // Lauf nachweislich sprechen konnte.
                && res.evtq_liveness
                && res.cfg_errors_zero //                Einheit lehnt die Tabellen nicht ab
                && res.cj_axes_differ //                 ext-36b: die Demo lief mit IOVA != PA
                // Beobachtete die SMMU den Fault (reale HW), muss der Eintrag **passen** —
                // sonst belegt er nichts. Unter QEMU ist `cj_smmu_enforced` false und die
                // Implikation trivial wahr; die Forderung ist also nur dort scharf, wo sie
                // beobachtbar ist, und dort dann vollstaendig.
                && (!res.cj_smmu_enforced
                    || (res.cj_evt_translation && res.cj_evt_sid_ok && res.cj_evt_input_ok))
                && res.audit_ok
                && system::domain_audit() == 0
                && system::vspace_audit() == 0;
            VRNG_OK.store(ok, Ordering::Release);
            VRNG_DONE.store(true, Ordering::Release);
        }

        // Generische DMA-Infrastruktur (ext-24): Richtung/Kohärenz (strukturell), Multi-Region-
        // Kontext, Stream-Gruppen, Scatter-Gather-Validierung, disjunkte Sub-Puffer. Synchron, ein Schritt.
        // Gegate auf virtiorng (ext-23) fertig (SMMU initialisiert).
        if !DMAGEN_DONE.load(Ordering::Acquire) && VRNG_DONE.load(Ordering::Acquire) {
            DMAGEN_OK.store(run_dmagen(), Ordering::Release);
            DMAGEN_DONE.store(true, Ordering::Release);
        }

        // Prozess-Heap (ext-25): ein Trusted-SAS-Kontext nutzt echten Box/Vec/BTreeMap-Heap.
        // Synchron, ein Schritt. Gegate auf dmagen (ext-24) fertig.
        // IOVA-Fenstergrenzen (ext-36b): laut scheitern statt still abschneiden. Synchron.
        if !DMAWIN_DONE.load(Ordering::Acquire) && DMAGEN_DONE.load(Ordering::Acquire) {
            DMAWIN_OK.store(run_dmawin(), Ordering::Release);
            DMAWIN_DONE.store(true, Ordering::Release);
        }

        // Teardown-Token (ext-37). Synchron, ein Schritt.
        if !DMATOK_DONE.load(Ordering::Acquire) && DMAWIN_DONE.load(Ordering::Acquire) {
            DMATOK_OK.store(run_dmatok(), Ordering::Release);
            DMATOK_DONE.store(true, Ordering::Release);
        }

        if !SASHEAP_DONE.load(Ordering::Acquire) && DMATOK_DONE.load(Ordering::Acquire) {
            SASHEAP_OK.store(run_sasheap(), Ordering::Release);
            SASHEAP_DONE.store(true, Ordering::Release);
        }

        // Binary-Loader (ext-26, L1): extern gebautes `hello` laden (gegate auf sasheap fertig),
        // dann sein Start-Signal in spaeteren Iterationen pollen (hello laeuft dazwischen).
        if !LOAD_STARTED.load(Ordering::Acquire) && SASHEAP_DONE.load(Ordering::Acquire) {
            let ntfn = run_load_start();
            LOAD_NTFN.store(ntfn, Ordering::Relaxed);
            if ntfn == usize::MAX {
                LOAD_DONE.store(true, Ordering::Release); // Laden fehlgeschlagen -> FAIL
            }
            LOAD_STARTED.store(true, Ordering::Release);
        }
        if LOAD_STARTED.load(Ordering::Acquire) && !LOAD_DONE.load(Ordering::Acquire) {
            let ntfn = LOAD_NTFN.load(Ordering::Relaxed);
            if ntfn != usize::MAX && system::notification_pending(ntfn) == HELLO_BADGE {
                LOAD_OK.store(true, Ordering::Release);
                LOAD_DONE.store(true, Ordering::Release);
            } else if LOAD_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                LOAD_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // Binary-Loader L2 (ext-26): cap-gegatetes Laden zur Laufzeit via SYS_LOAD. Gegate auf
        // load (L1) fertig; danach das Caller-Ergebnis + hellos Signal pollen.
        if !SYSLOAD_STARTED.load(Ordering::Acquire) && LOAD_DONE.load(Ordering::Acquire) {
            let n = run_sysload_start();
            SYSLOAD_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                SYSLOAD_FIN.store(true, Ordering::Release); // Setup fehlgeschlagen -> FAIL
            }
            SYSLOAD_STARTED.store(true, Ordering::Release);
        }
        if SYSLOAD_STARTED.load(Ordering::Acquire) && !SYSLOAD_FIN.load(Ordering::Acquire) {
            let n = SYSLOAD_NTFN.load(Ordering::Relaxed);
            // Caller fertig + SYS_LOAD OK + Negativfall ERR_BADCAP + hello signalisierte ueber die
            // delegierte Notification-Cap.
            if SYSLOAD_DONE.load(Ordering::Acquire)
                && n != usize::MAX
                && system::notification_pending(n) == HELLO_BADGE
            {
                let ok = SYSLOAD_RESULT.load(Ordering::Relaxed) == result::OK
                    && SYSLOAD_NEG_OK.load(Ordering::Relaxed);
                SYSLOAD_OK.store(ok, Ordering::Release);
                SYSLOAD_FIN.store(true, Ordering::Release);
            } else if SYSLOAD_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                SYSLOAD_FIN.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // Binary-Loader L3 (ext-26): HardwareLand-Programm laden (Backend+Partner+Kanal) + EL0-TrustedSAS-Laden.
        // Gegate auf sysload (L2) fertig.
        if !LOADHW_STARTED.load(Ordering::Acquire) && SYSLOAD_FIN.load(Ordering::Acquire) {
            LOADTRUSTED_EL0.store(check_loadtrusted_el0(), Ordering::Relaxed); // TrustedSAS EL0-geladen?
            let n = run_loadhw_start();
            LOADHW_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                LOADHW_FIN.store(true, Ordering::Release); // Setup fehlgeschlagen -> FAIL
            }
            LOADHW_STARTED.store(true, Ordering::Release);
        }
        if LOADHW_STARTED.load(Ordering::Acquire) && !LOADHW_FIN.load(Ordering::Acquire) {
            let n = LOADHW_NTFN.load(Ordering::Relaxed);
            if n != usize::MAX && system::notification_pending(n) == HELLO_BADGE {
                // HardwareLand-Programm signalisierte seinen Kanal UND das Trust-Gate wies EL1 ab.
                LOADHW_OK.store(LOADTRUSTED_EL0.load(Ordering::Relaxed), Ordering::Release);
                LOADHW_FIN.store(true, Ordering::Release);
            } else if LOADHW_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                LOADHW_FIN.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // Binary-Loader L4 (ext-26): Teardown geladener Prozesse (Balance). Synchron, ein Schritt.
        // Gegate auf loadhw (L3) fertig.
        if !LOADSTOP_DONE.load(Ordering::Acquire) && LOADHW_FIN.load(Ordering::Acquire) {
            LOADSTOP_OK.store(run_loadstop(), Ordering::Release);
            LOADSTOP_DONE.store(true, Ordering::Release);
        }

        // (Binary-Loader L5 / Loader-Fuzzer laeuft jetzt in `fuzz::drive()`, ADR 0013; gegate auf
        // loadstop. Im Release-Build ohne Fuzzer entfaellt er.)

        // ext-27 T0: UserLand-Aggressor (extern geladener Dienst greift den Kernel via Syscall-ABI
        // an). Start gegate auf den Binary-Loader L5 (Loader-Fuzzer) fertig -- bzw. im Release-Build
        // ohne Fuzzer direkt auf loadstop (das Gate gibt den Vorgaenger durch). SUCCESS-Badge pollen.
        if !AGGRU_STARTED.load(Ordering::Acquire)
            && fuzz::loaderfuzz_gate(LOADSTOP_DONE.load(Ordering::Acquire))
        {
            let n = run_el0_aggressor("aggressor-u", AGGRU_SUCCESS);
            AGGRU_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                AGGRU_DONE.store(true, Ordering::Release); // Setup fehlgeschlagen -> FAIL
            }
            AGGRU_STARTED.store(true, Ordering::Release);
        }
        if AGGRU_STARTED.load(Ordering::Acquire) && !AGGRU_DONE.load(Ordering::Acquire) {
            let n = AGGRU_NTFN.load(Ordering::Relaxed);
            if n != usize::MAX && system::notification_pending(n) == AGGRU_SUCCESS {
                AGGRU_OK.store(ext27_audits_ok(), Ordering::Release);
                AGGRU_DONE.store(true, Ordering::Release);
            } else if AGGRU_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                AGGRU_DONE.store(true, Ordering::Release); // Timeout -> FAIL (Angriff durchgelassen?)
            }
        }

        // ext-27 T1: UserLand-Intruder (Speicher-Isolation). Start gegate auf aggru fertig; die
        // el0_fault_count-Baseline VOR dem Laden schnappen. Dann auf PRE-Badge + Fault-Inkrement
        // pollen. Der Dienst terminiert per Fault (kein destroy_loaded; Reap gibt Kstack/VSpace frei).
        if !INTRU_STARTED.load(Ordering::Acquire) && AGGRU_DONE.load(Ordering::Acquire) {
            INTRU_FAULT_BASE.store(system::el0_fault_count(), Ordering::Relaxed);
            let n = run_el0_intruder("intruder-u", INTRU_PRE);
            INTRU_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                INTRU_DONE.store(true, Ordering::Release); // Setup fehlgeschlagen -> FAIL
            }
            INTRU_STARTED.store(true, Ordering::Release);
        }
        if INTRU_STARTED.load(Ordering::Acquire) && !INTRU_DONE.load(Ordering::Acquire) {
            let n = INTRU_NTFN.load(Ordering::Relaxed);
            let base = INTRU_FAULT_BASE.load(Ordering::Relaxed);
            let pre = n != usize::MAX && system::notification_pending(n) == INTRU_PRE;
            let faulted = base != usize::MAX && system::el0_fault_count() > base;
            if pre && faulted {
                // PRE erhalten (Dienst lief) UND er faultete beim Kernel-RAM-Zugriff (Isolation
                // hielt). Audits sauber -> der Angriff hatte keinen Korruptions-/Leak-Effekt.
                INTRU_OK.store(ext27_audits_ok(), Ordering::Release);
                INTRU_DONE.store(true, Ordering::Release);
            } else if INTRU_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                INTRU_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // ext-27 T2a: HardwareLand-Aggressor. Start gegate auf intru fertig; das SUCCESS-Badge ueber
        // den Kanal pollen. Der Dienst beendet sich selbst (exit) nach dem Signal.
        if !AGGRH_STARTED.load(Ordering::Acquire) && INTRU_DONE.load(Ordering::Acquire) {
            let n = run_hw_service_start("aggressor-h", AGGRH_SUCCESS, 0x27);
            AGGRH_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                AGGRH_DONE.store(true, Ordering::Release);
            }
            AGGRH_STARTED.store(true, Ordering::Release);
        }
        if AGGRH_STARTED.load(Ordering::Acquire) && !AGGRH_DONE.load(Ordering::Acquire) {
            let n = AGGRH_NTFN.load(Ordering::Relaxed);
            if n != usize::MAX && system::notification_pending(n) == AGGRH_SUCCESS {
                AGGRH_OK.store(ext27_audits_ok(), Ordering::Release);
                AGGRH_DONE.store(true, Ordering::Release);
            } else if AGGRH_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                AGGRH_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // ext-27 T2b: HardwareLand-Intruder (Speicher-Isolation domaenen-unabhaengig). Start gegate
        // auf aggrh fertig; Fault-Baseline schnappen, dann PRE-Badge + Fault-Inkrement pollen.
        if !INTRH_STARTED.load(Ordering::Acquire) && AGGRH_DONE.load(Ordering::Acquire) {
            INTRH_FAULT_BASE.store(system::el0_fault_count(), Ordering::Relaxed);
            let n = run_hw_service_start("intruder-h", INTRH_PRE, 0x28);
            INTRH_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                INTRH_DONE.store(true, Ordering::Release);
            }
            INTRH_STARTED.store(true, Ordering::Release);
        }
        if INTRH_STARTED.load(Ordering::Acquire) && !INTRH_DONE.load(Ordering::Acquire) {
            let n = INTRH_NTFN.load(Ordering::Relaxed);
            let base = INTRH_FAULT_BASE.load(Ordering::Relaxed);
            let pre = n != usize::MAX && system::notification_pending(n) == INTRH_PRE;
            let faulted = base != usize::MAX && system::el0_fault_count() > base;
            if pre && faulted {
                INTRH_OK.store(ext27_audits_ok(), Ordering::Release);
                INTRH_DONE.store(true, Ordering::Release);
            } else if INTRH_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                INTRH_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // ext-27 T3a: TrustedSAS-Aggressor (Trust != Privileg). Start gegate auf intrh fertig.
        if !AGGRT_STARTED.load(Ordering::Acquire) && INTRH_DONE.load(Ordering::Acquire) {
            let n = run_el0_aggressor("aggressor-t", AGGRT_SUCCESS);
            AGGRT_NTFN.store(n, Ordering::Relaxed);
            if n == usize::MAX {
                AGGRT_DONE.store(true, Ordering::Release);
            }
            AGGRT_STARTED.store(true, Ordering::Release);
        }
        if AGGRT_STARTED.load(Ordering::Acquire) && !AGGRT_DONE.load(Ordering::Acquire) {
            let n = AGGRT_NTFN.load(Ordering::Relaxed);
            if n != usize::MAX && system::notification_pending(n) == AGGRT_SUCCESS {
                AGGRT_OK.store(ext27_audits_ok(), Ordering::Release);
                AGGRT_DONE.store(true, Ordering::Release);
            } else if AGGRT_POLLS.fetch_add(1, Ordering::Relaxed) > 200_000 {
                AGGRT_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // ext-28 (umgewidmet von ext-27 T3b): UNZERTIFIZIERTES TrustedSAS wird ABGELEHNT. intruder-t
        // liegt OHNE Zertifikat im Archiv (es traegt absichtlich `unsafe` -> nicht zertifizierbar);
        // das verify_image-Gate (ADR 0014) muss das Laden mit `Unverified` abweisen, OHNE dass ein
        // Thread/eine PD entsteht und ohne Audit-Stoerung. Staerker als „laeuft + faultet": das
        // Binary laeuft gar nicht erst an. (Die EL0-Isolation der Trusted-Domaene bleibt durch
        // intruder-u/intruder-h domaenenunabhaengig belegt.) Synchron, gegate auf aggrt fertig.
        if !INTRT_STARTED.load(Ordering::Acquire) && AGGRT_DONE.load(Ordering::Acquire) {
            let rejected = trusted_load_rejected("intruder-t");
            INTRT_OK.store(rejected && ext27_audits_ok(), Ordering::Release);
            INTRT_DONE.store(true, Ordering::Release);
            INTRT_STARTED.store(true, Ordering::Release);
        }

        // ext-27 T4: Cross-Service-Matrix (3 Domaenen nebenlaeufig). Start gegate auf intrt fertig.
        if !CROSS_STARTED.load(Ordering::Acquire) && INTRT_DONE.load(Ordering::Acquire) {
            if !run_cross_start() {
                CROSS_DONE.store(true, Ordering::Release); // Setup fehlgeschlagen -> FAIL
            }
            CROSS_STARTED.store(true, Ordering::Release);
        }
        if CROSS_STARTED.load(Ordering::Acquire) && !CROSS_DONE.load(Ordering::Acquire) {
            let nu = CROSS_NU.load(Ordering::Relaxed);
            let nt = CROSS_NT.load(Ordering::Relaxed);
            let nh = CROSS_NH.load(Ordering::Relaxed);
            let base = CROSS_FAULT_BASE.load(Ordering::Relaxed);
            let su = nu != usize::MAX && system::notification_pending(nu) == CROSS_U;
            let st = nt != usize::MAX && system::notification_pending(nt) == CROSS_T;
            let sh = nh != usize::MAX && system::notification_pending(nh) == CROSS_H;
            let faulted = base != usize::MAX && system::el0_fault_count() > base;
            if su && st && sh && faulted {
                // Beide Aggressoren (U+T) meldeten unabhaengig SUCCESS, der Intruder (H) faultete --
                // alle drei NEBENLAEUFIG. Jetzt: Canary unberuehrt + Audits sauber.
                let canary_ok = peek_u64(CROSS_CANARY.load(Ordering::Relaxed)) == CROSS_CANARY_VAL;
                CROSS_OK.store(canary_ok && ext27_audits_ok(), Ordering::Release);
                CROSS_DONE.store(true, Ordering::Release);
            } else if CROSS_POLLS.fetch_add(1, Ordering::Relaxed) > 300_000 {
                CROSS_DONE.store(true, Ordering::Release); // Timeout -> FAIL
            }
        }

        // In-Kernel-Fuzzer (ADR 0013): je Tick den faelligen Fuzzer-Schritt fahren (loaderfuzz nach
        // loadstop, hwfuzz nach cross, fuzz nach stale/strand/hwfuzz, ipcfuzz nach fuzz). Im Release-
        // Build (ohne Feature `kernel-fuzz`) ist `fuzz::drive()` ein No-Op -- kein Fuzzer-Code.
        fuzz::drive();

        // Audit-Regression B (MCS-Stranding): budgetierten Worker auf STRAND_CORE
        // erschöpfen lassen, dann erneut binden -> er muss WIEDER laufen (Fix). Gegate
        // auf MCS fertig + SMP fertig (STRAND_CORE frei). Tick-basiert (anderer Kern).
        if !STRAND_DONE.load(Ordering::Acquire)
            && MCS_DONE.load(Ordering::Acquire)
            && (1..system::num_cores())
                .all(|c| XCORE_PROGRESS[c].load(Ordering::Relaxed) >= SMP_WORK_TARGET)
        {
            match STRAND_STEP.load(Ordering::Acquire) {
                0 => {
                    // Worker auf STRAND_CORE erzeugen + knappes Budget binden (budget=1,
                    // lange Periode -> nach Erschöpfung KEIN natürlicher Refill im Fenster).
                    if let Some(w) = system::spawn_on_core(
                        STRAND_CORE,
                        strand_worker as *const () as usize,
                        0,
                        3,
                    ) {
                        STRAND_TID.store(w.to_raw(), Ordering::Relaxed);
                        if let Ok(sc) = system::install_sched_context_cap(1, 10_000, Rights::WRITE)
                        {
                            system::bind_sched_context(sc, STRAND_CORE, w);
                        }
                        STRAND_STEP.store(1, Ordering::Release);
                    }
                }
                1 => {
                    // Warten, bis der Worker erschöpft ist (deplaniert, nicht in der Queue).
                    let (depl, _) = system::budget_stats(STRAND_CORE);
                    if depl >= 1 {
                        STRAND_SNAP1.store(STRAND_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
                        STRAND_STEP.store(2, Ordering::Release);
                    }
                }
                2 => {
                    // Erneut binden (set_budget auf den ERSCHÖPFTEN Thread). Mit Fix wird er
                    // wieder eingereiht und läuft; ohne Fix bleibt er für immer gestrandet.
                    let w = ThreadId::from_raw(STRAND_TID.load(Ordering::Relaxed));
                    if let Ok(sc) = system::install_sched_context_cap(50, 100, Rights::WRITE) {
                        system::bind_sched_context(sc, STRAND_CORE, w);
                    }
                    STRAND_SETTLE.store(0, Ordering::Relaxed);
                    STRAND_STEP.store(3, Ordering::Release);
                }
                _ => {
                    // Einige Ticks setzen lassen, dann prüfen, dass der Zähler seit dem
                    // Snapshot gewachsen ist (Worker lief nach dem Re-Bind wieder).
                    let s = STRAND_SETTLE.fetch_add(1, Ordering::Relaxed);
                    if s >= 40 {
                        let grew = STRAND_COUNT.load(Ordering::Relaxed)
                            > STRAND_SNAP1.load(Ordering::Relaxed);
                        STRAND_STOP.store(true, Ordering::Release); // Worker parken lassen
                        STRAND_OK.store(grew, Ordering::Release);
                        STRAND_DONE.store(true, Ordering::Release);
                    }
                }
            }
        }

        // Gegate auf `cross` (letzter adversarialer Dienst), `strand` (letzte Scheduler-Messung)
        // und `loadstop` (Ressourcen-Bilanz). Danach nimmt keine Pruefung mehr eine Baseline, und
        // genau deshalb steht der Farbtest hier: an seinem alten Platz (Anfang von `spawn_demo`)
        // hat er `captest` und `sched` gekippt. Ein Test, der andere Tests kippt, macht das
        // GESAMTE Ergebnis unbrauchbar -- das wiegt schwerer als drei zusaetzliche gruene Zeilen.
        //
        // Wer eine baseline-empfindliche Pruefung dahinter einhaengt, muss dieses Gate ergaenzen.
        if !COLOR_DONE.load(Ordering::Acquire)
            && CROSS_DONE.load(Ordering::Acquire)
            && STRAND_DONE.load(Ordering::Acquire)
            && LOADSTOP_DONE.load(Ordering::Acquire)
        {
            run_color_suite();
            COLOR_DONE.store(true, Ordering::Release);
        }

        // (Generativer Fuzzer + IPC-State-Machine-Fuzzer werden jetzt in `fuzz::drive()` gespawnt,
        // ADR 0013.)

        if !reported && ALL_DONE.load(Ordering::Acquire) && all_done(None) {
            report();
            reported = true;
            // Soak (Burn-in #2): statt herunterzufahren in den Dauerbetrieb gehen (Endlosschleife,
            // kehrt nie zurueck). Nur mit Feature `soak`; der Kernel-Kern ist dabei unveraendert.
            #[cfg(feature = "soak")]
            {
                println!(
                    "== SELFTEST COMPLETE -> SOAK (Dauerbetrieb einer Instanz, kein system_off) =="
                );
                soak::run();
            }
            // Alle Tests bestanden -> die (virtuelle) Maschine sauber herunterfahren,
            // damit das Test-Skript die vollständige Ausgabe erhält, ohne bis zum
            // Timeout warten zu müssen (verhindert ein Abschneiden des Berichts und
            // erlaubt umfangreichere Fuzz-Läufe innerhalb des Zeitfensters). Bei einem
            // Fehlschlag (all_done nie wahr) bleibt der Kernel im Idle -> Timeout greift.
            #[cfg(not(feature = "soak"))]
            {
                println!("== SELFTEST COMPLETE -> system_off ==");
                hal::power::system_off();
            }
        } else if !reported && hal::timer::ticks(0) > 6000 {
            // BUGFIX (nach Burn-in #1): Watchdog. Wird ein synchroner Test selten DONE-aber-OK=false
            // (z. B. eine flaky Messung), wuerde all_done() NIE true -> der Kernel spinnt ewig im
            // Idle (Hang). Deadline: 6000 Timer-Ticks @ TICK_HZ=100 = ~60 s (Normalabschluss ~6 s,
            // Burn-in-Backstop 120 s). Dann den Zustand melden -- report() zeigt je Test ALL PASS/
            // FAILURES, also welcher Test scheiterte -- und als FEHLER sauber herunterfahren.
            // So wird eine Per-Test-Flakiness zu einem GEMELDETEN FAILURE statt zu einem Hang.
            reported = true;
            println!(
                "== SELFTEST WATCHDOG: all_done() nicht erreicht nach ~60s -> offene Tests: =="
            );
            // **Die Liste, die die Kopfzeile verspricht.** Ohne sie ist ein Haenger nicht
            // klassifizierbar -- der volle Bericht darunter unterscheidet eine nie gesetzte
            // Aussage nicht von einer bestandenen.
            let mut w = [("", true); DONE_FLAGS_ARM];
            let _ = all_done(Some(&mut w));
            // Eine Zeile je offene Aussage: `print!` ist hier nicht importiert, und eine
            // Sammelzeile waere ohnehin abgeschnitten, sobald mehrere offen sind.
            for (n, v) in w {
                if !v {
                    println!("bringup : offen: {n}");
                }
            }
            report();
            println!("== SELFTEST FAILED (watchdog) -> system_off ==");
            hal::power::system_off();
        }
        hal::cpu::wfi();
    }
}

/// Die Zahl der benannten Abschlussaussagen (aarch64).
///
/// 2026-08-17: 59 -> 60 durch `numa` (Z8 N1).
/// 2026-08-17: 58 -> 59 durch `smt` (Z6 Stufe 1).
/// 2026-08-17: 57 -> 58 durch `iohealth`, die arch-neutrale IOMMU-Gesundheit. Dass diese Zahl von
/// Hand gefuehrt wird, ist genau die Form, die `system::MELDESTELLEN` am 2026-08-11 losgeworden
/// ist (abgeleitet statt gezaehlt) -- hier faengt der Typ es wenigstens beim Bau ab, weil die
/// Liste ein Array fester Laenge ist. **Der Bau hat es auch getan**, und zwar nur unter
/// `--features selftest`: ein Bau ohne das Merkmal enthaelt diese Datei gar nicht.
const DONE_FLAGS_ARM: usize = 62;

fn all_done(warum: Option<&mut [(&'static str, bool); DONE_FLAGS_ARM]>) -> bool {
    let workers = (0..NWORKERS).all(|i| WORKER_COUNTS[i].load(Ordering::Relaxed) >= THRESHOLD);
    // Z6 Stufe 1b: nur ONLINE-Kerne muessen ticken. Ein unterdruecktes SMT-Geschwister hat
    // Tabellen (`num_cores()` zaehlt es), aber keinen Timer -- `ticks == 0` ist dort die
    // Politik, kein Defekt. Wer ueber die konfigurierte Zahl ginge, verlangte von einem
    // Kern, der absichtlich nie bootet, einen Herzschlag -- und triebe jeden SMT-Lauf in
    // den Watchdog (`offen waren: cores`). Die Erwartung kommt aus `core_online()` (dem
    // Bit hinter `cores_online()`), nicht aus der Konfiguration. Ohne SMT ist jedes
    // konfigurierte Bit gesetzt und die Zeile buchstabengleich zur alten.
    let cores = (0..system::num_cores())
        .filter(|&c| caprock_sched::core_online(c))
        .all(|c| hal::timer::ticks(c) > 0);
    let fp = FP_COLLECTOR_DONE.load(Ordering::Acquire) && system::fp_switch_count() > 0;
    let prio = (0..NPRIO_TEST).all(|i| PRIO_DONE[i].load(Ordering::Acquire));
    let life = KILLER_DONE.load(Ordering::Acquire) && REAPED.load(Ordering::Relaxed) >= 2;
    let notif = PRODUCER_DONE.load(Ordering::Acquire)
        && NOTIF_COUNT.load(Ordering::Relaxed) >= NOTIF_ROUNDS;
    let xfer = XFER_DONE.load(Ordering::Acquire) && XFER_GRANTLK_DONE.load(Ordering::Acquire);
    let migrate = MIG_DONE.load(Ordering::Acquire);
    let scale = SCALE_DONE.load(Ordering::Acquire);
    let ckpt = CS_DONE.load(Ordering::Acquire);
    let el0 = USER_RECV.load(Ordering::Relaxed) == USER_MAGIC && system::el0_syscall_seen();
    let el0iso = system::el0_fault_count() >= 1;
    // SMP: alle Sekundärkerne haben ihren Worker abgearbeitet (parallele Einplanung)
    // und der Parker wurde kern-übergreifend per IPI geweckt.
    // Z6 Stufe 1b: Fortschritt wird nur von ONLINE-Kernen erwartet -- ein unterdruecktes
    // SMT-Geschwister laeuft nie und haette nie Fortschritt (derselbe Grund wie beim
    // `cores`-Konjunkt oben). Der IPI-Wake (`woken`) bleibt unbedingt: Faellt er, ist der
    // Cross-Core-Pfad wirklich kaputt -- oder der Parker liegt auf einem toten Kern
    // (feste Platzierung auf Kern 1, s. Spawn-Seite), und dann nennt der Watchdog `smp`
    // beim Namen statt `cores` danebenzuschuldigen.
    let smp = (1..system::num_cores())
        .filter(|&c| caprock_sched::core_online(c))
        .all(|c| XCORE_PROGRESS[c].load(Ordering::Relaxed) >= SMP_WORK_TARGET)
        && XCORE_WOKEN.load(Ordering::Acquire);
    let xipc = XIPC_DONE.load(Ordering::Acquire);
    // Reclaim: alle transienten EL0-Exiter wurden erzeugt (Pool-Slots wiederverwendet).
    let reclaim = RECLAIM_SPAWNED.load(Ordering::Relaxed) >= RECLAIM_TARGET;
    // Lastausgleich: alle lastbewusst platzierten Worker erzeugt.
    let balanced = BALANCED_SPAWNED.load(Ordering::Relaxed) >= BALANCED_TARGET;
    // Weg C: SAS-Probe las X (TRUSTED-Badge), isolierte Probe lief+IPC (RAN-Badge)
    // und faultete dann beim Fremdzugriff (iso_faulted).
    let m = ISO_MASK.load(Ordering::Relaxed);
    let vspace = (m & ISO_BADGE_RAN != 0) && (m & ISO_BADGE_TRUSTED != 0) && system::iso_faulted();
    // VMM: die VMM-Probe mappte+beschrieb einen Frame (MAPPED) und faultete nach dem
    // UNMAP -> beide isolierten Proben (iso + vmm) faulteten => iso_fault_count >= 2.
    let vmm = (m & ISO_BADGE_MAPPED != 0) && system::iso_fault_count() >= 2;
    // Shared-Memory-IPC: der Reader las den Wert des Writers via geteiltem Frame.
    let shm = m & ISO_BADGE_SHARED != 0;
    // Natives Code-Laden: privat geladener Code lief in eigener VSpace.
    let native = m & ISO_BADGE_NATIVE != 0;
    // 4-KiB-Seiten: RW-Schreiben + RO-Lesen einzelner Seiten erfolgreich.
    let pages4k = m & ISO_BADGE_PAGES != 0;
    // Strand-7: die Wachen-Sonde faultete mit FAR == Wache (eingerastet, s.
    // `guard_sonde_urteil`) — in der Abschlussbedingung, nicht bloss im
    // Bericht: eine Zusicherung, die nur druckt, faellt niemandem auf.
    let guard = system::guard_sonde_urteil().3;
    // Churn/Leak: tausende spawn/destroy-Zyklen ohne Ressourcen-Leck.
    let churn = CHURN_DONE.load(Ordering::Acquire) && CHURN_OK.load(Ordering::Acquire);
    // MCS: budget-basiertes Scheduling — budgetierter Thread gedrosselt, Budget gebunden.
    let mcs = MCS_DONE.load(Ordering::Acquire) && MCS_OK.load(Ordering::Acquire);
    // Audit-Regression A: IPC paniert nicht bei totem Eintrag in der Endpoint-Queue.
    let stale = STALE_DONE.load(Ordering::Acquire) && STALE_OK.load(Ordering::Acquire);
    // Audit-Regression B: erneutes Budget-Bind strandet einen erschöpften Thread nicht.
    let strand = STRAND_DONE.load(Ordering::Acquire) && STRAND_OK.load(Ordering::Acquire);
    // Audit-Regression C: toter Reply-Owner entblockt den Client mit ERR_SERVER_GONE.
    let rgone = RGONE_DONE.load(Ordering::Acquire) && RGONE_OK.load(Ordering::Acquire);
    // Budget-Donation: Server-Arbeit wird gegen das Client-Budget belastet (intra-core).
    let ddon = DDON_DONE.load(Ordering::Acquire) && DDON_OK.load(Ordering::Acquire);
    // First-class Reply-Cap: Löschen/Revoke einer Reply-Cap bricht den Call ab.
    let rcap = RCAP_DONE.load(Ordering::Acquire) && RCAP_OK.load(Ordering::Acquire);
    // Reply-Cap-Server-Migration: ausstehender Call überlebt einen Hot-Reload (v2 antwortet).
    let rmig = RMIG_DONE.load(Ordering::Acquire) && RMIG_OK.load(Ordering::Acquire);
    // CAPS-Read-Concurrency: zwei Kerne halten gleichzeitig den CAPS-Read-Lock (RwLock).
    let caplk = CAPLK_DONE.load(Ordering::Acquire) && CAPLK_OK.load(Ordering::Acquire);
    // Sicherheitsdomänen: Domänen-Policy-Oracle konsistent + Domänen round-trippen.
    let domain = DOMAIN_DONE.load(Ordering::Acquire) && DOMAIN_OK.load(Ordering::Acquire);
    // UserLand-Management: cap-gated SYS_PDCTL (PAUSE/RESUME/STOP + cap-/policy-Negativ).
    let pdctl = PDCTL_DONE.load(Ordering::Acquire) && PDCTL_OK.load(Ordering::Acquire);
    // Paarweiser Treiber<->Backend-Kanal: CALL über die unveränderliche Bindung + 1:N + Negativ.
    let chan = CHAN_DONE.load(Ordering::Acquire) && CHAN_OK.load(Ordering::Acquire);
    // RTC-Hardware-Backend: MMIO-Cap + Device-Mapping + echtes RTC_DR-Read über den Kanal.
    let rtc = RTC_DONE.load(Ordering::Acquire) && RTC_OK.load(Ordering::Acquire);
    // RTC-IRQ: IRQ-Cap + GIC-SPI-Routing + Deferred-IRQ-Zustellung an das HardwareLand-Backend.
    let irq = IRQT_DONE.load(Ordering::Acquire) && IRQT_OK.load(Ordering::Acquire);
    // DMA-Capability (ext-23): DmaCap + Normal-NC-Mapping + EL0-Round-Trip + Kohärenz + Audits.
    let dma = DMA_DONE.load(Ordering::Acquire) && DMA_OK.load(Ordering::Acquire);
    // PCIe-Enumeration (ext-23): virtio-rng-pci gefunden, BAR + Bus-Master, RID = StreamID.
    let pcie = PCIE_DONE.load(Ordering::Acquire) && PCIE_OK.load(Ordering::Acquire);
    // SMMUv3-Bring-up (ext-23): DmaEnforcer aktiv (CR0ACK), CMD_SYNC-Spike, Event-Queue leer.
    let smmu = SMMU_DONE.load(Ordering::Acquire) && SMMU_OK.load(Ordering::Acquire);
    // SMMU-Bindung (ext-23): enable_dma/disable_dma (STE/CD/Stage-1), Event-Queue leer, balanciert.
    let smmubind = SMMUB_DONE.load(Ordering::Acquire) && SMMUB_OK.load(Ordering::Acquire);
    // virtio-rng-DMA (ext-23): echter Bus-Master-DMA hinter der SMMU + Kronjuwel-Sensitivität.
    let virtiorng = VRNG_DONE.load(Ordering::Acquire) && VRNG_OK.load(Ordering::Acquire);
    // Generische DMA-Infra (ext-24): Richtung/Kohärenz + Multi-Region + Gruppen + SG + Pool.
    let dmagen = DMAGEN_DONE.load(Ordering::Acquire) && DMAGEN_OK.load(Ordering::Acquire);
    // IOVA-Fenstergrenzen (ext-36b): Erschöpfung + Geräte-Adressbreite als geprüfte Eigenschaft.
    let dmawin = DMAWIN_DONE.load(Ordering::Acquire) && DMAWIN_OK.load(Ordering::Acquire);
    // Teardown-Token (ext-37): Freigabe nur gegen Nachweis; Pending statt UAF.
    let dmatok = DMATOK_DONE.load(Ordering::Acquire) && DMATOK_OK.load(Ordering::Acquire);
    // Cache-Partitionierung (todo A1) + Streifenvergabe (B-4.2). In der ABSCHLUSSBEDINGUNG, nicht
    // bloss im Bericht -- aus demselben Grund, der auf x86 schon fuer B-4.2 notiert ist: eine
    // Zusicherung, die nur gedruckt wird, faellt beim Brechen niemandem auf.
    let color = COLOR_DONE.load(Ordering::Acquire)
        && COLOR_OK.load(Ordering::Acquire)
        && STRIPE_ALLOC_OK.load(Ordering::Acquire)
        && PPROBE_OK.load(Ordering::Acquire);
    // Prozess-Heap (ext-25): echter Box/Vec/BTreeMap-Heap auf realen Physadressen (safe Rust).
    let sasheap = SASHEAP_DONE.load(Ordering::Acquire) && SASHEAP_OK.load(Ordering::Acquire);
    // Binary-Loader L1 (ext-26): extern gebautes hello geladen + lief (signalisierte HELLO_BADGE).
    let load = LOAD_DONE.load(Ordering::Acquire) && LOAD_OK.load(Ordering::Acquire);
    // Binary-Loader L2 (ext-26): cap-gegatetes Laden zur Laufzeit via SYS_LOAD.
    let sysload = SYSLOAD_FIN.load(Ordering::Acquire) && SYSLOAD_OK.load(Ordering::Acquire);
    // Binary-Loader L3 (ext-26): HardwareLand-Laden + TrustedSAS/EL1-Trust-Gate.
    let loadhw = LOADHW_FIN.load(Ordering::Acquire) && LOADHW_OK.load(Ordering::Acquire);
    // Binary-Loader L4 (ext-26): Teardown geladener Prozesse (Balance).
    let loadstop = LOADSTOP_DONE.load(Ordering::Acquire) && LOADSTOP_OK.load(Ordering::Acquire);
    // ext-27 T0: UserLand-Aggressor (extern geladen) — alle Syscall-Angriffe korrekt abgewiesen.
    let aggru = AGGRU_DONE.load(Ordering::Acquire) && AGGRU_OK.load(Ordering::Acquire);
    // ext-27 T1: UserLand-Intruder (extern geladen) — Kernel-RAM-Zugriff aus EL0 faultete (Isolation).
    let intru = INTRU_DONE.load(Ordering::Acquire) && INTRU_OK.load(Ordering::Acquire);
    // ext-27 T2: HardwareLand-Aggressor — keine Management-Autoritaet, nichts ausserhalb des Kanals.
    let aggrh = AGGRH_DONE.load(Ordering::Acquire) && AGGRH_OK.load(Ordering::Acquire);
    // ext-27 T2: HardwareLand-Intruder — Kernel-RAM-Fault domaenen-unabhaengig (Backend isoliert).
    let intrh = INTRH_DONE.load(Ordering::Acquire) && INTRH_OK.load(Ordering::Acquire);
    // ext-27 T3: TrustedSAS-Aggressor — Trust != Privileg (ohne PdControl/Loader-Cap -> BADCAP).
    let aggrt = AGGRT_DONE.load(Ordering::Acquire) && AGGRT_OK.load(Ordering::Acquire);
    // ext-27 T3: TrustedSAS-Intruder — auch ein trusted EL0-Dienst faultet auf Kernel-RAM.
    let intrt = INTRT_DONE.load(Ordering::Acquire) && INTRT_OK.load(Ordering::Acquire);
    // ext-27 T4: Cross-Service-Matrix — 3 Domaenen nebenlaeufig, kein Stoeren, Canary intakt.
    let cross = CROSS_DONE.load(Ordering::Acquire) && CROSS_OK.load(Ordering::Acquire);
    // In-Kernel-Fuzzer (ADR 0013): bei `--features kernel-fuzz` alle vier bestanden; sonst (Stub) true.
    // **Welche Aussage offen ist, muss der Watchdog NENNEN koennen** (2026-08-07).
    //
    // Bis hierher war das eine `&&`-Kette, und die Kopfzeile des Watchdogs versprach „offene
    // Tests:", ohne welche zu nennen -- gedruckt wurde der volle Bericht, in dem eine noch nie
    // gesetzte Aussage von einer bestandenen nicht zu unterscheiden ist. Ein aarch64-Haenger war
    // damit nicht klassifizierbar: die Frage „welches Konjunkt fehlte?" hatte keine Antwort im
    // Protokoll, und die naheliegende Ersatzantwort ist die bequeme („das bekannte Sporadikum").
    // x86 nennt sie seit jeher (`bringup : offen waren: ipc`), und genau diese Zeile hat D0
    // eingegrenzt.
    // Dieselbe Bedingung wie auf x86 -- ein Waechter, den nur eine der beiden Architekturen
    // durchsetzt, ist eine Stichprobe. (Und der Zaehler steht in `system`, gilt also fuer beide.)
    let pdbind = system::LATE_PD_BIND.load(Ordering::Relaxed) == 0
        && system::LATE_PD_BIND_UNKLAR.load(Ordering::Relaxed) == 0
        && system::PD_BIND_GESAMT.load(Ordering::Relaxed) > 0;
    let flags: [(&'static str, bool); DONE_FLAGS_ARM] = [
        ("pdbind", pdbind),
        ("workers", workers),
        ("cores", cores),
        ("fp", fp),
        ("prio", prio),
        ("life", life),
        ("notif", notif),
        ("xfer", xfer),
        ("ckpt", ckpt),
        ("el0", el0),
        ("el0iso", el0iso),
        ("smp", smp),
        ("xipc", xipc),
        ("reclaim", reclaim),
        ("balanced", balanced),
        ("vspace", vspace),
        ("vmm", vmm),
        ("shm", shm),
        ("native", native),
        ("pages4k", pages4k),
        ("guard", guard),
        ("churn", churn),
        ("mcs", mcs),
        ("stale", stale),
        ("strand", strand),
        ("rgone", rgone),
        ("ddon", ddon),
        ("rcap", rcap),
        ("rmig", rmig),
        ("caplk", caplk),
        ("domain", domain),
        ("pdctl", pdctl),
        ("chan", chan),
        ("rtc", rtc),
        ("irq", irq),
        ("dma", dma),
        ("pcie", pcie),
        ("smmu", smmu),
        // Die arch-neutrale Aussage gattert MIT -- eine Zeile, die nur druckt, ist keine Pruefung.
        ("iohealth", IOHEALTH_OK.load(Ordering::Acquire)),
        // **Z6 stage 1: gates from the first day.** Criterion formulated against the EFFECT (no
        // two online logical CPUs share a physical core), recomputed from the IDs that came up.
        //
        // Under QEMU `virt`/`cortex-a72` there is no SMT at all, so this clause is trivially true
        // here — that is a correct reading, not a passing test of the policy. The policy itself is
        // exercised on x86 with `-smp cores=2,threads=2` (`tools/smt-messen.sh`); this line's job
        // on aarch64 is to make sure the reading happens and reports `Single` rather than being
        // absent. A line that exists on one architecture only is how `color` stood hard-wired to
        // `true` here for months.
        ("smt", crate::SMT_OK.load(Ordering::Acquire)),
        // Z8/N1: gates from the first day, same criterion as on x86.
        ("numa", crate::NUMA_OK.load(Ordering::Acquire)),
        ("smmubind", smmubind),
        ("virtiorng", virtiorng),
        ("dmagen", dmagen),
        ("dmawin", dmawin),
        ("dmatok", dmatok),
        ("color", color),
        ("sasheap", sasheap),
        ("load", load),
        ("sysload", sysload),
        ("loadhw", loadhw),
        ("loadstop", loadstop),
        ("aggru", aggru),
        ("intru", intru),
        ("aggrh", aggrh),
        ("intrh", intrh),
        ("aggrt", aggrt),
        ("intrt", intrt),
        ("cross", cross),
        ("migrate", migrate),
        ("scale", scale),
        // K1b: `SYS_SPAWN` mit Teilregion. **`!gattert()` und nicht `ist_bestanden()`**: ein SKIP
        // (kein Speicher, keine PD) haelt den Lauf nicht auf -- es steht im Bericht.
        //
        // **Und wie auf x86 ist das KEIN Gatter** (berichtigt 2026-08-26): `spawnarena::messen()`
        // laeuft in `report()`, also nachdem `all_done()` entschieden hat. Der Eintrag sorgt
        // dafuer, dass die Zeile in der Aufzaehlung des Watchdogs auftaucht; gegattert wird von
        // der Suite. Die Sonde steht dort unten, weil sie Speicher belegt und ein Test, der
        // Speicher belegt, baseline-empfindliche Tests kippt.
        ("arena", !crate::spawnarena::urteil().gattert()),
    ];
    if let Some(w) = warum {
        *w = flags;
    }
    flags.iter().all(|&(_, v)| v) && fuzz::all_passed()
}

/// **Derselbe Waechter auf beiden Architekturen.** Der x86-Bericht sitzt in
/// `arch/x86_64/bringup.rs`, der aarch64-Bericht hier -- zwei Wege, ein Kernel. Ein Waechter, den
/// nur einer der beiden faehrt, ist kein Waechter, sondern eine Stichprobe: genau deshalb liegen
/// auch die DMA-Tests architekturneutral in `dmatests.rs`.
fn pdbind_bericht() {
    use crate::system;
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
        let gesehen = system::SPAETBINDUNG_GESEHEN.load(Ordering::Relaxed);
        println!(
            "pdbind  : gebunden={gesamt} spaet-gebunden={spaet} unklar={unklar} \
             erklaert-gefeuert={gesehen:#x} (D0: eine PD, die an einen bereits zugelassenen Thread \
             geht, kommt zu spaet -- der Thread kann seinen ersten Syscall schon gemacht und \
             ERR_NOPD bekommen haben. Erklaerte Gruende zaehlen NICHT als spaet, stehen aber hier)"
        );
        for g in system::ERLAUBTE_SPAETBINDUNGEN {
            #[cfg(feature = "debug-log")]
            println!("pdbind  :   erklaert zulaessig: {g}");
            #[cfg(not(feature = "debug-log"))]
            let _ = g;
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
}

fn report() {
    pdbind_bericht();

    // **Z6b: die Speicher-Sonde, auf DIESEM Zweig gefahren.**
    //
    // Der ganze uebrige Debugger ist arch-neutral -- dort traegt ein gruener x86-Lauf diese Seite
    // mit. `vspace_resolve` nicht: vierstufiges Paging mit `P`/`PS`/`US` gegen Deskriptorbits
    // `0b01`/`0b11` und `AP[1]`, andere Masken, andere Blockgroessen. Eine Sonde, die nur ein Zweig
    // faehrt, laesst die andere ungeprueft -- und „ungeprueft" ist nicht „vermutlich gruen".
    crate::dbgmem::messen(system::IDLE_PRIO);
    crate::dbgmem::bericht();

    // Z6b: die Debugger-Sonde -- seit sie ihr eigenes Ziel mitbringt, faehrt sie hier genauso.
    crate::dbgprobe::messen(system::IDLE_PRIO);

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
    let mut sched_ok = true;
    for c in 0..system::num_cores() {
        // Z6 Stufe 1b: ein unterdruecktes SMT-Geschwister meldet `suppressed`, nicht
        // `ticks=0` -- dieselbe Unterscheidung wie im `cores`-Konjunkt oben: Abwesenheit
        // mit Grund ist keine fehlende Messung. Ohne SMT ist jeder Kern online und die
        // Ausgabe buchstabengleich zur alten (davon haengen die Suite-Greps ab).
        if !caprock_sched::core_online(c) {
            println!("sched   : core {c} suppressed (SMT-Stufe-1: nie gebootet, kein Tick erwartet)");
            continue;
        }
        let t = hal::timer::ticks(c);
        println!("sched   : core {c} ticks={t}");
        if t == 0 {
            sched_ok = false;
        }
    }
    for i in 0..NWORKERS {
        let n = WORKER_COUNTS[i].load(Ordering::Relaxed);
        println!("sched   : worker {i} count={n}");
        if n < THRESHOLD {
            sched_ok = false;
        }
    }
    println!(
        "sched   : {}",
        if sched_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Lazy-FP: EL0-FP-Threads behielten ihr Muster über jede FP-Owner-Abgabe.
    let fp_mask = FP_OK_MASK.load(Ordering::Relaxed);
    let fp_sw = system::fp_switch_count();
    println!("fp      : EL0-FP-Threads-OK={fp_mask:#04b}/{FP_ALL_OK:#04b}, Lazy-FP-Owner-Wechsel={fp_sw}");
    let fp_ok = fp_mask == FP_ALL_OK && fp_sw > 0;
    println!(
        "fp      : {} (Lazy-FP: FP-Kontext ueber Owner-Wechsel erhalten)",
        if fp_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Prioritäten: höher priorisierter Thread (id 0, prio 4) muss zuerst fertig sein.
    for i in 0..NPRIO_TEST {
        println!(
            "prio    : id {i} (prio {}) fertig als #{}",
            4 - i,
            PRIO_FINISH[i].load(Ordering::Relaxed)
        );
    }
    let f0 = PRIO_FINISH[0].load(Ordering::Relaxed);
    let f1 = PRIO_FINISH[1].load(Ordering::Relaxed);
    let f2 = PRIO_FINISH[2].load(Ordering::Relaxed);
    let prio_ok = f0 < f1 && f1 < f2;
    println!(
        "prio    : {}",
        if prio_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Thread-Lebenszyklus: cap-kontrolliertes KILL + Selbst-EXIT + Rückgewinnung.
    let s1 = KILL_SNAP1.load(Ordering::Relaxed);
    let s2 = KILL_SNAP2.load(Ordering::Relaxed);
    let denied = DENIED_KILL.load(Ordering::Relaxed);
    let reaped = REAPED.load(Ordering::Relaxed);
    // Zurückgewonnener Stack = monotone Summe der vom Reaper freigegebenen Bytes
    // (robust gegen die Speicher-Churn von Reclaim-/Balance-Test, anders als ein
    // absoluter total_free-Vergleich).
    let freed = system::reaped_bytes();
    println!(
        "life    : victim-count nach KILL={s1}, später={s2} (eingefroren: {})",
        s1 == s2
    );
    println!(
        "life    : KILL ohne Cap -> result={denied} (verweigert: {})",
        denied != result::OK
    );
    println!(
        "life    : {reaped} Threads eingesammelt, {} KiB Stack zurueckgewonnen",
        freed / 1024
    );
    let life_ok = s1 == s2 && denied != result::OK && reaped >= 2 && freed > 0;
    println!(
        "life    : {}",
        if life_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Notifications: asynchrone Badge-Signale.
    let nb = NOTIF_GOT_BADGE.load(Ordering::Relaxed);
    let nc = NOTIF_COUNT.load(Ordering::Relaxed);
    println!("notif   : {nc} Signale empfangen, Badge={nb:#x} (erwartet {NOTIF_BADGE_VAL:#x})");
    let notif_ok = nc >= NOTIF_ROUNDS && nb == NOTIF_BADGE_VAL;
    println!(
        "notif   : {}",
        if notif_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Capability-Transfer: Client nutzt eine per IPC vom Broker delegierte Cap.
    let xr = XFER_RESULT.load(Ordering::Relaxed);
    println!("xfer    : svc call(7) ueber transferierte Cap -> {xr} (erwartet 21)");
    println!(
        "xfer    : {}",
        if xr == 21 { "ALL PASS" } else { "FAILURES" }
    );

    // Thread-Migration (ext-30).
    let mig_audit = MIG_AUDIT.load(Ordering::Acquire);
    println!(
        "migrate : Migrant lief auf core 0 (before_ok={}), migriert nach core {MIG_DST_CORE} (moved={}), laeuft dort weiter (after_ok={}, zuletzt gesehen core {}), Tcb-Cap bezeichnet ihn weiterhin (cap_ok={}), balance_once() verschob (auto_ok={}), cross-core-KILL (killed={}), sched_audit={mig_audit}",
        MIG_BEFORE_OK.load(Ordering::Acquire),
        MIG_MOVED.load(Ordering::Acquire),
        MIG_AFTER_OK.load(Ordering::Acquire),
        MIG_CORE_SEEN.load(Ordering::Acquire),
        MIG_CAP_OK.load(Ordering::Acquire),
        MIG_AUTO_OK.load(Ordering::Acquire),
        MIG_KILLED_OK.load(Ordering::Acquire),
    );
    let migrate_ok = MIG_BEFORE_OK.load(Ordering::Acquire)
        && MIG_MOVED.load(Ordering::Acquire)
        && MIG_AFTER_OK.load(Ordering::Acquire)
        && MIG_CAP_OK.load(Ordering::Acquire)
        && MIG_AUTO_OK.load(Ordering::Acquire)
        && MIG_KILLED_OK.load(Ordering::Acquire)
        && mig_audit == 0;
    println!(
        "migrate : {} (Thread wechselt den Kern, behaelt Identitaet + Tcb-Cap, laeuft weiter)",
        if migrate_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Skalierung (ext-30): viele gleichzeitige Threads + vollstaendiger Abbau.
    let sc_audit = SCALE_AUDIT.load(Ordering::Acquire);
    println!(
        "scale   : {} von {SCALE_TARGET} Threads GLEICHZEITIG erzeugt (alle auffindbar={}, Slot-Buchhaltung={}), danach abgebaut -> Slots+RAM zurueck auf Baseline={}, sched_audit={sc_audit}",
        SCALE_SPAWNED.load(Ordering::Acquire),
        SCALE_ALIVE_OK.load(Ordering::Acquire),
        SCALE_SLOTS_OK.load(Ordering::Acquire),
        SCALE_TEARDOWN_OK.load(Ordering::Acquire),
    );
    let scale_ok = SCALE_SPAWNED.load(Ordering::Acquire) as usize == SCALE_TARGET
        && SCALE_ALIVE_OK.load(Ordering::Acquire)
        && SCALE_SLOTS_OK.load(Ordering::Acquire)
        && SCALE_TEARDOWN_OK.load(Ordering::Acquire)
        && sc_audit == 0;
    println!(
        "scale   : {} (Kapazitaet kommt zur Boot-Zeit aus dem RAM, Spawn/Teardown O(1))",
        if scale_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Grant-Leak-Regression: wiederholte Grants in denselben Empfangs-Slot duerfen keine
    // unerreichbaren Caps in der geteilten Tabelle zuruecklassen (Cross-PD-DoS).
    let children = XFER_SRC_CHILDREN.load(Ordering::Relaxed);
    let xr2 = XFER_RESULT_AFTER.load(Ordering::Relaxed);
    let cdt = system::cap_audit_cdt();
    println!(
        "grantlk : nach {}+1 Grants in denselben Slot: {children} lebende Ableitung(en) der Quell-Cap (erwartet 1), Cap weiter nutzbar -> {xr2} (erwartet 21), cdt_audit={cdt}",
        XFER_GRANT_LOOPS
    );
    let grantlk_ok = children == 1 && xr2 == 21 && cdt == 0;
    println!(
        "grantlk : {} (verdraengte Grant-Cap wird freigegeben — kein Slot-Leck in der geteilten Cap-Tabelle)",
        if grantlk_ok { "ALL PASS" } else { "FAILURES" }
    );

    // EL0-Userland: echter EL0-Thread hat per Syscall einen EL1-Server gerufen.
    let urecv = USER_RECV.load(Ordering::Relaxed);
    let el0_seen = system::el0_syscall_seen();
    println!("el0     : EL1-Server empfing {urecv:#x} (erwartet {USER_MAGIC:#x}); EL0-Syscall gesehen: {el0_seen}");
    let el0_ok = urecv == USER_MAGIC && el0_seen;
    println!(
        "el0     : {} (User-Thread laeuft auf EL0, nur via Syscall)",
        if el0_ok { "ALL PASS" } else { "FAILURES" }
    );

    // EL0-Isolation: bösartiger EL0-Thread las Kernel-Speicher -> isoliert, Kernel lebt.
    let faults = system::el0_fault_count();
    println!(
        "el0iso  : EL0-Faults abgefangen={faults} (Kernel laeuft -> dieser Bericht beweist es)"
    );
    let iso_ok = faults >= 1;
    println!(
        "el0iso  : {} (EL0-Zugriff auf Kernel-Speicher faultet, Thread beendet, Kernel ueberlebt)",
        if iso_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Stateful Hot-Reload: Zustand (Zähler) bleibt über v1->v2 erhalten.
    let r1 = [
        CS_R1[0].load(Ordering::Relaxed),
        CS_R1[1].load(Ordering::Relaxed),
        CS_R1[2].load(Ordering::Relaxed),
    ];
    let r2 = [
        CS_R2[0].load(Ordering::Relaxed),
        CS_R2[1].load(Ordering::Relaxed),
    ];
    // A-4.3: die Uebernahme selbst. `gen == 1` heisst: v2 hat den Zustand ueber den versionierten
    // Kopf UEBERNOMMEN. Ohne diese Zahl belegen [13, 23] nur, dass v2 dieselben Werte erzeugt hat
    // -- nicht, dass es sie geerbt hat.
    let gen = CS_GENERATION.load(Ordering::Acquire);
    println!("ckpt    : v1(+1)={r1:?} -> Reload -> v2(+10)={r2:?} (Uebernahme Generation {gen})");
    let ckpt_ok = r1 == [1, 2, 3] && r2 == [13, 23] && gen == 1;
    println!(
        "ckpt    : {} (Zustand ueber Komponententausch erhalten -- und nachweislich UEBERNOMMEN, \
         nicht neu erzeugt)",
        if ckpt_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Batch 1 (Server v1, verdoppelt) — cap-gesicherte IPC funktioniert.
    let mut ipc_ok = true;
    for (i, &v) in B1_IN.iter().enumerate() {
        let r = R1[i].load(Ordering::Relaxed);
        println!("ipc     : v1 call({v}) -> {r} (erwartet {})", 2 * v);
        if r != 2 * v {
            ipc_ok = false;
        }
    }
    println!("ipc     : {}", if ipc_ok { "ALL PASS" } else { "FAILURES" });

    // Batch 2 (Server v2 nach Hot-Reload, verdreifacht) — gleicher Endpoint!
    let mut reload_ok = true;
    for (i, &v) in B2_IN.iter().enumerate() {
        let r = R2[i].load(Ordering::Relaxed);
        println!("reload  : v2 call({v}) -> {r} (erwartet {})", 3 * v);
        if r != 3 * v {
            reload_ok = false;
        }
    }
    println!(
        "reload  : {} (Komponente ohne Kernel-Neustart getauscht)",
        if reload_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Per-Kern-paralleler Scheduler: jeder Sekundärkern hat seinen Worker parallel
    // abgearbeitet; der Parker wurde kern-übergreifend per IPI geweckt.
    let mut smp_ok = true;
    for c in 1..system::num_cores() {
        // Z6 Stufe 1b: wie in der `sched`-Zeile -- Abwesenheit mit Grund ist keine
        // fehlende Messung. Ohne SMT ist jeder Kern online und die Ausgabe
        // buchstabengleich zur alten.
        if !caprock_sched::core_online(c) {
            println!("smp     : core {c} suppressed (SMT-Stufe-1: nie gebootet, kein Fortschritt erwartet)");
            continue;
        }
        let p = XCORE_PROGRESS[c].load(Ordering::Relaxed);
        println!("smp     : core {c} Worker-Fortschritt={p}/{SMP_WORK_TARGET}");
        if p < SMP_WORK_TARGET {
            smp_ok = false;
        }
    }
    let woken = XCORE_WOKEN.load(Ordering::Acquire);
    println!("smp     : Cross-Core-IPI-Wake (core 0 -> core 1 Parker) erfolgreich={woken}");
    if !woken {
        smp_ok = false;
    }
    println!(
        "smp     : {} (per-Kern-parallele Einplanung + IPI-Cross-Core-Wake)",
        if smp_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Kern-übergreifende synchrone IPC: Client (core 0) <-> Server (core 2).
    let mut xipc_ok = true;
    for (i, &v) in XIPC_IN.iter().enumerate() {
        let r = XIPC_RESULT[i].load(Ordering::Relaxed);
        println!(
            "xipc    : core0->core{XIPC_SERVER_CORE} call({v}) -> {r} (erwartet {})",
            XIPC_FACTOR * v
        );
        if r != XIPC_FACTOR * v {
            xipc_ok = false;
        }
    }
    println!(
        "xipc    : {} (synchrone IPC ueber Kerngrenze, CALL+REPLY je per IPI)",
        if xipc_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Reclaim: transiente EL0-Threads (mehr als der 8er-Pool) liefen dank
    // Rückgabe der Kernel-Stack-Pool-Slots beim Thread-Ende.
    let spawned = RECLAIM_SPAWNED.load(Ordering::Relaxed);
    let free = system::user_kstack_free_count();
    println!(
        "reclaim : {spawned} transiente EL0-Threads erzeugt (Pool=8), jetzt {free} Slots frei"
    );
    let reclaim_ok = spawned >= RECLAIM_TARGET && free >= 4;
    println!(
        "reclaim : {} (EL0-Kernel-Stack-Pool-Slots werden beim Thread-Ende zurueckgegeben)",
        if reclaim_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Lastausgleich: lastbewusst platzierte Worker verteilten sich über die Kerne.
    let mut place = [0u64; NUM_CORES];
    let mut distinct = 0usize;
    let mut maxp = 0u64;
    for (c, p) in place.iter_mut().enumerate() {
        *p = BALANCED_PLACE[c].load(Ordering::Relaxed);
        if *p > 0 {
            distinct += 1;
        }
        if *p > maxp {
            maxp = *p;
        }
    }
    let bspawned = BALANCED_SPAWNED.load(Ordering::Relaxed);
    println!("balance : {bspawned} Worker verteilt {place:?} (Kerne genutzt={distinct}, max/Kern={maxp})");
    // Balanciert: alle platziert, über mehrere Kerne gestreut, kein Kern überladen
    // (ohne Lastausgleich landeten alle auf dem Bootkern -> distinct=1, max=TARGET).
    let balance_ok = bspawned >= BALANCED_TARGET && distinct >= 5 && maxp <= 4;
    println!(
        "balance : {} (lastbewusste Platzierung verteilt Threads ueber die Kerne)",
        if balance_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Weg C (Hybrid): isolierte PD vs. SAS-PD beim Zugriff auf dieselbe Adresse X.
    let m = ISO_MASK.load(Ordering::Relaxed);
    let x = ISO_SECRET_ADDR.load(Ordering::Relaxed);
    let iso_faulted = system::iso_faulted();
    let ran = m & ISO_BADGE_RAN != 0;
    let read = m & ISO_BADGE_READ != 0;
    let trusted = m & ISO_BADGE_TRUSTED != 0;
    println!("vspace  : X={x:#x}; SAS-Probe las X={trusted}; isol. Probe lief+IPC={ran}, faultete={iso_faulted}, las-X={read}");
    // Bestanden: SAS-PD darf X lesen; isolierte PD lief + meldete sich per IPC,
    // wurde beim Fremdzugriff hardware-seitig gefaultet und las X NIE.
    let vspace_ok = trusted && ran && iso_faulted && !read;
    println!(
        "vspace  : {} (per-Prozess-VSpace: echte User<->User-Trennung, nur IPC)",
        if vspace_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Allgemeiner VMM: map/unmap per Syscall + VSpace-Teardown.
    let mapped = m & ISO_BADGE_MAPPED != 0;
    let faults = system::iso_fault_count();
    println!("vmm     : VMM-Probe mappte+beschrieb Frame={mapped}; isolierte Faults gesamt={faults} (>=2 => unmap wirkte)");
    let vmm_ok = mapped && faults >= 2;
    println!(
        "vmm     : {} (Frame-Caps + map/unmap-Syscalls + VSpace-Teardown)",
        if vmm_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Shared-Memory-IPC: Reader las den Wert des Writers ueber den geteilten Frame.
    let shared = m & ISO_BADGE_SHARED != 0;
    println!("shm     : Reader las Writer-Wert via geteiltem Frame (zwei isol. VSpaces)={shared}");
    println!(
        "shm     : {} (Shared-Memory-IPC: ein Frame in zwei isolierte VSpaces, cap-gewaehrt)",
        if shared { "ALL PASS" } else { "FAILURES" }
    );

    // Natives Code-Laden: privat geladener Code lief in eigener EL0-RX-Region.
    let native = m & ISO_BADGE_NATIVE != 0;
    println!("native  : privat geladener Code lief in eigener VSpace (nicht geteilte .user_text)={native}");
    println!(
        "native  : {} (natives Code-Laden je isolierter VSpace, EL0-RX/W^X + I-Cache-Sync)",
        if native { "ALL PASS" } else { "FAILURES" }
    );

    // 4-KiB-Seiten: mehrere einzelne Seiten mit gemischten Rechten + Guard-Page.
    let pages4k = m & ISO_BADGE_PAGES != 0;
    println!("pages4k : einzelne 4-KiB-Seiten RW-Schreiben+RO-Lesen ok={pages4k} (Guard-Seite faultet danach)");
    println!(
        "pages4k : {} (4-KiB-Mappings: gemischte RW/RO-Rechte + Guard Pages + L3)",
        if pages4k { "ALL PASS" } else { "FAILURES" }
    );

    // Strand-7: ARM-Kernel-Stack-Wachen — dedizierte Kratz-Wache + EL0-Beruehrung.
    // Das Urteil ist rein aus eingerasteten Werten (erwartete Wache, eingerastete
    // FAR): `all_done()` und diese Zeile lesen dieselbe Wirklichkeit.
    let (g_erw, g_far, g_ec, guard_ok) = system::guard_sonde_urteil();
    let (gd_live, gd_total, gd_denied, gd_blk, gd_blkcap) = hal::mmu::guard_stats();
    println!("guard   : Sonde Wache={g_erw:#x} FAR={g_far:#x} EC={g_ec:#04x} (Kontrolle+4096 gelesen, Wache muss faulten); Guard-Pages {gd_live} stehen / {gd_total} gesetzt / {gd_denied} abgewiesen, {gd_blk}/{gd_blkcap} aufgeteilte 2-MiB-Bloecke");
    println!(
        "guard   : {} (Guard-Pages auf ARM: Stack-Wachen stehen + EL0-Zugriff faultet mit FAR=Wache)",
        if guard_ok { "ALL PASS" } else { "FAILURES" }
    );

    // Churn/Leak: tausende spawn/destroy-Zyklen, Ressourcen kehren zur Baseline.
    let churn = CHURN_DONE.load(Ordering::Acquire) && CHURN_OK.load(Ordering::Acquire);
    println!("churn   : {CHURN_TARGET} spawn/destroy-Zyklen isolierter PDs; MEM/TCB/ASID/kstack zurueck=Baseline: {churn}");
    println!(
        "churn   : {} (keine Mapping-/ASID-/TCB-/Speicher-Leaks ueber tausende Zyklen)",
        if churn { "ALL PASS" } else { "FAILURES" }
    );

    // D15-Melder: die GELEGENHEIT, nicht der Treffer. Gezaehlt wird der ZUSTAND beim spaeten
    // Aufraeumen eines Kernel-Stacks, und der ist in JEDEM Lauf pruefbar -- bei 0,225 % waere ein
    // Melder, der nur beim Unglueck spricht, in 443 von 444 Laeufen stumm.
    let (spaet, fremd, fuesse, rec_ges, rec_stack) = system::kstack_spaet_stats();
    let (zomb_ges, zomb_fuss) = caprock_sched::zombie_fuss_stats();
    #[cfg(feature = "debug-log")]
    println!(
        "kstackid: spaet-am-wiedervergebenen-Slot={spaet} fremder-Kstack-freigegeben={fremd} \
         (von {rec_ges} Aufraeumungen, {rec_stack} mit Stack)"
    );
    #[cfg(feature = "debug-log")]
    println!(
        "kstackid: EL0-Kstack unter den eigenen Fuessen freigegeben={fuesse}; \
         Zombie-Region unter den eigenen Fuessen eingereiht={zomb_fuss} von {zomb_ges}"
    );
    #[cfg(not(feature = "debug-log"))]
    let _ = (spaet, fremd, fuesse, rec_ges, rec_stack, zomb_ges, zomb_fuss);

    // MCS Scheduling Contexts: budgetierter vs. unbeschränkter Thread auf MCS_CORE.
    let (depl, refl) = system::budget_stats(MCS_CORE);
    let bg = MCS_BUDGETED_COUNT.load(Ordering::Relaxed);
    let gr = MCS_GREEDY_COUNT.load(Ordering::Relaxed);
    let bound = MCS_BOUND.load(Ordering::Acquire);
    println!("mcs     : Budget per Cap gebunden={bound} (budget={MCS_BUDGET}/Periode={MCS_PERIOD}); Erschoepfungen={depl}, Refills={refl}");
    println!("mcs     : Fortschritt budgetiert={bg} vs. greedy={gr} (budgetiert gedrosselt + garantiert > 0)");
    let mcs = MCS_DONE.load(Ordering::Acquire) && MCS_OK.load(Ordering::Acquire);
    println!(
        "mcs     : {} (MCS-Budget: Verbrauch/Tick, Erschoepfung->Block, Refill nach Periode, cap-vergeben)",
        if mcs { "ALL PASS" } else { "FAILURES" }
    );

    // Audit-Regression A: IPC-Panik bei totem Thread in Endpoint-Queue (behoben).
    let served = STALE_SERVED.load(Ordering::Acquire);
    let stale = STALE_DONE.load(Ordering::Acquire) && STALE_OK.load(Ordering::Acquire);
    println!("stale   : Opfer in senders gekillt; Server uebersprang toten Eintrag (keine Panik); Client-Antwort={served:#x} (erwartet {:#x})", STALE_MAGIC.wrapping_mul(2));
    println!(
        "stale   : {} (IPC paniert nicht bei gekilltem, in Endpoint-Queue blockiertem Thread)",
        if stale { "ALL PASS" } else { "FAILURES" }
    );

    // Audit-Regression B: erschoepfter Thread nach erneutem Budget-Bind nicht gestrandet.
    let snap1 = STRAND_SNAP1.load(Ordering::Relaxed);
    let now = STRAND_COUNT.load(Ordering::Relaxed);
    let strand = STRAND_DONE.load(Ordering::Acquire) && STRAND_OK.load(Ordering::Acquire);
    println!("strand  : erschoepfter Worker nach Re-Bind: Zaehler {snap1} -> {now} (muss wachsen)");
    println!(
        "strand  : {} (set_budget/bind_sched_context strandet einen erschoepften Thread nicht)",
        if strand { "ALL PASS" } else { "FAILURES" }
    );

    // Audit-Regression C: Reply-Liveness (Reply-Owner weg -> Client ERR_SERVER_GONE).
    let rgr = RGONE_RESULT.load(Ordering::Acquire);
    let rgq = RGONE_Q_RESULT.load(Ordering::Acquire);
    let rgalive = RGONE_Q_SRV_ALIVE.load(Ordering::Acquire);
    let rgone = RGONE_DONE.load(Ordering::Acquire) && RGONE_OK.load(Ordering::Acquire);
    println!("rgone   : kill-Runde Client={rgr}, quiesce-Runde Client={rgq} (Server lebt={rgalive}); erwartet ERR_SERVER_GONE={}", result::ERR_SERVER_GONE);
    println!(
        "rgone   : {} (toter ODER zurueckgezogener Reply-Owner entblockt den CALL-Aufrufer statt ihn haengen zu lassen)",
        if rgone { "ALL PASS" } else { "FAILURES" }
    );

    // Budget-Donation (MCS): Server-Arbeit gegen das Client-Budget belastet.
    let ddelta = DDON_DELTA.load(Ordering::Acquire);
    let ddon = DDON_DONE.load(Ordering::Acquire) && DDON_OK.load(Ordering::Acquire);
    println!("ddon    : budgetierter Client x{DDON_CALLS} Calls -> unbeschr. Server; Client-Konto-Erschoepfungen={ddelta} (erwartet >={DDON_MIN_DEPL})");
    println!(
        "ddon    : {} (Budget-Donation: intra-core CALL belastet die Server-Arbeit gegen das Aufrufer-Budget)",
        if ddon { "ALL PASS" } else { "FAILURES" }
    );

    // First-class Reply-Cap (ObjectKind::Reply) + Revocation.
    let rcr = RCAP_RESULT.load(Ordering::Acquire);
    let rcap = RCAP_DONE.load(Ordering::Acquire) && RCAP_OK.load(Ordering::Acquire);
    println!("rcap    : Reply-Cap fuer ausstehenden Call gepraegt + geloescht; Client-CALL-Ergebnis={rcr} (erwartet ERR_SERVER_GONE={})", result::ERR_SERVER_GONE);
    println!(
        "rcap    : {} (first-class ObjectKind::Reply: Revocation/Loeschen der Reply-Cap bricht den Call ab)",
        if rcap { "ALL PASS" } else { "FAILURES" }
    );

    // Reply-Cap-Server-Migration: ausstehender Call ueberlebt einen Hot-Reload des Servers.
    let rmr = RMIG_RESULT.load(Ordering::Acquire);
    let rmv = RMIG_VALUE.load(Ordering::Acquire);
    let rmmig = RMIG_MIGRATED.load(Ordering::Acquire);
    let rmig = RMIG_DONE.load(Ordering::Acquire) && RMIG_OK.load(Ordering::Acquire);
    println!("rmig    : v1 empfaengt Call, Hot-Reload migriert die Antwortpflicht auf v2; Client-Ergebnis={rmr} (erwartet OK={}) Wert={rmv} (erwartet {}={RMIG_V2_FACTOR}*{RMIG_INPUT}) migriert={rmmig}", result::OK, RMIG_V2_FACTOR * RMIG_INPUT);
    println!(
        "rmig    : {} (Reply-Cap-Server-Migration: ausstehende Reply-Cap ueberlebt den Server-Wechsel, v2 schliesst den Call ab)",
        if rmig { "ALL PASS" } else { "FAILURES" }
    );

    // In-Kernel-Fuzzer (ADR 0013): die vier Fuzzer-Reportzeilen (fuzz/ipcfuzz/loaderfuzz/hwfuzz).
    // Im Release-Build ohne Feature `kernel-fuzz` ist `fuzz::report()` ein No-Op (keine Zeilen).
    fuzz::report();

    // CAPS-Read-Concurrency (#3): Reader-Writer-Lock laesst parallele Cap-Lookups zu.
    let clmax = system::caps_max_concurrent_readers();
    let caplk = CAPLK_DONE.load(Ordering::Acquire) && CAPLK_OK.load(Ordering::Acquire);
    println!("caplk   : 2 Sonden auf Kern {CAPLK_CORE_A}+{CAPLK_CORE_B}; gleichzeitige CAPS-Leser-Hoechststand={clmax} (erwartet >={CAPLK_WANT}; exklusiver Lock waere strukturell 1)");
    println!(
        "caplk   : {} (CAPS-Reader-Writer-Lock: heisse Cap-Lookups laufen nebenlaeufig statt serialisiert)",
        if caplk { "ALL PASS" } else { "FAILURES" }
    );

    // Sicherheitsdomänen (ext-22, P1): Domänen-Policy-Oracle + Domänen-Round-Trip.
    let dcode = DOMAIN_AUDIT_CODE.load(Ordering::Acquire);
    let domain = DOMAIN_DONE.load(Ordering::Acquire) && DOMAIN_OK.load(Ordering::Acquire);
    println!("domain  : TrustedSas(global)+HardwareLand(iso)+UserLand(iso); domain_audit={dcode} (erwartet 0); Domaenen round-trippen");
    println!(
        "domain  : {} (Domaenen-Policy: Cap-Typen je Domaene + untrusted Domaenen sind isoliert)",
        if domain { "ALL PASS" } else { "FAILURES" }
    );

    // UserLand-Management (ext-22, P2): cap-gated SYS_PDCTL.
    let pdctl = PDCTL_DONE.load(Ordering::Acquire) && PDCTL_OK.load(Ordering::Acquire);
    println!("pdctl   : Controller steuert UserLand-Ziel: lief={} PAUSE-eingefroren={} RESUME-waechst={} leerer-Slot-ERR_BADCAP={} PdControl-in-UserLand-denied={} STOP={}",
        PDCTL_RAN.load(Ordering::Acquire), PDCTL_FROZE.load(Ordering::Acquire),
        PDCTL_RESUMED.load(Ordering::Acquire), PDCTL_NOCAP.load(Ordering::Acquire),
        PDCTL_POLICY.load(Ordering::Acquire), PDCTL_STOPPED.load(Ordering::Acquire));
    println!(
        "pdctl   : {} (Management-Cap PdControl: nur TrustedSas steuert nur UserLand, jede Op cap-gated)",
        if pdctl { "ALL PASS" } else { "FAILURES" }
    );

    // Paarweiser Treiber<->Backend-Kanal (ext-22, P3).
    let chr = CHAN_RESULT.load(Ordering::Acquire);
    let chan = CHAN_DONE.load(Ordering::Acquire) && CHAN_OK.load(Ordering::Acquire);
    println!("chan    : Trusted CALLt Backend ueber gebundenen Kanal -> {chr} (erwartet {}={CHAN_FACTOR}*{CHAN_INPUT}); 1:N={} Fremd-Cap-denied={} recv-cap-ok={}",
        CHAN_FACTOR * CHAN_INPUT, CHAN_1N_OK.load(Ordering::Acquire),
        CHAN_FOREIGN_DENIED.load(Ordering::Acquire), CHAN_BE_RECV_OK.load(Ordering::Acquire));
    println!(
        "chan    : {} (unveraenderliche paarweise HardwareLand<->Trusted-Bindung, 1:N, Backend spricht nur mit Partner)",
        if chan { "ALL PASS" } else { "FAILURES" }
    );

    // RTC-Hardware-Backend (ext-22, P4): generische MMIO-Infrastruktur, erstes echtes Geraet.
    let rtcv = RTC_VALUE.load(Ordering::Acquire);
    let rtc = RTC_DONE.load(Ordering::Acquire) && RTC_OK.load(Ordering::Acquire);
    println!("rtc     : HardwareLand-Backend liest PL031 RTC_DR={rtcv} (>0 erwartet) ueber MMIO-Cap+Device-Mapping; MMIO-in-HW={} MMIO-in-UserLand-denied={}",
        RTC_MMIO_OK.load(Ordering::Acquire), RTC_POLICY.load(Ordering::Acquire));
    println!(
        "rtc     : {} (generisches vspace_map_device + MMIO-Cap: Device-Zugriff nur in HardwareLand, kernel-autorisiert, W^X)",
        if rtc { "ALL PASS" } else { "FAILURES" }
    );

    // RTC-IRQ (ext-22, P5): IRQ-Cap + GIC-SPI-Routing + Deferred-IRQ-Zustellung.
    let irq = IRQT_DONE.load(Ordering::Acquire) && IRQT_OK.load(Ordering::Acquire);
    println!("irq     : HardwareLand-Backend armiert PL031-Match-IRQ (INTID {RTC_INTID}); Kernel routet+deferred-zustellt -> Backend-Antwort={} (1 erwartet), zugestellte IRQs={}; IRQ-Cap-in-HW={} in-UserLand-denied={}",
        IRQT_GOT_VALUE.load(Ordering::Acquire), system::irqs_delivered(),
        IRQT_CAP_OK.load(Ordering::Acquire), IRQT_POLICY.load(Ordering::Acquire));
    println!(
        "irq     : {} (IRQ-Cap + GICD_ITARGETSR-Routing + Deferred-Signal: Geraete-IRQ als Notification an HardwareLand, lock-/deadlock-frei)",
        if irq { "ALL PASS" } else { "FAILURES" }
    );

    // DMA-Capability (ext-23, D0): DmaCap + Normal-NC-Mapping + EL0-Round-Trip + Kohärenz.
    let dmav = DMA_VALUE.load(Ordering::Acquire) as u32;
    let dma = DMA_DONE.load(Ordering::Acquire) && DMA_OK.load(Ordering::Acquire);
    println!("dma     : HardwareLand-Backend schreibt+liest DMA-Region (EL0 Normal-NC) ueber DmaCap; EL0-read=0x{dmav:08x} (0x{DMA_PAT0:08x} erwartet); Kohaerenz(Kernel-Identity)+DmaCap-in-HW={} in-UserLand-denied={} bounds-audit-greift={}",
        DMA_CAP_OK.load(Ordering::Acquire), DMA_POLICY.load(Ordering::Acquire), DMA_SENS_OK.load(Ordering::Acquire));
    println!(
        "dma     : {} (DmaCap hinter DmaEnforcer-Abstraktion: kernel-ausgeschnittene Region, nur HardwareLand, Normal-NC-Mapping, dma_audit)",
        if dma { "ALL PASS" } else { "FAILURES" }
    );

    // PCIe-Enumeration (ext-23, D1): virtio-rng-pci finden, BAR + Bus-Master, RID = StreamID.
    let pcie = PCIE_DONE.load(Ordering::Acquire) && PCIE_OK.load(Ordering::Acquire);
    println!("pcie    : virtio-rng-pci gefunden: vendor=0x{:04x} device=0x{:04x} RID/StreamID=0x{:04x} BAR=0x{:08x} bus-master={} bogus-vendor-not-found={}",
        PCIE_VENDOR.load(Ordering::Acquire), PCIE_DEVICE.load(Ordering::Acquire),
        PCIE_RID.load(Ordering::Acquire), PCIE_BAR.load(Ordering::Acquire),
        PCIE_BM.load(Ordering::Acquire), PCIE_NEG.load(Ordering::Acquire));
    println!(
        "pcie    : {} (ECAM-Enumeration + BAR-Zuweisung + Bus-Master; RID == SMMU-StreamID, kernel-/Trusted-Setup)",
        if pcie { "ALL PASS" } else { "FAILURES" }
    );

    // SMMUv3-Bring-up (ext-23, D2): DmaEnforcer initialisiert (Default-Abort) + CMD_SYNC-Spike.
    let smmu = SMMU_DONE.load(Ordering::Acquire) && SMMU_OK.load(Ordering::Acquire);
    println!("smmu    : SMMUv3 IDR0=0x{:08x} SIDSIZE={} -> Bring-up: SMMUEN={} CMD_SYNC-Round-Trip={} Event-Queue-leer={} GERROR=0x{:x}",
        SMMU_IDR0.load(Ordering::Acquire), SMMU_SID.load(Ordering::Acquire),
        SMMU_EN.load(Ordering::Acquire), SMMU_SYNC.load(Ordering::Acquire),
        SMMU_EVTQ.load(Ordering::Acquire), SMMU_GERR.load(Ordering::Acquire));
    println!(
        "smmu    : {} (SMMUv3-Bring-up hinter DmaEnforcer: Command-/Event-Queue + lineare Stream-Tabelle, Default-Abort, CR0)",
        if smmu { "ALL PASS" } else { "FAILURES" }
    );

    // --- Die ARCH-NEUTRALE Gesundheitsaussage (2026-08-17) -----------------------------------
    //
    // Dieselbe Zeile druckt der x86-Hochlauf. Bis hierher berichtete jede Architektur in ihren
    // eigenen Worten (`smmu` gegen `iommu`/`vtdcaps`), und zwei Formulierungen derselben
    // Eigenschaft sind zwei Entwuerfe -- `todo.md` hatte genau das vorab als Warnsignal benannt.
    //
    // Der Round-Trip wird HEREINGEREICHT: `CMD_SYNC` braucht die Physadresse der Command-Queue
    // und den Producer-Index, und die gehoeren dem `DmaEnforcer`, nicht der HAL. Es gibt genau
    // einen Round-Trip je Boot, und berichtet wird DIESER -- nicht ein zweiter, nachgerechneter.
    println!(
        "iohealth: present={} translation={} units={}/{} round_trip={} faults_empty={} \
         config_errors={} hw_error={:#x}",
        IOHEALTH_PRESENT.load(Ordering::Acquire), IOHEALTH_XLAT.load(Ordering::Acquire),
        IOHEALTH_SPEAK_N.load(Ordering::Acquire), IOHEALTH_UNITS.load(Ordering::Acquire),
        SMMU_SYNC.load(Ordering::Acquire), IOHEALTH_FAULTS_EMPTY.load(Ordering::Acquire),
        IOHEALTH_CFGERR.load(Ordering::Acquire), IOHEALTH_HWERR.load(Ordering::Acquire)
    );
    println!(
        "iohealth: speaking={} -- `faults_empty` zaehlt NUR mit round_trip: eine tote Einheit \
         meldet ebenfalls eine leere Warteschlange (dieselbe Form wie die leere Event-Queue ohne \
         `CD.R`)",
        IOHEALTH_SPEAKING.load(Ordering::Acquire)
    );
    println!(
        "iohealth: {}",
        if IOHEALTH_OK.load(Ordering::Acquire) { "ALL PASS" } else { "FAILURES" }
    );

    // SMMU-Bindung (ext-23, D3): STE/CD/Stage-1 installieren+entziehen, balanciert.
    let smmubind = SMMUB_DONE.load(Ordering::Acquire) && SMMUB_OK.load(Ordering::Acquire);
    println!("smmubind: enable_dma(StreamID=0x{:04x}) STE->CD->Stage-1 installiert={} Event-Queue-leer={} Stage-1/CD-Frames balanciert nach disable={}",
        SMMUB_RID.load(Ordering::Acquire), SMMUB_ENABLE.load(Ordering::Acquire),
        SMMUB_EVTQ.load(Ordering::Acquire), SMMUB_BALANCED.load(Ordering::Acquire));
    println!(
        "smmubind: {} (enable_dma/disable_dma: STE->CD->Stage-1 bildet NUR die DMA-Region ab; Revoke gibt Tabellen frei)",
        if smmubind { "ALL PASS" } else { "FAILURES" }
    );

    // virtio-rng-DMA (ext-23, D4): echter Bus-Master-DMA + zweistufiger Kronjuwel.
    let virtiorng = VRNG_DONE.load(Ordering::Acquire) && VRNG_OK.load(Ordering::Acquire);
    println!("virtiorng: Geraet DMAt {} Zufallsbytes in die DmaCap-Region: used-adv={} bytes[0..8]=0x{:08x}{:08x} SMMU-Event-Queue-leer={}",
        VRNG_WRITTEN.load(Ordering::Acquire), VRNG_USED.load(Ordering::Acquire),
        VRNG_R1.load(Ordering::Acquire), VRNG_R0.load(Ordering::Acquire),
        VRNG_EVTQ.load(Ordering::Acquire));
    println!("virtiorng: KRONJUWEL out-of-window: L1-Software-abgewiesen={} Ziel-unveraendert={} (Sensitivitaet: ohne-Pruefung-Geraet-schrieb={}); L2-SMMU-Fault={} (Stage-1-STE->CD->Pagetable greift, Fault wird als Event aufgezeichnet)",
        VRNG_CJ_SW.load(Ordering::Acquire), VRNG_CJ_SENT.load(Ordering::Acquire),
        VRNG_CJ_UNGUARDED.load(Ordering::Acquire), VRNG_SMMU_ENF.load(Ordering::Acquire));
    println!("virtiorng: Kontrollen: Bypass-STE-Geraet-schrieb={} Event-Queue-hat-gesprochen={} keine-Konfig-Fehler={}",
        VRNG_BYPASS.load(Ordering::Acquire), VRNG_LIVE.load(Ordering::Acquire),
        VRNG_CFGERR0.load(Ordering::Acquire));
    println!("virtiorng: ext-36b Achsen getrennt: IOVA!=PA={} ; Negativtest (Deskriptor traegt absichtlich die PA): Event-F_TRANSLATION={} StreamID-passt={} Input-Adresse-passt={} (mit iommu_platform=on uebersetzt QEMU wirklich -> beobachtbar)",
        VRNG_AXES.load(Ordering::Acquire), VRNG_EVT_XLAT.load(Ordering::Acquire),
        VRNG_EVT_SID.load(Ordering::Acquire), VRNG_EVT_IN.load(Ordering::Acquire));
    println!(
        "virtiorng: {} (echter Bus-Master-DMA in die DmaCap-Region; zweistufig: Level-1-Software-Bounds erzwingt In-Window demonstrierbar, Level-2-SMMU als HW-Backstop fuer reale HW)",
        if virtiorng { "ALL PASS" } else { "FAILURES" }
    );

    // Generische DMA-Infrastruktur (ext-24).
    let dmagen = DMAGEN_DONE.load(Ordering::Acquire) && DMAGEN_OK.load(Ordering::Acquire);
    println!("dmagen  : Multi-Region(3-in-1-Kontext)={} Richtung(Read=RO/Write=RW)={} Kohaerenz(WB/NC)={} Stream-Gruppe(2 SIDs)={} Scatter-Gather={} Sub-Puffer={} balanciert={}",
        DMAGEN_MULTIREGION.load(Ordering::Acquire), DMAGEN_DIR.load(Ordering::Acquire),
        DMAGEN_COH.load(Ordering::Acquire), DMAGEN_GROUP.load(Ordering::Acquire),
        DMAGEN_SG.load(Ordering::Acquire), DMAGEN_POOL.load(Ordering::Acquire),
        DMAGEN_BALANCED.load(Ordering::Acquire));
    println!(
        "dmagen  : {} (generische DMA-Infra: Richtung/Kohaerenz als DmaCap-Attribute, Multi-Region-Kontext, Stream-Gruppen, SG-Validierung, disjunkte Sub-Puffer ueber den SG-Pfad — baut auf DmaCap/DmaEnforcer auf)",
        if dmagen { "ALL PASS" } else { "FAILURES" }
    );

    // IOVA-Fenstergrenzen (ext-36b Nacharbeit).
    let dmawin_fertig = DMAWIN_DONE.load(Ordering::Acquire);
    println!("dmawin  : 32-Bit-Geraet-abgewiesen={} Fenster-voll-abgewiesen={} Kontext-danach-intakt={} balanciert={} Geraete-ohne-deklarierte-Adressbreite={} (gefuehrt, nicht stillschweigend 64)",
        DMAWIN_NARROW.load(Ordering::Acquire), DMAWIN_EXHAUST.load(Ordering::Acquire),
        DMAWIN_INTACT.load(Ordering::Acquire), DMAWIN_BALANCED.load(Ordering::Acquire),
        DMAWIN_UNDECL.load(Ordering::Acquire));
    // B-3.4 (E-Rest 1): auf aarch64 stand diese Aussage bis hierher in KEINER Zeile -- sie steckte
    // still in `r.ok`. Eine Zusicherung, die nirgends gedruckt wird, ist von einer fehlenden nicht
    // zu unterscheiden.
    println!(
        "dmawin  : B-3.4 Fenster gegen den Interrupt-Nachrichtenbereich: {} -- belegte-DMA-Kontexte={} davon-schwaches-Fenster={} Fenster-weicht-von-der-Politik-ab={} eingetragenes-Fenster-im-Sperrbereich={}",
        crate::dmatests::msi_klartext(crate::dmatests::msi_from_code(
            DMAWIN_MSI_POLICY.load(Ordering::Acquire)
        )),
        DMAWIN_MSI_LIVE.load(Ordering::Acquire),
        DMAWIN_MSI_WEAK.load(Ordering::Acquire),
        DMAWIN_MSI_DRIFT.load(Ordering::Acquire),
        DMAWIN_MSI_OVL.load(Ordering::Acquire)
    );
    let dmawin_urteil = if dmawin_fertig {
        crate::dmatests::Verdict::from_code(DMAWIN_VERDICT.load(Ordering::Acquire))
    } else {
        // Nicht gelaufen ist nicht bestanden -- und auch kein SKIP: ein Test, der nicht
        // durchkam, hat kein Ergebnis, sondern einen Ausfall.
        crate::dmatests::Verdict::Fail
    };
    println!(
        "dmawin  : {} (IOVA-Fenstergrenzen: Geraete-Adressbreite + Fenster-Erschoepfung scheitern laut statt still abzuschneiden; kein halb aufgebauter Kontext)",
        dmawin_urteil.wort()
    );

    // Teardown-Token (ext-37).
    let dmatok = DMATOK_DONE.load(Ordering::Acquire) && DMATOK_OK.load(Ordering::Acquire);
    println!("dmatok  : delete-ohne-detach-baut-ab={} Region-danach-frei={} unbestaetigte-Stilllegung-bleibt-pending={} Audit-Code-7-haelt={}",
        DMATOK_DELETE_TEARS.load(Ordering::Acquire), DMATOK_FREED.load(Ordering::Acquire),
        DMATOK_PENDING.load(Ordering::Acquire), DMATOK_AUDIT7.load(Ordering::Acquire));
    println!(
        "dmatok  : {} (Teardown-Token: die Cap-Crate gibt DMA-Regionen nicht mehr selbst frei, sondern meldet sie; Freigabe nur gegen Nachweis -- aus einer bewiesenen wird eine erzwungene Reihenfolge)",
        if dmatok { "ALL PASS" } else { "FAILURES" }
    );

    // Prozess-Heap (ext-25): echter Box/Vec/BTreeMap-Heap auf realen Physadressen (safe Rust).
    let sasheap = SASHEAP_DONE.load(Ordering::Acquire) && SASHEAP_OK.load(Ordering::Acquire);
    println!("sasheap : Trusted-SAS-Heap (Slabs+Bump ueber Regionsliste): Vec(4096,Realloc/grow)={} Box={} BTreeMap(256)={} Large-Alloc(dedizierte Region)={} Region-angefordert={} balanciert(alle Regionen zurueck)={}",
        SASHEAP_VEC.load(Ordering::Acquire), SASHEAP_BOX.load(Ordering::Acquire),
        SASHEAP_MAP.load(Ordering::Acquire), SASHEAP_LARGE.load(Ordering::Acquire),
        SASHEAP_GREW.load(Ordering::Acquire), SASHEAP_BALANCED.load(Ordering::Acquire));
    println!(
        "sasheap : {} (echter Box/Vec/BTreeMap-Heap auf realen Physadressen; Testcode 100% safe, unsafe nur in der Region-Runtime; prozess-lokaler Allokator ueber RegionSource grow/shrink)",
        if sasheap { "ALL PASS" } else { "FAILURES" }
    );

    // Binary-Loader (ext-26, L1): extern gebautes Programm geladen + ausgefuehrt.
    let load = LOAD_DONE.load(Ordering::Acquire) && LOAD_OK.load(Ordering::Acquire);
    println!("load    : extern gebautes 'hello' aus Boot-Archiv geladen (eigene isolierte UserLand-VSpace, W^X-Segmente an Link-VA, vom Allokator beliebige Phys) -> signalisierte HELLO_BADGE={}",
        if load { "ja" } else { "NEIN" });
    println!(
        "load    : {} (generischer Binary-Loader: ELF64-Parse in Safe Rust, Segment-Kopie W^X, cap-gegatete PD + Endowment, EL0-Spawn -- extern gebauter Code laeuft NICHT aus dem Kernel-Image)",
        if load { "ALL PASS" } else { "FAILURES" }
    );

    // Binary-Loader L2 (ext-26): SYS_LOAD zur Laufzeit, cap-gegatet.
    let sysload = SYSLOAD_FIN.load(Ordering::Acquire) && SYSLOAD_OK.load(Ordering::Acquire);
    println!("sysload : SYS_LOAD result={} (OK={}) Negativ(ohne Loader-Cap -> ERR_BADCAP)={} hello-Signal={}",
        SYSLOAD_RESULT.load(Ordering::Relaxed), result::OK, SYSLOAD_NEG_OK.load(Ordering::Relaxed),
        if sysload { "ja" } else { "NEIN" });
    println!(
        "sysload : {} (Laden zur LAUFZEIT via SYS_LOAD: cap-gegatet ueber Loader-Cap, Caller delegiert eigene Notification-Cap in die neue PD; ohne Loader-Cap abgewiesen)",
        if sysload { "ALL PASS" } else { "FAILURES" }
    );

    // Binary-Loader L3 (ext-26): HardwareLand laden + EL0-TrustedSAS-Laden.
    let loadhw = LOADHW_FIN.load(Ordering::Acquire) && LOADHW_OK.load(Ordering::Acquire);
    println!("loadhw  : HardwareLand-Programm in vor-erstellte Backend-PD geladen (Partner+Kanal) -> Kanal-Signal={} ; TrustedSAS als EL0-isolierte PD geladen(domain_audit ok)={}",
        if LOADHW_NTFN.load(Ordering::Relaxed) != usize::MAX
            && system::notification_pending(LOADHW_NTFN.load(Ordering::Relaxed)) == HELLO_BADGE { "ja" } else { "NEIN" },
        LOADTRUSTED_EL0.load(Ordering::Relaxed));
    println!(
        "loadhw  : {} (HardwareLand-Laden: Backend-PD mit Partner+Kanal, Kanal-Cap-Policy gilt; TrustedSAS (zertifiziertes svc-demo) laeuft GELADEN EL0-isoliert -- nur mit gueltigem Zertifikat (ext-28) + trust_audit==0 (Key-DB selbstkonsistent, Gate setzt aktiv durch))",
        if loadhw { "ALL PASS" } else { "FAILURES" }
    );

    // Binary-Loader L4 (ext-26): Teardown geladener Prozesse.
    let loadstop = LOADSTOP_DONE.load(Ordering::Acquire) && LOADSTOP_OK.load(Ordering::Acquire);
    println!(
        "loadstop: {} (geladenen Prozess vollstaendig abgebaut: Thread+VSpace-Tabellen+geladene Segment-Frames+Kstack+PD -> MEM/VSpace/kstack-Baseline wiederhergestellt, kein Leck)",
        if loadstop { "ALL PASS" } else { "FAILURES" }
    );

    // (Loader-Fuzzer-Reportzeile wird von `fuzz::report()` ausgegeben, ADR 0013.)

    // ext-27 T0: UserLand-Aggressor (extern geladener Dienst, ADR 0012).
    let aggru = AGGRU_DONE.load(Ordering::Acquire) && AGGRU_OK.load(Ordering::Acquire);
    println!(
        "aggru   : {} (extern geladener UserLand-Aggressor: Cap-Confusion (leerer Slot/falscher Typ/falsche Rechte) + Eskalation (PDCTL/LOAD/KILL ohne Autoritaet) -> alle als BADCAP/RIGHTS/BADSYS abgewiesen; Dienst signalisiert SUCCESS nur bei voller Abweisung; Audits==0)",
        if aggru { "ALL PASS" } else { "FAILURES" }
    );

    // ext-27 T1: UserLand-Intruder (Speicher-Isolation, ADR 0012).
    let intru = INTRU_DONE.load(Ordering::Acquire) && INTRU_OK.load(Ordering::Acquire);
    println!(
        "intru   : {} (extern geladener UserLand-Intruder: las Kernel-RAM aus EL0 -> Translation-Fault (Ziel nicht in der isolierten VSpace gemappt) -> Kernel terminiert den Angreifer-Thread + laeuft weiter; PRE-Badge erhalten + el0_fault_count++ + Audits==0)",
        if intru { "ALL PASS" } else { "FAILURES" }
    );

    // ext-27 T2: HardwareLand-Dienste (ADR 0012).
    let aggrh = AGGRH_DONE.load(Ordering::Acquire) && AGGRH_OK.load(Ordering::Acquire);
    println!(
        "aggrh   : {} (extern geladenes HardwareLand-Backend als Aggressor: trotz 'Hardware'-Domaene KEINE Management-Autoritaet (PDCTL/LOAD/KILL -> BADCAP) + nichts ausserhalb des eigenen Kanals erreichbar; Cap-Confusion alle BADCAP/RIGHTS/BADSYS; meldet SUCCESS ueber den Kanal nur bei voller Abweisung; Audits==0)",
        if aggrh { "ALL PASS" } else { "FAILURES" }
    );
    let intrh = INTRH_DONE.load(Ordering::Acquire) && INTRH_OK.load(Ordering::Acquire);
    println!(
        "intrh   : {} (extern geladenes HardwareLand-Backend als Intruder: las Kernel-RAM aus EL0 -> Fault -> terminiert, Kernel laeuft weiter; Speicher-Isolation DOMAENEN-UNABHAENGIG (auch ein Hardware-Backend ist EL0-isoliert); PRE + el0_fault_count++ + Audits==0)",
        if intrh { "ALL PASS" } else { "FAILURES" }
    );

    // ext-27 T3: TrustedSAS-Dienste (ADR 0012).
    let aggrt = AGGRT_DONE.load(Ordering::Acquire) && AGGRT_OK.load(Ordering::Acquire);
    println!(
        "aggrt   : {} (extern geladener TrustedSAS-Aggressor (EL0-isoliert): TRUST != PRIVILEG -- hoechste Cap-Autoritaet der Domaene, doch ohne tatsaechliche PdControl/Loader-Cap scheitern PDCTL/LOAD/KILL als BADCAP; Cap-Confusion alle abgewiesen; SUCCESS nur bei voller Abweisung; Audits==0)",
        if aggrt { "ALL PASS" } else { "FAILURES" }
    );
    let intrt = INTRT_DONE.load(Ordering::Acquire) && INTRT_OK.load(Ordering::Acquire);
    println!(
        "intrt   : {} (ext-28 Zertifikats-Gate: UNZERTIFIZIERTES TrustedSAS wird abgewiesen -- intruder-t traegt absichtlich `unsafe` (nicht zertifizierbar), liegt OHNE Zertifikat im Archiv -> verify_image (ADR 0014) weist das Laden mit Unverified ab; KEIN Thread/keine PD entsteht; Audits==0)",
        if intrt { "ALL PASS" } else { "FAILURES" }
    );

    // ext-27 T4: Cross-Service-Matrix (ADR 0012).
    let cross = CROSS_DONE.load(Ordering::Acquire) && CROSS_OK.load(Ordering::Acquire);
    println!(
        "cross   : {} (Cross-Service-Matrix: drei extern geladene Angreifer DREIER Domaenen NEBENLAEUFIG -- aggressor-u (UserLand) + aggressor-t (TrustedSAS) melden UNABHAENGIG SUCCESS (gleichzeitige cross-domain Angreifer stoeren einander nicht), intruder-h (HardwareLand) faultet beim Fremdspeicher-Zugriff; kernel-geschuetztes Canary BIT-FUER-BIT unberuehrt; Audits==0)",
        if cross { "ALL PASS" } else { "FAILURES" }
    );

    // (HW-Fuzzer-Reportzeile wird von `fuzz::report()` ausgegeben, ADR 0013.)
}
