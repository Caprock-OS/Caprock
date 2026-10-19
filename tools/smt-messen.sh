#!/usr/bin/env bash
# **Does the Z6 stage 1 policy actually bite?** — one boot with real sibling hyperthreads.
#
# ## Why this script exists at all
#
# The `smt` report line gates in `all_done()` on both architectures. Under the suites' normal
# topology it is **vacuously true**: `-smp 4` is QEMU shorthand for `cores=4,threads=1`, so the
# guest sees no siblings, the policy suppresses nobody, and the clause "no two online CPUs share a
# physical core" holds because its antecedent is false.
#
# That is the RMRR-on-q35 trap verbatim: *was auf q35 nicht vorkommt, ist damit nicht abwesend,
# sondern ungeprueft.* A green `smt` line in the main suite says the reading happened, not that the
# policy works. This run is the half that says the policy works.
#
# ## What it can and cannot show — and the distinction is the point
#
# QEMU emulates the sibling **topology**, not the sibling **timing**. `-smp cores=2,threads=2`
# hands the guest a CPUID 0Bh enumeration with an SMT level of width 2, but the four vCPUs are
# ordinary host threads: they do not share execution ports, L1, a TLB or a store buffer.
#
# So this measures the POLICY (is a sibling recognised, suppressed, counted, and does the verdict
# recompute correctly?) and it can NEVER measure the CHANNEL. A green run here is not evidence that
# the isolation property holds on silicon — the same honest limitation as the SMMU-under-QEMU
# finding recorded as ADR 0008 / `docs/invariants.md` §6.
#
# ## The speaking test
#
# Failing "the policy suppressed nobody" would be indistinguishable from "the topology was never
# presented". So this script asserts BOTH directions:
#   * with `cores=2,threads=2` the line must read `topology=Multi` and `suppressed=2`,
#   * with `cores=4,threads=1` the very same kernel must read `topology=Single` and `suppressed=0`.
# One without the other proves nothing: the first alone could be a decoder that always says Multi.
set -uo pipefail
cd "$(dirname "$0")/.."

SEK="${SEK:-90}"
RAM="${RAM:-512M}"
KERNEL=build/target/x86_64-unknown-none/release/caprock-kernel.mb32

if [ ! -f "$KERNEL" ]; then
    echo "smt-messen: FEHLT: $KERNEL -- erst './build-x86.sh --features selftest'"
    exit 2
fi

# Dieselbe QEMU-Zeile wie die Suite. Zwei Aufbauten, die dasselbe verschieden aufsetzen, sind ein
# Riss (Fallenliste) -- und hier waere er besonders teuer, weil die Topologie GENAU die Groesse ist,
# um die es geht.
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    # `host-cache-info=on` bleibt: die Farbarithmetik haengt daran, und ein Lauf mit anderer
    # Cache-Geometrie waere nicht derselbe Kernel unter anderer Topologie.
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
else
    ACCEL=(-cpu Skylake-Client)
fi

mkdir -p build/diag

boot() { # $1 = -smp-Argument, $2 = Ausgabedatei
    timeout --signal=KILL "$SEK" qemu-system-x86_64 \
        -machine q35,kernel-irqchip=split -device intel-iommu,intremap=on,caching-mode=on \
        "${ACCEL[@]}" -smp "$1" -m "$RAM" \
        -kernel "$KERNEL" \
        -serial file:"$2" -display none -no-reboot >/dev/null 2>&1
    return 0
}

fail=0
zeile() { # erste `smt`-Zeile mit dem Topologiefeld aus $1
    grep -m1 "^smt     : topology=" "$1" 2>/dev/null
}

erwarte() { # $1 Beschreibung, $2 Datei, $3 Muster
    local got; got="$(zeile "$2")"
    if [ -z "$got" ]; then
        # **Fehlend und kaputt duerfen nicht gleich aussehen.** Ohne diesen Zweig laese sich ein
        # abgestuerzter Boot als "Muster nicht gefunden" -- also wie ein inhaltlicher Fehlschlag.
        echo "  FEHLSCHLAG ($1): keine smt-Zeile im Protokoll $2 -- Boot abgebrochen?"
        fail=1; return
    fi
    if grep -q "$3" <<<"$got"; then
        echo "  OK   ($1): $got"
    else
        echo "  FEHLSCHLAG ($1): erwartet /$3/"
        echo "                   gefunden: $got"
        fail=1
    fi
}

echo "== Z6 Stufe 1: beisst die Politik? =="

# --- Fall 1: ECHTE Geschwister --------------------------------------------------------------
#
# 2 physische Kerne mit je 2 Threads = 4 logische CPUs. Erwartet: 2 zugelassen, 2 unterdrueckt.
#
# **Dieser Lauf endet erwartungsgemaess im WATCHDOG, und das ist kein Fehlschlag der Politik.**
# Gemessen am 2026-08-17: `bringup : offen waren: cores sweep ist verif` -- vier Konjunkte des
# SELBSTTESTS, die voraussetzen, dass jeder konfigurierte Kern tickt (`sched : core 1 ticks=0`).
# Das Abbild ist fuer "alle MADT-CPUs kommen hoch" geschrieben; mit Stufe 1 tun sie das nicht mehr.
# Eine Annahme des Testabbilds, kein Kerneldefekt -- und ein eigener Eintrag in `TODO.md` Z6.
#
# Deshalb urteilt dieses Skript ueber die `smt`-Zeile und **nicht** ueber den Ausgang des Laufs.
L1=build/diag/smt-threads2.log
boot "cores=2,threads=2" "$L1"
erwarte "cores=2,threads=2" "$L1" "topology=Multi.*logical=4 online=2 suppressed=2"

# --- Fall 2: die Gegenprobe (derselbe Kernel, keine Geschwister) -----------------------------
#
# Ohne diesen Lauf koennte Fall 1 auch von einem Dekoder stammen, der IMMER `Multi` meldet -- die
# Sprechprobe muss in BEIDE Richtungen gehen. Dieser Lauf durchlaeuft die Suite vollstaendig.
L2=build/diag/smt-threads1.log
boot "cores=4,threads=1" "$L2"
erwarte "cores=4,threads=1" "$L2" "topology=Single.*logical=4 online=4 suppressed=0"

# --- Das Urteil selbst muss in beiden Faellen ALL PASS stehen -------------------------------
for f in "$L1" "$L2"; do
    if grep -q "^smt     : ALL PASS" "$f"; then
        echo "  OK   (Urteil $(basename "$f")): smt : ALL PASS"
    else
        echo "  FEHLSCHLAG (Urteil $(basename "$f")): $(grep -m1 '^smt     : readable' "$f" || echo 'keine Urteilszeile')"
        fail=1
    fi
done

if [ "$fail" -eq 0 ]; then
    echo "== SMT-MESSUNG: ALL PASS =="
    echo "   (geprueft ist die POLITIK; der KANAL ist unter QEMU nicht messbar -- s. Kopf)"
    exit 0
else
    echo "== SMT-MESSUNG: FAILURES =="
    exit 1
fi
