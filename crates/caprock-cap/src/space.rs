//! Capability-Space: Slot-Tabelle + Capability-Derivation-Tree (CDT).

use crate::object::{DmaCoherence, DmaDir, Object, ObjectKind};
use caprock_abi::ipc_perm;
use caprock_mem::{MemoryCap, PhysAllocator, PhysRegion, Rights};
use caprock_slab::Slab;

/// Sicheres Capability-Handle: Slot-Index + Generation (erkennt stale Pointer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapPtr {
    slot: usize,
    gen: u32,
}

/// Fehler einer Capability-Operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapError {
    /// Kein freier Capability-Slot mehr.
    NoSlot,
    /// Kein freier Objekt-Eintrag mehr.
    NoObject,
    /// Ungültiges/abgelaufenes Handle.
    Invalid,
    /// Operation auf einem Cap mit Kindern (erst `revoke` nötig).
    HasChildren,
    /// Die Region erfüllt eine **Ausrichtungs-/Granularitätsbedingung** nicht (ext-35: ein
    /// DMA-Puffer muss auf dem Cache-Writeback-Granule liegen, sonst zerstört die
    /// Cache-Wartung fremde Daten in einer angebrochenen Zeile).
    Unaligned,
    /// Die **Region ist zu klein für die Zusicherung, die die Cap trägt** (Z26/A3): ein
    /// Sidecar-Fenster muss **alle** Slots decken, die die Belegungsmaske vergeben kann.
    ///
    /// Eigener Name und nicht [`Self::Unaligned`], weil es eine andere Diagnose ist: „falsch
    /// ausgerichtet" heisst „verschiebe sie", „zu klein" heisst „nimm mehr Speicher". Eine
    /// Absage, die zwei Ursachen unter einem Namen führt, macht die Behebung zum Raten.
    ZuKlein,
    /// Das gewünschte **Teilfenster liegt nicht in der Quell-Region** (CSUB,
    /// Mem-Server-Transport): Überlauf in `Basis + Offset` oder `Offset + Länge`, leeres
    /// Fenster (`len == 0`) oder Ausschnitt jenseits der Cap.
    ///
    /// Eigener Name und nicht [`Self::Invalid`], weil die Behebung eine andere ist („kleiner
    /// fragen", nicht „andere Cap nehmen") — und weil ein Fenster ausserhalb der Cap die Form
    /// ist, die ein Angreifer produziert, nicht die eines Tippfehlers. Der Dispatch bildet ihn
    /// auf ABI `ERR_SUBREGION` (21) ab — derselbe Code, den die Stapelprüfung (K1b) für ein
    /// Fenster ausserhalb der Stack-Cap vergibt.
    Subregion,
}

/// Ableitungs-Metadaten eines Slots (MDB-Knoten / CDT-Verkettung).
#[derive(Clone, Copy)]
pub(crate) struct Mdb {
    parent: Option<usize>,
    first_child: Option<usize>,
    next_sibling: Option<usize>,
    prev_sibling: Option<usize>,
}

impl Mdb {
    const EMPTY: Mdb = Mdb {
        parent: None,
        first_child: None,
        next_sibling: None,
        prev_sibling: None,
    };
}

/// Ein Capability-Slot (CTE: Capability + MDB-Knoten).
///
/// **Öffentlich, aber undurchsichtig** — aus demselben Grund wie [`Object`]: der Kernel legt
/// die Slot-Tabelle zur Boot-Zeit an (`Slab<CapSlot>`) und braucht dafür Typ und
/// [`CapSlot::EMPTY`]. Rechte, Badge und CDT-Verkettung bleiben `pub(crate)`.
#[derive(Clone, Copy)]
pub struct CapSlot {
    pub(crate) used: bool,
    pub(crate) gen: u32,
    pub(crate) object: usize, // Index in die Objekt-Tabelle
    pub(crate) rights: Rights,
    pub(crate) badge: u64,
    /// **Das Teilfenster dieser Cap** (CSUB): `Some(f)` heisst „diese Cap autorisiert nur
    /// `f`", `None` heisst „die ganze Objekt-Region".
    ///
    /// Es steht am **Slot** und nicht am Objekt, und das ist der Punkt: alle Ableitungen
    /// teilen sich **ein** Objekt (der CDT-Audit verlangt genau das — Eltern und Kind
    /// zeigen auf dasselbe Objekt, Code 4 sonst), und die Freigabe der Region läuft einmal
    /// über dessen Referenzzähler. Ein eigenes Objekt je Teilfenster gäbe die
    /// Teilregion bei der Finalisierung an den Allokator zurück — mitten aus der noch
    /// lebenden Eltern-Region heraus. `copy`/`mint`/`move_cap` übernehmen das Fenster;
    /// `lookup`/`inspect`/`kind_of` melden die wirksame (verengte) Region.
    pub(crate) subregion: Option<PhysRegion>,
    /// **IPC permission mask** (see `caprock_abi::ipc_perm`): `Some(mask)` marks this slot as a
    /// *tagged* endpoint capability whose mask the kernel delivers to the server in the badge
    /// word; `None` is an ordinary capability and behaves exactly as before.
    ///
    /// It lives at the slot, like `rights` and `badge`, because every derivation shares one
    /// object. A derivation can only clear bits (`derive_ipc` intersects), so along any path in
    /// the CDT the mask never grows -- `audit_cdt` code 9 checks that.
    pub(crate) perms: Option<u16>,
    pub(crate) mdb: Mdb,
}

impl CapSlot {
    pub const EMPTY: CapSlot = CapSlot {
        used: false,
        gen: 0,
        object: 0,
        rights: Rights::NONE,
        badge: 0,
        subregion: None,
        perms: None,
        mdb: Mdb::EMPTY,
    };
}

// Die Kapazität, die ein `Finalized` mitbringen muss, damit keine Meldung verlorengeht, ist die
// Größe der Objekttabelle: mehr Objekte als sie fasst kann eine einzelne Operation nicht
// finalisieren. Bis A-3.4 stand das hier als Konstante `MAX_FINALIZED = NOBJECTS`; seit die
// Tabelle boot-dimensioniert ist, ist es keine Compile-Zeit-Zahl mehr, sondern
// `CapSpace::finalize_capacity()` — dieselbe *eine* Quelle, nur zur Laufzeit gelesen.

/// Was beim Löschen/Revoke **finalisiert** wurde und vom Kernel nachbereitet werden muss.
///
/// Das Cap-System kennt weder das IPC- noch das DMA-Subsystem. Es meldet nur, *was* finalisiert
/// wurde; die Nachbereitung führt der Kernel aus, außerhalb der Sperren dieser Crate:
///
/// * **Reply-Caps** — `(ep, caller)`-Paare, deren Call abzubrechen ist (`ERR_SERVER_GONE`).
/// * **DMA-Regionen** — `(phys, len)`. Diese werden hier **nicht** mehr freigegeben (ext-37).
///   Vorher rief `delete_leaf` direkt `alloc.free_region`, und dass davor stillgelegt,
///   unmappt und synchronisiert wurde, war eine *bewiesene* Vorbedingung des Gesamtsystems —
///   keine erzwungene. Diese Crate kennt weder Gerät noch Enforcer und kann sie nicht prüfen;
///   also gibt sie die Region zurück, statt sie freizugeben, und der Kernel darf sie nur über
///   einen Nachweis (`DmaTeardownToken`) tatsächlich zurückgeben.
///
/// **Der Rückmeldepuffer gehört dem Aufrufer** (A-3.3). Bis hierher trug diese Struktur zwei
/// `[_; NOBJECTS]`-Arrays **in sich** und wurde im Kernel als lokale Variable angelegt — also rund
/// 4 KiB auf dem Kernelstack, je Aufruf von `delete`/`revoke`. Damit hing `NOBJECTS` an der
/// Stackgröße: die Objekttabelle zu vergrößern (A-3.4, todo C3) hätte den Stack mitvergrößert,
/// und zwar den **jedes** Threads.
///
/// Jetzt sind es geliehene Slices. Der Kernel legt den Puffer einmal statisch an; sobald die
/// Tabellen dynamisch aus dem RAM kommen, kommt der Puffer aus derselben Quelle — **ohne dass
/// diese Crate sich ändert**. Sie bleibt allokationsfrei und ohne `unsafe`.
pub struct Finalized<'a> {
    // Ein `revoke` kann eine ganze CDT-Teilkette loeschen und dabei MEHRERE Reply-Objekte
    // finalisieren. Die Kapazitaet MUSS die maximal in einer Operation finalisierbaren Reply-Objekte
    // fassen (hart begrenzt durch die Objekttabelle NOBJECTS) -- sonst wuerden ueberzaehlige
    // (ep, caller)-Paare still verworfen und ihre CALL-Aufrufer nie abgebrochen -> sie haengen
    // dauerhaft (Liveness-Bug). NOBJECTS ist die beweisbar vollstaendige Schranke (n <= len garantiert).
    items: &'a mut [(u32, u64)],
    n: usize,
    /// Finalisierte DMA-Regionen `(phys, len)`. Dieselbe Schranke aus demselben Grund: ein
    /// `revoke` über einen Teilbaum kann mehrere DMA-Objekte auf einmal finalisieren, und eine
    /// still verworfene Region wäre ein Leck **und** eine stehende Übersetzung.
    dma: &'a mut [(u64, u64)],
    dn: usize,
    /// **PDs, deren letzte `DebugControl` gerade verschwunden ist** (Z6b).
    ///
    /// Dieselbe Schranke aus demselben Grund wie oben, und der Schaden ist von derselben Art: eine
    /// still verworfene Meldung ist ein Thread, der `BlockReasons::DEBUG` traegt und den **niemand**
    /// mehr entfernen darf. Er laeuft nie wieder — und von aussen ist das von einem Deadlock nicht
    /// zu unterscheiden. Wortgleich zum Kommentar ueber `items`: „man sieht nur einen Thread, der
    /// steht".
    debug: &'a mut [u16],
    gn: usize,
    /// Wurde eine Meldung verworfen, weil die Kapazität nicht reichte? Das darf nach der
    /// `NOBJECTS`-Schranke nicht vorkommen; steht hier, damit „kann nicht vorkommen" prüfbar
    /// ist statt behauptet.
    ///
    /// **Seit A-3.3 ist das keine Formalie mehr.** Solange die Arrays in dieser Struktur lagen,
    /// war die Kapazität durch den Typ garantiert; jetzt bringt der Aufrufer sie mit und kann sie
    /// zu klein wählen. Deshalb **muss** [`Finalized::overflowed`] ausgewertet werden — der Kernel
    /// tut das (`system::cap_delete`/`cap_revoke`, Audit-Code 70). Bis A-3.3 rief diese Methode
    /// **niemand**: die Prüfbarkeit war vorhanden, die Prüfung nicht.
    overflow: bool,
}

impl<'a> Finalized<'a> {
    /// Einen Kollektor über geliehenem Speicher anlegen. Beide Slices sollten mindestens
    /// [`CapSpace::finalize_capacity`] Einträge fassen; sind sie kürzer, geht keine Meldung
    /// *unbemerkt* verloren — [`Finalized::overflowed`] wird gesetzt.
    pub fn new(items: &'a mut [(u32, u64)], dma: &'a mut [(u64, u64)]) -> Self {
        Self::mit_debug(items, dma, &mut [])
    }
    /// Wie [`Finalized::new`], zusaetzlich mit der Ablage fuer freizugebende Debug-Ziele (Z6b).
    ///
    /// **Getrennter Konstruktor statt eines vierten Arguments an `new`**, damit die vorhandenen
    /// Aufrufer unveraendert bleiben und der Unterschied SICHTBAR ist: wer `new` benutzt, bekommt
    /// eine Ablage der Laenge 0 und damit bei der ersten Debug-Meldung ein `overflowed()` — also
    /// eine **laute** Absage statt einer stillen. Ein Vorgabewert, der schweigt, waere hier die
    /// falsche Bequemlichkeit.
    pub fn mit_debug(
        items: &'a mut [(u32, u64)],
        dma: &'a mut [(u64, u64)],
        debug: &'a mut [u16],
    ) -> Self {
        Self { items, n: 0, dma, dn: 0, debug, gn: 0, overflow: false }
    }
    fn push(&mut self, ep: u32, caller: u64) {
        if self.n < self.items.len() {
            self.items[self.n] = (ep, caller);
            self.n += 1;
        } else {
            self.overflow = true;
        }
    }
    fn push_dma(&mut self, phys: u64, len: u64) {
        if self.dn < self.dma.len() {
            self.dma[self.dn] = (phys, len);
            self.dn += 1;
        } else {
            self.overflow = true;
        }
    }
    fn push_debug(&mut self, pd: u16) {
        if self.gn < self.debug.len() {
            self.debug[self.gn] = pd;
            self.gn += 1;
        } else {
            self.overflow = true;
        }
    }
    /// Die PDs, deren Debug-Halt freizugeben ist (Z6b). Vom Kernel **nach** dem Freigeben von
    /// `CAPS` abgearbeitet — dieselbe Ordnung wie [`Finalized::iter`].
    pub fn iter_debug(&self) -> impl Iterator<Item = u16> + '_ {
        self.debug[..self.gn].iter().copied()
    }
    /// Die finalisierten `(ep, caller)`-Paare (für den Kernel zum Abbrechen der Calls).
    pub fn iter(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.items[..self.n].iter().copied()
    }
    /// Die finalisierten DMA-Regionen `(phys, len)` — **noch nicht freigegeben**.
    pub fn iter_dma(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.dma[..self.dn].iter().copied()
    }
    /// Dieselben Regionen als **zusammenhängender Slice**. Der Kernel reicht sie so direkt an den
    /// DMA-Enforcer weiter; vorher kopierte er sie erst in ein zweites Stack-Array derselben
    /// Größe um, das genau diese Form herstellte.
    pub fn dma_regions(&self) -> &[(u64, u64)] {
        &self.dma[..self.dn]
    }
    /// Ging eine Meldung verloren? (Muss immer `false` sein.)
    pub fn overflowed(&self) -> bool {
        self.overflow
    }
}

// --- Zählgrenzen der CDT-Läufe (B-5.5) --------------------------------------------------------
//
// Jede Schleife über den CDT läuft über eine Struktur, die **Mandanten mitgestalten**: jedes
// `copy`/`mint` hängt einen Knoten hinein. Solange niemand sagt, wie lang ein Lauf höchstens wird,
// ist „der Kernel ist reaktiv" eine Hoffnung — und hier ist es mehr als eine Latenzfrage: ein
// zyklisch verketteter CDT liesse `revoke` **endlos** laufen, und zwar unter der CAPS-Sperre, die
// dann niemand mehr freigibt. Aus einer Datenstrukturanomalie würde ein stehender Knoten.
//
// Die Grenze ist bewusst eine **Operationszahl**, keine Zeit: Zyklen hängen an Taktrate und
// Emulation, Schritte hängen an der Struktur. Und sie ist nicht gegriffen, sondern hergeleitet:
// ein azyklischer Lauf besucht keinen Slot zweimal, kann also nie mehr Schritte tun, als es Slots
// gibt. Genau diese Schranke benutzt [`CapSpace::audit_cdt`] seit jeher für ihren Code 7 — bisher
// als zwei eingestreute `steps > nslots`, jetzt als eine benannte Stelle.
//
// **Die Grenze schneidet nichts Legitimes ab.** Sie ist erst erreichbar, wenn die Baumform bereits
// verletzt ist. Ihr Erreichen ist deshalb ein *gezählter Befund*
// ([`CapSpace::cdt_walk_overruns`]) und kein Normalbetrieb — dieselbe Form wie
// [`Finalized::overflowed`]: „kann nicht vorkommen" muss prüfbar sein statt behauptet.

/// Von `start` aus zum ersten Blatt absteigen (immer `first_child`).
///
/// `Ok((blatt, schritte))`, oder `Err(())`, wenn die Schrittgrenze überschritten wurde oder ein
/// Kindindex ausserhalb der Tabelle liegt — beides heisst: der CDT ist **nicht baumförmig**.
///
/// Freie Funktion über einem Slice statt Methode, damit sie ohne `CapSpace` (und damit ohne
/// [`Slab`](caprock_slab::Slab), dessen Konstruktion `unsafe` ist) geprüft werden kann. Diese
/// Crate ist `forbid(unsafe_code)`; ein Test, der die Grenze belegen soll, muss also ohne
/// Tabellen-Handle auskommen.
fn descend_to_leaf(slots: &[CapSlot], start: usize, limit: usize) -> Result<(usize, usize), ()> {
    let mut leaf = start;
    let mut steps = 0usize;
    loop {
        let Some(s) = slots.get(leaf) else {
            return Err(()); // Index ausserhalb der Tabelle -> Verkettung kaputt
        };
        let Some(c) = s.mdb.first_child else {
            return Ok((leaf, steps));
        };
        steps += 1;
        if steps > limit {
            return Err(());
        }
        leaf = c;
    }
}

/// Länge der Kinderliste von `parent`, mit derselben Schranke und aus demselben Grund.
fn count_children(slots: &[CapSlot], parent: usize, limit: usize) -> Result<usize, ()> {
    let mut n = 0usize;
    let mut cur = match slots.get(parent) {
        Some(s) => s.mdb.first_child,
        None => return Err(()),
    };
    while let Some(i) = cur {
        n += 1;
        if n > limit {
            return Err(());
        }
        let Some(s) = slots.get(i) else {
            return Err(());
        };
        cur = s.mdb.next_sibling;
    }
    Ok(n)
}

/// Das CSUB-Teilfenster ausrechnen — die reine Entscheidung von [`CapSpace::subregion`].
///
/// `whole` ist die **wirksame** Region der Quell-Cap (Objekt-Region oder deren Teilfenster),
/// `offset`/`len` der gewünschte Ausschnitt. Alles `u64`, alles geprüft: ein Überlauf in
/// `Basis + Offset` oder `Offset + Länge`, ein leeres Fenster und ein Ausschnitt jenseits von
/// `whole` werden mit [`CapError::Subregion`] abgewiesen — **nicht abgeschnitten**. Ein
/// abgeschnittenes Fenster gäbe dem Aufrufer weniger, als er abbildet, und der Fehler fiele
/// erst beim Zugriff auf, nicht beim Ableiten.
///
/// Freie Funktion über eingespeisten Werten (Muster `descend_to_leaf`, `spawncheck`):
/// [`CapSpace::subregion`] ruft sie auf, statt die Rechnung nachzubauen — eine Prüfung an
/// zwei Stellen ist zwei Prüfungen, und die zweite altert.
pub(crate) fn teilfenster_rechnen(
    whole: PhysRegion,
    offset: u64,
    len: u64,
) -> Result<PhysRegion, CapError> {
    if len == 0 {
        return Err(CapError::Subregion);
    }
    let base = whole.base.checked_add(offset).ok_or(CapError::Subregion)?;
    let ende = base.checked_add(len).ok_or(CapError::Subregion)?;
    let whole_ende = whole
        .base
        .checked_add(whole.len)
        .ok_or(CapError::Subregion)?;
    if ende > whole_ende {
        return Err(CapError::Subregion);
    }
    Ok(PhysRegion::new(base, len))
}

/// `child` vorne in die Kinderliste von `parent` einhängen — die reine Verkettung von
/// [`CapSpace::link_child`], als freie Funktion über einem Slice, damit sie ohne `CapSpace`
/// (und damit ohne `Slab`, dessen Anbindung `unsafe` ist) geprüft werden kann.
///
/// Der Test unten belegt damit „Kind im CDT" auf dem Host: Wer das Fenster ableitet, hängt
/// es als Kind ein — und `revoke` läuft über genau diese Kette.
fn kind_einhaengen(slots: &mut [CapSlot], parent: usize, child: usize) {
    let old_first = slots[parent].mdb.first_child;
    slots[child].mdb.parent = Some(parent);
    slots[child].mdb.prev_sibling = None;
    slots[child].mdb.next_sibling = old_first;
    if let Some(f) = old_first {
        slots[f].mdb.prev_sibling = Some(child);
    }
    slots[parent].mdb.first_child = Some(child);
}

/// Lesbare Sicht auf einen Capability (für Diagnose/Tests).
#[derive(Clone, Copy, Debug)]
pub struct CapInfo {
    pub kind: ObjectKind,
    pub rights: Rights,
    pub badge: u64,
    pub refcount: u32,
    pub child_count: usize,
    /// **Ist dies die WURZEL des CDT-Teilbaums** (kein Elter)?
    ///
    /// Fuer Z6b eine Autoritaetsfrage und keine Auskunft: die `Debuggable`-Wurzel darf **ableiten**
    /// und sonst nichts — weder lesen noch anhalten. Ohne diese Unterscheidung waere „gewaehrt
    /// selbst nichts" nicht formulierbar, denn Rechte allein koennen Wurzel und Kind nicht trennen.
    pub is_root: bool,
}

/// Ein Capability-Space: Slot-Tabelle + Objekt-Tabelle.
///
/// **Beide Tabellen sind zur Boot-Zeit dimensioniert** (A-3.4, Teil 2), nicht mehr
/// `[_; NSLOTS]`/`[_; NOBJECTS]` im `.bss`. Der Grund steht in [`CapSpace::peak_slots`]: das
/// Cap-Budget deckelt den Verbrauch **einer** PD, die **Summe** über alle PDs prüfte niemand —
/// `NPDS * CAP_BUDGET_PER_PD` = 2048 stand gegen 256 vorhandene Slots. 32 PDs mit vollem Budget
/// füllten die Tabelle, die 33. bekam nichts. Das war kein Fehler *im* Budget, sondern eine
/// Zusage, die die Tabelle nicht einlösen konnte.
///
/// Ein frisch konstruierter `CapSpace` hat **Kapazität 0** — jede Installation scheitert mit
/// [`CapError::NoSlot`], bis [`CapSpace::attach`] gelaufen ist. Das ist Absicht: ein vergessenes
/// `attach` fällt als sauberer Fehler auf, nicht als stiller Fehlzugriff.
pub struct CapSpace {
    slots: Slab<CapSlot>,
    objects: Slab<Object>,
    /// **Höchststand belegter Slots** seit dem Start (A-3.4).
    ///
    /// `used_slots()` sagt, wie voll die Tabelle *jetzt* ist — und das ist genau der Wert, der
    /// nichts über Erschöpfung aussagt: ein Lauf, der zwischendurch an die Grenze stieß und
    /// danach aufräumte, sieht am Ende harmlos aus. Die Fairness-Zusage des Cap-Budgets
    /// (`CAP_BUDGET_PER_PD`) hängt aber am **gleichzeitigen** Verbrauch, nicht am Endstand.
    /// Deshalb wird der Höchststand mitgeführt statt hinterher gemessen.
    peak_slots: usize,
    /// Höchststand belegter Objekt-Einträge, aus demselben Grund.
    peak_objects: usize,
    /// **Längster CDT-Lauf** in Schritten seit dem Start (B-5.5) — die Zahl, die „reaktiv"
    /// überhaupt erst zu einer Aussage macht. Aus demselben Grund mitgeführt wie `peak_slots`:
    /// hinterher gemessen sieht ein Lauf harmlos aus, der zwischendurch an der Grenze war.
    peak_cdt_walk: usize,
    /// Grösster **Teilbaum**, den ein einzelnes `revoke` gelöscht hat (Anzahl `delete_leaf`).
    /// Das ist die zweite Hälfte der Zusage: ein Lauf kann kurz sein und trotzdem sehr oft
    /// stattfinden.
    peak_revoke_ops: usize,
    /// Wie oft hat ein Lauf die strukturelle Schranke erreicht? **Muss 0 sein.** Ist er es nicht,
    /// war der CDT nicht baumförmig — und die betroffene Operation ist unvollständig geblieben.
    cdt_walk_overruns: u32,
}

impl Default for CapSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl CapSpace {
    /// Ein **leerer** Space (Kapazität 0). `const`, damit `static`-Instanzen möglich bleiben;
    /// den Speicher gibt es erst per [`attach`](Self::attach).
    pub const fn new() -> Self {
        Self {
            slots: Slab::empty(),
            objects: Slab::empty(),
            peak_slots: 0,
            peak_objects: 0,
            peak_cdt_walk: 0,
            peak_revoke_ops: 0,
            cdt_walk_overruns: 0,
        }
    }

    /// Dem Space seine **Tabellen** geben — einmalig, beim Boot, bevor die erste Cap installiert
    /// wird.
    ///
    /// Die Slabs kommen **fertig angehängt** vom Aufrufer (der Kernel besorgt den Rohspeicher und
    /// trägt dessen `unsafe`-Vertrag). Diese Crate bleibt dadurch `forbid(unsafe_code)`: sie sieht
    /// nur zwei besitzende Handles, keine Zeiger.
    ///
    /// Ein zweiter Aufruf ist ein Programmierfehler und **panikt**, statt die alten Tabellen still
    /// zu vergessen — mit ihnen wären alle ausgegebenen [`CapPtr`] auf einen Schlag Verweise auf
    /// fremden Speicher, und zwar ohne dass irgendetwas es meldet.
    pub fn attach(&mut self, slots: Slab<CapSlot>, objects: Slab<Object>) {
        assert!(
            self.slots.is_empty() && self.objects.is_empty(),
            "CapSpace::attach zweimal gerufen"
        );
        self.slots = slots;
        self.objects = objects;
    }

    /// Kapazität `(Slots, Objekte)` — 0/0, solange nicht [`attach`](Self::attach)ed.
    pub fn capacity(&self) -> (usize, usize) {
        (self.slots.len(), self.objects.len())
    }

    /// **Die Kapazität, die ein [`Finalized`] mitbringen muss**, damit keine Meldung verlorengeht:
    /// mehr Objekte als die Objekttabelle fasst kann eine einzelne Operation nicht finalisieren.
    ///
    /// Sie steht hier und nicht beim Aufrufer, damit es **eine** Quelle gibt. Vorher hielt der
    /// Kernel eine eigene `MAX_FINALIZE`-Konstante mit dem Kommentar „dieselbe Schranke wie in der
    /// Cap-Crate" — eine Kopie, die beim Wachsen der Tabelle still auseinanderläuft.
    pub fn finalize_capacity(&self) -> usize {
        self.objects.len()
    }

    // --- Installation eines Wurzel-Caps ---

    /// Eine [`MemoryCap`] in den Space einbringen: legt ein Memory-Objekt
    /// (refcount 1) an und einen Wurzel-Cap (ohne Eltern) darauf. Die übergebene
    /// Cap wird konsumiert — der Besitz liegt nun beim Capability-System.
    pub fn install_memory(&mut self, cap: MemoryCap) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Memory(cap.region()), cap.rights())
    }

    /// Einen Endpoint als Objekt + Wurzel-Cap (mit gegebenen Rechten) einbringen.
    pub fn install_endpoint(&mut self, ep_id: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Endpoint(ep_id), rights)
    }

    /// Eine Thread-Capability (Tcb) für `thread_raw` (gepacktes ThreadId) einbringen.
    pub fn install_tcb(&mut self, thread_raw: u64, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Tcb(thread_raw), rights)
    }

    /// Eine Notification-Capability einbringen.
    pub fn install_notification(&mut self, ntfn_id: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Notification(ntfn_id), rights)
    }

    /// Eine **Scheduling-Context-Capability** (MCS) einbringen: die Autorität, einem
    /// Thread `budget` Ticks je `period` Ticks CPU-Zeit zuzuweisen.
    pub fn install_sched_context(&mut self, budget: u32, period: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::SchedContext { budget, period }, rights)
    }

    /// Eine **Reply-Capability** für den an Endpoint `ep` blockierten `caller` (gepacktes
    /// ThreadId-Raw) einbringen. Wird sie gelöscht/revoked, finalisiert das den Call —
    /// der Aufrufer wird mit `ERR_SERVER_GONE` entblockt (s. `delete`/`revoke`).
    pub fn install_reply(&mut self, ep: u32, caller: u64, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Reply { ep, caller }, rights)
    }

    /// Eine **Management-Capability** (ext-22) einbringen: die Autorität, den Lifecycle der
    /// Ziel-PD `pd` zu steuern (`SYS_PDCTL`). Hält keinen Allokator-Speicher.
    pub fn install_pd_control(&mut self, pd: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::PdControl { pd }, rights)
    }

    /// Eine **Loader-Capability** (ext-26) einbringen: die Autorität, über den generischen
    /// Binary-Loader ein Programm aus `source` (0 = Boot-Archiv) zu laden (`SYS_LOAD`). Hält
    /// keinen Allokator-Speicher -> keine Finalisierung.
    pub fn install_loader(&mut self, source: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Loader { source }, rights)
    }

    /// **Die Debug-Wurzel einer PD einbringen** (Z6b) — `Debuggable`.
    ///
    /// **Nur kernelseitig, und nur an EINER Stelle gerufen** (`loader::mint_debuggable_if_asked`).
    /// Es gibt bewusst keinen Syscall, der eine `Debuggable` erzeugt: gaebe es einen, waere die
    /// Vorgabe „nicht gepraegt" eine Bitte statt einer Eigenschaft, und die Zusage aus Z6b §0
    /// beschriebe keine PD mehr.
    pub fn install_debuggable(&mut self, pd: u16, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Debuggable { pd }, rights)
    }

    /// **Gibt es im ganzen Cspace IRGENDEINE Debug-Autoritaet ueber `pd`?** (Z6b)
    ///
    /// Das ist die Frage, auf der die Zusage aus §0 ruht, und sie ist ein **Durchlauf**, keine
    /// Behauptung: „ueber diese PD kann niemand Debug-Autoritaet erlangen" wird beantwortet, indem
    /// nachgesehen wird — nicht, indem ein Flag gelesen wird, das jemand gesetzt haben koennte.
    ///
    /// Gezaehlt wird die **Wurzel** mit: eine `Debuggable` ist selbst kein Zugriff, aber wer sie
    /// haelt, leitet jederzeit einen ab. Wer die Wiederbeschaffbarkeit nicht mitzaehlt, misst ein
    /// Fenster, das nicht geschlossen ist.
    /// **Lebt noch ein STEUERRECHT ueber `pd`?** (Z6b)
    ///
    /// Getrennt von [`any_debug_authority_over`](Self::any_debug_authority_over), weil es eine
    /// andere Frage ist: dort geht es um „kann jemand hier je debuggen" (Wurzel eingeschlossen,
    /// denn sie leitet ab), hier um „kann jemand einen laufenden Halt aufheben" (Wurzel
    /// ausgeschlossen, denn sie kann es nicht ohne vorher abzuleiten). Zwei Fragen, zwei
    /// Funktionen — eine gemeinsame waere ein Praedikat mit zwei Bedeutungen.
    pub fn any_debug_control_over(&self, pd: u16) -> bool {
        self.slots.iter().enumerate().any(|(i, sl)| {
            sl.used
                && sl.rights.contains(Rights::WRITE)
                && sl.mdb.parent.is_some()
                && matches!(self.objects[sl.object].kind, ObjectKind::Debuggable { pd: q } if q == pd)
                && { let _ = i; true }
        })
    }

    pub fn any_debug_authority_over(&self, pd: u16) -> bool {
        self.objects.iter().any(|o| {
            o.used
                && o.refcount > 0
                && matches!(o.kind, ObjectKind::Debuggable { pd: q } if q == pd)
        })
    }

    /// Eine **MMIO-Capability** (ext-22, HardwareLand) einbringen: die Autorität, die
    /// Geräte-Registerregion `[phys, phys+len)` als EL0-Device zu mappen. Hält keinen
    /// RAM-Allokator-Eintrag (Geräte-Bereich) -> keine Finalisierung. Nur kernelseitig.
    pub fn install_mmio(&mut self, phys: u64, len: u64, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Mmio { phys, len }, rights)
    }

    /// Eine **Syscall-Handler-Capability** (Z26/A3) einbringen: die Autorität, die Syscalls der
    /// an sie gebundenen Threads zu **beantworten**.
    ///
    /// `sidecar`/`len` beschreiben das geteilte Fenster, in dem die Trap-Frames der Gäste liegen.
    /// Die Cap hält **keinen** Allokator-Eintrag: das Fenster wird über eine getrennte
    /// `Memory`-Cap vergeben, und das ist der Punkt der Sidecar-Form — die Autorität über den
    /// Registerzustand fremder Threads ist eine **Region in der Speicherbuchhaltung** und nicht
    /// eine Fähigkeit, die nur im Cap-Audit auftaucht.
    ///
    /// Nur kernelseitig: kein User-Syscall erzeugt beliebige Handler-Caps.
    pub fn install_syscall_handler(
        &mut self,
        ep: u32,
        pd: u16,
        sidecar: u64,
        len: u64,
        rights: Rights,
    ) -> Result<CapPtr, CapError> {
        self.install(
            ObjectKind::SyscallHandler {
                ep,
                pd,
                sidecar,
                len,
            },
            rights,
        )
    }

    /// Eine **Fault-Handler-Capability** (Z26/A3) einbringen: die Autorität, die Seitenfehler der
    /// an sie gebundenen Threads zu **sehen**.
    ///
    /// Getrennt von [`Self::install_syscall_handler`], weil es eine andere Autorität ist — s.
    /// [`ObjectKind::FaultHandler`].
    pub fn install_fault_handler(
        &mut self,
        ep: u32,
        pd: u16,
        sidecar: u64,
        len: u64,
        rights: Rights,
    ) -> Result<CapPtr, CapError> {
        self.install(
            ObjectKind::FaultHandler {
                ep,
                pd,
                sidecar,
                len,
            },
            rights,
        )
    }

    /// Eine **IRQ-Capability** (ext-22, HardwareLand) einbringen: die Autorität, den Geräte-
    /// Interrupt `intid` zu empfangen. Hält keinen RAM-Eintrag. Nur kernelseitig.
    pub fn install_irq(&mut self, intid: u32, rights: Rights) -> Result<CapPtr, CapError> {
        self.install(ObjectKind::Irq { intid }, rights)
    }

    /// Eine **DMA-Capability** (ext-23, HardwareLand) einbringen: die Autorität über die
    /// kernel-ausgeschnittene RAM-DMA-Region `[phys, phys+len)`. Anders als Mmio/Irq ist dies
    /// echtes RAM -> die Finalisierung gibt es frei (`free_region`), garantiert sicher durch die
    /// Teardown-Reihenfolge (`enforcer.disable_dma` -> Unmap davor). Nur kernelseitig.
    pub fn install_dma(&mut self, phys: u64, len: u64, rights: Rights) -> Result<CapPtr, CapError> {
        // Rückwärtskompatibel (ext-23): Bidirectional + NonCoherent.
        self.install_dma_ex(phys, len, DmaDir::Bidirectional, DmaCoherence::NonCoherent, rights)
    }

    /// Wie [`install_dma`], aber mit expliziter **Richtung** + **Cache-Kohärenz** (ext-24). Die
    /// Cap kodiert damit die volle Autorität inkl. richtungsminimaler Hardware-Rechte.
    pub fn install_dma_ex(
        &mut self,
        phys: u64,
        len: u64,
        dir: DmaDir,
        coherence: DmaCoherence,
        rights: Rights,
    ) -> Result<CapPtr, CapError> {
        self.install(
            ObjectKind::Dma {
                phys,
                len,
                dir,
                coherence,
            },
            rights,
        )
    }

    /// Wurzel-Objekt + -Cap anlegen (gemeinsame Logik für alle Objekttypen).
    fn install(&mut self, kind: ObjectKind, rights: Rights) -> Result<CapPtr, CapError> {
        let obj = self.alloc_object(kind)?;
        let slot = match self.alloc_slot(obj, rights, 0) {
            Ok(s) => s,
            Err(e) => {
                self.objects[obj].used = false; // Objekt zurückrollen
                return Err(e);
            }
        };
        Ok(self.ptr(slot))
    }

    /// Objektart, Rechte und Badge eines Caps auflösen (für cap-gesicherte
    /// Invokation; der Badge identifiziert z. B. die Signalquelle bei Notifications).
    ///
    /// Eine Memory-Cap mit Teilfenster meldet die **verengte** Region, nicht die des Objekts:
    /// wer das Teil per `SYS_MAP` abbildet, bekommt das Teil — sonst wäre das Fenster eine
    /// Auskunft, keine Autorität.
    pub fn lookup(&self, ptr: CapPtr) -> Option<(ObjectKind, Rights, u64)> {
        let slot = self.resolve(ptr).ok()?;
        let obj = self.slots[slot].object;
        let kind = match self.objects[obj].kind {
            ObjectKind::Memory(r) => ObjectKind::Memory(self.slots[slot].subregion.unwrap_or(r)),
            k => k,
        };
        Some((kind, self.slots[slot].rights, self.slots[slot].badge))
    }

    /// Über alle **DMA-Objekte** (objektgranular, distinct) iterieren: ruft `f(phys, len)`
    /// je belegtem `ObjectKind::Dma`. Für das DMA-Policy-Oracle (Bounds/Disjunktheit) —
    /// objekt- statt cap-granular, damit Cap-Kopien dieselbe Region nicht mehrfach zählen
    /// (sonst falsch-positive Selbstüberlappung).
    pub fn for_each_dma(&self, f: &mut dyn FnMut(u64, u64)) {
        for o in self.objects.iter() {
            if o.used {
                if let ObjectKind::Dma { phys, len, .. } = o.kind {
                    f(phys, len);
                }
            }
        }
    }

    // --- Ableitungsoperationen ---

    /// Capability ableiten: neuer Kind-Cap auf dasselbe Objekt, Rechte
    /// eingeschränkt auf `src.rights ∩ rights` (keine Eskalation). Badge wird
    /// übernommen.
    ///
    /// Ein bestehendes Teilfenster ([`subregion`](Self::subregion)) wird **übernommen**, nicht
    /// geweitet: eine Kopie darf nie mehr Region autorisieren als die Vorlage — dieselbe
    /// Richtung wie bei den Rechten.
    pub fn copy(&mut self, src: CapPtr, rights: Rights) -> Result<CapPtr, CapError> {
        let s = self.resolve(src)?;
        let obj = self.slots[s].object;
        let new_rights = self.slots[s].rights.intersect(rights);
        let badge = self.slots[s].badge;
        let fenster = self.slots[s].subregion;
        let perms = self.slots[s].perms;
        let dst = self.alloc_slot(obj, new_rights, badge)?;
        self.slots[dst].subregion = fenster;
        self.slots[dst].perms = perms;
        self.objects[obj].refcount += 1;
        self.link_child(s, dst);
        Ok(self.ptr(dst))
    }

    /// Wie [`copy`](Self::copy), zusätzlich mit gesetztem Badge.
    pub fn mint(&mut self, src: CapPtr, rights: Rights, badge: u64) -> Result<CapPtr, CapError> {
        // A tagged capability keeps its permission mask in the high bits of the delivered badge
        // word, so its id must fit the low 48 bits. Checked before anything is allocated.
        if self.slot_perms(src)?.is_some() && !ipc_perm::id_fits(badge) {
            return Err(CapError::Invalid);
        }
        let dst = self.copy(src, rights)?;
        self.slots[dst.slot].badge = badge;
        Ok(dst)
    }

    /// The permission mask of the slot behind `ptr`: `Ok(None)` for an untagged capability.
    pub fn slot_perms(&self, ptr: CapPtr) -> Result<Option<u16>, CapError> {
        let s = self.resolve(ptr)?;
        Ok(self.slots[s].perms)
    }

    /// May the holder of `ptr` derive further capabilities from it (`CCOPY`)?
    ///
    /// Untagged capabilities always may (unchanged behaviour); a tagged one needs
    /// [`ipc_perm::DUP`]. Invalid handles answer `false`.
    pub fn may_dup(&self, ptr: CapPtr) -> bool {
        match self.slot_perms(ptr) {
            Ok(None) => true,
            Ok(Some(p)) => p & ipc_perm::DUP != 0,
            Err(_) => false,
        }
    }

    /// May the holder of `ptr` pass it on in a `REPLY` grant? Untagged: yes; tagged: needs
    /// [`ipc_perm::GRANT`].
    pub fn may_grant(&self, ptr: CapPtr) -> bool {
        match self.slot_perms(ptr) {
            Ok(None) => true,
            Ok(Some(p)) => p & ipc_perm::GRANT != 0,
            Err(_) => false,
        }
    }

    /// **Derive a tagged IPC capability from an endpoint capability.**
    ///
    /// The result points at the same endpoint object, carries `rights` (intersected with the
    /// source, like [`copy`](Self::copy)), the id `id` in its badge (`0` = inherit), and the
    /// permission mask `effective(src) & mask`, where an untagged source counts as
    /// [`ipc_perm::ALL`]. Permissions therefore only ever shrink along a derivation.
    ///
    /// * The source must be an endpoint capability -- other kinds have no server to attest to
    ///   ([`CapError::Invalid`]).
    /// * A tagged source needs [`ipc_perm::DUP`] ([`CapError::Invalid`] otherwise; the dispatch
    ///   maps it to `ERR_RIGHTS`, see [`may_dup`](Self::may_dup)).
    /// * `id` must fit 48 bits ([`CapError::Invalid`]).
    /// * `mask == 0` is rejected: an all-clear tag is a capability that can do nothing, and `0`
    ///   is the ABI's "not a tagged derivation" value.
    pub fn derive_ipc(
        &mut self,
        src: CapPtr,
        rights: Rights,
        id: u64,
        mask: u16,
    ) -> Result<CapPtr, CapError> {
        if mask == 0 || !ipc_perm::id_fits(id) {
            return Err(CapError::Invalid);
        }
        let s = self.resolve(src)?;
        if !matches!(self.objects[self.slots[s].object].kind, ObjectKind::Endpoint(_)) {
            return Err(CapError::Invalid);
        }
        let have = self.slots[s].perms.unwrap_or(ipc_perm::ALL);
        if self.slots[s].perms.is_some() && have & ipc_perm::DUP == 0 {
            return Err(CapError::Invalid);
        }
        let dst = self.copy(src, rights)?;
        if id != 0 {
            self.slots[dst.slot].badge = id;
        } else {
            self.slots[dst.slot].badge &= ipc_perm::ID_MASK;
        }
        self.slots[dst.slot].perms = Some(have & mask);
        Ok(dst)
    }

    /// The badge word a server receives for a `CALL` through `ptr`: for a tagged capability the
    /// id in the low 48 bits and the permission mask in the high 16
    /// ([`ipc_perm::wire`]), otherwise the plain badge.
    pub fn wire_badge(&self, ptr: CapPtr) -> Option<u64> {
        let s = self.resolve(ptr).ok()?;
        let slot = &self.slots[s];
        Some(match slot.perms {
            Some(p) => ipc_perm::wire(p, slot.badge),
            None => slot.badge,
        })
    }

    /// Einen **Teilbereich** einer Memory-Cap als eigene Cap ableiten (CSUB,
    /// Mem-Server-Transport): `[Basis + Offset, Basis + Offset + Länge)` aus der **wirksamen**
    /// Region der Quell-Cap (Objekt-Region oder deren Teilfenster — Ableitungen von
    /// Ableitungen verengen weiter, sie weiten nie).
    ///
    /// * Die Quell-Cap **muss** eine Memory-Cap sein — sonst [`CapError::Invalid`]: ohne Region
    ///   gibt es kein Fenster. Der Dispatch bildet das auf `ERR_BADCAP` ab.
    /// * Das Fenster muss in der Region liegen — sonst [`CapError::Subregion`] (Dispatch:
    ///   `ERR_SUBREGION`, Nr. 21). Die Rechnung steht in [`teilfenster_rechnen`].
    /// * Die Rechte sind der **Schnitt** mit denen der Quelle (keine Eskalation, wie
    ///   [`copy`](Self::copy)); das Badge wird übernommen.
    /// * Das Kind zeigt auf **dasselbe Objekt** und hängt als Kind im CDT: `revoke` an der
    ///   Quelle zieht das Teil ein, und die Region wird genau einmal freigegeben — wenn die
    ///   letzte Cap (Quelle oder Teil) fällt. Ein eigenes Objekt je Teilfenster gäbe die
    ///   Teilregion mitten aus der lebenden Eltern-Region an den Allokator zurück.
    pub fn subregion(
        &mut self,
        src: CapPtr,
        offset: u64,
        len: u64,
        rights: Rights,
    ) -> Result<CapPtr, CapError> {
        let s = self.resolve(src)?;
        let obj = self.slots[s].object;
        let whole = match self.objects[obj].kind {
            ObjectKind::Memory(r) => r,
            _ => return Err(CapError::Invalid),
        };
        let wirksam = self.slots[s].subregion.unwrap_or(whole);
        let fenster = teilfenster_rechnen(wirksam, offset, len)?;
        let new_rights = self.slots[s].rights.intersect(rights);
        let badge = self.slots[s].badge;
        let dst = self.alloc_slot(obj, new_rights, badge)?;
        self.slots[dst].subregion = Some(fenster);
        self.objects[obj].refcount += 1;
        self.link_child(s, dst);
        Ok(self.ptr(dst))
    }

    /// Cap in einen frischen Slot verschieben (Identität/Position im CDT bleibt).
    /// Das alte Handle wird ungültig; das neue wird zurückgegeben.
    pub fn move_cap(&mut self, src: CapPtr) -> Result<CapPtr, CapError> {
        let s = self.resolve(src)?;
        let dst = self.free_slot_index()?;

        // Inhalt übernehmen (Generation des Zielslots beibehalten).
        let gen = self.slots[dst].gen;
        self.slots[dst] = self.slots[s];
        self.slots[dst].gen = gen;

        // Alle Verweise auf `s` auf `dst` umbiegen.
        let mdb = self.slots[s].mdb;
        match mdb.prev_sibling {
            Some(p) => self.slots[p].mdb.next_sibling = Some(dst),
            None => {
                if let Some(par) = mdb.parent {
                    self.slots[par].mdb.first_child = Some(dst);
                }
            }
        }
        if let Some(n) = mdb.next_sibling {
            self.slots[n].mdb.prev_sibling = Some(dst);
        }
        // Ebenfalls begrenzt: die Kinderliste ist mandantengestaltet (jedes `copy` hängt vorne
        // ein), und eine zyklische Geschwisterkette liefe hier endlos.
        let limit = self.cdt_step_limit();
        let mut child = mdb.first_child;
        let mut steps = 0usize;
        while let Some(c) = child {
            steps += 1;
            if steps > limit {
                self.note_overrun();
                break;
            }
            self.slots[c].mdb.parent = Some(dst);
            child = self.slots[c].mdb.next_sibling;
        }
        self.note_walk(steps);

        // Quellslot freigeben (Generation erhöhen -> altes Handle ungültig).
        self.release_slot(s);
        Ok(self.ptr(dst))
    }

    /// Einen Cap löschen. Der Cap darf keine Kinder haben (sonst `HasChildren`;
    /// vorher `revoke`). Wird damit die letzte Referenz auf das Objekt entfernt,
    /// wird das Objekt finalisiert (Speicher an `alloc` zurückgegeben).
    pub fn delete(
        &mut self,
        alloc: &mut PhysAllocator,
        ptr: CapPtr,
        rf: &mut Finalized<'_>,
    ) -> Result<(), CapError> {
        let slot = self.resolve(ptr)?;
        if self.slots[slot].mdb.first_child.is_some() {
            return Err(CapError::HasChildren);
        }
        self.delete_leaf(alloc, slot, rf);
        Ok(())
    }

    /// Alle Abkömmlinge von `ptr` rekursiv löschen (der Cap selbst bleibt).
    /// Anschließend ist `ptr` kinderlos.
    pub fn revoke(
        &mut self,
        alloc: &mut PhysAllocator,
        ptr: CapPtr,
        rf: &mut Finalized<'_>,
    ) -> Result<(), CapError> {
        let slot = self.resolve(ptr)?;
        let limit = self.cdt_step_limit();
        // Wiederholt zu einem Blatt unterhalb von `slot` absteigen und löschen.
        //
        // **Beide** Schleifen sind begrenzt (B-5.5), und zwar aus verschiedenen Gründen: der
        // Abstieg endet bei einer zyklischen `first_child`-Kette nie, und die äussere Schleife
        // endet nie, wenn `delete_leaf` den Baum nicht kleiner macht. Ohne die Grenzen hinge der
        // Kern hier unter der CAPS-Sperre — bei einer Struktur, die Mandanten mitgestalten.
        let mut ops = 0usize;
        while let Some(child) = self.slots[slot].mdb.first_child {
            let (leaf, steps) = match descend_to_leaf(self.slots.as_slice(), child, limit) {
                Ok(v) => v,
                Err(()) => {
                    // Der Teilbaum ist nicht baumförmig. Abbrechen ist die einzig richtige
                    // Antwort — aber **gezählt**: dieses `revoke` ist unvollständig, es leben
                    // noch Abkömmlinge, die weg sein sollten.
                    self.note_overrun();
                    break;
                }
            };
            self.note_walk(steps);
            self.delete_leaf(alloc, leaf, rf);
            ops += 1;
            if ops > limit {
                // Mehr Löschungen als Slots: jede Löschung gibt genau einen Slot frei, das kann
                // nicht sein. Auch das ist ein Befund, kein Normalfall.
                self.note_overrun();
                break;
            }
        }
        if ops > self.peak_revoke_ops {
            self.peak_revoke_ops = ops;
        }
        Ok(())
    }

    // --- Inspektion ---

    /// Anzahl belegter Cap-Slots — für Leak-/Konsistenzprüfungen (Fuzzer-Oracle).
    pub fn used_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.used).count()
    }

    /// Anzahl belegter Objekt-Einträge — für Leak-/Konsistenzprüfungen. Nach dem
    /// Löschen aller abgeleiteten Caps muss dieser Wert zur Baseline zurückkehren
    /// (sonst CDT-/Refcount-Leck: ein Objekt ohne lebende Cap).
    pub fn used_objects(&self) -> usize {
        self.objects.iter().filter(|o| o.used).count()
    }

    /// Höchststand gleichzeitig belegter Slots seit dem Start (A-3.4) und die Kapazität dazu.
    ///
    /// **Wozu:** `CAP_BUDGET_PER_PD` deckelt den Verbrauch **einer** PD, prüft aber nirgends die
    /// **Summe**. Bei `NPDS` PDs mit vollem Budget wäre der Bedarf ein Vielfaches von der Slot-Kapazität —
    /// die Fairness-Zusage des Budgets ist damit eine Annahme über das Verhalten der PDs, keine
    /// Eigenschaft des Systems. Der Höchststand macht den Abstand zur Grenze **messbar**, statt
    /// ihn zu behaupten.
    pub fn peak_slots(&self) -> (usize, usize) {
        (self.peak_slots, self.slots.len())
    }

    /// Höchststand belegter Objekt-Einträge und die Kapazität dazu.
    pub fn peak_objects(&self) -> (usize, usize) {
        (self.peak_objects, self.objects.len())
    }

    /// **Längster CDT-Lauf in Schritten** und die strukturelle Schranke dazu (B-5.5).
    ///
    /// Eine Operationszahl, keine Zeit — damit sie zwischen Blech, KVM und TCG dieselbe Aussage
    /// trifft. Der Abstand zur Schranke ist der Spielraum, den die Zusage „begrenzte kritische
    /// Sektion" tatsächlich hat; ohne ihn ist sie eine Hoffnung.
    pub fn peak_cdt_walk(&self) -> (usize, usize) {
        (self.peak_cdt_walk, self.cdt_step_limit())
    }

    /// Grösster von einem einzelnen `revoke` gelöschter Teilbaum (Anzahl Löschoperationen) und
    /// die Schranke dazu. Die zweite Hälfte von [`peak_cdt_walk`](Self::peak_cdt_walk): ein
    /// einzelner Lauf kann kurz sein, das `revoke` darüber trotzdem lang.
    pub fn peak_revoke_ops(&self) -> (usize, usize) {
        (self.peak_revoke_ops, self.cdt_step_limit())
    }

    /// Wie oft ein CDT-Lauf die Schranke erreicht hat. **Muss 0 sein.**
    ///
    /// Ist er es nicht, war der CDT nicht baumförmig (dasselbe, was
    /// [`audit_cdt`](Self::audit_cdt) mit Code 7 meldet) — und die betroffene Operation ist
    /// **unvollständig** abgebrochen. Bei `revoke` heisst unvollständig: es leben noch
    /// Abkömmlinge, die weg sein sollten. Genau deshalb steht das hier als Zähler und nicht als
    /// stilles Abschneiden; der Aufrufer **muss** ihn auswerten, wie er
    /// [`Finalized::overflowed`] auswertet.
    pub fn cdt_walk_overruns(&self) -> u32 {
        self.cdt_walk_overruns
    }

    /// Den von `cap` bezeichneten Slot in `seen` **markieren** — Baustein der Summenprüfung
    /// (A-3.4, Abschluss).
    ///
    /// Warum das hier steht und nicht beim Aufrufer: der Slot-Index in [`CapPtr`] ist
    /// `pub(crate)` und soll es bleiben. Ein öffentlicher Index wäre eine zweite, ungeprüfte
    /// Adressierung des Cap-Systems neben dem Handle; das Markieren braucht ihn, sonst niemand.
    ///
    /// Gibt `false` zurück, wenn `cap` **nicht auflösbar** ist (Slot außerhalb der Tabelle, frei,
    /// oder Generation abgelaufen) oder `seen` zu kurz ist. Ein abgelaufenes Handle in einem
    /// PD-Cspace ist kein Verbrauch, darf also auch nichts markieren.
    pub fn mark_slot(&self, cap: CapPtr, seen: &mut [bool]) -> bool {
        let Some(slot) = self.slots.get(cap.slot) else {
            return false;
        };
        if !slot.used || slot.gen != cap.gen || cap.slot >= seen.len() {
            return false;
        }
        seen[cap.slot] = true;
        true
    }

    /// Belegte Slots, die in `seen` **nicht** markiert sind — die Gegenrechnung zu
    /// [`mark_slot`](Self::mark_slot).
    ///
    /// Markiert der Aufrufer vorher jede Cap jeder PD, ist das Ergebnis der Verbrauch, der auf
    /// **kein** PD-Budget geht: die Wurzel-Caps des Kernels. Genau diese Größe stand bisher gegen
    /// eine Reserve, die niemand nachgezählt hat.
    ///
    /// `None` heißt „konnte nicht laufen" (`seen` kürzer als die Slot-Tabelle) — dieselbe
    /// Unterscheidung wie Code 8 in [`audit_cdt`](Self::audit_cdt) und aus demselben Grund: eine
    /// Prüfung, die nicht laufen kann, sieht sonst aus wie eine bestandene.
    pub fn unmarked_used_slots(&self, seen: &[bool]) -> Option<usize> {
        if seen.len() < self.slots.len() {
            return None;
        }
        Some(
            self.slots
                .iter()
                .enumerate()
                .filter(|(i, s)| s.used && !seen[*i])
                .count(),
        )
    }

    /// **Property-Oracle des Capability-Systems** (read-only). Prüft die strukturellen
    /// Invarianten des CDT + der Refcounts und gibt `0` bei Konsistenz zurück, sonst
    /// einen Anomalie-Code:
    /// 1 = ein belegter Slot verweist auf ein **nicht belegtes** Objekt (toter CDT-Knoten),
    /// 2 = `refcount` eines Objekts stimmt **nicht** mit der Anzahl auf es zeigender Slots
    ///     überein (negativer/zu hoher Refcount),
    /// 3 = belegtes Objekt ohne lebende Cap **oder** unbelegtes Objekt mit lebenden Caps
    ///     (verlorenes/inkonsistentes Objekt),
    /// 4 = Eltern-Verkettung kaputt (Eltern-Slot unbelegt / anderes Objekt / Kind nicht in
    ///     der Kinderliste — Rechteeskalation/Ableitung verletzt),
    /// 5 = Geschwister-Verkettung nicht reziprok,
    /// 6 = `first_child`-Verkettung kaputt (Kind unbelegt / falsches `parent`),
    /// 7 = Zyklus bzw. überlange Kette (CDT nicht baumförmig),
    /// 8 = `refs` kürzer als die Objekttabelle — das Audit **konnte nicht laufen** (A-3.4),
    /// 9 = IPC-Berechtigungen **vergrößert**: ein Kind trägt Bits, die sein Elter nicht trägt
    ///     (ein ungetaggter Elter zählt als alle Bits). Die Berechtigungsmaske darf entlang des
    ///     CDT nur schrumpfen.
    ///
    /// `refs` ist die Zählfläche für die Refcount-Prüfung und muss mindestens
    /// [`finalize_capacity`](Self::finalize_capacity) Einträge fassen; ihr Inhalt beim Eintritt
    /// ist gleichgültig (wird genullt).
    /// Damit abgesichert: keine verlorenen Objekte, keine negativen Refcounts, keine toten
    /// CDT-Knoten, keine Ableitung auf ein fremdes Objekt (Rechteeskalation), Baumform.
    pub fn audit_cdt(&self, refs: &mut [u32]) -> u32 {
        let nslots = self.slots.len();
        let nobjects = self.objects.len();
        // **Dieselbe** Schranke, die auch die Operationen begrenzen (B-5.5). Sie stand hier schon
        // immer -- als zwei eingestreute `steps > nslots`. Dass ausgerechnet der *Prüfer* gegen
        // einen zyklischen CDT geschützt war und `revoke` nicht, war die eigentliche Schieflage:
        // der Prüfer läuft auf Anforderung, `revoke` auf Mandantenwunsch.
        let limit = self.cdt_step_limit();
        // Der Zählpuffer kommt seit A-3.4 vom Aufrufer (dieselbe Entscheidung wie bei
        // `Finalized`): ein `[0u32; NOBJECTS]` war ein Stack-Array mit Compile-Zeit-Größe und
        // hätte die Objekttabelle wieder an die Stackgröße gebunden. Zu klein ist ein eigener
        // Befund und **kein** stilles „konsistent": ein Audit, das nicht laufen kann, sieht sonst
        // aus wie ein bestandenes.
        if refs.len() < nobjects {
            return 8;
        }
        let refs = &mut refs[..nobjects];
        refs.fill(0);
        // (1)+(2)+(3): Refcount == Anzahl belegter Slots, die auf das Objekt zeigen.
        for s in 0..nslots {
            if !self.slots[s].used {
                continue;
            }
            let obj = self.slots[s].object;
            if obj >= nobjects || !self.objects[obj].used {
                return 1;
            }
            refs[obj] += 1;
        }
        for o in 0..nobjects {
            if self.objects[o].used {
                if self.objects[o].refcount != refs[o] {
                    return 2;
                }
                if refs[o] == 0 {
                    return 3;
                }
            } else if refs[o] != 0 {
                return 3;
            }
        }
        // (4)+(5)+(6): CDT-Verkettung konsistent; Ableitung teilt das Objekt.
        for s in 0..nslots {
            if !self.slots[s].used {
                continue;
            }
            let m = self.slots[s].mdb;
            if let Some(p) = m.parent {
                if p >= nslots || !self.slots[p].used || self.slots[p].object != self.slots[s].object
                {
                    return 4;
                }
                // s muss in der Kinderliste von p vorkommen.
                let mut c = self.slots[p].mdb.first_child;
                let mut found = false;
                let mut steps = 0;
                while let Some(ci) = c {
                    if ci == s {
                        found = true;
                        break;
                    }
                    c = self.slots[ci].mdb.next_sibling;
                    steps += 1;
                    if steps > limit {
                        return 7;
                    }
                }
                if !found {
                    return 4;
                }
            }
            // (9) IPC permissions never grow along a derivation. An untagged parent counts as ALL.
            if let (Some(p), Some(child_perms)) = (m.parent, self.slots[s].perms) {
                let parent_perms = self.slots[p].perms.unwrap_or(ipc_perm::ALL);
                if child_perms & !parent_perms != 0 {
                    return 9;
                }
            }
            if let Some(c) = m.first_child {
                // first_child muss belegt sein, `parent == s` haben UND der **Listenkopf** sein
                // (`prev_sibling == None`). Ohne die prev-Prüfung passierte ein reziproker
                // Geschwister-Zyklus (a<->b als first_child) das Audit (Verus-Invariante Klausel 6
                // verlangt beides; das Oracle war hier schwächer als die formale Invariante).
                if c >= nslots
                    || !self.slots[c].used
                    || self.slots[c].mdb.parent != Some(s)
                    || self.slots[c].mdb.prev_sibling.is_some()
                {
                    return 6;
                }
            }
            if let Some(n) = m.next_sibling {
                // Geschwister müssen reziprok verkettet sein UND denselben Parent teilen (Verus-
                // Klausel 4-sib). Ohne die parent-Prüfung könnten zwei Slots als Geschwister verkettet
                // sein, aber in verschiedenen Kinderlisten hängen (Oracle schwächer als die Invariante).
                if n >= nslots
                    || !self.slots[n].used
                    || self.slots[n].mdb.prev_sibling != Some(s)
                    || self.slots[n].mdb.parent != m.parent
                {
                    return 5;
                }
            }
            if let Some(pv) = m.prev_sibling {
                if pv >= nslots || !self.slots[pv].used || self.slots[pv].mdb.next_sibling != Some(s)
                {
                    return 5;
                }
            }
        }
        // (7): keine Zyklen in der Eltern-Kette.
        for s in 0..nslots {
            if !self.slots[s].used {
                continue;
            }
            let mut p = self.slots[s].mdb.parent;
            let mut steps = 0;
            while let Some(pi) = p {
                steps += 1;
                if steps > limit {
                    return 7;
                }
                p = self.slots[pi].mdb.parent;
            }
        }
        0
    }

    /// Sicht auf einen Cap (oder `None` bei ungültigem Handle).
    ///
    /// **Seit B-5.5 auch `None`, wenn die Kinderliste die Schrittgrenze reisst** — dann ist der
    /// CDT nicht baumförmig und jede Kinderzahl wäre erfunden. Der Fall wird hier **nicht**
    /// mitgezählt: `inspect` nimmt `&self` und kann den Zähler nicht führen. Er ist trotzdem
    /// nicht unsichtbar — dieselbe Struktur meldet [`audit_cdt`](Self::audit_cdt) mit Code 7,
    /// und jede *verändernde* Operation über denselben Pfad erhöht
    /// [`cdt_walk_overruns`](Self::cdt_walk_overruns).
    /// **Nur die Objektart** — ohne CDT-Lauf.
    ///
    /// [`inspect`](Self::inspect) rechnet `child_count`, und das ist ein Gang durch die
    /// Kinderliste mit einer Schranke von `slots.len()`. Wer nur die Art wissen will, bezahlt das
    /// sonst je Slot — unter der gehaltenen `CAPS`-Sperre, und **`SpinLock` maskiert IRQs**.
    /// Gemessen am 2026-08-21: `pd_haelt_dma` (Z23/S5) lief so ueber alle Cap-Slots mehrerer PDs
    /// und hat die laengste maskierte Strecke auf aarch64 so weit hochgezogen, dass die
    /// Stopp-Latenz-Zusage der Debugger-Sonde fiel — an einer Zeile, die mit dem Debugger nichts
    /// zu tun hat. Dieselbe Klasse wie der `while let`-Guard im C8-Verifizierer: kein Haenger,
    /// sondern ein Latenzloch, das keine Pruefzeile ansieht — nur diesmal HAT eine hingesehen.
    pub fn kind_of(&self, ptr: CapPtr) -> Option<ObjectKind> {
        let slot = self.resolve(ptr).ok()?;
        Some(match self.objects[self.slots[slot].object].kind {
            ObjectKind::Memory(r) => ObjectKind::Memory(self.slots[slot].subregion.unwrap_or(r)),
            k => k,
        })
    }

    pub fn inspect(&self, ptr: CapPtr) -> Option<CapInfo> {
        let slot = self.resolve(ptr).ok()?;
        let obj = self.slots[slot].object;
        // Reisst die Kinderliste die Schranke, ist die Zahl bedeutungslos -> `None` statt einer
        // erfundenen 0. Eine Sicht, die bei kaputter Verkettung „keine Kinder" meldet, wäre
        // genau die stille Falschaussage, gegen die die Schranke gebaut ist.
        let child_count = self.child_count(slot).ok()?;
        let kind = match self.objects[obj].kind {
            ObjectKind::Memory(r) => ObjectKind::Memory(self.slots[slot].subregion.unwrap_or(r)),
            k => k,
        };
        Some(CapInfo {
            kind,
            rights: self.slots[slot].rights,
            badge: self.slots[slot].badge,
            refcount: self.objects[obj].refcount,
            child_count,
            is_root: self.slots[slot].mdb.parent.is_none(),
        })
    }

    // --- intern ---

    /// Die strukturelle Schranke jedes CDT-Laufs: so viele Slots gibt es, mehr kann ein
    /// azyklischer Lauf nicht besuchen. **Eine** Quelle für alle Läufe und für `audit_cdt`.
    fn cdt_step_limit(&self) -> usize {
        self.slots.len()
    }

    fn note_walk(&mut self, steps: usize) {
        if steps > self.peak_cdt_walk {
            self.peak_cdt_walk = steps;
        }
    }

    fn note_overrun(&mut self) {
        self.cdt_walk_overruns = self.cdt_walk_overruns.saturating_add(1);
    }

    fn ptr(&self, slot: usize) -> CapPtr {
        CapPtr {
            slot,
            gen: self.slots[slot].gen,
        }
    }

    fn resolve(&self, ptr: CapPtr) -> Result<usize, CapError> {
        if ptr.slot < self.slots.len()
            && self.slots[ptr.slot].used
            && self.slots[ptr.slot].gen == ptr.gen
        {
            Ok(ptr.slot)
        } else {
            Err(CapError::Invalid)
        }
    }

    fn free_slot_index(&self) -> Result<usize, CapError> {
        self.slots
            .iter()
            .position(|s| !s.used)
            .ok_or(CapError::NoSlot)
    }

    fn alloc_slot(&mut self, object: usize, rights: Rights, badge: u64) -> Result<usize, CapError> {
        let i = self.free_slot_index()?;
        let gen = self.slots[i].gen;
        self.slots[i] = CapSlot {
            used: true,
            gen,
            object,
            rights,
            badge,
            subregion: None,
            perms: None,
            mdb: Mdb::EMPTY,
        };
        // Höchststand hier, im einzigen Belegungspfad: eine Stichprobe von aussen wuerde genau
        // die Spitzen verfehlen, um die es geht (A-3.4).
        let now = self.used_slots();
        if now > self.peak_slots {
            self.peak_slots = now;
        }
        Ok(i)
    }

    fn release_slot(&mut self, slot: usize) {
        let gen = self.slots[slot].gen.wrapping_add(1);
        self.slots[slot] = CapSlot::EMPTY;
        self.slots[slot].gen = gen;
    }

    fn alloc_object(&mut self, kind: ObjectKind) -> Result<usize, CapError> {
        let r = self.alloc_object_inner(kind);
        if r.is_ok() {
            let now = self.used_objects();
            if now > self.peak_objects {
                self.peak_objects = now;
            }
        }
        r
    }

    fn alloc_object_inner(&mut self, kind: ObjectKind) -> Result<usize, CapError> {
        let i = self
            .objects
            .iter()
            .position(|o| !o.used)
            .ok_or(CapError::NoObject)?;
        let gen = self.objects[i].gen;
        self.objects[i] = Object {
            used: true,
            kind,
            refcount: 1,
            gen,
        };
        Ok(i)
    }

    /// Länge der Kinderliste. `0` auch dann, wenn die Verkettung die Schranke reisst — der
    /// Zähler [`cdt_walk_overruns`](Self::cdt_walk_overruns) sagt, dass das passiert ist.
    /// `&self`, deshalb hier ohne Buchung; der Befund wird in [`inspect`](Self::inspect) gebucht.
    fn child_count(&self, slot: usize) -> Result<usize, ()> {
        count_children(self.slots.as_slice(), slot, self.cdt_step_limit())
    }

    /// `child` vorne in die Kinderliste von `parent` einhängen. Die Verkettung steht genau
    /// einmal in [`kind_einhaengen`] und wird dort auch geprüft (als Slice, ohne Kernel).
    fn link_child(&mut self, parent: usize, child: usize) {
        kind_einhaengen(self.slots.as_mut_slice(), parent, child);
    }

    /// `slot` aus der Geschwister-/Kinderverkettung lösen.
    fn unlink(&mut self, slot: usize) {
        let mdb = self.slots[slot].mdb;
        match mdb.prev_sibling {
            Some(p) => self.slots[p].mdb.next_sibling = mdb.next_sibling,
            None => {
                if let Some(par) = mdb.parent {
                    self.slots[par].mdb.first_child = mdb.next_sibling;
                }
            }
        }
        if let Some(n) = mdb.next_sibling {
            self.slots[n].mdb.prev_sibling = mdb.prev_sibling;
        }
        self.slots[slot].mdb = Mdb::EMPTY;
    }

    /// Ein Blatt (Cap ohne Kinder) löschen: aushängen, Refcount senken,
    /// ggf. Objekt finalisieren (Speicher zurückgeben), Slot freigeben.
    fn delete_leaf(&mut self, alloc: &mut PhysAllocator, slot: usize, rf: &mut Finalized<'_>) {
        let obj = self.slots[slot].object;
        // **Z6b: JEDES geloeschte Steuerrecht wird gemeldet, nicht erst das letzte.**
        //
        // Die naheliegende Fassung haengt die Meldung an die Objekt-Finalisierung unten
        // (`refcount == 0`). Gemessen am 2026-08-20 traegt das nicht: `revoke` loescht den
        // **Teilbaum**, nicht die Wurzel — das Objekt behaelt also mindestens eine Referenz, die
        // Finalisierung laeuft nie, und die `dbg`-Zeile meldete `revoke-bricht-nicht=false`.
        //
        // Gemeldet wird deshalb hier, je Cap; **ob** freigegeben wird, entscheidet der Kernel nach
        // dem Freigeben von `CAPS` (`release_finalized_debug`), indem er nachsieht, ob noch ein
        // Steuerrecht lebt. Sammler und Urteil sind getrennt, weil das Urteil einen Blick auf den
        // ganzen Cspace braucht und der hier gerade halb abgebaut ist.
        //
        // **Die Wurzel zaehlt nicht als Steuerrecht** (`is_root`): sie darf ableiten und sonst
        // nichts. Zaehlte sie mit, bliebe nach jedem Revoke ein „lebendes" Steuerrecht stehen, und
        // die Freigabe liefe nie.
        if let ObjectKind::Debuggable { pd } = self.objects[obj].kind {
            if self.slots[slot].rights.contains(Rights::WRITE)
                && self.slots[slot].mdb.parent.is_some()
            {
                rf.push_debug(pd);
            }
        }
        self.unlink(slot);
        self.release_slot(slot);

        self.objects[obj].refcount -= 1;
        if self.objects[obj].refcount == 0 {
            // Memory-Objekte geben ihre Region an den RAM-Allokator zurück; Reply-Objekte
            // melden ihren Call zum Abbruch (Kernel entblockt den Aufrufer). Alle anderen
            // (Endpoint/Notification/Tcb/SchedContext/PdControl/**Mmio**/**Irq**) halten KEINEN
            // RAM-Allokator-Eintrag — insbesondere `Mmio`/`Irq` verweisen auf einen Geräte-
            // Bereich (NICHT auf RAM): hier NIEMALS `free_region` rufen (sonst Korruption).
            //
            // `Dma` ist die Ausnahme (ext-23): es IST kernel-ausgeschnittenes RAM und wird wie
            // `Memory` freigegeben. Das ist DMA-use-after-free-sicher, **weil** die System-
            // Teardown-Reihenfolge (`enforcer.disable_dma` -> SMMU-Invalidierung -> VSpace-Unmap)
            // garantiert, dass vor dieser Freigabe kein Gerät mehr in die Region schreiben kann.
            match self.objects[obj].kind {
                ObjectKind::Memory(region) => {
                    alloc.free_region(region);
                }
                // **Nicht** freigeben (ext-37): die Region geht als Meldung an den Kernel, der
                // sie erst nach Stilllegung + Unmap + Sync über einen `DmaTeardownToken`
                // zurückgeben darf. Ein `free_region` hier wäre die Freigabe *vor* dem Nachweis
                // — genau die Reihenfolge, die den geräteseitigen Use-after-free ausmacht.
                ObjectKind::Dma { phys, len, .. } => rf.push_dma(phys, len),
                ObjectKind::Reply { ep, caller } => rf.push(ep, caller),
                // **Z6b: die LETZTE Debug-Cap ueber dieser PD geht.** Nur der Debugger entfernt
                // `BlockReasons::DEBUG`; ohne diese Meldung traegt ein angehaltener Thread einen
                // Grund, den niemand mehr entfernen darf.
                //
                // Dass „die letzte" hier keine Suche kostet, ist die Wirkung der Rechte-Ableitung:
                // alle Debug-Caps ueber `pd` teilen **ein** Objekt, und dieser Zweig laeuft erst
                // bei `refcount == 0`. Mit drei getrennten Objektarten haette hier ein Durchlauf
                // der Objekttabelle stehen muessen — und `revoke` an der Wurzel haette die anderen
                // beiden ueberhaupt nicht erreicht.
                _ => {}
            }
            let gen = self.objects[obj].gen.wrapping_add(1);
            self.objects[obj] = Object::EMPTY;
            self.objects[obj].gen = gen;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kette `0 -> 1 -> ... -> n-1` ueber `first_child` (Tiefe n-1).
    fn kette<const N: usize>() -> [CapSlot; N] {
        let mut v = [CapSlot::EMPTY; N];
        for (i, s) in v.iter_mut().enumerate() {
            s.used = true;
            s.mdb.first_child = if i + 1 < N { Some(i + 1) } else { None };
        }
        v
    }

    /// Der Normalfall: der Abstieg findet das Blatt und meldet die **Anzahl Schritte**.
    /// Genau diese Zahl ist die Zusage -- eine Operationszahl, keine Zeit.
    #[test]
    fn abstieg_findet_das_blatt_und_zaehlt_die_schritte() {
        let slots = kette::<4>();
        assert_eq!(descend_to_leaf(&slots, 0, 8), Ok((3, 3)));
        // Von der Mitte aus entsprechend kuerzer.
        assert_eq!(descend_to_leaf(&slots, 2, 8), Ok((3, 1)));
        // Ein Blatt ist nach null Schritten erreicht.
        assert_eq!(descend_to_leaf(&slots, 3, 8), Ok((3, 0)));
    }

    /// **Die Grenze greift.** Eine Kette, die laenger ist als erlaubt, wird abgewiesen statt
    /// zu Ende gelaufen. Ohne diese Pruefung gaebe es keine Aussage darueber, wie lang die
    /// kritische Sektion hoechstens wird.
    #[test]
    fn abstieg_bricht_an_der_schranke_ab() {
        let slots = kette::<6>();
        assert_eq!(descend_to_leaf(&slots, 0, 5), Ok((5, 5)), "genau an der Grenze noch gueltig");
        assert_eq!(descend_to_leaf(&slots, 0, 4), Err(()), "ueber der Grenze -> Befund");
    }

    /// **Der Fall, um den es wirklich geht:** eine zyklische `first_child`-Kette. Ohne Schranke
    /// laeuft der Abstieg hier endlos -- im Kernel unter der CAPS-Sperre, die dann niemand mehr
    /// freigibt. Dass dieser Test ueberhaupt *terminiert*, ist das Ergebnis.
    #[test]
    fn abstieg_terminiert_auf_einem_zyklus() {
        let mut slots = [CapSlot::EMPTY; 3];
        for s in slots.iter_mut() {
            s.used = true;
        }
        slots[0].mdb.first_child = Some(1);
        slots[1].mdb.first_child = Some(2);
        slots[2].mdb.first_child = Some(1); // zurueck -> Zyklus
        assert_eq!(descend_to_leaf(&slots, 0, 3), Err(()));
    }

    /// Ein Kindindex ausserhalb der Tabelle ist ebenfalls „nicht baumfoermig" -- und darf kein
    /// Panic sein. Ein `slots[i]` mit rohem Index waere hier ein Kernel-Panic aus Mandantendaten.
    #[test]
    fn abstieg_weist_index_ausserhalb_der_tabelle_ab() {
        let mut slots = [CapSlot::EMPTY; 2];
        slots[0].used = true;
        slots[0].mdb.first_child = Some(99);
        assert_eq!(descend_to_leaf(&slots, 0, 8), Err(()));
        assert_eq!(descend_to_leaf(&slots, 99, 8), Err(()), "Startindex ebenso");
    }

    /// Die Kinderliste zaehlt richtig -- und bricht bei einer zyklischen Geschwisterkette ab
    /// statt zu haengen. `move_cap` laeuft ueber genau diese Verkettung.
    #[test]
    fn kinderliste_zaehlt_und_bricht_ab() {
        // parent 0 mit den Kindern 1,2,3.
        let mut slots = [CapSlot::EMPTY; 4];
        for s in slots.iter_mut() {
            s.used = true;
            s.mdb = Mdb::EMPTY;
        }
        slots[0].mdb.first_child = Some(1);
        slots[1].mdb.next_sibling = Some(2);
        slots[2].mdb.next_sibling = Some(3);
        assert_eq!(count_children(&slots, 0, 8), Ok(3));
        assert_eq!(count_children(&slots, 3, 8), Ok(0), "ein Blatt hat keine Kinder");
        // Und jetzt der Zyklus: 3 zeigt zurueck auf 1.
        slots[3].mdb.next_sibling = Some(1);
        assert_eq!(count_children(&slots, 0, 8), Err(()));
    }

    /// Auch die Kinderliste weist einen Index ausserhalb der Tabelle ab.
    #[test]
    fn kinderliste_weist_index_ausserhalb_der_tabelle_ab() {
        let mut slots = [CapSlot::EMPTY; 2];
        slots[0].used = true;
        slots[0].mdb.first_child = Some(42);
        assert_eq!(count_children(&slots, 0, 8), Err(()));
    }

    // ============================================================================================
    // CSUB: das Teilfenster (reine Entscheidung) + die CDT-Einhängung
    // ============================================================================================
    //
    // Was hier steht, braucht weder Slab-Speicher noch Allokator und läuft deshalb auf dem
    // Host (`tools/host-tests.sh cap`): die Fensterreichnung über eingespeisten Werten und die
    // Verkettung über einem Stapel-Array. Die volle Kette (Rechte-Schnitt am `CapSpace`,
    // `revoke` zieht das Teil ein, Audit bleibt 0) braucht angebundenen Slab-Speicher
    // (`Slab::attach` ist `unsafe`, diese Crate ist `forbid(unsafe_code)`) und steht als
    // Integrationslauf ausserhalb der Crate.

    fn region_ab(base: u64, len: u64) -> PhysRegion {
        PhysRegion::new(base, len)
    }

    #[test]
    fn teilfenster_genau_innen() {
        // Mitte der Region, mit exakten Koordinaten.
        let f = teilfenster_rechnen(region_ab(0x10_000, 0x8000), 0x1000, 0x2000).unwrap();
        assert_eq!(f, region_ab(0x11_000, 0x2000));
        // Offset 0 + volle Länge = die Region selbst (bitgleich zur Vorlage).
        let f = teilfenster_rechnen(region_ab(0x10_000, 0x8000), 0, 0x8000).unwrap();
        assert_eq!(f, region_ab(0x10_000, 0x8000));
        // Das letzte Byte ist noch innen.
        let f = teilfenster_rechnen(region_ab(0x10_000, 0x8000), 0x7FFF, 1).unwrap();
        assert_eq!(f, region_ab(0x17_FFF, 1));
    }

    #[test]
    fn teilfenster_lehnt_leer_und_aussen_ab() {
        let w = region_ab(0x10_000, 0x8000);
        // Leeres Fenster ist kein Fenster — auch nicht an Offset 0.
        assert_eq!(teilfenster_rechnen(w, 0, 0), Err(CapError::Subregion));
        assert_eq!(teilfenster_rechnen(w, 0x1000, 0), Err(CapError::Subregion));
        // Ein Byte über das Ende hinaus ist draussen.
        assert_eq!(teilfenster_rechnen(w, 0x7FFF, 2), Err(CapError::Subregion));
        assert_eq!(teilfenster_rechnen(w, 0x8000, 1), Err(CapError::Subregion));
        // Offset hinter der Region ist draussen, auch mit Länge 1.
        assert_eq!(teilfenster_rechnen(w, 0x9000, 1), Err(CapError::Subregion));
        // Die Absage heisst `Subregion`, nicht `Invalid`: „kleiner fragen", nicht
        // „andere Cap nehmen" — der Dispatch bildet sie auf ERR_SUBREGION (21) ab.
        assert_ne!(teilfenster_rechnen(w, 0, 0), Err(CapError::Invalid));
    }

    #[test]
    fn teilfenster_lehnt_ueberlauf_ab() {
        let w = region_ab(0x10_000, 0x8000);
        // `Basis + Offset` läuft über.
        assert_eq!(teilfenster_rechnen(w, u64::MAX, 1), Err(CapError::Subregion));
        assert_eq!(
            teilfenster_rechnen(w, u64::MAX - 0x10_000 + 1, 1),
            Err(CapError::Subregion)
        );
        // `Offset + Länge` läuft über (Basis 0, damit nur die zweite Addition trägt).
        let flach = region_ab(0, 0x1000);
        assert_eq!(teilfenster_rechnen(flach, u64::MAX, u64::MAX), Err(CapError::Subregion));
        assert_eq!(teilfenster_rechnen(flach, 1, u64::MAX), Err(CapError::Subregion));
        // Eine Region, deren Ende nicht darstellbar ist, verleiht nichts: fail-closed statt
        // umlaufender `end()`-Vergleich.
        let kipp = region_ab(u64::MAX - 0xFFF, 0x2000);
        assert_eq!(teilfenster_rechnen(kipp, 0, 1), Err(CapError::Subregion));
    }

    #[test]
    fn einhaengen_baut_die_kindkette() {
        // **„Kind im CDT" auf dem Host.** `subregion` hängt über `link_child` ein, und das
        // steht genau einmal in `kind_einhaengen` — was hier über einem Stapel-Array gilt,
        // gilt dort über dem Slab.
        let mut slots = [CapSlot::EMPTY; 3];
        for s in slots.iter_mut() {
            s.used = true;
        }
        kind_einhaengen(&mut slots, 0, 1);
        assert_eq!(slots[0].mdb.first_child, Some(1));
        assert_eq!(slots[1].mdb.parent, Some(0));
        assert_eq!(slots[1].mdb.prev_sibling, None);
        assert_eq!(slots[1].mdb.next_sibling, None);
        // Das zweite Kind hängt sich VORNE ein; die Geschwisterkette bleibt reziprok.
        kind_einhaengen(&mut slots, 0, 2);
        assert_eq!(slots[0].mdb.first_child, Some(2));
        assert_eq!(slots[2].mdb.next_sibling, Some(1));
        assert_eq!(slots[1].mdb.prev_sibling, Some(2));
        assert_eq!(slots[2].mdb.parent, Some(0));
        assert_eq!(count_children(&slots, 0, 8), Ok(2));
    }
}
