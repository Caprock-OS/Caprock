//! **Seitenfarben / Cache-Partitionierung** (todo A1) — arch-neutral.
//!
//! Der Last-Level-Cache ist physisch indiziert: welche Cache-Sets eine Seite belegen kann,
//! entscheidet ihre **Physadresse**. Die Bits des Set-Index, die oberhalb des Seitenoffsets
//! liegen, sind die *Farbe* der Seite. Zwei Seiten verschiedener Farbe können einander im LLC
//! nicht verdrängen — und genau das ist der Hebel gegen den Zeitseitenkanal zwischen zwei PDs
//! (`TODO.md` A1): bekommt jede PD einen disjunkten Farbsatz, beobachtet keine mehr die
//! Verdrängungen der anderen.
//!
//! ## Was hier gemessen und was entschieden wird
//!
//! Gemessen wird nur die Geometrie (`hal::cache`). Entschieden wird hier: wie viele Farben es
//! gibt, wie sie auf PDs verteilt werden, und **ob die Partitionierung überhaupt trägt**.
//!
//! Der letzte Punkt ist der wichtige. `hal::cache::page_colors()` liefert `1`, wenn die
//! Plattform keine Cache-Geometrie meldet — „eine Farbe" heißt *keine* Partitionierung.
//! Ein Prüfer, der aus `1` ableitete „alle PDs haben disjunkte Farbsätze", würde eine
//! Abwesenheit als Erfüllung lesen. [`usable`] trennt das deshalb explizit: unter zwei Farben
//! ist die Eigenschaft **nicht vorhanden**, nicht etwa trivial erfüllt.

use caprock_hal::{self as hal};
#[cfg(any(feature = "selftest", feature = "debug-log"))]
use caprock_hal::println;
use caprock_mem::{color_of, ColorMask};

/// Seitengröße, gegen die Farben gerechnet werden.
pub const PAGE: u64 = 4096;

// `color_of` kommt aus `caprock-mem` — **dieselbe** Funktion, die auch der Allokator benutzt.
// Eine zweite Fassung hier wäre der klassische Fehler: der Test rechnete die Farbe anders als
// die Zuteilung und bestätigte am Ende nur seine eigene Arithmetik.

/// Anzahl Farben dieser Maschine (Zweierpotenz, mindestens 1).
pub fn count() -> u32 {
    hal::cache::page_colors()
}

/// Trägt die Farbpartitionierung auf dieser Maschine überhaupt?
///
/// **Nur** ab zwei Farben. Unter zwei Farben gibt es nichts zu trennen, und jede Aussage der
/// Form „die Farbsätze zweier PDs sind disjunkt" wäre dann strukturell wahr, ohne geprüft zu
/// haben — genau die leere Beobachtung, die dieses Projekt an anderer Stelle schon einmal
/// teuer bezahlt hat (leere SMMU-Event-Queue).
pub fn usable() -> bool {
    count() >= 2
}

/// In wie viele disjunkte Farbsätze der Farbraum geteilt wird.
///
/// Zweierpotenz und höchstens [`caprock_mem::MASK_BITS`]. Der Wert ist eine **Politik**, keine
/// Hardwaregröße: mehr Partitionen heißt bessere Trennung und weniger Cache je PD — und, weil
/// ein zusammenhängender Seitenlauf nur so lange in einem Streifen bleibt wie der Streifen
/// breit ist, auch eine kleinere größtmögliche zusammenhängende Region (s. [`region_bytes`]).
pub const PARTITIONS: u32 = 4;

/// Farbsatz des Streifens mit der Nummer `i` (rundläufig über [`PARTITIONS`]).
///
/// `None`, wenn die Aufteilung nicht aufgeht — dann gibt es keinen disjunkten Satz, und der
/// Aufrufer darf **nicht** ersatzweise „alle Farben" nehmen: das wäre die Zusicherung ohne
/// die Eigenschaft.
///
/// **Diese Funktion führt KEINE Belegung** — `i % PARTITIONS` heißt: die fünfte Nummer bekommt
/// wieder den Satz der ersten. Für einen Selbsttest, der zwei feste Nummern vergleicht, ist das
/// richtig; für die Zuteilung an PDs ist es die stille Farbüberschneidung aus B-4.2. Wer eine PD
/// bedient, nimmt [`claim_stripe`].
pub fn mask_for(i: u32) -> Option<ColorMask> {
    caprock_mem::stripe(i % PARTITIONS, PARTITIONS, count())
}

// --- Streifenvergabe mit Belegung (B-4.2) ---------------------------------------------------
//
// Bis hierher wurden Farbsätze rundläufig vergeben. Solange der gefärbte Pfad die Ausnahme war
// (nur der Selbsttest mit zwei festen Nummern), fiel das nicht auf. Als Normalfall — und dorthin
// soll er, B-4.1 — bedeutet es: ab der fünften gleichzeitigen PD teilen sich zwei PDs ihre
// Farben, **ohne dass es jemand merkt**. Das wäre schlimmer als der heutige Zustand: heute ist
// der reguläre Pfad ungefärbt und verspricht nichts; dann wäre er gefärbt und bräche das
// Versprechen still.
//
// Deshalb eine geführte Belegung, und der Fehlschlag ist **sauber**: ist kein Streifen frei,
// gibt es `None` und die PD entsteht gar nicht erst. Kein Ersatzsatz, keine Aufweichung, keine
// „alle Farben"-Rückfallebene — eine Zusicherung, die unter Last leise schwächer wird, ist
// keine.

/// Bit `i` gesetzt = Streifen `i` ist vergeben. Höchstens [`PARTITIONS`] Bits in Gebrauch.
static STRIPES_TAKEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

// Die Auswahl selbst (`caprock_mem::pick_free`) liegt bei `stripe`, nicht hier — aus demselben
// Grund wie `color_of` oben: eine zweite Fassung derselben Arithmetik im Kernel würde am Ende nur
// sich selbst bestätigen. Dort ist sie ohne Hardware host-getestet (Technik wie `cache_decode`).

/// Einen freien Farbstreifen belegen. Gibt `(Streifennummer, Farbsatz)`.
///
/// `None` heißt **kein freier Streifen** (oder die Aufteilung geht nicht auf) — und dann darf der
/// Aufrufer die PD nicht ungefärbt anlegen und so tun, als sei sie getrennt. Freigabe über
/// [`release_stripe`], gebunden an die Lebensdauer der VSpace (`vspace_teardown`).
pub fn claim_stripe() -> Option<(u32, ColorMask)> {
    use core::sync::atomic::Ordering;
    loop {
        let cur = STRIPES_TAKEN.load(Ordering::Acquire);
        let i = caprock_mem::pick_free(cur, PARTITIONS)?;
        // Der Farbsatz muss VOR dem Belegen feststehen: geht die Aufteilung nicht auf, wäre ein
        // belegter Streifen ohne Maske ein Leck, das niemand je freigibt.
        let mask = caprock_mem::stripe(i, PARTITIONS, count())?;
        if STRIPES_TAKEN
            .compare_exchange_weak(cur, cur | (1u32 << i), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some((i, mask));
        }
        // Verloren: ein anderer Kern war schneller. Neu lesen, nicht blind weiterzählen.
    }
}

/// Einen Streifen wieder freigeben. Doppelte Freigabe ist wirkungslos, nicht schädlich.
pub fn release_stripe(i: u32) {
    if i < PARTITIONS {
        STRIPES_TAKEN.fetch_and(!(1u32 << i), core::sync::atomic::Ordering::AcqRel);
    }
}

/// Wie viele Streifen sind gerade vergeben? (Telemetrie/Selbsttest.)
pub fn stripes_in_use() -> u32 {
    STRIPES_TAKEN
        .load(core::sync::atomic::Ordering::Acquire)
        .count_ones()
}

/// **B-4.2 auf der laufenden Maschine belegen:** die Streifen erschöpfen und prüfen, dass der
/// Versuch danach *scheitert*, statt einen Satz ein zweites Mal auszugeben.
///
/// Die Arithmetik dahinter ist host-getestet (`caprock_mem::pick_free`); hier geht es um die
/// Zustandsführung im Kernel — dass die Belegung wirklich atomar geführt und die Freigabe wirklich
/// wirksam ist. Der Test stellt den Ausgangszustand danach wieder her; er läuft vor dem Anlegen
/// gefärbter PDs, belegt also nichts, was jemandem gehört.
#[cfg(feature = "selftest")]
pub fn run_stripe_alloc() -> bool {
    // **Unter zwei Farben gibt es nichts zu vergeben.** Seit `stripe` die Farbanzahl kennt, liefert
    // es dann korrekt `None` — und ohne diese Abfrage laese der Test das als „nur 0 von 4 Streifen
    // vergebbar" und meldete FAILURES. Das waere ein Fehlschlag ueber die MASCHINE, nicht ueber die
    // Belegungsfuehrung, die hier geprueft wird. Dieselbe Trennung wie in [`run_color`]: nicht
    // durchfuehrbar ist nicht durchgefallen.
    if !usable() {
        println!(
            "stripe  : SKIP -- {} Farbe(n) gemessen, unter 2 gibt es keine disjunkten Farbsaetze \
             zu vergeben (die Belegungsfuehrung ist damit nicht pruefbar, nicht etwa erfuellt)",
            count()
        );
        return true;
    }
    let vorher = stripes_in_use();
    if vorher != 0 {
        // Nicht „durchgefallen", sondern nicht durchführbar: es hält schon jemand Streifen.
        println!("stripe  : SKIP ({vorher} Streifen bereits vergeben -- Test braucht den Ruhezustand)");
        return true;
    }
    let mut held: [Option<u32>; PARTITIONS as usize] = [None; PARTITIONS as usize];
    let mut n = 0usize;
    while n < PARTITIONS as usize {
        match claim_stripe() {
            Some((i, m)) => {
                // Jeder Satz muss nichtleer sein und darf keinen früheren überlappen.
                for prev in held.iter().take(n).flatten() {
                    if let Some(pm) = caprock_mem::stripe(*prev, PARTITIONS, count()) {
                        if pm.0 & m.0 != 0 {
                            println!("stripe  : FAILURES (Streifen {i} ueberlappt {prev})");
                            return false;
                        }
                    }
                }
                held[n] = Some(i);
                n += 1;
            }
            None => {
                println!("stripe  : FAILURES (nur {n} von {PARTITIONS} Streifen vergebbar)");
                for h in held.iter().flatten() {
                    release_stripe(*h);
                }
                return false;
            }
        }
    }
    // **Der eigentliche Punkt:** jetzt ist alles vergeben, und der nächste Versuch muss scheitern.
    let erschoepft = claim_stripe().is_none();
    if !erschoepft {
        println!("stripe  : FAILURES (der {}. Versuch bekam einen Satz -- stille Ueberschneidung)", PARTITIONS + 1);
    }
    for h in held.iter().flatten() {
        release_stripe(*h);
    }
    // Und nach der Freigabe muss wieder etwas gehen, sonst leckt die Belegung.
    let wieder_frei = stripes_in_use() == 0 && claim_stripe().is_some();
    if wieder_frei {
        release_stripe(0);
    }
    let ok = erschoepft && wieder_frei && stripes_in_use() == 0;
    println!(
        "stripe  : {} Streifen vergeben, {}. Versuch abgewiesen: {}; nach Freigabe wieder vergebbar: {}",
        PARTITIONS,
        PARTITIONS + 1,
        erschoepft,
        wieder_frei
    );
    println!(
        "stripe  : {} (B-4.2: erschoepfte Farbpartitionierung scheitert SAUBER, statt einen Satz still ein zweites Mal auszugeben)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

/// Größte **zusammenhängende** Region, die vollständig in einen Streifen passt.
///
/// Aufeinanderfolgende Seiten tragen aufeinanderfolgende Farben; ein Lauf verlässt den Streifen
/// nach `MASK_BITS / PARTITIONS` Seiten. **Das ist die harte Grenze des Verfahrens** und der
/// Grund, warum die 2-MiB-Region von [`crate::system::spawn_isolated`] nicht gefärbt werden
/// kann: 512 Seiten überstreichen jeden Streifen mehrfach, also alle Farben.
pub fn region_bytes() -> u64 {
    // **Die kleinere der beiden Schranken.** `MASK_BITS` begrenzt, wie viele Farben eine Maske
    // ueberhaupt fassen kann; `count()` sagt, wie viele es auf DIESER Maschine gibt. Frueher stand
    // hier nur `MASK_BITS`, und das war auf x86 zufaellig richtig (256 Farben > 64 Maskenbits).
    //
    // Auf aarch64 mit 16 Farben war es falsch: ein Streifen umfasst dort 16/4 = 4 Farben, eine
    // 64-KiB-Region aber 16 aufeinanderfolgende Seiten -- also jede Farbe, viermal. `alloc_colored`
    // fand folglich nie einen passenden Lauf, und der gesamte Farbtest fiel durch.
    //
    // Aufgefallen ist das erst, als der Test arch-neutral wurde (A1): auf einem Zweig gemessen,
    // auf dem anderen nie ausgefuehrt -- genau die Fehlerform, gegen die der Umzug gemacht wurde.
    let farben = count().min(caprock_mem::MASK_BITS);
    (farben / PARTITIONS).max(1) as u64 * PAGE
}

/// Ergebnis des Farb-Selbsttests.
#[cfg(feature = "selftest")]
#[derive(Default)]
pub struct ColorTest {
    pub ok: bool,
    /// Gemessene Farbanzahl der Maschine.
    pub colors: u32,
    /// **Trägt die Eigenschaft überhaupt?** Unter zwei Farben gibt es nichts zu trennen; alles
    /// Weitere wäre dann strukturell wahr, ohne geprüft zu sein. Ist das `false`, ist der Test
    /// *nicht durchgefallen*, sondern **nicht durchführbar** — und muss so gemeldet werden.
    pub usable: bool,
    /// Beide PDs bekamen überhaupt eine Region.
    pub spawned: bool,
    /// Physadressen der beiden privaten Regionen (E-Rest 3d): oberhalb 4 GiB heisst, dass die
    /// Identitaetsbindung wirklich gefallen ist.
    pub region_pa: [u64; 2],
    /// Jede Seite jeder PD-Region liegt im Farbsatz ihrer PD.
    pub in_mask: bool,
    /// Auch Kernel-Stack und die obersten Seitentabellen der PD liegen in ihrem Farbsatz.
    /// Getrennt gefuehrt, weil es eine eigene Zusicherung ist: der Kernel arbeitet dort **im
    /// Namen** des Subjekts, und ein Seitenlauf der MMU hinterlaesst dieselben Spuren.
    pub kernel_side_in_mask: bool,
    /// Aufschlüsselung von [`Self::kernel_side_in_mask`]: Kernel-Stack, oberste und zweitoberste
    /// Tabelle — jeweils über **beide** PDs. Ein einzelnes `false` sagt, *welche* der drei
    /// Zusicherungen gebrochen ist; ohne das ist der Sammelwert nur ein Alarm ohne Adresse.
    pub ks_in_mask: bool,
    pub l1_in_mask: bool,
    pub l2_in_mask: bool,
    /// **Konnte die Kernel-Seite überhaupt zurückgelesen werden?** Getrennt von
    /// [`Self::kernel_side_in_mask`], weil beides ganz verschiedene Befunde sind: eine gebrochene
    /// Färbung ist ein Isolationsfehler, eine `0` aus dem Rückkanal ein Fehler des Tests. Vorher
    /// fielen beide in dasselbe Bit (`l1 != 0 && in_mask(l1)`) — ein `kernelseite=0` liess sich
    /// dann nicht deuten, ohne zu raten. Dieselbe Trennung wie bei `audit_cdt` (Code 8:
    /// „konnte nicht laufen" statt still „konsistent").
    pub kernel_side_readable: bool,
    pub ks_read: bool,
    pub l1_read: bool,
    pub l2_read: bool,
    /// **Die A1-Eigenschaft**: die Farbmengen beider PDs sind disjunkt.
    pub disjoint: bool,
    /// Eine Region jenseits der Streifenbreite wird abgewiesen statt fremde Farben mitzunehmen.
    pub oversize_refused: bool,
    /// Nach dem Abbau steht die Speicherbilanz wieder.
    pub balanced: bool,
}

/// Belegt eine Region ausschließlich Farben aus `mask`?
#[cfg(feature = "selftest")]
fn region_in_mask(base: u64, len: u64, colors: u32, mask: ColorMask) -> bool {
    let mut p = base;
    while p < base + len {
        if !mask.contains(color_of(p, colors)) {
            return false;
        }
        p += PAGE;
    }
    true
}

/// Überschneiden sich die tatsächlich belegten Farbmengen zweier Regionen?
///
/// Bewusst über die **belegten** Farben und nicht über die Masken: dass zwei Masken disjunkt
/// sind, hat der Host-Test der Crate bereits gezeigt. Hier ist die Frage, ob der Allokator sich
/// auch daran gehalten hat.
#[cfg(feature = "selftest")]
fn regions_share_color(a: (u64, u64), b: (u64, u64), colors: u32) -> bool {
    let mut p = a.0;
    while p < a.0 + a.1 {
        let ca = color_of(p, colors);
        let mut q = b.0;
        while q < b.0 + b.1 {
            if ca == color_of(q, colors) {
                return true;
            }
            q += PAGE;
        }
        p += PAGE;
    }
    false
}

/// **Farb-Selbsttest** (todo A1): zwei isolierte PDs mit disjunkten Farbsätzen erzeugen und
/// nachweisen, dass sie sich keine Cache-Farbe teilen.
///
/// `entry` ist der Einsprungpunkt für die beiden Test-PDs (arch-spezifisch, kommt vom
/// Hochlaufweg). Die PDs werden unmittelbar wieder abgebaut; geprüft wird die Zuteilung, nicht
/// ihr Programm.
#[cfg(feature = "selftest")]
pub fn run_color(entry: usize, prio: u8) -> ColorTest {
    let mut r = ColorTest { colors: count(), ..Default::default() };
    r.usable = usable();
    if !r.usable {
        return r; // nicht durchgefallen — nicht durchfuehrbar. Siehe Feld-Doku.
    }
    let (Some(m0), Some(m1)) = (mask_for(0), mask_for(1)) else {
        return r;
    };

    let sz = region_bytes();

    // Eine Region jenseits der Streifenbreite MUSS abgewiesen werden. Zuerst, damit ein
    // Fehlschlag hier nicht von den beiden PDs verdeckt wird.
    r.oversize_refused = crate::system::alloc_colored(sz * 2, PAGE, m0).is_none();

    let a = crate::system::spawn_isolated_colored(entry, 0, prio, m0);
    let b = crate::system::spawn_isolated_colored(entry, 1, prio, m1);
    if let (Some((ta, ba, ksa)), Some((tb, bb, ksb))) = (a, b) {
        r.spawned = true;

        // Die Kernelseite wird **nicht mehr zurueckgelesen**, sondern vom Spawn geliefert.
        //
        // Warum das noetig war: Der Einsprungpunkt dieser PDs faultet ABSICHTLICH. Wird der
        // Thread eingesammelt, geben `kstack_of()`/`vspace_tables_of()` 0 zurueck -- alle drei
        // zugleich, weil sie an derselben Belegung haengen. Genau das zeigten am 2026-08-01
        // mehrere Laeufe: `rueckgelesen=0 (kstack=0 l1=0 l2=0)` bei sonst fehlerfreier Zuteilung,
        // und im Protokoll steht die Ursache woertlich in der Zeile davor:
        // `el0-trap: User-Thread 0x8 faultete ... -> beendet, Kernel laeuft weiter`.
        //
        // Zwei Anlaeufe halfen NICHT, und beide sind lehrreich:
        //  * frueher lesen (4/400 -> 3/500): das Fenster beginnt, sobald `spawn` zurueckkehrt --
        //    frueher als "sofort danach" geht nicht.
        //  * die Buchfuehrung im Kernel unter den SCHEDS-Lock ziehen (-> 5/500): das war ein
        //    ECHTER Fehler (ein Kernel-Stack-Leck, s. `system::record_user_kstack`) und ist
        //    behoben -- aber es war nicht DIESER.
        //
        // Ein Wert, der von der Lebendigkeit eines Threads abhaengt, der sterben darf, ist als
        // Messgroesse untauglich, egal wie schnell man liest. Deshalb kommt er jetzt von dort,
        // wo er stabil ist: aus dem Spawn selbst, vor der Existenz des Threads.
        let kernelseite: [(u64, u64, u64); 2] = [ksa, ksb];

        // **E-Rest 3d, der Zahlungseingang:** wo liegen die beiden Regionen physisch? Seit dem
        // Fenster-Umbau duerfen sie oberhalb 4 GiB liegen -- vorher war das strukturell
        // unmoeglich (identische Abbildung, per-PD-Tabelle nur fuer GiB 0). Die Zahl steht im
        // Bericht, damit „der Deckel ist weg" eine Beobachtung ist und keine Behauptung.
        r.region_pa = [ba, bb];
        r.in_mask = region_in_mask(ba, sz, r.colors, m0) && region_in_mask(bb, sz, r.colors, m1);
        r.disjoint = !regions_share_color((ba, sz), (bb, sz), r.colors);
        // Kernel-Seite: Stack (16 KiB) und die beiden obersten Tabellen (je 4 KiB) jeder PD.
        r.ks_in_mask = true;
        r.l1_in_mask = true;
        r.l2_in_mask = true;
        r.ks_read = true;
        r.l1_read = true;
        r.l2_read = true;
        for (i, &(_t, m)) in [(ta, m0), (tb, m1)].iter().enumerate() {
            let (ks, l1, l2) = kernelseite[i];
            // Lesbarkeit und Färbung getrennt: eine `0` heisst „nicht zurueckgelesen", nicht
            // „falsch gefaerbt". Beides faellt weiterhin durch (`ok` fordert beide Sammelwerte),
            // aber die Meldung sagt jetzt, welcher der beiden Faelle vorliegt.
            r.ks_read &= ks != 0;
            r.l1_read &= l1 != 0;
            r.l2_read &= l2 != 0;
            r.ks_in_mask &=
                ks == 0 || region_in_mask(ks, crate::system::USER_KSTACK_SIZE as u64, r.colors, m);
            r.l1_in_mask &= l1 == 0 || region_in_mask(l1, PAGE, r.colors, m);
            r.l2_in_mask &= l2 == 0 || region_in_mask(l2, PAGE, r.colors, m);
        }
        r.kernel_side_readable = r.ks_read && r.l1_read && r.l2_read;
        r.kernel_side_in_mask = r.ks_in_mask && r.l1_in_mask && r.l2_in_mask;
        crate::system::destroy_isolated(ta);
        crate::system::destroy_isolated(tb);
        // Leckprüfung an GENAU den beiden Regionen, nicht an der globalen Summe. Der
        // Summenvergleich war hier nachweislich unbrauchbar: nebenher sammelt ein anderer Kern
        // den Stack des gerade gefaulteten `iso_probe`-Threads ein, und je nachdem, ob dieser
        // Rückgang ins Messfenster fällt, wurde derselbe Kernel mal grün und mal rot gemeldet
        // (auf dem x86-Testaufbau beobachtet). Ein Test, der aus fremdem Grund fehlschlägt, ist
        // so wenig wert wie einer, der nicht fehlschlagen kann.
        r.balanced =
            crate::system::region_fully_free(ba, sz) && crate::system::region_fully_free(bb, sz);
    }
    r.ok = r.usable
        && r.spawned
        && r.in_mask
        && r.kernel_side_readable
        && r.kernel_side_in_mask
        && r.disjoint
        && r.oversize_refused
        && r.balanced;
    r
}

/// Die gemessene Geometrie in den Boot-Bericht schreiben.
///
/// Kein Testbericht, sondern eine Bring-up-Meldung (todo F3): sie sagt, was die *Hardware*
/// hergibt, unabhängig davon, ob ein Test läuft.
/// Das Ergebnis von [`run_color`] melden — **die einzige Druckstelle, von beiden Hochlaufwegen
/// gerufen.**
///
/// Sie stand bis 2026-08-01 in `arch/x86_64/bringup.rs`, also auf genau einem Zweig. Das hatte zwei
/// Folgen, und beide sind eingetreten: der Test lief auf aarch64 gar nicht (todo A1 „nur auf x86
/// gemessen"), und die Textfassung driftete — `color` schrieb als **einzige** von 69 Stellen `FAIL`
/// statt `FAILURES`, womit ein durchgefallener Test aus der Ergebnissignatur der Suite (B-1.2c)
/// HERAUSfiel, statt aufzufallen. Zwei Fassungen desselben Urteils driften; eine kann es nicht.
///
/// Hinter demselben Gate wie [`ColorTest`] — ohne das baut die schlanke Konfiguration nicht (F1).
#[cfg(feature = "selftest")]
pub fn report_color(c: &ColorTest) {
    if !c.usable {
        println!(
            "color   : SKIP -- {} Farbe(n) gemessen, unter 2 gibt es nichts zu trennen (QEMU meldet \
             ohne echtes CPU-Modell keine Cache-Geometrie; mit -cpu Skylake-Client sind es 256)",
            c.colors
        );
        return;
    }
    println!(
        "color   : {} Farben, {} Partitionen, Region {} KiB · in_mask={} \
         kernelseite={} (kstack={} l1={} l2={}) rueckgelesen={} (kstack={} l1={} l2={}) \
         disjunkt={} \
         uebergross_abgewiesen={} bilanz={}",
        c.colors,
        PARTITIONS,
        region_bytes() / 1024,
        c.in_mask as u8,
        c.kernel_side_in_mask as u8,
        c.ks_in_mask as u8,
        c.l1_in_mask as u8,
        c.l2_in_mask as u8,
        c.kernel_side_readable as u8,
        c.ks_read as u8,
        c.l1_read as u8,
        c.l2_read as u8,
        c.disjoint as u8,
        c.oversize_refused as u8,
        c.balanced as u8
    );
    println!(
        // `FAILURES`, nicht `FAIL` — s. den Kommentar am Kopf dieser Funktion.
        "color   : {} (zwei isolierte PDs teilen sich keine Cache-Farbe)",
        if c.ok { "ALL PASS" } else { "FAILURES" }
    );
    // **E-Rest 3d: wo liegen die Regionen wirklich?** Vor dem Fenster-Umbau war die Antwort
    // strukturell „unterhalb 1 GiB" -- die Abbildung war identisch, und die per-PD-Tabelle deckt
    // nur GiB 0. Jetzt ist sie eine Beobachtung. `SKIP` heisst hier ehrlich: diese Maschine hat
    // keinen Speicher oberhalb 4 GiB, der Fall ist also nicht entscheidbar -- **nicht**, dass er
    // nicht traegt.
    let oben = c.region_pa[0] >= hal::mmu::LOW_MAPPED_END && c.region_pa[1] >= hal::mmu::LOW_MAPPED_END;
    let hoch_vorhanden = hal::mmu::high_ram_gib() > 0;
    println!(
        "isohigh : Regionen zweier isolierter PDs bei {:#x} / {:#x}; oberhalb 4 GiB = {}",
        c.region_pa[0], c.region_pa[1], oben as u8
    );
    println!(
        "isohigh : {} (E-Rest 3d: die private Region einer isolierten PD haengt nicht mehr an \
         GiB 0. Sie wird ueber ein VA-Fenster ausserhalb der Identitaetskarte abgebildet, statt \
         identisch -- damit faellt der gemessene Deckel von 504 gleichzeitigen isolierten PDs. \
         Die Farbbedingung gilt unveraendert: sie ist eine Aussage ueber die Physadresse)",
        if !hoch_vorhanden {
            "SKIP -- kein RAM oberhalb 4 GiB auf dieser Maschine, der Fall ist nicht entscheidbar"
        } else if oben && c.ok {
            "ALL PASS"
        } else {
            "FAILURES"
        }
    );
}

pub fn report() {
    match hal::cache::llc() {
        Some(g) => {
            #[cfg(feature = "debug-log")]
            {
                let colors = count();
                println!(
                    "cache   : LLC L{} {} KiB, {}-fach, {} B/Zeile, {} Sets -> {} Seitenfarbe(n){}",
                    g.level,
                    g.size_bytes / 1024,
                    g.ways,
                    g.line_bytes,
                    g.sets,
                    colors,
                    if colors >= 2 { "" } else { " (keine Partitionierung moeglich)" }
                );
            }
            #[cfg(not(feature = "debug-log"))]
            let _ = g;
        }
        None => {
            #[cfg(feature = "debug-log")]
            println!(
                "cache   : keine Cache-Geometrie gemeldet -> 1 Seitenfarbe (keine Partitionierung moeglich)"
            )
        }
    }
}

// --- B-4.5: Wirkung statt nur Zuteilung (Prime+Probe) --------------------------------------
//
// Bis hierher belegt `run_color`, dass die Farbsätze zweier PDs **disjunkt sind**. Das ist eine
// Aussage über die Zuteilung, nicht über ihre Wirkung. Die eigentliche Behauptung von A1 lautet
// aber: *weil* sie disjunkt sind, verdrängen sich die PDs im LLC nicht gegenseitig. Diese Zeile
// stand bis 2026-08-01 ungeprüft in `docs/invariants.md` §12.
//
// ## Drei Messpunkte, nicht einer — und warum
//
//   base     — Opfer erneut lesen, ohne dass jemand dazwischen war
//   shared   — nachdem ein Angreifer mit dem GLEICHEN Farbsatz gelaufen ist
//   disjoint — nachdem ein Angreifer mit einem DISJUNKTEN Farbsatz gelaufen ist
//
// `shared` ist die **Positivkontrolle**. Bleibt sie nahe `base`, kann diese Maschine Verdrängung
// nicht zeigen, und dann ist über `disjoint` *nichts* auszusagen -> SKIP, nicht PASS. Ohne diese
// Kontrolle hätte der Test auf einer Maschine ohne echten Cache (TCG) „keine Verdrängung"
// gemeldet und damit A1 scheinbar bewiesen, ohne je etwas gemessen zu haben.
//
// ## Vier Fallen, die dieser Test beim Bauen selbst gestellt hat
//
// **1. Ein linearer Durchlauf misst den Vorauslader, nicht den Cache.** Erste Fassung: Opfer
// 64 KiB, Angreifer 6 MiB, sequenzielles Lesen je Cache-Zeile. Ergebnis 1904 / 4301 / 4528
// Zyklen — also 4,2 Zyklen je Zeile selbst im „verdrängten" Fall. Für einen DRAM-Zugriff ist das
// zwei Größenordnungen zu schnell: die Hardware hatte den nächsten Zugriff längst geholt, weil er
// vorhersagbar war. Gemessen wurde der Prefetcher. Deshalb jetzt eine **Zeigerkette** in
// bit-umgekehrter Reihenfolge: jeder Zugriff hängt am Ergebnis des vorigen (der Vorauslader kann
// die Adresse nicht kennen), und die Reihenfolge ist weder aufsteigend noch konstant-schrittig.
//
// **2. Ein Opfer, das in den L2 passt, sagt nichts über den L3.** Färbung partitioniert **nur**
// den Last-Level-Cache. Auf der Messmaschine hatte der L2 rund 1,5 MiB je Kern — ein 64-KiB-Opfer
// lag komplett darin, und der Angreifer räumte es dort heraus, unabhängig von jeder Farbe.
//
// **Die Behebung von damals war selbst eine feste Zahl** (2 MiB) und damit auf genau einer
// Maschine richtig. Gemessen am 2026-08-02 auf einer CPU mit **genau 2 MiB L2**: `ungestoert`
// lag bei 38–40 Zyklen — das ist L2-Latenz, das Opfer war also wieder komplett privat
// zwischengespeichert. Folge: der *disjunkte* Angreifer räumte es aus dem L2 (das kann er, Farbe
// hin oder her), und `disjoint` maß danach eine L3-Latenz statt der ungestörten — `effect` wäre
// selbst bei perfekt wirkender Färbung durchgefallen. Die Opfergröße wird deshalb jetzt aus
// `hal::cache::below_llc()` **abgeleitet** ([`victim_bytes`]), nicht festgeschrieben.
//
// **3. Der ANGREIFER durfte genausowenig linear laufen — und lief es bis 2026-08-02 doch.**
// Falle 1 wurde nur für das Opfer behoben; für den Angreifer stand hier ausdrücklich „hier genügt
// ein linearer Lauf: er soll den Cache *füllen*, nicht ihn messen". Das ist auf einer Maschine mit
// verdrängungsresistenter Einlagerung (adaptive/streaming-aware Insertion, jede aktuelle Intel-
// und AMD-Generation) **falsch**: ein rein sequenzieller Strom ist genau das Muster, das solche
// Caches als „kein Wiedergebrauch" erkennen und an der LRU-Position einlagern — er verdrängt den
// residenten Arbeitssatz dann NICHT. Gemessen am 2026-08-02, 10 Aufbauten mit BYTE-IDENTISCHER
// Zuteilung, Angreifer 24 MiB gegen 16 MiB LLC:
//
//     linear:      Positivkontrolle trug in  3 von 10 Aufbauten
//     bit-umgekehrt: Positivkontrolle trug in 10 von 10
//
// Im nicht tragenden Fall lag `gleichfarbig` bei 43 gegen `ungestoert` 38 Zyklen — der Angreifer
// war schlicht wirkungslos. Also läuft **auch der Angreifer** in bit-umgekehrter Reihenfolge.
//
// **4. Das Minimum ist der falsche Schätzer — für zwei von drei Messpunkten.** „Störungen können
// eine Messung nur verlängern, nie verkürzen" stimmt für `base` (reine Latenz). Für `shared` und
// `disjoint` ist die gesuchte Größe aber die **Verdrängung**, und jede Störung, die den Angreifer
// bremst oder den Arbeitssatz teilweise stehen lässt, macht die Messung *kürzer*. Das Minimum
// über N Proben greift also systematisch in genau die Richtung, die die Positivkontrolle
// zerstört. Gemessen (dieselben 10 Aufbauten, bit-umgekehrter Angreifer, 16 Proben):
//
//     Minimum: Positivkontrolle trug in  4 von 10
//     Median:  Positivkontrolle trug in 10 von 10
//
// Gewertet wird deshalb der **Median**, und gemeldet werden Minimum/Median/Maximum — eine
// Messung, die ihre eigene Streuung nicht kennt, taugt für eine Blech-Aussage nicht.
//
// Alle vier sind derselbe Fehlertyp, gegen den dieses Projekt seine Regel hat: eine Messung, die
// nicht messen KANN, was sie zu messen behauptet.
//
// ## Warum der Aufbau NICHT mehr aus `alloc_colored` kommt
//
// Bis 2026-08-02 wurde jede einzelne Region des Aufbaus mit `alloc_colored` geholt. Eine gefärbte
// Region ist auf [`region_bytes`] begrenzt (auf x86 64 KiB), also brauchte der Aufbau rund
// **800 Einzelallokationen** für 50 MiB. Das hat drei Folgen, und zwei davon sind still:
//
//  * Die Freiliste des Allokators ist endlich (`MAX_FRAGMENTS = 1024`). Gemessen wurde ein
//    Höchststand von **419** Fragmenten — noch nicht am Anschlag, aber der mit Abstand größte
//    Verbraucher im Kernel, und die Zahl skaliert mit `1/region_bytes`: auf einer Maschine mit
//    16 statt 256 Farben wären es viermal so viele.
//  * `MAX_ATTACKER` deckelte die Angreifergröße. Auf der Messmaschine war der Deckel mit 384
//    Regionen **exakt erreicht** — jede Maschine mit größerem LLC hätte einen stillschweigend zu
//    kleinen Angreifer bekommen, und damit eine stillschweigend geschwächte Positivkontrolle.
//  * Die drei Regionslisten lagen als lokale Arrays auf dem Kernel-Stack: 12,8 KiB Rahmen bei
//    16 KiB Stack.
//
// Jetzt kommt der Speicher aus wenigen **großen, ungefärbten** Blöcken ([`BLOCK_BYTES`]), und die
// farbreinen Läufe werden darin **gesucht** — mit `color_of` und derselben Maske, die auch der
// Allokator befragt. Das ist nicht nur billiger, es ist die **stärkere** Aussage: die Farbe jeder
// benutzten Seite wird an der Verwendungsstelle nachgerechnet, statt einem `alloc_colored`
// geglaubt zu werden. Und die Bilanz wird an genau den Blöcken geprüft, die der Test geholt hat
// (`region_fully_free`), nicht am globalen Summenzähler — der ist nachweislich untauglich, weil
// ein anderer Kern ihn nebenher bewegt (s. `caprock_mem::PhysAllocator::fully_free`).

/// **Opfergröße, aus der Geometrie abgeleitet** — und zwar aus BEIDEN Schranken.
///
/// Das Opfer muss in ein Fenster passen, das die Maschine vorgibt:
///
/// ```text
///     Größe der privaten Ebene   <   Opfer   ≤   LLC / PARTITIONS
///          (Untergrenze)                          (Obergrenze)
/// ```
///
/// * **Untergrenze** (Falle 2): Färbung partitioniert nur den LLC. Passt das Opfer vollständig in
///   die darunterliegende, *nicht* partitionierte Ebene, verdrängt es jeder Angreifer von dort —
///   auch der farblich disjunkte. `disjoint` misst dann eine Ebene tiefer als `base`, und
///   `effect` fällt durch, obwohl die Färbung tut, was sie soll.
/// * **Obergrenze**: der Farbanteil des LLC. Ein größeres Opfer kann selbst bei perfekt wirkender
///   Färbung nicht resident bleiben — `effect` wäre wieder unerfüllbar, diesmal aus dem
///   umgekehrten Grund.
///
/// **Das Fenster kann leer sein**, und auf der Messmaschine ist es das: QEMU meldet unter
/// `-cpu host` 4 MiB für die Ebene unter dem LLC und 16 MiB LLC, bei `PARTITIONS = 4` also
/// 4 MiB Farbanteil. Untergrenze = Obergrenze, kein gültiges Opfer. Das ist **kein Fehler des
/// Tests**, sondern eine Aussage über die Maschine: wo eine nicht partitionierte Cache-Ebene so
/// groß ist wie ein ganzer Farbanteil des LLC, kann Färbung mit dieser Streifenzahl nichts
/// schützen, was nicht ohnehin privat zwischengespeichert ist. Der Test meldet das, statt eine
/// Zahl zu liefern.
///
/// Zurückgegeben wird die größte Zweierpotenz ≤ Obergrenze (die Zeigerkette permutiert per
/// Bitumkehr und braucht eine Zweierpotenz an Zeilen); ob sie über der Untergrenze liegt, sagt
/// [`PrimeProbe::victim_over_private`].
#[cfg(feature = "selftest")]
fn victim_bytes(llc_bytes: u64) -> u64 {
    let obergrenze = (llc_bytes / u64::from(PARTITIONS)).max(16 * PAGE);
    // Größte Zweierpotenz <= obergrenze.
    let v = obergrenze.next_power_of_two();
    if v > obergrenze {
        v / 2
    } else {
        v
    }
}

/// Angreifergröße je Farbsatz: `ATT_ZAEHLER / ATT_NENNER` × LLC.
///
/// **Gegen den GANZEN LLC dimensioniert, nicht gegen den Streifenanteil.** Das war im ersten
/// Anlauf falsch herum gedacht: der Streifenanteil (LLC/PARTITIONS) genügt nur, WENN die Färbung
/// die Platzierung im LLC tatsächlich steuert — und genau das soll hier erst gezeigt werden. Tut
/// sie es nicht, verteilen sich die Zugriffe über den ganzen Cache, und ein Viertel der LLC-Größe
/// verdrängt dann NICHTS, auch nicht im gleichfarbigen Fall. Die Positivkontrolle wäre
/// gescheitert, ohne dass daraus etwas folgte — der Test hätte seine eigene Annahme geprüft.
#[cfg(feature = "selftest")]
const ATT_ZAEHLER: u64 = 3;
#[cfg(feature = "selftest")]
const ATT_NENNER: u64 = 2;

/// Farbreine Läufe je Rolle (Opfer, gleichfarbiger Angreifer, disjunkter Angreifer).
///
/// Reicht auf der Messmaschine für einen Angreifer von 32 MiB (512 × 64 KiB) — das Doppelte des
/// dortigen LLC. Wird die Zahl zur Schranke, sagt der Bericht das (`angreifer=…% des LLC`),
/// statt eine geschwächte Positivkontrolle zu verschweigen.
#[cfg(feature = "selftest")]
const RUNS_JE_ROLLE: usize = 512;

/// Ein Rückspeicherblock. Groß genug, dass wenige genügen; klein genug, dass keine einzelne
/// Anforderung an einer zerstückelten Freiliste scheitert.
#[cfg(feature = "selftest")]
const BLOCK_BYTES: u64 = 8 * 1024 * 1024;

/// Höchstzahl Rückspeicherblöcke.
#[cfg(feature = "selftest")]
const MAX_BLOECKE: usize = 64;

/// Proben je Messpunkt. **Ungerade**, damit der Median eine echte Probe ist und nicht ein
/// gemitteltes Zwischending zweier Ausreißer.
#[cfg(feature = "selftest")]
const PROBEN: usize = 9;

/// Arena des Tests: die Caps der Rückspeicherblöcke und die farbreinen Läufe.
///
/// **Statisch, nicht auf dem Kernel-Stack** (A-3.3): 1536 Läufe wären 24 KiB Rahmen bei 16 KiB
/// Stack. Genau diese Falle hat `ReplyFinal` schon einmal gestellt (Rahmen 6168 Byte) — und die
/// Vorgängerfassung dieses Tests trug sie mit 12,8 KiB unbemerkt mit sich herum.
#[cfg(feature = "selftest")]
struct PpArena {
    bloecke: [Option<caprock_mem::MemoryCap>; MAX_BLOECKE],
    /// Die Spannen der geholten Blöcke — **getrennt von den Caps geführt**, und das ist keine
    /// Redundanz, sondern die ganze Aussagekraft der Bilanzprüfung.
    ///
    /// Ein erster Entwurf prüfte `region_fully_free` innerhalb der Freigabeschleife, also nur an
    /// den Blöcken, die er *auch freigegeben hatte*. Gegenprobe (Freigabeschleife um einen Block
    /// verkürzt): `bilanz=1`, der Test lief grün, und ein 8-MiB-Block war weg. Ein Prüfer, der
    /// nur das prüft, was er selbst getan hat, kann ein Vergessen nicht sehen. Geprüft wird
    /// deshalb an dieser Liste — sie hält fest, was geholt wurde, unabhängig davon, was die
    /// Freigabe daraus gemacht hat.
    spannen: [(u64, u64); MAX_BLOECKE],
    /// `[0..R)` Opfer, `[R..2R)` gleichfarbiger Angreifer, `[2R..3R)` disjunkter Angreifer.
    laeufe: [(u64, u64); 3 * RUNS_JE_ROLLE],
}

#[cfg(feature = "selftest")]
static PP_ARENA: caprock_sync::SpinLock<PpArena> = caprock_sync::SpinLock::new(PpArena {
    bloecke: [const { None }; MAX_BLOECKE],
    spannen: [(0, 0); MAX_BLOECKE],
    laeufe: [(0, 0); 3 * RUNS_JE_ROLLE],
});

/// Fünf Kennzahlen einer Messreihe: Minimum, unteres Quartil, Median, oberes Quartil, Maximum.
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Default)]
pub struct Streuung {
    pub min: u64,
    pub q1: u64,
    pub med: u64,
    pub q3: u64,
    pub max: u64,
}

/// Ergebnis des Prime+Probe (B-4.5).
#[cfg(feature = "selftest")]
#[derive(Default)]
pub struct PrimeProbe {
    /// Urteil. Nur aussagekräftig, wenn [`Self::decidable`] gilt; sonst heißt `true` bloß
    /// „nichts kaputt" (Bilanz stimmt, Farbwahl stimmt).
    pub ok: bool,
    /// Konnte der Aufbau überhaupt hergestellt werden (Farben, Geometrie, Speicher)?
    pub ran: bool,
    /// Warum nicht, falls nicht. Kurzform für den Bericht.
    pub grund: &'static str,
    /// **Farbwahl geprüft**: Opfer und gleichfarbiger Angreifer teilen Farben, der disjunkte
    /// Angreifer teilt mit keinem von beiden eine. Das ist die Voraussetzung dafür, dass die drei
    /// Messpunkte überhaupt das messen, was ihre Namen sagen.
    pub farbtreu: bool,
    /// **Hat die Uhr auf dieser Maschine ueberhaupt Aufloesung fuer diese Kettenlaenge?**
    ///
    /// `cycles()` ist auf aarch64 `CNTPCT_EL0` (architektonischer Zaehler, grobe Granularitaet)
    /// und auf x86 der TSC. Ist die Gesamtdauer eines ungestoerten Durchlaufs **0**, hat der
    /// Zaehler waehrend der Messung nicht getickt — dann ist jede weitere Aussage
    /// Rundungsrauschen. Getrennt gefuehrt, weil das ein Befund ueber die UHR ist und keiner
    /// ueber den Cache.
    pub clock_ok: bool,
    /// Kettenglieder je Durchlauf — nur, um im Bericht aus der Gesamtdauer die vertraute Zahl
    /// „Zyklen je Glied" rechnen zu koennen.
    pub n_lines: u64,
    /// **Positivkontrolle**: hat der gleichfarbige Angreifer messbar verdrängt (Median)?
    pub sensitive: bool,
    /// **Auflösung**: sind `base` und `shared` als Verteilungen überhaupt trennbar (q3 < q1)?
    /// Ohne das ist ein Vergleich der Mediane eine Zahl ohne Aussage.
    pub resolved: bool,
    /// Ist die Frage auf DIESER Maschine entscheidbar? (`ran && !guest && sensitive && resolved`)
    pub decidable: bool,
    /// War der Effekt da (disjunkt näher am ungestörten Fall als am verdrängten)?
    pub effect: bool,
    /// Läuft der Kernel unter einem Hypervisor? Dann ist [`Self::effect`] **nicht entscheidbar**.
    pub guest: bool,
    /// Zyklen je Kettenglied — ungestört.
    pub base: Streuung,
    /// … nach einem Angreifer mit **gleichem** Farbsatz.
    pub shared: Streuung,
    /// … nach einem Angreifer mit **disjunktem** Farbsatz.
    pub disjoint: Streuung,
    /// Opfergröße in KiB.
    pub victim_kib: u64,
    /// Grösse der grössten **nicht partitionierten** Cache-Ebene in KiB (0 = nicht gemeldet).
    pub private_kib: u64,
    /// **Überschreitet das Opfer diese Ebene?**
    ///
    /// `Some(true)`: nachgewiesen. `Some(false)`: nachweislich **nicht** — dann misst der Test
    /// die private Ebene statt der Partitionierung, und die Zahlen sind keine Aussage über A1
    /// (Falle 2). `None`: die Maschine meldet keine solche Ebene, die Bedingung ist dann eine
    /// **Annahme** — und steht als solche im Bericht, statt als Häkchen durchzugehen.
    pub victim_over_private: Option<bool>,
    /// Größe **eines** Angreifers in KiB.
    pub attacker_kib: u64,
    /// Angreifergröße in Prozent des LLC. Unter 100 ist die Positivkontrolle geschwächt — das
    /// gehört in den Bericht, nicht in eine Fußnote.
    pub attacker_pct: u64,
    /// Rückspeicherblöcke, die der Test geholt hat.
    pub bloecke: usize,
    /// Freie Fragmente vor / nach dem Test. Gleich heißt: die Freiliste ist wieder koalesziert.
    pub frag_vor: usize,
    pub frag_nach: usize,
    /// **Jeder** geholte Block ist vollständig zurück (`region_fully_free`, nicht Summenvergleich).
    pub balanced: bool,
}

/// Adresse der `k`-ten Cache-Zeile über eine Liste von Regionen gleicher Länge.
#[cfg(feature = "selftest")]
fn line_addr(regions: &[(u64, u64)], line: u64, k: u64) -> u64 {
    let je_region = regions[0].1 / line;
    let (r, i) = ((k / je_region) as usize, k % je_region);
    regions[r].0 + i * line
}

/// Bit-umgekehrte Permutation über `2^bits` Indizes.
///
/// Bitumkehr ist eine Permutation (also besucht sie jeden Index genau einmal) und erzeugt eine
/// Folge, die weder aufsteigend noch konstant-schrittig ist — beides würde ein Stride-Prefetcher
/// erkennen, und ein rein sequenzieller Strom würde von der Einlagerungspolitik des Cache als
/// „kein Wiedergebrauch" behandelt (Falle 3).
#[cfg(feature = "selftest")]
#[inline]
fn bitumkehr(k: u64, bits: u32) -> u64 {
    k.reverse_bits() >> (64 - bits)
}

/// Eine **Zeigerkette** durch alle Cache-Zeilen legen, in bit-umgekehrter Reihenfolge.
/// Gibt den Einstiegspunkt zurück.
#[cfg(feature = "selftest")]
fn build_chain(regions: &[(u64, u64)], line: u64, bits: u32) -> u64 {
    let n = 1u64 << bits;
    for k in 0..n {
        let von = line_addr(regions, line, bitumkehr(k, bits));
        let nach = line_addr(regions, line, bitumkehr((k + 1) % n, bits));
        // SAFETY: die Region stammt aus `system::alloc`, gehört exklusiv dem Test und liegt im
        // identity-gemappten Normal-RAM. Geschrieben wird der Anfang jeder Cache-Zeile.
        unsafe { core::ptr::write_volatile(von as *mut u64, nach) };
    }
    line_addr(regions, line, bitumkehr(0, bits))
}

/// Die Kette `n` Glieder weit verfolgen und die **Gesamtdauer** melden.
///
/// Jeder Zugriff hängt am Ergebnis des vorigen — der Vorauslader kann die nächste Adresse nicht
/// erraten, und genau das trennt einen Cache-Treffer von einem DRAM-Zugriff.
///
/// **Gemeldet wird die Dauer des GANZEN Durchlaufs, nicht die je Glied.**
///
/// Das war bis 2026-08-02 anders (`dt / n`), und auf aarch64 hat es den Test blind gemacht:
/// `cycles()` ist dort `CNTPCT_EL0`, der architektonische Zähler mit grober Granularität. Bei
/// 4096 Kettengliedern ergab die Ganzzahldivision **0** — `ungestoert=0/0/0 disjunkt=0/0/1`.
/// Gemessen wurde die Auflösung des Zählers, nicht der Cache, und das Urteil hing an
/// Rundungsrauschen.
///
/// Alle Vergleiche des Tests sind **Verhältnisse** (`shared >= 1,5 × base`,
/// `2 × disjoint <= base + shared`); sie gelten für die Gesamtdauer unverändert, nur mit `n`-mal
/// mehr Auflösung. Die vertraute Zahl „Zyklen je Glied" steht weiterhin im Bericht — sie wird
/// dort aus der Gesamtdauer gerechnet, statt das Urteil zu tragen.
#[cfg(feature = "selftest")]
fn chase(start: u64, n: u64) -> u64 {
    let mut p = start;
    let t0 = hal::timer::cycles();
    for _ in 0..n {
        // SAFETY: `p` läuft ausschließlich über die von `build_chain` beschriebenen Zeilen.
        p = unsafe { core::ptr::read_volatile(p as *const u64) };
    }
    let dt = hal::timer::cycles().wrapping_sub(t0);
    core::hint::black_box(p);
    dt
}

/// Einen Datensatz einmal überschreiben — der Angreifer.
///
/// **In bit-umgekehrter Reihenfolge, nicht linear** (Falle 3): ein sequenzieller Strom wird von
/// der Einlagerungspolitik moderner Caches als „kein Wiedergebrauch" erkannt und verdrängt den
/// residenten Arbeitssatz dann nicht. Die Zahl der Speicherzugriffe ist dieselbe wie beim
/// linearen Lauf; nur ihre Reihenfolge ist es nicht.
#[cfg(feature = "selftest")]
fn walk(regions: &[(u64, u64)], line: u64) {
    if regions.is_empty() || regions[0].1 < line {
        return;
    }
    let n = regions.len() as u64 * (regions[0].1 / line);
    // Aufgerundete Zweierpotenz; Indizes jenseits von `n` werden übersprungen. Das bleibt eine
    // Permutation von `0..n` — jede Zeile genau einmal.
    let bits = u64::BITS - (n - 1).leading_zeros();
    for k in 0..(1u64 << bits) {
        let idx = bitumkehr(k, bits);
        if idx < n {
            // SAFETY: wie `build_chain`.
            unsafe { core::ptr::write_volatile(line_addr(regions, line, idx) as *mut u64, idx) };
        }
    }
}

/// Trägt **jede** Seite von `[start, start+len)` eine Farbe aus `mask`?
#[cfg(feature = "selftest")]
fn lauf_ist_farbrein(start: u64, len: u64, colors: u32, mask: ColorMask) -> bool {
    let mut p = start;
    while p < start + len {
        if !mask.contains(color_of(p, colors)) {
            return false;
        }
        p += PAGE;
    }
    true
}

/// Welche **Maskenbits** belegt diese Läufeliste tatsächlich?
///
/// Bewusst über die belegten Seiten und nicht über die angeforderte Maske: dass zwei Masken
/// disjunkt sind, hat der Host-Test der Crate bereits gezeigt. Hier ist die Frage, ob die
/// Auswahl sich auch daran gehalten hat — dieselbe Trennung wie `regions_share_color` in
/// [`run_color`].
#[cfg(feature = "selftest")]
fn belegte_maskenbits(runs: &[(u64, u64)], colors: u32) -> u64 {
    let mut m = 0u64;
    for &(b, l) in runs {
        let mut p = b;
        while p < b + l {
            m |= 1u64 << (color_of(p, colors) % caprock_mem::MASK_BITS);
            p += PAGE;
        }
    }
    m
}

/// Kennzahlen einer Probenreihe (sortiert eine Kopie; `PROBEN` ist winzig).
#[cfg(feature = "selftest")]
fn streuung(proben: &[u64; PROBEN]) -> Streuung {
    let mut v = *proben;
    for i in 1..PROBEN {
        let x = v[i];
        let mut j = i;
        while j > 0 && v[j - 1] > x {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
    Streuung { min: v[0], q1: v[PROBEN / 4], med: v[PROBEN / 2], q3: v[3 * PROBEN / 4], max: v[PROBEN - 1] }
}

/// **Prime+Probe: verdrängen disjunkte Farbsätze einander messbar weniger?** (B-4.5)
///
/// `ran == false`: Aufbau nicht herstellbar (eine Farbe, keine Geometrie, zu wenig Speicher).
/// `sensitive == false`: die Maschine kann Verdrängung nicht zeigen. `resolved == false`: sie ist
/// zu verrauscht, um die beiden Zustände zu trennen. Alle drei sind **kein Fehlschlag**, sondern
/// die Feststellung, dass hier nichts zu entscheiden ist — und alle drei stehen mit ihren Zahlen
/// im Bericht, damit „nichts zu entscheiden" nicht wie „alles in Ordnung" aussieht.
///
/// **Unter einem Hypervisor wird trotzdem gemessen.** Entschieden wird dort nichts (ein Gast
/// färbt gastphysische Adressen; die zweite Übersetzungsstufe bildet jede 4-KiB-Seite auf eine
/// beliebige Wirtsseite ab, und die Farbbits liegen oberhalb des Seitenoffsets) — aber der
/// Aufbau, die Positivkontrolle, die Farbwahl und die Bilanz sind dort genauso prüfbar wie auf
/// Blech, und ein Prüfpfad, der nur am Zieltag zum ersten Mal läuft, ist am Zieltag kaputt. Diese
/// Fehlerform hat das Projekt bereits viermal bezahlt (leere Event-Queue, nie ausgeführter
/// x86-Testpfad, DMAR-Ausschlusspfad, `--no-default-features` auf aarch64).
#[cfg(feature = "selftest")]
pub fn run_prime_probe() -> PrimeProbe {
    let mut r = PrimeProbe::default();
    r.guest = hal::cpu::hypervisor_present();
    r.balanced = true; // nichts geholt ist bilanziert
    if !usable() {
        r.grund = "weniger als 2 Seitenfarben";
        return r;
    }
    let Some(g) = hal::cache::llc() else {
        r.grund = "keine LLC-Geometrie";
        return r;
    };
    let (Some(m0), Some(m1)) = (mask_for(0), mask_for(1)) else {
        r.grund = "Farbaufteilung geht nicht auf";
        return r;
    };
    let colors = count();
    let line = u64::from(g.line_bytes).max(16);
    let sz = region_bytes();

    // **Die private (nicht partitionierte) Ebene bestimmt die Opfergröße** — s. Falle 2.
    let privat = hal::cache::below_llc().map(|p| p.size_bytes);
    r.private_kib = privat.unwrap_or(0) / 1024;
    let victim_soll = victim_bytes(u64::from(g.size_bytes));
    r.victim_over_private = privat.map(|p| victim_soll > p);

    // Soll-Zahlen. `v_soll` ist eine Zweierpotenz, weil `victim_bytes` und `sz` es sind — die
    // Zeigerkette permutiert per Bitumkehr und braucht eine Zweierpotenz an Zeilen.
    let v_soll = (victim_soll / sz).max(1) as usize;
    let a_roh = ((ATT_ZAEHLER * u64::from(g.size_bytes) / ATT_NENNER).div_ceil(sz)) as usize;
    let a_soll = a_roh.clamp(1, RUNS_JE_ROLLE);
    if v_soll > RUNS_JE_ROLLE {
        r.grund = "Opfer passt nicht in die Laeufeliste";
        return r;
    }
    let n_lines = v_soll as u64 * sz / line;
    if !n_lines.is_power_of_two() || n_lines < 2 {
        // Ohne Zweierpotenz deckte die Bitumkehr nur einen Teil der Kette ab — und der Test
        // maesse ein kleineres Opfer, als er meldet. Lieber gar nicht messen.
        r.grund = "Zeilenzahl des Opfers ist keine Zweierpotenz";
        return r;
    }

    let frag_vor = crate::system::fragments();
    let mut arena = PP_ARENA.lock();

    // --- Rückspeicher holen und farbreine Läufe darin suchen ---------------------------------
    //
    // Beide Farbsätze werden aus DENSELBEN Blöcken bedient: ein Block liefert je Streifen
    // ungefähr `1/PARTITIONS` seiner Seiten, und die beiden Rollen brauchen verschiedene
    // Streifen. Zwei getrennte Vorräte wären doppelt so viel Speicher für dasselbe Ergebnis.
    let m0_soll = v_soll + a_soll; // Opfer und gleichfarbiger Angreifer teilen den Streifen
    let m1_soll = a_soll;
    let (mut n0, mut n1, mut n_blk) = (0usize, 0usize, 0usize);
    let mut block = BLOCK_BYTES;
    while (n0 < m0_soll || n1 < m1_soll) && n_blk < MAX_BLOECKE {
        let Some(cap) = crate::system::alloc(block, PAGE) else {
            // Kein Block dieser Größe mehr: kleiner werden, statt aufzugeben. Unter `sz` hat es
            // keinen Sinn mehr — ein Block, der keinen farbreinen Lauf fassen kann, bringt nichts.
            if block > sz * u64::from(PARTITIONS) {
                block /= 2;
                continue;
            }
            break;
        };
        let (bb, bl) = (cap.base(), cap.len());
        arena.bloecke[n_blk] = Some(cap);
        arena.spannen[n_blk] = (bb, bl);
        n_blk += 1;

        let mut p = bb;
        while p + sz <= bb + bl && (n0 < m0_soll || n1 < m1_soll) {
            if lauf_ist_farbrein(p, sz, colors, m0) {
                if n0 < m0_soll {
                    // Opfer zuerst füllen, dann den gleichfarbigen Angreifer.
                    let idx = if n0 < v_soll { n0 } else { RUNS_JE_ROLLE + (n0 - v_soll) };
                    arena.laeufe[idx] = (p, sz);
                    n0 += 1;
                }
                p += sz;
            } else if lauf_ist_farbrein(p, sz, colors, m1) {
                if n1 < m1_soll {
                    arena.laeufe[2 * RUNS_JE_ROLLE + n1] = (p, sz);
                    n1 += 1;
                }
                p += sz;
            } else {
                p += PAGE;
            }
        }
    }
    r.bloecke = n_blk;
    let aufgebaut = n0 == m0_soll && n1 == m1_soll;

    if aufgebaut {
        r.ran = true;
        r.victim_kib = v_soll as u64 * sz / 1024;
        r.attacker_kib = a_soll as u64 * sz / 1024;
        r.attacker_pct = (a_soll as u64 * sz).saturating_mul(100) / u64::from(g.size_bytes).max(1);

        let (opfer, rest) = arena.laeufe.split_at(RUNS_JE_ROLLE);
        let (gleich_alle, disjunkt_alle) = rest.split_at(RUNS_JE_ROLLE);
        let opfer = &opfer[..v_soll];
        let gleich = &gleich_alle[..a_soll];
        let disjunkt = &disjunkt_alle[..a_soll];

        // **Die Farbwahl wird nachgerechnet, nicht geglaubt.** Ohne das könnte eine verrutschte
        // Index-Arithmetik dem „disjunkten" Angreifer Seiten des Opferstreifens geben — und die
        // Messung wäre dreimal dieselbe Größe unter drei Namen.
        let bv = belegte_maskenbits(opfer, colors);
        let bg = belegte_maskenbits(gleich, colors);
        let bd = belegte_maskenbits(disjunkt, colors);
        r.farbtreu = bv != 0 && bd != 0 && (bv | bg) & bd == 0 && bv & bg != 0;

        let start = build_chain(opfer, line, n_lines.trailing_zeros());

        // **Zwischen zwei Proben werden die IRQs kurz aufgemacht** — und das ist die wichtigere
        // Hälfte dieser Schleife.
        //
        // `SpinLock::lock()` maskiert Interrupts am eigenen Kern für die **gesamte Lebensdauer
        // des Guards** (`irq_save_disable`, gegen den reentranten Ticket-Deadlock). Der
        // Arena-Guard lebt hier über Aufbau, Messung und Abbau; ohne Gegenmaßnahme wäre der
        // Bootkern also eine halbe Sekunde am Stück ohne Timer-Tick. Gemessen, wohin das führt:
        // mit einem 8-MiB-Opfer (rund 1,5 s) endete der Lauf am 2026-08-02 im
        // `bringup : WATCHDOG` mit `ipc : FAILURES` — der Prime+Probe hatte einen Test gekippt,
        // der mit ihm nichts zu tun hat. Genau diese Falle hält den Farbtest auf aarch64 aus
        // `spawn_demo` heraus, und sie macht das GESAMTE Ergebnis unbrauchbar, nicht nur das
        // eigene.
        //
        // **Warum das Aufmachen hier zulässig ist und nicht allgemein:** die Maskierung schützt
        // davor, dass ein Interrupt-Pfad denselben Lock reentrant zieht. `PP_ARENA` wird von
        // genau einer Stelle genommen — dieser Funktion —, und die läuft nicht aus einem
        // Interrupt. Für jeden anderen Lock gilt die Begründung NICHT.
        let daif_offen = {
            let s = hal::cpu::local_irq_save();
            hal::cpu::local_irq_restore(s);
            s
        };
        let (mut pb, mut pd, mut ps) = ([0u64; PROBEN], [0u64; PROBEN], [0u64; PROBEN]);
        for k in 0..PROBEN {
            chase(start, n_lines); // aufwärmen, nicht gewertet
            pb[k] = chase(start, n_lines);

            chase(start, n_lines);
            walk(disjunkt, line);
            pd[k] = chase(start, n_lines);

            chase(start, n_lines);
            walk(gleich, line);
            ps[k] = chase(start, n_lines);

            // Atempause: aufgestaute Interrupts einmal zustellen lassen, dann wieder zu.
            hal::cpu::local_irq_restore(daif_offen);
            core::hint::spin_loop();
            let _ = hal::cpu::local_irq_save();
        }
        r.base = streuung(&pb);
        r.disjoint = streuung(&pd);
        r.shared = streuung(&ps);

        // Positivkontrolle über den MEDIAN (Falle 4): der gleichfarbige Angreifer muss den
        // Zugriff mindestens um die Hälfte verlängert haben.
        r.n_lines = n_lines;
        // Eine Uhr, die waehrend des Durchlaufs nicht tickt, misst nichts. Das ist kein
        // Fehlschlag der Faerbung, sondern die Feststellung, dass hier nicht gemessen werden kann.
        r.clock_ok = r.base.med > 0;
        r.sensitive = r.clock_ok && r.shared.med >= r.base.med.saturating_add(r.base.med / 2);
        // Auflösung: die beiden Verteilungen müssen sich trennen lassen. Überlappen sie, ist der
        // Medianvergleich eine Zahl ohne Aussage — und ein Urteil daraus wäre geraten.
        r.resolved = r.base.q3 < r.shared.q1;
        // Ein Opfer, das in die private Ebene passt, macht `effect` auch bei perfekter Färbung
        // unerfüllbar (Falle 2). Dann ist die Frage nicht beantwortet, sondern falsch gestellt —
        // und ein FAILURES daraus wäre eine Falschaussage über A1.
        r.decidable = !r.guest && r.sensitive && r.resolved && r.victim_over_private != Some(false);
        // Die eigentliche Behauptung: mit disjunkten Farben liegt die Zeit näher am ungestörten
        // Fall als am verdrängten.
        r.effect = r.disjoint.med.saturating_mul(2) <= r.base.med.saturating_add(r.shared.med);
    } else if n_blk == MAX_BLOECKE || block <= sz * u64::from(PARTITIONS) {
        r.grund = "zu wenig zusammenhaengender Speicher fuer den Rueckspeicher";
    } else {
        r.grund = "nicht genug farbreine Laeufe im Rueckspeicher";
    }

    // --- Rückgabe und Bilanz -----------------------------------------------------------------
    //
    // **Rückwärts freigeben**: so trifft jede Freigabe auf den bereits freien Nachbarn und
    // verschmilzt mit ihm, statt einen neuen Eintrag in der Freiliste zu brauchen.
    //
    // Geprüft wird an GENAU den geholten Blöcken (`region_fully_free`), nicht am globalen
    // Summenzähler: der ist hier nachweislich untauglich, weil nebenher ein anderer Kern einen
    // Thread-Stack einsammeln kann — dann wird derselbe Kernel mal grün und mal rot gemeldet.
    // Dieselbe Begründung wie in `run_color`, und dieselbe Falle, die dort schon einmal zugebissen
    // hat.
    for i in (0..n_blk).rev() {
        if let Some(cap) = arena.bloecke[i].take() {
            crate::system::free(cap);
        }
    }
    // **Getrennte Schleife, getrennte Quelle.** Geprüft wird an den gemerkten Spannen, nicht an
    // dem, was die Freigabe angefasst hat — s. `PpArena::spannen`.
    let mut alle_frei = true;
    for i in 0..n_blk {
        let (b, l) = arena.spannen[i];
        alle_frei &= crate::system::region_fully_free(b, l);
        arena.spannen[i] = (0, 0);
    }
    drop(arena);
    r.balanced = alle_frei;
    r.frag_vor = frag_vor;
    r.frag_nach = crate::system::fragments();
    // `ok` heißt: nichts ist kaputt. Ein Urteil über A1 steckt nur dann darin, wenn die Frage auf
    // dieser Maschine überhaupt entscheidbar war.
    r.ok = r.balanced && (!r.ran || r.farbtreu) && (!r.decidable || r.effect);
    r
}

/// Zyklen je Kettenglied als (Ganzzahl, Zehntel) — eine Nachkommastelle ohne Fliesskomma.
///
/// Der Kernel rechnet nirgends mit `f64`; die Zahl ist trotzdem noetig, weil die Ganzzahldivision
/// auf Plattformen mit grobem Zaehler (aarch64: `CNTPCT_EL0`) sonst `0` liefert — und `0` ist
/// keine Messgroesse, sondern eine verlorene Aussage.
#[cfg(feature = "selftest")]
/// Untergrenze fuer „das kann eine Speicherlatenz sein" (Zyklen je Kettenglied).
///
/// Bewusst **niedrig**: 3 Zyklen liegen unter jeder realen L1-Trefferlatenz (typisch 4–5), die
/// Schranke soll Emulation fangen, nicht schnelle Maschinen ausschliessen. Zu hoch gezogen wuerde
/// sie auf echter Hardware ein gueltiges Ergebnis verwerfen -- derselbe Fehler wie eine zu enge
/// Plausibilitaetsgrenze in der Zyklenabrechnung, nur andersherum.
const MIN_ZYKLEN_JE_GLIED: u64 = 3;

fn je_glied(gesamt: u64, n: u64) -> (u64, u64) {
    let zehntel = gesamt.saturating_mul(10) / n.max(1);
    (zehntel / 10, zehntel % 10)
}

/// Das Ergebnis von [`run_prime_probe`] melden — wie [`report_color`] die **einzige** Druckstelle.
///
/// Die Zahlen werden **immer** gedruckt, auch wenn nichts zu entscheiden ist. Eine Messung, die
/// nur im Erfolgsfall Zahlen zeigt, lässt sich beim nächsten Mal nicht mit dem letzten Mal
/// vergleichen — und genau das braucht der Blech-Lauf, den B-4.5 noch schuldet.
#[cfg(feature = "selftest")]
pub fn report_prime_probe(p: &PrimeProbe) {
    if !p.ran {
        println!(
            "pprobe  : SKIP -- kein Aufbau moeglich: {} ({} Farbe(n), {} Rueckspeicherbloecke geholt)",
            p.grund,
            count(),
            p.bloecke
        );
        // Auch ein nicht durchgefuehrter Test muss seinen Speicher zurueckgegeben haben.
        if !p.balanced {
            println!("pprobe  : FAILURES (Aufbau abgebrochen UND Speicher nicht vollstaendig zurueck)");
        }
        return;
    }
    println!(
        "pprobe  : Opfer {} KiB (privat {}, ueber-privat={}), Angreifer {} KiB je Farbsatz \
         ({}% des LLC), {} Bloecke, Kette {} Glieder, {} Proben · Zyklen je Durchlauf \
         (min/median/max): ungestoert={}/{}/{} disjunkt={}/{}/{} gleichfarbig={}/{}/{} \
         · je Glied (Median): {}.{}/{}.{}/{}.{} · farbtreu={} bilanz={} fragmente {}->{}",
        p.victim_kib,
        p.private_kib,
        // `unbekannt` heisst: die Maschine meldet keine Ebene unter dem LLC, die Bedingung ist
        // hier eine ANNAHME. Das als `ja` zu drucken waere Abwesenheit als Erfuellung gelesen.
        match p.victim_over_private {
            Some(true) => "ja",
            Some(false) => "NEIN",
            None => "unbekannt",
        },
        p.attacker_kib,
        p.attacker_pct,
        p.bloecke,
        p.n_lines,
        PROBEN,
        p.base.min,
        p.base.med,
        p.base.max,
        p.disjoint.min,
        p.disjoint.med,
        p.disjoint.max,
        p.shared.min,
        p.shared.med,
        p.shared.max,
        // Eine Nachkommastelle von Hand: auf aarch64 sind es unter TCG rund 0,7 Zyklen je Glied,
        // und `0` waere dort keine Zahl, sondern eine verlorene Aussage.
        je_glied(p.base.med, p.n_lines).0,
        je_glied(p.base.med, p.n_lines).1,
        je_glied(p.disjoint.med, p.n_lines).0,
        je_glied(p.disjoint.med, p.n_lines).1,
        je_glied(p.shared.med, p.n_lines).0,
        je_glied(p.shared.med, p.n_lines).1,
        p.farbtreu as u8,
        p.balanced as u8,
        p.frag_vor,
        p.frag_nach
    );
    // Reihenfolge mit Absicht: was KAPUTT ist, wird zuerst gemeldet. Ein SKIP, der einen
    // Bilanzfehler verdeckt, waere genau die Stille, die hier wie Erfolg aussieht.
    if !p.balanced {
        println!(
            "pprobe  : FAILURES (Speicher NICHT vollstaendig zurueck -- mindestens ein \
             Rueckspeicherblock ist nach `free` nicht wieder frei; die nachfolgenden Tests sehen \
             damit eine veraenderte Ausgangslage)"
        );
        return;
    }
    if !p.farbtreu {
        println!(
            "pprobe  : FAILURES (Farbwahl gebrochen: Opfer und gleichfarbiger Angreifer muessen \
             Farben TEILEN, der disjunkte Angreifer mit keinem von beiden eine teilen. Ist das \
             verletzt, messen die drei Messpunkte nicht, was ihre Namen sagen)"
        );
        return;
    }
    if !p.clock_ok {
        println!(
            "pprobe  : SKIP -- die UHR hat fuer diese Kettenlaenge keine Aufloesung: ein \
             ungestoerter Durchlauf ueber {} Glieder dauerte 0 Zaehlerschritte. `cycles()` ist \
             auf aarch64 CNTPCT_EL0 (architektonischer Zaehler) und auf x86 der TSC; tickt er \
             waehrend der Messung nicht, ist jede weitere Zahl Rundungsrauschen. Das ist ein \
             Befund ueber die Uhr, nicht ueber den Cache -- eine laengere Kette oder eine \
             feinere Zeitquelle waere noetig",
            p.n_lines
        );
        return;
    }
    // **Untergrenze der Physik** (2026-08-02). Ein Kettenglied ist eine abhaengige Ladeoperation:
    // sie kostet auf JEDER echten Maschine mindestens die L1-Trefferlatenz, also einige Zyklen.
    // Misst der Lauf weniger, misst er keinen Speicher, sondern eine **Emulation** -- unter TCG
    // haengt die Zeit an der Befehlszahl, nicht an der Speicherhierarchie.
    //
    // Der Fall ist real aufgetreten: auf aarch64 unter Last standen 0,6 / 1,1 / 1,3 Zyklen je
    // Glied im Bericht, die Positivkontrolle "trug" (Median 5456 gegen 2648) und die Verteilungen
    // waren "trennbar" -- der Test faellte daraufhin ein Urteil ueber die Wirkung der Faerbung auf
    // einer Maschine, die gar keinen Cache hat. Ergebnis: `pprobe : FAILURES`, und weil
    // `PPROBE_OK` in `all_done()` steht, ein Lauf in der Notbremse.
    //
    // Die Positivkontrolle allein reicht als Waechter also NICHT: unter Emulation ist ein
    // groesserer Arbeitssatz zuverlaessig langsamer, und zwar aus dem falschen Grund. Deshalb
    // diese zweite, von der Messung unabhaengige Bedingung -- sie fragt nicht "ist ein Unterschied
    // da", sondern "kann diese Zahl ueberhaupt eine Speicherlatenz sein".
    let (ganz, _) = je_glied(p.base.med, p.n_lines);
    if ganz < MIN_ZYKLEN_JE_GLIED {
        println!(
            "pprobe  : SKIP -- die Zahlen sind keine Speicherlatenzen: ein ungestoertes Glied \
             kostete {} Zyklen, unter der Untergrenze von {} (eine abhaengige Ladeoperation kostet \
             auf echter Hardware mindestens die L1-Trefferlatenz). Gemessen wurde eine EMULATION, \
             deren Zeit an der Befehlszahl haengt -- dort ist ein groesserer Arbeitssatz \
             zuverlaessig langsamer, und die Positivkontrolle traegt aus dem falschen Grund",
            ganz, MIN_ZYKLEN_JE_GLIED
        );
        return;
    }
    if !p.sensitive {
        println!(
            "pprobe  : SKIP -- die Positivkontrolle traegt nicht: der GLEICHFARBIGE Angreifer hat \
             nicht messbar verdraengt (Median gleichfarbig={} vs ungestoert={}). Auf einer \
             Maschine, die Verdraengung nicht zeigen kann (TCG hat keinen echten Cache), ist ueber \
             den disjunkten Fall nichts auszusagen -- 'kein Unterschied' waere kein Beleg, sondern \
             ein Artefakt",
            p.shared.med, p.base.med
        );
        return;
    }
    if p.victim_over_private == Some(false) {
        println!(
            "pprobe  : SKIP -- das Opfer ({} KiB) ueberschreitet die nicht partitionierte \
             Cache-Ebene ({} KiB) NICHT. Faerbung partitioniert nur den LLC; ein Opfer, das in \
             die private Ebene passt, wird von JEDEM Angreifer daraus verdraengt -- gemessen \
             wuerde diese Ebene, nicht die Partitionierung. Die Frage ist hier nicht beantwortet, \
             sondern falsch gestellt",
            p.victim_kib, p.private_kib
        );
        return;
    }
    if !p.resolved {
        println!(
            "pprobe  : SKIP -- nicht aufloesbar: die Verteilungen von ungestoert und gleichfarbig \
             ueberlappen (q3(ungestoert)={} >= q1(gleichfarbig)={}). Der Medianvergleich waere \
             hier eine Zahl ohne Aussage; wer daraus ein Urteil ableitet, raet",
            p.base.q3, p.shared.q1
        );
        return;
    }
    if p.guest {
        println!(
            "pprobe  : SKIP -- unter einem Hypervisor NICHT ENTSCHEIDBAR (CPUID.1:ECX[31]). Die \
             Positivkontrolle traegt (gleichfarbig={} vs ungestoert={}), der disjunkte Farbsatz \
             schuetzt {} (disjunkt={}). Das entscheidet ueber A1 NICHTS: ein Gast faerbt \
             GASTphysische Adressen, und die zweite Uebersetzungsstufe bildet jede 4-KiB-Seite auf \
             eine beliebige Wirtsseite ab -- die Farbbits liegen oberhalb des Seitenoffsets und \
             ueberleben das nicht. Gemessen wuerde die Seitenzuteilung des WIRTS. Belegbar ist die \
             WIRKUNG von A1 nur auf Blech (oder mit wirtsseitig farberhaltender Hinterlegung, \
             z. B. 1-GiB-Seiten)",
            p.shared.med,
            p.base.med,
            if p.effect { "" } else { "NICHT" },
            p.disjoint.med
        );
        return;
    }
    println!(
        "pprobe  : {} (B-4.5: disjunkte Farbsaetze verdraengen einander messbar weniger -- die \
         WIRKUNG von A1, nicht nur die Zuteilung; Positivkontrolle traegt und die Verteilungen \
         sind trennbar)",
        if p.ok { "ALL PASS" } else { "FAILURES" }
    );
}

// ================================================================================================
// A1 / Z11c: der Farbnachweis fuer eine GELADENE PD (2026-08-07)
// ================================================================================================
//
// Bis heute galt A1 nur fuer `spawn_isolated_colored` -- eine PD, die der Kernel selbst erzeugt.
// Der REGULAERE Weg, ein Programm auf diesen Knoten zu bringen, ist aber der Lader, und der war
// ungefaerbt. Genau das stand als offener Punkt in A1: „solange `spawn_isolated` der Normalfall
// ist, ist A1 im Normalbetrieb *nicht* wirksam."
//
// **Warum die Messung nicht aus dem Ladepfad kommt.** Der Lader hat die Frames selbst aus dem
// gefaerbten Allokator geholt; ihn zu fragen, ob sie gefaerbt sind, hiesse, einen Schreiber sein
// eigenes Ergebnis bestaetigen zu lassen. Gelesen wird deshalb die **Teardown-Buchhaltung**
// (`system::loaded_frames_of`): was dort steht, ist das, was der PD gehoert -- dieselbe Quelle,
// aus der die Freigabe kommt.
//
// **Die Positivkontrolle ist der wichtigere Teil.** „Alle Frames dieser PD liegen im Streifen"
// ist trivial wahr, wenn die PD gar keine Frames hat oder wenn es nur eine Farbe gibt. Beides
// wird getrennt gemeldet, und beides macht die Zeile zu SKIP statt zu ALL PASS. Dazu die
// Gegenprobe an einer UNGEFAERBTEN PD: liegen deren Frames zufaellig auch alle in einem Streifen,
// misst der Test nicht die Faerbung, sondern die Groesse der Zuteilung.

/// Ergebnis von [`run_pd_color`].
#[cfg(feature = "selftest")]
#[derive(Default)]
pub struct PdColorTest {
    /// Farbanzahl der Maschine.
    pub colors: u32,
    /// Frames der gefaerbten PD (Stuecke, nicht Seiten).
    pub frames_gefaerbt: usize,
    /// Seiten der gefaerbten PD.
    pub seiten_gefaerbt: usize,
    /// Liegen ALLE Seiten der gefaerbten PD in ihrem Streifen?
    pub in_maske: bool,
    /// Wie viele verschiedene Farben belegt die gefaerbte PD?
    pub farben_gefaerbt: u32,
    /// Seiten der ungefaerbten Vergleichs-PD (0 = keine da).
    pub seiten_ungefaerbt: usize,
    /// Wie viele Farben fasst ein Streifen?
    pub farben_je_streifen: u32,
    /// Laege die UNGEFAERBTE PD zufaellig auch im Streifen der gefaerbten? (Beobachtung -- bei
    /// kleinen PDs kommt das vor und macht die Messung dieses Laufs schwaecher, aber nicht falsch.)
    pub ungefaerbt_im_streifen: bool,
    /// Teilen die beiden PDs keine einzige Farbe?
    pub disjunkt: bool,
    /// Ist die Frage auf dieser Maschine ueberhaupt entscheidbar?
    pub entscheidbar: bool,
    pub ok: bool,
}

/// Den Farbsatz der Frames einer `asid` einsammeln: `(Seiten, Farbmaske als Bitfeld ueber
/// `MASK_BITS`, alle-in-`mask`)`.
#[cfg(feature = "selftest")]
fn farben_der_asid(asid: u16, mask: Option<ColorMask>) -> (usize, u64, bool) {
    let colors = count();
    let mut frames = [(0u64, 0u64); 64];
    let n = crate::system::loaded_frames_of(asid, &mut frames);
    let (mut seiten, mut bits, mut drin) = (0usize, 0u64, true);
    for &(base, len) in &frames[..n] {
        let mut off = 0u64;
        while off < len {
            let p = base + off;
            let c = color_of(p, colors);
            bits |= 1u64 << (c % caprock_mem::MASK_BITS);
            if let Some(m) = mask {
                if !m.contains(c) {
                    drin = false;
                }
            }
            seiten += 1;
            off += PAGE;
        }
    }
    (seiten, bits, drin)
}

/// **A1 fuer den regulaeren Weg**: liegen die Frames einer gefaerbt geladenen PD in ihrem
/// Streifen, und teilt sie mit einer anderen PD keine Farbe?
#[cfg(feature = "selftest")]
pub fn run_pd_color(asid_gefaerbt: u16, mask: ColorMask, asid_ungefaerbt: u16) -> PdColorTest {
    let mut t = PdColorTest { colors: count(), ..Default::default() };
    // **Entscheidbarkeit zuerst.** Unter zwei Farben gibt es nichts zu trennen, und ohne Frames
    // gibt es nichts zu messen -- in beiden Faellen waere „alle liegen im Streifen" wahr, ohne
    // etwas belegt zu haben.
    let (seiten, bits_g, drin) = farben_der_asid(asid_gefaerbt, Some(mask));
    t.seiten_gefaerbt = seiten;
    t.in_maske = drin;
    t.farben_gefaerbt = bits_g.count_ones();
    let (seiten_u, bits_u, _) = farben_der_asid(asid_ungefaerbt, None);
    t.seiten_ungefaerbt = seiten_u;
    let je_streifen = (count().min(caprock_mem::MASK_BITS) / PARTITIONS).max(1);
    t.farben_je_streifen = je_streifen;
    t.ungefaerbt_im_streifen = seiten_u > 0 && (bits_u & !maske_bits(mask)) == 0;
    t.disjunkt = (bits_g & bits_u) == 0;
    // **Wann die Messung ueberhaupt etwas sagen kann -- und was der erste Entwurf falsch machte.**
    //
    // Dort lautete die Gegenprobe „die ungefaerbte PD belegt MEHR Farben, als ein Streifen fasst".
    // Das war nicht erfuellbar: die geladenen Programme sind klein (gemessen 2 und 5 Seiten), und
    // 2 Seiten koennen nie mehr als 16 Farben belegen. Das Kriterium fiel durch, unabhaengig
    // davon, ob die Faerbung traegt -- ein Pruefer, der nicht bestehen KANN, misst nichts.
    //
    // Was hier wirklich traegt, ist `in_maske` selbst, und seine Kraft steht in der Zeile: die
    // gefaerbte PD haelt N Seiten in einem Streifen von `je_streifen` aus `colors` Farben. Ein
    // ungefaerbter Lauf traefe das nur zufaellig, und wie zufaellig, sagt das Verhaeltnis. Nicht
    // entscheidbar ist es dagegen, wenn
    //   * die Maschine unter zwei Farben hat (nichts zu trennen),
    //   * der Streifen ALLE Farben umfasst (die Bedingung waere leer),
    //   * die gefaerbte PD weniger als zwei Seiten hat oder alle auf einer Farbe liegen
    //     (dann ist „alle im Streifen" eine Aussage ueber eine einzige Seite).
    t.entscheidbar =
        usable() && je_streifen < count() && seiten >= 2 && t.farben_gefaerbt >= 2;
    t.ok = t.entscheidbar && t.in_maske;
    t
}

/// Die Farbmaske als Bitfeld ueber `MASK_BITS` -- dieselbe Verdichtung wie in [`farben_der_asid`].
#[cfg(feature = "selftest")]
fn maske_bits(mask: ColorMask) -> u64 {
    let mut b = 0u64;
    for c in 0..count() {
        if mask.contains(c) {
            b |= 1u64 << (c % caprock_mem::MASK_BITS);
        }
    }
    b
}

/// Die Berichtszeile zu [`run_pd_color`] -- **eine** Druckstelle (s. den `FAIL`/`FAILURES`-Fehler).
#[cfg(feature = "selftest")]
pub fn report_pd_color(t: &PdColorTest) {
    println!(
        "pdcolor : Farben={} Streifen={} Farben | gefaerbte-PD: {} Seiten in {} Farbe(n), \
         alle-im-Streifen={} | ungefaerbte-PD: {} Seiten, zufaellig-auch-im-Streifen={}, \
         disjunkt={}",
        t.colors,
        t.farben_je_streifen,
        t.seiten_gefaerbt,
        t.farben_gefaerbt,
        t.in_maske,
        t.seiten_ungefaerbt,
        t.ungefaerbt_im_streifen,
        t.disjunkt
    );
    if !t.entscheidbar {
        println!(
            "pdcolor : SKIP -- nicht entscheidbar auf dieser Maschine ({} Farben, {} bzw. {} \
             Seiten). Eine Aussage ueber Trennung braucht mindestens zwei Farben UND zwei PDs mit \
             Frames; ohne beides waere ALL PASS eine leere Beobachtung",
            t.colors, t.seiten_gefaerbt, t.seiten_ungefaerbt
        );
        return;
    }
    println!(
        "pdcolor : {} (A1 auf dem REGULAEREN Weg: eine ueber das Manifest als EXCLUSIVE_STRIPE \
         geladene PD haelt ihre Segmente, ihren Stack und ihre Seitentabellen in EINEM \
         Farbstreifen -- {} Seiten in {} von {} Farben. Gemessen an der TEARDOWN-Buchhaltung, \
         nicht am Ladepfad: ein Lader, der bestaetigt, was er selbst getan hat, bestaetigt nichts. \
         Ein ungefaerbter Lauf traefe diesen Streifen nur zufaellig -- vor dem 2026-08-07 wies der \
         Lader die Politik ueberhaupt ab, die Zeile meldete dann SKIP statt ALL PASS)",
        if t.ok { "ALL PASS" } else { "FAILURES" },
        t.seiten_gefaerbt,
        t.farben_je_streifen,
        t.colors
    );
}
