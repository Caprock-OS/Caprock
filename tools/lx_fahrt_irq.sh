#!/usr/bin/env bash
# E2E-IRQ-Fahrt — EINE Treiber-PD mit echter Geraetezuteilung faehrt BIND_IRQ + WAIT.
#
# STAND (Mitteilung 17): Fahrt 5 gruente bis gestartet=1, vollzahl 7/7,
# SELFTEST COMPLETE (MinELF EB FE, caps=0x0000, keine Geraeteberuehrung).
#
# NAECHSTE STUFE (diese Fahrt): Eintrag 7 traegt Geraete-Caps
# (mmio,dma,ntfn,ep + Selektor virtio-blk 1af4:1042) und das MinELF-IRQ-Bild
# (tools/lx_minelf_irq.py): BIND_IRQ(26) auf Slot 7/8, WAIT(9) auf Slot 8 mit
# Frist 200, dann kombinierter Seitenfehler [r12*0x1000+r13] — FAR traegt BEIDE
# Ergebnis-Codes (BIND in Bits 12.., WAIT in Bits 0..11; EC=0x0e erwartet).
#
# ZUTEILUNG: Eintraege 3/5 belegen blk0/net0. Damit Eintrag 7 ein FREIES Geraet
# findet, haengt diese Fahrt eine ZWEITE virtio-blk-Platte an (blk1, 16M Nullen
# — Zuteilung ist PCI-Ebene, Inhalt egal). Eintrag 3 behaelt blk0 (Reihenfolge:
# Root-Kette vor lxpddrv), Eintrag 7 bekommt blk1.
#
# ERFOLG = gestartet=1 + el0-trap(Programm 7, EC=0x0e) mit deutbarer FAR:
#   FAR=0x19           -> BIND=0 OK, WAIT=25 ERR_TIMEOUT (benannte Absage, Doku:
#                         caprock-abi WAIT-Frist; Geraet schweigt ohne Queue —
#                         erwartet, die Sonde programmiert keine)
#   FAR=0x0            -> BIND=0, WAIT=0 = GEWECKT (Interrupt kam an; weckrufe-
#                         Aequivalent — die Sonde meldet nicht via DMA wie
#                         virtio-blk, sondern via Rueckgabewert)
#   FAR=0x1001         -> BIND=1 ERR_BADCAP (kein Vektor/leerer Slot 7; s.
#                         loader.rs B2: Slot 7 nur MIT Vektor) + WAIT=1
#   lxpddrv ABGEWIESEN -> Loader-Absage (NoDevice/KeineZuteilung u.a.), exit 1.
#
# KVM-KONTENTION: vor QEMU-Start 120 s warten (Strang-Vorgabe) — im Skript, damit
# sie unabhaengig vom Bauende gilt. Toolchain: rustup run nightly (via
# build-x86.sh) + programs-Bau wie Fahrt 5. KEIN Commit/Push (Strang 4).
#
# Aufruf:  bash tools/lx_fahrt_irq.sh [VERSUCH-NR]   (Vorgabe: 1)
# Log:     build/diag/lx-e2e-irq-N.log (tee) + -seriell.log + -qemu-err.log.
# Exit: 0 = gestartet + Trap deutbar, 1 = benannte Loader-Absage, 2 = Aufbau/sonst.

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
LOG="$DIAG/lx-e2e-irq-$N.log"
exec > >(tee "$LOG") 2>&1

echo "== E2E-IRQ-Fahrt, Versuch $N ($(date -u '+%Y-%m-%d %H:%M UTC')) =="

KELF="build/target/x86_64-unknown-none/release/caprock-kernel"
PROG="programs/build/target/x86_64-caprock-user/release"
MANKEY="keys/manifest-test.manifest.ed25519"
TRUSTKEY="keys/trusted-test.ed25519"

# --- Schritt 0: MinELF-IRQ bauen -------------------------------------------------
MINELF="$DIAG/lx-minelf-irq.img"
echo "== Schritt 0: MinELF-IRQ (VA 0x20000000, BIND+WAIT+#PF-Melder) =="
python3 tools/lx_minelf_irq.py --out "$MINELF" || exit 2
[ "$(stat -c %s "$MINELF")" = 8192 ] \
    && echo "  Groesse 8192 B wie erwartet" \
    || { echo "  FEHLER: unerwartete Groesse (Aufbau, kein Testergebnis)"; exit 2; }

# --- Schritt 1: Container-Referenz (NUR LESEN, D5-Spannenrueckfall) ---------------
CONTAINER_SRC="tests/lxpd-boot-qemu/lxpd-test.lxpd"
echo "== Schritt 1: v1-Container (nur lesen, D5-Archivkopie) =="
ls -la "$CONTAINER_SRC"
sha256sum "$CONTAINER_SRC"

# --- Schluessel ------------------------------------------------------------------
echo "== Schluessel sicherstellen (dev-keys.sh) =="
DEVKEYS_OUT="$(tools/dev-keys.sh)" || exit 2
case "$DEVKEYS_OUT" in *DEVKEYS_REBUILD=1*)
    echo "  Schluessel neu erzeugt -> Kernel wird mit frischer Key-DB gebaut" ;;
esac

# --- Kernel + Programme -----------------------------------------------------------
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

# --- Platten: blk0 (GPT wie Fahrt 5) + blk1 (Nullen, Zuteilungsziel) --------------
BLK_IMG="$DIAG/lx-e2e-irq-blk0.img"
BLK2_IMG="$DIAG/lx-e2e-irq-blk1.img"
echo "== Platten bauen =="
python3 tools/mkgpt.py "$BLK_IMG" --sectors 32768 \
    --part "34:20000" --part 20001:32700 --magic-at 20001 \
    --fat16 "34:20000" --file "HELLO.TXT=CAPROCKS-DATEIINHALT" \
    || { echo "FEHLER: GPT-Abbild"; exit 2; }
[ -f "$BLK2_IMG" ] || truncate -s 16M "$BLK2_IMG" || { echo "FEHLER: blk1"; exit 2; }
ls -la "$BLK_IMG" "$BLK2_IMG"

# --- Schritt 2+3: Manifest (7) + Archiv (7, D5) ------------------------------------
# Eintrag 7: pid 7, BLOB=MinELF-IRQ, dom=1 HardwareLand, caps mmio,dma,ntfn,ep,
# Selektor virtio-blk — ECHTE Geraetezuteilung (assign_driver_device).
ARCHIV="$DIAG/lx-e2e-irq-archiv.bin"
echo "== Schritt 2+3: Manifest + Archiv (7/7) =="
python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
    --out "$DIAG/lx-e2e-irq-system.manifest" \
    --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
    --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
    --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1042" \
    --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
    --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
    --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
    --entry "7:lxpd-irq:1:1:$MINELF:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1042" \
    || { echo "FEHLER: Manifest"; exit 2; }
python3 tools/mkarchive.py "$ARCHIV" --system-manifest "$DIAG/lx-e2e-irq-system.manifest" \
    "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
    "2:hello:2:1:$PROG/hello.elf" \
    "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
    "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
    "5:virtio-net:1:1:$PROG/virtio-net.elf" \
    "6:wasmhost:2:1:$PROG/wasmhost.elf" \
    "7:lxpd-test:1:1:$CONTAINER_SRC" \
    || { echo "FEHLER: Archiv"; exit 2; }

# --- Schritt 4: ISO (2 Module: Archiv + MinELF-IRQ) --------------------------------
ISO="$DIAG/lx-e2e-irq.iso"
echo "== Schritt 4: ISO (2 Module) =="
bash tools/lx_fahrt4_iso.sh --kernel "$KELF.mb32" --archive "$ARCHIV" \
    --driver "$MINELF" --out "$ISO" || exit 2

# --- KVM-Kontention: 120 s vor QEMU-Start (Strang-Vorgabe) --------------------------
echo "== KVM-Kontention: 120 s warten vor QEMU-Start =="
for i in $(seq 120 -1 1); do
    [ $((i % 30)) = 0 ] && echo "  noch $i s ($(date -u '+%H:%M:%S'))"
    sleep 1
done
echo "  Warten beendet, QEMU startet"

# --- Schritt 5: QEMU-Boot (GRUB, -cdrom, blk1 zusaetzlich) --------------------------
SERLOG="$DIAG/lx-e2e-irq-$N-seriell.log"
QEMU_ERR="$DIAG/lx-e2e-irq-$N-qemu-err.log"
echo "== Schritt 5: QEMU-Boot (180 s, KVM, 512M, smp 4, q35+split+iommu/intremap, 2x blk) =="
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
    -drive if=none,id=blk1,format=raw,file="$BLK2_IMG" \
    -device virtio-blk-pci,drive=blk1,disable-legacy=on,iommu_platform=on \
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
if grep -q "lxpddrv : \[7\]lxpd-irq gestartet" <<<"$OUT"; then
    echo "  START: $(grep -m1 'lxpddrv : \[7\]' <<<"$OUT")"
    echo "  Bilanz: $(grep -m1 'lxpddrv : gestartet=' <<<"$OUT")"
    grep -m1 "^vollzahl" <<<"$OUT" || true
    TRAP="$(grep -m1 '(Programm 7)' <<<"$OUT" || true)"
    if [ -n "$TRAP" ]; then
        echo "  TRAP: $TRAP"
        FAR_HEX="$(grep -oE 'FAR=0x[0-9a-f]+' <<<"$TRAP" | head -1 | sed 's/FAR=//')"
        EC_HEX="$(grep -oE 'EC=0x[0-9a-f]+' <<<"$TRAP" | head -1 | sed 's/EC=//')"
        if [ -n "$FAR_HEX" ]; then
            FAR="$((FAR_HEX))"
            BIND="$(( (FAR >> 12) & 0xFFF ))"
            WAIT="$(( FAR & 0xFFF ))"
            echo "  DEUTUNG: EC=$EC_HEX (erwartet 0x0e=#PF), FAR=$FAR_HEX -> BIND=$BIND WAIT=$WAIT"
            echo "  Codes: 0=OK, 1=ERR_BADCAP, 3=ERR_RIGHTS, 23=ERR_IRQ_FULL, 25=ERR_TIMEOUT"
            case "$BIND/$WAIT" in
                0/25) echo "  ERFOLG (benannte Absage ERR_TIMEOUT): BIND ok, Geraet schweigt ohne Queue — erwartet" ;;
                0/0)  echo "  ERFOLG (Weckruf): BIND ok, WAIT kehrte mit OK zurueck — Interrupt kam an" ;;
                1/1)  echo "  ERFOLG (benannte Absage ERR_BADCAP): kein Vektor — Slot 7 leer (B2-Regel)" ;;
                *)    echo "  START mit unerwarteter Code-Kombination (s. Deutung oben)" ;;
            esac
        fi
        grep -m1 "SELFTEST COMPLETE" "$SERLOG" || true
        exit 0
    fi
    echo "  GESTARTET, aber KEIN el0-trap(Programm 7) — Sonde lief nicht bis zum Melder (unerwartet)"
    exit 2
fi
ABSAGE="$(grep -m1 'lxpddrv : \[7\]lxpd-irq ABGEWIESEN' <<<"$OUT")"
if [ -n "$ABSAGE" ]; then
    echo "  BENANNTE ABSAGE: $ABSAGE"
    echo "  Bilanz: $(grep -m1 'lxpddrv : gestartet=' <<<"$OUT")"
    grep -m1 "HINWEIS Archivkopie" <<<"$OUT" || true
    grep -m1 "fehlende Ressource" <<<"$OUT" || true
    exit 1
fi
echo "  WEDER Start NOCH Absage fuer [7] (unerwartet; HINWEIS- und Bilanzzeilen:)"
grep "lxpddrv" <<<"$OUT" || echo "  (keine einzige lxpddrv-Zeile)"
exit 2
