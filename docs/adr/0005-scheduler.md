# ADR 0005 — Scheduler

**Status:** implemented + refined · **Date:** 2026-06-23

> **Implementation status:** per-core bitmap scheduler (approach B) with fixed affinity
> is implemented (`caprock-sched`): preemptive, O(1) selection,
> `block`/`unblock`/`switch_to`, `exit`/`kill`/`reap`. **FP context: eager**
> (full q0..q31 save/restore in the trap path). **Lazy FP was tried and
> rejected** — `CPACR` trapping at EL1 also traps the kernel's own NEON code;
> lazy FP needs an EL0/EL1 separation (real userland) and is tied to it
> (see `docs/phase-reports/ext-3-lazy-fp-locks.md`). **Lock granularity:**
> the scheduler lock (`SCHED`) is separate from the resource lock (`RES`) (fixed order
> RES→SCHED); real per-core *parallel* scheduler instances (with cross-core IPI)
> are a follow-up refactor that has been noted. MCS scheduling contexts (budget/donation)
> remain open.

## Motivation

The scheduler is to be **deterministic, multicore-capable, ARM-optimised** and
**low-latency**. Target configuration: 8 cores, 4 GiB RAM.

## Reference: seL4

Classic: per domain/priority ready queues with an L1/L2 bitmap for O(1) selection,
round-robin domain scheduling (determinism/isolation). MCS variant: explicit
**scheduling contexts** (budget + sporadic-server refill), budget donation on
reply. SMP: per-core scheduler state, IPIs for reschedule. Details:
[`../seL4-architecture-map.md`](../seL4-architecture-map.md).

## Approaches analysed

### A) Global run queue
- **+** Simple load-balancing behaviour.
- **−** Global lock → contention and non-determinism on 8 cores. Rejected.

### B) Per-core ready queues + bitmap priorities, fixed affinity (chosen)
Each core has its own ready queues with an L1/L2 priority bitmap (O(1) selection).
Threads have a **fixed core affinity** (explicit, not heuristically
migrating). Migration only by an explicit capability call.

- **+** No global lock → determinism + scaling.
- **+** O(1) selection, predictable latency.
- **+** Fixed affinity = reproducible timing (good for *deterministic*).
- **−** Load balancing is explicit/manual instead of automatic (deliberate: predictability
  before heuristics).

### C) MCS scheduling contexts for budgeting (adopted as a layer)
Budget + period per scheduling context, sporadic-server refill, donation on
reply — as an optional layer on top of B.

- **+** Temporal isolation/guarantees, deterministic budgets, ideal for
  real time and for fair treatment of hot-reloadable services.
- **−** More state/complexity; hence step by step after the basic scheduler.

## Decision

**B as the base, C as a layer.** Per-core bitmap scheduler with fixed affinity
and priorities; on top optional scheduling contexts (budget/refill/donation)
on the MCS model. Cross-core reschedule via targeted IPIs (ADR 0004), not via
global locks. Idle thread per core (`wfi`/`wfe`).

ARM specifics: the time base is the **generic timer** (`CNTPCT`/`CNTV`), interrupts
via the **GIC** (phase 1). Context switch always saves GP registers, FP/SIMD
**lazily** (only if the thread uses them) — saves cycles in the common case.

## Security implications

Fixed affinity + per-core state reduce cross-core side channels and
timing variability. Scheduling contexts prevent a compromised
component from monopolising CPU time (availability protection).

## Performance implications

O(1) selection, no global contention, no TTBR switch on a switch (ADR 0002)
→ low, constant context-switch latency. Lazy FP saves the SIMD save/restore
in the typical case.
