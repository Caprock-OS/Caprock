#!/usr/bin/env bash
# Baut den Caprock-Kernel fuer x86_64 (Branch arch/x86_64): eingebauter Bare-Metal-Target
# x86_64-unknown-none, Multiboot1-Image (Linker kernel/x86_64-link.ld). Ergebnis:
#   build/target/x86_64-unknown-none/release/caprock-kernel
set -euo pipefail
cd "$(dirname "$0")"

# ================================================================================================
# DIE KONFIGURATION GEHOERT INS PROTOKOLL -- sonst bindet der Fingerabdruck nur das Artefakt
# ================================================================================================
#
# „`cargo build` laeuft durch" war am 2026-08-10 in einem Arbeitsbaum KEIN Beleg: es uebersetzte
# sauber und lieferte ein Abbild mit `__text_start = 0x100000`, das nie gebootet haette. Ursache
# war die Vorfahren-Mischung von `.cargo/config.toml` (s. `kernel/build.rs`). Der
# Binaerfingerabdruck der Suiten bindet seither das **Artefakt** -- niemand band die
# **Konfiguration, die es erzeugt hat**.
#
# Deshalb steht die effektive Flagliste ab jetzt IMMER im Protokoll, nicht nur im Fehlerfall:
# eine Zeile, die nur bei Verdacht spricht, fehlt genau in dem Lauf, den man spaeter nachlesen
# will. Der Wert kommt aus `cargo config get`, also aus der GEMISCHTEN Sicht -- nicht aus dem
# Text einer einzelnen Datei.
FLAGS_EFF="$(rustup run nightly cargo -Zunstable-options config get \
    --format=json-value target.x86_64-unknown-none.rustflags 2>/dev/null || echo '(nicht lesbar)')"
echo "rustflags(effektiv): $FLAGS_EFF"

rustup run nightly cargo build --release --target x86_64-unknown-none -p caprock-kernel "$@"
ELF=build/target/x86_64-unknown-none/release/caprock-kernel
# QEMUs Multiboot1-Loader akzeptiert nur ELF32. Der ELF-Container wird auf ELF32 downgecastet
# (alle Lade-/Entry-Adressen liegen < 4 GiB; der Code bleibt 64-bit, QEMU tritt am 32-bit-`_start`
# ein, das Trampolin schaltet in den Long Mode). -> *.mb32 ist das bootbare QEMU-Image.
#
# ================================================================================================
# OFFEN, und es blockiert JEDE x86-QEMU-Messung: binutils 2.46.1 erzeugt hier ein Image, das
# QEMU nicht laedt. Gemessen am 2026-08-10, auf dem UNVERAENDERTEN Stand 86ad033.
# ================================================================================================
#
# Fehlerbild:
#     qemu-system-x86_64: Error loading uncompressed kernel without PVH ELF Note
#     -> Logdatei 0 Byte, die Suite meldet korrekt „KEIN OUTPUT ... das ist KEIN Testergebnis".
#
# Diagnose (gemessen, nicht vermutet):
#   * Der ELF64 ist in Ordnung: Multiboot-Magic bei Dateioffset **4096**, also innerhalb der
#     8 KiB, die QEMU absucht.
#   * Der ELF32 aus `objcopy` hat es bei **741376** -- weit ausserhalb. QEMU findet keinen
#     Multiboot-Header, faellt auf den Linux-/PVH-Pfad zurueck und bricht mit obiger Zeile ab.
#   * Der Grund steht in `objcopy`s eigener Meldung:
#         section `.user_data' can't be allocated in segment 5
#         warning: allocated section `.boot' not in segment
#     Segment 5 des ELF64 (`readelf -l`) ist ein LOAD mit FileSiz 0 ueber die NULLGROSSEN
#     Duplikate `.text .rodata .user_text .user_data .aptramp_data`, die lld zusaetzlich anlegt.
#     2.46.1 weigert sich, die abzubilden, und legt danach `.boot` ausserhalb jedes Segments ab.
#   * `llvm-objcopy` legt das Magic korrekt auf 4096, und QEMU laedt das Image dann auch (der
#     Banner erscheint) -- der Kernel bleibt danach aber sofort stehen. **Also kein Ersatz**,
#     sondern nur der Beleg, dass die Ursache in diesem Schritt sitzt und nicht in QEMU.
#
# Die Behebung gehoert in `kernel/x86_64-link.ld` (die nullgrossen Duplikate / Segment 5
# loswerden), nicht hierher -- und sie braucht eine eigene Messung. Bis dahin ist die x86-Suite
# auf dieser Werkzeugkette nicht lauffaehig; das ist ein AUFBAU-Problem, kein Kernelbefund.
objcopy -I elf64-x86-64 -O elf32-i386 "$ELF" "$ELF.mb32"

# ================================================================================================
# DER MULTIBOOT-HEADER MUSS IN DEN ERSTEN 8192 DATEI-BYTES LIEGEN -- und das wird GEPRUEFT
# ================================================================================================
#
# Die Bedingung steht seit jeher im Linkerskript ("in den ersten 8 KiB der Datei -> .multiboot
# zuerst") -- und **nichts hat sie durchgesetzt**. Am 2026-08-10 hat das einen halben Tag gekostet:
# der Baum baute fehlerfrei, `objcopy` meldete nichts, und QEMU sagte nur
#
#     Error loading uncompressed kernel without PVH ELF Note
#
# mit einer 0-Byte-Logdatei. Zwei Agenten haben es unabhaengig als "die Suite laeuft in diesem Baum
# nicht" gemeldet, ich habe es zuerst der falschen Ursache zugeschrieben.
#
# **Die Ursache, gemessen:** `lld` legt ein NULLGROSSES LOAD-Segment an, das die vaddr eines
# anderen dupliziert (`.data`, zweimal). GNU objcopy 2.46.1 bildet es nicht ab und legt `.boot`
# danach ausserhalb jedes Segments -- bei Dateioffset ~740 000 statt 4096. Ob das kippt, haengt an
# den GROESSEN der Sektionen; derselbe Commit kann vorher gebaut und nachher nicht mehr booten.
# Genau deshalb ist eine Pruefung noetig und ein Kommentar nicht: die Bedingung ist nicht stabil
# verletzt, sondern **grenzwertig**.
#
# Ein Bauwerkzeug, das ein unbootbares Abbild ausliefert, ist die Bauzeit-Fassung von "Schweigen
# als Erfolg".
MB_MAGIC_OFF="$(python3 - "$ELF.mb32" <<'PYEOF'
import struct, sys
d = open(sys.argv[1], 'rb').read()
# Multiboot-1-Magic, 4-Byte-ausgerichtet gesucht -- so sucht der Bootloader auch.
for off in range(0, min(len(d), 1 << 20), 4):
    if d[off:off+4] == b'\x02\xb0\xad\x1b':
        print(off); break
else:
    print(-1)
PYEOF
)"
if [ "$MB_MAGIC_OFF" = "-1" ]; then
    echo "BUILD FAILED: im mb32 steht ueberhaupt kein Multiboot-Magic (0x1BADB002)." >&2
    exit 1
fi
if [ "$MB_MAGIC_OFF" -ge 8192 ]; then
    echo "BUILD FAILED: der Multiboot-Header liegt bei Dateioffset $MB_MAGIC_OFF, erlaubt sind < 8192." >&2
    echo "  QEMU laedt dieses Abbild NICHT und meldet nur 'Error loading uncompressed kernel" >&2
    echo "  without PVH ELF Note' bei leerer Logdatei -- ein Aufbaufehler, der wie ein Haenger aussieht." >&2
    echo "  Ursache (2026-08-10 gemessen): ein NULLGROSSES LOAD-Segment von lld, das die vaddr eines" >&2
    echo "  anderen dupliziert; GNU objcopy bildet es nicht ab und schiebt .boot ans Dateiende." >&2
    readelf -lW "$ELF" | grep -E "^  LOAD" | sed 's/^/    /' >&2
    exit 1
fi
echo "multiboot: Header bei Dateioffset $MB_MAGIC_OFF (< 8192 -- QEMU findet ihn)"

# **Kein LOAD-Segment darf Dateiinhalt tragen, wo nur NOBITS-Sektionen liegen.**
#
# Diese Zeile wandelt eine Behauptung in eine geprueffte Eigenschaft. Am 2026-08-10 stand nach
# einem halben Tag Suche der Satz "der Blocker existierte als Codeproblem nie" -- belegt war aber
# nur, dass er auf frischem Zweig NICHT REPRODUZIERT. Die Beobachtungen davor (acht einander
# ueberlappende LOADs, ~1 MiB `filesz` fuer nachweislich NOBITS-Sektionen) waren echte
# readelf-Ausgaben IRGENDEINES Binaries. War es der veraltete Stand: gut. Macht aber irgendeine
# Kombination aus Sektionsreihenfolge und Skript das Layout REIHENFOLGEABHAENGIG, kommt der Fall
# wieder -- und dann fehlte die Zeile, die ihn sofort erkennt.
#
# Geprueft wird die Form, an der er sich zeigte, nicht die vermutete Ursache: ein Segment, dessen
# Adressbereich ausschliesslich von NOBITS-Sektionen belegt ist, aber `filesz > 0` hat. Das ist
# genau die Signatur "der Linker schreibt den BSS-Bereich als Dateiinhalt aus, um etwas dahinter
# zu platzieren".
python3 - "$ELF" <<'PYEOF' || exit 1
import subprocess, sys, re
elf = sys.argv[1]

# **Die Zuordnung Sektion -> Segment wird GELESEN, nicht nachgerechnet.**
# Die erste Fassung dieser Pruefung leitete sie aus vaddr-Bereichen ab -- und ordnete dem Segment
# bei 0x9000 prompt `.boot`, `.text` und `.rodata` zu, weil dessen `memsz` den ganzen Bildbereich
# ueberspannt (VMA 0x9000, aber die BSS-Reservierung liegt hoch). Ein Pruefer, der die gepruefte
# Groesse nachrechnet statt sie zu lesen, prueft eine zweite Wirklichkeit -- dieselbe Falle wie
# `iova_window_clear_of_msi`. readelf druckt die Zuordnung; sie ist die eine Quelle.
aus = subprocess.run(["readelf","-lW",elf],capture_output=True,text=True).stdout

art = {}
for l in subprocess.run(["readelf","-SW",elf],capture_output=True,text=True).stdout.split("\n"):
    m = re.match(r"\s*\[\s*\d+\]\s+(\S+)\s+(\S+)\s+[0-9a-f]+\s+[0-9a-f]+\s+([0-9a-f]+)", l)
    if m and m.group(2) in ("PROGBITS","NOBITS"):
        art[m.group(1)] = (m.group(2), int(m.group(3),16))

lo = []
for l in aus.split("\n"):
    m = re.match(r"\s*LOAD\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)", l)
    if m:
        lo.append((int(m.group(2),16), int(m.group(4),16)))   # vaddr, filesz

zuordnung = []
in_map = False
for l in aus.split("\n"):
    if "Section to Segment mapping" in l: in_map = True; continue
    if in_map:
        m = re.match(r"\s*(\d+)\s+(.*)$", l)
        if m: zuordnung.append(m.group(2).split())
        elif l.strip() == "": continue

if not lo or not zuordnung:
    print("BUILD FAILED: LOAD-Segmente oder Sektionszuordnung nicht lesbar -- die Pruefung konnte "
          "nicht laufen. Das ist KEIN bestandener Test.", file=sys.stderr)
    sys.exit(1)

# Die Zuordnungsliste zaehlt ALLE Segmenttypen; nur die LOADs interessieren, in ihrer Reihenfolge.
loads_idx = [i for i, l in enumerate(aus.split("\n")) if re.match(r"\s*LOAD\s", l)]
typ_folge = [l.split()[0] for l in aus.split("\n") if re.match(r"\s*(LOAD|PHDR|NOTE|GNU_\S+|TLS|INTERP|DYNAMIC)\s", l)]
schlecht = 0
li = 0
for i, sektionen in enumerate(zuordnung):
    if i >= len(typ_folge) or typ_folge[i] != "LOAD":
        continue
    if li >= len(lo): break
    vaddr, filesz = lo[li]; li += 1
    echte = [x for x in sektionen if art.get(x, ("?",0))[1] > 0]
    if filesz > 0 and echte and all(art[x][0] == "NOBITS" for x in echte):
        print(f"BUILD FAILED: LOAD @ {vaddr:#x} traegt {filesz} Byte Dateiinhalt, enthaelt aber "
              f"nur NOBITS-Sektionen ({', '.join(echte)}).", file=sys.stderr)
        print("  Signatur eines reihenfolgeabhaengigen Layouts: der Linker schreibt den", file=sys.stderr)
        print("  BSS-Bereich als Datei aus, um etwas dahinter zu platzieren. S. TODO.md.", file=sys.stderr)
        schlecht += 1
sys.exit(1 if schlecht else 0)
PYEOF
echo "multiboot: $(readelf -lW "$ELF" | grep -c '^  LOAD') LOAD-Segmente, keines traegt Dateiinhalt ueber reinem NOBITS"
echo "x86_64-Kernel: $ELF (ELF64) + $ELF.mb32 (Multiboot-ELF32 fuer 'qemu-system-x86_64 -kernel')"
