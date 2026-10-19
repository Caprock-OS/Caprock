# Plan — per-PD capability budget, and SPAWN on a shared arena

Written 2026-08-26. Two changes, decided but not yet built. Everything below is measured
against the tree at the time of writing; the numbers are from this branch, not from memory.

> ## EXECUTED 2026-08-26 — and three things came out different
>
> Both changes are built and measured. `arena : ALL PASS` and `budget : ALL PASS` in the main
> suite, the load suite and on aarch64; `== ALL PASS ==` on all three. What this plan got wrong,
> in the order it mattered:
>
> **1. There was no probe to extend.** The plan said "extend the existing Z22 P2 probe". `pdthrd`
> makes its two threads in the **kernel**; `SYS_SPAWN` itself had **no caller and no gate anywhere
> in the tree** — 0 hits outside its own definition and one number anchor. It was built on
> 2026-08-17 and had never executed. So `arena` is not an extension of anything; it is the first
> run of the syscall, and it therefore carries the `x1 == 0` baseline (`ganze-region`) as its own
> conjunct before it says anything about windows.
>
> **2. `ueberlappung-abgewiesen` could not have fired.** The plan listed it as a conjunct to add.
> In fact the `Overlaps` refusal was **structurally unreachable** for exactly the threads
> `SYS_SPAWN` creates: `pd_mapping_overlaps` reads `KSTACKS.ubase_of`, and
> `spawn_with_stack_parked` deliberately writes nothing there. `stack_sibling_overlaps` had to be
> built for the conjunct to mean anything. That is the finding of this piece of work, and M1 of
> `tools/spawnarena-negativ.sh` is literally the tree as it stood before.
>
> **3. `CAP_BUDGET_MAX = 64` was an unkeepable promise.** A PD's local Cspace is an array of
> `NCAPS` = 16 slots; `install_cap` refuses any slot above it **without ever consulting the
> budget**. The self-test caught it on the day the ceiling was introduced. `CAP_BUDGET_MAX` is now
> `NCAPS` with a `const assert`, and **`NCAPS` is the real limit on a driver PD** — the account
> raises the reachable number from 8 to 16 at no cost to the other ten thousand PDs, and going
> past 16 needs a variable Cspace (TODO.md Part II K1c), which this plan explicitly put out of scope.
>
> Smaller deltas: the order was kept (Change 2 first, and it was right — the multi-thread PD is
> what Change 1's measurement stands on). The budget request rides on `SYS_LOAD` `MSG3` as planned.
> The `vorrat-erschoepft-abgewiesen` conjunct is **not** measured: triggering it needs ~80 000
> slots, i.e. thousands of PDs, which would move every other baseline in the run. What is measured
> instead is that the two refusal reasons are counted **separately** — over-max does not increment
> the pool counter. The gap is named in `TODO.md` A3.

---

## Why these two, and why together

The binding constraint on a Linux driver environment (LKL-style, see `TODO.md` L1) is **not** the
Linux API — it is the capability budget:

* `CAP_BUDGET_PER_PD = 8` (`crates/caprock-microkit/src/lib.rs`), enforced by `budget_allows`,
  tested by `budgettest` in `kernel/src/selftest.rs`.
* A driver PD already uses **6 of 8** at endowment (measured): slot 1 notification, 2 endpoint,
  3 MMIO cfg, 4 MMIO BAR, 5 DMA, 6 shared area.
* `SYS_SPAWN` binds **one whole `Memory` cap per thread stack**, and that cap cannot be deleted
  while the thread lives (`ERR_INUSE`). So a driver PD can spawn **at most two** threads. LKL wants
  more (boot thread, timer thread, workqueue threads).

Since 2026-08-25 `SYS_LOAD` can delegate up to 8 caps — which is useless while the receiver may
only hold 8 in total and already holds 6. The delivery side outgrew the holding side.

Change 1 fixes the account. Change 2 removes the reason the account is under pressure. Either alone
leaves the driver case blocked.

---

## Change 1 — the budget becomes an account

### The argument

A constant per PD costs the need of the **one** PD that needs it **times `NPDS`**. Measured:
`CapSlot` is ~96 bytes (the `Mdb` alone is 4 × `Option<usize>` = 64), and
`CAP_SLOTS_TOTAL = NPDS * CAP_BUDGET_PER_PD + 256 = 80 256` slots ≈ **7.7 MB**. Raising the
constant by 8 costs another ~7.7 MB for 10 000 PDs, to serve a handful.

This is the same move the repository has already made twice: `MELDESTELLEN` from a hand-kept number
to a derived one; `IDENTITY_DEBTS` from a count to a set. **Where a constant stands in for
bookkeeping, it eventually becomes wrong.**

### Already in the tree (inert, compiles, changes no behaviour)

* `CAP_BUDGET_MAX: usize = 64` — the ceiling for a single PD.
* `Pd.cap_budget: u16`, initialised from `CAP_BUDGET_PER_PD` in `Pd::EMPTY`.
* `PdTable.budget_vorrat: usize` (initialised to `CAP_SLOTS_FOR_ALL_PDS`) and
  `PdTable.budget_abgewiesen: u64`.

Nothing reads these yet — `budget_allows` still compares against the constant.

### To build

1. **`budget_allows` reads the PD's own budget**, not the constant:
   `self.pds.cap_count(pd) < self.pds.budget_of(pd)`. One line, plus a `budget_of` accessor.
   `endowment_fits` inherits this for free — it calls the same rule (that was the point of putting
   it there).

2. **Booking on creation.** `create_in_domain(domain, budget)`:
   * clamp the request to `CAP_BUDGET_MAX`, default `CAP_BUDGET_PER_PD` when `0`;
   * refuse if `budget > budget_vorrat` — **and count it** in `budget_abgewiesen`, separately from
     "no free PD slot". The two have different fixes and a shared counter would make the report
     unable to say which happened;
   * `budget_vorrat -= budget` on success.

3. **Return on teardown.** `free(pd)` adds `cap_budget` back. This is the half that decides whether
   the account is an account or a leak. It needs its own conjunct in the report — a pool that only
   ever shrinks looks exactly like a pool under load.

4. **Where the number comes from.** The manifest describes **boot only** (small disk driver + boot
   task manager), so it keeps the default. The runtime path carries it: `SYS_LOAD` `MSG3` is free
   since the multi-cap delegation change (`MSG0` index, `MSG1` pair list, `MSG2` count) — use it as
   the requested budget, `0` = default.

5. **Callers.** `create()`, `create_in_domain()`, `create_hardware_backend()` and every
   `system::create_pd*` wrapper gain the parameter or a `_mit_budget` variant. Existing call sites
   pass the default so behaviour is bit-identical where nothing asks.

### How it is measured

A report line `capbudget`, three-valued (`crate::befund::Befund`, built 2026-08-25):

| conjunct | says |
|---|---|
| `vorrat-anfangs == CAP_SLOTS_FOR_ALL_PDS` | the pool starts full — a speaking probe, without it every number below is unanchored |
| `angefordert>default` for at least one PD | the mechanism was *exercised*; a run where nobody asks for more proves nothing |
| `vorrat-nach == vorrat-vor - summe(budgets)` | the booking is exact, not approximate |
| `vorrat-nach-teardown == vorrat-vor-erzeugung` | **the return half** — the account closes |
| `ueber-max-abgewiesen` | a request beyond `CAP_BUDGET_MAX` is refused *by name*, not clamped silently |
| `vorrat-erschoepft-abgewiesen` | draining the pool refuses the next PD instead of over-committing |

### Counter-proofs (`tools/capbudget-negativ.sh`)

Each must make exactly one conjunct fall, with the others green:

* M1 — `free()` does not return the budget → `vorrat-nach-teardown` falls, booking stays green.
* M2 — the ceiling is not applied → `ueber-max-abgewiesen` falls.
* M3 — `budget_allows` reads the constant again → a PD with a raised budget cannot use it; the
  *ninth* installation must still be refused for a default PD (both directions).
* M4 — the pool is not decremented → `vorrat-nach` falls while teardown stays green.
* M5 — refusal counted in the shared counter instead of `budget_abgewiesen` → the report can no
  longer distinguish "no PD slot" from "no budget".

### Explicitly not in scope

* seL4-style Untyped/Retype. Not needed: a driver environment needs *memory*, and memory already
  works over a granted arena (`wasmhost` is the precedent). Kernel **objects** at runtime are the
  thing retype would buy, and the driver case does not need them.
* Growing a PD's budget after creation.
* Making `NCAPS` (the local index space, 16) variable.

---

## Change 2 — `SPAWN` on a sub-region

### The argument

Today one `Memory` cap = one thread stack, and the cap is pinned for the thread's life. That turns
"how many threads may a PD have" into "how many cap slots are left", which is the wrong question:
threads in a PD share the address space anyway, so a separate cap per stack buys **no isolation**
— the `UNPARK` doc says exactly this (*"wer einen Nachbarthread wecken kann, konnte vorher schon
seinen Stack beschreiben"*).

### The register, and why it is free

`SPAWN` is dispatched **before** the generic capability resolution
(`crates/caprock-microkit/src/lib.rs`, `if nr == sys::SPAWN`), so `x1` / `reg::EP_BADGE` is not
used by it. All four message words are taken (`MSG0` stack cap slot, `MSG1` entry, `MSG2` argument,
`MSG3` priority) — `x1` is the only free one.

**Layout, written out and not counted** (the C8 lesson — a marker on bit 63 collided with the top
number field and cost a debugging session):

```
x1 = (offset_pages << 32) | len_pages      // sub-region of the stack cap
x1 == 0                                    // the WHOLE region — today's behaviour
```

Pages, not bytes: both are page-aligned by construction, and 32 bits of pages is 16 TiB. `0` keeps
every existing caller bit-identical.

### To build

1. ABI: document the layout on `sys::SPAWN`; mirror any constant into `programs/libcaprock`
   **by hand** — that crate is MIT/Apache on purpose (`docs/grenze.md`) and must not depend on the
   AGPL workspace. It already mirrors `sys::` and `pdctl` for the same reason.
2. Dispatch: read `x1`, pass `(offset, len)` through the `spawn` function pointer.
3. `spawncheck::check_stack` validates the **sub-region**, not the cap's whole region: inside the
   region, page-aligned, non-zero, not device-reachable, no overlap with an existing mapping. The
   existing refusals keep their names; add one for "sub-region outside the cap".
4. `libcaprock::spawn` gains `(offset, len)`, defaulting to the whole region.

### How it is measured

Extend the existing Z22 P2 probe (two threads in one PD) to **four** threads on **one** arena cap:

| conjunct | says |
|---|---|
| `threads==4 slots-verbraucht==1` | the point: four stacks, one cap |
| each thread writes its own word and reads it back | the stacks are **disjoint** — without this, one arena and four threads would also "work" while they trample each other |
| `ueberlappung-abgewiesen` | two sub-regions that overlap are refused |
| `ausserhalb-abgewiesen` | a sub-region past the end of the cap is refused |
| `cap-nicht-loeschbar` | `ERR_INUSE` still holds while any of the four lives |

### Counter-proofs

* M1 — `check_stack` validates the whole region instead of the sub-region → `ausserhalb-abgewiesen`
  falls, the disjointness conjunct stays green.
* M2 — the overlap check ignores the offset → `ueberlappung-abgewiesen` falls.
* M3 — the offset is ignored entirely (all stacks at the region base) → the disjointness conjunct
  falls; this is the mutation that catches "works by accident because nothing wrote enough".

### Explicitly not in scope

* Guard pages between the sub-regions. Worth doing later; note that on aarch64
  `hal::mmu::guard_unterstuetzt() == false`, and a guard page that does nothing there once made a
  colour assertion structurally unsatisfiable (C9e) — so it needs its own measurement, not a
  side-effect of this change.
* Stack growth.

---

## Order

**Change 2 first.** It is smaller, it is self-contained, and it *reduces* the pressure that Change 1
relieves — measuring the budget account against a case that no longer needs four caps is a cleaner
measurement. Change 1's counter-proofs also want a PD that genuinely asks for more than the default,
and the multi-thread driver PD from Change 2 is that PD.

## What still blocks the driver environment after both

Named so nobody reads more into this than it does: monotonic time, deadlines on blocking calls,
`CAP_IRQ` (needs IRTE allocation with `SVT`/`SID`, B-3 work), TLS (named in `SYS_SPAWN`'s own doc,
absent from the tree), and a DMA bounce pool whose size is set at load time. Those are separate
plans.
