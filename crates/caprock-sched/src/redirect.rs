//! **Umgeleitete Syscalls — der Kern des Primitivs** (Z26/A3, gebaut 2026-08-10).
//!
//! Ein Thread einer PD führt Fremdcode aus; sein Syscall-Eintritt wird nicht vom Caprock-Kernel
//! bearbeitet, sondern an eine **Persönlichkeits-PD** zugestellt. Das ist die vierte Vorbedingung
//! aus [Z26] und steht auf keinem anderen Weg — sie ist der Preis der Entscheidung „Weg A".
//!
//! **Das Ziel ist NICHT gerätespezifisch.** Der Kernel kennt genau einen Satz: „die Syscalls
//! dieses Threads gehen an Handler H". Keine ioctl-Nummer, keine Gerätedatei, kein GPU-Begriff
//! kommt in dieser Datei vor — NVIDIA/CUDA ist der teuerste Anwendungsfall einer beliebigen
//! Linux-Persönlichkeit, nicht ihr Gegenstand.
//!
//! ## Warum diese Datei abhängigkeitsfrei ist
//!
//! Dieselbe Begründung wie bei [`crate::cycles`] (B-5.1): die Fallen hier sind **Reihenfolgen und
//! Zahlenbereiche** — ein Zyklus in der Handler-Kette, ein Slot-Index um eins daneben, ein
//! Rückfall auf die native ABI nach dem Entzug einer Cap. Alle drei lassen sich mit **Literalen**
//! auslösen und brauchen weder Maschine noch QEMU. Der Rest (Cap-Auflösung, Frame-Transport,
//! Scheduler) steht anderswo; hier steht die **Entscheidung**.
//!
//! ## Der Frame-Transport ist ein SIDECAR, keine Register-Schreib-Cap
//!
//! Der erste Entwurf in Z26 gab dem Handler eine Cap, mit der er **fremde Ergebnisregister
//! schreibt**. Gebaut ist etwas anderes, und der Grund ist gemessen, nicht ästhetisch:
//!
//! | | Wörter | Bytes |
//! |---|---|---|
//! | `caprock_abi::MSG_WORDS` (register-basierte IPC) | **4** | 32 |
//! | `TrapFrame` x86_64 (15 GPR + Vektor/Fehler/rip/cs/rflags/rsp/ss) | **22** | **176** |
//! | `TrapFrame` aarch64 (x0..x30 + ELR + SPSR + SP_EL0) | **34** | **272** |
//!
//! Eine Nachricht mit vier Wörtern kann `rt_sigreturn` (ersetzt den **ganzen** Frame) und `clone`
//! (braucht einen **zweiten** Frame) strukturell nicht tragen. Also liegt der Frame in einem
//! geteilten Fenster — dem **Sidecar** —, und der Handler liest und schreibt ihn **als Speicher**.
//! Das ist die Form, die Fuchsias `zx_restricted_bind_state` nimmt, und sie ist hier aus drei
//! Gründen die bessere:
//!
//! 1. **Die Autorität ist eine Region, keine Fähigkeit über fremden Registerzustand.** Z26 rechnet
//!    ehrlich zusammen: „die Persönlichkeits-PD hält Frame-Schreibrecht, vollen Speicherzugriff,
//!    Vspace-Manipulation — sie IST der Kernel des Gastes." Die Sidecar-Form **halbiert die
//!    erste** dieser drei: es gibt keine Operation „schreibe die Register des Threads T". Es gibt
//!    ein Fenster, in dem die Frames **seiner eigenen** Gäste liegen.
//! 2. **Sie ist auditierbar.** „Wer darf fremde Register schreiben" ist über eine Cap-Art
//!    beantwortbar; „welchen Speicher hält diese PD" ist es ohnehin. Eine Region taucht in der
//!    Speicherbuchhaltung auf, ein Schreibrecht nur im Cap-Audit.
//! 3. **Sie ist nicht umlenkbar.** Eine Register-Schreib-Cap müsste im Kernel auf „nur an mich
//!    gebundene Threads" eingeschränkt werden (Z26 sagt das ausdrücklich) — also eine Prüfung, die
//!    bei jedem `REPLY` richtig sein muss. Der Sidecar-Slot ist diese Einschränkung **von selbst**:
//!    ein Slot gehört genau einem Gast, und der Kernel schreibt nur aus dem Slot in den Frame des
//!    Gastes, dem er gehört.
//!
//! **Was die Sidecar-Form NICHT kann, und es ist genau eine Sache:** sie braucht **einen Slot je
//! gebundenem Gast-Thread**, nicht einen je Handler-Thread. Bei Fuchsia gehört das Sidecar dem
//! Thread, der `restricted mode` betritt — es gibt dort nichts umzuhängen. Hier ist der Handler
//! eine eigene PD mit eigenen Threads, an der mehrere Gäste hängen; ein einziges Fenster für alle
//! wäre ein Rennen zwischen zwei gleichzeitigen Gast-Syscalls. Der Preis ist Speicher
//! ([`SLOT_BYTES`] je Gast) und eine Schranke ([`BindUrteil::KeinSlot`]), nicht Umhängen.
//!
//! ## Was dieses Primitiv NICHT vergibt — ausdrücklich
//!
//! Z26/Nachtrag 2 zählt vier Autoritäten auf, die eine Linux-Persönlichkeit braucht. Dieses
//! Primitiv liefert **eine und eine halbe** — und die Zeile „Frame“ ist seit dem 2026-08-13 in
//! zwei geteilt, weil **Lesen und Schreiben nicht dieselbe Autorität sind**: der ganze Frame
//! enthält `cs`/`ss` bzw. `spsr`, also den RING. Wer sie zurückschreiben darf, befördert seinen
//! Gast — das wäre genau die Rechteausweitung, gegen die die Weiche unten steht, nur von der
//! anderen Seite:
//!
//! | Autorität | hier | Folge, wenn sie fehlt |
//! |---|---|---|
//! | Frame **lesen** | **ja**, ganzer Frame über das Sidecar | die Persönlichkeit sieht Nummer, Argumente und Zustand |
//! | Frame **schreiben** | **halb** — nur die Allzweckregister (s. [`uebernehmbar`]) | ein registerbasierter Syscall ist vollständig; `rt_sigreturn` braucht zusätzlich `rip`/`rsp` bzw. `elr`/`sp_el0`, und die sind **noch nicht** übernehmbar |
//! | Gast-**Speicher** lesen/schreiben (Zeigerargumente) | **nein** | jeder Syscall mit Zeiger (`read`, `write`, `openat`, `ioctl`) ist **nicht implementierbar**, solange die Gast-Region nicht getrennt in die Handler-PD gemappt ist |
//! | **Vspace** des Gastes manipulieren | **nein** | `mmap`/`mprotect`/Demand-Paging sind **nicht implementierbar** |
//! | Faults **sehen** | **ja**, getrennte Cap ([`Anlass::Fault`]) | — |
//!
//! Die beiden Neins sind **kein Versehen und keine Sparsamkeit**: sie sind eigene Caps mit eigenen
//! Entwurfsfragen (welche Cap trägt „der ganze Adressraum von G"? ein Kernel-Kopierdienst kostet
//! TCB je Syscall). Sie hier mitzunehmen hiesse, drei Entscheidungen in einer Zeile zu treffen —
//! dieselbe Form wie ein Wert mit zwei Bedeutungen. Sie stehen als offener Punkt in `TODO.md`.
//!
//! ## Der Adressraumwechsel ist eine OFFENE GRÖSSE, kein Nebensatz
//!
//! Bei Fuchsia teilen sich Gast und `starnix_kernel` **einen** Adressraum (untere Hälfte Gast,
//! obere Hälfte Supervisor); ein Syscall-Rundlauf kostet dort einen **Moduswechsel**, keinen
//! Adressraumwechsel. Diese Fassung setzt den Handler in eine **eigene PD** und wechselt damit
//! **zweimal je umgeleitetem Syscall**.
//!
//! **Das ist eine Entwurfsentscheidung und sie ist vor dem Bau gefallen**, mit Grund: Caprocks
//! isolierte PDs teilen sich die statischen oberen Tabellen (`ISO_PD_HIGH`, GiB 1..3). Einen
//! Handler dort einzublenden gäbe ihn **jeder** isolierten PD — genau die Falle, die in
//! `CLAUDE.md` unter „Geteilte Seitenverzeichnisse vertragen keine PD-spezifischen Einträge"
//! steht. Die Starnix-Form wäre hier also nicht billig, sondern verlangte private obere Tabellen
//! je Gast; und sie setzte den Kernel des Gastes **in den Adressraum des Gastes**, was der
//! Trennung widerspricht, um derentwillen das Ganze cap-förmig ist.
//!
//! Der Preis ist damit benannt und **nicht gemessen**: zwei Adressraumwechsel je Umlauf, auf x86
//! ohne PCID also zweimal ein vollständig geleerter TLB (Z18 (3)). Die Schwelle, ab der die
//! cap-förmige Fassung fällt, steht in `TODO.md` Z26/A3 — **vor** der Messung.
//!
//! [Z26]: ../../../TODO.md

#![allow(clippy::needless_range_loop)]

// ---------------------------------------------------------------------------------------
// Sidecar-Arithmetik
// ---------------------------------------------------------------------------------------

/// **Bytes je Sidecar-Slot.** Ein Slot fasst den Trap-Frame **eines** gebundenen Gast-Threads.
///
/// Die Zahl ist hergeleitet, nicht gewählt: der grösste Frame der beiden Architekturen ist
/// aarch64 mit 34 `u64` = 272 Bytes (x86_64: 22 `u64` = 176). 512 ist die nächste Zweierpotenz
/// darüber — die Potenz zählt, weil der Offset auf dem **heissen Pfad** eine Schiebung sein soll
/// und keine Multiplikation.
pub const SLOT_BYTES: usize = 512;

/// Grösster Frame, der in einen Slot passen muss (aarch64: 34 `u64`). Steht hier, damit die
/// Behauptung „512 reicht" **prüfbar** ist statt behauptet — s. `slot_fasst_beide_frames`.
pub const FRAME_MAX_BYTES: usize = 272;

/// Byte-Offset des Slots `slot` im Sidecar-Fenster.
///
/// **Ohne Schranke** — die Schranke ist [`slot_gueltig`], und sie steht getrennt, weil sie an der
/// **Vergabe** geprüft gehört und nicht erst beim Zugriff. Wer beides in eine Funktion legt, hat
/// eine Funktion, die im Fehlerfall entweder lügt (0 zurückgeben) oder panickt.
#[inline]
pub const fn slot_offset(slot: u16) -> usize {
    (slot as usize) * SLOT_BYTES
}

/// Ist `slot` in einem Fenster mit `slots` Plätzen gültig?
///
/// Der Fehler, gegen den das steht, ist ein Off-by-one: Slot `slots` läge **hinter** dem Fenster,
/// Slot `slots-1` ist der letzte. Ein Slot daneben heisst hier nicht „Absturz", sondern **ein Gast
/// schreibt in den Frame eines anderen** — die Klasse Fehler, die stumm bleibt.
#[inline]
pub const fn slot_gueltig(slot: u16, slots: u16) -> bool {
    slot < slots
}

// ---------------------------------------------------------------------------------------
// DAS SLOT-FORMAT — die Nutzlast (2026-08-13)
// ---------------------------------------------------------------------------------------
//
// Bis zum 2026-08-13 stellte `zustellen()` die **Nachricht** zu (welcher Gast, welcher Slot,
// welcher Anlass) und sonst nichts: der Handler erfuhr *dass* und *wer*, aber nicht *was*. Ein
// Primitiv, das den Frame nicht überträgt, ist richtig und nicht benutzbar.
//
// ## Warum es einen KOPF gibt und nicht nur den Frame
//
// Drei Gründe, und jeder einzelne würde reichen:
//
//  1. **Ein leerer Slot sieht aus wie ein Frame.** Ohne Kennung ist „der Kernel hat hier nichts
//     hingeschrieben" von „der Gast hatte lauter Nullen in den Registern" nicht zu unterscheiden.
//     Das ist wörtlich die leere Event-Queue ohne `CD.R`: eine Aussage sieht wahr aus, weil der
//     Fall, der sie widerlegen könnte, nicht sichtbar wird. Deshalb [`MAGIE`].
//  2. **Ein WIEDERHOLTER Frame sieht aus wie ein neuer.** Ein Slot wird über die Lebensdauer
//     eines Gastes hunderttausendfach beschrieben; der Handler muss „das ist die Zustellung, auf
//     die ich gerade warte" von „das steht hier noch von vorhin" unterscheiden können. Deshalb
//     [`KOPF_GEN`], monoton und nur vom Kernel geschrieben — dieselbe Konstruktion wie die
//     wachsende Kette bei Z4 Stufe 2 (ein reproduzierbarer Wert kann nicht belegen, dass er
//     geerbt wurde).
//  3. **Der Frame ist ARCHITEKTURABHÄNGIG, die Persönlichkeit soll es nicht sein.** Wo x0 liegt,
//     weiss der Kernel; der Handler soll es nicht nachrechnen müssen (ein Prüfer, der die
//     geprüfte Grösse nachrechnet, prüft eine zweite Wirklichkeit). Deshalb die Indextabelle
//     [`KOPF_ABI`].
//
// ## Lesen und Schreiben sind NICHT dieselbe Autorität
//
// Der Kopf trennt sie: [`KOPF_NGESAMT`] Wörter werden **hingeschrieben**, aber nur die ersten
// [`KOPF_NGPR`] dürfen **zurück** (s. [`uebernehmbar`]). Der Grund steht dort.

/// **Kennung im ersten Wort eines beschriebenen Slots.** „Der Kernel hat hier einen Frame
/// abgelegt" — die Aussage, ohne die ein leerer Slot von einem Frame aus Nullen nicht zu
/// unterscheiden wäre.
pub const MAGIE: u64 = 0x4350_524B_5F46_524D; // "CPRK_FRM"

/// **Formatversion des Slots.** Steht im zweiten Wort und wird bei jedem Lesen geprüft.
///
/// Die Vorlage ist A-4.3 (Zustandsübergabe beim Hot-Reload): dort trägt die Region einen
/// versionierten Kopf, und ein abweichendes Layout wird **abgewiesen**, statt die Bytes im
/// eigenen Sinn zu lesen. Dieselbe Regel wie bei `entry_len` im Manifest — und dieselbe
/// Begründung: das Sidecar ist die **Vertragsfläche** zwischen Kernel und Persönlichkeit, und
/// eine Persönlichkeit ist per Entwurf nicht dieselbe Fassung wie der Kernel. Ein implizites
/// Registerabbild hiesse: jede Änderung am Frame bricht jede Persönlichkeit **still**.
pub const FORMAT_VERSION: u64 = 1;

/// Wort 0: [`MAGIE`].
pub const KOPF_MAGIE: usize = 0;
/// Wort 1: [`FORMAT_VERSION`].
pub const KOPF_VERSION: usize = 1;
/// Wort 2: **Zustellungszähler**, monoton, beginnt bei 1.
///
/// Nur der Kernel schreibt ihn. Der Handler merkt sich den zuletzt gesehenen Stand; ein Frame mit
/// gleichem oder kleinerem Zähler ist ein **alter** Frame, kein neuer. Ohne diese Zahl könnte er
/// eine ausgebliebene Zustellung nicht von einer wiederholten unterscheiden — genau die
/// Verwechslung `rx_used` gegen „Daten sind angekommen".
pub const KOPF_GEN: usize = 2;
/// Wort 3: [`Anlass`] als Zahl.
pub const KOPF_ANLASS: usize = 3;
/// Wort 4: architekturabhängiger Anlasscode (x86: Vektor · aarch64: `ESR_EL1.EC`), `0` bei einem
/// Syscall.
pub const KOPF_CODE: usize = 4;
/// Wort 5: wie viele Frame-Wörter **zurückgeschrieben** werden dürfen (s. [`uebernehmbar`]).
pub const KOPF_NGPR: usize = 5;
/// Wort 6: wie viele Frame-Wörter überhaupt abgelegt sind — die **Länge** des Kopfes im Sinne von
/// A-4.3.
pub const KOPF_NGESAMT: usize = 6;
/// Wort 7: Architekturkennung ([`ARCH_X86_64`] / [`ARCH_AARCH64`]).
///
/// Sie ist **nicht** bloss Auskunft: der Kernel weist beim Zurückschreiben einen Slot ab, dessen
/// Architektur nicht die eigene ist. Ohne diese Prüfung läse er die Wörter einer fremden
/// Registerlage als seine eigenen GPR.
pub const KOPF_ARCH: usize = 7;
/// Wort 8: **Handler-GRUPPE** — reserviert, heute immer `0`.
///
/// Die Zielarchitektur sieht eine kleine, feste Zahl von Gruppen vor, in die Handler eingehängt
/// werden (Dateisystem, Speicher, Prozess, Netz, Geräte). Der Schnitt folgt dem **geteilten
/// Zustand** (fd-Tabelle, Speicherkarte, TCB-Zustand, Sockets, Geräte-Caps) und nicht der
/// Syscall-Nummer — deshalb steht das Feld hier und wird später nicht aus der Nummer *erraten*.
///
/// **Gebaut ist der Mechanismus NICHT**, nur das Feld. Und es gilt die Regel der reservierten
/// Manifest-Bytes: **`0` heisst „keine Angabe", nicht „passt auf alles"**. Ein Slot, der mit einer
/// Gruppe zurückkommt, wird deshalb **abgewiesen** ([`KopfUrteil::Reserviert`]) und nicht
/// ignoriert — sonst wäre der Tag der Einlösung der Tag, an dem alle bis dahin angesammelten
/// Werte falsch sind (Z11c, die Prioritäten im Test-Manifest).
pub const KOPF_GRUPPE: usize = 8;
/// Wörter 9..16: reserviert, **geprüft genullt**. Ein reserviertes Feld, das niemand prüft, ist
/// ein Feld, das still zu Müll wird.
pub const KOPF_RESERVIERT: usize = 9;
/// Wie viele reservierte Wörter.
pub const KOPF_RESERVIERT_N: usize = 7;
/// Erstes Wort der **ABI-Indextabelle**: `slot[KOPF_ABI + n]` ist der Frame-Wort-Index des
/// ABI-Registers `xn`. Damit findet eine Persönlichkeit ihre Argumente, ohne die Registerlage der
/// Architektur nachzubilden — ein Leser, der die geprüfte Grösse nachrechnet, prüft eine zweite
/// Wirklichkeit.
pub const KOPF_ABI: usize = 16;
/// Wie viele ABI-Register die Tabelle führt (`x0..x6`).
pub const KOPF_ABI_N: usize = 7;
/// Erstes Wort des **Frames**.
pub const FRAME_WORT: usize = 24;

/// Architekturkennung für [`KOPF_ARCH`].
pub const ARCH_X86_64: u64 = 0;
/// Architekturkennung für [`KOPF_ARCH`].
pub const ARCH_AARCH64: u64 = 1;

/// Wörter je Slot.
pub const SLOT_WOERTER: usize = SLOT_BYTES / 8;
/// Wie viele Frame-Wörter hinter dem Kopf noch Platz haben.
pub const FRAME_WOERTER_MAX: usize = SLOT_WOERTER - FRAME_WORT;

/// Wort-Index des `i`-ten Frame-Wortes im Slot.
#[inline]
pub const fn frame_wort(i: usize) -> usize {
    FRAME_WORT + i
}

/// Passt ein Frame mit `n` Wörtern hinter den Kopf?
///
/// Steht getrennt von [`fenster_deckt`], weil es eine andere Frage ist: dort geht es um das
/// Fenster gegen die Slots, hier um den **Kopf** gegen den Frame. Ein Slot, der den Frame nur
/// deshalb fasst, weil niemand den Kopf mitgerechnet hat, überschriebe seinen eigenen Anfang.
#[inline]
pub const fn frame_passt(n: usize) -> bool {
    n <= FRAME_WOERTER_MAX
}

/// **Urteil über den Kopf eines Slots** — jede Absage mit eigenem Namen.
///
/// Die Vorlage ist wörtlich A-4.3: „was drüben nicht dasselbe bezeichnen kann, wird abgewiesen,
/// nicht ausgelegt". Ein Leser, der bei unbekannter Version einfach weiterliest, interpretiert
/// fremde Bytes im eigenen Sinn — und das ist beim Sidecar nicht ein Formatfehler, sondern ein
/// **Registerinhalt**, den der Kernel in einen laufenden Thread schreibt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KopfUrteil {
    /// Der Kopf ist lesbar und gehört zu dieser Fassung.
    Ok,
    /// [`MAGIE`] fehlt: hier hat **nie** jemand einen Frame abgelegt. Unterschieden von einer
    /// fremden Version, weil es eine andere Diagnose ist — „leer" heisst „die Zustellung ist
    /// ausgeblieben", „fremd" heisst „zwei Fassungen reden miteinander".
    KeinFrame,
    /// Die Formatversion ist nicht [`FORMAT_VERSION`]. **Benannt abgewiesen**, nicht ausgelegt.
    FremdeVersion(u64),
    /// Die Architekturkennung ist nicht die eigene: die Frame-Wörter hätten eine andere Bedeutung.
    FremdeArchitektur(u64),
    /// Die Wortzahlen passen nicht zu dieser Architektur (`n_gpr`/`n_gesamt`).
    FremdeBreite(u64, u64),
    /// Ein **reserviertes** Feld ist nicht null — dazu zählt die Gruppenkennung. `0` heisst
    /// „keine Angabe"; ein Wert heisst „ich verlange etwas, wofür es keinen Mechanismus gibt",
    /// und das wird abgewiesen statt still ignoriert.
    Reserviert,
}

/// **Den Kopf eines Slots prüfen**, bevor irgendein Frame-Wort gelesen wird.
///
/// `slot` ist der ganze Slot als Wortfeld; `arch`/`n_gpr`/`n_ges` sind die Erwartungen des
/// **Lesers**. Die Funktion entscheidet nichts über den Inhalt — sie sagt nur, ob der Inhalt
/// überhaupt in dieser Fassung gemeint war.
pub fn kopf_pruefen(slot: &[u64], arch: u64, n_gpr: u64, n_ges: u64) -> KopfUrteil {
    if slot.len() < FRAME_WORT {
        return KopfUrteil::KeinFrame;
    }
    if slot[KOPF_MAGIE] != MAGIE {
        return KopfUrteil::KeinFrame;
    }
    // **Die Version zuerst.** Danach erst darf irgendein anderes Feld gelesen werden: bei einer
    // fremden Version bedeutet auch „Wort 7 ist die Architektur" nichts mehr.
    if slot[KOPF_VERSION] != FORMAT_VERSION {
        return KopfUrteil::FremdeVersion(slot[KOPF_VERSION]);
    }
    if slot[KOPF_ARCH] != arch {
        return KopfUrteil::FremdeArchitektur(slot[KOPF_ARCH]);
    }
    if slot[KOPF_NGPR] != n_gpr || slot[KOPF_NGESAMT] != n_ges {
        return KopfUrteil::FremdeBreite(slot[KOPF_NGPR], slot[KOPF_NGESAMT]);
    }
    if slot[KOPF_GRUPPE] != 0 {
        return KopfUrteil::Reserviert;
    }
    let mut i = 0;
    while i < KOPF_RESERVIERT_N {
        if slot[KOPF_RESERVIERT + i] != 0 {
            return KopfUrteil::Reserviert;
        }
        i += 1;
    }
    KopfUrteil::Ok
}

/// **Darf Frame-Wort `i` aus dem Sidecar in den Frame des Gastes ZURÜCK?**
///
/// Nur die ersten `n_gpr` Wörter — die **Allzweckregister**. Alles dahinter ist Zustand, den der
/// Kernel führt und der die AUSFÜHRUNGSART bestimmt, nicht den Inhalt:
///
/// | Architektur | zurück | **nicht** zurück | warum nicht |
/// |---|---|---|---|
/// | x86_64 | `gpr[0..15]` | `vector`, `error`, `rip`, `cs`, `rflags`, `rsp`, `ss` | `cs`/`ss` tragen den **Ring**: ein Handler, der `cs` schreiben darf, setzt seinen Gast nach Ring 0. `rflags` trägt `IOPL` und `IF`. `rip`/`rsp` sind harmlos*er*, aber ein nicht-kanonischer `rip` schlägt beim `iretq` **im Kernel** auf |
/// | aarch64 | `gpr[0..31]` | `elr`, `spsr`, `sp_el0` | `spsr` trägt das **Exception-Level** und die Maskenbits — dasselbe Argument wie `cs` |
///
/// **Das ist eine Einschränkung gegenüber dem Entwurfstext von Z26/A3**, und sie steht hier statt
/// in einer Fussnote: dort heisst es „Frame lesen **und schreiben** — ja, ganzer Frame über das
/// Sidecar → `rt_sigreturn`, `clone` sind damit möglich". Der ganze Frame wird **gelesen**; der
/// ganze Frame **zurückzuschreiben** ist eine andere und grössere Autorität, denn er enthält
/// Felder, mit denen eine Persönlichkeits-PD sich selbst befördern würde. `rt_sigreturn` braucht
/// zusätzlich `rip`/`rsp` (bzw. `elr`/`sp_el0`); die sind **absichtlich** noch nicht dabei —
/// jedes von ihnen braucht eine eigene Gültigkeitsprüfung (Kanonizität, Ausrichtung), und drei
/// Entscheidungen in einer Zeile zu treffen ist die Form, die dieses Projekt schon bezahlt hat.
#[inline]
pub const fn uebernehmbar(i: usize, n_gpr: usize) -> bool {
    i < n_gpr
}

// ---------------------------------------------------------------------------------------
// Z6b: die dritte Autoritaetsstufe
// ---------------------------------------------------------------------------------------

/// **Wieviel vom Frame ein Aufrufer zurueckschreiben darf.**
///
/// Drei Stufen, und die mittlere ist neu:
///
/// | Rolle | darf schreiben |
/// |---|---|
/// | Umleitungshandler ([`uebernehmbar`]) | Allzweckregister |
/// | **Debugger** | **GPR + PC + SP + maskierte Flags** |
/// | niemand, nie | `cs`/`ss` (x86), `spsr` (aarch64) — das ist der **Ring** |
///
/// Ohne `PC` gibt es kein `jump`, kein `return` und keine Fortsetzung nach einem Haltepunkt; ohne
/// `SP` keine Stackmanipulation. Beide koennen den Ring nicht aendern — aber sie sind nicht
/// umsonst: ein nicht-kanonischer `rip` schlaegt beim `iretq` **im Kernel** auf, nicht im Ziel.
/// Deshalb traegt jeder eine eigene Gueltigkeitspruefung, und `rflags` bekommt eine **Maske**
/// statt eines Ja/Nein: `TF` schaltet Einzelschritt ein (eine v2-Eigenschaft durch die Hintertuer)
/// und `IOPL` gibt Portzugriff.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchreibStufe {
    /// Der Umleitungshandler von Z26/A3.
    Handler,
    /// Ein Debugger mit `DebugControl` (Z6b).
    Debugger,
}

/// Wie ein Schreibversuch ausging. **Jede Absage mit eigenem Namen** — „abgewiesen" allein ist als
/// Diagnose wertlos, und die drei Lagen verlangen drei verschiedene Reaktionen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchreibUrteil {
    Ok,
    /// Das Wort traegt den **Ring**. Auf jeder Stufe abgewiesen, fuer jeden, immer.
    RingWort(usize),
    /// Das Wort liegt ueber der Autoritaet dieses Aufrufers.
    UeberDerStufe(usize),
    /// `rip`/`elr` ist nicht kanonisch — der Fehler traefe den **Kernel**, nicht das Ziel.
    NichtKanonisch(u64),
    /// `rsp`/`sp_el0` ist nicht 8-Byte-ausgerichtet.
    Schief(u64),
    /// Ein verbotenes Flagbit war gesetzt.
    VerbotenesFlag(u64),
}

/// x86: `TF` (Einzelschritt).
pub const RFLAGS_TF: u64 = 1 << 8;
/// x86: `IOPL` (Portzugriff).
pub const RFLAGS_IOPL: u64 = 3 << 12;
/// x86: was ein Debugger in `rflags` **nicht** setzen darf.
pub const RFLAGS_VERBOTEN: u64 = RFLAGS_TF | RFLAGS_IOPL;

/// Frame-Wort-Indizes je Architektur — **hier, weil die Regel hier steht.**
///
/// Ein Pruefer, der die gepruefte Groesse nachrechnet, prueft eine zweite Wirklichkeit. Die Zahlen
/// stammen aus `hal::exception::frame_woerter` beider Architekturen und werden von
/// [`indizes_stimmen`] gegen die vom Kernel gemeldete Frame-Breite gehalten.
pub struct FrameIndizes {
    pub n_gpr: usize,
    pub pc: usize,
    pub sp: usize,
    /// `None` auf aarch64: dort steckt der Flagteil in `spsr`, und `spsr` traegt das
    /// Exception-Level — es ist damit ein **Ringwort** und nicht maskierbar.
    pub flags: Option<usize>,
    pub ring: &'static [usize],
    pub n_gesamt: usize,
}

/// x86_64: `gpr[0..15]`, 15 = Vektor, 16 = Fehlercode, 17 = `rip`, 18 = `cs`, 19 = `rflags`,
/// 20 = `rsp`, 21 = `ss`.
pub const INDIZES_X86_64: FrameIndizes = FrameIndizes {
    n_gpr: 15,
    pc: 17,
    sp: 20,
    flags: Some(19),
    // `vector`/`error` stehen mit in der Ringliste: sie tragen zwar keinen Ring, aber sie sind
    // **Kernel-Buchfuehrung** ueber die Ursache des Eintritts. Wer sie schreibt, belaegt den
    // Kernel ueber seinen eigenen Grund, hier zu sein.
    ring: &[15, 16, 18, 21],
    n_gesamt: 22,
};

/// aarch64: `gpr[0..31]`, 31 = `elr`, 32 = `spsr`, 33 = `sp_el0`.
pub const INDIZES_AARCH64: FrameIndizes = FrameIndizes {
    n_gpr: 31,
    pc: 31,
    sp: 33,
    flags: None,
    ring: &[32],
    n_gesamt: 34,
};

/// Die Indizes zu einer Architekturkennung ([`ARCH_X86_64`] / [`ARCH_AARCH64`]).
pub fn indizes(arch: u64) -> Option<&'static FrameIndizes> {
    match arch {
        ARCH_X86_64 => Some(&INDIZES_X86_64),
        ARCH_AARCH64 => Some(&INDIZES_AARCH64),
        _ => None,
    }
}

/// **Sprechprobe der Tabelle:** stimmen die hier hinterlegten Zahlen mit dem ueberein, was der
/// Kernel tatsaechlich ablegt?
///
/// Ohne sie waere diese Tabelle eine zweite Wirklichkeit — und der Tag, an dem jemand ein Wort in
/// den Frame einfuegt, waere der Tag, an dem der Debugger `rflags` fuer `rsp` haelt und das
/// Ringwort daneben freigibt.
pub fn indizes_stimmen(arch: u64, n_gpr: usize, n_gesamt: usize) -> bool {
    matches!(indizes(arch), Some(i) if i.n_gpr == n_gpr && i.n_gesamt == n_gesamt)
}

/// **Darf `value` in Frame-Wort `i` geschrieben werden, auf Stufe `stufe`?**
///
/// Eine Funktion, nicht zwei. Zwei Kopien einer Regel sind der Riss, den dieses Projekt bei der
/// Farbarithmetik (`MASK_BITS` / `stripe`) schon einmal bezahlt hat: die eine wird nachgezogen,
/// die andere nicht, und die Abweichung ist eine Rechteausweitung.
pub fn writeback_erlaubt(arch: u64, i: usize, value: u64, stufe: SchreibStufe) -> SchreibUrteil {
    let Some(ix) = indizes(arch) else {
        // Unbekannte Architektur: **abweisen, nicht auslegen**. Die Woerter haetten eine andere
        // Bedeutung, und „vermutlich ein GPR" ist keine Sicherheitsaussage.
        return SchreibUrteil::UeberDerStufe(i);
    };
    // Der Ring zuerst, und unbedingt. Es gibt keine Stufe, auf der das erlaubt ist.
    if ix.ring.contains(&i) {
        return SchreibUrteil::RingWort(i);
    }
    if i >= ix.n_gesamt {
        return SchreibUrteil::UeberDerStufe(i);
    }
    if i < ix.n_gpr {
        return SchreibUrteil::Ok;
    }
    if stufe == SchreibStufe::Handler {
        return SchreibUrteil::UeberDerStufe(i);
    }
    if i == ix.pc {
        return if kanonisch(value) {
            SchreibUrteil::Ok
        } else {
            SchreibUrteil::NichtKanonisch(value)
        };
    }
    if i == ix.sp {
        return if value % 8 == 0 {
            SchreibUrteil::Ok
        } else {
            SchreibUrteil::Schief(value)
        };
    }
    if Some(i) == ix.flags {
        return if value & RFLAGS_VERBOTEN != 0 {
            SchreibUrteil::VerbotenesFlag(value & RFLAGS_VERBOTEN)
        } else {
            SchreibUrteil::Ok
        };
    }
    SchreibUrteil::UeberDerStufe(i)
}

/// Kanonische Form (x86-64 und aarch64 mit 48-Bit-VA): Bits 63..47 alle gleich.
pub fn kanonisch(va: u64) -> bool {
    let top = va >> 47;
    top == 0 || top == 0x1_ffff
}

/// Wie viele Slots passen in ein Fenster von `len` Bytes? (Abgerundet.)
#[inline]
pub const fn slots_in(len: u64) -> u16 {
    let n = len / (SLOT_BYTES as u64);
    if n > u16::MAX as u64 {
        u16::MAX
    } else {
        n as u16
    }
}

/// Deckt das Fenster `[0, len)` die Slots `0..slots` **vollständig** ab?
///
/// Die Richtung ist wichtig: geprüft wird, ob das **letzte Byte des letzten Slots** noch drin
/// liegt — nicht, ob der Slot-**Anfang** drin liegt. Ein Fenster von 600 Bytes hat Platz für
/// einen Slot, nicht für zwei, obwohl der zweite Slot bei Offset 512 anfängt.
#[inline]
pub const fn fenster_deckt(slots: u16, len: u64) -> bool {
    (slots as u64) * (SLOT_BYTES as u64) <= len
}

// ---------------------------------------------------------------------------------------
// Die Bindung
// ---------------------------------------------------------------------------------------

/// **„Die Syscalls dieses Threads gehen an H."** Steht im TCB, wandert also bei einer Migration
/// mit dem Thread mit (`Migrant` trägt den ganzen `Tcb`).
///
/// `sys_ep` und `fault_ep` sind **getrennt**, weil sie verschiedene Autoritäten sind: einen
/// Syscall zu beantworten heisst „ich bin der Kernel dieses Gastes", einen Seitenfehler zu sehen
/// heisst „ich verwalte seinen Speicher". Z26 nennt den überladenen Kanal als die Form, die dieses
/// Projekt dreimal bezahlt hat (`blocked`, die Park-Naht, `CR0.TS`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bindung {
    /// Endpoint, an dem die `SyscallHandler`-Cap hängt. [`KEIN_EP`] = kein Syscall-Handler.
    pub sys_ep: u32,
    /// Endpoint, an dem die `FaultHandler`-Cap hängt. [`KEIN_EP`] = kein Fault-Handler.
    pub fault_ep: u32,
    /// PD des Handlers — die Kante im Zyklus-Graphen.
    pub handler_pd: u16,
    /// Sidecar-Slot **dieses** Gastes. Genau einer, und nur der Kernel schreibt hinein.
    pub slot: u16,
    /// **Physische Basis des Sidecar-Fensters** der Handler-PD (0 = keins).
    ///
    /// Sie steht hier und nicht in der Cap, obwohl sie aus der Cap kommt: der Syscall-Pfad hat den
    /// Gast, nicht die Handler-Cap — die liegt im Cspace des Handlers, nicht in seinem. Ein
    /// Cap-Lookup je umgeleitetem Syscall wäre der Preis dafür, dieselbe Zahl zweimal zu führen;
    /// abgeschrieben wird sie **einmal**, bei der Bindung, und mit der Bindung fällt sie wieder
    /// weg. (Was daran offen bleibt, steht bei [`crate::Scheduler::handler_of`]: eine gelöschte
    /// Handler-**Cap** bei lebender Handler-**PD** lässt diese Zahl stehen — das ist derselbe
    /// benannte Riss wie `handler_lebt`, nicht ein zweiter.)
    pub sidecar: u64,
}

/// „Kein Endpoint" — ein Wert, den die Endpoint-Tabelle nie vergibt.
pub const KEIN_EP: u32 = u32::MAX;

impl Bindung {
    /// Hat dieser Thread einen **Syscall**-Handler?
    #[inline]
    pub const fn hat_syscall(&self) -> bool {
        self.sys_ep != KEIN_EP
    }
    /// Hat dieser Thread einen **Fault**-Handler?
    #[inline]
    pub const fn hat_fault(&self) -> bool {
        self.fault_ep != KEIN_EP
    }
    /// Ist diese Bindung **wirkungslos** (weder Syscall noch Fault)? Eine solche Bindung darf gar
    /// nicht erst entstehen — sie wäre eine Kante im Zyklus-Graphen ohne Wirkung, also eine
    /// Absage, die nach Erfolg aussieht.
    #[inline]
    pub const fn ist_leer(&self) -> bool {
        !self.hat_syscall() && !self.hat_fault()
    }
}

/// Anlass einer Umleitung — steht in der Nachricht an den Handler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Anlass {
    /// Der Gast hat einen Syscall abgesetzt.
    Syscall = 0,
    /// Der Gast hat gefaultet (Seitenfehler, illegaler Befehl, …).
    Fault = 1,
}

// ---------------------------------------------------------------------------------------
// Die Weiche — die Regel, ohne die es eine RECHTEAUSWEITUNG wäre
// ---------------------------------------------------------------------------------------

/// Wohin geht dieser Eintritt?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weiche {
    /// Der Caprock-Kernel bearbeitet, wie ohne Umleitung.
    ///
    /// **Für Syscalls nur zulässig, wenn gar keine Bindung besteht.** Eine *weggefallene* Bindung
    /// darf nie hierher führen: aus dem Entzug einer Cap würde sonst eine **Beförderung** — der
    /// Gast spräche plötzlich direkt mit dem Kernel, mit der vollen nativen ABI, die ihm die
    /// Bindung gerade genommen hatte.
    Kernel,
    /// Zustellen an diesen Endpoint, mit diesem Sidecar-Slot in diesem Fenster.
    ///
    /// `sidecar` ist die **physische Basis** des Fensters. Sie steht in der Weiche und nicht erst
    /// im Zusteller, damit „wohin wird zugestellt" und „wohin wird kopiert" **eine** Entscheidung
    /// sind: zwei getrennte Auflösungen wären zwei Wahrheiten über dieselbe Bindung, und die
    /// stumme Fehlerform dieses Primitivs ist genau die, dass ein Gast in das Fenster eines
    /// anderen schreibt.
    Handler { ep: u32, slot: u16, sidecar: u64 },
    /// Die Bindung besteht, der Handler ist weg → der Gast **faultet**, mit benanntem Code.
    /// Nie still: eine Absage ohne Namen ist von einem Hänger nicht zu unterscheiden.
    Fault(u64),
}

/// ABI-Code, mit dem ein Gast faultet, dessen Handler weggefallen ist.
/// Spiegelt `caprock_abi::result::ERR_HANDLER_GONE` — die Zahl steht hier noch einmal, weil diese
/// Datei abhängigkeitsfrei sein muss; `abi_codes_stimmen_ueberein` im Kernel hält beide zusammen.
pub const ERR_HANDLER_GONE: u64 = 11;

/// ABI-Code, mit dem ein Gast zurückkehrt, dessen Sidecar-Kopf **nicht lesbar** war (fremde
/// Version, fremde Architektur, gesetzte reservierte Felder). Spiegelt
/// `caprock_abi::result::ERR_HANDLER_ABI`; die Klammer zieht `abi_codes_stimmen_ueberein`.
///
/// Getrennt von [`ERR_HANDLER_GONE`], weil es eine andere Diagnose ist: „niemand da" gegen „jemand
/// da, andere Fassung".
pub const ERR_HANDLER_ABI: u64 = 14;

/// **Die Weiche für einen SYSCALL.**
///
/// Drei Fälle, und der dritte ist die ganze Regel:
///
/// | Bindung | Handler lebt | → |
/// |---|---|---|
/// | keine | — | [`Weiche::Kernel`] |
/// | ja | ja | [`Weiche::Handler`] |
/// | ja | **nein** | [`Weiche::Fault`] — **nicht** `Kernel` |
///
/// Die naheliegende Fassung des dritten Falls („dann halt wie früher") ist genau die
/// Rechteausweitung. Sie sieht aus wie Robustheit und ist das Gegenteil.
#[inline]
pub fn weiche_syscall(bindung: Option<Bindung>, handler_lebt: bool) -> Weiche {
    match bindung {
        None => Weiche::Kernel,
        Some(b) if !b.hat_syscall() => Weiche::Kernel,
        Some(_) if !handler_lebt => Weiche::Fault(ERR_HANDLER_GONE),
        Some(b) => Weiche::Handler {
            ep: b.sys_ep,
            slot: b.slot,
            sidecar: b.sidecar,
        },
    }
}

/// **Die Weiche für einen FAULT** — und sie ist nicht die Spiegelung der obigen.
///
/// Der Unterschied ist der Punkt: bei einem Syscall ist „der Kernel macht es" eine **Beförderung**
/// (der Gast bekommt die native ABI zurück), bei einem Fault ist es eine **Herabstufung** (der
/// Kernel tötet den Thread). Fail-closed heisst deshalb nicht in beiden Fällen dasselbe.
///
/// | Fault-Bindung | Handler lebt | → | warum |
/// |---|---|---|---|
/// | keine | — | [`Weiche::Kernel`] | der native Fault-Pfad gewährt nichts; er beendet |
/// | ja | ja | [`Weiche::Handler`] | |
/// | ja | **nein** | [`Weiche::Fault`] | der Gast stirbt **benannt** statt als gewöhnlicher Fault — „dein Kernel ist weg" und „du hast Mist gebaut" sind zwei Diagnosen |
#[inline]
pub fn weiche_fault(bindung: Option<Bindung>, handler_lebt: bool) -> Weiche {
    match bindung {
        None => Weiche::Kernel,
        Some(b) if !b.hat_fault() => Weiche::Kernel,
        Some(_) if !handler_lebt => Weiche::Fault(ERR_HANDLER_GONE),
        Some(b) => Weiche::Handler {
            ep: b.fault_ep,
            slot: b.slot,
            sidecar: b.sidecar,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Das Zyklusverbot — BAUPFLICHT, nicht Doku (Z26, Nachtrag 3)
// ---------------------------------------------------------------------------------------

/// Urteil über eine gewünschte Bindung. **Jede Absage hat einen eigenen Namen** — eine stille
/// Absage wäre von einem Erfolg nicht zu unterscheiden, und bei einer Bindung heisst das: der
/// Gast läuft weiter mit der nativen ABI, während der Aufrufer glaubt, er sei umgeleitet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindUrteil {
    /// Zulässig.
    Ok,
    /// **A behandelt B behandelt A.** Der Handler (oder einer weiter oben in seiner Kette) wird
    /// selbst von der Gast-PD behandelt — ein Deadlock per Konstruktion: der Handler kann seinen
    /// eigenen `RECV` nicht absetzen, ohne dass der umgeleitet wird.
    Zyklus,
    /// Die Gast-PD ist ihr **eigener** Handler. Sonderfall von [`Self::Zyklus`] mit Kettenlänge 0,
    /// eigener Name, weil er die häufigste Tippfehler-Form ist und im Audit unterscheidbar sein
    /// soll.
    SelbstBindung,
    /// Die Gast-PD hat bereits einen **anderen** Handler. Eine PD hat höchstens einen Kernel;
    /// zwei Persönlichkeiten über einem Adressraum wären zwei Wahrheiten über denselben Speicher.
    /// (Dieselbe Bindung noch einmal ist [`Self::Ok`] — idempotent.)
    FremderHandler,
    /// Die Kette ist länger als es PDs gibt. Das heisst: es gibt **bereits** einen Zyklus, an dem
    /// die Gast-PD nicht beteiligt ist — also einen Kernelfehler, keinen Aufruferfehler.
    /// Fail-closed abgewiesen, aber **getrennt zählbar**: nach aussen derselbe ABI-Code, im Audit
    /// eine andere Zeile. Wer beides zusammenwirft, verliert genau den Fall, der einen Fehler im
    /// Kernel meldet.
    KetteZuLang,
    /// Kein freier Sidecar-Slot im Fenster des Handlers.
    KeinSlot,
    /// Die gewünschte Bindung wäre **wirkungslos** (weder Syscall- noch Fault-Handler). Sie
    /// entstünde als Kante im Graphen ohne Wirkung — eine Absage, die nach Erfolg aussieht.
    Wirkungslos,
}

/// **Prüft die gewünschte Kante `gast -> handler` im Handler-Graphen.**
///
/// Der Graph ist **funktional** — jede PD hat höchstens eine ausgehende Kante —, und das ist eine
/// Entwurfsentscheidung, keine Vereinfachung: eine PD ist EIN Adressraum und hat höchstens EINEN
/// Kernel. Aus der Funktionalität folgt, dass die Zyklusprüfung ein **Gang** ist und keine Suche:
/// kein Hilfsspeicher, keine Besuchsmarken, O(Kettenlänge) — auf dem Syscall-Pfad tragbar.
///
/// `kante(pd)` liefert die ausgehende Kante, `n_knoten` die Knotenzahl (die Schrittschranke).
/// **Bewusst eine Funktion und kein Array:** die PD-Tabelle hat `NPDS = 10 000` Einträge; sie in
/// einen Puffer zu kopieren, um darin zu laufen, wäre 20 KiB Stack je `SETHANDLER` — und ein
/// zweites Abbild einer Wahrheit, die schon existiert.
pub fn pruefe_bindung(
    gast_pd: u16,
    handler_pd: u16,
    hat_syscall: bool,
    hat_fault: bool,
    kante: impl Fn(u16) -> Option<u16>,
    n_knoten: usize,
    freie_slots: u16,
) -> BindUrteil {
    if !hat_syscall && !hat_fault {
        return BindUrteil::Wirkungslos;
    }
    if gast_pd == handler_pd {
        return BindUrteil::SelbstBindung;
    }
    // Schon gebunden? Dieselbe Kante noch einmal ist zulässig (idempotent, z. B. wenn ein zweiter
    // Thread derselben PD gebunden wird); eine ANDERE ist es nicht.
    if let Some(vorhanden) = kante(gast_pd) {
        if vorhanden != handler_pd {
            return BindUrteil::FremderHandler;
        }
    }
    if freie_slots == 0 {
        return BindUrteil::KeinSlot;
    }
    // Der Gang: vom Handler aus die Kette hinauf. Erreicht sie den Gast, wäre der Kreis
    // geschlossen. Die Schranke ist die Knotenzahl -- mehr Schritte als Knoten heisst, dass wir
    // schon in einem Kreis laufen, den es vor diesem Aufruf gab.
    let mut k = handler_pd;
    let mut schritte = 0usize;
    loop {
        if k == gast_pd {
            return BindUrteil::Zyklus;
        }
        let Some(naechster) = kante(k) else {
            return BindUrteil::Ok; // Kettenende erreicht, ohne den Gast zu treffen
        };
        k = naechster;
        schritte += 1;
        if schritte > n_knoten {
            return BindUrteil::KetteZuLang;
        }
    }
}

/// Länge der Handler-Kette ab `pd` — reine Auskunft für den Bericht (**nicht** für
/// Entscheidungen). `None` heisst „läuft im Kreis"; genau diese Unterscheidung fehlt einer Zahl,
/// die im Kreisfall einfach die Schranke zurückgibt.
pub fn kettenlaenge(pd: u16, kante: impl Fn(u16) -> Option<u16>, n_knoten: usize) -> Option<usize> {
    let mut k = pd;
    let mut n = 0usize;
    while let Some(naechster) = kante(k) {
        k = naechster;
        n += 1;
        if n > n_knoten {
            return None;
        }
    }
    Some(n)
}

// ---------------------------------------------------------------------------------------
// Tests — die Fallen mit Literalen, ohne Maschine
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn b(sys: u32, fault: u32, hpd: u16, slot: u16) -> Bindung {
        Bindung {
            sys_ep: sys,
            fault_ep: fault,
            handler_pd: hpd,
            slot,
            sidecar: 0x20_0000,
        }
    }

    /// Wortzahlen der beiden echten Trap-Frames. Sie stehen hier als Literale, weil diese Datei
    /// abhaengigkeitsfrei ist; `abi_codes_stimmen_ueberein` im Kernel haelt sie gegen die HAL.
    const X86_GPR: usize = 15;
    const X86_GESAMT: usize = 22;
    const ARM_GPR: usize = 31;
    const ARM_GESAMT: usize = 34;

    // --- Sidecar ---------------------------------------------------------------------------

    #[test]
    fn slot_fasst_beide_frames() {
        // Die Behauptung „512 reicht" ist hier PRUEFBAR, nicht bloss im Kommentar. x86_64:
        // 22 u64 = 176; aarch64: 34 u64 = 272 -- die Zahlen stehen in den beiden HAL-Dateien.
        assert!(FRAME_MAX_BYTES <= SLOT_BYTES);
        assert_eq!(22 * 8, 176);
        assert_eq!(34 * 8, FRAME_MAX_BYTES);
        // Zweierpotenz: der Offset ist eine Schiebung, keine Multiplikation.
        assert_eq!(SLOT_BYTES & (SLOT_BYTES - 1), 0);
    }

    #[test]
    fn nachricht_kann_den_frame_nicht_tragen() {
        // Der gemessene Grund fuer das Sidecar: vier Nachrichtenwoerter gegen 22 bzw. 34.
        // `caprock_abi::MSG_WORDS` ist 4; die Zahl steht hier als Literal, weil diese Datei
        // abhaengigkeitsfrei ist -- `abi_codes_stimmen_ueberein` im Kernel haelt sie zusammen.
        let msg_words = 4usize;
        assert!(msg_words * 8 < 22 * 8, "x86-Frame passt nicht in eine Nachricht");
        assert!(msg_words * 8 < FRAME_MAX_BYTES, "aarch64-Frame erst recht nicht");
    }

    #[test]
    fn slot_offsets_ueberlappen_nicht() {
        for s in 0u16..64 {
            let a = slot_offset(s);
            let z = slot_offset(s + 1);
            assert_eq!(z - a, SLOT_BYTES);
            assert!(a + FRAME_MAX_BYTES <= z, "Slot {s} ragt in den naechsten");
        }
    }

    #[test]
    fn slot_schranke_ist_exklusiv() {
        // Der Off-by-one, gegen den die Funktion steht: Slot `slots` liegt HINTER dem Fenster.
        assert!(slot_gueltig(0, 1));
        assert!(!slot_gueltig(1, 1));
        assert!(slot_gueltig(7, 8));
        assert!(!slot_gueltig(8, 8));
        assert!(!slot_gueltig(0, 0), "ein Fenster ohne Plaetze hat keinen gueltigen Slot");
    }

    #[test]
    fn fenster_deckt_den_letzten_slot_ganz() {
        // 600 Bytes: der zweite Slot FAENGT bei 512 an, endet aber bei 1024 -- er passt nicht.
        // Wer nur den Anfang prueft, gibt hier zwei Plaetze frei und laesst einen Gast ueber die
        // Fensterkante schreiben.
        assert!(fenster_deckt(1, 600));
        assert!(!fenster_deckt(2, 600));
        assert!(fenster_deckt(2, 1024));
        assert_eq!(slots_in(600), 1);
        assert_eq!(slots_in(1024), 2);
        assert_eq!(slots_in(4096), 8);
        assert_eq!(slots_in(0), 0);
    }

    // --- Das Slot-Format (die Nutzlast) ------------------------------------------------------

    /// Ein gültiger Kopf für die Erwartungen `(arch, n_gpr, n_ges)`.
    fn kopf(arch: u64, n_gpr: u64, n_ges: u64) -> [u64; SLOT_WOERTER] {
        let mut s = [0u64; SLOT_WOERTER];
        s[KOPF_MAGIE] = MAGIE;
        s[KOPF_VERSION] = FORMAT_VERSION;
        s[KOPF_GEN] = 1;
        s[KOPF_NGPR] = n_gpr;
        s[KOPF_NGESAMT] = n_ges;
        s[KOPF_ARCH] = arch;
        s
    }

    #[test]
    fn gueltiger_kopf_wird_angenommen() {
        let s = kopf(ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64);
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::Ok
        );
        let a = kopf(ARCH_AARCH64, ARM_GPR as u64, ARM_GESAMT as u64);
        assert_eq!(
            kopf_pruefen(&a, ARCH_AARCH64, ARM_GPR as u64, ARM_GESAMT as u64),
            KopfUrteil::Ok
        );
    }

    #[test]
    fn fremde_version_wird_benannt_abgewiesen() {
        // **Die Aussage von A-4.3, hier noch einmal.** Ein Leser, der bei unbekannter Version
        // weiterliest, legt fremde Bytes im eigenen Sinn aus -- und beim Sidecar sind das
        // Registerinhalte, die der Kernel in einen laufenden Thread schreibt.
        let mut s = kopf(ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64);
        s[KOPF_VERSION] = FORMAT_VERSION + 1;
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::FremdeVersion(FORMAT_VERSION + 1)
        );
        // Und die Absage ist NICHT dieselbe wie „hier steht nichts": die beiden brauchen
        // verschiedene Antworten (warten gegen Fassungen abgleichen).
        assert_ne!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::KeinFrame
        );
    }

    #[test]
    fn die_version_wird_vor_allem_anderen_geprueft() {
        // Bei fremder Version bedeutet auch „Wort 7 ist die Architektur" nichts mehr. Ein Pruefer,
        // der zuerst die Architektur liest, meldete `FremdeArchitektur` fuer einen Kopf, dessen
        // eigentliches Problem die Fassung ist -- eine Diagnose, die in die falsche Richtung zeigt.
        let mut s = kopf(ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64);
        s[KOPF_VERSION] = 99;
        s[KOPF_ARCH] = 42;
        s[KOPF_NGPR] = 7;
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::FremdeVersion(99)
        );
    }

    #[test]
    fn fremde_architektur_und_breite_haben_eigene_namen() {
        let mut s = kopf(ARCH_AARCH64, ARM_GPR as u64, ARM_GESAMT as u64);
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::FremdeArchitektur(ARCH_AARCH64)
        );
        s[KOPF_ARCH] = ARCH_X86_64;
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::FremdeBreite(ARM_GPR as u64, ARM_GESAMT as u64)
        );
    }

    #[test]
    fn eine_gruppe_wird_abgewiesen_und_nicht_ignoriert() {
        // **Nullen heissen „keine Angabe", nicht „passt auf alles".** Der Gruppenmechanismus ist
        // nicht gebaut; ein Slot, der eine Gruppe verlangt, verlangt etwas, das niemand einloest.
        // Ihn durchzulassen hiesse, ungeprueft Werte anzusammeln -- und der Tag der Einloesung
        // waere der Tag, an dem sie alle falsch sind (Z11c, die Prioritaeten im Test-Manifest).
        let mut s = kopf(ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64);
        s[KOPF_GRUPPE] = 3;
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::Reserviert
        );
    }

    #[test]
    fn reservierte_woerter_muessen_null_sein() {
        for i in 0..KOPF_RESERVIERT_N {
            let mut s = kopf(ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64);
            s[KOPF_RESERVIERT + i] = 1;
            assert_eq!(
                kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
                KopfUrteil::Reserviert,
                "reserviertes Wort {i} wird nicht geprueft"
            );
        }
    }

    #[test]
    fn ein_leerer_slot_ist_kein_frame() {
        let s = [0u64; SLOT_WOERTER];
        assert_eq!(
            kopf_pruefen(&s, ARCH_X86_64, X86_GPR as u64, X86_GESAMT as u64),
            KopfUrteil::KeinFrame
        );
        // Und ein zu kurzes Feld ebenso -- ohne diese Zeile liefe die Pruefung ueber die Kante.
        assert_eq!(
            kopf_pruefen(&s[..FRAME_WORT - 1], ARCH_X86_64, 0, 0),
            KopfUrteil::KeinFrame
        );
    }

    #[test]
    fn jede_kopf_absage_hat_einen_eigenen_namen() {
        let alle = [
            KopfUrteil::Ok,
            KopfUrteil::KeinFrame,
            KopfUrteil::FremdeVersion(2),
            KopfUrteil::FremdeArchitektur(1),
            KopfUrteil::FremdeBreite(1, 2),
            KopfUrteil::Reserviert,
        ];
        for i in 0..alle.len() {
            for j in (i + 1)..alle.len() {
                assert_ne!(alle[i], alle[j]);
            }
        }
    }

    #[test]
    fn kopf_und_frame_ueberlappen_nicht() {
        // Jedes Kopffeld liegt VOR dem Frame, und die ABI-Tabelle passt zwischen beide. Ohne
        // diese Zeile waere ein hinzugefuegtes Kopffeld ein Feld, das den ersten Frame-Wort
        // ueberschreibt -- und das saehe nach einem kaputten Gastregister aus, nicht nach einem
        // Formatfehler.
        for k in [
            KOPF_MAGIE,
            KOPF_VERSION,
            KOPF_GEN,
            KOPF_ANLASS,
            KOPF_CODE,
            KOPF_NGPR,
            KOPF_NGESAMT,
            KOPF_ARCH,
            KOPF_GRUPPE,
        ] {
            assert!(k < KOPF_ABI, "Kopffeld {k} liegt in der ABI-Tabelle");
        }
        assert!(KOPF_RESERVIERT + KOPF_RESERVIERT_N <= KOPF_ABI, "reservierter Block ragt in die ABI-Tabelle");
        assert!(KOPF_ABI + KOPF_ABI_N <= FRAME_WORT, "ABI-Tabelle ragt in den Frame");
        assert_eq!(frame_wort(0), FRAME_WORT);
        assert_eq!(SLOT_WOERTER, 64);
        assert_eq!(FRAME_WOERTER_MAX, 40);
    }

    #[test]
    fn slot_traegt_beide_frames_mit_kopf() {
        // Die Behauptung „512 reicht" gilt erst MIT dem Kopf. `slot_fasst_beide_frames` prueft
        // 272 <= 512 -- das ist die Aussage ohne Kopf und war bis zum 2026-08-13 die ganze
        // Pruefung. Ein Kopf von 16 Woertern haette sie nicht gestoert und trotzdem den Frame
        // ueber die Slotkante geschoben.
        assert!(frame_passt(X86_GESAMT));
        assert!(frame_passt(ARM_GESAMT));
        assert!(frame_wort(ARM_GESAMT) <= SLOT_WOERTER);
        // Und die Kante ist scharf: ein Wort mehr als Platz ist, passt nicht.
        assert!(frame_passt(FRAME_WOERTER_MAX));
        assert!(!frame_passt(FRAME_WOERTER_MAX + 1));
    }

    #[test]
    fn nur_die_allzweckregister_kommen_zurueck() {
        // **Die Aussage, die diesen Handler von einem Kernel unterscheidet.** Lesen darf er den
        // ganzen Frame; zurueckschreiben nur die GPR. Die Woerter dahinter tragen den RING
        // (`cs`/`ss`, `spsr`) -- wer sie schreiben darf, befoerdert seinen Gast.
        for i in 0..X86_GPR {
            assert!(uebernehmbar(i, X86_GPR), "x86-GPR {i} muss zurueck duerfen");
        }
        for i in X86_GPR..X86_GESAMT {
            assert!(
                !uebernehmbar(i, X86_GPR),
                "x86-Wort {i} (vector/error/rip/cs/rflags/rsp/ss) darf NICHT zurueck"
            );
        }
        for i in 0..ARM_GPR {
            assert!(uebernehmbar(i, ARM_GPR));
        }
        for i in ARM_GPR..ARM_GESAMT {
            assert!(
                !uebernehmbar(i, ARM_GPR),
                "aarch64-Wort {i} (elr/spsr/sp_el0) darf NICHT zurueck"
            );
        }
    }

    #[test]
    fn das_ring_wort_ist_namentlich_gesperrt() {
        // Dieselbe Aussage noch einmal, aber an der Stelle, um die es geht -- nicht ueber einen
        // Bereich, sondern ueber DAS Wort. Ein Bereichstest bleibt gruen, wenn jemand die
        // Reihenfolge der Frame-Felder aendert; dieser faellt.
        //
        // x86_64-Framefolge: gpr[0..15] · vector · error · rip · cs · rflags · rsp · ss
        let (cs, ss, rflags) = (15 + 3, 15 + 6, 15 + 4);
        assert!(!uebernehmbar(cs, X86_GPR), "cs traegt den Ring");
        assert!(!uebernehmbar(ss, X86_GPR), "ss traegt den Ring");
        assert!(!uebernehmbar(rflags, X86_GPR), "rflags traegt IOPL und IF");
        // aarch64-Framefolge: gpr[0..31] · elr · spsr · sp_el0
        assert!(!uebernehmbar(31 + 1, ARM_GPR), "spsr traegt das Exception-Level");
    }

    #[test]
    fn die_magie_ist_kein_gueltiger_registerwert_aus_versehen() {
        // Sie muss von 0 verschieden sein -- sonst waere „nichts geschrieben" von „geschrieben"
        // nicht zu unterscheiden, und genau dafuer ist sie da.
        assert_ne!(MAGIE, 0);
        assert_ne!(MAGIE, u64::MAX);
        assert_ne!(ARCH_X86_64, ARCH_AARCH64);
    }

    // --- Die Weiche ------------------------------------------------------------------------

    #[test]
    fn ohne_bindung_bearbeitet_der_kernel() {
        assert_eq!(weiche_syscall(None, true), Weiche::Kernel);
        assert_eq!(weiche_syscall(None, false), Weiche::Kernel);
        assert_eq!(weiche_fault(None, true), Weiche::Kernel);
    }

    #[test]
    fn mit_lebendem_handler_wird_umgeleitet() {
        let x = b(7, 9, 3, 2);
        let w = |ep, slot| Weiche::Handler { ep, slot, sidecar: 0x20_0000 };
        assert_eq!(weiche_syscall(Some(x), true), w(7, 2));
        assert_eq!(weiche_fault(Some(x), true), w(9, 2));
        // Das FENSTER wandert mit -- eine Weiche, die den Slot nennt und das Fenster nicht,
        // liesse den Zusteller die Basis ein zweites Mal aufloesen.
        assert!(matches!(weiche_syscall(Some(x), true), Weiche::Handler { sidecar, .. } if sidecar == x.sidecar));
    }

    #[test]
    fn weggefallener_handler_faultet_und_faellt_nicht_zurueck() {
        // **Die Regel, ohne die es eine Rechteausweitung waere.** Aus dem Entzug einer Cap darf
        // keine Befoerderung werden.
        let x = b(7, 9, 3, 2);
        assert_eq!(weiche_syscall(Some(x), false), Weiche::Fault(ERR_HANDLER_GONE));
        assert_eq!(weiche_fault(Some(x), false), Weiche::Fault(ERR_HANDLER_GONE));
    }

    #[test]
    fn bindung_vorhanden_heisst_niemals_kernel() {
        // Die Aussage ueber das GANZE Kreuzprodukt, nicht ueber drei Beispiele. Sie ist die
        // eigentliche Zusicherung des Primitivs: wer gebunden ist, erreicht den Caprock-Kernel
        // nicht mehr -- gleich, was mit dem Handler passiert ist.
        for &lebt in &[true, false] {
            for &sys in &[0u32, 1, 4242] {
                for &flt in &[0u32, 1, 4242] {
                    let x = b(sys, flt, 3, 0);
                    assert_ne!(
                        weiche_syscall(Some(x), lebt),
                        Weiche::Kernel,
                        "sys={sys} fault={flt} lebt={lebt}"
                    );
                    assert_ne!(weiche_fault(Some(x), lebt), Weiche::Kernel);
                }
            }
        }
    }

    #[test]
    fn halbe_bindung_leitet_nur_ihre_haelfte_um() {
        // Nur Syscall-Handler: Faults bleiben beim Kernel (der Gast stirbt nativ) -- und das ist
        // KEINE Ausweitung, denn der native Fault-Pfad gewaehrt nichts.
        let nur_sys = b(7, KEIN_EP, 3, 0);
        let w = |ep, slot| Weiche::Handler { ep, slot, sidecar: 0x20_0000 };
        assert_eq!(weiche_syscall(Some(nur_sys), true), w(7, 0));
        assert_eq!(weiche_fault(Some(nur_sys), true), Weiche::Kernel);
        assert_eq!(weiche_fault(Some(nur_sys), false), Weiche::Kernel);
        // Nur Fault-Handler: Syscalls bleiben nativ. Das ist die Form, in der ein Debugger oder
        // ein Speicherserver arbeitet, ohne Persoenlichkeit zu sein.
        let nur_flt = b(KEIN_EP, 9, 3, 0);
        assert_eq!(weiche_syscall(Some(nur_flt), true), Weiche::Kernel);
        assert_eq!(weiche_fault(Some(nur_flt), true), w(9, 0));
    }

    #[test]
    fn leere_bindung_ist_erkennbar() {
        assert!(b(KEIN_EP, KEIN_EP, 3, 0).ist_leer());
        assert!(!b(0, KEIN_EP, 3, 0).ist_leer());
        assert!(!b(KEIN_EP, 0, 3, 0).ist_leer());
        // Endpoint 0 ist ein GUELTIGER Endpoint. Wer `0` als „keiner" nimmt, verliert ihn.
        assert!(b(0, 0, 3, 0).hat_syscall());
    }

    // --- Das Zyklusverbot ------------------------------------------------------------------

    /// Kantenmenge bauen: `paare` = (pd, handler_pd).
    fn g(n: usize, paare: &[(u16, u16)]) -> Vec<Option<u16>> {
        let mut k = vec![None; n];
        for &(a, h) in paare {
            k[a as usize] = Some(h);
        }
        k
    }

    /// Aus einer Kantentabelle die Zugriffsfunktion machen. Eine PD-Id ausserhalb der Tabelle
    /// liefert `None` -- genau das Verhalten, das der Kernel mit `handler_pd_of` hat.
    fn f(k: &[Option<u16>]) -> impl Fn(u16) -> Option<u16> + '_ {
        move |p: u16| k.get(p as usize).copied().flatten()
    }

    /// Kurzform: `pruefe_bindung` gegen eine Kantentabelle.
    fn pb(gast: u16, h: u16, sys: bool, flt: bool, k: &[Option<u16>], frei: u16) -> BindUrteil {
        pruefe_bindung(gast, h, sys, flt, f(k), k.len(), frei)
    }

    #[test]
    fn frische_bindung_ist_zulaessig() {
        let k = g(8, &[]);
        assert_eq!(pb(1, 2, true, false, &k, 4), BindUrteil::Ok);
    }

    #[test]
    fn selbstbindung_wird_abgewiesen() {
        let k = g(8, &[]);
        assert_eq!(pb(1, 1, true, true, &k, 4), BindUrteil::SelbstBindung);
    }

    #[test]
    fn zweierzyklus_wird_abgewiesen() {
        // A behandelt B; jetzt soll B auch A behandeln -> der Fall, den Nachtrag 3 nennt.
        let k = g(8, &[(2, 1)]); // PD 2 wird von PD 1 behandelt
        assert_eq!(pb(1, 2, true, false, &k, 4), BindUrteil::Zyklus);
    }

    #[test]
    fn laengerer_zyklus_wird_abgewiesen() {
        // 3 -> 2 -> 1 besteht; 1 -> 3 schloesse den Kreis. Eine Pruefung, die nur den DIREKTEN
        // Partner ansieht, laesst das durch -- und dann haengt die ganze Kette beim ersten
        // Syscall.
        let k = g(8, &[(3, 2), (2, 1)]);
        assert_eq!(pb(1, 3, true, false, &k, 4), BindUrteil::Zyklus);
    }

    #[test]
    fn gestapelte_persoenlichkeiten_bleiben_erlaubt() {
        // Die BILLIGE Absage aus Z26 („Threads einer PD mit Handler-Bindung duerfen selbst nicht
        // gebunden werden") verboete das hier -- 3 -> 2 -> 1 ist azyklisch und legitim: eine
        // Persoenlichkeit ueber einer Persoenlichkeit. Gebaut ist deshalb die allgemeine
        // Azyklizitaet, nicht die billige Form.
        let k = g(8, &[(2, 1)]);
        assert_eq!(pb(3, 2, true, false, &k, 4), BindUrteil::Ok);
        let k2 = g(8, &[(3, 2), (2, 1)]);
        assert_eq!(kettenlaenge(3, f(&k2), k2.len()), Some(2));
    }

    #[test]
    fn zweiter_handler_fuer_dieselbe_pd_wird_abgewiesen() {
        let k = g(8, &[(1, 2)]);
        assert_eq!(pb(1, 4, true, false, &k, 4), BindUrteil::FremderHandler);
        // ... dieselbe noch einmal ist idempotent (zweiter Thread derselben Gast-PD).
        assert_eq!(pb(1, 2, true, false, &k, 4), BindUrteil::Ok);
    }

    #[test]
    fn vorbestehender_zyklus_ist_unterscheidbar() {
        // 5 -> 6 -> 5 ist ein Kreis, an dem der Gast (1) nicht beteiligt ist. Der Gang laeuft in
        // ihn hinein und trifft den Gast nie. Ohne die Schrittschranke: Endlosschleife IM
        // SYSCALL-PFAD. Mit ihr: eine eigene, zaehlbare Meldung -- ein KERNELfehler, kein
        // Aufruferfehler, und die beiden duerfen nicht denselben Namen tragen.
        let k = g(8, &[(5, 6), (6, 5)]);
        assert_eq!(pb(1, 5, true, false, &k, 4), BindUrteil::KetteZuLang);
        assert_eq!(kettenlaenge(5, f(&k), k.len()), None);
    }

    #[test]
    fn wirkungslose_bindung_wird_abgewiesen() {
        // Weder Syscall- noch Fault-Handler: eine Kante ohne Wirkung. Sie durchzulassen hiesse,
        // eine Absage als Erfolg zu melden -- der Aufrufer glaubte, der Gast sei umgeleitet.
        let k = g(8, &[]);
        assert_eq!(pb(1, 2, false, false, &k, 4), BindUrteil::Wirkungslos);
    }

    #[test]
    fn ohne_slot_keine_bindung() {
        let k = g(8, &[]);
        assert_eq!(pb(1, 2, true, false, &k, 0), BindUrteil::KeinSlot);
    }

    #[test]
    fn unbekannte_pd_laeuft_nicht_ins_leere() {
        // `kanten.get` statt `kanten[..]`: eine PD-Id ausserhalb der Tabelle darf nicht panicken.
        // Sie kommt aus einer cap-geprueften Quelle -- aber „kommt aus einer geprueften Quelle"
        // ist genau die Annahme, die in diesem Projekt schon dreimal falsch war.
        let k = g(4, &[]);
        assert_eq!(pb(1, 99, true, false, &k, 4), BindUrteil::Ok);
        assert_eq!(pb(99, 1, true, false, &k, 4), BindUrteil::Ok);
        assert_eq!(kettenlaenge(99, f(&k), k.len()), Some(0));
    }

    #[test]
    fn jede_absage_hat_einen_eigenen_namen() {
        // Die Aussage, die den Bericht traegt: sechs unterscheidbare Absagen, keine zwei gleich.
        // Waeren zwei davon derselbe Wert, koennte der Audit sie nicht trennen -- und genau die
        // Trennung „Kernelfehler gegen Aufruferfehler" haengt daran.
        let alle = [
            BindUrteil::Ok,
            BindUrteil::Zyklus,
            BindUrteil::SelbstBindung,
            BindUrteil::FremderHandler,
            BindUrteil::KetteZuLang,
            BindUrteil::KeinSlot,
            BindUrteil::Wirkungslos,
        ];
        for i in 0..alle.len() {
            for j in (i + 1)..alle.len() {
                assert_ne!(alle[i], alle[j]);
            }
        }
    }
}
