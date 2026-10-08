#![no_std]
//! Microkit-Runtime im Kernelimage (ADR 0006): **Protection Domains** und
//! **cap-gesicherte IPC**.
//!
//! Eine Protection Domain (PD) ist die Schutz- und Autoritätseinheit: ein Thread
//! plus ein eigener **Capability-Space** — eine kleine Tabelle, die *lokale*
//! Cap-Indizes auf *globale* [`CapPtr`]s (in den einen `CapSpace`, ADR 0003)
//! abbildet. Ein Thread kann nur Endpoints invozieren, für die seine PD eine
//! Capability hält, und nur mit den Rechten dieser Capability — das schließt die
//! Lücke aus Phase 5 (IPC war per ID adressiert, ungated).
//!
//! Es gibt **kein** zweites Capability-System: die Caps leben im globalen
//! `CapSpace` (mit Derivation-Tree); der PD-Cspace ist nur die Indirektion, die
//! Sichtbarkeit/Zugriff pro PD festlegt. Eine über den CDT widerrufene Cap wird
//! im PD-Cspace automatisch ungültig (stale `CapPtr`).
//!
//! Reine Orchestrierung — **kein eigenes `unsafe`**.

use caprock_abi::{pdctl, reg, result, sys};
use caprock_cap::{CapError, CapPtr, CapSpace, DmaCoherence, DmaDir, ObjectKind};
use caprock_hal::cpu::array_index_nospec;
use caprock_hal::exception::{frame_reg, frame_set_reg};
use caprock_ipc::{Endpoint, Notification};
use caprock_mem::Rights;
use caprock_sched::{redirect, SchedOps, ThreadId};
use caprock_slab::Slab;
use caprock_sync::{RwSpinLock, SpinLock};

/// Variabler PD-Cspace: Vergabe, Absagen, Freigabe (todo.md A3, TODO0 K1c).
///
/// Abhaengigkeitsfrei (`core` nur) und per `rustc --test` als Datei pruefbar — die
/// Buchhaltung steht genau einmal dort, `PdTable` ruft sie auf, statt sie nachzubauen.
mod cspace;
pub use cspace::{CspaceAbweisung, STANDARD_PLAETZE};
use cspace::{CspaceAnker, Vergabe};

/// irq+NTFN je Vektor (Multi-Vektor-MSI-X): die reine Paar-Buchhaltung.
///
/// Abhaengigkeitsfrei (`core` nur) und per `rustc --test` als Datei pruefbar — die
/// Entscheidung steht genau einmal dort, der Kernel (Patch-Text) ruft sie auf, statt sie
/// nachzubauen (dieselbe Teilung wie `cspace.rs` und `proc.rs`).
mod irqvec;
pub use irqvec::{
    paare_pruefen, plaetze_fuer_vektoren, CapRolle, SlotSicht, VektorAbweisung, VektorPaar,
    VEKTOREN_JE_GERAET_MAX,
};

/// Prozessmodell: FORK/EXEC-Dispatchhelfer (Phase 1, volle Kopie, kein COW).
///
/// Abhaengigkeitsfrei (`core` nur), als Datei pruefbar (`rustc --test .../proc.rs`).
/// Die Entscheidung steht genau einmal dort; der FORK/EXEC-Zweig in
/// `dispatch_nativ` ruft sie auf, statt sie nachzubauen.
pub mod proc;

/// Größe des PD-Pools. **Öffentlich seit A-3.4**: wie viele Cap-Slots das System vorhalten muss,
/// folgt aus `NPDS * CAP_BUDGET_PER_PD` — diese Rechnung gehört an *eine* Stelle
/// ([`CAP_SLOTS_FOR_ALL_PDS`]) und nicht als abgeschriebene Zahl in den Boot-Code.
/// **Ziel-Zahl gleichzeitiger Protection Domains** (A-3.4, Teil 3).
///
/// War 256 und lag als `[Pd; NPDS]` im `.bss`. Damit waren 10000 Threads (`TARGET_THREADS`) zwar
/// darstellbar, aber nur solange sie sich Adressräume **teilen** — als 10000 isolierte Tenants
/// nicht, und genau das ist der Punkt des Systems. Die Tabelle wird jetzt beim Boot alloziert
/// (`PdTable::attach`), die Zahl kostet also RAM, keine Struktur.
///
/// Auf `TARGET_THREADS` abgestimmt: im Grenzfall bekommt jeder Thread seine eigene PD. Was das
/// kostet, meldet der Boot-Report — die Entscheidung hängt an einer gemessenen Größe.
pub const NPDS: usize = 10_000;
/// Cap-Slots je PD-Cspace — die **Vorgabe**, keine Schranke mehr.
///
/// Bis TODO0 K1c war das die harte Array-Laenge (`Pd::cspace: [..; NCAPS]`): eine
/// Treiber-PD mit dreissig Caps passte nicht, gleich was das Budget erlaubte. Jetzt ist
/// es die Standardgroesse (`plaetze == 0` heisst „16"); groessere Laeufe vergibt
/// [`PdTable`] aus dem beim Boot angehaengten Cspace-Pool
/// ([`PdTable::attach_cspace_pool`], Vorbild A-3.4). Bestehende 16er-PDs verhalten sich
/// bitgleich zu frueher.
const NCAPS: usize = STANDARD_PLAETZE as usize;

/// **Cap-Budget je PD** (Fairness-/DoS-Schranke).
///
/// Der globale [`CapSpace`] ist eine **systemweit geteilte** Tabelle fester Größe. `NCAPS`
/// begrenzt nur den lokalen Index-Adressraum einer PD, nicht ihren Verbrauch an globalen
/// Slots: ohne zusätzliche Schranke könnten wenige PDs (`NPDS * NCAPS` ≫ Slots der globalen
/// Tabelle) die Tabelle füllen und damit **allen anderen** PDs jede weitere Cap-Installation
/// verweigern — ein Cross-PD-Denial-of-Service über eine geteilte Ressource.
///
/// Das Budget deckelt, wie viele Slots des eigenen Cspace eine PD gleichzeitig belegen darf.
/// Es ist bewusst deutlich kleiner als `NCAPS`: die vorhandenen PDs nutzen 1–4 Slots, und so
/// bleibt selbst bei vielen gleichzeitig geladenen PDs Tabellenkapazität für alle übrig.
/// Überschreitung wird **abgewiesen** (kein Eintrag, keine Ableitung), nie still verworfen.
///
/// **8 -> 10 am 2026-08-26 (Stufe B).** Eine Treiber-PD haelt seither neun Caps: das Angebot des
/// Aufrufers in Slot 0 und die acht des Manifests (Notification, Endpoint, Konfigurationsseite,
/// BAR, DMA, Uebertragungsflaeche, `Irq`, Interrupt-Notification). Mit 8 war die Zusage des Laders
/// **unerfuellbar** -- und die Absage kam nicht als „Budget", sondern als eine PD, die laedt und
/// deren Cap-Slot leer bleibt. *Ein Deckel oberhalb der Struktur ist kein grosszuegiger Deckel*;
/// hier war es einer **unterhalb**, und das ist dieselbe Klasse.
///
/// Die Relation wird seither vom Uebersetzer gehalten: `kernel::loader::LOAD_CAPS_MAX` traegt ein
/// `const assert` dagegen. Wer einen Slot hinzufuegt, bricht den Bau, statt eine PD lautlos halb
/// auszustatten.
pub const CAP_BUDGET_PER_PD: usize = 10;

/// **Obergrenze fuer ein einzelnes PD-Budget** (2026-08-26) — die **Vorgabe des
/// Deckels**, seit TODO0 K1c je Tabelle einstellbar ([`PdTable::set_budget_deckel`]).
///
/// Ohne Deckel koennte eine PD den Vorrat aller anderen aufbrauchen -- dieselbe Cross-PD-DoS-Form,
/// gegen die [`CAP_BUDGET_PER_PD`] ueberhaupt eingefuehrt wurde, nur eine Ebene hoeher. Der Deckel
/// steht **neben** dem Vorrat und ersetzt ihn nicht: er begrenzt den Einzelnen, der Vorrat die
/// Summe. Wer nur eines von beidem hat, hat keine Zusage.
///
/// ## Warum die Vorgabe `NCAPS` ist
///
/// Hier stand zuerst `64`. Der Selbsttest hat es am selben Tag widerlegt: der **lokale Cspace
/// einer PD war ein Array von [`NCAPS`] Slots** (`Pd::cspace`), und `install_cap` wies jeden Slot
/// `>= NCAPS` ab -- ohne dass das Budget je gefragt wuerde. Ein Budget von 20 war also kein
/// grosszuegiges Budget, sondern eine **unerreichbare Zusage**.
///
/// *Ein Kriterium, das die gepruefte Sache nicht erreichen KANN, ist kein strenges Kriterium,
/// sondern gar keins* -- und ein Deckel ueber der Struktur ist dieselbe Form.
///
/// Seit TODO0 K1c ist der Cspace variabel, und die Schranke ist mitgewachsen: statt des
/// entfernten `const assert!(CAP_BUDGET_MAX <= NCAPS)` prueft die Erzeugung jetzt
/// **dynamisch**, ob das Budget in die Plaetze passt
/// ([`CspaceAbweisung::BudgetPasstNicht`]). Ein Deckel ueber der Struktur waere wieder
/// eine Zusage, die die Struktur nicht halten kann — und sie fiele erst der PD auf, die
/// sie einloest. Fail-closed zur Laufzeit statt zur Bauzeit.
///
/// **Damit ist `NCAPS` nicht mehr der Blocker fuer eine Treiberumgebung**: wer dreissig
/// Caps braucht, fordert Budget 30 mit 32 Plaetzen an und hebt den Deckel der Tabelle
/// (`set_budget_deckel`) — die anderen zehntausend PDs kostet das nichts.
pub const CAP_BUDGET_MAX: usize = NCAPS;

// Die Vorgabe liegt unter dem Deckel — was der Standard anfordert, muss der Deckel
// hergeben, sonst waere jede Standard-Erzeugung eine benannte Absage.
const _: () = assert!(
    CAP_BUDGET_PER_PD <= CAP_BUDGET_MAX,
    "die Vorgabe liegt ueber dem Deckel"
);

/// **Wie viele globale Cap-Slots die Summe aller PD-Budgets braucht** (A-3.4).
///
/// Der Kommentar an [`CAP_BUDGET_PER_PD`] sagt, das Budget verhindere einen Cross-PD-DoS über die
/// geteilte Tabelle. Nachgerechnet stimmte das nicht: 256 × 8 = 2048 Slots Bedarf standen gegen
/// **256** vorhandene. Das Budget deckelte den Verbrauch **einer** PD, die Summe prüfte niemand —
/// 32 PDs mit vollem Budget füllten die Tabelle, die 33. bekam nichts. Die Zusage war also eine
/// Annahme über das Verhalten der PDs, keine Eigenschaft des Systems.
///
/// Seit A-3.4 dimensioniert der Kernel die Tabelle **hiernach**. Damit ist die Aussage „jede PD
/// bekommt ihr Budget" eine Eigenschaft des Aufbaus. Der Weg dahin war ausdrücklich *nicht*, die
/// PD-Zahl auf 32 zu senken: das hätte dieselbe Zusage gerettet, indem es das System kleiner macht.
pub const CAP_SLOTS_FOR_ALL_PDS: usize = NPDS * CAP_BUDGET_PER_PD;

/// **Slots für die Caps, die der Kernel selbst hält** (Wurzel-Caps auf Speicherregionen,
/// Endpoints/Notifications der Bringup-Kanäle, Loader-Cap des Root-Task, DMA-/MMIO-/IRQ-Caps der
/// Treiber).
///
/// Die Reserve steht **neben** der Summe aller PD-Budgets, nicht in ihr: eine PD, die ihr Budget
/// ausschöpft, soll dem Kernel keinen Slot wegnehmen können — und umgekehrt. Bis hierher war das
/// eine Zahl im Boot-Code mit einem Kommentar daneben; nachgezählt hat sie niemand. Womit sie
/// nachgezählt wird, steht an [`Caps::nonbudget_slots`].
pub const CAP_SLOTS_KERNEL_RESERVE: usize = 256;

/// **Gesamte Slot-Kapazität des globalen Cap-Space** — Summe aller PD-Budgets plus Kernel-Reserve.
///
/// Stand als `CAP_SLOTS_FOR_ALL_PDS + 256` im Boot-Code des Kernels. Dieselbe Begründung wie bei
/// [`CAP_SLOTS_FOR_ALL_PDS`]: die Rechnung gehört an *eine* Stelle, sonst läuft die abgeschriebene
/// Zahl beim nächsten Drehen an [`NPDS`] oder [`CAP_BUDGET_PER_PD`] still auseinander — und still
/// auseinandergelaufen heißt hier: eine PD innerhalb ihres Budgets bekommt `NoSlot`.
pub const CAP_SLOTS_TOTAL: usize = CAP_SLOTS_FOR_ALL_PDS + CAP_SLOTS_KERNEL_RESERVE;

/// **Wie viele Endpoint-Objekte alle PDs zusammen brauchen** (A-3.4 Teil 4).
///
/// Dieselbe Rechnung eine Ebene weiter, und derselbe Fund: `NENDPOINTS = 32` in `caprock-ipc`
/// stand gegen [`NPDS`] = 10 000. Eine PD, die als Server auftreten will, braucht **mindestens
/// einen** Endpoint; ab der 33. war keiner mehr zu haben. Die Zusage „10 000 Tenants" endete
/// damit an einer Zahl, die nie mitgewachsen ist — Threads (Teil 1), Caps (Teil 2) und
/// Adressräume (Teil 3) waren gedreht, die Kommunikation nicht. Ein Tenant ohne Endpoint ist
/// aber kein Tenant, sondern ein Prozess, mit dem niemand reden kann.
///
/// Ein Endpoint je PD ist die **untere** Schranke, nicht die bequeme: mehr Endpoints je PD
/// (eine PD mit mehreren Diensten) sind damit nicht gedeckt und müssten die Zahl erhöhen.
pub const ENDPOINTS_FOR_ALL_PDS: usize = NPDS;

/// **Wie viele Notification-Objekte alle PDs zusammen brauchen** (A-3.4 Teil 4).
///
/// Wie [`ENDPOINTS_FOR_ALL_PDS`]: eine asynchrone Signalquelle je PD als untere Schranke.
/// Notifications sind mit Abstand die billigsten Objekte (drei Felder, keine Warteschlange) —
/// hier zu sparen bringt nichts und kostet dieselbe Zusage.
pub const NOTIFICATIONS_FOR_ALL_PDS: usize = NPDS;

/// **Sicherheitsdomäne** einer Protection Domain (ext-22).
///
/// Drei kernel-getrennte Klassen mit erzwungenen Regeln (siehe `domain_audit`):
/// - [`Domain::TrustedSas`]: läuft im globalen Adressraum (VSPACE_OF==0), nur
///   speichersicheres Rust ohne `unsafe`. Treiber-/Protokolllogik. Default für alle
///   bestehenden PDs (Rückwärtskompatibilität).
/// - [`Domain::HardwareLand`]: isolierte VSpace, darf Hardware-Caps (MMIO/IRQ) halten,
///   kleiner `unsafe`-Hardwarekern. Genau **ein** unveränderlicher Trusted-Partner.
/// - [`Domain::UserLand`]: isolierte VSpace, **keinerlei** Hardware-Rechte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    TrustedSas,
    HardwareLand,
    UserLand,
}

/// Cap-Management-Zustand hinter **einem** Lock (`CAPS` im Kernel): der globale
/// Capability-Space + die Protection-Domain-Tabelle. Cap-Auflösung und -Verwaltung
/// brauchen beides konsistent zusammen.
pub struct Caps {
    pub cspace: CapSpace,
    pub pds: PdTable,
}

impl Default for Caps {
    fn default() -> Self {
        Self::new()
    }
}

impl Caps {
    pub const fn new() -> Self {
        Caps {
            cspace: CapSpace::new(),
            pds: PdTable::new(),
        }
    }

    /// Eine Cap **policy- und budgetgeprüft** in den Cspace einer PD eintragen: der Cap-Typ
    /// muss in der Domäne der PD erlaubt sein (Hardware-Caps nur in HardwareLand, `PdControl`
    /// nur in TrustedSas) UND die PD darf ihr [`CAP_BUDGET_PER_PD`] nicht überschreiten. Gibt
    /// `false` zurück (ohne Eintrag), wenn eines von beiden verletzt ist.
    /// Zentraler Enforcement-Punkt — alle Cap-Installationen sollten hierüber laufen.
    pub fn install_cap_checked(&mut self, pd: usize, slot: usize, cap: CapPtr) -> bool {
        if !self.cap_allowed(pd, cap) || !self.budget_allows(pd, slot) {
            return false;
        }
        self.pds.install_cap(pd, slot, cap);
        true
    }

    /// Darf PD `pd` den Slot `slot` (neu) belegen, ohne **ihr eigenes** Budget zu überschreiten?
    /// Ein **Überschreiben** eines bereits belegten Slots erhöht den Verbrauch nicht und ist immer
    /// erlaubt.
    ///
    /// **Seit 2026-08-26 die Zahl der PD, nicht die Konstante.** Sie ist bei der Erzeugung gebucht
    /// worden (s. [`PdTable::create_mit_budget`]) und steht mit [`CAP_BUDGET_PER_PD`] als Vorgabe
    /// da, wo niemand mehr wollte. Der Unterschied ist nicht Bequemlichkeit: eine Konstante
    /// anzuheben kostet den Bedarf **einer** PD mal `NPDS` — rund 7,7 MB je acht Slots.
    pub fn budget_allows(&self, pd: usize, slot: usize) -> bool {
        if self.pds.cap_at(pd, slot).is_some() {
            return true; // Ersetzen, kein zusätzlicher Slot
        }
        self.pds.cap_count(pd) < self.pds.budget_of(pd)
    }

    /// **Passt ein ganzes Endowment -- ohne etwas zu installieren?** (2026-08-25)
    ///
    /// `Ok(n)` = es passt, dabei wurden `n` **Angebote** abgelehnt. `Err(slot)` = eine **Zusage**
    /// haelt nicht, und zwar an diesem Slot.
    ///
    /// ## Warum die Frage hierher gehoert und nicht in den Ladepfad
    ///
    /// Weil die Budgetregel hier steht. Ein Vorablauf, der sie im Kernel nachbaut, ist die zweite
    /// Wirklichkeit aus der Fallenliste — „Zuteiler und Pruefer brauchen EINE Quelle". Und der
    /// Nachbau waere auch inhaltlich falsch geworden: [`budget_allows`](Self::budget_allows)
    /// vergleicht gegen den **aktuellen** Verbrauch, also saehe eine Schleife, die jeden Cap
    /// einzeln fragt, den Zuwachs der vorigen nicht — vier Caps waeren einzeln zulaessig und
    /// zusammen ueber der Schranke. Hier wird der Zuwachs deshalb **mitgezaehlt**.
    ///
    /// ## Angebot gegen Zusage
    ///
    /// `angebote` ist eine **Bitmaske der Ziel-Slots**, in denen vom Aufrufer **angebotene** Caps
    /// liegen (`SYS_LOAD`). Ein Angebot darf eine Zieldomaene ablehnen — `init` bietet jedem Kind
    /// seine Notification an und kann dessen Domaene gar nicht kennen. Eine Zusage des signierten
    /// Manifests darf sie nicht ablehnen: dann ist das Programm nicht das, als das es zugelassen
    /// wurde.
    ///
    /// **Eine Maske und nicht ein Slot** (2026-08-25, mit der Mehrfachdelegation): solange
    /// `SYS_LOAD` genau einen Cap uebergab, war „der angebotene Slot" eindeutig. Mit acht waere
    /// ein einzelner Wert die erste Delegation — und die uebrigen sieben zaehlten als **Zusagen
    /// des Manifests**, deren Ablehnung den Ladevorgang abweist. Ein Kardinalwert, wo eine Menge
    /// gemeint ist, ist genau die Falle, die dieses Projekt bei `IDENTITY_DEBTS` schon bezahlt hat.
    pub fn endowment_fits(
        &self,
        pd: usize,
        endow: &[(usize, CapPtr)],
        angebote: u16,
    ) -> Result<usize, usize> {
        let mut abgelehnte_angebote = 0usize;
        let mut neue_slots = 0usize;
        for &(slot, cap) in endow {
            // Jenseits des Laufs gibt es keinen Slot — auch nicht als Angebot: ein Angebot
            // braucht einen Ziel-Slot, und was nicht adressierbar ist, ist eine Zusage,
            // die nicht haelt (derselbe Fail-closed wie bei `install_cap`).
            if slot >= self.pds.cspace_len_von(pd) {
                return Err(slot);
            }
            // Ein belegter Slot wird ERSETZT und kostet kein Budget — dieselbe Regel wie in
            // `budget_allows`, und sie steht deshalb auch nur dort.
            let ist_neu = self.pds.cap_at(pd, slot).is_none();
            let passt = self.cap_allowed(pd, cap)
                && (!ist_neu || self.pds.cap_count(pd) + neue_slots < self.pds.budget_of(pd));
            if passt {
                neue_slots += usize::from(ist_neu);
                continue;
            }
            // Die Angebots-Maske ist ein `u16` aus der ABI (Slots 0..15) — Slots jenseits
            // davon sind immer Zusagen des Manifests. Die ABI-Vorgabe steht in fremder
            // Hand; was sie nicht ausdrueckt, drueckt Ablehnung aus, nicht Zustimmung.
            if slot < 16 && angebote & (1u16 << slot) != 0 {
                abgelehnte_angebote += 1;
                continue;
            }
            return Err(slot);
        }
        Ok(abgelehnte_angebote)
    }

    /// **Die Summenprüfung** (A-3.4, Abschluss): wie viele globale Slots gehen auf **kein**
    /// PD-Budget?
    ///
    /// [`budget_allows`](Self::budget_allows) deckelt den Verbrauch **einer** PD; dass die Summe
    /// aller Budgets in die Tabelle passt, trägt seit A-3.4 die Dimensionierung
    /// ([`CAP_SLOTS_TOTAL`]). Ungeprüft blieb die andere Seite derselben Rechnung: der Kernel
    /// selbst installiert Wurzel-Caps, und nichts hindert ihn daran, mehr als
    /// [`CAP_SLOTS_KERNEL_RESERVE`] zu belegen. Dann ist jede einzelne PD innerhalb ihres Budgets
    /// und die Zusage „jede PD bekommt ihr Budget" trotzdem gebrochen — mit `NoSlot` an einer
    /// Stelle, die nichts falsch gemacht hat.
    ///
    /// Gezählt wird **nicht** über `used_slots() − Σ cap_count(pd)`: halten zwei PDs denselben
    /// [`CapPtr`], überzählt die Summe, und der Fehlbetrag zeigt in die unsichere Richtung (die
    /// Prüfung ginge durch, obwohl sie es nicht sollte). Stattdessen wird jeder von einer PD
    /// gehaltene Slot in `seen` markiert und danach ausgezählt, was belegt und unmarkiert blieb.
    ///
    /// `seen` muss mindestens so viele Einträge haben wie die Slot-Tabelle; sein Inhalt beim
    /// Eintritt ist gleichgültig (wird genullt). `None` heißt „konnte nicht laufen", nicht „in
    /// Ordnung" — siehe [`CapSpace::unmarked_used_slots`].
    pub fn nonbudget_slots(&self, seen: &mut [bool]) -> Option<usize> {
        let (nslots, _) = self.cspace.capacity();
        if seen.len() < nslots {
            return None;
        }
        seen[..nslots].fill(false);
        for pd in 0..self.pds.capacity() {
            if !self.pds.is_used(pd) {
                continue;
            }
            // Ueber die Lauf-Laenge, nicht ueber das Legacy-Fenster: was jenseits von
            // Slot 15 liegt, ist Verbrauch auf PD-Budget und darf nicht als
            // Kernel-Reserve gezaehlt werden (falsche Richtung: die Pruefung ginge durch,
            // obwohl sie es nicht sollte — dieselbe Form wie die Ueberzaehlung, vor der
            // der Kommentar oben warnt).
            for slot in 0..self.pds.cspace_len_von(pd) {
                if let Some(cap) = self.pds.cap_at(pd, slot) {
                    // Ein nicht auflösbares Handle markiert nichts — es ist kein Verbrauch.
                    let _ = self.cspace.mark_slot(cap, seen);
                }
            }
        }
        self.cspace.unmarked_used_slots(seen)
    }

    /// Hält der Kernel sich an seine Reserve? `Some(true)`/`Some(false)`, `None` = die Prüfung
    /// konnte nicht laufen (zu kleine Zählfläche) — die drei Fälle bleiben getrennt.
    pub fn kernel_reserve_ok(&self, seen: &mut [bool]) -> Option<bool> {
        self.nonbudget_slots(seen)
            .map(|n| n <= CAP_SLOTS_KERNEL_RESERVE)
    }

    /// Erlaubt die Domänen-Policy die Cap-Art von `cap` in PD `pd`? Reiner Test (kein Eintrag) —
    /// damit Pfade, die VOR dem Eintragen eine Ableitung/Kopie erzeugen (Grant in IPC), die Policy
    /// **prüfen, bevor** sie eine Kopie anlegen (kein Policy-Bypass + keine verwaiste Kopie bei
    /// Ablehnung). Identisch zur Prüfung in [`install_cap_checked`].
    pub fn cap_allowed(&self, pd: usize, cap: CapPtr) -> bool {
        let Some(domain) = self.pds.domain_of(pd) else {
            return false;
        };
        let Some((kind, _, _)) = self.cspace.lookup(cap) else {
            return false;
        };
        if !domain_allows_kind(domain, kind) {
            return false;
        }
        // HardwareLand-Backend: Kommunikations-Caps NUR für den eigenen Kanal (paarweise
        // Bindung) — ein Backend darf niemals eine Endpoint-/Notification-Cap zu irgendetwas
        // anderem als seinem Partner-Kanal erhalten.
        if domain == Domain::HardwareLand {
            let (cep, cntfn) = self.pds.chan_of(pd);
            let irq = self.pds.irq_ntfn_of(pd);
            match kind {
                ObjectKind::Endpoint(id) if id != cep => return false,
                // Kanal-Notification ODER die eigene Interrupt-Notification (B4) -- beide vom
                // Kernel gewaehlt, beide fuehren zu niemandem sonst.
                ObjectKind::Notification(id) if id != cntfn && id != irq => return false,
                _ => {}
            }
        }
        true
    }

    /// **Domänen-Policy-Oracle** (ext-22). `0` = konsistent, sonst Anomalie-Code:
    /// - `1` = Hardware-Cap (MMIO/IRQ/DMA) in einer Nicht-HardwareLand-PD.
    /// - `2` = `PdControl`-Cap in einer Nicht-TrustedSas-PD.
    /// - `3` = Isolations-Verletzung: eine **HardwareLand/UserLand**-PD läuft im globalen
    ///   Adressraum (VSPACE_OF==0) statt isoliert. Die sicherheitsrelevante Richtung ist
    ///   „untrusted Domänen MÜSSEN isoliert sein"; eine TrustedSas-PD darf isoliert ODER
    ///   global laufen (mehr Isolation ist nie eine Verletzung — wichtig für Rückwärts-
    ///   kompatibilität, da bestehende isolierte PDs per Default als TrustedSas getaggt sind).
    ///
    /// - `4` = HardwareLand-Backend ohne gültige Partner-Bindung (Partner fehlt oder ist
    ///   keine TrustedSas-PD).
    /// - `5` = HardwareLand-Backend hält eine Endpoint-/Notification-Cap, die NICHT zu seinem
    ///   dedizierten Partner-Kanal gehört (verbotene Kommunikationsbeziehung).
    ///
    /// `vspace_is_global(tid)` meldet, ob der Thread im globalen SAS-Adressraum läuft
    /// (VSPACE_OF==0). Die 1:N-Kardinalität ist strukturell (ein `partner`-Feld); das
    /// UserLand↔HardwareLand-Kommunikationsverbot folgt in P6.
    pub fn domain_audit(&self, is_live_global: &dyn Fn(ThreadId) -> bool) -> u32 {
        for pd in 0..self.pds.capacity() {
            if !self.pds.is_used(pd) {
                continue;
            }
            let domain = match self.pds.domain_of(pd) {
                Some(d) => d,
                None => continue,
            };
            // Cap-Typ-Policy: erlaubte Cap-Typen je Domäne.
            for slot in 0..self.pds.cspace_len_von(pd) {
                if let Some(cap) = self.pds.cap_at(pd, slot) {
                    if let Some((kind, _, _)) = self.cspace.lookup(cap) {
                        if kind_is_hardware(kind) && domain != Domain::HardwareLand {
                            return 1;
                        }
                        if kind_is_pd_control(kind) && domain != Domain::TrustedSas {
                            return 2;
                        }
                    }
                }
            }
            // Isolations-Mechanismus: untrusted Domänen MÜSSEN isoliert sein. TrustedSas
            // darf global ODER isoliert sein (mehr Isolation ist keine Verletzung).
            if domain != Domain::TrustedSas {
                // **ALLE Threads dieser PD, nicht nur der erste** (Z22 P2). Bis mehrere Threads
                // je PD möglich wurden, waren „die PD" und „ihr Thread" dasselbe; seither wäre
                // `thread_of(pd)` genau die Lücke, vor der die D0-Lehre warnt: ein Umbau, der
                // einen neuen Zustand einführt, muss jede Stelle mitnehmen, die über Zustände
                // URTEILT. Ein zweiter, global laufender Thread einer isolierten PD wäre sonst
                // unsichtbar — und der Audit-Code 3 ist genau dafür da.
                if self.pds.any_thread(pd, is_live_global) {
                    return 3;
                }
            }
            // Paarweise Bindung (HardwareLand-Backend):
            if domain == Domain::HardwareLand {
                // Regel 4: der Partner muss eine (existierende) TrustedSas-PD sein.
                match self.pds.partner_of(pd) {
                    Some(t) if self.pds.domain_of(t) == Some(Domain::TrustedSas) => {}
                    _ => return 4,
                }
                // Regel 5: ein Backend hält NUR Kommunikations-Caps für seinen eigenen Kanal.
                let (cep, cntfn) = self.pds.chan_of(pd);
                for slot in 0..self.pds.cspace_len_von(pd) {
                    if let Some(cap) = self.pds.cap_at(pd, slot) {
                        if let Some((kind, _, _)) = self.cspace.lookup(cap) {
                            match kind {
                                ObjectKind::Endpoint(id) if id != cep => return 5,
                                ObjectKind::Notification(id) if id != cntfn => return 5,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        0
    }

    /// **DMA-Bounds-Oracle** (ext-23, SMMU-agnostisch; Cap-Ebene). Prüft NUR die hardware-
    /// unabhängigen DmaCap-Bounds-Invarianten über alle (distinct) DMA-Objekte — die volle
    /// DMA-Policy (inkl. Enforcer-Durchsetzung + Revoke-Ordnung) ist `system::dma_audit`, das dies
    /// aufruft. (Konsolidierung O-C: umbenannt von `dma_audit`, um die Namensgleichheit mit dem
    /// aggregierenden `system::dma_audit` aufzulösen.) `0` = konsistent, sonst:
    /// - `1` = eine DMA-Region ist nicht 4-KiB-ausgerichtet/leer ODER liegt außerhalb des
    ///   mappbaren RAM-Fensters `[floor, ceil)` (mit `floor` = Kernel-Image-Ende deckt das
    ///   insbesondere „Region überlappt das Kernel-Image" ab — sie wäre nicht aus freiem RAM
    ///   ausgeschnitten).
    /// - `2` = zwei verschiedene DMA-Regionen überlappen einander (eine Region doppelt vergeben).
    ///
    /// „DmaCap nur in HardwareLand" deckt bereits [`Caps::domain_audit`] (Code 1) ab, da
    /// `kind_is_hardware` nun `Dma` einschließt. Die hardware-erzwungene Durchsetzung (SMMU)
    /// liegt hinter dem kernel-internen `DmaEnforcer` und wird separat auditiert.
    #[inline(never)] // Audit 2026-09-10 (#DF-Klasse): 512-B-Rahmen aus Crate-Aufrufern heraushalten.
    pub fn dma_bounds_audit(&self, floor: u64, ceil: u64) -> u32 {
        let mut regs: [(u64, u64); 32] = [(0, 0); 32];
        let mut n = 0usize;
        let mut bad = 0u32;
        self.cspace.for_each_dma(&mut |phys, len| {
            if len == 0
                || phys % 4096 != 0
                || len % 4096 != 0
                || phys < floor
                || phys.saturating_add(len) > ceil
            {
                bad = 1;
            }
            if n < regs.len() {
                regs[n] = (phys, len);
                n += 1;
            }
        });
        if bad != 0 {
            return bad;
        }
        // Paarweise Disjunktheit (verschiedene Objekte dürfen sich nie überlappen).
        for i in 0..n {
            for j in (i + 1)..n {
                let (a0, al) = regs[i];
                let (b0, bl) = regs[j];
                if a0 < b0 + bl && b0 < a0 + al {
                    return 2;
                }
            }
        }
        0
    }
}

/// Ist `kind` ein **Hardware-Cap** (MMIO/IRQ/DMA)? Generische Kategorie: alle Cap-Typen, die
/// direkten Geräte-Zugriff autorisieren und daher ausschließlich HardwareLand vorbehalten sind.
/// (ext-22: MMIO/IRQ; ext-23: zusätzlich DMA.)
fn kind_is_hardware(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Mmio { .. } | ObjectKind::Irq { .. } | ObjectKind::Dma { .. }
    )
}

/// Ist `kind` eine **TrustedSas-Autoritäts-Cap** (`PdControl` oder `Loader`, ext-26)? Solche dürfen
/// nur TrustedSas-PDs halten.
fn kind_is_pd_control(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::PdControl { .. } | ObjectKind::Loader { .. })
}

/// Ist `kind` eine **Handler-Cap** (Z26/A3)? Wer eine hält, ist der **Kernel eines Gastes**.
fn kind_is_handler(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::SyscallHandler { .. } | ObjectKind::FaultHandler { .. }
    )
}

/// Darf eine PD der Domäne `domain` eine Cap des Typs `kind` halten?
/// HW-Caps nur HardwareLand; `PdControl`/`Loader` und **Handler-Caps** nur TrustedSas; alles
/// andere überall.
///
/// **Warum Handler-Caps in dieselbe Klasse wie `PdControl` gehören** (Z26/A3): Z26 rechnet die
/// Autorität ehrlich zusammen — die Persönlichkeits-PD **ist der Kernel des Gastes**. Sie liest
/// und schreibt dessen ganzen Trap-Frame und kann ihn belügen. Das ist mindestens so viel wie
/// „darf eine PD starten und stoppen", und es an eine UserLand-PD zu vergeben hiesse, den Gast
/// einem Nachbarn auszuliefern, der selbst keiner Prüfung unterliegt.
///
/// Damit steht auch die Manifest-Frage aus Z26 schon halb beantwortet: wer Gast-Kernel sein darf,
/// ist eine Domänen-Eigenschaft der **signierten** Fläche und ergibt sich nicht aus dem Besitz
/// eines Endpoints.
fn domain_allows_kind(domain: Domain, kind: ObjectKind) -> bool {
    if kind_is_hardware(kind) {
        domain == Domain::HardwareLand
    } else if kind_is_pd_control(kind) || kind_is_handler(kind) {
        domain == Domain::TrustedSas
    } else {
        true
    }
}

/// Eine Protection Domain: ein Thread + ein Capability-Space + eine Sicherheitsdomäne.
#[derive(Clone, Copy)]
pub struct Pd {
    used: bool,
    /// **Der ERSTE gebundene Thread** — nicht mehr „der" Thread (Z22 P2).
    ///
    /// Seit eine PD mehrere Threads tragen darf, ist dieses Feld nur noch ein *Vertreter*: es
    /// beantwortet „gibt es hier überhaupt einen Thread, und welcher war zuerst da". Die
    /// Zuordnung Thread → PD steht seither in [`PdTable::owner`] und nicht mehr hier; sie
    /// hier zu suchen hiesse, mehrere Threads gar nicht darstellen zu können.
    thread: Option<ThreadId>,
    /// Wie viele Threads gerade an diese PD gebunden sind (Z22 P2). Auskunft für den Bericht —
    /// und die einzige Grösse, an der „mehrere Threads je PD" von aussen ablesbar ist.
    nthreads: u32,
    /// **Belegungs-Generation.** Wird bei jeder Freigabe erhöht und in jedem Rückwärts-Eintrag
    /// mitgeführt.
    ///
    /// Ohne sie hätte der O(1)-Weg eine Lücke, die der lineare Scan nicht hatte: eine PD wird
    /// freigegeben, ihr **Index** sofort neu vergeben — und ein noch lebender Thread der alten PD
    /// zeigte über den Rückwärts-Index auf die **neue**. Der alte Scan gab dort `None`, weil die
    /// neue PD den Thread nicht in ihrem `thread`-Feld trug. Eine Beschleunigung, die eine
    /// Fremd-PD-Zuordnung erfindet, wäre schlimmer als der lineare Scan.
    epoch: u32,
    /// Lokaler Cspace dieser PD: Lauf `[cspace_start, cspace_start + cspace_len)` im
    /// Cspace-Pool der Tabelle (TODO0 K1c).
    ///
    /// Bis dahin ein Inline-Array `[Option<CapPtr>; NCAPS]` fester Laenge — 16 Slots je
    /// PD, hart, und 256 Byte je Eintrag auch fuer PDs, die vier Caps halten. Jetzt kostet
    /// die Groesse Pool-Speicher statt Struktur: Standard-PDs bekommen 16 Plaetze
    /// (bitgleich zu frueher), Treiber-PDs mehr. Die Aufloesung Slot -> Pool-Index steht
    /// in [`PdTable::cap_at`]; der Lauf selbst wird bei der Erzeugung vergeben und bei
    /// der Freigabe zurueckgegeben (s. `cspace::Vergabe`).
    cspace_start: u32,
    /// Laenge des Cspace-Laufs in Slots. `0` fuer freie Eintraege — ein belegter Eintrag
    /// hat immer mindestens einen Platz (die Vergabe gibt keine leeren Laeufe an PDs).
    cspace_len: u32,
    /// Freilisten-Verkettung (Index + 1, `0` = Ende) — nur gueltig, solange der Eintrag
    /// **frei** ist (s. `cspace::AnkerAblage`).
    ///
    /// Die zurueckgegebenen Laeufe brauchen eine Ablage, und ein zweiter Pool dafuer
    /// waere eine zweite Boot-RAM-Forderung an den Kernel fuer dieselbe Sache: der
    /// ungenutzte Eintrag selbst ist die Ablage. Kein Byte Mehr-RAM fuer die Freiliste.
    freilauf_next: u32,
    /// **Wie viele Slots diese PD gleichzeitig belegen darf** (2026-08-26).
    ///
    /// Bis dahin eine Konstante fuer alle. Das war tragfaehig, solange jede PD 1--4 Slots
    /// brauchte; mit einer Treiberumgebung braucht **eine** PD dreissig und zehntausend andere
    /// weiterhin vier. Eine Konstante anzuheben kostet den Bedarf der einen PD **mal `NPDS`** --
    /// gemessen rund 7,7 MB je zusaetzlichen acht Slots, fuer einen Bedarf, den eine einzige PD
    /// hat.
    ///
    /// Deshalb eine Zahl je PD, gedeckelt durch [`CAP_BUDGET_MAX`] und gedeckt durch den
    /// **Vorrat** [`PdTable::budget_vorrat`]. Dieselbe Bewegung wie bei `MELDESTELLEN` (von Hand
    /// gefuehrt -> abgeleitet) und `IDENTITY_DEBTS` (Zahl -> Menge): wo eine Konstante eine
    /// Buchhaltung vertritt, wird sie irgendwann falsch.
    cap_budget: u16,
    /// Sicherheitsdomäne — **unveränderlich** nach der Erzeugung (Policy: bestimmt die
    /// erlaubten Cap-Typen + Kommunikationsbeziehungen). Migration = neue PD, nicht umschalten.
    domain: Domain,
    /// Nur für [`Domain::HardwareLand`]: der **eine** unveränderliche Trusted-SAS-Partner
    /// (PD-Index), mit dem dieses Backend ausschließlich kommunizieren darf. Bei der Erzeugung
    /// gesetzt; `None` für Trusted/User.
    partner: Option<u16>,
    /// **Z23 S1: die Tore dieser PD sind zu.** Erste Phase der Zwei-Phasen-Stilllegung.
    ///
    /// Gesetzt heisst: was diese PD **anfängt**, wird abgewiesen (`CALL`/`RECV` → `ERR_QUIESCING`);
    /// was schon läuft, darf **auslaufen** (`REPLY` bleibt erlaubt). Ohne diese Trennung wäre die
    /// Stilllegung entweder wirkungslos (alles erlaubt) oder ein Deadlock-Erzeuger (auch `REPLY`
    /// gesperrt → ein Server kann seine offene Antwort nicht mehr loswerden, und sein Client
    /// hängt für immer).
    ///
    /// **Am SUBJEKT, nicht am Objekt** — und das ist der Grund, warum es hier steht und nicht in
    /// `Endpoint`. An einem Endpoint hängen auch **fremde** PDs; ein Riegel dort fröre Dritte mit
    /// ein, die mit dem Freeze nichts zu tun haben. `Endpoint::begin_quiesce` (A-4.2) bleibt
    /// daneben bestehen: es legt **einen Kanal** still, dieses Bit **einen Teilnehmer**.
    ///
    /// **TCB-Kosten, benannt:** ein Bit je PD und eine Prüfung im Syscall-Pfad. Mehr nicht.
    quiescing: bool,
    /// Stabile **Backend-Id** (ext-22): identifiziert ein HardwareLand-Backend unabhängig vom
    /// PD-Index (für später: mehrere NICs/USB-Controller, Hotplug, PCIe). `0` = keins.
    backend_id: u16,
    /// Endpoint-/Notification-ID des **dedizierten Kanals** zum Partner (HardwareLand-Backend).
    /// Das Backend darf NUR über diesen Kanal kommunizieren. `u32::MAX` = keiner.
    chan_ep: u32,
    chan_ntfn: u32,
    /// **Die zweite erlaubte Notification einer HardwareLand-PD** (Stufe B, B4): die, auf der ihr
    /// Geraeteinterrupt ankommt. `u32::MAX` = keine.
    ///
    /// Die Regel darunter -- *ein Backend bekommt Kommunikations-Caps nur fuer seinen eigenen
    /// Kanal* -- bleibt unveraendert und richtig; sie hat den ersten Anlauf von B4 zu Recht
    /// abgewiesen. Aufgeweicht wird sie nicht, sondern **erweitert um genau ein Objekt, das der
    /// Kernel selbst fuer diese PD gepraegt hat**: es verbindet sie mit niemandem. Ein Backend, das
    /// darauf `signal` ruft, weckt sich selbst.
    ///
    /// Eine allgemeine Ausnahme („Notifications sind erlaubt") waere die bequeme Fassung und genau
    /// der Kanal zu einem Dritten, den die Regel ausschliesst.
    irq_ntfn: u32,
    /// **Empfangs-Slot für per IPC übertragene Caps** (A-3.2). Der Empfänger legt fest, wo eine
    /// gegrantete Cap landet — nicht der Sender. Voreinstellung ist der frühere feste Slot
    /// ([`caprock_abi::GRANT_RECV_SLOT`]), damit bestehende Programme unverändert laufen.
    ///
    /// Warum der Empfänger und nicht der Sender: der Sender kennt den Cspace des Empfängers
    /// nicht. Dürfte er den Slot wählen, könnte ein Server jeden Cap seines Clients verdrängen —
    /// eine Schreiboperation in fremdes Eigentum, verkleidet als Antwort.
    recv_slot: usize,
    /// **Die ausgehende Kante im Handler-Graphen** (Z26/A3): PD, deren Persönlichkeit die Threads
    /// **dieser** PD behandelt. `u16::MAX` = keine.
    ///
    /// Die Bindung selbst steht **je Thread** im TCB; hier steht die **PD-Sicht**, weil die Frage
    /// „schliesst das einen Kreis?" eine Frage über PDs ist und nicht über Threads: der Handler
    /// ist eine PD, und *jeder* ihrer Threads erbt beim `RECV` das Schicksal ihrer Bindung.
    ///
    /// Zwei Strukturen für eine Sache sind ein Riss — deshalb ist der Graph **funktional** (eine
    /// PD hat höchstens eine ausgehende Kante) und [`handler_bound`] zählt die Threads, die an
    /// ihr hängen. Fällt der Zähler auf 0, fällt die Kante. Eine PD hat höchstens EINEN Kernel;
    /// zwei Persönlichkeiten über einem Adressraum wären zwei Wahrheiten über denselben Speicher.
    handler_pd: u16,
    /// Wie viele Threads dieser PD hängen an [`handler_pd`]? Hält Kante und TCB-Bindungen
    /// zusammen: `handler_bound == 0` **genau dann**, wenn `handler_pd == u16::MAX`.
    handler_bound: u32,
    /// **Belegungsbitmaske der Sidecar-Slots**, wenn diese PD ein *Handler* ist. Bit `i` gesetzt =
    /// Slot `i` vergeben. 64 Gäste je Handler-PD; darüber [`caprock_abi::result::ERR_NOSPACE`].
    ///
    /// Warum eine Maske und keine Zahl: Slots werden **einzeln** frei (ein Gast stirbt), und ein
    /// Zähler könnte danach einen Slot zweimal vergeben — zwei Gäste im selben Fenster, und der
    /// eine liest den halben Syscall des anderen. Dieselbe Form wie `sc_donee` vor D9: ein
    /// Zeiger, wo eine Menge gemeint war.
    sidecar_used: u64,
}

/// „Keine Handler-Kante."
pub const KEIN_HANDLER_PD: u16 = u16::MAX;
/// Sidecar-Slots je Handler-PD (Breite von [`Pd::sidecar_used`]).
pub const SIDECAR_SLOTS: u16 = 64;

/// **Das Register der Bindungsurteile** (Z26/A3) — ein Zähler je [`redirect::BindUrteil`].
///
/// ## Warum gezählt und nicht nur abgewiesen
///
/// Nach aussen tragen `Zyklus`, `SelbstBindung` und `KetteZuLang` **denselben** ABI-Code: der
/// Aufrufer kann in allen drei Fällen nichts anderes tun. Innen sind sie **verschieden**, und
/// zwar so, dass es zählt: `KetteZuLang` heisst „es gibt bereits einen Kreis, an dem der Gast gar
/// nicht beteiligt ist" — ein **Kernelfehler**, kein Aufruferfehler. Wer die drei zusammenwirft,
/// verliert genau den, der auf einen Fehler im Kernel zeigt.
///
/// Und die Sprechprobe hängt daran: ein Prüfer, der meldet „kein Zyklus aufgetreten", muss
/// belegen können, dass überhaupt gebunden wurde. `HANDLER_URTEILE[0]` (= `Ok`) ist dieser Beleg.
pub static HANDLER_URTEILE: [core::sync::atomic::AtomicU32; 7] =
    [const { core::sync::atomic::AtomicU32::new(0) }; 7];

/// Index eines Urteils im Register. **Erschöpfend** — ein neues Urteil bricht hier den Bau, statt
/// still in einen Sammeleimer zu fallen.
pub fn urteil_index(u: redirect::BindUrteil) -> usize {
    use redirect::BindUrteil as B;
    match u {
        B::Ok => 0,
        B::Zyklus => 1,
        B::SelbstBindung => 2,
        B::FremderHandler => 3,
        B::KetteZuLang => 4,
        B::KeinSlot => 5,
        B::Wirkungslos => 6,
    }
}

/// Zählerstand eines Urteils (für Bericht und Prüfzeile).
pub fn handler_urteil_count(u: redirect::BindUrteil) -> u32 {
    HANDLER_URTEILE[urteil_index(u)].load(core::sync::atomic::Ordering::Relaxed)
}

/// Alle sieben Zählerstände auf einmal — in der Reihenfolge von [`urteil_index`].
pub fn handler_urteile() -> [u32; 7] {
    let mut o = [0u32; 7];
    for (i, x) in o.iter_mut().enumerate() {
        *x = HANDLER_URTEILE[i].load(core::sync::atomic::Ordering::Relaxed);
    }
    o
}

impl Pd {
    const EMPTY: Pd = Pd {
        used: false,
        thread: None,
        nthreads: 0,
        epoch: 0,
        cspace_start: 0,
        cspace_len: 0,
        freilauf_next: 0,
        cap_budget: CAP_BUDGET_PER_PD as u16,
        domain: Domain::TrustedSas, // Default: alle Altbestand-PDs sind Trusted-SAS
        quiescing: false,
        partner: None,
        backend_id: 0,
        chan_ep: u32::MAX,
        chan_ntfn: u32::MAX,
        irq_ntfn: u32::MAX,
        recv_slot: caprock_abi::GRANT_RECV_SLOT,
        handler_pd: KEIN_HANDLER_PD,
        handler_bound: 0,
        sidecar_used: 0,
    };
}

/// **Ein Eintrag der Rückwärts-Tabelle Thread-Slot → PD** (Z22 P2 / C4).
///
/// Der globale Thread-Slot (`ThreadId::slot()`) ist bereits der stabile Index der per-Thread-
/// Tabellen des Kernels (`FpState`, `VSPACE_OF`, `KSTACKS`); diese ist die vierte.
///
/// Warum die **volle** `ThreadId` (Slot **und** Generation) und nicht nur der PD-Index: ein
/// Thread-Slot wird wiederverwendet. Steht dort nur „Slot 7 → PD 3", erbt der nächste Thread auf
/// Slot 7 die PD seines Vorgängers — also dessen gesamten Cspace. Mit der Generation im Eintrag
/// ist das nicht bloss unwahrscheinlich, sondern nicht formulierbar.
#[derive(Clone, Copy)]
pub struct ThreadOwner {
    /// `ThreadId::to_raw()` des gebundenen Threads (Slot + Generation).
    tid: u64,
    /// PD-Index **+ 1**; `0` heisst „kein Eintrag". Spart ein `Option` in einer Tabelle mit
    /// zehntausend Einträgen und macht den leeren Zustand zum Nullmuster.
    pd1: u32,
    /// Belegungs-Generation der PD zum Zeitpunkt der Bindung (s. [`Pd::epoch`]).
    epoch: u32,
}

impl ThreadOwner {
    pub const EMPTY: ThreadOwner = ThreadOwner {
        tid: 0,
        pd1: 0,
        epoch: 0,
    };
}

/// **Wie oft [`PdTable::pd_of`] gefragt wurde** — der Nenner jeder Aussage über die Auflösung.
///
/// Ohne ihn wäre `PD_OF_SCAN_ITER == 0` von „nie gefragt" nicht zu unterscheiden: dieselbe
/// Sprechprobe wie bei `pdbind` (die Gelegenheit zählen, nicht nur den Treffer).
pub static PD_OF_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// **Iterationen im LINEAREN Rückfallpfad von [`PdTable::pd_of`]** (C4).
///
/// `pd_of` löst bei **jedem** Syscall die aufrufende PD auf — und tat das bis Z22 P2 mit einem
/// linearen Scan über die ganze PD-Tabelle. Bei `NPDS = 10_000` ist das der teuerste O(n)-Pfad
/// im System und stand in der C4-Liste nicht drin: die dort genannten Stellen laufen je
/// Cap-Allokation bzw. je Thread-Tod, diese je **Syscall**.
///
/// Gezählt statt gestoppt: eine Iterationszahl ist eine Eigenschaft des Programms, eine
/// Zeitmessung nicht (D10). Nach dem Umbau muss diese Zahl **0** sein, während `PD_OF_CALLS`
/// gross ist — beides zusammen ist die Aussage, keine der beiden allein.
pub static PD_OF_SCAN_ITER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// **Wie viele Aufrufe** in den linearen Rückfallpfad gerieten (nicht wie viele Iterationen).
///
/// Getrennt gezählt, weil die beiden verschiedene Fragen beantworten: die Iterationszahl sagt,
/// was es gekostet hat, diese hier, ob es **einmal** oder **immer** passiert. Eine grosse
/// Iterationszahl aus einem einzigen Aufruf ist ein anderer Befund als dieselbe Zahl aus
/// tausend.
pub static PD_OF_SCAN_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Der **erste** Thread-Slot, der in den Rückfallpfad geriet, `+1` (0 = keiner). Ohne ihn ist
/// „ein Aufruf war linear" nicht diagnostizierbar — und ein Rückfall, den niemand erklären kann,
/// ist keine Messung, sondern ein offener Posten.
pub static PD_OF_SCAN_ERSTER_SLOT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Iterationen im linearen Scan von [`PdTable::create_in_domain`]/`create_hardware_backend` (C4).
pub static PD_CREATE_SCAN_ITER: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Wie oft eine Bindung an der Rückwärts-Tabelle **vorbeilief**, weil sie (noch) keinen Speicher
/// hat. `> 0` heisst: `attach_owner` fehlt oder kam zu spät — dann ist die Auflösung wieder
/// linear, und der Bericht sagt es, statt still langsam zu sein.
pub static PD_OWNER_UNANGEHAENGT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Tabelle aller Protection Domains.
pub struct PdTable {
    pds: Slab<Pd>,
    /// Rückwärts-Index Thread-Slot → PD (Z22 P2 / C4). Länge = Thread-Kapazität des Systems.
    owner: Slab<ThreadOwner>,
    /// **Der noch nicht vergebene Rest des Slot-Vorrats** (2026-08-26).
    ///
    /// `CAP_SLOTS_FOR_ALL_PDS` war die **Summe aller Budgets**, solange jedes Budget gleich war.
    /// Mit Budgets je PD ist dieselbe Zahl ein **Vorrat**, aus dem vergeben wird -- und die
    /// Zusage „jede PD bekommt ihr Budget" haelt nur, wenn die Vergabe gegen ihn bucht. Ohne
    /// diesen Zaehler waere sie wieder eine Annahme ueber das Verhalten der PDs, genau wie vor
    /// A-3.4 (256 vorhandene Slots gegen 2048 zugesagte, und niemand rechnete nach).
    budget_vorrat: usize,
    /// Wie oft eine Erzeugung am **Vorrat** gescheitert ist (nicht an einem freien PD-Slot).
    /// Getrennt gezaehlt, weil die beiden verschiedene Behebungen haben.
    budget_abgewiesen: u64,
    /// Der Pool der PD-Cspace-Plaetze (TODO0 K1c): alle Laeufe aller PDs liegen hier.
    /// Beim Boot aus Boot-RAM angehaengt ([`PdTable::attach_cspace_pool`], Vorbild A-3.4);
    /// davor ist jede Erzeugung eine benannte Absage, kein Absturz.
    cspace_pool: Slab<Option<CapPtr>>,
    /// Vergabe-Zustand des Pools (Bump + Freiliste, s. `cspace::Vergabe`).
    cspace_vergabe: Vergabe,
    /// Der **eingestellte** Deckel fuer Einzel-Budgets — [`CAP_BUDGET_MAX`] als Vorgabe,
    /// anhebbar fuer Treiber-PDs ([`PdTable::set_budget_deckel`]).
    budget_deckel: u16,
    /// Wie oft eine Erzeugung **ueber dem Deckel** abgewiesen wurde (nicht gedeckelt!).
    /// Bis TODO0 K1c fiel diese Absage mit `None` ohne eigenen Zaehler — im Bericht war
    /// sie von „nie versucht" nicht zu unterscheiden.
    deckel_abgewiesen: u64,
    /// Wie oft eine Erzeugung am **Pool** gescheitert ist (weder Freiliste noch frischer
    /// Rest gaben die Plaetze her). Dritte Schranke, dritter Zaehler, dritte Behebung
    /// (Pool vergroessern statt weniger anfordern).
    cspace_abgewiesen: u64,
}

impl Default for PdTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Die PD-Eintraege als Anker-Ablage der Cspace-Vergabe (TODO0 K1c).
///
/// Ein Anker je Eintrag: vergeben beschreibt er den Lauf der PD, frei verketten
/// `freilauf_next`-Felder die zurueckgegebenen Laeufe. Eigener Typ statt Generik —
/// die Vergabe-Logik steht genau einmal in `cspace::Vergabe` und wird dort auch
/// geprueft (als Datei, ohne Kernel).
impl cspace::AnkerAblage for Slab<Pd> {
    fn anzahl(&self) -> usize {
        self.len()
    }
    fn lese(&self, i: usize) -> CspaceAnker {
        // Fail-closed: eine Freikette, die ausserhalb der Tabelle zeigt, reisst den
        // Knoten lieber mit, als einen fremden Lauf zweimal zu vergeben.
        let p = &self[i];
        CspaceAnker {
            start: p.cspace_start,
            len: p.cspace_len,
            naechster: p.freilauf_next,
        }
    }
    fn schreibe(&mut self, i: usize, a: CspaceAnker) {
        let p = &mut self[i];
        p.cspace_start = a.start;
        p.cspace_len = a.len;
        p.freilauf_next = a.naechster;
    }
}

impl PdTable {
    pub const fn new() -> Self {
        Self {
            pds: Slab::empty(),
            owner: Slab::empty(),
            budget_vorrat: CAP_SLOTS_FOR_ALL_PDS,
            budget_abgewiesen: 0,
            cspace_pool: Slab::empty(),
            cspace_vergabe: Vergabe::neu(),
            budget_deckel: CAP_BUDGET_MAX as u16,
            deckel_abgewiesen: 0,
            cspace_abgewiesen: 0,
        }
    }

    /// Eine neue (leere) PD anlegen; gibt ihre ID zurück. Default-Domäne Trusted-SAS
    /// (Rückwärtskompatibilität — alle bestehenden Aufrufer bleiben unverändert).
    pub fn create(&mut self) -> Option<usize> {
        self.create_in_domain(Domain::TrustedSas)
    }

    /// Eine neue PD in einer bestimmten **Sicherheitsdomäne** anlegen, mit dem Vorgabebudget.
    /// Die Domäne ist danach **unveränderlich** (es gibt bewusst keinen Setter).
    pub fn create_in_domain(&mut self, domain: Domain) -> Option<usize> {
        self.create_mit_budget(domain, 0)
    }

    /// **Eine PD mit einem eigenen Cap-Budget anlegen** (2026-08-26).
    ///
    /// `budget == 0` heisst „die Vorgabe" ([`CAP_BUDGET_PER_PD`]) — damit ist jeder vorhandene
    /// Aufrufer bitgleich, und die Vertraeglichkeit steckt in der Kodierung statt in einem Zweig,
    /// den jemand im Kopf behalten muss. Die Cspace-Groesse ist dabei die Vorgabe
    /// ([`NCAPS`]); wer mehr Plaetze braucht (Treiber-PD), ruft
    /// [`PdTable::create_mit_budget_und_cspace`] und bekommt die Absage **benannt**.
    ///
    /// Jede Absage wird gezaehlt (s. [`PdTable::ablehnungs_bilanz`]); `None` heisst nur noch
    /// „irgendeine Schranke", die genaue steht im Zaehler und — fuer neue Aufrufer — im
    /// Rueckgabewert der grossen Schwester.
    pub fn create_mit_budget(&mut self, domain: Domain, budget: u16) -> Option<usize> {
        self.create_mit_budget_und_cspace(domain, budget, 0).ok()
    }

    /// **Eine PD mit eigenem Cap-Budget UND eigener Cspace-Groesse anlegen** (TODO0 K1c).
    ///
    /// `budget == 0` heisst „die Vorgabe" ([`CAP_BUDGET_PER_PD`]), `plaetze == 0` heisst
    /// „der Standard" ([`NCAPS`]) — bestehende 16er-PDs entstehen hier bitgleich zu frueher.
    ///
    /// Vier Absagen, jede mit eigenem Namen ([`CspaceAbweisung`]) und eigenem Zaehler —
    /// D11-Form: benannt, gezaehlt, nie blockierend. Eine Schranke, die klammheimlich
    /// kuerzt statt abzuweisen, gaebe dem Aufrufer eine PD, die weniger kann, als er
    /// angefordert hat, und liesse ihn das erst beim dreissigsten Cap merken.
    ///
    /// **Der Vorrat wird gebucht, nicht geschaetzt.** `CAP_SLOTS_FOR_ALL_PDS` war die Summe aller
    /// Budgets, solange jedes Budget gleich war; mit Budgets je PD ist dieselbe Zahl ein Vorrat,
    /// und die Zusage „jede PD bekommt ihr Budget" haelt nur, wenn die Vergabe gegen ihn bucht.
    pub fn create_mit_budget_und_cspace(
        &mut self,
        domain: Domain,
        budget: u16,
        plaetze: u32,
    ) -> Result<usize, CspaceAbweisung> {
        // 1. Policy, 2. Struktur — reine Funktion, eine Quelle (s. `cspace`).
        let anf = match cspace::anforderung_aufloesen(
            budget as u32,
            plaetze,
            self.budget_deckel as u32,
            CAP_BUDGET_PER_PD as u32,
        ) {
            Ok(a) => a,
            Err(CspaceAbweisung::DeckelUeberschritten { verlangt, deckel }) => {
                self.deckel_abgewiesen = self.deckel_abgewiesen.saturating_add(1);
                return Err(CspaceAbweisung::DeckelUeberschritten { verlangt, deckel });
            }
            Err(CspaceAbweisung::BudgetPasstNicht { budget, plaetze }) => {
                self.deckel_abgewiesen = self.deckel_abgewiesen.saturating_add(1);
                return Err(CspaceAbweisung::BudgetPasstNicht { budget, plaetze });
            }
            Err(e) => return Err(e),
        };
        // 3. Der Vorrat — eigene Lage, eigener Zaehler, eigene Behebung.
        if anf.budget as usize > self.budget_vorrat {
            self.budget_abgewiesen = self.budget_abgewiesen.saturating_add(1);
            return Err(CspaceAbweisung::VorratErschoepft {
                verlangt: anf.budget,
                vorrat: self.budget_vorrat.min(u32::MAX as usize) as u32,
            });
        }
        let i = self.freien_slot_suchen().ok_or(CspaceAbweisung::KeinePdFrei)?;
        // Wiederbelebter Slot zuerst entketten (Doppelvergabe-Schutz, s. `Vergabe::entketten`):
        // ein stehengebliebenes Glied beschriebe den neuen Lauf als frei. Passt der geloeste
        // Lauf, wird er weiterverwendet (kein Leck, kein Bump-Verbrauch).
        let alt = self.cspace_vergabe.entketten(&mut self.pds, i);
        // 4. Der Pool — die autoritative Stelle ist die Vergabe (sie kennt die Freiliste).
        let pool_len = self.cspace_pool.len().min(u32::MAX as usize) as u32;
        let (start, len) = match alt {
            Some(a) if a.len >= anf.plaetze => (a.start, a.len),
            _ => match self.cspace_vergabe.belegen(&mut self.pds, pool_len, anf.plaetze)
            {
                Ok(lauf) => lauf,
                Err(e) => {
                    self.cspace_abgewiesen = self.cspace_abgewiesen.saturating_add(1);
                    return Err(e);
                }
            },
        };
        let epoch = self.pds[i].epoch;
        self.pds[i] = Pd {
            used: true,
            domain,
            epoch,
            cap_budget: anf.budget as u16,
            cspace_start: start,
            cspace_len: len,
            freilauf_next: 0,
            ..Pd::EMPTY
        };
        self.budget_vorrat -= anf.budget as usize;
        Ok(i)
    }

    /// Den Deckel fuer Einzel-Budgets einstellen (TODO0 K1c) — Vorgabe
    /// [`CAP_BUDGET_MAX`].
    ///
    /// Wofuer: eine Treiber-PD braucht Budget 30, zehntausend andere weiterhin 10. Den
    /// Deckel je Tabelle zu heben kostet die anderen nichts; eine Konstante anzuheben
    /// kostete den Bedarf der einen PD mal `NPDS` (gemessen rund 7,7 MB je acht Slots).
    /// Bestehende Budgets bleiben unberuehrt — der Deckel gilt der **Erzeugung**, nicht
    /// dem Bestand. Wer ihn senkt, erzeugt kuenftig kleinere PDs, entzieht aber keiner
    /// bestehenden etwas (das waere ein Entzug ohne Syscall).
    pub fn set_budget_deckel(&mut self, deckel: u16) {
        self.budget_deckel = deckel;
    }

    /// Der eingestellte Deckel (Vorgabe: [`CAP_BUDGET_MAX`]) — Auskunft fuer den Bericht.
    pub fn budget_deckel(&self) -> u16 {
        self.budget_deckel
    }

    /// `(Vorrat-Abweisungen, Deckel-Abweisungen, Pool-Abweisungen)` — fuer den Bericht.
    ///
    /// **Alle drei zusammen**, aus demselben Grund wie `budget_bilanz`: eine Zahl ohne die
    /// anderen sagt nicht, welche Schranke bindet. `budget_bilanz` bleibt daneben stehen
    /// (Fuellstand + Vorrat-Abweisungen) — bestehende Leser aendern sich nicht.
    pub fn ablehnungs_bilanz(&self) -> (u64, u64, u64) {
        (self.budget_abgewiesen, self.deckel_abgewiesen, self.cspace_abgewiesen)
    }

    /// Das Cap-Budget dieser PD. Ein freier oder nicht existierender Slot hat **null** — nicht die
    /// Vorgabe: `budget_allows` faellt damit fail-closed aus, statt einer PD, die es gar nicht
    /// gibt, acht Slots zuzugestehen.
    pub fn budget_of(&self, pd: usize) -> usize {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].cap_budget as usize
        } else {
            0
        }
    }

    /// `(freier Vorrat, am Vorrat gescheiterte Erzeugungen)` — fuer den Bericht.
    ///
    /// **Beide Zahlen zusammen**, weil die eine ohne die andere nicht urteilsfaehig ist: ein
    /// Vorrat, der nur faellt, sieht aus wie einer unter Last, und eine Abweisungszahl ohne
    /// Fuellstand sagt nicht, ob es knapp war.
    pub fn budget_bilanz(&self) -> (usize, u64) {
        (self.budget_vorrat, self.budget_abgewiesen)
    }

    /// Der lineare Scan nach einem freien PD-Slot — **gezählt** (C4).
    ///
    /// Er ist weiterhin O(`NPDS`); was sich geändert hat, ist, dass die Zahl im Bericht steht.
    /// Ein fester Vorrat ohne Füllstandsanzeige spricht erst, wenn es zu spät ist.
    fn freien_slot_suchen(&self) -> Option<usize> {
        let mut iter = 0u64;
        let r = self.pds.iter().position(|p| {
            iter += 1;
            !p.used
        });
        PD_CREATE_SCAN_ITER.fetch_add(iter, core::sync::atomic::Ordering::Relaxed);
        r
    }

    /// Eine PD **freigeben** (ext-26, L4): den Slot leeren (used=false, Cspace/Domäne/Partner
    /// zurückgesetzt). Der Aufrufer ist dafür verantwortlich, die im Cspace gehaltenen Caps vorher
    /// zu löschen (sonst lecken globale Cap-Objekte) und den Thread/die VSpace abzubauen. Gibt
    /// `false`, wenn die PD nicht belegt war.
    pub fn free(&mut self, pd: usize) -> bool {
        if pd < self.pds.len() && self.pds[pd].used {
            // **Die Generation überlebt die Freigabe und wird erhöht.** Damit werden alle
            // Rückwärts-Einträge, die auf diese Belegung zeigen, in EINER Operation ungültig —
            // ohne die Thread-Tabelle abzulaufen (das wäre ein neuer O(n)-Pfad im Teardown,
            // also genau die Sorte, die C4 beseitigen will).
            let e = self.pds[pd].epoch.wrapping_add(1);
            // **Die Haelfte, die entscheidet, ob das Konto ein Konto ist.** Ein Vorrat, der nur
            // schrumpft, ist von einem unter Last nicht zu unterscheiden -- und nach genug PDs
            // waere die Zusage „jede PD bekommt ihr Budget" nicht mehr einloesbar, ohne dass ein
            // einziger Zaehler es gesagt haette. `saturating_add` mit Deckel, weil ein doppeltes
            // `free` sonst Vorrat aus dem Nichts schuefe.
            self.budget_vorrat = self.budget_vorrat
                .saturating_add(self.pds[pd].cap_budget as usize)
                .min(CAP_SLOTS_FOR_ALL_PDS);
            // **Erst die Slots nullen, dann den Lauf zurueckgeben** — in dieser Reihenfolge,
            // und das ist kein Stil: wer den Lauf zurueckgaebe, ohne die Slots zu loeschen,
            // veraeusserte die Caps der toten PD an die naechste. Die Iterator-Form
            // (`skip`/`take`) statt Slice-Arithmetik, damit ein unmoeglicher Lauf (Ende
            // jenseits des Pools) abbricht statt zu paniken — die Vergabe erzeugt nur
            // gueltige Laeufe, und was nur die Vergabe erzeugt, prueft niemand zweimal.
            let (start, len) = (self.pds[pd].cspace_start as usize, self.pds[pd].cspace_len as usize);
            for s in self.cspace_pool.as_mut_slice().iter_mut().skip(start).take(len) {
                *s = None;
            }
            self.cspace_vergabe.freigeben(&mut self.pds, pd);
            self.pds[pd] = Pd::EMPTY;
            self.pds[pd].epoch = e;
            true
        } else {
            false
        }
    }

    // ---------------------------------------------------------------------------------
    // Z26/A3: der Handler-Graph
    // ---------------------------------------------------------------------------------

    /// Knotenzahl des Handler-Graphen = Länge der PD-Tabelle. Die **Schrittschranke** der
    /// Zyklusprüfung; sie wird aus der Tabelle hergeleitet und steht nicht als Konstante daneben
    /// (dieselbe Begründung wie `eps.len()` statt `NENDPOINTS` in A-3.4 Teil 4).
    pub fn pd_capacity(&self) -> usize {
        self.pds.len()
    }

    /// Die Handler-PD von `pd` (die ausgehende Kante), oder `None`.
    pub fn handler_pd_of(&self, pd: usize) -> Option<u16> {
        if pd < self.pds.len() && self.pds[pd].used && self.pds[pd].handler_pd != KEIN_HANDLER_PD {
            Some(self.pds[pd].handler_pd)
        } else {
            None
        }
    }

    /// **Einen freien Sidecar-Slot bei der Handler-PD belegen.** `None` = alle 64 vergeben.
    ///
    /// Der Slot gehört ab hier **genau einem** Gast-Thread; nur der Kernel schreibt hinein.
    pub fn sidecar_belegen(&mut self, handler_pd: usize) -> Option<u16> {
        if handler_pd >= self.pds.len() || !self.pds[handler_pd].used {
            return None;
        }
        let frei = self.pds[handler_pd].sidecar_used;
        if frei == u64::MAX {
            return None;
        }
        let slot = frei.trailing_ones() as u16;
        // `trailing_ones` findet das erste 0-Bit. Bei `u64::MAX` waere das 64 -- deshalb steht die
        // Vollprobe DAVOR und nicht als `if slot >= 64` danach: eine Schranke, die den Ueberlauf
        // erst nach dem Rechnen prueft, ist eine Schranke mit einem Loch.
        self.pds[handler_pd].sidecar_used |= 1u64 << slot;
        Some(slot)
    }

    /// Einen Sidecar-Slot wieder freigeben (Gast entbunden oder tot).
    pub fn sidecar_freigeben(&mut self, handler_pd: usize, slot: u16) -> bool {
        if handler_pd >= self.pds.len() || !self.pds[handler_pd].used || slot >= SIDECAR_SLOTS {
            return false;
        }
        let bit = 1u64 << slot;
        let war = self.pds[handler_pd].sidecar_used & bit != 0;
        self.pds[handler_pd].sidecar_used &= !bit;
        war
    }

    /// Freie Sidecar-Slots bei `handler_pd` (für die Vorprüfung und den Bericht).
    pub fn sidecar_frei(&self, handler_pd: usize) -> u16 {
        if handler_pd >= self.pds.len() || !self.pds[handler_pd].used {
            return 0;
        }
        SIDECAR_SLOTS - self.pds[handler_pd].sidecar_used.count_ones() as u16
    }

    /// **Die Kante `gast -> handler` setzen** und den Thread-Zähler erhöhen.
    ///
    /// Der Aufrufer hat die Zulässigkeit **vorher** geprüft (Zyklus, fremder Handler, Slot); diese
    /// Funktion legt ab. Absichtlich getrennt, damit die Prüfung nicht an zwei Stellen steht — und
    /// damit sie in einer host-testbaren, abhängigkeitsfreien Datei stehen kann.
    pub fn handler_kante_setzen(&mut self, gast: usize, handler: u16) -> bool {
        if gast >= self.pds.len() || !self.pds[gast].used {
            return false;
        }
        self.pds[gast].handler_pd = handler;
        self.pds[gast].handler_bound += 1;
        true
    }

    /// **Eine Bindung zurücknehmen.** Fällt der Zähler auf 0, fällt die Kante.
    ///
    /// Ohne diesen zweiten Halbsatz bliebe eine Kante stehen, an der kein Thread mehr hängt — und
    /// das Zyklusverbot verböte danach für immer eine Bindung, die längst zulässig wäre. Ein
    /// Verbot, das aus einer Leiche folgt, ist von einem echten Verbot nicht zu unterscheiden.
    pub fn handler_kante_loesen(&mut self, gast: usize) -> bool {
        if gast >= self.pds.len() || !self.pds[gast].used || self.pds[gast].handler_bound == 0 {
            return false;
        }
        self.pds[gast].handler_bound -= 1;
        if self.pds[gast].handler_bound == 0 {
            self.pds[gast].handler_pd = KEIN_HANDLER_PD;
        }
        true
    }

    /// Wie viele Threads dieser PD sind gebunden? (Sprechprobe/Bericht.)
    pub fn handler_bound_of(&self, pd: usize) -> u32 {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].handler_bound
        } else {
            0
        }
    }

    /// **Lebt die Handler-PD noch?** Die Größe, die [`caprock_sched::redirect::weiche_syscall`]
    /// als `handler_lebt` bekommt.
    ///
    /// **Was diese Funktion NICHT prüft** — und das gehört hierhin, nicht in einen Bericht: dass
    /// die Handler-*Cap* noch im Cspace der Handler-PD liegt. Geprüft wird, ob die PD existiert
    /// und nicht stillgelegt ist. Eine gelöschte Cap bei lebender PD führt heute dazu, dass der
    /// Handler nicht mehr `RECV`t — der Gast wartet dann in `BlockReasons::HANDLER`, statt zu
    /// faulten. **Das ist eine offene Lücke** (todo.md Z26/A3), und sie steht hier, weil ein
    /// Prüfer, der die falsche Größe liest, den Fehler strukturell nicht sehen kann.
    pub fn handler_lebt(&self, handler_pd: u16) -> bool {
        let i = handler_pd as usize;
        i < self.pds.len() && self.pds[i].used && !self.pds[i].quiescing
    }

    /// Die lokalen Cap-Slots einer PD (für den Teardown: jeden installierten Cap löschen). Gibt
    /// die belegten `(slot, CapPtr)` der **ersten [`NCAPS`] Plaetze** zurück.
    ///
    /// Legacy-Fenster, und das steht hier statt in einem Aufrufer: die Signatur
    /// `[Option<CapPtr>; NCAPS]` ist an Report- und Teardown-Stellen im Kernel
    /// festverdrahtet. PDs mit mehr als 16 Plaetzen melden hier ihre ersten 16 — wer alle
    /// braucht, laeuft ueber [`PdTable::cspace_len_von`] + [`PdTable::cap_ptr_at`]. Was
    /// das Fenster verschweigt, verschweigt es laut: die Laenge steht daneben.
    pub fn caps_of(&self, pd: usize) -> [Option<CapPtr>; NCAPS] {
        let mut o = [None; NCAPS];
        if pd < self.pds.len() && self.pds[pd].used {
            let start = self.pds[pd].cspace_start as usize;
            let pool = self.cspace_pool.as_slice();
            let n = (self.pds[pd].cspace_len as usize).min(NCAPS);
            for (dst, src) in o.iter_mut().zip(pool.iter().skip(start).take(n)) {
                *dst = *src;
            }
        }
        o
    }

    /// Die (unveränderliche) Domäne einer PD.
    /// **Z23 S1: die Tore dieser PD schliessen oder öffnen.** Rückgabe: hat sich etwas geändert?
    ///
    /// Idempotent. `false` heisst „war schon so" **oder** „PD gibt es nicht" — der Aufrufer
    /// unterscheidet das über [`is_quiescing`](Self::is_quiescing), das für eine unbekannte PD
    /// ebenfalls `false` gibt. Zwei Bedeutungen in einem `bool` wären hier harmlos, aber der
    /// nächste Leser weiss es nicht — deshalb steht es da.
    pub fn set_quiescing(&mut self, pd: usize, an: bool) -> bool {
        if pd >= self.pds.len() || !self.pds[pd].used || self.pds[pd].quiescing == an {
            return false;
        }
        self.pds[pd].quiescing = an;
        true
    }

    /// Sind die Tore dieser PD zu? Unbekannte PD → `false` (sie kann nichts anfangen).
    pub fn is_quiescing(&self, pd: usize) -> bool {
        pd < self.pds.len() && self.pds[pd].used && self.pds[pd].quiescing
    }

    /// Wie viele PDs gerade stillgelegt werden — für den Bericht. `0` ist von „es gibt keine PDs"
    /// nicht zu unterscheiden, deshalb wird die Gesamtzahl daneben genannt.
    pub fn quiescing_count(&self) -> usize {
        self.pds.iter().filter(|p| p.used && p.quiescing).count()
    }

    pub fn domain_of(&self, pd: usize) -> Option<Domain> {
        if pd < self.pds.len() && self.pds[pd].used {
            Some(self.pds[pd].domain)
        } else {
            None
        }
    }

    /// Den unveränderlichen Trusted-Partner einer HardwareLand-PD (PD-Index).
    pub fn partner_of(&self, pd: usize) -> Option<usize> {
        if pd < self.pds.len() {
            self.pds[pd].partner.map(|p| p as usize)
        } else {
            None
        }
    }

    /// Eine **HardwareLand-Backend-PD** mit unveränderlicher Partner-Bindung + dediziertem
    /// Kanal anlegen (ext-22, P3). `partner` = Trusted-SAS-PD (1:N erlaubt — mehrere Backends
    /// je Partner; N:1 ist strukturell ausgeschlossen, da `partner` ein einzelnes Feld ist).
    /// `ep`/`ntfn` = der einzige Kanal, über den dieses Backend kommunizieren darf. Alle
    /// Felder sind nach der Erzeugung **unveränderlich** (kein Setter).
    pub fn create_hardware_backend(
        &mut self,
        partner: usize,
        backend_id: u16,
        ep: u32,
        ntfn: u32,
    ) -> Option<usize> {
        if partner >= self.pds.len() {
            return None;
        }
        let i = self.freien_slot_suchen()?;
        // Wie im Standard-Create: wiederbelebten Slot zuerst entketten
        // (Doppelvergabe-Schutz, s. `Vergabe::entketten`).
        let alt = self.cspace_vergabe.entketten(&mut self.pds, i);
        // Der Cspace-Lauf kommt aus dem Pool wie bei jeder anderen PD (Standardgroesse):
        // ohne ihn haette das Backend null Plaetze und hielte keine einzige Cap. Der
        // Budget-Vorrat wird dabei **nicht** gebucht — das ist die uebernommene Eigenheit
        // dieses Pfades (er buchte nie), keine neue: wer sie behebt, verschiebt die
        // Vorratszahlen im Bericht und fasst damit die Abnahme an.
        let pool_len = self.cspace_pool.len().min(u32::MAX as usize) as u32;
        let (start, len) = match alt {
            Some(a) if a.len >= NCAPS as u32 => (a.start, a.len),
            _ => self
                .cspace_vergabe
                .belegen(&mut self.pds, pool_len, NCAPS as u32)
                .ok()?,
        };
        let epoch = self.pds[i].epoch;
        self.pds[i] = Pd {
            used: true,
            domain: Domain::HardwareLand,
            partner: Some(partner as u16),
            backend_id,
            chan_ep: ep,
            chan_ntfn: ntfn,
            epoch,
            cspace_start: start,
            cspace_len: len,
            freilauf_next: 0,
            ..Pd::EMPTY
        };
        Some(i)
    }

    /// Die stabile Backend-Id einer PD (0 = kein Backend).
    pub fn backend_id_of(&self, pd: usize) -> u16 {
        if pd < self.pds.len() {
            self.pds[pd].backend_id
        } else {
            0
        }
    }

    /// Die Interrupt-Notification einer HardwareLand-PD (`u32::MAX` = keine).
    pub fn irq_ntfn_of(&self, pd: usize) -> u32 {
        if pd < self.pds.len() {
            self.pds[pd].irq_ntfn
        } else {
            u32::MAX
        }
    }

    /// **Die Interrupt-Notification einer PD eintragen** (B4) — nur der Kernel ruft das, und nur
    /// mit einem Objekt, das er selbst fuer diese PD gepraegt hat.
    ///
    /// Gibt `false`, wenn die PD nicht existiert. **Ueberschreiben ist zulaessig**: beim
    /// Hot-Reload bekommt die Nachfolgefassung dieselbe Id, und eine Regel, die das verboete,
    /// machte den Austausch unmoeglich.
    pub fn set_irq_ntfn(&mut self, pd: usize, id: u32) -> bool {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].irq_ntfn = id;
            true
        } else {
            false
        }
    }

    /// Die Kanal-IDs (Endpoint, Notification) einer HardwareLand-Backend-PD.
    pub fn chan_of(&self, pd: usize) -> (u32, u32) {
        if pd < self.pds.len() {
            (self.pds[pd].chan_ep, self.pds[pd].chan_ntfn)
        } else {
            (u32::MAX, u32::MAX)
        }
    }

    /// Wie viele PD-Slots gerade belegt sind — der **Füllstand** des Vorrats.
    pub fn used_count(&self) -> usize {
        self.pds.iter().filter(|p| p.used).count()
    }

    /// Ist `pd` belegt?
    pub fn is_used(&self, pd: usize) -> bool {
        pd < self.pds.len() && self.pds[pd].used
    }

    /// Der gebundene Thread einer PD (für die Domäne↔VSpace-Isolationsprüfung).
    pub fn thread_of(&self, pd: usize) -> Option<ThreadId> {
        if pd < self.pds.len() {
            self.pds[pd].thread
        } else {
            None
        }
    }

    /// Anzahl der PD-Slots (für Audit-Iteration) — die **tatsächliche**, beim Boot zugewiesene.
    ///
    /// War eine Konstante (`NPDS`). Seit A-3.4 Teil 3 ist sie die Länge der angehängten Tabelle:
    /// eine Audit-Schleife, die über die Konstante läuft statt über die reale Kapazität, würde
    /// vor dem `attach` ins Leere greifen und danach womöglich an der Tabelle vorbei.
    pub fn capacity(&self) -> usize {
        self.pds.len()
    }

    /// Der Tabelle ihren Speicher geben (einmalig, beim Boot — vor der ersten PD).
    ///
    /// # Safety
    /// Vertrag von [`Slab::attach`]: exklusiver, ausgerichteter, dauerhafter Speicher für
    /// mindestens `len` Elemente, genau einmal.
    pub unsafe fn attach(&mut self, ptr: *mut Pd, len: usize) {
        // SAFETY: an den Aufrufer durchgereicht (s. Funktionsdoku).
        unsafe { self.pds.attach(ptr, len, |_| Pd::EMPTY) };
    }

    /// **Dem Cspace-Pool seinen Speicher geben** (TODO0 K1c) — einmalig beim Boot, vor der
    /// ersten PD, aus Boot-RAM (Vorbild A-3.4: 80256 Slots aus Boot-RAM statt `.bss`).
    ///
    /// Richtgroesse: `NPDS * STANDARD_PLAETZE` Plaetze kosten genau das, was die alten
    /// Inline-Arrays kosteten — jede groessere PD nimmt sich ihren Mehrbedarf aus
    /// demselben Topf. Ohne diesen Aufruf ist jede Erzeugung eine benannte Absage
    /// ([`CspaceAbweisung::PoolErschoepft`]), kein Absturz: ein vergessener Pool faellt
    /// als sauberer Fehler auf, nicht als stiller Fehlzugriff.
    ///
    /// # Safety
    /// Vertrag von [`Slab::attach`]: exklusiver, ausgerichteter, dauerhafter Speicher für
    /// mindestens `len` Elemente, genau einmal.
    pub unsafe fn attach_cspace_pool(&mut self, ptr: *mut Option<CapPtr>, len: usize) {
        assert!(
            self.cspace_pool.is_empty(),
            "PdTable::attach_cspace_pool zweimal gerufen"
        );
        // SAFETY: an den Aufrufer durchgereicht (s. Funktionsdoku).
        unsafe { self.cspace_pool.attach(ptr, len, |_| None) };
    }

    /// Cspace-Plaetze dieser PD — die **tatsaechliche** Lauf-Laenge, nicht die Vorgabe.
    /// `0` fuer unbekannte/freie PDs (fail-closed: jede Installation weist die Schranke
    /// ab, statt einer PD, die es nicht gibt, Plaetze zuzugestehen).
    pub fn cspace_len_von(&self, pd: usize) -> usize {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].cspace_len as usize
        } else {
            0
        }
    }

    /// Belegte Pool-Plaetze insgesamt — der Fuellstand des Cspace-Pools (fuer den Bericht:
    /// ein Vorrat ohne Fuellstandsanzeige spricht erst, wenn es zu spaet ist).
    pub fn cspace_pool_belegt(&self) -> usize {
        self.cspace_pool.as_slice().iter().filter(|s| s.is_some()).count()
    }

    /// **Der Rückwärts-Tabelle ihren Speicher geben** (Z22 P2 / C4) — einmalig beim Boot,
    /// vor der ersten Bindung. `len` = Thread-Kapazität des Systems (`ThreadId::slot()` ist
    /// darin der Index).
    ///
    /// # Safety
    /// Vertrag von [`Slab::attach`]: exklusiver, ausgerichteter, dauerhafter Speicher für
    /// mindestens `len` Elemente, genau einmal.
    pub unsafe fn attach_owner(&mut self, ptr: *mut ThreadOwner, len: usize) {
        // SAFETY: an den Aufrufer durchgereicht (s. Funktionsdoku).
        unsafe { self.owner.attach(ptr, len, |_| ThreadOwner::EMPTY) };
    }

    /// **Einen Thread an eine PD binden — mehrere Threads je PD sind erlaubt** (Z22 P2).
    ///
    /// Bis hierher hiess das Feld `Pd::thread` und trug genau einen Thread; eine zweite Bindung
    /// hat die erste **überschrieben**, und der erste Thread verlor damit lautlos seinen ganzen
    /// Cspace (`pd_of` fand ihn nicht mehr → `ERR_NOPD` bei jedem Syscall). Das ist dieselbe
    /// Form wie „eine Ablage je ROLLE" bei `CLIENT_NTFN`: eine Zelle für etwas, das es mehrfach
    /// gibt.
    pub fn bind_thread(&mut self, pd: usize, thread: ThreadId) {
        if pd >= self.pds.len() {
            return;
        }
        let s = thread.slot();
        if s < self.owner.len() {
            // Ein wiederverwendeter Thread-Slot kann noch die Bindung seines Vorgängers tragen —
            // dessen PD verliert dann einen Thread aus der Zählung.
            let alt = self.owner[s];
            if alt.pd1 != 0 {
                let ap = (alt.pd1 - 1) as usize;
                if ap < self.pds.len()
                    && self.pds[ap].epoch == alt.epoch
                    && self.pds[ap].nthreads > 0
                {
                    self.pds[ap].nthreads -= 1;
                }
            }
            self.owner[s] = ThreadOwner {
                tid: thread.to_raw(),
                pd1: (pd + 1) as u32,
                epoch: self.pds[pd].epoch,
            };
            self.pds[pd].nthreads += 1;
        } else {
            // Kein stiller Rückfall: ohne Tabelle ist die Auflösung wieder linear, und das
            // gehört in den Bericht statt in eine Vermutung.
            PD_OWNER_UNANGEHAENGT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        // Der Vertreter bleibt der ERSTE Thread (s. `Pd::thread`).
        if self.pds[pd].thread.is_none() {
            self.pds[pd].thread = Some(thread);
        }
    }

    /// Die PD eines Threads — öffentlich für Prüfer und Bericht, und **derselbe Weg**, den auch
    /// der Syscall-Dispatch nimmt. Ein Prüfer, der die Größe nachrechnet statt sie zu lesen,
    /// prüft eine zweite Wirklichkeit (s. `iova_window_clear_of_msi` in der Fallenliste).
    pub fn pd_of_thread(&self, thread: ThreadId) -> Option<usize> {
        self.pd_of(thread)
    }

    /// **Gilt `praedikat` für IRGENDEINEN Thread dieser PD?** (Z22 P2)
    ///
    /// Der Ersatz für „nimm `thread_of(pd)` und prüfe den": mit mehreren Threads je PD ist der
    /// erste keine Auskunft über die PD. Läuft über den Rückwärts-Index, ist also O(Threads)
    /// und **nicht** O(PDs × Threads).
    ///
    /// Solange die Rückwärts-Tabelle keinen Speicher hat, bleibt nur der Vertreter — das ist
    /// dann genau das alte Verhalten und keine stille Verschlechterung.
    pub fn any_thread(&self, pd: usize, praedikat: &dyn Fn(ThreadId) -> bool) -> bool {
        if pd >= self.pds.len() || !self.pds[pd].used {
            return false;
        }
        if self.owner.is_empty() {
            return self.pds[pd].thread.is_some_and(praedikat);
        }
        let (pd1, epoch) = ((pd + 1) as u32, self.pds[pd].epoch);
        self.owner
            .iter()
            .any(|e| e.pd1 == pd1 && e.epoch == epoch && praedikat(ThreadId::from_raw(e.tid)))
    }

    /// **Irgendein lebender Thread dieser PD** (Z6b) — fuer die VSpace-Aufloesung.
    ///
    /// „Irgendeiner" ist hier ausreichend und die Begruendung gehoert dazu: alle Threads einer PD
    /// teilen ihre VSpace, das ist die **Definition** einer PD. Waere das je nicht mehr so, waere
    /// diese Funktion die erste, die falsch wird — deshalb steht die Herleitung hier und nicht in
    /// drei Aufrufern.
    pub fn any_thread_of(&self, pd: usize) -> Option<ThreadId> {
        if pd >= self.pds.len() || !self.pds[pd].used {
            return None;
        }
        if self.owner.is_empty() {
            return self.pds[pd].thread;
        }
        let (pd1, epoch) = ((pd + 1) as u32, self.pds[pd].epoch);
        self.owner
            .iter()
            .find(|e| e.pd1 == pd1 && e.epoch == epoch)
            .map(|e| ThreadId::from_raw(e.tid))
    }

    /// Wie viele Threads an dieser PD hängen (Z22 P2). `0` für eine unbekannte/freie PD.
    pub fn thread_count(&self, pd: usize) -> u32 {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].nthreads
        } else {
            0
        }
    }

    /// Der grösste je erreichte `nthreads` über alle PDs — Auskunft für den Bericht: ohne ihn
    /// wäre „mehrere Threads je PD" von „einer, wie immer" nicht zu unterscheiden.
    pub fn max_threads_per_pd(&self) -> u32 {
        self.pds.iter().map(|p| p.nthreads).max().unwrap_or(0)
    }

    /// Eine globale Capability in den Cspace einer PD an `slot` eintragen.
    ///
    /// Die Schranke ist die **Lauf-Laenge dieser PD**, nicht mehr die Konstante: Slots
    /// jenseits des Laufs werden still nicht eingetragen (derselbe Vertrag wie frueher
    /// fuer Slots `>= NCAPS`). Budget- und Policy-Pruefung stehen darüber in
    /// [`Caps::install_cap_checked`]; wer direkt ruft (Rollback, `CMOVE`), hat sie selbst.
    pub fn install_cap(&mut self, pd: usize, slot: usize, cap: CapPtr) {
        if pd < self.pds.len() && slot < self.pds[pd].cspace_len as usize {
            let idx = self.pds[pd].cspace_start as usize + slot;
            if let Some(s) = self.cspace_pool.as_mut_slice().get_mut(idx) {
                *s = Some(cap);
            }
        }
    }

    /// Einen Cap-Slot einer PD leeren (Autorität entziehen — Hot-Reload).
    /// Den **Empfangs-Slot** einer PD setzen (A-3.2). `false`, wenn PD oder Slot ungültig sind.
    ///
    /// Die Slot-Schranke ist die Lauf-Laenge dieser PD: eine grosse PD darf ihren Empfang
    /// auch jenseits von Slot 15 legen, eine kleine nicht jenseits ihres Laufs.
    pub fn set_recv_slot(&mut self, pd: usize, slot: usize) -> bool {
        if pd < self.pds.len() && self.pds[pd].used && slot < self.pds[pd].cspace_len as usize {
            self.pds[pd].recv_slot = slot;
            true
        } else {
            false
        }
    }

    /// Der Empfangs-Slot einer PD (Vorgabe: [`caprock_abi::GRANT_RECV_SLOT`]).
    pub fn recv_slot(&self, pd: usize) -> usize {
        if pd < self.pds.len() && self.pds[pd].used {
            self.pds[pd].recv_slot
        } else {
            caprock_abi::GRANT_RECV_SLOT
        }
    }

    /// Anzahl der Cap-Slots je PD-Cspace (Adressraum der lokalen Slot-Indizes).
    ///
    /// Legacy-Vorgabe ([`NCAPS`]) fuer Stellen, die keine PD kennen (Dispatch-Grenzen,
    /// bevor die PD aufgeloest ist). Sobald die PD bekannt ist, gilt ihre Lauf-Laenge
    /// ([`PdTable::cspace_len_von`]) — eine Konstante, die neben der Tabelle her existiert,
    /// laeuft beim naechsten Drehen still auseinander (dieselbe Begruendung wie
    /// `eps.len()` statt `NENDPOINTS` in A-3.4 Teil 4).
    pub const fn caps_per_pd() -> usize {
        NCAPS
    }

    pub fn clear_cap(&mut self, pd: usize, slot: usize) {
        if pd < self.pds.len() && slot < self.pds[pd].cspace_len as usize {
            let idx = self.pds[pd].cspace_start as usize + slot;
            if let Some(s) = self.cspace_pool.as_mut_slice().get_mut(idx) {
                *s = None;
            }
        }
    }

    /// **Die PD eines Threads — O(1) über den Rückwärts-Index** (Z22 P2 / C4).
    ///
    /// Diese Funktion läuft bei **jedem** Syscall (der Dispatch löst damit die aufrufende PD auf).
    /// Bis Z22 P2 war sie ein linearer Scan über die ganze PD-Tabelle, also O(`NPDS`) = 10 000
    /// Vergleiche je Syscall. In der C4-Liste stand sie nicht.
    ///
    /// Der Rückfall auf den Scan bleibt, aber **gezählt**: eine Tabelle, die nicht angehängt
    /// wurde, macht das System langsam und nicht falsch — und genau das soll man sehen können,
    /// statt es zu vermuten.
    fn pd_of(&self, thread: ThreadId) -> Option<usize> {
        use core::sync::atomic::Ordering::Relaxed;
        PD_OF_CALLS.fetch_add(1, Relaxed);
        let s = thread.slot();
        // **Ein Slot JENSEITS der Tabelle ist keine Frage an die Tabelle, sondern die Antwort.**
        // Solange die Rückwärts-Tabelle hängt, kann ein solcher Thread nirgends gebunden sein —
        // `bind_thread` hätte ihn in `PD_OWNER_UNANGEHAENGT` gezählt und nichts eingetragen.
        // Ihn trotzdem linear zu suchen hiesse, den langsamsten Weg ausgerechnet für die
        // **Angriffs**eingabe zu nehmen: die Park-Sonde ruft `UNPARK 0xDEAD_BEEF`, und jeder
        // EL0-Thread kann dasselbe. Ein O(n)-Pfad, den ein Mandant per Syscall auslöst, ist
        // genau die Form, gegen die C4 gebaut wird.
        if !self.owner.is_empty() {
            if s >= self.owner.len() {
                return None;
            }
            let e = self.owner[s];
            if e.pd1 == 0 || e.tid != thread.to_raw() {
                return None;
            }
            let pd = (e.pd1 - 1) as usize;
            // Drei Bedingungen, und keine ist entbehrlich: der Eintrag muss diesem Thread
            // gehören (Generation der ThreadId), die PD muss belegt sein, und es muss
            // **dieselbe Belegung** sein wie bei der Bindung (Generation der PD).
            if pd < self.pds.len() && self.pds[pd].used && self.pds[pd].epoch == e.epoch {
                return Some(pd);
            }
            return None;
        }
        let mut iter = 0u64;
        let r = self.pds.iter().position(|p| {
            iter += 1;
            p.used && p.thread == Some(thread)
        });
        PD_OF_SCAN_ITER.fetch_add(iter, Relaxed);
        PD_OF_SCAN_CALLS.fetch_add(1, Relaxed);
        let _ = PD_OF_SCAN_ERSTER_SLOT.compare_exchange(0, s as u64 + 1, Relaxed, Relaxed);
        r
    }

    /// Länge der Rückwärts-Tabelle (0 = nie angehängt). Gehört in den Bericht: ohne sie ist
    /// „ein Aufruf lief linear" nicht von „die Tabelle fehlt ganz" zu unterscheiden.
    pub fn owner_len(&self) -> usize {
        self.owner.len()
    }

    /// Anzahl der aktuell belegten Cap-Slots einer PD (Verbrauch gegen [`CAP_BUDGET_PER_PD`]).
    pub fn cap_count(&self, pd: usize) -> usize {
        if pd < self.pds.len() && self.pds[pd].used {
            let start = self.pds[pd].cspace_start as usize;
            let len = self.pds[pd].cspace_len as usize;
            self.cspace_pool
                .as_slice()
                .iter()
                .skip(start)
                .take(len)
                .filter(|c| c.is_some())
                .count()
        } else {
            0
        }
    }

    /// **Wie [`PdTable::cap_at`], aber von aussen** (Z6b).
    ///
    /// Der Kernel braucht die Aufloesung `Slot -> Cap` fuer die Debug-Syscalls, und zwar in
    /// **derselben** kritischen Sektion, in der er danach `SCHEDS` nimmt (`CAPS` ist R0,
    /// `SCHEDS` ist R2 — die Schachtelung ist aufsteigend und damit erlaubt). Die private
    /// Fassung liegt in dieser Crate; sie hier nochmals hinzuschreiben waere eine zweite
    /// Spectre-Haertung, die beim naechsten Mal nur an einer der beiden Stellen nachgezogen wird.
    pub fn cap_ptr_at(&self, pd: usize, slot: usize) -> Option<CapPtr> {
        self.cap_at(pd, slot)
    }

    /// Der erste freie Cap-Slot einer PD, oder `None` (Z6b — `DEBUG_ATTACH` legt die abgeleitete
    /// Cap ab). **`None` heisst „kein Platz" und wird als solches gemeldet**, nicht durch
    /// Ueberschreiben eines belegten Slots aufgeloest: das waere ein Cap-Verlust, den der
    /// Aufrufer nicht angeordnet hat (dieselbe Regel wie bei `CMOVE`).
    ///
    /// Sucht ueber die Lauf-Laenge dieser PD — eine grosse PD findet freie Plaetze auch
    /// jenseits von Slot 15.
    pub fn free_cap_slot(&self, pd: usize) -> Option<usize> {
        (0..self.cspace_len_von(pd)).find(|&s| self.cap_at(pd, s).is_none())
    }

    /// Die PD eines Threads — von aussen (Z6b).
    pub fn pd_of_thread_pub(&self, thread: ThreadId) -> Option<usize> {
        self.pd_of(thread)
    }

    fn cap_at(&self, pd: usize, slot: usize) -> Option<CapPtr> {
        let len = self.cspace_len_von(pd);
        if slot < len {
            // Spectre-v1-Härtung: `slot` stammt bei jedem Syscall aus einem EL0-Register.
            // Die Verzweigung oben schützt nur den architektonischen Pfad — eine falsch
            // vorhergesagte Verzweigung könnte den Zugriff spekulativ mit einem beliebigen
            // Index ausführen und den Treffer im Cache hinterlassen. Die Maske macht den
            // Index zusätzlich datenabhängig gültig. `len >= 1` folgt aus `slot < len`.
            let start = self.pds[pd].cspace_start as usize;
            self.cspace_pool
                .as_slice()
                .get(start + array_index_nospec(slot, len))
                .copied()
                .flatten()
        } else {
            None
        }
    }
}

/// **Ausgang der Übergabe eines `SYS_LOAD` an den Verifiziererthread** (C8).
///
/// Vor C8 lud der Callback synchron und gab `Option<usize>` — die Signaturprüfung (Ed25519 +
/// SHA-2) lief damit auf dem Kernel-Stack des *aufrufenden* Threads. Gemessen füllte sie ihn zu
/// 73 %, und **jeder** Thread musste die 16 KiB dafür vorhalten, obwohl nur dieser eine Syscall
/// sie braucht.
///
/// Die drei Ausgänge sind mit Absicht unterscheidbar. Ein Aufrufer muss auf sie verschieden
/// reagieren: bei `Ausgelastet` lohnt ein zweiter Versuch, bei `KeinVerifizierer` nie.
pub enum LadeUebergabe {
    /// Der Auftrag liegt beim Verifizierer, der Aufrufer ist **blockiert** und weggewechselt;
    /// der Wert ist der Stackpointer des nächsten Threads. Das Ergebnis schreibt der Verifizierer
    /// später in den Frame des Aufrufers, **bevor** er dessen Wartegrund entfernt.
    Uebergeben(usize),
    /// Die Auftragsschlange ist voll — eine **Lastaussage**. Der Aufrufer wird **nicht** blockiert
    /// und bekommt [`result::ERR_LOAD_BUSY`].
    Ausgelastet,
    /// Es gibt keinen Verifiziererthread (Aufbaufehler). Der Aufrufer bekommt
    /// [`result::ERR_SERVER_GONE`] — „gibt es nicht", nicht „gerade voll".
    KeinVerifizierer,
}

/// Delegationsliste einsammeln (`LOAD`/`LOAD_IMAGE`): `(src,dst)`-Paare ableiten, bei
/// jedem Fehler die bereits abgeleiteten Kopien zuruecknehmen und den benannten Code liefern.
///
/// Steht genau einmal hier statt je einmal in beiden Armen: zwei Stellen, die dieselbe
/// Rueckabwicklung implementieren, altern auseinander — und eine halb abgebrochene
/// Delegation liesse abgeleitete Caps als verwaiste CDT-Kinder liegen (s. Kommentar im
/// `LOAD`-Arm). `Err` heisst immer „nichts angelegt, alles zurueckgenommen".
fn sammle_endow(
    caps: &RwSpinLock<Caps>,
    pd: usize,
    liste: u64,
    anzahl: usize,
    delete_cap: fn(CapPtr) -> Result<(), u64>,
) -> Result<[Option<(usize, CapPtr)>; caprock_abi::LOAD_MAX_DELEGATES], u64> {
    // **Die Ruecknahme steht VOR der Sperre**, und das ist kein Stil: `delete_cap`
    // finalisiert (Speicher zurueck, DMA abhaengen) und nimmt `CAPS` dafuer selbst. Sie
    // unter gehaltener Schreibsperre zu rufen waere ein Selbst-Deadlock -- dieselbe Form
    // wie `match lock() { .. None => lock() }` aus der Fallenliste.
    let zurueck = |bisher: &[Option<(usize, CapPtr)>], bis: usize| {
        for e in bisher.iter().take(bis).flatten() {
            let _ = delete_cap(e.1);
        }
    };
    // Fail-closed an drei Stellen, und jede hat einen eigenen Grund:
    //   * mehr Paare als [`LOAD_MAX_DELEGATES`](caprock_abi::LOAD_MAX_DELEGATES) -> die
    //     Liste kann gar nicht so lang sein, das ist ein Aufruffehler und kein
    //     Ressourcenmangel;
    //   * ein Quell-Slot, den der Aufrufer nicht haelt -> er delegiert, was er nicht hat;
    //   * **zwei Paare auf denselben Ziel-Slot** -> welcher dort landet, koennte niemand
    //     sagen. Genau die Sorte stiller Wahl, gegen die A-5.4 den `service_id`
    //     eingefuehrt hat.
    if anzahl > caprock_abi::LOAD_MAX_DELEGATES {
        return Err(result::ERR_BADCAP);
    }
    let mut gesammelt: [Option<(usize, CapPtr)>; caprock_abi::LOAD_MAX_DELEGATES] =
        [None; caprock_abi::LOAD_MAX_DELEGATES];
    for k in 0..anzahl {
        let (src, dst) = caprock_abi::delegate_unpack(((liste >> (8 * k)) & 0xff) as u8);
        // Zwei Paare auf denselben Ziel-Slot: welcher dort landet, koennte niemand sagen.
        if gesammelt.iter().flatten().any(|&(z, _)| z == dst) {
            zurueck(&gesammelt, k);
            return Err(result::ERR_BADCAP);
        }
        let abgeleitet = {
            let mut g = caps.write();
            let Some(c) = g.pds.cap_at(pd, src) else {
                drop(g);
                zurueck(&gesammelt, k);
                return Err(result::ERR_BADCAP);
            };
            let Some((_, r, _)) = g.cspace.lookup(c) else {
                drop(g);
                zurueck(&gesammelt, k);
                return Err(result::ERR_BADCAP);
            };
            g.cspace.copy(c, r)
        }; // CAPS je Paar kurz gehalten, nie ueber die Ruecknahme
        match abgeleitet {
            Ok(copy) => gesammelt[k] = Some((dst, copy)),
            Err(_) => {
                zurueck(&gesammelt, k);
                return Err(result::ERR_BADCAP);
            }
        }
    }
    Ok(gesammelt)
}

/// Cap-gesicherter Syscall-Dispatch.
///
/// Liest Syscall-Nummer (`x0`) und *lokalen* Cap-Index (`x1`) aus dem Frame,
/// löst die Capability im Cspace der aufrufenden PD auf, prüft Objekttyp und
/// Rechte und führt dann die IPC-Operation aus. Bei fehlender Cap / falschem
/// Recht wird ein Fehlercode gesetzt und (ohne Blockieren) zurückgekehrt.
///
/// Rechte: `CALL` (senden) braucht `WRITE`, `RECV`/`REPLY` (empfangen) `READ`.
/// Der Dispatch besorgt das **Locking selbst** (feinkörnig): `caps` (Cap-Auflösung
/// + -Verwaltung) ist EIN Lock, jedes Endpoint/Notification hat seinen eigenen.
/// Sperrordnung `CAPS < EPS[i]/NTFNS[i] < SCHEDS` (die `SchedOps` sperren intern je
/// Operation genau eine `SCHEDS`-Instanz). Cap-Auflösung sperrt `CAPS` kurz und gibt
/// ihn frei -> IPC auf verschiedenen Endpoints läuft danach parallel. Nur `REPLY`
/// mit Cap-Transfer hält `CAPS` über das Sperren des Endpoints (CAPS->EPS), gibt ihn
/// aber vor dem Rendezvous frei.
pub fn dispatch(
    frame: usize,
    core: usize,
    ops: &mut dyn SchedOps,
    caps: &RwSpinLock<Caps>,
    // A-3.4 Teil 4: **Slices** statt `&[_; NENDPOINTS]`. Die Schranke ist damit die
    // tatsächlich beim Boot zugewiesene Kapazität (`eps.len()`), nicht eine Konstante, die
    // neben der Tabelle her existiert und bei einer Änderung stillschweigend auseinanderläuft.
    eps: &[SpinLock<Endpoint>],
    ntfns: &[SpinLock<Notification>],
    // ext-26: `SYS_LOAD`-Callback in den kernel-spezifischen Binary-Loader. `(Archiv-Index,
    // AUFRUFER-PD, Endowment) -> neue PD-Id`. Vom Dispatch erst NACH dem Freigeben von `caps`
    // gerufen (der Loader re-lockt `CAPS`/`MEM`/`SCHEDS` selbst). `endow` = aus dem
    // Aufrufer-Cspace delegierte Caps (Slot, Cap).
    //
    // **Die Aufrufer-PD kam mit A-5.1 dazu**, und nicht aus Bequemlichkeit: ein
    // HardwareLand-Backend ist per Entwurf an einen **Partner** gebunden (ext-22, unveraenderlich,
    // 1:N). Wer es laedt, ist dieser Partner -- eine andere Antwort gibt es nicht, und sie muss
    // vom Dispatch kommen, weil nur er weiss, wer gerade syscallt.
    //
    // **Seit C8 ist der Callback eine ÜBERGABE, kein Aufruf** (2026-08-11). Er lädt nicht mehr
    // selbst, sondern reicht den Auftrag an den Verifiziererthread und **blockiert den Aufrufer**;
    // deshalb bekommt er `core` und `frame` und liefert eine [`LadeUebergabe`] statt einer PD-Id.
    // Der Grund ist gemessen: die Ed25519-/SHA-2-Verifikation lief auf dem 16-KiB-Kernel-Stack des
    // aufrufenden EL0-Threads und füllte ihn zu 73 % — jeder Thread zahlte 16 KiB für diesen einen
    // Pfad.
    load: fn(u32, usize, &[(usize, CapPtr)], usize, usize, u16, u32) -> LadeUebergabe,
    // Cap löschen (Finalisierung inkl. Speicherfreigabe/Call-Abbruch) — der Kernel sperrt darin
    // selbst `CAPS`+`MEM` und bricht finalisierte Reply-Calls ab. Nur zu rufen, wenn hier KEIN
    // Lock mehr gehalten wird. Wird für die beim Grant verdrängte Cap gebraucht (s. `grant_cap`)
    // und für `SYS_CDELETE`. **Rückgabe:** ob gelöscht wurde — `false` heißt, dass noch abgeleitete
    // Caps (CDT-Kinder) daran hängen. Für den Grant-Pfad ist das nur Telemetrie, für `CDELETE` die
    // Bedingung, unter der der Slot überhaupt geräumt werden darf.
    // K1a: der Rueckruf traegt seit 2026-08-17 den GRUND -- `ERR_HASCHILDREN` (abgeleitete Caps)
    // und `ERR_INUSE` (Stack eines lebenden Threads) sind zwei verschiedene Lagen, und ein
    // Aufrufer, der sie nicht unterscheiden kann, weiss nicht, ob Warten hilft.
    delete_cap: fn(CapPtr) -> Result<(), u64>,
    // K1a (2026-08-17): `SYS_SPAWN`. `(PD, Stack-Cap, Einsprung, Argument, Prioritaet)` ->
    // `Ok(ThreadId-Raw)` oder `Err(ABI-Code)`. Wie der `load`-Rueckruf ein **Kernel**-Aufruf:
    // die Entscheidung `check_stack` braucht Geraeteerreichbarkeit und Ueberlappung, und beides
    // weiss nur der Kernel. Der Rueckruf laeuft OHNE gehaltenes `CAPS`.
    spawn: fn(usize, CapPtr, usize, usize, u8, u64) -> Result<u64, u64>,
    // Z6b: die fuenf Debug-Syscalls. `(Nummer, Aufrufer-PD, Cap-Slot, a1, a2, a3)`.
    //
    // **Ein Rueckruf fuer alle fuenf, und die Cap-Aufloesung liegt drin** — anders als bei `spawn`,
    // wo der Dispatch die Cap aufloest und weiterreicht. Der Grund ist die Sperrung: `DEBUG_STOP`
    // muss die Cap pruefen und den Grund setzen in **derselben** kritischen Sektion, sonst passt
    // ein `revoke` dazwischen und der Zielthread traegt danach einen Grund, den niemand mehr
    // entfernen darf. Loeste der Dispatch hier auf und gaebe `CAPS` frei, waere genau dieses
    // Fenster wieder offen -- und es ist der Fehler, den man nicht sieht, weil er im Normalbetrieb
    // nie auftritt.
    debug: fn(u64, usize, usize, u64, u64, u64) -> Result<u64, u64>,
    // Stufe B / B3: die Interrupt-Bindung. `(intid, Notification-Id, Badge, Kern)`.
    //
    // **Ein Rueckruf, weil die Tabelle im Kernel liegt** -- `irq_hook` laeuft im IRQ-Kontext und
    // durchsucht sie lock-frei; sie hier zu fuehren hiesse, den heissesten Pfad des Systems an eine
    // Crate zu binden, die ihn nicht kennt. Der Dispatch loest auf und prueft die Rechte, der
    // Kernel bindet -- dieselbe Aufteilung wie bei `spawn`.
    bind_irq: fn(u32, usize, u64, usize) -> Result<(), u64>,
    // LXPD-Laufzeitpfad (`SYS_LOAD_IMAGE`): wie `load`, aber `(bild_phys, bild_len, pid,
    // AUFRUFER-PD, Endowment) -> Uebergabe`. Bild-Geometrie hat der Dispatch bereits gegen
    // die Aufrufer-Memory-Cap geprueft; der Kernel kopiert EINMAL in Staging und traut
    // danach nur der Kopie.
    load_image: fn(u64, u64, u32, usize, &[(usize, CapPtr)], usize, usize, u16, u32) -> LadeUebergabe,
) -> usize {
    let nr = frame_reg(frame, reg::SYSNO_RESULT);

    // --- Z26/A3: DIE WEICHE ------------------------------------------------------------------
    //
    // **Ganz oben, vor jedem Sonderfall** — und das ist die tragende Aussage des Primitivs: eine
    // gebundene PD erreicht den Caprock-Kernel **gar nicht mehr**. Kein `YIELD`, kein `EXIT`,
    // kein `SETHANDLER`. Insbesondere kann sich ein gebundener Thread nicht selbst entbinden;
    // rückgängig macht es, wer die Tcb-Cap hält, und das ist per Entwurf eine dritte Partei.
    //
    // Stünde die Weiche weiter unten, wäre jeder Syscall darüber ein **Loch** — und ein Loch,
    // das man einer Persönlichkeit nicht ansieht: sie bekäme genau die Aufrufe nicht zu sehen,
    // die der Kernel vorher schon abgefangen hat.
    //
    // **Kosten auf dem unbelasteten Pfad:** ein `Option`-Lesen am laufenden TCB je Syscall
    // (`current_handler`), also eine Sperrung und ein Vergleich. Kein Cap-Lookup, keine
    // Tabellensuche.
    if let Some(bindung) = ops.current_handler(core) {
        let lebt = handler_lebt(caps, bindung.handler_pd);
        return match redirect::weiche_syscall(Some(bindung), lebt) {
            // Unerreichbar bei `Some(..)` mit Syscall-Handler; s. den Kreuzprodukt-Test
            // `bindung_vorhanden_heisst_niemals_kernel`. Ein gebundener Thread OHNE
            // Syscall-Handler (nur Fault) landet hier und wird regulär bearbeitet -- das ist die
            // halbe Bindung, und sie ist gewollt (Debugger/Speicherserver ohne Persönlichkeit).
            redirect::Weiche::Kernel => dispatch_nativ(
                frame, core, ops, caps, eps, ntfns, load, delete_cap, spawn, debug, bind_irq,
                load_image, nr,
            ),
            redirect::Weiche::Fault(code) => {
                // **Kein Rückfall auf die native ABI.** Aus dem Entzug einer Cap darf keine
                // Beförderung werden. Der Gast bekommt den Code und wird blockiert -- er läuft
                // NICHT weiter, denn sein Kernel ist weg und niemand wird ihm antworten.
                frame_set_reg(frame, reg::SYSNO_RESULT, code);
                ops.block_for_handler(core, frame)
            }
            redirect::Weiche::Handler { ep, slot, sidecar } => zustellen(
                frame, core, ops, eps, ep, slot, sidecar, redirect::Anlass::Syscall, 0,
            ),
        };
    }
    dispatch_nativ(
        frame, core, ops, caps, eps, ntfns, load, delete_cap, spawn, debug, bind_irq, load_image,
        nr,
    )
}

/// **Lebt die Handler-PD?** — die Größe, die die Weiche als `handler_lebt` liest.
fn handler_lebt(caps: &RwSpinLock<Caps>, handler_pd: u16) -> bool {
    caps.read().pds.handler_lebt(handler_pd)
}

/// **Die Weiche für einen FAULT** (Z26/A3) — der Einstieg aus dem architekturabhängigen
/// Fault-Hook des Kernels.
///
/// Gibt `None` zurück, wenn der Kernel weitermachen soll (keine Fault-Bindung) — dann läuft der
/// native Pfad: den Thread beenden. `Some(next)` heisst „zugestellt oder benannt abgewiesen, der
/// Thread ist blockiert".
///
/// **Warum das hier steht und nicht im Kernel:** die Sperrordnung ist `CAPS < EPS < SCHEDS`. Der
/// Fault-Hook hält bei seiner Diagnose den `SCHEDS`-Lock; würde er von dort aus zustellen, nähme
/// er `EPS` unter `SCHEDS` und drehte die Ordnung um. Diese Funktion wird deshalb **vor** dem
/// Diagnoseblock gerufen, mit allen Locks frei.
pub fn fault_dispatch(
    frame: usize,
    core: usize,
    ops: &mut dyn SchedOps,
    caps: &RwSpinLock<Caps>,
    eps: &[SpinLock<Endpoint>],
    ec: u64,
) -> Option<usize> {
    let bindung = ops.current_handler(core)?;
    if !bindung.hat_fault() {
        return None; // halbe Bindung: nur Syscalls umgeleitet -- Faults bleiben nativ
    }
    let lebt = handler_lebt(caps, bindung.handler_pd);
    Some(match redirect::weiche_fault(Some(bindung), lebt) {
        redirect::Weiche::Kernel => return None,
        redirect::Weiche::Fault(code) => {
            // Der Fault-Handler ist weg. Der Gast wird **benannt** blockiert statt still beendet:
            // „dein Kernel ist verschwunden" und „du hast Mist gebaut" sind zwei Diagnosen, und
            // wer sie zusammenwirft, sucht die Ursache im falschen Programm.
            frame_set_reg(frame, reg::SYSNO_RESULT, code);
            ops.block_for_handler(core, frame)
        }
        redirect::Weiche::Handler { ep, slot, sidecar } => zustellen(
            frame,
            core,
            ops,
            eps,
            ep,
            slot,
            sidecar,
            redirect::Anlass::Fault,
            ec,
        ),
    })
}

/// **Eine Umleitung zustellen.**
///
/// Der Trap-Frame des Gastes gehört ins **Sidecar** (das geteilte Fenster an der Handler-Cap);
/// die Nachricht sagt nur, *welcher* Gast und *warum*. Der Grund ist eine Zahl: `MSG_WORDS` ist 4,
/// ein Trap-Frame hat 22 (x86_64) bzw. 34 (aarch64) Wörter — `rt_sigreturn` (ersetzt den ganzen
/// Frame) und `clone` (braucht einen zweiten) sind über vier Wörter strukturell unmöglich.
///
/// **Die Nutzlast steht seit dem 2026-08-13 im Sidecar** (vorher fehlte sie, s. unten). Der ganze
/// Trap-Frame wird über [`SchedOps::sidecar_ablegen`] in den Slot des Gastes gelegt, **bevor** die
/// Nachricht rausgeht — nachher wäre es ein Rennen: der Handler kann auf einem anderen Kern
/// losgelaufen sein und den Slot bereits gelesen haben.
///
/// Bis dahin stellte diese Funktion nur die **Nachricht** zu; der Handler erfuhr *dass* und *wer*,
/// aber nicht *was*. Das war ehrlich benannt und machte das Primitiv unbenutzbar.
///
/// **Schlägt das Ablegen fehl, wird nicht zugestellt.** Fail-closed, mit demselben Code wie ein
/// toter Handler: ein Gast, dessen Frame nicht im Fenster steht, bekäme sonst eine Antwort auf
/// einen Syscall, den niemand lesen konnte — und der Handler antwortete auf den Frame des
/// **vorigen** Aufrufs, der noch im Slot liegt. Weiterlaufen darf er dabei nicht (das wäre die
/// Beförderung auf die native ABI), also bleibt er blockiert.
#[allow(clippy::too_many_arguments)]
fn zustellen(
    frame: usize,
    core: usize,
    ops: &mut dyn SchedOps,
    eps: &[SpinLock<Endpoint>],
    ep: u32,
    slot: u16,
    sidecar: u64,
    anlass: redirect::Anlass,
    code: u64,
) -> usize {
    let ep_i = ep as usize;
    if ep_i >= eps.len() {
        // Der Endpoint ist weg -> derselbe Fall wie ein toter Handler, und **dieselbe** Antwort.
        // Nicht `ERR_BADCAP`: der Gast hat keine Cap benutzt, er hat einen Syscall gemacht.
        frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_HANDLER_GONE);
        return ops.block_for_handler(core, frame);
    }
    // **Zuerst der Frame, dann die Nachricht** — und zwar bevor die Registerfelder unten mit der
    // Umleitungsnachricht überschrieben werden. Andersherum läge im Sidecar nicht der Frame des
    // Gastes, sondern die Nachricht an den Handler: der Gast sähe seine eigenen Argumente nie
    // wieder, und `x1`..`x5` kämen beim Zurückschreiben als Müll bei ihm an.
    if !ops.sidecar_ablegen(frame, sidecar, slot, anlass as u64, code) {
        frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_HANDLER_GONE);
        return ops.block_for_handler(core, frame);
    }
    let guest = ops.current_id(core);
    frame_set_reg(frame, caprock_abi::redirect_msg::SLOT, slot as u64);
    frame_set_reg(frame, caprock_abi::redirect_msg::GUEST_TID, guest.to_raw());
    frame_set_reg(frame, caprock_abi::redirect_msg::ANLASS, anlass as u64);
    frame_set_reg(frame, caprock_abi::redirect_msg::CODE, code);
    // --- Der Grund kommt VOR die Zustellung, und das ist kein Stilfrage ----------------------
    //
    // Ein Gast, dessen Syscall beim Handler liegt, trägt **zwei** Gründe: `IPC` (setzt `call`)
    // und `HANDLER`. Der `reply` des Handlers entfernt `IPC`, `handler_reply` entfernt
    // `HANDLER`, und lauffähig wird der Gast erst bei **leerer Menge** (Z24). Ohne den zweiten
    // Grund liefe er los, sobald die IPC-Antwort da ist — also **bevor** sein Frame aus dem
    // Sidecar zurückgeschrieben ist. Genau die halbe Ausführung, die Z26/Nachtrag 3 vorhersagt.
    //
    // **Und die Reihenfolge ist der Punkt.** Nachher wäre ein Rennen: im kernübergreifenden
    // Zweig ruft `call` erst `ops.unblock(server)` (mit IPI!) und dann `block_current` — der
    // Handler kann auf seinem Kern **losgelaufen sein und geantwortet haben**, bevor wir
    // zurückkommen. `handler_reply` liefe dann vor `mark_handler_wait`, entfernte einen Grund,
    // den es noch nicht gibt, und der Gast hinge **für immer** — mit jedem Prüfer auf grün.
    // Wörtlich das Bild aus D11, und wörtlich der Grund, aus dem `unpark` seine Weckmarke
    // IMMER hinterlegt (Z22 P4).
    ops.mark_handler_wait(guest);
    // Über den **vorhandenen** Endpoint-Transport. Der Endpoint bleibt unverändert — A-5.1 hat
    // gezeigt, dass ein Transport, der von seinem Inhalt nichts weiss, der billigere ist.
    //
    // **Badge `0`, und das ist eine Aussage** (2026-08-25): ein umgeleiteter Gast hat keine
    // Endpoint-Cap vorgezeigt -- der Kernel ruft fuer ihn, ueber den Endpoint der HANDLER-Cap.
    // Es gibt also kein Absender-Badge, und eines zu erfinden waere schlimmer als keines: der
    // Handler unterscheidet seine Gaeste am Sidecar-Slot, nicht an einer Zahl im Register.
    // Dieselbe Ueberlegung wie bei `partner_of`, das fuer einen wartenden Empfaenger `None` gibt.
    let next = eps[array_index_nospec(ep_i, eps.len())]
        .lock()
        .call(ops, core, frame, 0);
    if next == frame {
        // **`call` hat NICHT blockiert** — Endpoint stillgelegt (`ERR_QUIESCING`) oder
        // Warteschlange voll (`ERR_EP_FULL`); der Code steht in `x0`. Ein gewöhnlicher Client
        // liefe hier weiter und wiederholte. Ein **Gast** darf das nicht: weiterlaufen hiesse,
        // mit der nativen ABI weiterzulaufen, und das ist die Beförderung, gegen die das ganze
        // Primitiv steht. Er bleibt liegen — mit `HANDLER` gesetzt und dem Grund im Frame.
        // Auflösen muss, wer die Tcb-Cap hält; der Gast selbst kann es per Entwurf nicht.
        return ops.block_for_handler(core, frame);
    }
    next
}

/// **CSUB — einen Teilbereich einer Memory-Cap als eigene Cap ableiten** (Mem-Server-Transport).
///
/// `x1` = Quell-Slot (muss eine Memory-Cap halten) · `MSG0` = freier Ziel-Slot ·
/// `MSG1` = Offset in Bytes · `MSG2` = Länge in Bytes. Die Ableitung trägt die verengte
/// Region, Rechte aus dem Schnitt mit der Quelle, das Badge der Quelle und hängt als Kind
/// im CDT (`revoke` an der Quelle zieht das Teil ein).
///
/// **Vier Worte und keine Rechte-Maske**: `SYS_MAP`/`SYS_SPAWN` brauchen neben Slots nur
/// Offset+Länge, und eine Maske wäre der fünfte Parameter. Wer weniger Rechte will, leitet
/// per `CCOPY` mit Maske ab und schneidet daraus — die Schnitt-Eigenschaft trägt das
/// Primitiv (`CapSpace::subregion`), nicht dieser Zweig.
///
/// Ausgänge: `OK` · `ERR_BADCAP` (kein Cap / keine Memory-Cap) · `ERR_NOSPACE` (Ziel belegt
/// oder ausserhalb, Budget, Tabelle) · `ERR_SUBREGION` (Fenster ausserhalb der Cap, Nr. 21 —
/// existiert seit K1b für die Stapelprüfung) · `ERR_RIGHTS` (Domänen-Policy, wie `CCOPY`).
///
/// **Nummer 37 — Original, kein Spiegel mehr.** `caprock_abi::sys::CSUB`, erste freie Nummer
/// nach FORK/EXEC + Debugger-v2 + LOAD_IMAGE (`4` bleibt historische Luecke).
pub const CSUB: u64 = caprock_abi::sys::CSUB;

/// Der Dispatch **ohne** Umleitung — der Rumpf, den es vor Z26/A3 gab.
#[allow(clippy::too_many_arguments)]
fn dispatch_nativ(
    frame: usize,
    core: usize,
    ops: &mut dyn SchedOps,
    caps: &RwSpinLock<Caps>,
    eps: &[SpinLock<Endpoint>],
    ntfns: &[SpinLock<Notification>],
    load: fn(u32, usize, &[(usize, CapPtr)], usize, usize, u16, u32) -> LadeUebergabe,
    delete_cap: fn(CapPtr) -> Result<(), u64>,
    // K1a: durchgereicht von `dispatch` -- der `SPAWN`-Zweig liegt hier, nicht dort.
    spawn: fn(usize, CapPtr, usize, usize, u8, u64) -> Result<u64, u64>,
    // Z6b: dito -- die fuenf Debug-Zweige liegen hier.
    debug: fn(u64, usize, usize, u64, u64, u64) -> Result<u64, u64>,
    // Stufe B / B3: dito.
    bind_irq: fn(u32, usize, u64, usize) -> Result<(), u64>,
    // LXPD-Laufzeitpfad: dito -- der Zweig liegt hier, nicht in `dispatch`.
    load_image: fn(u64, u64, u32, usize, &[(usize, CapPtr)], usize, usize, u16, u32) -> LadeUebergabe,
    nr: u64,
) -> usize {
    if nr == sys::YIELD {
        return ops.on_tick(core, frame);
    }
    // **`SETTLS` braucht keine Cap** und steht deshalb hier, vor der generischen Aufloesung: die
    // wuerde `x1` als Cap-Slot lesen und mit `ERR_BADCAP` abweisen. Dieselbe Lage wie `YIELD` und
    // `PARK` -- der Syscall wirkt nur auf den Aufrufer.
    // **`CLOCK` braucht keine Cap** und steht deshalb hier, vor der generischen Aufloesung -- die
    // wuerde `x1` als Cap-Slot lesen und mit `ERR_BADCAP` abweisen. Zeit zu lesen gewaehrt nichts.
    //
    // Die Werte kommen aus der **HAL**, nicht aus dem Scheduler: `cycles_per_sec` ist gegen die
    // Plattformuhr geeicht, und diese Crate hat davon keine Kenntnis -- sie reicht durch.
    if nr == sys::CLOCK {
        frame_set_reg(frame, reg::MSG0, ops.clock_hz());
        frame_set_reg(frame, reg::MSG1, ops.clock_now());
        frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
        return frame;
    }
    if nr == sys::SETTLS {
        let va = frame_reg(frame, reg::EP_BADGE);
        // **Die Schranke schuetzt den KERNEL, nicht den Aufrufer.** Ein `WRMSR` auf
        // `IA32_FS_BASE` mit nicht-kanonischem Wert faultet in **Ring 0**, an der Schreibstelle --
        // eine PD ohne jede Cap koennte den Kern damit umlegen. Das Autoritaetsargument („er
        // erreicht nichts, was er nicht schon hatte") gilt fuer das, was er ERREICHT; ueber das,
        // was er den Kernel TUN laesst, sagt es nichts. Zwei Fragen, und nur die erste beantwortet
        // „keine Cap noetig".
        //
        // Geprueft wird die **untere Adresshaelfte**, nicht bloss „kanonisch": strenger, und sie
        // bedeutet etwas. Ein Thread-Pointer in der oberen Haelfte ist ohnehin sinnlos.
        //
        // `0` bleibt erlaubt -- das ist das Abschalten.
        if va >= caprock_abi::USER_VA_TOP {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADTLS);
            return frame;
        }
        let tid = ops.current_id(core);
        let ok = ops.set_tls(tid, va as usize);
        // Zwei Gruende, zwei Codes: „Adresse unzulaessig" oben, „dich gibt es nicht" hier.
        frame_set_reg(
            frame,
            reg::SYSNO_RESULT,
            if ok { result::OK } else { result::ERR_BADCAP },
        );
        return frame;
    }
    if nr == sys::PARK {
        // Selbst-Park: Aufrufer schlafen legen; kein Capability nötig (wirkt nur auf ihn selbst).
        //
        // **Kehrt sofort zurück, wenn eine Weckmarke vorliegt** (Z22, P4) -- ohne das wäre die
        // Folge „Bedingung prüfen (falsch)" → „parken" unterbrechbar, und ein dazwischen
        // eintreffendes `UNPARK` ginge verloren. Wer `PARK` weiterhin als „für immer anhalten"
        // benutzt (Fuzz-Threads, geparkte Demo-Threads), merkt davon nichts: ohne ein `UNPARK`
        // wird nie eine Marke gesetzt.
        return match ops.park_current(core, frame) {
            Some(next) => next,
            None => {
                frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                frame
            }
        };
    }
    if nr == sys::PARK_TIMEOUT {
        // Selbst-Park mit Frist (A2-Rest): wie `PARK`, aber mit Rueckkehr per `ERR_TIMEOUT`.
        // `MSG0` = Frist in Ticks, `0` = keine (dann bitgleich zu `PARK`).
        //
        // Dispatch analog zur `WAIT`-Frist: **scharfgestellt VOR dem Blockieren**, sonst gaebe
        // es ein Fenster, in dem der Thread schon wartet und seine Frist noch nicht steht.
        //
        // **Der bewachte Grund ist `PARK`, nicht `IPC`** -- und das ist kein Stil, sondern die
        // Bedingung, unter der ueberhaupt geweckt wird: `unpark` entfernt genau `PARK`, `unblock`
        // genau `IPC`. Mit `IPC` als Grund weckte ein fremdes `reply` den Parker, waehrend sein
        // eigentliches `UNPARK` wirkungslos verpuffte (Marke gesetzt, Grund bleibt stehen).
        //
        // **Die Weckmarke bleibt erhalten** (`park_current` verbraucht sie zuerst): lag sie vor,
        // kehrt der Aufruf sofort mit `OK` zurueck, und die eben scharfgestellte Frist trifft
        // spaeter auf einen Thread ohne `PARK`-Grund -- sie wird still entwaffnet, ohne Bericht
        // („das Signal gewinnt", dieselbe Aufloesung wie bei der `WAIT`-Frist).
        let ticks = frame_reg(frame, reg::MSG0);
        if ticks != 0 {
            ops.frist_setzen(core, ticks, caprock_sched::BlockReasons::PARK);
        }
        return match ops.park_current(core, frame) {
            Some(next) => next,
            None => {
                frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                frame
            }
        };
    }
    if nr == sys::UNPARK {
        // Ziel muss in DERSELBEN PD liegen. Fail-closed in allen drei Richtungen: der Aufrufer
        // ohne PD, das Ziel ohne PD, oder verschiedene PDs -> abweisen. Siehe `sys::UNPARK`,
        // warum das ohne Cap auskommt und trotzdem keine Autorität hinzufügt.
        let target = ThreadId::from_raw(frame_reg(frame, reg::EP_BADGE));
        let me = ops.current_id(core);
        // **Auf sich selbst wirken braucht keine Autoritaet** -- derselbe Grund, aus dem `PARK`
        // ohne Cap auskommt. Und es ist nicht bloss ein Sonderfall: „erst die Marke setzen, dann
        // schlafen" ist die Redewendung, mit der ein Thread einen Weckruf ueberlebt, der ihn
        // erreicht, bevor er ueberhaupt eingeschlafen ist. Ohne diese Zeile waere sie in einem
        // Thread ohne PD (Boot-Umgebung) nicht formulierbar.
        let erlaubt = target == me || {
            let g = caps.read();
            match (g.pds.pd_of(me), g.pds.pd_of(target)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        };
        if !erlaubt {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADCAP);
            return frame;
        }
        ops.unpark(target);
        frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
        return frame;
    }
    if nr == sys::EXIT {
        // Selbst-Beenden: Stack/TCB werden zurückgewonnen; kein Capability nötig.
        return ops.exit_current(core, frame);
    }

    let deny = |code: u64| -> usize {
        frame_set_reg(frame, reg::SYSNO_RESULT, code);
        frame
    };

    // `CDELETE` **vor** der generischen Cap-Auflösung (A-3.1): die Operation braucht den rohen
    // `CapPtr` und interessiert sich weder für Objektart noch für Rechte. Das ist kein Schlupfloch
    // — der Slot gehört dem Aufrufer, und **Autorität abzugeben darf nie an einer Erlaubnis
    // hängen**. Ein Dienst, der seine empfangenen Caps nicht loswird, läuft sonst gegen
    // `CAP_BUDGET_PER_PD` und ist irgendwann handlungsunfähig, ohne dass jemand ihm etwas
    // entzogen hätte.
    if nr == sys::CDELETE {
        let thread = ops.current_id(core);
        let slot = frame_reg(frame, reg::EP_BADGE) as usize;
        // Reihenfolge: Slot **räumen**, dann löschen, bei Misserfolg zurücklegen.
        //
        // Andersherum (erst löschen, dann räumen) gäbe es ein Fenster, in dem der Slot auf ein
        // bereits finalisiertes Objekt zeigt — und die Finalisierung gibt Speicher frei und bricht
        // Calls ab, sie darf also nicht unter gehaltenem `CAPS` laufen. Die hier gewählte Richtung
        // hat nur ein Fenster, in dem der Slot leer ist, und in dem kann ihn niemand benutzen: der
        // einzige Thread, der ihn benutzen könnte, führt gerade diesen Syscall aus.
        let removed = {
            let mut g = caps.write();
            let Some(pd) = g.pds.pd_of(thread) else {
                return deny(result::ERR_NOPD);
            };
            let Some(cap) = g.pds.cap_at(pd, slot) else {
                return deny(result::ERR_BADCAP);
            };
            g.pds.clear_cap(pd, slot);
            (pd, cap)
        }; // CAPS freigegeben — `delete_cap` sperrt CAPS/MEM/SCHEDS selbst
        let (pd, cap) = removed;
        if let Err(code) = delete_cap(cap) {
            // Abgeleitete Caps hängen noch daran: unverändert zurücklegen. Ein „halb gelöschter"
            // Cap (aus dem Cspace entfernt, im CapSpace noch da) wäre für den Aufrufer unerreichbar
            // und für den Kernel weiterhin belegt — genau das Leck, gegen das CDELETE antritt.
            caps.write().pds.install_cap(pd, slot, cap);
            return deny(code);
        }
        frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
        return frame;
    }

    // `SPAWN` (K1a, 2026-08-17): ein zweiter Thread in der EIGENEN PD, mit einem Stack aus einer
    // Cap des Aufrufers.
    //
    // **Warum hier fast nichts steht.** Die Entscheidung („darf diese Region ein Stack sein?")
    // fällt in *einer* Funktion (`spawncheck::check_stack`), und die braucht Wissen, das nur der
    // Kernel hat — ob ein Gerät die Region erreicht, und ob sie eine bestehende Abbildung
    // überlappt. Dieser Zweig löst deshalb nur Slot → Cap → PD auf und übergibt; **eine Prüfung,
    // die an zwei Stellen halb passiert, ist zwei Prüfungen**, und die zweite altert.
    //
    // Der Aufrufer bleibt **unblockiert**: jede Absage kommt als Code zurück, keine wartet. Das
    // ist D11 wörtlich — der 33. Sender, der blockiert wurde, keinen Code bekam und als *ruhig*
    // gemeldet wurde, ist genau die Form, die hier nicht entstehen darf.
    if nr == sys::SPAWN {
        let thread = ops.current_id(core);
        let slot = frame_reg(frame, reg::MSG0) as usize;
        let entry = frame_reg(frame, reg::MSG0 + 1) as usize;
        let arg = frame_reg(frame, reg::MSG0 + 2) as usize;
        let prio = frame_reg(frame, reg::MSG0 + 3) as u8;
        // K1b: die **Teilregion** der Stack-Cap, roh weitergereicht. Ausgepackt wird sie in
        // `spawncheck::sub_region` -- hier stehen keine 32 und keine Schiebeoperation, damit es
        // nicht zwei Stellen gibt, die das Wort verstehen (dieselbe Begruendung wie fuer den
        // ganzen Zweig: eine Pruefung, die an zwei Stellen halb passiert, ist zwei Pruefungen).
        //
        // `reg::EP_BADGE` (x1) ist das einzige freie Register: alle vier Nachrichtenworte sind
        // vergeben, und `SPAWN` wird **vor** der generischen Cap-Aufloesung abgefertigt, also
        // traegt x1 hier keine Endpoint-ID.
        let sub = frame_reg(frame, reg::EP_BADGE);
        let (pd, cap) = {
            let g = caps.read();
            let Some(pd) = g.pds.pd_of(thread) else {
                return deny(result::ERR_NOPD);
            };
            let Some(cap) = g.pds.cap_at(pd, slot) else {
                return deny(result::ERR_BADCAP);
            };
            (pd, cap)
        }; // CAPS freigegeben -- `spawn` nimmt CAPS/MEM/SCHEDS selbst (Ordnung MEM < SCHEDS)
        match spawn(pd, cap, entry, arg, prio, sub) {
            Ok(tid_raw) => {
                frame_set_reg(frame, reg::MSG0, tid_raw);
                frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                return frame;
            }
            Err(code) => return deny(code),
        }
    }

    // Z6b + Debugger-v2: die Debug-Syscalls (21-25) plus 33-35. **Vor** der generischen
    // Cap-Aufloesung, aus demselben Grund wie `SPAWN`: die Entscheidung faellt in EINER
    // Kernelfunktion, nicht halb hier und halb dort. *Eine Pruefung, die an zwei Stellen halb
    // passiert, ist zwei Pruefungen, und die zweite altert* (K1a). 33-35 reiten auf demselben
    // Rueckruf (Cap-Aufloesung, Rechte, Slot-Weitergabe liegen in `dispatch_debug`); hier steht
    // nur die Weiche, keine Pruefung. Die Luecke 26-32 (BIND_IRQ..EXEC_REPLACE) bleibt draussen.
    if (nr >= sys::DEBUG_ATTACH && nr <= sys::DEBUG_WRITE_REGS)
        || (nr >= sys::DEBUG_WRITE_MEM && nr <= sys::DEBUG_HWBREAK)
    {
        let thread = ops.current_id(core);
        let slot = frame_reg(frame, reg::MSG0) as usize;
        let a1 = frame_reg(frame, reg::MSG0 + 1);
        let a2 = frame_reg(frame, reg::MSG0 + 2);
        let a3 = frame_reg(frame, reg::MSG0 + 3);
        let pd = {
            let g = caps.read();
            match g.pds.pd_of(thread) {
                Some(pd) => pd,
                None => return deny(result::ERR_NOPD),
            }
        }; // CAPS freigegeben -- der Rueckruf nimmt es selbst und HAELT es ueber den Vorgang.
        return match debug(nr, pd, slot, a1, a2, a3) {
            Ok(v) => {
                frame_set_reg(frame, reg::MSG0, v);
                frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                frame
            }
            Err(code) => deny(code),
        };
    }

    // `CCOPY`/`CMOVE`/`SETRECV`/`CSUB` (A-3.2 + Mem-Server-Transport) — wie `CDELETE` vor der
    // generischen Auflösung: sie arbeiten auf **Slots** des eigenen Cspace, nicht auf einem
    // Objekt hinter einer Cap.
    if nr == sys::CCOPY || nr == sys::CMOVE || nr == sys::SETRECV || nr == CSUB {
        let thread = ops.current_id(core);
        let src_slot = frame_reg(frame, reg::EP_BADGE) as usize;
        let dst_slot = frame_reg(frame, reg::MSG0) as usize;
        let mask = frame_reg(frame, reg::MSG0 + 1);
        let new_badge = frame_reg(frame, reg::MSG0 + 2);
        // Ein bei `install_cap_checked` abgelehnter Kopie-Cap muss gelöscht werden — aber
        // **ohne** gehaltenen Lock (die Finalisierung nimmt CAPS/MEM selbst). Deshalb wird er
        // hier herausgereicht statt sofort behandelt.
        let mut orphan: Option<CapPtr> = None;
        let code = {
            let mut g = caps.write();
            let Some(pd) = g.pds.pd_of(thread) else {
                return deny(result::ERR_NOPD);
            };
            match nr {
                sys::SETRECV => {
                    if g.pds.set_recv_slot(pd, src_slot) {
                        result::OK
                    } else {
                        result::ERR_BADCAP
                    }
                }
                sys::CMOVE => {
                    let Some(src) = g.pds.cap_at(pd, src_slot) else {
                        return deny(result::ERR_BADCAP);
                    };
                    if src_slot == dst_slot {
                        result::OK // nichts zu tun; kein Sonderfall, der irgendwo aufschlägt
                    } else if dst_slot >= g.pds.cspace_len_von(pd) {
                        result::ERR_BADCAP
                    } else if g.pds.cap_at(pd, dst_slot).is_some() {
                        // Ein belegtes Ziel wird NICHT stillschweigend überschrieben: das wäre ein
                        // Cap-Verlust, den der Aufrufer nicht angeordnet hat. Wer den Slot räumen
                        // will, ruft CDELETE — sichtbar und mit eigenem Ergebnis.
                        result::ERR_NOSPACE
                    } else {
                        // Reines Umhängen: derselbe Cap, keine Ableitung, Budget unverändert
                        // (ein Slot frei, einer belegt) -> Policy-Prüfung wäre hier ohne Inhalt.
                        g.pds.clear_cap(pd, src_slot);
                        g.pds.install_cap(pd, dst_slot, src);
                        result::OK
                    }
                }
                CSUB => {
                    // **Teilbereichs-Ableitung** (Mem-Server-Transport): `x1` = Quell-Slot,
                    // `MSG0` = Ziel-Slot, `MSG1` = Offset, `MSG2` = Länge. Slots wie `CCOPY`
                    // (Ziel frei + im Lauf + Budget), die Quelle muss eine Memory-Cap sein.
                    let offset = frame_reg(frame, reg::MSG0 + 1);
                    let len = frame_reg(frame, reg::MSG0 + 2);
                    let Some(src) = g.pds.cap_at(pd, src_slot) else {
                        return deny(result::ERR_BADCAP);
                    };
                    // **Die Quell-Cap muss Memory sein — geprüft VOR jeder Ableitung.** Eine
                    // Teil-MMIO- oder Teil-IRQ-Cap gäbe es nicht (Gerätefenster sind unteilbar),
                    // und sie hier zuzulassen hiesse, Geräte-Autorität zuzuschneiden, die der
                    // Kernel ungeschnitten vergeben hat.
                    let ist_memory = matches!(
                        g.cspace.lookup(src),
                        Some((ObjectKind::Memory(_), _, _))
                    );
                    if !ist_memory {
                        return deny(result::ERR_BADCAP);
                    }
                    if dst_slot >= g.pds.cspace_len_von(pd) || g.pds.cap_at(pd, dst_slot).is_some() {
                        result::ERR_NOSPACE
                    } else if !g.budget_allows(pd, dst_slot) {
                        result::ERR_NOSPACE
                    } else {
                        // **Keine Rechte-Maske** (vier Worte, s. `CSUB`-Doku): der Schnitt mit der
                        // Quelle ist die Identität — die Schnitt-Eigenschaft trägt das Primitiv
                        // (`CapSpace::subregion`), nicht dieser Zweig.
                        match g.cspace.subregion(src, offset, len, Rights::RWX) {
                            Ok(new) => {
                                if g.install_cap_checked(pd, dst_slot, new) {
                                    result::OK
                                } else {
                                    orphan = Some(new);
                                    result::ERR_RIGHTS
                                }
                            }
                            // **Jeder Cap-Fehler hat seinen ABI-Namen**: ausserhalb heisst
                            // `ERR_SUBREGION` (Nr. 21), nicht „bad cap" und nicht „kein Platz".
                            Err(CapError::Subregion) => result::ERR_SUBREGION,
                            Err(CapError::Invalid) => result::ERR_BADCAP,
                            Err(_) => result::ERR_NOSPACE,
                        }
                    }
                }
                _ => {
                    debug_assert!(nr == sys::CCOPY);
                    let Some(src) = g.pds.cap_at(pd, src_slot) else {
                        return deny(result::ERR_BADCAP);
                    };
                    let Some((_, have, _)) = g.cspace.lookup(src) else {
                        return deny(result::ERR_BADCAP);
                    };
                    if dst_slot >= g.pds.cspace_len_von(pd) || g.pds.cap_at(pd, dst_slot).is_some() {
                        result::ERR_NOSPACE
                    } else if !g.budget_allows(pd, dst_slot) {
                        result::ERR_NOSPACE
                    } else {
                        // **Nie mehr Rechte als das Original.** Die Maske kann nur einschränken;
                        // ein Programm, das RWX anfordert, bekommt den Schnitt mit dem, was es
                        // selbst hat — sonst wäre CCOPY ein Rechte-Aufwertungsdienst.
                        let want = rights_from_bits(mask).intersect(have);
                        // `MSG3` is the IPC permission mask (`caprock_abi::ipc_perm`). `0` = an
                        // ordinary derivation, exactly as before; non-zero = derive a *tagged*
                        // capability whose mask the kernel will deliver to the server.
                        let perm_raw = frame_reg(frame, reg::MSG0 + 3);
                        // A tagged source may only be copied by a holder of the DUP permission.
                        // Untagged capabilities always may, so existing programs are unaffected.
                        if !g.cspace.may_dup(src) {
                            return deny(result::ERR_RIGHTS);
                        }
                        // `0` = Badge erben. Ein eigenes Badge ist erlaubt, weil der Aufrufer das
                        // Objekt bereits besitzt: er vergibt ein Etikett auf eigener Autoritaet.
                        let tagged_src = matches!(g.cspace.slot_perms(src), Ok(Some(_)));
                        let derived = if perm_raw != 0 {
                            // Only endpoint capabilities have a server to attest to; the mask
                            // must fit 16 bits, be non-empty, and the id must fit 48 bits.
                            let is_endpoint =
                                matches!(g.cspace.kind_of(src), Some(ObjectKind::Endpoint(_)));
                            if !is_endpoint {
                                return deny(result::ERR_BADCAP);
                            }
                            if perm_raw > u64::from(u16::MAX)
                                || !caprock_abi::ipc_perm::id_fits(new_badge)
                            {
                                return deny(result::ERR_RIGHTS);
                            }
                            g.cspace.derive_ipc(src, want, new_badge, perm_raw as u16)
                        } else if new_badge != 0 {
                            if tagged_src && !caprock_abi::ipc_perm::id_fits(new_badge) {
                                return deny(result::ERR_RIGHTS);
                            }
                            g.cspace.mint(src, want, new_badge)
                        } else {
                            g.cspace.copy(src, want)
                        };
                        match derived {
                            Ok(new) => {
                                if g.install_cap_checked(pd, dst_slot, new) {
                                    result::OK
                                } else {
                                    orphan = Some(new);
                                    result::ERR_RIGHTS
                                }
                            }
                            Err(_) => result::ERR_NOSPACE,
                        }
                    }
                }
            }
        }; // CAPS freigegeben
        if let Some(c) = orphan {
            let _ = delete_cap(c);
        }
        return deny(code); // `deny` setzt nur x0 und gibt den Frame zurück — auch für OK richtig
    }

    // Cap-Auflösung unter dem **geteilten Read-Lock** auf `CAPS`: der heiße Lookup ist
    // rein lesend und läuft so auf verschiedenen Kernen parallel (kein globaler Engpass
    // mehr). Der Guard wird sofort wieder freigegeben; alle Pfade außer REPLY+grant
    // arbeiten danach ohne CAPS weiter (wie zuvor). Die Werte sind `Copy`/ownend.
    let thread = ops.current_id(core);
    let local = frame_reg(frame, reg::EP_BADGE) as usize;
    let (pd, kind, rights, badge) = {
        let g = caps.read();
        let Some(pd) = g.pds.pd_of(thread) else {
            return deny(result::ERR_NOPD);
        };
        let Some(cap) = g.pds.cap_at(pd, local) else {
            return deny(result::ERR_BADCAP);
        };
        let Some((kind, rights, badge)) = g.cspace.lookup(cap) else {
            return deny(result::ERR_BADCAP);
        };
        // A tagged IPC capability delivers `permissions << 48 | id` instead of the plain badge
        // (`caprock_abi::ipc_perm`); for every other capability this is the badge unchanged.
        let badge = g.cspace.wire_badge(cap).unwrap_or(badge);
        (pd, kind, rights, badge)
    }; // CAPS-Read-Lock hier freigegeben

    match nr {
        sys::CALL | sys::CALL_TIMEOUT | sys::RECV | sys::REPLY => {
            // --- Z26/A3: eine Handler-Cap darf hier EMPFANGEN und ANTWORTEN, aber nicht rufen ---
            //
            // `RECV`/`REPLY` sind die Operationen, mit denen eine Persönlichkeits-PD ihre Gäste
            // bedient. `CALL` ist es **nicht**: darüber gäbe ein Handler sich als Gast aus und
            // stellte sich selbst eine Umleitung zu.
            //
            // Der Unterschied zu einem gewöhnlichen Endpoint steckt im `REPLY`: es beendet nicht
            // nur eine Transaktion, es **schliesst einen Syscall ab** — und deshalb fällt dort
            // zusätzlich der Grund `HANDLER`. Genau diese Wirkung kann ein Endpoint nicht haben,
            // und genau deshalb sind es eigene Cap-Arten.
            let handler_kanal = matches!(
                kind,
                ObjectKind::SyscallHandler { .. } | ObjectKind::FaultHandler { .. }
            );
            let ep_id = match kind {
                ObjectKind::Endpoint(id) => id,
                ObjectKind::SyscallHandler { ep, .. } | ObjectKind::FaultHandler { ep, .. } => {
                    if nr == sys::CALL || nr == sys::CALL_TIMEOUT {
                        return deny(result::ERR_BADCAP);
                    }
                    ep
                }
                _ => return deny(result::ERR_BADCAP),
            };
            let need = if nr == sys::CALL || nr == sys::CALL_TIMEOUT {
                Rights::WRITE
            } else {
                Rights::READ
            };
            if !rights.contains(need) {
                return deny(result::ERR_RIGHTS);
            }
            // --- Z23 S1: die Tore dieser PD ---------------------------------------------------
            //
            // **Was sie ANFAENGT, wird abgewiesen; was schon laeuft, darf auslaufen.** `CALL` und
            // `RECV` beginnen etwas Neues, `REPLY` beendet etwas Bestehendes. Waere `REPLY` mit
            // gesperrt, koennte ein Server seine offene Antwort nicht mehr loswerden -- sein
            // Client haenge fuer immer, und die Stilllegung erzeugte genau den Deadlock, den sie
            // ermoeglichen soll aufzuloesen.
            //
            // Am **Subjekt** geprueft, nicht am Objekt: an einem Endpoint haengen auch fremde PDs
            // (`Endpoint::begin_quiesce` aus A-4.2 legt den KANAL stumm, dieses Bit den
            // TEILNEHMER). Ein Riegel am Objekt fröre Dritte mit ein.
            //
            // **`ERR_QUIESCING` und nicht `ERR_EP_FULL`/`ERR_BADCAP`:** der Code heisst „kommt
            // gleich wieder", die anderen heissen „gibt es nicht". Ein Aufrufer, der die beiden
            // nicht unterscheiden kann, muss raten, ob er wiederholen soll (s. der Grund fuer den
            // dritten Code in `caprock-abi`).
            //
            // **OFFEN und ausdruecklich nicht hier entschieden (S1b):** was ein FREMDER Aufrufer
            // erlebt, der in diese PD hineinruft. Das Subjekt-Gating stoppt nur, was sie selbst
            // anfaengt; ihre Endpoints existieren weiter, und ein Dritter blockiert dort bis zum
            // Thaw. Drei Optionen stehen in `todo.md` Z23/S1b -- keine ist gratis, und eine
            // stillschweigende Wahl waere die schlechteste.
            if nr != sys::REPLY && caps.read().pds.is_quiescing(pd) {
                return deny(result::ERR_QUIESCING);
            }
            // `ep_id` kommt aus der (cap-geprüften) Endpoint-Cap; die Schranke wird zusätzlich
            // spekulationssicher maskiert (s. `cap_at`), da die Cap-Auswahl EL0-gesteuert ist.
            let ep = array_index_nospec(ep_id as usize, eps.len());
            if ep_id as usize >= eps.len() {
                return deny(result::ERR_BADCAP);
            }
            match nr {
                // Das Badge ist oben bei der Cap-Aufloesung schon da (`lookup`); bis 2026-08-25
                // ging es nur an `signal` und wurde hier fallengelassen.
                sys::CALL => eps[ep].lock().call(ops, core, frame, badge),
                // A2-Rest: Frist VOR dem Blockieren scharfstellen -- dieselbe Ordnung wie bei
                // WAIT (dort MSG0) und PARK_TIMEOUT. Der bewachte Grund ist IPC: unblock
                // (Antwort) und Timer entfernen denselben Grund, das Rennen entscheidet
                // fristen_faellig wie bei WAIT (Signal gewinnt).
                sys::CALL_TIMEOUT => {
                    // Eigener Name (`frist` statt `ticks`): M3 in tools/fristen-negativ.sh
                    // verankert seine Mutation textuell am WAIT-Arm; zwei gleiche Zeilen
                    // wuerden die Sonde zweideutig machen.
                    let frist = frame_reg(frame, reg::MSG0);
                    if frist != 0 {
                        ops.frist_setzen(core, frist, caprock_sched::BlockReasons::IPC);
                    }
                    frame_set_reg(frame, reg::MSG0, frame_reg(frame, reg::MSG1));
                    frame_set_reg(frame, reg::MSG1, frame_reg(frame, reg::MSG2));
                    frame_set_reg(frame, reg::MSG2, frame_reg(frame, reg::MSG3));
                    frame_set_reg(frame, reg::MSG3, 0);
                    eps[ep].lock().call(ops, core, frame, badge)
                },
                sys::RECV => eps[ep].lock().recv(ops, core, frame),
                _ => {
                    // REPLY: ggf. Cap-Transfer (grant). Der Transfer MUTIERT den CapSpace,
                    // braucht also den **Write-Lock**; um die Sperrordnung CAPS->EPS zu
                    // wahren, wird CAPS-write VOR dem Endpoint-Lock genommen und nach dem
                    // Transfer (vor dem Rendezvous) freigegeben. Ohne grant ist kein CAPS
                    // nötig — reiner Endpoint-Lock.
                    let tag = frame_reg(frame, reg::TAG);
                    if tag & caprock_abi::GRANT_FLAG != 0 {
                        let mut g = caps.write();
                        let mut e = eps[ep].lock();
                        let replaced = match e.caller() {
                            Some(caller) => grant_cap(&mut g, caller, pd, (tag & 0xff) as usize),
                            None => None,
                        };
                        drop(g); // CAPS vor dem Rendezvous freigeben (EPS bleibt gehalten)
                        let next = e.reply(ops, core, frame);
                        drop(e); // EPS freigeben, BEVOR das Löschen CAPS/MEM/SCHEDS nimmt
                        // Die im Empfangs-Slot verdrängte Cap freigeben (sonst bliebe sie
                        // unerreichbar in der geteilten Tabelle belegt -> Cross-PD-DoS).
                        if let Some(old) = replaced {
                            let _ = delete_cap(old);
                        }
                        next
                    } else {
                        // **Der Handler-Fall: den Gast-Grund fallen lassen** (Z26/A3).
                        //
                        // Der Gast trägt zwei Gründe: `IPC` (setzt `call`, entfernt `reply`) und
                        // `HANDLER`. Erst wenn **beide** weg sind, ist die Menge leer und er läuft
                        // — und die Reihenfolge ist hier gutartig: `handler_reply` entfernt seinen
                        // Grund, `reply` den anderen, und wer zuletzt kommt, reiht ein.
                        //
                        // Der Aufrufer wird **vor** dem `reply` gelesen: danach hat der Endpoint
                        // ihn vergessen (`caller` ist zurückgesetzt), und wir wüssten nicht mehr,
                        // wessen Syscall gerade fertig geworden ist.
                        let guest = if handler_kanal {
                            eps[ep].lock().caller()
                        } else {
                            None
                        };
                        let next = eps[ep].lock().reply(ops, core, frame);
                        if let Some(g) = guest {
                            ops.handler_reply(g);
                        }
                        next
                    }
                }
            }
        }
        sys::SIGNAL | sys::WAIT => {
            let ObjectKind::Notification(id) = kind else {
                return deny(result::ERR_BADCAP);
            };
            let need = if nr == sys::SIGNAL { Rights::WRITE } else { Rights::READ };
            if !rights.contains(need) {
                return deny(result::ERR_RIGHTS);
            }
            let n = array_index_nospec(id as usize, ntfns.len());
            if id as usize >= ntfns.len() {
                return deny(result::ERR_BADCAP);
            }
            if nr == sys::SIGNAL {
                ntfns[n].lock().signal(ops, badge, frame)
            } else {
                // --- Stufe A / A2: `WAIT` mit Frist -------------------------------------------
                //
                // `MSG0` = Frist in Ticks, `0` = keine. **Rueckwaertskompatibel**: jeder vor A2
                // geschriebene Aufruf legt dort eine Null hin, und eine Null heisst „warte wie
                // bisher" -- bitgleich zum alten Verhalten.
                //
                // **Scharfgestellt VOR dem Blockieren.** Andersherum gaebe es ein Fenster, in dem
                // der Thread schon wartet und seine Frist noch nicht steht; ein Tick darin ginge
                // verloren. Dieselbe Ordnung wie bei C8, und aus demselben Grund.
                let ticks = frame_reg(frame, reg::MSG0);
                if ticks != 0 {
                    ops.frist_setzen(core, ticks, caprock_sched::BlockReasons::IPC);
                }
                ntfns[n].lock().wait(ops, core, frame)
            }
        }
        // --- Stufe B / B3: SYS_BIND_IRQ ------------------------------------------------------
        //
        // **Der Aufrufer nennt keinen Vektor, er nennt eine Cap.** Das ist der ganze Zweck des
        // Syscalls: `intid` kommt aus dem `Irq`-Objekt, nicht aus einem Register. Der Unterschied
        // ist nicht kosmetisch -- die in-Kernel-Fassung `bind_irq(intid, ntfn, badge, core)` nahm
        // rohe Zahlen und behauptete in ihrem eigenen Doku-Kommentar, "ueber die IRQ-Cap
        // autorisiert" zu sein. Eine Signatur kann das nicht tragen, und ein Waechter prueft die
        // EXISTENZ eines Grundes, nie seine WAHRHEIT.
        //
        // Beide Caps liegen im Cspace des **Aufrufers**, und beide werden gebraucht: die `Irq`-Cap
        // sagt *welcher Interrupt*, die Notification sagt *wohin*. Ohne die zweite koennte eine PD
        // den Interrupt ihres Geraets an ein fremdes Objekt zustellen lassen.
        //
        // **Multi-Vektor-MSI-X: ein Aufruf je Paar, kein neuer Syscall.** Ein Gerät mit `n`
        // Vektoren hält `n` `Irq`-Caps (je Vektor ein `intid`) und `n` Notifications; der
        // Treiber bindet Paar für Paar über genau diesen Zweig. Der ganze Satz wird VOR der
        // ersten Bindung geprüft (`irqvec::paare_pruefen` — injizierte Sicht auf den
        // **variablen** Cspace-Lauf, Rechte `READ`/`WRITE` wie hier); was darüber liegt,
        // meldet `ERR_IRQ_FULL` gerätelokal. Kernel-Seite: Patch-Text (DriverAssign-
        // Vektorisierung, fremder Strang).
        sys::BIND_IRQ => {
            let ObjectKind::Irq { intid } = kind else {
                return deny(result::ERR_BADCAP);
            };
            // **READ, nicht WRITE.** Binden greift in keinen fremden Thread ein -- es sagt, wohin
            // *dieser* Interrupt geht, und wer die Cap haelt, haelt genau diese Autoritaet.
            if !rights.contains(Rights::READ) {
                return deny(result::ERR_RIGHTS);
            }
            let ntfn_slot = frame_reg(frame, reg::MSG0) as usize;
            let badge = frame_reg(frame, reg::MSG1);
            let ntfn_id = {
                let g = caps.read();
                let Some(cap) = g.pds.cap_at(pd, ntfn_slot) else {
                    return deny(result::ERR_BADCAP);
                };
                let Some((k, r, _)) = g.cspace.lookup(cap) else {
                    return deny(result::ERR_BADCAP);
                };
                let ObjectKind::Notification(id) = k else {
                    return deny(result::ERR_BADCAP);
                };
                // **WRITE auf der Notification** -- der Kernel wird sie signalisieren, und
                // signalisieren ist ein Schreibzugriff. `SYS_SIGNAL` verlangt hier dasselbe; wer
                // ueber den Umweg des Interrupts mit READ signalisieren koennte, haette die
                // Rechtepruefung des direkten Weges umgangen.
                if !r.contains(Rights::WRITE) {
                    return deny(result::ERR_RIGHTS);
                }
                if id as usize >= ntfns.len() {
                    return deny(result::ERR_BADCAP);
                }
                id as usize
            }; // CAPS freigegeben -- der Kernel nimmt seine eigene Tabelle
            let code = match bind_irq(intid, ntfn_id, badge, core) {
                Ok(()) => result::OK,
                Err(e) => e,
            };
            // **Der Aufrufer wird nicht blockiert** -- jede Absage ist ein Code. D11 woertlich.
            frame_set_reg(frame, reg::SYSNO_RESULT, code);
            frame
        }
        sys::KILL => {
            let ObjectKind::Tcb(raw) = kind else {
                return deny(result::ERR_BADCAP);
            };
            if !rights.contains(Rights::WRITE) {
                return deny(result::ERR_RIGHTS);
            }
            let ok = ops.kill(ThreadId::from_raw(raw), core);
            frame_set_reg(frame, reg::SYSNO_RESULT, if ok { result::OK } else { result::ERR_BADCAP });
            frame
        }
        // --- Z26/A3: SYS_SETHANDLER ---------------------------------------------------------
        //
        // **Zwei Autoritäten von zwei Seiten, beide im Cspace des AUFRUFERS.** Die Tcb-Cap sagt
        // *wessen* Syscalls, die Handler-Caps sagen *wohin*. Wer umschaltet, ist damit weder Gast
        // noch Handler — ohne die Tcb-Cap könnte jede PD, die zufällig eine Handler-Cap hält, die
        // Syscalls eines fremden Threads an sich ziehen.
        //
        // Die generische Auflösung oben hat bereits `x1` als Tcb-Cap aufgelöst (`kind`/`rights`);
        // die beiden Handler-Caps kommen aus `x2`/`x3` und werden hier einzeln nachgeschlagen.
        sys::SETHANDLER => {
            let ObjectKind::Tcb(raw) = kind else {
                return deny(result::ERR_BADCAP);
            };
            // **WRITE, nicht READ.** Die Bindung ändert, wer den Thread ausführt — das ist
            // dieselbe Klasse Eingriff wie `KILL`, nicht wie „nachsehen".
            if !rights.contains(Rights::WRITE) {
                return deny(result::ERR_RIGHTS);
            }
            let ziel = ThreadId::from_raw(raw);
            let sys_slot = frame_reg(frame, reg::MSG0);
            let flt_slot = frame_reg(frame, reg::MSG1);
            let entbinden = sys_slot == u64::MAX && flt_slot == u64::MAX;

            // --- Entbinden: immer zulässig, und es braucht keine Handler-Cap ------------------
            //
            // **Autorität abzugeben darf nie an einer Erlaubnis hängen** — dieselbe Regel wie bei
            // `CDELETE`. Und eine Kante zu entfernen kann keinen Kreis schliessen.
            if entbinden {
                let alt = ops.handler_of(ziel);
                let Some(alt) = alt else {
                    // Nicht gebunden -> nichts zu tun. **OK und nicht ERR**: ein Entbinden, das
                    // schon erreicht ist, ist erreicht. Ein Fehler hier zwänge jeden Aufräumpfad,
                    // vorher zu fragen — und wer fragen muss, hat ein Rennen.
                    return deny(result::OK);
                };
                if !ops.set_handler(ziel, None) {
                    return deny(result::ERR_BADCAP);
                }
                let mut g = caps.write();
                if let Some(gast_pd) = g.pds.pd_of(ziel) {
                    g.pds.handler_kante_loesen(gast_pd);
                }
                g.pds.sidecar_freigeben(alt.handler_pd as usize, alt.slot);
                return deny(result::OK);
            }

            // --- Binden ----------------------------------------------------------------------
            let (sys_ep, flt_ep, handler_pd, sidecar) = {
                let g = caps.read();
                let mut sys_ep = redirect::KEIN_EP;
                let mut flt_ep = redirect::KEIN_EP;
                let mut hpd: Option<u16> = None;
                // **Das Fenster wird MITGENOMMEN, nicht nachgeschlagen** (2026-08-13). Der
                // Syscall-Pfad hat später den Gast, nicht die Handler-Cap — die liegt im Cspace
                // des Handlers. Ein Cap-Lookup je umgeleitetem Syscall wäre der Preis dafür,
                // dieselbe Zahl zweimal zu führen; abgeschrieben wird sie **einmal**, hier.
                let mut fenster: Option<(u64, u64)> = None;
                // Beide Caps müssen auf **dieselbe** Handler-PD zeigen. Zwei PDs, von denen die
                // eine die Syscalls und die andere die Faults eines Gastes sieht, wären zwei
                // Kernel über einem Adressraum — und im Zyklus-Graphen zwei Kanten von einem
                // Knoten, also genau die Form, die den billigen Gang unmöglich macht.
                for (slot, ist_sys) in [(sys_slot, true), (flt_slot, false)] {
                    if slot == u64::MAX {
                        continue;
                    }
                    let Some(cap) = g.pds.cap_at(pd, slot as usize) else {
                        return deny(result::ERR_BADCAP);
                    };
                    let Some((k, r, _)) = g.cspace.lookup(cap) else {
                        return deny(result::ERR_BADCAP);
                    };
                    if !r.contains(Rights::WRITE) {
                        return deny(result::ERR_RIGHTS);
                    }
                    let (ep, hp, sc, ln) = match (k, ist_sys) {
                        (
                            ObjectKind::SyscallHandler {
                                ep,
                                pd,
                                sidecar,
                                len,
                            },
                            true,
                        ) => (ep, pd, sidecar, len),
                        (
                            ObjectKind::FaultHandler {
                                ep,
                                pd,
                                sidecar,
                                len,
                            },
                            false,
                        ) => (ep, pd, sidecar, len),
                        // **Ein gewöhnlicher Endpoint wird hier ABGEWIESEN** — das ist der Grund
                        // für die eigenen Cap-Arten. Eine PD kann ihren Dienst-Endpoint nicht
                        // versehentlich als Persönlichkeit binden, und ein `SyscallHandler` im
                        // Fault-Slot ist ebenso wenig zulässig wie umgekehrt.
                        _ => return deny(result::ERR_BADCAP),
                    };
                    match hpd {
                        None => hpd = Some(hp),
                        Some(x) if x != hp => return deny(result::ERR_HANDLER_BUSY),
                        _ => {}
                    }
                    // **Und dasselbe FENSTER**, nicht nur dieselbe PD. Zwei Caps auf eine PD mit
                    // verschiedenen Fenstern hiessen: der Syscall-Frame landet woanders als der
                    // Fault-Frame, während beide denselben Slot-Index tragen — zwei Wahrheiten
                    // über einen Gast, und die Slot-Buchhaltung führte nur eine davon.
                    match fenster {
                        None => fenster = Some((sc, ln)),
                        Some(x) if x != (sc, ln) => return deny(result::ERR_HANDLER_BUSY),
                        _ => {}
                    }
                    // Das Fenster muss ALLE Slots decken, die die Maske vergeben kann. Die
                    // Prüfung steht **an der Bindung** und nicht am Zugriff: eine Schranke, die
                    // erst beim Schreiben zuschlägt, hat den Gast schon angenommen.
                    if !redirect::fenster_deckt(SIDECAR_SLOTS, ln) || sc == 0 {
                        return deny(result::ERR_NOSPACE);
                    }
                    if ist_sys {
                        sys_ep = ep;
                    } else {
                        flt_ep = ep;
                    }
                }
                let Some(hpd) = hpd else {
                    return deny(result::ERR_BADCAP);
                };
                let Some((sc, _)) = fenster else {
                    return deny(result::ERR_BADCAP);
                };
                (sys_ep, flt_ep, hpd, sc)
            }; // CAPS-Read freigegeben

            // --- Das Zyklusverbot, IM KERNEL (Z26/Nachtrag 3) ---------------------------------
            let mut g = caps.write();
            let Some(gast_pd) = g.pds.pd_of(ziel) else {
                return deny(result::ERR_NOPD);
            };
            let frei = g.pds.sidecar_frei(handler_pd as usize);
            let n_knoten = g.pds.pd_capacity();
            let urteil = redirect::pruefe_bindung(
                gast_pd as u16,
                handler_pd,
                sys_ep != redirect::KEIN_EP,
                flt_ep != redirect::KEIN_EP,
                // Der Gang liest die PD-Tabelle **direkt**. Ein kopiertes Kantenarray wäre bei
                // `NPDS = 10 000` zwanzig Kilobyte Stack je `SETHANDLER` -- und ein zweites
                // Abbild einer Wahrheit, die schon existiert.
                |p| g.pds.handler_pd_of(p as usize),
                n_knoten,
                frei,
            );
            HANDLER_URTEILE[urteil_index(urteil)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            match urteil {
                redirect::BindUrteil::Ok => {}
                // **Drei Absagen, drei Codes** — und die Trennung ist nicht Kosmetik. „Kreis"
                // heisst: nimm eine andere Handler-PD. „Schon gebunden" heisst: jemand muss eine
                // Entscheidung zurücknehmen. „Kein Slot" heisst: warte oder gib einen frei.
                // `KetteZuLang` ist ein KERNELfehler (ein Kreis, an dem der Gast gar nicht
                // beteiligt ist) und trägt nach aussen denselben Code — der Aufrufer kann nichts
                // anderes tun —, wird aber getrennt gezählt.
                redirect::BindUrteil::Zyklus
                | redirect::BindUrteil::SelbstBindung
                | redirect::BindUrteil::KetteZuLang => {
                    return deny(result::ERR_HANDLER_CYCLE)
                }
                redirect::BindUrteil::FremderHandler => return deny(result::ERR_HANDLER_BUSY),
                redirect::BindUrteil::KeinSlot => return deny(result::ERR_NOSPACE),
                redirect::BindUrteil::Wirkungslos => return deny(result::ERR_BADCAP),
            }
            let Some(slot) = g.pds.sidecar_belegen(handler_pd as usize) else {
                return deny(result::ERR_NOSPACE);
            };
            let bindung = redirect::Bindung {
                sys_ep,
                fault_ep: flt_ep,
                handler_pd,
                slot,
                sidecar,
            };
            g.pds.handler_kante_setzen(gast_pd, handler_pd);
            drop(g);
            if !ops.set_handler(ziel, Some(bindung)) {
                // **Rückwärts abbauen, nicht liegenlassen.** Der Thread ist zwischen Prüfung und
                // Ablage gestorben oder migriert; Kante und Slot stünden sonst für einen Gast,
                // den es nicht mehr gibt — und das Zyklusverbot verböte danach für immer eine
                // Bindung, die längst zulässig wäre.
                let mut g = caps.write();
                g.pds.handler_kante_loesen(gast_pd);
                g.pds.sidecar_freigeben(handler_pd as usize, slot);
                return deny(result::ERR_BADCAP);
            }
            deny(result::OK)
        }
        sys::MAP | sys::UNMAP => {
            // Frame über eine Memory-Cap in die eigene VSpace mappen/entfernen. Das
            // Recht der Cap bestimmt die Seitenrechte: EXEC -> RX (W^X), WRITE -> RW,
            // sonst READ -> RO. Ohne nutzbares Recht abgelehnt.
            //
            // **A-5.1: MMIO- und DMA-Caps gehen hier ebenfalls durch.** Bis dahin mappte der
            // Kernel die Fenster eines HardwareLand-Backends beim Erzeugen selbst; ein Treiber,
            // der sein Gerät erst *bekommt* und dann bedient, muss es aber selbst aufziehen und
            // wieder loslassen können — sonst ist „Hot-Reload" eine Eigenschaft, die der Kernel
            // für ihn ausübt.
            //
            // Was zurückkommt, ist die **Beschreibung des Fensters**, nicht neue Autorität: Basis,
            // Länge und (bei DMA) die Gerätesicht der Region, die der Aufrufer bereits hält. Ohne
            // sie bräuchte ein Treiber einen Boot-Info-Block, den jedes Programm ungefragt lesen
            // kann — und der wäre eine Autoritätsquelle neben dem Manifest (s. `loader::boot_arg`).
            // `ro_kind` = "das OBJEKT verlangt schreibgeschuetzt", unabhaengig von den Rechten.
            // Nur eine DMA-Cap in Richtung `DeviceRead` sagt das: dort liest das Geraet, und der
            // Puffer ist gegen es schreibgeschuetzt. Ein MMIO-Fenster sagt es NICHT -- Register
            // schreibt man, sonst steuert man nichts. (Genau hier lag beim ersten Anlauf ein
            // Fehler: MMIO pauschal `ro` gemappt, und der Treiber faultete beim allerersten
            // Schreiben auf `device_status`. Ob ein Fenster schreibbar ist, sagt die **Cap**.)
            let (base, len, dma, ro_kind) = match kind {
                ObjectKind::Memory(r) => (r.base, r.len, None, false),
                ObjectKind::Mmio { phys, len } => (phys, len, None, false),
                ObjectKind::Dma { phys, len, dir, coherence } => (
                    phys,
                    len,
                    Some(coherence == DmaCoherence::Coherent),
                    dir == DmaDir::DeviceRead,
                ),
                _ => return deny(result::ERR_BADCAP),
            };
            let perm_code = if rights.contains(Rights::EXEC) {
                2u8
            } else if rights.contains(Rights::WRITE) {
                1
            } else if rights.contains(Rights::READ) {
                0
            } else {
                return deny(result::ERR_RIGHTS);
            };
            // Geräte-Fenster (MMIO/DMA) laufen über `map_window`: ihre Speicherattribute folgen
            // dem Objekt, nicht den Rechten (s. Trait-Doku). Plain-RAM bleibt auf `map_frame`.
            let is_device = !matches!(kind, ObjectKind::Memory(_));
            if is_device {
                // Ein Register-Fenster nur-lesbar zu mappen wäre ein Treiber, der nichts
                // steuern kann; das Schreibrecht muss aus der Cap kommen.
                let ro = ro_kind || !rights.contains(Rights::WRITE);
                if nr == sys::MAP {
                    match ops.map_window(thread, base, len, ro, dma) {
                        Some(iova) => {
                            frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
                            frame_set_reg(frame, reg::MSG0, base);
                            frame_set_reg(frame, reg::MSG1, len);
                            frame_set_reg(frame, reg::MSG2, iova);
                        }
                        None => frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADCAP),
                    }
                } else {
                    let ok = ops.unmap_window(thread, base, len);
                    frame_set_reg(
                        frame,
                        reg::SYSNO_RESULT,
                        if ok { result::OK } else { result::ERR_BADCAP },
                    );
                }
                return frame;
            }
            let ok = if nr == sys::MAP {
                ops.map_frame(thread, base, len, perm_code)
            } else {
                ops.unmap_frame(thread, base, len)
            };
            if ok && nr == sys::MAP {
                frame_set_reg(frame, reg::MSG0, base);
                frame_set_reg(frame, reg::MSG1, len);
                frame_set_reg(frame, reg::MSG2, 0);
            }
            frame_set_reg(frame, reg::SYSNO_RESULT, if ok { result::OK } else { result::ERR_BADCAP });
            frame
        }
        sys::PDCTL => {
            // PD-Management (ext-22): Lifecycle einer Ziel-PD steuern. Gated auf eine
            // PdControl-Cap (WRITE) UND: der Aufrufer muss TrustedSas sein UND das Ziel
            // muss UserLand sein (Defense-in-Depth — nur Trusted steuert nur Untrusted).
            let ObjectKind::PdControl { pd: target } = kind else {
                return deny(result::ERR_BADCAP);
            };
            if !rights.contains(Rights::WRITE) {
                return deny(result::ERR_RIGHTS);
            }
            let target = target as usize;
            let subop = frame_reg(frame, reg::MSG0); // x2 = Sub-Operation
            // Domänen-Policy + Ziel-Thread unter CAPS.read auflösen (dann freigeben).
            let target_tid = {
                let g = caps.read();
                if g.pds.domain_of(pd) != Some(Domain::TrustedSas) {
                    return deny(result::ERR_RIGHTS);
                }
                if g.pds.domain_of(target) != Some(Domain::UserLand) {
                    return deny(result::ERR_RIGHTS);
                }
                g.pds.thread_of(target)
            };
            let ok = match (subop, target_tid) {
                (pdctl::PAUSE, Some(t)) => {
                    ops.pause(t);
                    true
                }
                // START und RESUME heben die **Pause** auf -- Z24. Vorher stand hier `unblock`,
                // also „hebe irgendeine Blockade auf": ein Ziel, das gerade in IPC wartete, lief
                // damit los, obwohl niemand es geweckt hatte. Seit dem Umbau nennt jeder Wecker
                // seinen Grund, und der Grund von PDCTL ist PAUSE.
                (pdctl::RESUME, Some(t)) | (pdctl::START, Some(t)) => {
                    ops.resume(t);
                    true
                }
                (pdctl::STOP, Some(t)) => ops.stop(t),
                (pdctl::PAUSE, None)
                | (pdctl::RESUME, None)
                | (pdctl::START, None)
                | (pdctl::STOP, None) => false, // Ziel hat keinen Thread
                _ => return deny(result::ERR_BADSYS), // unbekannte Sub-Operation
            };
            frame_set_reg(frame, reg::SYSNO_RESULT, if ok { result::OK } else { result::ERR_BADCAP });
            frame
        }
        sys::LOAD => {
            // ext-26: cap-gegatetes Laden eines extern gebauten Programms zur Laufzeit. Die
            // `Loader`-Cap ist die Autoritaet; der geladene Prozess erhaelt NUR den/die explizit
            // delegierten Caller-Cap(s) -- keine Sonderrechte ueber die Loader-Cap.
            let ObjectKind::Loader { .. } = kind else {
                return deny(result::ERR_BADCAP);
            };
            if !rights.contains(Rights::WRITE) {
                return deny(result::ERR_RIGHTS);
            }
            let index = frame_reg(frame, reg::MSG0) as u32; // x2: Archiv-Programm-Index
            let liste = frame_reg(frame, reg::MSG0 + 1); // x3: gepackte (src,dst)-Paare
            let anzahl = frame_reg(frame, reg::MSG0 + 2) as usize; // x4: wie viele davon gelten
            // x5: der **Ressourcenwunsch** der neuen PD -- DMA-Seiten und Cap-Budget in einem
            // Wort, Belegung ausgeschrieben in `caprock_abi::load_extras`. Beide `0` = Vorgabe.
            //
            // Sie stehen hier und nicht im Manifest, und das ist eine Entscheidung: das Manifest
            // beschreibt das Startverhalten und wird signiert; eine Treiberumgebung, die dreissig
            // Slots und einen grossen Pool braucht, wird nicht gebootet, sondern geladen. Ueber
            // ihre jeweilige Schranke hinaus wird **abgewiesen und nicht gedeckelt** -- eine
            // Schranke, die klammheimlich kuerzt, laesst den Aufrufer erst beim achten Cap merken,
            // dass er etwas anderes bekommen hat, als er angefordert hat; bei DMA merkt es
            // niemand, sondern das Geraet schreibt ueber das Ende hinaus.
            let (dma_pages, cap_budget) =
                caprock_abi::load_extras_unpack(frame_reg(frame, reg::MSG0 + 3));
            // **Die DMA-Absage steht VOR jeder Ableitung** (C2/D11). Danach haengen abgeleitete
            // Endowment-Caps in der Tabelle, die ein Abweispfad einzeln wieder loeschen muesste --
            // genau die Aufraeumpflicht, die die zwei C8-Ausgaenge schon einmal geerbt haben, ohne
            // sie zu erfuellen. Hier ist noch nichts alloziert, also kostet die Absage nichts.
            // Und der Aufrufer wird **nicht** blockiert: D11 woertlich.
            if dma_pages > caprock_abi::DRIVER_DMA_MAX_PAGES {
                return deny(result::ERR_DMA_TOO_LARGE);
            }
            // Den zu delegierenden Caller-Cap (x3) aufloesen + ableiten (CDT-Kind, gleiche Rechte)
            // unter CAPS.write; danach CAPS freigeben -- der Loader re-lockt CAPS/MEM/SCHEDS selbst.
            //
            // **Badge (x4):** wer eine Notification/einen Endpoint weiterreicht, darf der Kopie ein
            // eigenes Badge geben. Das ist keine Bequemlichkeit: `SYS_SIGNAL` verodert das Badge der
            // benutzten CAP, nicht ein Nachrichtenwort — ohne unterschiedliche Badges beim Vergeben
            // sind die Signale zweier Kinder ununterscheidbar. Erlaubt ist es, weil der Aufrufer das
            // Objekt bereits besitzt: er vergibt ein Etikett auf seiner eigenen Autorität, er
            // erwirbt keine neue. `0` = Badge des Originals erben (bisheriges Verhalten).
            // **Mehr als eine Delegation** (2026-08-25). Bis dahin genau eine, nach Slot 0.
            //
            // Fail-closed an drei Stellen, und jede hat einen eigenen Grund:
            //   * mehr Paare als [`LOAD_MAX_DELEGATES`] -> die Liste kann gar nicht so lang sein,
            //     das ist ein Aufruffehler und kein Ressourcenmangel;
            //   * ein Quell-Slot, den der Aufrufer nicht haelt -> er delegiert, was er nicht hat;
            //   * **zwei Paare auf denselben Ziel-Slot** -> welcher der beiden dort landet, kann
            //     niemand sagen. Genau die Sorte stiller Wahl, gegen die A-5.4 den `service_id`
            //     eingefuehrt hat.
            //
            // **Und die Rueckabwicklung ist vollstaendig**: schlaegt Paar `k` fehl, werden die
            // Kopien 0..k geloescht. Ein halb abgebrochenes `LOAD` liesse abgeleitete Caps in der
            // geteilten Tabelle liegen, die niemand mehr findet — und die das `delete` ihres
            // Eltern-Caps blockieren.
            // Delegation einsammeln (eine Stelle fuer beide Lade-Arme, s. `sammle_endow`).
            let gesammelt = match sammle_endow(caps, pd, liste, anzahl, delete_cap) {
                Ok(g) => g,
                Err(code) => return deny(code),
            };
            // `CapPtr` hat bewusst keinen oeffentlichen Konstruktor -- erst als `Option` sammeln,
            // dann mit dem ersten echten Cap als Fuellwert verdichten. Derselbe Weg wie in
            // `start_root_task` und `load_by_index`.
            let mut dicht: [(usize, CapPtr); caprock_abi::LOAD_MAX_DELEGATES];
            let endow: &[(usize, CapPtr)] = match gesammelt.iter().flatten().next() {
                None => &[],
                Some(&fuell) => {
                    dicht = [fuell; caprock_abi::LOAD_MAX_DELEGATES];
                    let mut n = 0;
                    for e in gesammelt.iter().flatten() {
                        dicht[n] = *e;
                        n += 1;
                    }
                    &dicht[..n]
                }
            };
            // **C8: der Auftrag wandert, der Aufrufer wartet.**
            //
            // Bis dahin lief `load` hier synchron -- also Ed25519 + SHA-2 auf dem 16-KiB-
            // Kernel-Stack des aufrufenden EL0-Threads (gemessen: 73 % davon). Jetzt reicht der
            // Dispatch den Auftrag an den Verifiziererthread und blockiert den Aufrufer regulaer;
            // das Ergebnis schreibt der Verifizierer in genau diesen Frame, bevor er den
            // Wartegrund entfernt.
            //
            // Die drei Ausgaenge sind unterscheidbar, und das ist keine Kosmetik: „gerade voll"
            // wiederholt man, „gibt es nicht" nicht.
            match load(index, pd, endow, core, frame, cap_budget, dma_pages as u32) {
                LadeUebergabe::Uebergeben(next) => next,
                // **Die abgeleiteten Caps muessen zurueck, wenn der Auftrag nicht angenommen
                // wird.** Auf dem angenommenen Weg raeumt `load_by_index` sie bei Misserfolg auf;
                // hier kommt der Loader gar nicht erst dran. Ohne diese Zeilen lecken sie als
                // verwaiste CDT-Kinder und blockieren sogar das `delete` des Eltern-Caps --
                // dieselbe Falle, die der Abweispfad im Loader schon einmal bezahlt hat, nur an
                // einer Stelle, die es vor C8 nicht gab.
                LadeUebergabe::Ausgelastet => {
                    for e in gesammelt.iter().flatten() {
                        let _ = delete_cap(e.1);
                    }
                    deny(result::ERR_LOAD_BUSY)
                }
                LadeUebergabe::KeinVerifizierer => {
                    for e in gesammelt.iter().flatten() {
                        let _ = delete_cap(e.1);
                    }
                    deny(result::ERR_SERVER_GONE)
                }
            }
        }
        // LXPD-Laufzeitpfad (`SYS_LOAD_IMAGE = 36`): Bild aus Aufrufer-RAM statt Archiv.
        //
        // Belegung wie `LOAD` (`MSG0` = Programm-ID aus dem Boot-Manifest, `MSG1` =
        // Delegationsliste, `MSG2` = Anzahl, `MSG3` = Ressourcenwunsch), dazu `TAG`:
        // Low-Byte = Slot der Memory-Cap mit dem Bild, Bits 8..40 = exakte Bildlaenge,
        // Bits 40..64 muessen `0` sein. Manifest-Cap braucht es keine: Vertrauen kommt
        // aus dem Boot-Manifest (Hash-Gleichheit), nicht aus mitgereichten Bytes.
        sys::LOAD_IMAGE => {
            let ObjectKind::Loader { .. } = kind else {
                return deny(result::ERR_BADCAP);
            };
            if !rights.contains(Rights::WRITE) {
                return deny(result::ERR_RIGHTS);
            }
            let tag = frame_reg(frame, reg::TAG);
            let bild_slot = (tag & 0xff) as usize;
            let bild_len = ((tag >> 8) & 0xffff_ffff) as u64;
            // Stille Abschneidungen gibt es nicht: hohe Bits gesetzt heisst „falsches
            // Format", nicht „langes Bild".
            if tag >> 40 != 0 {
                return deny(result::ERR_BADCAP);
            }
            if bild_len == 0 || bild_len > caprock_abi::sys::LXPD_MAX_BILD {
                return deny(result::ERR_BADCAP);
            }
            let pid = frame_reg(frame, reg::MSG0) as u32;
            let liste = frame_reg(frame, reg::MSG0 + 1);
            let anzahl = frame_reg(frame, reg::MSG0 + 2) as usize;
            let (dma_pages, cap_budget) =
                caprock_abi::load_extras_unpack(frame_reg(frame, reg::MSG0 + 3));
            // DMA-Schranke VOR jeder Ableitung (C2/D11) — dieselbe Ordnung wie bei `LOAD`.
            if dma_pages > caprock_abi::DRIVER_DMA_MAX_PAGES {
                return deny(result::ERR_DMA_TOO_LARGE);
            }
            // Bild-Geometrie gegen die Aufrufer-Memory-Cap: muss plain-RAM sein (kein
            // Geraet — Geraeteregister sind keine ladbaren Images) und die Laenge tragen.
            // Gelesen wird unter Aufrufer-Autoritaet, also braucht es READ.
            let bild_phys = {
                let g = caps.read();
                let Some(c) = g.pds.cap_at(pd, bild_slot) else {
                    return deny(result::ERR_BADCAP);
                };
                let Some((kind_b, rechte_b, _)) = g.cspace.lookup(c) else {
                    return deny(result::ERR_BADCAP);
                };
                if !rechte_b.contains(Rights::READ) {
                    return deny(result::ERR_RIGHTS);
                }
                let ObjectKind::Memory(r) = kind_b else {
                    return deny(result::ERR_BADCAP);
                };
                if r.len < bild_len {
                    return deny(result::ERR_BADCAP);
                }
                r.base
            }; // CAPS freigegeben — der Kernel sperrt selbst
            // Delegation wie `LOAD` (eine Stelle, s. `sammle_endow`).
            let gesammelt = match sammle_endow(caps, pd, liste, anzahl, delete_cap) {
                Ok(g) => g,
                Err(code) => return deny(code),
            };
            let mut dicht: [(usize, CapPtr); caprock_abi::LOAD_MAX_DELEGATES];
            let endow: &[(usize, CapPtr)] = match gesammelt.iter().flatten().next() {
                None => &[],
                Some(&fuell) => {
                    dicht = [fuell; caprock_abi::LOAD_MAX_DELEGATES];
                    let mut n = 0;
                    for e in gesammelt.iter().flatten() {
                        dicht[n] = *e;
                        n += 1;
                    }
                    &dicht[..n]
                }
            };
            // C8-Form wie `LOAD`: Auftrag an den Verifizierer, Aufrufer blockiert. Der
            // Kernel kopiert EINMAL in Staging und traut danach nur der Kopie.
            match load_image(
                bild_phys,
                bild_len,
                pid,
                pd,
                endow,
                core,
                frame,
                cap_budget,
                dma_pages as u32,
            ) {
                LadeUebergabe::Uebergeben(next) => next,
                LadeUebergabe::Ausgelastet => {
                    for e in gesammelt.iter().flatten() {
                        let _ = delete_cap(e.1);
                    }
                    deny(result::ERR_LOAD_BUSY)
                }
                LadeUebergabe::KeinVerifizierer => {
                    for e in gesammelt.iter().flatten() {
                        let _ = delete_cap(e.1);
                    }
                    deny(result::ERR_SERVER_GONE)
                }
            }
        }
        // Prozessmodell FORK/EXEC (31/32) — fail-closed bis zum Kernel-Patch.
        //
        // Die Nummern sind vergeben (`caprock-abi::sys::{FORK_SNAPSHOT, EXEC_REPLACE}`),
        // der Pfad ist es nicht: der Kernel kennt noch keine Rueckrufe dafuer (der
        // EXAKTE Patch-Text steht in der Moduldoku von `proc`). Was dieser Zweig tut,
        // ist die Vorpruefung aus `proc` (reserviert == 0, Token != 0, Laenge
        // gedeckelt) — ohne einen einzigen Seiteneffekt — und danach die ehrliche
        // Absage: `ERR_BADSYS` heisst hier „Antrag ok, Pfad fehlt", nicht „unbekannte
        // Nummer". Ein gueltiger Antrag wird also weder ausgefuehrt noch still
        // geduldet. Der Patch ersetzt den `BADSYS`-Ausgang durch die Rueckrufe
        // `fork(kind_slot, max_len, prio)` / `exec(loader_cap, prog_index, token)`.
        sys::FORK_SNAPSHOT => {
            let m0 = frame_reg(frame, reg::MSG0);
            let m1 = frame_reg(frame, reg::MSG0 + 1);
            let m2 = frame_reg(frame, reg::MSG0 + 2);
            let m3 = frame_reg(frame, reg::MSG0 + 3);
            match proc::dekodiere_fork(m0, m1, m2, m3) {
                proc::ForkExecEntscheid::Abgewiesen(code) => deny(code),
                // Vorpruefung ok, aber kein Rueckruf verdrahtet (s. Kommentar oben).
                _ => deny(result::ERR_BADSYS),
            }
        }
        sys::EXEC_REPLACE => {
            let x1 = frame_reg(frame, reg::EP_BADGE);
            let m0 = frame_reg(frame, reg::MSG0);
            let m1 = frame_reg(frame, reg::MSG0 + 1);
            let m2 = frame_reg(frame, reg::MSG0 + 2);
            let m3 = frame_reg(frame, reg::MSG0 + 3);
            match proc::dekodiere_exec(x1, m0, m1, m2, m3) {
                proc::ForkExecEntscheid::Abgewiesen(code) => deny(code),
                _ => deny(result::ERR_BADSYS),
            }
        }
        _ => deny(result::ERR_BADSYS),
    }
}

/// Eine Rechte-Bitmaske aus der ABI (1=R, 2=W, 4=X) in [`Rights`] übersetzen. Unbekannte Bits
/// werden **ignoriert**, nicht abgelehnt: `Rights` kennt heute genau drei, und eine Ablehnung
/// würde ein Programm brechen, das aus Bequemlichkeit `u64::MAX` schickt — es bekommt so den
/// Schnitt mit dem, was es hat, und das ist die sichere Richtung.
fn rights_from_bits(mask: u64) -> Rights {
    let mut r = Rights::NONE;
    if mask & 1 != 0 {
        r = r.union(Rights::READ);
    }
    if mask & 2 != 0 {
        r = r.union(Rights::WRITE);
    }
    if mask & 4 != 0 {
        r = r.union(Rights::EXEC);
    }
    r
}

/// Eine Capability vom Server (lokaler Slot `grant_slot` in PD `server_pd`) an den
/// `caller` delegieren: im globalen `CapSpace` ableiten (Kind im CDT) und in den
/// Cspace des Aufrufer-PDs an [`GRANT_RECV_SLOT`](caprock_abi::GRANT_RECV_SLOT)
/// eintragen. Läuft unter dem gehaltenen `CAPS`-Lock (vor dem REPLY-Rendezvous, also
/// bevor der Aufrufer geweckt wird/laufen kann).
///
/// **Rückgabe:** die Cap, die im Empfangs-Slot **verdrängt** wurde (falls dort schon eine
/// lag) — der Aufrufer MUSS sie anschließend löschen. Ohne das bliebe sie für immer im
/// globalen `CapSpace` belegt, ohne von irgendeiner PD noch erreichbar zu sein: ein Server,
/// der wiederholt mit `GRANT_FLAG` antwortet, könnte so die **systemweit geteilte**
/// Slot-/Objekt-Tabelle erschöpfen und allen PDs jede weitere Cap-Installation verwehren.
/// Das Löschen passiert bewusst NICHT hier: die Finalisierung kann Speicher freigeben
/// (Allokator) bzw. einen Call abbrechen (EPS/SCHEDS) — beides ist unter dem hier
/// gehaltenen `CAPS`+`EPS`-Paar sperrordnungswidrig. Der Dispatch erledigt es, nachdem
/// beide Locks gefallen sind (s. [`dispatch`]).
fn grant_cap(
    caps: &mut Caps,
    caller: ThreadId,
    server_pd: usize,
    grant_slot: usize,
) -> Option<CapPtr> {
    let src = caps.pds.cap_at(server_pd, grant_slot)?;
    // A tagged IPC capability may only be passed on by a holder of the GRANT permission.
    if !caps.cspace.may_grant(src) {
        return None;
    }
    let cpd = caps.pds.pd_of(caller)?;
    // **Domänen-Policy VOR der Ableitung prüfen** (konsistent zum zentralen Enforcement-Punkt
    // install_cap_checked) — sonst könnte ein Server eine policy-fremde Cap (z. B. HW-/Loader-/
    // PdControl-Cap) per Grant in eine fremde Domäne schleusen (Bypass; bisher nur von domain_audit
    // nachträglich DETEKTIERT statt VERHINDERT). Die Kopie hat dieselbe Art/Id wie `src` -> die
    // Prüfung auf `src` ist äquivalent; bei Ablehnung wird gar keine Kopie erzeugt (kein Leck).
    if !caps.cap_allowed(cpd, src) {
        return None;
    }
    // Budget des EMPFÄNGERS prüfen, bevor eine Kopie entsteht (ein Grant darf das Cap-Budget
    // einer PD nicht umgehen — sonst wäre der Grant-Pfad das Schlupfloch um die Schranke aus
    // `install_cap_checked`). Ein belegter Empfangs-Slot wird ersetzt, nicht zusätzlich belegt.
    // A-3.2: **der Empfänger** bestimmt den Slot (`SYS_SETRECV`), nicht der Sender. Ohne das
    // landete jede gegrantete Cap im festen Slot 1 — ein Server konnte damit den Cap verdrängen,
    // den sein Client dort zufällig hielt. Wer den Slot wählt, schreibt in fremdes Eigentum.
    let recv_slot = caps.pds.recv_slot(cpd);
    if !caps.budget_allows(cpd, recv_slot) {
        return None;
    }
    // Cap ableiten (erbt die Rechte) und beim Aufrufer eintragen (Policy bereits geprüft).
    // Der zuvor dort liegende Cap wird verdrängt und zum Löschen zurückgemeldet.
    match caps.cspace.copy(src, Rights::RWX) {
        Ok(new_cap) => {
            let replaced = caps.pds.cap_at(cpd, recv_slot);
            caps.pds.install_cap(cpd, recv_slot, new_cap);
            replaced
        }
        Err(_) => None,
    }
}
