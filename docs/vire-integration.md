# Vire auf Caprock — Integration: was nötig wäre, was gut wäre

*Vire ist ein AOT-Compiler (Python-ergonomisch, Rust-schnell, speichersicher via
RC + Zyklen-Collector) mit LLVM/clang-Backend und einer schlanken C-Runtime
(`~/Schreibtisch/FastLLVM`). Dieses Dokument beschreibt aus Caprock-Sicht, was
nötig ist, um Vire-Programme als Caprock-Komponenten laufen zu lassen, und was
darüber hinaus gut wäre. Vire-seitige Analyse: `sprache/CAPROCK-PORT-UND-
MICROKERNEL.md` im Vire-Repo.*

## Warum Vire zu Caprock passt (die Kernbeobachtung)
Caprocks Single-Address-Space (ADR 0002) gibt die Performance (Zero-Copy-IPC, keine
TLB-Shootdowns, SP-Tausch-Context-Switch) — **verlangt aber, dass JEDE Komponente
speichersicher ist**, weil es keine per-Prozess-MMU-Isolation gibt. Heute heißt das
„alles in Rust". **Vire erweitert die Menge sicherer Sprachen um eine zweite**:
speichersicher über RC + Collector statt Borrow-Checker, ergonomischer für
Anwendungslogik. Der Kernel liefert die Performance, die Sprache die Sicherheit, die
der Kernel voraussetzt — komplementär. Vire-Komponenten sind AOT (kein JIT/Warmup),
deterministisch, klein (16-B-Objekt-Header + Slab).

## Die Naht ist schon da: die `plat_*`-Schicht
Vires Runtime (`crates/driver/src/runtime.c`) hat unter `FASTLLVM_FREESTANDING`
bereits eine **plattform-abstrakte Schicht** mit dem ausdrücklichen Hinweis
„produktiv ersetzt die Zielumgebung `plat_*` durch ihren eigenen Allokator". Der
ganze Port ist im Kern: **diese wenigen Funktionen auf Caprock abbilden.** Kein
Sprachkern-Umbau, keine Runtime-Neuschreibung.

### Was Caprock bereitstellen muss (die konkrete C-ABI-Naht)
| Vire-Symbol | Semantik | Caprock-Abbildung |
|---|---|---|
| `plat_alloc(size)` → genullter Speicher | Objekt-/Slab-Speicher | aus einer **cap-besessenen `caprock-region`** (ADR 0010) bedienen; der Slab instanziiert pro Region |
| `plat_free(ptr)` | Freigabe | in die Region-Freiliste (oder no-op, wenn Region en bloc endet) |
| `plat_write(s, n)` | Ausgabe | **IPC** an einen Konsolen-/Serial-Server (Endpoint-Cap), kein stdio |
| `plat_abort()` / `jrt_platform_halt()` (weak) | Fehler-Halt | Komponente terminieren / Fault an Supervisor |
| `jrt_debug_putchar(c)` (weak) | Debug-Byte | optional Debug-Konsole |

Die Runtime baut `#![no_std]`-artig ohne libc; Caprock stellt diese ~5 Funktionen
als `extern "C"` (Rust) bereit. Das ist der gesamte Pflicht-Teil von Phase A.

## BERICHTIGUNG nach einem Blick in das Vire-Repo (2026-08-12)

Drei Angaben unten stimmen so nicht. Gemessen an `/home/simon/Schreibtisch/FastLLVM`
(`crates/driver/src/main.rs`, `crates/driver/src/runtime.c`, `sel4/README.md`):

1. **Es gibt kein `--target`-Flag.** Der Treiber kennt `-o`, `--main`, `--emit-ir`,
   `--emit-llvm`, `--stats`, `--no-solver`, `--freestanding` — und nimmt `.class`/`.jar`
   als Eingabe. Die Zielarchitektur kommt aus dem `clang`-Aufruf, nicht aus einem
   Sprachflag. „Das Flag existiert" war eine Annahme, und ein Grund, der nicht stimmt,
   macht den Punkt unbehebbar, statt ihn zu lösen.
2. **`--freestanding` liefert ein RELOKATIERBARES OBJEKT** (`clang -r`), kein ladbares
   ELF. Caprocks Loader nimmt `ET_EXEC`-ELF64 mit seitenausgerichteten `PT_LOAD`-Segmenten
   (s. `programs/user-x86.ld`, und am 2026-08-10 hat genau die Ausrichtung `wasmhost`
   gekostet). Zwischen Vire-Ausgabe und Loader fehlt also ein **Linkschritt mit
   Caprock-Linkerskript** — er ist klein, aber er ist nicht null.
3. **Die freestanding-Vorgabe ist ein statischer 16-MiB-Heap in `.bss`** (`plat_heap`,
   First-Fit-Freiliste, kein `brk`/`mmap`; über `-DFASTLLVM_HEAP_SIZE` änderbar). Das ist
   die wichtigste Zahl für Caprock: die private Region einer isolierten PD ist heute
   **2 MiB**, und die Mandantendichte hängt genau daran (`RAM / 2 MiB`, s. `TODO.md` C7).
   Eine Vire-Komponente mit Vorgabeheap kostete **das Achtfache**. Der Ausweg steht
   unten schon richtig — `plat_*` auf `caprock-region` legen —, aber er ist damit nicht
   „gut zu haben", sondern **Pflicht**, bevor eine Zahl über Dichte wieder gilt.

## Was nötig ist (Pflicht, Phase A — bootende Komponente)
1. **Target:** Vire baut `vire build --target aarch64-unknown-none <prog>.vr`
   (das `--target`-Flag existiert). Ergebnis: ein natives Objekt/ELF ohne libc.
   (x86 folgt, wenn der Caprock-x86-Branch landet → `x86_64-unknown-none`.)
2. **Laden:** über den generischen **Binary-Loader** (ADR 0011, cap-gegatet) — Vires
   Ausgabe ist ein extern gebautes Objekt, genau die Zielklasse des Loaders.
3. **Speicher:** die `plat_*`-Naht auf `caprock-region` legen (s.o.). Der
   16-B-Header + Slab passen ideal zu knappen Regionen.
4. **IO:** `plat_write` → Konsolen-IPC.
5. **Einstieg/Ende:** Vires `main`→`java_main`-Entry aufrufen; **kein `atexit`** (in
   der freestanding-Runtime schon weg) — die Komponente läuft dauerhaft / via
   Supervisor beendet.

## Was gut wäre (Phase B/C — nutzt Caprocks Stärken)
- **Zero-Copy-Objektübergabe (SAS-Bonus):** weil alle Komponenten einen Adressraum
  teilen, kann eine Vire-Objektreferenz **cap-gewährt ohne Kopie** an eine andere
  Komponente gehen — anders als bei seL4. Vires opaker **`Ptr`-Typ = eine Capability**
  (Handle auf Region/Endpoint) ist die natürliche FFI-Grenze.
- **Determinismus für Real-Time-Komponenten:** Vire hat die Hebel schon — **`--no-cycles`**
  (solver-bewiesen azyklisch → reine RC, kein Collector) + **Region-Inferenz** (borgt
  Traversal-Referenzen, kein RC) + **Auto-Arena** (per-Iteration-Bump, en-bloc-frei).
  Damit sind Vire-Komponenten **GC-pausenfrei** und speicher-vorhersagbar — passt zu
  Caprocks Determinismus-Ziel.
- **Nebenläufigkeit:** Vires `--threads`-Pfad (heute pthreads) auf **Caprock-TCBs +
  Notifications/IPC** (Scheduler Phase 4/5) neu implementieren. Monitore → Caprock-
  Primitiven.
- **Hot-Reload (Phase 7):** Vire-Komponenten sind AOT + deterministisch → passen zum
  v1→v2-Austausch über dieselbe Endpoint-Cap. Ein Vire-Server könnte hot-reloadbar
  sein wie die Rust-Server heute.
- **Das GC-Modell im SAS (Designentscheidung):** **nicht** einen geteilten Collector
  über Komponentengrenzen (koppelt alle → pausiert alle). Stattdessen **per-Komponente
  Regionen + eigener Slab/Collector**; über die Grenze nur cap-gewährte `Ptr`-Handles.
  Isolation der GC-Domänen = Isolation der Latenz.

## Integrationsmodell (Empfehlung)
**Die Runtime NICHT nach Rust umschreiben** — sie ist reines, freestanding-fähiges C
(RC/Collector/Slab laufen schon ohne libc). Stattdessen:
1. Ein Caprock-Crate `caprock-vire-rt` liefert die `plat_*`/`jrt_*`-Weak-Symbole
   als `extern "C"` (backed by `caprock-region` + IPC).
2. Vire baut das Programm-Objekt (`--target aarch64-unknown-none`); die Runtime.c
   wird freestanding mitkompiliert (Vires Treiber embed sie schon via `include_str!`).
3. Linken: Programm-Objekt + Vire-Runtime + `caprock-vire-rt` → eine
   Komponente; der Loader (ADR 0011) lädt sie cap-gegatet.

So bleibt die Vertrauenskette intact (Capabilities/Region-Runtime/Loader unverändert),
und die einzige neue vertrauenswürdige Fläche ist die kleine `plat_*`-Naht.

## Verifikations-Hinweis
Caprocks Sicherheit ruht auf „jede Komponente speichersicher". Vires Sicherheit ist
RC + Collector (kein Borrow-Checker-Beweis). Für hochsichere Komponenten: die
`plat_*`-Naht + der Collector wären der Audit-Fokus (analog zu ADR 0021
„unsafe memory safety"). Vires **Heap-Bilanz-Oracle** (0 live objects am Ende, in der
Testsuite geprüft) ist ein starker Runtime-Soundness-Check, den man in die
Caprock-Tests übernehmen könnte.

## Kurzfassung
**Nötig:** die `plat_*`-Naht (~5 Funktionen) auf `caprock-region` + Konsolen-IPC,
aarch64-Target, Laden via Loader. **Gut:** Zero-Copy-`Ptr`=Cap, `--no-cycles`/Region/
Arena für deterministische Komponenten, Caprock-Threads, Hot-Reload, per-Komponente
GC-Domänen. **Kein** Sprach- oder Runtime-Neubau — nur ein kleines Plattform-Crate.
