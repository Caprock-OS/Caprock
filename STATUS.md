# STATUS — laufender Stand beider Stränge

*Diese Datei ist der Blick von aussen: **was läuft gerade, was ist fertig, was blockiert.**
Beide Agenten schreiben ihren eigenen Abschnitt und lassen den des anderen in Ruhe.
Aktualisiert wird nach jedem abgeschlossenen Schritt, nicht nach der Uhr.*

**Zuletzt geändert (B): 2026-07-29 19:38 UTC**

---

## Strang B — Verlässlichkeit und Isolation (Claude B)

**Gerade in Arbeit:** B-1.2c (Wiederholungsmessung neu, mit dem getrennten Marker). B-1.5 bis
B-1.8 sind erledigt (s. unten), B-2 ebenfalls.

**B-1.7 beantwortet:** der x86-Fehler existiert auf aarch64 **nicht** — aber aus einem stärkeren
Grund als vermutet. Nicht „die ARM-Suite baut ein Archiv", sondern: das arch-neutrale `all_done()`
in `threads/mod.rs` enthält **überhaupt keine** archivabhängige Aussage. Kehrseite, dabei
gefunden und A gemeldet (Mitteilung 8): genau deshalb ist der Root-Task auf ARM in **keiner**
Abschlussbedingung und wird auch per grep nicht geprüft — sein Fehlschlag wäre unsichtbar.

**B-1.8 — der unangenehmste Fund:** `report_and_off()` druckte auf x86 `SELFTEST COMPLETE`
**bedingungslos**, auch nach dem Watchdog (im selben Log Z. 99 `WATCHDOG`, Z. 119 `COMPLETE`).
Damit konnte ausgerechnet der Marker, auf dem die Wiederholungsmessung steht, einen vollständigen
Lauf nicht von einem abgebrochenen unterscheiden. Nach aarch64-Vorbild getrennt, die Suite nimmt
die Trennung ab. **Folge: B-1.2s „16 von 16" ist nicht widerlegt, aber nicht belegt** — mit diesem
Marker gezählt. Neu zu messen als B-1.2c.

**Der Fund des Tages (B-1.6):** die x86-Suite hat ihren Bericht seit `6d68328` **jedes Mal aus der
Notbremse** abgesetzt — `all_done()` verlangte `root_chain_done() && cdelete_done()`, beide ohne
Boot-Archiv prinzipiell unerfüllbar, und diese Suite baut absichtlich keines. Damit erschien der
Bericht nach einem Spin-Zähler statt nach dem letzten Beleg, und **jede knappe Aussage war ein
Rennen**: der `iso`-Test lieferte bei identischem Bau `2x`, `1x`, `0x` Faults. Behoben; danach
**5 von 5 Läufen ohne WATCHDOG, `iso` durchgehend grün**, einziger FAIL `x2APIC` (TCG).

**Für A freigegeben (Mitteilung 6):** die volle x86-Suite ist mit A's `default = []` im
Arbeitsbaum gelaufen — **genau ein `FAIL`, `x2APIC`**, also unverändert gegenüber 12:38. A-2.2 ist
damit von B-Seite belegt; `kernel/Cargo.toml` und `test-qemu-x86-load.sh` warten auf A's Commit.
Ich habe sie nicht angefasst (Regel 2). Log: `build/diag/b-uebernahme-suite.log`.

**Als Nächstes: B-4.2 vor B-4.1 — die Reihenfolge ist gedreht, und zwar begründet.** Ursprünglich
stand B-4.1 (gefärbter Pfad als Normalfall) zuerst. Beim Lesen des Codes zeigten sich zwei Gründe,
warum das so nicht geht:

* **Es gibt nur vier Streifen.** `PARTITIONS = 4`, und `mask_for(i)` vergibt sie mit `i % 4` —
  rundläufig, **ohne Belegungsprüfung**. Solange der gefärbte Pfad die Ausnahme ist (heute ruft ihn
  nur der Farbtest mit `i = 0,1`), ist das harmlos. Als Normalfall teilt die **fünfte**
  gleichzeitige PD ihre Farben still mit der ersten — und die Suite startet reihenweise isolierte
  Threads. Aus „ungefärbt, also keine Zusage" würde „Zusage gegeben und still gebrochen".
* **Die Region schrumpft um Faktor 32.** `spawn_isolated` gibt 2 MiB (`ISO_REGION_SIZE`), der
  gefärbte Pfad `region_bytes() = MASK_BITS/PARTITIONS × 4 KiB = 64 KiB`. Ein Dreh würde jedem
  isolierten Thread den User-Stack lautlos kürzen, bis einer überläuft.

Der Weg für B-4.1 danach: die 2 MiB **aus mehreren gefärbten Läufen desselben Streifens**
zusammensetzen und seitenweise mappen — dann bleiben Regionsgröße *und* Farbeigenschaft. Preis
sind die 512 PTEs statt eines Blockdeskriptors, und der steht ohnehin schon in der Funktionsdoku.

**Fertig und belegt:**

| Punkt | Ergebnis | Commit |
|---|---|---|
| B-1.1/1.2 IRQ-Sicherheit der SpinLocks auf x86 | 7 von 8 → **16 von 16** vollständige Läufe | `ab76273` |
| B-1.4 Fehlerklasse gesucht + Wächter zur Übersetzungszeit | `caprock-sync` war die einzige betroffene Crate; Empfindlichkeit belegt | `ab76273` |
| B-1.3 Wiederholungsmodus der Suite | `RUNS=n`, Quote unter 100 % ist FAIL; Probelauf 5 von 5 | `b43fc14` |
| B-2.1 ARM-Suite aus frischem Klon | Testschlüssel wird erzeugt statt eingecheckt, Kernel danach neu gebaut; **`== ALL PASS ==` aus einem frischen Klon von HEAD** | `02a1407` |
| B-2.2 `hal::cache` auf ARM wirklich ausgeführt | `cortex-a72`/`a53` → 16 Farben, `max` → 32: **die Werte unterscheiden sich**, also wird CCSIDR gelesen und keine Konstante | `d4d27f1` |
| B-2.2b CCIDX-Zweig geprüft | Feldzerlegung als reine Funktion (`hal::cache_decode`), beide Layouts gegen eingespeiste Registerwerte, **5 von 5** — obwohl keine QEMU-CPU CCIDX meldet | `02a1407` |
| B-2.3 README | von „aarch64, Phase 7" auf den tatsächlichen Stand; zwei Falschaussagen des Entwurfs beim Prüfen gefunden und korrigiert | `529bc35` |
| B-2.4 `docs/verification.md` | Loom Stufe 2 abgehakt — **mit der Grenze daneben**, die 2026-07-29 teuer wurde | `529bc35` |
| B-4.4 Zusicherung ehrlich aufgeschrieben | `invariants.md` §12: was A1 trennt, und die längere Liste dessen, was **nicht** | `529bc35` |
| B-1.5 Erwartete Abwesenheit ausgesprochen | zwei Checks nehmen ab, dass ohne Boot-Archiv kein Root-Task laeuft **und der Kernel den Grund nennt** — kein Filter, der die Zeilen versteckt | `8f5b2a7` |
| B-1.6 Bericht kam aus der Notbremse | `all_done()` war ohne Archiv unerfuellbar -> WATCHDOG in JEDEM Lauf; danach **9 von 9 ohne Watchdog**, `iso` durchgehend gruen (vorher 2 von 4) | `8f5b2a7` |
| B-1.7 aarch64 gegengeprueft | Fehler existiert dort **nicht** — arch-neutrales `all_done()` hat gar keine archivabhaengige Aussage. Kehrseite an A: sein Root-Task ist auf ARM in KEINER Abschlussbedingung | *dieser Commit* |
| B-1.8 Erfolgsmarker log | `SELFTEST COMPLETE` wurde auch nach dem Watchdog gedruckt (Z. 99 + Z. 119 im selben Log); nach aarch64-Vorbild getrennt, Suite nimmt die Trennung ab | *dieser Commit* |
| A1 Stufe 1 Cache-Coloring | `color : ALL PASS`, 256 Farben gemessen | `7a87182` |
| Feature `selftest` (todo F1) | `.text` 0x25000 → 0x11000 (54 %) | `7a87182` |
| Zielarchitektur Z, Plan, Strang-Aufteilung | — | `6e4cf9d` |

**Testlage x86 (9 Läufe, 19:36, nach B-1.6/B-1.8, mit A's `default = []` im Baum):** **9 von 9 ohne
WATCHDOG**, `iso` durchgehend grün, einziger FAIL: `x2APIC` — TCG kann das
Merkmal grundsätzlich nicht (`TCG doesn't support requested feature: CPUID.01H:ECX.x2apic`), kein
`/dev/kvm` im Container. **Kein Regress, sondern eine Grenze des Aufbaus.**

**Testlage aarch64: läuft** (seit B-2.1), zuletzt `== ALL PASS ==` aus einem frischen Klon. Damit
ist auch A's neuer ARM-Root-Task-Pfad nicht mehr auf ein Argument angewiesen.

**Host-Arithmetik (17:15):** `caprock-mem` 13 von 13, `hal::cache_decode` 5 von 5, je 0,00 s.

**Blockiert:** nichts.

**Für Strang A relevant:** `system::testsupport` liegt jetzt hinter `selftest`;
`test-qemu-x86.sh` baut die Konfiguration ohne das Feature mit und prüft, dass `.text` dabei
schrumpft. Details in [AGENTS.md](AGENTS.md), Mitteilung 1.

---

## Strang A — Ausführen und Austauschen (Claude A)

**Zuletzt geändert (A): 2026-07-31 16:55 UTC**

**Gerade in Arbeit: nichts Angefangenes — A-4 ist VOLLSTÄNDIG.** A-4.3 ist committet (`78f0d52`),
damit stehen A-4.1 (`a25fa23`), A-4.2 (`4fca286`), A-4.3 (`78f0d52`), A-4.4 und A-4.5; A-3.4 ist
komplett (Teil 1 `ec26cfb`, Teil 2 `1e2bd51`, Teil 3 `f6e5186`, Teil 4 `25d388a`, Summenprüfung
`d1f5ed8`). Im Arbeitsbaum liegt nur **fremde** Arbeit: der QEMU-Auswahlblock in
`test-qemu-x86.sh` und `todo-B-verlaesslichkeit.md` (beides B, nicht mitcommittet).
**Nächster Schritt ist zu wählen** — A-4 gibt keinen mehr vor.

**A-4.3 — Zustandsübergabe (`78f0d52`).** Gewählt ist die Region, die den Austausch überlebt —
und damit ist ihr Format eine **ABI**. Der Punkt, an dem die Sache hängt, ist nicht das
Überleben der Bytes, sondern die Frage, ob die neue Fassung weiß, *welche* Bytes sie liest: eine
Region ohne Kopf verlagert das Problem bloß, weil v2 die Daten von v1 dann im eigenen Sinn deutet.
Das ist kein Datenverlust, sondern ein fehlinterpretierter Zustand — der eine fällt auf, der
andere nicht. Also versionierter Kopf (`state.rs` in `caprock-region`): `state_version`,
`program_id`, Übernahmezähler; abweichendes Layout und fremde `program_id` werden **abgewiesen**
statt gedeutet, eine frische Region meldet `NoState` statt „Version 0", und der Übernahmezähler
steigt nur bei tatsächlicher Übernahme — erhöhte ihn eine Abweisung, belegte er das Gegenteil
dessen, was er behauptet. Belegt in zwei Stufen: Torlogik auf x86 (`state : ALL PASS`),
Ernstfall auf aarch64 (`ckpt : ALL PASS` — v1 rechnet `+1` und hinterlässt `[1, 2, 3]`, v2 rechnet
mit `+10` weiter auf `[13, 23]`, also auf den übernommenen Werten, Generation 1). Der Ernstfall
läuft **nicht** auf x86, weil er an der arch-neutralen Thread-Demo hängt, die dort nicht startet;
das steht als Anmerkung im x86-Skript, damit ein grünes `state` nicht mehr verspricht, als es
prüft. Suitenlauf 16:21/16:23: `rc_load=0 rc_arm=0`, beide Logs `== ALL PASS ==`, keine FAIL-Zeile.

**Falle nach Container-Neubau (heute zweimal aufgeschlagen):** das Python-Paket `cryptography`
fehlte (Load-Suite bricht mit `rc_load=2` ab, Manifest-Signieren geht nicht) und die
**git-Identität war weg** (`Author identity unknown`). Ersteres per
`pip3 install --user --break-system-packages cryptography`, letzteres bewusst **nicht** in die
Repo-Config geschrieben, sondern pro Commit über `GIT_AUTHOR_*`/`GIT_COMMITTER_*` gesetzt — eine
repo-weite Identität ließe Strang B unter A's Namen committen.

**A-4.1 — atomares Umbinden (`a25fa23`).** Der Austausch prüfte bisher und tauschte danach.
Zwischen beidem liegt ein Fenster: der Befund „keine offene Transaktion" ist eine Momentaufnahme,
die im nächsten Takt nicht mehr gilt; und zwischen „alter Server gelöst" und „neuer gebunden"
hätte der Endpoint kurz **keinen** Empfänger — ein Client, der genau dann sendet, sieht einen
Server, den es nie gab. Jetzt laufen Prüfung und Tausch unter *einem* Lock. Ohne vorherige
Stilllegung wird abgewiesen, ein fremder Empfänger blockiert den Tausch statt still verdrängt zu
werden, und im überlappenden Fall ist durchgehend genau ein Empfänger gebunden. Belegt:
`rebind : ALL PASS`, Lauf 13:04 mit `rc_arm=0 rc_load=0 rc_main=0` — beide Suiten ohne eine
einzige FAIL-Zeile.

**A-4.2 — der ruhende Punkt (`4fca286`).** Vor A-4.1 gebaut, mit Absicht: 4.1s ehrliche Variante
(„Austausch nur ohne offene Transaktion") setzt den Begriff *ruhend* voraus, den es nicht gab, und
derselbe Begriff trägt später Z4a (Thread einfrieren). Stilllegen weist **neue** Transaktionen mit
`ERR_QUIESCING` ab, nicht mit `ERR_BADCAP` — „kommt gleich wieder" ist für den Client eine andere
Lage als „gibt es nicht"; wer beides vermengt, zwingt ihn zum Aufgeben, wo Warten richtig wäre.
Laufende Transaktionen dürfen abschließen, ein zweiter Austausch am selben Endpoint wird
abgewiesen, Freigabe öffnet das Tor wieder, doppelte Freigabe wird gemeldet. Belegt:
`quiesce : ALL PASS`, Lauf 11:30/11:51 mit `rc_arm=0 rc_load=0 rc_main=0`.

**Der `x2APIC`-Vorbehalt ist weg — und er lag nie am Kernel.** Seit `/opt/tools/qemu`
(QEMU 11.0.3, von außen installiert) meldet der Kernel `apic: x2APIC (MSR-Pfad)` statt des
MMIO-Rückfalls; Debians 7.2 kann das Merkmal unter TCG schlicht nicht
(`TCG doesn't support requested feature: CPUID.01H:ECX.x2apic`). Die Hauptsuite ist damit erstmals
`== ALL PASS ==` ohne Ausnahme. **Offene Unwucht:** `test-qemu-x86-load.sh:103` ruft weiter das
nackte `qemu-system-x86_64` (7.2) — beide Suiten laufen auf **verschiedenen** Maschinenmodellen,
ein grünes Ergebnis heißt in beiden nicht dasselbe. Nachzuziehen, sobald der fremde QEMU-Block
committet ist.

**Umgebungsfalle, die 11:47 einen Lauf gekostet hat:** der Container wurde neu gebaut, `~/.local`
war weg, damit auch das per `pip3 install --user` nachgerüstete `cryptography` — ohne das kann
kein Manifest signiert werden und die Lade-Suite bricht vor dem ersten Bau ab (`rc_load=2`).
Wiederhergestellt; **überlebt den nächsten Neubau wieder nicht**, gehört ins Dockerfile.

Erledigt ist die Thread-Kapazität als
**Zusage** (`TARGET_THREADS = 10_000`, gemessen:
`4 Kern, 10000 Thread-Slots (5000 hostbar), Tabellen 7872 KiB aus dem RAM`), die
Cap-Space-Telemetrie (Höchststand statt Endstand — ein Lauf, der zwischendurch an die Grenze
stiess und danach aufräumte, sieht am Ende harmlos aus) und seit Teil 2 der **Cap-Space aus dem
Boot-RAM**.

**Der Fund, der A-3.4 begründet — geschlossen:** über `CAP_BUDGET_PER_PD = 8` stand, es
verhindere einen Cross-PD-DoS. Nachgerechnet waren das `NPDS * CAP_BUDGET_PER_PD` = 256 × 8 =
2048 gegen **256** vorhandene Slots: 32 PDs mit vollem Budget füllten die Tabelle, die 33. bekam
nichts — genau der DoS, den das Budget verhindern soll. Teil 2 dreht `slots`/`objects` von
`[CapSlot; 256]`/`[Object; 128]` auf `Slab<_>` und lässt `configure_caps()` sie beim Boot
allozieren, dimensioniert nach `CAP_SLOTS_FOR_ALL_PDS` (2048) + 256 Reserve. Gemessen im
x86-Bringup: `cap : 2304 Slots / 2304 Objekte, Tabellen 400 KiB aus dem RAM (Summe aller
PD-Budgets: 2048)` und `capsz : Hoechststand 11/2304 … bei vollem Budget passen 288 PDs in die
globale Tabelle`. **288 > 256** — die Summe passt jetzt hinein, ohne die PD-Zahl zu senken.

Zwei Nebenwirkungen von Teil 2, die eigenständig zählen: `audit_cdt` nimmt die Zählfläche als
Puffer vom Aufrufer und meldet mit Code 8 „konnte nicht laufen" statt still „konsistent";
`ipc_audit()` in `system.rs` ging als **einzige** Stelle am Wrapper `cap_audit_cdt()` vorbei und
damit an der Sperrordnung (CAP_AUDIT vor CAPS) — behoben. `MAX_FINALIZED` ist raus,
`#![forbid(unsafe_code)]` erzwingt in der Cap-Crate jetzt, was vorher nur behauptet war.

**Teil 3 (`f6e5186`) schliesst die PD-Tabelle an:** `[Pd; NPDS]` im `.bss` war der Grund, warum
10000 Threads keine 10000 Tenants waren — 256 Adressräume, danach nur noch geteilte. `pds` ist
jetzt ein `Slab`, `configure_caps()` hängt ihn beim Boot an, `NPDS` steht auf **10000**. Gemessen:
`cap : 80256 Slots / 80256 Objekte / 10000 PDs, Tabellen 17792 KiB aus dem RAM`, dazu
`PASS: A-3.4: 10000 PD-Slots`. Die Zahl kostet RAM, keine Struktur.

**Der Fund in Teil 3 — kein Testartefakt:** `create_vspace_masked` allozierte die **dritte**
Tabellenebene über den ungefärbten `mem_alloc`. Auf x86_64 (PML4 → PDPT → PD) lag damit **eine der
drei** Tabellen jeder gefärbten PD ausserhalb ihres Farbsatzes — und der Farbtest sah es nicht,
weil er nur `l1`/`l2` zurückliest. Ein Seitenlauf der MMU im Namen dieser PD hinterlässt dort
dieselben Spuren wie in den beiden anderen. Auf `mem_alloc_masked` gezogen; schlägt die gefärbte
Zuteilung fehl, scheitert das Anlegen der VSpace, statt fremde Farben mitzunehmen. **Berührt
Strang B** (Streifenbuchhaltung) — deshalb hier benannt.

Dazu in `colors.rs` getrennt, was zwei Befunde sind: `kernelseite=` heisst jetzt nur noch „Farbe
hält", daneben steht `rueckgelesen=`. Vorher fielen eine gebrochene Färbung (Isolationsfehler) und
eine `0` aus dem Rückkanal (Fehler des Tests) in dasselbe Bit — ein `kernelseite=0` liess sich
nicht deuten, ohne zu raten. Beides fällt weiterhin durch (`ok` fordert beide). Dieselbe Trennung
wie bei `audit_cdt` (Code 8) seit Teil 2.

**Teil 4 (`25d388a`) schliesst die letzte feste Tabelle der Kette:** `NENDPOINTS`/`NNOTIFICATIONS
= 32` in `caprock-ipc` hiessen, dass mit `NPDS = 10000` zwar jede PD einen eigenen Adressraum
haben konnte, aber ab der 33. keine mehr Server sein. Beide sind jetzt `Slab<_>`, beim Boot
dimensioniert nach „eine PD, ein Endpoint" (`NPDS` + Reserve = 10064) und in **beiden**
Boot-Pfaden (`main.rs`, `bringup.rs`) vor dem Selbsttest angehängt — dieselbe `attach`-Mechanik
wie CapSpace (Teil 2) und PD-Tabelle (Teil 3). Gemessen: `ipc : 10064 Endpoints / 10064
Notifications, Tabellen 16908 KiB aus dem RAM (eine PD, ein Endpoint: 10000 PDs)`, dazu
`PASS: A-3.4: 10064 Endpoints / 10064 Notifications`.

**Der Abschluss (`d1f5ed8`) — die Summe wird jetzt geprüft, nicht nur unterstellt:**
`budget_allows` deckelt den Verbrauch **einer** PD; dass die Summe aller Budgets in die Tabelle
passt, trug bis hierher allein die Dimensionierung (10000 × 8 = 80000 + 256 Reserve = genau die
80256 Slots). Ungeprüft blieb die andere Seite derselben Rechnung: **der Kernel selbst** installiert
Wurzel-Caps, und nichts hinderte ihn daran, mehr als seine 256 Reserve zu belegen. Dann ist jede
einzelne PD innerhalb ihres Budgets und bekommt trotzdem `NoSlot` — an einer Stelle, die nichts
falsch gemacht hat. `Caps::nonbudget_slots` zählt die Slots, die auf kein PD-Budget gehen,
ausdrücklich **nicht** über `used_slots − Σ cap_count(pd)`: halten zwei PDs denselben `CapPtr`,
überzählt die Summe, und der Fehlbetrag zeigt in die unsichere Richtung — die Prüfung ginge durch,
obwohl sie es nicht sollte. Stattdessen wird jeder gehaltene Slot markiert und ausgezählt, was
belegt und unmarkiert blieb; eine zu kleine Zählfläche ist ein eigener Befund (`None`), kein
stilles „in Ordnung" (dieselbe Trennung wie Code 8 im CDT-Audit). Dazu
`CAP_SLOTS_TOTAL`/`CAP_SLOTS_KERNEL_RESERVE` in `caprock-microkit` — der Kernel hatte
`CAP_SLOTS_FOR_ALL_PDS + 256` abgeschrieben, eine zweite Stelle, die beim nächsten Drehen an `NPDS`
still auseinanderläuft — und eine Prüfung nach `attach`, dass die Tabelle wirklich so gross ist wie
gerechnet. Gemessen: `capsum : 1/256 Slots ausserhalb aller PD-Budgets (Kernel-Wurzelcaps);
Kapazitaet 80256 = 80000 PD-Budgets + 256 Reserve`.

**Nächster Schritt:** A-3.4 ist damit **komplett** — die dynamischen Tabellen stehen (Threads,
Caps, Objekte, PDs, Endpoints, Notifications), und die Summe ist geprüft statt unterstellt. Als
Nächstes A-4 (Hot-Reload, 4.1–4.3), danach A-5.

**Belegt durch Lauf 09:22** (`build/diag/a34-summe.log`): `rc_arm=0` (aarch64-Bau als
Regressionsschutz für die arch-neutralen Crates), `rc_load=0` mit `== ALL PASS ==`, `rc_main=1` mit
genau einem FAIL — `x2APIC`, der bekannte TCG-Vorbehalt ohne KVM. Teil 4 zuvor belegt durch Lauf
21:31 (`build/diag/a34-teil4.log`), gleiche Bilanz.

**Neu erledigt (2026-07-30):** A-2.2 (`default = []`), A-4.4 (Versionssperre im Lader, beide
Ausgänge belegt), A-4.5 (Negativliste `invariants.md` §13), A-3.4 Teil 1 bis Teil 4 (s. oben).

**Eine Grenze, die zu A-4.4 gehört und nicht verschwiegen wird:** über das Manifest ist der
Abweisungszweig heute **nicht erreichbar** — pro Boot gibt es genau ein Manifest. Er wird es erst
mit einem Austausch, der zur Laufzeit ein anderes Image mitbringt (A-4.1/A-4.3). Damit er bis
dahin nicht ungeprüft bleibt, füttert der Selbsttest die Buchhaltung direkt (`iface : ALL PASS`).

**Fertig und belegt:**

| Punkt | Ergebnis | Commit |
|---|---|---|
| A-1.1 Multiboot-Module | `mbmod : ALL PASS` — Modulbereiche werden aus der Freiliste **ausgeschnitten** (Rand, Überlappung, unsortiert, Vollabdeckung eingespeist) | `ee8029c` |
| A-1.2 Manifestformat | 80-B-Kopf / 96-B-Einträge, `entry_len` im Kopf; host-getestet inkl. Mutationslauf, Kani-Beweise | `6d68328` |
| A-1.3 Manifest als Autoritätsdokument | signiert, **an das Kernel-Image gebunden** (SHA-256 über `[__text_start, __rodata_end)`); Prüfreihenfolge trägt der Typ (`Verified`) | `6d68328` |
| A-1.4 Politikfelder | Format steht und wird ausgewiesen; **angewandt werden sie noch nicht** — dort übernimmt B | `6d68328` |
| A-1.5 `SYS_LOAD` auf x86 | `load_by_index` ist kein `None`-Stub mehr; ELF-Parser kannte nur `EM_AARCH64` | `6d68328` |
| A-2.1 Root-Task | `root : ALL PASS` — lädt über die **eigene** Loader-Cap nach (zweites Badge belegt es) | `6d68328` |
| A-3.1 `SYS_CDELETE` | Syscall 14, aus Ring 3 geprüft, **beide** Ausgänge | `6d68328` |
| A-3.2 `SYS_CMOVE`/`CCOPY`/`SETRECV` | Syscalls 15/16/17, Rechte-Schnitt, Badge, Empfangs-Slot beim **Empfänger** | `6d68328` |
| A-2.2 Vorarbeit | `--no-default-features` war auf **aarch64 nicht übersetzbar**; Testaufrufe gegatet, **Root-Task startet jetzt auch auf ARM**. Alle vier Konfigurationen bauen | `c413012` |

**Offen und benannt** (nicht vergessen, sondern bewusst später):

* `CAP_PD_CONTROL` ist nicht erteilbar — der Kernel **weist ein Manifest ab**, das sie verlangt,
  statt still weniger zu geben. Der ehrliche Weg wäre eine ABI-Erweiterung (`SYS_LOAD` gibt die
  PdControl-Cap der neuen PD zurück); gehört zu A-3.2, steht noch aus.

**Blockiert:** nichts. **A-2.2 ist erledigt** (2026-07-30) — B hat den Beleg geliefert
(Mitteilung 6), der Dreh `default = []` ist committet. Die Suiten fordern `--features selftest`
für den gebooteten Bau ausdrücklich an.

**Nächste Kopplung, bevor A-3.4 anfängt:** B-4.2 führt jetzt Buch über die Farbstreifen, und es
gibt genau vier. Eine dynamische PD-Zahl gegen eine statische Streifenzahl geht nicht auf; der
Vorschlag steht in [AGENTS.md](AGENTS.md) Mitteilung 9 (Weg 2: `POLICY_EXCLUSIVE_STRIPE` aus
A-1.4 — nur wer einen Streifen anfordert, bekommt einen).

**Testlage:** x86-Suite unverändert wie von B berichtet. aarch64 kann ich nicht laufen lassen
(B-2.1, `keys/` gitignored) — die vier Bau-Konfigurationen sind dort **gebaut, nicht gelaufen**,
und der neue ARM-Root-Task-Pfad ist damit **ungeprüft**. Er ist derselbe Aufruf wie auf x86, wo er
grün ist; das ist ein Argument, kein Beleg.

---

## Linux-Compat (opencode)

**Zuletzt geändert: 2026-09-11**

**Stand:** S0 **46/46** + `lx_messen` grün. Syscalls verdrahtet: `SYS_PARK_TIMEOUT`(29),
`SYS_CALL_TIMEOUT`(30), `SYS_FORK`(31), `SYS_EXEC`(32), `SYS_DEBUG`-v2 (33–35),
`SYS_LOAD_IMAGE`(36), `SYS_CSUB`(37). Timeout/Preempt/Kmalloc/Page-Schemata belegt,
`lx-shim-demo` 9/9.

**Dienste:** mem-server Stufe 2 — Protokoll 21/21 + CSUB-Grants 28/28. RSP Stufe 2
(m/M/g/`qSupported`/?). WASM 2a: Spec-Anhang + ENG + MEM + WASI, 27/27. EIME bedingt drin.

**Fahrt 5:** Verify-am-Boot steht (`6d780cc`, Spannen-Rückfall, Mangel-Details):
`[7]lxpd-minelf gestartet (PD=9)`, `vollzahl 7/7 ALL PASS`, `root ALL PASS`,
`SELFTEST COMPLETE`. E2E-IRQ-Fahrt läuft.

**Offen:** B4b (`weckrufe=0`; Adressbildung belegt korrekt, Masken-Fehlalarm entkräftet).
DEBUG 34/35 brauchen `hal::debug` (HAL-Besitz, nur Patch-Text geliefert).

**Audit-Befunde (2026-09-10):** FNV auf Standard `...25c5` zurückgedreht (`d23003e`,
Vektor-Tests + Interop-Anker); fremde `...2325`-Stellen benannt (`caprock-lxpd`,
`lx_driver_manifest.py:40`, `lxpd_glue.rs:231`). #DF-Regel: kein Array >~256 B auf
heissen Pfaden (`dispatch_fork` → Heap + `#[inline(never)]`). Lazy-Guards (Root-Fix).

**Nächstes:** WASM-Engine 2a.

**ARM-Trio + pdfreeze (2026-09-11):** Doppelvergabe im PD-Cspace-Pool gefunden und
behebt (committet `765d662`, s. AGENTS.md 19): PD 0/1 teilten Run `[32..48)` — stale
Freilisten-Glied bei Slot-Wiederverwendung. Belegt: cspace-Host 13/13,
aarch64-Suite `ALL PASS`, x86-Suite (512M) `ALL PASS`.

**Rewrite-Vorfeld (2026-09-11, AGENTS.md 20):** Builds rc=0, cspace 13/13. QEMU-Rot ist
lastkorreliert begründet (kein Fix-Effekt): ARM-Gast grün, Skript rot nur via
`dbg`-Worst-Case (259k/388k grün vs. 700k rot, IDENTISCHES Binary, Load 5–6);
x86 `sperre` 1,6 Mrd. Zyklen (Host-Deschedule) → Watchdog → verif/sweep Folge.
Empfehlung: `tools/abnahme.sh` auf idler Maschine.
