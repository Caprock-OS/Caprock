# ADR 0023 — Concurrency verification of the sync primitives (Loom)

Status: **accepted** · Date: 2026-06-29 · complements functional verification (Verus/Kani).
Reference: ADR 0015 (verification approach), `crates/caprock-sync`, `Verification/concurrency/`,
`docs/invariants.md` §1a (IRQ-safe locks).

## Context

Verus (tier 2) is **single-threaded** and cannot capture **concurrency** (interleavings across cores)
— this was consistently excluded as an HW/concurrency trust boundary. But the
security-critical synchronisation primitives (`SpinLock` = IRQ-safe ticket lock, `RwSpinLock` =
writer-preferring) carry the entire SMP correctness. The `RwSpinLock` contains a **subtle**
spot: the release clears ONLY the WRITER bit (`fetch_and(!WRITER)`, **not** `store(0)`), because a
reader may have transiently counted up optimistically. This argument had so far been
justified only **by reading the code**.

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 code review only | manual interleaving analysis | error-prone; the transient-reader argument is subtle |
| V2 TLA+/Spin | formal model of the lock logic | powerful, but a separate model (drift from reality), high effort |
| **V3 Loom (chosen)** | exhaustive interleaving exploration of the **real Rust code** (faithful copy with loom atomics) | checks the real lock logic over ALL interleavings; small; with a sensitivity counter-proof |

## Decision

> **Superseded since 2026-08-02 (B-7.2).** The copies are gone: `tools/loom-verify.sh` takes
> `crates/caprock-sync/src/lib.rs` **unchanged**, and the proofs are in the same file
> (`#[cfg(all(loom, test))] mod loom_proofs`). Measured why this matters: a weakened
> memory ordering in the ticket release (`Release` → `Relaxed`) passed **all**
> proofs on the copy; on the real code 2 of 10 fail. In `Verification/concurrency/loom/src/` only
> `hierarchy.rs` and `crosscore.rs` remain — they model **other** subjects (global
> lock order, cross-core IPC) and are not replicas of `caprock-sync`.
>
> The following section describes the superseded state and stays, because the reasoning
> for the decision is found there.

**V3** — a standalone Loom artefact (`Verification/concurrency/loom/`): **faithful copies** of the
`RwSpinLock` and `TicketLock` logic with `loom` atomics + `loom::cell::UnsafeCell`. `loom::model()`
explores **all** thread interleavings and checks: mutual exclusion, **no lost update**
(two writers → exactly +2), **no torn read** (a reader sees only consistent values), and that the
`fetch_and(!WRITER)` release preserves a **transient reader count**. Via `tools/loom-verify.sh`
(copies to `$TMPDIR` — Loom needs host std + crates.io, the workspace would otherwise force build-std).

**Sensitivity counter-proof:** with an injected `store(0)`-instead-of-`fetch_and` bug **Loom catches** it (two
of the three RwSpinLock proofs fail) → the harness is demonstrably meaningful (analogous to the
kernel sensitivity tests).

## Consequences

- **Positive:** the RwSpinLock release (`fetch_and`) is machine-verified as **necessary** (over
  all interleavings), not only by reading; the ticket lock exclusion likewise. **Extended** (second
  stage) to two further models: the **global lock hierarchy** (`hierarchy.rs` — the observed
  nestings CAPS→MEM/DMA_CTX→MEM/SCHEDS→FP_STATES are deadlock-free when concurrent; an inversion
  demonstrably deadlocks) and the **cross-core IPC** paths (`crosscore.rs` — one lock per op: one
  SCHEDS lock each, never two → deadlock-free; two held SCHEDS demonstrably deadlock). 8 models, each
  checked with a sensitivity counter-proof.
- **Limits/open:** the hierarchy/cross-core models use **representatives** of the real locks
  (not every path statically); the complete "every path nests ascending" proof remains with the
  lock-ordering sweep + the audits. DAIF IRQ masking is orthogonal (not modelled).
