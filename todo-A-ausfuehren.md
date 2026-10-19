# Strang A — Ausführen und Austauschen

**Frage, die dieser Strang beantwortet:** Wie bekommt der Kernel fremden Code zum Laufen, und wie
tauscht man ihn im Betrieb aus, ohne dass IPC-Beziehungen reißen?

Gegenstück: [todo-B-verlaesslichkeit.md](todo-B-verlaesslichkeit.md). Gemeinsame Grundlage:
[TODO.md](TODO.md) (Abschnitt Z) und [docs/plan-betriebsbereit.md](docs/plan-betriebsbereit.md).
Koordinationsregeln am Ende dieser Datei — **vor der ersten Änderung lesen.**

Dieser Strang ist der **kritische Pfad**. Solange er nicht steht, hat der Kernel keinen
Nicht-Test-Zweck: schaltet man das Feature `selftest` ab, bootet er und geht in eine Leerschleife.
Alles, was Strang B härtet, härtet bis dahin ein System ohne Anwendung.

---

## A-1. Startmenge und Manifest (Z11a, Z11b, C6)

Das Boot-Image enthält genau zwei Dinge: den Kernel und **eine** Datei, die festlegt, was geladen
wird. Alles andere liegt außerhalb.

- [x] **A-1.1 Multiboot-Module lesen.** GRUB übergibt die Startmenge als Multiboot-Module; der
      Kernel muss die Modulliste aus der Multiboot-Info auswerten (Feld `mods_count`/`mods_addr`,
      Flag Bit 3). Auf ARM gibt es das Gegenstück bereits über `-device loader` + das reservierte
      RAM-Fenster; die x86-Seite fehlt vollständig. **Achtung:** die Modulbereiche müssen dem
      `PhysAllocator` als belegt gemeldet werden, bevor irgendetwas alloziert — sonst überschreibt
      die erste Allokation das Modul, das gleich geladen werden soll.
      → `kernel/src/arch/x86_64/multiboot.rs`. Die Bereiche werden **ausgeschnitten**, nicht
      nachträglich markiert: was nie als frei gemeldet wurde, kann nicht vergeben werden. Die
      Grenzfälle (Rand, Überlappung, unsortierte Eingabe, Vollabdeckung) sieht der reale Lauf nie,
      also werden sie eingespeist (`mbmod : ALL PASS`). Die Archivquelle ist nicht mehr fest
      verdrahtet — `loader::set_archive_span`, auf ARM bleibt das statische Fenster der Rückfall.
- [x] **A-1.2 Manifestformat.** Ein Eintrag beschreibt: Name, erwarteter SHA-256 des Moduls,
      Domäne (TrustedSAS/HardwareLand/UserLand), Schnittstellenversion, Anfangs-Caps, und die
      Politikfelder aus [A-1.4](#a-14-politikfelder-schnittstelle-zu-strang-b). Format bewusst
      **einfach und selbstbegrenzend** (feste Feldbreiten, Längenpräfixe) — es wird vor jeder
      Signaturprüfung geparst, ist also Angriffsfläche. Kein TOML, kein JSON im Kernel.
      → `crates/caprock-loader/src/manifest.rs` (80-B-Kopf, 96-B-Einträge, `entry_len` im Kopf,
      damit ein fremdes Eintragsformat **erkannt** statt verrutscht gelesen wird). Host-getestet
      inkl. Mutations-Durchlauf; Kani-Beweise für Crash-Freiheit und exakte Partitionierung.
- [x] **A-1.3 Manifest ist ein Autoritätsdokument.** Es legt die gesamte Anfangsverteilung von
      Autorität fest. Also: signiert, und die Signatur **an das Kernel-Image gebunden** (Hash des
      Images geht in die signierte Nachricht ein). Wer die Datei tauschen kann, besitzt sonst die
      Maschine. Baut auf ADR 0014 auf, ist aber nicht dasselbe: dort werden Binaries zertifiziert,
      hier die Zuteilung. **Prüfreihenfolge:** Signatur zuerst, Inhalt danach — nie umgekehrt.
      → Eigene Schlüsselmenge (`kernel/src/manifest_keys.rs`, `tools/gen_manifest_key.py`),
      getrennt von `trusted_keys.rs`. Bindung = SHA-256 über `[__text_start, __rodata_end)`;
      Werkzeug-Gegenstück `tools/kernel_hash.py`. **Die Prüfreihenfolge trägt der Typ:**
      `SystemManifest::parse` liefert keinen einzigen Eintrag, Einträge gibt es nur über
      `Verified`, und das entsteht nur aus `verify_with`.
- [x] **A-1.4 Politikfelder** <a id="a-14-politikfelder-schnittstelle-zu-strang-b"></a>
      **(Schnittstelle zu Strang B).** Je Komponente: exklusiver Farbstreifen ja/nein,
      NUMA-Knoten, Kern-Affinität, Priorität, Budget. Strang A besitzt das **Format**, Strang B die
      **Bedeutung** (was ein Streifen ist, wie NUMA vergeben wird). Die konkrete Farbe gehört
      **nicht** ins Manifest — sie ist maschinenlokal.
      → Format steht (`policy_flags`, `numa_node`, `core_affinity`, `priority`, `budget_us`); der
      Kernel liest und weist sie aus.
      **Nachtrag 2026-07-30: „noch nicht angewandt" ist kein neutraler Zustand.** Wer
      `POLICY_EXCLUSIVE_STRIPE` in ein signiertes Dokument schreibt, glaubt danach an eine
      Cache-Trennung, die niemand herstellt — das ist schlimmer als gar kein Feld. Der Kernel
      **weist ein solches Manifest jetzt ab** (`UnsupportedPolicy`), genau wie A-2.1 es bei
      `CAP_PD_CONTROL` tut: lieber ablehnen als weniger geben, ohne es zu sagen.
      Warum es heute nicht einhaltbar ist: `alloc_colored` weist mehr als `MASK_BITS` Seiten ab,
      ein Streifen trägt `MASK_BITS/PARTITIONS` Seiten (64 KiB), und Segmente/Stack eines
      geladenen Programms kommen physisch **zusammenhängend** aus `mem_alloc`. Nötig wäre
      stückweise Allokation aus demselben Streifen mit seitenweisem Mapping, plus eine
      Freigabe-Buchhaltung, die mitwächst (`seglist`, `MAX_IMG_SEGS`). **Teilweise gefärbt ist
      nicht gefärbt** — deshalb ablehnen statt halb liefern.
      `POLICY_NO_HOTRELOAD` **wird durchgesetzt** (s. A-4.5).
- [x] **A-1.5 `SYS_LOAD` auf x86 zum Laufen bringen.** Scheitert heute sauber, weil es nichts zu
      laden gibt. Der ELF-Lader existiert (`caprock-loader`); es fehlt die Quelle.
      → Loader baut auf beiden Architekturen, `load_by_index` ist kein `None`-Stub mehr.
      Nebenbefund: der ELF-Parser kannte nur `EM_AARCH64` — ein x86-Binary war für ihn schlicht
      kein ELF. Jetzt `EXPECTED_MACHINE` per `cfg(target_arch)`, mit einem Test gegen die jeweils
      **andere** Architektur.

## A-2. Root-Task (F2)

- [x] **A-2.1 Ein Startprogramm aus der Startmenge laden und ihm die Wurzel-Caps übergeben.** Der
      seL4-Weg. Ab hier ist jede weitere Fähigkeit ein Userland-Programm statt eines
      Kernel-Patches — das ist der eigentliche Hebel des ganzen Plans.
      → `loader::start_root_task` + `programs/trusted/init`. Der Root-Task signalisiert, dass er
      läuft, und lädt dann **über seine eigene Loader-Cap** die restliche Startmenge nach; erst das
      zweite Badge belegt die Aussage, um die es geht. Boot-Argument ist bewusst das Minimum
      (`(Anzahl << 32) | eigener Index`) — alles Weitere gehört hinter eine Capability, nicht in
      ein Register.
      **Offen und benannt:** `CAP_PD_CONTROL` ist nicht erteilbar, weil eine PdControl-Cap eine
      Ziel-PD bezeichnet, die beim Start des Root-Tasks noch nicht existiert. Der Kernel weist ein
      Manifest, das sie verlangt, **ab** (`UnsupportedAuthority`) statt still weniger zu geben. Der
      ehrliche Weg wäre, dass `SYS_LOAD` die PdControl-Cap der neu erzeugten PD zurückgibt — das
      ist eine ABI-Erweiterung und gehört zu A-3.2 (wählbarer Empfangs-Slot).
- [x] **A-2.2 erledigt (2026-07-30). `default = []`.** Vorher wäre das Gating kein schlankerer
      Kernel gewesen, sondern ein leerer; seit A-2.1 hat der Vorgabebau einen Zweck
      (`start_root_task_reported()` liegt ausserhalb jedes Features). Beide Suiten fordern
      `--features selftest` für den gebooteten Bau **ausdrücklich** an, nicht über `default` —
      sonst wären sie beim Dreh still geworden statt rot, und Stille sieht wie Erfolg aus.
      Belegt von B: volle x86-Suite mit dem Dreh im Baum, **einziger FAIL `x2APIC`** (unverändert
      gegenüber dem Lauf davor), beide F1-Prüfungen mit gedrehten Vorzeichen (`.text` ohne Feature
      `0x24000`, mit `0x38000`), und die Lade-Suite `== ALL PASS ==` inklusive aller drei
      Negativfälle. Details in AGENTS.md, Mitteilung 6.

## A-3. Caps, die man wieder loswird (A4, A3, C3)

- [x] **A-3.1 `SYS_CDELETE`** (eigener Slot, cap-gegatet auf den eigenen Cspace). Ohne das kann
      ein langlebiger Dienst, der Caps per IPC empfängt, seine Slots nicht freigeben und läuft
      gegen `CAP_BUDGET_PER_PD`. Der Root-Task ist genau so ein Dienst: mit A-2 wird aus der
      ABI-Lücke ein Betriebsproblem.
      → Syscall 14, **ohne** zusätzliche Cap: Autorität abzugeben darf nie an einer Erlaubnis
      hängen. Reihenfolge Slot räumen → löschen → bei Misserfolg zurücklegen; ein Cap mit
      CDT-Kindern bleibt unverändert liegen (`ERR_HASCHILDREN`) statt halb entfernt zu werden.
      Geprüft aus **Ring 3** (im `init`), und zwar **beide** Ausgänge: der erfolgreiche (und die
      Autorität ist danach wirklich weg — ein weiteres `SYS_LOAD` wird abgewiesen) und der
      abgelehnte (der Cap bleibt benutzbar).
- [x] **A-3.2 `SYS_CMOVE`/`SYS_CCOPY`** und ein **wählbarer Empfangs-Slot** für Grants statt des
      festen `GRANT_RECV_SLOT`.
      → Syscalls 15/16/17. `CCOPY` schneidet die Rechte mit denen des Originals (eine Kopie darf nie
      mehr können als die Vorlage) und nimmt ein **Badge**; `CMOVE` überschreibt ein belegtes Ziel
      **nicht** (das wäre ein Cap-Verlust, den niemand angeordnet hat — wer räumen will, ruft
      `CDELETE`); `SETRECV` legt den Empfangs-Slot **je PD** fest, und zwar beim **Empfänger**:
      dürfte der Sender ihn wählen, könnte ein Server jeden Cap seines Clients verdrängen — eine
      Schreiboperation in fremdes Eigentum, verkleidet als Antwort.
      **Warum das Badge dazugehört und nicht Beiwerk ist:** `SYS_SIGNAL` verodert das Badge der
      benutzten **Cap** in `pending`; das Nachrichtenwort spielt keine Rolle. Ohne badgbare Kopien
      kann ein Programm dem Kernel genau **eine** Tatsache melden. Dasselbe gilt beim Weiterreichen,
      deshalb hat `SYS_LOAD` jetzt ein Badge-Argument (`x4`). Erlaubt ist beides, weil der Aufrufer
      das Objekt bereits besitzt — er vergibt ein Etikett auf eigener Autorität, er erwirbt keine.
      **Nicht erledigt:** `SYS_LOAD` gibt weiterhin keine PdControl-Cap der neu erzeugten PD zurück
      (s. A-2.1). Der wählbare Empfangs-Slot ist die Vorbedingung dafür; die Rückgabe selbst steht
      noch aus.
- [x] **A-3.3 `ReplyFinal` vom Kernelstack lösen.** Hält heute ein `[(u32,u64); NOBJECTS]`-Array
      auf dem 2-KiB-Kernelstack; `NOBJECTS` hochzuziehen koppelt an die Stackgröße. Vorbedingung
      von A-3.4.
      → `Finalized` (der heutige Name) leiht seinen Speicher jetzt vom Aufrufer statt ihn zu
      besitzen; der Kernel hält **einen** statischen Puffer (`FinalizeBuf`, äusserste Sperre).
      Kommen die Tabellen in A-3.4 aus dem RAM, kommt der Puffer aus derselben Quelle — **ohne
      Änderung an der Cap-Crate**. Es war mehr als das eine Array: `dma_finalize` hielt eine
      **Kopie** derselben Regionen (nur um daraus einen zusammenhängenden Slice zu machen), und
      **jede** `finalize`-Implementierung der Enforcer nochmals ein `[usize; 128]`.
      **Zwei Funde:** `Finalized::overflowed()` — daneben der Kommentar, sie stehe da, „damit «kann
      nicht vorkommen» prüfbar ist statt behauptet" — wurde von **niemandem** gerufen. Solange die
      Arrays im Typ lagen, war die Kapazität eine Typeigenschaft; jetzt ist sie eine Entscheidung
      des Aufrufers, also wird sie geprüft (Audit-Code 70, plus sofortige Meldung: ein Überlauf
      heisst, ein in `CALL` blockierter Aufrufer wird nie entblockt, und das sieht man hinterher
      an nichts mehr). Und die Schranke stand doppelt (`MAX_FINALIZE` im Kernel „dieselbe Schranke
      wie in der Cap-Crate") — eine Kopie, die beim Wachsen still auseinanderläuft.
- [x] **A-3.4 erledigt (2026-07-30/31), in vier Teilen + Abschluss.** Threads als Zusage
      (`TARGET_THREADS = 10_000`, vorher `cores * 256` — eine Eigenschaft des Testaufbaus);
      `CapSpace`, `PdTable`, Endpoints und Notifications aus dem Boot-RAM statt `.bss`
      (`Slab`-Muster aus ext-30). Gemessen: 80256 Slots/Objekte, 10000 PDs, 10064 Endpoints,
      10000 Thread-Slots, zusammen rund 42 MiB. **Der Fund, der A-3.4 begruendete, ist
      geschlossen:** `CAP_BUDGET_PER_PD` deckelte den Einzelverbrauch, die **Summe** prüfte
      niemand — 256 Slots gegen 2048 zugesagte, 32 PDs füllten die Tabelle. Jetzt wird die Summe
      geprüft (`d1f5ed8`). *(Alter Text:)* **A-3.4 Cap-/PD-/Endpoint-/Notification-Tabellen dynamisch.** Solange die Cap-Tabelle global
      und fest ist, bestimmt ein Tenant die Dichte aller anderen. **(Berührt Strang B:** die
      Streifenbuchhaltung aus A1 zählt PDs; wächst die PD-Zahl dynamisch, muss die
      Streifen-Freiliste das mitmachen.)

## A-4. Hot-Reload, der den Namen verdient (Z11d, Z11e, Z11f)

Auf ARM existiert der Fall (Phase 7: eine Server-PD wird über *dieselbe* Endpoint-Cap ersetzt).
Für einen Betriebsanspruch fehlen drei Dinge.

- [x] **A-4.1 erledigt (2026-07-31, `a25fa23`).** Prüfung und Tausch laufen unter *einem* Lock.
      Ohne vorherige Stilllegung (A-4.2) wird abgewiesen — der Befund „keine offene Transaktion"
      wäre sonst eine Momentaufnahme, die im nächsten Takt nicht mehr gilt. Ein fremder Empfänger
      blockiert den Tausch, statt still verdrängt zu werden; im überlappenden Fall ist durchgehend
      genau ein Empfänger gebunden. Belegt: `rebind : ALL PASS`.
- [x] **A-4.2 erledigt (2026-07-31, `4fca286`).** Vor A-4.1 gebaut, weil dessen ehrliche Variante
      den Begriff *ruhend* voraussetzt. Stilllegen weist **neue** Transaktionen mit
      `ERR_QUIESCING` ab (nicht `ERR_BADCAP`: „kommt gleich wieder" ist für den Client eine andere
      Lage als „gibt es nicht"), laufende dürfen abschließen; ein zweiter Austausch am selben
      Endpoint wird abgewiesen, doppelte Freigabe wird gemeldet. Belegt: `quiesce : ALL PASS`.
      Ursprüngliche Fassung: Ein Client, der in `CALL` blockiert, hält eine Reply-Cap auf den
      alten Server. Entweder der neue erbt die offenen Replys, oder der Austausch findet nur ohne
      offene Transaktion statt. Die zweite Variante ist ehrlich und für den Anfang wahrscheinlich
      richtig — sie braucht aber einen Begriff von „ruhend", den es heute nicht gibt.
      **Derselbe Begriff trägt später [Z4a](TODO.md#z4-checkpointrestore-of-a-thread)
      (Thread einfrieren).** Einmal bauen, zweimal benutzen — und deshalb hier gleich allgemein
      genug entwerfen.
- [x] **A-4.3 erledigt (2026-07-31).** Gewählt ist die **Region, die den Austausch überlebt** —
      und damit ist ihr Format eine ABI, also mit **versioniertem Kopf** (`state.rs` in
      `caprock-region`). Der Kopf trägt `state_version`, `program_id` und einen
      Übernahmezähler. **Der Punkt, an dem die Sache hängt:** eine abweichende Version wird
      ABGEWIESEN, statt die Bytes der alten Fassung im eigenen Sinn zu lesen. Das ist der
      Unterschied zwischen Datenverlust (merkt man) und fehlinterpretiertem Zustand (merkt man
      nicht). Ebenso weist eine fremde `program_id` ab; eine frische Region meldet `NoState`
      statt „Version 0", weil „noch keiner" und „der nullte" verschiedene Lagen sind. Der
      Übernahmezähler zählt nur bei tatsächlicher Übernahme, nicht bei Abweisungen — sonst
      belegte er das Gegenteil dessen, was er behauptet.
      **Belegt in zwei Stufen:** die Torlogik auf x86 (`state : ALL PASS`), der Ernstfall auf
      aarch64 (`ckpt : ALL PASS`): v1 rechnet `+1` und hinterlässt `[1, 2, 3]`, nach dem Reload
      rechnet v2 mit `+10` weiter und kommt auf `[13, 23]` — also auf den ÜBERNOMMENEN Werten,
      nicht auf frisch initialisierten, Übernahmegeneration 1. Der Ernstfall läuft nicht auf
      x86, weil er an der arch-neutralen Thread-Demo hängt, die dort nicht startet; im x86-Skript
      steht das als Anmerkung, damit ein grünes `state` nicht mehr verspricht, als es prüft.
      *(Alter Text:)* **A-4.3 Zustandsübergabe.** Die neue Fassung braucht den Zustand der alten.
      Entweder eine Region, die den Austausch überlebt (dann ist ihr Format eine ABI und muss
      versioniert werden), oder ein ausdrückliches Übergabeprotokoll. Ohne Festlegung ist
      Hot-Reload ein Neustart mit Datenverlust, der anders heißt.
- [x] **A-4.4 erledigt (2026-07-30), mit einer benannten Grenze.** Versionssperre im Lader:
      `iface_gate` hält beim ersten Laden einer `program_id` ihre `iface_version` fest und weist
      jeden weiteren Ladevorgang mit abweichender Version ab (`IfaceVersionChanged`). Verglichen
      wird gegen die **erste**, nicht gegen die vorige — sonst driftete die Schnittstelle in
      kleinen Schritten beliebig weit weg, obwohl kein einzelner Schritt erlaubt war. Die Version
      kommt aus dem **signierten** Manifest, nicht aus dem Image: sie ist eine Aussage über
      Zuteilung. Der Gate hängt in **beiden** Ladepfaden (`load_image`, `load_program_into_pd`),
      sonst wäre er umgehbar. Läuft die Buchhaltung voll, wird abgewiesen (`IfaceTableFull`) statt
      still nicht mehr zu prüfen — eine Prüfung, die unbemerkt aussetzt, sieht von aussen aus wie
      eine bestandene.
      **Die Grenze, und sie ist wichtig:** über das Manifest ist der Abweisungszweig heute
      **nicht erreichbar**. Pro Boot gibt es genau ein Manifest, jeder Ladevorgang derselben ID
      liest also dieselbe Version. Erreichbar wird er erst, wenn ein Austausch zur Laufzeit ein
      *anderes* Image mitbringt — das ist A-4.1/A-4.3 und existiert nicht. Damit der Zweig nicht
      bis dahin ungeprüft bleibt (ungeprüft heisst: vermutlich kaputt, wenn er zum ersten Mal
      gebraucht wird), ist die Buchhaltung als `iface_record_or_check` herausgezogen und wird vom
      Selbsttest direkt gefüttert: `iface : ALL PASS` belegt **beide** Ausgänge plus, dass eine
      andere `program_id` unberührt bleibt.
- [x] **A-4.5 erledigt (2026-07-30).** `docs/invariants.md` **§13**, normativ formuliert: der
      Kernel selbst (das Manifest ist an sein Image gebunden — ein getauschter Kernel entwertet
      jede Signatur, die auf ihn lautet), IOMMU-Kontexte und aktive DMA-Regionen (werden von der
      *Hardware* gelesen, nicht vom Kernel; „die PD ist weg" ist für ein Busmaster-Gerät keine
      Aussage), gebundene IRQ-Zustellung. Das Teardown-Token (ext-37) ist die richtige Grundlage:
      austauschbar ist eine PD erst, wenn sie ihre Geräte abgegeben hat.
      **Beim Schreiben dazugekommen:** eine gefärbte PD ist nur hot-reloadbar, wenn ein Streifen
      **frei** ist. A-4.1 verlangt atomares Umbinden, also existieren alte und neue Instanz
      kurzzeitig gleichzeitig und brauchen je einen disjunkten Satz. Bei vier Streifen und vier
      gefärbten PDs schlägt jeder Austausch fehl — sauber (B-4.2), aber er schlägt fehl. Die neue
      Instanz den Streifen der alten erben zu lassen ist **kein** Ausweg: solange beide leben,
      teilten sie sich die Farben.

## A-5. Etwas, das sich lohnt zu laden (Z10)

- [x] **A-5.1 erledigt (2026-08-02).** Ein Treiber laeuft als **Dienst** ausserhalb des Kerns,
      und der Kernel kann ihn **austauschen**, ohne zu wissen, was er treibt. Details, Messwerte
      und die Fehler, die dabei sichtbar wurden, in
      done.md.

      **Gemessen (Lade-Suite):**

          drv : Anfrage 1 an v1: Status=0 Bytes=0x454b414c344c4553 Kapazitaet=2048 bedient=1
          drv : Austausch: Ergebnis=0 (umgebunden OHNE Empfaengerluecke); v1 bereit=1 v2 bereit=1
          drv : Anfrage 2 an v2: Status=0 Bytes=0x454b414c344c4553 Kapazitaet=2048 bedient=2

      `programs/hardware/virtio-blk` mappt seine Fenster, loest sein Geraet auf **seiner eigenen
      Konfigurationsraum-Seite** auf und wartet dann in `recv`. Der Kernel ist **Client**, nicht
      Treiber. Zwischen den beiden Anfragen tauscht er den Empfaenger am laufenden Endpoint aus
      (A-4.1, `overlapped` — der Endpoint hatte zu keinem Zeitpunkt null Empfaenger). Der
      Bedienungszaehler 1 → 2 liegt in der DMA-Region und belegt, dass die neue Fassung **dieselbe
      Region geerbt** hat: ein Austausch, kein Neustart.

      In `loader::reload_driver` kommt das Wort „virtio" nicht vor. Das ist die Abnahmebedingung,
      nicht ein Zufall: solange der Kernel wuesste, was der Treiber treibt, waere „austauschbar"
      eine Eigenschaft dieses einen Treibers und nicht des Mechanismus.

      **Was ausdruecklich NICHT dazugehoert — und warum es nicht hierher gehoert:** `CAP_IRQ`.
      Der Treiber **pollt** seinen used-Ring. Ein Geraete-Interrupt kaeme auf x86 per MSI-X, und
      seit B-3.2 steht die Interrupt-Remapping-Tabelle auf lauter „not present": ein Geraet ohne
      IRTE kann keinen Interrupt ausloesen — mit Absicht. Eine **IRTE-Vergabe gibt es nicht**
      (geprueft, `crates/caprock-hal/src/x86_64/vtd.rs`). Der Weg dorthin ist damit B-3-Arbeit
      (Vergabe + Invalidierung ueber QI), nicht A-5.1. `endow_from_manifest` weist `CAP_IRQ`
      deshalb **ab**, statt eine Autoritaet zu erteilen, die niemand einloest — dieselbe Regel wie
      bei `CAP_PD_CONTROL` in A-2.1.

      *(Die urspruengliche Begruendung, weil sie weiter gilt:)* Simon, 2026-08-01: *„in den
      Mikrokernel sollen nur Sachen die reingehoeren, keine Treiber direkt im Mikrokernel."* Das
      ist die Voraussetzung fuer Hot-Reload, Fehlereindaemmung und Mandantentrennung — ein Treiber
      im Kern kann keins davon. Die Grenze ist seit 2026-08-01 **geprueft**, nicht nur
      beschrieben: `tools/kernel-grenze.sh` fuehrt eine Erlaubnisliste der HAL-Module, jeder
      Eintrag mit Begruendung, und weist im Selbsttest nach, dass er ein untergeschobenes Modul
      erkennt.

- [x] **A-5.2 erledigt (2026-08-01).** virtio auf x86: Transport, **Blockgerät und Netzkarte**.
      Details, Messwerte und die Annahme, die dabei umfiel, in
      done.md.

      Kurzfassung: `rng` belegte den Transport, aber nur **eine** Richtung — das Gerät schreibt in
      unseren Speicher, es liest nie etwas von uns. `blk` schließt die Lücke mit einer
      dreigliedrigen Deskriptorkette (Anfragekopf, den das Gerät **liest**), `net` fügt die zweite
      Queue mit eigenem `queue_notify_off` hinzu. Beide Treiber liegen kernfrei in
      `crates/caprock-virtio`; in der HAL blieb das Auffinden der Strukturen.

      **Korrektur zu einer Zeile, die hier erst falsch stand:** „`attach` liefert auf x86 weiter
      `None`" stimmt nicht. `VtdEnforcer::attach` ist implementiert und teilt zu — der `dmatok`-Test
      belegt es seit Längerem (`attach-installierte-Uebersetzung=true`), und A-5.1 benutzt es: die
      Treiber-PD bekommt eine echte IOVA (`0x20e00000` gegen PA `0x3b51000`). Die Zeile war aus dem
      alten Text übernommen, ohne sie gegen den Code zu halten; derselbe Satz stand auch im
      `vtdcaps`-Bericht des Kernels und ist dort ebenfalls korrigiert.
      Offen bleiben B-3.3 (Mehr-Einheiten-Aggregation) und B-3.4 (Fensterwahl gegen `0xFEE0_0000`)
      — beides Vollständigkeitslücken, keine Funktionssperre. Die A-5.2-Tests laufen trotzdem
      **vor** dem VT-d-Aufbau, weil sie den Fall „Gerät ohne Zuteilung" prüfen sollen.
## A-6. Ueber dem Sektor: Blockdienst, Partition, Dateisystem

Alles hier laeuft **ausserhalb des Kerns** (Simon, 2026-08-02: *„moeglichst als Treiber, nicht im
Kernel"*). Der Kern bekommt davon nichts: die Parser sind abhaengigkeitsfreie Crates wie
`caprock-virtio`, gelinkt von Userland-PDs.

- [x] **A-6.1 erledigt (2026-08-02): das Dienstprotokoll ueber dem Treiber.**
      `OP_INFO` (Kapazitaet, Hoechstzahl je Anfrage, Sektorgroesse), `OP_READ`, `OP_WRITE`,
      `OP_FLUSH`, und ein eigener Status **Bereich** fuer Sektoren jenseits der Platte.
      Details in done.md.

      **Gemessen:** `INFO Kapazitaet=2048 Sektorgroesse=512; READ(0)=0 WRITE(100)=0 FLUSH=0;
      Rueckgelesen=0x454b414c344c4553; READ(jenseits der Platte)=3`.

      Der Puffer des Treibers ist die **Ablage**: `READ` fuellt ihn, `WRITE` schreibt ihn zurueck.
      Der Client nennt Sektoren, keine Adressen. Eine geteilte Uebertragungsflaeche kommt erst mit
      A-6.2 dazu, wo es einen Abnehmer dafuer gibt — eine Schnittstelle vor ihrem ersten Benutzer
      belegt nur eine Vermutung.

- [x] **A-6.2 erledigt (2026-08-02): die Partitionstabelle.** `crates/caprock-part` —
      GPT-Parser, `#![no_std]`, `forbid(unsafe_code)`, **keine Abhaengigkeiten**, **14 von 14**
      Host-Tests. Gelesen wird sie im **Blockdienst**, nicht im Kern.
      Details in done.md.

      **Gemessen:** `GPT-Scan Status=0 belegte Eintraege=2; erste Partition LBA 34 ueber 967
      Sektoren`. Gegenprobe mit drei kaputten Tabellen: Signatur, Kopf-CRC, Eintrags-CRC — alle
      drei abgewiesen, mit **unterscheidbaren** Gruenden (2, 5, 8).

      Die Testabbilder baut `tools/mkgpt.py` (beide Suiten, dasselbe Werkzeug) — selbst gebaut
      statt `sgdisk` aufgerufen, weil eine Suite, die an einem Fremdwerkzeug haengt, auf einem
      Rechner ohne dieses Werkzeug als „Test rot" ausfaellt statt als „Aufbau unvollstaendig".
      Und nur ein eigenes Werkzeug kann den Negativfall herstellen (`--break`).

      **Was NICHT gebaut wurde, und warum:** die geteilte Uebertragungsflaeche. Sie wird erst
      gebraucht, wenn ein Client die Sektoren SELBST sehen soll — der Scan lief im Dienst, der die
      Bytes ohnehin hat. Eine Schnittstelle vor ihrem ersten Benutzer belegt nur eine Vermutung.
      Fuer A-6.3 kommt sie, und dann mit der offenen Frage: die DMA-Region des Treibers ist
      non-coherent gemappt, eine gecachte Zweitabbildung derselben Seiten waere auf x86 ein
      Attribut-Alias. Der saubere Weg ist eine **getrennte** Region und ein Kopierschritt im
      Treiber — ein echter Treiber tut das ohnehin, wenn der Client-Puffer nicht DMA-faehig ist.

- [x] **A-6.3 erledigt (2026-08-02): ein lesendes Dateisystem als eigene PD.**
      `crates/caprock-fat` (FAT16, **16 von 16** Host-Tests) + `programs/trusted/fs`.
      Details in done.md.

      **Gemessen:** `fs : Status=0; Groesse=20 erste acht Byte=0x454b414c344c4553 Cluster=1` —
      die Datei wurde nicht bloss gefunden, sondern **gelesen**, ueber GPT → FAT16 → Blockdienst →
      Treiber, und kein Schritt davon liegt im Kern.

      Die PD faehrt **kein Geraet**. Sie ruft den Blockdienst ueber dessen Kanal und liest die
      Bytes aus der **geteilten Uebertragungsflaeche** — einer eigenen Region aus normalem RAM,
      nicht der DMA-Region des Treibers: die ist non-coherent gemappt, und eine gecachte
      Zweitabbildung derselben Seiten waere auf x86 ein Attribut-Alias. Der Treiber kopiert.

      **Offen und benannt: die Domaene.** Die PD ist TrustedSAS, weil ein HardwareLand-Backend nur
      Caps seines eigenen Kanals halten darf und sein Partner TrustedSas sein muss (ext-22). Ein
      Dateisystem, das FREMDE Bytes liest, in einer vertrauenswuerdigen Domaene zu fuehren ist ein
      Geruch. Der richtige Weg ist ein UserLand↔TrustedSas-Kanal darueber (ext-22 P6) — dann ist
      diese PD der Server, und der Mandant sitzt untrusted darueber.

- [x] **A-6.4 erledigt (2026-08-02): Schreiben.** Die Dateisystem-PD verlaengert eine Datei ueber
      einen zweiten Cluster hinaus, schreibt die Kette in **alle** FAT-Kopien, flusht und liest
      **jedes Byte** zurueck. Details in done.md.

      **Gemessen:** `Schreiben Status=0; Rueckgelesen Status=0 Groesse=700 geprueft=700 Byte`, und
      unabhaengig davon am Abbild: `OK: HELLO.TXT, 700 Byte, 2 Cluster, 2 FAT-Kopien gleich`.

      Der zweite Melder (`tools/checkfat.py`) hat sich sofort bezahlt gemacht: in der Gegenprobe,
      in der die PD absichtlich nur EINE FAT-Kopie fortschreibt, meldet der Kernel weiterhin
      `fs : ALL PASS` — die PD liest ueber Kopie 0 korrekt zurueck —, und **nur** der unabhaengige
      Leser sieht, dass die Kopien auseinanderlaufen. Ein Schreiber, der sein eigenes Ergebnis
      bestaetigt, bestaetigt nichts.

      **Nicht dabei:** Anlegen und Loeschen von Dateien, Unterverzeichnisse, lange Namen, und die
      Suche nach einem freien Cluster geht nur ueber den ERSTEN FAT-Sektor. Alles benannt, nichts
      davon stillschweigend weggelassen.

- [x] **A-5.3 erledigt (2026-08-02): das Manifest sagt, WELCHES Gerät — nicht die Fundreihenfolge.**
      Details in done.md.

      Die Vorbedingung war erfüllt: Interrupt Remapping steht seit B-3.2 (aktiv, CFI abgeschaltet).

      Der Manifest-Eintrag trägt jetzt einen **Geräte-Selektor** (`vendor`/`device`/`class`,
      je einzeln „beliebig") in 8 der 12 reservierten Bytes — `ENTRY_LEN` bleibt 96, bestehende
      signierte Manifeste bleiben gültig, und Nullen dort heißen ausdrücklich **„beliebig"** und
      nicht „passt auf nichts". Der Selektor benennt eine **Art** von Gerät, nie eine Instanz: ein
      Manifest mit `00:04.0` wäre auf der nächsten Maschine stillschweigend falsch.

      **Fail-closed:** passt kein Gerät, gibt es keines. Der bequeme Rückfall („nichts passt → nimm
      irgendeins") wäre genau die versteckte Politik, gegen die der Selektor antritt.

      **Offen geblieben (klein, aber benannt):** der Kernel findet die *Kandidaten* weiterhin über
      `hal::pcie::find(VIRTIO_VENDOR, …)`, weil die BAR-Bestimmung durch den virtio-Fähigkeitslauf
      geht (`probe_transport`). Die **Auswahl** ist damit sauber im Manifest, das **Angebot** noch
      nicht: ein Nicht-virtio-Gerät ließe sich heute gar nicht anbieten. Wer eine zweite
      Gerätefamilie will, muss zuerst die BAR-Bestimmung von virtio lösen.

- [x] **A-5.4 erledigt (2026-08-02): zwei Treiber-PDs, und das Gerät der einen erreicht die
      DMA-Region der anderen nicht.** Details in
      done.md.

      **Teil 1** — `programs/hardware/virtio-net` ist der zweite Treiber, beide bekommen ihr im
      Manifest **benanntes** Gerät (`devsel : ALL PASS`, je Eintrag geprüft), der Client benennt
      seinen Dienst (`service_id`). Der Umbau legte **vier versteckte Politiken** frei — „die erste
      benutzte Zuteilung", „der zuletzt geladene Dienst", eine geteilte Notification-Ablage und
      eine geteilte Übertragungsfläche; alle vier laufen jetzt über die `program_id`, und wo eine
      Wahl mehrdeutig wäre, wird **abgewiesen statt geraten**.

      **Teil 2** — `dmaiso : ALL PASS`. Vier Zahlen, keine reicht allein: Positivkontrolle über
      denselben Treiber und dieselbe Deskriptorkette (nur **eine** Adresse wandert), keine Daten
      beim Fremdversuch, das Opfer **vom Kernel** nachgeprüft unberührt, und ein VT-d-Fault als
      *aktiver* Beleg, dass geblockt wurde. Zwei Mutationen belegen die Zeile: der Angreifer auf
      die eigene Region → FAILURES; kein Gegenüber → SKIP mit Begründung.

      Zwei Fehler im eigenen Entwurf gefunden und behoben: `arp_probe` nullte nur **acht Byte** des
      Empfangspuffers, also las die zweite Probe die Antwort der ersten; und `rx_used` (used-Ring)
      gehörte nicht ins Kriterium — es sagt, dass das Gerät *gehandelt* hat, nicht dass Daten
      ankamen.

---

## Koordinationsregeln

**Dateibesitz.** Wer eine Datei besitzt, ändert sie ohne Rückfrage; wer sie nicht besitzt, meldet
sich vorher.

* **Strang A besitzt:** `kernel/src/loader.rs`, `crates/caprock-loader/`, `crates/caprock-cap/`,
  `crates/caprock-ipc/`, `crates/caprock-abi/`, `crates/caprock-microkit/`,
  `kernel/src/arch/x86_64/bootinfo.rs`, `tools/`, `programs/`, `kernel/Cargo.toml`.
* **Strang B besitzt:** `crates/caprock-sync/`, `crates/caprock-mem/`, `crates/caprock-hal/`,
  `kernel/src/colors.rs`, `kernel/src/dmatests.rs`, `test-qemu*.sh`, `build*.sh`, `docs/`.
* **Geteilt, deshalb mit Ansage:** `kernel/src/system.rs`, `kernel/src/arch/x86_64/bringup.rs`,
  `kernel/src/main.rs`, `docs/invariants.md`, `TODO.md`.
  In `system.rs` gilt grob: A ändert Syscall-/Cap-/Loader-Pfade, B die Speicher-, Farb- und
  Telemetriepfade.

**Vier echte Kopplungen** — alles andere ist unabhängig:

1. **Politikfelder im Manifest** (A-1.4 ↔ B-4): A besitzt das Format, B die Bedeutung.
2. **Der Root-Task erzeugt PDs** (A-2.1 ↔ B-4.1): er muss den *gefärbten isolierten* Pfad
   benutzen, sobald B ihn zum Normalfall gemacht hat. Bis dahin den heutigen.
3. **Ruhender Punkt** (A-4.2 ↔ Z4a): einmal bauen, A besitzt ihn.
4. **Dynamische Tabellen** (A-3.4 ↔ B-4.2): mehr PDs als Streifen muss sauber scheitern.

**Reihenfolge über die Stränge hinweg:** B-1 (Determinismus) zuerst, von beiden abzuwarten. Ein
Testaufbau, der sporadisch hängt, macht jede Messung beider Stränge wertlos.

**Vor jeder Übergabe:** `./test-qemu-x86.sh` grün (bzw. begründet, was nicht), und die
Host-Unit-Tests: `rustc --test --edition 2021 -O crates/caprock-mem/src/lib.rs -o /tmp/t && /tmp/t`.
