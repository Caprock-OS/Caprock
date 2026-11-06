#!/usr/bin/env bash
# lx_bootcheck.sh -- Befund-Nachweis zur Bootloader-Entscheidung (tools/lx_bootentscheidung.md).
#
# Prueft LESEND am Arbeitsbaum, was die Entscheidung traegt -- und schreibt nichts:
#   * x86: Multiboot-Magic liegt im .mb32 innerhalb der ersten 8192 Datei-Bytes
#          (die Bedingung, an der QEMU/GRUB den Kernel ueberhaupt finden).
#   * Werkzeugseite: mkarchive.py / mkgpt.py / mkgrubiso.sh vorhanden.
#   * GRUB-Werkzeuge (grub-mkrescue, grub-file, xorriso) vorhanden oder als Luecke benannt.
#   * QEMU-Befehlszeilen beider Arches zur Gegenprobe ausgegeben (-initrd vs. -device loader).
#
# Aufruf:  bash tools/lx_bootcheck.sh [--help]
# Rueckgabe 0 = Befund vollständig erhebbar (nicht: "alles gruen").
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '2,12p' "$0"
    exit 0
fi

MB32="build/target/x86_64-unknown-none/release/caprock-kernel.mb32"
echo "== lx_bootcheck: Befund zur Bootloader-Entscheidung (nur lesend) =="

echo "--- x86: Multiboot-Magic im mb32 ---"
if [ -f "$MB32" ]; then
    OFF="$(python3 - "$MB32" <<'PYEOF'
import sys
d = open(sys.argv[1], 'rb').read()
for off in range(0, min(len(d), 1 << 20), 4):
    if d[off:off+4] == b'\x02\xb0\xad\x1b':
        print(off); break
else:
    print(-1)
PYEOF
)"
    if [ "$OFF" = "-1" ]; then
        echo "  KEIN Multiboot-Magic in $MB32 -- erst ./build-x86.sh fahren"
    elif [ "$OFF" -lt 8192 ]; then
        echo "  OK: Magic bei Dateioffset $OFF (< 8192) -- QEMU -kernel und GRUB finden den Header"
    else
        echo "  LUECKE: Magic bei $OFF (>= 8192) -- weder QEMU noch GRUB laden dieses Abbild"
    fi
else
    echo "  (kein $MB32 im Baum -- erst ./build-x86.sh fahren; Befund bleibt lesbar)"
fi

echo "--- Werkzeugseite (Z11a gehoert hierher, nicht in einen Loader) ---"
for w in tools/mkarchive.py tools/mkgpt.py tools/mkgrubiso.sh tools/checkfat.py tools/sign_manifest.py tools/dev-keys.sh; do
    if [ -f "$w" ]; then echo "  da: $w"; else echo "  FEHLT: $w"; fi
done

echo "--- GRUB-Werkzeuge (x86-Echtpfad) ---"
for w in grub-mkrescue grub-file xorriso; do
    if command -v "$w" >/dev/null 2>&1; then echo "  da: $w"; else echo "  LUECKE: '$w' fehlt (Paket grub/xorriso) -- ISO-Weg hier nicht baubar"; fi
done

echo "--- Transport je Arch (der einzige Vertrag, den der Kernel braucht) ---"
echo "  x86    : qemu-system-x86_64 -kernel <kernel>.mb32 -initrd <boot-archive>  (Multiboot-Modul -> set_archive_span)"
echo "  aarch64: qemu-system-aarch64 -kernel <kernel>.elf -device loader,file=<boot-archive>,addr=0x13F000000  (Test-Verabredung MOD_BASE)"
echo "== Entscheidung: KEIN eigener Bootloader (s. tools/lx_bootentscheidung.md) =="
