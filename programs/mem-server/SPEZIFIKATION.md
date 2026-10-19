# Mem-Server Stufe 2: SPEZIFIKATION (Entscheidung, Schnittstelle, Stufenplan)

Stand: 2026-09-09. Stufe 1 ist gebaut und host-geprüft (`src/lib.rs`, `src/protokoll.rs`):
eine PD hält eine 2-MiB-Arena, vergibt Grants per IPC, lebende Grants überlappen nie, ein
fremder Schein löst nicht auf. Was Stufe 1 bewusst nicht kann, steht in der `lib.rs`-Doku:
kein echter Cap-Transfer ohne `CSUB` (der Schein ist ein Versprechen, keine Cap), kein
Demand-Paging, kein COW, Entzug nur kooperativ. Diese Datei ändert daran nichts — sie
entscheidet, was Stufe 2 darauf baut. **Kein Engine-Bau.**

## Die Entscheidung: WASM zuerst, musl bedingt, Kompat-Lib punktuell

Drei Wege (TODO.md Z14 Stufe 2 / Z15, hier nur angewandt, nicht neu bewertet):

| Weg | TCB | Reichweite | Preis |
|---|---|---|---|
| WASM in einer PD | null (Engine ist Userland) | alles, was zu WASM/WASI kompiliert | keine nativen Binaries; `no_std`-Engine muss rein |
| musl gegen `libcaprock` neu gelinkt | null | fast alles aus Quelltext | je ein Dienst für FDs, Pfade, Threads, Signale |
| eigene Kompat-Bibliothek | null | was man selbst baut | kleinste Reichweite |

**Entschieden: WASM zuerst.** Drei Gründe, alle aus dem Bestand:

1. **Die Sandbox liegt in der PD, nicht im Kern** — TCB-Wirkung null bei geschlossenem
   WASI-Satz. Der Linux-Satz ist per Definition nicht geschlossen (gVisor: ~260 Syscalls in
   ~100 000 Zeilen Go mit Lücken; WSL1→WSL2: Microsoft gab die Persönlichkeit auf — das
   stärkste verfügbare Datum für „ABI-Treue einholen kostet Jahre").
2. **Der Speicherbedarf ist beim Start bekannt** (WASM-Linearspeicher) — er passt auf das
   heutige Grant-Modell ohne `memory.grow`: eine Anfrage, eine Länge, ein Zweck.
3. **musl braucht erst Stufe 1 UND einen Pfad-Dienst.** Heute: FAT16 über eine feste Datei
   (A-6.3). Datei-Deskriptoren, Pfade, Threads, Signale sind je ein eigener Dienst — der
   musl-Weg wird sinnvoll, wenn sie stehen, nicht vorher.

Kompat-Lib bleibt punktuell richtig (wo WASM nicht reicht), ersetzt aber keinen der Wege.

## Schnittstelle: WASM-Linearspeicher über Grants (Draht unverändert)

Der Draht ist `src/protokoll.rs` (`[u64; 4]`, vier Register — mehr trägt ein REPLY nicht):

| ART | Richtung | Worte | Antwort |
|---|---|---|---|
| ANFORDERN (1) | Client → Server | Länge, Zweck-Kennziffer | KODE, Handle |
| AUFLOESEN (2) | Client → Server | Handle | KODE, CPU, DEV, Länge |
| ABBILDEN (3) | Client → Server | Handle („per SYS_MAP abgebildet") | KODE |
| AUSBLENDEN (4) | Client → Server | Handle („per SYS_UNMAP entfernt") | KODE |
| FREIGEBEN (5) | Client → Server | Handle | KODE |
| ENTZUG (6) | Server → Client | Handle („bilde aus, der Schein ist tot") | — |

WASM-Abbildung: `ANFORDERN(Speicher, Zweck=Allgemein/Kennziffer 0)` → `AUFLOESEN` → `SYS_MAP`
→ `ABBILDEN`; Rückgabe `AUSBLENDEN` → `FREIGEBEN`. Keine neue Nachrichtenart, kein neues
Wort — der Linearspeicher ist ein Grant mit Zweck, weiter nichts.

## Stufenplan (jede Stufe einzeln abgenommen, keine baut die Engine)

- **2a — WASM ohne `memory.grow` (statisch).** Modul mit festem Speicher startet in einer PD,
  fordert beim Start genau einmal an, gibt beim Ende zurück. Abnahme: zwei WASM-PDs mit
  disjunktem Speicher, „zweite sieht nichts" wie Stufe 1, nur über den Draht.
- **2b — `memory.grow` via Server.** Wachsen heißt weiteres `ANFORDERN` (neuer Grant, kein
  Anhängen — Grants überlappen nie, also wächst der Speicher als Liste, nicht als Block).
  Abnahme: Modul wächst über die Startgröße, alter Inhalt bleibt, zweite PD sieht nichts.
- **2c — musl-Re-Link (bedingt).** Erst wenn Pfad-Dienst + Stufe 1 stehen: statisches
  `musl`-Binary gegen `libcaprock`, Abnahme `write`/`exit`/`brk`/`mmap` ohne Dateien, dann
  mit. Fällt die Bedingung, fällt die Stufe — 2a/2b hängen nicht daran.

**Explizit nicht hier:** Engine-Auswahl und -Bau (Kandidaten nur benannt: Engine mit
`no_std`-Pfad, z. B. wasmi-Klasse; Prüffrage ist der Speicherzugriff pro Lade/Speicher —
Trap bei Zugriff über den Linearspeicher hinaus). Erst die Schnittstelle, dann die Engine.

## Simon-Entscheidung 2026-09-10: WASM-Go

Go für den WASM-Weg. Nächster Schritt ist Stufe 2a ohne `memory.grow` (statisch, eine
Anfrage beim Start); 2b/2c hängen nicht daran und warten auf ihre Bedingung.

## Anhang: Stufe 2a — WASM ohne `memory.grow` (normativ, kein Engine-Bau)

Gilt nur für 2a. Jede Absage ist benannt — kein stilles Verhalten, kein Fallback.

1. **Grant-Abbildung.** Der lineare Speicher eines Moduls ist genau EIN Grant:
   `ANFORDERN(Länge, Zweck=Allgemein/Kennziffer 0)`, Länge = `memory.min * 64 KiB`,
   aufgerundet auf Seiten (bestehende `aufgerundet`-Regel). Absagen: `LeereAnfrage`
   (Länge 0 / `memory.min = 0`), `ZweckAbgelehnt` (jede Kennziffer ≠ 0),
   `KeinPlatz`/`TabelleVoll`/`KontingentErschoepft` wie bisher. Kein zweiter Grant
   für dasselbe Modul in 2a — ein zweites `ANFORDERN` derselben PD ist kein
   Wachstum, sondern ein zweiter Grant (2b macht daraus eine Liste).
2. **Kein `memory.grow` in 2a.** Die Engine bietet die Import-Funktion an, aber
   jeder Aufruf mit `delta > 0` trappt benannt: `WasmFehler::WachstumAbgelehnt`
   (Engine-seitig, kein Draht-Kode — der Draht bleibt ART 1–6). `delta = 0`
   (Größenabfrage) ist erlaubt und gibt die aktuelle Seitenzahl zurück. Stille
   Alternativen sind abgesagt: kein Abweisen per `-1` ohne Trap-Namen, kein
   Anhängen an den Grant (Grants überlappen nie), kein `ANFORDERN` aus der
   Engine heraus (die Engine spricht nicht mit dem Server).
3. **WASI-Imports: Negativliste.** 2a bietet NUR an: `fd_write` auf die
   Log-Senke (fd 1/2 → `caprock-log`, jede andere fd → `WasiFehler::FdAbgelehnt`),
   `clock_time_get` nur monoton (`CLOCK_MONOTONIC`, jede andere ID →
   `WasiFehler::UhrAbgelehnt`), `proc_exit` (beendet die PD, siehe 6) und
   `random_get` (Host-Zufall per Cap, kein deterministischer Fallback).
   Alles andere ist NICHT importiert — der Start scheitert benannt mit
   `WasmFehler::ImportAbgelehnt(Name)`, statt einen Stub zu linken:
   kein `path_open`/`fd_read`/`fd_seek` (kein Pfad-Dienst in 2a),
   kein `sock_*`, kein `thread_spawn`, kein `clock_time_set`, kein
   `poll_oneoff`-Warten (blockierendes Warten gibt es nur über PARK, nicht
   über WASI). `fd_write` auf fd ≠ 1/2 schreibt nichts und meldet BADF —
   kein Umleiten auf die Senke.
4. **Fuel/Rechenbudget.** Jede 2a-PD bekommt ein benanntes Budget:
   `WasmFehler::BudgetErschoepft` bricht die Ausführung ab (Trap, kein Hänger,
   kein Watchdog-Fall). Das Budget zählt Instruktionen (Engine-Fuel), nicht
   Wandzeit — Wandzeit ist nicht reproduzierbar. Budget 0 heißt: startet
   nicht (Absage beim Instanziieren, nicht beim ersten Schritt). Kein
   Fuel-Nachschub aus der PD selbst — wer weiterrechnen will, startet neu.
5. **Trap-Semantik.** Jeder WASM-Trap (OOB-Lade/Speichern, Division durch Null,
   unerreichbar, `WachstumAbgelehnt`, `BudgetErschoepft`) endet die PD mit
   benanntem Grund (`WasmFehler::*`), räumt per Draht auf
   (`AUSBLENDEN` → `FREIGEBEN`) und meldet an den Aufrufer — NIEMALS
   Kernel-Panic, NIEMALS Server-Zustandsänderung über den Draht hinaus.
   OOB ist ein PD-Fault wie ein Seitenfehler außerhalb des Grants, kein
   Beweis gegen die Engine: die Engine MUSS pro Lade/Speichern gegen die
   Grant-Grenze prüfen (Prüffrage aus der Engine-Auswahl, hier Norm).
6. **Instanziierungsmodell (Übergabe-PD).** Die WASM-PD instanziiert SELBST:
   Aufrufer liefert das Modul per `LOAD_IMAGE`-Cap (Byte-Schein, kein Grant),
   die neue PD fordert beim Start genau einmal Speicher an (Punkt 1), mappt
   per `AUFLOESEN` → `SYS_MAP` → `ABBILDEN` und startet die Engine darauf.
   Der mem-server instanziiert NICHT (er vergibt nur, er startet nichts).
   Absagen: Modul ohne `LOAD_IMAGE`-Schein → Start verweigert
   (`WasmFehler::KeinModul`); Modul mit `memory.min = 0` oder `memory.max`
   gesetzt → `WasmFehler::SpeicherformAbgelehnt` (2a kennt nur festen
   `min > 0` ohne `max` — `max` verspricht Wachstum, das 2a nicht einlöst).
7. **Abnahme 2a.** Zwei WASM-PDs, disjunkte Grants, „zweite sieht nichts" über
   den Draht (wie Stufe 1): fremdes Handle → `FremderSchein`; `memory.grow(1)`
   in beiden → `WachstumAbgelehnt`; Endlosschleife in einer → terminiert mit
   `BudgetErschoepft`, die andere läuft weiter; OOB-Schreiben in einer →
   PD-Fault mit Namen, Server-Bilanz unverändert.

### Offene Fragen (3, mit Empfehlung)

1. **Fuel-Höhe: fix oder pro Modul?** Empfehlung: fix pro 2a (eine Konstante,
   z. B. 10⁸ Schritte), erst in 2b pro Modul — 2a misst Terminierung, nicht
   Fairness.
2. **Log-Senke: eigene Zweck-Kennziffer oder `fd_write`→`caprock-log` direkt?**
   Empfehlung: direkt (kein Grant für Log-Bytes — der Draht trägt keine Bytes,
   s. `server_sieht_keine_daten`); Kennziffer bleibt 0.
3. **`proc_exit`-Kode: an Aufrufer zurück oder nur Log?** Empfehlung: Kode an
   Aufrufer per REPLY-Wort + Log-Zeile — ein Exit ohne Kode sieht wie ein
   Crash aus.
