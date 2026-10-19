# ext-30 — Thread-Migration + flexible Kapazitäten (Abschlussbericht)

Backlog: [TODO.md](../../TODO.md) B + C. Invarianten: [invariants.md §1, §11](../invariants.md).

## Ziel

Zielbild ist eine Maschine der **Dual-EPYC-Klasse: 256 Kerne, viele tausend Prozesse**. Zwei
Dinge standen dem strukturell im Weg:

1. **Threads waren fest an ihren Erzeugerkern gebunden.** Der Lastausgleich konnte nur bei der
   *Platzierung* wirken; danach war die Verteilung endgültig.
2. **Alle Kapazitäten waren Compile-Zeit-Konstanten** in `.bss`-Arrays (8 Kerne × 64 TCBs =
   512 Thread-Slots, `[FpState; 512]`, `[AtomicU64; 512]`, …). Für 256 Kerne hätte man für den
   schlimmsten Fall dimensionieren müssen — und das Image trüge die Arrays auch auf einer
   kleinen Maschine.

Beides ist behoben. Zusätzlich sind die Pfade, die bei tausenden Threads zum Engpass würden,
von linearen Scans auf O(1) umgestellt.

## 1. Warum Migration blockiert war: Identität hing an der Platzierung

```
vorher:   ThreadId.slot  =  core * PER_CORE + local        // der Kern STECKT in der Identität
nachher:  ThreadId.gid  ──> Directory[gid] = (used, gen, core, local)
```

Vorher war der besitzende Kern aus der `ThreadId` *ausgerechnet* (`slot / PER_CORE`). Ein
Thread konnte den Kern also gar nicht wechseln, ohne eine **andere ThreadId** zu bekommen —
und damit hätten alle Tcb-Caps, IPC-Queue-Einträge und per-Thread-Kerneltabellen
(`VSPACE_OF`, FP-Kontexte, Kernel-Stack-Slots) auf ihn ins Leere gezeigt.

Jetzt gibt es ein **Thread-Directory**: eine Tabelle von Atomics, die `gid -> (Kern, Slot)`
abbildet. Die `gid` ist lebenslang stabil; alle per-Thread-Tabellen sind über sie indiziert
und überleben eine Migration unverändert. Das Directory ist **lock-frei lesbar** — der Kernel
muss wissen, *welchen* Scheduler er sperren soll, bevor er sperrt.

### Die daraus folgende Wettlaufbedingung (und ihre Behandlung)

Zwischen „Directory lesen" und „Kern sperren" kann der Thread migrieren. Jeder
kernübergreifende Zugriff läuft deshalb über `system::with_owner`:

```
Besitzer lock-frei nachschlagen → dessen Lock nehmen → ERNEUT prüfen (`resolve`)
   → Erfolg: fertig
   → Fehlschlag: hat sich der Besitzer geändert? dann mit dem NEUEN wiederholen,
                 sonst echter Fehlschlag (Thread tot / Operation unzulässig)
```

`resolve` prüft dabei `gen` **und** Kern — ein migrierter Thread löst auf dem alten Kern
schlicht nicht mehr auf. Die Wiederholung ist auf `MIGRATION_RETRIES` begrenzt.

## 2. Migration ist ein **Push**, kein Pull

`migrate_to` wird immer vom **abgebenden** Kern ausgeführt. Das ist keine Bequemlichkeit,
sondern erzwungen: der Lazy-FP-Kontext eines Threads liegt physisch in den **FP-Registern
seines Kerns**. Nur dieser Kern kann sie sichern — ein fremder Kern könnte sie nicht einmal
lesen. Also: FP lokal ausspülen (`fp::save` in den gid-indizierten Puffer, `FP_OWNER` löschen,
Trap wieder scharf), dann übergeben.

Beide Scheduler-Locks werden in **aufsteigender Kern-Reihenfolge** genommen — zwei gleichzeitig
migrierende Kerne können sich damit nicht verklemmen.

**Nicht migriert wird:**

| Fall | Grund |
|---|---|
| der **laufende** Thread | sein Zustand steckt im aktiven Trap-Frame des Kerns |
| Thread mit aktiver **Budget-Donation** | die Donation-Links sind *lokale* Slots; eine Spende ist per Konstruktion intra-core. Nach dem REPLY ist er migrierbar. |
| der **Idle**-Thread | hat keinen eigenen Stack und ist die Rückfallebene seines Kerns |
| blockierte Threads (Policy) | sie verbrauchen keine CPU — sie zu verschieben brächte keinen Lastgewinn |

Schlägt die Aufnahme auf dem Zielkern fehl (keine Kapazität), wird der Migrant **zum Quellkern
zurückgehängt** — es geht nie ein Thread verloren.

## 3. Policy: automatischer Ausgleich (vorhanden, per Default AUS)

`balance_once()` gibt höchstens **einen** bereiten Thread je Aufruf an den am wenigsten
belasteten Kern ab, und nur ab einer Differenz von `IMBALANCE_THRESHOLD` — das dämpft
Oszillation (zwei Kerne, die sich Threads gegenseitig zuschieben). Im Tick-Pfad hängt er hinter
einem Intervall (`BALANCE_INTERVAL_TICKS`), weil Migration Cache-Lokalität kostet.

**Der Schalter `system::set_balancing()` steht per Default auf AUS.** Der Mechanismus ist
fertig und getestet; die Demo-/Testthreads *dieses Images* setzen aber teils feste Affinität
voraus (Cross-Core-IPC-Test, Budget-Donation, Platzierungs-Telemetrie). Die Trennung ist
bewusst: „Mechanismus fertig" ≠ „Policy standardmäßig an". Offen in [TODO.md](../../TODO.md) B4.

## 4. Kapazität kommt zur Boot-Zeit aus dem RAM

Neue Crate `caprock-slab`:

* `Slab<T>` — Tabelle, die `const` **leer** konstruierbar ist (statische Instanzen bleiben
  möglich) und ihren Speicher beim Boot bekommt. `Index`/`IndexMut` paniken bei Überschreitung
  wie ein Array; die einzige `unsafe`-Stelle ist `attach` (Rohspeicher → initialisierte Tabelle).
* `AtomicTable<T>` — dasselbe für Tabellen, die **lock-frei** aus jedem Kern gelesen werden
  müssen (Thread-Directory, `VSPACE_OF`). Zeiger + Länge werden Release/Acquire veröffentlicht.
* `FreeList` — O(1)-Belegen/Freigeben von Slab-Indizes (verkettete Freiliste in einem eigenen
  Slab, kein Allokator nötig).

`system::configure(cores)` dimensioniert beim Boot und legt an: Thread-Directory + gid-Freiliste,
je Kern TCB-Tabelle + Zombie-Ring + Freiliste, FP-Kontexte, `VSPACE_OF`, Kernel-Stack-Zuordnung.
Auch die **Sekundär-Stacks** kommen jetzt aus dem RAM statt aus einer Linker-Reservierung (bei
256 Kernen wären das 16 MiB tote Image-Größe).

Die Kernzahl liest der Kernel aus dem Device Tree (`Dtb::cpu_count`, neue `/cpus`-Zählung)
statt sie zu verdrahten. `MAX_CORES = 256` dimensioniert nur noch Arrays von *Locks*/Atomics
(klein), nicht die Datentabellen.

Messung im Testlauf (QEMU `virt`, 8 CPUs, 4 GiB):
```
dtb   : 8 CPUs (aus Device Tree; Kernel-Obergrenze 256)
sched : 8 Kerne, 2048 Thread-Slots (512 hostbar je Kern), Tabellen 1644 KiB aus dem RAM
```
Die Hosting-Kapazität je Kern liegt bewusst über dem Anteil (`MIGRATION_HEADROOM`) — sonst
könnte kein Kern einen Migranten aufnehmen, sobald alle ihren Anteil ausgeschöpft haben.

## 5. O(1) statt linearer Scans

Kapazität allein reicht nicht; bei tausenden Threads sind die Scans der Engpass:

| Pfad | vorher | jetzt |
|---|---|---|
| Ready-Queues | Ringpuffer je Priorität (`[usize; PER_CORE]` × 8), `remove` = dequeue/enqueue-Zyklus über die ganze Queue | **intrusive doppelt verkettete Listen** durch die TCBs — Einreihen/Ausklinken O(1), **kein** Queue-Speicher mehr |
| freien TCB-Slot finden | linearer Scan `position(!used)` | **Freiliste** O(1) |
| `load()` | Scan über alle TCBs | Zähler |
| lastärmsten Kern finden | sperrte **jeden** Kern-Scheduler nacheinander (bei 256 Kernen 256 Lock-Zyklen je Spawn) | **lock-freier** Lastzähler je Kern |
| MCS-Refill je Tick | Scan über alle TCBs, **jeden Tick** | entfällt vollständig, solange kein Budget erschöpft ist (Zähler) |
| Zombie einsammeln | `position(is_some())` über den Puffer | Ringpuffer O(1) |
| `thread_alive` / IPC-Liveness-Audit | sperrte den Zielkern | **lock-frei** über das Directory |

Der Scheduler-`audit()` prüft zusätzlich die neue Kern-Invariante (Code 8): **jeder
Directory-Eintrag zeigt auf genau den Kern + Slot, auf dem der TCB tatsächlich liegt.** Die
Duplikat-Prüfung der alten Ringpuffer entfällt — in einer intrusiven Liste ist ein Thread
strukturell nur einmal enthalten; dafür wird jetzt die **Rückverkettung** geprüft.

## 6. Tests

Zwei neue Checks in `test-qemu.sh`:

**`migrate`** — Migrant läuft nachweislich auf core 0 (er notiert bei jedem Durchlauf seine
eigene Kern-ID), wird nach core 3 migriert, **läuft dort weiter** (Kern-ID wechselt UND der
Fortschrittszähler steigt), **dieselbe Tcb-Cap** bezeichnet ihn weiterhin (Identität stabil),
`balance_once()` verschiebt auch automatisch, cross-core-`KILL` räumt ihn ab, und das
Scheduler-Audit über alle Kerne ist 0.

**`scale`** — **1024 Threads gleichzeitig** am Leben, alle über ihre ThreadId auffindbar,
danach vollständig abgebaut. Die Teardown-Buchhaltung ist bewusst **relativ** (zurückgegebene
Slots ≥ n, `reaped_bytes`-Zuwachs ≥ n × Stackgröße): die übrigen Demos laufen nebenher und
verändern absolute Zähler ständig — eine Gleichheitsprüfung wäre eine Wettlaufbedingung, kein
Beweis.

Gesamt: `./test-qemu.sh` **ALL PASS** (64 Checks), `KERNEL_FUZZ=1` **ALL PASS** inkl. aller
fünf Fuzzer (der Domänen/HW- und der IPC-Fuzzer laufen gegen die neue Scheduler-Struktur).

## 7. Was das **nicht** löst

* **Mehr als 8 Kerne sind auf ARM weiterhin nicht testbar**: die HAL spricht **GICv2**
  (8-CPU-Grenze, 8-Bit-Zielmaske in `GICD_SGIR`). Dafür braucht es GICv3 (Redistributoren je
  Kern, `ICC_SGI1R_EL1`, ITS). Die Kapazitäten sind vorbereitet, die Interrupt-Hardware nicht.
  → [TODO.md](../../TODO.md) C5.
* **Cap-/PD-/Endpoint-/Notification-Tabellen** sind weiterhin `.bss`-Arrays fester Größe.
  → TODO.md C3 (blockiert durch `ReplyFinal` auf dem Kernelstack, A3).
* **x86-64** (die eigentliche EPYC-Zielarchitektur) ist ein HAL-Port: Boot, Exceptions/Syscalls,
  MMU, APIC, Timer, IOMMU. Der Capability-/IPC-/Scheduler-Kern ist portabel, die HAL nicht.
  → TODO.md C6.
* Kernel-Threads haben weiterhin **64 KiB** Stack — bei zehntausenden Threads ist das die
  bestimmende Speichergröße, nicht die Tabellen.
