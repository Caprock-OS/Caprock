//! Capability-basierter physischer Allokator.
//!
//! Verwaltet freies RAM als sortierte, koaleszierende Freiliste (feste Kapazität,
//! daher allokationsfrei und deterministisch) und prägt Wurzel-[`MemoryCap`]s.
//! `alloc` carvt eine seitenausgerichtete Region (First-Fit nach Adresse);
//! `free` gibt sie zurück und verschmilzt benachbarte Bereiche.

use crate::cap::MemoryCap;
use crate::color::{color_of, ColorMask, MASK_BITS};
use crate::region::{PhysRegion, Rights};
use crate::PAGE;

/// Tragen **alle** `npages` Seiten ab `start` eine Farbe aus `mask`?
///
/// Bewusst eine eigene Funktion mit früher Rückgabe bei der ersten falschen Farbe: die
/// Bedingung ist ein „für alle", und ein „für alle" das aus Bequemlichkeit nur die erste Seite
/// prüft, hält die Zusicherung nicht.
fn run_is_colored(start: u64, npages: u64, colors: u32, mask: ColorMask) -> bool {
    for j in 0..npages {
        if !mask.contains(color_of(start + j * PAGE, colors)) {
            return false;
        }
    }
    true
}

/// Maximale Anzahl freier Fragmente. 64 genügte QEMU, wird aber auf realer HW mit vielen
/// GLEICHZEITIG lebenden isolierten PDs überschritten -> `alloc` gibt `None` trotz freiem RAM
/// (Split-Rest passt nicht in die Liste). 1024 * 16 B = 16 KiB BSS; alloc/free scannen linear (O(n)).
/// **Öffentlich seit 2026-08-10**, weil ein Aufrufer den Fall „freies RAM ist da, die Liste ist
/// voll" sonst nicht von „zu wenig Speicher" unterscheiden kann — und dann im Speicherverbrauch
/// sucht, wo eine Kapazität fehlt (s. `caprock_dma::gross::GrossDmaFehler::FreilisteVoll`). Eine
/// Kapazität, die niemand lesen kann, macht ihren Überlauf unbenennbar.
pub const MAX_FRAGMENTS: usize = 1024;

/// Physischer Speicher-Allokator über eine feste Freiliste.
pub struct PhysAllocator {
    regions: [PhysRegion; MAX_FRAGMENTS],
    len: usize,
}

const fn align_up(value: u64, align: u64) -> u64 {
    (value + (align - 1)) & !(align - 1)
}

impl Default for PhysAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysAllocator {
    pub const fn new() -> Self {
        Self {
            regions: [PhysRegion::new(0, 0); MAX_FRAGMENTS],
            len: 0,
        }
    }

    /// Freies RAM registrieren (wird in die Freiliste eingefügt + koalesziert).
    /// Gibt `false` zurück, wenn die Freiliste voll ist.
    pub fn add_region(&mut self, base: u64, len: u64) -> bool {
        self.insert(PhysRegion::new(base, len))
    }

    /// `size` Bytes mit Ausrichtung `align` (mind. [`PAGE`]) allozieren.
    /// Liefert eine RW-Wurzel-Cap oder `None`, wenn kein Platz passt.
    pub fn alloc(&mut self, size: u64, align: u64) -> Option<MemoryCap> {
        self.alloc_below(size, align, u64::MAX)
    }

    /// Wie [`alloc`](Self::alloc), aber die Region muss **vollständig unterhalb von `limit`**
    /// liegen — der Zonenwunsch des Aufrufers, im Allokator statt daneben.
    ///
    /// **Warum es diese Funktion gibt** (E-Rest 3b, 2026-08-04). Manche Regionen sind nicht
    /// austauschbar: eine DMA-Region und die private Region einer isolierten PD *müssen* in
    /// GiB 0 liegen, weil nur dort eine PD-eigene Abbildung entstehen kann. Bis hierher fragten
    /// diese Aufrufer nach „irgendeiner" Region, prüften die Grenze danach und gaben bei
    /// Verfehlung auf — **ohne ein zweites Mal zu fragen**. Das ging gut, solange der Speicher
    /// zusammenhing und Best-Fit von unten belegte; sobald ein zweiter, *kleinerer* Bereich
    /// oberhalb 4 GiB dazukam, wählte Best-Fit ihn für jede kleine Anforderung und der Aufrufer
    /// bekam eine Adresse, die er nicht gebrauchen konnte. Gemessen bei `-m 3G`:
    /// `dmawin`/`dmatok : FAILURES`, `iso : spawn_isolated fehlgeschlagen` — während 4G und 6G
    /// grün waren, weil dort der obere Bereich zufällig größer ist.
    ///
    /// Ein Fehler, der an einer **Größenrelation** hängt statt an der Struktur, verschwindet
    /// beim nächsten Messwert. Deshalb kennt die Freiliste den Wunsch jetzt, statt ihn zu
    /// erraten: gesucht wird nur unter Fragmenten, die ihn erfüllen können, und ein Fragment,
    /// das die Grenze überschreitet, wird an ihr **beschnitten** statt verworfen — sonst wäre
    /// ein einzelner Bereich über die Grenze hinweg für die Zone unbenutzbar.
    ///
    /// `limit == u64::MAX` heisst „egal" und ist bit-identisch zum bisherigen Verhalten.
    pub fn alloc_below(&mut self, size: u64, align: u64, limit: u64) -> Option<MemoryCap> {
        self.alloc_in(size, align, 0, limit)
    }

    /// Wie [`alloc_below`](Self::alloc_below), aber die Zone ist ein **Intervall** `[lo, hi)`.
    ///
    /// Die Untergrenze kam mit E-Rest 3d dazu, und sie hat einen anderen Zweck als die obere:
    /// die obere ist eine **Bedingung** („diese Region muss PD-privat abbildbar sein"), die
    /// untere eine **Vorliebe** („nimm bitte nicht aus dem knappen GiB 0, wenn du es nicht
    /// brauchst"). Beide über dieselbe Suche zu führen, ist billiger als zwei Verfahren — und
    /// vor allem: der Aufrufer sagt in EINEM Ausdruck, was er meint, statt es aus der
    /// Reihenfolge zweier Aufrufe folgen zu lassen.
    pub fn alloc_in(&mut self, size: u64, align: u64, lo: u64, hi: u64) -> Option<MemoryCap> {
        let align = align.max(PAGE);
        let size = align_up(size.max(1), PAGE);

        // BEST-FIT: das Fragment mit dem KLEINSTEN Rest (`r.len - size`) wählen, das die Allokation
        // (nach Alignment) fasst. So landen kleine Allocs in kleinen Löchern und GROSSE Löcher
        // bleiben für große/ausgerichtete Allocs erhalten (z. B. 2-MiB-aligned isolierte Stacks) —
        // vermeidet First-Fit-Fragmentierung (4-KiB-Tabellen knabbern das erste große Fragment an).
        // Semantik sonst unverändert (Split/Coalesce/Accounting) -> memtest (1 Fragment) unberührt.
        // BEST-FIT **innerhalb der Zone**: verglichen wird die Länge des *nutzbaren* Teils, nicht
        // die des Fragments. Sonst gewänne ein riesiges Fragment, von dem nur ein Zipfel unter
        // der Grenze liegt, gegen ein kleines, das ganz hineinpasst — und Best-Fit hiesse etwas
        // anderes, sobald jemand eine Grenze nennt.
        let mut best: Option<(usize, u64, u64)> = None;
        for i in 0..self.len {
            let r = self.regions[i];
            // Der in der Zone nutzbare Teil dieses Fragments -- an BEIDEN Enden beschnitten.
            let zone_start = r.base.max(lo);
            let zone_end = r.end().min(hi);
            if zone_end <= zone_start {
                continue; // liegt vollständig ausserhalb der Zone
            }
            let start = align_up(zone_start, align);
            // Overflow beim Ausrichten/Addieren -> dieses Fragment überspringen.
            let Some(end) = start.checked_add(size) else {
                continue;
            };
            let nutzbar = zone_end - zone_start;
            if start >= r.base && end <= zone_end {
                // Beidseitiger Verschnitt erhöht die Fragmentzahl um 1; ist die Liste dann voll,
                // ginge das Suffix beim `insert` verloren (Leck) -> dieses Fragment überspringen.
                let two_sided = start > r.base && end < r.end();
                if two_sided && self.len >= MAX_FRAGMENTS {
                    continue;
                }
                // Gemerkt wird der **Startpunkt**, nicht nur das Fragment. Ihn danach aus
                // `r.base` neu auszurechnen war der Fehler der ersten Fassung: die Suche kannte
                // die Untergrenze, der Zuschnitt nicht -- gefunden wurde in der Zone,
                // herausgeschnitten darunter. Drei Host-Tests haben das sofort gezeigt.
                if best.map_or(true, |(_, _, n)| nutzbar < n) {
                    best = Some((i, start, nutzbar));
                }
            }
        }
        let (i, start, _) = best?;
        let r = self.regions[i];
        let end = start + size;
        self.remove(i);
        self.insert(PhysRegion::new(r.base, start - r.base)); // Präfix (evtl. leer)
        self.insert(PhysRegion::new(end, r.end() - end)); //       Suffix (evtl. leer)
        Some(MemoryCap::new(PhysRegion::new(start, size), Rights::RW))
    }

    /// Wie [`alloc`](Self::alloc), aber **jede Seite** der gelieferten Region trägt eine Farbe
    /// aus `mask` (Cache-Partitionierung, `TODO.md` A1).
    ///
    /// Aufeinanderfolgende Seiten tragen aufeinanderfolgende Farben. Eine zusammenhängende
    /// Region über `n` Seiten überstreicht also `n` aufeinanderfolgende Farben — **das ist die
    /// eigentliche Grenze des Verfahrens**, nicht eine Eigenschaft dieser Funktion: wer eine
    /// Region will, die größer ist als sein Farbanteil, kann sie nicht bekommen, ohne fremde
    /// Farben mitzunehmen. Die Funktion gibt dann `None` zurück, statt die Zusicherung still
    /// aufzuweichen.
    ///
    /// `None` auch bei leerer Maske. `colors <= 1` heißt „keine Farbunterscheidung möglich":
    /// dann ist jede Region einfarbig, und die Funktion verhält sich wie [`alloc`](Self::alloc)
    /// — der **Aufrufer** muss entscheiden, ob er das akzeptiert (s. `colors::usable` im Kernel),
    /// denn eine erfüllte Farbbedingung über einer einzigen Farbe sagt nichts aus.
    pub fn alloc_colored(
        &mut self,
        size: u64,
        align: u64,
        colors: u32,
        mask: ColorMask,
    ) -> Option<MemoryCap> {
        self.alloc_colored_below(size, align, colors, mask, u64::MAX)
    }

    /// Wie [`alloc_colored`](Self::alloc_colored), aber zusätzlich mit dem Zonenwunsch aus
    /// [`alloc_below`](Self::alloc_below). Farbe **und** Zone werden hier gemeinsam entschieden;
    /// zwei nacheinander laufende Politiken würden gegeneinander arbeiten (dasselbe Argument
    /// steht in `TODO.md` Z8 für Farbe und NUMA-Knoten).
    pub fn alloc_colored_below(
        &mut self,
        size: u64,
        align: u64,
        colors: u32,
        mask: ColorMask,
        limit: u64,
    ) -> Option<MemoryCap> {
        self.alloc_colored_in(size, align, colors, mask, 0, limit)
    }

    /// Wie [`alloc_colored_below`](Self::alloc_colored_below), aber mit dem Zonen-**Intervall**
    /// aus [`alloc_in`](Self::alloc_in).
    pub fn alloc_colored_in(
        &mut self,
        size: u64,
        align: u64,
        colors: u32,
        mask: ColorMask,
        lo: u64,
        hi: u64,
    ) -> Option<MemoryCap> {
        self.alloc_colored_vorspann_in(0, size, align, colors, mask, lo, hi)
    }

    /// **Gefärbte Region mit einem farblich UNGEBUNDENEN Vorspann.**
    ///
    /// Die ersten `vorspann` Seiten der gelieferten Region dürfen jede Farbe tragen; erst der
    /// Rest muss in `mask` liegen. Es gibt genau einen Grund dafür, und er ist eine bezahlte
    /// Rechnung, keine Bequemlichkeit:
    ///
    /// **Eine Wachseite trägt keine Daten.** Sie ist unabgebildet (x86) bzw. auf dieser
    /// Architektur überhaupt nicht vorhanden (aarch64, `guard_unterstuetzt() == false`) — sie
    /// belegt kein einziges Cache-Set. Die Farbzusicherung ist eine Aussage darüber, welche
    /// Cache-Sets eine PD **benutzt**; eine Seite, die nie gelesen und nie geschrieben wird,
    /// gehört nicht dazu. Sie trotzdem in den Streifen zu zwingen, ist keine schärfere
    /// Zusicherung, sondern eine, die den Anforderer um eine Seite über die Streifenbreite
    /// hinausschiebt.
    ///
    /// **Was das kostete** (2026-08-13, C9e): auf aarch64 hat die Maschine 16 Farben, ein
    /// Streifen also 4. Ein EL0-Kernel-Stack ist dort 16 KiB = 4 Seiten — **genau** ein
    /// Streifen. Seit dem 2026-08-10 fordert `claim_user_kstack_masked` eine Seite mehr an (die
    /// Wache), also **5** aufeinanderfolgende Seiten. Fünf aufeinanderfolgende Seiten tragen
    /// fünf **verschiedene** Farben; in einen 4-Farben-Streifen passen sie nie. Damit war die
    /// gefärbte Stack-Anforderung **strukturell unerfüllbar**, `spawn_isolated_colored` gab
    /// immer `None`, und der ganze A1-Farbtest meldete lauter Nullen. Auf x86 fiel es nicht
    /// auf: dort ist ein Streifen 16 lebendige Maskenbits breit und der Stack seit C4 2 Seiten.
    ///
    /// Das ist dieselbe Klasse wie „unten zuerst war ein Zufall der Größenrelation": die
    /// Eigenschaft „der Stack passt in seinen Streifen" folgte aus `Stackseiten == Streifenbreite`
    /// und nicht aus der Struktur — und verschwand beim nächsten Messwert.
    ///
    /// `vorspann >= size / PAGE` wäre eine Anforderung ohne jede Farbbedingung und wird
    /// abgewiesen: wer keine Farbe braucht, nimmt [`alloc_in`](Self::alloc_in) und sagt es damit.
    pub fn alloc_colored_vorspann_in(
        &mut self,
        vorspann: u64,
        size: u64,
        align: u64,
        colors: u32,
        mask: ColorMask,
        lo: u64,
        hi: u64,
    ) -> Option<MemoryCap> {
        if mask.is_empty() {
            return None;
        }
        if colors <= 1 || mask == ColorMask::ALL {
            return self.alloc_in(size, align, lo, hi);
        }
        let align = align.max(PAGE);
        let size = align_up(size.max(1), PAGE);
        let npages = size / PAGE;
        // Ein Vorspann, der die ganze Region verschlingt, ist keine gefaerbte Anforderung mehr.
        // Fail-closed statt stillschweigend die Zusicherung fallen zu lassen.
        if vorspann >= npages {
            return None;
        }
        let gefaerbt = npages - vorspann;
        // Mehr Seiten als Maskenbits ginge nur mit lückenloser Maske — die ist oben schon
        // abgefangen. Ohne diese Zeile liefe die Suche über alle Fragmente ins Leere.
        // **Gezaehlt wird der gefaerbte Teil**, nicht die Gesamtgroesse: der Vorspann steht
        // unter keiner Farbbedingung und kann sie deshalb auch nicht sprengen.
        if gefaerbt > MASK_BITS as u64 {
            return None;
        }
        let step = align;

        let mut best: Option<(usize, u64)> = None;
        let mut best_nutzbar = u64::MAX;
        for i in 0..self.len {
            let r = self.regions[i];
            let zone_start = r.base.max(lo);
            let zone_end = r.end().min(hi);
            if zone_end <= zone_start {
                continue; // liegt vollständig ausserhalb der Zone
            }
            let mut start = align_up(zone_start, align);
            let nutzbar = zone_end - zone_start;
            // Das Farbmuster wiederholt sich spätestens nach `colors` Schritten; mehr zu
            // probieren kann nichts Neues finden.
            let mut tries = 0u32;
            while tries <= colors {
                let Some(end) = start.checked_add(size) else {
                    break;
                };
                if end > zone_end {
                    break;
                }
                if run_is_colored(start + vorspann * PAGE, gefaerbt, colors, mask) {
                    let two_sided = start > r.base && end < r.end();
                    if !(two_sided && self.len >= MAX_FRAGMENTS) {
                        // Best-Fit wie in `alloc_below`: kleinster **nutzbarer** Teil zuerst.
                        if nutzbar < best_nutzbar {
                            best = Some((i, start));
                            best_nutzbar = nutzbar;
                        }
                    }
                    break;
                }
                let Some(next) = start.checked_add(step) else {
                    break;
                };
                start = next;
                tries += 1;
            }
        }
        let (i, start) = best?;
        let r = self.regions[i];
        let end = start + size;
        self.remove(i);
        self.insert(PhysRegion::new(r.base, start - r.base));
        self.insert(PhysRegion::new(end, r.end() - end));
        Some(MemoryCap::new(PhysRegion::new(start, size), Rights::RW))
    }

    /// Eine Capability zurückgeben (konsumiert sie) und ihre Region freigeben.
    pub fn free(&mut self, cap: MemoryCap) -> bool {
        self.insert(cap.region())
    }

    /// Eine rohe Region freigeben (für die Finalisierung im Capability-System,
    /// wenn die letzte Cap auf ein Memory-Objekt gelöscht wird; die Korrektheit
    /// — genau einmalige Freigabe — sichert dort der Derivation-Tree).
    pub fn free_region(&mut self, region: PhysRegion) -> bool {
        self.insert(region)
    }

    /// Ist `[base, base+len)` **vollständig** frei?
    ///
    /// Gegenstück zu [`overlaps_free`](Self::overlaps_free), die schon bei teilweiser
    /// Überlappung `true` sagt. Für eine Leckprüfung braucht es die strenge Form: „die Region
    /// dieser PD ist zurück" ist etwas anderes als „ein Teil davon ist zurück".
    ///
    /// Warum das die richtige Leckprüfung ist und nicht ein Vorher/Nachher-Vergleich von
    /// [`total_free`](Self::total_free): der Summenzähler ist **global**. Läuft nebenher ein
    /// anderer Kern, der einen Thread-Stack einsammelt, ändert er die Summe, ohne dass die
    /// geprüfte Operation etwas damit zu tun hätte — der Test wird dann aus fremdem Grund rot
    /// (auf dem x86-Testaufbau reproduziert: `bilanz` mal 1, mal 0, gleicher Kernel). Diese
    /// Prüfung fragt stattdessen nach genau der Region, um die es geht.
    pub fn fully_free(&self, base: u64, len: u64) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        // Die Liste ist sortiert und koalesziert: eine vollständig freie Region liegt deshalb
        // in **einem** Fragment. Wäre sie über zwei verteilt, wären die beiden benachbart und
        // schon verschmolzen.
        for i in 0..self.len {
            let r = &self.regions[i];
            if r.base <= base && end <= r.end() {
                return true;
            }
        }
        false
    }

    /// Summe der freien Bytes.
    pub fn total_free(&self) -> u64 {
        let mut sum = 0;
        for i in 0..self.len {
            sum += self.regions[i].len;
        }
        sum
    }

    /// Anzahl freier (koaleszierter) Fragmente.
    pub fn fragments(&self) -> usize {
        self.len
    }

    /// Überlappt `[base, base+len)` ein aktuell **freies** Fragment? Trägt die DMA-Revoke-
    /// Ordnung-Invariante (`docs/invariants.md` §2): eine noch in einem SMMU-Kontext gemappte
    /// Region darf NIE freigegeben sein — andernfalls läge sie in der Free-Liste und überlappte
    /// hier (DMA-use-after-free). `len == 0` → nie eine Überlappung.
    pub fn overlaps_free(&self, base: u64, len: u64) -> bool {
        if len == 0 {
            return false;
        }
        let end = base.saturating_add(len);
        for i in 0..self.len {
            let r = &self.regions[i];
            if base < r.base + r.len && r.base < end {
                return true;
            }
        }
        false
    }

    // --- intern ---

    /// Sortiert (nach `base`) einfügen und anschließend koaleszieren.
    fn insert(&mut self, r: PhysRegion) -> bool {
        if r.is_empty() {
            return true;
        }
        if self.len >= MAX_FRAGMENTS {
            return false;
        }
        let mut i = 0;
        while i < self.len && self.regions[i].base < r.base {
            i += 1;
        }
        let mut j = self.len;
        while j > i {
            self.regions[j] = self.regions[j - 1];
            j -= 1;
        }
        self.regions[i] = r;
        self.len += 1;
        self.coalesce();
        true
    }

    fn remove(&mut self, idx: usize) {
        let mut k = idx;
        while k + 1 < self.len {
            self.regions[k] = self.regions[k + 1];
            k += 1;
        }
        self.len -= 1;
    }

    /// Benachbarte Regionen verschmelzen (Liste ist sortiert + überlappungsfrei).
    #[allow(clippy::needless_range_loop)]
    fn coalesce(&mut self) {
        let mut i = 0;
        while i + 1 < self.len {
            if self.regions[i].end() == self.regions[i + 1].base {
                self.regions[i].len += self.regions[i + 1].len;
                let mut k = i + 1;
                while k + 1 < self.len {
                    self.regions[k] = self.regions[k + 1];
                    k += 1;
                }
                self.len -= 1;
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::stripe;

    /// 256 Farben — die auf dem x86-Testaufbau **gemessene** Zahl (QEMU q35,
    /// `-cpu Skylake-Client`: LLC 16 MiB, 16-fach, 64 B/Zeile). Keine Wunschzahl.
    const COLORS: u32 = 256;

    /// Allokator mit einem zusammenhaengenden RAM-Fenster ab `base`.
    fn with_ram(base: u64, len: u64) -> PhysAllocator {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(base, len));
        a
    }

    /// Traegt jede Seite der Region eine Farbe aus `mask`?
    fn all_pages_in(cap: &MemoryCap, mask: ColorMask) -> bool {
        (0..cap.len() / PAGE).all(|j| mask.contains(color_of(cap.base() + j * PAGE, COLORS)))
    }

    /// Die Farbmenge, die eine Region tatsaechlich belegt.
    fn colors_used(cap: &MemoryCap) -> std::collections::BTreeSet<u32> {
        (0..cap.len() / PAGE).map(|j| color_of(cap.base() + j * PAGE, COLORS)).collect()
    }

    #[test]
    fn farbige_allokation_liefert_nur_erlaubte_farben() {
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let m = stripe(1, 4, COLORS).unwrap();
        let cap = a.alloc_colored(4 * PAGE, PAGE, COLORS, m).expect("Platz vorhanden");
        assert!(all_pages_in(&cap, m), "Seite mit unerlaubter Farbe vergeben");
    }

    /// **Die A1-Eigenschaft im Allokator**: zwei PDs mit disjunkten Streifen bekommen
    /// Seiten, die sich keine einzige Cache-Farbe teilen.
    #[test]
    fn disjunkte_streifen_teilen_keine_farbe() {
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let (m0, m1) = (stripe(0, 2, COLORS).unwrap(), stripe(1, 2, COLORS).unwrap());
        let c0 = a.alloc_colored(8 * PAGE, PAGE, COLORS, m0).expect("PD 0");
        let c1 = a.alloc_colored(8 * PAGE, PAGE, COLORS, m1).expect("PD 1");
        assert!(all_pages_in(&c0, m0));
        assert!(all_pages_in(&c1, m1));
        let (s0, s1) = (colors_used(&c0), colors_used(&c1));
        assert!(s0.is_disjoint(&s1), "gemeinsame Farben: {:?}", &s0 & &s1);
        // und die Regionen ueberlappen auch physisch nicht
        assert!(c0.base() + c0.len() <= c1.base() || c1.base() + c1.len() <= c0.base());
    }

    /// **Sensitivitaet**: ohne Farbbedingung liefert derselbe Allokator sehr wohl Seiten
    /// ausserhalb des Streifens. Ohne diesen Test koennte `disjunkte_streifen_teilen_keine_farbe`
    /// gruen sein, weil die Anordnung es zufaellig hergibt — nicht, weil die Maske wirkt.
    #[test]
    fn ohne_maske_faellt_die_eigenschaft_um() {
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let m = stripe(1, 4, COLORS).unwrap(); // Farben 16..31 (mod 64)
        let cap = a.alloc(4 * PAGE, PAGE).expect("Platz vorhanden");
        assert!(
            !all_pages_in(&cap, m),
            "ungefaerbte Allokation landete zufaellig im Streifen -> der Farbtest waere leer"
        );
    }

    /// Eine Region, die groesser ist als der Farbanteil, wird **abgewiesen** statt
    /// stillschweigend fremde Farben mitzunehmen. Das ist die harte Grenze des Verfahrens.
    #[test]
    fn zu_grosse_region_wird_abgewiesen() {
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let m = stripe(0, 4, COLORS).unwrap(); // 16 von 64 Bits
        assert!(a.alloc_colored(16 * PAGE, PAGE, COLORS, m).is_some(), "16 Seiten passen");
        assert!(
            a.alloc_colored(17 * PAGE, PAGE, COLORS, m).is_none(),
            "17 Seiten passen nicht in 16 aufeinanderfolgende Farben"
        );
        // Und der Klassiker: eine 2-MiB-Region (512 Seiten) ist mit KEINER echten
        // Teilmaske erfuellbar -- genau der Grund, warum der 2-MiB-Blockdeskriptor
        // der isolierten PD nicht gefaerbt werden kann (s. TODO.md A1).
        assert!(a.alloc_colored(512 * PAGE, 512 * PAGE, COLORS, m).is_none());
    }

    /// `fully_free` unterscheidet „ganz zurueck" von „teilweise zurueck" — genau die
    /// Unterscheidung, an der ein Vorher/Nachher-Summenvergleich vorbeigeht.
    #[test]
    fn fully_free_ist_streng() {
        let mut a = with_ram(0x4000_0000, 16 * 1024 * 1024);
        let cap = a.alloc(8 * PAGE, PAGE).unwrap();
        let (b, l) = (cap.base(), cap.len());
        assert!(!a.fully_free(b, l), "belegte Region gilt als frei");
        assert!(a.free(cap));
        assert!(a.fully_free(b, l), "zurueckgegebene Region gilt nicht als frei");
        // Haelfte wieder wegnehmen: `overlaps_free` sagt weiter ja, `fully_free` muss nein sagen.
        let half = a.alloc(4 * PAGE, PAGE).unwrap();
        assert!(a.overlaps_free(b, l), "Rest ist noch frei -> Ueberlappung besteht");
        assert!(!a.fully_free(b, l), "teilweise belegte Region gilt als vollstaendig frei");
        assert!(a.free(half));
    }

    /// **C9e, die Geometrie, an der die aarch64-Suite hing — ohne QEMU entscheidbar.**
    ///
    /// aarch64 hat 16 Farben; bei 4 Partitionen ist ein Streifen **4** Farben breit. Ein
    /// EL0-Kernel-Stack ist dort 16 KiB = **4** Seiten und passte damit GENAU. Seit der
    /// Wachseite (2026-08-10) wird eine Seite mehr angefordert: **5** aufeinanderfolgende
    /// Seiten tragen **5 verschiedene** Farben, und die passen in einen 4-Farben-Streifen an
    /// keiner Adresse und bei keinem Fuellstand.
    ///
    /// Der Test steht bewusst mit `COLORS_ARM = 16` da und nicht mit den 256 des uebrigen
    /// Moduls: bei 256 Farben ist ein Streifen 16 lebendige Maskenbits breit, und **genau
    /// deshalb** hat x86 nie etwas gemerkt. Ein Host-Test mit der Geometrie der einen
    /// Architektur prueft die andere nicht.
    #[test]
    fn wachseite_sprengt_den_streifen_bei_16_farben() {
        const COLORS_ARM: u32 = 16;
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let m = stripe(0, 4, COLORS_ARM).unwrap();
        // **`m.len()` ist hier die falsche Groesse und meldet 16.** `stripe` wiederholt das
        // Muster mit der Periode `live` ueber alle 64 Bits (damit „Vereinigung aller Streifen
        // ist lueckenlos" gilt); gezaehlt werden muss der LEBENDIGE Farbraum, also `0..colors`.
        // Genau diese Verwechslung — Maskenbits gegen Farbanzahl — ist der `MASK_BITS`-Fehler
        // aus der Fallenliste, und sie ist mir beim Schreiben dieses Tests noch einmal passiert.
        let lebendig = (0..COLORS_ARM).filter(|c| m.contains(*c)).count();
        assert_eq!(lebendig, 4, "Streifen muss 4 der 16 Farben halten");

        // Der Stack ALLEIN passt — genau, nicht mit Luft.
        let nur_stack = a.alloc_colored(4 * PAGE, PAGE, COLORS_ARM, m);
        assert!(nur_stack.is_some(), "4 Seiten sind genau ein Streifen");
        a.free(nur_stack.unwrap());

        // Stack + Wache in EINER gefaerbten Anforderung: strukturell unerfuellbar.
        assert!(
            a.alloc_colored(5 * PAGE, PAGE, COLORS_ARM, m).is_none(),
            "5 aufeinanderfolgende Seiten tragen 5 Farben -- ein 4-Farben-Streifen fasst sie nie"
        );

        // Und die Behebung: die Wache steht unter keiner Farbbedingung.
        let cap = a
            .alloc_colored_vorspann_in(1, 5 * PAGE, PAGE, COLORS_ARM, m, 0, u64::MAX)
            .expect("mit ungebundenem Vorspann ist die Anforderung erfuellbar");
        assert_eq!(cap.len(), 5 * PAGE);
        // Die Aussage ist NICHT „irgendetwas kam zurueck", sondern: jede Seite AB der Wache
        // liegt im Streifen. Ohne dieses Konjunkt wuerde der Test auch einen Allokator
        // bestehen, der die Farbbedingung einfach fallen laesst.
        for j in 1..5 {
            let c = color_of(cap.base() + j * PAGE, COLORS_ARM);
            assert!(m.contains(c), "Stackseite {j} traegt Fremdfarbe {c}");
        }
        assert!(a.free(cap));
    }

    /// Ein Vorspann, der die ganze Anforderung verschlingt, ist keine gefaerbte Anforderung
    /// mehr — und wird abgewiesen statt die Zusicherung still fallen zu lassen.
    #[test]
    fn vorspann_ueber_alles_wird_abgewiesen() {
        let mut a = with_ram(0x4000_0000, 16 * 1024 * 1024);
        let m = stripe(0, 4, COLORS).unwrap();
        assert!(a.alloc_colored_vorspann_in(4, 4 * PAGE, PAGE, COLORS, m, 0, u64::MAX).is_none());
        assert!(a.alloc_colored_vorspann_in(9, 4 * PAGE, PAGE, COLORS, m, 0, u64::MAX).is_none());
        // Eine Seite weniger Vorspann, und es geht wieder.
        let cap = a.alloc_colored_vorspann_in(3, 4 * PAGE, PAGE, COLORS, m, 0, u64::MAX);
        assert!(cap.is_some(), "Vorspann < Seitenzahl ist eine gueltige Anforderung");
        a.free(cap.unwrap());
    }

    #[test]
    fn leere_maske_wird_abgewiesen() {
        let mut a = with_ram(0x4000_0000, 16 * 1024 * 1024);
        assert!(a.alloc_colored(PAGE, PAGE, COLORS, ColorMask::EMPTY).is_none());
    }

    /// `colors <= 1` heisst „keine Farbunterscheidung" -> Verhalten wie `alloc`.
    /// Der Aufrufer, nicht der Allokator, muss das als fehlende Eigenschaft werten.
    #[test]
    fn ohne_farben_wie_alloc() {
        let mut a = with_ram(0x4000_0000, 16 * 1024 * 1024);
        // Die Maske ist eine echte Vierteilung; die MASCHINE hat nur eine Farbe. Genau diese
        // Kombination meint `colors <= 1` -- deshalb wird die Maske fuer COLORS gebaut.
        let cap = a.alloc_colored(4 * PAGE, PAGE, 1, stripe(0, 4, COLORS).unwrap());
        assert!(cap.is_some());
    }

    /// Farbige Allokationen lecken nichts: nach dem Zurueckgeben steht die Bilanz wieder.
    #[test]
    fn farbige_allokation_ist_bilanzneutral() {
        let mut a = with_ram(0x4000_0000, 64 * 1024 * 1024);
        let free0 = a.total_free();
        let caps: std::vec::Vec<_> = (0..4)
            .map(|i| a.alloc_colored(4 * PAGE, PAGE, COLORS, stripe(i, 4, COLORS).unwrap()).unwrap())
            .collect();
        assert!(a.total_free() < free0);
        for c in caps {
            assert!(a.free(c));
        }
        assert_eq!(a.total_free(), free0, "Speicher nach Rueckgabe nicht vollstaendig zurueck");
        assert_eq!(a.fragments(), 1, "Freiliste nicht wieder koalesziert");
    }

    // -- E-Rest 3b: der Zonenwunsch ------------------------------------------------------------
    //
    // Der Speicherplan, an dem der Befund gemessen wurde: `-m 3G` auf q35 ergibt UNTEN einen
    // grossen Bereich und OBEN einen KLEINEREN. Genau diese Groessenrelation liess Best-Fit den
    // oberen waehlen. Die Zahlen sind deshalb nicht frei gewaehlt -- sie bilden den Fall ab, der
    // `dmawin`/`dmatok : FAILURES` und `iso : spawn_isolated fehlgeschlagen` erzeugt hat.
    const GIB: u64 = 1 << 30;
    /// Wie `-m 3G`: unten 2032 MiB ab 16 MiB, oben 1024 MiB ab 4 GiB.
    fn wie_3g() -> PhysAllocator {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(16 * 1024 * 1024, 2032 * 1024 * 1024));
        assert!(a.add_region(4 * GIB, GIB));
        a
    }

    /// **Die Positivkontrolle -- und sie ist der eigentliche Test.** Ohne Zonenwunsch waehlt
    /// Best-Fit den kleineren OBEREN Bereich. Steht diese Zeile nicht, sagt der Test darunter
    /// nichts: er koennte auch dann gruen sein, wenn die Zone gar nichts bewirkt.
    #[test]
    fn ohne_zone_waehlt_best_fit_den_oberen_bereich() {
        let mut a = wie_3g();
        let cap = a.alloc(2 * 1024 * 1024, 2 * 1024 * 1024).expect("Platz vorhanden");
        assert!(
            cap.base() >= 4 * GIB,
            "Voraussetzung des Befunds entfaellt: Best-Fit nahm {:#x}, nicht den oberen Bereich",
            cap.base()
        );
    }

    /// **Die Aussage:** mit Zonenwunsch kommt dieselbe Anforderung aus GiB 0.
    #[test]
    fn mit_zone_kommt_die_region_aus_gib0() {
        let mut a = wie_3g();
        let cap = a
            .alloc_below(2 * 1024 * 1024, 2 * 1024 * 1024, GIB)
            .expect("unten ist reichlich Platz");
        assert!(cap.base() + cap.len() <= GIB, "Region {:#x} liegt nicht in GiB 0", cap.base());
    }

    /// Die Zone ist eine **Schranke**, keine Vorliebe: passt nichts hinein, gibt es `None` --
    /// und nicht ersatzweise etwas darueber.
    #[test]
    fn zone_ohne_platz_liefert_none() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(4 * GIB, GIB)); // NUR hoher Speicher
        assert!(a.alloc_below(PAGE, PAGE, GIB).is_none(), "Zone wurde ueberschritten");
        assert!(a.alloc(PAGE, PAGE).is_some(), "ohne Zone muss dieselbe Anforderung gelingen");
    }

    /// Ein Bereich, der die Grenze **ueberquert**, wird an ihr beschnitten statt verworfen --
    /// sonst waere ein einzelner durchgehender Speicher fuer die Zone unbenutzbar.
    #[test]
    fn bereich_ueber_die_grenze_wird_beschnitten() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(GIB / 2, GIB)); // 512 MiB .. 1,5 GiB -- ueberquert GIB
        let cap = a.alloc_below(64 * 1024 * 1024, PAGE, GIB).expect("die untere Haelfte reicht");
        assert!(cap.base() + cap.len() <= GIB, "Grenze verletzt: {:#x}", cap.base());
        // Und was jenseits liegt, bleibt vergebbar -- die Zone verbraucht es nicht.
        assert!(a.alloc(256 * 1024 * 1024, PAGE).is_some(), "Speicher jenseits der Grenze verloren");
    }

    /// `u64::MAX` heisst „egal" und muss **bit-identisch** zum alten Weg sein. Ohne diese Zeile
    /// koennte die Zonenlogik das Verhalten aller uebrigen Aufrufer still verschieben.
    #[test]
    fn ohne_grenze_identisch_zu_alloc() {
        let mut a = wie_3g();
        let mut b = wie_3g();
        for _ in 0..8 {
            let x = a.alloc(PAGE * 3, PAGE).unwrap();
            let y = b.alloc_below(PAGE * 3, PAGE, u64::MAX).unwrap();
            assert_eq!(x.base(), y.base(), "alloc und alloc_below(u64::MAX) laufen auseinander");
        }
    }

    /// Farbe UND Zone gemeinsam: die Region traegt nur erlaubte Farben **und** liegt in GiB 0.
    /// Nacheinander entschieden wuerde eine der beiden Bedingungen die andere ueberstimmen.
    #[test]
    fn farbe_und_zone_gelten_gemeinsam() {
        let mut a = wie_3g();
        let m = stripe(2, 4, COLORS).unwrap();
        let cap = a
            .alloc_colored_below(4 * PAGE, PAGE, COLORS, m, GIB)
            .expect("unten ist Platz in jeder Farbe");
        assert!(all_pages_in(&cap, m), "Seite mit unerlaubter Farbe vergeben");
        assert!(cap.base() + cap.len() <= GIB, "Farbe erfuellt, Zone verletzt: {:#x}", cap.base());
    }

    /// **Der GiB-0-Deckel als ZAHL, nicht als Schaetzung** (E-Rest 3d, zweite Haelfte).
    ///
    /// Eine isolierte PD braucht eine private, 2-MiB-ausgerichtete Region. Solange die Abbildung
    /// **identisch** ist (VA == PA), muss sie in `[USER_RAM_MIN, GIB1_END)` liegen — also in
    /// GiB 0. Wieviele PDs das sind, stand bisher als „rund 500" in einer Notiz; hier wird es
    /// gerechnet, und zwar auf demselben Speicherplan, den `-m 6G` erzeugt.
    ///
    /// Der Test ist die **Positivkontrolle des Umbaus**: fiele die Identitaetsbindung, muesste
    /// die zweite Zahl auf ein Vielfaches steigen. Steht er hier, kann „der Deckel ist weg"
    /// nicht behauptet werden, ohne dass sich diese Zeile aendert.
    #[test]
    fn gib0_deckel_ist_eine_zahl() {
        const ISO: u64 = 2 * 1024 * 1024;
        let bau = || {
            let mut a = PhysAllocator::new();
            assert!(a.add_region(16 * 1024 * 1024, 2032 * 1024 * 1024)); // unten, wie -m 6G
            assert!(a.add_region(4 * GIB, 4 * GIB)); //                     oben
            a
        };

        // (1) An GiB 0 gebunden -- der heutige Stand.
        let mut a = bau();
        let mut gebunden = 0;
        while a.alloc_below(ISO, ISO, GIB).is_some() {
            gebunden += 1;
        }

        // (2) Ohne die Bindung -- was derselbe Speicher hergaebe.
        let mut b = bau();
        let mut frei = 0;
        while b.alloc(ISO, ISO).is_some() {
            frei += 1;
        }

        // GiB 0 abzueglich der ersten 16 MiB: (1024 - 16) / 2 = 504.
        assert_eq!(gebunden, 504, "der GiB-0-Deckel hat sich verschoben");
        // Derselbe Speicher, ohne die Bindung: rund das Sechsfache.
        assert!(
            frei >= 6 * gebunden,
            "ohne Identitaetsbindung sollten es ein Vielfaches sein, sind aber {frei} gegen {gebunden}"
        );
    }

    // -- E-Rest 3d: die Zone als INTERVALL ------------------------------------------------------

    /// **Die Untergrenze wirkt** — und die Positivkontrolle steht daneben: dieselbe Anforderung
    /// ohne Untergrenze landet unten. Ohne sie waere nicht zu unterscheiden, ob `alloc_in`
    /// wirklich waehlt oder ob zufaellig oben lag, was ohnehin gekommen waere.
    ///
    /// **Der Speicherplan ist deshalb `wie_6g` und NICHT `wie_3g`.** Bei 3G ist der obere Bereich
    /// der kleinere, Best-Fit nimmt ihn also von sich aus — die Positivkontrolle wuerde dort
    /// fehlschlagen, und zwar zu Recht (`ohne_zone_waehlt_best_fit_den_oberen_bereich` belegt
    /// genau das). Erst gebaut, prompt darauf hereingefallen: eine Positivkontrolle muss zu dem
    /// Aufbau passen, in dem sie steht.
    #[test]
    fn untergrenze_waehlt_den_oberen_bereich() {
        // Wie `-m 6G`: unten 2032 MiB, oben 4096 MiB -- hier ist der UNTERE der kleinere.
        let bau = || {
            let mut a = PhysAllocator::new();
            assert!(a.add_region(16 * 1024 * 1024, 2032 * 1024 * 1024));
            assert!(a.add_region(4 * GIB, 4 * GIB));
            a
        };
        let mut a = bau();
        let unten = a.alloc_in(PAGE * 4, PAGE, 0, u64::MAX).expect("ohne Untergrenze");
        assert!(unten.base() < 4 * GIB, "Positivkontrolle: ohne Untergrenze kam {:#x}", unten.base());

        let mut b = bau();
        let oben = b.alloc_in(PAGE * 4, PAGE, 4 * GIB, u64::MAX).expect("oben ist Platz");
        assert!(oben.base() >= 4 * GIB, "Untergrenze missachtet: {:#x}", oben.base());
    }

    /// Ein Fragment, das die **Untergrenze** ueberquert, wird an ihr beschnitten -- symmetrisch
    /// zur Obergrenze. Sonst waere ein durchgehender Speicher fuer die obere Zone unbenutzbar.
    #[test]
    fn bereich_ueber_die_untergrenze_wird_beschnitten() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(GIB / 2, GIB)); // 512 MiB .. 1,5 GiB -- ueberquert GIB
        let cap = a.alloc_in(64 * 1024 * 1024, PAGE, GIB, u64::MAX).expect("obere Haelfte reicht");
        assert!(cap.base() >= GIB, "Untergrenze verletzt: {:#x}", cap.base());
        // Und was darunter liegt, bleibt vergebbar.
        assert!(a.alloc_below(256 * 1024 * 1024, PAGE, GIB).is_some(), "unterer Teil verloren");
    }

    /// Beide Grenzen zugleich: die Region liegt im Intervall, und ausserhalb bleibt alles frei.
    #[test]
    fn intervall_haelt_beide_grenzen() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(0, 8 * GIB));
        let cap = a.alloc_in(PAGE * 8, PAGE, 2 * GIB, 3 * GIB).expect("Intervall ist gross genug");
        assert!(cap.base() >= 2 * GIB && cap.base() + cap.len() <= 3 * GIB,
                "ausserhalb des Intervalls: {:#x}", cap.base());
    }

    /// Ein leeres oder verkehrtes Intervall liefert `None` -- nicht „irgendwas".
    #[test]
    fn leeres_intervall_liefert_none() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(0, 8 * GIB));
        assert!(a.alloc_in(PAGE, PAGE, 3 * GIB, 2 * GIB).is_none(), "verkehrtes Intervall bedient");
        assert!(a.alloc_in(PAGE, PAGE, 2 * GIB, 2 * GIB).is_none(), "leeres Intervall bedient");
        assert!(a.alloc(PAGE, PAGE).is_some(), "der Allokator ist danach unbrauchbar");
    }

    /// Farbe und Intervall zugleich -- beide Bedingungen gelten, keine ueberstimmt die andere.
    #[test]
    fn farbe_und_intervall_gelten_gemeinsam() {
        let mut a = PhysAllocator::new();
        assert!(a.add_region(0, 8 * GIB));
        let m = stripe(1, 4, COLORS).unwrap();
        let cap = a
            .alloc_colored_in(4 * PAGE, PAGE, COLORS, m, 4 * GIB, 5 * GIB)
            .expect("Platz in jeder Farbe");
        assert!(all_pages_in(&cap, m), "Seite mit unerlaubter Farbe vergeben");
        assert!(cap.base() >= 4 * GIB && cap.base() + cap.len() <= 5 * GIB,
                "Farbe erfuellt, Intervall verletzt: {:#x}", cap.base());
    }

    /// `alloc_in(.., 0, u64::MAX)` ist bit-identisch zu `alloc` -- sonst haette das Intervall das
    /// Verhalten aller uebrigen Aufrufer still verschoben.
    #[test]
    fn offenes_intervall_identisch_zu_alloc() {
        let mut a = wie_3g();
        let mut b = wie_3g();
        for _ in 0..8 {
            let x = a.alloc(PAGE * 3, PAGE).unwrap();
            let y = b.alloc_in(PAGE * 3, PAGE, 0, u64::MAX).unwrap();
            assert_eq!(x.base(), y.base(), "alloc und alloc_in(0, MAX) laufen auseinander");
        }
    }

    /// Der Zonenwunsch leckt nicht: nach der Rueckgabe steht die Bilanz wieder.
    #[test]
    fn zonen_allokation_ist_bilanzneutral() {
        let mut a = wie_3g();
        let free0 = a.total_free();
        let caps: std::vec::Vec<_> =
            (0..4).map(|_| a.alloc_below(PAGE * 5, PAGE, GIB).unwrap()).collect();
        assert!(a.total_free() < free0);
        for c in caps {
            assert!(a.free(c));
        }
        assert_eq!(a.total_free(), free0, "Speicher nach Rueckgabe nicht vollstaendig zurueck");
    }
}
