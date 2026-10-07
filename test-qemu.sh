#!/usr/bin/env bash
# Automatisierter QEMU-Boot-/SMP-/Timer-Test für Caprock.
#
# Baut den Kernel, bootet ihn unter QEMU (ARM virt, 8 Kerne, 4 GiB), erfasst die
# serielle Ausgabe für einige Sekunden und prüft auf die erwarteten Marker:
#   * MMU aktiv (M=1 C=1 I=1)
#   * alle 8 Kerne online
#   * jeder Kern erzeugt Timer-Ticks
set -uo pipefail
cd "$(dirname "$0")"

CORES=8
# Der Kernel fährt nach bestandenem Selbsttest QEMU per PSCI SYSTEM_OFF herunter
# (`== SELFTEST COMPLETE ==`), daher endet ein erfolgreicher Lauf, sobald er fertig
# ist — auf einem ruhigen Host in wenigen Sekunden. Das Timeout ist nur eine
# Obergrenze für den Fehlerfall (Kernel bleibt dann im Idle). Großzügig gewählt:
# zwei Fuzzer (Ressourcen + IPC) + SMP + isolierte VSpaces teilen unter
# single-threaded QEMU-TCG eine Host-CPU; unter schwerer Host-Last emuliert alles
# deutlich länger (kein Kernel-Hang — der Manager druckt sonst eine `DBG pending`-Zeile).
SECONDS_RUN="${1:-360}"
ELF="build/target/aarch64-caprock/release/caprock-kernel.elf"

# In-Kernel-Fuzzer (ADR 0013) sind ein optionales Feature. Default: RELEASE-Build OHNE Fuzzer
# (genau die Konfiguration des Langzeittests/Produktivkernels) -> die vier Fuzzer-Checks entfallen.
# Mit `KERNEL_FUZZ=1 ./test-qemu.sh` wird `--features kernel-fuzz` gebaut und die Fuzzer mitgeprueft.
# `selftest` wird AUSDRUECKLICH angefordert, nicht ueber `default` mitgenommen: nach A-2.2 steht
# es dort nicht mehr, und diese Suite bootet einen Kernel, dessen Selbsttestbericht sie auswertet.
# Ohne die Angabe waere der Lauf danach still statt rot (s. AGENTS.md Mitteilung 4).
FEAT="--features selftest${KERNEL_FUZZ:+,kernel-fuzz}"

# **D5: der Manifest-Schluessel, und zwar VOR dem Build.** Die oeffentliche Haelfte wird in
# `kernel/src/manifest_keys.rs` **einkompiliert** -- ein danach erzeugtes Paar waere ein Schluessel,
# den das laufende Image nicht kennt. Dieselbe Reihenfolge wie beim TrustedSAS-Schluessel weiter
# unten, und aus demselben Grund.
python3 -c "import cryptography" 2>/dev/null || {
    echo "== FEHLT: das Python-Paket 'cryptography' (signieren geht ohne nicht) =="
    echo "   pip3 install --user cryptography   # oder --break-system-packages auf Debian"
    exit 2
}
MANKEY=keys/manifest-test.manifest.ed25519
python3 tools/gen_manifest_key.py --ensure >/dev/null || exit 2

echo "== build ${FEAT:-(release, ohne Fuzzer)} =="
./build.sh $FEAT >/dev/null 2>&1 || { echo "BUILD FAILED"; exit 1; }

# ext-26 (L1): die EXTERNEN Programme bauen (eigener Workspace, eigene Target/Linker) + ins
# Boot-Archiv legen (Host-Tool tools/mkarchive.py, KEIN Kernelcode). `hello` ist ein echtes,
# extern gebautes EL0-Programm, das der Kernel laedt + ausfuehrt; `probe` ist ein Platzhalter,
# der den Multi-Modul-/Listen-Pfad zeigt.
mkdir -p build
( cd programs && rustup run nightly cargo build --release ) >/dev/null 2>&1 \
    || { echo "PROGRAMS BUILD FAILED"; exit 1; }
# ext-27: die ADVERSARIALEN Testdienste (eigener tests/-Workspace) bauen — geladen wie Drittsoftware,
# greifen den Kernel + sich gegenseitig an (ADR 0012). NICHT Teil des Kernel-Images.
( cd tests && rustup run nightly cargo build --release ) >/dev/null 2>&1 \
    || { echo "TESTS BUILD FAILED"; exit 1; }
HELLO="programs/build/target/aarch64-caprock-user/release/hello.elf"
SVCDEMO="programs/build/target/aarch64-caprock-user/release/svc-demo.elf"
INIT="programs/build/target/aarch64-caprock-user/release/init.elf"
TBIN="tests/build/target/aarch64-caprock-user/release"
printf 'PLACEHOLDER' > build/_probe.bin
# ext-28 (ADR 0014): TrustedSAS-Binaries signieren. tools/sign_trusted.py fuehrt zuerst den
# Unsafe-Audit (Allowlist {libcaprock}) durch -> KEIN Zertifikat bei Verletzung, dann SHA-256-
# Bindung an genau dies ELF + Ed25519-Signatur ueber die volle Nachricht. program_id/version MUESSEN
# zum Archiv-Eintrag passen (Identitaets-Bindung). Schluessel: keys/trusted-test (privat, gitignored).
mkdir -p certs

# **B-2.1: aus einem frischen Clone lauffaehig.** `keys/` ist gitignored -- bis hierher scheiterte
# diese Suite bei jedem, der das Repo neu ausgecheckt hat, an einem fehlenden Schluessel. Damit war
# der GESAMTE aarch64-Zweig ungeprueft, und genau diese Fehlerform hat das Projekt schon mehrfach
# bezahlt (leere Event-Queue, nie ausgefuehrter x86-Testpfad, DMAR-Ausschlusspfad).
#
# Der Weg ist derselbe wie auf der x86-Seite (Strang A, `test-qemu-x86-load.sh`): **erzeugen statt
# einchecken**. Ein privater Schluessel im Repo waere bei einem Open-Source-Projekt kein
# Testschluessel, sondern ein veroeffentlichter -- und der Vermerk "nur fuer Tests" haelt genau so
# lange, wie jemand ihn liest. Der Preis ist ein maschinenlokaler Wert im Image: er kostet
# Reproduzierbarkeit ZWISCHEN Entwicklern, nicht INNERHALB eines Checkouts, und nur Letzteres
# braucht die Suite.
#
# Reihenfolge ist zwingend: die Key-DB (`kernel/src/trusted_keys.rs`) wird in den Kernel
# **kompiliert**. Ein neu erzeugter Schluessel nach dem Build waere ein Schluessel, den das laufende
# Image nicht kennt. Deshalb hier, VOR `./build.sh`.
# **Geprueft wird die UEBEREINSTIMMUNG, nicht die EXISTENZ** (2026-08-13).
#
# Hier stand `[ ! -f keys/trusted-test.ed25519 ]`. Die x86-Lade-Suite hat genau diese Pruefung am
# 2026-08-01 ersetzt, mit Begruendung im Skript und einem eigenen Werkzeug
# (`tools/check_trusted_key.py`) -- und diese Suite behielt die alte Fassung. **Zwei Suiten, die
# dasselbe verschieden aufsetzen, sind ein Riss**, und dieser hier ist am 2026-08-13 wieder
# eingetreten:
#
#   `kernel/src/trusted_keys.rs` ist versioniert und traegt den OEFFENTLICHEN Schluessel; der
#   private unter `keys/` ist es nicht. Wer die Datei zuruecksetzt -- ein `git checkout`, ein Pull,
#   oder das in `AGENTS.md` verlangte Aufraeumen am Sitzungsende --, hat zwei Haelften, die nicht
#   zusammengehoeren. Die Existenzpruefung sieht das nicht, und der Lauf endet mit
#
#       root    : FAILURES (Rejected(Unverified)) -- kein Root-Task
#
#   plus drei weiteren roten Zeilen (`loadhw`, `aggrt`, `cross`), die alle daran haengen. Das
#   sieht wie ein Kernelbefund aus und ist ein Aufbauproblem -- genau die Verwechslung, die dieses
#   Projekt sonst ueberall vermeidet.
#
# `rc=2` heisst "nicht entscheidbar" und wird NICHT als in Ordnung gewertet: ein Pruefer, der nicht
# pruefen konnte, hat nichts belegt.
python3 tools/check_trusted_key.py
KEYRC=$?
if [ "$KEYRC" -eq 2 ]; then
    echo "== FEHLER: der TrustedSAS-Schluessel liess sich nicht gegen den eingebetteten pruefen. =="
    echo "   Das ist KEIN Testergebnis -- ohne diese Pruefung waere ein spaeteres"
    echo "   'Rejected(Unverified)' nicht von einem echten Befund zu unterscheiden."
    exit 2
fi
if [ "$KEYRC" -ne 0 ]; then
    # `gen_trusted_key.py` weigert sich, einen vorhandenen privaten Schluessel zu ueberschreiben --
    # eine bewusste Sicherung. Also beiseitelegen statt loeschen (dieselbe Behandlung wie in der
    # x86-Lade-Suite): ein privater Schluessel ist nichts, was ein Testskript unwiderruflich
    # wegwerfen darf, auch kein Testschluessel.
    if [ -f keys/trusted-test.ed25519 ]; then
        BEISEITE="keys/trusted-test.ed25519.passt-nicht-$(date +%Y%m%d-%H%M%S)"
        mv keys/trusted-test.ed25519 "$BEISEITE"
        [ -f keys/trusted-test.ed25519.pub ] && mv keys/trusted-test.ed25519.pub "$BEISEITE.pub"
        echo "== TrustedSAS-Schluessel passte nicht zum eingebetteten -> beiseite: $BEISEITE =="
    else
        echo "== TrustedSAS-Testschluessel fehlt -> erzeugen (frischer Clone) =="
    fi
    python3 tools/gen_trusted_key.py --name trusted-test >/dev/null 2>&1 || {
        echo "SCHLUESSEL-ERZEUGUNG FEHLGESCHLAGEN"; exit 2; }
    echo "   erzeugt; kernel/src/trusted_keys.rs regeneriert -> Kernel wird neu gebaut"
    ./build.sh $FEAT >/dev/null 2>&1 || { echo "BUILD FAILED (nach Key-Regen)"; exit 1; }
    python3 tools/check_trusted_key.py || {
        echo "   FEHLER: auch nach dem Neuerzeugen passt der Schluessel nicht -- hier stimmt"
        echo "           etwas Grundsaetzliches nicht (gen_trusted_key.py? Pfade?)"; exit 2; }
fi

sign() { python3 tools/sign_trusted.py --key keys/trusted-test.ed25519 "$@" >/dev/null 2>&1; }
sign --crate programs/trusted/svc-demo --elf "$SVCDEMO" --program-id 12 --version 1 \
     --out certs/trusted-x.cert || { echo "SIGN trusted-x FAILED"; exit 1; }
sign --crate tests/services/trusted/aggressor --elf "$TBIN/aggressor-t.elf" --program-id 24 --version 1 \
     --out certs/aggressor-t.cert || { echo "SIGN aggressor-t FAILED"; exit 1; }
# D5: `init` ist TrustedSAS -> das Zertifikats-Gate (ADR 0014) gilt auch fuer den Root-Task. Das
# Zertifikat bindet an DIESES aarch64-ELF; `certs/init-x86.cert` traegt hier nicht (anderer Hash).
sign --crate programs/trusted/init --elf "$INIT" --program-id 1 --version 1 \
     --out certs/init-arm.cert || { echo "SIGN init FAILED"; exit 1; }
# hello=UserLand(2), hwhello=HardwareLand(1) (gleiches ELF, L3); trusted-x=TrustedSAS(0): das saubere,
# zertifizierte svc-demo (laedt GELADEN als EL0-isolierte PD). probe=Platzhalter (Multi-Modul-Liste).
# ext-27 Testdienste je Domaene (2 je Domaene): aggressor/intruder -u=UserLand(2), -h=HardwareLand(1),
# -t=TrustedSAS(0). ext-28: TrustedSAS-Module tragen ein Zertifikat (7. Feld); aggressor-t ist
# zertifiziert (laedt + attackiert), intruder-t bewusst OHNE Zertifikat -> verify_image weist es ab.

# --- D5: das System-Manifest ---------------------------------------------------------------------
#
# **Was hier fehlte und warum es zaehlte.** Der aarch64-Kernel meldete `root : FAILURES
# (NoManifest)` -- eine rote Zeile, die diese Suite gar nicht prueft. Ein Urteil, das niemand
# ansieht, ist kein Urteil; der 67. Test, der wirklich bricht, waere darin verschwunden. Das ist
# dieselbe Form wie `-no-shutdown` (rc war immer 124), nur mit umgekehrtem Vorzeichen.
#
# **Die Startmenge ist GENAU EIN Eintrag.** Das Archiv enthaelt zehn weitere Module -- die
# adversarialen Testdienste, `probe` (ein Platzhalter, kein ELF) und die Loader-Testfaelle. Die
# gehoeren nicht zur Startmenge: sie werden von den Kerntests gezielt geladen. Genau daran haengt
# die Pruefung, die D5 im Kernel noetig gemacht hat (`StartSetNotPrefix`): der Root-Task bekommt
# die GROESSE der Startmenge aus dem Manifest und spricht ihre Mitglieder ueber einen ARCHIVINDEX
# an. Damit beide dasselbe meinen, muss `init` auf Archivposition 0 liegen -- deshalb steht es
# unten als ERSTES Argument, und deshalb prueft der Kernel es nach, statt es zu glauben.
python3 tools/sign_manifest.py --kernel "$ELF" --key "$MANKEY" --manifest-version 1 \
    --out build/system.manifest \
    --entry "1:init:0:1:$INIT:loader,ntfn:root:3::any:0" >/dev/null 2>&1 \
    || { echo "MANIFEST BUILD FAILED"; exit 1; }

python3 tools/mkarchive.py build/boot-archive.bin --system-manifest build/system.manifest \
    1:init:0:1:"$INIT"::certs/init-arm.cert \
    10:hello:2:1:"$HELLO" 11:hwhello:1:1:"$HELLO" 12:trusted-x:0:1:"$SVCDEMO"::certs/trusted-x.cert 2:probe:2:1:build/_probe.bin \
    20:aggressor-u:2:1:"$TBIN/aggressor-u.elf" 21:intruder-u:2:1:"$TBIN/intruder-u.elf" \
    22:aggressor-h:1:1:"$TBIN/aggressor-h.elf" 23:intruder-h:1:1:"$TBIN/intruder-h.elf" \
    24:aggressor-t:0:1:"$TBIN/aggressor-t.elf"::certs/aggressor-t.cert 25:intruder-t:0:1:"$TBIN/intruder-t.elf" \
    >/dev/null 2>&1 || { echo "ARCHIVE BUILD FAILED"; exit 1; }

echo "== boot ($SECONDS_RUN s) =="
# ext-23: SMMUv3 (IOMMU) + virtio-rng-pci HINTER einem pcie-root-port (StreamID = PCI-RID).
# QEMUs SMMUv3 uebersetzt nur Endpunkte hinter einem Root-Port (integrierte Bus-0-Endpunkte
# umgehen die SMMU) -> das DMA-Beweisgeraet haengt am Root-Port. Der Kernel ignoriert beide,
# solange die ext-23-Treiber nicht aktiv sind (Rueckwaertskompatibilitaet).
# --- Wiederholungen + zuverlaessiger Capture (D6) -----------------------------------------------
#
# **Zwei Aenderungen, und die erste ist wichtiger als sie aussieht.**
#
# 1. Die Ausgabe geht in eine **Datei**, nicht durch eine Pipe. Vorher hing sie an einer
#    Kommandosubstitution, und beendet wurde mit `--signal=KILL`. Ein per SIGKILL erschlagenes
#    QEMU flusht seinen stdout-Puffer nicht -- der Lauf verlor dann Ausgabe, und zwar
#    unvorhersehbar viel. Im Ergebnis fiel "mal dieser, mal jener Test" durch, ohne dass am Kernel
#    etwas anders war. Genau diese Falle hat die x86-Suite am 2026-08-01 schon einmal bezahlt.
# 2. **Wiederholungen** (B-1.3, hier nachgezogen): verglichen wird die Ergebnis-SIGNATUR ueber
#    `RUNS` Laeufe. Ein einzelner Lauf kann einen Nichtdeterminismus grundsaetzlich nicht finden --
#    er kann ihn nur zufaellig treffen. Die ARM-Suite lief bis heute genau einmal, und ein roter
#    Lauf war dort von einem echten Befund nicht zu unterscheiden.
#
# Die Mechanik ist bewusst **dieselbe** wie in `test-qemu-x86.sh` und keine zweite Fassung davon:
# zwei Fassungen derselben Messung laufen auseinander, und dann weiss niemand, welche gilt.
RUNS="${RUNS:-1}"
BOOT_TIMEOUTS=0

# **Jeder Lauf bekommt seine EIGENE Datei** ($1). Eine gemeinsame Datei ueber mehrere Laeufe war
# eine Fehlerquelle mit genau der Form, die man am schwersten sieht: gelegentlich fehlten in der
# ausgewerteten Ausgabe **fruehe** Bootzeilen, waehrend alle Ergebniszeilen da waren. Die Signatur
# blieb deshalb identisch, und trotzdem fiel mal `M=1 C=1 I=1`, mal `archive : 10 Modul` durch --
# also je nach Lauf eine ANDERE Pruefung, ohne dass am Kernel etwas anders war.
boot_once() {
    local LOG="$1"
    timeout --signal=KILL "$SECONDS_RUN" qemu-system-aarch64 \
        -machine virt,iommu=smmuv3,gic-version="${GIC_VERSION:-2}" -cpu cortex-a72 -smp "$CORES" -m 4G \
        -nographic -serial "file:$LOG" -no-reboot \
        -net none -device pcie-root-port,id=rp0,chassis=1 -device virtio-rng-pci,bus=rp0,iommu_platform=on \
        -device loader,file=build/boot-archive.bin,addr=0x13F000000 \
        -kernel "$ELF" </dev/null >/dev/null 2>/dev/null
    local rc=$?
    # rc 137 = SIGKILL durchs Zeitlimit. Ein zweiter, unabhaengiger Melder: er haengt an keiner
    # Zeile, die der Kernel drucken muss -- ein Kernel, der in der falschen Lage schweigt, koennte
    # die Logauswertung taeuschen, ein Zeitlimit nicht.
    { [ "$rc" -eq 137 ] || [ "$rc" -eq 124 ]; } && BOOT_TIMEOUTS=$((BOOT_TIMEOUTS + 1))
    return 0
}

# Die Ergebnissignatur: alle Ergebniszeilen plus der Abschlussmarker, sortiert. Gleiche Signatur
# heisst gleiches Testergebnis -- die Pruefungen unten sind reine greps auf genau diese Zeilen.
run_signature() {
    printf '%s\n' "$1" \
        | grep -oE '^[a-z0-9_]+ +: (ALL PASS|FAILURES|SKIP)|^== SELFTEST [A-Z]+( \(watchdog\))?' \
        | sort
}

echo "== boot ($SECONDS_RUN s, $RUNS Lauf/Laeufe) =="
LOG1="$(mktemp)"
boot_once "$LOG1"
OUT="$(cat "$LOG1" 2>/dev/null)"

REPEAT_OK=1
REPEAT_DONE=0
if [ "$RUNS" -gt 1 ]; then
    SIG0="$(mktemp)"; SIGN="$(mktemp)"
    run_signature "$OUT" > "$SIG0"
    REPEAT_DONE=1
    for n in $(seq 2 "$RUNS"); do
        LOGN="$(mktemp)"
        boot_once "$LOGN"
        OUTN="$(cat "$LOGN" 2>/dev/null)"
        run_signature "$OUTN" > "$SIGN"
        if cmp -s "$SIG0" "$SIGN"; then
            REPEAT_DONE=$((REPEAT_DONE + 1))
        else
            echo "== Lauf $n weicht vom ersten ab (< Lauf 1, > Lauf $n): =="
            diff "$SIG0" "$SIGN" | grep -E '^[<>]' | sed 's/^/     /'
            # Das VOLLE Log aufheben. Ohne das sieht man, DASS er abwich, aber nicht wo.
            mkdir -p build/diag
            cp -f "$LOGN" "build/diag/arm-abweichung-lauf-$n.log" 2>/dev/null \
                && echo "     (volles Log: build/diag/arm-abweichung-lauf-$n.log)"
        fi
        rm -f "$LOGN"
    done
    rm -f "$SIG0" "$SIGN"
    [ "$REPEAT_DONE" = "$RUNS" ] || REPEAT_OK=0
    echo "== Wiederholungen: $REPEAT_DONE von $RUNS mit IDENTISCHER Ergebnissignatur =="
fi

# Ein leerer Lauf sieht in der Auswertung aus wie "alle Pruefungen fehlgeschlagen" -- eine
# Fehldiagnose, die schlimmer ist als gar keine.
if [ -z "$OUT" ]; then
    echo "== KEIN OUTPUT: der Lauf hat nichts geliefert (Logdatei $(wc -c < "$LOG1" 2>/dev/null || echo 0) Byte) =="
    echo "== Das ist KEIN Testergebnis -- Aufbau pruefen (QEMU? Zeitlimit zu knapp?) =="
    rm -f "$LOG1"; exit 2
fi
# `$LOG1` bleibt bis zum Schluss liegen -- s. die Begruendung in test-qemu-x86.sh (D12).

echo "$OUT"
echo "== checks =="
fail=0
check() { if grep -q "$1" <<<"$OUT"; then echo "  PASS: $2"; else echo "  FAIL: $2"; fail=1; fi; }
# Fuzzer-Check: nur im `KERNEL_FUZZ=1`-Lauf relevant (im Release-Build ohne Fuzzer entfaellt die
# Zeile -> nicht als Fehler werten, sondern als bewusst uebersprungen melden).
fcheck() { if [ -n "${KERNEL_FUZZ:-}" ]; then check "$1" "$2"; else echo "  SKIP (release ohne Fuzzer): $2"; fi; }

check "dbg     : ALL PASS" "Z6b: Debug-Autoritaet ist eine Capability ueber GENAU EINE PD. Seit die Sonde arch-neutral ist und ihr eigenes Ziel mitbringt, faehrt sie hier genauso -- und **eine gedruckte Zeile, die niemand liest, ist die pdbind-Form**: `dbg : FAILURES` bei `== ALL PASS ==`, genau einmal so passiert, am 2026-08-20 auf diesem Zweig"
check "pdfreeze: ALL PASS" "Z23/S3: der GRUPPENSCHNITT -- eine ganze PD steht, oder keiner ihrer Threads. Die tragende Aussage ist `einzeln-unfrierbar=true/true` NEBEN `umfang=3`: dasselbe Thread-Paar, das `freeze_thread` einzeln mit `BusyOn` abweist (und zwar wechselseitig, jeder nennt den anderen), friert der Schnitt gemeinsam ein -- weil eine Beziehung, deren beide Enden im Schnitt liegen, keine offene Beziehung des Schnitts ist. Das ist der einzige Fall, den Z4a nicht schon zeigt. Dazu `frist-nennt-partner` (S2: eine Absage ohne Namen ist von einem Haenger nicht zu unterscheiden), `fremdruf-abgewiesen` (S1b, gemessen ueber DIESELBE `gate_new_transaction`, die `call`/`recv` ausfuehren) und `klient-bleibt-liegen` (das Auftauen entfernt FREEZE und **nur** das)"
check "ckptcut : ALL PASS" "Z4d stage 1: an open transaction must not cross the checkpoint cut. The line carries BOTH directions of one equivalence, measured at the same topology in the same run: offener-ruf-abgewiesen is Z4d verbatim (the caller migrates, his endpoint stays -- a waiting server left behind), and fremder-partner-abgewiesen is the half that had no name until 2026-08-25 (the endpoint migrates, the server blocked at it does not). That second half was unreachable before: Scope::endpoints was documented as 'endpoints whose both sides are part of the checkpoint' and was never held against the machine's actual IPC state -- a caller wrote a number down and the rule believed it, exactly the shape of ep_inv holding by call discipline rather than by type. geschlossene-beziehung-geht is the positive control without which 'refuses everything' would look identical to 'refuses the right thing'; wartender-empfaenger-abgewiesen covers the role WITHOUT a partner, where the tempting reading is that nobody is left behind (he leaves himself behind); and the ntfn- fields exercise the second channel kind, which would otherwise be unrun code. The kanten= counts are the speaking probe: a cut with no observed edges is clean, so 'found nothing' and 'nothing there' must not look alike"
check "arena  : ALL PASS" "K1b: VIER Thread-Stapel aus EINER Memory-Cap -- und der ERSTE Lauf von SYS_SPAWN ueberhaupt. Der Syscall stand seit dem 2026-08-17 in der ABI und hatte bis zum 2026-08-26 keinen Aufrufer und kein Gatter; ganze-region=true misst deshalb zuerst die x1==0-Form, die jeder vor der Teilregion geschriebene Aufruf kodiert. Die tragende Zahl ist slots=2 NEBEN threads=6: vier laufende Threads belegen nur, dass vier Threads laufen -- der Punkt ist, was sie gekostet haben, und eine Cap je Stapel machte `wie viele Threads darf eine PD haben` zu `wie viele Cap-Slots sind noch frei` (ein Treiber mit 6 von 8 belegten Slots kam auf zwei). disjunkt=true ist die Eigenschaft, an der alles haengt: jedes Kind legt ein aus SEINER Fensterbasis abgeleitetes Wort ab und liest es weiter nach -- ohne diese Zeile saehe `vier Threads auf einer Arena` genauso aus, wenn sie einander zertrampeln. lebendig=true ist die Lebendigkeit (eine Region mit einem Wert darin gehoert zu einem Thread, der eine Instruktion ausgefuehrt hat -- die Kapazitaetskurve hat einmal 3040 Leichen gezaehlt). ueberlappung-abgewiesen deckt eine Absage, die vor dem 2026-08-26 STRUKTURELL unerreichbar war: pd_mapping_overlaps liest KSTACKS.ubase_of, und spawn_with_stack_parked traegt dort absichtlich nichts ein -- der Pruefer konnte den Fall, gegen den er gebaut ist, nicht sehen"
check "vorher-undebuggbar=true" "Z6b §0 -- die tragende Zeile, auch hier: eine Sonde mit JEDER anderen Cap des Systems faehrt alle Cap-Slots der Wurzel-PD durch und wird jedes Mal abgewiesen"
check "Indizes-stimmen=true" "Z6b §3b: die Frame-Indizes kommen aus redirect::indizes, nicht aus dem Kopf -- die erste arch-neutrale Fassung schrieb hartkodiert auf Wort 18, was auf aarch64 ein ALLZWECKregister ist. Die Sprechprobe haelt die Tabelle gegen die Frame-Breite, die der Kernel wirklich ablegt"
check "revoke-bricht-nicht=true" "Z6b §6: ein Revoke waehrend das Ziel angehalten ist darf es nicht unbrauchbar machen -- gemessen an der Wirkung, auch auf dieser Architektur"
check "dbgmem  : ALL PASS" "Z6b: DEBUG_READ_MEM laeuft ueber die Seitentabellen des ZIELS. Das ist die EINZIGE Stelle in v1, an der x86_64 und aarch64 sich wirklich unterscheiden -- der Rest ist arch-neutral, dort traegt ein gruener x86-Lauf diese Seite mit, hier nicht"
check "luecke-abgewiesen=true" "Z6b: eine Adresse, die das Ziel nicht FUER SICH gemappt hat, loest nicht auf. Die erste Fassung prueft die Rechte nicht und loeste damit auch die geteilten KERNEL-Eintraege der isolierten VSpace auf -- ein Debugger mit blossem DebugRead haette Kernelspeicher gelesen. Gefunden hat es diese Sonde, nicht das Gegenlesen"
check "ohne-Leserecht-abgewiesen=true" "Z6b: die Debuggable-WURZEL gewaehrt selbst nichts -- sie ist das Recht abzuleiten. Ohne diese Zeile waere 'gewaehrt selbst nichts' Prosa ohne Gatter"
check "M=1 C=1 I=1" "MMU + Caches aktiv"
check "dtb     : ALL PASS" "DTB-Parsing (RAM-Größe aus dem Device Tree)"
check "archive : 11 Modul" "ext-26 L0: Boot-Archiv extern geladen + vom Kernel-Parser gelesen (reserviertes RAM-Fenster, caprock-loader)"
check "manifest: ALL PASS" "A-1.2/A-1.4 (D5): das System-Manifest wird auch auf aarch64 geprueft -- Signatur ueber die GESAMTE Nachricht, an DIESES Kernel-Image gebunden, Anti-Downgrade"
check "root    : ALL PASS" "A-2.1 (D5): der aarch64-Kernel hat einen Root-Task. Bis hierher meldete er 'root : FAILURES (NoManifest)', und diese Suite sah sich die Zeile ueberhaupt nicht an -- ein dauerhaft rotes Teilurteil lag unbeachtet im Bericht. Ein Urteil, das niemand ansieht, unterscheidet nicht mehr zwischen 'wie immer' und 'gerade gebrochen'"
check "memtest : ALL PASS" "Speichermodell-Selbsttest (alloc/split/transfer/free)"
check "zerotest: ALL PASS" "Datenremanenz (ext-29): frische UND wiederverwendete Allokationen sind genullt -- kein Restdatenleck zwischen Subjekten"
check "captest : ALL PASS" "Capability-Selbsttest (copy/mint/move/delete/revoke)"
check "dmaalign: ALL PASS" "DMA-Granularitaet (ext-35): unausgerichteter Anfang/angebrochene Laenge werden an der Cap-Praegung abgewiesen (Cache-Wartung wuerde sonst fremde Daten in der Randzeile verwerfen)"
check "budget  : ALL PASS" "Cap-Budget je PD (ext-29): Installationen ueber CAP_BUDGET_PER_PD hinaus abgewiesen (keine Monopolisierung der geteilten Cap-Tabelle), Ersetzen bleibt erlaubt"
# todo A1 / B-4.2. Bis 2026-08-01 fehlten diese beiden Zeilen hier nicht deshalb, weil der Test
# uebersprungen wurde -- er LIEF auf aarch64 gar nicht: sein einziger Aufrufer stand in
# `arch/x86_64/bringup.rs`. `hal::cache` hatte damit eine aarch64-Fassung, die uebersetzt, aber nie
# an einer echten Zuteilung geprueft wurde. Der Test liegt jetzt arch-neutral in `kernel/src/colors.rs`
# und wird von BEIDEN Hochlaufwegen gefahren -- dieselbe Behandlung wie `kernel/src/dmatests.rs`.
check "sched   : ALL PASS" "Scheduler (Preemption auf core 0 + alle Kerne ticken)"
check "fp      : ALL PASS" "FP/SIMD-Kontext bleibt über Preemption erhalten"
check "prio    : ALL PASS" "Bitmap-Prioritäten (höhere Priorität läuft zuerst)"
check "life    : ALL PASS" "Thread-Lebenszyklus (cap-KILL + EXIT + Stack-Rückgewinnung)"
check "notif   : ALL PASS" "Notifications (asynchrone Badge-Signale)"
check "xfer    : ALL PASS" "Capability-Transfer in IPC (Broker delegiert Service-Cap)"
check "grantlk : ALL PASS" "Grant-Leak-Regression (ext-29): 65 Grants in denselben Empfangs-Slot -> verdraengte Cap wird freigegeben, genau 1 lebende Ableitung, kein Leck in der geteilten Cap-Tabelle (Cross-PD-DoS)"
check "ipc     : ALL PASS" "Cap-gesicherte IPC (Server v1, PD<->PD)"
check "reload  : ALL PASS" "Hot-Reload (Server v2 ersetzt v1, gleicher Endpoint, kein Reboot)"
check "ckpt    : ALL PASS" "A-4.3 im Ernstfall: Stateful Hot-Reload -- v2 hat den Zaehler von v1 nachweislich UEBERNOMMEN (Uebernahmezaehler auf 1, gegen den versionierten Kopf der Zustandsregion geprueft), nicht bloss dieselben Werte erzeugt"
check "el0     : ALL PASS" "EL0-Userland (echter User-Thread ruft per Syscall)"
check "el0iso  : ALL PASS" "EL0-Isolation (Kernel-Zugriff faultet, Thread beendet, Kernel überlebt)"
check "scale   : ALL PASS" "Skalierung (ext-30): 1024 Threads GLEICHZEITIG (Kapazitaet zur Boot-Zeit aus dem RAM statt .bss-Konstante), alle auffindbar, danach vollstaendig abgebaut -> Slots + RAM zurueck auf Baseline"
check "migrate : ALL PASS" "Thread-Migration (ext-30): Thread wechselt zur Laufzeit den Kern -- gleiche ThreadId/Tcb-Cap, laeuft auf dem Zielkern weiter, balance_once() verschiebt automatisch, cross-core-KILL, Scheduler-Audit 0"
check "smp     : ALL PASS" "Per-Kern-paralleler Scheduler (Worker je Kern + Cross-Core-IPI-Wake)"
check "xipc    : ALL PASS" "Kern-übergreifende synchrone IPC (Client core0 <-> Server core2)"
check "reclaim : ALL PASS" "EL0-Kernel-Stack-Reclaim (>Pool-viele transiente EL0-Threads)"
check "balance : ALL PASS" "Lastausgleich (lastbewusste Thread-Platzierung über die Kerne)"
check "vspace  : ALL PASS" "Per-Prozess-VSpace (isolierte PD faultet bei Fremdzugriff, SAS-PD darf)"
check "vmm     : ALL PASS" "Allgemeiner VMM (Frame-Caps + map/unmap-Syscalls + VSpace-Teardown)"
check "shm     : ALL PASS" "Shared-Memory-IPC (ein Frame in zwei isolierte VSpaces, cap-gewährt)"
check "native  : ALL PASS" "Natives Code-Laden je isolierter VSpace (privates EL0-RX, W^X)"
check "pages4k : ALL PASS" "4-KiB-Seiten (gemischte RW/RO-Rechte + Guard Pages + L3)"
check "churn   : ALL PASS" "Ressourcen-/Teardown-Invarianten (tausende spawn/destroy ohne Leak)"
check "mcs     : ALL PASS" "MCS Scheduling Contexts (Budget per Cap, Verbrauch/Erschoepfung/Refill, Drosselung)"
check "stale   : ALL PASS" "Audit-Regression: IPC paniert nicht bei gekilltem, in Endpoint-Queue blockiertem Thread"
check "strand  : ALL PASS" "Audit-Regression: erneutes Budget-Bind strandet keinen erschoepften Thread"
check "rgone   : ALL PASS" "Reply-Liveness: toter Reply-Owner entblockt den CALL-Aufrufer (ERR_SERVER_GONE)"
check "ddon    : ALL PASS" "Budget-Donation: intra-core CALL belastet Server-Arbeit gegen das Aufrufer-Budget"
check "rcap    : ALL PASS" "First-class Reply-Cap (ObjectKind::Reply): Revocation bricht den ausstehenden Call ab"
check "rmig    : ALL PASS" "Reply-Cap-Server-Migration: ausstehender Call ueberlebt einen Hot-Reload (v2 schliesst ihn ab)"
fcheck "fuzz    : ALL PASS" "Generativer Kernel-Fuzzer (zufaellige Op-Sequenzen + Baseline-Oracle + SMP-Kontention)"
fcheck "ipcfuzz : ALL PASS" "IPC-State-Machine-Fuzzer (nebenlaeufige Aktoren, KILL/Reload/MCS waehrend IPC, Queue-Oracle)"
check "caplk   : ALL PASS" "CAPS-Reader-Writer-Lock: parallele Cap-Lookups (zwei Kerne halten gleichzeitig den Read-Lock)"
check "domain  : ALL PASS" "Sicherheitsdomaenen: Domaenen-Policy-Oracle (Cap-Typen je Domaene + untrusted Domaenen isoliert)"
check "pdctl   : ALL PASS" "UserLand-Management: cap-gated SYS_PDCTL (PAUSE/RESUME/STOP, nur TrustedSas->UserLand)"
check "chan    : ALL PASS" "Paarweiser Treiber<->Backend-Kanal: unveraenderliche Bindung bei Backend-Erzeugung, 1:N, nur Partner"
check "rtc     : ALL PASS" "RTC-HardwareLand-Backend: generisches MMIO-Cap + vspace_map_device, echtes PL031 RTC_DR-Read ueber Kanal"
check "irq     : ALL PASS" "RTC-IRQ: IRQ-Cap + GIC-SPI-Routing + Deferred-IRQ-Zustellung als Notification an HardwareLand"
check "dma     : ALL PASS" "DMA-Capability: DmaCap hinter DmaEnforcer-Abstraktion, kernel-ausgeschnittene Region, Normal-NC-Mapping, EL0-Round-Trip + Kohaerenz, dma_audit"
check "pcie    : ALL PASS" "PCIe-ECAM-Enumeration: virtio-rng-pci gefunden, BAR-Zuweisung + Bus-Master-Enable, RID == SMMU-StreamID"
check "smmu    : ALL PASS" "SMMUv3-Bring-up hinter DmaEnforcer: Command-/Event-Queue + Stream-Tabelle, Default-Abort, CR0-Enable, CMD_SYNC-Round-Trip"
check "smmubind: ALL PASS" "SMMU-Bindung: enable_dma/disable_dma installiert STE->CD->Stage-1 (nur die DMA-Region), Revoke gibt Tabellen frei (balanciert)"
# 2026-08-17: die ARCH-NEUTRALE Gesundheitsaussage -- dieselbe Zeile faehrt die x86-Suite.
# **Gefunden durch die Gegenprobe zu genau dieser Zeile:** der Kernel gatterte korrekt
# (`bringup : offen: iohealth`, Watchdog gefeuert), und diese Suite meldete trotzdem
# `== ALL PASS ==` -- sie sah sich die neue Zeile nicht an. Dieselbe Form wie bei `root` eine
# Zeile weiter oben, und derselbe Grund: eine Zeile, die keine Suite liest, gattert nichts.
check "iohealth: ALL PASS" "Arch-neutrale IOMMU-Gesundheit: faults_empty zaehlt NUR mit belegtem Invalidierungs-Round-Trip -- eine tote Einheit meldet ebenfalls eine leere Warteschlange (dieselbe Form wie die leere Event-Queue ohne CD.R)"
check "smt     : ALL PASS" "Z6 Stufe 1: ein physischer Kern traegt hoechstens EINE logische CPU. Fail-closed -- eine unlesbare Topologie laesst nur den Bootkern zu (Unknown ist NICHT Single). ACHTUNG: unter QEMU ist die Geschwisterbeziehung EMULIERT (-smp cores=n,threads=2 gibt die CPUID-Sicht, die vCPUs sind gewoehnliche Wirtsthreads ohne geteilte Ausfuehrungseinheiten) -- geprueft ist die POLITIK, nie der KANAL, dieselbe Einschraenkung wie beim SMMU-Befund in ADR 0008. Hier ausserdem VAKUOeS: -smp 4 heisst threads=1, es gibt gar keine Geschwister. Dass die Politik BEISST, misst tools/smt-messen.sh mit cores=2,threads=2."
check "numa    : ALL PASS" "Z8/N1: NUMA-Topologie gelesen und vollstaendig. init=false heisst NIE nachgesehen (nicht flach), truncated=true heisst eine Tabelle mit Loechern, die vollstaendig AUSSIEHT -- das faellt durch, waehrend readable=false erlaubt ist (die meisten Maschinen haben keine SRAT: keine Aussage ist etwas anderes als eine falsche). ACHTUNG: QEMU emuliert die TOPOLOGIE, nicht die LATENZ -- geprueft ist WOHER eine Seite kam, nie ob es schneller ist. Hier ausserdem VAKUOeS: ohne -numa gibt es nur einen Knoten. Dass ein zweiter gelesen und benutzt wird, misst tools/numa-messen.sh."
check "virtiorng: ALL PASS" "virtio-rng-DMA: Geraet DMAt echte Zufallsbytes in die DmaCap-Region; zweistufig: Level-1-Software-Bounds weist Out-of-Window demonstrierbar ab, Level-2-SMMU als HW-Backstop (QEMU emuliert-Geraet-Bypass)"
check "dmatok  : ALL PASS" "Teardown-Token (ext-37): cap_delete allein legt still, unmappt und synchronisiert vor der Freigabe; unbestaetigte Stilllegung -> Region bleibt dauerhaft pending (Leck statt UAF), Audit-Code 7"
check "dmawin  : ALL PASS" "IOVA-Fenstergrenzen (ext-36b): 32-Bit-Geraet + erschoepftes Fenster werden laut abgewiesen (kein stilles Abschneiden, kein halb aufgebauter Kontext)"
check "dmagen  : ALL PASS" "Generische DMA-Infra (ext-24): Richtung/Kohaerenz als DmaCap-Attribute (richtungsminimales SMMU-AP), Multi-Region-Kontext, Stream-Gruppen, Scatter-Gather-Validierung, disjunkte Sub-Puffer (SG-Pfad)"
check "sasheap : ALL PASS" "Prozess-Heap (ext-25): echter Box/Vec/BTreeMap-Heap auf realen Physadressen; Hybrid-Allokator (Slabs+Bump) ueber Regionsliste + grow/shrink; Testcode 100% safe, unsafe nur in der Region-Runtime"
check "load    : ALL PASS" "Binary-Loader (ext-26): extern gebautes EL0-Programm aus dem Boot-Archiv geladen + ausgefuehrt (ELF64-Parse in Safe Rust, Segment-Kopie W^X an Link-VA, cap-gegatete isolierte PD + Endowment) -- Prozess NICHT im Kernel-Image"
check "sysload : ALL PASS" "Binary-Loader L2 (ext-26): Laden zur LAUFZEIT via SYS_LOAD-Syscall, cap-gegatet ueber Loader-Cap; Caller delegiert eigene Notification-Cap in die neue PD; ohne Loader-Cap -> ERR_BADCAP"
check "loadhw  : ALL PASS" "Binary-Loader L3 (ext-26) + ext-28: HardwareLand-Programm in vor-erstellte Backend-PD geladen, signalisiert Kanal; TrustedSAS (zertifiziertes svc-demo) laeuft GELADEN EL0-isoliert -- nur mit gueltigem Zertifikat + trust_audit==0"
check "loadstop: ALL PASS" "Binary-Loader L4 (ext-26): geladenen Prozess vollstaendig abgebaut (Thread+VSpace+geladene Segmente+Kstack+PD) -> Ressourcen-Baseline wiederhergestellt, kein Leck"
fcheck "loaderfuzz: ALL PASS" "Binary-Loader L5 (ext-26): Loader-Fuzzer -- fehlerhafte ELF-Varianten durch load_image alle abgelehnt (kein Crash, Parser forbid(unsafe_code)), Baseline unveraendert, loader_audit==0"
fcheck "certfuzz: ALL PASS" "TrustedSAS-Zertifikats-Fuzzer (ext-28, ADR 0014): mutierte/zufaellige Zertifikate + Identitaets-/Binary-Transplantation durch das verify_image-Gate alle abgelehnt (echtes Cert akzeptiert, kein Crash/OOB, no-alloc Krypto), trust_audit+loader_audit==0"
check "aggru   : ALL PASS" "Adversariale Testdienste (ext-27 T0): extern geladener UserLand-Aggressor -- Cap-Confusion (leerer Slot/falscher Typ/falsche Rechte) + Autoritaets-Eskalation (PDCTL/LOAD/KILL ohne Cap) alle als BADCAP/RIGHTS/BADSYS abgewiesen; Dienst signalisiert SUCCESS nur bei voller Abweisung; Audits==0"
check "intru   : ALL PASS" "Adversariale Testdienste (ext-27 T1): extern geladener UserLand-Intruder -- liest Kernel-RAM aus EL0 -> Translation-Fault (nicht in der isolierten VSpace gemappt) -> Kernel terminiert den Angreifer + laeuft weiter; PRE-Badge + el0_fault_count++ + Audits==0 (Hardware-Isolation)"
check "aggrh   : ALL PASS" "Adversariale Testdienste (ext-27 T2): extern geladenes HardwareLand-Backend als Aggressor -- KEINE Management-Autoritaet (PDCTL/LOAD/KILL -> BADCAP), nichts ausserhalb des eigenen Kanals; Cap-Confusion abgewiesen; meldet SUCCESS ueber den Kanal; Audits==0"
check "intrh   : ALL PASS" "Adversariale Testdienste (ext-27 T2): extern geladenes HardwareLand-Backend als Intruder -- Kernel-RAM-Zugriff aus EL0 faultet ebenso (Speicher-Isolation domaenen-unabhaengig); Kernel ueberlebt; PRE + el0_fault_count++ + Audits==0"
check "aggrt   : ALL PASS" "Adversariale Testdienste (ext-27 T3): extern geladener TrustedSAS-Aggressor (EL0-isoliert) -- Trust != Privileg: ohne tatsaechliche PdControl/Loader-Cap PDCTL/LOAD/KILL = BADCAP; Cap-Confusion abgewiesen; SUCCESS nur bei voller Abweisung; Audits==0"
check "intrt   : ALL PASS" "TrustedSAS-Zertifikats-Gate (ext-28, ADR 0014): UNZERTIFIZIERTES TrustedSAS wird abgewiesen -- intruder-t traegt absichtlich unsafe (nicht zertifizierbar) + liegt OHNE Zertifikat im Archiv -> verify_image lehnt das Laden mit Unverified ab; KEIN Thread/keine PD; Audits==0"
check "cross   : ALL PASS" "Adversariale Testdienste (ext-27 T4): Cross-Service-Matrix -- 3 extern geladene Angreifer DREIER Domaenen NEBENLAEUFIG (aggressor-u + aggressor-t melden unabhaengig SUCCESS, intruder-h faultet); gleichzeitige cross-domain Angreifer stoeren einander nicht; kernel-geschuetztes Canary unberuehrt; Audits==0"
fcheck "hwfuzz  : ALL PASS" "Domaenen/HW-Fuzzer: HW-/Management-Cap-Churn gegen Domaenen-Policy + CDT/VSpace-Oracle + Ressourcen-Baseline"
# --- Farbe / Cache-Partitionierung (A1, B-4.2, B-4.5) ------------------------------------------
#
# Diese drei Zeilen gab es auf aarch64 bis 2026-08-02 NICHT. `spawn_demo` setzte `COLOR_OK`,
# `STRIPE_ALLOC_OK` und `PPROBE_OK` hart auf `true` -- drei dauerhaft wahre Konjunkte in
# `all_done()`, keine Berichtszeile, kein Check hier. Die Abwesenheit war damit nicht bloss
# unbelegt, sie war unsichtbar; ein Ausfall der Faerbung auf ARM haette `== ALL PASS ==` gemeldet.
#
# Jetzt laufen die Tests (am ENDE der Kette, hinter cross/strand/loadstop) und melden. Geprueft
# wird hier deshalb auch die ANWESENHEIT der Zeile: fehlt sie, ist die Kette vorher
# stehengeblieben, und genau das soll nicht wieder in der Stille verschwinden.
if grep -q "^color   : SKIP" <<<"$OUT"; then
    echo "  SKIP (Plattform meldet weniger als 2 Seitenfarben): A1 Cache-Partitionierung"
elif grep -q "^color   : ALL PASS" <<<"$OUT"; then
    echo "  PASS: A1: zwei isolierte PDs teilen sich KEINE Cache-Farbe -- Region, Kernel-Stack und Seitentabellen jeder PD aus disjunkten Farbsaetzen; eine Region jenseits der Streifenbreite wird abgewiesen. Auf aarch64 (16 Farben) laeuft das seit 2026-08-02 zum ersten Mal -- die 16-Farben-Aufteilung ist genau der Fall, den die MASK_BITS-Verwechslung falsch machte"
else
    echo "  FAIL: A1: keine verwertbare 'color'-Zeile -- der Farbtest lief nicht oder fiel durch"
    fail=1
fi
if grep -q "^stripe  : SKIP" <<<"$OUT"; then
    echo "  SKIP (weniger als 2 Seitenfarben): B-4.2 Streifenvergabe"
elif grep -q "^stripe  : ALL PASS" <<<"$OUT"; then
    echo "  PASS: B-4.2: erschoepfte Farbpartitionierung scheitert SAUBER -- der 5. Streifenversuch wird abgewiesen statt den Satz der ersten PD still ein zweites Mal auszugeben; nach Freigabe wieder vergebbar (kein Leck)"
else
    echo "  FAIL: B-4.2: keine verwertbare 'stripe'-Zeile"
    fail=1
fi
# B-4.5: drei Ausgaenge, alle ausgesprochen. Unter TCG ist SKIP der erwartete Fall (kein echter
# Cache -> die Positivkontrolle kann nicht tragen), aber Aufbau, Farbwahl und Bilanz sind auch
# dort pruefbar -- und werden geprueft. Ein SKIP, der ueber den Aufbau nichts sagt, waere eine
# Aussage ueber gar nichts.
# E-Rest 3d: haengt die private Region einer isolierten PD noch an GiB 0?
#
# `SKIP` ist hier ein ehrliches Urteil und kein Durchwinken: auf einer Maschine ohne RAM
# oberhalb 4 GiB ist die Frage NICHT ENTSCHEIDBAR -- die Region kann dort gar nicht hoch liegen.
# Deshalb faehrt die RAM-Reihe (`die x86-Reihe`) den Fall, in dem sie es kann.
if grep -q "^isohigh : FAILURES" <<<"$OUT"; then
    echo "  FAIL: E-Rest 3d: die Region liegt nicht oberhalb 4 GiB, obwohl dort RAM ist --"
    echo "        die Identitaetsbindung ist zurueck (oder die Farbtrennung gab nach)."
    fail=1
elif grep -q "^isohigh : ALL PASS" <<<"$OUT"; then
    echo "  PASS: E-Rest 3d: die private Region einer isolierten PD liegt OBERHALB 4 GiB und die Farbtrennung haelt -- die Abbildung laeuft ueber ein VA-Fenster ausserhalb der Identitaetskarte statt identisch. Der gemessene Deckel von 504 gleichzeitigen isolierten PDs (Host-Test gib0_deckel_ist_eine_zahl) faellt damit"
elif grep -q "^isohigh : SKIP" <<<"$OUT"; then
    echo "  SKIP: E-Rest 3d (nicht entscheidbar auf dieser RAM-Groesse): $(grep -m1 -oE '^isohigh : SKIP -- [^(]*' <<<"$OUT")"
else
    echo "  FAIL: E-Rest 3d: die Zeile isohigh fehlt ganz -- der Pruefer ist nicht sprechfaehig."
    fail=1
fi
if grep -q "^pprobe  : FAILURES" <<<"$OUT"; then
    echo "  FAIL: B-4.5: $(grep -m1 '^pprobe  : FAILURES' <<<"$OUT")"
    fail=1
elif grep -q "^pprobe  : ALL PASS" <<<"$OUT"; then
    echo "  PASS: B-4.5: disjunkte Farbsaetze verdraengen einander messbar weniger -- die WIRKUNG von A1"
elif grep -q "^pprobe  : SKIP" <<<"$OUT"; then
    echo "  SKIP: B-4.5 (nicht entscheidbar, Grund in der Zeile): $(grep -m1 -oE '^pprobe  : SKIP -- [^.]*' <<<"$OUT")"
    if grep -q "^pprobe  : Opfer" <<<"$OUT"; then
        if grep -q "^pprobe  : Opfer.*farbtreu=1 bilanz=1" <<<"$OUT"; then
            echo "  PASS: B-4.5: der Aufbau ist trotzdem geprueft -- Farbwahl korrekt und JEDER Rueckspeicherblock wieder frei (region_fully_free je Block, nicht Summenvergleich)"
        else
            echo "  FAIL: B-4.5: SKIP, aber Aufbau nicht sauber: $(grep -m1 -oE 'farbtreu=[01] bilanz=[01]' <<<"$OUT")"
            fail=1
        fi
    fi
else
    echo "  FAIL: B-4.5: keine pprobe-Zeile im Protokoll -- die Testkette ist vor dem Prime+Probe stehengeblieben"
    fail=1
fi
# **Die MELDUNGEN der Kerne zaehlen, nicht das Wort.** Bis 2026-08-17 stand hier
# `grep -c "online"` ueber das ganze Protokoll -- also jede Zeile, in der das Teilwort irgendwo
# vorkommt. Die neue `smt`-Zeile enthaelt `online=8`, und die Zaehlung sprang auf 9/8: ein
# FEHLSCHLAG, ohne dass ein Kern gefehlt haette. Dieselbe Klasse wie „ein Sammler darf nur `$?`
# lesen": wer eine Tatsache aus freiem Text erschliesst, baut eine Formel, und jede Formel ist ein
# Loch. Gezaehlt wird jetzt das Muster, mit dem sich ein Kern tatsaechlich meldet.
#
# `<<<` statt `echo | grep`: eine Pipeline unter `set -o pipefail` meldet bei grosser Ausgabe
# SIGPIPE durch (Fallenliste, gemessen zwischen 66 und 70 KiB).
online=$(grep -cE "^core [0-9]+ +: online" <<<"$OUT")
[ "$online" -eq "$CORES" ] && echo "  PASS: alle $CORES Kerne online" || { echo "  FAIL: nur $online/$CORES Kerne (Muster '^core N : online') gemeldet"; fail=1; }

# Das Zeitlimit als zweiter, unabhaengiger Melder (s. boot_once).
if [ "$BOOT_TIMEOUTS" -gt 0 ]; then
    echo "  FAIL: $BOOT_TIMEOUTS Lauf/Laeufe rissen das Zeitlimit von ${SECONDS_RUN}s -- der Gast"
    echo "        hat sich nicht heruntergefahren. Das ist ein Haenger, unabhaengig davon, was im"
    echo "        Log steht (s. todo D6)."
    fail=1
else
    echo "  PASS: kein Lauf riss das Zeitlimit -- jeder Gast fuhr selbst herunter"
fi
if [ "$RUNS" -gt 1 ]; then
    if [ "$REPEAT_OK" = 1 ]; then
        echo "  PASS: D6/B-1.3: $RUNS von $RUNS Laeufen mit IDENTISCHER Ergebnissignatur -- reproduzierbar,"
        echo "        und zwar im Ergebnis, nicht bloss im Durchlaufen"
    else
        echo "  FAIL: D6/B-1.3: nur $REPEAT_DONE von $RUNS Laeufen mit identischer Ergebnissignatur."
        echo "        Eine Quote unter 100 % ist KEIN 'meistens gruen', sondern ein Nichtdeterminismus --"
        echo "        und bei abweichenden Signaturen weiss niemand, welcher Lauf die Wahrheit sagt."
        fail=1
    fi
fi

# **Bei einem Fehlschlag das volle Protokoll BEHALTEN** (D12, 2026-08-05).
#
# Die Wiederholungsmessung legt bei abweichender SIGNATUR ein Log ab -- aber nur dann. Faellt ein
# Lauf durch, waehrend die Signatur haelt (oder laeuft die Suite mit RUNS=1), blieb bisher nichts
# zurueck. Genau so gingen am 2026-08-04 zwei Fehlschlaege verloren.
# **Der Block prueft `LOG1` und kopierte `LOG`** -- eine Variable, die es in diesem Skript gar
# nicht gibt. Unter `set -u` bricht er ab („LOG ist nicht gesetzt"), und zwar GENAU im Fehlerfall:
# der Code, der geschrieben wurde, damit keine Fehlschlagsprotokolle mehr verlorengehen, verlor
# sie selbst. Gefunden am 2026-08-10, als ein aarch64-Fehlschlag ausgewertet werden sollte.
if [ "$fail" != 0 ] && [ -s "${LOG1:-}" ]; then
    mkdir -p build/diag
    ZIEL="build/diag/ABWEICHUNG-$(date +%Y%m%d-%H%M%S).log"
    cp -f "$LOG1" "$ZIEL" 2>/dev/null && echo "  (volles Log: $ZIEL)"
fi
echo "== $([ $fail -eq 0 ] && echo 'ALL PASS' || echo 'FAILURES') =="
exit $fail
