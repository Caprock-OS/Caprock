#!/usr/bin/env python3
"""Caprock — Continuous-Soak-Orchestrator (Burn-in #2).

EINE Kernelinstanz (Feature `soak`) laeuft viele Stunden OHNE Neustart und arbeitet kontinuierlich:
nach dem regulaeren Selbsttest faehrt der Idle-Manager statt `system_off` eine Endlos-Epochenschleife
(Loader-/Zertifikat-Gate, DMA-Round-Trip, Allokator-Churn — jede Epoche balance-neutral, am Ende
Audits + Ressourcen-Baseline gegen den Soak-START geprueft). Der Kernel-Kern ist byte-identisch zum
Release (nur Harness/Testcode hinter Feature `soak`).

Dieser Host-Orchestrator bootet EINE lange QEMU-Instanz (KEIN system_off), liest den seriellen Strom
KONTINUIERLICH, parst die `SOAK`-Heartbeats und verfolgt die KURVEN ueber die Zeit (freies RAM =
zentraler Langzeit-Konsistenz-Indikator; Cap-Anzahl/Audits/Op-Zaehler), erkennt Anomalien/Faults/
Panics + HEARTBEAT-LUECKEN (= Hang/Stillstand) und schreibt den Soak-Bericht.

Aufruf:  tools/soak.py [--hours H] [--gap S] [--timeout S] [--no-build] [--outdir DIR]
Beendet bei Zieldauer / SIGINT / Panic / FAILURES / Heartbeat-Luecke > --gap; schreibt dann den
Abschlussbericht (Speicher-Kurve Start<->Ende, Drift-Analyse, Audit-Zeitreihe, Anomalien, Uptime).
"""
import argparse, json, os, re, select, signal, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ELF = "build/target/aarch64-caprock/release/caprock-kernel.elf"
ARCHIVE = "build/boot-archive.bin"
HELLO = "programs/build/target/aarch64-caprock-user/release/hello.elf"
SVCDEMO = "programs/build/target/aarch64-caprock-user/release/svc-demo.elf"
TBIN = "tests/build/target/aarch64-caprock-user/release"

QEMU = [
    "qemu-system-aarch64", "-machine", "virt,iommu=smmuv3", "-cpu", "cortex-a72",
    "-smp", "8", "-m", "4G", "-nographic", "-serial", "mon:stdio", "-no-reboot",
    "-net", "none", "-device", "pcie-root-port,id=rp0,chassis=1",
    "-device", "virtio-rng-pci,bus=rp0",
    "-device", f"loader,file={ARCHIVE},addr=0x13F000000", "-kernel", ELF,
]

RX_start = re.compile(r"SOAK start (.*)")
RX_hb = re.compile(r"SOAK hb (.*)")
RX_anom = re.compile(r"SOAK ANOMALY (.*)")
RX_soakhead = re.compile(r"SELFTEST COMPLETE -> SOAK")
RX_panic = re.compile(r"(?i)\bpanic\b")
RX_failures = re.compile(r"FAILURES")


def kv(s):
    """'k=v k=v ...' -> dict (ints wo moeglich)."""
    d = {}
    for tok in s.split():
        if "=" in tok:
            k, v = tok.split("=", 1)
            try:
                d[k] = int(v)
            except ValueError:
                d[k] = v
    return d


def sh(cmd, **kw):
    return subprocess.run(cmd, cwd=ROOT, shell=isinstance(cmd, str),
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **kw)


def sign(crate, elf, pid, ver, out):
    r = sh(["python3", "tools/sign_trusted.py", "--key", "keys/trusted-test.ed25519",
            "--crate", crate, "--elf", elf, "--program-id", str(pid), "--version", str(ver),
            "--out", out])
    if r.returncode != 0:
        sys.exit(f"SIGN {out} FAILED")


def build():
    print("== build (release, FEATURE soak) + Programme + Testdienste + signiertes Archiv ==", flush=True)
    # Schlüssel-Bootstrap (ensure manifest + trusted, konsistent zur Key-DB) — MUSS vor dem Build
    # laufen, damit die frischen öffentlichen Schlüssel in den Kernel kompiliert werden. Das schloss
    # die Lücke: soak.py signierte TrustedSAS-Zertifikate, ohne je den Trusted-Schlüssel
    # sicherzustellen (frischer Clone → keys/ fehlt → sign_trusted.py schlug fehl).
    kr = subprocess.run(["bash", "tools/dev-keys.sh"], cwd=ROOT,
                         capture_output=True, text=True)
    if kr.returncode != 0:
        sys.exit(f"DEV-KEYS FEHLGESCHLAGEN (rc={kr.returncode}):\n{kr.stdout}\n{kr.stderr}")
    if sh(["./build.sh", "--features", "soak"]).returncode != 0:
        sys.exit("BUILD FAILED")
    if sh("cd programs && rustup run nightly cargo build --release").returncode != 0:
        sys.exit("PROGRAMS BUILD FAILED")
    if sh("cd tests && rustup run nightly cargo build --release").returncode != 0:
        sys.exit("TESTS BUILD FAILED")
    with open(os.path.join(ROOT, "build/_probe.bin"), "w") as f:
        f.write("PLACEHOLDER")
    # ext-28: TrustedSAS-Binaries signieren (program_id/version == Archiv-Eintrag).
    os.makedirs(os.path.join(ROOT, "certs"), exist_ok=True)
    sign("programs/trusted/svc-demo", SVCDEMO, 12, 1, "certs/trusted-x.cert")
    sign("tests/services/trusted/aggressor", f"{TBIN}/aggressor-t.elf", 24, 1, "certs/aggressor-t.cert")
    mk = ["python3", "tools/mkarchive.py", ARCHIVE,
          f"10:hello:2:1:{HELLO}", f"11:hwhello:1:1:{HELLO}",
          f"12:trusted-x:0:1:{SVCDEMO}::certs/trusted-x.cert", "2:probe:2:1:build/_probe.bin",
          f"20:aggressor-u:2:1:{TBIN}/aggressor-u.elf", f"21:intruder-u:2:1:{TBIN}/intruder-u.elf",
          f"22:aggressor-h:1:1:{TBIN}/aggressor-h.elf", f"23:intruder-h:1:1:{TBIN}/intruder-h.elf",
          f"24:aggressor-t:0:1:{TBIN}/aggressor-t.elf::certs/aggressor-t.cert",
          f"25:intruder-t:0:1:{TBIN}/intruder-t.elf"]
    if sh(mk).returncode != 0:
        sys.exit("ARCHIVE BUILD FAILED")


def new_state(args):
    return {
        "started_at": time.time(), "started_iso": time.strftime("%Y-%m-%d %H:%M:%S"),
        "args": vars(args), "base": None, "heartbeats": [], "anomalies": [],
        "selftest_complete": False, "soak_started": False, "panic": False, "failures": False,
        "end_reason": None, "last_event": time.time(),
    }


def write_report(state, outdir, final=False):
    s = state
    hbs = s["heartbeats"]
    base = s["base"] or {}
    lines, A = [], lambda x: lines.append(x)
    up_h = (time.time() - s["started_at"]) / 3600.0
    A("# Caprock — Continuous-Soak-Bericht (Burn-in #2, EINE Instanz, Feature `soak`)")
    A("")
    A(f"Status: {'ABGESCHLOSSEN' if final else 'LAUFEND'} · Start: {s['started_iso']} · "
      f"Stand: {time.strftime('%Y-%m-%d %H:%M:%S')}")
    A("")
    A("Eine **einzige** Kernelinstanz laeuft ohne Neustart und arbeitet kontinuierlich (Epochen ueber "
      "Loader/Zertifikat-Gate, DMA-Round-Trip, Allokator-Churn). Jede Epoche baut vollstaendig ab + "
      "prueft **alle** Audits (`trust/domain/cap_cdt/vspace/dma/loader/ipc_audit`) und die "
      "Ressourcen-Baseline **gegen den Soak-START**. Der Kernel-Kern ist byte-identisch zum Release.")
    A("")
    A("## Eckdaten")
    A("")
    A(f"- **Uptime:** {up_h:.2f} h Wall-Clock")
    A(f"- **Heartbeats:** {len(hbs)}")
    if hbs:
        last = hbs[-1]
        A(f"- **Epochen (kumulativ):** {last.get('epoch','?'):,}")
        A(f"- **Uptime (Kernel-Ticks):** {last.get('uptime_ticks','?'):,} (TICK_HZ=100)")
        A(f"- **Kumulative Operationen:** loads={last.get('loads','?'):,} · dmas={last.get('dmas','?'):,} "
          f"· churns={last.get('churns','?'):,} · faults={last.get('faults','?'):,}")
    A(f"- **Ende-Grund:** {s['end_reason'] or '(laeuft)'}")
    A("")
    A("## Speicher-Kurve (zentraler Langzeit-Konsistenz-Indikator)")
    A("")
    if base:
        A(f"- **Soak-Baseline (Start):** free={base.get('free_bytes','?'):,} B "
          f"({base.get('free_mib','?')} MiB) · cap_obj={base.get('cap_obj','?')} · "
          f"cap_slots={base.get('cap_slots','?')}")
    if hbs:
        frees = [h["free_bytes"] for h in hbs if "free_bytes" in h]
        objs = [h["cap_obj"] for h in hbs if "cap_obj" in h]
        slots = [h["cap_slots"] for h in hbs if "cap_slots" in h]
        if frees:
            drift = frees[-1] - frees[0]
            A(f"- **freies RAM:** Start {frees[0]:,} B → Ende {frees[-1]:,} B · "
              f"min {min(frees):,} · max {max(frees):,} · **Drift {drift:+,} B** "
              f"({'STABIL ✓' if min(frees) == max(frees) else 'DRIFT ⚠'})")
            A(f"- **distinkte freie-RAM-Werte ueber alle Heartbeats:** {len(set(frees))} "
              f"({'konstant ✓' if len(set(frees)) == 1 else 'ABWEICHUNG ⚠'})")
        if objs:
            A(f"- **Cap-Objekte:** {len(set(objs))} distinkter Wert(e) "
              f"({'konstant ✓' if len(set(objs)) == 1 else 'ABWEICHUNG ⚠'}) · {sorted(set(objs))}")
        if slots:
            A(f"- **Cap-Slots:** {len(set(slots))} distinkter Wert(e) "
              f"({'konstant ✓' if len(set(slots)) == 1 else 'ABWEICHUNG ⚠'}) · {sorted(set(slots))}")
    A("")
    A("## Audit-Zeitreihe")
    A("")
    if hbs:
        bad = [h for h in hbs if h.get("audit", 0) != 0]
        A(f"- **Audits sauber bei jedem Heartbeat:** {'JA ✓' if not bad else f'NEIN ({len(bad)} Heartbeats != 0) ⚠'} "
          f"({len(hbs)} Heartbeats geprueft)")
    A(f"- **Per-Epoche-Anomalien (Audit/Baseline gegen Start):** {len(s['anomalies'])} "
      f"{'(keine) ✓' if not s['anomalies'] else '⚠'}")
    for a in s["anomalies"][:50]:
        A(f"  - Epoche {a.get('epoch','?')} · code={a.get('code','?')} · free={a.get('free_bytes','?')} "
          f"(base {a.get('base_free','?')}) · obj={a.get('cap_obj','?')} slots={a.get('cap_slots','?')}")
    A("")
    A("## Stillstand / Stabilitaet")
    A("")
    A(f"- **Panic/Assertion:** {'JA ⚠' if s['panic'] else 'keine ✓'}")
    A(f"- **FAILURES:** {'JA ⚠' if s['failures'] else 'keine ✓'}")
    A(f"- **Heartbeat-Luecke (Hang):** {'JA ⚠ (Ende-Grund HANG)' if s['end_reason']=='HANG' else 'keine ✓'}")
    A("")
    A("## Erfolgskriterien (Soak)")
    A("")
    ok_mem = bool(hbs) and len(set(h["free_bytes"] for h in hbs if "free_bytes" in h)) == 1
    ok_aud = bool(hbs) and not [h for h in hbs if h.get("audit", 0) != 0] and not s["anomalies"]
    ok_live = not s["panic"] and not s["failures"] and s["end_reason"] != "HANG"
    A(f"- [{'x' if ok_mem else ' '}] Freies RAM / Cap-Anzahl ueber die gesamte Laufzeit stabil (keine Drift)")
    A(f"- [{'x' if ok_aud else ' '}] Alle Audits == 0 bei jedem Heartbeat + keine Per-Epoche-Anomalie")
    A(f"- [{'x' if ok_live else ' '}] Keine Panic/Assertion/FAILURES, keine Heartbeat-Luecke (kein Hang)")
    A(f"- [{'x' if (final and s['end_reason'] in ('DEADLINE','SIGINT')) else ' '}] Ziel-Uptime erreicht")
    A("")
    A("## Komplement zu Burn-in #1 + offene Luecke")
    A("")
    A("Soak #2 belegt **Dauerbetrieb einer Instanz** + **Langzeit-Speicherkonsistenz** + **anhaltende "
      "Last ueber Zeit** (mit `+kernel-fuzz` zusaetzlich zufaellige Ereignisfolgen im Dauerbetrieb). "
      "**Weiterhin offen fuer beide Nachweise:** reale Hardware statt QEMU-TCG (echte Schwach-Speicher-"
      "Ordnung, Cache/TLB-Timing, SMMU-Durchsetzung — z. B. STM32MP25) als separater Validierungsschritt.")
    A("")
    with open(os.path.join(outdir, "report.md"), "w") as f:
        f.write("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hours", type=float, default=8.0, help="Ziel-Uptime")
    ap.add_argument("--gap", type=int, default=180, help="Heartbeat-Luecke (s) -> Hang-Befund")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--outdir", default="build/soak")
    ap.add_argument("--snapshot-every", type=int, default=10, help="Bericht alle N Heartbeats")
    args = ap.parse_args()

    outdir = os.path.join(ROOT, args.outdir)
    os.makedirs(outdir, exist_ok=True)
    if not args.no_build:
        build()
    if not os.path.exists(os.path.join(ROOT, ELF)) or not os.path.exists(os.path.join(ROOT, ARCHIVE)):
        sys.exit("Kernel-ELF oder Archiv fehlt — ohne --no-build starten.")

    state = new_state(args)
    stop = {"flag": False}
    signal.signal(signal.SIGINT, lambda *_: stop.update(flag=True))
    signal.signal(signal.SIGTERM, lambda *_: stop.update(flag=True))

    raw = open(os.path.join(outdir, "serial.log"), "w", buffering=1)
    plog = open(os.path.join(outdir, "progress.log"), "a", buffering=1)
    plog.write(f"\n=== Soak Start {state['started_iso']} hours={args.hours} gap={args.gap} ===\n")
    print(f"== Soak laeuft: Ziel {args.hours} h, Heartbeat-Gap-Backstop {args.gap}s, Ausgabe {args.outdir}/ ==",
          flush=True)

    p = subprocess.Popen(QEMU, cwd=ROOT, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
    deadline = state["started_at"] + args.hours * 3600
    hb_since_report = 0

    while True:
        if stop["flag"]:
            state["end_reason"] = "SIGINT"
            break
        if time.time() >= deadline:
            state["end_reason"] = "DEADLINE"
            break
        r, _, _ = select.select([p.stdout], [], [], 5.0)
        if r:
            line = p.stdout.readline()
            if line == "":
                state["end_reason"] = "QEMU_EXIT"  # unerwarteter Exit (kein system_off im Soak)
                break
            line = line.rstrip("\n")
            raw.write(line + "\n")
            if RX_soakhead.search(line):
                state["selftest_complete"] = True
            if RX_panic.search(line):
                state["panic"] = True
                state["end_reason"] = "PANIC"
                break
            if RX_failures.search(line):
                state["failures"] = True
            g = RX_start.search(line)
            if g:
                state["base"] = kv(g.group(1))
                state["soak_started"] = True
                state["last_event"] = time.time()
                plog.write(f"[{time.strftime('%H:%M:%S')}] SOAK start {g.group(1)}\n")
                print(f"  SOAK start: {g.group(1)}", flush=True)
            g = RX_hb.search(line)
            if g:
                hb = kv(g.group(1))
                state["heartbeats"].append(hb)
                state["last_event"] = time.time()
                hb_since_report += 1
                with open(os.path.join(outdir, "state.json"), "w") as f:
                    json.dump(state, f, default=list)
                if hb_since_report >= args.snapshot_every:
                    hb_since_report = 0
                    write_report(state, outdir, final=False)
                    print(f"  [{time.strftime('%H:%M:%S')}] hb#{len(state['heartbeats'])} "
                          f"epoch={hb.get('epoch')} freeB={hb.get('free_bytes')} "
                          f"obj={hb.get('cap_obj')} loads={hb.get('loads')} dmas={hb.get('dmas')} "
                          f"anom={hb.get('anomalies')} audit={hb.get('audit')}", flush=True)
                else:
                    plog.write(f"[{time.strftime('%H:%M:%S')}] hb#{len(state['heartbeats'])} "
                               f"{g.group(1)}\n")
            g = RX_anom.search(line)
            if g:
                a = kv(g.group(1))
                state["anomalies"].append(a)
                state["last_event"] = time.time()
                plog.write(f"[{time.strftime('%H:%M:%S')}] ANOMALY {g.group(1)}\n")
                print(f"  ⚠ ANOMALY: {g.group(1)}", flush=True)
        else:
            # Kein Output im Poll-Fenster: Heartbeat-Luecke pruefen (erst nach Soak-Start).
            if state["soak_started"] and (time.time() - state["last_event"]) > args.gap:
                state["end_reason"] = "HANG"
                break

    try:
        p.terminate()
        p.wait(timeout=10)
    except Exception:
        p.kill()
    raw.close()
    write_report(state, outdir, final=True)
    hbs = state["heartbeats"]
    stable = bool(hbs) and len(set(h.get("free_bytes") for h in hbs)) == 1 and not state["anomalies"]
    clean = stable and not state["panic"] and not state["failures"] and state["end_reason"] in ("DEADLINE", "SIGINT")
    print(f"== Soak fertig: Grund={state['end_reason']} · {len(hbs)} Heartbeats · "
          f"Anomalien={len(state['anomalies'])} · {'STABIL/CLEAN' if clean else 'BEFUND ⚠'} · "
          f"Bericht {args.outdir}/report.md ==", flush=True)
    plog.close()
    sys.exit(0 if clean else 1)


if __name__ == "__main__":
    main()
