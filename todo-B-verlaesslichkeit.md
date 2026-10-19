# Strang B — Verlässlichkeit und Isolation

**Frage, die dieser Strang beantwortet:** Kann man dem Kernel glauben, was er zusichert — und
trägt die Isolation für fremde Tenants?

Gegenstück: [todo-A-ausfuehren.md](todo-A-ausfuehren.md). Gemeinsame Grundlage: [TODO.md](TODO.md)
(Abschnitt Z) und [docs/plan-betriebsbereit.md](docs/plan-betriebsbereit.md). Die
Koordinationsregeln stehen am Ende von Strang A und gelten für beide.

Dieser Strang ist **nicht** der kritische Pfad — das ist A. Er ist aber die Voraussetzung dafür,
dass die Ergebnisse von A überhaupt etwas bedeuten: B-1 zuerst, sonst misst niemand etwas
Verlässliches.

---

## B-1. Der Lauf muss wiederholbar sein (D0) — **zuerst, für beide Stränge**

- [x] **B-1.1 Ursache gefunden und behoben (2026-07-29): auf x86 war kein `SpinLock` IRQ-sicher.**
      `caprock-sync::irq_save_disable`/`irq_restore` waren **nur für aarch64** implementiert; der
      No-Op-Zweig darunter war für *Host*-Builds gedacht (`cargo test`), fing aber auch das
      x86_64-Kernel-Ziel. Damit lag genau der reentrante Ticket-Deadlock vor, vor dem der
      Kommentar am Modulanfang warnt: der Timer-Tick trifft einen Kontext, der ein Ticket hält,
      der Handler zieht ein neues, das alte wird nie bedient. Beobachtbar als sporadisches
      Stehenbleiben **mitten in einer `println!`-Ausgabe**, bei etwa einem Achtel der Läufe.
      Behoben über `cfg(all(target_arch = "x86_64", target_os = "none"))` — die Unterscheidung
      muss `target_os` sein, denn `cli` ist im Userspace privilegiert.
- [x] **B-1.2 Nachgewiesen (2026-07-29).** Vorher 7 von 8 bzw. 11 von 12 Läufen vollständig; nach
      dem Fix **16 von 16**. Gemessen gegen `SELFTEST COMPLETE`, `-cpu Skylake-Client`, 4 Kerne.
- [x] **B-1.2b miterledigt mit B-1.2c** (Kaestchen stand offen, der Text darunter erklaerte es bereits fuer erledigt). Urspruenglich: Wiederholungsläufe gegen `SELFTEST COMPLETE`, nicht
      gegen die gerade interessierende Zeile. **Merke:** die frühere Aussage „5 von 5 grün" war
      wertlos, weil sie nur auf die `color`-Zeile prüfte — ein Lauf, der danach hängenblieb,
      zählte als Erfolg.
- [x] **B-1.3 erledigt (2026-07-29).** `RUNS=8 ./test-qemu-x86.sh` fährt den Bootlauf N-fach und
      meldet die Quote; eine Quote unter 100 % ist ein **FAIL**, nicht „meistens grün". Vorgabe
      bleibt 1, damit der übliche Aufruf schnell ist. Probelauf: 5 von 5.
- [x] **B-1.4 erledigt (2026-07-29).** `caprock-sync` war die **einzige** Bibliotheks-Crate mit
      arch-`cfg`s; die acht `cfg(not(target_arch = "aarch64"))` in `system.rs` sind ungefährlich,
      weil die Kernel-Crate nie für ein Host-Ziel baut. Damit die Annahme nicht ungeprüft bleibt:
      `IRQ_MASKING_IMPLEMENTED` prüft zur **Übersetzungszeit**, dass jedes Ziel mit
      `target_os = "none"` eine echte Maskierung mitbringt. Die Konstante steht neben jeder
      Implementierung in derselben `cfg`-Kette — eine eigene Kette wäre eine Wiederholung der
      Bedingung, und Wiederholungen laufen auseinander. Empfindlichkeit belegt.

- [x] **B-1.5 erledigt (2026-07-29). Erwartete Meldungen aussprechen, nicht verstecken.** In einem grünen Lauf von
      `test-qemu-x86.sh` stehen `root : FAILURES` und `cdelete : FAILURES` — richtig, weil die
      Suite **kein** Boot-Archiv baut (keine `programs`, kein `mkarchive`, kein Manifest, anders
      als die Lade-Suite) und der Kernel die fehlende Startmenge meldet, statt still zu idlen.
      Geprüft wird beides hier nicht (null Vorkommen im Skript); der Harness-Bericht läuft nur
      ungefiltert durch. Das ist „Stille sieht wie Erfolg aus" mit umgekehrtem Vorzeichen: wer das
      Log liest, kann erwartete von echter Meldung nicht trennen. **Der Weg ist ein Check, der die
      Abwesenheit ausdrücklich abnimmt** — dann schlägt die Suite an, wenn die Zeilen eines Tages
      *nicht* mehr kommen. Ein Filter, der sie versteckt, wäre das Gegenteil davon.
      Gefunden 2026-07-29 im Übernahmelauf (`build/diag/b-uebernahme-suite.log`).

- [x] **B-1.6 erledigt (2026-07-29). Der Bericht kam aus der Notbremse.** `all_done()` in
      `arch/x86_64/bringup.rs` verlangte seit `6d68328` auch `root_chain_done() && cdelete_done()`
      — beide brauchen ein Boot-Archiv, das `test-qemu-x86.sh` absichtlich **nicht** baut. Damit
      wurde `all_done()` dort **nie** wahr und der Bericht fiel jedes Mal aus dem Watchdog nach
      50 Mio. Spins. Belegt: `WATCHDOG` steht in jedem x86-Suite-Lauf, in der Lade-Suite (mit
      Archiv) **null Mal**. Folge: der Bericht erscheint nach einem Zählerstand, nicht nach dem
      letzten Beleg — **jede knappe Aussage der Suite ist seither ein Rennen**. Gemessen am
      `iso`-Test bei identischem Bau: `2x`, `1x`, `0x` Faults in drei Läufen, der letzte ein FAIL.
      Behoben, indem eine Aussage, die diese Konfiguration nicht belegen *kann*, als **nicht
      anwendbar** behandelt wird statt als dauerhaft unerfüllt (`archive`-Parameter, einmal vor
      der Schleife bestimmt). Liegt ein Archiv vor, gilt die Anforderung unverändert voll.
      **Die allgemeine Lehre:** ein Watchdog, der zur Regel wird, ist kein Watchdog mehr, sondern
      der normale Ausgang — und dann misst niemand mehr, was er zu messen glaubt.
- [x] **B-1.7 erledigt (2026-07-29). Der Fehler existiert auf aarch64 nicht — aus einem stärkeren
      Grund als vermutet.** Ich hatte angenommen: „die ARM-Suite baut ein Archiv, also träte er
      nicht auf." Tatsächlich enthält das arch-neutrale `all_done()` in `threads/mod.rs` (163
      Zeilen, ~60 Konjunkte) **überhaupt keine archivabhängige Aussage** — kein `root`, kein
      `cdelete`, kein `loader`. Die Bedingung kann dort nicht unerfüllbar werden, unabhängig vom
      Archiv. Die Vermutung wäre also zufällig richtig gewesen, mit falscher Begründung.
      **Kehrseite, dabei gefunden:** genau deshalb ist der Root-Task auf ARM in **keiner**
      Abschlussbedingung — und `test-qemu.sh` prüft ihn auch per grep nicht. A's neuer ARM-Pfad
      ist damit nicht nur ungeprüft, sein Fehlschlag wäre unsichtbar. Gehört A, ist ihm gemeldet.
- [x] **B-1.8 erledigt (2026-07-29). Der Erfolgsmarker log auf x86.** `report_and_off()` druckte
      `== SELFTEST COMPLETE ==` **bedingungslos** — auch nach dem Watchdog. Belegt: im selben Lauf
      standen `bringup : WATCHDOG` (Z. 99) und `SELFTEST COMPLETE` (Z. 119). Damit konnte
      ausgerechnet der Marker, auf dem die Wiederholungsmessung steht, einen vollständigen Lauf
      nicht von einem abgelaufenen unterscheiden: **B-1.3s `RUNS=n` zählt genau ihn**, und B-1.2s
      „16 von 16" beruht darauf. Der aarch64-Zweig macht es seit jeher richtig
      (`SELFTEST FAILED (watchdog)`); x86 spiegelt das jetzt, und die Suite nimmt die Trennung ab
      (ein Watchdog-Lauf ist ein FAIL, kein „meistens grün").
      **Folge für eine ältere Aussage:** „16 von 16" (B-1.2) wurde mit einem Marker gezählt, der
      beide Ausgänge gleich druckte. Die Zahl ist damit nicht widerlegt, aber sie ist **nicht
      belegt** — sie gehört nach dieser Korrektur neu gemessen. Steht als B-1.2c.
- [x] **B-1.2c erledigt (2026-07-30). `8 von 8 Läufen mit IDENTISCHER Ergebnissignatur.`** Nicht
      nur „achtmal durchgelaufen" — achtmal *dasselbe Ergebnis*. Dafür wurde das Kriterium
      verschärft: `RUNS=n` zählt nicht mehr den Marker, sondern vergleicht die **Ergebnissignatur**
      (alle `xxx : ALL PASS|FAILURES|SKIP`-Zeilen plus den Abschlussmarker, sortiert). Die
      Prüffunktionen der Suite sind reine greps auf genau diese Zeilen, gleiche Signatur heisst
      also gleiches Testergebnis. Weicht ein Lauf ab, druckt die Suite die Differenz und wertet
      ihn als FAIL — denn bei zwei verschiedenen Ergebnissen weiss niemand, welcher Lauf die
      Wahrheit sagt. Damit hätte auch der `iso`-Flake angeschlagen, den B-1.3 in seiner alten Form
      übersehen hätte. Einziger FAIL bleibt `x2APIC` (TCG-Grenze).
      **Damit ist B-1.2b miterledigt** — „gegen `SELFTEST COMPLETE` prüfen, nicht gegen die gerade
      interessierende Zeile" war die halbe Lehre; die ganze ist, gegen das *gesamte* Ergebnis zu
      prüfen.

## B-2. Der zweite Architekturzweig muss laufen

- [x] **B-2.1 erledigt (2026-07-29).** Gewählt wurde der zweite Weg — **erzeugen statt einchecken**:
      fehlt `keys/trusted-test.ed25519`, legt `test-qemu.sh` es über `tools/gen_trusted_key.py` an
      und baut den Kernel **danach** neu (die Key-DB wird hineinkompiliert, die Reihenfolge ist
      zwingend). Ein privater Schlüssel im Repo wäre bei Open Source kein Testschlüssel, sondern
      ein veröffentlichter. Belegt: die aarch64-Suite läuft **aus einem frischen Klon von HEAD**
      auf `== ALL PASS ==` — damit ist der zweite Architekturzweig zum ersten Mal überhaupt
      geprüft. Preis der Entscheidung: das Image ist maschinenlokal, also reproduzierbar
      *innerhalb* eines Checkouts, nicht *zwischen* Entwicklern.
- [x] **B-2.2 erledigt (2026-07-29) — bis auf einen benannten Rest.** Der aarch64-Hochlauf ruft
      jetzt `colors::report()` (neben der `spec`-Zeile: dieselbe Sorte Aussage, was die HW
      hergibt). Dafür war die Suite gar nicht nötig — die Bring-up-Meldungen kommen, bevor das
      Boot-Archiv angefasst wird, ein nackter `qemu-system-aarch64 -kernel` genügt. Gemessen:

      | CPU | LLC | Farben |
      |---|---|---|
      | `cortex-a72` | L2 1024 KiB, 16-fach, 64 B/Zeile, 1024 Sets | 16 |
      | `cortex-a53` | L2 1024 KiB, 16-fach, 64 B/Zeile, 1024 Sets | 16 |
      | `max` | L2 2048 KiB, 16-fach, 64 B/Zeile, 2048 Sets | 32 |

      Die Werte **unterscheiden sich zwischen den Modellen** — das ist der eigentliche Beleg, dass
      wirklich `CCSIDR_EL1` gelesen wird und keine Konstante zurückkommt. Die ARM-Modelle melden
      nur einen L2 als höchste Ebene, daher 16 statt 256 Farben wie auf x86.
- [x] **B-2.2b erledigt (2026-07-29, `02a1407`).** Die Feldzerlegung liegt jetzt als **reine
      Funktion** in `crates/caprock-hal/src/cache_decode.rs`, arch-neutral und ohne Hardware —
      beide Layouts werden auf dem Host gegen eingespeiste Registerwerte geprüft (5 von 5 grün,
      0,00 s). Damit ist der CCIDX-Zweig belegt, obwohl **keine** verfügbare QEMU-CPU ihn meldet.
      Das war der Punkt: bei gesetztem CCIDX stehen Assoziativität und Setzahl an **anderen
      Bitpositionen**, und wer sie falsch liest, bekommt eine plausible, aber falsche Farbanzahl.
      Ein Fehler, den kein Lauf auf dieser Maschine je gezeigt hätte. Technik wie bei `hal::dmar`:
      reine Funktion über eingespeiste Daten.
- [x] **B-2.3 erledigt (2026-07-29). `README.md` neu geschrieben.** Sie beschrieb einen
      aarch64-Kernel der Phase 7 — kein Wort vom x86-Port, von VT-d, von der Kern-Übergabe, vom
      Feature `selftest`. Bei einem Open-Source-Projekt die teuerste veraltete Datei überhaupt.
      Jetzt drin: beide Architekturen, das Zielbild (Basissystem statt Hypervisor), die **Regel,
      dass TrustedSAS nie Kundencode trägt**, der Lauf aus einem frischen Klon, das Feature-Gating
      — und ein Abschnitt „was fehlt", der nichts beschönigt. Beim Schreiben zwei Falschaussagen
      des Entwurfs gefunden und korrigiert: „kein einziges `cfg(target_arch)` im Kern" (es sind 48
      ausserhalb von `kernel/src/arch/`, nachgezählt) und „`virtio` ist aarch64-only" (der
      *DMA-Nachweis* ist es, die Geräteerkennung läuft auf beiden).
- [x] **B-2.4 erledigt (2026-07-29). `docs/verification.md`** führte die Concurrency-Modellprüfung
      als offen, obwohl Loom Stufe 2 seit `c2116ac` existiert. Jetzt abgehakt — **mit der Grenze
      danebengeschrieben**, und die ist der eigentliche Ertrag: Loom modelliert eine *Kopie* des
      Algorithmus, ein Fehler in der `cfg`-**Auswahl** ist für jedes Modell unsichtbar, weil das
      Modell den ausgewählten Code gar nicht sieht. Genau dort lag B-1.1.

## B-3. Geräte-Zuteilung, die man einem Tenant geben darf (E 3b/4)

- [x] **B-3.1 erledigt (2026-07-30).** Warteschlange (256 x 16 B), Wait-Deskriptor mit
      Statusschreibung an jedem Auftrag — `IQT` allein hiesse nur „eingereiht", nicht
      „durchgefuehrt". Kontext-Cache und IOTLB **umgestellt, nicht zusaetzlich**: sobald
      `GSTS.QIES` steht, verbietet die Architektur den Registerpfad (VT-d 6.5.2). Belegt:
      `qi : ALL PASS` — aktiv, Kontext-Cache quittiert, **Interrupt-Entry-Cache invalidiert**.
      Offen als Teil von B-3.3: QI laeuft nur auf Einheit 0. *(Alter Text:)* **B-3.1 Queued Invalidation.** Vorbedingung, nicht Alternative: die Invalidierung des
      Interrupt-Entry-Cache existiert **nur** als QI-Deskriptor. Der Registerpfad ist ein
      Provisorium mit bekanntem Ablaufdatum — beim Umstieg wirklich umstellen, nicht beides halten.
- [x] **B-3.2 erledigt (2026-07-31).** IRT mit lauter „not present" (Default-Block wie die
      Root-Tabelle): ein Geraet ohne IRTE kann keinen Interrupt ausloesen. Reihenfolge festgelegt:
      `IRTA` → `SIRTP` → **IEC invalidieren** → `IRE`; der mittlere Schritt geht nur ueber QI,
      deshalb scheitert `ir_enable` ohne QI, statt IR ohne Durchsetzungsmittel anzuschalten.
      **CFI ist Teil des Anschaltens**, keine Verschaerfung danach — `GSTS.CFIS == 0` wird als
      Gegenprobe abgenommen, nicht angenommen. Belegt: `ir : ALL PASS`, Lade-Suite `ALL PASS`
      rc=0 mit aktivem IR. *(Alter Text:)* **B-3.2 Interrupt Remapping + Compatibility-Format-Interrupts abschalten.** Ohne IR kann ein
      durchgereichtes Gerät beliebige Interrupt-Nachrichten erzeugen; IR mit weiter erlaubtem CFI
      ist eine offene Tür an der Seite. **Muss vor dem ersten Tenant-Gerät stehen** (Strang A-5.3
      hängt daran).
- [x] **B-3.3 erledigt (2026-08-02).** Die Notiz war zur Hälfte veraltet — und die andere Hälfte
      war schlimmer als beschrieben. Details in
      done.md.

      Schon aggregiert waren `caps_common()`, die Fault-Zähler und `discover`. **Nicht**
      aggregiert waren drei Stellen mit Zähnen: `init()` stellte **nur Einheit 0** scharf (die
      übrigen blieben mit `TE = 0` — das heißt nicht „blockiert", sondern **keine Übersetzung**),
      `invalidate_context_cache()` lief nur auf Einheit 0, und `flush_entry`/`slpt_map` trafen
      **Politik** (`clflush`, `SNP`) anhand der Fähigkeiten von Einheit 0.

      Dazu neu: eine **Sprechprobe** je Einheit. Eine deklarierte, stumme Einheit trug vorher
      genauso zu „keine Faults" bei wie eine fehlerfreie.

      **Offen geblieben (B-3.2-Rest, nicht B-3.3):** QI und IR laufen weiterhin nur auf Einheit 0.
      Für die Übersetzung folgenlos (die übrigen fahren den Registerpfad), **für Interrupt
      Remapping nicht**: hinter Einheit 1..n bleibt der nicht-remappte Nachrichtenpfad offen.
- [x] **B-3.4 erledigt (2026-08-02).** `0xFEE0_0000–0xFEEF_FFFF` ist als IOVA unbenutzbar (VT-d
      behandelt DMA dorthin als Interrupt-Nachricht und befragt die Übersetzung **gar nicht**).
      Die Fensterbasis liegt jetzt oberhalb dieses Bereichs — **strukturell**, nicht als Prüfung
      bei jeder Vergabe: eine Bedingung, die nicht gelten *kann*, ist besser als eine, die an
      jeder Vergabestelle richtig geprüft werden muss. Der Preis sind ein paar GiB ungenutzter
      IOVA-Raum von 39 Bit; das ist kein Preis.
      Der Bereich kommt aus der HAL (`iommu::interrupt_message_window`), nicht aus einer
      `cfg`-Verzweigung beim Aufrufer; auf aarch64 liefert sie `None`, und das ist eine **Zusage**
      („jede IOVA wird übersetzt"), keine Unkenntnis.
      Belegt: `dmawin : ... kein Kontext-Fenster im Interrupt-Nachrichtenbereich=1`, und in der
      Gegenprobe ohne das Überspringen `=0` mit `dmawin : FAILURES` — das Fenster lag vorher
      **wirklich** darin, es war kein theoretisches Loch.

## B-4. Isolation, die für fremde Tenants trägt (A1-Rest, Z1, Z6)

- [ ] **B-4.1 Der gefärbte isolierte Pfad wird der Normalfall** (Z1). Heute ist `spawn_isolated`
      regulär und ungefärbt, `spawn_isolated_colored` die Ausnahme. Dazu gehört die Entscheidung
      über die Regionsgröße: Färbung verträgt sich nicht mit dem 2-MiB-Blockdeskriptor (512 Seiten
      überstreichen alle 256 gemessenen Farben) — also kleinere Regionen mit seitenweisem Mapping
      oder mehrere gefärbte Läufe je PD. **(Strang A ruft das auf: A-2.1.)**
- [x] **B-4.2 erledigt (2026-07-30).** Geführte Streifenbelegung statt `i % PARTITIONS`.
      `caprock_mem::pick_free` (rein, host-getestet: 18 von 18, davon fünf neue — darunter
      „erschöpft ergibt `None` und ausdrücklich nicht wieder Streifen 0") liegt neben `stripe`,
      nicht im Kernel: eine zweite Fassung derselben Arithmetik bestätigt am Ende nur sich selbst.
      Im Kernel führen `claim_stripe`/`release_stripe` die Belegung über eine CAS-Schleife.
      **Der Streifen hängt an der VSpace, nicht am Thread** — er gehört dem Adressraum (Region,
      Kernel-Stack und Seitentabellen stammen daraus); am Thread aufgehängt würde er bei mehreren
      Threads je PD mehrfach oder gar nicht freigegeben. `vspace_teardown` gibt ihn zurück, und
      zwar **nach** der Slot-Freigabe: umgekehrt könnte eine neue PD ihn belegen, während die alte
      noch steht — dieselbe Überschneidung, nur in einem schmalen Fenster.
      `spawn_isolated_colored_auto` ist der Einstieg, den B-4.1 zum Normalfall macht; kein Streifen
      frei heißt, die PD entsteht **nicht**. Bewusst **keine** ungefärbte Rückfallebene: eine
      Trennung, die unter Last leise verschwindet, ist schlimmer als keine, weil dann niemand mehr
      weiß, welche PD getrennt ist.
      Belegt auf der Maschine (`stripe : ALL PASS`): vier Streifen vergeben, **der fünfte Versuch
      abgewiesen**, nach Freigabe wieder vergebbar. Steht in `all_done()`, nicht bloß im Bericht.
      **(Berührt A-3.4: dynamische Tabellen heben die PD-Zahl — vier Streifen bei beliebig vielen
      PDs heißt, die Färbung ist sofort erschöpft. Gehört gemeinsam entschieden.)**
- [ ] **B-4.3 SMT** (Z6). Cache-Coloring trennt den LLC und **prinzipiell nicht** L1/L2/TLB/
      Store-Buffer zwischen Geschwister-Hyperthreads. Entweder SMT aus, oder ein physischer Kern
      gehört zu jedem Zeitpunkt genau einem Tenant. Der zweite Weg braucht die CPU-Topologie
      (`CPUID.1F`/`0B`, MPIDR) im Scheduler — die liest heute niemand.
- [x] **B-4.4 erledigt (2026-07-29).** `docs/invariants.md` **§12** sagt jetzt, was A1 zusichert —
      und der längere Teil des Abschnitts ist die Liste dessen, was es **nicht** umfasst: L1/L2/
      TLB/Store-Buffer zwischen Geschwister-Hyperthreads (dagegen hilft keine Farbe, und zwar
      prinzipiell nicht), der ungefärbte `spawn_isolated` als Normalfall, stille
      Farbüberschneidung ab mehr PDs als Streifen (B-4.2), Zuteilung statt gemessener Wirkung, und
      `1` als kein Erfolgswert. Dazu die gesetzte Regel: **kein Kundencode in einer TrustedSAS-PD**,
      mit der Begründung aus dem Modell — intralinguale Sicherheit ist nur an Quellcode prüfbar,
      nie an einem fremden Binary. Eine Isolationszusage, deren Grenzen man nicht kennt, wird im
      Betrieb überdehnt; deshalb steht die Grenze neben der Zusage, nicht in einer Fussnote.
- [~] **B-4.5 Wirkung statt nur Zuteilung** — der Prime+Probe steht (`colors::run_prime_probe`,
      arch-neutral, Meldung `pprobe`, seit 2026-08-02 von `test-qemu-x86.sh` auch **geprüft**).
      Was fehlt, ist **Blech** — und der Eintrag sagt jetzt, was für eine Maschine das sein muss.
      **Die Reproduzierbarkeits-Nebenbedingung ist erledigt (2026-08-02).** Details in
      done.md.
      Kurz, weil die alte Notiz die Ursache falsch benannte:
      * **Der Allokator war es nicht.** Der Aufbau war byte-identisch reproduzierbar (gleiche
        Physadressen, gleiche Fragmentzahl davor/danach), der Fragment-Höchststand lag bei **419
        von 1024**, und alle 800 Regionen waren nach dem Abbau nachweislich frei. Ein identischer
        Aufbau kann keine Streuung erzeugen — sie lag in der **Messung**.
      * **Der Angreifer lief linear.** Falle 1 („linear misst den Vorauslader") war nur fürs Opfer
        behoben; ein sequenzieller Strom wird von verdrängungsresistenter Cache-Einlagerung als
        „kein Wiedergebrauch" behandelt und verdrängt nichts. Positivkontrolle: linear **3 von
        10**, bit-umgekehrt **10 von 10**.
      * **Das Minimum ist für `shared`/`disjoint` der falsche Schätzer** — jede Störung, die die
        Verdrängung schwächt, macht die Messung *kürzer*, das Minimum greift also einseitig in die
        Richtung, die die Positivkontrolle zerstört. Minimum **4 von 10**, Median **10 von 10**.
        Gewertet wird der Median; gemeldet werden Minimum/Median/Maximum plus ein
        **Auflösungs-Gate** (überlappen die Verteilungen, gibt es kein Urteil, sondern SKIP).
      * **„Opfer > L2" war eine feste Zahl.** 2 MiB, begründet mit „die Messmaschine hat ~1,5 MiB
        L2" — auf der heutigen Maschine sind es 4 MiB, das Opfer lag also wieder privat. Die Größe
        kommt jetzt aus `hal::cache::below_llc()` (neu) und muss in ein Fenster passen.
      * Der Aufbau kommt aus **wenigen großen ungefärbten Blöcken**, in denen die farbreinen Läufe
        gesucht werden — 14 Allokationen statt 800, und die Farbe jeder benutzten Seite wird an der
        Verwendungsstelle nachgerechnet statt `alloc_colored` geglaubt. Nebenbei fällt damit weg,
        dass `MAX_ATTACKER` die Angreifergröße auf großen Maschinen still deckelte (auf der
        Messmaschine war der Deckel **exakt erreicht**).
      Nachher: 10 Boots mit identischer Ergebnissignatur, `disjunkt`-Median 174–271 statt 45–189;
      acht Aufbauten in einem Boot: Median 189–215 (±7 %). `RUNS=5 ./test-qemu-x86.sh` grün.
      **Offen (das eigentliche B-4.5): ein Lauf auf echter Hardware — und die Maschine muss eine
      Bedingung erfüllen:**
          Größe der nicht partitionierten Cache-Ebene   <   LLC / PARTITIONS
      Sonst gibt es **keine gültige Opfergröße**: unterhalb der privaten Ebene misst der Test
      diese Ebene (jeder Angreifer verdrängt dort, Farbe hin oder her), oberhalb des Farbanteils
      kann das Opfer auch bei perfekt wirkender Färbung nicht resident bleiben. Auf der heutigen
      Messmaschine ist das Fenster **nicht** leer, sobald QEMU die echte Geometrie meldet: mit
      `host-cache-info=on` (seit 2026-08-02 in beiden x86-Suiten) sind es 2 MiB private Ebene
      gegen 24 MiB / 4 = 6 MiB Farbanteil. Ohne den Schalter meldet QEMU seine Legacy-Deskriptoren
      (L3 16 MiB, L2 4 MiB), und das Fenster war **leer** -- der SKIP-Grund war dann ein
      Aufbau-Artefakt statt der Sache. Jetzt nennt der Test den strukturellen Grund (Gast). Ein Xeon mit 1–2 MiB L2 und
      32+ MiB LLC erfüllt die Bedingung; ein hybrider Notebook-Kern mit 4 MiB E-Core-L2 nicht.

      Das ist zugleich ein Befund **über A1 selbst** und gehört neben `docs/invariants.md` §12:
      wo eine nicht partitionierte Cache-Ebene so groß ist wie ein ganzer Farbanteil des LLC, kann
      Färbung mit dieser Streifenzahl nichts schützen, was nicht ohnehin privat zwischengespeichert
      ist. Das ist keine Eigenschaft des Tests, sondern eine der Maschine.

      **Unter einem Hypervisor bleibt die Frage nicht entscheidbar** (ein Gast färbt gastphysische
      Adressen; die zweite Übersetzungsstufe bildet jede 4-KiB-Seite auf eine beliebige Wirtsseite
      ab, und die Farbbits liegen oberhalb des Seitenoffsets). **Gemessen wird trotzdem** — Aufbau,
      Positivkontrolle, Farbwahl und Bilanz sind dort genauso prüfbar wie auf Blech, und ein
      Prüfpfad, der zum ersten Mal am Zieltag läuft, ist am Zieltag kaputt. Das Urteil über A1
      unterbleibt, die Zahlen stehen im Log.

## B-5. Zeit, Abrechnung, Speicherorte (Z2, Z5, Z8)

- [x] **B-5.1 erledigt (2026-08-02).** Der Verbrauch wird bei **jeder Umplanung** gestempelt, nicht
      mehr beim Tick. Details in done.md.

      Der Zustand vorher war schlimmer als die Notiz vermutete: `on_tick` belastete nur bei
      `tick == true`, `block_current`/`switch_to`/`YIELD` **gar nicht**. Die Verzerrung war damit
      nicht „bis zu 10 ms", sondern **vollständig** — wer kurz vor dem Tick blockiert, zahlte
      **null**, und wer das systematisch tut, rechnet dauerhaft umsonst.

      Die Arithmetik liegt abhängigkeitsfrei in `crates/caprock-sched/src/cycles.rs` und wird auf
      dem Host geprüft (`tools/host-tests.sh cycles`, 9 Tests); die **Uhr** bleibt beim Kernel.
      Drei Fallen sind benannt und einzeln getestet: Rückwärtssprung wird **verworfen statt
      gewrappt** (`wrapping_sub` hätte aus 20 ns Messfehler ein für immer erschöpftes Konto
      gemacht), unplausible Differenzen fliegen raus, und ein Kernwechsel wird **vor** der Zahl
      geprüft — ein Zyklenzähler ist nur innerhalb eines Kerns eine Zeitachse. Vorgabe ist
      `Source::Untrusted`: ohne zugesicherte Invarianz wird **nichts** abgerechnet.

      Im Kernel klammert `charged()` jede Umplanung — mit **einem** Zählerstand, nicht zwei: zwei
      Lesungen ließen die Zyklen der Umplanung selbst zwischen den Stempeln liegen, und die Summe
      aller Konten wäre systematisch kleiner als die verstrichene Zeit.

      Geprüft wird es als `cycacct` (63 Proben gegen 39 Ticks). **Welcher Zweig gilt, sagt die
      Maschine** (`invariant_tsc()`), nicht der Kernel — der erste Entwurf las nur den
      Ablehnungszähler und hätte ein vergessenes `set_cycle_source` bestanden.

      **Offen bleibt** die Monitoring-Cap: `consumed_cycles(tid)` liefert die Zahl, aber es gibt
      noch keine Cap, über die ein Mandant sie abfragen kann. Das gehört zu B-6.1.
- [ ] **B-5.2 Tickless für Rechenkerne** (Z5): Timer nur armieren, wenn es etwas zu verdrängen
      gibt. Zusammen mit B-5.1 werden Abrechnung und Verdrängung entkoppelt — das ist der Punkt.
- [ ] **B-5.3 Kern-Isolierung als Politik:** keine IPIs, keine Balancierung, keine fremde
      Interrupt-Zustellung auf Rechenkernen.
- [ ] **B-5.4 NUMA** (Z8): Knoten aus ACPI SRAT/SLIT, Freilisten je Knoten, PD-Zuteilung
      knotenlokal. **Gemeinsam mit der Farbvergabe entscheiden**, nicht in zwei Schichten — sonst
      kämpfen beide Politiken um dieselbe Physadresse und wer zuerst zuteilt, gewinnt.
- [x] **B-5.5 erledigt (2026-08-02).** Die CDT-Läufe sind begrenzt — und der Befund dabei war,
      dass es **ausgerechnet umgekehrt** stand. Details in
      done.md.

      `audit_cdt` war gegen einen zyklischen CDT geschützt (`steps > nslots`), `revoke`,
      `move_cap` und `child_count` **nicht**. Der Prüfer läuft auf Anforderung, `revoke` auf
      **Mandantenwunsch** — und unter der CAPS-Sperre. Aus einer Datenstrukturanomalie wäre damit
      kein Latenzproblem geworden, sondern ein stehender Knoten.

      Die Schranke ist **hergeleitet**, nicht gegriffen: ein azyklischer Lauf besucht keinen Slot
      zweimal, also `slots.len()` — dieselbe Zahl, die `audit_cdt` schon nahm, jetzt aus **einer**
      Quelle. Ein Überlauf bricht ab und wird **gezählt**; der Kernel prüft ihn über den neuen
      `cdt_audit`-Code **9**, und die Höchststände stehen als **Operationszahl** im Bericht
      (`cdtlen : Abstieg 1/80256, Revoke 4/80256`). Zeit wäre das falsche Maß — sie hängt an
      Taktrate und Emulation, nicht an der Struktur.

      Sensitivität: Schranke gelockert → 1 Test fällt; Schranke **entfernt** → der Test „terminiert
      auf einem Zyklus" **hängt** und musste nach 60 s abgebrochen werden. Dass er zurückkehrt,
      ist das Ergebnis.

      **Nebenertrag:** `caprock-cap` hatte **keinen** Host-Test-Pfad — seine Tests liefen
      nirgends. `tools/host-tests.sh` sammelt jetzt alle reinen Crates (mem, part, fat, cap):
      **62 Tests**, `ALL PASS`.

- [ ] **B-6.1 Messbarer Boot + Attestierung.** Der Kunde will wissen, **worauf** er läuft — nicht
      beweisen, was er mitbringt (TrustedSAS ist nur für eigenen Code, s. Z1). Messkette bis in
      den Kernel, Signatur über die Messung. Vorbedingung für alles, was Tenant-Zustand über das
      Netz bewegt.
- [x] **B-6.2 erledigt (2026-08-02) — und die Prämisse des Eintrags war falsch.** Die Festlegung
      steht in [docs/fehlerdomaene.md](docs/fehlerdomaene.md) (betreiberseitig, mit Messvorschrift
      und Nicht-Zusicherungen), als Invariante §14 in [docs/invariants.md](docs/invariants.md).
      Details in done.md.

      **Zugesichert:** der Knoten ist die Fehlerdomäne; Redundanz über Knoten. Zusätzlich: alle PDs
      im **globalen SAS-Adressraum** (`VSPACE_OF == 0`) bilden untereinander **eine** Domäne —
      Trennung intralingual statt hardwareseitig, und das ist der Grund für „kein Kundencode in
      TrustedSAS", nicht seine nachträgliche Begründung. **Nicht** am Domänen-Etikett festmachen:
      `Domain::TrustedSas` darf global *oder* isoliert laufen, extern geladene bekommen heute immer
      eine eigene VSpace. Eingegrenzt ist und bleibt der Fault einer **isolierten** PD.

      **Der Eintrag sagte „ein Panic reißt heute den Knoten mit". Gemessen stimmt das nicht — und
      das ist die schlechtere Nachricht.** `panic.rs` ruft ein `halt()`, das die Interrupts nicht
      maskiert (`arch/x86_64/mod.rs:294` `loop { hlt }` ohne `cli`; aarch64 `loop { wfe }`, DAIF
      unverändert), also holt der nächste Timer-Tick den Kern zurück in den Scheduler. Vier
      verschiedene Ausgänge für dieselbe Ursache — welcher eintritt, hängt davon ab, **wo** der
      Panic auftrat, nicht wie schlimm er war:
      * Panic in einem Kernelfaden → der Knoten läuft **61 s weiter**, alle 4 Kerne ticken, nur der
        gepanickte Faden steht (`Worker-Runden [4908, 4910, 2]`). Auf aarch64 dasselbe.
      * Panic auf einem Sekundärkern → Prüfsignatur **identisch** zum sauberen Lauf, `rc=0`.
        Ohne die Konsolenzeile wäre der Panic durch nichts nachweisbar.
      * Panic unter gehaltener `MEM`-Sperre → **stiller Totalausfall**, kein Watchdog, `rc=124`
        (der Ticket-Lock in `caprock-sync` dreht unbegrenzt).
      * Panic im Steuerfaden des Bootkerns → Knoten läuft, meldet aber nie wieder etwas — von außen
        **nicht** von einem Deadlock zu unterscheiden. Das betrifft die Diagnose von D0.

      **Nicht gebaut, absichtlich** (Aufwände und Begründung in `docs/fehlerdomaene.md` §6): IRQs im
      Panic-Pfad maskieren (Minuten), Rekursionswächter im Panic-Handler (Minuten, heute gibt es
      **keinen** — gemessen 362 Ebenen ohne `#DF`, ohne Schutzseite), `panic` → `system_off`
      (~1 h). Die drei zusammen machen die Festlegung wahr, statt sie zu behaupten — aber das ist
      eine **Entscheidung** (Verfügbarkeit gegen Ehrlichkeit), keine Reparatur, und gehört Simon.

## B-7. Verifikation (D1–D5)

- [x] **B-7.1 erledigt (2026-08-02) — die Notiz war überholt, die Lücke lag woanders.**
      Details in done.md.

      `tools/kani-verify.sh` hatte `sync` längst als Ziel, und die CI ruft es **ohne Argumente**,
      also mit allen vier Zielen. Woher die Notiz kam, ist trotzdem sichtbar: der CI-Job hiess
      *„Kani — Tier-1-Beweise (Loader-Parser)"*. Wer die CI liest statt das Skript, musste
      schliessen, `sync` sei ungegatet. **Beschriftung korrigiert** — dieselbe Fehlerform wie
      B-7.2, nur eine Ebene höher: eine Beschreibung, die neben der Sache herläuft.

      **Die echte Lücke:** die drei vorhandenen Beweise liefen mit **konkreten** Werten (ein Leser,
      zwei Leser, ein Schreiber) — also in derselben Grössenordnung, die Loom seit B-7.2 über
      Interleavings abdeckt. Über den Zustandsraum, in dem die Zusicherungen leben (31 Bit
      Leserzahl neben dem Schreiberbit, überlaufende u32-Ticketzähler), sagte **keines** der beiden
      Werkzeuge etwas. Fünf neue Harnesses nehmen den Zustand jetzt **symbolisch** und rufen dabei
      den echten Code. `sync` steht damit bei 8 statt 3 Beweisen.

      Sensitivität am echten Lock gemessen (vier Mutationen, je andere Zahl fallender Beweise);
      selbst nachgemessen: `store(0)` statt `fetch_and(!RW_WRITER)` → **7 verified, 1 failure**.

      **Der Inert-Check der CI deckte nur `caprock-loader` ab** — jetzt auch `caprock-sync`, die
      Crate mit **zwei** externen cfgs (`kani` *und* `loom`). Der Kernel-Build fing das mit ab, aber
      nicht als benannte Zusicherung, und ein Schutz, den niemand ausspricht, fällt beim nächsten
      Umbau unbemerkt weg.

      *(Alter Text:)* **B-7.1** Kani läuft nur im CI-Gate; die ext-29-Änderung an `caprock-sync` ist dort nicht
      gegengeprüft — **und genau diese Crate hatte gerade den x86-IRQ-Fehler.**
- [x] **B-7.2 erledigt (2026-08-02).** Die Kopie ist weg: `tools/loom-verify.sh` übernimmt
      `crates/caprock-sync/src/lib.rs` **unverändert**, die Beweise stehen in derselben Datei.
      `Verification/concurrency/loom/src/{lib,ticket}.rs` sind **gelöscht** — eine tote Kopie, die
      autoritativ aussieht, ist schlimmer als keine. Details in
      done.md.

      **Der Fund, der zählt:** eine abgeschwächte Speicherordnung im Ticket-Release
      (`Release` → `Relaxed`) lief durch **alle** Beweise. Ursache war nicht die Kopie, sondern
      `core::cell::UnsafeCell` — damit prüfte Loom nur das Atomic-Protokoll, nicht die
      Veröffentlichung der Nutzlast. Erst mit `loom::cell::UnsafeCell` fallen 2 von 10. Selbst
      nachgemessen am echten Lock: `8 passed; 2 failed`, danach wieder `10 passed`.

      **Die IRQ-Maskierung bleibt außerhalb** — und steht jetzt als benannte Grenze im Modulkopf,
      im Skriptkopf und neben `IRQ_MASKING_IMPLEMENTED`. Der Punkt aus dem alten Text gilt
      unverändert (s. u.): der Fehler von B-1.1 lag genau dort, wo das Modell nicht hinsieht. Was
      ihn heute hält, ist die Übersetzungszeit-Zusicherung `target_os = "none"`, nicht Loom.

      *(Alter Text:)* **B-7.2** Loom modelliert eine *Kopie* des Lock-Algorithmus; die IRQ-Maskierung ist dort
      prinzipiell nicht modellierbar. **Nach B-1.1 ist das keine Randnotiz mehr:** der Fehler lag
      exakt in dem Teil, den das Modell nicht abbildet. Ein Test, der die reale `cfg`-Auswahl
      prüft (baut das Kernel-Ziel wirklich den maskierenden Zweig?), wäre wirksamer als ein
      feineres Modell.
- [ ] **B-7.3** *(Rest)* Verus **Scheduler/IPC vertiefen**. `delete_leaf` + Kinderlisten-
      Erreichbarkeit sind **zu** (2026-08-03, s. `done.md`): `cap_space.rs` steht auf
      `18 verified, 0 errors` (3,0 s), der Erreichbarkeitssatz `unreachable_after_delete` hat
      **beide** Hälften (keine Kante zeigt mehr auf den gelöschten Slot **und** kein fremder Elter
      hat sich bewegt), 8 von 9 Mutationen fallen — die neunte ist eine **bewiesene** Redundanz,
      keine Lücke. Neu dabei: `tools/verus-modelltreue.sh` (Wächter mit Selbsttest, hält das
      Modell an `crates/caprock-cap/src/space.rs`).

      **Was für Scheduler/IPC noch offen ist:** `Verification/scheduler/proofs/runqueue.rs`
      (13 verified) und `Verification/ipc/proofs/endpoint.rs` (6 verified) sind grün, aber ihr
      Bezug zum echten Quelltext ist **nicht** geprüft — der Modell-Treue-Wächter deckt heute nur
      `unlink`/`delete_leaf` ab. Vor jeder Vertiefung dort zuerst die Entsprechung festziehen,
      sonst wächst dieselbe stille Drift an zwei weiteren Stellen.

      **Und der Grund, warum B-7.3 überhaupt so lange offen aussah, war keiner:** der `delete`-
      Beweis galt seit `3384abb` (2026-06-27) als erbracht — Commit-Titel, README-Status und
      CI-Gate sagten das. Gemessen war er `9 verified, 1 ERRORS`, und zwar **auch gegen die damals
      gepinnte Verus-Fassung**. 37 Tage rotes Gate, das niemand gelesen hat.
- [ ] **B-7.4** Die ext-29-/ext-30-Invarianten haben Laufzeittests, aber keine Beweise. Für die
      Migration wäre die Sperrordnung „aufsteigende Kern-ID" ein lohnendes Loom-Modell.
