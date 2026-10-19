# ADR 0002 — No virtual RAM: single address space with capability isolation

**Status:** accepted (phase 0, to be implemented from phase 1) · **Date:** 2026-06-23

This is the *defining* architecture decision of Caprock and the biggest
difference from seL4.

## Requirement (from the specification)

- Every memory address is a real physical address.
- No per-process address spaces.
- No classic MMU-based memory isolation.
- No virtual memory translation.
- Memory safety exclusively via capabilities + ownership rights + Rust.

## Background: what "MMU off" really means on aarch64

An important hardware fact shapes the decision: if the MMU on aarch64 is
**completely disabled** (`SCTLR_ELx.M = 0`), the CPU treats **every**
data access as `Device-nGnRnE`. Consequences:

- **No caches** → every access goes to DRAM. A factor of 10–50× slower.
- **No speculation, no reordering, no write combining.**
- **Strict alignment enforced** → unaligned accesses fault.

"MMU completely off" thus directly contradicts the goal *high-performance*.

## Approaches analysed

### A) MMU completely disabled (literal interpretation)
- **+** Maximally simple conceptually; address == physical address trivially.
- **+** No page-table code, no TLB management.
- **− Performance catastrophic** (everything uncached, see above).
- **−** Strict alignment necessary everywhere; no `W^X`/NX, no
  read-only enforcement of code/`.rodata` possible.
- **Assessment:** acceptable only as a phase-0 transitional state, not as a goal.

### B) MMU on, **a single global identity map**, caches on (chosen)
The MMU is active, but there is **exactly one** translation set for the whole
system, mapping **virtual == physical** (identity/flat mapping). There are
**no per-process page tables and no address-space switch** on a
context switch (no `TTBR` switch, no ASID round trip).

- **+** The address a process sees **is** the physical address — the
  specification requirement "real physical address / no translation" remains
  semantically satisfied (identity map = no *conversion* of significance).
- **+** Caches/speculation active → **full performance**.
- **+** No TTBR switch on context switch → **very low switch latency**
  and **no TLB shootdowns** between processes (good for determinism and
  multicore scaling).
- **+** Coarse protection attributes can be set globally: code as `RX`, `.rodata` as `RO`,
  rest `RW`+`XN` (no-execute) → hardens the kernel itself (`W^X`) without
  introducing process isolation.
- **−** Offers **no** per-process hardware isolation (exactly intended) — separation
  between components comes solely from Rust + capabilities (see trust model).
- **−** Page-table set-up needed once (small, static code in the permitted
  `unsafe` domain "MMU init"). With 4 GiB of contiguous RAM a few
  block entries suffice (1 GiB/2 MiB blocks), no multi-level per-process walk.

### C) MMU on, per-process VSpaces (classic, like seL4)
- **+** Real hardware isolation, can also lock in unsafe native processes.
- **−** Directly contradicts the specification (per-process address spaces, translation).
- **−** TTBR switch + TLB management on context switch → higher latency,
  TLB shootdowns on multicore, more non-determinism. **Rejected.**

## Decision

**Approach B.** Caprock runs a **single address space (SAS)** with an active
MMU, but **a single, static identity map** and enabled caches.
There are no per-process address spaces and no address translation in the
semantic sense. Memory authority is granted via **memory capabilities**
(ADR 0003), memory safety within components is guaranteed by **Rust**.
In phase 0 the kernel still runs with the MMU disabled (pure
bring-up); the identity map + caches are enabled in phase 1.

## Trust model (compelling consequence)

Without per-process isolation: **all components must be memory safe**
(safe Rust, trusted toolchain). A process with arbitrary
`unsafe`/raw pointers could corrupt foreign memory without an MMU.
Capabilities limit *which* resources a component may address at all
(least privilege), but do **not** replace memory safety *within*
untrusted native code. Hence:

1. Userland components are delivered as safe Rust.
2. `unsafe` is restricted globally to the permitted low-level domains and is kept
   justified/auditable.
3. The coarse global MMU configuration (`W^X`, `XN` for data, `RO` for `.rodata`)
   catches whole classes of errors without introducing process isolation.

## Security implications

- **+** A large part of classic memory errors is *eliminated* by Rust, not
  merely caught.
- **+** `W^X`/`XN`/`RO` globally hardens the TCB.
- **−** No protection against malicious native code → the threat model excludes
  "untrusted native binaries". Must be documented prominently and
  possibly be complemented in future by optional hardware domains (e.g. MTE, PAN, or an
  optional sandbox VSpace for untrusted code).

## Performance implications

- **+** Caches active; no TTBR switch/TLB shootdown on a switch → low,
  predictable context-switch latency (good for *deterministic* + *multicore*).
- **+** IPC can pass data by reference/move instead of by copy (an
  address is globally valid) — very low IPC latency possible (ADR 0004).
- **−** One-time MMU set-up effort and some static `unsafe` code.

## Addendum: EL0/EL1 privilege separation in the SAS (implemented)

The single address space rules out **virtual memory** and per-process VSpaces
— but it does **not** rule out the ARM exception levels. Caprock uses the
*one* identity map to draw a **privilege boundary** (not an address-space boundary) via
AP bits:

- Kernel image (`.text`/`.rodata`/`.data`/`.bss`, kernel stacks) → **EL1-only**.
- User code (`.user_text`) → EL0-executable, read-only, `PXN` (not executable
  at EL1).
- User RAM (heap/user stacks above `__kernel_end`) → EL0+EL1 RW, `PXN`/`UXN`.

User threads run at **EL0** (`SPSR=EL0t`, own `SP_EL0`), interact with
the kernel exclusively via `svc` (syscall). If EL0 code accesses an
EL1-only page or attempts a privileged instruction, the hardware raises
a fault; the kernel **terminates only the faulty thread** and keeps running
(fault hook → `exit_current`) instead of halting. Verified in QEMU:
an EL0 thread reading `0x4008_0000` (kernel `.text`) produces a data
abort (`EC=0x24`) and is isolated (`el0`/`el0iso` ALL PASS).

This mitigates the `−` point above for **untrusted, but native**
user code: such code can indeed reach other *user* data in the SAS (no
address-space isolation), but can **no longer touch the kernel** on the hardware side.

## Addendum 2: hybrid model — optional per-process VSpace (implemented)

The gap remaining in addendum 1 (a compromised native process reaches
foreign *user* data in the SAS) is now closed for **isolated** PDs — **without** giving up the
SAS for trusted PDs (path C, hybrid; see
[ext-11](../phase-reports/ext-11-per-process-vspace.md)):

- **Trusted Rust PDs** → global SAS map (ASID 0), as before: fast,
  deterministic, zero-copy IPC.
- **Isolated PDs** → own VSpace (own tables + ASID), mapping only the kernel
  (EL1-only), the shared `.user_text` and **one private 2 MiB region** (EL0-RW,
  `nG`); all other RAM is EL1-only there. An access to foreign
  user memory faults → the kernel terminates the PD. Addressing stays identity;
  isolation comes from the **presence** of the mappings, not from translation.

Verified (`vspace` ALL PASS): the SAS probe and the isolated probe read the same foreign
address X — the SAS probe may, the isolated one faults (`EC=0x24`) and communicates
exclusively via IPC. This makes **real user↔user separation** for untrusted/native
PDs enforceable on the hardware side; the core threat model now covers it.

## Open points for later phases

- Exact page-table layout of the identity map (block sizes, MAIR attributes,
  cacheability for device MMIO regions vs. normal RAM).
- Reclaim of the EL1-only kernel stacks for EL0 threads (currently a fixed pool of 4
  slots, leaked at thread end; the dynamic user stack is reclaimed).
- Optional hardware hardening path for untrusted components
  (MTE/PAN/per-component sandbox VSpace) — deliberately outside the
  core threat model.
