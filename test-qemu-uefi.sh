#!/usr/bin/env bash
# Boot the aarch64 kernel through the UEFI stub under QEMU + edk2 and check the markers.
#
#   ./test-qemu-uefi.sh            # firmware enters the stub at EL1
#   EL2=1 ./test-qemu-uefi.sh      # virtualization=on: firmware (and stub) run at EL2
#   GPU=virtio ./test-qemu-uefi.sh # virtio-gpu-pci instead of ramfb
#
# Builds kernel + signed manifest + archive through `ARCHIVE_ONLY=1 ./test-qemu.sh` (the manifest is
# bound to the exact kernel image), then boots it twice: through the stub (this script's subject)
# and with plain `-kernel` as the reference. PASS = the stub path reaches the same result
# signature as the `-kernel` path, plus the handover markers.
# It does not replace test-qemu.sh (the `-kernel` path stays the regression suite).
set -uo pipefail
cd "$(dirname "$0")"

TIMEOUT="${TIMEOUT:-360}"
FW="${FW:-/usr/share/edk2/aarch64/QEMU_EFI.fd}"
ELF=build/target/aarch64-caprock/release/caprock-kernel.elf
STUB=boot/uefi-aarch64/build/target/aarch64-unknown-uefi/release/caprock-uefi-stub.efi
ESP=build/esp
LOG="${LOG:-build/diag/uefi-boot.log}"
[ -f "$FW" ] || { echo "MISSING firmware $FW"; exit 2; }

ARCHIVE_ONLY=1 ./test-qemu.sh >/dev/null 2>&1 || { echo "KERNEL/ARCHIVE BUILD FAILED (ARCHIVE_ONLY=1 ./test-qemu.sh)"; exit 1; }
[ -f build/boot-archive.bin ] || { echo "MISSING build/boot-archive.bin"; exit 2; }
( cd boot/uefi-aarch64 && cargo +nightly build -j14 --release ) >/dev/null 2>&1 || { echo "STUB BUILD FAILED"; exit 1; }

rm -rf "$ESP"; mkdir -p "$ESP/EFI/BOOT" build/diag
cp "$STUB" "$ESP/EFI/BOOT/BOOTAA64.EFI"
cp "$ELF" "$ESP/caprock.elf"
cp build/boot-archive.bin "$ESP/archive.bin"
# Stand-in disk-driver image (Simon 2026-10-09: the driver travels WITH the bootloader, not on
# disk). No driver PD exists yet, so this is a deterministic 64 KiB pattern blob with a magic;
# it exercises the stub load path + handover span end to end (the kernel bounds-checks it and
# passes it through, no parsing).
python3 -c "
import struct
magic = b'CAPROCK-DRIVER-TEST\x00'
body = bytes((i * 2654435761 >> 16) & 0xFF for i in range(65536 - len(magic) - 8))
open('$ESP/driver.bin','wb').write(magic + struct.pack('<I', 65536) + struct.pack('<I', 0x44565231) + body)
"

MACHINE="virt,iommu=smmuv3"
[ -n "${EL2:-}" ] && MACHINE="$MACHINE,virtualization=on"
GPUDEV="ramfb"
[ "${GPU:-}" = virtio ] && GPUDEV="virtio-gpu-pci"

echo "== uefi boot (${TIMEOUT}s, machine $MACHINE, gpu $GPUDEV) =="
rm -f "$LOG"
timeout --signal=TERM "$TIMEOUT" qemu-system-aarch64 \
    -machine "$MACHINE" -cpu cortex-a72 -smp 8 -m 4G \
    -display none -serial "file:$LOG" -no-reboot -net none \
    -bios "$FW" -device "$GPUDEV" \
    -drive "file=fat:rw:$ESP,format=raw,if=none,id=esp" -device virtio-blk-pci,drive=esp \
    -device pcie-root-port,id=rp0,chassis=1 -device virtio-rng-pci,bus=rp0,iommu_platform=on \
    </dev/null >/dev/null 2>&1
echo "qemu rc=$?"

sig() { grep -aoE '^[a-z0-9_]+ +: (ALL PASS|FAILURES|SKIP)|^== SELFTEST [A-Z]+( \(watchdog\))?' "$1" | sort; }

rc=0
if [ -z "${SKIP_REF:-}" ]; then
    REF=build/diag/kernel-ref.log; rm -f "$REF"
    echo "== reference: plain -kernel boot =="
    timeout --signal=TERM "$TIMEOUT" qemu-system-aarch64 \
        -machine virt,iommu=smmuv3 -cpu cortex-a72 -smp 8 -m 4G \
        -nographic -serial "file:$REF" -no-reboot -net none \
        -device pcie-root-port,id=rp0,chassis=1 -device virtio-rng-pci,bus=rp0,iommu_platform=on \
        -device loader,file=build/boot-archive.bin,addr=0x13F000000 \
        -kernel "$ELF" </dev/null >/dev/null 2>&1
    sig "$REF" > build/diag/sig-ref.txt; sig "$LOG" > build/diag/sig-uefi.txt
    if diff build/diag/sig-ref.txt build/diag/sig-uefi.txt >/dev/null; then
        echo "ok   : result signature identical to -kernel boot ($(wc -l < build/diag/sig-uefi.txt) lines)"
    else
        echo "FAIL : result signature differs from -kernel boot:"; diff build/diag/sig-ref.txt build/diag/sig-uefi.txt | head -20; rc=1
    fi
fi
chk() { if grep -aq -- "$2" "$LOG"; then echo "ok   : $1"; else echo "FAIL : $1  (missing: $2)"; rc=1; fi; }
chk "stub started"                 'stub: Caprock aarch64 UEFI stub'
chk "kernel loaded by stub"        'stub: kernel loaded'
chk "driver loaded by stub"        'stub: driver [0-9]* bytes at'
chk "kernel entered via stub"      'uefi   : kernel entered via UEFI stub'
chk "handover framebuffer read"    'uefi   : fb base='
chk "driver span in loader window" 'uefi   : driver image'
chk "device description passed"    'uefi   : devdesc dtb'
chk "framebuffer read-back"        'uefi   : fb ALL PASS'
chk "regular boot continues"       'boot: primary core up'
chk "selftest verdict line"         '== SELFTEST [A-Z]'
grep -a "^uefi   :\|^stub:" "$LOG" | head -20
exit $rc
