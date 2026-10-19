#!/usr/bin/env bash
# **Z11a, zweite Haelfte (Werkzeugseite): Treiber von der Platte — gebaut, nicht behauptet.**
#
# Stand in `TODO.md` Z11a: der Bootloader-Weg ist belegt (`tools/mkgrubiso.sh`), der
# Plattentreiber laeuft als geladenes Userland-Programm — was fehlt, ist alles darueber:
# Partitionstabelle, Dateisystem, Manifest-Eintrag. Dieses Werkzeug baut genau das, aus den
# vorhandenen Teilen, ohne einen einzigen neuen Parser:
#
#   1. `tools/mkgpt.py` schreibt GPT + lesbares FAT16 (P0: Lieferschein als Textdatei).
#   2. Das Treiber-Image liegt als rohe Sektoren in P1 (`dd`, kein Dateisystem noetig —
#      der Blocktreiber liest Bereiche, keine Dateien).
#   3. `tools/lx_driver_manifest.py` bindet das Image an Herkunft + Hash + Schluessel,
#      Herkunftsform `range:START:LEN` (deterministisch, kein GPT-Parse zur Manifestzeit;
#      `guid:` ginge erst, wenn die Partitions-GUID bis hierher durchgereicht wird — offen,
#      nicht gebraucht).
#   4. Geprueft wird doppelt: `--check` (Zeuge + Bild) UND rueckgelesene Sektoren (was auf
#      der Platte liegt, ist bytegleich zum signierten Image — kein `dd`-Versatz).
#
# **Kein Bootloader.** Dieses Skript fasst weder GRUB noch Multiboot noch `mkgrubiso.sh` an;
# es druckt nur die QEMU-`-drive`-Zeile aus, mit der die Platte anhaengbar ist. Wer booten
# will, nimmt den Bootloader-Weg (Z11a, erste Haelfte).
#
# Aufruf:
#   tools/lx_z11a_platte.sh --treiber e1000e --image build/drv.lxpd --out build/drv-platte.img \
#       --eintrag build/drv.entry [--key keys/manifest-test.manifest.pub] [--api X1]
#   tools/lx_z11a_platte.sh --selbsttest   # alles in /tmp, ohne QEMU, Austritt 0/1
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

P0_FIRST=34; P0_LAST=4400          # FAT16-Lieferschein (Clusterzahl s. mkgpt.py: braucht 4085..65524)
P1_FIRST=4401                      # Rohbereich des Treibers (Start, fest — das Manifest nennt ihn)
DISK_SECTORS=8192                  # 4 MiB; P1-Ende muss vor 8158 bleiben (Sicherungskopie am Ende)
API="X1"
KEY="keys/manifest-test.manifest.pub"

usage() { echo "Aufruf: $0 --treiber NAME --image DATEI --out PLATTE --eintrag EINTRAG [--key PUB] [--api X1] | $0 --selbsttest" >&2; exit 2; }

bauen() { # $1=treiber $2=image $3=out $4=eintrag
    local treiber="$1" image="$2" out="$3" eintrag="$4"
    [ -f "$image" ] || { echo "FEHLER: Treiber-Image '$image' fehlt." >&2; return 1; }
    [ -f "$KEY" ] || { echo "FEHLER: Schluessel '$KEY' fehlt." >&2; return 1; }
    local groesse len ende
    groesse="$(stat -c%s "$image")"
    len=$(( (groesse + 511) / 512 ))
    ende=$(( P1_FIRST + len - 1 ))
    if [ "$ende" -gt 8158 ]; then
        echo "FEHLER: Image braucht $len Sektoren (P1 $P1_FIRST..$ende), Platte endet bei 8158." >&2
        return 1
    fi
    local sha kid schein
    sha="$(sha256sum "$image" | cut -d' ' -f1)"
    kid="$(python3 -c "import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],'rb').read()).digest()[:16].hex())" "$KEY")"
    schein="treiber=$treiber;sha256=$sha;key_id=$kid;bereich=$P1_FIRST:$len"
    python3 tools/mkgpt.py "$out" --sectors "$DISK_SECTORS" \
        --part "$P0_FIRST:$P0_LAST" --part "$P1_FIRST:$ende" \
        --fat16 "$P0_FIRST:$P0_LAST" --file "LIEFER.TXT=$schein" || return 1
    dd if="$image" of="$out" bs=512 seek="$P1_FIRST" conv=notrunc status=none || return 1
    python3 tools/lx_driver_manifest.py --driver "$treiber" --api "$API" \
        --source "range:$P1_FIRST:$len" --image "$image" --key "$KEY" --out "$eintrag" || return 1
    python3 tools/lx_driver_manifest.py --check "$eintrag" --image "$image" --key "$KEY" >&2 || return 1
    # Ruecklesen: was auf der Platte liegt, ist bytegleich zum signierten Image.
    python3 - "$out" "$image" "$P1_FIRST" "$len" <<'EOF' || return 1
import hashlib, sys
platte, bild, start, n = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
with open(platte, 'rb') as f:
    f.seek(start * 512)
    sektoren = f.read(n * 512)
roh = open(bild, 'rb').read()
assert sektoren[:len(roh)] == roh, "Plattenbereich != Treiber-Image (dd-Versatz?)"
assert hashlib.sha256(roh).hexdigest() == hashlib.sha256(sektoren[:len(roh)]).hexdigest()
print("lx_z11a_platte: Ruecklesen BESTANDEN (%d B, %d Sektor(en) ab %d)" % (len(roh), n, start), file=sys.stderr)
EOF
    echo "lx_z11a_platte: FERTIG — Platte '$out', Eintrag '$eintrag' (Treiber '$treiber', range:$P1_FIRST:$len)"
    echo "QEMU: -drive file=$out,format=raw,if=none,id=lxpdblk"
    echo "Block-PD liest range:$P1_FIRST:$len (kein Bootloader beteiligt — Bootloader-Weg s. tools/mkgrubiso.sh)"
}

selbsttest() {
    local tmp; tmp="$(mktemp -d "${TMPDIR:-/tmp}/lxz11a.XXXXXX")"
    trap 'rm -rf "${tmp:-}"' EXIT
    # Stellvertreter-Treiber: 3 Sektoren deterministische Bytes (kein QEMU, kein echter Treiber).
    python3 -c "open('$tmp/drv.lxpd','wb').write(bytes((i*31+7)&0xFF for i in range(3*512)))"
    if ! bauen "selbsttest-treiber" "$tmp/drv.lxpd" "$tmp/platte.img" "$tmp/drv.entry"; then
        echo "LX-Z11A-SELBSTTEST: FAILURES"; return 1
    fi
    # Negativprobe: ein Byte in der Platte kippen — das Ruecklesen muss es sehen.
    python3 -c "d=open('$tmp/platte.img','r+b'); d.seek($P1_FIRST*512+100); d.write(b'\x00' if open('$tmp/drv.lxpd','rb').read()[100:101]!=b'\x00' else b'\xff')"
    if python3 - "$tmp/platte.img" "$tmp/drv.lxpd" "$P1_FIRST" 3 <<'EOF' 2>/dev/null; then
import sys
platte, bild, start, n = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
with open(platte, 'rb') as f:
    f.seek(start * 512)
    sektoren = f.read(n * 512)
roh = open(bild, 'rb').read()
assert sektoren[:len(roh)] == roh
EOF
        echo "LX-Z11A-SELBSTTEST: FAILURES (gekipptes Byte nicht bemerkt)"; return 1
    fi
    echo "LX-Z11A-SELBSTTEST: ALL PASS"
}

if [ "${1:-}" = "--selbsttest" ]; then selbsttest; exit "$?"; fi
TREIBER=""; IMAGE=""; OUT=""; EINTRAG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --treiber) TREIBER="$2"; shift 2;;
        --image) IMAGE="$2"; shift 2;;
        --out) OUT="$2"; shift 2;;
        --eintrag) EINTRAG="$2"; shift 2;;
        --key) KEY="$2"; shift 2;;
        --api) API="$2"; shift 2;;
        *) usage;;
    esac
done
[ -n "$TREIBER" ] && [ -n "$IMAGE" ] && [ -n "$OUT" ] && [ -n "$EINTRAG" ] || usage
bauen "$TREIBER" "$IMAGE" "$OUT" "$EINTRAG"
