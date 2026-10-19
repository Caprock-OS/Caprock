# ADR 0021 — Memory safety of the software `unsafe` sites (Kani, step by step)

Status: **accepted** · Date: 2026-06-27 · complements functional verification (phases 1–6).
Reference: ADR 0015 (verification approach), `ARMTest/unsafe-memory-safety-aufwand.md` (effort analysis),
`Verification/unsafe-safety/`, `docs/verification.md` (Kani region/sync).

## Context

The safe-Rust core (~5000 LOC) is memory safe by compiler; what remains to prove is only the
memory safety of the `unsafe` blocks. The effort analysis (`ARMTest/unsafe-memory-safety-aufwand.md`)
sharply divides the ~200 sites into **category A** (pure RAM pointers/slices — provable with Kani/Verus;
`region`/`sync` already done) and **category B** (hardware/machine model: page tables, MMIO,
context-switch assembly — research class, encapsulated as a trusted HAL base). This ADR deals with
**category A**: the individual software `unsafe` sites that are readily provable in 1–3 h are
proved memory-safe with Kani **one by one**.

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 `#[cfg(kani)]` in the kernel crate | harnesses directly in the kernel code (like region/sync) | the kernel crate builds only under a custom target + build-std + HW deps -> not buildable under `cargo kani` (host target) |
| V2 Verus buffer model | prove the model instead of the real `core::ptr` ops | proves the arithmetic, but does **not** check the real unsafe ops |
| **V3 standalone Kani artefact (chosen)** | `Verification/unsafe-safety/`: **faithful copies** of the kernel unsafe glue, Kani executes the **real** `core::ptr` ops on a modelled buffer | checks the real unsafe operations (OOB/underflow/UB); kernel unchanged; fidelity of the copy = small, auditable TCB |

## Decision

**V3** — a standalone Kani verification artefact (`Verification/unsafe-safety/kani/`): every
`*_logic` function is a **byte-exact copy** of the unsafe glue of one kernel site (line
referenced); Kani harnesses execute the real `copy_nonoverlapping`/`write_bytes`/… on a
modelled buffer and prove freedom from out-of-bounds/underflow/UB **under the documented
precondition** — which in turn is proved at the caller as pure arithmetic. Real kernel code
unchanged; via `tools/kani-verify.sh unsafe` + CI gate.

**Selection criterion "sensibly provable":** a site is included if it is (a) category A
(ordinary RAM) and (b) readily provable in 1–3 h. Pure trust primitives without in-function bounds
(`mem::peek/poke` on arbitrary cap addresses, MMIO `volatile`, fixed RAM-window slices) are **not**
"proved", but documented as an HW/cap contract (their validity comes from the already
verified cap system or the HAL TCB).

## Consequences

- **Positive:** the software `unsafe` sites (category A) obtain a machine-checked
  memory-safety proof with the real `core::ptr` operations; complements the existing region/sync
  Kani proofs. Extensible step by step (one site per iteration).
- **Limits:** **category B** (page tables/MMIO/assembly) remains the axiomatised, hand-audited
  HAL TCB (cf. effort analysis). The **fidelity** of copy ↔ kernel site is a documented
  assumption (small, one line reference per site; to be updated whenever the kernel changes).

## Sites covered (grows with each iteration)

| # | Kernel site | Property | Status |
|---|---|---|---|
| 1 | `kernel/src/system.rs::copy_segment` (the only unsafe site of the load path) | copy+`.bss` zeroing stays in the target frame; precondition `filesz<=total` established at the caller | ✅ |
| 2 | `crates/caprock-region/src/heap.rs` (slab free list, `read`/`write` of the embedded successor pointer) | every size class holds a `usize` + is usize-aligned; raw read/write of the slot pointer in-bounds + aligned (round trip) | ✅ |
| 3 | `kernel/src/system.rs` code copy (system.rs:1154) | guard `code_len<=clen` checked in the same body (system.rs:1140) protects the raw copy -> stays in the `clen` frame | ✅ |
| 4 | `kernel/src/system.rs::alloc_zeroed` (system.rs:1676) | `write_bytes(base,0,len)` over a `len`-byte block in-bounds + zeroes every byte | ✅ |
