#!/usr/bin/env bash
# **Die Host-Tests der reinen Crates — an einem Ort** (B-5.5-Nachtrag, 2026-08-02).
#
# Warum es dieses Skript gibt: mehrere Crates tragen `#[cfg(test)]`-Module mit echter Deckung, und
# ein Teil davon lief **nirgends**. `caprock-cap` ist das deutlichste Beispiel — sechs Tests der
# CDT-Laufgrenzen, die kein Skript und keine CI anfasste. Ein Test, der nirgends laeuft, ist kein
# Test, sondern eine Absichtserklaerung.
#
# Der Grund war mechanisch: `cargo test -p <crate>` scheitert am erzwungenen Custom-Target aus
# `.cargo/config.toml` (`build-std`). Der Ausweg steht in AGENTS.md und ist derselbe, den
# `tools/kani-verify.sh` schon nimmt — die Crate ausserhalb des Workspace mit einem
# Minimal-Manifest bauen:
#
#   * **abhaengigkeitsfreie** Crates direkt ueber `rustc --test` (Sekunden, kein Cargo noetig);
#   * Crates **mit** Abhaengigkeiten ueber ein Wegwerf-Cargo-Projekt im TMPDIR.
#
# Aufruf:
#   tools/host-tests.sh            # alles
#   tools/host-tests.sh cap        # nur ein Ziel
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP="${TMPDIR:-/tmp}"
RUSTC="rustup run nightly rustc"
CARGO="rustup run nightly cargo"
fail=0

# --- Abhaengigkeitsfreie Crates: ein Aufruf, kein Cargo ------------------------------------------
einzeln() { # $1 = Anzeigename, $2 = Pfad zur Quelldatei
    local name="$1" src="$2" bin="$TMP/hosttest_$1"
    echo "== Host-Tests: $name =="
    # **Fehlend und kaputt duerfen nicht gleich aussehen** (Befund aus B-5.1).
    #
    # Vorher meldete jede Quelle, die nicht vorlag, "liess sich nicht uebersetzen" -- derselbe
    # Wortlaut wie ein echter Compilerfehler. In einem unvollstaendigen Baum (anderer Zweig,
    # frischer Klon, Agenten-Worktree) las sich das wie ein kaputtes Projekt, und umgekehrt haette
    # sich ein wirklich kaputtes Ziel als "ist halt nicht da" abtun lassen.
    #
    # Dieselbe Form wie die stummen VT-d-Einheiten (B-3.3) und wie `CycleStats::measurable()`:
    # "nicht gemessen" ist keine Messung. Ein fehlendes Ziel ist deshalb ein eigener Ausgang --
    # er faellt DURCH (ein Ziel, das hier steht, soll existieren), sagt aber, warum.
    if [ ! -f "$src" ]; then
        echo "  FEHLT: $src ist nicht vorhanden -- Ziel nicht gelaufen (kein Uebersetzungsfehler)"
        fail=1; return
    fi
    rm -f "$bin"
    if ! $RUSTC --test --edition 2021 -O "$src" -o "$bin" 2>&1 | grep -E "^error" -A 6; then :; fi
    if [ ! -x "$bin" ]; then
        echo "  FEHLER: $name liess sich nicht uebersetzen"; fail=1; return
    fi
    "$bin" || fail=1
    rm -f "$bin"
}

# --- Crates mit Abhaengigkeiten: Wegwerf-Projekt ausserhalb des Workspace ------------------------
#
# Die Abhaengigkeiten werden **mitkopiert**, nicht per Pfad in den Workspace gezeigt: ein
# `path = "../.."` zoege `.cargo/config.toml` und damit das Custom-Target wieder herein, und wir
# waeren zurueck beim `build-std`-Umweg (16 Minuten, s. AGENTS.md).
mit_deps() { # $1 = Name, $2 = Crate-Verzeichnis, $3.. = Abhaengigkeiten (Verzeichnisname)
    local name="$1" dir="$2"; shift 2
    local SA="$TMP/hosttest_${name}_proj"
    echo "== Host-Tests: $name =="
    rm -rf "$SA"; mkdir -p "$SA/src"
    cp -r "$ROOT/crates/$dir/src/." "$SA/src/"
    {
        printf '[package]\nname="%s"\nversion="0.0.0"\nedition="2021"\n[workspace]\n[lib]\npath="src/lib.rs"\n[dependencies]\n' "$dir"
        for d in "$@"; do
            mkdir -p "$SA/$d/src"
            cp -r "$ROOT/crates/$d/src/." "$SA/$d/src/"
            printf '[package]\nname="%s"\nversion="0.0.0"\nedition="2021"\n[lib]\npath="src/lib.rs"\n' "$d" > "$SA/$d/Cargo.toml"
            printf '%s = { path = "%s" }\n' "$d" "$d"
        done
        printf '[lints.rust]\nunexpected_cfgs = { level = "warn", check-cfg = ['"'"'cfg(kani)'"'"', '"'"'cfg(loom)'"'"'] }\n'
    } > "$SA/Cargo.toml"
    ( cd "$SA" && $CARGO test --release ) || fail=1
    rm -rf "$SA"
}

ZIELE="${*:-mem part fat cycles loader cap virtio dma wait region irte irteneg grossdmaneg dmar dmarneg iohealth smt numa bootparams fbtext dtb acpi platform redirect cspace redirectneg typestate ipctreue schedtreue}"
for z in $ZIELE; do
    case "$z" in
        mem)  einzeln mem  "$ROOT/crates/caprock-mem/src/lib.rs" ;;
        part) einzeln part "$ROOT/crates/caprock-part/src/lib.rs" ;;
        fat)  einzeln fat  "$ROOT/crates/caprock-fat/src/lib.rs" ;;
        # `caprock-sched` als Ganzes haengt an `caprock-hal` (arch-Asm) und wird auf dem Host nie
        # bauen. Die Zyklenarithmetik (B-5.1) liegt deshalb abhaengigkeitsfrei in einem eigenen
        # Modul und wird als **Datei** geprueft -- die Fallen dort sind reine u64-Rechnung und
        # brauchen keine Maschine, sondern Literale.
        cycles) einzeln cycles "$ROOT/crates/caprock-sched/src/cycles.rs" ;;
        # Z26/A3: das Kernel-Primitiv fuer UMGELEITETE SYSCALLS. Dieselbe Begruendung wie bei
        # `cycles` -- die Fallen sind Reihenfolgen und Zahlenbereiche (ein Zyklus in der
        # Handler-Kette, ein Slot-Index um eins daneben, ein Rueckfall auf die native ABI nach dem
        # Entzug einer Cap) und mit LITERALEN ausloesbar. Der Rest des Primitivs (Cap-Aufloesung,
        # Frame-Transport, Scheduler) haengt an `caprock-hal` und wird in QEMU geprueft.
        redirect) einzeln redirect "$ROOT/crates/caprock-sched/src/redirect.rs" ;;
        # TODO0 K1c: die Vergabe-Logik des variablen PD-Cspace steht genau einmal in
        # `cspace.rs` (nur `core`, kein `alloc`/`unsafe`) und wird hier als DATEI
        # geprueft — der Kernel selbst haengt an `caprock-hal` und baut auf dem Host nie.
        cspace) einzeln cspace "$ROOT/crates/caprock-microkit/src/cspace.rs" ;;
        # `caprock-loader` ist abhaengigkeitsfrei und traegt die Parser fuer Boot-Archiv, ELF64
        # und **System-Manifest**. Es gibt kein `.github/workflows/` in diesem Baum -- die Tests
        # liefen also nirgends, genau wie die von `caprock-cap` vor B-5.5. Kani prueft Beweise,
        # keine `#[test]`s; das ist nicht dasselbe.
        loader) einzeln loader "$ROOT/crates/caprock-loader/src/lib.rs" ;;
        # `caprock-cap` haengt an `caprock-mem` und `caprock-slab`. Bis B-5.5 liefen seine Tests
        # deshalb nirgends -- die Huerde war das Manifest, nicht der Code.
        cap)  mit_deps cap caprock-cap caprock-mem caprock-slab ;;
        # `caprock-virtio` ist abhaengigkeitsfrei (A-5.1). Host-pruefbar ist daran die
        # Typestate-Buchhaltung (`owned.rs`, todo E): Schnittarithmetik ueber Adressen, ohne
        # Zugriff und ohne Geraet. Alles andere in der Crate fasst MMIO an und gehoert in die
        # QEMU-Suiten.
        virtio) einzeln virtio "$ROOT/crates/caprock-virtio/src/lib.rs" ;;
        # `caprock-dma` ist der DMA-Pool einer Treiber-PD (Z22 P3): abhaengigkeitsfrei,
        # `forbid(unsafe_code)`, reine Adressarithmetik. Genau die Sorte, die sich mit LITERALEN
        # ausloesen laesst statt mit einer Maschine -- die entscheidende Aussage („eine Adresse
        # ausserhalb des Pools bekommt KEINE IOVA") braucht kein Geraet, nur einen Zahlenbereich.
        dma)  einzeln dma  "$ROOT/crates/caprock-dma/src/lib.rs" ;;
        # `caprock-wait` (Z22 P2): Mutex/WaitQueue/Completion fuer eine PD mit mehreren Threads.
        # Der ganze Vertrag mit dem Kernel ist ein Trait mit zwei Methoden, also laesst sich die
        # Logik gegen einen STELLVERTRETER pruefen -- die Fallen hier sind Reihenfolgen (verlorenes
        # Wecken, voller Warteraum), keine Hardware. Derselbe Weg wie bei `ipctreue`.
        wait) einzeln wait "$ROOT/crates/caprock-wait/src/lib.rs" ;;
        # `caprock-region` (ext-25, Z22 Speicher): haengt an `caprock-mem`/`caprock-sync`
        # (beide abhaengigkeitsfrei). Traegt `page` (struct-page-Schema) und `kmalloc`
        # (GFP-Schema) fuer die Linux-Compat -- laeuft hier, nicht erst in QEMU.
        region) mit_deps region caprock-region caprock-mem caprock-sync ;;
        # `caprock-hal` als Ganzes ist arch-Asm und baut auf dem Host nie. `dmar.rs` ist die
        # Ausnahme: **reine Funktion ueber eingespeiste Daten** (`forbid(unsafe_code)`, keine
        # `use`-Zeile ausser `super::*` im Testmodul), also als DATEI pruefbar -- derselbe Weg wie
        # `cycles`. Bis zum 2026-08-03 lief das Testmodul deshalb NIRGENDS: vier Tests, kein
        # Aufrufer. Neu dazu die RMRR-Gruppenfaelle (E-Rest 2), die q35 mit seinen 0 RMRRs
        # strukturell nicht zeigen kann -- die QEMU-Suiten sind hier blind, nicht nachlaessig.
        # Z22 P1: die IRTE-/MSI-Kodierung. Dieselbe Begruendung wie `dmar` -- reine Schieberei,
        # und ein Bit an der falschen Stelle aeussert sich als „das Geraet unterbricht einfach
        # nicht": ohne Fehlermeldung, ohne Fault, ohne irgendetwas, das nach einem Fehler
        # aussieht. Mit Literalen in Sekunden pruefbar; in QEMU braeuchte es Geraet und Glueck.
        irte) einzeln irte "$ROOT/crates/caprock-hal/src/x86_64/irte.rs" ;;
        # ... und die Gegenprobe dazu (Z22 P1, 2026-08-10). Dieselbe Begruendung wie bei `dmarneg`:
        # die Tests in `irte.rs` sehen nur den BEHOBENEN Zustand. Sieben Mutationen bauen den
        # Fehler einzeln wieder ein, jede mit dem NAMEN des Tests, der fallen muss. In QEMU waere
        # das nicht zu zeigen -- ein falsches Bit in einer IRTE aeussert sich als „das Geraet
        # unterbricht einfach nicht".
        irteneg)
            if [ ! -f "$ROOT/tools/irte-vergabe-negativ.sh" ]; then
                echo "== Host-Tests: irteneg =="
                echo "  FEHLT: tools/irte-vergabe-negativ.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/irte-vergabe-negativ.sh" || fail=1
            fi ;;
        # Die Gegenprobe zur Klassifikation grosser DMA (Z26 V2). Der interessante Teil ist die
        # REIHENFOLGE der Pruefungen: dieselbe Lage kann zwei wahre Namen haben, und der falsche
        # schickt den Leser in die falsche Richtung.
        grossdmaneg)
            if [ ! -f "$ROOT/tools/grossdma-negativ.sh" ]; then
                echo "== Host-Tests: grossdmaneg =="
                echo "  FEHLT: tools/grossdma-negativ.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/grossdma-negativ.sh" || fail=1
            fi ;;
        dmar) einzeln dmar "$ROOT/crates/caprock-hal/src/x86_64/dmar.rs" ;;
        # Die arch-neutrale IOMMU-Gesundheit (2026-08-17). Dieselbe Begruendung wie `dmar`/`irte`:
        # ein reiner Typ ueber eingespeisten Werten, ohne eine `use`-Zeile ausser `super::*` --
        # also als DATEI pruefbar, waehrend `caprock-hal` als Ganzes auf dem Host nie baut.
        # Geprueft wird die Reihenfolge des Urteils, und die IST der Inhalt: `faults_empty` zaehlt
        # erst, wenn der Round-Trip belegt ist. Eine tote Einheit meldet ebenfalls eine leere
        # Warteschlange -- in QEMU waere genau dieser Fall nicht herstellbar, ohne die Einheit
        # kaputtzumachen.
        iohealth) einzeln iohealth "$ROOT/crates/caprock-hal/src/iommu_health.rs" ;;
        smt)  einzeln smt "$ROOT/crates/caprock-hal/src/smt.rs" ;;
        numa) einzeln numa "$ROOT/crates/caprock-hal/src/numa.rs" ;;
        bootparams) einzeln bootparams "$ROOT/crates/caprock-hal/src/bootparams.rs" ;;
        fbtext) einzeln fbtext "$ROOT/crates/caprock-hal/src/fbtext.rs" ;;
        # Device-tree parser (real QEMU virt + Qualcomm X1P42100 blobs under tests/fixtures).
        dtb) einzeln dtb "$ROOT/crates/caprock-dtb/src/lib.rs" ;;
        # Static ACPI table parser (real-table-shaped blobs built in-code, no AML).
        acpi) einzeln acpi "$ROOT/crates/caprock-acpi/src/lib.rs" ;;
        # Platform description: a single hal file that needs `caprock_dtb` + `caprock_acpi` as rlibs.
        platform)
            echo "== Host-Tests: platform =="
            if $RUSTC --crate-type rlib --crate-name caprock_dtb --edition 2021 -O "$ROOT/crates/caprock-dtb/src/lib.rs" -o "$TMP/libcaprock_dtb.rlib" 2>&1 | grep -E "^error" -A 6; then :; fi
            if $RUSTC --crate-type rlib --crate-name caprock_acpi --edition 2021 -O "$ROOT/crates/caprock-acpi/src/lib.rs" -o "$TMP/libcaprock_acpi.rlib" 2>&1 | grep -E "^error" -A 6; then :; fi
            rm -f "$TMP/hosttest_platform"
            if $RUSTC --test --edition 2021 -O --extern caprock_dtb="$TMP/libcaprock_dtb.rlib" --extern caprock_acpi="$TMP/libcaprock_acpi.rlib" "$ROOT/crates/caprock-hal/src/aarch64/platform.rs" -o "$TMP/hosttest_platform" 2>&1 | grep -E "^error" -A 6; then :; fi
            if [ -x "$TMP/hosttest_platform" ]; then "$TMP/hosttest_platform" || fail=1; else echo "  FEHLER: platform liess sich nicht uebersetzen"; fail=1; fi
            rm -f "$TMP/hosttest_platform" "$TMP/libcaprock_dtb.rlib" "$TMP/libcaprock_acpi.rlib" ;;
        # ... und die Gegenprobe dazu: die Tests oben sehen nur den BEHOBENEN Zustand. Vier
        # Mutationen bauen den Fehler einzeln wieder ein, jede mit dem NAMEN des Tests, der fallen
        # muss (sonst waere „irgendetwas ist rot" schon ein Beleg).
        dmarneg)
            if [ ! -f "$ROOT/tools/dmar-rmrr-negativ.sh" ]; then
                echo "== Host-Tests: dmarneg =="
                echo "  FEHLT: tools/dmar-rmrr-negativ.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/dmar-rmrr-negativ.sh" || fail=1
            fi ;;
        # **Der Uebersetzer als Pruefer.** Der Descriptor-Typestate behauptet etwas ueber Code, der
        # NICHT uebersetzt -- und ein solcher Code steht per Definition in keinem Testbinary. Der
        # Nachweis muss deshalb `rustc` selbst befragen, mit Positivkontrolle und mit ERWARTETEN
        # Fehlercodes (sonst waere jeder Tippfehler ein Beleg).
        # ... und die Gegenprobe: die Tests oben sehen nur den GEBAUTEN Zustand. Sieben
        # Mutationen bauen je EINEN Fehler wieder ein, jede mit dem NAMEN des Tests, der fallen
        # muss.
        redirectneg)
            if [ ! -f "$ROOT/tools/redirect-negativ.sh" ]; then
                echo "== Host-Tests: redirectneg =="
                echo "  FEHLT: tools/redirect-negativ.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/redirect-negativ.sh" || fail=1
            fi ;;
        typestate)
            if [ ! -f "$ROOT/tools/typestate-negativ.sh" ]; then
                echo "== Host-Tests: typestate =="
                echo "  FEHLT: tools/typestate-negativ.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/typestate-negativ.sh" || fail=1
            fi ;;
        # `caprock-ipc` haengt an `caprock-hal` (arch-Asm) und wird auf dem Host nie bauen -- wie
        # `caprock-sched`. Anders als bei `cycles` liegt die Logik aber NICHT abhaengigkeitsfrei in
        # einem eigenen Modul, sondern mitten im Endpoint. Der Ausweg sind Stellvertreter fuer
        # HAL/Scheduler/ABI; weil derselbe Aufbau zugleich das Verus-Modell gegen den echten Code
        # faehrt, liegt er in `tools/verus-modelltreue-ipc.sh` und wird hier nur gerufen.
        ipctreue)
            echo "== Host-Tests: ipctreue (Endpoint gegen das Verus-Modell) =="
            if [ ! -x "$ROOT/tools/verus-modelltreue-ipc.sh" ] && [ ! -f "$ROOT/tools/verus-modelltreue-ipc.sh" ]; then
                echo "  FEHLT: tools/verus-modelltreue-ipc.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/verus-modelltreue-ipc.sh" || fail=1
            fi ;;
        # **Warum der Scheduler-Waechter seit dem 2026-08-21 HIER steht** (Befund, nicht Kosmetik):
        # er hing allein an `tools/verus-verify.sh`, und das laeuft in keinem Gatter dieser Abnahme.
        # Gemessen an diesem Tag: er meldete **drei** unbenannte Schreibstellen aus Z6b
        # (`debug_stop`, `debug_continue`, `debug_release_pd`) -- seit zwei Tagen rot, ohne dass es
        # jemand sah. Ein Gatter, das existiert und nicht gefahren wird, ist keins; dieselbe Klasse
        # wie `.gitea/workflows` auf einem GitLab-Server. Er braucht kein Verus (nur Python) und
        # gehoert damit genau dorthin, wo der IPC-Waechter schon steht.
        schedtreue)
            echo "== Host-Tests: schedtreue (Scheduler gegen das Verus-Modell) =="
            if [ ! -f "$ROOT/tools/verus-modelltreue-sched.sh" ]; then
                echo "  FEHLT: tools/verus-modelltreue-sched.sh ist nicht vorhanden -- Ziel nicht gelaufen"
                fail=1
            else
                bash "$ROOT/tools/verus-modelltreue-sched.sh" || fail=1
            fi ;;
        *)    echo "  FEHLER: unbekanntes Ziel '$z' (bekannt: mem part fat cycles loader cap virtio dma wait region irte irteneg grossdmaneg dmar dmarneg iohealth smt numa bootparams fbtext dtb acpi platform redirect cspace redirectneg typestate ipctreue schedtreue)"; fail=1 ;;
    esac
done

if [ "$fail" = 0 ]; then echo "== HOST-TESTS: ALL PASS =="; else echo "== HOST-TESTS: FAILURES =="; fi
exit "$fail"
