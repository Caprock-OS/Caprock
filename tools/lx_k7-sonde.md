# K7 FORK probe: draft (strand 9, patch text for bringup.rs ownership)

As of 2026-09-10: `SWEEP_K7_FORK_PFAD=10` counts, but the sweep does not exercise the FORK path
(`bringup.rs` prose: "own probe open"). This draft turns it into
path 8. NOT applied (bringup.rs is shared/timing-sensitive) — for review/application
by B. Full hunk text in the strand-9 report (session log 2026-09-10).

## Findings (they carry the draft)

1. The sweep context cannot fork: `dispatch_fork` needs a source ASID (`asid==0` →
   `ERR_NOPD`). Source PD: a `spawn_isolated_native` PD (registered frames + entry).
2. At most 9 of 10 can be provoked: `MANGEL_L2_TABELLE` (`system.rs:4266`) is structurally dead
   (fresh ASID → always `Some`), like class 6/`4207`. Kstack/VSpace callee reports
   are covered by existing `benannt_alloc` sites.
3. The child starts at the PARK entry (`FORK_PARK_EINTRITT`), the second thread at the FORK stub —
   otherwise a fork bomb (every child forks again).

## Building blocks (all `kernel/src/arch/x86_64/bringup.rs`, `#[cfg(feature="selftest")]`)

- `SWEEP_PFADE` 8→9, `PFAD_FORK=8`, `SWEEP_KMAX` += 16 (10 sites + callee + reserve).
- Class-7 prose: "9 of 10 provoked, L2 dead like class 6".
- `sweep_versuch`: probe set-up (native PD + second thread via `spawn_parked`/`admit_in_pd`)
  BEFORE `mangel_vergiften()`; `nicht_fahrbar` return path on failure.
- New `PFAD_FORK` arm: exactly ONE fork per GO (handshake statics `FORK_GO/FERTIG/CODE/KIND`,
  pattern Q_PROBE), then clean-up (`destroy_loaded` + `free_pd_slot`, succeeded as well as
  rejected thanks to `kind_aufraeumen`).
- Verdict: demand 9 codes (without L2), `stumm/keiner/menge_falsch==0`; extend the report by
  `FORK={} ({}, Toepfe)`; `provoziert` computation only if `je_pfad[FORK]>0`.
- Balance: clean-up per pass + probe teardown at the end (conjuncts!).

## Risks

Fork bomb on an entry mix-up (mitigation: stub≠entry by construction +
at most 1 live child per pass); `forkexec_syscall` reachability assumed;
core-local lock (preemption → honestly "capped"); timing baselines shift
(the probe runs after all verdicts, like the rest of the sweep).
