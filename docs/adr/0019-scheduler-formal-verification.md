# ADR 0019 — Formal functional verification of the scheduler (Verus, phase 5)

Status: **accepted** · Date: 2026-06-27 · Phase 5 (last) of functional verification.
Reference: ADR 0015 (verification approach), ADR 0005 (per-core parallel MCS scheduler),
`Verification/scheduler/`, run-time `Scheduler::audit`.

## Context

The scheduler (`caprock-sched`) decides **which** thread runs. An inconsistent
ready queue (dead/blocked entry, duplicate, lost runnable thread) breaks
availability; a faulty MCS budget accounting permits run-time theft (a thread runs
beyond its budget) or starvation (a thread is never scheduled again). At run time
`Scheduler::audit` (codes 1–7) + the hwfuzz check; phase 5 **proves** the core invariants of the
sequential single-core model. **Concurrency** (the eight per-core instances, each behind its own
lock, reschedule IPIs, context switches) stays **outside** (concurrency/HAL TCB, cf. ADR 0015).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 concurrent multi-core model | eight instances + IPIs + migration in interleaving | Verus is single-threaded; needs Loom/TLA+ -> outside (ADR 0015). The code keeps every instance behind its own lock -> the single-core view is the faithful sequential abstraction |
| V2 concrete ring-buffer queues | rebuild `RunQueue` (buf/head/tail/count) bit-exactly | proves the ring-buffer implementation, not the *scheduling property*; much ballast, little security gain (the ring buffer is small + a candidate closer to fuzzer/Kani) |
| **V3 membership abstraction (chosen)** | ready queue as a per-thread flag `in_ready`; the invariant couples it to the thread state | captures **exactly** the `audit` property (codes 1/2/3/4/7) + the MCS budget invariants; duplicate freedom (code 3) is structural through the flag; small + faithful to the serialised reality |

## Decision

**V3** — model and prove the sequential single-core model with **membership coupling**:

1. **Runqueue coupling** (`coupled`, heart of `audit`): a thread is in a
   ready queue **if and only if** it is **runnable** (allocated ∧ not blocked ∧ not exhausted) and **not the
   running one**. Covers audit codes 1 (no dead entry), 2 (no blocked one), 4 (current not
   ready), 7 (no lost one); code 3 (no duplicate) is structural (flag instead of multiset).
2. **MCS budget coupling** (`budget_inv`): remaining budget ≤ budget; exhausted ⟹ remainder 0; **budget==0 ⟹
   never exhausted** (round robin never starves).
3. All operations (`block_current`, `pick`, `unblock`, `pause`, `tick_charge`, `refill`,
   `set_budget`) **preserve** both invariants; `refill` restores the budget, `set_budget`
   rescues an exhausted thread from loss, `tick_charge` deschedules cleanly on exhaustion.

Abstract model (V2 in the sense of ADR 0015 — faithful, real code unchanged).

## Consequences

- **Positive:** the scheduler core properties (ready queue consistency, no lost thread,
  MCS bound, no starvation, progress via an always-ready idle) are proved and complement
  `Scheduler::audit` + the hwfuzz. **Phase 5 completes the functional core verification.**
- **Limits/open:** **priority queue separation** (audit code 6 — thread in the queue of its
  priority) is structural and not modelled here; **budget donation** (intra-core IPC,
  shared scheduling context), the **zombie/reap lifecycle** and **concurrency** (multi-core,
  IPIs, migration, context switch) remain expressly outside (follow-up stages or HW TCB).
