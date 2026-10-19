#!/usr/bin/env bash
# **Strang A** — Startmenge, Manifest, Root-Task auf x86_64 (todo-A-ausfuehren.md, A-1/A-2).
#
# Getrennt von `test-qemu-x86.sh`, und das ist Absicht: jene Datei gehoert Strang B
# (Dateibesitz-Regel in todo-A-ausfuehren.md), und beide Straenge arbeiten parallel. Sobald die
# Straenge zusammenlaufen, gehoeren die Pruefungen hier in die Hauptsuite -- bis dahin waere ein
# gemeinsamer Schreibzugriff auf dieselbe Datei die teuerste Art, Zeit zu verlieren.
#
# Was hier geprueft wird und in `test-qemu-x86.sh` NICHT vorkommt:
#   * Multiboot-Module: der Bootloader liefert die Startmenge, der Allokator laesst sie in Ruhe.
#   * System-Manifest: signiert, an DIESES Kernel-Image gebunden, Anti-Downgrade, Live-Oracle.
#   * Root-Task: aus dem Manifest ausgewaehlt, Hash geprueft, Wurzel-Caps endowt -- und er laedt
#     seinerseits ueber SEINE Loader-Cap ein weiteres Programm.
#   * Negativfaelle: kein Manifest, falscher Hash, fremder Kernel. Ein Gate, das nur bei gueltiger
#     Eingabe geprueft wird, ist kein geprueftes Gate.
set -uo pipefail
cd "$(dirname "$0")"

SECONDS_RUN="${1:-120}"
RAM="${2:-512M}"

if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    # `host-cache-info=on` wie in `test-qemu-x86.sh`, und aus demselben Grund dort ausfuehrlich
    # begruendet: ohne den Schalter meldet QEMU eine synthetische Cache-Geometrie. Zwei Suiten, die
    # dieselbe Maschine verschieden beschreiben, sind derselbe Riss wie zwei, die dasselbe Geraet
    # verschieden aufsetzen -- das hat dieses Projekt schon einmal einen halben Tag gekostet.
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
    echo "== Beschleunigung: KVM (-cpu host) =="
else
    ACCEL=(-cpu Skylake-Client)
    echo "== Beschleunigung: TCG (kein /dev/kvm) =="
fi

KELF=build/target/x86_64-unknown-none/release/caprock-kernel
PROG=programs/build/target/x86_64-caprock-user/release
MANKEY=keys/manifest-test.manifest.ed25519

python3 -c "import cryptography" 2>/dev/null || {
    echo "== FEHLT: das Python-Paket 'cryptography' (signieren geht ohne nicht) =="
    echo "   pip3 install --user cryptography   # oder --break-system-packages auf Debian"
    exit 2
}

# **Schluessel aus einem frischen Clone.** `keys/` ist gitignored -- ohne diesen Schritt waere die
# Suite aus einem Clone nicht lauffaehig, und genau diese Fehlerform hat das Projekt schon
# mehrfach bezahlt (docs/plan-betriebsbereit.md, Stufe 0, Punkt 2). Vorhandene Schluessel bleiben
# unangetastet; fehlt das Paar, entsteht es und die in den Kernel kompilierte Key-DB wird neu
# geschrieben (danach MUSS neu gebaut werden -- deshalb steht das hier vor dem Build).
echo "== Manifest-Schluessel =="
python3 tools/gen_manifest_key.py --ensure || exit 2

echo "== build (Kernel, --features selftest) =="
# `--features selftest` ausdruecklich (nicht ueber `default`): seit A-2.2 ist `selftest` NICHT mehr
# in `default`. Diese Suite prueft den Root-Task-Pfad aus Ring 3 (zweites Badge, SYS_CDELETE) und
# das saubere `system_off` -- alles Aussagen, die der Testharness (`system::testsupport`) meldet.
# Ohne das Feature laedt der Kernel den Root-Task und geht dann in `idle()`: kein Bericht, kein
# `system_off`, Timeout. Das gebootete Image einer Test-Suite braucht den Harness -- dieselbe Zeile,
# die B fuer test-qemu-x86.sh eingezogen hat.
./build-x86.sh --features selftest >/dev/null 2>&1 || { echo "BUILD FAILED (Kernel)"; exit 1; }

echo "== build (Programme, x86_64-caprock-user) =="
( cd programs && rustup run nightly cargo build --release --target x86_64-caprock-user.json ) \
    >/dev/null 2>&1 || { echo "BUILD FAILED (Programme)"; exit 1; }

echo "== zertifizieren (TrustedSAS: init) =="
mkdir -p certs build
# Der Root-Task ist TrustedSAS (er darf eine Loader-Cap halten) -> ohne gueltiges, auf genau
# dieses Binary gebundenes Zertifikat laedt der Kernel ihn nicht (ADR 0014). Das Zertifikat kommt
# aus DEMSELBEN Werkzeug wie auf ARM; ohne den privaten TrustedSAS-Schluessel entsteht keines.
TRUSTKEY=keys/trusted-test.ed25519
# Geprueft wird, ob der Schluessel PASST -- nicht bloss, ob er da ist.
#
# `kernel/src/trusted_keys.rs` ist versioniert und traegt den oeffentlichen Schluessel dessen,
# der ihn zuletzt erzeugt hat; der private unter `keys/` ist es nicht (zu Recht). Wer einen
# fremden `trusted_keys.rs` zieht, waehrend lokal ein eigener privater Schluessel liegt, hat
# zwei Haelften, die nicht zusammengehoeren. Die alte Pruefung (`[ ! -f ]`) sah das nicht, und
# der Lauf endete mit
#     root    : FAILURES (Rejected(Unverified))
# -- einer Zeile, die wie ein Testergebnis aussieht und ein Aufbauproblem ist. Am 2026-08-01
# genau so aufgetreten, nach einem Pull vom Server.
#
# rc=2 heisst "nicht entscheidbar" und wird NICHT als in Ordnung gewertet: ein Pruefer, der
# nicht pruefen konnte, hat nichts belegt.
python3 tools/check_trusted_key.py
KEYRC=$?
if [ "$KEYRC" -eq 2 ]; then
    echo "  FEHLER: der TrustedSAS-Schluessel liess sich nicht gegen den eingebetteten pruefen."
    echo "          Das ist KEIN Testergebnis -- ohne diese Pruefung waere ein spaeteres"
    echo "          'Rejected(Unverified)' nicht von einem echten Befund zu unterscheiden."
    exit 2
fi
if [ "$KEYRC" -ne 0 ]; then
    # `gen_trusted_key.py` weigert sich, einen vorhandenen privaten Schluessel zu
    # ueberschreiben ("loeschen zum Neu-Erzeugen") -- eine bewusste Sicherung, die richtig ist.
    # Also beiseitelegen statt loeschen: ein privater Schluessel ist nichts, was ein Testskript
    # unwiderruflich wegwerfen darf, auch kein Testschluessel. Wer ihn doch braucht, findet ihn
    # unter dem Zeitstempel wieder.
    if [ -f "$TRUSTKEY" ]; then
        BEISEITE="$TRUSTKEY.passt-nicht-$(date +%Y%m%d-%H%M%S)"
        mv "$TRUSTKEY" "$BEISEITE"
        [ -f "$TRUSTKEY.pub" ] && mv "$TRUSTKEY.pub" "$BEISEITE.pub"
        echo "  (alter Schluessel beiseitegelegt: $BEISEITE)"
    fi
    python3 tools/gen_trusted_key.py --name trusted-test >/dev/null 2>&1 || {
        echo "  FEHLER: TrustedSAS-Schluessel liess sich nicht erzeugen"; exit 2; }
    echo "  (Schluessel fehlte oder passte nicht -> neu erzeugt, kernel/src/trusted_keys.rs"
    echo "   regeneriert, Kernel wird neu gebaut. Die Aenderung an trusted_keys.rs ist LOKAL"
    echo "   und gehoert nicht committet, solange nicht alle denselben Schluessel nutzen.)"
    ./build-x86.sh --features selftest >/dev/null 2>&1 || { echo "BUILD FAILED (nach Key-Regen)"; exit 1; }
    python3 tools/check_trusted_key.py || {
        echo "  FEHLER: auch nach dem Neuerzeugen passt der Schluessel nicht -- hier stimmt"
        echo "          etwas Grundsaetzliches nicht (gen_trusted_key.py? Pfade?)"; exit 2; }
fi
python3 tools/sign_trusted.py --crate programs/trusted/init --elf "$PROG/init.elf" \
    --program-id 1 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/init-x86.cert >/dev/null 2>&1 || { echo "  FEHLER: init liess sich nicht zertifizieren"; exit 2; }
# A-6.3: die Dateisystem-PD ist TrustedSAS (s. programs/trusted/fs/Cargo.toml, warum das ein
# Kompromiss ist) -> sie braucht ebenfalls ein Zertifikat, sonst weist das Gate sie ab.
python3 tools/sign_trusted.py --crate programs/trusted/fs --elf "$PROG/fs.elf" \
    --program-id 4 --version 1 --policy internal-test --key "$TRUSTKEY" \
    --out certs/fs-x86.cert >/dev/null 2>&1 || { echo "  FEHLER: fs liess sich nicht zertifizieren"; exit 2; }

# --- Plattenabbild fuer die Treiber-PD (A-5.1) ---------------------------------------------------
#
# Derselbe Inhalt wie in `test-qemu-x86.sh`, und aus demselben Grund: ein Puffer voller Nullen ist
# von einem NIE BESCHRIEBENEN Puffer nicht zu unterscheiden. Der Unterschied hier ist, WER liest --
# nicht der Kernel, sondern ein geladenes Userland-Programm. Geprueft wird die Magie trotzdem vom
# Kernel: er hat die DMA-Region ausgegeben und kennt den erwarteten Inhalt. Der Treiber meldet nur,
# dass seine Transaktion durchlief. Zwei Quellen, keine davon allein ausreichend.
BLK_IMG="$(mktemp)"
BLK_SECTORS=32768
# Seit A-6.2/A-6.3 eine **echte GPT mit zwei Partitionen**, gebaut von `tools/mkgpt.py`:
#   Partition 1 (34..20000): ein lesbares FAT16 mit HELLO.TXT (A-6.3)
#   Partition 2 (20001..):   roh, traegt die Magie (`MAGIC_LBA`)
# Getrennt, weil die Magie sonst auf dem FAT-Bootsektor laege.
#
# **Selbst gebaut statt `sgdisk`/`parted` aufgerufen**, und das ist kein Eigensinn: eine Suite, die
# an einem Fremdwerkzeug haengt, faellt auf einem Rechner ohne dieses Werkzeug als "Test rot" aus
# statt als "Aufbau unvollstaendig" -- eine Verwechslung, die dieses Projekt schon mehrfach
# bezahlt hat. Und nur mit einem eigenen Werkzeug laesst sich der Negativfall herstellen
# (`--break`), gegen den der Parser abgenommen wird.
BLK_PART1_LBA=34
BLK_PART1_SECTORS=19967
python3 tools/mkgpt.py "$BLK_IMG" --sectors "$BLK_SECTORS" \
    --part "$BLK_PART1_LBA:20000" --part 20001:32700 --magic-at 20001 \
    --fat16 "$BLK_PART1_LBA:20000" --file "HELLO.TXT=CAPROCKS-DATEIINHALT" \
    || { echo "  FEHLER: GPT-Abbild liess sich nicht bauen"; exit 2; }

# --- Das Boot-Image: Kernel + EINE Datei ---------------------------------------------------------
#
# Eintrag 1 = init       (TrustedSAS, Root-Task, Loader- + Notification-Cap)
# Eintrag 2 = hello      (UserLand) -- das, was init von sich aus nachlaedt.
# Eintrag 3 = virtio-blk (HardwareLand, A-5.1) -- der erste TREIBER als geladenes Programm.
#
# Sein Manifest-Eintrag ist die eigentliche Aussage: `mmio,dma` heisst "diese Komponente darf ein
# Registerfenster und eine DMA-Region halten". WELCHES Geraet das ist, steht nicht da -- die
# Instanz teilt der Kernel-Glue zu (das Manifest legt die Art der Autoritaet fest, nicht die
# Instanz). `ntfn` ist sein Meldekanal; ohne den koennte er nichts berichten, und ein Treiber, von
# dem man nichts hoert, ist von einem nicht gestarteten nicht zu unterscheiden.
#
# HardwareLand braucht KEIN Zertifikat: das Gate (ADR 0014) gilt fuer TrustedSAS, weil dort
# Vertrauen die Autoritaet traegt. Hier traegt sie die Cap.
build_archive() {   # $1 = Ausgabedatei, $2 = Kernel-ELF, $3 = manifest-version, $4 = Geraete-Selektor
    # $4 ist der A-5.3-Selektor der Treiber-PD. Vorgabe: das Blockgeraet. Die Negativfaelle unten
    # setzen ihn auf etwas Unerfuellbares bzw. auf die Netzkarte -- und brauchen dafuer das VOLLE
    # Archiv, sonst erreicht der Lauf den Bericht gar nicht und die Aussage waere ein Timeout.
    local sel="${4:-vendor=1af4,device=1042}"
    python3 tools/sign_manifest.py --kernel "$2" --key "$MANKEY" --manifest-version "$3" \
        --out build/system.manifest \
        --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:1::any:0" \
        --entry "2:hello:2:1:$PROG/hello.elf:ntfn,ep:stripe,service:2::any:0" \
        --entry "3:virtio-blk:1:1:$PROG/virtio-blk.elf:mmio,dma,ntfn,ep::1::any:0:$sel" \
        --entry "4:fs:0:1:$PROG/fs.elf:ntfn,ep,shared::1::any:0::3" \
        --entry "5:virtio-net:1:1:$PROG/virtio-net.elf:mmio,dma,ntfn,ep::1::any:0:vendor=1af4,device=1041" \
        --entry "6:wasmhost:2:1:$PROG/wasmhost.elf:ntfn,ep::1::any:0::2" \
        --entry "7:ipcperm-srv:2:1:$PROG/ipcperm-srv.elf:ntfn,ep:service:1::any:0" \
        --entry "8:ipcperm-cli:2:1:$PROG/ipcperm-cli.elf:ntfn,ep::1::any:0::7" \
        >/dev/null 2>&1 || return 1
    python3 tools/mkarchive.py "$1" --system-manifest build/system.manifest \
        "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" \
        "2:hello:2:1:$PROG/hello.elf" \
        "3:virtio-blk:1:1:$PROG/virtio-blk.elf" \
        "4:fs:0:1:$PROG/fs.elf::certs/fs-x86.cert" \
        "5:virtio-net:1:1:$PROG/virtio-net.elf" \
        "6:wasmhost:2:1:$PROG/wasmhost.elf" \
        "7:ipcperm-srv:2:1:$PROG/ipcperm-srv.elf" \
        "8:ipcperm-cli:2:1:$PROG/ipcperm-cli.elf" >/dev/null 2>&1
}

echo "== Boot-Archiv bauen =="
build_archive build/boot-archive-x86.bin "$KELF" 1 || { echo "  FEHLER: Archiv/Manifest"; exit 2; }

# $1 = Archiv (leer = keines), $2 = Logdatei, $3 = Zeitlimit (Vorgabe: $SECONDS_RUN)
#
# Die Negativfaelle bekommen bewusst ein KURZES Limit: dort erreicht der Kernel `SELFTEST COMPLETE`
# nie (es gibt keinen Root-Task), er laeuft in den Watchdog. Die Zeilen, um die es geht, stehen
# aber im ersten Boot-Abschnitt. Das volle Limit abzuwarten hiesse, dreimal auf einen Watchdog zu
# warten, dessen Ausgang schon feststeht.
# `disable-legacy=on,iommu_platform=on` am RNG ist kein Detail, sondern die Bedingung dafuer, dass
# diese Suite ueberhaupt fertig werden kann.
#
# Ohne die Schalter ist `virtio-rng-pci` auf x86 *transitional* und bietet
# `VIRTIO_F_ACCESS_PLATFORM` nicht an. Der Treiber bricht dann ab -- richtig so, denn ein Geraet
# ohne dieses Bit greift an der IOMMU vorbei, und ein Rueckfall auf physische Adressen waere
# genau die Achsenverwechslung, gegen die `Pa`/`Iova` getrennt sind. Nur: `virtio` steht in
# `all_done()`, also wurde der Lauf nie fertig, lief in den Watchdog und `SELFTEST COMPLETE` blieb
# aus. Das sah wie ein Haenger aus und war eine Geraetekonfiguration.
#
# Die Zeile stammt aus der Zeit vor der `virtio`-Pruefung (2026-08-01); `test-qemu-x86.sh` bekam
# die Schalter damals, diese Suite nicht. Zwei Suiten, die dasselbe Geraet verschieden aufsetzen,
# sind ein Riss, durch den genau so etwas faellt.
boot() {
    local extra=()
    [ -n "$1" ] && extra=(-initrd "$1")
    # **`$BOOT_EXTRA` ist die NUMA-Tuer** (Z8/N4): eine Topologie laesst sich nur beim Start
    # setzen, und die Manifest-Politik ist ohne sie nicht pruefbar. Vorgabe leer -- jeder
    # vorhandene Aufruf verhaelt sich unveraendert.
    local nx=(); [ -n "${BOOT_EXTRA:-}" ] && read -r -a nx <<<"$BOOT_EXTRA"
    # **`-m`/`-smp` sind ueberschreibbar, und das war ein BEFUND** (Z8/N4, 2026-08-17).
    #
    # Sie standen hier fest verdrahtet, VOR den Zusatzargumenten. Eine `-numa`-Topologie braucht
    # aber `-smp sockets=…` und `memory-backend`-Groessen, die zu `-m` passen -- ein zweites
    # `-m`/`-smp` dahinter kollidiert. Gemessen: QEMU startet, der Gast bekommt **kein SRAT**, und
    # die Kernelzeile liest sich danach wie ein Befund (`numa : readable=false`). Dieselbe Rissform
    # wie `iommu_platform=on`: zwei Suiten (hier: zwei Aufrufwege) setzen dasselbe Geraet
    # verschieden auf, und der Ausfall zeigt auf den Kernel statt auf den Aufbau.
    local mem=(-m "$RAM"); [ -n "${MEM_ARGS:-}" ] && read -r -a mem <<<"$MEM_ARGS"
    local smp=(-smp 4);    [ -n "${SMP_ARGS:-}" ] && read -r -a smp <<<"$SMP_ARGS"
    # **Und QEMUs eigene Meldungen gehen NICHT mehr nach /dev/null.** Genau das hat die Verwirrung
    # oben moeglich gemacht: eine abgewiesene Kommandozeile sah aus wie ein stummer Kernel. Sie
    # landen in einer Datei, die der Aufrufer bei leerer Ausgabe zeigt.
    QEMU_ERR="${QEMU_ERR:-$(mktemp)}"
    # **`intremap=on` seit 2026-08-26 (Stufe B).** Bis dahin stand es NUR in der Hauptsuite --
    # also ausgerechnet in der Suite OHNE Geraete. `irte_vergabe_moeglich()` verlangt
    # `ir_active()`; ohne diese Option meldet `irtevgb` hier SKIP, und die IRTE-Vergabe, an der
    # die MSI-Zustellung haengt, war in der einzigen Suite mit Treibern nicht scharf. Dieselbe
    # Rissform wie `iommu_platform=on`.
    #
    # **Der Kommentar steht HIER und nicht in der Befehlszeile**: ein `#` innerhalb einer
    # Backslash-Fortsetzung bricht sie auf, und `-machine ...` wird zu einem eigenen Befehl
    # ("Kommando nicht gefunden"). Gemessen, nicht erschlossen -- genau das ist beim ersten
    # Anlauf passiert.
    timeout "${3:-$SECONDS_RUN}" qemu-system-x86_64 \
        -kernel "$KELF.mb32" "${mem[@]}" "${smp[@]}" "${ACCEL[@]}" "${nx[@]}" \
        -machine q35,kernel-irqchip=split -device intel-iommu,caching-mode=on,intremap=on \
        -device virtio-rng-pci,disable-legacy=on,iommu_platform=on \
        -drive if=none,id=blk0,format=raw,file="$BLK_IMG" \
        -device virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on \
        -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
        -netdev user,id=n0,restrict=on "${extra[@]}" \
        -nographic -serial file:"$2" -no-reboot \
        </dev/null >/dev/null 2>"$QEMU_ERR" || true
}

LOG="$(mktemp)"
echo "== boot ($SECONDS_RUN s) =="
boot build/boot-archive-x86.bin "$LOG"
OUT="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$LOG" 2>/dev/null)"
# **Die neuen Urteilszeilen immer zeigen** -- sie sind das Ergebnis, nicht Diagnose. Eine Zeile,
# die man nur im Fehlerfall zu sehen bekommt, laesst sich nicht gegenlesen.
echo "$OUT" | grep -E "^(pdcolor|ladepol)" || true
if [ -z "$OUT" ]; then
    echo "== KEIN OUTPUT -- das ist KEIN Testergebnis, sondern ein Aufbauproblem (QEMU? Zeitlimit?) =="
    # **QEMUs eigene Worte, statt Raten.** Eine abgewiesene Kommandozeile (falsches `-numa`, ein
    # zweites `-m`) sieht ohne diese Zeilen aus wie ein stummer Kernel.
    if [ -s "${QEMU_ERR:-/dev/null}" ]; then
        echo "-- QEMU sagt dazu: --"; sed -n '1,20p' "$QEMU_ERR"
    else
        echo "-- QEMU hat nichts gesagt (also kein Aufrufproblem; eher Zeitlimit oder Haenger) --"
    fi
    rm -f "$LOG" "$BLK_IMG" "${QEMU_ERR:-}"; exit 2
fi
# `wasm` und `clientn` gehoeren hierher: sie sind ERGEBNISzeilen. Eine Zeile, die man nur im
# abgelegten Fehlerprotokoll zu sehen bekommt, laesst sich nicht gegenlesen -- und das abgelegte
# Protokoll ist der LETZTE Boot (ein Negativfall), nicht der Hauptlauf. Genau daran habe ich am
# 2026-08-10 eine Diagnose aus dem falschen Boot gelesen.
echo "$OUT" | grep -E "^(mbi|mbmod|archive|manifest|clientn|root|devassign|devsel|dmaiso|drv|blkdev|part|fs|wasm|bootckpt|irqmsi|tls) *:" || true
# **`el0-trap` gehoert IMMER ins Protokoll** (todo D17): ein Fault, den die Positivliste
# verschweigt, macht "das Programm laeuft nicht" von "das Programm ist gestorben"
# ununterscheidbar -- und genau daran ist die T5-Suche haengengeblieben.
echo "$OUT" | grep -E "^el0-trap" || true

echo "== checks =="
# ================================================================================================
# FINGERPRINT + BEKANNT-ROTE ZEILEN
# ================================================================================================
#
# **Der Fingerprint schliesst eine Hypothese fuer immer aus.** Am 2026-08-09 war ein roter Lauf
# nicht zuzuordnen: "Flattern oder veralteter Build" -- und *oder* ist keine Diagnose. Steht der
# Hash des gerade gepruefen Binaries in der Ausgabe, ist die zweite Haelfte nie wieder zu fragen.
#
# **Die known-red-Liste macht aus einer benannten Auslassung einen BEWACHTEN Zustand.** Eine rote
# Zeile ausserhalb des Gates ist genau der Zustand, in dem die Lade-Suite unbemerkt kippte: "bekannt
# rot" und "neu rot" sahen gleich aus. Jede rote Zeile, die NICHT auf der Liste steht, faerbt den
# Lauf; jede Zeile AUF der Liste traegt ein Datum -- eine Diagnose, die aelter ist als der letzte
# Umbau ihres Pfads, ist automatisch verdaechtig.
#
# Format: 'praefix|seit|eintrag|diagnose vom'
#
# **EINFACHE Anfuehrungszeichen, und das ist keine Stilfrage.** In doppelten Anfuehrungszeichen ist
# ein Backtick-Paar Kommandosubstitution: aus `Root-Task lief: false` wurde bis 2026-08-10 die
# Fehlermeldung "Kommando nicht gefunden" auf stderr und ein LEERER String im Text. Getroffen hat
# es ausgerechnet die zitierten Belege -- die Diagnose las sich vollstaendig und war es nicht.
# Dieselbe Form wie die verschluckten Pruefergebnisse eine Ebene hoeher: der Text sagt weniger,
# als er zu sagen scheint, und niemand sieht die Luecke.
BEKANNT_ROT=(
  # **LEER, und das ist ein Ergebnis.** Bis zum 2026-08-10 standen hier `wasm` und `vollzahl`.
  # Beide sind mit derselben Behebung gruen geworden (`.bss : ALIGN(4096)` in den beiden
  # Programm-Linkerskripten -- ein PT_LOAD an einer krummen VA, das der Lader korrekt abwies).
  # `vollzahl` war ausdruecklich als REGRESSIONSWAECHTER fuer `wasm` eingetragen: "diese Zeile MUSS
  # gruen werden, sobald der Ladefehler behoben ist" -- sie ist es, in demselben Lauf.
  #
  # Herausgenommen hat sie die Veraltungserkennung dieser Liste selbst, nicht ein Gegenlesen. Ein
  # Register, das Erledigtes fuehrt, macht die Frage "was ist offen" unbeantwortbar, und ein
  # known-red-Eintrag mit ueberholter Diagnose liest sich wie Wissen und ist eine Spur ins Leere.
)
fingerprint() {
    local f="$1"
    if [ -f "$f" ]; then
        printf '%s %s' "$(sha256sum "$f" | cut -c1-12)" "$(stat -c %y "$f" 2>/dev/null | cut -d. -f1)"
    else
        printf 'KEIN-BINARY'
    fi
}
# Rote Zeilen gegen die Liste halten. Gibt 1, wenn eine rote Zeile NICHT erklaert ist.
bekannt_rot_pruefen() {
    local out="$1" unerklaert=0
    local -a getroffen=()
    for _ in "${BEKANNT_ROT[@]}"; do getroffen+=(0); done
    echo "== bekannt-rote Zeilen =="
    while IFS= read -r zeile; do
        local praefix="${zeile%%:*}:" erklaert=0 i=0
        for e in "${BEKANNT_ROT[@]}"; do
            IFS='|' read -r p seit eintrag diag <<< "$e"
            if [ "${zeile:0:${#p}}" = "$p" ]; then
                echo "  bekannt: ${p}FAILURES -- rot seit $seit, $eintrag"
                echo "           $diag"
                getroffen[$i]=1
                erklaert=1; break
            fi
            i=$((i+1))
        done
        [ "$erklaert" = 1 ] || { echo "  NEU ROT: $zeile"; unerklaert=1; }
    done < <(echo "$out" | grep -E "^[a-z]+ *: .*FAILURES" | sort -u)
    # **Eintraege, die NICHTS erklaeren, sind Totholz -- und Totholz verrottet.** Ein
    # known-red-Eintrag fuer eine Zeile, die laengst gruen ist, liest sich wie Wissen und ist eine
    # Spur ins Leere; genau das war der `fp`-Eintrag am 2026-08-10, einen Tag nachdem die Zeile
    # gruen wurde. Kein FEHLSCHLAG, sondern eine Meldung: eine Suitenvariante darf eine Zeile
    # legitim gar nicht erzeugen, und ein Waechter, der in jedem gesunden Lauf schreit, wird
    # abgeschaltet.
    local i=0
    for e in "${BEKANNT_ROT[@]}"; do
        if [ "${getroffen[$i]}" = 0 ]; then
            IFS='|' read -r p seit eintrag diag <<< "$e"
            echo "  VERALTET? ${p} steht auf der Liste, war in diesem Lauf aber NICHT rot"
            echo "            (seit $seit, $eintrag) -- entweder behoben und der Eintrag gehoert weg,"
            echo "            oder diese Suitenvariante erzeugt die Zeile gar nicht."
        fi
        i=$((i+1))
    done
    if [ "$unerklaert" = 1 ]; then
        echo "  BEFUND: eine rote Zeile steht NICHT auf der Liste -- das ist eine neue Regression,"
        echo "          keine bekannte Luecke. Genau dieser Unterschied war bei der Lade-Suite unsichtbar."
        return 1
    fi
    echo "  (keine unerklaerte rote Zeile)"
    return 0
}

fail=0
check() { if grep -q "$1" <<<"$OUT"; then echo "  PASS: $2"; else echo "  FAIL: $2"; fail=1; fi; }

# ================================================================================================
# SPRECHPROBE DES PRUEFERS SELBST (2026-08-10)
# ================================================================================================
#
# Am 2026-08-10 meldete `check` **FAIL fuer Zeilen, die im Protokoll STANDEN**. Die Ursache lag
# nicht im Kernel, sondern hier: `echo "$OUT" | grep -q MUSTER`. `grep -q` steigt beim ersten
# Treffer aus, `echo` bekommt SIGPIPE, und `set -o pipefail` (Zeile 5) macht daraus den
# Rueckgabewert der ganzen Pipeline -- rc=141, also "nicht gefunden".
#
# **Das kippt erst oberhalb des Pipe-Puffers**: gemessen zwischen 66 und 70 KiB Ausgabe. Damit hing
# das Urteil der Suite an der **Groesse ihrer eigenen Ausgabe** -- solange das Protokoll klein
# blieb, war das Gruen Glueck. Ein sechster Archiveintrag hat es ueber die Kante geschoben, und
# neun Pruefungen meldeten FAIL fuer vorhandene Zeilen. Das ist die schlimmere Richtung von
# "erfundene Erfolge": erfundene MISSERFOLGE ertraenken den echten Befund.
#
# Behoben durch Here-Strings (`grep -q MUSTER <<<"$OUT"`) -- keine Pipeline, kein SIGPIPE, kein
# pipefail. Bewacht durch diese Sprechprobe, und zwar an einer bewusst **grossen** Eingabe:
# an einer kleinen waere sie waehrend des ganzen Fehlers gruen gewesen.
#
# Beide Richtungen, wie ueberall in diesem Projekt: vorhanden -> PASS, abwesend -> FAIL.
pruefer_selbsttest() {
    local gross da weg alt_out="$OUT"
    gross="MARKER-VORHANDEN
$(head -c 262144 /dev/zero | tr '\0' 'x')"
    OUT="$gross"
    da="$(check "MARKER-VORHANDEN" "selbsttest")"
    weg="$(check "MARKER-FEHLT-ABSICHTLICH" "selbsttest")"
    OUT="$alt_out"
    case "$da" in
        *PASS*) ;;
        *) echo "== PRUEFER DEFEKT: findet ein VORHANDENES Muster nicht (256 KiB Eingabe) --"
           echo "   das ist KEIN Testergebnis, sondern ein Aufbauproblem. Siehe SIGPIPE/pipefail oben. =="
           return 1 ;;
    esac
    case "$weg" in
        *FAIL*) ;;
        *) echo "== PRUEFER DEFEKT: meldet ein ABWESENDES Muster als vorhanden =="
           return 1 ;;
    esac
    echo "  Pruefer-Sprechprobe: beide Richtungen an 256 KiB Eingabe (vorhanden->PASS, abwesend->FAIL)"
}
pruefer_selbsttest || exit 2

check "mbi     : 1 Modul(e)" \
    "A-1.1: der Bootloader liefert die Startmenge als Multiboot-Modul (Flag Bit 3 ausgewertet)"
check "mbmod   : ALL PASS" \
    "A-1.1: Modulbereiche werden VOR der ersten Allokation aus der Freiliste ausgeschnitten (Rand/Ueberlappung/unsortiert/Vollabdeckung eingespeist)"
# Die ZAHL steht hier, nicht bloss "das Archiv parst": ein Archiv, aus dem beim Bauen still ein
# Modul herausfiel, parst genauso gut -- und der Treiber-Test darunter saehe dann aus wie ein
# Treiberfehler statt wie ein fehlendes Modul.
check "archive : 8 Modul(e)" \
    "A-1.1/A-1.5: das Archiv liegt an der vom Bootloader gemeldeten Adresse und parst (init + hello + virtio-blk + fs + virtio-net + wasmhost + ipcperm-srv + ipcperm-cli)"
# A1 / Z11c (2026-08-07). **Auf `ALL PASS` geprueft, nicht auf die Zeile** -- die Zeile gibt es
# auch als SKIP („kein Programm mit EXCLUSIVE_STRIPE geladen"), und genau der Fall ist beim Bau
# eingetreten: der zweite Ladepfad war nicht umgestellt, `hello` kam ungefaerbt an, und die Suite
# waere gruen geblieben, haette hier nur die Zeile gestanden.
check "pdcolor : ALL PASS" \
    "A1: eine ueber das Manifest als EXCLUSIVE_STRIPE geladene PD haelt Segmente, Stack und Seitentabellen in EINEM Farbstreifen -- gemessen an der Teardown-Buchhaltung, mit Gegenprobe an einer ungefaerbten PD"
check "ladepol : ALL PASS" \
    "Z11c: die Politik des Manifests wird ANGEWANDT, nicht nur gelesen (Prioritaet/Affinitaet aus dem TCB zurueckgelesen, nicht aus dem Ladepfad)"
# C4: die Stack-Wasserstandsmarke. **Diese Suite ist die interessantere von beiden** -- hier laufen
# geladene Programme, zwei Treiber-PDs, eine Dateisystem-PD und die verkettete IPC
# `fs -> Blockdienst -> Treiber`. Der Hoechststand aus der Hauptsuite deckt diese Pfade NICHT ab.
# Positiv geprueft und nicht bloss 'nicht rot': eine Zeile, die gar nicht kommt, faengt kein
# Rotzeilen-Scanner.
check "kstack  : ALL PASS" \
    "C4: die Stack-Wasserstandsmarke -- gemessen wird nicht, wie GROSS die Kernel-Stacks sind, sondern wieviel davon je BENUTZT wurde (Muster beim Anlegen, Abzaehlen beim Tod des Threads und am Ende des Laufs). Faellt die Fuellung aus, meldet die Messung die VOLLE Groesse als benutzt und die Zeile faellt durch"
# ------------------------------------------------------------------------------------------------
# C8: der VERIFIZIERERTHREAD -- und warum DIESE Suite die interessantere ist
# ------------------------------------------------------------------------------------------------
#
# **Die Umdefinition gehoert ins Protokoll, nicht nur in den Kopf.** Bis zum 2026-08-11 mass die
# `kstack`-Zeile dieser Suite den LADEPFAD: Ed25519 + SHA-2 liefen auf dem 16-KiB-Kstack des
# aufrufenden EL0-Threads, Hoechststand 11992 B (73,1 %), Reserve 4392 B. Seit C8 laeuft genau
# dieser Pfad auf dem 64-KiB-Stack des Verifizierers; die `kstack`-Zeile misst hier den RESTPFAD
# (gemessen 1312 B / 8,0 %). Wer die beiden Zahlen ueber diesen Bedeutungswechsel hinweg
# vergleicht, vergleicht zwei verschiedene Groessen -- daran ist in diesem Projekt schon eine Zahl
# wertlos geworden. Die Zeile sagt es deshalb selbst.
#
# Nur diese Suite hat ein Archiv, also laeuft nur hier ECHTE Krypto durch den Verifizierer -- die
# Hauptsuite provoziert ihn mit einem ungueltigen Index. Der Wasserstand hier ist die belastbare
# Zahl.
check "verif   : ALL PASS" \
    "C8: der Verifiziererthread traegt den tiefsten Kernelpfad, und der Aufrufer wartet mit dem EIGENEN Grund LOAD (Z24). Hier laeuft echte Ed25519-/SHA-2-Verifikation darueber -- die Hauptsuite kann das bauartbedingt nicht"
check "verif   : Absage gefahren -- 5 Sonden gegen eine Schranke von 4: abgewiesen=1 bedient=4" \
    "C8 (a): die Schranke ist GEFAHREN, mit benanntem Code (ERR_LOAD_BUSY), und der Ueberlaeufer bleibt lauffaehig statt blockiert liegenzubleiben -- D11 in beide Richtungen"
check "kstack  : Wasserstand VERIFIZIERER" \
    "C8 (b): die Stackgroesse des Verifizierers ist GEMESSEN. Er stirbt nie, wird vom Reap-Pfad also nie erfasst; ohne diese Zeile waeren 64 KiB eine Zahl mit derselben Berechtigung wie vorher die 16 KiB -- keiner"
# ------------------------------------------------------------------------------------------------
# C7: der MANGEL-SWEEP auf dem LADEPFAD -- und warum er ausgerechnet hier geprueft wird
# ------------------------------------------------------------------------------------------------
#
# Der Sweep provoziert jede Meldestelle einmal: `system::sperre_scharf(k)` laesst `k`
# Speicheranforderungen durch und weist ab der `k+1`-ten jede ab. Zehn der 32 Meldestellen lagen
# bis zum 2026-08-11 ungemessen im LADEPFAD, mit der Begruendung "braucht ein Boot-Archiv, das die
# Hauptsuite bauartbedingt nicht hat". **Diese Suite HAT das Archiv** -- und der Ladepfad ist
# derjenige, auf dem `NoResources` sechs Wochen lang stumm war und an dem `wasmhost` gestorben ist.
# Wer nach Erreichbarkeit priorisiert statt nach Vorgeschichte, hat die Diagnosewerkzeuge am Ende
# genau dort nicht, wo der letzte Fall lag.
#
# Drei Pruefungen, und sie sind verschieden:
#   1. das Urteil (gattert ohnehin ueber `all_done`, wird hier aber POSITIV gelesen -- eine Zeile,
#      die gar nicht kommt, faengt kein Rotzeilen-Scanner),
#   2. dass der Ladepfad in DIESEM Lauf wirklich gefahren ist (`LADEN=n` mit n > 0). Ohne diese
#      Zeile waere ein Sweep, der den Ladepfad ueberspringt, von einem, der ihn faehrt, nicht zu
#      unterscheiden -- und die Hauptsuite meldet zu Recht `LADEN=0`,
#   3. dass er nichts liegen laesst. Ein halb abgebauter Ladevorgang faelschte jede Zeile nach ihm.
check "sweep   : ALL PASS" \
    "C7: jede provozierbare Meldestelle hat einmal gesprochen -- geschwiegen=0 (an der GENERATION gemessen, nicht an der Marke), 'keiner' trotz Abweisung=0, und die gemeldete Menge kam aus dem AUFRUF und nicht aus einem Literal daneben"
check "LADEN=[1-9][0-9]* (Boot-Archiv da" \
    "C7: der ECHTE Ladepfad (system::load_into_pd_mit, dieselbe Funktion die SYS_LOAD ruft) wurde provoziert -- Segmentspeicher, Seitentabelle eines Segments, User-Stack und Seitentabelle des Stacks haben einzeln gesprochen. Die Hauptsuite meldet hier 0, und das ist dort die richtige Antwort: sie hat kein Boot-Archiv"
check "sweep   : Bilanz .* VSpaces=0 · PD-Slots=0 · Thread-Slots=0 · Seitentabellen-Rahmen=0" \
    "C7: nach dem Sweep bleibt nichts liegen -- vier exakt nachgezaehlte Groessen, nicht die Zusage einer Aufraeumroutine"
# Per-Kern-TSS + IST-Stacks. **Diese Suite ist auch hier die interessantere**: `SYS_LOAD`
# verifiziert Ed25519 + SHA-2 IM KERNEL auf dem Stack des Aufrufers und ist damit der tiefste
# Kernelpfad des Systems -- also genau der, dessen Ueberlauf ein #DF melden koennen muss. Positiv
# geprueft und nicht bloss 'nicht rot': eine Zeile, die gar nicht kommt, faengt kein Rotzeilen-Scanner.
check "ist     : ALL PASS" \
    "Per-Kern-TSS + IST-Stacks fuer #DF/NMI/#MC: jeder Kern hat seine eigene TSS, und die IST-Wirkung ist an jedem Kern AUSGELOEST und zurueckgelesen. Der echte #DF-Nachweis liegt in tools/df-sonde.sh (ein Abort laesst sich nicht in einer gruenen Suite fahren)"
check "#PF(14)=0" \
    "#PF bekommt AUSDRUECKLICH KEINEN IST -- er muss wiedereintrittsfaehig bleiben; der Stackueberlauf wird ueber #DF gefangen"
# **Die Antwort auf 'laufen Ring-3-Threads auf Sekundaerkernen?' gehoert in DIESE Suite.** Die
# Hauptsuite kennt nur die Demo-Threads; hier laufen geladene PDs (Root-Task, zwei Treiber-PDs,
# Dateisystem, WASM). Waere die Antwort hier 'ja', waere die frueher EINE gemeinsame TSS bereits
# ein Riss gewesen und nicht bloss eine kuenftige Gefahr.
check "ist     : Ring-3-Rueckkehr je Kern" \
    "gezaehlt wird die GELEGENHEIT: je Kern steht im Protokoll, wie oft er eine Rueckkehr nach Ring 3 vorbereitet hat -- in JEDEM Lauf ablesbar, waehrend ein Melder fuer den Zusammenstoss in einem gesunden Lauf stumm waere"
# Z15/W1. **Auf `ALL PASS` geprueft, nicht auf die Zeile** -- es gibt sie auch als SKIP.
check "wasm    : ALL PASS" \
    "Z15/W1: eine WASM-Laufzeit als gewoehnliche PD -- Modul instanziiert, GERECHNETES Ergebnis, mutiertes Modul abgewiesen, Uebergriff auf den Linearspeicher als WASM-Trap ohne dass die PD faultet"
check "manifest: ALL PASS" \
    "A-1.2..A-1.4: System-Manifest -- signiert ueber die GESAMTE Nachricht, an DIESES Kernel-Image gebunden, Anti-Downgrade, manipulierte Kopie wird abgewiesen"
check "manifest:   \[1\]init .* ROOT" \
    "A-1.4: Politikfelder (Domaene, Schnittstellenversion, Anfangs-Caps, Prioritaet, NUMA, Affinitaet, Budget) werden gelesen und ausgewiesen"
# ACHTUNG: es gibt MEHRERE `root    :`-Zeilen (eine ueber das Laden, eine ueber das Ergebnis).
# Gegen "root    : ALL PASS" zu pruefen traf die erste -- der Lauf war gruen, waehrend die
# eigentliche Aussage darunter FAILURES meldete. Genau dieser Fehler steht in AGENTS.md.
# Deshalb hier auf den EINDEUTIGEN Text der Ergebniszeile pruefen.
check "root    : ALL PASS (A-2.1" \
    "A-2.1: Root-Task laeuft wirklich (Badge) -- und laedt ueber SEINE Loader-Cap ein weiteres Programm nach"
check "root    : ALL PASS (Startprogramm" \
    "A-2.1: Root-Task aus dem Manifest geladen (Hash geprueft, Wurzel-Caps endowt)"
check "cdelete : ALL PASS" \
    "A-3.1: SYS_CDELETE aus Ring 3 -- beide Ausgaenge belegt (geloescht + Autoritaet weg; mit Ableitungen abgewiesen und weiter benutzbar)"
# Tagged IPC capabilities, end to end: the server received the permission mask and id the kernel
# wrote into the badge word, an untagged capability is unchanged, derivation only shrinks, DUP and
# the 48-bit id are enforced, and only endpoints can be tagged. The line is gated here and not in
# `all_done()` (same reasoning as `wasm`: a gate on late badges would turn a slow run into a
# watchdog).
check "ipcperm : ALL PASS" \
    "tagged IPC capabilities: the SERVER saw the permission mask and id the kernel delivered (not just what the client asked for)"
# A-5.1: der erste Treiber, der nicht im Kern laeuft.
check "drv     : ALL PASS" \
    "A-5.1: ein Treiber als DIENST ausserhalb des Kerns -- eigene Konfigurationsraum-Seite aufgeloest, virtio-Handshake, Sektoren per Bus-Master-DMA auf Anfrage. Der Kernel hat enumeriert, zugeteilt und den Empfaenger ausgetauscht, ohne einen virtio-Schritt auszufuehren"
check "dmaiso  : ALL PASS" \
    "A-5.4: das Geraet der EINEN Treiber-PD erreicht die DMA-Region der ANDEREN nicht. Die Aussage haengt an VIER Zahlen, und keine reicht allein: die Positivkontrolle laeuft ueber denselben Treiber, dasselbe Geraet und dieselbe Deskriptorkette (nur EINE Adresse wandert); der Fremdversuch liefert keine Daten; das Opfer ist unberuehrt, und zwar vom KERNEL nachgeprueft statt vom Angreifer gemeldet; und ein VT-d-Fault belegt AKTIV, dass geblockt wurde -- ohne ihn waere 'keine Antwort' auch mit einem stummen Gegenueber vereinbar"
# E-Rest 3: das Geraetefenster einer Treiber-PD oberhalb 4 GiB liegt in einer PRIVATEN Kopie.
# Ab `-m 3G` legt SeaBIOS die virtio-BARs bei 448 GiB ab; dort steht die Karte in einer GETEILTEN
# statischen Tabelle, und wer ein PD-Fenster dorthin schriebe, gaebe es JEDER isolierten PD --
# lautlos, denn die Cap-Pruefung liefe korrekt durch. SKIP heisst hier "die BARs lagen unter
# 4 GiB" (der Fall bei der Vorgabe 512M) und ist ausdruecklich KEIN Bestehen.
if grep -q "^hiiso   : FAILURES" <<<"$OUT"; then
    echo "  FAIL: E-Rest 3: $(grep -m1 '^hiiso   :' <<<"$OUT")"; fail=1
elif grep -q "^hiiso   : ALL PASS" <<<"$OUT"; then
    echo "  PASS: E-Rest 3: die Treiber-PD bekam ihr Fenster oberhalb 4 GiB in einer PRIVATEN Kopie des Seitenverzeichnisses -- die geteilte Tabelle traegt keinen PD-spezifischen Eintrag"
elif grep -q "^hiiso   : SKIP" <<<"$OUT"; then
    echo "  SKIP (keine BARs oberhalb 4 GiB -- mit '$RAM' legt die Firmware sie darunter; mit '6G' wird die Aussage scharf): private Geraete-Tabelle oberhalb 4 GiB"
else
    echo "  FAIL: E-Rest 3: keine hiiso-Zeile im Protokoll"; fail=1
fi
check "devsel  : ALL PASS" \
    "A-5.3: das MANIFEST sagt, welches Geraet die Treiber-PD bekommt -- nicht die Fundreihenfolge des Enumerators. Der Lauf bietet ZWEI Geraete an (virtio-blk und virtio-net); ohne die zweite waere die Zeile eine Aussage ueber nichts, denn bei einem einzigen trifft jeder Selektor dieselbe Wahl. Geprueft wird deshalb auch, dass die nicht gewaehlte Alternative LIEGEN BLIEB"
# Die Einzelaussagen werden HIER noch einmal gelesen, nicht nur das Sammelurteil. Sie sind
# verschieden, und ein Sammel-PASS verdeckte, welche davon traegt:
#
#   1. der Dienst BEDIENT (Anfrage 1 an v1 mit Status 0),
#   2. der Empfaenger wurde OHNE LUECKE ausgetauscht (A-4.1 am echten Dienst, nicht am lokalen
#      Objekt -- dort war der ueberlappende Fall bisher nur konstruiert erreichbar),
#   3. die neue Fassung erbt die REGION (Bedienungszaehler 1 -> 2, nicht 1 -> 1).
if grep -q "drv     : Anfrage 1 an v1: Status=0 " <<<"$OUT"; then
    echo "  PASS: A-5.1: der Treiber-DIENST beantwortet eine Anfrage -- der Kernel ist Client, nicht Treiber (Richtungsumkehr)"
else
    echo "  FAIL: A-5.1: die erste Anfrage an den Dienst kam nicht durch:"
    grep -m1 "drv     : Anfrage 1" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
if grep -q "drv     : Austausch: Ergebnis=0 .*v1 meldete bereit=1 v2 meldete bereit=1" <<<"$OUT"; then
    echo "  PASS: A-5.1/A-4.1: der Dienst wurde am LAUFENDEN Endpoint ausgetauscht, und der Endpoint hatte zu keinem Zeitpunkt null Empfaenger; beide Fassungen haben sich gemeldet"
else
    echo "  FAIL: A-5.1/A-4.1: der Austausch lief nicht sauber:"
    grep -m1 "drv     : Austausch" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
if grep -q "drv     : Anfrage 2 an v2: Status=0 " <<<"$OUT"; then
    echo "  PASS: A-5.1: die NEUE Fassung bedient weiter und hat die DMA-Region geerbt (Zaehler 1 -> 2) -- ein Austausch, kein Neustart"
else
    echo "  FAIL: A-5.1: die neue Fassung bediente nicht oder bekam eine frische Region:"
    grep -m1 "drv     : Anfrage 2" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
# A-6.1: das Dienstprotokoll ueber dem Treiber.
check "blkdev  : ALL PASS" \
    "A-6.1: der Blockdienst traegt -- Auskunft (Kapazitaet/Sektorgroesse), Lesen, SCHREIBEN, Flush, und ein Sektor jenseits der Platte wird ABGEWIESEN statt ans Geraet durchgereicht"
# Der Rueckleseschritt einzeln, weil er die eigentliche Aussage traegt: eine quittierte
# Schreibanfrage ist eine Quittung, keine Daten. Faellt nur er aus, ist das ein Befund am
# Schreibpfad -- und kein Sammel-FAIL, dem man nicht ansieht, welcher Schritt riss.
if grep -q "blkdev  : .*Rueckgelesen=0x534b434f52504143 (erwartet 0x534b434f52504143)" <<<"$OUT"; then
    echo "  PASS: A-6.1: geschrieben und ZURUECKGELESEN -- die Daten stehen wirklich auf der Platte, nicht bloss in einer Quittung"
else
    echo "  FAIL: A-6.1: das Zurueckgelesene passt nicht zum Geschriebenen:"
    grep -m1 "blkdev  : INFO" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
# A-6.2: die Partitionstabelle -- gelesen im Blockdienst, nicht im Kern.
check "part    : ALL PASS" \
    "A-6.2: GPT im BLOCKDIENST gelesen (caprock-part: abhaengigkeitsfrei, forbid(unsafe_code), host-getestet). Beide Pruefsummen geprueft; die Eintragsliste passt nicht in eine Anfrage und wird stueckweise gelesen, die Pruefsumme aber ueber das GANZE gebildet"
if grep -q "part    : .*erste Partition LBA $BLK_PART1_LBA ueber $BLK_PART1_SECTORS Sektoren" <<<"$OUT"; then
    echo "  PASS: A-6.2: die gemeldete erste Partition passt zu der, die diese Suite ins Abbild geschrieben hat (LBA $BLK_PART1_LBA, $BLK_PART1_SECTORS Sektoren)"
else
    echo "  FAIL: A-6.2: die gemeldete Partition passt nicht zum Abbild:"
    grep -m1 "part    : GPT-Scan" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
# A-6.3: das Dateisystem -- eine EIGENE PD, die kein Geraet faehrt.
check "fs      : ALL PASS" \
    "A-6.3: ein lesendes Dateisystem als eigene PD -- sie ruft den Blockdienst ueber dessen Kanal und liest die Bytes aus der geteilten Uebertragungsflaeche. GPT (caprock-part) und FAT16 (caprock-fat) sind kernfrei und forbid(unsafe_code); der Kern kennt weder Partitionen noch Dateien"
# Der Inhalt einzeln, weil er die eigentliche Aussage traegt: eine gefundene Datei ist noch keine
# gelesene. Groesse UND erste Bytes muessen zu dem passen, was `tools/mkgpt.py --file` hineinlegt.
if grep -q "fs      : Status=0 .*Groesse=20 erste acht Byte=0x534b434f52504143" <<<"$OUT"; then
    echo "  PASS: A-6.3: die Datei wurde nicht bloss GEFUNDEN, sondern GELESEN -- Groesse und Inhalt passen zu dem, was diese Suite ins Dateisystem geschrieben hat"
else
    echo "  FAIL: A-6.3: Groesse oder Inhalt der gelesenen Datei passen nicht:"
    grep -m1 "fs      : Status" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi
# A-6.4: **die zweite, unabhaengige Quelle.** Der Kernel meldet, dass die PD geschrieben und
# zurueckgelesen hat -- eine Aussage des Codes ueber sich selbst. Hier liest ein Werkzeug in einer
# ANDEREN Sprache dasselbe Abbild und sagt, ob wirklich auf der Platte steht, was behauptet wird.
# Es prueft zusaetzlich, was die PD gar nicht sehen kann: dass BEIDE FAT-Kopien dieselbe Kette
# tragen. Nur die erste fortzuschreiben ist der haeufigste Schreibfehler ueberhaupt.
if python3 tools/checkfat.py "$BLK_IMG" --part-lba "$BLK_PART1_LBA" --file HELLO.TXT \
        --expect-size 700 > "$LOG.fat" 2>&1; then
    echo "  PASS: A-6.4: $(cat "$LOG.fat") -- unabhaengig vom Kernel am Abbild nachgelesen"
else
    echo "  FAIL: A-6.4: das Abbild traegt nicht, was der Kernel meldet:"
    sed 's/^/          /' "$LOG.fat"
    fail=1
fi
rm -f "$LOG.fat"
# --- Zwei Zeilen, die bis 2026-08-26 NUR in der Hauptsuite gatterten -----------------------------
#
# Beide Sonden sind arch-neutral und laufen in JEDEM Lauf; gegattert wurden sie nur dort. Das ist
# woertlich die Rissform von `iommu_platform=on`: zwei Suiten, die dieselbe Sache verschieden
# behandeln, und der Fall faellt genau dort an, wo niemand hinsieht. Hier ist der Unterschied
# ausserdem nicht kosmetisch -- diese Suite hat ein Boot-Archiv, also mehr PDs, mehr Threads und
# einen anderen Speicherdruck, und `arena` weist bei knappem Speicher mit SKIP ab.
check "ckptcut : ALL PASS" \
    "Z4d Stufe 1: eine offene Transaktion darf den Schnitt nicht kreuzen -- hier unter Last MIT Boot-Archiv"
check "arena  : ALL PASS" \
    "K1b: vier Thread-Stapel aus EINER Memory-Cap, unter Last MIT Boot-Archiv. Der Lauf hat hier mehr PDs und mehr Threads als die Hauptsuite; SYS_SPAWN wird also nicht nur einmal auf der ruhigen Maschine gefahren"
check "dmapool : ALL PASS" \
    "C2: die Groesse des DMA-Pools ist ein ARGUMENT des Ladens (SYS_LOAD x5), keine Kernelkonstante. Vier Aussagen, und keine davon ist 'ein Pool ist da': eingeloest -- jede Zuteilung bekam GENAU das Angeforderte (das trennt 'so gewollt' von 'stillschweigend gekuerzt', und eine halbierte DMA-Region ist ein Geraet, das ueber ihr Ende hinausschreibt); verschieden -- zwei Treiber-PDs mit VERSCHIEDEN grossen Pools, ohne das waere 'eingeloest' auch dann wahr, wenn alle die Vorgabe bekommen und der Parameter nie gelesen wird; disjunkt -- die IOVA-Fenster ueberschneiden sich nicht, A-5.4 also bei GEAENDERTEN Groessen neu gefahren statt als weitergeltend angenommen; zu-gross-abgewiesen -- der Ring-3-Negativfall aus init, ohne den die Obergrenze eine Zahl waere, von der niemand weiss, ob sie beisst. dma_audit steht als ZAHL daneben und ist bewusst KEIN Konjunkt: nach dem A-5.1-Hot-Reload halten zwei Caps dieselbe Region, s. todo"
check "irqmsi  : ALL PASS" \
    "Stufe B (B1+B2+B3): die Interrupt-Autoritaet einer Treiber-PD. Der IRTE steht und wird AUS DER TABELLE zurueckgelesen -- ein Pruefer, der die Kodierung nachrechnet, prueft seine eigene Kopie. svt+sid zusammen sind die Sicherheitsaussage: ohne SVT=01 naehme der Eintrag eine MSI von JEDEM Geraet an (die Interruptachse desselben Kanals, den A-5.4 auf der DMA-Achse geschlossen hat), und ohne den SID-Vergleich waere svt mit jeder beliebigen BDF wahr. angeboten-dann-da trennt 'das Geraet hat kein MSI-X' (Pollen ist zulaessig) von 'die Vergabe ist GESCHEITERT' -- ohne diese Bedingung sind beide dieselbe gruene Zeile. cap-in-slot7 belegt, dass die Cap einen HALTER hat und nicht nur existiert (SYS_SPAWN stand neun Tage ohne Aufrufer im Baum), cap-nennt-vektor, dass es SEINER ist. bind-ohne-cap-abgewiesen ist der Ring-3-Negativfall aus init: er prueft auf ERR_BADCAP und nicht auf 'ungleich OK', denn ein nicht gebauter Syscall gaebe ERR_BADSYS und der Negativfall waere gruen, WEIL nichts gebaut ist"

check "SELFTEST COMPLETE" "sauberes system_off statt Timeout"

# --- Z4 Stufe 2: ein Thread ueber die BOOTGRENZE -------------------------------------------------
#
# **Die Maschinengrenze ist hier die Zeit.** Diese Suite bootet mehrfach mit DEMSELBEN `$BLK_IMG`;
# was ein Lauf auf die Platte schreibt, findet der naechste dort vor. Alles im RAM ist dazwischen
# weg -- genau das macht die Aussage zu einer ueber einen Checkpoint und nicht ueber eine Variable.
#
# Die eine Aussage: **der Zaehler im naechsten Lauf startet bei dem Wert aus dem vorigen, nicht
# bei 0.** Alles andere hier ist das, was noetig ist, damit diese Aussage etwas belegt.
#
# ## Warum DREI Laeufe und nicht zwei
#
# Gemessen (2026-08-02, KVM): derselbe Kernel erreicht an derselben Stelle des Hochlaufs 133, 148
# und 151 Worker-Runden -- eine Streuung von rund 18. In einem Mutationslauf, in dem der Kernel den
# gefundenen Wert LAS und MELDETE, ihn aber nicht in den Thread schrieb, traf sein eigener Zaehler
# den gespeicherten Wert **exakt** (151 gegen 151) -- und die Suite blieb gruen. Der Zaehler ist
# also gerade NICHT „bei jedem Lauf woanders"; unter KVM ist der Hochlauf reproduzierbar.
#
# Deshalb waechst der Zustand ueber die Kette: jeder Lauf laesst den wiederhergestellten Thread
# noch mindestens 100 Runden arbeiten (`CKPT_MIN_DELTA`) und speichert erst dann. Ab dem ZWEITEN
# Glied liegt der gespeicherte Wert damit strukturell ausserhalb dessen, was ein Lauf ohne
# Wiederherstellung erreicht -- und erst im DRITTEN Lauf ist „geerbt oder selbst gezaehlt?"
# entscheidbar. Lauf 2 allein waere genau die Zeile, die im Mutationslauf gruen blieb.
CKPT_SECTOR=32710   # muss zu `CKPT_SECTOR` in kernel/src/arch/x86_64/bringup.rs passen

# Ein Feld aus einer `bootckpt:`-Zeile ziehen: $1 = Ausgabe, $2 = Zeilenart, $3 = Feldname.
ck() { echo "$1" | sed -n "s/^bootckpt: $2 .*[ (]$3=\([0-9a-fx]\{1,\}\).*/\1/p" | head -1; }

# 1. Die Verweigerungsregel (Z4b) laeuft in JEDEM Lauf mit -- sie haengt nicht daran, ob gerade
#    gespeichert oder wiederhergestellt wird. Geprueft wird der GRUND, nicht bloss, dass es einen
#    gab: Grund 5 (Partner nicht im Umfang) waere durch einen groesseren Umfang behebbar und
#    belegte die Regel deshalb nicht. Der Kanal der Treiber-PD liegt darum ausdruecklich IM
#    Umfang; was uebrigbleibt, ist Geraete-Autoritaet (1=MMIO-Fenster, 2=IRQ, 3=DMA-Region).
if grep -qE "^bootckpt: Verweigerung: .* Grund (1|2|3) " <<<"$OUT"; then
    echo "  PASS: Z4b: eine nicht uebertragbare Cap im Umfang verhindert das SPEICHERN -- die Treiber-PD haelt Geraete-Autoritaet, und die kann auf der Zielmaschine nichts bezeichnen. Der Grund ist EINZELN (Geraetefenster/IRQ/DMA), nicht ein Sammel-Nein, und er ist durch keinen groesseren Umfang behebbar"
else
    echo "  FAIL: Z4b: die Treiber-PD wurde nicht mit einem GERAETE-Grund abgewiesen:"
    grep -m1 "^bootckpt: Verweigerung" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi

# 2. Lauf 1 ist der KALTSTART: kein Checkpoint auf der Platte -> speichern, Epoche 1. Bis zum
#    Flush, denn ohne Flush ist „geschrieben" eine Aussage ueber einen Puffer -- und ueber eine
#    Bootgrenze ist genau das der Unterschied.
check "^bootckpt: ALL PASS" \
    "Z4 Stufe 2: Lauf 1 (Kaltstart) hat einen Thread eingefroren und seinen Zustand geschrieben"
P1="$(ck "$OUT" gespeichert Fortschritt)"
N1="$(ck "$OUT" gespeichert Nonce)"
E1="$(ck "$OUT" gespeichert Epoche)"
if [ -n "$P1" ] && [ "$P1" != 0 ] && [ -n "$N1" ] && [ "$N1" != "0x0000000000000000" ] && [ "$E1" = 1 ]; then
    echo "  PASS: Z4a am echten Gegenstand: Lauf 1 speicherte Fortschritt=$P1 Nonce=$N1 als Epoche 1 (am EINGEFRORENEN Thread gelesen -- ein Wert, der waehrend des Lesens weiterlaeuft, gehoert zu keinem Zeitpunkt)"
else
    echo "  FAIL: Z4 Stufe 2: Lauf 1 hat nichts Brauchbares gespeichert (Fortschritt=$P1 Nonce=$N1 Epoche=$E1):"
    grep -m1 "^bootckpt: gespeichert" <<<"$OUT" | sed 's/^/          /'
    fail=1
fi

echo "== Z4 Stufe 2: zweiter Boot auf DERSELBEN Platte =="
LOG2="$(mktemp)"
boot build/boot-archive-x86.bin "$LOG2"
OUT2="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$LOG2" 2>/dev/null)"
echo "$OUT2" | grep -E "^bootckpt" || true
[ -z "$OUT2" ] && { echo "  FAIL: der zweite Boot lieferte keine Ausgabe -- Aufbauproblem, kein Testergebnis"; fail=1; }
R2P="$(ck "$OUT2" wiederhergestellt Fortschritt)"
R2N="$(ck "$OUT2" wiederhergestellt Nonce)"
R2E="$(ck "$OUT2" wiederhergestellt Epoche)"
R2NACH="$(ck "$OUT2" wiederhergestellt Zaehler-danach)"
S2P="$(ck "$OUT2" gespeichert Fortschritt)"
S2N="$(ck "$OUT2" gespeichert Nonce)"
S2E="$(ck "$OUT2" gespeichert Epoche)"
# **`Zaehler-danach` gehoert ausdruecklich dazu.** Ohne dieses Feld belegte der Vergleich nur, dass
# der Checkpoint richtig GELESEN wurde -- ein Kernel, der ihn liest, ausgibt und den Thread dann
# bei seinem eigenen Wert weiterlaufen laesst, kaeme damit durch. Genau das ist gemessen worden.
if [ "$R2P" = "$P1" ] && [ "$R2N" = "$N1" ] && [ "$R2E" = 1 ] && [ "$R2NACH" = "$P1" ]; then
    echo "  PASS: Z4 Stufe 2: Lauf 2 fand den Checkpoint aus Lauf 1 (Fortschritt=$P1, Nonce=$N1, Epoche 1) und schrieb ihn in den Thread (Zaehler-danach=$R2NACH). Die Nonce traegt die Beweislast fuer die HERKUNFT: sie ist der Zyklenzaehler von Lauf 1 und kann kein Rest im Puffer sein"
else
    echo "  FAIL: Z4 Stufe 2: Lauf 2 fand den Zustand aus Lauf 1 nicht (erwartet Fortschritt=$P1 Nonce=$N1 Epoche=1 Zaehler-danach=$P1; bekam $R2P / $R2N / $R2E / $R2NACH)"
    fail=1
fi
# Die Kette WAECHST -- das ist der Unterschied zwischen „wiederhergestellt" und „neu angelegt".
if [ "$S2E" = 2 ] && [ -n "$S2P" ] && [ -n "$P1" ] && [ "$S2P" -gt "$P1" ] 2>/dev/null; then
    echo "  PASS: Z4 Stufe 2: Lauf 2 schrieb selbst wieder einen Checkpoint -- Epoche 2, Fortschritt $S2P > $P1. Der Zustand WAECHST ueber die Bootgrenze hinweg, statt in jedem Lauf neu zu entstehen"
else
    echo "  FAIL: Z4 Stufe 2: die Kette waechst nicht (Epoche=$S2E Fortschritt=$S2P gegen $P1)"; fail=1
fi
if grep -q "^bootckpt: ALL PASS" <<<"$OUT2"; then
    echo "  PASS: Z4 Stufe 2: der wiederhergestellte Thread lief danach weiter und kam um mindestens 100 Runden voran -- ein Wiederherstellen, das den Thread kaputtmacht, waere sonst von einem korrekten nicht zu unterscheiden"
else
    echo "  FAIL: Z4 Stufe 2: der zweite Lauf meldet keinen sauberen Checkpoint-Ausgang:"
    echo "$OUT2" | grep -m3 "^bootckpt" | sed 's/^/          /'; fail=1
fi
grep -q "SELFTEST COMPLETE" "$LOG2" \
    && echo "  PASS: der zweite Boot laeuft sauber durch (system_off, kein Watchdog)" \
    || { echo "  FAIL: der zweite Boot wurde nicht fertig"; fail=1; }

echo "== Z4 Stufe 2: dritter Boot -- hier wird die Aussage ENTSCHEIDBAR =="
LOG4="$(mktemp)"
boot build/boot-archive-x86.bin "$LOG4"
OUT4="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$LOG4" 2>/dev/null)"
echo "$OUT4" | grep -E "^bootckpt" || true
R3P="$(ck "$OUT4" wiederhergestellt Fortschritt)"
R3N="$(ck "$OUT4" wiederhergestellt Nonce)"
R3E="$(ck "$OUT4" wiederhergestellt Epoche)"
R3VOR="$(ck "$OUT4" wiederhergestellt Zaehler-vorher)"
R3NACH="$(ck "$OUT4" wiederhergestellt Zaehler-danach)"
S3E="$(ck "$OUT4" gespeichert Epoche)"
# **Die Positivkontrolle:** war ueberhaupt etwas zu messen? `Zaehler-vorher` ist der Stand, den
# DIESER Lauf ohne jede Wiederherstellung erreicht haette. Ist er gleich dem gefundenen Wert, ist
# „gesetzt" von „nicht gesetzt" nicht zu unterscheiden -- dann hat der Test nichts gemessen, und
# das ist kein bestandener Test. Erst der Zuwachs aus Lauf 2 trennt die beiden Zahlen.
if [ -n "$R3VOR" ] && [ -n "$R3P" ] && [ "$R3VOR" != "$R3P" ]; then
    echo "  PASS: Z4 Stufe 2 (Positivkontrolle): der eigene Stand dieses Laufs waere $R3VOR gewesen, der geerbte ist $R3P -- die beiden Zahlen sind TRENNBAR, also ist die naechste Aussage ueberhaupt eine Messung"
else
    echo "  FAIL: Z4 Stufe 2: NICHT MESSBAR -- eigener Stand ($R3VOR) und geerbter Wert ($R3P) sind gleich. Das ist kein bestandener Test, sondern ein Aufbau, in dem sich 'wiederhergestellt' und 'selbst gezaehlt' nicht unterscheiden lassen"
    fail=1
fi
if [ "$R3P" = "$S2P" ] && [ "$R3N" = "$S2N" ] && [ "$R3E" = 2 ] && [ "$R3NACH" = "$S2P" ] && [ "$S3E" = 3 ]; then
    echo "  PASS: Z4 Stufe 2 -- DIE AUSSAGE: der Thread in Lauf 3 startet bei $S2P, dem Wert aus Lauf 2, und nicht bei seinen eigenen $R3VOR. Drei Boots, drei Glieder (Epoche 1 -> 2 -> 3), jedes Glied traegt die Nonce seines Erzeugers"
else
    echo "  FAIL: Z4 Stufe 2: Lauf 3 uebernahm den Zustand aus Lauf 2 nicht (erwartet Fortschritt=$S2P Nonce=$S2N Epoche=2 Zaehler-danach=$S2P Folge-Epoche=3; bekam $R3P / $R3N / $R3E / $R3NACH / $S3E)"
    fail=1
fi
grep -q "^bootckpt: ALL PASS" <<<"$OUT4" \
    && echo "  PASS: Z4 Stufe 2: auch das dritte Glied ist sauber (eingefroren, gesetzt, weitergelaufen, neu geschrieben)" \
    || { echo "  FAIL: Z4 Stufe 2: der dritte Lauf meldet keinen sauberen Ausgang"; fail=1; }

# --- Negativfall Z4f: ein Checkpoint eines FREMDEN Kernel-Images ---------------------------------
#
# **Der wichtigste Fall des ganzen Strangs.** Ein Checkpoint gehoert an genau das Kernel-Image,
# unter dem er entstand. Wandert er in eine Umgebung mit anderen Zusicherungen, merkt es sonst
# niemand -- und das ist die teuerste Form von „es lief ja".
#
# Hergestellt wird er so, wie ein echter aussaehe: der Sektor bleibt STRUKTURELL HEIL, nur das
# Hashfeld traegt einen anderen Wert, und die Pruefsumme wird neu gerechnet. Ein einfaches
# Bytekippen waere der schwaechere Fall -- der faellt schon an der Pruefsumme durch, und dann
# haette der Test „kaputte Bytes" gemessen statt „falsches Image".
#
# Die Pruefsumme rechnet hier `zlib.crc32` und im Kernel `checkpoint::crc32`. Zwei
# Implementierungen in zwei Sprachen, dieselbe Zahl -- dieselbe Form wie `tools/checkfat.py`:
# ein Schreiber, der sein eigenes Ergebnis bestaetigt, bestaetigt nichts.
#
# ================================================================================================
# D14: WARUM HIER EIN SHA-256 STEHT UND NICHT `d[16] != 0`  (gemessen 2026-08-11)
# ================================================================================================
#
# Bis heute lautete die Nachpruefung „der Sektor ist unveraendert":
#
#     d[0:8] == b'SL4KCKPT' and d[16] != 0x00
#
# `d[16]` ist nach `crates/caprock-cap/src/checkpoint.rs` (OFF_BODY = 16) das **erste Byte des
# KERNEL-HASHES**, und der Negativfall darueber kippt genau dieses Byte mit `^= 0xFF`. Damit hing
# das Urteil an einer Groesse, die mit dem Schreiben des Sektors nichts zu tun hat: **am Hash des
# gerade gebauten Kernels.**
#
# Zwei Richtungen, beide durchgerechnet (`d[16]` gegen alle 256 moeglichen Hash-Bytes):
#   * **Falsch-Alarm:** ist `kernel_code_hash[0] == 0xFF`, ergibt das Kippen `0x00` -- die Zeile
#     meldet „ueberschrieben", obwohl NICHTS geschrieben wurde. 1 von 256 Bauten.
#   * **Blinder Fleck:** ueberschreibt der Kernel wirklich (er schreibt dann SEINEN Hash hin),
#     meldet die Zeile in **255 von 256** Faellen „unveraendert". Sie hat den Fall, fuer den sie
#     da ist, praktisch nie gesehen.
#
# **Das ist der Mechanismus hinter D14.** Der Eintrag lautete „eine zusaetzliche `println!`-Zeile
# im Abschlussbericht kippt den Z4f-Negativfall -- nicht ihr Inhalt, ihre blosse Existenz", und
# vermutete Wanduhrzeit. Die Groesse, die eine Berichtszeile DETERMINISTISCH aendert, ist aber der
# **Kernel-Binaerinhalt** und damit sein Hash. Das erklaert jede Beobachtung des Eintrags: je Bau
# reproduzierbar (2 von 2 Laeufen), unabhaengig vom Inhalt der Zeile, und beim Zuruecknehmen weg.
#
# Gemessen wird jetzt, was die Zeile behauptet: die **512 Byte selbst**, vor und nach dem Boot.
# Ein Praedikat ueber EIN Byte kann „unveraendert" nicht ausdruecken.
echo "== Negativfall Z4f: Checkpoint eines FREMDEN Kernel-Images =="
CKPT_VORHER="$(python3 - "$BLK_IMG" "$CKPT_SECTOR" <<'EOF'
import hashlib, struct, sys, zlib
img, lba = sys.argv[1], int(sys.argv[2])
with open(img, "r+b") as f:
    f.seek(lba * 512)
    s = bytearray(f.read(512))
    if bytes(s[0:8]) != b"SL4KCKPT":
        sys.exit("kein Checkpoint auf dem Sektor -- der Negativfall haette nichts zu veraendern")
    body_len = struct.unpack_from("<I", s, 12)[0]
    s[16] ^= 0xFF                      # ein Byte im KERNEL-HASH, sonst alles unveraendert
    struct.pack_into("<I", s, 16 + body_len, zlib.crc32(bytes(s[:16 + body_len])) & 0xFFFFFFFF)
    f.seek(lba * 512)
    f.write(bytes(s))
    print(hashlib.sha256(bytes(s)).hexdigest())
EOF
)"
if [ -n "$CKPT_VORHER" ]; then
    LOG3="$(mktemp)"
    boot build/boot-archive-x86.bin "$LOG3"
    OUT3="$(grep -vE "SeaBIOS|iPXE|Press Ctrl|Booting from|C900|PMM|PnP" "$LOG3" 2>/dev/null)"
    echo "$OUT3" | grep -E "^bootckpt" || true
    if grep -q "^bootckpt: ABGEWIESEN .*Lesecode=7" <<<"$OUT3" \
        && ! grep -q "^bootckpt: wiederhergestellt" <<<"$OUT3"; then
        echo "  PASS: Z4f in klein -- ein STRUKTURELL HEILER Checkpoint eines anderen Kernel-Images wird ABGEWIESEN (Lesecode 7), nicht geladen. Die Pruefsumme stimmt, die Laengen stimmen, die Kennung stimmt: was allein nicht stimmt, ist die Bindung ans Image"
    else
        echo "  FAIL: Z4f -- ein fremd gebundener Checkpoint wurde nicht mit Lesecode 7 abgewiesen:"
        echo "$OUT3" | grep -m3 "^bootckpt" | sed 's/^/          /'
        fail=1
    fi
    # Und er darf den fremden Checkpoint weder ueberschrieben noch geladen haben: aus einer
    # Abweisung wuerde sonst beim naechsten Lauf stillschweigend ein eigener Zustand.
    if grep -q "^bootckpt: gespeichert" <<<"$OUT3"; then
        echo "  FAIL: Z4f -- nach der Abweisung wurde trotzdem gespeichert; damit waere der fremde Zustand lautlos durch einen eigenen ersetzt"
        fail=1
    else
        # Der VOLLE Sektor, nicht ein Byte daraus -- s. den D14-Block oben.
        CKPT_NACHHER="$(python3 -c "
import hashlib, sys
d = open(sys.argv[1],'rb').read()[int(sys.argv[2])*512:][:512]
print(hashlib.sha256(d).hexdigest())" "$BLK_IMG" "$CKPT_SECTOR")"
        if [ "$CKPT_NACHHER" = "$CKPT_VORHER" ]; then
            echo "  PASS: der abgewiesene Checkpoint blieb auf der Platte UNVERAENDERT -- alle 512 Byte byte-identisch (SHA-256 ${CKPT_VORHER:0:12}). Ein Kernel, der ihn ueberschriebe, machte aus einem fremden Zustand lautlos einen eigenen; die alte Fassung dieser Zeile las EIN Byte und haette 255 von 256 Ueberschreibungen durchgewinkt (D14)"
        else
            echo "  FAIL: der abgewiesene Checkpoint wurde ueberschrieben (SHA-256 vorher ${CKPT_VORHER:0:12}, nachher ${CKPT_NACHHER:0:12})"; fail=1
        fi
    fi
    rm -f "$LOG3"
else
    echo "  FAIL: der Negativfall liess sich nicht herstellen (kein Checkpoint auf Sektor $CKPT_SECTOR)"
    fail=1
fi
rm -f "$LOG2" "$LOG4"

# --- Negativfaelle ------------------------------------------------------------------------------
#
# Ein Gate, das nur mit gueltiger Eingabe geprueft wird, ist kein geprueftes Gate. Die drei Faelle
# unten sind genau die, die im Regelbetrieb NIE vorkommen -- und deshalb die, die verrotten.

echo "== Negativfall 1: gar kein Archiv =="
boot "" "$LOG" 25
if grep -q "root    : FAILURES (NoArchive)" "$LOG"; then
    echo "  PASS: ohne Startmenge sagt der Kernel das -- statt still zu idlen"
else
    echo "  FAIL: ohne Startmenge fehlt die Diagnose"; fail=1
fi

echo "== Negativfall 2: Manifest fuer einen ANDEREN Kernel =="
# An ein fremdes Image binden (das mb32 taugt als Stellvertreter: andere Bytes, gueltiges ELF).
# Erwartung: Signatur ist gueltig, Bindung nicht -> abgewiesen.
cp "$KELF" build/fremder-kernel.elf
python3 - "$KELF" build/fremder-kernel.elf <<'EOF' >/dev/null 2>&1
import sys
# Ein Byte im .text-Bereich kippen -> anderer Kernel-Code-Hash, sonst identisches ELF.
d = bytearray(open(sys.argv[1], 'rb').read())
d[0x2000] ^= 0xFF
open(sys.argv[2], 'wb').write(d)
EOF
if build_archive build/boot-archive-fremd.bin build/fremder-kernel.elf 1; then
    boot build/boot-archive-fremd.bin "$LOG" 25
    if grep -q "manifest: FAILURES" "$LOG" && grep -q "root    : FAILURES (NoManifest)" "$LOG"; then
        echo "  PASS: A-1.3 -- ein gueltig signiertes Manifest fuer ein ANDERES Kernel-Image wird abgewiesen"
    else
        echo "  FAIL: A-1.3 -- fremd gebundenes Manifest wurde nicht abgewiesen"; fail=1
    fi
else
    echo "  FAIL: Negativfall 2 liess sich nicht bauen"; fail=1
fi

echo "== Negativfall 3: Modul passt nicht zum Hash im Manifest =="
# Das Manifest gegen `hello` ausstellen, ins Archiv aber ein anderes Binary legen.
python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
    --out build/system.manifest \
    --entry "1:init:0:1:$PROG/hello.elf:loader,ntfn:root:3::any:0" >/dev/null 2>&1
python3 tools/mkarchive.py build/boot-archive-hash.bin --system-manifest build/system.manifest \
    "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" >/dev/null 2>&1
boot build/boot-archive-hash.bin "$LOG" 25
if grep -q "root    : FAILURES (HashMismatch)" "$LOG"; then
    echo "  PASS: A-1.2 -- der erwartete SHA-256 aus dem Manifest wird durchgesetzt, nicht bloss mitgefuehrt"
else
    echo "  FAIL: A-1.2 -- abweichender Modul-Hash wurde nicht bemerkt"; fail=1
fi

# --- Z8/N4: der Knotenwunsch des Manifests -------------------------------------------------
#
# **Nur der negative Fall steht hier, und das ist eine Einschraenkung des AUFBAUS, keine Wahl.**
#
# Gefahren wird: `numa_node=1` auf einer Maschine ohne Topologie -> eine Absage mit dem Grund
# "die Maschine kann es nicht", unterscheidbar von "das Dokument ist falsch". Bis heute waren
# beide Faelle ein Text, und "fehlend" sah aus wie "falsch".
#
# NICHT gefahren wird der positive Fall (`numa_node=1` auf einer Zwei-Knoten-Maschine). Der
# Grund steht in `boot()`: `-m "$RAM" -smp 4` sind dort fest vor den Extras verdrahtet, und eine
# `-numa`-Topologie braucht `-smp sockets=…` und passende `memory-backend`-Groessen an genau
# diesen Stellen. Ein zweites `-m`/`-smp` dahinter kollidiert -- gemessen 2026-08-17: QEMU startet,
# der Gast bekommt aber KEINE SRAT (`numa : readable=false`), und die Zeile las sich wie ein
# Kernelbefund. **`boot()` verschluckt jede QEMU-Meldung nach /dev/null**, was genau diese
# Verwechslung erzeugt -- dieselbe Form wie `iommu_platform=on` seinerzeit.
#
# **Seit dem 2026-08-20 steht der positive Fall darunter.** `boot()` nimmt `-m`/`-smp` jetzt aus
# `MEM_ARGS`/`SMP_ARGS`, und QEMUs Meldungen gehen nicht mehr nach /dev/null -- damit ist eine
# abgewiesene Kommandozeile von einem stummen Kernel unterscheidbar. Beide Faelle laufen hier
# nacheinander durch DIESELBE Suite, und das ist der Punkt: „fehlend" und „vorhanden" waren bis
# dahin zwei verschiedene Werkzeuge (`tools/numa-messen.sh` fuer den einen, diese Suite fuer den
# anderen), und zwei Werkzeuge fuer eine Eigenschaft driften.
echo "== Z8/N4: numa_node=1 ohne Topologie -> die MASCHINE kann es nicht (eigener Grund) =="
python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
    --out build/system.manifest \
    --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:3:1:any:0" >/dev/null 2>&1
python3 tools/mkarchive.py build/boot-archive-numa1.bin --system-manifest build/system.manifest \
    "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" >/dev/null 2>&1
boot build/boot-archive-numa1.bin "$LOG" 40
if grep -q "keine tragfaehige Topologie" "$LOG"; then
    echo "  PASS: N4 -- OHNE Topologie ein UNTERSCHEIDBARER Grund (fehlend != falsch)"
else
    echo "  FAIL: N4 -- erwartet 'keine tragfaehige Topologie': $(grep -m1 'POLICY ABGEWIESEN' "$LOG" || echo 'keine Absage')"; fail=1
fi

echo "== Z8/N4 POSITIV: numa_node=1 auf einer ZWEI-Knoten-Maschine -> eingeloest =="
# **Der Gegenfall zum Test darueber, und er ist der wichtigere.** Eine Absage, die immer kommt, ist
# von einer richtigen Absage nicht zu unterscheiden -- erst wenn dieselbe Manifestzeile auf einer
# Maschine MIT Topologie durchgeht, sagt die Absage etwas ueber die Maschine statt ueber den Kernel.
#
# Die Topologie muss zu `-m` und `-smp` passen: zwei Knoten zu je der Haelfte des RAMs, zwei
# Sockel. Deshalb sind es hier `MEM_ARGS`/`SMP_ARGS` und nicht `BOOT_EXTRA` -- ein zweites `-m`
# hinter dem ersten kollidiert, und genau daran ist der Fall bis heute gescheitert.
# **Die Form ist ABGESCHRIEBEN aus `tools/numa-messen.sh`, nicht neu erfunden** -- und der erste
# Versuch hier war genau der Fehler, den dieses Projekt an anderer Stelle als „zwei Zahlen fuer eine
# Tatsache" fuehrt: ich habe `-numa node,mem=2G` aus dem Kopf geschrieben, und QEMU antwortete
# `Parameter -numa node,mem is not supported by this machine type -- Use -numa node,memdev instead`.
# Gesehen habe ich das nur, weil `boot()` seit heute stderr nicht mehr verschluckt; vorher waere
# daraus ein „der Kernel liest die SRAT nicht" geworden.
NUMA_MEM="-m 2G"
NUMA_SMP="-smp 4,sockets=2,cores=2"
NUMA_TOPO="-object memory-backend-ram,id=m0,size=1G -object memory-backend-ram,id=m1,size=1G -numa node,nodeid=0,cpus=0-1,memdev=m0 -numa node,nodeid=1,cpus=2-3,memdev=m1 -numa dist,src=0,dst=1,val=21"
MEM_ARGS="$NUMA_MEM" SMP_ARGS="$NUMA_SMP" BOOT_EXTRA="$NUMA_TOPO" \
    boot build/boot-archive-numa1.bin "$LOG" 40
# **Zuerst: kam die Topologie ueberhaupt an?** Ohne diese Zeile liest sich ein Aufbauproblem als
# Kernelbefund -- genau die Verwechslung, wegen der dieser Fall ein Jahr lang nicht lief.
if grep -qE "^numa    : init=true readable=true .* nodes=2" "$LOG"; then
    echo "  PASS: N4 -- die Topologie ist im Gast angekommen (SRAT gelesen, 2 Knoten)"
    if grep -q "keine tragfaehige Topologie" "$LOG"; then
        echo "  FAIL: N4 -- MIT Topologie trotzdem abgewiesen: $(grep -m1 'POLICY ABGEWIESEN' "$LOG")"; fail=1
    else
        echo "  PASS: N4 -- derselbe Manifesteintrag wird MIT Topologie eingeloest statt abgewiesen. Die Absage oben sagt damit etwas ueber die MASCHINE, nicht ueber den Kernel"
    fi
else
    echo "  FAIL: N4 positiv -- keine Topologie im Gast. Das ist ein AUFBAU-Problem, kein Kernelbefund:"
    grep -m1 -E "^numa    : init" "$LOG" | sed 's/^/          /'
    [ -s "${QEMU_ERR:-/dev/null}" ] && sed -n '1,5p' "$QEMU_ERR" | sed 's/^/          QEMU: /'
    fail=1
fi

echo "== Negativfall 3b: Manifest in einem FORMAT, das dieser Kernel nicht kennt =="
# **Der Fall, den die Regel verbietet.** Signiert wird die GANZE Nachricht -- ein Lader, der aus
# einem v2-Manifest die ihm bekannten 96 von 104 Byte je Eintrag liest, bekaeme ein Ergebnis, das
# ECHT und MISSVERSTANDEN zugleich ist: `initial_caps` und `policy_flags` laegen auf fremden Bytes,
# und die Signatur stimmte darueber. Geprueft wird deshalb nicht bloss "abgewiesen", sondern dass
# die Absage den VERSIONSUNTERSCHIED BENENNT -- "neuer Kernel, altes Manifest" und "kaputte Bytes"
# fuehren zu entgegengesetzten Handlungen, und bis 2026-08-10 sahen sie gleich aus.
python3 tools/sign_manifest.py --kernel "$KELF" --key "$MANKEY" --manifest-version 1 \
    --out build/system.manifest \
    --entry "1:init:0:1:$PROG/init.elf:loader,ntfn:root:3::any:0" >/dev/null 2>&1
python3 - build/system.manifest <<'PYEOF' >/dev/null 2>&1
import sys
# NUR `entry_len` im Kopf auf 104 setzen. Die Signatur bleibt gueltig ueber die veraenderten
# Bytes? Nein -- und genau das ist hier egal: die Formatpruefung faellt VOR jeder Krypto, und
# dieser Test prueft genau diese Reihenfolge mit.
d = bytearray(open(sys.argv[1],'rb').read())
d[20:24] = (104).to_bytes(4,'little')
open(sys.argv[1],'wb').write(d)
PYEOF
python3 tools/mkarchive.py build/boot-archive-fmt.bin --system-manifest build/system.manifest \
    "1:init:0:1:$PROG/init.elf::certs/init-x86.cert" >/dev/null 2>&1
boot build/boot-archive-fmt.bin "$LOG" 25
if grep -q "im Archiv liegt ein Manifest in einem FORMAT, das dieser Kernel nicht kennt" "$LOG" \
    && grep -q "entry_len=104" "$LOG"; then
    echo "  PASS: A-1.2/Z11: eine unbekannte entry_len wird BENANNT abgewiesen (mit beiden Zahlen), nicht als Formfehler -- und gelesen wird nichts davon"
else
    echo "  FAIL: A-1.2/Z11: unbekanntes Manifest-Format nicht benannt abgewiesen:"
    grep -m1 "^manifest:   im Archiv" "$LOG" | sed 's/^/          /'
    fail=1
fi

echo "== Negativfall 4 (A-5.3): der Selektor passt auf KEIN vorhandenes Geraet =="
# **Der wichtigste der vier.** Die bequeme Zeile im Kernel waere "nichts passt -> nimm irgendeins",
# und sie waere unsichtbar: der Treiber liefe, der Bericht saehe gruen aus, und die Zuteilung
# folgte wieder der Fundreihenfolge statt dem Manifest. Genau diese Zeile gibt es nicht -- und der
# Beleg dafuer ist, dass ein unerfuellbarer Selektor gar keine Zuteilung ergibt.
#
# Der Lauf braucht das VOLLE Archiv und das volle Zeitlimit: die Zuteilung passiert erst, wenn
# `init` laeuft, und die Meldung steht im Abschlussbericht. Ein kurzer Lauf haette hier ein
# Timeout gemessen und es fuer ein Ergebnis gehalten.
if build_archive build/boot-archive-nodev.bin "$KELF" 1 "vendor=dead,device=beef"; then
    boot build/boot-archive-nodev.bin "$LOG"
    # Entscheidend ist, dass Eintrag 3 GAR NICHT auftaucht: `devsel` druckt je Zuteilung eine
    # Zeile, und ohne Zuteilung gibt es keine. Der zweite Treiber (Eintrag 5) bekommt sein Geraet
    # weiterhin -- das ist die Gegenprobe im selben Lauf: der unerfuellbare Selektor trifft GENAU
    # den Eintrag, der ihn traegt, und nicht die Zuteilung als Ganzes.
    if ! grep -q "devsel  : Eintrag 3 verlangt" "$LOG" \
        && grep -q "devsel  : Eintrag 5 verlangt" "$LOG" \
        && grep -q "1 vergeben" "$LOG"; then
        echo "  PASS: A-5.3 -- ein unerfuellbarer Selektor fuehrt fuer GENAU DIESEN Eintrag zu keiner Zuteilung; kein stiller Rueckfall auf 'irgendeins', und der zweite Treiber ist unbeeintraechtigt"
    else
        echo "  FAIL: A-5.3 -- auf einen unerfuellbaren Selektor hin wurde trotzdem ein Geraet vergeben (oder der zweite Treiber ging mit unter)"
        grep -m2 "devsel  :" "$LOG" | sed 's/^/          /'
        fail=1
    fi
else
    echo "  FAIL: Negativfall 4 liess sich nicht bauen"; fail=1
fi

echo "== Negativfall 5 (A-5.3): der Selektor nennt das ANDERE Geraet =="
# Belegt die Richtung, die Negativfall 4 nicht zeigen kann: der Selektor waehlt nicht nur AUS,
# er waehlt das BENANNTE. Hier zeigt er auf die Netzkarte (1af4:1041) -- und der Treiber bekommt
# sie, obwohl das Blockgeraet in der Angebotsliste VOR ihr steht. Ohne diesen Fall waere „es
# passte" von „es war ohnehin das erste" nicht zu unterscheiden.
if build_archive build/boot-archive-otherdev.bin "$KELF" 1 "vendor=1af4,device=1041"; then
    boot build/boot-archive-otherdev.bin "$LOG"
    if grep -q "devsel  : Eintrag 3 verlangt 1af4:1041, bekam RID 0x0020 1af4:1041" "$LOG"; then
        echo "  PASS: A-5.3 -- der Selektor der BLOCK-Treiber-PD zeigt auf die Netzkarte, und sie bekommt die Netzkarte; das Blockgeraet steht in der Angebotsliste DAVOR und bleibt liegen. Ohne diesen Fall waere 'es passte' von 'es war ohnehin das erste' nicht zu unterscheiden"
    else
        echo "  FAIL: A-5.3 -- der Selektor benannte 1af4:1041, vergeben wurde etwas anderes (die Fundreihenfolge entscheidet weiterhin)"
        grep -m2 "devsel  :" "$LOG" | sed 's/^/          /'
        fail=1
    fi
else
    echo "  FAIL: Negativfall 5 liess sich nicht bauen"; fail=1
fi

# **Bei einem Fehlschlag das volle Protokoll BEHALTEN** (D12, 2026-08-05).
#
# Bis hierher loeschte diese Zeile das Log bedingungslos -- und zwar auch dann, wenn der Lauf
# durchgefallen war. Am 2026-08-04 fiel die Suite bei `-m 512M` zweimal aus; isoliert danach
# 6 von 6 gruen. Untersuchen liess sich keiner der beiden, weil das Protokoll in dem Moment weg
# war, in dem es gebraucht wurde. Ich hatte das damals meiner Sammelschleife angelastet -- falsch:
# die anderen beiden Suiten legen bei Abweichung ein Log unter `build/diag/` ab, DIESE hatte den
# Mechanismus nie.
#
# Bei einer Rate um 1/20 kostet jeder verlorene Fehlschlag Stunden. Ein Testaufbau, der seinen
# eigenen Befund wegwirft, misst zwar -- aber er laesst nichts zurueck, woran man arbeiten kann.
if [ "$fail" = 0 ]; then
    rm -f "$LOG"
else
    mkdir -p build/diag
    STEMPEL="$(date +%Y%m%d-%H%M%S)"
    ZIEL="build/diag/load-abweichung-$STEMPEL-${RAM}.log"
    cp -f "$LOG" "$ZIEL" 2>/dev/null && echo "  (Log des LETZTEN Boots: $ZIEL)"
    # **Und der HAUPTBOOT eigens.** `$LOG` wird von jedem weiteren Boot ueberschrieben, und der
    # letzte ist ein NEGATIVFALL -- wer nach einem Fehlschlag „das abgelegte Protokoll" liest,
    # liest also den falschen Lauf. Am 2026-08-10 ist genau das zweimal passiert, mit Zahlen aus
    # einem Boot, der die Frage gar nicht stellte. Ein Protokoll, das den falschen Lauf zeigt, ist
    # schlimmer als keins: es sieht aus wie eine Antwort.
    HAUPT="build/diag/load-hauptboot-$STEMPEL-${RAM}.log"
    printf '%s\n' "$OUT" > "$HAUPT" && echo "  (Log des HAUPTboots:      $HAUPT)"
    rm -f "$LOG"
fi
rm -f "$BLK_IMG"
echo "fingerprint: $(fingerprint "$KELF") (Kernel-Binary, das GERADE geprueft wurde -- schliesst"
echo "             'veralteter Build' als Erklaerung fuer eine Abweichung aus)"
bekannt_rot_pruefen "$OUT" || fail=1

# --- Ein Dienst OHNE Geraet ist auffindbar (2026-08-25) ----------------------------------------
if grep -q "^dienst  : ALL PASS" <<<"$OUT"; then
    echo "  PASS: Eine PD ohne Geraet wird unter ihrer program_id REGISTRIERT, und ein Client, der sie mit service_id benennt, bekommt IHREN Endpoint. Bis heute lief set_driver_service nur auf dem HardwareLand-Zweig: der Client bekam None und danach einen frischen, UNVERBUNDENEN Endpoint -- beide Seiten haetten einen Kanal gehabt und keinen gemeinsamen, mit gueltigen Caps und ohne eine einzige Fehlermeldung. Gemessen wird eine Gleichheit UND eine Ungleichheit: der Kanal des Clients ist der des Dienstes und NICHT der des Blockdienstes; ohne die zweite Haelfte belegte die erste nur, dass irgendein Endpoint dort steht. $(grep -m1 -oE 'dienst-ep=[^ ]+ client-ep=[^)]+' <<<"$OUT")"
elif grep -q "^dienst  : SKIP" <<<"$OUT"; then
    echo "  FAIL: dienst meldet SKIP -- diese Suite HAT einen geraetelosen Dienst in der Startmenge"
    fail=1
else
    echo "  FAIL: Ein Dienst ohne Geraet ist nicht auffindbar: $(grep -m1 -oE '^dienst  : FAILURES [^-]*' <<<"$OUT")"
    fail=1
fi

# --- Das Endowment wird verbucht (2026-08-25) --------------------------------------------------
#
# Drei Ausgaenge und nicht zwei. Die Hauptsuite laedt bauartbedingt kein Programm; dort ist SKIP
# das RICHTIGE Ergebnis, und ein PASS waere die Luege ("nichts gebrochen" gegen "nichts
# gemessen"). Die Lade-Suite hat das Archiv und muss deshalb ALL PASS liefern -- mit einer Zahl
# groesser null bei `geprueft`, sonst hat der Ladepfad nicht stattgefunden.
if grep -q "^endow   : FAILURES" <<<"$OUT"; then
    echo "  FAIL: Eine Zusage des signierten Manifests liess sich nicht installieren -- ein Programm haette mit weniger Autoritaet angefangen, als das Dokument ihm zuspricht: $(grep -m1 -oE '^endow   : FAILURES [^-]*' <<<"$OUT")"
    fail=1
elif grep -q "^endow   : ALL PASS" <<<"$OUT"; then
    echo "  PASS: Das Endowment wird VERBUCHT statt still gekuerzt: eine Zusage des Manifests, die sich nicht installieren laesst, weist den Ladevorgang ab -- und zwar BEVOR etwas alloziert ist, also ohne Teardown-Pfad. Ein Angebot des Aufrufers (SYS_LOAD, Slot 0) darf eine Zieldomaene ablehnen und wird GEZAEHLT statt verschwiegen; genau diese stille Ablehnung ist der Grund, warum die Doku-Tabelle in virtio-blk bis heute behauptet, in Slot 0 laege eine Notification. $(grep -m1 '^endow   :' <<<"$OUT" | grep -oE 'geprueft=[0-9]+ Zusagen-gebrochen=[0-9]+ Angebote-abgelehnt=[0-9]+')"
elif grep -q "^endow   : SKIP" <<<"$OUT"; then
    echo "  FAIL: endow meldet SKIP -- diese Suite HAT ein Boot-Archiv und laedt Programme. SKIP heisst hier, dass der Ladepfad gar nicht lief"
    fail=1
else
    echo "  FAIL: die endow-Zeile fehlt ganz -- der Bericht kam nicht bis dorthin"
    fail=1
fi
if [ "$fail" = 0 ]; then echo "== ALL PASS =="; else echo "== FAILURES =="; fi
exit "$fail"
