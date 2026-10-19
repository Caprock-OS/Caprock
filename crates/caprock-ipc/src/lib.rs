#![no_std]
#![forbid(unsafe_code)]
//! Synchrone IPC über Endpoints + asynchrone Notifications (ADR 0004).
//!
//! Ein **Endpoint** ist ein Rendezvous-Punkt: Sender (Aufrufer) und Empfänger
//! (Server) treffen sich, Nachrichten-Register werden zwischen ihren TrapFrames
//! kopiert. Trifft kein Partner ein, blockiert der Thread; beim Rendezvous wird
//! über die [`SchedOps`] der Partner geweckt (kern-übergreifend per IPI) bzw. — auf
//! demselben Kern — direkt umgeschaltet.
//!
//! **Feinkörniges Locking:** Jedes [`Endpoint`]/[`Notification`] ist ein
//! eigenständiges Objekt; der Kernel hält je Objekt einen eigenen Lock
//! (`Slab<SpinLock<Endpoint>>`, seit A-3.4 Teil 4 beim Boot dimensioniert statt
//! `[SpinLock<Endpoint>; 32]` im `.bss`). IPC auf verschiedenen Endpoints läuft daher
//! parallel — nur die kurze Cap-Auflösung serialisiert (separater `CAPS`-Lock im
//! Kernel). Die Methoden hier operieren jeweils auf **einem** Objekt (`&mut self`)
//! ohne eigenes Locking; das Locking + die Sperrordnung besorgt der Kernel/Dispatch.
//!
//! Reine Orchestrierung über [`SchedOps`] + Frame-Accessoren — **kein eigenes
//! `unsafe`** (der Frame-Zugriff ist in der HAL gekapselt).

use caprock_abi::{reg, result, MSG_WORDS};
use caprock_hal::exception::{frame_reg, frame_set_reg};
use caprock_sched::{SchedOps, ThreadId};

/// **Warteschlangen-Tiefe je Endpoint** — so viele Sender bzw. Empfänger können an
/// *einem* Endpoint zugleich blockieren.
///
/// Diese Zahl ist von A-3.4 **nicht** angefasst worden und bleibt eine Compile-Zeit-Grenze:
/// Sie steckt in jedem `Endpoint` (zwei `TidQueue`), wächst also mit der Endpoint-Zahl
/// multiplikativ.
///
/// **Der Überlauf ist seit D11 benannt statt still** ([`result::ERR_EP_FULL`]): der 33.
/// gleichzeitige Sender wird abgewiesen und läuft weiter, statt blockiert und vergessen zu
/// werden. Eine Kapazität ohne benannten Überlauf ist kein Schutz, sondern ein Loch — wer die
/// Schranke einführt, muss sagen, was jenseits von ihr passiert.
pub const QUEUE_CAP: usize = 32;
const QCAP: usize = QUEUE_CAP;

/// FIFO-Warteschlange blockierter Threads (an einem Endpoint).
#[derive(Clone, Copy)]
struct TidQueue {
    buf: [Option<ThreadId>; QCAP],
    head: usize,
    tail: usize,
    count: usize,
}

impl TidQueue {
    const EMPTY: TidQueue = TidQueue {
        buf: [None; QCAP],
        head: 0,
        tail: 0,
        count: 0,
    };

    /// Einreihen. Gibt `false`, wenn die Warteschlange **voll** ist — und dieser Rückgabewert
    /// ist der ganze Punkt von D11.
    ///
    /// Vorher war das ein `if cap { … }` ohne `else`: der Aufrufer erfuhr nichts, blockierte
    /// den Thread trotzdem, und der stand danach in keiner Struktur dieses Endpoints. Weder
    /// `audit` noch `purge_thread` noch `quiescence_of` konnten ihn sehen — ein dauerhaft
    /// hängender Faden, über den jeder Prüfer „in Ordnung" meldete.
    ///
    /// **Jede** Aufrufstelle muss den Wert auswerten. Die beiden, an denen er strukturell nicht
    /// `false` werden kann ([`remove`](Self::remove), [`Endpoint::rebind_server`]), sagen dort,
    /// warum — nicht „das wird schon".
    #[must_use = "ein verworfenes Einreihen laesst einen Thread haengen -- genau D11"]
    fn enqueue(&mut self, t: ThreadId) -> bool {
        if self.count >= QCAP {
            return false;
        }
        self.buf[self.tail] = Some(t);
        self.tail = (self.tail + 1) % QCAP;
        self.count += 1;
        true
    }

    /// Ist kein Platz mehr? Vorabfrage für die Aufrufer, die **vor** dem Blockieren entscheiden
    /// müssen (`call`/`recv`) oder vor einem Zustandswechsel, den sie sonst nicht zurücknehmen
    /// könnten ([`Endpoint::migrate_owner`]).
    fn is_full(&self) -> bool {
        self.count >= QCAP
    }

    fn dequeue(&mut self) -> Option<ThreadId> {
        if self.count == 0 {
            return None;
        }
        let t = self.buf[self.head].take();
        self.head = (self.head + 1) % QCAP;
        self.count -= 1;
        t
    }

    /// Iterator über die belegten Einträge (in FIFO-Reihenfolge, für Audits).
    fn for_each(&self, mut f: impl FnMut(ThreadId)) {
        let mut i = self.head;
        for _ in 0..self.count {
            if let Some(t) = self.buf[i] {
                f(t);
            }
            i = (i + 1) % QCAP;
        }
    }

    /// Audit: existiert ein **toter** Eintrag (Prädikat `live` liefert false)?
    fn any_dead(&self, live: &mut dyn FnMut(ThreadId) -> bool) -> bool {
        let mut dead = false;
        self.for_each(|t| {
            if !live(t) {
                dead = true;
            }
        });
        dead
    }

    /// Audit: kommt ein Thread mehrfach vor (Duplikat = Ready-/Queue-Korruption)?
    fn has_dup(&self) -> bool {
        let mut seen: [Option<ThreadId>; QCAP] = [None; QCAP];
        let mut n = 0;
        let mut dup = false;
        self.for_each(|t| {
            if seen[..n].contains(&Some(t)) {
                dup = true;
            }
            seen[n] = Some(t);
            n += 1;
        });
        dup
    }

    /// Steht dieser Thread in der Warteschlange? (Ruhepunkt-Abfrage, A-4.2.)
    fn contains(&self, target: ThreadId) -> bool {
        let mut found = false;
        self.for_each(|t| {
            if t == target {
                found = true;
            }
        });
        found
    }

    /// Ist die Warteschlange leer? (Ruhepunkt-Abfrage für den Endpoint als Ganzes.)
    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Einen bestimmten Thread aus der Warteschlange entfernen (für Hot-Reload:
    /// einen blockierten Empfänger zurückziehen). Gibt `true`, falls gefunden.
    fn remove(&mut self, target: ThreadId) -> bool {
        let n = self.count;
        let mut found = false;
        for _ in 0..n {
            if let Some(t) = self.dequeue() {
                if t == target {
                    found = true;
                } else {
                    // Kann strukturell nicht scheitern und ist nicht bloss unwahrscheinlich:
                    // die Schleife nimmt `n` Einträge heraus und legt höchstens `n` zurück,
                    // jedes `enqueue` steht hinter genau einem `dequeue`. Der Füllstand ist
                    // hier also nie grösser als beim Eintritt, und der war <= QCAP.
                    let _ = self.enqueue(t);
                }
            }
        }
        found
    }
}

/// Nachrichten-Register (Datenwörter + Tag) von `src` nach `dst` kopieren.
fn transfer(src: usize, dst: usize) {
    for i in 0..MSG_WORDS {
        frame_set_reg(dst, reg::MSG0 + i, frame_reg(src, reg::MSG0 + i));
    }
    frame_set_reg(dst, reg::TAG, frame_reg(src, reg::TAG));
}

/// **Ruhepunkt-Befund (A-4.2).** Wieso ein Thread an einem Endpoint *nicht* ruht —
/// aufgeschlüsselt nach den vier Rollen, die er dort haben kann, statt zu einem Bit
/// vermengt.
///
/// Die Aufschlüsselung ist nicht Bequemlichkeit: die vier Rollen verlangen
/// **verschiedene** Antworten. `as_reply_owner` heisst „schuldet eine Antwort" — darauf
/// wartet man, oder man migriert sie ([`Endpoint::migrate_owner`]). `as_receiver` heisst
/// „wartet auf Arbeit" — den zieht man einfach zurück ([`Endpoint::retire_receiver`]).
/// `as_sender`/`as_caller` heisst, der Thread ist *Client* an diesem Endpoint, nicht
/// Server; ihn stillzulegen ist eine andere Entscheidung als einen Server auszutauschen.
/// Ein Sammelbit „nicht ruhig" liesse den Aufrufer raten, welcher der vier Fälle vorliegt —
/// derselbe Fehler wie das alte `kernelseite=0` im Farbtest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Quiescence {
    /// Blockierter Sender: hat `CALL` abgesetzt, noch kein Server hat ihn übernommen.
    pub as_sender: bool,
    /// Blockierter Empfänger: wartet in `RECV` auf einen Aufrufer.
    pub as_receiver: bool,
    /// Wartet als Aufrufer auf eine Antwort (Reply-Token zeigt auf ihn).
    pub as_caller: bool,
    /// Schuldet als Server eine Antwort (Reply-Owner).
    pub as_reply_owner: bool,
}

impl Quiescence {
    /// Ruht dieser Thread an diesem Endpoint — steht er in **keiner** der vier Rollen?
    pub fn is_quiescent(&self) -> bool {
        !(self.as_sender || self.as_receiver || self.as_caller || self.as_reply_owner)
    }

    /// Zwei Befunde (verschiedene Endpoints) zu einem verschmelzen — für die
    /// systemweite Frage „ruht dieser Thread überall?" (Z4a: Thread einfrieren).
    pub fn merge(self, other: Quiescence) -> Quiescence {
        Quiescence {
            as_sender: self.as_sender || other.as_sender,
            as_receiver: self.as_receiver || other.as_receiver,
            as_caller: self.as_caller || other.as_caller,
            as_reply_owner: self.as_reply_owner || other.as_reply_owner,
        }
    }
}

/// **One of the four roles**, singular — the counterpart to [`Quiescence`], which answers "which
/// roles does *this* thread hold" for one thread at a time.
///
/// The two are not redundant. `Quiescence` is asked **per thread** and is the right shape for "may
/// this thread be frozen"; `Role` comes back from [`Endpoint::occupants`], which asks the opposite
/// question — **who is here at all** — and that one cannot be answered by asking about a thread you
/// would have to name first. Z4d stage 1 needs exactly that direction: an endpoint that migrates
/// must not leave a participant behind, and the participant is by definition someone the checkpoint
/// never listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Blocked sender: `CALL` issued, no server has taken it.
    Sender,
    /// Blocked receiver: waiting in `RECV` — or, at a notification, in `WAIT`.
    Receiver,
    /// Waiting as a caller for a reply; the reply token points at him.
    Caller,
    /// Owes a reply as a server.
    ReplyOwner,
}

/// **Warum ein Umbinden nicht stattfinden konnte** (A-4.1) — aufgeschlüsselt, nicht zu
/// einem Bit vermengt. [`Endpoint::is_idle`] beantwortet „ruht der Endpoint?" mit ja/nein;
/// für einen abgewiesenen Austausch ist das zu wenig, weil die drei Fälle **verschieden**
/// zu behandeln sind: wartende Sender lösen sich von selbst auf, sobald die neue Instanz
/// empfängt (warten genügt), ein offenes Reply-Token nicht — dort schuldet die alte Instanz
/// eine Antwort, und wer sie mitten im Austausch verliert, lässt einen Client hängen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RebindBlocked {
    /// Blockierte Aufrufer stehen an: ihre Nachricht ist abgesetzt, ein Server fehlt.
    pub senders_waiting: bool,
    /// Ein **anderer** Empfänger als die abzulösende Instanz wartet hier.
    pub other_receiver: bool,
    /// Ein Reply-Token ist offen — jemand wartet auf eine Antwort der alten Instanz.
    pub reply_open: bool,
}

/// Ergebnis von [`Endpoint::rebind_server`]. Jeder Ausgang ist benannt: ein `false` liesse
/// den Aufrufer raten, ob er warten (Sender stehen an), abbrechen (nicht stillgelegt) oder
/// einen anderen Weg nehmen soll (die alte Instanz ist gar nicht gebunden).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rebind {
    /// Getauscht: `old` ist nicht mehr Empfänger, `new` ist es.
    ///
    /// `overlapped` unterscheidet die zwei Wege dorthin, und der Unterschied ist der Kern
    /// von A-4.1: bei `true` war `new` **schon vor** dem Aufruf gebunden (beide Instanzen
    /// standen kurz gleichzeitig bereit), der Endpoint hatte also zu **keinem** Zeitpunkt
    /// null Empfänger — auch nicht innerhalb dieser Operation. Bei `false` wurde `new` hier
    /// eingereiht; dann steht der Aufrufer dafür ein, dass `new` blockiert geparkt ist
    /// (siehe [`bind_receiver`](Self::bind_receiver)). Beides ist zulässig, aber nur das
    /// erste ist die starke Zusicherung — deshalb wird es gemeldet und nicht verschwiegen.
    Done { overlapped: bool },
    /// Der Endpoint ist nicht belegt — es gibt nichts umzubinden.
    NoEndpoint,
    /// **Nicht stillgelegt.** Ohne [`Endpoint::begin_quiesce`] wäre jede Vorbedingung nur
    /// eine Momentaufnahme: ein `CALL` auf einem anderen Kern macht sie falsch, bevor der
    /// Tausch geschieht. Deshalb ist das eine Abweisung und keine Warnung.
    NotQuiescing,
    /// Es ist noch etwas offen; welches der drei Dinge, steht im Befund.
    Blocked(RebindBlocked),
    /// Die alte Instanz ist hier nicht als Empfänger geparkt — sie läuft noch, ist tot, oder
    /// war nie gebunden. Umbinden würde dann etwas ablösen, das nicht da ist.
    NotReceiver,
    /// `old` und `new` sind derselbe Thread. Kein Fehler im Zustand, aber ein Fehler im
    /// Aufruf: die Operation hätte nichts zu tun und meldete trotzdem Erfolg — ein
    /// Austausch, der keiner war, sähe von aussen aus wie ein gelungener.
    SameThread,
}

impl Rebind {
    /// Ist der Austausch geschehen?
    pub fn is_done(&self) -> bool {
        matches!(self, Rebind::Done { .. })
    }
    /// Ist er **ohne** ein Fenster ohne Empfänger geschehen (die starke Zusicherung)?
    pub fn is_overlapped(&self) -> bool {
        matches!(self, Rebind::Done { overlapped: true })
    }
}

/// Ein Endpoint-Objekt. Der Kernel hält je Endpoint einen eigenen Lock; die
/// Methoden operieren auf genau diesem einen Objekt.
#[derive(Clone, Copy)]
pub struct Endpoint {
    used: bool,
    /// **Stillgelegt (A-4.2):** solange gesetzt, wird keine *neue* Transaktion eröffnet —
    /// `CALL` und `RECV` scheitern mit `ERR_QUIESCING`, `REPLY` bleibt erlaubt. Damit
    /// existiert ein Zustand, in dem die Menge der offenen Transaktionen nur noch
    /// schrumpfen kann; ohne ihn wäre jede Ruhe-Auskunft in dem Moment veraltet, in dem
    /// sie zurückkommt (ein `CALL` auf einem anderen Kern genügt).
    quiescing: bool,
    /// Blockierte Aufrufer, die auf einen Empfänger warten.
    senders: TidQueue,
    /// Blockierte Empfänger, die auf einen Aufrufer warten.
    receivers: TidQueue,
    /// Der zuletzt empfangene Aufrufer, der auf eine Antwort wartet (das **Reply-Token**:
    /// genau ein gültiger Antwort-Empfänger je Rendezvous, beim REPLY automatisch
    /// invalidiert).
    caller: Option<ThreadId>,
    /// Der **Reply-Owner**: der Server-Thread, der `caller` empfangen hat und ihm eine
    /// Antwort schuldet. Stirbt dieser Thread (KILL/EXIT/Fault) vor dem REPLY, wird der
    /// `caller` mit `ERR_SERVER_GONE` entblockt (Liveness — kein dauerhaftes Hängen).
    /// Stets gemeinsam mit `caller` gesetzt/gelöscht.
    reply_owner: Option<ThreadId>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Endpoint {
    pub const EMPTY: Endpoint = Endpoint {
        used: false,
        quiescing: false,
        senders: TidQueue::EMPTY,
        receivers: TidQueue::EMPTY,
        caller: None,
        reply_owner: None,
    };

    /// Ist dieses Objekt belegt (von `create` reserviert)?
    pub fn is_used(&self) -> bool {
        self.used
    }
    /// Als belegt markieren (von der Slot-Reservierung des Kernels).
    pub fn mark_used(&mut self) {
        *self = Endpoint {
            used: true,
            ..Endpoint::EMPTY
        };
    }

    /// Der aktuell auf eine Antwort wartende Aufrufer (für den Cap-Transfer bei
    /// `REPLY`).
    pub fn caller(&self) -> Option<ThreadId> {
        self.caller
    }

    /// **Der PARTNER eines Threads an diesem Endpoint** (Z23/S2) — wer haelt ihn fest?
    ///
    /// `Quiescence` sagt, in welcher der vier Rollen ein Thread steht; es sagt **nicht, mit wem**.
    /// Fuer einen Prozess-Freeze ist genau das die Auskunft, die zaehlt: „nicht einfrierbar" ist
    /// als Diagnose wertlos, „nicht einfrierbar, weil Thread X auf Server Y wartet" ist eine
    /// Handlungsanweisung.
    ///
    /// | Rolle von `tid` | Partner |
    /// |---|---|
    /// | wartet als Aufrufer auf eine Antwort | der **Reply-Owner** — der Server, der schuldet |
    /// | schuldet als Server eine Antwort | der **Aufrufer**, der wartet |
    /// | steht in `RECV` / in der Senderschlange | **keiner** — es gibt noch kein Gegenüber |
    ///
    /// Die letzte Zeile ist der Punkt, an dem eine bequeme Fassung falsch wuerde: ein wartender
    /// Empfaenger hat **keinen** Partner, und einen zu erfinden waere schlimmer als keiner zu
    /// nennen — man zoege den Falschen zur Rechenschaft.
    ///
    /// **An WEN diese Auskunft gehen darf, entscheidet der Aufrufer, nicht diese Funktion**, und
    /// die Regel steht in `TODO.md` Z23/S2: an den Halter der Freeze-Autoritaet, **nie** an die
    /// eingefrorene PD. Eine Fehlermeldung mit einem fremden Threadnamen darin ist ein Kanal, mit
    /// dem sich die IPC-Topologie anderer PDs ausforschen laesst.
    pub fn partner_of(&self, tid: ThreadId) -> Option<ThreadId> {
        if !self.used {
            return None;
        }
        if self.caller == Some(tid) {
            return self.reply_owner;
        }
        if self.reply_owner == Some(tid) {
            return self.caller;
        }
        None
    }

    /// Einen blockierten Empfänger zurückziehen (Hot-Reload). Der Thread bleibt
    /// danach blockiert (geparkt). Gibt `true`, falls er Empfänger war.
    pub fn retire_receiver(&mut self, tid: ThreadId) -> bool {
        self.used && self.receivers.remove(tid)
    }

    // -- A-4.2: der ruhende Punkt --------------------------------------------------

    /// **Stilllegen beginnen.** Ab jetzt wird keine neue Transaktion mehr eröffnet
    /// (`CALL`/`RECV` -> `ERR_QUIESCING`); laufende dürfen abschliessen. Gibt `true`, falls
    /// der Endpoint dadurch neu stillgelegt wurde, `false`, wenn er es schon war (dann läuft
    /// bereits ein Austausch — der zweite Aufrufer darf ihn nicht für seinen halten) oder der
    /// Endpoint gar nicht belegt ist.
    pub fn begin_quiesce(&mut self) -> bool {
        if !self.used || self.quiescing {
            return false;
        }
        self.quiescing = true;
        true
    }

    /// **Stilllegen beenden** (Austausch fertig oder abgebrochen). Der Endpoint nimmt wieder
    /// neue Transaktionen an. Gibt `true`, falls er stillgelegt war.
    pub fn end_quiesce(&mut self) -> bool {
        let war = self.quiescing;
        self.quiescing = false;
        war
    }

    /// Ist dieser Endpoint gerade stillgelegt?
    pub fn is_quiescing(&self) -> bool {
        self.quiescing
    }

    /// **Wie viele Empfaenger warten hier?** (Z23/S3)
    ///
    /// Der Gruppenschnitt braucht die Unterscheidung „ich bin der EINZIGE Server an diesem Kanal"
    /// von „hier bedient auch jemand anderes". Nur im ersten Fall darf er den Kanal nach innen
    /// zusperren: ein Riegel an einem geteilten Endpoint fröre **Dritte** mit ein, die mit dem
    /// Freeze nichts zu tun haben — genau der Einwand, an dem S1 sich fuer das Subjekt-Gating
    /// entschieden hat.
    pub fn receiver_count(&self) -> usize {
        if self.used {
            self.receivers.count
        } else {
            0
        }
    }

    /// **Wie viele Sender stehen an?** (Z23/S3 — und das ist eine Sprechprobe, keine Auskunft.)
    ///
    /// Die tragende Invariante des Schnitts lautet: *wartet hier ein Empfaenger, ist die
    /// Senderschlange leer* — denn ein Sender und ein Empfaenger treffen sich sofort. Der Schnitt
    /// **fragt das nach**, statt es zu glauben; findet er beides zugleich, weist er ab, statt auf
    /// einer Annahme weiterzubauen, die gerade nachweislich nicht gilt.
    pub fn sender_count(&self) -> usize {
        if self.used {
            self.senders.count
        } else {
            0
        }
    }

    /// **Ruhepunkt-Befund für einen Thread** an diesem Endpoint: in welchen der vier Rollen
    /// steht er noch? Siehe [`Quiescence`].
    ///
    /// Die Antwort ist nur dann mehr als eine Momentaufnahme, wenn der Endpoint
    /// [stillgelegt](Self::begin_quiesce) ist: dann kann die Menge der offenen Transaktionen
    /// nur schrumpfen, ein `is_quiescent()` bleibt also wahr. Ohne Stilllegung darf der
    /// Aufrufer daraus nichts folgern, was über den Moment hinausreicht.
    pub fn quiescence_of(&self, tid: ThreadId) -> Quiescence {
        if !self.used {
            return Quiescence::default();
        }
        Quiescence {
            as_sender: self.senders.contains(tid),
            as_receiver: self.receivers.contains(tid),
            as_caller: self.caller == Some(tid),
            as_reply_owner: self.reply_owner == Some(tid),
        }
    }

    /// **Who stands at this endpoint, and in which role?** (Z4d stage 1)
    ///
    /// The inverse of [`quiescence_of`](Self::quiescence_of), and the direction that could not be
    /// expressed before: that one answers for a thread you can already name — this one enumerates. A
    /// checkpoint that takes an endpoint along has to know **every** participant, and the dangerous
    /// participant is precisely the one nobody wrote down.
    ///
    /// **One entry per role, not per thread.** A thread can hold two roles here at once (a server
    /// that owes a reply and is queued as a sender for its next call); merging them would need a
    /// deduplicating pass, and the needed capacity a full buffer reports would then be a guess.
    /// One entry per role keeps `Err(needed)` **exact**, and it is the shape the caller wants
    /// anyway: a refusal names the role, not just the thread.
    ///
    /// **The overflow is named** (D11): `Err(needed)` and not a truncated list. A silently
    /// shortened list of participants is the worst possible outcome here — it is exactly a
    /// participant left behind, which is the thing this call exists to prevent.
    pub fn occupants(&self, out: &mut [(ThreadId, Role)]) -> Result<usize, usize> {
        if !self.used {
            return Ok(0);
        }
        let mut n = 0usize;
        let mut need = 0usize;
        {
            let mut push = |t: ThreadId, r: Role| {
                need += 1;
                if n < out.len() {
                    out[n] = (t, r);
                    n += 1;
                }
            };
            self.senders.for_each(|t| push(t, Role::Sender));
            self.receivers.for_each(|t| push(t, Role::Receiver));
            if let Some(c) = self.caller {
                push(c, Role::Caller);
            }
            if let Some(o) = self.reply_owner {
                push(o, Role::ReplyOwner);
            }
        }
        if need > out.len() {
            return Err(need);
        }
        Ok(n)
    }

    /// **Die Torentscheidung für eine NEUE Transaktion** (`CALL`/`RECV`), als reine Funktion:
    /// `None` = zulassen, `Some(code)` = mit diesem Ergebniscode abweisen.
    ///
    /// Herausgezogen aus demselben Grund wie `iface_record_or_check` bei A-4.4: der
    /// Abweisungszweig ist über den regulären Pfad nur zu erreichen, wenn gerade ein
    /// Austausch läuft — er bliebe also bis zum ersten echten Hot-Reload ungeprüft, und
    /// ungeprüft heisst: vermutlich kaputt, wenn er zum ersten Mal gebraucht wird. So kann
    /// der Selbsttest ihn direkt füttern, und zwar **dieselbe** Logik, die `call`/`recv`
    /// ausführen — keine Nachbildung, die auseinanderlaufen kann.
    pub fn gate_new_transaction(&self) -> Option<u64> {
        if !self.used {
            Some(result::ERR_BADCAP)
        } else if self.quiescing {
            Some(result::ERR_QUIESCING)
        } else {
            None
        }
    }

    /// **Ruht der Endpoint als Ganzes?** Keine wartenden Sender, keine wartenden Empfänger,
    /// kein offenes Reply-Token. Das ist die Bedingung, unter der ein Austausch der
    /// Server-Instanz *niemanden* trifft — die ehrliche Variante aus A-4.2 („der Austausch
    /// findet nur ohne offene Transaktion statt").
    ///
    /// Wartende **Sender** zählen mit, obwohl ihre Transaktion noch nicht begonnen hat: sie
    /// haben ihre Nachricht bereits abgesetzt und blockieren. Ein Austausch, der sie
    /// übergeht, liesse sie auf einen Server warten, den es nicht mehr gibt.
    pub fn is_idle(&self) -> bool {
        !self.used
            || (self.senders.is_empty()
                && self.receivers.is_empty()
                && self.caller.is_none()
                && self.reply_owner.is_none())
    }

    // -- A-4.1: atomares Umbinden ---------------------------------------------------

    /// **Die Server-Instanz austauschen — Prüfung und Tausch in EINEM Zug** (A-4.1).
    ///
    /// Das ist der ganze Punkt der Operation. Dieselbe Wirkung liesse sich aus
    /// [`retire_receiver`](Self::retire_receiver) und einem `RECV` der neuen Instanz
    /// zusammensetzen — aber dazwischen fällt der Lock, und in genau diesem Fenster hat der
    /// Endpoint **keinen** Empfänger. Ein `CALL` trifft dann keinen leeren Endpoint (das
    /// Objekt lebt weiter, der Aufrufer reiht sich als Sender ein), aber er trifft auch
    /// keinen Server; ob ihn je einer übernimmt, hängt davon ab, ob der Austausch danach
    /// noch gelingt. Hier gibt es dieses Fenster nicht: entweder die alte Instanz ist
    /// gebunden, oder die neue — nie keine von beiden.
    ///
    /// Vorbedingung ist die Stilllegung (A-4.2). Sie ist nicht Ordnungsliebe: ohne sie wäre
    /// der Ruhebefund veraltet, bevor er gelesen ist, und der Tausch geschähe mitten in einer
    /// Transaktion, deren Existenz die Prüfung gerade verneint hat.
    ///
    /// Der wartende **Sender** blockiert den Austausch, obwohl seine Transaktion noch nicht
    /// begonnen hat und die neue Instanz ihn übernehmen könnte. Das ist bewusst streng: die
    /// Zusicherung dieses Schrittes lautet „der Austausch trifft niemanden", nicht „der
    /// Austausch geht meistens gut". Wer Sender übernehmen will, braucht die
    /// Zustandsübergabe aus A-4.3 — sonst bedient v2 eine Nachricht, die für v1 gedacht war.
    pub fn rebind_server(&mut self, old: ThreadId, new: ThreadId) -> Rebind {
        if !self.used {
            return Rebind::NoEndpoint;
        }
        if !self.quiescing {
            return Rebind::NotQuiescing;
        }
        if old == new {
            return Rebind::SameThread;
        }
        // Steht die neue Instanz schon bereit? Dann ist dies der **überlappende** Fall: beide
        // Instanzen sind gebunden, und das Lösen der alten hinterlässt keine Lücke. Das ist
        // der Weg, den der Kernel gehen soll — v2 ruft sein RECV, bevor stillgelegt wird.
        let overlapped = self.receivers.contains(new);
        // Was ist sonst offen? Weder `old` noch (im überlappenden Fall) `new` zählen als
        // fremder Empfänger — sie abzulösen bzw. einzusetzen ist der Zweck des Aufrufs.
        let eigene = usize::from(self.receivers.contains(old)) + usize::from(overlapped);
        let blocked = RebindBlocked {
            senders_waiting: !self.senders.is_empty(),
            other_receiver: self.receivers.count > eigene,
            reply_open: self.caller.is_some() || self.reply_owner.is_some(),
        };
        if blocked != RebindBlocked::default() {
            return Rebind::Blocked(blocked);
        }
        if !self.receivers.remove(old) {
            return Rebind::NotReceiver;
        }
        if !overlapped {
            // Kann strukturell nicht scheitern: `remove(old)` hat gerade einen Eintrag
            // herausgenommen (sonst wären wir oben ausgestiegen), es ist also mindestens ein
            // Platz frei. Der Wert wird trotzdem gelesen — ein `let _` ohne diesen Satz wäre
            // wieder die D11-Form.
            let _ = self.receivers.enqueue(new);
        }
        Rebind::Done { overlapped }
    }

    /// **Eine Empfänger-Instanz binden, ohne dass sie selbst `RECV` ruft.**
    ///
    /// Nötig, weil ein stillgelegter Endpoint genau das verhindert: `RECV` der neuen Instanz
    /// liefe ins geschlossene Tor (`ERR_QUIESCING`). Die Alternative wäre, das Tor vor dem
    /// Start von v2 zu öffnen — dann steht der Endpoint ohne Empfänger da, und jeder `CALL`
    /// in dieser Lücke wartet auf einen Server, dessen Existenz vom Gelingen des restlichen
    /// Austauschs abhängt. Genau diese Lücke soll A-4.1 schliessen.
    ///
    /// **Vorbedingung, für die der Aufrufer einsteht:** `tid` ist blockiert und hat einen
    /// gültigen Frame, in den ein `CALL` seine Nachricht legen darf. Einen *laufenden* Thread
    /// einzureihen, zerstört seinen Zustand — der Rendezvous-Pfad schreibt ihm Register und
    /// weckt ihn, während er anderswo mitten in der Arbeit ist. Der überlappende Weg
    /// ([`rebind_server`](Self::rebind_server) mit bereits gebundenem `new`) braucht diese
    /// Vorbedingung nicht und ist deshalb vorzuziehen.
    ///
    /// Gibt `false`, wenn der Endpoint unbelegt ist, `tid` bereits als Empfänger steht —
    /// ein zweiter Eintrag wäre ein Duplikat in der Queue, also genau das, was `audit` als
    /// Korruption meldet — **oder die Empfänger-Warteschlange voll ist** (D11).
    ///
    /// Der letzte Fall war vorher der schlimmste der drei: die Funktion meldete `true`,
    /// während der Eintrag verworfen wurde. Der Aufrufer (Hot-Reload, A-4.1) hielt die neue
    /// Instanz danach für gebunden, obwohl der Endpoint sie nie gesehen hatte.
    pub fn bind_receiver(&mut self, tid: ThreadId) -> bool {
        if !self.used || self.receivers.contains(tid) {
            return false;
        }
        self.receivers.enqueue(tid)
    }

    /// Einen **sterbenden** Thread aus ALLEN Strukturen dieses Endpoints entfernen
    /// (Sender-/Empfänger-Queue + `caller`). Eager-Cleanup beim Thread-Tod: verhindert,
    /// dass tote TCBs in den festen Queues zurückbleiben (Corpse-Fill -> verdrängte
    /// echte Sender) und dass ein REPLY in einen recycelten Frame schreibt. Gibt `true`,
    /// falls der Thread irgendwo eingetragen war.
    pub fn purge_thread(&mut self, tid: ThreadId) -> bool {
        let mut found = self.senders.remove(tid);
        found |= self.receivers.remove(tid);
        if self.caller == Some(tid) {
            // Der sterbende Thread war selbst der wartende Aufrufer -> Token verfällt.
            self.caller = None;
            self.reply_owner = None;
            found = true;
        }
        found
    }

    /// **Reply-Liveness:** stirbt der `owner` (der Server, der eine Antwort schuldet),
    /// bevor er antwortet, verfällt das Reply-Token und der wartende `caller` muss mit
    /// `ERR_SERVER_GONE` entblockt werden. Gibt diesen `caller` zurück (falls vorhanden)
    /// und löscht caller+owner; sonst `None`. Aus den Todespfaden aufzurufen.
    pub fn owner_died(&mut self, owner: ThreadId) -> Option<ThreadId> {
        if self.reply_owner == Some(owner) {
            self.reply_owner = None;
            self.caller.take()
        } else {
            None
        }
    }

    /// **Reply-Cap-Revocation:** einen konkreten ausstehenden `caller` abbrechen (seine
    /// Reply-Cap wurde gelöscht/revoked). Gibt den `caller` zurück (zum Entblocken mit
    /// `ERR_SERVER_GONE`), falls er noch der wartende Aufrufer ist; sonst `None` (Call
    /// bereits beantwortet/anders -> die Reply-Cap war veraltet, No-Op).
    pub fn abort_call(&mut self, caller: ThreadId) -> Option<ThreadId> {
        if self.caller == Some(caller) {
            self.reply_owner = None;
            self.caller.take()
        } else {
            None
        }
    }

    /// **Reply-Cap-Server-Migration (Hot-Reload):** die ausstehende Antwortpflicht des
    /// `old_owner` (das Reload-Opfer) auf die NÄCHSTE Empfänger-Instanz desselben
    /// Endpoints übertragen, OHNE den wartenden Aufrufer abzubrechen. Der Aufrufer wird
    /// wieder als **Sender** eingereiht (er bleibt blockiert; seine ursprüngliche
    /// Nachricht liegt unverändert in seinem Frame). Die nächste `recv`-Instanz (v2)
    /// übernimmt dieselbe Nachricht und wird zum neuen Reply-Owner — der Call wird so von
    /// v2 abgeschlossen statt mit `ERR_SERVER_GONE` zu sterben (Reply-Cap überlebt den
    /// Server-Wechsel). Gibt `true`, falls eine ausstehende Antwortpflicht des `old_owner`
    /// migriert wurde; sonst `false` (kein passender Reply-Owner, oder die Sender-Queue ist
    /// voll -> No-Op).
    ///
    /// **Die Prüfung auf „voll" steht VOR dem `take()`, und das ist der Unterschied zwischen
    /// einem No-Op und einem verlorenen Thread** (D11). Vorher wurde erst `caller`
    /// herausgenommen und `reply_owner` gelöscht, dann eingereiht — schlug das Einreihen fehl,
    /// war die Antwortpflicht weg *und* der Aufrufer in keiner Struktur mehr: er wartet auf
    /// eine Antwort, die niemand mehr schuldet. Die Funktion meldete dabei `true`.
    ///
    /// Fail-closed heisst hier: die Antwortpflicht bleibt beim alten Besitzer. Der Aufrufer
    /// hängt dann nicht, sondern läuft über den vorhandenen Weg
    /// ([`owner_died`](Self::owner_died) -> `ERR_SERVER_GONE`) auf — eine begonnene
    /// Transaktion, die ehrlich scheitert, statt einer, die lautlos verschwindet.
    pub fn migrate_owner(&mut self, old_owner: ThreadId) -> bool {
        if self.used && self.reply_owner == Some(old_owner) && !self.senders.is_full() {
            if let Some(caller) = self.caller.take() {
                self.reply_owner = None;
                // Kann nach der `is_full`-Vorabfrage nicht scheitern (kein Zwischenschritt
                // füllt die Queue — `&mut self` schliesst einen fremden Zugriff aus).
                let _ = self.senders.enqueue(caller); // erneut zustellbar an die v2-RECV
                return true;
            }
        }
        false
    }

    /// Read-only Audit (Fuzzer-Oracle): prüft beide Queues + `caller` + `reply_owner`
    /// mit einem Lebendigkeits-Prädikat. Gibt `(tote_einträge, duplikat)` zurück. Unter
    /// dem Endpoint-Lock aufzurufen (konsistenter Snapshot).
    pub fn audit(&self, live: &mut dyn FnMut(ThreadId) -> bool) -> (bool, bool) {
        let dead = self.senders.any_dead(live)
            || self.receivers.any_dead(live)
            || self.caller.map_or(false, |c| !live(c))
            || self.reply_owner.map_or(false, |o| !live(o));
        let dup = self.senders.has_dup() || self.receivers.has_dup();
        (dead, dup)
    }

    /// `CALL`: Nachricht senden und auf Antwort warten. Gibt den fortzusetzenden
    /// Frame zurück (immer ein anderer Thread, da der Aufrufer blockiert).
    /// **Kern-übergreifend:** Liegt der Empfänger auf einem anderen Kern, wird die
    /// Nachricht in seinen (blockierten) Frame übertragen und er per `unblock`
    /// (+IPI) geweckt; der Aufrufer blockiert auf seinem Kern. Auf demselben Kern:
    /// Rendezvous-Fastpath (`switch_to`).
    pub fn call(
        &mut self,
        ops: &mut dyn SchedOps,
        core: usize,
        frame: usize,
        badge: u64,
    ) -> usize {
        // A-4.2: stillgelegt -> keine NEUE Transaktion. Der Aufrufer bekommt sofort einen
        // benennbaren Fehler, statt als Sender einzureihen und auf einen Server zu warten,
        // der gerade ausgetauscht wird. Auch ein wartender Empfänger wird dann bewusst NICHT
        // bedient: sonst begänne genau die Transaktion, die der Ruhepunkt ausschliessen soll.
        if let Some(code) = self.gate_new_transaction() {
            frame_set_reg(frame, reg::SYSNO_RESULT, code);
            return frame;
        }
        let caller = ops.current_id(core);
        // **Das Badge des Aufrufers, abgelegt in SEINEM Frame** (2026-08-25).
        //
        // Hier und nicht in der Warteschlange, und das ist die ganze Entwurfsentscheidung. Beim
        // `RECV` ist der Server am Zug; der Dispatch loest **dessen** Badge auf, nicht das des
        // Wartenden. Die naheliegende Fassung -- ein Badge je Warteschlangeneintrag -- kostet
        // `2 * QUEUE_CAP * 8` Byte **je Endpoint**, bei `ENDPOINTS_FOR_ALL_PDS = 10 000` also
        // rund 5 MiB, fuer ein Wort, das schon irgendwo liegt: der Aufrufer blockiert gleich,
        // und sein Frame bleibt bis zum Rendezvous unberuehrt (`reply` schreibt `SYSNO_RESULT`
        // und die Nachrichtenwoerter, nicht dieses Register).
        //
        // **Faelschungssicher**, weil der KERNEL schreibt und der Aufrufer zwischen diesem
        // Schreiben und dem Rendezvous nicht laeuft. Was er vorher selbst in `x1` stehen hatte,
        // war die Endpoint-/Cap-Nummer -- die ist zu diesem Zeitpunkt laengst aufgeloest.
        frame_set_reg(frame, reg::EP_BADGE, badge);
        // Einen *lebenden* wartenden Empfänger suchen. Ein zwischenzeitlich
        // gekillter/beendeter Empfänger hat keinen Frame mehr (`frame_of` == None)
        // und bleibt als toter Eintrag in der Queue zurück; solche Leichen werden
        // verworfen statt zu paniken (sonst Kernel-Panik via `.expect`).
        while let Some(server) = self.receivers.dequeue() {
            let Some(sframe) = ops.frame_of(server) else {
                continue; // toter Empfänger -> Eintrag verwerfen, nächsten versuchen
            };
            transfer(frame, sframe);
            frame_set_reg(sframe, reg::SYSNO_RESULT, result::OK);
            // **Der Server erfaehrt, WELCHES Badge der Aufrufer haelt** (2026-08-25).
            //
            // Bis dahin stand hier `0`, und `reg::EP_BADGE` versprach in der ABI seit jeher
            // „Austritt: Badge des Senders". Ein Server konnte seine Aufrufer damit nicht
            // auseinanderhalten -- wer zwei Clients unterscheiden wollte, brauchte zwei
            // Endpoints. Genau das deckt die Dimensionierung nicht ab
            // (`ENDPOINTS_FOR_ALL_PDS = NPDS`, „ein Endpoint je PD ist die UNTERE Schranke").
            frame_set_reg(sframe, reg::EP_BADGE, badge);
            self.caller = Some(caller);
            self.reply_owner = Some(server); // dieser Server schuldet die Antwort
            // Fastpath nur, wenn der Server **auf diesem Kern** lebt. Seit ext-30 kann er
            // migriert sein, deshalb den Besitzer im Thread-Directory nachschlagen (lock-frei)
            // statt ihn aus der ThreadId abzuleiten.
            return if caprock_sched::owner_core(server) == Some(core) {
                ops.switch_to(core, frame, server) // intra-Kern: direkt zum Server
            } else {
                ops.unblock(server); // anderer Kern: Server dort wecken (+IPI)
                ops.block_current(core, frame) // Aufrufer blockiert, nächster lokaler Thread
            };
        }
        // Kein lebender Empfänger -> als Sender einreihen und blockieren.
        //
        // **D11: erst einreihen, dann blockieren — und nur, wenn das Einreihen gelingt.** Die
        // Reihenfolge ist die ganze Behebung. Vorher lief `block_current` bedingungslos: der
        // 33. Aufrufer wurde blockiert, stand in keiner Warteschlange, hielt kein Token und
        // wurde damit von keinem RECV je erreicht. Wer ihn suchte, fand ihn nicht — `audit`
        // meldete `(false, false)`, `quiescence_of(..).is_quiescent()` meldete ihn als ruhig,
        // und diese Ruhemeldung hätte einen Hot-Reload (A-4.2) freigegeben.
        if !self.senders.enqueue(caller) {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_EP_FULL);
            return frame; // NICHT blockieren: der Aufrufer läuft weiter und kann wiederholen
        }
        ops.block_current(core, frame)
    }

    /// `RECV`: auf einen Aufrufer warten. Gibt den fortzusetzenden Frame zurück
    /// (der eigene, falls sofort ein Sender da war; sonst ein anderer Thread).
    pub fn recv(&mut self, ops: &mut dyn SchedOps, core: usize, frame: usize) -> usize {
        // A-4.2: stillgelegt -> keine NEUE Transaktion. Das trifft die **alte** Instanz, die
        // sonst noch einmal einen wartenden Sender übernähme und damit eine Antwortpflicht
        // aufbaute, die der Austausch gleich wieder wegnehmen müsste. Die neue Instanz ruft
        // ihr erstes RECV nach `end_quiesce` ab — das ist der Sinn der Reihenfolge.
        if let Some(code) = self.gate_new_transaction() {
            frame_set_reg(frame, reg::SYSNO_RESULT, code);
            return frame;
        }
        let server = ops.current_id(core);
        // Einen *lebenden* wartenden Aufrufer suchen. Ein zwischenzeitlich
        // gekillter/beendeter Sender hat keinen Frame mehr (`frame_of` == None) und
        // bleibt als toter Eintrag in der Queue; solche Leichen werden verworfen
        // statt zu paniken (sonst Kernel-Panik via `.expect`).
        while let Some(sender) = self.senders.dequeue() {
            // Aufrufer (ggf. auf anderem Kern, blockiert) -> Nachricht übernehmen.
            // Er bleibt blockiert (wartet auf die Antwort); kein Wecken nötig.
            let Some(cframe) = ops.frame_of(sender) else {
                continue; // toter Sender -> Eintrag verwerfen, nächsten versuchen
            };
            transfer(cframe, frame);
            frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
            // Dieselbe Aussage von der anderen Seite: der Wartende hat sein Badge bei `call` in
            // den eigenen Frame gelegt, und dort steht es noch. **Beide Schreiber der
            // Senderschlange sind `call`-Pfade** -- `call` selbst und `migrate_owner`, das einen
            // Aufrufer nach einem Serverwechsel erneut einreiht; in beiden Faellen hat der Kernel
            // das Wort geschrieben. Gaebe es einen dritten, stuende dort ein Rest.
            frame_set_reg(frame, reg::EP_BADGE, frame_reg(cframe, reg::EP_BADGE));
            self.caller = Some(sender);
            self.reply_owner = Some(server); // dieser Server schuldet die Antwort
            return frame; // Server läuft sofort weiter (kein Wechsel)
        }
        // Kein lebender Aufrufer -> als Empfänger einreihen und blockieren.
        // Dieselbe Zeile, dieselbe Behebung wie im `call`-Zweig (D11): der 33. RECV an
        // demselben Endpoint verschwand ebenso spurlos. Ein Server, dem das zustösst, ist
        // schlimmer als ein Client — er wartet auf Arbeit, die ihm niemand mehr geben kann.
        if !self.receivers.enqueue(server) {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_EP_FULL);
            return frame;
        }
        ops.block_current(core, frame)
    }

    /// `REPLY`: dem zuletzt empfangenen Aufrufer antworten (entblockt ihn, ggf.
    /// kern-übergreifend per `unblock`+IPI). Der Server läuft weiter. Ein etwaiger
    /// Capability-Transfer (`grant`) wird vom Kernel **vor** diesem Aufruf erledigt,
    /// solange noch dieser Endpoint-Lock + der CAPS-Lock gehalten werden.
    pub fn reply(&mut self, ops: &mut dyn SchedOps, core: usize, frame: usize) -> usize {
        if !self.used {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADCAP);
            return frame;
        }
        // Budget-Donation des Servers beenden: der antwortende Server (laufend) gibt das
        // geliehene Konto des Aufrufers frei und läuft wieder auf seinem eigenen Budget.
        ops.end_donation(core);
        // Das Reply-Token einmalig konsumieren (caller + reply_owner); ein zweites
        // REPLY findet `None` -> No-Op (kein Doppel-Reply).
        self.reply_owner = None;
        if let Some(caller) = self.caller.take() {
            // A2-Rest, zweite Partei: wartet der Aufrufer nicht mehr (Frist gefeuert, Timer
            // hat ERR_TIMEOUT geschrieben bzw. schreibt es), wird NICHT in seinen Frame
            // geschrieben und NICHT geweckt -- Form wie ERR_EP_FULL: benannter Ausgang ohne
            // Zustandsaenderung. Sperrordnung EPS<SCHEDS erlaubt die Frage hier.
            if !ops.wartet_auf_ipc(caller) {
            } else if let Some(cframe) = ops.frame_of(caller) {
                transfer(frame, cframe);
                frame_set_reg(cframe, reg::SYSNO_RESULT, result::OK);
                ops.unblock(caller);
            } else {
                ops.unblock(caller);
            }
        }
        frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
        frame
    }
}

// ---------------------------------------------------------------------------
// Notifications: asynchrone Badge-Signale (kein Rendezvous).
// ---------------------------------------------------------------------------

/// Ein Notification-Objekt (eigener Lock je Objekt im Kernel).
#[derive(Clone, Copy)]
pub struct Notification {
    used: bool,
    /// Akkumulierte Badge-Bits noch nicht abgeholter Signale.
    pending: u64,
    /// Blockierter Wartender (Phase: ein Konsument je Notification).
    waiter: Option<ThreadId>,
}

impl Default for Notification {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Notification {
    pub const EMPTY: Notification = Notification {
        used: false,
        pending: 0,
        waiter: None,
    };

    pub fn is_used(&self) -> bool {
        self.used
    }
    /// Den aktuell akkumulierten Badge lesen, **ohne** ihn zu konsumieren (Test-/Loader-Telemetrie:
    /// hat jemand signalisiert?). `WAIT` konsumiert ihn regulär.
    pub fn pending_badge(&self) -> u64 {
        self.pending
    }
    pub fn mark_used(&mut self) {
        *self = Notification {
            used: true,
            ..Notification::EMPTY
        };
    }

    /// Einen **sterbenden** Thread als Wartenden entfernen (eager Cleanup beim Tod):
    /// verhindert einen toten Waiter, in dessen recycelten Frame ein späteres SIGNAL
    /// schreiben würde. Gibt `true`, falls er der Wartende war.
    pub fn purge_thread(&mut self, tid: ThreadId) -> bool {
        if self.waiter == Some(tid) {
            self.waiter = None;
            true
        } else {
            false
        }
    }

    /// Read-only Audit (Fuzzer-Oracle): toter Wartender? Unter dem Notification-Lock
    /// aufzurufen.
    pub fn audit(&self, live: &mut dyn FnMut(ThreadId) -> bool) -> bool {
        self.waiter.map_or(false, |w| !live(w))
    }

    /// **Ruhepunkt-Befund (A-4.2/Z4a):** wartet dieser Thread hier in `WAIT`? Ein Wartender
    /// zählt als **Empfänger** — er hält keine Antwortpflicht (Notifications haben keine),
    /// blockiert aber auf ein Ereignis. Für „ist dieser Thread systemweit ruhig?" gehört er
    /// dazu: ihn einzufrieren, während er auf ein Signal wartet, verliert das Signal nicht
    /// (es akkumuliert in `pending`), aber ihn zu *übersehen* hiesse, ihn für lauffähig zu
    /// halten, obwohl er blockiert.
    pub fn quiescence_of(&self, tid: ThreadId) -> Quiescence {
        Quiescence {
            as_receiver: self.used && self.waiter == Some(tid),
            ..Quiescence::default()
        }
    }

    /// **Who waits here?** (Z4d stage 1) — the notification's [`Endpoint::occupants`].
    ///
    /// At most one, because `waiter` is a single slot (a capacity of one, whose overflow D11 named
    /// as `ERR_EP_FULL`). The signature carries the same `Err(needed)` anyway: a caller that sizes
    /// its buffer from the endpoint case must not silently succeed here on a buffer of zero.
    pub fn occupants(&self, out: &mut [(ThreadId, Role)]) -> Result<usize, usize> {
        match self.waiter {
            Some(w) if self.used => {
                if out.is_empty() {
                    return Err(1);
                }
                out[0] = (w, Role::Receiver);
                Ok(1)
            }
            _ => Ok(0),
        }
    }

    /// `SIGNAL`: `badge` ins Notification-Wort ODERn und einen etwaigen Wartenden
    /// wecken (ggf. kern-übergreifend per `unblock`+IPI). **Nicht blockierend.**
    pub fn signal(&mut self, ops: &mut dyn SchedOps, badge: u64, frame: usize) -> usize {
        if !self.used {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADCAP);
            return frame;
        }
        self.pending |= badge;
        if let Some(w) = self.waiter.take() {
            if let Some(wframe) = ops.frame_of(w) {
                frame_set_reg(wframe, reg::SYSNO_RESULT, result::OK);
                frame_set_reg(wframe, reg::EP_BADGE, self.pending);
                self.pending = 0;
            }
            ops.unblock(w);
        }
        frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
        frame
    }

    /// Wie [`signal`](Self::signal), aber **aus dem Kernel** (kein signalisierender Thread/
    /// Frame) — z. B. Deferred-IRQ-Zustellung (ext-22, P5): ein Geräte-Interrupt wird als
    /// Badge-Signal an den wartenden HardwareLand-Backend zugestellt. Setzt den Badge,
    /// entblockt den Waiter (ggf. kern-übergreifend per `unblock`+IPI); schreibt **keinen**
    /// Signalisierer-Frame. Unter dem Notification-Lock aufzurufen (Sperrordnung NTFNS<SCHEDS).
    pub fn signal_from_kernel(&mut self, ops: &mut dyn SchedOps, badge: u64) {
        if !self.used {
            return;
        }
        self.pending |= badge;
        if let Some(w) = self.waiter.take() {
            if let Some(wframe) = ops.frame_of(w) {
                frame_set_reg(wframe, reg::SYSNO_RESULT, result::OK);
                frame_set_reg(wframe, reg::EP_BADGE, self.pending);
                self.pending = 0;
            }
            ops.unblock(w);
        }
    }

    /// `WAIT`: akkumulierten Badge abholen (sofort, falls vorhanden) oder
    /// blockieren, bis signalisiert wird. Liefert den Badge in `x1`.
    pub fn wait(&mut self, ops: &mut dyn SchedOps, core: usize, frame: usize) -> usize {
        if !self.used {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_BADCAP);
            return frame;
        }
        if self.pending != 0 {
            frame_set_reg(frame, reg::SYSNO_RESULT, result::OK);
            frame_set_reg(frame, reg::EP_BADGE, self.pending);
            self.pending = 0;
            frame
        } else {
            // **D11, zweite Fundstelle — dieselbe Form, anderes Objekt.** `waiter` ist ein
            // einzelner Slot ("ein Konsument je Notification"), also eine Kapazität von 1 —
            // und ihr Überlauf war ebenso unbenannt: ein zweiter `WAIT` ÜBERSCHRIEB den
            // Wartenden. Der Überschriebene blieb blockiert, stand danach in keiner Struktur
            // dieser Notification, und `purge_thread`/`audit`/`quiescence_of` sahen ihn nicht
            // mehr — Faden weg, alle Prüfer still. Dass die Kapazität hier 1 statt 32 ist,
            // ändert nichts an der Struktur des Fehlers.
            if self.waiter.is_some() {
                frame_set_reg(frame, reg::SYSNO_RESULT, result::ERR_EP_FULL);
                return frame;
            }
            self.waiter = Some(ops.current_id(core));
            ops.block_current(core, frame)
        }
    }
}
