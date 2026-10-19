# Caprock — Architecture overview

Caprock is a standalone, capability-based microkernel in Rust,
inspired by seL4 and the seL4 Microkit runtime, but with a deliberately
different memory model. The goal is an operating system that is at once
**highly secure, high-performance, capability-based, deterministic and fully
modular**.

This document gives the overall overview. Reasoned individual decisions are kept
as ADRs (architecture decision records) under [`adr/`](adr/). The analysis of the
seL4 source code (as a reference, *not* as a fork base) is in
[`seL4-architecture-map.md`](seL4-architecture-map.md).

---

## 1. Guiding idea

Three existing systems form the intellectual foundation:

| System | What we adopt | What we do differently |
|--------|--------------------|------------------------|
| **seL4** | capability model, endpoint/notification IPC, untyped memory, fastpath, deterministic kernel structure | no per-process VSpace/MMU isolation; Rust instead of C; no proved, but unsafe C core |
| **seL4 Microkit** | protection domain model, channels, static system layout, lean runtime | the Microkit runtime becomes part of the kernel image; components are hot-reloadable |
| **Theseus OS** | single address space (SAS), memory safety *intralingual* through Rust, live replacement of modules | additionally an explicit seL4 capability model as the authority layer |

The central, defining difference from seL4: **Caprock uses no
per-process address spaces and no MMU-based memory isolation**. Every address
is a real physical address. Memory safety arises from two complementary
mechanisms — Rust type safety (prevents memory errors
*within* a component) and capabilities (govern authority *between*
components). Details and consequences: [ADR 0002](adr/0002-no-virtual-memory-sas.md).

---

## 2. What is in the kernel image — and what is not

**In the kernel image (trusted computing base):**

- microkernel core (boot, traps, scheduling loop)
- capability system
- scheduler
- IPC system (endpoints, notifications, reply)
- Microkit runtime (PD/channel model)

**Not in the kernel image** (run as userland components, hot-reloadable):

Structured map of the seL4 C kernel under
`/home/simon/Dokumente/Caprock/seL4/src`. **Reference, no fork model to
copy** — it serves to understand proven patterns for the Rust
re-implementation. Paths are relative to `seL4/src` (headers partly under `seL4/include`).

## Top-level subsystems

| Directory | Role | Size |
|-------------|-------|--------|
| `api/` | syscall entry, fault handling | ~36K |
| `kernel/` | core logic: boot, threads, scheduling, cspace | ~104K |
| `object/` | kernel objects (TCB, endpoint, CNode, …) | ~232K |
| `fastpath/` | optimised IPC hot path | ~36K |
| `machine/` | HW abstraction: IO, FPU, profiling | ~56K |
| `arch/` | architecture-specific (ARM/x86/RISC-V) | ~1.7M |
Structured map of the seL4 C kernel under
`/home/simon/Dokumente/Caprock/seL4/src`. **Reference, no fork model to
copy** — it serves to understand proven patterns for the Rust
re-implementation. Paths are relative to `seL4/src` (headers partly under `seL4/include`).

## Top-level subsystems

| Directory | Role | Size |
|-------------|-------|--------|
| `api/` | syscall entry, fault handling | ~36K |
| `kernel/` | core logic: boot, threads, scheduling, cspace | ~104K |
| `object/` | kernel objects (TCB, endpoint, CNode, …) | ~232K |
| `fastpath/` | optimised IPC hot path | ~36K |
| `machine/` | HW abstraction: IO, FPU, profiling | ~56K |
| `arch/` | architecture-specific (ARM/x86/RISC-V) | ~1.7M |
| `plat/` | platform drivers/config (40+ platforms) | ~716K |
| `drivers/` | timer, UART, IRQ, SMMU | ~136K |
| `smp/` | SMP: IPI, inter-core locking | ~16K |
| `model/` | scheduler data structures/state | ~16K |

Key observation: the core (`api/`, `kernel/`, `object/`) is relatively
architecture-independent; `arch/`+`plat/` (~2.4M) carry the hardware specifics.

## Capability system

- `include/object/cap.h`, `include/object/structures.h`,
  `include/object/cnode.h`; implementation in `kernel/cspace.c`, `object/cnode.c`.
- **CTE** = `cap_t` + `mdb_node_t` (cap + derivation metadata).
- **CNodes**: dynamically sized CTE tables, radix/guard-addressed
  (multi-level lookup like a page-table walk).
- **CDT/MDB**: parent-child relationships of the derivations → basis for revocation.
- Operations: `invokeCNodeInsert/Move/Revoke/Delete`, `cteRevoke/Delete/Move`,
  `isMDBParentOf`, `isFinalCapability`.

## IPC

- `include/object/endpoint.h|notification.h|reply.h`; impl. in
  `object/endpoint.c`, `object/notification.c`, `fastpath/fastpath.c`.
- Endpoint states: `Idle/Send/Recv`. Notification: `Idle/Waiting/Active(Badge)`.
- Syscall path: `api/syscall.c` → `handleSyscall` → `handleInvocation` →
  `decodeInvocation` (`object/objecttype.c`) → object-specific handler.
- Fastpath (`fastpath/fastpath.c`): when conditions are met (no fault,
  send right, receiver ready, priority fits) direct transition, otherwise `slowpath`.
- Syscall types: `Call, ReplyRecv, NBSendRecv, Recv, NBRecv, Send, Yield, Interrupt`.

## Scheduler

- `model/statedata.h`, `kernel/thread.h`, `kernel/thread.c`, `object/tcb.c`.
- Classic: ready queues `[domain][priority]` + L1/L2 bitmap (O(1) selection),
  round-robin domains (`ksDomSchedule`, `ksCurDomain`, `ksDomainTime`).
- MCS: `object/schedcontext.c`, `kernel/sporadic.h` — scheduling contexts with
  budget + sporadic-server refill (`scRefillHead/Tail`), budget donation on reply.
- SMP: per-core state via `NODE_STATE(...)`, reschedule per IPI (`smp/ipi.c`).
- Core functions: `schedule`, `scheduleChooseNewThread`,
  `tcbSchedEnqueue/Append/Dequeue`, `rescheduleRequired`.

## Untyped memory model

- `include/object/untyped.h`, `object/untyped.c`, `object/objecttype.c`,
  `kernel/boot.c`.
- All kernel objects arise from **untyped** via `invokeUntyped_Retype`:
  validates empty target slots, splits the region, creates caps, tracks
  `capFreeIndex`. Objects have fixed or variable sizes (bits).
- Finalisation (`finaliseCap`): last cap deleted → clean-up (abort endpoint IPC,
  unbind TCB); **zombie caps** for long, interruptible revokes.

## aarch64 specifics

- `arch/arm/64/head.S` (boot entry), `traps.S` (vectors/handlers),
  `c_traps.c` (C dispatch), `kernel/thread.c` (context switch),
  `kernel/vspace.c` (MMU/page tables/ASID — in Caprock the
  per-process part is **dropped**, ADR 0002), `machine/registerset.c`, `machine/fpu.c`.
- Boot: MMU-off entry → boot page tables → MMU on → `init_kernel` → root server.

## Consequences for Caprock

The *concepts* are adopted (caps/CDT, endpoint/notification/reply,
bitmap scheduler, retype idea); new are: Rust instead of C, **no per-process VSpace**
(ADR 0002), zero-copy IPC in the SAS (ADR 0004), index-arena CDT instead of pointer MDB
(ADR 0003), hot reload via stable cap identity (ADR 0006).

| `plat/` | platform drivers/config (40+ platforms) | ~716K |
| `drivers/` | timer, UART, IRQ, SMMU | ~136K |
| `smp/` | SMP: IPI, inter-core locking | ~16K |
| `model/` | scheduler data structures/state | ~16K |

Key observation: the core (`api/`, `kernel/`, `object/`) is relatively
architecture-independent; `arch/`+`plat/` (~2.4M) carry the hardware specifics.

## Capability system

- `include/object/cap.h`, `include/object/structures.h`,
  `include/object/cnode.h`; implementation in `kernel/cspace.c`, `object/cnode.c`.
- **CTE** = `cap_t` + `mdb_node_t` (cap + derivation metadata).
- **CNodes**: dynamically sized CTE tables, radix/guard-addressed
  (multi-level lookup like a page-table walk).
- **CDT/MDB**: parent-child relationships of the derivations → basis for revocation.
- Operations: `invokeCNodeInsert/Move/Revoke/Delete`, `cteRevoke/Delete/Move`,
  `isMDBParentOf`, `isFinalCapability`.

## IPC

- `include/object/endpoint.h|notification.h|reply.h`; impl. in
  `object/endpoint.c`, `object/notification.c`, `fastpath/fastpath.c`.
- Endpoint states: `Idle/Send/Recv`. Notification: `Idle/Waiting/Active(Badge)`.
- Syscall path: `api/syscall.c` → `handleSyscall` → `handleInvocation` →
  `decodeInvocation` (`object/objecttype.c`) → object-specific handler.
- Fastpath (`fastpath/fastpath.c`): when conditions are met (no fault,
  send right, receiver ready, priority fits) direct transition, otherwise `slowpath`.
- Syscall types: `Call, ReplyRecv, NBSendRecv, Recv, NBRecv, Send, Yield, Interrupt`.

## Scheduler

- `model/statedata.h`, `kernel/thread.h`, `kernel/thread.c`, `object/tcb.c`.
- Classic: ready queues `[domain][priority]` + L1/L2 bitmap (O(1) selection),
  round-robin domains (`ksDomSchedule`, `ksCurDomain`, `ksDomainTime`).
- MCS: `object/schedcontext.c`, `kernel/sporadic.h` — scheduling contexts with
  budget + sporadic-server refill (`scRefillHead/Tail`), budget donation on reply.
- SMP: per-core state via `NODE_STATE(...)`, reschedule per IPI (`smp/ipi.c`).
- Core functions: `schedule`, `scheduleChooseNewThread`,
  `tcbSchedEnqueue/Append/Dequeue`, `rescheduleRequired`.

## Untyped memory model

- `include/object/untyped.h`, `object/untyped.c`, `object/objecttype.c`,
  `kernel/boot.c`.
- All kernel objects arise from **untyped** via `invokeUntyped_Retype`:
  validates empty target slots, splits the region, creates caps, tracks
  `capFreeIndex`. Objects have fixed or variable sizes (bits).
- Finalisation (`finaliseCap`): last cap deleted → clean-up (abort endpoint IPC,
  unbind TCB); **zombie caps** for long, interruptible revokes.

## aarch64 specifics

- `arch/arm/64/head.S` (boot entry), `traps.S` (vectors/handlers),
  `c_traps.c` (C dispatch), `kernel/thread.c` (context switch),
  `kernel/vspace.c` (MMU/page tables/ASID — in Caprock the
  per-process part is **dropped**, ADR 0002), `machine/registerset.c`, `machine/fpu.c`.
- Boot: MMU-off entry → boot page tables → MMU on → `init_kernel` → root server.

## Consequences for Caprock

The *concepts* are adopted (caps/CDT, endpoint/notification/reply,
bitmap scheduler, retype idea); new are: Rust instead of C, **no per-process VSpace**
(ADR 0002), zero-copy IPC in the SAS (ADR 0004), index-arena CDT instead of pointer MDB
(ADR 0003), hot reload via stable cap identity (ADR 0006).

- drivers, network stack, file systems, USB, GUI
- system services, device manager, shell, logging

The only device contact in the kernel is a **debug console** (PL011 UART)
for the bring-up phase — analogous to seL4's debug `printf`. Productive logging is
a userland service. See [ADR 0006](adr/0006-hot-reload-architecture.md).

---

## 3. Planned crate structure

Subsystems are split out as their own crate only once they have real content
(no empty abstractions in stock). Present after phase 6:
`kernel`, `crates/caprock-{sync, abi, hal, mem, cap, sched, ipc, microkit}`.
Target structure:

```
Caprock/
  kernel/              # bootbares Image: Boot, Arch-Glue, verdrahtet Subsysteme
  crates/
    caprock-abi/      # geteilte Kernel<->User-ABI: Syscall-Nrn, Nachrichten-Layout, Cap-Rechte
    caprock-sync/     # no_std-Synchronisation: Ticket-Spinlock, Per-CPU-Daten
    caprock-hal/      # aarch64-HAL: Kontextwechsel, Traps, GIC, Timer, Identity-MMU, SMP
    caprock-mem/      # capability-basierter physischer Allokator (Untyped-artig)
    caprock-cap/      # Capability-Typen, CNode, Derivation-Tree (CDT)
    caprock-sched/    # deterministischer Multicore-Scheduler
    caprock-ipc/      # Endpoints, Notifications, Reply, Transfer, Fastpath
  microkit/
    caprock-microkit/ # In-Image-Microkit-Runtime (PD-/Channel-Modell)
  user/                # Out-of-Image-Komponenten (Treiber, Dienste) — hot-reloadbar
  targets/  docs/  build.sh  run-qemu.sh
```

**Dependency direction** (acyclic, from bottom to top):
`abi` → `sync` → `hal` → `mem` → `cap` → {`sched`, `ipc`} → `microkit` → `kernel`.

Deep, narrow interfaces (few, expressive APIs), no parallel
systems with the same function, no hidden global state.

---

## 4. Phase roadmap

| Phase | Contents | Tests in QEMU |
|------:|--------|---------------|
| **0** ✅ | bring-up: boot, parking of secondary cores, stack/BSS, debug console | boot test |
| **1** ✅ | HAL: exception vectors/traps, GICv2, generic timer, **MMU as a single identity map with caches on**, ticket spinlock, SMP bring-up (PSCI/HVC) | boot + multicore boot (8/8) + timer tick ✓ |
| **2** ✅ | physical memory model: linear memory capabilities, capability-based allocator (coalescing), ownership/transfer/return/`split`/`restrict`; **W^X hardening** brought forward | cap test (alloc/split/transfer/free) ✓ |
| **3** ✅ | capability system core: `CapSpace`, CDT, object refcount + finalisation, generation handles, copy/mint/move/delete/revoke | cap test (delegation/revocation/finalisation) ✓ |
| **4** ✅ | threads + scheduler: TCB, context switch (SP swap in the trap path), preemptive per-core round robin, idle, thread stacks from the allocator | scheduler test (preemption, 3 workers + idle) ✓ |
| **5** ✅ | IPC: synchronous endpoints (Call/Recv/Reply), SVC syscall dispatch + ABI, block/unblock + rendezvous direct switch | IPC test (call/reply round trip) ✓ |
| **6** ✅ | Microkit runtime in the image: protection domains with their own cap space, **cap-guarded IPC** (endpoints as caps, rights check) | PD↔PD IPC + access-denied test ✓ |
| **7** ✅ | hot reload: replace a server component in the running system (quiesce → swap via stable endpoint cap), without kernel restart, transparent to clients | hot-reload test (v1→v2, same endpoint) ✓ |

After every phase: build → boot test → IPC test → scheduler test → capability test
(as far as applicable to the phase) and a phase report under
[`phase-reports/`](phase-reports/).

---

## 5. Trust model (important consequence of the SAS design)

Without MMU isolation the kernel **cannot** lock in native, arbitrary machine code on the
hardware side: a process with raw pointers could write anywhere without an MMU.
Memory safety *between* components therefore rests on
**all components running in the system being memory safe** — i.e. written in
safe Rust and compiled by a trusted toolchain
(intralingual safety, as in Theseus). Capabilities are the
authority layer above it: they decide *which* memory/IPC/device rights
a component has at all (least privilege).

From this follows a hard project rule: `unsafe` is to be avoided outside the permitted
low-level domains (boot, context switch, register access, MMU/interrupt init),
because there — and only there — the intralingual safety guarantee can be broken.
Full justification and alternatives in
[ADR 0002](adr/0002-no-virtual-memory-sas.md).

---

## 6. Target hardware

ARM (aarch64), 8 cores, 4 GiB RAM. Development and test platform: QEMU
`virt`. Build and boot are described in [ADR 0001](adr/0001-toolchain-and-build.md);
build with `./build.sh`, start with `./run-qemu.sh`.
