# Path to operational readiness

As of 2026-07-29. The plan for how today's kernel becomes a base system for cloud servers —
no hypervisor beneath it, no VM inside it, isolation and time management by the kernel itself.

The items themselves are in [TODO.md](../TODO.md); what is here is the **order and why it is
so**. What is not here is the effort in person-days: the estimates would be guessed, and
guessed numbers are later read as promises.

## The sentence the plan hangs on

**Today the kernel has no non-test purpose.**

This is not a reproach but the measured state: if the `selftest` feature is switched off, the
kernel builds, boots, sets up paging, LAPIC, timer, SMP, PCI and VT-d — and then goes into an
empty loop. `.text` shrinks from 0x25000 to 0x11000, more than half of the
compiled code is test infrastructure. There is no boot archive on x86, no root task, no
userland. Every program that runs today is compiled into the kernel.

From this follows the structure: **stage 1 is not hardening and not scaling, but
being-able-to-execute-anything-at-all.** Everything else hangs on it, not
organisationally but technically — a driver, a network stack, a tenant process, a migrating thread: all
need first a way to load code from outside and give it authority.

## Stage 0 — straightening the ground (precondition of everything)

Non-negotiable, because every measurement stands on it.

1. **[D0] The x86 run must be deterministic again.** Measured: 4–6 of 8 runs of the same
   image reach `SELFTEST COMPLETE`, the rest stop at varying places. A
   set-up that hangs in a third of the cases cannot carry a statement — not even the one it
   is just reporting green. Two suspects are to be separated: the TCG fallback to
   `-cpu Skylake-Client` and the memory layout shifted by the colour test.
2. **[Verification] The ARM suite must run from a fresh clone.** `test-qemu.sh` signs
   TrustedSAS binaries with `keys/trusted-test.ed25519`; `/keys/` is gitignored. From a clone
   the suite is thus not runnable, and the second architecture branch remains unchecked — exactly
   the form of failure this project has already paid for three times. Solution: a **test key
   in the repo** (expressly named as such, with its own key slot marked `test`
   in the kernel), or a script that generates a pair on the first run and generates
   `trusted_keys.rs` from it. The production key naturally stays outside.
3. **`README.md` says what the project is.** Today it describes an aarch64 kernel of phase
   7 — not a word about the x86 port, VT-d, the core hand-over. In an open-source project that
   is the most expensive outdated file there is.

## Stage 1 — the kernel executes foreign code

This is the stage that turns a test scaffold into an operating system.

4. **[C6] Boot archive on x86 via Multiboot modules.** `SYS_LOAD` fails cleanly today, because
   there is nothing to load. On ARM the path exists (`tools/mkarchive.py`, `caprock-loader`) —
   x86 needs the counterpart via the Multiboot module list.
5. **[F2] Root task.** The seL4 way: load a start program from the archive and hand it
   the root caps. **Only then** may `default = []` happen (todo F2) — before that the
   gating would not be a leaner kernel but an empty one.
6. **[A4] `SYS_CDELETE`, then `CMOVE`/`CCOPY`.** Without it a long-lived service that receives caps
   via IPC cannot release its slots and runs against `CAP_BUDGET_PER_PD`. A
   root task is exactly such a service — with stage 1 the item turns from an ABI gap into an
   operational problem.

At the end of stage 1 the following holds: an externally built program starts, receives authority, gives it
back. From here on every further capability is a userland program and not a kernel patch.

## Stage 2 — a server that does something

7. **[Z10] Driver framework + virtio on x86.** `virtio` is aarch64-only to this day. Without network and
   block device it is not a cloud. As userland PDs, not in the kernel — that is the project boundary
   from the `README.md`, and it is right.
8. **[E, step 3b+4] Queued invalidation, then interrupt remapping with CFI switched off.**
   The order is enforced: the invalidation of the interrupt entry cache exists **only** as a
   QI descriptor. And without IR a passed-through device can generate arbitrary interrupt messages
   — that is the standard breakout from a device assignment and must stand **before** the first
   tenant device, not after.
9. ~~**[Z9] Define and write down the fault domain.**~~ **Done 2026-08-02 (B-6.2):**
   [fehlerdomaene.md](fehlerdomaene.md), invariant §14 in [invariants.md](invariants.md).
   What is fixed is the cheap variant — *the node is the fault domain, redundancy across nodes*,
   and all TrustedSAS PDs of a node together form **one** domain.
   **The premise of this item was wrong:** a kernel panic does *not* reliably take the node down
   today — in the most frequent cases it is swallowed, because the `halt()` in the panic path
   does not mask the interrupts and the timer tick brings the core back. Measured, with four
   different outcomes for the same cause. For a security product that is the worse
   outcome: the kernel keeps working after a demonstrably violated invariant. The three
   cheap countermeasures are named and **not** built — they are a decision
   (availability against honesty), not a repair.

## Stage 3 — more than one tenant, justifiably

10. **[Z1] The isolated, coloured path becomes the normal case.** Today `spawn_isolated` is regular
    and uncoloured, `spawn_isolated_colored` the exception. This includes the decision on
    the region size: colouring is incompatible with the 2 MiB block descriptor (512 pages
    sweep over all 256 measured colours), so either smaller regions with page-wise
    mapping or several coloured runs per PD.
11. **[A1 remainder] Stripe free list.** `mask_for` today allocates round-robin, **without** checking whether
    a stripe is occupied: more simultaneous PDs than partitions means silent
    colour overlap. What is needed is a clean failure instead of a silent weakening.
12. **[Z6] SMT.** Cache colouring separates the LLC and **in principle not** L1/L2/TLB/store buffer
    between sibling hyperthreads. Either SMT off (costs throughput, is honest) or one
    physical core belongs at every point in time to exactly one tenant. For the second way the
    scheduler must know the CPU topology; nobody reads it today.
13. **[A3/C3] Cap/PD/endpoint tables dynamic**, beforehand detach `ReplyFinal` from the kernel stack.
    As long as the cap table is global and fixed, one tenant determines the density of all the others.
14. **[Z7] Measured boot and attestation.** Precondition for everything that moves tenant state over the
    network.

## Stage 4 — the properties that replace VMs

15. **[Z2] Accounting decoupled from the clock tick.** Consumption by cycle stamp on switching in and
    out (per-TCB `consumed_cycles`, todo D), monitoring cap on top. **A faster
    tick is the wrong answer** — it raises resolution *and* overhead; a cycle stamp raises
    only the resolution.
16. **[Z5] Tickless for compute cores.** Arm the timer only if there is something to preempt.
    Together with 15 this yields: accounting by cycles, preemption by need — the two are
    decoupled, and that is exactly the point.
17. **[Z8] NUMA**, decided together with the colour allocation. Two separately developed policies
    otherwise fight over the same physical address, and whoever allocates first wins.
18. **[Z4] Checkpoint/restore of a thread**, in the order Z4a–Z4f. This is the
    most costly item of the plan, and it deliberately stands at the end: it presupposes stage 1 (processes
    at all), stage 3 (isolation that one can take along) and Z7 (a target machine that can
    be trusted).
19. **[C5] GICv3 on ARM**, if ARM remains a target platform — otherwise ARM stays at 8 cores. On
    x86 this is done with x2APIC.

## What I consider risky about this plan

**Z4 (thread migration) can force design decisions that lie further forward.** Caps are
today indices into a global table plus a CDT edge; across machine boundaries an index
means nothing. If during the design of Z4b it turns out that an external, machine-independent
cap representation is needed, this affects the cap crate, the ABI and the verification. It is worth
**building an early throwaway prototype** of Z4b — only the serialisation of a
cap space, without network, without migration — to find out whether the model holds. That costs little
and can spare expensive detours.

**The second architecture branch rots as long as it does not run.** As long as the ARM suite does not start
from a clone, every aarch64 line is unchecked — the `hal::cache` version for ARM is
written, compiled and never executed. That is why item 2 stands in stage 0 and not
somewhere at the back.

**The order hardening-before-function is tempting and would be wrong.** A1, SMT, NUMA are
visible, well-delimited work with nice test results. But a kernel that executes nothing
needs no cache partitioning between tenants that do not exist. First stage 1,
then the rest.
