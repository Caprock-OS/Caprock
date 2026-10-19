#!/usr/bin/env bash
# **B4b-Messstand** (TODO.md B4b): die Umgebung als Variable behandeln.
#
# Ein Aufruf = EINE QEMU-Variante. Drei Laeufe laut Vorgabe:
#   1. ohne KVM (TCG):          LX_B4B_ACCEL=tcg  LX_B4B_INTREMAP=on  LX_B4B_EIM=off
#   2. ohne intremap (Kontrolle): LX_B4B_ACCEL=tcg  LX_B4B_INTREMAP=off
#   3. mit eim=on:              LX_B4B_ACCEL=tcg  LX_B4B_INTREMAP=on  LX_B4B_EIM=on
#
# Gebrauch:
#   tools/lx_b4b-messstand.sh <name>            # baut + bootet einmal
#   LX_B4B_NOBUILD=1 tools/lx_b4b-messstand.sh <name>   # nur booten (Archiv wiederverwenden)
#
# Logs: build/diag/lx-b4b-<name>.log (+ .qemu-err, + .auszug). NICHT committen
# (`/build/` ist gitignored). Diese Datei misst nur -- sie urteilt nicht.
set -uo pipefail
cd "$(dirname "$0")/.."

NAME="${1:?Gebrauch: tools/lx_b4b-messstand.sh <name>}"
ACCEL_KIND="${LX_B4B_ACCEL:-tcg}"
INTREMAP="${LX_B4B_INTREMAP:-on}"
EIM="${LX_B4B_EIM:-off}"
RAM="${LX_B4B_RAM:-512M}"
LOG="build/diag/lx-b4b-${NAME}.log"
QEMU_ERR="build/diag/lx-b4b-${NAME}.qemu-err"
AUSZUG="build/diag/lx-b4b-${NAME}.auszug"
mkdir -p build/diag

if [ "$ACCEL_KIND" = tcg ]; then
    TIMEOUT="${LX_B4B_TIMEOUT:-300}"
else
    TIMEOUT="${LX_B4B_TIMEOUT:-150}"
fi

# --- Bauen (aus test-qemu-x86-load.sh uebernommen, gekuerzt auf das Noetige) ---
if [ -z "${LX_B4B_NOBUILD:-}" ]; then
    KELF=build/target/x86_64-unknown-none/release/caprock-kernel
    PROG=programs/build/target/x86_64-caprock-user/release
    MANKEY=keys/manifest-test.manifest.ed25519
    python3 -c "import cryptography" 2>/dev/null || { echo "FEHLT: python cryptography"; exit 2; }
    echo "== Manifest-Schluessel =="
    python3 tools/gen_manifest_key.py --ensure || exit 2
    echo "== build (Kernel, --features selftest) =="
    ./build-x86.sh --features selftest >/dev/null 2>&1 || { echo "BUILD FAILED (Kernel)"; exit 1; }
    echo "== build (Programme) =="
    ( cd programs && rustup run nightly cargo build --release --target x86_64-caprock-user.json ) \
        >/dev/null 2>&1 || { echo "BUILD FAILED (Programme)"; exit 1; }
    echo "== zertifizieren (init, fs) =="
    mkdir -p certs build
    TRUSTKEY=keys/trusted-test.ed25519
    python3 tools/check_trusted_key.py >/dev/null 2>&1 || {
        python3 tools/gen_trusted_key.py --name trusted-test >/dev/null 2>&1 || exit 2
        ./build-x86.sh --features selftest >/dev/null 2>&1 || exit 1
    }
    python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$PROG/init.elf" \
        --program-id 1 --version 1 --policy internal-test --key "$TRUSTKEY" \
        --out certs/init-x86.cert >/dev/null 2>&1 || exit 2
    python3 tools/sign_trusted.py --crate programs/trusted/fs --elf "$PROG/fs.elf" \
        --program-id 4 --version 1 --policy internal-test --key "$TRUSTKEY" \
        --out certs/fs-x86.cert >/dev/null 2>&1 || exit 2
    echo "== Boot-Archiv bauen =="
    python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
        --out build/system.manifest \
        --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
        --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
        --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1042" \
        --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
        --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
        --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
        >/dev/null 2>&1 || exit 2
    python3 tools/mkarchive.py build/boot-archive-x86.bin --system-manifest build/system.manifest \
        "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
        "2:hello:2:1:$PROG/hello.elf" \
        "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
        "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
        "5:virtio-net:1:1:$PROG/virtio-net.elf" \
        "6:wasmhost:2:1:$PROG/wasmhost.elf" >/dev/null 2>&1 || exit 2
elif [ -n "${LX_B4B_REARCHIV:-}" ]; then
    echo "== REARCHIV: Zertifikate/Manifest/Archiv neu, Kernel+Programme wiederverwendet =="
    KELF=build/target/x86_64-unknown-none/release/caprock-kernel
    PROG=programs/build/target/x86_64-caprock-user/release
    MANKEY=keys/manifest-test.manifest.ed25519
    TRUSTKEY=keys/trusted-test.ed25519
    mkdir -p certs build
    python3 tools/check_trusted_key.py >/dev/null 2>&1 || { echo "TRUSTKEY passt nicht zum Kernel"; exit 2; }
    python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$PROG/init.elf" \
        --program-id 1 --version 1 --policy internal-test --key "$TRUSTKEY" \
        --out certs/init-x86.cert >/dev/null 2>&1 || exit 2
    python3 tools/sign_trusted.py --crate programs/trusted/fs --elf "$PROG/fs.elf" \
        --program-id 4 --version 1 --policy internal-test --key "$TRUSTKEY" \
        --out certs/fs-x86.cert >/dev/null 2>&1 || exit 2
    python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
        --out build/system.manifest \
        --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
        --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
        --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1042" \
        --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
        --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
        --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
        >/dev/null 2>&1 || exit 2
    python3 tools/mkarchive.py build/boot-archive-x86.bin --system-manifest build/system.manifest \
        "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
        "2:hello:2:1:$PROG/hello.elf" \
        "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
        "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
        "5:virtio-net:1:1:$PROG/virtio-net.elf" \
        "6:wasmhost:2:1:$PROG/wasmhost.elf" >/dev/null 2>&1 || exit 2
else
    echo "== NOBUILD: vorhandenes Archiv/Kernel wird wiederverwendet =="
fi

# --- Plattenabbild (frisch je Variante: fruehere Laeufe beschreiben Sektoren) ---
KELF=build/target/x86_64-unknown-none/release/caprock-kernel
BLK_IMG="build/lx-b4b-blk-${NAME}.img"
python3 tools/mkgpt.py "$BLK_IMG" --sectors 32768 \
    --part "34:20000" --part 20001:32700 --magic-at 20001 \
    --fat16 "34:20000" --file "HELLO.TXT=CAPROCKS-DATEIINHALT" \
    || { echo "FEHLER: GPT-Abbild"; exit 2; }

# --- QEMU-Zeile (Variante) ---
if [ "$ACCEL_KIND" = tcg ]; then
    ACCEL=(-accel tcg -cpu Skylake-Client)
else
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
fi
IOMMU_DEV="intel-iommu,caching-mode=on"
[ "$INTREMAP" = on ] && IOMMU_DEV="$IOMMU_DEV,intremap=on"
[ "$INTREMAP" = on ] && [ "$EIM" = on ] && IOMMU_DEV="$IOMMU_DEV,eim=on"
echo "== Variante $NAME: accel=$ACCEL_KIND intremap=$INTREMAP eim=$EIM ram=$RAM timeout=${TIMEOUT}s =="
echo "== IOMMU: -device $IOMMU_DEV =="

timeout "$TIMEOUT" qemu-system-x86_64 \
    -kernel "$KELF.mb32" -m "$RAM" -smp 4 "${ACCEL[@]}" \
    -machine q35,kernel-irqchip=split -device "$IOMMU_DEV" \
    -device virtio-rng-pci,disable-legacy=on,iommu_platform=on \
    -drive if=none,id=blk0,format=raw,file="$BLK_IMG" \
    -device virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on \
    -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
    -netdev user,id=n0,restrict=on \
    -initrd build/boot-archive-x86.bin \
    -nographic -serial file:"$LOG" -no-reboot \
    </dev/null >/dev/null 2>"$QEMU_ERR" || true

# --- Auszug: je Lauf used.idx, IRTE-Zustand, Faults, IRQ? ---
{
echo "### Variante $NAME (accel=$ACCEL_KIND intremap=$INTREMAP eim=$EIM)"
echo "--- apic/x2apic ---"
grep -E "^apic    :" "$LOG" || echo "(keine apic-Zeile)"
echo "--- irtevgb (Vergabe scharf?) ---"
grep -E "^irtevgb :" "$LOG" || echo "(keine irtevgb-Zeile)"
echo "--- irqmsi Geraet (used.idx, queue-Vektor, MSI-X-Zeile, ctrl) ---"
grep -E "^irqmsi  : Eintrag .* (Geraet|MSI-X-Zeile)" "$LOG" || echo "(keine Geraetezeilen)"
echo "--- irqmsi B4 ---"
grep -E "^irqmsi  : Eintrag .* B4" "$LOG" || echo "(keine B4-Zeilen)"
echo "--- irqmsi Zustellprobe (remappable vs. KOMPAT, faults) ---"
grep -E "^irqmsi  : Zustellprobe" "$LOG" || echo "(keine Zustellprobe)"
echo "--- irqmsi Urteil ---"
grep -E "^irqmsi  : (ALL PASS|FAILURES|SKIP)" "$LOG" || echo "(kein Urteil)"
echo "--- vtd/iommu/fault-Zeilen ---"
grep -iE "fault|vtd|dmar|remap" "$LOG" | grep -vE "^irqmsi  : (Eintrag|Zustellprobe)" | head -20 || true
echo "--- Abschluss ---"
grep -E "SELFTEST (COMPLETE|FAILED)|WATCHDOG" "$LOG" | head -5 || echo "(kein Abschluss)"
} | tee "$AUSZUG"
