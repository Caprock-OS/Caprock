#!/usr/bin/env bash
# LXPD-End-to-End im GAST (`tools/lxpd-e2e.sh --mit-lader | --ohne-lader`).
#
# Beweist (oder grenzt ein), was der Laufzeitpfad `SYS_LOAD_IMAGE = 36` im Gast leistet:
# Der Dienst `lxpdrv` (programs/lxpd-runtime, TrustedSAS-PD aus dem Boot-Archiv) liest ein
# Treiber-Image von Platte (GPT + LXPD-Partition + LXIMG2-Verzeichnis, s.
# tools/lxpd-e2e-platte.py), verifiziert es (SHA-256, Zeugen, Manifest, ELF-Form — niemals
# laden ohne Pruefung) und stoesst die Instanziierung per `LOAD_IMAGE` an. Erfolg ist die
# Kernel-Zeile `lxpdimg : [8]... gestartet` bei sonst gruener Suite.
#
# D5-INVARIANTE (Startmenge: Manifestposition = Archivposition — hier 7=7, kein Zaehler weniger):
# Der Root-Task spricht die uebrige Startmenge ueber einen ARCHIVINDEX an (`SYS_LOAD`),
# bekommt ihre Groesse aber aus dem MANIFEST. Beide Zahlen meinen nur dann dasselbe, wenn
# die Manifest-Eintraege 1..7 genau auf den Archivpositionen 1..7 liegen (gleiche
# `program_id` je Position). Der Kernel prueft das fail-closed (`StartSetNotPrefix`,
# `kernel/src/loader.rs`): ein 7-Eintrag-Manifest mit nur 6 Archiv-Programmen bootet nicht
# in den falschen Dienst, sondern meldet `root : FAILURES (StartSetNotPrefix)` — per Design,
# s. `tests/lxpd-boot-qemu/BEFUND.md` B5. Deshalb baut `build_archive()` unten BEIDE Seiten
# aus EINER Stelle (`tools/sign_manifest.py --entry …` + `tools/mkarchive.py …`,
# Zertifikate fuer die TrustedSAS-Eintraege 1/init, 4/fs, 7/lxpdrv) — Muster aus
# `test-qemu-x86-load.sh` (dort 6=6, hier 7=7: Eintraege 1..6 byte-identisch zur Lade-Suite,
# dazu 7:lxpdrv mit Zertifikat, `shared` auf Dienst 3). Die Platte traegt genau EIN
# Treiber-Bild (`hello.elf`, ausserhalb der Startmenge — D5 gilt nur fuer den Boot, der
# Laufzeit-Treiber gehoert nicht dazu, s. `boot_arg`-Doku in `kernel/src/loader.rs`):
# Verbraucher ist `suchen(index 0)` in `programs/lxpd-runtime`, der nur das erste
# LXIMG2-Verzeichnis liest — sieben Platten-Bilder waeren sechs ohne Leser. Die Zahl steht
# doppelt als Check im Skript (`archive : 7 Modul(e)`) und trocken ohne QEMU unter
# `--selbsttest`.
#
# Aufbau (gegen `test-qemu-x86-load.sh` gelesen — LESEN, nicht geaendert):
#   * Platte: DIESELBE `mkgpt.py`-Zeile wie die Lade-Suite (P1 FAT + HELLO.TXT, P2 roh mit
#     Magie, Checkpoint-Sektor frei) — plus Typ-Patch von P2 auf die LXPD-GUID und LXIMG2
#     in freien P2-Sektoren. SCAN zaehlt weiter 2, `part` bleibt scharf.
#   * QEMU-Flags: dieselbe Maschine, dieselben Geraete, ein Boot (keine Z4-Kette — sie ist
#     nicht die Frage dieses Laufs).
#
# Modi (zwei Laeufe, ein Skript — die Frage ist die Loader-Autoritaet):
#   --mit-lader   Dienst-Manifest MIT `loader`-Bit. Erwartung: init's Slot-0-Angebot
#                 kollidiert damit (`load_by_index`: fail-closed, kein Start), das Root-Badge
#                 traegt Bit 7 (init meldet Index 6 als gescheitert), KEIN `lxpdimg`, KEIN
#                 Dienst-Bit. Suite sonst gruen. Belegt die Sperre mit Zeilen statt Worten.
#   --ohne-lader  Dienst-Manifest OHNE `loader` (laedt sauber, liest, prueft, ruft an).
#                 Erwartung: Bit 40 im Root-Badge (verifiziert + angestossen), KEIN Bit 41,
#                 KEIN `lxpdimg` (der Dispatch weist ohne Loader-Cap ab), Suite gruen inkl.
#                 `SELFTEST COMPLETE`. Belegt den Gast-Leseweg + das Kernel-Gatter.
#
# Gelänge `lxpdimg : [8]lxpdrv-test... gestartet` (Bit 41), waere das E2E bewiesen — dann
# muesste der Dienst eine Loader-Cap halten, was heute keine Startmenge hergibt (s. Modi).
# Was nicht gruen ist, wird begruendet, nicht weggelassen. Kein Commit aus diesem Skript.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

MODUS="${1:-}"
RAM="${2:-512M}"
SECONDS_RUN="${3:-180}"

# Trockene D5-Zaehllung ohne QEMU: Manifest-Eintraege == Archiv-Programme == 7,
# IDs beidseits 1..7, Platten-Aufruf (EIN Bild) vorhanden, Platten-Selbsttest gruen.
selbsttest() {
    echo "== LXPD-E2E-Selbsttest (trocken, ohne QEMU) =="
    fail=0
    # `--entry "[0-9]:` (mit Ziffer + Doppelpunkt) statt bloss `--entry "`: sonst
    # zaehlt diese Funktion ihre eigenen grep-Muster mit (Selbstbeobachtung).
    n_man="$(grep -c -- '--entry "[0-9]:' "$0" || true)"
    n_arc="$(grep -cE '^[[:space:]]*"[0-9]+:[^"]*\$PROG/' "$0" || true)"
    echo "  Manifest-Eintraege: $n_man, Archiv-Programme: $n_arc (erwartet 7=7, D5)"
    [ "$n_man" = 7 ] || { echo "  FAIL: Manifest-Zaehllung"; fail=1; }
    [ "$n_arc" = 7 ] || { echo "  FAIL: Archiv-Zaehllung"; fail=1; }
    man_ids="$(grep -o -- '--entry "[0-9]:' "$0" | grep -o '[0-9]' | tr '\n' ' ')"
    arc_ids="$(grep -oE '^[[:space:]]*"[0-9]+:' "$0" | grep -oE '[0-9]+' | tr '\n' ' ')"
    echo "  Manifest-IDs: ${man_ids:-keine} / Archiv-IDs: ${arc_ids:-keine} (erwartet 1..7 beidseits)"
    [ "$man_ids" = "1 2 3 4 5 6 7 " ] || { echo "  FAIL: Manifest-Folge"; fail=1; }
    [ "$arc_ids" = "1 2 3 4 5 6 7 " ] || { echo "  FAIL: Archiv-Folge"; fail=1; }
    if grep -q 'lxpd-e2e-platte.py --disk.*--image.*--lba' "$0"; then
        echo "  PASS: Platten-Aufruf (EIN Bild, ausserhalb der Startmenge)"
    else echo "  FAIL: Platten-Aufruf fehlt"; fail=1; fi
    if grep -q 'archive : 7 Modul(e)' "$0"; then
        echo "  PASS: Archiv-Check 7 Modul(e) im Skript"
    else echo "  FAIL: Archiv-Check fehlt"; fail=1; fi
    echo "== Platten-Selbsttest (Temp, ohne QEMU) =="
    python3 tools/lxpd-e2e-platte.py --selbsttest || fail=1
    if [ "$fail" -eq 0 ]; then echo "== LXPD-E2E-SELBSTTEST: ALL PASS ==";
    else echo "== LXPD-E2E-SELBSTTEST: FAIL =="; fi
    return "$fail"
}

case "$MODUS" in
  --mit-lader) CAPS="loader,ntfn,ep,shared"; E2E_NAME="mit-lader" ;;
  --ohne-lader) CAPS="ntfn,ep,shared"; E2E_NAME="ohne-lader" ;;
  --help|-h) echo "Aufruf: $0 --mit-lader|--ohne-lader [RAM] [SEKUNDEN]";
             echo "  --selbsttest: trockene D5-Zaehllung + Platten-Selbsttest, ohne QEMU.";
             exit 0 ;;
  --selbsttest) selbsttest; exit $? ;;
  *) echo "Aufruf: $0 --mit-lader|--ohne-lader [RAM] [SEKUNDEN]" >&2; exit 2 ;;
esac

TS="$(date +%Y%m%d-%H%M%S)"
LOGDIR="build/diag"
mkdir -p "$LOGDIR"
# E2E_TCG=1 erzwingt TCG (Diskriminator KVM-Flake vs. Baum-Bruch — s. Bericht).
if [ -n "${E2E_TCG:-}" ]; then
    E2E_NAME="$E2E_NAME-tcg"
fi
LOG="$LOGDIR/lxpd-e2e-$E2E_NAME-$TS.log"
QEMU_ERR="$LOGDIR/lxpd-e2e-$E2E_NAME-$TS.qemu-err"
echo "== LXPD-E2E ($E2E_NAME) — Log: $LOG =="

if [ -n "${E2E_TCG:-}" ]; then
    ACCEL=(-cpu Skylake-Client)
    echo "== Beschleunigung: TCG (erzwungen via E2E_TCG) =="
elif [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
    echo "== Beschleunigung: KVM (-cpu host) =="
else
    ACCEL=(-cpu Skylake-Client)
    echo "== Beschleunigung: TCG (kein /dev/kvm) =="
fi

KELF=build/target/x86_64-unknown-none/release/caprock-kernel
PROG=programs/build/target/x86_64-caprock-user/release
MANKEY=keys/manifest-test.manifest.ed25519
TRUSTKEY=keys/trusted-test.ed25519

python3 -c "import cryptography" 2>/dev/null || {
    echo "== FEHLT: Python-Paket 'cryptography' =="; exit 2; }

echo "== Schluessel sicherstellen (dev-keys.sh) =="
DEVKEYS_OUT="$(tools/dev-keys.sh)" || exit 2
case "$DEVKEYS_OUT" in *DEVKEYS_REBUILD=1*)
    echo "  Schluessel neu erzeugt -> Kernel wird mit frischer Key-DB gebaut" ;;
esac

echo "== build (Kernel, --features selftest) =="
./build-x86.sh --features selftest >/dev/null 2>&1 || { echo "BUILD FAILED (Kernel)"; exit 1; }

echo "== build (Programme, x86_64-caprock-user) =="
( cd programs && cargo build --release --target x86_64-caprock-user.json ) \
    >/dev/null 2>&1 || { echo "BUILD FAILED (Programme)"; exit 1; }

echo "== build (Dienst-PD lxpdrv, standalone) =="
# Das Linkerskript loest ueber `programs/lxpd-runtime/user-x86.ld` (Symlink auf das EINE
# Skript) auf — rustc-CWD ist die Workspace-Wurzel der gebauten Crate. Genau EIN -T.
( cd programs && cargo build --release --target x86_64-caprock-user.json \
    --manifest-path lxpd-runtime/Cargo.toml ) \
    >/dev/null 2>&1 || { echo "BUILD FAILED (lxpdrv)"; exit 1; }
[ -f "$PROG/lxpdrv.elf" ] || { echo "FEHLT: $PROG/lxpdrv.elf"; exit 1; }

echo "== zertifizieren (TrustedSAS: lxpdrv, program-id 7) =="
mkdir -p certs build
# TrustedSAS-Schlüssel ist oben vor dem Build über `tools/dev-keys.sh` sichergestellt
# (ensure + Übereinstimmungs-Prüfung + Beiseitelegen bei Mismatch).
python3 tools/sign_trusted.py --crate programs/lxpd-runtime --elf "$PROG/lxpdrv.elf" \
    --program-id 7 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/lxpdrv-x86.cert >/dev/null 2>&1 || { echo "  FEHLER: lxpdrv liess sich nicht zertifizieren"; exit 2; }

echo "== Platte (Lade-Suite-Layout + LXPD-Typ + LXIMG2) =="
BLK_IMG="$(mktemp)"
BLK_SECTORS=32768
BLK_PART1_LBA=34
BLK_PART1_SECTORS=19967
python3 tools/mkgpt.py "$BLK_IMG" --sectors "$BLK_SECTORS" \
    --part "$BLK_PART1_LBA:20000" --part 20001:32700 --magic-at 20001 \
    --fat16 "$BLK_PART1_LBA:20000" --file "HELLO.TXT=CAPROCKS-DATEIINHALT" \
    || { echo "  FEHLER: GPT-Abbild liess sich nicht bauen"; exit 2; }
# Ziel-Image auf Platte: hello.elf (5016 B, passt ins 8-KiB-Shared-Fenster; ELF-Form genug —
# der Kernel verlangt ELF + Manifest-Hash, keinen Container).
python3 tools/lxpd-e2e-platte.py --disk "$BLK_IMG" --image "$PROG/hello.elf" --lba 21000 \
    || { echo "  FEHLER: LXPD-Bereich liess sich nicht bauen"; exit 2; }

echo "== Boot-Archiv + Manifest (Eintraege 1..6 wie Lade-Suite, dazu 7:lxpdrv) =="
build_archive() {
    python3 tools/sign_manifest.py --kernel "$2" --key "$MANKEY" --manifest-version "$3" \
        --out build/system.manifest \
        --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
        --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
        --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1042" \
        --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
        --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
        --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
        --entry "7:lxpdrv:0:1:$PROG/lxpdrv.elf:$CAPS::1::any:0::3" \
        >/dev/null 2>&1 || return 1
    python3 tools/mkarchive.py "$1" --system-manifest build/system.manifest \
        "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
        "2:hello:2:1:$PROG/hello.elf" \
        "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
        "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
        "5:virtio-net:1:1:$PROG/virtio-net.elf" \
        "6:wasmhost:2:1:$PROG/wasmhost.elf" \
        "7:lxpdrv:0:1:$PROG/lxpdrv.elf::certs/lxpdrv-x86.cert" >/dev/null 2>&1
}
python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$PROG/init.elf" \
    --program-id 1 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/init-x86.cert >/dev/null 2>&1 || { echo "  FEHLER: init liess sich nicht zertifizieren"; exit 2; }
python3 tools/sign_trusted.py --crate programs/trusted/fs --elf "$PROG/fs.elf" \
    --program-id 4 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/fs-x86.cert >/dev/null 2>&1 || { echo "  FEHLER: fs liess sich nicht zertifizieren"; exit 2; }
build_archive build/boot-archive-e2e.bin "$KELF" 1 || { echo "  FEHLER: Archiv/Manifest"; exit 2; }

echo "== boot ($SECONDS_RUN s, $RAM) =="
# Flags wie test-qemu-x86-load.sh (ein Boot, keine Z4-Kette).
timeout "$SECONDS_RUN" qemu-system-x86_64 \
    -kernel "$KELF.mb32" -m "$RAM" -smp 4 "${ACCEL[@]}" \
    -machine q35,kernel-irqchip=split -device intel-iommu,caching-mode=on,intremap=on \
    -device virtio-rng-pci,disable-legacy=on,iommu_platform=on \
    -drive if=none,id=blk0,format=raw,file="$BLK_IMG" \
    -device virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on \
    -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
    -netdev user,id=n0,restrict=on -initrd build/boot-archive-e2e.bin \
    -nographic -serial file:"$LOG" -no-reboot \
    </dev/null >/dev/null 2>"$QEMU_ERR" || true
rm -f "$BLK_IMG"

OUT="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$LOG" 2>/dev/null)"
if [ -z "$OUT" ]; then
    echo "== KEIN OUTPUT — Aufbauproblem, kein Testergebnis =="
    [ -s "$QEMU_ERR" ] && { echo "-- QEMU sagt dazu: --"; sed -n '1,20p' "$QEMU_ERR"; }
    exit 2
fi
echo "$OUT" | grep -E "^(pdcolor|ladepol)" || true
echo "$OUT" | grep -E "^(mbi|mbmod|archive|manifest|clientn|root|devassign|devsel|dmaiso|drv|blkdev|part|fs|wasm|bootckpt|irqmsi|tls|lxpdimg|lxpddrv|loader) *:" || true
echo "$OUT" | grep -E "^el0-trap" || true

echo "== checks =="
fail=0
check() { if grep -q "$1" <<<"$OUT"; then echo "  PASS: $2"; else echo "  FAIL: $2"; fail=1; fi; }

# Sprechprobe des Pruefers (gegen SIGPIPE/pipefail, s. Lade-Suite).
gross="MARKER-VORHANDEN
$(head -c 262144 /dev/zero | tr '\0' 'x')"
alt_out="$OUT"; OUT="$gross"
da="$(check "MARKER-VORHANDEN" "selbsttest")"; weg="$(check "MARKER-FEHLT-ABSICHTLICH" "selbsttest")"
OUT="$alt_out"
case "$da" in *PASS*) ;; *) echo "== PRUEFER DEFEKT =="; exit 2 ;; esac
case "$weg" in *FAIL*) ;; *) echo "== PRUEFER DEFEKT =="; exit 2 ;; esac
echo "  Pruefer-Sprechprobe: beide Richtungen an 256 KiB Eingabe"

check "mbi     : 1 Modul(e)" "A-1.1: Startmenge als Multiboot-Modul"
check "mbmod   : ALL PASS" "A-1.1: Modulbereiche vor Allokation ausgeschnitten"
check "archive : 7 Modul(e)" "A-1.1/A-1.5: Archiv mit Dienst-Modul parst"
check "pdcolor : ALL PASS" "A1: Farbstreifen"
check "ladepol : ALL PASS" "Z11c: Manifest-Politik angewandt"
check "kstack  : ALL PASS" "C4: Stack-Wasserstand"
check "verif   : ALL PASS" "C8: Verifiziererthread"
check "verif   : Absage gefahren" "C8 (a): Schranke gefahren"
check "kstack  : Wasserstand VERIFIZIERER" "C8 (b): Verifizierer-Stack gemessen"
check "sweep   : ALL PASS" "C7: Mangel-Sweep"
check "LADEN=[1-9][0-9]* (Boot-Archiv da" "C7: echter Ladepfad provoziert"
check "sweep   : Bilanz .* VSpaces=0 · PD-Slots=0 · Thread-Slots=0 · Seitentabellen-Rahmen=0" "C7: nichts liegen gelassen"
check "ist     : ALL PASS" "IST-Stacks"
check "#PF(14)=0" "#PF ohne IST"
check "ist     : Ring-3-Rueckkehr je Kern" "Ring-3 je Kern"
check "wasm    : ALL PASS" "Z15/W1: WASM-PD"
check "manifest: ALL PASS" "A-1.2..A-1.4: System-Manifest"
check "manifest:   \[1\]init .* ROOT" "A-1.4: Politikfelder"
check "root    : ALL PASS (A-2.1" "A-2.1: Root-Task laeuft + laedt nach"
check "root    : ALL PASS (Startprogramm" "A-2.1: Root-Task aus Manifest"
check "cdelete : ALL PASS" "A-3.1: SYS_CDELETE beide Ausgaenge"
check "drv     : ALL PASS" "A-5.1: Treiber als Dienst"
check "dmaiso  : ALL PASS" "A-5.4: DMA-Isolation"
check "devsel  : ALL PASS" "A-5.3: Manifest waehlt Geraet"
if grep -q "drv     : Anfrage 1 an v1: Status=0 " <<<"$OUT"; then
    echo "  PASS: A-5.1: Dienst beantwortet Anfrage"; else echo "  FAIL: A-5.1: erste Anfrage"; fail=1; fi
if grep -q "drv     : Austausch: Ergebnis=0 .*v1 meldete bereit=1 v2 meldete bereit=1" <<<"$OUT"; then
    echo "  PASS: A-5.1/A-4.1: Austausch ohne Luecke"; else echo "  FAIL: A-5.1/A-4.1: Austausch"; fail=1; fi
if grep -q "drv     : Anfrage 2 an v2: Status=0 " <<<"$OUT"; then
    echo "  PASS: A-5.1: neue Fassung erbt Region"; else echo "  FAIL: A-5.1: neue Fassung"; fail=1; fi
check "blkdev  : ALL PASS" "A-6.1: Blockdienst traegt"
if grep -q "blkdev  : .*Rueckgelesen=0x534b434f52504143 (erwartet 0x534b434f52504143)" <<<"$OUT"; then
    echo "  PASS: A-6.1: geschrieben + zurueckgelesen"; else echo "  FAIL: A-6.1: Ruecklesen"; fail=1; fi
check "part    : ALL PASS" "A-6.2: GPT im Blockdienst"
if grep -q "part    : .*erste Partition LBA $BLK_PART1_LBA ueber $BLK_PART1_SECTORS Sektoren" <<<"$OUT"; then
    echo "  PASS: A-6.2: erste Partition wie geschrieben"; else echo "  FAIL: A-6.2: Partition"; fail=1; fi
check "fs      : ALL PASS" "A-6.3: Dateisystem-PD"
if grep -q "fs      : Status=0 .*Groesse=20 erste acht Byte=0x534b434f52504143" <<<"$OUT"; then
    echo "  PASS: A-6.3: Datei gelesen"; else echo "  FAIL: A-6.3: Dateiinhalt"; fail=1; fi
check "ckptcut : ALL PASS" "Z4d Stufe 1: Schnitt"
check "arena  : ALL PASS" "K1b: Thread-Stapel aus einer Cap"
check "dmapool : ALL PASS" "C2: DMA-Pool ist Argument"
check "irqmsi  : ALL PASS" "Stufe B: Interrupt-Autoritaet"
if grep -q "^hiiso   : FAILURES" <<<"$OUT"; then
    echo "  FAIL: E-Rest 3 hiiso"; fail=1
elif grep -q "^hiiso   : ALL PASS" <<<"$OUT"; then
    echo "  PASS: E-Rest 3: privates Fenster oberhalb 4 GiB"
elif grep -q "^hiiso   : SKIP" <<<"$OUT"; then
    echo "  SKIP (keine BARs oberhalb 4 GiB bei $RAM): hiiso"
else
    echo "  FAIL: E-Rest 3: keine hiiso-Zeile"; fail=1
fi
check "SELFTEST COMPLETE" "sauberes system_off statt Timeout"

echo "== E2E-Auswertung ($E2E_NAME) =="
BADGE_HEX="$(grep -m1 -oE '^root    : Notification-Badge 0x[0-9a-f]+' <<<"$OUT" | grep -oE '0x[0-9a-f]+' || echo 0x0)"
BADGE="$((BADGE_HEX))" 2>/dev/null || BADGE=0
printf '  Root-Badge: %s\n' "$BADGE_HEX"
LXPDIMG="$(grep -c '^lxpdimg :' <<<"$OUT" || true)"
echo "  lxpdimg-Zeilen: $LXPDIMG"
if [ "$E2E_NAME" = "mit-lader" ]; then
    # Bit 7 = init meldet Archiv-Index 6 (Dienst) als gescheitert; Bit 40/41 duerfen NICHT stehen.
    if [ $((BADGE & 0x80)) -ne 0 ]; then echo "  E2E-NACHWEIS: Bit 7 im Root-Badge — init lud Index 6 nicht (Slot-0-Kollision)"; else echo "  E2E-ABWEICHUNG: Bit 7 fehlt — keine Kollision sichtbar?"; fail=1; fi
    if [ $((BADGE & 0x10000000000)) -eq 0 ]; then echo "  E2E-NACHWEIS: Bit 40 fehlt — Dienst lief nie (erwartet bei Kollision)"; else echo "  E2E-ABWEICHUNG: Bit 40 steht — Dienst lief?"; fail=1; fi
    if [ "$LXPDIMG" -eq 0 ]; then echo "  E2E-NACHWEIS: keine lxpdimg-Zeile — LOAD_IMAGE nie erreicht"; else echo "  E2E-ABWEICHUNG: lxpdimg-Zeilen da"; grep '^lxpdimg :' <<<"$OUT" | head -5; fail=1; fi
else
    if [ $((BADGE & 0x10000000000)) -ne 0 ]; then echo "  E2E-NACHWEIS: Bit 40 steht — Dienst las + pruefte + stiess an"; else echo "  E2E-ABWEICHUNG: Bit 40 fehlt — Dienst kam nicht bis zum Anstoss"; fail=1; fi
    if [ $((BADGE & 0x20000000000)) -eq 0 ]; then echo "  E2E-NACHWEIS: Bit 41 fehlt — Kernel lud nicht (keine Loader-Cap)"; else echo "  E2E-ERFOLG?: Bit 41 steht — LOAD_IMAGE meldete OK"; grep '^lxpdimg :' <<<"$OUT" | head -5; fi
    if [ "$LXPDIMG" -eq 0 ]; then echo "  E2E-NACHWEIS: keine lxpdimg-Zeile — Dispatch wies ohne Loader-Cap ab"; else echo "  E2E-HINWEIS: lxpdimg-Zeilen:"; grep '^lxpdimg :' <<<"$OUT" | head -5; fi
fi

# Abnahme-Artefakte (Testschluessel) zuruecksetzen: sie sind maschinenlokal, nicht committbar.
git checkout -- kernel/src/manifest_keys.rs kernel/src/trusted_keys.rs 2>/dev/null || true
echo "== Log: $LOG (QEMU-Stderr: $QEMU_ERR) =="
if [ "$fail" -eq 0 ]; then echo "== LXPD-E2E ($E2E_NAME): ERWARTUNG ERFUELLT =="; else echo "== LXPD-E2E ($E2E_NAME): ABWEICHUNG (s. FAIL oben) =="; fi
exit "$fail"
