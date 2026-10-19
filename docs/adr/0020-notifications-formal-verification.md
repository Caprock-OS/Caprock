# ADR 0020 — Formal functional verification of notifications (Verus, phase 6)

Status: **accepted** · Date: 2026-06-27 · Phase 6 of functional verification (further component).
Reference: ADR 0015 (verification approach), ADR 0018 (IPC), `Verification/notifications/`,
run-time `Notification::audit`.

## Context

Notifications are the **asynchronous**, non-blocking signal channel (badge-accumulated events,
among others the deferred IRQ delivery to HardwareLand backends). They complement synchronous IPC (ADR 0018):
a `signal` ORs badge bits into the pending word and wakes any waiter; a `wait` fetches the
word or blocks. If a signal is lost or a waiter stays blocked despite pending signals
(lost wakeup), the channel is faulty. At run time `Notification::audit` (dead
waiter) + the fuzzer check; phase 6 **proves** the sequential signal/wait protocol.
**Concurrency** + cross-core waking (`unblock`+IPI) stay **outside** (concurrency/
HAL TCB, ADR 0015).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 u64 bit word bit-exactly | pending as `u64`, `signal`=`\|=`, proofs via bit_vector | forces the SMT bit-vector solver for every property; much ballast, the security gain (bit accumulation) can be expressed more cleanly in set theory |
| **V2 badge bits as `Set<nat>` (chosen)** | pending as a bit set; `signal`=union, `wait`=drain | captures **exactly** "no bit is lost / consumed exactly once"; small + faithful (OR ≙ union) |

## Decision

**V2** — model and prove the sequential notification protocol: (1) **invariant**
`waiter is Some ==> pending empty` (a blocked waiter never sits on undelivered signals);
(2) **no signal loss** (every signalled bit is afterwards in the pending word **or** delivered to the
woken waiter); (3) **exactly-once consumption** (`wait` fetches the entire pending word
and empties it — no remainder, no duplicate); (4) **progress** (with a pending word, `wait` does not
block — no lost wakeup); `signal`/`wait`/`purge` preserve the invariant. Abstract model; real
code unchanged.

## Consequences

- **Positive:** the signal/wait core protocol (no signal loss, exactly-once consumption, no
  lost wakeup) is proved, complements `Notification::audit` + the fuzzer.
- **Limits/open:** **multiple waiters** (currently one consumer per notification), **notification
  binding to a TCB** (bound delivery) and **concurrency** (cross-core
  `unblock`+IPI) remain expressly outside (follow-up stages or HW TCB).
