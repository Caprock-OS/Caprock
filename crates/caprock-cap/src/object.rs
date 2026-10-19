//! Kernel-Objekte, auf die Capabilities verweisen.
//!
//! Mehrere Caps können dasselbe Objekt referenzieren (über `copy`/`mint`); ein
//! Referenzzähler bestimmt, wann das Objekt finalisiert wird.

use caprock_mem::PhysRegion;

/// **DMA-Richtung** (ext-24): die Zugriffsrichtung des **Geräts** auf den DMA-Puffer. Bestimmt
/// die richtungsminimalen Hardware-Rechte (SMMU-Stage-1-AP) — ein reiner Lese-Puffer ist
/// gegen ein fehlerhaftes Gerät **schreibgeschützt**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDir {
    /// Das Gerät **liest** aus dem Puffer (CPU schreibt, Gerät liest). SMMU: read-only.
    DeviceRead,
    /// Das Gerät **schreibt** in den Puffer (Gerät schreibt, CPU liest). SMMU: read-write.
    DeviceWrite,
    /// Beide Richtungen. SMMU: read-write.
    Bidirectional,
}

/// **Cache-Kohärenz** (ext-24) eines DMA-Puffers gegenüber der CPU. Bestimmt die Speicher-
/// Attribute (cacheable vs. non-cacheable) und ob Cache-Maintenance (clean/invalidate) um
/// Transfers nötig ist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaCoherence {
    /// Hardware-kohärent (CCI/ACE): Normal **Cacheable**; Maintenance per `dma_prepare`/
    /// `dma_complete` (clean vor Geräte-Read, invalidate nach Geräte-Write).
    Coherent,
    /// Nicht kohärent: Normal **Non-Cacheable**; keine CPU-Cache-Maintenance nötig (Default,
    /// rückwärtskompatibel zu ext-23).
    NonCoherent,
}

/// Art des Objekts, auf das eine Capability verweist. Notifications, TCBs usw.
/// kommen in späteren Phasen hinzu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// Eine physische Speicherregion.
    Memory(PhysRegion),
    /// Ein IPC-Endpoint (referenziert per Endpoint-ID; Zustand liegt in der
    /// Endpoint-Tabelle des IPC-Subsystems).
    Endpoint(u32),
    /// Ein Thread (Thread-Control-Block, referenziert per gepacktem ThreadId-Raw).
    /// Ermöglicht capability-kontrolliertes Beenden (`KILL`).
    Tcb(u64),
    /// Ein Notification-Objekt (asynchrone Badge-Signale; referenziert per ID).
    Notification(u32),
    /// Ein **Scheduling Context** (MCS): CPU-Budget (`budget` Ticks je `period`
    /// Ticks). Die Cap ist die **Autorität**, einem Thread dieses Budget zuzuweisen
    /// — CPU-Zeit wird damit kapabilitätskontrolliert vergeben.
    SchedContext { budget: u32, period: u32 },
    /// Eine **Reply-Capability** (seL4-artig): die einmalige Autorität, einen konkreten
    /// per `CALL` blockierten Aufrufer (`caller`, gepacktes ThreadId-Raw) an Endpoint
    /// `ep` zu beantworten bzw. den Call abzubrechen. Wird beim Löschen/Revoke
    /// **finalisiert** -> der noch wartende Aufrufer wird mit `ERR_SERVER_GONE`
    /// entblockt (Revocation eines ausstehenden Calls). Genau einer je Call,
    /// call-spezifisch (matcht nur, solange `ep` noch diesen `caller` hält).
    Reply { ep: u32, caller: u64 },
    /// Eine **Management-Capability** (ext-22): die Autorität einer TrustedSas-PD, den
    /// Lifecycle einer Ziel-PD `pd` (typisch UserLand) zu steuern — starten/stoppen/
    /// pausieren/fortsetzen/Budget zuweisen/Cap übergeben/Reload (`SYS_PDCTL`). Nicht jede
    /// TrustedSas-PD bekommt sie; sie ist die explizite Steuerungsberechtigung über genau
    /// diese eine Ziel-PD. Hält keinen Allokator-Speicher (keine Finalisierung).
    PdControl { pd: u32 },
    /// Eine **Loader-Capability** (ext-26): die Autorität einer TrustedSas-PD, über den
    /// generischen Binary-Loader ein Programm aus `source` (0 = Boot-Archiv) zur Laufzeit zu
    /// laden + zu starten (`SYS_LOAD`). Wie `PdControl` eine reine **Autoritäts**-Cap (kein
    /// Allokator-Speicher, keine Finalisierung); nur in TrustedSas-PDs installierbar. Sie gewährt
    /// **keine** Sonderrechte am geladenen Prozess — dieser erhält nur die Caps, die der Aufrufer
    /// im `SYS_LOAD` explizit aus seinem eigenen Cspace delegiert.
    Loader { source: u32 },
    /// Eine **MMIO-Capability** (ext-22, HardwareLand): die Autorität, eine konkrete
    /// Geräte-Registerregion `[phys, phys+len)` in die eigene (isolierte) VSpace als
    /// EL0-Device zu mappen. Wird **nur kernelseitig** geprägt (kein User-Syscall erzeugt
    /// beliebige MMIO-Caps) und ist nur in HardwareLand-PDs installierbar. Verweist auf
    /// einen **Geräte**-Bereich (kein RAM-Allokator-Eintrag) -> keine Finalisierung.
    Mmio { phys: u64, len: u64 },
    /// Eine **IRQ-Capability** (ext-22, HardwareLand): die Autorität, den Geräte-Interrupt
    /// `intid` zu empfangen — der Kernel bindet ihn an eine Notification und stellt ihn dem
    /// Backend als Badge-Signal zu (`bind_irq`). Nur kernelseitig geprägt, nur in
    /// HardwareLand-PDs installierbar. Hält keinen RAM-Allokator-Eintrag -> keine Finalisierung.
    Irq { intid: u32 },
    /// Eine **DMA-Capability** (ext-23, HardwareLand): die Autorität über eine kernel-
    /// ausgeschnittene, kontiguierliche **RAM**-Region `[phys, phys+len)`, die als DMA-Puffer
    /// dient (Gerät liest/schreibt sie per Bus-Master). Nur kernelseitig geprägt, nur in
    /// HardwareLand-PDs installierbar. **Anders als Mmio/Irq ist dies echtes RAM** -> die
    /// Finalisierung gibt die Region an den Allokator zurück (`free_region`), **aber nur** weil
    /// die System-Teardown-Reihenfolge (`enforcer.disable_dma` -> VSpace-Unmap) garantiert, dass
    /// vorher kein Gerät mehr hineinschreiben kann (DMA-use-after-free-sicher). Die hardware-
    /// erzwungene Isolation (SMMUv3) liegt hinter der `DmaEnforcer`-Abstraktion im Kernel.
    ///
    /// ext-24: die Cap kodiert zusätzlich die **Richtung** (`dir`, → richtungsminimale SMMU-
    /// Rechte) und die **Cache-Kohärenz** (`coherence`, → Speicher-Attribute + Maintenance).
    /// Die Felder sind additiv; `install_dma` ohne sie nutzt `Bidirectional`/`NonCoherent`
    /// (= ext-23-Verhalten).
    Dma {
        phys: u64,
        len: u64,
        dir: DmaDir,
        coherence: DmaCoherence,
    },
    /// Eine **Syscall-Handler-Capability** (Z26/A3): die Autorität, der **Kernel eines Gastes**
    /// zu sein — den Syscall-Strom der an sie gebundenen Threads zu empfangen und ihre
    /// Trap-Frames zu beantworten.
    ///
    /// ## Warum das kein umgewidmeter Endpoint ist
    ///
    /// Die Autorität ist eine **andere** als „darf IPC empfangen": ein gewöhnlicher `REPLY`
    /// schreibt die Antwort einer Transaktion, dieser hier schreibt den **Ausführungszustand**
    /// eines fremden Threads. Er kann den Gast belügen. Läge beides auf derselben Art, könnte eine
    /// PD ihren *Dienst*-Endpoint versehentlich als Persönlichkeit binden, und `cdt_audit` könnte
    /// die Frage „wer darf hier fremde Frames schreiben" **nicht stellen**.
    ///
    /// ## Die Felder — und warum das Sidecar zur Cap gehört
    ///
    /// * `ep` — Endpoint, an dem die Umleitungsnachricht zugestellt wird (Transport; der Handler
    ///   `RECV`t dort). Die Nachricht sagt nur *welcher Gast* und *warum*.
    /// * `sidecar` / `len` — das **geteilte Fenster**, in dem der Kernel den Trap-Frame des Gastes
    ///   ablegt und aus dem er ihn zurückliest, ein Slot je gebundenem Gast-Thread
    ///   (`caprock_sched::redirect::SLOT_BYTES`).
    ///
    /// Der Frame liegt dort und nicht in der Nachricht, weil `caprock_abi::MSG_WORDS` **4** ist
    /// und ein Trap-Frame 22 (x86_64) bzw. 34 (aarch64) Wörter hat. `rt_sigreturn` ersetzt den
    /// **ganzen** Frame und `clone` braucht einen **zweiten** — beides ist über vier Wörter
    /// strukturell unmöglich, nicht bloss unbequem.
    ///
    /// Und es ist die Form, die Fuchsias `zx_restricted_bind_state` nimmt: der Handler bekommt
    /// **eine Region**, keine Fähigkeit, fremde Register zu schreiben. Damit halbiert sich die
    /// erste der drei Autoritäten aus Z26/Nachtrag 2 — die Persönlichkeits-PD bleibt der Kernel
    /// des Gastes, aber ihr Zugriff auf dessen Registerzustand ist **eine benannte Region** und
    /// steht in der Speicherbuchhaltung, nicht nur im Cap-Audit.
    ///
    /// `pd` ist die **Persönlichkeits-PD** — der Knoten, an dem das Zyklusverbot hängt.
    ///
    /// Sie steht **in der Cap** und wird nicht aus dem Besitz abgeleitet, und das ist eine
    /// Entscheidung: eine Cap darf kopiert werden, „der Besitzer" ist danach mehrdeutig. Die Cap
    /// bezeichnet eine bestimmte PD mit einem bestimmten Endpoint und einem bestimmten Fenster;
    /// wer sie weitergibt, gibt genau diese Autorität weiter und macht den Empfänger **nicht** zum
    /// Gast-Kernel. Nur kernelseitig geprägt — kein User-Syscall erzeugt beliebige Handler-Caps.
    ///
    /// Hält keinen Allokator-Eintrag (das Fenster wird über eine `Memory`-Cap vergeben) → keine
    /// Finalisierung.
    SyscallHandler {
        ep: u32,
        pd: u16,
        sidecar: u64,
        len: u64,
    },
    /// Eine **Fault-Handler-Capability** (Z26/A3): die Autorität, die **Seitenfehler** der an sie
    /// gebundenen Threads zu sehen.
    ///
    /// **Getrennt von [`Self::SyscallHandler`]**, weil es eine andere Autorität ist: Syscalls zu
    /// beantworten heisst „ich bin der Kernel dieses Gastes", Faults zu sehen heisst „ich verwalte
    /// seinen Speicher" — `mmap`-Semantik braucht das zweite, ein Debugger nur das zweite, eine
    /// reine Syscall-Persönlichkeit nur das erste. Ein Kanal mit zwei Bedeutungen ist die Form,
    /// die dieses Projekt schon dreimal bezahlt hat (`blocked`, die Park-Naht, `CR0.TS`).
    ///
    /// **Was sie NICHT gewährt:** einen Fault zu *beheben* heisst, Mappings im **Gast-Vspace** zu
    /// installieren. Das ist eine dritte Autorität, sie kommt in diesem Primitiv nicht vor, und
    /// ohne sie ist `mmap` nicht implementierbar (s. `TODO.md` Z26/A3, „was offen bleibt").
    FaultHandler {
        ep: u32,
        pd: u16,
        sidecar: u64,
        len: u64,
    },
    /// **Die Wurzel der Debug-Autoritaet ueber genau eine PD** (Z6b).
    ///
    /// Sie gewaehrt selbst **nichts** — weder Lesen noch Anhalten. Sie ist das Recht, die beiden
    /// abgeleiteten Rechte darunter zu praegen, und damit der Knoten, an dem `revoke` ansetzt.
    ///
    /// ## Warum die Praegung selbst die Zusage IST
    ///
    /// Die Zusage lautet: *eine PD, ueber die nie eine solche Cap gepraegt wurde, kann nicht
    /// debuggt werden.* Wuerde jede PD eine bekommen, beschriebe dieser Satz **keine einzige PD** —
    /// dieselbe Form wie ein RMRR-Test auf q35 oder die `smt`-Zeile unter `threads=1`: wahr, weil
    /// der Vordersatz leer ist. Deshalb ist **Nicht-Praegen die Vorgabe**, und die Entscheidung
    /// steht im **signierten** Manifest (`POLICY_DEBUGGABLE`), nicht in einem Laufzeitschalter.
    ///
    /// ## Und warum das Zurueckziehen an dieser Cap haengt und nicht an den Kindern
    ///
    /// Wer sie haelt, kann jederzeit ein neues `DebugControl` ableiten. Ein Freigabefenster, das
    /// ueber die Lebensdauer eines *abgeleiteten* Caps begrenzt wird, schliesst deshalb nicht — die
    /// **Wiederbeschaffbarkeit** der Autoritaet ueberlebt es. Das Fenster endet mit dem Revoke
    /// dieser Cap.
    ///
    /// ## Die Ableitung laeuft ueber RECHTE, nicht ueber eine zweite Objektart
    ///
    /// Der erste Entwurf sah drei Cap-Arten vor (`Debuggable`/`DebugRead`/`DebugControl`).
    /// **Gemessen am 2026-08-20 traegt das nicht:** dieses CDT leitet ueber `copy`/`mint` ab, und
    /// ein Kind zeigt auf **dasselbe Objekt** wie sein Elter — eine zweite Art waere gar kein Kind,
    /// sondern eine unabhaengige Wurzel. Der Fehler war nicht theoretisch: die `dbg`-Zeile meldete
    /// `revoke-bricht-nicht=false` und `nachher-undebuggbar=false`, weil `revoke` an der Wurzel die
    /// „abgeleiteten" Caps gar nicht sah.
    ///
    /// Also: **eine Art, und die Rechte unterscheiden.**
    ///
    /// | Recht | darf |
    /// |---|---|
    /// | `READ` | Frame und Speicher lesen — haelt **nie** etwas an |
    /// | `WRITE` | anhalten, fortsetzen, Register schreiben |
    ///
    /// Damit sind beliebig viele Leser trivial richtig (ein Grund**bit** hat keinen Referenzzaehler,
    /// also schrumpft die Eigentumsfrage auf das Anhalten), und `revoke` an der Wurzel nimmt jedes
    /// Kind mit — was die eigentliche Zusage ist.
    ///
    /// **Die Wurzel zaehlt selbst als Steuerrecht**, und das ist keine Nachlaessigkeit: wer sie
    /// haelt, praegt jederzeit ein Kind mit `WRITE`. Sie fuer sich als „gewaehrt nichts" zu fuehren
    /// waere eine Unterscheidung ohne Unterschied — und genau die Sorte, die ein Freigabefenster
    /// scheinbar schliesst, waehrend die Wiederbeschaffbarkeit weiterlaeuft.
    ///
    /// Haelt keinen Allokator-Speicher -> keine Finalisierung.
    Debuggable { pd: u16 },
}

/// Eintrag der Objekt-Tabelle.
///
/// **Öffentlich, aber undurchsichtig** (A-3.4): seit die Objekttabelle zur Boot-Zeit
/// dimensioniert wird, legt der *Kernel* den Speicher an (`Slab<Object>`) und braucht dafür
/// den Typ und [`Object::EMPTY`]. Die Felder bleiben `pub(crate)` — von aussen ist ein
/// `Object` ein Platzhalter ohne Innenleben, insbesondere ist `refcount` nicht von aussen
/// veränderbar. Nur so bleibt die Cap-Crate frei von `unsafe` (`forbid(unsafe_code)`), ohne
/// ihre Kapselung dafür aufzugeben.
#[derive(Clone, Copy)]
pub struct Object {
    pub(crate) used: bool,
    pub(crate) kind: ObjectKind,
    /// Anzahl der auf dieses Objekt verweisenden Capabilities.
    pub(crate) refcount: u32,
    /// Generationszähler (gegen stale Objekt-Indizes).
    pub(crate) gen: u32,
}

impl Object {
    pub const EMPTY: Object = Object {
        used: false,
        kind: ObjectKind::Memory(PhysRegion::new(0, 0)),
        refcount: 0,
        gen: 0,
    };
}
