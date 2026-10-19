//! LXPD-Spawn-Entscheidung — reine Glue-Logik zwischen verifiziertem Manifest und Laden.
//!
//! ## Warum diese Datei keine Abhaengigkeit auf `caprock-lxpd` hat
//!
//! Befund in `kernel/Cargo.toml` (Stand 2026-09-09): `[dependencies]` nennt `caprock-loader`,
//! aber **nicht** `caprock-lxpd`. Ein `use caprock_lxpd::...` verlangte eine `Cargo.toml`-Aenderung,
//! und die ist hier verboten (fremde aktive Arbeit im Baum). Darum liegt hier **kein**
//! Transport-Parser, sondern die reine Entscheidungslogik ueber einem generischen
//! [`LxpdFacts`]-Struct als Parameter: HAL-frei (nur `core`), `forbid(unsafe_code)`, per
//! `rustc --test` auf dem Wirt pruefbar — derselbe Weg wie `stack_arena.rs`.
//!
//! Die Fakten kommen aus dem Crate, die Entscheidung faellt hier:
//!
//! * `caprock-lxpd::manifest::manifest_facts` — Feld-Fakten **ohne** Kryptourteil
//!   (`signature_present` ist Anwesenheit, kein Beweis);
//! * `LxpdImage::verify_manifest` — Fakten **plus** Stubnamen-Abgleich gegen den Container
//!   **plus** echte Signaturnachrechnung. Erst dessen `Ok` setzt [`LxpdFacts::verified`].
//!
//! Wer `verified` ohne `verify_manifest` setzt, luegt den Gate an — das erzwingt kein Typ,
//! sondern die Verdrahtung in `loader.rs` (Skizze unten).
//!
//! ## Reihenfolge, und sie ist nicht verhandelbar
//!
//! 1. Verifikations-Gate — unverifiziert wird **nie** geladen ([`LxpdSpawnError::Unverified`]);
//! 2. Manifest-Inhalt — Fassung und Deckung ([`LxpdSpawnError::BadImage`]);
//! 3. Grant-Plausibilitaet — BAR/DMA-Groesse `> 0`, IRQ `!= 0`, Ueberlappungsfreiheit
//!    ([`LxpdSpawnError::BadGrants`], [`LxpdSpawnError::Ueberlappung`]);
//! 4. Budget-Deckel der Policy ([`LxpdSpawnError::BudgetErschoepft`]);
//! 5. Exec-Form — die Teardown-Token-Regel aus `caprock-loader::exec`
//!    ([`LxpdSpawnError::ExecAbgelehnt`]).
//!
//! Erst danach gibt es einen [`SpawnPlan`]: die geprüfte Zuteilung, die `loader.rs` dann
//! privilegiert umsetzt (`system::assign_driver_device` fuer Slots 3–8,
//! `system::load_into_pd_mit` fuer Segmente/Stack/Thread — s. `kernel/src/loader.rs`
//! `load_program_into_pd_auf` und `endow_from_manifest`).
//!
//! ## NICHT angewendeter Integrationspatch fuer `loader.rs` (Skizze, 5 Zeilen)
//!
//! ```rust,ignore
//! // in kernel/src/loader.rs, am LXPD-Ladepfad (NICHT angewendet — nur die Verdrahtung):
//! use crate::lxpd_glue::{spawn_entscheidung, LxpdFacts, LxpdPolicy};
//! let facts = LxpdFacts::aus_verifiziert(&img.verify_manifest(manifest_json, schluessel)?, grant, heap);
//! let plan = spawn_entscheidung(&facts, &policy)?; // erst danach: assign_driver_device + load_into_pd_mit
//! ```
//!
//! (`aus_verifiziert` ist hier bewusst KEIN Konstruktor: der Aufrufer mappt Grant plus
//! Heap-Angebot auf [`LxpdFacts`] — eine Zeile mehr, dafuer steht `verified` nur dort, wo
//! wirklich verifiziert wurde.)

#![forbid(unsafe_code)]

use core::fmt;

// --- Spiegel-Konstanten (Quelle steht dabei; Aenderung dort heisst Aenderung hier) ---

/// Manifest-Fassung, die der Glue versteht (Spiegel von `caprock-lxpd::manifest::MANIFEST_SCHEMA_VERSION`).
pub const LXPD_SCHEMA_VERSION: u32 = 1;
/// Geforderte Deckung in Prozent, strikt (Spiegel von `bind`: darunter wird gar nicht gebunden).
pub const LXPD_COVERAGE_PFLICHT: f64 = 100.0;
/// Obergrenze der User-Adressen (Spiegel von `caprock-loader::exec::USER_VA_TOP`).
pub const USER_VA_TOP: u64 = 1 << 47;
/// Teardown-Buchhaltung: so viele Segmente fasst ein EXEC-Antrag hoechstens
/// (Spiegel von `caprock-loader::exec::MAX_SEGS`).
pub const MAX_SEGS: usize = 64;

// --- Fehler ---

/// Warum ein LXPD-Spawn abgelehnt wurde — jeder Ausgang mit eigenem Namen, nie ein
/// Out-of-Bounds-Zugriff oder Panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LxpdSpawnError {
    /// Kein echtes Verify-Urteil: die Signatur wurde nicht nachgerechnet, das
    /// `signature`-Feld fehlt, oder die Stubnamen-Zahl passt nicht zur Container-Zahl.
    /// Unverifiziert wird **nie** geladen — das ist der Gate, nicht eine Vorpruefung.
    Unverified,
    /// Die Geraete-Zusagen sind leer oder null: keine BAR mit `size > 0`, `dma_size == 0`,
    /// `irq == 0`, oder eine Adresse laeuft ueber (`checked_add` schlaegt fehl — eine
    /// Bereichsangabe, die nicht in `u64` passt, ist keine Angabe).
    BadGrants,
    /// BAR- und DMA-Bereich ueberlappen sich: zwei Zusagen ueber dieselben Bytes sind
    /// keine zwei Zusagen. Angrenzend (`ende == anfang`) ist erlaubt und kein Ueberlapp.
    Ueberlappung,
    /// Die geprüfte Zusage sprengt den Budget-Deckel der Policy (BAR-, DMA- oder Heap-Budget).
    /// Lieber abweisen als still kuenstlich verkleinern — eine verkleinerte Zusage waere eine
    /// erfundene, und ein Treiber, der weniger bekommt als zugesagt, scheitert spaet und ortlos.
    BudgetErschoepft,
    /// Manifest-Inhalt oder Image unbrauchbar: falsche `schema_version`, `coverage_pct != 100.0`,
    /// oder das ELF scheitert an den Loader-Regeln (Magic/Klasse/Typ/Maschine/PHDR,
    /// `memsz < filesz`, nicht-seitenausgerichtetes `PT_LOAD` — s. `ElfImage::parse`).
    BadImage,
    /// Die Exec-Form ist verletzt: `token == 0` (kein Token — kein „egal"),
    /// Token passt nicht zu `(program_id, epoche)` (alter Stand), keine oder zu viele
    /// Segmente, oder der Eintrag liegt ausserhalb der unteren Adresshaelfte (kein Kernel-Sprung).
    /// Geprueft **vor** jedem Rueckbau — ein Ueberrest ist ein Baufehler.
    ExecAbgelehnt,
}

impl LxpdSpawnError {
    /// Fester Fehlertext (keine verstreuten Literale an den Abweisungsstellen).
    pub fn as_str(&self) -> &'static str {
        match self {
            LxpdSpawnError::Unverified => "lxpd-glue: kein verifiziertes Manifest — nie laden",
            LxpdSpawnError::BadGrants => "lxpd-glue: leere oder unplausible Geraete-Zusagen",
            LxpdSpawnError::Ueberlappung => "lxpd-glue: BAR- und DMA-Bereich ueberlappen sich",
            LxpdSpawnError::BudgetErschoepft => "lxpd-glue: Zusage ueber dem Budget-Deckel",
            LxpdSpawnError::BadImage => "lxpd-glue: Manifest-Inhalt oder Image verletzt die Regeln",
            LxpdSpawnError::ExecAbgelehnt => "lxpd-glue: Exec-Form verletzt (Token/Eintrag/Segmente)",
        }
    }
}

impl fmt::Display for LxpdSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- Eingaben: Fakten (Crate-Urteil) und Policy (Kernel-Vorgabe) ---

/// Die verifizierten Fakten eines LXPD-Treibers — generisch, damit der Kernel kein
/// `caprock-lxpd`-Depot braucht (s. Moduldoku).
///
/// `bar_*`/`dma_*`/`irq` stammen aus den Grant-Feldern des Manifests (s.
/// [`grants_aus_manifest`]); `verified` dagegen **nur** aus
/// `LxpdImage::verify_manifest` — Fakten aus `manifest_facts` allein setzen es nie.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LxpdFacts {
    /// Echtes Verify-Urteil: Signatur nachgerechnet UND Stubnamen-Zahl = Container-Zahl.
    pub verified: bool,
    /// Ein `signature`-Feld ist anwesend. Die schwaechere Aussage — fehlt schon sie,
    /// gab es keine Pruefung (niemals ein angenommener Pass).
    pub signature_present: bool,
    /// Manifest-Fassung — muss [`LXPD_SCHEMA_VERSION`] sein.
    pub schema_version: u32,
    /// Stubnamen der Form `src->dst` im Manifest.
    pub trampoline_names: usize,
    /// Stubs im Container — muss zu `trampoline_names` passen (Abgleich aus `verify_manifest`).
    pub tramp_count: usize,
    /// Deckung in Prozent — strikt [`LXPD_COVERAGE_PFLICHT`].
    pub coverage_pct: f64,
    /// BAR-Fenster: Basis und Groesse (Groesse `> 0`, s. Grant-Plausibilitaet).
    pub bar_base: u64,
    /// Siehe [`LxpdFacts::bar_base`].
    pub bar_size: u64,
    /// DMA-Fenster: Basis und Groesse (Groesse `> 0`, ueberlappungsfrei zum BAR-Fenster).
    pub dma_base: u64,
    /// Siehe [`LxpdFacts::dma_base`].
    pub dma_size: u64,
    /// IRQ-Vektor (`!= 0` — Vektor 0 ist kein Vektor, sondern Abwesenheit).
    pub irq: u32,
    /// Angebots-Heap in Seiten. Stammt **nicht** aus dem Manifest (das nennt keinen Heap),
    /// sondern aus der Aufrufer-Angabe — und wird gegen den Deckel geprüft, nicht geglaubt.
    pub heap_pages: u64,
}

/// Der Exec-Antrag dieses Spawns — die Form aus `caprock-loader::exec`, hier als Daten,
/// damit [`spawn_entscheidung`] sie prüfen kann, ohne den Dispatch nachzubauen
/// (eine Pruefung an zwei Stellen ist zwei Pruefungen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecAntrag {
    /// Wessen Stand das Token bindet (mit `epoche` zusammen die Token-Identitaet).
    pub program_id: u32,
    /// Wessen Stand das Token bindet — wer mit dem Token von gestern kommt, bekommt eine
    /// Absage, keinen halb geraeumten Kind-Zustand.
    pub epoche: u32,
    /// Teardown-Token — `0` heisst „kein Token" und wird ABGEWIESEN, nicht als „egal" gelesen.
    pub token: u64,
    /// ELF-Eintrag — muss in der unteren Adresshaelfte liegen (kein Kernel-Sprung).
    pub eintrag: u64,
    /// Segmentzahl — `1..=MAX_SEGS`, geprueft vor jedem Rueckbau.
    pub segmente: usize,
}

/// Die Kernel-Vorgabe dieses Spawns: Budget-Deckel plus Exec-Antrag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LxpdPolicy {
    /// Deckel fuer `bar_size` in Bytes — was darueber liegt, wird abgewiesen, nicht gestutzt.
    pub max_bar_bytes: u64,
    /// Deckel fuer `dma_size` in Bytes.
    pub max_dma_bytes: u64,
    /// Deckel fuer `heap_pages` in Seiten.
    pub max_heap_pages: u64,
    /// Der Exec-Antrag (Token-Bindung an Stand, Eintrag, Segmentzahl).
    pub exec: ExecAntrag,
}

/// Die geprüfte Zuteilung — was [`spawn_entscheidung`] freigibt, setzt `loader.rs`
/// privilegiert um (Geraetezuteilung, Segmente, Thread). Kein neuer Inhalt gegenueber den
/// Fakten, nur das Urteil „geprüft" obendrauf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Zuerkanntes BAR-Fenster: Basis.
    pub bar_base: u64,
    /// Zuerkanntes BAR-Fenster: Groesse.
    pub bar_size: u64,
    /// Zuerkanntes DMA-Fenster: Basis.
    pub dma_base: u64,
    /// Zuerkanntes DMA-Fenster: Groesse.
    pub dma_size: u64,
    /// Zuerkannter IRQ-Vektor.
    pub irq: u32,
    /// Zuerkannter Heap in Seiten.
    pub heap_pages: u64,
}

/// Die Grant-Anfrage aus dem Manifest — Adressen, noch ohne Urteil (das faellt erst in
/// [`spawn_entscheidung`], zusammen mit Verifikation und Budget).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LxpdGrantRequest {
    /// BAR-Basis aus dem ersten `grants_bar`-Eintrag.
    pub bar_base: u64,
    /// BAR-Groesse aus dem ersten `grants_bar`-Eintrag.
    pub bar_size: u64,
    /// DMA-Basis aus `dma_window`.
    pub dma_base: u64,
    /// DMA-Groesse aus `dma_window`.
    pub dma_size: u64,
    /// IRQ-Vektor aus `irq_vector`.
    pub irq: u32,
    /// Immer `0` — das Manifest nennt keinen Heap; ihn setzt die Policy, nicht das Dokument.
    /// (Wer hier eine Zahl erfaende, erfuelle eine Bitte, die niemand ausgesprochen hat.)
    pub heap_pages: u64,
}

// --- Teardown-Token (Spiegel von `caprock-loader::exec`) ---

/// Epoche + Programm-ID falten zu einem Token, das kein gueltiger „egal"-Wert ist.
/// Byte-identischer Spiegel von `caprock-loader::exec::teardown_token_fuer` (FNV-1a ueber
/// 8 Bytes, danach `0` verboten) — steht hier, damit die Glue-Datei abhaengigkeitsfrei bleibt.
pub fn teardown_token_fuer(program_id: u32, epoche: u32) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let bytes = [
        (program_id & 0xff) as u8,
        ((program_id >> 8) & 0xff) as u8,
        ((program_id >> 16) & 0xff) as u8,
        ((program_id >> 24) & 0xff) as u8,
        (epoche & 0xff) as u8,
        ((epoche >> 8) & 0xff) as u8,
        ((epoche >> 16) & 0xff) as u8,
        ((epoche >> 24) & 0xff) as u8,
    ];
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x100000001b3);
        i += 1;
    }
    if h == 0 { 1 } else { h }
}

/// Einen Exec-Antrag prüfen — Spiegel von `caprock-loader::exec::pruefe_exec_antrag`.
/// Jeder verletzte Formpunkt endet in [`LxpdSpawnError::ExecAbgelehnt`]; die Unterfaelle
/// nennt die Variante in ihrer Doku (ein benannter Ausgang statt vier).
pub fn exec_antrag_pruefen(a: &ExecAntrag) -> Result<(), LxpdSpawnError> {
    if a.token == 0 {
        return Err(LxpdSpawnError::ExecAbgelehnt);
    }
    if a.token != teardown_token_fuer(a.program_id, a.epoche) {
        return Err(LxpdSpawnError::ExecAbgelehnt);
    }
    if a.segmente == 0 || a.segmente > MAX_SEGS {
        return Err(LxpdSpawnError::ExecAbgelehnt);
    }
    if a.eintrag == 0 || a.eintrag >= USER_VA_TOP {
        return Err(LxpdSpawnError::ExecAbgelehnt);
    }
    Ok(())
}

// --- Ueberlappung (reine Adressarithmetik, überlauffest) ---

/// Ob sich `[a_base, a_base + a_len)` und `[b_base, b_base + b_len)` ueberlappen.
/// `None`, wenn ein Ende nicht in `u64` passt — eine Bereichsangabe, die überlaeuft,
/// ist keine Angabe (der Aufrufer wertet das als [`LxpdSpawnError::BadGrants`]).
/// Leere Bereiche (`len == 0`) ueberlappen nie — sie werden anderswo abgewiesen.
fn bereiche_ueberlappen(a_base: u64, a_len: u64, b_base: u64, b_len: u64) -> Option<bool> {
    let a_ende = a_base.checked_add(a_len)?;
    let b_ende = b_base.checked_add(b_len)?;
    Some(a_base < b_ende && b_base < a_ende)
}

// --- Grant-Extraktion aus dem Manifest (Byte-Ebene, kein Full-Parser, kein `alloc`) ---

fn ist_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// String-Spanne ab `pos` ueberspringen (`d[pos]` muss `"` sein) — mit Escape-Behandlung,
/// damit eine Suche kein `\"` fuer das Ende haelt. `None` bei abgeschnittenem String.
fn string_ende(d: &[u8], pos: usize) -> Option<usize> {
    if d.get(pos) != Some(&b'"') {
        return None;
    }
    let mut i = pos + 1;
    loop {
        match d.get(i) {
            Some(b'"') => return Some(i + 1),
            Some(b'\\') => {
                d.get(i + 1)?;
                i += 2;
            }
            Some(b) if *b < 0x20 => return None,
            Some(_) => i += 1,
            None => return None,
        }
    }
}

/// `"schluessel"` ausserhalb von Strings suchen — ein Treibername, der zufaellig
/// `grants_bar` enthaelt, darf die Suche nicht auf eine falsche Faehrte locken.
/// Gibt die Position des oeffnenden `"` zurueck.
fn schluessel_suchen(d: &[u8], schluessel: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < d.len() {
        match d.get(i) {
            Some(b'"') => {
                let nach = i + 1 + schluessel.len();
                if d.get(i + 1..nach) == Some(schluessel) && d.get(nach) == Some(&b'"') {
                    return Some(i);
                }
                i = string_ende(d, i)?;
            }
            Some(_) => i += 1,
            None => return None,
        }
    }
    None
}

/// Ab `pos` Leerraum ueberspringen, `:` erwarten, Zahl lesen (nur Dezimalziffern, keine
/// Vorzeichen, keine Brueche — Grant-Adressen sind ganze Zahlen). Dahinter muss ein
/// Begrenzer stehen (`,`/`}`/`]`/Leerraum/Ende), sonst ist es keine Zahl, sondern ein
/// Praefix einer groesseren. `None` bei jeder Abweichung — fail-closed.
fn doppelpunkt_zahl(d: &[u8], mut pos: usize) -> Option<u64> {
    while d.get(pos).is_some_and(|&b| ist_ws(b)) {
        pos += 1;
    }
    if d.get(pos) != Some(&b':') {
        return None;
    }
    pos += 1;
    while d.get(pos).is_some_and(|&b| ist_ws(b)) {
        pos += 1;
    }
    let mut v: u64 = 0;
    let mut stellen = 0u32;
    while d.get(pos).is_some_and(|b| b.is_ascii_digit()) {
        v = v.checked_mul(10)?.checked_add(u64::from(d[pos] - b'0'))?;
        pos += 1;
        stellen += 1;
    }
    if stellen == 0 {
        return None;
    }
    match d.get(pos) {
        None => Some(v),
        Some(b) if *b == b',' || *b == b'}' || *b == b']' || ist_ws(*b) => Some(v),
        _ => None,
    }
}

/// Zahl `schluessel` innerhalb der Spanne `span` lesen (suche + `:` + Zahl).
fn zahl_in_spanne(span: &[u8], schluessel: &[u8]) -> Option<u64> {
    let at = schluessel_suchen(span, schluessel)?;
    doppelpunkt_zahl(span, at + 1 + schluessel.len() + 1)
}

/// Ausgeglichene `{…}`-Spanne ab `pos` (`d[pos]` muss `{` sein) — string-bewusst,
/// Schachtelung gedeckelt (Grant-Objekte sind flach; Tiefe ist hier ein Angriff, kein Inhalt).
fn objekt_spanne(d: &[u8], pos: usize) -> Option<(usize, usize)> {
    if d.get(pos) != Some(&b'{') {
        return None;
    }
    let mut tiefe: u32 = 0;
    let mut i = pos;
    while i < d.len() {
        match d[i] {
            b'"' => i = string_ende(d, i)?,
            b'{' => {
                tiefe += 1;
                if tiefe > 8 {
                    return None;
                }
                i += 1;
            }
            b'}' => {
                tiefe -= 1;
                i += 1;
                if tiefe == 0 {
                    return Some((pos, i));
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Objekt zu Schluessel `key` finden: `"key"` suchen, dahinter `{…}` abgrenzen.
/// Gibt die Objekt-Spanne zurueck.
fn objekt_zu_schluessel<'a>(d: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let at = schluessel_suchen(d, key)?;
    let mut i = at + 1 + key.len() + 1;
    while d.get(i).is_some_and(|&b| ist_ws(b)) {
        i += 1;
    }
    if d.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while d.get(i).is_some_and(|&b| ist_ws(b)) {
        i += 1;
    }
    if d.get(i) == Some(&b'[') {
        // Array-Feld (`grants_bar`): erstes `{…}` darin nehmen.
        i += 1;
        while i < d.len() && d[i] != b'{' {
            if d[i] == b']' {
                return None; // leeres Array — kein Eintrag, keine Zusage
            }
            if d[i] == b'"' {
                i = string_ende(d, i)?;
            } else {
                i += 1;
            }
        }
    }
    let (s, e) = objekt_spanne(d, i)?;
    d.get(s..e)
}

/// Die Grant-Anfrage aus den Manifest-Bytes lesen: erster `grants_bar`-Eintrag
/// (`base`/`size`), `dma_window` (`base`/`size`), `irq_vector`.
///
/// Kein Full-Parser und kein Ersatz fuer `manifest_facts` — nur die Adress-Extraktion fuer
/// den Glue, nachdem das Manifest anderswo verifiziert wurde. Jede strukturelle Abweichung
/// (kein Objekt, Feld fehlt, keine Zahl) endet fail-closed: Grant-Felder in
/// [`LxpdSpawnError::BadGrants`], ein gar nicht erst objektfoermiges Dokument in
/// [`LxpdSpawnError::BadImage`] (dieselbe Aufteilung wie `manifest_facts`).
pub fn grants_aus_manifest(manifest_json: &[u8]) -> Result<LxpdGrantRequest, LxpdSpawnError> {
    let mut i = 0;
    while manifest_json.get(i).is_some_and(|&b| ist_ws(b)) {
        i += 1;
    }
    if manifest_json.get(i) != Some(&b'{') {
        return Err(LxpdSpawnError::BadImage);
    }
    let bar = objekt_zu_schluessel(manifest_json, b"grants_bar").ok_or(LxpdSpawnError::BadGrants)?;
    let bar_base = zahl_in_spanne(bar, b"base").ok_or(LxpdSpawnError::BadGrants)?;
    let bar_size = zahl_in_spanne(bar, b"size").ok_or(LxpdSpawnError::BadGrants)?;
    let dma = objekt_zu_schluessel(manifest_json, b"dma_window").ok_or(LxpdSpawnError::BadGrants)?;
    let dma_base = zahl_in_spanne(dma, b"base").ok_or(LxpdSpawnError::BadGrants)?;
    let dma_size = zahl_in_spanne(dma, b"size").ok_or(LxpdSpawnError::BadGrants)?;
    let irq_at = schluessel_suchen(manifest_json, b"irq_vector").ok_or(LxpdSpawnError::BadGrants)?;
    let irq = doppelpunkt_zahl(manifest_json, irq_at + 1 + b"irq_vector".len() + 1)
        .ok_or(LxpdSpawnError::BadGrants)?;
    let irq: u32 = irq.try_into().map_err(|_| LxpdSpawnError::BadGrants)?;
    Ok(LxpdGrantRequest { bar_base, bar_size, dma_base, dma_size, irq, heap_pages: 0 })
}

// --- Die Entscheidung ---

/// Spawn freigeben oder benannt abweisen — in der Reihenfolge der Moduldoku
/// (Verifikation, Inhalt, Grants, Deckel, Exec-Form). Der erste verletzte Punkt gewinnt;
/// ein `Ok` enthaelt den geprüften [`SpawnPlan`] und nichts weiter.
pub fn spawn_entscheidung(facts: &LxpdFacts, policy: &LxpdPolicy) -> Result<SpawnPlan, LxpdSpawnError> {
    // 1. Verifikations-Gate — unverifiziert wird nie geladen.
    if !facts.verified || !facts.signature_present {
        return Err(LxpdSpawnError::Unverified);
    }
    if facts.trampoline_names != facts.tramp_count {
        return Err(LxpdSpawnError::Unverified);
    }
    // 2. Manifest-Inhalt.
    if facts.schema_version != LXPD_SCHEMA_VERSION {
        return Err(LxpdSpawnError::BadImage);
    }
    if facts.coverage_pct != LXPD_COVERAGE_PFLICHT {
        return Err(LxpdSpawnError::BadImage);
    }
    // 3. Grant-Plausibilitaet.
    if facts.bar_size == 0 || facts.dma_size == 0 || facts.irq == 0 {
        return Err(LxpdSpawnError::BadGrants);
    }
    match bereiche_ueberlappen(facts.bar_base, facts.bar_size, facts.dma_base, facts.dma_size) {
        None => return Err(LxpdSpawnError::BadGrants),
        Some(true) => return Err(LxpdSpawnError::Ueberlappung),
        Some(false) => {}
    }
    // 4. Budget-Deckel.
    if facts.bar_size > policy.max_bar_bytes
        || facts.dma_size > policy.max_dma_bytes
        || facts.heap_pages > policy.max_heap_pages
    {
        return Err(LxpdSpawnError::BudgetErschoepft);
    }
    // 5. Exec-Form (Token-Regel aus `exec.rs`).
    exec_antrag_pruefen(&policy.exec)?;
    Ok(SpawnPlan {
        bar_base: facts.bar_base,
        bar_size: facts.bar_size,
        dma_base: facts.dma_base,
        dma_size: facts.dma_size,
        irq: facts.irq,
        heap_pages: facts.heap_pages,
    })
}

// --- Host-Tests (HAL-frei, Fake-Facts — kein Geraet, keine Maschine, nur Literale) ---

#[cfg(test)]
mod tests {
    use super::*;

    fn gute_facts() -> LxpdFacts {
        LxpdFacts {
            verified: true,
            signature_present: true,
            schema_version: 1,
            trampoline_names: 2,
            tramp_count: 2,
            coverage_pct: 100.0,
            bar_base: 0x1_0000,
            bar_size: 0x2000,
            dma_base: 0x2_0000,
            dma_size: 0x1_0000,
            irq: 7,
            heap_pages: 4,
        }
    }

    fn gute_policy() -> LxpdPolicy {
        LxpdPolicy {
            max_bar_bytes: 0x1_0000,
            max_dma_bytes: 0x10_0000,
            max_heap_pages: 16,
            exec: ExecAntrag {
                program_id: 7,
                epoche: 3,
                token: teardown_token_fuer(7, 3),
                eintrag: 0x400_0000,
                segmente: 2,
            },
        }
    }

    #[test]
    fn unverified_abweisung() {
        // Sonst perfekt — ohne Verify-Urteil wird nie geladen.
        let mut f = gute_facts();
        f.verified = false;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::Unverified));
        // Anwesenheit ist kein Beweis: Feld da, Rechnung nie gelaufen.
        let mut g = gute_facts();
        g.signature_present = false;
        assert_eq!(spawn_entscheidung(&g, &gute_policy()), Err(LxpdSpawnError::Unverified));
    }

    #[test]
    fn stubnamen_ungleich_container_abweisung() {
        // Zwei Stubnamen bei einem Stub: das Manifest gehoert nicht zu diesem Container.
        let mut f = gute_facts();
        f.tramp_count = 1;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::Unverified));
    }

    #[test]
    fn grants_null_abweisung() {
        let mut f = gute_facts();
        f.bar_size = 0;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::BadGrants));
        let mut g = gute_facts();
        g.dma_size = 0;
        assert_eq!(spawn_entscheidung(&g, &gute_policy()), Err(LxpdSpawnError::BadGrants));
    }

    #[test]
    fn irq_null_abweisung() {
        // Vektor 0 ist kein Vektor, sondern Abwesenheit.
        let mut f = gute_facts();
        f.irq = 0;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::BadGrants));
    }

    #[test]
    fn ueberlapp_abweisung() {
        // BAR [0x1000, 0x3000) vs. DMA [0x2000, 0x4000): zwei Zusagen ueber dieselben Bytes.
        let mut f = gute_facts();
        f.bar_base = 0x1000;
        f.bar_size = 0x2000;
        f.dma_base = 0x2000;
        f.dma_size = 0x2000;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::Ueberlappung));
        // Angrenzend ist kein Ueberlapp: [0x1000, 0x2000) + [0x2000, 0x3000) besteht.
        let mut g = gute_facts();
        g.bar_base = 0x1000;
        g.bar_size = 0x1000;
        g.dma_base = 0x2000;
        g.dma_size = 0x1000;
        assert!(spawn_entscheidung(&g, &gute_policy()).is_ok());
        // Ueberlaufende Bereichsangabe ist keine Angabe.
        let mut h = gute_facts();
        h.bar_base = u64::MAX;
        h.bar_size = 2;
        assert_eq!(spawn_entscheidung(&h, &gute_policy()), Err(LxpdSpawnError::BadGrants));
    }

    #[test]
    fn budget_deckel_greift() {
        let mut f = gute_facts();
        f.dma_size = 0x20_0000; // Deckel: 0x10_0000
        assert_eq!(
            spawn_entscheidung(&f, &gute_policy()),
            Err(LxpdSpawnError::BudgetErschoepft)
        );
        let mut g = gute_facts();
        g.heap_pages = 17; // Deckel: 16
        assert_eq!(
            spawn_entscheidung(&g, &gute_policy()),
            Err(LxpdSpawnError::BudgetErschoepft)
        );
        // Genau am Deckel ist noch innerhalb.
        let mut h = gute_facts();
        h.bar_size = 0x1_0000;
        assert!(spawn_entscheidung(&h, &gute_policy()).is_ok());
    }

    #[test]
    fn token_regel() {
        // `0` heisst „kein Token" — kein „egal".
        let mut p = gute_policy();
        p.exec.token = 0;
        assert_eq!(
            spawn_entscheidung(&gute_facts(), &p),
            Err(LxpdSpawnError::ExecAbgelehnt)
        );
        // Token von gestern (andere Epoche) bindet nicht an diesen Stand.
        let mut q = gute_policy();
        q.exec.token = teardown_token_fuer(7, 2);
        assert_eq!(
            spawn_entscheidung(&gute_facts(), &q),
            Err(LxpdSpawnError::ExecAbgelehnt)
        );
        // Kernel-Eintrag ist kein Exec-Ziel; leeres Image faellt vor dem Rueckbau.
        let mut r = gute_policy();
        r.exec.eintrag = USER_VA_TOP;
        assert_eq!(
            spawn_entscheidung(&gute_facts(), &r),
            Err(LxpdSpawnError::ExecAbgelehnt)
        );
        let mut s = gute_policy();
        s.exec.segmente = 0;
        assert_eq!(
            spawn_entscheidung(&gute_facts(), &s),
            Err(LxpdSpawnError::ExecAbgelehnt)
        );
    }

    #[test]
    fn coverage_nicht_100_abweisung() {
        // `bind` bindet unter 100 % gar nicht — ein solches Manifest ist Inhaltsschrott.
        let mut f = gute_facts();
        f.coverage_pct = 99.5;
        assert_eq!(spawn_entscheidung(&f, &gute_policy()), Err(LxpdSpawnError::BadImage));
        let mut g = gute_facts();
        g.schema_version = 2;
        assert_eq!(spawn_entscheidung(&g, &gute_policy()), Err(LxpdSpawnError::BadImage));
    }

    #[test]
    fn happy_path_liefert_plan() {
        let plan = spawn_entscheidung(&gute_facts(), &gute_policy()).unwrap();
        assert_eq!(
            plan,
            SpawnPlan {
                bar_base: 0x1_0000,
                bar_size: 0x2000,
                dma_base: 0x2_0000,
                dma_size: 0x1_0000,
                irq: 7,
                heap_pages: 4,
            }
        );
    }

    #[test]
    fn token_ist_stabil_und_nie_null() {
        // Spiegel-Garantie aus `exec.rs`: deterministisch, nie der „egal"-Wert.
        let t = teardown_token_fuer(7, 3);
        assert_eq!(t, teardown_token_fuer(7, 3));
        assert_ne!(t, 0);
        assert_ne!(teardown_token_fuer(7, 4), t);
    }

    const MANIFEST: &[u8] = br#"{
        "signature": "0123456789abcdef",
        "driver": "e1000e",
        "trampolines": ["dma_map_single->caprock_dma_map", "spin_lock->caprock_spin_lock"],
        "schema_version": 1,
        "api_version": "X1",
        "arch": "x86-64",
        "grants_bar": [{"index": 0, "base": 65536, "size": 8192, "flags": "RW"}],
        "dma_window": {"base": 131072, "size": 65536, "bits": 64},
        "irq_vector": 7,
        "class_b_objects": [],
        "gpl_affected": false,
        "coverage_pct": 100.0
    }"#;

    #[test]
    fn grants_aus_manifest_liest_fenster() {
        let g = grants_aus_manifest(MANIFEST).unwrap();
        assert_eq!(
            g,
            LxpdGrantRequest {
                bar_base: 65536,
                bar_size: 8192,
                dma_base: 131072,
                dma_size: 65536,
                irq: 7,
                heap_pages: 0,
            }
        );
    }

    #[test]
    fn grants_aus_manifest_fehlend_ist_badgrants() {
        // Leeres BAR-Array: keine Zusage.
        let leer: &[u8] = br#"{"grants_bar": [], "dma_window": {"base": 1, "size": 1}, "irq_vector": 1}"#;
        assert_eq!(grants_aus_manifest(leer), Err(LxpdSpawnError::BadGrants));
        // IRQ fehlt: keine Zusage.
        let ohne_irq: &[u8] =
            br#"{"grants_bar": [{"base": 1, "size": 1}], "dma_window": {"base": 1, "size": 1}}"#;
        assert_eq!(grants_aus_manifest(ohne_irq), Err(LxpdSpawnError::BadGrants));
        // Kein Objekt: kein Manifest-Inhalt.
        assert_eq!(grants_aus_manifest(b"[1,2]"), Err(LxpdSpawnError::BadImage));
    }

    #[test]
    fn grant_schluessel_im_string_taeuscht_nicht() {
        // Der Treibername enthaelt Grant-Schluesselwoerter — die Suche darf nicht in
        // String-Werte abbiegen.
        let fies: &[u8] = br#"{"driver": "grants_bar dma_window irq_vector", "grants_bar": [{"base": 16, "size": 32}], "dma_window": {"base": 64, "size": 128}, "irq_vector": 9}"#;
        let g = grants_aus_manifest(fies).unwrap();
        assert_eq!((g.bar_base, g.bar_size, g.dma_base, g.dma_size, g.irq), (16, 32, 64, 128, 9));
    }
}
