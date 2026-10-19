# Caprock — die Fehlerdomäne (B-6.2 / Z9)

**Für Betreiber und Mandanten.** Dieses Dokument sagt, **was mitgeht, wenn etwas kaputtgeht** — und
was ausdrücklich **nicht** zugesichert ist. Stand: 2026-08-02, Zweig `arch/x86_64`, Kernel `4963a19`.

Alle Zahlen und Logzeilen unten sind **gemessen**, nicht hergeleitet: ein QEMU-Lauf je Fall, mit
einem absichtlich ausgelösten Panic an einer definierten Stelle. Die Messvorschrift steht in
[§5](#5-wie-das-gemessen-wurde), damit sie wiederholbar ist.

---

## 1. Die Festlegung

> **Der Knoten ist die Fehlerdomäne.**
>
> Ein Fehler **im Kernel** kann jede PD auf diesem Knoten treffen. Wer Ausfallsicherheit braucht,
> baut sie **über Knoten hinweg**, nicht innerhalb eines Knotens. Zwei Repliken desselben Dienstes
> auf demselben Caprock-Knoten sind **eine** Replik.

Das ist die *billige* Variante aus Z9, bewusst gewählt: sie gilt ab sofort und ist ehrlich. Die
teure Variante (Kernel-Fehler auf die verursachende PD eingrenzen) ist Forschungsklasse, s. [§7](#7-was-forschungsklasse-ist).

**Was zugesichert ist — und geprüft wird:**

| Zusicherung | Beleg im Lauf |
|---|---|
| Ein Fault einer **isolierten** PD (EL0/Ring 3) beendet **nur ihren Thread**. Der Kernel läuft weiter. | `el0-trap: … -> beendet, Kernel laeuft weiter`; `ring3 : ALL PASS` (x86), `el0iso : ALL PASS` (aarch64) |
| Eine isolierte PD kann fremden Speicher **nicht** lesen — auch nicht beim Sterben. | `iso : … isolierter Thread faultete 1x an derselben Adresse` (x86), `vspace : … isol. Probe … las-X=false` (aarch64) |
| Ein ungültiger Syscall ist **kein** Fault: er liefert `ERR_BADSYS`, der Thread lebt weiter. | `caprock-microkit`, `deny(ERR_BADSYS)` |
| Beim Fault-Tod werden IPC-Warteschlangen, Kernel-Stack, VSpace und Farbstreifen freigegeben. | `purge_ipc_queues`, `reclaim_user_kstack`, `vspace_teardown` in `kernel/src/system.rs:605–637` |

**Was ausdrücklich NICHT zugesichert ist.** Diese Liste ist der wichtigere Teil. Eine Zusicherung,
die nicht sagt, was sie nicht zusichert, ist keine.

* **Kein Panic-Freiheitsversprechen.** Der Kernel ist teilweise verifiziert (Kani, Loom, Verus),
  aber nicht vollständig. Die Panic-Quellen sind gezählt, nicht geschätzt: in
  `kernel/src/system.rs` fünf (drei `expect("… RAM erschoepft")` beim Hochlauf, ein `assert!`,
  zwei `unwrap()` in Laufzeitpfaden — Zeilen 3025 und 3470), dazu Bereichsprüfungen bei
  Feldzugriffen. Arithmetik-Überläufe panicken im Release-Profil **nicht** (`overflow-checks` ist
  dort aus) — sie sind kein Panic-Grund, aber eben auch keine Meldung.
* **Keine Eingrenzung eines Kernel-Fehlers auf eine PD.** `CAPS`, `MEM`, `EPS`, `NTFNS`, `VSPACES`
  sind **knotenglobal**. Es gibt keinen Ressourcenschnitt, entlang dessen man einen Kernel-Fehler
  abschneiden könnte.
* **Keine Wiederherstellung.** Es gibt kein Kernel-`recover`, keinen Neustart eines Subsystems,
  keine Übernahme der Threads eines ausgefallenen Kerns. Ein toter Kern bleibt tot; seine
  Runqueue (`SCHEDS[core]`) wird von niemandem übernommen (Migration ist ausschließlich **Push
  durch den besitzenden Kern**, `kernel/src/system.rs:1484–1490`).
* **Kein zuverlässiges Anhalten bei einem Kernel-Panic.** Siehe [§2](#2-was-heute-wirklich-passiert-gemessen) —
  das ist der unangenehmste Befund dieses Dokuments und **schwächer** als die Festlegung oben.
* **Keine PD-Granularität bei Wartung.** Es gibt heute keine Live-Migration einzelner PDs
  (`TODO.md` Z3/Z4). Einen Knoten leeren heißt: jeden Mandanten darauf neu starten.
* **Keine Trennung zwischen PDs im globalen SAS-Adressraum.** Sie bilden untereinander **eine**
  Domäne; ihre Grenze ist eine Quelltext-Eigenschaft, keine Hardwaregrenze. Siehe
  [§3](#3-trustedsas--der-fall-der-aus-dem-modell-folgt).

---

## 2. Was heute wirklich passiert (gemessen)

Die Notiz in `TODO.md` Z9 lautete: *„Ein Kernel-Panic reißt heute den ganzen Knoten mit."*
**Das stimmt so nicht — und das Gegenteil ist keine gute Nachricht.**

Der Grund steht in zwei Zeilen. Der Panic-Handler (`kernel/src/panic.rs:9–25`) druckt und ruft
dann `halt()`. Dieses `halt()` **maskiert die Interrupts nicht**:

* x86: `kernel/src/arch/x86_64/mod.rs:294` → `loop { hlt }`, ohne `cli`.
  (Die HAL-Fassung `crates/caprock-hal/src/x86_64/cpu.rs:257` **würde** maskieren — der
  Panic-Pfad benutzt sie nicht.)
* aarch64: `crates/caprock-hal/src/aarch64/cpu.rs:203` → `loop { wfe }`, DAIF unverändert.

Waren die Interrupts beim Panic offen — und im laufenden Betrieb sind sie das —, holt der nächste
Timer-Tick den Kern aus der Halt-Schleife zurück in den Scheduler. **Der Panic ist dann eine
gedruckte Zeile und sonst nichts.**

### Die Messtabelle

| # | Fehlerart | Was heute reißt | Was der Kunde merkt |
|---|---|---|---|
| A | **Fault in isolierter PD** (EL0/Ring 3): Seitenfehler, `#UD`, `#DE` | **nur der fehlerhafte Thread**. IPC-Queues, Kernel-Stack, VSpace, Farbstreifen werden freigegeben. Die PD selbst bleibt als Slot stehen (kein `destroy_loaded`). | Sein Funktionsaufruf stirbt. Nachbarn merken nichts. **Das ist die Zusicherung, und sie hält.** |
| B | **Ungültiger Syscall / fehlende Cap** | nichts | Fehlercode (`ERR_BADSYS`/`BADCAP`), Thread läuft weiter |
| C | **Kernel-Panic in einem Kernelfaden** (EL1/Ring 0), ohne gehaltene Sperre | **genau dieser eine Faden** — er landet in einer Halt-Schleife, wird vom Timer wieder eingeplant, dreht dort für immer und belegt seinen Scheduler-Slot. Der Knoten läuft weiter. | Ein Dienst hängt, ohne zu sterben. Kein Fehlercode, kein Signal, keine Meldung außer einer Konsolenzeile. **Der Kernel arbeitet nach einer verletzten Invariante weiter.** |
| D | **Kernel-Panic auf einem Sekundärkern** | **nichts Sichtbares.** Der Kern kehrt über den Timer in den Scheduler zurück. | **gar nichts** — die Prüfsignatur ist identisch zum fehlerfreien Lauf |
| E | **Kernel-Panic unter gehaltener globaler Sperre** (`MEM`, `CAPS`, …) | **der ganze Knoten**, still. Der Ticket-Lock (`crates/caprock-sync/src/lib.rs:170–178`) hat keine Schranke: `now_serving` steht für immer, **jeder** spätere Zieher blockiert. | Totalausfall ohne jede weitere Ausgabe. Kein Watchdog, keine Diagnose. Von außen nicht von einem Hardwarehänger unterscheidbar. |
| F | **Kernel-Panic im Steuerfaden des Bootkerns** | der Steuerfaden. Der Knoten läuft weiter, **meldet aber nie wieder etwas** — Notbremse und Abschlussbericht hängen an genau diesem Faden. | sieht aus wie ein Hänger, ist ein Panic |
| G | **Panic im Panic-Handler** (Doppelfehler) | unbegrenzte Rekursion. Kein Wächter, kein `#DF`-Handler mit IST, keine Schutzseite am Kernel-Stack. | Kernel-Stack läuft still in fremden Speicher |
| H | **Fault einer PD im globalen SAS-Adressraum** | ihr Thread — die Hardware fängt den Fault wie bei jeder anderen PD. **Aber:** was sie vorher im geteilten Adressraum kaputtgeschrieben hat, bleibt kaputt, und die Nachbarn merken es nicht. | s. [§3](#3-trustedsas--der-fall-der-aus-dem-modell-folgt) |

### Die Belegstellen im Wortlaut

**Fall C** — Panic in einem Kernelfaden auf dem Bootkern, x86_64, `-smp 4`, KVM. Der Lauf läuft
danach **61 Sekunden weiter**, alle vier Kerne ticken, die anderen beiden Worker kommen auf ~4900
Runden, der gepanickte steht bei 2:

```
[KERNEL PANIC] panicked at kernel/src/arch/x86_64/bringup.rs:57:13:
B-6.2 Messung: Kernel-Panic in Kernelfaden (worker 2), Kern 0
...
bringup : WATCHDOG — nicht alle Aussagen belegt (nach 61s, 20417179 Umdrehungen)
sched   : core 0 ticks=6102
sched   : core 1 ticks=6088
sched   : core 2 ticks=6084
sched   : core 3 ticks=6080
sched   : Worker-Runden [4908, 4910, 2] (jeder >= 3 -> Timer verdraengt sie gegeneinander)
sched   : FAILURES
```

Alles andere im selben Lauf — `ipc`, `ring3`, `iommu`, `dmatok`, `quiesce`, `rebind`, `state`,
`capsz` — meldete **ALL PASS**. Der Knoten hat den Panic schlicht verdaut.

**Auf aarch64 dasselbe** (QEMU `virt`, `cortex-a72`, `-smp 8`): alle acht Kerne ticken weiter, die
gesunden Worker erreichen sechsstellige Rundenzahlen, der gepanickte steht bei 2 —

```
[KERNEL PANIC] panicked at kernel/src/threads/mod.rs:1970:17:
B-6.2 Messung: Kernel-Panic in Kernelfaden (worker 2) auf aarch64
...
sched   : core 0 ticks=6004
sched   : core 7 ticks=7006
sched   : worker 0 count=106773
sched   : worker 1 count=107023
sched   : worker 2 count=2
```

**Fall D** — Panic auf Sekundärkern 1. Die Prüfsignatur ist **Zeile für Zeile identisch** zum
fehlerfreien Lauf (31 Ergebniszeilen, `diff` leer), der Lauf endet mit `rc=0`:

```
[KERNEL PANIC] panicked at kernel/src/arch/x86_64/bringup.rs:202:9:
B-6.2 Messung: Kernel-Panic auf Sekundaerkern 1 (Idle-Kontext)
...
sched   : core 1 ticks=11
sched   : Worker-Runden [25, 26, 26] (jeder >= 3 -> Timer verdraengt sie gegeneinander)
sched   : ALL PASS
== SELFTEST COMPLETE -> system_off ==
```

Kern 1 tickt **nach** seinem Panic weiter. Wäre die Konsolenzeile nicht, wäre der Panic in diesem
Lauf durch nichts nachweisbar.

**Fall E** — Panic auf Kern 1 unter gehaltener `MEM`-Sperre. Das Log endet **mitten im Hochlauf**,
QEMU läuft ins Zeitlimit (`rc=124`). Es gibt kein `smp : 4 von 4 Kern(en) online` mehr, keine
Notbremse, keinen Bericht:

```
root    : FAILURES (NoArchive) -- kein Root-Task, der Kernel hat nichts auszufuehren

[KERNEL PANIC] panicked at kernel/src/system.rs:1077:5:
B-6.2 Messung: Panic unter gehaltener MEM-Sperre (frei=472555520)
<Ende des Logs>
```

Der Bootkern stand in `system::alloc` und wartete auf eine Sperre, deren Halter tot war.

**Fall F** — Panic im Steuerfaden des Bootkerns. Ebenfalls `rc=124`, ebenfalls Stille — obwohl der
Knoten weiterläuft:

```
smp     : 4 von 4 Kern(en) online

[KERNEL PANIC] panicked at kernel/src/arch/x86_64/bringup.rs:1075:17:
B-6.2 Messung: Panic im Steuerfaden des Bootkerns (Idle/Watchdog)
<Ende des Logs>
```

> **Ein Befund nebenbei, der `todo` D0 betrifft:** Fall F ist von außen **nicht** von einem
> Deadlock zu unterscheiden — beide zeigen „Log bricht ab, `rc=124`, keine WATCHDOG-Zeile". Wer
> einen solchen Hänger untersucht, muss zuerst ausschließen, dass er einen Panic vor sich hat.
> Die Notbremse kann das nicht leisten: sie sitzt **in** dem Faden, der gestorben ist
> (`kernel/src/arch/x86_64/bringup.rs:1065–1067` sagt das selbst).

**Fall G** — Doppelfehler. Ein Wächter existiert nicht; die Rekursion läuft, bis der Stack alle
ist. Gemessen wurden **362 Ebenen** auf einem 64-KiB-AP-Stack, ohne `#DF`, ohne Ausnahme, ohne
Schutzseite. Der Lauf endete erst, als der **Bootkern** die Maschine abschaltete — was tiefer
passiert wäre, ist damit **nicht** gemessen. Sichtbarer Nebeneffekt: die Ausgabe des sterbenden
Kerns verschränkt sich zeichenweise mit der der gesunden, weil `emit_raw` im Panic-Pfad
absichtlich sperrfrei ist:

```
B)-6.2
 Me=ss=ung : SDoppEelLfehFleTr, ETiSefeT 3 66C
```

### Was daraus folgt

**Die eigentliche Lücke ist nicht, dass ein Panic den Knoten reißt, sondern dass er es
unzuverlässig tut.** Vier verschiedene Ausgänge (C, D, E, F) für dieselbe Ursache, und welcher
eintritt, hängt davon ab, *wo* der Panic auftrat — nicht davon, *wie schlimm* er war.

Für ein Sicherheitsprodukt ist der Ausgang C/D der schlechtere von beiden. Ein Panic bedeutet:
**eine Invariante des Kernels ist nachweislich verletzt.** Danach vergibt derselbe Kernel weiter
Capabilities, teilt Speicher zu und schaltet Adressräume um. Ein Absturz wäre ein definierter
Zustand; „läuft weiter mit verletzter Invariante" ist keiner.

---

## 3. TrustedSAS — der Fall, der aus dem Modell folgt

**Erst die Präzisierung, weil hier leicht das Falsche steht.** `Domain::TrustedSas` ist eine
Vertrauens*stufe*, keine Adressraumaussage. Eine TrustedSAS-PD darf **global oder isoliert** laufen
— `domain_audit` verlangt Isolation nur für `HardwareLand`/`UserLand`, und „mehr Isolation ist nie
eine Verletzung" (`crates/caprock-microkit/src/lib.rs:113–121`, `:246–250`). Konkret heute:

* **Im Kernel erzeugte** TrustedSAS-PDs laufen im **globalen SAS-Adressraum** (`VSPACE_OF == 0`).
  Das ist der Default und der Zweck: kein Adressraumwechsel.
* **Extern geladene** TrustedSAS-PDs bekommen eine **eigene VSpace** und laufen EL0-isoliert
  (`kernel/src/loader.rs:719–722`) — hardwaregetrennt, aber mit ihrer Vertrauensstufe. Es gibt
  damit heute **keinen** Weg, auf dem eine *geladene* PD in EL1 faultet.

Die Fehlerdomänen-Aussage hängt also am Adressraum, nicht am Domänen-Etikett:

* **Ein Fault wird in beiden Fällen eingegrenzt wie jeder andere.** Die Hardware fängt ihn,
  `el0_fault` (`kernel/src/system.rs:605–637`) beendet den Thread, der Kernel läuft weiter.
* **Aber im globalen SAS-Adressraum ist der Speicher nicht getrennt.** Was eine dort laufende PD
  *vor* ihrem Fault überschrieben hat, ist überschrieben — ihre Trennung von den Nachbarn ruht
  **nicht** auf der Hardware, sondern auf *intralingualer* Sicherheit (safe Rust kann keinen Zeiger
  auf fremden Speicher erzeugen), geprüft vom Zertifikats-Gate (ADR 0014,
  `unsafe_status == ALL_PASS`) am **Quellcode**. Gemessen ist genau dieser Kontrast, den
  `iso`/`vspace` ohnehin prüfen:

  ```
  iso     : SAS-Thread las 0x5e141a4e0bedc0de (erwartet 0x5e141a4e0bedc0de); isolierter Thread faultete 1x an derselben Adresse
  vspace  : X=0x42c5f000; SAS-Probe las X=true; isol. Probe lief+IPC=true, faultete=true, las-X=false
  ```

  Der SAS-Faden **liest** fremden Speicher erfolgreich; die isolierte PD faultet an derselben
  Adresse. Das ist die Zusicherung für den isolierten Pfad — und zugleich der Beleg, dass sie im
  SAS-Pfad nicht existiert.

> **Zusicherung:** **Alle PDs im globalen SAS-Adressraum eines Knotens bilden EINE Fehlerdomäne**
> — und nur TrustedSAS-PDs dürfen dort laufen. Ein Speicherfehler in einer von ihnen kann jede
> andere treffen; die Grenze zwischen ihnen ist eine Quelltext-Eigenschaft, keine Hardwaregrenze,
> und sie fällt mit dem ersten `unsafe`-Block. Deshalb — und nur deshalb — gilt die gesetzte Regel:
> **kein Kundencode läuft jemals in einer TrustedSAS-PD** (`docs/invariants.md` §12). Ein Weg, auf
> dem ein Kunde ein TrustedSAS-Zertifikat erlangen könnte, wäre ein Entwurfsfehler.
>
> Für eine TrustedSAS-PD mit **eigener** VSpace (heute jede extern geladene) gilt das **nicht**:
> ihr Speicher ist hardwaregetrennt wie bei jeder isolierten PD. Wer die Zusicherung liest, muss
> also nach dem **Adressraum** fragen, nicht nach dem Domänen-Etikett.

---

## 4. Der Unterschied zur VM — ehrlich

Das ist ein **Produktrisiko**, kein Implementierungsdetail. Caprock tritt an, um VMs zu ersetzen
([Z1](../TODO.md)); an dieser Stelle ist es schlechter als eine VM, und das muss dastehen.

|  | Wirt mit 100 VMs | Caprock-Knoten mit 100 PDs |
|---|---|---|
| Gast-/PD-Anwendung stürzt ab | 1 von 100 weg | 1 von 100 weg |
| **Gast-Kernel** paniert | **1 von 100 weg** | *gibt es nicht* — die Aufgabe liegt im Caprock-Kernel |
| **Wirts-/Caprock-Kernel** paniert | 100 von 100 weg | **100 von 100 weg** (bzw. der undefinierte Zustand aus §2) |
| Größe des Codes in der geteilten Domäne | Hypervisor + Wirtskern | ~18 kLOC `kernel/src`, ~36 kLOC inkl. `crates/` |
| Größe des Codes in der **privaten** Domäne | vollständiger Gastkern je Mandant (Größenordnung 10⁷ LOC) | **null** — es gibt keine private Kernschicht |

**Der Handel, in einem Satz:** Eine VM-Plattform hat *viele große* Fehlerdomänen, Caprock hat
*eine kleine*. Weniger Code kann ausfallen — aber wenn er ausfällt, fällt **alles** aus.

Ob das ein guter Handel ist, entscheidet nicht die Architektur, sondern die Fehlerrate pro Zeile
mal die Zeilenzahl. Solange die nicht gemessen ist, ist die einzig verantwortbare Position die
Festlegung aus §1: **Redundanz über Knoten.**

**Zwei betriebliche Konsequenzen, die daraus unmittelbar folgen:**

1. **Ein Knoten ist keine Redundanzeinheit.** Bei VMs darf ein Betreiber zwei Repliken eines
   Mandanten auf denselben Wirt legen und einen Gast-Absturz überleben. Auf Caprock nicht. Der
   Scheduler kennt heute keinen Anti-Affinitäts-Begriff — die Regel muss **über** dem Knoten
   durchgesetzt werden.
2. **Wartung ist knotengranular.** Ohne Live-Migration einzelner PDs (Z3/Z4 offen) heißt „Knoten
   leeren" heute: alle Mandanten darauf neu starten. Für kurzlebige Funktionsaufrufe
   (Vercel-artige Lastform) ist das verkraftbar; für alles Zustandsbehaftete nicht.

---

## 5. Wie das gemessen wurde

Wiederholbarkeit ist Teil der Aussage. Aufbau am 2026-08-02:

* **Bau außerhalb des Repos.** Ein Worktree *innerhalb* von `.claude/worktrees/` ist nicht baubar
  (AGENTS.md: Cargo findet die `.cargo/config.toml` des Hauptcheckouts zusätzlich, das Linker-Skript
  wird zweimal übergeben, und **x86 scheitert dabei lautlos**). Der Quellbaum wurde nach `/tmp`
  kopiert und dort gebaut.
* **x86_64:** `cargo build --release --target x86_64-unknown-none -p caprock-kernel --features selftest`,
  danach `objcopy -I elf64-x86-64 -O elf32-i386`. QEMU: `-machine q35,kernel-irqchip=split
  -device intel-iommu,caching-mode=on,intremap=on -device virtio-rng-pci,disable-legacy=on,iommu_platform=on
  -m 512 -smp 4 -enable-kvm -cpu host,+invtsc -no-reboot`, Zeitlimit 120 s, Konsole in eine **Datei**
  (nicht in eine Pipe — die verliert beim SIGKILL den Puffer).
* **aarch64:** `cargo build --release -p caprock-kernel --features selftest`. QEMU:
  `-machine virt,iommu=smmuv3 -cpu cortex-a72 -smp 8 -m 4G`, ohne Boot-Archiv (die
  archivabhängigen Prüfungen melden in diesem Lauf erwartungsgemäß `FAILURES` — der gemessene
  Gegenstand ist die Lebendigkeit der Kerne, nicht die Testsignatur).
* **Referenzlauf ohne Eingriff:** x86 `rc=0`, 143 Zeilen, `== SELFTEST COMPLETE -> system_off ==`,
  31 Ergebniszeilen. Jeder Messlauf wird gegen diese Signatur verglichen.
* **Der Eingriff** war je Fall ein `panic!()` an genau einer Stelle im **Kopierbaum**. Im Repo
  steht davon nichts.

### Was NICHT gemessen wurde

Eine Fehlerdomäne auf ungeprüften Annahmen ist schlimmer als keine. Offen bleiben:

* **Echtes Blech.** Alle Messungen liefen unter QEMU (x86 mit KVM, aarch64 unter TCG). Der
  Interrupt-Weg aus der Halt-Schleife ist Architekturverhalten und sollte tragen — geprüft ist er
  auf Blech nicht.
* **Der Kernel-Stack-Überlauf im Doppelfehler.** Gemessen sind 362 Rekursionsebenen ohne Ausnahme;
  was beim Überschreiten des Stackendes passiert, wurde nicht abgewartet (der Bootkern schaltete
  vorher ab). Dass es **keine** Schutzseite und **keinen** `#DF`-Handler mit IST gibt, ist am Code
  belegt (`crates/caprock-hal/src/x86_64/exception.rs:494` benennt Vektor 8 nur), die Folge nicht.
* **Panic unter `CAPS`** — gemessen wurde `MEM`. `CAPS` ist ein `RwSpinLock` mit derselben
  unbegrenzten Spin-Schleife; die Folge sollte dieselbe sein, gemessen ist sie nicht.
* **Panic in einem Interrupt-Handler.** Nicht gemessen. Der Rückweg über den Timer, auf dem die
  Fälle C/D beruhen, könnte dort anders aussehen.
* **Die aarch64-Fälle E, F, G.** Nur Fall C wurde auf aarch64 gegengeprüft. Für E ist die Ursache
  (unbeschränkter Ticket-Lock in `caprock-sync`) architekturneutral, für F ebenfalls — belegt ist
  das nicht.
* **Ob ein Panic auf einem Kern die Übersetzungstabellen der IOMMU in einem Zwischenzustand
  hinterlässt.** Ein Gerät liest diese Tabellen ohne den Kernel; ein Panic zwischen zwei
  Schreibvorgängen ist ein eigener, hier nicht untersuchter Fall.

---

## 6. Was billig verbesserbar wäre — und was nicht

Nichts davon ist gebaut. Die Aufwände sind Schätzungen, die Reihenfolge ist Nutzen pro Aufwand.

| # | Maßnahme | Aufwand | Wirkung |
|---|---|---|---|
| 1 | **Interrupts im Panic-Pfad maskieren.** In `kernel/src/panic.rs` statt `arch::x86_64::halt()` die maskierende HAL-Fassung rufen (`hal::cpu::halt()`); auf aarch64 `local_irq_disable()` **vor** die `wfe`-Schleife. | **Minuten** | Macht aus „Panic wird verdaut" (C/D) „dieser Kern ist tot" — ein *definierter* Zustand. Hebt die Fälle C/D auf das Niveau der Festlegung aus §1. **Kostet Verfügbarkeit und gewinnt Ehrlichkeit** — das ist die richtige Richtung, aber es ist eine Entscheidung, keine Reparatur. |
| 2 | **Rekursionswächter im Panic-Handler.** Ein `AtomicU32`; ab Tiefe 1 keine Formatierung mehr, nur eine feste Zeichenkette und halt. | **Minuten** | Beendet Fall G, bevor der Stack alle ist. Die `format_args!`-Maschinerie ist das, was rekursiert. |
| 3 | **Panic-Marke, die andere Kerne sehen.** Ein globales `PANICKED`; jeder Kern prüft es im Timer-Tick und hält an. | **1–2 h** | Macht aus einem Ein-Kern-Panic einen sauberen Knotenstopp — **ohne** NMI, denn der Timer läuft ohnehin. Greift nicht bei einem Kern, der mit maskierten IRQs in einer Sperre dreht (dafür #5). |
| 4 | **Panic → `system_off`.** Der panickende Kern schaltet die Maschine ab (ACPI S5 / PSCI `SYSTEM_OFF`); beide Wege existieren bereits (`x86_64/power.rs:295`, `aarch64/psci.rs:20`) und werden heute nur aus Testabschlusspfaden gerufen, **nie** aus `panic.rs`. | **~1 h** | Die einfachste Art, die Festlegung aus §1 zur **Tatsache** zu machen. Preis: die Ausgabe der anderen Kerne bricht mitten im Satz ab, und der Knoten ist sofort weg statt geordnet geleert. |
| 5 | **Schranke im Ticket-Lock.** `SpinLock::lock` dreht unbegrenzt (`crates/caprock-sync/src/lib.rs:170–178`). Eine Obergrenze, die bei Überschreitung meldet und anhält. | **klein im Code, groß in der Abnahme** | Verwandelt Fall E aus einem stillen Totalausfall in einen diagnostizierten. **Aber:** das ist die zentrale Synchronisationsprimitive; jede Änderung daran zieht Loom (B-7.2) und Kani (B-7.1) nach sich, und eine zu knappe Schranke erzeugt Fehlalarme unter Last. Nicht „billig" im Sinne von risikoarm. |
| 6 | **`#DF`-Handler mit eigenem IST-Stack (x86).** Vektor 8 ist heute nur **benannt**. | **klein–mittel, HAL** | Ohne IST endet ein `#DF` auf kaputtem Stack im Triple Fault (Neustart ins BIOS). Mit IST gibt es eine letzte Meldung. |
| 7 | **Schutzseiten an Kernel-Stacks.** Heute gibt es keine; ein Überlauf läuft still in Nachbarspeicher. | **mittel** | Macht Stacküberläufe (auch außerhalb von Panics) zu einem Fault statt zu stiller Verfälschung. Kostet eine Seite je Stack — bei 64 KiB je Thread und dem Zielbild „viele tausend Prozesse" ist das zu rechnen (`TODO.md` C4). |
| 8 | **Panic-IPI/NMI an alle Kerne.** Die IPI-Maschinerie existiert (`system.rs:432` `kick`, `send_sgi`), aber **kein NMI**. | **mittel** | Ein normaler IPI erreicht keinen Kern mit maskierten IRQs — genau den Fall, den man treffen will. Zuverlässig braucht es NMI (x86) bzw. FIQ/`sgi` an einer Gruppe-0-Quelle (ARM), und beides existiert im Projekt nicht. Deshalb ist #3+#4 der bessere erste Schritt. |

**Empfehlung:** #1 + #2 + #4 zusammen (Größenordnung ein halber Tag) machen die Festlegung aus §1
wahr, statt sie nur zu behaupten. #3 ist der geordnetere, aber teurere Weg zum selben Ziel. #5–#8
sind eigene Vorgänge.

---

## 7. Was Forschungsklasse ist

**Einen Kernel-Fehler auf die verursachende PD eingrenzen.** Das setzt voraus, dass der Zustand,
den der Fehler beschädigt haben könnte, **einer PD zugeordnet** ist. Heute sind `CAPS`, `MEM`,
`EPS`, `NTFNS` und `VSPACES` knotenglobal, und die Sperrhierarchie (`docs/invariants.md` §1) ist
genau deshalb eine *totale* Ordnung über diese Globals. Eine PD-lokale Fehlerdomäne verlangt:

1. Ressourcen je PD statt global (der Cap-Space ist es schon budgetiert, aber nicht **getrennt**),
2. eine Zusicherung, welche Invarianten ein Panic verletzt haben *kann* — sonst weiß niemand, was
   nach dem Eingrenzen noch gilt,
3. einen Wiederanlauf, der die betroffene PD abbaut, **ohne** die Strukturen anzufassen, deren
   Konsistenz gerade fraglich ist.

Punkt 2 ist der harte: er ist gleichbedeutend damit, den Kernel für jeden Panic-Punkt mit einer
Wiederherstellungs-Invariante zu versehen. Das ist eine Verifikationsaufgabe in der Größenordnung
des Kernels selbst, kein Umbau.

Bis dahin gilt §1.
