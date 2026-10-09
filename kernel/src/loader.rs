//! Kernel-Glue des **generischen Binary-Loaders** (ext-26, [ADR 0011](../../docs/adr/0011-binary-loader.md)).
//!
//! Die **reine**, bounds-geprüfte Parse-Logik (Boot-Archiv, ab L1 Minimal-ELF64) liegt im Crate
//! `caprock-loader` (0 `unsafe`, host-getestet). Hier liegt der **privilegierte** Teil, der RAM
//! liest und (ab L1) Segmente in Regionen kopiert, W^X mappt, VSpace/PD anlegt, Caps endowt und
//! Threads spawnt — alles über die bestehenden `system::`-Primitive (keine neuen Sonderrechte).
//!
//! **L0:** das Boot-Archiv aus dem reservierten RAM-Fenster lesen + die Module melden.

use crate::manifest_keys::{MANIFEST_KEYS, MIN_MANIFEST_VERSION};
use crate::trusted_keys::{MIN_VERSION, TRUSTED_KEYS};
use core::sync::atomic::{AtomicU64, Ordering};
use caprock_cap::CapPtr;
use caprock_hal::{print, println};
use caprock_sync::SpinLock;
use caprock_loader::archive::Archive;
use caprock_loader::cert::{TrustedCert, SIG_ALG_ED25519, SIG_ED25519_LEN};
use caprock_loader::elf::ElfImage;
use caprock_loader::manifest::{
    Entry as ManifestEntry, SystemManifest, Verified, SIG_ALG_ED25519 as MAN_SIG_ALG_ED25519,
};
use caprock_loader::{LoaderError, Program, DOMAIN_HARDWARE, DOMAIN_TRUSTED, DOMAIN_USERLAND};
use caprock_mem::Rights;
use caprock_microkit::Domain;
use caprock_sched::ThreadId;
use caprock_trust::{fingerprint, sha256, verify_sig};

/// Größe des reservierten RAM-Fensters für das Boot-Archiv (oben in RAM, vom `PhysAllocator`
/// ausgenommen — siehe `init_mem`-Aufruf in `main.rs`). QEMU legt das Archiv per
/// `-device loader,addr=MOD_BASE` hierher; der Wert MUSS zu `test-qemu.sh` passen.
pub const MOD_WINDOW: u64 = 0x0100_0000; // 16 MiB

/// Basis des Archiv-Fensters = `ram_end - MOD_WINDOW` für die QEMU-`virt`-Maschine mit 4 GiB RAM
/// (RAM `0x4000_0000` + 4 GiB = `0x1_4000_0000`). Liegt in der statisch identity-gemappten
/// Normal-WB-Region (GiB 2..9), also EL1-lesbar.
pub const MOD_BASE: u64 = 0x0001_4000_0000 - MOD_WINDOW; // 0x1_3F00_0000

/// **Wo das Boot-Archiv liegt** — zur Laufzeit gesetzt, nicht fest verdrahtet (A-1.1).
///
/// Auf ARM ist die Lage eine Verabredung mit dem Testaufbau (`-device loader,addr=MOD_BASE` in ein
/// reserviertes Fenster); auf x86 legt der Bootloader das Archiv als Multiboot-Modul dorthin, wo
/// er will, und sagt die Adresse erst beim Start. Also entscheidet der Hochlauf, nicht der
/// Übersetzer. Fehlt die Angabe, bleibt es beim ARM-Fenster (unverändertes Verhalten dort) bzw.
/// bei „kein Archiv".
static ARCHIVE_BASE: AtomicU64 = AtomicU64::new(0);
static ARCHIVE_LEN: AtomicU64 = AtomicU64::new(0);

/// Die Lage des Boot-Archivs melden. Muss **vor** der ersten Archiv-Benutzung laufen und
/// beschreibt einen Bereich, den der `PhysAllocator` nicht vergeben darf.
pub fn set_archive_span(base: u64, len: u64) {
    ARCHIVE_BASE.store(base, Ordering::Relaxed);
    ARCHIVE_LEN.store(len, Ordering::Relaxed);
}

/// Die aktuell geltende Lage des Archivs (`(0, 0)` = keine).
pub fn archive_span() -> (u64, u64) {
    let base = ARCHIVE_BASE.load(Ordering::Relaxed);
    if base != 0 {
        return (base, ARCHIVE_LEN.load(Ordering::Relaxed));
    }
    // Rückfall: das statisch reservierte ARM-Fenster. Auf x86 gibt es kein solches Fenster —
    // dort heißt „nicht gemeldet" genau das, und `read_archive` liefert None statt an einer
    // erratenen Adresse zu lesen.
    #[cfg(target_arch = "aarch64")]
    {
        (MOD_BASE, MOD_WINDOW)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        (0, 0)
    }
}

/// Das Boot-Archiv aus dem reservierten Fenster lesen + **vollständig validieren**. `None`, wenn
/// kein gültiges Archiv vorliegt (fehlend/beschädigt) — der Kernel läuft dann ohne externe Module.
pub fn read_archive() -> Option<Archive<'static>> {
    let (base, len) = archive_span();
    if base == 0 || len == 0 {
        return None;
    }
    // SAFETY: `[base, base+len)` ist reservierter, identity-gemappter Normal-RAM — auf ARM das
    // statische Fenster (vom PhysAllocator ausgenommen, s. `system::init_mem`-Aufruf), auf x86 der
    // vom Bootloader gemeldete Modulbereich, der vor der ersten Allokation aus der Freiliste
    // ausgeschnitten wurde (`arch::x86_64::bringup`). Nur **lesender** Zugriff; der Parser
    // (`caprock-loader`) ist vollständig bounds-geprüft und panik-frei.
    let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) };
    Archive::parse(bytes).ok()
}

// ================================================================================================
// System-Manifest (A-1.2 – A-1.4): das eine Autoritätsdokument des Boot-Images
// ================================================================================================

extern "C" {
    /// Anfang des Kernel-Codes (Linker-Symbol, beide Architekturen).
    static __text_start: u8;
    /// Ende der Kernel-Konstanten (Linker-Symbol, beide Architekturen).
    static __rodata_end: u8;
}

/// **SHA-256 des Kernel-Codes** — `[__text_start, __rodata_end)` (A-1.3).
///
/// Genau diese Spanne, und nicht „das Image": sie ist zur Laufzeit **unveränderlich** (`.text` ist
/// R-X, `.rodata` R--, beides unter CR0.WP bzw. dem ARM-Pendant), sie ist auf beiden Architekturen
/// zusammenhängend, und sie steht im ELF byte-identisch so, wie sie später im Speicher liegt
/// (identity geladen, keine Relokation). Ein Hash über `.data`/`.bss` wäre schon nach dem ersten
/// Boot-Schritt ein anderer — der Kernel könnte ihn gar nicht reproduzieren.
///
/// Das Gegenstück auf der Werkzeugseite ist `tools/kernel_hash.py`; beide Definitionen müssen
/// zusammen geändert werden, sonst passt kein Manifest mehr auf keinen Kernel.
pub fn kernel_code_hash() -> [u8; 32] {
    // SAFETY: Linker-Symbole; es werden nur ihre Adressen gelesen und daraus ein **lesender**
    // Slice über den identity-gemappten, im Betrieb schreibgeschützten Kernel-Code gebildet.
    let (start, end) = unsafe {
        (
            &__text_start as *const u8 as usize,
            &__rodata_end as *const u8 as usize,
        )
    };
    if end <= start {
        return [0u8; 32]; // kann nur bei kaputtem Linker-Skript passieren; kein Panic im Kernel
    }
    // SAFETY: wie oben — die Spanne liegt vollständig im geladenen Kernel-Image.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
    sha256(bytes)
}

/// Das **System-Manifest** aus dem Archiv lesen und **vollständig prüfen** (A-1.3).
///
/// Reihenfolge, und sie ist nicht verhandelbar:
/// 1. strukturell parsen (feste Breiten, Längenpräfixe — noch keine Auswertung);
/// 2. Algorithmus + Signaturlänge;
/// 3. `key_id` in der read-only DB, nicht zurückgezogen, DB-selbstkonsistent;
/// 4. **Signatur über die gesamte Nachricht**;
/// 5. **Bindung an dieses Kernel-Image** (`kernel_hash`);
/// 6. Anti-Downgrade (`manifest_version >= MIN_MANIFEST_VERSION`).
///
/// Erst danach gibt es Einträge — das erzwingt der Typ [`Verified`], nicht die Disziplin des
/// Aufrufers: [`SystemManifest::parse`] liefert keinen einzigen Eintrag.
pub fn read_manifest() -> Option<Verified<'static>> {
    let archive = read_archive()?;
    let raw = archive.system_manifest();
    if raw.is_empty() {
        return None;
    }
    let m = SystemManifest::parse(raw).ok()?; // (1)
    if m.signature_algorithm_id != MAN_SIG_ALG_ED25519 {
        return None; // (2)
    }
    let sig: &[u8; SIG_ED25519_LEN] = m.signature().try_into().ok()?;
    let key = MANIFEST_KEYS.iter().find(|k| k.key_id == m.key_id)?; // (3)
    if key.revoked || fingerprint(&key.pubkey) != key.key_id {
        return None;
    }
    if m.kernel_hash != kernel_code_hash() {
        return None; // (5) — vorgezogen geprüft, aber die Signatur entscheidet in `verify_with`
    }
    if m.manifest_version < MIN_MANIFEST_VERSION {
        return None; // (6)
    }
    let pubkey = key.pubkey;
    m.verify_with(move |msg, _| verify_sig(&pubkey, msg, sig)).ok() // (4)
}

/// Puffergröße für die manipulierte Manifest-Kopie im [`manifest_audit`]-Live-Oracle.
const MANIFEST_AUDIT_MAX: usize = 2048;

/// **Manifest-Audit** — strukturelle Selbstkonsistenz der Key-DB **plus** ein Live-Oracle des
/// aktiven Gates. `0` = sauber, sonst ein Anomalie-Code:
///
/// * 1 — Key-DB leer (kein Manifest könnte je angenommen werden).
/// * 2 — ein `key_id != fingerprint(pubkey)` (DB nicht selbst-zertifizierend).
/// * 3 — doppelte `key_id`.
/// * 4 — es liegt ein Manifest im Archiv, aber es wird **abgelehnt** (Gate/DB/Kernel-Hash passen
///   nicht zusammen). Das ist die häufigste echte Ursache: neu gebauter Kernel, altes Manifest.
/// * 5 — Live-Oracle: eine **manipulierte** Kopie wird **angenommen** (Gate setzt nicht durch).
/// * 6 — das angenommene Manifest ist inhaltlich widersprüchlich ([`Verified::audit`] ≠ 0);
///   der Inhaltscode steht dann in den oberen 16 Bit.
///
/// **Warum Code 5 hier stehen muss:** ein Schweigen darf nicht als Erfolg durchgehen. Ohne das
/// Oracle bewiese ein grünes Audit nur, dass nichts geprüft wurde — dieselbe Verwechslung, die
/// dieses Projekt bei der leeren SMMU-Event-Queue schon einmal bezahlt hat.
pub fn manifest_audit() -> u32 {
    if MANIFEST_KEYS.is_empty() {
        return 1;
    }
    for (i, k) in MANIFEST_KEYS.iter().enumerate() {
        if fingerprint(&k.pubkey) != k.key_id {
            return 2;
        }
        for k2 in &MANIFEST_KEYS[i + 1..] {
            if k2.key_id == k.key_id {
                return 3;
            }
        }
    }
    let Some(archive) = read_archive() else { return 0 };
    let raw = archive.system_manifest();
    if raw.is_empty() {
        return 0; // kein Manifest vorhanden -> nichts durchzusetzen (die Startmenge bleibt leer)
    }
    let Some(v) = read_manifest() else { return 4 };
    // Live-Oracle: ein signiertes Byte kippen -> die Signatur MUSS brechen.
    if raw.len() <= MANIFEST_AUDIT_MAX {
        let mut buf = [0u8; MANIFEST_AUDIT_MAX];
        buf[..raw.len()].copy_from_slice(raw);
        buf[24] ^= 0x01; // erstes Byte des kernel_hash -- signiert UND inhaltlich bindend
        let tampered = SystemManifest::parse(&buf[..raw.len()])
            .ok()
            .and_then(|m| {
                let sig: &[u8; SIG_ED25519_LEN] = m.signature().try_into().ok()?;
                let key = MANIFEST_KEYS.iter().find(|k| k.key_id == m.key_id)?;
                let pubkey = key.pubkey;
                m.verify_with(move |msg, _| verify_sig(&pubkey, msg, sig)).ok()
            });
        if tampered.is_some() {
            return 5;
        }
    }
    let content = v.audit();
    if content != 0 {
        return 6 | (content << 16);
    }
    0
}

/// Das Manifest melden: was steht drin, und hält das Gate? Reines Lesen; greift nicht in den
/// Boot-Ablauf ein. Gibt den Audit-Code zurück (0 = sauber), damit der Aufrufer ihn bewerten kann.
pub fn manifest_report() -> u32 {
    let audit = manifest_audit();
    match read_manifest() {
        Some(v) => {
            let _h = v.header();
            #[cfg(feature = "debug-log")]
            println!(
                "manifest: v{} vom Schluessel {:02x}{:02x}{:02x}{:02x}.., an dieses Kernel-Image gebunden, {} Eintrag/Eintraege",
                _h.manifest_version, _h.key_id[0], _h.key_id[1], _h.key_id[2], _h.key_id[3], v.count()
            );
            for e in v.iter() {
                print!(
                    "manifest:   [{}]{} dom={} iface=v{} caps={:#06x} politik={:#x} prio={} numa={} kern=",
                    e.program_id, e.name(), e.domain, e.iface_version, e.initial_caps,
                    e.policy_flags, e.priority, e.numa_node
                );
                if e.core_affinity == caprock_loader::manifest::ANY_CORE {
                    print!("beliebig");
                } else {
                    print!("{}", e.core_affinity);
                }
                println!(" budget={}us{}", e.budget_us, if e.is_root_task() { " ROOT" } else { "" });
            }
        }
        None => {
            // Bei Ablehnung ist die haeufigste echte Ursache "neuer Kernel, altes Manifest".
            // Deshalb steht hier, WORAN gebunden wurde -- sonst raet man.
            let _k = kernel_code_hash();
            #[cfg(feature = "debug-log")]
            println!(
                "manifest: kein angenommenes System-Manifest (Audit-Code {audit}; dieser Kernel-Code-Hash beginnt {:02x}{:02x}{:02x}{:02x})",
                _k[0], _k[1], _k[2], _k[3]
            );
            if let Some(a) = read_archive() {
                let raw = a.system_manifest();
                if !raw.is_empty() {
                    match SystemManifest::parse(raw) {
                        Ok(_m) => {
                            #[cfg(feature = "debug-log")]
                            println!(
                            "manifest:   im Archiv: v{} alg={} key={:02x}{:02x}{:02x}{:02x}.. gebunden an {:02x}{:02x}{:02x}{:02x}.. ({} Eintraege, {} B Signatur)",
                            _m.manifest_version, _m.signature_algorithm_id,
                            _m.key_id[0], _m.key_id[1], _m.key_id[2], _m.key_id[3],
                            _m.kernel_hash[0], _m.kernel_hash[1], _m.kernel_hash[2], _m.kernel_hash[3],
                            _m.entry_count, _m.signature().len()
                        );
                        }
                        // **Der Versionsfall wird BENANNT.** Drei Zeilen weiter oben steht schon
                        // „die haeufigste echte Ursache ist *neuer Kernel, altes Manifest*, deshalb
                        // steht hier, WORAN gebunden wurde -- sonst raet man". Fuer den
                        // Kernel-Hash war die Lehre gezogen, fuer die FORMATVERSION nicht: bis
                        // 2026-08-10 endete auch sie in „Bytes, die nicht parsen". Dieselbe
                        // Fehlerklasse eine Ebene tiefer, mit derselben Folge -- man sucht nach
                        // Korruption, wo ein Versionsunterschied steht.
                        Err(caprock_loader::LoaderError::UnsupportedManifestFormat {
                            format_version,
                            entry_len,
                        }) => println!(
                            "manifest:   im Archiv liegt ein Manifest in einem FORMAT, das dieser \
                             Kernel nicht kennt: format_version={format_version} entry_len={entry_len} \
                             (dieser Kernel: {} bzw. {}). Das ist KEINE Korruption -- gelesen wird \
                             NICHTS davon, auch nicht die bekannten Felder: signiert ist die ganze \
                             Nachricht, ein teilgelesenes Manifest waere echt und missverstanden \
                             zugleich. **Die Absage faellt VOR der Signaturpruefung und ist damit \
                             unauthentifiziert** -- ein gekipptes Byte kann sie provozieren; was \
                             der signierte Kopf traegt, ist die andere Richtung (ein ANGENOMMENES \
                             Manifest hat keine untergeschobene Version)",
                            caprock_loader::manifest::MANIFEST_FORMAT_VERSION,
                            caprock_loader::manifest::ENTRY_LEN
                        ),
                        Err(_) => {
                            #[cfg(feature = "debug-log")]
                            println!("manifest:   im Archiv liegen {} B, die nicht parsen (Form, nicht Version -- die Formatversion haette einen eigenen Satz)", raw.len());
                        }
                    }
                }
            }
        }
    }
    // **Die Client-Notification-Bilanz steht NICHT hier.** Sie gehörte einen Anlauf lang an diese
    // Stelle und war dort wertlos: `manifest_audit` läuft **vor** dem Laden, die Zahl war immer
    // `0 PD(s), 0 verloren`. Eine Zahl, die zum Messzeitpunkt gar nicht anders sein kann, gattert
    // nichts — dieselbe Form wie ein Urteil, das erst im Bericht entsteht. Sie steht jetzt im
    // Abschlussbericht, wo die PDs geladen sind, und in `all_done`.
    println!(
        "manifest: {} (Signatur ueber die GESAMTE Nachricht, an DIESES Kernel-Image gebunden, Anti-Downgrade; manipulierte Kopie wird abgewiesen)",
        if audit == 0 { "ALL PASS" } else { "FAILURES" }
    );
    audit
}

// ================================================================================================
// Z7-Messkette (SW-PCR, kein TPM): jedes ERFOLGREICH geladene Image verlaengert genau einmal
// ================================================================================================
//
// Die Kette ist PCR-Philosophie ohne TPM-Hardware: `neu = SHA-256(prev || program_id ||
// domain || image_hash)`. Der Anker (`prev` des ersten Eintrags) ist [`kernel_code_hash`]
// — also genau die Spanne, an die das Manifest per `kernel_hash` gebunden ist
// (`read_manifest` weist ein Manifest an einen anderen Kernel ab). Damit haengt die Kette am
// Kernel-Image und ueber das Manifest an den Kernel-Keys — ohne eigene Key-DB.
//
// Was sie NICHT ist (ehrlich benannt): ohne TPM gibt es keinen Hardware-Anker. Ein
// physischer Angreifer mit Schreibzugriff aufs RAM schreibt die Liste um; der signierte
// Bericht (s. `programs/attest`, `tools/lx_attest_check.py`) ist Software-Evidenz, kein
// Remote-Trust gegen physische Angreifer. Die TPM-Messkette (Firmware misst Bootloader,
// Bootloader misst Kernel + Modul) kommt von aussen (s. `tools/lx_bootentscheidung.md` §2):
// diese Liste beginnt erst beim Kernel und misst Programme.
//
// Blattstellen (genau einmal je Ladevorgang, beide Blaetter, kein Doppelzaehlen):
// * `load_image_auf` (fremder Z7-Vorlauf: Aufruf dort, Definition hier — Aufruf unangetastet),
// * `load_program_into_pd_auf` (Aufruf unten — der LXPD-Pfad `lxpddrv_laden` laeuft DARUEBER,
//   `load_verified_image` ebenso; `lxpddrv_laden` selbst misst NICHT, sonst zaehlte der
//   LXPD-Weg doppelt).
//
// Abruf: [`messkette_bericht`] (Berichtsform wie `manifest_report`). KEIN neuer Syscall:
// ein lesender Zugriff waere eine Autoritaetsquelle neben dem Manifest; was die
// Attestierungs-PD braucht, steht als PATCH-TEXT im Arbeitsergebnis, nicht im Kernel.

/// Wie viele Messungen die Kette hoechstens haelt (wie `MAX_IFACE_TRACKED`: eine geladene
/// PD je ID; darueber wird BENANNT verworfen statt still ueberschrieben).
pub const MESSKETTE_MAX: usize = 64;

/// Ein Kettenglied: was gemessen wurde und der PCR-Stand danach.
#[derive(Clone, Copy)]
pub struct MessEintrag {
    /// Stabile Programm-ID aus dem Archiv/Manifest.
    pub program_id: u32,
    /// Zieldomaene (`DOMAIN_*` aus `caprock-loader`, u32 wie in `Program.domain`).
    pub domain: u32,
    /// `SHA-256(ELF-Bytes)` des geladenen Images.
    pub image_hash: [u8; 32],
    /// Kettenstand NACH diesem Eintrag (`SHA-256(prev || id || domain || hash)`).
    pub pcr: [u8; 32],
}

struct Messkette {
    glieder: [Option<MessEintrag>; MESSKETTE_MAX],
    anzahl: usize,
    verworfen: u64,
}

impl Messkette {
    const fn neu() -> Self {
        Messkette { glieder: [None; MESSKETTE_MAX], anzahl: 0, verworfen: 0 }
    }
}

static MESSKETTE: SpinLock<Messkette> = SpinLock::new(Messkette::neu());
// Sperr-Rang (Audit 2026-09-10, §1-Nachtrag): Blattlock -- alle Takes standalone, nie
// verschachtelt mit CAPS/MEM/SCHEDS gehalten (Leser: Bericht; Schreiber: je ein Lock ueber
// Lesen+Haengen, s. `messkette_verlaengern`).

/// Die Kette um ein erfolgreich geladenes Image verlaengern (Z7).
///
/// Signatur aus der Aufrufstelle abgeleitet: `messkette_verlaengern(prog.program_id,
/// prog.domain, sha256(prog.elf))` — `program_id: u32`, `domain: u32` (wie
/// `Program.domain`), `image_hash: [u8; 32]` (wie `sha256` ihn liefert).
///
/// Nur ERFOLG misst: beide Aufrufer stehen nach dem `Ok` ihres Ladepfads. Laeuft die Kette
/// voll, wird BENANNT verworfen (`verworfen + 1`, der Bericht meldet `FAILURES`) statt
/// still zu ueberschreiben — eine Messung, die unbemerkt aussetzt, saehe aus wie eine
/// bestandene.
///
/// EIN Lock ueber Lesen+Haengen (Audit 2026-09-10): der Hash (72 Byte, Mikrosekunden)
/// laeuft UNTER der Sperre, obwohl Rechnen unter Lock sonst verpoent ist -- zwei
/// nebenlaeufige Lader (Laufzeit-`LOAD_IMAGE` neben Boot) wuerden sonst denselben `prev`
/// falten und die Kette gabeln (zweiter Eintrag verlaengert nicht den ersten).
/// Ketten-Linearitaet schlaegt hier Haltezeit; geladen wird selten, nie heiss.
/// Sperr-Rang: Blattlock (alle Takes standalone, nie verschachtelt -- s. §1-Nachtrag an
/// der Statik unten).
pub fn messkette_verlaengern(program_id: u32, domain: u32, image_hash: [u8; 32]) {
    let mut g = MESSKETTE.lock();
    let prev = if g.anzahl == 0 {
        kernel_code_hash()
    } else {
        g.glieder[g.anzahl - 1].map(|e| e.pcr).unwrap_or_else(kernel_code_hash)
    };
    let mut buf = [0u8; 72];
    buf[..32].copy_from_slice(&prev);
    buf[32..36].copy_from_slice(&program_id.to_le_bytes());
    buf[36..40].copy_from_slice(&domain.to_le_bytes());
    buf[40..72].copy_from_slice(&image_hash);
    let pcr = sha256(&buf);
    if g.anzahl >= MESSKETTE_MAX {
        g.verworfen += 1;
        return;
    }
    let i = g.anzahl;
    g.glieder[i] = Some(MessEintrag { program_id, domain, image_hash, pcr });
    g.anzahl = i + 1;
}

/// Wie viele Images gemessen wurden (noch ohne Genesis — `0` heisst „nichts geladen").
pub fn messkette_anzahl() -> usize {
    MESSKETTE.lock().anzahl
}

/// Wie viele Messungen wegen voller Kette verworfen wurden (`0` = nichts verloren).
pub fn messkette_verworfen() -> u64 {
    MESSKETTE.lock().verworfen
}

/// Der aktuelle Kettenstand: `(pcr, anzahl)`. Ohne Ladung ist der Stand die Genesis
/// ([`kernel_code_hash`], `0`) — ein Pruefer kennt den Anker auch im leeren Lauf.
pub fn messkette_aktuell() -> ([u8; 32], usize) {
    let g = MESSKETTE.lock();
    if g.anzahl == 0 {
        (kernel_code_hash(), 0)
    } else {
        (g.glieder[g.anzahl - 1].map(|e| e.pcr).unwrap_or([0u8; 32]), g.anzahl)
    }
}

/// Das `i`-te Glied kopieren (`None` ausserhalb der belegten Spanne).
pub fn messkette_eintrag(i: usize) -> Option<MessEintrag> {
    MESSKETTE.lock().glieder.get(i).copied().flatten()
}

/// Die Kette melden (Berichtsform wie `manifest_report`): Genesis-Anker, je Glied
/// `program_id`/`domain`/Hash-Praefix/PCR-Praefix, dann die Bilanz. Gibt die Anzahl
/// zurueck. `FAILURES` faellt nur bei verworfenen Messungen — `0` Eintraege sind kein
/// Befund am Kernel, sondern ein Lauf ohne Module (der Anker steht trotzdem).
pub fn messkette_bericht() -> usize {
    let (anzahl, verworfen) = {
        let g = MESSKETTE.lock();
        (g.anzahl, g.verworfen)
    };
    let _genesis = kernel_code_hash();
    #[cfg(feature = "debug-log")]
    println!(
        "messkette: Anker Kernel-Code-Hash {:02x}{:02x}{:02x}{:02x}.., {} Eintrag/Eintraege, {} verworfen (SW-PCR ohne TPM-HW: neu = SHA-256(prev || program_id || domain || image_hash))",
        _genesis[0], _genesis[1], _genesis[2], _genesis[3], anzahl, verworfen
    );
    // Unter der Sperre wird KOPIERT, gedruckt wird danach (wie `iface_record_or_check`:
    // Drucken gehoert nicht in den kritischen Abschnitt).
    let mut kopie: [Option<MessEintrag>; MESSKETTE_MAX] = [None; MESSKETTE_MAX];
    {
        let g = MESSKETTE.lock();
        kopie.copy_from_slice(&g.glieder);
    }
    for _e in kopie[..anzahl].iter().flatten() {
        #[cfg(feature = "debug-log")]
        println!(
            "messkette:   [{}] dom={} bild={:02x}{:02x}{:02x}{:02x}.. pcr={:02x}{:02x}{:02x}{:02x}..",
            _e.program_id,
            _e.domain,
            _e.image_hash[0],
            _e.image_hash[1],
            _e.image_hash[2],
            _e.image_hash[3],
            _e.pcr[0],
            _e.pcr[1],
            _e.pcr[2],
            _e.pcr[3]
        );
    }
    println!(
        "messkette: {} (jedes erfolgreich geladene Image genau einmal; verworfene Messungen brechen die Kette)",
        if verworfen == 0 { "ALL PASS" } else { "FAILURES" }
    );
    anzahl
}

// ================================================================================================
// Root-Task (A-2.1): das erste Programm, und ab da ist jede Fähigkeit ein Userland-Programm
// ================================================================================================

/// **Boot-Argument** eines geladenen Programms: `(Anzahl der Programme << 32) | eigener Index`.
///
/// Das ist absichtlich das Minimum. Ein geladenes Programm muss wissen, wo es selbst in der
/// Startmenge steht und wie groß sie ist — sonst kann es „alle außer mir" nicht ausdrücken und
/// bräuchte stattdessen eine hart verdrahtete Indexverabredung mit dem Testskript. Alles
/// **darüber hinaus** gehört hinter eine Capability und nicht in ein Register: ein Boot-Info-Block,
/// den jedes Programm ungefragt lesen kann, wäre eine Autoritätsquelle neben dem Manifest.
pub const fn boot_arg(index: usize, count: usize) -> usize {
    (count << 32) | (index & 0xffff_ffff)
}

/// Warum ein Root-Task nicht startete. Bewusst **unterscheidbar**: „lädt nicht" ist als Diagnose
/// wertlos, und der Unterschied zwischen „kein Manifest" und „Hash passt nicht" ist der Unterschied
/// zwischen einem Aufbaufehler und einem Integritätsbefund.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootTaskError {
    /// Kein Boot-Archiv (fehlt/beschädigt).
    NoArchive,
    /// Kein angenommenes System-Manifest (fehlt, Signatur/Bindung/Downgrade — s. [`manifest_audit`]).
    NoManifest,
    /// Das Manifest benennt keinen **eindeutigen** Root-Task (keiner oder mehrere).
    NoRootEntry,
    /// Der im Manifest benannte Eintrag liegt nicht im Archiv.
    NotInArchive,
    /// **Der Modul-Hash weicht ab.** Das Manifest sagt, wie das Modul auszusehen hat; es sieht
    /// anders aus. Kein Grenzfall, sondern der Fall, für den der Hash da ist.
    HashMismatch,
    /// Manifest und Archiv widersprechen sich in der Domäne des Eintrags.
    DomainMismatch,
    /// **Die Startmenge liegt nicht vorn im Archiv** (D5). Der Root-Task bekommt seine Startmenge
    /// als `(Anzahl, eigener Index)` und spricht die anderen über `SYS_LOAD` mit einem
    /// **Archivindex** an. Damit das aufgeht, müssen die Manifest-Einträge `0..count` genau auf
    /// den Archivpositionen `0..count` liegen.
    ///
    /// Bis D5 stand diese Zusage nirgends — sie galt, weil die x86-Suite ihr Archiv passend
    /// baute. Ein Archiv mit Zusatzmodulen (die aarch64-Suite hat zehn Testdienste darin) hätte
    /// den Root-Task dazu gebracht, Fremdmodule als Startmenge zu laden: `probe` ist gar kein
    /// ELF, und die adversarialen Dienste wären ein zweites Mal gelaufen. Fail-closed statt
    /// stillschweigend das Falsche zu starten.
    StartSetNotPrefix,
    /// Das Manifest verlangt Autorität, die dieser Kernel nicht **erteilen** kann. Fail-closed:
    /// lieber gar nicht starten als mit stillschweigend weniger Rechten (ein halb bevollmächtigter
    /// Dienst scheitert später an einer Stelle, die niemand mit dem Manifest in Verbindung bringt).
    UnsupportedAuthority,
    /// Anfangs-Caps ließen sich nicht erzeugen (Kernel-Ressourcen erschöpft).
    NoResources,
    /// **Kein zuteilbares Gerät** (A-5.1). Das Manifest verlangt Geräte-Autorität, und es gibt
    /// keine — kein Gerät gefunden, oder es ist schon an einen anderen Treiber vergeben.
    /// Ausdrücklich **nicht** `NoResources`: ein erschöpfter Allokator ist ein Lastproblem, ein
    /// fehlendes Gerät ein Aufbauproblem, und die beiden führen zu verschiedenen Handgriffen.
    NoDevice,
    /// **Der Loader hat das Image abgelehnt** — mit dem genauen Grund.
    ///
    /// Dass dieser Grund mitgeführt wird, ist keine Bequemlichkeit. Beim ersten Lauf meldete
    /// dieser Pfad nur „LoadFailed", und die tatsächliche Ursache (der ELF-Parser kannte nur
    /// `EM_AARCH64`, das x86-Binary war für ihn schlicht kein ELF) war daran nicht zu erkennen.
    /// Eine Fehlerklasse, die alles einsammelt, ist als Diagnose wertlos.
    Rejected(LoaderError),
}

/// Die Anfangs-Caps eines Manifest-Eintrags erzeugen und als `(Slot, Cap)`-Liste ablegen.
///
/// **Slot-Konvention** (Teil der Loader-ABI, `docs/invariants.md`):
/// `0` = Loader-Cap, `1` = Notification, `2` = Endpoint, `3` = Konfigurationsraum-Seite,
/// `4` = Registerfenster (BAR), `5` = DMA-Region. Ein nicht angeforderter Cap lässt seinen
/// Slot leer — die Nummern verschieben sich also nicht, je nachdem was angefragt wurde. (Genau das
/// wäre die Art Detail, an der ein Programm später still das falsche Objekt anspricht.)
///
/// Slots 3–5 kamen mit A-5.1 dazu: sie sind die Autorität, aus der ein **Treiber im Userland**
/// besteht. `CAP_MMIO` erzeugt dabei **zwei** Caps, und das ist kein Schönheitsfehler — die
/// Manifest-Bitmaske nennt die *Art* der Autorität ("darf ein MMIO-Fenster halten"), nicht die
/// Anzahl der Fenster. Ein Treiber braucht zwei: seinen Konfigurationsraum (um sein Gerät
/// aufzulösen) und sein Registerfenster (um es zu bedienen).
/// **Woher der eigene Kanal einer PD kommt** (2026-08-25).
///
/// Beide Faelle belegen Slot 1 und 2 mit **ihren** Objekten statt mit frischen -- das war schon
/// vorher so und ist der Grund, warum der Parameter existiert. Was sie unterscheidet, ist das
/// **Badge** und die Ablage: zwei Melder mit demselben Etikett sind ein Melder, und ein Dienst
/// ohne Geraet ist kein Treiber.
#[derive(Clone, Copy)]
pub enum Kanal {
    /// HardwareLand-Backend: Endpoint und Notification sind ueber `cap_allowed` an die PD
    /// gebunden -- eine fremde Cap wuerde ihr gar nicht erst installiert.
    Geraet(usize, usize),
    /// Ein Dienst **ohne Geraet**: frisch gepraegt, keine Partner-Bindung.
    Dienst(usize, usize),
}

impl Kanal {
    fn ep(self) -> usize {
        match self {
            Kanal::Geraet(ep, _) | Kanal::Dienst(ep, _) => ep,
        }
    }
    fn ntfn(self) -> usize {
        match self {
            Kanal::Geraet(_, n) | Kanal::Dienst(_, n) => n,
        }
    }
}

/// **Wie viele Slots die Loader-ABI vergibt** — 0..=8, seit B2/B4 einschliesslich der `Irq`-Cap
/// (7) und der Notification, auf der ihr Interrupt ankommt (8).
///
/// Als Konstante und nicht als Literal an vier Stellen: die Zahl steht sonst in der Rückgabeart,
/// im Puffer, in der Aufräumschleife und in der Verdichtung, und beim nächsten Slot wandert sie an
/// dreien mit. *Eine Zahl, die ein Mensch parallel zur Wahrheit führt*, ist genau die Form, die
/// `MELDESTELLEN` gekostet hat.
pub const ENDOW_SLOTS: usize = 9;

/// **Wie viele Caps EIN Ladevorgang hoechstens vergibt** — alle Loader-Slots plus das eine Angebot
/// des Aufrufers (Konvention L2: hoechstens ein Cap, in Slot 0).
///
/// Die Zahl steht an **einer** Stelle, und sie hat schon eine gekostet: daneben lag ein
/// `let mut dense = [first; 8]` als Literal. Mit dem neunten Slot (B4) schrieb es ueber sein Ende,
/// der Ladepfad **panikte** — und weil ein Kernel-Panic den Knoten nicht mitreisst (B-6.2, der
/// Panic-Pfad haelt den Kern ohne IRQ-Maskierung und der naechste Tick holt ihn zurueck), sah das
/// von aussen aus wie „das Programm laedt nicht": Thread erzeugt, nie zugelassen, **keine
/// Fehlermeldung**. Vier der sechs Programme fielen aus, und die Ursache lag drei Ebenen weiter
/// unten in einer Zahl, die niemand mehr las.
///
/// *Eine Zahl, die ein Mensch parallel zur Wahrheit fuehrt*, in ihrer teuersten Form.
pub const LOAD_CAPS_MAX: usize = ENDOW_SLOTS + 1;

/// **Der Lader darf nie mehr zusagen, als eine PD halten darf.**
///
/// Ohne diesen Assert war die Zusage still unerfuellbar: mit `CAP_BUDGET_PER_PD = 8` und neun
/// vergebenen Slots blieb der letzte Cap einfach weg -- die PD lief, ihr Slot war leer, und die
/// Absage trug den Grund nicht. Jetzt bricht der Bau.
const _: () = assert!(
    LOAD_CAPS_MAX <= caprock_microkit::CAP_BUDGET_PER_PD,
    "ENDOW_SLOTS ueberschreitet CAP_BUDGET_PER_PD -- eine PD koennte ihre Ausstattung nicht halten"
);

fn endow_from_manifest(
    e: &ManifestEntry,
    channel: Option<Kanal>,
    dma_pages: u32,
    // **Wohin die Caps gehen** -- gebraucht fuer genau eine Eintragung: die Interrupt-Notification
    // muss in der PD stehen, BEVOR ihre Cap installiert wird (B4). Die HardwareLand-Politik
    // entscheidet ueber die Objekt-Id, nicht ueber die Cap, und sie prueft vor dem Einbau.
    ziel_pd: Option<usize>,
) -> Result<[Option<(usize, CapPtr)>; ENDOW_SLOTS], RootTaskError> {
    let channel_ep = channel.map(Kanal::ep);
    let channel_ntfn = channel.map(Kanal::ntfn);
    use caprock_loader::manifest as man;
    let mut out: [Option<(usize, CapPtr)>; ENDOW_SLOTS] = [None; ENDOW_SLOTS];
    // Bis hierher erzeugte Caps wieder abräumen, wenn ein späterer Schritt scheitert — sie sind
    // dann nirgends installiert und würden sonst in der geteilten Tabelle belegt bleiben.
    fn undo(out: &[Option<(usize, CapPtr)>; ENDOW_SLOTS]) {
        for &(_, cap) in out.iter().flatten() {
            let _ = crate::system::cap_delete(cap);
        }
    }
    // Was dieser Kernel heute NICHT erteilen kann, wird abgewiesen statt weggelassen.
    //
    // `CAP_PD_CONTROL` gehört dazu, und der Grund ist strukturell: eine PdControl-Cap bezeichnet
    // **eine bestimmte Ziel-PD**, die es beim Start des Root-Tasks noch gar nicht gibt — sie
    // entstünde erst mit dem, was er selbst lädt. Der ehrliche Weg wäre, dass `SYS_LOAD` die
    // PdControl-Cap der neu erzeugten PD zurückgibt; das ist eine ABI-Erweiterung und steht in
    // todo-A (A-3.2, wählbarer Empfangs-Slot). Bis dahin: nicht erteilbar, also nicht behaupten.
    //
    // `CAP_IRQ` ebenso, und der Grund liegt tiefer als „noch nicht gebaut": ein Geräte-Interrupt
    // käme auf x86 per MSI-X, und seit B-3.2 steht die Interrupt-Remapping-Tabelle auf lauter
    // „not present" — ein Gerät ohne IRTE kann keinen Interrupt auslösen, mit Absicht. Eine
    // IRTE-**Vergabe** gibt es nicht; sie gehört zu B-3 (Vergabe + Invalidierung über QI). Eine
    // IRQ-Cap zu erteilen, ohne sie binden zu können, wäre eine Autorität ohne Wirkung — und ein
    // Treiber, der auf einen Interrupt wartet, der strukturell nie kommt, hängt. Also abweisen.
    const GRANTABLE: u32 = man::CAP_LOADER
        | man::CAP_NOTIFICATION
        | man::CAP_ENDPOINT
        | man::CAP_MMIO
        | man::CAP_DMA
        | man::CAP_SHARED;
    if e.initial_caps & !GRANTABLE != 0 {
        return Err(RootTaskError::UnsupportedAuthority);
    }
    // MMIO und DMA hängen an **einer** Zuteilung: es ist dasselbe Gerät. Sie einzeln anzufordern
    // wäre ein halber Treiber — Register ohne Puffer oder Puffer ohne Gerät.
    if (e.initial_caps & man::CAP_MMIO != 0) != (e.initial_caps & man::CAP_DMA != 0) {
        return Err(RootTaskError::UnsupportedAuthority);
    }
    if e.initial_caps & man::CAP_LOADER != 0 {
        // Quelle 0 = Boot-Archiv (die einzige heute).
        match crate::system::install_loader_cap(0, Rights::RWX) {
            Ok(cap) => out[0] = Some((0, cap)),
            Err(_) => return Err(RootTaskError::NoResources),
        }
    }
    if e.initial_caps & man::CAP_NOTIFICATION != 0 {
        // **Wer sich meldet, muss unterscheidbar sein.** Der Root-Task und eine Treiber-PD melden
        // sich beide über eine endowte Notification; bekämen beide dasselbe Badge, wäre am
        // akkumulierten `pending` nicht mehr zu erkennen, wer gelaufen ist -- und ein Lauf, in dem
        // nur einer von beiden startete, sähe aus wie ein vollständiger.
        // **Drei Rollen, drei Badges, drei Ablagen.** Root-Task, Treiber (HardwareLand-Backend am
        // eigenen Kanal) und Client eines Dienstes melden sich alle ueber eine endowte
        // Notification. Waeren sie ununterscheidbar, saehe ein Lauf, in dem nur einer startete,
        // aus wie ein vollstaendiger -- und schlimmer: die zuletzt geladene PD ueberschriebe die
        // Ablage der frueheren, und der Kernel wartete auf ein Signal am falschen Objekt. Genau
        // das ist beim Bau von A-6.3 passiert.
        let is_root = e.policy_flags & man::POLICY_ROOT_TASK != 0;
        let is_driver = matches!(channel, Some(Kanal::Geraet(..)));
        let is_dienst = matches!(channel, Some(Kanal::Dienst(..)));
        // **Vier Rollen, vier Badges** (2026-08-25). Bis heute waren es drei, und `is_driver` war
        // `channel.is_some()` -- ein Dienst ohne Geraet haette damit das Treiber-Badge getragen
        // UND dessen Ablage ueberschrieben. Genau der Fehler, den A-6.3 schon einmal gekostet hat:
        // der Kernel wartete auf ein Signal am falschen Objekt.
        let badge = if is_root {
            ROOT_NTFN_BADGE
        } else if is_driver {
            DRIVER_NTFN_BADGE
        } else if is_dienst {
            SERVICE_NTFN_BADGE
        } else {
            CLIENT_NTFN_BADGE
        };
        // Ein HardwareLand-Backend bekommt die Notification **seines Kanals**, keine frische:
        // seine Cap-Policy erlaubt nur Caps des eigenen Kanals, und eine fremde waere abgewiesen
        // worden -- der Treiber haette dann Geraete-Autoritaet, aber keinen Weg, etwas zu sagen.
        let r = channel_ntfn.or_else(crate::system::create_notification).and_then(|ntfn| {
            // **Gebadgt**, nicht nackt: `SYS_SIGNAL` verodert das Badge der benutzten CAP in
            // `pending` -- das Nachrichtenwort spielt keine Rolle. Ohne Badge waere jedes Signal
            // ein ODER mit 0, und der Kernel saehe nicht, dass ueberhaupt jemand gerufen hat.
            let cap =
                crate::system::install_notification_cap_badged(ntfn as u32, Rights::RWX, badge)
                    .ok()?;
            // +1, damit 0 = "keine" bleibt
            if is_root {
                ROOT_NTFN.store(ntfn as u64 + 1, Ordering::Relaxed);
            } else if is_driver {
                DRIVER_NTFN.store(ntfn as u64 + 1, Ordering::Relaxed);
            } else if is_dienst {
                // Keine eigene Ablage: es gibt heute keinen Leser dafuer. Ein Zaehler ohne Leser
                // waere tote Sidecar-Arithmetik (Z26/A3) -- die Ablage kommt mit dem ersten
                // Pruefer, der sie braucht, und nicht auf Vorrat.
            } else {
                client_ntfn_store(e.program_id, ntfn);
            }
            Some(cap)
        });
        match r {
            Some(cap) => out[1] = Some((1, cap)),
            None => {
                undo(&out);
                return Err(RootTaskError::NoResources);
            }
        }
    }
    if e.initial_caps & man::CAP_ENDPOINT != 0 {
        // Wie bei der Notification: ein HardwareLand-Backend bekommt den Endpoint **seines
        // Kanals**. Ein frischer waere eine fremde Cap, die seine Policy abweist -- und der
        // Treiber haette eine Dienstschnittstelle, die niemand erreichen kann.
        // Reihenfolge: der eigene Kanal (HardwareLand-Backend), sonst der **benannte** Dienst,
        // sonst -- nur wenn es genau einen gibt -- eben dieser, sonst ein frischer Endpoint.
        //
        // **`service_id` ist der Punkt von A-5.4** (Z11b/Z11c). Vorher stand hier
        // `driver_service()` mit der Bedeutung „der zuletzt geladene". Bei einem Dienst konnte
        // „gib mir einen Endpoint" nur dessen Kanal meinen; bei zweien waere es „gib mir
        // irgendeinen" gewesen, und welchen, haette die Ladereihenfolge entschieden. Jetzt steht
        // die Antwort im Autoritaetsdokument -- und wo sie fehlt und mehrdeutig waere, liefert
        // `driver_service()` bewusst `None` statt zu raten.
        // **Ein benannter Dienst, den es nicht gibt, ist eine ABSAGE** (2026-08-25).
        //
        // Bis heute fiel dieser Fall auf `create_endpoint` durch: der Client bekam einen frischen,
        // **unverbundenen** Endpoint, der Dienst behielt seinen, und beide hatten einen Kanal
        // ohne Gegenseite -- mit gueltigen Caps und ohne eine einzige Fehlermeldung. Genau das
        // Bild, das der Netzstack-Entwurf beschreibt.
        //
        // Und es ist keine Randlage, sondern eine **Reihenfolgefrage**: `init` laedt die
        // Startmenge in Index-Ordnung. Ein Client mit kleinerem Index als sein Dienst kommt hier
        // zwangslaeufig zu frueh an. Fail-closed macht daraus einen benannten Ladefehler statt
        // eines stillen Fehlkanals -- die Reihenfolge im Manifest wird damit zu einer Bedingung,
        // die sich MELDET, wenn sie verletzt ist.
        if channel_ep.is_none() && e.service_id != 0 && driver_service_of(e.service_id).is_none() {
            undo(&out);
            return Err(RootTaskError::NoResources);
        }
        let ziel = channel_ep
            .or_else(|| {
                (e.service_id != 0)
                    .then(|| driver_service_of(e.service_id))
                    .flatten()
                    .map(|s| s.ep)
            })
            .or_else(|| (e.service_id == 0).then(driver_service).flatten().map(|s| s.ep));
        // **Hier wird erfasst, nicht im Bericht** (2026-08-25). Die Aussage „der Client bekam den
        // Endpoint SEINES Dienstes" gilt in diesem Augenblick; wer sie spaeter am Cap-Slot der PD
        // nachliest, liest sie an einem Thread, der laengst fertig und gestorben sein darf --
        // `wasmhost` tut genau das. Dieselbe Falle wie `drv : ALL PASS`, das den Puffer der
        // Treiber-PD im BERICHT las und damit den Sektor des letzten Clients erwischte.
        if let Some(ep) = ziel {
            if e.service_id != 0 {
                client_ep_merken(e.program_id, ep);
            }
        }
        let r = ziel.or_else(crate::system::create_endpoint).and_then(|ep| {
            crate::system::install_endpoint_cap(ep as u32, Rights::RWX).ok()
        });
        match r {
            Some(cap) => out[2] = Some((2, cap)),
            None => {
                undo(&out);
                return Err(RootTaskError::NoResources);
            }
        }
    }
    // A-5.1: die Geräte-Autorität. Eine Zuteilung, drei Caps — und **fail-closed**: gibt es kein
    // zuteilbares Gerät (oder ist es schon vergeben), startet der Treiber gar nicht. Ihn ohne
    // Gerät laufen zu lassen hieße, einen Dienst zu starten, der seine Aufgabe nicht erfüllen
    // kann und das erst an einer Stelle merkt, die niemand mit dem Manifest in Verbindung bringt.
    // A-5.3: **welches** Gerät, sagt jetzt der Eintrag. `DeviceSelector::ANY` (auch: ein vor A-5.3
    // erzeugtes Manifest) verhält sich wie bisher.
    if e.initial_caps & man::CAP_MMIO != 0 {
        match crate::system::assign_driver_device(e.device, e.program_id, dma_pages) {
            Some(g) => {
                out[3] = Some((3, g.cfg));
                out[4] = Some((4, g.bar));
                out[5] = Some((5, g.dma));
                out[6] = Some((6, g.shared));
                // B2: Slot 7 nur, wenn das Geraet wirklich einen Vektor bekommen hat. Ein leerer
                // Slot ist hier die ehrliche Auskunft und kein Mangel -- ein Treiber ohne Vektor
                // pollt zulaessig, und eine Platzhalter-Cap machte aus dieser Aussage eine Luege,
                // die erst beim `BIND_IRQ` auffiele.
                if let Some(irq) = g.irq {
                    out[7] = Some((7, irq));
                }
                // Slot 8 kommt mit Slot 7 oder gar nicht -- s. `DriverGrant::irq_ntfn`.
                if let Some(n) = g.irq_ntfn {
                    // **Erst eintragen, dann einbauen.** Andersherum weist `cap_allowed` die Cap
                    // ab: eine HardwareLand-PD darf Notification-Caps nur fuer Objekte halten, die
                    // der Kernel ihr zugeordnet hat -- und das ist die Regel, nicht der Fehler.
                    if let Some(pd) = ziel_pd {
                        crate::system::pd_set_irq_ntfn(pd, g.irq_ntfn_id);
                    }
                    out[8] = Some((8, n));
                }
            }
            None => {
                undo(&out);
                return Err(RootTaskError::NoDevice);
            }
        }
    } else if e.initial_caps & man::CAP_SHARED != 0 {
        // **Die geteilte Uebertragungsflaeche eines Dienstes** (A-6.3, seit 2026-08-25 an einem
        // eigenen Bit).
        //
        // Die Regel bleibt die von MMIO/DMA: das Dokument nennt die **Art** der Autoritaet, die
        // **Instanz** teilt der Kernel-Glue zu -- hier ueber `service_id`.
        //
        // **Warum das nicht mehr an `CAP_ENDPOINT` haengt:** die Flaeche kam bis heute mit dem
        // Endpoint, aber nur, wenn der benannte Dienst zufaellig ein Geraet hatte
        // (`driver_shared_cap` sucht in den Geraetezuteilungen). Damit hing die Slot-Zahl eines
        // Programms an einer Eigenschaft einer FREMDEN PD. Eine feste Kopplung waere ablesbar
        // gewesen, eine bedingte ist es nicht -- und bei einem Budget von acht Slots ist
        // Ablesbarkeit der ganze Zweck des Dokuments.
        //
        // **Fail-closed bleibt es trotzdem:** wer die Flaeche fordert und keinen Dienst mit einer
        // bekommt, faellt hier durch -- statt still ohne sie zu starten.
        match crate::system::driver_shared_cap(e.service_id) {
            Some(shared) => out[6] = Some((6, shared)),
            None => {
                undo(&out);
                return Err(RootTaskError::NoResources);
            }
        }
    }
    Ok(out)
}

/// **Badge der Root-Notification.** Ein hohes Bit, damit es sich mit den Badges, die der Root-Task
/// selbst an seine Kinder vergibt (unteres Wort), nicht vermischt: ein akkumuliertes Badge soll
/// beide Aussagen getrennt lesbar lassen, statt sie ineinander laufen zu lassen.
pub const ROOT_NTFN_BADGE: u64 = 1 << 32;

/// Die Notification, die dem Root-Task endowt wurde (`0` = keine). Der Selbsttest liest daran ab,
/// ob er wirklich gelaufen ist — der Kernel hat sonst kein Fenster in einen isolierten Prozess.
static ROOT_NTFN: AtomicU64 = AtomicU64::new(0);

/// **Welchen Endpoint ein Client mit benanntem Dienst bekommen hat** (2026-08-25).
///
/// `(program_id, ep + 1)`; `0` = kein Eintrag. Erfasst im Endowment, gelesen im Bericht -- die
/// beiden Zeitpunkte sind verschieden, und genau deshalb steht der Wert hier und nicht im
/// Cap-Slot der PD: der Client darf zwischendurch fertig werden und sterben.
static CLIENT_EP_PID: [core::sync::atomic::AtomicU32; MAN_MAX_ENTRIES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAN_MAX_ENTRIES];
static CLIENT_EP_ID: [AtomicU64; MAN_MAX_ENTRIES] =
    [const { AtomicU64::new(0) }; MAN_MAX_ENTRIES];

fn client_ep_merken(program_id: u32, ep: usize) {
    for (pid, id) in CLIENT_EP_PID.iter().zip(CLIENT_EP_ID.iter()) {
        let cur = pid.load(Ordering::Relaxed);
        if cur == program_id || cur == 0 {
            pid.store(program_id, Ordering::Relaxed);
            id.store(ep as u64 + 1, Ordering::Relaxed);
            return;
        }
    }
}

/// Den erfassten Endpoint eines Clients lesen (Bericht). `None` = dieser Client hat nie einen
/// **benannten** Dienst aufgeloest.
pub fn client_ep_of(program_id: u32) -> Option<usize> {
    CLIENT_EP_PID
        .iter()
        .zip(CLIENT_EP_ID.iter())
        .find(|(pid, _)| pid.load(Ordering::Relaxed) == program_id)
        .map(|(_, id)| id.load(Ordering::Relaxed))
        .filter(|&v| v != 0)
        .map(|v| v as usize - 1)
}

/// **Badge eines Dienstes OHNE Geraet** (2026-08-25). Ein eigenes hohes Bit aus demselben Grund
/// wie die drei daneben: ein akkumuliertes Badge soll die Melder getrennt lesbar lassen.
pub const SERVICE_NTFN_BADGE: u64 = 1 << 48;

/// **Badge einer Treiber-PD** (A-5.1). Wieder ein hohes Bit und ein anderes als
/// [`ROOT_NTFN_BADGE`]: ein akkumuliertes Badge soll "der Root-Task lief" und "der Treiber lief"
/// getrennt lesbar lassen. Zwei Melder mit demselben Etikett sind ein Melder.
pub const DRIVER_NTFN_BADGE: u64 = 1 << 40;

/// Die Notification einer Treiber-PD (`0` = keine endowt).
static DRIVER_NTFN: AtomicU64 = AtomicU64::new(0);

/// **Was über einen geladenen Treiber-Dienst bekannt ist** (A-5.1).
///
/// Genau das, was man braucht, um ihn **auszutauschen** — und nichts darüber hinaus: an welchem
/// Endpoint er hängt, in welcher PD er läuft, welcher Thread gerade Empfänger ist, und welches
/// Archivmodul ihn stellt. Was er *treibt*, steht hier nicht. Das ist die Aussage von A-5.1: der
/// Kernel ersetzt einen Empfänger an einem Endpoint; dass dahinter virtio steckt, weiß er nicht.
#[derive(Clone, Copy)]
pub struct DriverService {
    /// Der Kanal-Endpoint — die Dienstschnittstelle. Er überlebt den Austausch.
    pub ep: usize,
    /// Die Kanal-Notification (Melde-/Bereit-Kanal).
    pub ntfn: usize,
    /// Die PD der **laufenden** Fassung.
    pub pd: usize,
    /// Der Thread der laufenden Fassung — der Empfänger, der ersetzt wird.
    pub tid: ThreadId,
    /// Archiv-Index des Moduls, aus dem die nächste Fassung geladen wird.
    pub index: u32,
    /// **Wer** dieser Dienst ist — die `program_id` aus dem Manifest (A-5.4).
    pub program_id: u32,
}

/// Höchstzahl gleichzeitig laufender Treiber-Dienste.
const MAX_SERVICES: usize = 4;

/// **Die laufenden Treiber-Dienste.**
///
/// Bis A-5.3 war das ein einzelner Platz mit der Bedeutung „der zuletzt geladene". Das ging gut,
/// solange es einen gab. Bei zweien ist es eine **stille Fehlwahl**: der zweite überschriebe den
/// ersten, ein Client bekäme den Endpoint des falschen Dienstes, und ein Hot-Reload träfe den
/// falschen Empfänger — alles mit gültigen Caps und ohne eine einzige Fehlermeldung.
static DRIVER_SERVICES: SpinLock<[Option<DriverService>; MAX_SERVICES]> =
    SpinLock::new([None; MAX_SERVICES]);

/// Hoechstzahl der Manifest-Eintraege -- die Schranke aller Registertabellen hier ist damit
/// **hergeleitet** und nicht erfunden.
const MAN_MAX_ENTRIES: usize = caprock_loader::manifest::MAX_ENTRIES;

/// **Welches PROGRAMM gehoert zu diesem Thread** — die Zuordnung, die einer Fehlermeldung erst
/// eine Diagnose macht.
///
/// „`el0-trap: User-Thread 0x8 faultete (FAR=0x3c28000)`" nennt eine Zahl, die niemand zuordnen
/// kann; drei solcher Zeilen sahen am 2026-08-10 gleich aus, waehrend genau eine davon die
/// gesuchte war. Dasselbe Muster wie bei `manifest:   im Archiv liegen N B, die nicht parsen`:
/// abgewiesen wird richtig, gesagt wird nichts.
///
/// Fassungsvermoegen = Hoechstzahl der Manifest-Eintraege, also **hergeleitet**; wer nicht
/// hineinpasst, wird gezaehlt statt still vergessen.
static PROG_TIDS: [core::sync::atomic::AtomicU32; MAN_MAX_ENTRIES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAN_MAX_ENTRIES];
/// Rohe `ThreadId` zum Eintrag darueber, `+1` (damit `0` = „frei" bleibt; Slot 0/Generation 0 ist
/// eine gueltige `ThreadId`).
static PROG_TID_RAW: [AtomicU64; MAN_MAX_ENTRIES] =
    [const { AtomicU64::new(0) }; MAN_MAX_ENTRIES];
/// Nicht eingetragene Zuordnungen. Heute unerreichbar (s. o.) -- gezaehlt, weil eine Schranke,
/// deren Ueberlauf niemand benennt, kein Schutz ist (D11).
static PROG_TID_LOST: AtomicU64 = AtomicU64::new(0);

/// Zuordnung eintragen. Ein Programm kann im Betrieb neu geladen werden (Hot-Reload) -- dann
/// ersetzt der neue Thread den alten unter derselben `program_id`.
pub fn record_program_thread(program_id: u32, tid: ThreadId) {
    for (pid, raw) in PROG_TIDS.iter().zip(PROG_TID_RAW.iter()) {
        let cur = pid.load(Ordering::Relaxed);
        if cur == program_id || cur == 0 {
            pid.store(program_id, Ordering::Relaxed);
            raw.store(tid.to_raw() + 1, Ordering::Relaxed);
            return;
        }
    }
    PROG_TID_LOST.fetch_add(1, Ordering::Relaxed);
}

/// **Zu welchem Programm gehoert dieser Thread?** `None` = keine Zuordnung bekannt (Kernel-Thread,
/// Testfaden, oder ein Thread, den kein Ladepfad erzeugt hat) -- und das heisst *unbekannt*, nicht
/// *keins*.
pub fn program_of_thread(tid: ThreadId) -> Option<u32> {
    let ziel = tid.to_raw() + 1;
    PROG_TIDS
        .iter()
        .zip(PROG_TID_RAW.iter())
        .find(|(_, raw)| raw.load(Ordering::Relaxed) == ziel)
        .map(|(pid, _)| pid.load(Ordering::Relaxed))
}

/// Wie [`program_of_thread`], aber ueber den **Thread-Slot**: die Stack-Wasserstandsmarke (C4)
/// misst in `reclaim_user_kstack`, und dort ist der Slot bekannt, die volle `ThreadId` nicht mehr
/// zuverlaessig. Diagnose, keine Autoritaet — bei einem wiederverwendeten Slot kann die Antwort
/// die Generation daneben liegen, und genau deshalb steht sie in einer Berichtszeile und nicht in
/// einer Entscheidung.
pub fn program_of_slot(slot: usize) -> Option<u32> {
    if slot == usize::MAX {
        return None;
    }
    PROG_TIDS
        .iter()
        .zip(PROG_TID_RAW.iter())
        .find(|(_, raw)| {
            let v = raw.load(Ordering::Relaxed);
            v != 0 && ThreadId::from_raw(v - 1).slot() == slot
        })
        .map(|(pid, _)| pid.load(Ordering::Relaxed))
}

/// **Der Thread dieses Programms** — die Gegenrichtung zu [`program_of_thread`].
///
/// Damit ist ein Programm direkt befragbar: existiert sein Thread, ist er zugelassen, worin
/// blockiert er? Das sind Fragen an den **Scheduler**, nicht an eine Cap — und damit die einzigen,
/// die etwas über eine PD sagen, deren Cap-Pfad selbst in Frage steht.
pub fn thread_of_program(program_id: u32) -> Option<ThreadId> {
    PROG_TIDS
        .iter()
        .zip(PROG_TID_RAW.iter())
        .find(|(p, _)| p.load(Ordering::Relaxed) == program_id)
        .and_then(|(_, raw)| {
            let v = raw.load(Ordering::Relaxed);
            (v != 0).then(|| ThreadId::from_raw(v - 1))
        })
}

/// **VOLLZAEHLIGKEIT: steht fuer jeden Manifest-Eintrag auch ein geladenes Programm?**
///
/// Die Zeile, die es sechs Wochen lang nicht gab -- und deren Fehlen den ganzen wasm-Fall
/// getragen hat. `wasmhost` scheiterte seit `94a92ea` beim Laden; **keine einzige Pruefung hat es
/// bemerkt**, weil niemand die beiden Zahlen verglich. Gefunden wurde es ueber die Umwege zweier
/// kaputter Pruefer.
///
/// Vier Pruefer waren an dem Fall beteiligt und alle vier waren kaputt. Der fuenfte -- dieser --
/// existierte nicht, und das war die eigentliche Luecke: ein stiller Ladeausfall hatte nichts,
/// woran er haette auffallen koennen.
///
/// Rueckgabe: `(erwartet, geladen, fehlende program_ids, Zahl der fehlenden)`. Die **Namen** der
/// Fehlenden, nicht bloss eine Differenz -- „einer fehlt" ist keine Diagnose.
pub fn vollzaehligkeit(fehlend: &mut [u32]) -> (usize, usize, usize) {
    let Some(man) = read_manifest() else {
        return (0, 0, 0); // ohne Manifest gibt es keine Sollmenge -- das ist KEIN Befund
    };
    let mut erwartet = 0usize;
    let mut geladen = 0usize;
    let mut n = 0usize;
    for i in 0..man.count() {
        let Some(e) = man.entry(i) else { continue };
        erwartet += 1;
        if thread_of_program(e.program_id).is_some() {
            geladen += 1;
        } else if n < fehlend.len() {
            fehlend[n] = e.program_id;
            n += 1;
        }
    }
    (erwartet, geladen, n)
}

/// Wie viele Zuordnungen bekannt sind und wie viele verlorengingen.
pub fn program_thread_stats() -> (usize, u64) {
    (
        PROG_TIDS
            .iter()
            .filter(|p| p.load(Ordering::Relaxed) != 0)
            .count(),
        PROG_TID_LOST.load(Ordering::Relaxed),
    )
}

/// Der **eindeutige** Treiber-Dienst — `None`, wenn es keinen oder **mehrere** gibt.
///
/// „Mehrere" liefert bewusst `None` und nicht „den ersten": eine Auswahl, die niemand
/// aufgeschrieben hat, ist schlimmer als eine Verweigerung. Wer einen bestimmten meint, nimmt
/// [`driver_service_of`].
pub fn driver_service() -> Option<DriverService> {
    let g = DRIVER_SERVICES.lock();
    let mut it = g.iter().flatten();
    let first = *it.next()?;
    it.next().is_none().then_some(first)
}

/// Genau **diesen** Dienst (nach `program_id`).
pub fn driver_service_of(program_id: u32) -> Option<DriverService> {
    DRIVER_SERVICES
        .lock()
        .iter()
        .flatten()
        .find(|s| s.program_id == program_id)
        .copied()
}

/// Wie viele Dienste laufen (Bericht).
pub fn driver_service_count() -> usize {
    DRIVER_SERVICES.lock().iter().flatten().count()
}

/// Die laufende Fassung eines Treiber-Dienstes vermerken (Erstladen und nach einem Austausch).
///
/// Ein bereits vorhandener Eintrag **derselben** `program_id` wird ersetzt — das ist der
/// Austauschfall; ein neuer belegt den nächsten freien Platz.
pub fn set_driver_service(s: DriverService) {
    let mut g = DRIVER_SERVICES.lock();
    if let Some(slot) = g
        .iter_mut()
        .find(|x| x.map(|v| v.program_id) == Some(s.program_id))
    {
        *slot = Some(s);
        return;
    }
    if let Some(slot) = g.iter_mut().find(|x| x.is_none()) {
        *slot = Some(s);
    }
}

/// Wie ein Austausch ausging (A-5.1). Bewusst **unterscheidbar**: „hat nicht geklappt" ist als
/// Diagnose wertlos, und der Unterschied zwischen „keine neue Fassung geladen" und „umgebunden,
/// aber mit Lücke" ist der Unterschied zwischen einem Aufbaufehler und einem Isolationsbefund.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReloadOutcome {
    /// Umgebunden, und der Endpoint hatte zu **keinem** Zeitpunkt null Empfänger (die neue
    /// Fassung stand schon bereit, als umgebunden wurde). Die starke Zusicherung aus A-4.1.
    DoneOverlapped,
    /// Umgebunden, aber die neue Fassung wurde beim Tausch eingereiht statt vorher bereitzustehen.
    /// Zulässig, aber die schwächere Aussage.
    Done,
    /// Kein Treiber-Dienst bekannt.
    NoService,
    /// Die neue Fassung liess sich nicht aufsetzen (PD, Zuteilung oder Laden).
    NotLoaded,
    /// Die neue Fassung stand nicht rechtzeitig am Endpoint bereit.
    NotReady,
    /// Das Umbinden selbst wurde abgewiesen (Grund s. `Rebind`).
    Rejected,
}

/// **Den Treiber-Dienst durch eine neue Fassung ersetzen** (A-5.1, Richtungsumkehr).
///
/// Hier steht das eigentliche Ergebnis dieses Punktes: der Kernel tauscht einen **Empfänger an
/// einem Endpoint** aus. Er lädt dasselbe Archivmodul in eine zweite Backend-PD **am selben
/// Kanal**, gibt ihr dieselbe Gerätezuteilung, wartet, bis sie bereit ist, und bindet dann um.
///
/// **In dieser Funktion kommt das Wort „virtio" nicht vor**, und das ist kein Zufall, sondern die
/// Abnahmebedingung: solange der Kernel wüsste, was der Treiber treibt, wäre „austauschbar" eine
/// Eigenschaft dieses einen Treibers und nicht des Mechanismus.
///
/// Reihenfolge, und sie ist nicht verhandelbar:
/// 1. neue Fassung laden — sie ruft ihr `recv` und steht als **zweiter** Empfänger bereit;
/// 2. **erst dann** stilllegen (A-4.2): ohne Stilllegung wäre jede Vorbedingung des Tauschs eine
///    Momentaufnahme, die ein `CALL` auf einem anderen Kern falsch macht, bevor getauscht wird;
/// 3. umbinden (A-4.1) — weil (1) vor (2) lag, ist der Ausgang `overlapped`, der Endpoint hatte
///    also durchgehend einen Empfänger;
/// 4. Stilllegung aufheben, alte Fassung entkoppeln.
pub fn reload_driver(program_id: u32) -> ReloadOutcome {
    use caprock_ipc::Rebind;
    // **Genau dieser Dienst** (A-5.4). `driver_service()` liefert bei zwei Diensten bewusst
    // `None` -- „der zuletzt geladene" waere hier keine Abkuerzung, sondern ein Austausch am
    // falschen Empfaenger, und zwar lautlos.
    let Some(old) = driver_service_of(program_id) else {
        return ReloadOutcome::NoService;
    };
    let Some(archive) = read_archive() else {
        return ReloadOutcome::NotLoaded;
    };
    let Some(prog) = archive.program(old.index as usize) else {
        return ReloadOutcome::NotLoaded;
    };
    // Zweite Backend-PD am SELBEN Kanal. Der Endpoint ist das, was den Austausch ueberlebt.
    let partner = crate::system::pd_partner(old.pd).unwrap_or(0);
    let Some(v2_pd) =
        crate::system::create_hardware_backend_on(partner, old.index as u16, old.ep, old.ntfn)
    else {
        return ReloadOutcome::NotLoaded;
    };
    // Dieselbe Zuteilung, neue Caps. Das Geraet wird NICHT losgelassen -- s. `reassign_driver_device`.
    //
    // **Die Zuteilung wird ueber die `program_id` gefunden** (A-5.4), nicht ueber „die erste
    // benutzte". `prog.program_id` kommt aus dem Archiv-Eintrag derselben Komponente, die gerade
    // ersetzt wird -- bei zwei Treibern haette die alte Fassung sonst der Nachfolgerin ein FREMDES
    // Geraet gegeben, und zwar lautlos, weil alle Caps gueltig gewesen waeren.
    let Some(g) = crate::system::reassign_driver_device(prog.program_id) else {
        return ReloadOutcome::NotLoaded;
    };
    let ntfn_cap = crate::system::install_notification_cap_badged(
        old.ntfn as u32,
        Rights::RWX,
        DRIVER_V2_BADGE,
    );
    let ep_cap = crate::system::install_endpoint_cap(old.ep as u32, Rights::RWX);
    let (Ok(ntfn_cap), Ok(ep_cap)) = (ntfn_cap, ep_cap) else {
        return ReloadOutcome::NotLoaded;
    };
    // **Slot 6 gehoert dazu.** Beim ersten Anlauf fehlte er hier, und die neue Fassung endete
    // sofort: der Treiber verlangt seine Uebertragungsflaeche und bricht ohne sie ab -- richtig
    // so. Der Austausch meldete dann `NotReady`, was nach einem Zeitproblem aussieht und ein
    // fehlendes Endowment war. Wer eine Fassung ersetzt, muss ihr ALLES geben, was die alte hatte.
    let mut endow = [
        (1usize, ntfn_cap),
        (2, ep_cap),
        (3, g.cfg),
        (4, g.bar),
        (5, g.dma),
        (6, g.shared),
        (7, g.shared), // Platzhalter, s. unten -- nur die ersten `n` werden uebergeben
        (8, g.shared), // dito
    ];
    // **Slot 7 gehoert seit B2 dazu** -- und wenn er fehlt, ist es derselbe Fehler wie damals bei
    // Slot 6: die neue Fassung braeche ab, der Austausch meldete `NotReady`, und das sieht nach
    // einem Zeitproblem aus statt nach einem fehlenden Cap.
    //
    // Der Platzhalter oben wird nur ueberschrieben, nie uebergeben: `&endow[..n]` schneidet ihn ab,
    // wenn das Geraet keinen Vektor hat. Ein Array ohne Platzhalter waere hier ein `Option`-Array
    // mit einer zweiten Verdichtung -- mehr Mechanik fuer denselben Schnitt.
    let n = match (g.irq, g.irq_ntfn) {
        (Some(irq), Some(nt)) => {
            // **Erst die Erlaubnis, dann die Cap** -- dieselbe Reihenfolge wie beim Erstladen.
            // Die HardwareLand-Politik entscheidet ueber die Objekt-Id, und die neue PD kennt sie
            // noch nicht: ohne diese Zeile weist sie Slot 8 ab, der Austausch meldet `NotReady`,
            // und das sieht nach einem Zeitproblem aus statt nach einer fehlenden Eintragung.
            // *Wer eine Fassung ersetzt, muss ihr ALLES geben, was die alte hatte* -- und dazu
            // gehoert nicht nur die Cap, sondern die Erlaubnis, sie zu halten.
            crate::system::pd_set_irq_ntfn(v2_pd, g.irq_ntfn_id);
            endow[6] = (7, irq);
            endow[7] = (8, nt);
            8
        }
        _ => 6,
    };
    let Ok(v2) = load_program_into_pd(&prog, v2_pd, &endow[..n], boot_arg(old.index as usize, archive.count()))
    else {
        return ReloadOutcome::NotLoaded;
    };

    // (1) Warten, bis die neue Fassung wirklich am Endpoint steht. **Nicht** blind umbinden: ein
    // Tausch auf eine Fassung, die noch nicht empfaengt, waere genau die Luecke, die A-4.1
    // ausschliessen soll.
    let mut ready = false;
    for _ in 0..200_000_000u64 {
        if crate::system::endpoint_quiescence_of(old.ep, v2).as_receiver {
            ready = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !ready {
        return ReloadOutcome::NotReady;
    }
    // (2) Stilllegen, (3) umbinden, (4) freigeben.
    crate::system::endpoint_begin_quiesce(old.ep);
    let r = crate::system::endpoint_rebind_server(old.ep, old.tid, v2);
    crate::system::endpoint_end_quiesce(old.ep);
    let out = match r {
        Rebind::Done { overlapped: true } => ReloadOutcome::DoneOverlapped,
        Rebind::Done { overlapped: false } => ReloadOutcome::Done,
        _ => return ReloadOutcome::Rejected,
    };
    // Die alte Fassung entkoppeln: ohne Endpoint-Cap laeuft ihr `recv` ins Leere, und sie endet
    // von selbst. Sie zu killen waere haerter als noetig -- ein Dienst, der geordnet enden kann,
    // soll das auch duerfen.
    crate::system::endpoint_retire_receiver(old.ep, old.tid);
    crate::system::clear_pd_cap(old.pd, 2);
    set_driver_service(DriverService { pd: v2_pd, tid: v2, ..old });
    out
}

/// **Badge der zweiten Fassung** (A-5.1). Ein anderes als [`DRIVER_NTFN_BADGE`], damit am
/// akkumulierten Badge ablesbar ist, dass wirklich eine ZWEITE Fassung lief — und nicht bloss die
/// erste ein zweites Mal gemeldet hat.
pub const DRIVER_V2_BADGE: u64 = 1 << 41;

/// **Badge einer Client-PD** eines Dienstes (A-6.3) — wieder ein eigenes Bit.
pub const CLIENT_NTFN_BADGE: u64 = 1 << 44;

/// **Die Notification einer Client-PD — je PROGRAMM, nicht je ROLLE.**
///
/// Bis zum 2026-08-10 stand hier **ein** Slot. Die Behebung von A-6.3 lautete „drei Rollen, drei
/// Badges, drei Ablagen" — und *Client* ist eine **Rolle**, keine Instanz. Mit der zweiten
/// Client-PD (`wasmhost`, 2026-08-09) kam derselbe Fehler eine Ebene höher zurück: die zuletzt
/// geladene überschrieb die Ablage der früheren, der `drv`-Ablauf wartete auf das Badge der
/// **Dateisystem**-PD an **wasmhosts** Objekt, und drei Prüfzeilen (`drv`/`blkdev`/`part`) fielen
/// aus, **ohne dass am Treiber irgendetwas kaputt war**.
///
/// Gefunden per Bisect (erster schlechter Commit `a159b6b`), isoliert durch Weglassen genau dieses
/// einen Archiveintrags — der Commit änderte auch 69 Zeilen Bring-up, und ohne die Isolation wäre
/// nur der Commit bekannt, nicht die Ursache.
///
/// **Die Schranke ist hergeleitet, nicht erfunden:** so viele Einträge, wie ein Manifest überhaupt
/// haben darf ([`MAN_MAX_ENTRIES`]). Damit ist ein Überlauf **strukturell unerreichbar** statt
/// ein ungetesteter Pfad — dieselbe Lehre wie bei der `audit_cdt`-Schranke (B-5.5). Der Zähler
/// unten bleibt trotzdem, weil eine Schranke, deren Überlauf niemand benennt, kein Schutz ist
/// (D11), sondern ein Loch.
///
/// `0` in der Programm-Spalte heißt „frei" — `program_id` 0 vergibt kein Manifest.
static CLIENT_NTFN_PID: [core::sync::atomic::AtomicU32; MAN_MAX_ENTRIES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAN_MAX_ENTRIES];
/// Zugehörige Notification-Id, `+1` (damit `0` = „keine" bleibt).
static CLIENT_NTFN_ID: [AtomicU64; MAN_MAX_ENTRIES] =
    [const { AtomicU64::new(0) }; MAN_MAX_ENTRIES];
/// Verlorene Eintragungen. **Heute unerreichbar** (s. o.) — und genau deshalb gezählt statt
/// wegkommentiert: der Tag, an dem die Schranke sich ändert, ist der Tag, an dem das zählt.
static CLIENT_NTFN_LOST: AtomicU64 = AtomicU64::new(0);

/// Eintragen. Läuft ausschließlich im Ladepfad, der je Programm einmal und **sequenziell**
/// durchlaufen wird — deshalb genügt `Relaxed` ohne Sperre, wie bei den drei Vorgängern.
fn client_ntfn_store(program_id: u32, ntfn: usize) {
    for (p, n) in CLIENT_NTFN_PID.iter().zip(CLIENT_NTFN_ID.iter()) {
        let cur = p.load(Ordering::Relaxed);
        if cur == program_id || cur == 0 {
            p.store(program_id, Ordering::Relaxed);
            n.store(ntfn as u64 + 1, Ordering::Relaxed);
            return;
        }
    }
    CLIENT_NTFN_LOST.fetch_add(1, Ordering::Relaxed);
}

/// Die Notification-Id **dieser** Client-PD (`None` = keine endowt).
///
/// Es gibt bewusst **keine** Fassung ohne Argument mehr. „Die Client-Notification" war der Name
/// einer Mehrdeutigkeit: zwei Aufrufstellen meinten zwei verschiedene PDs und lasen dieselbe
/// Zelle. Wer hier nicht sagen kann, **wessen** Signal er meint, hat die Frage nicht gestellt.
pub fn client_notification_of(program_id: u32) -> Option<usize> {
    for (p, n) in CLIENT_NTFN_PID.iter().zip(CLIENT_NTFN_ID.iter()) {
        if p.load(Ordering::Relaxed) == program_id {
            let v = n.load(Ordering::Relaxed);
            return (v != 0).then(|| (v - 1) as usize);
        }
    }
    None
}

/// Wie viele Client-PDs eine Notification bekommen haben, und wie viele **verlorengingen**.
/// Für den Bericht: `(0, 0)` ist von „es gab keine Clients" nicht zu unterscheiden, und genau
/// diese Ununterscheidbarkeit hat den Fehler oben getragen.
/// Die Notification-**Ids** der Client-PDs, fuer den Bericht: `(program_id, ntfn_id)`.
///
/// Ohne die Ids ist eine **Id-Kollision** nicht sichtbar -- und genau die steht als Verdacht im
/// Raum, seit die ROOT-Notification einmal das CLIENT-Badge trug (`0x1000_0000_0000`, Bit 44).
/// Eine Zahl, die zweimal vorkommt, ist eine Antwort; „das Badge stimmt nicht" ist keine.
pub fn client_notification_ids(out: &mut [(u32, usize)]) -> usize {
    let mut n = 0;
    for (pid, raw) in CLIENT_NTFN_PID.iter().zip(CLIENT_NTFN_ID.iter()) {
        let p = pid.load(Ordering::Relaxed);
        let v = raw.load(Ordering::Relaxed);
        if p != 0 && v != 0 && n < out.len() {
            out[n] = (p, (v - 1) as usize);
            n += 1;
        }
    }
    n
}

pub fn client_notification_stats() -> (usize, u64) {
    let n = CLIENT_NTFN_PID
        .iter()
        .filter(|p| p.load(Ordering::Relaxed) != 0)
        .count();
    (n, CLIENT_NTFN_LOST.load(Ordering::Relaxed))
}

/// Die Notification-Id der Treiber-PD (`None` = keine endowt).
pub fn driver_notification() -> Option<usize> {
    let v = DRIVER_NTFN.load(Ordering::Relaxed);
    (v != 0).then(|| (v - 1) as usize)
}

/// Die Notification-Id des Root-Tasks (`None` = keine endowt).
pub fn root_notification() -> Option<usize> {
    match ROOT_NTFN.load(Ordering::Relaxed) {
        0 => None,
        n => Some((n - 1) as usize),
    }
}

/// **Den Root-Task starten** (A-2.1) — der seL4-Weg: ein Startprogramm aus der Startmenge laden
/// und ihm die Wurzel-Caps übergeben.
///
/// Ab hier ist jede weitere Fähigkeit ein **Userland-Programm** statt eines Kernel-Patches; das
/// ist der eigentliche Hebel des Plans, und deshalb steht dieser Aufruf im Hochlauf und nicht
/// hinter einem Test-Feature.
///
/// Reihenfolge, und jede Stufe hat einen Grund:
/// 1. Manifest (signiert, an dieses Kernel-Image gebunden) — **wer** darf **was**;
/// 2. Eintrag mit [`POLICY_ROOT_TASK`](caprock_loader::manifest::POLICY_ROOT_TASK), eindeutig;
/// 3. das Modul im Archiv finden und seinen **Hash gegen das Manifest** prüfen. Ohne diesen
///    Schritt sagte das Manifest nur, *dass* etwas geladen wird, nicht *was*;
/// 4. Domäne von Manifest und Archiv müssen übereinstimmen (zwei Quellen, eine Aussage);
/// 5. Anfangs-Caps erzeugen — nicht erteilbare Autorität ist ein Fehler, keine Kürzung;
/// 6. laden (das Trust-Gate aus ADR 0014 gilt zusätzlich weiter: TrustedSAS braucht sein Zertifikat).
pub fn start_root_task() -> Result<(ThreadId, usize), RootTaskError> {
    let archive = read_archive().ok_or(RootTaskError::NoArchive)?;
    let man = read_manifest().ok_or(RootTaskError::NoManifest)?;
    let entry = man.root_task().ok_or(RootTaskError::NoRootEntry)?;
    let index = (0..archive.count())
        .find(|&i| archive.program(i).map(|p| p.program_id) == Some(entry.program_id))
        .ok_or(RootTaskError::NotInArchive)?;
    let prog = archive.program(index).ok_or(RootTaskError::NotInArchive)?;
    if sha256(prog.elf) != entry.sha256 {
        return Err(RootTaskError::HashMismatch);
    }
    if prog.domain != entry.domain {
        return Err(RootTaskError::DomainMismatch);
    }
    // **Die Startmenge ist das Manifest, nicht das Archiv** (D5).
    //
    // Vorher stand hier `archive.count()`. Das war nicht bloss ungenau, es war die falsche Quelle:
    // das Archiv ist ein Behaelter, das Manifest ist das Autoritaetsdokument. Auf x86 fiel das nie
    // auf, weil die Suite ihr Archiv genau aus den Manifest-Modulen baute -- zwei Zahlen, die
    // uebereinstimmten, weil dasselbe Skript beide erzeugte. Die aarch64-Suite hat zehn
    // Testdienste im Archiv, die nicht zur Startmenge gehoeren; dort haette der Root-Task
    // `count = 11` bekommen und Fremdmodule geladen (darunter `probe`, das gar kein ELF ist).
    //
    // Die Zusage, die dadurch noetig wird, steht jetzt als **Pruefung** da und nicht als
    // Gewohnheit: der Root-Task spricht die uebrige Startmenge ueber einen ARCHIVINDEX an
    // (`SYS_LOAD`), bekommt aber die Groesse der Startmenge aus dem Manifest. Beide Zahlen
    // bedeuten nur dann dasselbe, wenn die Manifest-Eintraege `0..count` auf den Archivpositionen
    // `0..count` liegen. Genau das wird hier geprueft -- fail-closed.
    let n = man.count();
    for i in 0..n {
        let e = man.entry(i).ok_or(RootTaskError::StartSetNotPrefix)?;
        let p = archive.program(i).ok_or(RootTaskError::StartSetNotPrefix)?;
        if p.program_id != e.program_id {
            return Err(RootTaskError::StartSetNotPrefix);
        }
    }
    // Strand 5 (DTB-or-ACPI to the IRT): stage the firmware description
    // BEFORE the load (it owns frames, needs no PD) and map it into the root
    // task after. Fail-open by design: a handover outage must never brick the
    // boot; the IRT reports absent and the kernel counts the failure.
    #[cfg(target_arch = "aarch64")]
    let ho_staged = crate::handover::stage_for_root();
    let caps = endow_from_manifest(&entry, None, 0, None)?; // Startmenge: Vorgabe-Pool
    let arg = boot_arg(index, n);
    // Die belegten Einträge zu einem dichten Slice verdichten. `CapPtr` hat bewusst keinen
    // öffentlichen Konstruktor (ein fabrizierbarer Cap-Handle wäre eine Einladung), also dient
    // der erste erzeugte Cap als Füllwert — ohne Cap gibt es nichts zu verdichten.
    let Some(first) = caps.iter().flatten().next().copied() else {
        // Cap-less root entry: same handover mapping, no endowment to compact.
        return match load_image(&prog, &[], arg) {
            Ok(r) => {
                #[cfg(target_arch = "aarch64")]
                if ho_staged {
                    crate::handover::hand_to_root(r.0);
                }
                Ok(r)
            }
            Err(e) => Err(RootTaskError::Rejected(e)),
        };
    };
    let mut endow = [first; ENDOW_SLOTS];
    let mut n = 0usize;
    for &c in caps.iter().flatten() {
        endow[n] = c;
        n += 1;
    }
    match load_image(&prog, &endow[..n], arg) {
        Ok(r) => {
            #[cfg(target_arch = "aarch64")]
            if ho_staged {
                crate::handover::hand_to_root(r.0);
            }
            Ok(r)
        }
        Err(e) => {
            // Die erzeugten Endowment-Caps sind noch nirgends installiert -> sonst lecken sie.
            for &(_, cap) in &endow[..n] {
                let _ = crate::system::cap_delete(cap);
            }
            Err(RootTaskError::Rejected(e))
        }
    }
}

/// Den Root-Task starten **und melden**. Gibt `true`, wenn er läuft.
pub fn start_root_task_reported() -> bool {
    match start_root_task() {
        Ok((tid, pd)) => {
            println!(
                "root    : ALL PASS (Startprogramm aus der Startmenge geladen: Thread {:?}, PD {pd}; Manifest-Hash geprueft, Wurzel-Caps endowt)",
                tid.to_raw()
            );
            // LXPD-Treiber-Boot (s. Abschnitt unten): nach dem Archiv-Root-Task die
            // Manifest-Treiberliste abfahren. Der Bring-up ruft nichts weiter — der Aufruf
            // steht hier, damit beide Hochlaufwege (x86, aarch64) ihn ohne Änderung fahren.
            let _ = boot_lxpd_treiber(Some(pd));
            true
        }
        Err(e) => {
            println!("root    : FAILURES ({e:?}) -- kein Root-Task, der Kernel hat nichts auszufuehren");
            // Auch ohne Root-Task wird geprüft und BENANNT abgewiesen (`WurzelFehlt`),
            // statt still zu überspringen.
            let _ = boot_lxpd_treiber(None);
            false
        }
    }
}

// ================================================================================================
// LXPD-Treiber-Boot: Treiber-PDs aus Bootloader-Modulen (Strang Boot)
// ================================================================================================
//
// Die Hüllenprüfung liegt in [`lxpd_boot`] (eigene Datei, s. dort); hier liegt der
// **privilegierte** Teil: Treiber-Einträge des angenommenen Manifests auflösen und je
// Eintrag eine Treiber-PD erzeugen — über die bestehenden Pfade, keine neuen Sonderrechte.
//
// Verdrahtung ohne `main.rs`: Diese Datei ist die einzige geänderte, `lxpd_boot.rs` die
// einzige neue — der Dateibesitz dieses Strangs lässt nichts anderes zu. Deshalb hängt
// das Modul hier per Pfad-Attribut statt per `mod`-Zeile in `main.rs`:
//
// ```ignore
// #[path = "lxpd_boot.rs"] mod lxpd_boot;
// ```
//
// ## Zuständigkeit — und warum Archiv-ELFs hier NICHT angefasst werden
//
// Ein Manifest-Eintrag der Domäne HardwareLand endet in genau einem von drei Zählern
// (`gestartet`, `abgewiesen`, `an-init-verwiesen`); kein Eintrag wird still übersprungen.
// Aber „abfahren" heisst nicht „alles selbst laden":
//
// * **Archiv-ELF → init-Pfad (`SYS_LOAD`).** Das heutige `virtio-blk`/`virtio-net` liegt als
//   ELF im Archiv und wird von `init` über seine Loader-Cap geladen (A-5.1, A-5.3, A-5.4,
//   `drv :`/`dmaiso :`). Würde der Boot dieselben Module zuerst in eigene Backend-PDs
//   laden, schnappte er `init` die Geräte weg — die Zuteilung ist fail-closed, also fiele
//   der echte Treiberpfad danach mit `NoDevice` aus. Deshalb: gezählt, als Zeile gemeldet,
//   nicht angefasst.
// * **LXPD-Hülle (beide Quellen) → prüfen + benennen.** Ein v1-Container ist Transportnachweis,
//   kein ladbares Image — er wird vollständig validiert und mit `TransportNur` abgewiesen
//   (die ladbare `bind_elf`-Form kommt über denselben Eintrag herein, sobald der
//   Format-Strang sie liefert). Eine verletzte Hülle heisst `ContainerFehler`.
// * **Bootloader-Spanne (Module 1..) + ELF → Boot-PD.** Diese Module sieht `init` nicht
//   (`load_by_index` liest nur das Archiv) — hier entsteht keine Doppelbelegung, und nur
//   hier erzeugt der Boot eine PD.
//
// ## Warum die Zeilen `ABGEWIESEN` sagen statt `FAILURES`
//
// Die Suiten führen einen Rotzeilen-Scanner: jede `... : ...FAILURES`-Zeile ausserhalb der
// bekannten Liste färbt den Lauf. Eine LXPD-Absage ist aber kein Befund am Kernel, sondern
// eine benannte Aufbau-/Formatlage (kein Modul, falscher Hash, Transport statt Image) —
// sie trüge einen fremden Befund in jede Suite, die (noch) keine LXPD-Module mitgibt.
// Deshalb das eigene Wort; die Zähler stehen in der Bilanz.

/// LXPD-Boot-Transport (Hüllenprüfung + Modul-Spannen). Pfad-Attribut statt `main.rs`-
/// Eintrag, s. Abschnitt oben.
#[path = "lxpd_boot.rs"]
mod lxpd_boot;

/// LXPD-Spawn-Entscheidung (nur `core`): Pfad-Attribut wie `lxpd_boot`, genutzt in [`boot_lxpd_treiber`].
#[path = "lxpd_glue.rs"]
mod lxpd_glue;

/// Gemeldete Spannen zählen (Bericht). Re-Export für die Bring-up-Verdrahtung
/// (Module 1.. aus `bringup.rs` melden) — die Statik lebt weiter in `lxpd_boot`.
pub use lxpd_boot::set_lxpd_module_span;

/// Warum ein Boot-Treiber nicht zustande kam. Bewusst **unterscheidbar** — dieselbe
/// Begründung wie bei [`RootTaskError`]: „lädt nicht" ist als Diagnose wertlos, und der
/// Unterschied zwischen „kein Modul" und „Hash passt nicht" ist der Unterschied zwischen
/// einem Aufbaufehler und einem Integritätsbefund.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LxpdAbsage {
    /// Der Eintrag ist unlesbar, obwohl der Zähler ihn verspricht (Manifest innerlich unstimmig).
    EintragUnlesbar,
    /// Als ROOT markiert, aber Domäne HardwareLand — ein Widerspruch im Manifest, keine PD.
    WurzelWiderspruch,
    /// Weder Archiv noch Bootloader-Modul tragen diesen Hash.
    NichtGefunden,
    /// Das Archivmodul weicht vom Manifest-Hash ab — der Fall, für den der Hash da ist.
    HashAbweichung,
    /// Weder LXPD- noch ELF-Magic — kein bekanntes Transportformat.
    KeinContainer,
    /// LXPD-Hülle verletzt — mit dem genauen Grund.
    ContainerFehler(lxpd_boot::LxpdBootError),
    /// Gültiger v1-Container, aber **kein ladbares Image**: Die Stubs sind der
    /// Transportnachweis der lxport-Pipeline, kein Code. Der Boot braucht die
    /// `bind_elf`-Form (reines ET_EXEC-ELF); sie kommt über denselben Eintrag herein,
    /// sobald der Format-Strang sie liefert.
    ///
    /// Das ist kein Platzhalter, sondern die dauerhaft korrekte Antwort fuer Container:
    /// v1 ist Kopf + n x 32-B-Stubs (`LXTR` + ID + 2 x FNV32 + NOP-Padding) + `LXEND`
    /// (Fahrt 4: 153 B, 4 Stubs) — kein ausfuehrbares Segment, kein Einsprungpunkt.
    /// Starten ist damit strukturell unmoeglich, nicht bloss unverifiziert; auch nach
    /// Verify-am-Boot endet ein Container hier, nur das ELF daneben startet.
    TransportNur,
    /// Der Root-Task läuft nicht — ohne Partner keine Backend-PD (die Hülle wurde trotzdem geprüft).
    WurzelFehlt,
    /// Keine Backend-PD erzeugbar (Kernel-Ressourcen erschöpft).
    KeinBackend,
    /// Das Manifest verlangt Geräte-Autorität, und es gibt keine (s. [`RootTaskError::NoDevice`]).
    KeineZuteilung,
    /// Das Manifest verlangt Autorität, die dieser Kernel nicht erteilen kann.
    PolitikAbgewiesen,
    /// Anfangs-Caps ließen sich nicht erzeugen (Kernel-Ressourcen erschöpft).
    KeineRessourcen,
    /// Der Loader hat das Image abgelehnt — mit dem genauen Grund.
    LoaderAbgewiesen(LoaderError),
    /// Kein angenommenes Boot-Manifest — nur der Laufzeit-Pfad (`SYS_LOAD_IMAGE`): der
    /// Boot-Hook meldet das vor der Schleife, hier ist es eine Absage je Auftrag.
    KeinManifest,
    /// Kein Manifest-Eintrag mit dieser `pid` — der Join-Schlüssel des Laufzeit-Pfads
    /// verfehlt (Aufbaufehler, kein Integritätsbefund: geprüft wurde nichts, weil es nichts
    /// zu prüfen gab).
    PidUnbekannt,
    /// Der Eintrag ist nicht HardwareLand — nur Treiber laden über diesen Pfad (der
    /// Root-Task kommt aus dem Boot, Dienste/Clients über `SYS_LOAD`).
    FalscheDomaene,
    /// Derselbe Slot aus Manifest UND Aufrufer-Angebot — wie `collision` in `load_by_index`:
    /// welches von beiden das Programm dann meinte, könnte niemand mehr sagen.
    EndowKollision,
    /// Mehr Caps als [`ENDOW_SLOTS`] fassen — fail-closed statt Überlauf in den festen
    /// Puffer (die alte Inline-Fassung schrieb blind und war nur per Konstruktion sicher,
    /// weil sie eine einzige Quelle hatte).
    EndowZuViel,
}

/// Der Hook ist idempotent (der Bring-up darf ihn später direkt rufen, ohne eine zweite
/// Fassung zu erzeugen): `1`, sobald er einmal fuhr.
static LXPD_BOOT_GELAUFEN: AtomicU64 = AtomicU64::new(0);
/// Gestartete Boot-Treiber-PDs (Bilanz, kein Ruhm — die Zahl gehört zur Zeile).
static LXPD_GESTARTET: AtomicU64 = AtomicU64::new(0);
/// Benannt abgewiesene Einträge.
static LXPD_ABGEWIESEN: AtomicU64 = AtomicU64::new(0);
/// An den init-Pfad verwiesene Archiv-ELFs (Zuständigkeit, keine Absage).
static LXPD_AN_INIT: AtomicU64 = AtomicU64::new(0);

/// Eine Absage melden + zählen — eine Stelle, damit kein Pfad das Zählen vergisst.
fn lxpd_nein(pid: u32, name: &str, quelle: &str, grund: LxpdAbsage) {
    LXPD_ABGEWIESEN.fetch_add(1, Ordering::Relaxed);
    println!("lxpddrv : [{pid}]{name} ABGEWIESEN ({grund:?}, quelle={quelle}) -- keine Treiber-PD");
}

/// Einen Start melden + zählen.
fn lxpd_ja(pid: u32, name: &str, quelle: &str, pd: usize, tid: ThreadId) {
    LXPD_GESTARTET.fetch_add(1, Ordering::Relaxed);
    println!(
        "lxpddrv : [{pid}]{name} gestartet (quelle={quelle} PD={pd} Thread={:?}; Hash gebunden, Grants endowt)",
        tid.to_raw()
    );
}

/// Die Bilanz ziehen — **immer**, auch bei leerer Treiberliste: Die Zeile ist der Beleg,
/// dass der Hook fuhr. Enthält bewusst kein `FAILURES` (s. Abschnitt oben).
fn lxpd_bilanz() -> (usize, usize) {
    let g = LXPD_GESTARTET.load(Ordering::Relaxed) as usize;
    let a = LXPD_ABGEWIESEN.load(Ordering::Relaxed) as usize;
    let v = LXPD_AN_INIT.load(Ordering::Relaxed) as usize;
    let s = lxpd_boot::lxpd_module_gemeldet();
    println!(
        "lxpddrv : gestartet={g} abgewiesen={a} an-init-verwiesen={v} Spannen={s} (Treiber-PDs aus Bootloader-/Archiv-Modulen; Manifest-Signatur gegen MANIFEST_KEYS, Modul-Hash gegen Eintrag)"
    );
    (g, a)
}

/// **Einen hash-gebundenen LXPD-Treiber in eine Backend-PD laden** — die SHARED Fassung des
/// Per-Treiber-Bodys („Bootloader-Spanne mit ELF-Form" in [`boot_lxpd_treiber`]).
///
/// EIN Body, zwei Aufrufer: der Boot-Hook (Partner = Root-Task-PD, Vorgabe-Pool, ohne
/// Aufrufer-Angebot, Heimatkern = dieser Kern, Startmengen-Argument) und der Laufzeit-Pfad
/// ([`load_verified_image`]: Partner = Aufrufer-PD, Budgets + Angebot des Aufrufers,
/// Heimatkern des Aufrufers, `boot_arg(pid, 0)`). Was hier steht, gilt für beide — zwei
/// Stellen, die dasselbe täten, altern auseinander (s. `sammle_endow` im Dispatch).
///
/// Voraussetzungen (prüft der AUFRUFER, nicht diese Funktion): das Bild ist ELF
/// (`lxpd_boot::is_elf`) und `sha256(bild) == e.sha256` — Vertrauen aus dem BOOT-Manifest.
/// Gibt `(Backend-PD, Endpoint, Notification, Thread)` zurück; die Dienst-Registrierung
/// (`set_driver_service`, mit aufruferspezifischem `index`) bleibt beim Aufrufer, weil nur
/// er weiss, woher das Bild kam (Archiv-Index vs. `u32::MAX` = kein Archivmodul).
///
/// `caller_endow` sind die vom Dispatch aus dem Aufrufer-Cspace delegierten Caps (L2:
/// höchstens ein Cap, in Slot 0) — sie werden mit den Manifest-Caps KOMBINIERT, nicht daneben
/// gelegt: Schranke [`ENDOW_SLOTS`], derselbe Slot zweimal ist fail-closed
/// ([`LxpdAbsage::EndowKollision`]/[`LxpdAbsage::EndowZuViel`]). Die alte Inline-Fassung schrieb
/// blind in `[first; ENDOW_SLOTS]` — mit nur einer Quelle per Konstruktion ungefährlich, mit
/// zwei ein Panic-Pfad; die Schranke hier schliesst ihn.
///
/// `angebot` (die Bitmaske der Aufrufer-Slots für die Ladepolitik) folgt derselben Quelle:
/// leerer Aufruf heisst `0`, wie `load_program_into_pd` ihn immer trug — der Boot-Fall ist
/// damit bitgleich zur alten Fassung (`_auf(..., eigener_kern(), 0, 0)` IST
/// `load_program_into_pd`, s. dort).
#[allow(clippy::too_many_arguments)]
fn lxpddrv_laden(
    e: &ManifestEntry,
    bild: &[u8],
    partner: usize,
    caller_endow: &[(usize, CapPtr)],
    heimatkern: usize,
    cap_budget: u16,
    dma_pages: u32,
    arg: usize,
) -> Result<(usize, usize, usize, ThreadId), LxpdAbsage> {
    let Some((hpd, ep, ntfn)) =
        crate::system::create_hardware_backend(partner, (e.program_id & 0xffff) as u16)
    else {
        return Err(LxpdAbsage::KeinBackend);
    };
    // Vorgabe-Pool am Boot (`dma_pages = 0`), wie der Root-Task — die Grössenpolitik zur
    // Ladezeit (C2) gehört dem Laufzeit-Pfad, nicht dem Boot.
    let caps = match endow_from_manifest(e, Some(Kanal::Geraet(ep, ntfn)), dma_pages, Some(hpd)) {
        Ok(c) => c,
        Err(RootTaskError::NoDevice) => return Err(LxpdAbsage::KeineZuteilung),
        Err(RootTaskError::UnsupportedAuthority) => return Err(LxpdAbsage::PolitikAbgewiesen),
        Err(_) => return Err(LxpdAbsage::KeineRessourcen),
    };
    // Das synthetische `Program`: `Program::new` ist ausdrücklich quellen-agnostisch
    // („Archiv, Dateisystem, …"). Version 1 ist ungebunden, aber harmlos —
    // HardwareLand braucht kein Zertifikat (`verify_image` ignoriert alles ausser der
    // Domäne), und die Schnittstellenversion trägt das Manifest (`iface_gate`).
    let synth = Program::new(
        e.program_id,
        e.name().as_bytes(),
        1,
        DOMAIN_HARDWARE,
        e.sha256,
        bild,
        &[],
        &[],
    );
    // Kombinieren: erst Manifest, dann Angebot — mit Schranke UND Kollisionsprüfung (s. Doku).
    // Eine Absage hier braucht Aufräumen: die Manifest-Caps sind frisch geprägt und nirgends
    // installiert — ohne `cap_delete` blieben sie als verwaiste CDT-Kinder liegen (derselbe
    // Fehler, den der Abweispfad im Dispatch schon einmal bezahlt hat, s. `sammle_endow`).
    let mut alle: [Option<(usize, CapPtr)>; ENDOW_SLOTS] = [None; ENDOW_SLOTS];
    let mut n = 0usize;
    for &c in caps.iter().flatten().chain(caller_endow.iter()) {
        if alle[..n].iter().flatten().any(|&(s, _)| s == c.0) {
            for &(_, cap) in alle[..n].iter().flatten() {
                let _ = crate::system::cap_delete(cap);
            }
            return Err(LxpdAbsage::EndowKollision);
        }
        if n == alle.len() {
            for &(_, cap) in alle[..n].iter().flatten() {
                let _ = crate::system::cap_delete(cap);
            }
            return Err(LxpdAbsage::EndowZuViel);
        }
        alle[n] = Some(c);
        n += 1;
    }
    let angebot = caller_endow.iter().fold(0u16, |m, &(slot, _)| {
        m | if slot < 16 { 1u16 << slot } else { 0 }
    });
    let geladen = match alle[..n].iter().flatten().next().copied() {
        Some(first) => {
            let mut endow = [first; ENDOW_SLOTS];
            let mut k = 0usize;
            for &c in alle[..n].iter().flatten() {
                endow[k] = c;
                k += 1;
            }
            // Heimatkern + Budgets nennt der Aufrufer: am Boot ist das dieser Kern mit
            // Vorgaben (bitgleich zu `load_program_into_pd`), zur Laufzeit der Aufrufer-Kern
            // mit seinen Wünschen.
            load_program_into_pd_auf(
                &synth,
                hpd,
                &endow[..k],
                arg,
                heimatkern,
                angebot,
                cap_budget,
            )
        }
        None => load_program_into_pd_auf(&synth, hpd, &[], arg, heimatkern, angebot, cap_budget),
    };
    match geladen {
        Ok(tid) => Ok((hpd, ep, ntfn, tid)),
        Err(e) => {
            // Die endowten Caps sind noch nirgends installiert (wie beim Root-Task);
            // Backend-PD + Kanal bleiben bestehen wie in `load_by_index` — wiederverwendet,
            // nicht neu erfunden.
            for &(_, cap) in alle[..n].iter().flatten() {
                let _ = crate::system::cap_delete(cap);
            }
            // **Der Grund wird genannt** (wie in `do_load`): ein `NoResources` ohne
            // Ressource ist die Pruefer-Krankheit. `lade_mangel()` steht hier noch auf
            // dem Fehlschlag (nichts dazwischen setzt zurueck).
            if e == LoaderError::NoResources {
                let (code, bytes, frei) = crate::system::lade_mangel();
                println!(
                    "lxpddrv :   fehlende Ressource: {} (Code {code}); angefordert {bytes} Byte, \
                     freier Rest {frei} Byte",
                    crate::system::mangel_name(code)
                );
            }
            Err(LxpdAbsage::LoaderAbgewiesen(e))
        }
    }
}

/// **LXPD-Container durch das Glue-Gate** (`lxpd_glue::spawn_entscheidung`): Verifikation
/// (Manifest angenommen, Hash gebunden, Hülle geparst) → Facts → Entscheidung → Mapping.
///
/// Die JSON-Seite fehlt am Boot (Stubnamen/Grants/Coverage kommen separat, s. `lxpd_boot`):
/// ohne `verify_manifest`-Urteil kein `verified` (sonst belöge man den Gate, s. `lxpd_glue`) —
/// der Gate endet heute in `Unverified` → `TransportNur`. Alle Varianten sind benannt auf
/// bestehende Absagen abgebildet (kein Panic-Pfad); mit der JSON-Seite sprechen die
/// hinteren Gate ohne Umbau des Mappings.
fn lxpd_container_gate(pid: u32, tramp_count: usize) -> Result<(), LxpdAbsage> {
    use lxpd_glue::{spawn_entscheidung, ExecAntrag, LxpdFacts, LxpdPolicy, LxpdSpawnError};
    let facts = LxpdFacts {
        verified: false, // kein verify_manifest-Urteil am Boot (s. oben)
        signature_present: false, // keine LXPD-JSON-Signatur geprüft
        schema_version: lxpd_glue::LXPD_SCHEMA_VERSION, // Hülle ist v1 per Konstruktion
        trampoline_names: 0, // JSON-Namen unbekannt — Zähler allein bindet nichts
        tramp_count,     // verifiziert: aus der geparsten Hülle
        coverage_pct: 0.0, // JSON-Deckung unbekannt
        bar_base: 0, bar_size: 0, dma_base: 0, dma_size: 0, // keine Grant-Daten am Boot
        irq: 0, heap_pages: 0, // dito (Manifest nennt keinen Heap, s. lxpd_glue)
    };
    let policy = LxpdPolicy {
        max_bar_bytes: u64::MAX,
        max_dma_bytes: u64::MAX,
        max_heap_pages: u64::MAX,
        exec: ExecAntrag {
            program_id: pid,
            epoche: 0,
            token: lxpd_glue::teardown_token_fuer(pid, 0),
            eintrag: 1,
            segmente: 1,
        },
    };
    match spawn_entscheidung(&facts, &policy) {
        Ok(_) => Ok(()),
        Err(LxpdSpawnError::Unverified) => Err(LxpdAbsage::TransportNur),
        Err(LxpdSpawnError::BadImage) => Err(LxpdAbsage::EintragUnlesbar),
        Err(LxpdSpawnError::BadGrants) => Err(LxpdAbsage::KeineZuteilung),
        Err(LxpdSpawnError::Ueberlappung) => Err(LxpdAbsage::EndowKollision),
        Err(LxpdSpawnError::BudgetErschoepft) => Err(LxpdAbsage::PolitikAbgewiesen),
        Err(LxpdSpawnError::ExecAbgelehnt) => Err(LxpdAbsage::TransportNur),
    }
}

/// **LXPD-Treiber-Boot**: nach dem Archiv-Root-Task die Manifest-Treiberliste abfahren.
///
/// Auflösung je Eintrag (Domäne HardwareLand, nicht ROOT):
/// 1. **Welches Modul?** Archivmodul mit gleicher `program_id` (wie der Root-Task) oder —
///    ohne `program_id`-Feld im Container — Bootloader-Spanne mit gleichem Hash.
///    Weicht die Archivkopie vom Manifest-Hash ab, wird ersatzweise die Spanne mit
///    gleichem Hash genommen (HINWEIS-Zeile); traegt keine Spanne die geforderten
///    Bytes, bleibt es `HashAbweichung`.
/// 2. **Hash womit?** `sha256(Modulbytes) == Eintrag.sha256`, vor jeder weiteren Prüfung.
/// 3. **Signatur gegen welche Keys?** Es gibt nur angenommene Manifeste (`read_manifest`,
///    gegen [`crate::manifest_keys::MANIFEST_KEYS`]) — ungeprüfte Einträge existieren hier nicht.
///
/// `partner` ist die Root-Task-PD (Partner-Bindung der Backend-PD, wie in `load_by_index`);
/// `None` heisst „der Root-Task läuft nicht" — geprüft wird trotzdem, geladen nicht
/// (`WurzelFehlt`). Gibt `(gestartet, abgewiesen)` zurück. Idempotent: der zweite Aufruf
/// meldet den alten Stand und erzeugt keine zweite Fassung.
pub fn boot_lxpd_treiber(partner: Option<usize>) -> (usize, usize) {
    if LXPD_BOOT_GELAUFEN.swap(1, Ordering::Relaxed) != 0 {
        let g = LXPD_GESTARTET.load(Ordering::Relaxed) as usize;
        let a = LXPD_ABGEWIESEN.load(Ordering::Relaxed) as usize;
        println!("lxpddrv : bereits gefahren (idempotent -- keine zweite Fassung; gestartet={g} abgewiesen={a})");
        return (g, a);
    }
    let Some(man) = read_manifest() else {
        println!("lxpddrv : ABGEWIESEN (KeinManifest -- ohne angenommenes Manifest gibt es keine Treiberliste; s. manifest_audit)");
        return lxpd_bilanz();
    };
    let archiv = read_archive();
    if archiv.is_none() {
        // Benannt, einmal: Die Archivquelle entfällt — die Spannensuche läuft trotzdem.
        println!("lxpddrv : kein Boot-Archiv (KeinArchiv) -- Archivquelle entfaellt, Bootloader-Spannen bleiben suchbar");
    }
    let n = man.count();
    let mut treiber = 0usize;
    for mi in 0..n {
        let Some(e) = man.entry(mi) else {
            lxpd_nein(0, "?", "unbekannt", LxpdAbsage::EintragUnlesbar);
            continue;
        };
        if e.domain != DOMAIN_HARDWARE {
            continue;
        }
        treiber += 1;
        let pid = e.program_id;
        let name = e.name();
        if e.is_root_task() {
            lxpd_nein(pid, name, "Manifest", LxpdAbsage::WurzelWiderspruch);
            continue;
        }
        // --- Quelle suchen: Archiv zuerst (Join-Key `program_id`), dann Spannen (Join-Key Hash).
        //
        // Weicht die Archivkopie vom Manifest-Hash ab, faellt die Absage NICHT sofort:
        // erst werden die Spannen gefragt. Der Manifest-Hash ist die einzige Autoritaet
        // ueber das WAS — traegt eine Spanne genau die geforderten Bytes, erfuellt sie
        // dieselbe Bindung, die die Archivkopie verfehlt. Belegt: Fahrt 2 fand die Spanne
        // per Hash (`quelle=Bootloader-Modul`), Fahrt 4 nur das Archiv; ohne Rueckfall
        // waere der Spannen-ELF-Pfad bei gueltiger Wurzel (D5 verlangt die ID im Archiv,
        // das Archiv gewinnt immer) strukturell unerreichbar — gemessen nie gruen.
        // Die Abweichung bleibt sichtbar (HINWEIS bei Spannenfund, `HashAbweichung` ohne).
        let mut bild: Option<&[u8]> = None;
        let mut prog_aus_archiv: Option<Program<'_>> = None;
        let mut archiv_index: Option<u32> = None;
        let mut quelle = "unbekannt";
        let mut archiv_abweichung = false;
        if let Some(a) = archiv.as_ref() {
            if let Some(ai) =
                (0..a.count()).find(|&i| a.program(i).map(|p| p.program_id) == Some(pid))
            {
                match a.program(ai) {
                    Some(prog) if sha256(prog.elf) == e.sha256 => {
                        bild = Some(prog.elf);
                        prog_aus_archiv = Some(prog);
                        archiv_index = Some(ai as u32);
                        quelle = "Archiv";
                    }
                    _ => {
                        archiv_abweichung = true;
                    }
                }
            }
        }
        if bild.is_none() {
            // Der Container trägt kein `program_id`-Feld — der Hash allein bindet ihn.
            for si in 0..lxpd_boot::LXPD_MAX_MODULES {
                let Some(bytes) = lxpd_boot::lxpd_module_bytes(si) else { continue };
                if sha256(bytes) == e.sha256 {
                    bild = Some(bytes);
                    quelle = "Bootloader-Modul";
                    if archiv_abweichung {
                        println!(
                            "lxpddrv : [{pid}]{name} HINWEIS Archivkopie weicht vom Manifest-Hash ab -- Spanne {si} traegt die geforderten Bytes und wird geladen (Bindung: sha256 == Eintrag)"
                        );
                    }
                    break;
                }
            }
        }
        let Some(bild) = bild else {
            lxpd_nein(
                pid,
                name,
                if archiv_abweichung { "Archiv" } else { quelle },
                if archiv_abweichung {
                    LxpdAbsage::HashAbweichung
                } else {
                    LxpdAbsage::NichtGefunden
                },
            );
            continue;
        };
        // --- Form unterscheiden: ELF lädt (Zuständigkeit beachten), LXPD wird geprüft.
        if lxpd_boot::is_elf(bild) {
            if prog_aus_archiv.is_some() {
                // Zuständigkeit (s. Abschnitt oben): Archiv-ELFs lädt `init` über SYS_LOAD.
                LXPD_AN_INIT.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Bootloader-Spanne mit ELF-Form: `init` sieht sie nicht — hier entsteht die PD.
            // Läuft über die SHARED Fassung [`lxpddrv_laden`] (Vorgabe-Pool, ohne Angebot,
            // Heimatkern = dieser Kern): bitgleich zur alten Inline-Fassung, ein Body für Boot
            // UND Laufzeit statt zweier, die auseinanderaltern.
            let Some(proot) = partner else {
                lxpd_nein(pid, name, quelle, LxpdAbsage::WurzelFehlt);
                continue;
            };
            // Die Startmenge ist das Manifest (D5): Index = Manifestposition, nicht Behälterlage.
            let arg = boot_arg(mi, n);
            match lxpddrv_laden(
                &e,
                bild,
                proot,
                &[],
                crate::system::eigener_kern(),
                0,
                0,
                arg,
            ) {
                Ok((hpd, ep, ntfn, tid)) => {
                    // `u32::MAX` = kein Archivmodul: `reload_driver` findet darunter kein
                    // Archivprogramm und meldet benannt `NotLoaded`, statt ein FALSCHES
                    // Modul (fremder Spannenindex als Archivindex gelesen) zu laden.
                    //
                    // VOLLZAEHLIGKEIT: kein zweiter Eintrag noetig — `lxpddrv_laden`
                    // laeuft durch `load_program_into_pd_auf`, und dort traegt
                    // `record_program_thread` (s. dort) den Thread unter `program_id`
                    // ein, bevor dieser Zweig je erreicht wird. Ohne ihn luege
                    // `vollzahl` dauerhaft (Fahrt 4: `6/7 FEHLEND pid 7`). Kein Doppel
                    // mit dem init-Pfad: Spannen sieht `load_by_index` nicht (es liest
                    // nur das Archiv), und Archiv-ELFs fasst dieser Hook nicht an
                    // (`an-init-verwiesen`-Zweig oben) — jede PD wird genau einmal
                    // unter ihrer ID eingetragen.
                    let index = archiv_index.unwrap_or(u32::MAX);
                    set_driver_service(DriverService {
                        ep,
                        ntfn,
                        pd: hpd,
                        tid,
                        index,
                        program_id: pid,
                    });
                    lxpd_ja(pid, name, quelle, hpd, tid);
                }
                Err(g) => lxpd_nein(pid, name, quelle, g),
            }
        } else if lxpd_boot::is_lxpd(bild) {
            // Glue-Gate STATT reiner Hüllenprüfung: Hülle → Facts → Entscheidung →
            // Mapping (s. `lxpd_container_gate`); Ok endet wie bisher in `TransportNur`.
            match lxpd_boot::LxpdBild::parse(bild) {
                Ok(img) => match lxpd_container_gate(pid, img.tramp_count()) {
                    Ok(()) => {
                        println!(
                            "lxpddrv : [{pid}]{name} Transportnachweis gueltig (quelle={quelle}: {} Stubs, {} umverdrahtet) -- aber kein ladbares Image",
                            img.tramp_count(),
                            img.rewired_count()
                        );
                        lxpd_nein(pid, name, quelle, LxpdAbsage::TransportNur);
                    }
                    Err(g) => lxpd_nein(pid, name, quelle, g),
                },
                Err(fe) => {
                    lxpd_nein(pid, name, quelle, LxpdAbsage::ContainerFehler(fe));
                }
            }
        } else {
            lxpd_nein(pid, name, quelle, LxpdAbsage::KeinContainer);
        }
    }
    if treiber == 0 {
        println!("lxpddrv : keine Treiber-Eintraege im Manifest (Domaene HardwareLand) -- 0 gestartet, 0 abgewiesen");
    } else {
        let v = LXPD_AN_INIT.load(Ordering::Relaxed);
        if v > 0 {
            println!("lxpddrv : {v} Archiv-ELF(s) gehoeren dem init-Pfad (SYS_LOAD) -- hier nicht angefasst (Zustaendigkeit, keine Absage)");
        }
    }
    lxpd_bilanz()
}

// ================================================================================================
// Laufzeit-Treiber (`SYS_LOAD_IMAGE = 36`): Bild aus Aufrufer-RAM statt Boot-Archiv
// ================================================================================================

/// **Einen Laufzeit-Treiber aus Aufrufer-RAM laden** (`SYS_LOAD_IMAGE = 36`).
///
/// Der Dispatch hat Geometrie (plain-RAM, READ, Länge) und Delegation bereits geprüft; der
/// Verifizierer hat das Bild EINMAL nach Staging kopiert (`verifizierer::laden_bild`) und
/// reicht hier NUR die Kopie herein. Was ankommt, ist damit fixiert — was der Aufrufer
/// seither in sein RAM schreibt, lädt nicht mehr mit (TOCTOU, s. dort).
///
/// Reihenfolge, und sie ist die des Boot-Hooks ([`boot_lxpd_treiber`]):
/// 1. **Autorität aus dem BOOT-Manifest** — `read_manifest()` (Signatur, Kernel-Bindung,
///    Anti-Downgrade), Join per `pid`. Kein Manifest, keine `pid`, falsche Domäne,
///    Root-Task-Eintrag: benannte Absage, keine PD.
/// 2. **Form: NUR ELF.** Ein LXPD-Container ist Transport, kein Code
///    ([`LxpdAbsage::TransportNur`], wie am Boot) — wer ihn schickt, bekommt den Namen des
///    Falls, nicht ein wortloses `ERR_BADCAP`.
/// 3. **Bindung: `sha256(staged) == e.sha256`.** Das Vertrauen kommt aus dem BOOT-Manifest,
///    nicht aus mitgereichten Bytes — das Staging trägt keinen Absender, nur Inhalt.
/// 4. **Laden über die SHARED Fassung** ([`lxpddrv_laden`]): Backend mit `partner = caller_pd`
///    (wer lädt, ist der Partner — wie `load_by_index`), Manifest-Caps + Aufrufer-Angebot
///    kombiniert, synthetisches `Program`, Heimatkern/Budgets des Aufrufers.
///    Dienst-Registrierung mit `index = u32::MAX` (kein Archivmodul — `reload_driver` meldet
///    darunter benannt `NotLoaded`, statt ein falsches Modul zu laden).
///    `arg = boot_arg(pid, 0)`: **keine Manifestposition** — der Laufzeit-Treiber gehört nicht
///    zur Boot-Startmenge (D5); das Feld trägt die `pid`, die Zählung `0` markiert „keine
///    Startmenge".
///
/// Gibt die Backend-PD. `None` heisst immer „benannt abgewiesen (Zeile oben), nichts angelegt":
/// die delegierten Aufrufer-Caps werden zurückgenommen (wie `load_by_index`), Backend-PD +
/// Kanal bleiben bestehen (wiederverwendet, nicht neu erfunden — wie dort).
///
/// Die Zeilen sagen `ABGEWIESEN`, nie `FAILURES` (s. Abschnitt oben): Eine Absage ist kein
/// Befund am Kernel, und der Rotzeilen-Scanner der Suiten färbt jede `FAILURES`-Zeile.
pub fn load_verified_image(
    bild: &[u8],
    pid: u32,
    caller_pd: usize,
    caller_endow: &[(usize, CapPtr)],
    heimatkern: usize,
    cap_budget: u16,
    dma_pages: u32,
) -> Option<usize> {
    let pd = load_verified_image_inner(
        bild,
        pid,
        caller_pd,
        caller_endow,
        heimatkern,
        cap_budget,
        dma_pages,
    );
    if pd.is_none() {
        // Laden fehlgeschlagen → die delegierten Aufrufer-Cap-KOPIEN wurden NICHT installiert
        // (installiert wird erst nach vollem Erfolg). Frische CDT-Blätter, das Original im
        // Aufrufer-Cspace lebt → nur Refcount senken. Ohne dieses Cleanup lecken sie als
        // verwaiste CDT-Kinder und blockieren sogar `delete` des Eltern-Caps (HasChildren) —
        // derselbe Weg wie in `load_by_index`.
        for &(_, cap) in caller_endow {
            let _ = crate::system::cap_delete(cap);
        }
    }
    pd
}

fn load_verified_image_inner(
    bild: &[u8],
    pid: u32,
    caller_pd: usize,
    caller_endow: &[(usize, CapPtr)],
    heimatkern: usize,
    cap_budget: u16,
    dma_pages: u32,
) -> Option<usize> {
    let Some(man) = read_manifest() else {
        let _grund = LxpdAbsage::KeinManifest;
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]? ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- ohne angenommenes Boot-Manifest gibt es keinen Join-Schluessel"
        );
        return None;
    };
    let Some(e) = (0..man.count())
        .filter_map(|i| man.entry(i))
        .find(|e| e.program_id == pid)
    else {
        let _grund = LxpdAbsage::PidUnbekannt;
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]? ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- kein Manifest-Eintrag mit dieser pid"
        );
        return None;
    };
    let _name = e.name();
    if e.domain != DOMAIN_HARDWARE {
        let _grund = LxpdAbsage::FalscheDomaene;
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]{_name} ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- nur Treiber laden ueber diesen Pfad"
        );
        return None;
    }
    if e.is_root_task() {
        let _grund = LxpdAbsage::WurzelWiderspruch;
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]{_name} ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- der Root-Task kommt aus dem Boot, nicht aus Aufrufer-RAM"
        );
        return None;
    }
    if !lxpd_boot::is_elf(bild) {
        let _grund = if lxpd_boot::is_lxpd(bild) {
            match lxpd_boot::LxpdBild::parse(bild) {
                Ok(_img) => {
                    #[cfg(feature = "debug-log")]
                    println!(
                        "lxpdimg : [{pid}]{_name} Transportnachweis gueltig (quelle=Laufzeit: {} Stubs, {} umverdrahtet) -- aber kein ladbares Image",
                        _img.tramp_count(),
                        _img.rewired_count()
                    );
                    LxpdAbsage::TransportNur
                }
                Err(fe) => LxpdAbsage::ContainerFehler(fe),
            }
        } else {
            LxpdAbsage::KeinContainer
        };
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]{_name} ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- keine Treiber-PD"
        );
        return None;
    }
    // Bindung: Vertrauen aus dem BOOT-Manifest, nicht aus mitgereichten Bytes.
    if sha256(bild) != e.sha256 {
        let _grund = LxpdAbsage::HashAbweichung;
        #[cfg(feature = "debug-log")]
        println!(
            "lxpdimg : [{pid}]{_name} ABGEWIESEN ({_grund:?}, quelle=Laufzeit) -- Staging weicht vom Manifest-Hash ab"
        );
        return None;
    }
    let arg = boot_arg(pid as usize, 0);
    match lxpddrv_laden(
        &e,
        bild,
        caller_pd,
        caller_endow,
        heimatkern,
        cap_budget,
        dma_pages,
        arg,
    ) {
        Ok((hpd, ep, ntfn, tid)) => {
            set_driver_service(DriverService {
                ep,
                ntfn,
                pd: hpd,
                tid,
                index: u32::MAX,
                program_id: pid,
            });
            #[cfg(feature = "debug-log")]
            println!(
                "lxpdimg : [{pid}]{_name} gestartet (quelle=Laufzeit PD={hpd} Thread={:?}; Hash gebunden, Grants endowt)",
                tid.to_raw()
            );
            Some(hpd)
        }
        Err(_g) => {
            #[cfg(feature = "debug-log")]
            println!(
                "lxpdimg : [{pid}]{_name} ABGEWIESEN ({_g:?}, quelle=Laufzeit) -- keine Treiber-PD"
            );
            None
        }
    }
}

/// **L0-Selbsttest/Telemetrie:** das Archiv lesen + die gefundenen Module melden. Greift NICHT in
/// den Boot-Ablauf ein (reines Lesen); das eigentliche Laden folgt ab L1.
pub fn probe() {
    match read_archive() {
        Some(a) => {
            print!("archive : {} Modul(e):", a.count());
            for p in a.iter() {
                // Stabile program_id (Verfeinerung 4) + Name + Version melden.
                print!(" [{}]{}@v{}", p.program_id, p.name(), p.version);
            }
            println!(" -> ALL PASS");
        }
        None => println!("archive : kein gueltiges Boot-Archiv (0 Module, FAILURES)"),
    }
}

/// **Die öffentliche Loader-API** (ADR 0011, Verfeinerung 1): ein quellen-agnostisches `Program`
/// laden + starten. Parst das ELF (Safe Rust, [`ElfImage::parse`]), bildet die Domäne ab und
/// delegiert den privilegierten Teil (Segmente kopieren/mappen, VSpace/PD, Spawn) an
/// [`crate::system::load_elf`]. `endow` = initiale Caps für die neue PD (`(Slot, Cap)`), in L2 aus
/// dem Manifest abgeleitet. Gibt `(ThreadId, pd)` der neuen, laufbereiten PD.
///
/// `boot_arg` landet im ersten Argument von `_start` — die **einzige** Information, die ein
/// geladenes Programm ohne Cap-Aufruf bekommt. Sie muss deshalb klein und selbsterklärend sein
/// (heute: Lage des Programms in der Startmenge, s. [`start_root_task`]); alles Weitere gehört
/// hinter eine Capability.
///
/// L1: nur **isolierte EL0-Domänen** (UserLand/HardwareLand). TrustedSAS (EL1) ist signatur-gegatet
/// (L3) und wird hier abgelehnt (`UnsupportedDomain`).

// --- A-4.4: Schnittstellenversion beim Austausch pruefen, nicht hoffen -----------------------
//
// Hot-Reload ersetzt eine Server-PD ueber DIESELBE Endpoint-Cap. Die Clients merken davon nichts
// -- das ist der Zweck. Genau deshalb ist eine geaenderte Schnittstellenversion hier gefaehrlich:
// niemand wird benachrichtigt, und der Fehler zeigt sich erst beim ersten missverstandenen `CALL`,
// weit weg von seiner Ursache. Ein Austausch, der `iface_version` aendert, wird deshalb ABGEWIESEN.
//
// Wo die Grenze liegt: geprueft wird gegen die Version, mit der diese `program_id` **zuerst**
// geladen wurde -- nicht gegen die vorige. Sonst liesse sich die Schnittstelle in kleinen Schritten
// beliebig weit wegdriften (1 -> 2 -> 3), obwohl kein einzelner Schritt „erlaubt" war.
//
// Die Version kommt aus dem **signierten** Manifest, nicht aus dem Image: sie ist eine Aussage
// ueber die Zuteilung, und Aussagen ueber Zuteilung gehoeren in das Dokument, das signiert ist.

/// Wie viele verschiedene `program_id` die Versionsbuchhaltung fassen kann.
///
/// Laeuft sie voll, wird **abgewiesen** ([`LoaderError::IfaceTableFull`]) statt still nicht mehr
/// zu pruefen. Eine Pruefung, die unbemerkt aussetzt, sieht von aussen aus wie eine bestandene --
/// dieselbe Fehlerform, die dieses Projekt schon an der leeren SMMU-Event-Queue bezahlt hat.
const MAX_IFACE_TRACKED: usize = 64;

static IFACE_SEEN: SpinLock<([(u32, u32); MAX_IFACE_TRACKED], usize)> =
    SpinLock::new(([(0, 0); MAX_IFACE_TRACKED], 0));

/// Die im Manifest ausgewiesene Schnittstellenversion dieser `program_id`.
///
/// `None`, wenn es kein angenommenes Manifest gibt oder die ID darin nicht vorkommt. Dann gibt es
/// nichts zu vergleichen -- und **nichts zu behaupten**: der Gate laesst durch, statt eine Zusage
/// zu erfinden, die er nicht pruefen kann.
fn manifest_iface_version(program_id: u32) -> Option<u32> {
    manifest_entry_of(program_id).map(|(iface, _)| iface)
}

/// `(iface_version, policy_flags)` dieser `program_id` aus dem **angenommenen** Manifest.
fn manifest_entry_of(program_id: u32) -> Option<(u32, u32)> {
    let m = read_manifest()?;
    (0..m.count())
        .filter_map(|i| m.entry(i))
        .find(|e| e.program_id == program_id)
        .map(|e| (e.iface_version, e.policy_flags))
}

// --- A-1.4: Politikfelder, die der Kernel nicht einhalten kann, werden ABGEWIESEN -------------
//
// A-1.4 sagt: das Format steht, angewandt werden die Felder noch nicht. "Noch nicht angewandt"
// ist fuer ein Dokument, das Autoritaet verteilt, aber kein neutraler Zustand -- wer
// `POLICY_EXCLUSIVE_STRIPE` ins Manifest schreibt, glaubt danach an eine Cache-Trennung, die
// niemand herstellt. Das ist schlimmer als gar kein Feld.
//
// Deshalb dasselbe Muster wie bei `CAP_PD_CONTROL` in A-2.1: der Kernel weist ein Manifest ab,
// dessen Politik er nicht einhalten kann, statt still weniger zu geben.
//
// **Warum `EXCLUSIVE_STRIPE` heute nicht einhaltbar ist** (und was fehlt): die Streifenvergabe
// steht seit B-4.2, aber ein GELADENES Programm bekommt seine Segmente und seinen Stack ueber
// `mem_alloc` als physisch ZUSAMMENHAENGENDE Region. Ein gefaerbter Lauf endet nach
// `MASK_BITS / PARTITIONS` Seiten (heute 64 KiB) -- `alloc_colored` weist groesseres ausdruecklich
// ab. Eine gefaerbte geladene PD braucht also stueckweise Allokation aus DEMSELBEN Streifen mit
// seitenweisem Mapping, und die Freigabe-Buchhaltung (`seglist`, `MAX_IMG_SEGS`) muss mitwachsen.
// Bis dahin waere jede "gefaerbte" geladene PD nur teilweise gefaerbt -- also gar nicht.
fn policy_gate(program_id: u32) -> Result<(), LoaderError> {
    let Some((_, flags)) = manifest_entry_of(program_id) else {
        return Ok(());
    };
    use caprock_loader::manifest as m;
    // **EXCLUSIVE_STRIPE wird seit dem 2026-08-07 EINGEHALTEN** (A1/Z11c) und steht deshalb nicht
    // mehr hier. Die alte Begruendung ("Segmente kommen zusammenhaengend aus mem_alloc") beschrieb
    // die damalige Allokation, nicht eine Notwendigkeit -- gemappt wurde schon immer seitenweise.
    // Siehe `system::load_into_pd_colored`.
    //
    // Was hier bleibt, ist der Rest, und der Grundsatz ist derselbe: **was der Kernel nicht
    // einhalten kann, wird abgewiesen, nicht ignoriert.** Ein Politikfeld, das nur gedruckt wird,
    // ist schlechter als keins -- es sieht konfiguriert aus.
    if flags & m::POLICY_PINNED != 0 {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt PINNED. Der \
             Lastausgleich ist heute per Vorgabe AUS, aber \"aus\" ist keine Zusicherung: \
             `balance_once` ist aufrufbar, und eine Affinitaet, die nur solange gilt, wie niemand \
             sie anfasst, ist keine."
        );
        return Err(LoaderError::UnsupportedPolicy);
    }
    Ok(())
}

/// **Die Politikfelder ausserhalb der Bitmaske** -- gilt fuer JEDEN Ladevorgang.
///
/// Getrennt von [`policy_gate`], weil sie eine andere Quelle haben: `policy_flags` ist eine
/// Bitmaske mit `POLICY_KNOWN`-Pruefung im Parser, diese hier sind Zahlen, und eine Zahl hat kein
/// "unbekannt". `numa_node = 0` heisst nicht "keine Aussage", sondern "Knoten 0" -- auf einer
/// Maschine ohne NUMA-Begriff ist das zufaellig richtig und auf der naechsten falsch.
fn zahlenpolitik_gate(program_id: u32) -> Result<(), LoaderError> {
    let Some(e) = manifest_zahlen_of(program_id) else {
        return Ok(());
    };
    let (numa, affin, prio, budget) = e;
    // NUMA gibt es nicht (Z8 offen): der `PhysAllocator` hat eine flache Freiliste ohne
    // Knotenbegriff. Ein Manifest, das Knoten 1 verlangt, bekaeme heute Knoten 0 und niemand
    // erfuehre es. Knoten 0 ist die einzige einhaltbare Angabe -- und auch nur, weil es genau
    // einen gibt.
    // **N4 (2026-08-17): der Wunsch wird EINGELOEST, wenn es den Knoten gibt** -- und sonst mit
    // einem UNTERSCHEIDBAREN Grund abgewiesen.
    //
    // Bis heute war jede Angabe ausser 0 pauschal abgelehnt, weil der Allokator keine Knoten
    // kannte. Das war richtig, solange es stimmte. Jetzt gibt es zwei verschiedene Absagen, und
    // der Unterschied ist die halbe Diagnose:
    //
    //   * die Maschine hat KEINE lesbare Topologie -> das Dokument ist nicht einloesbar,
    //   * die Topologie ist lesbar und kennt diesen Knoten NICHT -> das Dokument ist falsch.
    //
    // Ein gemeinsamer Text haette beide Faelle gleich aussehen lassen, und das ist die Form, an
    // der dieses Projekt schon mehrfach Stunden verloren hat („fehlend und kaputt duerfen nicht
    // gleich aussehen").
    //
    // **`numa_node = 0` bleibt die Vorgabe und wird NICHT als Wunsch gewertet.** Das Format kann
    // "nichts gesagt" nicht von "Knoten 0" unterscheiden (dieselbe Formatgrenze wie bei
    // `priority = 0`), und aus einem schweigenden Dokument einen Knotenwunsch zu erfinden waere
    // die A1-Lehre andersherum.
    if numa != 0 {
        // **Erst fragen, ob ueberhaupt schon gelesen wurde** (2026-08-20). „Noch nicht gelesen"
        // und „die Maschine hat keine" sind zwei Lagen, und sie verlangen entgegengesetzte
        // Reaktionen: die eine ist ein Programmfehler im Hochlauf, die andere eine Eigenschaft der
        // Maschine. Bis heute sahen sie gleich aus -- der Lader lief VOR `numa::init()` und wies
        // auf einer Zwei-Knoten-Maschine mit „keine tragfaehige Topologie" ab.
        //
        // Die Behebung ist die Reihenfolge; diese Zeile ist das **Gatter dagegen, dass sie jemand
        // zurueckdreht**. Ohne sie waere die Behebung eine Gewohnheit.
        if !crate::numa::gelesen() {
            #[cfg(feature = "debug-log")]
            println!(
                "loader  : REIHENFOLGE-FEHLER -- program_id {program_id} verlangt numa_node={numa}, \
                 aber `numa::init()` ist noch nicht gelaufen. Das ist KEINE Aussage ueber die \
                 Maschine, sondern ueber den Hochlauf: das Gatter fragt eine Topologie, die es noch \
                 nicht gibt. (Bis 2026-08-20 stand `numa::init()` NACH dem Lader, und der Wunsch \
                 wurde auf einer Zwei-Knoten-Maschine abgewiesen.)"
            );
            return Err(LoaderError::UnsupportedPolicy);
        }
        let topo = crate::numa::topology();
        if !topo.trustworthy() {
            println!(
                "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt numa_node={numa}, \
                 die Maschine hat keine tragfaehige Topologie (readable={} truncated={}). Ein \
                 Wunsch, den niemand pruefen kann, wird nicht still erfuellt.",
                topo.readable, topo.truncated
            );
            return Err(LoaderError::UnsupportedPolicy);
        }
        if numa as usize >= topo.node_count() {
            #[cfg(feature = "debug-log")]
            println!(
                "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt numa_node={numa}, \
                 die Maschine hat {} Knoten (0..{}).",
                topo.node_count(),
                topo.node_count().saturating_sub(1)
            );
            return Err(LoaderError::UnsupportedPolicy);
        }
    }
    // Affinitaet, Prioritaet und Budget werden EINGEHALTEN -- s. `endow_and_load`. Hier steht nur
    // die Schranke: eine Kernnummer, die es nicht gibt, ist ein Fehler im Dokument.
    if affin != caprock_loader::manifest::ANY_CORE && (affin as usize) >= crate::system::num_cores()
    {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt core_affinity={affin}, \
             die Maschine hat {} Kerne.",
            crate::system::num_cores()
        );
        return Err(LoaderError::UnsupportedPolicy);
    }
    // **Prioritaet**: der Scheduler kennt 0..NPRIO-1 (8). Eine Zahl darueber ist kein "so hoch wie
    // moeglich", sondern ein Fehler im Dokument.
    if prio as usize >= caprock_sched::NPRIO {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt priority={prio}, der \
             Scheduler kennt 0..{}.",
            caprock_sched::NPRIO - 1
        );
        return Err(LoaderError::UnsupportedPolicy);
    }
    // **Budget: abgewiesen, und zwar nicht aus Bequemlichkeit.**
    //
    // Eine MCS-Reservierung besteht aus ZWEI Zahlen -- Budget *und* Periode ("wie viel je
    // wie lang"). Das Manifest hat nur `budget_us`. Aus einer Zahl eine Reservierung zu machen
    // hiesse, die Periode zu erfinden; sie stuende dann in keinem Dokument, waere auf keiner
    // Maschine nachlesbar, und ein Manifest, das "200 us" sagt, bekaeme eine Garantie, die es nie
    // verlangt hat.
    //
    // Dazu kommt die Aufloesung: `set_budget` rechnet in TICKS (100 Hz -> 10 000 us). Alles unter
    // einem Tick waere entweder 0 (kein Budget, also das Gegenteil des Gewuenschten) oder
    // aufgerundet -- eine stille Vervielfachung.
    //
    // Das ist deshalb eine **Formatfrage**, keine Kernelfrage: solange das Manifest keine Periode
    // traegt, ist `budget_us` nicht einhaltbar. Steht als offener Punkt in `todo.md` Z11c.
    if budget != 0 {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : POLICY ABGEWIESEN -- program_id {program_id} verlangt budget_us={budget}. \
             Eine MCS-Reservierung braucht Budget UND Periode; das Manifestformat hat nur eine \
             Zahl. Eine erfundene Periode waere eine Zusicherung, die niemand verlangt hat."
        );
        return Err(LoaderError::UnsupportedPolicy);
    }
    Ok(())
}

/// `(numa_node, core_affinity, priority, budget_us)` dieser `program_id`.
fn manifest_zahlen_of(program_id: u32) -> Option<(u32, u32, u32, u32)> {
    let m = read_manifest()?;
    (0..m.count())
        .filter_map(|i| m.entry(i))
        .find(|e| e.program_id == program_id)
        .map(|e| (e.numa_node, e.core_affinity, e.priority, e.budget_us))
}

/// Die [`crate::system::LadePolitik`] dieser `program_id` aus dem angenommenen Manifest.
///
/// **Nach den Gates**, nie davor: hier wird nichts mehr geprueft, sondern nur uebersetzt. Was der
/// Kernel nicht einhalten kann, hat `policy_gate`/`zahlenpolitik_gate` bereits abgewiesen -- diese
/// Funktion darf deshalb annehmen, dass jeder Wert gilt. Steht kein Eintrag im Manifest, gilt die
/// Vorgabe, und die ist bitgleich das Verhalten vor Z11c.
/// **Der Heimatkern ist ab C8 ein Argument, keine Umgebungsgroesse.**
///
/// Vorher stand in `LadePolitik.core` ein `None`, und `load_into_pd_mit` las daraufhin
/// `hal::cpu::core_id()` -- also den Kern dessen, der gerade laed. Bis C8 war das der **Aufrufer**;
/// seither ist es der Verifiziererthread, und jedes ohne Affinitaet geladene Programm waere
/// stillschweigend auf dessen Kern gewandert. Eine Platzierungspolitik, die sich als Nebenwirkung
/// einer Stack-Verschiebung aendert, ist genau die Sorte stiller Aenderung, die dieses Projekt
/// mehrfach bezahlt hat -- deshalb traegt der Auftrag den Kern des Aufrufers mit.
/// **Und der angebotene Slot ist aus demselben Grund ein Argument** (2026-08-25).
///
/// Woertlich dieselbe Ueberlegung wie beim Heimatkern eine Zeile darueber: welcher Slot ein
/// **Angebot** des Aufrufers traegt, ist eine Eigenschaft des Ladevorgangs und nicht der
/// Umgebung. Nur `load_by_index` weiss es -- dort ist `endow` die Delegation und `manifest_caps`
/// die Zusage. Wer hier `None` uebergibt, sagt: „alles in diesem Endowment ist eine Zusage",
/// und genau das gilt fuer den Root-Task und die In-Kernel-Pfade.
fn ladepolitik_auf(
    program_id: u32,
    heimatkern: usize,
    angebot: u16,
    cap_budget: u16,
) -> crate::system::LadePolitik {
    use crate::system::LadePolitik;
    let farbig = manifest_entry_of(program_id)
        .is_some_and(|(_, f)| f & caprock_loader::manifest::POLICY_EXCLUSIVE_STRIPE != 0);
    let Some((numa, affin, prio, _budget)) = manifest_zahlen_of(program_id) else {
        return LadePolitik {
            farbig,
            core: Some(heimatkern),
            angebotene_slots: angebot,
            cap_budget,
            ..LadePolitik::VORGABE
        };
    };
    LadePolitik {
        farbig,
        node: if numa != 0 {
            caprock_hal::numa::Node::At(numa as u8)
        } else {
            caprock_hal::numa::Node::Unaffiliated
        },
        angebotene_slots: angebot,
        cap_budget,
        // `priority = 0` heisst im Manifest "nichts gesagt" -- 0 ist im Scheduler eine gueltige
        // (die niedrigste) Prioritaet, aber ein Dokument, das schweigt, soll die Vorgabe bekommen
        // und nicht die niedrigste. Wer wirklich 0 will, sagt es heute nicht unterscheidbar; das
        // ist eine Formatgrenze und steht als solche in `todo.md`.
        prio: if prio == 0 { LadePolitik::VORGABE.prio } else { prio as u8 },
        // **Die Rangfolge steht hier und nur hier** (N4): eine ausdrueckliche `core_affinity`
        // sticht -- sie benennt EINEN Kern und ist damit praeziser als ein Knoten. Sagt das
        // Dokument nur einen Knoten, waehlt N3 den am wenigsten belasteten Kern DIESES Knotens.
        // Schweigt es zu beidem, gilt weiterhin der Kern des Aufrufers (C8).
        //
        // Dass der Knoten hier ueberhaupt einloesbar ist, hat `zahlenpolitik_gate` bereits
        // geprueft; kaeme ein unbekannter Knoten bis hierher, faende `least_loaded_core_on`
        // keinen Kern und faellt auf die globale Wahl zurueck -- gezaehlt als `remote`.
        core: Some(if affin != caprock_loader::manifest::ANY_CORE {
            affin as usize
        } else if numa != 0 {
            crate::system::least_loaded_core_on(caprock_hal::numa::Node::At(numa as u8))
        } else {
            heimatkern
        }),
        budget_us: 0, // abgewiesen, s. `zahlenpolitik_gate`
    }
}

/// **A-4.5 mit Zaehnen:** ein als nicht austauschbar markiertes Programm wird kein zweites Mal
/// geladen.
///
/// Die Negativliste in `docs/invariants.md` §13 sagt, was nicht austauschbar ist; hier wird es
/// durchgesetzt. Ein zweiter Ladevorgang derselben `program_id` in derselben Laufzeit **ist** der
/// Austausch -- ein anderer Weg, eine PD zu ersetzen, existiert nicht.
fn hotreload_gate(program_id: u32, schon_geladen: bool) -> Result<(), LoaderError> {
    if !schon_geladen {
        return Ok(());
    }
    let Some((_, flags)) = manifest_entry_of(program_id) else {
        return Ok(());
    };
    if flags & caprock_loader::manifest::POLICY_NO_HOTRELOAD != 0 {
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : A-4.5 ABGEWIESEN -- program_id {program_id} ist als NO_HOTRELOAD markiert \
             und wurde bereits geladen."
        );
        return Err(LoaderError::HotReloadForbidden);
    }
    Ok(())
}

/// A-4.4-Gate. Beim ersten Laden einer `program_id` wird ihre Schnittstellenversion festgehalten,
/// bei jedem weiteren gegen sie geprueft.
fn iface_gate(program_id: u32) -> Result<(), LoaderError> {
    policy_gate(program_id)?; // zuerst: eine unerfuellbare Politik macht alles Weitere sinnlos
    zahlenpolitik_gate(program_id)?;
    let schon = iface_bekannt(program_id);
    hotreload_gate(program_id, schon)?;
    let Some(jetzt) = manifest_iface_version(program_id) else {
        return Ok(()); // kein Manifest-Eintrag -> keine Aussage moeglich, also auch keine gemacht
    };
    iface_record_or_check(program_id, jetzt)
}

/// Wurde diese `program_id` in dieser Laufzeit schon geladen?
fn iface_bekannt(program_id: u32) -> bool {
    let g = IFACE_SEEN.lock();
    let (tab, used) = &*g;
    tab[..*used].iter().any(|&(id, _)| id == program_id)
}

/// Der pruefbare Kern von [`iface_gate`]: Buchhaltung und Vergleich, ohne Manifest.
///
/// **Getrennt, weil der Gate sonst unpruefbar waere.** Pro Boot gibt es genau EIN Manifest; jeder
/// Ladevorgang derselben `program_id` liest also zwangslaeufig dieselbe `iface_version`, und der
/// Abweisungszweig kann ueber [`iface_gate`] gar nicht erreicht werden. Er wird es erst, wenn ein
/// Austausch zur Laufzeit ein ANDERES Image mitbringt (A-4.1/A-4.3) -- diesen Weg gibt es heute
/// nicht. Bis dahin waere der Zweig ungeprueft, und ungeprueft heisst hier: vermutlich kaputt,
/// wenn er zum ersten Mal gebraucht wird. Der Selbsttest fuettert deshalb diese Funktion direkt.
fn iface_record_or_check(program_id: u32, jetzt: u32) -> Result<(), LoaderError> {
    // **Unter der Sperre wird ENTSCHIEDEN, gedruckt wird danach.**
    //
    // Bis zum 2026-08-13 stand hier ein `println!` **im** kritischen Abschnitt, und seit C9b ist
    // das die groesste IRQ-maskierte Sperrhaltung des Systems (269-390 Promille eines Ticks).
    // C9b wirkt hier strukturell nicht: die Konsole maskiert seither nur noch je Byte, aber
    // **wer maskiert hereinkommt, bleibt maskiert** -- `SpinLock::lock` hat die IRQs schon
    // gesperrt, bevor die Konsole ueberhaupt gerufen wird.
    //
    // Die Form ist dieselbe wie beim Verifiziererthread (`while let Some(a) = SCHLANGE.lock()..`)
    // und wie E-Rest 3b (`match lock() { .. None => lock() }`): **die Lebensdauer des Guards ist
    // laenger als die Arbeit, die ihn braucht.** Hier ist die Arbeit ein Tabellenblick; das
    // Drucken gehoert nicht dazu.
    enum Urteil {
        Ok,
        Geaendert(u32),
        Voll,
    }
    let urteil = {
        let mut g = IFACE_SEEN.lock();
        let (tab, used) = &mut *g;
        if let Some(&(_, zuerst)) = tab[..*used].iter().find(|&&(id, _)| id == program_id) {
            if zuerst != jetzt {
                Urteil::Geaendert(zuerst)
            } else {
                Urteil::Ok
            }
        } else if *used >= MAX_IFACE_TRACKED {
            Urteil::Voll
        } else {
            tab[*used] = (program_id, jetzt);
            *used += 1;
            Urteil::Ok
        }
    }; // <- der Guard faellt HIER, vor jeder Ausgabe

    match urteil {
        Urteil::Ok => Ok(()),
        Urteil::Geaendert(_zuerst) => {
            #[cfg(feature = "debug-log")]
            println!(
                "loader  : A-4.4 ABGEWIESEN -- program_id {program_id} wurde mit iface_version \
                 {_zuerst} geladen, das Archiv bietet {jetzt}. Ein Austausch ueber dieselbe \
                 Endpoint-Cap darf die Schnittstelle nicht aendern."
            );
            Err(LoaderError::IfaceVersionChanged)
        }
        Urteil::Voll => {
            #[cfg(feature = "debug-log")]
            println!(
                "loader  : A-4.4 ABGEWIESEN -- Versionsbuchhaltung voll ({MAX_IFACE_TRACKED} IDs). \
                 Lieber abweisen als ungeprueft laden."
            );
            Err(LoaderError::IfaceTableFull)
        }
    }
}


/// **A-4.4-Selbsttest:** beide Ausgaenge der Versionssperre belegen -- den durchgelassenen und den
/// abgewiesenen.
///
/// Faehrt gegen [`iface_record_or_check`] statt gegen [`iface_gate`], aus dem dort genannten Grund:
/// ueber das Manifest ist der Abweisungszweig heute nicht erreichbar. Benutzt `program_id`-Werte,
/// die kein Archiv vergibt, damit die echte Buchhaltung unberuehrt bleibt.
#[cfg(feature = "selftest")]
/// **A1 auf dem regulaeren Weg messen** (2026-08-07). `true`, wenn die Zeile ALL PASS meldet
/// oder die Frage auf dieser Maschine nicht entscheidbar ist (SKIP).
///
/// Ohne gefaerbt geladene PD gibt es nichts zu messen -- dann SKIP mit Grund, nicht ALL PASS.
/// Eine Zeile, die schweigt, weil der Fall nicht vorkam, darf nicht wie ein bestandener Test
/// aussehen.
#[cfg(feature = "selftest")]
pub fn run_pdcolor() -> bool {
    let Some((asid_g, mask, asid_u)) = crate::system::pdcolor_kandidaten() else {
        println!(
            "pdcolor : SKIP -- in diesem Lauf wurde kein Programm mit POLICY_EXCLUSIVE_STRIPE \
             geladen. Es gibt nichts zu messen; ALL PASS waere hier eine Aussage ueber die \
             Abwesenheit des Falls, nicht ueber die Faerbung"
        );
        return true;
    };
    let t = crate::colors::run_pd_color(asid_g, mask, asid_u);
    crate::colors::report_pd_color(&t);
    t.ok || !t.entscheidbar
}

/// **Z11c: wird die Politik des Manifests wirklich ANGEWANDT?** (2026-08-07)
///
/// Die Frage ist nicht, ob der Lader die Zahl gelesen hat -- das druckt die `manifest:`-Zeile
/// seit A-1.4. Die Frage ist, ob der Thread sie **hat**. Gelesen wird deshalb der TCB
/// (`system::priority_of`), nicht der Ladepfad.
///
/// **Warum der Kern dieser Zeile nicht das Scheduling-Verhalten ist.** Die naheliegende Pruefung
/// waere „laeuft der hoeher priorisierte Thread zuerst?". Die haengt am Verhalten der geladenen
/// Programme und misst damit die Programme, nicht die Zuteilung. Sie hat beim Bau dieser Sache
/// auch prompt etwas anderes gezeigt: ein POLLENDER Treiber (B-3.2, kein IRQ) auf einer hoeheren
/// Prioritaet als sein Client laesst den Client verhungern -- richtig zugeteilt und trotzdem
/// unbrauchbar. Das ist ein Befund ueber Treiber ohne Budget, nicht ueber die Zuteilung.
#[cfg(feature = "selftest")]
pub fn run_ladepolitik() -> bool {
    let Some((verlangt, bekommen, kern, kern_bekommen)) = crate::system::ladepolitik_abweichend()
    else {
        println!(
            "ladepol : SKIP -- in diesem Lauf verlangte kein Manifest-Eintrag eine Politik, die \
             von der Vorgabe abweicht. Ein Vergleich 'verlangt == bekommen' waere hier auch dann \
             wahr, wenn der Ladepfad die Angabe gar nicht liest"
        );
        return true;
    };
    let kern_ok = match kern {
        None => true, // keine Affinitaet verlangt -> nichts zu pruefen
        Some(k) => kern_bekommen == Some(k),
    };
    let prio_ok = bekommen == Some(verlangt);
    #[cfg(feature = "debug-log")]
    println!(
        "ladepol : Thread mit ABWEICHENDER Politik: Manifest verlangte prio={verlangt} \
         kern={kern:?}, Scheduler gab prio={bekommen:?} kern={kern_bekommen:?} (beim LADEN \
         erfasst, nicht im Bericht zurueckgelesen -- der Thread darf inzwischen tot sein)"
    );
    println!(
        "ladepol : {} (Z11c: die Politik des Manifests wird ANGEWANDT, nicht nur gelesen. \
         Gelesen wird der TCB, nicht der Ladepfad -- ein Lader, der bestaetigt, was er selbst \
         getan hat, bestaetigt nichts)",
        if prio_ok && kern_ok { "ALL PASS" } else { "FAILURES" }
    );
    prio_ok && kern_ok
}

pub fn run_iface_gate() -> bool {
    const ID: u32 = 0xA44_0001; // kein Archiv vergibt IDs in dieser Gegend
    let erst = iface_record_or_check(ID, 7).is_ok();
    let gleich = iface_record_or_check(ID, 7).is_ok();
    let anders = matches!(
        iface_record_or_check(ID, 8),
        Err(LoaderError::IfaceVersionChanged)
    );
    // Und die Sperre darf nicht ueber die ID hinaus greifen: eine ANDERE ID mit derselben
    // Nummer 8 muss durchgehen, sonst wuerde der Gate fremde Programme mitsperren.
    let fremd = iface_record_or_check(ID + 1, 8).is_ok();
    // A-4.5: die Hot-Reload-Sperre, beide Ausgaenge. Ohne Manifest-Eintrag darf sie NICHT greifen
    // (sonst spraeche sie eine Zusage aus, die im Dokument gar nicht steht) -- und die
    // Testkennungen kommen in keinem Archiv vor.
    let ohne_eintrag_durch = hotreload_gate(ID, true).is_ok();
    let unbekannt_durch = hotreload_gate(ID + 2, false).is_ok();
    let ok = erst && gleich && anders && fremd && ohne_eintrag_durch && unbekannt_durch;
    #[cfg(feature = "debug-log")]
    println!(
        "iface   : erstes Laden {erst}; gleiche Version {gleich}; GEAENDERTE Version abgewiesen \
         {anders}; andere program_id unberuehrt {fremd}; ohne Manifest-Eintrag keine \
         Hot-Reload-Sperre {ohne_eintrag_durch}; Erstladung nie gesperrt {unbekannt_durch}"
    );
    println!(
        "iface   : {} (A-4.4: ein Austausch, der die Schnittstellenversion aendert, wird abgewiesen -- \
         beide Ausgaenge belegt, nicht nur der erlaubte)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    ok
}

pub fn load_image(
    prog: &Program,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
) -> Result<(ThreadId, usize), LoaderError> {
    load_image_auf(prog, endow, boot_arg, crate::system::eigener_kern(), 0, 0)
}

/// Wie [`load_image`], aber mit **ausdruecklich genanntem Heimatkern** (C8) — s. `load_by_index`.
pub fn load_image_auf(
    prog: &Program,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
    heimatkern: usize,
    angebot: u16,
    cap_budget: u16,
) -> Result<(ThreadId, usize), LoaderError> {
    if !verify_image(prog) {
        return Err(LoaderError::Unverified);
    }
    iface_gate(prog.program_id)?; // A-4.4, nach dem Trust-Gate: erst echt, dann kompatibel
    let domain = match prog.domain {
        DOMAIN_USERLAND => Domain::UserLand,
        // TrustedSAS laeuft GELADEN als EL0-ISOLIERTE PD (nicht EL1): hardware-isoliert, behaelt
        // aber seine Trust-Stufe (darf PdControl/Loader-Caps halten). domain_audit erlaubt isolierte
        // TrustedSAS-PDs. So sind ALLE Domaenen sicher extern ladbar (auch die Trusted-Testdienste).
        DOMAIN_TRUSTED => Domain::TrustedSas,
        // HardwareLand braucht eine vor-erstellte Backend-PD (Partner-Bindung + Kanal) ->
        // ueber `load_program_into_pd`, NICHT hier (eine bare HardwareLand-PD bricht domain_audit).
        _ => return Err(LoaderError::UnsupportedDomain),
    };
    let img = ElfImage::parse(prog.elf)?; // Safe-Rust-Validierung; unsafe erst im Kopier-Glue
    // **Beide Ladepfade, sonst ist die Politik umgehbar** -- genau wie beim A-4.4-Gate daneben.
    // Der erste Anlauf stellte nur `load_program_into_pd` um; `hello` kommt aber ueber DIESEN Weg,
    // und die `pdcolor`-Zeile meldete daraufhin SKIP. Dass sie SKIP meldete und nicht ALL PASS,
    // ist der Grund, warum es aufgefallen ist.
    // Zuordnung Thread -> Programm auf BEIDEN Ladepfaden eintragen, aus demselben Grund wie das
    // A-4.4-Gate und die Ladepolitik daneben: was nur an einem Weg haengt, ist am anderen weg.
    let r = crate::system::load_elf_mit(
        &img,
        domain,
        endow,
        boot_arg,
        ladepolitik_auf(prog.program_id, heimatkern, angebot, cap_budget),
    )
    .ok_or(LoaderError::NoResources)?;
    record_program_thread(prog.program_id, r.0);
    // Z7: jedes ERFOLGREICH geladene Image verlaengert die Messkette (Boot + Laufzeit --
    // beide kommen ueber diese beiden Funktionen; der LXPD-Hook selbst bleibt unangetastet).
    messkette_verlaengern(prog.program_id, prog.domain, sha256(prog.elf));
    Ok(r)
}

/// Ein Programm in eine **vor-erstellte** PD laden (ext-26, L3) — fuer HardwareLand-Backends
/// (Partner-Bindung + Kanal vom Aufrufer aufgesetzt) und kuenftige spezialisierte PDs. Die
/// Domaenen-Policy traegt die PD selbst (`install_cap_checked` + `domain_audit`). Trust-Gate
/// ([`verify_image`]) gilt auch hier.
pub fn load_program_into_pd(
    prog: &Program,
    pd: usize,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
) -> Result<ThreadId, LoaderError> {
    load_program_into_pd_auf(prog, pd, endow, boot_arg, crate::system::eigener_kern(), 0, 0)
}

/// Wie [`load_program_into_pd`], aber mit **genanntem Heimatkern** (C8) — s. `load_by_index`.
pub fn load_program_into_pd_auf(
    prog: &Program,
    pd: usize,
    endow: &[(usize, CapPtr)],
    boot_arg: usize,
    heimatkern: usize,
    angebot: u16,
    cap_budget: u16,
) -> Result<ThreadId, LoaderError> {
    if !verify_image(prog) {
        return Err(LoaderError::Unverified);
    }
    iface_gate(prog.program_id)?; // A-4.4 -- gilt auf BEIDEN Ladepfaden, sonst ist er umgehbar
    let img = ElfImage::parse(prog.elf)?;
    let tid = crate::system::load_into_pd_mit(
        &img,
        pd,
        endow,
        boot_arg,
        ladepolitik_auf(prog.program_id, heimatkern, angebot, cap_budget),
    )
    .ok_or(LoaderError::NoResources)?;
    record_program_thread(prog.program_id, tid);
    // Z7: zweites Blatt der Messkette (das erste ist `load_image_auf` — fremder Vorlauf,
    // dort unangetastet). `lxpddrv_laden` (Boot + `load_verified_image`) laeuft DURCH diese
    // Funktion und misst damit genau einmal; ein Zaehlpunkt in `lxpddrv_laden` selbst zaehlte
    // doppelt.
    messkette_verlaengern(prog.program_id, prog.domain, sha256(prog.elf));
    debuggable_praegen(prog.program_id, pd);
    Ok(tid)
}

/// **Die einzige Stelle, an der eine `Debuggable`-Cap entsteht** (Z6b).
///
/// ## Warum sie eine eigene Funktion ist und keine Zeile im Ladepfad
///
/// Weil sie die Zusage IST. „Eine PD, ueber die nie eine `Debuggable` gepraegt wurde, kann nicht
/// debuggt werden" haelt nur, solange es **genau einen** Weg zu einer solchen Cap gibt und dieser
/// eine ausdrueckliche Angabe verlangt. Ein zweiter Weg — ein Syscall, ein Nachreichen, ein
/// Vorgabewert — macht den Satz zu einer Bitte. Deshalb: eine Funktion, ein Aufrufer, kein
/// Vorgabe-Ja.
///
/// ## Warum sie LAUT ist
///
/// Die Praegung ist eine benannte, protokollierte Handlung; bei Mandantenlasten eine, die der
/// Kunde sehen koennen muss. Eine stille Praegung waere allgegenwaertige Autoritaet mit
/// Zwischenschritten.
///
/// ## Und warum das Schweigen ebenfalls eine Zeile bekommt
///
/// Ein Pruefer, der ueber Abwesenheit urteilt, muss belegen koennen, dass er sprechfaehig ist. Ohne
/// die zweite Zeile waere „keine Praegung" von „diese Funktion wurde nie gerufen" nicht zu
/// unterscheiden — und ein leerer Lauf ist kein Testergebnis.
fn debuggable_praegen(program_id: u32, pd: usize) {
    let gewuenscht = manifest_entry_of(program_id)
        .is_some_and(|(_, f)| f & caprock_loader::manifest::POLICY_DEBUGGABLE != 0);
    if !gewuenscht {
        DEBUGGABLE_NICHT_GEPRAEGT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "debug-log")]
        println!(
            "loader  : pd {pd} (program_id {program_id}) OHNE Debuggable -- kein Debugger kann sie              anfassen, auch keiner mit jeder anderen Cap des Systems"
        );
        return;
    }
    match crate::system::mint_debuggable(pd) {
        Some(_) => {
            DEBUGGABLE_GEPRAEGT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            #[cfg(feature = "debug-log")]
            println!(
                "loader  : AUDIT Debuggable GEPRAEGT fuer pd {pd} (program_id {program_id}) --                  diese PD ist ab jetzt debuggbar; das Fenster endet mit dem Revoke DIESER Cap,                  nicht mit dem Ablauf eines abgeleiteten Rechts"
            );
        }
        None => {
            #[cfg(feature = "debug-log")]
            println!(
            "loader  : Debuggable fuer pd {pd} NICHT praegbar (kein Slot) -- die PD laeuft, ist              aber nicht debuggbar. Fail-closed: das ist die sichere Richtung"
        );
        }
    }
}

/// Wie oft eine `Debuggable` gepraegt wurde -- und wie oft ausdruecklich nicht (Z6b).
/// **Zwei Zaehler, weil es zwei Aussagen sind:** `0/0` heisst „nichts geladen", nicht „nichts
/// gepraegt".
pub static DEBUGGABLE_GEPRAEGT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static DEBUGGABLE_NICHT_GEPRAEGT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// **Integritaets-/Trust-Gate** (ADR 0011 §7 + ADR 0014, ext-28): darf dieses Image geladen werden?
///
/// **Alle** geladenen Prozesse laufen **EL0-isoliert** (`load_into_pd` spawnt stets EL0; selbst eine
/// TrustedSAS-PD ist geladen EL0-isoliert) — ein fehlerhaftes/boesartiges Image faultet daher nur
/// sich selbst, ohne Privileg-Eskalation. Daher braucht **UserLand/HardwareLand kein Zertifikat**
/// (unveraendert; ihre Isolation traegt die Hardware).
///
/// **TrustedSAS** dagegen darf seine Trust-Stufe behalten (PdControl-/Loader-Caps halten duerfen) —
/// deshalb wird es **nur mit einem gueltigen, auf genau dieses Binary gebundenen Ed25519-Zertifikat**
/// geladen ([`verify_trusted_cert`]). Ohne gueltiges Zertifikat: [`LoaderError::Unverified`].
fn verify_image(prog: &Program) -> bool {
    if prog.domain != DOMAIN_TRUSTED {
        return true; // UserLand/HardwareLand: hardware-isoliert, kein Zertifikat noetig.
    }
    verify_trusted_cert(prog)
}

/// **TrustedSAS-Zertifikatspruefung** (ext-28, ADR 0014). Akzeptiert das Image **nur**, wenn das
/// mitgelieferte Zertifikat in **jeder** Hinsicht gueltig ist. Rein verifizierend (kein Heap, kein
/// RNG); die read-only Key-DB ([`TRUSTED_KEYS`]) ist in den Kernel kompiliert.
///
/// Geprueft (alle Felder sind durch die Signatur ueber die **gesamte** Nachricht geschuetzt):
/// 1. Zertifikat parst (Magic/Formatversion/Laengen — [`TrustedCert::parse`]).
/// 2. Signaturalgorithmus = Ed25519 **und** Signaturlaenge = 64.
/// 3. `key_id` in der Key-DB vorhanden, **nicht** zurueckgezogen, und DB-selbstkonsistent
///    (`key_id == fingerprint(pubkey)`).
/// 4. Ed25519-Signatur ueber die **gesamte** Nachricht gueltig (`verify_strict`).
/// 5. **Binary-Bindung:** `binary_hash == SHA-256(ELF)` und `manifest_hash == SHA-256(Manifest)`.
/// 6. **Identitaet:** `program_id`/`version` stimmen mit dem Archiv-Eintrag ueberein.
/// 7. **Anti-Downgrade:** `version >= MIN_VERSION[program_id]` (falls gepflegt).
/// 8. **Unsafe-Audit:** `unsafe_status == ALL_PASS` (Programm forbid-rein, Projekt sauber,
///    Allowlist erfuellt).
fn verify_trusted_cert(prog: &Program) -> bool {
    let Ok(cert) = TrustedCert::parse(prog.cert) else {
        return false; // (1) fehlend/kaputt
    };
    // (2) Algorithmus + Signaturlaenge passend zum Verfahren.
    if cert.signature_algorithm_id != SIG_ALG_ED25519 {
        return false;
    }
    let Ok(sig): Result<&[u8; SIG_ED25519_LEN], _> = cert.signature().try_into() else {
        return false;
    };
    // (3) Key-ID-Lookup in der read-only Key-DB; Revocation + DB-Selbstkonsistenz.
    let Some(key) = TRUSTED_KEYS.iter().find(|k| k.key_id == cert.key_id) else {
        return false;
    };
    if key.revoked || fingerprint(&key.pubkey) != key.key_id {
        return false;
    }
    // (4) Signatur ueber die GESAMTE Nachricht.
    if !verify_sig(&key.pubkey, cert.message(), sig) {
        return false;
    }
    // (5) Binary-/Manifest-Bindung.
    if cert.binary_hash != sha256(prog.elf) || cert.manifest_hash != sha256(prog.manifest) {
        return false;
    }
    // (6) Identitaets-Konsistenz mit dem Archiv-Eintrag.
    if cert.program_id != prog.program_id || cert.version != prog.version {
        return false;
    }
    // (7) Anti-Downgrade (firmware-gepflegte Untergrenze je program_id).
    if let Some(&(_, min_ver)) = MIN_VERSION.iter().find(|&&(id, _)| id == prog.program_id) {
        if cert.version < min_ver {
            return false;
        }
    }
    // (8) Unsafe-Audit-Status: alle TrustedSAS-Regeln bestanden.
    cert.unsafe_all_pass()
}

/// Größe des Puffers für die manipulierte Zertifikatskopie im [`trust_audit`]-Live-Oracle.
const TRUST_AUDIT_CERT_MAX: usize = 512;

/// **Nur das Trust-Gate auswerten, NICHT laden** (ext-28). Fuehrt [`verify_image`] aus, ohne
/// Ressourcen zu allozieren — ausschliesslich fuer den `certfuzz`-Fuzzer (ADR 0013, daher nur mit
/// Feature `kernel-fuzz` einkompiliert; der Release-Kernel braucht diesen verify-only-Pfad nicht).
#[cfg(feature = "kernel-fuzz")]
pub fn verify_only(prog: &Program) -> bool {
    verify_image(prog)
}

/// **TrustedSAS-Trust-Audit** (ext-28, ADR 0014) — bleibt **immer** im Kernel (kein Fuzzer-Feature).
/// Strukturelle Selbstkonsistenz der read-only Key-DB + Live-Oracle des aktiven Gates. `0` = sauber,
/// sonst ein Anomalie-Code:
/// * 1 — Key-DB leer (kein TrustedSAS koennte je geladen werden).
/// * 2 — ein `key_id != fingerprint(pubkey)` (DB nicht selbst-zertifizierend).
/// * 3 — doppelte `key_id` in der DB.
/// * 4 — Live-Oracle: ein bekannt **gueltiges** Zertifikat wird abgelehnt (Gate/DB defekt).
/// * 5 — Live-Oracle: eine **manipulierte** Kopie wird akzeptiert (Gate setzt NICHT durch).
///
/// Die Invariante „jede geladene TrustedSAS-PD stammt aus einem verifizierten Zertifikat" gilt
/// **strukturell**: [`load_image`]/[`load_program_into_pd`] rufen [`verify_image`] **vor** jeder
/// Ressourcenvergabe — ein abgelehntes Image erzeugt weder Thread noch PD. Code 4/5 belegen, dass
/// dieses Gate zur Audit-Zeit aktiv durchsetzt (akzeptiert gueltige, weist manipulierte ab).
pub fn trust_audit() -> u32 {
    if TRUSTED_KEYS.is_empty() {
        return 1;
    }
    for (i, k) in TRUSTED_KEYS.iter().enumerate() {
        if fingerprint(&k.pubkey) != k.key_id {
            return 2;
        }
        for k2 in &TRUSTED_KEYS[i + 1..] {
            if k2.key_id == k.key_id {
                return 3;
            }
        }
    }
    // Live-Oracle gegen das erste echte TrustedSAS-Zertifikat im Archiv (falls vorhanden).
    if let Some(archive) = read_archive() {
        if let Some(tx) = archive
            .iter()
            .find(|p| p.domain == DOMAIN_TRUSTED && !p.cert.is_empty())
        {
            if !verify_image(&tx) {
                return 4; // gueltig -> MUSS akzeptiert werden
            }
            let c = tx.cert;
            if c.len() <= TRUST_AUDIT_CERT_MAX {
                let mut buf = [0u8; TRUST_AUDIT_CERT_MAX];
                buf[..c.len()].copy_from_slice(c);
                buf[40] ^= 0x01; // ein signiertes Byte (binary_hash) kippen -> Signatur bricht
                let tampered = Program::new(
                    tx.program_id,
                    b"trust-audit",
                    tx.version,
                    DOMAIN_TRUSTED,
                    [0u8; 32],
                    tx.elf,
                    tx.manifest,
                    &buf[..c.len()],
                );
                if verify_image(&tampered) {
                    return 5; // manipuliert -> MUSS abgelehnt werden
                }
            }
        }
    }
    0
}

/// `SYS_LOAD`-Callback (ext-26, L2): das Programm mit Index `index` aus dem Boot-Archiv laden +
/// die `endow`-Caps (vom Dispatch aus dem Aufrufer-Cspace delegiert) in die neue PD endowen. Gibt
/// die neue PD-Id. Der Dispatch hat die `Loader`-Cap-Autoritaet bereits geprueft.
/// **`heimatkern` ist der Kern des AUFRUFERS, nicht der des Ausfuehrenden** (C8). Seit der
/// Verifiziererthread diese Funktion faehrt, sind die beiden verschieden. `load_into_pd_mit` legt
/// einen ohne Affinitaet geladenen Thread auf `hal::cpu::core_id()` -- liefe das ueber den
/// Verifizierer, waere die Platzierungspolitik als Nebenwirkung einer Stack-Verschiebung
/// stillschweigend eine andere geworden. Ein Manifest-Eintrag mit `core_affinity` sticht weiterhin.
pub fn load_by_index(
    index: u32,
    caller_pd: usize,
    endow: &[(usize, CapPtr)],
    heimatkern: usize,
    cap_budget: u16,
    dma_pages: u32,
) -> Option<usize> {
    // **Das Manifest gilt auch hier** (A-5.1 / Z11b). Bis dahin bekamen nur der Root-Task seine
    // Anfangs-Caps aus dem Manifest; alles Weitere lebte von dem, was der Lader delegierte. Damit
    // war das Manifest für alle außer einem Programm ein Wunschzettel: es stand darin, was eine
    // Komponente bekommen soll, und niemand setzte es um.
    //
    // Die Aufteilung ist sauber und bleibt es: die **Loader-Cap** sagt, WER laden darf, das
    // **Manifest** sagt, WAS das Geladene bekommt. Der Lader kann dadurch nichts vergeben, was
    // nicht aufgeschrieben ist — und ein Treiber-PD kann Geräte-Autorität haben, ohne dass der
    // Root-Task sie je besessen hätte (er könnte sie sonst gar nicht weiterreichen).
    //
    // **HardwareLand zuerst.** Ein Treiber ist per Entwurf ein Backend an einem Kanal (ext-22,
    // unveraenderlich an seinen Partner gebunden) -- eine "bare" HardwareLand-PD bricht
    // `domain_audit`. Der Kanal muss deshalb **vor** den Anfangs-Caps stehen: seine Notification
    // IST die Cap, die das Manifest mit `ntfn` meint. Eine frisch erzeugte waere eine fremde, und
    // die HardwareLand-Cap-Policy weist sie -- zu Recht -- ab.
    let hardware_backend = (|| {
        let archive = read_archive()?;
        let prog = archive.program(index as usize)?;
        if prog.domain != DOMAIN_HARDWARE {
            return None;
        }
        crate::system::create_hardware_backend(caller_pd, (prog.program_id & 0xffff) as u16)
    })();
    // **Ein Dienst OHNE Geraet bekommt seinen Kanal genauso VORHER** (2026-08-25).
    //
    // Derselbe Grund wie beim Backend eine Zeile darueber: die Notification dieses Kanals IST die
    // Cap, die das Manifest mit `ntfn` meint, und der Endpoint der, unter dem der Dienst
    // registriert wird. Erst danach kann `endow_from_manifest` sie in Slot 1 und 2 legen -- eine
    // frisch erzeugte waere eine andere, und der Dienst haette einen Kanal, den kein Client
    // findet.
    //
    // **Und ohne dieses Bit gibt es keinen Kanal**: eine PD, die nichts anbietet, bekommt bei
    // `CAP_ENDPOINT` weiterhin den Kanal des benannten Dienstes bzw. einen frischen. Genau diese
    // Unterscheidung war bis heute nicht ausdrueckbar.
    let dienst_kanal = (|| {
        if hardware_backend.is_some() {
            return None;
        }
        let archive = read_archive()?;
        let prog = archive.program(index as usize)?;
        let man = read_manifest()?;
        let e = (0..man.count())
            .filter_map(|i| man.entry(i))
            .find(|e| e.program_id == prog.program_id)?;
        if e.policy_flags & caprock_loader::manifest::POLICY_PROVIDES_SERVICE == 0 {
            return None;
        }
        crate::system::create_service_channel()
    })();
    let manifest_caps = (|| {
        let archive = read_archive()?;
        let prog = archive.program(index as usize)?;
        let man = read_manifest()?;
        let e = (0..man.count())
            .filter_map(|i| man.entry(i))
            .find(|e| e.program_id == prog.program_id)?;
        if e.initial_caps == 0 {
            return None;
        }
        let kanal = hardware_backend
            .map(|(_, ep, ntfn)| Kanal::Geraet(ep, ntfn))
            .or_else(|| dienst_kanal.map(|(ep, ntfn)| Kanal::Dienst(ep, ntfn)));
        endow_from_manifest(&e, kanal, dma_pages, hardware_backend.map(|(pd, _, _)| pd)).ok()
    })();
    // `CapPtr` hat bewusst keinen öffentlichen Konstruktor (ein fabrizierbarer Cap-Handle wäre eine
    // Einladung), also erst als `Option` sammeln und dann mit dem ersten echten Cap als Füllwert
    // verdichten — derselbe Weg wie in `start_root_task`.
    //
    // **Die Groesse ist hergeleitet:** alle Loader-Slots plus das eine Angebot des Aufrufers
    // (Konvention L2: hoechstens ein Cap, in Slot 0). Vorher stand hier die 8 als Literal, und sie
    // passte genau — weil ein Manifest, das Slot 0 selbst vergibt, mit dem Angebot des Aufrufers
    // ohnehin kollidiert. Eine Eigenschaft, die aus einer Groessenrelation folgt statt aus der
    // Struktur, verschwindet beim naechsten Slot: mit B2 waeren es 9 gewesen, und der Ueberlauf
    // haette sich als `collision` gemeldet — also mit dem Grund eines ganz anderen Fehlers.
    let mut slots: [Option<(usize, CapPtr)>; LOAD_CAPS_MAX] = [None; LOAD_CAPS_MAX];
    let mut n = 0usize;
    let mut collision = false;
    for &c in endow {
        slots[n] = Some(c);
        n += 1;
    }
    if let Some(caps) = manifest_caps.as_ref() {
        for &(slot, cap) in caps.iter().flatten() {
            // Ein belegter Slot ist **fail-closed**: der Lader hat etwas dorthin delegiert, wo das
            // Manifest etwas anderes hinlegen will. Welches von beiden das Programm dann meint,
            // kann niemand mehr sagen -- also gar nicht erst starten.
            if slots[..n].iter().flatten().any(|&(s, _)| s == slot) || n == slots.len() {
                collision = true;
                break;
            }
            slots[n] = Some((slot, cap));
            n += 1;
        }
    }
    // **Hier und nur hier ist die Herkunft bekannt** (2026-08-25): `endow` IST das Angebot des
    // Aufrufers (Konvention L2, hoechstens ein Cap in Slot 0), `manifest_caps` sind die Zusagen
    // des signierten Dokuments. Eine Ebene tiefer sind beide dasselbe Tupel und nicht mehr
    // auseinanderzuhalten -- deshalb wandert die Auskunft mit, statt dort erraten zu werden.
    let angebot = endow
        .iter()
        .fold(0u16, |m, &(slot, _)| m | if slot < 16 { 1u16 << slot } else { 0 });
    let do_load = |endow: &[(usize, CapPtr)]| -> Option<usize> {
        let archive = read_archive()?;
        let prog = archive.program(index as usize)?;
        // Dieselbe Quelle wie beim Root-Task (D5): die Groesse der **Startmenge**, nicht die des
        // Behaelters. Zwei Bedeutungen fuer dieselbe Zahl waeren schlimmer als eine ungenaue.
        // Ohne Manifest bleibt es beim Archiv -- dann gibt es kein Autoritaetsdokument, das etwas
        // anderes behaupten koennte.
        let arg = boot_arg(
            index as usize,
            read_manifest().map(|m| m.count()).unwrap_or_else(|| archive.count()),
        );
        if let Some((hpd, ep, ntfn)) = hardware_backend {
            let tid = load_program_into_pd_auf(&prog, hpd, endow, arg, heimatkern, angebot, cap_budget).ok()?;
            // Festhalten, WAS da laeuft -- nicht, was es tut. Ohne diese vier Zahlen liesse sich
            // ein Dienst nicht austauschen, ohne ihn zu kennen.
            set_driver_service(DriverService { ep, ntfn, pd: hpd, tid, index, program_id: prog.program_id });
            return Some(hpd);
        }
        // **Der GRUND eines Fehlschlags wird genannt** (2026-08-10). Bis dahin ging er hier mit
        // `.ok()` verloren: `SYS_LOAD` gab dem Aufrufer `ERR_BADCAP`, `init` setzte ein Bit, und
        // was wirklich schiefging (Hash? Zertifikat? Ressourcen? Politik?) stand nirgends. Genau
        // die Form, die dieses Projekt beim Manifest-Format gerade erst behoben hat -- abgewiesen
        // wird richtig, gesagt wird nichts.
        match load_image_auf(&prog, endow, arg, heimatkern, angebot, cap_budget) {
            Ok((tid, pd)) => {
                // **Erst hier wird ein Dienst ohne Geraet auffindbar** (2026-08-25). Bis heute
                // rief nur der HardwareLand-Zweig `set_driver_service`; eine PD ohne Geraet stand
                // in keiner Registrierung, und ein Client, der sie mit `service_id` benannte,
                // bekam `None` -- und danach einen frischen, unverbundenen Endpoint. Beide Seiten
                // haetten einen Kanal gehabt und keinen gemeinsamen, mit gueltigen Caps und ohne
                // eine einzige Fehlermeldung.
                if let Some((ep, ntfn)) = dienst_kanal {
                    set_driver_service(DriverService {
                        ep,
                        ntfn,
                        pd,
                        tid,
                        index,
                        program_id: prog.program_id,
                    });
                }
                Some(pd)
            }
            Err(e) => {
                // **`NoResources` benennt jetzt die Ressource.** Ein Sammelbegriff im Fehlerwert
                // ist die Pruefer-Krankheit eine Ebene tiefer: ein Lader, der „NoResources" sagt,
                // ist ein Pruefer, der „FAIL" sagt. Dazu die angeforderte Groesse UND der freie
                // Rest -- ohne den sagt „n Byte angefordert" nicht, ob der Speicher knapp oder der
                // Pool der falsche war.
                let (_code, _bytes, _frei) = crate::system::lade_mangel();
                #[cfg(feature = "debug-log")]
                println!(
                    "loader  : SYS_LOAD fehlgeschlagen -- Index {index}, program_id {}, Grund {:?}",
                    prog.program_id, e
                );
                if e == LoaderError::NoResources {
                    #[cfg(feature = "debug-log")]
                    println!(
                        "loader  :   fehlende Ressource: {} (Code {_code}); angefordert {_bytes} Byte, \
                         freier Rest {_frei} Byte",
                        crate::system::mangel_name(_code)
                    );
                }
                None
            }
        }
    };
    // Verdichten — mit dem ersten echten Cap als Füllwert, weil `CapPtr` bewusst keinen
    // öffentlichen Konstruktor hat (ein fabrizierbarer Cap-Handle wäre eine Einladung). Ohne einen
    // einzigen Cap gibt es nichts zu verdichten und nichts aufzuräumen.
    let Some(first) = slots.iter().flatten().next().copied() else {
        return do_load(&[]);
    };
    // **Dieselbe Schranke wie `slots`**, und nicht noch einmal hingeschrieben: hier stand eine 8,
    // waehrend `slots` hergeleitet war. Siehe [`LOAD_CAPS_MAX`] fuer den Preis.
    let mut dense = [first; LOAD_CAPS_MAX];
    let mut dense_len = 0usize;
    for &c in slots[..n].iter().flatten() {
        dense[dense_len] = c;
        dense_len += 1;
    }
    let endow: &[(usize, CapPtr)] = &dense[..dense_len];
    let pd = if collision { None } else { do_load(endow) };
    if pd.is_none() {
        // Laden fehlgeschlagen (Archiv fehlt / Index ungueltig / verify/parse/Ressourcen) -> die vom
        // Syscall-Dispatch erzeugten Endowment-Cap-KOPIEN wurden NICHT installiert (load_into_pd endowt
        // erst nach vollem Erfolg). Sie sind frische CDT-Blaetter + NICHT die letzte Referenz (das
        // Original im Aufrufer-Cspace lebt) -> delete_leaf senkt nur den Refcount. Ohne dieses Cleanup
        // lecken sie als verwaiste CDT-Kinder und blockieren sogar `delete` des Eltern-Caps (HasChildren).
        for &(_, cap) in endow {
            let _ = crate::system::cap_delete(cap);
        }
    }
    pd
}
