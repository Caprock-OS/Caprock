//! **Seitenfarben** — die Arithmetik hinter der Cache-Partitionierung (`TODO.md` A1).
//!
//! Der Last-Level-Cache ist physisch indiziert. Die Set-Indexbits, die *oberhalb* des
//! Seitenoffsets liegen, hängen damit an der Physadresse und sind konstant für die gesamte
//! Seite — das ist ihre **Farbe**. Zwei Seiten verschiedener Farbe können einander im LLC
//! nicht verdrängen; ein Farbsatz ist deshalb ein Cache-Anteil.
//!
//! Dieses Modul ist **reine Arithmetik ohne Hardware** — genau deshalb ist es auf dem Host
//! prüfbar (`cargo test -p caprock-mem`). Die Geometrie kommt von `hal::cache`, die Politik
//! (welche PD welchen Satz bekommt) vom Kernel; hier steht nur, was eine Farbe *ist* und was
//! eine Maske bedeutet.

/// Seitengröße (Farbgranularität).
use crate::PAGE;

/// Ein Farbsatz: Bitmaske über bis zu 64 Farben.
///
/// 64 ist keine Verlegenheitsgrenze, sondern die Zahl, die ohne Allokation in ein Wort passt.
/// Reale LLCs haben mehr Farben (gemessen auf dem x86-Testaufbau: 256). Deshalb ist die Maske
/// **nicht** „Farbe i = Bit i", sondern ein Muster, das sich über den Farbraum wiederholt:
/// Farbe `c` gehört zur Maske, wenn Bit `c % 64` gesetzt ist. Bei 256 Farben und 64 Bits fasst
/// ein Maskenbit also 4 Farben zusammen — die Partitionierung wird gröber, bleibt aber
/// **disjunkt**, und das ist die Eigenschaft, auf die es ankommt.
///
/// **Die andere Richtung ist die gefährliche.** Hat die Maschine *weniger* als 64 Farben
/// (aarch64: 16), dann fragt `contains` nie ein Bit oberhalb von `colors` ab — die Bits
/// `colors..64` sind **tot**. Eine Maske, die nur dort Bits setzt, ist damit leer, *sieht* aber
/// nicht leer aus: `is_empty()` sagt `false`, `intersects` sagt „disjunkt". Deshalb darf keine
/// Rechnung, die Masken *aufteilt*, mit [`MASK_BITS`] arbeiten — sie braucht die tatsächliche
/// Farbanzahl (s. [`stripe`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorMask(pub u64);

/// Anzahl Bits in einer [`ColorMask`].
pub const MASK_BITS: u32 = 64;

impl ColorMask {
    /// Leere Maske — akzeptiert keine Farbe. Eine Allokation dagegen MUSS fehlschlagen.
    pub const EMPTY: ColorMask = ColorMask(0);
    /// Alle Farben (das Verhalten vor A1: keine Partitionierung).
    pub const ALL: ColorMask = ColorMask(u64::MAX);

    /// Enthält die Maske Farbe `c`?
    #[inline]
    pub const fn contains(self, c: u32) -> bool {
        self.0 & (1u64 << (c % MASK_BITS)) != 0
    }

    /// Ist die Maske leer?
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Überschneiden sich zwei Farbsätze? **Das ist die A1-Eigenschaft**: für zwei PDs
    /// muss das `false` sein, sonst teilen sie sich Cache-Sets.
    #[inline]
    pub const fn intersects(self, other: ColorMask) -> bool {
        self.0 & other.0 != 0
    }

    /// Anzahl gesetzter Bits.
    #[inline]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }
}

/// Farbe einer Physadresse bei `colors` Farben.
///
/// `colors` muss eine Zweierpotenz sein; andernfalls entspräche die Rechnung nicht den
/// Set-Indexbits, und die „Farbe" wäre eine Zahl ohne Bezug zur Hardware.
#[inline]
pub fn color_of(pa: u64, colors: u32) -> u32 {
    if colors <= 1 {
        return 0;
    }
    ((pa / PAGE) as u32) & (colors - 1)
}

/// **Streifen** `i` von `n` disjunkten Farbsätzen.
///
/// Die Aufteilung ist bewusst *streifenweise* (zusammenhängende Farbblöcke) und nicht
/// verschränkt: Farben eines Streifens liegen im RAM als zusammenhängende Seitenläufe
/// nebeneinander, was den Allokator zusammenhängende Blöcke schneiden lässt statt Einzelseiten
/// aus der Mitte zu stanzen (Fragmentierung). Bei verschränkter Vergabe wäre jede zweite Seite
/// ein eigenes Fragment.
///
/// `colors` ist die Farbanzahl **dieser Maschine** (Zweierpotenz, `hal::cache::page_colors()`).
///
/// **Warum das ein Parameter sein muss.** Früher rechnete diese Funktion mit [`MASK_BITS`] statt
/// mit der Farbanzahl — derselbe Fehler, den `colors::region_bytes` im Kernel schon bezahlt hat.
/// `contains` befragt Bit `c % MASK_BITS`; bei `colors < MASK_BITS` sind die Bits ab `colors`
/// deshalb **tot**, sie stehen für keine Farbe. Auf x86 (256 Farben) fiel das nicht auf, weil dort
/// alle 64 Bits lebendig sind und die Rechnung zufällig stimmte. Auf aarch64 (16 Farben) bekam
/// Streifen 0 alle 16 Farben — also den ganzen Farbraum statt eines Viertels — und die Streifen
/// 1..3 **keine einzige**. Gemessen, nicht vermutet: 16/0/0/0 statt 4/4/4/4.
///
/// Das ist die schlimmere Hälfte des Fehlers: `intersects` meldete die Sätze als disjunkt, weil
/// leere Mengen sich nicht schneiden. Der Selbsttest wäre grün gewesen, ohne dass irgendetwas
/// getrennt war — genau die leere Beobachtung, gegen die dieses Projekt an anderer Stelle den
/// `CD.R`-Fall gestellt hat.
///
/// **Warum es sich nicht ohne Parameter beheben lässt.** Ein festes 64-Bit-Muster kann nicht für
/// 16 *und* 256 Farben zugleich zusammenhängend und richtig sein: bei 16 Farben müsste Streifen `i`
/// die Farben `4i..4i+3` halten, bei 256 einen Block von 16 Bits — die beiden Forderungen legen
/// unvereinbare Muster fest. Wer den Parameter weglässt, muss die Zusammenhangs-Eigenschaft
/// aufgeben, und damit `region_bytes` im Kernel falsch machen.
///
/// `None`, wenn `n` keine Zweierpotenz ist, `i >= n`, `colors` keine Zweierpotenz ist, oder
/// **`n` grösser ist als der lebendige Farbraum** `min(colors, MASK_BITS)` — aus `colors` Farben
/// lassen sich keine `n > colors` nichtleeren disjunkten Sätze schneiden. Die Aufteilung muss
/// aufgehen, sonst wären die Sätze nicht disjunkt und die Zusicherung eine Behauptung.
pub fn stripe(i: u32, n: u32, colors: u32) -> Option<ColorMask> {
    if n == 0 || i >= n || !n.is_power_of_two() {
        return None;
    }
    if colors == 0 || !colors.is_power_of_two() {
        return None;
    }
    // Der **lebendige** Teil der Maske: so viele Bits, wie `contains` überhaupt je befragt.
    // Bei `colors >= MASK_BITS` faltet `c % MASK_BITS` den Farbraum auf alle 64 Bits (ein Bit
    // trägt dann `colors / MASK_BITS` Farben); bei `colors < MASK_BITS` bleiben nur `colors` Bits.
    let live = if colors < MASK_BITS { colors } else { MASK_BITS };
    if n > live {
        return None;
    }
    let per = live / n;
    let lo = i * per;
    // `per == MASK_BITS` (live == 64 und n == 1) würde bei `1<<64` überlaufen -> Sonderfall.
    let block = if per == MASK_BITS { u64::MAX } else { ((1u64 << per) - 1) << lo };
    // Das Muster mit der Periode `live` über alle 64 Bits wiederholen. Für `contains` ist das
    // gleichwertig zu „nur die unteren `live` Bits setzen" (es fragt `c % MASK_BITS`, und für
    // `c < colors = live` sind das genau die unteren Bits) — aber es hält die zwei Eigenschaften
    // aufrecht, an denen Aufrufer hängen: `stripe(0, 1, colors) == ColorMask::ALL` (`alloc_colored`
    // hat dafür einen schnellen Pfad) und „die Vereinigung aller `n` Streifen ist lückenlos".
    let mut bits = 0u64;
    let mut off = 0u32;
    while off < MASK_BITS {
        bits |= block << off;
        off += live;
    }
    Some(ColorMask(bits))
}

/// **Niedrigster freier Streifen** unter `n`, oder `None`, wenn alle vergeben sind (B-4.2).
///
/// `taken` ist eine Bitmenge: Bit `i` gesetzt heißt Streifen `i` ist vergeben. Die Funktion ist
/// bewusst rein und liegt hier statt im Kernel — dieselbe Technik wie bei `cache_decode`: die
/// Arithmetik gegen eingespeiste Werte prüfen, nicht gegen den Zustand einer laufenden Maschine.
///
/// **Der Rückgabewert `None` ist der eigentliche Zweck.** Vorher vergab der Kernel Farbsätze mit
/// `i % n` — die (n+1)-te PD bekam wieder den Satz der ersten, und niemand merkte es. Eine
/// erschöpfte Partitionierung muss ein *Fehlschlag* sein, keine stille Überschneidung.
pub const fn pick_free(taken: u32, n: u32) -> Option<u32> {
    if n == 0 || n > 32 {
        return None;
    }
    let mut i = 0;
    while i < n {
        if taken & (1u32 << i) == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leere Belegung -> Streifen 0; danach der jeweils niedrigste freie.
    #[test]
    fn pick_free_nimmt_den_niedrigsten_freien() {
        assert_eq!(pick_free(0b0000, 4), Some(0));
        assert_eq!(pick_free(0b0001, 4), Some(1));
        assert_eq!(pick_free(0b0011, 4), Some(2));
        assert_eq!(pick_free(0b0111, 4), Some(3));
        // Luecke in der Mitte: freigegebene Streifen werden wiederverwendet, nicht uebersprungen.
        assert_eq!(pick_free(0b1101, 4), Some(1));
    }

    /// **Der Punkt der Uebung:** ist alles vergeben, gibt es KEINEN Ersatz.
    #[test]
    fn pick_free_erschoepft_ist_ein_fehlschlag_keine_wiederholung() {
        assert_eq!(pick_free(0b1111, 4), None);
        // Und nicht etwa Streifen 0 erneut -- genau das war der Fehler von `i % n`.
        assert_ne!(pick_free(0b1111, 4), Some(0));
    }

    /// Bits oberhalb von `n` gehen die Auswahl nichts an.
    #[test]
    fn pick_free_ignoriert_bits_jenseits_von_n() {
        assert_eq!(pick_free(0b1111_0000, 4), Some(0));
        assert_eq!(pick_free(0b1111_0001, 4), Some(1));
    }

    /// Unsinnige Streifenzahlen ergeben keinen Streifen (statt Panik oder Ueberlauf).
    #[test]
    fn pick_free_weist_unsinnige_streifenzahl_ab() {
        assert_eq!(pick_free(0, 0), None);
        assert_eq!(pick_free(0, 33), None);
        // Genau 32 ist die Grenze und muss noch gehen.
        assert_eq!(pick_free(0, 32), Some(0));
        assert_eq!(pick_free(u32::MAX, 32), None);
    }

    /// Zusammenspiel mit `stripe`: jeder von `pick_free` gelieferte Index ergibt einen gueltigen
    /// Satz, zwei verschiedene ueberlappen nicht, und nach `n` Vergaben ist Schluss.
    ///
    /// Das ist die Aussage, auf der B-4.2 steht -- „sauberer Fehlschlag statt stiller
    /// Ueberschneidung" ist genau die Konjunktion dieser beiden Haelften.
    #[test]
    fn gelieferte_indizes_ergeben_disjunkte_saetze() {
        let mut taken = 0u32;
        let mut masks = [0u64; 4];
        let mut used = 0usize;
        while let Some(i) = pick_free(taken, 4) {
            let bits = stripe(i, 4, 64).expect("Aufteilung geht auf").0;
            assert_ne!(bits, 0, "ein leerer Satz waere keine Trennung");
            for m in masks.iter().take(used) {
                assert_eq!(bits & m, 0, "Streifen {i} ueberlappt einen frueheren");
            }
            masks[used] = bits;
            used += 1;
            taken |= 1 << i;
        }
        assert_eq!(used, 4, "genau vier disjunkte Saetze, dann Schluss");
        assert_eq!(pick_free(taken, 4), None, "erschoepft heisst erschoepft");
        // Gegenprobe: die vier Saetze zusammen decken den ganzen Farbraum ab, keiner faellt weg.
        assert_eq!(masks.iter().fold(0u64, |a, m| a | m), u64::MAX);
    }

    /// Farbe ist seitenkonstant und läuft mit der Seitennummer um.
    #[test]
    fn farbe_ist_seitenkonstant_und_zyklisch() {
        let colors = 256;
        assert_eq!(color_of(0, colors), 0);
        assert_eq!(color_of(PAGE - 1, colors), 0, "innerhalb der Seite konstant");
        assert_eq!(color_of(PAGE, colors), 1);
        assert_eq!(color_of(PAGE * 255, colors), 255);
        assert_eq!(color_of(PAGE * 256, colors), 0, "Umlauf nach `colors` Seiten");
    }

    /// Ohne Farbunterscheidung ist alles Farbe 0 — und das darf nicht wie Erfolg aussehen.
    #[test]
    fn eine_farbe_heisst_keine_farbe() {
        assert_eq!(color_of(PAGE * 7, 1), 0);
        assert_eq!(color_of(PAGE * 8, 1), 0);
        // Ein einziger Streifen ist der ganze Farbraum — und zwar bei JEDER Farbanzahl, damit
        // der schnelle Pfad `mask == ColorMask::ALL` in `alloc_colored` nicht arch-abhaengig wird.
        for colors in [1u32, 16, 64, 256] {
            assert_eq!(stripe(0, 1, colors).unwrap(), ColorMask::ALL, "colors={colors}");
        }
    }

    /// **Die A1-Eigenschaft**: n Streifen sind paarweise disjunkt und decken zusammen alles ab.
    #[test]
    fn streifen_sind_paarweise_disjunkt_und_vollstaendig() {
        for n in [1u32, 2, 4, 8, 16, 32, 64] {
            let masks: alloc_vec::Vec = (0..n).map(|i| stripe(i, n, 64).unwrap()).collect();
            let mut union = 0u64;
            for (a, ma) in masks.iter().enumerate() {
                assert!(!ma.is_empty(), "n={n}: Streifen {a} ist leer");
                for (b, mb) in masks.iter().enumerate() {
                    if a != b {
                        assert!(!ma.intersects(*mb), "n={n}: Streifen {a} und {b} ueberlappen");
                    }
                }
                union |= ma.0;
            }
            assert_eq!(union, u64::MAX, "n={n}: Streifen decken nicht alle Farben ab");
        }
    }

    /// **Die eigentliche A1-Eigenschaft, gegen ECHTE Farben statt gegen Maskenbits.**
    ///
    /// Jede der `colors` Farben liegt in genau einem Streifen, und jeder Streifen hält gleich
    /// viele. Die alte Rechnung mit `MASK_BITS` lieferte bei 16 Farben 16/0/0/0 statt 4/4/4/4 —
    /// also den ganzen Farbraum für Streifen 0 und leere Sätze für den Rest.
    ///
    /// **16 UND 256 stehen hier beide mit Absicht.** Bei 256 war die alte Rechnung zufällig
    /// richtig; ein Test, der nur 256 prüft, ist grün und belegt nichts. Erst das Paar trennt
    /// „stimmt" von „stimmte hier gerade".
    #[test]
    fn jede_farbe_in_genau_einem_streifen_bei_jeder_farbanzahl() {
        for colors in [2u32, 4, 8, 16, 32, 64, 128, 256, 1024] {
            for n in [1u32, 2, 4, 8, 16] {
                if n > colors {
                    continue;
                }
                let masks: std::vec::Vec<ColorMask> =
                    (0..n).map(|i| stripe(i, n, colors).expect("Aufteilung geht auf")).collect();
                for c in 0..colors {
                    let treffer = masks.iter().filter(|m| m.contains(c)).count();
                    assert_eq!(treffer, 1, "colors={colors}, n={n}: Farbe {c} in {treffer} Streifen");
                }
                for (i, m) in masks.iter().enumerate() {
                    let anteil = (0..colors).filter(|&c| m.contains(c)).count() as u32;
                    assert_eq!(
                        anteil,
                        colors / n,
                        "colors={colors}, n={n}: Streifen {i} haelt {anteil} statt {} Farben",
                        colors / n
                    );
                }
            }
        }
    }

    /// **Ein Streifen ohne Farbe ist kein Streifen.** Getrennt geführt, weil `is_empty()` und
    /// `intersects` diesen Fall NICHT sehen: leere Mengen sind bitweise ungleich null und
    /// schneiden sich nicht — sie sehen aus wie eine perfekte Trennung.
    #[test]
    fn kein_streifen_ist_ueber_den_echten_farben_leer() {
        for colors in [2u32, 16, 256] {
            for n in [2u32, 4] {
                if n > colors {
                    continue; // aus 2 Farben gibt es keine 4 Streifen -- das prueft ein eigener Test
                }
                for i in 0..n {
                    let m = stripe(i, n, colors).unwrap();
                    assert!(
                        (0..colors).any(|c| m.contains(c)),
                        "colors={colors}, n={n}: Streifen {i} traegt keine einzige Farbe"
                    );
                }
            }
        }
    }

    /// **Zusammenhang** — die Eigenschaft, auf der `colors::region_bytes` im Kernel steht:
    /// ein Lauf aufeinanderfolgender Seiten bleibt `min(colors, MASK_BITS) / n` Seiten lang
    /// im selben Streifen. Ohne sie schnitte der Allokator Einzelseiten statt Blöcke.
    ///
    /// Bei 16 Farben und 4 Streifen sind das 4 Seiten; die alte Rechnung gab Streifen 1..3
    /// einen Lauf der Länge **0**.
    #[test]
    fn streifen_bleibt_ueber_zusammenhaengende_seiten_erhalten() {
        for colors in [16u32, 64, 256] {
            let n = 4u32;
            let erwartet = colors.min(MASK_BITS) / n;
            for i in 0..n {
                let m = stripe(i, n, colors).unwrap();
                // Längster Lauf aufeinanderfolgender Farben, die alle im Streifen liegen.
                let (mut lauf, mut best) = (0u32, 0u32);
                for c in 0..colors {
                    lauf = if m.contains(c) { lauf + 1 } else { 0 };
                    best = best.max(lauf);
                }
                assert!(
                    best >= erwartet,
                    "colors={colors}: Streifen {i} haelt nur {best} zusammenhaengende Seiten, \
                     region_bytes() verspricht {erwartet}"
                );
            }
        }
    }

    /// Aus 16 Farben lassen sich keine 32 nichtleeren disjunkten Sätze schneiden — das muss
    /// ein **Fehlschlag** sein. Die alte Rechnung gab hier 32-mal `Some` zurück, davon 30
    /// Sätze ohne eine einzige Farbe.
    #[test]
    fn mehr_streifen_als_farben_wird_abgewiesen() {
        assert!(stripe(0, 32, 16).is_none(), "32 Streifen aus 16 Farben");
        assert!(stripe(0, 2, 1).is_none(), "2 Streifen aus 1 Farbe");
        // Genau so viele Streifen wie Farben ist die Grenze und muss noch gehen.
        assert!(stripe(15, 16, 16).is_some(), "16 Streifen aus 16 Farben");
        assert!(stripe(0, 64, 256).is_some(), "64 Streifen passen in die Maske");
        assert!(stripe(0, 128, 256).is_none(), "mehr Streifen als Maskenbits");
    }

    /// Eine Aufteilung, die nicht aufgeht, wird abgewiesen statt stillschweigend gerundet.
    #[test]
    fn ungueltige_aufteilung_wird_abgewiesen() {
        assert!(stripe(0, 0, 64).is_none(), "n=0");
        assert!(stripe(2, 2, 64).is_none(), "i >= n");
        assert!(stripe(0, 3, 64).is_none(), "keine Zweierpotenz");
        assert!(stripe(0, 128, 64).is_none(), "mehr Streifen als Maskenbits");
        // Eine Farbanzahl, die keine Zweierpotenz ist, passt nicht zu `color_of` (`& (colors-1)`).
        assert!(stripe(0, 2, 24).is_none(), "colors keine Zweierpotenz");
        assert!(stripe(0, 1, 0).is_none(), "colors=0");
    }

    /// Die Maske wiederholt sich über den Farbraum — bei 256 Farben trägt ein Bit 4 Farben,
    /// und zwei disjunkte Streifen bleiben auch dann disjunkt.
    #[test]
    fn maske_wiederholt_sich_ueber_den_farbraum() {
        let colors = 256;
        let (a, b) = (stripe(0, 2, colors).unwrap(), stripe(1, 2, colors).unwrap());
        for page in 0..(colors as u64 * 2) {
            let c = color_of(page * PAGE, colors);
            assert!(a.contains(c) ^ b.contains(c), "Farbe {c} in beiden oder keinem Streifen");
        }
    }

    /// Winziger Vec-Ersatz: die Crate ist `no_std`, die Tests laufen aber auf dem Host.
    mod alloc_vec {
        pub type Vec = std::vec::Vec<super::super::ColorMask>;
    }
}
