# ADR 0013 — Fuzzers behind the optional Cargo feature `kernel-fuzz`

Status: **accepted** · Date: 2026-06-27 · Context: preparation of the long-term test

## Context

The self-test harness (`kernel/src/threads.rs`) contains four **in-kernel fuzzers** — active
test-case generators that serve verification, **not** production operation:

- `fuzz` — generative kernel fuzzer (random cap/thread/VSpace op sequences, phase 1 + SMP).
- `ipcfuzz` — IPC state-machine fuzzer (concurrent actors + KILL/reload/MCS during IPC).
- `hwfuzz` — domain/HW/management cap churn against the policy.
- `loaderfuzz` — faulty ELF variants through the binary loader parser.

Before the long-term test, the **release kernel is to contain no fuzzer code at all**: the fuzzers
are development/verification tools and are treated as **optional build modules**. The
long-term test deliberately runs on the **same** configuration that is used in production (without
fuzzers).

**Decisive findings (analysis):** All four fuzzers live entirely in `threads.rs`; their
statics are referenced **only** there. **No** `system.rs` helper is fuzzer-exclusive — all
cap/PD/DMA operations and **all audit oracles** (`domain_audit`, `cap_audit_cdt`, `vspace_audit`,
`dma_audit`, `loader_audit`, `ipc_audit`) are **shared infrastructure** and stay unchanged
in the kernel. There are exactly **two** places where a **non-fuzzer** test gates on the `DONE` flag
of a fuzzer (chain reconnection points).

## Decision

### 1. Cap feature instead of code removal
New Cargo feature `kernel-fuzz` in `kernel/Cargo.toml`, **not** in `default`. The normal
release build (`cargo build --release`) compiles **without** the feature → **no** fuzzer code in the
binary. The verification build (`--features kernel-fuzz`) contains the fuzzers with **exactly** the
previous functionality.

### 2. Fuzzers in their own, feature-gated module
`threads.rs` → `threads/mod.rs`; the four fuzzers (driver functions, statics, constants,
helper structs, the idle-manager driver steps) move into the **child module**
`kernel/src/threads/fuzz.rs`, declared as `#[cfg(feature = "kernel-fuzz")] mod fuzz;`. The
child module uses `use super::*` and thereby reaches **all** (including private) harness items —
**without** changing the visibility (`pub`) of kernel internals (requirement "kernel APIs as far as possible
unchanged"). A separate crate would force extensive `pub` openings and was therefore
rejected; the child module is the least invasive "equivalent structure".

### 3. Small module API + stub for the release build
`fuzz` exports (only `pub(super)`): `drive()` (executes the due fuzzer step on each idle tick),
`all_passed()` (for `all_done()`), `report()` (print lines), the two chain gates
`loaderfuzz_gate(pred)` / `fuzzers_gate(pred)` and four `dbg_*()` bools. For the release build
(`#[cfg(not(feature = "kernel-fuzz"))]`) a **stub module** provides the same API as no-ops:
`drive()`/`report()` = empty, `all_passed()` = `true`, the gates pass their **predecessor** through
(`loaderfuzz_gate(pred)=pred` etc.), `dbg_*()` = `true`. Thus the idle-manager chain flows
through transparently when the fuzzers are absent — **without** special cases in the harness.

### 4. Audits remain part of the kernel
Express separation: **fuzzers** (active test-case generation) → optional module. **Audits**
(checking the kernel invariants) → **always** stay in the kernel, feature-independent. The fuzzers
**call** audits; the audit functions themselves are never gated.

## Validation

- `cargo build --release --features kernel-fuzz` + `KERNEL_FUZZ=1 ./test-qemu.sh` → **all** fuzzers
  run as before (`fuzz`/`ipcfuzz`/`hwfuzz`/`loaderfuzz : ALL PASS`), 62/62.
- `cargo build --release` (without the feature) + `./test-qemu.sh` → **no** fuzzer code; the non-fuzzer
  tests + **all audits** stay green (58/58); the four fuzzer checks drop out.
- `hang-stress.sh` deadlock-free in both configurations (default = release without fuzzers).

## Consequences

- The release kernel is leaner and corresponds exactly to the long-term-test/production configuration.
- Fuzzers remain as a development tool with unchanged functionality (feature build).
- No kernel API break; the audits are clearly separated from the fuzzers.
- Pattern for future pure verification tools: optional feature-gated module + stub.
