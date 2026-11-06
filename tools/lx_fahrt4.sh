#!/usr/bin/env bash
# Fahrt 4 — Erster Treiber-PD-Start im Gast (`lxpddrv : ... gestartet=1`)
# oder die naechste benannte Absage.
#
# Rezept: Manifest-Eintrag 7 traegt sha256(bind_elf-Image), Archiv-Programm 7
# ist der v1-Container (D5: IDs 1..7 beidseits), ISO-Modul 1 ist das
# bind_elf-Image. Weicht die Archivkopie vom Manifest-Hash ab, greift der
# Spannen-Rueckfall (kernel/src/loader.rs, `boot_lxpd_treiber`); traegt die
# Spanne die geforderten Bytes, laedt sie (`quelle=Bootloader-Modul`).
#
# Eintraege 1-6 + Werkzeugaufrufe sind aus test-qemu-x86-load.sh `build_archive`
# nachgebaut (gelesen, nicht geaendert). QEMU faehrt per GRUB-ISO (-cdrom),
# KEIN -kernel/-initrd.
#
# Aufruf:  bash tools/lx_fahrt4.sh [VERSUCH-NR]   (Vorgabe: 1)
# Log:     build/diag/lx-fahrt4-N.log (dieses Skript schreibt dorthin via tee).
# Ergebnis: Marker-Zitate am Ende; Exit 0 = gestartet=1, 1 = benannte Absage,
#           2 = Aufbaufehler (kein Testergebnis).

if [ -z "${BASH_VERSION:-}" ]; then
    echo "FEHLER: dieses Skript braucht bash, nicht sh/dash." >&2
    exit 2
fi
set -uo pipefail

N="${1:-1}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DIAG="build/diag"
mkdir -p "$DIAG"
LOG="$DIAG/lx-fahrt4-$N.log"
exec > >(tee "$LOG") 2>&1

echo "== Fahrt 4, Versuch $N ($(date -u '+%Y-%m-%d %H:%M UTC')) =="

KELF="build/target/x86_64-unknown-none/release/caprock-kernel"
PROG="programs/build/target/x86_64-caprock-user/release"
MANKEY="keys/manifest-test.manifest.ed25519"
TRUSTKEY="keys/trusted-test.ed25519"

# --- Schritt 1: Treiber-Bild (NUR LESEN, kopieren) ---------------------------------
TREIBER_SRC="tests/lxpd-boot-qemu/lxpd-test.img"
CONTAINER_SRC="tests/lxpd-boot-qemu/lxpd-test.lxpd"
TREIBER="$DIAG/lxpd-fahrt4-treiber.img"
cp "$TREIBER_SRC" "$TREIBER"
echo "== Schritt 1: Treiber-Bild =="
ls -la "$TREIBER_SRC" "$CONTAINER_SRC"
sha256sum "$TREIBER"
[ "$(stat -c %s "$TREIBER_SRC")" = 8811 ] \
    && echo "  Groesse 8811 B wie erwartet" \
    || { echo "  FEHLER: unerwartete Groesse (Aufbau, kein Testergebnis)"; exit 2; }

# --- Schluessel (ensure; aendert nichts, solange keys/ liegt) -----------------------
echo "== Schluessel sicherstellen (dev-keys.sh) =="
DEVKEYS_OUT="$(tools/dev-keys.sh)" || exit 2
case "$DEVKEYS_OUT" in *DEVKEYS_REBUILD=1*)
    echo "  Schluessel neu erzeugt -> Kernel wird mit frischer Key-DB gebaut" ;;
esac

# --- Kernel + Programme (Manifest bindet an DIESEN Kernel-Hash) ---------------------
echo "== build (Kernel, --features selftest) =="
./build-x86.sh --features selftest >/dev/null 2>&1 || { echo "BUILD FAILED (Kernel)"; exit 2; }
echo "  Kernel ok: $(ls -la "$KELF.mb32" | awk '{print $5, $6, $7, $8}')"

echo "== build (Programme, x86_64-caprock-user) =="
( cd programs && rustup run nightly cargo build --release --target x86_64-caprock-user.json ) \
    >/dev/null 2>&1 || { echo "BUILD FAILED (Programme)"; exit 2; }
echo "  Programme ok"

echo "== zertifizieren (TrustedSAS: init, fs) =="
mkdir -p certs
python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$PROG/init.elf" \
    --program-id 1 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/init-x86.cert >/dev/null 2>&1 || { echo "FEHLER: init-Zertifikat"; exit 2; }
python3 tools/sign_trusted.py --crate programs/trusted/fs --elf "$PROG/fs.elf" \
    --program-id 4 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/fs-x86.cert >/dev/null 2>&1 || { echo "FEHLER: fs-Zertifikat"; exit 2; }
echo "  Zertifikate ok"

# --- Platte (wie test-qemu-x86-load.sh) ----------------------------------------------
BLK_IMG="$DIAG/lx-fahrt4-blk.img"
BLK_SECTORS=32768
BLK_PART1_LBA=34
BLK_PART1_SECTORS=19967
echo "== Platte bauen (mkgpt.py) =="
python3 tools/mkgpt.py "$BLK_IMG" --sectors "$BLK_SECTORS" \
    --part "$BLK_PART1_LBA:20000" --part 20001:32700 --magic-at 20001 \
    --fat16 "$BLK_PART1_LBA:20000" --file "HELLO.TXT=CAPROCKS-DATEIINHALT" \
    || { echo "FEHLER: GPT-Abbild"; exit 2; }
echo "  Platte ok"

# --- Schritt 2+3: Manifest (7 Eintraege) + Archiv (7 Programme) -----------------------
# Eintraege 1-6 wie Standard-Ladesuite; Eintrag 7: pid 7, BLOB = bind_elf-Image
# (dessen sha256 wird der Manifest-Hash), dom=1 HardwareLand, caps 0x005c
# (mmio,dma,ntfn,ep wie virtio-blk), Politik leer wie virtio-blk, Selektor
# leer = beliebiges freies Geraet.
ARCHIV="$DIAG/lx-fahrt4-archiv.bin"
SEL_BLK="vendor=1af4,device=1042"
echo "== Schritt 2+3: Manifest + Archiv (7/7) =="
python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
    --out "$DIAG/lx-fahrt4-system.manifest" \
    --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
    --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
    --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:$SEL_BLK" \
    --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
    --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
    --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
    --entry "7:lxpd-test:1:1:$TREIBER:mmio,dma,ntfn,ep::1::any:0" \
    || { echo "FEHLER: Manifest"; exit 2; }
python3 tools/mkarchive.py "$ARCHIV" --system-manifest "$DIAG/lx-fahrt4-system.manifest" \
    "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
    "2:hello:2:1:$PROG/hello.elf" \
    "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
    "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
    "5:virtio-net:1:1:$PROG/virtio-net.elf" \
    "6:wasmhost:2:1:$PROG/wasmhost.elf" \
    "7:lxpd-test:1:1:$CONTAINER_SRC" \
    || { echo "FEHLER: Archiv"; exit 2; }

# --- Schritt 4: ISO (2 Module) ---------------------------------------------------------
ISO="$DIAG/lx-fahrt4.iso"
echo "== Schritt 4: ISO (2 Module) =="
bash tools/lx_fahrt4_iso.sh --kernel "$KELF.mb32" --archive "$ARCHIV" \
    --driver "$TREIBER" --out "$ISO" || exit 2

# --- Schritt 5: QEMU-Boot (GRUB, -cdrom, KEIN -kernel/-initrd) --------------------------
SERLOG="$DIAG/lx-fahrt4-$N-seriell.log"
QEMU_ERR="$DIAG/lx-fahrt4-$N-qemu-err.log"
echo "== Schritt 5: QEMU-Boot (180 s, KVM, 512M, smp 4, q35+split+iommu/intremap) =="
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
    echo "  Beschleunigung: KVM (-cpu host)"
else
    ACCEL=(-cpu Skylake-Client)
    echo "  Beschleunigung: TCG (kein /dev/kvm)"
fi
timeout 180 qemu-system-x86_64 \
    -cdrom "$ISO" -boot order=d,menu=off -m 512M -smp 4 "${ACCEL[@]}" \
    -machine q35,kernel-irqchip=split -device intel-iommu,caching-mode=on,intremap=on \
    -device virtio-rng-pci,disable-legacy=on,iommu_platform=on \
    -drive if=none,id=blk0,format=raw,file="$BLK_IMG" \
    -device virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on \
    -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
    -netdev user,id=n0,restrict=on \
    -nographic -serial file:"$SERLOG" -no-reboot \
    </dev/null >/dev/null 2>"$QEMU_ERR" || true
echo "  Boot beendet (Timeout oder system_off)"

OUT="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$SERLOG" 2>/dev/null)"
if [ -z "$OUT" ]; then
    echo "== KEIN OUTPUT (Aufbau, kein Testergebnis) =="
    [ -s "$QEMU_ERR" ] && { echo "-- QEMU sagt: --"; sed -n '1,20p' "$QEMU_ERR"; }
    exit 2
fi

echo "== Marker (Ergebniszeilen) =="
echo "$OUT" | grep -E "^(mbi|mbmod|archive|manifest|clientn|root|vollzahl|devassign|devsel|dmaiso|drv|lxpddrv|blkdev|part|fs|wasm|bootckpt|irqmsi|tls|loader|SELFTEST) *:?[^:]*:" || true
echo "$OUT" | grep -E "^el0-trap" || true

echo "== Schritt 6: Urteil =="
if grep -q "lxpddrv : \[7\]lxpd-test gestartet" <<<"$OUT"; then
    echo "  ERFOLG: $(grep -m1 'lxpddrv : \[7\]' <<<"$OUT")"
    echo "  Bilanz: $(grep -m1 'lxpddrv : gestartet=' <<<"$OUT")"
    grep -m1 "^vollzahl" <<<"$OUT" || true
    grep -m1 "^root    : ALL PASS" <<<"$OUT" || true
    grep -m1 "SELFTEST COMPLETE" "$SERLOG" || true
    exit 0
fi
ABSAGE="$(grep -m1 'lxpddrv : \[7\]lxpd-test ABGEWIESEN' <<<"$OUT")"
if [ -n "$ABSAGE" ]; then
    echo "  BENANNTE ABSAGE: $ABSAGE"
    echo "  Bilanz: $(grep -m1 'lxpddrv : gestartet=' <<<"$OUT")"
    grep -m1 "HINWEIS Archivkopie" <<<"$OUT" || true
    exit 1
fi
echo "  WEDER Start NOCH Absage fuer [7] (unerwartet; HINWEIS- und Bilanzzeilen:)"
grep "lxpddrv" <<<"$OUT" || echo "  (keine einzige lxpddrv-Zeile)"
exit 2
