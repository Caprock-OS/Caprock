# Linux-Kompatibilität in Caprock

**Architektur, Aufwandsstruktur und Arbeitsteilung**

Version 1 · Stand: 26. August 2026

---

## 1. Ziel und Geltungsbereich

### Zielsatz

> Alle PCIe-Treiber der Kernelversion X laufen unter Caprock, ohne manuelle Codeänderung je Treiber. Zielarchitektur x86-64, später ARM-SystemReady.

Die Formulierung ist bewusst relativ und nicht absolut. „Alle Treiber" ist kein erreichbares Ziel — das schafft Linux selbst nicht. „Alle Treiber der Version X" hat dagegen einen abzählbaren Nenner und ist damit als Abdeckungsquote messbar, ab dem ersten gebauten Treiber.

### Bedingung

Erlaubt ist alles, was ohne Menschen und ohne LLM von selbst läuft. Ein neuer Treiber ist ein Eintrag in der Konfiguration, kein Gutachten.

Diese Bedingung ist die schärfste Anforderung des Projekts und entscheidet die Architektur (Abschnitt 2).

### Ausdrücklich nicht enthalten

| Ausschluss | Grund |
|---|---|
| Nicht selbstbeschreibende Busse (SoC) | Device Tree, Clocks, Regulatoren, Pinctrl sind Arbeit je Platine |
| Architekturspezifischer Treibercode | `depends on X86`, Inline-Assembly gegen fremde Arch |
| Treiber ohne verfügbare Firmware | `request_firmware` scheitert erst zur Laufzeit |
| Versionsübergreifende Gültigkeit | Die Portierung hängt an genau einer Kernelversion |

---

## 2. Grundentscheidung: die A/B-Kante

Die zentrale Frage des gesamten Entwurfs lautet nicht „Bibliothek oder Arch-Port", sondern:

> **Welche Verzeichnisse des Linux-Baums werden übersetzt, und welche werden ersetzt?**

Die Antwort ergibt sich aus der Bedingung in Abschnitt 1.

### Die zwei Symbolklassen

Jedes undefinierte Symbol eines Treibers fällt in genau eine Klasse:

**Klasse A — Standard-Betriebssystemsemantik**

Elemente, deren Bedeutung außerhalb von Linux existiert und dokumentiert ist.

```
kmalloc, kmem_cache_alloc, ksize, alloc_pages
mutex_lock, spin_lock, atomic_*
msleep, jiffies, timer_setup
queue_work, flush_workqueue
wait_event_*, complete, wait_for_completion
ioremap, readl, writel
dma_alloc_coherent, dma_map_single, dma_map_sg
request_irq
```

Für diese Elemente gibt es einen Referenzpunkt außerhalb des Linux-Quellcodes. Eine Zusicherung ist formulierbar und prüfbar.

**Klasse B — Linux-eigene Datenstruktursemantik**

Elemente, deren Bedeutung ausschließlich in der Linux-Implementierung steht.

```
__alloc_skb, napi_gro_receive, netif_rx
blk_mq_alloc_tag_set, blk_mq_init_queue
device_add, device_register, get_device
pci_enable_device, pci_request_regions
drm_gem_object_init
```

Für diese Elemente gibt es keine Spezifikation. Die Invarianten stehen im Quellcode und nirgends sonst.

### Die Regel

> **Klasse A wird beim Übersetzen durch Schablonen auf Caprock abgebildet.**
> **Klasse B wird aus dem echten Linux-Quellbaum übersetzt.**
> **Treiber bleiben unverändert.**

### Schablonen sind Übersetzungsregeln, keine Bibliothek

Das ist der entscheidende Unterschied zu einer Emulationsschicht und er ist keine Formulierungsfrage.

Eine Schablone ist zur Laufzeit **nicht vorhanden**. Sie ist keine Komponente der Linux-Treiber-API und kein Objekt im Binary. Sie ist eine Regel, die beim Übersetzen feuert und den Aufruf durch Caprock-Code ersetzt.

```
lx_emul:      Treiber → ruft dma_map_single() → Stub-Objekt im Binary
Schablonen:   Treiber → dma_map_single() wird beim Übersetzen ersetzt
                        → kein Symbol, kein Stub, kein Aufruf
```

**Die Eigenschaft, die daraus folgt:** es gibt keinen Default. Eine Emulationsbibliothek muss alles definieren, was sie exportiert — fehlt Substanz, ist der Stub trotzdem da und tut nichts. Eine Regelmenge darf unvollständig sein: ein Aufruf ohne Schablone hat kein Ziel und ist ein **Übersetzungsfehler**.

Damit ist der teuerste Fehlermodus dieser Projektklasse — der Stub, der still Erfolg meldet — nicht durch Disziplin ausgeschlossen, sondern strukturell unmöglich. Das ist stärker als das, was ein Arch-Port leistet: dort existiert die echte Implementierung, hier existiert bei Lücke *nichts*, und das Bauen bricht.

### Was trotzdem im Binary landet

Nicht jede A-Regel ist eine reine Ersetzung. Zwei Sorten:

**A-rein — vollständige Ersetzung, nichts bleibt zurück**

| Element | wird zu |
|---|---|
| `readl` / `writel` | Load/Store auf das gemappte BAR, plus Barriere |
| `dma_map_single`, `dma_map_sg` | `base_iova + (vaddr − arena_base)` — Arithmetik |
| `spin_lock` / `rcu_read_lock` (falls kooperativ) | entfällt |
| `jiffies` | Leseoperation auf die Zeitquelle |

**A-Zustand — die Regel erzeugt Code mit Laufzeitzustand**

| Element | braucht |
|---|---|
| `kmalloc`, `kmem_cache_alloc`, `ksize` | Slab-Allokator mit Freilisten über der Arena |
| `alloc_pages`, `page_frag_alloc`, `struct page` | `mem_map`-artige Struktur |
| `queue_work`, `flush_workqueue` | Threadpool, Warteschlange, Schleife |
| `wait_for_completion`, `wait_event_*` | Completion-Struktur mit Buchführung |
| `timer_setup`, `msleep` | Timerrad |

Bei A-Zustand ist die **Schablone** weiterhin nicht zur Laufzeit aktiv — der **emittierte Code** ist es. Das ist kein Widerspruch zur Regel, aber es heißt: die Anforderung „A muss exakt richtig sein" (Abschnitt 3) bleibt unverändert bestehen. Sie verschiebt sich nur von einer Bibliothek in die Ausgabe des Übersetzers.

**Praktische Folge:** A-rein und A-Zustand sind getrennt zu führen. A-rein ist Regelarbeit und maschinell prüfbar. A-Zustand ist Implementierungsarbeit mit allen Fallen aus Abschnitt 3 und trägt das eigentliche Risiko der Schicht.

### Warum die Kante genau hier liegt

Der Grund ist nicht die Anzahl der Elemente, sondern die Prüfbarkeit der Zusicherung.

Am Beispiel `struct sk_buff`:

```
head ──┬─────────────┬────────────────┬──────────┬──────────────────┐
       │  Headroom   │  gültige Daten │ Tailroom │  skb_shared_info │
       └─────────────┴────────────────┴──────────┴──────────────────┘
       ↑             ↑                ↑          ↑
      head          data             tail       end
```

Invarianten, die jeder Netzwerktreiber unausgesprochen voraussetzt:

- `head ≤ data ≤ tail ≤ end`
- `len == (tail − data) + data_len`
- `truesize` entspricht der tatsächlichen Allokation (Socket-Accounting hängt daran)
- `dataref` ist zweigeteilt — obere 16 Bit Header-Referenzen, untere Payload-Referenzen
- Alignment nach `SKB_DATA_ALIGN`, Headroom-Konvention `NET_SKB_PAD`
- `skb_shared_info` liegt bei `head + end`, also innerhalb derselben Allokation

`skb_put()` ist eine `static inline` im echten Header. Sie prüft nichts, sie setzt voraus. Hergestellt wird der Zustand von `__alloc_skb()` — Klasse B.

Ein Nachbau von `__alloc_skb` müsste gegen eine Referenz zusichern, die nur als Quellcode existiert. Der Beweis wäre dann exakt so gut wie das Verständnis beim Lesen von `skbuff.c` und könnte den Irrtum nicht fangen. Eine Zusicherung ohne Referenzpunkt ist ein aufwändiger Weg, eine Annahme aufzuschreiben.

### Warum B sich in A auflöst

Entscheidend: **Klasse B ist keine Blattmenge.** `net/core/skbuff.c` ist reine Datenstrukturarbeit. Was es selbst von unten braucht:

```
kmem_cache_alloc, kmalloc, ksize
alloc_pages, page_frag_alloc, put_page
atomic_*, spinlock
WARN_ON, BUG_ON
```

Das ist vollständig Klasse A.

Dasselbe gilt der Reihe nach für `blk-mq`, `drivers/base`, `net/core/dev.c`: jedes B-Symbol ist in einer C-Datei implementiert, deren eigene Abhängigkeiten weitgehend A sind.

**Die offene Größe ist damit nicht „wie viele B-Symbole", sondern wie tief die transitive Hülle reicht, bis nur noch A übrig bleibt.** Das ist direkt messbar (Abschnitt 8, M1).

### Der eine Sonderfall in A

`struct page` ist das einzige A-Element ohne Referenzpunkt außerhalb von Linux. Die skb-Fragmente halten `struct page *`, nicht Zeiger; dazu `page_address()`, `page_to_phys()`, Refcounting, `page_frag_alloc`.

Das braucht eine `mem_map`-artige Struktur über der Arena. Lösbar, aber es gehört als benannter Posten geführt und nicht unter „Speicherverwaltung" subsumiert.

### Was diese Architektur erfüllt

Ein neuer Treiber zieht seine B-Symbole aus mitübersetzten Quellen. Es gibt kein Symbolgutachten, keine leeren Stubs und damit keinen stillen Erfolg. Nicht unterstützte Treiber scheitern **beim Bauen**, nicht im Betrieb.

Genau das macht die Bedingung aus Abschnitt 1 erfüllbar.

---

## 3. Treiberstapel

**Beim Übersetzen:**

```
  Treiberquelle ─┐
                 ├─► Übersetzer + Schablonen ─► Treiber-PD (Binary)
  Subsysteme ────┘        │
                          └─ A-Aufruf ohne Regel = Übersetzungsfehler
```

**Zur Laufzeit** — die Schablonen kommen hier nicht mehr vor:

```
┌────────────────────────────────────────────────────┐
│  Treiber-PD                                        │
│    · Treibercode (unverändert übersetzt)           │
│    · Subsysteme: blk-mq, net/core, drivers/base    │
│    · emittierter A-Zustand: Allokator, Timerrad,   │
│      Workqueue, mem_map                            │
└────────────────────────┬───────────────────────────┘
                         │ Caprock-Syscalls
┌────────────────────────┴───────────────────────────┐
│  Caprock: Arena, SPAWN, Notification, IRQ,         │
│  DMA-API, ioremap, Zeit                            │
└────────────────────────────────────────────────────┘
                         │ virtio-Ringe über IPC
┌────────────────────────────────────────────────────┐
│  Klassenschicht: Block, Netz, USB, Display         │
└────────────────────────────────────────────────────┘
```

### Baumodell

**Treiber werden für Caprock kompiliert.** Es gibt keine virtuelle Maschine, keine Instruktionsemulation und keinen Linux-Kernel unter dem Stapel. Der Treiberquellcode wird unverändert übersetzt, gegen die A-Schicht gelinkt und als natives Caprock-PD ausgeführt.

Das ist die Voraussetzung dafür, dass Caprock die Autorität behält: DMA läuft über die Caprock-API, Prozessverwaltung und Fristen über Caprock, und die Capabilities gelten für das Treiber-PD wie für jedes andere.

**Ein Treiber-PD je Gerät. Eine Instanz des Stapels je Treiber-PD.**

Das ist der Grund für die gesamte Konstruktion und darf nicht aus Bequemlichkeit aufgegeben werden. Es hat zwei Konsequenzen, die bewusst getragen werden müssen:

- Speicherbedarf je Gerät statt je System
- Der Stapel ist Uniprozessor. Durchsatzgrenze je Gerät, nicht je System.

**Die Klassenschicht muss gegen das Subsystem geschrieben sein, nicht gegen einen Treiber.** Sonst findet der zweite Treiber derselben Klasse eine Lücke, und die Bedingung fällt beim zweiten Gerät.

### DMA-Entwurf

Die gesamte Arena einer Instanz wird **einmal** als zusammenhängendes IOVA-Fenster gemappt.

```
dma_map_single(vaddr) → base_iova + (vaddr − arena_base)
```

Keine IOMMU-Operation je Transfer, kein Bounce, keine Kopie. Scatter-Gather wird zur Schleife über dieselbe Arithmetik — virtuelle Buffer sind damit gelöst, ohne dass die Caprock-API etwas Zusätzliches können muss.

Der Preis ist Granularität: das Gerät kann überall in seiner eigenen Instanz schreiben. Da die Instanz bereits die Fehlerdomäne ist, verletzt das keine gezogene Grenze.

**Konservative Variante für den ersten Treiber:** fester Pool mit Bounce. Die Umstellung auf das Arena-Fenster ist dann ein geplanter Schritt, kein späterer Rettungsversuch.

### Stille Fallen im DMA-Pfad

| Falle | Symptom bei Fehler |
|---|---|
| `dma_set_mask_and_coherent(DMA_BIT_MASK(32))` | IOVA über 4 GiB → Hardware schneidet ab, schreibt an falsche Adresse. Kein Fault. |
| `dma_map_sg` Rückgabewert | Darf kleiner sein als die Eingabe (Segmentverschmelzung). Empfehlung: nicht verschmelzen, Eingabezahl zurückgeben. |
| `dma_set_seg_boundary`, `dma_set_max_seg_size` | Gerät kann nicht über 4-GiB- oder 64-KiB-Grenze lesen. Splitter muss das respektieren. |
| Coherent vs. Streaming | Verschiedene Lebensdauern. Ein strengerer Teardown-Token als Linux' loses `unmap` reibt genau hier. |

Auf x86 kostenlos: `dma_sync_*` (kohärent), die meisten `DMA_ATTR_*`.

### Stille Fallen im MMIO-Pfad

`readl`/`writel` müssen echte Loads und Stores auf ein gemapptes BAR sein, kein Funktionsaufruf je Register.

- **Uncached mappen.** Ein cached gemapptes BAR funktioniert im Test und fällt unter Last um. Framebuffer wollen Write-Combining.
- **Barrieren.** Linux' `readl`/`writel` tragen implizite Ordnungsgarantien. Als Funktionsaufrufe sind sie geschenkt, als Inline-Zugriffe müssen sie explizit werden. Fehlen sie, ist das Symptom sporadisch und lastabhängig.

### Stille Fallen in der A-Schicht

A ist nicht schwer zu bauen. A ist schwer exakt richtig zu bauen — und ganze Subsysteme ruhen unverändert darauf.

| Element | Anforderung |
|---|---|
| `GFP_ATOMIC` vs. `GFP_KERNEL` | Schläft der Allokator unter `GFP_ATOMIC`, deadlockt der Softirq-Pfad. Kein Fehler, ein Hänger. |
| `kmalloc`-Alignment | `ARCH_KMALLOC_MINALIGN`; Zweierpotenz-Größen natürlich ausgerichtet. `SKB_DATA_ALIGN` verlässt sich darauf. |
| `kmalloc`-Speicher | Muss DMA-fähig sein. Treiber mappen Ergebnisse direkt. |
| `ksize` | Muss die *tatsächliche* Größe liefern, nicht die angeforderte. `truesize` hängt daran; ein falscher Wert bricht Socket-Accounting erst unter Last. |
| `alloc_pages` | Echte Seitengranularität, weil Fragmente Seiten sind. |

---

## 4. Programme über den Syscall-Server

Getrennt vom Treiberstapel und nachgelagert.

```
   unveränderte Linux-Binaries
          │ Syscall-ABI
   ┌──────┴───────────────────┐
   │  Syscall-Server (Rust)   │  eine Instanz je Domäne
   └──────┬───────────────────┘
          │ Caprock-IPC
   ┌──────┴───────────────────┐
   │  Klassenschicht → Treiber-PDs   │
   └──────────────────────────┘
```

### Eine Instanz je Domäne

Nicht ein geteilter Server. Damit liegt die unverifizierte ABI-Schicht **innerhalb** der Fehlerdomäne, nicht zwischen den Domänen. Ein Fehler kompromittiert eine Domäne, nicht das System.

Das ist die Eigenschaft, die das Verifikationsargument trägt, und sie ist eine bewusste Entscheidung, keine Nebenwirkung.

### Die Syscall-Eintrittsfrage

Zwei Wege, und die Entscheidung fällt vor dem Bau:

| Weg | Kosten | Aufwand |
|---|---|---|
| Fault-Endpoint über IPC | 2 IPC-Wege je Syscall | keine Kerneländerung |
| Kernelprimitiv (vCPU / restricted mode) | ein kernelvermittelter Adressraumwechsel | Erweiterung von TCB und Beweisfläche |

**Präzedenz:** Fiasco.OC hat den vCPU-Mechanismus gebaut, Zircon `zx_restricted_enter`. Zwei unabhängige Systeme, verschiedene Jahrzehnte, dieselbe Schlussfolgerung — beide hatten den IPC-Weg und haben ein Primitiv nachgerüstet.

Die Messung (M4) bleibt sinnvoll als Zahl, aber die Nullhypothese lautet: das Primitiv wird gebraucht.

### Der Router als eigenes Artefakt

Sobald mehrere Dienst-PDs beteiligt sind, entsteht Zustand, den es vorher nicht gab:

- **fd-Namensraum.** `socket()` liefert fd 3 aus der Netz-PD, `open()` liefert fd 3 aus der FS-PD. Der Router besitzt `files_struct` und bildet sichtbare fds auf `(PD, interne fd)` ab.
- **Querliegende Syscalls:** `epoll`/`select`/`poll` (Readiness über PDs aggregieren, edge-triggered Semantik erhalten), `sendfile`/`splice`, `fork` (fd-Zustand in jeder PD duplizieren), `mmap` einer Datei, Unix-Sockets, `io_uring` (in dieser Aufteilung praktisch nicht baubar).

`epoll` ist der harte Fall und liegt vollständig im Router — also in Bespoke-Code innerhalb der Fehlerdomäne.

`mmap` ist sauber lösbar: die FS-PD wird External Pager, der Fault geht über Caprock an sie. Offen bleibt die Kohärenz des Page Cache bei zwei mappenden PDs.

### systemd

Der größte einzelne Verbraucher der Linux-ABI: cgroups v2, logind, udev-Events, dbus, umfangreiche `/sys`- und `/proc`-Erwartungen, Mount-Namespaces, seccomp, langer ioctl-Schwanz.

„Minimale Syscall-Abdeckung" und „systemd läuft" sind unvereinbar. **Empfehlung: eigener nativer Init/Service-Manager.** Damit fällt der größte ABI-Verbraucher weg, und die Linux-Prozesse darüber brauchen ihn nicht — sie brauchen nur, dass jemand sie startet.

---

## 5. Die Caprock-Lücke

Stand der benötigten Primitive.

**Drei Zustände, nicht zwei.** Die erste Fassung dieser Tabelle führte *fehlt* und *gebaut* in
einer Spalte, und dazwischen fiel der Zustand, der die meisten Fehler trägt:

| Zustand | heißt | was man dagegen plant |
|---|---|---|
| **fehlt** | kein Code | den **Bau** |
| **gebaut, ungemessen** | Code da, keine Prüfzeile, kein Suitentor | die **Messung** |
| **gemessen** | eine Zeile gattert in jedem Lauf | nichts |

Der mittlere ist der teure: `SYS_SPAWN` stand neun Tage vollständig in der ABI und wurde hier als
*fehlend* geführt — gegen „fehlt" plant man den Bau, gegen „ungemessen" die Messung, und beide
Male plant man am Falschen vorbei. `grep` nach den Aufrufern entscheidet es in einer Zeile.

| Element | Zustand |
|---|---|
| Speicher, Threads, Mutex/Semaphore | **gemessen** |
| `ioremap`, Geräte-Enumeration | **gemessen** |
| DMA-API | **gemessen**, weitgehend vollständig |
| Monotone Zeit (A1) | **gemessen** 2026-08-27 — `SYS_CLOCK = 28` liefert Rate und Stand, kein Cap. Zeile `uhr` auf beiden Architekturen, Abweichung **0 %** gegen die Tick-Uhr, drei Gegenproben |
| Fristen (A2) | **gemessen** 2026-08-28 für `WAIT` — `ERR_TIMEOUT = 25`, Frist in `MSG0` (`0` = wie bisher). Die Frist ist ein **Wecker**, kein sechster Blockadegrund: sie entfernt genau den Grund, für den sie scharfgestellt wurde (Z24). Das Rennen ist entschieden — **das Signal gewinnt**. Vier Konjunkte in der `uhr`-Zeile, beide Richtungen. Gegengeprüft mit `tools/fristen-negativ.sh` (4 von 4, isolierend) — die hier vorgeschriebene Mutation *„der Timer entfernt alle Gründe statt des einen"* braucht einen Thread mit **zwei** Gründen, sonst ist sie folgenlos; der Kernel pausiert den Wartenden dafür. **Offen:** `CALL`/`PARK` (todo A2), das Rennen gezielt treffen (todo A2c), der Verus-Aufwand — nach wie vor die einzige unbepreiste Größe (todo A2v) |
| IRQ-Zustellung, Mechanik | **gemessen** — `irq : ALL PASS` auf aarch64 (RTC über GIC-SPI): IRQ-Cap, `bind_irq`, lock-freier Hook, Deferred-Zustellung als Notification. Der Hook hängt arch-neutral, x86 ruft ihn mit dem rohen Vektor |
| IRQ, x86/MSI — **B1** IRTE-Vergabe + MSI-X-Zeile bei der Zuteilung | **gemessen** 2026-08-26 — Capability-Walk in `pcie.rs`, `msi_grant`/`msi_revoke` in `assign_driver_device`, SVT/SID im selben Schritt, Ruecknahme in der Ordnung *Geraet → IRTE → Vektor*. Zeile `irqmsi`, Gegenproben `tools/irqmsi-negativ.sh` |
| IRQ, x86/MSI — **B2** die `Irq`-Cap im Treiber-Slot | **gemessen** 2026-08-26 — Slot 7, in beiden Wegen (Erstladen und Hot-Reload). Konjunkte `cap-in-slot7` (sie hat einen **Halter**) und `cap-nennt-vektor` (es ist **seiner**) |
| IRQ, x86/MSI — **B3** `SYS_BIND_IRQ` mit Cap-Riegel | **gemessen** 2026-08-26 — Nummer 26, `ERR_IRQ_FULL = 23`. Der Aufrufer nennt **keinen Vektor**, sondern eine Cap; `intid` kommt aus dem Objekt. Ring-3-Negativfall in **zwei** Lagen (leerer Slot: generische Aufloesung; gehaltene Cap vom falschen Typ: der Zweig selbst) |
| IRQ, x86/MSI — **B4** der Treiber wartet statt zu pollen | **gebaut, nicht wirksam** 2026-08-26 (`B4_WARTEN = false`) — MSI-X in `caprock-virtio`, Warteweg, Zaehlwerk, eigene Interrupt-Notification (Slot 8). **Blockiert an einem gemessenen Befund**: das Geraet erledigt die Anfrage (`used.idx=1`) und sendet nicht. Jedes Glied davor und dahinter ist zurueckgelesen — s. Abschnitt „Stufe B / der offene Rand von B4" |
| TLS | **gemessen** 2026-08-27 — `SYS_SETTLS`, `Tcb.tls`, `sync_thread_state`, `PT_TLS` + `.tdata`/`.tbss`, `libcaprock::tls_einrichten`. Zeile `tls` auf **beiden** Architekturen, vier Gegenproben. Ein Treiber laeuft mit `#[thread_local]`. Der Kernel haelt **ein Register**, das Layout (Variante 1 gegen 2) lebt im Userspace |
| Präemptions-Gatter je Thread | **fehlt** — folgt aus E3 (entschieden 2026-08-26); die Entscheidung steht, die Umsetzung nicht |
| SPAWN mit Offset | **gemessen** 2026-08-26 — `x1 = (offset_pages << 32) \| len_pages`, Zeile `arena`, beide Architekturen |
| Cap-Slots je PD | **16, hart** (`NCAPS`) — das Budget ist seit 2026-08-26 ein Konto je PD, aber der lokale Cspace ist ein Array fester Länge. Für dreissig Caps braucht es einen variablen Cspace, s. TODO.md Part II K1c |
| Mehrere Caps beim Laden | **gemessen** 2026-08-25 — `SYS_LOAD` nimmt 8 `(Quell,Ziel)`-Paare in einem Wort |
| DMA-Pool, Größe zur Ladezeit | **gemessen** 2026-08-26 — `SYS_LOAD` `x5`, Zeile `dmapool` |

### Stufe A — Uhr und Fristen

**A kommt vor B.** Ohne Fristen ist ein ausbleibender Interrupt ein Hänger und von einem langsamen Gerät nicht zu unterscheiden. Dann ist für CAP_IRQ keine Gegenprobe schreibbar.

**A1 · Die Rate.** `rdtsc` ist aus Ring 3 lesbar (CR4.TSD nie gesetzt); es fehlt nur die Bedeutung. Ein Syscall liefert die gegen den PIT geeichte `TSC_HZ`.

*Gegenprobe:* eine um 10 % verfälschte Rate melden — der Vergleich muss fallen.

**A2 · Die Frist — GEBAUT UND GEMESSEN 2026-08-28, für `WAIT`.** `WAIT`, `CALL`, `PARK` bekommen eine Deadline; läuft sie ab, wird der Thread mit `ERR_TIMEOUT` geweckt. Gebaut ist bisher **`WAIT`**; `CALL` und `PARK` tragen keine Frist. Positivkontrolle und *Gegenprobe* laufen (2026-08-28); offen bleibt das *Rennen*, das entschieden, aber nicht gezielt getroffen ist. Siehe `done.md`.

Der Entwurf hängt scharf an Z24: die Frist ist kein eigener Blockadegrund, sondern ein Wecker. Sie darf genau den Grund entfernen, für den sie scharfgestellt wurde — sonst ist es der D9-Fehler wörtlich, mit der Uhr als Auslöser.

*Messung, beide Richtungen:* Warten mit Frist auf ein Signal, das nie kommt → Wecken innerhalb `[d, d+1 Tick]` mit `ERR_TIMEOUT`. Positivkontrolle: derselbe Aufbau mit Signal → früheres Wecken mit `OK`.

*Gegenprobe:* der Timer entfernt alle Gründe statt des einen — ein danebenliegender, aus anderem Grund geparkter Thread muss losfallen, und eine zweite Sonde muss das sehen.

*Offene Kante:* das Rennen zwischen ankommendem Signal und feuerndem Timer. Beide Auflösungen sind vertretbar, aber sie muss benannt und gezielt getroffen werden. Ein Treiber, der `ERR_TIMEOUT` als „Gerät tot" liest und resettet, während die Completion zugestellt wurde, ist ein Korruptionspfad und kein Hänger. Der Test muss das Fenster absichtlich treffen.

*Nicht dabei:* hrtimer, Wanduhrzeit, absolute Zeit.

**A2 ist der aufwändigste Posten der Stufe, und der Aufwand liegt nicht im Code.** Fristen fassen die Blockade-Invarianten an. In einem verifizierten Kernel wird aus „der Thread ist genau dann blockiert, wenn Grund X gilt" ein „… oder die Frist ist nicht abgelaufen", und alles, was auf der ersten Fassung ruhte, muss nach. Der Verus-Aufwand ist die einzige unbepreiste Größe im Plan.

### Stufe B — CAP_IRQ

> **NEU ZUGESCHNITTEN 2026-08-26** — der Schnitt B1/B2/B3 unten ist überholt; die Fassung, die
> gilt, steht in **`docs/plan-cap-irq.md`**. Der Grund: die **Zustellmechanik war vorhanden** und
> wird auf aarch64 in jedem Lauf als `irq : ALL PASS` gemessen (RTC über GIC-SPI) — IRQ-Cap,
> `bind_irq`, lock-freier Hook, Deferred-Drain; der Hook hängt arch-neutral, x86 ruft ihn mit dem
> rohen Vektor. Auch die IRTE-Vergabe steht samt SVT/SID **und Hardwarezugriff**.
>
> Übrig bleiben vier kleinere Stücke: **B1** IRTE-Vergabe + MSI-X-Zeile bei der Zuteilung (mit dem
> fehlenden Capability-Walk — `pcie.rs` kennt kein MSI), **B2** die Cap in den Treiber-Slot,
> **B3** `SYS_BIND_IRQ` mit Cap-Riegel (`bind_irq` prüft heute **keine** Cap, obwohl sein
> Doku-Kommentar es behauptet), **B4** der Treiber wartet statt zu pollen.
>
> Dazu E10–E12 in Abschnitt 9 und **eine benannte Schuld**: *vor Stufe A hängt ein Treiber-PD,
> dessen Interrupt ausbleibt, unwiderruflich* — und ein Poll-Fallback als Zwischenlösung ist
> **verboten**, weil er ausgerechnet `poll-runden == 0` unterliefe.

Das Primitiv ist die vorhandene Notification, nicht ein neuer Zustellweg. Der Treiber wartet in `WAIT` statt einen Callback zu bekommen.

**B1 · IRTE-Vergabe.** Gehört an dieselbe Stelle wie die DMA-Anhängung — in `assign_driver_device`, fail-closed. **SVT/SID gehören in denselben Schritt:** ein IRTE ohne Quellprüfung nimmt eine MSI von jedem Gerät an, und dann ist die Interruptzustellung genau der Kanal, den A-5.4 auf der DMA-Achse geschlossen hat.

**B2 · Die Cap.** `Irq { intid }` wird bei der Gerätezuteilung geprägt, Slot 7. Damit ist ein Treiber-PD bei 7 von 8.

**B3 · Bindung.** `bind_irq(irq_cap, ntfn_cap, badge)`.

*Messung an der Wirkung:* virtio-blk gibt eine Anfrage ab und geht in `WAIT` statt zu pollen. Gezählt wird, dass der Poll-Zähler stehenbleibt — sonst ist „es kam an" von „er hat es selbst gemerkt" nicht zu trennen.

*Gegenprobe:* IRTE maskieren → der `WAIT` läuft in die Frist aus Stufe A.

*Nicht dabei:* geteilte Vektoren, IRQ-Affinität, aarch64 (GICv3/ITS ist ein anderer Mechanismus).

**Offener Posten:** *mehr als ein Vektor je Gerät*. Ein Vektor heißt Single-Queue. Für virtio-blk als Zieltreiber korrekt; jede echte NIC und NVMe unter Last will MSI-X mit einem Vektor je Queue. Das ist der zweite Treiber, nicht eine ferne Ausbaustufe.

### Der offene Rand von B4 — und was daran gemessen ist

Der Treiber bindet (ueber die ABI, aus einer echten Treiber-PD) und **wartet noch nicht**. Der
Grund ist kein Verdacht, sondern eine Kette zurueckgelesener Groessen:

| Glied | gemessen |
|---|---|
| Queue 0 → MSI-X-Zeile | `queue0-msix-vektor=0x0000` — die Queue haelt ihren Vektor |
| Zeile 0 im Geraet | `addr=0xfee00008` (remappable, Handle 0), `vctrl=0x0` — **unmaskiert** |
| Message Control | `0x8004` — Enable gesetzt, **Function Mask frei** |
| IRTE Handle 0 | praesent, Vektor `0x60`, `SVT=01`, SID = RID `0x0018` |
| IEC-Invalidierung | erfolgt (`vergib` bricht sonst ab, und es kam `Ok`) |
| Arbeit des Geraets | **`used.idx=1`** — die Anfrage ist erledigt |
| IOMMU-Faults | **leer** — eine abgewiesene Nachricht haette gefaultet |
| Zustellpfad dahinter | **bewiesen** per Self-IPI: Vektor `0x64` erreicht `irq_hook`, wird gedrained und kommt an der Notification an |

**Das Geraet erledigt die Anfrage und sendet nicht.** Alles, was der Gast sehen kann, ist geprueft;
die verbleibende Erklaerung liegt ausserhalb seiner Reichweite (QEMU/KVM-Routing einer
Remappable-Nachricht unter `intremap=on` mit `kernel-irqchip=split`).

**Damit ist B4 ein eigener Strang, und er gehoert an den MESSSTAND, nicht an den Kernel:** ein Lauf
ohne KVM (TCG), einer ohne `intremap`, einer mit `eim=on`. Drei Laeufe, die die Umgebung als
Variable behandeln — dieselbe Klasse wie *was auf q35 nicht vorkommt, ist nicht abwesend, sondern
ungeprueft*, nur andersherum.

**Zwei Instrumente sind dabei durchgefallen, bevor eines trug**, und beide Fehlschlaege stehen hier,
weil die Bedingung dahinter weitergilt:

* Ein Store nach `0xFEE0_0000` **in remappable Form** misst unter QEMU nichts: Interrupt-Remapping
  haengt dort als Speicherregion im **Geraete**-Adressraum, ein CPU-Store geht daran vorbei.
* Ein Store dorthin ueberhaupt ist kein Nachrichtenversand, sondern ein **Registerzugriff** auf die
  eigene LAPIC-Seite (und unter x2APIC ist die Seite abgeschaltet). Ein Prozessor schickt sich
  einen Vektor per **ICR**, nicht per Store.
* Und `IRQ_DELIVERED` zaehlt den **Drain**, nicht die **Ankunft**: drei Lagen — nie angekommen /
  angekommen und Vektor unbekannt / angekommen und nie gedrained — waren darin ununterscheidbar.
  Seither zaehlt `irq_hook` selbst mit (`irq_hook_stats`).

### Stufe C — Arena und DMA-Pool zur Laufzeit

Die Größen dürfen nicht aus dem Manifest kommen — das beschreibt nur den Bootzustand.

**C1 · Mehrere Caps beim Laden — GEBAUT 2026-08-25.** Gebaut ist die **erste** Form (Cap-Liste in `SYS_LOAD`: acht `(Quell,Ziel)`-Paare in einem Wort, je 4 Bit, weil `NCAPS = 16`). Die hier stehende Empfehlung für die zweite ist damit überholt, und der Grund ist das **Badge**: `SYS_SIGNAL` verodert das Badge der benutzten Cap, also müsste ein Badge-Argument für acht Caps acht Bedeutungen tragen. Wer je Kind ein eigenes Etikett will, badgt vorher selbst mit `CCOPY` und delegiert den Slot — eine Kopie, ein Etikett, keine Mehrdeutigkeit.

**C2 · DMA-Pool — GEBAUT 2026-08-26.** Dma-Caps bleiben nur kernelseitig prägbar; die **Größe** ist jetzt Argument der Gerätezuteilung (`SYS_LOAD` `x5`, `(dma_pages << 16) | cap_budget`, beide `0` = Vorgabe). `init` trägt die Politik als Tabelle Archivindex → Seiten — er *ist* der Boot-Taskmanager; ein Laufzeit-Treibermanager läse dieselbe Tabelle aus einer Konfiguration.

**D11 eingelöst:** mehr als `DRIVER_DMA_MAX_PAGES` (1024 Seiten = 4 MiB) → `ERR_DMA_TOO_LARGE`, keine gekürzte Region, Aufrufer nicht blockiert. Die Absage fällt am **Rand des Dispatch**, bevor Endowment-Caps abgeleitet sind — sonst erbte der Abweispfad eine Aufräumpflicht. Die Obergrenze steht in der **ABI**, nicht nur im Kernel: eine Schnittstelle, die oberhalb von `N` abweist, muss `N` veröffentlichen, sonst ist die naheliegende Reaktion des Aufrufers (halbieren und nochmal) genau die Kürzung, die vermieden werden soll.

*Gemessen* als `dmapool`: zwei Treiber-PDs mit 8 bzw. 32 Seiten, `eingeloest` (jede bekam genau das Angeforderte — der Wunsch wird **neben** der Gewährung geführt, sonst wären „so gewollt" und „gekürzt" nicht unterscheidbar), `verschieden`, IOVA-Fenster `disjunkt` (A-5.4 bei geänderten Größen **neu** gefahren), und der Ring-3-Negativfall aus `init`.

> **Eine Abweichung von der Vorregistrierung, und sie ist ein Befund:** `dma_audit == 0` steht hier als Bedingung. Am Ende eines Laufs mit A-5.1-Hot-Reload meldet es **2** — `reassign_driver_device` prägt der neuen Treiberfassung absichtlich eine Cap über **dieselbe** Region, und die Regel „verschiedene Objekte dürfen sich nie überlappen" kennt die *Übergabe* nicht. Es ist deshalb **kein** Konjunkt der Zeile, sondern eine Zahl darin. Die ernstere Frage dahinter — was beim Abbau der alten PD mit der Region passiert, auf der die neue läuft — steht als `A3d` in `TODO.md`.

### TLS

**Nicht das kleinste Element der Liste, sondern eines der ersten.** TLS ist, worauf `current` abgebildet wird, und `current` steckt in praktisch jedem Kernelpfad. Vor TLS läuft nichts sinnvoll. Gehört neben die Uhr, nicht ans Ende.

### SPAWN mit Offset — GEBAUT 2026-08-26

`x1 = (offset_pages << 32) | len_pages`; `0` ist die ganze Region und damit bitgleich zu jedem vorher geschriebenen Aufruf. Gemessen als `arena` (fünf EL0-Kinder aus zwei Caps, vier davon in Fenstern **einer** Arena, `threads=6 slots=2`), auf beiden Architekturen, mit fünf Gegenproben.

**Nebenbei war `SYS_SPAWN` bis dahin nie gelaufen** — gebaut am 2026-08-17, im ganzen Baum kein Aufrufer und kein Gatter.

**Der Preis ist zur Hälfte bezahlt.** Abgewiesen wird die **Überlappung bei der Vergabe** — und die Prüfung dafür musste erst gebaut werden: `pd_mapping_overlaps` liest `KSTACKS.ubase_of`, und der Spawn-Pfad trägt dort mit Begründung nichts ein, die `Overlaps`-Absage war für genau diese Threads also **strukturell unerreichbar**. Ungeschützt bleibt der **überlaufende Stapel zur Laufzeit**: dafür braucht es Wachseiten, und die gibt es auf aarch64 hardwareseitig nicht (`guard_unterstuetzt() == false`) — eine Seite, die dort nichts bewirkt, hat schon einmal eine Farbzusage strukturell unerfüllbar gemacht (C9e). Also eine eigene Messung, kein Nebeneffekt.

### Folgefrage: Nebenläufigkeit der kthreads — ENTSCHIEDEN 2026-08-26

Der SPAWN-Fix ist geliefert, also ist die Frage fällig. **Gemessen**, nicht angenommen:

| | Caprock heute |
|---|---|
| Präemption | **ja** — Timer-Tick, die vier Arena-Kinder werden umgeplant |
| Parallelität innerhalb einer PD | **nein** — `SYS_SPAWN` legt jeden neuen Thread auf den Kern des Aufrufers |

Das ist weder „ja" noch „nein" aus der obigen Alternative, sondern die Kombination
**`CONFIG_SMP=n` + `CONFIG_PREEMPT=y`** — und die ist in Linux unterstützt und im Baum getestet.
Damit muss die Antwort nicht erfunden werden; sie steht in `include/linux/spinlock_up.h`:

```
spin_lock()     → preempt_disable()      /* kein Spinnen */
spin_unlock()   → preempt_enable()
rcu_read_lock() → preempt_disable()      /* TINY_RCU */
```

Der *naive* Spinlock deadlockt in dieser Konstellation zwangsläufig: der Halter wird verdrängt,
der Warter belegt den einzigen Kern, der Halter kommt nie wieder dran. Die *richtige*
Implementierung spinnt aber gar nicht — ohne Parallelität genügt es, den Wechsel zu verhindern.

**Folge für den Zuschnitt: `spin_lock` und `rcu_read_lock` bleiben in Klasse A.** Die einzige neue
Caprock-Anforderung, die aus E3 folgt, ist ein **Präemptions-Gatter je Thread** — klein gegen die
Alternative (echte Sperren in der A-Schicht).

**Der Zuschnitt des Gatters ist die eigentliche Entscheidung:**

* Es darf **kein globaler Scheduling-Override** sein — das wäre ein System-DoS aus einem
  Treiber-PD heraus, also genau die Autorität, die eine PD nicht haben darf.
* Es muss heissen: *nicht auf einen anderen Thread **derselben PD** umschalten.* Verdrängung durch
  eine fremde PD ist unschädlich, weil es keinen geteilten Zustand gibt.
* Dass alle Threads einer PD auf dem Kern des Aufrufers landen, macht diese Fassung
  **ausreichend** statt nur bequem — sie deckt genau die Menge ab, die sich Zustand teilt.
* **Zweite Absicherung:** das Gatter durch das verbleibende SC-Budget deckeln. Dann kann es nicht
  unbegrenzt gehalten werden, ohne dass ein neues Fehlerbild entsteht (D11-Form: die Kapazität
  hat einen Namen).

Offen bleibt damit nur die Umsetzung, nicht die Frage. Sobald `SYS_SPAWN` eine Kernwahl bekommt
(heute gibt es keine), ändert sich die Voraussetzung und die Entscheidung ist neu zu stellen —
dann gilt `CONFIG_SMP=y`, und `spin_lock` verlässt Klasse A.

---

## 6. Arbeitsteilung auf zwei Spuren

Die Spuren sind so geschnitten, dass sie ab Tag 1 parallel laufen. Der Schnitt liegt an der A-Kante.

```
        Spur K (Kern)                    Spur L (Linux)
   ┌──────────────────────┐        ┌──────────────────────┐
   │ Caprock-Primitive    │        │ Baukette             │
   │ A-Schablonen         │        │ Subsystemauswahl     │
   │ IRTE/IOMMU           │        │ Klassenschicht       │
   │ Syscall-Eintritt     │        │ Router / fd-Raum     │
   └──────────┬───────────┘        └──────────┬───────────┘
              │                               │
              └────────► A-Header ◄───────────┘
                    (Vertrag, Tag 0)
```

### Der Mechanismus, der echte Parallelität erlaubt

**Der A-Header ist der Vertrag.** Er wird an Tag 0 gemeinsam festgelegt: Signaturen, Zusicherungen, Fehlercodes. Danach:

- **Spur K** implementiert ihn gegen Caprock.
- **Spur L** baut gegen eine **POSIX-Referenzimplementierung** desselben Headers und testet auf Linux.

Das ist die Standardtechnik (LKLs `tools/lkl/lib` ist der Präzedenzfall) und sie entkoppelt die Spuren vollständig. Spur L kann Subsysteme übersetzen, die Klassenschicht schreiben und den ersten Treiber unter Linux zum Laufen bringen, bevor ein einziges Caprock-Primitiv fertig ist.

Beim ersten Integrationspunkt wird die POSIX-Implementierung gegen die Caprock-Implementierung getauscht. Was dann bricht, ist ein Vertragsfehler und damit lokalisiert.

### Spur K — Kern und A-Schicht

**Lieferungen, in Reihenfolge:**

1. **TLS** — vor allem anderen, weil `current` daran hängt
2. **A1** Uhr, mit Gegenprobe
3. **SPAWN mit Offset** — inklusive Entscheidung zur kthread-Präemption und benanntem Guard-Page-Verzicht
4. **A2 Fristen** — der Verus-schwere Posten; das Signal/Timer-Rennen explizit
5. **B1–B3** IRQ, IRTE mit SVT/SID, fail-closed
6. **C1** Bootstrap-Cap + GRANT/SETRECV (keine ABI-Erweiterung)
7. **C2** DMA-Pool zur Ladezeit, D11
8. **A-Schablonen** gegen Caprock: Allokator über gewährte Arena, Sperren, Zeit, Workqueues, `ioremap`, DMA
9. **`struct page` / mem_map** über der Arena
10. **Syscall-Eintritt:** Messung M4, dann Entscheidung Fault-Endpoint vs. Kernelprimitiv

**Kompetenzprofil:** Caprock-Interna, Verus, IOMMU/IRTE. Das ist die Spur, die den verifizierten Kernel anfasst.

### Spur L — Linux-Baum, Baukette und Klassenschicht

**Lieferungen, in Reihenfolge:**

1. **M1 transitive Hülle** — beginnt an Tag 1, keine Voraussetzung
2. **M2 Massenbau** — Abdeckungsquote, keine Voraussetzung
3. **POSIX-Referenzimplementierung des A-Headers** — das Testgerüst für alles Weitere
4. **Kernelversion wählen** (Kriterien in Abschnitt 9)
5. **Baukette:** Konfiguration aus Geräteliste erzeugen, kbuild freistehend, linken, signieren. `wasmhost` ist der Präzedenzfall für fremden Fremdcode als PD.
6. **Subsystemauswahl und Übersetzung:** `drivers/base`, PCI, `blk-mq`, `net/core`
7. **Klassenschicht Block und Netz** — gegen das Subsystem geschrieben, nicht gegen einen Treiber
8. **Compiler-Plugin:** jeder Aufruf einer Kernelfunktion außerhalb der Liste ist ein Übersetzungsfehler
9. **Syscall-Server:** ABI-Schicht, fd-Namensraum, Router
10. **Nativer Init/Service-Manager**

**Kompetenzprofil:** Kbuild, Linux-Subsysteminterna, Rust für Klassenschicht und Server.

### Was bewusst *nicht* geteilt wird

**Die A-Schablonen liegen bei Spur K, nicht bei Spur L.** Naheliegend wäre das Gegenteil — sie sehen nach Linux-Arbeit aus. Aber ihr Inhalt ist Caprock-Semantik, und die Zusicherungen gehören zu dem, der die Kernelseite beweist. Spur L konsumiert sie über den Header.

**Der Router und die Klassenschicht liegen beide bei Spur L**, obwohl das viel ist. Sie teilen sich das fd- und Lebenszeitmodell; sie zu trennen erzeugt eine Naht ohne Nutzen.

### Die Naht, die keiner Spur zufällt

Zwischen Klassenschicht und ABI-Schicht: die Treiberkomponente liefert einen Blockdienst über virtio, aber die Linux-Binary erwartet `/dev/nvme0n1` mit passenden ioctls, sysfs-Attributen und udev-Events.

Diese Abbildung ist beschränkt, aber nicht null, und sie fällt bei keinem der beiden Teilprojekte an. **Ausdrücklich Spur L zuweisen, sonst landet sie nirgends.**

---

## 7. Synchronisationspunkte

| # | Punkt | Bedingung |
|---|---|---|
| S0 | **A-Header steht** | Tag 0. Signaturen, Zusicherungen, Fehlercodes. Danach arbeiten beide Spuren unabhängig. |
| S1 | **Erste Integration** | Spur K hat TLS, A1, A2, SPAWN. Spur L tauscht POSIX gegen Caprock. Ziel: virtio-blk baut und probet. |
| S2 | **L1 — erster Treiber unter Last** | B und C fertig. Der erste bepreisende Meilenstein. |
| S3 | **Zweiter Treiber derselben Klasse** | Kriterium: null Änderungen an Klassenschicht und Bauzeile. |
| S4 | **Syscall-Eintritt entschieden** | M4 gemessen, Fault-Endpoint oder Kernelprimitiv. |

Zwischen S0 und S1 gibt es keine erzwungene Kopplung. Das ist die eigentliche Leistung des Schnitts.

---

## 8. Vorregistrierte Messungen

### M1 · Transitive Hülle

**Frage:** Wie tief reicht B, bis nur noch A übrig ist?

**Verfahren:** `net/core/skbuff.o` bauen, `nm -u`, jedes B-Symbol zu seiner Quelldatei auflösen, diese bauen, iterieren bis Fixpunkt.

**Ergebnis:** die reale A-Menge für ein Subsystem.

**Aufwand:** ein Nachmittag. Keine Voraussetzung. **Beginnt an Tag 1.**

### M2 · Massenbau

**Frage:** Wie hoch ist die Bau-Abdeckung?

**Verfahren:** alle PCI-Treiber der Zielversion gegen den Ziel-Arch bauen, Erfolgsquote zählen.

**Aufwand:** eine Nacht Rechenzeit. Keine Voraussetzung.

**Vorregistriert:** > 80 % → Bauseite gilt als weitgehend gelöst, nur Laufzeit offen. < 50 % → die Ausschlussmenge ist das eigentliche Projekt, und der Zielsatz muss geändert werden.

### M3 · Amortisierung

**Frage:** Ist die Wiederverwendung real oder Fassade?

**Verfahren:**

| | Treiber | Misst |
|---|---|---|
| 1 | virtio-blk | Substrat ohne echtes DMA |
| 2 | echte NIC | Grenzkosten über Subsysteme hinweg |
| 3 | **zweite NIC, gleiches Subsystem** | **ob Amortisierung real ist** |

**Vorregistriert:** Treiber 3 verlangt null Änderungen an Klassenschicht und Bauzeile → trägt. Sonst ist die Liste der dazugekommenen Subsysteme die Hochrechnung für alle weiteren.

**Wichtig:** nicht Symbole zählen, sondern *welche zusätzlichen Subsysteme* der zweite Treiber zieht (z. B. `phylib`+MDIO, weil MAC und PHY getrennt sind; ethtool-Ops, PTP, GRO/XDP).

### M4 · Syscall-Eintritt

**Frage:** Trägt der Fault-Endpoint, oder braucht es ein Kernelprimitiv?

**Verfahren:** Fault-Handler-Round-Trip auf Caprock messen. Syscalls/s aus `sys_enter`-Histogramm einer realen Workload. Produkt bilden.

**Vorregistriert:** Overhead > 20 % CPU allein durch Eintritt → Primitiv unvermeidlich.

### M5 · Pfad-Diversität statt Gerätezahl

**Frage:** Wann ist die Induktion auf ungetestete Geräte belastbar?

Das Risiko ist **konzentriert, nicht verteilt** — alle Treiber teilen sich MMIO, DMA und IRQ-Zustellung. Sind die für ein Gerät korrekt, sind sie es für die Klasse. Sind sie subtil falsch, fällt das bei Gerät 2 auf, nicht bei Gerät 400.

Deshalb zählt Pfad-Diversität, nicht Anzahl:

| Gerät | Testet |
|---|---|
| virtio-blk | Substrat ohne echtes DMA |
| NVMe | Bus-Master-DMA, MSI-X, Doorbells, Queue Depth |
| Intel-NIC | Streaming-DMA, Ringe, hohe IRQ-Rate |
| xHCI | anderes DMA-Muster, komplexe Enumeration |

### M6 · Stufe-3-Tests je Treiber

Zwischen „probet" und „funktioniert" liegt der teuerste Fehler. Vier Tests, in dieser Reihenfolge:

1. **Etwas Gerätespezifisches zurücklesen** — bei NVMe `Identify Controller`, Model-String ausgeben. Beweist Admin-Queue, Doorbell, DMA und Completion-Pfad gleichzeitig.
2. **Feuert wirklich ein Interrupt?** Zähler im Handler; nach 10 000 I/Os prüfen, dass nicht gepollt wurde.
3. **Mit IOMMU an.** Eine falsche IOVA ist mit VT-d laut (DMAR-Fault), ohne VT-d still und korrumpierend.
4. **Last** — 10 000 I/Os mit verifiziertem Inhalt, nicht ein `read()`.

---

## 9. Offene Entscheidungen

| # | Entscheidung | Kriterium |
|---|---|---|
| E1 | **Kernelversion** | LTS bevorzugt (eingefrorener Zielbaum, längere Relevanz der Abdeckungszahl). Kollidiert das mit dem gewählten Arch-Vorbild, gewinnt das Vorbild — der Rebase ist teurer als die kürzere Relevanz. |
| E2 | **Eigener Arch-Port oder LKL als Basis** | LKL ist out-of-tree, versionsgebunden und muss für echtes MMIO/DMA ohnehin geforkt werden. Wer forken muss, kann selbst schreiben und die Trap-and-Emulate-Altlast weglassen. Entweder so — aber dann heißt der Posten „Arch-Port", nicht „Kompatibilitätsschicht". |
| E3 | **kthread-Präemption** | **entschieden 2026-08-26**: präemptiv, aber ohne Parallelität innerhalb einer PD — das ist `CONFIG_SMP=n` + `CONFIG_PREEMPT=y`, eine unterstützte Linux-Konfiguration. `spin_lock`/`rcu_read_lock` bleiben in Klasse A und werden nach `include/linux/spinlock_up.h` auf `preempt_disable` abgebildet. Einzige neue Caprock-Anforderung: ein **PD-lokales Präemptions-Gatter je Thread**, kein globaler Override. Siehe Abschnitt 5. |
| E4 | **Syscall-Eintritt** | M4. Nullhypothese: Primitiv wird gebraucht. |
| E5 | **DMA: Arena-Fenster oder Pool + Bounce** | Pool für Treiber 1, Arena-Fenster als geplanter Schritt. Nicht als späterer Rettungsversuch. |
| E6 | **Init: systemd oder nativ** | Empfehlung nativ. Siehe Abschnitt 4. |
| E7 | **Emittieren die Schablonen die Syscall-Sequenzen vollständig selbst?** | Ja → kein Caprock-Code im Treiber-PD, Lizenzfrage entfällt, Modell ist konsequent. Nein → der mitgelinkte Teil muss GPL-2.0-kompatibel sein und ist faktisch die Bibliothek durch die Hintertür. Siehe Abschnitt 11. Gehört zu S0. |
| E8 | **Schnitt zwischen A-rein und A-Zustand** | Bestimmt, wie viel Laufzeitcode entsteht und wo das Risiko der Schicht liegt. Siehe Abschnitt 2. |
| E10 | **Re-Trigger-Schutz zwischen Zustellung und Drain** | **entschieden 2026-08-26**: eine benannte **Zusicherung**, keine gemeinsame Implementierung — *nach der Zustellung und vor dem Drain darf derselbe Vektor nicht erneut auslösen.* aarch64 stellt sie mit `mask_intid` her, x86/MSI durch die Zustellart (edge). `mask_intid` auf x86 ist damit „erfüllt durch die Zustellart" statt leer — dokumentiert dort, wo `irq_hook` sich darauf verlässt. Die Prüfung ist auf **beiden** Architekturen dieselbe: zweimal auslösen vor dem Drain, höchstens eine Zustellung. Kehrt beim MSI-X-Ausbau **je Vektor** wieder. |
| E11 | **Wer konfiguriert MSI-X?** | **entschieden 2026-08-26**, und die Baumfrage ist gemessen: der Treiber bekommt heute **genau eine** BAR — die mit der virtio-Common-Config (`bringup.rs:6795`) —, die MSI-X-Tabelle liegt anderswo. Das ist die richtige Fassung, aber ein Zufall der Auswahlregel. Als Regel: der Kernel läuft die Capability-Liste (`0x11`), **weist das Gerät ab**, wenn die Tabelle in der angebotenen BAR liegt, und **schreibt die Zeile selbst**; der Treiber wählt nur den Queue-Index. Grund: *der Handle ist eine Zahl, keine Autorität — und der Vektor auch nicht.* |
| E12 | **Wo hängt die IRQ-Bindung?** | **entschieden 2026-08-26**: an der **Zuteilung** (`DriverAssign`), nicht an einer globalen Tabelle. Es gibt bereits eine natürliche Obergrenze — ein Vektor je Gerät —, also braucht es kein Konto: wer ein Gerät hat, hat genau dessen Bindungen, und kein Treiber-PD kann einem anderen die Ressource wegnehmen. `ERR_IRQ_FULL` bleibt und meint etwas **Lokales**. Trägt in den MSI-X-Ausbau: die Grenze ist dann *Vektoren dieses Geräts*, keine global neu zu verhandelnde Zahl. |

**E2 ist die Entscheidung, die alles andere bepreist.** Sie besteht praktisch aus einer Liste von Pfaden: welche Verzeichnisse des Linux-Baums übersetzt werden. Solange die Liste nicht steht, sind zwei verschiedene Projekte im Umlauf.

---

## 10. Bekannte Grenzen

| Grenze | Charakter |
|---|---|
| **Uniprozessor je Instanz** | Durchsatzdeckel je Gerät, zusätzlich zu einer etwaigen Bounce-Kopie. **Gemessen 2026-08-26**: `SYS_SPAWN` legt jeden neuen Thread auf den Kern des Aufrufers, alle Threads einer PD teilen sich also einen Kern. Das ist die Voraussetzung, unter der E3 entschieden ist — fällt sie, fällt die Entscheidung mit |
| **Ein Vektor je Gerät (Stufe B)** | Single-Queue. Der zweite Treiber verlangt MSI-X. |
| **Zero-Copy aus User-PDs** | DMA außerhalb der Arena ist nicht abgedeckt. Betrifft `O_DIRECT`, `sendfile`, Zero-Copy-Netzwerk. Optimierung, kein Korrektheitsposten. |
| **Firmware** | `request_firmware` scheitert zur Laufzeit. Pfad existiert; Blobs und Lizenzen sind eine getrennte, automatisierbare Frage — sofern sie vorliegen. |
| **Lizenz** | Siehe Abschnitt 11. Betrifft das Ausliefern, nicht das Laufen — aber eine Kante ist ungelöst. |
| **Versionsbindung** | „Alle Treiber" heißt „alle Treiber dieser Kernelversion". Der Sprung ist automatisierbar, aber nicht gratis. |
| **`mac80211`, DRM** | Eigene Projekte mit eigener Entscheidung, keine Punkte auf derselben Liste. `iwlwifi` heißt 802.11-Stack; GPU heißt DRM mit GEM/TTM, `dma-fence`, `drm_sched`, dazu Mesa mit libc, pthreads, `dlopen`, C++-Runtime und LLVM-Backend. |
| **Seitenkanäle** | Adressraumisolation hält gegen direkte Zugriffe, nicht gegen Page-Table-Covert-Channels. Bekanntes Ergebnis für vergleichbare Systeme. |

---

## 11. Lizenzierung

### Festlegung

| Artefakt | Lizenz | Läuft mit? |
|---|---|---|
| **Caprock (Kernel)** | AGPL-3.0 | eigenes Programm |
| **A-Schablonen** | GPL-2.0 | **nein** — nur beim Übersetzen |
| **Compiler-Plugin** | GPL-2.0 | **nein** — Werkzeug |
| **Emittierter A-Zustand** | GPL-2.0 | ja — im Treiber-PD |
| **Übersetzte Linux-Subsysteme** | GPL-2.0-only | ja |
| **Treiber** | GPL-2.0-only | ja |

### Warum die Schablonen die einfachere Hälfte sind

Weil sie zur Laufzeit nicht vorhanden sind, werden sie **nicht mit Linux-Code gelinkt**. Die übliche Argumentationskette für abgeleitete Werke — gemeinsamer Adressraum, gemeinsame Übersetzungseinheit, Header-Einbindung — greift für den Schablonen*quellcode* nicht. Das Verhältnis ist dasselbe wie zwischen einem Compiler und dem Code, den er übersetzt: GCC ist GPL-3.0, die von GCC erzeugten Binaries sind es nicht.

GPL-2.0 ist hier also eine **Wahl**, keine Pflicht. Sie ist trotzdem sinnvoll: sie hält alles, was das Treiber-PD betrifft, in einer Lizenz und macht die Frage nicht wieder auf.

Zwei Dinge sind davon unabhängig:

1. **Der emittierte A-Zustand** — Allokator, Timerrad, Workqueue, `mem_map` — landet sehr wohl im Binary, neben GPL-2.0-only-Code. Er muss GPL-2.0-kompatibel sein. Bei GPL-2.0-Schablonen ist er das automatisch.
2. **Das Binary insgesamt** ist GPL-2.0-only, weil Treiber und Subsysteme es sind. Das ist unabhängig von jeder Entscheidung, die hier getroffen wird.

### Die Kante, die bleibt

**AGPL-3.0 und GPL-2.0-only sind zueinander inkompatibel.** Linux ist ausdrücklich GPL-2.0-**only**, nicht „or later" — es gibt keinen Upgradepfad.

Für den Kernel ist das folgenlos: das Treiber-PD ruft **Syscalls** auf. Die Syscall-Grenze ist die klassische Trennlinie zwischen zwei Programmen, kein Link, und es ist genau die Grenze, die Linux für sein eigenes Userland selbst zieht. Caprock bleibt AGPL-3.0.

**Die Kante liegt bei allem, wogegen das Treiber-PD tatsächlich linkt.** Die Schablonen erzeugen Syscall-Sequenzen; ob dabei eine Caprock-Userland-Bibliothek mitgelinkt wird — Startup-Code, Syscall-Wrapper, Hilfsroutinen — ist eine Implementierungsfrage, die noch offen ist.

```
┌─────────────────────────────────────────┐
│  Treiber-PD (ein Binary)   GPL-2.0-only │
│    Treiber, Subsysteme, emittierter     │
│    A-Zustand                            │
│    + evtl. Caprock-Userland-Code        │
│      ← muss dann GPL-2.0-kompatibel sein│
└──────────────┬──────────────────────────┘
               │ Syscalls ← Programmgrenze, kein Link
┌──────────────┴──────────────────────────┐
│  Caprock (Kernel)          AGPL-3.0     │
└─────────────────────────────────────────┘
```

**Der saubere Ausweg ist architektonisch, nicht juristisch:** emittieren die Schablonen die Syscall-Sequenzen vollständig selbst, wird gar keine Caprock-Bibliothek gelinkt, und die Frage entfällt. Das ist ohnehin die konsequentere Fassung des Modells — eine mitgelinkte Laufzeit wäre die Bibliothek durch die Hintertür.

Fällt doch Caprock-Userland-Code an, muss er permissiv oder GPL-2.0-kompatibel sein. Der Kernel bleibt unberührt.

### Compiler-Plugin: GPL-2.0 kollidiert mit dem Hostcompiler

Anders als bei den Schablonen ist das hier ein konkretes Problem, weil die Kopplung zum **Compiler** besteht:

| Host | Lizenz | Verhältnis zu GPL-2.0-only |
|---|---|---|
| GCC | GPL-3.0-or-later, Plugin muss `plugin_is_GPL_compatible` setzen | **inkompatibel** |
| Clang/LLVM | Apache-2.0 mit LLVM-Ausnahme | **inkompatibel** (Patentklausel) |

**Empfehlung:** an die Lizenz des Hostcompilers angleichen oder permissiv. Es geht nichts verloren — das Plugin ist Werkzeug und kein Teil des ausgelieferten Systems.

### Firmware

Getrennte Frage, getrennte Lizenzen. Firmware-Blobs stehen unter herstellerspezifischen Redistributionslizenzen, die von „frei verteilbar" bis „nur mit Vertrag" reichen. Das Laden ist unproblematisch, das **Mitausliefern** je Blob zu prüfen.

Praktikable Trennung: Blobs nicht ins Image, sondern zur Installationszeit aus einer bestehenden Quelle beziehen — dieselbe Lösung, die Distributionen für `linux-firmware` mit unfreien Anteilen verwenden.

### Was zu klären bleibt

1. **Linkt das Treiber-PD überhaupt Caprock-Code?** Wenn die Schablonen ihre Syscall-Sequenzen vollständig selbst emittieren: nein, und die Frage entfällt. Fällt bei S0 an, weil sie zur Struktur des Baums gehört und nicht nachträglich zu trennen ist.
2. **AGPL §13 und der Treiberstapel** — die Netzwerkklausel greift für Caprock. Die Treiber-PDs sind separate Programme unter GPL-2.0-only; §13 reicht nicht in sie hinein. Das sollte dokumentiert und nicht implizit gelassen werden.
3. **Präzedenz ansehen:** Genode Labs betreibt AGPL-3.0 neben GPL-2.0-Treibern aus `dde_linux`. Die dortige Handhabung der Grenze ist das nächstliegende Vorbild und lohnt einen Blick, bevor die eigene Konstruktion festgeschrieben wird.

---

## 12. Einschätzung

**Erreichbar** für PCIe-Geräte unter x86-64, mit einer Kernelversion, gegen die in Abschnitt 5 aufgezählten Primitive und die drei einmaligen Bauteile (Host-Ops, Klassenschicht je Geräteklasse, Baukette).

**Der Hebel** ist, die Subsysteme aus dem echten Quellbaum zu übersetzen statt ihre API nachzubauen. Genau das macht die Bedingung „ohne Menschen je Treiber" erfüllbar; eine reine Emulationsschicht macht sie unerfüllbar, weil je Symbol ein Gutachten fällig wird, das erst im Betrieb geprüft wird.

**Der kritische Posten** ist der Pfad für echtes MMIO und echtes DMA. Dort liegen die Fehler, die still sind, und dort entscheidet sich alles Nachgelagerte.

**Der billigste Test zuerst:** M1 und M2 haben keine Voraussetzungen, kosten zusammen einen Nachmittag und eine Nacht Rechenzeit, und können die Abdeckungsseite widerlegen, bevor ein einziges Kernel-Primitiv gebaut ist.

---

## Anhang: Begriffe

| Begriff | Bedeutung |
|---|---|
| **A-Symbol** | Kernelfunktion mit Standard-OS-Semantik. Wird schabloniert. |
| **B-Symbol** | Kernelfunktion mit Linux-eigener Datenstruktursemantik. Wird übersetzt. |
| **A-Header** | Der Vertrag zwischen den Spuren. Signaturen, Zusicherungen, Fehlercodes. |
| **Klassenschicht** | Adapter je Geräteklasse (Block, Netz, USB, Display), gegen das Subsystem geschrieben. |
| **Schablone** | A-Element mit expliziter Zusicherung, maschinell gegen den Bedarf des Aufrufers geprüft. |
| **Spur K / Spur L** | Die beiden parallelen Arbeitsspuren (Kern / Linux). |
| **Stufe A / B / C** | Die drei Lieferstufen der Caprock-Primitive (Uhr, IRQ, Arena). |
