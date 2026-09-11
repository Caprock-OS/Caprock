//! Variabler PD-Cspace: Vergabe, Absagen, Freigabe (todo.md A3, TODO0 K1c).
//!
//! `NCAPS = 16` war eine harte Schranke: der lokale Cspace einer PD war ein Array
//! `[Option<CapPtr>; 16]` in `Pd`, und `install_cap` wies jeden Slot `>= 16` ab, ohne
//! das Budget je zu fragen. Eine Treiber-PD mit dreissig Caps — oder Multi-Vektor-MSI-X
//! mit je einem Irq- und einem Notification-Cap pro Vektor — passte nicht, gleich was das
//! Budget erlaubte. Das Budget-Konto hob die erreichbare Zahl von 8 auf 16; darueber
//! hinaus braucht es einen Cspace, dessen Groesse je PD gewaehlt wird.
//!
//! Diese Datei ist die **reine Buchhaltung** davon und haengt an nichts: nur `core`,
//! kein `alloc`, kein `unsafe`. Sie laeuft deshalb per `rustc --test` als Datei — Muster
//! `cycles.rs`/`redirect.rs`/`spawncheck.rs` — waehrend `caprock-microkit` als Ganzes an
//! `caprock-hal` (arch-Asm) haengt und auf dem Host nie baut. Was hier steht, ist die
//! **eine Quelle**: `PdTable` ruft es auf, statt die Regeln nachzubauen (Fallenliste:
//! „Zuteiler und Pruefer brauchen EINE Quelle").
//!
//! Aufteilung, und warum sie so liegt:
//!
//! * [`anforderung_aufloesen`] — reine Funktion ueber eingespeisten Werten: Rohwunsch
//!   (`0` = Vorgabe) gegen Deckel und Struktur. Kein Zustand, keine Tabelle.
//! * [`Vergabe`] — der Pool-Zustand (Bump + Freiliste) ueber einer injizierten
//!   [`AnkerAblage`]. Die Produktion legt die Anker in die (ungenutzten) `Pd`-Eintraege,
//!   der Test in einen `Vec`. Der Algorithmus steht genau einmal hier.
//! * [`CspaceAbweisung`] — jede Absage hat einen Namen (D11-Form): benannt, gezaehlt
//!   (die Zaehler stehen in `PdTable`), nie blockierend.
//!
//! Was diese Datei NICHT tut: Pool-Slots loeschen. Die Freigabe gibt nur den Lauf an die
//! Buchhaltung zurueck; die Slots selbst nullt der Halter des Pools (`PdTable::free`),
//! der als einziger den Elementtyp kennt. Zwei Haelften einer Freigabe an zwei Stellen
//! ist ein Riss — deshalb steht die Reihenfolge (erst nullen, dann zurueckgeben) an
//! `PdTable::free` und nicht hier.

/// Standardgroesse eines PD-Cspace in Slots — 16, der alte `NCAPS`-Wert als Zahl.
///
/// Frueher war das die harte Schranke (Array-Laenge in `Pd`); jetzt ist es die
/// **Vorgabe** (`plaetze == 0` heisst „Standard"). Bestehende 16er-PDs verhalten sich
/// bitgleich zu frueher: dieselbe Groesse, dasselbe Budget, dieselben Schranken.
pub const STANDARD_PLAETZE: u32 = 16;

/// Benannte Absagen der Cspace-/Budget-Vergabe (D11-Form).
///
/// Jede Schranke, die eine PD-Erzeugung ablehnen kann, hat hier einen eigenen Namen mit
/// den Zahlen, die die Behebung braucht. Eine Sammelabsage (`None`) machte „Deckel zu
/// niedrig" und „Speicher alle" ununterscheidbar — und die Behebungen sind verschieden
/// (Deckel anheben gegen Pool vergroessern gegen weniger anfordern). Wer sie
/// zusammenwirft, schickt den Leser in die falsche Richtung.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CspaceAbweisung {
    /// Das verlangte Budget liegt ueber dem Deckel der Tabelle. Behebung: weniger
    /// anfordern — oder den Deckel anheben (`PdTable::set_budget_deckel`), wenn die
    /// PD es wert ist (Treiber-PD). Wird **nicht** still gedeckelt: eine Schranke, die
    /// klammheimlich kuerzt, gibt dem Aufrufer eine PD, die weniger kann, als er
    /// angefordert hat, und laesst ihn das erst beim dreissigsten Cap merken.
    DeckelUeberschritten {
        /// Verlangtes Budget (nach Vorgabe-Aufloesung).
        verlangt: u32,
        /// Geltender Deckel.
        deckel: u32,
    },
    /// Das Budget passt nicht in die gewuenschten Plaetze — die **unerfuellbare Zusage**.
 ///
    /// Das ist der dynamische Nachfolger des entfernten
    /// `const assert!(CAP_BUDGET_MAX <= NCAPS)`: ein Budget, das der Cspace nicht fassen
    /// kann, ist kein grosszuegiges Budget, sondern gar keins (2026-08-26: Budget 20 bei
    /// 16 Slots scheiterte an der Struktur, und die Absage trug nicht einmal den Grund
    /// „Budget"). Behebung: mehr Plaetze anfordern.
    BudgetPasstNicht {
        /// Aufgeloestes Budget.
        budget: u32,
        /// Aufgeloeste Plaetze.
        plaetze: u32,
    },
    /// Mehr als der Budget-Vorrat hergibt. Andere Lage als „kein freier PD-Slot", andere
    /// Behebung — deshalb getrennt gezaehlt und benannt.
    VorratErschoepft {
        /// Verlangtes Budget.
        verlangt: u32,
        /// Noch freier Vorrat.
        vorrat: u32,
    },
    /// Kein freier PD-Slot in der Tabelle. Bisher ein blosses `None` ohne Namen; benannt
    /// ist es erst hier — eine Absage ohne Namen ist im Bericht von „nie versucht" nicht
    /// zu unterscheiden.
    KeinePdFrei,
    /// Der Cspace-Pool gibt die Plaetze nicht mehr her (weder Freiliste noch frischer
    /// Rest). Behebung: Pool vergroessern (Boot-RAM) — oder kleinere PDs bauen.
    PoolErschoepft {
        /// Verlangte Plaetze.
        verlangt: u32,
        /// Noch unverbrauchter Rest hinter dem Bump (pessimistisch: wiederverwendbare
        /// Freilisten-Laeufe sind nicht eingerechnet — sie sind stueckelbar nur als
        /// Ganzes, s. [`Vergabe::belegen`]).
        frei: u32,
    },
}

/// Aufgeloeste PD-Anforderung: so viele Budget-Slots, so viele Cspace-Plaetze.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Anforderung {
    /// Gleichzeit belegbare Cap-Slots dieser PD (gebucht gegen den Vorrat).
    pub budget: u32,
    /// Cspace-Plaetze dieser PD (Lauf im Pool). Immer `>= budget`.
    pub plaetze: u32,
}

/// Einen Rohwunsch gegen Deckel und Struktur pruefen — reine Funktion.
///
/// `0` heisst jeweils „die Vorgabe" (`budget`: `standard_budget`, `plaetze`:
/// [`STANDARD_PLAETZE`]) — damit ist jeder vorhandene Aufrufer bitgleich, und die
/// Vertraeglichkeit steckt in der Kodierung statt in einem Zweig, den jemand im Kopf
/// behalten muss (dieselbe Kodierung wie `create_mit_budget` seit jeher).
///
/// Reihenfolge, und sie ist festgelegt: erst die Policy ([`CspaceAbweisung::DeckelUeberschritten`]),
/// dann die Struktur ([`CspaceAbweisung::BudgetPasstNicht`]). Ein Doppelfehler bekommt
/// den ersten Namen — wer beides falsch hat, behebt zuerst die Policy.
///
/// Vorrat und Pool prueft diese Funktion **nicht**: der Vorrat ist ein Vergleich, den der
/// Halter in einer Zeile selbst tut, und der Pool braucht die Freiliste
/// ([`Vergabe::belegen`] ist dafuer die autoritative Stelle — ein zweiter
/// Pool-Blick hier waere die zweite Wirklichkeit).
pub fn anforderung_aufloesen(
    budget_roh: u32,
    plaetze_roh: u32,
    deckel: u32,
    standard_budget: u32,
) -> Result<Anforderung, CspaceAbweisung> {
    let budget = if budget_roh == 0 {
        standard_budget
    } else {
        budget_roh
    };
    let plaetze = if plaetze_roh == 0 {
        STANDARD_PLAETZE
    } else {
        plaetze_roh
    };
    if budget > deckel {
        return Err(CspaceAbweisung::DeckelUeberschritten {
            verlangt: budget,
            deckel,
        });
    }
    if budget > plaetze {
        return Err(CspaceAbweisung::BudgetPasstNicht { budget, plaetze });
    }
    Ok(Anforderung { budget, plaetze })
}

/// Ein Cspace-Lauf: zusammenhaengende Pool-Plaetze `[start, start + len)`.
///
/// `naechster` verkettet freie Laeufe (Index + 1 des naechsten, `0` = Kettenende) und ist
/// nur gueltig, solange der Eintrag **frei** ist. Die Produktion haelt einen Anker je
/// PD-Eintrag vor (im ungenutzten Eintrag ist er die Freilisten-Verkettung); vergeben
/// beschreibt ihn den Lauf, freigegeben haengt ihn ein.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CspaceAnker {
    /// Erster Pool-Slot des Laufs.
    pub start: u32,
    /// Laenge des Laufs in Slots.
    pub len: u32,
    /// Freilisten-Verkettung (Index + 1, `0` = Ende). Nur fuer freie Eintraege.
    pub naechster: u32,
}

impl CspaceAnker {
    /// Leerer Anker — belegt nichts, verkettet nichts.
    pub const LEER: CspaceAnker = CspaceAnker {
        start: 0,
        len: 0,
        naechster: 0,
    };
}

/// Ablage der Anker — die Naht, ueber der [`Vergabe`] ohne Tabelle auskommt.
///
/// Die Produktion implementiert sie auf ihren PD-Eintraege (ein Anker je Eintrag, ohne
/// einen Byte Mehr-RAM), der Test auf einem `Vec`. Der Algorithmus steht genau einmal in
/// [`Vergabe`]; was hier verwechselt wuerde (falscher Index), faellt als falscher Lauf
/// auf, nicht als stille Wiedervergabe: `belegen` vergibt nur Laeufe, `freigeben` haengt
/// nur den eigenen Index ein.
pub trait AnkerAblage {
    /// Zahl der Anker-Eintraege (Produktion: PD-Kapazitaet).
    fn anzahl(&self) -> usize;
    /// Anker `i` lesen. Nur mit Indizes rufen, die es gibt — wie `cap_at` vertraut auch
    /// diese Schicht ihren Aufrufern (die Kette entsteht aus gueltigen Indizes).
    fn lese(&self, i: usize) -> CspaceAnker;
    /// Anker `i` schreiben.
    fn schreibe(&mut self, i: usize, a: CspaceAnker);
}

/// Der Pool-Zustand: Bump plus Freiliste.
///
/// Der Pool selbst (die Slots) gehoert dem Halter; hier steht nur, was noch zu vergeben
/// ist: `bump` ist der erste noch nie vergebene Slot, `kopf` die Kette zurueckgegebener
/// Laeufe (Index + 1, `0` = leer). Frische Laeufe kommen vom Bump, wiederverwendete aus
/// der Liste — in dieser Reihenfolge: ein freier Lauf kostet kein frisches Pool-Stueck,
/// und wer ihn liegen liesse, liesse Speicher liegen, den er schon bezahlt hat.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Vergabe {
    bump: u32,
    kopf: u32,
}

impl Vergabe {
    /// Leere Vergabe: nichts vergeben, nichts zurueckgegeben. `const`, damit statische
    /// Tabellen moeglich bleiben.
    pub const fn neu() -> Self {
        Vergabe { bump: 0, kopf: 0 }
    }

    /// Erster noch nie vergebener Pool-Slot — Auskunft fuer den Bericht. Was davor liegt,
    /// ist vergeben oder war es einmal (Freiliste); was dahinter liegt, ist frisch.
    pub fn bump_stand(&self) -> u32 {
        self.bump
    }

    /// Einen Lauf der Laenge `n` belegen: `(start, len)`.
    ///
    /// Zuerst first-fit ueber die Freiliste, dann frisch vom Bump. Ein wiederverwendeter
    /// Lauf wird **als Ganzes** vergeben (`len` darf groesser sein als `n`): ein Rest
    /// braeuchte einen zweiten freien Anker, und den gibt es nicht sicher — Ueberversorgung
    /// ist ehrlich (die gemeldete Laenge stimmt), Stueckelung ohne Ablage waere ein Leck.
    /// Wer genaue Laengen braucht, fordert genaue Laengen an; was er bekommt, steht in
    /// der Rueckgabe, nicht in der Anforderung.
    ///
    /// `n == 0` vergibt nichts und gibt `(bump, 0)`: eine PD ohne Plaetze — sie kann keine
    /// Cap halten, und jede Installation weist die Schranke ab. Kein Fehler, nur leer.
    ///
    /// Absage: [`CspaceAbweisung::PoolErschoepft`] — benannt, mit den Zahlen fuer die
    /// Behebung. Nie blockierend (D11): der Aufrufer bekommt die Absage als Wert.
    pub fn belegen(
        &mut self,
        ablage: &mut impl AnkerAblage,
        pool_len: u32,
        n: u32,
    ) -> Result<(u32, u32), CspaceAbweisung> {
        if n == 0 {
            return Ok((self.bump, 0));
        }
        // --- Freiliste: first-fit, ganzer Lauf -----------------------------------------
        // `vorgaenger` haelt den Anker-Index + 1, dessen `naechster` gerade verfolgt wird;
        // `0` heisst „der Kopf selbst". So laesst sich das gefundene Glied aushaket, ohne
        // den Kopf als Sonderfall zu behandeln.
        let mut vorgaenger: u32 = 0;
        let mut i = self.kopf;
        while i != 0 {
            let idx = (i - 1) as usize;
            let a = ablage.lese(idx);
            if a.len >= n {
                let weiter = a.naechster;
                if vorgaenger == 0 {
                    self.kopf = weiter;
                } else {
                    let mut v = ablage.lese((vorgaenger - 1) as usize);
                    v.naechster = weiter;
                    ablage.schreibe((vorgaenger - 1) as usize, v);
                }
                // Der Anker beschreibt ab hier den vergebenen Lauf; die Verkettung ist
                // verbraucht. Wer sie stehen liesse, haette einen vergebenen Eintrag in
                // der Freikette — und die naechste Vergabe veraeusserte ihn zweimal.
                ablage.schreibe(idx, CspaceAnker {
                    start: a.start,
                    len: a.len,
                    naechster: 0,
                });
                return Ok((a.start, a.len));
            }
            vorgaenger = i;
            i = a.naechster;
        }
        // --- Frisch vom Bump -------------------------------------------------------------
        let ende = self.bump.checked_add(n).ok_or(CspaceAbweisung::PoolErschoepft {
            verlangt: n,
            frei: pool_len.saturating_sub(self.bump),
        })?;
        if ende > pool_len {
            return Err(CspaceAbweisung::PoolErschoepft {
                verlangt: n,
                frei: pool_len.saturating_sub(self.bump),
            });
        }
        let start = self.bump;
        self.bump = ende;
        Ok((start, n))
    }

    /// Einen Lauf zurueckgeben — die Buchhaltungs-Haelfte der Freigabe.
    ///
    /// Haengt `idx` mit seinem in `ablage` beschriebenen Lauf an die Freiliste. Die Slots
    /// selbst ruehrt das nicht an: wer sie weiterlesen koennte, laese fremde Caps — nullen
    /// muss der Halter des Pools **vorher** (`PdTable::free` tut das). Eine Freigabe, die
    /// die Slots nicht loescht, ist keine.
    pub fn freigeben(&mut self, ablage: &mut impl AnkerAblage, idx: usize) {
        let mut a = ablage.lese(idx);
        a.naechster = self.kopf;
        ablage.schreibe(idx, a);
        self.kopf = (idx as u32) + 1;
    }

    /// Ein wiederbelebtes Kettenglied loesen — Doppelvergabe-Schutz bei Slot-Wiederverwendung.
    ///
    /// Der Anker lebt IM Tabelleneintrag: wird der Slot wiederverwendet, ohne sein
    /// Kettenglied zu entfernen, beschreibt das stehengebliebene Glied den NEUEN Lauf
    /// als frei — die naechste Vergabe haendigt ihn ein zweites Mal aus (zwei PDs, ein
    /// Lauf; gemessen 2026-09-11 auf ARM: PD 0/1 teilten `[32..48)`, ipc/reload rot,
    /// Cap-Zaehlung beider PDs identisch). Wer einen Slot wiederbelebt (alle
    /// `create`-Pfade), entkettet ihn ZUERST und verwendet den geloesten Lauf weiter,
    /// wenn er passt (kein Leck); ein zu kleiner gelooster Lauf verfällt (einheitliche
    /// Groessen lassen das praktisch nie eintreten).
    ///
    /// Rueckgabe: der geloeste Anker (`None`, wenn `idx` nicht verkettet war). Schritt-
    /// begrenzt (`anzahl + 1`): eine zyklische Kette haelt hier an, statt zu haengen.
    pub fn entketten(
        &mut self,
        ablage: &mut impl AnkerAblage,
        idx: usize,
    ) -> Option<CspaceAnker> {
        if idx >= ablage.anzahl() {
            return None;
        }
        let marke = idx as u32 + 1;
        // Kopf-Glied: Kopf umhaengen, Anker neutralisieren (kein freier Lauf mehr).
        if self.kopf == marke {
            let mut a = ablage.lese(idx);
            self.kopf = a.naechster;
            a.naechster = 0;
            ablage.schreibe(idx, a);
            return Some(a);
        }
        // Vorgaenger suchen — schrittbegrenzt, fail-closed bei Kettenbruch.
        let schranke = ablage.anzahl().saturating_add(1);
        let mut i = self.kopf;
        let mut schritte = 0usize;
        while i != 0 && schritte < schranke {
            schritte += 1;
            let pi = (i - 1) as usize;
            if pi >= ablage.anzahl() {
                return None;
            }
            if ablage.lese(pi).naechster == marke {
                let ziel = ablage.lese(idx).naechster;
                let mut v = ablage.lese(pi);
                v.naechster = ziel;
                ablage.schreibe(pi, v);
                let mut a = ablage.lese(idx);
                a.naechster = 0;
                ablage.schreibe(idx, a);
                return Some(a);
            }
            i = ablage.lese(pi).naechster;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-Ablage: ein Anker je Eintrag, wie die PD-Tabelle — nur ohne Kernel.
    struct TestAblage {
        anker: Vec<CspaceAnker>,
    }

    impl TestAblage {
        fn neu(n: usize) -> Self {
            TestAblage {
                anker: alloc_anker(n),
            }
        }
    }

    fn alloc_anker(n: usize) -> Vec<CspaceAnker> {
        let mut v = Vec::new();
        v.resize(n, CspaceAnker::LEER);
        v
    }

    impl AnkerAblage for TestAblage {
        fn anzahl(&self) -> usize {
            self.anker.len()
        }
        fn lese(&self, i: usize) -> CspaceAnker {
            self.anker[i]
        }
        fn schreibe(&mut self, i: usize, a: CspaceAnker) {
            self.anker[i] = a;
        }
    }

    #[test]
    fn null_heisst_vorgabe() {
        // (0, 0) mit Vorgabe-Budget 10 und Deckel 16: die bitgleiche Alt-PD.
        let a = anforderung_aufloesen(0, 0, 16, 10).unwrap();
        assert_eq!(a, Anforderung {
            budget: 10,
            plaetze: 16
        });
    }

    #[test]
    fn budget_ueber_deckel_wird_benannt_abgewiesen() {
        // Der alte `create_mit_budget`-Pfad: mehr als CAP_BUDGET_MAX ist keine PD.
        let e = anforderung_aufloesen(17, 32, 16, 10).unwrap_err();
        assert_eq!(e, CspaceAbweisung::DeckelUeberschritten {
            verlangt: 17,
            deckel: 16
        });
        // Auch die Vorgabe kann ueber einem gesenkten Deckel liegen — dann trifft es die
        // Standard-PD, und die Absage nennt trotzdem beide Zahlen.
        let e = anforderung_aufloesen(0, 0, 8, 10).unwrap_err();
        assert_eq!(e, CspaceAbweisung::DeckelUeberschritten {
            verlangt: 10,
            deckel: 8
        });
    }

    #[test]
    fn budget_ohne_platz_ist_unerfuellbar() {
        // Der Befund vom 2026-08-26 als Test: Budget 20 bei 16 Plaetzen war keine
        // grosszuegige Zusage, sondern eine, die die Struktur nicht halten konnte.
        // Frueher trug die Absage nicht einmal den Grund „Budget" (sie kam als
        // Slot-Schranke); jetzt traegt sie ihn im Namen.
        let e = anforderung_aufloesen(20, 16, 64, 10).unwrap_err();
        assert_eq!(e, CspaceAbweisung::BudgetPasstNicht {
            budget: 20,
            plaetze: 16
        });
    }

    #[test]
    fn treiber_pd_mit_dreissig_caps_passt() {
        // Die Treiberumgebung aus der Aufgabe: ~30 Caps, Budget 30, 32 Plaetze, Deckel 32.
        let a = anforderung_aufloesen(30, 32, 32, 10).unwrap();
        assert_eq!(a, Anforderung {
            budget: 30,
            plaetze: 32
        });
        // Acht Plaetze Reserve (irq + NTFN je Vektor bei Multi-Vektor-MSI-X) oben drauf:
        // passt ebenfalls, solange der Deckel es deckt.
        let a = anforderung_aufloesen(30, 40, 64, 10).unwrap();
        assert_eq!(a.budget, 30);
        assert_eq!(a.plaetze, 40);
    }

    #[test]
    fn deckel_prueft_vor_struktur() {
        // Beides falsch: die Absage nennt die Policy, nicht die Struktur.
        let e = anforderung_aufloesen(40, 16, 32, 10).unwrap_err();
        assert_eq!(e, CspaceAbweisung::DeckelUeberschritten {
            verlangt: 40,
            deckel: 32
        });
    }

    #[test]
    fn vergabe_und_freigabe() {
        // Pool fuer 48 Plaetze, drei Eintraege: Standard-PD (16), Treiber-PD (32),
        // dann ist der Pool voll — die naechste Anforderung wird benannt abgewiesen.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        let (s0, l0) = v.belegen(&mut ablage, 48, 16).unwrap();
        assert_eq!((s0, l0), (0, 16));
        // Der Anker des vergebenen Laufs steht in der Ablage (die Produktion liest ihn
        // dort fuer jede Slot-Aufloesung).
        ablage.schreibe(0, CspaceAnker {
            start: s0,
            len: l0,
            naechster: 0,
        });
        let (s1, l1) = v.belegen(&mut ablage, 48, 32).unwrap();
        assert_eq!((s1, l1), (16, 32));
        ablage.schreibe(1, CspaceAnker {
            start: s1,
            len: l1,
            naechster: 0,
        });
        assert_eq!(v.bump_stand(), 48);
        let e = v.belegen(&mut ablage, 48, 16).unwrap_err();
        assert_eq!(e, CspaceAbweisung::PoolErschoepft {
            verlangt: 16,
            frei: 0
        });
        // Freigabe der Treiber-PD: der Lauf kommt zurueck, und die naechste Vergabe
        // nimmt ihn wieder — Freigabe wirkt, der Pool verliert nichts.
        v.freigeben(&mut ablage, 1);
        let (s2, l2) = v.belegen(&mut ablage, 48, 32).unwrap();
        assert_eq!((s2, l2), (16, 32));
        assert_eq!(v.bump_stand(), 48);
    }

    #[test]
    fn freiliste_nimmt_nur_passende() {
        // Ein zurueckgegebener 16er-Lauf hilft einer 32er-Anforderung nicht: sie kommt
        // frisch vom Bump, statt den kleinen Lauf zu zerreissen (der als Ganzes gilt).
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        let (s0, l0) = v.belegen(&mut ablage, 64, 16).unwrap();
        ablage.schreibe(0, CspaceAnker {
            start: s0,
            len: l0,
            naechster: 0,
        });
        v.freigeben(&mut ablage, 0);
        let (s1, _) = v.belegen(&mut ablage, 64, 32).unwrap();
        assert_eq!(s1, 16);
        // Und umgekehrt: danach passt eine 16er-Anforderung wieder in den freien Lauf —
        // am alten Start, ohne den Bump zu bewegen.
        let stand = v.bump_stand();
        let (s2, l2) = v.belegen(&mut ablage, 64, 16).unwrap();
        assert_eq!((s2, l2), (0, 16));
        assert_eq!(v.bump_stand(), stand);
    }

    #[test]
    fn pool_erschöpfung_nennt_zahlen() {
        // Knapp bemessener Pool: nach zwei 16er-Laeufen bleiben 8 von 40 — eine 16er-
        // Anforderung scheitert mit verlangt/frei, nicht mit Stille.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        v.belegen(&mut ablage, 40, 16).unwrap();
        v.belegen(&mut ablage, 40, 16).unwrap();
        let e = v.belegen(&mut ablage, 40, 16).unwrap_err();
        assert_eq!(e, CspaceAbweisung::PoolErschoepft {
            verlangt: 16,
            frei: 8
        });
    }

    #[test]
    fn leere_anforderung_vergibt_nichts() {
        let mut ablage = TestAblage::neu(2);
        let mut v = Vergabe::neu();
        let (s, l) = v.belegen(&mut ablage, 48, 0).unwrap();
        assert_eq!((s, l), (0, 0));
        assert_eq!(v.bump_stand(), 0);
    }

    #[test]
    fn entketten_verhindert_doppelvergabe() {
        // Der ARM-Trio-Befund vom 2026-09-11 als Test: Slot 0 freigegeben (8er-Lauf),
        // wiederbelebt mit frischem 16er-Lauf vom Bump (das 8er-Glied passte nicht) —
        // ohne Entketten haendigte die naechste 16er-Vergabe [24,40) ein ZWEITES Mal
        // aus (PD 0/1 teilten den Lauf, ipc/reload rot). Mit Entketten kommt sie
        // frisch vom Bump, und der alte Lauf geht genau einmal raus.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        let (s0, l0) = v.belegen(&mut ablage, 128, 8).unwrap();
        assert_eq!((s0, l0), (0, 8));
        ablage.schreibe(0, CspaceAnker {
            start: s0,
            len: l0,
            naechster: 0,
        });
        let (s1, l1) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!((s1, l1), (8, 16));
        ablage.schreibe(1, CspaceAnker {
            start: s1,
            len: l1,
            naechster: 0,
        });
        // Slot 0 freigeben, dann wiederbeleben (create-Pfad: entketten zuerst).
        v.freigeben(&mut ablage, 0);
        let alt = v.entketten(&mut ablage, 0).expect("verketteter Slot");
        assert_eq!((alt.start, alt.len), (0, 8));
        // Zu klein fuer 16: frisch vom Bump — NICHT der alte Lauf.
        let (s2, l2) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!((s2, l2), (24, 16));
        // Neubelegung beschreibt Eintrag 0 neu (wie create) ...
        ablage.schreibe(0, CspaceAnker {
            start: s2,
            len: l2,
            naechster: 0,
        });
        // ... und die naechste Vergabe teilt ihn NICHT ein zweites Mal aus.
        let (s3, _) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!(s3, 40);
    }

    #[test]
    fn entketten_gibt_passenden_lauf_zurueck() {
        // Passt der geloeste Lauf, verwendet create ihn weiter (kein Leck, kein
        // Bump-Verbrauch) — und die Kette bleibt leer dahinter.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        let (s0, l0) = v.belegen(&mut ablage, 128, 16).unwrap();
        ablage.schreibe(0, CspaceAnker {
            start: s0,
            len: l0,
            naechster: 0,
        });
        v.freigeben(&mut ablage, 0);
        let alt = v.entketten(&mut ablage, 0).expect("verketteter Slot");
        assert!(alt.len >= 16);
        assert_eq!((alt.start, alt.len), (0, 16));
        // Kette leer: die naechste Vergabe kommt vom Bump, nicht aus der Liste.
        let (s, _) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!(s, 16);
    }

    #[test]
    fn ohne_entketten_teilt_vergabe_doppelt_aus() {
        // Der alte Pfad als Negativbeleg (laeuft auf altem wie neuem Code — er benutzt
        // nur belegen/freigeben): Slot 0 freigegeben, wiederbelebt OHNE Entketten.
        // Das stehengebliebene Glied beschreibt den neuen Lauf als frei und teilt ihn
        // ein zweites Mal aus. Genau deshalb muessen alle create-Pfade entketten.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        let (s0, l0) = v.belegen(&mut ablage, 128, 8).unwrap();
        ablage.schreibe(0, CspaceAnker {
            start: s0,
            len: l0,
            naechster: 0,
        });
        let (s1, l1) = v.belegen(&mut ablage, 128, 16).unwrap();
        ablage.schreibe(1, CspaceAnker {
            start: s1,
            len: l1,
            naechster: 0,
        });
        v.freigeben(&mut ablage, 0);
        // Wiederbelebung OHNE Entketten: frischer Lauf vom Bump ...
        let (s2, l2) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!((s2, l2), (24, 16));
        ablage.schreibe(0, CspaceAnker {
            start: s2,
            len: l2,
            naechster: 0,
        });
        // ... und derselbe Lauf geht ein zweites Mal raus (Doppelvergabe).
        let (s3, _) = v.belegen(&mut ablage, 128, 16).unwrap();
        assert_eq!(s3, 24);
    }

    #[test]
    fn entketten_ohne_kette_ist_none() {
        // Nie verketteter Slot: nichts zu loesen, nichts veraendert.
        let mut ablage = TestAblage::neu(4);
        let mut v = Vergabe::neu();
        v.belegen(&mut ablage, 128, 16).unwrap();
        assert!(v.entketten(&mut ablage, 0).is_none());
        assert!(v.entketten(&mut ablage, 9).is_none());
    }
}
