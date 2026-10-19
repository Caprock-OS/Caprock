#![no_std]
#![forbid(unsafe_code)] // Audit 2026-09-10: reine Zahlen/Typen-Deklaration, kein Grund fuer unsafe.
//! Geteilte Kernel<->Thread-ABI (ADR 0004).
//!
//! Syscalls werden über `SVC #0` ausgelöst; Argumente und Rückgaben liegen in
//! General-Purpose-Registern (register-basierte IPC für niedrige Latenz). Diese
//! Crate definiert die Nummern, das Register-Layout und die Ergebniscodes — die
//! einzige gemeinsame Schnittstelle zwischen Aufrufer-Thread und Kernel.

/// Syscall-Nummern (Register `x0` beim Eintritt).
pub mod sys {
    pub const YIELD: u64 = 0;
    /// Synchroner Aufruf: Nachricht senden + auf Antwort warten.
    pub const CALL: u64 = 1;
    /// Auf einen Aufrufer warten (Server).
    pub const RECV: u64 = 2;
    /// Den zuletzt empfangenen Aufrufer beantworten.
    pub const REPLY: u64 = 3;
    /// Den aufrufenden Thread schlafen legen — **es sei denn, eine Weckmarke liegt vor**
    /// (Selbst-Park; kein Cap nötig, weil er nur auf sich selbst wirkt).
    ///
    /// Zusammen mit [`UNPARK`] ist das der Baustein, auf dem eine PD `wait_event`, Completions
    /// und Mutex-Warteschlangen baut, **ohne je ein Kernelobjekt anzulegen** (Z22, P4): die
    /// Warteschlange ist eine Liste im Speicher der PD. Ohne Weckmarke ist der unbelastete Weg
    /// „Bedingung ist schon wahr" **null Syscalls**.
    pub const PARK: u64 = 5;
    /// Den aufrufenden Thread beenden (Stack/TCB werden zurückgewonnen; kein Cap).
    pub const EXIT: u64 = 6;
    /// Den über eine Tcb-Cap bezeichneten Thread beenden (cap-kontrolliert).
    pub const KILL: u64 = 7;
    /// Über eine Notification-Cap signalisieren (asynchron, nicht blockierend).
    pub const SIGNAL: u64 = 8;
    /// Auf eine Notification warten (blockiert; liefert den Badge in `x1`).
    pub const WAIT: u64 = 9;
    /// Einen über eine Memory-Cap bezeichneten Frame in die **eigene VSpace** mappen
    /// (identity, EL0-RW; cap-gated). Nur in einer isolierten VSpace sinnvoll.
    pub const MAP: u64 = 10;
    /// Einen zuvor gemappten Frame wieder aus der eigenen VSpace entfernen.
    pub const UNMAP: u64 = 11;
    /// **PD-Management** (ext-22): Lifecycle einer Ziel-PD steuern, gated auf eine
    /// `PdControl`-Cap (WRITE). Die Sub-Operation steht in `x2` (s. [`pdctl`]), Argumente
    /// in `x3`..`x5`. Nur eine TrustedSas-PD darf eine UserLand-PD so steuern.
    pub const PDCTL: u64 = 12;
    /// **Binary-Loader laden** (ext-26): ein Programm aus dem Boot-Archiv zur Laufzeit laden +
    /// starten, gated auf eine `Loader`-Cap (WRITE).
    ///
    /// `x1` = Loader-Cap-Index · `x2` = Archiv-Programm-Index · `x3` = **Delegationsliste**
    /// (bis zu [`LOAD_MAX_DELEGATES`] Bytes, je `delegate_pair(src, dst)`) · `x4` = **Anzahl**
    /// der gueltigen Paare · `x5` = **Ressourcenwunsch** der neuen PD (s. [`load_extras`]).
    /// Rückgabe: `x0` = result, `x1` = neue PD-Id (bei OK). Der geladene
    /// Prozess erhält NUR die so explizit delegierten Caps (keine Sonderrechte über die Cap).
    ///
    /// ## `x5` traegt ZWEI Felder, und die Belegung steht ausgeschrieben (2026-08-26)
    ///
    /// ```text
    /// x5 = (dma_pages << 16) | cap_budget
    ///      dma_pages   : Seiten DMA-Pool fuer die Geraetezuteilung, 0 = Vorgabe
    ///      cap_budget  : Cap-Slots der neuen PD, 0 = Vorgabe
    /// ```
    ///
    /// Ausgeschrieben und nicht abgezaehlt: in C8 kollidierte eine Marke auf Bit 63 mit dem
    /// obersten Zahlenfeld eines Ergebniswortes, das Urteil fiel durch, der Lauf ging in den
    /// Watchdog — und jedes gedruckte Feld war gruen. `x5 == 0` heisst „beides Vorgabe" und ist
    /// damit bitgleich zu jedem Aufruf, den es vor diesen Feldern gab.
    ///
    /// **Der DMA-Pool wird ueber [`DRIVER_DMA_MAX_PAGES`] hinaus ABGEWIESEN, nicht gekuerzt**
    /// ([`result::ERR_DMA_TOO_LARGE`]). Eine stillschweigend halbierte DMA-Region ist ein Geraet,
    /// das ueber ihr Ende hinausschreibt — die Kuerzung waere der Korruptionspfad, nicht der
    /// Komfort. Geprueft wird am **Rand**, bevor irgendetwas alloziert ist, und der Aufrufer wird
    /// dabei nicht blockiert (D11).
    ///
    /// ## Das Budget in `x5` (2026-08-26) — warum es hier steht und nicht im Manifest
    ///
    /// Bis dahin bekam **jede** PD dieselbe feste Zahl Cap-Slots. Das trug, solange eine PD 1–4
    /// Slots brauchte; eine Treiberumgebung braucht dreissig, und zehntausend andere weiterhin
    /// vier. Die Konstante anzuheben kostet den Bedarf **einer** PD mal `NPDS` — gemessen rund
    /// 7,7 MB je acht Slots.
    ///
    /// Das Manifest kommt dafuer nicht in Frage: es beschreibt das **Startverhalten** (ein kleiner
    /// Plattentreiber, ein Boot-Taskmanager) und wird signiert; was zur Laufzeit entsteht, steht
    /// nicht darin. Also traegt der Ladeaufruf die Zahl.
    ///
    /// Zwei Absagen, beide ohne Deckelung: mehr als das Maximum je PD, oder mehr, als der
    /// systemweite **Vorrat** noch hergibt. Stillschweigend zu kuerzen gaebe dem Aufrufer eine PD,
    /// die weniger kann als angefordert — und er merkte es erst am achten Cap.
    ///
    /// ## Bis 2026-08-25 war es GENAU EINER, nach Slot 0
    ///
    /// `x3` war ein einzelner Slot, `x4` ein Badge, das Ziel die Konvention L2. Damit konnte ein
    /// Lader einem Programm zur Laufzeit **eine** Autoritaet mitgeben — was reicht, solange alles
    /// Weitere im Manifest steht. Sobald das Manifest nur noch den Bootzustand beschreibt, ist es
    /// die Grenze des ganzen Systems: eine PD, die zur Laufzeit einen Dienst startet und ihm
    /// Arena, Kanal und Puffer mitgeben will, kann das mit einem Cap nicht sagen.
    ///
    /// ## Warum das Badge WEGGEFALLEN ist und nicht mitgewachsen
    ///
    /// Ein Badge fuer acht Caps waere ein Parameter mit acht Bedeutungen. Es wird auch nicht
    /// gebraucht: [`CCOPY`](Self::CCOPY) badgt eine Kopie im **eigenen** Cspace, und genau dafuer
    /// ist es da („wer zwei unterscheidbare Kanaele auf dasselbe Objekt braucht, badgt zwei
    /// Kopien"). Wer gebadgt delegieren will, badgt vorher und delegiert den Slot — das ist
    /// **mehr** ausdrueckbar als vorher (ein Badge JE Cap statt eines fuer alle) und kostet den
    /// Kernel eine Fallunterscheidung weniger.
    pub const LOAD: u64 = 13;
    /// **Einen Cap im eigenen Cspace löschen** (A-3.1). `x1` = lokaler Slot. Kein zusätzliches
    /// Cap nötig — die Autorität ist der eigene Cspace: Autorität *abzugeben* darf nie an einer
    /// Erlaubnis hängen.
    ///
    /// Ohne das kann ein langlebiger Dienst, der Caps per IPC empfängt, seine Slots nicht
    /// freigeben und läuft gegen `CAP_BUDGET_PER_PD`. Mit dem Root-Task (A-2) wird aus dieser
    /// ABI-Lücke ein Betriebsproblem: er ist genau so ein Dienst.
    ///
    /// ## Das ist bereits der Selbst-Lösch-Syscall aus A4 — es gibt keinen zweiten
    ///
    /// A4 verlangte „mindestens `SYS_CDELETE` (eigener Slot)". Genau das steht hier: aufgelöst
    /// wird `slot` in der PD **des Aufrufers** (`pd_of(thread)`), ein fremder Slot existiert in
    /// diesem Cspace gar nicht. Wer einen fremden oder leeren Slot nennt, bekommt die benannte
    /// Absage [`result::ERR_BADCAP`] — „fremd" und „leer" fallen zusammen, weil beides aus Sicht
    /// des Aufrufers dasselbe ist: *dort liegt nichts, das dir gehört*. Daneben stehen drei
    /// weitere benannte Ausgänge, jeder mit eigenem Grund: [`result::ERR_NOPD`] (der Aufrufer
    /// gehört zu keiner PD), [`result::ERR_HASCHILDREN`] (noch abgeleitete Caps daran — der Cap
    /// bleibt unverändert im Slot, ein halb entfernter Cap wäre schlimmer als gar keiner) und
    /// [`result::ERR_INUSE`] (die Stack-Cap eines lebenden Threads — erst `KILL`, dann löschen).
    /// Eine zweite Nummer für „dasselbe, aber wirklich nur eigene" könnte nichts anderes
    /// auflösen und wäre ein toter Syscall; die Nummer `30` ist seit A2-Rest als
    /// [`CALL_TIMEOUT`](Self::CALL_TIMEOUT) vergeben (Lücke `4` bleibt historische Lücke,
    /// nie vergeben).
    pub const CDELETE: u64 = 14;
    /// **Einen Cap im eigenen Cspace kopieren** (A-3.2). `x1` = Quell-Slot, `x2` = Ziel-Slot
    /// (muss frei sein), `x3` = gewünschte Rechte-Bitmaske (1=R, 2=W, 4=X), `x4` = Badge für die
    /// Kopie (`0` = Badge des Originals erben). Die Kopie bekommt **höchstens** die Rechte des
    /// Originals — eine Kopie darf nie mehr können als die Vorlage.
    ///
    /// Das Badge ist hier kein Beiwerk: bei Notifications und Endpoints steckt die Absender-
    /// Kennung in der **Cap**, nicht in der Nachricht. Wer zwei unterscheidbare Kanäle auf
    /// dasselbe Objekt braucht, badgt zwei Kopien — genau dafür ist die Operation da.
    pub const CCOPY: u64 = 15;
    /// **Einen Cap im eigenen Cspace verschieben** (A-3.2). `x1` = Quell-Slot, `x2` = Ziel-Slot
    /// (muss frei sein). Erzeugt **keine** Ableitung: derselbe Cap, ein anderer Slot.
    pub const CMOVE: u64 = 16;
    /// **Den Empfangs-Slot für per IPC übertragene Caps festlegen** (A-3.2). `x1` = Slot.
    ///
    /// Der **Empfänger** bestimmt, wo eine gegrantete Cap landet — nicht der Sender. Ohne das
    /// landete jeder Grant im festen [`GRANT_RECV_SLOT`], und ein Server konnte damit den Cap
    /// verdrängen, den sein Client dort gerade hielt.
    pub const SETRECV: u64 = 17;
    /// **Einen Thread der EIGENEN PD wecken** (Z22, P4). `x1` = rohe `ThreadId` des Ziels.
    ///
    /// Gegenstück zu [`PARK`]. Die Weckmarke wird **immer** hinterlegt, auch wenn das Ziel noch
    /// gar nicht schläft — genau dann ginge das Wecken sonst verloren. Geweckt wird nur, wer
    /// **wegen `PARK`** blockiert ist; wer in IPC wartet oder pausiert wurde, bleibt liegen.
    ///
    /// ## Warum das ohne Cap geht, und warum das keine Lücke ist
    ///
    /// Das Ziel muss in **derselben PD** liegen wie der Aufrufer (fail-closed: keine PD, fremde
    /// PD oder unbekannter Thread → [`result::ERR_BADCAP`]). Innerhalb einer PD teilen sich die
    /// Threads ohnehin den Adressraum — wer einen Nachbarthread wecken kann, konnte vorher schon
    /// seinen Stack beschreiben. Es kommt also **keine** Autorität hinzu, und eine Tcb-Cap je
    /// Thread wäre ein Slot je Warteschlangeneintrag, ohne etwas zu schützen.
    ///
    /// PD-übergreifend wecken bleibt, was es war: eine Notification.
    pub const UNPARK: u64 = 18;
    /// **Die Syscalls eines Threads an eine Persönlichkeits-PD umleiten** (Z26/A3).
    ///
    /// `x1` = Slot der **Tcb-Cap** des Zielthreads · `x2` = Slot der **`SyscallHandler`-Cap**
    /// (`u64::MAX` = keiner) · `x3` = Slot der **`FaultHandler`-Cap** (`u64::MAX` = keiner).
    /// Beide `u64::MAX` heisst **entbinden**.
    ///
    /// ## Der Aufruf braucht ZWEI Autoritäten von ZWEI Seiten
    ///
    /// Die Tcb-Cap sagt **wessen** Syscalls, die Handler-Caps sagen **wohin**. Beide werden im
    /// Cspace des **Aufrufers** aufgelöst — und der Aufrufer ist damit eine dritte Partei, weder
    /// Gast noch Handler. Ohne die Tcb-Cap könnte jede PD, die zufällig eine Handler-Cap hält, die
    /// Syscalls eines fremden Threads an sich ziehen; ohne die Handler-Cap wäre „umleiten" ein
    /// globaler Schalter statt einer Autorität.
    ///
    /// ## Die Gast-PD hält NICHTS — ihre Autorität wird verringert
    ///
    /// Sie bekommt keinen Cap und keine Operation. Nach der Bindung erreicht sie den
    /// Caprock-Kernel **gar nicht mehr**: auch dieser Syscall wird umgeleitet, ein gebundener
    /// Thread kann sich also nicht selbst entbinden. Rückgängig macht es, wer die Tcb-Cap hält.
    ///
    /// Rückgabe in `x0`: [`result::OK`], [`result::ERR_BADCAP`], [`result::ERR_RIGHTS`],
    /// [`result::ERR_HANDLER_CYCLE`], [`result::ERR_HANDLER_BUSY`], [`result::ERR_NOSPACE`].
    pub const SETHANDLER: u64 = 19;

    /// **Create a second thread inside the caller's own PD** (K1a, 2026-08-17).
    ///
    /// The kernel half of "several threads per PD" has existed since 2026-08-10 (Z22 P2): the PD
    /// table carries `nthreads`, a reverse index and `any_thread`, and the `pdthrd` line measures
    /// two threads in one PD every run. What was missing was **a way for userspace to ask** —
    /// without it there is no `pthread_create`, and therefore no Mesa, no smithay, no JVM.
    ///
    /// ## The stack comes from a Cap of the CALLER — decided, not defaulted
    ///
    /// `MSG0` names a slot in the caller's own Cspace holding an `ObjectKind::Memory` region; it
    /// becomes the new thread's stack. **The kernel allocates nothing.** Three reasons, each of
    /// which this project has already paid for separately:
    ///
    /// * a kernel-side stack would be **a second memory policy in the kernel** (the `system::alloc`
    ///   lesson — a classification checked against only one suite);
    /// * it would break **attribution**: the caller carries the cost out of its own Cap, the same
    ///   logic as `consumed_cycles` per TCB;
    /// * **the kernel manages authority, not supply.** seL4 answers the same question the same way
    ///   for the same reason.
    ///
    /// ## What this couples — and the honest price of the decision
    ///
    /// A TCB whose stack comes from a Cap couples **CapSpace and Scheduler**: what happens on
    /// `revoke`/`delete` of that Cap while the thread runs? Two honest outcomes existed; this ABI
    /// takes the second, and says so:
    ///
    /// > **Deleting a bound stack Cap is REFUSED while the thread lives** ([`result::ERR_INUSE`]).
    ///
    /// Not because it is nicer, but because it is *countable*: the TCB holds a reference the K1
    /// class can count, and the alternative ("deletion kills the thread with it") would be a group
    /// operation across `CAPS` **and** `SCHEDS[core]` — the V4 class with two locks and an
    /// ordering. It is also fail-closed, and it has an idiom in this tree already
    /// ([`result::ERR_HASCHILDREN`]). A thread that must go is killed explicitly with
    /// [`KILL`]; afterwards the Cap deletes normally.
    ///
    /// ## The checks at the edge, each with its OWN code — never a blanket refusal
    ///
    /// | condition | code |
    /// |---|---|
    /// | slot empty / not a memory Cap | [`result::ERR_BADCAP`] |
    /// | Cap lacks `rw`, or is not in the `normal` address space | [`result::ERR_RIGHTS`] |
    /// | region is **reachable by a device** (DMA window) | [`result::ERR_DMA_REACHABLE`] |
    /// | too small, or not aligned | [`result::ERR_BADSTACK`] |
    /// | overlaps an existing mapping of the target VSpace | [`result::ERR_NOSPACE`] |
    /// | PD is already at its thread limit | [`result::ERR_THREAD_LIMIT`] |
    ///
    /// **`ERR_DMA_REACHABLE` is not paranoia**: a stack a device can write is the `by ops`
    /// placement rule turned into an attack — the return address is data, and a device that
    /// reaches it chooses where the thread goes next.
    ///
    /// ## Admission order is D0, literally
    ///
    /// `spawn_parked` → stack and TLS base set → `bind_pd` → `admit`. A thread that becomes
    /// runnable before it holds its authority makes its first syscall with an empty Cspace; that
    /// was D0, it cost ten days, and its rate was 0,018 %.
    ///
    /// ## What is NAMED here and deliberately not decided
    ///
    /// After binding, the stack Cap **stays writable by the caller**. Within one PD that is the
    /// same trust zone and therefore fine — a spawner can already write its own memory. **Should
    /// `SPAWN` ever cross a PD boundary, this stops being harmless**: the spawner could corrupt
    /// the spawnee's stack at any moment, and the Cap would have to be transferred exclusively
    /// instead of shared. One sentence today, so that it is a decision later and not a discovery.
    ///
    /// ## One Cap, several stacks — the sub-region (K1b, 2026-08-26)
    ///
    /// Until this, one `Memory` Cap bought exactly **one** thread, and the Cap stayed pinned for
    /// that thread's life ([`result::ERR_INUSE`]). "How many threads may a PD have" was therefore
    /// really "how many Cap slots are left", which is the wrong question: threads of one PD share
    /// the address space anyway, so a separate Cap per stack buys **no isolation** — see the note
    /// on [`UNPARK`]. A driver PD that already spends 6 of its 8 slots on its endowment could
    /// spawn at most two.
    ///
    /// So `x1` ([`reg::EP_BADGE`], the only register this syscall does not already use) names a
    /// **sub-region** of the Cap:
    ///
    /// ```text
    /// x1 = (offset_pages << 32) | length_pages     // a window inside the Cap
    /// x1 == 0                                      // the WHOLE region — the old behaviour
    /// ```
    ///
    /// Pages and not bytes, because both quantities are page-aligned by construction anyway and
    /// 32 bits of pages is 16 TiB. **`0` is bit-identical to every call written before this
    /// existed** — the compatibility is in the encoding, not in a branch somebody has to
    /// remember.
    ///
    /// The layout is written out rather than counted: in C8 a marker on bit 63 collided with the
    /// top number field of a result word, the judgement failed, and the run died in the watchdog
    /// while every printed field was green.
    ///
    /// A window outside the Cap gets its **own** code ([`result::ERR_SUBREGION`]) — *you named a
    /// region you do not hold* is a different statement from *that region is unusable as a stack*.
    ///
    /// **Sub-regions of one Cap must not overlap each other**, and that is checked
    /// ([`result::ERR_NOSPACE`]): sibling stacks in one arena are exactly the case where an
    /// off-by-one is a silent trampling rather than a fault.
    ///
    /// `MSG0` stack Cap slot · `MSG1` entry point · `MSG2` argument · `MSG3` priority ·
    /// `x1` sub-region (`0` = whole).
    /// Returns the new `ThreadId` in [`reg::MSG0`] on [`result::OK`].
    pub const SPAWN: u64 = 20;

    // --- Z6b: the debugger -----------------------------------------------------------------
    //
    // **Debug authority is a capability over exactly one PD**, and these five operations are the
    // whole kernel surface of it. Everything else a debugger does — DWARF, disassembly, expression
    // evaluation, the GDB remote protocol — runs unprivileged, outside, or on the developer's host.
    //
    // Why each of these is in the kernel and not outside it:
    //
    // | operation | in the kernel because |
    // |---|---|
    // | `DEBUG_STOP`/`DEBUG_CONTINUE` | scheduler state — no userspace path sets a `BlockReasons` bit |
    // | `DEBUG_WRITE_REGS` | **the mask is the entire security statement** |
    // | `DEBUG_READ_MEM` | it walks the **target's** page tables, not the caller's |
    // | `DEBUG_ATTACH` | it derives a CDT child, which is a `CAPS.write()` |
    //
    // Frame *reading* deliberately has no syscall: the Z26/A3 sidecar is mapped read-only into the
    // debugger, so reading registers costs no kernel entry at all.

    /// **Derive a debug capability from a `Debuggable`.**
    ///
    /// `MSG0` slot of the `Debuggable` Cap · `MSG1` requested rights
    /// ([`debug::RIGHT_READ`] / [`debug::RIGHT_CONTROL`]) · `MSG2` destination slot.
    ///
    /// Refused with [`result::ERR_NOT_DEBUGGABLE`] when the target PD never had a `Debuggable`
    /// minted — which is a **different statement** from [`result::ERR_BADCAP`] ("you do not hold
    /// one"), and the difference is the whole claim.
    pub const DEBUG_ATTACH: u64 = 21;

    /// **Stop a thread of the target PD.** `MSG0` Cap slot (`DebugControl`) · `MSG1` raw ThreadId.
    ///
    /// The target stops at its **next kernel entry**, not mid-instruction — at 100 Hz that is
    /// ≤ 10 ms, and that bound is part of the promise rather than an implementation detail.
    ///
    /// A second holder attempting to stop an already-stopped target gets
    /// [`result::ERR_DEBUG_BUSY`]: the stop has exactly one owner, because a reason **bit** has no
    /// refcount. *Wer eine Kapazitaet einfuehrt, muss den Ueberlauf benennen* — and "one" is a
    /// capacity.
    pub const DEBUG_STOP: u64 = 22;

    /// **Let a stopped thread run again.** `MSG0` Cap slot (`DebugControl`) · `MSG1` raw ThreadId.
    pub const DEBUG_CONTINUE: u64 = 23;

    /// **Read the target's memory.** `MSG0` Cap slot (`DebugRead` or `DebugControl`) ·
    /// `MSG1` target VA · `MSG2` length · `MSG3` VA of the caller's own buffer.
    ///
    /// Returns bytes transferred in [`reg::MSG0`]. Allowed **while the target runs**: a byte range
    /// has no internal consistency condition, and a continuous observer needs exactly this. The
    /// register frame is the opposite case and needs the sidecar generation.
    pub const DEBUG_READ_MEM: u64 = 24;

    /// **Write registers back.** `MSG0` Cap slot (`DebugControl`) · `MSG1` raw ThreadId ·
    /// `MSG2` frame word index · `MSG3` value.
    ///
    /// A debugger may write GPRs **plus PC, SP and masked flags** — a third authority level above
    /// the redirect handler, which may write GPRs only. Neither may ever write `cs`/`ss` (x86) or
    /// the EL bits of `spsr` (aarch64): those carry the **ring**, and whoever writes them promotes
    /// their target.
    pub const DEBUG_WRITE_REGS: u64 = 25;

    /// **Bind a device interrupt to a notification** (Stufe B, B3).
    ///
    /// `x1` slot of the `Irq` Cap · `MSG0` slot of the `Notification` Cap · `MSG1` badge.
    ///
    /// **The caller does not pass a vector — it passes a Cap.** That is the whole point of this
    /// syscall existing, and it is why the number never appears in the argument list: the
    /// interrupt this call may bind is the one the `Irq` Cap names, and the kernel reads it out of
    /// the Cap rather than checking a number the caller supplied against a table. The in-kernel
    /// `bind_irq` took raw ids and its own doc-comment claimed to be "authorised via the IRQ Cap";
    /// a signature cannot support that claim, and *ein Waechter prueft die EXISTENZ eines Grundes,
    /// nie seine WAHRHEIT.*
    ///
    /// Rights: `READ` on the `Irq` Cap (binding is not an intervention in another thread), `WRITE`
    /// on the notification (the kernel will signal it).
    ///
    /// **Rebinding the same Cap replaces its entry** rather than consuming a second one —
    /// otherwise a driver that reloads exhausts its own device. Refused with
    /// [`result::ERR_IRQ_FULL`] when there is no room, and the caller is **not** blocked.
    pub const BIND_IRQ: u64 = 26;

    /// **Set the calling thread's thread pointer** (TLS, T2). `x1` = VA; `0` disables it.
    ///
    /// **No capability, and that is the whole argument.** This acts only on the caller — the same
    /// class as [`YIELD`], [`PARK`] and [`EXIT`]. Whoever sets their own thread pointer gains
    /// nothing: the address is dereferenced by *their* code in *their* address space, and what is
    /// readable there was already decided by the MMU. A cap here would guard a door that opens
    /// onto the room you are standing in.
    ///
    /// **The address IS checked — against [`super::USER_VA_TOP`] — and that check is not about
    /// what the caller may reach.** It is about what the caller can make the *kernel* do.
    ///
    /// `WRMSR` to `IA32_FS_BASE` with a **non-canonical** value raises `#GP(0)` **in ring 0, at
    /// the write**. Without a bound, `SETTLS(0x1234_5678_9ABC_DEF0)` from a PD holding **no
    /// capability at all** takes the kernel down. The authority argument above is sound for what
    /// the caller can *reach* — the MMU decides that — and says nothing about what it can make the
    /// kernel *execute*. Two different questions, and only the first one is answered by "no cap
    /// needed".
    ///
    /// The bound is the **user half**, not "canonical": stronger, and it means something. A thread
    /// pointer in the upper half is useless anyway, and every user window this kernel maps lies far
    /// below it. Refused with [`super::result::ERR_BADTLS`].
    ///
    /// What is deliberately **not** checked is whether the address is *mapped*: an unreadable one
    /// faults on first use, in the caller, at the instruction that used it — a better place to
    /// learn it than a return code, and it costs the kernel no knowledge of the layout.
    ///
    /// **aarch64 needs none of this**: `TPIDR_EL0` accepts any value. Same shape as E-B1 — one
    /// architecture leans on a check the other structurally does not need, and if the reason is
    /// not written at the site, it reads as a redundant line and gets removed.
    ///
    /// Setting the pointer of **another** thread is a different authority (the debugger's) and is
    /// not offered.
    pub const SETTLS: u64 = 27;

    /// **Read the monotonic clock** (Stufe A, A1). No arguments.
    ///
    /// Returns the rate in [`reg::MSG0`] (ticks per second) and the current counter in
    /// [`reg::MSG1`].
    ///
    /// **No capability**, same class as [`YIELD`] and [`SETTLS`]: reading time grants nothing and
    /// reveals nothing the caller could not already observe by counting its own instructions.
    ///
    /// **Two numbers, and the rate is the one that was missing.** The counter itself is readable
    /// from ring 3 on both architectures already (`rdtsc`; `CNTVCT_EL0`) — what a program cannot
    /// know is what a tick is *worth*, and that is calibrated in the kernel. Returning the counter
    /// too costs nothing and spares a caller that does not want to write architecture-specific
    /// assembly.
    ///
    /// **What this is NOT:** wall-clock time, an absolute epoch, or a deadline. Deadlines are A2
    /// and change the blocking invariants; this syscall changes nothing about them.
    pub const CLOCK: u64 = 28;

    /// **Sich selbst mit Frist parken** (A2-Rest): wie [`PARK`], aber mit Rueckkehr.
    ///
    /// `MSG0` = Frist in Ticks derselben monotonen Uhr, die [`CLOCK`] liest (`0` = keine Frist,
    /// dann verhaelt sich der Aufruf bitgleich zu [`PARK`]). Rueckgabe in `x0`: [`result::OK`]
    /// (per [`UNPARK`] geweckt oder Weckmarke verbraucht) oder [`result::ERR_TIMEOUT`] (die Frist
    /// lief ab, ohne dass geweckt wurde).
    ///
    /// **Die Weckmarken-Semantik von [`PARK`] bleibt erhalten:** liegt eine Marke vor, wird sie
    /// verbraucht und SOFORT mit [`result::OK`] zurueckgekehrt — die Frist greift dann nicht.
    /// Ohne Marke wird blockiert, und der Wecker ist ausschliesslich [`UNPARK`] (wie bei
    /// [`PARK`], nicht wie bei [`WAIT`]: ein fremdes `reply` weckt hier niemanden).
    ///
    /// **Keine Cap**, dieselbe Klasse wie [`PARK`]: der Aufruf wirkt nur auf den Aufrufer. Er
    /// steht deshalb im Dispatch vor der generischen Cap-Aufloesung — sie wuerde `x1` als
    /// Cap-Slot lesen und mit [`result::ERR_BADCAP`] abweisen.
    ///
    /// Der Baustein, auf dem eine PD `wait_event_timeout`, `wait_for_completion_timeout`,
    /// `msleep` und `delayed_work` baut, ohne je ein Kernelobjekt anzulegen (Z22, P4): die
    /// Warteschlange bleibt eine Liste im Speicher der PD, die Frist liegt im Scheduler.
    pub const PARK_TIMEOUT: u64 = 29;

    /// **Aufruf mit Frist** (A2-Rest): wie [`CALL`], aber mit Rueckkehr.
    ///
    /// `MSG0` = Frist in Ticks (`0` = keine, dann wie `CALL` mit 3-Wort-Nachricht).
    /// Die Nachricht steht in `MSG1..MSG3` -- drei Worte statt vier: vier Worte plus Frist
    /// passen nicht in `x2..x5`, und ein Flag in `TAG` waere ein zweiter Parameter, der zwei
    /// Bedeutungen traegt. Der Dispatch schiebt vor dem Rendezvous nach unten (`MSG1->MSG0`,
    /// ...), der Server sieht eine normale 3-Wort-Nachricht. Rueckgabe `OK` oder
    /// [`result::ERR_TIMEOUT`]; die Antwort bleibt vier Worte.
    ///
    /// Der bewachte Grund ist `IPC` (nicht `PARK`): `unblock` (Antwort) und Timer entfernen
    /// denselben Grund, das Rennen entscheidet `fristen_faellig` wie bei [`WAIT`]
    /// (Signal gewinnt). Laeuft die Frist ab, schreibt der Timer `ERR_TIMEOUT` und weckt;
    /// ein spaetes `reply` findet keinen Wartenden mehr und wird benannt abgewiesen
    /// (kein Schreiben in fremde Frames).
    pub const CALL_TIMEOUT: u64 = 30;

    /// **Geprueftes Treiber-Bild laden** (LXPD-Laufzeitpfad): wie [`LOAD`](Self::LOAD), aber
    /// das Bild kommt NICHT aus dem Boot-Archiv, sondern aus einer Memory-Cap des Aufrufers.
    ///
    /// Register-Belegung wie `LOAD` (`MSG0` = Programm-ID aus dem Boot-Manifest, `MSG1` =
    /// Delegationsliste, `MSG2` = Anzahl, `MSG3` = Ressourcenwunsch), dazu `TAG`: Low-Byte =
    /// Slot der Memory-Cap mit dem geprueften Bild, Bits 8..40 = exakte Bildlaenge in Byte,
    /// Bits 40..64 muessen `0` sein (sonst Absage — kein stilles Abschneiden). Die Laenge muss
    /// exakt der geladenen Laenge entsprechen — kuerzer heisst abgebrochen, laenger heisst
    /// Anhaengsel, beides wird abgewiesen. Manifest-Cap braucht es keine: Vertrauen kommt aus
    /// dem Boot-Manifest (Hash-Gleichheit mit dem Eintrag dieser Programm-ID), nicht aus
    /// mitgereichten Bytes — „niemals der PD glauben".
    ///
    /// Gatter wie `LOAD` (Loader-Cap + WRITE), Delegation wie `LOAD`, DMA-Schranke VOR jeder
    /// Ableitung. Der Kernel kopiert die Bytes EINMAL in Staging und prueft danach nur noch
    /// die Kopie (kein TOCTOU zwischen Hash und Laden). Schranke
    /// [`LXPD_MAX_BILD`](Self::LXPD_MAX_BILD) gilt vor jeder Kopie.
    pub const LOAD_IMAGE: u64 = 36;

    /// **Obergrenze eines Laufzeit-Treiberbilds** (`LOAD_IMAGE`): 4 MiB. Wer mehr braucht,
    /// bekommt eine benannte Absage statt einer stillen Teilkopie — Treiber-Images sind
    /// klein, und eine Schranke hier ist billiger als ein halb geladener Treiber.
    pub const LXPD_MAX_BILD: u64 = 4 * 1024 * 1024;

    /// **Speicher der Ziel-PD schreiben** (Debugger v2, `M`-Paket der gdbserver-PD).
    ///
    /// `MSG0` Cap-Slot (`DebugControl`) · `MSG1` Ziel-VA · `MSG2` Laenge ·
    /// `MSG3` VA des eigenen Puffers im Aufrufer. Gibt uebertragene Bytes in
    /// [`reg::MSG0`] zurueck. Wie [`DEBUG_READ_MEM`](Self::DEBUG_READ_MEM) gegen eine
    /// benannte Kapazitaet (`debug::WRITE_MAX`) gedeckelt — dieselbe Latenzform,
    /// gespiegelt: der Aufrufer schleift, seine Schleife ist preemptibel.
    ///
    /// **Adressraum-Schnappschuss in eine frische Kind-PD (FORK, Phase 1: volle Kopie).**
    ///
    /// `x1` = Quell-PD ist IMMER die eigene (der Aufrufer klont sich selbst; eine fremde PD
    /// zu klonen waere ein `DEBUG_READ_MEM` ohne Debug-Cap) · `MSG0` = Ziel-Slot fuer die
    /// Kind-`PdControl`-Cap im Aufrufer-Cspace (muss frei sein) · `MSG1` = maximale
    /// Kopierlaenge in Bytes (`0` = ganze abgebildete User-Flaeche; gedeckelt gegen
    /// `fork::SNAPSHOT_MAX_BYTES`) · `MSG2` = Ziel-Prio des Kind-Hauptthreads ·
    /// `MSG3` = reserviert (`0`).
    ///
    /// Rueckgabe: `x0` = result, `MSG0` = Kind-PD-Id (bei OK). Das Kind startet PARKIERT
    /// (D0: parken -- binden -- zulassen; erst `PDCTL/START` laesst es laufen).
    ///
    /// ## Volle Kopie, kein COW — und das steht hier, nicht im Kleingedruckten
    ///
    /// Phase 1 kopiert jede abgebildete User-Seite (s. `fork`-Modul in `caprock-loader`).
    /// COW ist Phase 2 und braucht zwei Dinge, die heute fehlen: Dirty-Tracking
    /// (kein Write-Protect-Bit wird je gesetzt, kein Fault meldet „schreibend") und einen
    /// Seitenfehler-Pfad, der nachlaedt statt beendet (Faults beenden heute den Thread).
    /// Wer COW ohne beides verspricht, teilt Seiten still statt kopiert.
    ///
    /// Nummer `31`: erste der beiden bewusst freigehaltenen Luecken nach `CALL_TIMEOUT`.
    pub const FORK_SNAPSHOT: u64 = 31;

    /// **Neues Image in die BESTEHENDE eigene PD laden (EXEC-Replace).**
    ///
    /// `x1` = Loader-Cap-Slot im eigenen Cspace (WRITE) · `MSG0` = Archiv-Programm-Index ·
    /// `MSG1` = Teardown-Token (s. `exec`-Modul in `caprock-loader`: `token == 0` heisst
    /// „kein Token" und wird abgewiesen, nicht als „egal" gelesen) · `MSG2` = neue
    /// Eintragsadresse wird IGNORIERT (sie kommt aus dem Image; ein Aufrufer, der sie
    /// waehlt, waehlt fremden Code) · `MSG3` = reserviert (`0`).
    ///
    /// ## Slots/Caps werden geordnet zurueckgezogen, nicht ueberschrieben
    ///
    /// Vor dem Laden zieht der Kernel alle Threads der PD ausser dem Aufrufer ab
    /// (`KILL`-Ordnung), loescht alle Slots ausser Loader- + Aufrufer-Stack-Cap
    /// (Teardown-Token-Form: wer das Token nicht nennt, bekommt `ERR_STALE_TOKEN`,
    /// kein halb geraeumtes Kind), und erst dann laedt er. Ein Ueberrest (alter Thread,
    /// alte Cap, altes Mapping) ist ein Baufehler, kein „wird schon ueberschrieben".
    ///
    /// Nummer `32`: zweite der beiden Luecken nach `CALL_TIMEOUT`.
    pub const EXEC_REPLACE: u64 = 32;

    /// **Speicher der Ziel-PD schreiben** (Debugger v2, `M`-Paket der gdbserver-PD).
    ///
    /// `MSG0` Cap-Slot (`DebugControl`) · `MSG1` Ziel-VA · `MSG2` Laenge ·
    /// `MSG3` VA des eigenen Puffers im Aufrufer. Gibt uebertragene Bytes in
    /// [`reg::MSG0`] zurueck. Wie [`DEBUG_READ_MEM`](Self::DEBUG_READ_MEM) gegen eine
    /// benannte Kapazitaet (`debug::WRITE_MAX`) gedeckelt — dieselbe Latenzform,
    /// gespiegelt: der Aufrufer schleift, seine Schleife ist preemptibel.
    ///
    /// Nummer `33`: `30` ist `CALL_TIMEOUT`, `31`/`32` sind seit Prozessmodell FORK/EXEC.
    pub const DEBUG_WRITE_MEM: u64 = 33;

    /// **Einen Thread genau einen Schritt tun lassen** (Debugger v2, `s`-Paket).
    ///
    /// `MSG0` Cap-Slot (`DebugControl`) · `MSG1` rohe ThreadId. Setzt keinen
    /// `BlockReasons`-Zustand, sondern scharft den Einzelschritt der CPU (`TF` auf
    /// x86, `MDSCR_EL1.SS` + `PSTATE.SS` auf aarch64, s. `hal::debug`) und laesst den
    /// angehaltenen Thread genau einen Befehl ausfuehren. Abgewiesen mit
    /// [`result::ERR_DEBUG_BUSY`], wenn das Ziel nicht gehalten ist.
    pub const DEBUG_SINGLE_STEP: u64 = 34;

    /// **Hardware-Breakpoint setzen/loeschen** (Debugger v2, `Z`/`z`-Pakete).
    ///
    /// `MSG0` Cap-Slot (`DebugControl`) · `MSG1` rohe ThreadId · `MSG2` Adresse ·
    /// `MSG3` Art (`0` Software — abgewiesen, s. Plan §10d · `1` Hardware). Vier
    /// Register je Kern sind eine benannte Kapazitaet wie der Halt selbst (§8a):
    /// Erschoepfung wird abgewiesen, nicht still geteilt. Erfordert pro-Thread-Sichern
    /// der Debugregister im Kontextwechsel (`hal::debug`).
    ///
    /// Stand 2026-09-10 (Audit): Nummern 33-35 sind VERGEBEN, aber UNDISPATCHED -- der
    /// Kernel deckt nur 21-25 ab, Aufrufe fallen auf `ERR_BADSYS` (kein Stub, der Erfolg
    /// meldet). Verdrahtung zusammen mit `hal::debug` + RSP-PD (`programs/rspd`).
    pub const DEBUG_HWBREAK: u64 = 35;

    /// **Einen Teilbereich einer Memory-Cap als eigene Cap ableiten** (CSUB,
    /// Mem-Server-Transport): das Fenster steht am Slot, nicht am Objekt — Eltern und
    /// Kind teilen das Objekt, `revoke` zieht das Teil ueber die Kindkette ein, die
    /// Region wird genau einmal freigegeben (wenn die letzte Cap faellt).
    ///
    /// `x1` = Quell-Slot (Memory) · `MSG0` = freier Ziel-Slot · `MSG1` = Offset ·
    /// `MSG2` = Laenge. Vier Worte, keine Rechte-Maske: die Schnitt-Eigenschaft traegt
    /// das Primitiv (`CapSpace::subregion`), nicht der Aufruf. Ausgaenge: `OK` ·
    /// `ERR_BADCAP` (kein Cap / keine Memory-Cap) · `ERR_NOSPACE` (Ziel belegt oder
    /// ausserhalb, Budget, Tabelle) · `ERR_SUBREGION` (21) · `ERR_RIGHTS`.
    ///
    /// Nummer 37 = erste freie nach FORK/EXEC + Debugger-v2 + LOAD_IMAGE.
    pub const CSUB: u64 = 37;

    /// **Derive a channel-restricted copy of an endpoint capability** (`CHAN`, 2026-10-09).
    ///
    /// `x1` = source slot (endpoint) · `x2` = free destination slot · `x3` = channel value
    /// (low 32 bits) · `x4` = channel mask (low 32 bits, must be non-zero).
    ///
    /// The result carries the rule `(channel & mask) == value`, checked by the kernel on
    /// every `CALL`/`CALL_TIMEOUT` through it (channel = low 32 bits of the `x6` tag word).
    /// One rule per cap: deriving onto an already restricted cap is refused. The rule is
    /// preserved by `copy`/`move`/`derive_ipc` and never widened — like `perms`, it lives
    /// at the slot, and `audit_cdt` code 10 checks its well-formedness.
    ///
    /// Outcomes: `OK` · `ERR_BADCAP` (no cap / no endpoint cap / destination out of range) ·
    /// `ERR_NOSPACE` (destination occupied, budget, table) · `ERR_RIGHTS` (mask == 0,
    /// already restricted, or no DUP on a tagged source).
    ///
    /// Nummer 38 = erste freie nach CSUB.
    pub const CHAN: u64 = 38;
}

/// Rights and frame-word names for the debug syscalls (Z6b).
pub mod debug {
    /// Read the frame and the target's memory. Never stops anything.
    pub const RIGHT_READ: u64 = 1 << 0;
    /// Stop, continue, write registers.
    pub const RIGHT_CONTROL: u64 = 1 << 1;
    /// Both — the ordinary interactive session.
    pub const RIGHT_BOTH: u64 = RIGHT_READ | RIGHT_CONTROL;

    /// How many bytes one [`super::sys::DEBUG_READ_MEM`] transfers at most.
    ///
    /// A **named** capacity rather than an unbounded loop in the kernel: the read walks the
    /// target's page tables under a lock, and an unbounded caller-chosen length is a latency hole
    /// nobody sees until it is a hang. The caller loops instead, and its loop is preemptible.
    pub const READ_MAX: u64 = 512;

    /// How many bytes one [`super::sys::DEBUG_WRITE_MEM`] transfers at most.
    ///
    /// The mirror of [`READ_MAX`]: the write walks the **target's** page tables under
    /// a lock, so the same latency hole applies in the other direction. Same value,
    /// same reason — one capacity per direction, not one number with two meanings.
    ///
    /// Stand 2026-09-10 (Audit): noch ohne Durchsetzung -- `DEBUG_WRITE_MEM` (33) ist
    /// undispatched (s. dort), erst die Verdrahtung zieht diese Schranke nach.
    pub const WRITE_MAX: u64 = 512;
}

/// **Das Register-Layout der Umleitungsnachricht** (Z26/A3) — was der Handler in seinem `RECV`
/// vorfindet.
///
/// Die **Nutzlast** ist ausdrücklich nicht hier: der Trap-Frame des Gastes liegt im **Sidecar**,
/// dem geteilten Fenster, das an der `SyscallHandler`-Cap hängt. Der Grund ist eine Zahl:
/// [`MSG_WORDS`] ist **4**, ein x86_64-Trap-Frame hat **22** Wörter und ein aarch64-Frame **34**.
/// Eine Nachricht kann ihn nicht tragen — und `rt_sigreturn` (ersetzt den ganzen Frame) und
/// `clone` (braucht einen zweiten) wären damit strukturell unmöglich, nicht bloss unbequem.
///
/// Die Nachricht sagt also nur, **welcher** Gast und **warum**; gelesen und geschrieben wird im
/// Fenster.
pub mod redirect_msg {
    /// `x2`: Sidecar-Slot des Gastes. Der Frame steht bei `slot * 512` im Fenster.
    pub const SLOT: usize = super::reg::MSG0;
    /// `x3`: rohe `ThreadId` des Gastes (für `KILL`/Diagnose; der Handler kennt seine Gäste).
    pub const GUEST_TID: usize = super::reg::MSG1;
    /// `x4`: Anlass — `0` = Syscall, `1` = Fault (s. `caprock_sched::redirect::Anlass`).
    pub const ANLASS: usize = super::reg::MSG2;
    /// `x5`: architekturabhängiger Anlasscode (x86: Vektor · aarch64: `ESR_EL1.EC`), `0` bei
    /// einem Syscall.
    pub const CODE: usize = super::reg::MSG3;
    /// Bytes je Sidecar-Slot — hergeleitet aus dem grössten Trap-Frame (aarch64: 272 B),
    /// aufgerundet auf die nächste Zweierpotenz. Muss mit
    /// `caprock_sched::redirect::SLOT_BYTES` übereinstimmen; der Kernel prüft das
    /// (`handler_selbsttest`).
    pub const SLOT_BYTES: u64 = 512;
}

/// Sub-Operationen für [`sys::PDCTL`] (Register `x2`). Jede ist auf den Besitz der
/// `PdControl`-Cap für die Ziel-PD beschränkt.
pub mod pdctl {
    /// Ziel-PD starten (initial blockierten Haupt-Thread wecken).
    pub const START: u64 = 0;
    /// Ziel-PD stoppen (Thread beenden + isolierte Ressourcen abbauen).
    pub const STOP: u64 = 1;
    /// Ziel-PD pausieren (Thread blockieren, ohne ihn zu beenden).
    pub const PAUSE: u64 = 2;
    /// Pausierte Ziel-PD fortsetzen (Thread entblocken).
    pub const RESUME: u64 = 3;
    /// Der Ziel-PD ein CPU-Budget zuweisen (Arg: SchedContext-Cap-Slot in `x3`).
    pub const ASSIGN_BUDGET: u64 = 4;
    /// **Create a UserLand PD** (2026-10-09): `x3` = cap budget (0 = default),
    /// `x4` = free caller slot receiving the new `PdControl` cap. Returns `OK` with the
    /// new PD index in `MSG0`. The PD starts empty (no threads, no mappings, no caps);
    /// bring it up with `SPAWN_INTO`, `MAP_INTO`, then `START`.
    pub const CREATE: u64 = 5;
    /// **Map caller-held memory into the target PD** (2026-10-09): `x3` = memory-cap slot
    /// in the *caller* cspace (whole region, rights-derived perm like [`super::sys::MAP`]).
    /// The target needs at least one thread (its address space must exist) — hence the
    /// order CREATE → SPAWN_INTO → MAP_INTO → START. Refusals mirror `MAP`.
    pub const MAP_INTO: u64 = 6;
    /// **Spawn a thread in the target PD** (2026-10-09): `x3` = memory-cap slot in the
    /// *caller* cspace (stack, whole region), `x4` = entry, `x5` = arg, `x6` = priority
    /// (low 8 bits, rest must be zero). Same stack checks as [`super::sys::SPAWN`]; the
    /// stack region is mapped into the target address space (created on first use).
    /// Sub-windows in foreign PDs are a named gap (register pressure).
    pub const SPAWN_INTO: u64 = 7;
}

/// Anzahl der Nachrichten-Datenwörter (Register `x2`..`x5`).
pub const MSG_WORDS: usize = 4;

/// Capability-Transfer: Ist dieses Bit im Tag (`x6`) gesetzt, überträgt ein
/// `REPLY` zusätzlich die Capability am lokalen Slot `tag & 0xff` des Servers an
/// den Aufrufer (in dessen [`GRANT_RECV_SLOT`]).
pub const GRANT_FLAG: u64 = 1 << 63;
/// Slot im Empfänger-Cspace, in dem eine per IPC übertragene Cap landet.
pub const GRANT_RECV_SLOT: usize = 1;

/// **IPC permissions: the general, kernel-attested rights word of an endpoint capability.**
///
/// An endpoint capability can carry a 16-bit *permission mask* next to its badge. A client holds
/// a derived capability (`CCOPY` with a non-zero permission mask, see [`sys::CCOPY`]); the
/// **kernel** places the mask in the high 16 bits of the badge word the server receives in
/// [`reg::EP_BADGE`] on `RECV`:
///
/// ```text
///  63            48 47                              0
/// +----------------+---------------------------------+
/// |  permissions   |  object id (chosen by minter)   |
/// +----------------+---------------------------------+
/// ```
///
/// The client cannot forge either half: the id was fixed when the capability was derived, and the
/// permissions can only be *removed* on every further derivation (monotone attenuation).
///
/// ## What the kernel enforces, and what it does not
///
/// The kernel enforces **who may hold, copy and pass on** the capability ([`DUP`], [`GRANT`]) and
/// that permissions never grow. It does **not** interpret bits `0..=13`: their meaning belongs to
/// the interface that receives the capability (for a file service, say, bit 0 = read, bit 1 =
/// write). A server checks `perms_of(badge) & NEEDED` on every request. Keeping the meaning out of
/// the kernel is what keeps the kernel a microkernel.
///
/// A capability that was never derived with a permission mask is *untagged*; it behaves exactly as
/// before (the badge is delivered unchanged), so existing programs are unaffected.
pub mod ipc_perm {
    /// Bit position of the permission mask inside the delivered badge word.
    pub const SHIFT: u32 = 48;
    /// Mask for the object id inside the delivered badge word (low 48 bits).
    pub const ID_MASK: u64 = (1u64 << SHIFT) - 1;
    /// Kernel-interpreted: the holder may derive further capabilities (`CCOPY`) from this one.
    pub const DUP: u16 = 1 << 15;
    /// Kernel-interpreted: the holder may pass this capability on in a `REPLY` grant.
    pub const GRANT: u16 = 1 << 14;
    /// Bits whose meaning is defined by the receiving interface, not by the kernel.
    pub const SERVICE_MASK: u16 = 0x3fff;
    /// All permissions. The starting point when a permission mask is first applied to an untagged
    /// endpoint capability.
    pub const ALL: u16 = 0xffff;

    /// Compose the badge word a server receives.
    pub const fn wire(perms: u16, id: u64) -> u64 {
        ((perms as u64) << SHIFT) | (id & ID_MASK)
    }
    /// Permission mask of a received badge word.
    pub const fn perms_of(wire: u64) -> u16 {
        (wire >> SHIFT) as u16
    }
    /// Object id of a received badge word.
    pub const fn id_of(wire: u64) -> u64 {
        wire & ID_MASK
    }
    /// `true` if `id` fits the id field (a tagged capability rejects wider badges).
    pub const fn id_fits(id: u64) -> bool {
        id & !ID_MASK == 0
    }
}

/// **Send-time channel rule for endpoint capabilities** (`CHAN = 38`, 2026-10-09).
///
/// A channel-restricted cap carries `(value, mask)`; a `CALL`/`CALL_TIMEOUT` through it
/// passes only if `(channel & mask) == value`, where `channel` is the low 32 bits of the
/// caller's `x6` tag word. The check is the *same* shape as an MMIO address decode
/// (`(addr & mask) == value`): one interval or one exact channel per rule, constant time,
/// no lists in the kernel.
///
/// ```text
/// mask = 0xFFFF_FFFF, value = n   → exactly channel n
/// mask = 0xFFFF_FF00, value = c   → the 256-channel block starting at c
/// mask = 0, any value             → refused at derivation (a rule that matches
///                                   everything or nothing is a caller bug, not a rule)
/// ```
///
/// Identity is cap identity: the server already receives the unforgable badge word
/// (`perms << 48 | id`, see [`ipc_perm`]), and the tag word (including the channel) is
/// copied to it. There is deliberately no separate "sender PD" word — authority is the
/// cap held, not the speaker.
///
/// Out of scope, named: `SIGNAL` carries no per-signal word (badge-OR semantics), so its
/// badge stays the coarse channel; queue-full and timing remain observable by design.
pub mod ipc_chan {
    /// A channel rule: `(channel & mask) == value` passes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Rule {
        /// Required value after masking.
        pub value: u32,
        /// Significant channel bits. Never zero (enforced at derivation).
        pub mask: u32,
    }

    impl Rule {
        /// `true` if `channel` passes this rule. Pure, total, branchless-comparable —
        /// this is the function the Verus model pins down.
        pub const fn allows(self, channel: u32) -> bool {
            (channel & self.mask) == self.value
        }

        /// `true` if the stored rule is well-formed (mask non-zero and value within mask).
        /// `audit_cdt` code 10 checks this over the live table.
        pub const fn is_wellformed(self) -> bool {
            self.mask != 0 && (self.value & !self.mask) == 0
        }
    }

    /// Extract the channel from a `CALL` tag word (low 32 bits of `x6`).
    pub const fn channel_of_tag(tag: u64) -> u32 {
        tag as u32
    }
}

/// **Wie viele Caps ein [`sys::LOAD`] hoechstens delegieren kann** (2026-08-25).
///
/// Acht, und die Zahl ist **hergeleitet**, nicht gewaehlt: mehr als `CAP_BUDGET_PER_PD` kann eine
/// PD ohnehin nicht halten. Eine Delegationsliste, die groesser sein darf als das Budget des
/// Empfaengers, verspricht etwas, das die Installation gleich wieder abweist.
///
/// Genau acht Paare passen ausserdem in **ein** Nachrichtenwort (je 4 Bit fuer Quell- und
/// Zielslot, `NCAPS = 16`) — die Liste braucht damit keinen Zeiger in den Aufrufer-Speicher, und
/// der Kernel liest keine fremden Bytes.
pub const LOAD_MAX_DELEGATES: usize = 8;

/// Ein Paar (Quell-Slot im Aufrufer, Ziel-Slot in der neuen PD) in ein Byte packen.
///
/// **Der Ziel-Slot ist frei waehlbar und nicht die Identitaet**, und das ist eine Entscheidung:
/// die Slot-Belegung der neuen PD ist ihre Loader-ABI (0 Loader, 1 Notification, 2 Endpoint,
/// 3..6 Geraet). Ein Aufrufer, der nur aus seinem eigenen Slot heraus delegieren koennte, muesste
/// seinen Cspace nach der Erwartung des Kindes ordnen — bei acht Slots je PD ist das keine
/// Freiheit, sondern eine Kollision.
pub const fn delegate_pair(src: u8, dst: u8) -> u8 {
    ((src & 0x0f) << 4) | (dst & 0x0f)
}

/// Quell- und Ziel-Slot aus einem gepackten Byte lesen.
pub const fn delegate_unpack(b: u8) -> (usize, usize) {
    ((b >> 4) as usize, (b & 0x0f) as usize)
}

/// **Pack the `x1` word of [`sys::SPAWN`]** — a sub-region of the stack Cap, in pages (K1b).
///
/// `spawn_sub(0, 0)` is `0` and means *the whole region*, which is what every call written before
/// the sub-region existed encodes. The compatibility lives in the encoding rather than in a branch
/// somebody has to remember.
pub const fn spawn_sub(offset_pages: u32, length_pages: u32) -> u64 {
    ((offset_pages as u64) << 32) | (length_pages as u64)
}

/// **Obergrenze des DMA-Pools einer Geraetezuteilung, in Seiten** (C2, 2026-08-26).
///
/// Sie steht in der **ABI** und nicht nur im Kernel, weil eine Schnittstelle, die oberhalb von `N`
/// abweist, `N` veroeffentlichen muss — sonst ist die Absage fuer den Aufrufer ein Ratespiel, und
/// die naheliegende Reaktion (halbieren und nochmal) ist genau die Kuerzung, die hier vermieden
/// wird.
///
/// **1024 Seiten = 4 MiB je Zuteilung.** Die Zahl ist nicht frei: der Kernel haelt hoechstens vier
/// gleichzeitige Zuteilungen, alle vier muessen aus GiB 0 bedienbar bleiben (dort liegt der einzige
/// Bereich, den `vspace_map_dma` abbilden kann), und auf der kleinsten gefahrenen Maschine
/// (`-m 512M`) sind 4 x 4 MiB ein Anteil, den der Ladepfad daneben noch traegt.
pub const DRIVER_DMA_MAX_PAGES: u64 = 1024;

/// **Obergrenze der Adressen, die ein Programm nennen darf** — die untere Adresshaelfte.
///
/// `2^47`, und die Zahl ist mit Absicht **kleiner** als jede Architekturgrenze: auf x86-64 endet
/// die kanonische untere Haelfte bei `2^47`, auf aarch64 spannt `TTBR0` bis `2^48`. Wer die
/// kleinere nimmt, ist auf beiden richtig — und jedes User-Fenster dieses Kernels liegt weit
/// darunter (`ISO_USER_VA` = 512 GiB auf x86, 9 GiB auf aarch64).
///
/// **Warum das eine ABI-Groesse ist und keine Kernel-Interna:** sie sagt, welche Adressen ein
/// Programm dem Kernel gegenueber **nennen** darf. Ob dort etwas abgebildet ist, ist eine andere
/// Frage und wird woanders beantwortet.
pub const USER_VA_TOP: u64 = 1 << 47;

/// **Das `x5`-Wort von [`sys::LOAD`] packen** — DMA-Seiten und Cap-Budget in EINEM Register.
///
/// `load_extras(0, 0) == 0` heisst „beides Vorgabe" und ist bitgleich zu jedem Aufruf, den es vor
/// diesen Feldern gab. Die Vertraeglichkeit steckt in der Kodierung, nicht in einem Zweig, den
/// jemand im Kopf behalten muss.
pub const fn load_extras(dma_pages: u32, cap_budget: u16) -> u64 {
    ((dma_pages as u64) << 16) | (cap_budget as u64)
}

/// `(dma_pages, cap_budget)` aus dem `x5`-Wort von [`sys::LOAD`] lesen.
///
/// `dma_pages` wird **ungekappt** zurueckgegeben: eine absurde Zahl soll die benannte Absage
/// ausloesen und nicht durch eine Maske zu einer plausiblen werden. Genau das war der Fehler, den
/// `spawn_sub` in seiner ersten Fassung fast gemacht haette.
pub const fn load_extras_unpack(v: u64) -> (u64, u16) {
    (v >> 16, (v & 0xffff) as u16)
}

/// Read `(offset_pages, length_pages)` back out of the `x1` word of [`sys::SPAWN`].
///
/// Returns `(0, 0)` for `0`, which the kernel reads as *the whole region* — the two halves are
/// **never** interpreted separately, because a request naming a length of zero pages at a non-zero
/// offset would otherwise silently become "the whole Cap from the base".
pub const fn spawn_sub_unpack(v: u64) -> (u64, u64) {
    (v >> 32, v & 0xffff_ffff)
}

/// Register-Indizes im TrapFrame (`gpr[i]` == `xi`).
pub mod reg {
    /// Eintritt: Syscall-Nummer · Austritt: Ergebniscode.
    pub const SYSNO_RESULT: usize = 0;
    /// Eintritt: Endpoint-ID · Austritt: Badge des Senders.
    pub const EP_BADGE: usize = 1;
    /// Erstes Nachrichten-Datenwort (`x2`); weitere folgen aufsteigend.
    pub const MSG0: usize = 2;
    /// Zweites Nachrichten-Datenwort (`x3`).
    pub const MSG1: usize = 3;
    /// Drittes Nachrichten-Datenwort (`x4`).
    pub const MSG2: usize = 4;
    /// Viertes Nachrichten-Datenwort (`x5`).
    pub const MSG3: usize = 5;
    /// Nachrichten-Tag/Label (`x6`).
    pub const TAG: usize = 6;
}

/// Ergebniscodes (Register `x0` beim Austritt).
pub mod result {
    pub const OK: u64 = 0;
    /// Kein gültiger Capability an der angegebenen Stelle / falscher Objekttyp.
    pub const ERR_BADCAP: u64 = 1;
    /// Unbekannte Syscall-Nummer.
    pub const ERR_BADSYS: u64 = 2;
    /// Capability hat nicht die nötigen Rechte für diese Operation.
    pub const ERR_RIGHTS: u64 = 3;
    /// Aufrufer gehört zu keiner Protection Domain.
    pub const ERR_NOPD: u64 = 4;
    /// Operation auf einem Cap, von dem noch Kopien/Mints abgeleitet sind (CDT-Kinder). Der Cap
    /// bleibt unverändert im Slot — ein halb entfernter Cap wäre schlimmer als gar keiner.
    pub const ERR_HASCHILDREN: u64 = 6;
    /// Kein Platz: der Ziel-Slot ist belegt, oder das Cap-Budget der PD ist erschöpft.
    pub const ERR_NOSPACE: u64 = 7;
    /// **Antwort-seitiger Liveness-Fehler:** der Server, der eine Antwort schuldete
    /// (Reply-Owner), ist verschwunden (KILL/EXIT/Fault/Reload), bevor er antworten
    /// konnte. Der blockierte `CALL`-Aufrufer wird damit entblockt, statt dauerhaft zu
    /// hängen — der Client kann den Fehler behandeln (Retry/Abbruch).
    pub const ERR_SERVER_GONE: u64 = 5;
    /// **Der Endpoint wird gerade stillgelegt** (A-4.2, ruhender Punkt): ein Austausch der
    /// Server-Instanz läuft, deshalb wird keine *neue* Transaktion mehr eröffnet. Laufende
    /// Transaktionen dürfen abschliessen (`REPLY` bleibt erlaubt) — abgewiesen werden nur
    /// `CALL` und `RECV`.
    ///
    /// Der Unterschied zu [`ERR_SERVER_GONE`] ist der, auf den es ankommt: dort ist eine
    /// **begonnene** Transaktion verloren, hier ist eine **nicht begonnene** abgewiesen. Ein
    /// Client darf hierauf gefahrlos wiederholen, sobald der Austausch durch ist; dort muss er
    /// wissen, ob der Server die Wirkung schon hatte. Denselben Code für beides zu nehmen
    /// hiesse, dem Client diesen Unterschied zu verschweigen.
    pub const ERR_QUIESCING: u64 = 8;
    /// **Die Warteschlange dieses Endpoints ist voll** (D11): mehr als `QUEUE_CAP` Threads
    /// können an *einem* Endpoint nicht zugleich blockieren.
    ///
    /// Vor diesem Code gab es die Lage nicht als Antwort, sondern als Loch: der 33. Sender
    /// wurde **trotzdem** blockiert, landete in keiner Struktur des Endpoints, bekam keinen
    /// Ergebniscode und wurde nie geweckt — und `is_quiescent()` meldete ihn als *ruhig*.
    /// Ein Thread ging verloren, und jeder Prüfer meldete Ordnung.
    ///
    /// **Warum ein dritter Code und nicht [`ERR_QUIESCING`]** (die Frage, die D11 offenließ):
    /// die drei Lagen verlangen vom Client verschiedene Antworten. [`ERR_BADCAP`] heisst „gibt
    /// es nicht" — nie wieder versuchen. `ERR_QUIESCING` heisst „kommt gleich wieder" — warten,
    /// bis der Austausch durch ist; die Wartezeit ist durch den Austausch begrenzt und hängt
    /// nicht am Verhalten anderer Clients. `ERR_EP_FULL` heisst „gerade kein Platz" — das ist
    /// eine **Lastaussage**: sie hängt an den anderen 32 Wartenden, kann sofort wieder gelten,
    /// und ein Client, der stumpf wiederholt, verschärft sie. Wer die beiden zusammenwürfe,
    /// nähme dem Client genau die Unterscheidung, die A-4.2 zwischen `ERR_QUIESCING` und
    /// `ERR_BADCAP` gerade eingeführt hat.
    pub const ERR_EP_FULL: u64 = 9;
    /// **Die gewünschte Handler-Bindung schlösse einen Kreis** (Z26/A3, Nachtrag 3): der Handler
    /// wird — direkt oder über eine Kette — selbst von der Gast-PD behandelt. Das ist ein
    /// **Deadlock per Konstruktion**: der Handler kann sein eigenes `RECV` nicht absetzen, ohne
    /// dass es umgeleitet wird, und zwar an einen Thread, der auf ihn wartet.
    ///
    /// **Warum das im Kernel steht und nicht in der Doku:** ein Kreis erzeugt kein Fehlerbild,
    /// sondern Stille. Beide PDs sind blockiert, `is_quiescent()` meldet Ruhe, kein Audit-Code
    /// schlägt an — dasselbe Bild wie der 33. Sender vor D11. Eine Regel, die nur aufgeschrieben
    /// ist, ist bei dieser Fehlerform keine Regel.
    pub const ERR_HANDLER_CYCLE: u64 = 10;
    /// **Der Handler ist weggefallen** (Cap gelöscht, entzogen, PD tot) — der Gast **faultet**
    /// damit, statt auf die native ABI zurückzufallen.
    ///
    /// Der Rückfall wäre aus dem Entzug einer Cap eine **Beförderung**: der Gast spräche plötzlich
    /// direkt mit dem Caprock-Kernel, mit genau der Autorität, die die Bindung ihm genommen hatte.
    /// Fail-closed, und es ist dieselbe Form wie [`ERR_SERVER_GONE`] beim Endpoint-Austausch —
    /// nur schwerer, weil dort eine Transaktion verloren ist und hier eine Autoritätsgrenze.
    pub const ERR_HANDLER_GONE: u64 = 11;
    /// **Die Gast-PD hat bereits einen anderen Handler.** Eine PD ist EIN Adressraum und hat
    /// höchstens EINEN Kernel; zwei Persönlichkeiten darüber wären zwei Wahrheiten über denselben
    /// Speicher. Dieselbe Bindung noch einmal ist [`OK`] (idempotent — ein zweiter Thread
    /// derselben Gast-PD).
    ///
    /// **Nicht [`ERR_NOSPACE`]:** dort ist etwas voll und wird wieder frei, hier liegt eine
    /// **Entscheidung** vor, die jemand zurücknehmen muss. Ein Aufrufer, der die beiden nicht
    /// unterscheiden kann, wiederholt in einem Fall sinnvoll und im anderen für immer.
    pub const ERR_HANDLER_BUSY: u64 = 12;
    /// **Die Auftragsschlange des Verifiziererthreads ist voll** (C8).
    ///
    /// Seit C8 läuft die Signaturprüfung eines [`super::sys::LOAD`] nicht mehr auf dem
    /// 16-KiB-Kernel-Stack des *aufrufenden* Threads, sondern auf dem eigenen Stack eines
    /// dedizierten Verifiziererthreads. Das serialisiert Ladevorgänge — und Serialisierung ist ein
    /// **Kanal**: eine PD, die `SYS_LOAD` spammt, verzögert fremde Ladevorgänge.
    ///
    /// Deshalb hat die Schlange eine Schranke, **und die Schranke hat einen Namen**. Genau die
    /// Lehre aus [`ERR_EP_FULL`]: wer eine Kapazität einführt und den Überlauf nicht benennt, hat
    /// keinen Schutz gebaut, sondern ein Loch — der Überzählige wurde dort blockiert, stand in
    /// keiner Struktur und wurde nie geweckt.
    ///
    /// **Der Überläufer bleibt lauffähig.** Er wird *nicht* blockiert; er bekommt diesen Code und
    /// kehrt aus dem Syscall zurück. Wie [`ERR_EP_FULL`] ist das eine **Lastaussage**: sie hängt am
    /// Verhalten anderer, kann sofort wieder gelten, und ein Client, der stumpf wiederholt,
    /// verschärft sie. Nicht zu verwechseln mit [`ERR_SERVER_GONE`] — das sagt der Kernel, wenn es
    /// den Verifizierer gar nicht gibt, und das ist keine Frage der Last, sondern des Aufbaus.
    pub const ERR_LOAD_BUSY: u64 = 13;
    /// **Das Sidecar trägt ein Format, das dieser Kernel nicht lesen darf** (Z26/A3).
    ///
    /// Der Slot hat einen **versionierten Kopf** (Magie, Formatversion, Architektur, Wortzahlen,
    /// reservierte Felder). Passt er nicht, wird **nichts** in den Frame des Gastes übernommen und
    /// er bekommt diesen Code — statt dass der Kernel fremde Bytes im eigenen Sinn ausliest und
    /// als **Registerinhalt** in einen laufenden Thread schreibt.
    ///
    /// Das ist wörtlich die Regel aus A-4.3 (Zustandsübergabe): was drüben nicht dasselbe
    /// bezeichnen kann, wird abgewiesen, nicht ausgelegt. Und es ist ein **anderer** Fall als
    /// [`ERR_HANDLER_GONE`]: dort ist niemand da, hier ist jemand da und redet eine andere
    /// Fassung. Ein gemeinsamer Code zwänge den Betreiber zu raten, ob er auf jemanden wartet
    /// oder Fassungen abgleichen muss.
    pub const ERR_HANDLER_ABI: u64 = 14;

    // --- K1a (`SYS_SPAWN`), 2026-08-17 --------------------------------------------------------
    //
    // Four codes, not one. *Whoever introduces a capacity must NAME the overflow* (D11): a
    // blanket refusal makes "your stack is too small" and "a device can reach your stack"
    // indistinguishable, and the second is an attack while the first is a typo.

    /// The PD is already at its thread limit. **The caller is NOT blocked** — it gets this code
    /// and keeps running. That is D11 literally: the 33rd sender that was blocked without a code,
    /// stood in no structure and was reported as *quiescent* is the failure mode this avoids.
    pub const ERR_THREAD_LIMIT: u64 = 15;

    /// The proposed stack region is **reachable by a device** (it lies in a DMA window).
    ///
    /// A stack a device can write is the `by ops` placement rule as an attack: the return address
    /// is data, and whoever can write it chooses where the thread goes next. Refused at the edge,
    /// not audited afterwards.
    pub const ERR_DMA_REACHABLE: u64 = 16;

    /// The stack region is too small or badly aligned. Its own code, because it is a **caller
    /// mistake** and must be distinguishable from the two above at a glance.
    pub const ERR_BADSTACK: u64 = 17;

    /// The Cap is bound as a live thread's stack and therefore cannot be deleted.
    ///
    /// The named price of taking the stack from a Cap of the caller (see [`super::sys::SPAWN`]):
    /// the TCB holds a reference, and a reference that blocks deletion is **countable**, whereas
    /// "deletion kills the thread with it" would be a group operation across `CAPS` and
    /// `SCHEDS[core]`. Kill the thread first, then the Cap deletes normally.
    pub const ERR_INUSE: u64 = 18;

    /// **The target is already stopped by another `DebugControl` holder** (Z6b).
    ///
    /// The named overflow of a capacity of exactly one. `DEBUG` is a bit in `BlockReasons` and a
    /// bit has no refcount, so the stop has one owner — and the second asker is **refused**, not
    /// queued and not silently accepted. Silently accepting would give two debuggers each the
    /// belief that their `DEBUG_CONTINUE` releases the target; the first one to call it would
    /// release it under the other.
    pub const ERR_DEBUG_BUSY: u64 = 19;

    /// **No `Debuggable` was ever minted for this PD** (Z6b).
    ///
    /// Deliberately **distinct from [`ERR_BADCAP`]**, and the distinction is the product claim:
    /// `ERR_BADCAP` says *you* do not hold the authority, this says the authority **does not exist
    /// and cannot be obtained** — not by the caller, not by the operator, not by anyone holding
    /// every other capability in the system. A single code for both would make the two
    /// indistinguishable from outside, and then „diese PD ist nicht debuggbar" would be a claim
    /// about the caller instead of about the system.
    pub const ERR_NOT_DEBUGGABLE: u64 = 20;

    /// **The named sub-region does not lie inside the stack Cap** (K1b, 2026-08-26).
    ///
    /// Deliberately **not** [`ERR_BADSTACK`]. That code says *the region you named is unusable as
    /// a stack* (too small, misaligned); this one says *you named a region you do not hold*. The
    /// two have different fixes, and the second is the shape an attacker produces: a Cap over
    /// 64 KiB plus an offset of 4 GiB is a request for somebody else's memory, not a typo. A
    /// shared code would put both in the same log line and make the interesting one invisible.
    pub const ERR_SUBREGION: u64 = 21;

    /// **Der angeforderte DMA-Pool ist groesser, als eine Geraetezuteilung traegt** (C2,
    /// 2026-08-26) — s. [`super::DRIVER_DMA_MAX_PAGES`].
    ///
    /// Eigener Code, und das ist die ganze Aussage: die Alternative waere eine **gekuerzte**
    /// Region, und eine stillschweigend halbierte DMA-Region ist ein Geraet, das ueber ihr Ende
    /// hinausschreibt. *Wer eine Kapazitaet einfuehrt, muss den Ueberlauf benennen* (D11) — und
    /// der Aufrufer bekommt den Code, ohne blockiert zu werden.
    ///
    /// Er ist **nicht** [`ERR_BADCAP`] (der Sammelcode eines fehlgeschlagenen Ladevorgangs): „du
    /// hast mehr verlangt, als es gibt" ist wiederholbar mit einer kleineren Zahl, „das Archiv
    /// oder das ELF taugt nicht" ist es nicht.
    pub const ERR_DMA_TOO_LARGE: u64 = 22;

    /// **Kein Platz mehr fuer eine Interrupt-Bindung** ([`super::sys::BIND_IRQ`], Stufe B).
    ///
    /// Die Aussage ist **lokal** und das ist die Entscheidung dahinter (E12): *dieses Geraet hat
    /// keinen freien Vektor.* Die Bindung haengt an der Zuteilung und nicht an einem globalen
    /// Konto — wer ein Geraet hat, hat genau dessen Bindungen, und kein Treiber-PD kann einem
    /// anderen die Ressource wegnehmen, weil die Zuteilung schon die Vergabestelle ist.
    ///
    /// *Wer eine Kapazitaet einfuehrt, muss den Ueberlauf benennen* (D11): der Aufrufer bekommt
    /// den Code und wird **nicht** blockiert — dieselbe Form wie [`ERR_EP_FULL`] und
    /// [`ERR_LOAD_BUSY`].
    pub const ERR_IRQ_FULL: u64 = 23;

    /// **Der Thread-Pointer liegt nicht in der unteren Adresshaelfte** ([`super::sys::SETTLS`]).
    ///
    /// Eigener Code, weil die Absage einen eigenen Grund hat und kein Sammel-Nein ist: „diese
    /// Adresse darf ich nicht schreiben" ist mit einer anderen Zahl wiederholbar, „dich gibt es
    /// nicht" ([`ERR_BADCAP`]) nicht.
    ///
    /// Die Schranke schuetzt den **Kernel**, nicht den Aufrufer: ein `WRMSR` auf `IA32_FS_BASE`
    /// mit nicht-kanonischem Wert faultet in Ring 0.
    pub const ERR_BADTLS: u64 = 24;

    /// **Die Frist ist abgelaufen** (Stufe A / A2).
    ///
    /// Der Aufrufer hat mit einer Frist gewartet und ist geweckt worden, **ohne** dass das
    /// Ereignis eintrat. Ein eigener Code, weil „nichts kam" und „es kam etwas" fuer den Aufrufer
    /// verschiedene Fortsetzungen haben — und weil ein Treiber, der `ERR_TIMEOUT` als
    /// *„Geraet tot"* liest, waehrend die Antwort gerade zugestellt wurde, ein **Korruptionspfad**
    /// ist und kein Haenger. Diese Kante ist benannt und noch nicht gemessen (todo A2c).
    pub const ERR_TIMEOUT: u64 = 25;

    /// **Das Teardown-Token passt nicht zur PD-Epoche** (EXEC-Replace, Prozessmodell).
    ///
    /// Eigener Code, und das ist die ganze Teardown-Token-Form: `0` heisst „kein Token"
    /// und wird hier abgewiesen, nicht als „egal" gelesen. Wer `ERR_BADCAP` naehme,
    /// sagte „dich gibt es nicht" ueber eine Lage, die „du hast den alten Stand"
    /// heisst — und ein Aufrufer, der die beiden nicht unterscheiden kann, wiederholt
    /// mit demselben alten Token fuer immer.
    pub const ERR_STALE_TOKEN: u64 = 26;

    /// **Der Schnappschuss passt nicht in die benannte Schranke** (FORK, Phase 1).
    ///
    /// Eigener Code nach D11: die Schranke ist `fork::SNAPSHOT_MAX_BYTES` (volle Kopie,
    /// keine COW-Ankuendigung). Gekuerzt zu kopieren hiesse, dem Kind einen
    /// halb adressierten Raum zu geben — still halbiert ist hier ein Korruptionspfad,
    /// kein Komfort.
    pub const ERR_SNAPSHOT_LIMIT: u64 = 27;
}

/// **Schranken und Kodierung des Prozessmodells (FORK/EXEC, Phase 1).**
///
/// Reine Konstanten, keine Logik — die Pruefung steht in `caprock-loader` (`fork`,
/// `exec`) und im Kernel-Dispatch (Patch-Text, s. `caprock-microkit::proc`).
pub mod fork {
    /// Obergrenze der Kopierlaenge eines FORK-Schnappschusses in Bytes (Phase 1).
    ///
    /// 8 MiB: viermal die groesste heute gefahrene User-Flaeche (16-KiB-Stack +
    /// wenige Segmente), klein genug, dass die Kopierschleife unter der
    /// Sperrhaltedauer-Marke bleibt (Aufrufer schleift, preemptibel).
    pub const SNAPSHOT_MAX_BYTES: u64 = 8 * 1024 * 1024;
    /// Naechste freie Syscall-Nummer nach FORK/EXEC + Debugger-v2 + LOAD_IMAGE + CSUB + CHAN.
    /// `39` ist frei (`38` ist seit 2026-10-09 `CHAN`); `4` bleibt historische Luecke, nie vergeben.
    pub const NAECHSTE_FREIE_SYSCALL: u64 = 39;
}

// --- Host-nahe Pruefung der Nummernvergabe (A2-Rest) ------------------------------------------
//
// `caprock-abi` ist abhaengigkeitsfrei und laeuft ueber `rustc --test` in Sekunden (derselbe Weg
// wie `tools/host-tests.sh`, Ziel `einzeln`). Was hier steht, ist keine Logik, sondern die
// einzige Stelle, an der eine Kollision zweier Syscall-Nummern auffiele, bevor sie ein
// lauffaehiges Image baut: zwei gleiche Nummern waeren im Dispatch kein Fehler, sondern ein
// toter Syscall.
#[cfg(test)]
mod nummern {
    use super::*;

    #[test]
    fn ipc_perm_wire_roundtrip_and_layout() {
        use super::ipc_perm::*;
        let w = wire(DUP | 0b101, 0xabcd_1234);
        assert_eq!(perms_of(w), DUP | 0b101);
        assert_eq!(id_of(w), 0xabcd_1234);
        // the id never leaks into the permission half
        assert_eq!(wire(0, u64::MAX), ID_MASK);
        assert!(!id_fits(1 << 48));
        assert!(id_fits(ID_MASK));
        // kernel bits and service bits are disjoint and cover the word
        assert_eq!(DUP & GRANT, 0);
        assert_eq!((DUP | GRANT | SERVICE_MASK), ALL);
    }

    #[test]
    fn park_timeout_ist_die_naechste_freie_nummer() {
        // `CLOCK = 28` war die hoechste vergebene Nummer; `PARK_TIMEOUT` folgt direkt.
        assert_eq!(sys::CLOCK, 28);
        assert_eq!(sys::PARK_TIMEOUT, 29);
    }

    #[test]
    fn debugger_v2_syscalls_liegen_hinter_call_timeout() {
        // `CALL_TIMEOUT = 30`; `31`/`32` sind seit Prozessmodell FORK/EXEC; v2 liegt dahinter.
        assert_eq!(sys::CALL_TIMEOUT, 30);
        assert_eq!(sys::FORK_SNAPSHOT, 31);
        assert_eq!(sys::EXEC_REPLACE, 32);
        assert_eq!(sys::DEBUG_WRITE_MEM, 33);
        assert_eq!(sys::DEBUG_SINGLE_STEP, 34);
        assert_eq!(sys::DEBUG_HWBREAK, 35);
        assert_eq!(super::debug::WRITE_MAX, super::debug::READ_MAX);
    }

    #[test]
    fn fork_exec_belegen_die_luecke_38_bleibt_frei() {
        // Prozessmodell: die Luecke `31`/`32` ist geschlossen, `36` ist LOAD_IMAGE,
        // `37` ist CSUB, `38` ist seit 2026-10-09 CHAN, `39+` bleibt frei.
        // `4` bleibt historische Luecke, nie vergeben.
        // Kollision = Baufehler: faellt dieser Test, ist eine Nummer doppelt
        // vergeben (s. naechsten Test).
        assert_eq!(sys::FORK_SNAPSHOT, 31);
        assert_eq!(sys::EXEC_REPLACE, 32);
        assert_eq!(sys::LOAD_IMAGE, 36);
        assert_eq!(sys::CSUB, 37);
        assert_eq!(sys::CHAN, 38);
        assert_eq!(super::fork::NAECHSTE_FREIE_SYSCALL, 39);
        assert_eq!(super::fork::SNAPSHOT_MAX_BYTES, 8 * 1024 * 1024);
        // Kein bekannter Syscall liegt auf/ueber 38 — wuerde einer hinzukommen, ohne
        // diesen Test zu erweitern, schwiege die Einmaligkeitspruefung nicht, aber die
        // „naechste freie"-Aussage waere falsch. Deshalb steht die Aufzaehlung hier.
        for n in [
            sys::YIELD,
            sys::CALL,
            sys::RECV,
            sys::REPLY,
            sys::PARK,
            sys::EXIT,
            sys::KILL,
            sys::SIGNAL,
            sys::WAIT,
            sys::MAP,
            sys::UNMAP,
            sys::PDCTL,
            sys::LOAD,
            sys::CDELETE,
            sys::CCOPY,
            sys::CMOVE,
            sys::SETRECV,
            sys::UNPARK,
            sys::SETHANDLER,
            sys::SPAWN,
            sys::DEBUG_ATTACH,
            sys::DEBUG_STOP,
            sys::DEBUG_CONTINUE,
            sys::DEBUG_READ_MEM,
            sys::DEBUG_WRITE_REGS,
            sys::DEBUG_WRITE_MEM,
            sys::DEBUG_SINGLE_STEP,
            sys::DEBUG_HWBREAK,
            sys::BIND_IRQ,
            sys::SETTLS,
            sys::CLOCK,
            sys::PARK_TIMEOUT,
            sys::CALL_TIMEOUT,
            sys::FORK_SNAPSHOT,
            sys::EXEC_REPLACE,
            sys::LOAD_IMAGE,
            sys::CSUB,
            sys::CHAN,
        ] {
            assert!(n < 39, "Syscall-Nummer {n} liegt auf/ueber der naechsten freien 39");
        }
    }

    #[test]
    fn jede_syscall_nummer_ist_einmalig() {
        let alle = [
            sys::YIELD,
            sys::CALL,
            sys::RECV,
            sys::REPLY,
            sys::PARK,
            sys::EXIT,
            sys::KILL,
            sys::SIGNAL,
            sys::WAIT,
            sys::MAP,
            sys::UNMAP,
            sys::PDCTL,
            sys::LOAD,
            sys::CDELETE,
            sys::CCOPY,
            sys::CMOVE,
            sys::SETRECV,
            sys::UNPARK,
            sys::SETHANDLER,
            sys::SPAWN,
            sys::DEBUG_ATTACH,
            sys::DEBUG_STOP,
            sys::DEBUG_CONTINUE,
            sys::DEBUG_READ_MEM,
            sys::DEBUG_WRITE_REGS,
            sys::DEBUG_WRITE_MEM,
            sys::DEBUG_SINGLE_STEP,
            sys::DEBUG_HWBREAK,
            sys::BIND_IRQ,
            sys::SETTLS,
            sys::CLOCK,
            sys::PARK_TIMEOUT,
            sys::CALL_TIMEOUT,
            sys::FORK_SNAPSHOT,
            sys::EXEC_REPLACE,
            sys::LOAD_IMAGE,
            sys::CSUB,
            sys::CHAN,
        ];
        let mut sortiert = alle;
        sortiert.sort_unstable();
        let mut i = 0;
        while i + 1 < sortiert.len() {
            assert_ne!(
                sortiert[i],
                sortiert[i + 1],
                "Syscall-Nummer doppelt vergeben: {}",
                sortiert[i]
            );
            i += 1;
        }
    }

    #[test]
    fn chan_channel_rule_is_pure_and_total() {
        use super::ipc_chan::{channel_of_tag, Rule};
        // exact channel
        let only7 = Rule { value: 7, mask: 0xFFFF_FFFF };
        assert!(only7.allows(7));
        assert!(!only7.allows(8));
        assert!(only7.is_wellformed());
        // 256-channel block
        let block = Rule { value: 0x1200, mask: 0xFF00 };
        assert!(block.allows(0x12AB));
        assert!(!block.allows(0x1300));
        assert!(block.is_wellformed());
        // mask 0 is never well-formed (matches-all or matches-nothing by accident)
        assert!(!Rule { value: 0, mask: 0 }.is_wellformed());
        // value bits outside the mask are a caller bug, not a rule
        assert!(!Rule { value: 0x1_0000, mask: 0xFFFF }.is_wellformed());
        // channel comes from the low 32 bits of the CALL tag word; high bits
        // (including GRANT_FLAG) are not part of the channel
        assert_eq!(channel_of_tag(0x8000_0000_0000_0042), 0x42);
        assert_eq!(channel_of_tag(u64::MAX), u32::MAX);
    }

    #[test]
    fn frist_rueckgabe_ist_benannt() {
        // `PARK_TIMEOUT` braucht genau diese beiden Ausgaenge, und beide muessen verschieden
        // sein — sonst kann der Aufrufer „geweckt" von „abgelaufen" nicht unterscheiden.
        assert_ne!(result::OK, result::ERR_TIMEOUT);
    }

    #[test]
    fn selbst_loeschen_ist_benannt_abgesichert() {
        // A4 ist durch `CDELETE = 14` geschlossen: eigener Slot im eigenen Cspace, kein Cap
        // noetig, fremde/leere Slots → `ERR_BADCAP`. Die vier Absagen muessen vier verschiedene
        // Codes sein — sonst koennte der Aufrufer „dort liegt nichts von dir" nicht von „du
        // hast Kinder daraus abgeleitet" oder „der Thread lebt noch" unterscheiden.
        assert_eq!(sys::CDELETE, 14);
        assert_ne!(result::OK, result::ERR_BADCAP);
        assert_ne!(result::ERR_BADCAP, result::ERR_NOPD);
        assert_ne!(result::ERR_BADCAP, result::ERR_HASCHILDREN);
        assert_ne!(result::ERR_BADCAP, result::ERR_INUSE);
        assert_ne!(result::ERR_HASCHILDREN, result::ERR_INUSE);
        // Geprueft am 2026-09-09: `30` ist seit A2-Rest als `CALL_TIMEOUT` vergeben
        // (`4` ist eine historische Luecke, nie vergeben). `31`/`32` sind seit
        // Prozessmodell FORK/EXEC vergeben, `36` seit LXPD-Laufzeit LOAD_IMAGE;
        // naechste freie Nummer: `37`.
        assert_eq!(sys::PARK_TIMEOUT, 29);
        assert_eq!(sys::CALL_TIMEOUT, 30);
        assert_eq!(sys::FORK_SNAPSHOT, 31);
        assert_eq!(sys::EXEC_REPLACE, 32);
        assert_ne!(result::ERR_STALE_TOKEN, result::ERR_BADCAP);
        assert_ne!(result::ERR_SNAPSHOT_LIMIT, result::ERR_NOSPACE);
    }
}
