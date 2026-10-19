# Caprock — Systeminvarianten (Konsolidierung, ext-22…ext-28)

Dieses Dokument macht die bis ext-25 **impliziten** Invarianten explizit: die Sperrordnung, die
DMA-Revoke-Reihenfolge, die Region-Balance, den `RegionView`/`Pod`-Sicherheitsvertrag und das
SMMU-unter-QEMU-Verhalten. Es ist die Referenz für die **formale Verifikation** (welche Eigenschaft
trägt welche Isolations-/Sicherheitsaussage) und für jede künftige Erweiterung (welche Ordnung darf
nicht gebrochen werden). Wo eine Invariante maschinell geprüft wird, ist das Audit/der Test genannt.

## 1. Sperrordnung (Lock-Hierarchie)

Alle globalen Locks haben einen **Rang**. Ein Pfad darf Locks nur mit **streng steigendem** Rang
schachteln; `MEM` ist **innerster** Lock (hält nie einen weiteren). Verklemmungsfreiheit folgt aus
der Azyklizität dieser totalen Ordnung.

| Rang | Lock(s) | Typ | Rolle |
|---|---|---|---|
| R0 | `CAPS` | `RwSpinLock<Caps>` | Capability-Space + PDs (Autorität) |
| R1 | `EPS[]`, `NTFNS[]`, `VSPACES`, `DMA_CTX` | `SpinLock` | Ressourcentabellen (je Objekt/global) |
| R1.5 | `verifizierer::SCHLANGE` | `SpinLock<Schlange>` | C8: Auftragsschlange des Verifiziererthreads |
| R2 | `SCHEDS[core]` | `SpinLock<Scheduler>` | Per-Kern-Runqueue |
| R2.5 | `FP_STATES` | `SpinLock<[FpState; N]>` | Lazy-FP-Kontexte — **stets UNTER `SCHEDS` genommen** (nie davor), hält selbst nichts weiter |
| R3 | `Heap.inner` | `SpinLock<HeapInner>` | prozess-lokaler Allokator (nur Region-Runtime) |
| R4 | `MEM` | `SpinLock<PhysAllocator>` | physischer Allokator — **innerster** |

**Ergänzung ext-30 (Migration):** eine Migration hält **zwei** `SCHEDS`-Locks gleichzeitig —
den des abgebenden und den des aufnehmenden Kerns. Innerhalb desselben Rangs R2 gilt dafür die
Ordnung **aufsteigende Kern-ID**: `SCHEDS[min(src,dst)]` vor `SCHEDS[max(src,dst)]`. Zwei
gleichzeitig migrierende Kerne können sich damit nicht verklemmen. Neu ist außerdem `GID_FREE`
(Freiliste der globalen Thread-Slots): ein **Leaf-Lock**, der unter `SCHEDS` genommen wird
(Spawn) bzw. ganz ohne weiteren Lock (Reap) — nie umgekehrt, und nie zusammen mit `MEM`.

**Regel:** beim Schachteln nur aufsteigend (R0 → … → R4); niemals einen Lock kleineren Rangs
nehmen, während ein größerer gehalten wird. Faustregel der bestehenden Pfade: *nie zwei grobe Locks
gleichzeitig halten, wenn ein Kopieren-und-Freigeben es vermeidet.*

**Belegte Schachtelungen (aus dem Code):**
- `delete_leaf` (Cap-Teardown): `CAPS.write` → `MEM` (`free_region`).  R0 → R4
- `SmmuV3Enforcer::attach`/`detach`: `DMA_CTX` → `MEM` (Tabellen-Alloc/Free).  R1 → R4
- `Heap::allocate` (`RegionSource::request`): `Heap.inner` → `MEM`.  R3 → R4
- `endpoint_quiesce_owner`/`purge_ipc_queues`: `EPS`/`NTFNS` **freigeben**, dann `SCHEDS`
  (`unblock_with_error`).  R1 vor R2
- `ipc_audit`: `EPS`→`SCHEDS` bzw. `NTFNS`→`SCHEDS` (Liveness-Closure, je ein Objekt).  R1 → R2
- `bind_sched_context`: `CAPS.read` (Budget lesen) **freigeben**, dann `SCHEDS[core]`.  R0 vor R2
- `reap_core`: `SCHEDS[core]` (Zombies kopieren) **freigeben**, dann `MEM`.  R2 vor R4 (bewusst
  disjunkt statt geschachtelt — Kontention)
- `dma_attach`/`dma_audit`: `CAPS.read` (DmaCap-Attribute/Snapshot) **freigeben**, dann `DMA_CTX`.
  R0 vor R1
- `fp_trap` / `fp_reset_slot` (Lazy-FP): `SCHEDS[core]` → `FP_STATES`.  R2 → R2.5 (FP_STATES wird
  **immer** unter dem gehaltenen `SCHEDS` genommen, nie davor; danach nur Atomics `FP_OWNER`/`VSPACE_OF`).
- `verifizierer::uebergeben` (C8, `SYS_LOAD`): `SCHLANGE` → `SCHEDS[core]`.  R1.5 → R2.

**Ergänzung C8 (Verifiziererthread):** `SCHLANGE` wird über den Blockiervorgang des Aufrufers
**gehalten**, und das ist keine Bequemlichkeit, sondern die Bedingung, unter der es kein verlorenes
Wecken gibt. Der Aufrufer muss blockiert sein (`BlockReasons::LOAD`, sein Frame gesichert), **bevor**
sein Auftrag für den Verifizierer sichtbar wird; sonst könnte dieser auf einem anderen Kern schon
geantwortet haben, wenn der Aufrufer den Grund erst setzt — und `load_reply` entfernt einen Grund,
den es zu diesem Zeitpunkt noch nicht gibt. `PARK` fängt genau das mit einer Weckmarke ab; für
`LOAD` tut es diese Sperrung.

Der Verifizierer selbst hält **nie beide**: er entnimmt unter `SCHLANGE`, gibt frei, lädt (dabei
nimmt `load_by_index` `CAPS`/`MEM`/`SCHEDS` — alle mit freigegebener `SCHLANGE`) und antwortet dann
über `SCHEDS`. Kein Pfad nimmt `SCHEDS` vor `SCHLANGE`; die Ordnung bleibt azyklisch.

**Leaf-Locks** (halten nie einen weiteren Lock; daher deadlock-sicher unabhängig vom Aufrufkontext):
`KSTACKS`, `VIRTIO_PCI`, `LOADED_IMAGES`, `CS_STATE_REGION`/`RELOAD_INFO`, `hal::console::CONSOLE`.
(`FP_STATES` ist **kein** Leaf — es wird unter `SCHEDS` gehalten, s. R2.5 + Schachtelung oben.)

### 1a. IRQ-Sicherheit der SpinLocks (reentranter Ticket-Lock-Deadlock — Bugfix)

> **Nachtrag 2026-07-29: derselbe Deadlock, zweite Architektur.** Die hier beschriebene Maskierung
> war **nur für aarch64** implementiert; der No-Op-Zweig darunter war für *Host*-Builds gedacht
> (`cargo test`), fing aber auch das x86_64-Kernel-Ziel mit. Auf x86 war damit **kein einziger
> `SpinLock` IRQ-sicher**, und der unten beschriebene Ablauf trat real ein: sporadisches
> Stehenbleiben mitten in einer `println!`-Ausgabe (die Konsolensperre wird am häufigsten
> genommen), etwa jeder achte Lauf. Vorher 7 von 8 vollständige Läufe, nach dem Fix 16 von 16.
> Die Unterscheidung muss `target_os` sein, nicht `target_arch` — `cli` ist im Userspace
> privilegiert. Ein Wächter zur Übersetzungszeit (`IRQ_MASKING_IMPLEMENTED`) verlangt jetzt für
> **jedes** Ziel mit `target_os = "none"` eine echte Maskierung.
>
> Die Lehre über den Einzelfall hinaus: **ein Fehler in der `cfg`-Auswahl ist für eine Testsuite
> unsichtbar.** Jeder Test lief entweder auf dem Host (dort war der No-Op richtig) oder auf ARM
> (dort war die Auswahl richtig). Verhalten zu prüfen reicht nicht, wenn der Fehler in der Auswahl
> steckt; geprüft werden muss die Auswahl selbst.

`SpinLock` ist ein **FIFO-Ticket-Lock**. Mehrere per-Kern-Locks werden **sowohl im IRQ-/Reschedule-
Pfad** (Timer-Tick → `reschedule` nimmt `SCHEDS[core]`; `drain_pending_irqs` nimmt `NTFNS[]`) **als
auch in Thread-/Idle-Kontext** genommen (`idle → reap_core → SCHEDS[core]`; Syscalls; Fuzzer-
`kill_remote`/`reap_core`). Ohne IRQ-Maske ist das tödlich: feuert der Timer-Tick, während Thread-/
Idle-Kontext einen solchen Lock **hält oder erwartet**, zieht der Reschedule-Hook ein **zweites
Ticket** auf denselben Lock — der erste Halter ist aber im IRQ-Handler suspendiert und gibt sein
Ticket nie frei → **Deadlock** (andere Kerne, die cross-core auf `SCHEDS[C]` warten, hängen mit).

**Invariante (erzwungen durch `SpinLock` selbst):** `lock()` maskiert IRQs am eigenen Kern (DAIF
sichern + I-Bit setzen) **vor** dem Ticket-Ziehen und der Guard stellt den vorherigen Zustand beim
`Drop` wieder her (nesting-sicher: jeder Guard sichert den Stand von vor seinem Lock; der äußerste
gibt „IRQs an" frei). Damit kann der Reschedule-/IRQ-Hook einen SpinLock-Halter **nie** unterbrechen.

**Seit ext-29 gilt dieselbe Invariante für `RwSpinLock` (`CAPS`)** — `read()`/`write()` maskieren
ebenfalls selbst. Zuvor tat es der Lock nicht, weil er nicht im IRQ-Pfad genommen wird; das war zu
schwach begründet. Der gefährliche Fall ist nicht der IRQ-Pfad, sondern der **preemptierbare
EL1-Threadkontext**: Kernel-Threads (Demos, Loader-Setup, Fuzzer-Controller) nehmen `CAPS`, und ein
Timer-Tick darf sie dabei nicht verdrängen — läuft danach auf demselben Kern ein **Syscall** an (im
Trap sind IRQs hardwareseitig maskiert), der `CAPS` nimmt, spinnt dieser Kern für immer, weil der
verdrängte Halter nie wieder eingeplant werden kann. Die Deadlockfreiheit hing damit an der
**Konvention**, dass jede dieser ~35 Aufrufstellen selbst `local_irq_disable()` klammert (sie tun
es), statt am **Mechanismus**. Jetzt trägt der Lock sie; die vorhandenen Klammern bleiben gültig
(Save/Restore ist nesting-sicher) und dokumentieren weiterhin die gewünschte Atomarität ganzer
Setup-Sequenzen.

*Befund:* dieser Deadlock war **vorbestehend** (seit der SMP-/MCS-Phase) und trat unter QEMU-TCG mit
~27 % je Lauf auf (im el0iso-/reclaim-/native-/Fuzzer-Abschnitt, der viel spawnt/faultet/reapt +
cross-core killt); er wurde fälschlich als „Host-Last-Flakiness" abgetan. Nach dem Fix: 30/30 Läufe
deadlock-frei (vorher 4/15).

## 1b. Observability ordering — three instances, therefore a rule (2026-08-20)

> **Nothing may become observable before the condition holds under which it may be observed.**
>
> In its two concrete forms:
> * a thread must not become **runnable** before it holds the authority its first syscall needs;
> * a **kick** that reacts to a state must not become visible before that state does.

Three instances have been paid for separately, and at the third it stops being a coincidence:

| | What became observable too early | Cost |
|---|---|---|
| **D0** | `spawn()` made a thread runnable before `bind_pd()` — it made its first `RECV` with an **empty** cspace | ten days, four measurement runs, 0.018 % (one in 5556) |
| **C8** | the caller must be blocked **before** its job becomes visible, or a verifier on another core answers before the reason is set | caught in review; both now under one lock on the job queue |
| **Debugger (§2d)** | the revocation must be visible **before** the IPI goes out, else the target core re-checks, still sees the authority, does nothing — and the kick is spent | named in advance, 2026-08-20 |

The D0 entry is the "make observable" direction, the other two are the "kick" direction; the
condition is the same one.

**Why this is written as a rule rather than three notes.** Each instance was found by a different
route — a 50 000-run measurement, a code review, a plan review — and none of the three would have
found the others. A rule can be checked against a **new** design before it is built, which is what
happened at the third.

### The open question this poses to Gabbro

Every instance is two operations that must not be reordered, with nothing in the type system saying
so. The obvious remedy is a construct that carries *"set the state"* and *"send the kick"* as **one
unit with an enforced order** — then the fourth instance cannot be written down.

That is exactly the class of rule a language is for, and it is a concrete requirement for Gabbro
rather than a wish: **not a lint that finds the pattern, but a pairing that makes the wrong order
unspellable.** Recorded here so the requirement exists before the language work reaches it.

## 1c. Urteilende Stellen fragen die LEERE der Grundmenge, nie einzelne Bits (2026-08-20)

> **Wer über „darf dieser Thread laufen?" urteilt, fragt `reasons.is_empty()`. Wer einzelne Gründe
> aufzählt, urteilt über die Gründe von gestern.**

### Warum das eine Regel ist und keine Stilnotiz

Z24 hat die drei Bits `blocked`/`budget_blocked`/`parked` durch **eine Menge** ersetzt, weil jede
Abspaltung die letzte Kollision reparierte und die nächste aufstellte. Der Gewinn ist nicht die
Menge, sondern was aus ihr folgt: **ein neuer Grund ist überall gedeckt, wo die Leere gefragt wird.**

Gemessen am 2026-08-20 an `BlockReasons::DEBUG` (siebter Grund, dritter vorhergesagter): von den
**sechs** Stellen, die über Thread-Zustände urteilen, brauchten **fünf keinerlei Änderung** —
`Scheduler::audit` Code 2 („blockierter Thread in einer Ready-Liste") und Code 7 („lauffähig, in
keiner Liste") lesen beide `reasons.is_empty()`, und die IPC-Audits fragen eine andere Frage
(lebt/doppelt), auf die ein Grund keinen Einfluss hat.

### Und warum die Ersparnis ohne diese Zeile ZUFÄLLIG wäre

Die Deckung hält, **solange** die urteilenden Stellen die Leere fragen. Die siebte Stelle, die
stattdessen `has(IPC) || has(BUDGET) || has(PAUSE) || …` schreibt, bricht sie **still** — und sie
findet es nicht, denn beim achten Grund sind fünf Stellen ja nachweislich unverändert
durchgekommen. Genau die Beweisführung, die diesmal richtig war, wäre dann die falsche.

Deshalb gilt:

| erlaubt | verboten |
|---|---|
| `reasons.is_empty()` — „darf er laufen?" | eine Aufzählung von Bits als Ersatz dafür |
| `reasons.has(X)` — „wartet er auf **X**?", von genau dem Wecker, der X entfernt | `has(X) \|\| has(Y) \|\| …` als Lauffähigkeitsurteil |
| `remove(X)` durch **den einen** zuständigen Wecker | `reasons = NONE` oder das Entfernen fremder Gründe |

### Die zweite Hälfte, ohne die die Menge nur eine andere Schreibweise ist

**Eingereiht wird nur bei leerer Menge.** Ohne diesen Halbsatz sind die Bits wieder Bits; mit ihm
ist die Park-Naht *unformulierbar* statt behoben. Beide Hälften gehören zusammen, und die zweite ist
die, die beim Nachbauen vergessen wird.

### Ein Literal, das die Verbreiterung überlebt, ist noch nicht richtig

`BlockReasons` ist am 2026-08-20 von `u8` auf `u16` gewachsen. Ein `0xff` als Platzhalter für „nicht
auflösbar" **kompiliert danach fehlerfrei** und wird an dem Tag falsch, an dem Bit 8 vergeben wird:
dann ist `0xff` eine gewöhnliche, erreichbare Menge. Eine solche Stelle stand in
`kernel/src/handlermess.rs` und ist auf `u16::MAX` berichtigt. **Ein Platzhalter, der außerhalb des
Wertebereichs liegen soll, wird aus dem Typ abgeleitet, nicht hingeschrieben** — dieselbe Klasse wie
`MASK_BITS` gegen die Farbanzahl.

---

## 1d. The group cut: a relation with both ends inside the cut is not an open relation (2026-08-21)

> **A process freeze is possible exactly where a thread freeze is not — and the reason is a change
> of question, not a change of mechanism.**

### The rule

`freeze_thread` refuses a thread that holds *any* open IPC relation, because freezing it would
strand a partner. `freeze_pd` asks the narrower question: **does the relation cross the boundary of
the cut?** Two threads of the same PD that call each other are both refused individually and both
frozen together — the reply token stays untouched inside the frozen set, and after the thaw the
transaction continues as if nothing had happened.

Without that distinction a process freeze is not merely unimplemented but **structurally
impossible**: for a mutually calling pair `freeze_thread` answers `Busy` for both, and no ordering
resolves it.

### The two gates, and why there are two

| gate | where it sits | what it stops |
|---|---|---|
| **subject gate** (S1) — `Pd::quiescing` | the PD | what the PD *starts*: `CALL`/`RECV` → `ERR_QUIESCING`, `REPLY` stays allowed |
| **object gate** (S1b) — `Endpoint::begin_quiesce` | an endpoint **the frozen PD serves alone** | what *others* start towards it |

The subject gate makes the set of open transactions **shrink-only** — without it every quiescence
answer is stale the moment it returns.

**The object gate is set only where the frozen PD is the sole receiver.** A bolt on a shared
endpoint would freeze in third parties who have nothing to do with the freeze; and where a foreign
server also serves, foreign callers keep being served, so no bolt is needed. This is the decision
S1b left open, taken with its cost stated: **a client calling into a frozen PD's exclusive channel
gets `ERR_QUIESCING` and must retry.** The two rejected alternatives were bounded queueing (nobody
pays for the buffer, and the overflow is another D11 case) and unbounded blocking (honest for a
PaaS, but it hands the deadlock to an uninvolved third party).

### What the cut must undo, and in which order

Thawing is **not** the inverse read backwards. Receivers are re-enqueued **before** any channel
re-opens: the other way round a caller could hit the channel while the server is not yet in the
queue and land in the sender queue instead of being served.

### The freeze is its own block reason

`BlockReasons::FREEZE` (bit 7, the eighth instance) and **not** `PAUSE`, although `PAUSE`'s own doc
named the group cut as a future user. With `PAUSE` a single `SYS_PDCTL RESUME` on one member would
lift one participant out of the set: the PD would be **half** frozen, its open transactions growing
again, and every checker would report order — the D11 form. Measured by the conjunct
`resume-wirkt-nicht` in the `pdfreeze` line, and by counter-proof M1.

### Named refusals, all of them fail-closed

`HasDma` (S5, first version: a PD holding a DMA capability is not frozen at all — its device keeps
writing, so the promise would not hold), `TooManyThreads`, `MultiReceiver`, `SenderQueued`,
`Debugged`, `AlreadyQuiescing`, `Empty`, and `Deadline { tid, partner }`. **Every outcome other than
`Frozen` leaves the state it found** — gates open, receivers enqueued, no reason bit set.

`SenderQueued` is a speaking probe, not information: the load-bearing invariant is *where a receiver
waits, the sender queue is empty* (they meet immediately). The cut asks instead of believing; if the
invariant ever fails it must **show**, not be built upon.

### 1d.1 Time does not pause — the duration is reported instead (2026-08-21)

> **A thawed thread sees the clock jump. The kernel does not hide that; it reports how long the PD
> stood, to whoever holds the freeze authority.**

The obvious alternative — a per-PD clock that pauses with the freeze — is **rejected**, for three
reasons, and the third one alone settles it:

1. It would be a **second memory for one fact**. MCS budgets, periods and cycle stamps all run on
   the global clock; a second clock beside it will diverge.
2. A timestamp received by IPC from a PD that was *not* frozen would then lie in the future of the
   receiver's own clock. „The clock jumps" becomes „foreign timestamps are unusable" — a worse
   trade.
3. **EL0 reads the clock directly** (`rdtsc` / `CNTVCT_EL0`). A kernel clock that pauses would not
   cover the hardware clock the thread actually reads — the kernel cannot keep that promise at all.

`thaw_pd` therefore returns `Thawed { threads, ticks }`. The duration goes to the holder of the
freeze authority, the same rule as the partner name in `Freeze::BusyOn`.

**Named gap, and it is not a side effect:** the frozen PD itself cannot learn the duration — that
needs a self-query syscall. A thread that computed a deadline from `rdtsc` before the freeze
therefore **cannot correct it**.

### 1d.2 An image carrying register state is confidential (2026-08-21)

> **Image contains register state ⇒ confidential. The storage and transport rule stands before the
> migration code is written, not implicitly inside it.**

An `Image` with `progress` and capability classes was a **metadatum**. One with registers, stack and
memory contents is a **data carrier** — key material, and after the eager-FP conversion expressly
**including the XMM registers**, i.e. exactly the material for whose sake eager was chosen. Under
migration the image **leaves the machine**; who may read it, where it rests, whether it is encrypted
at rest, is the same kind of decision as `SVT`/`SID` on the IRTE: authority, to be specified up
front.

Machine-readable in `checkpoint::image_is_confidential`; a rule in prose has no gate. Measured
consequence: **every complete image is confidential** — there is no migration case in which the rule
lapses (`die_volle_liste_ist_vertraulich`).

The rest of S4 is the existing rule one level down: *what cannot denote the same thing over there
does not travel, and is refused **by name*** — `classify_thread_part` alongside `classify`, with
`LocalReason::MachineLocalNumber` as its own reason. A core affinity is not a device window: the
window is **not constructible** over there, the affinity **arises anew** over there. Same code for
both would send someone looking for a device where an assignment is missing.

---

## 2. DMA-Revoke-Reihenfolge (DMA-use-after-free-Sicherheit)

Eine DMA-Region wird **immer** in dieser Reihenfolge abgebaut (`revoke_dma` + `vspace_teardown`):

1. **Gerät stilllegen** (`pcie::quiesce_by_rid`, ext-35): Bus-Master löschen, dann ein
   **Config-Read vom selben Gerät**. PCIe garantiert, dass eine Completion posted Writes nicht
   überholt — die Antwort trifft also erst ein, nachdem die zuvor vom Gerät abgesetzten Writes
   zugestellt sind.
2. `enforcer.disable_dma(binding)` — die Stage-1-Region wird aus dem Übersetzungskontext entfernt
   + `CMD_TLBI`+`CMD_SYNC` (synchron: `wait_cons` wartet auf `CMDQ_CONS == PROD`).
3. VSpace-Unmap (`unmap_dma_from_thread`) + `flush_asid`.
4. **Erst dann** `free_region` (über `delete_leaf` der DmaCap bzw. `free_raw_region`).

### Was Schritt 1 und Schritt 2 je abdecken — und warum keiner den anderen ersetzt

Bis ext-34 stand hier: *„Die Stage-1-Region wird entfernt. **Danach kann kein Gerät mehr in die
Region DMAen.**"* Das war für **künftige** Übersetzungen richtig und für bereits übersetzte, noch
unterwegs befindliche (posted) Writes **falsch**. Das Entfernen eines Übersetzungseintrags sagt
nichts über Transaktionen, die die Übersetzung schon durchlaufen haben.

| | deckt ab | deckt **nicht** ab |
|---|---|---|
| **Schritt 1** (BME-Clear + Flush-Read) | das **gutartige** Gerät: bereits abgesetzte Writes sind nach dem Flush-Read zugestellt | ein **kompromittiertes** Gerät, das `BME` ignoriert |
| **Schritt 2** (Stage-1/STE entfernen) | jede **künftige** Anforderung — auch die eines kompromittierten Geräts | Transaktionen, die die Übersetzung bereits passiert haben |

**Invariante:** zwischen Schritt 4 und einem späteren Re-Alloc desselben RAM zeigt **keine**
SMMU-Stage-1 mehr auf die Region **und** ist keine vom Gerät abgesetzte Transaktion mehr in
Zustellung. Verletzung = DMA-use-after-free.

**Grenzen von Schritt 1** (gehören zur Aussage, nicht als Fußnote): Die Ordnungsgarantie hält
nur, solange *Relaxed Ordering* / *ID-Based Ordering* für diese Funktion nicht aktiv sind und der
Pfad einheitlich ist. Ein Gerät, das `BME` nicht respektiert, wird davon nicht erfasst — für das
ist Schritt 2 die Grenze. Ein vollständiges Quiesce (FLR bzw. der dokumentierte Weg des Geräts)
wäre stärker; BME + Flush-Read ist der Teil, der **ohne** gerätespezifisches Wissen auskommt.

**BME-Wiederherstellung** (`release_quiesce`): Trägt der Übersetzungskontext nach dem Detach noch
andere Regionen, ist das Gerät legitim weiter in Betrieb und der vorherige Zustand wird
zurückgeschrieben (die eben entfernte Region ist ab Schritt 2 ohnehin unerreichbar). Ist es die
letzte Region, bleibt Bus-Master **aus** — ein Gerät ohne jede Zuteilung soll auch keine Requests
absetzen dürfen. Zurückgeschrieben wird stets der **gesicherte** Wert, nie „auf 1 gesetzt": ein
Gerät, das absichtlich aus war, darf ein Teardown nicht einschalten.

Die Wiederherstellung bringt drei Bedingungen mit, die der naive Ablauf nicht hatte:

0. **Über *alle* StreamIDs des Kontexts.** Die Region ist im **Kontext** gemappt, nicht an einer
   RID — bei einer Stream-Gruppe (mehrere Geräte teilen eine Stage-1-Tabelle) können in-flight
   Writes von **jedem** Gerät der Gruppe kommen. Entwaffnet würde nur das eine, dessen StreamID
   der Teardown trägt, wäre der Zähler korrekt und die Flush-Garantie trotzdem unvollständig.
   Reihenfolge: erst **alle** entwaffnen, dann **alle** spülen, dann unmappen, dann in umgekehrter
   Ordnung freigeben — so ist kein Gerät der Gruppe mehr scharf, während ein anderes noch spült.
1. **Serialisierung je RID — gegen Detach *und* Attach.** Zwei nebenläufige Teardowns auf
   demselben Gerät dürfen sich nicht gegenseitig entwaffnen: entwaffnet A, spült B, und stellt A
   dann wieder her, *bevor* B seine Region entfernt hat, ist B's Flush-Garantie wertlos. Dieselbe
   Kante kommt über `attach` herein, wenn dort Bus-Master **unbedingt** gesetzt wird. Deshalb ein
   **Tiefenzähler je StreamID** (`ctx_quiesce`/`ctx_release`/`ctx_arm_bus_master`): entwaffnen bei
   0→1, wiederherstellen bei 1→0 — und `attach` schreibt bei Tiefe > 0 **nicht** direkt, sondern
   vermerkt die Absicht im gesicherten Command-Wert, den das Restore mitnimmt.
   Der Zähler liegt **im Übersetzungskontext**, parallel zur StreamID-Liste, nicht in einer
   Seitentabelle: eine RID gehört zu genau einem Kontext, damit ist der Zähler durch denselben
   Lock geschützt wie die RID selbst (er kann nicht aus dem Tritt geraten, wenn das
   Kontext-Locking später verfeinert wird) und er kann **nicht überlaufen**, weil seine Kapazität
   dieselbe Quelle hat wie die Kontextobergrenze (`NDMA_CTX × MAX_CTX_SIDS`). Ein „Tabelle voll"-
   Zweig — und damit die Wahl zwischen Sicherheit und Verfügbarkeit — existiert nicht.
2. **Kopplung an die ATS-Entscheidung** (§2b). „Ab Schritt 2 ist die Region unerreichbar" gilt,
   *weil* nach `CMD_TLBI`+`CMD_SYNC` keine gecachte Übersetzung mehr existiert. Mit **ATS**
   existiert sie sehr wohl — im ATC des Geräts. Wird ATS je freigeschaltet, muss vor der
   Wiederherstellung eine **ATC-Invalidierung** stehen, sonst ist sie unsolide. Der Vermerk steht
   auch an `release_quiesce` selbst, damit die Kopplung nicht still bricht.
3. **Gegenstück in `attach`.** Weil beim letzten Detach Bus-Master bewusst aus bleibt, muss das
   Installieren einer Übersetzung es wieder scharf schalten (`arm_bus_master`) — sonst wäre ein
   Gerät nach einem vollständigen Detach/Re-Attach-Zyklus dauerhaft tot. Reihenfolge: erst die
   Übersetzung, dann die Erlaubnis.

**Audit:** `dma_audit()` Code `4` — *jede* in einem `DMA_CTX` gemappte Region muss einer **lebenden
DmaCap** entsprechen. Eine Kontext-Region ohne zugehörige Cap bedeutet: RAM wurde freigegeben,
während die SMMU noch darauf zeigte (Schritt 4 vor Schritt 2) → Verletzung. (Snapshot von `DMA_CTX`
ziehen, Lock freigeben, dann gegen `CAPS.read().for_each_dma` prüfen — Rangordnung R0 vor R1.)

### 2c. Zwei Adressachsen (ext-36)

Ein DMA-Puffer hat zwei Adressen: die **physische** (CPU-Sicht — Cache-Wartung, Allokator,
Lebendigkeitsprüfung) und die **IOVA** (Gerätesicht — Stage-1-Abbildung, Deskriptorinhalt,
Bounds-Prüfung des Treibers). Seit Schritt b **laufen sie auseinander**: eine IOVA entsteht
ausschließlich aus dem Fenster eines Übersetzungskontexts (`ctx_alloc_iova`), dessen Basis
oberhalb von `RAM_TOP` liegt. Es gibt keinen Konstruktor mehr, der IOVA = PA setzt
(`DmaRegion::identity` ist entfernt, nicht nur ungenutzt), und keine Rechenbeziehung zwischen
beiden — `detach` findet die IOVA über die geführte Region, nicht über Arithmetik.

**Fenster und Schutzbänder:** Basis = erste `IOVA_GUARD`-ausgerichtete Adresse oberhalb `RAM_TOP`,
je Kontext 1 GiB aus einem globalen Bump. Innerhalb des Fensters wird jede Region
`IOVA_GUARD`-ausgerichtet vergeben, mit anschließendem Schutzband, und eine IOVA wird **nie**
wiederverwendet. `IOVA_GUARD` ist mit 2 MiB auf die größte Stage-1-Blockgranularität dimensioniert
— ein 4-KiB-Band könnte von einem Block-Mapping überspannt werden und wäre dann keins. IOVA 0
bleibt unabgebildet. Die Eingangsbreite ist durch `CD.T0SZ = 25` auf 39 Bit begrenzt; eine
Fensterbasis darüber wäre nicht übersetzbar und wird abgelehnt.

**Invariante:** Die beiden Achsen sind **typgetrennt** (`addr::Pa` / `addr::Iova`), nicht nur
benannt. Eine Verwechslung ist damit ein Compilerfehler, keine stille Fehlfunktion — das ist
wesentlich, weil eine vertauschte Achse nicht abstürzt, sondern *falsch prüft*: Cache-Wartung auf
einer Adresse, unter der nichts liegt, oder eine Bounds-Prüfung gegen den falschen Raum.
`DmaHandle.pa` ist **kernelprivat**; ein Treiber sieht nur die IOVA.

Die Umwandlung nach `u64` (`raw()`) ist bewusst eine sichtbare Handlung und markiert jede Stelle,
an der die Trennung an die HAL-Grenze übergeben wird.

**Wächter (`dma_audit` Code `5`):** Der Lebendigkeits-Vergleich (Code 4) läuft gegen die Freiliste
des **physischen** Allokators und ist nur aussagekräftig, wenn er physische Adressen sieht. Würde
dort die IOVA stehen, überlappte ab Schritt b nie etwas — das Oracle bliebe grün, ohne noch etwas
zu prüfen. Code 5 prüft deshalb, dass die geführten PA-Werte im RAM-Fenster des Allokators liegen;
eine vertauschte Achse schlägt dort an. (Ein Oracle, das nach einem Achsenwechsel unverändert grün
bleibt, ist der Normalfall des Blindwerdens — nicht der Beleg, dass alles stimmt.)

**Beleg (negativ, `virtiorng`):** Ein Oracle, das nur bestätigt, ist kein Beleg. Der Test gibt dem
Gerät im Deskriptor absichtlich die **PA** statt der IOVA — die Verwechslung, die vor Schritt b
folgenlos war — und verlangt drei Dinge: die Event-Queue ist **vorher** geleert (sonst bestünde er
an einem Altbestand), der Eintrag wird **geprüft statt gezählt** (`F_TRANSLATION`, erwartete
StreamID, Input-Adresse gleich der eingetragenen PA), und im selben Lauf geht dieselbe Anfrage mit
der korrekten IOVA durch, **ohne** Event (Positivkontrolle). `cj_axes_differ` belegt zusätzlich,
dass der Lauf überhaupt mit IOVA != PA stattfand — ohne das prüfte der Negativtest nichts.

Damit das messbar ist, muss das Gerät die IOMMU auch wirklich benutzen: der virtio-Treiber
verlangt **verbindlich** `VIRTIO_F_ACCESS_PLATFORM` und bricht sonst ab, statt still auf physische
Adressen zurückzufallen — dieser Rückfall *wäre* die Achsenverwechslung. Erst dadurch wurden zwei
Konfigurationsfehler sichtbar, die vorher niemand bemerken konnte, weil das emulierte Gerät die
SMMU umging: `STE.S1STALLD` war unbedingt gesetzt (zulässig nur bei `IDR0.STALL_MODEL == 0b10`,
sonst `C_BAD_STE` — der Stream übersetzte **gar nicht**), und im CD fehlten `A` (Terminate-Modell:
Fault bricht ab) und `R` (Fault wird als Event aufgezeichnet). Zwei Fehler, die sich gegenseitig
verdeckten und zusammen wie ein funktionierender Aufbau aussahen.

**Liveness als Vorbedingung jedes Abwesenheits-Oracles.** Zweimal hintereinander hat hier dieselbe
Klasse zugeschlagen: Code 4 verglich gegen die falsche Achse und wäre nach Schritt b still grün
geblieben, und die Event-Queue wäre ohne `CD.R` strukturell leer gewesen. Beide Male hätte Grün
nichts bedeutet. Die Regel daraus: **jedes Oracle, das über Abwesenheit entscheidet, braucht einen
Nachweis, dass es überhaupt sprechen kann.** Umgesetzt in zwei Formen:

* **`dma_audit` Code `6`** — `hal::smmu::config_errors()` zählt die Ereignisklasse, mit der die
  Einheit sagt „deine Tabellen ergeben keinen Sinn" (`C_BAD_STE`, `C_BAD_CD`, `C_BAD_STREAMID`,
  `F_STE_FETCH`, `F_CD_FETCH`). Der Zähler wird beim **Leeren** der Queue fortgeschrieben, also
  unabhängig davon, ob gerade jemand hinsieht, und überlebt das Leeren — er ist eine Aussage über
  den Kernel, nicht über den Verkehr. Ein solcher Eintrag heißt: der Stream übersetzt *gar nicht*,
  und jedes spätere „keine Faults" ist bedeutungslos.
* **Queue-Liveness (`evtq_liveness`)** — „Event-Queue leer" zählt im `virtiorng`-Test nur, wenn im
  **selben Lauf** ein echter `F_TRANSLATION` beobachtet wurde. Ein Selbsttest beim Hochlauf wäre
  die schönere Form, ist aber nicht konstruierbar: ein Übersetzungsfehler entsteht nur durch eine
  echte Bus-Master-Anforderung, und `ATOS` liefert sein Ergebnis ins `PAR`, nicht in die
  Event-Queue (QEMU implementiert es ohnehin nicht). Der Nachweis im selben Lauf ist die
  erreichbare Fassung — und er ruht auf einer Beobachtung statt auf `CD.R`, das jemand später aus
  Performancegründen wieder abschalten kann.

**Sensitivitätskontrolle (`cj_bypass_wrote`).** Die frühere Kontrolle war eingeklappt: „das Gerät
schrieb ODER die SMMU faultete" prüft dieselbe Beobachtung wie die Hauptaussage und kann nicht
fehlschlagen, während diese besteht. Die Kontrolle, die trägt, hebt die Durchsetzung auf: mit einer
**Bypass-STE** muss dasselbe Gerät dieselbe Adresse wirklich schreiben. Erst damit ist das
Ausbleiben des Schreibzugriffs eine Aussage über die SMMU und nicht über ein Gerät, das aus
irgendeinem Grund gar nicht mehr DMAt. (Die naheliegende Mutation „Fensterbasis auf 0" leistet das
**nicht**: die Sentinel-Seite liegt auch bei IOVA = PA außerhalb der gemappten Region und faultet
weiterhin — sie unterscheidet die beiden Welten nicht.) Unter Bypass gibt es zudem keine
Gerätesicht mehr; der Test programmiert dort in beiden Rollen die PA, und genau das ist die
Konfiguration, in der die Achsentrennung wirkungslos ist.

**Fenstergrenzen als geprüfte Eigenschaft (`dmawin`).** Der Bump gibt nie zurück, also gibt es eine
Lebenszeit-Obergrenze; sie darf nur kein Betriebszustand sein. Das Fenster hängt jetzt am
**Kontext-Slot** statt an einer Erzeugung (`NDMA_CTX` Slots, `NDMA_CTX` Fenster, Bump je Slot
überlebt den Kontextabbau) — vorher war die schärfere, nirgends notierte Grenze nicht die ~256
Attach-Vorgänge je Kontext, sondern ~500 Kontext-*Erzeugungen* insgesamt. Drei Bedingungen mit
eigener Fehlerursache, alle drei laut statt still: Fensterende (`WindowExhausted`), Eingangsbreite
der Stage-1 (`InputWidth`), **Adressbreite des Geräts** (`DeviceAddrWidth`). Die letzte ist die
gefährlichste: ein Gerät mit 32-Bit-DMA bekäme aus einem Fenster oberhalb des RAM eine Adresse, die
der Bus abschneidet — und die abgeschnittene Adresse trifft etwas anderes. Voreinstellung sind 64
Bit — aber **undeklariert heißt nicht stillschweigend 64**: jede StreamID ohne Deklaration wird
einmal protokolliert und gezählt (`dma_undeclared_devices`). Ein harter Fehlschlag wäre strenger,
ist aber nicht zumutbar, weil die Adressbreite in keinem Konfigurationsregister steht — sie ist
Treiberwissen. Die geführte Annahme ist die ehrliche Mittelstellung: sie behauptet nicht, etwas zu
wissen. `dma_declare_device_addr_bits` schreibt eine schmalere Breite zu.

**Nicht-Wiederverwendung ist Politik, kein Defizit.** Eine IOVA kommt nie zurück — weder innerhalb
eines Kontexts noch über dessen Abbau hinweg (der Bump hängt am Slot und überlebt ihn). Das ist
keine Übergangslösung bis zu einem Recycling-Mechanismus, sondern die gewollte Eigenschaft: eine
Adresse, die nie wiederkehrt, kann keine veraltete Übersetzung tragen, und die gesamte Klasse
„Deskriptor mit alter IOVA trifft neue Region desselben Kontexts" existiert nicht. Die
Schutzbänder deckten sie ohnehin nicht ab, weil eine wiederverwendete IOVA *legitim* gemappt wäre.
Der Preis ist eine Lebenszeit-Obergrenze je Slot; sie liegt nach der Slot-Umstellung im vier- bis
fünfstelligen Bereich an Attach-Vorgängen und ist ein geprüfter, sauberer Fehlschlag mit intaktem
Kontext (`dmawin`), kein Betriebszustand. Wird die Grenze eines Tages doch erreicht, ist der
richtige Ausweg **Slot-Recycling beim Kontextabbau**, nicht IOVA-Recycling innerhalb eines
Kontexts — dort ist der Teardown ohnehin vollständig.

**Was Newtypes nicht finden:** Sie markieren Kanten. Eine Funktion, die vollständig in `u64` lebt,
ist keine Kante, sondern ein Loch — der `dmagen`-Test las die Stage-1-Blätter mit `r1.base` (PA)
statt mit der IOVA und blieb für den Compiler unsichtbar; gefunden hat ihn erst das
Auseinanderlaufen der Werte. Die Restprüfung ist deshalb ein `grep` nach `raw()` im DMA-Pfad
(16 Stellen, jede einzeln begründet).

### 2e. Warum RMRR-Geräte abgewiesen werden (x86)

Eine RMRR verlangt, dass die genannte Region für das betroffene Gerät **identitätsgemappt** ist.
Identität heißt IOVA = PA — also die Aufhebung genau der Eigenschaft, die §2c herstellt, und zwar
**innerhalb** desselben Übersetzungskontexts, in dem sonst das Fenster oberhalb `RAM_TOP` gilt.
Ein solcher Kontext trüge zwei Achsenregime nebeneinander, und jede Bounds-Prüfung müsste beide
kennen.

**Invariante:** Ein Gerät im Scope einer RMRR ist **nicht zuteilbar** (`Exclusion::Rmrr`), und die
Ausschlussliste wird protokolliert. Die Begründung ist damit strukturell, nicht bloß konservativ:
die Alternative wäre kein „Gerät mit einer kleinen Lücke", sondern ein Kontexttyp, der die
Kernaussage des Kapitels nicht trägt. Diese Begründung hält auch dann noch, wenn jemand später
eine Quirk-Liste hübsch findet. Die Regionsadressen liest der Kernel deshalb gar nicht — nur den
Device-Scope, denn er filtert, er mappt nicht.

### 2d. Teardown-Token: aus einer bewiesenen wird eine erzwungene Reihenfolge (ext-37)

Bis ext-37 gab `CapSpace::delete_leaf` eine `ObjectKind::Dma`-Region direkt über
`alloc.free_region` zurück. Dass davor stillgelegt, unmappt und synchronisiert worden war, war eine
**bewiesene** Vorbedingung des Gesamtsystems — nirgends im Typ, und von der Cap-Crate auch nicht
prüfbar: sie kennt weder Gerät noch Enforcer.

**Invariante:** Eine DMA-Region gelangt nur über einen `DmaTeardownToken` zurück in den Allokator.
Der Token ist ausschließlich in `dma_finalize` konstruierbar und entsteht erst, wenn für seine
Region gilt: Gerät stillgelegt **und bestätigt**, Übersetzung entfernt, `CMD_SYNC` durch. Er trägt
eine `DmaRegion` (beide Achsen), keine `PhysRegion` — der Unmap liegt auf der IOVA-Achse, der Free
auf der PA-Achse; mit einer reinen `PhysRegion` könnte der Token einen erfolgten Free bezeugen,
ohne dass der Unmap darin überhaupt vorkommt.

Die Cap-Crate meldet stattdessen: `ReplyFinal` heißt jetzt `Finalized` und führt neben den
abzubrechenden Calls die finalisierten DMA-Regionen. Derselbe Rückmeldeweg, dieselbe `NOBJECTS`-
Schranke, dieselbe Stelle im Kernel (nach dem Freigeben von CAPS/MEM) — plus ein `overflowed()`,
damit „kann nicht vorkommen" prüfbar ist statt behauptet.

**Synchron, nicht aufgeschoben.** Der Preis ist eine geräteabhängige Laufzeit im `delete`; der
Gewinn ist, dass es den Zwischenzustand für den Normalfall **gar nicht gibt**. Ein Zustand, den es
meistens nicht gibt, ist schwerer richtig zu halten als einer, den es nie gibt.

**Gebündelt.** Ein `revoke` über einen CDT-Teilbaum finalisiert N DMA-Objekte auf einmal. Naiv
wären das N Quiesce/Flush/`CMD_SYNC`-Zyklen — dieselbe Form, die beim Multi-SID-Detach schon einmal
die falsche war. `DmaEnforcer::finalize` nimmt deshalb den ganzen Stapel: erst alle entwaffnen,
alle spülen, alle unmappen, **ein** `TLBI`+`SYNC`, dann freigeben.

**Pending (`dma_audit` Code `7`).** Bestätigt ein Gerät die Stilllegung nicht — das
Konfigurations-Read liefert `0xFFFF`, oder das Bus-Master-Bit bleibt trotz Löschen stehen —, dann
wird die Übersetzung **trotzdem** entfernt (das ist immer zwingend; eine stehende Übersetzung auf
eine gleich freigegebene Region *ist* der Use-after-free), die Physadresse aber **nie**
zurückgegeben. Ein Leck ist gegenüber einem UAF der richtige Failure-Mode — aber als Entscheidung:
die Menge ist beschränkt (`MAX_PENDING_DMA`), gezählt, protokolliert und auditiert. Code 7 prüft
die Konjunktion, die den Zustand ausmacht: eine Pending-Region steht **weder** in der Freiliste
**noch** in irgendeiner Übersetzungstabelle. Läge sie in der Freiliste, wäre sie trotz
unbestätigter Stilllegung neu vergebbar; stünde sie noch in einer Tabelle, wäre der zwingende
Unmap ausgefallen. Genau diese Konjunktion reißt, wenn die Fädelung durch `Finalized` irgendwo
abbricht.

**Wo Pending entstehen darf.** `destroy_pd` ist der realistische Weg in „quiesziert nie": das Gerät
hängt, und niemand ist mehr da, der auf einen Fehlercode reagieren könnte. Dort läuft ein
`KillScope`, und Pending zählt nicht als Anomalie. Überall sonst schlägt es zusätzlich auf
`dma_pending_stats().1` durch — den Zähler, den man sehen will.

**Was der Token nicht leistet:** Eine IOVA gibt er nicht zurück, weil es nichts zurückzugeben gibt
(s. Nicht-Wiederverwendung in §2c). Der Zustandsraum ist damit um einen Zustand kleiner, als er in
der ursprünglichen Planung war.

### 2a. Granularität eines DMA-Puffers (ext-35)

**Invariante:** Anfang **und** Länge einer DMA-Region liegen auf dem **Cache-Writeback-Granule**
der Architektur (`hal::mmu::dma_granule()`; `CTR_EL0.CWG` auf ARM, `1` auf x86 — dort ist DMA
hardware-kohärent, es gibt keine Wartung und damit keine Bedingung).

Zwei Fallen, an denen der Wert still zu klein antworten kann, beide behandelt:
* **`CWG == 0` heißt „nicht angegeben", nicht „4 Byte".** Würde man `4 << 0` rechnen, käme 4 heraus
  und die Prüfung wäre auf Hardware, die CWG nicht meldet, praktisch vakuum. Stattdessen gilt die
  architektonische Obergrenze (2048).
* **`CTR_EL0` ist pro Kern.** Auf heterogenen Systemen können die Werte differieren; jeder Kern
  meldet seinen beim Hochlauf (`record_cache_granule` in `init_primary`/`init_secondary`),
  `dma_granule` liefert das **Maximum**. Bis `seal_cache_granule()` nach dem SMP-Hochlauf gerufen
  ist, liefert es stattdessen die **architektonische Obergrenze** — eine früh geprägte Cap wird
  also gegen die strengstmögliche Granularität geprüft und kann nie zu schwach geprüft worden
  sein. Damit ist die Annahme **erzwungen statt dokumentiert**. Meldet ein Kern **nach** dem
  Versiegeln einen *größeren* Wert (CPU-Hotplug, verzögerter Sekundärkern), ist das ein
  **Abbruch**, kein leises Anheben: seither geprägte Caps wären rückwirkend zu schwach geprüft,
  und das würde ein Anheben genau verdecken. Heute starten alle Kerne vor dem Versiegeln.

Zur Unterscheidung: **DminLine** ist die Schrittweite der Wartungsschleife (Minimum, damit keine
Zeile ausgelassen wird), **CWG** die Ausrichtungs-/Padding-Granularität (Maximum, damit keine
fremden Daten in derselben Writeback-Einheit liegen). Beide werden jetzt aus `CTR_EL0` gelesen;
vorher stand an beiden Stellen eine hartkodierte 64. Erzwungen an der
**Cap-Prägung** (`install_dma_cap`/`install_dma_cap_ex` → `CapError::Unaligned`), nicht als
Treiberpflicht.

**Grund:** Cache-Wartung arbeitet auf ganzen Zeilen — `dc civac` (nach einem Geräte-Write bzw. bei
bidirektionalen Puffern) **verwirft** eine komplette Zeile. Liegt in einer angebrochenen Randzeile
fremder Speicher, verliert der seine noch nicht zurückgeschriebenen Daten. Das ist ein Schaden
**außerhalb** des Puffers, den weder die Bounds-Prüfung noch die IOMMU sieht: beide betrachten den
Puffer, nicht seine Nachbarschaft.

Die Bedingung gilt **einheitlich**, nicht nur für die Richtungen, bei denen invalidiert wird
(`DeviceWrite`/`Bidirectional`). Richtungsabhängig wäre näher am tatsächlich Gefährlichen, aber
eine Cap ist langlebig und ihre Richtung ein Feld — eine später erlaubte bidirektionale Nutzung
säße sonst auf einer Ausrichtung, die nie dafür geprüft wurde.

**Test:** `dmaalign` (Boot-Selbsttest) — ausgerichtete Region wird angenommen, verschobener Anfang
und angebrochene Länge werden abgewiesen, und aus den Fehlschlägen entsteht keine Cap.

### 2b. ATS (Address Translation Services) — Entscheidung, nicht Unterlassung

**ATS wird nicht aktiviert.** Ein Gerät mit ATS darf Übersetzungen **selbst cachen** (ATC) und
Anforderungen als „bereits übersetzt" markieren — die IOMMU reicht die dann durch. Ein
kompromittiertes oder fehlerhaftes Gerät umgeht damit praktisch die gesamte Durchsetzung
(*Thunderclap*-Klasse). Für ein System, dessen DMA-Isolation genau auf dieser Durchsetzung
ruht, ist ATS deshalb standardmäßig aus.

Die Bedingungen, unter denen es je eingeschaltet werden dürfte — damit das eine überprüfbare
Entscheidung bleibt und nicht beim nächsten Durchsatzproblem stillschweigend fällt:

1. eine **explizite Geräteliste** (nicht „alles, was es kann"), begründet je Eintrag;
2. **ATC-Invalidierung im Teardown-Pfad vorhanden und getestet** — ohne sie ist Schritt 2 oben
   wirkungslos, weil das Gerät seine alte Übersetzung weiterbenutzt;
3. die Geräteliste ist Teil des Trust-Modells (ADR 0007), nicht Treiberkonfiguration.

Solange auch nur eine der drei Bedingungen fehlt, bleibt die ATS-Capability ungesetzt.

## 3. Region-Balance (kein RAM-Leck)

Jede `PhysAllocator`-Allokation hat genau einen Rückgabepfad; über einen vollständigen
Alloc-/Free-Zyklus bleibt `MEM.total_free()` unverändert.

- **DmaCap-Region:** carvt über `KernelRegionSource::request` (eine MEM-Carve-Stelle, K3), aber das
  **Besitzmodell ist (phys,len)-basiert**: die Lebensdauer hängt an der **DmaCap**, nicht an einem
  `Region`. Freigabe genau einmal über `delete_leaf`(Dma) → `free_region` (Cap-Pfad) bzw.
  `free_dma_region` (roher Pfad). Der `Region`-Wrapper aus `request` wird sofort zum reinen
  `MemoryCap`-Deskriptor aufgelöst (`into_cap`, kein Drop-Free) — bewusst **nicht** Region-besessen
  (eine DMA-Region wird nie über `RegionSource::release` zurückgegeben). Das ist kein Duplikat,
  sondern ein zweites legitimes Besitzmodell für dasselbe RAM.
- **Region-Runtime (Heap):** jede `Region` **besitzt** ihren `MemoryCap` (lineares Eigentum);
  `Heap::drop` gibt **alle** Regionen über `RegionSource::release` zurück.
- **Thread-Stacks:** `spawn` ↔ `reap_core` (`REAPED_BYTES` belegt die Rückgabe monoton).

**Audit/Test:** hwfuzz-Baseline (`total_free` balanciert je Epoche), `churn`, `dmagen`
(`balanciert`), `sasheap` (`balanciert(alle Regionen zurück)`).

## 4. `RegionView`/`Pod`-Sicherheitsvertrag (das gesamte Speicher-`unsafe`)

Das gesamte Speicher-`unsafe` der Trusted-SAS-Schicht liegt in `crates/caprock-region` (RegionView-
Accessoren + Allokator-Glue). Es ist begründet durch:

1. **Cap-validierter Besitz:** eine `Region` hält eine `MemoryCap` (lineares Eigentum); die Bytes
   `[base, base+len)` sind exklusiv dieser Region zugeordnet (vom Kernel ausgeschnitten, disjunkt).
2. **Bounds:** jeder `RegionView`-Zugriff (`get`/`set`/`copy_*`/`fill`/`with_bytes`/`subview`) prüft
   `offset + size_of::<T>() <= len` **vor** dem Roh-Zugriff; out-of-bounds → kein Zugriff.
3. **`Pod`-Beschränkung:** typisierte Zugriffe nur für `unsafe trait Pod` (`Copy`, keine Padding-/
   Pointer-Invarianten, jede Bitkombination gültig) — kein Erzeugen ungültiger Werte.
4. **Kein nackter Slice nach außen:** `with_bytes(|s: &mut [u8]| …)` bindet den Slice an die Closure
   (kann nicht entkommen); es gibt **keinen** öffentlichen `MemoryCap -> &mut [u8]`-Wrapper.

**Folge (ADR 0002 + 0010):** mit *no unsafe im App-Code + buglosem Compiler* kann ein safe-Rust-
Prozess nur Speicher berühren, der von seinen legitimen Referenzen erreichbar ist (Stack, Statik,
seine `Region`-Menge). Diese Schicht trägt die Isolation **ohne MMU**; ihre Korrektheit (Bounds +
cap-Besitz) ist die zentrale Verifikationsverpflichtung.

## 5. Domänen-/Cap-Policy (ext-22, Kurzfassung)

- HW-Caps (`Mmio`/`Irq`/`Dma`, `kind_is_hardware`) nur in **HardwareLand**; `PdControl` nur in
  **TrustedSas** (`install_cap_checked`).  Audit: `domain_audit()` Code 1/2.
- Eine untrusted Domäne (HardwareLand/UserLand) MUSS eine **isolierte** VSpace haben (ASID ≠ 0).
  Audit: `domain_audit()` Code 3 (nur für lebende gebundene Threads).
- HardwareLand-Backend: unveränderliche Partner-Bindung an genau einen TrustedSas + genau einen
  Kanal (bei Erzeugung fixiert).

## 6. SMMU unter QEMU (ehrlicher Befund, ADR 0008)

QEMU 11 übersetzt **emulierte** Geräte-DMA **nicht** durch die SMMUv3 (kein `smmuv3_translate`/
`smmu_ptw` selbst bei `V=0`-STE, Gerät hinter Root-Port). Daher ist die **Level-2-SMMU-Durchsetzung
unter QEMU für emulierte Geräte nicht beobachtbar** (`cj_smmu_enforced == false` ist korrekte
Telemetrie, kein Bug). Die demonstrierbare Durchsetzung ist **Level 1** (Software-Bounds,
`region_contains`): der vertrauenswürdige Treiber validiert jede Geräte-Adresse vor dem
Programmieren. Auf realer HW (z. B. STM32MP25) ist die installierte Stage-1-STE der HW-Backstop.

## 7. Audit-Katalog (aggregiert in `ipc_audit`)

| Bereich | Funktion | Codes |
|---|---|---|
| Endpoint/Notification/Scheduler | `ipc_audit` (Basis) | 1–3, 10+n |
| Cap-CDT/Refcount | `audit_cdt` | 20+n |
| Domänen-Policy | `domain_audit` | 30+n (1=HW-Cap, 2=PdControl, 3=VSpace) |
| DMA-Policy + Enforcer + Revoke-Ordnung | `dma_audit` | 40+n (1=Bounds, 2=Überlappung, 3=Enforcer, 4=Ctx-Region ohne Cap) |
| VSpace W^X / Tabellen | `vspace_audit` | separat |
| TrustedSAS-Key-DB + Trust-Gate (ext-28) | `loader::trust_audit` | separat (1=DB leer, 2=key_id≠fingerprint, 3=Dublette, 4=gültig abgelehnt, 5=manipuliert akzeptiert) |

`ipc_audit() == 0` bei jedem Quiescenz-Punkt + zwischen allen Fuzzer-Operationen = alle obigen
Invarianten halten.

## 8. Adversariale Validierung von außen (ext-27, ADR 0012)

Die Isolations-Invarianten (insbesondere #1 Hardware-Adressraumtrennung, #5 Domänen-/Cap-Policy)
werden zusätzlich durch **extern geladene Drittsoftware** geprüft: sechs adversariale EL0-Dienste
(`tests/services/`, 2 je Domäne), vom Binary-Loader geladen, greifen den Kernel + sich gegenseitig
ausschließlich über die Syscall-ABI an. Schlüsselaussagen, empirisch bestätigt:

- **Hardware-Isolation ist domänen-unabhängig.** Ein geladener Dienst **jeder** Domäne (auch
  HardwareLand und TrustedSAS) faultet beim Lesen von Kernel-RAM aus EL0 (`el0-trap FAR=0x40000000`),
  wird terminiert, der Kernel überlebt. Trust befreit **nicht** von der MMU-Trennung.
- **Trust = Cap-Autorität, nicht Privileg.** Ein TrustedSAS-Dienst ohne tatsächlich gehaltene
  `PdControl`/`Loader`-Cap erhält auf PDCTL/LOAD/KILL `ERR_BADCAP` — die Domäne allein gewährt keine
  Operationsmacht.
- **Cross-Service-Nicht-Interferenz unter Nebenläufigkeit:** drei Angreifer dreier Domänen gleichzeitig
  → keiner stört die korrekte Abweisung eines anderen, ein kernel-geschütztes Canary bleibt
  bit-genau unberührt, alle Audits 0.

Aus einem EL0-Prozess sind **nur** ABI-Operationen ausdrückbar; Cap-/CDT-Operationen (kein Syscall)
bleiben im In-Kernel-Selbsttest (`captest`/`fuzz`/`ipcfuzz`). Vollständige Matrix:
`docs/phase-reports/ext-27-adversarial-tests.md`.

## 9. TrustedSAS-Zertifikate / Trust-Gate (ext-28, ADR 0014)

`DOMAIN_TRUSTED` behält seine **Cap-Autorität** (darf `PdControl`/`Loader`-Caps halten) — daher gilt
für sie eine zusätzliche Lade-Invariante. Maßgeblich: `kernel::loader::verify_image`. Belege:
`loadhw`/`load`/`aggrt`/`intrt`/`cross` (Selbsttest) + `certfuzz`/`trust_audit` (Fuzzer/Audit).

- **Trust-Gate-Invariante.** Eine TrustedSAS-PD entsteht **nur** aus einem Image mit gültigem,
  auf genau dies Binary gebundenem Ed25519-Zertifikat. Strukturell: `load_image`/
  `load_program_into_pd` rufen `verify_image` **vor** jeder Ressourcenvergabe; ein abgelehntes Image
  erzeugt **weder** Thread **noch** PD (`LoaderError::Unverified`). UserLand/HardwareLand sind
  ausgenommen (hardware-isoliert, kein Zertifikat).
- **Bindung (alles signiert über die gesamte Nachricht).** `binary_hash == SHA-256(ELF)`,
  `manifest_hash == SHA-256(Manifest)`, `program_id`/`version == Archiv-Eintrag`,
  `version >= MIN_VERSION[program_id]`, `unsafe_status == ALL_PASS`, `key_id ∈ TRUSTED_KEYS` (nicht
  `revoked`). Bricht **eine** Bedingung → Ablehnung.
- **Schlüssel-Invariante.** Der Kernel hält **nur** öffentliche Schlüssel; die Key-DB
  (`trusted_keys.rs`) ist kompiliert + read-only, **nur** per Firmware-/Kernel-Update änderbar — es
  existiert **kein** Syscall dafür. `verify_strict` (nicht `verify`) → keine Signatur-Malleability.
- **Unsafe-Invariante (host-erzwungen).** Ein zertifiziertes TrustedSAS-Programm ist
  `#![forbid(unsafe_code)]`; `unsafe` existiert im gesamten App-Dep-Baum **nur** in der Allowlist
  `{libcaprock}`. `tools/sign_trusted.py` verweigert sonst das Zertifikat; der Kernel verlangt
  `unsafe_status == ALL_PASS`.
- **`trust_audit()` (Laufzeit-Oracle).** Key-DB-Selbstkonsistenz (`key_id == fingerprint(pubkey)`,
  Eindeutigkeit, nicht leer) **plus** Live-Test: ein bekannt gültiges Zertifikat wird akzeptiert,
  eine manipulierte Kopie abgelehnt → das Gate setzt zur Audit-Zeit aktiv durch.

Eingefrorenes Zertifikatsformat + Sicherheitsanalyse:
`docs/phase-reports/ext-28-trusted-certificates-report.md`. Schlüssel-Runbook:
`docs/runbook-trusted-keys.md`.


## 10. Härtung ext-29: Cap-Budget, Datenremanenz, Spekulation

Vier Invarianten aus dem Sicherheits-Review (Details + Herleitung:
`docs/phase-reports/ext-29-hardening.md`).

### 10.1 Kein Cap-Leck beim IPC-Grant (Cross-PD-DoS)

Der globale `CapSpace` ist eine **systemweit geteilte** Tabelle fester Größe — jede unerreichbar
gewordene Cap darin ist ein permanenter Verlust **für alle PDs**.

- **Invariante.** Ein `REPLY`-Grant, der eine bereits im Empfangs-Slot liegende Cap verdrängt, gibt
  die verdrängte Cap frei. Nach *n* Grants derselben Quell-Cap in denselben Slot existiert **genau
  eine** lebende Ableitung. Strukturell: `grant_cap` **meldet** die verdrängte Cap zurück (es löscht
  sie nicht selbst — die Finalisierung nimmt `MEM` bzw. bricht Calls ab, unter dem dort gehaltenen
  `CAPS`+`EPS` wäre das sperrordnungswidrig), und `dispatch` löscht sie, **nachdem** beide Locks
  gefallen sind.
- **Test.** `grantlk` — 65 Grants in denselben Slot → `child_count(Quell-Cap) == 1`, Cap weiter
  nutzbar, `cap_audit_cdt() == 0`. Sensitivitätsgeprüft: mit deaktiviertem Fix meldet der Test 65.

### 10.2 Cap-Budget je PD

`NCAPS` begrenzt nur den lokalen Index-Adressraum einer PD, nicht ihren Verbrauch an globalen Slots
(`NPDS * NCAPS` ≫ Tabellengröße).

- **Invariante.** Keine PD belegt mehr als `CAP_BUDGET_PER_PD` Slots gleichzeitig; Überschreitung
  wird **abgewiesen** (kein Eintrag, keine Ableitung). Ein **Ersetzen** eines belegten Slots
  verbraucht nichts und bleibt erlaubt. Erzwungen an **allen** Eintragspfaden: `install_cap_checked`
  und `grant_cap` (sonst wäre der Grant das Schlupfloch um die Schranke).
- **Test.** `budget` (Boot-Selbsttest).

### 10.3 Keine Datenremanenz über Subjektgrenzen

- **Invariante.** Jede vom Kernel vergebene RAM-Region ist bei der Vergabe **genullt**. Erzwungen
  an genau einer Stelle: `system::mem_alloc` (alle Allokationen laufen darüber, nicht über
  `MEM.lock().alloc`). Genullt wird bei der **Vergabe**, nicht bei der Rückgabe — das deckt auch
  fabrikfrisches RAM mit Firmware-Resten ab und ist robust gegen Freigabepfade, die eine Region
  ohne `free` verlieren.
- **Test.** `zerotest` (Boot-Selbsttest): frische Allokation genullt; Muster schreiben → freigeben →
  dieselbe Region erneut allozieren → wieder genullt.

### 10.4 Spekulations-Härtung (Spectre-Klasse)

- **Invariante.** Jeder Tabellenindex, den EL0 beeinflusst (Cap-Slot, Endpoint-/Notification-Id),
  wird nicht nur architektonisch geprüft, sondern zusätzlich **datenabhängig** maskiert
  (`cpu::array_index_nospec`, Linux-Manier: arithmetische Maske + `CSDB`) — das überlebt auch eine
  falsch vorhergesagte Verzweigung. Beim VSpace-Wechsel steht eine Spekulationsbarriere (`SB`, sonst
  `dsb sy; isb`).
- **Sichtbar beim Boot.** `spec : CSV2=… CSV3=… FEAT_SB=…` meldet, was die HW von sich aus
  garantiert (CSV2 → Spectre-v2-immun, CSV3 → Meltdown-immun).
- **NICHT abgedeckt (bewusst, offen).** Cache-/Timing-Seitenkanäle zwischen PDs — dafür bräuchte es
  Cache-Partitionierung/Coloring (Architekturänderung, kein Patch). Ebenso Spectre-v2 auf HW **ohne**
  FEAT_CSV2: die Gegenmaßnahme wäre Predictor-Invalidierung per Firmware-Call
  (SMCCC_ARCH_WORKAROUND_1), den QEMU `virt` nicht anbietet.


## 11. Thread-Migration + Boot-Kapazitäten (ext-30)

Details + Herleitung: `docs/phase-reports/ext-30-migration-und-kapazitaet.md`.

### 11.1 Identität ist von der Platzierung getrennt

- **Invariante.** Die `gid` einer [`ThreadId`] ist **lebenslang stabil** und unabhängig vom
  besitzenden Kern. Alle per-Thread-Kerneltabellen (FP-Kontext, `VSPACE_OF`, Kernel-Stack-Slot)
  sind über sie indiziert und überleben eine Migration unverändert; eine Tcb-Cap bezeichnet
  nach der Migration denselben Thread.
- **Invariante (Directory-Kohärenz).** Für jeden belegten TCB gilt: der Directory-Eintrag
  seiner `gid` ist `used`, trägt dieselbe Generation und zeigt auf **genau** den Kern und den
  lokalen Slot, an dem der TCB liegt. Maschinell geprüft: `Scheduler::audit()` Code **8**,
  aggregiert über alle Kerne in `system::sched_audit_all()`.
- **Invariante (stale Handles).** Beim Thread-Ende wird der Directory-Eintrag **sofort**
  ungültig gemacht (`used = 0`, Generation +1); die `gid` kehrt erst beim **Reap** in die
  Freiliste zurück. Zwischen beiden Zeitpunkten ist sie nicht neu vergebbar — ein altes Handle
  kann also nie einen *anderen* Thread treffen.

### 11.2 Wettlauf „Besitzer nachschlagen ↔ Migration"

- **Invariante.** Jeder kernübergreifende Zugriff prüft nach dem Sperren **erneut**
  (`resolve` vergleicht Generation *und* Kern). Ein Fehlschlag mit gewechseltem Besitzer wird
  mit dem neuen Besitzer wiederholt (`system::with_owner`, begrenzt auf `MIGRATION_RETRIES`);
  ein Fehlschlag ohne Besitzerwechsel ist ein echter Fehlschlag.
- **Konsequenz:** aus einer `ThreadId` darf **nie** ein Kern abgeleitet werden (das ging bis
  ext-29 arithmetisch). Einzige Quelle ist das Directory.

### 11.3 Was nicht migriert werden darf

- Der **laufende** Thread (Zustand im aktiven Trap-Frame), ein Thread mit aktiver
  **Budget-Donation** (die Links sind lokale Slots → Donation ist intra-core), und der
  **Idle**-Thread. Strukturell erzwungen in `Scheduler::detach_for_migration`.
- **Ein für einen Debugger angehaltener Thread** (`BlockReasons::DEBUG`, Z6b). Ebenfalls
  strukturell in `detach_for_migration` erzwungen — und das ist eine **Invariante, keine
  Beobachtung**: gemessen am 2026-08-20 konnte es ohnehin nicht vorkommen, weil
  `migration_candidate` nur die Ready-Queues läuft und ein Thread mit nicht-leerer Grundmenge dort
  nicht steht. Aber `migrate_to(tid, dst)` nimmt eine **beliebige** `tid` und prüft den Zustand
  nicht; die Eigenschaft hinge damit an der Disziplin jedes künftigen Aufrufers.

  **Warum sie gebraucht wird:** `Scheduler::debug_release_pd` läuft die TCB-Tabelle **je Kern**
  unter dessen Sperre. Ein Thread, der während des Durchlaufs von einem bereits besuchten Kern auf
  einen noch nicht besuchten wandert, wird von **keinem** der beiden gesehen — und bliebe mit einem
  Grund liegen, den niemand mehr entfernen darf. Ohne die Invariante bräuchte die Freigabe einen
  Wiederholungslauf, und ein Wiederholungslauf über Kerngrenzen ist genau die Sorte Schleife, die
  hier niemand messen kann.

  **Eng und nicht breit, und das ist eine Entscheidung.** Die naheliegende Fassung („nur Threads
  aus einer Ready-Queue") verböte auch die Migration IPC-wartender Threads und änderte den
  Lastausgleich in einer Weise, die hier niemand gemessen hat. D8 ist der Beleg, dass die breitere
  Behebung die schlechtere sein kann.
- **Ein Thread im Gruppenschnitt** (`BlockReasons::FREEZE`, Z23/S3, 2026-08-21). Derselbe
  Mechanismus, dieselbe Stelle — aber die Folge ist schärfer. `freeze_pd` läuft die TCB-Tabelle
  ebenfalls **je Kern** und prüft danach, ob noch ein Teilnehmer auf einem Kern steht. Ein
  Teilnehmer, der während des Durchlaufs von einem besuchten auf einen unbesuchten Kern wandert,
  wird von keinem der beiden gesehen: die Gruppe wäre **halb** eingefroren — und genau die Aussage
  des Schnitts ist, dass es diesen Zustand nicht gibt (§1d). Bei `DEBUG` bliebe ein Thread liegen;
  hier bricht eine Zusicherung über eine **Menge**.
- **Push statt Pull:** die Migration läuft immer auf dem **abgebenden** Kern, weil nur er den
  Lazy-FP-Kontext des Migranten aus seinen eigenen FP-Registern sichern kann.
- **Kein Thread-Verlust:** scheitert die Aufnahme (Zielkern voll), wird der Migrant beim
  Quellkern wieder eingehängt.

### 11.4 Kapazität

- **Invariante.** Kerntabellen werden **einmalig beim Boot** dimensioniert
  (`system::configure`) und leben bis zum Reboot; es gibt keinen Pfad, der sie freigibt oder
  ein zweites Mal anhängt. Zugriffe sind bounds-geprüft (`Slab`/`AtomicTable` panieren bei
  Überschreitung wie ein Array — kein UB).
- **Invariante.** Die Hosting-Kapazität eines Kerns liegt über seinem Anteil
  (`MIGRATION_HEADROOM`), sonst könnte kein Kern einen Migranten aufnehmen.
- **Test.** `scale` — 1024 Threads gleichzeitig, danach vollständige Rückgabe (Slots +
  Stack-RAM), Scheduler-Audit 0.


## 12. Cache-Partitionierung zwischen PDs (A1) — und ihre Grenzen

**Invariante:** Zwei über `system::spawn_isolated_colored` **oder über den Lader mit
`POLICY_EXCLUSIVE_STRIPE`** erzeugte PDs mit disjunkten Farbsätzen
teilen sich **keine Cache-Farbe des Last-Level-Cache** — weder in ihrer privaten Region noch im
Kernel-Stack noch in ihren obersten Seitentabellen. Geprüft im Lauf (`color : ALL PASS`), die
Arithmetik zusätzlich auf dem Host (`caprock-mem`, `hal::cache_decode`).

**Grundlage:** Der LLC ist physisch indiziert. Die Set-Indexbits oberhalb des Seitenoffsets hängen
damit an der Physadresse und sind seitenkonstant — das ist die *Farbe*. Die Geometrie wird
**gemessen** (`hal::cache`: CPUID-Blatt 4 bzw. CLIDR/CCSIDR), nicht angenommen.

**Auf beiden Zweigen geprüft (seit 2026-08-01).** Der Test lag bis dahin im x86-Hochlauf und lief
auf aarch64 nicht — die CLIDR/CCSIDR-Fassung war übersetzt, aber nie an einer echten Zuteilung
geprüft. Er liegt jetzt arch-neutral in `kernel/src/colors.rs` und wird von beiden Wegen gefahren
(wie `kernel/src/dmatests.rs`), gemeldet aus **einer** Druckstelle. Die *Geometrie* wurde auf
aarch64 schon vorher gelesen (siehe `todo-B` B-2.2, gegen drei CPU-Modelle mit verschiedenen
Werten); ungeprüft war die **Zuteilung**. Unter QEMU `virt`: L2, 1024 KiB, 16-fach, 64 B/Zeile,
1024 Sets → 16 Seitenfarben.

### Was diese Zusicherung ausdrücklich NICHT umfasst

Diese Liste ist der wichtigere Teil des Abschnitts. Eine Isolationszusage, deren Grenzen man nicht
kennt, wird im Betrieb überdehnt.

* **Kernlokale Strukturen.** L1, L2, TLB, Store-Buffer und Branch-Predictor werden von
  Geschwister-Hyperthreads desselben physischen Kerns geteilt. **Dagegen hilft keine Farbe, und
  zwar prinzipiell nicht** — die Farbe wirkt über den Set-Index des LLC, nicht über die
  kernlokalen Strukturen.

  **Seit 2026-08-17 ist das nicht mehr nur beschrieben, sondern erzwungen** — siehe §12a.

### 12a. SMT: a physical core carries at most one logical CPU (Z6 stage 1)

**The claim.** At most one logical CPU per physical core is ever admitted. Sibling hyperthreads of
an admitted CPU are not brought up at all, so no two subjects — of any kind — are ever co-resident
on one physical core.

**Why nothing weaker would do.** Every other mitigation this kernel owns is *switch-shaped*:
colouring picks cache sets, scrubbing happens at a context switch. Both need a moment at which one
subject stops and another starts. SMT has no such moment — the sibling runs *simultaneously*. This
is the same reason the vendors' own MDS mitigation (clearing buffers on kernel exit) does not cover
the cross-sibling direction: there is nothing "in between" to clear. Exactly one measure works, and
it is scheduling.

**How it is established.** `caprock_hal::smt` reads `CPUID.1Fh`/`0Bh` on x86 and `MPIDR_EL1.MT` on
aarch64, and the bring-up loop claims one logical CPU per physical core before `cpu_on`. It is
established *there* because that is the last moment the question is open: once a sibling is online
it is a core the balancer may place anything on.

**Fail-closed, and `Unknown` is not `Single`.** A topology that cannot be read admits **only the
boot CPU**. A decode that fails must not look like a decode that found one thread per core — that
is the `NOSEL_TEXT` failure (a measurement that did not happen reading as one that passed).

**What is checked, and what is not.** The report line `smt` gates in `all_done()` on both
architectures; the verdict is recomputed from the IDs that actually came online rather than read
back from the policy's own bookkeeping. But under QEMU the sibling relationship is **emulated**:
`-smp cores=n,threads=2` hands the guest the CPUID enumeration, while the vCPUs are ordinary host
threads with no shared execution units. **Verified is the policy, never the channel** — the same
honest limitation as the SMMU-under-QEMU finding in §6 (ADR 0008). And in the main suites the line
is additionally *vacuous* (`-smp 4` means `threads=1`, so there is no sibling to suppress); that the
policy bites is measured separately by `tools/smt-messen.sh`.

#### The decision that stage 3 will have to make — recorded before it is needed

Stage 1 is crude and therefore complete: with siblings never co-resident, it covers
tenant-against-tenant **and** tenant-against-kernel. It also costs the entire SMT throughput —
measured on a 13th-gen Intel part on 2026-08-17, a sibling is worth **×1.36** on issue-bound work
and **×1.89** on latency-bound work, i.e. close to a second physical core exactly where the target
workloads sit.

Stage 3 (gang-scheduling siblings of one trust domain) buys that back, and it **does not** cover
tenant-against-kernel: once two siblings of one domain run together, a syscall or interrupt taken
on one runs **kernel** code alongside **user** code of the other — and the kernel holds secrets of
other tenants (the trusted-key store, foreign frames on the sidecar path). Linux's core scheduling
has exactly this hole; the sibling-stunning half that would close it was the expensive part and
never landed. For an IPC-heavy microkernel it is more expensive still, because kernel entry is the
common case rather than the rare one.

**So stage 3 must choose, explicitly, one of:**

1. **Kernel entry joins the gang semantics** — stun the sibling while one thread is in the kernel.
   Correct, and it taxes the operation this kernel does most.
2. **The claim is narrowed to tenant-against-tenant**, in writing, here — and then the kernel's own
   secrets need a different answer (per-tenant key material, no foreign frames resident during
   user execution).

**This decision must fall before the trust-domain concept is written**, because it determines what
a trust domain has to mean for the scheduler. Recording it after the fact would repeat exactly the
form this section exists to prevent: *a claim that sounds broader than it is.*
* **Der reguläre Weg — seit 2026-08-07 gefärbt, und zwar über das Manifest.** Ein Programm, dessen
  Manifest-Eintrag `POLICY_EXCLUSIVE_STRIPE` trägt, wird **stückweise aus einem Streifen** geladen:
  PT_LOAD-Segmente, User-Stack, Seitentabellen und EL0-Kernel-Stack. Fail-closed — ist kein
  Streifen frei, entsteht die PD gar nicht erst. Geprüft als `pdcolor : ALL PASS`, gemessen an der
  **Teardown-Buchhaltung** (`system::loaded_frames_of`), nicht am Ladepfad: ein Lader, der
  bestätigt, was er selbst getan hat, bestätigt nichts.

  Die frühere Begründung, warum das nicht ginge, war zur Hälfte falsch: „Segmente kommen
  zusammenhängend aus `mem_alloc`" beschrieb die damalige *Allokation*, nicht eine Notwendigkeit —
  gemappt wurde längst seitenweise, physische Zusammenhängung wird gar nicht gebraucht.

  **Was weiterhin ungefärbt ist:** `spawn_isolated` (2-MiB-Region, ein Blockdeskriptor) — 512
  Seiten überstreichen jede Farbe mehrfach, das ist Arithmetik und kein Zuteilungsproblem. Der Weg
  ist kernel-intern (Selbsttests, Sonden); der **Produktpfad** ist der Lader, und der ist gefärbt,
  wenn das Manifest es sagt. Wer `spawn_isolated` benutzt, bekommt keine Farbzusage.
* **Mehr PDs als Streifen — behoben (B-4.2), aber die Grenze bleibt.** `claim_stripe` führt
  Belegung; ist kein Streifen frei, gibt es `None`, und die PD entsteht **gar nicht erst**. Es gibt
  keine ungefärbte Rückfallebene. Die Zusicherung wird also nicht mehr still schwächer — sie hört
  auf, vergeben zu werden. Der Preis ist derselbe in anderer Form: **mehr als `PARTITIONS`
  gleichzeitig getrennte PDs gibt es nicht**, und der Aufrufer muss den Fehlschlag behandeln.
  (`colors::mask_for`, rundläufig und ungeprüft, steht nur noch im Selbsttest.)
* **Wirkung, nicht nur Zuteilung — und diese Zusicherung gilt nur auf Blech.** Belegt ist, dass
  die Farbsätze disjunkt *sind*. **Nicht** belegt ist, dass daraus messbar geringere gegenseitige
  Verdrängung folgt. Der Prime+Probe dafür existiert (`colors::run_prime_probe`, B-4.5), aber:

  **In einer virtuellen Maschine ist die Aussage prinzipiell nicht entscheidbar.** Der Gast färbt
  *gastphysische* Adressen; die zweite Übersetzungsstufe (EPT/Stage-2) bildet jede 4-KiB-Seite auf
  eine beliebige Wirtsseite ab, und die Farbbits liegen **oberhalb** des Seitenoffsets. Sie
  überleben die Abbildung nicht. Gemessen am 2026-08-01 unter KVM, Zyklen je Kettenglied:

      ungestoert=41   disjunkt=234   gleichfarbig=210

  Der disjunkte Farbsatz schützt dort **nicht**. Das widerlegt A1 nicht — es zeigt, dass A1 eine
  Aussage über die *physische* Adresse ist und ein Gast diese nicht besitzt. Wer Caprock als Gast
  betreibt, darf sich auf §12 **nicht** verlassen, solange der Wirt nicht farberhaltend hinterlegt
  (z. B. 1-GiB-Seiten). Der Test erkennt den Gastfall (`CPUID.1:ECX[31]`) und meldet SKIP mit
  Begründung, statt eine Zahl zu liefern, die etwas anderes bedeutet, als sie zu bedeuten scheint.
* **`1` ist kein Erfolgswert.** Meldet die Plattform keine Cache-Geometrie, gibt es genau eine
  Farbe. „Die Farbsätze zweier PDs sind disjunkt" wäre dann strukturell wahr, ohne geprüft zu
  sein. `colors::usable()` trennt das, und der Test meldet **SKIP statt PASS**.

### TrustedSAS ist ausschließlich für eigenen Code

**Gesetzte Regel (Simon, 2026-07-29): Kein Kundencode läuft jemals in einer TrustedSAS-PD.** Das
folgt aus dem Modell und ist keine Vorsichtsmaßnahme: TrustedSAS-PDs teilen sich einen Adressraum,
und ihre Isolation ruht auf *intralingualer* Sicherheit — safe Rust kann keinen Zeiger auf fremden
Speicher erzeugen. Diese Voraussetzung lässt sich nur an **Quellcode** prüfen (das Gate aus §9 tut
genau das: `unsafe_status == ALL_PASS`), niemals an einem fremden Binary.

Folgerungen: der **isolierte** Pfad ist der Produktpfad, und seine Leistung ist das Produkt. Das
Zertifikats-Gate ist ein **Betreiberwerkzeug, keine Kundenschnittstelle** — ein Weg, auf dem ein
Kunde ein TrustedSAS-Zertifikat erlangen könnte, wäre ein Entwurfsfehler. Und Attestierung
(`TODO.md` Z7) betrifft die **Maschine**, nicht das Kundenbinary: der Kunde will wissen, *worauf*
er läuft, nicht beweisen, *was* er mitbringt.

---

## 13. Was **nicht** austauschbar ist (A-4.5)

Hot-Reload ist im Projekt als Fähigkeit beschrieben (Phase 7: eine Server-PD wird über *dieselbe*
Endpoint-Cap ersetzt). Eine Fähigkeit ohne benannte Grenze wird im Betrieb überdehnt, deshalb steht
hier die Gegenliste. Sie ist **normativ**: was hier steht, wird abgewiesen, nicht „nach Möglichkeit
vermieden".

**Der Kernel selbst.** Er ist kein austauschbares Objekt. Das ist keine Bequemlichkeit, sondern
folgt aus §9 und A-1.3: das System-Manifest ist **an das Kernel-Image gebunden** (SHA-256 über
`[__text_start, __rodata_end)`). Ein getauschter Kernel entwertet jede Signatur, die auf ihn
lautet — die gesamte Anfangsverteilung von Autorität wäre danach unbelegt. Kernelwechsel heißt
Neustart mit neuem Manifest, und dafür gibt es einen Namen, der nicht „Hot-Reload" ist.

**Alles, was eine Cap auf maschinenlokale Hardware hält, solange sie in Benutzung ist.** Konkret:

* **IOMMU-Kontexte** — VT-d-Root-/Kontexttabellen, SMMU-STE/CD. Diese Strukturen werden von der
  *Hardware* gelesen, nicht vom Kernel. Ein Austausch der besitzenden PD, während ein Gerät
  überträgt, tauscht Tabellen unter einem laufenden DMA-Zugriff — der Kernel sieht davon nichts,
  weil der Zugriff nicht durch ihn läuft.
* **Aktive DMA-Regionen.** Dasselbe Argument eine Ebene tiefer: die Region ist einem Gerät
  zugesagt. „Die PD ist weg" ist für ein Busmaster-Gerät keine Aussage.
* **IRQ-Zustellung an ein HardwareLand-Backend**, solange die IRQ-Cap gebunden ist. Ein Interrupt,
  der zwischen alter und neuer Instanz eintrifft, gehört keiner von beiden.

Das Teardown-Token (ext-37) ist der bestehende Mechanismus für genau diese Fälle und die richtige
Grundlage: austauschbar ist eine PD erst, wenn sie ihre Geräte abgegeben hat.

### Und eine Grenze, die neu ist: gefärbte PDs brauchen einen freien Streifen

Seit B-4.2 wird die Vergabe der Farbstreifen **geführt**, und es gibt `PARTITIONS` davon (heute 4).
Ein Hot-Reload erzeugt die neue Instanz, **bevor** die alte verschwindet — das verlangt A-4.1
(atomares Umbinden: zwischen „alter Server weg" und „neuer Server empfangsbereit" darf kein
Zustand liegen, in dem ein `CALL` mit `NoEndpoint` scheitert). Beide existieren also kurzzeitig
gleichzeitig, und beide brauchen einen eigenen, disjunkten Streifen.

**Folge, unbequem und deshalb hier notiert:** sind alle Streifen vergeben, ist eine gefärbte PD
**nicht** hot-reloadbar. Bei vier Streifen und vier gefärbten PDs schlägt jeder Austausch fehl —
sauber (die neue PD entsteht gar nicht erst, s. B-4.2), aber er schlägt fehl. Wer Hot-Reload für
gefärbte PDs zusagt, muss entweder einen Streifen freihalten oder die Färbung anfordernd machen
statt pauschal (`POLICY_EXCLUSIVE_STRIPE`, A-1.4).

Der naheliegende Ausweg — die neue Instanz übernimmt den Streifen der alten — ist **keiner**:
solange beide leben, teilten sie sich dann die Farben, und genau diese Überschneidung soll B-4.2
verhindern. Ein Sonderfall „nur ganz kurz" wäre die stille Aufweichung, gegen die der ganze
Abschnitt geschrieben ist.

---

## 14. Fehlerdomäne (B-6.2 / Z9)

**Der Knoten ist die Fehlerdomäne.** Ein Fehler *im Kernel* kann jede PD auf diesem Knoten treffen;
Ausfallsicherheit wird **über Knoten hinweg** gebaut. **Alle PDs im globalen SAS-Adressraum
(`VSPACE_OF == 0`) bilden zusätzlich untereinander EINE Fehlerdomäne** — ihre Trennung ruht auf §12
(intralinguale Sicherheit, Zertifikats-Gate §9), nicht auf der Hardware. Nur TrustedSAS-PDs dürfen
dort laufen (`domain_audit` Code 3), und das ist der Grund für die gesetzte Regel „kein Kundencode
in TrustedSAS" — nicht ihre Begründung im Nachhinein. **Die Zusicherung hängt am Adressraum, nicht
am Domänen-Etikett:** `Domain::TrustedSas` darf global *oder* isoliert laufen, und extern geladene
TrustedSAS-PDs bekommen heute eine eigene VSpace (`kernel/src/loader.rs:719–722`) — für die gilt
die Hardwaretrennung wie für jede isolierte PD.

**Eingegrenzt ist und bleibt der Fault einer isolierten PD** (EL0/Ring 3): er beendet genau ihren
Thread, gibt IPC-Queues, Kernel-Stack, VSpace und Farbstreifen frei, und der Kernel läuft weiter
(`kernel/src/system.rs:605–637`; geprüft als `ring3`/`el0iso`/`iso`/`vspace`).

**Gemessen am 2026-08-02, und die bisherige Notiz stand verkehrt:** ein Kernel-Panic reißt den
Knoten **nicht zuverlässig** mit — er wird in den häufigsten Fällen *verschluckt*. `panic.rs` ruft
ein `halt()`, das die Interrupts **nicht maskiert** (`kernel/src/arch/x86_64/mod.rs:294`
`loop { hlt }` ohne `cli`; aarch64 `loop { wfe }` mit unverändertem DAIF), und der nächste
Timer-Tick holt den Kern in den Scheduler zurück. Vier verschiedene Ausgänge für dieselbe Ursache,
je nachdem *wo* der Panic auftrat — Belege, Zahlen und die Liste der ungeprüften Fälle in
[docs/fehlerdomaene.md](fehlerdomaene.md).

**Für diesen Abschnitt heißt das:** die Sperrordnung aus §1 sichert Verklemmungsfreiheit im
**fehlerfreien** Betrieb. Sie sagt nichts über einen Kern, der *mitten in* einem kritischen
Abschnitt stehenbleibt. Der Ticket-Lock (`crates/caprock-sync`) hat keine Schranke; ein Panic
unter `MEM` oder `CAPS` friert damit alle Kerne ein, ohne eine weitere Zeile auszugeben — gemessen.
Wer §1 erweitert, erweitert damit **nicht** die Fehlerdomäne.
