# ADR 0018 — Formal functional verification of IPC (Verus, phase 4)

Status: **accepted** · Date: 2026-06-27 · Phase 4 of functional verification.
Reference: ADR 0015 (verification approach), `Verification/ipc/`, run-time `ipc_audit`.

## Context

IPC is the only permitted communication channel of isolated PDs. The correctness of the CALL/REPLY
rendezvous + the consistency of the endpoint queues (no message loss, no duplicates)
are security-critical. At run time `ipc_audit` + the IPC fuzzer check this; phase 4 **proves** the
sequential protocol model. **Concurrency** (simultaneous multi-core accesses) stays **outside**
(serialised by the `CAPS`/`EPS` locks — concurrency/HAL TCB, cf. ADR 0015 + the Kani sync boundary).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 concurrent model (interleavings) | simultaneous senders/receivers on several cores | Verus is single-threaded; needs Loom/TLA+ -> outside (see ADR 0015) |
| **V2 sequential protocol (chosen)** | endpoint as sender/receiver queues; `send`/`recv`/(`reply`) as state transitions | captures the core invariant (rendezvous exclusion, no loss/duplicate, progress); small + faithful to the serialised reality |

## Decision

**V2** — model and prove the sequential endpoint protocol: (1) **rendezvous exclusion**
(`send`/`recv` preserve "never sender **and** receiver blocked at the same time"); (2) **no
message loss/no duplication** (`send` raises the total by exactly 1; `recv` delivers a
pending message **exactly once**); (3) **rendezvous progress** (with a partner present,
immediate delivery, no deadlock). Abstract model; real code unchanged.

## Consequences

- **Positive:** the CALL/REPLY core protocol (endpoint consistency, message preservation, progress)
  is proved, complements `ipc_audit` + the IPC fuzzer.
- **Limits/open:** **reply caps** (an outstanding reply belongs to exactly one caller) +
  reply liveness (ERR_SERVER_GONE) + cap transfer in IPC are the next stages; **concurrency**
  stays expressly outside (Loom/TLA+/concurrency logic, hardware trust boundary).
