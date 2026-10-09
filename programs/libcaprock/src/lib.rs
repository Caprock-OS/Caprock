//! Minimal-Userland-SDK für extern geladene Caprock-EL0-Programme (ext-26).
//!
//! Bietet die **Syscall-Stubs** (SVC-`#0`-ABI: `x0`=nr, `x1`=cap-Index, `x2..x5`=msg, `x6`=tag;
//! Rückgabe `x0`=result, `x1`=badge, `x2..x5`=reply, `x6`=tag) + einen **Panik-Handler**. Bewusst
//! ohne Abhängigkeit vom Kernel-Workspace (die ABI-Konstanten sind dupliziert) — ein extern
//! gebautes Programm hängt nur von diesem SDK ab.

#![no_std]

// Der Testharness braucht `std` — test-only, der PD-Bau sieht es nie (dasselbe Muster wie
// `programs/lxpd-runtime`: `no_std` + `forbid` gibt es dort, hier nur `no_std`).
#[cfg(test)]
extern crate std;

use core::arch::asm;

/// Syscall-Nummern (Spiegel von `caprock_abi::sys`).
pub mod sys {
    pub const YIELD: u64 = 0;
    pub const CALL: u64 = 1;
    pub const RECV: u64 = 2;
    pub const REPLY: u64 = 3;
    pub const PARK: u64 = 5;
    pub const EXIT: u64 = 6;
    pub const KILL: u64 = 7;
    pub const SIGNAL: u64 = 8;
    pub const WAIT: u64 = 9;
    pub const MAP: u64 = 10;
    pub const UNMAP: u64 = 11;
    pub const PDCTL: u64 = 12;
    pub const LOAD: u64 = 13;
    /// **Geprüftes Treiber-Bild laden** (LXPD-Laufzeitpfad) — Spiegel von
    /// `caprock_abi::sys::LOAD_IMAGE` (die EINZIGE Wahrheit steht dort).
    ///
    /// Wie [`LOAD`](Self::LOAD), aber das Bild kommt NICHT aus dem Boot-Archiv, sondern aus
    /// einer Memory-Cap des Aufrufers. Belegung: `x1` = Loader-Cap-Index (WRITE, wie `LOAD`),
    /// `MSG0` = Programm-ID aus dem Boot-Manifest, `MSG1` = Delegationsliste, `MSG2` = Anzahl,
    /// `MSG3` = Ressourcenwunsch (wie `LOAD`), dazu `TAG`: Low-Byte = Slot der Memory-Cap mit
    /// dem geprüften Bild, Bits 8..40 = exakte Bildlänge in Byte, Bits 40..64 = `0` (sonst
    /// Absage — kein stilles Abschneiden). Manifest-Cap braucht es keine: Vertrauen kommt aus
    /// dem Boot-Manifest (Hash-Gleichheit mit dem Eintrag dieser Programm-ID), nicht aus
    /// mitgereichten Bytes. S. [`super::load_image`] + [`super::pack_tag`].
    pub const LOAD_IMAGE: u64 = 36;
    pub const CDELETE: u64 = 14;
    pub const CCOPY: u64 = 15;
    /// **Teilbereich einer Memory-Cap ableiten** (CSUB, Mem-Server-Transport) — Spiegel von
    /// `caprock_abi::sys::CSUB` (die EINZIGE Wahrheit steht dort).
    pub const CSUB: u64 = 37;
    /// **Kanalbeschränkte Kopie einer Endpoint-Cap ableiten** (CHAN) — Spiegel von
    /// `caprock_abi::sys::CHAN` (die EINZIGE Wahrheit steht dort).
    pub const CHAN: u64 = 38;
    pub const CMOVE: u64 = 16;
    pub const SETRECV: u64 = 17;
    /// **Einen zweiten Thread in der EIGENEN PD erzeugen** (K1a/K1b) — s. [`super::spawn`].
    pub const SPAWN: u64 = 20;
    /// **Einen Geraeteinterrupt binden** (Stufe B, B3) — s. [`super::bind_irq`].
    pub const BIND_IRQ: u64 = 26;
    /// **Den eigenen Thread-Pointer setzen** (TLS) — s. [`super::set_tls`].
    pub const SETTLS: u64 = 27;
    /// **Die monotone Uhr lesen** (Stufe A) — s. [`super::clock`].
    pub const CLOCK: u64 = 28;
}

/// Ergebniscodes (Register `x0` beim Austritt; Spiegel von `caprock_abi::result`).
pub mod result {
    pub const OK: u64 = 0;
    /// Kein gültiger Capability an der Stelle / falscher Objekttyp.
    pub const ERR_BADCAP: u64 = 1;
    /// Unbekannte Syscall-Nummer.
    pub const ERR_BADSYS: u64 = 2;
    /// Capability hat nicht die nötigen Rechte.
    pub const ERR_RIGHTS: u64 = 3;
    /// Aufrufer gehört zu keiner Protection Domain.
    pub const ERR_NOPD: u64 = 4;
    /// Antwort-seitiger Liveness-Fehler (Reply-Owner verschwunden).
    pub const ERR_SERVER_GONE: u64 = 5;
    /// Am Cap hängen noch abgeleitete Kopien/Mints — er bleibt unverändert im Slot.
    pub const ERR_HASCHILDREN: u64 = 6;
    /// Kein Platz: Ziel-Slot belegt oder Cap-Budget der PD erschöpft. Bei [`super::spawn`]
    /// zusaetzlich: die Teilregion ueberlappt den Stapel eines lebenden Geschwisterthreads.
    pub const ERR_NOSPACE: u64 = 7;
    /// [`super::spawn`]: die PD haelt bereits so viele Threads, wie der Kernel zusagt.
    pub const ERR_THREAD_LIMIT: u64 = 15;
    /// [`super::spawn`]: die Region liegt in einem Fenster, das ein Geraet per DMA erreicht.
    /// **Das ist ein Angriffsbild, kein Tippfehler** — die Ruecksprungadresse ist Daten.
    pub const ERR_DMA_REACHABLE: u64 = 16;
    /// [`super::spawn`]: die Region taugt nicht als Stapel (zu klein, schief ausgerichtet).
    pub const ERR_BADSTACK: u64 = 17;
    /// [`super::spawn`]/`cdelete`: die Cap ist der Stapel eines lebenden Threads.
    pub const ERR_INUSE: u64 = 18;
    /// [`super::spawn`]: die genannte **Teilregion** liegt nicht innerhalb der Cap. Absichtlich
    /// von [`ERR_BADSTACK`] getrennt: „diese Region taugt nicht" und „du hast eine Region
    /// genannt, die du nicht haeltst" haben verschiedene Behebungen.
    pub const ERR_SUBREGION: u64 = 21;
    /// [`super::load`]: der angeforderte DMA-Pool ist groesser, als eine Geraetezuteilung traegt.
    /// **Abgewiesen, nicht gekuerzt** — s. [`super::DRIVER_DMA_MAX_PAGES`].
    pub const ERR_DMA_TOO_LARGE: u64 = 22;
    /// [`super::bind_irq`]: kein Platz mehr fuer eine Bindung. Die Aussage ist **lokal** —
    /// *dieses Geraet hat keinen freien Vektor* —, und der Aufrufer wird nicht blockiert.
    pub const ERR_IRQ_FULL: u64 = 23;
    /// [`super::set_tls`]: der Zeiger liegt nicht in der unteren Adresshaelfte. Die Schranke
    /// schuetzt den **Kernel** (ein `WRMSR` mit nicht-kanonischem Wert faultet in Ring 0), nicht
    /// den Aufrufer.
    pub const ERR_BADTLS: u64 = 24;
    /// [`super::wait_frist`]: die Frist ist abgelaufen, **ohne** dass das Ereignis eintrat.
    pub const ERR_TIMEOUT: u64 = 25;
}

/// **Wie viele Caps ein [`load`] hoechstens delegieren kann** — Spiegel von
/// `caprock_abi::LOAD_MAX_DELEGATES`.
///
/// Gespiegelt und nicht importiert, aus demselben Grund wie alles andere in dieser Datei: diese
/// Crate wird von **jeder** PD gelinkt und steht deshalb unter MIT/Apache. Eine Abhaengigkeit auf
/// den AGPL-Workspace machte die ABI-Ausnahme gegenstandslos (`docs/grenze.md`).
///
/// **Der Preis ist zwei Gedaechtnisse fuer eine Zahl**, und er ist hier bewusst bezahlt. Laufen
/// sie auseinander, meldet der Kernel `ERR_BADCAP` fuer eine Liste, die der Aufrufer fuer gueltig
/// haelt — sichtbar, aber nicht selbsterklaerend.
pub const LOAD_MAX_DELEGATES: usize = 8;

/// Sub-Operationen für [`sys::PDCTL`] (Register `x2`; Spiegel von `caprock_abi::pdctl`).
pub mod pdctl {
    pub const START: u64 = 0;
    pub const STOP: u64 = 1;
    pub const PAUSE: u64 = 2;
    pub const RESUME: u64 = 3;
    pub const ASSIGN_BUDGET: u64 = 4;
    /// **UserLand-PD erzeugen** — Spiegel von `caprock_abi::pdctl::CREATE`.
    pub const CREATE: u64 = 5;
    /// **Eigenen Speicher in die Ziel-PD mappen** — Spiegel von `caprock_abi::pdctl::MAP_INTO`.
    pub const MAP_INTO: u64 = 6;
    /// **Thread in der Ziel-PD erzeugen** — Spiegel von `caprock_abi::pdctl::SPAWN_INTO`.
    pub const SPAWN_INTO: u64 = 7;
}

/// Syscall-Ergebnis (Register `x0..x6` nach `eret`).
#[derive(Clone, Copy)]
pub struct Ret {
    pub result: u64,
    pub badge: u64,
    pub msg: [u64; 4],
    pub tag: u64,
}

/// Roh-Syscall. `cap` = lokaler Cap-Index der eigenen PD.
///
/// Die ABI ist auf beiden Architekturen **dieselbe** (`x0`=Nummer, `x1`=Cap, `x2..x5`=Nachricht,
/// `x6`=Tag); nur die Träger unterscheiden sich: `svc #0` mit `x0..x6` auf aarch64, `int 0x80` mit
/// `rax/rdi/rsi/rdx/r10/r8/r9` auf x86_64 (Abbildung: `caprock_hal::x86_64::exception::ABI_TO_GPR`).
/// Deshalb sieht ein Programm oberhalb dieser Funktion keinen Unterschied.
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn invoke(nr: u64, cap: u64, msg: [u64; 4], tag: u64) -> Ret {
    let (r0, r1, r2, r3, r4, r5, r6);
    // SAFETY: reiner Supervisor-Call; der Kernel restauriert beim `eret` alle Register aus dem
    // Frame (nur x0..x6 tragen das Ergebnis). Kein Speicher-/Stack-Effekt im User-Kontext.
    unsafe {
        asm!(
            "svc #0",
            inout("x0") nr => r0,
            inout("x1") cap => r1,
            inout("x2") msg[0] => r2,
            inout("x3") msg[1] => r3,
            inout("x4") msg[2] => r4,
            inout("x5") msg[3] => r5,
            inout("x6") tag => r6,
            options(nostack),
        );
    }
    Ret { result: r0, badge: r1, msg: [r2, r3, r4, r5], tag: r6 }
}

/// Roh-Syscall (`int 0x80`) — x86_64. Siehe die aarch64-Fassung für die ABI.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn invoke(nr: u64, cap: u64, msg: [u64; 4], tag: u64) -> Ret {
    let (r0, r1, r2, r3, r4, r5);
    // SAFETY: `int 0x80` ist der für Ring 3 freigegebene Syscall-Vektor (IDT-Gate DPL 3). Der
    // Kernel liest/schreibt ausschließlich die ABI-Register dieses Frames. `r9` (Tag) wird nur
    // gelesen — der Kernel gibt den Antwort-Tag über denselben Weg zurück, aber `asm!` darf `r9`
    // hier nicht als `inout` führen, weil `clobber_abi` es sonst doppelt beansprucht.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") nr => r0,
            inout("rdi") cap => r1,
            inout("rsi") msg[0] => r2,
            inout("rdx") msg[1] => r3,
            inout("r10") msg[2] => r4,
            inout("r8")  msg[3] => r5,
            in("r9") tag,
            clobber_abi("sysv64"),
        );
    }
    Ret { result: r0, badge: r1, msg: [r2, r3, r4, r5], tag: 0 }
}

/// Notification (Cap `cap`) signalisieren (asynchron, nicht blockierend).
///
/// **Achtung, das ist die haeufigste Fehlannahme an dieser ABI:** das Badge, das beim Empfaenger
/// ankommt, ist eine Eigenschaft der **Capability**, nicht dieses Aufrufs. Der Kernel verodert das
/// Badge der benutzten Cap in `pending`; `badge` hier wird **nicht** ausgewertet und bleibt nur als
/// Dokumentation der Absicht stehen. Wer unterscheidbar signalisieren will, muss beim *Vergeben*
/// unterschiedlich badgen (`SYS_LOAD` mit `badge != 0`, s. [`load`]).
pub fn signal(cap: u64, badge: u64) {
    let _ = invoke(sys::SIGNAL, cap, [badge, 0, 0, 0], 0);
}
/// Auf eine Notification (Cap `cap`) warten; gibt das akkumulierte Badge zurück.
pub fn wait(cap: u64) -> u64 {
    invoke(sys::WAIT, cap, [0; 4], 0).badge
}
/// **Warten mit FRIST** (Stufe A / A2) — `(Ergebniscode, Badge)`.
///
/// `ticks` in Timer-Ticks; `0` heisst „ohne Frist" und ist bitgleich zu [`wait`]. Laeuft die Frist
/// ab, ohne dass signalisiert wurde, kommt [`result::ERR_TIMEOUT`] zurueck.
///
/// **`ERR_TIMEOUT` heisst „nichts kam", nicht „das Geraet ist tot".** Wer daraus einen Reset
/// ableitet, waehrend die Antwort gerade zugestellt wurde, baut einen Korruptionspfad -- die
/// Aufloesung des Rennens ist, dass das **Signal gewinnt**: ein Thread, den das Signal geweckt
/// hat, bekommt nie `ERR_TIMEOUT`.
pub fn wait_frist(cap: u64, ticks: u64) -> (u64, u64) {
    let r = invoke(sys::WAIT, cap, [ticks, 0, 0, 0], 0);
    (r.result, r.badge)
}
/// Synchroner Aufruf (Endpoint-Cap `cap`): senden + auf Antwort warten.
pub fn call(cap: u64, msg: [u64; 4]) -> Ret {
    invoke(sys::CALL, cap, msg, 0)
}
/// Aufruf mit explizitem Kanal (Kanal = Low-32 von `x6`): geht nur durch kanalbeschränkte
/// Caps (`CHAN`), deren Regel ihn deckt — sonst [`result::ERR_RIGHTS`], ohne dass der
/// Server je etwas sieht. Unbeschränkte Caps prüfen `x6` nicht; `0` heisst dort nichts.
pub fn call_with_channel(cap: u64, msg: [u64; 4], channel: u32) -> Ret {
    invoke(sys::CALL, cap, msg, u64::from(channel))
}
/// Auf einen Aufruf warten (Server, Endpoint-Cap `cap`).
pub fn recv(cap: u64) -> Ret {
    invoke(sys::RECV, cap, [0; 4], 0)
}
/// Den letzten Aufrufer beantworten (Endpoint-Cap `cap`).
pub fn reply(cap: u64, msg: [u64; 4]) {
    let _ = invoke(sys::REPLY, cap, msg, 0);
}
/// Frame (Memory-Cap `cap`) in die eigene VSpace mappen; gibt den Ergebniscode zurück.
pub fn map(cap: u64) -> u64 {
    invoke(sys::MAP, cap, [0; 4], 0).result
}

/// Ein **Fenster** (Memory-, MMIO- oder DMA-Cap) mappen und erfahren, **was** gemappt wurde:
/// `(Basis, Länge, Gerätesicht)`. `None` bei Fehlschlag.
///
/// Die Gerätesicht (IOVA) ist bei DMA-Caps die Adresse, unter der **das Gerät** die Region sieht;
/// bei MMIO ist sie `0`. Sie ist **nicht** die Basis: die IOVA stammt aus dem Fenster, das der
/// Kernel beim Zuteilen gewählt hat, und liegt oberhalb des RAM. Ein Treiber, der die beiden
/// vermischt, programmiert dem Gerät eine Adresse, die es nicht auflösen kann — oder beschreibt
/// eine, unter der nichts liegt. Deshalb kommen sie hier **getrennt** zurück, auch wenn das
/// umständlicher aussieht als ein Wert.
///
/// **Warum es das gibt** (A-5.1): ein Treiber im Userland hält Caps auf seine Fenster, aber die
/// Adressen kennt er nicht — `map` legt sie an ihre physische Lage, und die steht nirgends im
/// Programm. Der bequeme Weg wäre ein Boot-Info-Block gewesen; der wäre eine Autoritätsquelle
/// neben dem Manifest, die jedes Programm ungefragt lesen kann. Hier beschreibt der Kernel
/// stattdessen genau die Cap, die der Aufrufer ohnehin schon hält — neue Autorität entsteht
/// dabei keine.
pub fn map_window(cap: u64) -> Option<Window> {
    let r = invoke(sys::MAP, cap, [0; 4], 0);
    if r.result != result::OK {
        return None;
    }
    Some(Window { base: r.msg[0], len: r.msg[1], iova: r.msg[2] })
}

/// Ein **gemapptes Fenster** — und die Grenze, innerhalb derer darauf zugegriffen werden darf.
///
/// ## Warum es diesen Typ gibt (A-6.3)
///
/// Ein Programm, das `map_window` bekommt, hält eine Adresse und eine Länge. Um sie zu benutzen,
/// braucht es rohe Zeigerzugriffe — also `unsafe`. Für eine **TrustedSAS**-PD ist das ein
/// Ausschlusskriterium: das Zertifikats-Gate (ADR 0014) verlangt `forbid(unsafe_code)`, und zwar
/// zu Recht — eine Komponente, die ihre Vertrauensstufe behält, muss auditierbar sein.
///
/// Der Ausweg ist **nicht**, die Regel aufzuweichen, sondern das `unsafe` dorthin zu legen, wo es
/// hingehört: in das auditierte SDK, das ohnehin auf der Allowlist steht. Dieser Typ ist die
/// Kapsel dafür. Er entsteht **nur** aus [`map_window`] — also aus Basis und Länge, die der
/// Kernel gerade selbst gemappt hat — und **jeder** Zugriff wird gegen die Länge geprüft. Ein
/// Programm kann ihn nicht fälschen: die Felder sind privat und es gibt keinen Konstruktor.
///
/// Das ist derselbe Gedanke wie bei `Verified` im Manifest-Parser: die Bedingung trägt der Typ,
/// nicht die Disziplin des Aufrufers.
#[derive(Clone, Copy)]
pub struct Window {
    base: u64,
    len: u64,
    iova: u64,
}

impl Window {
    /// Die Adresse, unter der **dieses Programm** das Fenster sieht.
    pub fn base(&self) -> u64 {
        self.base
    }
    /// Die Länge in Bytes.
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Die **Gerätesicht** (IOVA) — `0`, wenn es keine gibt. Sie ist **nicht** die Basis; s.
    /// [`map_window`].
    pub fn iova(&self) -> u64 {
        self.iova
    }

    /// Ob `[off, off+n)` noch im Fenster liegt. Mit geprüfter Addition — eine Bereichsprüfung
    /// hinter einem übergelaufenen Produkt ist eine Attrappe.
    fn passt(&self, off: u64, n: u64) -> bool {
        off.checked_add(n).is_some_and(|e| e <= self.len)
    }

    /// `n` Bytes ab `off` lesen. `None`, wenn das nicht mehr ins Fenster passt — **kein**
    /// gekürzter Slice: ein halber Sektor sieht aus wie ein ganzer und wird als solcher gelesen.
    pub fn bytes(&self, off: u64, n: u64) -> Option<&[u8]> {
        if !self.passt(off, n) {
            return None;
        }
        // SAFETY: `base` und `len` stammen aus `SYS_MAP` — der Kernel hat genau diesen Bereich
        // eben in diese VSpace gemappt. `off + n <= len` ist geprüft. Nur lesend.
        Some(unsafe { core::slice::from_raw_parts((self.base + off) as *const u8, n as usize) })
    }

    /// Ein 64-Bit-Wort ab `off` lesen (little-endian, ungeprüfte Ausrichtung nicht nötig:
    /// `read_unaligned`).
    pub fn read_u64(&self, off: u64) -> Option<u64> {
        if !self.passt(off, 8) {
            return None;
        }
        // SAFETY: wie `bytes`.
        Some(unsafe { core::ptr::read_volatile((self.base + off) as *const u64) })
    }

    /// Ein einzelnes Byte ab `off` schreiben.
    pub fn write_u8(&self, off: u64, v: u8) -> Option<()> {
        if !self.passt(off, 1) {
            return None;
        }
        // SAFETY: wie `bytes`; `off < len` ist geprueft.
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u8, v) };
        Some(())
    }

    /// Ein 16-Bit-Wort ab `off` schreiben (little-endian, byteweise — der Aufrufer darf nicht
    /// annehmen, dass `off` ausgerichtet ist).
    pub fn write_u16(&self, off: u64, v: u16) -> Option<()> {
        self.write_u8(off, v as u8)?;
        self.write_u8(off + 1, (v >> 8) as u8)
    }

    /// Ein 32-Bit-Wort ab `off` schreiben (little-endian, byteweise).
    pub fn write_u32(&self, off: u64, v: u32) -> Option<()> {
        for i in 0..4 {
            self.write_u8(off + i, (v >> (8 * i)) as u8)?;
        }
        Some(())
    }

    /// Ein 64-Bit-Wort ab `off` schreiben. `None`, wenn es nicht mehr ins Fenster passt.
    pub fn write_u64(&self, off: u64, v: u64) -> Option<()> {
        if !self.passt(off, 8) {
            return None;
        }
        // SAFETY: wie `bytes`; das Fenster wurde mit Schreibrecht gemappt, sonst haette
        // `SYS_MAP` es nicht herausgegeben.
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u64, v) };
        Some(())
    }
}
/// Frame (Memory-Cap `cap`) wieder entfernen; gibt den Ergebniscode zurück.
pub fn unmap(cap: u64) -> u64 {
    invoke(sys::UNMAP, cap, [0; 4], 0).result
}
/// Ziel-PD steuern (PdControl-Cap `cap`, Sub-Op `subop`); gibt den Ergebniscode zurück.
pub fn pdctl(cap: u64, subop: u64) -> u64 {
    invoke(sys::PDCTL, cap, [subop, 0, 0, 0], 0).result
}
/// Leere UserLand-PD erzeugen (Loader-Cap `cap`); `budget` 0 = Vorgabe, `dst` freier
/// eigener Slot für die neue PdControl-Cap. Gibt das volle `Ret` zurück: bei `OK` steht
/// die neue PD-Nummer in `msg[0]`. Danach `pd_spawn_into`, `pd_map_into`, dann `pdctl`
/// mit `START` — diese Reihenfolge steht auch im Kernel (`MAP_INTO` braucht den Thread).
pub fn pd_create(cap: u64, budget: u64, dst: u64) -> Ret {
    invoke(sys::PDCTL, cap, [pdctl::CREATE, budget, dst, 0], 0)
}
/// Eigenen Speicher (`mem`, Memory-Cap im eigenen Cspace) in die Ziel-PD mappen
/// (PdControl-Cap `cap`). Gibt das volle `Ret` zurück (Fensterbeschreibung wie `MAP`).
pub fn pd_map_into(cap: u64, mem: u64) -> Ret {
    invoke(sys::PDCTL, cap, [pdctl::MAP_INTO, mem, 0, 0], 0)
}
/// Thread in der Ziel-PD erzeugen (PdControl-Cap `cap`); Stack = ganze `mem`-Region,
/// `entry`/`arg` wie `spawn`, `prio` 0..=255. Gibt das volle `Ret` zurück: bei `OK`
/// steht die rohe Thread-Id in `msg[0]`.
pub fn pd_spawn_into(cap: u64, mem: u64, entry: u64, arg: u64, prio: u64) -> Ret {
    invoke(sys::PDCTL, cap, [pdctl::SPAWN_INTO, mem, entry, arg], prio)
}
/// Programm `index` laden (Loader-Cap `cap`); gibt den Ergebniscode zurück.
///
/// `delegates` sind bis zu [`LOAD_MAX_DELEGATES`] Paare `(eigener Slot, Slot in der neuen PD)`.
/// **Wer je Kind ein eigenes Etikett will, badgt vorher selbst** ([`ccopy`]) und delegiert die
/// Kopie: ein Badge-Argument waere fuer acht Caps ein Parameter mit acht Bedeutungen.
///
/// `cap_budget` ist die Zahl der Cap-Slots, die die neue PD halten darf; `0` = Vorgabe. Ueber das
/// Maximum oder den systemweiten Vorrat hinaus wird **abgewiesen, nicht gedeckelt** — eine
/// stillschweigend gekuerzte PD merkt ihren Mangel erst beim ersten Cap, das nicht mehr passt.
///
/// `dma_pages` ist der DMA-Pool der Geraetezuteilung in Seiten; `0` = Vorgabe. Ueber
/// [`DRIVER_DMA_MAX_PAGES`] hinaus kommt [`result::ERR_DMA_TOO_LARGE`] zurueck — **abgewiesen,
/// nicht gekuerzt**: eine halbierte DMA-Region ist ein Geraet, das ueber ihr Ende hinausschreibt,
/// und dieser Fehler ist still.
pub fn load(
    cap: u64,
    index: u64,
    delegates: &[(u8, u8)],
    cap_budget: u16,
    dma_pages: u32,
) -> u64 {
    if delegates.len() > LOAD_MAX_DELEGATES {
        return result::ERR_BADCAP;
    }
    // Acht Paare, ein Wort. Byte `i` traegt `(Quell-Slot, Ziel-Slot)`; der Kernel liest nur die
    // ersten `len` davon, der Rest bleibt 0 und wird nie angesehen.
    let mut liste = 0u64;
    let mut i = 0;
    while i < delegates.len() {
        let (src, dst) = delegates[i];
        liste |= ((((src & 0x0f) << 4) | (dst & 0x0f)) as u64) << (8 * i);
        i += 1;
    }
    // x5 traegt zwei Felder; die Belegung steht in `caprock_abi::load_extras` und ist hier von
    // Hand gespiegelt (s. `LOAD_MAX_DELEGATES` fuer den Grund).
    let extras = ((dma_pages as u64) << 16) | (cap_budget as u64);
    invoke(sys::LOAD, cap, [index, liste, delegates.len() as u64, extras], 0).result
}

/// **Das `TAG`-Wort von [`sys::LOAD_IMAGE`] packen** — rein, ohne Syscall.
///
/// Layout (Spiegel von `caprock_abi::sys::LOAD_IMAGE`): Low-Byte = Slot der Memory-Cap mit
/// dem geprüften Bild, Bits 8..40 = exakte Bildlänge in Byte, Bits 40..64 = `0`.
///
/// `None` heisst „kein Tag": der Slot passt nicht in ein Byte, die Länge nicht in 32 Bit
/// oder sie ist `0` (ein leeres Bild verifiziert nie — der Kernel wiese es ab, also wird es
/// hier gar nicht erst gebaut). **Lieber Absage als Abschneiden**: Wer `bild_len >> 32`
/// maskierte statt prüfte, lüde ein Präfix und hielte es für das Bild — dieselbe Sorte
/// wie die DMA-Kürzung, gegen die [`load`] mit [`result::ERR_DMA_TOO_LARGE`] steht.
///
/// Die Rückgabe ist `Option` statt `u64`, weil ein reines `u64` die Absage nicht ausdrücken
/// könnte — und eine Absage, die man nicht ausdrücken kann, wird still umgangen.
pub fn pack_tag(bild_slot: u64, bild_len: u64) -> Option<u64> {
    if bild_slot > 0xff {
        return None;
    }
    if bild_len == 0 || bild_len >> 32 != 0 {
        return None;
    }
    Some(bild_slot | (bild_len << 8))
}

/// **Geprüftes Treiber-Bild laden** (LXPD-Laufzeitpfad) — wie [`load`], aber die Bytes kommen
/// aus einer Memory-Cap des Aufrufers statt aus dem Boot-Archiv.
///
/// `loader_slot` = Loader-Cap-Index (WRITE, wie [`load`]), `bild_slot`/`bild_len` = Slot und
/// exakte Länge der Memory-Cap mit dem geprüften Bild (ins [`TAG`](sys::LOAD_IMAGE) gepackt
/// per [`pack_tag`]), `pid` = Programm-ID aus dem Boot-Manifest, `liste`/`anzahl` = gepackte
/// Delegationsliste + Zahl der gültigen Paare (die Packung steckt in `liste`, wie bei
/// [`load`]), `extras` = Ressourcenwunsch (die Belegung steckt in `load_extras`, wie bei
/// [`load`]).
///
/// Client-seitig fail-closed wie [`load`]: passt der Slot/die Länge nicht ins Tag, ist die
/// Länge `0` oder nennt `anzahl` mehr als [`LOAD_MAX_DELEGATES`] Paare, kommt
/// [`result::ERR_BADCAP`] zurück, **ohne dass ein Syscall gebaut wird** — eine Absage, kein
/// gekürzter Anstoss. Sonst das volle [`Ret`]: `result` ist der Ergebniscode, `badge` die
/// neue PD-Id bei `OK` (der Kernel legt sie in `x1` ab, wie bei [`sys::LOAD`]).
pub fn load_image(
    loader_slot: u64,
    bild_slot: u64,
    bild_len: u64,
    pid: u64,
    liste: u64,
    anzahl: u64,
    extras: u64,
) -> Ret {
    let absage = Ret { result: result::ERR_BADCAP, badge: 0, msg: [0; 4], tag: 0 };
    if anzahl > LOAD_MAX_DELEGATES as u64 {
        return absage;
    }
    let tag = match pack_tag(bild_slot, bild_len) {
        Some(t) => t,
        None => return absage,
    };
    invoke(sys::LOAD_IMAGE, loader_slot, [pid, liste, anzahl, extras], tag)
}
/// **Obergrenze des DMA-Pools einer Geraetezuteilung, in Seiten** — Spiegel von
/// `caprock_abi::DRIVER_DMA_MAX_PAGES`. 1024 Seiten = 4 MiB. Darueber weist [`load`] mit
/// [`result::ERR_DMA_TOO_LARGE`] ab, statt zu kuerzen.
pub const DRIVER_DMA_MAX_PAGES: u32 = 1024;

/// **Wie viele Seiten eine Teilregion hoechstens ueberspringen/umfassen kann** — die Felder des
/// `x1`-Wortes von [`spawn`] sind je 32 Bit breit. Gespiegelt, nicht importiert (s.
/// [`LOAD_MAX_DELEGATES`]).
pub const SPAWN_SUB_MAX_PAGES: u64 = 0xffff_ffff;

/// **Einen zweiten Thread in der EIGENEN PD erzeugen** (K1a/K1b).
///
/// `stack_slot` nennt eine `Memory`-Cap des Aufrufers; ihr **Ende** wird der Anfangs-Stackzeiger
/// (Stapel wachsen nach unten). Zurueck kommt `Ok(ThreadId)` oder der Ergebniscode.
///
/// ## Die Teilregion — mehrere Stapel aus EINER Cap
///
/// `offset_pages`/`length_pages` schneiden ein Fenster aus der Cap. **`(0, 0)` heisst „die ganze
/// Region"** und ist bitgleich zu einem Aufruf ohne Teilregion; wer vier Threads auf einer
/// 64-KiB-Arena will, nimmt `(0,4) (4,4) (8,4) (12,4)`.
///
/// Der Grund, warum das nicht Bequemlichkeit ist: eine Cap je Stapel machte „wie viele Threads
/// darf eine PD haben" zu „wie viele Cap-Slots sind noch frei" — und Threads einer PD teilen sich
/// den Adressraum ohnehin, eine eigene Cap je Stapel kauft also **keine Isolation**.
///
/// Ueberlappende Fenster werden abgewiesen ([`result::ERR_NOSPACE`]); ein Fenster ausserhalb der
/// Cap bekommt [`result::ERR_SUBREGION`].
///
/// Der Kernel weist die Loeschung der Stack-Cap ab, solange der Thread lebt
/// ([`result::ERR_INUSE`]) — erst [`kill`](sys::KILL), dann [`cdelete`].
pub fn spawn(
    stack_slot: u64,
    entry: usize,
    arg: usize,
    prio: u8,
    offset_pages: u64,
    length_pages: u64,
) -> Result<u64, u64> {
    if offset_pages > SPAWN_SUB_MAX_PAGES || length_pages > SPAWN_SUB_MAX_PAGES {
        // Sonst schnitte das Packen die oberen Bits ab, und der Kernel bekaeme ein Fenster, das
        // der Aufrufer nie gemeint hat -- eine Schranke, die still umlaeuft, ist keine.
        return Err(result::ERR_SUBREGION);
    }
    let sub = (offset_pages << 32) | length_pages;
    let r = invoke(
        sys::SPAWN,
        sub,
        [stack_slot, entry as u64, arg as u64, prio as u64],
        0,
    );
    if r.result == result::OK {
        Ok(r.msg[0])
    } else {
        Err(r.result)
    }
}

/// **Einen Cap im eigenen Cspace löschen** (Slot `slot`); gibt den Ergebniscode zurück.
///
/// Braucht **kein** Cap: der eigene Cspace ist die Autorität. Ein langlebiger Dienst, der Caps per
/// IPC empfängt, muss sie loswerden können — sonst läuft er gegen sein Cap-Budget, ohne dass ihm
/// jemand etwas entzogen hätte. [`result::ERR_HASCHILDREN`] heißt: es hängen noch abgeleitete Caps
/// daran, der Slot ist **unverändert**.
pub fn cdelete(slot: u64) -> u64 {
    invoke(sys::CDELETE, slot, [0; 4], 0).result
}
/// **Einen Cap im eigenen Cspace kopieren** (A-3.2): `src` → `dst` (muss frei sein), Rechte
/// `rights` (1=R, 2=W, 4=X; wird mit den Rechten des Originals geschnitten), `badge` = Etikett der
/// Kopie (`0` = Badge des Originals erben).
///
/// Bei Notifications/Endpoints ist das Badge der Weg, zwei **unterscheidbare** Kanäle auf dasselbe
/// Objekt zu bekommen — s. [`signal`].
pub fn ccopy(src: u64, dst: u64, rights: u64, badge: u64) -> u64 {
    invoke(sys::CCOPY, src, [dst, rights, badge, 0], 0).result
}
/// IPC permission constants (mirror of `caprock_abi::ipc_perm`; the single source of truth is there).
pub mod ipc_perm {
    pub const SHIFT: u32 = 48;
    pub const ID_MASK: u64 = (1u64 << SHIFT) - 1;
    pub const DUP: u16 = 1 << 15;
    pub const GRANT: u16 = 1 << 14;
    pub const SERVICE_MASK: u16 = 0x3fff;
    pub const ALL: u16 = 0xffff;
    /// Permission mask of a badge word received from `recv`.
    pub const fn perms_of(wire: u64) -> u16 {
        (wire >> SHIFT) as u16
    }
    /// Object id of a badge word received from `recv`.
    pub const fn id_of(wire: u64) -> u64 {
        wire & ID_MASK
    }
}
/// **Derive a tagged IPC capability from an endpoint capability** (server side).
///
/// `src` must hold an endpoint capability, `dst` must be free. The result carries `id` (48 bits,
/// `0` = inherit) and the permission mask `perms & mask(src)` (see [`ipc_perm`]); the kernel
/// delivers both to the server in the badge word of every `CALL` made through it, so a client
/// cannot forge either. Give the client only `WRITE` in `rights` unless it should also receive.
/// `perms` must be non-zero and fit 16 bits.
pub fn ccopy_ipc(src: u64, dst: u64, rights: u64, id: u64, perms: u16) -> u64 {
    if perms == 0 {
        return result::ERR_RIGHTS;
    }
    invoke(sys::CCOPY, src, [dst, rights, id, perms as u64], 0).result
}
/// **Kanalbeschränkte Kopie einer Endpoint-Cap ableiten** (CHAN): `src` muss eine
/// Endpoint-Cap halten, `dst` muss frei sein, `(value & mask) == value` mit `mask != 0`
/// (sonst [`result::ERR_RIGHTS`]). Client-Gatter wie [`ccopy_ipc`]: was der Kernel
/// sicher abweist, kostet keinen Syscall.
pub fn chan(src: u64, dst: u64, value: u32, mask: u32) -> u64 {
    if mask == 0 || (value & !mask) != 0 {
        return result::ERR_RIGHTS;
    }
    invoke(sys::CHAN, src, [dst, u64::from(value), u64::from(mask), 0], 0).result
}
/// **Einen Cap im eigenen Cspace verschieben** (A-3.2): `src` → `dst` (muss frei sein). Keine
/// Ableitung — derselbe Cap, ein anderer Slot.
pub fn cmove(src: u64, dst: u64) -> u64 {
    invoke(sys::CMOVE, src, [dst, 0, 0, 0], 0).result
}
/// **Teilbereich einer Memory-Cap als eigene Cap ableiten** (CSUB): `src` muss eine
/// Memory-Cap halten, `dst` muss frei sein, `[offset, offset+len)` muss in der Region
/// liegen (sonst [`result::ERR_SUBREGION`]). Die Rechte sind der Schnitt mit der Quelle.
/// Client-Gatter wie [`ccopy`]/[`load`]: unpassende Slots/Null-Laenge werden OHNE Syscall
/// mit [`result::ERR_BADCAP`] beantwortet -- ein Aufruf, der die Antwort schon kennt,
/// kostet keinen Fall in den Kernel.
pub fn csub(src: u64, dst: u64, offset: u64, len: u64) -> u64 {
    if len == 0 {
        return result::ERR_BADCAP;
    }
    invoke(sys::CSUB, src, [dst, offset, len, 0], 0).result
}
/// **Empfangs-Slot festlegen** (A-3.2): wo per IPC übertragene Caps landen. Der Empfänger
/// entscheidet das, nicht der Sender.
pub fn setrecv(slot: u64) -> u64 {
    invoke(sys::SETRECV, slot, [0; 4], 0).result
}
/// Thread (Tcb-Cap `cap`) beenden; gibt den Ergebniscode zurück.
pub fn kill(cap: u64) -> u64 {
    invoke(sys::KILL, cap, [0; 4], 0).result
}
/// **Einen Geraeteinterrupt an eine Notification binden** (Stufe B, B3).
///
/// `irq` = Slot der `Irq`-Cap, `ntfn` = Slot der Notification, `badge` = das Etikett, mit dem der
/// Kernel signalisiert. Ergebniscode; [`result::ERR_IRQ_FULL`], wenn kein Platz mehr ist — der
/// Aufrufer wird dabei **nicht** blockiert.
///
/// **Es gibt kein Vektorargument, und das ist der Punkt.** Welcher Interrupt gebunden wird, sagt
/// die Cap; eine Zahl daneben waere eine zweite Antwort auf dieselbe Frage.
pub fn bind_irq(irq: u64, ntfn: u64, badge: u64) -> u64 {
    invoke(sys::BIND_IRQ, irq, [ntfn, badge, 0, 0], 0).result
}
// ================================================================================================
// TLS: den Block aufbauen (T4)
// ================================================================================================
//
// **Hier und nur hier lebt der Unterschied zwischen Variante 1 und 2** — und dass er hier lebt und
// nicht im Kernel, ist die Probe auf den Schnitt aus `docs/plan-tls.md`: der Kernel haelt eine
// Zahl, das Layout ist eine ABI der Werkzeugkette.
//
// | | `tp` zeigt auf | Variablen | TCB |
// |---|---|---|---|
// | x86-64 (Variante 2) | **Ende** des Blocks | negative Offsets, `tp-size .. tp` | 8 B ab `tp`, `tp[0] = tp` |
// | aarch64 (Variante 1) | **Anfang** | ab `tp + 16` | 16 B ab `tp`, **kein** Selbstzeiger |
//
// Der Selbstzeiger auf x86 ist keine Konvention um ihrer selbst willen: Ring 3 kann `FS_BASE`
// ohne `CR4.FSGSBASE` **nicht lesen**, und `fs:[0]` ist der einzige Weg, an den eigenen
// Thread-Pointer zu kommen. Auf aarch64 ist `TPIDR_EL0` aus EL0 direkt lesbar, deshalb gibt es
// dort keinen.

extern "C" {
    /// Anfang des `.tdata`-Anfangsbildes (Linkerskript, T3).
    static __tdata_start: u8;
    /// Ende des Anfangsbildes.
    static __tdata_end: u8;
    /// Ende von `.tbss` — zusammen mit [`__tdata_start`] die **Gesamtgroesse** eines Blocks.
    static __tbss_end: u8;
}

/// **Ausrichtung und Rundung des TLS-Blocks — eine Seite, und das ist gemessen.**
///
/// Der Uebersetzer adressiert eine thread-lokale Variable im local-exec-Modell als
/// `tp - (aufgerundete Blockgroesse - Offset)`, und aufgerundet wird auf **`p_align` des
/// `PT_TLS`** — eine Zahl, die die **Werkzeugkette** setzt und nicht dieses SDK. Gemessen an
/// `virtio-blk`: `p_align = 0x1000`, auch nachdem `.tdata` im Linkerskript auf `ALIGN(16)` stand.
///
/// Mit 16 reservierten Byte lag die Variable deshalb **eine ganze Seite** unter ihrem Speicher:
/// `FAR = 0x2000_2010` bei `tp = 0x2000_3010`. Kein Uebersetzungsfehler, keine Meldung -- der
/// Treiber starb still, und sichtbar wurde es erst, als `el0-trap` ins Protokoll kam (todo D17).
///
/// **Reserviert wird deshalb die Seitengroesse, statt die Zahl parallel zur Wahrheit zu fuehren**
/// (todo D16). Das kostet 4 KiB je Thread und ist der Preis dafuer, dass hier keine zweite Quelle
/// fuer `p_align` entsteht. Verlangt eine Werkzeugkette je mehr, faultet es sichtbar -- nicht
/// still.
pub const TLS_ALIGN: usize = 4096;

/// **Wie viele Bytes ein TLS-Puffer mindestens haben muss.**
///
/// Anfangsbild + `.tbss` + TCB + Spielraum fuer die Ausrichtung. Als Funktion und nicht als
/// Konstante, weil die Groesse aus dem **gelinkten Programm** kommt und nicht aus diesem SDK.
pub fn tls_bedarf() -> usize {
    let anfang = core::ptr::addr_of!(__tdata_start) as usize;
    let ende = core::ptr::addr_of!(__tbss_end) as usize;
    // **Die AUFGERUNDETE Groesse, nicht die rohe** — der Uebersetzer rechnet gegen sie, s.
    // [`TLS_ALIGN`]. Dazu einmal Ausrichtung (der Puffer faengt irgendwo an) und der TCB.
    let gesamt = ende - anfang;
    ((gesamt + TLS_ALIGN - 1) & !(TLS_ALIGN - 1)) + TLS_ALIGN + 16
}

/// **Einen TLS-Block in `puffer` aufbauen und scharfstellen.**
///
/// Gibt den gesetzten Thread-Pointer zurueck, oder `0`, wenn der Puffer zu klein ist oder der
/// Kernel den Zeiger abweist ([`result::ERR_BADTLS`]).
///
/// **Der Puffer gehoert dem Aufrufer, und das ist der Entwurf**: ein Thread, ein Puffer. Wer zwei
/// Threads mit demselben Puffer aufsetzt, hat kein TLS, sondern zwei Namen fuer eine Variable —
/// und der Kernel kann das nicht bemerken, weil er das Layout nicht kennt. Genau dagegen steht die
/// Gegenprobe M3 in `docs/plan-tls.md`.
///
/// # Safety
/// `puffer` muss dem aufrufenden Thread allein gehoeren und ihn ueberleben (`static mut`, nicht
/// Stack). Nach dem Aufruf darf niemand sonst hineinschreiben.
pub unsafe fn tls_einrichten(puffer: *mut u8, len: usize) -> u64 {
    let anfang = core::ptr::addr_of!(__tdata_start) as usize;
    let bild_ende = core::ptr::addr_of!(__tdata_end) as usize;
    let ende = core::ptr::addr_of!(__tbss_end) as usize;
    let (bild, gesamt) = (bild_ende - anfang, ende - anfang);
    if len < gesamt + TLS_ALIGN + 16 {
        return 0;
    }
    // Ausgerichteter Anfang im Puffer.
    let basis = ((puffer as usize) + TLS_ALIGN - 1) & !(TLS_ALIGN - 1);

    #[cfg(target_arch = "x86_64")]
    let (tp, daten) = {
        // **Variante 2: die Daten enden GENAU bei `tp`.**
        //
        // Der Uebersetzer adressiert eine thread-lokale Variable als `tp - (Blockgroesse -
        // Offset)`; bei einer einzigen `u64` also `tp - 8`. Die erste Fassung legte die Daten an
        // `basis` und `tp` auf `basis + align_up(gesamt)` — bei `gesamt = 8` und Ausrichtung 16
        // sind das **acht Byte Versatz**, und die Variable lag neben ihrem Speicher. Kein Absturz,
        // keine Meldung: sie las eine genullte Stelle. Die teurere Sorte.
        //
        // Ausgerichtet wird deshalb `tp`, nicht der Datenanfang.
        // **Der Block ist `align_up(gesamt, TLS_ALIGN)` gross, nicht `gesamt`** -- und das ist
        // keine Vorsicht, sondern die Rechnung des Uebersetzers: er adressiert eine
        // thread-lokale Variable als `tp - (aufgerundete Blockgroesse - Offset)`. Wer nur
        // `gesamt` reserviert, laesst die Variable neben ihrem Speicher liegen; bei
        // `p_align = 4096` faultete sie eine ganze Seite darunter (`FAR = tp - 0x1000`).
        //
        // [`TLS_ALIGN`] MUSS die Ausrichtung von `.tdata` im Linkerskript spiegeln. Zwei Zahlen
        // fuer eine Tatsache -- der Kommentar dort sagt es von der anderen Seite.
        let block = (gesamt + TLS_ALIGN - 1) & !(TLS_ALIGN - 1);
        let tp = basis + block;
        (tp, tp - block)
    };
    #[cfg(target_arch = "aarch64")]
    let (tp, daten) = {
        // **Variante 1: `tp` an den Anfang, 16 B TCB, Daten dahinter — AUSGERICHTET.**
        //
        // Das `align_up` ist kein Zierat: die Variablen liegen bei positiven Offsets ab dem
        // ausgerichteten Blockanfang, nicht ab `tp + 16`. Mit `TLS_ALIGN` = eine Seite faellt
        // beides auseinander, sobald jemand hier TLS benutzt -- und weil aarch64 heute keine
        // thread-lokale Variable hat, wuerde es niemand bemerken. Genau die Sorte Fehler, die
        // dieser Baum schon zweimal auf der jeweils ungefahrenen Architektur bezahlt hat.
        let tp = basis;
        (tp, (tp + 16 + TLS_ALIGN - 1) & !(TLS_ALIGN - 1))
    };

    // SAFETY: der Aufrufer sichert zu, dass `puffer[..len]` ihm allein gehoert; die Schranke oben
    // stellt sicher, dass Bild, `.tbss` und TCB hineinpassen.
    unsafe {
        // **Erst den GANZEN Block nullen, dann das Bild hinein.** Andersherum bliebe der
        // Aufrundungsrest (`block - gesamt`) uninitialisiert -- und genau dort liegen bei
        // Variante 2 die Variablen mit den kleinsten Offsets.
        let block = (gesamt + TLS_ALIGN - 1) & !(TLS_ALIGN - 1);
        let _ = block;
        #[cfg(target_arch = "x86_64")]
        core::ptr::write_bytes(daten as *mut u8, 0, block);
        #[cfg(target_arch = "aarch64")]
        core::ptr::write_bytes(daten as *mut u8, 0, gesamt);
        core::ptr::copy_nonoverlapping(anfang as *const u8, daten as *mut u8, bild);
        #[cfg(target_arch = "x86_64")]
        core::ptr::write_volatile(tp as *mut u64, tp as u64); // Selbstzeiger, s. o.
        #[cfg(target_arch = "aarch64")]
        core::ptr::write_bytes(tp as *mut u8, 0, 16); // TCB genullt
    }
    if set_tls(tp as u64) != result::OK {
        return 0;
    }
    tp as u64
}

/// **Die monotone Uhr lesen** (Stufe A, A1) — `(Rate in Hz, aktueller Stand)`.
///
/// Kein Cap. **Keine Wanduhrzeit, keine Epoche, keine Frist** -- Fristen sind A2 und fassen die
/// Blockade-Invarianten an; dieser Aufruf tut das nicht.
///
/// Die **Rate** ist das, was gefehlt hat: den Zaehler kann Ring 3 auf beiden Architekturen selbst
/// lesen (`rdtsc`, `CNTVCT_EL0`) -- was es nicht wissen kann, ist, was ein Schritt wert ist.
pub fn clock() -> (u64, u64) {
    let r = invoke(sys::CLOCK, 0, [0; 4], 0);
    (r.msg[0], r.msg[1])
}

/// **Den eigenen Thread-Pointer setzen** (TLS). `0` schaltet ihn ab.
///
/// Kein Cap: der Aufruf wirkt nur auf den aufrufenden Thread. Was hinter dem Zeiger liegt, geht
/// den Kernel nichts an — er haelt die Zahl und sorgt dafuer, dass sie den Kontextwechsel
/// ueberlebt.
///
/// **Die Aufteilung des Blocks ist ARCHITEKTURABHAENGIG** und Sache des Aufrufers:
/// x86-64 ist Variante 2 (Zeiger ans **Ende**, Variablen bei negativen Offsets, Selbstzeiger in
/// `tp[0]`), aarch64 ist Variante 1 (Zeiger an den **Anfang**, 16 Byte reservierter TCB,
/// Variablen ab `tp+16`, **kein** Selbstzeiger).
pub fn set_tls(ptr: u64) -> u64 {
    invoke(sys::SETTLS, ptr, [0; 4], 0).result
}
/// Freiwilliger Zeitscheibenabtritt.
pub fn yield_now() {
    let _ = invoke(sys::YIELD, 0, [0; 4], 0);
}
/// Sich selbst dauerhaft blockieren (kein Cap nötig).
pub fn park() -> ! {
    loop {
        let _ = invoke(sys::PARK, 0, [0; 4], 0);
    }
}
/// Sich selbst beenden (Stack/TCB werden zurückgewonnen).
pub fn exit() -> ! {
    let _ = invoke(sys::EXIT, 0, [0; 4], 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Definiert den ELF-Entry-Point `_start` eines extern geladenen Programms.
///
/// Die `#[no_mangle]`-Glue (in aktuellem Rust ein *unsafe* Attribut) gehoert zur auditierten
/// SDK-Schicht (= Allowlist), damit das eigentliche Programm `#![forbid(unsafe_code)]` bleiben und
/// damit zertifiziert werden kann. `$main` ist eine **sichere** `fn(usize) -> !` des Programms (x0 =
/// Boot-Arg). Verwendung:  `libcaprock::entry!(run);`  mit  `fn run(_arg: usize) -> ! { ... }`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "C" fn _start(arg: usize) -> ! {
            $main(arg)
        }
    };
}

// Nur auf freistehenden Zielen (PD-Bau, `programs/*-caprock-user.json` ohne `os`-Feld —
// Default `none`): Dort gibt es kein `std` und damit keinen Panik-Handler. Auf dem Host
// (Host-Tests, `target_os = linux`) linkt der Testharness `std` mit eigenem Handler — ein
// zweiter hier wäre E0152, auch wenn diese Crate nur Dependenz ist (dann gilt `cfg(test)`
// nämlich nicht für sie). Der Handler parkt; der Kernel beobachtet das Ausbleiben.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Kein Heap/Konsole im EL0-Programm verfügbar -> still parken; der Kernel beobachtet, dass
    // kein erwartetes Signal kam (bzw. ein Fault terminiert den Thread regulär).
    park()
}

// --- Host-Tests ---------------------------------------------------------------------------
//
// `asm!` kompiliert auf dem Host (x86_64-Host trifft den `int 0x80`-Zweig), läuft aber nie:
// Diese Tests prüfen NUR die reine [`pack_tag`]-Funktion und den [`sys::LOAD_IMAGE`]-Spiegel,
// nie den Syscall. Wer hier `invoke`/`load_image` riefe, führte auf dem Host `int 0x80` mit
// Kernel-ABI-Registern aus — das ist kein Test, sondern ein Absturz mit Ansage.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_image_spiegel_hat_die_abi_nummer() {
        // EINZIGE Wahrheit: `caprock_abi::sys::LOAD_IMAGE`. Läuft der Spiegel auseinander,
        // stellte jeder Anstoss einen fremden Syscall — sichtbar erst im Kernel-Log.
        assert_eq!(sys::LOAD_IMAGE, 36);
    }

    #[test]
    fn csub_spiegel_hat_die_abi_nummer() {
        // Wie oben, fuer CSUB=37 (s. Doku am Spiegel).
        assert_eq!(sys::CSUB, 37);
    }

    #[test]
    fn chan_spiegel_hat_die_abi_nummer() {
        // Wie oben, fuer CHAN=38 (s. Doku am Spiegel).
        assert_eq!(sys::CHAN, 38);
    }

    #[test]
    fn pdctl_subops_spiegeln_die_abi() {
        // EINZIGE Wahrheit: `caprock_abi::pdctl`. Neue Sub-Operationen landen hier,
        // sonst ruft der Prozessdienst ins Leere.
        assert_eq!(pdctl::CREATE, 5);
        assert_eq!(pdctl::MAP_INTO, 6);
        assert_eq!(pdctl::SPAWN_INTO, 7);
    }

    #[test]
    fn chan_gatter_ohne_syscall() {
        // Client-Gatter: leere/irrefuehrende Regeln kehren ohne `invoke` mit
        // ERR_RIGHTS zurueck — wie dokumentiert wird der Syscall gar nicht erst
        // gestellt (auf dem Host liesse er sich ohnehin nicht stellen).
        assert_eq!(chan(3, 4, 0, 0), result::ERR_RIGHTS);
        assert_eq!(chan(3, 4, 0x1_0000, 0xFFFF), result::ERR_RIGHTS);
    }

    #[test]
    fn csub_null_laenge_ohne_syscall() {
        // Client-Gatter: Laenge 0 kehrt ohne `invoke` mit ERR_BADCAP zurueck -- wie
        // dokumentiert wird der Syscall gar nicht erst gestellt (auf dem Host liesse
        // er sich ohnehin nicht stellen, s. Moduldoku).
        assert_eq!(csub(3, 4, 0, 0), result::ERR_BADCAP);
        assert_eq!(csub(3, 4, 16, 0), result::ERR_BADCAP);
    }

    #[test]
    fn pack_rundweg() {
        // Slot ins Low-Byte, Länge in Bits 8..40, Bits 40..64 null — und zurück.
        let tag = pack_tag(5, 96).expect("passt");
        assert_eq!(tag, 5 | (96 << 8));
        assert_eq!(tag & 0xff, 5, "Low-Byte = Bild-Slot");
        assert_eq!((tag >> 8) & 0xffff_ffff, 96, "Bits 8..40 = exakte Bildlänge");
        assert_eq!(tag >> 40, 0, "Bits 40..64 = 0");
        // Ränder: Slot 0xFF und volle 32-Bit-Länge packen noch.
        let voll = pack_tag(0xff, 0xffff_ffff).expect("Rand passt");
        assert_eq!(voll & 0xff, 0xff);
        assert_eq!((voll >> 8) & 0xffff_ffff, 0xffff_ffff);
        assert_eq!(voll >> 40, 0);
    }

    #[test]
    fn pack_slot_ueber_ein_byte_ist_absage() {
        // Slot 256 bräuchte ein neuntes Bit — maskieren hieße Slot 0 laden statt Slot 256.
        assert_eq!(pack_tag(0x100, 96), None);
        assert_eq!(pack_tag(u64::MAX, 96), None);
    }

    #[test]
    fn pack_high_bits_sind_absage_kein_abschnitt() {
        // Bit 32+ fiele aus Bits 8..40 heraus — abschneiden lüde ein Präfix und hielte es
        // für das Bild. Lieber Absage (`None`) als ein gekürzter Anstoss.
        assert_eq!(pack_tag(5, 1 << 32), None);
        assert_eq!(pack_tag(5, u64::MAX), None);
    }

    #[test]
    fn pack_len_null_ist_absage() {
        // Ein leeres Bild verifiziert nie — der Kernel wiese es ab, also wird das Tag gar
        // nicht erst gebaut (fail-closed client-seitig, wie `load` bei zu vielen Delegaten).
        assert_eq!(pack_tag(5, 0), None);
    }
}
