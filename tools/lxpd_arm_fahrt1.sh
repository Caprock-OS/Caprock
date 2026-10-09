#!/usr/bin/env bash
# LXPD-ARM Fahrt 1 — first driver start on aarch64 (strand 8).
#
# WHAT THIS PROVES (the x86 bar is Fahrt 5: verify-am-boot + `[7]lxpd-minelf
# gestartet`, vollzahl 7/7 — see tools/lx_minelf_fahrt5.sh on the main line,
# read, not changed):
#   * the LXPD manifest/driver crates are usable on ARM,
#   * the image-basis rule holds on ARM (user window [0x40200000, 0x80000000);
#     below it the kernel rightly refuses with Code 10, like x86's FINE_BLOCKS),
#   * verify-before-load (Ed25519 system manifest + sha256 image bind) runs on ARM,
#   * the first lxpd-minelf START lands in the guest log, with vollzahl 2/2.
#
# Full E2E (IRQ delivery, block E2E, DMA) is explicitly OUT — a driver that
# starts but cannot do DMA/IRQ counts as first start. That boundary is named
# in programs/lxpd-runtime/BETRIEB.md ("ARM status").
#
# RECIPE (ARM analogue of Fahrt 5):
# Manifest 2 entries (1:init as in test-qemu.sh + entry 7: pid 7,
# sha256=minelf hash, dom=1 HardwareLand, caps EMPTY, no device); archive 2
# programs (init + a 1-stub v1 container as pid 7, whose hash deliberately
# differs -> the span fallback in boot_lxpd_treiber loads the slot module,
# exactly like Fahrt 5's archive/container vs. span/minelf split).
# The driver travels in ARM module SLOT 0 (top of the MOD window, framed
# "LXARMOD1" + len; kernel/src/lxpd_boot.rs documents the convention) via an
# extra QEMU -device loader. QEMU otherwise mirrors test-qemu.sh (virt,
# cortex-a72, 4G — the MOD window assumes >=4G RAM), with -smp 2 for TCG speed.
#
# Two boots: POSITIVE (basis 0x41000000 -> gestartet=1, vollzahl 2/2 ALL PASS),
# then NEGATIVE (basis 0x100000, the old lxport failure shape -> named
# ABGEWIESEN + Code-10 Mangel line + vollzahl 1/2 FEHLEND program_id 7).
#
# Usage:  bash tools/lxpd_arm_fahrt1.sh [TRY-NR]   (default: 1)
# Logs:   build/diag/lxpd-arm-fahrt1-{pos,neg}-<N>.log (serial) + this script's
#         own log build/diag/lxpd-arm-fahrt1-<N>.log (writes there via tee).
# Result: marker quotes at the end; exit 0 = positive started AND negative
#         refused as specified, 1 = a named refusal where a start belonged
#         (or vice versa), 2 = setup failure (no test result).

if [ -z "${BASH_VERSION:-}" ]; then
    echo "ERROR: this script needs bash, not sh/dash." >&2
    exit 2
fi
set -uo pipefail

N="${1:-1}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DIAG="build/diag"
mkdir -p "$DIAG"
LOG="$DIAG/lxpd-arm-fahrt1-$N.log"
exec > >(tee "$LOG") 2>&1

echo "== LXPD-ARM Fahrt 1, try $N ($(date -u '+%Y-%m-%d %H:%M UTC')) =="

KELF="build/target/aarch64-caprock/release/caprock-kernel"
PROG="programs/build/target/aarch64-caprock-user/release"
MANKEY="keys/manifest-test.manifest.ed25519"
TRUSTKEY="keys/trusted-test.ed25519"

# ARM module slot 0 (must match kernel/src/lxpd_boot.rs: top-down from window end).
MOD_BASE=0x13F000000
MOD_WINDOW=0x1000000
SLOT_LEN=0x100000
SLOT0_ADDR=0x13FF00000
# Archive must never reach the lowest slot (7 slots top-down => 9 MiB budget).
ARCH_MAX=$((8 * 1024 * 1024))

# --- Step 0: MinELFs (good basis + low basis) -------------------------------------
MINELF_GOOD="$DIAG/lxpd-arm-minelf-good.img"
MINELF_BAD="$DIAG/lxpd-arm-minelf-low.img"
echo "== step 0: MinELFs (ARM VA 0x41000000 / low VA 0x100000) =="
python3 tools/lx_minelf_arm.py --out "$MINELF_GOOD" || exit 2
python3 tools/lx_minelf_arm.py --out "$MINELF_BAD" --vaddr 0x100000 || exit 2
[ "$(stat -c %s "$MINELF_GOOD")" = 8192 ] \
    && echo "  size 8192 B as expected" \
    || { echo "  ERROR: unexpected size (setup, no test result)"; exit 2; }
python3 - "$MINELF_GOOD" 0x41000000 <<'EOF' || exit 2
import struct, sys
path, va = sys.argv[1], int(sys.argv[2], 0)
d = open(path, 'rb').read()
assert d[:4] == b'\x7fELF', 'not ELF'
assert struct.unpack_from('<H', d, 16)[0] == 2, 'not ET_EXEC'
assert struct.unpack_from('<H', d, 18)[0] == 0xB7, 'not EM_AARCH64'
assert struct.unpack_from('<Q', d, 24)[0] == va, 'entry mismatch'
assert struct.unpack_from('<I', d, 64)[0] == 1, 'not 1 PT_LOAD'
assert struct.unpack_from('<Q', d, 64 + 16)[0] == va, 'PT_LOAD VA wrong'
assert struct.unpack_from('<I', d, 0x1000)[0] == 0x14000000, 'no `b .` at segment start'
assert va >= 0x40200000, 'good basis below ARM user window (setup bug)'
print('  host self-check: ET_EXEC aarch64, entry=VA, `b .` ok, basis in window')
EOF
python3 - "$MINELF_BAD" <<'EOF' || exit 2
import struct, sys
d = open(sys.argv[1], 'rb').read()
assert struct.unpack_from('<Q', d, 24)[0] == 0x100000, 'bad entry mismatch'
print('  host self-check: low-basis image links at 0x100000 (must fall in-guest)')
EOF

# --- Step 1: pid-7 archive filler (valid 1-stub v1 container, hash differs) --------
CONTAINER="$DIAG/lxpd-arm-filler.lxpd"
echo "== step 1: v1-container filler (read-only payload for archive slot 7) =="
python3 - "$CONTAINER" <<'EOF' || exit 2
import struct, sys
# LXPD + tramp_count=1 + rewired=0 + section_size=32 + 1 stub + LXEND = 57 B.
v = bytearray()
v += b'LXPD'
v += struct.pack('<I', 1) + struct.pack('<I', 0) + struct.pack('<Q', 32)
v += b'LXTR' + struct.pack('<I', 0) + struct.pack('<I', 0x811c9dc5) * 2 + bytes([0x90] * 16)
v += b'LXEND'
assert len(v) == 57
open(sys.argv[1], 'wb').write(bytes(v))
print(f'  filler: {sys.argv[1]} (57 B, 1 stub)')
EOF

# --- Keys (ensure; changes nothing while keys/ exists) -----------------------------
echo "== manifest key =="
python3 tools/gen_manifest_key.py --ensure || exit 2
echo "== trusted key (test-qemu.sh flow, read-only reuse) =="
python3 tools/check_trusted_key.py
KEYRC=$?
if [ "$KEYRC" -eq 2 ]; then
    echo "== ERROR: trusted key uncheckable (setup, no test result) =="
    exit 2
fi
if [ "$KEYRC" -ne 0 ]; then
    if [ -f keys/trusted-test.ed25519 ]; then
        BEISEITE="keys/trusted-test.ed25519.passt-nicht-$(date +%Y%m%d-%H%M%S)"
        mv keys/trusted-test.ed25519 "$BEISEITE"
        [ -f keys/trusted-test.ed25519.pub ] && mv keys/trusted-test.ed25519.pub "$BEISEITE.pub"
        echo "== trusted key mismatched -> set aside: $BEISEITE =="
    else
        echo "== trusted test key missing -> generating (fresh clone) =="
    fi
    python3 tools/gen_trusted_key.py --name trusted-test >/dev/null 2>&1 || {
        echo "SCHLUESSEL-ERZEUGUNG FEHLGESCHLAGEN"; exit 2; }
    echo "   generated; kernel/src/trusted_keys.rs regenerated -> kernel rebuilds below"
fi

# --- Kernel (default build: no selftest — markers print unconditionally) ------------
echo "== build (kernel, default) =="
./build.sh >/dev/null 2>&1 || { echo "BUILD FAILED (kernel)"; exit 2; }
[ -f "$KELF.elf" ] || { echo "ERROR: $KELF.elf missing (setup)"; exit 2; }
echo "  kernel ok"

echo "== build (programs, aarch64-caprock-user) =="
( cd programs && rustup run nightly cargo build --release ) >/dev/null 2>&1 \
    || { echo "PROGRAMS BUILD FAILED"; exit 2; }
INIT="$PROG/init.elf"
[ -f "$INIT" ] || { echo "ERROR: $INIT missing (setup)"; exit 2; }
echo "  programs ok"

echo "== certify (TrustedSAS init) =="
mkdir -p certs
python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$INIT" \
    --program-id 1 --version 1 --out certs/init-arm.cert >/dev/null 2>&1 \
    || { echo "ERROR: init certificate"; exit 2; }
echo "  certificate ok"

# --- Manifest + archive, per boot variant ------------------------------------------
build_set() { # $1 = variant (pos|neg), $2 = minelf blob
    local variant="$1" blob="$2"
    local manifest="$DIAG/lxpd-arm-fahrt1-$variant.manifest"
    local archiv="$DIAG/lxpd-arm-fahrt1-$variant-archive.bin"
    python3 tools/sign_manifest.py --kernel "$KELF.elf" --key "$MANKEY" --manifest-version 1 \
        --out "$manifest" \
        --entry "1:init:0:1:$INIT:loader,ntfn:root:3::any:0" \
        --entry "7:lxpd-minelf:1:1:$blob:::1::any:0" \
        || { echo "ERROR: manifest ($variant)"; exit 2; }
    python3 tools/mkarchive.py "$archiv" --system-manifest "$manifest" \
        "1:init:0:1:$INIT::certs/init-arm.cert" \
        "7:lxpd-test:1:1:$CONTAINER" \
        || { echo "ERROR: archive ($variant)"; exit 2; }
    local size
    size="$(stat -c %s "$archiv")"
    echo "  $variant: manifest + archive ($size B)"
    [ "$size" -lt "$ARCH_MAX" ] \
        || { echo "  ERROR: archive $size B reaches module slots (setup)"; exit 2; }
}
echo "== step 2+3: manifest + archive (2/2, entry 7 = minelf hash) =="
build_set pos "$MINELF_GOOD"
build_set neg "$MINELF_BAD"

# --- Slot files (framing: magic + len + image; kernel hashes the image bytes) -------
make_slot() { # $1 = minelf, $2 = slot file
    python3 - "$1" "$2" <<'EOF' || exit 2
import struct, sys
img = open(sys.argv[1], 'rb').read()
open(sys.argv[2], 'wb').write(b'LXARMOD1' + struct.pack('<Q', len(img)) + img)
print(f'  slot: {sys.argv[2]} ({len(img)} B image + 16 B header)')
EOF
}
echo "== step 4: slots =="
SLOT_GOOD="$DIAG/lxpd-arm-slot0-good.bin"
SLOT_BAD="$DIAG/lxpd-arm-slot0-low.bin"
make_slot "$MINELF_GOOD" "$SLOT_GOOD"
make_slot "$MINELF_BAD" "$SLOT_BAD"
printf 'SLOT0=0x%x (must equal lxpd_boot.rs slot 0)\n' "$SLOT0_ADDR"
[ "$SLOT0_ADDR" = 0x13FF00000 ] || { echo "ERROR: slot math drifted (setup)"; exit 2; }

# --- QEMU boots ---------------------------------------------------------------------
boot() { # $1 = variant, $2 = archive, $3 = slot file
    local variant="$1" archiv="$2" slot="$3"
    local serlog="$DIAG/lxpd-arm-fahrt1-$variant-$N-seriell.log"
    local qemuerr="$DIAG/lxpd-arm-fahrt1-$variant-$N-qemu-err.log"
    echo "== step 5 ($variant): QEMU boot (300 s, TCG cortex-a72, smp 2, 4G) =="
    timeout 300 qemu-system-aarch64 \
        -machine virt,iommu=smmuv3 -cpu cortex-a72 -smp 2 -m 4G \
        -nographic -serial "file:$serlog" -no-reboot \
        -net none -device pcie-root-port,id=rp0,chassis=1 -device virtio-rng-pci,bus=rp0,iommu_platform=on \
        -device loader,file="$archiv,addr=0x13F000000" \
        -device loader,file="$slot,addr=0x13FF00000" \
        -kernel "$KELF.elf" </dev/null >/dev/null 2>"$qemuerr" || true
    echo "  boot done (timeout or system_off)"
    grep -a -E "^(archive|manifest|root|lxpddrv|vollzahl|loader|mbi) *:" "$serlog" 2>/dev/null || true
}

echo "=== POSITIVE boot (basis 0x41000000) ==="
boot pos "$DIAG/lxpd-arm-fahrt1-pos-archive.bin" "$SLOT_GOOD"
POSLOG="$DIAG/lxpd-arm-fahrt1-pos-$N-seriell.log"

echo "=== NEGATIVE boot (basis 0x100000) ==="
boot neg "$DIAG/lxpd-arm-fahrt1-neg-archive.bin" "$SLOT_BAD"
NEGLOG="$DIAG/lxpd-arm-fahrt1-neg-$N-seriell.log"

# --- Verdict -------------------------------------------------------------------------
echo "== step 6: verdict =="
rc=0
if grep -aq "lxpddrv : \[7\]lxpd-minelf gestartet" "$POSLOG"; then
    echo "  POSITIVE PASS: $(grep -a -m1 'lxpddrv : \[7\]lxpd-minelf gestartet' "$POSLOG")"
    echo "  Bilanz: $(grep -a -m1 'lxpddrv : gestartet=' "$POSLOG")"
    grep -a -m1 "^vollzahl" "$POSLOG" || { echo "  MISSING: vollzahl line (positive)"; rc=1; }
    grep -aq "^vollzahl: 2 von 2 .* ALL PASS" "$POSLOG" \
        || { echo "  MISSING: vollzahl 2/2 ALL PASS (positive)"; rc=1; }
    grep -a -m1 "^root    : ALL PASS" "$POSLOG" || { echo "  NOTE: no root ALL PASS (positive)"; rc=1; }
else
    echo "  POSITIVE FAIL: no start for [7]; refusal/balance lines:"
    grep -a "lxpddrv" "$POSLOG" || echo "  (not a single lxpddrv line)"
    rc=1
fi
if grep -aq "lxpddrv : \[7\]lxpd-minelf ABGEWIESEN" "$NEGLOG"; then
    echo "  NEGATIVE PASS: $(grep -a -m1 'lxpddrv : \[7\]lxpd-minelf ABGEWIESEN' "$NEGLOG")"
    grep -aq "Code 10" "$NEGLOG" \
        && echo "  Basis-Regel: $(grep -a -m1 'Code 10' "$NEGLOG")" \
        || { echo "  MISSING: Code-10 Mangel line (negative)"; rc=1; }
    grep -aq "^vollzahl: 1 von 2 .* FEHLEND: program_id 7" "$NEGLOG" \
        && echo "  Vollzahl: $(grep -a -m1 '^vollzahl' "$NEGLOG")" \
        || { echo "  MISSING: vollzahl 1/2 FEHLEND pid 7 (negative)"; rc=1; }
else
    if grep -aq "lxpddrv : \[7\]lxpd-minelf gestartet" "$NEGLOG"; then
        echo "  NEGATIVE FAIL: low-basis image STARTED (basis rule broken!)"
    else
        echo "  NEGATIVE FAIL: neither start nor refusal for [7]:"
        grep -a "lxpddrv" "$NEGLOG" || echo "  (not a single lxpddrv line)"
    fi
    rc=1
fi
[ "$rc" -eq 0 ] && echo "== FAHRT 1 PASSED (start + vollzahl + basis negative) =="
exit "$rc"
